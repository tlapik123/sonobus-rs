//! Connect page (ConnectView.cpp): the protocol choice, then for compatible groups: private
//! group, public groups, recents, direct connect, and hosting a session on this device (secure
//! sessions: [`super::secure`]).

use egui::{RichText, Ui};
use sonobus_engine::Connection;

use super::{row, ConnectTab, Page, SonobusApp, text_field};
use crate::controller::ConnectRequest;
use crate::links;
use crate::tr;

pub fn page(app: &mut SonobusApp, ui: &mut Ui) {
    super::secure::protocol_choice(app, ui);
    if app.protocol == super::Protocol::Secure {
        return super::secure::page(app, ui);
    }
    row(ui, |ui| {
        for (t, name) in [
            (ConnectTab::Group, tr("PRIVATE GROUP")),
            (ConnectTab::Public, tr("PUBLIC GROUPS")),
            (ConnectTab::Recents, tr("RECENTS")),
            (ConnectTab::Direct, tr("DIRECT")),
            (ConnectTab::Host, tr("HOST")),
        ] {
            if t != ConnectTab::Host || app.caps.host_server {
                ui.selectable_value(&mut app.tab, t, name);
            }
        }
    });
    if app.tab == ConnectTab::Host && !app.caps.host_server {
        app.tab = ConnectTab::Group;
    }
    ui.separator();
    match app.tab {
        ConnectTab::Group => group_tab(app, ui),
        ConnectTab::Public => public_tab(app, ui),
        ConnectTab::Recents => recents_tab(app, ui),
        ConnectTab::Direct => direct_tab(app, ui),
        ConnectTab::Host => host_tab(app, ui),
    }
}

/// One `host[:port]` field as upstream (port 10998 unless given), so changing the server cannot
/// leave another server's port behind.
fn server_fields(form: &mut ConnectRequest, ui: &mut Ui) {
    // the text being typed ("host:" parses the same as "host"); dropped when the form changes elsewhere
    let id = egui::Id::unique("connection server");
    let mut text = ui
        .data(|d| d.get_temp::<String>(id))
        .filter(|t| links::parse_host_port(t) == (form.host.clone(), form.port))
        .unwrap_or_else(|| links::host_port_text(&form.host, form.port));
    text_field(ui, tr("Connection Server:"), &mut text, "aoo.sonobus.net", false);
    (form.host, form.port) = links::parse_host_port(&text);
    ui.data_mut(|d| d.insert_temp(id, text));
}

fn user_fields(app: &mut SonobusApp, ui: &mut Ui) {
    text_field(ui, tr("Your Displayed Name:"), &mut app.form.user, tr("Your Name"), false);
}

fn group_fields(app: &mut SonobusApp, ui: &mut Ui) {
    row(ui, |ui| {
        text_field(ui, tr("Group Name:"), &mut app.form.group, tr("enter group name"), false);
        if ui.button(tr("Randomize Group Name")).on_hover_text(tr("Generate a random group name")).clicked() {
            app.form.group = links::random_group_name();
        }
    });
    row(ui, |ui| {
        text_field(ui, tr("Password:"), &mut app.form.group_password, tr("optional"), !app.show_password);
        ui.checkbox(&mut app.show_password, tr("Show Password"));
    });
}

fn group_tab(app: &mut SonobusApp, ui: &mut Ui) {
    ui.label(tr("All who join the same Group will be able to connect with each other."));
    group_fields(app, ui);
    user_fields(app, ui);
    server_fields(&mut app.form, ui);
    row(ui, |ui| {
        let connecting = app.ctrl.vm().connecting;
        let b = ui.add_enabled(!connecting, egui::Button::new(RichText::new(tr("Connect to Group")).strong()));
        if b.clicked() {
            connect(app);
        }
        if connecting {
            ui.spinner();
            ui.label(tr("Connecting..."));
        }
        if ui.button(tr("Copy Share Link")).on_hover_text(tr("Copy connection information to the clipboard to share")).clicked() {
            let f = &app.form;
            ui.ctx().copy_text(links::share_link(&f.host, f.port, f.group.trim(), &f.group_password, f.public));
        }
    });
    row(ui, |ui| {
        // the clipboard arrives as a paste event on a later frame
        if ui.button(tr("Paste Share Link")).on_hover_text(tr("Paste connection information from the clipboard")).clicked() {
            app.awaiting_paste = true;
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::RequestPaste);
        }
        if app.awaiting_paste
            && let Some(text) = ui.input(|i| i.events.iter().find_map(|e| if let egui::Event::Paste(t) = e { Some(t.clone()) } else { None }))
        {
            app.awaiting_paste = false;
            if !app.ctrl.apply_link(&text, &mut app.form) {
                app.ctrl.set_status(tr("No share link in the clipboard"), true);
            }
        }
        let l = ui.label(tr("Link"));
        let r = ui
            .add(egui::TextEdit::singleline(&mut app.link_text).hint_text("sonobus://… / https://go.sonobus.net/sblaunch?…").desired_width(220.0))
            .labelled_by(l.id);
        if r.changed() && app.ctrl.apply_link(&app.link_text, &mut app.form) {
            app.link_text.clear();
        }
    });
    ui.add_space(8.0);
    ui.label(RichText::new(tr("The connection server is only used to help users find each other, no audio passes through it. All audio is sent directly between users (peer to peer).")).weak());
}

