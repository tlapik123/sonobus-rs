# Android shares device sound as an input; no in-app QR scanner

The point of the Android app is low-latency sound sharing: system or app playback is selectable as an *input*, so a phone can host a session and stream what it plays to several listeners. Capture uses MediaProjection with `AudioPlaybackCaptureConfiguration` in a Kotlin foreground service, handed to the Rust engine over JNI. This sets the minimum to Android 10 (SDK 29), and apps that opt out of capture stay silent.

The app has **no QR scanner of its own**. Any camera or scanner app can read the code; we only register for `sonobus2://join` links so that scanning opens our app.

## Consequences

- The link handler is the one thing that must keep working: the invite QR must always encode a `sonobus2://join/...` link, and the manifest must keep its intent filter.
