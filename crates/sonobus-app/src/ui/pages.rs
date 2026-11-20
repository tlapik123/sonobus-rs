//! Secondary pages: audio setup, options, effects (CompressorView, ExpanderView,
//! ParametricEqView, limiter), chat, and the setup-file / import path dialog.

use std::path::PathBuf;
use std::time::Instant;

use egui::{CollapsingHeader, RichText, Ui};
use sonobus_core::state::ChannelGroup;
use sonobus_dsp::{Compressor, Expander, ParametricEq};
use sonobus_engine::formats::FORMATS;

use super::mixer::JITTER_MODES;
use super::{row, FileAction, FxTarget, Page, SonobusApp, gain_slider, media, skew_slider, spec, text_field};
use crate::audio::{DeviceChoice, DeviceSetup, DeviceState};
use crate::tr;

fn back(app: &mut SonobusApp, ui: &mut Ui) {
    if ui.button(format!("< {}", tr("Back"))).clicked() {
        app.page = Page::Main;
    }
}

// ---- audio setup ------------------------------------------------------------------------------

fn device_combo(ui: &mut Ui, label: &str, choice: &mut DeviceChoice, devices: &[(String, String)]) {
    let name = match choice {
        DeviceChoice::Default => tr("System default").to_owned(),
        DeviceChoice::None => tr("None").to_owned(),
        DeviceChoice::Id(id) => devices.iter().find(|d| d.0 == *id).map_or(id.clone(), |d| d.1.clone()),
    };
    egui::ComboBox::from_label(label).selected_text(name).show_ui(ui, |ui| {
        ui.selectable_value(choice, DeviceChoice::Default, tr("System default"));
        ui.selectable_value(choice, DeviceChoice::None, tr("None"));
        for (id, n) in devices {
            ui.selectable_value(choice, DeviceChoice::Id(id.clone()), n);
        }
    });
}

