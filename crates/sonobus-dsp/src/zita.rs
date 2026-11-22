//! Port of upstream `Source/zitaRev.h` (Faust-generated `re.zita_rev1_stereo`), transcribed
//! statement by statement so it stays bit-identical; field names keep the Faust numbering
//! (`vecN` = `fVecN`, `recN` = `fRecN`) to make diffs against the C++ traceable.
//!
//! Faust behaviour kept: after `reset()` the level and dry/wet controls restart from 0 and
//! approach their targets through a one-pole (`0.999`) smoother, so the first ~20 ms after
//! a reset let some dry input through even at dry/wet -1 (full wet).

#![allow(clippy::excessive_precision)] // float literals copied verbatim from the C++

use crate::{cmax, cmin};

#[inline(always)]
fn sq(x: f32) -> f32 {
    x * x
}

fn zeros(n: usize) -> Box<[f32]> {
    vec![0.0; n].into_boxed_slice()
}

/// Per-branch slow coefficients (`fSlow11..19`-style groups).
#[derive(Debug, Clone, Copy, Default)]
struct Branch {
    a: f32, // fSlow18 (pole)
    b: f32, // fSlow19 (gain)
    c: f32, // fSlow21 (low-band boost)
}

/// Recursive state (Faust `fRecN`, `IOTA`), zeroed by `instanceClear`.
#[derive(Debug, Clone, Copy, Default)]
struct St {
    iota: i32,
    rec0: [f32; 2],
    rec1: [f32; 2],
    rec12: [f32; 2],
    rec14: [f32; 2],
    rec15: [f32; 2],
    rec16: [f32; 2],
    rec18: [f32; 2],
    rec19: [f32; 2],
    rec20: [f32; 2],
    rec22: [f32; 2],
    rec23: [f32; 2],
    rec24: [f32; 2],
    rec26: [f32; 2],
    rec27: [f32; 2],
    rec28: [f32; 2],
    rec30: [f32; 2],
    rec31: [f32; 2],
    rec32: [f32; 2],
    rec34: [f32; 2],
    rec35: [f32; 2],
    rec36: [f32; 2],
    rec38: [f32; 2],
    rec39: [f32; 2],
    rec40: [f32; 2],
    rec42: [f32; 2],
    rec43: [f32; 2],
    rec2: [f32; 3],
    rec3: [f32; 3],
    rec4: [f32; 3],
    rec5: [f32; 3],
    rec6: [f32; 3],
    rec7: [f32; 3],
    rec8: [f32; 3],
    rec9: [f32; 3],
    rec10: [f32; 3],
    rec11: [f32; 3],
    rec44: [f32; 3],
    rec45: [f32; 3],
}

#[derive(Debug, Clone)]
pub struct Zita {
    // Controls (Faust zones).
    level_db: f32,    // fVslider0
    dry_wet: f32,     // fVslider1
    eq2_freq: f32,    // fVslider2
    eq2_level: f32,   // fVslider3
    eq1_freq: f32,    // fVslider4
    eq1_level: f32,   // fVslider5
    mid_rt60: f32,    // fVslider6
    hf_damping: f32,  // fVslider7
    low_rt60: f32,    // fVslider8
    lf_x: f32,        // fVslider9
    in_delay_ms: f32, // fVslider10
    // Constants.
    c1: f32,
    c4: f32,
    c7: f32,
    cdecay: [f32; 8],    // fConst3, 10, 15, 20, 25, 30, 35, 40
    idelay: [usize; 16], // iConst6, 8, 12, 13, 17, 18, 22, 23, 27, 28, 32, 33, 37, 38, 42, 43
    vec: [Box<[f32]>; 18],
    st: St,
}

const VEC_SIZES: [usize; 18] = [
    16384, 16384, 32768, 4096, 16384, 4096, 16384, 4096, 16384, 2048, 32768, 4096, 16384, 2048,
    32768, 2048, 16384, 4096,
];

