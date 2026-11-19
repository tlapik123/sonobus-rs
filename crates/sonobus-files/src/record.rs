//! Recording: an audio-thread [`RecordWriter`] pushes into a preallocated ring, a disk thread
//! owned by [`Recording`] encodes and writes. Mirrors upstream's `AudioFormatWriter::ThreadedWriter`
//! with a 32768-frame FIFO on a `TimeSliceThread` (`SonobusPluginProcessor.cpp:9092-9508`).

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::num::{NonZeroU8, NonZeroU32};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use flacenc::bitsink::ByteSink;
use flacenc::component::{BitRepr, StreamInfo};
use flacenc::error::{Verified, Verify};
use flacenc::source::{Context, Fill, FrameBuf};
use rtrb::{Consumer, Producer, RingBuffer};
use vorbis_rs::{VorbisBitrateManagementStrategy, VorbisEncoder, VorbisEncoderBuilder};

use crate::naming::nonexistent_path;

/// Ring size per recorder, in frames (upstream `ThreadedWriter(..., 32768)`).
pub const RING_FRAMES: usize = 32768;
/// How often the disk thread drains the ring. ~680 ms of audio fit in the ring at 48 kHz.
const POLL: Duration = Duration::from_millis(10);
const DRAIN_FRAMES: usize = 4096;
const FLAC_BLOCK: usize = 4096;
/// Leading silence written per disk-thread pass (then the ring is drained again).
const SILENCE_CHUNKS_PER_PASS: usize = 16;
/// [`RecordWriter::set_leading_silence`] not called yet.
const LEAD_PENDING: u64 = u64::MAX;
/// Upstream passes quality index 8 ("256 kbps") to JUCE, which calls
/// `vorbis_encode_init_vbr(..., index * 0.1)` (juce_OggVorbisAudioFormat.cpp:275).
const OGG_QUALITY: f32 = 0.8;

/// Container/codec of a recording. Upstream default is FLAC (`mDefaultRecordingFormat`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RecordFormat {
    #[default]
    Flac,
    Wav,
    /// Ogg Vorbis, VBR quality 0.8; `bits` is ignored.
    Ogg,
}

impl RecordFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Flac => "flac",
            Self::Wav => "wav",
            Self::Ogg => "ogg",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecordSpec {
    pub format: RecordFormat,
    /// 16 or 24 (upstream offers exactly these; default 16).
    pub bits: u16,
    pub sample_rate: u32,
    pub channels: usize,
}

impl RecordSpec {
    /// FLAC cannot hold more than 8 channels; upstream falls back to WAV
    /// (`SonobusPluginProcessor.cpp:9134-9137`, per user `:9446-9452`). Use this format's
    /// extension when naming the file.
    pub fn effective_format(&self) -> RecordFormat {
        if self.format == RecordFormat::Flac && self.channels > 8 {
            RecordFormat::Wav
        } else {
            self.format
        }
    }
}

/// Audio-thread side of a recording. Never blocks or allocates.
pub struct RecordWriter {
    ring: Producer<f32>,
    channels: usize,
    dropped: Arc<AtomicU64>,
    lead: Arc<AtomicU64>,
}

impl RecordWriter {
    /// Queues one block. `channels[c]` is channel `c`; missing channels are written as
    /// silence, extra ones ignored, the block length is the shortest slice. If the disk thread
    /// is behind and the whole block does not fit, it is dropped and counted (upstream's
    /// ThreadedWriter drops the block too). Returns whether the block was queued.
    pub fn write(&mut self, channels: &[&[f32]]) -> bool {
        let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
        if frames == 0 {
            return true;
        }
        let Ok(chunk) = self.ring.write_chunk_uninit(frames * self.channels) else {
            self.dropped.fetch_add(frames as u64, Ordering::Relaxed);
            return false;
        };
        let ch = self.channels;
        chunk.fill_from_iter(
            (0..frames).flat_map(|f| (0..ch).map(move |c| channels.get(c).map_or(0.0, |s| s[f]))),
        );
        true
    }

    /// Frames dropped so far because the ring was full.
    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// For a writer from [`start_recording_after_silence`]: the file starts with `frames` of
    /// silence, then what was written. Real-time safe (one atomic operation); only the first
    /// call counts, and it does nothing on a writer from [`start_recording`].
    pub fn set_leading_silence(&mut self, frames: u64) {
        let frames = frames.min(LEAD_PENDING - 1);
        let _ = self
            .lead
            .compare_exchange(LEAD_PENDING, frames, Ordering::AcqRel, Ordering::Relaxed);
    }
}

