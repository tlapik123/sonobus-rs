//! Port of upstream `Source/MVerb.h` (MVerb by Martin Eastwood, GPLv3), `MVerb<float>`.
//!
//! The C++ mixes float and double arithmetic (double literals promote whole
//! sub-expressions); the casts below reproduce that so the output matches bit for bit.
//! Parameter smoothing is per `process` call: each control ramps linearly from its last
//! value to its target over the call, so results depend on the block size.
//!
//! Upstream behaviour kept: `set_parameter(SIZE, ..)` clears the tank delay lines (audible
//! dropout when the size changes); `reset()` keeps the smoothed controls and the tank
//! feedback state; the first block after construction ramps every control up from 0.

const MAX_LEN: i32 = 96000;

/// One MVerb delay buffer with `T` read taps (`Allpass`, `StaticAllpassFourTap`,
/// `StaticDelayLine*` share this layout: tap 0 is the read/write position).
#[derive(Debug, Clone)]
struct Line<const T: usize> {
    buf: Box<[f32]>,
    idx: [i32; T],
    len: i32,
}

impl<const T: usize> Line<T> {
    fn new() -> Self {
        Self { buf: vec![0.0; MAX_LEN as usize].into_boxed_slice(), idx: [0; T], len: MAX_LEN - 1 }
    }
    fn set_len(&mut self, len: i32) {
        self.len = len.clamp(0, MAX_LEN);
    }
    fn clear(&mut self) {
        self.buf.fill(0.0);
        self.idx = [0; T];
    }
    #[inline(always)]
    fn tap(&self, n: usize) -> f32 {
        self.buf[self.idx[n] as usize]
    }
    #[inline(always)]
    fn advance(&mut self) {
        for i in &mut self.idx {
            *i += 1;
            if *i >= self.len {
                *i = 0;
            }
        }
    }
    #[inline(always)]
    fn delay(&mut self, input: f32) -> f32 {
        let out = self.tap(0);
        self.buf[self.idx[0] as usize] = input;
        self.advance();
        out
    }
    #[inline(always)]
    fn allpass(&mut self, input: f32, feedback: f32) -> f32 {
        let bufout = self.tap(0);
        let temp = input * -feedback;
        self.buf[self.idx[0] as usize] = input + ((bufout + temp) * feedback);
        self.advance();
        bufout + temp
    }
}

/// `StateVariable<float, 4>` in low-pass mode with resonance 0.
#[derive(Debug, Clone, Copy)]
struct Svf {
    sample_rate: f32,
    frequency: f32,
    f: f32,
    low: f32,
    band: f32,
}

impl Svf {
    fn new() -> Self {
        let mut s = Self { sample_rate: 0.0, frequency: 1000.0, f: 0.0, low: 0.0, band: 0.0 };
        s.set_sample_rate(44100.0);
        s
    }
    fn set_sample_rate(&mut self, sr: f32) {
        self.sample_rate = sr * 4.0;
        self.update();
    }
    fn set_frequency(&mut self, freq: f32) {
        self.frequency = freq;
        self.update();
    }
    #[allow(clippy::approx_constant)] // upstream's literal, not exactly PI
    fn update(&mut self) {
        let x = (3.141592654 * f64::from(self.frequency) / f64::from(self.sample_rate)) as f32;
        self.f = (2.0 * f64::from(x.sin())) as f32;
    }
    fn reset(&mut self) {
        self.low = 0.0;
        self.band = 0.0;
    }
    #[inline(always)]
    fn process(&mut self, input: f32) -> f32 {
        for _ in 0..4 {
            self.low = (f64::from(self.low) + (f64::from(self.f * self.band) + 1e-25)) as f32;
            let high = input - self.low - 2.0 * self.band; // q = 2 - 2 * resonance
            self.band += self.f * high;
        }
        self.low
    }
}

/// `MVerb::setParameter` indices.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MVerbParam {
    DampingFreq,
    Density,
    BandwidthFreq,
    Decay,
    PreDelay,
    Size,
    Gain,
    Mix,
    EarlyMix,
}

