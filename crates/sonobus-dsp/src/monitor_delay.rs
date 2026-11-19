//! Per-channel-group monitor delay: `ChannelGroup::setMonitoringDelay*` and the delay part
//! of `ChannelGroup::processMonitor` (ChannelGroup.cpp:154-200, 406-480), on top of JUCE
//! `dsp::DelayLine<float, None>` (integer delay, max 240000 samples = 5 s at 48 kHz).
//!
//! Transitions are faded by ramping both what goes into the line and what comes out over
//! one block: enabling clears the line and fades in; disabling fades the delayed signal
//! out for one block, after which the undelayed signal returns at full level (no
//! crossfade, as upstream). A time change fades out for one block, then switches the
//! delay and fades in on the next. Changes of at most one sample are ignored.

use crate::ParamSpec;
use crate::gain::apply_gain_ramp;

/// `MAX_DELAY_SAMPLES` in ChannelGroup.cpp (5 s at 48 kHz, less at higher rates).
pub const MAX_DELAY_SAMPLES: usize = 240_000;
/// JUCE allocates `max + 2` samples and reads with a modulo of that size.
const TOTAL: usize = MAX_DELAY_SAMPLES + 2;

#[derive(Debug, Clone)]
pub struct MonitorDelay {
    sample_rate: f64,
    delay_ms: f64,
    delay_samples: f64, // _monitorDelayTimeSamples
    time_changed: bool,
    time_changing: bool,
    active: bool,
    last_active: bool,
    created: bool, // upstream creates the line (with the current delay) on first enable
    bufs: Vec<Box<[f32]>>,
    write_pos: usize,
    read_pos: usize,
    delay_int: usize,
}

impl MonitorDelay {
    /// MonitorDelayView slider; upstream default is 0 ms, disabled.
    pub const DELAY_MS: ParamSpec = ParamSpec { min: 0.0, max: 5000.0, step: 0.1, skew: 0.25, default: 0.0 };

    /// Allocates the delay line for `channels` channels (about 1 MB each).
    pub fn new(sample_rate: u32, channels: usize) -> Self {
        Self {
            sample_rate: f64::from(sample_rate),
            delay_ms: 0.0,
            delay_samples: 0.0,
            time_changed: false,
            time_changing: false,
            active: false,
            last_active: false,
            created: false,
            bufs: (0..channels).map(|_| vec![0.0; TOTAL].into_boxed_slice()).collect(),
            write_pos: 0,
            read_pos: 0,
            delay_int: 0,
        }
    }

    pub fn enabled(&self) -> bool {
        self.active
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        if enabled && !self.created {
            self.created = true;
            self.set_line_delay();
        }
        self.active = enabled;
    }

    pub fn delay_ms(&self) -> f64 {
        self.delay_ms
    }
    pub fn set_delay_ms(&mut self, ms: f64) {
        self.delay_ms = ms;
        let samples = 1e-3 * ms * self.sample_rate;
        if (self.delay_samples - samples).abs() > 1.0 {
            self.delay_samples = samples.min(MAX_DELAY_SAMPLES as f64);
            self.time_changed = true;
        }
    }

    /// JUCE `setDelay((float) samples)` with `None` interpolation.
    fn set_line_delay(&mut self) {
        let d = self.delay_samples as f32;
        let upper = MAX_DELAY_SAMPLES as f32;
        let d = if d < 0.0 { 0.0 } else if upper < d { upper } else { d };
        self.delay_int = d.floor() as usize;
    }

    fn clear_line(&mut self) {
        self.bufs.iter_mut().for_each(|b| b.fill(0.0));
        self.write_pos = 0;
        self.read_pos = 0;
    }

    /// Delays the group's channels in place (one slice per channel, at most the channel
    /// count given to `new`). Leaves them untouched while disabled.
    pub fn process(&mut self, channels: &mut [&mut [f32]]) {
        let active = self.active;
        if !active && !self.last_active {
            return;
        }
        let mut fade = 0i8;
        if self.last_active != active {
            if active {
                self.clear_line();
                fade = 1;
            } else {
                fade = -1;
            }
        }
        if active {
            if self.time_changed && !self.time_changing {
                self.time_changed = false;
                self.time_changing = true;
                fade = -1;
            } else if self.time_changing {
                self.time_changing = false;
                self.set_line_delay();
                fade = 1;
            }
        }
        let (start, end) = if fade > 0 { (0.0, 1.0) } else { (1.0, 0.0) };
        let (w0, r0, d) = (self.write_pos, self.read_pos, self.delay_int);
        for (ch, buf) in channels.iter_mut().zip(&mut self.bufs) {
            if fade != 0 {
                apply_gain_ramp(ch, start, end);
            }
            let (mut w, mut r) = (w0, r0);
            for x in ch.iter_mut() {
                buf[w] = *x;
                w = (w + TOTAL - 1) % TOTAL;
                *x = buf[(r + d) % TOTAL];
                r = (r + TOTAL - 1) % TOTAL;
            }
            (self.write_pos, self.read_pos) = (w, r);
            if fade != 0 {
                apply_gain_ramp(ch, start, end);
            }
        }
        self.last_active = active;
    }
}
