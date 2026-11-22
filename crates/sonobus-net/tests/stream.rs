//! TxStream -> simulated network -> RxStream with virtual device clocks (seeded loss, latency,
//! jitter, duplication), plus latency parity against the legacy `aoo` source/sink.

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use aoo::codec::PcmBitDepth;
use bytes::Bytes;
use sonobus_net::stream::{
    Device, Encoding, RxAudio, RxEvent, RxOptions, RxStream, StreamFormat, TxAudio, TxOptions,
    TxStream,
};
use sonobus_net::wire::{self, Datagram, Format};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

enum Msg {
    /// To the receiver.
    Audio(Bytes),
    /// FORMAT on the (reliable) control stream.
    Ctl(Format),
    /// Receiver to sender (NACK, REPORT).
    Back(Bytes),
}

/// Decides a datagram's fate: `true` drops it. Arguments: datagram, is a NACK resend.
type DropFn = Box<dyn FnMut(&wire::Audio, bool) -> bool>;

struct Net {
    q: BTreeMap<(u64, u64), Msg>,
    n: u64,
    rng: Rng,
    loss: f64,
    latency: f64,
    jitter: f64,
    dup: f64,
    dropped: usize,
    /// NACK resends sent (datagrams).
    resends: usize,
    drop: DropFn,
}

impl Net {
    fn at(&mut self, t: f64, m: Msg) {
        let at = t + self.latency + self.rng.next() * self.jitter;
        self.n += 1;
        self.q.insert(((at * 1e9) as u64, self.n), m);
    }

    fn audio(&mut self, t: f64, b: Bytes, resend: bool) {
        let Ok(Datagram::Audio(a)) = Datagram::decode(&b) else {
            panic!("not audio")
        };
        assert!(b.len() <= wire::MAX_DATAGRAM);
        self.resends += usize::from(resend);
        if (self.drop)(&a, resend) || self.rng.next() < self.loss {
            self.dropped += 1;
            return;
        }
        if self.rng.next() < self.dup {
            self.at(t, Msg::Audio(b.clone()));
        }
        self.at(t, Msg::Audio(b));
    }
}

fn net(seed: u64, loss: f64, latency: f64, jitter: f64) -> Net {
    Net {
        q: BTreeMap::new(),
        n: 0,
        rng: Rng(seed),
        loss,
        latency,
        jitter,
        dup: 0.0,
        dropped: 0,
        resends: 0,
        drop: Box::new(|_, _| false),
    }
}

const SUB: u64 = 1;

type Signal = Box<dyn Fn(usize, usize) -> f32>;

struct Sim {
    tx: TxStream,
    tx_audio: TxAudio,
    rx: RxStream,
    rx_audio: RxAudio,
    tx_dev: Device,
    rx_dev: Device,
    /// Source device clock speed relative to wall time.
    src_speed: f64,
    /// The source stops feeding at this time.
    src_until: f64,
    net: Net,
    t_src: f64,
    t_sink: f64,
    in_frames: usize,
    signal: Signal,
    out: Vec<Vec<f32>>,
    events: Vec<(f64, RxEvent)>,
    nacks: usize,
    reports: Vec<wire::Report>,
    fill: Vec<(f64, f32)>,
    rtt: Duration,
    base: Instant,
}

fn dev(sr: u32, block: usize, channels: usize) -> Device {
    Device {
        sample_rate: sr,
        block_size: block,
        channels,
    }
}

fn pcm(sr: u32, block: u16, channels: u8, depth: PcmBitDepth) -> StreamFormat {
    StreamFormat {
        sample_rate: sr,
        block_size: block,
        channels,
        voice: false,
        encodings: vec![Encoding::Pcm(depth)],
    }
}

fn opus(block: u16, channels: u8) -> StreamFormat {
    StreamFormat {
        sample_rate: 48000,
        block_size: block,
        channels,
        voice: false,
        encodings: vec![Encoding::Opus {
            bitrate: 96_000 * u32::from(channels),
            complexity: 10,
        }],
    }
}

fn rx_opts(buffer_ms: f32, dynamic: bool) -> RxOptions {
    RxOptions {
        buffer_ms,
        dynamic_resampling: dynamic,
        ..RxOptions::default()
    }
}

