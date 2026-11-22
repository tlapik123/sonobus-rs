//! Secure sessions (protocol v2) in the UI: the protocol choice on the connect page with what
//! it means, hosting, joining (invite link, this network, by name), remembered sessions, the
//! session panel (invite link, QR code, PIN, members; leaving is the top bar's), the relay choice, and their
//! confirmations.

use egui::{Color32, RichText, Ui};
use sonobus_engine::modern::{AuthKind, RelayChoice, SessionMode, SessionRole};

use super::{Icon, SonobusApp, paint_icon, row, text_field};
use crate::secure::{self, How, InvitePreview, JoinMethod};
use crate::tr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    /// Legacy SonoBus 1.x groups.
    Compatible,
    Secure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecureTab {
    Host,
    Join,
    Remembered,
}

/// The join forms.
#[derive(Debug, Clone, Default)]
pub struct JoinForm {
    pub invite: String,
    pub remember: bool,
    /// The LAN session picked (its host's endpoint id) and its PIN or password.
    pub lan_pick: Option<[u8; 32]>,
    pub lan_secret: String,
    /// By name: rendezvous server, session name, password.
    pub server: String,
    pub session: String,
    pub password: String,
}

const CALM: Color32 = Color32::from_rgb(110, 160, 230);
const SECURE: Color32 = Color32::from_rgb(70, 170, 100);
const OPEN: Color32 = Color32::from_rgb(220, 150, 60);

/// The protocol choice at the top of the connect page, with what each one means.
pub fn protocol_choice(app: &mut SonobusApp, ui: &mut Ui) {
    if !app.caps.secure {
        app.protocol = Protocol::Compatible;
        return;
    }
    row(ui, |ui| {
        ui.selectable_value(
            &mut app.protocol,
            Protocol::Compatible,
            tr("Compatible (SonoBus 1.x)"),
        );
        ui.selectable_value(
            &mut app.protocol,
            Protocol::Secure,
            tr("Secure (this app only)"),
        );
    });
    let text = match app.protocol {
        Protocol::Compatible => tr(
            "Works with stock SonoBus. Not encrypted: anyone on the network path can listen to the audio and read the chat.",
        ),
        Protocol::Secure => tr(
            "End-to-end encrypted: only the people in the session can hear it. Works only between copies of this app, not with stock SonoBus.",
        ),
    };
    ui.label(text);
    ui.label(RichText::new(tr("A compatible group and a secure session with the same name are different things: they never meet.")).weak());
    ui.separator();
}

/// The "Secure" part of the connect page.
pub fn page(app: &mut SonobusApp, ui: &mut Ui) {
    if app.ctrl.vm().secure.is_some() {
        session_panel(app, ui, true);
        return;
    }
    if app.ctrl.vm().secure_starting {
        ui.horizontal(|ui| {
            ui.spinner();
            ui.label(tr("Starting the secure session..."));
        });
    }
    retry_notice(app, ui);
    row(ui, |ui| {
        for (t, name) in [
            (SecureTab::Host, tr("Host a secure session")),
            (SecureTab::Join, tr("Join")),
            (SecureTab::Remembered, tr("Remembered")),
        ] {
            ui.selectable_value(&mut app.secure_tab, t, name);
        }
    });
    ui.separator();
    match app.secure_tab {
        SecureTab::Host => host_tab(app, ui),
        SecureTab::Join => join_tab(app, ui),
        SecureTab::Remembered => remembered_tab(app, ui),
    }
}

