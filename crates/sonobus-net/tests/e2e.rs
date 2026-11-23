//! Transport + stream + adapt together: a sine streamed through `TxStream` → `PeerTx` (real
//! QUIC over loopback) → `Net` sink → lossy shim → `RxStream`, NACK/REPORT back the same way.
//! Wall-clock device callbacks, so the buffers are generous. Relays disabled, 127.x only.

mod common;

use std::time::{Duration, Instant};

use aoo::codec::PcmBitDepth;
use common::e2e::*;
use common::*;
use sonobus_net::auth::Key;
use sonobus_net::stream::{Device, Encoding, RxOptions, StreamFormat};
use sonobus_net::wire::Audio;
use sonobus_net::{Config, Event, Join, Net};

const K: Key = [0x44; 32];

fn dev(block: usize) -> Device {
    Device {
        sample_rate: 48000,
        block_size: block,
        channels: 2,
    }
}

fn pcm(depth: PcmBitDepth) -> StreamFormat {
    StreamFormat {
        sample_rate: 48000,
        block_size: 120,
        channels: 2,
        voice: false,
        encodings: vec![Encoding::Pcm(depth)],
    }
}

struct Run {
    host: Net,
    join: Net,
    h_in: Inbox,
    j_in: Inbox,
    src: Source,
    lis: Listener,
    start: Instant,
    /// The time both sides are stepped with, and when it was last advanced: see [`Run::run`].
    clock: Instant,
    stepped: Instant,
}

/// The most the clock advances in one step of [`Run::run`].
const MAX_STEP: Duration = Duration::from_millis(10);

fn setup(format: StreamFormat, opts: RxOptions, drop: DropFn, delay: Duration) -> Run {
    let (hs, h_in) = channel_sinks();
    let host = Net::start(cfg("host", Some(K), None), hs).unwrap();
    let (js, j_in) = channel_sinks();
    let join = Net::start(
        Config {
            bind_ip: lo(2).into(),
            ..cfg("join", Some(K), None)
        },
        js,
    )
    .unwrap();
    join.connect(host.local_addr().clone(), Join::Key);
    let (_, jtx, _) = connected(&join);
    let (hconn, htx, _) = connected(&host);
    let now = Instant::now();
    let mut src = Source::new(dev(120), format, sine);
    src.add(&host, hconn, htx, now);
    let mut lis = Listener::new(dev(128), opts, jtx);
    lis.record = true;
    lis.drop = drop;
    lis.delay = delay;
    Run {
        host,
        join,
        h_in,
        j_in,
        src,
        lis,
        start: now,
        clock: now,
        stepped: now,
    }
}

impl Run {
    /// Step both sides until `secs` after the start or until `stop` holds.
    ///
    /// Source and listener share this thread and its clock. When the thread stalled for
    /// longer than the listener's buffer (a busy CI runner), the real clock made the listener
    /// play past blocks the source had only just sent: 83 blocks "lost" with no loss at all.
    /// So the clock follows real time but advances by at most [`MAX_STEP`] per step: a stall
    /// makes the run take longer, it does not make time jump.
    fn run(&mut self, secs: f64, stop: impl Fn(&Run) -> bool) {
        let end = self.start + Duration::from_secs_f64(secs);
        while self.clock < end && !stop(self) {
            let real = Instant::now();
            self.clock += real.duration_since(self.stepped).min(MAX_STEP);
            self.stepped = real;
            let now = self.clock;
            self.src.step(now);
            while let Ok((c, d)) = self.h_in.try_recv() {
                self.src.on_datagram(c, now, &d);
            }
            while let Ok((_, d)) = self.j_in.try_recv() {
                self.lis.on_datagram(now, d);
            }
            while let Ok(e) = self.join.events().try_recv() {
                if let Event::Control { frame, .. } = e {
                    self.lis.on_control(&frame);
                }
            }
            while self.host.events().try_recv().is_ok() {}
            self.lis.step(now);
            std::thread::sleep(Duration::from_micros(500));
        }
    }

