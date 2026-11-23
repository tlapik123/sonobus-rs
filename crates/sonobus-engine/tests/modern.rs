//! Modern (protocol v2) sessions end to end: engines in one process over loopback QUIC, relays
//! disabled (`RelayChoice::None`), nothing leaves the machine. Mirrors the core of
//! `session.rs` / `stage2.rs` for modern sessions, plus the v2-only paths (join methods,
//! broadcast, adaptation, eviction, the legacy <-> modern switch).
#![cfg(feature = "modern")]

mod common;

use std::net::Ipv4Addr;

use common::*;
use sonobus_core::files::{SoundSample, Soundboard, Soundboards};
use sonobus_dsp::gain::DEFAULT_CENTER_PAN_LAW as LAW;
use sonobus_engine::modern::{
    AuthKind, DiscoveredSession, HostOptions, LanScope, NetOptions, PeerAddr, RelayChoice,
    SessionEvent, SessionMode, SessionRole,
};
use sonobus_engine::{Connection, Event, Notification};

const CENTRE: f32 = AMP * LAW;
const TOL: f32 = 0.15;

/// Loopback only, ephemeral port, no relays.
fn net(name: &str) -> NetOptions {
    NetOptions {
        name: name.into(),
        bind_ip: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        relay: RelayChoice::None,
    }
}

/// `nodes[0]` hosts `session` (mesh), the others join by its invite link; true once every
/// node hears every other one.
fn mesh(nodes: &mut [&mut Node], session: &str) {
    let st = nodes[0]
        .e
        .host_session(&net("user0"), HostOptions::new(session))
        .expect("host");
    let link = st.invite.expect("invite link");
    assert_eq!(st.role, SessionRole::Host);
    for (i, n) in nodes.iter().enumerate().skip(1) {
        n.e.join_invite(&net(&format!("user{i}")), &link)
            .expect("join");
    }
    let count = nodes.len() - 1;
    assert!(
        run_until(nodes, 15.0, |n| all_receiving(n, count)),
        "peers did not connect: {}",
        nodes
            .iter()
            .map(|n| diag(n))
            .collect::<Vec<_>>()
            .join(" | ")
    );
}

fn hears(nodes: &mut [&mut Node], node: usize, ch: usize, freq: f64, amp: f32) -> bool {
    let ok = eventually(nodes, 5.0, |n| near(n[node].tone(ch, freq), amp, TOL));
    if !ok {
        eprintln!(
            "node {node} ch {ch}: {freq} Hz at {} (want {amp}) {}",
            nodes[node].tone(ch, freq),
            diag(nodes[node])
        );
    }
    ok
}

/// Encodings `nodes[0]` makes over one second of audio, not of wall time: a macOS CI runner
/// got through less audio than that in a second and counted 242 where 400 are made.
fn encodes_per_second(nodes: &mut [&mut Node]) -> u64 {
    let before = nodes[0].e.modern_encode_count();
    let left = std::cell::Cell::new(SR as usize / BLOCK);
    run_until(nodes, 10.0, |_| {
        left.set(left.get() - 1);
        left.get() == 0
    });
    run(nodes, 0.1); // the send thread encodes what was captured last
    nodes[0].e.modern_encode_count() - before
}

/// `node` stops hearing `freq` on `ch`. Not checked at one moment: the analysis window is
/// half a second of audio, which a slow test loop (macOS CI) has not replaced when expected.
fn silent(nodes: &mut [&mut Node], node: usize, ch: usize, freq: f64) -> bool {
    eventually(nodes, 5.0, |n| n[node].tone(ch, freq) < 0.01)
}

#[test]
fn two_engines_opus_mono_by_invite_then_leave() {
    let _serial = serial();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    mesh(&mut [&mut a, &mut b], "jam");
    for ch in 0..2 {
        assert!(
            hears(&mut [&mut a, &mut b], 1, ch, 440.0, CENTRE),
            "b hears a"
        );
        assert!(
            hears(&mut [&mut a, &mut b], 0, ch, 660.0, CENTRE),
            "a hears b"
        );
    }
    let s = b.snap();
    assert_eq!(s.connection, Connection::Connected);
    assert_eq!(s.group.as_deref(), Some("jam"));
    let p = &s.peers[0];
    assert_eq!(
        (p.user.as_str(), p.recv_channels, p.send_channels),
        ("user0", 1, 1)
    );
    assert_eq!(p.recv_format.as_deref(), Some("96 kbps/ch"));
    assert!(p.packets_received > 100 && p.recv_bps > 50_000.0, "{p:?}");
    let net = p.net.as_ref().expect("modern peer");
    assert!(net.is_invite_host && net.direct, "{net:?}");
    assert_eq!(net.endpoint_id, a.e.session_status().unwrap().endpoint_id);
    assert!(!p.latency.legacy, "{:?}", p.latency);
    assert!(p.meter.peak[0] > 0.3);
    assert!(
        !b.e.start_latency_test(p.id),
        "no measured test in modern sessions"
    );

    b.e.leave_session();
    assert!(run_until(&mut [&mut a, &mut b], 5.0, |n| n[0]
        .snap()
        .peers
        .is_empty()
        && n[1].snap().peers.is_empty()));
    run(&mut [&mut a, &mut b], 0.05);
    assert!(a.has(|e| matches!(e, Event::PeerLeft { user, .. } if user == "user1")));
    assert!(b.e.session_status().is_none());
    let _ = std::iter::from_fn(|| a.e.poll_session_event()).collect::<Vec<SessionEvent>>();
}

