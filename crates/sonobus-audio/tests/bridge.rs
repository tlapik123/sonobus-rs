//! Simulated-clock tests of the bridge: independent input/output device clocks with drift,
//! jittery callback sizes and timing, rate conversion, stalls, input-only and output-only modes.

use std::f64::consts::TAU;

use sonobus_audio::{AudioBlock, Duplex, ExternalInput, InputMonitor, InputStats, default_margin};

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> f64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A device clock: callback `k` carries `chunks[k % len]` frames and is due once those frames
/// have elapsed at the drifted rate, plus up to `jitter` seconds of scheduling delay.
#[derive(Clone)]
struct Clock {
    rate: u32,
    drift: f64,
    chunks: Vec<usize>,
    jitter: f64,
    /// No callbacks in [t0, t1): an input loses that audio, an output plays nothing.
    pause: Option<(f64, f64)>,
}

struct InDev {
    clock: Clock,
    freq: f64,
}

struct Sim {
    inputs: Vec<InDev>,
    /// None = input-only: the engine is advanced from the first input's callbacks.
    output: Option<Clock>,
    engine_rate: u32,
    block: usize,
    secs: f64,
    /// Measurements (fill bounds, underflows, continuity) start here.
    check_from: f64,
}

#[derive(Debug)]
struct Report {
    stats: Vec<InputStats>,
    /// Stats at `check_from`.
    stats_at_check: Vec<InputStats>,
    fill_min: Vec<usize>,
    fill_max: Vec<usize>,
    fill_avg: Vec<f64>,
    ratio_min: Vec<f64>,
    ratio_max: Vec<f64>,
    ratio_avg: Vec<f64>,
    max_residual: Vec<f64>,
    delays: Vec<usize>,
    blocks: u64,
}

const AMP: f64 = 0.5;

struct Dev {
    clock: Clock,
    due: f64,
    k: usize,
    rng: Rng,
}
impl Dev {
    fn new(clock: Clock, seed: u64) -> Self {
        let mut d = Dev {
            clock,
            due: 0.0,
            k: 0,
            rng: Rng(seed),
        };
        d.due = d.chunk() as f64 / d.actual_rate();
        d
    }
    fn actual_rate(&self) -> f64 {
        self.clock.rate as f64 * (1.0 + self.clock.drift)
    }
    fn chunk(&self) -> usize {
        self.clock.chunks[self.k % self.clock.chunks.len()]
    }
    fn when(&mut self) -> f64 {
        self.due + self.clock.jitter * self.rng.next()
    }
    fn advance(&mut self) {
        self.k += 1;
        self.due += self.chunk() as f64 / self.actual_rate();
    }
    fn paused(&self, t: f64) -> bool {
        self.clock.pause.is_some_and(|(a, b)| t >= a && t < b)
    }
}

