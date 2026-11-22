//! Engines talking through a local `aoo::server` (or directly), audio checked end to end.
//! Everything runs on 127.0.0.1. Positive checks wait (`eventually`) so a scheduler hiccup on
//! a loaded machine only delays them; absence checks are immediate.

mod common;

use common::*;
use sonobus_dsp::gain::DEFAULT_CENTER_PAN_LAW as LAW;
use sonobus_engine::{Connection, Event};

/// A mono sine sent mono arrives centre-panned: `AMP * law` on both outputs.
const CENTRE: f32 = AMP * LAW;
const TOL: f32 = 0.15;

fn server() -> aoo::server::ServerHandle {
    aoo::server::run("127.0.0.1:0".parse().unwrap()).expect("server")
}

fn connect_all(srv: std::net::SocketAddr, nodes: &mut [&mut Node], group: &str) {
    for (i, n) in nodes.iter().enumerate() {
        n.join(srv, &format!("user{i}"), group, "pw");
    }
    let count = nodes.len() - 1;
    assert!(
        run_until(nodes, 15.0, |n| all_receiving(n, count)),
        "peers did not connect"
    );
}

/// `node` hears `freq` on `ch` at about `amp` within a few seconds.
fn hears(nodes: &mut [&mut Node], node: usize, ch: usize, freq: f64, amp: f32) -> bool {
    let ok = eventually(nodes, 4.0, |n| near(n[node].tone(ch, freq), amp, TOL));
    if !ok {
        eprintln!(
            "node {node} ch {ch}: {freq} Hz at {} (want {amp}) {}",
            nodes[node].tone(ch, freq),
            diag(nodes[node])
        );
    }
    ok
}

#[test]
fn two_engines_opus_mono_then_leave() {
    let _serial = serial();
    let srv = server();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "g1");
    for ch in 0..2 {
        assert!(
            hears(&mut [&mut a, &mut b], 1, ch, 440.0, CENTRE),
            "b hears a"
        );
        assert!(
            hears(&mut [&mut a, &mut b], 0, ch, 660.0, CENTRE),
            "a hears b"
        );
        assert!(b.tone(ch, 660.0) < 0.01, "no own monitoring at dry 0");
    }
    let s = b.snap();
    assert_eq!(s.connection, Connection::Connected);
    assert_eq!(s.group.as_deref(), Some("g1"));
    let p = &s.peers[0];
    assert_eq!(
        (p.user.as_str(), p.recv_channels, p.send_channels),
        ("user0", 1, 1)
    );
    assert_eq!(p.recv_format.as_deref(), Some("96 kbps/ch"));
    assert!(p.packets_received > 100 && p.recv_bps > 50_000.0, "{p:?}");
    assert!(
        p.latency.ping_ms > 0.0 && !p.latency.legacy,
        "{:?}",
        p.latency
    );
    assert!(p.meter.peak[0] > 0.3);

    // leaving the group removes the peer on both sides
    b.e.leave_group();
    assert!(run_until(&mut [&mut a, &mut b], 5.0, |n| n[0]
        .snap()
        .peers
        .is_empty()
        && n[1].snap().peers.is_empty()));
    run(&mut [&mut a, &mut b], 0.05); // collect the last events
    assert!(a.has(|e| matches!(e, Event::PeerLeft { user, .. } if user == "user1")));
    assert!(b.has(|e| matches!(e, Event::GroupLeft { .. })));
}

#[test]
fn wrong_password_fails_with_the_server_message() {
    let _serial = serial();
    let srv = server();
    let (mut a, mut c) = (Node::mono(440.0), Node::mono(550.0));
    a.join(srv.local_addr(), "alice", "g2", "secret");
    assert!(run_until(&mut [&mut a], 5.0, |n| n[0]
        .has(|e| matches!(e, Event::GroupJoined { .. }))));
    c.join(srv.local_addr(), "carol", "g2", "wrong");
    assert!(run_until(&mut [&mut a, &mut c], 5.0, |n| n[1]
        .has(|e| matches!(e, Event::GroupJoinFailed { .. }))));
    assert!(c.has(|e| *e
        == Event::GroupJoinFailed {
            group: "g2".into(),
            error: "wrong password".into()
        }));
    assert!(a.snap().peers.is_empty());
}

#[test]
fn pcm_stereo_send() {
    let _serial = serial();
    let srv = server();
    let mut st = state(groups(&[(0, 2)]));
    st.params.send_channels = 2;
    st.params.def_send_qual = 8; // PCM 16 bit
    let mut a = Node::new(&[300.0, 500.0], 2, st);
    let mut b = Node::mono(700.0);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "g3");
    // stereo group panned -1/+1 sent stereo, received as a stereo group: channel for channel
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| near(
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
    assert_eq!(a.snap().peers[0].send_channels, 2);
}