#[test]
fn pcm_stereo_and_four_channel_layout() {
    let _serial = serial();
    let mut st = state(groups(&[(0, 2)]));
    st.params.send_channels = 2;
    st.params.def_send_qual = 8; // PCM 16 bit
    let mut a = Node::new(&[300.0, 500.0], 2, st);
    let mut b = Node::mono(700.0);
    mesh(&mut [&mut a, &mut b], "pcm");
    assert!(eventually(&mut [&mut a, &mut b], 5.0, |n| near(
        n[1].tone(0, 300.0),
        AMP,
        0.03
    ) && near(
        n[1].tone(1, 500.0),
        AMP,
        0.03
    )));
    assert!(b.tone(1, 300.0) < 0.005 && b.tone(0, 500.0) < 0.005);
    let p = &b.snap().peers[0];
    assert_eq!(
        (p.recv_channels, p.recv_format.as_deref()),
        (2, Some("PCM 16 bit"))
    );
    drop((a, b));

    // "match inputs" with four channels in three groups: the receiver adopts the layout
    let mut st = state(groups(&[(0, 2), (2, 1), (3, 1)]));
    st.params.send_channels = 0;
    st.input_groups[1].pans[0] = -1.0;
    st.input_groups[2].pans[0] = 1.0;
    let mut a = Node::new(&[300.0, 500.0, 700.0, 900.0], 2, st);
    let mut b = Node::mono(1100.0);
    mesh(&mut [&mut a, &mut b], "four");
    let placed = |n: &[&mut Node]| {
        [(0, 300.0), (1, 500.0), (0, 700.0), (1, 900.0)]
            .iter()
            .all(|&(ch, f)| near(n[1].tone(ch, f), AMP, TOL) && n[1].tone(1 - ch, f) < 0.01)
    };
    assert!(
        eventually(&mut [&mut a, &mut b], 5.0, placed),
        "{}",
        diag(&b)
    );
    let p = &b.snap().peers[0];
    assert_eq!(p.recv_channels, 4);
    let layout: Vec<_> = p
        .groups
        .iter()
        .map(|g| (g.chan_start, g.num_channels))
        .collect();
    assert_eq!(layout, [(0, 2), (2, 1), (3, 1)]);
}

#[test]
fn three_engines_controls_then_the_host_leaves() {
    let _serial = serial();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(990.0));
    mesh(&mut [&mut a, &mut b, &mut c], "three");
    for (node, f) in [
        (0, 660.0),
        (0, 990.0),
        (1, 440.0),
        (1, 990.0),
        (2, 440.0),
        (2, 660.0),
    ] {
        assert!(hears(&mut [&mut a, &mut b, &mut c], node, 0, f, CENTRE));
    }
    let (pa, pc) = (b.peer("user0"), b.peer("user2"));
    b.e.set_peer_level(pa, 0.5);
    assert!(hears(
        &mut [&mut a, &mut b, &mut c],
        1,
        0,
        440.0,
        CENTRE * 0.25
    ));
    b.e.set_peer_level(pa, 1.0);
    b.e.update_peer_groups(pa, |g, _| g[0].pans[0] = -1.0);
    assert!(hears(&mut [&mut a, &mut b, &mut c], 1, 0, 440.0, AMP));
    assert!(silent(&mut [&mut a, &mut b, &mut c], 1, 1, 440.0));
    b.e.set_peer_soloed(pc, true);
    run(&mut [&mut a, &mut b, &mut c], 0.6);
    assert!(silent(&mut [&mut a, &mut b, &mut c], 1, 0, 440.0));
    b.e.set_peer_soloed(pc, false);
    // receive mute: the stream is unsubscribed
    b.e.set_peer_recv_muted(pc, true);
    run(&mut [&mut a, &mut b, &mut c], 1.0);
    assert!(silent(&mut [&mut a, &mut b, &mut c], 1, 0, 990.0));
    assert!(
        !b.snap()
            .peers
            .iter()
            .find(|p| p.id == pc)
            .unwrap()
            .recv_active
    );
    b.e.set_peer_recv_muted(pc, false);
    assert!(
        hears(&mut [&mut a, &mut b, &mut c], 1, 0, 990.0, CENTRE),
        "receive resumes"
    );
    // a stops sending to b only
    let pb = a.peer("user1");
    a.e.set_peer_send_muted(pb, true);
    run(&mut [&mut a, &mut b, &mut c], 1.0);
    assert!(silent(&mut [&mut a, &mut b, &mut c], 1, 0, 440.0));
    assert!(
        hears(&mut [&mut a, &mut b, &mut c], 2, 0, 440.0, CENTRE),
        "c still hears a"
    );
    a.e.set_peer_send_muted(pb, false);
    assert!(
        hears(&mut [&mut a, &mut b, &mut c], 1, 0, 440.0, AMP),
        "sending resumes"
    );
    // encode once: three peers, one shared stream (two encodings per block), no private ones
    let per_sec = encodes_per_second(&mut [&mut a, &mut b, &mut c]);
    // 240-frame Opus blocks: 200 per second, times main + low
    assert!((300..=480).contains(&per_sec), "{per_sec}");

    // the host leaves: b and c keep each other
    a.e.leave_session();
    assert!(run_until(&mut [&mut a, &mut b, &mut c], 5.0, |n| n[1]
        .snap()
        .peers
        .len()
        == 1
        && n[2].snap().peers.len() == 1));
    assert!(hears(&mut [&mut a, &mut b, &mut c], 1, 0, 990.0, CENTRE));
    assert!(hears(&mut [&mut a, &mut b, &mut c], 2, 0, 660.0, CENTRE));
}

