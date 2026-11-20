//! Audio streams of protocol v2 (sans-IO: byte slices, [`Bytes`] and caller-supplied
//! timestamps in, datagrams out).
//!
//! [`TxStream`] encodes one outgoing stream once per encoding and turns every block into AUDIO
//! datagrams for any number of subscribers; [`RxStream`] turns the datagrams of one incoming
//! stream back into audio. Both split into a network half and an audio half like
//! `aoo::source` / `aoo::sink`: [`TxAudio::process`] and [`RxAudio::process`] run on the audio
//! thread and never allocate, lock, block or log; everything else runs on the network side.
//!
//! Wire conventions defined here (the datagram framing itself is [`wire`]):
//! - FORMAT `codec`: [`CODEC_PCM`] or [`CODEC_OPUS`]; all encodings of a stream share it.
//!   `encodings` = `[0, 1, ..]` (encoding 0 = main, 1 = low). `options` = `u8 flags` (bit 0:
//!   voice preset) then per encoding PCM `u8 depth` (`aoo::codec::PcmBitDepth`) or Opus
//!   `u32 bitrate (LE) | u8 complexity`.
//! - PCM payload: interleaved big-endian samples, as AOO (`aoo::codec::pcm_encode`); the
//!   receiver infers the depth from the payload length, so encodings can switch freely.
//!   Opus payload: one coupled multistream packet (1-2 channels: plain Opus), frame = block.
//! - Every generation starts at sequence 0 with the discontinuity flag. The sequence never wraps
//!   inside a generation: after `u32::MAX` the sender bumps the generation (a new FORMAT goes
//!   out) and restarts at 0, so the receiver's extended (u64) sequence is the wire one.
//! - NACK fragment index [`NACK_ALL`] asks for every fragment of the block; the receiver uses it
//!   when nothing of the block arrived and names the missing fragments otherwise.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aoo::codec::{
    OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_SIGNAL_MUSIC, PcmBitDepth, pcm_decode, pcm_encode,
};
use aoo::opus;
use aoo::resampler::Resampler;
use aoo::time::{Dll, TIMEFILTER_BANDWIDTH};
use bytes::Bytes;
use rtrb::{Consumer, Producer, RingBuffer};

use crate::wire::{
    self, AUDIO_HEADER, Audio, AudioHeader, Error, Format, MAX_DATAGRAM, MAX_NACK, Nack,
    Reassembler, Report,
};

pub const CODEC_PCM: u8 = 0;
pub const CODEC_OPUS: u8 = 1;
/// Encodings per stream (main and low).
pub const MAX_ENCODINGS: usize = 2;
/// Largest redundancy `k` a subscriber gets.
pub const MAX_REDUNDANCY: u8 = 2;
/// NACK fragment index: every fragment of the block.
pub const NACK_ALL: u8 = 255;

/// Format bounds, so a peer cannot make us allocate without limit.
pub const MAX_CHANNELS: u8 = 64;
const MIN_RATE: u32 = 8000;
const MAX_RATE: u32 = 192_000;
/// `sample_rate * channels` cap (48 kHz x 64, 192 kHz x 16).
const MAX_RATE_CHANNELS: u64 = 3_072_000;
const MIN_BLOCK: u16 = 16;
const MAX_BLOCK_FRAMES: u16 = 8192;
/// Largest Opus packet per stream (1275 B frame + self-delimiting length).
const OPUS_MAX_PACKET: usize = 1277;
/// libopus `OPUS_APPLICATION_VOIP` / `OPUS_SIGNAL_VOICE`.
const OPUS_APPLICATION_VOIP: i32 = 2048;
const OPUS_SIGNAL_VOICE: i32 = 3001;
const FLAG_VOICE: u8 = 1;
/// Most datagrams one NACK makes us send.
const MAX_RESEND_PER_NACK: usize = 64;
/// A subscriber's resend budget: this share of its stream's bit rate...
pub const RESEND_SHARE: f64 = 0.25;
/// ...with a burst of at most this long at that rate, but at least two full datagrams.
pub const RESEND_BURST: Duration = Duration::from_millis(100);
/// Times one fragment of one block is resent to one subscriber.
pub const RESEND_RETRIES: u8 = 3;
/// Encoded bytes the receive queue may hold.
const QUEUE_BUDGET: usize = 8 << 20;
const MAX_EVENTS: usize = 256;
/// Largest jitter buffer target.
pub const MAX_BUFFER_MS: f32 = 2000.0;
/// Playout anchor: share of each callback's time estimate in the smoothed one, and the
/// deviation (s) taken as a real jump instead.
const ANCHOR_SMOOTH: f64 = 0.05;
const ANCHOR_JUMP: f64 = 0.020;
/// Playout sync: early by more than this (s), silence is inserted at once; less, the resampling
/// slows down by at most [`SYNC_MAX_STEER`], proportionally ([`SYNC_GAIN`] per second of
/// error). Late is left alone (sync only ever adds latency); a target that moves earlier by more
/// than this restarts buffering.
pub const SYNC_HARD: f64 = 0.010;
pub const SYNC_MAX_STEER: f64 = 0.002;
const SYNC_GAIN: f64 = 1.0;
/// Time constant (s) of the smoothed error (callback times are jittery).
const SYNC_SMOOTH: f64 = 0.2;
/// No new correction this long after a hard one (the audio half applies it first).
const SYNC_HOLD: Duration = Duration::from_millis(100);
/// Playout sync keeps the least headroom of arriving blocks per second for this many seconds
/// (going late looks at the last one to two, and at whether it went late before in them).
const LOWS: usize = 10;
/// Playout sync: the most it adds beyond the deficit when it has to go late again (s).
const SYNC_LATE_STEP: f64 = 0.030;
/// Playout sync's error reading counts as current while a block arrived within this.
const SYNC_STALE: Duration = Duration::from_millis(200);

/// `t - epoch` in seconds, negative when `t` is earlier.
fn secs(t: Instant, epoch: Instant) -> f64 {
    match t.checked_duration_since(epoch) {
        Some(d) => d.as_secs_f64(),
        None => -epoch.duration_since(t).as_secs_f64(),
    }
}

/// `epoch + secs` (negative: earlier).
fn at(epoch: Instant, secs: f64) -> Instant {
    let d = Duration::from_secs_f64(secs.abs().min(1e9));
    if secs >= 0.0 {
        epoch + d
    } else {
        epoch.checked_sub(d).unwrap_or(epoch)
    }
}

// ---------------------------------------------------------------------------------------------
// Format

/// One encoding of a stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Pcm(PcmBitDepth),
    /// `bitrate` in bit/s over all channels.
    Opus {
        bitrate: u32,
        complexity: u8,
    },
}

/// What a FORMAT frame describes, minus stream id and generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFormat {
    pub sample_rate: u32,
    /// Frames per block. Opus: 2.5, 5, 10 or 20 ms.
    pub block_size: u16,
    pub channels: u8,
    /// Voice preset: Opus with in-band FEC (and the expected loss from `adapt`) when blocks are
    /// 10 ms or longer.
    pub voice: bool,
    /// 1..=[`MAX_ENCODINGS`] encodings of one codec; index = encoding id.
    pub encodings: Vec<Encoding>,
}

impl StreamFormat {
    pub fn codec(&self) -> u8 {
        match self.encodings.first() {
            Some(Encoding::Opus { .. }) => CODEC_OPUS,
            _ => CODEC_PCM,
        }
    }

    /// Opus in-band FEC is on (voice preset with frames of 10 ms or more).
    pub fn fec(&self) -> bool {
        self.voice
            && self.codec() == CODEC_OPUS
            && u32::from(self.block_size) * 100 >= self.sample_rate
    }

    /// Largest encoded block of any encoding.
    pub fn max_block_bytes(&self) -> usize {
        let samples = usize::from(self.block_size) * usize::from(self.channels);
        let streams = usize::from(self.channels).div_ceil(2);
        self.encodings
            .iter()
            .map(|e| match e {
                Encoding::Pcm(d) => samples * d.sample_size(),
                Encoding::Opus { .. } => OPUS_MAX_PACKET * streams,
            })
            .max()
            .unwrap_or(0)
    }

    fn block_secs(&self) -> f64 {
        f64::from(self.block_size) / f64::from(self.sample_rate)
    }

    /// Bounds and codec constraints; what [`Self::from_wire`] accepts.
    pub fn validate(&self) -> Result<(), Error> {
        let same_codec = self
            .encodings
            .iter()
            .all(|e| std::mem::discriminant(e) == std::mem::discriminant(&self.encodings[0]));
        let ok = (1..=MAX_ENCODINGS).contains(&self.encodings.len())
            && same_codec
            && (1..=MAX_CHANNELS).contains(&self.channels)
            && (MIN_RATE..=MAX_RATE).contains(&self.sample_rate)
            && u64::from(self.sample_rate) * u64::from(self.channels) <= MAX_RATE_CHANNELS
            && (MIN_BLOCK..=MAX_BLOCK_FRAMES).contains(&self.block_size);
        if !ok {
            return Err(Error::Invalid);
        }
        if self.codec() == CODEC_OPUS {
            let f = self.sample_rate / 400;
            if ![8000, 12000, 16000, 24000, 48000].contains(&self.sample_rate)
                || ![f, 2 * f, 4 * f, 8 * f].contains(&u32::from(self.block_size))
            {
                return Err(Error::Invalid);
            }
        }
        if self.max_block_bytes() > wire::MAX_BLOCK {
            return Err(Error::TooLong);
        }
        Ok(())
    }

    pub fn to_wire(&self, stream: u8, generation: u8) -> Format {
        let mut options = vec![if self.voice { FLAG_VOICE } else { 0 }];
        for e in &self.encodings {
            match *e {
                Encoding::Pcm(d) => options.push(d as u8),
                Encoding::Opus {
                    bitrate,
                    complexity,
                } => {
                    options.extend_from_slice(&bitrate.to_le_bytes());
                    options.push(complexity);
                }
            }
        }
        Format {
            stream,
            generation,
            codec: self.codec(),
            sample_rate: self.sample_rate,
            block_size: self.block_size,
            channels: self.channels,
            options,
            encodings: (0..self.encodings.len() as u8).collect(),
        }
    }

    pub fn from_wire(f: &Format) -> Result<Self, Error> {
        if f.encodings
            .iter()
            .enumerate()
            .any(|(i, &e)| usize::from(e) != i)
        {
            return Err(Error::Invalid);
        }
        let mut r = wire::Rd(&f.options);
        let flags = r.u8()?;
        let encodings = f
            .encodings
            .iter()
            .map(|_| match f.codec {
                CODEC_PCM => PcmBitDepth::from_bytes(&[0, 0, 0, r.u8()?])
                    .map(Encoding::Pcm)
                    .map_err(|_| Error::Invalid),
                CODEC_OPUS => Ok(Encoding::Opus {
                    bitrate: r.u32()?,
                    complexity: r.u8()?,
                }),
                _ => Err(Error::Invalid),
            })
            .collect::<Result<_, _>>()?;
        let sf = Self {
            sample_rate: f.sample_rate,
            block_size: f.block_size,
            channels: f.channels,
            voice: flags & FLAG_VOICE != 0,
            encodings,
        };
        sf.validate()?;
        Ok(sf)
    }
}

/// Audio device the audio half runs on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Device {
    pub sample_rate: u32,
    /// Largest callback in frames (callbacks may be shorter).
    pub block_size: usize,
    pub channels: usize,
}

// ---------------------------------------------------------------------------------------------
// Codecs

enum Enc {
    Pcm(PcmBitDepth),
    /// `bool`: FEC on.
    Opus(opus::Encoder, bool),
}

