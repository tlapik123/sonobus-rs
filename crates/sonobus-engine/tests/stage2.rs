//! Stage 2 end to end on 127.0.0.1: recording, file playback, soundboard, latency test and
//! echo, latency match / receive sync, suggest group, patchbay, peer cache, packet redundancy.

mod common;

use common::*;
use sonobus_core::files::{SoundSample, Soundboard, Soundboards};
use sonobus_dsp::gain::DEFAULT_CENTER_PAN_LAW as LAW;
use sonobus_engine::{Notification, SuggestGroup};

const CENTRE: f32 = AMP * LAW;

fn server() -> aoo::server::ServerHandle {
    aoo::server::run("127.0.0.1:0".parse().unwrap()).expect("server")
}

fn connect_all(srv: std::net::SocketAddr, nodes: &mut [&mut Node], group: &str) {
    for (i, n) in nodes.iter().enumerate() {
        n.join(srv, &format!("user{i}"), group, "pw");
    }
    let count = nodes.len() - 1;
    assert!(
        run_until(nodes, 15.0, |n| all_receiving(n, count)),
        "peers did not connect"
    );
}

fn notes(n: &Node) -> Vec<Notification> {
    std::iter::from_fn(|| n.e.poll_notification()).collect()
}

/// Run until `n` has a notification matching `f`; returns it.
fn wait_note(
    nodes: &mut [&mut Node],
    who: usize,
    secs: f64,
    f: impl Fn(&Notification) -> bool,
) -> Option<Notification> {
    let mut found = None;
    let start = std::time::Instant::now();
    while found.is_none() && start.elapsed().as_secs_f64() < secs {
        run(nodes, 0.1);
        found = notes(nodes[who]).into_iter().find(|x| f(x));
    }
    found
}

