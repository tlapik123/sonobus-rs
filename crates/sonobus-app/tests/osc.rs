//! OSC remote control against the SonoBusMendeni fork's protocol: every address the fork
//! registers, sent over UDP on 127.0.0.1 to a controller, with the state change and the exact
//! feedback bytes checked; the fork's TouchOSC template's addresses are all known.

use std::collections::BTreeSet;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use aoo::osc::{self, Arg};
use sonobus_app::controller::{AppController, ConnectRequest, ControllerConfig};
use sonobus_app::osc_control::{self as oc, OscConfig};
use sonobus_core::state::ChannelGroup;
use sonobus_engine::Connection;

fn controller() -> AppController {
    AppController::new(ControllerConfig { settings_dir: None, use_devices: false }).expect("controller")
}

/// The "TouchOSC" end: sends to the app, receives its feedback.
struct Remote {
    sock: UdpSocket,
    app: SocketAddr,
    /// Feedback received so far: address and raw packet.
    got: Vec<(String, Vec<u8>)>,
}

impl Remote {
    fn attach(c: &mut AppController, peer_levels: bool) -> Remote {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        c.set_osc_config(OscConfig {
            enabled: true,
            receive_port: 0,
            target_port: sock.local_addr().unwrap().port(),
            send_state_on_start: false,
            send_peer_levels: peer_levels,
            ..OscConfig::default()
        });
        let app = c.vm().osc.listening.expect("listening");
        Remote { sock, app, got: Vec::new() }
    }

    fn send(&self, addr: &str, a: Arg) {
        let mut p = Vec::new();
        osc::encode(&mut p, addr, [a]);
        self.sock.send_to(&p, self.app).unwrap();
    }

    fn raw(&self, p: &[u8]) {
        self.sock.send_to(p, self.app).unwrap();
    }

    fn drain(&mut self) {
        let mut buf = [0u8; 2048];
        while let Ok(n) = self.sock.recv(&mut buf) {
            let addr = osc::decode(&buf[..n]).expect("well-formed feedback").addr.to_owned();
            self.got.push((addr, buf[..n].to_vec()));
        }
    }

