//! Golden files written by JUCE 7.0.8 itself (SonoBus 1.7.2's bundled deps/juce) with
//! `.locki/tmp/state-ref/main.cpp` (build.sh there writes `tests/data/state/`).
//!
//! The XML/base64/binary/PropertiesFile serialization in those files is real JUCE output. The
//! SonoBus state trees in them are JUCE-serialized but HAND-ASSEMBLED: the harness replays the
//! `setProperty`/`appendChild` sequences copied from upstream's getValueTree/getState code,
//! with the values that [`fixture_state`] below mirrors. `*_stock.*` files replicate upstream's
//! EQ `p1q` key bug; `*_ours.*` files write `p2q` as sonobus-rs does.

use sonobus_core::files::{
    GlobalState, MAX_FILE_SIZE, PropertiesFile, SoundSample, Soundboard, Soundboards,
    StandaloneSettings, load_plugin_default, load_settings, load_settings_checked, load_setup,
    load_setup_checked, save_plugin_default, save_settings, save_setup,
};
use sonobus_core::juce_xml::{
    TextFormat, XmlElement, copy_xml_to_binary, from_base64, get_xml_from_binary, to_base64,
};
use sonobus_core::state::{
    AppState, ChannelGroup, Dynamics, MonitorDelay, ParametricEq, PeerStateCache, Scope,
    ServerConnectionInfo,
};
use sonobus_core::valuetree::{ValueTree, Var};

macro_rules! data {
    ($f:literal) => {
        include_bytes!(concat!("data/state/", $f)).as_slice()
    };
}

fn text(b: &[u8]) -> &str {
    std::str::from_utf8(b).unwrap()
}

const DEVICE_SETUP: &str = "<DEVICESETUP deviceType=\"ALSA\" audioOutputDeviceName=\"Built-in Audio Analog Stereo\" audioInputDeviceName=\"Built-in Audio Analog Stereo\" audioDeviceRate=\"48000.0\" audioDeviceBufferSize=\"256\" audioDeviceInChans=\"11\" audioDeviceOutChans=\"11\"/>";

fn all_types_tree() -> ValueTree {
    let mut root = ValueTree::new("Root");
    let props = [
        ("i", Var::Int(42)),
        ("neg", Var::Int(-7)),
        ("i64", Var::Int64(0x0123456789abcdef)),
        ("t", Var::Bool(true)),
        ("f", Var::Bool(false)),
        ("d", Var::Double(3.25)),
        ("fl", Var::Double(f64::from(-0.596f32))),
        ("zero", Var::Double(0.0)),
        ("big", Var::Double(1234567.0)),
        ("tiny", Var::Double(0.000006)),
        ("small", Var::Double(0.00006)),
        ("third", Var::Double(1.0 / 3.0)),
        ("thou", Var::Double(1234.5678)),
        ("hund", Var::Double(-192000.0)),
        ("e40", Var::Double(1e40)),
        ("s", Var::String("héllo € 🎵".into())),
        (
            "awk",
            Var::String("q\"uo'te & <tag> \t tab\nnl\rcr ;:,.!?#$%*+-=@[]^_`{|}~\\/".into()),
        ),
        ("e", Var::String(String::new())),
        ("bin", Var::Binary(vec![0, 1, 2, 0xff])),
        (
            "arr",
            Var::Array(vec![Var::Int(1), Var::String("x".into())]),
        ),
        ("v", Var::Void),
        ("u", Var::Undefined),
        ("long", Var::String("ab".repeat(40))),
    ];
    for (k, v) in props {
        root.set(k, v);
    }
    let mut a = ValueTree::new("A");
    a.set("x", Var::Int(1));
    let mut b = ValueTree::new("B");
    let mut c = ValueTree::new("C");
    c.set("y", Var::String("z".into()));
    b.children.push(c);
    root.children = vec![a, b];
    root
}

