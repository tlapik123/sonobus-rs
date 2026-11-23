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

/// The sender's clock runs 200 ppm fast or slow: the receivers follow its rate (drift field)
/// and sync slows down for what is left early; what is left late it leaves (sync only adds
/// latency). As the rate estimate settles (over a minute or two, swinging by 1-2 ms) that
/// leaves a late error of up to 2 ms (1.79 ms measured in these 30 s with the faster sender;
/// over 5 minutes `fast_sender_error` shows it back near 0 after 2 minutes with a faster
/// sender, and staying at 1.84 ms with a slower one); never early by more than 0.5 ms.
#[test]
fn sync_holds_against_clock_drift() {
    for (speed, bound) in [(1.0002, 2.0), (0.9998, 2.0)] {
        let mut sim = Sim::new(&[0.005, 0.040], Some(0.100), speed);
        sim.run(30.0);
        for i in 0..2 {
            let d = sim.delays(i, 2.0);
            assert!(d.len() > 100);
            let late = d.iter().map(|x| x - 100.0).fold(f64::MIN, f64::max);
            let early = d.iter().map(|x| 100.0 - x).fold(f64::MIN, f64::max);
            let last = d.last().unwrap() - 100.0;
            eprintln!("sender x{speed}, receiver {i}: up to {late:.2} ms late, {early:.2} ms early, {last:.2} at 30 s");
            assert!(late < bound && early < 0.5, "receiver {i}: {d:.2?}");
        }
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
/// meet: sync removes nothing (not one sample skipped, at any time), so the synced receiver
/// loses no more than the unsynced one; it goes late for the late arrivals, so here it loses
/// far fewer.
#[test]
fn an_unreachable_target_under_heavy_jitter_plays_no_worse_than_unsynced() {
    let f = Fmt { sr: 48000, block: 120, dev: 128, buffer_ms: 20.0 };
    let pat = pattern("18 ms jitter", 0.040, 10.0, Box::new(|_, r1, _| Some(0.070 + 0.018 * r1)));
    let (p, s) = (cell(f, &pat, false, 1), cell(f, &pat, true, 1));
    eprintln!(
        "unsynced: {} underruns, {} lost, splices {:.1?}; synced: {} underruns, {} lost, splices \
         {:.1?}",
        p.stops, p.lost, p.splices, s.stops, s.lost, s.splices
    );
    assert!(s.splices.iter().all(|x| x.1 > 0.0), "a skip: {:?}", s.splices);
    assert!(s.stops <= p.stops && s.lost <= p.lost);
    assert!(s.delivered >= p.delivered);
}

/// A sender's target that moves 30 ms earlier (a better clock estimate, say): sync removes no
/// audio, it restarts buffering once (one gap) and plays on the new target from then on. One
/// that moves 30 ms later is early: silence, no restart.
#[test]
fn a_target_that_moves_restarts_or_waits() {
    let mut sim = Sim::new(&[0.010], Some(0.150), 1.0);
    sim.run(2.0);
    sim.delay = Some(0.120);
    sim.run(5.0);
    let d = sim.delays(0, 1.0);
    eprintln!("target 150 -> 120 ms: {d:.2?}, {} underruns", sim.rxs[0].stops);
    let (before, after) = (sim.delays(0, 1.0).len() - sim.delays(0, 2.0).len(), sim.delays(0, 2.6));
    assert!(d[..before].iter().all(|x| (x - 150.0).abs() < 0.1), "{d:?}");
    assert!(after.len() >= 9 && after.iter().all(|x| (x - 120.0).abs() < 0.1), "{after:?}");
    assert!(sim.rxs[0].stops <= 1 && sim.rxs[0].lost == 0);

    let mut sim = Sim::new(&[0.010], Some(0.120), 1.0);
    sim.run(2.0);
    let stops = sim.rxs[0].stops;
    sim.delay = Some(0.150);
    sim.run(5.0);
    let after = sim.delays(0, 2.5);
    eprintln!("target 120 -> 150 ms: {after:.2?}");
    assert!(after.iter().all(|x| (x - 150.0).abs() < 0.1), "{after:?}");
    assert_eq!((sim.rxs[0].stops, sim.rxs[0].lost), (stops, 0));
}

/// The error reading is there only while it is current: on target while the stream plays;
/// gone within [`SYNC_STALE`] (200 ms) of the last block when the sender falls silent and while
/// the stream runs dry and rebuffers (never a stale "in sync"); back once it plays again,
/// late: it restarted on the backlog, and stays that late (sync removes nothing) until its
/// buffering restarts again.
#[test]
fn the_error_reading_is_gone_while_nothing_plays() {
    let mut sim = Sim::new(&[0.010], Some(0.150), 1.0);
    sim.stall = (3.0, 4.0);
    sim.run(2.9);
    let e = sim.rxs[0].rx.playout_error_ms();
    assert!(e.is_some_and(|e| e.abs() < 0.1), "{e:?}");
    // the last block before the stall arrives at about 3.012 s
    let mut seen = Vec::new();
    for k in 0..40 {
        let t = 3.25 + k as f64 * 0.025;
        sim.run(t);
        seen.push((t, sim.rxs[0].rx.playout_error_ms()));
    }
    assert!(seen[..30].iter().all(|x| x.1.is_none()), "{seen:.3?}");
    sim.run(5.0);
    let late = sim.rxs[0].rx.playout_error_ms().expect("playing again");
    sim.run(30.0);
    let still = sim.rxs[0].rx.playout_error_ms().expect("playing");
    eprintln!("after the outage: {late:.2} ms at 5 s, {still:.2} ms at 30 s");
    assert!(late < -100.0 && (still - late).abs() < 0.5, "{late} {still}");
    assert_eq!(sim.rxs[0].stops, 1, "one underrun: the outage");
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
    // never less buffered than unsynced; later by at most the jitter (it buffers for the late
    // arrivals, unsynced for the one it ran dry on) plus a block or callback
    let (dp, ds) = (plain.delays(0, 30.0), synced.delays(0, 30.0));
    eprintln!("latency step, after 30 s: unsynced {:.2} ms, synced {:.2} ms", dp[0], ds[0]);
    assert!(ds.len() == dp.len() && ds.iter().zip(&dp).all(|(a, b)| a - b > -0.5 && a - b < 4.0 + 2.7));
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

/// One run for the sweeps: 10 ms link, 20 ms buffer, the link `step` s slower from 2 s on,
/// `jitter` s uniform; optionally the target moves to `moved` at 6 s. Returns underruns, lost
/// blocks and the most the ring held (ms) in the last 5 of 20 s.
fn stepped(delay: Option<f64>, step: f64, jitter: f64, moved: Option<f64>) -> (u32, u32, f32) {
    let mut sim = Sim::new(&[0.010], delay, 1.0);
    sim.jitter = jitter;
    sim.step = (2.0, step);
    sim.run(6.0);
    if moved.is_some() {
        sim.delay = moved;
    }
    sim.run(15.0);
    let mut fill = 0.0f32;
    for k in 1..=500 {
        sim.run(15.0 + k as f64 * 0.01);
        fill = fill.max(sim.rxs[0].rx.fill_ms());
    }
    let r = &sim.rxs[0];
    (r.stops, r.lost, fill)
}

/// Sync only ever adds latency to what the jitter buffer needs. Whatever the latency step (0 to
/// the 150 ms target plus 100 ms, every 5 ms, at three jitter levels; plus targets moved earlier
/// while barely reachable), the synced receiver loses no more than the unsynced one (within 1
/// underrun and 1 % of the 8000 blocks) and holds no less: its ring at its fullest at least
/// what unsynced playout holds (the 20 ms jitter target, or less if unsynced holds less) less
/// one 2.5 ms block.
#[test]
fn latency_step_sweep_never_buffers_below_unsynced() {
    let mut cases: Vec<(f64, f64, Option<f64>)> = [0.002, 0.008, 0.012]
        .iter()
        .flat_map(|&j| (0..=50).map(move |k| (k as f64 * 0.005, j, None)))
        .collect();
    cases.extend([
        (0.125, 0.008, Some(0.140)),
        (0.120, 0.010, Some(0.145)),
        (0.130, 0.006, Some(0.148)),
        (0.110, 0.004, Some(0.130)),
    ]);
    let (mut worst, mut bad) = ((0i64, 0.0, 0.0), Vec::new());
    for &(step, jitter, moved) in &cases {
        let (pu, pl, pf) = stepped(None, step, jitter, None);
        let (su, sl, sf) = stepped(Some(0.150), step, jitter, moved);
        let excess = i64::from(sl) - i64::from(pl);
        if excess > worst.0 {
            worst = (excess, step, jitter);
        }
        if su > pu + 1 || excess > 80 || sf < pf.min(20.0) - 2.5 {
            bad.push(format!(
                "step {:.0} ms jitter {:.0} ms moved {moved:?}: unsynced {pu} underruns {pl} lost \
                 fill {pf:.1}; synced {su} / {sl} / {sf:.1}",
                step * 1e3,
                jitter * 1e3,
            ));
        }
    }
    eprintln!(
        "{} cases; worst lost-block excess {} at step {:.0} ms, jitter {:.0} ms",
        cases.len(),
        worst.0,
        worst.1 * 1e3,
        worst.2 * 1e3
    );
    assert!(bad.is_empty(), "{} cases worse than unsynced:\n{}", bad.len(), bad.join("\n"));
}

// ---------------------------------------------------------------------------------------------
// Matrix: synced against unsynced playout, cell by cell, over formats and link patterns.
//
// The sender's input names every sample (a ramp over CODE samples), so each output sample tells
// which input sample it plays and with what latency. A stretch of audio at one latency is a
// run; the latency changing between runs is a splice (an underrun, a silence insert, a skip);
// a concealed block does not change it.

/// Input samples before the code repeats (2.73 s at 48 kHz, 1.37 s at 96 kHz: above any
/// latency here).
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

impl Fmt {
    fn ms(&self, frames: f64) -> f64 {
        frames * 1e3 / f64::from(self.sr)
    }

    /// The headroom (ms) sync keeps arriving blocks above: what unsynced playout restarts
    /// with (the jitter target less a block), at least the margin (a block and two callbacks)
    /// with a block and a callback over it. `RxStream`'s formula, restated.
    fn need(&self) -> f64 {
        let (b, d) = (f64::from(self.block), self.dev as f64);
        let target = (f64::from(self.buffer_ms) * 1e-3 * f64::from(self.sr)).max(d);
        let restart = ((target / b).ceil() - 1.0) * b;
        self.ms(restart.max(b + 2.0 * d + b + b.max(d)))
    }
}

type Link = Box<dyn Fn(f64, f64, f64) -> Option<f64>>;

struct Pattern {
    name: String,
    /// Playout delay (s) of what is sent at `t` s.
    delay: Box<dyn Fn(f64) -> f64>,
    /// Sender and receiver clocks against true time.
    speed: f64,
    rx_speed: f64,
    secs: f64,
    /// Link delay (s) of a datagram sent at `t` s, from two uniform randoms; `None`: lost.
    link: Link,
    /// Round trip for NACKs (answered by the sender, the resend crossing the link), s; `None`:
    /// NACK off. (NACKs go out only while the jitter buffer exceeds it.)
    rtt: Option<f64>,
    /// Device callbacks come in pairs (two back to back, then a double pause).
    pairs: bool,
    /// The session thread (datagrams and the timer) runs only every this many callbacks.
    session: usize,
    /// Seeds to run (the jitter's random draws).
    seeds: u64,
    /// The final latency is checked against the target.
    reachable_at_end: bool,
    /// Splices allowed beyond [`SPLICES_PER_MIN`]: a link growing slower for seconds (a ramp)
    /// leaves sync going late in steps of at most 30 ms (`SYNC_LATE_STEP`) where unsynced
    /// playout eats into its jitter buffer and restarts with a whole one when it runs dry; a
    /// target moving earlier restarts buffering.
    extra_splices: usize,
    /// Times (s) the target moves earlier (a restart of buffering, by design).
    restarts: Vec<f64>,
    /// The host's anchor as the listener sees it: renewed once a second (as PLAYOUT), each
    /// time off by up to this (s, uniform), and shifted by `due_off(t)` (s; a clock offset
    /// estimate stepping).
    anchor_jitter: f64,
    due_off: Option<Box<dyn Fn(f64) -> f64>>,
}

fn pattern(name: &str, delay: f64, secs: f64, link: Link) -> Pattern {
    Pattern {
        name: name.into(),
        delay: Box::new(move |_| delay),
        speed: 1.0,
        rx_speed: 1.0,
        secs,
        link,
        rtt: None,
        pairs: false,
        session: 1,
        seeds: 1,
        reachable_at_end: false,
        extra_splices: 0,
        restarts: Vec::new(),
        anchor_jitter: 0.0,
        due_off: None,
    }
}

/// 2 ms of uniform jitter on top of `lat`.
fn jittery(lat: impl Fn(f64) -> f64 + 'static) -> Link {
    Box::new(move |t, r1, _| Some(lat(t) + 0.002 * r1))
}

/// Every `period` s (from 1 s into it), nothing arrives for `len(k)` s (the `k`-th period);
/// what was sent then arrives at its end.
fn stalls(lat: f64, period: f64, len: impl Fn(u64) -> f64 + 'static) -> Link {
    jittery(move |t| {
        let k = (t / period).floor();
        let (ph, l) = (t - k * period, len(k as u64));
        lat + if (1.0..1.0 + l).contains(&ph) { 1.0 + l - ph } else { 0.0 }
    })
}

fn heavy_tail(lat: f64) -> Link {
    Box::new(move |_, r1, r2| {
        let extra = if r2 < 0.02 { 0.005 + 0.015 * (r2 / 0.02) } else { 0.0 };
        Some(lat + 0.002 * r1 + extra)
    })
}

fn spikes(lat: f64) -> Link {
    jittery(move |t| lat + if t.rem_euclid(1.0) < 0.0026 { 0.012 } else { 0.0 })
}

fn loss_step(lat: f64) -> Link {
    Box::new(move |t, r1, r2| (r2 >= 0.03).then_some(lat + 0.008 * r1 + if t < 2.0 { 0.0 } else { 0.130 }))
}

fn patterns(f: Fmt) -> Vec<Pattern> {
    let buf = f64::from(f.buffer_ms) * 1e-3;
    // a link that leaves the target room (40 ms; 20 with the 100 ms buffer); one leaving the
    // target just a few ms over what the jitter buffer needs; one 5 ms short of it
    let spare = (0.150 - buf - 0.030).min(0.040);
    let tight = 0.150 - f.need() * 1e-3 - f.ms(f.dev as f64) * 1e-3 - 0.004;
    let short = 0.150 + 0.005 - buf - 0.002;
    let reach = |mut p: Pattern| {
        p.reachable_at_end = true;
        p
    };
    let seeds = |mut p: Pattern| {
        p.seeds = 3;
        p
    };
    let clocks = |name: &str, link: f64, speed: f64, rx_speed: f64| Pattern {
        speed,
        rx_speed,
        ..reach(pattern(name, 0.150, 30.0, jittery(move |_| link)))
    };
    // on a 30 ms link
    let moves = |name: &str, secs: f64, at: &'static [(f64, f64)]| Pattern {
        delay: Box::new(move |t| at.iter().rev().find(|x| t >= x.0).map_or(0.150, |x| x.1)),
        reachable_at_end: at.last().unwrap().1 * 1e3 - 32.0 - f.ms(f.dev as f64) > f.need() + 1.0,
        restarts: at.windows(2).filter(|w| w[1].1 < w[0].1).map(|w| w[1].0).collect(),
        extra_splices: at.len(),
        ..pattern(name, 0.0, secs, jittery(|_| 0.030))
    };
    vec![
        reach(pattern("steady", 0.150, 20.0, jittery(move |_| spare))),
        pattern("5 ms short", 0.150, 20.0, jittery(move |_| short)),
        pattern("step +125 ms", 0.150, 25.0, jittery(|t| if t < 2.0 { 0.010 } else { 0.135 })),
        pattern(
            "step up/down",
            0.150,
            25.0,
            jittery(|t| if (2.0..12.0).contains(&t) { 0.135 } else { 0.010 }),
        ),
        Pattern {
            // 190 ms in 30 ms steps
            extra_splices: 7,
            ..pattern("ramp 10-200 ms", 0.150, 25.0, jittery(|t| 0.010 + 0.019 * (t - 2.0).clamp(0.0, 10.0)))
        },
        pattern("60 ms stall / 2 s", 0.150, 30.0, stalls(0.120, 2.0, |_| 0.060)),
        pattern(
            "stalls / 3 s, 10-90 ms",
            0.150,
            60.0,
            stalls(0.040, 3.0, |k| 0.010 + 0.010 * ((k * 7919) % 9) as f64),
        ),
        seeds(pattern("15 ms jitter", 0.150, 30.0, Box::new(|_, r1, _| Some(0.040 + 0.015 * r1)))),
        seeds(pattern("heavy tail", 0.040, 30.0, heavy_tail(0.070))),
        seeds(pattern("12 ms spike / s", 0.040, 30.0, spikes(0.070))),
        seeds(pattern("3 % loss, step", 0.150, 25.0, loss_step(0.010))),
        Pattern {
            rtt: Some(0.010),
            ..seeds(pattern("3 % loss, 5 ms link, NACK", 0.150, 25.0, Box::new(|_, r1, r2| {
                (r2 >= 0.03).then_some(0.005 + 0.002 * r1)
            })))
        },
        Pattern {
            rtt: Some(0.160),
            ..pattern("60 ms stall / 2 s at 80 ms, NACK", 0.150, 30.0, stalls(0.080, 2.0, |_| 0.060))
        },
        Pattern {
            rtt: Some(0.020),
            ..seeds(pattern("3 % loss, step, NACK", 0.150, 25.0, loss_step(0.010)))
        },
        // not checked against the target: the callback times no longer tell when the audio
        // plays (it comes out 4 to 17 ms later than the target here, a constant offset)
        Pattern { pairs: true, ..seeds(pattern("callbacks in pairs", 0.150, 20.0, jittery(move |_| spare))) },
        Pattern { session: 2, ..reach(pattern("session late 1 callback", 0.150, 20.0, jittery(move |_| spare))) },
        Pattern { session: 3, ..pattern("session late 2 callbacks, 15 ms jitter", 0.150, 20.0, Box::new(|_, r1, _| Some(0.040 + 0.015 * r1))) },
        clocks("sender +200 ppm", spare, 1.0002, 1.0),
        clocks("sender -200 ppm", spare, 0.9998, 1.0),
        clocks("sender +200 ppm, tight", tight, 1.0002, 1.0),
        clocks("sender -200 ppm, tight", tight, 0.9998, 1.0),
        clocks("receiver +200 ppm, tight", tight, 1.0, 1.0002),
        clocks("receiver -200 ppm, tight", tight, 1.0, 0.9998),
        moves("target 300 -> 100 ms", 40.0, &[(0.0, 0.300), (20.0, 0.100)]),
        moves("target 100 -> 150 ms", 30.0, &[(0.0, 0.100), (15.0, 0.150)]),
        moves(
            "target 150-300-100-150",
            90.0,
            &[(0.0, 0.150), (20.0, 0.300), (45.0, 0.100), (70.0, 0.150)],
        ),
    ]
}

/// The long patterns of the sixth review, on two formats: stalls of 100 to 150 ms every 12 to
/// 60 s; a tight link with ±200 ppm and a 40 ms stall every 25 s.
fn long_patterns(f: Fmt, which: usize) -> Vec<Pattern> {
    let tight = 0.150 - f.need() * 1e-3 - f.ms(f.dev as f64) * 1e-3 - 0.004;
    let stall = |period: f64, secs: f64| {
        pattern(
            &format!("100-150 ms stall / {period} s"),
            0.150,
            secs,
            stalls(0.040, period, |k| 0.100 + 0.025 * (k % 3) as f64),
        )
    };
    let drift = |speed: f64| Pattern {
        speed,
        ..pattern(
            &format!("tight, sender x{speed}, 40 ms stall / 25 s"),
            0.150,
            600.0,
            stalls(tight, 25.0, |_| 0.040),
        )
    };
    match which {
        0 => vec![stall(12.0, 300.0), stall(20.0, 300.0)],
        1 => vec![stall(35.0, 300.0), stall(60.0, 600.0)],
        _ => vec![drift(1.0002), drift(0.9998)],
    }
}

#[derive(Debug, Default)]
struct Outcome {
    stops: u32,
    lost: u32,
    /// Output samples of input from 5 s to the last second.
    delivered: usize,
    /// (output time s, latency change ms, latency after ms) between runs.
    splices: Vec<(f64, f64, f64)>,
    /// Mean latency (ms) over the last second.
    latency: f64,
    /// Per device callback: blocks lost, the error reading said "on target" (0.5 ms), the mean
    /// latency of what it played (NaN: none), the ring fill after it (ms).
    lost_at: Vec<u32>,
    on_target: Vec<bool>,
    lat: Vec<f64>,
    fill: Vec<f64>,
    /// Per device callback: underruns so far.
    stops_at: Vec<u32>,
    /// The fastest playout sync resampling offset seen, ppm.
    max_steer: i32,
}

/// One receiver of `pat` in format `f`; `sync`: with the playout target.
fn cell(f: Fmt, pat: &Pattern, sync: bool, seed: u64) -> Outcome {
    let sr = f64::from(f.sr);
    let bl = usize::from(f.block);
    let dev = |block| Device {
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
    let (mut tx, mut tx_audio) = TxStream::new(0, dev(bl), format, TxOptions::default()).unwrap();
    let fw = tx.take_format_change().unwrap();
    assert!(tx.subscribe(0, 0, 0));
    let opts = RxOptions {
        buffer_ms: f.buffer_ms,
        ..RxOptions::default()
    };
    let (mut rx, mut audio) = RxStream::new(0, dev(f.dev), opts);
    rx.set_format(&fw).unwrap();
    let rtt = pat.rtt.unwrap_or(10.0);
    let base = Instant::now();
    let calls = if pat.pairs { 2 } else { 1 };
    let period = (calls * f.dev) as f64 / (sr * pat.rx_speed);
    let (mut t_src, mut t_cb, mut t_sess, mut in_frames) = (0.0, 0.0007, 0.0007, 0usize);
    // datagrams on the way to the receiver, NACKs on the way back
    let mut q: BTreeMap<(u64, u64), Bytes> = BTreeMap::new();
    let mut back: BTreeMap<(u64, u64), Bytes> = BTreeMap::new();
    let (mut n, mut rng) = (0u64, 0x9e37_79b9_7f4a_7c15u64 ^ seed.wrapping_mul(0x2545_f491_4f6c_dd1d));
    let mut rand = move || {
        rng ^= rng << 13;
        rng ^= rng >> 7;
        rng ^= rng << 17;
        (rng >> 11) as f64 / (1u64 << 53) as f64
    };
    let mut o = Outcome::default();
    let (mut lat_sum, mut lat_n) = (0.0, 0usize);
    // the current run: latency of its last sample, length; the end of the last run counted
    let (mut run_lat, mut run_len, mut last_end) = (f64::NAN, 0usize, f64::NAN);
    let in_rate = sr * pat.speed;
    let mut buf = vec![0.0f32; f.dev];
    let mut send = |t: f64, b: Bytes, q: &mut BTreeMap<(u64, u64), Bytes>| {
        let (r1, r2) = (rand(), rand());
        if let Some(d) = (pat.link)(t, r1, r2) {
            n += 1;
            q.insert((((t + d) * 1e9) as u64, n), b);
        }
    };
    let first = |q: &BTreeMap<(u64, u64), Bytes>| {
        q.first_key_value().map_or(f64::MAX, |((t, _), _)| *t as f64 / 1e9)
    };
    let noisy = pat.anchor_jitter > 0.0 || pat.due_off.is_some();
    let (mut held, mut held_at, mut jr) = (None, f64::MIN, 0x2545_f491_4f6c_dd1du64 ^ seed);
    loop {
        // with a late session thread, datagrams wait for it
        let t_net = if pat.session > 1 { f64::MAX } else { first(&q) };
        let t_sess_next = if pat.session > 1 { t_sess } else { f64::MAX };
        let t_back = first(&back);
        let t = t_net.min(t_back).min(t_src).min(t_cb).min(t_sess_next);
        if t >= pat.secs {
            break;
        }
        let now = base + Duration::from_secs_f64(t);
        if t == t_src {
            let block: Vec<f32> = (in_frames..in_frames + bl).map(code).collect();
            assert!(tx_audio.process(&[&block], now));
            in_frames += bl;
            let mut sent = Vec::new();
            tx.pump(|_, b| sent.push(b));
            for b in sent {
                send(t, b, &mut q);
            }
            let mut anchor = tx.playout_anchor();
            if noisy {
                if t >= held_at + 1.0 {
                    jr ^= jr << 13;
                    jr ^= jr >> 7;
                    jr ^= jr << 17;
                    let j = ((jr >> 11) as f64 / (1u64 << 53) as f64 - 0.5) * 2.0 * pat.anchor_jitter;
                    held = anchor.map(|a| (a, j));
                    held_at = t;
                }
                let off = pat.due_off.as_ref().map_or(0.0, |f| f(t));
                anchor = held.map(|((g, seq, at), j)| (g, seq, shift(at, j + off)));
            }
            match (sync, anchor) {
                (false, _) => rx.set_playout(None),
                (true, Some((generation, seq, at))) => {
                    let d = Duration::from_secs_f64((pat.delay)(t));
                    rx.set_playout(Some(Playout {
                        generation,
                        seq,
                        due: at + d,
                        span: d,
                    }))
                }
                _ => {}
            }
            t_src += bl as f64 / in_rate;
        } else if t == t_cb {
            for k in 0..calls {
                // called back to back; the second plays a callback after the first
                audio.process(&mut [&mut buf[..]], base + Duration::from_secs_f64(t + k as f64 * 50e-6));
                let t = t + (k * f.dev) as f64 / sr;
                let (mut cb_lat, mut cb_n) = (0.0, 0usize);
                for (k, &v) in buf.iter().enumerate() {
                    let t_out = t + k as f64 / sr;
                    if !(0.04..0.96).contains(&v) {
                        run_len = 0;
                        continue;
                    }
                    // latency in input samples, and the input sample this is
                    let lat = (t_out * in_rate - f64::from(v - 0.05) / 0.9 * CODE).rem_euclid(CODE);
                    let t_in = t_out - lat / in_rate;
                    let lat = lat / in_rate * 1e3;
                    if run_len > 0 && (lat - run_lat).abs() < 3e3 / sr {
                        run_len += 1;
                    } else {
                        run_len = 1;
                    }
                    run_lat = lat;
                    // a run counts from 16 samples on (not the fades around a concealed block)
                    if run_len < 16 {
                        continue;
                    }
                    let new = if run_len == 16 { 16 } else { 1 };
                    if run_len == 16 && (lat - last_end).abs() > 0.25 {
                        o.splices.push((t_out, lat - last_end, lat));
                    }
                    last_end = lat;
                    cb_lat += lat;
                    cb_n += 1;
                    if t_in >= 5.0 && t_in < pat.secs - 1.0 {
                        o.delivered += new;
                    }
                    if t_out >= pat.secs - 1.0 {
                        lat_sum += lat;
                        lat_n += 1;
                    }
                }
                o.lat.push(if cb_n > 0 { cb_lat / cb_n as f64 } else { f64::NAN });
            }
            if pat.session == 1 {
                rx.tick(now, Duration::from_secs_f64(rtt));
            }
            while let Some(d) = rx.poll_transmit(now) {
                if pat.rtt.is_some() && matches!(Datagram::decode(&d), Ok(Datagram::Nack(_))) {
                    let k = back.len() as u64;
                    back.insert((((t + rtt / 2.0) * 1e9) as u64, k), d);
                }
            }
            let mut lost = 0;
            while let Some(e) = rx.poll_event() {
                match e {
                    RxEvent::StateChanged { playing: false } => o.stops += 1,
                    RxEvent::BlocksLost { count } => lost += count,
                    _ => {}
                }
            }
            o.lost += lost;
            for k in 0..calls {
                o.lost_at.push(if k == 0 { lost } else { 0 });
                o.on_target.push(rx.playout_error_ms().is_some_and(|e| e.abs() < 0.5));
                o.fill.push(f64::from(rx.fill_ms()));
                o.stops_at.push(o.stops);
            }
            o.max_steer = o.max_steer.max(rx.playout_steer_ppm());
            t_cb += period;
        } else if t == t_sess_next {
            while first(&q) <= t {
                let (_, b) = q.pop_first().unwrap();
                let Ok(Datagram::Audio(a)) = Datagram::decode(&b) else {
                    unreachable!()
                };
                rx.handle_audio(now, &a);
            }
            rx.tick(now, Duration::from_secs_f64(rtt));
            t_sess += pat.session as f64 * f.dev as f64 / (sr * pat.rx_speed);
        } else if t == t_back {
            let (_, d) = back.pop_first().unwrap();
            let Ok(Datagram::Nack(nack)) = Datagram::decode(&d) else {
                unreachable!()
            };
            let mut resent = Vec::new();
            tx.handle_nack_at(now, 0, &nack, |_, b| resent.push(b));
            for b in resent {
                send(t, b, &mut q);
            }
        } else {
            let (_, b) = q.pop_first().unwrap();
            let Ok(Datagram::Audio(a)) = Datagram::decode(&b) else {
                unreachable!()
            };
            rx.handle_audio(now, &a);
        }
    }
    o.latency = lat_sum / lat_n.max(1) as f64;
    o
}

/// Splices (beyond unsynced playout's) allowed per minute after the first 5 s.
const SPLICES_PER_MIN: f64 = 3.0;
/// Lost blocks allowed beyond unsynced playout's: per run, plus this share of the blocks.
const LOST_MARGIN: (u32, f64) = (4, 0.002);
/// Final latency against a reachable target, ms.
const SYNC_TOLERANCE_MS: f64 = 0.1;

/// Checks one pattern in format `f`, synced against unsynced, every seed; returns a row per
/// seed and whether it failed.
///
/// Per cell: no more underruns than unsynced (plus one per restart a target moving earlier
/// asks for), no more lost blocks (+ [`LOST_MARGIN`]), as much audio delivered (but where a
/// restart drops the ring); no skip ever (no latency drop that unsynced does not have too,
/// but at those restarts) and never a faster resampling for sync; at every callback (but within
/// 2 s after a splice in either run, where one restarted a moment before the other) latency at
/// least unsynced's less a block and ring fill at least unsynced's less a block and a callback
/// (its level after a callback depends on where playback restarted against it); splices within
/// [`SPLICES_PER_MIN`] of unsynced's; while the reading says "on target", no more blocks lost
/// than unsynced over the same callbacks; reachable targets met within [`SYNC_TOLERANCE_MS`]
/// at the end unless sync had to go late (a splice up) or the sender's clock is faster.
fn check(f: Fmt, pat: &Pattern) -> Vec<(String, bool)> {
    (1..=pat.seeds)
        .map(|seed| {
            let (u, s) = (cell(f, pat, false, seed), cell(f, pat, true, seed));
            let near_restart = |t: f64| pat.restarts.iter().any(|&r| t >= r && t < r + 2.0);
            let splices = |o: &Outcome| o.splices.iter().filter(|x| x.0 >= 5.0).count();
            let drops = |o: &Outcome| o.splices.iter().filter(|x| x.1 < 0.0 && !near_restart(x.0)).count();
            let blocks = pat.secs * f64::from(f.sr) / f64::from(f.block);
            let lost_margin = LOST_MARGIN.0 + (LOST_MARGIN.1 * blocks) as u32;
            let block_ms = f.ms(f64::from(f.block));
            let cb_s = f.dev as f64 / f64::from(f.sr);
            // callbacks within 2 s after a splice of either run, or before 5 s; and once
            // unsynced playout underran twice more than synced (HEAD's false underruns with a
            // full ring each add a callback of latency it keeps: more buffered, by accident)
            let settled = |k: usize| {
                let t = 0.0007 + k as f64 * cb_s;
                t >= 5.0
                    && !u.splices.iter().chain(&s.splices).any(|x| t >= x.0 - 0.1 && t < x.0 + 2.0)
                    && !near_restart(t)
                    && u.stops_at[k] <= s.stops_at[k] + 1
            };
            let n = u.lat.len().min(s.lat.len());
            // a block and a device callback: the ring's level after a callback depends on
            // where playback restarted against it
            let slack = block_ms + f.ms(f.dev as f64);
            // less latency than unsynced playout only while on target (where sync has made sure
            // of the jitter buffer: it would have gone late otherwise)
            let behind = (0..n)
                .filter(|&k| settled(k) && !s.on_target[k] && s.lat[k] < u.lat[k] - slack)
                .map(|k| u.lat[k] - s.lat[k])
                .fold(0.0, f64::max);
            let mean = |o: &Outcome| {
                let from = o.fill.len() * 2 / 3;
                o.fill[from..].iter().sum::<f64>() / (o.fill.len() - from).max(1) as f64
            };
            let on_at_end = s.on_target.last().copied().unwrap_or(false);
            let emptier = if on_at_end { 0.0 } else { (mean(&u) - mean(&s) - slack).max(0.0) };
            let on: Vec<usize> = (0..n).filter(|&k| s.on_target[k]).collect();
            let lost_on = |o: &Outcome| on.iter().map(|&k| o.lost_at[k]).sum::<u32>();
            let went_late = s.splices.iter().any(|x| x.1 > 0.0 && !near_restart(x.0));
            let err = s.latency - (pat.delay)(pat.secs) * 1e3;
            let mut why = Vec::new();
            if s.stops > u.stops + pat.restarts.len() as u32 {
                why.push("underruns");
            }
            if s.lost > u.lost + lost_margin {
                why.push("lost");
            }
            // a concealed block costs its frames and the fade-in of the next (64 frames)
            let concealed = lost_margin as usize * (usize::from(f.block) + 64);
            if pat.restarts.is_empty() && s.delivered + concealed < u.delivered {
                why.push("delivered");
            }
            if drops(&s) > drops(&u) {
                why.push("skip");
            }
            if s.max_steer > 0 {
                why.push("sped up");
            }
            if behind > 0.0 {
                why.push("less latency");
            }
            if emptier > 0.0 {
                why.push("emptier ring");
            }
            let allowed = (SPLICES_PER_MIN * (pat.secs - 5.0) / 60.0).ceil() as usize
                + pat.extra_splices
                + splices(&u) / 100;
            if splices(&s) > splices(&u) + allowed {
                why.push("splices");
            }
            if lost_on(&s) > lost_on(&u) + lost_margin {
                why.push("lost while reading on target");
            }
            // with clock drift, as late as unsynced playout's own latency swings (HEAD's rate
            // matching settling, ±1-2 ms over the first minutes; sync slows down for the early
            // half and leaves the late half), and never early
            let drifting = pat.speed != 1.0 || pat.rx_speed != 1.0;
            let from = (0.5 / cb_s) as usize;
            let seen = u.lat[from..].iter().filter(|x| x.is_finite());
            let (lo, hi) = seen.fold((f64::MAX, f64::MIN), |(a, b), &x| (a.min(x), b.max(x)));
            let u_drift = hi - lo;
            let bound = if drifting { u_drift + 0.2 } else { SYNC_TOLERANCE_MS };
            if pat.reachable_at_end && !went_late && (err > bound || err < -SYNC_TOLERANCE_MS) {
                why.push("off target");
            }
            let row = format!(
                "{f:?} {:>40} seed {seed}: unsynced {:>3} stops {:>5} lost {:>3} splices lat {:>7.2} \
                 drift {:>6.2} | synced {:>3} / {:>5} / {:>3} err {:>+8.3} steer {:>5} behind \
                 {:.2} emptier {:.2} on-target lost {}/{} {}",
                pat.name,
                u.stops,
                u.lost,
                splices(&u),
                u.latency,
                u_drift,
                s.stops,
                s.lost,
                splices(&s),
                err,
                s.max_steer,
                behind,
                emptier,
                lost_on(&s),
                lost_on(&u),
                why.join(", "),
            );
            eprintln!("{row}");
            if !why.is_empty() {
                eprintln!("   unsynced splices {:.1?}\n   synced splices {:.1?}", u.splices, s.splices);
            }
            (row, !why.is_empty())
        })
        .collect()
}

fn matrix(f: Fmt) {
    let bad: Vec<String> = patterns(f).iter().flat_map(|p| check(f, p)).filter(|x| x.1).map(|x| x.0).collect();
    assert!(bad.is_empty(), "{f:?}: cells worse than unsynced:\n{}", bad.join("\n"));
}

fn long_matrix(which: usize) {
    let mut bad = Vec::new();
    for f in [
        Fmt { sr: 48000, block: 120, dev: 128, buffer_ms: 20.0 },
        Fmt { sr: 48000, block: 128, dev: 512, buffer_ms: 20.0 },
    ] {
        bad.extend(long_patterns(f, which).iter().flat_map(|p| check(f, p)).filter(|x| x.1).map(|x| x.0));
    }
    assert!(bad.is_empty(), "cells worse than unsynced:\n{}", bad.join("\n"));
}

#[test]
fn matrix_48k_block_120_device_128() {
    matrix(Fmt { sr: 48000, block: 120, dev: 128, buffer_ms: 20.0 });
}

#[test]
fn matrix_44k1_block_256_device_512() {
    matrix(Fmt { sr: 44100, block: 256, dev: 512, buffer_ms: 20.0 });
}

#[test]
fn matrix_48k_block_128_device_512() {
    matrix(Fmt { sr: 48000, block: 128, dev: 512, buffer_ms: 20.0 });
}

#[test]
fn matrix_48k_block_64_device_256_buffer_10() {
    matrix(Fmt { sr: 48000, block: 64, dev: 256, buffer_ms: 10.0 });
}

#[test]
fn matrix_96k_block_64_device_1024() {
    matrix(Fmt { sr: 96000, block: 64, dev: 1024, buffer_ms: 20.0 });
}

#[test]
fn matrix_48k_buffer_100() {
    matrix(Fmt { sr: 48000, block: 120, dev: 128, buffer_ms: 100.0 });
}

#[test]
fn matrix_long_stalls_12_20_s() {
    long_matrix(0);
}

#[test]
fn matrix_long_stalls_35_60_s() {
    long_matrix(1);
}

#[test]
fn matrix_long_tight_drift_and_stalls() {
    long_matrix(2);
}

/// How the late error with a faster sender develops over 5 minutes (the rate matching settles;
/// sync does not speed up): `cargo test -- --ignored --nocapture fast_sender_error`.
#[test]
#[ignore]
fn fast_sender_error() {
    for f in [
        Fmt { sr: 48000, block: 120, dev: 128, buffer_ms: 20.0 },
        Fmt { sr: 48000, block: 128, dev: 512, buffer_ms: 20.0 },
    ] {
        for speed in [1.0002, 0.9998] {
            let pat = Pattern { speed, ..pattern("drift", 0.150, 300.0, jittery(|_| 0.040)) };
            let (u, s) = (cell(f, &pat, false, 1), cell(f, &pat, true, 1));
            let at = |o: &Outcome, t: f64| {
                let k = (t * f64::from(f.sr) / f.dev as f64) as usize;
                o.lat[k..k + 50].iter().cloned().filter(|x| x.is_finite()).fold(f64::NAN, f64::max)
            };
            let row: Vec<String> = [1.0, 10.0, 30.0, 60.0, 120.0, 299.0]
                .iter()
                .map(|&t| format!("{t}s {:.2}/{:.2}", at(&u, t), at(&s, t) - 150.0))
                .collect();
            eprintln!("{f:?} x{speed}: unsynced latency / synced error: {}", row.join("  "));
        }
    }
}

/// `at` moved by `s` seconds.
fn shift(at: Instant, s: f64) -> Instant {
    let d = Duration::from_secs_f64(s.abs());
    if s >= 0.0 { at + d } else { at - d }
}

/// The due time wobbles without the host changing anything: its anchor jittering by ±6 or
/// ±12 ms (renewed once a second, as PLAYOUT), or the clock offset estimate stepping by 12 or
/// 30 ms every 8 s, later or earlier. Sync never restarts buffering for it (no audio dropped
/// but up to a block at the counting window's edges, no latency drop, no extra underrun) and inserts silence at most once after the first 5 s:
/// it settles at the late edge of the wobble, where nothing is early by more than
/// `SYNC_HARD`. 120 s, 30 ms link, 150 ms target.
#[test]
fn a_noisy_due_time_never_restarts_buffering() {
    let mut bad = Vec::new();
    for f in [
        Fmt { sr: 48000, block: 120, dev: 128, buffer_ms: 20.0 },
        Fmt { sr: 48000, block: 128, dev: 512, buffer_ms: 20.0 },
    ] {
        let mut pats: Vec<Pattern> = [0.006, 0.012]
            .iter()
            .map(|&j| Pattern {
                anchor_jitter: j,
                ..pattern(&format!("anchor jitter ±{:.0} ms", j * 1e3), 0.150, 120.0, jittery(|_| 0.030))
            })
            .collect();
        for step in [0.012, -0.012, 0.030, -0.030] {
            pats.push(Pattern {
                due_off: Some(Box::new(move |t| if (t / 8.0).floor() as i64 % 2 == 1 { step } else { 0.0 })),
                ..pattern(&format!("offset {:+.0} ms every other 8 s", step * 1e3), 0.150, 120.0, jittery(|_| 0.030))
            });
        }
        for pat in &pats {
            let (u, s) = (cell(f, pat, false, 1), cell(f, pat, true, 1));
            let drops = s.splices.iter().filter(|x| x.1 < 0.0).count();
            let inserts = s.splices.iter().filter(|x| x.0 >= 5.0 && x.1 > 0.0).count();
            let dropped = u.delivered.saturating_sub(s.delivered);
            let row = format!(
                "{f:?} {}: {} restarts or drops, {inserts} inserts after 5 s, {} underruns (unsynced \
                 {}), {dropped} samples fewer delivered; splices {:.1?}",
                pat.name, drops, s.stops, u.stops, s.splices
            );
            eprintln!("{row}");
            // (counted at the window's edges, and a slower resampling stretches a little: up
            // to a block either way; a restart drops some 100 ms)
            if drops > 0 || inserts > 1 || s.stops > u.stops || dropped > usize::from(f.block) {
                bad.push(row);
            }
        }
    }
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}
