//! Synchronised playout at the stream level: one TxStream, several RxStreams behind links of
//! different latency, virtual clocks. With a playout target every receiver plays each input
//! sample at its capture time plus the delay; without, each one as soon as its buffer fills.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use aoo::codec::PcmBitDepth;
use bytes::Bytes;
use sonobus_net::stream::{
    Device, Encoding, Playout, RxAudio, RxEvent, RxOptions, RxStream, StreamFormat, TxAudio,
    TxOptions, TxStream,
};
use sonobus_net::wire::Datagram;

const SR: u32 = 48000;
/// An impulse every quarter second.
const PERIOD: usize = 12000;

struct Rx {
    rx: RxStream,
    audio: RxAudio,
    latency: f64,
    t: f64,
    out: Vec<f32>,
    /// Underruns and lost (concealed or dropped) blocks.
    stops: u32,
    lost: u32,
}

struct Sim {
    tx: TxStream,
    tx_audio: TxAudio,
    rxs: Vec<Rx>,
    q: BTreeMap<(u64, u64), (usize, Bytes)>,
    n: u64,
    rng: u64,
    jitter: f64,
    t_src: f64,
    /// Sender clock speed against the receivers' (1.0002 = 200 ppm fast).
    speed: f64,
    in_frames: usize,
    base: Instant,
    /// Playout delay; `None` = no sync.
    delay: Option<f64>,
    /// Added to every due time (a hostile or broken sender's target), s.
    skew: f64,
    /// From this time on (s), every link is this much slower (s).
    step: (f64, f64),
    /// Nothing arrives in this window (s); what was sent meanwhile arrives at its end.
    stall: (f64, f64),
}

impl Sim {
    fn new(latencies: &[f64], delay: Option<f64>, speed: f64) -> Self {
        let dev = |block| Device {
            sample_rate: SR,
            block_size: block,
            channels: 1,
        };
        let format = StreamFormat {
            sample_rate: SR,
            block_size: 120,
            channels: 1,
            voice: false,
            encodings: vec![Encoding::Pcm(PcmBitDepth::Float32)],
        };
        let (mut tx, tx_audio) = TxStream::new(0, dev(120), format, TxOptions::default()).unwrap();
        let f = tx.take_format_change().unwrap();
        let rxs = latencies
            .iter()
            .enumerate()
            .map(|(i, &latency)| {
                assert!(tx.subscribe(i as u64, 0, 0));
                let opts = RxOptions {
                    buffer_ms: 20.0,
                    ..RxOptions::default()
                };
                let (mut rx, audio) = RxStream::new(0, dev(128), opts);
                rx.set_format(&f).unwrap();
                // device callbacks at an arbitrary phase per receiver
                let t = 0.0007 + 0.0011 * i as f64;
                Rx {
                    rx,
                    audio,
                    latency,
                    t,
                    out: Vec::new(),
                    stops: 0,
                    lost: 0,
                }
            })
            .collect();
        Self {
            tx,
            tx_audio,
            rxs,
            q: BTreeMap::new(),
            n: 0,
            rng: 0x9e37_79b9_7f4a_7c15,
            jitter: 0.002,
            t_src: 0.0,
            speed,
            in_frames: 0,
            base: Instant::now(),
            delay,
            skew: 0.0,
            step: (f64::MAX, 0.0),
            stall: (f64::MAX, f64::MAX),
        }
    }

    fn now(&self, t: f64) -> Instant {
        self.base + Duration::from_secs_f64(t)
    }