#[test]
fn recording_mix_minus_self_users_and_a_rejoin() {
    let _serial = serial();
    let srv = server();
    let mut st = state(groups(&[(0, 1)]));
    st.extra.default_recording_options = 15;
    st.extra.default_recording_format = 3; // WAV
    let mut a = Node::new(&[440.0], 2, st);
    let mut b = Node::mono(660.0);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "rec");
    let ok = eventually(&mut [&mut a, &mut b], 4.0, |n| {
        near(n[0].tone(0, 660.0), CENTRE, 0.15)
    });
    assert!(ok, "a hears b: {} {}", a.tone(0, 660.0), diag(&a));
    let dir = tmp("rec");
    let folder = a.e.start_recording(&dir, "take").unwrap();
    assert_eq!(folder, dir.join("take"));
    run(&mut [&mut a, &mut b], 1.5);
    assert!(b.snap().peers[0].remote_recording, "rec flag in /sb/pinfo");

    // b drops out and comes back: a second segment for it
    b.e.leave_group();
    assert!(run_until(&mut [&mut a, &mut b], 5.0, |n| n[0]
        .snap()
        .peers
        .is_empty()));
    run(&mut [&mut a, &mut b], 0.3);
    b.e.join_group("rec", "pw", false);
    assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| all_receiving(
        n, 1
    )));
    run(&mut [&mut a, &mut b], 1.5);

    let status = a.e.recording_status().expect("recording");
    let files = a.e.stop_recording().unwrap();
    let elapsed = (status.elapsed_secs * f64::from(SR)).round() as usize;
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    for want in [
        "take-MIXMINUS.wav",
        "take-SELF.wav",
        "take-MIX.wav",
        "take-user1.wav",
        "take-user1(2).wav",
    ] {
        assert!(names.iter().any(|n| n == want), "{want} in {names:?}");
    }
    let file = |n: &str| read_wav(&folder.join(n));
    let (mix, minus, own) = (
        file("take-MIX.wav"),
        file("take-MIXMINUS.wav"),
        file("take-SELF.wav"),
    );
    // the mix: our monitored input (monitoring off: in full) and the peer, both centred
    assert!(
        near(tone_at(&mix[0], 440.0, 0), CENTRE, 0.15),
        "{}",
        tone_at(&mix[0], 440.0, 0)
    );
    assert!(
        near(tone_at(&mix[1], 660.0, 0), CENTRE, 0.15),
        "{}",
        tone_at(&mix[1], 660.0, 0)
    );
    // mix minus: the peer only
    assert!(tone_at(&minus[0], 440.0, 0) < 0.01);
    assert!(near(tone_at(&minus[0], 660.0, 0), CENTRE, 0.15));
    // self: our raw input
    assert_eq!(own.len(), 1);
    assert!(near(tone_at(&own[0], 440.0, 0), AMP, 0.05) && tone_at(&own[0], 660.0, 0) < 0.01);
    // lengths: what the audio thread counted (no block ran between status and stop)
    for (name, f) in [("mix", &mix), ("minus", &minus), ("self", &own)] {
        assert!(
            f[0].len().abs_diff(elapsed) <= BLOCK,
            "{name}: {} vs {elapsed}",
            f[0].len()
        );
    }
    // each segment of the peer holds its raw stream; the first ends when it left, the
    // second starts with silence up to its rejoin, so it ends with the recording
    let (first, second) = (file("take-user1.wav"), file("take-user1(2).wav"));
    for (seg, u) in [("first", &first), ("second", &second)] {
        assert!(near(tone_at(&u[0], 660.0, 0), AMP, 0.1), "{seg}");
        assert!(tone_at(&u[0], 440.0, 0) < 0.01, "{seg}");
    }
    assert!(first[0].len() < elapsed - SR as usize);
    assert!(
        second[0].len().abs_diff(elapsed) <= BLOCK,
        "aligned: {} vs {elapsed}",
        second[0].len()
    );
    // silent until b came back (it left 1.5 s in)
    assert!(second[0][..SR as usize * 3 / 2].iter().all(|&x| x == 0.0));
    // the latency sidecar: the peer at the start and at the stop
    let side = std::fs::read_to_string(folder.join("take-latency.txt")).unwrap();
    let lines: Vec<&str> = side.lines().collect();
    assert_eq!(lines.len(), 2, "{side}");
    assert!(lines[0].starts_with("start\tuser1\t") && lines[1].starts_with("stop\tuser1\t"));
    run(&mut [&mut a, &mut b], 0.5);
    assert!(!b.snap().peers[0].remote_recording, "flag cleared");
    assert!(
        notes(&a)
            .iter()
            .any(|n| matches!(n, Notification::RecordingStopped { error: None, .. }))
    );
}

#[test]
fn file_playback_local_and_to_the_peer_seek_loop_and_end() {
    let _serial = serial();
    let srv = server();
    let dir = tmp("file");
    let wav = sine_wav(&dir, "tone.wav", 1000.0, 3.0);
    let mut st = state(groups(&[(0, 1)]));
    st.params.send_file_audio = true;
    let mut a = Node::new(&[440.0], 2, st);
    let mut b = Node::mono(660.0);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "file");
    let info = a.e.load_file(&wav).unwrap();
    assert_eq!((info.channels, info.sample_rate), (1, SR));
    assert!((info.length_secs.unwrap() - 3.0).abs() < 1e-3);
    a.e.set_file_looping(true);
    a.e.set_file_playing(true);
    // monitored locally (file group centred) and sent to the peer (mono send)
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| near(
        n[0].tone(0, 1000.0),
        CENTRE,
        0.15
    ) && near(
        n[1].tone(0, 1000.0),
        CENTRE,
        0.2
    )));
    // seek
    a.e.seek_file(2.0);
    let p = a.e.file_status().unwrap().position_secs;
    assert!((p - 2.0).abs() < 0.01, "{p}");
    // a loop range keeps the position inside it (after the read-ahead)
    a.e.set_file_loop_range(0.5, 0.25);
    run(&mut [&mut a, &mut b], 2.5);
    let p = a.e.file_status().unwrap().position_secs;
    assert!((0.49..0.80).contains(&p), "{p}");
    // no loop: plays to the end, stops, returns to the start
    a.e.set_file_loop_range(0.0, 0.0);
    a.e.set_file_looping(false);
    let end = wait_note(&mut [&mut a, &mut b], 0, 8.0, |n| {
        *n == Notification::FileFinished
    });
    assert!(end.is_some(), "file ended");
    let s = a.e.file_status().unwrap();
    assert!(!s.playing && s.position_secs < 0.01, "{s:?}");
    run(&mut [&mut a, &mut b], 0.6);
    assert!(
        b.tone(0, 1000.0) < 0.01 && a.tone(0, 1000.0) < 0.01,
        "silent when stopped"
    );
    a.e.close_file();
    assert!(a.e.file_status().is_none());
}

