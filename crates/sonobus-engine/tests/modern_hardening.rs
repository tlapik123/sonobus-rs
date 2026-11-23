//! Modern sessions against misbehaving or unlucky peers: eviction whatever the join method,
//! notices that must be ignored, removal and blocking that must be told, identity by endpoint
//! id (not by display name), quality requests within limits, NACK and datagram floods, and a
//! bandwidth-capped path. Engines and raw `sonobus_net` clients in one process over loopback,
//! relays disabled (`RelayChoice::None`): nothing leaves the machine.
#![cfg(feature = "modern")]

mod common;

use std::collections::VecDeque;
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::*;
use sonobus_core::sbmsg::SbMessage;
use sonobus_dsp::gain::DEFAULT_CENTER_PAN_LAW as LAW;
use sonobus_engine::modern::{
    EVICT, FORMAT_REQ, HostOptions, MAX_PRIVATE_STREAMS, NetOptions, RelayChoice, SessionEvent,
    SessionMode,
};
use sonobus_engine::{Event, PeerId};
use sonobus_net::auth::Invite;
use sonobus_net::wire::{self, Datagram, Frame, QualityReq, Role, Subscribe};
use sonobus_net::{Bytes, ConnId, Join, Net, PeerAddr, PeerTx};

const CENTRE: f32 = AMP * LAW;
const TOL: f32 = 0.15;

fn net(name: &str) -> NetOptions {
    NetOptions {
        name: name.into(),
        bind_ip: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        relay: RelayChoice::None,
    }
}

fn session_events(n: &Node) -> Vec<SessionEvent> {
    std::iter::from_fn(|| n.e.poll_session_event()).collect()
}

fn wait_event(
    nodes: &mut [&mut Node],
    who: usize,
    secs: f64,
    f: impl Fn(&SessionEvent) -> bool,
) -> Option<SessionEvent> {
    let start = Instant::now();
    while start.elapsed().as_secs_f64() < secs {
        run(nodes, 0.1);
        if let Some(e) = session_events(nodes[who]).into_iter().find(|e| f(e)) {
            return Some(e);
        }
    }
    None
}

fn hears(nodes: &mut [&mut Node], node: usize, freq: f64) -> bool {
    let ok = eventually(nodes, 6.0, |n| near(n[node].tone(0, freq), CENTRE, TOL));
    if !ok {
        eprintln!("node {node}: {freq} Hz at {} {}", nodes[node].tone(0, freq), diag(nodes[node]));
    }
    ok
}

fn id_of(n: &Node) -> [u8; 32] {
    n.e.session_status().unwrap().endpoint_id
}

fn fingerprint(id: &[u8; 32]) -> String {
    id.iter().take(4).map(|b| format!("{b:02x}")).collect()
}

/// A bare protocol v2 endpoint that joins by invite link and does what a test makes it do.
struct Raw {
    net: Net,
    /// Connections: (connection, datagram sender, peer endpoint id).
    conns: Vec<(ConnId, PeerTx, [u8; 32])>,
    inbox: std::sync::mpsc::Receiver<(ConnId, Bytes)>,
    /// Control frames received.
    frames: Vec<(ConnId, Frame)>,
    closed: Vec<String>,
}

impl Raw {
    fn join(link: &str, name: &str, nodes: &mut [&mut Node]) -> Raw {
        let inv = Invite::parse(link).unwrap();
        let cfg = sonobus_net::Config {
            bind_ip: Ipv4Addr::LOCALHOST.into(),
            port: 0,
            session: inv.session.clone(),
            key: Some(inv.key()),
            host: Some(inv.host),
            name: name.into(),
            role: if inv.mode == SessionMode::Broadcast {
                Role::Listener
            } else {
                Role::Peer
            },
            mode: inv.mode,
            relay: RelayChoice::None,
            ..sonobus_net::Config::default()
        };
        let (tx, inbox) = std::sync::mpsc::channel();
        let net = Net::start(cfg, move |conn, _| {
            let tx = tx.clone();
            Box::new(move |d| {
                let _ = tx.send((conn, d));
            }) as sonobus_net::Sink
        })
        .unwrap();
        net.connect(
            PeerAddr {
                id: inv.host,
                addrs: inv.addrs.clone(),
                relay: None,
            },
            Join::Key,
        );
        let mut r = Raw {
            net,
            conns: vec![],
            inbox,
            frames: vec![],
            closed: vec![],
        };
        assert!(r.until(nodes, 10.0, |r| r.to(inv.host).is_some()), "raw {name} joined");
        r
    }

