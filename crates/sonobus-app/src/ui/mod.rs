//! egui front end over [`AppController`]. Only renders the view model and issues commands.
//!
//! Layout is one column of "pages" (connect, main mixer, audio setup, options, effects, chat)
//! that works from phone width up; on wide windows chat also docks as a side panel. Every
//! control has a text label or hover text (AccessKit names), everything is reachable with Tab,
//! and upstream's keyboard shortcuts apply where the feature exists (SonobusPluginEditor.cpp
//! 5400-5700): M / Ctrl-M mute input, Ctrl-U mute users, T push to talk, Ctrl-N/B connect,
//! Ctrl-D disconnect, Ctrl-L/S load/save setup, Ctrl-Y chat, Ctrl-K monitor delay, Ctrl-J
//! reset jitter buffers, Ctrl-comma options, Ctrl-Alt-C copy group link, Ctrl-R record,
//! Ctrl-G soundboard, Space / Ctrl-P play/pause, 0 / Ctrl-0 return to start, Alt-L loop,
//! Ctrl-O open audio file, Ctrl-W close it, Ctrl-Alt-L latency match, Ctrl-Alt-S suggest group,
//! and the soundboard's sample hotkeys while it is shown.

mod connect;
mod media;
mod mixer;
mod pages;
mod secure;
mod session;

pub use media::SampleEdit;
pub use secure::{JoinForm, Protocol, SecureTab};
pub use session::SuggestForm;

use std::time::Duration;

use egui::{Color32, Key, KeyboardShortcut, Modifiers, Response, RichText, Ui};
use sonobus_dsp::ParamSpec;
use sonobus_engine::{Connection, MeterLevels, PeerId};

use crate::controller::{AppController, ConnectRequest};
use crate::tr;