impl Enc {
    fn new(f: &StreamFormat, e: Encoding) -> Result<Self, Error> {
        Ok(match e {
            Encoding::Pcm(d) => Enc::Pcm(d),
            Encoding::Opus {
                bitrate,
                complexity,
            } => {
                let fec = f.fec();
                let (app, signal) = if fec {
                    (OPUS_APPLICATION_VOIP, OPUS_SIGNAL_VOICE)
                } else {
                    (OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_SIGNAL_MUSIC)
                };
                let (mut enc, ..) = opus::Encoder::with_coupling(
                    f.sample_rate as i32,
                    i32::from(f.channels),
                    true,
                    app,
                    bitrate.min(i32::MAX as u32) as i32,
                    i32::from(complexity),
                    signal,
                )
                .map_err(|_| Error::Invalid)?;
                if fec {
                    enc.set_fec(true, 0);
                }
                Enc::Opus(enc, fec)
            }
        })
    }

    /// Encoded size, 0 on failure.
    fn encode(&mut self, block: &[f32], out: &mut [u8]) -> usize {
        match self {
            Enc::Pcm(d) => pcm_encode(*d, block, out),
            Enc::Opus(e, _) => e.encode(block, out).unwrap_or(0),
        }
    }
}

enum Dec {
    /// Last frame played (for the fade-out of a concealed block) and whether the previous
    /// block was concealed (the next real one fades in).
    Pcm {
        last: Vec<f32>,
        concealed: bool,
    },
    Opus(opus::Decoder),
}

/// Fade length of PCM concealment and recovery, frames.
const PCM_FADE: usize = 64;

impl Dec {
    fn new(f: &StreamFormat) -> Result<Self, Error> {
        let nch = usize::from(f.channels);
        Ok(match f.codec() {
            CODEC_OPUS => Dec::Opus(
                opus::Decoder::with_coupling(f.sample_rate as i32, nch as i32, true)
                    .map_err(|_| Error::Invalid)?,
            ),
            _ => Dec::Pcm {
                last: vec![0.0; nch],
                concealed: false,
            },
        })
    }

    fn reset(&mut self) {
        match self {
            Dec::Pcm { last, concealed } => {
                last.fill(0.0);
                *concealed = false;
            }
            Dec::Opus(d) => d.reset(),
        }
    }

    /// One block into `out`: decoded from `data`, or concealed when that is `None` or does not
    /// decode (Opus: FEC from `next` when given, else PLC; PCM: a short fade to silence).
    /// Output is sanitised. Returns whether `out` holds decoded data.
    fn block(
        &mut self,
        data: Option<&[u8]>,
        next: Option<&[u8]>,
        nch: usize,
        out: &mut [f32],
    ) -> bool {
        let ok = match self {
            Dec::Pcm { last, concealed } => {
                let ok = data.is_some_and(|d| pcm_block(d, out));
                sanitize(out);
                let frames = out.len() / nch;
                let fade = PCM_FADE.min(frames);
                if ok {
                    if *concealed {
                        for (f, frame) in out.chunks_exact_mut(nch).take(fade).enumerate() {
                            let g = (f + 1) as f32 / (fade + 1) as f32;
                            frame.iter_mut().for_each(|x| *x *= g);
                        }
                    }
                } else {
                    out.fill(0.0);
                    for (f, frame) in out.chunks_exact_mut(nch).take(fade).enumerate() {
                        let g = 1.0 - (f + 1) as f32 / fade as f32;
                        for (x, l) in frame.iter_mut().zip(last.iter()) {
                            *x = l * g;
                        }
                    }
                }
                *concealed = !ok;
                last.copy_from_slice(&out[out.len() - nch..]);
                ok
            }
            Dec::Opus(d) => match data.and_then(|x| d.decode(Some(x), out).ok()) {
                Some(n) => {
                    let len = out.len();
                    out[(n * nch).min(len)..].fill(0.0);
                    true
                }
                None => {
                    let n = next
                        .and_then(|x| d.decode_fec(x, out).ok())
                        .or_else(|| d.decode(None, out).ok())
                        .unwrap_or(0);
                    let len = out.len();
                    out[(n * nch).min(len)..].fill(0.0);
                    false
                }
            },
        };
        sanitize(out);
        ok
    }
}

/// Non-finite samples become 0, the rest is clamped to +-100 (as the engine does).
fn sanitize(out: &mut [f32]) {
    for x in out.iter_mut() {
        *x = if x.is_finite() {
            x.clamp(-100.0, 100.0)
        } else {
            0.0
        };
    }
}

/// PCM block whose depth follows from its length; `false` if the length fits no depth.
fn pcm_block(data: &[u8], out: &mut [f32]) -> bool {
    if out.is_empty() || !data.len().is_multiple_of(out.len()) {
        return false;
    }
    let depth = match data.len() / out.len() {
        2 => PcmBitDepth::Int16,
        3 => PcmBitDepth::Int24,
        4 => PcmBitDepth::Float32,
        8 => PcmBitDepth::Float64,
        _ => return false,
    };
    pcm_decode(depth, data, out);
    true
}

// ---------------------------------------------------------------------------------------------
// Sender

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TxOptions {
    /// Audio-to-network ring, ms (at least two callbacks).
    pub ring_ms: u32,
    /// Resend history, ms.
    pub history_ms: u32,
    /// Send the DLL-measured clock deviation in `drift` (else 0).
    pub dynamic_resampling: bool,
    pub dll_bandwidth: f64,
}

