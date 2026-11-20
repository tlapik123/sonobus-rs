//! Unsynchronised playout against the defects of its first version (e437ab1) and of the first
//! fix of them:
//! - the audio half took the sender's new rate after filling its resampler, so a rate estimate
//!   rising between callbacks left it a frame short with the ring full: an underrun that added
//!   a callback of latency for good (44.1 kHz, a faster sender clock);
//! - a hole was concealed as soon as the ring was down to a block and two device callbacks,
//!   above the jitter buffer with a device callback large against it: nearly every reordered
//!   block was lost, hidden by the latency those underruns added;
//! - raising the buffer for late blocks without a bound let latency creep up for good under a
//!   heavy-tailed jitter; a hole concealed in the call that revealed it was never asked for
//!   again (resends useless); a lateness that recurs beyond the jitter buffer was lost for
//!   good; the raise was not reported.
//!
//! Virtual clocks; the input is a per-sample code, so every output sample tells its latency.
//! The first version's numbers, measured on the same cells, are with each test; bounds are
//! about twice the worst seed measured here.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use aoo::codec::PcmBitDepth;
use bytes::Bytes;
use sonobus_net::stream::{
    Device, Encoding, RxEvent, RxOptions, RxStream, StreamFormat, TxOptions, TxStream,
};
use sonobus_net::wire::{Audio, Datagram};

const CODE: f64 = 131072.0;

fn code(n: usize) -> f32 {
    (0.05 + 0.9 * (n as f64 % CODE) / CODE) as f32
}

#[derive(Debug, Clone, Copy)]
struct Fmt {
    sr: u32,
    block: u16,
    dev: usize,
    buffer_ms: f32,
}

const fn fmt(sr: u32, block: u16, dev: usize, buffer_ms: f32) -> Fmt {
    Fmt {
        sr,
        block,
        dev,
        buffer_ms,
    }
}

/// One-way delay of block `k` (sent at `t` s) from a uniform random `r`, s; `None`: lost.
type Link = Box<dyn Fn(f64, u64, f64) -> Option<f64>>;

/// 40 ms and `jitter` s of uniform jitter.
fn jittery(jitter: f64) -> Link {
    Box::new(move |_, _, r| Some(0.040 + jitter * r))
}

#[derive(Default)]
struct Sc {
    secs: f64,
    /// Sender clock against ours.
    speed: f64,
    /// Round trip for NACKs (answered by the sender, the resend crossing the link).
    rtt: Option<f64>,
    /// Every 10th block comes with an empty payload.
    empty_late: bool,
    /// Buffering restarts then (s).
    flush_at: Option<f64>,
}

#[derive(Debug, Default)]
struct Outcome {
    /// Callbacks that stopped playing; those with at least a block in the ring.
    underruns: u32,
    full: u32,
    lost: u32,
    /// Mean latency (ms) of what played from 5 s on, and in the last minute.
    latency: f64,
    last_min: f64,
    /// NACK entries sent; the most the buffer was raised (ms); `buffer_ms()` always the target
    /// and the raise.
    nacks: u32,
    lift_ms: f32,
    buffer_ok: bool,
    /// The raise right after the restart.
    lift_after_flush: Option<f32>,
    /// Blocks that came after they were concealed; raises (the raise grew); the fastest
    /// resampling offset (ppm; unsynced playout never speeds up).
    late: u32,
    raises: u32,
    max_steer: i32,
    /// The raise when the audio half ran dry, and when it played again (after buffering
    /// afresh), the first time.
    lift_at_underrun: Option<(f32, f32)>,
}