impl Sim {
    #[allow(clippy::too_many_arguments)]
    fn new(
        tx_dev: Device,
        rx_dev: Device,
        format: StreamFormat,
        redundancy: u8,
        rx_opts: RxOptions,
        net: Net,
        signal: Signal,
    ) -> Self {
        let tx_opts = TxOptions {
            dynamic_resampling: rx_opts.dynamic_resampling,
            ..TxOptions::default()
        };
        let (mut tx, tx_audio) = TxStream::new(0, tx_dev, format, tx_opts).unwrap();
        assert!(tx.subscribe(SUB, 0, redundancy));
        let (mut rx, rx_audio) = RxStream::new(0, rx_dev, rx_opts);
        rx.set_format(&tx.take_format_change().unwrap()).unwrap();
        let rtt = Duration::from_secs_f64(2.0 * net.latency + net.jitter);
        Self {
            tx,
            tx_audio,
            rx,
            rx_audio,
            tx_dev,
            rx_dev,
            src_speed: 1.0,
            src_until: f64::MAX,
            net,
            t_src: 0.0,
            t_sink: 0.0013,
            in_frames: 0,
            signal,
            out: vec![Vec::new(); rx_dev.channels],
            events: Vec::new(),
            nacks: 0,
            reports: Vec::new(),
            fill: Vec::new(),
            rtt,
            base: Instant::now(),
        }
    }

    fn now(&self, t: f64) -> Instant {
        self.base + Duration::from_secs_f64(t)
    }

    fn transmit(&mut self, t: f64) {
        while let Some(b) = self.rx.poll_transmit(self.now(t)) {
            if matches!(Datagram::decode(&b), Ok(Datagram::Nack(_))) {
                self.nacks += 1;
            }
            self.net.at(t, Msg::Back(b));
        }
        while let Some(e) = self.rx.poll_event() {
            self.events.push((t, e));
        }
    }

    fn run(&mut self, until: f64) {
        loop {
            let t_net = self
                .net
                .q
                .first_key_value()
                .map_or(f64::MAX, |((t, _), _)| *t as f64 / 1e9);
            let t = t_net.min(self.t_src).min(self.t_sink);
            if t >= until {
                return;
            }
            let now = self.now(t);
            if t == self.t_src {
                let d = self.tx_dev;
                if t < self.src_until {
                    let chans: Vec<Vec<f32>> = (0..d.channels)
                        .map(|c| {
                            (0..d.block_size)
                                .map(|i| (self.signal)(self.in_frames + i, c))
                                .collect()
                        })
                        .collect();
                    let refs: Vec<&[f32]> = chans.iter().map(|c| &c[..]).collect();
                    assert!(self.tx_audio.process(&refs, now));
                    self.in_frames += d.block_size;
                    let net = &mut self.net;
                    self.tx.pump(|id, b| {
                        assert_eq!(id, SUB);
                        net.audio(t, b, false)
                    });
                    if let Some(f) = self.tx.take_format_change() {
                        self.net.at(t, Msg::Ctl(f));
                    }
                }
                self.t_src += d.block_size as f64 / (f64::from(d.sample_rate) * self.src_speed);
            } else if t == self.t_sink {
                let d = self.rx_dev;
                let mut bufs = vec![vec![0.0f32; d.block_size]; d.channels];
                let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
                self.rx_audio.process(&mut refs, now);
                for (o, b) in self.out.iter_mut().zip(&bufs) {
                    o.extend_from_slice(b);
                }
                self.rx.tick(now, self.rtt);
                self.transmit(t);
                if let Some(r) = self.rx.buffer_fill_ratio() {
                    self.fill.push((t, r));
                }
                self.t_sink += d.block_size as f64 / f64::from(d.sample_rate);
            } else {
                let (_, m) = self.net.q.pop_first().unwrap();
                match m {
                    Msg::Audio(b) => {
                        let Ok(Datagram::Audio(a)) = Datagram::decode(&b) else {
                            unreachable!()
                        };
                        self.rx.handle_audio(now, &a);
                        self.transmit(t);
                    }
                    Msg::Ctl(f) => self.rx.set_format(&f).unwrap(),
                    Msg::Back(b) => match Datagram::decode(&b).unwrap() {
                        Datagram::Nack(n) => {
                            let net = &mut self.net;
                            self.tx
                                .handle_nack_at(now, SUB, &n, |_, d| net.audio(t, d, true));
                        }
                        Datagram::Report(r) => self.reports.push(r),
                        d => panic!("{d:?}"),
                    },
                }
            }
        }
    }

