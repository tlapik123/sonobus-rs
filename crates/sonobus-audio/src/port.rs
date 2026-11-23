//! One engine input: a lock-free ring fed by any producer ([`ExternalInput`]) at its own sample
//! rate and clock, drained by the engine side ([`InputPort`]) through a resampler whose ratio a
//! drift controller trims so the ring neither runs dry nor grows.
//!
//! Drift control: at every engine block the port measures the ring's *slack* (frames available
//! beyond what the resampler needs for this block). Per 0.5 s window it takes the mean and the
//! minimum slack; the largest (mean - min) seen is the jitter of the sawtooth produced by
//! callback sizes and scheduling. A PI controller steers the mean to that spread plus a small
//! safety margin by trimming the resampler's ratio by at most [`MAX_DEVIATION`]. The ratio is ramped across a block, so the correction is a
//! sub-cent, click-free pitch change. Same-clock input and output converge to ratio 1.0.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::Relaxed};

use cpal::{FromSample, Sample};
use rtrb::{Consumer, Producer, RingBuffer};
use rubato::audioadapter_buffers::direct::{InterleavedSlice, SequentialSlice};
use rubato::{
    Adjustable, Async, FixedAsync, PolynomialDegree, Resampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};

/// Largest ratio trim the drift controller applies (0.2 % = 3.5 cents). Real crystal drift is
/// well under 0.05 %; the headroom lets the controller absorb start-up offsets without dropping.
pub const MAX_DEVIATION: f64 = 0.002;
/// Measurement window of the drift controller.
const WINDOW_SECS: f64 = 0.5;
/// Proportional time constant: a constant slack error is corrected in about this long.
const TP_SECS: f64 = 4.0;
/// Integral time constant.
const TI_SECS: f64 = 40.0;
/// Sinc length used when the input rate differs from the engine rate (delay = half of it).
const SINC_LEN: usize = 64;

/// Counters shared by the producer, the engine-side consumer and observers.
#[derive(Default)]
struct Shared {
    pushes: AtomicU64,
    overflow_frames: AtomicU64,
    max_push: AtomicUsize,
    underflows: AtomicU64,
    skipped_frames: AtomicU64,
    fill: AtomicUsize,
    min_slack: AtomicUsize,
    ratio_bits: AtomicU64,
    running: AtomicBool,
}

/// Snapshot of one input's health, for meters and diagnostics.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct InputStats {
    /// Number of `push` calls so far (a stalled producer stops advancing this).
    pub pushes: u64,
    /// Frames the producer had to drop because the ring was full.
    pub overflow_frames: u64,
    /// Engine blocks that found too little input and were fed silence.
    pub underflows: u64,
    /// Frames the engine side discarded to pull latency back after a stall.
    pub skipped_frames: u64,
    /// Input frames buffered at the last engine block.
    pub fill_frames: usize,
    /// Minimum slack (frames beyond the block's need) in the last control window.
    pub min_slack_frames: usize,
    /// Current relative resampling ratio applied for drift (1.0 = none).
    pub drift_ratio: f64,
    /// False while (re)priming after start or an underflow; the engine gets silence then.
    pub running: bool,
}

/// Read-only view of an input's [`InputStats`], cheap to clone and usable from any thread.
#[derive(Clone)]
pub struct InputMonitor(Arc<Shared>);

impl InputMonitor {
    pub fn stats(&self) -> InputStats {
        let s = &self.0;
        InputStats {
            pushes: s.pushes.load(Relaxed),
            overflow_frames: s.overflow_frames.load(Relaxed),
            underflows: s.underflows.load(Relaxed),
            skipped_frames: s.skipped_frames.load(Relaxed),
            fill_frames: s.fill.load(Relaxed),
            min_slack_frames: s.min_slack.load(Relaxed),
            drift_ratio: f64::from_bits(s.ratio_bits.load(Relaxed)),
            running: s.running.load(Relaxed),
        }
    }
}

/// Producer side of an engine input: push interleaved audio at the rate declared when the input
/// was created, from whichever single thread owns this handle (a cpal input callback, a JNI
/// `AudioRecord` thread, a test). Wait-free; never allocates or blocks.
pub struct ExternalInput {
    producer: Producer<f32>,
    channels: usize,
    sample_rate: u32,
    shared: Arc<Shared>,
}

