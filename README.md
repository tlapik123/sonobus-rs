# sonobus-rs

A rewrite of SonoBus (1.7.2) in Rust: low-latency
peer-to-peer audio between musicians, as a desktop app, a CLAP/VST3 plugin, an Android app and a
self-hostable rendezvous server. Licensed GPL-3.0-or-later (see `LICENSE` and ADR 0008).

## Two protocols

Each session is either **Compatible** or **Secure**, never both (ADR 0001):

- **Compatible** speaks the stock AOO protocol of SonoBus 1.7.x, byte for byte, so it talks to
  existing SonoBus users and servers. It is not encrypted and the UI says so.
- **Secure** is this project's own protocol (`sonobus/2`) on iroh/QUIC: encrypted, with hole
  punching and relay fallback. Only this app speaks it. Sessions are joined by invite link or QR
  code, by name and password, or by a PIN on the local network (ADR 0002-0004).

The decisions and their consequences are in `docs/adr/`.

## Crates

| Crate | What it is |
|---|---|
| `aoo` | The AOO (audio over OSC) protocol as spoken by SonoBus 1.7.x |
| `sonobus-core` | SonoBus data formats: JUCE ValueTree/XML, `/sb/...` peer messages, settings and setup files |
| `sonobus-dsp` | DSP ported from 1.7.2: channel-group effects, reverbs, monitor delay, metronome |
| `sonobus-files` | Recording (WAV/FLAC/OGG Vorbis) and streaming file playback |
| `sonobus-audio` | Audio devices (cpal) bridged into one fixed-block duplex callback |
| `sonobus-net` | The Secure protocol over iroh: wire codecs, authentication, network runtime |
| `sonobus-engine` | Session and audio engine: peers, jitter buffers, mixer, both protocols |
| `sonobus-app` | The desktop app (`sonobus` binary): egui UI, controller, settings, command line |
| `sonobus-plugin` | SonoBus as a CLAP and VST3 plugin |
| `sonobus-android` | The Android app's Rust side (the Kotlin shell is in `android/`) |
| `sonobus-server` | Rendezvous server: legacy AOO (1.7.x clients) and Secure name + password in one process |

## Building on Linux

The dev shell in `devenv.nix` provides the Rust toolchain and every system library. Enter it with
`devenv shell`, or let direnv load it on `cd`. If `devenv.nix` changed, re-enter the shell.

Without it you need stable Rust plus the packages the CI workflow installs (Debian/Ubuntu names):

```sh
sudo apt-get install pkg-config libopus-dev libasound2-dev libpipewire-0.3-dev libclang-dev
```

The window also needs, at run time only (loaded with dlopen): libGL, libxkbcommon, wayland,
fontconfig and the X11 libraries (libX11, libXcursor, libXi, libXrandr, libxcb).

```sh
cargo build --workspace --all-targets
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings

cargo run --release -p sonobus-app                   # the desktop app
cargo run --release -p sonobus-app -- --help         # command-line options
cargo run --release -p sonobus-app -- --headless     # no window, same controller
```

With mise instead of the dev shell, `mise.toml` provides stable Rust (`mise exec -- cargo ...`);
the system packages above are still needed.

Tests use simulated audio devices and stay on one machine; they never contact public relays.

## Plugin, Android, server

- Plugin bundles (CLAP, VST3): `crates/sonobus-plugin/README.md` (`bundle.sh`, validation,
  state compatibility with the JUCE plugin).
- Android APK: `android/README.md` (toolchain setup, Gradle build, sound sharing, manual test
  checklist).
- Rendezvous server: `crates/sonobus-server/README.md` (options, systemd, Docker, relay).

## Platforms

Linux (app, plugin, server) and Android are built and tested (ADR 0006).
`.github/workflows/ci.yml` also builds and tests on Windows and macOS, but it has never run, so
nothing is known to work there yet. There are no installers, signed builds or app bundles.

## Status

Work in progress. What has never been verified outside the development sandbox, what is not
built yet, what is not planned and the known defects are listed in `docs/open-issues.md`.
