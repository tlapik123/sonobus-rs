//! File playback extras (upstream's `WaveformTransportComponent` and the file menu): a
//! waveform overview computed off the UI thread and cached per file, and "Trim to New"
//! (`TrimFileJob`, SonobusPluginEditor.cpp:5179-5299).

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use sonobus_files::{PlayerOutput, RecordFormat, RecordSpec, open_player, start_recording};

/// Columns of an overview: min/max pairs, 8 KB per file.
pub const BUCKETS: usize = 1024;
/// Files whose overview is kept.
const CACHED: usize = 8;
const BLOCK: usize = 4096;

/// Min/max over all channels, in [`BUCKETS`] equal slices of the file (fewer for very short
/// files).
pub type Peaks = Vec<[f32; 2]>;

/// Decodes `frames` (all when `None`) from where the player stands, calling `f` with each
/// chunk of real audio, channel by channel.
fn pump(
    out: &mut PlayerOutput,
    finished: impl Fn() -> bool,
    frames: Option<u64>,
    mut f: impl FnMut(&[Vec<f32>], usize),
) {
    let ch = out.channels().max(1);
    let mut bufs = vec![vec![0.0f32; BLOCK]; ch];
    let mut left = frames.unwrap_or(u64::MAX);
    let mut idle = 0;
    while left > 0 {
        let want = BLOCK.min(usize::try_from(left).unwrap_or(BLOCK));
        let n = {
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..want]).collect();
            out.read(&mut refs)
        };
        if n == 0 {
            // the decoder is behind, or the file ended
            if finished() || idle > 2000 {
                break;
            }
            idle += 1;
            std::thread::sleep(Duration::from_millis(1));
            continue;
        }
        idle = 0;
        f(&bufs, n);
        left -= n as u64;
    }
}

/// The overview of a file (blocks the calling thread while it decodes).
pub fn compute_peaks(path: &Path, rate: u32) -> io::Result<Peaks> {
    let (player, mut out) = open_player(path, rate.max(1))?;
    let info = player.info();
    let total = info
        .frames
        .map(|f| f * u64::from(rate.max(1)) / u64::from(info.sample_rate.max(1)));
    // fine slices first (a known length gives the final ones directly), merged below
    let per = total.map_or(BLOCK as u64, |t| (t / BUCKETS as u64).max(1));
    let mut fine: Peaks = Vec::new();
    let (mut cur, mut in_cur) = ([f32::MAX, f32::MIN], 0u64);
    pump(
        &mut out,
        || player.is_finished(),
        total,
        |bufs, n| {
            for i in 0..n {
                for b in bufs {
                    cur[0] = cur[0].min(b[i]);
                    cur[1] = cur[1].max(b[i]);
                }
                in_cur += 1;
                if in_cur == per {
                    fine.push(cur);
                    (cur, in_cur) = ([f32::MAX, f32::MIN], 0);
                }
            }
        },
    );
    if in_cur > 0 {
        fine.push(cur);
    }
    Ok(reduce(&fine, BUCKETS))
}

/// Merges `fine` slices into at most `n`.
fn reduce(fine: &[[f32; 2]], n: usize) -> Peaks {
    if fine.len() <= n {
        return fine.to_vec();
    }
    (0..n)
        .map(|i| {
            let s =
                &fine[i * fine.len() / n..((i + 1) * fine.len() / n).max(i * fine.len() / n + 1)];
            s.iter().fold([f32::MAX, f32::MIN], |a, p| {
                [a[0].min(p[0]), a[1].max(p[1])]
            })
        })
        .collect()
}

/// Overviews of the last few files, each computed once on its own thread.
#[derive(Default)]
pub struct Waveforms {
    cache: VecDeque<(PathBuf, Arc<OnceLock<Option<Peaks>>>)>,
}

impl Waveforms {
    /// The overview of `path`, once computed (`None` meanwhile, or if it cannot be decoded).
    pub fn get(&mut self, path: &Path, rate: u32) -> Option<&Peaks> {
        let i = match self.cache.iter().position(|(p, _)| p == path) {
            Some(i) => i,
            None => {
                let cell = Arc::new(OnceLock::new());
                let (p, c) = (path.to_owned(), cell.clone());
                std::thread::Builder::new()
                    .name("sonobus-waveform".into())
                    .spawn(move || {
                        let _ = c.set(compute_peaks(&p, rate).ok());
                    })
                    .ok()?;
                if self.cache.len() >= CACHED {
                    self.cache.pop_front();
                }
                self.cache.push_back((path.to_owned(), cell));
                self.cache.len() - 1
            }
        };
        self.cache[i].1.get().and_then(Option::as_ref)
    }
}

/// "Trim to New": `len_secs` from `start_secs` of `path` into `<name>-trim.<ext>` next to it
/// (`-trim(2)`... if taken), WAV and Ogg kept, anything else written as FLAC, 16 bit. Blocks
/// while it works; returns the new file.
pub fn trim_to_new(path: &Path, rate: u32, start_secs: f64, len_secs: f64) -> io::Result<PathBuf> {
    let (player, mut out) = open_player(path, rate.max(1))?;
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let format = match ext.as_str() {
        "wav" => RecordFormat::Wav,
        "ogg" => RecordFormat::Ogg,
        _ => RecordFormat::Flac,
    };
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let dest = path.with_file_name(format!("{stem}-trim.{}", format.extension()));
    let channels = out.channels().max(1);
    let spec = RecordSpec {
        format,
        bits: 16,
        sample_rate: rate.max(1),
        channels,
    };
    let (mut writer, rec) = start_recording(&dest, spec)?;
    let start = (start_secs.max(0.0) * f64::from(rate)) as u64;
    player.seek(start * u64::from(player.info().sample_rate) / u64::from(rate.max(1)));
    let frames = (len_secs.max(0.0) * f64::from(rate)).round() as u64;
    pump(
        &mut out,
        || player.is_finished(),
        Some(frames),
        |bufs, n| {
            let refs: Vec<&[f32]> = bufs.iter().map(|b| &b[..n]).collect();
            // the disk thread is behind: wait, nothing may be dropped here
            while !writer.write(&refs) {
                std::thread::sleep(Duration::from_millis(2));
            }
        },
    );
    drop(writer);
    rec.stop()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduce_keeps_extremes() {
        let fine: Peaks = (0..10).map(|i| [-(i as f32), i as f32]).collect();
        assert_eq!(reduce(&fine, 20), fine);
        assert_eq!(reduce(&fine, 2), [[-4.0, 4.0], [-9.0, 9.0]]);
        assert_eq!(reduce(&fine, 3).len(), 3);
    }
}
