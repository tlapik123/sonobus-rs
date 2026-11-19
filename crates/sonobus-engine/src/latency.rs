//! Latency test and group latency match.
//!
//! - [`LatencyMeasurer`]: port of upstream `LatencyMeasurer.{h,cpp}` (pulse `images/lgc_bar.wav`
//!   out, wait until it comes back above a threshold). Runs on the audio thread on each peer's
//!   latency stream (our latency source -> their echo pair -> our latency sink, P.cpp:5762-5818,
//!   8040-8063).
//! - [`LatencyShared`]: the control/audio handshake for one peer's test.
//! - [`match_buffer`], [`receive_sync`]: the buffer arithmetic of `commitLatencyMatch`
//!   (P.cpp:3059-3088) and the Mendeni "Receive Sync" (equal total receive latency + padding).

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use crate::jitter::Cumulant;

/// `LATENCY_ID_OFFSET`, `ECHO_ID_OFFSET` (P.cpp:235-236).
pub(crate) const LATENCY_ID_OFFSET: i32 = 20000;
pub(crate) const ECHO_ID_OFFSET: i32 = 40000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    MeasureAverageLoudness,
    PlayingAndListening,
    Waiting,
    Passthrough,
    Idle,
}

/// Upstream `LatencyMeasurer`, real-time safe after construction.
pub struct LatencyMeasurer {
    /// -1 passthrough, 0 idle, 1.. measurements taken + 1.
    pub state: i32,
    pub latency_ms: f32,
    noise_measure_time: f32,
    max_secs_to_wait: f32,
    min_wait_time: f32,
    override_threshold: f32,
    measurement_count: i32,
    measure_only_once: bool,
    samplerate: i32,
    buffersize: usize,
    smoothed: Cumulant,
    pulse: Box<[f32]>,
    pulse_pos: usize,
    phase: Phase,
    next_phase: Phase,
    sum: f32,
    samples_elapsed: i64,
    threshold: f32,
    skip_measure: bool,
    playing: bool,
}

impl Default for LatencyMeasurer {
    fn default() -> Self {
        Self::new()
    }
}

impl LatencyMeasurer {
    pub fn new() -> Self {
        Self {
            state: 0,
            latency_ms: 0.0,
            noise_measure_time: 0.2,
            max_secs_to_wait: 3.0,
            min_wait_time: 0.6,
            override_threshold: 0.0,
            measurement_count: 10,
            measure_only_once: true,
            samplerate: 0,
            buffersize: 0,
            smoothed: Cumulant::default(),
            pulse: crate::wav::decode_pcm16(include_bytes!("../assets/lgc_bar.wav")).into(),
            pulse_pos: 0,
            phase: Phase::Idle,
            next_phase: Phase::Idle,
            sum: 0.0,
            samples_elapsed: 0,
            threshold: 0.0,
            skip_measure: false,
            playing: false,
        }
    }

    /// `startRemotePeerLatencyTest` (P.cpp:5781-5786): endless measurements, threshold 0.2,
    /// 0.2 s of noise measurement, then `toggle(true)`.
    pub fn start_peer_test(&mut self) {
        self.measurement_count = 10000;
        self.override_threshold = 0.2;
        self.noise_measure_time = 0.2;
        self.toggle(true);
    }

    pub fn toggle(&mut self, force_start: bool) {
        if !force_start && (self.state == -1 || (self.state > 0 && self.state < 11)) {
            self.state = 0;
            self.next_phase = Phase::Idle;
        } else {
            self.state = 1;
            self.samplerate = 0;
            self.latency_ms = 0.0;
            self.buffersize = 0;
            self.skip_measure = self.measure_only_once;
            self.smoothed.reset();
            self.playing = false;
            self.next_phase = Phase::MeasureAverageLoudness;
        }
    }

    fn start_pulse(&mut self, seed: usize) {
        self.phase = Phase::PlayingAndListening;
        self.next_phase = Phase::PlayingAndListening;
        self.samples_elapsed = seed as i64;
        self.sum = 0.0;
        self.pulse_pos = 0;
        self.playing = true;
    }

