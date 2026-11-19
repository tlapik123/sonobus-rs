//! NTP time tags, the stream timer and the time DLL, ported operation for operation so the
//! samplerate fields on the wire match the C++ (`time.hpp`, `common.cpp:964-1083`,
//! `time_dll.hpp`). SonoBus builds with `AOO_TIMEFILTER_CHECK=0`, so the timer never reports
//! an error.

use std::time::{SystemTime, UNIX_EPOCH};

/// OSC/NTP time tag: high 32 bits seconds since 1900, low 32 bits fraction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Hash)]
pub struct TimeTag(pub u64);

const NTP_UNIX_OFFSET: u64 = 2_208_988_800;
const FRAC: f64 = 4_294_967_296.0;

impl TimeTag {
    /// Current system time (`time_tag::now`, `time.cpp:58-87`).
    pub fn now() -> Self {
        let d = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default();
        let high = (d.as_secs() + NTP_UNIX_OFFSET) as u32;
        let low = (d.subsec_nanos() as f64 * 4.294_967_296) as u32;
        Self((high as u64) << 32 | low as u64)
    }

    /// `time_tag(double)`.
    pub fn from_secs(s: f64) -> Self {
        let high = s as u64 as u32;
        let low = ((s - high as f64) * FRAC) as u32;
        Self((high as u64) << 32 | low as u64)
    }

    pub fn to_secs(self) -> f64 {
        (self.0 >> 32) as f64 + (self.0 as u32) as f64 / FRAC
    }

    /// Seconds from `t1` to `t2`, negative if `t2` is earlier (`time_tag::duration`).
    pub fn duration(t1: TimeTag, t2: TimeTag) -> f64 {
        if t2 >= t1 {
            TimeTag(t2.0.wrapping_sub(t1.0)).to_secs()
        } else {
            -TimeTag(t1.0.wrapping_sub(t2.0)).to_secs()
        }
    }
}

/// Stream clock driven by audio callback times (`timer`, without the disabled xrun check).
#[derive(Debug, Default, Clone)]
pub(crate) struct Timer {
    last: TimeTag,
    elapsed: f64,
}

impl Timer {
    pub fn reset(&mut self) {
        *self = Self::default();
    }

    /// Returns `true` on the first call after a reset (`timer::state::reset`).
    pub fn update(&mut self, t: TimeTag) -> bool {
        if self.last.0 == 0 {
            self.last = t;
            return true;
        }
        self.elapsed += TimeTag::duration(self.last, t);
        self.last = t;
        false
    }

    pub fn elapsed(&self) -> f64 {
        self.elapsed
    }

    pub fn absolute(&self) -> TimeTag {
        self.last
    }
}

/// Delay-locked loop over callback times (Fons Adriaensen, "Using a DLL to filter time").
/// [`Self::setup`] with the nominal rate, period and the first callback time (s), then
/// [`Self::update`] with every later callback time; [`Self::samplerate`] is the measured rate.
#[derive(Debug, Default, Clone)]
pub struct Dll {
    b: f64,
    c: f64,
    t0: f64,
    t1: f64,
    e2: f64,
    nper: i32,
}

impl Dll {
    pub fn setup(&mut self, sr: f64, nper: i32, bandwidth: f64, t: f64) {
        let tper = nper as f64 / sr;
        self.nper = nper;
        let omega = 2.0 * std::f64::consts::PI * bandwidth * (nper as f64 / sr);
        #[allow(clippy::approx_constant)] // the C++ constant, not exact sqrt(2)
        let sqrt2 = 1.4142135623731;
        self.b = omega * sqrt2;
        self.c = omega * omega;
        self.e2 = tper;
        self.t0 = t;
        self.t1 = t + tper;
    }

    pub fn update(&mut self, t: f64) {
        let e = t - self.t1;
        self.t0 = self.t1;
        self.t1 += self.b * e + self.e2;
        self.e2 += self.c * e;
        // the C++ "denormal flush" also zeroes negative values; kept for identical output
        if self.e2 <= f64::MIN_POSITIVE {
            self.e2 = 0.0;
        }
    }

    pub fn samplerate(&self) -> f64 {
        self.nper as f64 / (self.t1 - self.t0)
    }
}

/// Default DLL bandwidth (`aoo.h:58`). The C++ keeps it in a `float`.
pub const TIMEFILTER_BANDWIDTH: f32 = 0.008;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timetag_conversions() {
        let t = TimeTag::from_secs(1.5);
        assert_eq!(t.0, (1u64 << 32) | 0x8000_0000);
        assert_eq!(t.to_secs(), 1.5);
        let a = TimeTag::from_secs(100.25);
        let b = TimeTag::from_secs(99.75);
        assert_eq!(TimeTag::duration(b, a), 0.5);
        assert_eq!(TimeTag::duration(a, b), -0.5);
        let now = TimeTag::now();
        // after 2020 and before 2100 in NTP seconds
        assert!((3_786_825_600..6_311_433_600).contains(&(now.0 >> 32)));
    }

    #[test]
    fn timer_accumulates() {
        let mut t = Timer::default();
        assert!(t.update(TimeTag::from_secs(10.0)));
        assert!(!t.update(TimeTag::from_secs(10.5)));
        assert!(!t.update(TimeTag::from_secs(11.0)));
        assert_eq!(t.elapsed(), 1.0);
        assert_eq!(t.absolute(), TimeTag::from_secs(11.0));
        t.reset();
        assert!(t.update(TimeTag::from_secs(1.0)));
    }

    /// Callbacks with jitter around a clock 0.1 % fast: the estimate converges to the true rate.
    #[test]
    fn dll_converges() {
        let (sr, n) = (48000.0, 256);
        let true_sr = sr * 1.001;
        let mut dll = Dll::default();
        dll.setup(sr, n, TIMEFILTER_BANDWIDTH as f64, 0.0);
        let mut x: u32 = 1;
        for k in 1..20_000 {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            let jitter = (x as f64 / u32::MAX as f64 - 0.5) * 0.002; // +/- 1 ms
            dll.update(k as f64 * n as f64 / true_sr + jitter);
        }
        let est = dll.samplerate();
        assert!((est - true_sr).abs() < 0.5, "{est} vs {true_sr}");
    }
}
