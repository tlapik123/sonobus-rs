//! Secure sessions in the UI (egui_kittest, no window): every new page, tab and dialog at phone
//! and desktop size with every widget named, a click-through host -> copy link -> a second
//! user joins with the pasted link -> remove them, and a listener without send controls.
//! Relays off, loopback only: nothing leaves the machine.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use egui_kittest::Harness;
use egui_kittest::kittest::Queryable;
use sonobus_app::controller::{AppController, ConnectRequest, ControllerConfig};
use sonobus_app::secure::{HostForm, JoinMethod, NetPlace};
use sonobus_app::ui::{Page, Protocol, SecureTab, SonobusApp};
use sonobus_engine::modern::{LanScope, RelayChoice};

fn controller() -> AppController {
    let mut c = AppController::new(ControllerConfig { settings_dir: None, use_devices: false }).unwrap();
    c.net_place = NetPlace { bind_ip: Ipv4Addr::LOCALHOST.into(), port: 0, lan: LanScope::Loopback };
    c.set_secure_relay(RelayChoice::None);
    c
}

fn harness(size: (f32, f32), user: &str) -> Harness<'static, SonobusApp> {
    sonobus_app::i18n::init("en");
    let ctrl = controller();
    let form = ConnectRequest { user: user.into(), ..ctrl.default_request() };
    let mut app = SonobusApp::new(ctrl, form);
    app.protocol = Protocol::Secure;
    Harness::builder().with_size(size).build_ui_state(|ui, app: &mut SonobusApp| app.show(ui), app)
}