#[test]
fn soundboard_sample_heard_locally_and_by_the_peer() {
    let _serial = serial();
    let srv = server();
    let dir = tmp("sb");
    let wav = sine_wav(&dir, "s.wav", 1500.0, 1.0);
    let mut st = state(groups(&[(0, 1)]));
    st.params.send_soundboard_audio = true;
    let mut a = Node::new(&[440.0], 2, st);
    let mut b = Node::mono(660.0);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "sb");
    a.e.set_soundboards(Soundboards {
        boards: vec![Soundboard {
            name: "fx".into(),
            samples: vec![SoundSample {
                name: "tone".into(),
                file_path: Some(wav.to_string_lossy().into_owned()),
                end_playback_behaviour: 1, // loop
                ..SoundSample::default()
            }],
        }],
        ..Soundboards::default()
    });
    a.e.soundboard_press((0, 0)).unwrap();
    // a mono sample plays on both sides of the stereo soundboard bus: centre-panned left
    // and right, summed into the mono send at 1/2 each
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| n[0]
        .tone(0, 1500.0)
        > 0.2
        && n[1].tone(0, 1500.0) > 0.2));
    assert_eq!(a.e.soundboard_status().len(), 1);
    a.e.stop_all_samples();
    assert!(eventually(&mut [&mut a, &mut b], 3.0, |n| n[1]
        .tone(0, 1500.0)
        < 0.01));
    assert!(a.e.soundboard_status().is_empty());
}

/// Our latency source -> their echo sink (their 40 ms buffer) -> their echo source -> our
/// latency sink (our 40 ms buffer) -> measurer. Expected: the two jitter buffers (80 ms) plus
/// one or two blocks of 5.33 ms (block quantisation of the hops, the measurer's own block):
/// ~85-96 ms (measured here: 90.4 ms). The engine's estimate for the same path is
/// 2 x (Opus 2.5 + remote block 5.33 + ping/2 + 40) = ~96 ms. Accepted: 82..120 ms, and the
/// estimate within 15 ms of the measurement.
#[test]
fn latency_test_between_two_engines() {
    let _serial = serial();
    let srv = server();
    let mut st_a = state(groups(&[(0, 1)]));
    st_a.params.def_net_auto = 0;
    let mut st_b = st_a.clone();
    st_b.input_groups = groups(&[(0, 1)]);
    let mut a = Node::new(&[440.0], 2, st_a);
    let mut b = Node::new(&[660.0], 2, st_b);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "lat");
    let pb = a.peer("user1");
    assert!(a.e.start_latency_test(pb));
    let got = wait_note(&mut [&mut a, &mut b], 0, 8.0, |n| {
        matches!(n, Notification::LatencyMeasured { .. })
    });
    let Some(Notification::LatencyMeasured {
        id,
        round_trip_ms,
        estimated_ms,
    }) = got
    else {
        panic!("no measurement: {}", diag(&a))
    };
    eprintln!("latency test: measured {round_trip_ms:.1} ms, estimated {estimated_ms:.1} ms");
    assert_eq!(id, pb);
    assert!((82.0..120.0).contains(&round_trip_ms), "{round_trip_ms}");
    assert!(
        (round_trip_ms - estimated_ms).abs() < 15.0,
        "{estimated_ms}"
    );
    let p = &a.snap().peers[0];
    assert!(p.latency_test_active && p.measured_round_trip_ms.is_some());
    a.e.stop_latency_test(pb);
    assert!(!a.snap().peers[0].latency_test_active);
    // the main streams were not disturbed
    run(&mut [&mut a, &mut b], 0.6);
    assert!(near(b.tone(0, 440.0), CENTRE, 0.15));
}

