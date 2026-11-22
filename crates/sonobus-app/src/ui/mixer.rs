//! Main view: main controls and meters, the input mixer (ChannelGroupsView), metronome, main
//! reverb, monitor delay, and one strip per peer (PeersContainerView).

use egui::{CollapsingHeader, RichText, Ui};
use sonobus_core::state::ChannelGroup;
use sonobus_dsp::{MonitorDelay, ReverbParams};
use sonobus_engine::formats::FORMATS;
use sonobus_engine::{JitterMode, PeerStatus};

use super::{
    FxTarget, Page, SonobusApp, gain_slider, media, meter, row, session, skew_slider, spec, toggle,
};
use crate::controller::AppController;
use crate::tr;

pub const JITTER_MODES: [(JitterMode, &str); 4] = [
    (JitterMode::Off, "Manual"),
    (JitterMode::AutoIncrease, "Auto Up"),
    (JitterMode::AutoFull, "Auto"),
    (JitterMode::InitialAuto, "Initial Auto"),
];

pub fn page(app: &mut SonobusApp, ui: &mut Ui) {
    let vm = app.ctrl.vm();
    if let Some(s) = &vm.secure {
        let host = s.role == sonobus_engine::modern::SessionRole::Host;
        let waiting = vm.snapshot.peers.is_empty();
        CollapsingHeader::new(
            RichText::new(format!("{}: {}", tr("Secure session"), s.session)).heading(),
        )
        .id_salt("secure_session")
        .default_open(host)
        .show(ui, |ui| super::secure::session_panel(app, ui, false));
        if waiting {
            ui.label(tr("Waiting for other users to join group"));
        }
    } else if let Some(g) = &vm.snapshot.group {
        ui.heading(format!("{}{g}", tr("Group: ")));
        if vm.snapshot.peers.is_empty() {
            ui.label(tr("Waiting for other users to join group"));
        }
    } else if vm.connecting {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(tr("Connecting..."));
        });
    } else {
        ui.label(tr("Press Connect button to start."));
    }
    main_controls(app, ui);
    media::transport(app, ui);
    ui.separator();
    if !app.ctrl.vm().snapshot.peers.is_empty() {
        session::receive_sync(app, ui);
    }
    peers(app, ui);
    if app.ctrl.vm().snapshot.peers.len() >= 2 {
        CollapsingHeader::new(tr("Patchbay")).show(ui, |ui| session::patchbay(app, ui));
    }
    ui.separator();
    CollapsingHeader::new(tr("INPUT MIXER"))
        .default_open(true)
        .show(ui, |ui| input_mixer(app, ui));
    let (plugin, sends) = (app.caps.plugin, app.sends());
    CollapsingHeader::new(tr("Metronome"))
        .show(ui, |ui| metronome(&mut app.ctrl, ui, plugin, sends));
    CollapsingHeader::new(tr("Reverb")).show(ui, |ui| reverb(&mut app.ctrl, ui));
    CollapsingHeader::new(tr("Monitor Delay")).show(ui, |ui| monitor_delay(&mut app.ctrl, ui));
}