pub fn audio_setup(app: &mut SonobusApp, ui: &mut Ui) {
    back(app, ui);
    ui.heading(tr("Setup Audio"));
    let st = app.ctrl.vm().audio.clone();
    match &st.state {
        Some(DeviceState::Off) | None => {
            ui.label(tr("No audio device: the engine runs on a timer (networking only)."));
        }
        Some(DeviceState::Running) => {
            ui.label(RichText::new(tr("Audio device running")).strong());
        }
        Some(DeviceState::Retrying { error, at }) => {
            let secs = at.saturating_duration_since(Instant::now()).as_secs_f32();
            ui.colored_label(egui::Color32::LIGHT_RED, format!("{}: {error}", tr("Audio device error")));
            row(ui, |ui| {
                ui.label(format!("{} {secs:.0} s", tr("Retrying in")));
                if ui.button(tr("Retry now")).clicked() {
                    app.ctrl.retry_audio();
                }
            });
        }
    }
    if !st.input_channels.is_empty() {
        let ch: Vec<String> = st.input_channels.iter().map(ToString::to_string).collect();
        ui.label(format!("{}: {}", tr("Input channels opened"), ch.join(" + ")));
    }
    ui.label(format!(
        "{} Hz, {} {} | {} {} | {} {}{}",
        st.sample_rate,
        st.block_size,
        tr("samples per block"),
        tr("xruns"),
        st.xruns,
        tr("input underflows"),
        st.underflows(),
        st.input_latency_ms().map_or(String::new(), |ms| format!(" | {} {ms:.1} ms", tr("input latency"))),
    ));
    ui.separator();
    let Some(mut s) = app.setup_edit.clone() else { return };
    let devs = &st.devices;
    let outputs: Vec<_> = devs.iter().filter(|d| d.output_channels > 0).map(|d| (d.id.to_string(), d.name.clone())).collect();
    let inputs: Vec<_> = devs.iter().filter(|d| d.input_channels > 0).map(|d| (d.id.to_string(), d.name.clone())).collect();
    let loopbacks: Vec<_> = devs.iter().filter(|d| d.loopback).map(|d| (d.id.to_string(), d.name.clone())).collect();
    ui.checkbox(&mut s.enabled, tr("Use audio devices"));
    ui.add_enabled_ui(s.enabled, |ui| {
        let old_host = s.host.clone();
        let host_name = s.host.clone().unwrap_or_else(|| tr("System default").into());
        egui::ComboBox::from_label(tr("Audio System")).selected_text(host_name).show_ui(ui, |ui| {
            ui.selectable_value(&mut s.host, None, tr("System default"));
            for h in &st.hosts {
                ui.selectable_value(&mut s.host, Some(h.clone()), h);
            }
        });
        if s.host != old_host {
            app.ctrl.scan_devices(s.host.clone());
        }
        if st.scanning {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tr("Scanning devices..."));
            });
        }
        if let Some(e) = &st.scan_error {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
        device_combo(ui, tr("Output:"), &mut s.output, &outputs);
        let old_in = s.input.clone();
        device_combo(ui, tr("Input:"), &mut s.input, &inputs);
        if s.input != old_in {
            s.input_channels = match &s.input {
                DeviceChoice::Id(id) => devs.iter().find(|d| d.id.to_string() == *id).map(|d| d.input_channels.min(2)),
                _ => None,
            };
        }
        if app.platform.is_some() {
            capture_source(app, ui);
        }
        row(ui, |ui| {
            let mut ch = s.input_channels.unwrap_or(0);
            let l = ui.label(tr("Input channels (0 = device default)"));
            if ui.add(egui::DragValue::new(&mut ch).range(0..=64)).labelled_by(l.id).changed() {
                s.input_channels = (ch > 0).then_some(ch);
            }
        });
        // headline: share what this device plays
        let sys_name = s.system_sound.as_ref().map_or(tr("Off").to_owned(), |id| {
            loopbacks.iter().find(|d| d.0 == *id).map_or(id.clone(), |d| d.1.clone())
        });
        let (old_sys, old_app) = (s.system_sound.clone(), s.app_sound.clone());
        egui::ComboBox::from_label(tr("System sound")).selected_text(sys_name).show_ui(ui, |ui| {
            ui.selectable_value(&mut s.system_sound, None, tr("Off"));
            for (id, n) in &loopbacks {
                ui.selectable_value(&mut s.system_sound, Some(id.clone()), n);
            }
        });
        // or one application's sound (PipeWire), on the same channels
        if st.app_capture || s.app_sound.is_some() {
            let app_name = s.app_sound.as_ref().map_or(tr("Off").to_owned(), |id| {
                st.apps.iter().find(|a| a.id == *id).map_or(id.clone(), |a| a.name.clone())
            });
            egui::ComboBox::from_label(tr("App sound")).selected_text(app_name).show_ui(ui, |ui| {
                ui.selectable_value(&mut s.app_sound, None, tr("Off"));
                for a in &st.apps {
                    ui.selectable_value(&mut s.app_sound, Some(a.id.clone()), &a.name);
                }
            });
            if s.app_sound != old_app && s.app_sound.is_some() {
                s.system_sound = None;
            } else if s.system_sound != old_sys && s.system_sound.is_some() {
                s.app_sound = None;
            }
            ui.label(
                RichText::new(tr("Sends what one application plays instead of system sound. Lists the applications playing now (Rescan devices to refresh); a chosen one is picked up again whenever it plays."))
                    .weak(),
            );
        }
        ui.label(
            RichText::new(if loopbacks.is_empty() && !st.scanning {
                tr("This audio system cannot capture system sound (use PipeWire/PulseAudio on Linux, WASAPI on Windows, CoreAudio on macOS 14.2+).")
            } else {
                tr("Sends what this device plays to the others, as an extra input (or the only one with Input = None).")
            })
            .weak(),
        );
        let rates = match &s.output {
            DeviceChoice::Id(id) => devs.iter().find(|d| d.id.to_string() == *id).map(|d| d.sample_rates.clone()),
            _ => None,
        }
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| vec![44100, 48000, 88200, 96000]);
        egui::ComboBox::from_label(tr("Sample rate:")).selected_text(format!("{} Hz", s.sample_rate)).show_ui(ui, |ui| {
            for r in rates {
                ui.selectable_value(&mut s.sample_rate, r, format!("{r} Hz"));
            }
        });
        egui::ComboBox::from_label(tr("Audio Buffer Size:")).selected_text(s.block_size.to_string()).show_ui(ui, |ui| {
            for b in [32, 64, 96, 128, 192, 256, 384, 512, 1024] {
                ui.selectable_value(&mut s.block_size, b, b.to_string());
            }
        });
    });
    app.setup_edit = Some(s.clone());
    row(ui, |ui| {
        let changed = s != *app.ctrl.audio_setup();
        if ui.add_enabled(changed, egui::Button::new(tr("Apply"))).clicked() {
            let old = app.ctrl.audio_setup();
            let rebuild = s.sample_rate != old.sample_rate || s.block_size != old.block_size || s.audio_config().input_channels > old.audio_config().input_channels;
            if app.ctrl.vm().recording.is_some() && rebuild {
                app.confirm_setup = Some(s.clone());
            } else if let Err(e) = app.ctrl.set_audio_setup(s.clone()) {
                app.ctrl.set_status(format!("{}: {e}", tr("Audio setup failed")), true);
            }
        }
        if ui.button(tr("Rescan devices")).clicked() {
            app.ctrl.scan_devices(s.host.clone());
        }
        if ui.button(tr("Reset")).clicked() {
            app.setup_edit = Some(DeviceSetup::default());
        }
    });
}

