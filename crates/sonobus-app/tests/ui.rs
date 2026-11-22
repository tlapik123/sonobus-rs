//! UI tests without a window (egui_kittest): every page renders at phone and desktop size with
//! every input widget named (kittest's accessibility check), and a click-through from hosting a
//! session to a peer's effects panel.

use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use sonobus_app::controller::{AppController, ConnectRequest, ControllerConfig};
use sonobus_app::ui::{ConnectTab, FileAction, FxTarget, Page, SampleEdit, SonobusApp};

fn app() -> SonobusApp {
    sonobus_app::i18n::init("en"); // labels below are English whatever the test machine's locale
    let ctrl = AppController::new(ControllerConfig {
        settings_dir: None,
        use_devices: false,
    })
    .unwrap();
    let form = ConnectRequest {
        user: "alice".into(),
        ..ctrl.default_request()
    };
    SonobusApp::new(ctrl, form)
}

fn harness(size: (f32, f32)) -> Harness<'static, SonobusApp> {
    Harness::builder()
        .with_size(size)
        .build_ui_state(|ui, app: &mut SonobusApp| app.show(ui), app())
}

/// Steps the UI in real time until `cond` holds.
fn until(
    h: &mut Harness<'_, SonobusApp>,
    secs: u64,
    what: &str,
    cond: impl Fn(&SonobusApp) -> bool,
) {
    let t = Instant::now();
    while !cond(h.state()) {
        assert!(
            t.elapsed() < Duration::from_secs(secs),
            "timed out: {what} ({})",
            h.state().ctrl.vm().status
        );
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.step();
}

#[test]
fn every_page_renders_at_both_sizes() {
    for size in [(400.0, 800.0), (1200.0, 800.0)] {
        let mut h = harness(size);
        let pages = [
            Page::Connect,
            Page::Main,
            Page::AudioSetup,
            Page::Options,
            Page::Effects(FxTarget::Input(0)),
            Page::Chat,
            Page::File(FileAction::LoadSetup),
            Page::File(FileAction::SaveSetup),
            Page::File(FileAction::ImportSonoBus),
            Page::File(FileAction::OpenAudio),
            Page::File(FileAction::RecordDir),
            Page::File(FileAction::SampleFile),
            Page::Soundboard,
            Page::SampleEdit,
            Page::LatencyMatch,
            Page::SuggestGroup,
        ];
        for page in pages {
            if page == Page::SampleEdit {
                h.state_mut().sample_edit = Some(SampleEdit {
                    board: 0,
                    index: None,
                    sample: Default::default(),
                    path: String::new(),
                    capturing_hotkey: false,
                });
            }
            h.state_mut().open_page(page);
            h.run_steps(3);
            open_sections(&mut h);
            h.check_accessibility(); // kittest checks only in run(); every page here, sections open
            if page == Page::Connect {
                for tab in [
                    ConnectTab::Group,
                    ConnectTab::Public,
                    ConnectTab::Recents,
                    ConnectTab::Direct,
                    ConnectTab::Host,
                ] {
                    h.state_mut().tab = tab;
                    h.run_steps(2);
                    h.check_accessibility();
                }
            }
        }
        // every dialog
        h.state_mut().ctrl.answer_pending_link(false);
        for d in 0..3 {
            match d {
                0 => h.state_mut().confirm_block = Some((sonobus_engine::PeerId(1), "bob".into())),
                1 => h.state_mut().confirm_setup = Some(Default::default()),
                _ => h.state_mut().confirm_quit = true,
            }
            h.run_steps(2);
            h.check_accessibility();
            (
                h.state_mut().confirm_block,
                h.state_mut().confirm_setup,
                h.state_mut().confirm_quit,
            ) = (None, None, false);
        }
        // key widgets by accessible name
        h.state_mut().open_page(Page::Main);
        h.run_steps(2);
        h.get_by_label("Mute All Input");
        assert!(h.query_all_by_label("Out Level").next().is_some());
        h.get_by_label("Add Input Group");
        h.get_by_label("Record");
        // the soundboard as a side panel on wide windows, a page on narrow ones
        h.get_by_label("Soundboard").click();
        h.run_steps(2);
        let wide = size.0 > 700.0;
        assert_eq!(h.state().page == Page::Soundboard, !wide);
        assert_eq!(h.state().show_soundboard, wide);
        h.get_by_label("New soundboard...");
    }
}

/// Opens every collapsed section on screen (their contents are checked too).
fn open_sections(h: &mut Harness<'_, SonobusApp>) {
    use egui_kittest::kittest::by;
    for _ in 0..4 {
        let closed = || by().predicate(|n| n.data().is_expanded() == Some(false));
        let n = h.query_all(closed()).count();
        if n == 0 {
            return;
        }
        for _ in 0..n {
            if let Some(node) = h.query_all(closed()).next() {
                node.click_accesskit();
            }
            h.run_steps(1);
        }
        h.run_steps(1);
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sonobus-ui-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 16-bit mono 48 kHz WAV of a 440 Hz tone.
fn wav(path: &std::path::Path, secs: f32) {
    let n = (48000.0 * secs) as u32;
    let mut b = Vec::new();
    for (tag, v) in [(&b"RIFF"[..], 36 + 2 * n), (b"WAVEfmt ", 16)] {
        b.extend_from_slice(tag);
        b.extend_from_slice(&v.to_le_bytes());
    }
    for v in [1u16, 1] {
        b.extend_from_slice(&v.to_le_bytes());
    }
    b.extend_from_slice(&48000u32.to_le_bytes());
    b.extend_from_slice(&96000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(2 * n).to_le_bytes());
    for i in 0..n {
        let v = (0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48000.0).sin() * 32767.0)
            as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

/// Types into the labelled text field, replacing what it holds.
fn type_into(h: &mut Harness<'_, SonobusApp>, label: &str, text: &str) {
    use eframe::egui::accesskit::Role;
    h.get_by_role_and_label(Role::TextInput, label).focus();
    h.run_steps(1);
    h.key_press_modifiers(eframe::egui::Modifiers::COMMAND, eframe::egui::Key::A);
    h.run_steps(1);
    h.get_by_role_and_label(Role::TextInput, label)
        .type_text(text);
    h.run_steps(1);
}

#[test]
fn record_play_and_soundboard() {
    let dir = temp_dir("media");
    let tone = dir.join("tone.wav");
    wav(&tone, 2.0);
    let mut h = harness((1200.0, 800.0));
    h.state_mut().page = Page::Main;
    h.state_mut().ctrl.set_record_dir(&dir.join("rec"));
    h.run_steps(2);

    // record start and stop (the files are created off the UI thread)
    h.get_by_label("Record").click();
    h.run_steps(1);
    until(&mut h, 5, "recording", |a| a.ctrl.vm().recording.is_some());
    std::thread::sleep(Duration::from_millis(300));
    h.get_by_label("Record").click();
    h.run_steps(1);
    until(&mut h, 10, "recorded files", |a| {
        !a.ctrl.vm().last_recording.is_empty()
    });
    assert!(h.state().ctrl.vm().last_recording[0].exists());

    // load a file through the path page, play it
    h.get_by_label("Load File").click();
    h.run_steps(2);
    assert_eq!(h.state().page, Page::File(FileAction::OpenAudio));
    type_into(&mut h, "File", &tone.to_string_lossy());
    h.get_by_label("Load File").click();
    h.run_steps(2);
    assert_eq!(h.state().page, Page::Main, "{}", h.state().ctrl.vm().status);
    h.get_by_label("Play").click();
    h.run_steps(1);
    until(&mut h, 5, "playing", |a| {
        a.ctrl
            .vm()
            .file
            .as_ref()
            .is_some_and(|f| f.playing && f.position_secs > 0.0)
    });
    h.get_by_label("Close Audio File").click();
    h.run_steps(2);
    assert!(h.state().ctrl.vm().file.is_none());

    // soundboard: a board, a sample, press it
    h.state_mut().open_page(Page::Soundboard);
    h.run_steps(2);
    h.get_by_label("New soundboard...").click();
    h.run_steps(2);
    assert_eq!(h.state().ctrl.vm().soundboards.boards.len(), 1);
    h.get_by_label("+ Add Sample").click();
    h.run_steps(2);
    assert_eq!(h.state().page, Page::SampleEdit);
    type_into(&mut h, "Name", "Beep");
    type_into(&mut h, "File", &tone.to_string_lossy());
    h.get_by_label("Loop at End").click();
    h.run_steps(1);
    h.get_by_label("Add Sample").click();
    h.run_steps(2);
    let s = h.state().ctrl.vm().soundboards.boards[0].samples[0].clone();
    assert_eq!((s.name.as_str(), s.end_playback_behaviour), ("Beep", 1));
    h.get_by_label("Beep").click();
    h.run_steps(1);
    until(&mut h, 5, "sample playing", |a| {
        !a.ctrl.vm().voices.is_empty()
    });
    h.get_by_label("Stop all playback").click();
    h.run_steps(1);
    until(&mut h, 5, "sample stopped", |a| {
        a.ctrl.vm().voices.is_empty()
    });
    // numeric hotkey 1 plays the first sample while the soundboard is shown
    h.key_press(eframe::egui::Key::Num1);
    h.run_steps(1);
    until(&mut h, 5, "hotkey", |a| !a.ctrl.vm().voices.is_empty());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn latency_match_and_prompts() {
    let mut h = harness((400.0, 800.0));
    h.run_steps(1);
    let form = ConnectRequest {
        user: "alice".into(),
        group: "lm".into(),
        ..Default::default()
    };
    h.state_mut().ctrl.host_session(0, form).unwrap();
    let (port, addrs) = h.state().ctrl.vm().hosting.clone().unwrap();
    let mut bob = AppController::new(ControllerConfig {
        settings_dir: None,
        use_devices: false,
    })
    .unwrap();
    let host = addrs
        .first()
        .map_or("127.0.0.1".into(), ToString::to_string);
    bob.connect(ConnectRequest {
        host,
        port,
        user: "bob".into(),
        group: "lm".into(),
        ..Default::default()
    });
    let step = |h: &mut Harness<'_, SonobusApp>,
                bob: &mut AppController,
                secs: u64,
                what: &str,
                cond: &dyn Fn(&SonobusApp, &AppController) -> bool| {
        let t = Instant::now();
        while !cond(h.state(), bob) {
            assert!(
                t.elapsed() < Duration::from_secs(secs),
                "timed out: {what}; alice {:?} {}; bob {:?} {}",
                h.state().ctrl.vm().snapshot.group,
                h.state().ctrl.vm().status,
                bob.vm().snapshot.group,
                bob.vm().status
            );
            bob.tick();
            h.step();
            std::thread::sleep(Duration::from_millis(20));
        }
        h.step();
    };
    step(&mut h, &mut bob, 20, "peers", &|a, b| {
        a.ctrl.vm().snapshot.peers.len() == 1 && b.vm().snapshot.peers.len() == 1
    });
    h.state_mut().page = Page::Main;
    h.run_steps(1);
    h.get_by_label("Latency").click(); // the per-peer latency test
    h.run_steps(1);
    step(&mut h, &mut bob, 20, "latency measured", &|a, _| {
        !a.ctrl.vm().latency_tests.is_empty()
    });

    // alice asks for a group match from the dialog; bob gets the request
    h.state_mut().open_page(Page::LatencyMatch);
    step(&mut h, &mut bob, 20, "latency table", &|a, _| {
        a.ctrl.vm().latency_match.as_ref().is_some_and(|m| m.1)
    });
    h.get_by_label("Request Group Match").click();
    h.run_steps(1);
    assert_eq!(h.state().page, Page::Main);
    step(&mut h, &mut bob, 20, "request at bob", &|_, b| {
        b.vm().latency_request.is_some()
    });

    // bob asks back: alice sees the prompt and accepts
    bob.begin_latency_match();
    step(&mut h, &mut bob, 20, "bob's table", &|_, b| {
        b.vm().latency_match.as_ref().is_some_and(|m| m.1)
    });
    let target = bob.vm().latency_match.as_ref().unwrap().2 + 40.0;
    bob.send_latency_match(target);
    step(&mut h, &mut bob, 20, "prompt", &|a, _| {
        a.ctrl.vm().latency_request.is_some()
    });
    h.get_by_label("Match Latency").click();
    h.run_steps(1);
    assert!(h.state().ctrl.vm().latency_request.is_none());

    // suggest a group to bob from the dialog; his acceptance moves both
    h.state_mut().open_page(Page::SuggestGroup);
    h.run_steps(1);
    h.run_steps(1); // the page's accessibility tree is there one frame later
    type_into(&mut h, "Group", "lm2");
    assert_eq!(
        h.state().suggest.group,
        "lm2",
        "{:?} {:?}",
        h.state().ctrl.vm().latency_request,
        h.state().ctrl.vm().group_suggestion
    );
    h.get_by_label("Suggest and Connect to Group").click();
    h.run_steps(1);
    step(&mut h, &mut bob, 20, "suggestion at bob", &|_, b| {
        b.vm().group_suggestion.is_some()
    });
    bob.answer_group_suggestion(true);
    step(&mut h, &mut bob, 30, "moved", &|a, b| {
        a.ctrl.vm().snapshot.group.as_deref() == Some("lm2")
            && b.vm().snapshot.group.as_deref() == Some("lm2")
            && a.ctrl.vm().snapshot.peers.len() == 1
    });
}

#[test]
fn host_session_to_peer_effects() {
    let mut h = harness((1200.0, 800.0));
    h.run_steps(2);
    h.get_by_label("HOST").click();
    h.run_steps(2);
    // fill in the group through the labelled text field, then host
    h.get_by_label("Group Name:").focus();
    h.run_steps(1);
    h.get_by_label("Group Name:").type_text("ui-test");
    h.run_steps(1);
    h.get_by_label("Host Session").click();
    h.run_steps(1);
    until(&mut h, 10, "hosting", |a| a.ctrl.vm().hosting.is_some());
    let (port, addrs) = h.state().ctrl.vm().hosting.clone().unwrap();
    assert_eq!(h.state().form.group, "ui-test");

    // a second user joins the hosted session
    let mut bob = AppController::new(ControllerConfig {
        settings_dir: None,
        use_devices: false,
    })
    .unwrap();
    let host = addrs
        .first()
        .map_or("127.0.0.1".into(), ToString::to_string);
    bob.connect(ConnectRequest {
        host,
        port,
        user: "bob".into(),
        group: "ui-test".into(),
        ..Default::default()
    });
    let t = Instant::now();
    while h.state().ctrl.vm().snapshot.peers.is_empty() {
        assert!(
            t.elapsed() < Duration::from_secs(20),
            "bob never appeared: {}",
            h.state().ctrl.vm().status
        );
        bob.tick();
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.state_mut().page = Page::Main;
    h.run_steps(2);
    // the peer strip
    h.get_by_label("bob");
    h.get_by_label("Disable Sending");
    h.get_by_label("Receive Options").click();
    h.run_steps(2);
    h.get_by_label("Reset Dropped");
    // mute bob from the strip (the only peer MUTE; input groups have their own)
    let peer = h.state().ctrl.vm().snapshot.peers[0].id;
    h.get_all_by_label("MUTE").next().unwrap().click();
    h.run_steps(1);
    until(&mut h, 5, "peer muted", |a| {
        a.ctrl
            .vm()
            .snapshot
            .peers
            .iter()
            .any(|p| p.id == peer && !p.recv_allow)
    });

    // BLOCK asks first
    h.get_by_label("Send Options").click();
    h.run_steps(2);
    h.check_accessibility(); // a live peer strip with its sections open
    h.get_by_label("BLOCK").click_accesskit();
    h.run_steps(2);
    assert!(h.state().confirm_block.is_some());
    assert!(
        h.state().ctrl.engine().blocked_addresses().is_empty(),
        "not before confirming"
    );
    h.get_by_label("Cancel").click_accesskit();
    h.run_steps(2);
    assert!(
        h.state().confirm_block.is_none() && h.state().ctrl.engine().blocked_addresses().is_empty()
    );

    // the peer's effects panel
    h.get_all_by_label("Effects").next().unwrap().click(); // peers come before the input mixer
    h.run_steps(2);
    assert_eq!(h.state().page, Page::Effects(FxTarget::Peer(peer, 0)));
    h.get_by_label("Compressor");
    h.get_by_label("Parametric EQ");
    h.get_all_by_label("Threshold").next().unwrap();

    // chat through the UI reaches bob
    h.state_mut().open_page(Page::Chat);
    h.run_steps(2);
    h.get_by_label("Message Text").focus();
    h.run_steps(1);
    h.get_by_label("Message Text").type_text("hi bob");
    h.run_steps(1);
    h.get_by_label("Send").click();
    h.run_steps(1);
    let t = Instant::now();
    while !bob.vm().chat.iter().any(|m| m.message == "hi bob") {
        assert!(t.elapsed() < Duration::from_secs(10), "chat never arrived");
        bob.tick();
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn keyboard_shortcuts() {
    use eframe::egui::{Key, Modifiers};
    let mut h = harness((1200.0, 800.0));
    h.state_mut().page = Page::Main;
    h.run_steps(1);
    let params = |h: &Harness<'_, SonobusApp>| h.state().ctrl.vm().state.params.clone();
    assert!(!params(&h).main_in_mute);
    h.key_press_modifiers(Modifiers::COMMAND, Key::M);
    h.run_steps(1);
    assert!(params(&h).main_in_mute);
    h.key_press(Key::M);
    h.run_steps(1);
    assert!(!params(&h).main_in_mute);
    h.key_press_modifiers(Modifiers::COMMAND, Key::U);
    h.run_steps(1);
    assert!(params(&h).main_recv_mute);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Comma);
    h.run_steps(1);
    assert_eq!(h.state().page, Page::Options);
    // push to talk: T held unmutes send and mutes the others; release restores
    h.key_down(Key::T);
    h.run_steps(1);
    assert!(params(&h).main_recv_mute && !params(&h).main_send_mute);
    h.key_up(Key::T);
    h.run_steps(1);
    assert!(params(&h).main_recv_mute);

    // push to talk held from elsewhere (OSC) is not released by the next frames
    h.state_mut()
        .ctrl
        .edit(|s| (s.params.main_recv_mute, s.params.main_send_mute) = (false, true));
    h.state_mut().ctrl.push_to_talk(true);
    h.run_steps(3);
    assert!(
        params(&h).main_recv_mute && !params(&h).main_send_mute,
        "OSC push to talk released by the UI"
    );
    h.state_mut().ctrl.push_to_talk(false);
    h.run_steps(1);
    assert!(!params(&h).main_recv_mute && params(&h).main_send_mute);

    // app shortcuts stay out of a focused text field
    h.state_mut().open_page(Page::Chat);
    h.run_steps(2);
    h.get_by_label("Message Text").focus();
    h.run_steps(1);
    h.key_press_modifiers(Modifiers::COMMAND, Key::U);
    h.key_press_modifiers(Modifiers::COMMAND, Key::Comma);
    h.run_steps(1);
    assert!(!params(&h).main_recv_mute, "Ctrl-U while typing");
    assert_eq!(h.state().page, Page::Chat);
}

#[test]
fn unblock_and_hosting_indicator() {
    let mut h = harness((400.0, 800.0));
    let ip: std::net::IpAddr = "192.0.2.44".parse().unwrap();
    h.state_mut().ctrl.engine().set_blocked_addresses(vec![ip]);
    h.state_mut().open_page(Page::Options);
    h.run();
    h.get_by_label("Blocked addresses");
    h.get_by_label("Unblock").click_accesskit();
    h.run();
    assert!(h.state().ctrl.vm().blocked.is_empty());
    // a hosted server shows on every page, with Stop
    let form = ConnectRequest {
        user: "alice".into(),
        group: "ind".into(),
        ..Default::default()
    };
    h.state_mut().ctrl.host_session(0, form).unwrap();
    h.state_mut().open_page(Page::Main);
    h.run_steps(2);
    h.check_accessibility();
    assert!(h.query_by_label_contains("Hosting on").is_some());
    h.get_by_label("Stop Hosting").click_accesskit();
    h.run_steps(2);
    h.check_accessibility();
    assert!(h.state().ctrl.vm().hosting.is_none());
}

#[test]
fn max_send_quality_and_quality_requests_options() {
    use sonobus_engine::formats::FORMATS;
    let mut h = harness((1200.0, 800.0));
    h.state_mut().open_page(Page::Options);
    h.run();
    assert_eq!(h.state().ctrl.engine().max_send_quality(), None);
    h.get_by_label("Max Send Quality").click();
    h.run();
    h.get_by_label(FORMATS[2].name).click();
    h.run();
    assert_eq!(h.state().ctrl.engine().max_send_quality(), Some(2));
    assert_eq!(
        sonobus_engine::options::max_send_quality(&h.state().ctrl.vm().state.extra),
        Some(2),
        "the view follows"
    );
    h.get_by_label("Max Send Quality").click();
    h.run();
    h.get_by_label("No limit").click();
    h.run();
    assert_eq!(h.state().ctrl.engine().max_send_quality(), None);
    assert!(h.state().ctrl.engine().honour_quality_requests());
    h.get_by_label("Honour peers' quality requests").click();
    h.run();
    assert!(!h.state().ctrl.engine().honour_quality_requests());
    h.check_accessibility();
}