    fn pump(&mut self) {
        while let Ok(e) = self.net.events().try_recv() {
            match e {
                sonobus_net::Event::Connected { conn, peer, tx, .. } => {
                    self.conns.push((conn, tx, peer.id))
                }
                sonobus_net::Event::Control { conn, frame } => self.frames.push((conn, frame)),
                sonobus_net::Event::Closed { conn, reason } => {
                    self.conns.retain(|c| c.0 != conn);
                    self.closed.push(reason);
                }
                _ => {}
            }
        }
        while self.inbox.try_recv().is_ok() {}
    }

    /// Run the nodes until `f` holds for us.
    fn until(&mut self, nodes: &mut [&mut Node], secs: f64, f: impl Fn(&Raw) -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < secs {
            run(nodes, 0.05);
            self.pump();
            if f(self) {
                return true;
            }
        }
        false
    }

    fn to(&self, id: [u8; 32]) -> Option<ConnId> {
        self.conns.iter().find(|c| c.2 == id).map(|c| c.0)
    }

    fn send(&self, id: [u8; 32], f: Frame) {
        self.net.send_control(self.to(id).expect("connected"), f).unwrap();
    }

    fn sb(&self, id: [u8; 32], m: &SbMessage) {
        let mut b = Vec::new();
        m.encode(&mut b).unwrap();
        self.send(id, Frame::AppOsc(b));
    }

    fn evict(&self, to: [u8; 32], victim: [u8; 32]) {
        let mut m = Vec::new();
        aoo::osc::encode(&mut m, EVICT, [aoo::osc::Arg::Blob(&victim)]);
        self.send(to, Frame::AppOsc(m));
    }

    fn chats(&self) -> Vec<String> {
        self.frames
            .iter()
            .filter_map(|(_, f)| match f {
                Frame::AppOsc(b) => match sonobus_core::sbmsg::parse(b) {
                    Ok(Some(SbMessage::Chat { message, .. })) => Some(message.to_owned()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }
}

fn find(n: &Node, user: &str) -> Option<PeerId> {
    n.snap().peers.iter().find(|p| p.user == user).map(|p| p.id)
}

/// Review finding 1: a member that joined by name + password follows the host's eviction
/// (its key authority is the host that handed it the key): it drops the removed member at
/// once, the removed member is told ("removed") and cannot be heard by anyone.
#[test]
fn eviction_reaches_a_member_who_joined_by_name() {
    let _serial = serial();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(880.0));
    let mut o = HostOptions::new("named");
    o.password = Some("pw".into());
    o.rendezvous = true;
    let st = a.e.host_session(&net("host"), o).unwrap();
    b.e.join_by_name(&net("byname"), st.rendezvous.unwrap(), "named", "pw")
        .unwrap();
    assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1)));
    c.e.join_invite(&net("victim"), &a.e.invite_link().unwrap())
        .unwrap();
    let nodes = &mut [&mut a, &mut b, &mut c];
    assert!(run_until(nodes, 15.0, |n| all_receiving(n, 2)), "{}", diag(nodes[1]));
    assert!(hears(nodes, 1, 880.0), "b hears c");
    let pc = nodes[0].peer("victim");
    let cid = id_of(nodes[2]);
    let t0 = Instant::now();
    nodes[0].e.rotate_session_key(Some(pc)).unwrap();
    // well before the key-proof timeout: the notice did it
    let ended = wait_event(nodes, 2, 4.0, |e| {
        matches!(e, SessionEvent::Ended { reason } if reason == "removed")
    });
    assert!(ended.is_some(), "c is told it was removed");
    assert!(
        run_until(nodes, 4.0, |n| n[1].snap().peers.len() == 1 && n[2].snap().peers.is_empty()),
        "{} | {}",
        diag(nodes[1]),
        diag(nodes[2])
    );
    assert!(t0.elapsed() < sonobus_net::rt::KEY_PROOF_TIMEOUT, "{:?}", t0.elapsed());
    run(nodes, 1.0);
    assert!(nodes[1].tone(0, 880.0) < 0.01, "b no longer hears c");
    assert!(nodes[1].e.blocked_endpoints().contains(&cid));
    assert!(near(nodes[1].tone(0, 440.0), CENTRE, TOL), "b still hears the host");
}

