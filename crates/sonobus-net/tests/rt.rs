//! Runtime tests over 127.x only (relays disabled, no address lookup).

mod common;

use std::net::UdpSocket;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use sonobus_net::auth::{self, Key};
use sonobus_net::rt::CLOSE_AUTH;
use sonobus_net::wire::{self, AudioHeader, Datagram, Frame, Method, Reassembler, Subscribe};
use sonobus_net::{Config, ConnId, Event, Join, Net, PeerInfo, PeerTx, Sink};

const K1: Key = [0x11; 32];
const K2: Key = [0x22; 32];

fn t0() -> Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

/// Connect `b` to `a` with `join`, return (a's view, b's view).
fn pair(a: &Net, b: &Net, join: Join) -> ((ConnId, PeerTx, PeerInfo), (ConnId, PeerTx, PeerInfo)) {
    b.connect(a.local_addr().clone(), join);
    let bv = connected(b);
    (connected(a), bv)
}

/// p99 one-way latency of 2000 datagrams at 400/s from a fresh pair.
fn latency_run() -> Duration {
    let (tx, rx) = channel::<(u32, u64)>();
    let a = Net::start(cfg("a", Some(K1), None), move |_, _| {
        let tx = tx.clone();
        Box::new(move |d: Bytes| {
            let now = t0().elapsed().as_nanos() as u64;
            if let Ok(Datagram::Audio(au)) = Datagram::decode(&d) {
                let sent = u64::from_le_bytes(au.payload[..8].try_into().unwrap());
                let _ = tx.send((au.header.seq, now - sent));
            }
        }) as Sink
    })
    .unwrap();
    let b = simple(cfg("b", Some(K1), None));
    let ((_, _, pa), (_, btx, pb)) = pair(&a, &b, Join::Key);
    assert_eq!(
        (pa.name.as_str(), pa.incoming, pb.name.as_str(), pb.incoming),
        ("b", true, "a", false)
    );
    assert_eq!(pb.id, a.local_addr().id);
    // The acceptor learns where the connector can be dialed.
    assert!(
        pa.addrs.contains(&b.local_addr().addrs[0]),
        "{:?}",
        pa.addrs
    );
    let path = btx.path().expect("selected path");
    assert!(
        path.direct && path.remote == Some(a.local_addr().addrs[0]),
        "{path:?}"
    );

    const N: u32 = 2000;
    let sender = std::thread::spawn(move || {
        let period = Duration::from_micros(2500);
        let start = Instant::now();
        let mut buf = Vec::new();
        let mut payload = [0u8; 200];
        for seq in 0..N {
            let due = start + period * seq;
            while Instant::now() < due {
                let left = due - Instant::now();
                if left > Duration::from_micros(300) {
                    std::thread::sleep(left - Duration::from_micros(200));
                } else {
                    std::hint::spin_loop();
                }
            }
            payload[..8].copy_from_slice(&(t0().elapsed().as_nanos() as u64).to_le_bytes());
            let h = AudioHeader {
                seq,
                ..AudioHeader::default()
            };
            wire::encode_audio(&h, &[], &payload, &mut buf).unwrap();
            btx.send(Bytes::copy_from_slice(&buf)).unwrap();
        }
    });
    sender.join().unwrap();
    let mut got = Vec::new();
    while got.len() < N as usize {
        got.push(
            rx.recv_timeout(Duration::from_secs(2))
                .expect("datagram missing"),
        );
    }
    let reordered = got.windows(2).filter(|w| w[1].0 < w[0].0).count();
    let mut seqs: Vec<u32> = got.iter().map(|g| g.0).collect();
    seqs.sort();
    seqs.dedup();
    assert_eq!(seqs.len(), N as usize);
    assert!(reordered < N as usize / 100);
    let mut lat: Vec<u64> = got.iter().map(|g| g.1).collect();
    lat.sort();
    let p = |q: f64| Duration::from_nanos(lat[((lat.len() - 1) as f64 * q) as usize]);
    println!(
        "loopback one-way latency over {N} datagrams: p50 {:?} p99 {:?} max {:?}; reordered {reordered}",
        p(0.5),
        p(0.99),
        p(1.0)
    );
    p(0.99)
}

