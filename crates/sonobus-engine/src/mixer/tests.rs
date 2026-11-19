use super::*;
use sonobus_core::state::AppState;
use sonobus_dsp::gain::DEFAULT_CENTER_PAN_LAW as LAW;

const N: usize = 64;

fn cfg(n_in: usize, n_out: usize) -> AudioConfig {
    AudioConfig {
        sample_rate: 48000,
        block_size: N,
        input_channels: n_in,
        output_channels: n_out,
    }
}

fn group(start: i32, nch: i32) -> ChannelGroup {
    let mut g = ChannelGroup::input_default(start as usize, false);
    g.num_channels = nch;
    g.pans = vec![0.0; nch as usize];
    g.limiter.enabled = false;
    g
}

fn globals(groups: Vec<ChannelGroup>) -> Box<Globals> {
    let mut s = AppState::new(false);
    s.params.dry = 1.0;
    s.params.wet = 1.0;
    Box::new(Globals {
        params: s.params,
        input_solo: vec![false; groups.len()],
        input_groups: groups,
        metronome_group: s.metronome_group,
        file_group: s.file_playback_group,
        soundboard_group: s.soundboard_group,
        routed_to: [0; MAX_PEERS],
        beats_per_bar: 1,
    })
}

struct Rig {
    m: Mixer,
    garbage: Vec<Garbage>,
}

impl Rig {
    fn new(c: AudioConfig, g: Box<Globals>) -> Self {
        Self {
            m: Mixer::new(c, g, Arc::default()),
            garbage: Vec::new(),
        }
    }

    fn cmd(&mut self, c: Cmd) {
        let g = &mut self.garbage;
        self.m.apply(c, &mut |b| g.push(b));
    }

    fn update(&mut self, f: impl FnOnce(&mut Globals)) {
        let mut g = self.m.g.clone();
        f(&mut g);
        self.cmd(Cmd::Globals(g));
    }

    /// One block; `peer_in` fills each present peer's buffer with this value on channel 0.
    fn run(
        &mut self,
        inputs: &[Vec<f32>],
        n_out: usize,
        peer_in: &[(usize, f32)],
    ) -> Vec<Vec<f32>> {
        let ins: Vec<&[f32]> = inputs.iter().map(|v| &v[..]).collect();
        let mut outs = vec![vec![9.0f32; N]; n_out];
        self.m.process_input(&ins, N);
        for &(slot, v) in peer_in {
            if let Some(b) = self.m.peer_buffer(slot, N) {
                b[0].fill(v);
                self.m.process_peer(slot, N);
            }
        }
        let mut o: Vec<&mut [f32]> = outs.iter_mut().map(|v| &mut v[..]).collect();
        self.m.process_output(&mut o, N);
        outs
    }

    /// Runs `k` blocks and returns the last.
    fn settle(
        &mut self,
        inputs: &[Vec<f32>],
        n_out: usize,
        peer_in: &[(usize, f32)],
        k: usize,
    ) -> Vec<Vec<f32>> {
        (0..k)
            .map(|_| self.run(inputs, n_out, peer_in))
            .last()
            .unwrap()
    }
}

fn noise(seed: u32) -> Vec<f32> {
    let mut x = seed;
    (0..N)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 8) as f32 / (1u32 << 24) as f32 - 0.5
        })
        .collect()
}

#[test]
fn monitor_path_is_bit_exact_after_the_first_ramps() {
    let mut r = Rig::new(cfg(2, 2), globals(vec![group(0, 2)]));
    // block 1 ramps the group gain, pans, monitor level and dry/wet from 0
    let first = r.run(&[noise(1), noise(2)], 2, &[]);
    assert_ne!(first[0], noise(1));
    for k in 0..5 {
        let ins = [noise(10 + k), noise(20 + k)];
        let out = r.run(&ins, 2, &[]);
        assert_eq!(out[0], ins[0], "block {k}");
        assert_eq!(out[1], ins[1], "block {k}");
        // the send mix (standalone default: mono) of a stereo group: both channels summed
        // into one (one destination channel: no pan law, gain 1 / max(1, 2 * 0.5))
        let send = &r.m.send_mix()[0];
        for i in 0..N {
            assert!(
                (send[i] - (ins[0][i] + ins[1][i])).abs() < 1e-6,
                "block {k}"
            );
        }
        assert_eq!(r.m.send_mix()[1], vec![0.0; N]);
    }
}

