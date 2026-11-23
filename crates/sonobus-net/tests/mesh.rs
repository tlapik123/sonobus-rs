//! Mesh formation from one invite, duplicate-connection resolution and session-key rotation.
//! Loopback only, relays disabled.

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use common::*;
use sonobus_net::auth::{self, Id, Key};
use sonobus_net::wire::{Frame, Rekey, Role};
use sonobus_net::{Config, ConnId, Event, Join, Net, PeerInfo};

const K: Key = [0x33; 32];

/// One endpoint and what its events told us so far.
struct Node {
    net: Net,
    live: HashMap<ConnId, PeerInfo>,
    keys: Vec<Key>,
    failed: Vec<String>,
    closed: Vec<String>,
    /// A slow app: stalls this long after each [`Event::SessionKey`].
    key_delay: Duration,
}

impl Node {
    fn new(net: Net) -> Self {
        Node {
            net,
            live: HashMap::new(),
            keys: vec![],
            failed: vec![],
            closed: vec![],
            key_delay: Duration::ZERO,
        }
    }

    fn id(&self) -> Id {
        self.net.local_addr().id
    }

    fn pump(&mut self) {
        while let Ok(e) = self.net.events().try_recv() {
            match e {
                Event::Connected { conn, peer, .. } => {
                    self.live.insert(conn, peer);
                }
                Event::Closed { conn, reason } => {
                    self.live.remove(&conn);
                    self.closed.push(reason);
                }
                Event::ConnectFailed { reason, .. } => self.failed.push(reason),
                Event::SessionKey(k) => {
                    self.keys.push(k);
                    std::thread::sleep(self.key_delay);
                }
                _ => {}
            }
        }
    }

    fn peers(&self) -> Vec<Id> {
        let mut v: Vec<Id> = self.live.values().map(|p| p.id).collect();
        v.sort();
        v
    }
}