/// EVICT counts only from the endpoint whose key rotations we accept: a member sending one
/// (naming another member, or the host) changes nothing.
#[test]
fn evict_notices_from_a_non_authority_are_ignored() {
    let _serial = serial();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    let link = a.e.host_session(&net("host"), HostOptions::new("ev")).unwrap().invite.unwrap();
    b.e.join_invite(&net("b"), &link).unwrap();
    assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1)));
    let (aid, bid) = (id_of(&a), id_of(&b));
    let mut r = Raw::join(&link, "mallory", &mut [&mut a, &mut b]);
    assert!(r.until(&mut [&mut a, &mut b], 10.0, |r| r.to(bid).is_some()));
    assert!(run_until(&mut [&mut a, &mut b], 5.0, |n| n.iter().all(|x| x.snap().peers.len() == 2)));
    r.evict(bid, aid);
    r.evict(aid, bid);
    r.evict(bid, bid);
    run(&mut [&mut a, &mut b], 1.5);
    assert_eq!(b.snap().peers.len(), 2, "b keeps the host and mallory");
    assert_eq!(a.snap().peers.len(), 2, "the host follows nobody's notices");
    assert!(b.e.blocked_endpoints().is_empty() && a.e.blocked_endpoints().is_empty());
    assert!(!session_events(&b).iter().any(|e| matches!(e, SessionEvent::Ended { .. })));
    assert!(hears(&mut [&mut a, &mut b], 1, 440.0));
}

/// Review finding 8: a blocked or removed peer learns it. The block notice queued right
/// before the close is delivered (graceful close), and the session ends for it.
#[test]
fn blocked_and_removed_peers_are_told() {
    let _serial = serial();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(880.0));
    let link = a.e.host_session(&net("host"), HostOptions::new("bl")).unwrap().invite.unwrap();
    b.e.join_invite(&net("b"), &link).unwrap();
    c.e.join_invite(&net("c"), &link).unwrap();
    assert!(run_until(&mut [&mut a, &mut b, &mut c], 15.0, |n| all_receiving(n, 2)));
    let (pb, pc) = (a.peer("b"), a.peer("c"));
    a.e.block_peer(pb);
    let ended = wait_event(&mut [&mut a, &mut b, &mut c], 1, 3.0, |e| {
        matches!(e, SessionEvent::Ended { reason } if reason == "blocked")
    });
    assert!(ended.is_some(), "b: the host blocked it");
    assert!(b.has(|e| matches!(e, Event::BlockedByPeer { user, blocked: true, .. } if user == "host")));
    assert_eq!(a.e.blocked_endpoints(), [id_of(&b)]);
    a.e.unblock_endpoint(id_of(&b));
    assert!(a.e.blocked_endpoints().is_empty());
    // removed (not blocked) by the host
    a.e.remove_peer(pc);
    let ended = wait_event(&mut [&mut a, &mut b, &mut c], 2, 3.0, |e| {
        matches!(e, SessionEvent::Ended { reason } if reason == "removed")
    });
    assert!(ended.is_some(), "c: the host removed it");
}