impl ExternalInput {
    pub fn channels(&self) -> usize {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn monitor(&self) -> InputMonitor {
        InputMonitor(self.shared.clone())
    }

    /// Pushes the whole frames of `interleaved` (a trailing partial frame is ignored), converting
    /// to f32. Returns the number of frames accepted; frames that do not fit are dropped and
    /// counted in [`InputStats::overflow_frames`].
    pub fn push<T: Sample>(&mut self, interleaved: &[T]) -> usize
    where
        f32: FromSample<T>,
    {
        let ch = self.channels;
        let frames = interleaved.len() / ch;
        self.shared.pushes.fetch_add(1, Relaxed);
        self.shared.max_push.fetch_max(frames, Relaxed);
        let fit = (self.producer.slots() / ch).min(frames);
        if fit < frames {
            self.shared
                .overflow_frames
                .fetch_add((frames - fit) as u64, Relaxed);
        }
        if fit > 0
            && let Ok(chunk) = self.producer.write_chunk_uninit(fit * ch)
        {
            chunk.fill_from_iter(interleaved[..fit * ch].iter().map(|s| s.to_sample::<f32>()));
        }
        fit
    }
}

/// Engine side of an input: yields exactly one engine block per call, at the engine rate.
pub(crate) struct InputPort {
    consumer: Consumer<f32>,
    channels: usize,
    in_rate: f64,
    out_rate: f64,
    block: usize,
    shared: Arc<Shared>,
    resampler: Async<f32>,
    scratch: Vec<f32>,
    priming: bool,
    /// Largest number of input frames one output device callback can consume.
    burst_in: usize,
    margin: usize,
    margin_extra: usize,
    window_pulls: usize,
    pulls: usize,
    window_min: usize,
    window_sum: usize,
    /// Largest (window mean - window min) slack seen: the jitter the mean must stay above.
    spread: f64,
    integ: f64,
    kp: f64,
    ki: f64,
}

impl InputPort {
    /// Creates a ring of about half a second plus the engine side reading it. `margin` is the
    /// safety slack in input frames the controller keeps beyond the observed worst case.
    pub(crate) fn new(
        channels: usize,
        in_rate: u32,
        out_rate: u32,
        block: usize,
        margin: usize,
    ) -> (Self, ExternalInput) {
        assert!(channels > 0 && in_rate > 0 && out_rate > 0 && block > 0);
        let ratio = out_rate as f64 / in_rate as f64;
        let max_rel = 1.0 + 2.0 * MAX_DEVIATION;
        let resampler = if in_rate == out_rate {
            Async::new_poly(
                ratio,
                max_rel,
                PolynomialDegree::Cubic,
                block,
                channels,
                FixedAsync::Output,
            )
        } else {
            let params = SincInterpolationParameters {
                sinc_len: SINC_LEN,
                f_cutoff: None,
                oversampling_factor: 256,
                interpolation: SincInterpolationType::Cubic,
                window: WindowFunction::BlackmanHarris2,
            };
            Async::new_sinc(ratio, max_rel, &params, block, channels, FixedAsync::Output)
        }
        .expect("valid resampler parameters");
        let max_in = resampler.input_frames_max();
        let cap_frames = (in_rate as usize / 2).max(8 * (max_in + block));
        let (producer, consumer) = RingBuffer::new(cap_frames * channels);
        let shared = Arc::new(Shared::default());
        shared.ratio_bits.store(1f64.to_bits(), Relaxed);
        let window_pulls = ((WINDOW_SECS * out_rate as f64) / block as f64).ceil() as usize;
        let kp = 1.0 / (TP_SECS * in_rate as f64);
        let port = InputPort {
            consumer,
            channels,
            in_rate: in_rate as f64,
            out_rate: out_rate as f64,
            block,
            shared: shared.clone(),
            resampler,
            scratch: vec![0.0; max_in * channels],
            priming: true,
            burst_in: 0,
            margin,
            margin_extra: 0,
            window_pulls: window_pulls.max(1),
            pulls: 0,
            window_min: usize::MAX,
            window_sum: 0,
            spread: 0.0,
            integ: 0.0,
            kp,
            ki: kp * WINDOW_SECS / TI_SECS,
        };
        let input = ExternalInput {
            producer,
            channels,
            sample_rate: in_rate,
            shared,
        };
        (port, input)
    }

    pub(crate) fn channels(&self) -> usize {
        self.channels
    }

