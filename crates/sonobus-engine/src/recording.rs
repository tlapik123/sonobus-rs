//! Recording control (`startRecordingToFile` / `stopRecordingToFile`, P.cpp:9092-9576): which
//! files, their names and formats, and handing the writers to the audio thread. The audio side
//! is [`crate::mixer::RecTaps`] plus one raw-stream writer per peer slot.
//!
//! Options bitmask as upstream: mix 1, self 2, mix minus self 4, individual users 8. Only the
//! mix gives `<dir>/<base>.<ext>`; anything else a folder `<dir>/<base>` with `<base>-MIX`,
//! `-MIXMINUS`, `-SELF[-<group>]` (one per input group) and `-<user>` files.
//!
//! Upstream issue #268 by design: upstream gives a writer only to the peers present when the
//! recording starts, owned by the peer, so a peer that drops and comes back (a new peer object)
//! is not recorded any more, and a peer that joins later never is. Here every peer that joins
//! or rejoins during a recording with "individual users" gets a new file in the folder; a name
//! already taken becomes `<base>-<user>(2).<ext>` (JUCE `getNonexistentSibling`), which sorts
//! next to the first segment. Every segment starts with silence up to the moment the peer
//! (re)joined (set by the audio thread when it starts feeding it, to the frame), so all files
//! of a recording share one timeline and line up in an editor. Cost of that silence: FLAC
//! stores it in a few bytes per 4096-frame block; WAV stores it in full (channels x bytes per
//! sample x frames: a stereo 16-bit segment starting 10 minutes in carries 115 MB of zeros);
//! Ogg Vorbis encodes it at a small but non-zero bitrate (a few kbit/s).
//!
//! Fork ADOPT (Mendeni `.rtms`): `<base>-latency.txt` next to the recording, one line per peer
//! at the start and at the stop: `start|stop`, user, estimated round trip ms, measured round
//! trip ms (`-` without a latency test result), tab separated.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sonobus_files::{
    RecordFormat, RecordSpec, Recording, TrackKind, legal_file_name, nonexistent_path,
    start_recording, start_recording_after_silence, track_file_name,
};

use crate::mixer::{self, MAX_PEERS, Meters, RecTaps};
use crate::{AudioConfig, AudioTx, Notification};
use sonobus_core::state::AppState;

/// What the recorder needs from the engine host.
pub(crate) struct RecCtx<'a> {
    pub cfg: AudioConfig,
    pub state: &'a AppState,
    pub tx: &'a mut AudioTx,
    pub notes: &'a mut Vec<Notification>,
}

pub const RECORD_MIX: i32 = 1;
pub const RECORD_SELF: i32 = 2;
pub const RECORD_MIX_MINUS_SELF: i32 = 4;
pub const RECORD_INDIVIDUAL_USERS: i32 = 8;

/// PR #78: a mask that would record nothing, or only the individual users, gets the full mix.
pub fn normalize_options(mask: i32) -> i32 {
    match mask & 15 {
        0 | RECORD_INDIVIDUAL_USERS => mask | RECORD_MIX,
        _ => mask,
    }
}

/// One peer as the recorder sees it.
pub(crate) struct PeerRec {
    pub slot: usize,
    pub user: String,
    pub recv_channels: usize,
    pub estimated_ms: f32,
    pub measured_ms: Option<f32>,
}

/// What a running recording looks like from outside.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordingStatus {
    /// The single file (mix only) or the folder.
    pub path: PathBuf,
    pub files: Vec<PathBuf>,
    pub elapsed_secs: f64,
    pub dropped_frames: u64,
}

struct Active {
    path: PathBuf,
    folder: Option<PathBuf>,
    base: String,
    format: RecordFormat,
    bits: u16,
    sample_rate: u32,
    options: i32,
    main: Vec<Recording>,
    users: Vec<Option<Recording>>,
    /// Segments of peers that left; finalized at the stop.
    closed: Vec<Recording>,
    files: Vec<PathBuf>,
    sidecar: PathBuf,
    dropped_reported: u64,
}

pub(crate) struct Recorder {
    meters: Arc<Meters>,
    active: Option<Active>,
    /// [`RecTaps::generation`] of the last taps sent.
    generation: u64,
    /// The error of a recording that stopped by itself (disk full), until
    /// [`crate::Engine::stop_recording`] reports it or a new one starts.
    pub failed: Option<String>,
}

