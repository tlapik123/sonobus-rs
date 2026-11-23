//! `EngineAudio::process` never allocates or frees: across a peer joining (new mixer strip,
//! stream halves, sink slot install on the first format), parameter snapshots, effects (EQ,
//! expander, compressor, monitor delay, input and main reverb), a send channel change (new
//! source half), a format change, a remote layout and the peer leaving; the stage-2 paths
//! (recording of every kind, file playback, soundboard voices, metronome synced to the file,
//! latency test with the echo service, patchbay, packet redundancy) starting and stopping;
//! and with another block size, channel counts and callbacks that are not whole blocks; and in
//! a modern (protocol v2) session.

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use common::*;
use sonobus_core::files::{SoundSample, Soundboard, Soundboards};
use sonobus_engine::{AudioConfig, Engine, EngineConfig};
use sonobus_files::{RecordFormat, RecordSpec, start_recording};

/// A mono WAV of a sine (written on the test thread, before arming).
fn wav(dir: &std::path::Path, name: &str, freq: f32) -> std::path::PathBuf {
    let spec = RecordSpec {
        format: RecordFormat::Wav,
        bits: 16,
        sample_rate: SR,
        channels: 1,
    };
    let (mut w, r) = start_recording(&dir.join(name), spec).unwrap();
    let x: Vec<f32> = (0..SR as usize)
        .map(|i| 0.3 * (i as f32 * freq * std::f32::consts::TAU / SR as f32).sin())
        .collect();
    for c in x.chunks(4096) {
        while !w.write(&[c]) {
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    r.stop().unwrap()
}

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|a| {
        if a.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

struct Counting;

// SAFETY: forwards every call unchanged to the system allocator; the bookkeeping only touches
// const-initialised thread-locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

struct Rig {
    input: Vec<Vec<f32>>,
    output: Vec<Vec<f32>>,
}

impl Rig {
    fn new() -> Self {
        let input = vec![(0..BLOCK).map(|i| 0.3 * (i as f32 * 0.07).sin()).collect(); 2];
        Self {
            input,
            output: vec![vec![0.0; BLOCK]; 2],
        }
    }

    /// One armed `process` per node, real-time paced.
    fn run(&mut self, nodes: &mut [&mut Node], secs: f64) {
        let start = Instant::now();
        let mut next = start;
        while start.elapsed().as_secs_f64() < secs {
            for n in nodes.iter_mut() {
                let [i0, i1] = &self.input[..] else {
                    unreachable!()
                };
                let ins: [&[f32]; 2] = [i0, i1];
                let [o0, o1] = &mut self.output[..] else {
                    unreachable!()
                };
                let mut outs: [&mut [f32]; 2] = [o0, o1];
                ARMED.with(|a| a.set(true));
                n.a.process(&ins, &mut outs);
                ARMED.with(|a| a.set(false));
                while n.e.poll_event().is_some() {}
            }
            next += Duration::from_secs_f64(BLOCK as f64 / SR as f64);
            match next.checked_duration_since(Instant::now()) {
                Some(_) => sleep_until(next),
                None => next = Instant::now(),
            }
        }
    }
}

#[test]
fn process_does_not_allocate() {
    let mut a = Node::new(&[440.0, 550.0], 2, state(groups(&[(0, 2)])));
    let mut b = Node::new(&[660.0, 770.0], 2, state(groups(&[(0, 1), (1, 1)])));
    let mut rig = Rig::new();
    rig.run(&mut [&mut a, &mut b], 0.2);
    a.e.connect_direct(b.addr());
    rig.run(&mut [&mut a, &mut b], 2.0);
    let snap = a.snap();
    assert_eq!(snap.peers.len(), 1);
    assert!(snap.peers[0].recv_active, "audio flowed: {snap:?}");
    let pb = snap.peers[0].id;

    a.e.update_state(|s| {
        s.params.dry = 0.7;
        s.params.wet = 0.8;
        s.params.main_reverb_enabled = true;
        s.params.met_enabled = true;
        let g = &mut s.input_groups[0];
        g.compressor.enabled = true;
        g.eq.enabled = true;
        g.expander.enabled = true;
        g.in_reverb_send = 0.4;
        g.mon_reverb_send = 0.3;
        g.monitor_delay.enabled = true;
        g.monitor_delay.delay_time_ms = 30.0;
    });
    a.e.set_peer_level(pb, 0.6);
    a.e.update_peer_groups(pb, |g, _| {
        g[0].eq.enabled = true;
        g[0].expander.enabled = true;
        g[0].mon_reverb_send = 0.5;
    });
    rig.run(&mut [&mut a, &mut b], 0.5);
    // a longer delay (the line is reused), input reverb off again
    a.e.update_state(|s| {
        s.input_groups[0].monitor_delay.delay_time_ms = 120.0;
        s.input_groups[0].in_reverb_send = 0.0;
    });
    rig.run(&mut [&mut a, &mut b], 0.3);
    // match inputs on b: a 2-channel send with a new layout; stereo on a: new source halves
    b.e.update_state(|s| s.params.send_channels = 0);
    a.e.update_state(|s| s.params.send_channels = 2);
    rig.run(&mut [&mut a, &mut b], 1.0);
    a.e.set_peer_send_format(pb, 9);
    a.e.update_peer_groups(pb, |g, solo| {
        g[0].muted = true;
        solo[0] = true;
    });
    rig.run(&mut [&mut a, &mut b], 1.0);
    assert_eq!(b.snap().peers[0].recv_format.as_deref(), Some("PCM 24 bit"));

    // stage 2: every recording kind, a looping file and a soundboard sample sent to the peer
    // with the metronome on the file's beat, a latency test, a patchbay route, redundancy
    let dir = std::env::temp_dir().join(format!("sonobus-rt-alloc-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let (file, sample) = (wav(&dir, "f.wav", 1000.0), wav(&dir, "s.wav", 1500.0));
    a.e.update_state(|s| {
        s.extra.default_recording_options = 15;
        s.extra.default_recording_format = 3;
        s.params.send_file_audio = true;
        s.params.send_soundboard_audio = true;
        s.params.sync_met_file = true;
        s.params.hear_latency_test = true;
    });
    a.e.start_recording(&dir, "rt").unwrap();
    a.e.load_file(&file).unwrap();
    a.e.set_file_looping(true);
    a.e.set_file_playing(true);
    a.e.set_soundboards(Soundboards {
        boards: vec![Soundboard {
            name: "b".into(),
            samples: vec![SoundSample {
                file_path: Some(sample.to_string_lossy().into_owned()),
                end_playback_behaviour: 1,
                ..SoundSample::default()
            }],
        }],
        ..Soundboards::default()
    });
    a.e.soundboard_press((0, 0)).unwrap();
    assert!(a.e.start_latency_test(pb));
    a.e.set_patch(pb, pb, true);
    a.e.set_packet_redundancy(2);
    a.e.set_met_beats_per_bar(4);
    rig.run(&mut [&mut a, &mut b], 1.5);
    assert!(a.e.recording_status().unwrap().elapsed_secs > 1.0);
    a.e.seek_file(0.2);
    a.e.set_patch(pb, pb, false);
    a.e.stop_latency_test(pb);
    a.e.stop_all_samples();
    a.e.set_file_playing(false);
    rig.run(&mut [&mut a, &mut b], 0.3);
    a.e.close_file();
    let files = a.e.stop_recording().unwrap();
    assert!(files.len() >= 4, "{files:?}");
    rig.run(&mut [&mut a, &mut b], 0.3);
    let _ = std::fs::remove_dir_all(&dir);

    a.e.remove_peer(pb);
    rig.run(&mut [&mut a, &mut b], 0.5);
    assert!(a.snap().peers.is_empty());
    assert_eq!(
        COUNT.with(|c| c.get()),
        0,
        "allocations on the audio thread"
    );
}

/// Block 128, one input, four outputs, callbacks of 100 and 300 frames (the one-block FIFO
/// path), a peer with effects.
#[test]
fn odd_sizes_and_channel_counts_do_not_allocate() {
    let mut st = state(groups(&[(0, 1)]));
    st.params.dry = 1.0;
    st.params.main_reverb_enabled = true;
    st.input_groups[0].eq.enabled = true;
    st.input_groups[0].in_reverb_send = 0.2;
    let (e, mut a) = Engine::new(EngineConfig {
        audio: AudioConfig {
            sample_rate: SR,
            block_size: 128,
            input_channels: 1,
            output_channels: 4,
        },
        udp_port: 0,
        state: st,
    })
    .unwrap();
    let mut b = Node::mono(660.0);
    e.connect_direct(b.addr());
    let input: Vec<f32> = (0..300).map(|i| 0.3 * (i as f32 * 0.05).sin()).collect();
    let mut out = vec![vec![0.0f32; 300]; 5];
    let start = Instant::now();
    let mut next = start;
    let mut k = 0usize;
    while start.elapsed() < Duration::from_secs(3) {
        let n = if k.is_multiple_of(2) { 100 } else { 300 };
        k += 1;
        let ins = [&input[..n]];
        let [o0, o1, o2, o3, o4] = &mut out[..] else {
            unreachable!()
        };
        // five device outputs for four configured
        let mut outs = [
            &mut o0[..n],
            &mut o1[..n],
            &mut o2[..n],
            &mut o3[..n],
            &mut o4[..n],
        ];
        ARMED.with(|a| a.set(true));
        a.process(&ins, &mut outs);
        ARMED.with(|a| a.set(false));
        while e.poll_event().is_some() {}
        if k.is_multiple_of(2) {
            b.block();
        }
        if k == 200 {
            let p = e.snapshot().peers[0].id;
            e.update_peer_groups(p, |g, _| {
                g[0].compressor.enabled = true;
                g[0].mon_reverb_send = 0.5;
            });
        }
        next += Duration::from_secs_f64(n as f64 / SR as f64);
        match next.checked_duration_since(Instant::now()) {
            Some(_) => sleep_until(next),
            None => next = Instant::now(),
        }
    }
    let snap = e.snapshot();
    assert!(snap.peers[0].recv_active, "audio flowed: {snap:?}");
    assert!(out[4].iter().all(|&x| x == 0.0));
    assert_eq!(
        COUNT.with(|c| c.get()),
        0,
        "allocations on the audio thread"
    );
}

/// A modern (protocol v2) session: peer join (receive half, shared send half), audio both
/// ways, a per-peer format outside the shared stream (private send half) and back, the shared
/// stream's format and channel count changing (new send half), induced loss (NACK and
/// redundancy recovery), a patchbay route, the peer leaving and rejoining, the session ending.
#[cfg(feature = "modern")]
#[test]
fn modern_session_process_does_not_allocate() {
    use sonobus_engine::modern::{HostOptions, NetOptions, RelayChoice};
    let net = |name: &str| NetOptions {
        name: name.into(),
        bind_ip: std::net::Ipv4Addr::LOCALHOST.into(),
        port: 0,
        relay: RelayChoice::None,
    };
    let mut a = Node::new(&[440.0, 550.0], 2, state(groups(&[(0, 2)])));
    let mut b = Node::new(&[660.0, 770.0], 2, state(groups(&[(0, 1), (1, 1)])));
    let mut rig = Rig::new();
    rig.run(&mut [&mut a, &mut b], 0.2);
    let link = a.e.host_session(&net("a"), HostOptions::new("rt")).unwrap().invite.unwrap();
    b.e.join_invite(&net("b"), &link).unwrap();
    rig.run(&mut [&mut a, &mut b], 2.0);
    let snap = a.snap();
    assert_eq!(snap.peers.len(), 1);
    assert!(snap.peers[0].recv_active, "audio flowed: {snap:?}");
    let pb = snap.peers[0].id;
    a.e.set_peer_send_format(pb, 9);
    rig.run(&mut [&mut a, &mut b], 1.0);
    assert_eq!(b.snap().peers[0].recv_format.as_deref(), Some("PCM 24 bit"));
    a.e.set_peer_send_format(pb, 4);
    a.e.set_send_format_for_all(8);
    a.e.update_state(|s| s.params.send_channels = 2);
    b.e.update_state(|s| s.params.send_channels = 0);
    rig.run(&mut [&mut a, &mut b], 1.0);
    assert_eq!(b.snap().peers[0].recv_format.as_deref(), Some("PCM 16 bit"));
    b.e.set_modern_receive_loss(40);
    rig.run(&mut [&mut a, &mut b], 2.0);
    b.e.set_modern_receive_loss(0);
    assert!(b.snap().peers[0].packets_resent > 0);
    a.e.set_patch(pb, pb, true);
    rig.run(&mut [&mut a, &mut b], 0.5);
    a.e.set_patch(pb, pb, false);
    b.e.leave_session();
    rig.run(&mut [&mut a, &mut b], 0.5);
    assert!(a.snap().peers.is_empty());
    b.e.join_invite(&net("b"), &link).unwrap();
    rig.run(&mut [&mut a, &mut b], 2.0);
    assert!(a.snap().peers.first().is_some_and(|p| p.recv_active));
    a.e.leave_session();
    rig.run(&mut [&mut a, &mut b], 0.5);
    assert_eq!(
        COUNT.with(|c| c.get()),
        0,
        "allocations on the audio thread"
    );
}