impl Zita {
    /// Faust `init(sample_rate)`: constants, controls at their Faust defaults, state cleared.
    pub fn new(sample_rate: u32) -> Self {
        let c0 = cmin(192000.0, cmax(1.0, sample_rate as f32));
        let fl = |k: f32| ((k * c0) + 0.5).floor();
        let decay = |n: f32| (0.0 - (6.90775537 * n)) / c0;
        // (long, short) delay-line pairs for the 8 branches, with the Faust clamps.
        let pairs: [(f32, f32, f32, f32); 8] = [
            (0.256891012, 0.0273330007, 16384.0, 2048.0),
            (0.127837002, 0.0316039994, 8192.0, 2048.0),
            (0.174713001, 0.0229039993, 8192.0, 2048.0),
            (0.153128996, 0.0203460008, 8192.0, 1024.0),
            (0.210389003, 0.0244210009, 16384.0, 2048.0),
            (0.125, 0.0134579996, 8192.0, 1024.0),
            (0.219990999, 0.0191229992, 16384.0, 1024.0),
            (0.192303002, 0.0292910002, 8192.0, 2048.0),
        ];
        let mut cdecay = [0.0; 8];
        let mut idelay = [0; 16];
        for (i, (kl, ks, ml, ms)) in pairs.into_iter().enumerate() {
            let long = fl(kl);
            let short = fl(ks);
            cdecay[i] = decay(long);
            idelay[2 * i] = cmin(ml, cmax(0.0, long - short)) as usize;
            idelay[2 * i + 1] = cmin(ms, cmax(0.0, short + -1.0)) as usize;
        }
        Self {
            level_db: -20.0,
            dry_wet: 0.0,
            eq2_freq: 1500.0,
            eq2_level: 0.0,
            eq1_freq: 315.0,
            eq1_level: 0.0,
            mid_rt60: 2.0,
            hf_damping: 6000.0,
            low_rt60: 3.0,
            lf_x: 200.0,
            in_delay_ms: 60.0,
            c1: 6.28318548 / c0,
            c4: 3.14159274 / c0,
            c7: 0.00100000005 * c0,
            cdecay,
            idelay,
            vec: VEC_SIZES.map(zeros),
            st: St::default(),
        }
    }

    /// Faust `instanceClear()`: clears all state, keeps controls.
    pub fn reset(&mut self) {
        self.st = St::default();
        self.vec.iter_mut().for_each(|v| v.fill(0.0));
    }

    /// `/Zita_Rev1/Output/Level` in dB (Faust range -70..40).
    pub fn set_level_db(&mut self, v: f32) {
        self.level_db = v;
    }
    /// `/Zita_Rev1/Output/Dry/Wet_Mix`: -1 is full wet, 1 full dry (Faust's tooltip has it
    /// backwards).
    pub fn set_dry_wet(&mut self, v: f32) {
        self.dry_wet = v;
    }
    /// `/Zita_Rev1/Input/In_Delay` in ms (Faust range 20..100; not clamped, upstream sends 0..100).
    pub fn set_in_delay_ms(&mut self, v: f32) {
        self.in_delay_ms = v;
    }
    pub fn set_low_rt60(&mut self, v: f32) {
        self.low_rt60 = v;
    }
    pub fn set_mid_rt60(&mut self, v: f32) {
        self.mid_rt60 = v;
    }
    pub fn set_hf_damping(&mut self, v: f32) {
        self.hf_damping = v;
    }

    /// Faust `compute`, stereo in place.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let slow0 = 0.00100000005 * 10.0f32.powf(0.0500000007 * self.level_db);
        let slow1 = 0.00100000005 * self.dry_wet;
        let slow2 = self.eq2_freq;
        let slow3 = 10.0f32.powf(0.0500000007 * self.eq2_level);
        let slow4 = self.c1 * (slow2 / cmax(0.0, slow3).sqrt());
        let slow5 = (1.0 - slow4) / (slow4 + 1.0);
        let slow6 = self.eq1_freq;
        let slow7 = 10.0f32.powf(0.0500000007 * self.eq1_level);
        let slow8 = self.c1 * (slow6 / cmax(0.0, slow7).sqrt());
        let slow9 = (1.0 - slow8) / (slow8 + 1.0);
        let slow10 = self.mid_rt60;
        let slow13 = (self.c1 * self.hf_damping).cos();
        let slow20 = self.low_rt60;
        let slow22 = 1.0 / (self.c4 * self.lf_x).tan();
        let slow23 = 1.0 / (slow22 + 1.0);
        let slow24 = 1.0 - slow22;
        let islow25 = cmin(8192.0, cmax(0.0, self.c7 * self.in_delay_ms)) as i32;
        let br: [Branch; 8] = self.cdecay.map(|cd| {
            let g = (cd / slow10).exp();
            let g2 = sq(g);
            let t14 = 1.0 - (g2 * slow13);
            let t15 = 1.0 - g2;
            let t16 = t14 / t15;
            let t17 = cmax(0.0, (sq(t14) / sq(t15)) + -1.0).sqrt();
            Branch {
                a: t16 - t17,
                b: g * (t17 + (1.0 - t16)),
                c: ((cd / slow20).exp() / g) + -1.0,
            }
        });
        let slow89 = 0.0 - ((self.c1 * slow6).cos() * (slow9 + 1.0));
        let slow90 = 0.0 - ((self.c1 * slow2).cos() * (slow5 + 1.0));
        let ic = self.idelay;
        let [
            v0,
            v1,
            v2,
            v3,
            v4,
            v5,
            v6,
            v7,
            v8,
            v9,
            v10,
            v11,
            v12,
            v13,
            v14,
            v15,
            v16,
            v17,
        ] = &mut self.vec;