#[test]
fn datagrams_2000_at_400_per_second() {
    // Timing on a shared CI machine: generous bound, one retry.
    let bound = Duration::from_millis(50);
    if latency_run() >= bound {
        assert!(latency_run() < bound);
    }
}

#[test]
fn wrong_key_is_rejected_and_gets_nothing() {
    let (ca, cb) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let a = net(cfg("a", Some(K1), None), ca.clone());
    let b = net(cfg("b", Some(K2), None), cb.clone());
    b.connect(a.local_addr().clone(), Join::Key);
    assert_eq!(failed(&b).0, "authentication failed");
    // The acceptor's detail is local only.
    assert_eq!(incoming_failed(&a), "wrong session key");

    // A raw client with the wrong key sends datagrams before and after its HELLO and control
    // frames after it: nothing may reach a's sink or events.
    let to = a.local_addr().clone();
    let code = block_on(async {
        let ep = raw_endpoint(lo(1)).await;
        let c = dial(&ep, &to).await.unwrap();
        for _ in 0..20 {
            let _ = c.send_datagram(audio());
        }
        let (mut s, mut r) = c.open_bi().await.unwrap();
        let tag = auth::session_tag(SESSION);
        let e = exporter(&c, &tag);
        let proof = auth::key_proof(&K2, auth::Side::Connector, &e, ep.id().as_bytes(), &to.id);
        wf(&mut s, &hello(Method::Key, tag, proof.to_vec()))
            .await
            .unwrap();
        for _ in 0..20 {
            let _ = c.send_datagram(audio());
            let _ = wf(&mut s, &Frame::AppOsc(b"/sb/x\0\0\0".to_vec())).await;
        }
        assert!(rf(&mut r).await.is_err(), "got a frame");
        closed_by_peer(&c).await
    });
    assert_eq!(code, Some((CLOSE_AUTH as u64, Bytes::new())));
    let ea = drain(&a, Duration::from_millis(1200));
    assert!(
        ea.iter().all(|e| e.starts_with("IncomingFailed")) && !ea.is_empty(),
        "{ea:?}"
    );
    assert!(drain(&b, Duration::from_millis(1)).is_empty());
    assert_eq!(
        (ca.load(Ordering::Relaxed), cb.load(Ordering::Relaxed)),
        (0, 0)
    );

    // Same key but another session name: the joiner only learns "authentication failed".
    let c = simple(Config {
        session: "other".into(),
        ..cfg("c", Some(K1), None)
    });
    c.connect(a.local_addr().clone(), Join::Key);
    assert_eq!(failed(&c).0, "authentication failed");
    assert_eq!(incoming_failed(&a), "unknown session");
}

#[test]
fn pake_join_learns_key_and_key_proves_to_third() {
    let host = simple(cfg("host", Some(K1), Some("open sesame")));
    let third = simple(cfg("third", Some(K1), None));
    let joiner = simple(cfg("joiner", None, None));
    // Typed with surrounding whitespace: normalised on both sides.
    joiner.connect(
        host.local_addr().clone(),
        Join::Password("  open sesame\n".into()),
    );
    let k = wait(&joiner, |e| match e {
        Event::SessionKey(k) => Some(k),
        Event::ConnectFailed { reason, .. } => panic!("{reason}"),
        _ => None,
    });
    assert_eq!(k, K1);
    let (_, _, hp) = connected(&joiner);
    assert_eq!(hp.name, "host");
    assert!(!hp.is_invite_host);
    assert_eq!(connected(&host).2.name, "joiner");
    // Now a member: KEY proof to a third endpoint.
    let ((_, _, tp), (_, _, jp)) = pair(&third, &joiner, Join::Key);
    assert_eq!((tp.name.as_str(), jp.name.as_str()), ("joiner", "third"));
}

