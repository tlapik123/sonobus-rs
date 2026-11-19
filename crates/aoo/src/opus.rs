//! Safe wrapper over the libopus multistream API. [`Encoder::new`] / [`Decoder::new`] are set up
//! exactly like `codec_opus.cpp:154-209` and `324-371`: `streams = nchannels`,
//! `coupled_streams = 0`, identity mapping. The `with_coupling` constructors pair channels into
//! coupled (stereo) streams instead; with 1 or 2 channels that is plain single-stream Opus.

use opusic_sys as ffi;
use std::ffi::c_int;
use std::ptr::NonNull;

/// `(streams, coupled_streams)`: uncoupled = one mono stream per channel; coupled = channel
/// pairs share a stereo stream (an odd last channel is mono). The identity mapping fits both.
fn layout(nch: i32, coupled: bool) -> (i32, i32) {
    if coupled { (nch - nch / 2, nch / 2) } else { (nch, 0) }
}

fn mapping(nch: usize) -> [u8; 256] {
    let mut m = [255u8; 256];
    for (i, v) in m.iter_mut().take(nch).enumerate() {
        *v = i as u8;
    }
    m
}

fn check_channels(nch: i32) -> Result<(), i32> {
    if (1..=255).contains(&nch) {
        Ok(())
    } else {
        Err(ffi::OPUS_BAD_ARG)
    }
}

/// Opus multistream encoder over interleaved `f32`.
pub struct Encoder {
    st: NonNull<ffi::OpusMSEncoder>,
    nch: usize,
}

// SAFETY: the encoder state is a plain heap block owned exclusively by this value; libopus keeps
// no thread-local or shared state, so moving it to another thread is sound.
unsafe impl Send for Encoder {}

impl Encoder {
    /// Returns the encoder and the complexity and signal type libopus actually applied
    /// (`OPUS_GET_*` after `OPUS_SET_*`, as the C++ does). The bitrate is not read back.
    pub fn new(
        sr: i32,
        nch: i32,
        application: i32,
        bitrate: i32,
        complexity: i32,
        signal: i32,
    ) -> Result<(Self, i32, i32), i32> {
        Self::with_coupling(sr, nch, false, application, bitrate, complexity, signal)
    }

    /// As [`Self::new`], with channel pairs coupled into stereo streams when `coupled`.
    pub fn with_coupling(
        sr: i32,
        nch: i32,
        coupled: bool,
        application: i32,
        bitrate: i32,
        complexity: i32,
        signal: i32,
    ) -> Result<(Self, i32, i32), i32> {
        check_channels(nch)?;
        let map = mapping(nch as usize);
        let (streams, pairs) = layout(nch, coupled);
        let mut err: c_int = 0;
        // SAFETY: `map` holds 256 entries (>= nch); `err` is a valid out pointer.
        let st = unsafe {
            ffi::opus_multistream_encoder_create(
                sr,
                nch,
                streams,
                pairs,
                map.as_ptr(),
                application,
                &mut err,
            )
        };
        let st = match NonNull::new(st) {
            Some(st) if err == ffi::OPUS_OK => st,
            Some(st) => {
                // SAFETY: created above and not used afterwards.
                unsafe { ffi::opus_multistream_encoder_destroy(st.as_ptr()) };
                return Err(err);
            }
            None => return Err(err),
        };
        let enc = Self { st, nch: nch as usize };
        let (mut cplx, mut sig): (i32, i32) = (0, 0);
        // SAFETY: valid state; request codes take one `opus_int32` value or `*mut opus_int32`.
        unsafe {
            let p = enc.st.as_ptr();
            ffi::opus_multistream_encoder_ctl(p, ffi::OPUS_SET_COMPLEXITY_REQUEST, complexity);
            ffi::opus_multistream_encoder_ctl(p, ffi::OPUS_GET_COMPLEXITY_REQUEST, &mut cplx as *mut i32);
            ffi::opus_multistream_encoder_ctl(p, ffi::OPUS_SET_BITRATE_REQUEST, bitrate);
            ffi::opus_multistream_encoder_ctl(p, ffi::OPUS_SET_SIGNAL_REQUEST, signal);
            ffi::opus_multistream_encoder_ctl(p, ffi::OPUS_GET_SIGNAL_REQUEST, &mut sig as *mut i32);
        }
        Ok((enc, cplx, sig))
    }