fn run(f: Fmt, link: &Link, sc: &Sc, seed: u64) -> Outcome {
    let device = |block| Device {
        sample_rate: f.sr,
        block_size: block,
        channels: 1,
    };
    let format = StreamFormat {
        sample_rate: f.sr,
        block_size: f.block,
        channels: 1,
        voice: false,
        encodings: vec![Encoding::Pcm(PcmBitDepth::Float32)],
    };
    let bl = usize::from(f.block);
    let (mut tx, mut tx_audio) =
        TxStream::new(0, device(bl), format, TxOptions::default()).unwrap();
    let fw = tx.take_format_change().unwrap();
    assert!(tx.subscribe(0, 0, 0));
    let opts = RxOptions {
        buffer_ms: f.buffer_ms,
        ..RxOptions::default()
    };
    let (mut rx, mut audio) = RxStream::new(0, device(f.dev), opts);
    rx.set_format(&fw).unwrap();
    let sr = f64::from(f.sr);
    let rtt = sc.rtt.unwrap_or(10.0);
    let base = Instant::now();
    let mut q: BTreeMap<(u64, u64), Bytes> = BTreeMap::new();
    let mut back: BTreeMap<(u64, u64), Bytes> = BTreeMap::new();
    let (mut n, mut rng) = (
        0u64,
        0x9e37_79b9_7f4a_7c15u64 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d),
    );
    let mut rand = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 11) as f64 / (1u64 << 53) as f64
    };
    let (mut t_src, mut t_cb, mut frames, mut k) = (0.0f64, 0.0007f64, 0usize, 0u64);
    let mut buf = vec![0.0f32; f.dev];
    let mut lift = 0.0f32;
    let mut o = Outcome {
        buffer_ok: true,
        ..Outcome::default()
    };
    let (mut playing, mut lat_sum, mut lat_n, mut end_sum, mut end_n) =
        (false, 0.0, 0usize, 0.0, 0usize);
    // a sample counts once 16 in a row agree (not a concealed block or its fades)
    let (mut run_lat, mut run_len) = (f64::NAN, 0usize);
    let in_rate = sr * if sc.speed > 0.0 { sc.speed } else { 1.0 };
    let block_ms = f64::from(f.block) * 1e3 / sr;
    loop {
        let first = |q: &BTreeMap<(u64, u64), Bytes>| {
            q.first_key_value()
                .map_or(f64::MAX, |((t, _), _)| *t as f64 / 1e9)
        };
        let (t_net, t_back) = (first(&q), first(&back));
        let t = t_net.min(t_back).min(t_src).min(t_cb);
        if t >= sc.secs {
            break;
        }
        let now = base + Duration::from_secs_f64(t);
        if sc.flush_at.is_some_and(|at| t >= at) && o.lift_after_flush.is_none() {
            rx.flush();
            o.lift_after_flush = Some(rx.lift_ms());
        }
        if t == t_src {
            let input: Vec<f32> = (frames..frames + bl).map(code).collect();
            assert!(tx_audio.process(&[&input], now));
            frames += bl;
            let mut sent = Vec::new();
            tx.pump(|_, b| sent.push(b));
            for b in sent {
                if let Some(d) = link(t, k, rand()) {
                    n += 1;
                    q.insert((((t + d) * 1e9) as u64, n), b);
                }
            }
            k += 1;
            t_src += bl as f64 / in_rate;
        } else if t == t_cb {
            let played = audio.process(&mut [&mut buf[..]], now);
            if !playing && played && o.underruns == 1 {
                o.lift_at_underrun = o.lift_at_underrun.map(|(a, _)| (a, rx.lift_ms()));
            }
            if playing && !played {
                o.underruns += 1;
                if o.underruns == 1 {
                    o.lift_at_underrun = Some((rx.lift_ms(), f32::NAN));
                }
                if f64::from(rx.fill_ms()) >= block_ms - 1e-6 {
                    o.full += 1;
                }
            }
            playing = played;
            for (j, &v) in buf.iter().enumerate() {
                let t_out = t + j as f64 / sr;
                if !(0.04..0.96).contains(&v) {
                    run_len = 0;
                    continue;
                }
                let lat = (t_out * in_rate - f64::from(v - 0.05) / 0.9 * CODE).rem_euclid(CODE)
                    / in_rate
                    * 1e3;
                run_len = if run_len > 0 && (lat - run_lat).abs() < 3e3 / sr {
                    run_len + 1
                } else {
                    1
                };
                run_lat = lat;
                if run_len >= 16 && t_out >= 5.0 {
                    lat_sum += lat;
                    lat_n += 1;
                    if t_out >= sc.secs - 60.0 {
                        end_sum += lat;
                        end_n += 1;
                    }
                }
            }
            rx.tick(now, Duration::from_secs_f64(rtt));
            while let Some(d) = rx.poll_transmit(now) {
                if let Ok(Datagram::Nack(nack)) = Datagram::decode(&d) {
                    o.nacks += nack.iter().count() as u32;
                    if sc.rtt.is_some() {
                        n += 1;
                        back.insert((((t + rtt / 2.0) * 1e9) as u64, n), d);
                    }
                }
            }
            while let Some(e) = rx.poll_event() {
                match e {
                    RxEvent::BlocksLost { count } => o.lost += count,
                    RxEvent::BlocksLate { count } => o.late += count,
                    _ => {}
                }
            }
            o.raises += u32::from(rx.lift_ms() > lift);
            lift = rx.lift_ms();
            o.max_steer = o.max_steer.max(rx.playout_steer_ppm());
            o.lift_ms = o.lift_ms.max(lift);
            o.buffer_ok &= (rx.buffer_ms() - f.buffer_ms - rx.lift_ms()).abs() < 1e-3;
            t_cb += f.dev as f64 / sr;
        } else if t == t_back {
            let (_, d) = back.pop_first().unwrap();
            let Ok(Datagram::Nack(nack)) = Datagram::decode(&d) else {
                unreachable!()
            };
            let mut resent = Vec::new();
            tx.handle_nack_at(now, 0, &nack, |_, b| resent.push(b));
            for b in resent {
                // a resend crosses the link one way: half the round trip
                n += 1;
                q.insert((((t + rtt / 2.0) * 1e9) as u64, n), b);
            }
        } else {
            let (_, b) = q.pop_first().unwrap();
            let Ok(Datagram::Audio(a)) = Datagram::decode(&b) else {
                unreachable!()
            };
            // a late copy of a block already played: garbage instead
            let late = sc.empty_late && a.header.seq % 10 == 9;
            let a = if late { Audio { payload: &[], ..a } } else { a };
            rx.handle_audio(now, &a);
        }
    }
    o.latency = lat_sum / lat_n.max(1) as f64;
    o.last_min = end_sum / end_n.max(1) as f64;
    o
}