#[test]
fn xml_matches_juce() {
    let xml = all_types_tree().to_xml().unwrap();
    assert_eq!(
        xml.to_xml_string(TextFormat::DEFAULT),
        text(data!("alltypes.xml"))
    );
    assert_eq!(
        xml.to_xml_string(TextFormat::SINGLE_LINE_NO_HEADER),
        text(data!("alltypes_single.xml"))
    );

    // Reading back: everything is a string except base64: attributes.
    let back = ValueTree::from_xml_str(text(data!("alltypes.xml"))).unwrap();
    let expected = all_types_tree();
    assert_eq!(back.properties.len(), expected.properties.len());
    for ((kb, vb), (ke, ve)) in back.properties.iter().zip(&expected.properties) {
        assert_eq!(kb, ke);
        match ve {
            Var::Binary(_) => assert_eq!(vb, ve),
            _ => assert_eq!(
                vb,
                &Var::String(sonobus_core::juce_xml::var_to_string(ve)),
                "{kb}"
            ),
        }
    }
    assert_eq!(
        back.children[1].children[0].get("y"),
        Some(&Var::String("z".into()))
    );
    assert_eq!(back.to_xml().unwrap(), xml);
}

#[test]
fn xml_binary_matches_juce() {
    let xml = all_types_tree().to_xml().unwrap();
    assert_eq!(copy_xml_to_binary(&xml), data!("alltypes_xml.bin"));
    assert_eq!(get_xml_from_binary(data!("alltypes_xml.bin")).unwrap(), xml);
}

#[test]
fn base64_matches_juce() {
    for line in text(data!("base64.txt")).lines() {
        let (n, enc) = line.split_once(' ').unwrap();
        let bytes: Vec<u8> = (0..n.parse::<usize>().unwrap())
            .map(|i| (i * 37 + 11) as u8)
            .collect();
        assert_eq!(to_base64(&bytes), enc, "{n} bytes");
        assert_eq!(from_base64(enc).unwrap(), bytes, "{n} bytes");
    }
}

fn group(f: impl FnOnce(&mut ChannelGroup)) -> ChannelGroup {
    let mut g = ChannelGroup::default();
    f(&mut g);
    g
}

