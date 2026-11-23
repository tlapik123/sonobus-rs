//! Group tools: latency match (LatencyMatchView), Receive Sync, suggest a new group
//! (SuggestNewGroupView), the prompts for incoming requests, and the patchbay.

use egui::{RichText, Ui};

use super::{Page, SonobusApp, row, text_field};
use crate::tr;

/// The "Suggest New Group" form.
#[derive(Debug, Clone, Default)]
pub struct SuggestForm {
    pub group: String,
    pub password: String,
    pub public: bool,
    /// Users to invite.
    pub users: Vec<String>,
}

fn back(app: &mut SonobusApp, ui: &mut Ui) -> bool {
    let b = ui.button(format!("< {}", tr("Back"))).clicked();
    if b {
        app.page = Page::Main;
    }
    b
}

pub fn latency_match(app: &mut SonobusApp, ui: &mut Ui) {
    if back(app, ui) {
        app.ctrl.end_latency_match();
        return;
    }
    ui.heading(tr("Group Latency Match"));
    let Some((entries, ready, default)) = app.ctrl.vm().latency_match.clone() else {
        return;
    };
    egui::Grid::new("latmatch")
        .striped(true)
        .num_columns(3)
        .show(ui, |ui| {
            ui.label(RichText::new(tr("From")).strong());
            ui.label(RichText::new(tr("To")).strong());
            ui.label(RichText::new(tr("Latency (ms)")).strong());
            ui.end_row();
            for e in &entries {
                ui.label(&e.src_name);
                ui.label(&e.dest_name);
                ui.label(format!("{:.0}", e.latency_ms));
                ui.end_row();
            }
        });
    if !ready {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(tr("Waiting for the latencies of all users..."));
        });
    }
    let mut target = app.latency_target.unwrap_or(default);
    if ui
        .add(
            egui::Slider::new(&mut target, 0.0..=1000.0)
                .text(tr("Target Latency"))
                .suffix(" ms")
                .step_by(1.0),
        )
        .changed()
    {
        app.latency_target = Some(target);
    }
    if ui
        .add_enabled(
            ready,
            egui::Button::new(RichText::new(tr("Request Group Match")).strong()),
        )
        .clicked()
    {
        app.ctrl.send_latency_match(target);
        app.page = Page::Main;
    }
    ui.separator();
    receive_sync(app, ui);
}

/// "Receive Sync": the same total receive latency from every peer, plus a padding.
pub fn receive_sync(app: &mut SonobusApp, ui: &mut Ui) {
    row(ui, |ui| {
        if ui
            .button(tr("Receive Sync"))
            .on_hover_text(tr(
                "Sets every user's jitter buffer so that all arrive with the same latency",
            ))
            .clicked()
        {
            app.ctrl.engine().receive_sync(app.sync_padding);
        }
        ui.add(
            egui::Slider::new(&mut app.sync_padding, 0.0..=500.0)
                .text(tr("Sync Receive Padding"))
                .suffix(" ms")
                .step_by(1.0),
        );
    });
}

pub fn suggest_group(app: &mut SonobusApp, ui: &mut Ui) {
    if back(app, ui) {
        return;
    }
    ui.heading(tr("Suggest New Group"));
    let f = &mut app.suggest;
    row(ui, |ui| {
        text_field(ui, tr("Group"), &mut f.group, tr("enter group name"), false);
        if ui
            .button(tr("Randomize Group Name"))
            .on_hover_text(tr("Generate a random group name"))
            .clicked()
        {
            f.group = crate::links::random_group_name();
        }
    });
    row(ui, |ui| {
        text_field(ui, tr("Password"), &mut f.password, tr("optional"), true);
        ui.checkbox(&mut f.public, tr("Public"));
    });
    let peers: Vec<String> = app
        .ctrl
        .vm()
        .snapshot
        .peers
        .iter()
        .map(|p| p.user.clone())
        .collect();
    row(ui, |ui| {
        if ui.button(tr("Select All")).clicked() {
            f.users.clone_from(&peers);
        }
        if ui.button(tr("Select None")).clicked() {
            f.users.clear();
        }
    });
    for p in &peers {
        let mut on = f.users.contains(p);
        if ui.checkbox(&mut on, p).changed() {
            if on {
                f.users.push(p.clone())
            } else {
                f.users.retain(|u| u != p)
            }
        }
    }
    let f = app.suggest.clone();
    let ok = !f.group.trim().is_empty() && f.users.iter().any(|u| peers.contains(u));
    if ui
        .add_enabled(
            ok,
            egui::Button::new(RichText::new(tr("Suggest and Connect to Group")).strong()),
        )
        .clicked()
    {
        let users: Vec<String> = f
            .users
            .iter()
            .filter(|u| peers.contains(u))
            .cloned()
            .collect();
        app.ctrl
            .suggest_group(f.group.trim(), &f.password, f.public, &users);
        app.page = Page::Main;
    }
}

