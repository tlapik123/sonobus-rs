//! The engine's two reverbs as `SonobusPluginProcessor.cpp` drives them: the main reverb
//! (selectable model, fed by the per-channel `monreverbsend` sends) and the input reverb
//! (always MVerb, fed by `inreverbsend`). Both run wet-only on a stereo FX bus that the
//! engine fills with the sends and then mixes into the output.
//!
//! Parameter mapping: P.cpp:6608-6690 (`parameterChanged`) and 6921-6973 (`prepareToPlay`).
//! Enable/model handling: P.cpp:7759-7782 (input) and 8105-8199 (main).
//!
//! Deliberate divergences from upstream (all at init or on mono outputs):
//! - Freeverb wet gain is `level * 0.35` (upstream's `parameterChanged`); upstream's
//!   constructor uses `level * 0.5` until the level parameter is first touched.
//! - Zita's pre-delay always follows the parameter; upstream's `prepareToPlay` re-inits
//!   Zita and leaves `In_Delay` at Faust's 60 ms until the pre-delay is next changed.
//! - The FX bus is always stereo. With a mono output bus upstream runs Freeverb in mono
//!   and skips MVerb/Zita entirely (so the dry send leaks into the output); here the
//!   engine downmixes the stereo result instead.
//! - Main reverb disable fades the last processed block out (as the input reverb does);
//!   upstream skips that fade and cuts the tail at the next block boundary.

use crate::gain::apply_gain_ramp;
use crate::{Freeverb, MVerb, MVerbParam, ParamSpec, Zita};

/// `mainreverbmodel` choice, in upstream's index order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReverbModel {
    Freeverb = 0,
    #[default]
    MVerb = 1,
    Zita = 2,
}

/// App-level reverb controls shared by both reverbs (`*reverblevel/size/damp/predelay`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReverbParams {
    pub level: f32,
    pub size: f32,
    pub damping: f32,
    pub predelay_ms: f32,
}

impl ReverbParams {
    pub const LEVEL: ParamSpec = ParamSpec { min: 0.0, max: 1.0, step: 0.0, skew: 0.4, default: 1.0 };
    pub const SIZE: ParamSpec = ParamSpec { min: 0.0, max: 1.0, step: 0.0, skew: 1.0, default: 0.15 };
    pub const DAMPING: ParamSpec = ParamSpec { min: 0.0, max: 1.0, step: 0.0, skew: 1.0, default: 0.5 };
    pub const PREDELAY_MS: ParamSpec = ParamSpec { min: 0.0, max: 100.0, step: 1.0, skew: 1.0, default: 20.0 };
}

impl Default for ReverbParams {
    fn default() -> Self {
        Self {
            level: Self::LEVEL.default,
            size: Self::SIZE.default,
            damping: Self::DAMPING.default,
            predelay_ms: Self::PREDELAY_MS.default,
        }
    }
}

/// JUCE `jmap(v, lo, hi)`.
fn jmap(v: f32, lo: f32, hi: f32) -> f32 {
    lo + v * (hi - lo)
}

/// JUCE `jlimit(lo, hi, v)`.
fn jlimit(lo: f32, hi: f32, v: f32) -> f32 {
    if v < lo {
        lo
    } else if hi < v {
        hi
    } else {
        v
    }
}

/// `jmap(predelay, 0, 100, 0, 0.5)`: MVerb's pre-delay control (1.0 = 200 ms).
fn mverb_predelay(ms: f32) -> f32 {
    0.0 + ((0.5 - 0.0) * (ms - 0.0)) / (100.0 - 0.0)
}

fn mverb_set_size(m: &mut MVerb, size: f32) {
    m.set_parameter(MVerbParam::Size, jmap(size, 0.45, 0.95));
    m.set_parameter(MVerbParam::Decay, jmap(size, 0.45, 0.95));
}