/// Mirrors `buildState` in .locki/tmp/state-ref/main.cpp.
fn fixture_state() -> AppState {
    let mut s = AppState::default();
    let p = &mut s.params;
    (p.in_gain, p.in_mon_mono_pan, p.in_mon_pan1, p.in_mon_pan2) = (1.5, 0.25, -0.5, 0.75);
    (p.dry, p.wet, p.def_net_buf, p.send_channels) = (0.3, 1.2, 0.02, 2);
    (p.met_enabled, p.send_met_audio, p.met_gain, p.met_tempo) = (true, true, 0.7, 120.0);
    (
        p.send_file_audio,
        p.send_soundboard_audio,
        p.hear_latency_test,
        p.met_is_recorded,
    ) = (true, true, true, false);
    (
        p.main_reverb_enabled,
        p.main_reverb_level,
        p.main_reverb_size,
        p.main_reverb_damp,
    ) = (true, 0.8, 0.4, 0.6);
    (p.main_reverb_predelay, p.main_reverb_model) = (35.0, 2);
    (
        p.main_send_mute,
        p.main_recv_mute,
        p.main_in_mute,
        p.main_monitor_solo,
    ) = (true, true, true, true);
    (
        p.def_net_auto,
        p.def_send_qual,
        p.dynamic_resampling,
        p.reconnect_last,
    ) = (3, 7, true, true);
    (p.def_peer_level, p.sync_met_host, p.sync_met_file) = (0.9, true, true);
    (
        p.in_reverb_level,
        p.in_reverb_size,
        p.in_reverb_damp,
        p.in_reverb_predelay,
    ) = (0.6, 0.3, 0.2, 10.0);

    s.recents = vec![
        ServerConnectionInfo {
            user_name: "alice".into(),
            user_password: "pw&<\"1".into(),
            group_name: "Jam Group".into(),
            group_password: "secrét".into(),
            server_host: "aoo.sonobus.net".into(),
            server_port: 10998,
            timestamp: 1700000000123,
            group_is_public: false,
        },
        ServerConnectionInfo {
            user_name: "bob".into(),
            group_name: "public1".into(),
            server_host: "example.org".into(),
            server_port: 10999,
            timestamp: 1600000000000,
            group_is_public: true,
            ..Default::default()
        },
    ];

    let x = &mut s.extra;
    x.use_udp_port = 11001;
    (x.change_qual_for_all, x.change_recv_qual_for_all) = (true, true);
    (
        x.default_recording_options,
        x.default_recording_format,
        x.default_recording_bits_per_sample,
    ) = (11, 3, 24);
    (
        x.record_self_pre_fx,
        x.record_self_silence_when_muted,
        x.record_finish_open,
    ) = (false, false, false);
    x.default_record_dir = Some("/home/u/Music/SonoBus".into());
    x.default_record_dir_url = "file:///home/u/Music/SonoBus".into();
    x.last_browse_dir = "/home/u/Music".into();
    (
        x.slider_snap_to_mouse,
        x.disable_key_shortcuts,
        x.peer_display_mode,
    ) = (false, true, 1);
    (
        x.last_chat_width,
        x.last_chat_shown,
        x.chat_font_size_offset,
        x.chat_fixed_width_font,
    ) = (300, true, -1, true);
    (
        x.last_soundboard_width,
        x.last_soundboard_shown,
        x.link_mon_delay_times,
    ) = (280, true, false);
    (x.last_username, x.lang_override_code, x.use_univ_font) = ("Zoë".into(), "de".into(), true);
    (x.last_window_width, x.last_window_height) = (1024, 768);
    (x.auto_drop_rate_thresh, x.reconnect_after_server_loss) = (2.5, false);
    let v = &mut x.video_link;
    (v.room_mode, v.show_names, v.screen_share, v.extra_params) =
        (false, false, true, "&quality=2".into());
    (v.be_director, v.large_share, v.push_view_mode) = (true, true, 2);

    let mut g0 = ChannelGroup::input_default(0, false);
    (g0.name, g0.muted, g0.gain, g0.pans, g0.pan_stereo) =
        ("Vocal".into(), true, 1.25, vec![-0.25], [-0.8, 0.9]);
    (
        g0.send_main_mix,
        g0.invert_polarity,
        g0.in_reverb_send,
        g0.mon_reverb_send,
    ) = (false, true, 0.1, 0.2);
    (g0.monitor, g0.mon_dest_start) = (0.8, 2);
    let dynamics =
        |enabled, threshold_db, ratio, attack_ms, release_ms, makeup_gain_db, auto_makeup| {
            Dynamics {
                enabled,
                threshold_db,
                ratio,
                attack_ms,
                release_ms,
                makeup_gain_db,
                auto_makeup,
            }
        };
    g0.compressor = dynamics(true, -20.0, 3.0, 5.0, 100.0, 6.0, false);
    g0.expander = dynamics(true, -50.0, 4.0, 2.0, 150.0, 0.0, true);
    g0.limiter = dynamics(false, -2.0, 5.0, 0.02, 120.0, 0.0, true);
    g0.eq = ParametricEq {
        enabled: true,
        low_shelf_gain: 2.0,
        low_shelf_freq: 80.0,
        para1_gain: -3.0,
        para1_freq: 200.0,
        para1_q: 2.5,
        para2_gain: 4.0,
        para2_freq: 1000.0,
        para2_q: 6.0,
        high_shelf_gain: -1.0,
        high_shelf_freq: 8000.0,
    };
    g0.monitor_delay = MonitorDelay {
        enabled: true,
        delay_time_ms: 25.0,
    };
    let mut g1 = ChannelGroup::input_default(1, false);
    (g1.name, g1.num_channels, g1.pans) = ("Keys".into(), 2, vec![0.1, -0.1]);
    s.input_groups = vec![g0, g1];

    s.file_playback_group = group(|g| {
        (g.name, g.num_channels, g.pans, g.gain, g.monitor) =
            ("File Playback".into(), 2, vec![0.0; 2], 0.5, 0.6)
    });
    s.metronome_group =
        group(|g| (g.name, g.monitor, g.mon_dest_start) = ("Metronome".into(), 0.4, 2));
    s.soundboard_group = group(|g| {
        (g.name, g.num_channels, g.pans, g.gain) = ("Soundboard".into(), 2, vec![0.0; 2], 0.9);
        g.monitor_delay = MonitorDelay {
            enabled: true,
            delay_time_ms: 10.0,
        };
    });

    s.peer_cache = vec![
        PeerStateCache {
            name: "carol".into(),
            netbuf: 25.5,
            netbufauto: 1,
            send_format: 9,
            level: 0.75,
            order_priority: 3,
            channel_groups: vec![
                group(|g| (g.gain, g.name) = (1.1, "carol mic".into())),
                group(|g| (g.chan_start, g.muted) = (1, true)),
            ],
            multi_channel_groups: vec![group(|g| {
                (g.name, g.num_channels) = ("multi".into(), 4);
                g.pans = (1..=4).map(|i| 0.1f32 * i as f32).collect();
            })],
            modified_chan_groups: true,
        },
        PeerStateCache {
            name: "dave".into(),
            netbufauto: 2,
            ..Default::default()
        },
    ];
    s
}