/// Below this width the layout is single-column (phones, narrow windows).
const NARROW: f32 = 700.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Connect,
    Main,
    AudioSetup,
    Options,
    Effects(FxTarget),
    Chat,
    /// Load/save a setup file, or import stock settings (path entry).
    File(FileAction),
    Soundboard,
    /// Add or edit a soundboard sample ([`SonobusApp::sample_edit`]).
    SampleEdit,
    /// Group latency match (LatencyMatchView).
    LatencyMatch,
    /// Suggest a new group to some users (SuggestNewGroupView).
    SuggestGroup,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FxTarget {
    Input(usize),
    Peer(PeerId, usize),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileAction {
    LoadSetup,
    SaveSetup,
    ImportSonoBus,
    /// An audio file for playback.
    OpenAudio,
    /// The folder for new recordings.
    RecordDir,
    /// The file of the sample being edited.
    SampleFile,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectTab {
    Group,
    Public,
    Recents,
    Direct,
    Host,
}

/// What the UI offers, by where it runs. Recording, file playback, the soundboard, OSC control
/// and the language choice are in both (upstream's plugin has the first three too).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// The audio device page (in a plugin the host does the audio, E.cpp:3710-3720).
    pub audio_setup: bool,
    /// Hosting a connection server (upstream starts one only standalone, E.cpp:1307-1310).
    pub host_server: bool,
    /// Settings files: setup load/save (upstream: standalone only, E.cpp:2548, 2595), stock
    /// SonoBus import, the settings directory and "Save now".
    pub settings_files: bool,
    /// An application window: quit confirmation and its saved geometry (a plugin's host
    /// keeps the editor size).
    pub app_window: bool,
    /// Plugin-only controls: metronome "Sync to Host" (E.cpp:1206), "Save as default plugin
    /// options" / "Reset default plugin options" (OptionsView.cpp:384-387), and a language change
    /// that takes effect when the host reopens the view (OptionsView.cpp:1241-1247).
    pub plugin: bool,
    /// Secure sessions (protocol v2: this app only, end-to-end encrypted).
    pub secure: bool,
    /// "Advertise on the local network" starts ticked when hosting a secure session.
    pub lan_advertise_default: bool,
}

impl Capabilities {
    /// The desktop app and the Android app.
    pub const STANDALONE: Capabilities = Capabilities {
        audio_setup: true,
        host_server: true,
        settings_files: true,
        app_window: true,
        plugin: false,
        secure: true,
        lan_advertise_default: true,
    };
    /// A plugin joins and hosts secure sessions too; it advertises one on the LAN only when
    /// asked (several plugin instances in a studio would all show up).
    pub const PLUGIN: Capabilities = Capabilities {
        audio_setup: false,
        host_server: false,
        settings_files: false,
        app_window: false,
        plugin: true,
        secure: true,
        lan_advertise_default: false,
    };

    /// Whether `page` exists here.
    pub fn has(&self, page: Page) -> bool {
        match page {
            Page::AudioSetup => self.audio_setup,
            Page::File(
                FileAction::LoadSetup | FileAction::SaveSetup | FileAction::ImportSonoBus,
            ) => self.settings_files,
            _ => true,
        }
    }
}

/// The application window's state: the controller plus UI-only form state.
pub struct SonobusApp {
    pub ctrl: AppController,
    /// From the controller's mode: [`Capabilities::PLUGIN`] when hosted.
    pub caps: Capabilities,
    pub page: Page,
    pub tab: ConnectTab,
    pub form: ConnectRequest,
    pub link_text: String,
    pub direct_addr: String,
    pub host_port: u16,
    pub chat_text: String,
    /// `None` = send to all. By peer id: secure sessions' names are only labels.
    pub chat_target: Option<PeerId>,
    pub show_chat: bool,
    pub path_text: String,
    pub setup_edit: Option<crate::audio::DeviceSetup>,
    pub confirm_quit: bool,
    pub allowed_to_close: bool,
    pub show_password: bool,
    /// Platform extras (Android playback capture); `None` on desktop.
    pub platform: Option<Box<dyn crate::platform::Platform>>,
    /// "App sound" chosen in audio setup, app not picked yet.
    pub picking_app: bool,
    /// Soundboard side panel on wide windows.
    pub show_soundboard: bool,
    pub sample_edit: Option<SampleEdit>,
    /// Name field of the soundboard menu (new / rename).
    pub board_name: String,
    pub confirm_delete_board: bool,
    pub suggest: SuggestForm,
    /// Latency match target chosen in the dialog (`None` = the default).
    pub latency_target: Option<f32>,
    /// Receive Sync padding, ms.
    pub sync_padding: f32,
    /// A hold-button sample under the pointer.
    held_sample: Option<(usize, usize)>,
    /// The soundboard is on screen (its hotkeys work only then, as upstream).
    soundboard_visible: bool,
    last_window: Option<[i32; 4]>,
    /// "Paste Share Link" asked for the clipboard.
    awaiting_paste: bool,
    /// File playback overviews.
    pub waveforms: crate::waveform::Waveforms,
    /// BLOCK pressed for this peer: waiting for confirmation.
    pub confirm_block: Option<(PeerId, String)>,
    /// An audio setup change that would stop the recording: waiting for confirmation.
    pub confirm_setup: Option<crate::audio::DeviceSetup>,
    /// The push-to-talk key was down on the last frame (push to talk acts on changes only, so
    /// OSC push to talk is not released by the next frame).
    ptt_key: bool,
    /// This device's LAN addresses, looked up off the UI thread now and then.
    lan: (
        Option<std::time::Instant>,
        Vec<std::net::IpAddr>,
        Option<std::sync::mpsc::Receiver<Vec<std::net::IpAddr>>>,
    ),
    /// A running "Trim to New".
    pub trim: Option<std::sync::mpsc::Receiver<std::io::Result<std::path::PathBuf>>>,
    /// The connect page's protocol: compatible (legacy) groups or secure sessions.
    pub protocol: Protocol,
    pub secure_tab: SecureTab,
    pub host_form: crate::secure::HostForm,
    pub join: JoinForm,
    /// "My relay server" URL being typed.
    pub relay_url: String,
    pub show_qr: bool,
    /// "Remove from session" pressed for this peer: waiting for confirmation.
    pub confirm_remove: Option<(PeerId, String)>,
    pub confirm_reset_identity: bool,
    awaiting_invite_paste: bool,
    /// LAN browsing was started (or failed) for the join tab.
    lan_tried: bool,
}

impl SonobusApp {
    /// `form` pre-fills the connect page (from the command line or a link).
    pub fn new(ctrl: AppController, form: ConnectRequest) -> Self {
        let connected =
            ctrl.vm().snapshot.connection != Connection::Disconnected || ctrl.vm().connecting;
        let caps = if ctrl.is_hosted() {
            Capabilities::PLUGIN
        } else {
            Capabilities::STANDALONE
        };
        SonobusApp {
            caps,
            // a secure session (or one asked for on the command line) shows the secure side
            protocol: if ctrl.vm().secure.is_some()
                || ctrl.vm().secure_starting
                || ctrl.vm().pending_invite.is_some()
            {
                Protocol::Secure
            } else {
                Protocol::Compatible
            },
            secure_tab: SecureTab::Host,
            host_form: crate::secure::HostForm {
                lan: caps.lan_advertise_default,
                ..Default::default()
            },
            join: JoinForm::default(),
            relay_url: String::new(),
            show_qr: true,
            confirm_remove: None,
            confirm_reset_identity: false,
            awaiting_invite_paste: false,
            lan_tried: false,
            page: if connected { Page::Main } else { Page::Connect },
            tab: ConnectTab::Group,
            form,
            link_text: String::new(),
            direct_addr: String::new(),
            host_port: crate::controller::DEFAULT_HOST_PORT,
            chat_text: String::new(),
            chat_target: None,
            show_chat: ctrl.vm().state.extra.last_chat_shown,
            path_text: String::new(),
            setup_edit: None,
            confirm_quit: false,
            allowed_to_close: false,
            show_password: false,
            platform: None,
            picking_app: false,
            show_soundboard: ctrl.vm().state.extra.last_soundboard_shown,
            sample_edit: None,
            board_name: String::new(),
            confirm_delete_board: false,
            suggest: SuggestForm::default(),
            latency_target: None,
            sync_padding: 0.0,
            held_sample: None,
            soundboard_visible: false,
            last_window: None,
            awaiting_paste: false,
            waveforms: Default::default(),
            trim: None,
            confirm_block: None,
            confirm_setup: None,
            ptt_key: false,
            lan: (None, Vec::new(), None),
            ctrl,
        }
    }

    pub fn open_page(&mut self, page: Page) {
        if !self.caps.has(page) {
            return;
        }
        match page {
            Page::AudioSetup => {
                self.setup_edit = Some(self.ctrl.audio_setup().clone());
                self.ctrl.scan_devices(self.ctrl.audio_setup().host.clone());
            }
            Page::File(a) => {
                let dir = self.ctrl.setup_folder();
                self.path_text = match a {
                    FileAction::LoadSetup => self
                        .ctrl
                        .vm()
                        .recent_setups
                        .last()
                        .cloned()
                        .unwrap_or_default(),
                    FileAction::SaveSetup => {
                        dir.join("MySetup.sonobus").to_string_lossy().into_owned()
                    }
                    FileAction::ImportSonoBus => crate::settings::upstream_settings_file()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    FileAction::OpenAudio => self
                        .ctrl
                        .vm()
                        .recent_files
                        .first()
                        .cloned()
                        .unwrap_or_else(|| {
                            let mut d = self.ctrl.record_dir().to_string_lossy().into_owned();
                            d.push(std::path::MAIN_SEPARATOR);
                            d
                        }),
                    FileAction::RecordDir => self.ctrl.record_dir().to_string_lossy().into_owned(),
                    FileAction::SampleFile => self
                        .sample_edit
                        .as_ref()
                        .and_then(|e| crate::settings::url_path(&e.sample.file_url))
                        .map(|p| p.to_string_lossy().into_owned())
                        .or_else(|| self.ctrl.vm().recent_files.first().cloned())
                        .unwrap_or_default(),
                };
            }
            Page::LatencyMatch => {
                self.latency_target = None;
                self.ctrl.begin_latency_match();
            }
            Page::SuggestGroup => {
                self.suggest = SuggestForm {
                    group: crate::links::random_group_name(),
                    users: self
                        .ctrl
                        .vm()
                        .snapshot
                        .peers
                        .iter()
                        .map(|p| p.user.clone())
                        .collect(),
                    ..SuggestForm::default()
                };
            }
            Page::Chat => self.ctrl.mark_chat_read(),
            _ => {}
        }
        self.page = page;
    }

    /// Ticks the controller and follows the platform capture: once it runs, its format becomes
    /// the engine's platform input. Called every frame, and by the Android keep-alive thread
    /// while no frame is drawn.
    pub fn tick(&mut self) {
        self.ctrl.tick();
        let Some(c) = self.platform.as_ref().map(|p| p.capture()) else {
            return;
        };
        if c.state == crate::platform::CaptureState::Running
            && c.format.is_some()
            && self.ctrl.audio_setup().external != c.format
            && let Err(e) = self.ctrl.set_external_input(c.format)
        {
            self.ctrl
                .set_status(format!("{}: {e}", tr("Audio setup failed")), true);
        }
    }

    /// One frame: tick the controller, handle shortcuts, draw.
    pub fn show(&mut self, ui: &mut Ui) {
        // this instance's language while it draws (plugin instances may differ)
        let _lang = crate::i18n::scope(self.ctrl.language());
        // once per egui context: a plugin editor gets a new one each time it opens
        let fonts = egui::Id::unique("sonobus_fonts");
        if ui.ctx().data(|d| d.get_temp::<bool>(fonts)).is_none() {
            ui.ctx().data_mut(|d| d.insert_temp(fonts, true));
            install_fonts(ui.ctx());
        }
        if !self.caps.has(self.page) {
            self.page = Page::Main;
        }
        if self.page == Page::LatencyMatch && self.ctrl.vm().latency_match.is_none() {
            self.page = Page::Main; // left the group meanwhile
        }
        // "On this network" looks only while it is shown
        let joining = self.page == Page::Connect
            && self.protocol == Protocol::Secure
            && self.secure_tab == SecureTab::Join
            && self.ctrl.vm().secure.is_none();
        if !joining && self.lan_tried {
            self.lan_tried = false;
            self.ctrl.lan_browse(false);
        }
        self.tick();
        let wide = ui.available_width() >= NARROW;
        self.soundboard_visible = self.page == Page::Soundboard || (wide && self.show_soundboard);
        self.shortcuts(ui.ctx());
        self.dropped_files(ui.ctx());
        if self.caps.app_window {
            self.track_window(ui.ctx());
        }

        egui::Panel::top("top_bar").show(ui, |ui| self.top_bar(ui));
        egui::Panel::bottom("status_bar").show(ui, |ui| self.status_bar(ui));
        if wide && self.show_chat && self.page != Page::Chat {
            egui::Panel::right("chat_panel")
                .default_size(280.0)
                .resizable(true)
                .show(ui, |ui| {
                    pages::chat(self, ui);
                });
        }
        if wide && self.show_soundboard && self.page != Page::Soundboard {
            egui::Panel::right("soundboard_panel")
                .default_size(300.0)
                .resizable(true)
                .show(ui, |ui| {
                    egui::ScrollArea::vertical()
                        .id_salt("sb_scroll")
                        .show(ui, |ui| media::soundboard(self, ui));
                });
        }
        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| match self.page {
                    Page::Connect => connect::page(self, ui),
                    Page::Main => mixer::page(self, ui),
                    Page::AudioSetup => pages::audio_setup(self, ui),
                    Page::Options => pages::options(self, ui),
                    Page::Effects(t) => pages::effects(self, ui, t),
                    Page::Chat => pages::chat(self, ui),
                    Page::File(a) => pages::file(self, ui, a),
                    Page::Soundboard => media::soundboard(self, ui),
                    Page::SampleEdit => media::sample_edit(self, ui),
                    Page::LatencyMatch => session::latency_match(self, ui),
                    Page::SuggestGroup => session::suggest_group(self, ui),
                });
        });
        if self.confirm_quit {
            self.quit_dialog(ui.ctx());
        }
        if !secure::prompts(self, ui.ctx()) {
            session::prompts(self, ui.ctx());
        }
        // meters and network state move on their own
        ui.ctx().request_repaint_after(Duration::from_millis(50));
    }

    fn top_bar(&mut self, ui: &mut Ui) {
        ui.horizontal_wrapped(|ui| {
            ui.heading(tr("SonoBus"));
            let vm = self.ctrl.vm();
            let connected = vm.snapshot.connection != Connection::Disconnected || vm.connecting;
            if connected {
                let leave = if self.ctrl.vm().secure.is_some() { tr("Leave Session") } else { tr("Disconnect") };
                if ui.button(leave).on_hover_text("Ctrl-D").clicked() {
                    self.ctrl.disconnect();
                    self.page = Page::Connect;
                }
                if self.page != Page::Main && ui.button(tr("Main")).clicked() {
                    self.page = Page::Main;
                }
            } else if self.page != Page::Connect && ui.button(tr("Connect...")).clicked() {
                self.page = Page::Connect;
            }
            if connected && self.page == Page::Main && ui.button(tr("Connect to Other")).on_hover_text(tr("Show the connections page, while staying connected to current group")).clicked() {
                self.page = Page::Connect;
            }
            if self.page == Page::Connect && !connected && ui.button(tr("Main")).on_hover_text(tr("INPUT MIXER")).clicked() {
                self.page = Page::Main;
            }
            let unread = self.ctrl.vm().unread_chat;
            let chat = if unread > 0 { format!("{} ({unread})", tr("Chat")) } else { tr("Chat").to_owned() };
            if ui.selectable_label(self.show_chat || self.page == Page::Chat, chat).on_hover_text(tr("Show or hide chat area")).clicked() {
                self.toggle_chat(ui.available_width() >= NARROW);
            }
            if ui.selectable_label(self.soundboard_visible, tr("Soundboard")).on_hover_text(tr("Show/Hide Soundboard")).clicked() {
                self.toggle_soundboard(ui.ctx().content_rect().width() >= NARROW);
            }
            if self.ctrl.vm().snapshot.group.is_some() {
                let secure = self.ctrl.vm().secure.is_some();
                ui.menu_button(tr("Group"), |ui| {
                    if !secure
                        && ui.button(tr("Share Group Link")).on_hover_text(tr("Copy connection information to the clipboard to share")).clicked()
                        && let Some(link) = self.ctrl.group_link()
                    {
                        ui.ctx().copy_text(link);
                        self.ctrl.set_status(tr("Copied group connection info to clipboard for you to share with others"), false);
                    }
                    if secure && ui.button(tr("Secure session")).clicked() {
                        (self.page, self.protocol) = (Page::Connect, Protocol::Secure);
                    }
                    if ui.button(tr("Group Latency Match...")).clicked() {
                        self.open_page(Page::LatencyMatch);
                    }
                    // a suggestion moves everyone to a compatible group
                    if !secure && ui.button(tr("Suggest New Group...")).clicked() {
                        self.open_page(Page::SuggestGroup);
                    }
                });
            }
            ui.menu_button(tr("File"), |ui| {
                if self.caps.settings_files {
                    if ui.button(tr("Load Setup...")).clicked() {
                        self.open_page(Page::File(FileAction::LoadSetup));
                    }
                    if ui.button(tr("Save Setup...")).clicked() {
                        self.open_page(Page::File(FileAction::SaveSetup));
                    }
                    if ui.button(tr("Import settings from SonoBus...")).clicked() {
                        self.open_page(Page::File(FileAction::ImportSonoBus));
                    }
                }
                if ui.button(tr("Open Audio File...")).clicked() {
                    self.open_page(Page::File(FileAction::OpenAudio));
                }
            });
            if self.caps.audio_setup && ui.button(tr("Setup Audio")).clicked() {
                self.open_page(Page::AudioSetup);
            }
            if ui.button(tr("Settings")).on_hover_text(tr("Show Options")).clicked() {
                self.open_page(Page::Options);
            }
        });
    }

    /// A link the OS opened the app with: a secure session invite is shown for confirmation
    /// (the connect page's secure side behind it).
    pub fn open_link(&mut self, url: &str) {
        if self.caps.secure && self.ctrl.open_invite(url) {
            (self.page, self.protocol) = (Page::Connect, Protocol::Secure);
        }
    }

    /// Whether we send audio at all: a broadcast listener does not (its send controls go).
    pub fn sends(&self) -> bool {
        self.ctrl
            .vm()
            .secure
            .as_ref()
            .is_none_or(|s| s.role != sonobus_engine::modern::SessionRole::Listener)
    }

    fn toggle_chat(&mut self, wide: bool) {
        if wide {
            self.show_chat = !self.show_chat;
            self.ctrl.mark_chat_read();
            let shown = self.show_chat;
            self.ctrl.edit(move |s| s.extra.last_chat_shown = shown);
        } else if self.page == Page::Chat {
            self.page = Page::Main;
        } else {
            self.open_page(Page::Chat);
        }
    }

    fn toggle_soundboard(&mut self, wide: bool) {
        if wide {
            self.show_soundboard = !self.show_soundboard;
            let shown = self.show_soundboard;
            self.ctrl
                .edit(move |s| s.extra.last_soundboard_shown = shown);
            if self.page == Page::Soundboard {
                self.page = Page::Main;
            }
        } else if self.page == Page::Soundboard {
            self.page = Page::Main;
        } else {
            self.open_page(Page::Soundboard);
        }
    }

    /// Files dropped on the window: onto the soundboard as new samples, else into playback.
    fn dropped_files(&mut self, ctx: &egui::Context) {
        let files: Vec<std::path::PathBuf> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .filter(|p| !p.as_os_str().is_empty())
                .collect()
        });
        for f in files {
            if self.soundboard_visible {
                media::add_sample_file(self, &f);
            } else if let Err(e) = self.ctrl.load_file(&f) {
                self.ctrl
                    .set_status(format!("{}: {e}", tr("Cannot play file")), true);
            }
        }
    }

    /// Remembers the window geometry for the next start (saved with the settings).
    fn track_window(&mut self, ctx: &egui::Context) {
        let (outer, inner) = ctx.input(|i| (i.viewport().outer_rect, i.viewport().inner_rect));
        if let (Some(o), Some(i)) = (outer, inner) {
            let r = [o.min.x, o.min.y, i.width(), i.height()].map(|v| v.round() as i32);
            if self.last_window != Some(r) {
                self.last_window = Some(r);
                self.ctrl.set_window(r);
            }
        }
    }

    /// This device's LAN addresses (refreshed every 10 s on a worker thread).
    pub fn lan_addresses(&mut self) -> &[std::net::IpAddr] {
        if let Some(rx) = &self.lan.2
            && let Ok(v) = rx.try_recv()
        {
            (self.lan.1, self.lan.2) = (v, None);
        }
        if self.lan.2.is_none()
            && self
                .lan
                .0
                .is_none_or(|t| t.elapsed() >= Duration::from_secs(10))
        {
            let (tx, rx) = std::sync::mpsc::channel();
            std::thread::spawn(move || tx.send(crate::controller::lan_addresses()));
            self.lan = (
                Some(std::time::Instant::now()),
                std::mem::take(&mut self.lan.1),
                Some(rx),
            );
        }
        &self.lan.1
    }

    fn status_bar(&mut self, ui: &mut Ui) {
        // a server hosted here stays visible (and stoppable) on every page
        let mut stop = false;
        if let Some((port, addrs)) = &self.ctrl.vm().hosting {
            ui.horizontal_wrapped(|ui| {
                let a = addrs
                    .first()
                    .map_or("127.0.0.1".into(), ToString::to_string);
                ui.label(RichText::new(format!("{} {a}:{port}", tr("Hosting on"))).strong());
                stop = ui.small_button(tr("Stop Hosting")).clicked();
            });
        }
        if stop {
            self.ctrl.stop_hosting();
        }
        let mut retry = false;
        if let Some(e) = &self.ctrl.vm().engine_error {
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(Color32::from_rgb(230, 90, 80), e);
                retry = ui.small_button(tr("Retry")).clicked();
            });
        }
        if retry {
            self.ctrl.retry_engine();
        }
        let vm = self.ctrl.vm();
        let mut clear = false;
        ui.horizontal_wrapped(|ui| {
            // what protects this session, whenever there is one
            if vm.secure.is_some() {
                secure::badge(ui, true);
            } else if vm.snapshot.connection != Connection::Disconnected {
                secure::badge(ui, false);
            }
            let conn = match (vm.snapshot.connection, vm.reconnecting) {
                (Connection::Connected, _) if vm.secure.is_some() => tr("Connected"),
                (_, true) => tr("Reconnecting..."),
                (Connection::Connected, _) => tr("Connected to server"),
                (Connection::Connecting, _) => tr("Connecting..."),
                (Connection::Disconnected, _) if vm.connecting => tr("Connecting..."),
                (Connection::Disconnected, _) => tr("Disconnected"),
            };
            ui.label(conn);
            if let Some(s) = &vm.secure {
                let role = match s.role {
                    sonobus_engine::modern::SessionRole::Host => tr("Host"),
                    sonobus_engine::modern::SessionRole::Member => tr("Member"),
                    sonobus_engine::modern::SessionRole::Listener => tr("Listener"),
                };
                ui.label(format!(
                    "| {} | {role} | {}: {}",
                    s.session,
                    tr("User Count"),
                    vm.snapshot.peers.len() + 1
                ));
            } else if let Some(g) = &vm.snapshot.group {
                ui.label(format!(
                    "| {}{g} | {}: {}",
                    tr("Group: "),
                    tr("User Count"),
                    vm.snapshot.peers.len() + 1
                ));
            }
            if !vm.status.is_empty() {
                let color = if vm.status_is_error {
                    Color32::from_rgb(230, 90, 80)
                } else {
                    ui.visuals().text_color()
                };
                ui.label(RichText::new(&vm.status).color(color));
                clear = ui
                    .small_button(tr("Clear"))
                    .on_hover_text(tr("Clear status message"))
                    .clicked();
            }
        });
        if clear {
            self.ctrl.clear_status();
        }
    }

    fn shortcuts(&mut self, ctx: &egui::Context) {
        if self.caps.app_window
            && ctx.input(|i| i.viewport().close_requested())
            && !self.allowed_to_close
        {
            if self.ctrl.vm().snapshot.connection == Connection::Connected {
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.confirm_quit = true;
            } else {
                self.allowed_to_close = true;
            }
        }
        if self.ctrl.vm().state.extra.disable_key_shortcuts {
            return;
        }
        if self.page == Page::SampleEdit
            && self
                .sample_edit
                .as_ref()
                .is_some_and(|e| e.capturing_hotkey)
        {
            return; // the next key is the sample's hotkey
        }
        let typing = ctx.egui_wants_keyboard_input();
        // push to talk on changes of the key only (OSC may hold it meanwhile)
        let talk = !typing && ctx.input(|i| i.key_down(Key::T) && i.modifiers.is_none());
        if talk != std::mem::replace(&mut self.ptt_key, talk) {
            self.ctrl.push_to_talk(talk);
        }
        if typing {
            return; // the text field gets the keys, Ctrl ones included
        }
        if self.soundboard_visible {
            media::hotkeys(self, ctx);
        }
        let cmd = |k: Key| KeyboardShortcut::new(Modifiers::COMMAND, k);
        let hit = |s: KeyboardShortcut| ctx.input_mut(|i| i.consume_shortcut(&s));
        let plain = |k: Key| !typing && ctx.input(|i| i.key_pressed(k) && i.modifiers.is_none());
        if hit(cmd(Key::M)) || plain(Key::M) {
            let m = !self.ctrl.vm().state.params.main_in_mute;
            self.ctrl.edit(move |s| s.params.main_in_mute = m);
        }
        if hit(cmd(Key::U)) {
            let m = !self.ctrl.vm().state.params.main_recv_mute;
            self.ctrl.edit(move |s| s.params.main_recv_mute = m);
        }
        if hit(cmd(Key::N)) || hit(cmd(Key::B)) {
            self.page = Page::Connect;
        }
        if hit(cmd(Key::D)) {
            self.ctrl.disconnect();
        }
        if hit(cmd(Key::L)) {
            self.open_page(Page::File(FileAction::LoadSetup));
        }
        if hit(cmd(Key::S)) {
            self.open_page(Page::File(FileAction::SaveSetup));
        }
        if hit(cmd(Key::Y)) {
            let wide = ctx.content_rect().width() >= NARROW;
            self.toggle_chat(wide);
        }
        if hit(cmd(Key::K)) {
            mixer::toggle_monitor_delay(&mut self.ctrl);
        }
        if hit(cmd(Key::J)) {
            self.ctrl.engine().reset_all_jitter_buffers();
        }
        if hit(cmd(Key::Comma)) {
            self.open_page(Page::Options);
        }
        if hit(KeyboardShortcut::new(
            Modifiers::COMMAND | Modifiers::ALT,
            Key::C,
        )) && let Some(link) = self
            .ctrl
            .group_link()
            .or_else(|| self.ctrl.vm().secure.as_ref()?.invite.clone())
        {
            ctx.copy_text(link);
        }
        if hit(cmd(Key::R)) {
            self.ctrl.toggle_recording();
        }
        if hit(cmd(Key::G)) {
            let wide = ctx.content_rect().width() >= NARROW;
            self.toggle_soundboard(wide);
        }
        if hit(cmd(Key::O)) {
            self.open_page(Page::File(FileAction::OpenAudio));
        }
        if hit(cmd(Key::W)) {
            self.ctrl.close_file();
        }
        let no_focus = ctx.memory(|m| m.focused().is_none());
        if let Some(f) = self.ctrl.vm().file.clone() {
            if hit(cmd(Key::P)) || (no_focus && plain(Key::Space)) {
                self.ctrl.engine().set_file_playing(!f.playing);
            }
            if hit(cmd(Key::Num0)) || plain(Key::Num0) {
                self.ctrl.engine().seek_file(0.0);
            }
            if hit(KeyboardShortcut::new(Modifiers::ALT, Key::L)) {
                self.ctrl.engine().set_file_looping(!f.looping);
            }
        }
        if self.ctrl.vm().snapshot.group.is_some() {
            if hit(KeyboardShortcut::new(
                Modifiers::COMMAND | Modifiers::ALT,
                Key::L,
            )) {
                self.open_page(Page::LatencyMatch);
            }
            if self.ctrl.vm().secure.is_none()
                && hit(KeyboardShortcut::new(
                    Modifiers::COMMAND | Modifiers::ALT,
                    Key::S,
                ))
            {
                self.open_page(Page::SuggestGroup);
            }
        }
    }

    fn quit_dialog(&mut self, ctx: &egui::Context) {
        egui::Modal::new(egui::Id::unique("quit")).show(ctx, |ui| {
            ui.heading(tr("Quit Confirmation"));
            ui.label(tr("You are connected, are you sure you want to quit?"));
            ui.horizontal(|ui| {
                if ui.button(tr("Quit")).clicked() {
                    self.allowed_to_close = true;
                    self.confirm_quit = false;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
                if ui.button(tr("Cancel")).clicked() {
                    self.confirm_quit = false;
                }
            });
        });
    }
}