    pub fn process_input(&mut self, audio: &[f32], samplerate: i32) {
        let n = audio.len();
        self.samplerate = samplerate;
        self.buffersize = n;
        if self.next_phase != self.phase {
            if self.next_phase == Phase::MeasureAverageLoudness {
                self.samples_elapsed = 0;
            }
            self.phase = self.next_phase;
        }
        let sr = samplerate as f32;
        match self.phase {
            Phase::MeasureAverageLoudness => {
                self.sum += audio.iter().map(|x| x.abs()).sum::<f32>();
                self.samples_elapsed += n as i64;
                if self.samples_elapsed as f32 >= self.noise_measure_time * sr {
                    // Upstream inverts this test (`if (overrideThreshold > 0)` computes the
                    // noise-floor threshold, else uses the override, LatencyMeasurer.cpp:
                    // 133-140), so its 0.2 override is ignored and an override of 0 would
                    // trigger on any sample. Fixed: an override > 0 is the threshold.
                    self.threshold = if self.override_threshold > 0.0 {
                        self.override_threshold
                    } else {
                        let avg = (self.sum / (self.samples_elapsed >> 1) as f32).max(0.001);
                        10f32.powf((20.0 * avg.log10() + 24.0) / 20.0)
                    };
                    self.start_pulse(n);
                }
            }
            Phase::PlayingAndListening => {
                let peak = audio.iter().fold(0.0f32, |m, x| m.max(x.abs()));
                if peak > self.threshold {
                    let k = audio
                        .iter()
                        .position(|x| x.abs() > self.threshold)
                        .unwrap_or(n);
                    self.samples_elapsed += k as i64;
                    if self.samples_elapsed > n as i64 {
                        let ms = (self.samples_elapsed * 1000) as f32 / sr;
                        if self.smoothed.z > 1.0 {
                            self.smoothed.z *= 0.75;
                        }
                        self.smoothed.push(ms);
                        self.latency_ms = self.smoothed.mean;
                        let last = self.state == self.measurement_count;
                        self.phase = if last { Phase::Idle } else { Phase::Waiting };
                        self.next_phase = self.phase;
                        self.state += 1;
                    } else {
                        // an early noise
                        self.phase = Phase::Waiting;
                        self.next_phase = Phase::Waiting;
                    }
                } else {
                    self.samples_elapsed += n as i64;
                    if self.samples_elapsed as f32 > sr * self.max_secs_to_wait {
                        self.playing = false;
                        self.phase = Phase::Waiting;
                        self.next_phase = Phase::Waiting;
                        self.latency_ms = -1.0;
                    }
                }
            }
            Phase::Passthrough | Phase::Idle => {}
            Phase::Waiting => {
                self.samples_elapsed += n as i64;
                self.playing = false;
                let wait = f64::from(self.min_wait_time.max(1.5 * self.latency_ms * 0.001));
                if self.samples_elapsed as f64 > f64::from(samplerate) * wait {
                    self.samples_elapsed = 0;
                    if self.skip_measure {
                        self.start_pulse(n);
                    } else {
                        self.phase = Phase::MeasureAverageLoudness;
                        self.next_phase = Phase::MeasureAverageLoudness;
                    }
                }
            }
        }
    }

    /// The pulse (or silence) for this block, written over `audio`.
    pub fn process_output(&mut self, audio: &mut [f32]) {
        if self.phase == Phase::Passthrough {
            return;
        }
        let n = self.buffersize.min(audio.len());
        audio[..n].fill(0.0);
        if self.playing && self.pulse_pos < self.pulse.len() {
            let k = n.min(self.pulse.len() - self.pulse_pos);
            audio[..k].copy_from_slice(&self.pulse[self.pulse_pos..self.pulse_pos + k]);
            self.pulse_pos += k;
        }
    }
}

/// One peer's latency test, shared by the control side and the audio thread.
#[derive(Debug, Default)]
pub struct LatencyShared {
    pub active: AtomicBool,
    /// Bumped by the control side to (re)start the measurer.
    pub restart: AtomicU32,
    /// Measurer `state` and `latencyMs` (f32 bits) as of the last block.
    pub state: AtomicI32,
    pub latency_bits: AtomicU32,
    /// The `restart` value the measurer of `state` and `latency_bits` was started with
    /// (stored after them).
    pub generation: AtomicU32,
}

impl LatencyShared {
    /// Round trip in ms once at least one measurement of the current test (the current
    /// `restart`) came back (upstream: `state > 1`). Values the audio thread stored for a
    /// previous test (stopped and restarted before it saw the restart) are not reported.
    pub fn measured(&self) -> Option<(i32, f32)> {
        let current = self.restart.load(Ordering::Acquire);
        if self.generation.load(Ordering::Acquire) != current {
            return None;
        }
        let s = self.state.load(Ordering::Acquire);
        (s > 1).then(|| (s, f32::from_bits(self.latency_bits.load(Ordering::Relaxed))))
    }
}

/// `commitLatencyMatch` for one peer: the jitter buffer that makes its incoming latency
/// `target_ms`, given `baseline_ms` = incoming latency with buffer `base_buffer_ms`. `None`
/// when the peer is already at or above the target (upstream leaves it alone).
pub fn match_buffer(baseline_ms: f32, base_buffer_ms: f32, target_ms: f32) -> Option<f32> {
    (baseline_ms < target_ms).then_some(base_buffer_ms + (target_ms - baseline_ms))
}