fn format_of(v: i32) -> RecordFormat {
    match v {
        3 => RecordFormat::Wav,
        4 => RecordFormat::Ogg,
        _ => RecordFormat::Flac,
    }
}

/// `late`: the file waits for its leading silence ([`mixer::Cmd::UserRec`] sets it).
fn start_file(
    path: &Path,
    a: &Active,
    channels: usize,
    files: &mut Vec<PathBuf>,
    late: bool,
) -> io::Result<(sonobus_files::RecordWriter, Recording)> {
    let spec = RecordSpec {
        format: a.format,
        bits: a.bits,
        sample_rate: a.sample_rate,
        channels,
    };
    let r = if late {
        start_recording_after_silence(path, spec)?
    } else {
        start_recording(path, spec)?
    };
    files.push(r.1.path().to_owned());
    Ok(r)
}

impl Recorder {
    pub fn new(meters: Arc<Meters>) -> Self {
        Self {
            meters,
            active: None,
            generation: 0,
            failed: None,
        }
    }

    pub fn is_recording(&self) -> bool {
        self.active.is_some()
    }

    /// FLAC above 8 channels falls back to WAV (P.cpp:9134, 9446).
    fn ext(a: &Active, channels: usize) -> &'static str {
        RecordSpec {
            format: a.format,
            bits: a.bits,
            sample_rate: a.sample_rate,
            channels,
        }
        .effective_format()
        .extension()
    }

    fn track(a: &Active, kind: TrackKind<'_>, channels: usize) -> PathBuf {
        let name = track_file_name(&a.base, kind, Self::ext(a, channels));
        a.folder.as_deref().unwrap_or(Path::new("")).join(name)
    }

    /// Start recording into `dir` under `base`. On success the main path (file or folder).
    pub fn start(
        &mut self,
        h: &mut RecCtx<'_>,
        dir: &Path,
        base: &str,
        peers: &[PeerRec],
    ) -> io::Result<PathBuf> {
        if self.active.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "already recording",
            ));
        }
        self.failed = None;
        let extra = &h.state.extra;
        let options = normalize_options(extra.default_recording_options);
        let rec_ch = match h.cfg.output_channels {
            0 => 2,
            n => n.min(mixer::MAX_CHANNELS),
        };
        let mut a = Active {
            path: PathBuf::new(),
            folder: None,
            base: legal_file_name(base),
            format: format_of(extra.default_recording_format),
            bits: if extra.default_recording_bits_per_sample == 24 {
                24
            } else {
                16
            },
            sample_rate: h.cfg.sample_rate,
            options,
            main: Vec::new(),
            users: (0..MAX_PEERS).map(|_| None).collect(),
            closed: Vec::new(),
            files: Vec::new(),
            sidecar: PathBuf::new(),
            dropped_reported: 0,
        };
        let mut taps = RecTaps {
            generation: self.generation + 1,
            self_pre_fx: extra.record_self_pre_fx,
            self_silence_when_muted: extra.record_self_silence_when_muted,
            counting: true,
            ..RecTaps::default()
        };
        let mut users = Vec::new();
        let mut first_err = None;
        let mut files = Vec::new();
        let mut note = |e: io::Error| {
            first_err.get_or_insert(e);
        };
        if options == RECORD_MIX {
            // upstream deletes an existing file of that name first; we never clobber
            let ext = Self::ext(&a, rec_ch);
            let p = dir.join(legal_file_name(&format!("{}.{ext}", a.base)));
            fs::create_dir_all(dir)?;
            let (w, r) = start_file(&p, &a, rec_ch, &mut files, false)?;
            a.path = r.path().to_owned();
            taps.mix = Some(w);
            a.main.push(r);
        } else {
            fs::create_dir_all(dir)?;
            let folder = nonexistent_path(&dir.join(&a.base));
            fs::create_dir(&folder)?;
            a.folder = Some(folder.clone());
            a.path = folder;
            if options & RECORD_MIX_MINUS_SELF != 0 {
                match start_file(
                    &Self::track(&a, TrackKind::MixMinus, rec_ch),
                    &a,
                    rec_ch,
                    &mut files,
                    false,
                ) {
                    Ok((w, r)) => {
                        taps.mix_minus = Some(w);
                        a.main.push(r);
                    }
                    Err(e) => note(e),
                }
            }
            if options & RECORD_SELF != 0 {
                for g in h.state.input_groups.iter().take(mixer::MAX_GROUPS) {
                    let nch = g.num_channels.clamp(0, mixer::MAX_CHANNELS as i32) as usize;
                    let kind = TrackKind::SelfInput(Some(g.name.as_str()));
                    let w = match nch {
                        0 => None,
                        _ => match start_file(
                            &Self::track(&a, kind, nch),
                            &a,
                            nch,
                            &mut files,
                            false,
                        ) {
                            Ok((w, r)) => {
                                a.main.push(r);
                                Some((w, nch))
                            }
                            Err(e) => {
                                note(e);
                                None
                            }
                        },
                    };
                    taps.selfs.push(w);
                }
            }
            if options & RECORD_MIX != 0 {
                match start_file(
                    &Self::track(&a, TrackKind::Mix, rec_ch),
                    &a,
                    rec_ch,
                    &mut files,
                    false,
                ) {
                    Ok((w, r)) => {
                        taps.mix = Some(w);
                        a.main.push(r);
                    }
                    Err(e) => note(e),
                }
            }
            if options & RECORD_INDIVIDUAL_USERS != 0 {
                for p in peers {
                    match Self::user_file(&a, p, &mut files) {
                        Ok((w, r)) => {
                            users.push((p.slot, w));
                            a.users[p.slot] = Some(r);
                        }
                        Err(e) => note(e),
                    }
                }
            }
        }
        a.files = files;
        if a.files.is_empty() {
            if let Some(f) = &a.folder {
                let _ = fs::remove_dir(f);
            }
            return Err(first_err.unwrap_or_else(|| io::Error::other("nothing to record")));
        }
        let side_dir = a.folder.clone().unwrap_or_else(|| dir.to_owned());
        a.sidecar =
            nonexistent_path(&side_dir.join(legal_file_name(&format!("{}-latency.txt", a.base))));
        let _ = Self::sidecar_lines(&a.sidecar, "start", peers);
        self.meters.rec_frames.store(0, Ordering::Relaxed);
        h.tx.mix(mixer::Cmd::Rec(Box::new(taps)));
        for (slot, w) in users {
            h.tx.mix(mixer::Cmd::UserRec(slot, Some(Box::new(w))));
        }
        let path = a.path.clone();
        self.active = Some(a);
        self.generation += 1;
        h.notes
            .push(Notification::RecordingStarted { path: path.clone() });
        Ok(path)
    }

    /// A peer whose name is not known yet (a direct connection) records as `user<slot + 1>`;
    /// the file keeps that name when the name becomes known (upstream would write `<base>-`).
    fn user_file(
        a: &Active,
        p: &PeerRec,
        files: &mut Vec<PathBuf>,
    ) -> io::Result<(sonobus_files::RecordWriter, Recording)> {
        // nothing known yet: assume there will be something eventually (upstream: 2)
        let nch = if p.recv_channels == 0 {
            2
        } else {
            p.recv_channels
        };
        let placeholder;
        let user = if p.user.is_empty() {
            placeholder = format!("user{}", p.slot + 1);
            &placeholder
        } else {
            &p.user
        };
        start_file(
            &Self::track(a, TrackKind::User(user), nch),
            a,
            nch,
            files,
            true,
        )
    }

    fn sidecar_lines(path: &Path, what: &str, peers: &[PeerRec]) -> io::Result<()> {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        for p in peers {
            let measured = p
                .measured_ms
                .map_or_else(|| "-".to_owned(), |m| format!("{m:.1}"));
            writeln!(f, "{what}\t{}\t{:.1}\t{measured}", p.user, p.estimated_ms)?;
        }
        Ok(())
    }

    /// A peer joined (or its name became known) while recording individual users.
    pub fn peer_joined(&mut self, h: &mut RecCtx<'_>, p: &PeerRec) {
        let Some(a) = &mut self.active else { return };
        if a.options & RECORD_INDIVIDUAL_USERS == 0 || a.users[p.slot].is_some() {
            return;
        }
        let mut files = Vec::new();
        match Self::user_file(a, p, &mut files) {
            Ok((w, r)) => {
                a.users[p.slot] = Some(r);
                a.files.extend(files);
                h.tx.mix(mixer::Cmd::UserRec(p.slot, Some(Box::new(w))));
            }
            Err(e) => h.notes.push(Notification::RecordingError {
                error: e.to_string(),
            }),
        }
    }

    pub fn peer_left(&mut self, h: &mut RecCtx<'_>, slot: usize) {
        let Some(a) = &mut self.active else { return };
        if let Some(r) = a.users.get_mut(slot).and_then(Option::take) {
            h.tx.mix(mixer::Cmd::UserRec(slot, None));
            a.closed.push(r);
        }
    }

    /// Detach every writer from the audio thread; the returned recordings still have to be
    /// [`finish`]ed (off the engine lock: it waits for the disk threads).
    pub fn stop(&mut self, h: &mut RecCtx<'_>, peers: &[PeerRec]) -> Option<Stopped> {
        let mut a = self.active.take()?;
        // every writer leaves the audio thread on one block, acknowledged by the generation
        self.generation += 1;
        h.tx.mix(mixer::Cmd::Rec(Box::new(RecTaps {
            generation: self.generation,
            detach_users: true,
            detached: (0..MAX_PEERS).map(|_| None).collect(),
            ..RecTaps::default()
        })));
        // (overrides a writer install still pending in the command queue)
        for (slot, u) in a.users.iter_mut().enumerate() {
            if let Some(r) = u.take() {
                h.tx.mix(mixer::Cmd::UserRec(slot, None));
                a.closed.push(r);
            }
        }
        let _ = Self::sidecar_lines(&a.sidecar, "stop", peers);
        let mut recs = a.main;
        recs.append(&mut a.closed);
        Some(Stopped {
            recs,
            files: a.files,
            meters: self.meters.clone(),
            generation: self.generation,
        })
    }

    pub fn status(&self) -> Option<RecordingStatus> {
        let a = self.active.as_ref()?;
        let frames = self.meters.rec_frames.load(Ordering::Relaxed);
        Some(RecordingStatus {
            path: a.path.clone(),
            files: a.files.clone(),
            elapsed_secs: frames as f64 / f64::from(a.sample_rate.max(1)),
            dropped_frames: Self::dropped(a),
        })
    }

    fn dropped(a: &Active) -> u64 {
        a.main
            .iter()
            .chain(a.users.iter().flatten())
            .chain(&a.closed)
            .map(Recording::dropped_frames)
            .sum()
    }

    /// Send-thread poll: newly dropped frames are reported; true when a disk thread died
    /// (disk full, device gone): the caller stops the recording.
    pub fn poll(&mut self, h: &mut RecCtx<'_>) -> bool {
        let Some(a) = self.active.as_mut() else {
            return false;
        };
        let dropped = Self::dropped(a);
        if dropped > a.dropped_reported {
            a.dropped_reported = dropped;
            h.notes
                .push(Notification::RecordingDroppedFrames { frames: dropped });
        }
        a.main
            .iter()
            .chain(a.users.iter().flatten())
            .any(Recording::failed)
    }
}