    fn count(&self, after: f64, f: impl Fn(&RxEvent) -> Option<u32>) -> u32 {
        self.events
            .iter()
            .filter(|(t, _)| *t >= after)
            .filter_map(|(_, e)| f(e))
            .sum()
    }

    fn lost(&self, after: f64) -> u32 {
        self.count(after, |e| match e {
            RxEvent::BlocksLost { count } => Some(*count),
            _ => None,
        })
    }

    fn stops(&self, after: f64) -> u32 {
        self.count(after, |e| {
            matches!(e, RxEvent::StateChanged { playing: false }).then_some(1)
        })
    }
}

fn resent(e: &RxEvent) -> Option<u32> {
    match e {
        RxEvent::BlocksResent { count } => Some(*count),
        _ => None,
    }
}

fn recovered(e: &RxEvent) -> Option<u32> {
    match e {
        RxEvent::BlocksRecovered { count } => Some(*count),
        _ => None,
    }
}

/// Find `d` with `out[d + n] == input(n)` over a window starting at `from`.
fn find_delay(out: &[f32], input: impl Fn(usize) -> f32, from: usize, max: usize) -> usize {
    (0..max)
        .find(|&d| (from..from + 64).all(|n| out[d + n] == input(n)))
        .expect("no alignment")
}

/// Samples of `from..to` (input numbering) that differ from the input at delay `d`.
fn mismatches(sim: &Sim, d: usize, from: usize, to: usize) -> usize {
    let mut bad = 0;
    for (c, out) in sim.out.iter().enumerate() {
        for n in from..to.min(out.len() - d) {
            if out[d + n] != (sim.signal)(n, c) {
                bad += 1;
            }
        }
    }
    bad
}

fn sine(n: usize, c: usize) -> f32 {
    (n as f32 * 0.01 + c as f32).sin() * 0.8
}

/// float32 PCM, 2.5 ms blocks into 128-frame device callbacks.
fn pcm_sim(net: Net, k: u8, buffer_ms: f32) -> Sim {
    Sim::new(
        dev(48000, 120, 2),
        dev(48000, 128, 2),
        pcm(48000, 120, 2, PcmBitDepth::Float32),
        k,
        rx_opts(buffer_ms, false),
        net,
        Box::new(sine),
    )
}

#[test]
fn pcm_bit_exact_without_loss() {
    let mut sim = pcm_sim(net(1, 0.0, 0.005, 0.001), 0, 20.0);
    sim.run(3.0);
    let d = find_delay(&sim.out[0], |n| sine(n, 0), 48000, 9600);
    assert_eq!(mismatches(&sim, d, 4800, 48000 * 3), 0);
    assert_eq!((sim.lost(0.0), sim.stops(0.0), sim.nacks), (0, 0, 0));
    // 20 ms target, 5-6 ms network: delay within the target plus a few blocks
    assert!((960..=960 + 960).contains(&d), "{d}");
    // reports flow back once a second
    assert!(
        (2..=3).contains(&sim.reports.len()),
        "{}",
        sim.reports.len()
    );
    assert!(sim.reports.iter().all(|r| r.lost == 0 && r.buffer_ms > 0));
}

/// Isolated single losses (5 %) with k = 1: all recovered from the next datagram's
/// redundancy, no NACK, no loss, and the same delay as without loss.
#[test]
fn red_recovers_single_losses_without_nack() {
    let int16 = |net| {
        Sim::new(
            dev(48000, 120, 2),
            dev(48000, 128, 2),
            pcm(48000, 120, 2, PcmBitDepth::Int16),
            1,
            rx_opts(30.0, false),
            net,
            Box::new(sine),
        )
    };
    let mut clean = int16(net(2, 0.0, 0.005, 0.0005));
    clean.run(4.0);

    let mut n = net(2, 0.0, 0.005, 0.0005);
    let mut rng = Rng(77);
    let mut prev = u32::MAX;
    n.drop = Box::new(move |a, resend| {
        let s = a.header.seq;
        let drop = !resend && (10..1500).contains(&s) && prev != s - 1 && rng.next() < 0.05;
        if drop {
            prev = s;
        }
        drop
    });
    let mut sim = int16(n);
    sim.run(4.0);
    assert!(sim.net.dropped > 50, "{}", sim.net.dropped);
    // same samples at the same time as without loss
    assert!(sim.out == clean.out);
    assert!(sim.out[0].iter().any(|&x| x != 0.0));
    assert_eq!((sim.lost(0.0), sim.nacks), (0, 0));
    assert_eq!(sim.count(0.0, recovered), sim.net.dropped as u32);
}