#[cfg(feature = "desktop")]
impl eframe::App for SonobusApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        self.show(ui);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.ctrl.shutdown();
    }
}

// ---- shared widgets ---------------------------------------------------------------------------

/// A row of controls: wraps on wide screens, stacks on phones (a slider with its value and
/// label is a nested horizontal layout that a wrapping row cannot break).
fn row<R>(ui: &mut Ui, add: impl FnOnce(&mut Ui) -> R) -> R {
    if ui.available_width() < 520.0 {
        ui.vertical(add).inner
    } else {
        ui.horizontal_wrapped(add).inner
    }
}

/// A framed on/off button (upstream's MUTE/SOLO style toggles).
fn toggle(ui: &mut Ui, value: &mut bool, text: &str) -> Response {
    let mut r = ui.add(egui::Button::new(text).selected(*value));
    if r.clicked() {
        *value = !*value;
        r.mark_changed();
    }
    r
}

/// A text field with a visible label that is also its accessible name.
fn text_field(
    ui: &mut Ui,
    label: &str,
    value: &mut String,
    hint: &str,
    password: bool,
) -> Response {
    ui.horizontal(|ui| {
        let l = ui.label(label);
        ui.add(
            egui::TextEdit::singleline(value)
                .hint_text(hint)
                .password(password)
                .desired_width(220.0),
        )
        .labelled_by(l.id)
    })
    .inner
}

