//! Streaming file playback: a decoder thread decodes, loops and resamples to the engine rate
//! into a lock-free ring; the audio thread only copies out of it. Mirrors upstream's
//! `AudioTransportSource` over an `AudioFormatReaderSource` with 65536 frames of read-ahead on
//! a disk thread and sample-rate correction (`SonobusPluginProcessor.cpp:9595-9660`).

use std::fs::File;
use std::io;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::{Fft, FixedSync, Indexing, Resampler};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::{AudioDecoder, AudioDecoderOptions};
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo, TrackType};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::units::{TimeBase, Timestamp};

/// Read-ahead in output frames (upstream `setSource(..., 65536, ...)`).
const RING_FRAMES: usize = 65536;
const CHUNK: usize = 1024;
const MARKERS: usize = 256;
const POLL: Duration = Duration::from_millis(5);
/// Upstream's shortest loop (`WaveformTransportComponent.h:452`).
const MIN_LOOP: u64 = 2048;
const END: u64 = u64::MAX;

/// What [`open_player`] found in the file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileInfo {
    pub sample_rate: u32,
    pub channels: usize,
    /// Length in frames at `sample_rate`, if the container states it.
    pub frames: Option<u64>,
}

/// Start of a stretch of output that maps linearly onto the source: output frame `out` (counted
/// since the last seek) plays source frame `src`. `src == END` marks the end of the file.
#[derive(Debug, Clone, Copy)]
struct Marker {
    out: u64,
    src: u64,
}

struct Shared {
    seek_target: AtomicU64,
    /// Bumped by `Player::seek`.
    seek_gen: AtomicU64,
    /// Set by the decoder once it stopped pushing pre-seek audio.
    decoder_gen: AtomicU64,
    /// Set by the audio thread once it discarded everything queued before `decoder_gen`.
    audio_gen: AtomicU64,
    looping: AtomicBool,
    /// (start, length) in source frames; length 0 = whole file.
    loop_range: Mutex<(u64, u64)>,
    gain: AtomicU32,
    position: AtomicU64,
    finished: AtomicBool,
    stop: AtomicBool,
}

/// Control side of a playing file (UI/engine thread).
pub struct Player {
    shared: Arc<Shared>,
    info: FileInfo,
    thread: Option<JoinHandle<()>>,
}

/// Audio-thread side: pulls decoded audio at the engine rate. Never blocks or allocates.
pub struct PlayerOutput {
    samples: Consumer<f32>,
    markers: Consumer<Marker>,
    shared: Arc<Shared>,
    channels: usize,
    src_rate: u64,
    out_rate: u64,
    generation: u64,
    out_index: u64,
    segment: Marker,
    ended: bool,
    gain: f32,
}

/// Opens `path` and starts decoding, resampled to `out_rate`, from the beginning. Readable:
/// WAV, AIFF, FLAC, Ogg Vorbis, MP3, M4A (AAC/ALAC), CAF.
pub fn open_player(path: &Path, out_rate: u32) -> io::Result<(Player, PlayerOutput)> {
    if out_rate == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output rate must be non-zero",
        ));
    }
    #[cfg(any(test, feature = "test-hooks"))]
    let slow = testing::slow_io(path);
    let (src, info) = Source::open(path)?;
    let resampler = if info.sample_rate == out_rate {
        None
    } else {
        let r = Fft::<f32>::new(
            info.sample_rate as usize,
            out_rate as usize,
            CHUNK,
            info.channels,
            FixedSync::Input,
        )
        .map_err(io::Error::other)?;
        Some(r)
    };
    let shared = Arc::new(Shared {
        seek_target: AtomicU64::new(0),
        seek_gen: AtomicU64::new(0),
        decoder_gen: AtomicU64::new(0),
        audio_gen: AtomicU64::new(0),
        looping: AtomicBool::new(false),
        loop_range: Mutex::new((0, 0)),
        gain: AtomicU32::new(1f32.to_bits()),
        position: AtomicU64::new(0),
        finished: AtomicBool::new(false),
        stop: AtomicBool::new(false),
    });
    let (sample_tx, sample_rx) = RingBuffer::new(RING_FRAMES * info.channels);
    let (marker_tx, marker_rx) = RingBuffer::new(MARKERS);
    #[allow(unused_mut)]
    let mut dec = Decoder::new(
        src,
        resampler,
        out_rate,
        sample_tx,
        marker_tx,
        shared.clone(),
    );
    #[cfg(any(test, feature = "test-hooks"))]
    {
        dec.slow = slow;
    }
    let thread = thread::Builder::new()
        .name("sonobus-playback".into())
        .spawn(move || dec.run())?;
    Ok((
        Player {
            shared: shared.clone(),
            info,
            thread: Some(thread),
        },
        PlayerOutput {
            samples: sample_rx,
            markers: marker_rx,
            shared,
            channels: info.channels,
            src_rate: info.sample_rate.into(),
            out_rate: out_rate.into(),
            generation: 0,
            out_index: 0,
            segment: Marker { out: 0, src: 0 },
            ended: false,
            gain: 1.0,
        },
    ))
}

