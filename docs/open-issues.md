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
- The translations of the session-ended messages (11 languages) were written without a native speaker.
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
- Broadcast isolation does not hold for listeners that join by password, PIN, LAN or name: they run as mesh members with their own send stream, and a later joiner that connects through one of them exchanges microphone audio with it directly; the hub never sees that link. Found by reading the code (`crates/sonobus-engine/src/modern/mod.rs`, `crates/sonobus-net/src/rt.rs`), not run. Only invite listeners are covered by the isolation test.
- Synchronised playout only ever adds latency (timed start, silence when early, slowing down, going late when arriving blocks get less headroom than the jitter buffer needs); it never skips or speeds up. Consequences:
  - A listener that fell behind stays behind and shows it as its sync error. It returns to the target only when its buffering restarts: at its next underrun, when the host lowers the delay (one gap), or when sync is switched off and on. After a latency step and recovery it stays 6-86 ms late depending on format and buffer; after a 1 s outage about 230 ms late.
  - Sender and listener clocks 200 ppm apart leave up to about 1.8 ms of late error; with a slower sender it is permanent.
  - Going late can add latency (and one splice) where unsynced playout would have played on, mainly with a device callback that is large against the jitter buffer.
  - With device callbacks delivered in pairs the reading can say "on target" while the audio is 4-17 ms late.
  - In a few simulated cases (44.1 kHz with heavy-tailed jitter, 48 kHz / block 64 / device 256 / 10 ms buffer with spikes) a synced listener is 18-29 ms less buffered than an unsynced one and loses one more block, because unsynced playout pads its latency through the false underruns described next.
  - Seven review rounds were needed; six designs that also removed latency or changed unsynced playout each failed the next review. This version passed on "unsynced playout is unchanged" and "nothing removes latency"; its last small fix (restart only when the host lowers the delay) was checked by the author's tests only. The simulation matrix (`crates/sonobus-net/tests/sync_playout.rs`) stops comparing latency in about half its cells once unsynced playout has underrun twice more than synced.
- Unsynced Secure playout sets the resampler's ratio after it has filled the resampler, so when the sender's rate estimate rises between callbacks a callback can come up a frame short while the ring is full: a false underrun, one callback of dropout and one callback of added latency (seen at 44.1 kHz and with fast-clock senders). Those false underruns hide a second issue: `drain` conceals a hole once the ring is down to a block plus two device callbacks, which with a device callback large against the jitter target (96 kHz / 1024, or 48 kHz / 128 / 512 with a 20 ms buffer) is above the jitter buffer. With the first fixed alone, nearly every reordered block is concealed (25-62 % at 8 ms jitter), so the two must be fixed together, with a prefill floor that keeps legacy latency parity and does not assume a tick per device callback. Synced streams order the update first and keep more headroom.
- Unsynced Secure playout with resend requests on, 3 % loss and a latency step loses heavily at 96 kHz (about 20,000 blocks in 60 s in the simulation).
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
