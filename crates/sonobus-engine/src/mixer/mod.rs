//! The audio-thread signal flow: upstream `processBlock` (P.cpp:7330-8407) steps 1-8, 11-16
//! and 18, independent of the network protocol. The engine glue feeds each remote peer's
//! received audio into [`Mixer::peer_buffer`] and takes [`Mixer::send_mix`] for every peer.
//!
//! Stage 2: file playback (step 9) and soundboard (step 10) go into the send mix after the
//! metronome's channel and into the output after the input reverb; the recording taps (step
//! 17) write the pre/post-FX input, each peer's raw stream, the mix minus our input and the
//! full mix; [`Mixer::peer_send`] adds patchbay cross-routes to a peer's send. The latency
//! test/echo streams run next to the per-peer send (`legacy::Links`).
//!
//! Nothing here allocates, locks or blocks after construction: parameter snapshots, peers and
//! monitor-delay lines arrive as [`Cmd`]s and the replaced boxes leave through `drop_later`.

mod group;
pub mod meter;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use sonobus_core::state::{ChannelGroup, Params};
use sonobus_dsp::gain::{
    add_from, add_from_with_ramp, apply_gain, apply_gain_ramp, approximately_equal,
};
use sonobus_dsp::{InputReverb, MainReverb, Metronome, MonitorDelay, ReverbModel, ReverbParams};
use sonobus_files::{PlayerOutput, RecordWriter};

use group::{GroupDsp, idx};
use meter::MeterState;
pub use meter::{Meter, MeterLevels};

/// Channels of any internal bus (`MAX_PANNERS`).
pub const MAX_CHANNELS: usize = 64;
/// `MAX_PEERS` (P.h:27).
pub const MAX_PEERS: usize = 32;
/// `MAX_CHANGROUPS`.
pub const MAX_GROUPS: usize = 64;
/// Soundboard samples playing at once.
pub const MAX_VOICES: usize = 16;
/// The soundboard bus is stereo (`getFileSourceNumberOfChannels`, SoundboardChannelProcessor).
const SB_CH: usize = 2;

/// Boxes the audio thread hands back for dropping elsewhere.
pub type Garbage = Box<dyn Send>;

/// The plugin host's transport at the start of a block (upstream reads the play head every
/// `processBlock`, P.cpp:7405-7420). With `syncMetHost` the metronome takes its tempo from
/// `bpm` and, while `playing`, its beat position from `ppq`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct HostTransport {
    /// Host tempo; ignored unless > 0.
    pub bpm: f64,
    /// Position in quarter notes, if the host gives one.
    pub ppq: Option<f64>,
    pub playing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    /// Block size the engine processes in (`EngineAudio::process` takes any callback size).
    pub block_size: usize,
    pub input_channels: usize,
    pub output_channels: usize,
}

impl AudioConfig {
    pub fn block_ms(&self) -> f32 {
        1000.0 * self.block_size as f32 / self.sample_rate as f32
    }
}

/// Snapshot of the global and input-side settings the audio thread reads.
#[derive(Debug, Clone)]
pub struct Globals {
    pub params: Params,
    pub input_groups: Vec<ChannelGroup>,
    /// Not persisted upstream (`ChannelGroupParams::soloed`).
    pub input_solo: Vec<bool>,
    pub metronome_group: ChannelGroup,
    pub file_group: ChannelGroup,
    pub soundboard_group: ChannelGroup,
    /// Patchbay: bit `j` of `routed_to[i]` routes peer slot `j`'s audio into slot `i`'s send
    /// (`mRemoteSendMatrix[j][i]`).
    pub routed_to: [u32; MAX_PEERS],
    pub beats_per_bar: i32,
}

/// Snapshot of one remote peer's receive mix.
#[derive(Debug, Clone, Default)]
pub struct PeerParams {
    /// Peer level (`RemotePeer::gain`).
    pub gain: f32,
    pub soloed: bool,
    pub groups: Vec<ChannelGroup>,
    pub group_solo: Vec<bool>,
    pub recv_channels: usize,
    /// Channels we send this peer (the patchbay mixes into this many).
    pub send_channels: usize,
}

/// State the network side sets and the audio side reads (and the meter, the other way).
#[derive(Debug, Default)]
pub struct PeerShared {
    pub recv_active: AtomicBool,
    pub safety_muted: AtomicBool,
    pub meter: Meter,
}

/// The engine-wide meters: device input, send mix, device output.
#[derive(Debug, Default)]
pub struct Meters {
    pub input: Meter,
    pub send: Meter,
    pub output: Meter,
    /// Frames processed while recording (`mElapsedRecordSamples`).
    pub rec_frames: AtomicU64,
    /// [`RecTaps::generation`] of the taps the audio thread installed last.
    pub rec_installed: AtomicU64,
}

/// Audio side of a playing file: the file transport or one soundboard sample.
pub struct Deck {
    pub out: PlayerOutput,
    /// Transport running; stopped decks are silent and keep their position.
    pub playing: Arc<AtomicBool>,
    pub sample_rate: u32,
}

/// The engine-wide recording writers (per-peer ones go by slot, [`Cmd::UserRec`]).
#[derive(Default)]
pub struct RecTaps {
    pub mix: Option<RecordWriter>,
    pub mix_minus: Option<RecordWriter>,
    /// Per input group: the writer and the channels it was made for.
    pub selfs: Vec<Option<(RecordWriter, usize)>>,
    pub self_pre_fx: bool,
    pub self_silence_when_muted: bool,
    /// Count recorded frames into [`Meters::rec_frames`].
    pub counting: bool,
    /// Acknowledged in [`Meters::rec_installed`] once installed.
    pub generation: u64,
    /// Detach every peer's raw-stream writer on the same block (a stop): this holds
    /// [`MAX_PEERS`] empty entries to swap in, and the writers leave with the replaced taps.
    pub detach_users: bool,
    pub detached: Vec<Option<Box<RecordWriter>>>,
}

/// Audio-side state of one remote peer.
pub struct PeerMix {
    params: Box<PeerParams>,
    dsp: Vec<GroupDsp>,
    work: Vec<Vec<f32>>,
    last_gain: f32,
    meter: MeterState,
    shared: Arc<PeerShared>,
}

