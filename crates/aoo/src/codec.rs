//! Stream formats, codec option blobs carried in `/format` and `/codecchange`, and the PCM and
//! Opus block codecs (spec 4.5, 4.6; `codec_pcm.cpp`, `codec_opus.cpp`).

use crate::{Error, opus};

pub const CODEC_PCM: &str = "pcm";
pub const CODEC_OPUS: &str = "opus";

/// PCM sample format, sent as one big-endian int32 (`aoo_pcm.h:18-25`, `codec_pcm.cpp:276-296`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PcmBitDepth {
    Int16 = 0,
    Int24 = 1,
    Float32 = 2,
    Float64 = 3,
}

impl PcmBitDepth {
    pub fn to_bytes(self) -> [u8; 4] {
        (self as i32).to_be_bytes()
    }

    /// Extra bytes after the first 4 are ignored, as in `codec_pcm.cpp:354-366`.
    pub fn from_bytes(b: &[u8]) -> Result<Self, Error> {
        let v = i32::from_be_bytes(*b.first_chunk().ok_or(Error::BadOptions)?);
        Ok(match v {
            0 => Self::Int16,
            1 => Self::Int24,
            2 => Self::Float32,
            3 => Self::Float64,
            _ => return Err(Error::BadOptions),
        })
    }
}

/// libopus `opus_defines.h` values used by SonoBus presets and the 12-byte default.
pub const OPUS_SIGNAL_MUSIC: i32 = 3002;
pub const OPUS_APPLICATION_AUDIO: i32 = 2049;
pub const OPUS_APPLICATION_RESTRICTED_LOWDELAY: i32 = 2051;

/// Opus options: four big-endian int32 (`codec_opus.cpp:213-237`). Values are passed through
/// unvalidated; libopus validates them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpusOptions {
    /// Total bits per second over all channels.
    pub bitrate: i32,
    pub complexity: i32,
    pub signal_type: i32,
    pub application_type: i32,
}

impl OpusOptions {
    pub fn to_bytes(&self) -> [u8; 16] {
        let mut b = [0; 16];
        let fields = [
            self.bitrate,
            self.complexity,
            self.signal_type,
            self.application_type,
        ];
        for (chunk, v) in b.as_chunks_mut::<4>().0.iter_mut().zip(fields) {
            *chunk = v.to_be_bytes();
        }
        b
    }

    /// Accepts 12 bytes (application defaults to AUDIO) or more (`codec_opus.cpp:245-259`).
    pub fn from_bytes(b: &[u8]) -> Result<Self, Error> {
        if b.len() < 12 {
            return Err(Error::BadOptions);
        }
        let int = |i: usize| i32::from_be_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        Ok(Self {
            bitrate: int(0),
            complexity: int(4),
            signal_type: int(8),
            application_type: if b.len() >= 16 {
                int(12)
            } else {
                OPUS_APPLICATION_AUDIO
            },
        })
    }
}

/// A stream format as carried in `/format` (codec name + options blob).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Format {
    pub nchannels: i32,
    pub samplerate: i32,
    pub blocksize: i32,
    pub codec: Codec,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Codec {
    Pcm(PcmBitDepth),
    Opus(OpusOptions),
}

/// Upper bounds for formats received from the network, so a peer cannot make us allocate
/// unbounded buffers. Generous compared to anything SonoBus sends.
const MAX_CHANNELS: i32 = 255;
const MAX_BLOCKSIZE: i32 = 1 << 14;
const MAX_SAMPLERATE: i32 = 768_000;

