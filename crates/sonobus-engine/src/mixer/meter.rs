//! Level meters as ff_meters' `LevelMeterSource::measureBlock` computes them (the version in
//! `deps/ff_meters/LevelMeter/LevelMeterSource.h`): per block the magnitude and RMS of each
//! channel; the peak is held for 500 ms (here: in audio blocks, the audio thread has no clock),
//! the RMS is averaged over `sr * 0.03 / block` blocks (`METER_RMS_SEC`, P.cpp:155, 7019).

use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use super::MAX_CHANNELS;

/// Published levels of one meter; written by the audio thread, read by anyone.
#[derive(Debug)]
pub struct Meter {
    channels: AtomicUsize,
    peak: [AtomicU32; MAX_CHANNELS],
    rms: [AtomicU32; MAX_CHANNELS],
}

impl Default for Meter {
    fn default() -> Self {
        Self {
            channels: AtomicUsize::new(0),
            peak: std::array::from_fn(|_| AtomicU32::new(0)),
            rms: std::array::from_fn(|_| AtomicU32::new(0)),
        }
    }
}

/// Linear levels, one entry per metered channel.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MeterLevels {
    /// Held peak, clamped to 1.
    pub peak: Vec<f32>,
    /// RMS over the last ~30 ms, clamped to 1.
    pub rms: Vec<f32>,
}

impl Meter {
    pub fn levels(&self) -> MeterLevels {
        let n = self.channels.load(Ordering::Relaxed).min(MAX_CHANNELS);
        let get = |a: &[AtomicU32]| {
            a[..n]
                .iter()
                .map(|v| f32::from_bits(v.load(Ordering::Relaxed)))
                .collect()
        };
        MeterLevels {
            peak: get(&self.peak),
            rms: get(&self.rms),
        }
    }
}

/// Audio-thread state of one meter (RMS history and peak hold); preallocated.
pub(crate) struct MeterState {
    window: usize,
    history: Vec<f32>,
    pos: usize,
    max: [f32; MAX_CHANNELS],
    hold_until: [u64; MAX_CHANNELS],
    hold_blocks: u64,
    block: u64,
}

impl MeterState {
    pub fn new(sample_rate: u32, block_size: usize) -> Self {
        let window = ((sample_rate as f64 * 0.03 / block_size.max(1) as f64) as usize).max(1);
        Self {
            window,
            history: vec![0.0; window * MAX_CHANNELS],
            pos: 0,
            max: [0.0; MAX_CHANNELS],
            hold_until: [0; MAX_CHANNELS],
            hold_blocks: (0.5 * sample_rate as f64 / block_size.max(1) as f64).ceil() as u64,
            block: 0,
        }
    }

    /// Measure the first `n` samples of the first `channels` buffers into `out`.
    pub fn measure<B: AsRef<[f32]>>(&mut self, bufs: &[B], channels: usize, n: usize, out: &Meter) {
        let channels = channels.min(bufs.len()).min(MAX_CHANNELS);
        self.block += 1;
        for (c, buf) in bufs[..channels].iter().enumerate() {
            // non-finite samples count as silence
            let x = buf.as_ref()[..n].iter().filter(|v| v.is_finite());
            let mag = x.clone().fold(0.0f32, |m, v| m.max(v.abs()));
            let rms = if n == 0 {
                0.0
            } else {
                (x.map(|&v| f64::from(v) * f64::from(v)).sum::<f64>() / n as f64).sqrt() as f32
            };
            // setLevels
            if mag >= self.max[c] {
                self.max[c] = mag.min(1.0);
                self.hold_until[c] = self.block + self.hold_blocks;
            } else if self.block > self.hold_until[c] {
                self.max[c] = mag.min(1.0);
            }
            // pushNextRMS
            let r = rms.min(1.0);
            let sq = if r.is_normal() { (r * r).min(1.0) } else { 0.0 };
            let hist = &mut self.history[c * self.window..(c + 1) * self.window];
            hist[self.pos] = sq;
            let avg = (hist.iter().sum::<f32>() / self.window as f32).sqrt();
            out.peak[c].store(self.max[c].to_bits(), Ordering::Relaxed);
            out.rms[c].store(avg.to_bits(), Ordering::Relaxed);
        }
        self.pos = (self.pos + 1) % self.window;
        out.channels.store(channels, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_and_dc_levels() {
        let out = Meter::default();
        let mut m = MeterState::new(48000, 256); // window 5 blocks, hold 94 blocks
        let sine: Vec<f32> = (0..256)
            .map(|i| 0.5 * (i as f32 * std::f32::consts::TAU / 64.0).sin())
            .collect();
        let dc = vec![-0.25f32; 256];
        for _ in 0..5 {
            m.measure(&[&sine[..], &dc[..]], 2, 256, &out);
        }
        let l = out.levels();
        assert!((l.peak[0] - 0.5).abs() < 1e-6);
        assert!((l.rms[0] - 0.5 / 2f32.sqrt()).abs() < 1e-4);
        assert_eq!(l.peak[1], 0.25);
        assert!((l.rms[1] - 0.25).abs() < 1e-6);
        // silence: RMS falls after the window, the peak only after the hold time
        let z = vec![0.0f32; 256];
        for _ in 0..5 {
            m.measure(&[&z[..], &z[..]], 2, 256, &out);
        }
        let l = out.levels();
        assert_eq!(l.rms, [0.0, 0.0]);
        assert_eq!(l.peak[1], 0.25);
        for _ in 0..100 {
            m.measure(&[&z[..], &z[..]], 2, 256, &out);
        }
        assert_eq!(out.levels().peak, [0.0, 0.0]);
        // clipping input is clamped
        let loud = vec![2.0f32; 256];
        m.measure(&[&loud[..]], 1, 256, &out);
        assert_eq!(out.levels().peak, [1.0]);
    }
}
