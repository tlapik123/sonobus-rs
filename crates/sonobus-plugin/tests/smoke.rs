//! Host-less smoke tests through the `Plugin` trait: audio, sample-rate changes, state in
//! upstream's format, `PluginDefault.xml`, host transport, bypass, and two instances in one
//! process talking through a local `aoo::server` (with the per-peer aux outputs).

mod common;

use common::*;
use nice_plug::params::InternalParamMut;
use nice_plug::params::persist::PersistentField;
use sonobus_core::files;
use sonobus_core::juce_xml::from_base64;
use sonobus_core::state::{AppState, Scope};
use sonobus_core::valuetree::ValueTree;
use sonobus_plugin::session::PLUGIN_DEFAULT_FILE;

fn set_params(h: &Host, f: impl FnOnce(&mut sonobus_core::state::Params)) {
    let p = h.plugin.sb_params();
    let mut c = p.to_core();
    f(&mut c);
    p.set_from_core(&c);
    // the parameter thread forwards it within 10 ms
    std::thread::sleep(std::time::Duration::from_millis(60));
}

#[test]
fn sine_in_monitored_out_and_sample_rate_change() {
    let mut h = Host::new(440.0);
    assert!(h.activate(SR, BLOCK as u32, 0));
    for _ in 0..200 {
        h.block(BLOCK);
    }
    // plugin default: input monitored at dry 1.0
    let a = h.tone(0, 440.0);
    assert!(a > 0.05, "monitored input: {a}");
    assert!(h.out.iter().flatten().all(|x| x.is_finite()));
    // aux outputs are silent without peers
    assert!(h.out[2..].iter().flatten().all(|&x| x == 0.0));
    h.plugin.session().engine().unwrap().update_state(|s| s.extra.last_username = "kept".into());

    // the host switches to 44.1 kHz with bigger blocks: engine rebuilt, state kept
    assert!(h.activate(44_100.0, 512, 0));
    assert_eq!(h.plugin.session().engine().unwrap().audio_config().sample_rate, 44_100);
    assert_eq!(h.plugin.session().state().extra.last_username, "kept");
    h.out.iter_mut().for_each(|r| r.clear());
    for _ in 0..200 {
        h.block(512);
    }
    assert!(h.tone(0, 440.0) > 0.05);
    // the same configuration again is a no-op (hosts re-activate after state loads)
    let port = h.port();
    assert!(h.activate(44_100.0, 512, 0));
    assert_eq!(h.port(), port);
    // odd host callback sizes (engine runs one block late through its FIFO)
    for n in [100, 37, 512, 1, 300] {
        h.block(n);
    }
    assert!(h.out.iter().flatten().all(|x| x.is_finite()));
}

#[test]
fn mono_layouts() {
    for layout in [2, 3] {
        let mut h = Host::new(440.0);
        assert!(h.activate(SR, BLOCK as u32, layout));
        for _ in 0..200 {
            h.block(BLOCK);
        }
        assert!(h.tone(0, 440.0) > 0.05, "layout {layout}");
    }
}

#[test]
fn state_is_upstreams_binary_tree_and_round_trips() {
    let mut a = Host::new(440.0);
    assert!(a.activate(SR, BLOCK as u32, 0));
    set_params(&a, |p| {
        p.dry = 0.5;
        p.def_send_qual = 7;
    });
    a.plugin.session().engine().unwrap().update_state(|s| {
        s.extra.last_username = "alice".into();
        s.extra.use_udp_port = 0;
    });
    let field = &a.plugin.sb_params().state;
    let saved: String = field.map(|s| s.clone());
    // the field is JUCE base64 of `getStateInformation`: a binary `SonoBusAoO` ValueTree
    let bytes = from_base64(&saved).expect("JUCE base64");
    let tree = ValueTree::read(&bytes).expect("ValueTree binary");
    assert_eq!(tree.type_name, "SonoBusAoO");
    let st = AppState::from_tree(&tree);
    assert_eq!(st.params.dry, 0.5);
    assert_eq!(st.params.def_send_qual, 7);
    assert_eq!(st.extra.last_username, "alice");
    assert_eq!(st.input_groups.len(), 1, "input groups saved (full scope)");
    // byte-exact with the core model's writer
    let mut again = Vec::new();
    a.plugin.session().state().to_tree_scoped(Scope::FULL).write(&mut again);
    assert_eq!(again, bytes);

    // restore into a fresh instance, before and after activation
    let b = Host::new(0.0);
    b.plugin.sb_params().state.set(saved.clone());
    assert_eq!(b.plugin.session().state().extra.last_username, "alice");
    assert_eq!(b.plugin.session().state().params.dry, 0.5);
    let mut c = Host::new(0.0);
    assert!(c.activate(SR, BLOCK as u32, 0));
    c.plugin.sb_params().state.set(saved);
    let s = c.plugin.session().state();
    assert_eq!((s.extra.last_username.as_str(), s.params.def_send_qual), ("alice", 7));
    assert!(!s.params.met_enabled, "metronome never restored on");
    // garbage is ignored
    c.plugin.sb_params().state.set("not a state".into());
    c.plugin.sb_params().state.set("12.AAAA".into());
    assert_eq!(c.plugin.session().state().extra.last_username, "alice");
}