/// Plate-style stereo reverb (Dattorro tank with early reflections).
#[derive(Debug, Clone)]
pub struct MVerb {
    allpass: [Line<1>; 4],
    allpass_feedback: [f32; 4],
    allpass_four_tap: [Line<4>; 4],
    four_tap_feedback: [f32; 4],
    bandwidth_filter: [Svf; 2],
    damping: [Svf; 2],
    predelay: Line<1>,
    static_delay: [Line<4>; 4],
    early_reflections: [Line<8>; 2],
    sample_rate: f32,
    damping_freq: f32,
    density1: f32,
    bandwidth_freq: f32,
    predelay_time: f32,
    decay: f32,
    gain: f32,
    mix: f32,
    early_mix: f32,
    size: f32,
    mix_smooth: f32,
    early_late_smooth: f32,
    bandwidth_smooth: f32,
    damping_smooth: f32,
    predelay_smooth: f32,
    decay_smooth: f32,
    previous_left_tank: f32,
    previous_right_tank: f32,
    control_rate: i32,
    control_rate_counter: i32,
}

impl MVerb {
    /// `MVerb()` followed by `setSampleRate(sample_rate)`.
    pub fn new(sample_rate: u32) -> Self {
        let mut m = Self {
            allpass: std::array::from_fn(|_| Line::new()),
            allpass_feedback: [0.5; 4],
            allpass_four_tap: std::array::from_fn(|_| Line::new()),
            four_tap_feedback: [0.5; 4],
            bandwidth_filter: [Svf::new(); 2],
            damping: [Svf::new(); 2],
            predelay: Line::new(),
            static_delay: std::array::from_fn(|_| Line::new()),
            early_reflections: std::array::from_fn(|_| Line::new()),
            sample_rate: 44100.0,
            damping_freq: 0.9,
            density1: 0.0, // uninitialised upstream; always set before use
            bandwidth_freq: 0.9,
            predelay_time: 100.0 * (44100.0 / 1000.0),
            decay: 0.5,
            gain: 1.0,
            mix: 1.0,
            early_mix: 1.0,
            size: 1.0,
            mix_smooth: 0.0,
            early_late_smooth: 0.0,
            bandwidth_smooth: 0.0,
            damping_smooth: 0.0,
            predelay_smooth: 0.0,
            decay_smooth: 0.0,
            previous_left_tank: 0.0,
            previous_right_tank: 0.0,
            control_rate: 0,
            control_rate_counter: 0,
        };
        m.sample_rate = sample_rate as f32;
        m.control_rate = (m.sample_rate / 1000.0) as i32;
        m.reset();
        m
    }

    fn sr(&self, k: f64) -> i32 {
        (k * f64::from(self.sample_rate)) as i32
    }
    fn srs(&self, k: f64) -> i32 {
        (k * f64::from(self.sample_rate) * f64::from(self.size)) as i32
    }

    fn set_tank_geometry(&mut self) {
        for l in &mut self.allpass_four_tap {
            l.clear();
        }
        for (i, k) in [0.020, 0.060, 0.030, 0.089].into_iter().enumerate() {
            let n = self.srs(k);
            self.allpass_four_tap[i].set_len(n);
        }
        self.allpass_four_tap[1].idx = [0, self.srs(0.006), self.srs(0.041), 0];
        self.allpass_four_tap[3].idx = [0, self.srs(0.031), self.srs(0.011), 0];
        for (i, k) in [0.15, 0.12, 0.14, 0.11].into_iter().enumerate() {
            self.static_delay[i].clear();
            let n = self.srs(k);
            self.static_delay[i].set_len(n);
        }
        let idx = [
            [0, self.srs(0.067), self.srs(0.011), self.srs(0.121)],
            [0, self.srs(0.036), self.srs(0.089), 0],
            [0, self.srs(0.0089), self.srs(0.099), 0],
            [0, self.srs(0.067), self.srs(0.0041), 0],
        ];
        for (l, i) in self.static_delay.iter_mut().zip(idx) {
            l.idx = i;
        }
    }