    /// Filter delay of the resampler in engine frames (2 for same-rate cubic, ~32 for sinc).
    pub(crate) fn resampler_delay(&self) -> usize {
        self.resampler.output_delay()
    }

    /// Tells the port how many engine frames the current device callback will consume at once.
    pub(crate) fn note_burst(&mut self, out_frames: usize) {
        let n = (out_frames as f64 * self.in_rate / self.out_rate).ceil() as usize;
        self.burst_in = self.burst_in.max(n);
    }

    /// Fills `out` (planar: `channels` runs of `block` samples) with the next engine block.
    pub(crate) fn pull(&mut self, out: &mut [f32]) {
        let ch = self.channels;
        let avail = self.consumer.slots() / ch;
        let need = self.resampler.input_frames_next();
        self.shared.fill.store(avail, Relaxed);
        let margin = self.margin + self.margin_extra;
        if self.priming {
            let max_push = self.shared.max_push.load(Relaxed);
            if avail < need + margin + max_push + self.burst_in {
                out.fill(0.0);
                return;
            }
            self.priming = false;
            self.shared.running.store(true, Relaxed);
            self.reset_window();
        }
        if avail < need {
            out.fill(0.0);
            self.shared.underflows.fetch_add(1, Relaxed);
            self.shared.running.store(false, Relaxed);
            // The observed jitter outgrew the margin: keep one more block of headroom.
            let cap = self.consumer.buffer().capacity() / ch / 4;
            self.margin_extra = (self.margin_extra + need).min(cap);
            self.priming = true;
            return;
        }
        let input = &mut self.scratch[..need * ch];
        if self.consumer.pop_entire_slice(input).is_err() {
            out.fill(0.0);
            return;
        }
        let res = match (
            InterleavedSlice::new(&*input, ch, need),
            SequentialSlice::new_mut(&mut out[..], ch, self.block),
        ) {
            (Ok(i), Ok(mut o)) => self.resampler.process_into_buffer(&i, &mut o, None),
            _ => unreachable!("buffers sized at construction"),
        };
        if res.is_err() {
            out.fill(0.0);
        }
        self.window_min = self.window_min.min(avail - need);
        self.window_sum += avail - need;
        self.pulls += 1;
        if self.pulls >= self.window_pulls {
            self.end_window(margin);
        }
    }

    fn reset_window(&mut self) {
        self.pulls = 0;
        self.window_min = usize::MAX;
        self.window_sum = 0;
    }

    fn end_window(&mut self, margin: usize) {
        let min = self.window_min;
        let mean = self.window_sum as f64 / self.pulls as f64;
        self.reset_window();
        self.shared.min_slack.store(min, Relaxed);
        // Steer the smooth window mean, not the minimum: the minimum depends on how the input
        // and output callback patterns happen to line up, which beats slowly with drift.
        let max_push = self.shared.max_push.load(Relaxed);
        let hard = (0.02 * self.in_rate).max(2.0 * (max_push + self.burst_in) as f64);
        if mean - (margin as f64 + self.spread) > hard {
            // Far too much buffered (output stalled, or input burst after a pause): drop it
            // instead of pitching the input for many seconds. The window straddles the jump,
            // so it says nothing about jitter and must not feed `spread`.
            let skip = (mean - (margin as f64 + self.spread)) as usize;
            if let Ok(chunk) = self.consumer.read_chunk(skip * self.channels) {
                chunk.commit_all();
                self.shared.skipped_frames.fetch_add(skip as u64, Relaxed);
            }
            return;
        }
        // ponytail: spread only ever grows (one late callback raises latency until reopen);
        // add a slow decay if that shows up in practice.
        self.spread = self.spread.max(mean - min as f64);
        let e = mean - (margin as f64 + self.spread);
        // Integrate only while the output is not saturated, so a large start-up error does not
        // wind the integrator up and overshoot into an underflow afterwards.
        let p = self.kp * e;
        if (p + self.integ).abs() < MAX_DEVIATION {
            self.integ += self.ki * e;
        }
        let dev = (p + self.integ).clamp(-MAX_DEVIATION, MAX_DEVIATION);
        let rel = 1.0 - dev;
        if self
            .resampler
            .set_resample_ratio_relative(rel, true)
            .is_ok()
        {
            self.shared.ratio_bits.store(rel.to_bits(), Relaxed);
        }
    }
}