/// Bursts of 3 lost blocks with a 60 ms buffer over a 20 ms round trip: NACKs repair them in
/// time, except the bursts whose resends are lost as well, which are concealed and counted.
#[test]
fn burst_loss_nack_repairs_or_conceals() {
    let mut n = net(3, 0.0, 0.01, 0.0005);
    // every 100th block starts a 3-block burst; for every 4th burst the resends die too
    n.drop = Box::new(|a, resend| {
        let s = a.header.seq;
        let burst = s >= 200 && s % 100 < 3;
        burst && (!resend || (s / 100) % 4 == 0)
    });
    let mut sim = pcm_sim(n, 0, 60.0);
    sim.run(5.0);
    let bursts = (200..2000).filter(|s| s % 100 == 0).count() as u32; // seqs sent in 5 s: 2000
    let dead = (200..2000)
        .filter(|s| s % 100 == 0 && (s / 100) % 4 == 0)
        .count() as u32;
    assert!(sim.nacks > 0);
    assert_eq!(sim.lost(0.0), 3 * dead);
    assert_eq!(sim.count(0.0, resent), 3 * (bursts - dead));
    let d = find_delay(&sim.out[0], |n| sine(n, 0), 50000, 9600);
    // the repaired bursts play bit-exact; a dead one is 3 concealed blocks plus the fade-in
    // of the next (64 frames), 2 channels
    let bad = mismatches(&sim, d, 4800, 48000 * 4);
    assert!(
        bad > 0 && bad <= (dead as usize) * (3 * 120 + 64) * 2,
        "{bad}"
    );
    assert_eq!(sim.stops(0.5), 0);
}

/// Buffer (10 ms) below the round trip (40 ms): no NACK at all, losses are concealed.
#[test]
fn no_nack_when_buffer_below_rtt() {
    let mut n = net(4, 0.0, 0.02, 0.0005);
    n.drop = Box::new(|a, _| a.header.seq > 50 && a.header.seq % 50 == 0);
    let mut sim = pcm_sim(n, 0, 10.0);
    sim.src_until = 2.9;
    sim.run(3.0);
    assert_eq!(sim.nacks, 0);
    let dropped = sim.net.dropped as u32;
    assert!(dropped > 20);
    assert_eq!(sim.lost(0.0), dropped);
}

/// Heavy jitter (reordering) and 10 % duplicates: still bit-exact.
#[test]
fn reordering_and_duplicates_tolerated() {
    let mut n = net(5, 0.0, 0.005, 0.008);
    n.dup = 0.1;
    let mut sim = pcm_sim(n, 0, 40.0);
    sim.run(4.0);
    let reordered = sim.count(0.0, |e| match e {
        RxEvent::BlocksReordered { count } => Some(*count),
        _ => None,
    });
    assert!(reordered > 100, "{reordered}");
    let d = find_delay(&sim.out[0], |n| sine(n, 0), 48000, 9600);
    assert_eq!(mismatches(&sim, d, 4800, 48000 * 4), 0);
    assert_eq!(sim.lost(0.0), 0);
}

/// 8 ch float32 at 2.5 ms = 3840 B = 4 fragments: reassembled bit-exact; a lost fragment is
/// one lost block without NACK and none with it.
#[test]
fn multi_fragment_blocks() {
    let fmt = pcm(48000, 120, 8, PcmBitDepth::Float32);
    assert_eq!(fmt.max_block_bytes(), 3840);
    let run = |buffer_ms: f32, latency: f64| {
        let mut n = net(6, 0.0, latency, 0.0005);
        n.drop = Box::new(|a, resend| {
            !resend && a.header.seq % 200 == 100 && a.fragment.map(|f| f.0) == Some(1)
        });
        let mut sim = Sim::new(
            dev(48000, 120, 8),
            dev(48000, 128, 8),
            fmt.clone(),
            2, // ignored for fragmented blocks
            rx_opts(buffer_ms, false),
            n,
            Box::new(sine),
        );
        sim.run(3.0);
        sim
    };
    let sim = run(10.0, 0.02);
    assert_eq!(sim.nacks, 0);
    assert_eq!(sim.net.dropped, 6); // seq 100, 300, .., 1100
    assert_eq!(sim.lost(0.0), 6);
    let sim = run(60.0, 0.01);
    assert_eq!(sim.lost(0.0), 0);
    assert_eq!(sim.count(0.0, resent), 6);
    // each repair resends only the lost fragment (1 of 4), not the whole block
    assert_eq!(sim.net.resends, 6);
    let d = find_delay(&sim.out[0], |n| sine(n, 0), 48000, 9600);
    assert_eq!(mismatches(&sim, d, 4800, 48000 * 3), 0);
}