fn sc(secs: f64) -> Sc {
    Sc {
        secs,
        ..Sc::default()
    }
}

/// Per seed 1..=`seeds`: no underrun with a full ring, at most `underruns` underruns and
/// `lost` lost blocks, mean latency at most `latency` ms; `buffer_ms()` the target and the
/// raise.
#[allow(clippy::too_many_arguments)]
fn check(
    f: Fmt,
    link: Link,
    sc: Sc,
    seeds: u64,
    underruns: u32,
    lost: u32,
    latency: f64,
) -> Vec<Outcome> {
    (1..=seeds)
        .map(|seed| {
            let o = run(f, &link, &sc, seed);
            eprintln!("{f:?} seed {seed}: {o:?}");
            assert_eq!(o.full, 0, "underrun with a full ring: {o:?}");
            assert!(o.underruns <= underruns, "underruns: {o:?}");
            assert!(o.lost <= lost, "lost: {o:?}");
            assert!(o.latency <= latency, "latency: {o:?}");
            assert!(
                o.buffer_ok,
                "buffer_ms() is not the target and the raise: {o:?}"
            );
            o
        })
        .collect()
}

// The first version's numbers below are per seed over 20 s; "rate first alone" and
// "concealment alone" are that version with only the one fix.

/// 44.1 kHz (the rate ratio below 1, the estimate wobbling): first version 6 underruns with a
/// full ring per seed and 140 ms; rate first alone 119-132 lost.
#[test]
fn no_false_underruns_at_44k1() {
    check(
        fmt(44100, 256, 512, 20.0),
        jittery(0.008),
        sc(20.0),
        3,
        0,
        12,
        95.0,
    );
}

