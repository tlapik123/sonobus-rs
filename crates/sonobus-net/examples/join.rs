//! Join a session by invite link (from the `host` example) and play its stream on a wall-clock
//! "device" (the audio is discarded). Prints path, latency and loss/repair stats every second.
//!
//! `cargo run --release -p sonobus-net --example join -- <invite link> [secs] [buffer ms] [relay]`
//!
//! `relay`: `none`, `n0` or a relay URL; by default the invite's relay URL, if any.

#[path = "../tests/common/mod.rs"]
mod common;

use std::net::Ipv4Addr;
use std::time::{Duration, Instant};

use common::e2e::*;
use sonobus_net::auth::Invite;
use sonobus_net::stream::{Device, RxEvent, RxOptions};
use sonobus_net::{Config, Event, Join, Net, PeerAddr, RelayChoice};

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let link = args
        .next()
        .expect("usage: join <invite link> [seconds] [buffer ms]");
    let secs: u64 = args.next().and_then(|a| a.parse().ok()).unwrap_or(20);
    let buffer_ms: f32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(40.0);
    let inv = Invite::parse(&link).map_err(|e| std::io::Error::other(e.to_string()))?;
    let relay = match (args.next().as_deref(), &inv.relay) {
        (None, Some(r)) => RelayChoice::Custom(vec![r.clone()]),
        (None | Some("none"), _) => RelayChoice::None,
        (Some("n0"), _) => RelayChoice::N0Default,
        (Some(url), _) => RelayChoice::Custom(vec![url.into()]),
    };
    let (sinks, inbox) = channel_sinks();
    let net = Net::start(
        Config {
            bind_ip: Ipv4Addr::UNSPECIFIED.into(),
            port: 0,
            session: inv.session.clone(),
            key: Some(inv.key()),
            host: Some(inv.host),
            name: "joiner".into(),
            relay,
            ..Config::default()
        },
        sinks,
    )?;
    net.connect(
        PeerAddr {
            id: inv.host,
            addrs: inv.addrs.clone(),
            relay: inv.relay.clone(),
        },
        Join::Key,
    );
    let (host_conn, tx) = loop {
        match net.events().recv_timeout(Duration::from_secs(10)) {
            Ok(Event::Connected { conn, peer, tx, .. }) if peer.id == inv.host => break (conn, tx),
            Ok(Event::ConnectFailed { reason, .. }) => return Err(std::io::Error::other(reason)),
            Ok(_) => {}
            Err(_) => return Err(std::io::Error::other("no answer from the host")),
        }
    };
    println!("joined '{}', path {:?}", inv.session, tx.path());
    let dev = Device {
        sample_rate: 48000,
        block_size: 128,
        channels: 2,
    };
    let opts = RxOptions {
        buffer_ms,
        ..RxOptions::default()
    };
    let mut lis = Listener::new(dev, opts, tx);
    let start = Instant::now();
    let mut last = start;
    while start.elapsed() < Duration::from_secs(secs) {
        let now = Instant::now();
        while let Ok((conn, d)) = inbox.try_recv() {
            if conn == host_conn {
                lis.on_datagram(now, d);
            }
        }
        while let Ok(e) = net.events().try_recv() {
            match e {
                Event::Control { conn, frame } if conn == host_conn => lis.on_control(&frame),
                Event::PathChanged { path, .. } => println!("path changed: {path:?}"),
                Event::Closed { conn, reason } if conn == host_conn => {
                    println!("host left: {reason}");
                    return Ok(());
                }
                _ => {}
            }
        }
        lis.step(now);
        if now.duration_since(last) >= Duration::from_secs(1) {
            let c = |f: fn(&RxEvent) -> Option<u32>| lis.count(last, f);
            let path = lis.host().path();
            let rtt = path.as_ref().map_or(0.0, |p| p.rtt.as_secs_f64() * 1e3);
            println!(
                "{} rtt {rtt:.1} ms, one-way ≈ {:.1} ms + buffer {:.1} ms | lost {} resent {} recovered {} | {} NACKs",
                if path.is_some_and(|p| p.direct) {
                    "direct "
                } else {
                    "relayed"
                },
                rtt / 2.0,
                lis.rx.fill_ms(),
                c(lost),
                c(resent),
                c(recovered),
                lis.nacks,
            );
            last = now;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    Ok(())
}
