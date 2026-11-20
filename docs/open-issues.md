# Open issues

State on 2026-11-20. Decisions behind the "not planned" items are in `docs/adr/`.

## Never verified outside the development sandbox

- The Android APK ran on one Android 11 phone: it starts, audio works with the default devices and it joins a group on `aoo.sonobus.net`. The rest of the checklist in `android/README.md`, including sound sharing, was not run.
- Real audio devices (tests use simulated ones).
- A real stock SonoBus client talking to ours. Interop is tested against the upstream AOO C++ code only.
- Any real network: all tests run on one machine; n0 relays were never contacted.
- The plugin in a real DAW (it passes clap-validator, pluginval and Steinberg's validator).
- Windows and macOS: `.github/workflows/ci.yml` builds and tests there, but it has never run. Locally the Rust code only type-checks for those targets; nothing was linked and no C code (libopus, vorbis) was built. The Windows file-URL handling and Ctrl-C shutdown of the server are untested.
- Synchronised playout ("Sync listeners") on real devices: tests run host and listeners in one process, and output latency is assumed to be one block, so phones with different output latency will be off by that difference.
- Per-application capture with real desktop applications (browsers, Flatpak, pipewire-pulse clients). It was run against a headless PipeWire with `pw-play` only. An application that keeps its stream but stops delivering audio is covered by a unit test only.
- The "App sound" picker and the "Sync listeners" controls in a real window (headless UI tests only).

## Not built yet

- Synchronised playout in a Secure mesh session (it exists for broadcast sessions only).
- Per-application sound capture on Windows and macOS (Linux has it through PipeWire).
- Signed Windows and macOS builds, installers and app bundles; the standalone macOS app has no microphone usage description (ADR 0006).

## Not planned

- LV2, AU and AAX plugin formats (ADR 0005).
- An in-app QR scanner on Android (ADR 0007).
- A browser listener (ADR 0002).

## Known defects and gaps

- The last change to key rotation and member removal (REKEY carrying the removed device, `Net::evict`) has not been independently reviewed. The two rounds before it each had a hole found by the next review.
- One timing gap in joining (a key change between WELCOME being written and the connection being registered) is closed by reasoning only; it has no test.
- `legacy_and_modern_sessions_alternate_without_leaks` (`crates/sonobus-engine/tests/modern.rs`) failed once in four full workspace runs with two file descriptors still open half a second after teardown. It passes alone, also under CPU load. Cause unknown.
- A member that learns of a removal only through the key change does not list that device under "Blocked devices".
- Three fixes are tested at unit level only, not through a live session: the shared-stream loss estimate, the subscription rebuilt after a format change, and the baseline reset on a path change.
- `congested_uplink_lowers_the_send_rate` (`crates/sonobus-engine/tests/modern_hardening.rs`) failed once in a full workspace run and passes alone; `nack_repairs_bursts_bit_exact` (`crates/sonobus-net/tests/e2e.rs`) did the same once. Both are wall-clock tests that fail under load.
- The last round of synchronised-playout fixes (slack capped at ring fill, 16 outstanding pings with back-off, PLAYOUT accepted only from a sole peer, ring size reset on sync off) has not been independently reviewed. The two rounds before it each had a hole found by the next review.
- A member that joined by password, PIN, LAN or name cannot tell a broadcast from a mesh, so it accepts a playout-sync frame from the peer it got the key from while that is its only peer (a two-member mesh, or before its other dials complete). A modified peer there can delay playout of its own stream by at most 1 s and make the member ping it; nobody else's audio is affected.
- An application that is listed but can never be opened for capture makes the audio devices reopen once every 10 s (one short glitch each time).
- Synchronised playout uses an NTP-style clock offset: a path whose delay differs by direction shifts a listener by half the difference.
- Per-application capture hides SonoBus's own streams by node name, not process id, so other running SonoBus instances are hidden too and its streams on a PulseAudio or JACK host may not be.
- If the captured application is the only stream driving the engine (no output device), losing it stops the session until it is back.
- System sound and app sound cannot be on together.
- On Windows the server key file and the app's settings files get no access restriction.
- `cargo fmt --check` reports differences across the workspace; fmt has never been run.

## Housekeeping

- CI exists but has never run (see above).
- Building the desktop app on Linux now needs libpipewire and libclang; `devenv.nix` has them, the dev shell must be re-entered.