/// Slider over a JUCE-skewed range (`proportion = ((v - min) / (max - min))^skew`), stepped.
fn skew_slider(
    ui: &mut Ui,
    value: &mut f32,
    spec: ParamSpec,
    label: &str,
    unit: &'static str,
) -> Response {
    let span = spec.max - spec.min;
    let to_p = |v: f32| ((v - spec.min) / span).clamp(0.0, 1.0).powf(spec.skew) as f64;
    let from_p = move |p: f64| {
        let v = spec.min + span * (p.clamp(0.0, 1.0) as f32).powf(1.0 / spec.skew);
        if spec.step > 0.0 {
            (spec.min + ((v - spec.min) / spec.step).round() * spec.step).clamp(spec.min, spec.max)
        } else {
            v
        }
    };
    let mut p = to_p(*value);
    let decimals = if spec.step >= 1.0 {
        0
    } else if spec.step >= 0.1 || span > 10.0 {
        1
    } else {
        2
    };
    let r = ui.add(
        egui::Slider::new(&mut p, 0.0..=1.0)
            .text(label.trim_end_matches([':', ' ']))
            .custom_formatter(move |p, _| format!("{:.*}{unit}", decimals, from_p(p)))
            .custom_parser(move |s| {
                let v: f32 = s.trim().trim_end_matches(unit.trim()).trim().parse().ok()?;
                Some(to_p(v.clamp(spec.min, spec.max)))
            }),
    );
    if r.changed() {
        *value = from_p(p);
    }
    r
}

