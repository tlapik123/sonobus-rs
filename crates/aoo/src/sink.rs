//! Audio stream sink (`sink.cpp`): receives streams from any number of sources, reassembles
//! and reorders blocks, requests resends, decodes and mixes them out with drift compensation.
//!
//! Two handles, created together by [`Sink::new`]:
//!
//! - [`Sink`] lives on the network thread: [`Sink::handle_message`] for every
//!   `/aoo/sink/<id>/...` and `/d` datagram, [`Sink::send`] every few ms (SonoBus: 20 ms and
//!   after every received packet) for format/resend requests, ping replies and invitations.
//!   Blocks are decoded here, as in the C++, into one lock-free ring per source.
//! - [`SinkAudio`] lives on the audio thread: [`SinkAudio::process`] resamples each source's
//!   ring to the device rate (DLL-driven if dynamic resampling is on) and sums them into the
//!   output. It never allocates, locks, blocks or logs. Per-source audio state is handed over
//!   through an SPSC command ring and handed back for deallocation on the network thread.
//!
//! Sequence numbers are compared wrap-safe, so a stream survives the `i32` wrap of a stock
//! source (which changes salt there; we then ask for the format). Robustness additions over
//! the C++, both harmless to stock peers: an invitation is repeated every second until the
//! source's format arrives, and a `/d` with an unknown salt from the address of a known source
//! that has gone quiet triggers a format request (the C++ dropped those silently forever).

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::codec::{Decoder, Format};
use crate::message::{Message, Requests};
use crate::resampler::Resampler;
use crate::time::{Dll, TIMEFILTER_BANDWIDTH, TimeTag, Timer};
use crate::{Setup, check_version, emit, make_version, seq_diff};

/// Most sources one sink tracks (entries are never removed, as in the C++).
pub const MAX_SOURCES: usize = 64;
/// Not in the C++: the largest jitter buffer a sink allocates (`buffersize_ms` is clamped to
/// it), so that no caller can make it allocate gigabytes. SonoBus' UI goes to 5 s.
pub const MAX_BUFFER_MS: i32 = 10_000;
/// Not in the C++: the most samples one source's jitter ring holds (64 MiB of `f32`), whatever
/// format the source announces (up to 255 channels at 768 kHz pass format validation). 5 s of
/// 8 channels at 192 kHz is 7.7 M.
const MAX_RING_SAMPLES: usize = 1 << 24;
const MAX_EVENTS: usize = 1024;
const MAX_RESEND_REQUESTS: usize = 256; // `sink.cpp:699`
const MAX_FRAMES: i32 = 64; // missing-frame bitmap width
const RESEND_CHECK_THRESHOLD: usize = 3; // `sink.cpp:1389`
const INVITE_REPEAT: f64 = 1.0;
/// A known source that delivered nothing for this long may be asked for its format when an
/// unknown-salt `/d` arrives from its address.
const QUIET_BEFORE_FORMAT_REQUEST: f64 = 0.5;

/// Sink options. Defaults are the library's, except `protocol_flags` which SonoBus sets to
/// "compact data supported" (`SonobusPluginProcessor.cpp:6025-6026`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SinkOptions {
    /// Jitter buffer in ms (SonoBus: per peer).
    pub buffersize_ms: i32,
    /// Only used to batch resend requests.
    pub packetsize: i32,
    /// Resend attempts per block; 0 disables resending.
    pub resend_limit: i32,
    pub resend_interval_ms: i32,
    /// Max frames requested per check.
    pub resend_maxnumframes: i32,
    /// Sent in format requests and invitations (bit 0: compact `/d` supported).
    pub protocol_flags: i32,
    pub dynamic_resampling: bool,
    /// DLL bandwidth; takes effect on [`Sink::setup`].
    pub timefilter_bandwidth: f32,
}