fn main_controls(app: &mut SonobusApp, ui: &mut Ui) {
    let mut p = app.ctrl.vm().state.params.clone();
    let mut change_all = app.ctrl.vm().state.extra.change_qual_for_all;
    let old = p.clone();
    // a broadcast listener sends nothing: no send controls
    let sends = app.sends();
    row(ui, |ui| {
        toggle(ui, &mut p.main_in_mute, tr("Mute All Input"))
            .on_hover_text(tr("Silences your Input, none of your audio (including file playback) will be sent to users and they will see a muted indicator"));
        toggle(ui, &mut p.main_recv_mute, tr("Mute All Users")).on_hover_text(tr(
            "Mutes/Unmutes all users, no audio data will be received when users are muted",
        ));
        if sends {
            toggle(ui, &mut p.main_send_mute, tr("Send Mute")).on_hover_text(tr(
                "Mutes your input preventing everyone from hearing you, without any indicator",
            ));
        }
    });
    row(ui, |ui| {
        gain_slider(ui, &mut p.wet, 2.0, tr("Out Level")).on_hover_text(tr(
            "This is the main volume control which affects everything you hear",
        ));
        skew_slider(
            ui,
            &mut p.dry,
            spec(0.0, 1.0, 0.0, 0.5, 0.0),
            tr("Monitor"),
            "",
        )
        .on_hover_text(tr(
            "This adjusts the level of the monitoring of your input, that only you hear",
        ));
    });
    if sends {
        row(ui, |ui| {
            let send_names = [
                (1, tr("Send Mono")),
                (2, tr("Send Stereo")),
                (0, tr("Send Multichannel")),
            ];
            let cur = send_names
                .iter()
                .find(|(v, _)| *v == p.send_channels)
                .map_or("?", |(_, n)| n);
            egui::ComboBox::from_label(tr("Send Channels"))
                .selected_text(cur)
                .show_ui(ui, |ui| {
                    for (v, n) in send_names {
                        ui.selectable_value(&mut p.send_channels, v, n);
                    }
                });
            let q = sonobus_engine::formats::checked(p.def_send_qual);
            egui::ComboBox::from_label(tr("Send Quality:"))
                .selected_text(FORMATS[q].name)
                .show_ui(ui, |ui| {
                    for (i, f) in FORMATS.iter().enumerate() {
                        ui.selectable_value(&mut p.def_send_qual, i as i32, f.name);
                    }
                });
            ui.checkbox(&mut change_all, tr("Change all connected"));
        });
    }
    row(ui, |ui| {
        let s = &app.ctrl.vm().snapshot;
        ui.vertical(|ui| {
            ui.label(tr("In Level"));
            meter(ui, &s.input, tr("Input meter"));
        });
        if sends {
            ui.vertical(|ui| {
                ui.label(tr("Send"));
                meter(ui, &s.send, tr("Send meter"));
            });
        }
        ui.vertical(|ui| {
            ui.label(tr("Output"));
            meter(ui, &s.output, tr("Output meter"));
        });
    });
    if p != old || change_all != app.ctrl.vm().state.extra.change_qual_for_all {
        app.ctrl.edit(move |s| {
            s.extra.change_qual_for_all = change_all;
            s.params = p.clone();
        });
    }
}

/// Edits of one channel group; returns true when something changed.
fn group_controls(ui: &mut Ui, g: &mut ChannelGroup, input: bool) -> bool {
    let mut changed = false;
    row(ui, |ui| {
        if input {
            let l = ui.label(tr("Name"));
            changed |= ui
                .add(egui::TextEdit::singleline(&mut g.name).desired_width(120.0))
                .labelled_by(l.id)
                .on_hover_text(tr("Set name for this group that others will see"))
                .changed();
            let l = ui.label(tr("Input Source"));
            let mut start = g.chan_start + 1;
            changed |= ui
                .add(egui::DragValue::new(&mut start).range(1..=64))
                .labelled_by(l.id)
                .changed();
            g.chan_start = start - 1;
            let l = ui.label(tr("ch"));
            let n = g.num_channels;
            changed |= ui
                .add(egui::DragValue::new(&mut g.num_channels).range(1..=64))
                .labelled_by(l.id)
                .changed();
            if g.num_channels != n {
                g.pans.resize(g.num_channels as usize, 0.0);
            }
        }
        changed |= toggle(ui, &mut g.muted, tr("MUTE"))
            .on_hover_text(tr("Mute this channel for both sending and monitoring"))
            .changed();
    });
    row(ui, |ui| {
        changed |= gain_slider(ui, &mut g.gain, 2.0, tr("Level: ")).changed();
        let pan = spec(-1.0, 1.0, 0.0, 1.0, 0.0);
        if g.num_channels == 2 {
            changed |= skew_slider(ui, &mut g.pan_stereo[0], pan, tr("Pan: "), " L").changed();
            changed |= skew_slider(ui, &mut g.pan_stereo[1], pan, tr("Pan: "), " R").changed();
        } else if let Some(p) = g.pans.first_mut() {
            changed |= skew_slider(ui, p, pan, tr("Pan"), "").changed();
        }
        if input {
            changed |= gain_slider(ui, &mut g.monitor, 2.0, tr("Monitor: "))
                .on_hover_text(tr("Monitor output level"))
                .changed();
            changed |= skew_slider(
                ui,
                &mut g.in_reverb_send,
                ReverbParams::LEVEL,
                tr("In Reverb"),
                "",
            )
            .changed();
        }
    });
    changed
}