        let s = &mut self.st;
        for (o0, o1) in left.iter_mut().zip(right.iter_mut()) {
            let iota = s.iota;
            let at = |mask: i32, d: i32| (iota.wrapping_sub(d) & mask) as usize;
            let w = |mask: i32| (iota & mask) as usize;
            let temp0 = *o0;
            v0[w(16383)] = temp0;
            let temp1 = *o1;
            v1[w(16383)] = temp1;
            s.rec0[0] = slow0 + (0.999000013 * s.rec0[1]);
            s.rec1[0] = slow1 + (0.999000013 * s.rec1[1]);
            let temp2 = s.rec1[0] + 1.0;
            let temp3 = 1.0 - (0.5 * temp2);

            s.rec15[0] = 0.0 - (slow23 * ((slow24 * s.rec15[1]) - (s.rec7[1] + s.rec7[2])));
            s.rec14[0] = (br[0].a * s.rec14[1]) + (br[0].b * (s.rec7[1] + (br[0].c * s.rec15[0])));
            v2[w(32767)] = (0.353553385 * s.rec14[0]) + 9.99999968e-21;
            let temp4 = 0.300000012 * v1[at(16383, islow25)];
            let temp5 = ((0.600000024 * s.rec12[1]) + v2[at(32767, ic[0] as i32)]) - temp4;
            v3[w(4095)] = temp5;
            s.rec12[0] = v3[at(4095, ic[1] as i32)];
            let rec13 = 0.0 - (0.600000024 * temp5);

            s.rec19[0] = 0.0 - (slow23 * ((slow24 * s.rec19[1]) - (s.rec6[1] + s.rec6[2])));
            s.rec18[0] = (br[1].a * s.rec18[1]) + (br[1].b * (s.rec6[1] + (br[1].c * s.rec19[0])));
            v4[w(16383)] = (0.353553385 * s.rec18[0]) + 9.99999968e-21;
            let temp6 = 0.300000012 * v0[at(16383, islow25)];
            let temp7 = v4[at(16383, ic[2] as i32)] - (temp6 + (0.600000024 * s.rec16[1]));
            v5[w(4095)] = temp7;
            s.rec16[0] = v5[at(4095, ic[3] as i32)];
            let rec17 = 0.600000024 * temp7;

            s.rec23[0] = 0.0 - (slow23 * ((slow24 * s.rec23[1]) - (s.rec8[1] + s.rec8[2])));
            s.rec22[0] = (br[2].a * s.rec22[1]) + (br[2].b * (s.rec8[1] + (br[2].c * s.rec23[0])));
            v6[w(16383)] = (0.353553385 * s.rec22[0]) + 9.99999968e-21;
            let temp8 = (temp6 + v6[at(16383, ic[4] as i32)]) - (0.600000024 * s.rec20[1]);
            v7[w(4095)] = temp8;
            s.rec20[0] = v7[at(4095, ic[5] as i32)];
            let rec21 = 0.600000024 * temp8;

            s.rec27[0] = 0.0 - (slow23 * ((slow24 * s.rec27[1]) - (s.rec4[1] + s.rec4[2])));
            s.rec26[0] = (br[3].a * s.rec26[1]) + (br[3].b * (s.rec4[1] + (br[3].c * s.rec27[0])));
            v8[w(16383)] = (0.353553385 * s.rec26[0]) + 9.99999968e-21;
            let temp9 = (v8[at(16383, ic[6] as i32)] + temp6) - (0.600000024 * s.rec24[1]);
            v9[w(2047)] = temp9;
            s.rec24[0] = v9[at(2047, ic[7] as i32)];
            let rec25 = 0.600000024 * temp9;

            s.rec31[0] = 0.0 - (slow23 * ((slow24 * s.rec31[1]) - (s.rec5[1] + s.rec5[2])));
            s.rec30[0] = (br[4].a * s.rec30[1]) + (br[4].b * (s.rec5[1] + (br[4].c * s.rec31[0])));
            v10[w(32767)] = (0.353553385 * s.rec30[0]) + 9.99999968e-21;
            let temp10 = temp4 + ((0.600000024 * s.rec28[1]) + v10[at(32767, ic[8] as i32)]);
            v11[w(4095)] = temp10;
            s.rec28[0] = v11[at(4095, ic[9] as i32)];
            let rec29 = 0.0 - (0.600000024 * temp10);

            s.rec35[0] = 0.0 - (slow23 * ((slow24 * s.rec35[1]) - (s.rec10[1] + s.rec10[2])));
            s.rec34[0] = (br[5].a * s.rec34[1]) + (br[5].b * (s.rec10[1] + (br[5].c * s.rec35[0])));
            v12[w(16383)] = (0.353553385 * s.rec34[0]) + 9.99999968e-21;
            let temp11 = v12[at(16383, ic[10] as i32)] - (temp6 + (0.600000024 * s.rec32[1]));
            v13[w(2047)] = temp11;
            s.rec32[0] = v13[at(2047, ic[11] as i32)];
            let rec33 = 0.600000024 * temp11;

            s.rec39[0] = 0.0 - (slow23 * ((slow24 * s.rec39[1]) - (s.rec11[1] + s.rec11[2])));
            s.rec38[0] = (br[6].a * s.rec38[1]) + (br[6].b * (s.rec11[1] + (br[6].c * s.rec39[0])));
            v14[w(32767)] = (0.353553385 * s.rec38[0]) + 9.99999968e-21;
            let temp12 = ((0.600000024 * s.rec36[1]) + v14[at(32767, ic[12] as i32)]) - temp4;
            v15[w(2047)] = temp12;
            s.rec36[0] = v15[at(2047, ic[13] as i32)];
            let rec37 = 0.0 - (0.600000024 * temp12);

            s.rec43[0] = 0.0 - (slow23 * ((slow24 * s.rec43[1]) - (s.rec9[1] + s.rec9[2])));
            s.rec42[0] = (br[7].a * s.rec42[1]) + (br[7].b * (s.rec9[1] + (br[7].c * s.rec43[0])));
            v16[w(16383)] = (0.353553385 * s.rec42[0]) + 9.99999968e-21;
            let temp13 = v16[at(16383, ic[14] as i32)] + (temp4 + (0.600000024 * s.rec40[1]));
            v17[w(4095)] = temp13;
            s.rec40[0] = v17[at(4095, ic[15] as i32)];
            let rec41 = 0.0 - (0.600000024 * temp13);

            let (r12, r16, r20, r24) = (s.rec12[1], s.rec16[1], s.rec20[1], s.rec24[1]);
            let (r28, r32, r36, r40) = (s.rec28[1], s.rec32[1], s.rec36[1], s.rec40[1]);
            let temp14 = rec41 + rec13;
            let temp15 = rec37 + temp14;
            s.rec4[0] = r12
                + (r16
                    + (r20
                        + (r24
                            + (rec29
                                + (rec33
                                    + (rec17
                                        + (rec21
                                            + (rec25
                                                + (r36 + (r40 + (r28 + (r32 + temp15))))))))))));
            s.rec5[0] = (r16 + (r20 + (r24 + (rec33 + (rec17 + (rec21 + (rec25 + r32)))))))
                - (r12 + (rec29 + (r36 + (r40 + (r28 + temp15)))));
            let temp16 = rec13 + rec37;
            s.rec6[0] = (r20 + (r24 + (rec29 + (rec21 + (rec25 + (r40 + (rec41 + r28)))))))
                - (r12 + (r16 + (rec33 + (rec17 + (r36 + (r32 + temp16))))));
            s.rec7[0] = (r12 + (r20 + (r24 + (rec21 + (rec25 + (r36 + temp16))))))
                - (r16 + (rec29 + (rec33 + (rec17 + (r40 + (r28 + (rec41 + r32)))))));
            let temp17 = rec41 + rec37;
            s.rec8[0] = (r12 + (r16 + (r24 + (rec29 + (rec17 + (rec25 + (rec13 + r28)))))))
                - (r20 + (rec33 + (rec21 + (r36 + (r40 + (r32 + temp17))))));
            s.rec9[0] = (r16 + (r24 + (rec17 + (rec25 + (r36 + (r40 + temp17))))))
                - (r12 + (r20 + (rec29 + (rec33 + (rec21 + (r28 + (rec13 + r32)))))));
            s.rec10[0] = (r24 + (rec29 + (rec33 + (rec25 + (r36 + (r28 + (rec37 + r32)))))))
                - (r12 + (r16 + (r20 + (rec17 + (rec21 + (r40 + temp14))))));
            s.rec11[0] = (r12 + (r24 + (rec33 + (rec25 + (r40 + (r32 + temp14))))))
                - (r16 + (r20 + (rec29 + (rec17 + (rec21 + (r36 + (rec37 + r28)))))));

            let temp18 = 0.370000005 * (s.rec5[0] + s.rec6[0]);
            let temp19 = slow89 * s.rec3[1];
            s.rec3[0] = temp18 - (temp19 + (slow9 * s.rec3[2]));
            let temp20 = slow9 * s.rec3[0];
            let temp21 = 0.5
                * ((temp20 + (s.rec3[2] + (temp18 + temp19)))
                    + (slow7 * ((temp20 + (temp19 + s.rec3[2])) - temp18)));
            let temp22 = slow90 * s.rec2[1];
            s.rec2[0] = temp21 - (temp22 + (slow5 * s.rec2[2]));
            let temp23 = slow5 * s.rec2[0];
            *o0 = 0.5
                * (s.rec0[0]
                    * ((temp0 * temp2)
                        + (temp3
                            * ((temp23 + (s.rec2[2] + (temp21 + temp22)))
                                + (slow3 * ((temp23 + (temp22 + s.rec2[2])) - temp21))))));
            let temp24 = 0.370000005 * (s.rec5[0] - s.rec6[0]);
            let temp25 = slow89 * s.rec45[1];
            s.rec45[0] = temp24 - (temp25 + (slow9 * s.rec45[2]));
            let temp26 = slow9 * s.rec45[0];
            let temp27 = 0.5
                * ((temp26 + (s.rec45[2] + (temp24 + temp25)))
                    + (slow7 * ((temp26 + (temp25 + s.rec45[2])) - temp24)));
            let temp28 = slow90 * s.rec44[1];
            s.rec44[0] = temp27 - (temp28 + (slow5 * s.rec44[2]));
            let temp29 = slow5 * s.rec44[0];
            *o1 = 0.5
                * (s.rec0[0]
                    * ((temp1 * temp2)
                        + (temp3
                            * ((temp29 + (s.rec44[2] + (temp27 + temp28)))
                                + (slow3 * ((temp29 + (temp28 + s.rec44[2])) - temp27))))));

            s.iota = s.iota.wrapping_add(1);
            for r in [
                &mut s.rec0,
                &mut s.rec1,
                &mut s.rec15,
                &mut s.rec14,
                &mut s.rec12,
                &mut s.rec19,
                &mut s.rec18,
                &mut s.rec16,
                &mut s.rec23,
                &mut s.rec22,
                &mut s.rec20,
                &mut s.rec27,
                &mut s.rec26,
                &mut s.rec24,
                &mut s.rec31,
                &mut s.rec30,
                &mut s.rec28,
                &mut s.rec35,
                &mut s.rec34,
                &mut s.rec32,
                &mut s.rec39,
                &mut s.rec38,
                &mut s.rec36,
                &mut s.rec43,
                &mut s.rec42,
                &mut s.rec40,
            ] {
                r[1] = r[0];
            }
            for r in [
                &mut s.rec4,
                &mut s.rec5,
                &mut s.rec6,
                &mut s.rec7,
                &mut s.rec8,
                &mut s.rec9,
                &mut s.rec10,
                &mut s.rec11,
                &mut s.rec3,
                &mut s.rec2,
                &mut s.rec45,
                &mut s.rec44,
            ] {
                r[2] = r[1];
                r[1] = r[0];
            }
        }
    }
}