impl Player {
    pub fn info(&self) -> FileInfo {
        self.info
    }

    /// Jumps to source frame `frame` (clamped to the length). Clears the finished state.
    pub fn seek(&self, frame: u64) {
        let frame = self.info.frames.map_or(frame, |n| frame.min(n));
        self.shared.seek_target.store(frame, Ordering::Relaxed);
        self.shared.seek_gen.fetch_add(1, Ordering::AcqRel);
    }

    fn seek_pending(&self) -> bool {
        self.shared.seek_gen.load(Ordering::Acquire)
            != self.shared.audio_gen.load(Ordering::Acquire)
    }

    /// Source frame now leaving [`PlayerOutput::read`] (or the pending seek target).
    pub fn position(&self) -> u64 {
        if self.seek_pending() {
            self.shared.seek_target.load(Ordering::Relaxed)
        } else {
            self.shared.position.load(Ordering::Relaxed)
        }
    }

    /// End of file reached while not looping. Upstream rewinds to 0 and stops the transport
    /// at this point (`SonobusPluginProcessor.cpp:5252-5256`); do that with `seek(0)`.
    pub fn is_finished(&self) -> bool {
        !self.seek_pending() && self.shared.finished.load(Ordering::Acquire)
    }

    /// Takes effect at the read-ahead point, up to 65536 frames ahead of what is heard, like
    /// upstream's buffered reader.
    pub fn set_looping(&self, looping: bool) {
        self.shared.looping.store(looping, Ordering::Relaxed);
    }

    pub fn is_looping(&self) -> bool {
        self.shared.looping.load(Ordering::Relaxed)
    }

    /// Loop region in source frames, clamped like upstream's `setLoopFromTimeRange`: at least
    /// `min(2048, length)` long and inside the file. A zero length loops the whole file.
    pub fn set_loop_range(&self, start: u64, len: u64) {
        let (mut start, mut len) = (start, len);
        if let (Some(total), true) = (self.info.frames, len > 0) {
            len = len.clamp(MIN_LOOP.min(total), total);
            start = start.min(total - len);
        }
        *self
            .shared
            .loop_range
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = (start, len);
    }

    pub fn loop_range(&self) -> (u64, u64) {
        *self
            .shared
            .loop_range
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    /// Linear gain, ramped over the next block (JUCE `AudioTransportSource::setGain`).
    pub fn set_gain(&self, gain: f32) {
        self.shared.gain.store(gain.to_bits(), Ordering::Relaxed);
    }
}

impl Drop for Player {
    /// Tells the decoder thread to end and does not wait for it: a read stuck on a hung
    /// network share must not block the dropping thread (the engine drops players under its
    /// lock). The thread ends after its current read and closes the file then.
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        drop(self.thread.take());
    }
}

/// Test seams (feature `test-hooks`, or this crate's tests).
#[cfg(any(test, feature = "test-hooks"))]
pub mod testing {
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    static SLOW: Mutex<Vec<(PathBuf, Duration)>> = Mutex::new(Vec::new());

    /// Opening `path` and every decoder read from it take `delay` longer, as on a hung
    /// network share (`Duration::ZERO` removes it).
    pub fn set_slow_io(path: &Path, delay: Duration) {
        let mut s = SLOW
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        s.retain(|(p, _)| p != path);
        if !delay.is_zero() {
            s.push((path.to_owned(), delay));
        }
    }

    /// The delay for `path` (slept here once, for the open).
    pub(crate) fn slow_io(path: &Path) -> Duration {
        let d = SLOW
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|(p, _)| p == path)
            .map_or(Duration::ZERO, |(_, d)| *d);
        std::thread::sleep(d);
        d
    }
}

