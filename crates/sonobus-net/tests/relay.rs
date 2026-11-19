//! Relayed connections through an in-process relay server (plain http on 127.0.0.1; never
//! n0's relays): relay-only paths, the relay → direct upgrade, datagram latency over a relay.

mod common;

use std::net::{Ipv4Addr, Ipv6Addr};
use std::sync::mpsc::channel;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use iroh_relay::server::{RelayConfig, Server, ServerConfig};
use sonobus_net::auth::{self, Invite, Key};
use sonobus_net::wire::{self, AudioHeader, Datagram};
use sonobus_net::{Config, Event, Join, Net, PeerAddr, RelayChoice, Sink};

const K: Key = [0x66; 32];

/// A relay server on its own thread until dropped.
struct Relay {
    url: String,
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Relay {
    fn start() -> Relay {
        let (url_tx, url_rx) = channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel::<()>();
        let thread = std::thread::spawn(move || {
            block_on(async move {
                let mut c = ServerConfig::default();
                c.relay = Some(RelayConfig::new((Ipv4Addr::LOCALHOST, 0)));
                let server = Server::spawn(c).await.expect("relay server");
                let addr = server.http_addr().expect("http");
                url_tx.send(format!("http://{addr}")).unwrap();
                let _ = stop_rx.await;
                let _ = server.shutdown().await;
            })
        });
        Relay {
            url: url_rx.recv().unwrap(),
            stop: Some(stop),
            thread: Some(thread),
        }
    }

    fn choice(&self) -> RelayChoice {
        RelayChoice::Custom(vec![self.url.clone()])
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.stop.take().map(|s| s.send(()));
        let _ = self.thread.take().map(|t| t.join());
    }
}

/// Wait for the home relay connection; the URL then shows in `local_addr`.
fn online(n: &Net) -> PeerAddr {
    let end = Instant::now() + WAIT;
    loop {
        let a = n.local_addr();
        if a.relay.is_some() {
            return a;
        }
        assert!(Instant::now() < end, "never connected to the relay");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn t0() -> Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

/// No direct path is possible (the host listens on IPv6 loopback only, the joiner on IPv4
/// only): the connection runs over the relay, and stays there.
#[test]
fn relay_only_connection_and_latency() {
    let relay = Relay::start();
    let (tx, rx) = channel::<u64>();
    let host = Net::start(
        Config {
            bind_ip: Ipv6Addr::LOCALHOST.into(),
            relay: relay.choice(),
            ..cfg("host", Some(K), None)
        },
        move |_, _| {
            let tx = tx.clone();
            Box::new(move |d: Bytes| {
                let now = t0().elapsed().as_nanos() as u64;
                if let Ok(Datagram::Audio(a)) = Datagram::decode(&d) {
                    let sent = u64::from_le_bytes(a.payload[..8].try_into().unwrap());
                    let _ = tx.send(now - sent);
                }
            }) as Sink
        },
    )
    .unwrap();
    let join = simple(Config {
        bind_ip: lo(2).into(),
        relay: relay.choice(),
        ..cfg("join", Some(K), None)
    });
    let haddr = online(&host);
    assert_eq!(haddr.relay.as_deref(), Some(&*format!("{}/", relay.url)));
    // the runtime announces its relay once it is known
    let announced = std::iter::from_fn(|| host.events().recv_timeout(WAIT).ok())
        .find_map(|e| match e {
            Event::LocalRelay(r) => Some(r),
            _ => None,
        });
    assert_eq!(announced, haddr.relay);
    join.connect(haddr.clone(), Join::Key);
    let (_, jtx, hp) = connected(&join);
    // The peer info carries the relay URL the host announced.
    assert_eq!(hp.relay, haddr.relay);
    let p = jtx.path().unwrap();
    assert!(!p.direct && p.relay == haddr.relay && p.remote.is_none(), "{p:?}");

    const N: u32 = 400;
    let mut buf = Vec::new();
    let mut payload = [0u8; 200];
    let start = Instant::now();
    for seq in 0..N {
        let due = start + Duration::from_micros(2500) * seq;
        while Instant::now() < due {
            std::thread::sleep(Duration::from_micros(200));
        }
        payload[..8].copy_from_slice(&(t0().elapsed().as_nanos() as u64).to_le_bytes());
        let h = AudioHeader {
            seq,
            ..AudioHeader::default()
        };
        wire::encode_audio(&h, &[], &payload, &mut buf).unwrap();
        jtx.send(Bytes::copy_from_slice(&buf)).unwrap();
    }
    let mut lat = vec![];
    while let Ok(l) = rx.recv_timeout(Duration::from_secs(2)) {
        lat.push(l);
    }
    lat.sort();
    let q = |x: f64| Duration::from_nanos(lat[((lat.len() - 1) as f64 * x) as usize]);
    println!(
        "relayed (in-process relay, loopback): {}/{N} datagrams, one-way p50 {:?} p99 {:?} max {:?}",
        lat.len(),
        q(0.5),
        q(0.99),
        q(1.0)
    );
    assert!(lat.len() as u32 >= N * 99 / 100, "{}", lat.len());
    let p = jtx.path().unwrap();
    assert!(!p.direct, "{p:?}");
}

/// Dialed with the relay URL only (as from an invite without usable addresses): the
/// connection starts over the relay and iroh moves it to a direct path by itself, reported as
/// `PathChanged`.
#[test]
fn relayed_connection_upgrades_to_direct() {
    let relay = Relay::start();
    let host = simple(Config {
        relay: relay.choice(),
        ..cfg("host", Some(K), None)
    });
    let join = simple(Config {
        bind_ip: lo(3).into(),
        relay: relay.choice(),
        ..cfg("join", Some(K), None)
    });
    let haddr = online(&host);
    let inv = Invite {
        seed: auth::random_seed(),
        host: haddr.id,
        addrs: vec![],
        relay: haddr.relay.clone(),
        session: SESSION.into(),
        mode: Default::default(),
    };
    let inv = Invite::parse(&inv.to_url().unwrap()).unwrap();
    assert_eq!(inv.relay, haddr.relay);
    let t = Instant::now();
    join.connect(
        PeerAddr {
            id: inv.host,
            addrs: inv.addrs,
            relay: inv.relay,
        },
        Join::Key,
    );
    let (conn, jtx, _) = connected(&join);
    let first = jtx.path().unwrap();
    println!("connected after {:?} over {first:?}", t.elapsed());
    // Only the relay URL was known when dialing.
    assert!(!first.direct && first.relay == haddr.relay, "{first:?}");
    let ev = wait(&join, |e| match e {
        Event::PathChanged { conn: c, path } if path.direct => Some((c, path)),
        _ => None,
    });
    println!("direct after {:?}: {:?}", t.elapsed(), ev.1);
    let ev = {
        assert_eq!(ev.0, conn);
        ev.1
    };
    assert_eq!(ev.remote, Some(host.local_addr().addrs[0]));
    assert!(ev.relay.is_none());
}