/// A stopped recording whose files are being finalized.
pub(crate) struct Stopped {
    recs: Vec<Recording>,
    files: Vec<PathBuf>,
    meters: Arc<Meters>,
    generation: u64,
}

/// How long [`Stopped::finish`] waits for the audio thread to let go of the writers; it is
/// not running (no device, null audio) when that takes longer, and then cannot write anyway.
const DETACH_WAIT: std::time::Duration = std::time::Duration::from_millis(500);

impl Stopped {
    /// Wait until the audio thread detached the writers (bounded by [`DETACH_WAIT`]), so
    /// that every file ends on the same block, then for every disk thread; the first error,
    /// if any. Not on the audio thread nor under the engine lock.
    pub fn finish(self) -> (Vec<PathBuf>, Option<io::Error>) {
        let t0 = std::time::Instant::now();
        while self.meters.rec_installed.load(Ordering::Acquire) < self.generation
            && t0.elapsed() < DETACH_WAIT
        {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let mut err = None;
        for r in self.recs {
            if let Err(e) = r.stop() {
                err.get_or_insert(e);
            }
        }
        (self.files, err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_mask_always_has_the_mix() {
        assert_eq!(normalize_options(0), RECORD_MIX);
        assert_eq!(
            normalize_options(RECORD_INDIVIDUAL_USERS),
            RECORD_INDIVIDUAL_USERS | RECORD_MIX
        );
        assert_eq!(normalize_options(RECORD_SELF), RECORD_SELF);
        assert_eq!(
            normalize_options(RECORD_SELF | RECORD_INDIVIDUAL_USERS),
            RECORD_SELF | RECORD_INDIVIDUAL_USERS
        );
    }
}