fn fx_summary(g: &ChannelGroup) -> String {
    let mut on = Vec::new();
    for (e, n) in [
        (g.expander.enabled, "Noise Gate"),
        (g.compressor.enabled, "Compressor"),
        (g.eq.enabled, "EQ"),
        (g.limiter.enabled, "Limiter"),
    ] {
        if e {
            on.push(tr(n));
        }
    }
    on.join(", ")
}

fn input_mixer(app: &mut SonobusApp, ui: &mut Ui) {
    let groups = app.ctrl.vm().state.input_groups.clone();
    let mut remove = None;
    for (i, g) in groups.iter().enumerate() {
        ui.push_id(("input", i), |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| {
                let mut g = g.clone();
                let changed = group_controls(ui, &mut g, true);
                row(ui, |ui| {
                    let mut solo = app.ctrl.vm().input_solo.get(i).copied().unwrap_or(false);
                    if toggle(ui, &mut solo, tr("SOLO"))
                        .on_hover_text(tr(
                            "Solo - Listen to only this channel, does not affect sending",
                        ))
                        .changed()
                    {
                        app.ctrl.set_input_soloed(i, solo);
                    }
                    if ui
                        .button(tr("Effects"))
                        .on_hover_text(tr("Edit input effects (applied before sending)"))
                        .clicked()
                    {
                        app.page = Page::Effects(FxTarget::Input(i));
                    }
                    ui.label(RichText::new(fx_summary(&g)).weak());
                    if groups.len() > 1 && ui.button(tr("REMOVE")).clicked() {
                        remove = Some(i);
                    }
                });
                if changed {
                    app.ctrl.edit(move |s| {
                        if let Some(x) = s.input_groups.get_mut(i) {
                            *x = g.clone();
                        }
                    });
                }
            });
        });
    }
    if let Some(i) = remove {
        app.ctrl.remove_input_group(i);
    }
    if ui
        .button(tr("Add Input Group"))
        .on_hover_text(tr("Add New Input Group"))
        .clicked()
    {
        app.ctrl.add_input_group();
    }
}

/// `plugin`: with "Sync to Host" (upstream shows it only in the plugin, E.cpp:1206); `sends`:
/// with "Send Metronome to All".
fn metronome(ctrl: &mut AppController, ui: &mut Ui, plugin: bool, sends: bool) {
    let mut p = ctrl.vm().state.params.clone();
    let old = p.clone();
    row(ui, |ui| {
        toggle(ui, &mut p.met_enabled, tr("Metronome On/Off"));
        if sends {
            ui.checkbox(&mut p.send_met_audio, tr("Send Metronome to All"));
        }
    });
    row(ui, |ui| {
        skew_slider(
            ui,
            &mut p.met_tempo,
            spec(10.0, 400.0, 1.0, 0.5, 100.0),
            tr("Tempo"),
            " bpm",
        );
        skew_slider(
            ui,
            &mut p.met_gain,
            spec(0.0, 1.0, 0.0, 0.5, 0.5),
            tr("Metronome Level"),
            "",
        );
    });
    let mut beats = sonobus_engine::options::beats_per_bar(&ctrl.vm().state.extra);
    let mut beats_changed = false;
    row(ui, |ui| {
        let l = ui.label(tr("Beats per bar"));
        beats_changed = ui
            .add(egui::DragValue::new(&mut beats).range(1..=12))
            .labelled_by(l.id)
            .on_hover_text(tr("1: no accent on the first beat"))
            .changed();
        toggle(ui, &mut p.sync_met_file, tr("Sync with File"))
            .on_hover_text(tr("Synchronize metronome start with file playback"));
        if plugin {
            toggle(ui, &mut p.sync_met_host, tr("Sync to Host"))
                .on_hover_text(tr("Synchronize metronome tempo with plugin host"));
        }
    });
    if p != old {
        ctrl.edit(move |s| s.params = p.clone());
    }
    if beats_changed {
        ctrl.engine_edit(|e| e.set_met_beats_per_bar(beats));
    }
}