#[test]
fn monitor_gain_pan_mute_solo_matrix() {
    let mut g0 = group(0, 1);
    g0.gain = 0.5;
    g0.pans[0] = 1.0; // hard right
    let g1 = group(1, 1); // centre
    let mut r = Rig::new(cfg(2, 2), globals(vec![g0, g1]));
    let ones = vec![1.0f32; N];
    let twos = vec![2.0f32; N];
    let out = r.settle(&[ones.clone(), twos.clone()], 2, &[], 3);
    // left: only group 1 at the centre law; right: group 0 at 0.5 + group 1 at the centre law
    assert_eq!(out[0], vec![2.0 * LAW; N]);
    assert_eq!(out[1], vec![0.5 + 2.0 * LAW; N]);

    // solo group 0: group 1 leaves the monitor
    r.update(|g| g.input_solo[0] = true);
    let out = r.settle(&[ones.clone(), twos.clone()], 2, &[], 3);
    assert_eq!(out[0], vec![0.0; N]);
    assert_eq!(out[1], vec![0.5; N]);

    // mute group 0 (solo stays): silence
    r.update(|g| g.input_groups[0].muted = true);
    let out = r.settle(&[ones.clone(), twos.clone()], 2, &[], 3);
    assert_eq!(out, vec![vec![0.0; N]; 2]);

    // main input mute silences every group; monitor level 0 (dry) too
    r.update(|g| {
        g.input_solo[0] = false;
        g.input_groups[0].muted = false;
        g.params.main_in_mute = true;
    });
    assert_eq!(
        r.settle(&[ones.clone(), twos.clone()], 2, &[], 3),
        vec![vec![0.0; N]; 2]
    );
    r.update(|g| {
        g.params.main_in_mute = false;
        g.params.dry = 0.0;
    });
    assert_eq!(r.settle(&[ones, twos], 2, &[], 3), vec![vec![0.0; N]; 2]);
}

#[test]
fn send_mix_modes() {
    let g0 = group(0, 1);
    let mut g1 = group(1, 1);
    g1.pans[0] = -1.0;
    let mut r = Rig::new(cfg(2, 2), globals(vec![g0, g1]));
    let ins = [vec![1.0f32; N], vec![0.5; N]];
    // mono (default standalone): both groups summed into channel 0, gain 1 / max(1, 2 * 0.5)
    r.settle(&ins, 2, &[], 3);
    assert_eq!(r.m.send_mix()[0], vec![1.5; N]);
    assert_eq!(r.m.send_mix()[1], vec![0.0; N]);
    // stereo: pan law per group
    r.update(|g| g.params.send_channels = 2);
    r.settle(&ins, 2, &[], 3);
    assert_eq!(r.m.send_mix()[0], vec![LAW + 0.5; N]);
    assert_eq!(r.m.send_mix()[1], vec![LAW; N]);
    // match inputs: straight copy of the processed inputs
    r.update(|g| g.params.send_channels = 0);
    r.settle(&ins, 2, &[], 3);
    assert_eq!(&r.m.send_mix()[..2], &ins[..]);
}

fn add_peer(r: &mut Rig, slot: usize, gain: f32) -> Arc<PeerShared> {
    let shared = Arc::new(PeerShared::default());
    shared.recv_active.store(true, Ordering::Relaxed);
    let mut g = ChannelGroup {
        num_channels: 1,
        ..Default::default()
    };
    g.pans = vec![0.0];
    let params = Box::new(PeerParams {
        gain,
        groups: vec![g],
        group_solo: vec![false],
        recv_channels: 1,
        ..Default::default()
    });
    let c = r.m.cfg;
    r.cmd(Cmd::AddPeer(slot, PeerMix::new(&c, params, shared.clone())));
    shared
}

