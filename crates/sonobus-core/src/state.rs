//! The persistent processor state of SonoBus 1.7.2, as a typed model with lossless conversion to
//! and from the `SonoBusAoO` ValueTree that upstream saves (host plugin state, `filterStateXML` in
//! `SonoBus.settings` and `.sonobus` setups, `PluginDefault.xml`).
//!
//! Upstream authority: `SonobusAudioProcessor::getStateInformationWithOptions` /
//! `setStateInformationWithOptions` (SonobusPluginProcessor.cpp:8490-8802), the APVTS parameter
//! layout (P.cpp:595-710), `PeerStateCache` (P.cpp:8867-8963), `ChannelGroupParams`
//! (ChannelGroup.cpp:620-725) and `EffectParams.cpp`.
//!
//! Reading follows upstream's `x = tree.getProperty(key, x)` pattern: [`AppState::apply_tree`]
//! leaves every field whose key is missing at its current value (except the APVTS parameters,
//! which reset to their defaults, see [`AppState::apply_tree`]), values of any JUCE var type are
//! converted the way JUCE's `var` conversions do (XML files hold everything as strings), and
//! unknown keys never cause an error. Parameters are clamped to their ranges as JUCE does; the
//! other enum-like `i32` fields keep the stored number (so values from newer versions or forks
//! round-trip) and have `*_checked` accessors returning `None` for values 1.7.2 does not define. Writing produces the exact property order and var types
//! upstream produces for a freshly started processor, so a tree from [`AppState::to_tree`]
//! serializes byte-identically to what stock 1.7.2 writes for the same values (verified against
//! JUCE-generated fixtures in `tests/state_golden.rs`).
//!
//! Upstream persistence bugs and what this module does about them:
//! - **EQ para2 Q** (EffectParams.cpp:28): upstream writes `para2Q` under the para1 key `p1q`,
//!   replacing `para1Q`, and reads both Qs from `p1q`. See [`ParametricEq`].
//! - **Peer multichannel cache** (P.cpp:8950): `PeerStateCache::setFromValueTree` reads the
//!   multichannel groups from `ChannelGroups` instead of `MultiChannelGroups`. See
//!   [`PeerStateCache::multi_channel_groups`].
//! - `numChanGroups`/`numMultiChanGroups` of a peer cache entry are clamped to 0..=63 on read
//!   (`MAX_CHANGROUPS-1`, P.cpp:8916, 8947) although 64 can be written; we clamp to 0..=64.
//!
//! Not restored by upstream after a load (P.cpp:8778-8786): `metenabled` and `mastmonsolo` are
//! forced off. That is engine behaviour; this model reports the stored values.

use crate::juce_xml::var_to_string;
use crate::valuetree::{ValueTree, Var, set_all};

/// `MAX_CHANGROUPS` (SonobusPluginProcessor.h:33).
pub const MAX_CHANGROUPS: usize = 64;
/// `MAX_CHANNELS` (ChannelGroup.h:18): channels per group.
pub const MAX_CHANNELS: usize = 64;
/// `DEFAULT_SERVER_PORT` (SonobusPluginProcessor.h:34).
pub const DEFAULT_SERVER_PORT: i32 = 10998;