/// A sender 200 ppm fast, 8 ms jitter, device callback 4 blocks: first version 1681-1706 lost,
/// rate first alone 1706-1843.
#[test]
fn fast_sender_jitter_and_large_callback() {
    let s = Sc {
        speed: 1.0002,
        ..sc(20.0)
    };
    check(
        fmt(48000, 128, 512, 20.0),
        jittery(0.008),
        s,
        3,
        0,
        82,
        82.0,
    );
}

/// Margin above the jitter buffer (a 1024 callback at 96 kHz): first version an underrun with
/// a full ring, 204-218 lost and 84 ms; rate first alone 7372-7425 lost; concealment alone the
/// underrun.
#[test]
fn reordered_blocks_with_a_large_callback() {
    check(
        fmt(96000, 64, 1024, 20.0),
        jittery(0.002),
        sc(20.0),
        3,
        0,
        22,
        70.0,
    );
}

/// 10 ms buffer, callback 4 blocks: first version 4 underruns with a full ring and 64.6 ms; rate
/// first alone 833-883 lost; concealment alone the underruns.
#[test]
fn reordered_blocks_with_a_small_buffer() {
    check(
        fmt(48000, 64, 256, 10.0),
        jittery(0.002),
        sc(20.0),
        3,
        0,
        6,
        60.0,
    );
}

/// 5 ms buffer, 8 ms jitter: first version 2077-2133 lost (53.7 ms; an underrun with a full
/// ring in one seed); rate first alone the same losses. Not losing them costs a few ms.
#[test]
fn jitter_beyond_a_small_buffer() {
    check(
        fmt(48000, 120, 128, 5.0),
        jittery(0.008),
        sc(20.0),
        3,
        1,
        6,
        60.0,
    );
}

/// Heavy-tailed jitter (Pareto, alpha 1.5, 1 ms scale: about 1 % over 20 ms), 10 minutes: the
/// raise stays under its cap (twice the concealment margin, 15.7 ms here) instead of creeping
/// up (the first fix: 78.5 ms in the first minute, 91.1 in the tenth). First version 66.2 ms
/// and 3803-3816 lost.
#[test]
fn heavy_tail_latency_stays_bounded() {
    let pareto: Link = Box::new(|_, _, r| {
        Some(0.040 + (0.001 * ((1.0 - r).max(1e-9).powf(-1.0 / 1.5) - 1.0)).min(0.3))
    });
    let o = check(
        fmt(48000, 120, 128, 20.0),
        pareto,
        sc(600.0),
        1,
        0,
        2600,
        78.0,
    );
    assert!(
        o[0].last_min <= 78.0 && o[0].lift_ms <= 15.7 && o[0].max_steer == 0,
        "{:?}",
        o[0]
    );
}

/// 1 % loss, resends on, a 5 ms link (10 ms round trip), a 20 ms buffer and a 512 callback at
/// 44.1 kHz: the buffer is too short for a resend to beat the concealment margin until it is
/// raised; a hole concealed when it shows is asked for still, and its resend raises the buffer.
/// First version 0-1 lost (at 100 ms, 112-142 NACKs); the first fix 96-115 lost, no NACK.
#[test]
fn resends_come_in_time() {
    let lossy: Link = Box::new(|_, _, r| (r >= 0.01).then_some(0.005 + 0.002 * (r - 0.01) / 0.99));
    let s = Sc {
        rtt: Some(0.010),
        ..sc(60.0)
    };
    for o in check(fmt(44100, 256, 512, 20.0), lossy, s, 4, 1, 14, 80.0) {
        // about one or two per lost datagram (some 100 of them), no storm
        assert!(o.nacks <= 250, "{o:?}");
    }
}