fn run(sim: Sim) -> Report {
    let n_in = sim.inputs.len();
    let out_ch = n_in.max(1);
    let engine = move |b: &mut AudioBlock<'_>| {
        for (o, i) in b.outputs.iter_mut().zip(b.inputs.iter()) {
            o.copy_from_slice(i);
        }
    };
    let mut duplex = Duplex::new(engine, sim.engine_rate, sim.block, out_ch);
    let mut producers: Vec<ExternalInput> = sim
        .inputs
        .iter()
        .map(|d| duplex.add_input(1, d.clock.rate, default_margin(d.clock.rate)))
        .collect();
    let monitors: Vec<InputMonitor> = producers.iter().map(ExternalInput::monitor).collect();
    let delays = duplex.input_delays();

    let mut in_devs: Vec<Dev> = sim
        .inputs
        .iter()
        .enumerate()
        .map(|(i, d)| Dev::new(d.clock.clone(), 7 + i as u64))
        .collect();
    let mut phase = vec![0usize; n_in];
    let mut out_dev = sim.output.clone().map(|c| Dev::new(c, 99));
    let mut recorded: Vec<Vec<f32>> = vec![Vec::new(); out_ch];
    let mut check_index = None;
    let mut stats_at_check = Vec::new();
    let mut fill_min = vec![usize::MAX; n_in];
    let mut fill_max = vec![0; n_in];
    let mut fill_sum = vec![0f64; n_in];
    let mut fill_n = 0usize;
    let mut ratio_min = vec![f64::MAX; n_in];
    let mut ratio_max = vec![f64::MIN; n_in];
    let mut ratio_sum = vec![0f64; n_in];
    let mut buf = Vec::new();
    let mut samples = Vec::new();

    let trace = std::env::var_os("SIM_TRACE").is_some();
    let mut next_trace = 0.0;
    let mut record = |frames: usize, t: f64, rendered: Option<&[f32]>| {
        if let Some(out) = rendered {
            for f in 0..frames {
                for c in 0..out_ch {
                    recorded[c].push(out[f * out_ch + c]);
                }
            }
        }
        if trace && t >= next_trace {
            next_trace += 1.0;
            for (i, m) in monitors.iter().enumerate() {
                let s = m.stats();
                println!(
                    "t={t:5.1} in{i} fill {:5} minslack {:5} ratio {:.6} under {} skip {}",
                    s.fill_frames,
                    s.min_slack_frames,
                    s.drift_ratio,
                    s.underflows,
                    s.skipped_frames
                );
            }
        }
        if t >= sim.check_from {
            if check_index.is_none() {
                check_index = Some(recorded[0].len());
                stats_at_check = monitors.iter().map(InputMonitor::stats).collect();
            }
            for (i, m) in monitors.iter().enumerate() {
                let s = m.stats();
                fill_min[i] = fill_min[i].min(s.fill_frames);
                fill_max[i] = fill_max[i].max(s.fill_frames);
                fill_sum[i] += s.fill_frames as f64;
                ratio_min[i] = ratio_min[i].min(s.drift_ratio);
                ratio_max[i] = ratio_max[i].max(s.drift_ratio);
                ratio_sum[i] += s.drift_ratio;
            }
            fill_n += 1;
        }
    };

    loop {
        let (mut best, mut t_best) = (usize::MAX, f64::MAX);
        for (i, d) in in_devs.iter_mut().enumerate() {
            let t = d.when();
            if t < t_best {
                (best, t_best) = (i, t);
            }
        }
        if let Some(o) = out_dev.as_mut() {
            let t = o.when();
            if t < t_best {
                (best, t_best) = (n_in, t);
            }
        }
        if t_best > sim.secs {
            break;
        }
        if best < n_in {
            let d = &mut in_devs[best];
            let n = d.chunk();
            let stalled = d.paused(t_best);
            let w = TAU * sim.inputs[best].freq / d.clock.rate as f64;
            samples.clear();
            samples.extend((0..n).map(|k| (AMP * ((phase[best] + k) as f64 * w).sin()) as f32));
            phase[best] += n;
            d.advance();
            if !stalled {
                assert_eq!(producers[best].push(&samples), n, "ring overflow");
            }
            if out_dev.is_none() && best == 0 && !stalled {
                // Input-only: the first input's callback is the engine clock.
                duplex.advance(n);
                record(n, t_best, None);
            }
        } else {
            let o = out_dev.as_mut().unwrap();
            let n = o.chunk();
            let paused = o.paused(t_best);
            o.advance();
            if paused {
                continue;
            }
            buf.clear();
            buf.resize(n * out_ch, 0f32);
            duplex.render(&mut buf);
            record(n, t_best, Some(&buf));
        }
    }

    let start = check_index.unwrap_or(0);
    let max_residual = (0..n_in)
        .map(|i| {
            let y = &recorded[i];
            let w = TAU * sim.inputs[i].freq / sim.engine_rate as f64;
            let c2 = 2.0 * w.cos();
            (start.max(1)..y.len().saturating_sub(1))
                .map(|n| (y[n + 1] as f64 + y[n - 1] as f64 - c2 * y[n] as f64).abs())
                .fold(0.0, f64::max)
        })
        .collect();
    Report {
        stats: monitors.iter().map(InputMonitor::stats).collect(),
        stats_at_check,
        fill_min,
        fill_max,
        fill_avg: fill_sum.iter().map(|s| s / fill_n.max(1) as f64).collect(),
        ratio_min,
        ratio_max,
        ratio_avg: ratio_sum.iter().map(|s| s / fill_n.max(1) as f64).collect(),
        max_residual,
        delays,
        blocks: duplex.blocks(),
    }
}

fn clock(rate: u32, drift: f64, chunks: &[usize], jitter_ms: f64) -> Clock {
    Clock {
        rate,
        drift,
        chunks: chunks.to_vec(),
        jitter: jitter_ms / 1000.0,
        pause: None,
    }
}

fn mic(clock: Clock, freq: f64) -> InDev {
    InDev { clock, freq }
}

