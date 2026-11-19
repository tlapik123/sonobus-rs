//! Two endpoints on 127.0.0.1: KEY-authenticated connection, one second of 400 datagrams/s.

use std::net::Ipv4Addr;
use std::sync::mpsc::channel;
use std::time::{Duration, Instant};

use bytes::Bytes;
use sonobus_net::{Config, Event, Join, Net, Sink};

fn main() -> std::io::Result<()> {
    let cfg = |name: &str| Config {
        bind_ip: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        session: "loopback".into(),
        key: Some([7; 32]),
        name: name.into(),
        relay: sonobus_net::RelayChoice::None,
        ..Config::default()
    };
    let t0 = Instant::now();
    let (tx, rx) = channel();
    let a = Net::start(cfg("a"), move |_, _| {
        let tx = tx.clone();
        Box::new(move |d: Bytes| {
            let sent = u64::from_le_bytes(d[..8].try_into().unwrap());
            let _ = tx.send(t0.elapsed().as_micros() as u64 - sent);
        }) as Sink
    })?;
    let b = Net::start(cfg("b"), |_, _| Box::new(|_| {}) as Sink)?;
    b.connect(a.local_addr().clone(), Join::Key);
    let peer = loop {
        match b
            .events()
            .recv_timeout(Duration::from_secs(10))
            .expect("no event")
        {
            Event::Connected { tx, .. } => break tx,
            Event::ConnectFailed { reason, .. } => panic!("{reason}"),
            _ => {}
        }
    };
    for _ in 0..400 {
        let mut d = vec![0u8; 200];
        d[..8].copy_from_slice(&(t0.elapsed().as_micros() as u64).to_le_bytes());
        peer.send(d.into()).expect("send");
        std::thread::sleep(Duration::from_micros(2500));
    }
    let mut lat: Vec<u64> = rx.iter().take(400).collect();
    lat.sort();
    println!("400 datagrams: p50 {} us, max {} us", lat[200], lat[399]);
    Ok(())
}