fn tones(n: usize, c: usize) -> f32 {
    let w = 2.0 * std::f64::consts::PI * n as f64 / 48000.0 * (1.0 + 0.1 * c as f64);
    (0.25 * (311.0 * w).sin() + 0.25 * (457.0 * w + 1.0).sin() + 0.2 * (1013.0 * w + 2.0).sin())
        as f32
}

fn best_delay(out: &[f32], input: impl Fn(usize) -> f32, from: usize, max: usize) -> usize {
    (0..max)
        .map(|d| {
            let e: f64 = (from..from + 2000)
                .map(|n| (out[d + n] as f64 - input(n) as f64).powi(2))
                .sum();
            (d, e)
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
        .0
}

fn snr_db(
    out: &[f32],
    input: impl Fn(usize) -> f32,
    d: usize,
    range: std::ops::Range<usize>,
) -> f64 {
    let (mut sig, mut err) = (0.0f64, 0.0f64);
    for n in range {
        let x = input(n) as f64;
        sig += x * x;
        err += (out[d + n] as f64 - x).powi(2);
    }
    10.0 * (sig / err).log10()
}

fn opus_loopback(nch: u8) {
    let mut sim = Sim::new(
        dev(48000, 240, nch.into()),
        dev(48000, 240, nch.into()),
        opus(240, nch),
        1,
        rx_opts(20.0, false),
        net(7, 0.0, 0.005, 0.002),
        Box::new(tones),
    );
    sim.run(3.0);
    let d = best_delay(&sim.out[0], |n| tones(n, 0), 48000, 9000);
    for (c, out) in sim.out.iter().enumerate() {
        let snr = snr_db(out, |n| tones(n, c), d, 48000..48000 * 2);
        assert!(snr > 30.0, "{nch} ch, channel {c}: {snr:.1} dB (delay {d})");
    }
}

#[test]
fn opus_mono() {
    opus_loopback(1);
}

#[test]
fn opus_stereo() {
    opus_loopback(2);
}

#[test]
fn opus_four_channels() {
    opus_loopback(4);
}

/// Opus stereo, 10 ms blocks, isolated and double losses without NACK or redundancy: packet
/// loss concealment, no click (no step larger than the signal's own steepest slope allows).
#[test]
fn opus_plc_no_click() {
    for voice in [false, true] {
        opus_plc(voice);
    }
}

/// `voice`: the voice preset (Opus in-band FEC at 10 ms frames, expected loss set as `adapt`
/// would); concealment then uses the next block's FEC data.
fn opus_plc(voice: bool) {
    let mut n = net(8, 0.0, 0.02, 0.0005);
    n.drop = Box::new(|a, _| a.header.seq > 50 && matches!(a.header.seq % 40, 0 | 20 | 21));
    let fmt = StreamFormat {
        voice,
        ..opus(480, 2)
    };
    assert_eq!(fmt.fec(), voice);
    let mut sim = Sim::new(
        dev(48000, 480, 2),
        dev(48000, 480, 2),
        fmt,
        0,
        rx_opts(20.0, false),
        n,
        Box::new(tones),
    );
    sim.tx.set_expected_loss(8);
    sim.run(4.0);
    assert!(sim.lost(0.0) > 20);
    let slope = |x: &[f32]| {
        x.windows(2)
            .map(|w| (w[1] - w[0]).abs())
            .fold(0.0f32, f32::max)
    };
    let input: Vec<f32> = (0..48000).map(|n| tones(n, 0)).collect();
    let out = &sim.out[0][48000..48000 * 4];
    assert!(
        slope(out) < 2.0 * slope(&input),
        "{} vs {}",
        slope(out),
        slope(&input)
    );
    assert!(out.iter().map(|x| x * x).sum::<f32>() > 1000.0);
}

/// Source device 0.1 % fast for 10 simulated minutes: with dynamic resampling the buffer
/// stays bounded and nothing is lost; without, the buffer overflows.
#[test]
fn clock_drift_buffer_bounded() {
    let sig = |n: usize, _| (n as f32 * 0.05).sin() * 0.5;
    let run = |dynamic: bool| {
        let mut sim = Sim::new(
            dev(48000, 256, 1),
            dev(48000, 256, 1),
            pcm(48000, 256, 1, PcmBitDepth::Int16),
            0,
            rx_opts(50.0, dynamic),
            net(9, 0.0, 0.005, 0.002),
            Box::new(sig),
        );
        sim.src_speed = 1.001;
        sim.run(600.0);
        let late: Vec<f32> = sim
            .fill
            .iter()
            .filter(|(t, _)| *t > 10.0)
            .map(|(_, r)| *r)
            .collect();
        let min = late.iter().copied().fold(1.0f32, f32::min);
        (sim.lost(2.0), sim.stops(2.0), min)
    };
    let (lost, stops, min_fill) = run(true);
    assert_eq!((lost, stops), (0, 0));
    assert!(min_fill > 0.3, "{min_fill}");
    let (lost, ..) = run(false);
    assert!(lost > 0, "expected overflow without drift compensation");
}

fn zero_crossings(x: &[f32]) -> usize {
    x.windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count()
}

/// 48 kHz sender into a 44.1 kHz receiver: no underruns or losses and the pitch is right.
#[test]
fn resample_48k_to_44k1() {
    let sig = |n: usize, _| {
        ((n as f64 * 2.0 * std::f64::consts::PI * 1000.0 / 48000.0).sin() * 0.5) as f32
    };
    let mut sim = Sim::new(
        dev(48000, 256, 1),
        dev(44100, 256, 1),
        pcm(48000, 256, 1, PcmBitDepth::Float32),
        0,
        rx_opts(50.0, true),
        net(10, 0.0, 0.005, 0.002),
        Box::new(sig),
    );
    sim.run(30.0);
    assert_eq!((sim.lost(2.0), sim.stops(2.0)), (0, 0));
    let out = &sim.out[0];
    let last10 = &out[out.len() - 441_000..];
    let hz = zero_crossings(last10) as f64 / 2.0 / 10.0;
    assert!((hz - 1000.0).abs() < 1.0, "{hz} Hz");
}

/// PCM to Opus mid-stream: the receiver follows the new generation; a stale old-generation
/// datagram afterwards changes nothing.
#[test]
fn format_change_mid_stream() {
    let mut sim = Sim::new(
        dev(48000, 240, 2),
        dev(48000, 240, 2),
        pcm(48000, 120, 2, PcmBitDepth::Int16),
        1,
        rx_opts(30.0, false),
        net(11, 0.0, 0.005, 0.001),
        Box::new(tones),
    );
    sim.run(1.0);
    let mut stale = Vec::new();
    wire::encode_audio(
        &wire::AudioHeader {
            seq: 1_000_000,
            ..Default::default()
        },
        &[],
        &[0x7f; 480],
        &mut stale,
    )
    .unwrap();
    sim.tx.set_format(opus(240, 2)).unwrap();
    sim.run(2.0);
    assert_eq!(sim.rx.format(), Some(&opus(240, 2)));
    let Ok(Datagram::Audio(a)) = Datagram::decode(&stale) else {
        panic!()
    };
    let before = sim.events.len();
    sim.rx.handle_audio(sim.now(2.0), &a);
    sim.run(3.0);
    assert!(
        sim.events[before..]
            .iter()
            .all(|(_, e)| !matches!(e, RxEvent::BlocksGap { .. }))
    );
    assert_eq!((sim.lost(2.0), sim.stops(1.5)), (0, 0));
    let d = best_delay(&sim.out[0], |n| tones(n, 0), 48000 * 2, 9000);
    assert!(snr_db(&sim.out[0], |n| tones(n, 0), d, 48000 * 2..48000 * 5 / 2) > 25.0);
}

/// The block after `u32::MAX` starts generation + 1 at sequence 0 with the discontinuity flag;
/// the receiver follows without a gap.
#[test]
fn sequence_wrap() {
    let mut n = net(12, 0.0, 0.005, 0.0);
    let seen = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let s2 = seen.clone();
    n.drop = Box::new(move |a, _| {
        s2.borrow_mut()
            .push((a.header.generation, a.header.seq, a.header.discontinuity));
        false
    });
    let mut sim = pcm_sim(n, 1, 20.0);
    sim.tx.set_sequence_for_testing(u32::MAX - 200);
    sim.run(2.0);
    let seen = seen.borrow();
    let i = seen.iter().position(|x| x.1 == u32::MAX).unwrap();
    assert_eq!(seen[i], (0, u32::MAX, false));
    assert_eq!(seen[i + 1], (1, 0, true));
    assert_eq!(seen[i + 2], (1, 1, false));
    assert_eq!((sim.lost(0.0), sim.stops(0.2)), (0, 0));
    // playing at the end
    assert!(
        sim.out[0][sim.out[0].len() - 4800..]
            .iter()
            .any(|&x| x != 0.0)
    );
}

/// The last two blocks before the source stops are lost: no later datagram reveals the hole,
/// so only the timer-driven NACK can repair them.
#[test]
fn tail_loss_repaired_by_timer_nack() {
    let mut n = net(13, 0.0, 0.01, 0.0);
    n.drop = Box::new(|a, resend| !resend && (398..400).contains(&a.header.seq));
    let mut sim = pcm_sim(n, 0, 60.0);
    sim.src_until = 1.0; // 400 callbacks of 120 frames = blocks 0..400
    sim.run(1.5);
    assert_eq!(sim.net.dropped, 2);
    assert!(sim.nacks > 0);
    assert_eq!(sim.count(0.0, resent), 2);
    assert_eq!(sim.lost(0.0), 0);
    let d = find_delay(&sim.out[0], |n| sine(n, 0), 4800, 9600);
    assert_eq!(mismatches(&sim, d, 4800, 400 * 120), 0);
}

/// Four subscribers of one (encoding, redundancy) get pointer-identical datagrams; each
/// encoder runs once per block.
#[test]
fn encode_once_shared_bytes() {
    let fmt = StreamFormat {
        encodings: vec![
            Encoding::Pcm(PcmBitDepth::Float32),
            Encoding::Pcm(PcmBitDepth::Int16),
        ],
        ..pcm(48000, 120, 1, PcmBitDepth::Float32)
    };
    let (mut tx, mut audio) =
        TxStream::new(0, dev(48000, 120, 1), fmt, TxOptions::default()).unwrap();
    for id in 1..=4 {
        tx.subscribe(id, 0, 1);
    }
    tx.subscribe(5, 0, 0);
    tx.subscribe(6, 1, 2);
    assert!(!tx.subscribe(7, 2, 0));
    let base = Instant::now();
    let buf = [0.25f32; 120];
    for k in 0..10u64 {
        audio.process(&[&buf], base + Duration::from_micros(2500 * k));
        let mut got: Vec<(u64, Bytes)> = Vec::new();
        assert_eq!(tx.pump(|id, b| got.push((id, b))), 1);
        assert_eq!(got.len(), 6);
        let first = &got[0].1;
        for (id, b) in &got[..4] {
            assert_eq!(b.as_ptr(), first.as_ptr(), "subscriber {id}");
        }
        let red = |b: &Bytes| match Datagram::decode(b).unwrap() {
            Datagram::Audio(a) => (a.redundant.len(), a.payload.len()),
            _ => panic!(),
        };
        assert_eq!(red(first), ((k as usize).min(1), 480));
        assert_eq!(red(&got[4].1), (0, 480));
        assert_eq!(red(&got[5].1), ((k as usize).min(2), 240));
    }
    assert_eq!(tx.encode_count(), 20);
    // two 960 B stereo float32 blocks do not fit one datagram: k drops to 0 for those
    let (mut tx, mut audio) = TxStream::new(
        0,
        dev(48000, 120, 2),
        pcm(48000, 120, 2, PcmBitDepth::Float32),
        TxOptions::default(),
    )
    .unwrap();
    tx.subscribe(1, 0, 2);
    for k in 0..3u64 {
        audio.process(&[&buf, &buf], base + Duration::from_micros(2500 * k));
        tx.pump(|_, b| {
            assert!(b.len() <= wire::MAX_DATAGRAM);
            let Ok(Datagram::Audio(a)) = Datagram::decode(&b) else {
                panic!()
            };
            assert!(a.redundant.is_empty());
        });
    }
}

// ---------------------------------------------------------------------------------------------
// Latency parity with the legacy AOO source/sink

/// Delay in samples through `aoo` Source -> Sink (PCM float32 mono, no loss, fixed latency).
fn legacy_delay(block: i32, buffer_ms: i32, latency: f64) -> usize {
    use aoo::codec::{Codec, Format as AooFormat};
    use aoo::sink::{Sink, SinkOptions};
    use aoo::source::{Source, SourceOptions};
    use aoo::{Setup, TimeTag, message};
    let a: std::net::SocketAddr = "10.0.0.1:1000".parse().unwrap();
    let b: std::net::SocketAddr = "10.0.0.2:2000".parse().unwrap();
    let setup = Setup {
        samplerate: 48000,
        blocksize: block,
        nchannels: 1,
    };
    let (mut src, mut src_audio) = Source::new(
        1,
        setup,
        SourceOptions {
            dynamic_resampling: false,
            ..SourceOptions::default()
        },
    );
    src.set_format(AooFormat {
        nchannels: 1,
        samplerate: 48000,
        blocksize: block,
        codec: Codec::Pcm(PcmBitDepth::Float32),
    })
    .unwrap();
    src.add_sink(b, 7, 1);
    src.start();
    let (mut sink, mut sink_audio) = Sink::new(
        7,
        setup,
        SinkOptions {
            buffersize_ms: buffer_ms,
            dynamic_resampling: false,
            ..SinkOptions::default()
        },
    );
    let tt = |t: f64| TimeTag::from_secs(1000.0 + t);
    let mut q: BTreeMap<(u64, u64), (bool, Vec<u8>)> = BTreeMap::new();
    let mut n = 0u64;
    let (mut t_src, mut t_sink) = (0.0f64, 0.0013f64);
    let mut frames = 0usize;
    let mut out = Vec::new();
    let dt = block as f64 / 48000.0;
    loop {
        let t_net = q
            .first_key_value()
            .map_or(f64::MAX, |((t, _), _)| *t as f64 / 1e9);
        let t = t_net.min(t_src).min(t_sink);
        if t >= 3.0 {
            break;
        }
        let mut push = |q: &mut BTreeMap<_, _>, to_sink: bool, p: &[u8]| {
            n += 1;
            q.insert((((t + latency) * 1e9) as u64, n), (to_sink, p.to_vec()));
        };
        if t == t_src {
            let input: Vec<f32> = (0..block as usize).map(|i| sine(frames + i, 0)).collect();
            src_audio.process(&[&input], tt(t));
            frames += block as usize;
            src.send(|_, p| push(&mut q, true, p));
            t_src += dt;
        } else if t == t_sink {
            let mut buf = vec![0.0f32; block as usize];
            sink_audio.process(&mut [&mut buf], tt(t));
            out.extend_from_slice(&buf);
            sink.send(tt(t), |_, p| push(&mut q, false, p));
            t_sink += dt;
        } else {
            let (_, (to_sink, p)) = q.pop_first().unwrap();
            let msg = message::parse(&p).unwrap();
            if to_sink {
                sink.handle_message(a, &msg, tt(t));
                sink.send(tt(t), |_, p| push(&mut q, false, p));
            } else {
                src.handle_message(b, &msg, tt(t));
                src.send(|_, p| push(&mut q, true, p));
            }
        }
    }
    find_delay(&out, |n| sine(n, 0), 48000, 48000)
}

#[test]
fn latency_parity_with_legacy() {
    for (block, buffer_ms) in [(256u16, 50.0f32), (128, 20.0), (120, 10.0)] {
        let legacy = legacy_delay(block.into(), buffer_ms as i32, 0.005);
        let mut sim = Sim::new(
            dev(48000, block.into(), 1),
            dev(48000, block.into(), 1),
            pcm(48000, block, 1, PcmBitDepth::Float32),
            0,
            rx_opts(buffer_ms, false),
            net(14, 0.0, 0.005, 0.0),
            Box::new(sine),
        );
        sim.run(3.0);
        let new = find_delay(&sim.out[0], |n| sine(n, 0), 48000, 48000);
        println!("block {block}, buffer {buffer_ms} ms: v2 {new} samples, legacy {legacy} samples");
        assert!(new <= legacy, "block {block}: v2 {new} > legacy {legacy}");
    }
}
