//! Port of upstream `Source/faustExpander.h` (Faust 2.28.3, `scripts/expander.dsp`) plus
//! the mapping in `ChannelGroup::commitExpanderParams`.
//!
//! Deliberate divergence from upstream: with ratio exactly 1 and an envelope of exactly 0
//! (digital silence) upstream computes the gain as `0 * inf = NaN`, so silent input comes
//! out NaN and poisons everything downstream. Here ratio 1 takes the formula's limit
//! (no expansion, unity gain), so silence in gives silence out. Every other sample is
//! bit-identical to upstream (at ratio 1 upstream's gain is `10^(0.05 * ±0) = 1` too).
//! Upstream also never reads the expander's gain bargraph (it asks for
//! `/expander/outgain`, the zone is `/expander/gain`), so no meter is exposed here.

use crate::{Gate, ParamSpec, RATIO_ENGINE_MAX, ar_coef, cmax, cmin, inv_sample_rate, ms_to_s};

/// Stereo-linked downward expander / noise gate with soft knee.
#[derive(Debug, Clone)]
pub struct Expander {
    fconst0: f32,
    ratio: f32,        // fHslider0
    threshold_db: f32, // fHslider1
    knee_db: f32,      // fHslider2
    attack_s: f32,     // fHslider3
    release_s: f32,    // fHslider4
    env_l: f32,        // fRec1 == fRec0
    env_r: f32,        // fRec3 == fRec2
    gate: Gate,
}

impl Expander {
    pub const THRESHOLD_DB: ParamSpec =
        ParamSpec { min: -96.0, max: 0.0, step: 1.0, skew: 1.5, default: -60.0 };
    /// UI range; the engine accepts up to [`RATIO_ENGINE_MAX`].
    pub const RATIO: ParamSpec = ParamSpec { min: 1.0, max: 20.0, step: 0.1, skew: 0.5, default: 2.0 };
    pub const ATTACK_MS: ParamSpec =
        ParamSpec { min: 1.0, max: 1000.0, step: 1.0, skew: 0.5, default: 1.0 };
    pub const RELEASE_MS: ParamSpec =
        ParamSpec { min: 1.0, max: 1000.0, step: 1.0, skew: 0.5, default: 200.0 };
    /// Fixed by `commitExpanderParams`.
    pub const KNEE_DB: f32 = 3.0;

    /// Upstream defaults, disabled.
    pub fn new(sample_rate: u32) -> Self {
        let mut e = Self {
            fconst0: inv_sample_rate(sample_rate),
            ratio: Self::RATIO.default,
            threshold_db: Self::THRESHOLD_DB.default,
            knee_db: Self::KNEE_DB,
            attack_s: 0.0,
            release_s: 0.0,
            env_l: 0.0,
            env_r: 0.0,
            gate: Gate::default(),
        };
        e.set_attack_ms(Self::ATTACK_MS.default);
        e.set_release_ms(Self::RELEASE_MS.default);
        e
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

    /// Faust `instanceClear()`: clears envelopes, keeps parameters.
    pub fn reset(&mut self) {
        self.env_l = 0.0;
        self.env_r = 0.0;
    }

    pub fn process(&mut self, left: &mut [f32], right: Option<&mut [f32]>) {
        if !self.gate.run() {
            return;
        }
        let one_minus_ratio = 1.0 - self.ratio;
        let thresh_plus_knee = self.threshold_db + self.knee_db;
        let attack = ar_coef(self.fconst0, self.attack_s);
        let release = ar_coef(self.fconst0, self.release_s);
        let inv_knee = 1.0 / (self.knee_db + 0.001);
        let mut tick = |x0: f32, x1: f32| {
            let a0 = x0.abs();
            let c0 = if self.env_l > a0 { release } else { attack };
            self.env_l = self.env_l * c0 + a0 * (1.0 - c0);
            let a1 = x1.abs();
            let c1 = if self.env_r > a1 { release } else { attack };
            self.env_r = self.env_r * c1 + a1 * (1.0 - c1);
            let under = cmax(0.0, thresh_plus_knee - 20.0 * cmax(self.env_l, self.env_r).log10());
            // `under` is +inf on silence; `0 * inf` would be NaN (see module docs).
            let gain_db = if one_minus_ratio == 0.0 {
                0.0
            } else {
                one_minus_ratio * (under * cmin(1.0, cmax(0.0, inv_knee * under)))
            };
            10.0f32.powf(0.05 * gain_db)
        };
        match right {
            Some(right) => {
                debug_assert_eq!(left.len(), right.len());
                for (l, r) in left.iter_mut().zip(right.iter_mut()) {
                    let g = tick(*l, *r);
                    *l *= g;
                    *r *= g;
                }
            }
            None => {
                for l in left.iter_mut() {
                    *l *= tick(*l, 0.0);
                }
            }
        }
    }
}
