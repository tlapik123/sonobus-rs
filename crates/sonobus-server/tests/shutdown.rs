//! Graceful shutdown joins every thread and returns 0. Alone in its test binary: it counts the
//! process' threads.
#![cfg(target_os = "linux")]

use std::path::PathBuf;
use std::sync::mpsc::channel;
use std::time::Duration;

use sonobus_server::{Settings, Signal, serve};

/// Names of the process' threads.
fn threads() -> Vec<String> {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .map(|t| std::fs::read_to_string(t.unwrap().path().join("comm")).unwrap_or_default())
        .collect()
}

#[test]
fn stop_signal_leaves_no_threads_and_exits_0() {
    let key = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("shutdown.key");
    let s = Settings {
        legacy_bind: "127.0.0.1:0".parse().unwrap(),
        modern_bind: "127.0.0.1:0".parse().unwrap(),
        key_file: key,
        stats_interval: Duration::from_millis(50),
        ..Settings::default()
    };
    let before = threads();
    let (tx, rx) = channel();
    let t = std::thread::spawn(move || serve(&s, rx));
    std::thread::sleep(Duration::from_millis(300));
    assert!(threads().len() > before.len() + 3, "servers running");
    tx.send(Signal::Stats).unwrap();
    tx.send(Signal::Stop).unwrap();
    assert_eq!(t.join().unwrap(), 0);
    // A joined thread can linger in /proc for a moment while the kernel reaps it.
    let mut left = threads();
    for _ in 0..100 {
        if left.len() == before.len() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
        left = threads();
    }
    assert_eq!(left.len(), before.len(), "{before:?} -> {left:?}");
}