/// "Try again in N s" while a host's back-off refuses us.
fn retry_notice(app: &mut SonobusApp, ui: &mut Ui) {
    let Some(t) = app.ctrl.vm().secure_retry_at else {
        return;
    };
    let left = t
        .saturating_duration_since(std::time::Instant::now())
        .as_secs_f32()
        .ceil() as u64;
    if left > 0 {
        ui.label(RichText::new(format!("{} {left} s", tr("Try again in"))).color(CALM));
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
}

fn name_field(app: &mut SonobusApp, ui: &mut Ui) {
    text_field(
        ui,
        tr("Your Displayed Name:"),
        &mut app.form.user,
        tr("Your Name"),
        false,
    );
}

fn host_tab(app: &mut SonobusApp, ui: &mut Ui) {
    name_field(app, ui);
    let f = &mut app.host_form;
    text_field(
        ui,
        tr("Session name"),
        &mut f.session,
        tr("enter session name"),
        false,
    );
    row(ui, |ui| {
        ui.radio_value(&mut f.broadcast, false, tr("Jam: everyone plays"));
        ui.radio_value(&mut f.broadcast, true, tr("Share sound: the others listen"));
    });
    if app.host_form.broadcast {
        row(ui, |ui| {
            let l = ui.label(tr("Most listeners (0: no limit)"));
            ui.add(egui::DragValue::new(&mut app.host_form.max_listeners).range(0..=64))
                .labelled_by(l.id);
        });
        sync_listeners(app, ui);
    }
    let f = &mut app.host_form;
    ui.add_space(4.0);
    ui.label(RichText::new(tr("How others join")).strong());
    ui.label(tr("Anyone with the invite link (or its QR code) can join."));
    row(ui, |ui| {
        text_field(
            ui,
            tr("Password:"),
            &mut f.password,
            tr("optional"),
            !app.show_password,
        );
        ui.checkbox(&mut app.show_password, tr("Show Password"));
    });
    if !f.password.is_empty() {
        ui.label(secure::password_hint(&f.password));
    }
    ui.label(RichText::new(tr("A password is only as strong as it is against online guessing: joins allow about 720 guesses a day in total.")).weak());
    ui.checkbox(&mut f.pin, tr("PIN for people on this network (6 digits)"));
    ui.checkbox(&mut f.lan, tr("Advertise on the local network"));
    ui.checkbox(
        &mut f.rendezvous_here,
        tr("Also run rendezvous on this device (join by name and password)"),
    );
    text_field(
        ui,
        tr("Rendezvous server (optional)"),
        &mut f.register_at,
        "sonobus2://rdv/…",
        false,
    );
    if !f.register_at.trim().is_empty()
        && let Err(e) = sonobus_net::RdvAddr::parse(&f.register_at)
    {
        ui.label(RichText::new(crate::clean(&e, 200)).color(OPEN));
    }
    ui.checkbox(
        &mut f.remember,
        tr("Remember this session (keeps its invite link on this device)"),
    );
    relay_choice(app, ui);
    let problem = app.host_form.problem();
    if let Some(p) = problem {
        ui.label(RichText::new(tr(p)).color(OPEN));
    }
    let starting = app.ctrl.vm().secure_starting;
    if ui
        .add_enabled(
            problem.is_none() && !starting,
            egui::Button::new(RichText::new(tr("Host Secure Session")).strong()),
        )
        .clicked()
    {
        app.ctrl.host_secure(&app.form.user, &app.host_form.clone());
    }
}

/// Broadcast host: "Sync listeners" and its delay (remembered; applies at once while hosting).
fn sync_listeners(app: &mut SonobusApp, ui: &mut Ui) {
    let p = &app.ctrl.vm().secure_prefs;
    let (was_on, was_ms) = (p.playout_sync, p.playout_ms);
    let (mut on, mut ms) = (was_on, was_ms);
    row(ui, |ui| {
        ui.checkbox(&mut on, tr("Sync listeners"))
            .on_hover_text(tr("Every listener plays the sound at the same moment, this long after you play it. A listener whose connection needs longer plays late."));
        let l = ui.label(tr("Delay (ms)"));
        ui.add_enabled(
            on,
            egui::DragValue::new(&mut ms)
                .range(secure::MIN_PLAYOUT_MS..=sonobus_engine::modern::MAX_PLAYOUT_MS),
        )
        .labelled_by(l.id);
    });
    if (on, ms) != (was_on, was_ms) {
        app.ctrl.set_playout_sync(on, ms);
    }
}

fn join_tab(app: &mut SonobusApp, ui: &mut Ui) {
    name_field(app, ui);
    // ---- invite link
    row(ui, |ui| {
        let l = ui.label(tr("Invite link"));
        ui.add(
            egui::TextEdit::singleline(&mut app.join.invite)
                .hint_text("sonobus2://join/…")
                .desired_width(260.0),
        )
        .labelled_by(l.id);
        if ui.button(tr("Paste")).clicked() {
            app.awaiting_invite_paste = true;
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        }
        if app.awaiting_invite_paste
            && let Some(t) = ui.input(|i| {
                i.events.iter().find_map(|e| {
                    if let egui::Event::Paste(t) = e {
                        Some(t.clone())
                    } else {
                        None
                    }
                })
            })
        {
            app.awaiting_invite_paste = false;
            app.join.invite = t.trim().to_owned();
        }
    });
    if !app.join.invite.trim().is_empty() {
        match InvitePreview::parse(&app.join.invite) {
            Some(p) => {
                preview(ui, &p);
                ui.checkbox(
                    &mut app.join.remember,
                    tr("Remember this session (keeps its invite link on this device)"),
                );
                if ui
                    .add_enabled(
                        !app.ctrl.vm().secure_starting,
                        egui::Button::new(RichText::new(tr("Join Session")).strong()),
                    )
                    .clicked()
                {
                    app.ctrl.join_secure(
                        &app.form.user,
                        JoinMethod::Invite(p.link),
                        app.join.remember,
                    );
                }
            }
            None => {
                ui.label(
                    RichText::new(tr("This is not a secure session invite link.")).color(OPEN),
                );
            }
        }
    }
    ui.separator();
    // ---- this network
    ui.label(RichText::new(tr("On this network")).strong());
    if !app.ctrl.vm().lan_browsing && !app.lan_tried {
        app.lan_tried = true;
        app.ctrl.lan_browse(true);
    }
    let found = app.ctrl.vm().lan_sessions.clone();
    if found.is_empty() {
        ui.label(RichText::new(tr("No secure sessions found on this network yet.")).weak());
    }
    for (i, d) in found.iter().enumerate() {
        ui.push_id(("lan", i), |ui| {
            let kind = match d.auth {
                AuthKind::Pin => tr("PIN"),
                AuthKind::Password => tr("Password"),
            };
            row(ui, |ui| {
                ui.label(RichText::new(&d.session).strong());
                ui.label(format!("{} {}", tr("host"), secure::short_id(&d.host.id)));
                ui.label(RichText::new(kind).weak());
                if ui.button(tr("Join...")).clicked() {
                    app.join.lan_pick = Some(d.host.id);
                    app.join.lan_secret.clear();
                }
            });
            if app.join.lan_pick == Some(d.host.id) {
                row(ui, |ui| {
                    let l = ui.label(kind);
                    let big = egui::TextEdit::singleline(&mut app.join.lan_secret)
                        .password(d.auth == AuthKind::Password)
                        .desired_width(160.0);
                    let r = ui.add(big).labelled_by(l.id);
                    let go = ui
                        .button(RichText::new(tr("Join Session")).strong())
                        .clicked()
                        || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if go && !app.join.lan_secret.trim().is_empty() {
                        app.ctrl.join_secure(
                            &app.form.user,
                            JoinMethod::Lan(d.clone(), app.join.lan_secret.clone()),
                            false,
                        );
                        app.join.lan_pick = None;
                    }
                });
            }
        });
    }
    ui.separator();
    // ---- by name
    ui.label(RichText::new(tr("By name")).strong());
    if app.join.server.is_empty() {
        app.join
            .server
            .clone_from(&app.ctrl.vm().secure_prefs.rendezvous);
    }
    let j = &mut app.join;
    text_field(
        ui,
        tr("Rendezvous server"),
        &mut j.server,
        "sonobus2://rdv/…",
        false,
    );
    text_field(
        ui,
        tr("Session name"),
        &mut j.session,
        tr("enter session name"),
        false,
    );
    text_field(ui, tr("Password:"), &mut j.password, "", true);
    let ok = sonobus_net::RdvAddr::parse(&j.server).is_ok()
        && !j.session.trim().is_empty()
        && !j.password.is_empty();
    if ui
        .add_enabled(
            ok && !app.ctrl.vm().secure_starting,
            egui::Button::new(tr("Join by Name")),
        )
        .clicked()
    {
        let m = JoinMethod::Name {
            server: j.server.clone(),
            session: j.session.clone(),
            password: j.password.clone(),
        };
        app.ctrl.join_secure(&app.form.user, m, false);
    }
    relay_choice(app, ui);
}

/// What an invite link names (shown before joining).
fn preview(ui: &mut Ui, p: &InvitePreview) {
    egui::Frame::group(ui.style()).show(ui, |ui| {
        ui.label(format!("{}: {}", tr("Session name"), p.session));
        ui.label(format!("{} {}", tr("Host device:"), p.host));
        ui.label(format!(
            "{} {}",
            tr("Relay:"),
            p.relay.as_deref().unwrap_or(tr("none"))
        ));
        ui.label(if p.broadcast {
            tr("Share sound: the others listen")
        } else {
            tr("Jam: everyone plays")
        });
    });
}

fn remembered_tab(app: &mut SonobusApp, ui: &mut Ui) {
    let recents = app.ctrl.vm().secure_prefs.recents.clone();
    if recents.is_empty() {
        ui.label(RichText::new(tr("No remembered sessions.")).weak());
    }
    let mut forget = None;
    for (i, r) in recents.iter().enumerate() {
        ui.push_id(("recent", i), |ui| {
            row(ui, |ui| {
                let mode = if r.broadcast {
                    tr("Share sound: the others listen")
                } else {
                    tr("Jam: everyone plays")
                };
                ui.label(RichText::new(&r.name).strong())
                    .on_hover_text(mode);
                match r.how {
                    How::Hosted => {
                        if ui.button(tr("Host again")).clicked() {
                            (app.host_form.session, app.host_form.broadcast) =
                                (r.name.clone(), r.broadcast);
                            app.host_form.remember = r.invite.is_some();
                            app.secure_tab = SecureTab::Host;
                        }
                    }
                    How::Invite => {
                        if let Some(link) = &r.invite
                            && ui.button(tr("Join Session")).clicked()
                        {
                            app.ctrl.join_secure(
                                &app.form.user,
                                JoinMethod::Invite(link.clone()),
                                true,
                            );
                        }
                    }
                    How::Lan => {
                        if ui.button(tr("On this network")).clicked() {
                            app.secure_tab = SecureTab::Join;
                        }
                    }
                    How::Name => {
                        if ui.button(tr("By name")).clicked() {
                            (app.join.session, app.join.server) =
                                (r.name.clone(), r.server.clone().unwrap_or_default());
                            app.secure_tab = SecureTab::Join;
                        }
                    }
                }
                if ui.button(tr("Forget")).clicked() {
                    forget = Some(i);
                }
            });
        });
    }
    if let Some(i) = forget {
        app.ctrl.forget_secure_recent(i);
    }
}

/// The relay choice (secure sessions), remembered.
pub fn relay_choice(app: &mut SonobusApp, ui: &mut Ui) {
    egui::CollapsingHeader::new(tr("Relays")).id_salt("relays").show(ui, |ui| {
        let cur = app.ctrl.vm().secure_prefs.relay.clone();
        let mut kind = match &cur {
            RelayChoice::N0Default => 0,
            RelayChoice::Custom(_) => 1,
            RelayChoice::None => 2,
        };
        if let (RelayChoice::Custom(v), true) = (&cur, app.relay_url.is_empty()) {
            app.relay_url = v.first().cloned().unwrap_or_default();
        }
        row(ui, |ui| {
            ui.radio_value(&mut kind, 0, tr("Public relays (n0)"));
            ui.radio_value(&mut kind, 1, tr("My relay server"));
            ui.radio_value(&mut kind, 2, tr("No relays"));
        });
        let mut want = match kind {
            0 => RelayChoice::N0Default,
            2 => RelayChoice::None,
            _ => RelayChoice::Custom(vec![app.relay_url.clone()]),
        };
        match kind {
            0 => {
                ui.label(tr("n0 describes these as best-effort; they see who connects to whom, never your audio."));
            }
            1 => {
                text_field(ui, tr("Relay URL"), &mut app.relay_url, "https://…", false);
                match crate::links::relay_choice(&app.relay_url) {
                    Ok(r @ RelayChoice::Custom(_)) => want = r,
                    _ => {
                        ui.label(RichText::new(tr("Enter an http(s) URL.")).color(OPEN));
                        want = cur.clone(); // nothing usable yet
                    }
                }
            }
            _ => {
                ui.label(tr("Direct connections only: two people who are both behind home routers then cannot connect over the internet."));
            }
        }
        if want != cur {
            app.ctrl.set_secure_relay(want);
        }
    });
}

/// The QR code of an invite link: modules as rectangles, dark on white with the quiet zone.
pub fn qr_code(ui: &mut Ui, link: &str) {
    let Some((w, m)) = secure::qr_modules(link) else {
        return;
    };
    let total = (w + 2 * secure::QUIET) as f32;
    let side = ui.available_width().clamp(160.0, 320.0);
    let module = (side / total).floor().max(1.0);
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(module * total, module * total),
        egui::Sense::hover(),
    );
    let p = ui.painter();
    p.rect_filled(rect, 0.0, Color32::WHITE);
    for (x, y, s) in secure::qr_squares(w, &m, (rect.min.x, rect.min.y), module) {
        p.rect_filled(
            egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(s, s)),
            0.0,
            Color32::BLACK,
        );
    }
    resp.widget_info(|| {
        egui::WidgetInfo::labeled(egui::Role::Image, true, tr("QR code of the invite link"))
    });
}