    /// Ticks until `cond` holds (panics after a few seconds).
    fn until(&mut self, c: &mut AppController, what: &str, cond: impl Fn(&AppController, &Remote) -> bool) {
        let t = Instant::now();
        loop {
            c.tick();
            self.drain();
            if cond(c, self) {
                return;
            }
            assert!(t.elapsed() < Duration::from_secs(5), "timed out: {what}");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Ticks for a while, collecting feedback.
    fn pump(&mut self, c: &mut AppController, ms: u64) {
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(ms) {
            c.tick();
            self.drain();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn last(&self, addr: &str) -> Option<&Vec<u8>> {
        self.got.iter().rev().find(|g| g.0 == addr).map(|g| &g.1)
    }
}

fn packet(addr: &str, a: Arg) -> Vec<u8> {
    let mut p = Vec::new();
    osc::encode(&mut p, addr, [a]);
    p
}

fn close(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-4
}

type Check = fn(&AppController) -> bool;

/// Sends `addr arg`, waits for `check`, then for the feedback `fb` on the same address
/// (`None`: none expected, e.g. a momentary button).
fn case(r: &mut Remote, c: &mut AppController, addr: &str, arg: Arg, check: Check, fb: Option<Arg>) {
    assert!(!check(c), "{addr}: state already as expected before the message");
    r.got.clear();
    r.send(addr, arg);
    r.until(c, addr, |c, _| check(c));
    if let Some(fb) = fb {
        let want = packet(addr, fb);
        r.until(c, &format!("{addr} feedback {fb:?}"), |_, r| r.last(addr) == Some(&want));
    }
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sonobus-osc-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn disabled_and_loopback_by_default() {
    let mut c = controller();
    let st = c.vm().osc.clone();
    assert!(!st.config.enabled);
    assert!(st.listening.is_none());
    assert!(!st.config.allow_network);
    assert_eq!((st.config.receive_port, st.config.target_port), (6000, 6001));
    c.set_osc_config(OscConfig { enabled: true, receive_port: 0, ..OscConfig::default() });
    let a = c.vm().osc.listening.expect("listening");
    assert!(a.ip().is_loopback(), "{a}");
    c.set_osc_config(OscConfig { enabled: true, receive_port: 0, allow_network: true, ..OscConfig::default() });
    assert!(c.vm().osc.listening.unwrap().ip().is_unspecified());
    // persisted under the fork's ExtraState keys
    let e = &c.vm().state.extra;
    assert!(e.unknown.iter().any(|(k, _)| k == oc::KEY_ENABLED));
    assert!(e.unknown.iter().any(|(k, _)| k == "OSCTargetIPAddress"));
    c.set_osc_config(OscConfig::default());
    assert!(c.vm().osc.listening.is_none());
}

#[test]
fn global_addresses() {
    let mut c = controller();
    let rec_dir = temp_dir("rec");
    c.set_record_dir(&rec_dir);
    let mut r = Remote::attach(&mut c, false);
    use Arg::{Float as F, Int as I};
    let cases: Vec<(&str, Arg, Check, Option<Arg>)> = vec![
        // gain faders: inbound x2 then squared, outbound sqrt then x0.5
        ("/OutGainSlider", F(0.5), |c| p(c).wet == 0.5, Some(F(0.5))),
        ("/DrySlider", F(0.25), |c| p(c).dry == 0.5, Some(F(0.25))),
        // inverted on the wire: 0 = muted
        ("/MainMuteButton", I(0), |c| p(c).main_send_mute, Some(I(0))),
        ("/MainMuteButton", F(0.75), |c| !p(c).main_send_mute, Some(I(1))),
        ("/MainRecvMuteButton", F(0.2), |c| p(c).main_recv_mute, Some(I(0))),
        ("/MainRecvMuteButton", I(1), |c| !p(c).main_recv_mute, Some(I(1))),
        // raw in, x0.5 out
        ("/MetLevelSlider", F(0.25), |c| p(c).met_gain == 0.25, Some(F(0.125))),
        ("/MetTempoSlider", F(120.0), |c| p(c).met_tempo == 120.0, Some(F(60.0))),
        ("/MetEnableButton", I(1), |c| p(c).met_enabled, Some(I(1))),
        ("/OptionsDynamicResamplingButton", F(1.0), |c| p(c).dynamic_resampling, Some(I(1))),
        ("/OptionsAutoReconnectButton", I(1), |c| p(c).reconnect_last, Some(I(1))),
        ("/OptionsInputLimiterButton", I(0), |c| c.vm().state.input_groups.iter().all(|g| !g.limiter.enabled), Some(I(0))),
        ("/OptionsDefaultLevelSlider", F(0.5), |c| p(c).def_peer_level == 0.5, Some(F(0.25))),
        ("/OptionsAutosizeDefaultChoice", I(1), |c| p(c).def_net_auto == 1, Some(I(1))),
        ("/OptionsFormatChoiceDefaultChoice", I(7), |c| p(c).def_send_qual == 7, Some(I(7))),
        ("/OptionsMetRecordedButton", I(0), |c| !p(c).met_is_recorded, Some(I(0))),
        ("/OptionsRecSelfButton", I(1), |c| x(c).default_recording_options == 3, Some(I(1))),
        ("/OptionsRecMixButton", I(0), |c| x(c).default_recording_options == 2, Some(I(0))),
        ("/OptionsRecOthersButton", I(1), |c| x(c).default_recording_options == 10, Some(I(1))),
        ("/OptionsRecMixMinusButton", I(1), |c| x(c).default_recording_options == 14, Some(I(1))),
        ("/MainReverbEnabled", I(1), |c| p(c).main_reverb_enabled, Some(I(1))),
        ("/ReverbLevelSlider", F(0.5), |c| p(c).main_reverb_level == 0.5, Some(F(0.25))),
        ("/ReverbSizeSlider", F(0.75), |c| p(c).main_reverb_size == 0.75, Some(F(0.375))),
        ("/ReverbDampingSlider", F(0.25), |c| p(c).main_reverb_damp == 0.25, Some(F(0.125))),
        ("/ReverbPreDelaySlider", F(50.0), |c| p(c).main_reverb_predelay == 50.0, Some(F(25.0))),
        ("/MetSyncButton", I(1), |c| p(c).sync_met_host, Some(I(1))),
        ("/MetSyncFileButton", I(1), |c| p(c).sync_met_file, Some(I(1))),
        ("/InMuteButton", I(1), |c| p(c).main_in_mute, Some(I(1))),
        ("/InSoloButton", F(1.0), |c| p(c).main_monitor_solo, Some(I(1))),
        ("/OptionsRecSelfPostFxButton", I(1), |c| !x(c).record_self_pre_fx, Some(I(1))),
        ("/OptionsRecSelfSilenceMutedButton", I(0), |c| !x(c).record_self_silence_when_muted, Some(I(0))),
        ("/OptionsDisableShortcutButton", I(1), |c| x(c).disable_key_shortcuts, Some(I(1))),
        ("/OptionsChangeAllFormatButton", I(1), |c| x(c).change_qual_for_all, Some(I(1))),
        // slider value s gives threshold 1/s; feedback is the slider value
        ("/OptionsAutoDropThreshSlider", F(4.0), |c| x(c).auto_drop_rate_thresh == 0.25, Some(F(2.0))),
        ("/BufferTimeSlider", F(0.0), |c| p(c).def_net_buf == 0.0, Some(F(0.0))),
        ("/OptionsUdpPortEditor", I(12000), |c| x(c).use_udp_port == 12000, Some(I(12000))),
        ("/OptionsUseSpecificUdpPortButton", I(0), |c| x(c).use_udp_port == 0, Some(I(0))),
        ("/OptionsSliderSnapToMouseButton", I(0), |c| !x(c).slider_snap_to_mouse, Some(I(0))),
        ("/MetSendButton", I(1), |c| p(c).send_met_audio, Some(I(1))),
        ("/FileSendButton", I(1), |c| p(c).send_file_audio, Some(I(1))),
        ("/SoundboardSendButton", F(1.0), |c| p(c).send_soundboard_audio, Some(I(1))),
        ("/PlaybackSlider", F(0.5), |c| c.vm().state.file_playback_group.gain == 0.5, Some(F(0.25))),
        ("/FilePlaybackPreLevel", F(1.5), |c| c.vm().state.file_playback_group.gain == 1.5, Some(F(0.75))),
        ("/FileMonitorSlider", F(0.5), |c| c.vm().state.file_playback_group.monitor == 0.5, Some(F(0.25))),
        ("/SoundboardLevelSlider", F(1.5), |c| c.vm().state.soundboard_group.gain == 1.5, Some(F(0.75))),
        ("/SoundboardMonitorSlider", F(0.5), |c| c.vm().state.soundboard_group.monitor == 0.5, Some(F(0.25))),
        // like the gain faders
        ("/SoundboardVolumeSlider", F(0.5), |c| c.vm().state.soundboard_group.gain == 0.5, Some(F(0.5))),
        ("/MetPanSlider", F(-0.5), |c| c.vm().state.metronome_group.pans[0] == -0.5, Some(F(-0.25))),
        ("/MetMonitorSlider", F(0.5), |c| c.vm().state.metronome_group.monitor == 0.5, Some(F(0.25))),
        ("/InputReverbLevel", F(0.5), |c| p(c).in_reverb_level == 0.5, Some(F(0.25))),
        ("/InputReverbSize", F(0.5), |c| p(c).in_reverb_size == 0.5, Some(F(0.25))),
        ("/InputReverbDamping", F(0.25), |c| p(c).in_reverb_damp == 0.25, Some(F(0.125))),
        ("/InputReverbPreDelay", F(30.0), |c| p(c).in_reverb_predelay == 30.0, Some(F(15.0))),
        ("/MonDelayButton", I(1), |c| c.vm().state.input_groups.iter().all(|g| g.monitor_delay.enabled), Some(I(1))),
        ("/MonDelayButton", F(0.0), |c| !c.vm().state.metronome_group.monitor_delay.enabled && c.vm().state.input_groups.iter().all(|g| !g.monitor_delay.enabled), Some(I(0))),
        ("/MainPushToTalkButton", I(1), |c| p(c).main_recv_mute && !p(c).main_send_mute, Some(I(1))),
        ("/MainPushToTalkButton", I(0), |c| !p(c).main_recv_mute, Some(I(0))),
        ("/OptionsMaxRecvPaddingSlider", F(40.0), |c| c.engine().receive_sync_padding() == 40.0, Some(F(20.0))),
        ("/RecordingButton", I(1), |c| c.vm().recording.is_some(), Some(I(1))),
        ("/RecordingButton", F(0.0), |c| c.vm().recording.is_none(), Some(I(0))),
    ];
    for (addr, arg, check, fb) in cases {
        case(&mut r, &mut c, addr, arg, check, fb);
    }
    // triggers: float, `(int) v == 1`, echoed as int 1; an int does nothing
    for addr in ["/RecvSyncButton", "/BufferMinButton"] {
        r.got.clear();
        r.send(addr, I(1));
        r.send(addr, F(0.9));
        r.pump(&mut c, 150);
        assert!(r.last(addr).is_none(), "{addr}");
        r.send(addr, F(1.5));
        let want = packet(addr, I(1));
        r.until(&mut c, addr, |_, r| r.last(addr) == Some(&want));
    }
    // momentary, accepted-and-ignored and type-mismatched messages change nothing
    let before = c.vm().state.clone();
    r.got.clear();
    for (addr, a) in [
        ("/SoundboardStopAllPlayback", F(1.0)),
        ("/OptionsRecStealth", I(1)),
        ("/OptionsOverrideSamplerateButton", I(1)),
        ("/OptionsShouldCheckForUpdateButton", I(1)),
        ("/InReverbButton", I(1)),
        ("/MetTempoSlider", I(200)),               // float only
        ("/OptionsAutosizeDefaultChoice", F(2.0)), // int only
        ("/MainPushToTalkButton", F(1.0)),         // int only
        ("/volumeControl", F(1.0)),                // the fork's legacy listener, unregistered
        ("/Nope", F(1.0)),
    ] {
        r.send(addr, a);
    }
    r.pump(&mut c, 200);
    assert_eq!(c.vm().state.params, before.params);
    assert!(r.got.is_empty(), "{:?}", r.got.iter().map(|g| &g.0).collect::<Vec<_>>());
    // a string on a toggle counts as off (the fork's handlers)
    r.send("/MetEnableButton", Arg::Str("on"));
    r.until(&mut c, "string toggle", |c, _| !c.vm().state.params.met_enabled);
    // OSC's own configuration is not settable over OSC (only from the local UI)
    let before = c.vm().osc.config.clone();
    let stored = x(&c).unknown.clone();
    r.send("/OSCTargetPort", I(9));
    r.send("/OSCReceivePort", I(1));
    r.send("/OSCTargetIPAddress", Arg::Str("203.0.113.9"));
    r.send("/MetTempoSlider", F(90.0));
    r.until(&mut c, "tempo", |c, _| c.vm().state.params.met_tempo == 90.0);
    assert_eq!(c.vm().osc.config, before);
    assert_eq!(x(&c).unknown, stored, "nothing persisted");
    // and the feedback still reaches the configured target
    r.until(&mut c, "feedback", |_, r| r.last("/MetTempoSlider") == Some(&packet("/MetTempoSlider", F(45.0))));
    let _ = std::fs::remove_dir_all(rec_dir);
}

/// `/InputGroup1...`: floats raw (x0.5 back), toggles float > 0.5 (float 1.0 x0.5 back).
#[test]
fn input_group_addresses() {
    let mut c = controller();
    let mut r = Remote::attach(&mut c, false);
    let stereo = g(&c).num_channels == 2;
    use Arg::Float as F;
    let pan: Vec<(&str, Arg, Check, Option<Arg>)> = if stereo {
        vec![
            ("/InputGroup1PanLeft", F(-0.5), |c| c.vm().state.input_groups[0].pan_stereo[0] == -0.5, Some(F(-0.25))),
            ("/InputGroup1PanRight", F(0.5), |c| c.vm().state.input_groups[0].pan_stereo[1] == 0.5, Some(F(0.25))),
        ]
    } else {
        vec![("/InputGroup1Pan", F(-0.5), |c| c.vm().state.input_groups[0].pans[0] == -0.5, Some(F(-0.25)))]
    };
    let cases: Vec<(&str, Arg, Check, Option<Arg>)> = vec![
        ("/InputGroup1PreLevel", F(0.5), |c| g(c).gain == 0.5, Some(F(0.25))),
        ("/InputGroup1Monitor", F(0.5), |c| g(c).monitor == 0.5, Some(F(0.25))),
        ("/InputGroup1MonDelayEnable", F(1.0), |c| g(c).monitor_delay.enabled, Some(F(0.5))),
        ("/InputGroup1MonDelayTime", F(100.0), |c| g(c).monitor_delay.delay_time_ms == 100.0, Some(F(50.0))),
        ("/InputGroup1MonDelayLink", F(0.5), |c| !c.vm().state.extra.link_mon_delay_times, Some(F(0.0))),
        ("/InputGroup1MonReverbSend", F(0.5), |c| g(c).mon_reverb_send == 0.5, Some(F(0.25))),
        ("/InputGroup1Mute", F(1.0), |c| g(c).muted, Some(F(0.5))),
        ("/InputGroup1Solo", F(0.75), |c| c.vm().input_solo[0], Some(F(0.5))),
        ("/InputGroup1InputReverbSend", F(0.5), |c| g(c).in_reverb_send == 0.5, Some(F(0.25))),
        ("/InputGroup1PolarityInvert", F(1.0), |c| g(c).invert_polarity, Some(F(0.5))),
        ("/InputGroup1CompressorAuto", F(0.0), |c| !g(c).compressor.auto_makeup, Some(F(0.0))),
        ("/InputGroup1CompressorEnable", F(1.0), |c| g(c).compressor.enabled, Some(F(0.5))),
        ("/InputGroup1CompressorThreshold", F(-20.0), |c| g(c).compressor.threshold_db == -20.0, Some(F(-10.0))),
        ("/InputGroup1CompressorRatio", F(4.0), |c| g(c).compressor.ratio == 4.0, Some(F(2.0))),
        ("/InputGroup1CompressorAttack", F(20.0), |c| g(c).compressor.attack_ms == 20.0, Some(F(10.0))),
        ("/InputGroup1CompressorRelease", F(100.0), |c| g(c).compressor.release_ms == 100.0, Some(F(50.0))),
        ("/InputGroup1CompressorMakeupGain", F(6.0), |c| g(c).compressor.makeup_gain_db == 6.0, Some(F(3.0))),
        ("/InputGroup1ExpanderEnable", F(1.0), |c| g(c).expander.enabled, Some(F(0.5))),
        ("/InputGroup1ExpanderNoiseFloor", F(-50.0), |c| g(c).expander.threshold_db == -50.0, Some(F(-25.0))),
        ("/InputGroup1ExpanderRatio", F(4.0), |c| g(c).expander.ratio == 4.0, Some(F(2.0))),
        ("/InputGroup1ExpanderAttack", F(2.0), |c| g(c).expander.attack_ms == 2.0, Some(F(1.0))),
        ("/InputGroup1ExpanderRelease", F(300.0), |c| g(c).expander.release_ms == 300.0, Some(F(150.0))),
        ("/InputGroup1EqEnable", F(1.0), |c| g(c).eq.enabled, Some(F(0.5))),
        ("/InputGroup1EqLowShelfFreq", F(100.0), |c| g(c).eq.low_shelf_freq == 100.0, Some(F(50.0))),
        ("/InputGroup1EqLowShelfGain", F(3.0), |c| g(c).eq.low_shelf_gain == 3.0, Some(F(1.5))),
        ("/InputGroup1EqPara1Freq", F(200.0), |c| g(c).eq.para1_freq == 200.0, Some(F(100.0))),
        ("/InputGroup1EqPara1Gain", F(-3.0), |c| g(c).eq.para1_gain == -3.0, Some(F(-1.5))),
        ("/InputGroup1EqPara1Q", F(2.0), |c| g(c).eq.para1_q == 2.0, Some(F(1.0))),
        ("/InputGroup1EqHighShelfFreq", F(8000.0), |c| g(c).eq.high_shelf_freq == 8000.0, Some(F(4000.0))),
        ("/InputGroup1EqHighShelfGain", F(2.0), |c| g(c).eq.high_shelf_gain == 2.0, Some(F(1.0))),
        ("/InputGroup1EqPara2Freq", F(1000.0), |c| g(c).eq.para2_freq == 1000.0, Some(F(500.0))),
        ("/InputGroup1EqPara2Gain", F(4.0), |c| g(c).eq.para2_gain == 4.0, Some(F(2.0))),
        ("/InputGroup1EqPara2Q", F(3.0), |c| g(c).eq.para2_q == 3.0, Some(F(1.5))),
    ];
    let tested: BTreeSet<String> = cases.iter().chain(&pan).map(|c| c.0.trim_start_matches("/InputGroup1").to_owned()).collect();
    for (addr, arg, check, fb) in pan.into_iter().chain(cases) {
        case(&mut r, &mut c, addr, arg, check, fb);
    }
    // every registered suffix is covered (the pan of the other layout aside)
    for s in oc::GROUP.iter().chain(oc::FX) {
        assert!(tested.contains(*s) || s.starts_with("Pan"), "untested /InputGroup<n>{s}");
    }
    // an int is ignored (float only), a group that does not exist too, and safety clamps
    let before = c.vm().state.input_groups.clone();
    r.send("/InputGroup1PreLevel", Arg::Int(1));
    r.send("/InputGroup16Mute", F(1.0));
    r.pump(&mut c, 150);
    assert_eq!(c.vm().state.input_groups, before);
    r.send("/InputGroup1PreLevel", F(1e30));
    r.until(&mut c, "clamped gain", |c, _| c.vm().state.input_groups[0].gain == 2.0);
}

/// Bytes of the full-state dump: order, the fork's quirks, cleared peer slots, soundboards.
#[test]
fn full_state_dump() {
    let mut c = controller();
    c.edit(|s| {
        s.params.wet = 1.0;
        s.params.def_net_auto = 2; // Auto-Full: the dump's truncated normalised value is 0
        s.params.def_net_buf = 0.25;
        s.params.def_peer_level = 0.5;
    });
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_nonblocking(true).unwrap();
    let mut r = Remote { sock, app: "127.0.0.1:9".parse().unwrap(), got: Vec::new() };
    c.set_osc_config(OscConfig {
        enabled: true,
        receive_port: 0,
        target_port: r.sock.local_addr().unwrap().port(),
        ..OscConfig::default() // send state on start: on
    });
    r.until(&mut c, "dump", |_, r| r.got.iter().any(|g| g.0 == "/Soundboard16Track16"));
    let order: Vec<&str> = r.got.iter().map(|g| g.0.as_str()).collect();
    assert_eq!(&order[..5], ["/OutGainSlider", "/DrySlider", "/MainMuteButton", "/MainRecvMuteButton", "/MainPushToTalkButton"]);
    let get = |a: &str| r.got.iter().find(|g| g.0 == a).map(|g| g.1.clone()).unwrap_or_else(|| panic!("{a} not dumped"));
    // unity gain = sqrt(0.5) on the wire: 0x3f3504f3
    assert_eq!(get("/OutGainSlider"), b"/OutGainSlider\0\0,f\0\0\x3f\x35\x04\xf3");
    assert_eq!(get("/MainMuteButton"), packet("/MainMuteButton", Arg::Int(1))); // not muted, inverted
    assert_eq!(get("/OptionsDefaultLevelSlider"), packet("/OptionsDefaultLevelSlider", Arg::Float(0.125))); // defnetbuf (sic)
    assert_eq!(get("/OptionsAutosizeDefaultChoice"), packet("/OptionsAutosizeDefaultChoice", Arg::Int(0)));
    assert_eq!(get("/Peer1RemotePeerUserName"), packet("/Peer1RemotePeerUserName", Arg::Str("")));
    assert_eq!(get("/Peer16Level"), packet("/Peer16Level", Arg::Int(0)));
    assert_eq!(get("/Peer1Mute"), packet("/Peer1Mute", Arg::Float(0.0)));
    assert_eq!(get("/Peer3CompressorRatio"), packet("/Peer3CompressorRatio", Arg::Float(0.5)));
    assert_eq!(get("/Peer3ExpanderEnable"), packet("/Peer3ExpanderEnable", Arg::Int(0)));
    assert_eq!(get("/Soundboard1Name"), packet("/Soundboard1Name", Arg::Str("")));
    // the dump leaves out what the fork's leaves out
    for a in ["/MetSyncButton", "/OptionsFormatChoiceDefaultChoice", "/InputGroup1PreLevel", "/OSCTargetPort"] {
        assert!(!order.contains(&a), "{a} dumped");
    }
    // nothing twice, and the whole dump is 45 globals + 16 x 31 empty peer slots + 16 x 17 soundboard
    let unique: BTreeSet<&str> = order.iter().copied().collect();
    assert_eq!(unique.len(), order.len());
    assert_eq!(order.len(), 45 + 16 * 31 + 16 * 17);
    // then only changes go out
    r.got.clear();
    r.pump(&mut c, 200);
    assert!(r.got.is_empty(), "{:?}", r.got.iter().map(|g| &g.0).collect::<Vec<_>>());
    c.edit(|s| s.params.met_gain = 1.0);
    r.until(&mut c, "met level", |_, r| r.last("/MetLevelSlider") == Some(&packet("/MetLevelSlider", Arg::Float(0.5))));
    assert_eq!(r.got.len(), 1);
    // closing clears the soundboard slots and pulls two faders down (read on a thread: they
    // go out in paced bursts)
    let sock = r.sock.try_clone().unwrap();
    let reader = std::thread::spawn(move || {
        let mut r = Remote { sock, app: "127.0.0.1:9".parse().unwrap(), got: Vec::new() };
        let t = Instant::now();
        while t.elapsed() < Duration::from_millis(1000) && r.last("/OutGainSlider").is_none() {
            r.drain();
            std::thread::sleep(Duration::from_millis(1));
        }
        r.got
    });
    c.shutdown();
    r.got = reader.join().unwrap();
    assert_eq!(r.last("/OutGainSlider"), Some(&packet("/OutGainSlider", Arg::Float(0.0))));
    assert_eq!(r.got.iter().filter(|g| g.0.starts_with("/Soundboard")).count(), 16 * 17 + 1);
}

#[test]
fn peers_soundboard_and_meters() {
    let mut a = controller();
    let mut b = controller();
    let req = |user: &str| ConnectRequest { user: user.into(), group: "osc-band".into(), ..ConnectRequest::default() };
    a.host_session(0, req("alice")).expect("host");
    let (port, _) = a.vm().hosting.clone().unwrap();
    b.connect(ConnectRequest { host: "127.0.0.1".into(), port, ..req("bob") });
    let t = Instant::now();
    while !(a.vm().snapshot.peers.len() == 1 && b.vm().snapshot.connection == Connection::Connected) {
        a.tick();
        b.tick();
        assert!(t.elapsed() < Duration::from_secs(20), "session");
        std::thread::sleep(Duration::from_millis(20));
    }
    // a soundboard sample to trigger
    let dir = temp_dir("sb");
    let tone = dir.join("tone.wav");
    wav(&tone, 2.0);
    a.edit_soundboards(|bd| {
        bd.boards.push(sonobus_core::files::Soundboard {
            name: "Board".into(),
            samples: vec![sonobus_core::files::SoundSample {
                name: "tone".into(),
                file_url: sonobus_app::settings::file_url(&tone),
                ..Default::default()
            }],
        })
    });
    let mut r = Remote::attach(&mut a, true);
    let tick_b = |b: &mut AppController| b.tick();
    // connected peer state follows on change
    r.until(&mut a, "peer name", |_, r| r.last("/Peer1RemotePeerUserName") == Some(&packet("/Peer1RemotePeerUserName", Arg::Str("bob"))));
    r.until(&mut a, "soundboard names", |_, r| {
        r.last("/Soundboard1Name") == Some(&packet("/Soundboard1Name", Arg::Str("Board")))
            && r.last("/Soundboard1Track1") == Some(&packet("/Soundboard1Track1", Arg::Str("tone")))
    });
    use Arg::{Float as F, Int as I};
    let cases: Vec<(&str, Arg, Check, Option<Arg>)> = vec![
        // float != 0; fed back as a double (x0.5)
        ("/Peer1Mute", F(1.0), |c| !peer(c).recv_allow, Some(F(0.5))),
        ("/Peer1Mute", F(0.0), |c| peer(c).recv_allow, Some(F(0.0))),
        ("/Peer1Solo", F(0.3), |c| peer(c).soloed, Some(F(0.5))),
        ("/Peer1Level", F(0.5), |c| peer(c).level == 0.5, Some(F(0.5))),
        // x2 in, x0.5 out
        ("/Peer1Pan", F(0.25), |c| pan0(&g0(c)) == 0.5, Some(F(0.25))),
        // the peer's monitor reverb send (P.cpp:4776)
        ("/Peer1InputReverbSend", F(0.5), |c| g0(c).mon_reverb_send == 0.5, Some(F(0.25))),
        // int only for flags, fed back as int
        ("/Peer1PolarityInvert", I(1), |c| g0(c).invert_polarity, Some(I(1))),
        ("/Peer1CompressorAuto", I(0), |c| !g0(c).compressor.auto_makeup, Some(I(0))),
        ("/Peer1CompressorEnable", I(1), |c| g0(c).compressor.enabled, Some(I(1))),
        ("/Peer1CompressorThreshold", F(-20.0), |c| g0(c).compressor.threshold_db == -20.0, Some(F(-10.0))),
        ("/Peer1CompressorRatio", F(4.0), |c| g0(c).compressor.ratio == 4.0, Some(F(2.0))),
        ("/Peer1CompressorAttack", F(20.0), |c| g0(c).compressor.attack_ms == 20.0, Some(F(10.0))),
        ("/Peer1CompressorRelease", F(100.0), |c| g0(c).compressor.release_ms == 100.0, Some(F(50.0))),
        ("/Peer1CompressorMakeupGain", F(6.0), |c| g0(c).compressor.makeup_gain_db == 6.0, Some(F(3.0))),
        ("/Peer1ExpanderEnable", I(1), |c| g0(c).expander.enabled, Some(I(1))),
        ("/Peer1ExpanderNoiseFloor", F(-50.0), |c| g0(c).expander.threshold_db == -50.0, Some(F(-25.0))),
        ("/Peer1ExpanderRatio", F(4.0), |c| g0(c).expander.ratio == 4.0, Some(F(2.0))),
        ("/Peer1ExpanderAttack", F(2.0), |c| g0(c).expander.attack_ms == 2.0, Some(F(1.0))),
        ("/Peer1ExpanderRelease", F(300.0), |c| g0(c).expander.release_ms == 300.0, Some(F(150.0))),
        ("/Peer1EqEnable", I(1), |c| g0(c).eq.enabled, Some(I(1))),
        ("/Peer1EqLowShelfFreq", F(100.0), |c| g0(c).eq.low_shelf_freq == 100.0, Some(F(50.0))),
        ("/Peer1EqLowShelfGain", F(3.0), |c| g0(c).eq.low_shelf_gain == 3.0, Some(F(1.5))),
        ("/Peer1EqPara1Freq", F(200.0), |c| g0(c).eq.para1_freq == 200.0, Some(F(100.0))),
        ("/Peer1EqPara1Gain", F(-3.0), |c| g0(c).eq.para1_gain == -3.0, Some(F(-1.5))),
        ("/Peer1EqPara1Q", F(2.0), |c| g0(c).eq.para1_q == 2.0, Some(F(1.0))),
        ("/Peer1EqHighShelfFreq", F(8000.0), |c| g0(c).eq.high_shelf_freq == 8000.0, Some(F(4000.0))),
        ("/Peer1EqHighShelfGain", F(2.0), |c| g0(c).eq.high_shelf_gain == 2.0, Some(F(1.0))),
        ("/Peer1EqPara2Freq", F(1000.0), |c| g0(c).eq.para2_freq == 1000.0, Some(F(500.0))),
        ("/Peer1EqPara2Gain", F(4.0), |c| g0(c).eq.para2_gain == 4.0, Some(F(2.0))),
        ("/Peer1EqPara2Q", F(3.0), |c| g0(c).eq.para2_q == 3.0, Some(F(1.5))),
    ];
    let tested: BTreeSet<String> = cases.iter().map(|c| c.0.trim_start_matches("/Peer1").to_owned()).collect();
    for (addr, arg, check, fb) in cases {
        tick_b(&mut b);
        case(&mut r, &mut a, addr, arg, check, fb);
    }
    // triggers: int != 0, echoed as int 1
    for addr in ["/Peer1BufferMin", "/Peer1ResetDrop"] {
        r.got.clear();
        r.send(addr, F(1.0)); // float: ignored
        r.pump(&mut a, 100);
        assert!(r.last(addr).is_none());
        r.send(addr, I(1));
        let want = packet(addr, I(1));
        r.until(&mut a, addr, |_, r| r.last(addr) == Some(&want));
    }
    for s in oc::PEER.iter().chain(oc::FX) {
        assert!(tested.contains(*s) || ["BufferMin", "ResetDrop"].contains(s), "untested /Peer<n>{s}");
    }
    // a peer slot that is empty ignores everything
    r.send("/Peer2Mute", F(1.0));
    r.pump(&mut a, 100);
    // meters every 150 ms with "send peer levels"
    r.got.clear();
    r.pump(&mut a, 400);
    assert!(r.got.iter().filter(|g| g.0 == "/Peer1RecvMeterLevel").count() >= 2);
    // soundboard trigger: float >= 0.5
    r.send("/Soundboard1Track1", F(0.4));
    r.pump(&mut a, 100);
    assert!(a.vm().voices.is_empty());
    r.send("/Soundboard1Track1", F(1.0));
    r.until(&mut a, "sample plays", |c, _| !c.vm().voices.is_empty());
    r.send("/SoundboardStopAllPlayback", F(1.0));
    r.until(&mut a, "samples stop", |c, _| c.vm().voices.is_empty());
    // the peer leaves: its slot is cleared
    b.disconnect();
    r.until(&mut a, "slot cleared", |_, r| {
        r.last("/Peer1RemotePeerUserName") == Some(&packet("/Peer1RemotePeerUserName", Arg::Str("")))
            && r.last("/Peer1Level") == Some(&packet("/Peer1Level", Arg::Int(0)))
            && r.last("/Peer1RecvMeterLevel") == Some(&packet("/Peer1RecvMeterLevel", Arg::Float(0.0)))
    });
    a.stop_hosting();
    let _ = std::fs::remove_dir_all(dir);
}

fn p(c: &AppController) -> sonobus_core::state::Params {
    c.vm().state.params.clone()
}

fn x(c: &AppController) -> sonobus_core::state::ExtraState {
    c.vm().state.extra.clone()
}

/// Input group 1.
fn g(c: &AppController) -> ChannelGroup {
    c.vm().state.input_groups[0].clone()
}

fn peer(c: &AppController) -> sonobus_engine::PeerStatus {
    c.vm().snapshot.peers[0].clone()
}

/// The first channel group of peer 1.
fn g0(c: &AppController) -> ChannelGroup {
    peer(c).groups.first().cloned().unwrap_or_default()
}

fn pan0(g: &ChannelGroup) -> f32 {
    if g.num_channels == 2 { g.pan_stereo[0] } else { g.pans[0] }
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

#[test]
fn hostile_packets_are_ignored() {
    let mut c = controller();
    let mut r = Remote::attach(&mut c, false);
    let mut big = Vec::new();
    osc::encode(&mut big, "/MetTempoSlider", [Arg::Float(200.0), Arg::Blob(&[0; 9000])]);
    for p in [&big[..], b"", b"#bundle\0", b"/MetTempoSlider\0", &[0xff; 100], b"/MetTempoSlider\0,f\0\0\x43"] {
        r.raw(p);
    }
    let mut d = Vec::new();
    osc::encode(&mut d, "/MetTempoSlider", [Arg::Double(200.0)]); // not a JUCE type: dropped
    r.raw(&d);
    r.pump(&mut c, 200);
    assert_eq!(c.vm().state.params.met_tempo, 100.0);
    // NaN and infinities are not applied
    r.send("/MetTempoSlider", Arg::Float(f32::NAN));
    r.send("/OutGainSlider", Arg::Float(f32::INFINITY));
    r.pump(&mut c, 100);
    assert_eq!(c.vm().state.params.met_tempo, 100.0);
    assert_eq!(c.vm().state.params.wet, 1.0);
    // still listening
    r.send("/MetTempoSlider", Arg::Float(200.0));
    r.until(&mut c, "tempo", |c, _| c.vm().state.params.met_tempo == 200.0);
    assert!(close(c.vm().state.params.met_tempo, 200.0));
}

/// The fork's TouchOSC template (util/SonoBusMendeni.tosc from the SonoBusMendeni fork of
/// SonoBus, GPLv3, copied unchanged into tests/data): every address it uses is one we know.
#[test]
fn touchosc_template_addresses_are_implemented() {
    let z = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/SonoBusMendeni.tosc")).unwrap();
    let xml = String::from_utf8(miniz_oxide::inflate::decompress_to_vec_zlib(&z).expect("zlib")).unwrap();
    let addrs = template_addresses(&xml);
    // TouchOSC's own controls (page switchers, unnamed scratch controls): the fork does not
    // register them either
    let internal = ["/ApplicationPage", "/button1", "/button2", "/fader16", "/fader17", "/text1", "/{parent.name}/Soundboard"];
    let used: Vec<&String> = addrs.iter().filter(|a| !internal.contains(&a.as_str())).collect();
    assert!(used.len() > 120, "{}", used.len());
    for a in &used {
        assert!(oc::is_known(a), "template address {a} not implemented");
    }
    for a in ["/OutGainSlider", "/MainMuteButton", "/Peer1Level", "/Soundboard3Track16", "/SoundboardStopAllPlayback"] {
        assert!(addrs.contains(a), "{a}");
    }
}

/// The OSC paths of a `.tosc` document: `/` + the control's name, or constants. Properties of
/// a node precede its messages, and its children come after them, so the last `name` property
/// seen is the owner's.
fn template_addresses(xml: &str) -> BTreeSet<String> {
    const NAME: &str = "<key><![CDATA[name]]></key><value><![CDATA[";
    let mut out = BTreeSet::new();
    let mut name = "";
    let mut rest = xml;
    loop {
        let (n, o) = (rest.find(NAME), rest.find("<path>"));
        match (n, o) {
            (Some(n), o) if o.is_none_or(|o| n < o) => {
                let v = &rest[n + NAME.len()..];
                name = &v[..v.find("]]>").unwrap()];
                rest = v;
            }
            (_, Some(o)) => {
                let end = rest[o..].find("</path>").unwrap() + o;
                let mut path = String::new();
                for part in rest[o..end].split("<partial>").skip(1) {
                    let ty = &part[part.find("<type>").unwrap() + 6..part.find("</type>").unwrap()];
                    let v0 = part.find("<value><![CDATA[").unwrap() + 16;
                    let v = &part[v0..v0 + part[v0..].find("]]>").unwrap()];
                    match (ty, v) {
                        ("CONSTANT", v) => path.push_str(v),
                        ("PROPERTY", "name") => path.push_str(name),
                        (_, v) => path.push_str(&format!("{{{v}}}")),
                    }
                }
                out.insert(path);
                rest = &rest[end..];
            }
            _ => return out,
        }
    }
}
