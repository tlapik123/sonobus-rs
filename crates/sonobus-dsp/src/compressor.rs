//! Port of upstream `Source/faustCompressor.h` (Faust 2.28.3, `scripts/compressor2.dsp`)
//! plus the mapping in `ChannelGroup::commitCompressorParams` / `commitLimiterParams`.
//! The limiter is the same DSP (upstream never uses `faustLimiter.h`).

use crate::{Gate, ParamSpec, RATIO_ENGINE_MAX, ar_coef, cmax, cmin, inv_sample_rate, ms_to_s};

/// Stereo-linked feed-forward compressor with soft knee and smoothed makeup gain.
#[derive(Debug, Clone)]
pub struct Compressor {
    fconst0: f32,
    // Faust controls, in Faust units (dB, ratio, seconds).
    makeup_gain_db: f32, // fHslider0, smoothed by si.smoo
    ratio: f32,          // fHslider1
    knee_db: f32,        // fHslider2
    attack_s: f32,       // fHslider3
    release_s: f32,      // fHslider4
    threshold_db: f32,   // fHslider5
    // State.
    makeup_smooth: f32, // fRec0
    env_l: f32,         // fRec2 == fRec1
    env_r: f32,         // fRec4 == fRec3
    outgain_db: f32,    // fHbargraph0
    gate: Gate,
}

impl Compressor {
    pub const THRESHOLD_DB: ParamSpec =
        ParamSpec { min: -60.0, max: 0.0, step: 1.0, skew: 1.5, default: -16.0 };
    /// UI range; the engine accepts up to [`RATIO_ENGINE_MAX`].
    pub const RATIO: ParamSpec = ParamSpec { min: 1.0, max: 20.0, step: 0.1, skew: 0.5, default: 2.0 };
    pub const ATTACK_MS: ParamSpec =
        ParamSpec { min: 1.0, max: 1000.0, step: 1.0, skew: 0.5, default: 10.0 };
    pub const RELEASE_MS: ParamSpec =
        ParamSpec { min: 1.0, max: 1000.0, step: 1.0, skew: 0.5, default: 80.0 };
    /// Manual makeup range. Upstream defaults to auto makeup, which gives 4 dB at the
    /// default threshold/ratio.
    pub const MAKEUP_GAIN_DB: ParamSpec =
        ParamSpec { min: 0.0, max: 60.0, step: 1.0, skew: 0.75, default: 0.0 };
    pub const AUTO_MAKEUP_DEFAULT: bool = true;
    /// Fixed by `commitCompressorParams`.
    pub const KNEE_DB: f32 = 2.0;

    /// Upstream defaults (ChannelGroup init + commit), disabled.
    pub fn new(sample_rate: u32) -> Self {
        let mut c = Self::faust_init(sample_rate);
        c.knee_db = Self::KNEE_DB;
        c.threshold_db = Self::THRESHOLD_DB.default;
        c.ratio = Self::RATIO.default;
        c.set_attack_ms(Self::ATTACK_MS.default);
        c.set_release_ms(Self::RELEASE_MS.default);
        c.makeup_gain_db = auto_makeup_gain_db(c.threshold_db, c.ratio);
        c
    }