#[test]
fn peer_level_mute_solo_and_safety() {
    let mut r = Rig::new(cfg(1, 2), globals(vec![group(0, 1)]));
    r.update(|g| g.params.dry = 0.0);
    let a = add_peer(&mut r, 0, 0.5);
    let b = add_peer(&mut r, 3, 1.0);
    let silent = [vec![0.0f32; N]];
    let feed = [(0, 1.0), (3, 0.25)];
    let out = r.settle(&silent, 2, &feed, 3);
    // the peer level is applied twice upstream: 0.5 * 0.5 * law, plus peer b at 0.25 * law
    let want = 0.25 * LAW + 0.25 * LAW;
    assert!((out[0][0] - want).abs() < 1e-6 && (out[1][N - 1] - want).abs() < 1e-6);
    let lv = a.meter.levels();
    assert_eq!(lv.peak, [0.5]); // post group gain

    // solo b: a is silent
    let mut p = (*r.m.peers[3].as_ref().unwrap().params).clone();
    p.soloed = true;
    r.cmd(Cmd::Peer(3, Box::new(p.clone())));
    let out = r.settle(&silent, 2, &feed, 3);
    assert!((out[0][0] - 0.25 * LAW).abs() < 1e-6);
    // b's group muted: nothing at all
    p.groups[0].muted = true;
    r.cmd(Cmd::Peer(3, Box::new(p)));
    assert_eq!(r.settle(&silent, 2, &feed, 3), vec![vec![0.0; N]; 2]);

    // receive inactive / safety muted
    r.cmd(Cmd::RemovePeer(3));
    a.recv_active.store(false, Ordering::Relaxed);
    assert_eq!(r.settle(&silent, 2, &feed, 3), vec![vec![0.0; N]; 2]);
    a.recv_active.store(true, Ordering::Relaxed);
    a.safety_muted.store(true, Ordering::Relaxed);
    assert_eq!(r.settle(&silent, 2, &feed, 3), vec![vec![0.0; N]; 2]);
    a.safety_muted.store(false, Ordering::Relaxed);
    assert!(r.settle(&silent, 2, &feed, 3)[0][0] > 0.0);
    // replaced snapshots and the removed peer went to the garbage list, not dropped in place
    assert_eq!(r.garbage.len(), 4);
    drop(b);
}

#[test]
fn output_gain_and_meters() {
    let mut r = Rig::new(cfg(2, 2), globals(vec![group(0, 2)]));
    r.update(|g| g.params.wet = 0.5);
    let ins = [vec![0.8f32; N], vec![-0.4; N]];
    let out = r.settle(&ins, 2, &[], 40);
    assert_eq!(out[0], vec![0.4; N]);
    assert_eq!(out[1], vec![-0.2; N]);
    let o = r.m.meters.output.levels();
    assert_eq!(o.peak, [0.4, 0.2]);
    assert!((o.rms[0] - 0.4).abs() < 1e-6 && (o.rms[1] - 0.2).abs() < 1e-6);
    let i = r.m.meters.input.levels();
    assert_eq!(i.peak, [0.8, 0.4]);
}

