//! What only some platforms can do, behind one trait the UI asks: Android playback capture
//! ("System sound" / "App sound" as an input). The desktop app has no [`Platform`]; the Android
//! app implements it over JNI. Capture audio itself never passes through here: the platform
//! pushes it into [`crate::controller::AppController::external_input`].

use crate::audio::ExternalSpec;

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureTarget {
    /// Everything other apps play with media, game or unknown usage.
    System,
    /// One app's playback, by Linux UID.
    App { uid: i32, label: String },
}

#[derive(Debug, Clone, PartialEq, Default)]
pub enum CaptureState {
    #[default]
    Off,
    /// The system consent dialog is up (or about to be).
    NeedsConsent,
    Running,
    /// The user declined, the projection was revoked, the screen locked, another app took
    /// the projection, or recording failed; a new start asks for consent again.
    Stopped(String),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct CaptureStatus {
    pub state: CaptureState,
    pub target: Option<CaptureTarget>,
    /// The `AudioRecord` format once running.
    pub format: Option<ExternalSpec>,
    /// Frames per read.
    pub buffer_frames: u32,
    /// Capture latency: age of the newest frame when it reached us (from the `AudioRecord`
    /// timestamp), when the device reports timestamps.
    pub latency_ms: Option<f32>,
}

/// A launchable app the user can pick for "App sound".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppInfo {
    pub label: String,
    pub package: String,
    pub uid: i32,
}

pub trait Platform: Send {
    fn capture(&self) -> CaptureStatus;
    /// Launchable apps (empty until [`Platform::load_apps`] finished).
    fn apps(&self) -> Vec<AppInfo>;
    fn load_apps(&self);
    /// Asks for consent if needed, then starts capturing `target`.
    fn start_capture(&self, target: CaptureTarget);
    fn stop_capture(&self);
}