/// Android: share what the phone plays ("System sound", one app's sound) as an input.
fn capture_source(app: &mut SonobusApp, ui: &mut Ui) {
    use crate::platform::{CaptureState, CaptureTarget};
    let Some(p) = app.platform.as_ref() else { return };
    let c = p.capture();
    let active = !matches!(c.state, CaptureState::Off | CaptureState::Stopped(_));
    let current = match (&c.target, active) {
        (Some(CaptureTarget::System), true) => tr("System sound").to_owned(),
        (Some(CaptureTarget::App { label, .. }), true) => format!("{}: {label}", tr("App sound")),
        _ if app.picking_app => tr("App sound").to_owned(),
        _ => tr("Microphone only").to_owned(),
    };
    ui.label(RichText::new(tr("Share this phone's sound")).strong());
    egui::ComboBox::from_label(tr("Input source")).selected_text(current).show_ui(ui, |ui| {
        if ui.selectable_label(!active && !app.picking_app, tr("Microphone only")).clicked() {
            app.picking_app = false;
            p.stop_capture();
            if let Err(e) = app.ctrl.set_external_input(None) {
                app.ctrl.set_status(format!("{}: {e}", tr("Audio setup failed")), true);
            }
        }
        if ui.selectable_label(active && c.target == Some(CaptureTarget::System), tr("System sound")).clicked() {
            app.picking_app = false;
            p.start_capture(CaptureTarget::System);
        }
        if ui.selectable_label(app.picking_app, tr("App sound...")).clicked() {
            app.picking_app = true;
            p.load_apps();
        }
    });
    if app.picking_app {
        let apps = p.apps();
        if apps.is_empty() {
            ui.horizontal(|ui| {
                ui.spinner();
                ui.label(tr("Loading apps..."));
            });
        }
        egui::ComboBox::from_label(tr("App")).selected_text(tr("Choose an app")).show_ui(ui, |ui| {
            for a in &apps {
                if ui.selectable_label(false, &a.label).on_hover_text(&a.package).clicked() {
                    app.picking_app = false;
                    p.start_capture(CaptureTarget::App { uid: a.uid, label: a.label.clone() });
                }
            }
        });
    }
    match &c.state {
        CaptureState::Off => {}
        CaptureState::NeedsConsent => {
            ui.label(tr("Waiting for your permission: Android asks to allow recording or casting. Choose \"Start\" (on Android 14+ pick \"Share entire screen\" if asked; only sound is used)."));
        }
        CaptureState::Running => {
            let f = c.format.map_or(String::new(), |f| format!(" ({} Hz, {} ch, {} {})", f.sample_rate, f.channels, c.buffer_frames, tr("frames")));
            let lat = c.latency_ms.map_or(String::new(), |ms| format!(" | {} {ms:.0} ms", tr("capture latency")));
            ui.label(RichText::new(format!("{}{f}{lat}", tr("Capturing"))).color(egui::Color32::from_rgb(60, 180, 90)));
        }
        CaptureState::Stopped(why) => {
            ui.colored_label(egui::Color32::LIGHT_RED, format!("{}: {why}", tr("Capture stopped")));
            if let Some(t) = c.target.clone()
                && ui.button(tr("Capture again")).clicked()
            {
                p.start_capture(t);
            }
        }
    }
    ui.label(
        RichText::new(tr("Sends what other apps play to everyone in the session, in stereo, without playing it again here. Android shows a recording notice while it runs. Not captured: apps that block it (many streaming and banking apps), calls, and this app's own sound."))
            .weak(),
    );
    ui.separator();
}

// ---- options ----------------------------------------------------------------------------------