#[test]
fn latency_match_receive_sync_suggestions() {
    let _serial = serial();
    let srv = server();
    let mut st = state(groups(&[(0, 1)]));
    st.params.def_net_auto = 0;
    let mut a = Node::new(&[440.0], 2, st.clone());
    let mut b = Node::new(&[660.0], 2, st);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "match");
    run(&mut [&mut a, &mut b], 2.5); // a few pings for the estimate
    let pb = a.peer("user1");

    // receive sync with one peer: its buffer gets exactly the padding; repeating changes nothing
    let buf = |n: &Node| n.snap().peers[0].buffer_ms;
    assert!((buf(&a) - 40.0).abs() < 0.01);
    a.e.receive_sync(10.0);
    assert!((buf(&a) - 50.0).abs() < 0.01, "{}", buf(&a));
    a.e.receive_sync(10.0);
    assert!((buf(&a) - 50.0).abs() < 0.01, "idempotent: {}", buf(&a));
    // the padding is remembered (and persisted with the state)
    assert_eq!(a.e.receive_sync_padding(), 10.0);
    a.e.set_receive_sync_padding(900.0);
    assert_eq!(a.e.receive_sync_padding(), 500.0);
    assert!((buf(&a) - 50.0).abs() < 0.01, "setting it does not sync");
    a.e.set_receive_sync_padding(10.0);
    assert!(
        a.e.state()
            .extra
            .unknown
            .iter()
            .any(|(k, v)| k == "ReceiveSyncPadding" && v.to_f64() == 10.0)
    );

    // commit a match 20 ms above the current incoming latency: the buffer grows by 20 and the
    // incoming latency lands on the target
    let target = a.snap().peers[0].latency.incoming_ms + 20.0;
    a.e.commit_latency_match(target);
    assert!((buf(&a) - 70.0).abs() < 0.05, "{}", buf(&a));
    let inc = a.snap().peers[0].latency.incoming_ms;
    assert!((inc - target).abs() < 0.5, "{inc} vs {target}");
    // below the current latency: nothing changes
    a.e.commit_latency_match(1.0);
    assert!((buf(&a) - 70.0).abs() < 0.05);

    // the table: our entry about user1 and theirs about user0
    a.e.begin_latency_match();
    assert!(
        wait_note(&mut [&mut a, &mut b], 0, 3.0, |n| *n
            == Notification::LatencyMatchReady)
        .is_some()
    );
    let (entries, ready, max) = a.e.latency_match_info();
    assert!(ready && entries.len() == 2, "{entries:?}");
    assert!(
        entries
            .iter()
            .any(|e| e.src_name == "user1" && e.dest_name == "user0")
    );
    assert!(
        entries
            .iter()
            .any(|e| e.src_name == "user0" && e.dest_name == "user1")
    );
    assert_eq!(
        max,
        entries
            .iter()
            .map(|e| e.latency_ms as f32)
            .fold(0.0, f32::max)
    );

    // /sb/suggestlat and /sb/suggestgroup reach the peer as notifications
    a.e.send_latency_match(77.0);
    let want = Notification::LatencyMatchRequested {
        from: "user0".into(),
        latency_ms: 77.0,
    };
    assert!(wait_note(&mut [&mut a, &mut b], 1, 3.0, |n| *n == want).is_some());
    a.e.suggest_group("elsewhere", "pw2", true, &["user1"]);
    let want = Notification::GroupSuggested(SuggestGroup {
        user: "user0".into(),
        group: "elsewhere".into(),
        group_password: "pw2".into(),
        public: true,
        others: vec!["user1".into()],
    });
    assert!(wait_note(&mut [&mut a, &mut b], 1, 3.0, |n| *n == want).is_some());
    let _ = pb;
}

