//! Port of upstream `Source/faustParametricEQ.h` (Faust, `scripts/parametric_eq.dsp`:
//! low shelf -> 2 peaking sections -> high shelf) plus `ChannelGroup::commitEqParams`.
//! Upstream runs two mono instances (one per channel) with identical controls.
//!
//! Upstream quirks kept on purpose:
//! - every control except para1 Q is smoothed (one-pole, 0.999/sample) starting from 0
//!   after init/reset, so the bands glide in from 0 Hz / 0 dB;
//! - para1 Q is not smoothed, para2 Q is.
//!
//! The arithmetic below is a transliteration of the generated `compute`; names follow
//! the Faust variables so it can be checked line by line against the header.

use crate::{Gate, ParamSpec};
use std::f32::consts::{PI, TAU};

#[derive(Debug, Clone, Copy)]
struct Controls {
    ls_freq: f32,  // fHslider0
    ls_gain: f32,  // fHslider1
    p1_freq: f32,  // fHslider2
    p1_gain: f32,  // fHslider3
    p1_q: f32,     // fHslider4
    p2_freq: f32,  // fHslider5
    p2_gain: f32,  // fHslider6
    p2_q: f32,     // fHslider7
    hs_freq: f32,  // fHslider8
    hs_gain: f32,  // fHslider9
}

/// Per-instance Faust state (`instanceClear` zeroes all of it).
#[derive(Debug, Clone, Copy, Default)]
struct State {
    vec0: [f32; 2],
    rec6: [f32; 2],
    rec5: [f32; 2],
    rec4: [f32; 3],
    rec8: [f32; 2],
    rec7: [f32; 3],
    rec9: [f32; 2],
    rec10: [f32; 2],
    rec11: [f32; 2],
    rec3: [f32; 3],
    rec12: [f32; 2],
    rec13: [f32; 2],
    rec14: [f32; 2],
    rec2: [f32; 3],
    vec1: [f32; 2],
    rec15: [f32; 2],
    rec1: [f32; 2],
    rec0: [f32; 3],
    rec16: [f32; 2],
    rec18: [f32; 2],
    rec17: [f32; 3],
}

/// Per-block constants (`fSlow*`).
struct Slow {
    s0: f32,
    s1: f32,
    s2: f32,
    s3: f32,
    s4: f32,
    s5: f32,
    s6: f32,
    s7: f32,
    s8: f32,
    s9: f32,
}

/// 4-band parametric EQ: low shelf, two peaking bands, high shelf.
#[derive(Debug, Clone)]
pub struct ParametricEq {
    c1: f32, // fConst1 = pi / SR
    c2: f32, // fConst2 = 2 pi / SR
    ctl: Controls,
    st: [State; 2],
    gate: Gate,
}

const GAIN_DB: ParamSpec = ParamSpec { min: -24.0, max: 24.0, step: 0.1, skew: 1.0, default: 0.0 };
const PEAK_FREQ: ParamSpec = ParamSpec { min: 40.0, max: 10000.0, step: 1.0, skew: 0.3, default: 0.0 };
const PEAK_Q: ParamSpec = ParamSpec { min: 0.4, max: 100.0, step: 0.1, skew: 0.35, default: 0.0 };

impl ParametricEq {
    pub const LOW_SHELF_GAIN_DB: ParamSpec = GAIN_DB;
    pub const LOW_SHELF_FREQ_HZ: ParamSpec =
        ParamSpec { min: 20.0, max: 2000.0, step: 1.0, skew: 0.25, default: 60.0 };
    pub const PARA1_GAIN_DB: ParamSpec = GAIN_DB;
    pub const PARA1_FREQ_HZ: ParamSpec = ParamSpec { default: 90.0, ..PEAK_FREQ };
    pub const PARA1_Q: ParamSpec = ParamSpec { default: 1.5, ..PEAK_Q };
    pub const PARA2_GAIN_DB: ParamSpec = GAIN_DB;
    pub const PARA2_FREQ_HZ: ParamSpec = ParamSpec { default: 360.0, ..PEAK_FREQ };
    pub const PARA2_Q: ParamSpec = ParamSpec { default: 4.0, ..PEAK_Q };
    pub const HIGH_SHELF_GAIN_DB: ParamSpec = GAIN_DB;
    pub const HIGH_SHELF_FREQ_HZ: ParamSpec =
        ParamSpec { min: 500.0, max: 16000.0, step: 1.0, skew: 0.5, default: 10000.0 };