/// A yes/no dialog; `Some(answer)` once answered.
fn confirm(ctx: &egui::Context, id: &str, text: &str, yes: &str, no: &str) -> Option<bool> {
    let mut answer = None;
    egui::Modal::new(egui::Id::unique(id)).show(ctx, |ui| {
        ui.label(text);
        ui.horizontal(|ui| {
            if ui.button(yes).clicked() {
                answer = Some(true);
            }
            if ui.button(no).clicked() {
                answer = Some(false);
            }
        });
    });
    answer
}

/// Incoming requests (a latency match, a group suggestion: ApproveComponent in the editor) and
/// confirmations (a link to an unknown server, blocking a user, a setup change that stops the
/// recording).
pub fn prompts(app: &mut SonobusApp, ctx: &egui::Context) {
    if let Some(r) = app.ctrl.vm().pending_link.clone() {
        let text = format!(
            "{}\n{}: {}:{}\n{}: {}",
            tr(
                "A link asks to connect to a server you have not used before. It will see your address and user name."
            ),
            tr("Server"),
            crate::clean(&r.host, 100),
            r.port,
            tr("Group"),
            crate::clean(&r.group, 100)
        );
        if let Some(a) = confirm(ctx, "pending_link", &text, tr("Connect"), tr("Cancel")) {
            if a {
                app.page = Page::Main;
            }
            app.ctrl.answer_pending_link(a);
        }
    } else if let Some((id, user)) = app.confirm_block.clone() {
        let text = format!(
            "{} {user}?",
            tr(
                "BLOCK any connections from this users IP address for yourself, does not affect other users"
            )
        );
        if let Some(a) = confirm(ctx, "confirm_block", &text, tr("BLOCK"), tr("Cancel")) {
            if a {
                app.ctrl.engine().block_peer(id);
            }
            app.confirm_block = None;
        }
    } else if let Some(setup) = app.confirm_setup.clone() {
        let text = tr("Changing the audio setup stops the recording. Change it?");
        if let Some(a) = confirm(ctx, "confirm_setup", text, tr("Apply"), tr("Cancel")) {
            if a && let Err(e) = app.ctrl.set_audio_setup(setup) {
                app.ctrl
                    .set_status(format!("{}: {e}", tr("Audio setup failed")), true);
            }
            app.confirm_setup = None;
        }
    } else if let Some((from, ms)) = app.ctrl.vm().latency_request.clone() {
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("latency_request")).show(ctx, |ui| {
            ui.label(format!(
                "{from} {} {ms:.0} ms",
                tr("requests to use a matched group latency of:")
            ));
            ui.horizontal(|ui| {
                if ui.button(tr("Match Latency")).clicked() {
                    answer = Some(true);
                }
                if ui.button(tr("Ignore")).clicked() {
                    answer = Some(false);
                }
            });
        });
        if let Some(a) = answer {
            app.ctrl.answer_latency_request(a);
        }
    } else if let Some(g) = app.ctrl.vm().group_suggestion.clone() {
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("group_suggestion")).show(ctx, |ui| {
            ui.label(if g.public {
                tr("Requested to join a new public group:")
            } else {
                tr("Requested to join a new private group:")
            });
            ui.label(format!(
                "   {}{}\n   {}{}\n   {}{}",
                tr("From: "),
                g.user,
                tr("New Group: "),
                g.group,
                tr("With: "),
                g.others.join(", ")
            ));
            ui.horizontal(|ui| {
                if ui.button(tr("Connect To Group")).clicked() {
                    answer = Some(true);
                }
                if ui.button(tr("Ignore")).clicked() {
                    answer = Some(false);
                }
            });
        });
        if let Some(a) = answer {
            if a {
                app.form.group.clone_from(&g.group);
                app.form.group_password.clone_from(&g.group_password);
                app.form.public = g.public;
            }
            app.ctrl.answer_group_suggestion(a);
        }
    }
}

/// Patchbay (PatchMatrixView): route one user's audio to another; rows send, columns receive.
pub fn patchbay(app: &mut SonobusApp, ui: &mut Ui) {
    let peers: Vec<(sonobus_engine::PeerId, String)> = app
        .ctrl
        .vm()
        .snapshot
        .peers
        .iter()
        .map(|p| (p.id, p.user.clone()))
        .collect();
    ui.label(
        RichText::new(tr(
            "Send the audio of the user in a row to the user in a column as well.",
        ))
        .weak(),
    );
    egui::ScrollArea::horizontal()
        .id_salt("patchbay")
        .show(ui, |ui| {
            egui::Grid::new("patch").striped(true).show(ui, |ui| {
                ui.label("");
                for (_, to) in &peers {
                    ui.label(RichText::new(to).strong());
                }
                ui.end_row();
                for (from, fname) in &peers {
                    ui.label(RichText::new(fname).strong());
                    for (to, tname) in &peers {
                        if from == to {
                            ui.label("-");
                            continue;
                        }
                        let e = app.ctrl.engine();
                        let mut on = e.patch(*from, *to);
                        let r = ui.checkbox(&mut on, "");
                        let name = format!("{fname} -> {tname}");
                        r.widget_info(|| {
                            egui::WidgetInfo::selected(egui::Role::CheckBox, true, on, &name)
                        });
                        if r.on_hover_text(&name).changed() {
                            e.set_patch(*from, *to, on);
                        }
                    }
                    ui.end_row();
                }
            });
        });
}