/// What reading a stock-written fixture yields: upstream wrote para2 Q under `p1q` and nothing
/// else, so both Qs come back as para2 Q.
fn as_read_from_stock(mut s: AppState) -> AppState {
    let groups = s
        .input_groups
        .iter_mut()
        .chain([
            &mut s.file_playback_group,
            &mut s.metronome_group,
            &mut s.soundboard_group,
        ])
        .chain(s.peer_cache.iter_mut().flat_map(|c| {
            c.channel_groups
                .iter_mut()
                .chain(&mut c.multi_channel_groups)
        }));
    for g in groups {
        g.eq.para1_q = g.eq.para2_q;
    }
    s
}

#[test]
fn state_writer_matches_juce() {
    let s = fixture_state();
    assert_eq!(s.to_tree().to_xml_string(), text(data!("state_ours.xml")));
    let mut bin = Vec::new();
    s.to_tree().write(&mut bin);
    assert_eq!(bin, data!("state_ours.bin"));
    assert_eq!(save_plugin_default(&s), data!("PluginDefault_ours.xml"));
}

#[test]
fn state_reads_our_files_exactly() {
    let s = fixture_state();
    assert_eq!(
        AppState::from_tree(&ValueTree::from_xml_str(text(data!("state_ours.xml"))).unwrap()),
        s
    );
    assert_eq!(
        AppState::from_tree(&ValueTree::read(data!("state_ours.bin")).unwrap()),
        s
    );
}

#[test]
fn state_reads_stock_files() {
    let expected = as_read_from_stock(fixture_state());
    assert_eq!(
        AppState::from_tree(&ValueTree::from_xml_str(text(data!("state_stock.xml"))).unwrap()),
        expected
    );
    assert_eq!(
        AppState::from_tree(&ValueTree::read(data!("state_stock.bin")).unwrap()),
        expected
    );
    assert!(!text(data!("state_stock.xml")).contains("p2q"));
}

#[test]
fn plugin_default_scope() {
    let full = fixture_state();
    for (file, stock) in [
        (data!("PluginDefault_ours.xml"), false),
        (data!("PluginDefault_stock.xml"), true),
    ] {
        let mut s = AppState::default();
        load_plugin_default(file, &mut s).unwrap();
        let src = if stock {
            as_read_from_stock(full.clone())
        } else {
            full.clone()
        };
        let expected = AppState {
            recents: vec![],
            input_groups: vec![],
            peer_cache: vec![],
            ..src
        };
        assert_eq!(s, expected);
    }
}

#[test]
fn setup_file_matches_juce() {
    let s = fixture_state();
    assert_eq!(
        save_setup(&s, Some(DEVICE_SETUP)),
        text(data!("setup_ours.sonobus"))
    );
    for (file, stock) in [
        (data!("setup_ours.sonobus"), false),
        (data!("setup_stock.sonobus"), true),
    ] {
        // a setup must not touch the recents
        let mut current = AppState {
            recents: vec![ServerConnectionInfo::default()],
            ..Default::default()
        };
        let audio = load_setup(file, &mut current).unwrap();
        assert_eq!(audio.as_deref(), Some(DEVICE_SETUP));
        let src = if stock {
            as_read_from_stock(s.clone())
        } else {
            s.clone()
        };
        let expected = AppState {
            recents: vec![ServerConnectionInfo::default()],
            peer_cache: vec![],
            ..src
        };
        assert_eq!(current, expected);
    }
}