impl Default for TxOptions {
    fn default() -> Self {
        Self {
            ring_ms: 20,
            history_ms: 1000,
            dynamic_resampling: true,
            dll_bandwidth: f64::from(TIMEFILTER_BANDWIDTH),
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct Record {
    t: Instant,
    frames: u32,
}

/// Audio-thread half of a [`TxStream`].
pub struct TxAudio {
    samples: Producer<f32>,
    records: Producer<Record>,
    nch: usize,
}

impl TxAudio {
    /// Queue one callback: one slice per device channel (missing channels are silence), all
    /// the same length. `now` is the callback time. `false` if the network side fell behind
    /// and the callback was dropped. Real-time safe.
    pub fn process(&mut self, input: &[&[f32]], now: Instant) -> bool {
        let n = input.first().map_or(0, |c| c.len());
        let nch = self.nch;
        let total = n * nch;
        if n == 0 || self.records.is_full() || self.samples.slots() < total {
            return false;
        }
        let Ok(chunk) = self.samples.write_chunk_uninit(total) else {
            return false;
        };
        chunk.fill_from_iter((0..n).flat_map(|j| {
            (0..nch).map(move |i| input.get(i).and_then(|c| c.get(j)).copied().unwrap_or(0.0))
        }));
        let _ = self.records.push(Record {
            t: now,
            frames: n as u32,
        });
        true
    }
}

fn tx_rings(d: &Device, ring_ms: u32) -> (TxAudio, Consumer<f32>, Consumer<Record>) {
    let frames = (f64::from(ring_ms) * 1e-3 * f64::from(d.sample_rate)) as usize;
    let frames = frames.max(2 * d.block_size).max(1);
    let nch = d.channels.max(1);
    let (sp, sc) = RingBuffer::new(frames * nch);
    let (rp, rc) = RingBuffer::new(frames.max(256));
    (
        TxAudio {
            samples: sp,
            records: rp,
            nch,
        },
        sc,
        rc,
    )
}

/// Encoded blocks of one sent block, kept for RED and NACKs.
struct Sent {
    seq: u32,
    drift: i16,
    discontinuity: bool,
    /// Per encoding; empty if the encoder failed.
    data: [Bytes; MAX_ENCODINGS],
    /// Resends so far: (subscriber, fragment, count).
    resent: Vec<(u64, u8, u8)>,
}

#[derive(Debug, Clone, Copy)]
struct Sub {
    id: u64,
    encoding: u8,
    redundancy: u8,
    /// Resend token bucket (bytes) and when it was last refilled. A new subscriber starts
    /// empty when it subscribes (the bucket fills while its stream runs): re-subscribing does
    /// not buy a fresh burst.
    tokens: f64,
    refilled: Option<Instant>,
    /// Resends paused (the path is congested).
    paused: bool,
}

/// Network half of one outgoing stream.
pub struct TxStream {
    stream: u8,
    device: Device,
    opts: TxOptions,
    format: StreamFormat,
    generation: u8,
    format_changed: bool,
    encs: Vec<Enc>,
    samples: Consumer<f32>,
    records: Consumer<Record>,
    t0: Option<Instant>,
    dll: Dll,
    drift: i16,
    resampler: Resampler,
    input: Vec<f32>,
    mapped: Vec<f32>,
    block: Vec<f32>,
    scratch: Vec<u8>,
    seq: u32,
    discontinuity: bool,
    history: VecDeque<Sent>,
    history_len: usize,
    subs: Vec<Sub>,
    encodes: u64,
    /// Current Opus bit rate per encoding ([`Self::set_bitrate`]); 0 for PCM.
    bitrates: [u32; MAX_ENCODINGS],
    /// Time base of [`Self::playout_anchor`].
    epoch: Instant,
    /// When frame 0 of this generation entered the audio half, s since `epoch` (smoothed).
    base: Option<f64>,
}

impl TxStream {
    /// `stream` < 8. Fails on an invalid format.
    pub fn new(
        stream: u8,
        device: Device,
        format: StreamFormat,
        opts: TxOptions,
    ) -> Result<(Self, TxAudio), Error> {
        if stream >= 8 || device.sample_rate == 0 {
            return Err(Error::Invalid);
        }
        format.validate()?;
        let (audio, samples, records) = tx_rings(&device, opts.ring_ms);
        let mut s = Self {
            stream,
            device,
            opts,
            format,
            generation: 0,
            format_changed: true,
            encs: Vec::new(),
            samples,
            records,
            t0: None,
            dll: Dll::default(),
            drift: 0,
            resampler: Resampler::default(),
            input: Vec::new(),
            mapped: Vec::new(),
            block: Vec::new(),
            scratch: Vec::new(),
            seq: 0,
            discontinuity: true,
            history: VecDeque::new(),
            history_len: 0,
            subs: Vec::new(),
            encodes: 0,
            bitrates: [0; MAX_ENCODINGS],
            epoch: Instant::now(),
            base: None,
        };
        s.apply_format()?;
        Ok((s, audio))
    }

    /// New audio device: returns the audio half to install (the old one stops feeding this
    /// stream). The stream continues with a discontinuity.
    pub fn setup(&mut self, device: Device) -> TxAudio {
        let (audio, samples, records) = tx_rings(&device, self.opts.ring_ms);
        self.device = device;
        self.samples = samples;
        self.records = records;
        self.t0 = None;
        self.base = None;
        self.reset_resampler();
        self.discontinuity = true;
        audio
    }

    /// Change the format: new generation, sequence 0, encoders re-created. Subscribers of an
    /// encoding that no longer exists move to encoding 0.
    pub fn set_format(&mut self, format: StreamFormat) -> Result<(), Error> {
        format.validate()?;
        let old = std::mem::replace(&mut self.format, format);
        if let Err(e) = self.apply_format() {
            self.format = old;
            return Err(e);
        }
        self.restart();
        for s in &mut self.subs {
            if usize::from(s.encoding) >= self.encs.len() {
                s.encoding = 0;
            }
        }
        Ok(())
    }

    fn apply_format(&mut self) -> Result<(), Error> {
        let encs = self
            .format
            .encodings
            .iter()
            .map(|&e| Enc::new(&self.format, e))
            .collect::<Result<_, _>>()?;
        self.encs = encs;
        self.bitrates = [0; MAX_ENCODINGS];
        for (b, e) in self.bitrates.iter_mut().zip(&self.format.encodings) {
            if let Encoding::Opus { bitrate, .. } = *e {
                *b = bitrate;
            }
        }
        let f = &self.format;
        self.block = vec![0.0; usize::from(f.block_size) * usize::from(f.channels)];
        self.scratch = vec![0; f.max_block_bytes()];
        self.history_len =
            ((f64::from(self.opts.history_ms) * 1e-3 / f.block_secs()).ceil() as usize).max(1);
        self.history.clear();
        self.reset_resampler();
        Ok(())
    }

    fn reset_resampler(&mut self) {
        let f = &self.format;
        self.resampler.setup(
            self.device.block_size,
            usize::from(f.block_size),
            usize::from(f.channels),
        );
        self.resampler
            .update(f64::from(self.device.sample_rate), f64::from(f.sample_rate));
    }

    fn restart(&mut self) {
        self.generation = (self.generation + 1) % 32;
        self.base = None;
        self.seq = 0;
        self.discontinuity = true;
        self.history.clear();
        self.format_changed = true;
    }

    pub fn format(&self) -> &StreamFormat {
        &self.format
    }

    /// The FORMAT frame body of the current generation.
    pub fn wire_format(&self) -> Format {
        self.format.to_wire(self.stream, self.generation)
    }

    /// The FORMAT to announce when the generation changed since the last call (also once
    /// after creation).
    pub fn take_format_change(&mut self) -> Option<Format> {
        std::mem::take(&mut self.format_changed).then(|| self.wire_format())
    }

    /// Add or update a subscriber. `false` if `encoding` does not exist. `redundancy` is
    /// clamped to [`MAX_REDUNDANCY`].
    pub fn subscribe(&mut self, id: u64, encoding: u8, redundancy: u8) -> bool {
        if usize::from(encoding) >= self.encs.len() {
            return false;
        }
        let redundancy = redundancy.min(MAX_REDUNDANCY);
        match self.subs.iter_mut().find(|s| s.id == id) {
            Some(s) => (s.encoding, s.redundancy) = (encoding, redundancy),
            None => self.subs.push(Sub {
                id,
                encoding,
                redundancy,
                tokens: 0.0,
                refilled: Some(Instant::now()),
                paused: false,
            }),
        }
        true
    }

    /// Pause (or resume) NACK resends to a subscriber, e.g. while its path is congested.
    pub fn set_resend_paused(&mut self, id: u64, paused: bool) {
        if let Some(s) = self.subs.iter_mut().find(|s| s.id == id) {
            s.paused = paused;
        }
    }

    /// Change an Opus encoding's bit rate in place (no new generation: decoders follow any
    /// rate). `false` for PCM or an unknown encoding.
    pub fn set_bitrate(&mut self, encoding: usize, bitrate: u32) -> bool {
        match self.encs.get_mut(encoding) {
            Some(Enc::Opus(e, _)) => {
                e.set_bitrate(bitrate.min(i32::MAX as u32) as i32);
                self.bitrates[encoding] = bitrate;
                true
            }
            _ => false,
        }
    }

    /// Bit rate of encoding `e` as configured (PCM: sample bits x rate x channels).
    pub fn bitrate(&self, e: usize) -> u32 {
        match self.format.encodings.get(e) {
            Some(Encoding::Pcm(d)) => (d.sample_size() as u32 * 8)
                .saturating_mul(self.format.sample_rate)
                .saturating_mul(u32::from(self.format.channels)),
            Some(Encoding::Opus { .. }) => self.bitrates[e],
            None => 0,
        }
    }

    pub fn unsubscribe(&mut self, id: u64) {
        self.subs.retain(|s| s.id != id);
    }

    /// Expected loss for the voice preset's Opus FEC (from `adapt`); no-op without FEC.
    pub fn set_expected_loss(&mut self, percent: u8) {
        for e in &mut self.encs {
            if let Enc::Opus(enc, true) = e {
                enc.set_fec(true, i32::from(percent));
            }
        }
    }

    /// Encoder invocations so far (one per block and encoding).
    pub fn encode_count(&self) -> u64 {
        self.encodes
    }

    /// Playout sync: `(generation, seq, t)`, block `seq` (the next one to be sent) has its
    /// first frame entering the audio half at `t` (callback times, smoothed). `None` before
    /// the first callback of a generation (and after a device change).
    pub fn playout_anchor(&self) -> Option<(u8, u32, Instant)> {
        let b = self.base?;
        let t = b + f64::from(self.seq) * self.format.block_secs();
        Some((self.generation, self.seq, at(self.epoch, t)))
    }

    /// Drain the audio ring: encode every complete block and hand each subscriber its
    /// datagrams (`out(subscriber, datagram)`; subscribers of one encoding and redundancy get
    /// clones of the same [`Bytes`]). Returns the number of blocks sent.
    pub fn pump(&mut self, mut out: impl FnMut(u64, Bytes)) -> usize {
        let dnch = self.device.channels.max(1);
        let snch = usize::from(self.format.channels);
        let sr = f64::from(self.device.sample_rate);
        let mut blocks = 0;
        while let Ok(rec) = self.records.pop() {
            self.input.resize(rec.frames as usize * dnch, 0.0);
            if self.samples.pop_entire_slice(&mut self.input).is_err() {
                break; // cannot happen: samples are pushed before their record
            }
            match self.t0 {
                None => {
                    self.t0 = Some(rec.t);
                    self.dll.setup(
                        sr,
                        self.device.block_size as i32,
                        self.opts.dll_bandwidth,
                        0.0,
                    );
                }
                Some(t0) => self
                    .dll
                    .update(rec.t.saturating_duration_since(t0).as_secs_f64()),
            }
            let dev = self.dll.samplerate() / sr - 1.0;
            self.drift = if self.opts.dynamic_resampling && dev.abs() <= 0.1 {
                (dev * 8e6)
                    .round()
                    .clamp(f64::from(i16::MIN), f64::from(i16::MAX)) as i16
            } else {
                0
            };
            self.mapped.clear();
            for frame in self.input.chunks_exact(dnch) {
                self.mapped
                    .extend((0..snch).map(|c| frame.get(c).copied().unwrap_or(0.0)));
            }
            // this record's first frame becomes stream frame `x` of the generation
            let x = f64::from(self.seq) * f64::from(self.format.block_size)
                + (self.resampler.read_available() / snch) as f64;
            let est = secs(rec.t, self.epoch) - x / f64::from(self.format.sample_rate);
            self.base = Some(match self.base {
                // callback jitter is smoothed; a jump (dropped callbacks) is taken at once
                Some(b) if (est - b).abs() < ANCHOR_JUMP => b + (est - b) * ANCHOR_SMOOTH,
                _ => est,
            });
            let mut pos = 0;
            while pos < self.mapped.len() {
                let take = (self.mapped.len() - pos).min(self.resampler.write_available());
                self.resampler.write(&self.mapped[pos..pos + take]);
                pos += take;
                let mut consumed = false;
                while self.resampler.read_available() >= self.block.len() {
                    self.resampler.read(&mut self.block);
                    self.send_block(&mut out);
                    blocks += 1;
                    consumed = true;
                }
                if take == 0 && !consumed {
                    break;
                }
            }
        }
        blocks
    }

    fn send_block(&mut self, out: &mut impl FnMut(u64, Bytes)) {
        let mut data: [Bytes; MAX_ENCODINGS] = Default::default();
        for (d, enc) in data.iter_mut().zip(&mut self.encs) {
            let n = enc.encode(&self.block, &mut self.scratch);
            self.encodes += 1;
            *d = Bytes::copy_from_slice(&self.scratch[..n]);
        }
        let h = AudioHeader {
            stream: self.stream,
            generation: self.generation,
            drift: self.drift,
            seq: self.seq,
            discontinuity: std::mem::take(&mut self.discontinuity),
        };
        // each distinct datagram is built once per block: [encoding][redundancy]
        let mut cache: [[Option<Vec<Bytes>>; MAX_REDUNDANCY as usize + 1]; MAX_ENCODINGS] =
            Default::default();
        for s in &self.subs {
            let (e, k) = (usize::from(s.encoding), usize::from(s.redundancy));
            let dgs =
                cache[e][k].get_or_insert_with(|| datagrams(&h, &data[e], &self.history, e, k));
            for d in dgs.iter() {
                out(s.id, d.clone());
            }
        }
        self.history.push_back(Sent {
            seq: h.seq,
            drift: h.drift,
            discontinuity: h.discontinuity,
            data,
            resent: Vec::new(),
        });
        while self.history.len() > self.history_len {
            self.history.pop_front();
        }
        if self.seq == u32::MAX {
            self.restart();
        } else {
            self.seq += 1;
        }
    }

    /// [`Self::handle_nack_at`] now.
    pub fn handle_nack(&mut self, id: u64, nack: &Nack, out: impl FnMut(u64, Bytes)) {
        self.handle_nack_at(Instant::now(), id, nack, out);
    }

    /// Resend what a subscriber's NACK asks for, as plain datagrams (no redundancy) of its
    /// encoding, within its budget: [`RESEND_SHARE`] of its encoding's bit rate (burst
    /// [`RESEND_BURST`]), each fragment at most [`RESEND_RETRIES`] times, nothing while
    /// paused. Blocks no longer (or not yet) in the history are skipped.
    pub fn handle_nack_at(
        &mut self,
        now: Instant,
        id: u64,
        nack: &Nack,
        mut out: impl FnMut(u64, Bytes),
    ) {
        if nack.stream != self.stream {
            return;
        }
        let Some(si) = self.subs.iter().position(|s| s.id == id) else {
            return;
        };
        let rate = f64::from(self.bitrate(usize::from(self.subs[si].encoding))) / 8.0
            * RESEND_SHARE;
        let burst = (rate * RESEND_BURST.as_secs_f64()).max(2.0 * MAX_DATAGRAM as f64);
        let sub = &mut self.subs[si];
        if sub.paused {
            return;
        }
        let dt = sub.refilled.map_or(0.0, |t| now.saturating_duration_since(t).as_secs_f64());
        sub.tokens = (sub.tokens + rate * dt).min(burst);
        sub.refilled = Some(now);
        let e = usize::from(sub.encoding);
        let Some(front) = self.history.front().map(|b| b.seq) else {
            return;
        };
        let (stream, generation) = (self.stream, self.generation);
        let mut sent = 0;
        for (seq, frag) in nack.iter() {
            let Some(b) = self.history.get_mut(seq.wrapping_sub(front) as usize) else {
                continue;
            };
            let block = &b.data[e];
            if block.is_empty() {
                continue;
            }
            let h = AudioHeader {
                stream,
                generation,
                drift: b.drift,
                seq: b.seq,
                discontinuity: b.discontinuity,
            };
            // the cheap checks first: a datagram is only built to be sent
            let count = wire::fragment_count(block.len());
            let frags = match frag {
                NACK_ALL => 0..count,
                f => usize::from(f)..(usize::from(f) + 1).min(count),
            };
            for fi in frags {
                if sent >= MAX_RESEND_PER_NACK {
                    return;
                }
                let done = b.resent.iter().find(|r| r.0 == id && usize::from(r.1) == fi);
                if done.is_some_and(|r| r.2 >= RESEND_RETRIES) {
                    continue;
                }
                let len = wire::fragment_len(block.len(), fi) as f64;
                if sub.tokens < len {
                    return;
                }
                sub.tokens -= len;
                sent += 1;
                let _ = wire::fragment_at(&h, block, fi, |d| out(id, Bytes::copy_from_slice(d)));
                match b.resent.iter_mut().find(|r| r.0 == id && usize::from(r.1) == fi) {
                    Some(r) => r.2 += 1,
                    None => b.resent.push((id, fi as u8, 1)),
                }
            }
        }
    }

    /// Test hook: jump the block counter (e.g. to just below `u32::MAX`).
    #[doc(hidden)]
    pub fn set_sequence_for_testing(&mut self, seq: u32) {
        self.seq = seq;
        self.history.clear();
    }
}

/// The datagram(s) of one block for subscribers of encoding `e` with redundancy `k`. A block
/// that needs fragments goes out without redundancy; otherwise up to `k` previous blocks ride
/// along, fewer if they would not fit.
fn datagrams(
    h: &AudioHeader,
    payload: &[u8],
    history: &VecDeque<Sent>,
    e: usize,
    k: usize,
) -> Vec<Bytes> {
    let mut v = Vec::new();
    if payload.is_empty() {
        return v;
    }
    if AUDIO_HEADER + payload.len() > MAX_DATAGRAM {
        let _ = wire::fragment(h, payload, |d| v.push(Bytes::copy_from_slice(d)));
        return v;
    }
    // the history is contiguous up to seq - 1 within this generation
    let mut n = history
        .iter()
        .rev()
        .take(k)
        .take_while(|b| !b.data[e].is_empty() && AUDIO_HEADER + b.data[e].len() < MAX_DATAGRAM)
        .count();
    let mut buf = Vec::with_capacity(MAX_DATAGRAM);
    loop {
        let red = history.range(history.len() - n..);
        let size: usize = red.clone().map(|b| 2 + b.data[e].len()).sum();
        if n == 0 || AUDIO_HEADER + 1 + size + payload.len() <= MAX_DATAGRAM {
            let red: Vec<&[u8]> = red.map(|b| &b.data[e][..]).collect();
            if wire::encode_audio(h, &red, payload, &mut buf).is_ok() {
                v.push(Bytes::from(buf));
            }
            return v;
        }
        n -= 1;
    }
}

// ---------------------------------------------------------------------------------------------
// Receiver

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RxOptions {
    /// Jitter buffer target, ms (0..=[`MAX_BUFFER_MS`]; at least one device callback is used).
    pub buffer_ms: f32,
    /// Resample by `own rate / sender rate` from both DLLs (else nominal rates only).
    pub dynamic_resampling: bool,
    pub dll_bandwidth: f64,
    /// NACKs per missing block.
    pub nack_retries: u8,
}

impl Default for RxOptions {
    fn default() -> Self {
        Self {
            buffer_ms: 20.0,
            dynamic_resampling: true,
            dll_bandwidth: f64::from(TIMEFILTER_BANDWIDTH),
            nack_retries: 3,
        }
    }
}

/// Receiver events, the counterparts of `aoo::sink::SinkEventKind` the jitter controller uses.
/// Counts are blocks since the previous event of the kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RxEvent {
    /// Playback started (`true`) or ran dry (`false`).
    StateChanged { playing: bool },
    /// Blocks played as concealment (lost, unrepaired, undecodable) or dropped on overflow.
    BlocksLost { count: u32 },
    /// Blocks that arrived after their playout time.
    BlocksLate { count: u32 },
    /// NACKed blocks that arrived.
    BlocksResent { count: u32 },
    /// Blocks that arrived out of order (not resends).
    BlocksReordered { count: u32 },
    /// Missing blocks filled from a later datagram's redundancy.
    BlocksRecovered { count: u32 },
    /// Blocks skipped by a sequence jump.
    BlocksGap { count: u32 },
}

/// Synchronised playout target of an [`RxStream`] ([`RxStream::set_playout`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Playout {
    pub generation: u8,
    /// Block `seq` of `generation` is due at the audio half's output at `due` (local clock);
    /// the others at the nominal block rate from there.
    pub seq: u32,
    pub due: Instant,
    /// Audio the buffer must hold beyond its jitter target to meet it (the playout delay).
    pub span: Duration,
}

#[derive(Debug, Default)]
struct Shared {
    playing: AtomicBool,
    underrun: AtomicBool,
    /// f64 bits: sender block rate (stream rate corrected by its drift).
    sender_rate: AtomicU64,
    dynamic: AtomicBool,
    /// Playout sync: the sender rate scaled by `1 + steer_ppm * 1e-6`.
    steer_ppm: AtomicI32,
    /// Playout sync is on: the audio half takes the callback's resampling ratio before it fills
    /// the resampler. (Filling with the previous ratio and then taking a smaller one, as when
    /// sync's slowing down ends, leaves it a frame short with the ring full: an underrun that
    /// keeps the stream a callback later. Unsynced playout keeps that order as it always had,
    /// with the same rare underrun when the sender's rate estimate rises.)
    ratio_first: AtomicBool,
}

/// Audio-thread state of one format/buffer configuration.
struct Slot {
    samples: Consumer<f32>,
    nsamples: usize,
    nch: usize,
    resampler: Resampler,
    scratch: Vec<f32>,
    playing: bool,
    /// Frames taken from the ring so far.
    read: u64,
    /// f64 bits, published every callback: the ring frame at the output at `epoch`
    /// (extrapolated at the stream rate `rate`).
    pos: Arc<AtomicU64>,
    epoch: Instant,
    rate: f64,
}

impl Slot {
    fn process(
        &mut self,
        out: &mut [&mut [f32]],
        n: usize,
        from: f64,
        to: f64,
        shared: &Shared,
        now: Instant,
    ) -> bool {
        let want = n * self.nch;
        let ratio_first = shared.ratio_first.load(Ordering::Relaxed);
        if ratio_first {
            self.resampler.update(from, to);
        }
        while want > self.resampler.read_available()
            && self.resampler.write_available() >= self.nsamples
        {
            let Ok(chunk) = self.samples.read_chunk(self.nsamples) else {
                break;
            };
            let (a, b) = chunk.as_slices();
            self.resampler.write(a);
            self.resampler.write(b);
            chunk.commit_all();
            self.read += (self.nsamples / self.nch) as u64;
        }
        if !ratio_first {
            self.resampler.update(from, to);
        }
        // the next frame out is the oldest one the resampler holds, now; when it cannot play
        // this callback, at the next one
        let ratio = if from == to { 1.0 } else { to / from };
        let held = self.resampler.read_available() as f64 / (self.nch as f64 * ratio);
        let mut t = secs(now, self.epoch);
        if self.resampler.read_available() < want {
            t += n as f64 / to;
        }
        let pos = self.read as f64 - held - t * self.rate;
        self.pos.store(pos.to_bits(), Ordering::Release);
        if self.resampler.read_available() < want {
            if self.playing {
                self.playing = false;
                shared.playing.store(false, Ordering::Release);
                shared.underrun.store(true, Ordering::Release);
            }
            return false;
        }
        let buf = &mut self.scratch[..want];
        self.resampler.read(buf);
        for (c, o) in out.iter_mut().enumerate().take(self.nch) {
            for (j, o) in o[..n].iter_mut().enumerate() {
                *o = buf[j * self.nch + c];
            }
        }
        if !self.playing {
            self.playing = true;
            shared.playing.store(true, Ordering::Release);
        }
        true
    }
}

/// Audio-thread half of an [`RxStream`].
pub struct RxAudio {
    slot: Option<Box<Slot>>,
    installs: Consumer<Box<Slot>>,
    garbage: Producer<Box<Slot>>,
    shared: Arc<Shared>,
    device: Device,
    bandwidth: f64,
    t0: Option<Instant>,
    dll: Dll,
}

impl RxAudio {
    /// Fill `out` (one slice per channel, at most the device block size, all the same length)
    /// with the stream; silence when nothing plays. `now` is the callback time. Returns whether
    /// audio was played. Real-time safe.
    pub fn process(&mut self, out: &mut [&mut [f32]], now: Instant) -> bool {
        while !self.garbage.is_full() {
            let Ok(slot) = self.installs.pop() else { break };
            if let Some(old) = self.slot.replace(slot) {
                let _ = self.garbage.push(old);
            }
        }
        for o in out.iter_mut() {
            o.fill(0.0);
        }
        let sr = f64::from(self.device.sample_rate);
        match self.t0 {
            None => {
                self.t0 = Some(now);
                self.dll
                    .setup(sr, self.device.block_size as i32, self.bandwidth, 0.0);
            }
            Some(t0) => self
                .dll
                .update(now.saturating_duration_since(t0).as_secs_f64()),
        }
        let dynamic = self.shared.dynamic.load(Ordering::Relaxed);
        let own = if !dynamic || (self.dll.samplerate() - sr).abs() > 0.1 * sr {
            sr
        } else {
            self.dll.samplerate()
        };
        let n = out
            .first()
            .map_or(0, |o| o.len())
            .min(self.device.block_size);
        let steer = f64::from(self.shared.steer_ppm.load(Ordering::Relaxed)) * 1e-6;
        let from = f64::from_bits(self.shared.sender_rate.load(Ordering::Relaxed)) * (1.0 + steer);
        match &mut self.slot {
            Some(slot) if n > 0 => slot.process(out, n, from, own, &self.shared, now),
            _ => false,
        }
    }
}

#[derive(Debug, Default)]
struct Counters {
    lost: u32,
    late: u32,
    resent: u32,
    reordered: u32,
    recovered: u32,
    gap: u32,
}

fn sat(n: u64) -> u32 {
    n.min(u64::from(u32::MAX)) as u32
}

/// One queued block: `have` = data present; NACK bookkeeping otherwise.
#[derive(Debug, Default)]
struct Entry {
    have: bool,
    data: Vec<u8>,
    /// When the NACK scan first saw this block missing.
    seen: Option<Instant>,
    tries: u8,
    last_nack: Option<Instant>,
}

/// Reorder queue: `queue[i]` is block `next + i`.
struct Jb {
    queue: VecDeque<Entry>,
    next: Option<u64>,
    highest: Option<u64>,
    highest_at: Instant,
    /// Bit `i`: block `next - 1 - i` was played from data.
    played: u64,
    window: usize,
    max_bytes: usize,
    /// Redundancy of the newest datagram.
    red: u64,
}

impl Jb {
    fn insert(&mut self, seq: u64, data: &[u8], primary: bool, c: &mut Counters) {
        if data.is_empty() || data.len() > self.max_bytes {
            return;
        }
        let next = *self.next.get_or_insert(seq);
        if seq < next {
            let back = next - seq;
            if primary && back <= 64 && self.played & (1 << (back - 1)) == 0 {
                self.played |= 1 << (back - 1);
                c.late += 1;
            }
            return;
        }
        let i = (seq - next) as usize;
        if i >= self.window {
            return;
        }
        while self.queue.len() <= i {
            self.queue.push_back(Entry::default());
        }
        let e = &mut self.queue[i];
        if e.have {
            return;
        }
        e.have = true;
        e.data.clear();
        e.data.extend_from_slice(data);
        if !primary {
            c.recovered += 1;
        } else if e.tries > 0 {
            c.resent += 1;
        } else if self.highest.is_some_and(|h| seq < h) {
            c.reordered += 1;
        }
    }
}

/// Network-side state of one format generation.
struct Active {
    generation: u8,
    format: StreamFormat,
    nch: usize,
    dec: Dec,
    reasm: Reassembler,
    jb: Jb,
    ring: Producer<f32>,
    nsamples: usize,
    block_frames: usize,
    /// Blocks the ring holds right after (re)buffering.
    prefill: usize,
    /// Fill (frames) at which a hole is concealed.
    margin: usize,
    /// Push silence up to `prefill` before the next block (start, underrun, flush).
    rebuffer: bool,
    scratch: Vec<f32>,
    zeros: Vec<f32>,
    /// Arrival time of the first datagram (jitter time base).
    t_base: Option<Instant>,
    last_transit: Option<f64>,
    /// RFC 3550 interarrival jitter, s.
    jitter: f64,
    /// Frames pushed into the ring so far (blocks and silence): where the next block goes.
    pushed: u64,
    /// The audio half's output position ([`Slot::pos`]).
    pos: Arc<AtomicU64>,
    epoch: Instant,
    /// Playout sync: smoothed error (frames, positive = early) and when it was updated.
    sync_err: Option<(f64, Instant)>,
    /// Playout sync: the latest error (frames) while the stream plays against a target and
    /// blocks arrive; else `None`.
    reported: Option<f64>,
    sync_hold: Option<Instant>,
    /// Playout sync: the target moved; start the smoothed error afresh.
    retarget: bool,
    /// Playout sync's view of the jitter buffer since it last (re)started: per second `s` of
    /// `epoch` time, in slot `s % LOWS`, `(s, the least headroom an arriving block had, sync
    /// went late)` (headroom: frames that play before it, ring fill plus the blocks queued
    /// ahead of it). Sync's own corrections shift them, so they stay comparable with what
    /// arrives now.
    lows: [(u64, f64, bool); LOWS],
    /// The resampling offset in effect since `steer_at` (ppm), to shift `lows` by.
    steer_ppm: f64,
    steer_at: Option<Instant>,
}

impl Active {
    fn fill_frames(&self) -> usize {
        (self.ring.buffer().capacity() - self.ring.slots()) / self.nch
    }

