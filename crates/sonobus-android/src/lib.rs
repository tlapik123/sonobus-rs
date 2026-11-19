//! The SonoBus Android app: [`sonobus_app`]'s controller and egui UI inside a GameActivity,
//! plus Android playback capture ("System sound" / one app's sound) as an engine input.
//!
//! The Kotlin shell (`android/app/src/main/java/io/github/sonobusrs/`) owns everything only
//! Java can do: consent, the foreground service, `AudioRecord` with an
//! `AudioPlaybackCaptureConfiguration`, locks, insets, the app list. It talks to this crate
//! through `NativeBridge` (JNI both ways, see `android.rs`).
//!
//! Lifetime: the controller (engine, audio, hosted server) lives for the process, which the
//! foreground service keeps alive; the egui surface comes and goes with the activity.

pub mod capture;

#[cfg(target_os = "android")]
mod android;