fn fixture_settings() -> StandaloneSettings {
    StandaloneSettings {
        audio_setup: Some(DEVICE_SETUP.into()),
        should_override_sample_rate: false,
        should_check_for_new_version: true,
        allow_bluetooth_input: false,
        recent_setup_files: vec![
            "/home/u/Setups/band.sonobus".into(),
            "/home/u/Setups/Café & co.sonobus".into(),
        ],
        last_recents_setup_folder: "/home/u/Setups".into(),
        last_state_file: None,
        window: [Some(10), Some(20), Some(900), Some(700)],
        should_mute_input: None,
    }
}

#[test]
fn settings_file_matches_juce() {
    let golden = text(data!("SonoBus_ours.settings"));
    // A fresh file, set in stock's order, with one key we do not model in the middle.
    let mut p = PropertiesFile::default();
    StandaloneSettings {
        window: [None; 4],
        ..fixture_settings()
    }
    .store(&mut p);
    p.set("someFutureKey", "kept <verbatim> & \"quoted\"");
    let mut p2 = p.clone();
    p.set_processor_state(&fixture_state().to_tree());
    for (k, v) in ["windowX", "windowY", "windowW", "windowH"]
        .iter()
        .zip([10, 20, 900, 700])
    {
        p.set(k, v.to_string());
    }
    assert_eq!(p.to_xml_string(), golden);
    // save_settings does the same in one call
    assert_eq!(
        save_settings(&mut p2, &fixture_settings(), &fixture_state()),
        golden
    );

    // Loading keeps everything; writing back unchanged is byte-identical.
    for (file, stock) in [
        (data!("SonoBus_ours.settings"), false),
        (data!("SonoBus_stock.settings"), true),
    ] {
        let mut state = AppState::default();
        let (mut props, settings) = load_settings(file, &mut state).unwrap();
        assert_eq!(settings, fixture_settings());
        assert_eq!(
            props.get("someFutureKey"),
            Some("kept <verbatim> & \"quoted\"")
        );
        assert_eq!(
            state,
            if stock {
                as_read_from_stock(fixture_state())
            } else {
                fixture_state()
            }
        );
        assert_eq!(props.to_xml_string(), text(file));
        assert_eq!(
            save_settings(&mut props, &settings, &fixture_state()),
            golden
        );
    }
}

#[test]
fn legacy_binary_filter_state() {
    let mut p = PropertiesFile::default();
    p.set_processor_state_binary(&fixture_state().to_tree());
    let mut state = AppState::default();
    let (_, _) = load_settings(p.to_xml_string().as_bytes(), &mut state).unwrap();
    assert_eq!(state, fixture_state());
}

fn fixture_soundboards() -> Soundboards {
    Soundboards {
        selected: Some(1),
        hotkeys_muted: true,
        hotkeys_allow_numeric: false,
        boards: vec![
            Soundboard {
                name: "Board A".into(),
                samples: vec![
                    SoundSample {
                        name: "Applause".into(),
                        file_path: Some("/home/u/a b.wav".into()),
                        file_url: "file:///home/u/a%20b.wav".into(),
                        end_playback_behaviour: 1,
                        button_colour: 0xff8800,
                        hotkey: 49,
                        playback_behaviour: 1,
                        button_behaviour: 2,
                        replay_behaviour: 1,
                        gain: 0.8,
                        ..Default::default()
                    },
                    SoundSample {
                        name: "Horn & <stuff>".into(),
                        file_url: "https://example.org/h.wav".into(),
                        file_url_bookmark: Some(vec![1, 2, 3, 250]),
                        end_playback_behaviour: 2,
                        ..Default::default()
                    },
                ],
            },
            Soundboard {
                name: "Board B".into(),
                samples: vec![],
            },
        ],
    }
}

#[test]
fn soundboards_match_juce() {
    assert_eq!(
        fixture_soundboards().to_xml_string(),
        text(data!("soundboards.xml"))
    );
    assert_eq!(
        Soundboards::parse(data!("soundboards.xml")).unwrap(),
        fixture_soundboards()
    );
    // legacy sample: only `loop`, no behaviour keys
    let old = Soundboards::parse(b"<soundboards><soundboard name=\"x\"><samples><soundSample loop=\"1\"/></samples></soundboard></soundboards>").unwrap();
    assert_eq!(old.selected, Some(0));
    assert_eq!(
        old.boards[0].samples[0],
        SoundSample {
            end_playback_behaviour: 1,
            ..Default::default()
        }
    );
}