    fn fill_secs(&self) -> f64 {
        self.fill_frames() as f64 / f64::from(self.format.sample_rate)
    }

    /// Decode (or conceal) the front block into the ring. `false` if the ring is full.
    fn play_front(&mut self, c: &mut Counters) -> bool {
        if self.ring.slots() < self.nsamples {
            return false;
        }
        let Some(next) = self.jb.next else {
            return false;
        };
        let front = self.jb.queue.pop_front().unwrap_or_default();
        let after = self
            .jb
            .queue
            .front()
            .filter(|e| e.have)
            .map(|e| &e.data[..]);
        let data = front.have.then_some(&front.data[..]);
        let ok = self.dec.block(data, after, self.nch, &mut self.scratch);
        if !ok {
            c.lost += 1;
        }
        let _ = self.ring.push_entire_slice(&self.scratch);
        self.pushed += self.block_frames as u64;
        self.jb.next = Some(next + 1);
        self.jb.played = self.jb.played << 1 | u64::from(ok);
        true
    }

    /// Up to `frames` of silence into the ring (as much as fits). Returns the frames pushed.
    fn push_zeros(&mut self, frames: usize) -> usize {
        let mut left = frames.saturating_mul(self.nch).min(self.ring.slots());
        let mut done = 0;
        while left > 0 {
            let k = left.min(self.zeros.len());
            let _ = self.ring.push_entire_slice(&self.zeros[..k]);
            self.pushed += (k / self.nch) as u64;
            done += k / self.nch;
            left -= k;
        }
        done
    }

