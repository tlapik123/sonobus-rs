//! Port of JUCE `juce::Reverb` (`juce_audio_basics/utilities/juce_Reverb.h`, JUCE 7.0.8 as
//! bundled with SonoBus 1.7.2), restricted to what upstream uses: stereo processing,
//! width 1, freeze off, dry level 0 (`setMainReverbDryLevel` is never called), so the dry
//! and cross-channel wet terms are always zero and are left out.
//!
//! `JUCE_UNDENORMALISE` (`x += 0.1; x -= 0.1`) is reproduced as on x86, where upstream
//! builds enable it; on ARM JUCE compiles it out, so ARM upstream output differs slightly.

use crate::gain::approximately_equal;

const COMB_TUNINGS: [i32; 8] = [1116, 1188, 1277, 1356, 1422, 1491, 1557, 1617];
const ALLPASS_TUNINGS: [i32; 4] = [556, 441, 341, 225];
const STEREO_SPREAD: i32 = 23;

#[inline(always)]
fn undenormalise(x: f32) -> f32 {
    (x + 0.1) - 0.1
}

/// JUCE `SmoothedValue<float, Linear>`.
#[derive(Debug, Clone, Copy, Default)]
struct Smoothed {
    current: f32,
    target: f32,
    step: f32,
    countdown: i32,
    steps_to_target: i32,
}

impl Smoothed {
    fn reset(&mut self, sample_rate: f64, ramp_s: f64) {
        self.steps_to_target = (ramp_s * sample_rate).floor() as i32;
        self.current = self.target;
        self.countdown = 0;
    }
    fn set_target(&mut self, v: f32) {
        if approximately_equal(v, self.target) {
            return;
        }
        if self.steps_to_target <= 0 {
            (self.current, self.target, self.countdown) = (v, v, 0);
            return;
        }
        self.target = v;
        self.countdown = self.steps_to_target;
        self.step = (self.target - self.current) / self.countdown as f32;
    }
    fn next(&mut self) -> f32 {
        if self.countdown <= 0 {
            return self.target;
        }
        self.countdown -= 1;
        self.current = if self.countdown > 0 { self.current + self.step } else { self.target };
        self.current
    }
}

#[derive(Debug, Clone)]
struct Comb {
    buf: Box<[f32]>,
    idx: usize,
    last: f32,
}

impl Comb {
    fn new(size: i32) -> Self {
        Self { buf: vec![0.0; size as usize].into_boxed_slice(), idx: 0, last: 0.0 }
    }
    /// JUCE `clear()` keeps the index.
    fn clear(&mut self) {
        self.last = 0.0;
        self.buf.fill(0.0);
    }
    #[inline(always)]
    fn process(&mut self, input: f32, damp: f32, feedback: f32) -> f32 {
        let out = self.buf[self.idx];
        self.last = undenormalise(out * (1.0 - damp) + self.last * damp);
        self.buf[self.idx] = undenormalise(input + self.last * feedback);
        self.idx = (self.idx + 1) % self.buf.len();
        out
    }
}

#[derive(Debug, Clone)]
struct AllPass {
    buf: Box<[f32]>,
    idx: usize,
}

impl AllPass {
    fn new(size: i32) -> Self {
        Self { buf: vec![0.0; size as usize].into_boxed_slice(), idx: 0 }
    }
    #[inline(always)]
    fn process(&mut self, input: f32) -> f32 {
        let buffered = self.buf[self.idx];
        self.buf[self.idx] = undenormalise(input + buffered * 0.5);
        self.idx = (self.idx + 1) % self.buf.len();
        buffered - input
    }
}

/// Freeverb-style stereo reverb (8 parallel combs + 4 series allpasses per channel),
/// wet-only output. Parameter changes ramp linearly over 10 ms.
#[derive(Debug, Clone)]
pub struct Freeverb {
    comb: [[Comb; 8]; 2],
    allpass: [[AllPass; 4]; 2],
    damping: Smoothed,
    feedback: Smoothed,
    wet: Smoothed,
}

impl Freeverb {
    /// Equivalent to upstream's `Reverb()` + `setParameters` + `setSampleRate(sr)`:
    /// buffers cleared and all parameters already at their targets.
    pub fn new(sample_rate: u32, room_size: f32, damping: f32, wet_level: f32) -> Self {
        let sr = sample_rate as i32;
        let comb = |ch: i32| std::array::from_fn(|i| Comb::new(sr * (COMB_TUNINGS[i] + ch * STEREO_SPREAD) / 44100));
        let ap = |ch: i32| std::array::from_fn(|i| AllPass::new(sr * (ALLPASS_TUNINGS[i] + ch * STEREO_SPREAD) / 44100));
        let mut r = Self {
            comb: [comb(0), comb(1)],
            allpass: [ap(0), ap(1)],
            damping: Smoothed::default(),
            feedback: Smoothed::default(),
            wet: Smoothed::default(),
        };
        r.set_parameters(room_size, damping, wet_level);
        for s in [&mut r.damping, &mut r.feedback, &mut r.wet] {
            s.reset(f64::from(sample_rate), 0.01);
        }
        r
    }

    /// JUCE `setParameters` with width 1, dry 0, freeze off. Takes effect with a 10 ms ramp.
    pub fn set_parameters(&mut self, room_size: f32, damping: f32, wet_level: f32) {
        let wet = wet_level * 3.0;
        self.wet.set_target(0.5 * wet * (1.0 + 1.0));
        self.damping.set_target(damping * 0.4);
        self.feedback.set_target(room_size * 0.28 + 0.7);
    }

    /// JUCE `reset()`: clears the delay lines (not the read positions, not the ramps).
    pub fn reset(&mut self) {
        self.comb.iter_mut().flatten().for_each(Comb::clear);
        self.allpass.iter_mut().flatten().for_each(|a| a.buf.fill(0.0));
    }

    /// JUCE `processStereo`, in place, wet only.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let [c0, c1] = &mut self.comb;
        let [a0, a1] = &mut self.allpass;
        for (l, r) in left.iter_mut().zip(right.iter_mut()) {
            let input = (*l + *r) * 0.015;
            let damp = self.damping.next();
            let fb = self.feedback.next();
            let (mut out_l, mut out_r) = (0.0f32, 0.0f32);
            for (cl, cr) in c0.iter_mut().zip(c1.iter_mut()) {
                out_l += cl.process(input, damp, fb);
                out_r += cr.process(input, damp, fb);
            }
            for (al, ar) in a0.iter_mut().zip(a1.iter_mut()) {
                out_l = al.process(out_l);
                out_r = ar.process(out_r);
            }
            let wet1 = self.wet.next();
            *l = out_l * wet1;
            *r = out_r * wet1;
        }
    }
}