impl Default for SinkOptions {
    fn default() -> Self {
        Self {
            buffersize_ms: 100,
            packetsize: 512,
            resend_limit: 5,
            resend_interval_ms: 10,
            resend_maxnumframes: 16,
            protocol_flags: crate::PROTOCOL_FLAG_COMPACT_DATA as i32,
            dynamic_resampling: true,
            timefilter_bandwidth: TIMEFILTER_BANDWIDTH,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SinkEvent {
    /// Source address and id.
    pub addr: SocketAddr,
    pub id: i32,
    pub kind: SinkEventKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SinkEventKind {
    /// First message from (or invitation of) this source.
    SourceAdded,
    /// New `/format`; `user_format` is the last user blob received (SonoBus: channel layout).
    FormatChanged {
        format: Format,
        user_format: Vec<u8>,
    },
    /// Playback started (`true`) or ran dry (`false`).
    StateChanged {
        playing: bool,
    },
    BlocksLost {
        count: i32,
    },
    BlocksReordered {
        count: i32,
    },
    BlocksResent {
        count: i32,
    },
    /// Blocks skipped by a transmission gap.
    BlocksGap {
        count: i32,
    },
    /// Ping from the source: `tt1` its send time, `tt2` our receive time.
    Ping {
        tt1: TimeTag,
        tt2: TimeTag,
    },
}

/// Flags the audio half reports back for one source.
#[derive(Debug, Default)]
struct SlotShared {
    playing: AtomicBool,
    underrun: AtomicBool,
    /// f64 bits of the samplerate of the block last played. Like the C++ `samplerate_`,
    /// it is what a `/d` without samplerate inherits (`sink.cpp:1028`, `1258`).
    samplerate: AtomicU64,
}

#[derive(Debug, Clone, Copy, Default)]
struct BlockInfo {
    samplerate: f64,
    channel: i32,
}

/// Audio-thread state of one source.
struct Slot {
    samples: Consumer<f32>,
    info: Consumer<BlockInfo>,
    nsamples: usize,
    nch: usize,
    resampler: Resampler,
    samplerate: f64,
    channel: i32,
    scratch: Vec<f32>,
    playing: bool,
    shared: Arc<SlotShared>,
}

impl Slot {
    /// `source_desc::process` (`sink.cpp:957-1093`).
    fn process(&mut self, out: &mut [&mut [f32]], n: usize, real_sr: f64) -> bool {
        let readsamples = n * self.nch;
        while !self.info.is_empty()
            && readsamples > self.resampler.read_available()
            && self.resampler.write_available() >= self.nsamples
        {
            let Ok(info) = self.info.pop() else { break };
            self.channel = info.channel;
            self.samplerate = info.samplerate;
            self.shared
                .samplerate
                .store(info.samplerate.to_bits(), Ordering::Relaxed);
            if let Ok(chunk) = self.samples.read_chunk(self.nsamples) {
                let (a, b) = chunk.as_slices();
                self.resampler.write(a);
                self.resampler.write(b);
                chunk.commit_all();
            }
        }
        self.resampler.update(self.samplerate, real_sr);
        if self.resampler.read_available() >= readsamples {
            let buf = &mut self.scratch[..readsamples];
            self.resampler.read(buf);
            for i in 0..self.nch {
                let chn = i as i64 + self.channel as i64;
                if let Some(o) = usize::try_from(chn).ok().and_then(|c| out.get_mut(c)) {
                    for (j, o) in o[..n].iter_mut().enumerate() {
                        *o += buf[j * self.nch + i];
                    }
                }
            }
            if !self.playing {
                self.playing = true;
                self.shared.playing.store(true, Ordering::Release);
            }
            true
        } else {
            if self.playing {
                self.playing = false;
                self.shared.playing.store(false, Ordering::Release);
                self.shared.underrun.store(true, Ordering::Release);
            }
            false
        }
    }
}

struct Install {
    index: usize,
    slot: Box<Slot>,
}

#[derive(Debug, Default)]
struct AudioShared {
    dynamic_resampling: AtomicBool,
}

/// Audio-thread half of a sink.
pub struct SinkAudio {
    slots: Vec<Option<Box<Slot>>>,
    installs: Consumer<Install>,
    garbage: Producer<Box<Slot>>,
    setup: Setup,
    bandwidth: f64,
    timer: Timer,
    dll: Dll,
    shared: Arc<AudioShared>,
}

impl SinkAudio {
    /// Fill `output` (one slice per channel, at most `blocksize` frames, all the same length)
    /// with the sum of all playing sources. `now` is the callback's wall-clock time. Returns
    /// whether any source contributed. Real-time safe.
    pub fn process(&mut self, output: &mut [&mut [f32]], now: TimeTag) -> bool {
        while !self.garbage.is_full() {
            let Ok(Install { index, slot }) = self.installs.pop() else {
                break;
            };
            if let Some(old) = self.slots[index].replace(slot) {
                let _ = self.garbage.push(old);
            }
        }
        for o in output.iter_mut() {
            o.fill(0.0);
        }
        let sr = self.setup.samplerate as f64;
        if self.timer.update(now) {
            self.dll
                .setup(sr, self.setup.blocksize, self.bandwidth, 0.0);
        } else {
            self.dll.update(self.timer.elapsed());
        }
        let ignore_dll = !self.shared.dynamic_resampling.load(Ordering::Relaxed)
            || (self.dll.samplerate() - sr).abs() > 0.1 * sr;
        let real_sr = if ignore_dll {
            sr
        } else {
            self.dll.samplerate()
        };
        let n = output
            .first()
            .map_or(0, |o| o.len())
            .min(self.setup.blocksize.max(0) as usize);
        let mut any = false;
        for slot in self.slots.iter_mut().flatten() {
            any |= slot.process(output, n, real_sr);
        }
        any
    }
}

/// One block being reassembled (`block`, `common.cpp:209-300`).
#[derive(Debug)]
struct Block {
    seq: i32,
    samplerate: f64,
    channel: i32,
    data: Vec<u8>,
    missing: u64,
    nframes: i32,
}

impl Block {
    fn complete(&self) -> bool {
        self.missing == 0
    }

    fn has_frame(&self, i: i32) -> bool {
        (self.missing >> i) & 1 == 0
    }

    /// Non-last frames go to `i * len`, the last one to the end. `false` if out of bounds.
    fn add_frame(&mut self, i: i32, d: &[u8]) -> bool {
        let total = self.data.len();
        let onset = if i == self.nframes - 1 {
            total.checked_sub(d.len())
        } else {
            (i as usize)
                .checked_mul(d.len())
                .filter(|o| o + d.len() <= total)
        };
        let Some(onset) = onset else { return false };
        self.data[onset..onset + d.len()].copy_from_slice(d);
        self.missing &= !(1u64 << i);
        true
    }
}

/// Resend bookkeeping per block (`block_ack`).
#[derive(Debug)]
struct Ack {
    seq: i32,
    count: i32,
    timestamp: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Invitation {
    None,
    Invite,
    Uninvite,
}

/// One data frame as received (`data_packet`).
struct Packet<'a> {
    seq: i32,
    samplerate: f64,
    channel: i32,
    total_size: i32,
    nframes: i32,
    frame: i32,
    data: &'a [u8],
}

/// Sink settings a source needs.
#[derive(Clone, Copy)]
struct Params {
    setup: Setup,
    opts: SinkOptions,
}

/// Network-thread state of one source (`source_desc`).
struct SourceDesc {
    addr: SocketAddr,
    id: i32,
    index: usize,
    salt: Option<i32>,
    decoder: Option<Decoder>,
    format: Option<Format>,
    user_format: Vec<u8>,
    protocol_flags: i32,
    newest: Option<i32>,
    next: Option<i32>,
    fadein: Option<i32>,
    channel: i32,
    queue: Vec<Block>,
    queue_cap: usize,
    acks: Vec<Ack>,
    resend: Vec<(i32, i32)>,
    samples: Option<Producer<f32>>,
    info: Option<Producer<BlockInfo>>,
    nsamples: usize,
    scratch: Vec<f32>,
    shared: Arc<SlotShared>,
    pending: Option<Box<Slot>>,
    recover: bool,
    need_format: bool,
    codec_change: Option<Format>,
    invitation: Invitation,
    inviting: bool,
    last_invite: f64,
    last_data: f64,
    last_format_request: f64,
    ping: Option<(u64, u64)>,
    playing: bool,
    lost: i32,
    reordered: i32,
    resent: i32,
    gap: i32,
    lost_since_ping: i32,
}

impl SourceDesc {
    fn new(addr: SocketAddr, id: i32, index: usize, salt: Option<i32>, flags: i32) -> Self {
        Self {
            addr,
            id,
            index,
            salt,
            decoder: None,
            format: None,
            user_format: Vec::new(),
            protocol_flags: flags,
            newest: None,
            next: None,
            fadein: None,
            channel: 0,
            queue: Vec::new(),
            queue_cap: 0,
            acks: Vec::new(),
            resend: Vec::new(),
            samples: None,
            info: None,
            nsamples: 0,
            scratch: Vec::new(),
            shared: Arc::default(),
            pending: None,
            recover: false,
            need_format: false,
            codec_change: None,
            invitation: Invitation::None,
            inviting: false,
            last_invite: f64::NEG_INFINITY,
            last_data: f64::NEG_INFINITY,
            last_format_request: f64::NEG_INFINITY,
            ping: None,
            playing: false,
            lost: 0,
            reordered: 0,
            resent: 0,
            gap: 0,
            lost_since_ping: 0,
        }
    }

    fn add_lost(&mut self, n: i32) {
        self.lost += n;
        self.lost_since_ping += n;
    }

    /// `do_update` (`sink.cpp:753-802`): new rings pre-filled with silence, fresh audio state.
    fn update(&mut self, p: &Params) {
        let Some(f) = self.format else { return };
        let (cbs, csr, nch) = (f.blocksize, f.samplerate, f.nchannels as usize);
        let bufsize =
            (p.opts.buffersize_ms as f64 * csr as f64 * 0.001).max(p.setup.blocksize as f64);
        self.nsamples = nch * cbs as usize;
        let nbuffers = (((bufsize as i32 + cbs - 1) / cbs).max(1) as usize)
            .min(MAX_RING_SAMPLES / self.nsamples.max(1))
            .max(1);
        let (mut sp, sc) = RingBuffer::new(nbuffers * self.nsamples);
        let (mut ip, ic) = RingBuffer::new(nbuffers);
        if let Ok(c) = sp.write_chunk(nbuffers * self.nsamples) {
            c.commit_all(); // zero-initialised
        }
        for _ in 0..nbuffers {
            let _ = ip.push(BlockInfo {
                samplerate: csr as f64,
                channel: 0,
            });
        }
        let mut resampler = Resampler::default();
        resampler.setup(cbs as usize, p.setup.blocksize as usize, nch);
        self.shared = Arc::new(SlotShared {
            samplerate: AtomicU64::new((csr as f64).to_bits()),
            ..SlotShared::default()
        });
        self.pending = Some(Box::new(Slot {
            samples: sc,
            info: ic,
            nsamples: self.nsamples,
            nch,
            resampler,
            samplerate: csr as f64,
            channel: 0,
            scratch: vec![0.0; p.setup.blocksize.max(1) as usize * nch],
            playing: false,
            shared: self.shared.clone(),
        }));
        self.samples = Some(sp);
        self.info = Some(ip);
        self.scratch.resize(self.nsamples, 0.0);
        self.queue.clear();
        self.queue_cap = nbuffers + 8;
        self.newest = None;
        self.next = None;
        self.fadein = None;
        self.channel = 0;
        // `stream_state::reset`
        (self.lost, self.reordered, self.resent, self.gap) = (0, 0, 0, 0);
        self.playing = false;
        self.need_format = false;
        self.codec_change = None;
        self.ping = None;
        self.acks.clear();
        self.recover = true;
    }

    /// Free block slots in the audio ring.
    fn ring_space(&self) -> usize {
        match (&self.samples, &self.info) {
            (Some(s), Some(i)) => (s.slots() / self.nsamples.max(1)).min(i.slots()),
            _ => 0,
        }
    }

    /// Decode one block (or conceal a missing one) into the ring. `false` if the ring is full.
    fn push_block(
        &mut self,
        data: Option<&[u8]>,
        info: BlockInfo,
        fade: bool,
        zero_on_error: bool,
    ) -> bool {
        if self.ring_space() == 0 {
            return false;
        }
        let Some(dec) = &mut self.decoder else {
            return false;
        };
        let buf = &mut self.scratch[..self.nsamples];
        if !dec.decode(data, buf) {
            if zero_on_error {
                buf.fill(0.0);
            }
        } else if fade {
            let nch = self.format.map_or(1, |f| f.nchannels as usize);
            let frames = buf.len() / nch;
            let delta = 1.0 / frames as f32;
            let mut gain = 0.0f32;
            for frame in buf.chunks_exact_mut(nch) {
                for x in frame {
                    *x *= gain;
                }
                gain += delta;
            }
        }
        if let (Some(s), Some(i)) = (&mut self.samples, &mut self.info) {
            let _ = s.push_entire_slice(buf);
            let _ = i.push(info);
        }
        true
    }

    fn nominal_info(&self) -> BlockInfo {
        BlockInfo {
            samplerate: self.format.map_or(0.0, |f| f.samplerate as f64),
            channel: self.channel,
        }
    }

    /// Fill the ring with concealment blocks, leaving room for one block.
    fn fill_silence(&mut self) -> usize {
        let mut count = 0;
        while self.ring_space() > 1 {
            let info = self.nominal_info();
            self.push_block(None, info, false, false);
            count += 1;
        }
        count
    }

    fn ack(&mut self, seq: i32, limit: i32) -> &mut Ack {
        let i = match self.acks.iter().position(|a| a.seq == seq) {
            Some(i) => i,
            None => {
                self.acks.push(Ack {
                    seq,
                    count: limit,
                    timestamp: -1e9,
                });
                self.acks.len() - 1
            }
        };
        &mut self.acks[i]
    }

    fn remove_ack(&mut self, seq: i32) {
        self.acks.retain(|a| a.seq != seq);
    }

    /// `source_desc::handle_data` (`sink.cpp:854-907`).
    fn handle_data(&mut self, p: &Params, salt: i32, d: &Packet, now: f64) {
        if self.salt != Some(salt) {
            self.need_format = true;
            return;
        }
        if self.decoder.is_none() {
            return;
        }
        // reject malformed frames (the C++ trusts them)
        let valid = if d.total_size == 0 {
            true // empty block = source skipped one
        } else {
            (1..=MAX_FRAMES).contains(&d.nframes)
                && (0..d.nframes).contains(&d.frame)
                && d.total_size > 0
                && d.total_size <= MAX_FRAMES * crate::MAX_PACKET_SIZE as i32
                && d.data.len() <= d.total_size as usize
        };
        if !valid {
            return;
        }
        self.last_data = now;
        if self.next.is_none() {
            self.next = Some(d.seq);
            self.fadein = Some(d.seq);
        }
        if !self.check_packet(d) || !self.add_packet(d) {
            return;
        }
        self.process_blocks(p);
        self.check_outdated_blocks();
        self.check_missing_blocks(p, now);
    }

    fn next(&self) -> i32 {
        self.next.unwrap_or(0)
    }

    /// `sink.cpp:1112-1199`.
    fn check_packet(&mut self, d: &Packet) -> bool {
        if seq_diff(d.seq, self.next()) < 0 {
            return false; // too old
        }
        let diff = self.newest.map(|n| seq_diff(d.seq, n));
        let large_gap = diff.is_some_and(|diff| diff > self.queue_cap as i32);
        let recover = std::mem::take(&mut self.recover);
        let dropped = d.total_size == 0;
        let underrun = self.shared.underrun.swap(false, Ordering::AcqRel);
        match diff {
            Some(diff) if diff < 0 => {
                if self.acks.iter().any(|a| a.seq == d.seq) {
                    self.resent += 1;
                } else {
                    self.reordered += 1;
                }
            }
            _ => self.newest = Some(d.seq),
        }
        if large_gap || recover || dropped || underrun {
            self.add_lost(self.queue.len() as i32);
            if let Some(diff) = diff.filter(|&d| d > 1) {
                self.gap += diff - 1;
            }
            self.queue.clear();
            self.acks.clear();
            self.next = Some(d.seq);
            if self.fill_silence() > 0 {
                self.fadein = self.next;
            }
            if dropped {
                self.next = Some(d.seq.wrapping_add(1));
                self.fadein = self.next;
                return false;
            }
        }
        true
    }

    /// `sink.cpp:1201-1277`.
    fn add_packet(&mut self, d: &Packet) -> bool {
        if let Some(b) = self.queue.iter_mut().find(|b| b.seq == d.seq) {
            if d.frame >= b.nframes || b.has_frame(d.frame) {
                return false; // duplicate
            }
            return b.add_frame(d.frame, d.data);
        }
        if self.queue.len() >= self.queue_cap {
            let old = self.queue[0].seq;
            if Some(old) == self.next && self.queue[0].complete() {
                // the source is too fast: flush everything
                self.add_lost(self.queue.len() as i32);
                self.queue.clear();
                self.acks.clear();
                self.fill_silence();
                self.next = Some(d.seq);
            } else {
                let info = self.nominal_info();
                self.push_block(None, info, false, false);
                self.add_lost(1);
                self.remove_ack(old);
                if seq_diff(self.next(), old) <= 0 {
                    self.next = Some(old.wrapping_add(1));
                }
                self.queue.remove(0);
            }
        }
        let mut block = Block {
            seq: d.seq,
            samplerate: if d.samplerate > 0.0 {
                d.samplerate
            } else {
                f64::from_bits(self.shared.samplerate.load(Ordering::Relaxed))
            },
            channel: if d.channel >= 0 {
                d.channel
            } else {
                self.channel
            },
            data: vec![0; d.total_size as usize],
            missing: if d.nframes >= 64 {
                u64::MAX
            } else {
                (1u64 << d.nframes) - 1
            },
            nframes: d.nframes,
        };
        if !block.add_frame(d.frame, d.data) {
            return false;
        }
        let pos = self
            .queue
            .iter()
            .position(|b| seq_diff(b.seq, d.seq) > 0)
            .unwrap_or(self.queue.len());
        self.queue.insert(pos, block);
        true
    }

    /// Move consecutive complete blocks (or abandoned holes) into the ring (`sink.cpp:1279-1367`).
    fn process_blocks(&mut self, p: &Params) {
        let mut next = self.next();
        let mut idx = 0;
        while idx < self.queue.len() && self.ring_space() > 0 {
            let b = &self.queue[idx];
            let (bseq, ready) = (b.seq, b.seq == next && b.complete());
            let fade = Some(bseq) == self.fadein;
            if ready {
                let info = BlockInfo {
                    samplerate: b.samplerate,
                    channel: b.channel,
                };
                let data = std::mem::take(&mut self.queue[idx].data);
                self.push_block(Some(&data), info, fade, true);
                self.queue[idx].data = data;
                idx += 1;
            } else if self.ack(next, p.opts.resend_limit).count == 0 {
                // won't be resent any more: conceal
                if bseq == next {
                    idx += 1;
                }
                let info = self.nominal_info();
                self.push_block(None, info, fade, true);
                self.add_lost(1);
            } else {
                break;
            }
            if fade {
                self.fadein = None;
            }
            next = next.wrapping_add(1);
        }
        self.next = Some(next);
        for b in self.queue.drain(..idx) {
            self.acks.retain(|a| a.seq != b.seq);
        }
    }

    fn check_outdated_blocks(&mut self) {
        let Some(newest) = self.newest else { return };
        while let Some(front) = self.queue.first() {
            if seq_diff(newest, front.seq) < self.queue_cap as i32 {
                break;
            }
            let old = front.seq;
            self.remove_ack(old);
            self.queue.remove(0);
            if seq_diff(self.next(), old) <= 0 {
                self.next = Some(old.wrapping_add(1));
            }
            self.add_lost(1);
        }
    }

    /// Queue resend requests for holes (`sink.cpp:1392-1475`).
    fn check_missing_blocks(&mut self, p: &Params, now: f64) {
        if self.queue.is_empty() {
            self.acks.clear();
            return;
        }
        if self.queue.len() < RESEND_CHECK_THRESHOLD {
            return;
        }
        let (limit, interval) = (
            p.opts.resend_limit,
            p.opts.resend_interval_ms as f64 * 0.001,
        );
        let maxframes = p.opts.resend_maxnumframes.max(1);
        let mut numframes = 0;
        // incomplete blocks except the newest
        'incomplete: for k in 0..self.queue.len() - 1 {
            let (seq, complete, nframes) = {
                let b = &self.queue[k];
                (b.seq, b.complete(), b.nframes)
            };
            if complete || self.resend.len() >= MAX_RESEND_REQUESTS {
                continue;
            }
            if self.ack(seq, limit).update(now, interval) {
                for i in 0..nframes {
                    if !self.queue[k].has_frame(i) {
                        if numframes < maxframes {
                            self.resend.push((seq, i));
                            numframes += 1;
                        } else {
                            break 'incomplete;
                        }
                    }
                }
            }
        }
        // whole missing blocks
        let mut next = self.next();
        'missing: for k in 0..self.queue.len() {
            let (seq, nframes) = (self.queue[k].seq, self.queue[k].nframes);
            let missing = seq_diff(seq, next);
            for i in 0..missing.max(0) {
                if self.resend.len() >= MAX_RESEND_REQUESTS {
                    break;
                }
                let s = next.wrapping_add(i);
                if self.ack(s, limit).update(now, interval) {
                    if numframes + nframes <= maxframes {
                        self.resend.push((s, -1));
                        numframes += nframes;
                    } else {
                        break 'missing;
                    }
                }
            }
            next = seq.wrapping_add(1);
        }
        let first = self.next();
        self.acks.retain(|a| seq_diff(a.seq, first) >= 0);
    }
}

impl Ack {
    fn update(&mut self, time: f64, interval: f64) -> bool {
        if self.count > 0 && time - self.timestamp >= interval {
            self.timestamp = time;
            self.count -= 1;
            return true;
        }
        false
    }
}

type Counter = fn(i32) -> SinkEventKind;

fn push_event(q: &mut VecDeque<SinkEvent>, addr: SocketAddr, id: i32, kind: SinkEventKind) {
    if q.len() < MAX_EVENTS {
        q.push_back(SinkEvent { addr, id, kind });
    }
}

/// Network-thread half of a sink.
pub struct Sink {
    id: i32,
    /// [`Self::set_max_sources`].
    max_sources: usize,
    p: Params,
    sources: Vec<SourceDesc>,
    installs: Producer<Install>,
    garbage: Consumer<Box<Slot>>,
    audio: Arc<AudioShared>,
    events: VecDeque<SinkEvent>,
    packet: Vec<u8>,
}

fn audio_half(
    setup: Setup,
    opts: &SinkOptions,
    shared: Arc<AudioShared>,
) -> (SinkAudio, Producer<Install>, Consumer<Box<Slot>>) {
    let (ip, ic) = RingBuffer::new(2 * MAX_SOURCES);
    let (gp, gc) = RingBuffer::new(2 * MAX_SOURCES);
    let audio = SinkAudio {
        slots: (0..MAX_SOURCES).map(|_| None).collect(),
        installs: ic,
        garbage: gp,
        setup,
        bandwidth: opts.timefilter_bandwidth.clamp(0.0, 1.0) as f64,
        timer: Timer::default(),
        dll: Dll::default(),
        shared,
    };
    (audio, ip, gc)
}

impl Sink {
    /// `setup` values must be positive.
    pub fn new(id: i32, setup: Setup, opts: SinkOptions) -> (Self, SinkAudio) {
        let opts = SinkOptions {
            buffersize_ms: opts.buffersize_ms.clamp(0, MAX_BUFFER_MS),
            ..opts
        };
        let shared = Arc::new(AudioShared::default());
        shared
            .dynamic_resampling
            .store(opts.dynamic_resampling, Ordering::Relaxed);
        let (audio, installs, garbage) = audio_half(setup, &opts, shared.clone());
        let sink = Self {
            id,
            max_sources: MAX_SOURCES,
            p: Params { setup, opts },
            sources: Vec::new(),
            installs,
            garbage,
            audio: shared,
            events: VecDeque::with_capacity(64),
            packet: Vec::with_capacity(crate::MAX_PACKET_SIZE),
        };
        (sink, audio)
    }

    pub fn id(&self) -> i32 {
        self.id
    }

    /// New device setup: all sources restart buffering. Returns the audio half to install on
    /// the audio thread; the previous one stops receiving audio.
    pub fn setup(&mut self, setup: Setup) -> SinkAudio {
        self.p.setup = setup;
        let (audio, installs, garbage) = audio_half(setup, &self.p.opts, self.audio.clone());
        self.installs = installs;
        self.garbage = garbage;
        self.update_sources();
        audio
    }

    pub fn options(&self) -> SinkOptions {
        self.p.opts
    }

    /// Not in the C++: most sources this sink tracks (1..=[`MAX_SOURCES`]; entries are never
    /// removed, so this bounds every source it will ever decode). Already known ones stay.
    pub fn set_max_sources(&mut self, n: usize) {
        self.max_sources = n.clamp(1, MAX_SOURCES);
    }

    /// A changed buffer size restarts buffering of every source.
    pub fn set_options(&mut self, opts: SinkOptions) {
        let resize = opts.buffersize_ms != self.p.opts.buffersize_ms;
        self.p.opts = SinkOptions {
            packetsize: opts.packetsize.clamp(64, crate::MAX_PACKET_SIZE as i32),
            buffersize_ms: opts.buffersize_ms.clamp(0, MAX_BUFFER_MS),
            resend_limit: opts.resend_limit.max(0),
            resend_interval_ms: opts.resend_interval_ms.max(0),
            ..opts
        };
        self.audio
            .dynamic_resampling
            .store(opts.dynamic_resampling, Ordering::Relaxed);
        if resize {
            self.update_sources();
        }
    }

    /// Invite a source (creating its entry). The invitation is repeated every second until its
    /// format arrives or it is uninvited.
    pub fn invite_source(&mut self, addr: SocketAddr, id: i32) -> bool {
        let Some(i) = self.find_or_add(addr, id, None) else {
            return false;
        };
        let s = &mut self.sources[i];
        s.invitation = Invitation::Invite;
        s.inviting = true;
        s.last_invite = f64::NEG_INFINITY;
        true
    }

    pub fn uninvite_source(&mut self, addr: SocketAddr, id: i32) -> bool {
        let Some(s) = self.find(addr, id) else {
            return false;
        };
        s.invitation = Invitation::Uninvite;
        s.inviting = false;
        true
    }

    /// `sink::reset`: every source's buffers start over (refilled with silence), dropping
    /// what was buffered.
    pub fn reset(&mut self) {
        self.update_sources();
    }

    /// Sources known to this sink (see [`Self::set_max_sources`]).
    pub fn source_count(&self) -> usize {
        self.sources.len()
    }

    pub fn uninvite_all(&mut self) {
        for s in &mut self.sources {
            s.invitation = Invitation::Uninvite;
            s.inviting = false;
        }
    }

    /// Ask a source to switch codec (it keeps its channel count and samplerate).
    pub fn request_codec_change(&mut self, addr: SocketAddr, id: i32, format: Format) -> bool {
        self.find(addr, id)
            .map(|s| s.codec_change = Some(format))
            .is_some()
    }

    /// Move a source to a new address (peer NAT rebinding).
    pub fn repoint_source(&mut self, addr: SocketAddr, id: i32, new_addr: SocketAddr) -> bool {
        self.find(addr, id).map(|s| s.addr = new_addr).is_some()
    }

    pub fn source_format(&self, addr: SocketAddr, id: i32) -> Option<&Format> {
        self.sources
            .iter()
            .find(|s| s.addr == addr && s.id == id)
            .and_then(|s| s.format.as_ref())
    }

    /// Jitter buffer fill, 0..=1 (`aoo_opt_buffer_fill_ratio`).
    pub fn buffer_fill_ratio(&self, addr: SocketAddr, id: i32) -> Option<f32> {
        let s = self.sources.iter().find(|s| s.addr == addr && s.id == id)?;
        let p = s.samples.as_ref()?;
        let cap = p.buffer().capacity();
        Some((cap - p.slots()) as f32 / cap as f32)
    }

    pub fn poll_event(&mut self) -> Option<SinkEvent> {
        self.events.pop_front()
    }

    /// Handle a datagram for this sink. Returns `false` if it is not for us, in particular a
    /// `/d` whose salt matches none of our sources (so the caller can try other sinks).
    pub fn handle_message(&mut self, from: SocketAddr, msg: &Message, now: TimeTag) -> bool {
        let t = now.to_secs();
        match *msg {
            Message::CompactData {
                salt,
                seq,
                samplerate,
                data,
            } => {
                let Some(i) = self
                    .sources
                    .iter()
                    .position(|s| s.addr == from && s.salt == Some(salt))
                else {
                    // robustness: a known source restarted and its /format got lost
                    for s in self.sources.iter_mut().filter(|s| s.addr == from) {
                        if t - s.last_data >= QUIET_BEFORE_FORMAT_REQUEST
                            && t - s.last_format_request >= QUIET_BEFORE_FORMAT_REQUEST
                        {
                            s.need_format = true;
                            s.last_format_request = t;
                        }
                    }
                    return false;
                };
                let pkt = Packet {
                    seq,
                    samplerate: samplerate.unwrap_or(0.0),
                    channel: 0,
                    total_size: data.len() as i32,
                    nframes: 1,
                    frame: 0,
                    data,
                };
                let p = self.p;
                self.sources[i].handle_data(&p, salt, &pkt, t);
                true
            }
            Message::SinkFormat {
                sink,
                src,
                version,
                salt,
                nchannels,
                samplerate,
                blocksize,
                codec,
                options,
                user_format,
            } => {
                if !self.for_us(sink) {
                    return false;
                }
                if !check_version(version) || src < 0 {
                    return true;
                }
                let Some(i) = self.find_or_add(from, src, Some(salt)) else {
                    return true;
                };
                let Ok(f) = Format::from_wire(codec, nchannels, samplerate, blocksize, options)
                else {
                    return true;
                };
                if !f.check_decodable() {
                    return true;
                }
                let Ok(dec) = Decoder::new(&f) else {
                    return true;
                };
                let p = self.p;
                let s = &mut self.sources[i];
                s.salt = Some(salt);
                s.protocol_flags = crate::version_flags(version) as i32;
                if let Some(u) = user_format {
                    s.user_format.clear();
                    s.user_format.extend_from_slice(u);
                }
                s.decoder = Some(dec);
                s.format = Some(f);
                s.inviting = false;
                s.update(&p);
                Self::flush_pending(&mut self.installs, s);
                let kind = SinkEventKind::FormatChanged {
                    format: f,
                    user_format: s.user_format.clone(),
                };
                self.event(from, src, kind);
                true
            }
            Message::SinkData {
                sink,
                src,
                salt,
                seq,
                samplerate,
                channel,
                total_size,
                nframes,
                frame,
                data,
            } => {
                if !self.for_us(sink) {
                    return false;
                }
                if src < 0 {
                    return true;
                }
                match self
                    .sources
                    .iter()
                    .position(|s| s.addr == from && s.id == src)
                {
                    Some(i) => {
                        let pkt = Packet {
                            seq,
                            samplerate,
                            channel,
                            total_size,
                            nframes,
                            frame,
                            data,
                        };
                        let p = self.p;
                        self.sources[i].handle_data(&p, salt, &pkt, t);
                    }
                    None => {
                        if let Some(i) = self.find_or_add(from, src, Some(salt)) {
                            self.sources[i].need_format = true;
                        }
                    }
                }
                true
            }
            Message::SinkPing { sink, src, tt } => {
                if !self.for_us(sink) {
                    return false;
                }
                if let Some(s) = self.find(from, src)
                    && s.shared.playing.load(Ordering::Acquire)
                {
                    s.ping = Some((tt, now.0));
                    let kind = SinkEventKind::Ping {
                        tt1: TimeTag(tt),
                        tt2: now,
                    };
                    self.event(from, src, kind);
                }
                true
            }
            _ => false,
        }
    }

    /// Produce pending requests, ping replies and invitations, and collect audio-side state
    /// changes into events.
    pub fn send(&mut self, now: TimeTag, mut out: impl FnMut(SocketAddr, &[u8])) -> bool {
        while self.garbage.pop().is_ok() {}
        let t = now.to_secs();
        let my_id = self.id;
        let flags = self.p.opts.protocol_flags;
        let maxrequests = ((self.p.opts.packetsize - 29 - 16) / 10).max(1) as usize;
        let mut did = false;
        for i in 0..self.sources.len() {
            Self::flush_pending(&mut self.installs, &mut self.sources[i]);
            let s = &mut self.sources[i];
            let (addr, id) = (s.addr, s.id);
            let ev = &mut self.events;
            let playing = s.shared.playing.load(Ordering::Acquire);
            if playing != s.playing {
                s.playing = playing;
                push_event(ev, addr, id, SinkEventKind::StateChanged { playing });
            }
            let counters: [(i32, Counter); 4] = [
                (std::mem::take(&mut s.lost), |count| {
                    SinkEventKind::BlocksLost { count }
                }),
                (std::mem::take(&mut s.reordered), |count| {
                    SinkEventKind::BlocksReordered { count }
                }),
                (std::mem::take(&mut s.resent), |count| {
                    SinkEventKind::BlocksResent { count }
                }),
                (std::mem::take(&mut s.gap), |count| {
                    SinkEventKind::BlocksGap { count }
                }),
            ];
            for (count, kind) in counters {
                if count > 0 {
                    push_event(ev, addr, id, kind(count));
                }
            }
            let pk = &mut self.packet;
            if std::mem::take(&mut s.need_format) {
                let msg = Message::SourceFormat {
                    src: id,
                    sink: my_id,
                    version: make_version(flags as u8),
                };
                emit(pk, &mut out, addr, &msg);
                did = true;
            }
            if let Some(f) = s.codec_change.take() {
                let (opts, len) = f.options();
                let msg = Message::SourceCodecChange {
                    src: id,
                    sink: my_id,
                    nchannels: f.nchannels,
                    samplerate: f.samplerate,
                    blocksize: f.blocksize,
                    codec: f.codec_name(),
                    options: &opts[..len],
                };
                emit(pk, &mut out, addr, &msg);
                did = true;
            }
            if let Some(salt) = s.salt {
                for chunk in s.resend.chunks(maxrequests) {
                    let msg = Message::SourceData {
                        src: id,
                        sink: my_id,
                        salt,
                        requests: Requests::Pairs(chunk),
                    };
                    emit(pk, &mut out, addr, &msg);
                    did = true;
                }
            }
            s.resend.clear();
            if let Some((tt1, tt2)) = s.ping.take()
                && playing
            {
                let msg = Message::SourcePing {
                    src: id,
                    sink: my_id,
                    tt1,
                    tt2,
                    lost_blocks: std::mem::take(&mut s.lost_since_ping),
                };
                emit(pk, &mut out, addr, &msg);
                did = true;
            }
            let invitation = std::mem::replace(&mut s.invitation, Invitation::None);
            let repeat = s.inviting && t - s.last_invite >= INVITE_REPEAT;
            if invitation == Invitation::Invite || repeat {
                s.last_invite = t;
                let msg = Message::SourceInvite {
                    src: id,
                    sink: my_id,
                    flags: Some(s.protocol_flags),
                };
                emit(pk, &mut out, addr, &msg);
                did = true;
            } else if invitation == Invitation::Uninvite {
                emit(
                    pk,
                    &mut out,
                    addr,
                    &Message::SourceUninvite {
                        src: id,
                        sink: my_id,
                    },
                );
                did = true;
            }
        }
        did
    }

    fn for_us(&self, sink: i32) -> bool {
        sink == self.id || sink == crate::ID_WILDCARD
    }

    fn event(&mut self, addr: SocketAddr, id: i32, kind: SinkEventKind) {
        push_event(&mut self.events, addr, id, kind);
    }

    fn find(&mut self, addr: SocketAddr, id: i32) -> Option<&mut SourceDesc> {
        self.sources
            .iter_mut()
            .find(|s| s.addr == addr && s.id == id)
    }

    fn find_or_add(&mut self, addr: SocketAddr, id: i32, salt: Option<i32>) -> Option<usize> {
        if let Some(i) = self
            .sources
            .iter()
            .position(|s| s.addr == addr && s.id == id)
        {
            return Some(i);
        }
        let index = self.sources.len();
        if index >= self.max_sources {
            return None;
        }
        let flags = self.p.opts.protocol_flags;
        self.sources
            .push(SourceDesc::new(addr, id, index, salt, flags));
        self.event(addr, id, SinkEventKind::SourceAdded);
        Some(index)
    }

    fn update_sources(&mut self) {
        let p = self.p;
        for s in &mut self.sources {
            s.update(&p);
            Self::flush_pending(&mut self.installs, s);
        }
    }

    fn flush_pending(installs: &mut Producer<Install>, s: &mut SourceDesc) {
        if let Some(slot) = s.pending.take()
            && let Err(rtrb::PushError::Full(Install { slot, .. })) = installs.push(Install {
                index: s.index,
                slot,
            })
        {
            s.pending = Some(slot);
        }
    }
}

#[cfg(test)]
mod tests;