#[test]
fn master_mutes_and_format_changes() {
    let _serial = serial();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    mesh(&mut [&mut a, &mut b], "fmt");
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    a.e.update_state(|s| s.params.main_send_mute = true);
    run(&mut [&mut a, &mut b], 1.0);
    assert!(b.tone(0, 440.0) < 0.01, "master send mute");
    a.e.update_state(|s| s.params.main_send_mute = false);
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    b.e.update_state(|s| s.params.main_recv_mute = true);
    run(&mut [&mut a, &mut b], 1.0);
    assert!(b.tone(0, 440.0) < 0.01, "master receive mute");
    b.e.update_state(|s| s.params.main_recv_mute = false);
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    // a per-peer format outside the shared stream's encodings: its own encoder
    let pb = a.peer("user1");
    a.e.set_peer_send_format(pb, 10);
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| n[1]
        .snap()
        .peers[0]
        .recv_format
        .as_deref()
        == Some("PCM 32 bit float")));
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    // the receiver asks for another format; the sender follows
    let pa = b.peer("user0");
    b.e.request_peer_send_format(pa, Some(8));
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| n[0]
        .snap()
        .peers[0]
        .send_format
        == 8
        && n[1].snap().peers[0].recv_format.as_deref()
            == Some("PCM 16 bit")));
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    // back to the default: the shared stream again
    a.e.set_peer_send_format(pb, 4);
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| n[1]
        .snap()
        .peers[0]
        .recv_format
        .as_deref()
        == Some("96 kbps/ch")));
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    // "change for all": the shared stream's format follows
    a.e.set_send_format_for_all(9);
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| n[1]
        .snap()
        .peers[0]
        .recv_format
        .as_deref()
        == Some("PCM 24 bit")));
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
}

fn session_events(n: &Node) -> Vec<SessionEvent> {
    std::iter::from_fn(|| n.e.poll_session_event()).collect()
}

/// Run until `who` reports a session event matching `f`.
fn wait_event(
    nodes: &mut [&mut Node],
    who: usize,
    secs: f64,
    f: impl Fn(&SessionEvent) -> bool,
) -> Option<SessionEvent> {
    let start = std::time::Instant::now();
    while start.elapsed().as_secs_f64() < secs {
        run(nodes, 0.1);
        if let Some(e) = session_events(nodes[who]).into_iter().find(|e| f(e)) {
            return Some(e);
        }
    }
    None
}

fn auth_failed(e: &SessionEvent) -> bool {
    matches!(e, SessionEvent::JoinFailed { reason, .. } if reason == "authentication failed")
}

#[test]
fn join_by_pin_through_lan_discovery() {
    let _serial = serial();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(880.0));
    let mut o = HostOptions::new("Friday jam");
    o.pin = true;
    o.lan = LanScope::Loopback;
    let st = a.e.host_session(&net("host"), o).expect("host");
    let pin = st.pin.expect("pin");
    assert!(st.lan_advertised);
    b.e.lan_browse(LanScope::Loopback).unwrap();
    let mut found = None;
    let start = std::time::Instant::now();
    while found.is_none() && start.elapsed().as_secs() < 8 {
        run(&mut [&mut a, &mut b], 0.1);
        found =
            b.e.lan_sessions()
                .into_iter()
                .find(|s| s.host.id == st.endpoint_id);
    }
    let Some(s) = found else {
        println!(
            "SKIPPED: no mDNS over loopback (is multicast enabled on `lo`? \
             `ip link set lo multicast on`)"
        );
        return;
    };
    assert_eq!(s.session, "Friday jam");
    // a wrong PIN: authentication fails, nobody is admitted
    c.e.join_lan(
        &net("guess"),
        &s,
        if pin == "000000" { "111111" } else { "000000" },
    )
    .unwrap();
    assert!(wait_event(&mut [&mut a, &mut b, &mut c], 2, 6.0, auth_failed).is_some());
    assert!(a.snap().peers.is_empty() && c.snap().peers.is_empty());
    assert!(a.e.session_status().unwrap().failed_joins >= 1);
    // the right one
    b.e.join_lan(&net("joiner"), &s, &pin).unwrap();
    assert!(
        run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1)),
        "{}",
        diag(&a)
    );
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    assert!(hears(&mut [&mut a, &mut b], 0, 0, 660.0, CENTRE));
    b.e.stop_lan_browse();
}

#[test]
fn join_by_name_through_the_hosts_rendezvous() {
    let _serial = serial();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(880.0));
    let mut o = HostOptions::new("Thursday");
    o.password = Some("open sesame".into());
    o.rendezvous = true;
    let st = a.e.host_session(&net("host"), o).expect("host");
    let server = st.rendezvous.expect("rendezvous address");
    c.e.join_by_name(&net("intruder"), server.clone(), "Thursday", "wrong")
        .unwrap();
    assert!(wait_event(&mut [&mut a, &mut c], 1, 8.0, auth_failed).is_some());
    assert!(a.snap().peers.is_empty() && c.snap().peers.is_empty());
    b.e.join_by_name(&net("joiner"), server, "Thursday", "open sesame")
        .unwrap();
    assert!(
        run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1)),
        "{}",
        diag(&a)
    );
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    assert!(hears(&mut [&mut a, &mut b], 0, 0, 660.0, CENTRE));
    assert_eq!(b.snap().peers[0].user, "host");
    let st = b.e.session_status().unwrap();
    assert_eq!((st.mode, st.role), (SessionMode::Mesh, SessionRole::Member));
}