impl Format {
    pub fn codec_name(&self) -> &'static str {
        match self.codec {
            Codec::Pcm(_) => CODEC_PCM,
            Codec::Opus(_) => CODEC_OPUS,
        }
    }

    /// Options blob: 4 bytes for PCM, 16 for Opus.
    pub fn options(&self) -> ([u8; 16], usize) {
        let mut b = [0; 16];
        match self.codec {
            Codec::Pcm(d) => {
                b[..4].copy_from_slice(&d.to_bytes());
                (b, 4)
            }
            Codec::Opus(o) => (o.to_bytes(), 16),
        }
    }

    /// Parse the format fields of `/format` or `/codecchange`. Unknown codecs, bad options and
    /// values outside sane bounds are errors.
    pub fn from_wire(
        codec: &str,
        nchannels: i32,
        samplerate: i32,
        blocksize: i32,
        options: &[u8],
    ) -> Result<Self, Error> {
        let codec = match codec {
            CODEC_PCM => Codec::Pcm(PcmBitDepth::from_bytes(options)?),
            CODEC_OPUS => Codec::Opus(OpusOptions::from_bytes(options)?),
            _ => return Err(Error::BadOptions),
        };
        Ok(Self {
            nchannels,
            samplerate,
            blocksize,
            codec,
        })
    }

    /// Bounds check for a format a sink is told to decode (the C++ decoders do not validate).
    pub(crate) fn check_decodable(&self) -> bool {
        (1..=MAX_CHANNELS).contains(&self.nchannels)
            && (1..=MAX_SAMPLERATE).contains(&self.samplerate)
            && (1..=MAX_BLOCKSIZE).contains(&self.blocksize)
    }

    /// Encoder-side validation, as `codec_setformat` (`codec_pcm.cpp:137-175`) and
    /// `validate_format` (`codec_opus.cpp:62-104`) do it.
    pub fn validated(mut self) -> Self {
        match self.codec {
            Codec::Pcm(_) => {
                if self.blocksize <= 0 {
                    self.blocksize = 64;
                }
                if self.samplerate <= 0 {
                    self.samplerate = 44100;
                }
                if !(1..=255).contains(&self.nchannels) {
                    self.nchannels = 1;
                }
            }
            Codec::Opus(ref mut o) => {
                if ![8000, 12000, 16000, 24000, 48000].contains(&self.samplerate) {
                    self.samplerate = 48000;
                }
                if !(1..=255).contains(&self.nchannels) {
                    self.nchannels = 1;
                }
                let mut min = self.samplerate / 400;
                let max = min * 24;
                if self.blocksize < min {
                    self.blocksize = min;
                } else if self.blocksize > max {
                    self.blocksize = max;
                } else {
                    while self.blocksize > min * 2 - 1 {
                        min *= 2;
                    }
                    self.blocksize = min;
                }
                if o.application_type == 0 {
                    o.application_type = OPUS_APPLICATION_AUDIO;
                }
            }
        }
        self.blocksize = self.blocksize.min(MAX_BLOCKSIZE);
        self
    }
}

impl PcmBitDepth {
    /// Bytes per sample.
    pub fn sample_size(self) -> usize {
        match self {
            Self::Int16 => 2,
            Self::Int24 => 3,
            Self::Float32 => 4,
            Self::Float64 => 8,
        }
    }
}

/// `sample_to_int16`: `x * 32767 + 0.5` truncated, clamped.
fn to_int16(x: f32) -> [u8; 2] {
    let t = (x * 32767.0 + 0.5) as i32;
    (t.clamp(i16::MIN as i32, i16::MAX as i32) as i16).to_be_bytes()
}

/// `sample_to_int24`: top 3 bytes of `x * 2^31 + 0.5`. `as` saturates, which fixes the C++
/// overflow at full scale (where `+1.0` wrapped to negative full scale).
fn to_int24(x: f32) -> [u8; 3] {
    let t = (x * 2_147_483_648.0 + 0.5) as i32;
    let b = t.to_be_bytes();
    [b[0], b[1], b[2]]
}

/// Big-endian PCM of `samples` into `out` (which must hold `samples.len() * sample_size`
/// bytes); returns the bytes written.
pub fn pcm_encode(depth: PcmBitDepth, samples: &[f32], out: &mut [u8]) -> usize {
    let ss = depth.sample_size();
    for (x, o) in samples.iter().zip(out.chunks_exact_mut(ss)) {
        match depth {
            PcmBitDepth::Int16 => o.copy_from_slice(&to_int16(*x)),
            PcmBitDepth::Int24 => o.copy_from_slice(&to_int24(*x)),
            PcmBitDepth::Float32 => o.copy_from_slice(&x.to_be_bytes()),
            PcmBitDepth::Float64 => o.copy_from_slice(&(*x as f64).to_be_bytes()),
        }
    }
    samples.len() * ss
}

/// Decodes as many samples as `data` holds and zeroes the rest of `out` (the C++ over-reads a
/// short block and leaves `out` stale for a long one).
pub fn pcm_decode(depth: PcmBitDepth, data: &[u8], out: &mut [f32]) {
    let ss = depth.sample_size();
    let mut chunks = data.chunks_exact(ss);
    for o in out.iter_mut() {
        *o = match chunks.next() {
            None => 0.0,
            Some(b) => match depth {
                PcmBitDepth::Int16 => i16::from_be_bytes([b[0], b[1]]) as f32 / 32768.0,
                PcmBitDepth::Int24 => {
                    i32::from_be_bytes([b[0], b[1], b[2], 0]) as f32 / 2_147_483_648.0
                }
                PcmBitDepth::Float32 => f32::from_be_bytes([b[0], b[1], b[2], b[3]]),
                PcmBitDepth::Float64 => f64::from_be_bytes(b.try_into().unwrap_or([0; 8])) as f32,
            },
        };
    }
}