/// Every 10th block 15 ms late on a 20 ms buffer (44.1 kHz / 512 callback): concealed (the
/// margin), but here by its playout time: raised at once (by at most the cap), they play.
/// First version 2 lost (at 134 ms); the first fix 1032.
#[test]
fn lateness_by_the_playout_time_raises_the_buffer_at_once() {
    let late: Link =
        Box::new(|_, k, r| Some(0.040 + 0.002 * r + if k % 10 == 9 { 0.015 } else { 0.0 }));
    for o in check(fmt(44100, 256, 512, 20.0), late, sc(60.0), 3, 0, 10, 100.0) {
        assert!(o.lift_ms > 0.0 && o.lift_ms <= 58.1, "{o:?}");
    }
}

/// Every 10th block 10 ms late on a 10 ms buffer (48 kHz / 120 / 128): they come after their
/// playout time, again and again; only the rule for a recurring lateness raises the buffer
/// (once, by what they lacked). Without it: 1206 lost a minute.
#[test]
fn recurring_lateness_raises_the_buffer_once() {
    let late: Link =
        Box::new(|_, k, r| Some(0.040 + 0.002 * r + if k % 10 == 9 { 0.010 } else { 0.0 }));
    for o in check(
        fmt(48000, 120, 128, 10.0),
        late,
        sc(60.0),
        3,
        0,
        1000,
        100.0,
    ) {
        assert!(o.lift_ms > 0.0 && o.max_steer == 0, "{o:?}");
    }
}

/// Every 10th block 15 ms late on a 20 ms buffer with the late copies garbled (empty payload):
/// nothing taken, nothing raised.
#[test]
fn only_data_taken_raises_the_buffer() {
    let late: Link =
        Box::new(|_, k, r| Some(0.040 + 0.002 * r + if k % 10 == 9 { 0.015 } else { 0.0 }));
    let s = Sc {
        empty_late: true,
        ..sc(20.0)
    };
    let o = run(fmt(44100, 256, 512, 20.0), &late, &s, 1);
    assert_eq!(o.lift_ms, 0.0, "{o:?}");
    assert!(o.lost > 300, "{o:?}");
}

/// A raise is a gap: reported as a lost block, and in `buffer_ms()`; restarting buffering
/// drops it.
#[test]
fn a_raise_is_reported() {
    let s = Sc {
        flush_at: Some(4.0),
        ..sc(5.0)
    };
    let o = run(fmt(96000, 64, 1024, 20.0), &jittery(0.002), &s, 1);
    // nothing is lost on this link: every block concealed comes late after all; each raise
    // is a lost block more
    assert!(o.raises >= 1 && o.lost >= o.late + o.raises, "{o:?}");
    assert!(o.buffer_ok, "{o:?}");
    assert_eq!(o.lift_after_flush, Some(0.0), "{o:?}");
}

/// Running dry restarts buffering from the jitter target: the raise is gone.
#[test]
fn an_underrun_drops_the_raise() {
    // 2 ms jitter for 3 s (raises), then none; nothing for 100 ms at 4 s
    let outage: Link = Box::new(|t, _, r| {
        let j = if t < 3.0 { 0.002 * r } else { 0.0 };
        Some(
            0.040
                + j
                + if (4.0..4.1).contains(&t) {
                    4.1 - t
                } else {
                    0.0
                },
        )
    });
    let o = run(fmt(96000, 64, 1024, 20.0), &outage, &sc(5.0), 1);
    let (before, after) = o.lift_at_underrun.expect("an underrun");
    assert!(before > 0.0 && after == 0.0, "{o:?}");
}

/// After a raise, a clean link for two minutes: the raise stays, and unsynced playout never
/// resamples faster than the sender's rate to give it back.
#[test]
fn a_raise_stays() {
    let calming: Link = Box::new(|t, _, r| Some(0.040 + if t < 10.0 { 0.008 } else { 0.001 } * r));
    let o = run(fmt(48000, 128, 512, 20.0), &calming, &sc(130.0), 1);
    assert!(o.lift_ms > 0.0 && o.max_steer == 0, "{o:?}");
    assert!(o.last_min >= o.latency - 0.5, "{o:?}");
}
