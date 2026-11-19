//! Shutdown after the peer vanished. Its own test binary so the thread count is not disturbed
//! by other tests.

use std::net::Ipv4Addr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use sonobus_net::{Config, Event, Join, Net, Sink};

static PANICS: AtomicUsize = AtomicUsize::new(0);

fn threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .map(|d| d.count())
        .unwrap_or(0)
}

fn start(name: &str) -> Net {
    let cfg = Config {
        bind_ip: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        session: "s".into(),
        key: Some([3; 32]),
        name: name.into(),
        relay: sonobus_net::RelayChoice::None,
        ..Config::default()
    };
    Net::start(cfg, |_, _| Box::new(|_| {}) as Sink).unwrap()
}

fn connected(n: &Net) -> sonobus_net::PeerTx {
    loop {
        match n.events().recv_timeout(Duration::from_secs(10)).unwrap() {
            Event::Connected { tx, .. } => return tx,
            Event::ConnectFailed { reason, .. } => panic!("{reason}"),
            _ => {}
        }
    }
}

#[test]
fn clean_shutdown_when_peer_vanishes() {
    let hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |i| {
        PANICS.fetch_add(1, Ordering::Relaxed);
        hook(i)
    }));
    let before = threads();
    let a = start("a");
    let b = start("b");
    b.connect(a.local_addr().clone(), Join::Key);
    let atx = connected(&a);
    connected(&b);
    // b crashes: no BYE, no QUIC close.
    b.kill();
    // a keeps sending into the void without error or blocking.
    for _ in 0..100 {
        atx.send(Bytes::from_static(b"x")).unwrap();
    }
    let t = Instant::now();
    drop(a);
    let took = t.elapsed();
    println!("shutdown with a vanished peer took {took:?}");
    assert!(took < Duration::from_secs(4), "{took:?}");
    // The PeerTx outliving the Net is harmless.
    let _ = atx.send(Bytes::from_static(b"x"));
    drop(atx);
    assert_eq!(threads(), before, "threads left behind");
    assert_eq!(PANICS.load(Ordering::Relaxed), 0);
}