#[test]
fn match_inputs_four_channels_follow_the_sender_layout() {
    let _serial = serial();
    let srv = server();
    let mut st = state(groups(&[(0, 2), (2, 1), (3, 1)]));
    st.params.send_channels = 0; // match inputs
    st.input_groups[1].pans[0] = -1.0;
    st.input_groups[2].pans[0] = 1.0;
    let mut a = Node::new(&[300.0, 500.0, 700.0, 900.0], 2, st);
    let mut b = Node::mono(1100.0);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "g4");
    // the stereo group lands left/right, the mono ones where the sender panned them
    let placed = |n: &[&mut Node]| {
        let b = &n[1];
        [(0, 300.0), (1, 500.0), (0, 700.0), (1, 900.0)]
            .iter()
            .all(|&(ch, f)| near(b.tone(ch, f), AMP, TOL) && b.tone(1 - ch, f) < 0.01)
    };
    assert!(
        eventually(&mut [&mut a, &mut b], 4.0, placed),
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
    assert_eq!(
        layout,
        [(0, 2), (2, 1), (3, 1)],
        "receiver adopted the layout"
    );
}

/// "Match inputs": pan, level and name edits of the sending group (every 4 blocks for 3 s)
/// must not restart the stream; the receiver hears it without gaps.
#[test]
fn send_layout_edits_do_not_interrupt_the_stream() {
    let _serial = serial();
    let srv = server();
    let mut st = state(groups(&[(0, 1)]));
    st.params.send_channels = 0;
    let mut a = Node::new(&[440.0], 2, st);
    let mut b = Node::mono(660.0);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "g9");
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    let from = b.block_rms.len();
    let k = std::cell::Cell::new(0usize);
    run_until(&mut [&mut a, &mut b], 3.0, |n| {
        let j = k.get();
        k.set(j + 1);
        if j.is_multiple_of(4) {
            let j = j / 4;
            n[0].e.update_state(|s| {
                let g = &mut s.input_groups[0];
                g.pans[0] = if j.is_multiple_of(2) { 0.3 } else { -0.3 };
                g.gain = if j.is_multiple_of(3) { 0.9 } else { 1.0 };
                g.name = format!("mic {}", j % 5);
            });
        }
        false
    });
    // a centre-panned sine of amplitude CENTRE has an RMS of about 0.21
    let rms = &b.block_rms[from..];
    let gaps = rms.iter().filter(|&&r| r < 0.05).count();
    assert!(
        gaps * 100 <= rms.len(),
        "{gaps} of {} blocks near silent",
        rms.len()
    );
    // the name edits did reach the receiver (as `/sb/clayinfo`)
    assert!(eventually(&mut [&mut a, &mut b], 2.0, |n| n[1]
        .snap()
        .peers[0]
        .groups
        .first()
        .is_some_and(|g| g.name.starts_with("mic"))));
}

#[test]
fn three_engines_level_pan_solo_and_mutes() {
    let _serial = serial();
    let srv = server();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(990.0));
    connect_all(srv.local_addr(), &mut [&mut a, &mut b, &mut c], "g5");
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

    // level 0.5 is applied twice upstream: a quarter of the amplitude
    b.e.set_peer_level(pa, 0.5);
    assert!(hears(
        &mut [&mut a, &mut b, &mut c],
        1,
        0,
        440.0,
        CENTRE * 0.25
    ));
    b.e.set_peer_level(pa, 1.0);

    // pan a hard left
    b.e.update_peer_groups(pa, |g, _| g[0].pans[0] = -1.0);
    assert!(hears(&mut [&mut a, &mut b, &mut c], 1, 0, 440.0, AMP));
    assert!(b.tone(1, 440.0) < 0.01);

    // solo c: a disappears
    b.e.set_peer_soloed(pc, true);
    run(&mut [&mut a, &mut b, &mut c], 0.6);
    assert!(b.tone(0, 440.0) < 0.01);
    assert!(hears(&mut [&mut a, &mut b, &mut c], 1, 0, 990.0, CENTRE));
    b.e.set_peer_soloed(pc, false);

    // receive-mute c: its stream is uninvited
    b.e.set_peer_recv_muted(pc, true);
    run(&mut [&mut a, &mut b, &mut c], 1.0);
    assert!(b.tone(0, 990.0) < 0.01);
    assert!(
        !b.snap()
            .peers
            .iter()
            .find(|p| p.id == pc)
            .unwrap()
            .recv_active
    );
    assert!(hears(&mut [&mut a, &mut b, &mut c], 1, 0, 440.0, AMP));
    b.e.set_peer_recv_muted(pc, false);
    assert!(
        hears(&mut [&mut a, &mut b, &mut c], 1, 0, 990.0, CENTRE),
        "receive resumes"
    );

    // a stops sending to b only
    let pb = a.peer("user1");
    a.e.set_peer_send_muted(pb, true);
    run(&mut [&mut a, &mut b, &mut c], 1.0);
    assert!(b.tone(0, 440.0) < 0.01);
    assert!(
        hears(&mut [&mut a, &mut b, &mut c], 2, 0, 440.0, CENTRE),
        "c still hears a"
    );
    let s = a.snap();
    let st = s.peers.iter().find(|p| p.id == pb).unwrap();
    assert!(!st.send_active && !st.send_allow);
    a.e.set_peer_send_muted(pb, false);
    assert!(
        hears(&mut [&mut a, &mut b, &mut c], 1, 0, 440.0, AMP),
        "sending resumes"
    );
}