/// Review finding 6: display names are labels. An endpoint joining with a member's name is
/// shown distinctly, gets none of that member's cached settings, cannot pose as anyone in
/// chat, and chat to the name (or the id) reaches only its owner.
#[test]
fn impersonating_names_never_merge_identities() {
    let _serial = serial();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    let link = a.e.host_session(&net("host"), HostOptions::new("id")).unwrap().invite.unwrap();
    b.e.join_invite(&net("bob"), &link).unwrap();
    assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1)));
    let pb = a.peer("bob");
    a.e.set_peer_level(pb, 0.3);
    a.e.set_peer_soloed(pb, true);
    run(&mut [&mut a, &mut b], 1.2); // cache commit
    let aid = id_of(&a);
    let mut r = Raw::join(&link, "bob", &mut [&mut a, &mut b]);
    let rname = format!("bob #{}", fingerprint(&r.net.local_addr().id));
    assert!(run_until(&mut [&mut a, &mut b], 5.0, |n| find(n[0], &rname).is_some()));
    let s = a.snap();
    let imp = s.peers.iter().find(|p| p.user == rname).unwrap();
    assert!((imp.level - 1.0).abs() < 1e-6 && !imp.soloed, "{imp:?}");
    assert!(s.peers.iter().any(|p| p.user == "bob" && (p.level - 0.3).abs() < 1e-6));
    // the real bob's cache entry is untouched by the impostor's settings
    let pr = imp.id;
    a.e.set_peer_level(pr, 0.9);
    run(&mut [&mut a, &mut b], 1.2);
    let st = a.e.state();
    let bob_key = format!(
        "sbid:{}",
        id_of(&b).iter().map(|x| format!("{x:02x}")).collect::<String>()
    );
    let entry = st.peer_cache.iter().find(|c| c.name == bob_key).expect("bob's entry");
    assert!((entry.level - 0.3).abs() < 1e-6);
    assert!(st.peer_cache.iter().all(|c| c.name != "bob"));
    // chat: the body's name means nothing
    r.sb(aid, &SbMessage::Chat {
        group: "id",
        from: "host",
        targets: "",
        tags: "",
        message: "trust me",
    });
    assert!(run_until(&mut [&mut a, &mut b], 3.0, |n| n[0].has(
        |e| matches!(e, Event::Chat(m) if m.message == "trust me" && m.from == rname)
    )));
    // to "bob" by name: only bob; to the impostor by id: only it
    a.e.try_send_chat_to(&[], "all").unwrap();
    a.e.send_chat(&["bob"], "for bob");
    let m = a.e.try_send_chat_to(&[pr], "for you").unwrap();
    assert_eq!(m.targets, rname);
    assert!(run_until(&mut [&mut a, &mut b], 3.0, |n| n[1].has(
        |e| matches!(e, Event::Chat(m) if m.message == "for bob")
    )));
    assert!(r.until(&mut [&mut a, &mut b], 3.0, |r| r.chats().len() == 2));
    assert_eq!(r.chats(), ["all", "for you"]);
    assert!(!b.has(|e| matches!(e, Event::Chat(m) if m.message == "for you")));
}

/// Review finding 6, broadcast: a listener named like another listener asks the hub for
/// latency info and gets its own entry only (the hub answers by connection).
#[test]
fn hub_latency_answers_by_connection_not_name() {
    let _serial = serial();
    let (mut h, mut l) = (Node::mono(440.0), Node::mono(500.0));
    let mut o = HostOptions::new("radio");
    o.mode = SessionMode::Broadcast;
    let link = h.e.host_session(&net("host"), o).unwrap().invite.unwrap();
    l.e.join_invite(&net("l0"), &link).unwrap();
    assert!(run_until(&mut [&mut h, &mut l], 15.0, |n| n[1].snap().peers.len() == 1
        && n[1].snap().peers[0].recv_active));
    let hid = id_of(&h);
    let mut r = Raw::join(&link, "l0", &mut [&mut h, &mut l]);
    let rname = format!("l0 #{}", fingerprint(&r.net.local_addr().id));
    assert!(run_until(&mut [&mut h, &mut l], 5.0, |n| n[0].snap().peers.len() == 2));
    r.sb(hid, &SbMessage::ReqLatInfo);
    let got = |r: &Raw| {
        r.frames.iter().find_map(|(_, f)| match f {
            Frame::AppOsc(b) => match sonobus_core::sbmsg::parse(b) {
                Ok(Some(SbMessage::LatInfo(e))) => Some(e),
                _ => None,
            },
            _ => None,
        })
    };
    assert!(r.until(&mut [&mut h, &mut l], 3.0, |r| got(r).is_some()));
    let entries = got(&r).unwrap();
    assert_eq!(entries.len(), 1, "{entries:?}");
    assert_eq!((entries[0].src_name.as_str(), entries[0].dest_name.as_str()), (rname.as_str(), "host"));
    // and the hub's own latency match completes with two listeners (2 entries per listener)
    h.e.begin_latency_match();
    run(&mut [&mut h, &mut l], 1.0);
    let (entries, ready, _) = h.e.latency_match_info();
    // the raw listener does not answer: not ready with one of two answers...
    assert!(!ready && entries.len() == 3, "{entries:?}");
    drop(r);
    assert!(run_until(&mut [&mut h, &mut l], 5.0, |n| n[0].snap().peers.len() == 1));
    h.e.begin_latency_match();
    run(&mut [&mut h, &mut l], 1.0);
    let (entries, ready, _) = h.e.latency_match_info();
    assert!(ready && entries.len() == 2, "...ready with every listener's {entries:?}");
}