/// Control side of a recording: owns the disk thread.
pub struct Recording {
    path: PathBuf,
    stop: Arc<AtomicBool>,
    dropped: Arc<AtomicU64>,
    thread: Option<JoinHandle<io::Result<()>>>,
}

impl Recording {
    /// The file being written (after the non-clobbering rename).
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// The disk thread ended before [`Self::stop`]: a write failed (disk full, device gone).
    /// [`Self::stop`] returns the error.
    pub fn failed(&self) -> bool {
        self.thread.as_ref().is_some_and(JoinHandle::is_finished)
    }

    /// Writes everything queued so far, finalizes the headers and returns the path. Detach the
    /// [`RecordWriter`] from the audio thread first; later writes are not recorded. Blocks the
    /// calling (non-audio) thread until the file is closed.
    pub fn stop(mut self) -> io::Result<PathBuf> {
        self.stop.store(true, Ordering::Release);
        match self.thread.take().map(JoinHandle::join) {
            Some(Ok(r)) => r.map(|()| self.path.clone()),
            _ => Err(io::Error::other("recording thread panicked")),
        }
    }
}

impl Drop for Recording {
    /// Dropping without `stop()` still finalizes the file, in the background.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// Creates the file (`path`, or `name(2).ext`... if it exists) and starts the disk thread.
/// Build `path` with [`crate::track_file_name`] and [`RecordSpec::effective_format`].
pub fn start_recording(path: &Path, spec: RecordSpec) -> io::Result<(RecordWriter, Recording)> {
    start_inner(path, spec, RING_FRAMES, Duration::ZERO, 0)
}

/// Like [`start_recording`], for a track that joins a running recording: the file starts with
/// silence whose length the writer's user sets later with
/// [`RecordWriter::set_leading_silence`] (the audio thread can, when it starts feeding it, so
/// the track lines up with the others to the frame). Until then (or until the stop) the disk
/// thread writes nothing; then it writes the silence a chunk at a time without allocating it,
/// keeping what the ring receives meanwhile in memory.
pub fn start_recording_after_silence(
    path: &Path,
    spec: RecordSpec,
) -> io::Result<(RecordWriter, Recording)> {
    start_inner(path, spec, RING_FRAMES, Duration::ZERO, LEAD_PENDING)
}

fn start_inner(
    path: &Path,
    spec: RecordSpec,
    ring_frames: usize,
    slow_disk: Duration,
    lead: u64,
) -> io::Result<(RecordWriter, Recording)> {
    let format = spec.effective_format();
    if spec.channels == 0 || spec.sample_rate == 0 {
        return Err(invalid("channels and sample rate must be non-zero"));
    }
    if format != RecordFormat::Ogg && !matches!(spec.bits, 16 | 24) {
        return Err(invalid("bits per sample must be 16 or 24"));
    }
    let path = nonexistent_path(path);
    let file = BufWriter::new(File::create_new(&path)?);

    let (prod, cons) = RingBuffer::new(ring_frames * spec.channels);
    let stop = Arc::new(AtomicBool::new(false));
    let dropped = Arc::new(AtomicU64::new(0));
    let lead = Arc::new(AtomicU64::new(lead));
    // The Vorbis encoder is not `Send`, so every encoder is built on the disk thread, which
    // reports whether that worked before we hand out the writer.
    let (init_tx, init_rx) = mpsc::sync_channel(1);
    let thread = {
        let stop = stop.clone();
        let lead = lead.clone();
        thread::Builder::new()
            .name("sonobus-record".into())
            .spawn(move || match Encoder::new(file, format, spec) {
                Ok(enc) => {
                    let _ = init_tx.send(Ok(()));
                    disk_thread(cons, enc, spec.channels, &stop, slow_disk, &lead)
                }
                Err(e) => {
                    let _ = init_tx.send(Err(e));
                    Ok(())
                }
            })?
    };
    let init = init_rx
        .recv()
        .unwrap_or_else(|_| Err(io::Error::other("recording thread died")));
    if let Err(e) = init {
        let _ = thread.join();
        let _ = std::fs::remove_file(&path);
        return Err(e);
    }
    Ok((
        RecordWriter {
            ring: prod,
            channels: spec.channels,
            dropped: dropped.clone(),
            lead,
        },
        Recording {
            path,
            stop,
            dropped,
            thread: Some(thread),
        },
    ))
}

fn disk_thread(
    ring: Consumer<f32>,
    mut enc: Encoder,
    channels: usize,
    stop: &AtomicBool,
    slow_disk: Duration,
    lead: &AtomicU64,
) -> io::Result<()> {
    match drain(ring, &mut enc, channels, stop, slow_disk, lead) {
        Ok(()) => enc.finish(),
        Err(e) => {
            // Like JUCE's writer: still try to leave a readable file behind.
            let _ = enc.finish();
            Err(e)
        }
    }
}

fn drain(
    mut ring: Consumer<f32>,
    enc: &mut Encoder,
    channels: usize,
    stop: &AtomicBool,
    slow_disk: Duration,
    lead: &AtomicU64,
) -> io::Result<()> {
    let mut buf = vec![0f32; DRAIN_FRAMES * channels];
    // leading silence still to write (`None`: not known yet), and the ring audio drained
    // while it is being written
    let mut silence: Option<u64> = None;
    let mut zeros = Vec::new();
    let mut backlog: Vec<f32> = Vec::new();
    loop {
        // Read the flag before draining so everything pushed before stop() gets written.
        let stopping = stop.load(Ordering::Acquire);
        let left = match silence {
            Some(n) => n,
            None => match lead.load(Ordering::Acquire) {
                LEAD_PENDING if !stopping => {
                    thread::sleep(POLL);
                    continue;
                }
                LEAD_PENDING => *silence.insert(0),
                n => *silence.insert(n),
            },
        };
        if left > 0 {
            zeros.resize(DRAIN_FRAMES * channels, 0.0);
            let mut left = left;
            for _ in 0..SILENCE_CHUNKS_PER_PASS {
                let k = left.min(DRAIN_FRAMES as u64) as usize;
                if k == 0 {
                    break;
                }
                write(enc, &zeros[..k * channels])?;
                left -= k as u64;
            }
            silence = Some(left);
        }
        let in_silence = silence != Some(0);
        loop {
            let n = ring.slots().min(buf.len()) / channels * channels;
            if n == 0 {
                break;
            }
            let chunk = ring.read_chunk(n).expect("n <= slots");
            let (a, b) = chunk.as_slices();
            buf[..a.len()].copy_from_slice(a);
            buf[a.len()..n].copy_from_slice(b);
            chunk.commit_all();
            if in_silence {
                backlog.extend_from_slice(&buf[..n]);
                continue;
            }
            if !backlog.is_empty() {
                write(enc, &std::mem::take(&mut backlog))?;
            }
            write(enc, &buf[..n])?;
            if !slow_disk.is_zero() {
                thread::sleep(slow_disk);
            }
        }
        if in_silence {
            continue; // the next chunk of silence right away
        }
        if !backlog.is_empty() {
            write(enc, &std::mem::take(&mut backlog))?;
        }
        if stopping {
            return Ok(());
        }
        thread::sleep(POLL);
    }
}

fn write(enc: &mut Encoder, interleaved: &[f32]) -> io::Result<()> {
    #[cfg(feature = "test-hooks")]
    if testing::FAIL_WRITES.load(Ordering::Relaxed) {
        return Err(io::Error::new(
            io::ErrorKind::StorageFull,
            "no space left on device (injected)",
        ));
    }
    enc.write(interleaved)
}

/// Test seams for crates that use this one (feature `test-hooks`, for their tests only).
#[cfg(feature = "test-hooks")]
pub mod testing {
    use std::sync::atomic::{AtomicBool, Ordering};