/// Asserts a clean steady state for input `i` and prints the measurements for the report.
fn assert_clean(name: &str, r: &Report, i: usize, in_rate: u32) {
    let (s, c) = (&r.stats[i], &r.stats_at_check[i]);
    let lat_ms = (r.fill_avg[i] / in_rate as f64 + r.delays[i] as f64 / 48000.0) * 1000.0;
    println!(
        "{name}[{i}]: fill {}..{} avg {:.0} frames (~{lat_ms:.2} ms incl. resampler), \
         ratio {:.6}..{:.6} avg {:.6}, residual {:.2e}, underflows {}->{}, skipped {}->{}",
        r.fill_min[i],
        r.fill_max[i],
        r.fill_avg[i],
        r.ratio_min[i],
        r.ratio_max[i],
        r.ratio_avg[i],
        r.max_residual[i],
        c.underflows,
        s.underflows,
        c.skipped_frames,
        s.skipped_frames
    );
    assert!(s.running, "{name}: input not running at end");
    assert_eq!(
        s.underflows, c.underflows,
        "{name}: underflow in steady state"
    );
    assert_eq!(
        s.skipped_frames, c.skipped_frames,
        "{name}: frames skipped in steady state"
    );
    assert_eq!(s.overflow_frames, 0, "{name}: ring overflow");
    assert!(
        r.max_residual[i] < 1e-3,
        "{name}: discontinuity, residual {}",
        r.max_residual[i]
    );
}

#[test]
fn drift_plus_minus_005_percent_three_minutes_jittery_callbacks() {
    for (d_in, d_out) in [(0.0005, -0.0005), (-0.0005, 0.0005)] {
        let r = run(Sim {
            inputs: vec![mic(
                clock(48000, d_in, &[96, 160, 128, 64, 192, 128], 1.0),
                440.0,
            )],
            output: Some(clock(48000, d_out, &[128, 256, 100, 156, 128], 1.0)),
            engine_rate: 48000,
            block: 128,
            secs: 180.0,
            check_from: 30.0,
        });
        assert_clean("drift", &r, 0, 48000);
        // The controller holds input/output frame rates equal: ratio = out clock / in clock on
        // average. It wobbles (sub-cent) as the push/pull phase of the two patterns beats.
        let want = (1.0 + d_out) / (1.0 + d_in);
        assert!(
            (r.ratio_avg[0] - want).abs() < 3e-5,
            "avg ratio {} want {want}",
            r.ratio_avg[0]
        );
        assert!((r.ratio_min[0] - want).abs() < 5e-4 && (r.ratio_max[0] - want).abs() < 5e-4);
        // Fill stays bounded: latency is jitter-limited, not drifting away.
        assert!(r.fill_max[0] < 1500, "fill grew to {}", r.fill_max[0]);
    }
}

#[test]
fn same_clock_converges_to_unity_ratio() {
    let c = clock(48000, 0.0001, &[128], 0.0);
    let r = run(Sim {
        inputs: vec![mic(c.clone(), 1000.0)],
        output: Some(c),
        engine_rate: 48000,
        block: 128,
        secs: 60.0,
        check_from: 20.0,
    });
    assert_clean("same-clock", &r, 0, 48000);
    assert!((r.ratio_min[0] - 1.0).abs() < 2e-5 && (r.ratio_max[0] - 1.0).abs() < 2e-5);
}

#[test]
fn input_44k1_to_engine_48k_with_drift() {
    let r = run(Sim {
        inputs: vec![mic(clock(44100, 0.0003, &[441, 220, 300], 1.0), 440.0)],
        output: Some(clock(48000, 0.0, &[256], 0.5)),
        engine_rate: 48000,
        block: 256,
        secs: 40.0,
        check_from: 20.0,
    });
    assert_clean("44k1->48k", &r, 0, 44100);
    assert!(r.delays[0] > 0);
}

#[test]
fn two_inputs_with_independent_clocks_stay_aligned_and_clean() {
    let r = run(Sim {
        inputs: vec![
            mic(clock(48000, 0.0004, &[64], 0.5), 440.0),
            mic(clock(44100, -0.0004, &[512, 256], 2.0), 660.0),
        ],
        output: Some(clock(48000, 0.0, &[128], 0.5)),
        engine_rate: 48000,
        block: 128,
        secs: 60.0,
        check_from: 25.0,
    });
    assert_clean("multi", &r, 0, 48000);
    assert_clean("multi", &r, 1, 44100);
}

#[test]
fn input_stall_feeds_silence_counts_and_recovers() {
    let r = run(Sim {
        inputs: vec![mic(
            Clock {
                pause: Some((10.0, 11.0)),
                ..clock(48000, 0.0002, &[128], 0.5)
            },
            440.0,
        )],
        output: Some(clock(48000, 0.0, &[128], 0.5)),
        engine_rate: 48000,
        block: 128,
        secs: 40.0,
        check_from: 20.0,
    });
    assert!(r.stats_at_check[0].underflows >= 1, "stall not counted");
    assert_clean("stall", &r, 0, 48000);
}