/// Review finding 3: a broadcast listener can only pick among the hub's shared encodings;
/// a flood of requests creates no encoder.
#[test]
fn listener_quality_requests_map_to_shared_encodings() {
    let _serial = serial();
    let mut h = Node::mono(440.0);
    let mut l = Node::mono(500.0);
    let mut o = HostOptions::new("radio2");
    o.mode = SessionMode::Broadcast;
    let link = h.e.host_session(&net("host"), o).unwrap().invite.unwrap();
    l.e.join_invite(&net("l0"), &link).unwrap();
    assert!(run_until(&mut [&mut h, &mut l], 15.0, |n| n[1].snap().peers.len() == 1));
    let hid = id_of(&h);
    let mut r = Raw::join(&link, "greedy", &mut [&mut h, &mut l]);
    assert!(run_until(&mut [&mut h, &mut l], 5.0, |n| n[0].snap().peers.len() == 2));
    let e0 = h.e.modern_encode_count();
    run(&mut [&mut h, &mut l], 1.0);
    let base = h.e.modern_encode_count() - e0;
    for k in 0..100 {
        let want = if k % 2 == 0 { 10 } else { 9 };
        r.send(hid, Frame::QualityReq(QualityReq {
            max_bitrate: 0,
            preferred_format: FORMAT_REQ | want,
        }));
        if k % 10 == 0 {
            run(&mut [&mut h, &mut l], 0.02);
            r.pump();
        }
    }
    run(&mut [&mut h, &mut l], 1.5);
    let e1 = h.e.modern_encode_count();
    run(&mut [&mut h, &mut l], 1.0);
    let after = h.e.modern_encode_count() - e1;
    eprintln!("hub encoder runs/s: {base} before, {after} after 100 PCM requests");
    assert!(after <= base + base / 10, "{base} -> {after}");
    let s = h.snap();
    let p = s.peers.iter().find(|p| p.user == "greedy").unwrap();
    assert_eq!(p.send_format, 4, "nearest shared encoding to PCM: the main one");
    let streams = r.frames.iter().filter(|(_, f)| matches!(f, Frame::Format(x) if x.stream == 1)).count();
    assert_eq!(streams, 0, "no private stream announced");
    assert!(hears(&mut [&mut h, &mut l], 1, 440.0));
}

/// Review finding 3, mesh: requests honoured within limits: at most one change per second
/// and peer, "Max Send Quality" caps them, at most MAX_PRIVATE_STREAMS peers get their own
/// encoder, and nothing when requests are not honoured. The cap holds for legacy too
/// (`legacy::stock_tests`).
#[test]
fn member_quality_requests_are_capped_and_rate_limited() {
    let _serial = serial();
    let mut a = Node::mono(440.0);
    let link = a.e.host_session(&net("host"), HostOptions::new("q")).unwrap().invite.unwrap();
    let aid = id_of(&a);
    let mut raws: Vec<Raw> = (0..MAX_PRIVATE_STREAMS + 1)
        .map(|i| Raw::join(&link, &format!("r{i}"), &mut [&mut a]))
        .collect();
    assert!(run_until(&mut [&mut a], 10.0, |n| n[0].snap().peers.len() == raws.len()));
    let fmt = |a: &Node, name: &str| a.snap().peers.iter().find(|p| p.user == name).unwrap().send_format;
    let req = |r: &Raw, f: u8| {
        r.send(aid, Frame::QualityReq(QualityReq {
            max_bitrate: 0,
            preferred_format: FORMAT_REQ | f,
        }))
    };
    // rate limit: the second request within a second waits for its turn
    req(&raws[0], 9);
    assert!(run_until(&mut [&mut a], 2.0, |n| fmt(n[0], "r0") == 9));
    let t = Instant::now();
    req(&raws[0], 10);
    assert!(run_until(&mut [&mut a], 3.0, |n| fmt(n[0], "r0") == 10));
    assert!(t.elapsed() >= Duration::from_millis(700), "{:?}", t.elapsed());
    // the cap: PCM 16 at most
    a.e.set_max_send_quality(Some(8));
    assert_eq!(a.e.max_send_quality(), Some(8));
    run(&mut [&mut a], 1.1);
    req(&raws[0], 10);
    req(&raws[1], 10);
    for r in &raws[2..] {
        req(r, 9);
    }
    run(&mut [&mut a], 1.5);
    let fs: Vec<usize> = (0..raws.len()).map(|i| fmt(&a, &format!("r{i}"))).collect();
    eprintln!("formats after requests: {fs:?}");
    assert_eq!(fs[0], 8);
    assert_eq!(fs.iter().filter(|&&f| f == 8).count(), MAX_PRIVATE_STREAMS, "{fs:?}");
    assert_eq!(fs.iter().filter(|&&f| f == 4).count(), 1, "one over the cap gets the shared stream");
    // not honoured: ignored
    a.e.set_honour_quality_requests(false);
    assert!(!a.e.honour_quality_requests());
    run(&mut [&mut a], 1.1);
    req(&raws[0], 2);
    run(&mut [&mut a], 1.5);
    assert_eq!(fmt(&a, "r0"), 8);
    for r in &mut raws {
        r.pump();
    }
    let st = a.e.state();
    assert!(st.extra.unknown.iter().any(|(k, _)| k == "MaxSendQuality"));
}

