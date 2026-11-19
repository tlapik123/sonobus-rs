//! Two controllers with null audio in one process: one hosts a session, both join it, chat,
//! and the metronome one sends shows up on the other's peer meter.

use std::time::{Duration, Instant};

use sonobus_app::controller::{AppController, ConnectRequest, ControllerConfig};
use sonobus_engine::Connection;

fn controller() -> AppController {
    AppController::new(ControllerConfig { settings_dir: None, use_devices: false }).expect("controller")
}

fn until(ctrls: &mut [&mut AppController], secs: u64, what: &str, cond: impl Fn(&[&mut AppController]) -> bool) {
    let t = Instant::now();
    loop {
        for c in ctrls.iter_mut() {
            c.tick();
        }
        if cond(ctrls) {
            return;
        }
        let status: Vec<_> = ctrls.iter().map(|c| c.vm().status.clone()).collect();
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out waiting for {what}; status {status:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn host_join_chat_and_meter() {
    let mut a = controller();
    let mut b = controller();
    let req = |user: &str| ConnectRequest {
        user: user.into(),
        group: "test-band".into(),
        group_password: "pw".into(),
        ..ConnectRequest::default()
    };
    a.host_session(0, req("alice")).expect("host");
    let (port, addrs) = a.vm().hosting.clone().expect("hosting");
    assert!(port > 0);
    // b joins the way a LAN user would: the advertised address (loopback without a LAN)
    let host = addrs.first().map_or("127.0.0.1".into(), ToString::to_string);
    b.connect(ConnectRequest { host, port, ..req("bob") });

    until(&mut [&mut a, &mut b], 20, "peers", |c| {
        c.iter().all(|c| c.vm().snapshot.connection == Connection::Connected && c.vm().snapshot.peers.len() == 1)
    });
    assert_eq!(a.vm().snapshot.peers[0].user, "bob");
    assert_eq!(b.vm().snapshot.peers[0].user, "alice");
    assert_eq!(b.vm().snapshot.group.as_deref(), Some("test-band"));
    // the join is in the recents, most recent first
    assert_eq!(b.vm().state.recents[0].group_name, "test-band");
    assert!(b.group_link().unwrap().contains("g=test-band"));

    a.send_chat(&[], "hello bob");
    until(&mut [&mut a, &mut b], 10, "chat", |c| c[1].vm().chat.iter().any(|m| m.message == "hello bob"));
    let m = b.vm().chat.iter().find(|m| m.message == "hello bob").unwrap();
    assert_eq!((m.from.as_str(), m.own), ("alice", false));
    assert_eq!(b.vm().unread_chat, 1);
    assert!(a.vm().chat.iter().any(|m| m.own && m.message == "hello bob"));

    // alice's metronome, sent to peers, moves bob's meter for alice
    a.edit(|s| {
        s.params.met_enabled = true;
        s.params.send_met_audio = true;
        s.params.met_gain = 1.0;
    });
    until(&mut [&mut a, &mut b], 20, "peer meter", |c| {
        c[1].vm().snapshot.peers.first().is_some_and(|p| p.meter.peak.iter().any(|&v| v > 0.01))
    });

    b.disconnect();
    until(&mut [&mut a, &mut b], 10, "disconnect", |c| {
        c[1].vm().snapshot.connection == Connection::Disconnected && c[0].vm().snapshot.peers.is_empty()
    });
    a.stop_hosting();
}

#[test]
fn connect_errors_are_reported() {
    let mut c = controller();
    c.connect(ConnectRequest { user: String::new(), ..ConnectRequest::default() });
    assert!(c.vm().status_is_error);
    // an unresolvable host fails off-thread and is reported on a later tick
    c.connect(ConnectRequest { host: "no-such-host.invalid".into(), user: "x".into(), ..ConnectRequest::default() });
    assert!(c.vm().connecting);
    until(&mut [&mut c], 30, "connect failure", |c| !c[0].vm().connecting);
    assert!(c.vm().status.starts_with("Connect failed: "), "{}", c.vm().status);
}

#[test]
fn settings_persist_through_controller() {
    let dir = std::env::temp_dir().join(format!("sonobus-app-ctrl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    {
        let mut c = AppController::new(ControllerConfig { settings_dir: Some(dir.clone()), use_devices: false }).unwrap();
        c.edit(|s| s.params.wet = 0.25);
        c.add_input_group();
        c.shutdown();
    }
    let c = AppController::new(ControllerConfig { settings_dir: Some(dir.clone()), use_devices: false }).unwrap();
    assert_eq!(c.vm().state.params.wet, 0.25);
    assert_eq!(c.vm().state.input_groups.len(), 2);
    assert!(dir.join("SonoBus.settings").exists());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn command_line_hosts_and_links_fill_the_form() {
    let mut c = controller();
    let args = |s: &str| sonobus_app::links::parse_args(s.split_whitespace().map(String::from)).unwrap();
    // a link fills server/group without a server named elsewhere; a group means connect, after
    // a confirmation when the server is new to us (it would get our address and name)
    let form = c.apply_cli(&args("-n carol sonobus://192.0.2.7:11000/?g=jam&p=x"));
    assert_eq!((form.host.as_str(), form.port, form.group.as_str(), form.group_password.as_str()), ("192.0.2.7", 11000, "jam", "x"));
    assert_eq!(form.user, "carol");
    assert!(!c.vm().connecting);
    assert_eq!(c.vm().pending_link.as_ref().map(|r| r.host.as_str()), Some("192.0.2.7"));
    c.answer_pending_link(true);
    assert!(c.vm().connecting && c.vm().pending_link.is_none());
    c.disconnect();
    // the default server connects right away
    let mut d = controller();
    d.apply_cli(&args("-n carol -g jam"));
    assert!(d.vm().connecting && d.vm().pending_link.is_none());
    d.disconnect();
    // --host-server hosts here and joins it
    let mut h = controller();
    let form = h.apply_cli(&args("--host-server 0 -g band -n dave"));
    assert!(h.vm().hosting.is_some());
    assert_eq!(form.group, "band");
    until(&mut [&mut h], 10, "joined own session", |c| c[0].vm().snapshot.group.as_deref() == Some("band"));
}

/// A stand-in for Android playback capture: reports a running 48 kHz stereo capture.
struct FakeCapture;

impl sonobus_app::platform::Platform for FakeCapture {
    fn capture(&self) -> sonobus_app::platform::CaptureStatus {
        use sonobus_app::platform::*;
        CaptureStatus {
            state: CaptureState::Running,
            target: Some(CaptureTarget::System),
            format: Some(sonobus_app::audio::ExternalSpec { channels: 2, sample_rate: 48000 }),
            buffer_frames: 480,
            latency_ms: None,
        }
    }
    fn apps(&self) -> Vec<sonobus_app::platform::AppInfo> {
        Vec::new()
    }
    fn load_apps(&self) {}
    fn start_capture(&self, _: sonobus_app::platform::CaptureTarget) {}
    fn stop_capture(&self) {}
}

#[test]
fn captured_playback_reaches_the_peer() {
    use sonobus_app::audio::push_external;
    use sonobus_app::ui::SonobusApp;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    let mut phone = SonobusApp::new(controller(), ConnectRequest::default());
    let mut desk = controller();
    let req = |user: &str| ConnectRequest {
        user: user.into(),
        group: "capture".into(),
        ..ConnectRequest::default()
    };
    phone.ctrl.host_session(0, req("phone")).expect("host");
    let (port, addrs) = phone.ctrl.vm().hosting.clone().unwrap();
    let host = addrs.first().map_or("127.0.0.1".into(), ToString::to_string);
    desk.connect(ConnectRequest { host, port, ..req("desk") });

    until(&mut [&mut phone.ctrl, &mut desk], 20, "peers", |c| c.iter().all(|c| c.vm().snapshot.peers.len() == 1));
    // capture starts mid-session: the platform's running capture becomes the engine's third
    // and fourth input channel (reserved, so no engine rebuild: same UDP port), with a stereo,
    // unmonitored "System Sound" group and a stereo send
    let udp_port = phone.ctrl.vm().snapshot.udp_port;
    phone.platform = Some(Box::new(FakeCapture));
    phone.tick();
    phone.tick();
    assert_eq!(phone.ctrl.vm().snapshot.udp_port, udp_port);
    let spec = phone.ctrl.audio_setup().external.expect("external input set");
    assert_eq!((spec.channels, spec.sample_rate), (2, 48000));
    let st = &phone.ctrl.vm().state;
    let g = st.input_groups.iter().find(|g| g.name == "System Sound").expect("group");
    assert_eq!((g.chan_start, g.num_channels, g.monitor), (2, 2, 0.0));
    assert_eq!(st.params.send_channels, 2);

    // the "AudioRecord thread": a 1 kHz tone in 10 ms buffers, in real time
    let slot = phone.ctrl.external_input();
    let stop = Arc::new(AtomicBool::new(false));
    let s = stop.clone();
    let pusher = std::thread::spawn(move || {
        let mut buf = [0.0f32; 960];
        let mut n = 0usize;
        let mut taken = 0;
        while !s.load(Ordering::Relaxed) {
            for f in buf.as_chunks_mut::<2>().0 {
                let v = 0.5 * (n as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin();
                f.fill(v);
                n += 1;
            }
            taken += push_external(&slot, &buf);
            std::thread::sleep(Duration::from_millis(10));
        }
        taken
    });

    let t = Instant::now();
    loop {
        phone.tick();
        desk.tick();
        let heard = desk.vm().snapshot.peers.first().is_some_and(|p| p.meter.peak.iter().any(|&v| v > 0.05));
        if heard {
            break;
        }
        assert!(
            t.elapsed() < Duration::from_secs(30),
            "desk never heard the capture; desk {:?} peers {:?}; phone peers {:?} input {:?} audio {:?}",
            desk.vm().status,
            desk.vm().snapshot.peers.iter().map(|p| (&p.user, &p.meter.peak)).collect::<Vec<_>>(),
            phone.ctrl.vm().snapshot.peers.iter().map(|p| (&p.user, &p.meter.peak)).collect::<Vec<_>>(),
            phone.ctrl.vm().snapshot.input.peak,
            phone.ctrl.vm().audio.state,
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    // the phone's input meter shows it on the capture channels only (the mic is silent)
    let input = phone.ctrl.vm().snapshot.input.peak.clone();
    stop.store(true, Ordering::Relaxed);
    assert!(pusher.join().unwrap() > 0);
    assert!(input.len() >= 4 && input[2] > 0.05 && input[0] == 0.0, "input meter {input:?}");
    desk.disconnect();
    phone.ctrl.stop_hosting();
}

/// The capture never lands in the mic's group: a "device" that opens one channel instead of
/// the two the mic group was made for, then no mic at all (permission denied), with the fake
/// platform pushing a tone. Every engine channel the tone shows up on belongs to the "System
/// Sound" group only.
#[test]
fn capture_stays_out_of_the_mic_group() {
    use sonobus_app::audio::{DeviceChoice, DeviceSetup, push_external};
    use sonobus_app::ui::SonobusApp;

    let mut phone = SonobusApp::new(controller(), ConnectRequest::default());
    assert_eq!(phone.ctrl.vm().state.input_groups[0].num_channels, 2);
    let cases = [
        (DeviceSetup { input: DeviceChoice::Id("fake-mono".into()), input_channels: Some(1), ..DeviceSetup::default() }, [1, 2]),
        (DeviceSetup { input: DeviceChoice::None, ..DeviceSetup::default() }, [0, 1]),
    ];
    phone.platform = Some(Box::new(FakeCapture));
    for (setup, capture_channels) in cases {
        let setup = DeviceSetup { external: phone.ctrl.audio_setup().external, ..setup };
        phone.ctrl.set_audio_setup(setup).unwrap();
        phone.tick(); // the platform's capture becomes the external input
        assert!(phone.ctrl.audio_setup().external.is_some());
        let slot = phone.ctrl.external_input();
        let mut buf = [0.0f32; 960];
        for (i, f) in buf.as_chunks_mut::<2>().0.iter_mut().enumerate() {
            f.fill(0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 1000.0 / 48000.0).sin());
        }
        let t = Instant::now();
        loop {
            push_external(&slot, &buf);
            phone.tick();
            let peak = phone.ctrl.vm().snapshot.input.peak.clone();
            if capture_channels.iter().all(|&c| peak.get(c).is_some_and(|&v| v > 0.05)) {
                break;
            }
            assert!(t.elapsed() < Duration::from_secs(10), "capture not on {capture_channels:?}: {peak:?}");
            std::thread::sleep(Duration::from_millis(10));
        }
        let groups = &phone.ctrl.vm().state.input_groups;
        for ch in capture_channels {
            let on: Vec<&str> = groups
                .iter()
                .filter(|g| (g.chan_start..g.chan_start + g.num_channels).contains(&(ch as i32)))
                .map(|g| g.name.as_str())
                .collect();
            assert_eq!(on, ["System Sound"], "channel {ch}; groups {groups:?}");
        }
    }
}

// ---- stage 2 ----------------------------------------------------------------------------------

/// alice hosts `group`, bob joins; both see each other.
fn session(group: &str) -> (AppController, AppController) {
    let mut a = controller();
    let mut b = controller();
    let req = |user: &str| ConnectRequest { user: user.into(), group: group.into(), ..ConnectRequest::default() };
    a.host_session(0, req("alice")).expect("host");
    let (port, addrs) = a.vm().hosting.clone().unwrap();
    let host = addrs.first().map_or("127.0.0.1".into(), ToString::to_string);
    b.connect(ConnectRequest { host, port, ..req("bob") });
    until(&mut [&mut a, &mut b], 20, "peers", |c| c.iter().all(|c| c.vm().snapshot.peers.len() == 1));
    // the streams are up once bob's meter for alice exists
    until(&mut [&mut a, &mut b], 20, "streams", |c| c.iter().all(|c| !c.vm().snapshot.peers[0].meter.peak.is_empty()));
    (a, b)
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sonobus-app-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A 16-bit mono 48 kHz WAV of a 440 Hz tone.
fn wav(path: &std::path::Path, secs: f32) {
    let n = (48000.0 * secs) as u32;
    let mut b = Vec::new();
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + 2 * n).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&48000u32.to_le_bytes());
    b.extend_from_slice(&96000u32.to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(2 * n).to_le_bytes());
    for i in 0..n {
        let v = (0.5 * (i as f32 * 2.0 * std::f32::consts::PI * 440.0 / 48000.0).sin() * 32767.0) as i16;
        b.extend_from_slice(&v.to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

fn heard(c: &AppController) -> bool {
    c.vm().snapshot.peers.first().is_some_and(|p| p.meter.peak.iter().any(|&v| v > 0.05))
}

fn quiet(c: &AppController) -> bool {
    c.vm().snapshot.peers.first().is_some_and(|p| p.meter.peak.iter().all(|&v| v < 0.001))
}

#[test]
fn record_file_playback_and_soundboard() {
    let dir = temp_dir("media");
    let tone = dir.join("tone.wav");
    wav(&tone, 3.0);
    let (mut a, mut b) = session("media");

    // recording: full mix and each user (folder of tracks); FLAC
    a.set_record_dir(&dir.join("rec"));
    a.set_recording_options(1 | 8);
    a.edit(|s| (s.extra.default_recording_format, s.extra.record_finish_open) = (2, false));
    a.start_recording(); // creates the files off the UI thread
    until(&mut [&mut a], 5, "recording", |c| c[0].vm().recording.is_some());

    // file playback, sent to bob
    a.load_file(&tone).expect("load");
    assert_eq!(a.vm().file.as_ref().map(|f| f.channels), Some(1));
    assert_eq!(a.vm().recent_files.len(), 0); // no settings dir: nothing remembered
    a.edit(|s| s.params.send_file_audio = true);
    a.engine().set_file_playing(true);
    until(&mut [&mut a, &mut b], 20, "file heard", |c| heard(c[1]));
    a.close_file();
    until(&mut [&mut a, &mut b], 10, "silence", |c| quiet(c[1]));

    // a soundboard sample, sent to bob
    let mut boards = sonobus_engine::Soundboards::default();
    boards.boards.push(sonobus_core::files::Soundboard {
        name: "Board".into(),
        samples: vec![sonobus_core::files::SoundSample {
            name: "tone".into(),
            file_url: sonobus_app::settings::file_url(&tone),
            ..Default::default()
        }],
    });
    a.set_soundboards(boards);
    a.edit(|s| s.params.send_soundboard_audio = true);
    a.engine().soundboard_press((0, 0)).expect("press");
    until(&mut [&mut a, &mut b], 20, "sample heard", |c| heard(c[1]) && !c[0].vm().voices.is_empty());
    a.engine().stop_all_samples();

    // stop: the files arrive as a notification and decode
    assert!(a.vm().recording.as_ref().unwrap().elapsed_secs > 0.5);
    a.stop_recording();
    until(&mut [&mut a, &mut b], 20, "recording files", |c| !c[0].vm().last_recording.is_empty());
    let files = a.vm().last_recording.clone();
    assert!(files.len() >= 2, "{files:?}");
    for f in &files {
        assert_eq!(f.extension().unwrap(), "flac", "{f:?}");
        let (p, _out) = sonobus_files::open_player(f, 48000).expect("decodes");
        assert!(p.info().frames.unwrap_or(0) > 24000, "{f:?} {:?}", p.info());
    }
    assert!(files.iter().any(|f| f.to_string_lossy().ends_with("-bob.flac")), "{files:?}");
    assert!(sonobus_app::controller::recording_main_file(&files).is_some());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn latency_test_and_match() {
    let (mut a, mut b) = session("latency");
    let bob = a.vm().snapshot.peers[0].id;
    assert!(a.engine().start_latency_test(bob));
    until(&mut [&mut a, &mut b], 20, "measurement", |c| c[0].vm().latency_tests.iter().any(|t| t.0 == bob && t.1 > 0.0));
    a.engine().stop_latency_test(bob);

    // group latency match: alice asks, bob gets the prompt and accepts
    a.begin_latency_match();
    until(&mut [&mut a, &mut b], 20, "latency table", |c| c[0].vm().latency_match.as_ref().is_some_and(|m| m.1));
    let (entries, _, target) = a.vm().latency_match.clone().unwrap();
    assert_eq!(entries.len(), 2);
    let before = b.vm().snapshot.peers[0].buffer_ms;
    a.send_latency_match(target + 60.0);
    assert!(a.vm().latency_match.is_none());
    until(&mut [&mut a, &mut b], 20, "match request", |c| c[1].vm().latency_request.is_some());
    assert_eq!(b.vm().latency_request.as_ref().unwrap().0, "alice");
    b.answer_latency_request(true);
    assert!(b.vm().latency_request.is_none());
    until(&mut [&mut a, &mut b], 10, "padded buffer", |c| c[1].vm().snapshot.peers[0].buffer_ms > before + 20.0);
}

#[test]
fn suggest_group_moves_both() {
    let (mut a, mut b) = session("before");
    a.suggest_group("after", "pw", false, &["bob".to_string()]);
    until(&mut [&mut a, &mut b], 20, "suggestion", |c| c[1].vm().group_suggestion.is_some());
    let g = b.vm().group_suggestion.clone().unwrap();
    assert_eq!((g.user.as_str(), g.group.as_str(), g.group_password.as_str()), ("alice", "after", "pw"));
    b.answer_group_suggestion(true);
    until(&mut [&mut a, &mut b], 30, "both in the new group", |c| {
        c.iter().all(|c| c.vm().snapshot.group.as_deref() == Some("after") && c.vm().snapshot.peers.len() == 1)
    });
    // join/leave lines in the (private group) chat
    assert!(a.vm().chat.iter().any(|m| m.from.is_empty() && m.message.contains("bob")), "{:?}", a.vm().chat);
}