impl PlayerOutput {
    /// Channels of the file; `read` fills this many output channels.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Source frame the last [`Self::read`] ended at (what [`Player::position`] reports once
    /// no seek is pending). Real-time safe.
    pub fn position(&self) -> u64 {
        self.shared.position.load(Ordering::Relaxed)
    }

    /// Fills `out[c]` with file channel `c` (extra output channels get silence) and returns the
    /// frames of real audio; the rest is zeroed (decoder behind, after a seek, or end of file).
    pub fn read(&mut self, out: &mut [&mut [f32]]) -> usize {
        let frames = out.iter().map(|c| c.len()).min().unwrap_or(0);
        let sg = self.shared.seek_gen.load(Ordering::Acquire);
        if sg != self.generation {
            // Seek requested: go silent at once; once the decoder has stopped pushing for this
            // seek, everything queued is stale and gets dropped.
            if self.shared.decoder_gen.load(Ordering::Acquire) != sg {
                for o in out.iter_mut() {
                    o[..frames].fill(0.0);
                }
                return 0;
            }
            let dg = sg;
            let n = self.samples.slots();
            if n > 0 {
                self.samples.read_chunk(n).expect("n <= slots").commit_all();
            }
            let n = self.markers.slots();
            if n > 0 {
                self.markers.read_chunk(n).expect("n <= slots").commit_all();
            }
            self.generation = dg;
            self.out_index = 0;
            self.segment = Marker {
                out: 0,
                src: self.shared.seek_target.load(Ordering::Relaxed),
            };
            self.ended = false;
            self.shared.finished.store(false, Ordering::Release);
            self.shared.audio_gen.store(dg, Ordering::Release);
        }

        let ch = self.channels;
        let g0 = self.gain;
        let g1 = f32::from_bits(self.shared.gain.load(Ordering::Relaxed));
        let step = if frames > 0 {
            (g1 - g0) / frames as f32
        } else {
            0.0
        };
        let mut done = 0;
        while done < frames {
            while let Ok(&m) = self.markers.peek() {
                if m.out > self.out_index {
                    break;
                }
                let _ = self.markers.pop();
                if m.src == END {
                    self.ended = true;
                } else {
                    self.segment = m;
                }
            }
            if self.ended {
                break;
            }
            let mut n = (frames - done).min(self.samples.slots() / ch);
            if let Ok(m) = self.markers.peek() {
                n = n.min((m.out - self.out_index) as usize);
            }
            if n == 0 {
                break;
            }
            let chunk = self.samples.read_chunk(n * ch).expect("n <= slots");
            let (a, b) = chunk.as_slices();
            // Ring capacity and every push are whole frames, so the split is on a frame edge.
            for (i, frame) in a.chunks_exact(ch).chain(b.chunks_exact(ch)).enumerate() {
                let g = g0 + step * (done + i) as f32;
                for (c, o) in out.iter_mut().enumerate() {
                    o[done + i] = frame.get(c).map_or(0.0, |s| s * g);
                }
            }
            chunk.commit_all();
            done += n;
            self.out_index += n as u64;
        }
        for o in out.iter_mut() {
            o[done..frames].fill(0.0);
        }
        self.gain = g1;
        if self.ended {
            self.shared.finished.store(true, Ordering::Release);
        }
        let pos =
            self.segment.src + (self.out_index - self.segment.out) * self.src_rate / self.out_rate;
        self.shared.position.store(pos, Ordering::Relaxed);
        done
    }
}

// ---------------------------------------------------------------------------------------------
// Decoder thread

struct Decoder {
    src: Source,
    resampler: Option<Fft<f32>>,
    ratio: f64,
    samples: Producer<f32>,
    markers: Producer<Marker>,
    shared: Arc<Shared>,
    /// Interleaved source frames waiting for the resampler.
    pending: Vec<f32>,
    out_buf: Vec<f32>,
    /// Source frames fed / output frames pushed since the last seek.
    fed: u64,
    out: u64,
    /// Resampler latency still to drop from the output.
    skip: usize,
    /// Something was read since the last loop wrap (guards against looping an empty region).
    progressed: bool,
    src_done: bool,
    end_sent: bool,
    /// [`testing::set_slow_io`]: added to every read.
    #[cfg(any(test, feature = "test-hooks"))]
    slow: Duration,
}