/// Patchbay on a: user1's audio also goes to user2. user2 mutes user1's direct stream and
/// still hears it, through a.
#[test]
fn patchbay_routes_one_peer_to_another() {
    let _serial = serial();
    let srv = server();
    let (mut a, mut b, mut c) = (Node::mono(440.0), Node::mono(660.0), Node::mono(880.0));
    connect_all(srv.local_addr(), &mut [&mut a, &mut b, &mut c], "patch");
    let b_on_c = c.peer("user1");
    c.e.set_peer_recv_muted(b_on_c, true);
    assert!(eventually(&mut [&mut a, &mut b, &mut c], 4.0, |n| n[2]
        .tone(0, 660.0)
        < 0.01));
    let (pb, pc) = (a.peer("user1"), a.peer("user2"));
    a.e.set_patch(pb, pc, true);
    assert!(a.e.patch(pb, pc) && !a.e.patch(pc, pb));
    let to_c = |n: &Node| {
        n.snap()
            .peers
            .iter()
            .find(|p| p.user == "user2")
            .unwrap()
            .send_channels
    };
    assert_eq!(to_c(&a), 2, "a routed peer gets our output channel count");
    assert!(
        eventually(&mut [&mut a, &mut b, &mut c], 5.0, |n| n[2].tone(0, 660.0)
            > 0.1),
        "c hears b through a: {}",
        c.tone(0, 660.0)
    );
    a.e.set_patch(pb, pc, false);
    assert_eq!(to_c(&a), 1);
    // the per-peer override wins over the send mode (and is capped at our outputs)
    a.e.set_peer_send_channels_override(pc, Some(8));
    assert_eq!(to_c(&a), 2);
    a.e.set_peer_send_channels_override(pc, None);
    assert_eq!(to_c(&a), 1);
    assert!(eventually(&mut [&mut a, &mut b, &mut c], 4.0, |n| n[2]
        .tone(0, 660.0)
        < 0.01));
}

#[test]
fn peer_cache_restores_a_rejoining_user() {
    let _serial = serial();
    let srv = server();
    let mut st = state(groups(&[(0, 1)]));
    st.params.def_net_auto = 0;
    let mut a = Node::new(&[440.0], 2, st);
    let mut b = Node::mono(660.0);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "cache");
    let pb = a.peer("user1");
    a.e.set_peer_level(pb, 0.3);
    a.e.set_peer_buffer_ms(pb, 70.0);
    a.e.set_peer_send_format(pb, 8);
    a.e.set_peer_soloed(pb, true);
    run(&mut [&mut a, &mut b], 0.3);
    b.e.leave_group();
    assert!(run_until(&mut [&mut a, &mut b], 5.0, |n| n[0]
        .snap()
        .peers
        .is_empty()));
    b.e.join_group("cache", "pw", false);
    assert!(run_until(&mut [&mut a, &mut b], 15.0, |n| n[0]
        .snap()
        .peers
        .len()
        == 1));
    let p = &a.snap().peers[0];
    assert_eq!((p.user.as_str(), p.send_format), ("user1", 8));
    assert!((p.level - 0.3).abs() < 1e-6 && (p.buffer_ms - 70.0).abs() < 1e-3);
    assert!(p.soloed, "solo remembered (PR #291)");
    let s = a.e.state();
    assert!(
        s.peer_cache
            .iter()
            .any(|c| c.name == "user1" && c.send_format == 8)
    );
}

