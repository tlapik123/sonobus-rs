//! Rust source -> Rust sink over a simulated network (seeded loss, latency, jitter/reordering)
//! with virtual device clocks, plus two-thread tests of the audio/network hand-over.

use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use aoo::codec::{
    Codec, Format, OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_SIGNAL_MUSIC, OpusOptions,
    PcmBitDepth,
};
use aoo::sink::{Sink, SinkAudio, SinkEventKind, SinkOptions};
use aoo::source::{Source, SourceAudio, SourceOptions};
use aoo::{Message, Setup, TimeTag, message};

const A: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1)),
    1000,
);
const B: SocketAddr = SocketAddr::new(
    std::net::IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2)),
    2000,
);
const EPOCH: f64 = 1000.0;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// (delivery time ns, tiebreak, to sink?, bytes)
type InFlight = Reverse<(u64, u64, bool, Vec<u8>)>;

struct Net {
    q: BinaryHeap<InFlight>,
    n: u64,
    rng: Rng,
    loss: f64,
    latency: f64,
    jitter: f64,
    dropped: usize,
}

impl Net {
    fn push(&mut self, t: f64, to_sink: bool, p: &[u8]) {
        // audio data is lossy; control traffic too, but less
        let loss = if to_sink { self.loss } else { self.loss / 2.0 };
        if self.rng.next() < loss {
            self.dropped += 1;
            return;
        }
        let at = t + self.latency + self.rng.next() * self.jitter;
        self.n += 1;
        self.q
            .push(Reverse(((at * 1e9) as u64, self.n, to_sink, p.to_vec())));
    }
}

struct Sim {
    src: Source,
    src_audio: SourceAudio,
    sink: Sink,
    sink_audio: SinkAudio,
    src_setup: Setup,
    sink_setup: Setup,
    /// Source device clock speed relative to wall time.
    src_speed: f64,
    net: Net,
    t_src: f64,
    t_sink: f64,
    in_frames: usize,
    signal: Box<dyn Fn(usize, usize) -> f32>,
    output: Vec<Vec<f32>>,
    events: Vec<(f64, SinkEventKind)>,
    fill: Vec<(f64, f32)>,
}

impl Sim {
    #[allow(clippy::too_many_arguments)]
    fn new(
        src_setup: Setup,
        sink_setup: Setup,
        format: Format,
        src_opts: SourceOptions,
        sink_opts: SinkOptions,
        net: Net,
        signal: Box<dyn Fn(usize, usize) -> f32>,
    ) -> Self {
        let (mut src, src_audio) = Source::new(1, src_setup, src_opts);
        src.set_format(format).unwrap();
        src.add_sink(B, 7, 1);
        src.start();
        let (sink, sink_audio) = Sink::new(7, sink_setup, sink_opts);
        Self {
            src,
            src_audio,
            sink,
            sink_audio,
            src_setup,
            sink_setup,
            src_speed: 1.0,
            net,
            t_src: 0.0,
            t_sink: 0.0013,
            in_frames: 0,
            signal,
            output: vec![Vec::new(); sink_setup.nchannels as usize],
            events: Vec::new(),
            fill: Vec::new(),
        }
    }

    fn tt(t: f64) -> TimeTag {
        TimeTag::from_secs(EPOCH + t)
    }

    fn sink_send(&mut self, t: f64) {
        let net = &mut self.net;
        self.sink.send(Self::tt(t), |to, p| {
            assert_eq!(to, A);
            net.push(t, false, p)
        });
        while let Some(e) = self.sink.poll_event() {
            self.events.push((t, e.kind));
        }
    }

    fn src_send(&mut self, t: f64) {
        let net = &mut self.net;
        self.src.send(|to, p| {
            assert_eq!(to, B);
            assert!(p.len() <= 600);
            net.push(t, true, p)
        });
        while self.src.poll_event().is_some() {}
    }