#[test]
fn master_mutes_and_format_change() {
    let _serial = serial();
    let srv = server();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "g7");
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
    // PR #201: both advertise every format of the table (and so may pick any)
    let all: Vec<usize> = (0..sonobus_engine::formats::FORMATS.len()).collect();
    for n in [&a, &b] {
        assert_eq!(n.snap().peers[0].supported_formats, all);
    }
    // a format change for one peer is followed by the receiver
    let pb = a.peer("user1");
    a.e.set_peer_send_format(pb, 10);
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| n[1]
        .snap()
        .peers[0]
        .recv_format
        .as_deref()
        == Some("PCM 32 bit float")));
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    // the receiver asks for another format; the sender switches
    let pa = b.peer("user0");
    b.e.request_peer_send_format(pa, Some(8));
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| n[0]
        .snap()
        .peers[0]
        .send_format
        == 8
        && n[1].snap().peers[0].recv_format.as_deref()
            == Some("PCM 16 bit")));
}

#[test]
fn direct_connect_without_a_server() {
    let _serial = serial();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    a.e.connect_direct(b.addr());
    assert!(
        run_until(&mut [&mut a, &mut b], 10.0, |n| all_receiving(n, 1)),
        "direct connect failed"
    );
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    assert!(hears(&mut [&mut a, &mut b], 0, 0, 660.0, CENTRE));
    assert_eq!(a.snap().connection, Connection::Disconnected);

    // chat both ways
    a.e.send_chat(&[], "hello");
    assert!(run_until(&mut [&mut a, &mut b], 3.0, |n| n[1].has(
        |e| matches!(e, Event::Chat(m) if m.message == "hello" && !m.own)
    )));
    assert_eq!(a.e.chat_history().len(), 1);

    // blocking: the peer is dropped and told; it stops sending to us
    let pb = a.peer("");
    a.e.block_peer(pb);
    assert!(run_until(&mut [&mut a, &mut b], 3.0, |n| n[1].has(
        |e| matches!(e, Event::BlockedByPeer { blocked: true, .. })
    )));
    assert!(a.snap().peers.is_empty());
    assert_eq!(a.e.blocked_addresses(), [b.addr().ip()]);
    assert!(!b.snap().peers[0].send_active);
    run(&mut [&mut a, &mut b], 1.0);
    assert!(
        a.snap().peers.is_empty(),
        "a blocked address cannot reconnect"
    );
}

#[test]
fn server_loss_keeps_audio_and_reconnects() {
    let _serial = serial();
    let srv = server();
    let addr = srv.local_addr();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    connect_all(addr, &mut [&mut a, &mut b], "g6");
    drop(srv);
    assert!(run_until(&mut [&mut a, &mut b], 5.0, |n| n.iter().all(
        |x| x.has(|e| matches!(e, Event::Disconnected { error: Some(_) }))
    )));
    run(&mut [&mut a, &mut b], 1.0);
    assert!(
        hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE),
        "audio is peer to peer"
    );
    assert!(hears(&mut [&mut a, &mut b], 0, 0, 660.0, CENTRE));
    assert_eq!(
        a.snap().peers.len(),
        1,
        "peers kept while the server is gone"
    );

    let _srv = aoo::server::run(addr).expect("server restarts on the same port");
    let rejoined = |n: &[&mut Node]| {
        n.iter().all(|x| {
            let s = x.snap();
            s.connection == Connection::Connected && s.group.as_deref() == Some("g6")
        })
    };
    assert!(
        run_until(&mut [&mut a, &mut b], 40.0, rejoined),
        "no reconnect"
    );
    assert!(a.has(|e| *e == Event::Reconnecting));
    assert!(hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE));
    assert_eq!(a.snap().peers.len(), 1);
    assert_eq!(b.snap().peers.len(), 1);
}

