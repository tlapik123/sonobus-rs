//! OSC remote control, wire-compatible with the SonoBusMendeni fork so its TouchOSC template
//! (`util/SonoBusMendeni.tosc`) drives this app. Source of truth: the fork's
//! `Source/OSCManager.cpp` (the codec quirks), `SonobusPluginEditor.cpp`
//! (`registerAllOSCControls` 1500-4062: inbound; `sendAllOSCState` 4229-4382,
//! `parameterChanged` 7247-7480, `buttonClicked`, the peer-level timer 5323-5352: outbound),
//! `ChannelGroupsView.cpp`, `OptionsView.cpp`, `SoundboardView.cpp` (more outbound).
//!
//! Wire behaviour (fork quirks reproduced):
//! - UDP, one message per address with one argument; receive port 6000, feedback to
//!   127.0.0.1:6001 (`SonobusPluginProcessor.h:1258-1260`; the fork's docs say 9000/9001).
//! - Outbound: a float/double value is sent as float32 times 0.5, ints as int32, strings as
//!   strings (`OSCManager::sendMessage`). `/MainMuteButton` and `/MainRecvMuteButton` are
//!   inverted on the wire both ways (int 0 <-> 1; an inbound float < 0.5 counts as 0).
//! - Inbound floats are taken raw except `/OutGainSlider`, `/DrySlider`, `/SoundboardVolumeSlider`,
//!   `/Peer<n>Level`, `/Peer<n>Pan` (times 2); the gain faders then go through the fork's
//!   square curve (`oscPositionToGainValue`), so 0.707 is unity.
//! - Only `i f s b` arguments are understood (JUCE drops a packet with any other tag);
//!   bundles are unpacked (JUCE dispatches their messages at once).
//!
//! Ours on top (security: an unauthenticated control channel): disabled by default, bound to
//! 127.0.0.1 unless "allow control from the network" is on, packets over [`MAX_PACKET`]
//! ignored, at most [`MAX_RATE`] messages a second. Inbound messages are queued by a
//! background thread and applied on [`crate::controller::AppController::tick`]; feedback is
//! the difference between the current and the last sent state, at most every
//! [`FEEDBACK_PERIOD`]; peer meters every [`METER_PERIOD`] (the fork's 150 ms timer).

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::time::{Duration, Instant};

use aoo::osc::{self, Arg};
use sonobus_core::state::{ChannelGroup, ExtraState};
use sonobus_core::valuetree::Var as TreeVar;
use sonobus_dsp::{Compressor, Expander, ParamSpec, ParametricEq};
use sonobus_engine::options as opt;
use sonobus_engine::{MeterLevels, PeerStatus};

use crate::controller::AppController;
use crate::tr;

/// Settings keys, as the fork persists them in `ExtraState` (P.cpp:101-106, 8624-8629).
pub const KEY_ENABLED: &str = "OSCEnabled";
pub const KEY_SEND_STATE: &str = "OSCSendStateOnStart";
pub const KEY_PEER_LEVELS: &str = "OSCSendPeerLevels";
pub const KEY_TARGET_IP: &str = "OSCTargetIPAddress";
pub const KEY_TARGET_PORT: &str = "OSCTargetPort";
pub const KEY_RECEIVE_PORT: &str = "OSCReceivePort";
/// Ours: listen on all interfaces instead of 127.0.0.1 (the fork always does).
pub const KEY_ALLOW_NETWORK: &str = "OSCAllowNetwork";

/// The fork's TouchOSC template (GPLv3, in its `util/` folder).
pub const TEMPLATE_NAME: &str = "SonoBusMendeni.tosc";

/// Bigger datagrams are dropped (TouchOSC messages are tens of bytes).
pub const MAX_PACKET: usize = 8192;
/// Inbound messages per second; the rest of the second is dropped.
pub const MAX_RATE: u32 = 1000;
pub const FEEDBACK_PERIOD: Duration = Duration::from_millis(50);
pub const METER_PERIOD: Duration = Duration::from_millis(150);
/// Feedback messages sent per [`FEEDBACK_PERIOD`] at most: the full-state dump (~800
/// messages) goes out over several ticks instead of overflowing the receiver's socket buffer
/// (the fork sends it in one burst).
pub const BURST: usize = 128;
/// Queued feedback beyond this is dropped, oldest first.
const OUT_QUEUE: usize = 4096;
/// Inbound messages waiting for a tick.
const QUEUE: usize = 1024;
const SLOTS: usize = 16;

/// The fork's `OSC_SCALE_FACTOR` (outbound floats) and `OSC_INVERSE_SCALE_FACTOR`.
const SCALE: f32 = 0.5;
const INVERSE: f32 = 2.0;

#[derive(Debug, Clone, PartialEq)]
pub struct OscConfig {
    pub enabled: bool,
    /// Bind to 0.0.0.0 instead of 127.0.0.1.
    pub allow_network: bool,
    pub receive_port: u16,
    /// Feedback target: an IP address (as the fork's "Target IP Address").
    pub target_host: String,
    pub target_port: u16,
    pub send_state_on_start: bool,
    pub send_peer_levels: bool,
}

impl Default for OscConfig {
    /// The fork's defaults (SonobusPluginProcessor.h:1255-1260), loopback only.
    fn default() -> Self {
        OscConfig {
            enabled: false,
            allow_network: false,
            receive_port: 6000,
            target_host: "127.0.0.1".into(),
            target_port: 6001,
            send_state_on_start: true,
            send_peer_levels: false,
        }
    }
}

fn port(v: i32, default: u16) -> u16 {
    u16::try_from(v).ok().filter(|&p| p > 0).unwrap_or(default)
}

impl OscConfig {
    pub fn load(e: &ExtraState) -> Self {
        let d = OscConfig::default();
        let host = opt::get_string(e, KEY_TARGET_IP);
        OscConfig {
            enabled: opt::get_bool(e, KEY_ENABLED, d.enabled),
            allow_network: opt::get_bool(e, KEY_ALLOW_NETWORK, d.allow_network),
            receive_port: port(opt::get_i32(e, KEY_RECEIVE_PORT, 0), d.receive_port),
            target_host: if host.is_empty() { d.target_host } else { host },
            target_port: port(opt::get_i32(e, KEY_TARGET_PORT, 0), d.target_port),
            send_state_on_start: opt::get_bool(e, KEY_SEND_STATE, d.send_state_on_start),
            send_peer_levels: opt::get_bool(e, KEY_PEER_LEVELS, d.send_peer_levels),
        }
    }

    pub fn store(&self, e: &mut ExtraState) {
        opt::set(e, KEY_ENABLED, TreeVar::Bool(self.enabled));
        opt::set(e, KEY_SEND_STATE, TreeVar::Bool(self.send_state_on_start));
        opt::set(e, KEY_PEER_LEVELS, TreeVar::Bool(self.send_peer_levels));
        opt::set(e, KEY_TARGET_IP, TreeVar::String(self.target_host.clone()));
        opt::set(e, KEY_TARGET_PORT, TreeVar::Int(i32::from(self.target_port)));
        opt::set(e, KEY_RECEIVE_PORT, TreeVar::Int(i32::from(self.receive_port)));
        opt::set(e, KEY_ALLOW_NETWORK, TreeVar::Bool(self.allow_network));
    }

    pub fn bind_addr(&self) -> SocketAddr {
        let ip = if self.allow_network { Ipv4Addr::UNSPECIFIED } else { Ipv4Addr::LOCALHOST };
        SocketAddr::from((ip, self.receive_port))
    }

    pub fn target(&self) -> Option<SocketAddr> {
        let ip: IpAddr = self.target_host.trim().parse().ok()?;
        Some(SocketAddr::new(ip, self.target_port))
    }
}

/// For the UI.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OscStatus {
    pub config: OscConfig,
    /// Where the receiver listens while enabled.
    pub listening: Option<SocketAddr>,
    pub error: Option<String>,
}

// ---- wire codec (OSCManager.cpp) ----------------------------------------------------------------

/// A `juce::var` as the fork hands it to `OSCManager::sendMessage`: `Double` for every C++
/// `float`/`double` source.
#[derive(Debug, Clone, PartialEq)]
pub enum Var {
    Int(i32),
    Double(f64),
    Str(String),
}