impl Decoder {
    fn new(
        src: Source,
        resampler: Option<Fft<f32>>,
        out_rate: u32,
        samples: Producer<f32>,
        markers: Producer<Marker>,
        shared: Arc<Shared>,
    ) -> Self {
        let ch = src.channels;
        let out_frames = resampler.as_ref().map_or(CHUNK, |r| r.output_frames_max());
        Self {
            ratio: f64::from(out_rate) / f64::from(src.rate),
            skip: resampler.as_ref().map_or(0, |r| r.output_delay()),
            src,
            resampler,
            samples,
            markers,
            shared,
            pending: Vec::with_capacity(CHUNK * ch),
            out_buf: vec![0.0; out_frames * ch],
            fed: 0,
            out: 0,
            progressed: true,
            src_done: false,
            end_sent: false,
            #[cfg(any(test, feature = "test-hooks"))]
            slow: Duration::ZERO,
        }
    }

    fn run(mut self) {
        let mut generation = 0;
        let _ = self.markers.push(Marker { out: 0, src: 0 });
        while !self.shared.stop.load(Ordering::Acquire) {
            let sg = self.shared.seek_gen.load(Ordering::Acquire);
            if sg != generation {
                // Stop pushing, then wait until the audio thread has flushed the old audio.
                self.shared.decoder_gen.store(sg, Ordering::Release);
                if self.shared.audio_gen.load(Ordering::Acquire) != sg {
                    thread::sleep(Duration::from_millis(1));
                    continue;
                }
                generation = sg;
                let target = self.shared.seek_target.load(Ordering::Relaxed);
                self.restart();
                if self.src.seek(target).is_err() {
                    self.src_done = true;
                }
                let _ = self.markers.push(Marker {
                    out: 0,
                    src: target,
                });
                continue;
            }
            if self.end_sent
                || self.samples.slots() < self.out_buf.len()
                || self.markers.slots() < 2
            {
                thread::sleep(POLL);
                continue;
            }
            #[cfg(any(test, feature = "test-hooks"))]
            thread::sleep(self.slow);
            // A decode/IO error mid-stream ends playback there (truncated file).
            if self.step().is_err() {
                self.src_done = true;
                self.pending.clear();
            }
        }
    }

    fn restart(&mut self) {
        if let Some(r) = &mut self.resampler {
            r.reset();
            self.skip = r.output_delay();
        }
        self.pending.clear();
        self.fed = 0;
        self.out = 0;
        self.progressed = true;
        self.src_done = false;
        self.end_sent = false;
    }