    pub(crate) static FAIL_WRITES: AtomicBool = AtomicBool::new(false);

    /// Every recording's next disk write fails (process-wide) while `on`, as on a full disk.
    pub fn fail_writes(on: bool) {
        FAIL_WRITES.store(on, Ordering::Relaxed);
    }
}

enum Encoder {
    Wav(WavWriter),
    Flac(Box<FlacWriter>),
    Ogg(Box<VorbisEncoder<BufWriter<File>>>, Vec<Vec<f32>>),
}

impl Encoder {
    fn new(file: BufWriter<File>, format: RecordFormat, spec: RecordSpec) -> io::Result<Self> {
        Ok(match format {
            RecordFormat::Wav => Self::Wav(WavWriter::new(file, spec)?),
            RecordFormat::Flac => Self::Flac(Box::new(FlacWriter::new(file, spec)?)),
            RecordFormat::Ogg => {
                let ch = u8::try_from(spec.channels)
                    .ok()
                    .and_then(NonZeroU8::new)
                    .ok_or_else(|| invalid("Ogg Vorbis supports at most 255 channels"))?;
                let rate = NonZeroU32::new(spec.sample_rate).ok_or_else(|| invalid("zero rate"))?;
                // The serial only has to differ between chained streams; one stream per file.
                let serial = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.subsec_nanos() as i32);
                let enc = VorbisEncoderBuilder::new_with_serial(rate, ch, file, serial)
                    .bitrate_management_strategy(VorbisBitrateManagementStrategy::QualityVbr {
                        target_quality: OGG_QUALITY,
                    })
                    .build()
                    .map_err(io::Error::other)?;
                Self::Ogg(Box::new(enc), vec![Vec::new(); spec.channels])
            }
        })
    }