/// Review finding 4: a subscriber flooding NACKs gets resends within its budget (a quarter of
/// its stream's rate), and the others keep hearing the sender.
#[test]
fn nack_flood_is_budgeted() {
    let _serial = serial();
    let mut st = state(groups(&[(0, 1)]));
    st.params.def_send_qual = 8; // PCM 16: 768 kbit/s, big resends
    let mut a = Node::new(&[440.0], 2, st);
    let mut b = Node::mono(660.0);
    let link = a.e.host_session(&net("host"), HostOptions::new("nack")).unwrap().invite.unwrap();
    b.e.join_invite(&net("b"), &link).unwrap();
    assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1)));
    let aid = id_of(&a);
    let r = Raw::join(&link, "flood", &mut [&mut a, &mut b]);
    // subscribe to the shared stream, then ask for everything, again and again
    r.send(aid, Frame::Subscribe(Subscribe { stream: 0, encoding: 0, redundancy: 0 }));
    let mut high = 0u32;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(1) {
        run(&mut [&mut a, &mut b], 0.05);
        while let Ok((_, d)) = r.inbox.try_recv() {
            if let Ok(Datagram::Audio(x)) = Datagram::decode(&d) {
                high = high.max(x.header.seq);
            }
        }
    }
    assert!(high > 100, "streaming to the flooder: {high}");
    let pr = a.peer("flood");
    let sent = |a: &Node| a.snap().peers.iter().find(|p| p.id == pr).unwrap().send_bps;
    let normal = sent(&a);
    let tx = r.conns.iter().find(|c| c.2 == aid).unwrap().1.clone();
    let entries: Vec<(u32, u8)> = (0..wire::MAX_NACK as u32)
        .map(|k| (high.saturating_sub(k), 255))
        .collect();
    let mut nack = Vec::new();
    wire::encode_nack(0, &entries, &mut nack).unwrap();
    let nack = Bytes::from(nack);
    let start = Instant::now();
    let mut flood_bps = 0.0f64;
    while start.elapsed() < Duration::from_secs(3) {
        for _ in 0..20 {
            let _ = tx.send(nack.clone());
        }
        run(&mut [&mut a, &mut b], 0.01);
        while r.inbox.try_recv().is_ok() {}
        if start.elapsed() > Duration::from_secs(1) {
            flood_bps = flood_bps.max(sent(&a));
        }
    }
    eprintln!("to the flooder: {normal:.0} bit/s streaming, at most {flood_bps:.0} bit/s while flooding");
    // stream + 25 % (+ the burst, the per-second sampling)
    assert!(flood_bps < normal * 1.45, "{normal} -> {flood_bps}");
    assert!(near(b.tone(0, 440.0), CENTRE, TOL), "b still hears a: {}", b.tone(0, 440.0));
    // garbage datagrams cost nothing either
    for _ in 0..5000 {
        let _ = tx.send(Bytes::from_static(&[9, 9, 9, 9]));
    }
    run(&mut [&mut a, &mut b], 1.0);
    eprintln!("dropped on arrival: {}", a.e.modern_dropped_datagrams());
    assert!(hears(&mut [&mut a, &mut b], 1, 440.0));
}