/// MVerb as configured by `prepareToPlay`.
fn new_mverb(sample_rate: u32, p: ReverbParams) -> MVerb {
    let mut m = MVerb::new(sample_rate);
    m.set_parameter(MVerbParam::Mix, 1.0);
    m.set_parameter(MVerbParam::Gain, jmap(p.level, 0.0, 0.8));
    mverb_set_size(&mut m, p.size);
    m.set_parameter(MVerbParam::EarlyMix, 0.75);
    m.set_parameter(MVerbParam::PreDelay, mverb_predelay(p.predelay_ms));
    m.set_parameter(MVerbParam::DampingFreq, jmap(p.damping, 0.0, 0.85));
    m.set_parameter(MVerbParam::BandwidthFreq, 1.0);
    m.set_parameter(MVerbParam::Density, 0.5);
    m
}

/// Freeverb `(roomSize, damping, wetLevel)`.
fn freeverb_params(p: ReverbParams) -> (f32, f32, f32) {
    (jmap(p.size, 0.55, 1.0), p.damping, p.level * 0.35)
}

/// JUCE `Decibels::gainToDecibels` (floor -100 dB).
fn gain_to_db(g: f32) -> f32 {
    if g > 0.0 {
        let db = g.log10() * 20.0;
        if -100.0 < db { db } else { -100.0 }
    } else {
        -100.0
    }
}

/// Per-block enable state shared by both reverbs: `enabled || last` processes one more
/// block after disabling.
#[derive(Debug, Clone, Copy, Default)]
struct EnableState {
    enabled: bool,
    last: bool,
}

impl EnableState {
    fn active(&self) -> bool {
        self.enabled || self.last
    }
}

/// The main (monitor) reverb with its three selectable models.
#[derive(Debug, Clone)]
pub struct MainReverb {
    params: ReverbParams,
    model: ReverbModel,
    last_model: ReverbModel,
    state: EnableState,
    last_active: bool,
    freeverb: Freeverb,
    mverb: MVerb,
    zita: Zita,
}

impl MainReverb {
    /// Upstream defaults (model MVerb), disabled.
    pub fn new(sample_rate: u32) -> Self {
        let p = ReverbParams::default();
        let (room, damp, wet) = freeverb_params(p);
        let mut zita = Zita::new(sample_rate);
        zita.set_dry_wet(-1.0);
        let mut r = Self {
            params: p,
            model: ReverbModel::default(),
            last_model: ReverbModel::default(),
            state: EnableState::default(),
            last_active: false,
            freeverb: Freeverb::new(sample_rate, room, damp, wet),
            mverb: new_mverb(sample_rate, p),
            zita,
        };
        r.set_params(p); // Zita's controls; a no-op for the other two
        r
    }

    fn update_freeverb(&mut self) {
        let (room, damp, wet) = freeverb_params(self.params);
        self.freeverb.set_parameters(room, damp, wet);
    }

    pub fn params(&self) -> ReverbParams {
        self.params
    }

    /// Applies all four controls (each through its own setter, so unchanged values have
    /// no side effects).
    pub fn set_params(&mut self, p: ReverbParams) {
        self.set_level(p.level);
        self.set_size(p.size);
        self.set_damping(p.damping);
        self.set_predelay_ms(p.predelay_ms);
    }

    /// `nmainreverblevel`, 0..1.
    pub fn set_level(&mut self, level: f32) {
        self.params.level = level;
        self.update_freeverb();
        self.mverb.set_parameter(MVerbParam::Gain, jmap(level, 0.0, 0.8));
        self.zita.set_level_db(jlimit(-70.0, 40.0, gain_to_db(level) + 6.0));
    }

    /// `mainreverbsize`, 0..1. For MVerb this clears the tank (upstream behaviour).
    pub fn set_size(&mut self, size: f32) {
        self.params.size = size;
        self.update_freeverb();
        mverb_set_size(&mut self.mverb, size);
        let rt60 = jlimit(1.0, 8.0, size * 7.0 + 1.0);
        self.zita.set_low_rt60(rt60);
        self.zita.set_mid_rt60(rt60);
    }

