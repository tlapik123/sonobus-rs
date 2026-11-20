use std::process::exit;
use std::sync::mpsc::{Receiver, channel};

use sonobus_server::{HELP, HELP_LIMITS, HELP_RELAY, Parsed, Signal, parse, serve};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let env: Vec<(String, String)> = std::env::vars_os()
        .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
        .collect();
    match parse(&args, &env) {
        Ok(Parsed::Help) => print!("{HELP}{HELP_RELAY}{HELP_LIMITS}"),
        Ok(Parsed::Version) => println!("sonobus-server {}", env!("CARGO_PKG_VERSION")),
        Ok(Parsed::Run(s)) => exit(serve(&s, signals())),
        Err(e) => {
            eprintln!("sonobus-server: {e}\nTry --help.");
            exit(2)
        }
    }
}

/// SIGINT/SIGTERM -> Stop, SIGUSR1 -> Stats. The signals are blocked before any other thread
/// starts (threads inherit the mask) and taken synchronously by one `sigwait` thread, so no
/// async-signal-safety concerns.
#[cfg(unix)]
fn signals() -> Receiver<Signal> {
    let (tx, rx) = channel();
    // SAFETY: plain libc calls on a local sigset.
    let set = unsafe {
        let mut set: libc::sigset_t = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        for s in [libc::SIGINT, libc::SIGTERM, libc::SIGUSR1] {
            libc::sigaddset(&mut set, s);
        }
        libc::pthread_sigmask(libc::SIG_BLOCK, &set, std::ptr::null_mut());
        set
    };
    std::thread::spawn(move || {
        loop {
            let mut sig = 0;
            // SAFETY: `set` is initialised and outlives the call.
            if unsafe { libc::sigwait(&set, &mut sig) } != 0 {
                continue;
            }
            let s = if sig == libc::SIGUSR1 {
                Signal::Stats
            } else {
                Signal::Stop
            };
            if tx.send(s).is_err() {
                return;
            }
        }
    });
    rx
}

/// Ctrl-C -> Stop (Windows has no SIGTERM/SIGUSR1), through tokio on a thread of its own.
#[cfg(not(unix))]
fn signals() -> Receiver<Signal> {
    let (tx, rx) = channel();
    std::thread::spawn(move || {
        if let Ok(rt) = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            rt.block_on(async {
                while tokio::signal::ctrl_c().await.is_ok() {
                    if tx.send(Signal::Stop).is_err() {
                        return;
                    }
                }
            });
        }
        // No handler: keep the sender, or `serve` would take the closed channel for Stop.
        std::mem::forget(tx);
    });
    rx
}