    /// Played samples that differ from the sine, both channels, over at least `min` frames.
    /// Silence is skipped: a wall-clock stall of this test thread longer than the buffer makes
    /// the receiver underrun and re-buffer (silence, `lost` unchanged), which shifts the delay
    /// but must not change the content. Channel 1 of the sine is never exactly 0.
    fn mismatches(&self, min: usize) -> usize {
        let out = &self.lis.out;
        let gaps = out[1].windows(2).filter(|w| w[0] != 0.0 && w[1] == 0.0).count();
        println!("{} silence gaps after the start", gaps.saturating_sub(1));
        assert_eq!(self.src.dropped_callbacks, 0);
        let played: Vec<usize> = (0..out[1].len()).filter(|&i| out[1][i] != 0.0).collect();
        assert!(played.len() >= min, "only {} frames played", played.len());
        // Blocks that arrive before the FORMAT frame cannot be decoded and are skipped, so the
        // stream may start a few blocks in: find where, once.
        let k = (0..48000)
            .find(|&k| (0..64).all(|n| out[1][played[n]] == sine(k + n, 1)))
            .expect("signal not found");
        println!("playback started at frame {k}");
        played
            .iter()
            .enumerate()
            .map(|(n, &i)| {
                usize::from(out[0][i] != sine(k + n, 0)) + usize::from(out[1][i] != sine(k + n, 1))
            })
            .sum()
    }
}

fn opts(buffer_ms: f32, nack_retries: u8) -> RxOptions {
    RxOptions {
        buffer_ms,
        dynamic_resampling: false,
        nack_retries,
        ..RxOptions::default()
    }
}

#[test]
fn pcm_bit_exact_without_loss() {
    let mut r = setup(
        pcm(PcmBitDepth::Float32),
        opts(100.0, 3),
        Box::new(|_| false),
        Duration::ZERO,
    );
    r.run(3.0, |_| false);
    assert_eq!(r.lis.count(r.start, lost), 0);
    assert_eq!(r.mismatches(48000 * 2), 0);
    let reports = &r.src.reports;
    assert!(
        !reports.is_empty() && reports.iter().all(|(_, x)| x.lost == 0),
        "{reports:?}"
    );
}

/// Bursts of 3 lost blocks (only their first transmission) every 100 blocks: NACK resends
/// repair every one in time, the output stays bit-exact.
#[test]
fn nack_repairs_bursts_bit_exact() {
    let mut seen = std::collections::HashSet::new();
    let drop = Box::new(move |a: &Audio| {
        let s = a.header.seq;
        s >= 200 && s % 100 < 3 && seen.insert(s)
    });
    let mut r = setup(
        pcm(PcmBitDepth::Float32),
        opts(150.0, 3),
        drop,
        Duration::from_millis(5),
    );
    r.run(4.0, |_| false);
    let (dropped, resent) = (r.lis.dropped, r.lis.count(r.start, resent));
    println!("dropped {dropped}, resent {resent}, nacks {}", r.lis.nacks);
    assert!(dropped >= 30, "{dropped}");
    assert_eq!(r.lis.count(r.start, lost), 0);
    // the last burst's resends may still be in flight at the end
    assert!(resent as usize + 3 >= dropped, "{resent} of {dropped}");
    assert!(r.src.nacks > 0);
    assert_eq!(r.mismatches(48000 * 3), 0);
}

/// Isolated random losses with NACK off: the receiver reports concealed blocks, `adapt` raises
/// the subscriber's redundancy, and from then on every loss is recovered from RED.
#[test]
fn report_drives_adapt_to_redundancy() {
    let mut rng = wire_rng(9);
    let mut prev = 0;
    let drop = Box::new(move |a: &Audio| {
        let s = a.header.seq;
        let d = s >= 50 && prev + 1 != s && rng() < 0.05;
        if d {
            prev = s;
        }
        d
    });
    let mut r = setup(
        pcm(PcmBitDepth::Int16),
        opts(40.0, 0),
        drop,
        Duration::ZERO,
    );
    r.run(15.0, |r| !r.src.changes.is_empty());
    let (_, d) = *r.src.changes.first().expect("adapt never changed the subscription");
    assert_eq!((d.level, d.redundancy), (0, 1));
    let (t0, switched) = (r.start, r.clock);
    let lost_before = r.lis.count(r.start, lost);
    assert!(lost_before > 0 && r.src.reports.iter().any(|(_, x)| x.lost > 0));
    r.start = switched;
    r.run(2.5, |_| false);
    let after = switched + Duration::from_millis(500);
    println!(
        "switch after {:?}: lost before {lost_before}, after: lost {} recovered {}, dropped total {}",
        switched - t0,
        r.lis.count(after, lost),
        r.lis.count(after, recovered),
        r.lis.dropped
    );
    assert!(r.lis.with_red > 0);
    assert!(r.lis.count(after, recovered) > 0);
    assert_eq!(r.lis.count(after, lost), 0);
}

fn wire_rng(seed: u64) -> impl FnMut() -> f64 + Send {
    let mut x = seed;
    move || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        (x >> 11) as f64 / (1u64 << 53) as f64
    }
}