    /// `mainreverbdamp`, 0..1.
    pub fn set_damping(&mut self, damping: f32) {
        self.params.damping = damping;
        self.update_freeverb();
        self.zita.set_hf_damping(jmap(damping, 23520.0, 1500.0));
        self.mverb.set_parameter(MVerbParam::DampingFreq, jmap(damping, 0.0, 0.85));
    }

    /// `mainreverbpredelay`, 0..100 ms. Freeverb has no pre-delay.
    pub fn set_predelay_ms(&mut self, ms: f32) {
        self.params.predelay_ms = ms;
        self.zita.set_in_delay_ms(jlimit(0.0, 100.0, ms));
        self.mverb.set_parameter(MVerbParam::PreDelay, mverb_predelay(ms));
    }

    pub fn model(&self) -> ReverbModel {
        self.model
    }
    /// Takes effect at the next block: all models are cleared and the new one starts
    /// from silence (no crossfade, the old tail is cut).
    pub fn set_model(&mut self, model: ReverbModel) {
        self.model = model;
    }

    pub fn enabled(&self) -> bool {
        self.state.enabled
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        self.state.enabled = enabled;
    }

    /// Whether the next `process` call uses the bus: the engine only needs to fill the
    /// sends (and mix the result) when this is true.
    pub fn is_active(&self) -> bool {
        self.state.active()
    }

    /// Clears all three models' state (allocation-free) and makes the next block an enabling
    /// one: when enabled it starts from silence with the enable fade-in; a pending disable fade
    /// is dropped. Parameters, model and the enable flag are kept.
    pub fn reset(&mut self) {
        self.freeverb.reset();
        self.mverb.reset();
        self.zita.reset();
        self.state.last = false;
        self.last_active = false;
    }

    /// Processes the stereo FX bus (the summed sends) in place into wet reverb output.
    /// Returns `false` when the reverb is off; the bus must then not be mixed in.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) -> bool {
        let active = self.state.active();
        let EnableState { enabled, last } = self.state;
        if (enabled && !last) || (active && self.model != self.last_model) {
            self.freeverb.reset();
            self.mverb.reset();
            self.zita.reset();
        }
        if active {
            match self.model {
                ReverbModel::Freeverb => self.freeverb.process(left, right),
                ReverbModel::MVerb => self.mverb.process(left, right),
                ReverbModel::Zita => self.zita.process(left, right),
            }
            let ramp = if !self.last_active {
                Some((0.0, 1.0))
            } else if !enabled && last {
                Some((1.0, 0.0))
            } else {
                None
            };
            if let Some((a, b)) = ramp {
                apply_gain_ramp(left, a, b);
                apply_gain_ramp(right, a, b);
            }
        }
        self.last_active = active;
        self.state.last = enabled;
        self.last_model = self.model;
        active
    }
}

/// The input reverb: MVerb on the sum of the `inreverbsend` sends, mixed into what we
/// send and (scaled by the monitor level) into our own monitor.
#[derive(Debug, Clone)]
pub struct InputReverb {
    params: ReverbParams,
    state: EnableState,
    mverb: MVerb,
}

impl InputReverb {
    /// Upstream defaults, disabled.
    pub fn new(sample_rate: u32) -> Self {
        let params = ReverbParams::default();
        Self { params, state: EnableState::default(), mverb: new_mverb(sample_rate, params) }
    }

    pub fn params(&self) -> ReverbParams {
        self.params
    }
    pub fn set_params(&mut self, p: ReverbParams) {
        self.set_level(p.level);
        self.set_size(p.size);
        self.set_damping(p.damping);
        self.set_predelay_ms(p.predelay_ms);
    }
    /// `inreverblevel`, 0..1.
    pub fn set_level(&mut self, level: f32) {
        self.params.level = level;
        self.mverb.set_parameter(MVerbParam::Gain, jmap(level, 0.0, 0.8));
    }
    /// `inreverbsize`, 0..1. Clears the tank (upstream behaviour).
    pub fn set_size(&mut self, size: f32) {
        self.params.size = size;
        mverb_set_size(&mut self.mverb, size);
    }
    /// `inreverbdamp`, 0..1.
    pub fn set_damping(&mut self, damping: f32) {
        self.params.damping = damping;
        self.mverb.set_parameter(MVerbParam::DampingFreq, jmap(damping, 0.0, 0.85));
    }
    /// `inreverbpredelay`, 0..100 ms.
    pub fn set_predelay_ms(&mut self, ms: f32) {
        self.params.predelay_ms = ms;
        self.mverb.set_parameter(MVerbParam::PreDelay, mverb_predelay(ms));
    }

