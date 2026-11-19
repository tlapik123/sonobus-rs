//! `sonobus`: the desktop app (`--headless`: the same controller without a window).

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use eframe::egui;
use sonobus_app::controller::{AppController, ControllerConfig};
use sonobus_app::links::{self, USAGE};
use sonobus_app::settings;
use sonobus_app::ui::SonobusApp;

fn main() -> ExitCode {
    let cli = match links::parse_args(std::env::args().skip(1)) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    if cli.help {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    if cli.version {
        println!("sonobus version {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }
    let settings_dir = cli.settings_dir.as_ref().map(PathBuf::from).or_else(settings::app_dir);
    let mut ctrl = match AppController::new(ControllerConfig { settings_dir, use_devices: !cli.no_audio }) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("sonobus: cannot start the engine: {e}");
            return ExitCode::FAILURE;
        }
    };
    let form = ctrl.apply_cli(&cli);
    if cli.headless {
        // the command line is the user's explicit choice: no confirmation for a new server
        ctrl.answer_pending_link(true);
        ctrl.answer_pending_invite(&form.user, true);
        headless(ctrl, cli.print_invite);
        return ExitCode::SUCCESS;
    }
    // SONOBUS_WINDOW_SIZE=WxH: initial size (layout checks at phone size); else the last one
    let saved = ctrl.window();
    let size = std::env::var("SONOBUS_WINDOW_SIZE")
        .ok()
        .and_then(|s| s.split_once('x').and_then(|(w, h)| Some([w.parse().ok()?, h.parse().ok()?])))
        .or(saved.map(|w| [w[2].max(320) as f32, w[3].max(400) as f32]))
        .unwrap_or([960.0, 760.0]);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("SonoBus")
        .with_app_id("sonobus-rs")
        .with_inner_size(size)
        .with_min_inner_size([320.0, 400.0]);
    if let Some(w) = saved {
        viewport = viewport.with_position([w[0] as f32, w[1] as f32]);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    match eframe::run_native("SonoBus", options, Box::new(move |_cc| Ok(Box::new(SonobusApp::new(ctrl, form))))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("sonobus: {e} (run with --headless to use it without a window)");
            ExitCode::FAILURE
        }
    }
}

/// Runs until killed, logging status and peers; settings autosave every 30 s. The invite link
/// and PIN of a hosted secure session only with `print_invite` (`--print-invite`): they are
/// secrets.
fn headless(mut ctrl: AppController, print_invite: bool) {
    if let Some((port, addrs)) = &ctrl.vm().hosting {
        println!("hosting a session on port {port}; others connect to {addrs:?} port {port}");
    }
    let (mut status, mut peers, mut audio) = (String::new(), Vec::new(), String::new());
    let mut secret = (None, None);
    loop {
        ctrl.tick();
        let vm = ctrl.vm();
        if print_invite && let Some(s) = &vm.secure {
            // the link changes once the relay URL is known, and with every new key
            let now = (s.invite.clone(), s.pin.clone());
            if now != secret {
                if let Some(l) = &now.0 {
                    println!("invite: {l}");
                }
                if let Some(p) = &now.1 {
                    println!("PIN: {p}");
                }
                secret = now;
            }
        }
        let a = match &vm.audio.state {
            Some(sonobus_app::audio::DeviceState::Retrying { error, .. }) => {
                format!("audio device error: {} (retrying; null audio meanwhile)", sonobus_app::clean(error, 400))
            }
            Some(s) => format!("audio: {s:?}"),
            None => String::new(),
        };
        if a != audio {
            println!("{a}");
            audio = a;
        }
        if vm.status != status {
            status.clone_from(&vm.status);
            println!("{status}");
        }
        let now: Vec<String> = vm.snapshot.peers.iter().map(|p| p.user.clone()).collect();
        if now != peers {
            println!("peers: {now:?}");
            peers = now;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