pub fn options(app: &mut SonobusApp, ui: &mut Ui) {
    back(app, ui);
    ui.heading(tr("OPTIONS"));
    let st = app.ctrl.vm().state.clone();
    let (mut p, mut x) = (st.params.clone(), st.extra.clone());
    let q = sonobus_engine::formats::checked(p.def_send_qual);
    egui::ComboBox::from_label(tr("Default Send Quality:")).selected_text(FORMATS[q].name).show_ui(ui, |ui| {
        for (i, f) in FORMATS.iter().enumerate() {
            ui.selectable_value(&mut p.def_send_qual, i as i32, f.name);
        }
    });
    ui.checkbox(&mut x.change_qual_for_all, tr("Change all connected"));
    // caps what peers' quality requests get from us (engine options, persisted by the engine)
    let mut max_q = sonobus_engine::options::max_send_quality(&st.extra);
    let mut honour = sonobus_engine::options::honour_quality_requests(&st.extra);
    row(ui, |ui| {
        let cur = max_q.and_then(|i| FORMATS.get(i)).map_or(tr("No limit"), |f| f.name);
        let r = egui::ComboBox::from_label(tr("Max Send Quality")).selected_text(cur).show_ui(ui, |ui| {
            let mut c = ui.selectable_value(&mut max_q, None, tr("No limit")).changed();
            for (i, f) in FORMATS.iter().enumerate() {
                c |= ui.selectable_value(&mut max_q, Some(i), f.name).changed();
            }
            c
        });
        if r.inner == Some(true) {
            app.ctrl.engine_edit(|e| e.set_max_send_quality(max_q));
        }
        if ui.checkbox(&mut honour, tr("Honour peers' quality requests")).on_hover_text(tr("Peers can ask for a different send quality from you (up to Max Send Quality). Off: you always send your own choice.")).changed() {
            app.ctrl.engine_edit(|e| e.set_honour_quality_requests(honour));
        }
    });
    let cur = JITTER_MODES.iter().find(|m| m.0 as i32 == p.def_net_auto).map_or("?", |m| m.1);
    egui::ComboBox::from_label(tr("Default Jitter Buffer")).selected_text(tr(cur)).show_ui(ui, |ui| {
        for (m, n) in JITTER_MODES {
            ui.selectable_value(&mut p.def_net_auto, m as i32, tr(n));
        }
    });
    let mut ms = p.def_net_buf * 1000.0;
    if skew_slider(ui, &mut ms, spec(0.0, 1000.0, 1.0, 0.5, 1.0), tr("Default Jitter Buffer"), " ms")
        .on_hover_text(tr("This controls controls the default jitter buffer size to start with. When using the Auto modes, it is recommended to keep the value at the minimum so it starts from the lowest possible value. Generally you will only want to set this higher if you are using a Manual mode default which is not recommended."))
        .changed()
    {
        p.def_net_buf = ms / 1000.0;
    }
    // shown as 1/threshold in seconds, 1-20 (OptionsView.cpp:318-336, 600)
    let mut secs = 1.0 / x.auto_drop_rate_thresh.max(0.001);
    if skew_slider(ui, &mut secs, spec(1.0, 20.0, 0.1, 0.5, 2.0), tr("Auto Adjust Drop Threshold"), " sec")
        .on_hover_text(tr("This controls how sensitive the auto-jitter buffer adjustment is when there are audio dropouts. The jitter buffer size will be increased if there are any dropouts within the number of seconds specified here. When this value is smaller it will be less likely to increase the jitter buffer size automatically."))
        .changed()
    {
        x.auto_drop_rate_thresh = 1.0 / secs.max(1.0);
    }
    skew_slider(ui, &mut p.def_peer_level, spec(0.0, 1.0, 0.0, 0.5, 1.0), tr("Default User Level"), "");
    ui.checkbox(&mut p.dynamic_resampling, tr("Use Drift Correction (NOT RECOMMENDED)"));
    ui.checkbox(&mut p.reconnect_last, tr("Auto-Reconnect to Last Group"));
    ui.checkbox(&mut x.reconnect_after_server_loss, tr("Reconnect after losing the server"));
    let mut limiter = st.input_groups.iter().any(|g| g.limiter.enabled);
    let limiter_changed = ui.checkbox(&mut limiter, tr("Use Input FX Limiter")).changed();
    ui.checkbox(&mut x.disable_key_shortcuts, tr("Disable keyboard shortcuts"));
    row(ui, |ui| {
        let mut use_port = x.use_udp_port > 0;
        if ui.checkbox(&mut use_port, tr("Use Specific UDP Port")).changed() {
            x.use_udp_port = if use_port { 11000 } else { 0 };
        }
        if use_port {
            let l = ui.label(tr("Port"));
            ui.add(egui::DragValue::new(&mut x.use_udp_port).range(1024..=65535)).labelled_by(l.id);
            ui.label(RichText::new(tr("(takes effect on next start)")).weak());
        }
    });
    language(ui, &mut x.lang_override_code, app.ctrl.language(), app.caps.plugin);
    if p != st.params || x != st.extra || limiter_changed {
        app.ctrl.edit(move |s| {
            s.params = p.clone();
            s.extra = x.clone();
            if limiter_changed {
                s.input_groups.iter_mut().for_each(|g| g.limiter.enabled = limiter);
            }
        });
    }
    // options the engine keeps (and persists) itself
    use sonobus_engine::options as o;
    let e = &st.extra;
    let mut redundancy = o::packet_redundancy(e);
    let (mut solo_in, mut solo_peer) = (o::get_bool(e, o::REMEMBER_INPUT_SOLO, true), o::get_bool(e, o::REMEMBER_PEER_SOLO, true));
    let mut solo_changed = false;
    row(ui, |ui| {
        let l = ui.label(tr("Packet redundancy"));
        if ui
            .add(egui::Slider::new(&mut redundancy, 1..=5).suffix(format!(" {}", tr("packets"))))
            .labelled_by(l.id)
            .on_hover_text(tr("Sends every packet this many times: survives more packet loss, but multiplies your upload and everyone's download."))
            .changed()
        {
            app.ctrl.engine_edit(|e| e.set_packet_redundancy(redundancy));
        }
    });
    row(ui, |ui| {
        solo_changed |= ui.checkbox(&mut solo_in, tr("Remember Local Track Solo")).changed();
        solo_changed |= ui.checkbox(&mut solo_peer, tr("Remember Peer Solo")).changed();
    });
    if solo_changed {
        app.ctrl.engine_edit(|e| e.set_remember_solo(solo_in, solo_peer));
    }
    blocked(app, ui);
    egui::CollapsingHeader::new(tr("RECORDING")).default_open(true).show(ui, |ui| media::recording_options(app, ui));
    egui::CollapsingHeader::new(tr("OSC control")).default_open(true).show(ui, |ui| osc_options(app, ui));
    if app.caps.secure {
        egui::CollapsingHeader::new(tr("Secure sessions")).default_open(true).show(ui, |ui| super::secure::options(app, ui));
    }
    ui.separator();
    if app.caps.plugin {
        row(ui, |ui| {
            if ui.button(tr("Save as default plugin options")).clicked() {
                app.ctrl.save_plugin_default();
            }
            if ui.button(tr("Reset default plugin options")).clicked() {
                app.ctrl.reset_plugin_default();
            }
        });
    }
    if app.caps.settings_files {
        settings_files(app, ui);
    }
    ui.label(RichText::new(format!("sonobus-rs {}", env!("CARGO_PKG_VERSION"))).weak());
}