    /// Playout sync error in frames (positive: the output is early), once the audio half has
    /// reported its position.
    fn sync_error(&self, p: &Playout) -> Option<f64> {
        if p.generation != self.generation {
            return None;
        }
        let next = self.jb.next?;
        let a = f64::from_bits(self.pos.load(Ordering::Acquire));
        if !a.is_finite() {
            return None;
        }
        // the frame at the output when the next block is due, against where that block goes
        let sr = f64::from(self.format.sample_rate);
        let due = secs(p.due, self.epoch)
            + (next as f64 - f64::from(p.seq)) * self.format.block_secs();
        let e = a + due * sr - self.pushed as f64;
        // an honest target is due at most `span` after now: anything later is no target
        let span = p.span.as_secs_f64().min(f64::from(MAX_BUFFER_MS) * 1e-3);
        (e.is_finite() && e <= (span + SYNC_HARD) * sr).then_some(e)
    }

    /// The headroom block `seq` would arrive with now (frames that play before it: ring fill
    /// and the blocks queued ahead), if its data is not here yet (the original, or a resend of
    /// a NACKed one: whichever comes first; not a duplicate) and we are not (re)buffering. One
    /// that missed its turn (concealed) by at most the jitter target has it negative: what it
    /// lacked; a straggler later than that sets no requirement.
    fn arrival_headroom(&self, seq: u64) -> Option<f64> {
        let next = self.jb.next.filter(|_| !self.rebuffer)?;
        let fill = self.fill_frames() as f64;
        if seq < next {
            let behind = seq.abs_diff(next) as usize;
            let new = self.jb.played & (1 << (behind - 1).min(63)) == 0;
            return (new && behind <= self.prefill).then(|| fill - (behind * self.block_frames) as f64);
        }
        let i = (seq - next) as usize;
        let first = self.jb.queue.get(i).is_none_or(|e| !e.have);
        first.then(|| fill + (i * self.block_frames) as f64)
    }

    /// The second of `epoch` time that `now` falls in.
    fn second(&self, now: Instant) -> u64 {
        now.saturating_duration_since(self.epoch).as_secs()
    }

    /// A block arrived at `now` with headroom `h` (frames).
    fn note_headroom(&mut self, now: Instant, h: f64) {
        let s = self.second(now);
        let slot = &mut self.lows[s as usize % LOWS];
        *slot = if slot.0 == s {
            (s, slot.1.min(h), slot.2)
        } else {
            (s, h, false)
        };
    }

    /// The least headroom of this second and the one before (`None` without arrivals).
    fn recent_headroom(&self, now: Instant) -> Option<f64> {
        let s = self.second(now);
        (s.saturating_sub(1)..=s)
            .filter_map(|t| Some(self.lows[t as usize % LOWS]).filter(|x| x.0 == t))
            .map(|x| x.1)
            .reduce(f64::min)
    }

    /// Sync went late in the last [`LOWS`] seconds, before the last two (which one event, a
    /// latency step, spans).
    fn went_late(&self, now: Instant) -> bool {
        let s = self.second(now);
        let ago = |at: u64| s.saturating_sub(at);
        self.lows.iter().any(|&(at, _, late)| late && (2..LOWS as u64).contains(&ago(at)))
    }

    /// Our own correction moved what plays before any later block by `d` frames.
    fn shift_lows(&mut self, d: f64) {
        for (_, h, _) in &mut self.lows {
            *h += d;
        }
    }

    fn clear_lows(&mut self) {
        self.lows = [(u64::MAX, f64::INFINITY, false); LOWS];
    }

    /// Playout sync, on the timer. It only ever adds latency:
    /// - going late: when a block arrived in the last one to two seconds with less headroom
    ///   than unsynced playout restarts with (the jitter target less the block it restarts on;
    ///   at least a block over the margin, measured) by more than a block or device callback (the
    ///   measuring granularity), silence brings it up to that, as unsynced playout would
    ///   restart after running dry; if it went late already in the last [`LOWS`] seconds (not
    ///   just now), by up to [`SYNC_LATE_STEP`] more;
    /// - early by more than [`SYNC_HARD`]: silence;
    /// - early by less: the resampling slows down, proportionally;
    /// - late: nothing. The stream stays late (and says so) until its buffering restarts: after
    ///   an underrun, or when the target moves earlier ([`RxStream::set_playout`]).
    fn steer(&mut self, p: Option<&Playout>, now: Instant, shared: &Shared, playing: bool) {
        let sr = f64::from(self.format.sample_rate);
        // the slower resampling since the last call moved what plays before the next block
        if let Some(t) = self.steer_at {
            self.shift_lows(-self.steer_ppm * 1e-6 * sr * secs(now, t).max(0.0));
        }
        self.steer_at = Some(now);
        // a reading is current while the stream plays and blocks arrive
        let live = playing
            && !self.rebuffer
            && now.saturating_duration_since(self.jb.highest_at) < SYNC_STALE;
        if !live {
            self.reported = None;
        }
        let e = p.filter(|_| !self.rebuffer).and_then(|p| self.sync_error(p));
        let Some(e) = e else {
            self.reported = None;
            self.sync_err = None;
            self.set_steer(0.0, shared);
            return;
        };
        if std::mem::take(&mut self.retarget) {
            (self.sync_err, self.sync_hold) = (None, None);
        }
        if self.sync_hold.is_some_and(|t| now < t) {
            return;
        }
        let mut err = match self.sync_err {
            Some((x, t)) => x + (e - x) * (secs(now, t) / SYNC_SMOOTH).clamp(0.0, 1.0),
            None => e,
        };
        // what unsynced playout restarts with, and at least a block over the margin: below it
        // `drain` conceals every block that comes after its successor (unsynced playout gets
        // there with a device callback large against the jitter target too; sync does not
        // follow it there)
        let callback = (self.margin - self.block_frames) / 2;
        let granularity = self.block_frames.max(callback);
        let restart = self.prefill.saturating_sub(1) * self.block_frames;
        let need = restart.max(self.margin + self.block_frames + granularity) as f64;
        let low = need - granularity as f64;
        let mut added = 0.0;
        if let Some(h) = self.recent_headroom(now).filter(|&h| h < low) {
            // still getting later (a ramp, a heavier tail): more, rather than many small cuts
            let more = if self.went_late(now) {
                let step = (SYNC_LATE_STEP * sr) as usize;
                (self.prefill * self.block_frames).min(step)
            } else {
                0
            };
            added = self.push_zeros((need - h).ceil() as usize + more) as f64;
            let s = self.second(now);
            if let Some(slot) = self.lows.iter_mut().find(|x| x.0 <= s && s - x.0 <= 1) {
                slot.2 = true;
            }
        } else if err > SYNC_HARD * sr {
            // all of it at once: the smoothed error crosses the threshold before it settles
            added = self.push_zeros(err.max(e) as usize) as f64;
        }
        if added > 0.0 {
            self.shift_lows(added);
            // measure afresh once the audio half plays it
            self.sync_err = None;
            self.sync_hold = Some(now + SYNC_HOLD);
            err -= added;
        } else {
            self.sync_err = Some((err, now));
        }
        if live {
            self.reported = Some(err);
        }
        // early: slower (never faster for sync)
        let ppm = (-err / sr * SYNC_GAIN).clamp(-SYNC_MAX_STEER, 0.0) * 1e6;
        self.set_steer(ppm.round(), shared);
    }

    fn set_steer(&mut self, ppm: f64, shared: &Shared) {
        self.steer_ppm = ppm;
        shared.steer_ppm.store(ppm as i32, Ordering::Relaxed);
    }

    /// Move playable blocks into the ring; conceal a hole only when a later block is here and
    /// the ring is about to run dry. With playout sync, (re)buffering starts the next block
    /// when it is due (if the jitter target allows).
    fn drain(&mut self, c: &mut Counters, p: Option<&Playout>) {
        if self.rebuffer && self.jb.queue.front().is_some() {
            let sync = p.and_then(|p| self.sync_error(p));
            if sync.is_none()
                && p.is_some_and(|p| p.generation == self.generation)
                && !f64::from_bits(self.pos.load(Ordering::Acquire)).is_finite()
            {
                return; // the audio half has not reported its position yet
            }
            self.rebuffer = false;
            let have = self.fill_frames() / self.block_frames;
            let normal = self.prefill.saturating_sub(have + 1) * self.block_frames;
            let early = sync.map_or(0.0, |e| e.max(0.0));
            self.push_zeros(normal.max(early as usize));
            // a new jitter buffer: watch it afresh
            self.clear_lows();
            self.sync_err = None;
        }
        while let Some(front) = self.jb.queue.front() {
            let go = front.have
                || (self.fill_frames() <= self.margin
                    && self.jb.queue.iter().skip(1).any(|e| e.have));
            if !go || !self.play_front(c) {
                break;
            }
        }
    }

    /// Restart the queue at `seq`: everything queued is lost, the skipped range is a gap.
    fn resync(&mut self, seq: u64, c: &mut Counters) {
        if let Some(next) = self.jb.next {
            c.lost = c.lost.saturating_add(sat(self.jb.queue.len() as u64));
            let seen = self.jb.highest.unwrap_or(next).max(next);
            c.gap = c.gap.saturating_add(sat(seq.saturating_sub(seen + 1)));
        }
        self.jb.queue.clear();
        self.jb.next = Some(seq);
        self.jb.played = 0;
    }

    /// Make block `seq` fit the queue window: play/conceal (or drop, when the ring is full) the
    /// oldest blocks, or resync on a far jump.
    fn make_room(&mut self, seq: u64, c: &mut Counters) {
        let Some(next) = self.jb.next else { return };
        let w = self.jb.window as u64;
        if seq < next + w {
            return;
        }
        if seq - next >= 2 * w {
            self.resync(seq, c);
            return;
        }
        while let Some(next) = self.jb.next.filter(|&n| seq >= n + w) {
            if !self.play_front(c) {
                self.jb.queue.pop_front();
                self.jb.next = Some(next + 1);
                self.jb.played <<= 1;
                c.lost += 1;
            }
        }
    }

