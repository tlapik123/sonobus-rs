# Linux and Android first; Windows and macOS through CI later

Development happens in a Linux sandbox, so only Linux (app, plugin, server) and Android (APK) are built and tested. Windows and macOS builds are deferred to CI runners on those systems rather than cross-compiled locally: the audio backends, plugin bundles and code signing all need the real platform.

## Consequences

- Nothing platform-specific for Windows or macOS has been compiled yet. `sonobus-plugin/bundle.sh` has untested branches for both.
- Per-application sound capture on desktop (PipeWire, Windows process loopback) is not built; `InputSource::App` exists for it.