/// Defines a C++ enum as a Rust enum with `from_i32` (`None` for undefined values).
macro_rules! int_enum {
    ($(#[$m:meta])* $name:ident { $($v:ident = $n:literal),* $(,)? }) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        pub enum $name { $($v = $n),* }

        impl $name {
            /// The variant stored as `v`; `None` for values stock 1.7.2 does not define.
            pub fn from_i32(v: i32) -> Option<Self> {
                match v {
                    $($n => Some(Self::$v),)*
                    _ => None,
                }
            }
        }
    };
}
pub(crate) use int_enum;

int_enum! {
    /// `sendchannels` choices (P.cpp:633).
    SendChannels { MatchInputs = 0, Mono = 1, Stereo = 2 }
}
int_enum! {
    /// `ReverbModel` (SonobusPluginProcessor.h:132-136).
    ReverbModel { Freeverb = 0, MVerb = 1, Zita = 2 }
}
int_enum! {
    /// `AutoNetBufferMode` (SonobusPluginProcessor.h:123-128).
    NetBufAuto { Off = 0, AutoIncrease = 1, AutoFull = 2, InitialAuto = 3 }
}
int_enum! {
    /// `RecordFileFormat` (SonobusPluginProcessor.h:147-153).
    RecordFormat { Default = 0, Auto = 1, Flac = 2, Wav = 3, Ogg = 4 }
}
int_enum! {
    /// `PeerDisplayMode` (SonobusPluginProcessor.h:155-158).
    PeerDisplayMode { Full = 0, Minimal = 1 }
}
int_enum! {
    /// `VideoLinkInfo::pushViewMode` (SonobusPluginProcessor.h:590-603).
    PushViewMode { PushAndView = 0, PushOnly = 1, ViewOnly = 2 }
}

/// `x = tree.getProperty(key, x)` for each supported field type.
trait Prop: Sized {
    fn from_var(v: &Var) -> Self;
    fn to_var(&self) -> Var;
}

impl Prop for f32 {
    fn from_var(v: &Var) -> Self {
        v.to_f64() as f32
    }
    /// A C++ `float` passed to `setProperty` becomes `var(double)`.
    fn to_var(&self) -> Var {
        Var::Double(f64::from(*self))
    }
}

impl Prop for i32 {
    fn from_var(v: &Var) -> Self {
        v.to_i32()
    }
    fn to_var(&self) -> Var {
        Var::Int(*self)
    }
}

impl Prop for i64 {
    fn from_var(v: &Var) -> Self {
        v.to_i64()
    }
    fn to_var(&self) -> Var {
        Var::Int64(*self)
    }
}

impl Prop for bool {
    fn from_var(v: &Var) -> Self {
        v.to_bool()
    }
    fn to_var(&self) -> Var {
        Var::Bool(*self)
    }
}

impl Prop for String {
    fn from_var(v: &Var) -> Self {
        var_to_string(v)
    }
    fn to_var(&self) -> Var {
        Var::String(self.clone())
    }
}

fn rd<T: Prop>(t: &ValueTree, key: &str, field: &mut T) {
    if let Some(v) = t.get(key) {
        *field = T::from_var(v);
    }
}

fn wr<T: Prop>(t: &mut ValueTree, key: &str, field: &T) {
    t.set(key, field.to_var());
}

// ---- APVTS parameters ---------------------------------------------------------------------------

/// How an APVTS parameter value (always stored as a float, `PARAM@value`) maps to the field.
trait ParamValue: Copy {
    /// The value the parameter holds after APVTS `setDenormalisedValue(v)` on the range
    /// `(start, end, interval)`: `RangedAudioParameter::convertTo0to1`/`convertFrom0to1`
    /// (juce_RangedAudioParameter.cpp:39-49) snap and clamp it.
    fn from_f32(v: f32, range: (f64, f64, f64)) -> Self;
    fn to_f32(self) -> f32;
}

/// `AudioParameterFloat`: `NormalisableRange::snapToLegalValue` (juce_NormalisableRange.h:
/// 176-185), computed in double precision so values on the grid stay exactly as they are (JUCE's
/// float arithmetic and skew round trip can move them by an ulp). NaN gives `start` (JUCE keeps
/// NaN).
impl ParamValue for f32 {
    fn from_f32(v: f32, (start, end, interval): (f64, f64, f64)) -> Self {
        if v.is_nan() {
            return start as f32;
        }
        let mut v = f64::from(v);
        if interval > 0.0 {
            v = start + interval * ((v - start) / interval + 0.5).floor();
        }
        v.clamp(start, end) as f32
    }
    fn to_f32(self) -> f32 {
        self
    }
}

/// `AudioParameterBool`: range 0..1 step 1, `get()` is `value >= 0.5`.
impl ParamValue for bool {
    fn from_f32(v: f32, _: (f64, f64, f64)) -> Self {
        v >= 0.5
    }
    fn to_f32(self) -> f32 {
        if self { 1.0 } else { 0.0 }
    }
}

/// `AudioParameterChoice`/`AudioParameterInt`: `roundToInt (jlimit (start, end, v))`
/// (juce_AudioParameterInt.cpp:33-41; roundToInt rounds half to even). NaN gives `start`.
impl ParamValue for i32 {
    fn from_f32(v: f32, (start, end, _): (f64, f64, f64)) -> Self {
        if v.is_nan() {
            return start as i32;
        }
        f64::from(v).clamp(start, end).round_ties_even() as i32
    }
    fn to_f32(self) -> f32 {
        self as f32
    }
}

macro_rules! params {
    ($($(#[$m:meta])* $field:ident: $ty:ty = $id:literal, $standalone:expr, $plugin:expr, $range:expr;)*) => {
        /// The `AudioProcessorValueTreeState` parameters (P.cpp:44-81, 595-710), stored as
        /// `<PARAM id=".." value=".."/>` children of the state root (all `ParameterID` version 1).
        ///
        /// Ranges are the upstream `NormalisableRange`s `(start, end, interval)`. Like JUCE,
        /// [`Params::set_by_id`] (and so every load) clamps to them and snaps to the interval;
        /// fields written directly are not checked.
        #[derive(Debug, Clone, PartialEq)]
        pub struct Params { $($(#[$m])* pub $field: $ty,)* }

        impl Params {
            /// Upstream defaults in the standalone app.
            pub fn standalone() -> Self { Params { $($field: $standalone,)* } }
            /// Upstream defaults when loaded as a plugin (P.cpp:614, 622, 694, 769-783).
            pub fn plugin() -> Self { Params { $($field: $plugin,)* } }

            /// `(id, value)` for every parameter, values as APVTS stores them.
            pub fn values(&self) -> Vec<(&'static str, f32)> {
                vec![$(($id, ParamValue::to_f32(self.$field)),)*]
            }

            /// Sets the parameter with this id, clamped and snapped to its range as APVTS does;
            /// false if the id is unknown.
            pub fn set_by_id(&mut self, id: &str, value: f32) -> bool {
                match id {
                    $($id => self.$field = ParamValue::from_f32(value, $range),)*
                    _ => return false,
                }
                true
            }
        }
    };
}

params! {
    /// `ingain`: input gain, 0..4 skew 0.33. Unused by the 1.7.2 engine.
    in_gain: f32 = "ingain", 1.0, 1.0, (0.0, 4.0, 0.0);
    /// `inmonmonopan`: legacy mono input monitor pan, -1..1 (writes group 0's pan).
    in_mon_mono_pan: f32 = "inmonmonopan", 0.0, 0.0, (-1.0, 1.0, 0.0);
    /// `inmonpan1`: legacy stereo input monitor pan left, -1..1.
    in_mon_pan1: f32 = "inmonpan1", -1.0, -1.0, (-1.0, 1.0, 0.0);
    /// `inmonpan2`: legacy stereo input monitor pan right, -1..1.
    in_mon_pan2: f32 = "inmonpan2", 1.0, 1.0, (-1.0, 1.0, 0.0);
    /// `dry`: monitor level, 0..1 skew 0.5 (standalone 0, plugin 1).
    dry: f32 = "dry", 0.0, 1.0, (0.0, 1.0, 0.0);
    /// `wet`: output level, 0..2 skew 0.5.
    wet: f32 = "wet", 1.0, 1.0, (0.0, 2.0, 0.0);
    /// `defnetbuf`: default jitter buffer in SECONDS, 0..1 step 0.001 skew 0.5.
    def_net_buf: f32 = "defnetbuf", 0.001, 0.001, (0.0, 1.0, 0.001);
    /// `sendchannels`: 0 match inputs, 1 mono, 2 stereo (standalone 1, plugin 0).
    send_channels: i32 = "sendchannels", 1, 0, (0.0, 2.0, 1.0);
    /// `metenabled`: metronome on. Upstream forces it off after every load.
    met_enabled: bool = "metenabled", false, false, BOOL;
    /// `sendmetaudio`: send metronome to peers.
    send_met_audio: bool = "sendmetaudio", false, false, BOOL;
    /// `metgain`: metronome level, 0..1 skew 0.5.
    met_gain: f32 = "metgain", 0.5, 0.5, (0.0, 1.0, 0.0);
    /// `mettempo`: bpm, 10..400 step 1 skew 0.5.
    met_tempo: f32 = "mettempo", 100.0, 100.0, (10.0, 400.0, 1.0);
    /// `sendfileaudio`: send file playback to peers.
    send_file_audio: bool = "sendfileaudio", false, false, BOOL;
    /// `sendsoundboardaudio`: send soundboard to peers.
    send_soundboard_audio: bool = "sendsoundboardaudio", false, false, BOOL;
    /// `hearlatencytest`.
    hear_latency_test: bool = "hearlatencytest", false, false, BOOL;
    /// `metisrecorded`: metronome in recordings.
    met_is_recorded: bool = "metisrecorded", true, true, BOOL;
    /// `mainreverbenabled`.
    main_reverb_enabled: bool = "mainreverbenabled", false, false, BOOL;
    /// `nmainreverblevel`: 0..1 skew 0.4.
    main_reverb_level: f32 = "nmainreverblevel", 1.0, 1.0, (0.0, 1.0, 0.0);
    /// `mainreverbsize`: 0..1.
    main_reverb_size: f32 = "mainreverbsize", 0.15, 0.15, (0.0, 1.0, 0.0);
    /// `mainreverbdamp`: 0..1.
    main_reverb_damp: f32 = "mainreverbdamp", 0.5, 0.5, (0.0, 1.0, 0.0);
    /// `mainreverbpredelay`: ms, 0..100 step 1.
    main_reverb_predelay: f32 = "mainreverbpredelay", 20.0, 20.0, (0.0, 100.0, 1.0);
    /// `mainreverbmodel`: 0 Freeverb, 1 MVerb, 2 Zita.
    main_reverb_model: i32 = "mainreverbmodel", 1, 1, (0.0, 2.0, 1.0);
    /// `mastsendmute`.
    main_send_mute: bool = "mastsendmute", false, false, BOOL;
    /// `mastrecvmute`.
    main_recv_mute: bool = "mastrecvmute", false, false, BOOL;
    /// `mastinmute`.
    main_in_mute: bool = "mastinmute", false, false, BOOL;
    /// `mastmonsolo`: upstream forces it off after every load.
    main_monitor_solo: bool = "mastmonsolo", false, false, BOOL;
    /// `defnetauto`: 0 Off, 1 Auto-Increase, 2 Auto-Full, 3 Initial-Auto.
    def_net_auto: i32 = "defnetauto", 2, 2, (0.0, 3.0, 1.0);
    /// `defsendqual`: send format index 0..14 (4 = Opus 96 kbps/ch).
    def_send_qual: i32 = "defsendqual", 4, 4, (0.0, 14.0, 1.0);
    /// `dynamicresampling`: drift correction.
    dynamic_resampling: bool = "dynamicresampling", false, false, BOOL;
    /// `reconnectlast`: reconnect to the most recent group on start.
    reconnect_last: bool = "reconnectlast", false, false, BOOL;
    /// `defPeerLevel`: default level for new users, 0..1 skew 0.5.
    def_peer_level: f32 = "defPeerLevel", 1.0, 1.0, (0.0, 1.0, 0.0);
    /// `syncMetHost`: metronome follows host tempo (standalone false, plugin true).
    sync_met_host: bool = "syncMetHost", false, true, BOOL;
    /// `inreverblevel`: 0..1 skew 0.4.
    in_reverb_level: f32 = "inreverblevel", 1.0, 1.0, (0.0, 1.0, 0.0);
    /// `inreverbsize`: 0..1.
    in_reverb_size: f32 = "inreverbsize", 0.15, 0.15, (0.0, 1.0, 0.0);
    /// `inreverbdamp`: 0..1.
    in_reverb_damp: f32 = "inreverbdamp", 0.5, 0.5, (0.0, 1.0, 0.0);
    /// `inreverbpredelay`: ms, 0..100 step 1.
    in_reverb_predelay: f32 = "inreverbpredelay", 20.0, 20.0, (0.0, 100.0, 1.0);
    /// `syncMetFile`: metronome follows file playback.
    sync_met_file: bool = "syncMetFile", false, false, BOOL;
}

const BOOL: (f64, f64, f64) = (0.0, 1.0, 1.0);

impl Params {
    pub fn send_channels_checked(&self) -> Option<SendChannels> {
        SendChannels::from_i32(self.send_channels)
    }
    pub fn main_reverb_model_checked(&self) -> Option<ReverbModel> {
        ReverbModel::from_i32(self.main_reverb_model)
    }
    pub fn def_net_auto_checked(&self) -> Option<NetBufAuto> {
        NetBufAuto::from_i32(self.def_net_auto)
    }
}

impl Default for Params {
    fn default() -> Self {
        Params::standalone()
    }
}

// ---- effect parameters (EffectParams.h/.cpp) ----------------------------------------------------

/// `CompressorParams` (EffectParams.h:11-24), used for the compressor, expander and limiter
/// (`CompressorState`, `ExpanderState`, `LimiterState`). [`Default`] is the C++ struct default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Dynamics {
    /// `enabled`
    pub enabled: bool,
    /// `threshold` (dB)
    pub threshold_db: f32,
    /// `ratio`
    pub ratio: f32,
    /// `attack` (ms)
    pub attack_ms: f32,
    /// `release` (ms)
    pub release_ms: f32,
    /// `makeupgain` (dB)
    pub makeup_gain_db: f32,
    /// `automakeup`
    pub auto_makeup: bool,
}

impl Default for Dynamics {
    fn default() -> Self {
        Dynamics {
            enabled: false,
            threshold_db: -16.0,
            ratio: 2.0,
            attack_ms: 10.0,
            release_ms: 80.0,
            makeup_gain_db: 0.0,
            auto_makeup: true,
        }
    }
}

impl Dynamics {
    fn to_tree(self, key: &str) -> ValueTree {
        let mut t = ValueTree::new(key);
        wr(&mut t, "enabled", &self.enabled);
        wr(&mut t, "threshold", &self.threshold_db);
        wr(&mut t, "ratio", &self.ratio);
        wr(&mut t, "attack", &self.attack_ms);
        wr(&mut t, "release", &self.release_ms);
        wr(&mut t, "makeupgain", &self.makeup_gain_db);
        wr(&mut t, "automakeup", &self.auto_makeup);
        t
    }

    fn apply(&mut self, t: &ValueTree) {
        rd(t, "enabled", &mut self.enabled);
        rd(t, "threshold", &mut self.threshold_db);
        rd(t, "ratio", &mut self.ratio);
        rd(t, "attack", &mut self.attack_ms);
        rd(t, "release", &mut self.release_ms);
        rd(t, "makeupgain", &mut self.makeup_gain_db);
        rd(t, "automakeup", &mut self.auto_makeup);
    }
}

/// `ParametricEqParams` (EffectParams.h:28-43), child `ParametricEqState`.
///
/// Upstream bug (EffectParams.cpp:28, `eqPara2QKey("p1q")`): stock writes `para1Q` then
/// overwrites it in place with `para2Q`, so its files hold one `p1q` = para2 Q and no para1 Q;
/// on load it sets BOTH Qs from `p1q`.
/// - Reading a stock file (no `p2q`): both Qs = `p1q`. That recovers para2 Q exactly and gives
///   para1 Q the value a stock client itself shows after a reload; the real para1 Q is not in
///   the file.
/// - Writing: `p1q` = para1 Q and an extra `p2q` = para2 Q (what an upstream fix renaming the
///   key would write). A stock client loading our file ignores `p2q` and sets both Qs to
///   para1 Q; no error, every other band value is exact.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParametricEq {
    /// `enabled`
    pub enabled: bool,
    /// `lsgain` (dB)
    pub low_shelf_gain: f32,
    /// `lsfreq` (Hz)
    pub low_shelf_freq: f32,
    /// `p1gain` (dB)
    pub para1_gain: f32,
    /// `p1freq` (Hz)
    pub para1_freq: f32,
    /// `p1q` (see the type docs)
    pub para1_q: f32,
    /// `p2gain` (dB)
    pub para2_gain: f32,
    /// `p2freq` (Hz)
    pub para2_freq: f32,
    /// `p2q` when written by us, `p1q` in stock files (see the type docs)
    pub para2_q: f32,
    /// `hsgain` (dB)
    pub high_shelf_gain: f32,
    /// `hsfreq` (Hz)
    pub high_shelf_freq: f32,
}

impl Default for ParametricEq {
    fn default() -> Self {
        ParametricEq {
            enabled: false,
            low_shelf_gain: 0.0,
            low_shelf_freq: 60.0,
            para1_gain: 0.0,
            para1_freq: 90.0,
            para1_q: 1.5,
            para2_gain: 0.0,
            para2_freq: 360.0,
            para2_q: 4.0,
            high_shelf_gain: 0.0,
            high_shelf_freq: 10000.0,
        }
    }
}

impl ParametricEq {
    fn to_tree(self) -> ValueTree {
        let mut t = ValueTree::new("ParametricEqState");
        wr(&mut t, "enabled", &self.enabled);
        wr(&mut t, "lsfreq", &self.low_shelf_freq);
        wr(&mut t, "lsgain", &self.low_shelf_gain);
        wr(&mut t, "hsfreq", &self.high_shelf_freq);
        wr(&mut t, "hsgain", &self.high_shelf_gain);
        wr(&mut t, "p1gain", &self.para1_gain);
        wr(&mut t, "p1freq", &self.para1_freq);
        wr(&mut t, "p1q", &self.para1_q);
        wr(&mut t, "p2gain", &self.para2_gain);
        wr(&mut t, "p2freq", &self.para2_freq);
        wr(&mut t, "p2q", &self.para2_q);
        t
    }

    fn apply(&mut self, t: &ValueTree) {
        rd(t, "enabled", &mut self.enabled);
        rd(t, "lsfreq", &mut self.low_shelf_freq);
        rd(t, "lsgain", &mut self.low_shelf_gain);
        rd(t, "hsfreq", &mut self.high_shelf_freq);
        rd(t, "hsgain", &mut self.high_shelf_gain);
        rd(t, "p1gain", &mut self.para1_gain);
        rd(t, "p1freq", &mut self.para1_freq);
        rd(t, "p2gain", &mut self.para2_gain);
        rd(t, "p2freq", &mut self.para2_freq);
        rd(t, "p1q", &mut self.para1_q);
        if t.get("p2q").is_some() {
            rd(t, "p2q", &mut self.para2_q);
        } else {
            rd(t, "p1q", &mut self.para2_q); // stock file: p1q holds para2 Q
        }
    }
}

/// `DelayParams` (EffectParams.h:45-52), child `MonitorDelayState`.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct MonitorDelay {
    /// `enabled`
    pub enabled: bool,
    /// `delaytimems`
    pub delay_time_ms: f32,
}

// ---- channel groups (ChannelGroup.h:24-87, ChannelGroup.cpp:620-725) ---------------------------

/// `ChannelGroupParams` as persisted in a `ChannelGroup` tree: input groups, the extra groups
/// (file playback, metronome, soundboard) and per-peer receive groups.
///
/// [`Default`] is the C++ struct default (what peer-cache and extra groups start from).
/// Input groups start from [`ChannelGroup::input_default`] instead. `soloed` and
/// `centerPanLaw` are not persisted upstream.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelGroup {
    /// `name`
    pub name: String,
    /// `chanstart`: first source channel.
    pub chan_start: i32,
    /// `numchan`: clamped to 0..=[`MAX_CHANNELS`] on read (stock's arrays hold 64).
    pub num_channels: i32,
    /// `muted`
    pub muted: bool,
    /// `gain` (linear)
    pub gain: f32,
    /// `StereoPanners/Panner@pan`, one per channel (used when `num_channels != 2`). Written as
    /// exactly `num_channels` (max 64) panners, padded with 0.0; after a read it holds at least
    /// `num_channels` entries.
    pub pans: Vec<f32>,
    /// `span1`, `span2`: stereo pan pair (used when `num_channels == 2`).
    pub pan_stereo: [f32; 2],
    /// `pandeststart`
    pub pan_dest_start: i32,
    /// `pandestchans`
    pub pan_dest_channels: i32,
    /// `sendmainmix` (remote peers)
    pub send_main_mix: bool,
    /// `CompressorState`
    pub compressor: Dynamics,
    /// `ExpanderState`
    pub expander: Dynamics,
    /// `LimiterState`
    pub limiter: Dynamics,
    /// `ParametricEqState`
    pub eq: ParametricEq,
    /// `invertpolarity`
    pub invert_polarity: bool,
    /// `inreverbsend`
    pub in_reverb_send: f32,
    /// `monreverbsend`
    pub mon_reverb_send: f32,
    /// `monitorlev`
    pub monitor: f32,
    /// `mondeststart`
    pub mon_dest_start: i32,
    /// `mondestchans`
    pub mon_dest_channels: i32,
    /// `MonitorDelayState`
    pub monitor_delay: MonitorDelay,
}

impl Default for ChannelGroup {
    fn default() -> Self {
        ChannelGroup {
            name: String::new(),
            chan_start: 0,
            num_channels: 1,
            muted: false,
            gain: 1.0,
            pans: vec![0.0],
            pan_stereo: [-1.0, 1.0],
            pan_dest_start: 0,
            pan_dest_channels: 2,
            send_main_mix: true,
            compressor: Dynamics::default(),
            expander: Dynamics::default(),
            limiter: Dynamics::default(),
            eq: ParametricEq::default(),
            invert_polarity: false,
            in_reverb_send: 0.0,
            mon_reverb_send: 0.0,
            monitor: 1.0,
            mon_dest_start: 0,
            mon_dest_channels: 2,
            monitor_delay: MonitorDelay::default(),
        }
    }
}

impl ChannelGroup {
    /// Input group `index` as the processor constructor sets it up (P.cpp:810-817):
    /// one channel at `index`, then `setToDefaults(isplugin)` (ChannelGroup.cpp:67-90).
    pub fn input_default(index: usize, is_plugin: bool) -> Self {
        let mut g = ChannelGroup { chan_start: index as i32, ..Default::default() };
        g.expander.threshold_db = -60.0;
        g.expander.release_ms = 200.0;
        g.expander.attack_ms = 1.0;
        g.expander.ratio = 2.0;
        let c = &mut g.compressor;
        c.makeup_gain_db = (-c.threshold_db - (c.threshold_db / c.ratio).abs()) * 0.5;
        g.limiter = Dynamics {
            enabled: !is_plugin,
            threshold_db: -1.0,
            ratio: 4.0,
            attack_ms: 0.01,
            release_ms: 100.0,
            ..Dynamics::default()
        };
        g
    }

    /// An extra (pseudo) group as the processor sets it up (P.cpp:820-831,
    /// SoundboardChannelProcessor.cpp:184-185).
    fn extra_default(name: &str, num_channels: i32) -> Self {
        let n = num_channels as usize;
        ChannelGroup { name: name.into(), num_channels, pans: vec![0.0; n], ..Default::default() }
    }

    /// `ChannelGroupParams::getValueTree` (ChannelGroup.cpp:626-672).
    pub fn to_tree(&self) -> ValueTree {
        let mut t = ValueTree::new("ChannelGroup");
        wr(&mut t, "gain", &self.gain);
        wr(&mut t, "chanstart", &self.chan_start);
        wr(&mut t, "numchan", &self.num_channels);
        wr(&mut t, "muted", &self.muted);
        wr(&mut t, "span1", &self.pan_stereo[0]);
        wr(&mut t, "span2", &self.pan_stereo[1]);
        wr(&mut t, "monreverbsend", &self.mon_reverb_send);
        wr(&mut t, "inreverbsend", &self.in_reverb_send);
        wr(&mut t, "monitorlev", &self.monitor);
        wr(&mut t, "mondeststart", &self.mon_dest_start);
        wr(&mut t, "mondestchans", &self.mon_dest_channels);
        wr(&mut t, "pandeststart", &self.pan_dest_start);
        wr(&mut t, "pandestchans", &self.pan_dest_channels);
        wr(&mut t, "sendmainmix", &self.send_main_mix);
        wr(&mut t, "invertpolarity", &self.invert_polarity);
        wr(&mut t, "name", &self.name);
        let mut panners = ValueTree::new("StereoPanners");
        for i in 0..(self.num_channels.max(0) as usize).min(64) {
            let mut p = ValueTree::new("Panner");
            wr(&mut p, "pan", &self.pans.get(i).copied().unwrap_or(0.0));
            panners.children.push(p);
        }
        t.children.push(panners);
        t.children.push(self.compressor.to_tree("CompressorState"));
        t.children.push(self.expander.to_tree("ExpanderState"));
        t.children.push(self.limiter.to_tree("LimiterState"));
        t.children.push(self.eq.to_tree());
        let mut d = ValueTree::new("MonitorDelayState");
        wr(&mut d, "enabled", &self.monitor_delay.enabled);
        wr(&mut d, "delaytimems", &self.monitor_delay.delay_time_ms);
        t.children.push(d);
        t
    }

    /// `ChannelGroupParams::setFromValueTree` (ChannelGroup.cpp:674-725). The legacy mono
    /// `pan` property is not read by upstream either.
    pub fn apply(&mut self, t: &ValueTree) {
        rd(t, "gain", &mut self.gain);
        rd(t, "chanstart", &mut self.chan_start);
        rd(t, "numchan", &mut self.num_channels);
        rd(t, "muted", &mut self.muted);
        rd(t, "span1", &mut self.pan_stereo[0]);
        rd(t, "span2", &mut self.pan_stereo[1]);
        rd(t, "monreverbsend", &mut self.mon_reverb_send);
        rd(t, "inreverbsend", &mut self.in_reverb_send);
        rd(t, "monitorlev", &mut self.monitor);
        rd(t, "pandeststart", &mut self.pan_dest_start);
        rd(t, "pandestchans", &mut self.pan_dest_channels);
        rd(t, "mondeststart", &mut self.mon_dest_start);
        rd(t, "mondestchans", &mut self.mon_dest_channels);
        rd(t, "sendmainmix", &mut self.send_main_mix);
        rd(t, "invertpolarity", &mut self.invert_polarity);
        rd(t, "name", &mut self.name);
        self.num_channels = self.num_channels.clamp(0, MAX_CHANNELS as i32);
        if let Some(panners) = t.child_with_name("StereoPanners") {
            for (i, p) in panners.children.iter().take(MAX_CHANNELS).enumerate() {
                if i >= self.pans.len() {
                    self.pans.resize(i + 1, 0.0);
                }
                rd(p, "pan", &mut self.pans[i]);
            }
        }
        if self.pans.len() < self.num_channels as usize {
            self.pans.resize(self.num_channels as usize, 0.0);
        }
        let children: [(&str, &mut Dynamics); 3] = [
            ("CompressorState", &mut self.compressor),
            ("ExpanderState", &mut self.expander),
            ("LimiterState", &mut self.limiter),
        ];
        for (key, d) in children {
            if let Some(c) = t.child_with_name(key) {
                d.apply(c);
            }
        }
        if let Some(c) = t.child_with_name("ParametricEqState") {
            self.eq.apply(c);
        }
        if let Some(c) = t.child_with_name("MonitorDelayState") {
            rd(c, "enabled", &mut self.monitor_delay.enabled);
            rd(c, "delaytimems", &mut self.monitor_delay.delay_time_ms);
        }
    }
}

/// Reads up to 64 `ChannelGroup` children onto `groups` (padded with `pad(i)`), then keeps
/// `count` of them. Upstream holds 64 groups and a separate count; a saved file has `count`
/// children, so `groups.len()` is that count here.
fn apply_groups(
    groups: &mut Vec<ChannelGroup>,
    tree: Option<&ValueTree>,
    count: i32,
    pad: impl Fn(usize) -> ChannelGroup,
) {
    let n = groups.len();
    groups.extend((n..MAX_CHANGROUPS).map(&pad));
    if let Some(tree) = tree {
        for (g, c) in groups.iter_mut().zip(tree.children.iter().filter(|c| c.is_valid())) {
            g.apply(c);
        }
    }
    groups.truncate(count.clamp(0, MAX_CHANGROUPS as i32) as usize);
}

fn groups_tree(name: &str, groups: &[ChannelGroup]) -> ValueTree {
    let mut t = ValueTree::new(name);
    t.children = groups.iter().take(MAX_CHANGROUPS).map(ChannelGroup::to_tree).collect();
    t
}

// ---- recents, video link, extra state -----------------------------------------------------------

/// `AooServerConnectionInfo` (SonobusPluginProcessor.h:36-52), `RecentConnections` child
/// `ServerConnectionInfo` (P.cpp:8425-8452).
#[derive(Debug, Clone, PartialEq)]
pub struct ServerConnectionInfo {
    /// `userName`
    pub user_name: String,
    /// `userPassword`
    // SECURITY: stored in clear text by upstream; candidate for the OS keyring.
    pub user_password: String,
    /// `groupName`
    pub group_name: String,
    /// `groupPassword`
    // SECURITY: stored in clear text by upstream; candidate for the OS keyring.
    pub group_password: String,
    /// `serverHost`
    pub server_host: String,
    /// `serverPort` (uninitialised in C++ when missing; we use [`DEFAULT_SERVER_PORT`])
    pub server_port: i32,
    /// `timestamp`: ms since 1970 (int64; uninitialised in C++ when missing; we use 0)
    pub timestamp: i64,
    /// `groupIsPublic`
    pub group_is_public: bool,
}

impl Default for ServerConnectionInfo {
    fn default() -> Self {
        ServerConnectionInfo {
            user_name: String::new(),
            user_password: String::new(),
            group_name: String::new(),
            group_password: String::new(),
            server_host: String::new(),
            server_port: DEFAULT_SERVER_PORT,
            timestamp: 0,
            group_is_public: false,
        }
    }
}

impl ServerConnectionInfo {
    fn to_tree(&self) -> ValueTree {
        let mut t = ValueTree::new("ServerConnectionInfo");
        wr(&mut t, "userName", &self.user_name);
        wr(&mut t, "userPassword", &self.user_password);
        wr(&mut t, "groupName", &self.group_name);
        wr(&mut t, "groupPassword", &self.group_password);
        wr(&mut t, "serverHost", &self.server_host);
        wr(&mut t, "serverPort", &self.server_port);
        wr(&mut t, "timestamp", &self.timestamp);
        wr(&mut t, "groupIsPublic", &self.group_is_public);
        t
    }

    fn from_tree(t: &ValueTree) -> Self {
        let mut s = ServerConnectionInfo::default();
        rd(t, "userName", &mut s.user_name);
        rd(t, "userPassword", &mut s.user_password);
        rd(t, "groupName", &mut s.group_name);
        rd(t, "groupPassword", &mut s.group_password);
        rd(t, "serverHost", &mut s.server_host);
        rd(t, "serverPort", &mut s.server_port);
        rd(t, "timestamp", &mut s.timestamp);
        rd(t, "groupIsPublic", &mut s.group_is_public);
        s
    }
}

/// `VideoLinkInfo` (SonobusPluginProcessor.h:588-606), child of `ExtraState` (P.cpp:8454-8488).
#[derive(Debug, Clone, PartialEq)]
pub struct VideoLinkInfo {
    /// `roomMode`
    pub room_mode: bool,
    /// `showNames`
    pub show_names: bool,
    /// `screenShare`
    pub screen_share: bool,
    /// `extraParams`
    pub extra_params: String,
    /// `beDir`
    pub be_director: bool,
    /// `largeShare`
    pub large_share: bool,
    /// `pushViewMode`: 0 push and view, 1 push only, 2 view only (see
    /// [`Self::push_view_mode_checked`]).
    pub push_view_mode: i32,
}

impl VideoLinkInfo {
    pub fn push_view_mode_checked(&self) -> Option<PushViewMode> {
        PushViewMode::from_i32(self.push_view_mode)
    }
}

impl Default for VideoLinkInfo {
    fn default() -> Self {
        VideoLinkInfo {
            room_mode: true,
            show_names: true,
            screen_share: false,
            extra_params: String::new(),
            be_director: false,
            large_share: false,
            push_view_mode: 0,
        }
    }
}

/// Generates [`ExtraState`] with one field per `ExtraState` property, in upstream write order.
macro_rules! extra_state {
    ($($(#[$m:meta])* $field:ident: $ty:ty = $key:literal, $default:expr;)*) => {
        /// The `ExtraState` child (P.cpp:86-114, 8517-8549 write, 8640-8724 read): options and
        /// UI state that are not host parameters.
        #[derive(Debug, Clone, PartialEq)]
        pub struct ExtraState {
            $($(#[$m])* pub $field: $ty,)*
            /// `DefaultRecordDir`: legacy local path, written only when the record location
            /// is a local file. Upstream reads it only when `DefaultRecordDirURL` is empty
            /// (and never on iOS/Android).
            pub default_record_dir: Option<String>,
            /// `VideoLinkInfo` child.
            pub video_link: VideoLinkInfo,
            /// Properties this model does not know (forks, newer versions), kept in order and
            /// written back after the known ones. Upstream keeps them too (it updates the loaded
            /// tree in place).
            pub unknown: Vec<(String, Var)>,
        }

        impl Default for ExtraState {
            fn default() -> Self {
                ExtraState {
                    $($field: $default,)*
                    default_record_dir: None,
                    video_link: VideoLinkInfo::default(),
                    unknown: Vec::new(),
                }
            }
        }

        impl ExtraState {
            const KEYS: &[&str] = &[$($key,)* "DefaultRecordDir"];

            fn write_props(&self, t: &mut ValueTree, before_url: bool) {
                let mut url_seen = false;
                $(
                    url_seen |= $key == "DefaultRecordDirURL";
                    if url_seen != before_url {
                        wr(t, $key, &self.$field);
                    }
                )*
            }

            fn read_props(&mut self, t: &ValueTree) {
                $(rd(t, $key, &mut self.$field);)*
            }
        }
    };
}

extra_state! {
    /// `UseUdpPort`: fixed local UDP port, 0 = any.
    use_udp_port: i32 = "UseUdpPort", 0;
    /// `ChangeQualForAll`: changing the default send quality changes all peers.
    change_qual_for_all: bool = "ChangeQualForAll", false;
    /// `ChangeRecvQualForAll`: changing the default receive quality changes all peers.
    change_recv_qual_for_all: bool = "ChangeRecvQualForAll", false;
    /// `DefaultRecordingOptions`: bitmask, 1 mix, 2 self, 4 mix minus self, 8 individual users.
    default_recording_options: i32 = "DefaultRecordingOptions", 1;
    /// `DefaultRecordingFormat`: 0 default, 1 auto, 2 FLAC, 3 WAV, 4 OGG (see
    /// [`ExtraState::default_recording_format_checked`]).
    default_recording_format: i32 = "DefaultRecordingFormat", 2;
    /// `DefaultRecordingBitsPerSample`: 16 or 24.
    default_recording_bits_per_sample: i32 = "DefaultRecordingBitsPerSample", 16;
    /// `RecordSelfPreFx`
    record_self_pre_fx: bool = "RecordSelfPreFx", true;
    /// `RecordSelfSilenceWhenMuted`
    record_self_silence_when_muted: bool = "RecordSelfSilenceWhenMuted", true;
    /// `RecordFinishOpen`: open the folder of a finished recording.
    record_finish_open: bool = "RecordFinishOpen", true;
    /// `DefaultRecordDirURL`: recording location as a URL (upstream default
    /// `file://<music dir>/SonoBus` on desktop, Documents on iOS, empty on Android; the app
    /// layer fills that in).
    default_record_dir_url: String = "DefaultRecordDirURL", String::new();
    /// `LastBrowseDir` (ignored on iOS/Android).
    last_browse_dir: String = "LastBrowseDir", String::new();
    /// `SliderSnapToMouse`
    slider_snap_to_mouse: bool = "SliderSnapToMouse", true;
    /// `DisableKeyShortcuts`
    disable_key_shortcuts: bool = "DisableKeyShortcuts", false;
    /// `PeerDisplayMode`: 0 full, 1 minimal (see [`ExtraState::peer_display_mode_checked`]).
    peer_display_mode: i32 = "PeerDisplayMode", 0;
    /// `lastChatWidth`
    last_chat_width: i32 = "lastChatWidth", 250;
    /// `lastChatShown`
    last_chat_shown: bool = "lastChatShown", false;
    /// `chatFontSizeOffset`: -3 tiny, -1 small, 0 normal, 2 large, 4 huge.
    chat_font_size_offset: i32 = "chatFontSizeOffset", 0;
    /// `chatFixedWidthFont`
    chat_fixed_width_font: bool = "chatFixedWidthFont", false;
    /// `lastSoundboardWidth`
    last_soundboard_width: i32 = "lastSoundboardWidth", 250;
    /// `lastSoundboardShown`
    last_soundboard_shown: bool = "lastSoundboardShown", false;
    /// `linkMonDelayTimes`
    link_mon_delay_times: bool = "linkMonDelayTimes", true;
    /// `lastUsername`
    last_username: String = "lastUsername", String::new();
    /// `langOverrideCode`: empty = system language.
    lang_override_code: String = "langOverrideCode", String::new();
    /// `useUnivFont`
    use_univ_font: bool = "useUnivFont", false;
    /// `lastWindowWidth` (plugin editor)
    last_window_width: i32 = "lastWindowWidth", 800;
    /// `lastWindowHeight` (plugin editor)
    last_window_height: i32 = "lastWindowHeight", 600;
    /// `autoDropRateThreshNew`: drops/s before auto jitter buffer grows.
    auto_drop_rate_thresh: f32 = "autoDropRateThreshNew", 0.5;
    /// `reconnServLoss`: reconnect after losing the server.
    reconnect_after_server_loss: bool = "reconnServLoss", true;
}

impl ExtraState {
    pub fn default_recording_format_checked(&self) -> Option<RecordFormat> {
        RecordFormat::from_i32(self.default_recording_format)
    }
    pub fn peer_display_mode_checked(&self) -> Option<PeerDisplayMode> {
        PeerDisplayMode::from_i32(self.peer_display_mode)
    }

    fn to_tree(&self) -> ValueTree {
        let mut t = ValueTree::new("ExtraState");
        self.write_props(&mut t, true);
        if let Some(dir) = &self.default_record_dir {
            wr(&mut t, "DefaultRecordDir", dir);
        }
        self.write_props(&mut t, false);
        // what `t.set` per unknown key would give, in linear time
        let known = std::mem::take(&mut t.properties);
        t.properties = set_all(known.into_iter().chain(self.unknown.iter().cloned()));
        let v = &self.video_link;
        let mut vt = ValueTree::new("VideoLinkInfo");
        wr(&mut vt, "roomMode", &v.room_mode);
        wr(&mut vt, "showNames", &v.show_names);
        wr(&mut vt, "screenShare", &v.screen_share);
        wr(&mut vt, "extraParams", &v.extra_params);
        wr(&mut vt, "beDir", &v.be_director);
        wr(&mut vt, "largeShare", &v.large_share);
        wr(&mut vt, "pushViewMode", &v.push_view_mode);
        t.children.push(vt);
        t
    }

    fn apply(&mut self, t: &ValueTree) {
        self.read_props(t);
        if let Some(v) = t.get("DefaultRecordDir") {
            self.default_record_dir = Some(String::from_var(v));
        }
        self.unknown = t.properties.iter().filter(|(k, _)| !Self::KEYS.contains(&k.as_str())).cloned().collect();
        if let Some(vt) = t.child_with_name("VideoLinkInfo") {
            let v = &mut self.video_link;
            rd(vt, "roomMode", &mut v.room_mode);
            rd(vt, "showNames", &mut v.show_names);
            rd(vt, "screenShare", &mut v.screen_share);
            rd(vt, "extraParams", &mut v.extra_params);
            rd(vt, "beDir", &mut v.be_director);
            rd(vt, "largeShare", &mut v.large_share);
            rd(vt, "pushViewMode", &mut v.push_view_mode);
        }
    }
}

// ---- peer state cache ---------------------------------------------------------------------------

/// `PeerStateCache` (SonobusPluginProcessor.h:836-853, P.cpp:8867-8963): per-user receive
/// settings, keyed by user name, in `PeerStateCacheMap`.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerStateCache {
    /// `name`: the remote user name (the map key).
    pub name: String,
    /// `netbuf`: jitter buffer, ms.
    pub netbuf: f32,
    /// `netbufauto`: jitter buffer mode (see [`Params::def_net_auto`],
    /// [`Self::netbufauto_checked`]). Uninitialised in C++ when missing; we use 2 (Auto-Full,
    /// the default mode).
    pub netbufauto: i32,
    /// `sendformat`: the send format index used towards this user. Not validated: stock
    /// 1.7.2 has 11 formats although `defsendqual` allows 0..=14; bounds-check before indexing.
    pub send_format: i32,
    /// `level`: receive level (linear).
    pub level: f32,
    /// `orderpriority`
    pub order_priority: i32,
    /// `ChannelGroups` children; `numChanGroups` = len (max 64). Legacy entries without this
    /// subtree carry group 0's pan/stereo pan/compressor as `pan`, `span1`, `span2` properties and
    /// a `CompressorState` child (read like upstream, P.cpp:8923-8931; never written).
    pub channel_groups: Vec<ChannelGroup>,
    /// `MultiChannelGroups` children; `numMultiChanGroups` = len (max 64).
    ///
    /// Upstream bug (P.cpp:8950): stock reads these from `ChannelGroups`, so after a stock load
    /// the multichannel layout equals the first normal groups, and that is what the next stock
    /// save writes here. Stock always WRITES this subtree correctly, so we read it from
    /// `MultiChannelGroups`: files saved by stock directly after the user edited the layout
    /// give the real layout back; files that went through a stock load/save cycle give the
    /// copy of the normal groups, which is what a stock user sees. We write the correct data;
    /// a stock client loading our file gets its usual behaviour (multichannel = normal groups),
    /// without error.
    pub multi_channel_groups: Vec<ChannelGroup>,
    /// `modifiedChanGroups`
    pub modified_chan_groups: bool,
}

impl Default for PeerStateCache {
    fn default() -> Self {
        PeerStateCache {
            name: String::new(),
            netbuf: 10.0,
            netbufauto: 2,
            send_format: 4,
            level: 1.0,
            order_priority: -1,
            channel_groups: vec![Self::group_default(0)],
            multi_channel_groups: Vec::new(),
            modified_chan_groups: false,
        }
    }
}

impl PeerStateCache {
    pub fn netbufauto_checked(&self) -> Option<NetBufAuto> {
        NetBufAuto::from_i32(self.netbufauto)
    }

    /// Normal group `i` of a fresh cache entry (P.cpp:8867-8874).
    fn group_default(i: usize) -> ChannelGroup {
        ChannelGroup { chan_start: i as i32, ..Default::default() }
    }

    fn to_tree(&self) -> ValueTree {
        let mut t = ValueTree::new("PeerStateCache");
        wr(&mut t, "name", &self.name);
        wr(&mut t, "netbuf", &self.netbuf);
        wr(&mut t, "netbufauto", &self.netbufauto);
        wr(&mut t, "sendformat", &self.send_format);
        wr(&mut t, "numChanGroups", &(self.channel_groups.len() as i32));
        wr(&mut t, "level", &self.level);
        wr(&mut t, "orderpriority", &self.order_priority);
        t.children.push(groups_tree("ChannelGroups", &self.channel_groups));
        wr(&mut t, "numMultiChanGroups", &(self.multi_channel_groups.len() as i32));
        wr(&mut t, "modifiedChanGroups", &self.modified_chan_groups);
        t.children.push(groups_tree("MultiChannelGroups", &self.multi_channel_groups));
        t
    }

    fn from_tree(t: &ValueTree) -> Self {
        let mut s = PeerStateCache::default();
        rd(t, "name", &mut s.name);
        rd(t, "netbuf", &mut s.netbuf);
        rd(t, "netbufauto", &mut s.netbufauto);
        rd(t, "sendformat", &mut s.send_format);
        rd(t, "level", &mut s.level);
        rd(t, "orderpriority", &mut s.order_priority);
        rd(t, "modifiedChanGroups", &mut s.modified_chan_groups);
        let mut count = 1;
        rd(t, "numChanGroups", &mut count);
        let mut multi_count = 0;
        rd(t, "numMultiChanGroups", &mut multi_count);

        let g0 = &mut s.channel_groups[0];
        rd(t, "pan", &mut g0.pans[0]);
        rd(t, "span1", &mut g0.pan_stereo[0]);
        rd(t, "span2", &mut g0.pan_stereo[1]);
        if let Some(c) = t.child_with_name("CompressorState") {
            g0.compressor.apply(c);
        }
        apply_groups(&mut s.channel_groups, t.child_with_name("ChannelGroups"), count, Self::group_default);
        apply_groups(
            &mut s.multi_channel_groups,
            t.child_with_name("MultiChannelGroups"),
            multi_count,
            |_| ChannelGroup::default(),
        );
        s
    }
}

// ---- the whole state ----------------------------------------------------------------------------

/// Which optional parts a save includes or a load applies
/// (`getStateInformationWithOptions`/`setStateInformationWithOptions` flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scope {
    /// `includecache`: `RecentConnections` and `PeerStateCacheMap`.
    pub cache: bool,
    /// `includeInputGroups`: `InputChannelGroups`.
    pub input_groups: bool,
}

impl Scope {
    /// Host plugin state and `SonoBus.settings` (P.cpp:8604, SonoStandaloneFilterWindow.h:466).
    pub const FULL: Scope = Scope { cache: true, input_groups: true };
    /// `.sonobus` setup files (SonobusPluginEditor.cpp:2660, 2764).
    pub const SETUP_FILE: Scope = Scope { cache: false, input_groups: true };
    /// `PluginDefault.xml` (P.cpp:8809, 8821).
    pub const PLUGIN_DEFAULT: Scope = Scope { cache: false, input_groups: false };
}

/// The complete persisted processor state: the `SonoBusAoO` tree.
#[derive(Debug, Clone, PartialEq)]
pub struct AppState {
    /// Not persisted: selects upstream's plugin-vs-standalone defaults (P.cpp:769-790) for
    /// parameters and for input groups created while loading.
    pub is_plugin: bool,
    /// `PARAM` children.
    pub params: Params,
    /// `RecentConnections/ServerConnectionInfo`, most recent first.
    pub recents: Vec<ServerConnectionInfo>,
    /// `ExtraState`
    pub extra: ExtraState,
    /// `InputChannelGroups` children; `numChanGroups` = len (max 64). Upstream starts with 0
    /// until the audio device sets the count.
    pub input_groups: Vec<ChannelGroup>,
    /// `ExtraChannelGroups/ChannelGroup[chgID=filepb]`
    pub file_playback_group: ChannelGroup,
    /// `ExtraChannelGroups/ChannelGroup[chgID=met]`
    pub metronome_group: ChannelGroup,
    /// `ExtraChannelGroups/ChannelGroup[chgID=soundboard]`
    pub soundboard_group: ChannelGroup,
    /// `PeerStateCacheMap/PeerStateCache`. Upstream keeps a `std::map<String, ..>` keyed by name
    /// (P.cpp:8965-8987): a load keeps the first entry of a repeated name and leaves them sorted
    /// by name (code point order), and a save writes them in that order. [`AppState::apply_tree`]
    /// and [`AppState::to_tree_scoped`] do the same.
    pub peer_cache: Vec<PeerStateCache>,
    /// Root children this model does not know (including `PARAM`s with unknown ids), kept in
    /// order and written after the known ones. Upstream keeps them too (the loaded tree becomes
    /// its state).
    pub unknown_children: Vec<ValueTree>,
}

impl Default for AppState {
    fn default() -> Self {
        AppState::new(false)
    }
}

/// `PeerStateCacheMap` (a `std::map<String, ..>`) order: sorted by name, the first entry of a
/// repeated name kept. Rust `str` order is code point order, as JUCE `String::compare`.
fn peer_map_order(entries: &[PeerStateCache]) -> Vec<&PeerStateCache> {
    let mut v: Vec<_> = entries.iter().collect();
    v.sort_by(|a, b| a.name.cmp(&b.name)); // stable: equal names keep file order
    v.dedup_by(|later, first| later.name == first.name);
    v
}

const KNOWN_CHILDREN: &[&str] =
    &["RecentConnections", "ExtraState", "InputChannelGroups", "ExtraChannelGroups", "PeerStateCacheMap"];

impl AppState {
    /// A freshly constructed processor's state (upstream defaults).
    pub fn new(is_plugin: bool) -> Self {
        AppState {
            is_plugin,
            params: if is_plugin { Params::plugin() } else { Params::standalone() },
            recents: Vec::new(),
            extra: ExtraState::default(),
            input_groups: Vec::new(),
            file_playback_group: ChannelGroup::extra_default("File Playback", 2),
            metronome_group: ChannelGroup::extra_default("Metronome", 1),
            soundboard_group: ChannelGroup::extra_default("Soundboard", 2),
            peer_cache: Vec::new(),
            unknown_children: Vec::new(),
        }
    }

    /// Standalone defaults with a full-scope tree applied.
    pub fn from_tree(tree: &ValueTree) -> Self {
        let mut s = AppState::default();
        s.apply_tree(tree, Scope::FULL);
        s
    }

    /// `setStateInformationWithOptions` (P.cpp:8610-8797), which every stock load path uses
    /// (host state, settings, setup files, `PluginDefault.xml`): an invalid tree changes nothing;
    /// parts outside `scope` are ignored; a missing subtree or key leaves the current value,
    /// except:
    /// - every parameter without a `PARAM` (or without its `value`) resets to its default, in
    ///   every scope: `mState.state = tree` makes JUCE add a value-less `PARAM` for it, which
    ///   sets the default (juce_AudioProcessorValueTreeState.cpp:397-449);
    /// - unknown root children and unknown `ExtraState` properties are replaced by the tree's
    ///   (the loaded tree is what stock saves them from).
    pub fn apply_tree(&mut self, tree: &ValueTree, scope: Scope) {
        if !tree.is_valid() {
            return;
        }
        let defaults = if self.is_plugin { Params::plugin() } else { Params::standalone() };
        let default_values = defaults.values();
        let mut params = defaults;
        let mut unknown = Vec::new();
        for c in &tree.children {
            if c.type_name == "PARAM" {
                let id = c.get("id").map(var_to_string).unwrap_or_default();
                let default = default_values.iter().find(|(i, _)| *i == id).map(|&(_, v)| v);
                let value = c.get("value").map(|v| v.to_f64() as f32).or(default).unwrap_or(0.0);
                if !params.set_by_id(&id, value) {
                    unknown.push(c.clone());
                }
            } else if !KNOWN_CHILDREN.contains(&c.type_name.as_str()) {
                unknown.push(c.clone());
            }
        }
        self.params = params;
        self.unknown_children = unknown;

        if let Some(r) = tree.child_with_name("RecentConnections").filter(|_| scope.cache) {
            self.recents = r.children.iter().map(ServerConnectionInfo::from_tree).collect();
        }
        match tree.child_with_name("ExtraState") {
            Some(e) => self.extra.apply(e),
            None => self.extra.unknown.clear(),
        }
        if let Some(g) = tree.child_with_name("InputChannelGroups").filter(|_| scope.input_groups) {
            let mut count = self.input_groups.len() as i32;
            rd(g, "numChanGroups", &mut count);
            let plugin = self.is_plugin;
            apply_groups(&mut self.input_groups, Some(g), count, |i| ChannelGroup::input_default(i, plugin));
        }
        if let Some(x) = tree.child_with_name("ExtraChannelGroups") {
            for c in x.children.iter().filter(|c| c.is_valid()) {
                let mut g = ChannelGroup::default(); // P.cpp:8754: a fresh ChannelGroupParams
                g.apply(c);
                match c.get("chgID").map(var_to_string).as_deref() {
                    Some("filepb") => self.file_playback_group = g,
                    Some("met") => self.metronome_group = g,
                    Some("soundboard") => self.soundboard_group = g,
                    _ => {}
                }
            }
        }
        if let Some(m) = tree.child_with_name("PeerStateCacheMap").filter(|_| scope.cache) {
            let entries: Vec<_> = m.children.iter().map(PeerStateCache::from_tree).collect();
            self.peer_cache = peer_map_order(&entries).into_iter().cloned().collect();
        }
    }

    /// [`AppState::to_tree_scoped`] with [`Scope::FULL`].
    pub fn to_tree(&self) -> ValueTree {
        self.to_tree_scoped(Scope::FULL)
    }

    /// `getStateInformationWithOptions` (P.cpp:8490-8601) on a freshly started processor:
    /// `PARAM`s sorted by id, then `RecentConnections`, `ExtraState`, `InputChannelGroups`,
    /// `ExtraChannelGroups`, `PeerStateCacheMap` (sorted by name, see
    /// [`AppState::peer_cache`]), then [`AppState::unknown_children`].
    pub fn to_tree_scoped(&self, scope: Scope) -> ValueTree {
        let mut root = ValueTree::new("SonoBusAoO");
        let mut params = self.params.values();
        params.sort_by(|a, b| a.0.cmp(b.0));
        for (id, value) in params {
            let mut p = ValueTree::new("PARAM");
            wr(&mut p, "id", &id.to_string());
            wr(&mut p, "value", &value);
            root.children.push(p);
        }
        if scope.cache {
            let mut r = ValueTree::new("RecentConnections");
            r.children = self.recents.iter().map(ServerConnectionInfo::to_tree).collect();
            root.children.push(r);
        }
        root.children.push(self.extra.to_tree());
        if scope.input_groups {
            let groups = &self.input_groups[..self.input_groups.len().min(MAX_CHANGROUPS)];
            let mut g = groups_tree("InputChannelGroups", groups);
            wr(&mut g, "numChanGroups", &(groups.len() as i32));
            root.children.push(g);
        }
        let mut x = ValueTree::new("ExtraChannelGroups");
        for (g, id) in [
            (&self.file_playback_group, "filepb"),
            (&self.metronome_group, "met"),
            (&self.soundboard_group, "soundboard"),
        ] {
            let mut t = g.to_tree();
            wr(&mut t, "chgID", &id.to_string());
            x.children.push(t);
        }
        root.children.push(x);
        if scope.cache {
            let mut m = ValueTree::new("PeerStateCacheMap");
            m.children = peer_map_order(&self.peer_cache).into_iter().map(PeerStateCache::to_tree).collect();
            root.children.push(m);
        }
        root.children.extend(self.unknown_children.iter().cloned());
        root
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Upstream defaults, each checked against its source line.
    #[test]
    fn defaults_match_upstream() {
        let s = Params::standalone();
        // SonobusPluginProcessor.h:964-1001 Atomic initialisers, P.cpp:595-710 layout
        assert_eq!((s.in_gain, s.in_mon_mono_pan, s.in_mon_pan1, s.in_mon_pan2), (1.0, 0.0, -1.0, 1.0));
        assert_eq!((s.dry, s.wet, s.def_net_buf, s.send_channels), (0.0, 1.0, 0.001, 1));
        assert_eq!((s.met_gain, s.met_tempo, s.met_is_recorded), (0.5, 100.0, true));
        assert_eq!((s.main_reverb_level, s.main_reverb_size, s.main_reverb_damp), (1.0, 0.15, 0.5));
        assert_eq!((s.main_reverb_predelay, s.main_reverb_model), (20.0, 1));
        assert_eq!((s.def_net_auto, s.def_send_qual, s.def_peer_level, s.sync_met_host), (2, 4, 1.0, false));
        assert_eq!((s.in_reverb_level, s.in_reverb_size, s.in_reverb_damp, s.in_reverb_predelay), (1.0, 0.15, 0.5, 20.0));
        assert!(!s.met_enabled && !s.main_reverb_enabled && !s.sync_met_file && !s.reconnect_last);
        let p = Params::plugin();
        assert_eq!((p.dry, p.send_channels, p.sync_met_host), (1.0, 0, true));
        assert_eq!(s.values().len(), 37);

        // EffectParams.h:16-42, ChannelGroup.h:36-83, ChannelGroup.cpp:67-90
        let g = ChannelGroup::input_default(3, false);
        assert_eq!((g.chan_start, g.num_channels, g.gain, g.monitor), (3, 1, 1.0, 1.0));
        assert_eq!(g.compressor.makeup_gain_db, 4.0);
        assert_eq!((g.expander.threshold_db, g.expander.ratio, g.expander.attack_ms, g.expander.release_ms), (-60.0, 2.0, 1.0, 200.0));
        assert_eq!((g.limiter.enabled, g.limiter.threshold_db, g.limiter.ratio, g.limiter.attack_ms, g.limiter.release_ms), (true, -1.0, 4.0, 0.01, 100.0));
        assert!(!ChannelGroup::input_default(0, true).limiter.enabled);
        let e = ParametricEq::default();
        assert_eq!((e.low_shelf_freq, e.para1_freq, e.para1_q, e.para2_freq, e.para2_q, e.high_shelf_freq), (60.0, 90.0, 1.5, 360.0, 4.0, 10000.0));

        // SonobusPluginProcessor.h:1021-1069, 1213-1265
        let x = ExtraState::default();
        assert_eq!((x.default_recording_options, x.default_recording_format, x.default_recording_bits_per_sample), (1, 2, 16));
        assert_eq!((x.last_chat_width, x.last_soundboard_width, x.last_window_width, x.last_window_height), (250, 250, 800, 600));
        assert!(x.slider_snap_to_mouse && x.link_mon_delay_times && x.reconnect_after_server_loss && x.record_finish_open);
        assert_eq!(x.auto_drop_rate_thresh, 0.5);
        assert_eq!(VideoLinkInfo::default().push_view_mode, 0);
        let c = PeerStateCache::default();
        assert_eq!((c.netbuf, c.send_format, c.level, c.order_priority), (10.0, 4, 1.0, -1));
    }

    #[test]
    fn missing_keys_keep_current_values() {
        let mut s = AppState::new(true);
        s.extra.last_chat_width = 123;
        s.apply_tree(&ValueTree::new("SonoBusAoO"), Scope::FULL);
        assert_eq!(s.extra.last_chat_width, 123);
        assert_eq!(s.params.dry, 1.0);
        s.apply_tree(&ValueTree::default(), Scope::FULL);
        assert_eq!(s, {
            let mut x = AppState::new(true);
            x.extra.last_chat_width = 123;
            x
        });
    }

    fn param(id: &str, value: &str) -> ValueTree {
        let mut p = ValueTree::new("PARAM");
        p.set("id", Var::String(id.into()));
        p.set("value", Var::String(value.into()));
        p
    }

    // `mState.state = tree` re-adds a value-less PARAM for every parameter the tree lacks, which
    // resets it to its default (juce_AudioProcessorValueTreeState.cpp:397-449).
    #[test]
    fn missing_params_reset_to_defaults() {
        for plugin in [false, true] {
            let mut s = AppState::new(plugin);
            (s.params.dry, s.params.met_tempo, s.params.def_send_qual) = (0.5, 200.0, 9);
            s.extra.last_chat_width = 123;
            let mut t = ValueTree::new("SonoBusAoO");
            t.children.push(param("mettempo", "150"));
            s.apply_tree(&t, Scope::PLUGIN_DEFAULT);
            let defaults = if plugin { Params::plugin() } else { Params::standalone() };
            assert_eq!(s.params, Params { met_tempo: 150.0, ..defaults });
            assert_eq!(s.extra.last_chat_width, 123);
        }
    }

    // RangedAudioParameter::convertTo0to1/convertFrom0to1 snap and clamp to the range
    // (juce_RangedAudioParameter.cpp:39-49, juce_NormalisableRange.h:176-185).
    #[test]
    fn params_clamped_to_range_on_load() {
        let mut t = ValueTree::new("SonoBusAoO");
        for (id, v) in [
            ("defsendqual", "99"),
            ("mainreverbmodel", "7"),
            ("sendchannels", "1.5"),
            ("defnetauto", "-3"),
            ("mettempo", "120.4"),
            ("defnetbuf", "0.0204"),
            ("mainreverbpredelay", "1e9"),
            ("dry", "-1"),
            ("wet", "nan"),
            ("ingain", "inf"),
            ("metenabled", "0.5"),
            ("mastsendmute", "0.49"),
        ] {
            t.children.push(param(id, v));
        }
        let s = AppState::from_tree(&t);
        let p = &s.params;
        assert_eq!((p.def_send_qual, p.main_reverb_model, p.send_channels, p.def_net_auto), (14, 2, 2, 0));
        assert_eq!((p.met_tempo, p.def_net_buf, p.main_reverb_predelay), (120.0, 0.02, 100.0));
        assert_eq!((p.dry, p.wet, p.in_gain, p.met_enabled, p.main_send_mute), (0.0, 0.0, 4.0, true, false));
        assert_eq!(p.main_reverb_model_checked(), Some(ReverbModel::Zita));
        assert_eq!(p.send_channels_checked(), Some(SendChannels::Stereo));
        assert_eq!(p.def_net_auto_checked(), Some(NetBufAuto::Off));
        // in-range values are left exactly as they are
        let mut ok = Params::standalone();
        (ok.def_net_buf, ok.met_tempo, ok.dry) = (0.02, 120.0, 0.3);
        let mut ok2 = Params::plugin();
        for (id, v) in ok.values() {
            assert!(ok2.set_by_id(id, v));
        }
        assert_eq!(ok2, ok);
    }

    #[test]
    fn enum_accessors_preserve_unknown_values() {
        let x = ExtraState {
            default_recording_format: 9,
            peer_display_mode: 1,
            video_link: VideoLinkInfo { push_view_mode: 2, ..Default::default() },
            ..Default::default()
        };
        assert_eq!(x.default_recording_format_checked(), None);
        assert_eq!(x.peer_display_mode_checked(), Some(PeerDisplayMode::Minimal));
        assert_eq!(x.video_link.push_view_mode_checked(), Some(PushViewMode::ViewOnly));
        let c = PeerStateCache { netbufauto: 7, ..Default::default() };
        assert_eq!(c.netbufauto_checked(), None);
        let s = AppState { extra: x.clone(), ..Default::default() };
        assert_eq!(AppState::from_tree(&s.to_tree()).extra, x);
    }

    // PeerStateCacheMap is a std::map<String, PeerStateCache>: insert keeps the first entry of a
    // name, iteration (and so the next save) is sorted by name (P.cpp:8965-8987).
    #[test]
    fn peer_cache_is_a_sorted_map() {
        let entry = |name: &str, level: f32| PeerStateCache { name: name.into(), level, ..Default::default() };
        let peer_cache = vec![entry("zed", 0.1), entry("amy", 0.2), entry("zed", 0.3), entry("Émile", 0.4), entry("Bob", 0.5)];
        let s = AppState { peer_cache, ..Default::default() };
        let names = |t: &ValueTree| -> Vec<String> {
            let m = t.child_with_name("PeerStateCacheMap").unwrap();
            m.children.iter().map(|c| var_to_string(c.get("name").unwrap())).collect()
        };
        let tree = s.to_tree();
        assert_eq!(names(&tree), ["Bob", "amy", "zed", "Émile"]);
        // a file with duplicates in any order loads as the map would hold it
        let mut unsorted = tree.clone();
        let m = unsorted.children.iter_mut().find(|c| c.type_name == "PeerStateCacheMap").unwrap();
        m.children = s.peer_cache.iter().map(PeerStateCache::to_tree).collect();
        let back = AppState::from_tree(&unsorted);
        let got: Vec<_> = back.peer_cache.iter().map(|c| (c.name.as_str(), c.level)).collect();
        assert_eq!(got, [("Bob", 0.5), ("amy", 0.2), ("zed", 0.1), ("Émile", 0.4)]);
    }

    #[test]
    fn channel_count_clamped_and_pans_cover_every_channel() {
        let mut t = ValueTree::new("ChannelGroup");
        t.set("numchan", Var::String("1000".into()));
        let mut g = ChannelGroup::default();
        g.apply(&t);
        assert_eq!((g.num_channels, g.pans.len()), (64, 64));
        t.set("numchan", Var::Int(-5));
        g.apply(&t);
        assert_eq!(g.num_channels, 0);
    }

    #[test]
    fn eq_q_keys() {
        let mut t = ValueTree::new("ParametricEqState");
        t.set("p1q", Var::String("6.0".into()));
        let mut eq = ParametricEq::default();
        eq.apply(&t);
        assert_eq!((eq.para1_q, eq.para2_q), (6.0, 6.0));
        t.set("p1q", Var::Double(2.0));
        t.set("p2q", Var::Double(7.0));
        eq.apply(&t);
        assert_eq!((eq.para1_q, eq.para2_q), (2.0, 7.0));
    }

    #[test]
    fn legacy_peer_cache_entry() {
        let mut t = ValueTree::new("PeerStateCache");
        t.set("name", Var::String("old".into()));
        t.set("pan", Var::String("-0.5".into()));
        t.set("span1", Var::Double(-0.25));
        let mut comp = ValueTree::new("CompressorState");
        comp.set("enabled", Var::String("1".into()));
        t.children.push(comp);
        let c = PeerStateCache::from_tree(&t);
        assert_eq!(c.channel_groups.len(), 1);
        let g = &c.channel_groups[0];
        assert_eq!((g.pans[0], g.pan_stereo, g.compressor.enabled), (-0.5, [-0.25, 1.0], true));
    }
}