fn connect(app: &mut SonobusApp) {
    if app.form.group.trim().is_empty() {
        return app.ctrl.set_status(tr("You need to specify a group name!"), true);
    }
    app.ctrl.connect(app.form.clone());
    if !app.ctrl.vm().status_is_error {
        app.page = Page::Main;
    }
}

fn public_tab(app: &mut SonobusApp, ui: &mut Ui) {
    let snap = &app.ctrl.vm().snapshot;
    let watching = snap.connection == Connection::Connected && snap.group.is_none();
    if !watching {
        user_fields(app, ui);
        server_fields(&mut app.form, ui);
        if ui.button(tr("Connect")).on_hover_text(tr("Public Groups List")).clicked() {
            app.ctrl.connect(ConnectRequest { group: String::new(), ..app.form.clone() });
        }
        if app.ctrl.vm().connecting {
            ui.spinner();
        }
        return;
    }
    ui.label(RichText::new(tr("Active Public Groups")).strong());
    let groups = snap.public_groups.clone();
    if groups.is_empty() {
        ui.label(tr("Select existing group below OR "));
    }
    for (name, users) in groups {
        row(ui, |ui| {
            let unit = if users == 1 { tr(" active user") } else { tr(" active users") };
            if ui.button(&name).on_hover_text(tr("Connect to Group")).clicked() {
                app.ctrl.join_group(&name, "", true);
                app.page = Page::Main;
            }
            ui.label(format!("{users}{unit}"));
        });
    }
    ui.separator();
    ui.label(tr("Choose a descriptive group name that includes geographic information and genre"));
    row(ui, |ui| {
        text_field(ui, tr("Public Group Name"), &mut app.form.group, tr("enter group name"), false);
        if ui.button(tr("Create Group...")).clicked() && !app.form.group.trim().is_empty() {
            let g = app.form.group.trim().to_owned();
            app.ctrl.join_group(&g, "", true);
            app.page = Page::Main;
        }
    });
}

fn recents_tab(app: &mut SonobusApp, ui: &mut Ui) {
    let recents = app.ctrl.vm().state.recents.clone();
    if recents.is_empty() {
        ui.label(tr("Recents List"));
    }
    for r in &recents {
        let label = format!(
            "{}{}  —  {}  @ {}:{}",
            if r.group_is_public { tr("PUBLIC") } else { "" },
            r.group_name,
            r.user_name,
            r.server_host,
            r.server_port
        );
        if ui.button(label).on_hover_text(tr("Connect to Group")).clicked() {
            app.form = ConnectRequest::from_recent(r);
            app.ctrl.connect(app.form.clone());
            app.page = Page::Main;
        }
    }
    if !recents.is_empty() && ui.button(tr("Clear All")).clicked() {
        app.ctrl.edit(|s| s.recents.clear());
    }
}

fn direct_tab(app: &mut SonobusApp, ui: &mut Ui) {
    ui.label(tr("Connect directly to other instances of SonoBus on your local network with the local address that they advertise. This is experimental, using a private group is recommended instead, and works fine on local networks."));
    let port = app.ctrl.vm().snapshot.udp_port;
    let local = app.lan_addresses().first().map_or("127.0.0.1".into(), ToString::to_string);
    ui.label(format!("{} {local}:{port}", tr("Local Address:")));
    row(ui, |ui| {
        text_field(ui, tr("Remote Host:Port"), &mut app.direct_addr, tr("IPaddress:port"), false);
        if ui.button(tr("Direct Connect")).clicked() && !app.direct_addr.trim().is_empty() {
            app.ctrl.connect_direct(&app.direct_addr);
            app.page = Page::Main;
        }
    });
}

fn host_tab(app: &mut SonobusApp, ui: &mut Ui) {
    ui.label(tr("Host a session on this device: others on your network connect to it as their connection server. No internet server is needed."));
    if let Some((port, addrs)) = app.ctrl.vm().hosting.clone() {
        ui.label(RichText::new(tr("Hosting a session. Others enter this as the connection server:")).strong());
        if addrs.is_empty() {
            ui.label(format!("127.0.0.1:{port}  ({})", tr("no network address found")));
        }
        for a in &addrs {
            row(ui, |ui| {
                ui.label(RichText::new(format!("{a}:{port}")).monospace().size(18.0));
                if ui.button(tr("Copy Share Link")).clicked() {
                    let f = &app.form;
                    ui.ctx().copy_text(links::sonobus_link(&a.to_string(), port, f.group.trim(), &f.group_password, false));
                }
            });
        }
        ui.checkbox(&mut app.ctrl.keep_hosting, tr("Keep hosting after disconnecting"));
        if ui.button(tr("Stop Hosting")).clicked() {
            app.ctrl.stop_hosting();
        }
        return;
    }
    group_fields(app, ui);
    user_fields(app, ui);
    row(ui, |ui| {
        let l = ui.label(tr("Port"));
        ui.add(egui::DragValue::new(&mut app.host_port).range(1024..=65535)).labelled_by(l.id);
        if ui.button(RichText::new(tr("Host Session")).strong()).clicked() {
            if app.form.group.trim().is_empty() {
                app.form.group = "session".into();
            }
            if let Err(e) = app.ctrl.host_session(app.host_port, app.form.clone()) {
                app.ctrl.set_status(format!("{}: {e}", tr("Could not host a session")), true);
            }
        }
    });
}
