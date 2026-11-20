//! Synchronised playout in a broadcast session, engine to engine over loopback QUIC: one host,
//! two listeners whose audio arrives with different delays (`set_modern_receive_delay`). With
//! sync off each plays a click as soon as its own buffer allows; with sync on both play it at
//! the host's target.
#![cfg(feature = "modern")]

mod common;

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use common::*;
use sonobus_engine::modern::{HostOptions, NetOptions, RelayChoice, SessionMode, SessionRole};
use sonobus_net::auth::{Invite, key_from_seed, random_seed};
use sonobus_net::wire::{Frame, KIND_PING, Playout, Role};
use sonobus_net::{Bytes, ConnId, Net};

fn net(name: &str) -> NetOptions {
    NetOptions {
        name: name.into(),
        bind_ip: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        relay: RelayChoice::None,
    }
}

/// Silent input, PCM 16 (a clean click), fixed 40 ms jitter buffers.
fn quiet() -> Node {
    let mut st = state(groups(&[(0, 1)]));
    st.params.def_net_auto = 0;
    st.params.def_send_qual = 8;
    Node::new(&[0.0], 2, st)
}

/// A click into the host; samples until each listener plays it (lockstep, real-time pace).
fn clicks(nodes: &mut [&mut Node]) -> Vec<Option<usize>> {
    let block = Duration::from_secs_f64(BLOCK as f64 / f64::from(SR));
    let (zero, mut click) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    click[0] = 1.0;
    let mut out = [vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]];
    let mut got = vec![None; nodes.len() - 1];
    let mut next = Instant::now();
    let k0 = 10;
    for k in 0..(2 * SR as usize / BLOCK) {
        for (j, n) in nodes.iter_mut().enumerate() {
            let input = if j == 0 && k == k0 { &click } else { &zero };
            let [l, r] = &mut out;
            n.a.process(&[&input[..]], &mut [&mut l[..], &mut r[..]]);
            if j > 0
                && k >= k0
                && got[j - 1].is_none()
                && let Some(i) = out[0].iter().position(|x| x.abs() > 0.05)
            {
                got[j - 1] = Some((k - k0) * BLOCK + i);
            }
        }
        if got.iter().all(Option::is_some) {
            break;
        }
        next += block;
        match next.checked_duration_since(Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = Instant::now(),
        }
    }
    got
}

fn ms(n: usize) -> f64 {
    n as f64 * 1000.0 / f64::from(SR)
}

fn error_ms(n: &Node) -> Option<f32> {
    n.snap().peers.first()?.net.as_ref()?.playout_error_ms
}