    /// Placeholders for blocks that are overdue after the newest one (tail loss), so the
    /// timer-driven NACK can ask for them.
    fn expect_tail(&mut self, now: Instant) {
        let (Some(next), Some(high)) = (self.jb.next, self.jb.highest) else {
            return;
        };
        let block = self.format.block_secs();
        let late = now
            .saturating_duration_since(self.jb.highest_at)
            .as_secs_f64()
            - block
            - 2.0 * self.jitter;
        if late < block {
            return;
        }
        let upto = (high + (late / block) as u64)
            .min(high + self.prefill as u64)
            .min(next + self.jb.window as u64 - 1)
            .min(u64::from(u32::MAX));
        if upto >= next {
            while self.jb.queue.len() <= (upto - next) as usize {
                self.jb.queue.push_back(Entry::default());
            }
        }
    }

    fn scan_nacks(
        &mut self,
        stream: u8,
        now: Instant,
        rtt: Duration,
        retries: u8,
        out: &mut Vec<(u32, u8)>,
    ) {
        let Some(next) = self.jb.next else { return };
        let high = self.jb.highest.unwrap_or(next);
        let block = self.format.block_secs();
        // time left before the front block is concealed
        let slack = self.fill_secs() - self.margin as f64 / f64::from(self.format.sample_rate);
        let rtt_s = rtt.as_secs_f64();
        let retry = rtt.saturating_add(Duration::from_secs_f64(block));
        // reordering tolerance before the first NACK
        let hold = Duration::from_secs_f64(2.0 * self.jitter);
        for (i, e) in self.jb.queue.iter_mut().enumerate() {
            if out.len() >= MAX_NACK {
                return;
            }
            let s = next + i as u64;
            let seen = *e.seen.get_or_insert(now);
            let eligible = (s > high || high >= s + 1 + self.jb.red)
                && (e.tries > 0 || now.saturating_duration_since(seen) >= hold);
            if e.have
                || !eligible
                || slack + i as f64 * block <= rtt_s
                || e.tries >= retries
                || e.last_nack
                    .is_some_and(|t| now.saturating_duration_since(t) < retry)
            {
                continue;
            }
            e.tries += 1;
            e.last_nack = Some(now);
            // A partly received block: only its missing fragments.
            let seq = s as u32;
            match self.reasm.missing(now, stream, self.generation, seq) {
                Some(frags) => out.extend(frags.map(|f| (seq, f))),
                None => out.push((seq, NACK_ALL)),
            }
        }
    }
}

/// Network half of one incoming stream.
pub struct RxStream {
    stream: u8,
    device: Device,
    opts: RxOptions,
    shared: Arc<Shared>,
    installs: Producer<Box<Slot>>,
    garbage: Consumer<Box<Slot>>,
    pending: Option<Box<Slot>>,
    st: Option<Active>,
    playing: bool,
    rtt: Duration,
    c: Counters,
    events: VecDeque<RxEvent>,
    nacks: Vec<(u32, u8)>,
    report_lost: u32,
    report_late: u32,
    report_at: Option<Instant>,
    playout: Option<Playout>,
    /// Ring room for playout sync beyond the jitter target, ms.
    span_ms: f32,
}

fn rx_audio(
    device: Device,
    opts: &RxOptions,
    shared: Arc<Shared>,
) -> (RxAudio, Producer<Box<Slot>>, Consumer<Box<Slot>>) {
    let (ip, ic) = RingBuffer::new(4);
    let (gp, gc) = RingBuffer::new(8);
    let audio = RxAudio {
        slot: None,
        installs: ic,
        garbage: gp,
        shared,
        device,
        bandwidth: opts.dll_bandwidth,
        t0: None,
        dll: Dll::default(),
    };
    (audio, ip, gc)
}

impl RxStream {
    /// `stream` < 8 is the id this stream's datagrams carry. Nothing plays until
    /// [`Self::set_format`].
    pub fn new(stream: u8, device: Device, opts: RxOptions) -> (Self, RxAudio) {
        let shared = Arc::new(Shared::default());
        let opts = RxOptions {
            buffer_ms: opts.buffer_ms.clamp(0.0, MAX_BUFFER_MS),
            ..opts
        };
        shared
            .dynamic
            .store(opts.dynamic_resampling, Ordering::Relaxed);
        let (audio, installs, garbage) = rx_audio(device, &opts, shared.clone());
        let s = Self {
            stream,
            device,
            opts,
            shared,
            installs,
            garbage,
            pending: None,
            st: None,
            playing: false,
            rtt: Duration::MAX,
            c: Counters::default(),
            events: VecDeque::new(),
            nacks: Vec::new(),
            report_lost: 0,
            report_late: 0,
            report_at: None,
            playout: None,
            span_ms: 0.0,
        };
        (s, audio)
    }

    /// New audio device: returns the audio half to install. Buffering restarts.
    pub fn setup(&mut self, device: Device) -> RxAudio {
        self.device = device;
        let (audio, installs, garbage) = rx_audio(device, &self.opts, self.shared.clone());
        self.installs = installs;
        self.garbage = garbage;
        self.pending = None;
        self.rebuild();
        audio
    }

    /// Apply a FORMAT frame for this stream. A new generation (or content) flushes the stream
    /// and re-creates the decoder; datagrams of any other generation are ignored from now on.
    pub fn set_format(&mut self, f: &Format) -> Result<(), Error> {
        if f.stream != self.stream || f.generation >= 32 {
            return Err(Error::Invalid);
        }
        let sf = StreamFormat::from_wire(f)?;
        if self
            .st
            .as_ref()
            .is_some_and(|st| st.generation == f.generation && st.format == sf)
        {
            return Ok(());
        }
        let st = self.activate(f.generation, sf)?;
        self.st = Some(st);
        self.set_ratio_first();
        Ok(())
    }

    /// The audio half orders the resampler for sync only while a target applies to the
    /// current generation (else exactly as without sync).
    fn set_ratio_first(&self) {
        let on = self
            .playout
            .zip(self.st.as_ref())
            .is_some_and(|(p, st)| p.generation == st.generation);
        self.shared.ratio_first.store(on, Ordering::Relaxed);
    }

    pub fn format(&self) -> Option<&StreamFormat> {
        self.st.as_ref().map(|s| &s.format)
    }

    pub fn buffer_ms(&self) -> f32 {
        self.opts.buffer_ms
    }

    /// New jitter buffer target. As the legacy sink, this restarts buffering (queued audio is
    /// dropped and the next block plays after the new target).
    pub fn set_buffer_ms(&mut self, ms: f32) {
        let ms = ms.clamp(0.0, MAX_BUFFER_MS);
        if ms != self.opts.buffer_ms {
            self.opts.buffer_ms = ms;
            self.rebuild();
        }
    }

    pub fn set_dynamic_resampling(&mut self, on: bool) {
        self.opts.dynamic_resampling = on;
        self.shared.dynamic.store(on, Ordering::Relaxed);
    }

    /// Drop everything queued and buffered; playback restarts with the next block.
    pub fn flush(&mut self) {
        self.rebuild();
    }

    /// Synchronised playout: play the blocks of `p.generation` when they are due instead of as
    /// soon as the jitter target is buffered. (Re)buffering waits for the due time; sync only
    /// ever adds latency: early by more than [`SYNC_HARD`] it inserts silence, by less it
    /// slows the resampling; it goes late when the jitter buffer is not met; late, it does
    /// nothing until buffering restarts (after an underrun). The host lowering the delay
    /// (`span`) restarts buffering (one gap); raising it is early (silence). A due time that
    /// moves for any other reason (the anchor's jitter, a new clock offset) is only early or
    /// late. A `span` larger than the ring has room for (half as much again as the largest
    /// before), or turning sync off, restarts buffering (off: with the ring of a stream that
    /// never synced). Without a target (and for other generations) nothing changes.
    pub fn set_playout(&mut self, p: Option<Playout>) {
        let span = p.map_or(0.0, |p| (p.span.as_secs_f32() * 1e3).min(MAX_BUFFER_MS));
        let off = p.is_none() && self.playout.is_some();
        let mut lowered = false;
        if let (Some(st), Some(new), Some(old)) = (&mut self.st, p, self.playout) {
            if new.generation != old.generation || new.span != old.span {
                st.retarget = true;
            }
            lowered = new.generation == old.generation && new.span < old.span;
        }
        self.playout = p;
        self.set_ratio_first();
        if span > self.span_ms || off || lowered {
            // off: the ring as if sync had never been on; else room for half as much again, so
            // that a target moving later does not drop the buffered audio
            if off || span > self.span_ms {
                self.span_ms = if off { 0.0 } else { (span * 1.5).min(MAX_BUFFER_MS) };
            }
            self.shared.steer_ppm.store(0, Ordering::Relaxed);
            self.rebuild();
        }
    }

    /// Playout sync: how early (positive) or late the output is against the target, ms; `None`
    /// unless the stream plays against a target (sync off, another generation, an absurd
    /// target, (re)buffering) with blocks arriving (none for [`SYNC_STALE`]: a paused sender,
    /// an outage).
    pub fn playout_error_ms(&self) -> Option<f32> {
        let st = self.st.as_ref()?;
        let e = st.reported.filter(|_| self.playout.is_some())?;
        Some((e * 1e3 / f64::from(st.format.sample_rate)) as f32)
    }

    /// Playout sync's resampling offset now, ppm (never positive: sync only slows down).
    #[doc(hidden)]
    pub fn playout_steer_ppm(&self) -> i32 {
        self.shared.steer_ppm.load(Ordering::Relaxed)
    }

    /// Ring fill relative to the target, 0..=1 (`aoo_opt_buffer_fill_ratio`).
    pub fn buffer_fill_ratio(&self) -> Option<f32> {
        let st = self.st.as_ref()?;
        Some((st.fill_frames() as f32 / (st.prefill * st.block_frames) as f32).min(1.0))
    }

    /// Decoded audio waiting for the audio thread, ms.
    pub fn fill_ms(&self) -> f32 {
        self.st
            .as_ref()
            .map_or(0.0, |s| (s.fill_secs() * 1e3) as f32)
    }

    pub fn poll_event(&mut self) -> Option<RxEvent> {
        self.events.pop_front()
    }

    fn rebuild(&mut self) {
        if let Some(st) = self.st.take() {
            self.st = self.activate(st.generation, st.format).ok();
        }
    }