fn f(x: f32) -> Var {
    Var::Double(f64::from(x))
}

fn b(on: bool) -> Var {
    Var::Int(i32::from(on))
}

/// `1.0f : 0.0f` (the fork's float-valued toggles).
fn bf(on: bool) -> Var {
    Var::Double(if on { 1.0 } else { 0.0 })
}

fn s(x: &str) -> Var {
    Var::Str(x.to_owned())
}

fn inverted(addr: &str) -> bool {
    addr == "/MainMuteButton" || addr == "/MainRecvMuteButton"
}

/// `OSCManager::sendMessage`: the argument on the wire.
pub fn wire_arg<'a>(addr: &str, v: &'a Var) -> Arg<'a> {
    match v {
        Var::Int(i) if inverted(addr) => Arg::Int(i32::from(*i == 0)),
        Var::Double(d) if inverted(addr) => Arg::Float(if d.abs() < 0.5 { 1.0 } else { 0.0 } * SCALE),
        Var::Int(i) => Arg::Int(*i),
        Var::Double(d) => Arg::Float(*d as f32 * SCALE),
        Var::Str(x) => Arg::Str(x),
    }
}

pub fn encode(out: &mut Vec<u8>, addr: &str, v: &Var) {
    osc::encode(out, addr, [wire_arg(addr, v)]);
}

/// The first argument of an inbound message, as the fork's handlers see it (after
/// `OSCManager::oscMessageReceived`'s inversion).
#[derive(Debug, Clone, PartialEq)]
pub enum InArg {
    /// No argument (`message.size() == 0`: every handler ignores it).
    None,
    Int(i32),
    Float(f32),
    Str(String),
    Blob,
}

/// Unpacks one datagram (message or bundle) into `out`. Malformed input yields nothing (JUCE
/// throws and drops the packet).
pub fn decode_packet(p: &[u8], out: &mut Vec<(String, InArg)>) {
    let mut msgs = Vec::new();
    if unpack(p, 0, &mut msgs).is_some() {
        out.extend(msgs);
    }
}

fn unpack(p: &[u8], depth: u32, out: &mut Vec<(String, InArg)>) -> Option<()> {
    if let Some(rest) = p.strip_prefix(b"#bundle\0") {
        if depth >= 4 || rest.len() < 8 {
            return None;
        }
        let mut rest = &rest[8..]; // time tag: JUCE dispatches at once
        while !rest.is_empty() {
            let (n, r) = rest.split_first_chunk::<4>()?;
            let n = usize::try_from(i32::from_be_bytes(*n)).ok()?;
            if n % 4 != 0 || n > r.len() {
                return None;
            }
            unpack(&r[..n], depth + 1, out)?;
            rest = &r[n..];
        }
        return Some(());
    }
    let m = osc::decode(p).ok()?;
    // JUCE knows only i f s b (and r colours, which it would pass on: we drop them)
    if !m.tags().bytes().all(|t| matches!(t, b'i' | b'f' | b's' | b'b')) {
        return None;
    }
    let arg = match m.args().next() {
        None => InArg::None,
        Some(Arg::Int(i)) if inverted(m.addr) => InArg::Int(i32::from(i == 0)),
        Some(Arg::Float(x)) if inverted(m.addr) => InArg::Float(if x.abs() < 0.5 { 1.0 } else { 0.0 }),
        Some(Arg::Int(i)) => InArg::Int(i),
        Some(Arg::Float(x)) => InArg::Float(x),
        Some(Arg::Str(x)) => InArg::Str(x.to_owned()),
        Some(_) => InArg::Blob,
    };
    out.push((m.addr.to_owned(), arg));
    Some(())
}

// ---- the fork's fader curves (SonobusPluginEditor.cpp:1464-1497) --------------------------------

/// `gainValueToOSCPosition`, cast to float as every caller does.
pub fn gain_to_position(v: f32) -> f32 {
    let v = f64::from(v).clamp(0.0, 2.0);
    ((v / 2.0).sqrt() * 2.0) as f32
}

/// `oscPositionToGainValue`.
pub fn position_to_gain(p: f32) -> f32 {
    let p = f64::from(p).clamp(0.0, 2.0) / 2.0;
    (p * p * 2.0) as f32
}

/// The peer meter value before scaling (SonobusPluginEditor.cpp:5323-5352): mean RMS of the
/// channels, times 10, over 1.2 (a double literal in C++).
pub fn meter_value(m: &MeterLevels) -> f32 {
    let n = m.rms.len();
    let mut avg = 0.0f32;
    if n > 0 {
        for v in &m.rms {
            avg += v;
        }
        avg /= n as f32;
    }
    (f64::from(avg * 10.0) / 1.20) as f32
}

// ---- addresses ----------------------------------------------------------------------------------

/// Every global address the fork registers (`registerAllOSCControls`).
pub const GLOBAL: &[&str] = &[
    "/OutGainSlider", "/MainMuteButton", "/RecvSyncButton", "/OptionsMaxRecvPaddingSlider", "/OptionsRecStealth",
    "/DrySlider", "/MainRecvMuteButton", "/MainPushToTalkButton", "/MetLevelSlider", "/MetEnableButton",
    "/MetTempoSlider", "/RecordingButton", "/BufferMinButton", "/OptionsDynamicResamplingButton",
    "/OptionsAutoReconnectButton", "/OptionsInputLimiterButton", "/OptionsDefaultLevelSlider",
    "/OptionsAutosizeDefaultChoice", "/OptionsFormatChoiceDefaultChoice", "/OptionsMetRecordedButton",
    "/OptionsRecMixButton", "/OptionsRecSelfButton", "/OptionsRecOthersButton", "/OptionsRecMixMinusButton",
    "/MainReverbEnabled", "/ReverbLevelSlider", "/ReverbSizeSlider", "/ReverbDampingSlider", "/ReverbPreDelaySlider",
    "/MetSyncButton", "/MetSyncFileButton", "/InMuteButton", "/InSoloButton", "/OptionsRecSelfPostFxButton",
    "/OptionsRecSelfSilenceMutedButton", "/OptionsDisableShortcutButton", "/OptionsChangeAllFormatButton",
    "/OptionsAutoDropThreshSlider", "/BufferTimeSlider", "/OptionsUseSpecificUdpPortButton",
    "/OptionsOverrideSamplerateButton", "/OptionsShouldCheckForUpdateButton", "/OptionsUdpPortEditor",
    "/OSCTargetIPAddress", "/OSCTargetPort", "/OSCReceivePort", "/OptionsSliderSnapToMouseButton", "/MetSendButton",
    "/FileSendButton", "/SoundboardSendButton", "/PlaybackSlider", "/SoundboardLevelSlider",
    "/SoundboardMonitorSlider", "/SoundboardVolumeSlider", "/SoundboardStopAllPlayback", "/FileMonitorSlider",
    "/MetPanSlider", "/MetMonitorSlider", "/InputReverbLevel", "/InputReverbSize", "/InputReverbDamping",
    "/InputReverbPreDelay", "/InReverbButton", "/MonDelayButton", "/FilePlaybackPreLevel",
];

/// Registered addresses this app has no feature for: accepted and ignored.
pub const IGNORED: &[&str] = &[
    "/OptionsRecStealth",
    "/OptionsOverrideSamplerateButton",
    "/OptionsShouldCheckForUpdateButton",
    "/InReverbButton",
];

/// Effect parameters shared by `/InputGroup<n>...` and `/Peer<n>...`, in the fork's send order.
pub const FX: &[&str] = &[
    "CompressorEnable", "CompressorThreshold", "CompressorRatio", "CompressorAttack", "CompressorRelease",
    "CompressorMakeupGain", "CompressorAuto", "ExpanderEnable", "ExpanderNoiseFloor", "ExpanderRatio",
    "ExpanderAttack", "ExpanderRelease", "EqEnable", "EqLowShelfFreq", "EqLowShelfGain", "EqPara1Freq",
    "EqPara1Gain", "EqPara1Q", "EqHighShelfFreq", "EqHighShelfGain", "EqPara2Freq", "EqPara2Gain", "EqPara2Q",
];

/// `/InputGroup<n><suffix>` (besides [`FX`]).
pub const GROUP: &[&str] = &[
    "PreLevel", "Pan", "PanLeft", "PanRight", "Monitor", "MonDelayEnable", "MonDelayTime", "MonDelayLink",
    "MonReverbSend", "Mute", "Solo", "InputReverbSend", "PolarityInvert",
];