fn reverb(ctrl: &mut AppController, ui: &mut Ui) {
    let mut p = ctrl.vm().state.params.clone();
    let old = p.clone();
    row(ui, |ui| {
        toggle(ui, &mut p.main_reverb_enabled, tr("Reverb Enabled"));
        let models = [(0, tr("Freeverb")), (1, tr("MVerb")), (2, tr("Zita"))];
        let cur = models
            .iter()
            .find(|m| m.0 == p.main_reverb_model)
            .map_or("?", |m| m.1);
        egui::ComboBox::from_label(tr("Reverb Style"))
            .selected_text(cur)
            .show_ui(ui, |ui| {
                for (v, n) in models {
                    ui.selectable_value(&mut p.main_reverb_model, v, n);
                }
            });
    });
    row(ui, |ui| {
        skew_slider(
            ui,
            &mut p.main_reverb_level,
            ReverbParams::LEVEL,
            tr("Level"),
            "",
        );
        skew_slider(
            ui,
            &mut p.main_reverb_size,
            ReverbParams::SIZE,
            tr("Size"),
            "",
        );
        skew_slider(
            ui,
            &mut p.main_reverb_damp,
            ReverbParams::DAMPING,
            tr("Damping"),
            "",
        );
        skew_slider(
            ui,
            &mut p.main_reverb_predelay,
            ReverbParams::PREDELAY_MS,
            tr("Pre-Delay"),
            " ms",
        );
    });
    if p != old {
        ctrl.edit(move |s| s.params = p.clone());
    }
}

/// "MON DELAY": monitor delay on/off for every input group.
pub fn toggle_monitor_delay(ctrl: &mut AppController) {
    let on = !ctrl
        .vm()
        .state
        .input_groups
        .iter()
        .any(|g| g.monitor_delay.enabled);
    ctrl.edit(move |s| {
        s.input_groups
            .iter_mut()
            .for_each(|g| g.monitor_delay.enabled = on)
    });
}

fn monitor_delay(ctrl: &mut AppController, ui: &mut Ui) {
    let st = &ctrl.vm().state;
    let mut on = st.input_groups.iter().any(|g| g.monitor_delay.enabled);
    let mut link = st.extra.link_mon_delay_times;
    let mut times: Vec<f32> = st
        .input_groups
        .iter()
        .map(|g| g.monitor_delay.delay_time_ms)
        .collect();
    let peers = &ctrl.vm().snapshot.peers;
    let avg = |f: fn(&PeerStatus) -> f32| {
        (!peers.is_empty()).then(|| peers.iter().map(f).sum::<f32>() / peers.len() as f32)
    };
    let (one_way, round_trip) = (
        avg(|p| p.latency.outgoing_ms),
        avg(|p| p.latency.round_trip_ms),
    );
    ui.label(tr("Control additional self-monitoring delay, which can help mitigate synchronization with others"));
    let mut changed = false;
    row(ui, |ui| {
        changed |= toggle(ui, &mut on, tr("MON DELAY"))
            .on_hover_text(tr("Enable/Disable Monitor Delay"))
            .changed();
        changed |= ui
            .checkbox(&mut link, tr("Link Delay Time with other inputs"))
            .changed();
    });
    for (i, t) in times.iter_mut().enumerate() {
        if link && i > 0 {
            break;
        }
        let label = if link {
            tr("Delay Time").to_owned()
        } else {
            format!("{} {}", tr("Delay Time"), i + 1)
        };
        ui.push_id(("mondelay", i), |ui| {
            row(ui, |ui| {
                changed |= skew_slider(ui, t, MonitorDelay::DELAY_MS, &label, " ms").changed();
                if let (Some(ow), Some(rt)) = (one_way, round_trip) {
                    ui.label(tr("Set From Peers"));
                    if ui.button(tr("One-way")).clicked() {
                        (*t, changed) = (ow, true);
                    }
                    if ui.button(tr("Round-trip")).clicked() {
                        (*t, changed) = (rt, true);
                    }
                }
            });
        });
    }
    if changed {
        if link && let Some(&t0) = times.first() {
            times.iter_mut().for_each(|t| *t = t0);
        }
        ctrl.edit(move |s| {
            s.extra.link_mon_delay_times = link;
            for (g, &t) in s.input_groups.iter_mut().zip(&times) {
                g.monitor_delay.enabled = on;
                g.monitor_delay.delay_time_ms = t;
            }
        });
    }
}