    /// Decodes up to one chunk, resamples it and pushes it. Caller ensured ring space.
    fn step(&mut self) -> io::Result<()> {
        let ch = self.src.channels;
        let need = self
            .resampler
            .as_ref()
            .map_or(CHUNK, |r| r.input_frames_next());
        let looping = self.shared.looping.load(Ordering::Relaxed);
        let (loop_start, loop_len) = *self
            .shared
            .loop_range
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        while !self.src_done && self.pending.len() / ch < need {
            let pos = self.src.pos;
            let loop_end = if loop_len == 0 {
                self.src.total
            } else {
                Some(loop_start + loop_len)
            };
            // Inside the loop region play to its end; elsewhere play to the end of the file.
            let end = match loop_end {
                Some(e) if looping && pos <= e => Some(e),
                _ => self.src.total,
            };
            let want = need - self.pending.len() / ch;
            let max = end.map_or(want, |e| (e.saturating_sub(pos)).min(want as u64) as usize);
            let got = if max == 0 {
                0
            } else {
                self.src.read(&mut self.pending, max)?
            };
            if got > 0 {
                self.progressed = true;
                continue;
            }
            if max > 0 {
                self.src.total = Some(self.src.pos); // true end of stream: now we know the length
            }
            if looping && self.progressed {
                let start = if loop_len == 0 { 0 } else { loop_start };
                self.src.seek(start)?;
                self.progressed = false;
                // Position bookkeeping only; keep one slot for the end marker.
                if self.markers.slots() > 1 {
                    let at = (self.fed + (self.pending.len() / ch) as u64) as f64 * self.ratio;
                    let _ = self.markers.push(Marker {
                        out: at.round() as u64,
                        src: start,
                    });
                }
            } else {
                self.src_done = true;
            }
        }

        let frames = self.pending.len() / ch;
        let produced = match &mut self.resampler {
            None => {
                self.out_buf[..frames * ch].copy_from_slice(&self.pending);
                frames
            }
            Some(r) => {
                let input =
                    InterleavedSlice::new(&self.pending, ch, frames).map_err(io::Error::other)?;
                let out_frames = self.out_buf.len() / ch;
                let mut output = InterleavedSlice::new_mut(&mut self.out_buf, ch, out_frames)
                    .map_err(io::Error::other)?;
                let partial = Indexing::new().partial_len(frames);
                let idx = (frames < need).then_some(&partial);
                let (_, written) = r
                    .process_into_buffer(&input, &mut output, idx)
                    .map_err(io::Error::other)?;
                written
            }
        };
        self.fed += frames as u64;
        self.pending.clear();

        let skip = self.skip.min(produced);
        self.skip -= skip;
        let mut n = produced - skip;
        let expected = (self.fed as f64 * self.ratio).round() as u64;
        if self.src_done {
            n = n.min(expected.saturating_sub(self.out) as usize);
        }
        if n > 0 {
            let data = &self.out_buf[skip * ch..(skip + n) * ch];
            let chunk = self
                .samples
                .write_chunk_uninit(n * ch)
                .map_err(io::Error::other)?;
            chunk.fill_from_iter(data.iter().copied());
            self.out += n as u64;
        }
        if self.src_done && self.out >= expected {
            let _ = self.markers.push(Marker {
                out: self.out,
                src: END,
            });
            self.end_sent = true;
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------------
// Symphonia source

struct Source {
    reader: Box<dyn FormatReader>,
    decoder: Box<dyn AudioDecoder>,
    track_id: u32,
    time_base: Option<TimeBase>,
    rate: u32,
    channels: usize,
    /// Next source frame `read` returns.
    pos: u64,
    total: Option<u64>,
    /// After a seek: drop decoded frames before this one.
    skip_before: Option<u64>,
    buf: Vec<f32>,
    buf_off: usize,
}

impl Source {
    fn open(path: &Path) -> io::Result<(Self, FileInfo)> {
        let bad = |msg: &str| io::Error::new(io::ErrorKind::InvalidData, msg.to_owned());
        let mss = MediaSourceStream::new(Box::new(File::open(path)?), Default::default());
        let mut hint = Hint::new();
        if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
            hint.with_extension(ext);
        }
        let reader = symphonia::default::get_probe()
            .probe(
                &hint,
                mss,
                FormatOptions::default(),
                MetadataOptions::default(),
            )
            .map_err(sym_err)?;
        let track = reader
            .default_track(TrackType::Audio)
            .ok_or_else(|| bad("no audio track"))?;
        let Some(CodecParameters::Audio(params)) = &track.codec_params else {
            return Err(bad("no audio codec parameters"));
        };
        let rate = params
            .sample_rate
            .filter(|&r| r > 0)
            .ok_or_else(|| bad("unknown sample rate"))?;
        let channels = params
            .channels
            .as_ref()
            .map(|c| c.count())
            .filter(|&c| c > 0)
            .ok_or_else(|| bad("unknown channel count"))?;
        let decoder = symphonia::default::get_codecs()
            .make_audio_decoder(params, &AudioDecoderOptions::default())
            .map_err(sym_err)?;
        let info = FileInfo {
            sample_rate: rate,
            channels,
            frames: track.num_frames,
        };
        let src = Self {
            track_id: track.id,
            time_base: track.time_base,
            reader,
            decoder,
            rate,
            channels,
            pos: 0,
            total: info.frames,
            skip_before: None,
            buf: Vec::new(),
            buf_off: 0,
        };
        Ok((src, info))
    }

    fn ts_to_frame(&self, ts: Timestamp) -> i64 {
        match self.time_base {
            Some(tb) => {
                (i128::from(ts.get()) * i128::from(tb.numer.get()) * i128::from(self.rate)
                    / i128::from(tb.denom.get())) as i64
            }
            None => ts.get(),
        }
    }

    fn frame_to_ts(&self, frame: u64) -> Timestamp {
        let f = i128::from(frame);
        Timestamp::new(match self.time_base {
            Some(tb) => {
                (f * i128::from(tb.denom.get())
                    / (i128::from(tb.numer.get()) * i128::from(self.rate))) as i64
            }
            None => f as i64,
        })
    }

    fn seek(&mut self, frame: u64) -> io::Result<()> {
        let ts = self.frame_to_ts(frame);
        self.reader
            .seek(
                SeekMode::Accurate,
                SeekTo::Timestamp {
                    ts,
                    track_id: self.track_id,
                },
            )
            .map_err(sym_err)?;
        self.decoder.reset();
        self.buf.clear();
        self.buf_off = 0;
        self.pos = frame;
        self.skip_before = Some(frame);
        Ok(())
    }

    /// Appends up to `max` interleaved frames to `dst`; 0 means end of stream.
    fn read(&mut self, dst: &mut Vec<f32>, max: usize) -> io::Result<usize> {
        while self.buf_off >= self.buf.len() {
            let packet = match self.reader.next_packet() {
                Ok(Some(p)) => p,
                Ok(None) | Err(SymError::ResetRequired) => return Ok(0),
                Err(SymError::IoError(e)) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    return Ok(0);
                }
                Err(e) => return Err(sym_err(e)),
            };
            if packet.track_id != self.track_id {
                continue;
            }
            let first = self.ts_to_frame(packet.pts);
            match self.decoder.decode(&packet) {
                Ok(decoded) => decoded.copy_to_vec_interleaved(&mut self.buf),
                Err(SymError::DecodeError(_)) => continue, // skip a corrupt packet
                Err(e) => return Err(sym_err(e)),
            }
            let frames = self.buf.len() / self.channels;
            self.buf_off = match self.skip_before {
                Some(target) => {
                    let skip = (target as i64 - first).clamp(0, frames as i64) as usize;
                    if skip < frames {
                        self.skip_before = None;
                    }
                    skip * self.channels
                }
                None => 0,
            };
        }
        let n = max.min((self.buf.len() - self.buf_off) / self.channels);
        let end = self.buf_off + n * self.channels;
        dst.extend_from_slice(&self.buf[self.buf_off..end]);
        self.buf_off = end;
        self.pos += n as u64;
        Ok(n)
    }
}

fn sym_err(e: SymError) -> io::Error {
    match e {
        SymError::IoError(e) => e,
        e => io::Error::new(io::ErrorKind::InvalidData, e.to_string()),
    }
}

/// Decodes a whole file to planar channels (tests and offline checks).
#[cfg(test)]
pub(crate) fn decode_all(path: &Path) -> io::Result<(FileInfo, Vec<Vec<f32>>)> {
    let (mut src, info) = Source::open(path)?;
    let mut inter = Vec::new();
    while src.read(&mut inter, 4096)? > 0 {}
    let ch = info.channels;
    let planar = (0..ch)
        .map(|c| inter.iter().skip(c).step_by(ch).copied().collect())
        .collect();
    Ok((info, planar))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::tests::{record, signal, tmp};
    use crate::record::{RecordFormat, RecordSpec};
    use std::path::PathBuf;
    use std::time::Instant;

    /// Dropping a player whose decoder is stuck in a slow read returns at once (review
    /// finding 6: the engine drops players under its lock).
    #[test]
    fn drop_does_not_wait_for_a_stuck_read() {
        let path = ramp_file(48000, 48000);
        testing::set_slow_io(&path, Duration::from_millis(400));
        let t0 = Instant::now();
        let (p, out) = open_player(&path, 48000).unwrap();
        assert!(
            t0.elapsed() >= Duration::from_millis(400),
            "the open is slow"
        );
        std::thread::sleep(Duration::from_millis(50)); // the decoder is inside a read
        let t0 = Instant::now();
        drop(p);
        drop(out);
        assert!(
            t0.elapsed() < Duration::from_millis(100),
            "{:?}",
            t0.elapsed()
        );
        testing::set_slow_io(&path, Duration::ZERO);
        std::fs::remove_file(path).unwrap();
    }

    /// Ramp file: frame i of channel c holds (i + c) / 2^15, so values identify positions.
    fn ramp_file(frames: usize, rate: u32) -> PathBuf {
        let sig: Vec<Vec<f32>> = (0..2)
            .map(|c| {
                (0..frames)
                    .map(|i| ((i + c) % 32768) as f32 / 32768.0)
                    .collect()
            })
            .collect();
        let spec = RecordSpec {
            format: RecordFormat::Wav,
            bits: 16,
            sample_rate: rate,
            channels: 2,
        };
        record(&tmp("ramp.wav"), spec, &sig)
    }

    fn frame_at(x: f32) -> usize {
        (x * 32768.0).round() as usize
    }

    /// Pulls blocks like an audio callback until `want` real frames came out (or timeout).
    fn pull(out: &mut PlayerOutput, want: usize, block: usize) -> Vec<Vec<f32>> {
        let ch = out.channels();
        let mut got = vec![Vec::new(); ch];
        let mut bufs = vec![vec![0f32; block]; ch];
        let t0 = Instant::now();
        while got[0].len() < want {
            assert!(t0.elapsed() < Duration::from_secs(10), "playback stalled");
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| b.as_mut_slice()).collect();
            let n = out.read(&mut refs);
            if n == 0 {
                thread::sleep(Duration::from_millis(1));
            }
            let keep = n.min(want - got[0].len());
            for (g, b) in got.iter_mut().zip(&bufs) {
                g.extend_from_slice(&b[..keep]);
            }
        }
        got
    }