/// `/Peer<n><suffix>` (besides [`FX`]).
pub const PEER: &[&str] = &["Mute", "Solo", "BufferMin", "ResetDrop", "Level", "Pan", "InputReverbSend", "PolarityInvert"];

/// Outbound-only addresses: `/Peer<n>RemotePeerUserName`, `/Peer<n>RecvMeterLevel`,
/// `/Soundboard<n>Name` (and `/Soundboard<n>Track<m>` names).
pub const PEER_OUT: &[&str] = &["RemotePeerUserName", "RecvMeterLevel"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Addr<'a> {
    Global(&'a str),
    /// Group index (0-based), suffix.
    Group(usize, &'a str),
    Peer(usize, &'a str),
    /// Board and sample index (0-based).
    Track(usize, usize),
}

/// `<prefix><1..16><rest>` exactly as the fork spells it (no leading zeros).
fn indexed<'a>(addr: &'a str, prefix: &str) -> Option<(usize, &'a str)> {
    let rest = addr.strip_prefix(prefix)?;
    let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
    let n: usize = rest[..digits].parse().ok()?;
    if !(1..=SLOTS).contains(&n) || rest[..digits] != n.to_string() {
        return None;
    }
    Some((n - 1, &rest[digits..]))
}

/// An address the fork listens to.
pub fn parse(addr: &str) -> Option<Addr<'_>> {
    if let Some(g) = GLOBAL.iter().find(|g| **g == addr) {
        return Some(Addr::Global(g));
    }
    if let Some((i, suffix)) = indexed(addr, "/InputGroup")
        && (GROUP.contains(&suffix) || FX.contains(&suffix))
    {
        return Some(Addr::Group(i, suffix));
    }
    if let Some((i, suffix)) = indexed(addr, "/Peer")
        && (PEER.contains(&suffix) || FX.contains(&suffix))
    {
        return Some(Addr::Peer(i, suffix));
    }
    let (b, rest) = indexed(addr, "/Soundboard")?;
    let (t, rest) = indexed(rest, "Track")?;
    rest.is_empty().then_some(Addr::Track(b, t))
}

/// Any address of the fork's protocol, either direction.
pub fn is_known(addr: &str) -> bool {
    parse(addr).is_some()
        || indexed(addr, "/Peer").is_some_and(|(_, x)| PEER_OUT.contains(&x))
        || indexed(addr, "/Soundboard").is_some_and(|(_, x)| x == "Name")
}

// ---- inbound handlers -------------------------------------------------------------------------

/// `if int: != 0, else if float: != 0, else false` (the fork's toggle handlers).
fn any_bool(a: &InArg) -> Option<bool> {
    match a {
        InArg::None => None,
        InArg::Int(i) => Some(*i != 0),
        InArg::Float(x) => Some(*x != 0.0),
        _ => Some(false),
    }
}

fn float(a: &InArg) -> Option<f32> {
    match a {
        InArg::Float(x) if x.is_finite() => Some(*x),
        _ => None,
    }
}

fn int(a: &InArg) -> Option<i32> {
    match a {
        InArg::Int(i) => Some(*i),
        _ => None,
    }
}

fn clamp(x: f32, spec: ParamSpec) -> f32 {
    x.clamp(spec.min, spec.max)
}

/// A float effect parameter by its address suffix, with its range.
fn fx_value<'a>(g: &'a mut ChannelGroup, name: &str) -> Option<(&'a mut f32, ParamSpec)> {
    let (c, x, e) = (&mut g.compressor, &mut g.expander, &mut g.eq);
    Some(match name {
        "CompressorThreshold" => (&mut c.threshold_db, Compressor::THRESHOLD_DB),
        "CompressorRatio" => (&mut c.ratio, Compressor::RATIO),
        "CompressorAttack" => (&mut c.attack_ms, Compressor::ATTACK_MS),
        "CompressorRelease" => (&mut c.release_ms, Compressor::RELEASE_MS),
        "CompressorMakeupGain" => (&mut c.makeup_gain_db, Compressor::MAKEUP_GAIN_DB),
        "ExpanderNoiseFloor" => (&mut x.threshold_db, Expander::THRESHOLD_DB),
        "ExpanderRatio" => (&mut x.ratio, Expander::RATIO),
        "ExpanderAttack" => (&mut x.attack_ms, Expander::ATTACK_MS),
        "ExpanderRelease" => (&mut x.release_ms, Expander::RELEASE_MS),
        "EqLowShelfFreq" => (&mut e.low_shelf_freq, ParametricEq::LOW_SHELF_FREQ_HZ),
        "EqLowShelfGain" => (&mut e.low_shelf_gain, ParametricEq::LOW_SHELF_GAIN_DB),
        "EqPara1Freq" => (&mut e.para1_freq, ParametricEq::PARA1_FREQ_HZ),
        "EqPara1Gain" => (&mut e.para1_gain, ParametricEq::PARA1_GAIN_DB),
        "EqPara1Q" => (&mut e.para1_q, ParametricEq::PARA1_Q),
        "EqHighShelfFreq" => (&mut e.high_shelf_freq, ParametricEq::HIGH_SHELF_FREQ_HZ),
        "EqHighShelfGain" => (&mut e.high_shelf_gain, ParametricEq::HIGH_SHELF_GAIN_DB),
        "EqPara2Freq" => (&mut e.para2_freq, ParametricEq::PARA2_FREQ_HZ),
        "EqPara2Gain" => (&mut e.para2_gain, ParametricEq::PARA2_GAIN_DB),
        "EqPara2Q" => (&mut e.para2_q, ParametricEq::PARA2_Q),
        _ => return None,
    })
}

fn fx_flag<'a>(g: &'a mut ChannelGroup, name: &str) -> Option<&'a mut bool> {
    Some(match name {
        "CompressorEnable" => &mut g.compressor.enabled,
        "CompressorAuto" => &mut g.compressor.auto_makeup,
        "ExpanderEnable" => &mut g.expander.enabled,
        "EqEnable" => &mut g.eq.enabled,
        "PolarityInvert" => &mut g.invert_polarity,
        _ => return None,
    })
}

/// Sets an effect parameter or flag from an already type-checked value; keeps the auto makeup
/// gain current as the effects page does.
fn set_fx(g: &mut ChannelGroup, name: &str, v: f32, on: bool) {
    if let Some(flag) = fx_flag(g, name) {
        *flag = on;
    } else if let Some((x, spec)) = fx_value(g, name) {
        *x = clamp(v, spec);
    }
    let c = &mut g.compressor;
    if c.auto_makeup {
        c.makeup_gain_db = sonobus_dsp::auto_makeup_gain_db(c.threshold_db, c.ratio);
    }
}

/// `pan[chan]`, or `panStereo[chan]` for a 2-channel group (P.cpp:2017-2027, 4933-4970).
fn pan_mut(g: &mut ChannelGroup, chan: usize) -> Option<&mut f32> {
    if g.num_channels == 2 && chan < 2 { Some(&mut g.pan_stereo[chan]) } else { g.pans.get_mut(chan) }
}

fn pan(g: &ChannelGroup, chan: usize) -> f32 {
    if g.num_channels == 2 && chan < 2 { g.pan_stereo[chan] } else { g.pans.get(chan).copied().unwrap_or(0.0) }
}

const GAIN: (f32, f32) = (0.0, 2.0);

/// Applies one inbound message (the fork's handler for `addr`, mapped onto this app).
pub fn apply(c: &mut AppController, addr: &str, a: &InArg) {
    match parse(addr) {
        Some(Addr::Global(g)) => global(c, g, a),
        Some(Addr::Group(i, x)) => group(c, i, x, a),
        Some(Addr::Peer(i, x)) => peer(c, i, x, a),
        Some(Addr::Track(bi, ti)) => {
            // float >= 0.5 triggers (SoundboardView::triggerSampleBySoundboardAndTrackIndex)
            let exists = c.vm().soundboards.boards.get(bi).is_some_and(|bd| ti < bd.samples.len());
            if float(a).is_some_and(|v| v >= 0.5)
                && exists
                && let Err(e) = c.engine().soundboard_press((bi, ti))
            {
                c.set_status(format!("{}: {e}", tr("Cannot play file")), true);
            }
        }
        None => {}
    }
}