#[test]
fn global_state_matches_juce() {
    let g = GlobalState {
        blocked_addresses: vec!["192.168.1.5".into(), "10.0.0.7".into()],
        other: vec![],
    };
    assert_eq!(g.to_xml_string(), text(data!("GlobalState.xml")));
    assert_eq!(GlobalState::parse(data!("GlobalState.xml")).unwrap(), g);
}

#[test]
fn round_trip_with_unknowns() {
    let mut s = fixture_state();
    s.extra.unknown = vec![("ForkOption".into(), Var::Int(3))];
    let mut odd = ValueTree::new("PARAM");
    odd.set("id", Var::String("forkparam".into()));
    odd.set("value", Var::Double(0.5));
    s.unknown_children = vec![odd, ValueTree::new("ForkChild")];
    assert_eq!(AppState::from_tree(&s.to_tree()), s);
    let xml = ValueTree::from_xml_str(&s.to_tree().to_xml_string()).unwrap();
    let back = AppState::from_tree(&xml);
    assert_eq!(
        back.extra.unknown,
        vec![("ForkOption".into(), Var::String("3".into()))]
    );
    assert_eq!(back.unknown_children.len(), 2);
    assert_eq!(
        AppState {
            extra: s.extra.clone(),
            unknown_children: s.unknown_children.clone(),
            ..back
        },
        s
    );
}

#[test]
fn truncated_and_garbage_inputs_error() {
    for file in [
        data!("state_ours.xml"),
        data!("SonoBus_ours.settings"),
        data!("soundboards.xml"),
    ] {
        let end = text(file).trim_end().len();
        for n in (0..end).step_by(61).chain(end - 20..end) {
            assert!(XmlElement::parse(text(&file[..n])).is_err(), "prefix {n}");
            assert!(PropertiesFile::parse(&file[..n]).is_err(), "prefix {n}");
        }
    }
    for n in (0..data!("state_ours.bin").len() - 1).step_by(7) {
        assert!(
            ValueTree::read(&data!("state_ours.bin")[..n]).is_err(),
            "prefix {n}"
        );
    }
    let mut state = AppState::default();
    assert!(load_settings(b"\x00\xff garbage", &mut state).is_err());
    assert!(GlobalState::parse(b"<SonobusGlobalState>").is_err());
    assert!(Soundboards::parse(b"").is_err());
    assert!(get_xml_from_binary(&data!("alltypes_xml.bin")[..20]).is_err());
    assert_eq!(state, AppState::default());
}

/// Stock ignores an unreadable embedded state and still loads everything else
/// (SonoStandaloneFilterWindow.h:489-506, SonobusPluginEditor.cpp:2643-2700).
#[test]
fn bad_embedded_state_is_skipped() {
    let bad_xml = "<PROPERTIES><VALUE name=\"audioSetup\"><DEVICESETUP deviceType=\"ALSA\"/></VALUE><VALUE name=\"filterStateXML\" val=\"&lt;oops\"/><VALUE name=\"windowX\" val=\"10\"/></PROPERTIES>";
    let bad_b64 = "<PROPERTIES><VALUE name=\"filterState\" val=\"5.AAA\"/><VALUE name=\"lastRecentsSetupFolder\" val=\"/x\"/></PROPERTIES>";
    let mut state = fixture_state();
    let (props, settings) = load_settings(bad_xml.as_bytes(), &mut state).unwrap();
    assert_eq!(settings.window[0], Some(10));
    assert!(props.processor_state().is_err());
    let (_, settings, err) = load_settings_checked(bad_b64.as_bytes(), &mut state).unwrap();
    assert_eq!(settings.last_recents_setup_folder, "/x");
    assert_eq!(err, Some(sonobus_core::juce_xml::Error::Base64));
    assert_eq!(
        load_setup(bad_xml.as_bytes(), &mut state)
            .unwrap()
            .as_deref(),
        Some("<DEVICESETUP deviceType=\"ALSA\"/>")
    );
    let (_, err) = load_setup_checked(bad_xml.as_bytes(), &mut state).unwrap();
    assert!(matches!(err, Some(sonobus_core::juce_xml::Error::Xml(_))));
    assert_eq!(state, fixture_state());
    let (_, err) = load_setup_checked(data!("setup_ours.sonobus"), &mut state).unwrap();
    assert_eq!(err, None);
}