    /// Encodes one block of interleaved samples (`pcm.len() / nch` frames) into `out`.
    pub fn encode(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize, i32> {
        let frames = (pcm.len() / self.nch) as c_int;
        // SAFETY: `pcm` holds `frames * nch` samples, `out` is writable for its full length.
        let r = unsafe {
            ffi::opus_multistream_encode_float(
                self.st.as_ptr(),
                pcm.as_ptr(),
                frames,
                out.as_mut_ptr(),
                out.len().min(i32::MAX as usize) as i32,
            )
        };
        if r > 0 { Ok(r as usize) } else { Err(r) }
    }

    pub fn reset(&mut self) {
        // SAFETY: valid state; OPUS_RESET_STATE takes no argument.
        unsafe { ffi::opus_multistream_encoder_ctl(self.st.as_ptr(), ffi::OPUS_RESET_STATE) };
    }

    /// Total bits per second over all channels.
    pub fn set_bitrate(&mut self, bitrate: i32) {
        // SAFETY: valid state; the request takes one `opus_int32`.
        unsafe {
            ffi::opus_multistream_encoder_ctl(self.st.as_ptr(), ffi::OPUS_SET_BITRATE_REQUEST, bitrate)
        };
    }

    /// In-band FEC on/off and the expected packet loss in percent (0..=100), which also sets
    /// how much FEC the encoder spends. FEC only works for frames of 10 ms and longer.
    pub fn set_fec(&mut self, fec: bool, loss_percent: i32) {
        let p = self.st.as_ptr();
        // SAFETY: valid state; both requests take one `opus_int32`.
        unsafe {
            ffi::opus_multistream_encoder_ctl(p, ffi::OPUS_SET_INBAND_FEC_REQUEST, i32::from(fec));
            ffi::opus_multistream_encoder_ctl(
                p,
                ffi::OPUS_SET_PACKET_LOSS_PERC_REQUEST,
                loss_percent.clamp(0, 100),
            );
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_multistream_encoder_create, destroyed exactly once.
        unsafe { ffi::opus_multistream_encoder_destroy(self.st.as_ptr()) };
    }
}

/// Opus multistream decoder into interleaved `f32`.
pub struct Decoder {
    st: NonNull<ffi::OpusMSDecoder>,
    nch: usize,
}

// SAFETY: as for `Encoder`.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new(sr: i32, nch: i32) -> Result<Self, i32> {
        Self::with_coupling(sr, nch, false)
    }

    /// As [`Self::new`], for a stream made by [`Encoder::with_coupling`] with the same `coupled`.
    pub fn with_coupling(sr: i32, nch: i32, coupled: bool) -> Result<Self, i32> {
        check_channels(nch)?;
        let map = mapping(nch as usize);
        let (streams, pairs) = layout(nch, coupled);
        let mut err: c_int = 0;
        // SAFETY: `map` holds 256 entries (>= nch); `err` is a valid out pointer.
        let st = unsafe {
            ffi::opus_multistream_decoder_create(sr, nch, streams, pairs, map.as_ptr(), &mut err)
        };
        match NonNull::new(st) {
            Some(st) if err == ffi::OPUS_OK => Ok(Self { st, nch: nch as usize }),
            Some(st) => {
                // SAFETY: created above and not used afterwards.
                unsafe { ffi::opus_multistream_decoder_destroy(st.as_ptr()) };
                Err(err)
            }
            None => Err(err),
        }
    }

    /// Decodes into `out` (`out.len() / nch` frames); `None` runs packet loss concealment.
    pub fn decode(&mut self, data: Option<&[u8]>, out: &mut [f32]) -> Result<usize, i32> {
        self.decode_impl(data, out, 0)
    }

    /// Recovers the packet *before* `next` from `next`'s in-band FEC data into `out` (plain
    /// concealment when `next` carries none). `next` must then be decoded normally.
    pub fn decode_fec(&mut self, next: &[u8], out: &mut [f32]) -> Result<usize, i32> {
        self.decode_impl(Some(next), out, 1)
    }