fn param(c: &mut AppController, id: &'static str, v: f32) {
    c.edit(move |s| {
        s.params.set_by_id(id, v);
    });
}

fn global(c: &mut AppController, addr: &str, a: &InArg) {
    let name = &addr[1..];
    // toggles: int != 0 / float != 0 / other false
    let bool_param = |n: &str| -> Option<&'static str> {
        Some(match n {
            "MainMuteButton" => "mastsendmute",
            "MainRecvMuteButton" => "mastrecvmute",
            "MetEnableButton" => "metenabled",
            "OptionsDynamicResamplingButton" => "dynamicresampling",
            "OptionsAutoReconnectButton" => "reconnectlast",
            "OptionsMetRecordedButton" => "metisrecorded",
            "MainReverbEnabled" => "mainreverbenabled",
            "MetSyncButton" => "syncMetHost",
            "MetSyncFileButton" => "syncMetFile",
            "InMuteButton" => "mastinmute",
            "InSoloButton" => "mastmonsolo",
            "MetSendButton" => "sendmetaudio",
            "FileSendButton" => "sendfileaudio",
            "SoundboardSendButton" => "sendsoundboardaudio",
            _ => return None,
        })
    };
    // raw floats into a parameter (clamped and snapped like APVTS / the slider)
    let float_param = |n: &str| -> Option<&'static str> {
        Some(match n {
            "MetLevelSlider" => "metgain",
            "MetTempoSlider" => "mettempo",
            "OptionsDefaultLevelSlider" => "defPeerLevel",
            "ReverbLevelSlider" => "nmainreverblevel",
            "ReverbSizeSlider" => "mainreverbsize",
            "ReverbDampingSlider" => "mainreverbdamp",
            "ReverbPreDelaySlider" => "mainreverbpredelay",
            "BufferTimeSlider" => "defnetbuf",
            "InputReverbLevel" => "inreverblevel",
            "InputReverbSize" => "inreverbsize",
            "InputReverbDamping" => "inreverbdamp",
            "InputReverbPreDelay" => "inreverbpredelay",
            _ => return None,
        })
    };
    if let Some(id) = bool_param(name) {
        if let Some(on) = any_bool(a) {
            param(c, id, f32::from(u8::from(on)));
        }
        return;
    }
    if let Some(id) = float_param(name) {
        if let Some(v) = float(a) {
            param(c, id, v);
        }
        return;
    }
    let rec_bit = |n: &str| match n {
        "OptionsRecMixButton" => 1,
        "OptionsRecSelfButton" => 2,
        "OptionsRecMixMinusButton" => 4,
        "OptionsRecOthersButton" => 8,
        _ => 0,
    };
    if rec_bit(name) != 0 {
        if let Some(on) = any_bool(a) {
            let bit = rec_bit(name);
            let old = c.vm().state.extra.default_recording_options;
            let mut mask = if on { old | bit } else { old & !bit };
            if mask == 0 {
                mask = 1;
            }
            c.set_recording_options(mask);
        }
        return;
    }
    match name {
        "OutGainSlider" => {
            if let Some(v) = float(a) {
                param(c, "wet", position_to_gain(v * INVERSE));
            }
        }
        "DrySlider" => {
            if let Some(v) = float(a) {
                param(c, "dry", v * INVERSE);
            }
        }
        "RecvSyncButton" | "BufferMinButton" => {
            // float only, `(int) value == 1`
            if float(a).is_some_and(|v| v as i32 == 1) {
                if name == "RecvSyncButton" {
                    let pad = c.engine().receive_sync_padding();
                    c.engine().receive_sync(pad);
                } else {
                    c.engine().reset_all_jitter_buffers();
                }
                c.osc_event(addr, Var::Int(1));
            }
        }
        "OptionsMaxRecvPaddingSlider" => {
            if let Some(v) = float(a) {
                c.engine_edit(|e| e.set_receive_sync_padding(v.round()));
            }
        }
        "MainPushToTalkButton" => match int(a) {
            Some(1) => c.push_to_talk(true),
            Some(0) => c.push_to_talk(false),
            _ => {}
        },
        "RecordingButton" => {
            if let Some(on) = any_bool(a)
                && on != c.vm().recording.is_some()
            {
                c.toggle_recording();
            }
        }
        "OptionsInputLimiterButton" => {
            if let Some(on) = any_bool(a) {
                c.edit(move |s| s.input_groups.iter_mut().for_each(|g| g.limiter.enabled = on));
            }
        }
        "OptionsAutosizeDefaultChoice" | "OptionsFormatChoiceDefaultChoice" => {
            if let Some(v) = int(a) {
                let id = if name == "OptionsAutosizeDefaultChoice" { "defnetauto" } else { "defsendqual" };
                param(c, id, v as f32);
            }
        }
        "OptionsRecSelfPostFxButton" => {
            if let Some(on) = any_bool(a) {
                c.edit(move |s| s.extra.record_self_pre_fx = !on);
            }
        }
        "OptionsRecSelfSilenceMutedButton" => {
            if let Some(on) = any_bool(a) {
                c.edit(move |s| s.extra.record_self_silence_when_muted = on);
            }
        }
        "OptionsDisableShortcutButton" => {
            if let Some(on) = any_bool(a) {
                c.edit(move |s| s.extra.disable_key_shortcuts = on);
            }
        }
        "OptionsChangeAllFormatButton" => {
            if let Some(on) = any_bool(a) {
                c.edit(move |s| s.extra.change_qual_for_all = on);
            }
        }
        "OptionsSliderSnapToMouseButton" => {
            if let Some(on) = any_bool(a) {
                c.edit(move |s| s.extra.slider_snap_to_mouse = on);
            }
        }
        "OptionsAutoDropThreshSlider" => {
            if let Some(v) = float(a) {
                let thresh = (1.0 / f64::from(v).max(1.0)) as f32;
                c.edit(move |s| s.extra.auto_drop_rate_thresh = thresh);
            }
        }
        "OptionsUseSpecificUdpPortButton" => {
            // `setUseSpecificUdpPort(state ? current : 0)`
            if any_bool(a) == Some(false) {
                c.edit(|s| s.extra.use_udp_port = 0);
            }
        }
        "OptionsUdpPortEditor" => {
            if let Some(p) = int(a).filter(|p| (0..=65535).contains(p)) {
                c.edit(move |s| s.extra.use_udp_port = p);
            }
        }
        // OSC's own configuration only from the local UI: over OSC anyone who reaches the port
        // could send the feedback elsewhere or close the port (the fork allows both)
        "OSCTargetIPAddress" | "OSCTargetPort" | "OSCReceivePort" => {}
        "PlaybackSlider" | "FilePlaybackPreLevel" => {
            if let Some(v) = float(a) {
                c.edit(move |s| s.file_playback_group.gain = v.clamp(GAIN.0, GAIN.1));
            }
        }
        "FileMonitorSlider" => {
            if let Some(v) = float(a) {
                c.edit(move |s| s.file_playback_group.monitor = v.clamp(GAIN.0, GAIN.1));
            }
        }
        "SoundboardLevelSlider" => {
            if let Some(v) = float(a) {
                c.edit(move |s| s.soundboard_group.gain = v.clamp(GAIN.0, GAIN.1));
            }
        }
        "SoundboardMonitorSlider" => {
            if let Some(v) = float(a) {
                c.edit(move |s| s.soundboard_group.monitor = v.clamp(GAIN.0, GAIN.1));
            }
        }
        "SoundboardVolumeSlider" => {
            if let Some(v) = float(a) {
                let g = position_to_gain(v * INVERSE);
                c.edit(move |s| s.soundboard_group.gain = g);
            }
        }
        "SoundboardStopAllPlayback" => {
            if float(a).is_some_and(|v| v >= 0.5) {
                c.engine().stop_all_samples();
            }
        }
        "MetPanSlider" => {
            if let Some(v) = float(a) {
                c.edit(move |s| {
                    if let Some(p) = s.metronome_group.pans.first_mut() {
                        *p = v.clamp(-1.0, 1.0);
                    }
                });
            }
        }
        "MetMonitorSlider" => {
            if let Some(v) = float(a) {
                c.edit(move |s| s.metronome_group.monitor = v.clamp(GAIN.0, GAIN.1));
            }
        }
        "MonDelayButton" => {
            // ChannelGroupsView::toggleAllMonitorDelay when the state differs
            if let Some(on) = any_bool(a)
                && on != any_mon_delay(&c.vm().state)
            {
                c.edit(move |s| {
                    let groups = s.input_groups.iter_mut();
                    let extra = [&mut s.metronome_group, &mut s.file_playback_group, &mut s.soundboard_group];
                    groups.chain(extra).for_each(|g| g.monitor_delay.enabled = on);
                });
            }
        }
        _ => {} // IGNORED
    }
}