    /// Upstream defaults, disabled.
    pub fn new(sample_rate: u32) -> Self {
        let sr = crate::cmin(192000.0, crate::cmax(1.0, sample_rate as f32));
        Self {
            c1: PI / sr,
            c2: TAU / sr,
            ctl: Controls {
                ls_freq: Self::LOW_SHELF_FREQ_HZ.default,
                ls_gain: Self::LOW_SHELF_GAIN_DB.default,
                p1_freq: Self::PARA1_FREQ_HZ.default,
                p1_gain: Self::PARA1_GAIN_DB.default,
                p1_q: Self::PARA1_Q.default,
                p2_freq: Self::PARA2_FREQ_HZ.default,
                p2_gain: Self::PARA2_GAIN_DB.default,
                p2_q: Self::PARA2_Q.default,
                hs_freq: Self::HIGH_SHELF_FREQ_HZ.default,
                hs_gain: Self::HIGH_SHELF_GAIN_DB.default,
            },
            st: [State::default(); 2],
            gate: Gate::default(),
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.gate.enabled = enabled;
    }
    pub fn enabled(&self) -> bool {
        self.gate.enabled
    }
    pub fn set_low_shelf_gain_db(&mut self, v: f32) {
        self.ctl.ls_gain = v;
    }
    pub fn set_low_shelf_freq_hz(&mut self, v: f32) {
        self.ctl.ls_freq = v;
    }
    pub fn set_para1_gain_db(&mut self, v: f32) {
        self.ctl.p1_gain = v;
    }
    pub fn set_para1_freq_hz(&mut self, v: f32) {
        self.ctl.p1_freq = v;
    }
    pub fn set_para1_q(&mut self, v: f32) {
        self.ctl.p1_q = v;
    }
    pub fn set_para2_gain_db(&mut self, v: f32) {
        self.ctl.p2_gain = v;
    }
    pub fn set_para2_freq_hz(&mut self, v: f32) {
        self.ctl.p2_freq = v;
    }
    pub fn set_para2_q(&mut self, v: f32) {
        self.ctl.p2_q = v;
    }
    pub fn set_high_shelf_gain_db(&mut self, v: f32) {
        self.ctl.hs_gain = v;
    }
    pub fn set_high_shelf_freq_hz(&mut self, v: f32) {
        self.ctl.hs_freq = v;
    }

    /// Faust `instanceClear()` on both instances: filters and smoothers to 0, keeps parameters.
    pub fn reset(&mut self) {
        self.st = [State::default(); 2];
    }

    /// `None` = mono group: only the first instance runs (the second keeps its state).
    pub fn process(&mut self, left: &mut [f32], right: Option<&mut [f32]>) {
        if !self.gate.run() {
            return;
        }
        let c = &self.ctl;
        let k = Slow {
            s0: 0.001 * c.ls_freq,
            s1: 0.001 * c.ls_gain,
            s2: 0.001 * c.p1_freq,
            s3: 0.001 * c.p1_gain,
            s4: self.c1 / c.p1_q,
            s5: 0.001 * c.p2_freq,
            s6: 0.001 * c.p2_gain,
            s7: 0.001 * c.p2_q,
            s8: 0.001 * c.hs_freq,
            s9: 0.001 * c.hs_gain,
        };
        let (c1, c2) = (self.c1, self.c2);
        let [s_l, s_r] = &mut self.st;
        for x in left.iter_mut() {
            *x = tick(s_l, &k, c1, c2, *x);
        }
        if let Some(right) = right {
            for x in right.iter_mut() {
                *x = tick(s_r, &k, c1, c2, *x);
            }
        }
    }
}

#[inline(always)]
fn pow10(x: f32) -> f32 {
    10.0f32.powf(x)
}

#[inline(always)]
fn tick(s: &mut State, k: &Slow, c1: f32, c2: f32, x: f32) -> f32 {
    let t0 = x;
    s.vec0[0] = t0;
    // low shelf
    s.rec6[0] = k.s0 + 0.999 * s.rec6[1];
    let t1 = (c1 * s.rec6[0]).tan();
    let t2 = 1.0 / t1;
    let t3 = t2 + 1.0;
    let t4 = 1.0 - t2;
    s.rec5[0] = (s.vec0[1] * (0.0 - (1.0 / (t1 * t3)))) - (((s.rec5[1] * t4) - (t0 / t1)) / t3);
    let t5 = ((t2 + -1.0) / t1) + 1.0;
    let t6 = t1 * t1;
    let t7 = 1.0 - (1.0 / t6);
    let t8 = ((t2 + 1.0) / t1) + 1.0;
    s.rec4[0] = s.rec5[0] - (((s.rec4[2] * t5) + (2.0 * (s.rec4[1] * t7))) / t8);
    s.rec8[0] = ((t0 + s.vec0[1]) - (t4 * s.rec8[1])) / t3;
    s.rec7[0] = s.rec8[0] - (((t5 * s.rec7[2]) + (2.0 * (t7 * s.rec7[1]))) / t8);
    s.rec9[0] = k.s1 + 0.999 * s.rec9[1];
    // para1
    s.rec10[0] = k.s2 + 0.999 * s.rec10[1];
    let t9 = (c1 * s.rec10[0]).tan();
    let t10 = 1.0 / t9;
    s.rec11[0] = k.s3 + 0.999 * s.rec11[1];
    let i11 = s.rec11[0] > 0.0;
    let t12 = (c2 * s.rec10[0]).sin();
    let t13 = k.s4 * ((s.rec10[0] * pow10(0.05 * s.rec11[0].abs())) / t12);
    let t14 = k.s4 * (s.rec10[0] / t12);
    let t15 = if i11 { t14 } else { t13 };
    let t16 = 2.0 * (s.rec3[1] * (1.0 - (1.0 / (t9 * t9))));
    let t17 = ((t10 + t15) / t9) + 1.0;
    s.rec3[0] = (((((s.rec4[1] * (0.0 - (2.0 / t6))) + (s.rec4[0] / t6)) + (s.rec4[2] / t6))
        + ((s.rec7[2] + (s.rec7[0] + (2.0 * s.rec7[1]))) * pow10(0.05 * s.rec9[0])))
        / t8)
        - (((s.rec3[2] * (((t10 - t15) / t9) + 1.0)) + t16) / t17);
    let t18 = if i11 { t13 } else { t14 };
    // para2
    s.rec12[0] = k.s5 + 0.999 * s.rec12[1];
    let t19 = (c1 * s.rec12[0]).tan();
    let t20 = 1.0 / t19;
    s.rec13[0] = k.s6 + 0.999 * s.rec13[1];
    let i21 = s.rec13[0] > 0.0;
    s.rec14[0] = k.s7 + 0.999 * s.rec14[1];
    let t22 = s.rec14[0] * (c2 * s.rec12[0]).sin();
    let t23 = c1 * ((s.rec12[0] * pow10(0.05 * s.rec13[0].abs())) / t22);
    let t24 = c1 * (s.rec12[0] / t22);
    let t25 = if i21 { t24 } else { t23 };
    let t26 = 2.0 * (s.rec2[1] * (1.0 - (1.0 / (t19 * t19))));
    let t27 = ((t20 + t25) / t19) + 1.0;
    s.rec2[0] = (((t16 + (s.rec3[0] * (((t10 + t18) / t9) + 1.0))) + (s.rec3[2] * (((t10 - t18) / t9) + 1.0)))
        / t17)
        - (((s.rec2[2] * (((t20 - t25) / t19) + 1.0)) + t26) / t27);
    let t28 = if i21 { t23 } else { t24 };
    let t29 = (t26 + (s.rec2[0] * (((t20 + t28) / t19) + 1.0))) + (s.rec2[2] * (((t20 - t28) / t19) + 1.0));
    let t30 = t29 / t27;
    s.vec1[0] = t30;
    // high shelf
    s.rec15[0] = k.s8 + 0.999 * s.rec15[1];
    let t31 = (c1 * s.rec15[0]).tan();
    let t32 = 1.0 / t31;
    let t33 = t32 + 1.0;
    let t34 = 1.0 - t32;
    s.rec1[0] = (s.vec1[1] * (0.0 - (1.0 / (t31 * t33)))) + (((t29 / (t31 * t27)) - (s.rec1[1] * t34)) / t33);
    let t35 = ((t32 + -1.0) / t31) + 1.0;
    let t36 = t31 * t31;
    let t37 = 1.0 - (1.0 / t36);
    let t38 = ((t32 + 1.0) / t31) + 1.0;
    s.rec0[0] = s.rec1[0] - (((s.rec0[2] * t35) + (2.0 * (s.rec0[1] * t37))) / t38);
    s.rec16[0] = k.s9 + 0.999 * s.rec16[1];
    s.rec18[0] = 0.0 - (((t34 * s.rec18[1]) - (t30 + s.vec1[1])) / t33);
    s.rec17[0] = s.rec18[0] - (((t35 * s.rec17[2]) + (2.0 * (t37 * s.rec17[1]))) / t38);
    let out = (((((s.rec0[1] * (0.0 - (2.0 / t36))) + (s.rec0[0] / t36)) + (s.rec0[2] / t36)) * pow10(0.05 * s.rec16[0]))
        + (s.rec17[2] + (s.rec17[0] + (2.0 * s.rec17[1]))))
        / t38;
    // shift delay lines
    s.vec0[1] = s.vec0[0];
    s.rec6[1] = s.rec6[0];
    s.rec5[1] = s.rec5[0];
    s.rec4[2] = s.rec4[1];
    s.rec4[1] = s.rec4[0];
    s.rec8[1] = s.rec8[0];
    s.rec7[2] = s.rec7[1];
    s.rec7[1] = s.rec7[0];
    s.rec9[1] = s.rec9[0];
    s.rec10[1] = s.rec10[0];
    s.rec11[1] = s.rec11[0];
    s.rec3[2] = s.rec3[1];
    s.rec3[1] = s.rec3[0];
    s.rec12[1] = s.rec12[0];
    s.rec13[1] = s.rec13[0];
    s.rec14[1] = s.rec14[0];
    s.rec2[2] = s.rec2[1];
    s.rec2[1] = s.rec2[0];
    s.vec1[1] = s.vec1[0];
    s.rec15[1] = s.rec15[0];
    s.rec1[1] = s.rec1[0];
    s.rec0[2] = s.rec0[1];
    s.rec0[1] = s.rec0[0];
    s.rec16[1] = s.rec16[0];
    s.rec18[1] = s.rec18[0];
    s.rec17[2] = s.rec17[1];
    s.rec17[1] = s.rec17[0];
    out
}