/// The receiver's bit-rate cap holds over our packet-redundancy floor: with redundancy 3 (a
/// floor of two extra blocks) and a cap only the lowest level fits once, no datagram carries
/// redundant blocks.
#[test]
fn receiver_bitrate_cap_beats_the_redundancy_floor() {
    let _serial = serial();
    let mut a = Node::mono(440.0);
    a.e.set_packet_redundancy(3);
    let link = a.e.host_session(&net("host"), HostOptions::new("cap")).unwrap().invite.unwrap();
    let aid = id_of(&a);
    let r = Raw::join(&link, "capped", &mut [&mut a]);
    r.send(aid, Frame::Subscribe(Subscribe { stream: 0, encoding: 0, redundancy: 0 }));
    r.send(aid, Frame::QualityReq(QualityReq { max_bitrate: 1, preferred_format: 0xff }));
    run(&mut [&mut a], 1.0);
    while r.inbox.try_recv().is_ok() {}
    let (mut n, mut most) = (0, 0);
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(1) {
        run(&mut [&mut a], 0.05);
        while let Ok((_, d)) = r.inbox.try_recv() {
            if let Ok(Datagram::Audio(x)) = Datagram::decode(&d) {
                n += 1;
                most = most.max(x.redundant.len());
            }
        }
    }
    assert!(n > 20, "streaming to the capped subscriber: {n}");
    assert_eq!(most, 0, "redundant blocks over the receiver's cap");
    let pr = a.peer("capped");
    let p = a.snap().peers.into_iter().find(|p| p.id == pr).unwrap();
    assert_eq!(p.net.unwrap().send_level.map(|l| l.1), Some(0));
}

/// A UDP relay between two endpoints: everything passes, but the direction towards the
/// joiner can be capped (token bucket, tail-drop queue of `QUEUE` at the cap rate), as a
/// constrained uplink would.
struct Bottleneck {
    addr: SocketAddr,
    /// Cap, bit/s (0: none).
    cap: Arc<AtomicU64>,
    /// Bytes that arrived from the sender, and that were dropped at the queue.
    from_sender: Arc<AtomicU64>,
    dropped: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

const QUEUE: Duration = Duration::from_millis(300);

impl Bottleneck {
    fn new(sender: SocketAddr) -> Self {
        let front = UdpSocket::bind("127.0.0.1:0").unwrap();
        // towards the sender on its own address family (see the test: no way around us)
        let back = UdpSocket::bind(if sender.is_ipv6() { "[::1]:0" } else { "127.0.0.1:0" }).unwrap();
        front.set_nonblocking(true).unwrap();
        back.set_nonblocking(true).unwrap();
        let addr = front.local_addr().unwrap();
        let (cap, from_sender, dropped, stop) = (
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicU64::new(0)),
            Arc::new(AtomicBool::new(false)),
        );
        let (c, f, d, s) = (cap.clone(), from_sender.clone(), dropped.clone(), stop.clone());
        let thread = std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            let mut joiner: Option<SocketAddr> = None;
            let mut q: VecDeque<Vec<u8>> = VecDeque::new();
            let (mut queued, mut tokens, mut last) = (0usize, 0.0f64, Instant::now());
            while !s.load(Ordering::Relaxed) {
                while let Ok((n, from)) = front.recv_from(&mut buf) {
                    joiner = Some(from);
                    let _ = back.send_to(&buf[..n], sender);
                }
                let cap = c.load(Ordering::Relaxed) as f64 / 8.0;
                while let Ok((n, _)) = back.recv_from(&mut buf) {
                    f.fetch_add(n as u64, Ordering::Relaxed);
                    if cap == 0.0 {
                        if let Some(j) = joiner {
                            let _ = front.send_to(&buf[..n], j);
                        }
                    } else if (queued + n) as f64 <= cap * QUEUE.as_secs_f64() {
                        queued += n;
                        q.push_back(buf[..n].to_vec());
                    } else {
                        d.fetch_add(1, Ordering::Relaxed);
                    }
                }
                let now = Instant::now();
                tokens = (tokens + cap * now.duration_since(last).as_secs_f64()).min(3000.0);
                last = now;
                while let Some(p) = q.front() {
                    if (p.len() as f64) > tokens && cap > 0.0 {
                        break;
                    }
                    let p = q.pop_front().unwrap();
                    tokens -= p.len() as f64;
                    queued -= p.len();
                    if let Some(j) = joiner {
                        let _ = front.send_to(&p, j);
                    }
                }
                std::thread::sleep(Duration::from_micros(200));
            }
        });
        Self {
            addr,
            cap,
            from_sender,
            dropped,
            stop,
            thread: Some(thread),
        }
    }

    /// Bytes per second from the sender over `secs` while running the nodes.
    fn rate(&self, nodes: &mut [&mut Node], secs: f64) -> f64 {
        let b0 = self.from_sender.load(Ordering::Relaxed);
        let t0 = Instant::now();
        run(nodes, secs);
        (self.from_sender.load(Ordering::Relaxed) - b0) as f64 / t0.elapsed().as_secs_f64()
    }
}

