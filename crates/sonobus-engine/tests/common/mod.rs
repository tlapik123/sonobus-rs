//! Lockstep harness: engines in one process, each fed a sine per input channel, their
//! `process` called once per block on a real-time schedule (the "audio clock"), the last
//! half second of every output channel kept for analysis.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use sonobus_core::state::{AppState, ChannelGroup};
use sonobus_engine::{
    AudioConfig, Engine, EngineAudio, EngineConfig, Event, GroupJoin, PeerId, Snapshot,
};
use sonobus_files::{RecordFormat, RecordSpec, start_recording};

pub const SR: u32 = 48000;
pub const BLOCK: usize = 256;
pub const KEEP: usize = SR as usize / 2;
pub const AMP: f32 = 0.5;

pub struct Node {
    pub e: Engine,
    pub a: EngineAudio,
    freqs: Vec<f64>,
    phase: Vec<f64>,
    out: Vec<VecDeque<f32>>,
    /// RMS of output channel 0, one entry per block since the start.
    pub block_rms: Vec<f32>,
    pub events: Vec<Event>,
}

/// Input groups for `freqs.len()` input channels: `layout` gives (start, channels) per group.
pub fn groups(layout: &[(i32, i32)]) -> Vec<ChannelGroup> {
    layout
        .iter()
        .map(|&(start, n)| {
            let mut g = ChannelGroup::input_default(start as usize, false);
            g.num_channels = n;
            g.pans = vec![0.0; n as usize];
            g.limiter.enabled = false;
            g
        })
        .collect()
}

/// Test defaults: no monitoring of our own input, 40 ms jitter buffers.
pub fn state(input_groups: Vec<ChannelGroup>) -> AppState {
    let mut s = AppState::new(false);
    s.params.dry = 0.0;
    s.params.def_net_buf = 0.04;
    s.input_groups = input_groups;
    s
}

impl Node {
    pub fn new(freqs: &[f64], n_out: usize, state: AppState) -> Self {
        let audio = AudioConfig {
            sample_rate: SR,
            block_size: BLOCK,
            input_channels: freqs.len(),
            output_channels: n_out,
        };
        let (e, a) = Engine::new(EngineConfig {
            audio,
            udp_port: 0,
            state,
        })
        .expect("engine");
        Self {
            e,
            a,
            freqs: freqs.to_vec(),
            phase: vec![0.0; freqs.len()],
            out: vec![VecDeque::with_capacity(KEEP); n_out],
            block_rms: Vec::new(),
            events: Vec::new(),
        }
    }

    /// Simple mono-input node.
    pub fn mono(freq: f64) -> Self {
        Self::new(&[freq], 2, state(groups(&[(0, 1)])))
    }

    pub fn block(&mut self) {
        let ins: Vec<Vec<f32>> = self
            .freqs
            .iter()
            .zip(&mut self.phase)
            .map(|(&f, ph)| {
                (0..BLOCK)
                    .map(|_| {
                        let v = AMP * (*ph * std::f64::consts::TAU).sin() as f32;
                        *ph = (*ph + f / SR as f64).fract();
                        v
                    })
                    .collect()
            })
            .collect();
        let ins: Vec<&[f32]> = ins.iter().map(|v| &v[..]).collect();
        let mut outs = vec![vec![0.0f32; BLOCK]; self.out.len()];
        let mut o: Vec<&mut [f32]> = outs.iter_mut().map(|v| &mut v[..]).collect();
        self.a.process(&ins, &mut o);
        if let Some(b) = outs.first() {
            self.block_rms
                .push((b.iter().map(|x| x * x).sum::<f32>() / b.len() as f32).sqrt());
        }
        for (ring, blk) in self.out.iter_mut().zip(&outs) {
            for &x in blk {
                if ring.len() == KEEP {
                    ring.pop_front();
                }
                ring.push_back(x);
            }
        }
        while let Some(ev) = self.e.poll_event() {
            self.events.push(ev);
        }
    }