/// Steps the UI (and `others`) in real time until `cond` holds.
fn until(h: &mut Harness<'_, SonobusApp>, others: &mut [&mut AppController], secs: u64, what: &str, cond: impl Fn(&SonobusApp, &[&mut AppController]) -> bool) {
    let t = Instant::now();
    while !cond(h.state(), others) {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out: {what} ({})", h.state().ctrl.vm().status);
        others.iter_mut().for_each(|c| c.tick());
        h.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    h.step();
}

/// Opens every collapsed section on screen.
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

fn check(h: &mut Harness<'_, SonobusApp>) {
    h.run_steps(2);
    open_sections(h);
    h.check_accessibility();
}

fn invite(session: &str) -> String {
    sonobus_net::auth::Invite {
        seed: [1; 16],
        host: [7; 32],
        addrs: vec!["127.0.0.1:9".parse().unwrap()],
        relay: None,
        session: session.into(),
        mode: sonobus_engine::modern::SessionMode::Mesh,
    }
    .to_url()
    .unwrap()
}

#[test]
fn every_secure_page_and_dialog_at_both_sizes() {
    for size in [(400.0, 800.0), (1200.0, 800.0)] {
        let mut h = harness(size, "alice");
        h.state_mut().open_page(Page::Connect);
        // the explanation is always next to the choice
        h.run_steps(2);
        assert!(h.query_by_label_contains("End-to-end encrypted").is_some());
        h.get_by_label("Compatible (SonoBus 1.x)").click();
        h.run_steps(2);
        assert!(h.query_by_label_contains("Not encrypted: anyone on the network path").is_some());
        h.get_by_label("Secure (this app only)").click();
        h.run_steps(1);
        for tab in [SecureTab::Host, SecureTab::Join, SecureTab::Remembered] {
            h.state_mut().secure_tab = tab;
            check(&mut h);
        }
        // the host form with everything shown: listeners, a password and its hint, a bad
        // rendezvous address
        h.state_mut().secure_tab = SecureTab::Host;
        h.state_mut().host_form = HostForm { session: "x".into(), broadcast: true, password: "pw".into(), register_at: "nope".into(), ..HostForm::default() };
        check(&mut h);
        assert!(h.query_by_label_contains("720 guesses a day").is_some());
        assert!(h.query_by_label_contains("Weak:").is_some());
        // a pasted link shows what it names before joining
        h.state_mut().secure_tab = SecureTab::Join;
        h.state_mut().join.invite = invite("Preview me");
        check(&mut h);
        assert!(h.query_by_label_contains("Preview me").is_some());
        h.get_by_label("Join Session");
        // relay choices, each with its explanation
        for (choice, text) in [
            (RelayChoice::N0Default, "they see who connects to whom, never your audio"),
            (RelayChoice::Custom(vec!["https://relay.example/".into()]), "Relay URL"),
            (RelayChoice::None, "cannot connect over the internet"),
        ] {
            h.state_mut().ctrl.set_secure_relay(choice);
            check(&mut h);
            assert!(h.query_by_label_contains(text).is_some(), "{text}");
        }
        h.state_mut().ctrl.set_secure_relay(RelayChoice::None);
        // dialogs: a link from outside, removing a member, a new identity
        assert!(h.state_mut().ctrl.open_invite(&invite("From outside")));
        check(&mut h);
        assert!(h.query_by_label_contains("From outside").is_some());
        h.get_by_label("Cancel").click_accesskit();
        h.run_steps(2);
        assert!(h.state().ctrl.vm().pending_invite.is_none() && h.state().ctrl.vm().secure.is_none());
        h.state_mut().confirm_remove = Some((sonobus_engine::PeerId(1), "bob".into()));
        check(&mut h);
        h.state_mut().confirm_remove = None;
        h.state_mut().confirm_reset_identity = true;
        check(&mut h);
        h.state_mut().confirm_reset_identity = false;
        h.state_mut().open_page(Page::Options);
        check(&mut h);
        h.get_by_label("Reset device identity...");

        // in a session (hosting, with a PIN): the panel on the connect page, the QR code, the
        // badge in the status bar, the session on the main page
        let f = HostForm { session: "Badges".into(), pin: true, lan: false, ..HostForm::default() };
        h.state_mut().ctrl.host_secure("alice", &f);
        until(&mut h, &mut [], 10, "hosting", |a, _| a.ctrl.vm().secure.as_ref().is_some_and(|s| s.invite.is_some()));
        h.state_mut().open_page(Page::Connect);
        h.state_mut().show_qr = true;
        check(&mut h);
        h.get_by_label("QR code of the invite link");
        h.get_by_label("Copy Invite Link");
        h.get_by_label("New PIN");
        assert!(h.query_all_by_label_contains("Secure session").next().is_some());
        h.state_mut().open_page(Page::Main);
        check(&mut h);
        h.get_by_label("Leave Session");
    }
}

#[test]
fn host_copy_link_join_with_the_pasted_link_and_remove() {
    let mut alice = harness((1200.0, 800.0), "alice");
    alice.state_mut().open_page(Page::Connect);
    alice.run_steps(2);
    alice.get_by_label("Session name").focus();
    alice.run_steps(1);
    alice.get_by_label("Session name").type_text("ui-secure");
    alice.run_steps(1);
    // advertising needs a PIN or password: untick it, an invite link is enough
    alice.get_by_label("Advertise on the local network").click();
    alice.run_steps(1);
    alice.get_by_label("Host Secure Session").click();
    alice.run_steps(1);
    until(&mut alice, &mut [], 10, "hosting", |a, _| a.ctrl.vm().secure.as_ref().is_some_and(|s| s.invite.is_some()));
    alice.get_by_label("Copy Invite Link").click();
    alice.run_steps(1);
    let copied = alice.output().platform_output.commands.iter().find_map(|c| match c {
        egui::OutputCommand::CopyText(t) => Some(t.clone()),
        _ => None,
    });
    let link = alice.state().ctrl.vm().secure.as_ref().unwrap().invite.clone().unwrap();
    assert_eq!(copied.as_deref(), Some(link.as_str()), "the invite link is on the clipboard");

    // bob pastes it into his join tab and joins
    let mut bob = harness((400.0, 800.0), "bob");
    bob.state_mut().open_page(Page::Connect);
    bob.state_mut().secure_tab = SecureTab::Join;
    bob.run_steps(2);
    bob.get_by_label("Invite link").focus();
    bob.run_steps(1);
    bob.event(egui::Event::Paste(link.clone()));
    bob.run_steps(2);
    assert_eq!(bob.state().join.invite, link);
    assert!(bob.query_by_label_contains("ui-secure").is_some(), "the preview names the session");
    bob.get_by_label("Join Session").click();
    bob.run_steps(1);
    let t = Instant::now();
    while alice.state().ctrl.vm().snapshot.peers.is_empty() || bob.state().ctrl.vm().snapshot.peers.is_empty() {
        assert!(t.elapsed() < Duration::from_secs(20), "bob never joined: {}", bob.state().ctrl.vm().status);
        alice.step();
        bob.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    bob.run_steps(2);
    bob.check_accessibility();
    assert!(bob.query_all_by_label_contains("Secure session").next().is_some(), "the badge");

    // alice removes bob: asked first, then a new link, bob is out
    alice.state_mut().open_page(Page::Connect);
    alice.run_steps(2);
    alice.get_by_label("Remove from session").click();
    alice.run_steps(2);
    assert!(alice.state().confirm_remove.is_some());
    alice.check_accessibility();
    assert!(!alice.state().ctrl.vm().snapshot.peers.is_empty(), "not before confirming");
    alice.get_by_label("Remove").click_accesskit();
    alice.run_steps(2);
    let t = Instant::now();
    while !(alice.state().ctrl.vm().snapshot.peers.is_empty() && bob.state().ctrl.vm().snapshot.peers.is_empty()) {
        assert!(t.elapsed() < Duration::from_secs(10), "bob still in");
        alice.step();
        bob.step();
        std::thread::sleep(Duration::from_millis(20));
    }
    let new = alice.state().ctrl.vm().secure.as_ref().unwrap().invite.clone().unwrap();
    assert_ne!(new, link);
    assert!(alice.state().ctrl.vm().status.contains("bob"), "{}", alice.state().ctrl.vm().status);
    // bob is told; alice sees his device blocked, by its short id, and can unblock it
    bob.run_steps(2);
    assert_eq!(bob.state().ctrl.vm().status, "You were removed from the session");
    assert!(bob.state().ctrl.vm().secure.is_none());
    alice.run_steps(2);
    alice.check_accessibility();
    alice.get_by_label("Blocked devices");
    let short = sonobus_app::secure::short_id(&alice.state().ctrl.vm().blocked_endpoints[0]);
    assert!(alice.query_by_label_contains(&short).is_some(), "{short}");
    alice.get_by_label("Unblock").click_accesskit();
    alice.run_steps(2);
    assert!(alice.state().ctrl.vm().blocked_endpoints.is_empty());
    assert!(alice.query_by_label("Blocked devices").is_none());
}

#[test]
fn a_listener_has_no_send_controls() {
    let mut host = controller();
    host.host_secure("host", &HostForm { session: "radio".into(), broadcast: true, lan: false, ..HostForm::default() });
    let mut l = harness((400.0, 800.0), "listener");
    until(&mut l, &mut [&mut host], 10, "hosting", |_, c| c[0].vm().secure.as_ref().is_some_and(|s| s.invite.is_some()));
    let link = host.vm().secure.as_ref().unwrap().invite.clone().unwrap();
    l.state_mut().ctrl.join_secure("listener", JoinMethod::Invite(link), false);
    until(&mut l, &mut [&mut host], 20, "listening", |a, c| a.ctrl.vm().snapshot.peers.len() == 1 && c[0].vm().snapshot.peers.len() == 1);
    l.state_mut().open_page(Page::Main);
    check(&mut l);
    for gone in ["Send Mute", "Disable Sending", "Send Options", "Send Metronome to All", "Send meter", "Latency"] {
        assert!(l.query_by_label(gone).is_none(), "{gone} shown to a listener");
    }
    assert!(l.query_all_by_label_contains("Listener").next().is_some());
    l.get_by_label("Mute All Users");
    // the host keeps them
    let mut h = harness((1200.0, 800.0), "member");
    h.state_mut().ctrl.host_secure("member", &HostForm { session: "jam".into(), lan: false, ..HostForm::default() });
    until(&mut h, &mut [], 10, "hosting", |a, _| a.ctrl.vm().secure.is_some());
    h.state_mut().open_page(Page::Main);
    h.run_steps(2);
    h.get_by_label("Send Mute");
}