const fn spec(min: f32, max: f32, step: f32, skew: f32, default: f32) -> ParamSpec {
    ParamSpec {
        min,
        max,
        step,
        skew,
        default,
    }
}

/// Gain slider in dB over a linear 0..max gain (upstream level sliders show dB).
fn gain_slider(ui: &mut Ui, gain: &mut f32, max: f32, label: &str) -> Response {
    skew_slider(ui, gain, spec(0.0, max, 0.0, 0.5, 1.0), label, "")
}

fn db(v: f32) -> f32 {
    if v <= 0.0 { -120.0 } else { 20.0 * v.log10() }
}

/// Peak/RMS meter, one bar per channel, -60..0 dBFS, with a dB scale and a 2 s peak hold
/// (upstream's LevelMeter look).
fn meter(ui: &mut Ui, levels: &MeterLevels, label: &str) -> Response {
    let n = levels.peak.len().max(1);
    let w = ui.available_width().clamp(60.0, 240.0);
    let bar_h = 5.0;
    let (rect, resp) = ui.allocate_exact_size(
        egui::vec2(w, 2.0 + (bar_h + 1.0) * n as f32 + 9.0),
        egui::Sense::hover(),
    );
    let p = ui.painter();
    let bars = egui::Rect::from_min_size(rect.min, egui::vec2(w, 2.0 + (bar_h + 1.0) * n as f32));
    p.rect_filled(bars, 2.0, ui.visuals().extreme_bg_color);
    let frac = |v: f32| ((db(v) + 60.0) / 60.0).clamp(0.0, 1.0);
    let colour = |v: f32| {
        if v >= 0.99 {
            Color32::RED
        } else if db(v) > -6.0 {
            Color32::YELLOW
        } else {
            Color32::from_rgb(60, 180, 90)
        }
    };
    let now = ui.input(|i| i.time);
    let hold_id = resp.id.with("hold");
    let mut holds: Vec<(f32, f64)> = ui.data(|d| d.get_temp(hold_id)).unwrap_or_default();
    holds.resize(levels.peak.len(), (0.0, 0.0));
    for c in 0..levels.peak.len() {
        let y = bars.top() + 1.0 + (bar_h + 1.0) * c as f32;
        let bar = |v: f32| {
            egui::Rect::from_min_size(
                egui::pos2(bars.left(), y),
                egui::vec2(bars.width() * frac(v), bar_h),
            )
        };
        let peak = levels.peak[c];
        p.rect_filled(
            bar(levels.rms.get(c).copied().unwrap_or(0.0)),
            0.0,
            colour(peak).gamma_multiply(0.7),
        );
        let x = bars.left() + bars.width() * frac(peak);
        p.vline(x, y..=y + bar_h, egui::Stroke::new(1.5, colour(peak)));
        let h = &mut holds[c];
        if peak >= h.0 || now - h.1 > 2.0 {
            *h = (peak, now);
        }
        if h.0 > 0.0 {
            let x = bars.left() + bars.width() * frac(h.0);
            p.vline(x, y..=y + bar_h, egui::Stroke::new(2.0, colour(h.0)));
        }
    }
    ui.data_mut(|d| d.insert_temp(hold_id, holds));
    let tick = ui.visuals().weak_text_color();
    for d in [-48.0f32, -36.0, -24.0, -12.0, -6.0, 0.0] {
        let x = bars.left() + bars.width() * (d + 60.0) / 60.0;
        p.vline(
            x,
            bars.bottom()..=bars.bottom() + 2.0,
            egui::Stroke::new(1.0, tick),
        );
        if w >= 140.0 || d == -24.0 || d == 0.0 {
            let align = if d == 0.0 {
                egui::Align2::RIGHT_TOP
            } else {
                egui::Align2::CENTER_TOP
            };
            p.text(
                egui::pos2(x, bars.bottom() + 1.0),
                align,
                format!("{}", d.abs()),
                egui::FontId::proportional(7.5),
                tick,
            );
        }
    }
    let peak_db = levels.peak.iter().copied().fold(0.0, f32::max);
    let text = format!("{label}: {:.0} dB", db(peak_db).max(-60.0));
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::Role::Meter, true, &text));
    resp.on_hover_text(text)
}