    fn activate(&mut self, generation: u8, format: StreamFormat) -> Result<Active, Error> {
        let dec = Dec::new(&format)?;
        let nch = usize::from(format.channels);
        let block = usize::from(format.block_size);
        let nsamples = nch * block;
        let sr = f64::from(format.sample_rate);
        let dev_block =
            (self.device.block_size as f64 * sr / f64::from(self.device.sample_rate.max(1))).ceil();
        let target = (f64::from(self.opts.buffer_ms) * 1e-3 * sr).max(dev_block);
        let prefill = ((target / block as f64).ceil() as usize).max(1);
        let span = (f64::from(self.span_ms) * 1e-3 * sr / block as f64).ceil() as usize;
        let cap = prefill + prefill.max(8) + span;
        let (ring, samples) = RingBuffer::new(cap * nsamples);
        let max_bytes = format.max_block_bytes();
        let mut resampler = Resampler::default();
        resampler.setup(block, self.device.block_size.max(1), nch);
        self.shared
            .sender_rate
            .store(sr.to_bits(), Ordering::Relaxed);
        let pos = Arc::new(AtomicU64::new(f64::NAN.to_bits()));
        let epoch = Instant::now();
        self.pending = Some(Box::new(Slot {
            samples,
            nsamples,
            nch,
            resampler,
            scratch: vec![0.0; self.device.block_size.max(1) * nch],
            playing: false,
            read: 0,
            pos: pos.clone(),
            epoch,
            rate: sr,
        }));
        self.flush_pending();
        let timeout = Duration::from_secs_f64((2e-3 * f64::from(self.opts.buffer_ms)).max(0.2));
        // placeholder until the first datagram (only read once `highest` is set)
        let now = Instant::now();
        Ok(Active {
            generation,
            nch,
            dec,
            reasm: Reassembler::new(4, max_bytes, timeout),
            jb: Jb {
                queue: VecDeque::new(),
                next: None,
                highest: None,
                highest_at: now,
                played: 0,
                window: (cap + 8).min(QUEUE_BUDGET / max_bytes.max(1)).max(4),
                max_bytes,
                red: 0,
            },
            ring,
            nsamples,
            block_frames: block,
            prefill,
            margin: block + 2 * dev_block as usize,
            rebuffer: true,
            scratch: vec![0.0; nsamples],
            zeros: vec![0.0; nsamples],
            t_base: None,
            last_transit: None,
            jitter: 0.0,
            pushed: 0,
            pos,
            epoch,
            sync_err: None,
            reported: None,
            sync_hold: None,
            retarget: false,
            lows: [(u64::MAX, f64::INFINITY, false); LOWS],
            steer_ppm: 0.0,
            steer_at: None,
            format,
        })
    }

    fn flush_pending(&mut self) {
        if let Some(slot) = self.pending.take()
            && let Err(rtrb::PushError::Full(slot)) = self.installs.push(slot)
        {
            self.pending = Some(slot);
        }
    }

    /// Reclaim old audio state and pick up what the audio half reported.
    fn service(&mut self) {
        while self.garbage.pop().is_ok() {}
        self.flush_pending();
        if self.shared.underrun.swap(false, Ordering::AcqRel)
            && let Some(st) = &mut self.st
        {
            st.rebuffer = true;
        }
        let playing = self.shared.playing.load(Ordering::Acquire);
        if playing != self.playing {
            self.playing = playing;
            self.event(RxEvent::StateChanged { playing });
        }
    }

    fn event(&mut self, e: RxEvent) {
        if self.events.len() < MAX_EVENTS {
            self.events.push_back(e);
        }
    }

    fn flush_counters(&mut self) {
        let c = std::mem::take(&mut self.c);
        self.report_lost = self.report_lost.saturating_add(c.lost);
        self.report_late = self.report_late.saturating_add(c.late);
        let all = [
            (c.lost, RxEvent::BlocksLost { count: c.lost }),
            (c.late, RxEvent::BlocksLate { count: c.late }),
            (c.resent, RxEvent::BlocksResent { count: c.resent }),
            (c.reordered, RxEvent::BlocksReordered { count: c.reordered }),
            (c.recovered, RxEvent::BlocksRecovered { count: c.recovered }),
            (c.gap, RxEvent::BlocksGap { count: c.gap }),
        ];
        for (n, e) in all {
            if n > 0 {
                self.event(e);
            }
        }
    }

    fn nack_enabled(&self) -> bool {
        f64::from(self.opts.buffer_ms) * 1e-3 > self.rtt.as_secs_f64()
    }

    /// One AUDIO datagram of this stream (any order, duplicates and garbage tolerated).
    pub fn handle_audio(&mut self, now: Instant, a: &Audio) {
        self.service();
        let nack = self.nack_enabled();
        let Some(st) = &mut self.st else { return };
        let h = a.header;
        if h.stream != self.stream || h.generation != st.generation {
            return;
        }
        let c = &mut self.c;
        let seq = u64::from(h.seq);
        let newest = st.jb.highest.is_none_or(|x| seq > x);
        if newest {
            if h.discontinuity {
                if st.jb.next.is_some_and(|n| n != seq) {
                    st.resync(seq, c);
                }
                st.dec.reset();
            }
            let sr = f64::from(st.format.sample_rate);
            let rate = if self.opts.dynamic_resampling {
                sr * (1.0 + f64::from(h.drift) / 8e6)
            } else {
                sr
            };
            self.shared
                .sender_rate
                .store(rate.to_bits(), Ordering::Relaxed);
        }
        st.make_room(seq, c);
        let headroom = self.playout.and_then(|_| st.arrival_headroom(seq));
        let data = match a.fragment {
            None => Some(a.payload),
            Some((i, n)) => st.reasm.push(now, &h, i, n, a.payload),
        };
        let complete = data.is_some();
        if let Some(d) = data {
            st.jb.insert(seq, d, true, c);
        }
        if let Some(h) = headroom.filter(|_| complete) {
            st.note_headroom(now, h);
        }
        let k = a.redundant.len() as u64;
        for (j, b) in a.redundant.iter().enumerate() {
            if let Some(s) = (seq + j as u64).checked_sub(k) {
                st.jb.insert(s, b, false, c);
            }
        }
        if newest {
            st.jb.highest = Some(seq);
            st.jb.highest_at = now;
            st.jb.red = k;
            let base = *st.t_base.get_or_insert(now);
            let transit = now.saturating_duration_since(base).as_secs_f64()
                - seq as f64 * st.format.block_secs();
            if let Some(last) = st.last_transit {
                st.jitter += ((transit - last).abs() - st.jitter) / 16.0;
            }
            st.last_transit = Some(transit);
        }
        st.drain(c, self.playout.as_ref());
        if nack {
            st.scan_nacks(
                self.stream,
                now,
                self.rtt,
                self.opts.nack_retries,
                &mut self.nacks,
            );
        }
        self.flush_counters();
    }

    /// Timer work, at least once per device callback period: conceal holes whose playout is
    /// due, NACK missing and overdue blocks (only while the buffer target exceeds `rtt`).
    pub fn tick(&mut self, now: Instant, rtt: Duration) {
        self.rtt = rtt;
        self.service();
        let enabled = self.nack_enabled();
        if let Some(st) = &mut self.st {
            st.drain(&mut self.c, self.playout.as_ref());
            st.steer(self.playout.as_ref(), now, &self.shared, self.playing);
            if enabled {
                st.expect_tail(now);
                st.scan_nacks(self.stream, now, rtt, self.opts.nack_retries, &mut self.nacks);
            }
        }
        self.flush_counters();
    }