#[test]
fn metronome_is_heard_and_sent() {
    let mut r = Rig::new(cfg(1, 2), globals(vec![group(0, 1)]));
    r.update(|g| {
        g.params.met_enabled = true;
        g.params.send_met_audio = true;
        g.params.met_gain = 1.0;
    });
    r.update(|g| {
        g.params.met_gain = 0.5;
        g.params.met_tempo = 120.0;
    });
    let silent = [vec![0.0f32; N]];
    // 120 bpm at 48 kHz: a click every 24000 frames, from the first one (relative start 0)
    let (mut out, mut send) = (Vec::new(), Vec::new());
    for _ in 0..(24000 * 2 + 4000) / N {
        let o = r.run(&silent, 2, &[]);
        out.extend_from_slice(&o[0]);
        send.extend_from_slice(&r.m.send_mix()[0]);
    }
    let click = crate::wav::beat_click();
    assert!(click.len() < 20000 && click.iter().any(|x| x.abs() > 0.1));
    for beat in [0, 24000, 48000] {
        // sent: the click at the metronome gain (one destination channel, no pan law)
        for (i, c) in click.iter().enumerate().take(4000) {
            assert!(
                (send[beat + i] - 0.5 * c).abs() < 1e-6,
                "beat {beat} frame {i}"
            );
        }
        if beat > 0 {
            // heard: centre-panned, after the first block's gain ramp
            for (i, c) in click.iter().enumerate().take(4000) {
                assert!(
                    (out[beat + i] - 0.5 * LAW * c).abs() < 1e-6,
                    "beat {beat} frame {i}"
                );
            }
            assert!(
                out[beat - 1000..beat].iter().all(|&x| x == 0.0),
                "silence before {beat}"
            );
            assert!(send[beat - 1000..beat].iter().all(|&x| x == 0.0));
        }
    }
}

/// A peer stream full of NaN/Inf (32-bit float PCM passes them through) must not reach the
/// output or poison the main reverb it is sent to.
#[test]
fn non_finite_peer_audio_is_dropped_at_the_boundary() {
    let mut r = Rig::new(cfg(1, 2), globals(vec![group(0, 1)]));
    r.update(|g| {
        g.params.dry = 0.0;
        g.params.main_reverb_enabled = true;
    });
    let a = add_peer(&mut r, 0, 1.0);
    let mut p = (*r.m.peers[0].as_ref().unwrap().params).clone();
    p.groups[0].mon_reverb_send = 1.0;
    r.cmd(Cmd::Peer(0, Box::new(p)));
    let silent = [vec![0.0f32; N]];
    for (k, v) in [f32::NAN, f32::INFINITY, -f32::INFINITY, f32::MAX]
        .into_iter()
        .cycle()
        .take(200)
        .enumerate()
    {
        let out = r.run(&silent, 2, &[(0, v)]);
        assert!(out.iter().flatten().all(|x| x.is_finite()), "block {k}");
    }
    // clean audio again: finite, and the reverb still works (its tank is not stuck)
    let quiet = [(0, 0.05)];
    let out = r.settle(&silent, 2, &quiet, 100);
    assert!(out.iter().flatten().all(|x| x.is_finite()));
    assert!(
        r.m.main_fx[0].iter().any(|x| x.abs() > 1e-4),
        "reverb output"
    );
    let m = a.meter.levels();
    assert!(
        m.peak[0].is_finite() && (m.rms[0] - 0.05).abs() < 1e-4,
        "{m:?}"
    );
    let o = r.m.meters.output.levels();
    // (still loud: the clamped f32::MAX blocks went into the reverb at +40 dB)
    assert!(o.rms.iter().chain(&o.peak).all(|v| v.is_finite()), "{o:?}");
}

/// The last line of defence: a reverb whose state went non-finite anyway (white box) is
/// silenced at once and reset, and works again.
#[test]
fn poisoned_reverb_is_reset() {
    let mut r = Rig::new(cfg(1, 2), globals(vec![group(0, 1)]));
    r.update(|g| {
        g.params.main_reverb_enabled = true;
        g.input_groups[0].mon_reverb_send = 1.0;
    });
    let ins = [vec![0.25f32; N]];
    r.settle(&ins, 2, &[], 5);
    assert!(r.m.main_fx[0].iter().any(|x| x.abs() > 1e-4), "reverb runs");
    let mut nan = [vec![f32::NAN; N], vec![f32::NAN; N]];
    let [l, rr] = &mut nan;
    r.m.main_rev.process(l, rr);
    for _ in 0..3 {
        let out = r.run(&ins, 2, &[]);
        assert!(out.iter().flatten().all(|x| x.is_finite()));
    }
    let out = r.settle(&ins, 2, &[], 50);
    assert!(out.iter().flatten().all(|x| x.is_finite()));
    assert!(r.m.main_fx[0].iter().all(|x| x.is_finite()));
    assert!(r.m.main_fx[0].iter().any(|x| x.abs() > 1e-4), "reverb back");
}