#[test]
fn wrong_password_backs_off_per_address() {
    let host = simple(cfg("host", None, Some("right")));
    let joiner = simple(cfg("joiner", None, None));
    for i in 1..=3 {
        joiner.connect(host.local_addr().clone(), Join::Password("wrong".into()));
        assert_eq!(failed(&joiner).0, "authentication failed");
        let n = wait(&host, |e| match e {
            Event::JoinFailed { failed_total } => Some(failed_total),
            _ => None,
        });
        assert_eq!(n, i);
    }
    let third_failed_at = Instant::now();
    // Even the right password is refused from this address during back-off, before any PAKE
    // work, with the delay as a number.
    joiner.connect(host.local_addr().clone(), Join::Password("right".into()));
    let (reason, retry) = failed(&joiner);
    assert!(reason.starts_with("retry later"), "{reason}");
    assert_eq!(retry, Some(Duration::from_secs(2)));
    let ip = wait(&host, |e| match e {
        Event::Throttled { ip, .. } => Some(ip),
        _ => None,
    });
    assert_eq!(ip, Some(lo(1).into()));
    // Another address is not affected.
    let other = simple(Config {
        bind_ip: lo(2).into(),
        ..cfg("other", None, None)
    });
    other.connect(host.local_addr().clone(), Join::Password("right".into()));
    connected(&other);
    std::thread::sleep(Duration::from_millis(2200).saturating_sub(third_failed_at.elapsed()));
    joiner.connect(host.local_addr().clone(), Join::Password("right".into()));
    connected(&joiner);
}

#[test]
fn control_frames_round_trip() {
    let a = simple(cfg("a", Some(K1), None));
    let b = simple(cfg("b", Some(K1), None));
    let ((ca, _, _), (cb, _, _)) = pair(&a, &b, Join::Key);
    let osc = b"/sb/chat\0\0\0\0,s\0\0hi\0\0".to_vec();
    a.send_control(ca, Frame::AppOsc(osc.clone())).unwrap();
    let f = wait(&b, |e| match e {
        Event::Control { conn, frame } if conn == cb => Some(frame),
        _ => None,
    });
    assert_eq!(f, Frame::AppOsc(osc));
    let sub = Frame::Subscribe(Subscribe {
        stream: 1,
        encoding: 2,
        redundancy: 1,
    });
    b.send_control(cb, sub.clone()).unwrap();
    assert_eq!(
        wait(&a, |e| match e {
            Event::Control { frame, .. } => Some(frame),
            _ => None,
        }),
        sub
    );
    // Disconnect says BYE: both sides see Closed.
    b.disconnect(cb);
    wait(&a, |e| {
        matches!(e, Event::Closed { conn, .. } if conn == ca).then_some(())
    });
    wait(&b, |e| {
        matches!(e, Event::Closed { conn, .. } if conn == cb).then_some(())
    });
}

#[test]
fn fragmented_block_is_reassembled() {
    let (tx, rx) = channel::<Vec<u8>>();
    let a = Net::start(cfg("a", Some(K1), None), move |_, _| {
        let tx = tx.clone();
        let mut r = Reassembler::new(4, 16 * 1024, Duration::from_millis(200));
        Box::new(move |d: Bytes| {
            let Ok(Datagram::Audio(au)) = Datagram::decode(&d) else {
                return;
            };
            match au.fragment {
                Some((i, n)) => {
                    if let Some(b) = r.push(Instant::now(), &au.header, i, n, au.payload) {
                        let _ = tx.send(b.to_vec());
                    }
                }
                None => drop(tx.send(au.payload.to_vec())),
            }
        }) as Sink
    })
    .unwrap();
    let b = simple(cfg("b", Some(K1), None));
    let (_, (_, btx, _)) = pair(&a, &b, Join::Key);
    let block: Vec<u8> = (0..5000u32).map(|i| (i * 31 % 251) as u8).collect();
    let h = AudioHeader {
        stream: 2,
        generation: 1,
        seq: 77,
        ..AudioHeader::default()
    };
    let mut n = 0;
    wire::fragment(&h, &block, |d| {
        assert!(d.len() <= wire::MAX_DATAGRAM);
        n += 1;
        btx.send(Bytes::copy_from_slice(d)).unwrap();
    })
    .unwrap();
    assert_eq!(n, 5);
    assert_eq!(rx.recv_timeout(WAIT).unwrap(), block);
}