    fn write(&mut self, interleaved: &[f32]) -> io::Result<()> {
        match self {
            Self::Wav(w) => w.write(interleaved),
            Self::Flac(w) => w.write(interleaved),
            Self::Ogg(enc, planar) => {
                let ch = planar.len();
                for (c, p) in planar.iter_mut().enumerate() {
                    p.clear();
                    p.extend(interleaved.iter().skip(c).step_by(ch));
                }
                enc.encode_audio_block(&*planar).map_err(io::Error::other)
            }
        }
    }

    fn finish(self) -> io::Result<()> {
        match self {
            Self::Wav(w) => w.finish(),
            Self::Flac(w) => w.finish(),
            Self::Ogg(enc, _) => enc
                .finish()
                .map_err(io::Error::other)?
                .into_inner()?
                .sync_all(),
        }
    }
}

fn invalid(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, msg)
}

/// Float to integer PCM: `round(x * 2^(bits-1))`, clamped. JUCE instead scales by
/// `INT_MAX` and truncates (`juce_AudioFormatWriter.cpp:64-79` then `>> (32 - bits)`), which can
/// be one LSB lower; this rounding keeps `k / 2^(bits-1)` inputs exact.
fn quantize(x: f32, bits: u16) -> i32 {
    let full = (1i64 << (bits - 1)) as f64;
    (f64::from(x) * full).round().clamp(-full, full - 1.0) as i32
}

// ---------------------------------------------------------------------------------------------
// WAV

struct WavWriter {
    file: BufWriter<File>,
    channels: u16,
    rate: u32,
    bits: u16,
    data_bytes: u64,
    scratch: Vec<u8>,
}

impl WavWriter {
    fn new(mut file: BufWriter<File>, spec: RecordSpec) -> io::Result<Self> {
        let channels =
            u16::try_from(spec.channels).map_err(|_| invalid("too many channels for WAV"))?;
        file.write_all(&wav_header(channels, spec.sample_rate, spec.bits, 0))?;
        Ok(Self {
            file,
            channels,
            rate: spec.sample_rate,
            bits: spec.bits,
            data_bytes: 0,
            scratch: Vec::new(),
        })
    }