    pub fn enabled(&self) -> bool {
        self.state.enabled
    }
    /// Upstream enables it while any input group has `inreverbsend > 0`.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.state.enabled = enabled;
    }
    /// See [`MainReverb::is_active`].
    pub fn is_active(&self) -> bool {
        self.state.active()
    }

    /// See [`MainReverb::reset`].
    pub fn reset(&mut self) {
        self.mverb.reset();
        self.state.last = false;
    }

    /// Processes the stereo input-reverb bus in place (wet only). Enabling clears the
    /// reverb and fades in over the block; disabling fades the last block out.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) -> bool {
        let active = self.state.active();
        let EnableState { enabled, last } = self.state;
        if active {
            if enabled && !last {
                self.mverb.reset();
            }
            self.mverb.process(left, right);
            if enabled != last {
                let (a, b) = if enabled { (0.0, 1.0) } else { (1.0, 0.0) };
                apply_gain_ramp(left, a, b);
                apply_gain_ramp(right, a, b);
            }
        }
        self.state.last = enabled;
        active
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, seed: &mut u32) -> Vec<f32> {
        (0..len)
            .map(|_| {
                *seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (*seed >> 8) as f32 / (1u32 << 24) as f32 - 0.5
            })
            .collect()
    }

    /// After `reset` the tank is empty: silence in gives exact silence out, for every model
    /// (without it the tail of the noise rings on), and the reverb works again afterwards.
    #[test]
    fn reset_clears_the_tail() {
        let mut seed = 1;
        for model in [ReverbModel::Freeverb, ReverbModel::MVerb, ReverbModel::Zita] {
            let tail = [false, true].map(|reset| {
                let mut r = MainReverb::new(48000);
                r.set_model(model);
                r.set_enabled(true);
                for _ in 0..20 {
                    let (mut l, mut rr) = (noise(256, &mut seed), noise(256, &mut seed));
                    r.process(&mut l, &mut rr);
                }
                if reset {
                    r.reset();
                    assert!(r.enabled() && r.is_active());
                }
                let mut peak = 0.0f32;
                for _ in 0..4 {
                    let (mut l, mut rr) = (vec![0.0; 256], vec![0.0; 256]);
                    assert!(r.process(&mut l, &mut rr));
                    peak = l.iter().chain(&rr).fold(peak, |m, v| m.max(v.abs()));
                }
                // (Freeverb's shortest comb is longer than one block)
                let mut out = false;
                for _ in 0..8 {
                    let (mut l, mut rr) = (noise(256, &mut seed), noise(256, &mut seed));
                    r.process(&mut l, &mut rr);
                    out |= l.iter().chain(&rr).any(|v| *v != 0.0);
                }
                assert!(out, "{model:?} works again");
                peak
            });
            assert!(tail[0] > 1e-4, "{model:?}: {}", tail[0]);
            // (MVerb's filters leave values around 1e-25 on silence, a fresh one too)
            assert!(tail[1] < 1e-20, "{model:?}: {}", tail[1]);
        }
        let mut r = InputReverb::new(48000);
        r.set_enabled(true);
        for _ in 0..20 {
            let (mut l, mut rr) = (noise(256, &mut seed), noise(256, &mut seed));
            r.process(&mut l, &mut rr);
        }
        r.reset();
        let (mut l, mut rr) = (vec![0.0; 256], vec![0.0; 256]);
        assert!(r.process(&mut l, &mut rr));
        assert!(l.iter().chain(&rr).all(|v| v.abs() < 1e-20));
        // disabled: reset keeps it off
        r.set_enabled(false);
        r.reset();
        assert!(!r.is_active());
    }
}
