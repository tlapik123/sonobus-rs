# Open issues

State on 2026-11-22. Decisions behind the "not planned" items are in `docs/adr/`.

## Never verified outside the development sandbox

- The Android APK ran on one Android 11 phone: it starts, audio works with the default devices and it joins a group on `aoo.sonobus.net`. The rest of the checklist in `android/README.md`, including sound sharing, was not run.
- Real audio devices (tests use simulated ones).
- A real stock SonoBus client talking to ours. Interop is tested against the upstream AOO C++ code only.
- Any real network: all tests run on one machine; n0 relays were never contacted.
- The plugin in a real DAW (it passes clap-validator, pluginval and Steinberg's validator).
- Windows and macOS: CI builds and runs the tests on both (first green run on all three systems: PR 1, commit 951cd51). That covers compiling, linking and the simulated-device tests only: nothing was run on a real Windows or macOS machine with real audio devices, the app window was never opened there, and the plugin was not loaded in a host. The Windows file-URL handling and Ctrl-C shutdown of the server have no test.
- On macOS only 127.0.0.1 is a loopback address, so seven network tests that need distinct source addresses (per-address caps, back-off, PIN lockout) run only on Linux and Windows.
- On the macOS CI runner `thread::sleep` lasts several times longer than asked; the engine test loop that stands in for the audio device spins there instead. Why the runner oversleeps is not known.
- The engine tests are not reliable on the macOS CI runner: with identical code one run passed and the next failed three wall-clock tests (the test audio loop fell behind real time: 45 instead of more than 50 repaired blocks in 3 s, 325 instead of more than 450 encodes a second, a 6.4 ms sync reading). Those three thresholds are lower on macOS now; other wall-clock tests may fail there the same way. The Windows runner did the same once (48 repaired blocks, and `engine_drop_finalizes_a_recording` got 28 of 40 blocks because the recorder started late): the repaired-blocks threshold is now 25 everywhere and the recording test counts from the first recorded block.
- `password_joined_listener_syncs` checks its timing bound on Linux only: the Windows runner reads 5.5-8.4 ms without any join, too close to the 10 ms a restart would show.
- The DSP reference tests allow 1e-5 on Linux x86_64, where the reference vectors were made, and 2e-4 elsewhere: macOS arm64 was up to 5.2e-5 off (compressor meter, extreme EQ).
- Synchronised playout ("Sync listeners") on real devices: tests run host and listeners in one process, and output latency is assumed to be one block, so phones with different output latency will be off by that difference.
- Per-application capture with real desktop applications (browsers, Flatpak, pipewire-pulse clients). It was run against a headless PipeWire with `pw-play` only. An application that keeps its stream but stops delivering audio is covered by a unit test only.
- The translations of the session-ended messages (11 languages) were written without a native speaker.
- The "App sound" picker and the "Sync listeners" controls in a real window (headless UI tests only).

## Not built yet

- Synchronised playout in a Secure mesh session (it exists for broadcast sessions only).
- Per-application sound capture on Windows and macOS (Linux has it through PipeWire).
- Signed Windows and macOS builds, installers and app bundles (CI builds and tests only); the standalone macOS app has no microphone usage description (ADR 0006).

## Not planned

- LV2, AU and AAX plugin formats (ADR 0005).
- An in-app QR scanner on Android (ADR 0007).
- A browser listener (ADR 0002).

## Known defects and gaps

- Key rotation and member removal (REKEY carrying the removed device, `Net::evict`) were reviewed once more, by reading only. The transport held on every path traced. The review found that removing a member whose connection had just been replaced became a plain key change which that device received too; this is fixed (nothing changes and the app says nobody was removed). Left open from that review:
  - A member that joined with the key or an invite while the host changes the key (its WELCOME written before the change) gets no REKEY: the host drops it after 5 s and it is told it was removed. Members that joined by password through it go with it. Password joins in the same window do get the new key. The host side of this window has no test.
  - REKEY carries no counter. With two overlapping connections between a member and its key authority, an older REKEY read after a newer one would roll that member back; it then fails the key proof and is dropped.
  - A removed device that joined by name stays registered at the rendezvous. Until the host changes the password it can answer new name joiners with its own old key, and they land in its group, not in the session.
  - How soon audio stops going to a device blocked at the transport only was not measured (expected under 1 s).
- `legacy_and_modern_sessions_alternate_without_leaks` (`crates/sonobus-engine/tests/modern.rs`) failed once in four full workspace runs with two file descriptors still open half a second after teardown. It did not fail again in 50 runs under CPU load, so the cause is not known. The test now waits up to 5 s for the counts to return and prints the open descriptors when they do not.
- A member that learns of a removal only through the key change does not list that device under "Blocked devices".
- Three fixes are tested at unit level only, not through a live session: the shared-stream loss estimate, the subscription rebuilt after a format change, and the baseline reset on a path change.
- `congested_uplink_lowers_the_send_rate` went around its bottleneck whenever the test's relay thread was slow: iroh moved the connection to the direct loopback path (4 of 8 runs with 64 busy processes). Host and joiner now listen on different address families, so the bottleneck is the only path. It is still skipped on Windows and macOS: the new setup was not tried there.
- `nack_repairs_bursts_bit_exact` (`crates/sonobus-net/tests/e2e.rs`) failed once under load and not again in 110 runs alone and 16 runs of its whole test binary, with up to 64 busy processes. Cause unknown.
- With 48 busy processes two more wall-clock tests in `crates/sonobus-engine/tests/modern.rs` fail (`induced_loss_is_repaired_and_adaptation_reacts` 3 of 10, `patchbay_and_peer_cache_in_a_modern_session` 2 of 10). Not looked into.
- A joiner learns that a session is a broadcast from one trailing byte in the host's WELCOME and then behaves like an invite listener. The byte is protected by the connection's TLS, not by the WELCOME MAC; only the endpoint we dialed can leave it out or add it, and adding it only restricts the joiner. Limits: an older joiner ignores the byte and stays a mesh member that serves password joins (the leak between listeners remains for it), and an older host sends none; in such a mixed session a second password connect to an old listener can replace a new listener's key authority. PIN joins and a LAN join aimed at the host have no engine-level test, and real LAN discovery was not run.
- A join refused by a broadcast listener shows iroh's "the server refused to accept a new connection" text. A joiner that loses the race for the last free slot sees "the host left", not "session full".
- Synchronised playout only ever adds latency (timed start, silence when early, slowing down, going late when arriving blocks get less headroom than the jitter buffer needs); it never skips or speeds up. Consequences:
  - A listener that fell behind stays behind and shows it as its sync error. It returns to the target only when its buffering restarts: at its next underrun, when the host lowers the delay (one gap), or when sync is switched off and on. After a latency step and recovery it stays 6-86 ms late depending on format and buffer; after a 1 s outage about 230 ms late.
  - Sender and listener clocks 200 ppm apart leave up to about 1.8 ms of late error; with a slower sender it is permanent.
  - Going late can add latency (and one splice) where unsynced playout would have played on, mainly with a device callback that is large against the jitter buffer.
  - With device callbacks delivered in pairs the reading can say "on target" while the audio is 4-17 ms late.
  - In a few simulated cases (44.1 kHz with heavy-tailed jitter, 48 kHz / block 64 / device 256 / 10 ms buffer with spikes) a synced listener is 18-29 ms less buffered than an unsynced one and loses one more block, because unsynced playout used to pad its latency through false underruns (measured before the unsynced rework below; not re-measured).
  - Seven review rounds were needed; six designs that also removed latency or changed unsynced playout each failed the next review. This version passed on "unsynced playout is unchanged" and "nothing removes latency"; its last small fix (restart only when the host lowers the delay) was checked by the author's tests only. The simulation matrix (`crates/sonobus-net/tests/sync_playout.rs`) stops comparing latency in about half its cells once unsynced playout has underrun twice more than synced.
- Unsynced Secure playout was reworked (false underruns with a full ring are gone; a missing block is concealed only when the audio side is about to need it; a block that comes late raises the buffer). Measured in simulation only, against the previous version. It is much better in common setups (48 kHz, device callback 128-256, manual buffer: 2-6x fewer audible gaps), but worse in these:
  - The raise (at most twice the concealment margin: about 16 ms at 48 kHz / 120 / 128, 48 ms at 128 / 512, 58 ms at 44.1 kHz / 256 / 512, plus the round trip while resends are on) stays until the next underrun, buffer change or flush. It shows in the reported buffer, so a manual 20 ms setting can read 35 ms, and each raise counts as one dropped block.
  - With a device callback of 512 or more the ring keeps the backlog of a Wi-Fi-like stall: latency after 150-200 ms stalls stays 37-68 ms higher than before (for example 91 to 129 ms at 48 kHz / 128 / 512 / 20 ms).
  - After a dropout, audio resumes 35-80 ms later than before (the restart is followed by one raise).
  - In automatic buffer mode on a link with steady reordering the controller lowers the buffer while raises hide the drops: about 25 audible events in 10 minutes instead of 10, until it settles after about 90 s.
  - With 100 ms stalls every 5 s at 48 kHz / 120 / 128 / 20 ms: about 85 underruns in 10 minutes instead of 36.
  - Small formats (64-frame callbacks, 5-10 ms buffers) lose more under heavy-tailed jitter or a recurring lateness larger than the cap; the previous version ran 20-80 ms later by accident.
  - Resends at 44.1 kHz / 512 / 20 ms with a 10 ms round trip lose 4-13 blocks a minute at 1 % loss (before: 0-1, at about 100 ms instead of 70-85 ms).
  - A mechanism that gave the raise back after a clean minute was removed: it added audible gaps on links that hiccup every 70-300 s.
- An application that is listed but can never be opened for capture makes the audio devices reopen once every 10 s (one short glitch each time).
- Synchronised playout uses an NTP-style clock offset: a path whose delay differs by direction shifts a listener by half the difference.
- Per-application capture hides SonoBus's own streams by node name, not process id, so other running SonoBus instances are hidden too and its streams on a PulseAudio or JACK host may not be.
- If the captured application is the only stream driving the engine (no output device), losing it stops the session until it is back.
- System sound and app sound cannot be on together.
- On Windows the server key file and the app's settings files get no access restriction.
- `cargo fmt --check` reports differences across the workspace; fmt has never been run.

## Housekeeping

- CI passes on Linux, Windows and macOS; some timing and per-address tests run on Linux only (see "Never verified").
- Building the desktop app on Linux now needs libpipewire and libclang; `devenv.nix` has them, the dev shell must be re-entered.
