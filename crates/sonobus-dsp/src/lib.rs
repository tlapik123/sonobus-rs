//! DSP blocks of the SonoBus 1.7.2 engine, ported sample-accurately from upstream: the
//! per-channel-group effects (Faust-generated `Source/faust*.h`, mapping in
//! `Source/ChannelGroup.cpp`), the main and input reverbs (JUCE `Reverb`, `MVerb.h`,
//! `zitaRev.h`, mapping in `SonobusPluginProcessor.cpp`), the monitor delay and the
//! metronome click scheduler.
//!
//! Chain order in upstream `ChannelGroup::processBlock`: gain (+polarity) -> [`Expander`]
//! -> [`Compressor`] -> [`ParametricEq`] -> [`Limiter`] -> reverb send. Effects only run for
//! 1- or 2-channel groups; that routing decision belongs to the engine.
//!
//! Every effect takes `(left, Option<right>)`. `None` reproduces upstream's mono path:
//! the 2-in/2-out Faust dynamics get a silent second channel, the EQ runs only its
//! first instance.

mod compressor;
mod eq;
mod expander;
mod freeverb;
pub mod gain;
mod metronome;
mod monitor_delay;
mod mverb;
mod reverb;
mod zita;

pub use compressor::{Compressor, Limiter, auto_makeup_gain_db, reduction_meter_level};
pub use eq::ParametricEq;
pub use expander::Expander;
pub use freeverb::Freeverb;
pub use metronome::Metronome;
pub use monitor_delay::{MAX_DELAY_SAMPLES, MonitorDelay};
pub use mverb::{MVerb, MVerbParam};
pub use reverb::{InputReverb, MainReverb, ReverbModel, ReverbParams};
pub use zita::Zita;

/// UI range of a parameter as upstream's JUCE slider defines it (`setRange(min, max, step)`,
/// `setSkewFactor(skew)`), plus the upstream default value.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ParamSpec {
    pub min: f32,
    pub max: f32,
    pub step: f32,
    pub skew: f32,
    pub default: f32,
}

/// Engine-side ratio clamp (`jlimit(1, 120, ratio)` in SonobusPluginProcessor.cpp).
pub const RATIO_ENGINE_MAX: f32 = 120.0;

/// `std::max<float>` semantics (`a < b ? b : a`), kept for NaN parity with the C++.
#[inline(always)]
fn cmax(a: f32, b: f32) -> f32 {
    if a < b { b } else { a }
}

/// `std::min<float>` semantics (`b < a ? b : a`).
#[inline(always)]
fn cmin(a: f32, b: f32) -> f32 {
    if b < a { b } else { a }
}

/// Faust `1 / min(192000, max(1, SR))`.
fn inv_sample_rate(sample_rate: u32) -> f32 {
    1.0 / cmin(192000.0, cmax(1.0, sample_rate as f32))
}

/// Upstream converts ms to s in double, then the Faust zone stores a float.
fn ms_to_s(ms: f32) -> f32 {
    (f64::from(ms) * 1e-3) as f32
}

/// One-pole attack/release coefficient as Faust `an.amp_follower_ar` computes it.
fn ar_coef(fconst0: f32, time_s: f32) -> f32 {
    let t = cmax(fconst0, time_s);
    if t.abs() < 1.192_092_9e-7 {
        0.0
    } else {
        (0.0 - fconst0 / t).exp()
    }
}

/// Upstream's enable handling: `if (_lastEnabled || enabled) compute(); _lastEnabled = enabled;`
/// so one more block is processed after disabling. There is no crossfade and the DSP
/// state is not cleared, so re-enabling resumes from the stale state.
#[derive(Debug, Clone, Copy, Default)]
struct Gate {
    enabled: bool,
    last: bool,
}

impl Gate {
    fn run(&mut self) -> bool {
        let run = self.enabled || self.last;
        self.last = self.enabled;
        run
    }
}