fn any_mon_delay(s: &sonobus_core::state::AppState) -> bool {
    s.input_groups.iter().chain([&s.metronome_group, &s.file_playback_group, &s.soundboard_group]).any(|g| g.monitor_delay.enabled)
}

fn group(c: &mut AppController, i: usize, name: &str, a: &InArg) {
    if i >= c.vm().state.input_groups.len() {
        return;
    }
    // input group toggles: float > 0.5
    let flag = match a {
        InArg::Float(x) => *x > 0.5,
        _ => return,
    };
    let Some(v) = float(a) else { return };
    match name {
        "Solo" => return c.set_input_soloed(i, flag),
        "MonDelayLink" => return c.edit(move |s| s.extra.link_mon_delay_times = flag),
        _ => {}
    }
    let name = name.to_owned();
    c.edit(move |s| {
        let Some(g) = s.input_groups.get_mut(i) else { return };
        match name.as_str() {
            "PreLevel" => g.gain = v.clamp(GAIN.0, GAIN.1),
            "Pan" | "PanLeft" | "PanRight" => {
                if let Some(p) = pan_mut(g, usize::from(name == "PanRight")) {
                    *p = v.clamp(-1.0, 1.0);
                }
            }
            "Monitor" => g.monitor = v.clamp(GAIN.0, GAIN.1),
            "MonDelayEnable" => g.monitor_delay.enabled = flag,
            "MonDelayTime" => g.monitor_delay.delay_time_ms = clamp(v, sonobus_dsp::MonitorDelay::DELAY_MS),
            "MonReverbSend" => g.mon_reverb_send = v.clamp(0.0, 1.0),
            "InputReverbSend" => g.in_reverb_send = v.clamp(0.0, 1.0),
            "Mute" => g.muted = flag,
            fx => set_fx(g, fx, v, flag),
        }
    });
}

fn peer(c: &mut AppController, i: usize, name: &str, a: &InArg) {
    let Some(id) = c.vm().snapshot.peers.get(i).map(|p| p.id) else { return };
    match name {
        // float != 0
        "Mute" => {
            if let Some(v) = float(a) {
                c.engine().set_peer_recv_muted(id, v != 0.0);
            }
        }
        "Solo" => {
            if let Some(v) = float(a) {
                c.engine().set_peer_soloed(id, v != 0.0);
            }
        }
        // int != 0 triggers, echoed as int 1
        "BufferMin" | "ResetDrop" => {
            if int(a).is_some_and(|v| v != 0) {
                if name == "BufferMin" {
                    c.engine().set_peer_buffer_ms(id, 0.0);
                } else {
                    c.engine().reset_peer_drop_count(id);
                }
                c.osc_event(&format!("/Peer{}{name}", i + 1), Var::Int(1));
            }
        }
        "Level" => {
            if let Some(v) = float(a) {
                c.engine().set_peer_level(id, position_to_gain(v * INVERSE));
            }
        }
        _ => {
            // group 0 of the peer: floats raw (Pan times 2), flags int != 0
            let (v, on) = match a {
                InArg::Float(x) if x.is_finite() => (*x, false),
                InArg::Int(n) => (0.0, *n != 0),
                _ => return,
            };
            let is_flag = matches!(name, "PolarityInvert" | "CompressorEnable" | "CompressorAuto" | "ExpanderEnable" | "EqEnable");
            if is_flag != matches!(a, InArg::Int(_)) {
                return;
            }
            let name = name.to_owned();
            c.engine().update_peer_groups(id, move |gs, _| {
                let Some(g) = gs.first_mut() else { return };
                match name.as_str() {
                    "Pan" => {
                        if let Some(p) = pan_mut(g, 0) {
                            *p = (v * INVERSE).clamp(-1.0, 1.0);
                        }
                    }
                    "InputReverbSend" => g.mon_reverb_send = v.clamp(0.0, 1.0), // sic: P.cpp:4776-4783
                    fx => set_fx(g, fx, v, on),
                }
            });
        }
    }
}

// ---- outbound state ---------------------------------------------------------------------------