/// Block encoder for one format.
pub(crate) enum Encoder {
    Pcm(PcmBitDepth),
    Opus(opus::Encoder),
}

impl Encoder {
    /// `f` must be [`Format::validated`]. For Opus the returned format carries the complexity
    /// and signal type libopus accepted, like `encoder_setformat` writes them back.
    pub fn new(f: &Format) -> Result<(Self, Format), Error> {
        match f.codec {
            Codec::Pcm(d) => Ok((Self::Pcm(d), *f)),
            Codec::Opus(o) => {
                let (enc, complexity, signal_type) = opus::Encoder::new(
                    f.samplerate,
                    f.nchannels,
                    o.application_type,
                    o.bitrate,
                    o.complexity,
                    o.signal_type,
                )
                .map_err(|_| Error::BadOptions)?;
                let codec = Codec::Opus(OpusOptions {
                    complexity,
                    signal_type,
                    ..o
                });
                Ok((Self::Opus(enc), Format { codec, ..*f }))
            }
        }
    }

    /// Worst-case encoded size of one block (the C++ `sendbuffer_` size).
    pub fn max_size(f: &Format) -> usize {
        8 * f.nchannels as usize * f.blocksize as usize
    }

    /// Encodes one interleaved block into `out`; returns the size, 0 on failure.
    pub fn encode(&mut self, samples: &[f32], out: &mut [u8]) -> usize {
        match self {
            Self::Pcm(d) => pcm_encode(*d, samples, out),
            Self::Opus(e) => e.encode(samples, out).unwrap_or(0),
        }
    }

    pub fn reset(&mut self) {
        if let Self::Opus(e) = self {
            e.reset();
        }
    }
}

/// Block decoder for one format.
pub(crate) enum Decoder {
    Pcm(PcmBitDepth),
    Opus(opus::Decoder),
}

impl Decoder {
    /// `decoder_readformat`: no validation beyond what libopus does.
    pub fn new(f: &Format) -> Result<Self, Error> {
        match f.codec {
            Codec::Pcm(d) => Ok(Self::Pcm(d)),
            Codec::Opus(_) => opus::Decoder::new(f.samplerate, f.nchannels)
                .map(Self::Opus)
                .map_err(|_| Error::BadOptions),
        }
    }

