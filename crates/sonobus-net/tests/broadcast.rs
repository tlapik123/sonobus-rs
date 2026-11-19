//! Broadcast ("star") sessions: listeners connect only to the host, never learn of each other,
//! cannot publish, and the host caps their number. Loopback only, relays disabled.

mod common;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use sonobus_net::auth::{Id, Key};
use sonobus_net::stream::{Encoding, StreamFormat};
use sonobus_net::wire::{self, Frame, Ping, Role, SessionMode, Subscribe};
use sonobus_net::{Config, Event, Join, Net, PeerAddr, Sink};

const K: Key = [0x55; 32];

fn listener(n: u8, host: Id) -> Net {
    simple(Config {
        bind_ip: lo(n).into(),
        mode: SessionMode::Broadcast,
        role: Role::Listener,
        host: Some(host),
        ..cfg(&format!("l{n}"), Some(K), None)
    })
}

/// Every id or address a listener's events reveal about other endpoints.
fn revealed(e: &Event) -> Vec<(Id, Vec<std::net::SocketAddr>)> {
    match e {
        Event::Connected { peer, roster, .. } => {
            let mut v = vec![(peer.id, peer.addrs.clone())];
            v.extend(roster.iter().map(|r| (r.id, r.addrs.clone())));
            v
        }
        Event::Control {
            frame: Frame::Roster(r),
            ..
        } => r.iter().map(|r| (r.id, r.addrs.clone())).collect(),
        _ => vec![],
    }
}

#[test]
fn listeners_stay_private_cannot_publish_and_are_capped() {
    // Host sink: kinds of the datagrams that reach it.
    let kinds = Arc::new(Mutex::new(Vec::<u8>::new()));
    let k2 = kinds.clone();
    let host = Net::start(
        Config {
            role: Role::Host,
            mode: SessionMode::Broadcast,
            max_listeners: Some(4),
            ..cfg("host", Some(K), None)
        },
        move |_, _| {
            let k = k2.clone();
            Box::new(move |d: Bytes| k.lock().unwrap().push(d[0] & 0x0f)) as Sink
        },
    )
    .unwrap();
    let hid = host.local_addr().id;
    let haddr = host.local_addr().clone();

    // A raw listener joins first and watches every frame the host sends it.
    let (raw_tx, raw_rx) = std::sync::mpsc::channel::<Vec<Frame>>();
    let to = haddr.clone();
    let raw = std::thread::spawn(move || {
        block_on(async move {
            let ep = raw_endpoint(lo(20)).await;
            let mut c = key_join(&ep, &to, &K).await.unwrap();
            let mut got = vec![Frame::Roster(c.welcome.roster.clone())];
            let end = Instant::now() + Duration::from_secs(3);
            while let Ok(Ok(f)) =
                tokio::time::timeout_at(end.into(), rf(&mut c.recv)).await
            {
                got.push(f);
            }
            raw_tx.send(got).unwrap();
            c.conn.close(0u32.into(), b"");
        })
    });
    wait(&host, |e| matches!(e, Event::Connected { .. }).then_some(()));

    let ls: Vec<Net> = (2..5).map(|n| listener(n, hid)).collect();
    let mut events: Vec<Vec<Event>> = vec![];
    for l in &ls {
        l.connect(haddr.clone(), Join::Key);
        let mut ev = vec![];
        wait(l, |e| {
            let done = matches!(e, Event::Connected { .. });
            if let Event::ConnectFailed { reason, .. } = &e {
                panic!("{reason}");
            }
            ev.push(e);
            done.then_some(())
        });
        events.push(ev);
    }
    // The cap: a fifth listener is refused, after authenticating, with "session full".
    let fifth = listener(5, hid);
    fifth.connect(haddr.clone(), Join::Key);
    assert_eq!(failed(&fifth).0, "session full");

    // Listeners try to publish: FORMAT (refused), AUDIO datagrams (dropped), and send a
    // SUBSCRIBE and PINGs, which must arrive.
    let l0 = &ls[0];
    let (conn, tx) = events[0]
        .iter()
        .find_map(|e| match e {
            Event::Connected { conn, tx, .. } => Some((*conn, tx.clone())),
            _ => None,
        })
        .unwrap();
    let fmt = StreamFormat {
        sample_rate: 48000,
        block_size: 120,
        channels: 1,
        voice: false,
        encodings: vec![Encoding::Pcm(aoo::codec::PcmBitDepth::Int16)],
    };
    l0.send_control(conn, Frame::Format(fmt.to_wire(0, 0)))
        .unwrap();
    for _ in 0..20 {
        tx.send(audio()).unwrap();
    }
    let mut ping = vec![];
    Ping { id: 1, t1: 2 }.encode(&mut ping);
    for _ in 0..5 {
        tx.send(Bytes::from(ping.clone())).unwrap();
    }
    let sub = Frame::Subscribe(Subscribe {
        stream: 0,
        encoding: 0,
        redundancy: 0,
    });
    l0.send_control(conn, sub.clone()).unwrap();
    wait(&host, |e| match e {
        Event::Control {
            frame: f @ Frame::Format(_),
            ..
        } => panic!("listener FORMAT delivered: {f:?}"),
        Event::Control { frame, .. } if frame == sub => Some(()),
        _ => None,
    });
    let end = Instant::now() + WAIT;
    while kinds.lock().unwrap().len() < 5 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(200));
    let k = kinds.lock().unwrap().clone();
    assert!(k.iter().all(|&k| k == wire::KIND_PING) && k.len() == 5, "{k:?}");

    // Nobody learned anyone but the host, by event or by frame.
    std::thread::sleep(Duration::from_millis(1500));
    for (i, l) in ls.iter().enumerate() {
        while let Ok(e) = l.events().try_recv() {
            events[i].push(e);
        }
        for e in &events[i] {
            for (id, addrs) in revealed(e) {
                assert_eq!(id, hid, "listener {i} learned {id:?}");
                assert_eq!(addrs, haddr.addrs.clone(), "listener {i}");
            }
        }
    }
    raw.join().unwrap();
    let frames = raw_rx.recv().unwrap();
    // Only the WELCOME roster, naming the host alone.
    match &frames[..] {
        [Frame::Roster(r)] => assert_eq!(r.iter().map(|e| e.id).collect::<Vec<_>>(), [hid]),
        f => panic!("raw listener got {f:?}"),
    }

    // Listeners accept no incoming connections, so their addresses would not help anyway.
    let other = &ls[1];
    let l2 = PeerAddr {
        id: ls[2].local_addr().id,
        addrs: ls[2].local_addr().addrs.clone(),
        relay: None,
    };
    other.connect(l2, Join::Key);
    failed(other);
}