fn peers(app: &mut SonobusApp, ui: &mut Ui) {
    let peers = app.ctrl.vm().snapshot.peers.clone();
    for p in &peers {
        ui.push_id(("peer", p.id.0), |ui| {
            egui::Frame::group(ui.style()).show(ui, |ui| peer_strip(app, ui, p));
        });
    }
}

fn peer_strip(app: &mut SonobusApp, ui: &mut Ui, p: &PeerStatus) {
    let e = app.ctrl.engine();
    row(ui, |ui| {
        ui.label(RichText::new(&p.user).strong().size(16.0));
        if !p.connected {
            ui.label(tr("Connecting..."));
        }
        let l = &p.latency;
        let estimate_only = if p.net.is_some() {
            format!(
                "\n{}",
                tr(
                    "Secure sessions show this estimate; the measured test is only in compatible sessions."
                )
            )
        } else {
            String::new()
        };
        ui.label(format!("{}: {:.0} ms", tr("Latency"), l.round_trip_ms))
            .on_hover_text(format!(
                "{} {:.1} ms\n{} {:.1} ms\n{} {:.1} ms\n{} {:.1} ms\n{} {:.1} ms{}{estimate_only}",
                tr("Estimated Round-trip Latency:"),
                l.round_trip_ms,
                tr("Round-trip Network Ping:"),
                l.ping_ms,
                tr("Est. Outgoing:"),
                l.outgoing_ms,
                tr("Est. Incoming:"),
                l.incoming_ms,
                tr("Est. Jitter:"),
                l.jitter_ms,
                if l.legacy {
                    format!(
                        "\n{}",
                        tr("Legacy-mode, ask them \n to install latest version")
                    )
                } else {
                    String::new()
                }
            ));
        ui.label(format!("{} {:.0} ms", tr("Ping"), l.ping_ms));
        // the real round trip of a latency test, beside the estimate at that moment
        if let Some(&(_, measured, est)) = app.ctrl.vm().latency_tests.iter().find(|t| t.0 == p.id)
        {
            ui.label(
                RichText::new(format!(
                    "{} {measured:.0} ms ({} {est:.0} ms)",
                    tr("Round-trip"),
                    tr("Est.")
                ))
                .strong(),
            )
            .on_hover_text(tr("Estimated Round-trip Latency:"));
        } else if let Some(m) = p.measured_round_trip_ms {
            ui.label(RichText::new(format!("{} {m:.0} ms", tr("Round-trip"))).strong());
        }
        if p.blocked_us {
            ui.colored_label(egui::Color32::LIGHT_RED, tr("Other end BLOCKED us"));
        } else if !p.send_active && p.send_allow {
            ui.label(tr("Other end muted us"));
        }
        if !p.send_allow {
            ui.colored_label(egui::Color32::LIGHT_RED, tr("SEND DISABLED"));
        }
        if p.remote_recording {
            ui.label(tr("Recording"));
        }
        // a listener sends nothing to settle
        if p.safety_muted
            && p.net
                .as_ref()
                .is_none_or(|n| n.role != sonobus_engine::PeerRole::Listener)
        {
            ui.label(tr("Muted until the network settles"));
        }
    });
    if let Some(n) = &p.net {
        row(ui, |ui| super::secure::peer_net(ui, n));
    }
    let sends = app.sends();
    row(ui, |ui| {
        let mut level = p.level;
        if gain_slider(ui, &mut level, 2.0, tr("Level: ")).changed() {
            e.set_peer_level(p.id, level);
        }
        let mut muted = !p.recv_allow;
        if toggle(ui, &mut muted, tr("MUTE"))
            .on_hover_text(tr(
                "Toggles receive muting, preventing audio from being heard for this user",
            ))
            .changed()
        {
            e.set_peer_recv_muted(p.id, muted);
        }
        let mut send_off = !p.send_allow;
        if sends && toggle(ui, &mut send_off, tr("Disable Sending")).changed() {
            e.set_peer_send_muted(p.id, send_off);
        }
        let mut solo = p.soloed;
        if toggle(ui, &mut solo, tr("SOLO")).on_hover_text(tr("Listen to only this user, and other soloed users. Alt-click to exclusively solo this user.")).changed() {
            e.set_peer_soloed(p.id, solo);
        }
        let mut lat = p.latency_test_active;
        // secure sessions: the estimate only (no measured test)
        if p.net.is_none()
            && toggle(ui, &mut lat, tr("Latency"))
                .on_hover_text(tr("Measuring actual round-trip latency"))
                .changed()
        {
            if lat {
                e.start_latency_test(p.id);
            } else {
                e.stop_latency_test(p.id);
            }
        }
        meter(ui, &p.meter, &format!("{} {}", p.user, tr("Level")));
    });
    // pans of the first group (the whole peer when it sends one group)
    if let Some(g0) = p.groups.first() {
        let mut g = g0.clone();
        let pan = spec(-1.0, 1.0, 0.0, 1.0, 0.0);
        let changed = ui
            .horizontal_wrapped(|ui| {
                if p.recv_channels == 2 && p.groups.len() == 1 {
                    let a = skew_slider(ui, &mut g.pan_stereo[0], pan, tr("Pan: "), " L").changed();
                    a | skew_slider(ui, &mut g.pan_stereo[1], pan, tr("Pan: "), " R").changed()
                } else if p.groups.len() == 1
                    && let Some(x) = g.pans.first_mut()
                {
                    skew_slider(ui, x, pan, tr("Pan"), "").changed()
                } else {
                    false
                }
            })
            .inner;
        if changed {
            e.update_peer_groups(p.id, move |gs, _| {
                if let Some(x) = gs.first_mut() {
                    *x = g;
                }
            });
        }
    }
    CollapsingHeader::new(tr("Receive Options")).show(ui, |ui| receive_options(app, ui, p));
    if sends {
        CollapsingHeader::new(tr("Send Options")).show(ui, |ui| send_options(app, ui, p));
    }
    if p.groups.len() > 1 {
        CollapsingHeader::new(tr("Channel Groups")).show(ui, |ui| {
            for (gi, g) in p.groups.iter().enumerate() {
                ui.push_id(("pg", gi), |ui| {
                    ui.label(
                        RichText::new(if g.name.is_empty() {
                            format!("{}", gi + 1)
                        } else {
                            g.name.clone()
                        })
                        .strong(),
                    );
                    let mut g = g.clone();
                    if group_controls(ui, &mut g, false) {
                        app.ctrl.engine().update_peer_groups(p.id, move |gs, _| {
                            if let Some(x) = gs.get_mut(gi) {
                                *x = g;
                            }
                        });
                    }
                    if ui.button(tr("Effects")).clicked() {
                        app.page = Page::Effects(FxTarget::Peer(p.id, gi));
                    }
                });
            }
        });
    } else if ui
        .button(tr("Effects"))
        .on_hover_text(tr("Edit effects"))
        .clicked()
    {
        app.page = Page::Effects(FxTarget::Peer(p.id, 0));
    }
}