    /// Resets the decoder state (e.g. across a stream discontinuity).
    pub fn reset(&mut self) {
        // SAFETY: valid state; OPUS_RESET_STATE takes no argument.
        unsafe { ffi::opus_multistream_decoder_ctl(self.st.as_ptr(), ffi::OPUS_RESET_STATE) };
    }

    fn decode_impl(&mut self, data: Option<&[u8]>, out: &mut [f32], fec: c_int) -> Result<usize, i32> {
        let (ptr, len) = match data {
            Some(d) => (d.as_ptr(), d.len().min(i32::MAX as usize) as i32),
            None => (std::ptr::null(), 0),
        };
        // SAFETY: `ptr` is null (PLC) or valid for `len` bytes; `out` holds `frames * nch`
        // samples, which is what libopus writes at most.
        let r = unsafe {
            ffi::opus_multistream_decode_float(
                self.st.as_ptr(),
                ptr,
                len,
                out.as_mut_ptr(),
                (out.len() / self.nch) as c_int,
                fec,
            )
        };
        if r >= 0 { Ok(r as usize) } else { Err(r) }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        // SAFETY: created by opus_multistream_decoder_create, destroyed exactly once.
        unsafe { ffi::opus_multistream_decoder_destroy(self.st.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec::{OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_SIGNAL_MUSIC};

    fn round_trip(nch: usize) {
        let (mut enc, cplx, sig) =
            Encoder::new(48000, nch as i32, OPUS_APPLICATION_RESTRICTED_LOWDELAY, 96_000 * nch as i32, 10, OPUS_SIGNAL_MUSIC)
                .unwrap();
        assert_eq!((cplx, sig), (10, OPUS_SIGNAL_MUSIC));
        let mut dec = Decoder::new(48000, nch as i32).unwrap();
        let mut packet = vec![0u8; 4000];
        let mut out = vec![0f32; 480 * nch];
        let mut energy = 0.0;
        for b in 0..20 {
            let pcm: Vec<f32> = (0..480 * nch)
                .map(|i| ((b * 480 + i / nch) as f32 * 0.05 * (1 + i % nch) as f32).sin() * 0.5)
                .collect();
            let n = enc.encode(&pcm, &mut packet).unwrap();
            assert_eq!(dec.decode(Some(&packet[..n]), &mut out).unwrap(), 480);
            energy += out.iter().map(|x| x * x).sum::<f32>();
        }
        assert!(energy > 1.0);
        assert_eq!(dec.decode(None, &mut out).unwrap(), 480);
        enc.reset();
    }

    #[test]
    fn mono_and_multistream() {
        round_trip(1);
        round_trip(4);
    }

    /// Coupled stereo is plain single-stream Opus: the TOC byte's stereo flag is set and a
    /// coupled decoder round-trips it, PLC and FEC included.
    #[test]
    fn coupled_stereo() {
        let (mut enc, ..) =
            Encoder::with_coupling(48000, 2, true, OPUS_APPLICATION_RESTRICTED_LOWDELAY, 96_000, 5, OPUS_SIGNAL_MUSIC)
                .unwrap();
        enc.set_fec(true, 10);
        enc.set_bitrate(64_000);
        let mut dec = Decoder::with_coupling(48000, 2, true).unwrap();
        let mut packet = vec![0u8; 4000];
        let mut out = vec![0f32; 960];
        let pcm: Vec<f32> = (0..960).map(|i| ((i / 2) as f32 * 0.05).sin() * 0.5).collect();
        let n = enc.encode(&pcm, &mut packet).unwrap();
        assert_ne!(packet[0] & 0x04, 0, "stereo TOC flag");
        assert_eq!(dec.decode(Some(&packet[..n]), &mut out).unwrap(), 480);
        assert_eq!(dec.decode_fec(&packet[..n], &mut out).unwrap(), 480);
        dec.reset();
        assert_eq!(dec.decode(None, &mut out).unwrap(), 480);
    }

    #[test]
    fn bad_args() {
        assert!(Decoder::new(44100, 2).is_err());
        assert!(Decoder::new(48000, 0).is_err());
        assert!(Encoder::new(48000, 256, OPUS_APPLICATION_RESTRICTED_LOWDELAY, 1, 1, 1).is_err());
    }
}