    /// Decodes one block into `out` (one block of interleaved samples). `None` = missing block:
    /// silence for PCM, packet loss concealment for Opus. Returns `false` on decoder error.
    pub fn decode(&mut self, data: Option<&[u8]>, out: &mut [f32]) -> bool {
        match (self, data) {
            (Self::Pcm(_), None) => {
                out.fill(0.0);
                true
            }
            (Self::Pcm(d), Some(data)) => {
                pcm_decode(*d, data, out);
                true
            }
            (Self::Opus(dec), data) => dec.decode(data, out).is_ok(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pcm() {
        assert_eq!(PcmBitDepth::Int24.to_bytes(), [0, 0, 0, 1]);
        assert_eq!(
            PcmBitDepth::from_bytes(&[0, 0, 0, 3]),
            Ok(PcmBitDepth::Float64)
        );
        assert_eq!(
            PcmBitDepth::from_bytes(&[0, 0, 0, 2, 9]),
            Ok(PcmBitDepth::Float32)
        );
        for d in [
            PcmBitDepth::Int16,
            PcmBitDepth::Int24,
            PcmBitDepth::Float32,
            PcmBitDepth::Float64,
        ] {
            assert_eq!(PcmBitDepth::from_bytes(&d.to_bytes()), Ok(d));
        }
        assert_eq!(PcmBitDepth::from_bytes(&[0, 0, 0]), Err(Error::BadOptions));
        assert_eq!(
            PcmBitDepth::from_bytes(&[0, 0, 0, 4]),
            Err(Error::BadOptions)
        );
        assert_eq!(PcmBitDepth::from_bytes(&[0xff; 4]), Err(Error::BadOptions));
    }

    #[test]
    fn opus() {
        let o = OpusOptions {
            bitrate: 192_000,
            complexity: 10,
            signal_type: OPUS_SIGNAL_MUSIC,
            application_type: OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        };
        let golden = [
            0, 2, 0xee, 0, 0, 0, 0, 10, 0, 0, 0x0b, 0xba, 0, 0, 0x08, 0x03,
        ];
        assert_eq!(o.to_bytes(), golden);
        assert_eq!(OpusOptions::from_bytes(&golden), Ok(o));
        let short = OpusOptions::from_bytes(&golden[..12]).unwrap();
        assert_eq!(short.application_type, OPUS_APPLICATION_AUDIO);
        assert_eq!(short.signal_type, OPUS_SIGNAL_MUSIC);
        assert_eq!(
            OpusOptions::from_bytes(&golden[..11]),
            Err(Error::BadOptions)
        );
    }

    #[test]
    fn pcm_int16() {
        assert_eq!(to_int16(0.0), [0, 0]);
        assert_eq!(to_int16(1.0), 32767i16.to_be_bytes());
        assert_eq!(to_int16(-1.0), (-32766i16).to_be_bytes()); // C++: trunc(-32767 + 0.5)
        assert_eq!(to_int16(2.0), 32767i16.to_be_bytes());
        assert_eq!(to_int16(-2.0), (-32768i16).to_be_bytes());
        assert_eq!(to_int16(0.5), 16384i16.to_be_bytes()); // trunc(16383.5 + 0.5)
        assert_eq!(to_int16(f32::NAN), [0, 0]);
    }

    #[test]
    fn pcm_int24_clamps_full_scale() {
        assert_eq!(to_int24(1.0), [0x7f, 0xff, 0xff]); // the C++ gives 0x800000 here
        assert_eq!(to_int24(10.0), [0x7f, 0xff, 0xff]);
        assert_eq!(to_int24(-1.0), [0x80, 0, 0]);
        assert_eq!(to_int24(-10.0), [0x80, 0, 0]);
        assert_eq!(to_int24(0.5), [0x40, 0, 0]);
    }

    #[test]
    fn pcm_round_trips() {
        let input: Vec<f32> = (0..200).map(|i| (i as f32 * 0.37).sin() * 0.9).collect();
        for (d, tol) in [
            (PcmBitDepth::Int16, 3.0 / 32768.0), // truncation + 32767/32768 scale, as the C++,
            (PcmBitDepth::Int24, 1.0 / 8_000_000.0),
            (PcmBitDepth::Float32, 0.0),
            (PcmBitDepth::Float64, 0.0),
        ] {
            let mut bytes = vec![0; 200 * 8];
            let n = pcm_encode(d, &input, &mut bytes);
            assert_eq!(n, 200 * d.sample_size());
            let mut out = vec![1.0; 210];
            pcm_decode(d, &bytes[..n], &mut out);
            for (a, b) in input.iter().zip(&out) {
                assert!((a - b).abs() <= tol, "{d:?}: {a} vs {b}");
            }
            assert!(out[200..].iter().all(|&x| x == 0.0));
        }
        // hand-derived bytes
        let mut b = [0; 8];
        pcm_encode(PcmBitDepth::Float32, &[-0.5], &mut b);
        assert_eq!(b[..4], [0xbf, 0, 0, 0]);
        pcm_encode(PcmBitDepth::Float64, &[1.0], &mut b);
        assert_eq!(b, [0x3f, 0xf0, 0, 0, 0, 0, 0, 0]);
        let mut o = [0.0];
        pcm_decode(PcmBitDepth::Int16, &[0x80, 0], &mut o);
        assert_eq!(o[0], -1.0);
    }

    #[test]
    fn format_validation() {
        let opus = |sr, bs| {
            Format {
                nchannels: 2,
                samplerate: sr,
                blocksize: bs,
                codec: Codec::Opus(OpusOptions::from_bytes(&[0; 12]).unwrap()),
            }
            .validated()
        };
        assert_eq!(opus(48000, 256).blocksize, 240);
        assert_eq!(opus(48000, 100).blocksize, 120);
        assert_eq!(opus(48000, 5000).blocksize, 2880);
        assert_eq!(opus(48000, 1919).blocksize, 960);
        assert_eq!(opus(44100, 512).samplerate, 48000);
        let f = Format::from_wire("pcm", 2, 48000, 256, &[0, 0, 0, 1]).unwrap();
        assert_eq!(f.codec, Codec::Pcm(PcmBitDepth::Int24));
        assert_eq!(f.options().1, 4);
        assert!(Format::from_wire("mp3", 2, 48000, 256, &[]).is_err());
    }
}
