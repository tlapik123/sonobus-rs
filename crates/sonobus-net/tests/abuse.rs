//! Hostile-peer tests: a raw iroh client bypasses `Net` to misbehave. Loopback only, relays
//! disabled. Separate 127.0.0.x source addresses stand in for separate hosts.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use sonobus_net::auth::{self, Key};
use sonobus_net::rt::{
    AUTH_TIMEOUT, CLOSE_AUTH, CLOSE_RETRY_LATER, CONTROL_FRAMES_PER_SEC, EVENT_QUEUE_BYTES,
    MAX_PENDING_PER_IP,
};
use sonobus_net::wire::{Frame, Method, PeerEntry, Rekey, Role};
use sonobus_net::{Config, ControlError, Event, Join, Net, SendError, Sink};

const K: Key = [0x11; 32];

fn host_pw(pw: &str, pin: bool) -> Net {
    simple(Config {
        pin_mode: pin,
        ..cfg("host", Some(K), Some(pw))
    })
}

fn at(n: u8, c: Config) -> Config {
    Config {
        bind_ip: lo(n).into(),
        ..c
    }
}

/// Finding 1: a member that stops reading its control stream is disconnected in bounded time
/// and our queue for it is bounded.
#[test]
fn non_reading_peer_is_disconnected_and_queue_is_bounded() {
    let count = Arc::new(AtomicUsize::new(0));
    let host = net(cfg("host", Some(K), None), count.clone());
    let to = host.local_addr().clone();
    block_on(async {
        let ep = raw_endpoint(lo(1)).await;
        // 1. Disconnect while our writes are stuck under the peer's flow control.
        let rc = key_join(&ep, &to, &K).await.unwrap();
        let conn = wait_async(&host, |e| match e {
            Event::Connected { conn, .. } => Some(conn),
            _ => None,
        })
        .await;
        for _ in 0..12 {
            host.send_control(conn, Frame::AppOsc(vec![0; 60_000]))
                .unwrap();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        let t = Instant::now();
        host.disconnect(conn);
        let reason = wait_async(&host, |e| match e {
            Event::Closed { conn: c, reason } if c == conn => Some(reason),
            _ => None,
        })
        .await;
        println!(
            "Closed after disconnect of a non-reading peer: {:?} ({reason})",
            t.elapsed()
        );
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(rc.conn.close_reason().is_some() || closed_by_peer(&rc.conn).await.is_some());
        // Datagrams no longer reach the sink.
        let before = count.load(Ordering::Relaxed);
        for _ in 0..20 {
            let _ = rc.conn.send_datagram(audio());
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(count.load(Ordering::Relaxed), before);
        assert_eq!(
            host.send_control(conn, Frame::AppOsc(vec![1])),
            Err(ControlError::NotConnected)
        );

        // 2. Keep queueing to a non-reader: the queue overflows, send_control says so and the
        // connection is closed with a clear reason.
        let rc = key_join(&ep, &to, &K).await.unwrap();
        let conn = wait_async(&host, |e| match e {
            Event::Connected { conn, .. } => Some(conn),
            _ => None,
        })
        .await;
        let mut overflowed = false;
        for _ in 0..200 {
            match host.send_control(conn, Frame::AppOsc(vec![0; 60_000])) {
                Ok(()) => {}
                Err(ControlError::Overflow) => {
                    overflowed = true;
                    break;
                }
                Err(e) => panic!("{e:?}"),
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(overflowed, "12 MB queued for a peer that reads nothing");
        let reason = wait_async(&host, |e| match e {
            Event::Closed { conn: c, reason } if c == conn => Some(reason),
            _ => None,
        })
        .await;
        assert_eq!(reason, "peer not reading control stream");
        drop(rc);
    });
}

/// Finding 2: unauthenticated connections are capped per source address and refused before
/// TLS; other addresses still get in.
#[test]
fn pending_connections_are_capped_per_address() {
    let host = simple(cfg("host", Some(K), None));
    let to = host.local_addr().clone();
    block_on(async {
        let ep = raw_endpoint(lo(1)).await;
        // Hold MAX_PENDING_PER_IP connections that never send HELLO.
        let mut held = vec![];
        for _ in 0..MAX_PENDING_PER_IP {
            held.push(dial(&ep, &to).await.unwrap());
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
        let refused = dial(&ep, &to).await;
        assert!(
            refused.is_err(),
            "connection over the per-address cap accepted"
        );
        let reason = wait_async(&host, |e| match e {
            Event::IncomingFailed { last_reason, .. } => Some(last_reason),
            _ => None,
        })
        .await;
        assert!(reason.contains("too many unauthenticated"), "{reason}");
        // Another source address is unaffected.
        let ep2 = raw_endpoint(lo(2)).await;
        key_join(&ep2, &to, &K).await.unwrap();
        // Dropping the held connections frees the slots.
        for c in held.drain(..) {
            c.close(0u32.into(), b"");
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        key_join(&ep, &to, &K).await.unwrap();
    });
}

/// Finding 2: an oversized handshake frame is refused at its header, not after a timeout.
#[test]
fn oversized_handshake_frame_is_refused_at_once() {
    let host = simple(cfg("host", Some(K), None));
    let to = host.local_addr().clone();
    block_on(async {
        let ep = raw_endpoint(lo(1)).await;
        let c = dial(&ep, &to).await.unwrap();
        let (mut s, _r) = c.open_bi().await.unwrap();
        let t = Instant::now();
        // Claims a 65535-byte HELLO, sends one byte of it.
        s.write_all(&[0xff, 0xff, 1, 0]).await.unwrap();
        let code = closed_by_peer(&c).await;
        assert_eq!(code, Some((CLOSE_AUTH as u64, Bytes::new())));
        assert!(t.elapsed() < AUTH_TIMEOUT / 2, "{:?}", t.elapsed());
    });
}

/// Finding 5: the unauthenticated peer only ever learns "auth failed" (empty reason) or
/// "retry later" (seconds); unknown session and wrong password run the same PAKE path.
#[test]
fn outward_failures_are_opaque() {
    let host = host_pw("right", false);
    let to = host.local_addr().clone();
    block_on(async {
        let ep = raw_endpoint(lo(1)).await;
        let auth_failed = Some((CLOSE_AUTH as u64, Bytes::new()));
        // KEY proof, unknown session.
        let c = dial(&ep, &to).await.unwrap();
        let (mut s, _r) = c.open_bi().await.unwrap();
        let other = auth::session_tag("other");
        wf(&mut s, &hello(Method::Key, other, vec![0; 32]))
            .await
            .unwrap();
        assert_eq!(closed_by_peer(&c).await, auth_failed);
        // KEY proof, wrong key.
        assert!(key_join(&ep, &to, &[9; 32]).await.is_err());
        // PAKE: unknown session and wrong password look the same: PAKE2 (33-byte Y), then
        // auth failed after PAKE3.
        let ep2 = raw_endpoint(lo(2)).await;
        let unknown = raw_pake(&ep2, &to, "other", "right", true).await;
        let wrong = raw_pake(&ep2, &to, SESSION, "wrong", true).await;
        assert_eq!(unknown, Pake::Completed(auth_failed.clone()));
        assert_eq!(wrong, Pake::Completed(auth_failed.clone()));
        assert_eq!(
            raw_pake(&ep2, &to, "other", "x", false).await,
            Pake::Abandoned(33)
        );
        // Throttled: retry later with the delay in seconds.
        match raw_pake(&ep2, &to, SESSION, "right", true).await {
            Pake::Refused(Some((code, reason))) => {
                assert_eq!(code, CLOSE_RETRY_LATER as u64);
                let secs: u64 = std::str::from_utf8(&reason).unwrap().parse().unwrap();
                assert!((1..=3).contains(&secs), "{secs}");
            }
            r => panic!("{r:?}"),
        }
    });
    // Details stay local, coalesced into counts (at most one report per second).
    let mut failures = 0;
    let end = Instant::now() + Duration::from_millis(1500);
    while let Ok(e) = host
        .events()
        .recv_timeout(end.saturating_duration_since(Instant::now()))
    {
        if let Event::IncomingFailed { count, .. } = e {
            failures += count;
        }
    }
    assert_eq!(failures, 6);
}

/// Finding 3: HELLOs that never send PAKE3 have tested no guess and do not kill the PIN.
#[test]
fn abandoned_pin_attempts_do_not_invalidate_the_pin() {
    let host = host_pw("123456", true);
    let to = host.local_addr().clone();
    block_on(async {
        for n in 0..auth::PIN_FAILURES as u8 + 1 {
            let ep = raw_endpoint(lo(10 + n)).await;
            assert_eq!(
                raw_pake(&ep, &to, SESSION, "000000", false).await,
                Pake::Abandoned(33)
            );
        }
    });
    let joiner = simple(at(2, cfg("joiner", None, None)));
    joiner.connect(to, Join::Password("123-456".into()));
    connected(&joiner);
    assert!(!drain(&host, Duration::from_millis(300)).contains(&"PinInvalidated".to_string()));
}

/// Finding 3: completed wrong guesses do invalidate the PIN (with an event); a new PIN works.
#[test]
fn completed_wrong_guesses_invalidate_the_pin() {
    let host = host_pw("123456", true);
    let to = host.local_addr().clone();
    block_on(async {
        for n in 0..auth::PIN_FAILURES as u8 {
            let ep = raw_endpoint(lo(20 + n)).await;
            let r = raw_pake(&ep, &to, SESSION, "000000", true).await;
            assert_eq!(r, Pake::Completed(Some((CLOSE_AUTH as u64, Bytes::new()))));
        }
    });
    wait(&host, |e| matches!(e, Event::PinInvalidated).then_some(()));
    let joiner = simple(at(2, cfg("joiner", None, None)));
    joiner.connect(to.clone(), Join::Password("123456".into()));
    assert_eq!(failed(&joiner).0, "authentication failed");
    host.set_password(Some("654 321".into())).unwrap();
    assert!(host.set_password(Some("65a321".into())).is_err());
    joiner.connect(to, Join::Password("654321".into()));
    connected(&joiner);
}

/// Finding 3: invite-link (KEY) joins are never subject to the lockout.
#[test]
fn key_joins_are_unaffected_by_lockout() {
    let host = host_pw("right", false);
    let to = host.local_addr().clone();
    block_on(async {
        // Exhaust the global budget from many addresses.
        for n in 0..auth::GLOBAL_BURST as u8 {
            let ep = raw_endpoint(lo(30 + n)).await;
            raw_pake(&ep, &to, SESSION, "wrong", true).await;
        }
        let ep = raw_endpoint(lo(30)).await;
        assert!(matches!(
            raw_pake(&ep, &to, SESSION, "right", true).await,
            Pake::Refused(Some((c, _))) if c == CLOSE_RETRY_LATER as u64
        ));
    });
    wait(&host, |e| match e {
        Event::Throttled { ip: None, .. } => Some(()),
        _ => None,
    });
    // KEY join from a throttled address: immediate.
    let member = simple(at(30, cfg("member", Some(K), None)));
    member.connect(to, Join::Key);
    connected(&member);
}

/// Finding 4: a member that floods control frames is throttled, then disconnected.
#[test]
fn control_flood_is_throttled_then_disconnected() {
    let host = simple(cfg("host", Some(K), None));
    let to = host.local_addr().clone();
    let (controls, reason, took) = block_on(async {
        let ep = raw_endpoint(lo(1)).await;
        let mut rc = key_join(&ep, &to, &K).await.unwrap();
        let t = Instant::now();
        let flood = async {
            loop {
                if wf(&mut rc.send, &Frame::AppOsc(vec![1; 8])).await.is_err() {
                    break;
                }
            }
            std::future::pending::<()>().await
        };
        let mut controls = 0;
        let watch = wait_async(&host, |e| match e {
            Event::Control { .. } => {
                controls += 1;
                None
            }
            Event::Closed { reason, .. } => Some(reason),
            _ => None,
        });
        let reason = tokio::select! {
            r = watch => r,
            _ = flood => unreachable!(),
        };
        (controls, reason, t.elapsed())
    });
    println!("flood: {controls} control events delivered, closed after {took:?}: {reason}");
    assert_eq!(reason, "control flood");
    assert!(
        controls <= CONTROL_FRAMES_PER_SEC as usize * 6,
        "{controls}"
    );
    assert!(took < Duration::from_secs(10));
}

/// Finding 4: when the app does not read events, a member's control frames stop being read
/// (bounded queue) instead of piling up; reading resumes the flow.
#[test]
fn event_queue_applies_back_pressure() {
    let host = simple(cfg("host", Some(K), None));
    let to = host.local_addr().clone();
    let frame = 60_000usize;
    let written = Arc::new(AtomicUsize::new(0));
    let w2 = written.clone();
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
    let client = std::thread::spawn(move || {
        block_on(async move {
            let ep = raw_endpoint(lo(1)).await;
            let mut rc = key_join(&ep, &to, &K).await.unwrap();
            // ~180 KB/s: under the rate limit, so only the event queue can stop it.
            loop {
                if stop_rx.try_recv().is_ok() {
                    break;
                }
                let f = Frame::AppOsc(vec![7; frame]);
                let w = wf(&mut rc.send, &f);
                if tokio::time::timeout(Duration::from_millis(330), w)
                    .await
                    .is_ok()
                {
                    w2.fetch_add(frame, Ordering::Relaxed);
                }
                tokio::time::sleep(Duration::from_millis(330)).await;
            }
        })
    });
    // Do not read events for 4 s.
    std::thread::sleep(Duration::from_secs(4));
    let mut queued = 0;
    while let Ok(e) = host.events().try_recv() {
        queued += matches!(e, Event::Control { .. }) as usize;
    }
    println!(
        "back-pressure: {queued} control events queued, {} KB written by the peer",
        written.load(Ordering::Relaxed) / 1000
    );
    assert!(queued <= EVENT_QUEUE_BYTES / frame + 1, "{queued}");
    // Reading again lets the stream flow.
    wait(&host, |e| matches!(e, Event::Control { .. }).then_some(()));
    let _ = stop_tx.send(());
    client.join().unwrap();
}

/// Finding 8: a panicking sink closes the connection with a reason; PeerTx then fails.
#[test]
fn sink_panic_closes_the_connection() {
    let a = Net::start(cfg("a", Some(K), None), |_, _| {
        Box::new(|_: Bytes| -> () { panic!("sink bug") }) as Sink
    })
    .unwrap();
    let b = simple(cfg("b", Some(K), None));
    b.connect(a.local_addr().clone(), Join::Key);
    let (_, btx, _) = connected(&b);
    let (ca, atx, _) = connected(&a);
    btx.send(audio()).unwrap();
    let reason = wait(&a, |e| match e {
        Event::Closed { conn, reason } if conn == ca => Some(reason),
        _ => None,
    });
    assert_eq!(reason, "sink panicked");
    wait(&b, |e| matches!(e, Event::Closed { .. }).then_some(()));
    assert_eq!(atx.send(audio()), Err(SendError::Closed));
}

/// Finding 9: invalid frames are refused at the API without touching the connection; long
/// rosters are split.
#[test]
fn send_control_validates_and_splits() {
    let a = simple(cfg("a", Some(K), None));
    let b = simple(cfg("b", Some(K), None));
    b.connect(a.local_addr().clone(), Join::Key);
    let (cb, _, _) = connected(&b);
    let (ca, _, _) = connected(&a);
    let entry = |i: u8| PeerEntry {
        id: [i; 32],
        addrs: vec![],
        relay: None,
        name: format!("p{i}"),
        role: Role::Peer,
    };
    let long_name = PeerEntry {
        name: "x".repeat(65),
        ..entry(0)
    };
    assert!(matches!(
        a.send_control(ca, Frame::Roster(vec![long_name])),
        Err(ControlError::Invalid(_))
    ));
    assert_eq!(
        a.send_control(ca, Frame::Bye),
        Err(ControlError::NotAllowed)
    );
    assert_eq!(
        a.send_control(
            ca,
            Frame::Rekey(Rekey {
                key: [0; 32],
                grace_ms: 0,
                evicted: None,
            })
        ),
        Err(ControlError::NotAllowed)
    );
    assert_eq!(
        a.send_control(9999, Frame::AppOsc(vec![])),
        Err(ControlError::NotConnected)
    );
    let roster: Vec<PeerEntry> = (0..100).map(entry).collect();
    a.send_control(ca, Frame::Roster(roster.clone())).unwrap();
    let mut got = vec![];
    while got.len() < 100 {
        got.extend(wait(&b, |e| match e {
            Event::Control {
                conn,
                frame: Frame::Roster(r),
            } if conn == cb => Some(r),
            Event::Closed { .. } => panic!("closed"),
            _ => None,
        }));
    }
    assert_eq!(got, roster);
}

/// Finding 6: only the host we joined through may rotate our key.
#[test]
fn rekey_from_a_non_host_is_ignored() {
    // `m` joined via an invite naming `h` as host; a raw member (not h) sends REKEY.
    let h_id = [0x42; 32];
    let m = simple(Config {
        host: Some(h_id),
        ..cfg("m", Some(K), None)
    });
    let to = m.local_addr().clone();
    block_on(async {
        let ep = raw_endpoint(lo(1)).await;
        let mut rc = key_join(&ep, &to, &K).await.unwrap();
        wf(
            &mut rc.send,
            &Frame::Rekey(Rekey {
                key: [1; 32],
                grace_ms: 0,
                evicted: None,
            }),
        )
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
    });
    let ev = drain(&m, Duration::from_millis(200));
    assert!(!ev.contains(&"SessionKey".to_string()), "{ev:?}");
    // The old key still works.
    let b = simple(cfg("b", Some(K), None));
    b.connect(m.local_addr().clone(), Join::Key);
    connected(&b);
}

#[test]
fn connecting_to_ourselves_fails() {
    let a = simple(cfg("a", Some(K), None));
    a.connect(a.local_addr().clone(), Join::Key);
    assert_eq!(failed(&a).0, "cannot connect to ourselves");
}