/// `mState.state = tree` resets every parameter the tree does not hold to its default
/// (juce_AudioProcessorValueTreeState.cpp:397-449); other parts keep their current values.
#[test]
fn foreign_tree_resets_params_only() {
    let mut state = fixture_state();
    load_plugin_default(b"<Other x=\"1\"/>", &mut state).unwrap();
    let expected = AppState {
        params: sonobus_core::state::Params::standalone(),
        ..fixture_state()
    };
    assert_eq!(state, expected);
}

#[test]
fn soundboard_selection_validated() {
    let sb = |sel: &str, boards: usize| {
        let b = "<soundboard name=\"x\"/>".repeat(boards);
        Soundboards::parse(format!("<soundboards selected=\"{sel}\">{b}</soundboards>").as_bytes())
            .unwrap()
            .selected
    };
    assert_eq!(
        (
            sb("99", 0),
            sb("0", 0),
            sb("1", 2),
            sb("2", 2),
            sb("-1", 2),
            sb("1.9", 2)
        ),
        (None, None, Some(1), None, None, Some(1))
    );
}

/// Parsing is linear in the number of attributes/properties/keys (JUCE appends while parsing).
#[test]
fn many_attributes_parse_quickly() {
    const N: usize = 200_000;
    let start = std::time::Instant::now();
    let mut xml = String::from("<ExtraState");
    for i in 0..N {
        xml.push_str(&format!(" k{i}=\"{i}\""));
    }
    xml.push_str(" k0=\"dup\"/>");
    let tree = ValueTree::from_xml_str(&xml).unwrap();
    assert_eq!(
        (tree.properties.len(), tree.get("k0")),
        (N + 1, Some(&Var::String("0".into())))
    );
    let text = tree.to_xml_string();
    let mut bin = Vec::new();
    tree.write(&mut bin);
    assert_eq!(
        ValueTree::read(&bin).unwrap().get("k0"),
        Some(&Var::String("dup".into()))
    );
    let mut root = ValueTree::new("SonoBusAoO");
    root.children.push(tree);
    let s = AppState::from_tree(&root);
    assert_eq!(s.extra.unknown.len(), N + 1);
    let _ = s.to_tree().to_xml_string();

    let mut props = String::from("<PROPERTIES>");
    for i in 0..N {
        props.push_str(&format!("<VALUE name=\"k{i}\" val=\"{i}\"/>"));
    }
    props.push_str("<VALUE name=\"k0\" val=\"last\"/></PROPERTIES>");
    let p = PropertiesFile::parse(props.as_bytes()).unwrap();
    // StringPairArray::set: a repeated key keeps its position and takes the last value
    assert_eq!(
        (p.entries.len(), p.get("k0"), p.entries[0].0.as_str()),
        (N, Some("last"), "k0")
    );
    assert!(text.len() > N);
    let elapsed = start.elapsed();
    assert!(elapsed.as_secs() < 20, "took {elapsed:?}");
}

#[test]
fn oversized_files_rejected() {
    let mut big = b"<PROPERTIES>".to_vec();
    big.resize(MAX_FILE_SIZE + 1, b' ');
    assert!(PropertiesFile::parse(&big).is_err());
    assert!(Soundboards::parse(&big).is_err());
    assert!(GlobalState::parse(&big).is_err());
    assert!(load_plugin_default(&big, &mut AppState::default()).is_err());
}

#[test]
fn stock_scope_constants() {
    assert_eq!(
        Scope::FULL,
        Scope {
            cache: true,
            input_groups: true
        }
    );
    assert!(
        !fixture_state()
            .to_tree_scoped(Scope::SETUP_FILE)
            .children
            .iter()
            .any(|c| c.type_name == "PeerStateCacheMap")
    );
}