#[test]
fn plugin_default_file_applies_without_input_groups() {
    let dir = tmp_dir("plugdef");
    let mut st = AppState::new(true);
    st.params.wet = 0.7;
    st.params.def_send_qual = 2;
    st.extra.last_username = "bob".into();
    std::fs::write(dir.join(PLUGIN_DEFAULT_FILE), files::save_plugin_default(&st)).unwrap();
    let mut h = Host::with_dir(440.0, Some(dir.clone()));
    let p = h.plugin.sb_params();
    assert_eq!(p.wet.value(), 0.7);
    assert_eq!(p.def_send_qual.value(), 2);
    assert_eq!(p.dry.value(), 1.0);
    assert!(h.activate(SR, BLOCK as u32, 0));
    let s = h.plugin.session().state();
    assert_eq!(s.extra.last_username, "bob");
    assert_eq!(s.params.wet, 0.7);
    // save as default writes XML without cache or input groups; reset removes it
    let path = h.plugin.session().save_plugin_default().unwrap();
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.contains("<SonoBusAoO") && !text.contains("InputChannelGroups"));
    h.plugin.session().reset_plugin_default().unwrap();
    assert!(!path.exists());
    h.plugin.session().reset_plugin_default().unwrap();
}

#[test]
fn metronome_follows_host_tempo_and_beat() {
    let mut h = Host::new(0.0); // silent input
    assert!(h.activate(SR, BLOCK as u32, 1));
    set_params(&h, |p| {
        p.met_enabled = true;
        p.met_tempo = 100.0; // the host's 120 bpm must win (syncMetHost, plugin default on)
    });
    let t = &mut h.ctx.transport;
    (t.playing, t.tempo, t.pos_beats) = (true, Some(120.0), Some(0.25));
    let mut out = Vec::new();
    for _ in 0..(3 * 48_000 / BLOCK) {
        h.block(BLOCK);
        out.extend_from_slice(&h.main[0][..BLOCK]);
    }
    let onsets: Vec<usize> = (1..out.len())
        .filter(|&i| out[i].abs() > 1e-6 && out[i.saturating_sub(400)..i].iter().all(|x| x.abs() <= 1e-6))
        .collect();
    assert!(onsets.len() >= 2, "clicks: {onsets:?}");
    // beat n is at frame (n - 0.25) * 24000 at 120 bpm
    for &o in &onsets {
        let off = (o + 6_000) % 24_000;
        assert!(off <= 2 || off >= 23_998, "click at {o}, not on a host beat: {onsets:?}");
    }
    assert!(onsets.windows(2).all(|w| (w[1] - w[0]) % 24_000 <= 2), "{onsets:?}");
}

#[test]
fn bypass_passes_input_and_silences_aux() {
    let mut h = Host::new(440.0);
    assert!(h.activate(SR, BLOCK as u32, 0));
    // SAFETY: atomic store, as the wrapper does for host automation
    unsafe { h.plugin.sb_params().bypass._internal_set_plain_value(true) };
    h.aux_out.iter_mut().for_each(|c| c.fill(1.0));
    h.fill(BLOCK);
    let input = h.main[0][..BLOCK].to_vec();
    h.process(BLOCK, &mut |f| f());
    assert_eq!(&h.main[0][..BLOCK], &input[..]);
    assert!(h.aux_out.iter().all(|c| c[..BLOCK].iter().all(|&x| x == 0.0)));
}

#[test]
fn not_activated_process_is_harmless() {
    let mut h = Host::new(440.0);
    h.block(BLOCK);
    assert!(h.plugin.session().engine().is_none());
}

#[test]
fn two_instances_through_a_local_server() {
    let _s = serial();
    let srv = aoo::server::run("127.0.0.1:0".parse().unwrap()).expect("server");
    let (mut a, mut b) = (Host::new(440.0), Host::new(660.0));
    assert!(a.activate(SR, BLOCK as u32, 0));
    assert!(b.activate(SR, BLOCK as u32, 0));
    assert_ne!(a.port(), b.port(), "each instance has its own UDP socket");
    a.join(srv.local_addr(), "alice", "plug");
    b.join(srv.local_addr(), "bob", "plug");
    assert!(
        run_until(&mut [&mut a, &mut b], 15.0, |h| h[0].receiving() == 1 && h[1].receiving() == 1),
        "peers did not connect"
    );
    let ok = |h: &mut [&mut Host], f: &dyn Fn(&[&mut Host]) -> bool| {
        for _ in 0..16 {
            run(h, 0.25);
            if f(h) {
                return true;
            }
        }
        false
    };
    // b hears a on its main output and, alone, on "Aux 1 Out" (a is b's peer slot 0)
    assert!(ok(&mut [&mut a, &mut b], &|h| h[1].tone(0, 440.0) > 0.05), "b hears a: {}", b.tone(0, 440.0));
    assert!(ok(&mut [&mut a, &mut b], &|h| h[1].tone(2, 440.0) > 0.05), "aux 1: {}", b.tone(2, 440.0));
    assert!(b.tone(2, 660.0) < 0.005, "own input not on the peer bus");
    assert!(b.out[4..].iter().flatten().all(|&x| x == 0.0), "other aux outs silent");
    assert!(ok(&mut [&mut a, &mut b], &|h| h[0].tone(0, 660.0) > 0.05), "a hears b");

    // b's host changes the sample rate: engine rebuilt, session rejoined, audio flows again
    // (96 kHz with 512-frame blocks keeps the lockstep pacing real time)
    assert!(b.activate(96_000.0, 512, 0));
    b.block_len = 512;
    b.out.iter_mut().for_each(|r| r.clear());
    a.out.iter_mut().for_each(|r| r.clear());
    assert!(
        run_until(&mut [&mut a, &mut b], 20.0, |h| h[0].receiving() == 1 && h[1].receiving() == 1),
        "b did not rejoin"
    );
    assert!(ok(&mut [&mut a, &mut b], &|h| h[0].tone(0, 660.0) > 0.05), "a hears b again");
}