/// PR #202: three copies of every packet; the Rust sink keeps playing them as one stream.
#[test]
fn packet_redundancy_multiplies_packets() {
    let _serial = serial();
    let srv = server();
    let (mut a, mut b) = (Node::mono(440.0), Node::mono(660.0));
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "red");
    let sent = |n: &Node| n.snap().peers[0].packets_sent;
    let rate = |a: &mut Node, b: &mut Node| {
        let s0 = sent(a);
        run(&mut [a, b], 1.0);
        sent(a) - s0
    };
    let one = rate(&mut a, &mut b);
    a.e.set_packet_redundancy(3);
    assert_eq!(a.e.packet_redundancy(), 3);
    run(&mut [&mut a, &mut b], 0.2);
    let three = rate(&mut a, &mut b);
    assert!(
        (2.5..3.5).contains(&(three as f64 / one as f64)),
        "{one} -> {three}"
    );
    assert!(
        near(b.tone(0, 440.0), CENTRE, 0.15),
        "still one clean stream"
    );
    assert_eq!(
        a.e.state()
            .extra
            .unknown
            .iter()
            .find(|(k, _)| k == "PacketRedundancy")
            .map(|(_, v)| v.to_i32()),
        Some(3)
    );
}

/// The disk fills up mid-recording (injected through `sonobus-files`' test hook): the error
/// is reported, the recording stops by itself, the session keeps running, and
/// `stop_recording` returns the error instead of panicking.
#[test]
fn recording_disk_full() {
    let _serial = serial();
    let srv = server();
    let mut st = state(groups(&[(0, 1)]));
    st.extra.default_recording_options = 15;
    st.extra.default_recording_format = 1; // FLAC
    let mut a = Node::new(&[440.0], 2, st);
    let mut b = Node::mono(660.0);
    connect_all(srv.local_addr(), &mut [&mut a, &mut b], "full");
    let dir = tmp("full");
    a.e.start_recording(&dir, "take").unwrap();
    run(&mut [&mut a, &mut b], 0.5);
    sonobus_files::testing::fail_writes(true);
    let err = wait_note(&mut [&mut a, &mut b], 0, 5.0, |n| {
        matches!(n, Notification::RecordingError { .. })
    });
    sonobus_files::testing::fail_writes(false);
    let Some(Notification::RecordingError { error }) = err else {
        panic!("no RecordingError")
    };
    assert!(error.contains("no space"), "{error}");
    assert!(a.e.recording_status().is_none(), "stopped by itself");
    let e = a.e.stop_recording().expect_err("the failure");
    assert!(e.to_string().contains("no space"), "{e}");
    assert_eq!(
        a.e.stop_recording().unwrap(),
        Vec::<std::path::PathBuf>::new(),
        "reported once"
    );
    // still in session, and it can record again
    assert!(eventually(&mut [&mut a, &mut b], 4.0, |n| near(
        n[0].tone(0, 660.0),
        CENTRE,
        0.15
    )));
    a.e.start_recording(&dir, "again").unwrap();
    run(&mut [&mut a, &mut b], 0.5);
    assert!(!a.e.stop_recording().unwrap().is_empty());
}

/// Review finding 5: the audio thread runs on its own thread (as with a real device) while a
/// recording stops; every file of the recording must end on the same block, also when the
/// stop's commands reach the audio thread late.
#[test]
fn recording_stop_with_a_running_audio_thread() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    let _serial = serial();
    let mut st = state(groups(&[(0, 1)]));
    st.extra.default_recording_options = 7; // mix, self, mix minus self
    st.extra.default_recording_format = 3; // WAV
    const BIG: usize = BLOCK;
    let cfg = sonobus_engine::AudioConfig {
        sample_rate: SR,
        block_size: BIG,
        input_channels: 1,
        output_channels: 2,
    };
    let (e, mut audio) = sonobus_engine::Engine::new(sonobus_engine::EngineConfig {
        audio: cfg,
        udp_port: 0,
        state: st,
    })
    .unwrap();
    let quit = Arc::new(AtomicBool::new(false));
    let t = {
        let quit = quit.clone();
        std::thread::spawn(move || {
            let block = std::time::Duration::from_secs_f64(BIG as f64 / f64::from(SR));
            let x = vec![0.25f32; BIG];
            let (mut l, mut r) = (vec![0.0f32; BIG], vec![0.0f32; BIG]);
            while !quit.load(Ordering::Relaxed) {
                audio.process(&[&x[..]], &mut [&mut l[..], &mut r[..]]);
                std::thread::sleep(block);
            }
        })
    };
    let dir = tmp("rt-stop");
    for take in 0..3 {
        let folder = e.start_recording(&dir, &format!("t{take}")).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(400));
        // a command batch the audio thread has not taken yet: the stop's commands wait in
        // the engine's queue (until the send thread's next round) while it keeps recording
        e.update_state(|s| s.params.dry = 0.5 + take as f32 * 0.1);
        let files = e.stop_recording().unwrap();
        assert_eq!(files.len(), 3, "{files:?}");
        let lens: Vec<usize> = files.iter().map(|f| read_wav(f)[0].len()).collect();
        assert!(
            lens.iter().all(|&n| n == lens[0] && n > 0),
            "{folder:?}: {lens:?}"
        );
    }
    quit.store(true, Ordering::Relaxed);
    t.join().unwrap();
}

