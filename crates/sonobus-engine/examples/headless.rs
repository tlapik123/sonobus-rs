//! Headless test client: joins a group on a connection server (or connects directly to a
//! peer) and logs peers, formats, latency and levels once a second while sending a test tone
//! or silence. No audio device: the example runs `EngineAudio::process` on a real-time
//! schedule itself and discards the output (only its meter is shown).
//!
//! ```sh
//! # join group "test" (password "pw") on a server, send a 440 Hz tone, run for 60 s
//! mise exec -- cargo run -p sonobus-engine --example headless -- \
//!     --server 127.0.0.1:10998 --user rusty --group test --password pw --tone 440 --seconds 60
//! # serverless: connect straight to a peer's UDP port (it needs "Use Specific UDP Port")
//! mise exec -- cargo run -p sonobus-engine --example headless -- --direct 192.168.1.20:11000
//! # modern (protocol v2) session: host one and print its invite link, then join it elsewhere
//! mise exec -- cargo run -p sonobus-engine --example headless -- --host-session jam
//! mise exec -- cargo run -p sonobus-engine --example headless -- --join 'sonobus2://join/...'
//! ```
//!
//! Options: `--server host:port`, `--user`, `--group`, `--password`, `--public`,
//! `--direct host:port`, `--port N` (local UDP port, 0 = any), `--tone HZ` (0 = silence),
//! `--format N` (send format index, 0..10, default 4 = Opus 96 kbps/ch), `--stereo`,
//! `--seconds N` (default: run until killed).
//!
//! Modern sessions: `--host-session NAME` (with `--broadcast` for a broadcast session,
//! `--password` / `--pin` for password or PIN joins, `--lan` to advertise it on the LAN),
//! `--join LINK` (invite link), `--relay none|n0|URL` (default `none` here: direct UDP only;
//! the shipped app defaults to `n0`, iroh's public relays), `--modern-port N` (default 10999,
//! taken → ephemeral).

use std::net::ToSocketAddrs;
use std::time::{Duration, Instant};

use sonobus_core::state::AppState;
use sonobus_engine::formats::FORMATS;
use sonobus_engine::modern::{HostOptions, LanScope, NetOptions, RelayChoice, SessionMode};
use sonobus_engine::{AudioConfig, Engine, EngineConfig, GroupJoin};

const SR: u32 = 48000;
const BLOCK: usize = 256;