/// Where the settings live, "Save now", import.
fn settings_files(app: &mut SonobusApp, ui: &mut Ui) {
    let dir = app.ctrl.vm().settings_dir.clone();
    ui.label(format!(
        "{} {}",
        tr("Settings are saved in"),
        dir.map_or(tr("(not saved)").to_owned(), |d| d.display().to_string())
    ));
    row(ui, |ui| {
        if ui.button(tr("Save now")).clicked()
            && let Err(e) = app.ctrl.save()
        {
            app.ctrl.set_status(format!("{}: {e}", tr("Error while saving")), true);
        }
        if ui.button(tr("Import settings from SonoBus...")).clicked() {
            app.open_page(Page::File(FileAction::ImportSonoBus));
        }
    });
}

/// Blocked addresses, each with Unblock (upstream: PeersContainerView.cpp:726).
fn blocked(app: &mut SonobusApp, ui: &mut Ui) {
    let list = app.ctrl.vm().blocked.clone();
    if list.is_empty() {
        return;
    }
    ui.label(RichText::new(tr("Blocked addresses")).strong());
    for ip in list {
        row(ui, |ui| {
            ui.label(ip.to_string());
            if ui.button(tr("Unblock")).on_hover_text(format!("{} {ip}", tr("Unblock"))).clicked() {
                app.ctrl.unblock(ip);
            }
        });
    }
}

/// OSC remote control (the SonoBusMendeni fork's options; network exposure is ours).
fn osc_options(app: &mut SonobusApp, ui: &mut Ui) {
    let st = app.ctrl.vm().osc.clone();
    let mut c = st.config.clone();
    ui.checkbox(&mut c.enabled, tr("Enable OSC control"));
    ui.add_enabled_ui(c.enabled, |ui| {
        row(ui, |ui| {
            let l = ui.label(tr("Receive Port"));
            ui.add(egui::DragValue::new(&mut c.receive_port).range(1..=65535)).labelled_by(l.id);
        });
        ui.checkbox(&mut c.allow_network, tr("Allow control from the network"));
        if c.allow_network {
            ui.colored_label(
                egui::Color32::from_rgb(230, 170, 60),
                tr("Warning: OSC has no password. Anyone who can reach this port on your network can control SonoBus (mute, levels, recording)."),
            );
        } else {
            ui.label(RichText::new(tr("Only apps on this device can control SonoBus.")).weak());
        }
        row(ui, |ui| {
            text_field(ui, tr("Target IP Address"), &mut c.target_host, "127.0.0.1", false);
            let l = ui.label(tr("Target Port"));
            ui.add(egui::DragValue::new(&mut c.target_port).range(1..=65535)).labelled_by(l.id);
        });
        ui.checkbox(&mut c.send_state_on_start, tr("Send state to target on start"));
        ui.checkbox(&mut c.send_peer_levels, tr("Send peer levels via OSC"));
    });
    match (&st.error, st.listening) {
        (Some(e), _) => {
            ui.colored_label(egui::Color32::LIGHT_RED, e);
        }
        (None, Some(a)) => {
            ui.label(format!("{} {a}", tr("Listening on")));
        }
        (None, None) => {}
    }
    ui.label(
        RichText::new(format!(
            "{} {} ({})",
            tr("Compatible with the TouchOSC template"),
            crate::osc_control::TEMPLATE_NAME,
            tr("SonoBusMendeni project, util folder"),
        ))
        .weak(),
    );
    if c != st.config {
        app.ctrl.set_osc_config(c);
    }
}