#[test]
fn meters_ignore_non_finite_samples() {
    let out = Meter::default();
    let mut m = meter::MeterState::new(48000, N);
    let x = [f32::NAN, f32::INFINITY, 0.5, -0.25];
    m.measure(&[&x[..]], 1, 4, &out);
    let l = out.levels();
    assert_eq!(l.peak, [0.5]);
    assert!(l.rms[0].is_finite() && l.rms[0] < 0.5);
}

#[test]
fn effects_commit_only_on_change() {
    let mut d = group::GroupDsp::new(48000);
    let mut g = group(0, 1);
    d.commit(&g);
    // white box: a commit of unchanged parameters is a no-op; a change goes through
    assert!(d.committed_is(&g));
    g.compressor.threshold_db -= 3.0;
    assert!(!d.committed_is(&g));
    d.commit(&g);
    assert!(d.committed_is(&g));
}

/// Patchbay (P.cpp:7984-8018): peer 1's processed stream goes into peer 0's send; a stereo
/// stream stays left/right in a stereo send, a mono one goes to both channels, unrouted
/// peers get the plain send mix.
#[test]
fn patchbay_routes_processed_streams_into_a_peer_send() {
    let mut r = Rig::new(cfg(1, 2), globals(vec![group(0, 1)]));
    add_peer(&mut r, 0, 1.0);
    add_peer(&mut r, 1, 1.0);
    let mut stereo = ChannelGroup {
        num_channels: 2,
        ..Default::default()
    };
    stereo.pans = vec![-1.0, 1.0];
    let params = |groups: Vec<ChannelGroup>, recv, send| {
        Box::new(PeerParams {
            gain: 1.0,
            group_solo: vec![false; groups.len()],
            groups,
            recv_channels: recv,
            send_channels: send,
            ..Default::default()
        })
    };
    r.cmd(Cmd::Peer(0, params(vec![group(0, 1)], 1, 2)));
    r.cmd(Cmd::Peer(1, params(vec![stereo], 2, 1)));
    r.update(|g| g.routed_to[0] = 1 << 1);
    let block = |r: &mut Rig, src: [f32; 2]| {
        r.m.process_input(&[&[0.0; N][..]], N);
        if let Some(b) = r.m.peer_buffer(1, N) {
            b[0].fill(src[0]);
            b[1].fill(src[1]);
        }
        r.m.process_peer(1, N);
        r.m.peer_buffer(0, N);
        r.m.process_peer(0, N);
    };
    for _ in 0..4 {
        block(&mut r, [0.25, 0.5]);
    }
    let s = r.m.peer_send(0, N);
    assert!(
        (s[0][N - 1] - 0.25).abs() < 1e-6,
        "left stays left: {}",
        s[0][N - 1]
    );
    assert!(
        (s[1][N - 1] - 0.5).abs() < 1e-6,
        "right stays right: {}",
        s[1][N - 1]
    );
    // not routed: the plain send mix
    assert!(r.m.peer_send(1, N)[0].iter().all(|&x| x == 0.0));
    // a mono stream reaches both channels
    let mut mono = group(0, 1);
    mono.pans = vec![0.0];
    r.cmd(Cmd::Peer(1, params(vec![mono], 1, 1)));
    for _ in 0..4 {
        block(&mut r, [0.3, 0.0]);
    }
    let s = r.m.peer_send(0, N);
    assert!((s[0][N - 1] - 0.3).abs() < 1e-6 && (s[1][N - 1] - 0.3).abs() < 1e-6);
    // the route removed: nothing of peer 1 any more
    r.update(|g| g.routed_to[0] = 0);
    block(&mut r, [0.3, 0.0]);
    assert!(r.m.peer_send(0, N)[0].iter().all(|&x| x == 0.0));
}

