# Open issues

State on 2026-11-19. Decisions behind the "not planned" items are in `docs/adr/`.

## Never verified outside the development sandbox

- The Android APK ran on one Android 11 phone: it starts, audio works with the default devices and it joins a group on `aoo.sonobus.net`. The rest of the checklist in `android/README.md`, including sound sharing, was not run.
- Real audio devices (tests use simulated ones).
- A real stock SonoBus client talking to ours. Interop is tested against the upstream AOO C++ code only.
- Any real network: all tests run on one machine; n0 relays were never contacted.
- The plugin in a real DAW (it passes clap-validator, pluginval and Steinberg's validator).

## Not built yet

- Synchronised playout across listeners in a Secure session.
- Per-application sound capture on desktop.
- Windows and macOS builds (ADR 0006).

## Not planned

- LV2, AU and AAX plugin formats (ADR 0005).
- An in-app QR scanner on Android (ADR 0007).
- A browser listener (ADR 0002).

## Known defects and gaps

- The last change to key rotation and member removal (REKEY carrying the removed device, `Net::evict`) has not been independently reviewed. The two rounds before it each had a hole found by the next review.
- One timing gap in joining (a key change between WELCOME being written and the connection being registered) is closed by reasoning only; it has no test.
- `legacy_and_modern_sessions_alternate_without_leaks` (`crates/sonobus-engine/tests/modern.rs`) failed once in four full workspace runs with two file descriptors still open half a second after teardown. It passes alone, also under CPU load. Cause unknown.
- A member that learns of a removal only through the key change does not list that device under "Blocked devices".
- The four session-ended messages are English only.
- Three fixes are tested at unit level only, not through a live session: the shared-stream loss estimate, the subscription rebuilt after a format change, and the baseline reset on a path change.
- `cargo fmt --check` reports differences across the workspace; fmt has never been run.

## Housekeeping

- No README at the repository root.
- No CI.