    fn run(&mut self, until: f64) {
        loop {
            let t_net = self
                .net
                .q
                .peek()
                .map_or(f64::MAX, |Reverse((t, ..))| *t as f64 / 1e9);
            let t = t_net.min(self.t_src).min(self.t_sink);
            if t >= until {
                return;
            }
            if t == self.t_src {
                let s = self.src_setup;
                let chans: Vec<Vec<f32>> = (0..s.nchannels as usize)
                    .map(|c| {
                        (0..s.blocksize as usize)
                            .map(|i| (self.signal)(self.in_frames + i, c))
                            .collect()
                    })
                    .collect();
                let refs: Vec<&[f32]> = chans.iter().map(|c| &c[..]).collect();
                assert!(self.src_audio.process(&refs, Self::tt(t)));
                self.in_frames += s.blocksize as usize;
                self.src_send(t);
                self.t_src += s.blocksize as f64 / (s.samplerate as f64 * self.src_speed);
            } else if t == self.t_sink {
                let s = self.sink_setup;
                let mut bufs = vec![vec![0.0f32; s.blocksize as usize]; s.nchannels as usize];
                let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
                self.sink_audio.process(&mut refs, Self::tt(t));
                for (o, b) in self.output.iter_mut().zip(&bufs) {
                    o.extend_from_slice(b);
                }
                self.sink_send(t);
                if let Some(r) = self.sink.buffer_fill_ratio(A, 1) {
                    self.fill.push((t, r));
                }
                self.t_sink += s.blocksize as f64 / s.samplerate as f64;
            } else {
                let Reverse((_, _, to_sink, p)) = self.net.q.pop().unwrap();
                let msg = message::parse(&p).unwrap();
                if to_sink {
                    self.sink.handle_message(A, &msg, Self::tt(t));
                    self.sink_send(t);
                } else {
                    assert!(self.src.handle_message(B, &msg, Self::tt(t)), "{msg:?}");
                    self.src_send(t);
                }
            }
        }
    }

    fn count(&self, after: f64, f: impl Fn(&SinkEventKind) -> Option<i32>) -> i32 {
        self.events
            .iter()
            .filter(|(t, _)| *t >= after)
            .filter_map(|(_, e)| f(e))
            .sum()
    }
}

fn setup(sr: i32, bs: i32, nch: i32) -> Setup {
    Setup {
        samplerate: sr,
        blocksize: bs,
        nchannels: nch,
    }
}

fn net(seed: u64, loss: f64, latency: f64, jitter: f64) -> Net {
    Net {
        q: BinaryHeap::new(),
        n: 0,
        rng: Rng(seed),
        loss,
        latency,
        jitter,
        dropped: 0,
    }
}

fn no_dynamic() -> (SourceOptions, SinkOptions) {
    (
        SourceOptions {
            dynamic_resampling: false,
            ..SourceOptions::default()
        },
        SinkOptions {
            dynamic_resampling: false,
            ..SinkOptions::default()
        },
    )
}

/// Find `d` with `out[d + n] == input(n)` over a window late in the run.
fn find_delay(out: &[f32], input: impl Fn(usize) -> f32, from: usize, max: usize) -> usize {
    (0..max)
        .find(|&d| (from..from + 64).all(|n| out[d + n] == input(n)))
        .expect("no alignment")
}