/// Language override (OptionsView.cpp:40-76, 1240-1270); takes effect after a restart (a
/// plugin: when the host reopens its view). `current`: the language in use.
fn language(ui: &mut Ui, code: &mut String, current: Option<&'static str>, plugin: bool) {
    use crate::i18n::{self, LANGUAGES};
    let cjk = i18n::needs_cjk_font(current);
    let name = |c: &str| {
        let (_, en, native) = LANGUAGES.iter().find(|l| l.0 == c).copied().unwrap_or(("", "?", ""));
        let latin = !native.chars().any(|ch| ch as u32 >= 0x2e80);
        if native.is_empty() || !(latin || cjk) { tr(en).to_owned() } else { format!("{native} - {}", tr(en)) }
    };
    egui::ComboBox::from_label(tr("Language")).selected_text(name(code)).show_ui(ui, |ui| {
        for (c, _, _) in LANGUAGES {
            ui.selectable_value(code, c.to_owned(), name(c));
        }
    });
    let wanted = if code.is_empty() { i18n::resolve(&i18n::system_locale(), true) } else { i18n::resolve(code, false) };
    if wanted != current {
        let msg = if plugin {
            tr("In order to change the language, the plugin host must close the plugin view and reopen it.")
        } else {
            tr("In order to change the language, the application must be closed and restarted by you.")
        };
        ui.colored_label(egui::Color32::from_rgb(230, 170, 60), msg);
    }
}

// ---- effects ----------------------------------------------------------------------------------

fn dynamics(ui: &mut Ui, d: &mut sonobus_core::state::Dynamics, gate: bool) -> bool {
    let mut c = ui.checkbox(&mut d.enabled, tr("Enabled")).changed();
    let (th, ratio, att, rel) = if gate {
        (Expander::THRESHOLD_DB, Expander::RATIO, Expander::ATTACK_MS, Expander::RELEASE_MS)
    } else {
        (Compressor::THRESHOLD_DB, Compressor::RATIO, Compressor::ATTACK_MS, Compressor::RELEASE_MS)
    };
    row(ui, |ui| {
        c |= skew_slider(ui, &mut d.threshold_db, th, tr(if gate { "Noise Floor" } else { "Threshold" }), " dB").changed();
        c |= skew_slider(ui, &mut d.ratio, ratio, tr("Ratio"), "").changed();
        c |= skew_slider(ui, &mut d.attack_ms, att, tr("Attack"), " ms").changed();
        c |= skew_slider(ui, &mut d.release_ms, rel, tr("Release"), " ms").changed();
        if !gate {
            c |= ui.checkbox(&mut d.auto_makeup, tr("Autoset Makeup")).changed();
            ui.add_enabled_ui(!d.auto_makeup, |ui| {
                c |= skew_slider(ui, &mut d.makeup_gain_db, Compressor::MAKEUP_GAIN_DB, tr("Makeup Gain"), " dB").changed();
            });
            if d.auto_makeup {
                d.makeup_gain_db = sonobus_dsp::auto_makeup_gain_db(d.threshold_db, d.ratio);
            }
        }
    });
    c
}

/// All effects of one channel group; true when changed.
fn effects_ui(ui: &mut Ui, g: &mut ChannelGroup) -> bool {
    let mut c = false;
    CollapsingHeader::new(tr("Noise Gate")).default_open(true).show(ui, |ui| c |= dynamics(ui, &mut g.expander, true));
    CollapsingHeader::new(tr("Compressor")).default_open(true).show(ui, |ui| c |= dynamics(ui, &mut g.compressor, false));
    CollapsingHeader::new(tr("Parametric EQ")).default_open(true).show(ui, |ui| {
        let e = &mut g.eq;
        c |= ui.checkbox(&mut e.enabled, tr("Enabled")).changed();
        row(ui, |ui| {
            c |= skew_slider(ui, &mut e.low_shelf_gain, ParametricEq::LOW_SHELF_GAIN_DB, tr("Low Shelf Gain"), " dB").changed();
            c |= skew_slider(ui, &mut e.low_shelf_freq, ParametricEq::LOW_SHELF_FREQ_HZ, tr("Low Shelf"), " Hz").changed();
        });
        row(ui, |ui| {
            c |= skew_slider(ui, &mut e.para1_gain, ParametricEq::PARA1_GAIN_DB, tr("Gain 1"), " dB").changed();
            c |= skew_slider(ui, &mut e.para1_freq, ParametricEq::PARA1_FREQ_HZ, tr("Freq 1"), " Hz").changed();
            c |= skew_slider(ui, &mut e.para1_q, ParametricEq::PARA1_Q, tr("Q 1"), "").changed();
        });
        row(ui, |ui| {
            c |= skew_slider(ui, &mut e.para2_gain, ParametricEq::PARA2_GAIN_DB, tr("Gain 2"), " dB").changed();
            c |= skew_slider(ui, &mut e.para2_freq, ParametricEq::PARA2_FREQ_HZ, tr("Freq 2"), " Hz").changed();
            c |= skew_slider(ui, &mut e.para2_q, ParametricEq::PARA2_Q, tr("Q 2"), "").changed();
        });
        row(ui, |ui| {
            c |= skew_slider(ui, &mut e.high_shelf_gain, ParametricEq::HIGH_SHELF_GAIN_DB, tr("High Shelf Gain"), " dB").changed();
            c |= skew_slider(ui, &mut e.high_shelf_freq, ParametricEq::HIGH_SHELF_FREQ_HZ, tr("High Shelf"), " Hz").changed();
        });
    });
    // upstream's only limiter UI is the "Use Input FX Limiter" option; the same switch per group
    CollapsingHeader::new(tr("Limiter")).show(ui, |ui| {
        c |= ui.checkbox(&mut g.limiter.enabled, tr("Enabled")).changed();
    });
    CollapsingHeader::new(tr("Polarity Invert")).show(ui, |ui| {
        c |= ui.checkbox(&mut g.invert_polarity, tr("Invert polarity")).changed();
    });
    c
}