    fn write(&mut self, interleaved: &[f32]) -> io::Result<()> {
        let bytes = usize::from(self.bits / 8);
        self.scratch.clear();
        for &x in interleaved {
            self.scratch
                .extend_from_slice(&quantize(x, self.bits).to_le_bytes()[..bytes]);
        }
        self.file.write_all(&self.scratch)?;
        self.data_bytes += self.scratch.len() as u64;
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        if self.data_bytes & 1 == 1 {
            self.file.write_all(&[0])?;
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.file.write_all(&wav_header(
            self.channels,
            self.rate,
            self.bits,
            self.data_bytes,
        ))?;
        self.file.into_inner()?.sync_all()
    }
}

/// Byte-for-byte the header JUCE's `WavAudioFormatWriter::writeHeader` produces without
/// metadata (juce_WavAudioFormat.cpp:1678-1797): always 104 bytes, a `JUNK` chunk reserving room
/// for `ds64` so the file can turn into RF64 past 4 GiB, `WAVE_FORMAT_EXTENSIBLE` with the
/// canonical channel mask for 3-8 channels (and for RF64), plain PCM otherwise.
fn wav_header(channels: u16, rate: u32, bits: u16, data_bytes: u64) -> [u8; 104] {
    // canonicalWavChannelSet: LCR, quad, 5.0, 5.1, 7.0 SDDS, 7.1 SDDS; mono/stereo/discrete -> 0.
    let mask: u32 = match channels {
        3 => 0x7,
        4 => 0x33,
        5 => 0x37,
        6 => 0x3f,
        7 => 0xf7,
        8 => 0xff,
        _ => 0,
    };
    let rf64 = data_bytes >= 1 << 32;
    let ext = rf64 || mask != 0;
    let frame_bytes = u32::from(channels) * u32::from(bits) / 8;
    let riff_size = 4 + 8 + 40 + 8 + data_bytes + (data_bytes & 1) + 8 + 28;

    let mut h = Vec::with_capacity(104);
    if rf64 {
        h.extend_from_slice(b"RF64");
        h.extend_from_slice(&u32::MAX.to_le_bytes());
        h.extend_from_slice(b"WAVE");
        h.extend_from_slice(b"ds64");
        h.extend_from_slice(&28u32.to_le_bytes());
        h.extend_from_slice(&riff_size.to_le_bytes());
        h.extend_from_slice(&data_bytes.to_le_bytes());
        h.extend_from_slice(&[0; 12]);
    } else {
        h.extend_from_slice(b"RIFF");
        h.extend_from_slice(&(riff_size as u32).to_le_bytes());
        h.extend_from_slice(b"WAVE");
        let junk = if ext { 28u32 } else { 28 + 24 };
        h.extend_from_slice(b"JUNK");
        h.extend_from_slice(&junk.to_le_bytes());
        h.resize(h.len() + junk as usize, 0);
    }
    h.extend_from_slice(b"fmt ");
    h.extend_from_slice(&(if ext { 40u32 } else { 16 }).to_le_bytes());
    h.extend_from_slice(&(if ext { 0xfffeu16 } else { 1 }).to_le_bytes());
    h.extend_from_slice(&channels.to_le_bytes());
    h.extend_from_slice(&rate.to_le_bytes());
    h.extend_from_slice(&(frame_bytes * rate).to_le_bytes());
    h.extend_from_slice(&(frame_bytes as u16).to_le_bytes());
    h.extend_from_slice(&bits.to_le_bytes());
    if ext {
        h.extend_from_slice(&22u16.to_le_bytes());
        h.extend_from_slice(&bits.to_le_bytes());
        h.extend_from_slice(&mask.to_le_bytes());
        // KSDATAFORMAT_SUBTYPE_PCM
        h.extend_from_slice(&[
            1, 0, 0, 0, 0, 0, 0x10, 0, 0x80, 0, 0, 0xaa, 0, 0x38, 0x9b, 0x71,
        ]);
    }
    h.extend_from_slice(b"data");
    h.extend_from_slice(&(if rf64 { u32::MAX } else { data_bytes as u32 }).to_le_bytes());
    h.try_into().expect("header is 104 bytes")
}

// ---------------------------------------------------------------------------------------------
// FLAC

struct FlacWriter {
    file: BufWriter<File>,
    config: Verified<flacenc::config::Encoder>,
    info: StreamInfo,
    framebuf: FrameBuf,
    context: Context,
    bits: u16,
    pending: Vec<i32>,
    sink: ByteSink,
}

impl FlacWriter {
    fn new(file: BufWriter<File>, spec: RecordSpec) -> io::Result<Self> {
        let info = StreamInfo::new(spec.sample_rate as usize, spec.channels, spec.bits.into())
            .map_err(io::Error::other)?;
        let config = flacenc::config::Encoder::default()
            .into_verified()
            .map_err(|(_, e)| io::Error::other(e))?;
        let mut w = Self {
            file,
            framebuf: FrameBuf::with_size(spec.channels, FLAC_BLOCK).map_err(io::Error::other)?,
            context: Context::new(spec.bits.into(), spec.channels),
            config,
            info,
            bits: spec.bits,
            pending: Vec::with_capacity(FLAC_BLOCK * spec.channels),
            sink: ByteSink::new(),
        };
        w.write_header()?;
        Ok(w)
    }

    /// `fLaC` + the only metadata block, STREAMINFO (rewritten in place by `finish`).
    fn write_header(&mut self) -> io::Result<()> {
        self.sink.clear();
        self.info
            .write(&mut self.sink)
            .map_err(|e| io::Error::other(e.to_string()))?;
        self.file.write_all(b"fLaC")?;
        self.file.write_all(&[0x80, 0, 0, 34])?;
        self.file.write_all(self.sink.as_slice())
    }

    fn write(&mut self, interleaved: &[f32]) -> io::Result<()> {
        let block = FLAC_BLOCK * self.info.channels();
        for &x in interleaved {
            self.pending.push(quantize(x, self.bits));
            if self.pending.len() == block {
                self.encode_pending()?;
            }
        }
        Ok(())
    }

