# All-Rust UI with egui; plugins are CLAP and VST3 only

The UI is egui/eframe so the standalone app, the plugin editor and Android share one codebase and one language. Plugins are built with nice-plug, which gives CLAP and VST3.

LV2, AU and AAX are **not planned**: nice-plug has no route to them and nobody has asked for them. If one is ever needed, wrapping the CLAP build with clap-wrapper is the cheapest path (AU and standalone VST3 come from it; AAX also needs Avid's SDK and signing).

## Consequences

- The app logic lives in a window-free `AppController`, so it is testable without a display and is shared by the standalone app and the plugin.
- The plugin has no settings file, so its Secure-session device identity lasts only as long as the instance.
