//! Controller fixes from the security/robustness review: setup files cannot carry machine
//! settings (OSC exposure), a rebuild during a recording reports the files, a failed rebuild
//! leaves a working engine, a second instance on one settings directory saves nothing, hosting
//! stops on disconnect, unblocking.

use std::time::{Duration, Instant};

use sonobus_app::audio::DeviceSetup;
use sonobus_app::controller::{AppController, ConnectRequest, ControllerConfig};
use sonobus_app::osc_control::OscConfig;

fn controller() -> AppController {
    AppController::new(ControllerConfig { settings_dir: None, use_devices: false }).expect("controller")
}

fn temp_dir(name: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sonobus-review-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn until(c: &mut AppController, secs: u64, what: &str, cond: impl Fn(&AppController) -> bool) {
    let t = Instant::now();
    while !cond(c) {
        assert!(t.elapsed() < Duration::from_secs(secs), "timed out: {what} ({})", c.vm().status);
        c.tick();
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn a_setup_file_cannot_turn_on_network_osc() {
    let dir = temp_dir("setup");
    // someone else's setup with network OSC on, their UDP port and language
    let mut theirs = controller();
    theirs.set_osc_config(OscConfig { enabled: true, allow_network: true, receive_port: 1, target_host: "203.0.113.5".into(), ..OscConfig::default() });
    theirs.edit(|s| {
        s.params.wet = 0.3;
        s.extra.use_udp_port = 2;
        s.extra.lang_override_code = "ja".into();
    });
    let f = dir.join("band.sonobus");
    theirs.save_setup(&f).unwrap();
    let text = std::fs::read_to_string(&f).unwrap();
    assert!(!text.contains("203.0.113.5") && !text.contains("OSCEnabled"), "own OSC config not shared");

    let mut c = controller();
    c.load_setup(&f).unwrap();
    let x = c.engine().state().extra;
    assert_eq!(c.vm().state.params.wet, 0.3, "the mix applies");
    assert_eq!((x.use_udp_port, x.lang_override_code.as_str()), (0, ""));
    assert!(!OscConfig::load(&x).enabled);
    assert!(!c.vm().osc.config.enabled);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn rebuild_while_recording_reports_the_files() {
    let dir = temp_dir("rec");
    let mut c = controller();
    c.set_record_dir(&dir);
    c.start_recording();
    until(&mut c, 5, "recording", |c| c.vm().recording.is_some());
    std::thread::sleep(Duration::from_millis(200));
    let setup = DeviceSetup { sample_rate: 44_100, ..c.audio_setup().clone() };
    c.set_audio_setup(setup).unwrap();
    assert!(c.vm().recording.is_none());
    assert!(!c.vm().last_recording.is_empty(), "files of the stopped recording");
    assert!(c.vm().last_recording[0].exists());
    assert!(c.vm().status.contains("audio setup changed"), "{}", c.vm().status);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_failed_rebuild_keeps_an_engine() {
    let mut c = controller();
    let bad = DeviceSetup { sample_rate: 0, ..c.audio_setup().clone() };
    assert!(c.set_audio_setup(bad).is_err());
    assert!(c.vm().engine_error.is_some());
    c.tick(); // no panic: an engine runs
    assert!(c.engine().audio_config().sample_rate > 0);
    c.retry_engine();
    assert!(c.vm().engine_error.is_none(), "the fallback setup works");
}

#[test]
fn second_instance_on_one_settings_dir_saves_nothing() {
    let dir = temp_dir("two");
    let cfg = || ControllerConfig { settings_dir: Some(dir.clone()), use_devices: false };
    let mut a = AppController::new(cfg()).unwrap();
    a.edit(|s| s.params.wet = 0.4);
    a.save().unwrap();
    let mut b = AppController::new(cfg()).unwrap();
    assert!(b.vm().settings_read_only && b.vm().status_is_error);
    assert!(!a.vm().settings_read_only);
    assert!(dir.join("SonoBus.settings").exists(), "b's sentinel did not move a's settings");
    assert_eq!(b.vm().state.params.wet, 0.4, "b reads them");
    b.edit(|s| s.params.wet = 0.9);
    b.save().unwrap();
    drop(b);
    drop(a);
    let c = AppController::new(cfg()).unwrap();
    assert_eq!(c.vm().state.params.wet, 0.4, "b saved nothing");
    assert!(!c.vm().settings_read_only, "the lock is free again");
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn hosting_stops_on_disconnect_unless_kept() {
    let mut c = controller();
    let r = ConnectRequest { user: "a".into(), group: "g".into(), ..ConnectRequest::default() };
    c.host_session(0, r.clone()).unwrap();
    c.disconnect();
    assert!(c.vm().hosting.is_none());
    c.keep_hosting = true;
    c.host_session(0, r).unwrap();
    c.disconnect();
    assert!(c.vm().hosting.is_some());
    c.shutdown();
    assert!(c.vm().hosting.is_none(), "stopped on exit");
}

#[test]
fn unblock() {
    let mut c = controller();
    let ip = "192.0.2.1".parse().unwrap();
    c.engine().set_blocked_addresses(vec![ip]);
    c.tick();
    assert_eq!(c.vm().blocked, [ip]);
    c.unblock(ip);
    assert!(c.vm().blocked.is_empty() && c.engine().blocked_addresses().is_empty());
}

// ---- secure sessions: secrets stay on this machine ---------------------------------------------

fn secure_controller(dir: &std::path::Path) -> AppController {
    let mut c = AppController::new(ControllerConfig { settings_dir: Some(dir.to_path_buf()), use_devices: false }).expect("controller");
    c.net_place = sonobus_app::secure::NetPlace { bind_ip: std::net::Ipv4Addr::LOCALHOST.into(), port: 0, lan: sonobus_engine::modern::LanScope::Loopback };
    c.set_secure_relay(sonobus_engine::modern::RelayChoice::None);
    c
}

/// The identity key as stored in our settings file.
fn stored_identity(dir: &std::path::Path) -> String {
    let text = std::fs::read_to_string(dir.join("SonoBus.settings")).unwrap();
    let i = text.find("sonobusRsIdentityKey").expect("identity in our settings");
    let v = &text[i..];
    let v = &v[v.find("val=\"").unwrap() + 5..];
    v[..64].to_owned()
}

fn hosted_endpoint(c: &mut AppController, form: &sonobus_app::secure::HostForm) -> ([u8; 32], String) {
    c.host_secure("me", form);
    until(c, 10, "hosting", |c| c.vm().secure.as_ref().is_some_and(|s| s.invite.is_some()));
    let s = c.vm().secure.clone().unwrap();
    (s.endpoint_id, s.invite.unwrap())
}

#[test]
fn identity_persists_and_secure_secrets_stay_out_of_shared_files() {
    let dir = temp_dir("identity");
    let form = sonobus_app::secure::HostForm { session: "Kept".into(), lan: false, remember: true, ..Default::default() };
    let mut c = secure_controller(&dir);
    let key = stored_identity(&dir); // stored on first start
    let (id1, link) = hosted_endpoint(&mut c, &form);
    let code = link.trim_start_matches("sonobus2://join/").to_owned();
    // a setup file, the shareable state (plugin projects), share links, the OSC state: none
    // carries the identity, the invite or anything secure
    let f = dir.join("mine.sonobus");
    c.save_setup(&f).unwrap();
    let setup = std::fs::read_to_string(&f).unwrap();
    let shared = format!("{:?}", sonobus_app::settings::shareable(&c.engine().state()));
    for text in [&setup, &shared] {
        assert!(!text.contains(&key) && !text.contains(&code) && !text.contains("sonobusRsSecure") && !text.contains("sonobusRsIdentity") && !text.contains("ModernPeerNames"), "{text}");
    }
    assert!(c.group_link().is_none());
    let osc = format!("{:?}", sonobus_app::osc_control::state(&c, 0.0, false));
    assert!(!osc.contains(&code) && !osc.contains(&key));
    // a setup someone else saved cannot bring an identity or remembered sessions with it
    let mut theirs = c.engine().state();
    theirs.extra.unknown.push(("sonobusRsIdentityKey".into(), sonobus_core::Var::String("00".repeat(32))));
    sonobus_app::settings::keep_local(&mut theirs, &c.engine().state());
    assert!(!theirs.extra.unknown.iter().any(|(k, _)| k.starts_with("sonobusRs")));
    c.shutdown();
    let settings = std::fs::read_to_string(dir.join("SonoBus.settings")).unwrap();
    assert!(settings.contains(&code), "remembered with its link (the user asked)");
    drop(c);

    // a restart: the same identity, the remembered session hosts with the same link
    let mut c = secure_controller(&dir);
    assert_eq!(stored_identity(&dir), key);
    let r = &c.vm().secure_prefs.recents[0];
    assert_eq!((r.name.as_str(), r.invite.as_deref()), ("Kept", Some(link.as_str())));
    let (id2, link2) = hosted_endpoint(&mut c, &form);
    assert_eq!(id2, id1);
    assert_eq!(sonobus_net::auth::Invite::parse(&link2).unwrap().seed, sonobus_net::auth::Invite::parse(&link).unwrap().seed);
    // without "remember": the session is remembered, its link is not
    c.forget_secure_recent(0);
    c.host_secure("me", &sonobus_app::secure::HostForm { remember: false, ..form.clone() });
    until(&mut c, 10, "remembered", |c| c.vm().secure_prefs.recents.first().is_some_and(|r| r.name == "Kept"));
    assert_eq!(c.vm().secure_prefs.recents[0].invite, None);
    c.save().unwrap();
    let invite_now = c.vm().secure.as_ref().unwrap().invite.clone().unwrap();
    assert!(!std::fs::read_to_string(dir.join("SonoBus.settings")).unwrap().contains(invite_now.trim_start_matches("sonobus2://join/")));
    // "Reset device identity": a new one, from the next session on
    c.leave_secure();
    c.reset_identity();
    assert_ne!(stored_identity(&dir), key);
    let (id3, _) = hosted_endpoint(&mut c, &form);
    assert_ne!(id3, id1);
    drop(c);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Runs the `sonobus` binary headless for a few seconds; its output.
fn headless_output(dir: &std::path::Path, extra: &[&str]) -> String {
    use std::io::BufRead;
    let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_sonobus"))
        .args(["--headless", "--no-audio", "--settings-dir"])
        .arg(dir)
        .args(["--host-session", "cli-jam", "--pin", "--relay", "none"])
        .args(extra)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let out = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for l in std::io::BufReader::new(out).lines().map_while(Result::ok) {
            let _ = tx.send(l);
        }
    });
    let mut lines = Vec::new();
    let t = Instant::now();
    while t.elapsed() < Duration::from_secs(4) {
        if let Ok(l) = rx.recv_timeout(Duration::from_millis(100)) {
            lines.push(l);
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    lines.join("\n")
}

#[test]
fn headless_prints_the_invite_only_when_asked() {
    let dir = temp_dir("headless");
    let quiet = headless_output(&dir, &[]);
    assert!(quiet.contains("Hosting secure session: cli-jam"), "{quiet}");
    let key = stored_identity(&dir);
    assert!(!quiet.contains("sonobus2://") && !quiet.contains("PIN") && !quiet.contains(&key), "{quiet}");
    let loud = headless_output(&dir, &["--print-invite"]);
    assert!(loud.contains("invite: sonobus2://join/") && loud.contains("PIN: "), "{loud}");
    assert!(!loud.contains(&key), "never the identity");
    let _ = std::fs::remove_dir_all(&dir);
}