fn main() {
    let mut args = std::env::args().skip(1);
    let (mut server, mut direct) = (None, None);
    let (mut user, mut group, mut password) = ("rust-headless".to_owned(), None, String::new());
    let (mut public, mut stereo) = (false, false);
    let (mut port, mut tone, mut format, mut seconds) = (0u16, 440.0f64, 4i32, f64::INFINITY);
    let (mut host_session, mut join, mut broadcast, mut pin, mut lan) =
        (None, None, false, false, false);
    let (mut relay, mut modern_port) = (RelayChoice::None, None);
    while let Some(a) = args.next() {
        let mut val = || args.next().unwrap_or_else(|| panic!("{a} needs a value"));
        match a.as_str() {
            "--server" => server = Some(val()),
            "--direct" => direct = Some(val()),
            "--user" => user = val(),
            "--group" => group = Some(val()),
            "--password" => password = val(),
            "--public" => public = true,
            "--stereo" => stereo = true,
            "--port" => port = val().parse().expect("port"),
            "--tone" => tone = val().parse().expect("tone"),
            "--format" => format = val().parse().expect("format"),
            "--seconds" => seconds = val().parse().expect("seconds"),
            "--host-session" => host_session = Some(val()),
            "--join" => join = Some(val()),
            "--broadcast" => broadcast = true,
            "--pin" => pin = true,
            "--lan" => lan = true,
            "--modern-port" => modern_port = Some(val().parse().expect("port")),
            "--relay" => {
                relay = match val().as_str() {
                    "none" => RelayChoice::None,
                    "n0" => RelayChoice::N0Default,
                    url => RelayChoice::Custom(vec![url.to_owned()]),
                }
            }
            other => panic!("unknown option {other}"),
        }
    }

    let mut state = AppState::new(false);
    state.params.def_send_qual = format;
    state.params.send_channels = if stereo { 2 } else { 1 };
    let audio = AudioConfig {
        sample_rate: SR,
        block_size: BLOCK,
        input_channels: 1,
        output_channels: 2,
    };
    let (engine, mut rt) = Engine::new(EngineConfig {
        audio,
        udp_port: port,
        state,
    })
    .expect("engine");
    println!("local UDP port {}", engine.local_port());

    let mut net = NetOptions::new(&user);
    net.relay = relay;
    if let Some(p) = modern_port {
        net.port = p;
    }
    if let Some(name) = host_session {
        let mut o = HostOptions::new(&name);
        if broadcast {
            o.mode = SessionMode::Broadcast;
        }
        o.pin = pin;
        o.password = (!password.is_empty()).then(|| password.clone());
        o.lan = if lan { LanScope::Network } else { LanScope::Off };
        let st = engine.host_session(&net, o).expect("host session");
        println!("hosting {name}: invite {}", st.invite.unwrap_or_default());
        if let Some(pin) = st.pin {
            println!("PIN {pin}");
        }
    } else if let Some(link) = join {
        engine.join_invite(&net, &link).expect("join");
    } else if let Some(s) = server {
        let (host, port) = s.rsplit_once(':').map_or((s.as_str(), 10998), |(h, p)| {
            (h, p.parse().expect("server port"))
        });
        let g = group.map(|name| GroupJoin {
            name,
            password,
            public,
        });
        engine.connect(host, port, &user, "", g).expect("connect");
    } else if let Some(d) = direct {
        let addr = d
            .to_socket_addrs()
            .expect("address")
            .find(|a| a.is_ipv4())
            .expect("IPv4 address");
        engine.connect_direct(addr);
    } else {
        eprintln!("need --server, --direct, --host-session or --join");
        return;
    }

    let start = Instant::now();
    let mut next = start;
    let mut last_log = start;
    let mut phase = 0.0f64;
    let mut input = vec![0.0f32; BLOCK];
    let mut out = [vec![0.0f32; BLOCK], vec![0.0f32; BLOCK]];
    while start.elapsed().as_secs_f64() < seconds {
        for x in &mut input {
            *x = 0.25 * (phase * std::f64::consts::TAU).sin() as f32;
            phase = (phase + tone / f64::from(SR)).fract();
        }
        let [l, r] = &mut out;
        rt.process(&[&input], &mut [&mut l[..], &mut r[..]]);

        while let Some(e) = engine.poll_event() {
            println!("event: {e:?}");
        }
        while let Some(e) = engine.poll_session_event() {
            println!("session event: {e:?}");
        }
        if last_log.elapsed() >= Duration::from_secs(1) {
            last_log = Instant::now();
            let s = engine.snapshot();
            println!(
                "{:?} user={} group={:?} out peak={:?}",
                s.connection, s.user, s.group, s.output.peak
            );
            for p in &s.peers {
                println!(
                    "  {} {} recv={} send={} fmt_out={} fmt_in={:?} ch_in={} buf={:.1}ms drop={} ping={:.1}ms rtt={:.1}ms level={:?} in={:.0}kbps out={:.0}kbps",
                    p.user,
                    p.addr,
                    p.recv_active,
                    p.send_active,
                    FORMATS[p.send_format].name,
                    p.recv_format,
                    p.recv_channels,
                    p.buffer_ms,
                    p.packets_dropped,
                    p.latency.ping_ms,
                    p.latency.round_trip_ms,
                    p.meter.peak,
                    p.recv_bps / 1000.0,
                    p.send_bps / 1000.0,
                );
            }
        }
        next += Duration::from_secs_f64(BLOCK as f64 / f64::from(SR));
        match next.checked_duration_since(Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = Instant::now(),
        }
    }
}