/// What the full-state dump (`sendAllOSCState`) sends for an entry.
#[derive(Debug, Clone, PartialEq)]
pub enum Dump {
    No,
    Same,
    /// The dump sends something else than the change notification (a fork quirk).
    As(Var),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fb {
    pub addr: String,
    pub val: Var,
    pub dump: Dump,
}

fn fx_feedback(out: &mut Vec<Fb>, prefix: &str, g: &ChannelGroup, flag: fn(bool) -> Var, dump: &Dump) {
    let mut g = g.clone();
    for name in FX {
        let val = match fx_flag(&mut g, name) {
            Some(on) => flag(*on),
            None => f(*fx_value(&mut g, name).map(|(v, _)| v).expect("fx name")),
        };
        out.push(Fb { addr: format!("{prefix}{name}"), val, dump: dump.clone() });
    }
}

/// Every stateful address and its current value, in `sendAllOSCState` order for the dumped
/// ones. `rx_padding`: the Receive Sync padding; `ptt`: push-to-talk held.
pub fn state(c: &AppController, rx_padding: f32, ptt: bool) -> Vec<Fb> {
    let vm = c.vm();
    let (st, p, x) = (&vm.state, &vm.state.params, &vm.state.extra);
    let mut out = Vec::with_capacity(1100);
    let mut push = |addr: &str, val: Var, dump: Dump| out.push(Fb { addr: addr.to_owned(), val, dump });
    use Dump::{No, Same};
    let rec = x.default_recording_options;
    let (file, sb, met) = (&st.file_playback_group, &st.soundboard_group, &st.metronome_group);
    push("/OutGainSlider", f(gain_to_position(p.wet)), Same);
    push("/DrySlider", f(p.dry), Same);
    push("/MainMuteButton", b(p.main_send_mute), Same);
    push("/MainRecvMuteButton", b(p.main_recv_mute), Same);
    push("/MainPushToTalkButton", b(ptt), Same);
    push("/InMuteButton", b(p.main_in_mute), Same);
    push("/InSoloButton", b(p.main_monitor_solo), Same);
    push("/RecordingButton", b(vm.recording.is_some()), Same);
    push("/BufferTimeSlider", f(p.def_net_buf), Same);
    push("/MetLevelSlider", f(p.met_gain), Same);
    push("/MetEnableButton", b(p.met_enabled), Same);
    push("/MetTempoSlider", f(p.met_tempo), Same);
    push("/OptionsMetRecordedButton", b(p.met_is_recorded), Same);
    push("/MetSendButton", b(p.send_met_audio), Same);
    push("/MetSyncFileButton", b(p.sync_met_file), Same);
    push("/FileSendButton", b(p.send_file_audio), Same);
    push("/PlaybackSlider", f(file.gain), Same);
    push("/FilePlaybackPreLevel", f(file.gain), Same);
    push("/FileMonitorSlider", f(file.monitor), Same);
    push("/SoundboardSendButton", b(p.send_soundboard_audio), Same);
    push("/SoundboardLevelSlider", f(sb.gain), Same);
    push("/SoundboardMonitorSlider", f(sb.monitor), Same);
    push("/SoundboardVolumeSlider", f(gain_to_position(sb.gain)), Same);
    push("/MetPanSlider", f(met.pans.first().copied().unwrap_or(0.0)), Same);
    push("/MetMonitorSlider", f(met.monitor), Same);
    push("/InputReverbLevel", f(p.in_reverb_level), Same);
    push("/InputReverbSize", f(p.in_reverb_size), Same);
    push("/InputReverbDamping", f(p.in_reverb_damp), Same);
    push("/InputReverbPreDelay", f(p.in_reverb_predelay), Same);
    push("/MainReverbEnabled", b(p.main_reverb_enabled), Same);
    push("/ReverbLevelSlider", f(p.main_reverb_level), Same);
    push("/ReverbSizeSlider", f(p.main_reverb_size), Same);
    push("/ReverbDampingSlider", f(p.main_reverb_damp), Same);
    push("/ReverbPreDelaySlider", f(p.main_reverb_predelay), Same);
    push("/OptionsMaxRecvPaddingSlider", f(rx_padding), Same);
    push("/OptionsDynamicResamplingButton", b(p.dynamic_resampling), Same);
    push("/OptionsAutoReconnectButton", b(p.reconnect_last), Same);
    // the dump sends paramDefaultNetbufMs here, and the normalised choice truncated to int
    push("/OptionsDefaultLevelSlider", f(p.def_peer_level), Dump::As(f(p.def_net_buf)));
    push("/OptionsAutosizeDefaultChoice", Var::Int(p.def_net_auto), Dump::As(Var::Int(i32::from(p.def_net_auto >= 3))));
    push("/OptionsRecMixButton", b(rec & 1 != 0), Same);
    push("/OptionsRecMixMinusButton", b(rec & 4 != 0), Same);
    push("/OptionsRecSelfButton", b(rec & 2 != 0), Same);
    push("/OptionsRecOthersButton", b(rec & 8 != 0), Same);
    push("/OptionsRecSelfPostFxButton", b(!x.record_self_pre_fx), Same);
    push("/OptionsRecSelfSilenceMutedButton", b(x.record_self_silence_when_muted), Same);
    // change notifications only
    push("/MetSyncButton", b(p.sync_met_host), No);
    push("/OptionsFormatChoiceDefaultChoice", Var::Int(p.def_send_qual), No);
    push("/OptionsInputLimiterButton", b(st.input_groups.iter().any(|g| g.limiter.enabled)), No);
    push("/OptionsDisableShortcutButton", b(x.disable_key_shortcuts), No);
    push("/OptionsChangeAllFormatButton", b(x.change_qual_for_all), No);
    push("/OptionsSliderSnapToMouseButton", b(x.slider_snap_to_mouse), No);
    let drop_slider = (1.0 / f64::from(x.auto_drop_rate_thresh).max(0.001)).clamp(1.0, 20.0) as f32;
    push("/OptionsAutoDropThreshSlider", f(drop_slider), No);
    push("/OptionsUseSpecificUdpPortButton", b(x.use_udp_port != 0), No);
    push("/OptionsUdpPortEditor", Var::Int(x.use_udp_port), No);
    let o = &vm.osc.config;
    push("/OSCTargetIPAddress", s(&o.target_host), No);
    push("/OSCTargetPort", Var::Int(i32::from(o.target_port)), No);
    push("/OSCReceivePort", Var::Int(i32::from(o.receive_port)), No);
    push("/MonDelayButton", b(any_mon_delay(st)), No);
    for (i, g) in st.input_groups.iter().enumerate().take(SLOTS) {
        let pre = format!("/InputGroup{}", i + 1);
        let mut push = |name: &str, val: Var| out.push(Fb { addr: format!("{pre}{name}"), val, dump: No });
        push("PreLevel", f(g.gain));
        if g.num_channels == 2 {
            push("PanLeft", f(g.pan_stereo[0]));
            push("PanRight", f(g.pan_stereo[1]));
        } else {
            push("Pan", f(pan(g, 0)));
        }
        push("Monitor", f(g.monitor));
        push("MonDelayEnable", bf(g.monitor_delay.enabled));
        push("MonDelayTime", f(g.monitor_delay.delay_time_ms));
        push("MonDelayLink", bf(x.link_mon_delay_times));
        push("MonReverbSend", f(g.mon_reverb_send));
        push("Mute", bf(g.muted));
        push("Solo", bf(vm.input_solo.get(i).copied().unwrap_or(false)));
        push("InputReverbSend", f(g.in_reverb_send));
        push("PolarityInvert", bf(g.invert_polarity));
        fx_feedback(&mut out, &pre, g, bf, &No);
    }
    for i in 0..SLOTS {
        peer_state(&mut out, i, vm.snapshot.peers.get(i));
    }
    for bi in 0..SLOTS {
        let board = vm.soundboards.boards.get(bi);
        let name = board.map_or("", |bd| bd.name.as_str());
        out.push(Fb { addr: format!("/Soundboard{}Name", bi + 1), val: s(name), dump: Same });
        for ti in 0..SLOTS {
            let t = board.and_then(|bd| bd.samples.get(ti)).map_or("", |x| x.name.as_str());
            out.push(Fb { addr: format!("/Soundboard{}Track{}", bi + 1, ti + 1), val: s(t), dump: Same });
        }
    }
    out
}

/// `sendPeerOSCState` for a connected peer, `clearPeerOSCState` for an empty slot.
fn peer_state(out: &mut Vec<Fb>, i: usize, peer: Option<&PeerStatus>) {
    let pre = format!("/Peer{}", i + 1);
    let mut push = |name: &str, val: Var| out.push(Fb { addr: format!("{pre}{name}"), val, dump: Dump::Same });
    match peer {
        Some(p) => {
            let g0 = p.groups.first().cloned().unwrap_or_default();
            push("RemotePeerUserName", s(&p.user));
            push("Mute", Var::Double(if p.recv_allow { 0.0 } else { 1.0 }));
            push("Solo", Var::Double(if p.soloed { 1.0 } else { 0.0 }));
            push("Level", f(gain_to_position(p.level)));
            push("Pan", f(pan(&g0, 0)));
            let polarity = g0.invert_polarity;
            let rev = g0.mon_reverb_send;
            fx_feedback(out, &pre, &g0, b, &Dump::Same);
            out.push(Fb { addr: format!("{pre}InputReverbSend"), val: f(rev), dump: Dump::Same });
            out.push(Fb { addr: format!("{pre}PolarityInvert"), val: b(polarity), dump: Dump::Same });
        }
        None => {
            push("RemotePeerUserName", s(""));
            push("Mute", Var::Double(0.0));
            push("Solo", Var::Double(0.0));
            push("Level", Var::Int(0));
            push("Pan", f(0.0));
            push("RecvMeterLevel", f(0.0));
            for name in FX {
                let val = match *name {
                    "CompressorEnable" | "CompressorAuto" | "ExpanderEnable" | "EqEnable" => Var::Int(0),
                    "CompressorRatio" | "ExpanderRatio" => f(1.0),
                    _ => f(0.0),
                };
                out.push(Fb { addr: format!("{pre}{name}"), val, dump: Dump::Same });
            }
            out.push(Fb { addr: format!("{pre}InputReverbSend"), val: f(0.0), dump: Dump::Same });
            out.push(Fb { addr: format!("{pre}PolarityInvert"), val: Var::Int(0), dump: Dump::Same });
        }
    }
}

/// What the fork sends when its window closes: soundboard slots cleared, two faders down.
pub fn closing_messages() -> Vec<(String, Var)> {
    let mut v = Vec::new();
    for bi in 1..=SLOTS {
        v.push((format!("/Soundboard{bi}Name"), s("")));
        for ti in 1..=SLOTS {
            v.push((format!("/Soundboard{bi}Track{ti}"), s("")));
        }
    }
    v.push(("/SoundboardVolumeSlider".into(), f(0.0)));
    v.push(("/OutGainSlider".into(), f(0.0)));
    v
}

// ---- socket -----------------------------------------------------------------------------------

struct Listener {
    stop: Arc<AtomicBool>,
    /// Asked for / bound to (differ for port 0).
    bind: SocketAddr,
    addr: SocketAddr,
}

impl Drop for Listener {
    /// Never waits (it runs on the UI thread): the thread is told to stop and woken with an
    /// empty datagram; it ends and frees the port on its own.
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        let wake = SocketAddr::from((Ipv4Addr::LOCALHOST, self.addr.port()));
        if let Ok(s) = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)) {
            let _ = s.send_to(&[], wake);
        }
    }
}

/// The inbound rate limit's window: (start, messages so far). Shared by every listener of an
/// [`Osc`], so a rebind does not start a fresh second.
type RateWindow = Arc<std::sync::Mutex<(Instant, u32)>>;

