//! Audio stream source (`source.cpp`): encodes one stream and sends it to any number of sinks.
//!
//! Two handles, created together by [`Source::new`]:
//!
//! - [`SourceAudio`] lives on the audio thread. [`SourceAudio::process`] interleaves one
//!   callback (with the start/stop fades) into a lock-free SPSC ring. It never allocates, locks,
//!   blocks or logs.
//! - [`Source`] lives on the network thread. It owns everything else: sink list, encoder,
//!   resampling to the codec rate/blocksize, the time DLL, history buffer and all messages.
//!   Call [`Source::handle_message`] for every `/aoo/src/<id>/...` datagram and
//!   [`Source::send`] after each audio callback (or every few ms) to get the outgoing packets.
//!
//! Differences from the C++ worth knowing:
//! - The block that would follow sequence `i32::MAX` restarts the stream instead (new salt,
//!   sequence 0, format re-sent), which a stock sink follows; the C++ only changed the salt and
//!   overflowed the counter, which silenced `/d` streams for good.
//! - The format's channel count is always the setup channel count (the C++ would encode garbage
//!   if they differed).
//! - Resampling to the codec rate happens on the network thread, so `buffersize_ms` bounds the
//!   audio-to-network ring in device frames rather than in codec blocks.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};

use rtrb::{Consumer, Producer, RingBuffer};

use crate::codec::{Encoder, Format};
use crate::message::Message;
use crate::resampler::Resampler;
use crate::time::{Dll, TIMEFILTER_BANDWIDTH, TimeTag, Timer};
use crate::{PROTOCOL_FLAG_COMPACT_DATA, Setup, VERSION, check_version, emit};

/// `/data` header allowance: max frame payload = packet size - 80 (`source.cpp:15`).
pub const DATA_HEADER_SIZE: i32 = 80;
const MAX_EVENTS: usize = 1024;
const MAX_FORMAT_REQUESTS: usize = 64; // `source.cpp:30`
const MAX_DATA_REQUESTS: usize = 1024; // `source.cpp:31`

/// Source options; defaults are what SonoBus sets (`SonobusPluginProcessor.cpp:6030-6074`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceOptions {
    /// Audio-to-network ring in ms (at least 2 callbacks). Takes effect on [`Source::setup`].
    pub buffersize_ms: i32,
    /// Max UDP payload, 144..=4096 (SonoBus 600, library 512).
    pub packetsize: i32,
    /// Resend history in ms; 0 disables resending.
    pub resend_buffersize_ms: i32,
    /// Send every block this many times, 1..=16.
    pub redundancy: i32,
    /// Send the DLL-measured samplerate instead of the nominal one.
    pub dynamic_resampling: bool,
    /// DLL bandwidth; takes effect on the next stream update.
    pub timefilter_bandwidth: f32,
    /// Ping interval in ms of stream time; 0 = no pings (SonoBus 2000, library 1000).
    pub ping_interval_ms: i32,
    /// Honour `/codecchange` from known sinks (SonoBus: yes).
    pub respect_codec_change: bool,
}