/// Review finding 6: a file on a hung share (every open and read slowed down by
/// `sonobus-files`' test hook) never holds the engine lock: other calls go on while it opens,
/// and closing it does not wait for its stuck decoder; the same for a soundboard sample.
#[test]
fn slow_files_do_not_block_the_engine() {
    use std::time::{Duration, Instant};
    let _serial = serial();
    let dir = tmp("slow");
    let wav = sine_wav(&dir, "slow.wav", 1000.0, 1.0);
    let cfg = sonobus_engine::AudioConfig {
        sample_rate: SR,
        block_size: BLOCK,
        input_channels: 1,
        output_channels: 2,
    };
    let (e, _audio) = sonobus_engine::Engine::new(sonobus_engine::EngineConfig {
        audio: cfg,
        udp_port: 0,
        state: state(groups(&[(0, 1)])),
    })
    .unwrap();
    let e = std::sync::Arc::new(e);
    let slow = Duration::from_millis(600);
    sonobus_files::testing::set_slow_io(&wav, slow);
    for sample in [false, true] {
        let opener = {
            let (e, wav) = (e.clone(), wav.clone());
            std::thread::spawn(move || {
                if sample {
                    e.set_soundboards(Soundboards {
                        boards: vec![Soundboard {
                            name: "b".into(),
                            samples: vec![SoundSample {
                                name: "s".into(),
                                file_path: Some(wav.to_string_lossy().into_owned()),
                                ..SoundSample::default()
                            }],
                        }],
                        ..Soundboards::default()
                    });
                    e.soundboard_press((0, 0)).map(|_| ())
                } else {
                    e.load_file(&wav).map(|_| ())
                }
            })
        };
        std::thread::sleep(Duration::from_millis(150));
        let t0 = Instant::now();
        let _ = e.snapshot();
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "blocked {:?}",
            t0.elapsed()
        );
        opener.join().unwrap().unwrap();
        std::thread::sleep(Duration::from_millis(100)); // the decoder is inside a read
        let t0 = Instant::now();
        if sample {
            e.stop_all_samples();
        } else {
            e.close_file();
        }
        assert!(
            t0.elapsed() < Duration::from_millis(200),
            "close {:?}",
            t0.elapsed()
        );
    }
    sonobus_files::testing::set_slow_io(&wav, Duration::ZERO);
}