    /// Faust `init()`: UI zones at their Faust defaults, state cleared.
    fn faust_init(sample_rate: u32) -> Self {
        Self {
            fconst0: inv_sample_rate(sample_rate),
            makeup_gain_db: 0.0,
            ratio: 2.0,
            knee_db: 3.0,
            attack_s: 0.002,
            release_s: 0.5,
            threshold_db: -20.0,
            makeup_smooth: 0.0,
            env_l: 0.0,
            env_r: 0.0,
            outgain_db: 0.0,
            gate: Gate::default(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.gate.enabled = enabled;
    }
    pub fn enabled(&self) -> bool {
        self.gate.enabled
    }
    pub fn set_threshold_db(&mut self, db: f32) {
        self.threshold_db = db;
    }
    /// Clamped to `1..=RATIO_ENGINE_MAX` like the engine does.
    pub fn set_ratio(&mut self, ratio: f32) {
        self.ratio = ratio.clamp(1.0, RATIO_ENGINE_MAX);
    }
    pub fn set_attack_ms(&mut self, ms: f32) {
        self.attack_s = ms_to_s(ms);
    }
    pub fn set_release_ms(&mut self, ms: f32) {
        self.release_s = ms_to_s(ms);
    }
    /// Target makeup gain; the DSP glides to it (one-pole, 0.999 per sample). With auto
    /// makeup on, pass [`auto_makeup_gain_db`] whenever threshold or ratio change.
    pub fn set_makeup_gain_db(&mut self, db: f32) {
        self.makeup_gain_db = db;
    }

    /// The `/compressor/outgain` bargraph: gain reduction in dB (<= 0, makeup excluded)
    /// of the last processed sample. Upstream shows it as [`reduction_meter_level`].
    pub fn gain_reduction_db(&self) -> f32 {
        self.outgain_db
    }

    /// Faust `instanceClear()`: clears envelopes and smoothers, keeps parameters.
    pub fn reset(&mut self) {
        self.makeup_smooth = 0.0;
        self.env_l = 0.0;
        self.env_r = 0.0;
    }

    pub fn process(&mut self, left: &mut [f32], right: Option<&mut [f32]>) {
        if self.gate.run() {
            self.compute(left, right);
        }
    }

    fn compute(&mut self, left: &mut [f32], right: Option<&mut [f32]>) {
        let k = Coefs {
            makeup: 0.001 * self.makeup_gain_db,
            one_minus_ratio: 1.0 - self.ratio,
            knee: self.knee_db,
            attack: ar_coef(self.fconst0, self.attack_s),
            release: ar_coef(self.fconst0, self.release_s),
            threshold: self.threshold_db,
            inv_knee: 1.0 / (self.knee_db + 0.001),
        };
        match right {
            Some(right) => {
                debug_assert_eq!(left.len(), right.len());
                for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                    let g = self.tick(&k, *l, *r);
                    *l *= g;
                    *r *= g;
                }
            }
            // Upstream feeds a silent dummy buffer as the second channel.
            None => {
                for l in left.iter_mut() {
                    *l *= self.tick(&k, *l, 0.0);
                }
            }
        }
    }

    #[inline(always)]
    fn tick(&mut self, k: &Coefs, x0: f32, x1: f32) -> f32 {
        self.makeup_smooth = k.makeup + 0.999 * self.makeup_smooth;
        let a0 = x0.abs();
        let c0 = if self.env_l > a0 { k.release } else { k.attack };
        self.env_l = self.env_l * c0 + a0 * (1.0 - c0);
        let a1 = x1.abs();
        let c1 = if self.env_r > a1 { k.release } else { k.attack };
        self.env_r = self.env_r * c1 + a1 * (1.0 - c1);
        let over = cmax(0.0, k.knee + (20.0 * cmax(self.env_l, self.env_r).log10() - k.threshold));
        let p = cmin(1.0, cmax(0.0, k.inv_knee * over));
        let red = k.one_minus_ratio * ((over * p) / (1.0 - k.one_minus_ratio * p));
        self.outgain_db = red;
        10.0f32.powf(0.05 * (self.makeup_smooth + red))
    }
}

struct Coefs {
    makeup: f32,
    one_minus_ratio: f32,
    knee: f32,
    attack: f32,
    release: f32,
    threshold: f32,
    inv_knee: f32,
}

/// Upstream auto makeup: `(-thresh - |thresh / ratio|) / 2`.
pub fn auto_makeup_gain_db(threshold_db: f32, ratio: f32) -> f32 {
    (-threshold_db - (threshold_db / ratio).abs()) * 0.5
}

/// Meter value upstream feeds to `setReductionLevel`:
/// `jlimit(0, 1, Decibels::decibelsToGain(outgain))` (JUCE floor -100 dB -> 0).
pub fn reduction_meter_level(gain_reduction_db: f32) -> f32 {
    let g = if gain_reduction_db > -100.0 { 10.0f32.powf(gain_reduction_db * 0.05) } else { 0.0 };
    g.clamp(0.0, 1.0)
}

/// Input limiter: the compressor DSP driven by `commitLimiterParams`, which never sets
/// knee or makeup, so those stay at the Faust defaults (3 dB, 0 dB).
#[derive(Debug, Clone)]
pub struct Limiter(Compressor);

impl Limiter {
    pub const THRESHOLD_DB: f32 = -1.0;
    pub const RATIO: f32 = 4.0;
    pub const ATTACK_MS: f32 = 0.01;
    pub const RELEASE_MS: f32 = 100.0;
    /// `limiterParams.enabled = !isplugin`.
    pub const ENABLED_DEFAULT_STANDALONE: bool = true;
    pub const ENABLED_DEFAULT_PLUGIN: bool = false;

    /// Upstream limiter settings, disabled.
    pub fn new(sample_rate: u32) -> Self {
        let mut c = Compressor::faust_init(sample_rate);
        c.threshold_db = Self::THRESHOLD_DB;
        c.ratio = Self::RATIO;
        c.set_attack_ms(Self::ATTACK_MS);
        c.set_release_ms(Self::RELEASE_MS);
        Self(c)
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.0.set_enabled(enabled);
    }
    pub fn enabled(&self) -> bool {
        self.0.enabled()
    }
    pub fn set_threshold_db(&mut self, db: f32) {
        self.0.set_threshold_db(db);
    }
    pub fn set_ratio(&mut self, ratio: f32) {
        self.0.set_ratio(ratio);
    }
    pub fn set_attack_ms(&mut self, ms: f32) {
        self.0.set_attack_ms(ms);
    }
    pub fn set_release_ms(&mut self, ms: f32) {
        self.0.set_release_ms(ms);
    }
    pub fn reset(&mut self) {
        self.0.reset();
    }
    pub fn process(&mut self, left: &mut [f32], right: Option<&mut [f32]>) {
        self.0.process(left, right);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auto_makeup_matches_upstream_default() {
        assert_eq!(auto_makeup_gain_db(-16.0, 2.0), 4.0);
    }

    #[test]
    fn meter_level() {
        assert_eq!(reduction_meter_level(0.0), 1.0);
        assert_eq!(reduction_meter_level(-100.0), 0.0);
        assert!((reduction_meter_level(-20.0) - 0.1).abs() < 1e-6);
    }

    #[test]
    fn disabled_is_passthrough_after_one_tail_block() {
        let mut c = Compressor::new(48000);
        let mut buf = [0.5f32; 64];
        c.process(&mut buf, None);
        assert_eq!(buf, [0.5; 64]);
        c.set_enabled(true);
        c.process(&mut buf, None);
        assert_ne!(buf, [0.5; 64]);
        c.set_enabled(false);
        let mut tail = [0.5f32; 64];
        c.process(&mut tail, None); // upstream still processes this block
        assert_ne!(tail, [0.5; 64]);
        let mut off = [0.5f32; 64];
        c.process(&mut off, None);
        assert_eq!(off, [0.5; 64]);
    }
}