impl Drop for Bottleneck {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Review finding 2: a sender on a constrained uplink backs off. A streams PCM 16 (about
/// 840 kbit/s on the wire) to B through a 500 kbit/s bottleneck with a 300 ms tail-drop
/// queue: the queue's delay shows in the RTT, A's adaptation finds the shared stream's one
/// level exhausted and moves B to a private Opus stream at most half the rate (with its own
/// bit-rate ladder); the bytes A sends per second drop below the cap and B keeps hearing A.
#[test]
#[cfg_attr(
    not(target_os = "linux"),
    ignore = "on the Windows and macOS CI runners the connection took the direct loopback \
              path around the bottleneck from the start: nothing went through it"
)]
fn congested_uplink_lowers_the_send_rate() {
    let _serial = serial();
    let mut st = state(groups(&[(0, 1)]));
    st.params.def_send_qual = 8;
    let mut a = Node::new(&[440.0], 2, st);
    let mut b = Node::mono(660.0);
    // The host listens on IPv6 loopback only and the joiner on IPv4 only, so the bottleneck is
    // the one path between them. With both on 127.0.0.1 iroh moved the connection to the
    // direct path whenever the relay thread was slow (machine under load).
    let host = NetOptions { bind_ip: std::net::Ipv6Addr::LOCALHOST.into(), ..net("host") };
    let st = a.e.host_session(&host, HostOptions::new("cap")).unwrap();
    let quic = st.addrs.iter().copied().find(SocketAddr::is_ipv6).expect("loopback address");
    let neck = Bottleneck::new(quic);
    let mut inv = Invite::parse(&st.invite.unwrap()).unwrap();
    inv.addrs = vec![neck.addr];
    b.e.join_invite(&net("b"), &inv.to_url().unwrap()).unwrap();
    let nodes = &mut [&mut a, &mut b];
    assert!(run_until(nodes, 15.0, |n| all_receiving(n, 1)), "{}", diag(nodes[1]));
    assert!(hears(nodes, 1, 440.0));
    let free = neck.rate(nodes, 3.0);
    let bps = |n: &Node| n.snap().peers[0].send_bps;
    let free_bps = bps(nodes[0]);
    neck.cap.store(500_000, Ordering::Relaxed);
    let mut trace = vec![];
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(12) {
        trace.push(neck.rate(nodes, 1.0));
    }
    let capped = neck.rate(nodes, 3.0);
    let s = nodes[1].snap();
    eprintln!(
        "bytes/s from the sender: {free:.0} uncapped ({free_bps:.0} bit/s of datagrams); \
         per second after the 500 kbit/s cap: {:?}; last 3 s {capped:.0} ({:.0} bit/s of \
         datagrams); {} dropped at the queue; receiving {:?}",
        trace.iter().map(|x| *x as u64).collect::<Vec<_>>(),
        bps(nodes[0]),
        neck.dropped.load(Ordering::Relaxed),
        s.peers[0].recv_format
    );
    assert!(free > 0.0, "nothing went through the bottleneck: iroh took the direct path");
    assert!(capped < 0.6 * free, "{free} -> {capped}");
    assert!(capped * 8.0 < 500_000.0 * 0.9, "under the cap with room: {capped}");
    assert_ne!(s.peers[0].recv_format.as_deref(), Some("PCM 16 bit"), "{:?}", s.peers[0]);
    assert!(hears(nodes, 1, 440.0), "audio continues at the lower rate");
    assert!(neck.rate(nodes, 2.0) <= capped * 1.2, "and stays down");
}
