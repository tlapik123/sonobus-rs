//! The built binary: --help, readiness line, SIGUSR1 stats, SIGTERM -> exit 0, port in use ->
//! exit 1.
#![cfg(unix)]

use std::io::{BufRead, BufReader};
use std::net::UdpSocket;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::channel;
use std::time::Duration;

const BIN: &str = env!("CARGO_BIN_EXE_sonobus-server");

fn tmp(name: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&p);
    p
}

#[test]
fn help_and_version() {
    let o = Command::new(BIN).arg("--help").output().unwrap();
    assert!(o.status.success());
    let out = String::from_utf8(o.stdout).unwrap();
    assert!(
        out.contains("USAGE") && out.contains("--legacy-max-groups"),
        "{out}"
    );
    let o = Command::new(BIN).arg("-V").output().unwrap();
    assert!(
        String::from_utf8(o.stdout)
            .unwrap()
            .starts_with("sonobus-server ")
    );
    let o = Command::new(BIN).arg("--nope").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn ready_stats_and_sigterm() {
    let mut child = Command::new(BIN)
        .args([
            "--legacy-bind",
            "127.0.0.1:0",
            "--modern-bind",
            "127.0.0.1:0",
        ])
        .arg("--key-file")
        .arg(tmp("bin.key"))
        .env("SONOBUS_SERVER_STATS_INTERVAL", "0")
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let (tx, rx) = channel();
    let err = child.stderr.take().unwrap();
    std::thread::spawn(move || {
        for l in BufReader::new(err).lines() {
            let _ = tx.send(l.unwrap());
        }
    });
    let next = |want: &str| loop {
        let l = rx.recv_timeout(Duration::from_secs(15)).expect(want);
        if l.contains(want) {
            return l;
        }
    };
    let ready = next(" INFO ready ");
    assert!(ready.contains("legacy=127.0.0.1:") && ready.contains("address=sonobus2://rdv/"));
    let kill = |sig| assert_eq!(unsafe { libc::kill(child.id() as i32, sig) }, 0);
    kill(libc::SIGUSR1);
    let stats = next(" INFO stats ");
    assert!(
        stats.contains("legacy_users=0 legacy_groups=0 legacy_refused=none")
            && stats.contains("modern_regs=0 modern_refused=none"),
        "{stats}"
    );
    kill(libc::SIGTERM);
    next(" INFO stopped");
    assert_eq!(child.wait().unwrap().code(), Some(0));
}

#[test]
fn port_in_use_exits_1() {
    let held = UdpSocket::bind("127.0.0.1:0").unwrap();
    let o = Command::new(BIN)
        .args(["--legacy-only", "--legacy-bind"])
        .arg(held.local_addr().unwrap().to_string())
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(1));
    let err = String::from_utf8(o.stderr).unwrap();
    assert!(err.contains("ERROR legacy server: cannot bind"), "{err}");
}