    fn encode_pending(&mut self) -> io::Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let err = |e: &dyn std::fmt::Display| io::Error::other(e.to_string());
        self.framebuf
            .fill_interleaved(&self.pending)
            .map_err(|e| err(&e))?;
        self.context
            .fill_interleaved(&self.pending)
            .map_err(|e| err(&e))?;
        self.pending.clear();
        let frame_no = self.context.current_frame_number().expect("filled above");
        let frame =
            flacenc::encode_fixed_size_frame(&self.config, &self.framebuf, frame_no, &self.info)
                .map_err(|e| err(&e))?;
        self.sink.clear();
        frame.write(&mut self.sink).map_err(|e| err(&e))?;
        self.file.write_all(self.sink.as_slice())?;
        self.info.update_frame_info(&frame);
        Ok(())
    }

    fn finish(mut self) -> io::Result<()> {
        self.encode_pending()?;
        self.info.set_md5_digest(&self.context.md5_digest());
        // Fixed-blocksize stream: the short last block does not count (FLAC format spec).
        self.info
            .set_block_sizes(FLAC_BLOCK, FLAC_BLOCK)
            .map_err(io::Error::other)?;
        if self.info.total_samples() == 0 {
            // No frames: STREAMINFO still needs sane frame sizes.
            self.info.set_frame_sizes(0, 0).map_err(io::Error::other)?;
        }
        self.file.seek(SeekFrom::Start(0))?;
        self.write_header()?;
        self.file.into_inner()?.sync_all()
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::play::decode_all;

    pub(crate) fn tmp(name: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("sbfiles-{}-{n}-{name}", std::process::id()))
    }

    /// Deterministic multichannel test signal: channel c is a sine at 220*(c+1) Hz, amplitude
    /// 0.5, quantized to `k / 2^(bits-1)` so integer formats must round-trip it exactly.
    pub(crate) fn signal(channels: usize, frames: usize, rate: u32, bits: u16) -> Vec<Vec<f32>> {
        let full = (1i64 << (bits - 1)) as f64;
        (0..channels)
            .map(|c| {
                (0..frames)
                    .map(|i| {
                        let t = i as f64 / f64::from(rate);
                        let x = 0.5 * (std::f64::consts::TAU * 220.0 * (c + 1) as f64 * t).sin();
                        ((x * full).round() / full) as f32
                    })
                    .collect()
            })
            .collect()
    }

    pub(crate) fn record(path: &Path, spec: RecordSpec, sig: &[Vec<f32>]) -> PathBuf {
        // Ring big enough for the whole signal: debug-build encoders under parallel tests can
        // be slower than real time.
        let frames = sig[0].len();
        let (mut w, rec) = start_inner(path, spec, frames, Duration::ZERO, 0).unwrap();
        // Feed in audio-callback sized blocks.
        for start in (0..frames).step_by(256) {
            let end = (start + 256).min(frames);
            let block: Vec<&[f32]> = sig.iter().map(|c| &c[start..end]).collect();
            assert!(w.write(&block), "ring overflow in test");
        }
        assert_eq!(w.dropped_frames(), 0);
        rec.stop().unwrap()
    }