fn listen(bind: SocketAddr, tx: SyncSender<(String, InArg)>, rate: RateWindow) -> io::Result<Listener> {
    let sock = UdpSocket::bind(bind)?;
    sock.set_read_timeout(Some(Duration::from_millis(100)))?;
    let addr = sock.local_addr()?;
    let stop = Arc::new(AtomicBool::new(false));
    let stop2 = stop.clone();
    std::thread::Builder::new().name("osc-control".into()).spawn(move || {
        let mut buf = vec![0u8; MAX_PACKET + 1];
        let mut msgs = Vec::new();
        while !stop2.load(Ordering::Relaxed) {
            let n = match sock.recv_from(&mut buf) {
                Ok((n, _)) => n,
                Err(_) => continue, // timeout (or a transient error)
            };
            if n > MAX_PACKET {
                continue;
            }
            msgs.clear();
            decode_packet(&buf[..n], &mut msgs);
            for m in msgs.drain(..) {
                {
                    let mut w = rate.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                    if w.0.elapsed() >= Duration::from_secs(1) {
                        *w = (Instant::now(), 0);
                    }
                    w.1 += 1;
                    if w.1 > MAX_RATE {
                        continue;
                    }
                }
                if let Err(TrySendError::Disconnected(_)) = tx.try_send(m) {
                    return;
                }
            }
        }
    })?;
    Ok(Listener { stop, bind, addr })
}

/// The OSC side of the controller: listener, feedback sender and the last sent state.
pub struct Osc {
    cfg: OscConfig,
    listener: Option<Listener>,
    tx: SyncSender<(String, InArg)>,
    rx: Receiver<(String, InArg)>,
    sender: Option<UdpSocket>,
    error: Option<String>,
    prev: HashMap<String, Var>,
    events: Vec<(String, Var)>,
    out: VecDeque<(String, Var)>,
    dump_pending: bool,
    last_feedback: Option<Instant>,
    last_meter: Option<Instant>,
    buf: Vec<u8>,
    rate: RateWindow,
    /// A rebind waiting for the old listener to free the port: since when.
    rebind: Option<Instant>,
}

impl Default for Osc {
    fn default() -> Self {
        let (tx, rx) = sync_channel(QUEUE);
        Osc {
            cfg: OscConfig::default(),
            listener: None,
            tx,
            rx,
            sender: None,
            error: None,
            prev: HashMap::new(),
            events: Vec::new(),
            out: VecDeque::new(),
            dump_pending: false,
            last_feedback: None,
            last_meter: None,
            buf: Vec::new(),
            rate: Arc::new(std::sync::Mutex::new((Instant::now(), 0))),
            rebind: None,
        }
    }
}

impl Osc {
    pub fn status(&self) -> OscStatus {
        OscStatus { config: self.cfg.clone(), listening: self.listener.as_ref().map(|l| l.addr), error: self.error.clone() }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Applies a configuration: (re)binds the receiver when the bind address changed, drops
    /// everything when disabled. `dump`: send the full state on the next feedback.
    pub fn configure(&mut self, cfg: OscConfig, dump: bool) {
        self.error = None;
        if !cfg.enabled {
            self.listener = None;
            self.sender = None;
            self.out.clear();
        } else {
            if self.listener.as_ref().is_none_or(|l| l.bind != cfg.bind_addr()) {
                // its thread frees the port shortly; a pending rebind means an older listener
                // may still be letting go of the port we are going back to
                let had = self.listener.take().is_some() | self.rebind.take().is_some();
                match listen(cfg.bind_addr(), self.tx.clone(), self.rate.clone()) {
                    Ok(l) => self.listener = Some(l),
                    Err(e) if had && e.kind() == io::ErrorKind::AddrInUse => self.rebind = Some(Instant::now()),
                    Err(e) => self.error = Some(format!("{} {}: {e}", tr("Cannot listen on"), cfg.bind_addr())),
                }
            }
            if self.sender.is_none() {
                self.sender = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).ok();
            }
            if cfg.target().is_none() {
                self.error = Some(format!("{}: {}", tr("Invalid feedback IP address"), cfg.target_host));
            }
        }
        self.dump_pending |= dump && cfg.enabled;
        self.cfg = cfg;
    }

    /// Inbound messages received since the last call. Also finishes a rebind that waited
    /// for the old listener to let go of the port (up to a second).
    pub fn take_inbound(&mut self) -> Vec<(String, InArg)> {
        if let Some(t) = self.rebind {
            let bind = self.cfg.bind_addr();
            match listen(bind, self.tx.clone(), self.rate.clone()) {
                Ok(l) => (self.listener, self.rebind) = (Some(l), None),
                Err(e) if t.elapsed() >= Duration::from_secs(1) || e.kind() != io::ErrorKind::AddrInUse => {
                    self.rebind = None;
                    self.error = Some(format!("{} {bind}: {e}", tr("Cannot listen on")));
                }
                Err(_) => {}
            }
        }
        self.rx.try_iter().collect()
    }

    /// One-off messages (button echoes) for the next feedback.
    pub fn event(&mut self, addr: &str, v: Var) {
        if self.cfg.enabled {
            self.events.push((addr.to_owned(), v));
        }
    }

    pub fn send(&mut self, addr: &str, v: &Var) {
        let (Some(sock), Some(to)) = (&self.sender, self.cfg.target()) else { return };
        self.buf.clear();
        encode(&mut self.buf, addr, v);
        let _ = sock.send_to(&self.buf, to);
    }

    /// Whether [`Self::feedback`] is due.
    pub fn feedback_due(&self, now: Instant) -> bool {
        self.cfg.enabled && self.last_feedback.is_none_or(|t| now - t >= FEEDBACK_PERIOD)
    }

    /// Queues the pending dump, the events and every entry that changed since the last call,
    /// then sends up to [`BURST`] queued messages.
    pub fn feedback(&mut self, now: Instant, entries: Vec<Fb>) {
        self.last_feedback = Some(now);
        if std::mem::take(&mut self.dump_pending) {
            for e in &entries {
                match &e.dump {
                    Dump::No => {}
                    Dump::Same => self.out.push_back((e.addr.clone(), e.val.clone())),
                    Dump::As(v) => self.out.push_back((e.addr.clone(), v.clone())),
                }
            }
            self.prev = entries.iter().map(|e| (e.addr.clone(), e.val.clone())).collect();
        }
        self.out.extend(std::mem::take(&mut self.events));
        for e in entries {
            if self.prev.get(&e.addr) != Some(&e.val) {
                self.out.push_back((e.addr.clone(), e.val.clone()));
                self.prev.insert(e.addr, e.val);
            }
        }
        while self.out.len() > OUT_QUEUE {
            self.out.pop_front();
        }
        for _ in 0..BURST {
            let Some((a, v)) = self.out.pop_front() else { break };
            self.send(&a, &v);
        }
    }