/// Review finding 7: dropping the engine while recording finalizes the files and reports
/// them through a handle that outlives it.
#[test]
fn engine_drop_finalizes_a_recording() {
    let _serial = serial();
    let mut st = state(groups(&[(0, 1)]));
    st.extra.default_recording_options = 1;
    st.extra.default_recording_format = 3; // WAV
    let cfg = sonobus_engine::AudioConfig {
        sample_rate: SR,
        block_size: BLOCK,
        input_channels: 1,
        output_channels: 2,
    };
    let (e, mut audio) = sonobus_engine::Engine::new(sonobus_engine::EngineConfig {
        audio: cfg,
        udp_port: 0,
        state: st,
    })
    .unwrap();
    let dir = tmp("drop");
    let path = e.start_recording(&dir, "take").unwrap();
    let x = vec![0.25f32; BLOCK];
    let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
    for _ in 0..40 {
        audio.process(&[&x[..]], &mut [&mut l[..], &mut r[..]]);
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
    let handle = e.notification_handle();
    drop(e);
    let notes: Vec<Notification> = std::iter::from_fn(|| handle.poll()).collect();
    assert!(
        notes.iter().any(|n| matches!(n,
            Notification::RecordingStopped { files, error: None } if files == std::slice::from_ref(&path))),
        "{notes:?}"
    );
    let ch = read_wav(&path);
    assert!(ch[0].len() >= 30 * BLOCK, "{}", ch[0].len());
    let b = std::fs::read(&path).unwrap();
    assert_eq!(
        u32::from_le_bytes(b[100..104].try_into().unwrap()) as usize,
        b.len() - 104,
        "header finalized"
    );
    drop(audio);
}

fn bare_engine(
    st: sonobus_core::state::AppState,
) -> (sonobus_engine::Engine, sonobus_engine::EngineAudio) {
    let cfg = sonobus_engine::AudioConfig {
        sample_rate: SR,
        block_size: BLOCK,
        input_channels: 2,
        output_channels: 2,
    };
    sonobus_engine::Engine::new(sonobus_engine::EngineConfig {
        audio: cfg,
        udp_port: 0,
        state: st,
    })
    .unwrap()
}

/// A mix-only recording over an existing file of that name gets `name(2)`; the old file is
/// kept (upstream deletes it first).
#[test]
fn mix_only_recording_name_collision() {
    let _serial = serial();
    let mut st = state(groups(&[(0, 1)]));
    st.extra.default_recording_options = 1;
    st.extra.default_recording_format = 3;
    let (e, _audio) = bare_engine(st);
    let dir = tmp("collide");
    std::fs::write(dir.join("take.wav"), b"keep").unwrap();
    let p = e.start_recording(&dir, "take").unwrap();
    assert_eq!(p, dir.join("take(2).wav"));
    assert_eq!(e.stop_recording().unwrap(), [p]);
    assert_eq!(std::fs::read(dir.join("take.wav")).unwrap(), b"keep");
}

/// PR #291 through the engine: a soloed input group is remembered in the state and solos
/// again in an engine made from it (output = that group alone), unless "remember" is off.
#[test]
fn input_solo_is_restored_through_the_engine() {
    let _serial = serial();
    let mut st = state(groups(&[(0, 1), (1, 1)]));
    st.params.dry = 1.0;
    let render = |st: sonobus_core::state::AppState, inputs: [f32; 2]| -> Vec<f32> {
        let (_e, mut audio) = bare_engine(st);
        let x: Vec<Vec<f32>> = inputs.iter().map(|&v| vec![v; BLOCK]).collect();
        let (mut l, mut r) = (vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]);
        for _ in 0..50 {
            audio.process(&[&x[0][..], &x[1][..]], &mut [&mut l[..], &mut r[..]]);
        }
        [l, r].concat()
    };
    let (e, _audio) = bare_engine(st.clone());
    e.set_input_soloed(1, true);
    let soloed = e.state();
    drop(e);
    let only_second = render(st.clone(), [0.0, 0.25]);
    let both = render(st.clone(), [0.5, 0.25]);
    assert_ne!(only_second, both);
    assert_eq!(
        render(soloed.clone(), [0.5, 0.25]),
        only_second,
        "solo restored"
    );
    let mut forget = soloed;
    sonobus_engine::options::set(
        &mut forget.extra,
        sonobus_engine::options::REMEMBER_INPUT_SOLO,
        sonobus_core::valuetree::Var::Bool(false),
    );
    assert_eq!(render(forget, [0.5, 0.25]), both, "not remembered");
}
