//! Port of upstream `Source/Metronome.{h,cpp}`: sample-based click scheduler.
//!
//! The clicks are the upstream assets `images/beat_click.wav` and `images/bar_click.wav`
//! (16-bit mono 44.1 kHz, played unresampled at any rate, as upstream does); decoding them
//! is the caller's job, this type takes the decoded samples. Upstream never changes
//! beats-per-bar from 1, so only the beat click is ever heard.
//!
//! App-side driving (P.cpp:7690-7726), left to the engine: on enable
//! `set_gain(gain, true)` and, unless synced to host/file, `reset_relative_start(0.0)`;
//! on disable `set_gain(0.0, false)` and render one more block (fade-out); every rendered
//! block `set_gain(gain, false)`, `set_tempo(bpm)`, then `render` with the host PPQ or file
//! beat position (`relative = false`) or free-running (`relative = true`).

use crate::gain::{apply_gain, apply_gain_ramp};

#[derive(Debug, Clone, Copy, Default)]
struct Voice {
    pos: usize,
    remain: i64,
}

#[derive(Debug, Clone)]
pub struct Metronome {
    sample_rate: f64,
    tempo: f64,
    beats_per_bar: i32,
    gain: f32,
    pending_gain: f32,
    bar_remain_ratio: f64,
    beat_remain_ratio: f64,
    beat_pos: f64,
    beat_click: Box<[f32]>,
    bar_click: Box<[f32]>,
    beat_voice: Voice,
    bar_voice: Voice,
}

impl Metronome {
    /// Tempo 100 bpm (as the app sets it), 1 beat per bar, gain 1.
    pub fn new(sample_rate: u32, beat_click: Vec<f32>, bar_click: Vec<f32>) -> Self {
        Self {
            sample_rate: f64::from(sample_rate),
            tempo: 100.0,
            beats_per_bar: 1,
            gain: 1.0,
            pending_gain: 1.0,
            bar_remain_ratio: 0.0,
            beat_remain_ratio: 0.0,
            beat_pos: 0.0,
            beat_click: beat_click.into(),
            bar_click: bar_click.into(),
            beat_voice: Voice::default(),
            bar_voice: Voice::default(),
        }
    }

    pub fn tempo(&self) -> f64 {
        self.tempo
    }
    pub fn set_tempo(&mut self, bpm: f64) {
        self.tempo = bpm;
    }
    /// Values above 1 play the bar click on the first beat of each bar.
    pub fn set_beats_per_bar(&mut self, n: i32) {
        self.beats_per_bar = n;
    }
    /// The new gain ramps in over the next block unless `force`.
    pub fn set_gain(&mut self, gain: f32, force: bool) {
        self.pending_gain = gain;
        if force {
            self.gain = gain;
        }
    }
    /// Restarts free-running mode at `beat` (beats since time zero).
    pub fn reset_relative_start(&mut self, beat: f64) {
        self.beat_pos = beat;
        self.beat_remain_ratio = (1.0 - beat % 1.0) % 1.0;
        self.bar_remain_ratio = (1.0 - (beat / f64::from(self.beats_per_bar)) % 1.0) % 1.0;
    }
    /// Current beat position (advanced in free-running mode, the last `beat_time` otherwise).
    pub fn beat_position(&self) -> f64 {
        self.beat_pos
    }

    /// Writes the click track for one block into `out` (upstream `processMix` into a
    /// cleared buffer). `beat_time` is the absolute beat position of the block start and
    /// is ignored when `relative`, which continues from the previous block instead.
    pub fn render(&mut self, out: &mut [f32], beat_time: f64, relative: bool) {
        out.fill(0.0);
        if self.beat_click.is_empty() || self.tempo == 0.0 {
            return;
        }
        let bpb = i64::from(self.beats_per_bar);
        let frames_in_beat_f = self.sample_rate * 60.0 / self.tempo;
        let frames_in_beat = frames_in_beat_f as i64;
        let frames_in_bar = bpb * frames_in_beat;
        let lrint = |x: f64| x.round_ties_even() as i64;
        let (mut until_bar, mut until_beat) = if relative {
            (
                lrint(self.bar_remain_ratio * frames_in_bar as f64),
                lrint(self.beat_remain_ratio * frames_in_beat as f64),
            )
        } else {
            self.beat_pos = beat_time;
            let beat_frac = beat_time.fract();
            let bar_frac = (beat_time / bpb as f64).fract();
            (
                lrint(((1.0 - bar_frac) % 1.0) * frames_in_bar as f64),
                lrint(((1.0 - beat_frac) % 1.0) * frames_in_beat as f64),
            )
        };
        if until_beat != until_bar && (until_beat - until_bar).abs() < frames_in_beat / 2 {
            until_bar = until_beat;
        }

        let mut frames = out.len() as i64;
        let mut at = 0usize;
        while frames > 0 {
            if until_bar == 0 {
                until_bar = frames_in_bar;
                if bpb > 1 {
                    until_beat = frames_in_beat;
                    self.bar_voice = Voice {
                        pos: 0,
                        remain: self.bar_click.len() as i64,
                    };
                }
            }
            if until_beat == 0 {
                until_beat = frames_in_beat;
                self.beat_voice = Voice {
                    pos: 0,
                    remain: self.beat_click.len() as i64,
                };
            }
            let n = 1.max(frames.min(until_bar.min(until_beat)));
            for (v, click) in [
                (&mut self.bar_voice, &self.bar_click),
                (&mut self.beat_voice, &self.beat_click),
            ] {
                if v.remain > 0 {
                    let k = n.min(v.remain) as usize;
                    for (o, c) in out[at..at + k].iter_mut().zip(&click[v.pos..v.pos + k]) {
                        *o += *c;
                    }
                    v.remain -= k as i64;
                    v.pos += k;
                }
            }
            until_bar -= n;
            until_beat -= n;
            frames -= n;
            at += n as usize;
        }

        if (self.pending_gain - self.gain).abs() > 0.0001 {
            apply_gain_ramp(out, self.gain, self.pending_gain);
            self.gain = self.pending_gain;
        } else if self.gain != 1.0 {
            apply_gain(out, self.gain);
        }

        self.bar_remain_ratio = until_bar as f64 / frames_in_bar as f64;
        self.beat_remain_ratio = until_beat as f64 / frames_in_beat as f64;
        if relative {
            self.beat_pos += out.len() as f64 / frames_in_beat_f;
        }
    }
}
