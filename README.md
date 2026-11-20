# sonobus-rs

SonoBus (1.7.2) rewritten in Rust: low-latency peer-to-peer audio, for musicians playing
together and for silent discos (one host, many listeners in sync). A desktop app, a CLAP/VST3
plugin, an Android app and a self-hostable rendezvous server. GPL-3.0-or-later.

A session is either **Compatible** (the stock AOO protocol of SonoBus 1.7.x, unencrypted, talks
to existing SonoBus users) or **Secure** (this project's own encrypted protocol on iroh/QUIC,
joined by invite link, name and password, or PIN). The decisions behind this are in `docs/adr/`.

## Build

Linux, inside the dev shell (`devenv shell`, or direnv), which provides Rust and every library:

```sh
cargo test --workspace
cargo run --release -p sonobus-app              # desktop app; --help, --headless
```

Without the dev shell: stable Rust and, on Debian/Ubuntu,
`pkg-config libopus-dev libasound2-dev libpipewire-0.3-dev libclang-dev`.

- Plugin: `crates/sonobus-plugin/README.md`
- Android: `android/README.md`
- Server: `crates/sonobus-server/README.md`

## Status

Work in progress. Linux and Android are built and tested; the Windows and macOS CI jobs have
never passed. What is unverified, not built and known to be broken: `docs/open-issues.md`.
