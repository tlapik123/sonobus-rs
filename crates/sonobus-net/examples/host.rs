//! Host a session and stream a test tone (Opus, 10 ms frames, main 96 kbit/s + low 32 kbit/s)
//! to everyone who joins with the printed invite link. NACKs are served, REPORTs drive
//! `adapt`; per-joiner stats are printed every second.
//!
//! `cargo run --release -p sonobus-net --example host -- [port] [bind ip] [session] [relay]`
//! then `cargo run --release -p sonobus-net --example join -- <invite link> [secs] [ms] [relay]`.
//!
//! `relay`: `none` (default: the joiner must reach a printed address directly), `n0` (iroh's
//! public relays) or a relay URL. The invite carries the host's home relay URL.

#[path = "../tests/common/mod.rs"]
mod common;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::time::{Duration, Instant};

use common::e2e::*;
use sonobus_net::auth::{self, Invite};
use sonobus_net::stream::{Device, Encoding, StreamFormat};
use sonobus_net::wire::Role;
use sonobus_net::{Config, Event, Net, RelayChoice, rt::DEFAULT_PORT};

fn tone(n: usize, c: usize) -> f32 {
    let f = if c == 0 { 440.0 } else { 660.0 };
    (std::f32::consts::TAU * f * (n % 48000) as f32 / 48000.0).sin() * 0.3
}

/// `none` (default), `n0`, or a relay URL.
pub fn relay_arg(a: Option<String>) -> RelayChoice {
    match a.as_deref() {
        None | Some("none") => RelayChoice::None,
        Some("n0") => RelayChoice::N0Default,
        Some(url) => RelayChoice::Custom(vec![url.into()]),
    }
}

fn main() -> std::io::Result<()> {
    let mut args = std::env::args().skip(1);
    let port = args.next().and_then(|a| a.parse().ok()).unwrap_or(DEFAULT_PORT);
    let bind_ip: IpAddr = args
        .next()
        .and_then(|a| a.parse().ok())
        .unwrap_or(Ipv4Addr::UNSPECIFIED.into());
    let session = args.next().unwrap_or_else(|| "demo".into());
    let relay = relay_arg(args.next());
    let relay_wanted = relay != RelayChoice::None;
    let seed = auth::random_seed();
    let (sinks, inbox) = channel_sinks();
    let net = Net::start(
        Config {
            bind_ip,
            port,
            session: session.clone(),
            key: Some(auth::key_from_seed(&seed)),
            name: "host".into(),
            role: Role::Host,
            relay,
            ..Config::default()
        },
        sinks,
    )?;
    // The home relay URL shows up once the relay connection is up.
    let t = Instant::now();
    while net.local_addr().relay.is_none() && relay_wanted && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    let local = net.local_addr();
    let invite = Invite {
        seed,
        host: local.id,
        addrs: local.addrs.clone(),
        relay: local.relay.clone(),
        session,
        mode: Default::default(),
    };
    println!("listening on {:?}, relay {:?}", local.addrs, local.relay);
    println!("invite: {}", invite.to_url().expect("invite"));

    let dev = Device {
        sample_rate: 48000,
        block_size: 120,
        channels: 2,
    };
    let opus = |bitrate| Encoding::Opus {
        bitrate,
        complexity: 10,
    };
    let format = StreamFormat {
        sample_rate: 48000,
        block_size: 480,
        channels: 2,
        voice: false,
        encodings: vec![opus(96_000), opus(32_000)],
    };
    let mut src = Source::new(dev, format, tone);
    let mut names = HashMap::new();
    let mut last_print = Instant::now();
    let mut seen_reports = 0;
    loop {
        let now = Instant::now();
        while let Ok(e) = net.events().try_recv() {
            match e {
                Event::Connected { conn, peer, tx, .. } => {
                    println!("+ {} ({conn}) path {:?}", peer.name, tx.path());
                    names.insert(conn, peer.name);
                    src.add(&net, conn, tx, now);
                }
                Event::Closed { conn, reason } => {
                    println!("- {} ({conn}): {reason}", names.remove(&conn).unwrap_or_default());
                    src.remove(conn);
                }
                Event::PathChanged { conn, path } => println!("  path ({conn}) {path:?}"),
                Event::IncomingFailed { count, last_reason } => {
                    println!("{count} incoming failed: {last_reason}")
                }
                _ => {}
            }
        }
        while let Ok((conn, d)) = inbox.try_recv() {
            src.on_datagram(conn, now, &d);
        }
        src.step(now);
        if now.duration_since(last_print) >= Duration::from_secs(1) {
            last_print = now;
            for (conn, r) in &src.reports[seen_reports..] {
                println!(
                    "{:>12} lost {:>3} late {:>3} jitter {:>3} ms buffer {:>3} ms  {:?}",
                    names.get(conn).map_or("?", |n| n.as_str()),
                    r.lost,
                    r.late,
                    r.jitter_ms,
                    r.buffer_ms,
                    src.decision(*conn).unwrap_or_default(),
                );
            }
            seen_reports = src.reports.len();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}