    /// Amplitude of a sine at `freq` in the last half second of output channel `ch`
    /// (Hann-windowed DFT bin).
    pub fn tone(&self, ch: usize, freq: f64) -> f32 {
        let x = &self.out[ch];
        let n = x.len();
        let (mut re, mut im, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
        for (i, &v) in x.iter().enumerate() {
            let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
            let a = std::f64::consts::TAU * freq * i as f64 / SR as f64;
            re += w * v as f64 * a.cos();
            im -= w * v as f64 * a.sin();
            wsum += w;
        }
        (2.0 * (re * re + im * im).sqrt() / wsum) as f32
    }

    pub fn snap(&self) -> Snapshot {
        self.e.snapshot()
    }

    pub fn peer(&self, user: &str) -> PeerId {
        self.snap()
            .peers
            .iter()
            .find(|p| p.user == user)
            .unwrap_or_else(|| panic!("no peer {user}"))
            .id
    }

    pub fn has(&self, f: impl Fn(&Event) -> bool) -> bool {
        self.events.iter().any(f)
    }

    pub fn join(&self, server: SocketAddr, user: &str, group: &str, password: &str) {
        let g = GroupJoin {
            name: group.into(),
            password: password.into(),
            public: false,
        };
        self.e
            .connect("127.0.0.1", server.port(), user, "", Some(g))
            .expect("connect");
    }

    /// The node's UDP address as peers on this host see it.
    pub fn addr(&self) -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], self.e.local_port()))
    }
}

/// Run every node's audio in lockstep at real-time pace for `secs`.
pub fn run(nodes: &mut [&mut Node], secs: f64) {
    run_until(nodes, secs, |_| false);
}

/// The lockstep loop fell this far behind its schedule: the machine was busy, and what the
/// engines measured around then (playout timing, underruns) is not the network's doing.
const STARVED: Duration = Duration::from_millis(10);
static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
/// When the loop last starved: ns since `EPOCH`, plus one (0: never).
static STARVED_AT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn ns(t: Instant) -> u64 {
    t.saturating_duration_since(*EPOCH.get_or_init(Instant::now))
        .as_nanos() as u64
}

/// A lockstep loop that meant to run at `next` runs now: note it if that is starving.
pub fn note_late(next: Instant) {
    let now = Instant::now();
    if now.saturating_duration_since(next) > STARVED {
        STARVED_AT.store(ns(now) + 1, std::sync::atomic::Ordering::Relaxed);
    }
}

/// The lockstep loop starved at `t` or later (`t`: the start of the test that asks).
pub fn starved_since(t: Instant) -> bool {
    let at = STARVED_AT.load(std::sync::atomic::Ordering::Relaxed);
    at > ns(t)
}

/// The lockstep loop starved within `d` before now.
pub fn starved_within(d: Duration) -> bool {
    let at = STARVED_AT.load(std::sync::atomic::Ordering::Relaxed);
    at > 0 && ns(Instant::now()).saturating_sub(at - 1) < d.as_nanos() as u64
}

/// Wait until `t`, the lockstep's next block. macOS CI runners oversleep `thread::sleep`
/// several-fold (20 ms took about 125 ms), which ran the audio clock at a fraction of real
/// time there; on macOS this yields until `t` instead.
pub fn sleep_until(t: Instant) {
    if cfg!(target_os = "macos") {
        while Instant::now() < t {
            std::thread::yield_now();
        }
    } else if let Some(d) = t.checked_duration_since(Instant::now()) {
        std::thread::sleep(d);
    }
}

/// Run until `done` holds (checked every block); false on timeout.
pub fn run_until(nodes: &mut [&mut Node], secs: f64, done: impl Fn(&[&mut Node]) -> bool) -> bool {
    let start = Instant::now();
    let block = Duration::from_secs_f64(BLOCK as f64 / SR as f64);
    let mut next = start;
    while start.elapsed().as_secs_f64() < secs {
        for n in nodes.iter_mut() {
            n.block();
        }
        if done(nodes) {
            return true;
        }
        next += block;
        match next.checked_duration_since(Instant::now()) {
            Some(_) => sleep_until(next),
            // late: carry on from now instead of catching up with a burst, like a device would
            None => {
                note_late(next);
                next = Instant::now();
            }
        }
    }
    false
}