    fn round_trip_exact(format: RecordFormat, bits: u16, channels: usize) {
        let rate = 48000;
        let sig = signal(channels, 20000, rate, bits);
        let spec = RecordSpec {
            format,
            bits,
            sample_rate: rate,
            channels,
        };
        let path = tmp(&format!("rt.{}", spec.effective_format().extension()));
        let path = record(&path, spec, &sig);
        let (info, got) = decode_all(&path).unwrap();
        assert_eq!(info.sample_rate, rate);
        assert_eq!(info.channels, channels);
        assert_eq!(info.frames, Some(20000));
        assert_eq!(got.len(), channels);
        for c in 0..channels {
            assert_eq!(got[c].len(), 20000, "{format:?}/{bits}/{channels} ch {c}");
            assert!(
                got[c] == sig[c],
                "{format:?}/{bits}/{channels} ch {c} differs"
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wav_round_trips_bit_exact() {
        round_trip_exact(RecordFormat::Wav, 16, 2);
        round_trip_exact(RecordFormat::Wav, 24, 2);
        round_trip_exact(RecordFormat::Wav, 16, 1);
        round_trip_exact(RecordFormat::Wav, 24, 6); // WAVE_FORMAT_EXTENSIBLE
    }

    #[test]
    fn flac_round_trips_bit_exact() {
        round_trip_exact(RecordFormat::Flac, 16, 2);
        round_trip_exact(RecordFormat::Flac, 24, 2);
        round_trip_exact(RecordFormat::Flac, 16, 1);
        round_trip_exact(RecordFormat::Flac, 24, 8);
    }

    #[test]
    fn flac_above_8_channels_falls_back_to_wav() {
        let spec = RecordSpec {
            format: RecordFormat::Flac,
            bits: 16,
            sample_rate: 48000,
            channels: 10,
        };
        assert_eq!(spec.effective_format(), RecordFormat::Wav);
        round_trip_exact(RecordFormat::Flac, 16, 10);
    }

    #[test]
    fn ogg_round_trip_snr() {
        let (rate, channels, frames) = (48000, 2, 48000);
        let sig = signal(channels, frames, rate, 24);
        let spec = RecordSpec {
            format: RecordFormat::Ogg,
            bits: 16,
            sample_rate: rate,
            channels,
        };
        let path = record(&tmp("rt.ogg"), spec, &sig);
        let (info, got) = decode_all(&path).unwrap();
        assert_eq!((info.sample_rate, info.channels), (rate, channels));
        for c in 0..channels {
            assert_eq!(got[c].len(), frames);
            let (mut s, mut n) = (0f64, 0f64);
            for (a, b) in sig[c].iter().zip(&got[c]) {
                s += f64::from(*a).powi(2);
                n += f64::from(a - b).powi(2);
            }
            let snr = 10.0 * (s / n).log10();
            assert!(snr > 30.0, "ch {c} snr {snr:.1} dB");
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wav_header_lengths_after_stop() {
        let sig = signal(2, 1001, 44100, 24);
        let spec = RecordSpec {
            format: RecordFormat::Wav,
            bits: 24,
            sample_rate: 44100,
            channels: 2,
        };
        let path = record(&tmp("len.wav"), spec, &sig);
        let bytes = std::fs::read(&path).unwrap();
        let data = 1001 * 2 * 3;
        assert_eq!(bytes.len(), 104 + data); // 6006 is even: no pad byte
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(bytes[4..8].try_into().unwrap()) as usize,
            bytes.len() - 8
        );
        assert_eq!(&bytes[96..100], b"data");
        assert_eq!(
            u32::from_le_bytes(bytes[100..104].try_into().unwrap()) as usize,
            data
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn wav_header_bytes_match_juce_layout() {
        // Stereo 16-bit 48 kHz, 4 bytes of data: hand-derived from WavAudioFormatWriter::writeHeader.
        let mut want = Vec::new();
        want.extend_from_slice(b"RIFF");
        want.extend_from_slice(&(96u32 + 4).to_le_bytes());
        want.extend_from_slice(b"WAVEJUNK");
        want.extend_from_slice(&52u32.to_le_bytes());
        want.extend_from_slice(&[0; 52]);
        want.extend_from_slice(b"fmt ");
        want.extend_from_slice(&[16, 0, 0, 0, 1, 0, 2, 0]);
        want.extend_from_slice(&48000u32.to_le_bytes());
        want.extend_from_slice(&192000u32.to_le_bytes());
        want.extend_from_slice(&[4, 0, 16, 0]);
        want.extend_from_slice(b"data");
        want.extend_from_slice(&4u32.to_le_bytes());
        assert_eq!(wav_header(2, 48000, 16, 4).as_slice(), want.as_slice());

        // 6 channels: extensible fmt (mask 5.1 = 0x3f), 28-byte JUNK.
        let h = wav_header(6, 44100, 24, 18);
        assert_eq!(&h[12..20], b"JUNK\x1c\0\0\0");
        assert_eq!(&h[48..58], b"fmt \x28\0\0\0\xfe\xff");
        assert_eq!(&h[76..80], &0x3fu32.to_le_bytes());
        assert_eq!(&h[96..104], b"data\x12\0\0\0");

        // Past 4 GiB: RF64 with the real sizes in ds64.
        let big = 5u64 << 30;
        let h = wav_header(2, 48000, 16, big);
        assert_eq!(&h[..12], b"RF64\xff\xff\xff\xffWAVE");
        assert_eq!(&h[12..20], b"ds64\x1c\0\0\0");
        assert_eq!(u64::from_le_bytes(h[20..28].try_into().unwrap()), 96 + big);
        assert_eq!(u64::from_le_bytes(h[28..36].try_into().unwrap()), big);
        assert_eq!(&h[48..58], b"fmt \x28\0\0\0\xfe\xff");
        assert_eq!(&h[96..104], b"data\xff\xff\xff\xff");
    }

    #[test]
    fn quantize_rounds_and_clamps() {
        assert_eq!(quantize(1.0, 16), 32767);
        assert_eq!(quantize(-1.0, 16), -32768);
        assert_eq!(quantize(2.0, 24), 8_388_607);
        assert_eq!(quantize(0.5 / 32768.0, 16), 1); // round half away from zero
        assert_eq!(quantize(-3.0 / 32768.0, 16), -3);
    }

    #[test]
    fn slow_disk_drops_instead_of_blocking() {
        let spec = RecordSpec {
            format: RecordFormat::Wav,
            bits: 16,
            sample_rate: 48000,
            channels: 2,
        };
        // 1024-frame ring, disk thread sleeps 50 ms after every drained chunk.
        let (mut w, rec) =
            start_inner(&tmp("slow.wav"), spec, 1024, Duration::from_millis(50), 0).unwrap();
        let block = vec![0.25f32; 256];
        let mut written = 0u64;
        let t0 = std::time::Instant::now();
        for _ in 0..200 {
            if w.write(&[&block, &block]) {
                written += 256;
            }
        }
        // 200 calls against a stalled disk must not have waited for it.
        assert!(
            t0.elapsed() < Duration::from_millis(40),
            "{:?}",
            t0.elapsed()
        );
        assert!(w.dropped_frames() > 0);
        assert_eq!(written + w.dropped_frames(), 200 * 256);
        assert_eq!(rec.dropped_frames(), w.dropped_frames());
        let path = rec.stop().unwrap();
        let (_, got) = decode_all(&path).unwrap();
        assert_eq!(got[0].len() as u64, written);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn existing_file_is_not_clobbered() {
        let p = tmp("dup.wav");
        std::fs::write(&p, b"keep").unwrap();
        let spec = RecordSpec {
            format: RecordFormat::Wav,
            bits: 16,
            sample_rate: 8000,
            channels: 1,
        };
        let got = record(&p, spec, &signal(1, 10, 8000, 16));
        assert_ne!(got, p);
        assert!(got.to_string_lossy().ends_with("dup(2).wav"));
        assert_eq!(std::fs::read(&p).unwrap(), b"keep");
        std::fs::remove_file(p).unwrap();
        std::fs::remove_file(got).unwrap();
    }

    /// A track joining a running recording: the silence set after the writer was made (from
    /// the "audio thread"), then the audio, also what arrived before the silence was set and
    /// while it was written. A long silence is written without stalling the ring.
    #[test]
    fn leading_silence() {
        let spec = RecordSpec {
            format: RecordFormat::Flac,
            bits: 16,
            sample_rate: 48000,
            channels: 2,
        };
        let sig = signal(2, 4000, 48000, 16);
        let (mut w, rec) = start_recording_after_silence(&tmp("lead.flac"), spec).unwrap();
        let blocks: Vec<Vec<&[f32]>> = (0..4000)
            .step_by(500)
            .map(|s| sig.iter().map(|c| &c[s..s + 500]).collect())
            .collect();
        assert!(w.write(&blocks[0]));
        std::thread::sleep(Duration::from_millis(30)); // nothing written yet: lead unknown
        // 10 minutes of silence; only the first call counts
        let lead = 48000 * 600;
        w.set_leading_silence(lead);
        w.set_leading_silence(5);
        for b in &blocks[1..] {
            assert!(w.write(b));
        }
        let path = rec.stop().unwrap();
        // FLAC stores the silence in a few bytes per block
        assert!(std::fs::metadata(&path).unwrap().len() < 2_000_000);
        let (_, got) = decode_all(&path).unwrap();
        for c in 0..2 {
            assert_eq!(got[c].len() as u64, lead + 4000);
            assert!(got[c][..lead as usize].iter().all(|&x| x == 0.0));
            assert!(got[c][lead as usize..] == sig[c][..], "ch {c}");
        }
        std::fs::remove_file(path).unwrap();

        // never set (writer dropped before use) or a plain writer: no silence
        let spec = RecordSpec { format: RecordFormat::Wav, ..spec };
        let (mut w, rec) = start_recording_after_silence(&tmp("nolead.wav"), spec).unwrap();
        assert!(w.write(&blocks[0]));
        let path = rec.stop().unwrap();
        assert_eq!(decode_all(&path).unwrap().1[0].len(), 500);
        std::fs::remove_file(path).unwrap();
        let (mut w, rec) = start_recording(&tmp("plain.wav"), spec).unwrap();
        w.set_leading_silence(1000);
        assert!(w.write(&blocks[0]));
        let path = rec.stop().unwrap();
        assert_eq!(decode_all(&path).unwrap().1[0].len(), 500);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_bad_spec() {
        let spec = RecordSpec {
            format: RecordFormat::Wav,
            bits: 12,
            sample_rate: 48000,
            channels: 2,
        };
        assert!(start_recording(&tmp("bad.wav"), spec).is_err());
    }
}