/// The default target of the latency-match view: the largest latency in the table
/// (LatencyMatchView.cpp:173-191).
pub fn match_target(latencies: impl IntoIterator<Item = f32>) -> f32 {
    latencies.into_iter().fold(0.0, f32::max)
}

/// Receive Sync: the extra buffer per peer so that every peer's incoming latency equals the
/// largest one plus `padding_ms`. `bases` are the incoming latencies without any earlier
/// sync padding, so repeating it gives the same buffers.
pub fn receive_sync(bases: &[f32], padding_ms: f32) -> Vec<f32> {
    let target = bases.iter().copied().fold(f32::NEG_INFINITY, f32::max) + padding_ms;
    bases.iter().map(|b| target - b).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Review finding 10: a result the audio thread stored for the previous test (it had not
    /// seen the restart yet) is not reported for the new one.
    #[test]
    fn measurements_of_an_old_test_are_ignored() {
        let l = LatencyShared::default();
        l.state.store(3, Ordering::Relaxed);
        l.latency_bits.store(42.0f32.to_bits(), Ordering::Relaxed);
        assert_eq!(l.measured(), Some((3, 42.0)));
        // stop, start: the control side bumps `restart`; the audio thread still stores the
        // old measurer's values with the old generation
        l.restart.fetch_add(1, Ordering::AcqRel);
        l.state.store(4, Ordering::Relaxed);
        assert_eq!(l.measured(), None);
        // the new measurer's first result
        l.state.store(2, Ordering::Relaxed);
        l.latency_bits.store(80.0f32.to_bits(), Ordering::Relaxed);
        l.generation.store(1, Ordering::Release);
        assert_eq!(l.measured(), Some((2, 80.0)));
    }

    /// A loopback with `delay` frames: the measurer hears its own pulse back.
    fn measure(delay: usize, block: usize, blocks: usize, noise: f32) -> LatencyMeasurer {
        let mut m = LatencyMeasurer::new();
        m.start_peer_test();
        let mut line = vec![0.0f32; delay + block];
        let mut buf = vec![0.0f32; block];
        for b in 0..blocks {
            // what comes back now went out `delay` frames ago
            buf.copy_from_slice(&line[..block]);
            for (i, x) in buf.iter_mut().enumerate() {
                *x += noise * (((b * block + i) * 7919 % 1000) as f32 / 1000.0 - 0.5);
            }
            m.process_input(&buf, 48000);
            m.process_output(&mut buf);
            line.drain(..block);
            line.extend_from_slice(&buf);
        }
        m
    }

    #[test]
    fn loopback_round_trip_is_measured() {
        // what is measured is the line plus one block (the pulse goes out in the block after
        // the one it is heard in, as with a device); its second sample crosses 0.2
        let m = measure(1000, 256, 600, 0.0);
        assert!(m.state > 3, "several measurements: {}", m.state);
        let want = (1000.0 + 256.0 + 1.0) / 48.0;
        assert!(
            (m.latency_ms - want).abs() < 0.3,
            "{} vs {want}",
            m.latency_ms
        );
    }

    #[test]
    fn override_threshold_is_used_not_the_noise_floor() {
        // noise at +-0.15 stays under the 0.2 override (upstream's inverted branch would
        // compute ~noise floor + 24 dB = way above, never detect, and report -1)
        let m = measure(2000, 128, 1500, 0.3);
        assert!(m.state > 1, "measured despite the noise");
        let want = (2000.0 + 128.0 + 1.0) / 48.0;
        assert!(
            (m.latency_ms - want).abs() < 0.5,
            "{} vs {want}",
            m.latency_ms
        );
    }

    #[test]
    fn match_and_sync_arithmetic() {
        // P.cpp:3066-3082 worked by hand: block 5.333, ping 20, Opus, remote inlat 5.333,
        // buffer 15 -> baseline = 2.5 + 5.333 + 10 + 15 = 32.833; target 50 -> 32.167 buffer
        let base = 2.5 + 5.333 + 10.0 + 15.0;
        let b = match_buffer(base, 15.0, 50.0).unwrap();
        assert!((b - (15.0 + 50.0 - base)).abs() < 1e-4);
        assert_eq!(match_buffer(60.0, 15.0, 50.0), None);
        assert_eq!(match_target([12.0, 47.5, 30.0]), 47.5);
        let pads = receive_sync(&[20.0, 35.0, 28.0], 10.0);
        assert_eq!(pads, vec![25.0, 10.0, 17.0]);
        assert_eq!(receive_sync(&[20.0, 35.0, 28.0], 10.0), pads, "idempotent");
    }
}