    fn rand(&mut self) -> f64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        (self.rng >> 11) as f64 / (1u64 << 53) as f64
    }

    fn run(&mut self, until: f64) {
        loop {
            let t_net = self
                .q
                .first_key_value()
                .map_or(f64::MAX, |((t, _), _)| *t as f64 / 1e9);
            let (ri, t_rx) = self
                .rxs
                .iter()
                .enumerate()
                .map(|(i, r)| (i, r.t))
                .fold((0, f64::MAX), |a, b| if b.1 < a.1 { b } else { a });
            let t = t_net.min(self.t_src).min(t_rx);
            if t >= until {
                return;
            }
            let now = self.now(t);
            if t == self.t_src {
                let block: Vec<f32> = (self.in_frames..self.in_frames + 120)
                    .map(|n| if n % PERIOD == 0 { 1.0 } else { 0.0 })
                    .collect();
                assert!(self.tx_audio.process(&[&block], now));
                self.in_frames += 120;
                let mut sent = Vec::new();
                self.tx.pump(|id, b| sent.push((id as usize, b)));
                for (i, b) in sent {
                    let step = if t >= self.step.0 { self.step.1 } else { 0.0 };
                    let t_out = if (self.stall.0..self.stall.1).contains(&t) {
                        self.stall.1
                    } else {
                        t
                    };
                    let at = t_out + self.rxs[i].latency + step + self.rand() * self.jitter;
                    self.n += 1;
                    self.q.insert(((at * 1e9) as u64, self.n), (i, b));
                }
                // the host's anchor, on the same clock as the receivers here
                if self.delay.is_none() {
                    for r in &mut self.rxs {
                        r.rx.set_playout(None);
                    }
                }
                if let (Some(d), Some((generation, seq, at))) =
                    (self.delay, self.tx.playout_anchor())
                {
                    let due = at + Duration::from_secs_f64(d);
                    let skew = Duration::from_secs_f64(self.skew.abs());
                    let due = if self.skew >= 0.0 {
                        due + skew
                    } else {
                        due.checked_sub(skew).unwrap_or(self.base)
                    };
                    for r in &mut self.rxs {
                        r.rx.set_playout(Some(Playout {
                            generation,
                            seq,
                            due,
                            span: Duration::from_secs_f64(d),
                        }));
                    }
                }
                self.t_src += 120.0 / (f64::from(SR) * self.speed);
            } else if t == t_rx {
                let r = &mut self.rxs[ri];
                let mut buf = [0.0f32; 128];
                r.audio.process(&mut [&mut buf[..]], now);
                r.out.extend_from_slice(&buf);
                r.rx.tick(now, Duration::from_secs_f64(2.0 * r.latency));
                while r.rx.poll_transmit(now).is_some() {}
                while let Some(e) = r.rx.poll_event() {
                    match e {
                        RxEvent::StateChanged { playing: false } => r.stops += 1,
                        RxEvent::BlocksLost { count } => r.lost += count,
                        _ => {}
                    }
                }
                r.t += 128.0 / f64::from(SR);
            } else {
                let (_, (i, b)) = self.q.pop_first().unwrap();
                let Ok(Datagram::Audio(a)) = Datagram::decode(&b) else {
                    unreachable!()
                };
                self.rxs[i].rx.handle_audio(now, &a);
            }
        }
    }

    /// Receiver `i`: input-to-output delay (ms) of every impulse that came out after `from` s.
    fn delays(&self, i: usize, from: f64) -> Vec<f64> {
        let r = &self.rxs[i];
        let t0 = 0.0007 + 0.0011 * i as f64;
        let period = PERIOD as f64 / (f64::from(SR) * self.speed);
        let mut v = Vec::new();
        let mut k = 0;
        while k < r.out.len() {
            if r.out[k].abs() > 0.4 {
                let t = t0 + k as f64 / f64::from(SR);
                if t >= from {
                    v.push(t.rem_euclid(period) * 1e3);
                }
                k += 1000;
            } else {
                k += 1;
            }
        }
        v
    }
}

fn spread(v: &[f64]) -> f64 {
    v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min)
}