#[test]
fn broadcast_host_three_listeners_encode_once_and_isolation() {
    let _serial = serial();
    let mut h = Node::mono(440.0);
    let mut ls: Vec<Node> = (0..4)
        .map(|i| Node::mono(500.0 + 100.0 * i as f64))
        .collect();
    let mut o = HostOptions::new("radio");
    o.mode = SessionMode::Broadcast;
    o.max_listeners = Some(3);
    let link = h.e.host_session(&net("host"), o).unwrap().invite.unwrap();
    for (i, l) in ls.iter().take(3).enumerate() {
        l.e.join_invite(&net(&format!("l{i}")), &link).unwrap();
    }
    {
        let [l0, l1, l2, _] = &mut ls[..] else {
            unreachable!()
        };
        let nodes = &mut [&mut h, l0, l1, l2];
        assert!(
            run_until(nodes, 15.0, |n| n[0].snap().peers.len() == 3
                && n[1..].iter().all(
                    |l| l.snap().peers.len() == 1 && l.snap().peers[0].recv_active
                )),
            "{}",
            diag(nodes[0])
        );
        for k in 1..4 {
            assert!(
                hears(nodes, k, 0, 440.0, CENTRE),
                "listener {k} hears the host"
            );
        }
        // listeners send nothing: the host hears none of them
        run(nodes, 0.5);
        for k in 0..3 {
            assert!(nodes[0].tone(0, 500.0 + 100.0 * k as f64) < 0.01);
        }
        // one shared stream for three listeners: the encoder runs once per block and
        // encoding, not per listener
        let per_sec = encodes_per_second(nodes);
        assert!((300..=480).contains(&per_sec), "{per_sec}");
        for l in &nodes[1..] {
            assert_eq!(l.e.modern_encode_count(), 0, "listeners have no encoder");
            let st = l.e.session_status().unwrap();
            assert_eq!(st.role, SessionRole::Listener);
            // isolation: only the host, never another listener
            let p = &l.snap().peers;
            assert_eq!(p.len(), 1);
            assert!(p[0].net.as_ref().unwrap().is_invite_host);
            assert_eq!(p[0].user, "host");
        }
        // a latency-info request to the hub reveals no other listener: exactly our own path
        // and the hub's answer for our connection (an unanswered request would not do)
        nodes[1].e.begin_latency_match();
        run(nodes, 1.0);
        let (entries, ready, _) = nodes[1].e.latency_match_info();
        let paths: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.src_name.as_str(), e.dest_name.as_str()))
            .collect();
        assert_eq!(paths, [("host", "l0"), ("l0", "host")], "{entries:?}");
        assert!(ready);
    }
    // the fourth is refused: the host is full
    let [l0, l1, l2, l3] = &mut ls[..] else {
        unreachable!()
    };
    l3.e.join_invite(&net("l3"), &link).unwrap();
    let nodes = &mut [&mut h, l0, l1, l2, l3];
    assert!(wait_event(nodes, 4, 8.0, |e| *e == SessionEvent::SessionFull).is_some());
    assert_eq!(nodes[0].snap().peers.len(), 3);
    // the host leaves: the session is over for its listeners
    nodes[0].e.leave_session();
    assert!(wait_event(nodes, 1, 5.0, |e| matches!(e, SessionEvent::Ended { .. })).is_some());
}

/// Broadcast isolation for listeners that join by name and password (not by invite). L1 joins
/// by name; L2 first aims a password join straight at L1 (as a LAN join, or a rendezvous
/// listing that names L1, would): L1 is a listener and serves no joins, so that fails. L2 then
/// joins by name the ordinary way. Each feeds its own tone: neither ever hears the other or has
/// it as a peer, both hear the host, neither runs an encoder.
#[test]
fn password_joined_broadcast_listeners_stay_isolated() {
    let _serial = serial();
    let (mut h, mut l1, mut l2) = (Node::mono(440.0), Node::mono(550.0), Node::mono(660.0));
    let mut o = HostOptions::new("radio pw");
    o.mode = SessionMode::Broadcast;
    o.password = Some("open sesame".into());
    o.rendezvous = true;
    let server =
        h.e.host_session(&net("host"), o)
            .unwrap()
            .rendezvous
            .unwrap();
    l1.e.join_by_name(&net("l1"), server.clone(), "radio pw", "open sesame")
        .unwrap();
    assert!(run_until(&mut [&mut h, &mut l1], 15.0, |n| {
        n[1].snap().peers.first().is_some_and(|p| p.recv_active)
    }));
    let st = l1.e.session_status().unwrap();
    let to = DiscoveredSession {
        session: "radio pw".into(),
        auth: AuthKind::Password,
        host: PeerAddr {
            id: st.endpoint_id,
            addrs: st.addrs.clone(),
            relay: None,
        },
    };
    l2.e.join_lan(&net("l2"), &to, "open sesame").unwrap();
    let nodes = &mut [&mut h, &mut l1, &mut l2];
    let isolated = |nodes: &mut [&mut Node], what: &str| {
        run(nodes, 2.0);
        let (l1_hears_l2, l2_hears_l1) = (nodes[1].tone(0, 660.0), nodes[2].tone(0, 550.0));
        let users = |n: &Node| {
            n.snap()
                .peers
                .iter()
                .map(|p| p.user.clone())
                .collect::<Vec<_>>()
        };
        let (p1, p2) = (users(nodes[1]), users(nodes[2]));
        eprintln!(
            "{what}: l1 hears l2 at {l1_hears_l2}, l2 hears l1 at {l2_hears_l1}; peers {p1:?} / {p2:?}"
        );
        assert!(
            l1_hears_l2 < 0.01 && l2_hears_l1 < 0.01,
            "{what}: audio between listeners"
        );
        assert_eq!(p1, ["host"], "{what}");
        assert!(p2.iter().all(|u| u == "host"), "{what}: {p2:?}");
        assert_eq!(
            nodes[1].e.modern_encode_count(),
            0,
            "{what}: l1 has no encoder"
        );
    };
    let failed = wait_event(nodes, 2, 8.0, |e| {
        matches!(e, SessionEvent::JoinFailed { .. })
    });
    isolated(nodes, "join aimed at l1");
    assert_eq!(
        (st.mode, st.role),
        (SessionMode::Broadcast, SessionRole::Listener)
    );
    assert!(failed.is_some(), "a listener serves no password joins");
    assert!(nodes[2].snap().peers.is_empty());

    nodes[2].e.leave_session();
    nodes[2]
        .e
        .join_by_name(&net("l2"), server, "radio pw", "open sesame")
        .unwrap();
    assert!(run_until(nodes, 15.0, |n| n[0].snap().peers.len() == 2
        && n[2].snap().peers.first().is_some_and(|p| p.recv_active)));
    isolated(nodes, "join by name");
    for k in 1..3 {
        assert!(
            hears(nodes, k, 0, 440.0, CENTRE),
            "listener {k} hears the host"
        );
        assert_eq!(
            nodes[k].e.modern_encode_count(),
            0,
            "listener {k} has no encoder"
        );
        let st = nodes[k].e.session_status().unwrap();
        assert_eq!(
            (st.mode, st.role),
            (SessionMode::Broadcast, SessionRole::Listener)
        );
    }
    // the host hears neither
    assert!(nodes[0].tone(0, 550.0) < 0.01 && nodes[0].tone(0, 660.0) < 0.01);
}