/// Keep running until `cond` holds, checking every quarter second (a transient dropout on a
/// loaded test machine only delays it); false after `secs`.
pub fn eventually(nodes: &mut [&mut Node], secs: f64, cond: impl Fn(&[&mut Node]) -> bool) -> bool {
    let start = Instant::now();
    loop {
        run(nodes, 0.25);
        if cond(nodes) {
            return true;
        }
        if start.elapsed().as_secs_f64() > secs {
            return false;
        }
    }
}

/// Audio tests share the machine's real-time budget: run them one at a time.
pub fn serial() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    LOCK.lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Peer statistics for failure messages.
pub fn diag(n: &Node) -> String {
    n.snap()
        .peers
        .iter()
        .map(|p| {
            format!(
                "[{} recv={} safety={} buf={:.1}ms drop={} pkts={}]",
                p.user,
                p.recv_active,
                p.safety_muted,
                p.buffer_ms,
                p.packets_dropped,
                p.packets_received
            )
        })
        .collect()
}

/// Every node sees `n` peers whose receive is active.
pub fn all_receiving(nodes: &[&mut Node], n: usize) -> bool {
    nodes.iter().all(|x| {
        let s = x.snap();
        s.peers.len() == n && s.peers.iter().all(|p| p.recv_active && !p.safety_muted)
    })
}

/// Relative closeness.
pub fn near(got: f32, want: f32, tol: f32) -> bool {
    (got - want).abs() <= tol * want.abs()
}

/// A fresh directory for this test's files.
pub fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("sonobus-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A mono 16-bit WAV with a sine of `freq` at amplitude `AMP`.
pub fn sine_wav(dir: &Path, name: &str, freq: f64, secs: f64) -> PathBuf {
    let spec = RecordSpec {
        format: RecordFormat::Wav,
        bits: 16,
        sample_rate: SR,
        channels: 1,
    };
    let (mut w, r) = start_recording(&dir.join(name), spec).unwrap();
    let n = (secs * f64::from(SR)) as usize;
    let x: Vec<f32> = (0..n)
        .map(|i| AMP * (std::f64::consts::TAU * freq * i as f64 / f64::from(SR)).sin() as f32)
        .collect();
    for c in x.chunks(4096) {
        while !w.write(&[c]) {
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }
    r.stop().unwrap()
}

/// 16-bit PCM WAV channels.
pub fn read_wav(path: &Path) -> Vec<Vec<f32>> {
    let b = std::fs::read(path).unwrap();
    assert_eq!(&b[..4], b"RIFF", "{path:?}");
    let (mut pos, mut nch) = (12, 0usize);
    while pos + 8 <= b.len() {
        let len = u32::from_le_bytes(b[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body = &b[pos + 8..(pos + 8 + len).min(b.len())];
        match &b[pos..pos + 4] {
            b"fmt " => {
                nch = u16::from_le_bytes([body[2], body[3]]) as usize;
                assert_eq!(u16::from_le_bytes([body[14], body[15]]), 16);
            }
            b"data" => {
                let mut ch = vec![Vec::new(); nch];
                for f in body.chunks_exact(2 * nch) {
                    for (c, s) in f.as_chunks::<2>().0.iter().enumerate() {
                        ch[c].push(f32::from(i16::from_le_bytes(*s)) / 32768.0);
                    }
                }
                return ch;
            }
            _ => {}
        }
        pos += 8 + len + (len & 1);
    }
    panic!("no data in {path:?}")
}

/// Amplitude of `freq` in the half second of `x` ending `from_end` samples before its end.
pub fn tone_at(x: &[f32], freq: f64, from_end: usize) -> f32 {
    let end = x.len().saturating_sub(from_end);
    let x = &x[end.saturating_sub(SR as usize / 2)..end];
    let n = x.len().max(1);
    let (mut re, mut im, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
        let a = std::f64::consts::TAU * freq * i as f64 / f64::from(SR);
        re += w * f64::from(v) * a.cos();
        im -= w * f64::from(v) * a.sin();
        wsum += w;
    }
    (2.0 * (re * re + im * im).sqrt() / wsum.max(1.0)) as f32
}