/// Pump every node until `done` holds (panics after [`WAIT`]), then for `settle` more.
fn until(nodes: &mut [&mut Node], settle: Duration, done: impl Fn(&[&mut Node]) -> bool) {
    let end = Instant::now() + WAIT;
    loop {
        nodes.iter_mut().for_each(|n| n.pump());
        if done(nodes) {
            break;
        }
        assert!(
            Instant::now() < end,
            "timed out: {:?}",
            nodes
                .iter()
                .map(|n| (n.live.len(), &n.closed, &n.failed))
                .collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let end = Instant::now() + settle;
    while Instant::now() < end {
        nodes.iter_mut().for_each(|n| n.pump());
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn member(name: &str, ip: u8, key: Key, host: Id) -> Node {
    Node::new(simple(Config {
        bind_ip: lo(ip).into(),
        host: Some(host),
        ..cfg(name, Some(key), None)
    }))
}

/// Every node is connected exactly once to every other node.
fn full_mesh(nodes: &[&mut Node]) -> bool {
    nodes.iter().all(|n| {
        let mut want: Vec<Id> = nodes
            .iter()
            .map(|m| m.id())
            .filter(|&i| i != n.id())
            .collect();
        want.sort();
        n.peers() == want
    })
}

#[test]
fn invite_forms_full_mesh_and_survives_host_leaving() {
    let mut h = Node::new(simple(Config {
        role: Role::Host,
        ..cfg("host", Some(K), None)
    }));
    let hid = h.id();
    let invite = h.net.local_addr().clone();
    let mut b = member("b", 2, K, hid);
    let mut c = member("c", 3, K, hid);
    let mut d = member("d", 4, K, hid);
    // Everyone dials only the invite's host; the newcomers dial the roster themselves.
    for n in [&b, &c, &d] {
        n.net.connect(invite.clone(), Join::Key);
        std::thread::sleep(Duration::from_millis(300));
    }
    until(
        &mut [&mut h, &mut b, &mut c, &mut d],
        Duration::from_millis(500),
        full_mesh,
    );
    let conns: usize = [&h, &b, &c, &d].iter().map(|n| n.live.len()).sum();
    assert_eq!(conns / 2, 6, "6 connections, no duplicates");
    for n in [&b, &c, &d] {
        assert!(
            n.failed.is_empty() && n.closed.is_empty(),
            "{:?} {:?}",
            n.failed,
            n.closed
        );
        // Only the real host is marked; names/roles are just labels.
        for p in n.live.values() {
            assert_eq!(p.is_invite_host, p.id == hid, "{p:?}");
        }
        let host_view = n.live.values().find(|p| p.id == hid).unwrap();
        assert_eq!(
            (host_view.name.as_str(), host_view.role),
            ("host", Role::Host)
        );
    }
    // Existing members never dialed newcomers: b's links to c and d are incoming.
    assert!(b.live.values().filter(|p| p.id != hid).all(|p| p.incoming));

    // The host leaves; the others stay connected to each other.
    drop(h);
    until(
        &mut [&mut b, &mut c, &mut d],
        Duration::from_millis(300),
        full_mesh,
    );
    // A fifth joins through any remaining member (an invite naming b).
    let mut e = member("e", 5, K, b.id());
    e.net.connect(b.net.local_addr().clone(), Join::Key);
    until(
        &mut [&mut b, &mut c, &mut d, &mut e],
        Duration::from_millis(300),
        full_mesh,
    );
}

/// Five newcomers join at the same moment, two through the host and three through other
/// newcomers that are themselves still joining (anyone holding the key accepts joins). Their
/// WELCOMEs cannot list each other; the members' ROSTER forwards close the gaps. 20 rounds.
#[test]
fn simultaneous_newcomers_converge_to_full_mesh() {
    let mut h = Node::new(simple(Config {
        role: Role::Host,
        ..cfg("host", Some(K), None)
    }));
    let hid = h.id();
    for round in 0..20 {
        let mut news: Vec<Node> = (0..5)
            .map(|i| member(&format!("n{i}"), 10 + i as u8, K, hid))
            .collect();
        let addr = |n: &Node| n.net.local_addr().clone();
        let via = [
            addr(&h),
            addr(&h),
            addr(&news[0]),
            addr(&news[1]),
            addr(&news[2]),
        ];
        for (n, to) in news.iter().zip(via) {
            n.net.connect(to, Join::Key);
        }
        let [a, b, c, d, e] = &mut news[..] else {
            unreachable!()
        };
        let all = &mut [&mut h, a, b, c, d, e];
        until(all, Duration::from_millis(200), full_mesh);
        // A late duplicate (both sides dialed) may still be resolving: it must settle again.
        until(all, Duration::ZERO, full_mesh);
        let failed: Vec<&String> = news.iter().flat_map(|n| &n.failed).collect();
        assert!(
            failed.iter().all(|r| r.contains("duplicate")),
            "round {round}: {failed:?}"
        );
        drop(news);
        until(&mut [&mut h], Duration::ZERO, |n| n[0].live.is_empty());
    }
}

#[test]
fn simultaneous_dials_leave_one_connection() {
    let mut a = Node::new(simple(cfg("a", Some(K), None)));
    let mut b = member("b", 2, K, [0; 32]);
    a.net.connect(b.net.local_addr().clone(), Join::Key);
    b.net.connect(a.net.local_addr().clone(), Join::Key);
    until(&mut [&mut a, &mut b], Duration::from_millis(800), |n| {
        n.iter().all(|n| n.live.len() == 1)
    });
    assert_eq!((a.live.len(), b.live.len()), (1, 1));
    // The survivor is the connection dialed by the smaller endpoint id, on both sides.
    let a_dialed = a.id() < b.id();
    assert_eq!(a.live.values().next().unwrap().incoming, !a_dialed);
    assert_eq!(b.live.values().next().unwrap().incoming, a_dialed);
    // A third dial of an existing pair is refused as a duplicate (same initiator, keep old).
    a.net.connect(b.net.local_addr().clone(), Join::Key);
    b.net.connect(a.net.local_addr().clone(), Join::Key);
    std::thread::sleep(Duration::from_millis(500));
    until(&mut [&mut a, &mut b], Duration::from_millis(300), |_| true);
    assert_eq!((a.live.len(), b.live.len()), (1, 1));
    let dups = a
        .failed
        .iter()
        .chain(&b.failed)
        .chain(&a.closed)
        .chain(&b.closed);
    assert!(
        dups.clone().all(|r| r.contains("duplicate")),
        "{:?}",
        dups.collect::<Vec<_>>()
    );
}

#[test]
fn rotated_key_locks_out_a_removed_member() {
    let mut h = Node::new(simple(Config {
        role: Role::Host,
        ..cfg("host", Some(K), Some("pw"))
    }));
    let hid = h.id();
    let inv = h.net.local_addr().clone();
    let mut a = member("a", 2, K, hid);
    let mut b = member("b", 3, K, hid);
    let mut r = member("r", 4, K, hid);
    for n in [&a, &b, &r] {
        n.net.connect(inv.clone(), Join::Key);
        std::thread::sleep(Duration::from_millis(300));
    }
    until(
        &mut [&mut h, &mut a, &mut b, &mut r],
        Duration::from_millis(300),
        full_mesh,
    );
    // Remove r: everyone drops it, then the host rotates with no grace period.
    for n in [&h, &a, &b] {
        let (&conn, _) = n.live.iter().find(|(_, p)| p.id == r.id()).unwrap();
        n.net.disconnect(conn);
    }
    until(&mut [&mut h, &mut a, &mut b, &mut r], Duration::ZERO, |n| {
        n[3].live.is_empty()
    });
    let seed = h.net.rotate_session_key(Duration::ZERO);
    let k2 = auth::key_from_seed(&seed);
    until(&mut [&mut h, &mut a, &mut b], Duration::ZERO, |n| {
        n.iter().all(|n| n.keys.last() == Some(&k2))
    });
    // r still holds the old key: refused by the host and by members.
    for to in [&h, &a] {
        r.net.connect(to.net.local_addr().clone(), Join::Key);
        until(&mut [&mut r], Duration::ZERO, |n| !n[0].failed.is_empty());
        assert_eq!(r.failed.pop().unwrap(), "authentication failed");
    }
    // Remaining members reconnect to each other with the new key...
    let (&ab, _) = a.live.iter().find(|(_, p)| p.id == b.id()).unwrap();
    a.net.disconnect(ab);
    until(&mut [&mut a, &mut b], Duration::ZERO, |n| {
        n[0].live.len() == 1 && n[1].live.len() == 1
    });
    a.net.connect(b.net.local_addr().clone(), Join::Key);
    until(&mut [&mut a, &mut b], Duration::ZERO, |n| {
        n[0].live.len() == 2
    });
    // ...and a newcomer with an invite from the new seed joins the whole mesh.
    let mut n = member("n", 5, k2, hid);
    n.net.connect(inv.clone(), Join::Key);
    until(
        &mut [&mut h, &mut a, &mut b, &mut n],
        Duration::from_millis(300),
        full_mesh,
    );
    // A password joiner receives the new key too.
    let mut p = Node::new(simple(Config {
        bind_ip: lo(6).into(),
        ..cfg("p", None, None)
    }));
    p.net.connect(inv, Join::Password("pw".into()));
    until(&mut [&mut p], Duration::ZERO, |n| {
        n[0].keys.last() == Some(&k2)
    });
    assert!(r.failed.is_empty() && r.live.is_empty());
}

/// Removal without any notice: the host closes on `r` (reason "removed") and rotates the key;
/// nobody else is told. Every remaining member, one of which joined by password (its
/// authority is the endpoint that handed it the key, [`Net::authority`]), exchanges KEY_PROOF
/// with each peer and drops `r`, which cannot prove the new key, within the proof timeout.
#[test]
fn rotation_cuts_a_removed_member_off_from_everyone_without_notice() {
    let mut h = Node::new(simple(Config {
        role: Role::Host,
        ..cfg("host", Some(K), Some("pw"))
    }));
    let hid = h.id();
    let inv = h.net.local_addr().clone();
    let mut a = member("a", 2, K, hid);
    let mut p = Node::new(simple(Config {
        bind_ip: lo(3).into(),
        ..cfg("p", None, None)
    }));
    let mut r = member("r", 4, K, hid);
    assert_eq!(a.net.authority(), Some(hid));
    assert_eq!(h.net.authority(), None);
    a.net.connect(inv.clone(), Join::Key);
    std::thread::sleep(Duration::from_millis(300));
    p.net.connect(inv.clone(), Join::Password("pw".into()));
    std::thread::sleep(Duration::from_millis(300));
    r.net.connect(inv.clone(), Join::Key);
    until(
        &mut [&mut h, &mut a, &mut p, &mut r],
        Duration::from_millis(300),
        full_mesh,
    );
    assert_eq!(p.net.authority(), Some(hid), "the password join's acceptor");
    let (&hr, _) = h.live.iter().find(|(_, x)| x.id == r.id()).unwrap();
    h.net.close(hr, sonobus_net::rt::CloseReason::Removed);
    until(&mut [&mut h, &mut r], Duration::ZERO, |n| {
        n[0].live.len() == 2
    });
    let seed = h.net.rotate_session_key(Duration::ZERO);
    let k2 = auth::key_from_seed(&seed);
    let t0 = Instant::now();
    until(&mut [&mut h, &mut a, &mut p, &mut r], Duration::ZERO, |n| {
        n[3].live.is_empty() && n[1].live.len() == 2 && n[2].live.len() == 2
    });
    let took = t0.elapsed();
    eprintln!("removed member cut off after {took:?}");
    assert!(
        took >= sonobus_net::rt::KEY_PROOF_TIMEOUT - Duration::from_millis(200),
        "{took:?}"
    );
    for n in [&a, &p] {
        assert_eq!(n.keys.last(), Some(&k2));
        assert!(n.live.values().all(|x| x.id != r.id()));
    }
    // r learnt why, from the host and from both members
    assert_eq!(
        r.closed.iter().filter(|c| *c == "removed").count(),
        3,
        "{:?}",
        r.closed
    );
    // the rest stay connected: no false removals after the proofs
    until(&mut [&mut h, &mut a, &mut p], Duration::from_secs(1), |n| {
        n.iter().all(|n| n.live.len() == 2)
    });
    // their only closed connection is the one to r, which they dropped themselves
    for n in [&a, &p] {
        assert_eq!(n.closed, ["removed: no proof of the current session key"]);
    }
}

/// Control frames queued right before a close reach the peer, and the reason comes with it.
#[test]
fn close_flushes_queued_frames_with_a_reason() {
    let mut h = Node::new(simple(Config {
        role: Role::Host,
        ..cfg("host", Some(K), None)
    }));
    let mut a = member("a", 2, K, h.id());
    a.net.connect(h.net.local_addr().clone(), Join::Key);
    until(&mut [&mut h, &mut a], Duration::ZERO, |n| {
        n.iter().all(|n| n.live.len() == 1)
    });
    let (&ha, _) = h.live.iter().next().unwrap();
    for i in 0..20u8 {
        h.net
            .send_control(ha, sonobus_net::wire::Frame::AppOsc(vec![i; 200]))
            .unwrap();
    }
    h.net.close(ha, sonobus_net::rt::CloseReason::Blocked);
    let mut got = vec![];
    let end = Instant::now() + WAIT;
    let reason = loop {
        assert!(Instant::now() < end, "timed out");
        match a.net.events().recv_timeout(Duration::from_millis(50)) {
            Ok(Event::Control {
                frame: sonobus_net::wire::Frame::AppOsc(b),
                ..
            }) => got.push(b[0]),
            Ok(Event::Closed { reason, .. }) => break reason,
            _ => {}
        }
    };
    assert_eq!(got, (0..20).collect::<Vec<u8>>());
    assert_eq!(reason, "blocked");
    // a host leaving tells its members so
    let mut b = member("b", 3, K, h.id());
    b.net.connect(h.net.local_addr().clone(), Join::Key);
    until(&mut [&mut h, &mut b], Duration::ZERO, |n| {
        n[1].live.len() == 1
    });
    drop(h);
    until(&mut [&mut b], Duration::ZERO, |n| n[0].live.is_empty());
    assert_eq!(b.closed, ["host left"]);
}

fn conn_to(n: &Node, id: Id) -> ConnId {
    *n.live.iter().find(|(_, p)| p.id == id).unwrap().0
}

/// Pump every node for `d`.
fn pump_for(nodes: &mut [&mut Node], d: Duration) {
    until(nodes, d, |_| true);
}

/// The engine's removal: close on `r`, then rotate at once, without waiting for the close to
/// take effect. The REKEY must never reach `r` (its link is closing), however the threads are
/// scheduled: several trials here; run it under `taskset -c 0` as well.
#[test]
fn close_then_rotate_never_sends_the_new_key_to_the_removed_member() {
    for trial in 0..5 {
        let mut h = Node::new(simple(Config {
            role: Role::Host,
            ..cfg("host", Some(K), None)
        }));
        let hid = h.id();
        let inv = h.net.local_addr();
        let mut a = member("a", 2, K, hid);
        let mut r = member("r", 3, K, hid);
        a.net.connect(inv.clone(), Join::Key);
        until(&mut [&mut h, &mut a], Duration::ZERO, |n| {
            n[1].live.len() == 1
        });
        r.net.connect(inv, Join::Key);
        until(&mut [&mut h, &mut a, &mut r], Duration::ZERO, full_mesh);
        let hr = conn_to(&h, r.id());
        h.net.close(hr, sonobus_net::rt::CloseReason::Removed);
        let k2 = auth::key_from_seed(&h.net.rotate_session_key(Duration::ZERO));
        // whatever h wrote to r arrives before r sees the close
        until(
            &mut [&mut h, &mut a, &mut r],
            Duration::from_millis(300),
            |n| n[2].live.values().all(|p| p.id != hid),
        );
        assert!(
            !r.keys.contains(&k2),
            "trial {trial}: the removed member got the new key"
        );
        assert_eq!(r.closed.first().map(String::as_str), Some("removed"));
    }
}

/// A handshake that overlaps a rotation proved the old key only: the peer must prove the new
/// one by the deadline like every other member (this one cannot, and is cut off).
#[test]
fn handshake_overlapping_a_rotation_must_prove_the_new_key() {
    use sonobus_net::auth::Side;
    use sonobus_net::rt::{CloseReason, KEY_PROOF_TIMEOUT};
    let q = simple(Config {
        role: Role::Host,
        ..cfg("q", Some(K), None)
    });
    block_on(async {
        let ep = raw_endpoint(lo(2)).await;
        let me = *ep.id().as_bytes();
        let to = sonobus_net::PeerAddr {
            id: me,
            addrs: ep.bound_sockets(),
            relay: None,
        };
        q.connect(to, Join::Key);
        let conn = ep.accept().await.unwrap().accept().unwrap().await.unwrap();
        let (mut send, mut recv) = conn.accept_bi().await.unwrap();
        assert!(matches!(rf(&mut recv).await, Ok(Frame::Hello(_))));
        // q rotates while it waits for our WELCOME
        q.rotate_session_key(Duration::ZERO);
        wait_async(&q, |e| matches!(e, Event::SessionKey(_)).then_some(())).await;
        let e = exporter(&conn, &auth::session_tag(SESSION));
        let mac = auth::key_proof(&K, Side::Acceptor, &e, &me, &q.local_addr().id);
        let w = sonobus_net::wire::Welcome {
            mac,
            key: None,
            roster: vec![],
            formats: vec![],
            mode: sonobus_net::wire::SessionMode::Mesh,
        };
        wf(&mut send, &Frame::Welcome(w)).await.unwrap();
        wait_async(&q, |e| matches!(e, Event::Connected { .. }).then_some(())).await;
        let t0 = Instant::now();
        let close = tokio::time::timeout(KEY_PROOF_TIMEOUT + Duration::from_secs(3), async {
            loop {
                match rf(&mut recv).await {
                    Ok(Frame::Close(c)) => return Some(c),
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await;
        assert_eq!(
            close,
            Ok(Some(CloseReason::Removed as u8)),
            "{:?}",
            t0.elapsed()
        );
        assert!(t0.elapsed() >= KEY_PROOF_TIMEOUT - Duration::from_millis(200));
    });
}

/// Two rotations back to back, the peer's proof of the second key arriving before we hold
/// either: the proof is kept until a later key verifies it, so the honest peer stays.
#[test]
fn proof_of_a_key_two_rotations_ahead_is_kept() {
    use sonobus_net::rt::KEY_PROOF_TIMEOUT;
    block_on(async {
        let ep = raw_endpoint(lo(2)).await;
        let me = *ep.id().as_bytes();
        // we are q's key authority
        let q = simple(Config {
            host: Some(me),
            ..cfg("q", Some(K), None)
        });
        let qa = q.local_addr();
        let mut c = key_join(&ep, &qa, &K).await.unwrap();
        wait_async(&q, |e| matches!(e, Event::Connected { .. }).then_some(())).await;
        let e = exporter(&c.conn, &auth::session_tag(SESSION));
        let (k2, k3) = ([2u8; 32], [3u8; 32]);
        let proof = auth::rekey_proof(&k3, &e, &me, &qa.id);
        wf(&mut c.send, &Frame::KeyProof(proof)).await.unwrap();
        wf(
            &mut c.send,
            &Frame::Rekey(Rekey {
                key: k2,
                grace_ms: 0,
                evicted: None,
            }),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        wf(
            &mut c.send,
            &Frame::Rekey(Rekey {
                key: k3,
                grace_ms: 0,
                evicted: None,
            }),
        )
        .await
        .unwrap();
        let end = tokio::time::timeout(KEY_PROOF_TIMEOUT + Duration::from_secs(2), async {
            loop {
                match rf(&mut c.recv).await {
                    Ok(Frame::Close(x)) => return Some(x),
                    Ok(_) => {}
                    Err(_) => return None,
                }
            }
        })
        .await;
        assert!(end.is_err(), "q closed on us: {end:?}");
        let mut keys = vec![];
        while let Ok(e) = q.events().try_recv() {
            if let Event::SessionKey(k) = e {
                keys.push(k);
            }
        }
        assert_eq!(keys, [k2, k3]);
    });
}

/// A plain invite refresh: `d` joined by password through member `c` (its key authority is
/// `c`, not the host). `c` passes the new key on, so `d` proves it and nobody is cut off.
#[test]
fn rotation_reaches_members_whose_authority_is_another_member() {
    use sonobus_net::rt::KEY_PROOF_TIMEOUT;
    let mut h = Node::new(simple(Config {
        role: Role::Host,
        ..cfg("host", Some(K), None)
    }));
    let hid = h.id();
    let mut c = member("c", 2, K, hid);
    c.net.set_password(Some("pw".into())).unwrap();
    c.net.connect(h.net.local_addr(), Join::Key);
    until(&mut [&mut h, &mut c], Duration::ZERO, |n| {
        n[1].live.len() == 1
    });
    let mut d = Node::new(simple(Config {
        bind_ip: lo(3).into(),
        ..cfg("d", None, None)
    }));
    d.net
        .connect(c.net.local_addr(), Join::Password("pw".into()));
    until(
        &mut [&mut h, &mut c, &mut d],
        Duration::from_millis(300),
        full_mesh,
    );
    assert_eq!(d.net.authority(), Some(c.id()));
    let k2 = auth::key_from_seed(&h.net.rotate_session_key(Duration::ZERO));
    pump_for(
        &mut [&mut h, &mut c, &mut d],
        KEY_PROOF_TIMEOUT + Duration::from_millis(1500),
    );
    assert_eq!(d.keys.last(), Some(&k2));
    for n in [&h, &c, &d] {
        assert!(n.closed.is_empty(), "{:?}", n.closed);
    }
    assert!(full_mesh(&[&mut h, &mut c, &mut d]));
}

/// The host evicts `r`, whose key authority is member `c`: the REKEY names `r`, so `c` blocks
/// it before it passes the new key on and `r` never gets it while `d`, another password
/// joiner through `c`, does and stays. Several trials; run it under `taskset -c 0`.
#[test]
fn evicted_member_never_gets_the_key_from_its_authority() {
    use sonobus_net::rt::KEY_PROOF_TIMEOUT;
    for trial in 0..2 {
        let mut h = Node::new(simple(Config {
            role: Role::Host,
            ..cfg("host", Some(K), None)
        }));
        let hid = h.id();
        let mut c = member("c", 2, K, hid);
        c.net.set_password(Some("pw".into())).unwrap();
        c.net.connect(h.net.local_addr(), Join::Key);
        until(&mut [&mut h, &mut c], Duration::ZERO, |n| {
            n[1].live.len() == 1
        });
        let ca = c.net.local_addr();
        let joiner = |name: &str, ip: u8| {
            let n = Node::new(simple(Config {
                bind_ip: lo(ip).into(),
                ..cfg(name, None, None)
            }));
            n.net.connect(ca.clone(), Join::Password("pw".into()));
            n
        };
        let mut r = joiner("r", 3);
        until(&mut [&mut h, &mut c, &mut r], Duration::ZERO, full_mesh);
        let mut d = joiner("d", 4);
        until(
            &mut [&mut h, &mut c, &mut r, &mut d],
            Duration::from_millis(300),
            full_mesh,
        );
        assert_eq!(r.net.authority(), Some(c.id()));
        let k2 = remove(&h, r.id());
        pump_for(
            &mut [&mut h, &mut c, &mut r, &mut d],
            KEY_PROOF_TIMEOUT + Duration::from_millis(1500),
        );
        assert!(!r.keys.contains(&k2), "trial {trial}: r got the new key");
        assert!(r.live.is_empty(), "trial {trial}: {:?}", r.live);
        assert_eq!(d.keys.last(), Some(&k2), "trial {trial}");
        let mut want = vec![hid, c.id()];
        want.sort();
        assert_eq!(d.peers(), want, "trial {trial}: {:?}", d.closed);
    }
}

/// The host removes `rid` ([`Net::evict`]); returns the new key.
fn remove(h: &Node, rid: Id) -> Key {
    auth::key_from_seed(&h.net.evict(rid))
}

/// A password joiner of `via` (no key of its own).
fn pw_joiner(name: &str, ip: u8, via: &Node) -> Node {
    let n = Node::new(simple(Config {
        bind_ip: lo(ip).into(),
        ..cfg(name, None, None)
    }));
    n.net
        .connect(via.net.local_addr(), Join::Password("pw".into()));
    n
}

/// Removal through a chain of password joins `h -> a -> b -> r` (each the key authority of
/// the next): the eviction travels with the REKEY, so `b` blocks `r` before it passes the new
/// key on and `r` never gets it. Several trials; run it under `taskset -c 0` as well.
#[test]
fn eviction_follows_the_rekey_down_a_chain_of_password_joins() {
    use sonobus_net::rt::KEY_PROOF_TIMEOUT;
    for trial in 0..2 {
        let mut h = Node::new(simple(Config {
            role: Role::Host,
            ..cfg("host", Some(K), Some("pw"))
        }));
        let mut a = pw_joiner("a", 2, &h);
        until(&mut [&mut h, &mut a], Duration::ZERO, full_mesh);
        a.net.set_password(Some("pw".into())).unwrap();
        let mut b = pw_joiner("b", 3, &a);
        until(&mut [&mut h, &mut a, &mut b], Duration::ZERO, full_mesh);
        b.net.set_password(Some("pw".into())).unwrap();
        let mut r = pw_joiner("r", 4, &b);
        until(
            &mut [&mut h, &mut a, &mut b, &mut r],
            Duration::from_millis(300),
            full_mesh,
        );
        assert_eq!(
            [a.net.authority(), b.net.authority(), r.net.authority()],
            [Some(h.id()), Some(a.id()), Some(b.id())]
        );
        let rid = r.id();
        let k2 = remove(&h, rid);
        pump_for(
            &mut [&mut h, &mut a, &mut b, &mut r],
            KEY_PROOF_TIMEOUT + Duration::from_millis(1500),
        );
        assert!(!r.keys.contains(&k2), "trial {trial}: r got the new key");
        assert!(r.live.is_empty(), "trial {trial}: {:?}", r.live);
        for (n, other) in [(&a, b.id()), (&b, a.id())] {
            assert_eq!(n.keys.last(), Some(&k2), "trial {trial}");
            let mut want = vec![h.id(), other];
            want.sort();
            assert_eq!(n.peers(), want, "trial {trial}: {:?}", n.closed);
        }
    }
}

/// Two rotations back to back, the second removing `r`, a password joiner of member `c`,
/// whose app is slow to take events: `c` must never pass the second key to `r` on the
/// strength of the first rotation. Run it under `taskset -c 0` as well.
#[test]
fn a_second_rotation_removing_a_delegated_member_never_leaks_its_key() {
    use sonobus_net::rt::KEY_PROOF_TIMEOUT;
    for trial in 0..2 {
        let mut h = Node::new(simple(Config {
            role: Role::Host,
            ..cfg("host", Some(K), None)
        }));
        let hid = h.id();
        let mut c = member("c", 2, K, hid);
        c.net.set_password(Some("pw".into())).unwrap();
        c.net.connect(h.net.local_addr(), Join::Key);
        until(&mut [&mut h, &mut c], Duration::ZERO, |n| {
            n[1].live.len() == 1
        });
        let mut r = pw_joiner("r", 3, &c);
        until(&mut [&mut h, &mut c, &mut r], Duration::ZERO, full_mesh);
        let mut d = pw_joiner("d", 4, &c);
        until(
            &mut [&mut h, &mut c, &mut r, &mut d],
            Duration::from_millis(300),
            full_mesh,
        );
        let rid = r.id();
        c.key_delay = Duration::from_millis(200);
        let k2 = auth::key_from_seed(&h.net.rotate_session_key(Duration::ZERO));
        let k3 = remove(&h, rid);
        // both REKEYs reach c before its app takes any event
        std::thread::sleep(Duration::from_millis(300));
        pump_for(
            &mut [&mut h, &mut c, &mut r, &mut d],
            KEY_PROOF_TIMEOUT + Duration::from_millis(1500),
        );
        assert!(
            !r.keys.contains(&k3),
            "trial {trial}: r got the key that removed it"
        );
        assert!(r.live.is_empty(), "trial {trial}: {:?}", r.live);
        assert_eq!(c.keys, [k2, k3], "trial {trial}");
        assert_eq!(d.keys.last(), Some(&k3), "trial {trial}");
        let mut want = vec![hid, c.id()];
        want.sort();
        assert_eq!(d.peers(), want, "trial {trial}: {:?}", d.closed);
    }
}

/// A grace-0 rotation while an honest password joiner is between PAKE2 and PAKE3: its
/// WELCOME carries the key current once the password is confirmed, so it holds the new key
/// (an older one would get it cut off at the proof deadline).
#[test]
fn password_join_overlapping_a_rotation_gets_the_current_key() {
    use sonobus_net::auth::Side;
    use sonobus_net::wire::Method;
    let q = simple(Config {
        role: Role::Host,
        ..cfg("q", Some(K), Some("pw"))
    });
    let qa = q.local_addr();
    block_on(async {
        let ep = raw_endpoint(lo(2)).await;
        let me = *ep.id().as_bytes();
        let conn = dial(&ep, &qa).await.unwrap();
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        let tag = auth::session_tag(SESSION);
        let e = exporter(&conn, &tag);
        let (c, x) = auth::PakeConnector::start("pw", &e, &me, &qa.id);
        wf(&mut send, &hello(Method::Pake, tag, x)).await.unwrap();
        let Ok(Frame::Pake2(p2)) = rf(&mut recv).await else {
            panic!("no PAKE2")
        };
        let k2 = auth::key_from_seed(&q.rotate_session_key(Duration::ZERO));
        wait_async(&q, |e| matches!(e, Event::SessionKey(_)).then_some(())).await;
        let (kc, conf) = c.finish(&p2.y).unwrap();
        wf(&mut send, &Frame::Pake3 { conf }).await.unwrap();
        let Ok(Frame::Welcome(w)) = rf(&mut recv).await else {
            panic!("no WELCOME")
        };
        let k = kc.unwrap(&w.key.unwrap()).unwrap();
        assert!(k == k2, "WELCOME wrapped the key from before the rotation");
        auth::key_verify(&k2, Side::Acceptor, &e, &qa.id, &me, &w.mac).unwrap();
    });
}

/// A mesh member dialing a peer that is not its key authority (a roster dial) ignores a
/// WELCOME that says broadcast: it stays a mesh member and still accepts connections. Only the
/// endpoint we joined through can make us a listener.
#[test]
fn broadcast_welcome_from_a_roster_peer_is_ignored() {
    use sonobus_net::auth::Side;
    use sonobus_net::wire::SessionMode;
    let q = simple(Config {
        host: Some([9; 32]),
        ..cfg("q", Some(K), None)
    });
    block_on(async {
        let ep = raw_endpoint(lo(2)).await;
        let me = *ep.id().as_bytes();
        let to = sonobus_net::PeerAddr {
            id: me,
            addrs: ep.bound_sockets(),
            relay: None,
        };
        q.connect(to, Join::Key);
        let conn = ep.accept().await.unwrap().accept().unwrap().await.unwrap();
        let (mut send, _recv) = conn.accept_bi().await.unwrap();
        let e = exporter(&conn, &auth::session_tag(SESSION));
        let w = sonobus_net::wire::Welcome {
            mac: auth::key_proof(&K, Side::Acceptor, &e, &me, &q.local_addr().id),
            key: None,
            roster: vec![],
            formats: vec![],
            mode: SessionMode::Broadcast,
        };
        wf(&mut send, &Frame::Welcome(w)).await.unwrap();
        let mut seen = vec![];
        wait_async(&q, |e| {
            seen.push(kind(&e));
            matches!(e, Event::Connected { .. }).then_some(())
        })
        .await;
        assert!(!seen.iter().any(|s| s == "JoinedBroadcast"), "{seen:?}");
        conn.close(0u32.into(), b"");
    });
    let p = simple(cfg("p", Some(K), None));
    p.connect(q.local_addr(), Join::Key);
    connected(&p);
}