impl Default for SourceOptions {
    fn default() -> Self {
        Self {
            buffersize_ms: 10,
            packetsize: 600,
            resend_buffersize_ms: 1000,
            redundancy: 1,
            dynamic_resampling: true,
            timefilter_bandwidth: TIMEFILTER_BANDWIDTH,
            ping_interval_ms: 2000,
            respect_codec_change: true,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SourceEvent {
    /// Sink address and id.
    pub addr: SocketAddr,
    pub id: i32,
    pub kind: SourceEventKind,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SourceEventKind {
    /// A sink not in our list invited us; `flags` are its protocol flags (pass them to
    /// [`Source::add_sink`]).
    Invite { flags: i32 },
    /// A listed sink asked us to stop sending to it.
    Uninvite,
    /// A listed sink asked for the format (handled; for information).
    FormatRequested,
    /// A listed sink's `/codecchange` was applied; the new format is being announced.
    CodecChanged { format: Format },
    /// Ping reply: `tt1` our ping time, `tt2` the sink's receive time, `tt3` our receive time,
    /// `rtt = tt3 - tt1` in seconds, `lost` blocks the sink lost since its last reply.
    Ping {
        tt1: TimeTag,
        tt2: TimeTag,
        tt3: TimeTag,
        rtt: f64,
        lost: i32,
    },
}

/// State shared between the two halves; atomics only.
#[derive(Debug, Default)]
struct Shared {
    play: AtomicBool,
    /// Set by the audio half when the stop fade and trailing silence are out.
    flushed: AtomicBool,
    /// Frames of silence to push after the stop fade (`4 * codec blocksize`).
    silence_frames: AtomicI32,
    /// Fade in again on the next callback (set by every stream update).
    refade: AtomicBool,
}

#[derive(Debug, Clone, Copy, Default)]
struct Record {
    t: TimeTag,
    frames: u32,
}

/// Audio-thread half of a source.
pub struct SourceAudio {
    shared: Arc<Shared>,
    samples: Producer<f32>,
    records: Producer<Record>,
    nch: usize,
    last_play: bool,
    stopping: bool,
    silence_left: i32,
}

impl SourceAudio {
    /// Consume one callback: `input` holds one slice per channel (missing channels are
    /// silence), all of the same length. `now` is the callback's wall-clock time. Returns
    /// whether anything was queued. Real-time safe.
    pub fn process(&mut self, input: &[&[f32]], now: TimeTag) -> bool {
        let play = self.shared.play.load(Ordering::Acquire);
        if self.shared.refade.swap(false, Ordering::AcqRel) {
            self.last_play = false;
        }
        let n = input.first().map_or(0, |c| c.len());
        let fade_in = play && !self.last_play;
        let fade_out = !play && self.last_play;
        if fade_out {
            self.silence_left = self.shared.silence_frames.load(Ordering::Relaxed);
            self.stopping = true;
        }
        self.last_play = play;
        let silence = !play && !fade_out && self.silence_left > 0;
        if !play && !fade_out && !silence {
            if self.stopping {
                self.stopping = false;
                self.shared.flushed.store(true, Ordering::Release);
            }
            return false;
        }
        let (mut gain, delta) = if fade_out {
            (1.0f32, -1.0 / n as f32)
        } else if silence {
            (0.0, 0.0)
        } else if fade_in {
            (0.0, 1.0 / n as f32)
        } else {
            (1.0, 0.0)
        };
        let scaled = fade_in || fade_out || silence;
        if self.silence_left > 0 {
            self.silence_left -= n as i32;
        }
        let nch = self.nch;
        let total = n * nch;
        if self.records.is_full() || self.samples.slots() < total {
            return false; // network thread too slow: drop this callback
        }
        let Ok(chunk) = self.samples.write_chunk_uninit(total) else {
            return false;
        };
        let samples = (0..n).flat_map(|j| {
            let g = gain;
            gain += delta;
            (0..nch).map(move |i| {
                let x = input.get(i).and_then(|c| c.get(j)).copied().unwrap_or(0.0);
                if scaled { x * g } else { x }
            })
        });
        chunk.fill_from_iter(samples);
        let _ = self.records.push(Record {
            t: now,
            frames: n as u32,
        });
        true
    }
}

#[derive(Debug)]
struct SinkDesc {
    addr: SocketAddr,
    id: i32,
    format_changed: bool,
    flags: i32,
}

#[derive(Debug, Clone, Copy)]
struct DataRequest {
    addr: SocketAddr,
    id: i32,
    salt: i32,
    seq: i32,
    frame: i32,
}

#[derive(Debug, Default)]
struct HistoryBlock {
    seq: Option<i32>,
    samplerate: f64,
    data: Vec<u8>,
    nframes: i32,
    framesize: i32,
}

impl HistoryBlock {
    fn frame(&self, i: i32) -> &[u8] {
        let onset = (i * self.framesize) as usize;
        if i == self.nframes - 1 {
            &self.data[onset..]
        } else {
            &self.data[onset..onset + self.framesize as usize]
        }
    }
}

/// Network-thread half of a source.
pub struct Source {
    id: i32,
    /// [`Self::set_max_sinks`].
    max_sinks: usize,
    setup: Setup,
    opts: SourceOptions,
    shared: Arc<Shared>,
    samples: Consumer<f32>,
    records: Consumer<Record>,
    format: Option<Format>,
    encoder: Option<Encoder>,
    user_format: Vec<u8>,
    sinks: Vec<SinkDesc>,
    format_changed: bool,
    play: bool,
    active: bool,
    salt: i32,
    sequence: i32,
    prev_sent_samplerate: f64,
    timer: Timer,
    dll: Dll,
    resampler: Resampler,
    last_ping: f32,
    format_requests: VecDeque<(SocketAddr, i32)>,
    data_requests: VecDeque<DataRequest>,
    history: Vec<HistoryBlock>,
    history_head: usize,
    events: VecDeque<SourceEvent>,
    rng: u64,
    input: Vec<f32>,
    block: Vec<f32>,
    encoded: Vec<u8>,
    packet: Vec<u8>,
}

fn rings(
    setup: &Setup,
    buffersize_ms: i32,
) -> (
    Producer<f32>,
    Consumer<f32>,
    Producer<Record>,
    Consumer<Record>,
) {
    let bs = setup.blocksize.max(1) as usize;
    let frames = (buffersize_ms.max(0) as f64 * setup.samplerate as f64 * 0.001) as usize;
    let frames = frames.max(2 * bs);
    let (sp, sc) = RingBuffer::new(frames * setup.nchannels.max(1) as usize);
    // a record per callback, callbacks may be shorter than `blocksize`
    let (rp, rc) = RingBuffer::new(frames.max(256));
    (sp, sc, rp, rc)
}

/// [`Source::set_max_sinks`] default.
pub const DEFAULT_MAX_SINKS: usize = 64;

impl Source {
    /// A stopped source without format. `setup` values must be positive.
    pub fn new(id: i32, setup: Setup, opts: SourceOptions) -> (Self, SourceAudio) {
        let shared = Arc::new(Shared::default());
        let (sp, sc, rp, rc) = rings(&setup, opts.buffersize_ms);
        let src = Self {
            id,
            max_sinks: DEFAULT_MAX_SINKS,
            setup,
            opts,
            shared: shared.clone(),
            samples: sc,
            records: rc,
            format: None,
            encoder: None,
            user_format: Vec::new(),
            sinks: Vec::new(),
            format_changed: false,
            play: false,
            active: false,
            salt: 0,
            sequence: 0,
            prev_sent_samplerate: 0.0,
            timer: Timer::default(),
            dll: Dll::default(),
            resampler: Resampler::default(),
            last_ping: 0.0,
            format_requests: VecDeque::new(),
            data_requests: VecDeque::new(),
            history: Vec::new(),
            history_head: 0,
            events: VecDeque::with_capacity(64),
            rng: crate::client::random_seed() | 1,
            input: Vec::new(),
            block: Vec::new(),
            encoded: Vec::new(),
            packet: Vec::with_capacity(crate::MAX_PACKET_SIZE),
        };
        let audio = SourceAudio {
            shared,
            samples: sp,
            records: rp,
            nch: setup.nchannels.max(1) as usize,
            last_play: false,
            stopping: false,
            silence_left: 0,
        };
        (src, audio)
    }

    pub fn id(&self) -> i32 {
        self.id
    }

    /// New device setup. Returns the audio half to install on the audio thread; the previous
    /// one stops feeding this source. Restarts the stream.
    pub fn setup(&mut self, setup: Setup) -> SourceAudio {
        let (sp, sc, rp, rc) = rings(&setup, self.opts.buffersize_ms);
        self.setup = setup;
        self.samples = sc;
        self.records = rc;
        if let Some(f) = self.format {
            let _ = self.set_format(f);
        }
        SourceAudio {
            shared: self.shared.clone(),
            samples: sp,
            records: rp,
            nch: setup.nchannels.max(1) as usize,
            last_play: false,
            stopping: false,
            silence_left: 0,
        }
    }

    pub fn options(&self) -> SourceOptions {
        self.opts
    }

    pub fn set_options(&mut self, opts: SourceOptions) {
        let resend_changed = opts.resend_buffersize_ms != self.opts.resend_buffersize_ms;
        self.opts = SourceOptions {
            packetsize: opts
                .packetsize
                .clamp(DATA_HEADER_SIZE + 64, crate::MAX_PACKET_SIZE as i32),
            redundancy: opts.redundancy.clamp(1, 16),
            ..opts
        };
        if resend_changed {
            self.resize_history();
        }
    }

    /// Set the stream format (validated like the C++ encoders; the channel count is forced to
    /// the setup's). Restarts the stream. Returns the effective format.
    pub fn set_format(&mut self, f: Format) -> Result<Format, crate::Error> {
        let f = Format {
            nchannels: self.setup.nchannels,
            ..f
        }
        .validated();
        let (enc, f) = Encoder::new(&f)?;
        self.encoder = Some(enc);
        self.format = Some(f);
        self.update();
        Ok(f)
    }

    pub fn format(&self) -> Option<&Format> {
        self.format.as_ref()
    }

    /// Opaque blob appended to `/format` (SonoBus: channel layout). Restarts the stream.
    pub fn set_user_format(&mut self, data: &[u8]) {
        self.user_format.clear();
        self.user_format.extend_from_slice(data);
        self.update();
    }

    /// Start streaming: new salt, sequence 0, format to every sink, fade in.
    pub fn start(&mut self) {
        self.update();
        self.shared.flushed.store(false, Ordering::Relaxed);
        self.shared.play.store(true, Ordering::Release);
        self.play = true;
        self.active = true;
    }

    /// Fade out, send a few blocks of silence, then go quiet (no messages at all).
    pub fn stop(&mut self) {
        self.shared.play.store(false, Ordering::Release);
        self.play = false;
    }

    pub fn is_playing(&self) -> bool {
        self.play
    }

    /// Add a sink; `flags` are its protocol flags (from the invite or 0). The format goes out
    /// with the next [`Self::send`]. `false` if already listed.
    pub fn add_sink(&mut self, addr: SocketAddr, id: i32, flags: i32) -> bool {
        if self.find_sink(addr, id).is_some() || self.sinks.len() >= self.max_sinks {
            return false;
        }
        self.sinks.push(SinkDesc {
            addr,
            id,
            format_changed: true,
            flags,
        });
        self.format_changed = true;
        true
    }

    pub fn remove_sink(&mut self, addr: SocketAddr, id: i32) -> bool {
        let n = self.sinks.len();
        self.sinks.retain(|s| !(s.addr == addr && s.id == id));
        n != self.sinks.len()
    }

    pub fn remove_all(&mut self) {
        self.sinks.clear();
    }

    /// Not in the C++: most sinks this source sends to (default [`DEFAULT_MAX_SINKS`], at least
    /// 1); [`Self::add_sink`] refuses more. Every sink costs a copy of the upload, so a source
    /// that adds whoever invites it should be capped. Already listed sinks stay.
    pub fn set_max_sinks(&mut self, n: usize) {
        self.max_sinks = n.max(1);
    }

    pub fn sink_count(&self) -> usize {
        self.sinks.len()
    }

    pub fn set_sink_protocol_flags(&mut self, addr: SocketAddr, id: i32, flags: i32) -> bool {
        self.find_sink(addr, id).map(|s| s.flags = flags).is_some()
    }

    /// Move a sink to a new address (peer NAT rebinding).
    pub fn repoint_sink(&mut self, addr: SocketAddr, id: i32, new_addr: SocketAddr) -> bool {
        self.find_sink(addr, id)
            .map(|s| s.addr = new_addr)
            .is_some()
    }

    pub fn poll_event(&mut self) -> Option<SourceEvent> {
        self.events.pop_front()
    }

    /// Handle a datagram addressed to `/aoo/src/<id>/...`. Returns `false` if it is not for
    /// this source. `now` stamps ping replies.
    pub fn handle_message(&mut self, from: SocketAddr, msg: &Message, now: TimeTag) -> bool {
        use Message as M;
        let src = match *msg {
            M::SourceFormat { src, .. }
            | M::SourceData { src, .. }
            | M::SourcePing { src, .. }
            | M::SourceInvite { src, .. }
            | M::SourceUninvite { src, .. }
            | M::SourceCodecChange { src, .. } => src,
            _ => return false,
        };
        if src != self.id {
            return false;
        }
        match *msg {
            M::SourceFormat { sink, version, .. } => {
                if !check_version(version) {
                    return true;
                }
                if let Some(s) = self.find_sink(from, sink) {
                    s.flags = crate::version_flags(version) as i32;
                    if self.format_requests.len() < MAX_FORMAT_REQUESTS {
                        self.format_requests.push_back((from, sink));
                    }
                    self.event(from, sink, SourceEventKind::FormatRequested);
                }
            }
            M::SourceData {
                sink,
                salt,
                requests,
                ..
            } => {
                if self.find_sink(from, sink).is_some() {
                    for (seq, frame) in requests.iter() {
                        if self.data_requests.len() < MAX_DATA_REQUESTS {
                            self.data_requests.push_back(DataRequest {
                                addr: from,
                                id: sink,
                                salt,
                                seq,
                                frame,
                            });
                        }
                    }
                }
            }
            M::SourceInvite { sink, flags, .. } => {
                if self.find_sink(from, sink).is_none() {
                    let flags = flags.unwrap_or(0);
                    self.event(from, sink, SourceEventKind::Invite { flags });
                }
            }
            M::SourceUninvite { sink, .. } => {
                if self.find_sink(from, sink).is_some() {
                    self.event(from, sink, SourceEventKind::Uninvite);
                }
            }
            M::SourcePing {
                sink,
                tt1,
                tt2,
                lost_blocks,
                ..
            } => {
                if self.find_sink(from, sink).is_some() {
                    let (tt1, tt3) = (TimeTag(tt1), now);
                    self.event(
                        from,
                        sink,
                        SourceEventKind::Ping {
                            tt1,
                            tt2: TimeTag(tt2),
                            tt3,
                            rtt: TimeTag::duration(tt1, tt3),
                            lost: lost_blocks,
                        },
                    );
                }
            }
            M::SourceCodecChange {
                sink,
                blocksize,
                codec,
                options,
                ..
            } => {
                if !self.opts.respect_codec_change || sink < 0 {
                    return true;
                }
                if self.find_sink(from, sink).is_none() {
                    return true;
                }
                // keep our channel count and samplerate (`source.cpp:1473-1481`)
                let Ok(f) = Format::from_wire(
                    codec,
                    self.setup.nchannels,
                    self.setup.samplerate,
                    blocksize,
                    options,
                ) else {
                    return true;
                };
                if let Ok(format) = self.set_format(f) {
                    self.event(from, sink, SourceEventKind::CodecChanged { format });
                }
            }
            _ => unreachable!(),
        }
        true
    }

    /// Produce all pending packets. Sends nothing while stopped (after the stop fade is out).
    pub fn send(&mut self, mut out: impl FnMut(SocketAddr, &[u8])) -> bool {
        if !self.play && !self.active {
            return false;
        }
        let mut did = self.send_format(&mut out);
        did |= self.send_data(&mut out);
        did |= self.resend_data(&mut out);
        did |= self.send_ping(&mut out);
        if !self.play && self.shared.flushed.load(Ordering::Acquire) && self.records.is_empty() {
            self.active = false;
        }
        did
    }

    fn event(&mut self, addr: SocketAddr, id: i32, kind: SourceEventKind) {
        if self.events.len() < MAX_EVENTS {
            self.events.push_back(SourceEvent { addr, id, kind });
        }
    }

    fn find_sink(&mut self, addr: SocketAddr, id: i32) -> Option<&mut SinkDesc> {
        self.sinks.iter_mut().find(|s| s.addr == addr && s.id == id)
    }

    fn make_salt(&mut self) -> i32 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 33) as i32
    }

    /// `source::update`: reset buffers, encoder and timing, then restart the stream.
    fn update(&mut self) {
        let Some(f) = self.format else { return };
        let nch = self.setup.nchannels as usize;
        let bs = f.blocksize as usize;
        self.resampler.setup(self.setup.blocksize as usize, bs, nch);
        self.resampler
            .update(self.setup.samplerate as f64, f.samplerate as f64);
        self.block.resize(bs * nch, 0.0);
        self.encoded.resize(Encoder::max_size(&f), 0);
        self.resize_history();
        if let Some(e) = &mut self.encoder {
            e.reset();
        }
        self.timer.reset();
        self.last_ping = -1000.0;
        // drop queued audio, keeping samples and records in step
        while let Ok(r) = self.records.pop() {
            let n = r.frames as usize * nch;
            if let Ok(c) = self.samples.read_chunk(n.min(self.samples.slots())) {
                c.commit_all();
            }
        }
        self.shared
            .silence_frames
            .store(4 * f.blocksize, Ordering::Relaxed);
        self.shared.refade.store(true, Ordering::Release);
        self.restart();
    }

    /// New salt and sequence; every sink gets the format again.
    fn restart(&mut self) {
        self.salt = self.make_salt();
        self.sequence = 0;
        for h in &mut self.history {
            h.seq = None;
        }
        for s in &mut self.sinks {
            s.format_changed = true;
        }
        self.format_changed = true;
    }

    fn resize_history(&mut self) {
        let Some(f) = self.format else { return };
        // `source.cpp:983-990`: device samplerate over codec blocksize, rounded up
        let frames = (self.opts.resend_buffersize_ms.max(0) as f64
            * 0.001
            * self.setup.samplerate as f64) as i32;
        let n = (frames + f.blocksize - 1) / f.blocksize;
        self.history.clear();
        self.history.resize_with(n as usize, HistoryBlock::default);
        self.history_head = 0;
    }

    fn send_format(&mut self, out: &mut impl FnMut(SocketAddr, &[u8])) -> bool {
        let changed = std::mem::take(&mut self.format_changed);
        if !changed && self.format_requests.is_empty() {
            return false;
        }
        let Some(f) = self.format else { return false };
        let (opts, len) = f.options();
        let msg = |sink| Message::SinkFormat {
            sink,
            src: self.id,
            version: VERSION,
            salt: self.salt,
            nchannels: f.nchannels,
            samplerate: f.samplerate,
            blocksize: f.blocksize,
            codec: f.codec_name(),
            options: &opts[..len],
            user_format: Some(&self.user_format),
        };
        if changed {
            for s in self.sinks.iter_mut().filter(|s| s.format_changed) {
                s.format_changed = false;
                emit(&mut self.packet, out, s.addr, &msg(s.id));
            }
        }
        while let Some((addr, id)) = self.format_requests.pop_front() {
            emit(&mut self.packet, out, addr, &msg(id));
        }
        true
    }

    /// Drain the audio ring: DLL, resampling to codec blocks, encode, send.
    fn send_data(&mut self, out: &mut impl FnMut(SocketAddr, &[u8])) -> bool {
        let nch = self.setup.nchannels as usize;
        let sr = self.setup.samplerate as f64;
        let mut did = false;
        while let Ok(rec) = self.records.pop() {
            let n = rec.frames as usize * nch;
            self.input.resize(n, 0.0);
            if self.samples.pop_entire_slice(&mut self.input).is_err() {
                break; // cannot happen: samples are pushed before their record
            }
            if self.timer.update(rec.t) {
                self.dll.setup(
                    sr,
                    self.setup.blocksize,
                    self.opts.timefilter_bandwidth as f64,
                    0.0,
                );
            } else {
                self.dll.update(self.timer.elapsed());
            }
            let ignore_dll =
                !self.opts.dynamic_resampling || (self.dll.samplerate() - sr).abs() > 0.1 * sr;
            let Some(f) = self.format else { continue };
            let block_sr = if ignore_dll {
                f.samplerate as f64
            } else {
                self.dll.samplerate() * (f.samplerate as f64 / sr)
            };
            // `source.cpp:641-683`
            let mut pos = 0;
            let mut avail = self.resampler.write_available();
            while pos < n {
                let take = (n - pos).min(avail);
                self.resampler.write(&self.input[pos..pos + take]);
                pos += take;
                let mut consumed = false;
                while self.resampler.read_available() >= self.block.len() {
                    self.resampler.read(&mut self.block);
                    self.send_block(block_sr, out);
                    consumed = true;
                    did = true;
                }
                avail = self.resampler.write_available();
                if !consumed && n - pos > avail {
                    break;
                }
            }
        }
        did
    }

    /// Encode `self.block` and send it to every sink (`source.cpp:1174-1270`).
    fn send_block(&mut self, samplerate: f64, out: &mut impl FnMut(SocketAddr, &[u8])) {
        let seq = self.sequence;
        self.sequence = seq.wrapping_add(1);
        let sendrate = (samplerate - self.prev_sent_samplerate).abs() > 0.1;
        if sendrate {
            self.prev_sent_samplerate = samplerate;
        }
        if !self.sinks.is_empty() {
            if self.format_changed {
                self.send_format(out);
            }
            let Some(enc) = &mut self.encoder else { return };
            let total = enc.encode(&self.block, &mut self.encoded);
            if total > 0 {
                let maxframe = (self.opts.packetsize - DATA_HEADER_SIZE) as usize;
                let nframes = total.div_ceil(maxframe) as i32;
                if !self.history.is_empty() {
                    let h = &mut self.history[self.history_head];
                    h.seq = Some(seq);
                    h.samplerate = samplerate;
                    h.data.clear();
                    h.data.extend_from_slice(&self.encoded[..total]);
                    h.nframes = nframes;
                    h.framesize = maxframe as i32;
                    self.history_head = (self.history_head + 1) % self.history.len();
                }
                for _ in 0..self.opts.redundancy {
                    for (frame, data) in self.encoded[..total].chunks(maxframe).enumerate() {
                        for s in &self.sinks {
                            let msg = if nframes == 1
                                && s.flags & PROTOCOL_FLAG_COMPACT_DATA as i32 != 0
                            {
                                Message::CompactData {
                                    salt: self.salt,
                                    seq,
                                    samplerate: sendrate.then_some(samplerate),
                                    data,
                                }
                            } else {
                                Message::SinkData {
                                    sink: s.id,
                                    src: self.id,
                                    salt: self.salt,
                                    seq,
                                    samplerate,
                                    channel: 0,
                                    total_size: total as i32,
                                    nframes,
                                    frame: frame as i32,
                                    data,
                                }
                            };
                            emit(&mut self.packet, out, s.addr, &msg);
                        }
                    }
                }
            }
        }
        if seq == i32::MAX {
            self.restart();
        }
    }

    fn resend_data(&mut self, out: &mut impl FnMut(SocketAddr, &[u8])) -> bool {
        let mut did = false;
        while let Some(r) = self.data_requests.pop_front() {
            if r.salt != self.salt {
                continue; // outdated
            }
            let Some(b) = self.history.iter().find(|h| h.seq == Some(r.seq)) else {
                continue;
            };
            let frames = if r.frame < 0 {
                0..b.nframes
            } else if r.frame < b.nframes {
                r.frame..r.frame + 1
            } else {
                continue;
            };
            for frame in frames {
                let msg = Message::SinkData {
                    sink: r.id,
                    src: self.id,
                    salt: self.salt,
                    seq: r.seq,
                    samplerate: b.samplerate,
                    channel: 0,
                    total_size: b.data.len() as i32,
                    nframes: b.nframes,
                    frame,
                    data: b.frame(frame),
                };
                emit(&mut self.packet, out, r.addr, &msg);
            }
            did = true;
        }
        did
    }

    fn send_ping(&mut self, out: &mut impl FnMut(SocketAddr, &[u8])) -> bool {
        // the C++ keeps interval and last ping time in floats
        let elapsed = self.timer.elapsed();
        let interval = (self.opts.ping_interval_ms.max(0) as f64 * 0.001) as f32;
        if interval <= 0.0 || elapsed - (self.last_ping as f64) < interval as f64 {
            return false;
        }
        let tt = self.timer.absolute().0;
        for s in &self.sinks {
            let msg = Message::SinkPing {
                sink: s.id,
                src: self.id,
                tt,
            };
            emit(&mut self.packet, out, s.addr, &msg);
        }
        self.last_ping = elapsed as f32;
        true
    }

    /// Test hook: jump the block counter (e.g. to just below `i32::MAX`).
    #[doc(hidden)]
    pub fn set_sequence_for_testing(&mut self, seq: i32) {
        self.sequence = seq;
    }

    #[cfg(test)]
    pub(crate) fn salt(&self) -> i32 {
        self.salt
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{Codec, PcmBitDepth};
    use crate::message::{Requests, parse};

    fn addr(p: u16) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], p))
    }

    fn pcm_source(depth: PcmBitDepth, bs: i32) -> (Source, SourceAudio) {
        let setup = Setup {
            samplerate: 48000,
            blocksize: bs,
            nchannels: 2,
        };
        let (mut s, a) = Source::new(1, setup, SourceOptions::default());
        s.set_format(Format {
            nchannels: 2,
            samplerate: 48000,
            blocksize: bs,
            codec: Codec::Pcm(depth),
        })
        .unwrap();
        (s, a)
    }

    fn collect(s: &mut Source) -> Vec<(SocketAddr, Vec<u8>)> {
        let mut v = Vec::new();
        s.send(|a, b| v.push((a, b.to_vec())));
        v
    }

    #[test]
    fn sink_cap() {
        let (mut s, _a) = pcm_source(PcmBitDepth::Int16, 64);
        for i in 0..DEFAULT_MAX_SINKS as i32 + 5 {
            s.add_sink(addr(1), i, 0);
        }
        assert_eq!(s.sink_count(), DEFAULT_MAX_SINKS);
        s.remove_all();
        s.set_max_sinks(0);
        assert!(s.add_sink(addr(1), 1, 0));
        assert!(!s.add_sink(addr(1), 2, 0));
        assert_eq!(s.sink_count(), 1);
    }

    #[test]
    fn stopped_source_is_silent() {
        let (mut s, mut a) = pcm_source(PcmBitDepth::Int16, 64);
        s.add_sink(addr(1), 7, 1);
        let buf = [0.1f32; 64];
        assert!(!a.process(&[&buf, &buf], TimeTag::from_secs(1.0)));
        assert!(collect(&mut s).is_empty());
        // a format request is queued but not answered while stopped
        s.handle_message(
            addr(1),
            &Message::SourceFormat {
                src: 1,
                sink: 7,
                version: VERSION,
            },
            TimeTag(1),
        );
        assert!(collect(&mut s).is_empty());
    }

    /// Large float32 blocks are split into packetsize - 80 byte frames; sink flags decide /d.
    #[test]
    fn fragmentation_and_compact() {
        let (mut s, mut a) = pcm_source(PcmBitDepth::Float32, 256); // 2048 bytes per block
        s.add_sink(addr(1), 7, 0);
        s.add_sink(addr(2), 8, 1);
        s.start();
        let buf = [0.25f32; 256];
        assert!(a.process(&[&buf, &buf], TimeTag::from_secs(1.0)));
        let pkts = collect(&mut s);
        let mut frames = Vec::new();
        let mut formats = 0;
        for (to, p) in &pkts {
            assert!(p.len() <= 600, "{}", p.len());
            match parse(p).unwrap() {
                Message::SinkFormat { .. } => formats += 1,
                Message::SinkData {
                    total_size,
                    nframes,
                    frame,
                    data,
                    ..
                } => {
                    assert_eq!((total_size, nframes), (2048, 4));
                    frames.push((*to, frame, data.len()));
                }
                Message::SinkPing { .. } => {}
                m => panic!("{m:?}"),
            }
        }
        assert_eq!(formats, 2);
        // both sinks get /data because the block needs 4 frames
        assert_eq!(frames.len(), 8);
        assert!(frames.contains(&(addr(1), 3, 2048 - 3 * 520)));
        // small blocks go out as /d to the compact sink only
        let (mut s, mut a) = pcm_source(PcmBitDepth::Int16, 64);
        s.add_sink(addr(1), 7, 0);
        s.add_sink(addr(2), 8, 1);
        s.start();
        a.process(&[&buf[..64], &buf[..64]], TimeTag::from_secs(1.0));
        let kinds: Vec<_> = collect(&mut s)
            .iter()
            .map(|(to, p)| (*to, std::str::from_utf8(&p[..2]).unwrap().to_owned()))
            .collect();
        assert!(kinds.contains(&(addr(2), "/d".into())));
        assert_eq!(
            kinds
                .iter()
                .filter(|(a, k)| *a == addr(1) && k == "/d")
                .count(),
            0
        );
    }

    #[test]
    fn resend_from_history() {
        let (mut s, mut a) = pcm_source(PcmBitDepth::Float32, 256);
        s.add_sink(addr(1), 7, 1);
        s.start();
        let buf = [0.5f32; 256];
        for k in 0..3 {
            a.process(&[&buf, &buf], TimeTag::from_secs(1.0 + k as f64 * 0.01));
            collect(&mut s); // the 10 ms ring holds two callbacks
        }
        let salt = s.salt();
        let req = |salt, pairs: &'static [(i32, i32)]| Message::SourceData {
            src: 1,
            sink: 7,
            salt,
            requests: Requests::Pairs(pairs),
        };
        s.handle_message(
            addr(1),
            &req(salt, &[(1, -1), (2, 2), (2, 9), (99, -1)]),
            TimeTag(1),
        );
        let got: Vec<_> = collect(&mut s)
            .into_iter()
            .filter_map(|(_, p)| match parse(&p).unwrap() {
                Message::SinkData { seq, frame, .. } => Some((seq, frame)),
                _ => None,
            })
            .collect();
        assert_eq!(got, [(1, 0), (1, 1), (1, 2), (1, 3), (2, 2)]);
        // stale salt is ignored, unknown sink too
        s.handle_message(addr(1), &req(salt ^ 1, &[(1, -1)]), TimeTag(1));
        s.handle_message(addr(9), &req(salt, &[(1, -1)]), TimeTag(1));
        assert!(
            collect(&mut s)
                .iter()
                .all(|(_, p)| !p.starts_with(b"/aoo/sink/7/data"))
        );
    }

    /// The block after `i32::MAX` restarts the stream with a new salt and a fresh format.
    #[test]
    fn sequence_wrap_restarts() {
        let (mut s, mut a) = pcm_source(PcmBitDepth::Int16, 64);
        s.add_sink(addr(1), 7, 1);
        s.start();
        let buf = [0.5f32; 64];
        a.process(&[&buf, &buf], TimeTag::from_secs(1.0));
        collect(&mut s);
        let old = s.salt();
        s.set_sequence_for_testing(i32::MAX);
        a.process(&[&buf, &buf], TimeTag::from_secs(1.01));
        a.process(&[&buf, &buf], TimeTag::from_secs(1.02));
        let msgs: Vec<_> = collect(&mut s).into_iter().map(|(_, p)| p).collect();
        let parsed: Vec<_> = msgs.iter().map(|p| parse(p).unwrap()).collect();
        assert!(
            matches!(parsed[0], Message::CompactData { seq: i32::MAX, salt, .. } if salt == old)
        );
        assert!(matches!(parsed[1], Message::SinkFormat { salt, .. } if salt != old));
        assert!(matches!(parsed[2], Message::CompactData { seq: 0, salt, .. } if salt != old));
    }

    #[test]
    fn invite_uninvite_ping_codecchange() {
        let (mut s, _a) = pcm_source(PcmBitDepth::Int16, 64);
        let t = TimeTag::from_secs(100.0);
        s.handle_message(
            addr(1),
            &Message::SourceInvite {
                src: 1,
                sink: 7,
                flags: Some(1),
            },
            t,
        );
        s.handle_message(
            addr(1),
            &Message::SourceInvite {
                src: 2,
                sink: 7,
                flags: Some(1),
            },
            t,
        );
        let e = s.poll_event().unwrap();
        assert_eq!(
            (e.addr, e.id, e.kind),
            (addr(1), 7, SourceEventKind::Invite { flags: 1 })
        );
        assert!(s.poll_event().is_none());
        assert!(s.add_sink(addr(1), 7, 1));
        assert!(!s.add_sink(addr(1), 7, 1));
        // invites from listed sinks are ignored
        s.handle_message(
            addr(1),
            &Message::SourceInvite {
                src: 1,
                sink: 7,
                flags: None,
            },
            t,
        );
        assert!(s.poll_event().is_none());
        s.handle_message(
            addr(1),
            &Message::SourcePing {
                src: 1,
                sink: 7,
                tt1: TimeTag::from_secs(99.5).0,
                tt2: 5,
                lost_blocks: 3,
            },
            t,
        );
        match s.poll_event().unwrap().kind {
            SourceEventKind::Ping { rtt, lost, .. } => assert_eq!((rtt, lost), (0.5, 3)),
            k => panic!("{k:?}"),
        }
        let opus = crate::codec::OpusOptions {
            bitrate: 64000,
            complexity: 5,
            signal_type: crate::codec::OPUS_SIGNAL_MUSIC,
            application_type: crate::codec::OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        };
        s.handle_message(
            addr(1),
            &Message::SourceCodecChange {
                src: 1,
                sink: 7,
                nchannels: 1,
                samplerate: 24000,
                blocksize: 500,
                codec: "opus",
                options: &opus.to_bytes(),
            },
            t,
        );
        match s.poll_event().unwrap().kind {
            SourceEventKind::CodecChanged { format } => {
                // channels and samplerate stay ours, blocksize is validated
                assert_eq!(
                    (format.nchannels, format.samplerate, format.blocksize),
                    (2, 48000, 480)
                );
            }
            k => panic!("{k:?}"),
        }
        s.handle_message(addr(1), &Message::SourceUninvite { src: 1, sink: 7 }, t);
        assert_eq!(s.poll_event().unwrap().kind, SourceEventKind::Uninvite);
        assert!(s.remove_sink(addr(1), 7));
    }

    /// stop: one faded callback, then silence, then nothing; start again fades in.
    #[test]
    fn stop_fade_and_flush() {
        let (mut s, mut a) = pcm_source(PcmBitDepth::Float32, 64);
        s.add_sink(addr(1), 7, 1);
        s.start();
        let buf = [1.0f32; 64];
        let block = |a: &mut SourceAudio, s: &mut Source, k: u32| {
            a.process(&[&buf, &buf], TimeTag::from_secs(1.0 + k as f64 / 750.0));
            collect(s)
                .into_iter()
                .filter_map(|(_, p)| match parse(&p).unwrap() {
                    Message::CompactData { data, .. } => {
                        Some(f32::from_be_bytes(data[..4].try_into().unwrap()).max(
                            f32::from_be_bytes(data[data.len() - 4..].try_into().unwrap()),
                        ))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(block(&mut a, &mut s, 0), [1.0 - 1.0 / 64.0]); // fade-in ends below 1
        assert_eq!(block(&mut a, &mut s, 1), [1.0]);
        s.stop();
        assert_eq!(block(&mut a, &mut s, 2), [1.0]); // fade-out starts at 1
        let mut silent = 0;
        for k in 3..20 {
            let v = block(&mut a, &mut s, k);
            silent += v.iter().filter(|&&x| x == 0.0).count();
        }
        assert_eq!(silent, 3); // 4 * blocksize minus the fade-out callback
        assert!(!s.active);
        s.start();
        assert_eq!(block(&mut a, &mut s, 30), [1.0 - 1.0 / 64.0]);
    }
}