pub fn effects(app: &mut SonobusApp, ui: &mut Ui, t: FxTarget) {
    back(app, ui);
    let group = match t {
        FxTarget::Input(i) => app.ctrl.vm().state.input_groups.get(i).cloned(),
        FxTarget::Peer(id, gi) => {
            app.ctrl.vm().snapshot.peers.iter().find(|p| p.id == id).and_then(|p| p.groups.get(gi).cloned())
        }
    };
    let Some(mut g) = group else {
        ui.label(tr("This channel is gone."));
        return;
    };
    let title = match t {
        FxTarget::Input(_) => tr("Input Effects").to_owned(),
        FxTarget::Peer(id, _) => {
            let user = app.ctrl.vm().snapshot.peers.iter().find(|p| p.id == id).map(|p| p.user.clone()).unwrap_or_default();
            format!("{} - {user}", tr("Effects"))
        }
    };
    ui.heading(format!("{title}: {}", g.name));
    let pre = gain_slider(ui, &mut g.gain, 2.0, tr("Pre Level: ")).changed();
    if effects_ui(ui, &mut g) | pre {
        match t {
            FxTarget::Input(i) => app.ctrl.edit(move |s| {
                if let Some(x) = s.input_groups.get_mut(i) {
                    *x = g.clone();
                }
            }),
            FxTarget::Peer(id, gi) => app.ctrl.engine().update_peer_groups(id, move |gs, _| {
                if let Some(x) = gs.get_mut(gi) {
                    *x = g;
                }
            }),
        }
    }
}

// ---- chat -------------------------------------------------------------------------------------

pub fn chat(app: &mut SonobusApp, ui: &mut Ui) {
    if app.page == Page::Chat {
        back(app, ui);
    }
    ui.heading(tr("Chat"));
    app.ctrl.mark_chat_read();
    let peers: Vec<_> = app.ctrl.vm().snapshot.peers.iter().map(|p| (p.id, p.user.clone())).collect();
    row(ui, |ui| {
        let cur = app.chat_target.map_or(tr("ALL"), |t| peers.iter().find(|p| p.0 == t).map_or("?", |p| p.1.as_str()));
        egui::ComboBox::from_label(tr("To")).selected_text(cur).show_ui(ui, |ui| {
            ui.selectable_value(&mut app.chat_target, None, tr("ALL"));
            for (id, p) in &peers {
                ui.selectable_value(&mut app.chat_target, Some(*id), format!("{}{p}", tr("Private Chat with:")));
            }
        });
    });
    egui::ScrollArea::vertical().id_salt("chat_log").max_height(320.0).stick_to_bottom(true).show(ui, |ui| {
        for m in &app.ctrl.vm().chat {
            if m.from.is_empty() && !m.own {
                ui.label(RichText::new(&m.message).italics().weak());
                continue;
            }
            let who = if m.own { tr("Me") } else { m.from.as_str() };
            let to = if m.targets.is_empty() { String::new() } else { format!(" -> {}", m.targets) };
            ui.label(RichText::new(format!("{who}{to}:")).strong());
            ui.label(&m.message);
        }
    });
    let mut send = false;
    ui.horizontal(|ui| {
        let l = ui.label(tr("Message Text"));
        let r = ui
            .add(egui::TextEdit::singleline(&mut app.chat_text).hint_text(tr("Enter message here...")).desired_width(200.0))
            .labelled_by(l.id);
        send |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        send |= ui.button(tr("Send")).on_hover_text(tr("Send Message")).clicked();
        if send {
            r.request_focus();
        }
    });
    if send && !app.chat_text.trim().is_empty() {
        let targets: Vec<_> = app.chat_target.into_iter().collect();
        let text = std::mem::take(&mut app.chat_text);
        app.ctrl.send_chat(&targets, text.trim());
    }
}

// ---- file path dialog -------------------------------------------------------------------------