/// Links of 10 and 70 ms (2 ms jitter): with a 150 ms playout delay both play every impulse
/// 150 ms after it entered the sender, within 0.1 ms; without sync they are ~60 ms apart.
#[test]
fn receivers_behind_different_latency_play_in_sync() {
    let mut sim = Sim::new(&[0.010, 0.070], Some(0.150), 1.0);
    sim.run(4.0);
    for i in 0..2 {
        let d = sim.delays(i, 1.5);
        eprintln!("sync, receiver {i}: {d:.2?}");
        assert!(d.len() >= 9, "{d:?}");
        assert!(d.iter().all(|x| (x - 150.0).abs() < 0.1), "receiver {i}: {d:?}");
    }

    let mut sim = Sim::new(&[0.010, 0.070], None, 1.0);
    sim.run(4.0);
    let (a, b) = (sim.delays(0, 1.5), sim.delays(1, 1.5));
    eprintln!("no sync: {a:.2?} / {b:.2?}");
    assert!(spread(&a) < 1.0 && spread(&b) < 1.0);
    assert!(b[0] - a[0] > 50.0, "{a:?} {b:?}");
}

/// The sender's clock runs 200 ppm fast: the receivers follow its rate (drift field) and the
/// sync keeps them within 0.5 ms of the target for half a minute.
#[test]
fn sync_holds_against_clock_drift() {
    let mut sim = Sim::new(&[0.005, 0.040], Some(0.100), 1.0002);
    sim.run(30.0);
    for i in 0..2 {
        let d = sim.delays(i, 2.0);
        assert!(d.len() > 100);
        let worst = d.iter().map(|x| (x - 100.0).abs()).fold(0.0, f64::max);
        eprintln!("drift, receiver {i}: worst {worst:.2} ms off over {} impulses", d.len());
        assert!(worst < 0.5, "receiver {i}: {d:.2?}");
    }
}

/// A target the link cannot meet (40 ms delay on a 70 ms link): the receiver plays late at
/// its own buffering, as without sync, and does not go silent; the one that can meet it does.
#[test]
fn an_unreachable_target_degrades_to_unsynced_playout() {
    let mut sim = Sim::new(&[0.070], None, 1.0);
    sim.run(3.0);
    let plain = sim.delays(0, 1.0);
    let mut sim = Sim::new(&[0.070, 0.001], Some(0.040), 1.0);
    sim.run(3.0);
    let late = sim.delays(0, 1.0);
    let ok = sim.delays(1, 1.0);
    eprintln!("unsynced {plain:.2?}, late {late:.2?}, reachable {ok:.2?}");
    assert_eq!(late.len(), plain.len(), "every impulse still plays");
    assert!(late.iter().all(|x| (x - plain[0]).abs() < 3.0), "{late:?} vs {plain:?}");
    assert!(ok.iter().all(|x| (x - 40.0).abs() < 0.1), "{ok:?}");
}

/// Jitter close to the buffer (18 ms on a 20 ms buffer) and a target the 70 ms link cannot
/// meet: the synced receiver plays exactly what the unsynced one plays, sample for sample (no
/// cuts towards the target, no margin given up, the same underruns and losses).
#[test]
fn an_unreachable_target_under_heavy_jitter_plays_like_unsynced() {
    let run = |delay| {
        let mut sim = Sim::new(&[0.070], delay, 1.0);
        sim.jitter = 0.018;
        sim.run(10.0);
        sim
    };
    let (plain, synced) = (run(None), run(Some(0.040)));
    let (p, s) = (&plain.rxs[0], &synced.rxs[0]);
    eprintln!(
        "unsynced: {} underruns, {} lost; synced: {} underruns, {} lost",
        p.stops, p.lost, s.stops, s.lost
    );
    assert_eq!((s.stops, s.lost), (p.stops, p.lost));
    let diff = p.out.iter().zip(&s.out).filter(|(a, b)| a != b).count();
    assert_eq!(diff, 0, "samples that differ from unsynced playout");
}

