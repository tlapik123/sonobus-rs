# sonobus-plugin

SonoBus as an audio plugin, so a DAW track can send its audio to a SonoBus group and play the
group back, like upstream's JUCE plugin. Built with [nice-plug](https://codeberg.org/RustAudio/nice-plug)
0.4 (the maintained fork of NIH-plug) and nice-plug-egui 0.6 (egui 0.37, the app's egui).

## Formats

| Format | Status | Why |
|---|---|---|
| CLAP | built (Linux tested) | nice-plug |
| VST3 | built (Linux tested) | nice-plug, `vst3` crate bindings |
| LV2 | not provided | no maintained Rust LV2 path: the `lv2` crates were last released in 2020; clap-wrapper does not target LV2 |
| AU v2 / AU v3 (macOS/iOS) | not provided | no Rust route; [clap-wrapper](https://github.com/free-audio/clap-wrapper) (C++, MIT) can wrap the CLAP as AUv2/AUv3 (not built here) |
| AAX | not provided | proprietary SDK; clap-wrapper can wrap a CLAP, but distributing a GPLv3 plugin linked with the AAX SDK needs a licence exception only the copyright holders can give |
| VST2 | not provided | SDK no longer licensed by Steinberg |

Licences: nice-plug and nice-plug-egui ISC; `vst3` bindings MIT OR Apache-2.0 (Steinberg's VST3 SDK
is MIT since 3.8, 2025); clap-sys MIT OR Apache-2.0; baseview/egui MIT OR Apache-2.0. All are
GPLv3-compatible, so the bundles are distributed under this project's GPL-3.0-or-later with
source. Nothing from the VST3 SDK is vendored.

## Build

```sh
mise exec -- crates/sonobus-plugin/bundle.sh          # release; --debug for a debug build
```

Bundles land in `target/bundled/`: `SonoBus RS.clap` and `SonoBus RS.vst3/`. The script also lays
out Windows (`Contents/x86_64-win/*.vst3`) and macOS bundles (`Contents/MacOS` + `Info.plist`,
unsigned), but only the Linux path has been run.

Install locations:

| OS | CLAP | VST3 |
|---|---|---|
| Linux | `~/.clap/` or `/usr/lib/clap/` | `~/.vst3/` or `/usr/lib/vst3/` |
| Windows | `%COMMONPROGRAMFILES%\CLAP\` | `%COMMONPROGRAMFILES%\VST3\` |
| macOS | `~/Library/Audio/Plug-Ins/CLAP/` | `~/Library/Audio/Plug-Ins/VST3/` |

## Behaviour (as upstream's plugin mode)

- Buses (upstream develop, P.cpp:547-595): stereo "Main In"/"Mix Out", eight stereo "Aux N In"
  (extra input channels an input group can use) and eight stereo "Aux N Out": peer slot N-1's
  received audio before its level/effects, so each user can be recorded on its own track. Also
  stereo-only, mono-in/stereo-out and mono layouts. Deviation: upstream assigns aux outputs by
  position in its peer list (which shifts when someone leaves); here by stable peer slot.
- Parameters: upstream's 37 APVTS parameters with the same ids, names, order, ranges, skews,
  steps and plugin defaults (dry 1.0, send channels "Match # Inputs", sync to host on); VST3
  parameter ids equal JUCE's (`String::hashCode` with the sign bit cleared). Plus a host bypass
  (JUCE's wrapper adds one too): input passes through, aux outputs are silent, the engine does not
  run (JUCE's default `processBlockBypassed`).
- Host transport: with "Sync to Host" the metronome takes the host tempo and, while playing, the
  host beat position (ppq). Deviation: upstream also writes the host tempo back into the
  `mettempo` parameter (notifying the host); nice-plug has no audio-thread parameter output, so
  the parameter keeps its value and only the metronome follows the host.
- State: hosts store nice-plug's JSON. Its `sonobus-state` field is upstream's
  `getStateInformation` payload (the binary `SonoBusAoO` ValueTree, full scope) in JUCE base64.
  On load the metronome and monitor solo are switched off, as upstream does. Deviation: what
  belongs to the machine (OSC remote control, UDP port, record folder, language, recent
  connections with their passwords) is neither saved into nor loaded from projects (they travel
  between people); `PluginDefault.xml` is where an instance gets those.
- `PluginDefault.xml` is read from the app's settings directory (`~/.config/sonobus-rs/` etc.)
  when an instance is created; the editor can save and reset it (upstream's format: XML, no cache,
  no input groups).
- Latency reported: 0, tail: 0 (as upstream). The engine runs in blocks of the host's maximum
  buffer size; a host that sends smaller or odd-sized callbacks makes it run one block late
  through a FIFO (unreported latency of one block).
- Sample-rate/block-size/layout changes rebuild the engine on the host's activate call (never on
  the audio thread); a connected session is rejoined, the first activation reconnects to the
  most recent group when "Reconnect Last" is on.
- Instances: each has its own controller, engine, threads and UDP socket (any free port, or the
  saved `UseUdpPort` and the next 99), and its own UI language. No globals (translation tables
  are shared read-only). The plugin never opens audio devices.
- Parameter changes reach the engine through a 10 ms poll thread per instance, so the audio
  thread never calls the engine's control side (upstream applies them immediately on the thread
  that set them).

## Editor

The app's own UI (`sonobus_app::ui::SonobusApp`) with the plugin's capabilities, drawn through
nice-plug-egui. The app's controller runs in hosted mode (`AppController::hosted`): the plugin
builds and processes the engine and hands the controller a new one on a sample-rate/layout
change; the controller keeps the session (chat, status, peers rejoin). Controller and UI state
live with the instance, so closing and reopening the editor keeps them; with the editor closed
the parameter thread ticks the controller (events, chat, OSC).

Not in the plugin (as upstream's plugin mode): audio device setup, hosting a connection server
(E.cpp:1307-1310), setup files load/save (E.cpp:2548, 2595) and importing stock settings, quit
confirmation, the settings directory and "Save now". Plugin-only: metronome "Sync to Host"
(E.cpp:1206), "Save as default plugin options" / "Reset default plugin options"
(OptionsView.cpp:384-387). Recording, file playback, the soundboard (saved to the app's
`soundboards.xml`, shared by every instance and the app, as upstream) and OSC control (off by
default) are there. A language change applies when the host reopens the editor
(OptionsView.cpp:1241-1247); every instance has its own language.

Parameters: a UI (or OSC) edit of one of the 37 automatable parameters is a host gesture
(begin/set/end) on the same nice-plug parameter objects host automation moves; automation shows
in the UI on the next frame. Edits made while the editor is closed (OSC) set the values without a
gesture. Each change is its own gesture (a slider drag is many), not one per drag.

Resizable (minimum 320x340 as upstream, default 800x600); the size is kept in
`lastWindowWidth/Height`. Below 700 points wide the layout is single-column.

Text fields: hosts that keep keyboard focus for themselves (some DAWs pass keys to the plugin
window only when it asks) may not deliver typing to the connect form or chat; not verified in a
real DAW.

## Validation (Linux, sandbox)

- clap-validator 0.4.1: 44 tests, 34 passed, 0 failed, 10 skipped (extensions the plugin does not
  implement: presets, notes, 64-bit audio, configurable ports).
- pluginval 1.0.4, strictness 10, with the editor under Xvfb: SUCCESS.
- Steinberg VST3 SDK `validator` (SDK master, MIT): 47 passed, 0 failed. It prints
  "The bypass parameter was not correctly set" in "Parameter Bypass persistence": nice-plug
  ignores `setParamNormalized` while processing (the value arrives through `process`); the
  persistence check itself passes.
- `cargo test -p sonobus-plugin`: parameters, state, `PluginDefault.xml`, metronome sync,
  bypass, layouts, the editor UI of two instances in two languages (and reopened), two instances through a local server (with aux outputs and rejoin after a
  sample-rate change), and an allocation-counting test of `process`.

Not verified: real DAWs, Windows and macOS builds.

## State compatibility with the JUCE plugin

Not usable for project migration, by design: the class ids differ (two different plugins must not
collide), so a DAW never loads this plugin for a project saved with the JUCE one. What is
compatible is the payload: the `sonobus-state` field holds exactly the bytes JUCE's
`getStateInformation` returns (`ValueTree::writeToStream` via `sonobus_core`, whose writer has
its own golden tests; the plugin test checks the field is byte-identical to that writer), and `PluginDefault.xml` is upstream's file. The host chunk
around it is nice-plug JSON, not JUCE's (JUCE's VST3 wrapper appends its private bypass data;
upstream has no CLAP). Not checked against a state captured from the JUCE plugin.

Upstream ids, for reference: VST3 component UID ABCDEF01-9182FAEB-536F6E6F-4E427573
(manufacturer `Sono`, plugin `NBus`; instrument `IBus`), LV2 URI `https://sonobus.net/lv2/sonobus`.