/// Total and worst single-send time of 400 sends while the receiver's runtime is blocked.
fn blocked_sends() -> (Duration, Duration) {
    // a's sink blocks a's runtime thread for 300 ms on the first datagram.
    let (entered_tx, entered_rx) = channel::<()>();
    let a = Net::start(cfg("a", Some(K1), None), move |_, _| {
        let entered = entered_tx.clone();
        Box::new(move |_| {
            let _ = entered.send(());
            std::thread::sleep(Duration::from_millis(300));
        }) as Sink
    })
    .unwrap();
    let b = simple(cfg("b", Some(K1), None));
    let ((_, atx, _), (_, btx, _)) = pair(&a, &b, Join::Key);
    btx.send(Bytes::from_static(&[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]))
        .unwrap();
    entered_rx.recv_timeout(WAIT).unwrap();
    // 400 datagrams (more than the 32 KiB queue holds) while a's runtime cannot drain anything.
    let d = Bytes::from(vec![0u8; 1000]);
    let start = Instant::now();
    let mut worst = Duration::ZERO;
    for _ in 0..400 {
        let t = Instant::now();
        atx.send(d.clone()).unwrap();
        worst = worst.max(t.elapsed());
    }
    let total = start.elapsed();
    println!("400 sends with the runtime blocked: total {total:?}, worst single send {worst:?}");
    assert!(
        atx.send(Bytes::from(vec![0u8; wire::MAX_DATAGRAM + 1]))
            .is_err()
    );
    (total, worst)
}

#[test]
fn send_does_not_wait_for_a_blocked_runtime() {
    // The runtime is blocked for 300 ms: waiting on it would take at least that long. Generous
    // bounds well below it, one retry for a noisy machine.
    let ok = |(total, worst): (Duration, Duration)| {
        total < Duration::from_millis(200) && worst < Duration::from_millis(50)
    };
    if !ok(blocked_sends()) {
        let r = blocked_sends();
        assert!(ok(r), "send waited on the runtime: {r:?}");
    }
}

#[test]
fn port_fallback_when_taken() {
    let taken = UdpSocket::bind("127.0.0.1:0").unwrap();
    let port = taken.local_addr().unwrap().port();
    let n = simple(Config {
        port,
        ..cfg("a", Some(K1), None)
    });
    let got = n.local_addr().addrs[0];
    assert!(got.port() != port && got.port() != 0, "{got}");
}

#[test]
fn datagrams_flow_both_ways() {
    let (ca, cb) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let a = net(cfg("a", Some(K1), None), ca.clone());
    let b = net(cfg("b", Some(K1), None), cb.clone());
    let ((_, atx, _), (_, btx, _)) = pair(&a, &b, Join::Key);
    for _ in 0..10 {
        atx.send(Bytes::from_static(b"x")).unwrap();
        btx.send(Bytes::from_static(b"y")).unwrap();
    }
    let end = Instant::now() + WAIT;
    while (ca.load(Ordering::Relaxed) < 10 || cb.load(Ordering::Relaxed) < 10)
        && Instant::now() < end
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        (ca.load(Ordering::Relaxed), cb.load(Ordering::Relaxed)),
        (10, 10)
    );
    assert!(drain(&a, Duration::from_millis(50)).is_empty());
}

#[test]
fn identity_persists_and_addresses_are_concrete() {
    let sk = sonobus_net::generate_secret_key();
    let a = simple(Config {
        secret_key: Some(sk),
        ..cfg("a", Some(K1), None)
    });
    let id = a.local_addr().id;
    drop(a);
    let a = simple(Config {
        secret_key: Some(sk),
        ..cfg("a", Some(K1), None)
    });
    assert_eq!(a.local_addr().id, id);
    // Bound to the unspecified address: real interface addresses only, never 0.0.0.0, no
    // loopback.
    let any = simple(Config {
        bind_ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
        ..cfg("any", Some(K1), None)
    });
    let addrs = &any.local_addr().addrs;
    assert!(
        addrs
            .iter()
            .all(|a| !a.ip().is_unspecified() && !a.ip().is_loopback()),
        "{addrs:?}"
    );
    // Loopback-bound: loopback is advertised.
    assert!(a.local_addr().addrs.iter().all(|a| a.ip().is_loopback()));
    // Bad PIN at start is refused.
    assert!(
        Net::start(
            Config {
                pin_mode: true,
                ..cfg("p", None, Some("12a4"))
            },
            |_, _| Box::new(|_| {}) as Sink
        )
        .is_err()
    );
}