/// The file playback and soundboard monitors go through their group's monitor delay, as
/// upstream's (P.cpp:1709-1719, SoundboardChannelProcessor.cpp:258-265). A mono file of a
/// constant: the output onset moves by the delay.
#[test]
fn file_and_soundboard_monitors_are_delayed() {
    use sonobus_files::{RecordFormat, RecordSpec, open_player, start_recording};
    let dir = std::env::temp_dir().join(format!("sb-mixdelay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("dc.wav");
    let _ = std::fs::remove_file(&path);
    let spec = RecordSpec {
        format: RecordFormat::Wav,
        bits: 16,
        sample_rate: 48000,
        channels: 1,
    };
    let (mut w, rec) = start_recording(&path, spec).unwrap();
    assert!(w.write(&[&vec![0.5f32; 24000][..]]));
    let path = rec.stop().unwrap();
    let deck = || {
        let (player, out) = open_player(&path, 48000).unwrap();
        // let the decoder fill its ring
        std::thread::sleep(std::time::Duration::from_millis(200));
        let d = Box::new(Deck {
            out,
            playing: Arc::new(AtomicBool::new(true)),
            sample_rate: 48000,
        });
        (player, d)
    };
    let delay = 10.0f32; // 480 samples
    let onset = |target: Option<DelayTarget>, voice: bool| -> usize {
        let mut g = globals(vec![group(0, 1)]);
        for grp in [&mut g.file_group, &mut g.soundboard_group] {
            grp.monitor_delay.enabled = target.is_some();
            grp.monitor_delay.delay_time_ms = delay;
            grp.num_channels = if voice { 2 } else { 1 };
        }
        let mut r = Rig::new(cfg(1, 2), g);
        if let Some(t) = target {
            let ch = if voice { SB_CH } else { 1 };
            r.cmd(Cmd::Delay(t, Box::new(MonitorDelay::new(48000, ch)), ch));
        }
        let (_player, d) = deck();
        r.cmd(if voice { Cmd::Voice(0, Some(d)) } else { Cmd::File(Some(d)) });
        let silent = vec![0.0f32; N];
        let mut out = Vec::new();
        for _ in 0..30 {
            out.extend(r.run(std::slice::from_ref(&silent), 2, &[]).swap_remove(0));
        }
        out.iter().position(|v| v.abs() > 1e-4).expect("heard")
    };
    let (plain, delayed) = (onset(None, false), onset(Some(DelayTarget::File), false));
    // (the first block's gain ramps start at 0, the delayed one's fade sits inside the line)
    let near = |d: usize| (470..=482).contains(&d);
    assert!(near(delayed - plain), "file: {plain} -> {delayed}");
    let (plain, delayed) = (onset(None, true), onset(Some(DelayTarget::Soundboard), true));
    assert!(near(delayed - plain), "soundboard: {plain} -> {delayed}");
    std::fs::remove_file(path).unwrap();
}

/// Review finding 8: on a mono output the heard latency-test return must land on channel 0
/// (the links put it on channel 1 only when the bus they get has two).
#[test]
fn latency_monitor_bus_follows_the_output() {
    for (n_out, want) in [(1, 1), (2, 2), (6, 6)] {
        let mut r = Rig::new(cfg(1, n_out), globals(vec![group(0, 1)]));
        assert!(r.m.latency_monitor().is_none());
        r.update(|g| g.params.hear_latency_test = true);
        assert_eq!(r.m.latency_monitor().map(|b| b.len()), Some(want), "{n_out}");
    }
}
