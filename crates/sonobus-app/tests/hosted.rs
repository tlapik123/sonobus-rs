//! The controller in hosted mode (the plugin's), driven by a fake host: the host builds and
//! processes the engine, automatable parameters go both ways through it, soundboards and plugin
//! defaults persist through it, nothing touches settings files (the settings locations point
//! where nothing can be written), and a host sample-rate change swaps the engine while the
//! session (chat, peers, settings) stays. One test: it sets process environment variables.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use sonobus_app::controller::{AppController, ConnectRequest, ControllerConfig, Host};
use sonobus_core::state::{AppState, Params};
use sonobus_engine::{AudioConfig, Connection, Engine, EngineAudio, EngineConfig, Soundboards};

#[derive(Default)]
struct Log {
    params: Option<Params>,
    edits: Vec<(Params, Params)>,
    boards: usize,
    defaults: usize,
}

struct FakeHost(Arc<Mutex<Log>>);

impl Host for FakeHost {
    fn params(&self) -> Params {
        self.0
            .lock()
            .unwrap()
            .params
            .clone()
            .unwrap_or_else(Params::plugin)
    }
    fn params_edited(&mut self, old: &Params, new: &Params) {
        let mut l = self.0.lock().unwrap();
        l.edits.push((old.clone(), new.clone()));
        l.params = Some(new.clone()); // as the plugin's parameters take the value
    }
    fn soundboards_changed(&mut self, _: &Soundboards) -> std::io::Result<()> {
        self.0.lock().unwrap().boards += 1;
        Ok(())
    }
    fn save_default(&mut self, _: &AppState) -> std::io::Result<std::path::PathBuf> {
        self.0.lock().unwrap().defaults += 1;
        Ok("PluginDefault.xml".into())
    }
    fn reset_default(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn config(sample_rate: u32, block_size: usize) -> AudioConfig {
    AudioConfig {
        sample_rate,
        block_size,
        input_channels: 2,
        output_channels: 2,
    }
}

fn build(cfg: AudioConfig, state: AppState) -> std::io::Result<(Arc<Engine>, EngineAudio)> {
    let (e, rt) = Engine::new(EngineConfig {
        audio: cfg,
        udp_port: 0,
        state,
    })?;
    Ok((Arc::new(e), rt))
}

/// The host's audio thread: processes the engine in real time, silent input.
struct Driver(Arc<AtomicBool>, Option<JoinHandle<()>>);

impl Driver {
    fn start(mut rt: EngineAudio, cfg: AudioConfig) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let s = stop.clone();
        let h = std::thread::spawn(move || {
            let ins = vec![vec![0.0f32; cfg.block_size]; cfg.input_channels];
            let mut outs = vec![vec![0.0f32; cfg.block_size]; cfg.output_channels];
            let block = Duration::from_secs_f64(cfg.block_size as f64 / f64::from(cfg.sample_rate));
            let mut next = Instant::now();
            while !s.load(Ordering::Acquire) {
                let i: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
                let mut o: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
                rt.process(&i, &mut o);
                next += block;
                if let Some(d) = next.checked_duration_since(Instant::now()) {
                    std::thread::sleep(d);
                }
            }
        });
        Driver(stop, Some(h))
    }
}

impl Drop for Driver {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
        if let Some(h) = self.1.take() {
            let _ = h.join();
        }
    }
}

