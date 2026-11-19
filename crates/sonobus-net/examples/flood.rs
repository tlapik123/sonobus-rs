//! Measurement: does a handshake flood (within the admission caps) delay datagram delivery on
//! the runtime thread? A legitimate peer streams 400 datagrams/s to a host for 5 s, once
//! alone and once while hostile clients (one source address, so at most
//! `MAX_PENDING_PER_IP` handshakes at a time) open connections as fast as they can and send
//! bad KEY proofs or PAKE HELLOs. Prints p50/p99/max one-way latency and the flood rate.
//! Loopback only, relays disabled.
//!
//! `cargo run --release -p sonobus-net --example flood [concurrency]`

#[path = "../tests/common/mod.rs"]
mod common;

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::channel;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use sonobus_net::auth;
use sonobus_net::wire::{self, AudioHeader, Datagram, Method};
use sonobus_net::{Config, Join, Net, Sink};

const K: [u8; 32] = [0x11; 32];

fn t0() -> Instant {
    static T0: OnceLock<Instant> = OnceLock::new();
    *T0.get_or_init(Instant::now)
}

fn run(flood: bool, concurrency: usize) {
    let (tx, rx) = channel::<u64>();
    let host = Net::start(cfg("host", Some(K), Some("pw")), move |_, _| {
        let tx = tx.clone();
        Box::new(move |d: Bytes| {
            let now = t0().elapsed().as_nanos() as u64;
            if let Ok(Datagram::Audio(au)) = Datagram::decode(&d) {
                let sent = u64::from_le_bytes(au.payload[..8].try_into().unwrap());
                let _ = tx.send(now - sent);
            }
        }) as Sink
    })
    .unwrap();
    let peer = simple(Config {
        bind_ip: lo(2).into(),
        ..cfg("peer", Some(K), None)
    });
    peer.connect(host.local_addr().clone(), Join::Key);
    let (_, ptx, _) = connected(&peer);

    let stop = Arc::new(AtomicBool::new(false));
    let handshakes = Arc::new(AtomicUsize::new(0));
    let flooder = flood.then(|| {
        let (to, stop, n) = (host.local_addr().clone(), stop.clone(), handshakes.clone());
        std::thread::spawn(move || {
            block_on(async move {
                let ep = raw_endpoint(lo(1)).await;
                let tag = auth::session_tag(SESSION);
                let mut tasks = vec![];
                for i in 0..concurrency {
                    let (ep, to, stop, n) = (ep.clone(), to.clone(), stop.clone(), n.clone());
                    tasks.push(tokio::spawn(async move {
                        let mut j = 0;
                        while !stop.load(Ordering::Relaxed) {
                            j += 1;
                            let Ok(c) = dial(&ep, &to).await else {
                                continue;
                            };
                            let Ok((mut s, mut r)) = c.open_bi().await else {
                                continue;
                            };
                            let f = if (i + j) % 2 == 0 {
                                hello(Method::Key, tag, vec![0; 32])
                            } else {
                                let e = exporter(&c, &tag);
                                let (_, x) = auth::PakeConnector::start(
                                    "guess",
                                    &e,
                                    ep.id().as_bytes(),
                                    &to.id,
                                );
                                hello(Method::Pake, tag, x)
                            };
                            let _ = wf(&mut s, &f).await;
                            let _ = rf(&mut r).await;
                            n.fetch_add(1, Ordering::Relaxed);
                            c.close(0u32.into(), b"");
                        }
                    }));
                }
                for t in tasks {
                    let _ = t.await;
                }
            })
        })
    });
    std::thread::sleep(Duration::from_millis(500));
    let h0 = handshakes.load(Ordering::Relaxed);
    const N: u32 = 2000;
    let start = Instant::now();
    let mut buf = Vec::new();
    let mut payload = [0u8; 200];
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
        ptx.send(Bytes::copy_from_slice(&buf)).unwrap();
    }
    let secs = start.elapsed().as_secs_f64();
    let rate = (handshakes.load(Ordering::Relaxed) - h0) as f64 / secs;
    let mut lat = vec![];
    while let Ok(l) = rx.recv_timeout(Duration::from_secs(1)) {
        lat.push(l);
    }
    stop.store(true, Ordering::Relaxed);
    if let Some(f) = flooder {
        f.join().unwrap();
    }
    lat.sort();
    let p = |q: f64| Duration::from_nanos(lat[((lat.len() - 1) as f64 * q) as usize]);
    println!(
        "flood={flood:5} concurrency={concurrency}: {}/{N} datagrams, latency p50 {:?} p99 {:?} max {:?}; hostile handshakes {rate:.0}/s",
        lat.len(),
        p(0.5),
        p(0.99),
        p(1.0)
    );
}

fn main() {
    let c = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(sonobus_net::rt::MAX_PENDING_PER_IP);
    run(false, c);
    run(true, c);
    run(false, c);
    run(true, c);
}
