//! Smoke test against whatever audio backends this machine has. Must pass with no sound
//! hardware at all; exercises real streams only where ALSA's `null` PCM exists.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Duration;

use sonobus_audio::{
    AudioBlock, DeviceWatcher, InputConfig, InputSource, Session, SessionConfig, devices, hosts,
};

#[test]
fn enumerate_every_host_tolerating_no_devices() {
    for host in hosts() {
        match devices(host) {
            Ok(list) => {
                println!("{host:?}: {} device(s)", list.len());
                for d in &list {
                    println!(
                        "  {} [{}] in {} out {} rates {:?} buf {:?} def_in {} def_out {} loopback {}",
                        d.name, d.id, d.input_channels, d.output_channels, d.sample_rates,
                        d.buffer_size, d.default_input, d.default_output, d.loopback
                    );
                    assert_eq!(d.id.to_string().parse::<sonobus_audio::DeviceId>().unwrap(), d.id);
                }
            }
            Err(e) => println!("{host:?}: enumeration failed: {e}"),
        }
        if let Ok(mut w) = DeviceWatcher::new(host) {
            // Nothing is plugged in between two polls in CI.
            assert_eq!(w.poll().ok().flatten(), None);
        }
    }
}

fn counting_engine(count: Arc<AtomicUsize>) -> impl FnMut(&mut AudioBlock<'_>) + Send + 'static {
    move |b: &mut AudioBlock<'_>| {
        assert_eq!(b.frames, 64);
        for (o, i) in b.outputs.iter_mut().zip(b.inputs.iter()) {
            o.copy_from_slice(i);
        }
        count.fetch_add(1, Relaxed);
    }
}

#[test]
fn alsa_null_device_duplex_input_only_and_reopen() {
    let Some(host) = hosts().into_iter().find(|h| h.name() == "ALSA") else { return };
    let Ok(list) = devices(host) else { return };
    let Some(null) = list.iter().find(|d| d.id.id() == "null") else {
        println!("no ALSA null PCM here");
        return;
    };
    println!("null: {null:?}");
    let base = SessionConfig {
        host,
        output: Some(null.id.clone()),
        output_channels: Some(2),
        inputs: vec![
            InputConfig { source: InputSource::Device(null.id.clone()), channels: Some(2) },
            InputConfig { source: InputSource::External { channels: 1, sample_rate: 44100 }, channels: None },
            InputConfig { source: InputSource::Loopback(null.id.clone()), channels: None },
        ],
        sample_rate: Some(48000),
        block_size: 64,
        buffer_frames: Some(256),
    };

    // ALSA cannot capture system sound.
    let count = Arc::new(AtomicUsize::new(0));
    let err = Session::open(base.clone(), counting_engine(count.clone())).err().expect("loopback on ALSA");
    assert_eq!(err.kind(), sonobus_audio::ErrorKind::UnsupportedOperation);

    // Output + device input + external input. The null PCM is unclocked (capture outruns
    // playback), so this exercises overflow and backlog skipping rather than steady state.
    let mut cfg = base.clone();
    cfg.inputs.pop();
    let mut s = Session::open(cfg, counting_engine(count.clone())).expect("open null duplex");
    let mut ext = s.take_external_input(1).expect("external producer");
    assert!(s.take_external_input(1).is_none());
    assert_eq!(ext.sample_rate(), 44100);
    std::thread::sleep(Duration::from_millis(300));
    ext.push(&[0.1f32; 441]);
    let ran = count.load(Relaxed);
    println!("duplex: {ran} blocks, stats {:?}, xruns {}", s.input_stats(), s.xruns());
    assert!(ran > 0, "engine never ran on the null device");
    assert!(s.stalled().is_empty(), "running streams reported stalled");
    for e in s.events() {
        println!("event: {e:?}");
    }
    s.reopen().expect("reopen");
    let before = count.load(Relaxed);
    std::thread::sleep(Duration::from_millis(200));
    assert!(count.load(Relaxed) > before, "engine not running after reopen");
    drop(s.close().expect("engine back"));

    // Input-only: the null capture stream drives the engine.
    let mut cfg = base.clone();
    cfg.output = None;
    cfg.inputs.truncate(1);
    count.store(0, Relaxed);
    let s = Session::open(cfg, counting_engine(count.clone())).expect("open input-only");
    std::thread::sleep(Duration::from_millis(300));
    println!("input-only: {} blocks", count.load(Relaxed));
    assert!(count.load(Relaxed) > 0, "input-only engine never ran");
    drop(s.close().expect("engine back"));

    // App capture: no PipeWire (unsupported), or no such application playing.
    let mut cfg = base;
    cfg.inputs = vec![InputConfig { source: InputSource::App("sonobus-test-no-such-app".into()), channels: None }];
    let err = Session::open(cfg, counting_engine(count)).err().expect("app capture");
    use sonobus_audio::ErrorKind::{DeviceNotAvailable, UnsupportedOperation};
    assert!(matches!(err.kind(), UnsupportedOperation | DeviceNotAvailable), "{err}");
}