/// A mesh invite for a broadcast host's key (the host hosted the same session as a mesh
/// before, with the remembered seed): the host's WELCOME makes the joiner a listener all the
/// same. It hears the host, sends nothing, runs no encoder.
#[test]
fn mesh_invite_to_a_broadcast_host_joins_as_listener() {
    let _serial = serial();
    let (mut h, mut l) = (Node::mono(440.0), Node::mono(550.0));
    let mut o = HostOptions::new("radio again");
    o.mode = SessionMode::Broadcast;
    let link = h.e.host_session(&net("host"), o).unwrap().invite.unwrap();
    let mut inv = sonobus_net::auth::Invite::parse(&link).unwrap();
    assert_eq!(inv.mode, SessionMode::Broadcast);
    inv.mode = SessionMode::Mesh;
    l.e.join_invite(&net("l"), &inv.to_url().unwrap()).unwrap();
    let nodes = &mut [&mut h, &mut l];
    assert!(run_until(nodes, 15.0, |n| n[1]
        .snap()
        .peers
        .first()
        .is_some_and(|p| p.recv_active)));
    let st = nodes[1].e.session_status().unwrap();
    assert_eq!(
        (st.mode, st.role),
        (SessionMode::Broadcast, SessionRole::Listener)
    );
    assert!(hears(nodes, 1, 0, 440.0, CENTRE));
    assert_eq!(nodes[1].e.modern_encode_count(), 0);
    assert!(nodes[0].tone(0, 550.0) < 0.01);
}

/// The lossy shim (`set_modern_receive_loss`) drops AUDIO datagrams before the receive stream.
/// 10 %: NACK resends repair it (REPORTs show no residual loss, the adaptation stays put).
/// 50 %: NACK resends are budgeted (a quarter of the stream's rate), so much stays lost; the
/// REPORTs show it, the sender's adaptation adds redundancy step by step (random loss: flat
/// RTT), and with two redundant blocks per datagram plus the budgeted NACKs most comes back.
#[test]
fn induced_loss_is_repaired_and_adaptation_reacts() {
    let _serial = serial();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    mesh(&mut [&mut a, &mut b], "lossy");
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    let level = |n: &Node| n.snap().peers[0].net.as_ref().unwrap().send_level;
    let stats = |n: &Node| {
        let p = &n.snap().peers[0];
        (p.packets_resent, p.packets_dropped)
    };
    assert_eq!(level(&a), Some((0, 0)));
    b.e.set_modern_receive_loss(10);
    let (r0, d0) = stats(&b);
    run(&mut [&mut a, &mut b], 3.0);
    let (r1, d1) = stats(&b);
    eprintln!(
        "10 % loss, 3 s: {} repaired, {} concealed",
        r1 - r0,
        d1 - d0
    );
    // (shared CI runners do not always keep the test's audio loop at real-time pace: 45
    // repaired in "3 s" on macOS, 48 on Windows; what counts is that few are concealed)
    assert!(r1 - r0 > 25 && d1 - d0 < 10, "NACK repairs it");
    // (not at this very moment: one concealed block inside the analysis window is enough to
    // miss the level, and a few are allowed above)
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    assert_eq!(level(&a), Some((0, 0)), "nothing left for the adaptation");

    b.e.set_modern_receive_loss(50);
    let ok = eventually(&mut [&mut a, &mut b], 20.0, |n| {
        level(n[0]).is_some_and(|(_, k)| k >= 2)
    });
    assert!(ok, "level {:?}, {:?}", level(&a), stats(&b));
    let (r2, d2) = stats(&b);
    run(&mut [&mut a, &mut b], 2.0);
    let (r3, d3) = stats(&b);
    eprintln!(
        "50 % loss at {:?}: 2 s: {} repaired, {} concealed",
        level(&a),
        r3 - r2,
        d3 - d2
    );
    // random loss (flat RTT) adds redundancy, it does not lower the quality level
    assert_eq!(level(&a).map(|l| l.0), Some(0));
    // most of what is lost comes back (two thirds of 400 blocks are hit in 2 s)
    assert!(d3 - d2 < 40, "{}", d3 - d2);
    b.e.set_modern_receive_loss(0);
}

#[test]
fn key_rotation_evicts_a_member_and_outsiders_get_nothing() {
    let _serial = serial();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(880.0));
    mesh(&mut [&mut a, &mut b, &mut c], "evict");
    let old = a.e.invite_link().unwrap();
    // an outsider with a forged invite (right host, wrong seed) is refused and hears nothing
    let mut forged = sonobus_net::auth::Invite::parse(&old).unwrap();
    forged.seed[0] ^= 1;
    let mut d = Node::mono(1200.0);
    d.e.join_invite(&net("outsider"), &forged.to_url().unwrap())
        .unwrap();
    assert!(wait_event(&mut [&mut a, &mut b, &mut c, &mut d], 3, 8.0, auth_failed).is_some());
    assert!(d.snap().peers.is_empty() && a.snap().peers.len() == 2);
    assert!(d.tone(0, 440.0) < 0.01);

    let pc = a.peer("user2");
    let new = a.e.rotate_session_key(Some(pc)).expect("new link");
    assert_ne!(new, old);
    assert!(run_until(&mut [&mut a, &mut b, &mut c], 5.0, |n| n[0]
        .snap()
        .peers
        .len()
        == 1
        && n[1].snap().peers.len() == 1
        && n[2].snap().peers.is_empty()));
    // c cannot come back with the old link
    c.e.join_invite(&net("user2"), &old).unwrap();
    assert!(wait_event(&mut [&mut a, &mut b, &mut c], 2, 8.0, auth_failed).is_some());
    run(&mut [&mut a, &mut b, &mut c], 1.5);
    assert!(a.snap().peers.len() == 1 && b.snap().peers.len() == 1);
    // the session goes on, and a newcomer with the new link joins everyone left
    d.e.join_invite(&net("user3"), &new).unwrap();
    assert!(run_until(&mut [&mut a, &mut b, &mut d], 15.0, |n| {
        all_receiving(n, 2)
    }));
    assert!(hears(&mut [&mut a, &mut b, &mut d], 2, 0, 660.0, CENTRE));
    // removing someone who is no longer a peer under that id changes nothing: it must not
    // become a plain key change that the "removed" device would receive too
    assert!(a.e.rotate_session_key(Some(pc)).is_none());
    assert_eq!(a.e.invite_link().as_deref(), Some(new.as_str()));
}