fn role_name(r: SessionRole) -> &'static str {
    match r {
        SessionRole::Host => tr("Host"),
        SessionRole::Member => tr("Member"),
        SessionRole::Listener => tr("Listener"),
    }
}

/// The lock badge: secure session, or unencrypted (compatible).
pub fn badge(ui: &mut Ui, secure: bool) {
    let (text, colour) = if secure {
        (tr("Secure session"), SECURE)
    } else {
        (tr("Unencrypted (compatible)"), OPEN)
    };
    ui.horizontal(|ui| {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 16.0), egui::Sense::hover());
        paint_icon(
            ui.painter(),
            rect.center(),
            if secure { Icon::Lock } else { Icon::Unlock },
            colour,
        );
        ui.label(RichText::new(text).color(colour).strong());
    });
}

/// The running secure session: invite link, QR code, PIN, rendezvous address, members.
/// `title`: with the badge and the session name (the main page has them in its header).
pub fn session_panel(app: &mut SonobusApp, ui: &mut Ui, title: bool) {
    let Some(s) = app.ctrl.vm().secure.clone() else {
        return;
    };
    let mode = if s.mode == SessionMode::Broadcast {
        tr("Share sound: the others listen")
    } else {
        tr("Jam: everyone plays")
    };
    if title {
        badge(ui, true);
        ui.label(RichText::new(&s.session).heading());
    }
    ui.label(format!("{} | {mode}", role_name(s.role)));
    ui.label(
        RichText::new(format!(
            "{} {}",
            tr("This device:"),
            secure::short_id(&s.endpoint_id)
        ))
        .weak(),
    );
    if s.role == SessionRole::Host {
        match &s.invite {
            Some(link) => {
                let short: String = link.chars().take(40).collect();
                ui.label(RichText::new(format!("{short}…")).monospace());
                row(ui, |ui| {
                    if ui
                        .button(RichText::new(tr("Copy Invite Link")).strong())
                        .clicked()
                    {
                        ui.ctx().copy_text(link.clone());
                        app.ctrl.set_status(tr("Invite link copied. Anyone who has it can join: share it only with them."), false);
                    }
                    ui.checkbox(&mut app.show_qr, tr("Show QR Code"));
                    if ui
                        .button(tr("New Invite Link"))
                        .on_hover_text(tr("New invite link: earlier links no longer work."))
                        .clicked()
                    {
                        app.ctrl.rotate_invite(None);
                    }
                });
                if app.show_qr {
                    qr_code(ui, link);
                }
            }
            None => {
                ui.spinner();
            }
        }
        if let Some(pin) = &s.pin {
            row(ui, |ui| {
                ui.label(tr("PIN"));
                ui.label(
                    RichText::new(format!(
                        "{} {}",
                        &pin[..3.min(pin.len())],
                        pin.get(3..).unwrap_or("")
                    ))
                    .monospace()
                    .size(32.0)
                    .strong(),
                );
                if ui.button(tr("New PIN")).clicked() {
                    app.ctrl.new_pin();
                }
            });
            if let Some(t) = app.ctrl.vm().pin_invalidated {
                let mins = t.elapsed().as_secs() / 60;
                ui.label(
                    RichText::new(format!(
                        "{} ({mins} min)",
                        tr("Wrong guesses invalidated this PIN")
                    ))
                    .color(OPEN),
                );
            }
        }
        if let Some(r) = &s.rendezvous {
            let text = secure::rdv_text(r);
            row(ui, |ui| {
                ui.label(tr("Others join by name through:"));
                if ui.button(tr("Copy")).on_hover_text(&text).clicked() {
                    ui.ctx().copy_text(text.clone());
                }
            });
        }
        if s.failed_joins > 0 {
            ui.label(
                RichText::new(format!(
                    "{} {}",
                    tr("Failed join attempts:"),
                    s.failed_joins
                ))
                .color(CALM),
            );
        }
        if s.mode == SessionMode::Broadcast {
            sync_listeners(app, ui);
        }
    }
    if let Some(r) = &s.relay {
        ui.label(RichText::new(format!("{} {}", tr("Relay:"), secure::relay_host(r))).weak());
    }
    members(app, ui, s.role == SessionRole::Host);
    blocked(app, ui);
    retry_notice(app, ui);
}