    /// Next datagram to send to the stream's sender: pending NACKs, then the REPORT once a
    /// second while the stream is active.
    pub fn poll_transmit(&mut self, now: Instant) -> Option<Bytes> {
        let mut out = Vec::new();
        if !self.nacks.is_empty() {
            let n = self.nacks.len().min(MAX_NACK);
            let r = wire::encode_nack(self.stream, &self.nacks[..n], &mut out);
            self.nacks.drain(..n);
            return r.ok().map(|_| out.into());
        }
        let st = self.st.as_ref()?;
        let high = st.jb.highest?;
        if self
            .report_at
            .is_some_and(|t| now.saturating_duration_since(t) < Duration::from_secs(1))
        {
            return None;
        }
        self.report_at = Some(now);
        let ms = |s: f64| (s * 1e3).round().clamp(0.0, f64::from(u16::MAX)) as u16;
        let r = Report {
            stream: self.stream,
            highest_seq: high as u32,
            lost: std::mem::take(&mut self.report_lost).min(u32::from(u16::MAX)) as u16,
            late: std::mem::take(&mut self.report_late).min(u32::from(u16::MAX)) as u16,
            jitter_ms: ms(st.jitter),
            buffer_ms: ms(st.fill_secs()),
        };
        r.encode(&mut out).ok()?;
        Some(out.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pcm(depths: &[PcmBitDepth]) -> StreamFormat {
        StreamFormat {
            sample_rate: 48000,
            block_size: 120,
            channels: 2,
            voice: false,
            encodings: depths.iter().map(|&d| Encoding::Pcm(d)).collect(),
        }
    }

    #[test]
    fn format_wire_golden_and_round_trip() {
        let f = StreamFormat {
            voice: true,
            encodings: vec![
                Encoding::Opus {
                    bitrate: 96_000,
                    complexity: 10,
                },
                Encoding::Opus {
                    bitrate: 0x0102_0304,
                    complexity: 5,
                },
            ],
            block_size: 480,
            ..pcm(&[])
        };
        let w = f.to_wire(3, 7);
        assert_eq!(
            (w.codec, &w.encodings[..], &w.options[..]),
            (
                CODEC_OPUS,
                &[0, 1][..],
                &[1, 0x00, 0x77, 0x01, 0x00, 10, 4, 3, 2, 1, 5][..]
            )
        );
        assert_eq!(StreamFormat::from_wire(&w), Ok(f.clone()));
        assert!(f.fec());
        let p = pcm(&[PcmBitDepth::Float32, PcmBitDepth::Int16]);
        let w = p.to_wire(0, 0);
        assert_eq!((w.codec, &w.options[..]), (CODEC_PCM, &[0, 2, 0][..]));
        assert_eq!(StreamFormat::from_wire(&w), Ok(p));
        // truncated options, bad depth, unknown codec, non-sequential ids
        for bad in [
            Format {
                options: vec![0, 2],
                ..w.clone()
            },
            Format {
                options: vec![0, 2, 9],
                ..w.clone()
            },
            Format {
                codec: 9,
                ..w.clone()
            },
            Format {
                encodings: vec![1, 0],
                ..w.clone()
            },
        ] {
            assert!(StreamFormat::from_wire(&bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn format_validation() {
        let ok = pcm(&[PcmBitDepth::Int16]);
        assert_eq!(ok.validate(), Ok(()));
        let opus = |sr, bs| StreamFormat {
            sample_rate: sr,
            block_size: bs,
            encodings: vec![Encoding::Opus {
                bitrate: 64000,
                complexity: 5,
            }],
            ..ok.clone()
        };
        for bs in [120, 240, 480, 960] {
            assert_eq!(opus(48000, bs).validate(), Ok(()));
        }
        assert!(opus(48000, 256).validate().is_err());
        assert!(opus(44100, 441).validate().is_err());
        assert!(!opus(48000, 240).fec()); // not voice
        let bad = [
            StreamFormat {
                channels: 0,
                ..ok.clone()
            },
            StreamFormat {
                channels: 65,
                ..ok.clone()
            },
            StreamFormat {
                block_size: 8,
                ..ok.clone()
            },
            StreamFormat {
                sample_rate: 192_000,
                channels: 32,
                ..ok.clone()
            },
            StreamFormat {
                encodings: vec![],
                ..ok.clone()
            },
            StreamFormat {
                encodings: vec![Encoding::Pcm(PcmBitDepth::Int16); 3],
                ..ok.clone()
            },
            StreamFormat {
                encodings: vec![
                    Encoding::Pcm(PcmBitDepth::Int16),
                    Encoding::Opus {
                        bitrate: 1,
                        complexity: 1,
                    },
                ],
                ..ok.clone()
            },
        ];
        for f in bad {
            assert!(f.validate().is_err(), "{f:?}");
        }
        // 64 ch x 8192 frames float64 exceeds the 255-fragment block limit
        let huge = StreamFormat {
            sample_rate: 48000,
            channels: 64,
            block_size: 8192,
            encodings: vec![Encoding::Pcm(PcmBitDepth::Float64)],
            voice: false,
        };
        assert_eq!(huge.validate(), Err(Error::TooLong));
    }

    /// A NACK flood gets a subscriber at most its budget (a quarter of its stream's rate, plus
    /// the burst), each fragment at most `RESEND_RETRIES` times, nothing for blocks outside the
    /// history, and nothing while paused; other subscribers keep their own budgets.
    #[test]
    fn resend_budget_and_retry_limit() {
        // PCM 16 mono 48 kHz, 120-frame blocks: 768 kbit/s, 248-byte datagrams
        let f = StreamFormat {
            channels: 1,
            ..pcm(&[PcmBitDepth::Int16])
        };
        let dev = Device {
            sample_rate: 48000,
            block_size: 120,
            channels: 1,
        };
        let (mut tx, mut audio) = TxStream::new(0, dev, f, TxOptions::default()).unwrap();
        assert_eq!(tx.bitrate(0), 768_000);
        tx.subscribe(1, 0, 0);
        tx.subscribe(2, 0, 0);
        let t0 = Instant::now();
        for k in 0..100u32 {
            let x = [0.25f32; 120];
            assert!(audio.process(&[&x[..]], t0 + Duration::from_micros(2500 * u64::from(k))));
            tx.pump(|_, _| {});
        }
        let nack = |entries: &[(u32, u8)]| {
            let mut b = Vec::new();
            wire::encode_nack(0, entries, &mut b).unwrap();
            b
        };
        let all: Vec<(u32, u8)> = (0..100).map(|s| (s, NACK_ALL)).collect();
        let raw = nack(&all[..MAX_NACK.min(100)]);
        let Ok(wire::Datagram::Nack(n)) = wire::Datagram::decode(&raw) else { panic!() };
        let mut bytes = [0usize; 3];
        let flood = |tx: &mut TxStream, id: u64, now: Instant, bytes: &mut [usize; 3]| {
            tx.handle_nack_at(now, id, &n, |to, d| {
                assert_eq!(to, id);
                bytes[id as usize] += d.len();
            });
        };
        // a new subscriber starts with an empty bucket (re-subscribing buys nothing); 100 ms
        // later it holds the burst (100 ms of a quarter of 768 kbit/s = 2400 B)
        flood(&mut tx, 1, t0 - Duration::from_millis(500), &mut bytes);
        assert_eq!(bytes[1], 0);
        flood(&mut tx, 1, t0, &mut bytes);
        assert!((2400 - 248..=2400).contains(&bytes[1]), "{}", bytes[1]);
        // 1000 NACKs over one second: about a quarter of the stream rate (24 kB/s) more
        for ms in 1..=1000 {
            flood(&mut tx, 1, t0 + Duration::from_millis(ms), &mut bytes);
        }
        assert!((24_000..=24_000 + 2400).contains(&bytes[1]), "{}", bytes[1]);
        // the other subscriber has its own budget
        flood(&mut tx, 2, t0, &mut bytes);
        flood(&mut tx, 2, t0 + Duration::from_secs(1), &mut bytes);
        assert!(bytes[2] > 2000, "{}", bytes[2]);
        // unsubscribing and subscribing again starts from empty
        tx.unsubscribe(2);
        tx.subscribe(2, 0, 0);
        let before = bytes[2];
        flood(&mut tx, 2, Instant::now(), &mut bytes);
        assert_eq!(bytes[2], before);
        // plenty of budget later: each block at most RESEND_RETRIES times in total
        bytes[1] = 0;
        for s in 2..60 {
            flood(&mut tx, 1, t0 + Duration::from_secs(s), &mut bytes);
        }
        let resent: usize = tx.history.iter().flat_map(|b| &b.resent).filter(|r| r.0 == 1)
            .map(|r| usize::from(r.2)).sum();
        assert_eq!(resent, 100 * usize::from(RESEND_RETRIES));
        // blocks before the history or not sent yet: nothing, cheaply
        let raw = nack(&[(5000, NACK_ALL), (u32::MAX, 0)]);
        let Ok(wire::Datagram::Nack(n2)) = wire::Datagram::decode(&raw) else { panic!() };
        tx.handle_nack_at(t0 + Duration::from_secs(70), 2, &n2, |_, _| panic!("resent"));
        // paused
        tx.set_resend_paused(2, true);
        tx.handle_nack_at(t0 + Duration::from_secs(80), 2, &n, |_, _| panic!("paused"));
        tx.set_resend_paused(2, false);
        let mut got = 0;
        tx.handle_nack_at(t0 + Duration::from_secs(90), 2, &n, |_, _| got += 1);
        assert!(got > 0);
    }

    /// NACKs that cannot be served (no such fragment, no budget, retries used up) build no
    /// datagram; the others build exactly what they send.
    #[test]
    fn unservable_nacks_copy_nothing() {
        // PCM 16 mono 48 kHz, 120-frame blocks: one 248-byte datagram per block
        let f = StreamFormat {
            channels: 1,
            ..pcm(&[PcmBitDepth::Int16])
        };
        let dev = Device {
            sample_rate: 48000,
            block_size: 120,
            channels: 1,
        };
        let (mut tx, mut audio) = TxStream::new(0, dev, f, TxOptions::default()).unwrap();
        tx.subscribe(1, 0, 0);
        let t0 = Instant::now();
        for k in 0..100u32 {
            let x = [0.25f32; 120];
            assert!(audio.process(&[&x[..]], t0 + Duration::from_micros(2500 * u64::from(k))));
            tx.pump(|_, _| {});
        }
        let raw = |frag: u8| {
            let mut b = Vec::new();
            let n = MAX_NACK.min(100) as u32;
            wire::encode_nack(0, &(0..n).map(|s| (s, frag)).collect::<Vec<_>>(), &mut b).unwrap();
            b
        };
        let (absent, all) = (raw(7), raw(NACK_ALL));
        let nack = |b| match wire::Datagram::decode(b) {
            Ok(wire::Datagram::Nack(n)) => n,
            _ => panic!(),
        };
        let (absent, all) = (nack(&absent), nack(&all));
        let copies = || wire::FRAGMENT_COPIES.with(|c| c.get());
        let c0 = copies();
        // fragment 7 of single-datagram blocks, with plenty of budget
        for s in 1..20 {
            tx.handle_nack_at(t0 + Duration::from_secs(s), 1, &absent, |_, _| panic!("sent"));
        }
        assert_eq!(copies(), c0);
        // an empty bucket (a new subscriber)
        tx.subscribe(2, 0, 0);
        tx.handle_nack_at(t0 - Duration::from_millis(500), 2, &all, |_, _| panic!("sent"));
        assert_eq!(copies(), c0);
        // within the budget
        let mut sent = 0;
        for ms in 0..1000 {
            let before = (copies(), sent);
            tx.handle_nack_at(t0 + Duration::from_millis(ms), 1, &all, |_, _| sent += 1);
            assert_eq!(copies() - before.0, sent - before.1);
        }
        // retries used up
        let mut resent = 0;
        for s in 20..200 {
            tx.handle_nack_at(t0 + Duration::from_secs(s), 1, &all, |_, _| resent += 1);
        }
        assert!(resent > 0);
        let c1 = copies();
        for s in 200..210 {
            tx.handle_nack_at(t0 + Duration::from_secs(s), 1, &all, |_, _| panic!("sent"));
        }
        assert_eq!(copies(), c1);
    }

    /// A single-Opus stream's ladder: the encoder's bit rate changes in place (same
    /// generation), the encoded blocks shrink, PCM has no bit rate to change.
    #[test]
    fn opus_bitrate_changes_in_place() {
        let f = StreamFormat {
            sample_rate: 48000,
            block_size: 480,
            channels: 1,
            voice: false,
            encodings: vec![Encoding::Opus {
                bitrate: 96_000,
                complexity: 10,
            }],
        };
        let dev = Device {
            sample_rate: 48000,
            block_size: 480,
            channels: 1,
        };
        let (mut tx, mut audio) = TxStream::new(1, dev, f, TxOptions::default()).unwrap();
        tx.subscribe(1, 0, 0);
        let t0 = Instant::now();
        let mut k = 0u32;
        let mut run = |tx: &mut TxStream, blocks: u32| {
            let mut bytes = 0;
            for _ in 0..blocks {
                let x: Vec<f32> = (0..480)
                    .map(|i| ((k * 480 + i) as f32 * 0.05).sin() * 0.5 + ((i * 7919) % 13) as f32 * 0.01)
                    .collect();
                assert!(audio.process(&[&x[..]], t0 + Duration::from_millis(10 * u64::from(k))));
                k += 1;
                tx.pump(|_, d| bytes += d.len());
            }
            bytes / blocks as usize
        };
        let generation = tx.wire_format().generation;
        let high = run(&mut tx, 50);
        assert!(tx.set_bitrate(0, 24_000));
        assert_eq!(tx.bitrate(0), 24_000);
        let low = run(&mut tx, 50);
        eprintln!("bytes per block: {high} at 96 kbit/s, {low} at 24 kbit/s");
        assert!(low * 2 < high, "{high} -> {low}");
        assert_eq!(tx.wire_format().generation, generation, "no new generation");
        assert!(!tx.set_bitrate(1, 1000), "no such encoding");
        let (mut p, _) = TxStream::new(0, dev, pcm(&[PcmBitDepth::Int16]), TxOptions::default())
            .unwrap();
        assert!(!p.set_bitrate(0, 1000));
        assert_eq!(p.bitrate(0), 16 * 48000 * 2);
    }

    #[test]
    fn pcm_depth_from_length() {
        let mut out = [1.0f32; 4];
        assert!(pcm_block(&[0x40, 0, 0x40, 0, 0x40, 0, 0x40, 0], &mut out));
        assert_eq!(out, [0.5; 4]);
        assert!(!pcm_block(&[0; 5], &mut out));
        assert!(!pcm_block(&[0; 20], &mut out)); // 5 bytes per sample
        assert!(pcm_block(&[0; 16], &mut out));
    }

    /// PCM concealment fades from the last sample to silence, the next real block fades in;
    /// non-finite and huge decoded values are sanitised.
    #[test]
    fn pcm_conceal_and_sanitise() {
        let f = StreamFormat {
            channels: 1,
            block_size: 128,
            ..pcm(&[PcmBitDepth::Float32])
        };
        let mut d = Dec::new(&f).unwrap();
        let mut out = vec![0.0f32; 128];
        let bytes: Vec<u8> = (0..128)
            .flat_map(|i| {
                if i == 5 {
                    f32::NAN
                } else if i == 6 {
                    1e9
                } else {
                    0.5f32
                }
                .to_be_bytes()
            })
            .collect();
        assert!(d.block(Some(&bytes), None, 1, &mut out));
        assert_eq!((out[5], out[6], out[127]), (0.0, 100.0, 0.5));
        assert!(!d.block(None, None, 1, &mut out));
        assert!(out[0] < 0.5 && out[0] > 0.49 && out[63] == 0.0 && out[127] == 0.0);
        assert!(out.windows(2).all(|w| w[1] <= w[0]));
        assert!(d.block(Some(&bytes), None, 1, &mut out));
        assert!(out[0] < 0.01 && out[64] == 0.5);
    }

    /// The audio half's resampler order changes only for a target of the stream's own
    /// generation: one for another generation (stale, or a format not here yet) changes
    /// nothing; a new generation drops it until its own target comes.
    #[test]
    fn sync_order_only_for_a_target_of_this_generation() {
        let dev = Device {
            sample_rate: 48000,
            block_size: 128,
            channels: 1,
        };
        let (mut rx, _audio) = RxStream::new(0, dev, RxOptions::default());
        let f = pcm(&[PcmBitDepth::Float32]);
        rx.set_format(&f.to_wire(0, 3)).unwrap();
        let target = |generation| Playout {
            generation,
            seq: 0,
            due: Instant::now(),
            span: Duration::from_millis(150),
        };
        let on = |rx: &RxStream| rx.shared.ratio_first.load(Ordering::Relaxed);
        rx.set_playout(Some(target(4)));
        assert!(!on(&rx), "another generation's target");
        rx.set_playout(Some(target(3)));
        assert!(on(&rx));
        rx.set_format(&f.to_wire(0, 4)).unwrap();
        rx.set_playout(Some(target(3)));
        assert!(!on(&rx), "the old generation's target after a new format");
        rx.set_playout(Some(target(4)));
        assert!(on(&rx));
        rx.set_playout(None);
        assert!(!on(&rx));
    }
}