fn until(
    ctrls: &mut [&mut AppController],
    secs: u64,
    what: &str,
    cond: impl Fn(&[&mut AppController]) -> bool,
) {
    let t = Instant::now();
    loop {
        for c in ctrls.iter_mut() {
            c.tick();
        }
        if cond(ctrls) {
            return;
        }
        let status: Vec<_> = ctrls.iter().map(|c| c.vm().status.clone()).collect();
        assert!(
            t.elapsed() < Duration::from_secs(secs),
            "timed out waiting for {what}; status {status:?}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn hosted_controller_with_a_fake_host() {
    // every settings location under a regular file: nothing can be created there (even as root)
    let base = std::env::temp_dir().join(format!("sonobus-hosted-{}", std::process::id()));
    std::fs::create_dir_all(&base).unwrap();
    let blocker = base.join("not-a-dir");
    std::fs::write(&blocker, b"x").unwrap();
    let unwritable = blocker.join("home");
    // SAFETY: the only test in this binary, set before any other thread reads the environment
    unsafe {
        for k in ["HOME", "XDG_CONFIG_HOME", "APPDATA"] {
            std::env::set_var(k, &unwritable);
        }
    }
    assert!(sonobus_app::settings::app_dir().is_some_and(|d| d.starts_with(&unwritable)));

    let log = Arc::new(Mutex::new(Log::default()));
    let cfg = config(48_000, 256);
    let (engine, rt) = build(cfg, AppState::new(true)).unwrap();
    let mut drv = Some(Driver::start(rt, cfg));
    let mut c = AppController::hosted(
        engine,
        Box::new(FakeHost(log.clone())),
        Soundboards::default(),
    );
    assert!(c.is_hosted());
    assert!(c.vm().settings_dir.is_none());
    assert_eq!(c.vm().state.params.dry, 1.0, "plugin default monitor level");

    // a UI edit of an automatable parameter reaches the host; other settings do not
    c.edit(|s| s.params.dry = 0.25);
    c.edit(|s| s.extra.last_username = "alice".into());
    {
        let l = log.lock().unwrap();
        assert_eq!(l.edits.len(), 1);
        assert_eq!((l.edits[0].0.dry, l.edits[0].1.dry), (1.0, 0.25));
    }
    // host automation shows in the view model on the next tick
    log.lock().unwrap().params.as_mut().unwrap().wet = 0.5;
    c.tick();
    assert_eq!(c.vm().state.params.wet, 0.5);
    // soundboards and plugin defaults persist through the host
    c.edit_soundboards(|b| b.hotkeys_muted = !b.hotkeys_muted);
    c.save_plugin_default();
    let (boards, defaults) = {
        let l = log.lock().unwrap();
        (l.boards, l.defaults)
    };
    assert_eq!((boards, defaults), (1, 1));
    // no devices, no settings files
    assert!(
        c.set_audio_setup(sonobus_app::audio::DeviceSetup::default())
            .is_err()
    );
    c.save().unwrap();
    c.shutdown();
    assert!(!c.vm().status_is_error, "{}", c.vm().status);

    // a session with a peer
    let mut peer = AppController::new(ControllerConfig {
        settings_dir: None,
        use_devices: false,
    })
    .unwrap();
    let req = |user: &str| ConnectRequest {
        user: user.into(),
        group: "hosted".into(),
        ..ConnectRequest::default()
    };
    peer.host_session(0, req("bob")).expect("host");
    let (port, _) = peer.vm().hosting.clone().unwrap();
    c.connect(ConnectRequest {
        host: "127.0.0.1".into(),
        port,
        ..req("alice")
    });
    until(&mut [&mut c, &mut peer], 20, "peers", |x| {
        x.iter()
            .all(|x| x.vm().snapshot.peers.iter().any(|p| p.connected))
    });
    c.send_chat(&[], "before the rate change");

    // the host switches to 96 kHz: it stops processing, the controller swaps in its new engine
    drop(drv.take());
    let cfg2 = config(96_000, 512);
    let mut rt2 = None;
    c.swap_engine(|s| {
        let (e, rt) = build(cfg2, s)?;
        rt2 = Some(rt);
        Ok(e)
    })
    .unwrap();
    drv = Some(Driver::start(rt2.unwrap(), cfg2));
    assert_eq!(c.engine().audio_config().sample_rate, 96_000);
    assert_eq!(c.vm().state.params.dry, 0.25, "settings kept");
    assert_eq!(c.vm().state.extra.last_username, "alice");
    assert!(
        c.vm()
            .chat
            .iter()
            .any(|m| m.message == "before the rate change"),
        "chat kept"
    );
    assert_eq!(
        c.vm().request.as_ref().map(|r| r.group.as_str()),
        Some("hosted")
    );
    // the group is rejoined and the peer comes back
    until(&mut [&mut c, &mut peer], 20, "rejoin", |x| {
        x[0].vm().snapshot.connection == Connection::Connected
            && x.iter()
                .all(|x| x.vm().snapshot.peers.iter().any(|p| p.connected))
    });
    drop(drv);
    assert!(!unwritable.exists());
}