/// A sender's target that moves 30 ms earlier (a better clock estimate, say) is caught up with
/// once, from what the target had added: back on the new target, without underruns.
#[test]
fn a_target_that_moves_earlier_is_caught_up() {
    let mut sim = Sim::new(&[0.010], Some(0.150), 1.0);
    sim.run(2.0);
    sim.delay = Some(0.120);
    sim.run(5.0);
    let d = sim.delays(0, 3.5);
    eprintln!("moved target: {d:.2?}, {} underruns", sim.rxs[0].stops);
    assert!(d.iter().all(|x| (x - 120.0).abs() < 0.5), "{d:?}");
    assert_eq!((sim.rxs[0].stops, sim.rxs[0].lost), (0, 0));
}

/// Absurd targets (a million seconds ahead or behind; a hostile host or a forged clock) do not
/// panic and change nothing: the receiver plays exactly as without sync.
#[test]
fn absurd_targets_are_ignored() {
    let plain = {
        let mut sim = Sim::new(&[0.030], None, 1.0);
        sim.run(3.0);
        sim
    };
    for skew in [1e6, -1e6, 10.0, -10.0] {
        let mut sim = Sim::new(&[0.030], Some(0.150), 1.0);
        sim.skew = skew;
        sim.run(3.0);
        let (p, s) = (&plain.rxs[0], &sim.rxs[0]);
        let diff = p.out.iter().zip(&s.out).filter(|(a, b)| a != b).count();
        eprintln!("skew {skew} s: {:.2?}, {diff} samples differ", sim.delays(0, 1.0));
        assert_eq!(diff, 0, "skew {skew}");
        assert_eq!((s.stops, s.lost), (p.stops, p.lost));
    }
}

/// The link turns 190 ms slower while a reachable 150 ms target is met (4 ms jitter, 20 ms
/// buffer): both run dry once, after which the target is out of reach. The synced receiver must
/// then buffer as the unsynced one does: no catching up on the target with audio it does not
/// have (no more underruns or lost blocks than unsynced).
#[test]
fn a_latency_step_beyond_the_target_buffers_like_unsynced() {
    let run = |delay| {
        let mut sim = Sim::new(&[0.010], delay, 1.0);
        sim.jitter = 0.004;
        sim.step = (2.0, 0.190);
        sim.run(60.0);
        sim
    };
    let (plain, synced) = (run(None), run(Some(0.150)));
    let (p, s) = (&plain.rxs[0], &synced.rxs[0]);
    eprintln!(
        "latency step: unsynced {} underruns, {} lost; synced {} underruns, {} lost, late {:?} ms",
        p.stops,
        p.lost,
        s.stops,
        s.lost,
        s.rx.playout_error_ms()
    );
    assert!(s.stops <= p.stops && s.lost <= p.lost, "{:?} vs {:?}", (s.stops, s.lost), (p.stops, p.lost));
    // never less buffered than unsynced; later by at most a block or so (it ran dry at
    // another moment)
    let (dp, ds) = (plain.delays(0, 30.0), synced.delays(0, 30.0));
    eprintln!("latency step, after 30 s: unsynced {:.2} ms, synced {:.2} ms", dp[0], ds[0]);
    assert!(ds.len() == dp.len() && ds.iter().zip(&dp).all(|(a, b)| a - b > -0.5 && a - b < 3.0));
}

/// Sync on, then off: the stream is again exactly one that never synced. A 300 ms stall then
/// a burst leaves the same latency as for a never-synced stream (no ring kept larger by the
/// old target holding the burst).
#[test]
fn after_sync_off_the_ring_is_as_if_never_synced() {
    let run = |delay| {
        let mut sim = Sim::new(&[0.010], delay, 1.0);
        sim.run(2.0);
        sim.delay = None;
        sim.stall = (4.0, 4.3);
        sim.run(8.0);
        sim
    };
    let (plain, was) = (run(None), run(Some(0.150)));
    let (dp, dw) = (plain.delays(0, 6.0), was.delays(0, 6.0));
    eprintln!("after the stall: never synced {dp:.2?}, synced before {dw:.2?}");
    assert!(!dp.is_empty() && dp.len() == dw.len());
    assert!(dw.iter().zip(&dp).all(|(a, b)| (a - b).abs() < 1.0), "{dw:.2?} vs {dp:.2?}");
}