#[test]
fn output_stall_drops_backlog_instead_of_growing_latency() {
    // The output device stops calling back for 300 ms while the input keeps capturing.
    let r = run(Sim {
        inputs: vec![mic(clock(48000, 0.0, &[128], 0.5), 440.0)],
        output: Some(Clock {
            pause: Some((10.0, 10.3)),
            ..clock(48000, 0.0, &[128], 0.5)
        }),
        engine_rate: 48000,
        block: 128,
        secs: 30.0,
        check_from: 20.0,
    });
    assert!(
        r.stats_at_check[0].skipped_frames >= 10_000,
        "{:?}",
        r.stats_at_check[0]
    );
    // Latency returns to the pre-stall level instead of keeping the backlog.
    assert!(r.fill_max[0] < 400, "fill {}", r.fill_max[0]);
    assert_clean("out-stall", &r, 0, 48000);
}

#[test]
fn input_only_mode_engine_driven_by_input_callbacks() {
    let r = run(Sim {
        inputs: vec![mic(clock(48000, 0.0, &[256, 128], 1.0), 440.0)],
        output: None,
        engine_rate: 48000,
        block: 64,
        secs: 20.0,
        check_from: 5.0,
    });
    let s = r.stats[0];
    assert!(s.running && s.underflows == r.stats_at_check[0].underflows);
    // About 20 s of input at 48 kHz in blocks of 64.
    assert!(r.blocks >= 14_990, "{}", r.blocks);
    assert_clean("input-only", &r, 0, 48000);
}

#[test]
fn output_only_mode_runs_engine_without_inputs() {
    let mut calls = 0usize;
    let mut d = Duplex::new(
        |b: &mut AudioBlock<'_>| {
            assert!(b.inputs.is_empty());
            assert_eq!(b.frames, 32);
            b.outputs[1].fill(0.25);
            calls += 1;
        },
        48000,
        32,
        2,
    );
    let mut out = vec![0f32; 2 * 100];
    d.render(&mut out); // 100 frames -> 4 blocks, 28 frames left over
    d.render(&mut out[..2 * 28]); // consumes exactly the leftover
    assert_eq!(d.blocks(), 4);
    assert!(out.chunks(2).all(|f| f == [0.0, 0.25]));
    let mut out_i16 = vec![0i16; 2 * 8];
    d.render(&mut out_i16);
    assert_eq!(out_i16[1], (0.25 * 32768.0) as i16);
    drop(d);
    assert_eq!(calls, 5);
}

#[test]
fn external_input_stress_from_another_thread_keeps_frames_aligned() {
    // Producer thread pushes stereo frames (k, -k) in ragged chunks as fast as the ring allows;
    // the consumer renders on this thread. Whatever drops, skips or underflows happen, every
    // output frame must still pair a channel with its own negation and hold a pushed value.
    let mut d = Duplex::new(
        |b: &mut AudioBlock<'_>| {
            for (o, i) in b.outputs.iter_mut().zip(b.inputs.iter()) {
                o.copy_from_slice(i);
            }
        },
        48000,
        64,
        2,
    );
    let mut input = d.add_input(2, 48000, default_margin(48000));
    let monitor = input.monitor();
    let total: usize = 400_000;
    let producer = std::thread::spawn(move || {
        let mut k = 0usize;
        let mut sizes = [17usize, 64, 3, 128, 255, 1].iter().cycle();
        let mut frame = Vec::new();
        while k < total {
            let n = (*sizes.next().unwrap()).min(total - k);
            frame.clear();
            for j in 0..n {
                let v = ((k + j) % 1_000_000) as f32 / 1_000_000.0;
                frame.extend([v, -v]);
            }
            let mut off = 0;
            while off < n {
                off += input.push(&frame[off * 2..]);
                if off < n {
                    std::thread::yield_now();
                }
            }
            k += n;
        }
    });
    let mut out = vec![0f32; 2 * 96];
    let mut rendered = 0;
    while !producer.is_finished() || rendered < 50 {
        d.render(&mut out);
        rendered += 1;
        for f in out.chunks(2) {
            assert!((f[0] + f[1]).abs() < 1e-6, "channels misaligned: {f:?}");
            assert!((-0.1..1.1).contains(&f[0]), "garbage sample {f:?}");
        }
        std::thread::yield_now();
    }
    producer.join().unwrap();
    assert!(monitor.stats().pushes > 0);
}