fn notes(n: &Node) -> Vec<Notification> {
    std::iter::from_fn(|| n.e.poll_notification()).collect()
}

fn wait_note(
    nodes: &mut [&mut Node],
    who: usize,
    secs: f64,
    f: impl Fn(&Notification) -> bool,
) -> Option<Notification> {
    let start = std::time::Instant::now();
    while start.elapsed().as_secs_f64() < secs {
        run(nodes, 0.1);
        if let Some(x) = notes(nodes[who]).into_iter().find(|x| f(x)) {
            return Some(x);
        }
    }
    None
}

/// Recording (all kinds), file playback and the soundboard sent to the peer, chat with the
/// sender taken from the connection, latency match, receive sync and suggestions (which never
/// join anything by themselves), all in a modern session: the same engine paths as legacy.
#[test]
fn stage2_features_in_a_modern_session() {
    let _serial = serial();
    let dir = tmp("modern-stage2");
    let file = sine_wav(&dir, "tone.wav", 1000.0, 3.0);
    let sample = sine_wav(&dir, "s.wav", 1500.0, 1.0);
    let mut st = state(groups(&[(0, 1)]));
    st.extra.default_recording_options = 15;
    st.extra.default_recording_format = 3; // WAV
    st.params.send_file_audio = true;
    st.params.send_soundboard_audio = true;
    st.params.def_net_auto = 0;
    let mut a = Node::new(&[440.0], 2, st);
    let mut stb = state(groups(&[(0, 1)]));
    stb.params.def_net_auto = 0;
    let mut b = Node::new(&[660.0], 2, stb);
    mesh(&mut [&mut a, &mut b], "stage2");
    assert!(hears(&mut [&mut a, &mut b], 0, 0, 660.0, CENTRE));

    let folder = a.e.start_recording(&dir, "take").unwrap();
    // chat: the receiver names the sender by its connection
    a.e.send_chat(&[], "hello");
    assert!(run_until(&mut [&mut a, &mut b], 3.0, |n| n[1].has(
        |e| matches!(
            e,
            Event::Chat(m) if m.from == "user0" && m.message == "hello" && !m.own
        )
    )));
    assert_eq!(b.e.chat_history().len(), 1);
    // file playback and soundboard reach the peer through the shared stream
    a.e.load_file(&file).unwrap();
    a.e.set_file_looping(true);
    a.e.set_file_playing(true);
    assert!(eventually(&mut [&mut a, &mut b], 5.0, |n| n[1]
        .tone(0, 1000.0)
        > 0.2));
    a.e.set_soundboards(Soundboards {
        boards: vec![Soundboard {
            name: "fx".into(),
            samples: vec![SoundSample {
                name: "tone".into(),
                file_path: Some(sample.to_string_lossy().into_owned()),
                end_playback_behaviour: 1,
                ..SoundSample::default()
            }],
        }],
        ..Soundboards::default()
    });
    a.e.soundboard_press((0, 0)).unwrap();
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| n[1]
        .tone(0, 1500.0)
        > 0.1));
    assert!(b.snap().peers[0].remote_recording, "rec flag in /sb/pinfo");
    a.e.stop_all_samples();
    a.e.set_file_playing(false);
    a.e.close_file();
    run(&mut [&mut a, &mut b], 1.0);
    let files = a.e.stop_recording().unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for want in [
        "take-MIXMINUS.wav",
        "take-SELF.wav",
        "take-MIX.wav",
        "take-user1.wav",
    ] {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
    let peer = read_wav(&folder.join("take-user1.wav"));
    assert!(near(tone_at(&peer[0], 660.0, 0), AMP, 0.1));
    let side = std::fs::read_to_string(folder.join("take-latency.txt")).unwrap();
    assert!(
        side.lines().next().unwrap().starts_with("start\tuser1\t"),
        "{side}"
    );
    run(&mut [&mut a, &mut b], 0.5);
    assert!(!b.snap().peers[0].remote_recording);

    // receive sync and latency match
    run(&mut [&mut a, &mut b], 2.5); // RTT samples for the estimate
    let buf = |n: &Node| n.snap().peers[0].buffer_ms;
    assert!((buf(&a) - 40.0).abs() < 0.01);
    a.e.receive_sync(10.0);
    assert!((buf(&a) - 50.0).abs() < 0.01, "{}", buf(&a));
    a.e.receive_sync(10.0);
    assert!((buf(&a) - 50.0).abs() < 0.01, "idempotent");
    let target = a.snap().peers[0].latency.incoming_ms + 20.0;
    a.e.commit_latency_match(target);
    assert!((buf(&a) - 70.0).abs() < 0.05, "{}", buf(&a));
    let p = &a.snap().peers[0];
    assert!(
        p.latency.ping_ms > 0.0 && !p.latency.legacy,
        "{:?}",
        p.latency
    );
    a.e.begin_latency_match();
    assert!(
        wait_note(&mut [&mut a, &mut b], 0, 3.0, |n| *n
            == Notification::LatencyMatchReady)
        .is_some()
    );
    let (entries, ready, _) = a.e.latency_match_info();
    assert!(ready && entries.len() == 2, "{entries:?}");
    a.e.send_latency_match(77.0);
    let want = Notification::LatencyMatchRequested {
        from: "user0".into(),
        latency_ms: 77.0,
    };
    assert!(wait_note(&mut [&mut a, &mut b], 1, 3.0, |n| *n == want).is_some());
    a.e.suggest_group("elsewhere", "pw2", true, &["user1"]);
    assert!(
        wait_note(&mut [&mut a, &mut b], 1, 3.0, |n| matches!(
            n,
            Notification::GroupSuggested(g) if g.user == "user0" && g.group == "elsewhere"
        ))
        .is_some()
    );
    // a suggestion is only a notification: b is still in the same session
    run(&mut [&mut a, &mut b], 0.5);
    assert_eq!(b.snap().group.as_deref(), Some("stage2"));
    assert_eq!(b.snap().peers.len(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Patchbay on a: user1's audio also goes to user2 (through a private stream for user2).
/// Then the peer cache: user1 comes back under another name with the same identity and gets
/// its settings back (remembered by endpoint id).
#[test]
fn patchbay_and_peer_cache_in_a_modern_session() {
    let _serial = serial();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(880.0));
    mesh(&mut [&mut a, &mut b, &mut c], "patch");
    let b_on_c = c.peer("user1");
    c.e.set_peer_recv_muted(b_on_c, true);
    assert!(eventually(&mut [&mut a, &mut b, &mut c], 4.0, |n| n[2]
        .tone(0, 660.0)
        < 0.01));
    let (pb, pc) = (a.peer("user1"), a.peer("user2"));
    let e0 = a.e.modern_encode_count();
    a.e.set_patch(pb, pc, true);
    assert!(a.e.patch(pb, pc));
    let to_c = |n: &Node| {
        n.snap()
            .peers
            .iter()
            .find(|p| p.user == "user2")
            .unwrap()
            .send_channels
    };
    assert_eq!(to_c(&a), 2);
    assert!(
        eventually(&mut [&mut a, &mut b, &mut c], 5.0, |n| n[2].tone(0, 660.0)
            > 0.1),
        "c hears b through a: {}",
        c.tone(0, 660.0)
    );
    // one more encoder: the private stream (b still gets the shared one)
    let e1 = a.e.modern_encode_count();
    run(&mut [&mut a, &mut b, &mut c], 1.0);
    let per_sec = a.e.modern_encode_count() - e1;
    // (325 on a shared macOS runner that fell behind real time)
    let pace = if cfg!(target_os = "macos") { 250 } else { 450 };
    assert!(per_sec > pace && e1 > e0, "{per_sec}");
    assert!(near(b.tone(0, 440.0), CENTRE, TOL));
    a.e.set_patch(pb, pc, false);
    assert_eq!(to_c(&a), 1);
    assert!(eventually(&mut [&mut a, &mut b, &mut c], 4.0, |n| n[2]
        .tone(0, 660.0)
        < 0.01));
    assert!(hears(&mut [&mut a, &mut b, &mut c], 2, 0, 440.0, CENTRE));

    a.e.set_peer_level(pb, 0.3);
    a.e.set_peer_buffer_ms(pb, 70.0);
    a.e.set_peer_send_format(pb, 8);
    a.e.set_peer_soloed(pb, true);
    run(&mut [&mut a, &mut b, &mut c], 0.3);
    b.e.leave_session();
    assert!(run_until(&mut [&mut a, &mut b, &mut c], 5.0, |n| n[0]
        .snap()
        .peers
        .len()
        == 1));
    let link = a.e.invite_link().unwrap();
    b.e.join_invite(&net("renamed"), &link).unwrap();
    assert!(run_until(&mut [&mut a, &mut b, &mut c], 15.0, |n| n[0]
        .snap()
        .peers
        .len()
        == 2));
    let s = a.snap();
    let p = s
        .peers
        .iter()
        .find(|p| p.user == "renamed")
        .expect("rejoined");
    assert_eq!(p.send_format, 8);
    assert!(
        (p.level - 0.3).abs() < 1e-6 && (p.buffer_ms - 70.0).abs() < 1e-3,
        "{p:?}"
    );
    // the cache entry is the endpoint's, whatever its name
    run(&mut [&mut a, &mut b, &mut c], 1.2);
    let id = p.net.as_ref().unwrap().endpoint_id;
    let key = format!(
        "sbid:{}",
        id.iter().map(|x| format!("{x:02x}")).collect::<String>()
    );
    assert!(
        a.e.state()
            .peer_cache
            .iter()
            .any(|c| c.name == key && c.send_format == 8)
    );
    assert!(
        a.e.state()
            .peer_cache
            .iter()
            .all(|c| c.name != "renamed" && c.name != "user1")
    );
}

fn aoo_server() -> aoo::server::ServerHandle {
    aoo::server::run("127.0.0.1:0".parse().unwrap()).expect("server")
}

/// Threads and open file descriptors of this process (Linux).
fn threads_and_fds() -> (usize, usize) {
    let count = |d: &str| std::fs::read_dir(d).map_or(0, |r| r.count());
    (count("/proc/self/task"), count("/proc/self/fd"))
}

/// The same two engines go legacy -> modern -> legacy -> modern -> legacy, hearing each other
/// every time; afterwards no thread or socket is left over.
#[test]
fn legacy_and_modern_sessions_alternate_without_leaks() {
    let _serial = serial();
    let srv = aoo_server();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    run(&mut [&mut a, &mut b], 0.2);
    // The first tokio runtime of a process with tokio's `signal` feature on (the workspace build)
    // makes the signal driver's socket pair, which is never closed: not ours to count.
    drop(
        tokio::runtime::Builder::new_current_thread()
            .enable_io()
            .build()
            .unwrap(),
    );
    let before = threads_and_fds();
    for round in 0..2 {
        a.join(srv.local_addr(), "user0", "g", "pw");
        b.join(srv.local_addr(), "user1", "g", "pw");
        assert!(
            run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1)),
            "legacy {round}"
        );
        assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
        assert!(a.snap().peers[0].net.is_none(), "a legacy peer");
        // hosting a modern session ends the legacy one
        let link =
            a.e.host_session(&net("user0"), HostOptions::new("m"))
                .unwrap()
                .invite
                .unwrap();
        assert!(a.snap().public_groups.is_empty() && a.snap().group.as_deref() == Some("m"));
        b.e.join_invite(&net("user1"), &link).unwrap();
        assert!(
            run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1)
                && n.iter().all(|x| x.snap().peers[0].net.is_some())),
            "modern {round}"
        );
        assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
        assert!(hears(&mut [&mut a, &mut b], 0, 0, 660.0, CENTRE));
        // connecting to a server ends the modern session
        a.e.leave_session();
        b.e.disconnect();
        assert!(
            b.e.session_status().is_some(),
            "disconnect is the legacy session's"
        );
        b.e.leave_session();
        assert!(run_until(&mut [&mut a, &mut b], 5.0, |n| n
            .iter()
            .all(|x| x.snap().peers.is_empty())));
    }
    a.join(srv.local_addr(), "user0", "g", "pw");
    b.join(srv.local_addr(), "user1", "g", "pw");
    assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(
        n, 1
    )));
    a.e.disconnect();
    b.e.disconnect();
    // the server closes its side of the two connections on its own threads: give it time
    run_until(&mut [&mut a, &mut b], 5.0, |_| threads_and_fds() == before);
    let after = threads_and_fds();
    let open: Vec<_> = std::fs::read_dir("/proc/self/fd")
        .into_iter()
        .flatten()
        .filter_map(|e| std::fs::read_link(e.ok()?.path()).ok())
        .collect();
    eprintln!("threads, fds before {before:?} after {after:?}; open now: {open:?}");
    #[cfg(target_os = "linux")] // counted from /proc: zero elsewhere
    assert_eq!(after, before, "threads and fds");
}