    /// The fork's peer level timer (only with "send peer levels").
    pub fn meters(&mut self, now: Instant, peers: &[PeerStatus]) {
        if !(self.cfg.enabled && self.cfg.send_peer_levels) || self.last_meter.is_some_and(|t| now - t < METER_PERIOD) {
            return;
        }
        self.last_meter = Some(now);
        for (i, p) in peers.iter().enumerate().take(SLOTS) {
            self.send(&format!("/Peer{}RecvMeterLevel", i + 1), &f(meter_value(&p.meter)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn curves_match_the_fork() {
        // unity sits at sqrt(0.5) on the controller
        assert_eq!(gain_to_position(1.0) * SCALE, 0.707_106_77);
        assert_eq!(position_to_gain(0.5 * INVERSE), 0.5);
        assert_eq!(position_to_gain(1.0 * INVERSE), 2.0);
        assert_eq!(gain_to_position(5.0), 2.0);
        assert_eq!(position_to_gain(-1.0), 0.0);
        // mean RMS 0.12 -> 0.12 * 10 / 1.2 = 1.0
        let m = MeterLevels { peak: vec![0.0, 0.0], rms: vec![0.1, 0.14] };
        assert!((meter_value(&m) - 1.0).abs() < 1e-6);
        assert_eq!(meter_value(&MeterLevels::default()), 0.0);
    }

    #[test]
    fn parse_addresses() {
        assert_eq!(parse("/Peer16Mute"), Some(Addr::Peer(15, "Mute")));
        assert_eq!(parse("/Peer1EqPara2Q"), Some(Addr::Peer(0, "EqPara2Q")));
        assert_eq!(parse("/InputGroup3PanLeft"), Some(Addr::Group(2, "PanLeft")));
        assert_eq!(parse("/Soundboard2Track16"), Some(Addr::Track(1, 15)));
        assert_eq!(parse("/OutGainSlider"), Some(Addr::Global("/OutGainSlider")));
        for bad in ["/Peer0Mute", "/Peer17Mute", "/Peer01Mute", "/PeerMute", "/Peer1Nope", "/Soundboard1Track0", "/Soundboard1Name", "/outgainslider", "/Peer1RecvMeterLevel"] {
            assert_eq!(parse(bad), None, "{bad}");
        }
        assert!(is_known("/Peer1RecvMeterLevel") && is_known("/Soundboard16Name") && !is_known("/Soundboard17Name"));
    }

    #[test]
    fn bundles_and_hostile_packets() {
        let mut m = Vec::new();
        encode(&mut m, "/DrySlider", &f(0.25));
        let mut bundle = b"#bundle\0\0\0\0\0\0\0\0\x01".to_vec();
        bundle.extend_from_slice(&(m.len() as u32).to_be_bytes());
        bundle.extend_from_slice(&m);
        let mut out = Vec::new();
        decode_packet(&bundle, &mut out);
        assert_eq!(out, [("/DrySlider".to_owned(), InArg::Float(0.125))]);
        let mut nested = b"#bundle\0\0\0\0\0\0\0\0\x01".to_vec();
        nested.extend_from_slice(&(bundle.len() as u32).to_be_bytes());
        nested.extend_from_slice(&bundle);
        out.clear();
        decode_packet(&nested, &mut out);
        assert_eq!(out.len(), 1);
        // bad element size, truncated bundles, unknown tags (JUCE drops the whole packet)
        let mut bad = bundle.clone();
        bad[16..20].copy_from_slice(&1000u32.to_be_bytes());
        let mut d = Vec::new();
        osc::encode(&mut d, "/DrySlider", [Arg::Double(1.0)]);
        let mut t = Vec::new();
        osc::encode(&mut t, "/MetEnableButton", [Arg::Bool(true)]);
        for p in [&bad[..], &bundle[..bundle.len() - 1], b"#bundle\0", &d, &t, b"", b"\0\0\0\0", &[0xff; 64]] {
            out.clear();
            decode_packet(p, &mut out);
            assert!(out.is_empty(), "{p:?}");
        }
        // deep nesting stops
        let mut deep = m.clone();
        for _ in 0..10 {
            let mut b2 = b"#bundle\0\0\0\0\0\0\0\0\x01".to_vec();
            b2.extend_from_slice(&(deep.len() as u32).to_be_bytes());
            b2.extend_from_slice(&deep);
            deep = b2;
        }
        out.clear();
        decode_packet(&deep, &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn inversion_both_ways() {
        let enc = |addr: &str, v: Var| {
            let mut o = Vec::new();
            encode(&mut o, addr, &v);
            o
        };
        assert_eq!(enc("/MainMuteButton", Var::Int(1)), b"/MainMuteButton\0,i\0\0\0\0\0\0");
        assert_eq!(enc("/MainRecvMuteButton", Var::Int(0)), b"/MainRecvMuteButton\0,i\0\0\0\0\0\x01");
        assert_eq!(enc("/InMuteButton", Var::Int(1)), b"/InMuteButton\0\0\0,i\0\0\0\0\0\x01");
        // double: inverted, then scaled
        assert_eq!(enc("/MainMuteButton", Var::Double(0.0)), b"/MainMuteButton\0,f\0\0\x3f\0\0\0");
        let dec = |p: &[u8]| {
            let mut o = Vec::new();
            decode_packet(p, &mut o);
            o.pop().unwrap().1
        };
        let mut p = Vec::new();
        osc::encode(&mut p, "/MainMuteButton", [Arg::Float(0.3)]);
        assert_eq!(dec(&p), InArg::Float(1.0));
        p.clear();
        osc::encode(&mut p, "/MainMuteButton", [Arg::Int(5)]);
        assert_eq!(dec(&p), InArg::Int(0));
        p.clear();
        osc::encode(&mut p, "/MainMuteButton", [Arg::Str("x")]);
        assert_eq!(dec(&p), InArg::Str("x".into()));
    }

    #[test]
    fn listener_drops_oversize_and_rate_limits() {
        let (tx, rx) = sync_channel(4 * MAX_RATE as usize);
        let l = listen("127.0.0.1:0".parse().unwrap(), tx, Arc::new(std::sync::Mutex::new((Instant::now(), 0)))).unwrap();
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut big = Vec::new();
        osc::encode(&mut big, "/DrySlider", [Arg::Float(1.0), Arg::Blob(&[0; MAX_PACKET])]);
        s.send_to(&big, l.addr).unwrap();
        let mut m = Vec::new();
        encode(&mut m, "/DrySlider", &f(0.5));
        // in bursts the receive buffer can hold; all within the one-second window
        for _ in 0..(MAX_RATE as usize * 3 / 2) / 100 {
            for _ in 0..100 {
                s.send_to(&m, l.addr).unwrap();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        std::thread::sleep(Duration::from_millis(200));
        let got: Vec<_> = rx.try_iter().collect();
        assert!(got.iter().all(|g| g == &("/DrySlider".to_owned(), InArg::Float(0.25))));
        assert!(got.len() <= MAX_RATE as usize && got.len() > MAX_RATE as usize / 2, "{}", got.len());
        drop(l);
    }

    /// Reconfiguring never waits for the old listener, and the rate limit's second survives a
    /// rebind (review finding 3: a burst of rebinds took seconds, each reset the limit).
    #[test]
    fn rebinds_are_quick_and_keep_the_rate_limit() {
        let free = || UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let (p1, p2) = (free(), free());
        let mut o = Osc::default();
        let cfg = |p| OscConfig { enabled: true, receive_port: p, ..OscConfig::default() };
        let t = Instant::now();
        for i in 0..40 {
            o.configure(cfg(if i % 2 == 0 { p1 } else { p2 }), false);
            let _ = o.take_inbound();
        }
        assert!(t.elapsed() < Duration::from_millis(500), "{:?}", t.elapsed());
        // now on p2; wait until it listens (a rebind may be pending)
        let t = Instant::now();
        while o.status().listening.is_none() {
            assert!(t.elapsed() < Duration::from_secs(2), "{:?}", o.status().error);
            let _ = o.take_inbound();
            std::thread::sleep(Duration::from_millis(5));
        }
        *o.rate.lock().unwrap() = (Instant::now(), MAX_RATE); // the second's budget is spent
        o.configure(cfg(p1), false);
        let s = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut m = Vec::new();
        encode(&mut m, "/DrySlider", &f(0.5));
        let mut got = 0;
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(300) {
            let _ = s.send_to(&m, ("127.0.0.1", p1));
            got += o.take_inbound().len();
            std::thread::sleep(Duration::from_millis(10));
        }
        // the window started before the rebind: nothing passes until it ends
        assert_eq!(got, 0, "the rebind reset the limit");
    }

    #[test]
    fn config_round_trip_and_defaults() {
        let d = OscConfig::default();
        assert!(!d.enabled && !d.allow_network);
        assert_eq!(d.bind_addr(), "127.0.0.1:6000".parse().unwrap());
        assert_eq!(d.target(), Some("127.0.0.1:6001".parse().unwrap()));
        assert_eq!(OscConfig::load(&ExtraState::default()), d);
        let c = OscConfig { enabled: true, allow_network: true, receive_port: 7000, target_host: "10.0.0.2".into(), target_port: 7001, send_state_on_start: false, send_peer_levels: true };
        let mut e = ExtraState::default();
        c.store(&mut e);
        assert_eq!(OscConfig::load(&e), c);
        assert_eq!(c.bind_addr(), "0.0.0.0:7000".parse().unwrap());
        // the fork's key names and types
        assert!(e.unknown.contains(&(KEY_TARGET_PORT.into(), TreeVar::Int(7001))));
        assert!(e.unknown.contains(&(KEY_ENABLED.into(), TreeVar::Bool(true))));
    }
}