/// Listener 1's audio arrives 100 ms later than listener 0's. Without sync their clicks are
/// more than 50 ms apart; with a 200 ms target both play it 200 ms after the host took it in
/// (less one block of output latency), within 2 ms of each other. The host's session clock
/// reads 1 s ahead and listener 1's 3 s ahead (as on separate devices): an offset applied with
/// the wrong sign would put listener 1's target 6 s off, which it rejects, and never sync.
/// Off again: back to each one's own buffering; a listener joining then has no stale target,
/// and turning sync on again brings all three together.
#[test]
fn broadcast_listeners_play_a_click_together() {
    let _serial = serial();
    let (mut h, mut l0, mut l1, mut l2) = (quiet(), quiet(), quiet(), quiet());
    let mut o = HostOptions::new("sync");
    o.mode = SessionMode::Broadcast;
    let link = h.e.host_session(&net("host"), o).unwrap().invite.unwrap();
    h.e.set_modern_clock_skew(1000);
    l0.e.join_invite(&net("l0"), &link).unwrap();
    l1.e.join_invite(&net("l1"), &link).unwrap();
    l1.e.set_modern_clock_skew(3000);
    {
        let nodes = &mut [&mut h, &mut l0, &mut l1];
        assert!(
            run_until(nodes, 15.0, |n| n[1..].iter().all(|l| {
                let s = l.snap();
                s.peers.len() == 1 && s.peers[0].recv_active
            })),
            "{}",
            diag(nodes[0])
        );
        nodes[2].e.set_modern_receive_delay(100);
        run(nodes, 1.5);
        assert!(nodes[1..].iter().all(|l| error_ms(l).is_none()), "no sync without PLAYOUT");

        let off = clicks(nodes);
        let (a, b) = (off[0].expect("l0 click"), off[1].expect("l1 click"));
        eprintln!("sync off: {:.1} ms / {:.1} ms", ms(a), ms(b));
        assert!(ms(b) - ms(a) > 50.0, "{} vs {}", ms(a), ms(b));

        nodes[0].e.set_playout_sync(Some(200));
        assert!(synced(nodes, 10.0), "{:?} {:?}", error_ms(nodes[1]), error_ms(nodes[2]));
        run(nodes, 1.0);
        let on = clicks(nodes);
        let (a, b) = (on[0].expect("l0 click"), on[1].expect("l1 click"));
        eprintln!(
            "sync on: {:.2} ms / {:.2} ms, apart {:.2} ms",
            ms(a),
            ms(b),
            ms(a.abs_diff(b))
        );
        assert!(ms(a.abs_diff(b)) <= 2.0, "{} vs {}", ms(a), ms(b));
        // the target: 200 ms after capture, less one block (5.3 ms) of output latency; the
        // host's anchor is common to both, so its error moves both alike
        let want = 200.0 - ms(BLOCK);
        assert!((ms(a) - want).abs() < 10.0 && (ms(b) - want).abs() < 10.0, "{} {}", ms(a), ms(b));

        // off again: back to each listener's own buffering
        nodes[0].e.set_playout_sync(None);
        assert!(eventually(nodes, 5.0, |n| n[1..].iter().all(|l| error_ms(l).is_none())));
        run(nodes, 1.0);
        let back = clicks(nodes);
        let (a, b) = (back[0].expect("l0 click"), back[1].expect("l1 click"));
        eprintln!("sync off again: {:.1} ms / {:.1} ms", ms(a), ms(b));
        assert!(ms(b) - ms(a) > 50.0 && ms(a) < 150.0, "{} vs {}", ms(a), ms(b));
    }
    // a listener joining while sync is off plays unsynced, then everyone syncs again
    l2.e.join_invite(&net("l2"), &link).unwrap();
    let nodes = &mut [&mut h, &mut l0, &mut l1, &mut l2];
    assert!(run_until(nodes, 15.0, |n| n[3].snap().peers.first().is_some_and(|p| p.recv_active)));
    run(nodes, 1.5);
    assert!(nodes[1..].iter().all(|l| error_ms(l).is_none()));
    nodes[0].e.set_playout_sync(Some(200));
    assert!(synced(nodes, 10.0), "{:?}", nodes[1..].iter().map(|l| error_ms(l)).collect::<Vec<_>>());
    run(nodes, 1.0);
    let all: Vec<usize> = clicks(nodes).into_iter().map(|c| c.expect("click")).collect();
    let spread = all.iter().max().unwrap() - all.iter().min().unwrap();
    eprintln!("sync on again: {:?} ms, spread {:.2} ms", all.iter().map(|&c| ms(c)).collect::<Vec<_>>(), ms(spread));
    assert!(ms(spread) <= 2.0);
}

/// Every listener of `nodes[0]` reports its playout within 0.5 ms of the target.
fn synced(nodes: &mut [&mut Node], secs: f64) -> bool {
    eventually(nodes, secs, |n| n[1..].iter().all(|l| error_ms(l).is_some_and(|e| e.abs() < 0.5)))
}