/// Devices blocked in this session (removed or blocked), each with Unblock.
fn blocked(app: &mut SonobusApp, ui: &mut Ui) {
    let list = app.ctrl.vm().blocked_endpoints.clone();
    if list.is_empty() {
        return;
    }
    ui.label(RichText::new(tr("Blocked devices")).strong());
    for id in list {
        let short = secure::short_id(&id);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("ID {short}")).monospace().weak());
            if ui
                .button(tr("Unblock"))
                .on_hover_text(format!("{} {short}", tr("Unblock")))
                .clicked()
            {
                app.ctrl.unblock_endpoint(id);
            }
        });
    }
}

/// The people in the session: proven device id, path, host marker; the host can remove them.
fn members(app: &mut SonobusApp, ui: &mut Ui, host: bool) {
    let peers = app.ctrl.vm().snapshot.peers.clone();
    for p in &peers {
        let Some(n) = &p.net else { continue };
        ui.push_id(("member", p.id.0), |ui| {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(&p.user).strong());
                peer_net(ui, n);
                if host && ui.button(tr("Remove from session")).clicked() {
                    app.confirm_remove = Some((p.id, p.user.clone()));
                }
            });
        });
    }
}

/// A secure peer's connection: host marker, direct or relayed, round trip, proven device id.
pub fn peer_net(ui: &mut Ui, n: &sonobus_engine::PeerNet) {
    if n.is_invite_host {
        ui.label(RichText::new(tr("Host")).color(SECURE).strong())
            .on_hover_text(tr("The host named by the invite link (verified)"));
    } else if n.role == sonobus_engine::PeerRole::Listener {
        ui.label(tr("Listener"));
    }
    let path = match (n.direct, &n.relay) {
        (true, _) => tr("direct connection").to_owned(),
        (false, Some(r)) => format!("{} {}", tr("relayed via"), secure::relay_host(r)),
        (false, None) => tr("relayed").to_owned(),
    };
    ui.label(format!("{path}, {:.0} ms", n.rtt_ms));
    if let Some(e) = n.playout_error_ms {
        ui.label(format!("{} {e:+.1} ms", tr("sync")))
            .on_hover_text(tr(
                "How early (+) or late (-) this device plays against the host's sync target",
            ));
    }
    ui.label(
        RichText::new(format!("ID {}", secure::short_id(&n.endpoint_id)))
            .monospace()
            .weak(),
    )
    .on_hover_text(tr(
        "This device's proven identity: names can be chosen freely, this cannot.",
    ));
}