/// float32 PCM, 4 frames per block, 5 % loss with jitter: after the jitter delay the output
/// is bit-identical to the input because resends recover every drop.
#[test]
fn pcm_lossy_reordering_bit_exact() {
    let signal = |n: usize, c: usize| (n as f32 * 0.01 + c as f32).sin() * 0.8;
    let fmt = Format {
        nchannels: 2,
        samplerate: 48000,
        blocksize: 256,
        codec: Codec::Pcm(PcmBitDepth::Float32),
    };
    let (so, ko) = no_dynamic();
    let mut sim = Sim::new(
        setup(48000, 256, 2),
        setup(48000, 256, 2),
        fmt,
        so,
        ko,
        // upstream resend timing (5 tries, 10 ms apart) needs the round trip below ~40 ms
        net(42, 0.05, 0.004, 0.004),
        Box::new(signal),
    );
    sim.run(10.0);
    assert!(sim.net.dropped > 100, "{}", sim.net.dropped);
    let out = &sim.output;
    let d = find_delay(&out[0], |n| signal(n, 0), 48000 * 5, 48000);
    let end = out[0].len() - d;
    let mut bad = 0;
    for (c, out) in out.iter().enumerate() {
        // the start is lossy before the format arrives
        for n in 24000..end {
            if out[d + n] != signal(n, c) {
                bad += 1;
            }
        }
    }
    assert_eq!(bad, 0, "delay {d}");
    let resent = sim.count(0.0, |e| match e {
        SinkEventKind::BlocksResent { count } => Some(*count),
        _ => None,
    });
    let lost = sim.count(1.0, |e| match e {
        SinkEventKind::BlocksLost { count } => Some(*count),
        _ => None,
    });
    assert!(resent > 50, "resent {resent}");
    assert_eq!(lost, 0);
    // jitter buffer delay: ~100 ms buffer plus network latency
    assert!((4800..9600).contains(&d), "{d}");
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

fn opus_loopback(nch: i32) {
    // incommensurate tones per channel: the alignment search has a unique optimum
    let signal = move |n: usize, c: usize| {
        let w = 2.0 * std::f64::consts::PI * n as f64 / 48000.0 * (1.0 + 0.1 * c as f64);
        (0.25 * (311.0 * w).sin() + 0.25 * (457.0 * w + 1.0).sin() + 0.2 * (1013.0 * w + 2.0).sin())
            as f32
    };
    let fmt = Format {
        nchannels: nch,
        samplerate: 48000,
        blocksize: 240,
        codec: Codec::Opus(OpusOptions {
            bitrate: 96_000 * nch,
            complexity: 10,
            signal_type: OPUS_SIGNAL_MUSIC,
            application_type: OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        }),
    };
    let (so, ko) = no_dynamic();
    let mut sim = Sim::new(
        setup(48000, 240, nch),
        setup(48000, 240, nch),
        fmt,
        so,
        ko,
        net(7, 0.0, 0.005, 0.002),
        Box::new(signal),
    );
    sim.run(4.0);
    let d = best_delay(&sim.output[0], |n| signal(n, 0), 48000 * 2, 9000);
    for (c, out) in sim.output.iter().enumerate() {
        let snr = snr_db(out, |n| signal(n, c), d, 48000..48000 * 3);
        // the codec alone reaches ~38-41 dB on this signal
        assert!(snr > 30.0, "channel {c}: {snr:.1} dB (delay {d})");
    }
}

#[test]
fn opus_mono() {
    opus_loopback(1);
}

#[test]
fn opus_four_channels() {
    opus_loopback(4);
}

fn zero_crossings(x: &[f32]) -> usize {
    x.windows(2)
        .filter(|w| (w[0] < 0.0) != (w[1] < 0.0))
        .count()
}

/// 48 kHz source into a 44.1 kHz sink for a simulated minute: no underruns or losses, the
/// buffer stays put and the pitch is right.
#[test]
fn resample_48k_to_44k1_no_drift() {
    let signal = |n: usize, _| {
        ((n as f64 * 2.0 * std::f64::consts::PI * 1000.0 / 48000.0).sin() * 0.5) as f32
    };
    let fmt = Format {
        nchannels: 1,
        samplerate: 48000,
        blocksize: 256,
        codec: Codec::Pcm(PcmBitDepth::Float32),
    };
    let mut sim = Sim::new(
        setup(48000, 256, 1),
        setup(44100, 256, 1),
        fmt,
        SourceOptions::default(),
        SinkOptions::default(),
        net(3, 0.0, 0.005, 0.002),
        Box::new(signal),
    );
    sim.run(60.0);
    let stops = sim.count(2.0, |e| {
        matches!(e, SinkEventKind::StateChanged { playing: false }).then_some(1)
    });
    let lost = sim.count(2.0, |e| match e {
        SinkEventKind::BlocksLost { count } => Some(*count),
        _ => None,
    });
    assert_eq!((stops, lost), (0, 0));
    let late: Vec<f32> = sim
        .fill
        .iter()
        .filter(|(t, _)| *t > 10.0)
        .map(|(_, r)| *r)
        .collect();
    let (min, max) = late
        .iter()
        .fold((1.0f32, 0.0f32), |(a, b), &r| (a.min(r), b.max(r)));
    assert!(max - min < 0.5, "fill {min}..{max}");
    let out = &sim.output[0];
    let last10 = &out[out.len() - 441_000..];
    let hz = zero_crossings(last10) as f64 / 2.0 / 10.0;
    assert!((hz - 1000.0).abs() < 1.0, "{hz} Hz");
}

/// Source device 0.1 % fast. With dynamic resampling the sink buffer stays bounded for two
/// simulated minutes; without, it overflows and drops blocks.
#[test]
fn clock_drift_buffer_bounded() {
    let signal = |n: usize, _| (n as f32 * 0.05).sin() * 0.5;
    let fmt = Format {
        nchannels: 1,
        samplerate: 48000,
        blocksize: 256,
        codec: Codec::Pcm(PcmBitDepth::Float32),
    };
    let run = |dynamic: bool| {
        let so = SourceOptions {
            dynamic_resampling: dynamic,
            ..SourceOptions::default()
        };
        let ko = SinkOptions {
            dynamic_resampling: dynamic,
            buffersize_ms: 50,
            ..SinkOptions::default()
        };
        let mut sim = Sim::new(
            setup(48000, 256, 1),
            setup(48000, 256, 1),
            fmt,
            so,
            ko,
            net(5, 0.0, 0.005, 0.002),
            Box::new(signal),
        );
        sim.src_speed = 1.001;
        sim.run(120.0);
        let lost = sim.count(2.0, |e| match e {
            SinkEventKind::BlocksLost { count } => Some(*count),
            _ => None,
        });
        let stops = sim.count(2.0, |e| {
            matches!(e, SinkEventKind::StateChanged { playing: false }).then_some(1)
        });
        (lost, stops)
    };
    assert_eq!(run(true), (0, 0));
    let (lost, _) = run(false);
    assert!(lost > 0, "expected overflow without drift compensation");
}

/// Source audio half on its own thread: every callback crosses the ring intact and in order.
#[test]
fn source_rings_two_threads() {
    let (mut src, mut audio) = Source::new(1, setup(48000, 64, 2), SourceOptions::default());
    src.set_format(Format {
        nchannels: 2,
        samplerate: 48000,
        blocksize: 64,
        codec: Codec::Pcm(PcmBitDepth::Float32),
    })
    .unwrap();
    src.add_sink(B, 7, 1);
    src.start();
    const CALLBACKS: usize = 20_000;
    let producer = std::thread::spawn(move || {
        let mut l = [0.0f32; 64];
        let mut r = [0.0f32; 64];
        for k in 0..CALLBACKS {
            for i in 0..64 {
                l[i] = (k * 64 + i) as f32;
                r[i] = -l[i];
            }
            let t = TimeTag::from_secs(EPOCH + k as f64 * 64.0 / 48000.0);
            while !audio.process(&[&l, &r], t) {
                std::thread::yield_now();
            }
        }
        audio
    });
    let mut expect = 0usize;
    let mut blocks = 0;
    while blocks < CALLBACKS {
        src.send(|_, p| {
            if let Ok(Message::CompactData { data, .. }) = message::parse(p) {
                for (i, s) in data.chunks(4).enumerate() {
                    let v = f32::from_be_bytes(s.try_into().unwrap());
                    let want = if i % 2 == 0 {
                        expect as f32
                    } else {
                        -(expect as f32)
                    };
                    // first callback is faded in
                    if blocks > 0 {
                        assert_eq!(v, want, "block {blocks}");
                    }
                    if i % 2 == 1 {
                        expect += 1;
                    }
                }
                blocks += 1;
            }
        });
    }
    producer.join().unwrap();
}

/// Sink audio half on its own thread, fed from this thread: the output is the exact stream.
#[test]
fn sink_rings_two_threads() {
    let (mut sink, mut audio) = Sink::new(
        7,
        setup(48000, 64, 1),
        SinkOptions {
            buffersize_ms: 10,
            dynamic_resampling: false,
            ..SinkOptions::default()
        },
    );
    let fmt = Message::SinkFormat {
        sink: 7,
        src: 1,
        version: aoo::VERSION,
        salt: 5,
        nchannels: 1,
        samplerate: 48000,
        blocksize: 64,
        codec: "pcm",
        options: &[0, 0, 0, 2],
        user_format: None,
    };
    let t0 = TimeTag::from_secs(EPOCH);
    sink.handle_message(A, &fmt, t0);
    sink.send(t0, |_, _| {});
    const BLOCKS: usize = 20_000;
    let fed = Arc::new(AtomicUsize::new(0));
    let played = Arc::new(AtomicUsize::new(0));
    let done = Arc::new(AtomicBool::new(false));
    let consumer = {
        let (fed, played, done) = (fed.clone(), played.clone(), done.clone());
        std::thread::spawn(move || {
            let mut out = Vec::with_capacity(BLOCKS * 64);
            let mut buf = [0.0f32; 64];
            let mut k = 0;
            while !done.load(Ordering::Acquire) {
                // the ring (8 blocks, prefilled) keeps at least 2 blocks: never an underrun.
                // Blocks only move into the ring when a packet arrives, as in the C++.
                if fed.load(Ordering::Acquire) + 7 <= played.load(Ordering::Relaxed) {
                    std::thread::yield_now();
                    continue;
                }
                audio.process(
                    &mut [&mut buf],
                    TimeTag::from_secs(EPOCH + k as f64 * 64.0 / 48000.0),
                );
                k += 1;
                out.extend_from_slice(&buf);
                played.fetch_add(1, Ordering::Release);
            }
            out
        })
    };
    let mut bytes = [0u8; 256];
    for seq in 0..BLOCKS {
        // feed only when the ring has room, so every block moves straight in
        while seq >= played.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        for (i, b) in bytes.chunks_mut(4).enumerate() {
            b.copy_from_slice(&((seq * 64 + i) as f32 + 1.0).to_be_bytes());
        }
        let m = Message::CompactData {
            salt: 5,
            seq: seq as i32,
            samplerate: None,
            data: &bytes,
        };
        sink.handle_message(A, &m, t0);
        sink.send(t0, |_, _| {});
        fed.fetch_add(1, Ordering::Release);
    }
    while played.load(Ordering::Acquire) + 20 < BLOCKS {
        std::thread::yield_now();
    }
    done.store(true, Ordering::Release);
    let out = consumer.join().unwrap();
    // first sample of block 0 is faded to 0
    let start = out.iter().position(|&x| x != 0.0).unwrap() - 1;
    // skip the faded-in first block, then the ramp must be exact
    let ramp = &out[start + 64..];
    let n = ramp.len().min((BLOCKS - 30) * 64);
    for (i, &x) in ramp[..n].iter().enumerate() {
        assert_eq!(x, (64 + i) as f32 + 1.0, "sample {i}");
    }
    while let Some(e) = sink.poll_event() {
        assert!(!matches!(e.kind, SinkEventKind::BlocksLost { .. }), "{e:?}");
    }
}