/// Transport symbols, painted (egui's bundled fonts have no media glyphs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Icon {
    Record,
    Play,
    Pause,
    ToStart,
    Loop,
    Lock,
    Unlock,
}

fn paint_icon(p: &egui::Painter, c: egui::Pos2, icon: Icon, colour: Color32) {
    let tri = |pts: [egui::Pos2; 3]| {
        egui::Shape::convex_polygon(pts.to_vec(), colour, egui::Stroke::NONE)
    };
    let r = 6.0;
    match icon {
        Icon::Record => {
            p.circle_filled(c, r, colour);
        }
        Icon::Play => {
            p.add(tri([
                c + egui::vec2(-r * 0.7, -r),
                c + egui::vec2(-r * 0.7, r),
                c + egui::vec2(r, 0.0),
            ]));
        }
        Icon::Pause => {
            for dx in [-r * 0.5, r * 0.5] {
                p.rect_filled(
                    egui::Rect::from_center_size(
                        c + egui::vec2(dx, 0.0),
                        egui::vec2(r * 0.6, r * 2.0),
                    ),
                    1.0,
                    colour,
                );
            }
        }
        Icon::ToStart => {
            p.rect_filled(
                egui::Rect::from_center_size(
                    c + egui::vec2(-r * 0.8, 0.0),
                    egui::vec2(2.0, r * 2.0),
                ),
                0.0,
                colour,
            );
            p.add(tri([
                c + egui::vec2(r, -r),
                c + egui::vec2(r, r),
                c + egui::vec2(-r * 0.5, 0.0),
            ]));
        }
        Icon::Loop => {
            p.circle_stroke(c, r * 0.9, egui::Stroke::new(2.0, colour));
            p.add(tri([
                c + egui::vec2(r * 0.9 - 4.0, -2.0),
                c + egui::vec2(r * 0.9 + 4.0, -2.0),
                c + egui::vec2(r * 0.9, 3.0),
            ]));
        }
        Icon::Lock | Icon::Unlock => {
            // body, and the shackle: closed, or lifted and open on the right
            p.rect_filled(
                egui::Rect::from_center_size(
                    c + egui::vec2(0.0, r * 0.45),
                    egui::vec2(r * 1.6, r * 1.1),
                ),
                1.5,
                colour,
            );
            let lift = if icon == Icon::Lock { 0.0 } else { r * 0.45 };
            let top = c + egui::vec2(0.0, -r * 0.35 - lift);
            let stroke = egui::Stroke::new(1.8, colour);
            let arc: Vec<egui::Pos2> = (0..=12)
                .map(|i| {
                    let a = std::f32::consts::PI * (1.0 + i as f32 / 12.0);
                    top + egui::vec2(a.cos(), a.sin()) * r * 0.55
                })
                .collect();
            p.add(egui::Shape::line(arc, stroke));
            p.line_segment(
                [
                    top + egui::vec2(-r * 0.55, 0.0),
                    c + egui::vec2(-r * 0.55, 0.0),
                ],
                stroke,
            );
            if icon == Icon::Lock {
                p.line_segment(
                    [
                        top + egui::vec2(r * 0.55, 0.0),
                        c + egui::vec2(r * 0.55, 0.0),
                    ],
                    stroke,
                );
            }
        }
    }
}