    /// Clears all delay lines and filter states and re-derives the geometry.
    pub fn reset(&mut self) {
        self.control_rate_counter = 0;
        for f in self.bandwidth_filter.iter_mut().chain(&mut self.damping) {
            f.set_sample_rate(self.sample_rate);
            f.reset();
        }
        self.predelay.clear();
        self.predelay.set_len(self.predelay_time as i32);
        for (i, k) in [0.0048, 0.0036, 0.0127, 0.0093].into_iter().enumerate() {
            self.allpass[i].clear();
            let n = self.sr(k);
            self.allpass[i].set_len(n);
        }
        self.allpass_feedback = [0.75, 0.75, 0.625, 0.625];
        // Upstream clears the four-tap allpasses before setting their lengths and resets
        // taps 0 and 2 to zero; `set_tank_geometry` does the same.
        self.set_tank_geometry();
        self.allpass_four_tap[0].idx = [0; 4];
        self.allpass_four_tap[2].idx = [0; 4];
        self.four_tap_feedback = [self.density1, 0.0, self.density1, 0.0];
        self.early_reflections[0].clear();
        self.early_reflections[1].clear();
        let n = self.sr(0.089);
        self.early_reflections[0].set_len(n);
        self.early_reflections[0].idx = [
            0,
            self.sr(0.0199),
            self.sr(0.0219),
            self.sr(0.0354),
            self.sr(0.0389),
            self.sr(0.0414),
            self.sr(0.0692),
            0,
        ];
        let n = self.sr(0.069);
        self.early_reflections[1].set_len(n);
        self.early_reflections[1].idx = [
            0,
            self.sr(0.0099),
            self.sr(0.011),
            self.sr(0.0182),
            self.sr(0.0189),
            self.sr(0.0213),
            self.sr(0.0431),
            0,
        ];
    }

    pub fn set_parameter(&mut self, p: MVerbParam, value: f32) {
        match p {
            MVerbParam::DampingFreq => self.damping_freq = (1.0 - f64::from(value)) as f32,
            MVerbParam::Density => self.density1 = value,
            MVerbParam::BandwidthFreq => self.bandwidth_freq = value,
            MVerbParam::PreDelay => self.predelay_time = value,
            MVerbParam::Size => {
                self.size = (0.95 * f64::from(value) + 0.05) as f32;
                self.set_tank_geometry();
            }
            MVerbParam::Decay => self.decay = value,
            MVerbParam::Gain => self.gain = value,
            MVerbParam::Mix => self.mix = value,
            MVerbParam::EarlyMix => self.early_mix = value,
        }
    }