/// A listener that joins a broadcast by session name and password (not by invite: the engine
/// sees it as a member, the hub as a listener) follows the host's playout too; the host turned
/// sync on when it started the session.
#[test]
fn password_joined_listener_syncs() {
    let _serial = serial();
    let (mut h, mut l) = (quiet(), quiet());
    let mut o = HostOptions::new("named radio");
    o.mode = SessionMode::Broadcast;
    o.password = Some("open sesame".into());
    o.rendezvous = true;
    o.playout_ms = Some(150);
    let server = h.e.host_session(&net("host"), o).unwrap().rendezvous.expect("rendezvous");
    l.e.join_by_name(&net("l0"), server, "named radio", "open sesame").unwrap();
    let nodes = &mut [&mut h, &mut l];
    assert!(run_until(nodes, 15.0, |n| n[1].snap().peers.first().is_some_and(|p| p.recv_active)));
    assert_eq!(nodes[1].e.session_status().unwrap().role, SessionRole::Member);
    assert!(synced(nodes, 10.0), "{:?}", error_ms(nodes[1]));
    run(nodes, 1.0);
    let c = clicks(nodes)[0].expect("click");
    eprintln!("password join: click after {:.2} ms", ms(c));
    assert!((ms(c) - (150.0 - ms(BLOCK))).abs() < 10.0, "{}", ms(c));
}

/// A mesh of three: a bare protocol v2 host (that sends PLAYOUT, which an engine never does in
/// a mesh) and two engine members, one joined by invite, one by session name and password (its
/// key came from that host). Neither acts on the PLAYOUT: neither pings the host, neither
/// reports a playout error.
#[test]
fn mesh_members_ignore_playout() {
    let _serial = serial();
    let seed = random_seed();
    let cfg = sonobus_net::Config {
        bind_ip: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        session: "jam".into(),
        key: Some(key_from_seed(&seed)),
        password: Some("open sesame".into()),
        name: "raw host".into(),
        role: Role::Host,
        mode: SessionMode::Mesh,
        relay: RelayChoice::None,
        ..sonobus_net::Config::default()
    };
    let (tx, inbox) = std::sync::mpsc::channel::<(ConnId, Bytes)>();
    let host = Net::start(cfg, move |conn, _| {
        let tx = tx.clone();
        Box::new(move |d| {
            let _ = tx.send((conn, d));
        }) as sonobus_net::Sink
    })
    .unwrap();
    host.serve_rendezvous(sonobus_net::rdv::Limits::default());
    let me = host.local_addr();
    let link = Invite {
        seed,
        host: me.id,
        addrs: me.addrs.clone(),
        relay: None,
        session: "jam".into(),
        mode: SessionMode::Mesh,
    }
    .to_url()
    .unwrap();
    let (mut a, mut b) = (quiet(), quiet());
    a.e.join_invite(&net("a"), &link).unwrap();
    b.e.join_by_name(&net("b"), me.clone(), "jam", "open sesame").unwrap();
    let nodes = &mut [&mut a, &mut b];
    let mut conns = Vec::new();
    let pump = |conns: &mut Vec<ConnId>| {
        while let Ok(e) = host.events().try_recv() {
            if let sonobus_net::Event::Connected { conn, .. } = e {
                conns.push(conn);
            }
        }
    };
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < 15.0
        && !(conns.len() == 2 && nodes.iter().all(|n| n.snap().peers.len() == 2))
    {
        run(nodes, 0.1);
        pump(&mut conns);
    }
    assert!(nodes.iter().all(|n| n.snap().peers.len() == 2), "a three-node mesh");
    assert_eq!(nodes[1].e.session_status().unwrap().role, SessionRole::Member);
    while inbox.try_recv().is_ok() {}
    let mut pings = 0;
    for _ in 0..12 {
        for &c in &conns {
            let pl = Playout {
                stream: 0,
                generation: 0,
                seq: 0,
                time_us: 0,
                delay_ms: 150,
            };
            let _ = host.send_control(c, Frame::Playout(pl));
        }
        run(nodes, 0.25);
        pump(&mut conns);
        pings += inbox.try_iter().filter(|(_, d)| d.first() == Some(&KIND_PING)).count();
    }
    eprintln!("mesh: {pings} PINGs to the host after 3 s of PLAYOUT");
    assert_eq!(pings, 0);
    assert!(nodes.iter().all(|n| n.snap().peers.iter().all(|p| p.net.as_ref().unwrap().playout_error_ms.is_none())));
}