/// A button whose face is a painted symbol; `name` is its accessible name and tooltip.
fn icon_button(ui: &mut Ui, icon: Icon, name: &str, selected: bool) -> Response {
    let r = ui.add(
        egui::Button::new("")
            .selected(selected)
            .min_size(egui::vec2(34.0, 26.0)),
    );
    let colour = if icon == Icon::Record {
        Color32::from_rgb(220, 60, 60)
    } else {
        ui.visuals().widgets.style(&r).fg_stroke.color
    };
    paint_icon(ui.painter(), r.rect.center(), icon, colour);
    r.widget_info(|| egui::WidgetInfo::selected(egui::Role::Button, true, selected, name));
    r.on_hover_text(name)
}

/// `m:ss` (or `h:mm:ss`).
fn hms(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    if s >= 3600 {
        format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
    } else {
        format!("{}:{:02}", s / 60, s % 60)
    }
}

/// Loads system CJK fonts as fallbacks when the language needs them (egui's bundled fonts
/// cover Latin and Cyrillic only), from a fixed list of paths: synchronous, so the first frame
/// is right. In the app window, eframe's system font provider (feature `system_fonts`:
/// fontconfig, DirectWrite, CoreText, Android's fonts.xml) then finds any glyph still missing
/// (other scripts, unusual installs) on demand. Uses the current thread's language.
pub fn install_fonts(ctx: &egui::Context) {
    if !crate::i18n::needs_cjk_font(crate::i18n::current()) {
        return;
    }
    for (i, path) in crate::i18n::CJK_FONTS.iter().enumerate() {
        let Ok(bytes) = std::fs::read(path) else {
            continue;
        };
        ctx.add_font(egui::epaint::text::FontInsert::new(
            &format!("cjk{i}"),
            egui::FontData::from_owned(bytes),
            [egui::FontFamily::Proportional, egui::FontFamily::Monospace]
                .map(|family| egui::epaint::text::InsertFontFamily {
                    family,
                    priority: egui::epaint::text::FontPriority::Lowest,
                })
                .to_vec(),
        ));
    }
}