/// Samples from a click entering `a` until it leaves `b`, same block size (256) and fixed
/// 40 ms jitter buffers; both engines run in lockstep at real-time pace.
fn click_latency(a: &mut Node, b: &mut Node) -> Option<usize> {
    let block = std::time::Duration::from_secs_f64(BLOCK as f64 / f64::from(SR));
    let (zero, mut click) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    click[0] = 1.0;
    let mut out = [vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]];
    let mut next = std::time::Instant::now();
    let k0 = 50;
    for k in 0..(2 * SR as usize / BLOCK) {
        let input = if k == k0 { &click } else { &zero };
        let [l, r] = &mut out;
        a.a.process(&[&input[..]], &mut [&mut l[..], &mut r[..]]);
        b.a.process(&[&zero[..]], &mut [&mut l[..], &mut r[..]]);
        if k >= k0
            && let Some(i) = out[0].iter().position(|x| x.abs() > 0.05)
        {
            return Some((k - k0) * BLOCK + i);
        }
        next += block;
        sleep_until(next);
    }
    None
}

fn quiet_pair(fmt: i32) -> (Node, Node) {
    let mk = || {
        let mut st = state(groups(&[(0, 1)]));
        st.params.def_net_auto = 0; // fixed 40 ms buffers
        st.params.def_send_qual = fmt;
        Node::new(&[0.0], 2, st)
    };
    (mk(), mk())
}