impl PeerMix {
    /// Allocates everything (64 groups of effects, a 64-channel work buffer): call off the
    /// audio thread.
    pub fn new(cfg: &AudioConfig, params: Box<PeerParams>, shared: Arc<PeerShared>) -> Box<Self> {
        let mut dsp: Vec<_> = (0..MAX_GROUPS)
            .map(|_| GroupDsp::new(cfg.sample_rate))
            .collect();
        for (d, g) in dsp.iter_mut().zip(&params.groups) {
            d.commit(g);
        }
        Box::new(Self {
            params,
            dsp,
            work: vec![vec![0.0; cfg.block_size]; MAX_CHANNELS],
            last_gain: 0.0,
            meter: MeterState::new(cfg.sample_rate, cfg.block_size),
            shared,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayTarget {
    Input(usize),
    Metronome,
    /// File playback and soundboard monitors (upstream `setFilePlaybackMonitorDelayParams`,
    /// `SoundboardChannelProcessor::setMonitorDelayParams`).
    File,
    Soundboard,
}

pub enum Cmd {
    Globals(Box<Globals>),
    /// A monitor delay line for a group, made for this many channels.
    Delay(DelayTarget, Box<MonitorDelay>, usize),
    AddPeer(usize, Box<PeerMix>),
    RemovePeer(usize),
    Peer(usize, Box<PeerParams>),
    /// The file transport (`None`: unloaded).
    File(Option<Box<Deck>>),
    /// One soundboard voice.
    Voice(usize, Option<Box<Deck>>),
    Rec(Box<RecTaps>),
    /// The raw-stream recorder of a peer slot; installing one sets its leading silence to
    /// the frames recorded so far ([`RecordWriter::set_leading_silence`]).
    UserRec(usize, Option<Box<RecordWriter>>),
}

/// Per-block values shared by the processing phases.
#[derive(Default)]
struct Block {
    dry: f32,
    any_soloed: bool,
    main_rev_enabled: bool,
    do_reverb: bool,
    do_in_rev: bool,
    met_enabled: bool,
}

pub struct Mixer {
    cfg: AudioConfig,
    g: Box<Globals>,
    inputs: Vec<GroupDsp>,
    met: GroupDsp,
    metronome: Metronome,
    main_rev: MainReverb,
    in_rev: InputReverb,
    main_rev_params: (ReverbParams, i32),
    in_rev_params: ReverbParams,
    /// `inputPostBuffer`: input groups after gain and effects, one channel per group channel.
    input_post: Vec<Vec<f32>>,
    post_ch: usize,
    /// `sendWorkBuffer`
    send_work: Vec<Vec<f32>>,
    send_ch: usize,
    /// `inputBuffer`: what is monitored of our own input.
    mon_bus: Vec<Vec<f32>>,
    main_fx: Vec<Vec<f32>>,
    in_rev_bus: Vec<Vec<f32>>,
    met_bus: Vec<Vec<f32>>,
    /// `tempBuffer`: the remote peers' mix.
    peers_bus: Vec<Vec<f32>>,
    scratch: Vec<Vec<f32>>,
    silent: Vec<f32>,
    peers: Vec<Option<Box<PeerMix>>>,
    b: Block,
    last_dry: f32,
    last_wet: f32,
    last_met_enabled: bool,
    in_meter: MeterState,
    send_meter: MeterState,
    out_meter: MeterState,
    meters: Arc<Meters>,
    file: Option<Box<Deck>>,
    file_dsp: GroupDsp,
    file_rec_dsp: GroupDsp,
    /// `fileBuffer`
    file_bus: Vec<Vec<f32>>,
    /// Channels of file audio this block; 0 when no file is loaded (`hasfiledata`).
    file_ch: usize,
    last_fgain: f32,
    voices: Vec<Option<Box<Deck>>>,
    sb_bus: Vec<Vec<f32>>,
    sb_tmp: Vec<Vec<f32>>,
    sb_dsp: GroupDsp,
    sb_rec_dsp: GroupDsp,
    /// Some soundboard voice played this block (`hassoundboarddata`).
    has_sb: bool,
    last_sbgain: f32,
    met_rec_dsp: GroupDsp,
    rec: Box<RecTaps>,
    user_rec: Vec<Option<Box<RecordWriter>>>,
    /// `inputPreBuffer`: the raw input per group channel, for pre-FX self recording.
    input_pre: Vec<Vec<f32>>,
    rec_bus: Vec<Vec<f32>>,
    /// One peer's send when something is cross-routed to it.
    peer_send_bus: Vec<Vec<f32>>,
    /// The main reverb's output went into the output this block (`hasmainfx`).
    fx_added: bool,
    /// Set by a plugin host before each block (`None`: standalone, no play head).
    pub host: Option<HostTransport>,
}

/// Received and device samples are clamped to this magnitude (+40 dB over full scale), so a
/// bogus stream cannot drive the filters and reverbs into overflow.
pub const SAMPLE_LIMIT: f32 = 100.0;

/// Non-finite samples become 0, the rest is clamped to [`SAMPLE_LIMIT`].
pub(crate) fn sanitize(x: &mut [f32]) {
    for v in x {
        *v = if v.is_finite() {
            v.clamp(-SAMPLE_LIMIT, SAMPLE_LIMIT)
        } else {
            0.0
        };
    }
}

fn all_finite<B: AsRef<[f32]>>(bufs: &[B], n: usize) -> bool {
    bufs.iter()
        .all(|b| b.as_ref()[..n].iter().all(|v| v.is_finite()))
}

fn bus(ch: usize, n: usize) -> Vec<Vec<f32>> {
    vec![vec![0.0; n]; ch]
}

fn clear(b: &mut [Vec<f32>], ch: usize, n: usize) {
    for c in b.iter_mut().take(ch) {
        c[..n].fill(0.0);
    }
}

fn reverb_params(p: &Params, main: bool) -> ReverbParams {
    if main {
        ReverbParams {
            level: p.main_reverb_level,
            size: p.main_reverb_size,
            damping: p.main_reverb_damp,
            predelay_ms: p.main_reverb_predelay,
        }
    } else {
        ReverbParams {
            level: p.in_reverb_level,
            size: p.in_reverb_size,
            damping: p.in_reverb_damp,
            predelay_ms: p.in_reverb_predelay,
        }
    }
}

/// JUCE `copyFrom` with gain.
fn copy_from(dst: &mut [f32], src: &[f32], gain: f32) {
    if approximately_equal(gain, 0.0) {
        dst.fill(0.0);
    } else {
        dst.copy_from_slice(src);
        apply_gain(dst, gain);
    }
}

/// JUCE `copyFromWithRamp`.
fn copy_from_with_ramp(dst: &mut [f32], src: &[f32], start: f32, end: f32) {
    if approximately_equal(start, end) {
        return copy_from(dst, src, start);
    }
    let step = (end - start) / dst.len() as f32;
    let mut g = start;
    for (d, s) in dst.iter_mut().zip(src) {
        *d = *s * g;
        g += step;
    }
}

impl Mixer {
    /// Allocates all buses; call off the audio thread.
    pub fn new(cfg: AudioConfig, globals: Box<Globals>, meters: Arc<Meters>) -> Self {
        let sr = cfg.sample_rate;
        let n = cfg.block_size;
        let maxch = cfg
            .input_channels
            .max(cfg.output_channels)
            .clamp(2, MAX_CHANNELS);
        let mut metronome = Metronome::new(sr, crate::wav::beat_click(), crate::wav::bar_click());
        metronome.set_tempo(f64::from(globals.params.met_tempo));
        let mut m = Self {
            cfg,
            inputs: (0..MAX_GROUPS).map(|_| GroupDsp::new(sr)).collect(),
            met: GroupDsp::new(sr),
            metronome,
            main_rev: MainReverb::new(sr),
            in_rev: InputReverb::new(sr),
            main_rev_params: (ReverbParams::default(), ReverbModel::default() as i32),
            in_rev_params: ReverbParams::default(),
            input_post: bus(MAX_CHANNELS, n),
            post_ch: 0,
            send_work: bus(MAX_CHANNELS, n),
            send_ch: 0,
            mon_bus: bus(maxch, n),
            main_fx: bus(2, n),
            in_rev_bus: bus(2, n),
            met_bus: bus(1, n),
            peers_bus: bus(maxch, n),
            scratch: bus(MAX_CHANNELS, n),
            silent: vec![0.0; n],
            peers: (0..MAX_PEERS).map(|_| None).collect(),
            b: Block::default(),
            last_dry: 0.0,
            last_wet: 0.0,
            last_met_enabled: false,
            in_meter: MeterState::new(sr, n),
            send_meter: MeterState::new(sr, n),
            out_meter: MeterState::new(sr, n),
            meters,
            file: None,
            file_dsp: GroupDsp::new(sr),
            file_rec_dsp: GroupDsp::new(sr),
            file_bus: bus(MAX_CHANNELS, n),
            file_ch: 0,
            last_fgain: 0.0,
            voices: (0..MAX_VOICES).map(|_| None).collect(),
            sb_bus: bus(SB_CH, n),
            sb_tmp: bus(SB_CH, n),
            sb_dsp: GroupDsp::new(sr),
            sb_rec_dsp: GroupDsp::new(sr),
            has_sb: false,
            last_sbgain: 0.0,
            met_rec_dsp: GroupDsp::new(sr),
            rec: Box::default(),
            user_rec: (0..MAX_PEERS).map(|_| None).collect(),
            input_pre: bus(MAX_CHANNELS, n),
            rec_bus: bus(maxch, n),
            peer_send_bus: bus(MAX_CHANNELS, n),
            fx_added: false,
            host: None,
            g: globals.clone(),
        };
        m.set_globals(globals);
        m
    }

    fn set_globals(&mut self, g: Box<Globals>) -> Box<Globals> {
        for (d, grp) in self.inputs.iter_mut().zip(&g.input_groups) {
            d.commit(grp);
        }
        self.met.commit(&g.metronome_group);
        self.met_rec_dsp.commit(&g.metronome_group);
        for d in [&mut self.file_dsp, &mut self.file_rec_dsp] {
            d.commit(&g.file_group);
        }
        for d in [&mut self.sb_dsp, &mut self.sb_rec_dsp] {
            d.commit(&g.soundboard_group);
        }
        self.metronome.set_beats_per_bar(g.beats_per_bar.max(1));
        let p = &g.params;
        // only touch changed reverb controls: MVerb clears its tank on a size change
        let main = (reverb_params(p, true), p.main_reverb_model);
        if main != self.main_rev_params {
            let (new, old) = (main.0, self.main_rev_params.0);
            if new.level != old.level {
                self.main_rev.set_level(new.level);
            }
            if new.size != old.size {
                self.main_rev.set_size(new.size);
            }
            if new.damping != old.damping {
                self.main_rev.set_damping(new.damping);
            }
            if new.predelay_ms != old.predelay_ms {
                self.main_rev.set_predelay_ms(new.predelay_ms);
            }
            self.main_rev.set_model(match main.1 {
                0 => ReverbModel::Freeverb,
                2 => ReverbModel::Zita,
                _ => ReverbModel::MVerb,
            });
            self.main_rev_params = main;
        }
        let inp = reverb_params(p, false);
        if inp != self.in_rev_params {
            let old = self.in_rev_params;
            if inp.level != old.level {
                self.in_rev.set_level(inp.level);
            }
            if inp.size != old.size {
                self.in_rev.set_size(inp.size);
            }
            if inp.damping != old.damping {
                self.in_rev.set_damping(inp.damping);
            }
            if inp.predelay_ms != old.predelay_ms {
                self.in_rev.set_predelay_ms(inp.predelay_ms);
            }
            self.in_rev_params = inp;
        }
        std::mem::replace(&mut self.g, g)
    }

    /// Apply one command. Produces at most one [`Garbage`] box.
    pub fn apply(&mut self, cmd: Cmd, drop_later: &mut dyn FnMut(Garbage)) {
        match cmd {
            Cmd::Globals(g) => drop_later(self.set_globals(g)),
            Cmd::Delay(target, d, ch) => {
                let (dsp, grp) = match target {
                    DelayTarget::Input(i) if i < MAX_GROUPS => {
                        (&mut self.inputs[i], self.g.input_groups.get(i))
                    }
                    DelayTarget::Input(_) => return drop_later(d),
                    DelayTarget::Metronome => (&mut self.met, Some(&self.g.metronome_group)),
                    DelayTarget::File => (&mut self.file_dsp, Some(&self.g.file_group)),
                    DelayTarget::Soundboard => (&mut self.sb_dsp, Some(&self.g.soundboard_group)),
                };
                if let Some((old, _)) = dsp.set_delay((d, ch)) {
                    drop_later(old);
                }
                if let Some(grp) = grp {
                    dsp.commit(grp);
                }
            }
            Cmd::AddPeer(slot, p) => {
                if let Some(old) = self.peers.get_mut(slot).and_then(|s| s.replace(p)) {
                    drop_later(old);
                }
            }
            Cmd::RemovePeer(slot) => {
                if let Some(old) = self.peers.get_mut(slot).and_then(Option::take) {
                    drop_later(old);
                }
            }
            Cmd::Peer(slot, params) => match self.peers.get_mut(slot).and_then(Option::as_mut) {
                Some(p) => {
                    for (d, g) in p.dsp.iter_mut().zip(&params.groups) {
                        d.commit(g);
                    }
                    let old: Box<PeerParams> = std::mem::replace(&mut p.params, params);
                    drop_later(old);
                }
                None => drop_later(params),
            },
            Cmd::File(d) => {
                if let Some(old) = std::mem::replace(&mut self.file, d) {
                    drop_later(old);
                }
            }
            Cmd::Voice(i, d) => match self.voices.get_mut(i) {
                Some(v) => {
                    if let Some(old) = std::mem::replace(v, d) {
                        drop_later(old);
                    }
                }
                None => {
                    if let Some(d) = d {
                        drop_later(d);
                    }
                }
            },
            Cmd::Rec(r) => {
                let mut old: Box<RecTaps> = std::mem::replace(&mut self.rec, r);
                if self.rec.detach_users && self.rec.detached.len() == self.user_rec.len() {
                    std::mem::swap(&mut self.user_rec, &mut self.rec.detached);
                    std::mem::swap(&mut old.detached, &mut self.rec.detached);
                }
                self.meters
                    .rec_installed
                    .store(self.rec.generation, Ordering::Release);
                drop_later(old);
            }
            Cmd::UserRec(slot, mut w) => match self.user_rec.get_mut(slot) {
                Some(u) => {
                    // the segment starts where the recording is now: silence up to here
                    if let Some(w) = &mut w {
                        w.set_leading_silence(self.meters.rec_frames.load(Ordering::Relaxed));
                    }
                    if let Some(old) = std::mem::replace(u, w) {
                        drop_later(old);
                    }
                }
                None => {
                    if let Some(w) = w {
                        drop_later(w);
                    }
                }
            },
        }
    }

    pub fn has_peer(&self, slot: usize) -> bool {
        self.peers.get(slot).is_some_and(Option::is_some)
    }

    /// Steps 1-13: input groups, send mix, monitor bus, metronome, input reverb, meters.
    pub fn process_input(&mut self, inputs: &[&[f32]], n: usize) {
        let n_out = self.cfg.output_channels.min(MAX_CHANNELS);
        let p = &self.g.params;
        let groups = &self.g.input_groups[..self.g.input_groups.len().min(MAX_GROUPS)];

        // 1
        let in_gain = if p.main_in_mute { 0.0 } else { 1.0 };
        let any_peer_solo = self.peers.iter().flatten().any(|pm| pm.params.soloed);
        self.b.any_soloed = p.main_monitor_solo || any_peer_solo;
        self.b.dry = if self.b.any_soloed && !p.main_monitor_solo {
            0.0
        } else {
            p.dry
        };

        // 3: file and soundboard count when sent (a file's channels, 2 without a file)
        let met_send = usize::from(p.send_met_audio);
        let file_nch = self
            .file
            .as_ref()
            .map_or(2, |d| d.out.channels().min(MAX_CHANNELS));
        let (send_file, send_sb) = (p.send_file_audio, p.send_soundboard_audio);
        let file_send = if send_file { file_nch } else { 0 };
        let sb_send = if send_sb { SB_CH } else { 0 };
        let post_ch = (groups.iter().map(|g| idx(g.num_channels)).sum::<usize>()
            + met_send
            + file_send
            + sb_send)
            .min(MAX_CHANNELS);
        self.post_ch = post_ch;

        // 5: the JUCE buffer has max(in, out) channels; missing ones replicate the last input
        // into channels 0/1, else silence
        let n_in = inputs.len().min(MAX_CHANNELS);
        let nbuf = n_in.max(n_out).min(MAX_CHANNELS);
        let silent = &self.silent[..n];
        let pick = |c: usize| inputs[c].get(..n).unwrap_or(silent);
        let from: [&[f32]; MAX_CHANNELS] = std::array::from_fn(|c| {
            if c < n_in {
                pick(c)
            } else if n_in > 0 && c < 2 {
                pick(n_in - 1)
            } else {
                silent
            }
        });

        // 6
        self.in_meter
            .measure(&from[..n_in], n_in, n, &self.meters.input);
        clear(&mut self.input_post, post_ch, n);
        let in_rev_enabled = groups.iter().any(|g| g.in_reverb_send > 0.0);
        self.in_rev.set_enabled(in_rev_enabled);
        self.b.do_in_rev = self.in_rev.is_active();
        if self.b.do_in_rev {
            clear(&mut self.in_rev_bus, 2, n);
        }
        let pre_fx = self.rec.self_pre_fx && self.rec.selfs.iter().any(Option::is_some);
        let mut destch = 0;
        for (d, g) in self.inputs.iter_mut().zip(groups) {
            let nch = idx(g.num_channels);
            if pre_fx {
                // the raw input of the group's channels, for pre-FX self recording
                for ch in 0..nch.min(MAX_CHANNELS.saturating_sub(destch)) {
                    let usech = idx(g.chan_start) + ch;
                    let src = if usech < nbuf { from[usech] } else { silent };
                    self.input_pre[destch + ch][..n].copy_from_slice(&src[..n]);
                }
            }
            let rev = self
                .b
                .do_in_rev
                .then_some((&mut self.in_rev_bus[..], in_rev_enabled));
            d.input_block(
                g,
                &from[..nbuf],
                &mut self.input_post,
                post_ch,
                (destch, nch),
                n,
                in_gain,
                rev,
            );
            destch += nch;
        }

        // 7: send mix
        let send_mode = p.send_channels;
        let send_pan_ch = if send_mode <= 0 {
            post_ch
        } else {
            (send_mode as usize).min(MAX_CHANNELS)
        };
        let sw_ch = post_ch.max(n_in).max(n_out).clamp(2, MAX_CHANNELS);
        self.send_ch = if send_mode <= 0 {
            post_ch
        } else {
            send_mode as usize
        };
        let tgain = if send_pan_ch == 1 && post_ch > 0 {
            1.0 / (post_ch as f32 * 0.5).max(1.0)
        } else {
            1.0
        };
        clear(&mut self.send_work, sw_ch, n);
        if send_pan_ch > 2 || send_mode == 0 {
            for c in 0..sw_ch.min(post_ch) {
                let (src, dst) = (&self.input_post[c], &mut self.send_work[c]);
                dst[..n].copy_from_slice(&src[..n]);
            }
        } else {
            let mut srcstart = 0;
            for (d, g) in self.inputs.iter_mut().zip(groups) {
                let dests = (
                    idx(g.pan_dest_start),
                    send_pan_ch.min(idx(g.pan_dest_channels)),
                );
                d.pan(
                    g,
                    &self.input_post,
                    post_ch,
                    srcstart,
                    &mut self.send_work,
                    sw_ch,
                    dests,
                    n,
                    tgain,
                );
                srcstart += idx(g.num_channels);
            }
        }

        // 8: monitor bus
        self.b.main_rev_enabled = p.main_reverb_enabled;
        self.main_rev.set_enabled(p.main_reverb_enabled);
        self.b.do_reverb = self.main_rev.is_active();
        clear(&mut self.main_fx, 2, n);
        let mon_ch = self.mon_bus.len();
        clear(&mut self.mon_bus, mon_ch, n);
        let solo = &self.g.input_solo;
        let any_input_solo = solo.iter().take(groups.len()).any(|&s| s);
        let mon_pan_ch = mon_ch.min(n_out);
        let tmgain = if mon_pan_ch == 1 && n_in > 0 {
            1.0 / (n_in as f32 * 0.5).max(1.0)
        } else {
            1.0
        };
        let mut srcstart = 0;
        for (i, (d, g)) in self.inputs.iter_mut().zip(groups).enumerate() {
            let soloed = solo.get(i).copied().unwrap_or(false);
            let utm = if any_input_solo && !soloed {
                0.0
            } else {
                tmgain
            };
            let dests = (
                idx(g.mon_dest_start),
                mon_pan_ch.min(idx(g.mon_dest_channels)),
            );
            let rev = self.b.do_reverb.then_some((
                &mut self.main_fx[..],
                self.b.main_rev_enabled,
                self.b.dry,
            ));
            d.monitor(
                g,
                &self.input_post,
                post_ch,
                srcstart,
                &mut self.mon_bus,
                mon_ch,
                dests,
                n,
                utm,
                rev,
                &mut self.scratch,
            );
            srcstart += idx(g.num_channels);
        }
        let met_start = srcstart;
        let file_start = met_start + met_send;

        // 9: file playback (P.cpp:7617-7686); the transport plays silence while stopped
        self.file_ch = 0;
        let mut file_pos = None;
        if let Some(d) = &mut self.file {
            let ch = d.out.channels().min(MAX_CHANNELS);
            let mut it = self.file_bus.iter_mut();
            let mut outs: [&mut [f32]; MAX_CHANNELS] =
                std::array::from_fn(|_| it.next().map_or(&mut [][..], |c| &mut c[..n]));
            if d.playing.load(Ordering::Relaxed) {
                file_pos = Some(d.out.position() as f64 / f64::from(d.sample_rate.max(1)));
                d.out.read(&mut outs[..ch]);
            } else {
                outs[..ch].iter_mut().for_each(|o| o.fill(0.0));
            }
            self.file_ch = ch;
            self.g.file_group.num_channels = ch as i32;
        }
        if self.file_ch > 0 && send_file {
            let ch = self.file_ch;
            let fg = &self.g.file_group;
            // upstream ramps from the new gain to the last one (`addFromWithRamp(.., fgain,
            // lastfgain)`, P.cpp:7646, 7663); we ramp last -> new
            let last = self.last_fgain;
            if send_pan_ch == 1 {
                let fgain = fg.gain / (ch as f32).max(1.0);
                for c in 0..ch {
                    let (src, dst) = (&self.file_bus[c][..n], &mut self.send_work[0][..n]);
                    add_from_with_ramp(dst, src, last, fgain);
                }
                self.last_fgain = fgain;
            } else if send_pan_ch > 2 {
                for c in 0..ch.min(sw_ch.saturating_sub(file_start)) {
                    let (src, dst) = (&self.file_bus[c], &mut self.send_work[file_start + c]);
                    add_from_with_ramp(&mut dst[..n], &src[..n], last, fg.gain);
                }
                self.last_fgain = fg.gain;
            } else if send_pan_ch == 2 {
                let dests = (
                    idx(fg.pan_dest_start),
                    send_pan_ch.min(idx(fg.pan_dest_channels)),
                );
                self.file_dsp.pan(
                    fg,
                    &self.file_bus,
                    ch,
                    0,
                    &mut self.send_work,
                    sw_ch,
                    dests,
                    n,
                    fg.gain,
                );
                self.last_fgain = fg.gain;
            }
        }

        // 10: soundboard (SoundboardChannelProcessor::processAudioBlock/sendAudioBlock): the
        // playing samples summed into a stereo bus; a mono file plays on both sides (JUCE)
        clear(&mut self.sb_bus, SB_CH, n);
        self.has_sb = false;
        for v in self.voices.iter_mut().flatten() {
            if !v.playing.load(Ordering::Relaxed) {
                continue;
            }
            let [a, b] = &mut self.sb_tmp[..] else {
                unreachable!()
            };
            v.out.read(&mut [&mut a[..n], &mut b[..n]]);
            if v.out.channels() == 1 {
                b[..n].copy_from_slice(&a[..n]);
            }
            for (dst, src) in self.sb_bus.iter_mut().zip(&self.sb_tmp) {
                add_from(&mut dst[..n], &src[..n], 1.0);
            }
            self.has_sb = true;
        }
        self.g.soundboard_group.num_channels = SB_CH as i32;
        if self.has_sb && send_sb {
            let start = file_start + file_send;
            let sg = &self.g.soundboard_group;
            let last = self.last_sbgain;
            let mut gain = sg.gain;
            if send_pan_ch == 1 {
                gain /= SB_CH as f32;
                for c in 0..SB_CH {
                    let (src, dst) = (&self.sb_bus[c][..n], &mut self.send_work[0][..n]);
                    add_from_with_ramp(dst, src, last, gain);
                }
            } else if send_pan_ch > 2 {
                for c in 0..SB_CH.min(sw_ch.saturating_sub(start)) {
                    let (src, dst) = (&self.sb_bus[c], &mut self.send_work[start + c]);
                    add_from_with_ramp(&mut dst[..n], &src[..n], last, gain);
                }
            } else if send_pan_ch == 2 {
                let dests = (
                    idx(sg.pan_dest_start),
                    send_pan_ch.min(idx(sg.pan_dest_channels)),
                );
                self.sb_dsp.pan(
                    sg,
                    &self.sb_bus,
                    SB_CH,
                    0,
                    &mut self.send_work,
                    sw_ch,
                    dests,
                    n,
                    gain,
                );
            }
            self.last_sbgain = gain;
        }

        // 11: metronome, free running, at the host's beat (`syncMetHost`, P.cpp:7405-7428,
        // 7690-7724) or at the file position's beat (`syncMetFile`)
        let p = &self.g.params;
        let host = self.host.filter(|_| p.sync_met_host);
        let tempo = host
            .map(|h| h.bpm)
            .filter(|&b| b > 0.0)
            .unwrap_or(f64::from(p.met_tempo));
        let host_beat = host.filter(|h| h.playing).map(|h| h.ppq.unwrap_or(0.0));
        let sync_file = p.sync_met_file && file_pos.is_some();
        let sync = host_beat.is_some() || sync_file;
        let met_enabled = p.met_enabled;
        let mut met_gain = p.met_gain;
        if met_enabled != self.last_met_enabled {
            if met_enabled {
                self.metronome.set_gain(met_gain, true);
                if !sync {
                    self.metronome.reset_relative_start(0.0);
                }
            } else {
                met_gain = 0.0;
                self.metronome.set_gain(0.0, false);
            }
        }
        self.b.met_enabled = met_enabled;
        if self.last_met_enabled || met_enabled {
            self.metronome.set_gain(met_gain, false);
            self.metronome.set_tempo(tempo);
            let file_beat = file_pos.filter(|_| sync_file).map(|s| tempo / 60.0 * s);
            let beat = host_beat.or(file_beat).unwrap_or(0.0);
            self.metronome
                .render(&mut self.met_bus[0][..n], beat, !sync);
            if p.send_met_audio {
                if send_pan_ch > 2 {
                    if met_start < sw_ch {
                        let (src, dst) = (&self.met_bus[0], &mut self.send_work[met_start]);
                        add_from(&mut dst[..n], &src[..n], 1.0);
                    }
                } else {
                    let mg = &self.g.metronome_group;
                    let dests = (
                        idx(mg.pan_dest_start),
                        send_pan_ch.min(idx(mg.pan_dest_channels)),
                    );
                    self.met.pan(
                        mg,
                        &self.met_bus,
                        1,
                        0,
                        &mut self.send_work,
                        sw_ch,
                        dests,
                        n,
                        1.0,
                    );
                }
            }
        }
        self.last_met_enabled = met_enabled;

        // 12: input reverb into the send mix (mono/stereo sends only)
        if self.b.do_in_rev {
            let [l, r] = &mut self.in_rev_bus[..] else {
                unreachable!()
            };
            self.in_rev.process(&mut l[..n], &mut r[..n]);
            if send_mode == 1 || send_mode == 2 {
                for c in 0..send_pan_ch.min(2) {
                    add_from(&mut self.send_work[c][..n], &self.in_rev_bus[c][..n], 1.0);
                }
            }
        }

        // defensive: a processor that went non-finite anyway is reset, its block dropped
        if !all_finite(&self.send_work[..sw_ch], n) {
            clear(&mut self.send_work, sw_ch, n);
            self.reset_dsp();
        }

        // 13
        self.send_meter
            .measure(&self.send_work, self.send_ch, n, &self.meters.send);

        // the peers' mix is rebuilt by `process_peer`
        let pb = self.peers_bus.len();
        clear(&mut self.peers_bus, pb, n);
    }

    /// A peer's received audio as the transport filled it (before level and effects) and its
    /// channel count, while receiving is active: what upstream copies to the per-user plugin
    /// output bus (P.cpp:7875-7890).
    pub fn peer_received(&self, slot: usize) -> Option<(&[Vec<f32>], usize)> {
        let p = self.peers.get(slot)?.as_ref()?;
        let n = p.params.recv_channels.min(MAX_CHANNELS);
        (n > 0 && p.shared.recv_active.load(Ordering::Relaxed)).then_some((&p.work[..], n))
    }

    /// The (cleared) receive buffer of a peer, 64 channels, for the transport to fill.
    pub fn peer_buffer(&mut self, slot: usize, n: usize) -> Option<&mut [Vec<f32>]> {
        let p = self.peers.get_mut(slot)?.as_mut()?;
        clear(&mut p.work, MAX_CHANNELS, n);
        Some(&mut p.work)
    }

    /// Step 14 for one peer whose buffer the transport has filled.
    pub fn process_peer(&mut self, slot: usize, n: usize) {
        let Some(Some(peer)) = self.peers.get_mut(slot) else {
            return;
        };
        let n_out = self.cfg.output_channels.min(MAX_CHANNELS);
        let pp = &peer.params;
        // the boundary: whatever the stream decoded, only finite, bounded samples go on (the
        // sink fills at most max(output channels, stream channels))
        for c in &mut peer.work[..pp.recv_channels.max(n_out).min(MAX_CHANNELS)] {
            sanitize(&mut c[..n]);
        }
        // the raw stream, pre level/effects, ignoring mute and solo (P.cpp:7856-7874)
        if let Some(Some(w)) = self.user_rec.get_mut(slot) {
            let nch = pp.recv_channels.min(MAX_CHANNELS);
            let silent = &self.silent[..n];
            let chans: [&[f32]; MAX_CHANNELS] =
                std::array::from_fn(|c| if c < nch { &peer.work[c][..n] } else { silent });
            w.write(&chans);
        }
        let recv_active = peer.shared.recv_active.load(Ordering::Relaxed);
        let safety = peer.shared.safety_muted.load(Ordering::Relaxed);
        let mut usegain = pp.gain;
        let mut was_silent = false;
        if !recv_active || (self.b.any_soloed && !pp.soloed) || safety {
            usegain = 0.0;
            was_silent = peer.last_gain <= 0.0;
        }
        let groups = &pp.groups[..pp.groups.len().min(MAX_GROUPS)];
        let any_sub_solo = pp.group_solo.iter().take(groups.len()).any(|&s| s);
        for (d, g) in peer.dsp.iter_mut().zip(groups) {
            d.inplace_block(g, &mut peer.work, MAX_CHANNELS, n, usegain);
        }
        peer.last_gain = usegain;
        peer.meter
            .measure(&peer.work, pp.recv_channels, n, &peer.shared.meter);
        if was_silent {
            return;
        }
        // upstream applies the level twice: in the group gain above and here (P.cpp:7946-7947)
        let mut tgain = if n_out == 1 && pp.recv_channels > 0 {
            1.0 / pp.recv_channels as f32
        } else {
            1.0
        };
        tgain *= usegain;
        let pb = self.peers_bus.len();
        for (i, (d, g)) in peer.dsp.iter_mut().zip(groups).enumerate() {
            let adj = if any_sub_solo && !pp.group_solo.get(i).copied().unwrap_or(false) {
                0.0
            } else {
                tgain
            };
            let start = idx(g.chan_start);
            let dests = (idx(g.pan_dest_start), n_out.min(idx(g.pan_dest_channels)));
            d.pan(
                g,
                &peer.work,
                MAX_CHANNELS,
                start,
                &mut self.peers_bus,
                pb,
                dests,
                n,
                adj,
            );
            if self.b.do_reverb {
                let src = (start, idx(g.num_channels));
                d.reverb_send(
                    g,
                    &peer.work,
                    MAX_CHANNELS,
                    src,
                    &mut self.main_fx,
                    n,
                    self.b.main_rev_enabled,
                    adj,
                );
            }
        }
    }

    /// Step 17 (P.cpp:8268-8392): own input per group, mix minus our input, full mix.
    fn record(&mut self, n: usize, n_out: usize) {
        let p = &self.g.params;
        let silent = &self.silent[..n];
        let r = &mut *self.rec;
        if r.selfs.iter().any(Option::is_some) {
            let src = if r.self_pre_fx {
                &self.input_pre
            } else {
                &self.input_post
            };
            let mut chindex = 0;
            for (i, g) in self.g.input_groups.iter().take(MAX_GROUPS).enumerate() {
                let chcnt = idx(g.num_channels);
                if let Some(Some((w, nch))) = r.selfs.get_mut(i) {
                    let mute = r.self_silence_when_muted && (p.main_in_mute || g.muted);
                    let chans: [&[f32]; MAX_CHANNELS] =
                        std::array::from_fn(|j| match src.get(chindex + j) {
                            Some(c) if j < chcnt && !mute => &c[..n],
                            _ => silent,
                        });
                    w.write(&chans[..(*nch).min(MAX_CHANNELS)]);
                }
                chindex += chcnt;
            }
        }
        if r.mix.is_some() || r.mix_minus.is_some() {
            let rec_ch = if n_out == 0 { 2 } else { n_out }.min(self.rec_bus.len());
            clear(&mut self.rec_bus, rec_ch, n);
            let wet = p.wet;
            let wet_ramp = (wet - self.last_wet).abs() > 0.00001;
            for (c, dst) in self.rec_bus.iter_mut().enumerate().take(rec_ch) {
                let fx = self.main_fx.get(c).filter(|_| self.fx_added);
                for src in [self.peers_bus.get(c), fx].into_iter().flatten() {
                    if wet_ramp {
                        add_from_with_ramp(&mut dst[..n], &src[..n], self.last_wet, wet);
                    } else {
                        add_from(&mut dst[..n], &src[..n], wet);
                    }
                }
            }
            if self.file_ch > 0 {
                let fg = &self.g.file_group;
                let dests = (idx(fg.mon_dest_start), n_out.min(idx(fg.mon_dest_channels)));
                self.file_rec_dsp.monitor(
                    fg,
                    &self.file_bus,
                    self.file_ch,
                    0,
                    &mut self.rec_bus,
                    rec_ch,
                    dests,
                    n,
                    fg.gain * wet,
                    None,
                    &mut self.scratch,
                );
            }
            if self.has_sb {
                let sg = &self.g.soundboard_group;
                let dests = (idx(sg.mon_dest_start), n_out.min(idx(sg.mon_dest_channels)));
                self.sb_rec_dsp.monitor(
                    sg,
                    &self.sb_bus,
                    SB_CH,
                    0,
                    &mut self.rec_bus,
                    rec_ch,
                    dests,
                    n,
                    sg.gain * wet,
                    None,
                    &mut self.scratch,
                );
            }
            if self.b.met_enabled && p.met_is_recorded {
                let mg = &self.g.metronome_group;
                let dests = (idx(mg.mon_dest_start), n_out.min(idx(mg.mon_dest_channels)));
                self.met_rec_dsp.monitor(
                    mg,
                    &self.met_bus,
                    1,
                    0,
                    &mut self.rec_bus,
                    rec_ch,
                    dests,
                    n,
                    mg.gain * wet,
                    None,
                    &mut self.scratch,
                );
            }
            let chans: [&[f32]; MAX_CHANNELS] =
                std::array::from_fn(|c| self.rec_bus.get(c).map_or(silent, |v| &v[..n]));
            if let Some(w) = &mut r.mix_minus {
                w.write(&chans[..rec_ch]);
            }
            // our monitored input: at the monitor level, or in full when monitoring is off
            // and nobody else is soloed
            let dry = self.b.dry;
            let dry_ramp = (dry - self.last_dry).abs() > 0.00001;
            let full = !self.b.any_soloed || p.main_monitor_solo;
            for (c, dst) in self.rec_bus.iter_mut().enumerate().take(rec_ch) {
                let rev = self.in_rev_bus.get(c).filter(|_| self.b.do_in_rev);
                for src in [self.mon_bus.get(c), rev].into_iter().flatten() {
                    if p.dry > 0.0 {
                        if dry_ramp {
                            add_from_with_ramp(&mut dst[..n], &src[..n], self.last_dry, dry);
                        } else {
                            add_from(&mut dst[..n], &src[..n], dry);
                        }
                    } else if full {
                        add_from(&mut dst[..n], &src[..n], 1.0);
                    }
                }
            }
            let chans: [&[f32]; MAX_CHANNELS] =
                std::array::from_fn(|c| self.rec_bus.get(c).map_or(silent, |v| &v[..n]));
            if let Some(w) = &mut r.mix {
                w.write(&chans[..rec_ch]);
            }
        }
        if r.counting {
            self.meters
                .rec_frames
                .fetch_add(n as u64, Ordering::Relaxed);
        }
    }

    /// Clear every effect and reverb state (allocation-free; only after non-finite output).
    fn reset_dsp(&mut self) {
        for d in self.inputs.iter_mut().chain([&mut self.met]) {
            d.reset_fx();
        }
        for p in self.peers.iter_mut().flatten() {
            for d in &mut p.dsp {
                d.reset_fx();
            }
        }
        self.main_rev.reset();
        self.in_rev.reset();
        for b in self.main_fx.iter_mut().chain(&mut self.in_rev_bus) {
            b.fill(0.0);
        }
    }

    /// The send mix (`sendWorkBuffer`); a peer sending `k` channels takes the first `k`.
    pub fn send_mix(&self) -> &[Vec<f32>] {
        &self.send_work
    }

    /// Step 15 for one peer: the send mix plus whatever the patchbay routes to it, after every
    /// peer went through [`Self::process_peer`] (P.cpp:7966-8023). The cross-routed audio is
    /// the source peer's processed stream; upstream's pans for it are never changed from their
    /// defaults (mono and >2 channels centre, a stereo stream left/right), applied as a
    /// balance (no pan law) to the first destination channel and to all the others.
    pub fn peer_send(&mut self, slot: usize, n: usize) -> &[Vec<f32>] {
        let routed = self.g.routed_to.get(slot).copied().unwrap_or(0);
        let sc = match self.peers.get(slot) {
            Some(Some(p)) if routed != 0 => p.params.send_channels.min(MAX_CHANNELS),
            _ => return &self.send_work,
        };
        for c in 0..sc {
            let (src, dst) = (&self.send_work[c], &mut self.peer_send_bus[c]);
            dst[..n].copy_from_slice(&src[..n]);
        }
        for j in (0..MAX_PEERS).filter(|j| routed & (1 << j) != 0) {
            let Some(Some(src)) = self.peers.get(j) else {
                continue;
            };
            let rc = src.params.recv_channels.min(MAX_CHANNELS);
            for (channel, dst) in self.peer_send_bus.iter_mut().enumerate().take(sc) {
                if rc > 0 && sc > 1 {
                    for (ch, w) in src.work.iter().enumerate().take(rc) {
                        let pan = match (rc, ch % 2) {
                            (2, 0) => -1.0,
                            (2, _) => 1.0,
                            _ => 0.0,
                        };
                        let pg = match (channel, pan >= 0.0) {
                            (0, true) => 1.0 - pan,
                            (0, false) | (_, true) => 1.0,
                            (_, false) => 1.0 + pan,
                        };
                        add_from(&mut dst[..n], &w[..n], pg);
                    }
                } else {
                    add_from(&mut dst[..n], &src.work[channel][..n], 1.0);
                }
            }
        }
        &self.peer_send_bus
    }

    /// The peers' bus, as many channels as the output has, when the latency test is to be
    /// heard (`hearlatencytest`): the return goes to channel 1, or 0 on a mono output
    /// (`mainBusOutputChannels > 1 ? 1 : 0`, P.cpp:8044).
    pub fn latency_monitor(&mut self) -> Option<&mut [Vec<f32>]> {
        let n = self.cfg.output_channels.clamp(1, self.peers_bus.len());
        self.g
            .params
            .hear_latency_test
            .then_some(&mut self.peers_bus[..n])
    }

    /// Steps 16 and 18: monitor, peers, reverbs, metronome, output gain, output meter.
    pub fn process_output(&mut self, outputs: &mut [&mut [f32]], n: usize) {
        let n_out = self
            .cfg
            .output_channels
            .min(outputs.len())
            .min(MAX_CHANNELS);
        let p = &self.g.params;
        let dry = self.b.dry;
        let in_rev_direct = !(self.b.any_soloed && !p.main_monitor_solo) && dry == 0.0;
        let dry_ramp = (dry - self.last_dry).abs() > 0.00001;
        for (c, out) in outputs.iter_mut().enumerate() {
            let out = &mut out[..n];
            match self.mon_bus.get(c).filter(|_| c < n_out) {
                Some(src) if dry_ramp => copy_from_with_ramp(out, &src[..n], self.last_dry, dry),
                Some(src) => copy_from(out, &src[..n], dry),
                None => out.fill(0.0),
            }
            if let Some(src) = self.peers_bus.get(c).filter(|_| c < n_out) {
                add_from(out, &src[..n], 1.0);
            }
        }
        // main reverb (always stereo here; a mono output gets the downmix)
        self.fx_added = false;
        if self.b.do_reverb {
            let [l, r] = &mut self.main_fx[..] else {
                unreachable!()
            };
            if self.main_rev.process(&mut l[..n], &mut r[..n]) {
                self.fx_added = true;
                if n_out == 1 {
                    add_from(&mut outputs[0][..n], &l[..n], 0.5);
                    add_from(&mut outputs[0][..n], &r[..n], 0.5);
                } else {
                    for (out, fx) in outputs.iter_mut().zip(&self.main_fx).take(n_out) {
                        add_from(&mut out[..n], &fx[..n], 1.0);
                    }
                }
            }
        }
        // input reverb: scaled by the monitor level, or full when monitoring is off
        if self.b.do_in_rev {
            for (out, rv) in outputs.iter_mut().zip(&self.in_rev_bus).take(n_out.min(2)) {
                if dry > 0.0 || dry_ramp {
                    if dry_ramp {
                        add_from_with_ramp(&mut out[..n], &rv[..n], self.last_dry, dry);
                    } else {
                        add_from(&mut out[..n], &rv[..n], dry);
                    }
                } else if in_rev_direct {
                    add_from(&mut out[..n], &rv[..n], 1.0);
                }
            }
        }
        // file playback and soundboard monitors (P.cpp:8214-8226)
        if self.file_ch > 0 {
            let fg = &self.g.file_group;
            let dests = (idx(fg.mon_dest_start), n_out.min(idx(fg.mon_dest_channels)));
            self.file_dsp.monitor(
                fg,
                &self.file_bus,
                self.file_ch,
                0,
                outputs,
                n_out,
                dests,
                n,
                fg.gain,
                None,
                &mut self.scratch,
            );
        }
        if self.has_sb {
            let sg = &self.g.soundboard_group;
            let dests = (idx(sg.mon_dest_start), n_out.min(idx(sg.mon_dest_channels)));
            self.sb_dsp.monitor(
                sg,
                &self.sb_bus,
                SB_CH,
                0,
                outputs,
                n_out,
                dests,
                n,
                sg.gain,
                None,
                &mut self.scratch,
            );
        }
        if self.b.met_enabled {
            let mg = &self.g.metronome_group;
            let dests = (idx(mg.mon_dest_start), n_out.min(idx(mg.mon_dest_channels)));
            self.met.monitor(
                mg,
                &self.met_bus,
                1,
                0,
                outputs,
                n_out,
                dests,
                n,
                mg.gain,
                None,
                &mut self.scratch,
            );
        }
        let wet = p.wet;
        for out in outputs.iter_mut().take(n_out) {
            if (wet - self.last_wet).abs() > 0.00001 {
                apply_gain_ramp(&mut out[..n], self.last_wet, wet);
            } else {
                apply_gain(&mut out[..n], wet);
            }
        }
        if !all_finite(&outputs[..n_out], n) {
            for out in outputs.iter_mut() {
                out[..n].fill(0.0);
            }
            self.reset_dsp();
        }
        self.out_meter
            .measure(outputs, n_out, n, &self.meters.output);
        self.record(n, n_out);
        self.last_dry = dry;
        self.last_wet = wet;
    }
}

#[cfg(test)]
mod tests;