fn receive_options(app: &mut SonobusApp, ui: &mut Ui, p: &PeerStatus) {
    let e = app.ctrl.engine();
    row(ui, |ui| {
        let mut mode = p.jitter_mode;
        let cur = JITTER_MODES
            .iter()
            .find(|m| m.0 == mode)
            .map_or("?", |m| m.1);
        egui::ComboBox::from_label(tr("Jitter Buffer"))
            .selected_text(tr(cur))
            .show_ui(ui, |ui| {
                for (m, n) in JITTER_MODES {
                    ui.selectable_value(&mut mode, m, tr(n));
                }
            });
        if mode != p.jitter_mode {
            e.set_peer_jitter_mode(p.id, mode);
        }
        let mut ms = p.buffer_ms;
        if skew_slider(
            ui,
            &mut ms,
            spec(0.0, 5000.0, 1.0, 0.25, 20.0),
            tr("Recv Jitter Buffer:"),
            " ms",
        )
        .changed()
        {
            e.set_peer_buffer_ms(p.id, ms);
        }
        ui.add(
            egui::ProgressBar::new(p.fill_ratio.clamp(0.0, 1.0))
                .desired_width(80.0)
                .text(tr("fill")),
        )
        .on_hover_text(tr("Jitter buffer fill"));
        if ui
            .button(tr("Reset Jitter Buffer"))
            .on_hover_text(tr(
                "Resets jitter buffer to the minimum. Hold Alt key to reset for all (with auto).",
            ))
            .clicked()
        {
            if ui.input(|i| i.modifiers.alt) {
                e.reset_all_jitter_buffers();
            } else {
                e.set_peer_buffer_ms(p.id, 0.0);
            }
        }
    });
    row(ui, |ui| {
        ui.label(format!(
            "{} {}  {} {}  {} {}",
            tr("received"),
            p.packets_received,
            tr("dropped"),
            p.packets_dropped,
            tr("resent"),
            p.packets_resent
        ));
        if ui.button(tr("Reset Dropped")).clicked() {
            e.reset_peer_drop_count(p.id);
        }
    });
    row(ui, |ui| {
        ui.label(format!(
            "{} {} ({} ch, {:.0} kbps)",
            tr("Receiving:"),
            p.recv_format.as_deref().unwrap_or("-"),
            p.recv_channels,
            p.recv_bps / 1000.0
        ));
        let mut req = p.requested_format;
        let cur = req.map_or(tr("No Preference"), |i| {
            FORMATS.get(i).map_or("?", |f| f.name)
        });
        egui::ComboBox::from_label(tr("Preferred Recv Quality"))
            .selected_text(cur)
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut req, None, tr("No Preference"));
                for (i, f) in FORMATS.iter().enumerate() {
                    ui.selectable_value(&mut req, Some(i), f.name);
                }
            });
        if req != p.requested_format {
            e.request_peer_send_format(p.id, req);
        }
    });
    ui.label(format!("{} {}", tr("Remote address:"), p.addr));
}

