//! The daemon in-process on 127.x with ephemeral ports; relays disabled everywhere.

use std::net::{SocketAddr, TcpListener, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use aoo::client::{Client, Event as AooEvent, State, TcpDriver};
use aoo::message;
use sonobus_net::wire::Role;
use sonobus_net::{Config, Event, Net, RdvAddr, RelayChoice, Sink};
use sonobus_server::{Daemon, Parsed, Settings, parse};

const WAIT: Duration = Duration::from_secs(15);

fn tmp(name: &str) -> PathBuf {
    let p = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = std::fs::remove_file(&p);
    p
}

fn local(key: &str) -> Settings {
    Settings {
        legacy_bind: "127.0.0.1:0".parse().unwrap(),
        modern_bind: "127.0.0.1:0".parse().unwrap(),
        key_file: tmp(key),
        ..Settings::default()
    }
}

#[test]
fn legacy_client_logs_in_and_joins_a_group() {
    let d = Daemon::start(&Settings {
        legacy_only: true,
        ..local("legacy.key")
    })
    .unwrap();
    assert!(d.rendezvous_addr().is_none());
    let srv = d.legacy_addr().unwrap();
    let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
    udp.set_nonblocking(true).unwrap();
    let mut c = Client::new(udp.local_addr().unwrap().port(), 7);
    c.connect(srv, "alice", "");
    let mut drv = TcpDriver::default();
    let mut events = vec![];
    let mut joined = false;
    let end = Instant::now() + WAIT;
    while !events
        .iter()
        .any(|e| matches!(e, AooEvent::GroupJoined { .. }))
    {
        assert!(Instant::now() < end, "timed out: {events:?}");
        let now = Instant::now();
        let mut buf = [0u8; 4096];
        while let Ok((n, from)) = udp.recv_from(&mut buf) {
            if let Ok(m) = message::parse(&buf[..n]) {
                c.handle_udp(from, &m, now);
            }
        }
        drv.pump(&mut c, &udp, now, |e| events.push(e));
        if !joined && c.state() == State::Connected {
            c.join_group("band", "pw", false);
            joined = true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(d.stats_line().split_once(' ').unwrap().0, "stats");
    assert!(
        d.stats_line()
            .ends_with(" legacy_conns=1 legacy_users=1 legacy_groups=1 legacy_refused=none"),
        "{}",
        d.stats_line()
    );
}

/// Every 127.0.0.n is local on Linux and Windows; macOS only configures 127.0.0.1.
const MANY_LOOPBACKS: bool = cfg!(any(target_os = "linux", windows));

/// 127.0.0.n where [`MANY_LOOPBACKS`], else 127.0.0.1.
fn lo(n: u8) -> [u8; 4] {
    [127, 0, 0, if MANY_LOOPBACKS { n } else { 1 }]
}

fn net(ip: [u8; 4], name: &str, host: bool) -> Net {
    Net::start(
        Config {
            bind_ip: ip.into(),
            port: 0,
            session: if host {
                "Friday jam".into()
            } else {
                String::new()
            },
            password: host.then(|| "open sesame".into()),
            role: if host { Role::Host } else { Role::Peer },
            name: name.into(),
            relay: RelayChoice::None,
            ..Config::default()
        },
        |_, _| Box::new(|_| {}) as Sink,
    )
    .unwrap()
}

fn connected(n: &Net) {
    let end = Instant::now() + WAIT;
    loop {
        match n
            .events()
            .recv_timeout(end.saturating_duration_since(Instant::now()))
        {
            Ok(Event::Connected { .. }) => return,
            Ok(Event::ConnectFailed { reason, .. }) => panic!("connect failed: {reason}"),
            Ok(_) => {}
            Err(e) => panic!("timed out: {e}"),
        }
    }
}

#[test]
fn two_nets_meet_by_name_and_password() {
    let d = Daemon::start(&Settings {
        modern_only: true,
        ..local("meet.key")
    })
    .unwrap();
    assert!(d.legacy_addr().is_none());
    // through the printed string, as a user would paste it
    let text = d.rendezvous_addr().unwrap();
    assert!(text.starts_with("sonobus2://rdv/"), "{text}");
    let srv: RdvAddr = text.parse().unwrap();
    assert_eq!(srv.resolve(), Ok(d.rendezvous_endpoint().unwrap()));
    let h = net(lo(2), "host", true);
    h.register_by_name(srv.clone());
    let j = net(lo(3), "joiner", false);
    j.join_by_name(srv, "Friday jam", "open sesame").unwrap();
    connected(&j);
    connected(&h);
    let line = d.stats_line();
    // sources: distinct addresses
    let sources = if MANY_LOOPBACKS { 2 } else { 1 };
    assert!(
        line.contains(&format!(" modern_conns=2 modern_sources={sources} modern_tags=1 modern_regs=2"))
            && line.ends_with(" modern_refused=none")
            && !line.contains("legacy"),
        "{line}"
    );
}

#[test]
fn key_file_created_once_0600_and_reused() {
    let s = Settings {
        modern_only: true,
        ..local("persist.key")
    };
    let first = Daemon::start(&s).unwrap().rendezvous_endpoint().unwrap();
    let text = std::fs::read_to_string(&s.key_file).unwrap();
    assert_eq!(text.trim().len(), 64);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&s.key_file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let second = Daemon::start(&s).unwrap().rendezvous_endpoint().unwrap();
    assert_eq!(first.id, second.id, "same endpoint id across restarts");
    assert_eq!(
        std::fs::read_to_string(&s.key_file).unwrap(),
        text,
        "not rewritten"
    );
    // a corrupt key file is an error, never silently replaced
    std::fs::write(&s.key_file, "nope\n").unwrap();
    let e = Daemon::start(&s).err().unwrap();
    assert!(e.contains("not 64 hex digits"), "{e}");
}

#[test]
fn precedence_cli_over_env_over_file() {
    let cfg = tmp("prec.conf");
    std::fs::write(
        &cfg,
        "# comment\nlegacy-bind = 127.0.0.1:1\nmodern-bind = \"127.0.0.1:2\"\n\
         key-file = /from/file\nlegacy-max-groups = 7\nmodern-per-tag = 9\n",
    )
    .unwrap();
    let env = |extra: &[(&str, &str)]| {
        let mut v = vec![(
            "SONOBUS_SERVER_CONFIG".to_string(),
            cfg.display().to_string(),
        )];
        v.extend(extra.iter().map(|(k, v)| (k.to_string(), v.to_string())));
        v.push(("UNRELATED".into(), "x".into()));
        v
    };
    let run = |args: &[&str], env: &[(String, String)]| {
        let args: Vec<String> = args.iter().map(|s| s.to_string()).collect();
        match parse(&args, env) {
            Ok(Parsed::Run(s)) => Ok(*s),
            Ok(p) => panic!("{p:?}"),
            Err(e) => Err(e),
        }
    };
    let e = env(&[
        ("SONOBUS_SERVER_MODERN_BIND", "127.0.0.1:3"),
        ("SONOBUS_SERVER_KEY_FILE", "/from/env"),
        ("SONOBUS_SERVER_LEGACY_ONLY", "yes"),
    ]);
    let s = run(&["--key-file", "/from/cli", "--modern-per-tag=11"], &e).unwrap();
    let a = |p: u16| SocketAddr::from(([127, 0, 0, 1], p));
    assert_eq!(s.legacy_bind, a(1), "file");
    assert_eq!(s.modern_bind, a(3), "env over file");
    assert_eq!(s.key_file, PathBuf::from("/from/cli"), "cli over env");
    assert_eq!(s.legacy_limits.max_groups, 7);
    assert_eq!(s.modern_limits.per_tag, 11, "cli over file");
    assert!(s.legacy_only);
    // defaults where nothing is set
    let s = run(&[], &[]).unwrap();
    assert_eq!(s.legacy_bind, "0.0.0.0:10998".parse().unwrap());
    assert_eq!(s.modern_bind, "0.0.0.0:10999".parse().unwrap());
    assert_eq!(s.relay, RelayChoice::None);
    assert_eq!(s.legacy_limits, aoo::server::Limits::standalone());
    // --config on the command line beats the env one; flags without a value
    let s = run(&["--config", "/nonexistent"], &e);
    assert!(s.unwrap_err().contains("/nonexistent"));
    assert!(run(&["--modern-only"], &e).unwrap_err().contains("exclude"));
    // errors name their source
    assert!(
        run(&["--bogus", "1"], &[])
            .unwrap_err()
            .starts_with("--bogus")
    );
    let e2 = vec![("SONOBUS_SERVER_LEGACY_BIND".into(), "nope".into())];
    assert!(
        run(&[], &e2)
            .unwrap_err()
            .starts_with("SONOBUS_SERVER_LEGACY_BIND")
    );
    assert!(run(&["--relay", "not a url"], &[]).is_err());
    // `self` names the --relay-http relay
    let e = run(&["--relay", "self"], &[]).unwrap_err();
    if cfg!(feature = "relay") {
        assert!(e.contains("needs --relay-http"), "{e}");
    } else {
        assert!(e.contains("relay feature"), "{e}");
    }
    assert!(run(&["--public-addr", "example.org"], &[]).is_err());
    assert!(
        run(&["--legacy-bind"], &[])
            .unwrap_err()
            .contains("needs a value")
    );
    let s = run(
        &[
            "--relay",
            "http://127.0.0.1:1/",
            "--public-addr",
            "a.example:1,b:2",
        ],
        &[],
    )
    .unwrap();
    assert_eq!(
        s.relay,
        RelayChoice::Custom(vec!["http://127.0.0.1:1/".into()])
    );
    assert_eq!(s.public_addrs, ["a.example:1", "b:2"]);
    assert!(matches!(parse(&["-h".into()], &[]), Ok(Parsed::Help)));
    assert!(matches!(
        parse(&["--version".into()], &[]),
        Ok(Parsed::Version)
    ));
}

#[test]
fn port_in_use_is_an_error() {
    let held = UdpSocket::bind("127.0.0.1:0").unwrap();
    let e = Daemon::start(&Settings {
        modern_only: true,
        modern_bind: held.local_addr().unwrap(),
        ..local("taken.key")
    })
    .err()
    .unwrap();
    let want = format!("rendezvous: cannot bind UDP {}: ", held.local_addr().unwrap());
    assert!(e.starts_with(&want), "{e}");
    let tcp = TcpListener::bind("127.0.0.1:0").unwrap();
    let e = Daemon::start(&Settings {
        legacy_only: true,
        legacy_bind: tcp.local_addr().unwrap(),
        ..local("taken2.key")
    })
    .err()
    .unwrap();
    assert!(e.starts_with("legacy server: cannot bind TCP+UDP"), "{e}");
}

#[test]
fn public_addr_overrides_printed_address() {
    let d = Daemon::start(&Settings {
        modern_only: true,
        public_addrs: vec!["rdv.example.org:10999".into()],
        ..local("public.key")
    })
    .unwrap();
    let id = d.rendezvous_endpoint().unwrap().id;
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(
        d.rendezvous_addr().unwrap(),
        format!("sonobus2://rdv/{hex}@rdv.example.org:10999")
    );
    assert!(d.ready_line().contains(&format!("rendezvous_id={hex}")));
}

#[cfg(feature = "relay")]
#[test]
fn relay_http_serves_a_home_relay() {
    let d = Daemon::start(&Settings {
        legacy_only: true,
        relay_http: Some("127.0.0.1:0".parse().unwrap()),
        ..local("relay.key")
    })
    .unwrap();
    let url = format!("http://{}", d.relay_addr().unwrap());
    let n = Net::start(
        Config {
            bind_ip: lo(4).into(),
            port: 0,
            relay: RelayChoice::Custom(vec![url]),
            ..Config::default()
        },
        |_, _| Box::new(|_| {}) as Sink,
    )
    .unwrap();
    let end = Instant::now() + WAIT;
    while n.local_addr().relay.is_none() {
        assert!(Instant::now() < end, "never got a home relay");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[cfg(feature = "relay")]
#[test]
fn relay_self_is_the_rendezvous_relay() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let e = parse(&args(&["--relay", "self", "--relay-http", "0.0.0.0:1"]), &[]).unwrap_err();
    assert!(e.contains("no address clients can use"), "{e}");
    let Ok(Parsed::Run(s)) = parse(&args(&["--relay=self", "--relay-http=127.0.0.1:0"]), &[])
    else {
        panic!()
    };
    assert!(s.relay_self);
    let d = Daemon::start(&Settings {
        modern_only: true,
        relay_http: s.relay_http,
        relay_self: true,
        ..local("relay-self.key")
    })
    .unwrap();
    let url = format!("http://{}", d.relay_addr().unwrap());
    let rdv = d.rendezvous_endpoint().unwrap();
    assert!(rdv.relay.as_deref().unwrap_or("").starts_with(&url), "{rdv:?}");
    let text = d.rendezvous_addr().unwrap();
    assert!(text.contains(&format!("?relay={url}")), "{text}");
}