#[test]
fn taken_user_name_is_retried_three_times_then_incremented() {
    let _serial = serial();
    let srv = server();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    a.join(srv.local_addr(), "sam", "g8", "pw");
    assert!(run_until(&mut [&mut a], 5.0, |n| n[0]
        .has(|e| matches!(e, Event::GroupJoined { .. }))));
    b.join(srv.local_addr(), "sam", "g8", "pw");
    let start = std::time::Instant::now();
    assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| n[1]
        .has(|e| matches!(e, Event::GroupJoined { .. }))));
    let retries = b
        .events
        .iter()
        .filter(|e| matches!(e, Event::UserNameInUse { retrying: true, .. }))
        .count();
    assert_eq!(retries, 3, "{:?}", b.events);
    assert!(start.elapsed().as_secs_f64() > 7.0, "2.5 s apart");
    assert!(b.has(|e| *e
        == Event::UserNameInUse {
            retrying: false,
            user: "sam 2".into()
        }));
    assert_eq!(b.snap().user, "sam 2");
    // the original name is what gets remembered
    assert_eq!(b.e.state().recents[0].user_name, "sam");
}

/// Context switches per second of the send threads (Linux `/proc`), idle and with three
/// peers streaming. Run with `--ignored --nocapture` to see the numbers.
#[cfg(target_os = "linux")]
#[test]
#[ignore]
fn send_thread_wakeups() {
    fn switches() -> u64 {
        let mut total = 0;
        for t in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
            let comm = std::fs::read_to_string(t.path().join("comm")).unwrap_or_default();
            if comm.trim() != "sonobus-send" {
                continue;
            }
            let status = std::fs::read_to_string(t.path().join("status")).unwrap_or_default();
            for l in status.lines() {
                if let Some(v) = l.strip_prefix("voluntary_ctxt_switches:") {
                    total += v.trim().parse::<u64>().unwrap();
                }
            }
        }
        total
    }
    let _serial = serial();
    let srv = server();
    let mut nodes = [440.0, 550.0, 660.0, 770.0].map(Node::mono);
    let [a, b, c, d] = &mut nodes;
    run(&mut [a, b, c, d], 0.5);
    let s0 = switches();
    run(&mut [a, b, c, d], 2.0);
    let idle = (switches() - s0) as f64 / 2.0 / 4.0;
    connect_all(srv.local_addr(), &mut [a, b, c, d], "wk");
    run(&mut [a, b, c, d], 1.0);
    let s0 = switches();
    run(&mut [a, b, c, d], 2.0);
    let busy = (switches() - s0) as f64 / 2.0 / 4.0;
    eprintln!("send thread wake-ups per engine per second: idle {idle:.0}, 3 peers each {busy:.0}");
}

/// The primary non-loopback IPv4 address of this machine (the route to a public address; a
/// UDP connect sends nothing), if any.
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

/// A session hosted on this machine (server on all interfaces, the host joining through its
/// LAN address, as the app's "host a session" does) and a second engine on the same machine
/// joining through 127.0.0.1. Each side announces a different address of the same host, so
/// each sees the other's datagrams come from the other alias, depending on which handshake
/// ping arrived first. Audio must flow both ways every time.
#[test]
fn same_host_join_via_loopback_and_lan_address() {
    let _serial = serial();
    let Some(lan) = lan_ip() else {
        eprintln!("no LAN address: skipped");
        return;
    };
    let srv = aoo::server::run("0.0.0.0:0".parse().unwrap()).expect("server");
    let port = srv.local_addr().port();
    let join = |n: &Node, host: &str, user: &str| {
        let g = sonobus_engine::GroupJoin {
            name: "same-host".into(),
            password: "pw".into(),
            public: false,
        };
        n.e.connect(host, port, user, "", Some(g)).expect("connect");
    };
    let rounds = 12;
    for round in 0..rounds {
        let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
        // both join orders: which side's handshake ping lands first decides the aliases
        if round % 2 == 0 {
            join(&a, &lan.to_string(), "host");
            join(&b, "127.0.0.1", "joiner");
        } else {
            join(&b, "127.0.0.1", "joiner");
            join(&a, &lan.to_string(), "host");
        }
        let ok = run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(n, 1));
        eprintln!(
            "round {round}: host sees {:?}, joiner sees {:?}",
            a.snap().peers.first().map(|p| p.addr),
            b.snap().peers.first().map(|p| p.addr)
        );
        assert!(
            ok,
            "round {round}: no streams: a {} b {}",
            diag(&a),
            diag(&b)
        );
        assert!(
            hears(&mut [&mut a, &mut b], 1, 0, 440.0, CENTRE)
                && hears(&mut [&mut a, &mut b], 0, 0, 660.0, CENTRE),
            "round {round}: audio"
        );
    }
}
