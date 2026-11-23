//! Secure sessions through the controller: null-audio controllers in one process on 127.0.0.1,
//! relays disabled (`RelayChoice::None`), LAN discovery on loopback only: nothing leaves the
//! machine.

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use sonobus_app::controller::{AppController, ConnectRequest, ControllerConfig};
use sonobus_app::secure::{HostForm, JoinMethod, NetPlace};
use sonobus_engine::Connection;
use sonobus_engine::modern::{LanScope, RelayChoice, SessionRole};

fn controller() -> AppController {
    let mut c = AppController::new(ControllerConfig { settings_dir: None, use_devices: false }).expect("controller");
    c.net_place = NetPlace { bind_ip: Ipv4Addr::LOCALHOST.into(), port: 0, lan: LanScope::Loopback };
    c.set_secure_relay(RelayChoice::None);
    c
}

fn until(ctrls: &mut [&mut AppController], secs: u64, what: &str, cond: impl Fn(&[&mut AppController]) -> bool) {
    let t = Instant::now();
    loop {
        for c in ctrls.iter_mut() {
            c.tick();
        }
        if cond(ctrls) {
            return;
        }
        let status: Vec<_> = ctrls.iter().map(|c| c.vm().status.clone()).collect();
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}; status {status:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn host(c: &mut AppController, f: HostForm) -> String {
    c.host_secure("host", &f);
    until(&mut [&mut *c], 10, "hosting", |c| c[0].vm().secure.as_ref().is_some_and(|s| s.invite.is_some()));
    c.vm().secure.as_ref().unwrap().invite.clone().unwrap()
}

fn form(session: &str) -> HostForm {
    HostForm { session: session.into(), lan: false, ..HostForm::default() }
}

fn peers(c: &AppController) -> usize {
    c.vm().snapshot.peers.len()
}

fn metronome_to_peers(c: &mut AppController) {
    c.edit(|s| {
        s.params.met_enabled = true;
        s.params.send_met_audio = true;
        s.params.met_gain = 1.0;
    });
}

fn hears(c: &AppController, user: &str) -> bool {
    c.vm().snapshot.peers.iter().any(|p| p.user == user && p.meter.peak.iter().any(|&v| v > 0.01))
}

#[test]
fn host_mesh_join_by_invite_chat_and_meters_both_ways() {
    let (mut a, mut b) = (controller(), controller());
    let link = host(&mut a, form("jam"));
    assert!(link.starts_with("sonobus2://join/"));
    assert_eq!(a.vm().secure.as_ref().unwrap().role, SessionRole::Host);
    b.join_secure("bob", JoinMethod::Invite(link), false);
    until(&mut [&mut a, &mut b], 20, "peers", |c| c.iter().all(|c| peers(c) == 1 && c.vm().snapshot.connection == Connection::Connected));
    let sb = b.vm().secure.clone().unwrap();
    assert_eq!((sb.role, sb.session.as_str()), (SessionRole::Member, "jam"));
    let p = &b.vm().snapshot.peers[0];
    assert_eq!(p.user, "host");
    let net = p.net.as_ref().expect("a secure peer");
    assert!(net.is_invite_host, "{net:?}");
    assert_eq!(net.endpoint_id, a.vm().secure.as_ref().unwrap().endpoint_id);
    assert!(a.group_link().is_none(), "no legacy share link for a secure session");
    assert_eq!(b.vm().state.extra.last_username, "bob");

    a.send_chat(&[], "hello bob");
    b.send_chat(&[], "hello host");
    until(&mut [&mut a, &mut b], 10, "chat", |c| {
        c[1].vm().chat.iter().any(|m| m.message == "hello bob" && m.from == "host") && c[0].vm().chat.iter().any(|m| m.message == "hello host" && m.from == "bob")
    });
    metronome_to_peers(&mut a);
    metronome_to_peers(&mut b);
    until(&mut [&mut a, &mut b], 20, "meters both ways", |c| hears(c[0], "bob") && hears(c[1], "host"));
    until(&mut [&mut a, &mut b], 10, "direct path", |c| c[1].vm().snapshot.peers[0].net.as_ref().is_some_and(|n| n.direct && n.rtt_ms > 0.0));

    b.disconnect();
    until(&mut [&mut a, &mut b], 10, "left", |c| peers(c[0]) == 0 && c[1].vm().secure.is_none());
    assert_eq!(b.vm().snapshot.connection, Connection::Disconnected);
}

#[test]
fn broadcast_host_and_two_listeners() {
    let (mut h, mut l1, mut l2) = (controller(), controller(), controller());
    let link = host(&mut h, HostForm { broadcast: true, max_listeners: 2, ..form("radio") });
    l1.join_secure("l1", JoinMethod::Invite(link.clone()), false);
    l2.join_secure("l2", JoinMethod::Invite(link), false);
    until(&mut [&mut h, &mut l1, &mut l2], 20, "listeners", |c| peers(c[0]) == 2 && peers(c[1]) == 1 && peers(c[2]) == 1);
    for l in [&l1, &l2] {
        assert_eq!(l.vm().secure.as_ref().unwrap().role, SessionRole::Listener);
        // only the host, never the other listener
        assert_eq!(l.vm().snapshot.peers[0].user, "host");
    }
    metronome_to_peers(&mut h);
    until(&mut [&mut h, &mut l1, &mut l2], 20, "listeners hear the host", |c| hears(c[1], "host") && hears(c[2], "host"));
    run(&mut [&mut h, &mut l1, &mut l2], 1.0);
    assert!(peers(&l1) == 1 && peers(&l2) == 1, "listeners never learn of each other");
}

fn sync_error(c: &AppController) -> Option<f32> {
    c.vm().snapshot.peers.first()?.net.as_ref()?.playout_error_ms
}

/// "Sync listeners" on before hosting reaches the listener (it reports its sync error); turned
/// off mid-session it stops; the choice is remembered.
#[test]
fn broadcast_sync_listeners_setting() {
    let (mut h, mut l) = (controller(), controller());
    h.set_playout_sync(true, 5);
    assert_eq!((h.vm().secure_prefs.playout_sync, h.vm().secure_prefs.playout_ms), (true, sonobus_app::secure::MIN_PLAYOUT_MS), "clamped");
    h.set_playout_sync(true, 200);
    let link = host(&mut h, HostForm { broadcast: true, ..form("synced") });
    l.join_secure("l", JoinMethod::Invite(link), false);
    metronome_to_peers(&mut h);
    until(&mut [&mut h, &mut l], 20, "listener in sync", |c| sync_error(c[1]).is_some());
    h.set_playout_sync(false, 200);
    until(&mut [&mut h, &mut l], 10, "sync off", |c| sync_error(c[1]).is_none());
    assert_eq!(h.vm().secure_prefs.playout(), None);
    h.set_playout_sync(true, 200);
    until(&mut [&mut h, &mut l], 10, "sync on again mid-session", |c| sync_error(c[1]).is_some());
}

fn run(ctrls: &mut [&mut AppController], secs: f64) {
    let t = Instant::now();
    while t.elapsed().as_secs_f64() < secs {
        ctrls.iter_mut().for_each(|c| c.tick());
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn join_by_pin_from_the_lan_list_and_a_wrong_pin_is_calm() {
    let (mut a, mut b, mut c) = (controller(), controller(), controller());
    host(&mut a, HostForm { pin: true, lan: true, ..form("Friday jam") });
    let pin = a.vm().secure.as_ref().unwrap().pin.clone().expect("pin");
    assert!(a.vm().secure.as_ref().unwrap().lan_advertised);
    b.lan_browse(true);
    let t = Instant::now();
    while !b.vm().lan_sessions.iter().any(|s| s.session == "Friday jam") {
        if t.elapsed() > Duration::from_secs(8) {
            println!("SKIPPED: no mDNS over loopback (is multicast enabled on `lo`? `ip link set lo multicast on`)");
            return;
        }
        run(&mut [&mut a, &mut b], 0.1);
    }
    let d = b.vm().lan_sessions.iter().find(|s| s.session == "Friday jam").unwrap().clone();
    // a wrong PIN: a calm failure, not an error; the attempt is over
    let wrong = if pin == "000000" { "111111" } else { "000000" };
    c.join_secure("guess", JoinMethod::Lan(d.clone(), wrong.into()), false);
    until(&mut [&mut a, &mut c], 10, "refused", |x| x[1].vm().status.contains("not accepted"));
    assert!(!c.vm().status_is_error && c.vm().secure.is_none());
    until(&mut [&mut a, &mut c], 5, "counted", |x| x[0].vm().secure.as_ref().unwrap().failed_joins >= 1);
    b.join_secure("bob", JoinMethod::Lan(d, pin), false);
    until(&mut [&mut a, &mut b], 20, "joined by PIN", |x| peers(x[0]) == 1 && peers(x[1]) == 1);
    b.lan_browse(false);
    assert!(b.vm().lan_sessions.is_empty());
}

#[test]
fn join_by_name_through_the_hosts_rendezvous_and_throttled_guesses() {
    let (mut a, mut b, mut c) = (controller(), controller(), controller());
    host(&mut a, HostForm { password: "open sesame".into(), rendezvous_here: true, ..form("Thursday") });
    let server = sonobus_app::secure::rdv_text(a.vm().secure.as_ref().unwrap().rendezvous.as_ref().expect("rendezvous"));
    let by_name = |pw: &str| JoinMethod::Name { server: server.clone(), session: "Thursday".into(), password: pw.into() };
    b.join_secure("bob", by_name("open sesame"), false);
    until(&mut [&mut a, &mut b], 20, "joined by name", |x| peers(x[0]) == 1 && peers(x[1]) == 1);
    assert_eq!(b.vm().snapshot.peers[0].user, "host");
    assert_eq!(b.vm().secure_prefs.rendezvous, server, "the address is remembered");
    // wrong guesses: calm failures, then a retry-after from the host's back-off
    for i in 0..6 {
        c.join_secure("guess", by_name("wrong"), false);
        until(&mut [&mut a, &mut b, &mut c], 15, "refused", |x| x[2].vm().secure.is_none() && !x[2].vm().secure_starting);
        assert!(!c.vm().status_is_error, "{}", c.vm().status);
        if c.vm().secure_retry_at.is_some() {
            assert!(c.vm().status.contains("Try again in"), "{}", c.vm().status);
            assert!(i >= 2, "free attempts first");
            assert!(a.vm().secure.as_ref().unwrap().failed_joins >= 3);
            assert!(peers(&a) == 1 && peers(&b) == 1, "the session goes on");
            return;
        }
    }
    panic!("never throttled: {}", c.vm().status);
}

#[test]
fn removing_a_member_rotates_the_link() {
    let (mut a, mut b, mut c) = (controller(), controller(), controller());
    let old = host(&mut a, form("evict"));
    b.join_secure("bob", JoinMethod::Invite(old.clone()), false);
    c.join_secure("carol", JoinMethod::Invite(old.clone()), false);
    until(&mut [&mut a, &mut b, &mut c], 20, "three", |x| x.iter().all(|c| peers(c) == 2));
    let carol = a.vm().snapshot.peers.iter().find(|p| p.user == "carol").unwrap().id;
    let new = a.rotate_invite(Some(carol)).expect("new link");
    assert_ne!(new, old);
    assert!(a.vm().status.contains("carol"), "{}", a.vm().status);
    until(&mut [&mut a, &mut b, &mut c], 10, "carol out", |x| peers(x[0]) == 1 && peers(x[1]) == 1 && peers(x[2]) == 0);
    assert_eq!(a.vm().secure.as_ref().unwrap().invite.as_deref(), Some(new.as_str()));
    // the old link no longer works
    c.join_secure("carol", JoinMethod::Invite(old), false);
    until(&mut [&mut a, &mut b, &mut c], 10, "refused", |x| x[2].vm().status.contains("not accepted"));
    run(&mut [&mut a, &mut b, &mut c], 0.5);
    assert!(peers(&a) == 1 && peers(&b) == 1 && peers(&c) == 0);
}

#[test]
fn secure_then_compatible_then_secure_again() {
    let (mut a, mut b) = (controller(), controller());
    let both = |n: usize| move |x: &[&mut AppController]| x.iter().all(|c| peers(c) == n && c.vm().snapshot.peers.iter().all(|p| p.connected));
    let link = host(&mut a, form("one"));
    b.join_secure("bob", JoinMethod::Invite(link), false);
    until(&mut [&mut a, &mut b], 20, "secure", both(1));
    assert!(b.vm().snapshot.peers[0].net.is_some());
    // a compatible session ends the secure one
    let req = |u: &str| ConnectRequest { user: u.into(), group: "legacy".into(), ..ConnectRequest::default() };
    a.host_session(0, req("alice")).unwrap();
    let (port, _) = a.vm().hosting.clone().unwrap();
    b.connect(ConnectRequest { host: "127.0.0.1".into(), port, ..req("bob") });
    until(&mut [&mut a, &mut b], 20, "compatible", |x| both(1)(x) && x.iter().all(|c| c.vm().secure.is_none() && c.vm().snapshot.peers[0].net.is_none()));
    assert!(a.group_link().is_some());
    // and a secure session ends the compatible one
    let link = host(&mut a, form("two"));
    assert!(a.vm().hosting.is_none(), "the compatible server stopped");
    b.join_secure("bob", JoinMethod::Invite(link), false);
    until(&mut [&mut a, &mut b], 20, "secure again", |x| both(1)(x) && x[1].vm().snapshot.peers[0].net.is_some());
    assert_eq!(b.vm().snapshot.group.as_deref(), Some("two"));
    assert!(a.vm().request.is_none(), "nothing to rejoin on the old server");
}

#[test]
fn an_audio_rebuild_keeps_hosting_with_the_same_key() {
    let mut a = controller();
    let link = host(&mut a, form("rebuild"));
    let seed = |l: &str| sonobus_net::auth::Invite::parse(l).unwrap().seed;
    let setup = sonobus_app::audio::DeviceSetup { sample_rate: 44_100, ..a.audio_setup().clone() };
    a.set_audio_setup(setup).unwrap();
    until(&mut [&mut a], 10, "hosting again", |c| c[0].vm().secure.as_ref().is_some_and(|s| s.invite.is_some()));
    let again = a.vm().secure.as_ref().unwrap().invite.clone().unwrap();
    assert_eq!(seed(&again), seed(&link), "earlier invite links still name the session key");
}

#[test]
fn an_outside_invite_asks_first() {
    let mut a = controller();
    let link = host(&mut a, form("Sunday"));
    let mut b = controller();
    assert!(!b.open_invite("sonobus://h:1/?g=x"));
    assert!(b.open_invite(&link));
    let p = b.vm().pending_invite.clone().unwrap();
    let id = a.vm().secure.as_ref().unwrap().endpoint_id;
    assert_eq!((p.session.as_str(), p.host), ("Sunday", sonobus_app::secure::short_id(&id)));
    assert!(b.vm().secure.is_none() && !b.vm().secure_starting, "nothing before the answer");
    b.answer_pending_invite("bob", true);
    until(&mut [&mut a, &mut b], 20, "joined", |x| peers(x[0]) == 1 && peers(x[1]) == 1);
}

#[test]
fn private_chat_goes_by_id_when_names_collide() {
    let (mut a, mut b, mut c) = (controller(), controller(), controller());
    let link = host(&mut a, form("twins"));
    b.join_secure("bob", JoinMethod::Invite(link.clone()), false);
    c.join_secure("bob", JoinMethod::Invite(link), false);
    until(&mut [&mut a, &mut b, &mut c], 20, "three", |x| x.iter().all(|c| peers(c) == 2));
    let mut names: Vec<_> = a.vm().snapshot.peers.iter().map(|p| p.user.clone()).collect();
    names.sort();
    assert!(names[0] == "bob" && names[1].starts_with("bob #"), "{names:?}");
    // the second "bob" by id: only that device gets it
    let cid = c.vm().secure.as_ref().unwrap().endpoint_id;
    let to = a.vm().snapshot.peers.iter().find(|p| p.net.as_ref().is_some_and(|n| n.endpoint_id == cid)).unwrap().id;
    a.send_chat(&[to], "psst");
    let got = |c: &AppController| c.vm().chat.iter().any(|m| m.message == "psst" && !m.own);
    until(&mut [&mut a, &mut b, &mut c], 10, "private chat", |x| got(x[2]));
    run(&mut [&mut a, &mut b, &mut c], 0.5);
    assert!(!got(&b), "the other bob got it");
    assert!(a.vm().chat.iter().any(|m| m.own && m.message == "psst"));
}

#[test]
fn a_removed_member_is_told_and_leaves_the_session() {
    use sonobus_engine::modern::{END_BLOCKED, END_FULL, END_HOST_LEFT, END_REMOVED};
    let texts: Vec<String> = [END_REMOVED, END_BLOCKED, END_HOST_LEFT, END_FULL].map(sonobus_app::secure::ended_text).into();
    assert_eq!(texts, ["You were removed from the session", "A member blocked you", "The host ended the session", "The session is full"]);

    let (mut a, mut b) = (controller(), controller());
    let link = host(&mut a, form("out"));
    b.join_secure("bob", JoinMethod::Invite(link), false);
    until(&mut [&mut a, &mut b], 20, "joined", |x| peers(x[0]) == 1 && peers(x[1]) == 1);
    let (bob, bob_device) = (a.vm().snapshot.peers[0].id, b.vm().secure.as_ref().unwrap().endpoint_id);
    a.rotate_invite(Some(bob));
    until(&mut [&mut a, &mut b], 10, "told", |x| x[1].vm().secure.is_none());
    run(&mut [&mut a, &mut b], 0.3);
    assert_eq!(b.vm().status, "You were removed from the session");
    assert!(!b.vm().status_is_error);
    assert_eq!(b.vm().snapshot.connection, Connection::Disconnected);
    // the host keeps the device blocked until it says otherwise
    assert_eq!(a.vm().blocked_endpoints, [bob_device]);
    a.unblock_endpoint(bob_device);
    assert!(a.vm().blocked_endpoints.is_empty());
}