/// Confirmations of secure sessions: an invite link from outside, removing a member, a new
/// device identity. Returns true while one is shown.
pub fn prompts(app: &mut SonobusApp, ctx: &egui::Context) -> bool {
    if let Some(p) = app.ctrl.vm().pending_invite.clone() {
        let mut answer = None;
        egui::Modal::new(egui::Id::unique("pending_invite")).show(ctx, |ui| {
            ui.label(tr("A link asks to join a secure session. Join it?"));
            preview(ui, &p);
            ui.horizontal(|ui| {
                if ui.button(tr("Join Session")).clicked() {
                    answer = Some(true);
                }
                if ui.button(tr("Cancel")).clicked() {
                    answer = Some(false);
                }
            });
        });
        if let Some(a) = answer {
            app.ctrl.answer_pending_invite(&app.form.user, a);
            if a {
                (app.protocol, app.page) = (Protocol::Secure, super::Page::Connect);
            }
        }
        return true;
    }
    if let Some((id, user)) = app.confirm_remove.clone() {
        let text = format!(
            "{user}: {}",
            tr(
                "remove from the session? The invite link changes: every link shared so far stops working (share the new one). Everyone else stays."
            )
        );
        if let Some(a) = confirm(ctx, "confirm_remove", &text, tr("Remove")) {
            if a {
                app.ctrl.rotate_invite(Some(id));
            }
            app.confirm_remove = None;
        }
        return true;
    }
    if app.confirm_reset_identity {
        let text = tr(
            "Others recognise this device by its identity in secure sessions. A new one makes the invite links you shared stop working, and others see a new device.",
        );
        if let Some(a) = confirm(
            ctx,
            "confirm_reset_identity",
            text,
            tr("Reset device identity"),
        ) {
            if a {
                app.ctrl.reset_identity();
            }
            app.confirm_reset_identity = false;
        }
        return true;
    }
    false
}

fn confirm(ctx: &egui::Context, id: &str, text: &str, yes: &str) -> Option<bool> {
    let mut answer = None;
    egui::Modal::new(egui::Id::unique(id)).show(ctx, |ui| {
        ui.label(text);
        ui.horizontal(|ui| {
            if ui.button(yes).clicked() {
                answer = Some(true);
            }
            if ui.button(tr("Cancel")).clicked() {
                answer = Some(false);
            }
        });
    });
    answer
}

/// Options: the relay choice and "Reset device identity".
pub fn options(app: &mut SonobusApp, ui: &mut Ui) {
    relay_choice(app, ui);
    if ui.button(tr("Reset device identity...")).clicked() {
        app.confirm_reset_identity = true;
    }
}
