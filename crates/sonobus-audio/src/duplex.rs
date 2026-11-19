//! The hardware-free heart of the device layer: turns "the output device wants N frames now"
//! into fixed-size engine blocks, pulling one aligned block from every input per engine call.

use cpal::{FromSample, Sample};

use crate::port::{ExternalInput, InputPort};

/// Most input or output channels one engine block carries (pointer arrays live on the stack).
pub const MAX_CHANNELS: usize = 64;

/// One engine block: `frames` samples per channel, planar, at `sample_rate`.
pub struct AudioBlock<'a> {
    /// Engine input channels: the inputs in the order they were added, each a contiguous range.
    pub inputs: &'a [&'a [f32]],
    /// Engine output channels, zeroed before the engine is called.
    pub outputs: &'a mut [&'a mut [f32]],
    pub sample_rate: u32,
    pub frames: usize,
}

/// Drives an engine callback with fixed-size blocks. Owns the engine side of every input; the
/// clock comes from whoever calls [`render`](Self::render) or [`advance`](Self::advance).
pub struct Duplex<F> {
    engine: F,
    sample_rate: u32,
    block: usize,
    out_channels: usize,
    in_channels: usize,
    ports: Vec<InputPort>,
    in_buf: Vec<f32>,
    out_buf: Vec<f32>,
    /// Next frame of `out_buf` to hand out; `block` means it is used up.
    pos: usize,
    blocks: u64,
}

impl<F: FnMut(&mut AudioBlock<'_>)> Duplex<F> {
    /// Panics if `block` is 0 or `out_channels` exceeds [`MAX_CHANNELS`].
    pub fn new(engine: F, sample_rate: u32, block: usize, out_channels: usize) -> Self {
        assert!(block > 0 && sample_rate > 0 && out_channels <= MAX_CHANNELS);
        Duplex {
            engine,
            sample_rate,
            block,
            out_channels,
            in_channels: 0,
            ports: Vec::new(),
            in_buf: Vec::new(),
            out_buf: vec![0.0; out_channels * block],
            pos: block,
            blocks: 0,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn block_size(&self) -> usize {
        self.block
    }

    pub fn out_channels(&self) -> usize {
        self.out_channels
    }

    pub fn in_channels(&self) -> usize {
        self.in_channels
    }

    /// Engine blocks processed so far.
    pub fn blocks(&self) -> u64 {
        self.blocks
    }

    /// Adds an input occupying the next `channels` engine input channels, fed at `sample_rate`
    /// on its own clock, and returns its producer. `margin` is the safety slack in input frames
    /// the drift controller keeps on top of the jitter it observes (see [`default_margin`]).
    /// Not real-time safe (allocates); call before the stream starts. Panics past
    /// [`MAX_CHANNELS`] inputs channels.
    pub fn add_input(&mut self, channels: usize, sample_rate: u32, margin: usize) -> ExternalInput {
        assert!(self.in_channels + channels <= MAX_CHANNELS);
        let (port, input) = InputPort::new(channels, sample_rate, self.sample_rate, self.block, margin);
        self.ports.push(port);
        self.in_channels += channels;
        self.in_buf = vec![0.0; self.in_channels * self.block];
        input
    }

    /// Replaces input `index` with a fresh ring at `sample_rate` (e.g. after its device was
    /// reopened), keeping its channel range. Not real-time safe.
    pub fn reset_input(&mut self, index: usize, sample_rate: u32, margin: usize) -> ExternalInput {
        let ch = self.ports[index].channels();
        let (port, input) = InputPort::new(ch, sample_rate, self.sample_rate, self.block, margin);
        self.ports[index] = port;
        input
    }

    /// Engine-rate frames of filter delay each input's resampler adds.
    pub fn input_delays(&self) -> Vec<usize> {
        self.ports.iter().map(InputPort::resampler_delay).collect()
    }

    pub fn into_engine(self) -> F {
        self.engine
    }

    /// Fills an interleaved device buffer of `out_channels` channels (any length), running the
    /// engine whenever a new block is needed. Real-time safe.
    pub fn render<T: Sample + FromSample<f32>>(&mut self, out: &mut [T]) {
        let ch = self.out_channels;
        if ch == 0 {
            return;
        }
        let block = self.block;
        self.advance_with(out.len() / ch, |buf, src, dst, n| {
            for f in 0..n {
                for c in 0..ch {
                    out[(dst + f) * ch + c] = T::from_sample(buf[c * block + src + f]);
                }
            }
        });
    }

    /// Advances the engine by `frames` without a device to play the output to (input-only mode:
    /// call from the clock-master input's callback). Real-time safe.
    pub fn advance(&mut self, frames: usize) {
        self.advance_with(frames, |_, _, _, _| {});
    }

    /// `copy(out_buf, src_frame, dst_frame, frames)` moves the produced audio out.
    fn advance_with(&mut self, frames: usize, mut copy: impl FnMut(&[f32], usize, usize, usize)) {
        for p in &mut self.ports {
            p.note_burst(frames);
        }
        let mut done = 0;
        while done < frames {
            if self.pos == self.block {
                self.run_block();
                self.pos = 0;
            }
            let n = (self.block - self.pos).min(frames - done);
            copy(&self.out_buf, self.pos, done, n);
            self.pos += n;
            done += n;
        }
    }

    fn run_block(&mut self) {
        let b = self.block;
        let mut off = 0;
        for p in &mut self.ports {
            let len = p.channels() * b;
            p.pull(&mut self.in_buf[off..off + len]);
            off += len;
        }
        self.out_buf.fill(0.0);
        let mut ins: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
        for (r, c) in ins.iter_mut().zip(self.in_buf.chunks_exact(b)) {
            *r = c;
        }
        let mut outs: [&mut [f32]; MAX_CHANNELS] = std::array::from_fn(|_| Default::default());
        for (r, c) in outs.iter_mut().zip(self.out_buf.chunks_exact_mut(b)) {
            *r = c;
        }
        let mut blk = AudioBlock {
            inputs: &ins[..self.in_channels],
            outputs: &mut outs[..self.out_channels],
            sample_rate: self.sample_rate,
            frames: b,
        };
        (self.engine)(&mut blk);
        self.blocks += 1;
    }
}

/// Default drift-controller safety margin: 1 ms of input frames, at least 16.
pub fn default_margin(sample_rate: u32) -> usize {
    (sample_rate as usize / 1000).max(16)
}