fn send_options(app: &mut SonobusApp, ui: &mut Ui, p: &PeerStatus) {
    let e = app.ctrl.engine();
    row(ui, |ui| {
        let mut f = p.send_format;
        egui::ComboBox::from_label(tr("Send Quality"))
            .selected_text(FORMATS.get(f).map_or("?", |x| x.name))
            .show_ui(ui, |ui| {
                for (i, x) in FORMATS.iter().enumerate() {
                    ui.selectable_value(&mut f, i, x.name);
                }
            });
        if f != p.send_format {
            e.set_peer_send_format(p.id, f);
        }
        if ui.button(tr("Change for all")).clicked() {
            e.set_send_format_for_all(p.send_format);
        }
        ui.label(format!("{:.0} kbps", p.send_bps / 1000.0));
    });
    row(ui, |ui| {
        let mut ch = p.send_channels_override;
        let name = |c: Option<usize>| match c {
            None => tr("Follow Send Channels").to_owned(),
            Some(1) => tr("Send Mono").to_owned(),
            Some(2) => tr("Send Stereo").to_owned(),
            Some(n) => format!("{n} ch"),
        };
        egui::ComboBox::from_label(tr("Send Channels"))
            .selected_text(name(ch))
            .show_ui(ui, |ui| {
                for c in [None, Some(1), Some(2)] {
                    ui.selectable_value(&mut ch, c, name(c));
                }
            });
        if ch != p.send_channels_override {
            e.set_peer_send_channels_override(p.id, ch);
        }
        ui.label(format!("{} {}", p.send_channels, tr("ch")));
    });
    row(ui, |ui| {
        if ui
            .button(tr("Remove"))
            .on_hover_text(tr(
                "Removes user from your own connections, does not affect the whole group",
            ))
            .clicked()
        {
            e.remove_peer(p.id);
        }
        if ui.button(tr("BLOCK")).on_hover_text(tr("BLOCK any connections from this users IP address for yourself, does not affect other users")).clicked() {
            app.confirm_block = Some((p.id, p.user.clone()));
        }
    });
}
