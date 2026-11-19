# SonoBus for Android

The Rust app (`crates/sonobus-android`: engine, egui UI, hosted server) inside a small Kotlin
shell (this Gradle project). The headline feature: share what the phone plays ("System sound",
or one app's sound) with everyone in a session, and host that session on the phone itself.

| Piece | Where |
|---|---|
| `android_main`, egui surface (safe area, soft-keyboard text), JNI | `crates/sonobus-android/src/android.rs` |
| Capture state, PCM hand-off into the engine input | `crates/sonobus-android/src/capture.rs` |
| `MainActivity` (GameActivity): permissions, consent dialog, insets, Back | `app/src/main/java/io/github/sonobusrs/MainActivity.kt` |
| `AudioService`: foreground service, MediaProjection, notification + Stop, locks, network callback | `AudioService.kt` |
| `PlaybackCapture`: `AudioRecord` with `AudioPlaybackCaptureConfiguration`, read thread | `PlaybackCapture.kt` |
| `NativeBridge`: JNI declarations both ways | `NativeBridge.kt` |

Versions: compile/target SDK 36 (Play requirement since 2026-08-31), min SDK 29 (playback
capture), NDK r30 (30.0.16248370, LTS), build-tools 36.1.0, CMake 4.1.2 (libopus is built from
source), AGP 9.5.1 with its built-in Kotlin, Gradle 9.6.0 (wrapper), `androidx.games:games-activity`
4.4.0 (the version `android-activity` 0.6.1 supports; its prefab C++ glue is NOT enabled),
cargo-ndk 4.1.2, JDK 17+.

## Build

```sh
./android/setup-toolchain.sh            # once: Rust targets, cargo-ndk, SDK/NDK in ~/android-sdk
export ANDROID_HOME=~/android-sdk
cd android
mise exec -- ./gradlew assembleDebug    # `mise exec --` only puts cargo on PATH; plain ./gradlew works if cargo is there
```

The APK is `android/app/build/outputs/apk/debug/app-debug.apk` (debug-signed). Gradle runs
`cargo ndk ... build --release -p sonobus-android` first (task `cargoNdk`; the Rust side is always
optimised, a debug engine underruns). ABIs: `arm64-v8a` by default; for an x86_64 emulator add
`-Psonobus.abis=arm64-v8a,x86_64`.

Tests off-device: `./gradlew testDebugUnitTest` (Kotlin: PCM framing, capture latency, app
list), `cargo test -p sonobus-android -p sonobus-app` (capture state machine, PCM hand-off, and a
two-controller test where a fake capture source reaches a peer).

## Install

```sh
adb install -r android/app/build/outputs/apk/debug/app-debug.apk
adb logcat -s sonobus AndroidRuntime     # Rust logs under the "sonobus" tag
```

## How it behaves

- Starting SonoBus starts a foreground service ("Audio session running" notification). The
  engine, network and a hosted session keep running with the app in the background or the
  screen off. **Stop** in the notification quits SonoBus. Back moves it to the background;
  swiping it away from Recents quits it (the UI cannot be recreated in the same process).
- Setup Audio > *Share this phone's sound* > Input source: *Microphone only*, *System sound*, or
  *App sound...* (pick an app). Android then shows its "start recording or casting" consent; on
  Android 14+ the dialog offers the entire screen only (single-app sharing would not cover system
  sound). Only sound is used. Each start asks again (Android 14+ allows one capture per consent).
- What is sent: the captured stereo as an extra "System Sound" input group (after the mic),
  stereo send, not monitored locally. SonoBus' own output (the peers) is never captured: the
  manifest sets `allowAudioPlaybackCapture="false"` and system capture also excludes our uid.
- Not capturable: apps that opt out (`allowAudioPlaybackCapture="false"` or a capture policy,
  e.g. many streaming/banking apps), voice calls, and anything not using media/game/unknown
  usage. The setup page says so.
- The capture state is shown next to the choice: capturing (rate, channels, buffer, measured
  capture latency from the `AudioRecord` timestamp), waiting for permission, or stopped (revoked,
  screen locked on some versions, another app started sharing, recorder failed) with *Capture
  again*.

- Secure sessions (Connect > *Secure (this app only)*): the headline flow is *Host a secure
  session* > *Share sound* on the phone, *Show QR Code*, and the others scan it. The app has no
  QR scanner of its own (no camera pipeline): scanning the code with the system camera app opens
  the `sonobus2://join/...` link, which reaches SonoBus through the manifest's intent filter
  (`MainActivity.openLink` -> `NativeBridge.onOpenUrl`); SonoBus then shows the session name,
  the host's device id and relay, and asks before joining. *Join* > *Invite link* takes a pasted
  link too.

## Debugging on a phone

```sh
adb logcat -c                                          # clear old logs
adb logcat -s sonobus AndroidRuntime                   # live: Rust logs + Java crashes
adb logcat -d -v threadtime > .locki/tmp/logcat.txt    # full dump after reproducing
adb shell getprop ro.build.version.release             # Android version
adb shell getprop ro.product.model                     # phone model
# can the phone reach a connection server at all? (bypasses the app)
adb shell 'for p in 10998 10999; do echo "port $p:"; nc -w 5 aoo.sonobus.net $p </dev/null; echo "exit=$?"; done'
```

The public server accepts TCP on 10998, refuses 10999 and does not answer ping. A failed
connect shows the address it tried: `Connect failed: <ip>:<port>: ...`.

Found on the first phone run (Android 11, 2026-11-19), both fixed:

- `audio device error: device aaudio:-1 not found`: cpal's AAudio default device has the id
  `aaudio:-1` but is not in `devices()`, so the lookup by id missed it. `find()` in
  `crates/sonobus-audio/src/device.rs` now falls back to the host's default devices.
- `Connect failed: connection refused (os error 111)` to `aoo.sonobus.net`: host and port were
  separate fields and a stale 10999 stayed next to the new host. The connect page now has one
  `host[:port]` field as upstream, port 10998 unless given.

## Manual test checklist (real phone)

Run so far, on one Android 11 phone: install and start, audio with the default devices, joining
a group on `aoo.sonobus.net`. Nothing else below was run on a device yet.

Setup: phone with Android 10+ and a desktop running `sonobus` on the same Wi-Fi.

1. **Install and start.** `adb install`, open SonoBus. Expect: microphone and notification
   permission prompts; the notification "Audio session running" appears; the UI fits between
   status bar, camera cutout and navigation bar (rotate the phone too). No crash in
   `adb logcat -s sonobus AndroidRuntime`.
2. **Typing.** Tap the user name field: the soft keyboard opens, typed text appears, backspace
   works, the field stays visible above the keyboard. Repeat for server, group, password and chat.
3. **Host on the phone.** Connect > Host: start hosting. Note the address and port shown.
4. **Join from the desktop.** On the desktop connect to that address/port, same group. Both list
   each other; talk into the phone mic, the desktop hears it.
5. **System sound.** Setup Audio > Input source > System sound. Accept the Android dialog.
   Expect "Capturing (48000 Hz, 2 ch, 240 frames) | capture latency N ms" and a "System Sound"
   group in the mixer. Play music in another app (e.g. a music player or a YouTube video in the
   browser): the desktop hears it in stereo; the phone does not play it twice; the desktop's
   voice (played on the phone) does not come back to the desktop (no echo/feedback).
6. **Latency.** Play a click track / clap recording on the phone while recording the phone
   speaker and the desktop output with one microphone (or watch both meters): note the end-to-end
   delay and the capture latency shown. Record numbers with the phone model.
7. **Lock screen.** Lock the phone for a minute. Expect audio to continue both ways; if Android
   stops the projection, the setup page shows "Capture stopped: ..." with *Capture again*.
8. **Switch apps.** Use other apps for a few minutes, come back: the UI reattaches with the
   session still running and meters moving.
9. **Revoke.** Pull down quick settings / status bar and stop the screen-sharing (cast) chip.
   Expect "Capture stopped" in SonoBus, the session continues with the mic only.
10. **App sound.** Input source > App sound... > pick the music app. Accept. Only that app is
    heard on the desktop (play a notification sound / another app: not sent).
11. **Opted-out app.** Pick an app that blocks capture (e.g. Netflix or a banking app): silence
    on the desktop, no crash.
12. **Phone hotspot.** Turn on the phone's hotspot, join it from the laptop, host on the phone,
    connect the desktop to the hotspot address. Repeat step 5.
13. **Stop.** Notification > Stop: SonoBus quits, the notification disappears; reopen: settings
    (user name, recents) are kept.

Report for each step: phone model, Android version, pass/fail, logcat excerpt for failures.
13. **Secure session by QR.** Connect > Secure > Host a secure session > Share sound, untick
    *Advertise on the local network*, Host. *Show QR Code*. Scan it with a second phone's camera
    app: SonoBus opens there and asks "A link asks to join a secure session" with the same
    session name and host id. Join: it plays the first phone's sound; the status bar shows the
    lock and "Secure session", the role "Listener"; no send controls on the listener.
