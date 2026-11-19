//! Playback-capture state as Kotlin reports it, and the PCM hand-off into the engine input.
//! JNI-free so it is tested on the desktop.

use sonobus_app::audio::{ExternalSlot, ExternalSpec, push_external};
use sonobus_app::platform::{AppInfo, CaptureState, CaptureStatus, CaptureTarget};

/// What `PlaybackCapture.kt` / `AudioService.kt` report (the `state` codes of
/// `NativeBridge.onCaptureState`).
#[derive(Debug, Clone, PartialEq)]
pub enum CaptureEvent {
    /// The consent dialog is being shown for `target`.
    Requested(CaptureTarget),
    /// `AudioRecord` started.
    Started { sample_rate: u32, channels: u16, buffer_frames: u32 },
    /// Consent declined, projection stopped by the system/user, or `AudioRecord` failed.
    Stopped(String),
    /// Stopped on our request.
    Off,
    Latency(f32),
}

/// Shared between the JNI callbacks and the UI.
#[derive(Debug, Default)]
pub struct CaptureModel {
    pub status: CaptureStatus,
    pub apps: Vec<AppInfo>,
}

impl CaptureModel {
    pub fn on_event(&mut self, e: CaptureEvent) {
        let s = &mut self.status;
        match e {
            CaptureEvent::Requested(t) => {
                *s = CaptureStatus { state: CaptureState::NeedsConsent, target: Some(t), ..CaptureStatus::default() };
            }
            CaptureEvent::Started { sample_rate, channels, buffer_frames } => {
                s.state = CaptureState::Running;
                s.format = Some(ExternalSpec { channels, sample_rate });
                s.buffer_frames = buffer_frames;
            }
            // a stop of something not running (late callback after "Microphone only") stays off
            CaptureEvent::Stopped(why) if s.state != CaptureState::Off => {
                s.state = CaptureState::Stopped(why);
                s.latency_ms = None;
            }
            CaptureEvent::Stopped(_) => {}
            CaptureEvent::Off => {
                s.state = CaptureState::Off;
                s.latency_ms = None;
            }
            CaptureEvent::Latency(ms) => s.latency_ms = (s.state == CaptureState::Running).then_some(ms),
        }
    }
}

/// Native-endian f32 PCM bytes from a direct `ByteBuffer` into the engine's platform input.
/// No allocation; a misaligned buffer is converted through a small stack buffer.
pub fn push_pcm_f32(slot: &ExternalSlot, bytes: &[u8]) -> usize {
    // SAFETY: every bit pattern is a valid f32; `align_to` only yields the aligned middle part.
    let (head, mid, _) = unsafe { bytes.align_to::<f32>() };
    if head.is_empty() {
        return push_external(slot, mid);
    }
    let mut tmp = [0.0f32; 240]; // whole frames for 1..=6 and 8 channels
    let mut taken = 0;
    for chunk in bytes.chunks(tmp.len() * 4) {
        let n = chunk.len() / 4;
        for (d, s) in tmp.iter_mut().zip(chunk.as_chunks::<4>().0) {
            *d = f32::from_ne_bytes(*s);
        }
        taken += push_external(slot, &tmp[..n]);
    }
    taken
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_lifecycle() {
        let mut m = CaptureModel::default();
        m.on_event(CaptureEvent::Requested(CaptureTarget::System));
        assert_eq!(m.status.state, CaptureState::NeedsConsent);
        m.on_event(CaptureEvent::Latency(12.0)); // not running yet: ignored
        assert_eq!(m.status.latency_ms, None);
        m.on_event(CaptureEvent::Started { sample_rate: 48000, channels: 2, buffer_frames: 480 });
        assert_eq!(m.status.state, CaptureState::Running);
        assert_eq!(m.status.format, Some(ExternalSpec { channels: 2, sample_rate: 48000 }));
        m.on_event(CaptureEvent::Latency(12.0));
        assert_eq!(m.status.latency_ms, Some(12.0));
        // revoked from the status bar: stopped, target kept for "Capture again"
        m.on_event(CaptureEvent::Stopped("revoked".into()));
        assert_eq!(m.status.state, CaptureState::Stopped("revoked".into()));
        assert_eq!(m.status.target, Some(CaptureTarget::System));
        assert_eq!(m.status.latency_ms, None);
        // a new request clears the old format until the new recorder reports
        let app = CaptureTarget::App { uid: 10123, label: "Player".into() };
        m.on_event(CaptureEvent::Requested(app.clone()));
        assert_eq!((m.status.format, m.status.target.clone()), (None, Some(app)));
        // user picks "Microphone only"; a late stop callback does not resurrect it
        m.on_event(CaptureEvent::Off);
        m.on_event(CaptureEvent::Stopped("projection stopped".into()));
        assert_eq!(m.status.state, CaptureState::Off);
    }

    #[test]
    fn pcm_bytes_reach_the_input_aligned_or_not() {
        use sonobus_audio::Duplex;
        let slot = ExternalSlot::default();
        assert_eq!(push_pcm_f32(&slot, &[0; 16]), 0); // nothing open yet
        let mut d = Duplex::new(|_: &mut sonobus_audio::AudioBlock<'_>| {}, 48000, 64, 2);
        *slot.lock().unwrap() = Some(d.add_input(2, 48000, 48));
        let samples: Vec<f32> = (0..200).map(|i| i as f32 / 200.0).collect();
        let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_ne_bytes()).collect();
        assert_eq!(push_pcm_f32(&slot, &bytes), 100);
        // misaligned view of the same samples (one leading pad byte), trailing partial frame
        let mut padded = vec![0u8; 1];
        padded.extend_from_slice(&bytes);
        assert_eq!(push_pcm_f32(&slot, &padded[1..padded.len() - 2]), 99);
        let st = slot.lock().unwrap().as_ref().unwrap().monitor().stats();
        assert_eq!((st.pushes, st.overflow_frames), (2, 0));
    }
}