/// Path entry instead of a native dialog (no GTK/portal dependency; works on every platform).
pub fn file(app: &mut SonobusApp, ui: &mut Ui, a: FileAction) {
    back(app, ui);
    let (title, button) = match a {
        FileAction::LoadSetup => (tr("Choose a setup file to load"), tr("Load Setup file")),
        FileAction::SaveSetup => (tr("Choose a location and name to store the setup"), tr("Save Setup file")),
        FileAction::ImportSonoBus => (tr("Import settings from SonoBus..."), tr("Import")),
        FileAction::OpenAudio => (tr("Choose an audio file to open..."), tr("Load File")),
        FileAction::RecordDir => (tr("Choose the folder for new recordings"), tr("Choose Folder")),
        FileAction::SampleFile => (tr("Select an audio file..."), tr("Select")),
    };
    ui.heading(title);
    let label = if a == FileAction::RecordDir { tr("Folder") } else { tr("File") };
    text_field(ui, label, &mut app.path_text, "/path/to/file", false);
    if ui.button(button).clicked() {
        let path = PathBuf::from(app.path_text.trim());
        let mut next = Page::Main;
        let res = match a {
            FileAction::LoadSetup => app.ctrl.load_setup(&path),
            FileAction::SaveSetup => app.ctrl.save_setup(&path),
            FileAction::ImportSonoBus => app.ctrl.import_upstream(Some(&path)),
            FileAction::OpenAudio => app.ctrl.load_file(&path),
            FileAction::RecordDir => {
                next = Page::Options;
                std::fs::create_dir_all(&path).map(|()| app.ctrl.set_record_dir(&path))
            }
            FileAction::SampleFile => {
                next = Page::SampleEdit;
                if let Some(e) = &mut app.sample_edit {
                    e.path = path.to_string_lossy().into_owned();
                }
                Ok(())
            }
        };
        match res {
            Ok(()) => {
                if matches!(a, FileAction::LoadSetup | FileAction::SaveSetup) {
                    app.ctrl.set_status(format!("{} {}", button, path.display()), false);
                }
                app.page = next;
            }
            Err(e) => {
                let what = if a == FileAction::SaveSetup { tr("Error while saving") } else { tr("Error while loading") };
                app.ctrl.set_status(format!("{what}: {e}"), true);
            }
        }
    }
    let recents: Vec<String> = match a {
        FileAction::LoadSetup => app.ctrl.vm().recent_setups.iter().rev().cloned().collect(),
        FileAction::OpenAudio | FileAction::SampleFile => app.ctrl.vm().recent_files.clone(),
        _ => Vec::new(),
    };
    if !recents.is_empty() {
        ui.label(RichText::new(tr(if a == FileAction::LoadSetup { "Load Recent Setup" } else { "Recent files" })).strong());
    }
    for r in &recents {
        if ui.button(r).clicked() {
            app.path_text = r.clone();
        }
    }
    browse(app, ui, a == FileAction::RecordDir);
}

/// The folder of the typed path, to click through (folders, and files unless `dirs_only`).
fn browse(app: &mut SonobusApp, ui: &mut Ui, dirs_only: bool) {
    let text = app.path_text.trim().to_owned();
    // the folder of the typed text, checked once per text, not every frame
    let typed = egui::Id::unique("browse_typed");
    let dir = match ui.data(|d| d.get_temp::<(String, PathBuf)>(typed)) {
        Some((t, d)) if t == text => d,
        _ => {
            let p = PathBuf::from(&text);
            let d = if text.ends_with(std::path::MAIN_SEPARATOR) || p.is_dir() { p } else { p.parent().map(PathBuf::from).unwrap_or_default() };
            ui.data_mut(|m| m.insert_temp(typed, (text.clone(), d.clone())));
            d
        }
    };
    if dir.as_os_str().is_empty() {
        return;
    }
    // listed once per folder, not every frame
    let id = egui::Id::unique("browse_listing");
    let cached: Option<(PathBuf, Vec<(String, bool)>)> = ui.data(|d| d.get_temp(id));
    let entries = match cached {
        Some((d, e)) if d == dir => e,
        _ => {
            let mut e: Vec<(String, bool)> = std::fs::read_dir(&dir)
                .map(|rd| rd.flatten().map(|x| (x.file_name().to_string_lossy().into_owned(), x.path().is_dir())).filter(|(n, d)| !n.starts_with('.') && (*d || !dirs_only)).collect())
                .unwrap_or_default();
            e.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.to_lowercase().cmp(&b.0.to_lowercase())));
            ui.data_mut(|d| d.insert_temp(id, (dir.clone(), e.clone())));
            e
        }
    };
    ui.separator();
    row(ui, |ui| {
        ui.label(RichText::new(dir.display().to_string()).weak());
        if let Some(up) = dir.parent()
            && ui.small_button(format!("^ {}", tr("Up"))).clicked()
        {
            app.path_text = format!("{}{}", up.display(), std::path::MAIN_SEPARATOR);
        }
    });
    egui::ScrollArea::vertical().id_salt("browse").max_height(260.0).show(ui, |ui| {
        for (name, is_dir) in entries {
            let label = if is_dir { format!("[{name}]") } else { name.clone() };
            if ui.selectable_label(false, label).clicked() {
                let p = dir.join(&name);
                app.path_text = if is_dir { format!("{}{}", p.display(), std::path::MAIN_SEPARATOR) } else { p.display().to_string() };
            }
        }
    });
}