    fn wait_finished(p: &Player, out: &mut PlayerOutput) {
        let t0 = Instant::now();
        let mut buf = [0f32; 64];
        while !p.is_finished() {
            assert!(t0.elapsed() < Duration::from_secs(10), "never finished");
            let mut b2 = [0f32; 64];
            assert_eq!(out.read(&mut [&mut buf, &mut b2]), 0, "audio after end");
            thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn plays_whole_file_then_finishes() {
        let path = ramp_file(10000, 48000);
        let (p, mut out) = open_player(&path, 48000).unwrap();
        assert_eq!(
            p.info(),
            FileInfo {
                sample_rate: 48000,
                channels: 2,
                frames: Some(10000)
            }
        );
        let got = pull(&mut out, 10000, 480);
        for i in 0..10000 {
            assert_eq!(frame_at(got[0][i]), i);
            assert_eq!(frame_at(got[1][i]), i + 1);
        }
        assert_eq!(p.position(), 10000);
        wait_finished(&p, &mut out);
        // Rewind like upstream does at the end.
        p.seek(0);
        assert!(!p.is_finished());
        let got = pull(&mut out, 100, 64);
        assert_eq!(frame_at(got[0][0]), 0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn seek_jumps_and_reports_position() {
        let path = ramp_file(30000, 48000);
        let (p, mut out) = open_player(&path, 48000).unwrap();
        pull(&mut out, 1000, 256);
        p.seek(20000);
        assert_eq!(p.position(), 20000);
        let got = pull(&mut out, 500, 256);
        assert_eq!(frame_at(got[0][0]), 20000);
        assert_eq!(frame_at(got[0][499]), 20499);
        assert!(p.position() >= 20500);
        drop(p);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn loops_whole_file_and_range() {
        let path = ramp_file(5000, 48000);
        let (p, mut out) = open_player(&path, 48000).unwrap();
        p.set_looping(true);
        let got = pull(&mut out, 12000, 300);
        for (i, &x) in got[0].iter().enumerate() {
            assert_eq!(frame_at(x), i % 5000, "frame {i}");
        }
        assert!(!p.is_finished());

        // Loop range [1000, 3000): clamping keeps it as asked (>= 2048 long).
        p.set_loop_range(1000, 2000);
        assert_eq!(p.loop_range(), (1000, 2048));
        p.set_loop_range(1000, 2500);
        assert_eq!(p.loop_range(), (1000, 2500));
        p.seek(3000);
        let got = pull(&mut out, 6000, 300);
        assert_eq!(frame_at(got[0][0]), 3000);
        assert_eq!(frame_at(got[0][499]), 3499);
        assert_eq!(frame_at(got[0][500]), 1000); // wrapped at 3500
        assert_eq!(frame_at(got[0][3000]), 1000);
        let pos = p.position();
        assert!((1000..3500).contains(&pos), "{pos}");
        // Loop off: plays through to the end and finishes.
        p.set_looping(false);
        p.seek(4000);
        let got = pull(&mut out, 1000, 300);
        assert_eq!(frame_at(got[0][999]), 4999);
        wait_finished(&p, &mut out);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn gain_scales_output() {
        let path = ramp_file(4000, 48000);
        let (p, mut out) = open_player(&path, 48000).unwrap();
        p.set_gain(0.5);
        thread::sleep(Duration::from_millis(200)); // let read-ahead fill so pulls are whole blocks
        pull(&mut out, 256, 256); // ramp block
        let got = pull(&mut out, 100, 100);
        assert_eq!(got[0][0], (256.0 / 32768.0) * 0.5);
        std::fs::remove_file(path).unwrap();
    }

    fn dominant_freq(x: &[f32], rate: f64) -> f64 {
        // Zero-crossing count over the steady middle part.
        let mid = &x[x.len() / 4..x.len() * 3 / 4];
        let crossings = mid.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
        crossings as f64 * rate / mid.len() as f64
    }

    #[test]
    fn resamples_44k1_to_48k() {
        let frames = 44100; // 1 s
        let sig = signal(2, frames, 44100, 16); // 220 Hz and 440 Hz
        let spec = RecordSpec {
            format: RecordFormat::Flac,
            bits: 16,
            sample_rate: 44100,
            channels: 2,
        };
        let path = record(&tmp("rs.flac"), spec, &sig);
        let (p, mut out) = open_player(&path, 48000).unwrap();
        let got = pull(&mut out, 48000, 512);
        wait_finished(&p, &mut out);
        assert_eq!(p.position(), 44100);
        let f0 = dominant_freq(&got[0], 48000.0);
        let f1 = dominant_freq(&got[1], 48000.0);
        assert!((f0 - 220.0).abs() < 3.0, "{f0}");
        assert!((f1 - 440.0).abs() < 3.0, "{f1}");
        // Amplitude preserved and aligned with the source (no resampler delay left in).
        let peak = got[0].iter().fold(0f32, |m, x| m.max(x.abs()));
        assert!((peak - 0.5).abs() < 0.02, "{peak}");
        for i in [4800usize, 12345, 30000] {
            let t = i as f64 / 48000.0;
            let want = 0.5 * (std::f64::consts::TAU * 220.0 * t).sin();
            assert!((f64::from(got[0][i]) - want).abs() < 0.01, "frame {i}");
        }

        // FLAC seek + whole-file loop through the resampler. 1 s of 220 Hz is 220 whole periods,
        // so the looped signal stays a continuous sine and can be checked against the ideal.
        p.set_looping(true);
        p.seek(22050); // 0.5 s
        let got = pull(&mut out, 48000, 512);
        for k in [100usize, 1000, 23900, 24100, 30000, 47000] {
            let t = (0.5 + k as f64 / 48000.0).fract();
            let want = 0.5 * (std::f64::consts::TAU * 220.0 * t).sin();
            assert!(
                (f64::from(got[0][k]) - want).abs() < 0.01,
                "looped frame {k}"
            );
        }
        let pos = p.position();
        assert!(pos < 44100 && pos > 20000, "{pos}"); // wrapped: ~0.5 s again
        assert!(!p.is_finished());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn reads_ogg() {
        let sig = signal(1, 24000, 48000, 16);
        let spec = RecordSpec {
            format: RecordFormat::Ogg,
            bits: 16,
            sample_rate: 48000,
            channels: 1,
        };
        let path = record(&tmp("p.ogg"), spec, &sig);
        let (p, mut out) = open_player(&path, 48000).unwrap();
        assert_eq!(p.info().channels, 1);
        let got = pull(&mut out, 24000, 512);
        assert!((dominant_freq(&got[0], 48000.0) - 220.0).abs() < 3.0);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn corrupt_and_truncated_files_do_not_panic() {
        let garbage = tmp("garbage.wav");
        std::fs::write(
            &garbage,
            (0..5000u32)
                .map(|i| (i * 7919 % 251) as u8)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        assert!(open_player(&garbage, 48000).is_err());
        let empty = tmp("empty.flac");
        std::fs::write(&empty, b"").unwrap();
        assert!(open_player(&empty, 48000).is_err());
        assert!(open_player(&tmp("missing.wav"), 48000).is_err());

        for (fmt, ext) in [
            (RecordFormat::Wav, "wav"),
            (RecordFormat::Flac, "flac"),
            (RecordFormat::Ogg, "ogg"),
        ] {
            let spec = RecordSpec {
                format: fmt,
                bits: 16,
                sample_rate: 48000,
                channels: 2,
            };
            let path = record(
                &tmp(&format!("trunc.{ext}")),
                spec,
                &signal(2, 30000, 48000, 16),
            );
            let bytes = std::fs::read(&path).unwrap();
            for cut in [20, 200, bytes.len() / 2] {
                std::fs::write(&path, &bytes[..cut]).unwrap();
                if let Ok((p, mut out)) = open_player(&path, 44100) {
                    let t0 = Instant::now();
                    let mut a = [0f32; 256];
                    let mut b = [0f32; 256];
                    while !p.is_finished() {
                        assert!(
                            t0.elapsed() < Duration::from_secs(10),
                            "{ext} cut {cut} hangs"
                        );
                        out.read(&mut [&mut a, &mut b]);
                        thread::sleep(Duration::from_millis(1));
                    }
                }
            }
            std::fs::remove_file(path).unwrap();
        }
        std::fs::remove_file(garbage).unwrap();
        std::fs::remove_file(empty).unwrap();
    }
}