    /// Stereo in place.
    pub fn process(&mut self, left: &mut [f32], right: &mut [f32]) {
        let n = left.len().min(right.len());
        if n == 0 {
            return;
        }
        let one_over = (1.0 / n as f64) as f32;
        let one_over_d = f64::from(one_over);
        let mix_delta = (self.mix - self.mix_smooth) * one_over;
        let early_late_delta = (self.early_mix - self.early_late_smooth) * one_over;
        let bandwidth_delta = (((f64::from(self.bandwidth_freq) * 18400.0 + 100.0)
            - f64::from(self.bandwidth_smooth))
            * one_over_d) as f32;
        let damping_delta =
            (((f64::from(self.damping_freq) * 18400.0 + 100.0) - f64::from(self.damping_smooth)) * one_over_d)
                as f32;
        let predelay_delta =
            ((self.predelay_time * 200.0 * (self.sample_rate / 1000.0)) - self.predelay_smooth) * one_over;
        let decay_delta =
            ((f64::from(0.7995f32 * self.decay) + 0.005 - f64::from(self.decay_smooth)) * one_over_d) as f32;

        let d = f64::from;
        for (l, r) in left[..n].iter_mut().zip(&mut right[..n]) {
            let (left_in, right_in) = (*l, *r);
            self.mix_smooth += mix_delta;
            self.early_late_smooth += early_late_delta;
            self.bandwidth_smooth += bandwidth_delta;
            self.damping_smooth += damping_delta;
            self.predelay_smooth += predelay_delta;
            self.decay_smooth += decay_delta;
            if self.control_rate_counter >= self.control_rate {
                self.control_rate_counter = 0;
                for f in &mut self.bandwidth_filter {
                    f.set_frequency(self.bandwidth_smooth);
                }
                for f in &mut self.damping {
                    f.set_frequency(self.damping_smooth);
                }
            }
            self.control_rate_counter += 1;
            self.predelay.set_len(self.predelay_smooth as i32);
            let density2 = ((d(self.decay_smooth) + 0.15) as f32).clamp(0.25, 0.5);
            self.four_tap_feedback = [self.density1, density2, self.density1, density2];

            let bl = self.bandwidth_filter[0].process(left_in);
            let br = self.bandwidth_filter[1].process(right_in);

            // C++ evaluates the delay-line call before the tap reads (left to right).
            let er = &mut self.early_reflections[0];
            let e0 = d(er.delay((d(bl) * 0.5 + d(br) * 0.3) as f32));
            let early_l = (e0
                + d(er.tap(2)) * 0.6
                + d(er.tap(3)) * 0.4
                + d(er.tap(4)) * 0.3
                + d(er.tap(5)) * 0.3
                + d(er.tap(6)) * 0.1
                + d(er.tap(7)) * 0.1
                + (d(bl) * 0.4 + d(br) * 0.2) * 0.5) as f32;
            let er = &mut self.early_reflections[1];
            let e0 = d(er.delay((d(bl) * 0.3 + d(br) * 0.5) as f32));
            let early_r = (e0
                + d(er.tap(2)) * 0.6
                + d(er.tap(3)) * 0.4
                + d(er.tap(4)) * 0.3
                + d(er.tap(5)) * 0.3
                + d(er.tap(6)) * 0.1
                + d(er.tap(7)) * 0.1
                + (d(bl) * 0.2 + d(br) * 0.4) * 0.5) as f32;

            let mut smeared = self.predelay.delay((br + bl) * 0.5);
            for (a, fb) in self.allpass.iter_mut().zip(self.allpass_feedback) {
                smeared = a.allpass(smeared, fb);
            }

            let fb = self.four_tap_feedback;
            let [ap0, ap1, ap2, ap3] = &mut self.allpass_four_tap;
            let [sd0, sd1, sd2, sd3] = &mut self.static_delay;
            let mut left_tank = ap0.allpass(smeared + self.previous_right_tank, fb[0]);
            left_tank = sd0.delay(left_tank);
            left_tank = self.damping[0].process(left_tank);
            left_tank = ap1.allpass(left_tank, fb[1]);
            left_tank = sd1.delay(left_tank);
            let mut right_tank = ap2.allpass(smeared + self.previous_left_tank, fb[2]);
            right_tank = sd2.delay(right_tank);
            right_tank = self.damping[1].process(right_tank);
            right_tank = ap3.allpass(right_tank, fb[3]);
            right_tank = sd3.delay(right_tank);
            self.previous_left_tank = left_tank * self.decay_smooth;
            self.previous_right_tank = right_tank * self.decay_smooth;

            let acc_l = (0.6 * d(sd2.tap(1)) + 0.6 * d(sd2.tap(2)) - 0.6 * d(ap3.tap(1)) + 0.6 * d(sd3.tap(1))
                - 0.6 * d(sd0.tap(1))
                - 0.6 * d(ap1.tap(1))
                - 0.6 * d(sd1.tap(1))) as f32;
            let acc_r = (0.6 * d(sd0.tap(2)) + 0.6 * d(sd0.tap(3)) - 0.6 * d(ap1.tap(2)) + 0.6 * d(sd1.tap(2))
                - 0.6 * d(sd2.tap(3))
                - 0.6 * d(ap3.tap(2))
                - 0.6 * d(sd3.tap(2))) as f32;
            let els = self.early_late_smooth;
            let acc_l = acc_l * els + (1.0 - els) * early_l;
            let acc_r = acc_r * els + (1.0 - els) * early_r;
            let ms = self.mix_smooth;
            *l = ((1.0 - ms) * left_in + ms * (acc_l - left_in)) * self.gain;
            *r = ((1.0 - ms) * right_in + ms * (acc_r - right_in)) * self.gain;
        }
    }
}
