//! Variable-ratio linear interpolating resampler over interleaved samples, a direct port of
//! `dynamic_resampler` (`common.cpp:847-962`) so output matches the C++ bit for bit.
//!
//! ponytail: linear interpolation as upstream; swap the body of `read` for a better kernel if
//! drift compensation artefacts matter. All methods are allocation free except `setup`.

/// Extra capacity factor (`AOO_RESAMPLER_SPACE`).
const SPACE: f64 = 2.5;

/// See the module docs. Interleaved samples, `nch` channels.
#[derive(Debug, Default)]
pub struct Resampler {
    buf: Vec<f32>,
    nch: usize,
    rdpos: f64,
    wrpos: usize,
    balance: f64,
    ratio: f64,
}

impl Resampler {
    /// Allocates; `nfrom`/`nto` are block sizes in frames.
    pub fn setup(&mut self, nfrom: usize, nto: usize, nch: usize) {
        self.nch = nch.max(1);
        let size = (nfrom.max(nto) * self.nch) as f64 * SPACE;
        self.buf.clear();
        self.buf.resize(size as usize, 0.0);
        self.clear();
    }

    pub fn clear(&mut self) {
        self.ratio = 1.0;
        self.rdpos = 0.0;
        self.wrpos = 0;
        self.balance = 0.0;
    }

    pub fn update(&mut self, srfrom: f64, srto: f64) {
        self.ratio = if srfrom == srto { 1.0 } else { srto / srfrom };
    }

    /// Free space in samples.
    pub fn write_available(&self) -> usize {
        (self.buf.len() as f64 - self.balance).max(0.0) as usize
    }

    /// `data.len()` must not exceed [`Self::write_available`].
    pub fn write(&mut self, data: &[f32]) {
        let size = self.buf.len();
        let n = data.len();
        let split = n.min(size - self.wrpos);
        self.buf[self.wrpos..self.wrpos + split].copy_from_slice(&data[..split]);
        self.buf[..n - split].copy_from_slice(&data[split..]);
        self.wrpos += n;
        if self.wrpos >= size {
            self.wrpos -= size;
        }
        self.balance += n as f64;
    }

    /// Output samples that can be read.
    pub fn read_available(&self) -> usize {
        (self.balance * self.ratio).max(0.0) as usize
    }

    /// `out.len()` must be a multiple of the channel count and not exceed
    /// [`Self::read_available`].
    pub fn read(&mut self, out: &mut [f32]) {
        let size = self.buf.len();
        let nch = self.nch;
        let limit = (size / nch) as f64;
        let n = out.len();
        let intpos = self.rdpos as usize;
        if self.ratio != 1.0 || self.rdpos - intpos as f64 != 0.0 {
            let incr = 1.0 / self.ratio;
            for frame in out.chunks_exact_mut(nch) {
                let index = self.rdpos as usize;
                let fract = self.rdpos - index as f64;
                for (j, o) in frame.iter_mut().enumerate() {
                    let a = self.buf[index * nch + j] as f64;
                    let b = self.buf[((index + 1) * nch + j) % size] as f64;
                    *o = (a + (b - a) * fract) as f32;
                }
                self.rdpos += incr;
                if self.rdpos >= limit {
                    self.rdpos -= limit;
                }
            }
            self.balance -= n as f64 * incr;
        } else {
            let pos = intpos * nch;
            let n1 = n.min(size - pos);
            out[..n1].copy_from_slice(&self.buf[pos..pos + n1]);
            out[n1..].copy_from_slice(&self.buf[..n - n1]);
            self.rdpos += (n / nch) as f64;
            if self.rdpos >= limit {
                self.rdpos -= limit;
            }
            self.balance -= n as f64;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_ratio_is_a_copy() {
        let mut r = Resampler::default();
        r.setup(64, 64, 2);
        let input: Vec<f32> = (0..10_000).map(|i| i as f32).collect();
        let mut out = Vec::new();
        let mut buf = [0.0; 48];
        for chunk in input.chunks(128) {
            r.write(chunk);
            while r.read_available() >= buf.len() {
                r.read(&mut buf);
                out.extend_from_slice(&buf);
            }
        }
        assert_eq!(out[..], input[..out.len()]);
        assert!(input.len() - out.len() < 48);
    }

    /// 48 kHz in, 44.1 kHz out: output/input sample count follows the ratio over a long run and
    /// a ramp stays a ramp.
    #[test]
    fn ratio_sanity() {
        let mut r = Resampler::default();
        r.setup(480, 441, 1);
        r.update(48000.0, 44100.0);
        let (mut produced, mut consumed) = (0usize, 0usize);
        let mut buf = vec![0.0; 441];
        let mut last = -1.0f32;
        for block in 0..1000 {
            let input: Vec<f32> = (0..480).map(|i| (block * 480 + i) as f32 * 1e-6).collect();
            r.write(&input);
            consumed += 480;
            while r.read_available() >= buf.len() {
                r.read(&mut buf);
                produced += buf.len();
                for &x in &buf {
                    assert!(x >= last, "not monotonic");
                    last = x;
                }
            }
        }
        let ratio = produced as f64 / consumed as f64;
        assert!((ratio - 44100.0 / 48000.0).abs() < 1e-3, "{ratio}");
        assert!(r.write_available() >= 480);
    }
}