/// End-to-end latency engine to engine, legacy (AOO over UDP) vs modern (v2 over QUIC
/// datagrams), PCM 16 and Opus 96 kbps/ch, block 256 at 48 kHz, 40 ms manual jitter buffers.
#[test]
fn end_to_end_latency_legacy_vs_modern() {
    let _serial = serial();
    let srv = aoo_server();
    for (fmt, name) in [(8, "PCM 16"), (4, "Opus 96")] {
        let (mut a, mut b) = quiet_pair(fmt);
        a.join(srv.local_addr(), "user0", &format!("lat{fmt}"), "pw");
        b.join(srv.local_addr(), "user1", &format!("lat{fmt}"), "pw");
        assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(
            n, 1
        )));
        run(&mut [&mut a, &mut b], 1.0);
        let legacy = click_latency(&mut a, &mut b).expect("legacy click");
        drop((a, b));
        let (mut a, mut b) = quiet_pair(fmt);
        mesh(&mut [&mut a, &mut b], &format!("lat{fmt}"));
        run(&mut [&mut a, &mut b], 1.0);
        let modern = click_latency(&mut a, &mut b).expect("modern click");
        let ms = |n: usize| n as f64 * 1000.0 / f64::from(SR);
        println!(
            "end-to-end latency {name}: legacy {legacy} samples ({:.1} ms), modern {modern} \
             samples ({:.1} ms)",
            ms(legacy),
            ms(modern)
        );
        // both: the 40 ms buffer plus a few blocks; modern within 25 ms of legacy. A stream
        // keeps the latency it started with, and that differs from start to start (Linux:
        // legacy 48 to 54 ms, modern 43 to 64 ms; a macOS CI runner started legacy at 26.7 ms,
        // below its buffer, and modern at 58.7 ms), so legacy counts as its buffer at least.
        assert!(ms(legacy) < 120.0 && ms(modern) < 120.0);
        // (compared on Linux only: on a macOS CI runner one modern stream started at 32 ms
        // and the next at 99 ms)
        if cfg!(target_os = "linux") {
            assert!(
                ms(modern) < ms(legacy).max(40.0) + 25.0,
                "{} vs {}",
                ms(modern),
                ms(legacy)
            );
        }
    }
}
