//! `sonobus-server`: both rendezvous services in one process.
//!
//! * Legacy: the AOO net protocol of SonoBus 1.7.x ([`aoo::server`]) on TCP+UDP (default
//!   `0.0.0.0:10998`, the port of SonoBus' public server).
//! * Modern: [`sonobus_net::rdv`] (`sonobus/rdv/1`) on its own iroh endpoint (default UDP
//!   `0.0.0.0:10999`) with a persistent secret key, so its endpoint id (part of the address
//!   users paste) survives restarts.
//!
//! Settings come from defaults < config file < `SONOBUS_SERVER_*` env vars < command line.

use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{ErrorKind, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use aoo::server::{Limits as LegacyLimits, Rate, ServerHandle};
use sonobus_net::rdv::Limits as RdvLimits;
use sonobus_net::{PeerAddr, RdvAddr, RelayChoice, RendezvousServer};

pub const HELP: &str = "\
sonobus-server: SonoBus rendezvous server (legacy AOO for SonoBus 1.7.x + sonobus/rdv/1)

USAGE: sonobus-server [OPTIONS]

Every option is also a config file line `name = value` and an env var
SONOBUS_SERVER_<NAME> (upper case, '-' -> '_'), e.g. SONOBUS_SERVER_LEGACY_BIND.
Precedence: command line > env > config file > defaults.

  -h, --help                 this text
  -V, --version              version
  --config <file>            config file (env SONOBUS_SERVER_CONFIG)
  --legacy-bind <ip:port>    legacy AOO server, TCP+UDP       [0.0.0.0:10998]
  --modern-bind <ip:port>    rendezvous iroh endpoint, UDP    [0.0.0.0:10999]
  --key-file <path>          rendezvous endpoint secret key; created (0600) on
                             first start, keep it: the endpoint id derives from it
                                                              [sonobus-rdv.key]
  --relay <none|n0|url,...>  relays for the rendezvous endpoint (for a server
                             without a public address)        [none]
  --public-addr <host:port,...>  addresses printed in the rendezvous address
                             (default: the endpoint's interface addresses)
  --legacy-only              run only the legacy server
  --modern-only              run only the rendezvous server
  --stats-interval <dur>     stats line period, 0 = off; SIGUSR1 prints one too [5m]
";

pub const HELP_LIMITS: &str = "
Limits (durations: 250ms, 10s, 5m or plain seconds; rates: burst/per_sec):
  legacy (defaults: aoo::server::Limits::standalone()):
  --legacy-max-connections <n> [256]   --legacy-max-connections-per-ip <n> [16]
  --legacy-login-timeout <dur> [10s]   --legacy-idle-timeout <dur> [30s]
  --legacy-max-groups <n> [512]        --legacy-max-groups-per-ip <n> [32]
  --legacy-max-groups-per-conn <n> [4] --legacy-send-queue-bytes <n> [196608]
  --legacy-join-rate <b/s> [4/1]       --legacy-join-rate-per-ip <b/s> [8/2]
  --legacy-list-rate-per-ip <b/s> [131072/32768]
  --legacy-public-update-interval <dur> [1s]
  modern (defaults: sonobus_net::rdv::Limits::default()):
  --modern-max-connections <n> [4096]  --modern-per-ip <n> [16]
  --modern-per-tag <n> [256]           --modern-listed <n> [32]
  --modern-register-burst <n> [10]     --modern-register-refill <dur> [6s]
  --modern-register-timeout <dur> [5s] --modern-idle <dur> [60s]
  --modern-fanout-interval <dur> [250ms]
";

#[cfg(feature = "relay")]
pub const HELP_RELAY: &str = "
  --relay-http <ip:port>     also run an iroh relay, PLAIN HTTP: for a LAN or behind
                             a TLS-terminating reverse proxy only    [off]
                             `--relay self` makes it the rendezvous endpoint's relay
                             (http://<ip:port>, so not 0.0.0.0)
";
#[cfg(not(feature = "relay"))]
pub const HELP_RELAY: &str = "";

/// Options that take no value on the command line.
const FLAGS: &[&str] = &["legacy-only", "modern-only"];

#[derive(Debug, Clone)]
pub struct Settings {
    pub legacy_bind: SocketAddr,
    pub modern_bind: SocketAddr,
    pub key_file: PathBuf,
    pub relay: RelayChoice,
    pub public_addrs: Vec<String>,
    pub legacy_only: bool,
    pub modern_only: bool,
    pub stats_interval: Duration,
    pub legacy_limits: LegacyLimits,
    pub modern_limits: RdvLimits,
    #[cfg(feature = "relay")]
    pub relay_http: Option<SocketAddr>,
    /// `--relay self`: the rendezvous endpoint uses the `relay_http` relay.
    #[cfg(feature = "relay")]
    pub relay_self: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            legacy_bind: ([0, 0, 0, 0], 10998).into(),
            // sonobus-net's client default port too; a client on the same host falls back to
            // an ephemeral port, the server refuses to (its port is in the address users paste).
            modern_bind: ([0, 0, 0, 0], sonobus_net::rt::DEFAULT_PORT).into(),
            key_file: "sonobus-rdv.key".into(),
            relay: RelayChoice::None,
            public_addrs: vec![],
            legacy_only: false,
            modern_only: false,
            stats_interval: Duration::from_secs(300),
            legacy_limits: LegacyLimits::standalone(),
            modern_limits: RdvLimits::default(),
            #[cfg(feature = "relay")]
            relay_http: None,
            #[cfg(feature = "relay")]
            relay_self: false,
        }
    }
}

#[derive(Debug)]
pub enum Parsed {
    Run(Box<Settings>),
    Help,
    Version,
}

/// Parse the command line `args` (without the program name) on top of `env` (all env vars;
/// `SONOBUS_SERVER_*` are used) and the config file named by `--config` or
/// `SONOBUS_SERVER_CONFIG`.
pub fn parse(args: &[String], env: &[(String, String)]) -> Result<Parsed, String> {
    let mut config = env
        .iter()
        .find(|(k, _)| k == "SONOBUS_SERVER_CONFIG")
        .map(|(_, v)| PathBuf::from(v));
    let mut cli = vec![];
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => return Ok(Parsed::Help),
            "-V" | "--version" => return Ok(Parsed::Version),
            _ => {}
        }
        let Some(k) = a.strip_prefix("--") else {
            return Err(format!("unexpected argument {a:?}"));
        };
        let (k, v) = match k.split_once('=') {
            Some((k, v)) => (k, v.to_string()),
            None if FLAGS.contains(&k) => (k, "true".into()),
            None => (k, it.next().ok_or(format!("--{k} needs a value"))?.clone()),
        };
        if k == "config" {
            config = Some(v.into());
        } else {
            cli.push((k.to_string(), v));
        }
    }
    let mut s = Settings::default();
    if let Some(p) = &config {
        let text = fs::read_to_string(p).map_err(|e| format!("config {}: {e}", p.display()))?;
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let at = || format!("{}:{}", p.display(), n + 1);
            let (k, v) = line
                .split_once('=')
                .ok_or_else(|| format!("{}: expected `name = value`", at()))?;
            apply(&mut s, k.trim(), v.trim().trim_matches('"'))
                .map_err(|e| format!("{}: {}: {e}", at(), k.trim()))?;
        }
    }
    for (k, v) in env {
        if let Some(name) = k.strip_prefix("SONOBUS_SERVER_")
            && name != "CONFIG"
        {
            apply(&mut s, &name.to_lowercase().replace('_', "-"), v)
                .map_err(|e| format!("{k}: {e}"))?;
        }
    }
    for (k, v) in &cli {
        apply(&mut s, k, v).map_err(|e| format!("--{k}: {e}"))?;
    }
    if s.legacy_only && s.modern_only {
        return Err("--legacy-only and --modern-only exclude each other".into());
    }
    #[cfg(feature = "relay")]
    if s.relay_self {
        match s.relay_http {
            None => return Err("--relay self needs --relay-http".into()),
            Some(a) if a.ip().is_unspecified() => {
                return Err(format!(
                    "--relay self: --relay-http {a} is no address clients can use; bind a \
                     specific one or pass the relay's public URL to --relay"
                ));
            }
            Some(_) => {}
        }
    }
    Ok(Parsed::Run(Box::new(s)))
}

fn num<T: FromStr>(v: &str) -> Result<T, String> {
    v.parse().map_err(|_| format!("not a number: {v:?}"))
}

fn dur(v: &str) -> Result<Duration, String> {
    let (n, ms) = if let Some(n) = v.strip_suffix("ms") {
        (n, 1)
    } else if let Some(n) = v.strip_suffix('s') {
        (n, 1000)
    } else if let Some(n) = v.strip_suffix('m') {
        (n, 60_000)
    } else {
        (v, 1000)
    };
    let n: u64 = n
        .trim()
        .parse()
        .map_err(|_| format!("not a duration: {v:?}"))?;
    Ok(Duration::from_millis(n.saturating_mul(ms)))
}

fn rate(v: &str) -> Result<Rate, String> {
    let (b, s) = v
        .split_once('/')
        .ok_or(format!("not burst/per_sec: {v:?}"))?;
    Ok(Rate {
        burst: num(b.trim())?,
        per_sec: num(s.trim())?,
    })
}

fn boolean(v: &str) -> Result<bool, String> {
    match v.to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" | "on" => Ok(true),
        "false" | "0" | "no" | "off" => Ok(false),
        _ => Err(format!("not a boolean: {v:?}")),
    }
}

fn sockaddr(v: &str) -> Result<SocketAddr, String> {
    v.parse()
        .map_err(|_| format!("not ip:port: {v:?} (IPv6: [::]:port)"))
}

fn list(v: &str) -> Vec<String> {
    v.split(',')
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(Into::into)
        .collect()
}

fn apply(s: &mut Settings, k: &str, v: &str) -> Result<(), String> {
    let (l, m) = (&mut s.legacy_limits, &mut s.modern_limits);
    match k {
        "legacy-bind" => s.legacy_bind = sockaddr(v)?,
        "modern-bind" => s.modern_bind = sockaddr(v)?,
        "key-file" => s.key_file = v.into(),
        "relay" => {
            s.relay = match v {
                "" | "none" => RelayChoice::None,
                "n0" => RelayChoice::N0Default,
                // filled in at start, once the --relay-http relay is bound
                "self" if cfg!(feature = "relay") => RelayChoice::None,
                "self" => return Err("`self` (the --relay-http relay) needs the relay feature".into()),
                urls => RelayChoice::Custom(list(urls)),
            };
            #[cfg(feature = "relay")]
            {
                s.relay_self = v == "self";
            }
            sonobus_net::rt::relay_mode(&s.relay)?;
        }
        "public-addr" => {
            s.public_addrs = list(v);
            for a in &s.public_addrs {
                // host:port shape only; resolving is the client's business
                let (h, p) = a.rsplit_once(':').ok_or(format!("not host:port: {a:?}"))?;
                if h.is_empty() || p.parse::<u16>().is_err() {
                    return Err(format!("not host:port: {a:?}"));
                }
            }
        }
        "legacy-only" => s.legacy_only = boolean(v)?,
        "modern-only" => s.modern_only = boolean(v)?,
        "stats-interval" => s.stats_interval = dur(v)?,
        "legacy-max-connections" => l.max_connections = num(v)?,
        "legacy-max-connections-per-ip" => l.max_connections_per_ip = num(v)?,
        "legacy-login-timeout" => l.login_timeout = dur(v)?,
        "legacy-idle-timeout" => l.idle_timeout = dur(v)?,
        "legacy-max-groups" => l.max_groups = num(v)?,
        "legacy-max-groups-per-ip" => l.max_groups_per_ip = num(v)?,
        "legacy-max-groups-per-conn" => l.max_groups_per_conn = num(v)?,
        "legacy-send-queue-bytes" => l.send_queue_bytes = num(v)?,
        "legacy-join-rate" => l.join_rate = rate(v)?,
        "legacy-join-rate-per-ip" => l.join_rate_per_ip = rate(v)?,
        "legacy-list-rate-per-ip" => l.list_rate_per_ip = rate(v)?,
        "legacy-public-update-interval" => l.public_update_interval = dur(v)?,
        "modern-max-connections" => m.max_connections = num(v)?,
        "modern-per-ip" => m.per_ip = num(v)?,
        "modern-per-tag" => m.per_tag = num(v)?,
        "modern-listed" => m.listed = num(v)?,
        "modern-register-burst" => m.register_burst = num(v)?,
        "modern-register-refill" => m.register_refill = dur(v)?,
        "modern-register-timeout" => m.register_timeout = dur(v)?,
        "modern-idle" => m.idle = dur(v)?,
        "modern-fanout-interval" => m.fanout_interval = dur(v)?,
        #[cfg(feature = "relay")]
        "relay-http" => s.relay_http = Some(sockaddr(v)?),
        _ => return Err("unknown option".into()),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Key file

fn hex(b: &[u8]) -> String {
    b.iter().fold(String::new(), |mut s, x| {
        let _ = write!(s, "{x:02x}");
        s
    })
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

/// The rendezvous endpoint's secret key from `path` (64 hex digits), or a new one written
/// there with `create_new` and mode 0600. Returns `(key, created)`. Never logs the key.
pub fn load_or_create_key(path: &Path) -> Result<([u8; 32], bool), String> {
    let at = path.display();
    let mut o = OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut o, 0o600);
    match o.open(path) {
        Ok(mut f) => {
            let key = sonobus_net::generate_secret_key();
            let w = f.write_all(format!("{}\n", hex(&key)).as_bytes());
            if let Err(e) = w.and_then(|_| f.sync_all()) {
                let _ = fs::remove_file(path);
                return Err(format!("key file {at}: {e}"));
            }
            Ok((key, true))
        }
        Err(e) if e.kind() == ErrorKind::AlreadyExists => {
            let text = fs::read_to_string(path).map_err(|e| format!("key file {at}: {e}"))?;
            #[cfg(unix)]
            if let Ok(md) = fs::metadata(path)
                && std::os::unix::fs::PermissionsExt::mode(&md.permissions()) & 0o077 != 0
            {
                log(
                    "WARN",
                    &format!("key file {at} is readable by others (chmod 600 it)"),
                );
            }
            let key = unhex32(text.trim())
                .ok_or(format!("key file {at}: not 64 hex digits (corrupt?)"))?;
            Ok((key, false))
        }
        Err(e) => Err(format!("key file {at}: {e}")),
    }
}

// ---------------------------------------------------------------------------------------------
// Logging

fn utc(secs: u64, ms: u32) -> String {
    // days -> civil date (H. Hinnant's algorithm)
    let z = (secs / 86400) as i64 + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let t = secs % 86400;
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{ms:03}Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

/// One line to stderr: `<UTC time> <LEVEL> <message>`.
pub fn log(level: &str, msg: &str) {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    eprintln!("{} {level} {msg}", utc(d.as_secs(), d.subsec_millis()));
}

// ---------------------------------------------------------------------------------------------
// Daemon

/// Both services; dropping it stops them and joins their threads.
pub struct Daemon {
    // field order = drop order
    #[cfg(feature = "relay")]
    relay: Option<relay::Relay>,
    modern: Option<(RendezvousServer, String)>,
    legacy: Option<ServerHandle>,
    started: Instant,
}

impl Daemon {
    pub fn start(s: &Settings) -> Result<Daemon, String> {
        // first, so `--relay` may name it
        #[cfg(feature = "relay")]
        let relay = s.relay_http.map(relay::Relay::start).transpose()?;
        #[cfg(feature = "relay")]
        let s = &match &relay {
            Some(r) if s.relay_self => Settings {
                relay: RelayChoice::Custom(vec![format!("http://{}", r.addr)]),
                ..s.clone()
            },
            _ => s.clone(),
        };
        let legacy = match s.modern_only {
            true => None,
            false => Some(
                aoo::server::run_with(s.legacy_bind, s.legacy_limits).map_err(|e| {
                    format!("legacy server: cannot bind TCP+UDP {}: {e}", s.legacy_bind)
                })?,
            ),
        };
        let modern = match s.legacy_only {
            true => None,
            false => Some(start_modern(s)?),
        };
        Ok(Daemon {
            #[cfg(feature = "relay")]
            relay,
            modern,
            legacy,
            started: Instant::now(),
        })
    }

    pub fn legacy_addr(&self) -> Option<SocketAddr> {
        self.legacy.as_ref().map(|l| l.local_addr())
    }

    /// The rendezvous address string users paste ([`RdvAddr`]).
    pub fn rendezvous_addr(&self) -> Option<&str> {
        self.modern.as_ref().map(|(_, a)| a.as_str())
    }

    /// The rendezvous endpoint as it reports itself (id, interface addresses, home relay).
    pub fn rendezvous_endpoint(&self) -> Option<PeerAddr> {
        self.modern.as_ref().map(|(m, _)| m.addr())
    }

    #[cfg(feature = "relay")]
    pub fn relay_addr(&self) -> Option<SocketAddr> {
        self.relay.as_ref().map(|r| r.addr)
    }

    pub fn ready_line(&self) -> String {
        let mut s = "ready".to_string();
        if let Some(a) = self.legacy_addr() {
            let _ = write!(s, " legacy={a}");
        }
        if let (Some(e), Some(a)) = (self.rendezvous_endpoint(), self.rendezvous_addr()) {
            let port = e.addrs.first().map_or(0, |a| a.port());
            let _ = write!(
                s,
                " rendezvous_id={} rendezvous_udp={port} address={a}",
                hex(&e.id)
            );
        }
        #[cfg(feature = "relay")]
        if let Some(a) = self.relay_addr() {
            let _ = write!(s, " relay_http={a}");
        }
        s
    }

    /// `stats uptime=… legacy_conns=… legacy_users=… legacy_groups=… legacy_refused=… modern_conns=…
    /// modern_sources=… modern_tags=… modern_regs=… modern_refused=…`; `*_refused` lists the
    /// cumulative refusals and drops by reason (`reason:count,…`, or `none`).
    pub fn stats_line(&self) -> String {
        let mut s = format!("stats uptime={}s", self.started.elapsed().as_secs());
        if let Some(l) = &self.legacy {
            let st = l.stats();
            let r = st.refused;
            let _ = write!(
                s,
                " legacy_conns={} legacy_users={} legacy_groups={}",
                st.connections, st.users, st.groups
            );
            reasons(
                &mut s,
                "legacy_refused",
                &[
                    ("global_cap", r.global_cap),
                    ("per_ip_cap", r.per_ip_cap),
                    ("login_timeout", r.login_timeout),
                    ("idle_timeout", r.idle_timeout),
                    ("send_queue_full", r.send_queue_full),
                    ("join_rate", r.join_rate_limited),
                    ("groups_cap", r.groups_cap),
                    ("groups_per_ip_cap", r.groups_per_ip_cap),
                    ("groups_per_conn_cap", r.groups_per_conn_cap),
                    ("bad_address", r.bad_address),
                    ("name_too_long", r.name_too_long),
                    ("password_too_long", r.password_too_long),
                ],
            );
        }
        if let Some((m, _)) = &self.modern {
            let c = m.counts();
            let r = c.refused;
            let _ = write!(
                s,
                " modern_conns={} modern_sources={} modern_tags={} modern_regs={}",
                c.connections, c.sources, c.tags, c.registrations
            );
            reasons(
                &mut s,
                "modern_refused",
                &[
                    ("global_cap", r.busy),
                    ("per_ip_cap", r.per_ip),
                    ("handshake", r.handshake),
                    ("register_timeout", r.register_timeout),
                    ("register_rate", r.rate_limited),
                    ("tag_full", r.tag_full),
                    ("id_mismatch", r.id_mismatch),
                    ("protocol", r.protocol),
                    ("idle_timeout", r.idle),
                    ("flood", r.flood),
                ],
            );
        }
        s
    }
}

/// ` key=reason:n,reason:n` (non-zero ones only), or ` key=none`.
fn reasons(s: &mut String, key: &str, list: &[(&str, u64)]) {
    let _ = write!(s, " {key}=");
    let mut sep = "";
    for (name, n) in list.iter().filter(|(_, n)| *n > 0) {
        let _ = write!(s, "{sep}{name}:{n}");
        sep = ",";
    }
    if sep.is_empty() {
        s.push_str("none");
    }
}

fn start_modern(s: &Settings) -> Result<(RendezvousServer, String), String> {
    let (key, created) = load_or_create_key(&s.key_file)?;
    let at = s.key_file.display();
    log(
        "INFO",
        &format!(
            "{} key file {at}",
            if created { "created" } else { "loaded" }
        ),
    );
    // strict: the port is in the address users have, never an ephemeral fallback
    let srv = RendezvousServer::start_strict(
        s.modern_bind.ip(),
        s.modern_bind.port(),
        Some(key),
        s.relay.clone(),
        s.modern_limits.clone(),
    )
    .map_err(|e| format!("rendezvous: {e}"))?;
    let mut a = srv.addr();
    if s.relay != RelayChoice::None {
        let end = Instant::now() + Duration::from_secs(10);
        while a.relay.is_none() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(50));
            a = srv.addr();
        }
        if a.relay.is_none() {
            log(
                "WARN",
                "rendezvous: no relay connection yet; address has no relay",
            );
        }
    }
    let addrs = match s.public_addrs.is_empty() {
        true => a.addrs.iter().map(ToString::to_string).collect(),
        false => s.public_addrs.clone(),
    };
    if addrs.is_empty() && a.relay.is_none() {
        log(
            "WARN",
            "rendezvous: no address to advertise; set --public-addr",
        );
    }
    let text = RdvAddr {
        id: a.id,
        hosts: addrs,
        relay: a.relay,
    }
    .to_url();
    Ok((srv, text))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    /// SIGINT / SIGTERM.
    Stop,
    /// SIGUSR1: print stats now.
    Stats,
}

/// Run until [`Signal::Stop`] (or every sender is gone); returns the exit code.
pub fn serve(s: &Settings, signals: Receiver<Signal>) -> i32 {
    let d = match Daemon::start(s) {
        Ok(d) => d,
        Err(e) => {
            log("ERROR", &e);
            return 1;
        }
    };
    log("INFO", &d.ready_line());
    wait(s.stats_interval, &signals, |_| log("INFO", &d.stats_line()));
    log("INFO", "shutting down");
    drop(d);
    log("INFO", "stopped");
    0
}

/// Until [`Signal::Stop`] (or every sender is gone): `stats(true)` every `interval` (zero:
/// never) and `stats(false)` on each [`Signal::Stats`], which does not move the schedule.
fn wait(interval: Duration, signals: &Receiver<Signal>, mut stats: impl FnMut(bool)) {
    let next = || Instant::now().checked_add(interval).filter(|_| !interval.is_zero());
    let mut due = next();
    loop {
        let r = match due {
            None => signals.recv().map_err(|_| RecvTimeoutError::Disconnected),
            Some(t) => signals.recv_timeout(t.saturating_duration_since(Instant::now())),
        };
        match r {
            Ok(Signal::Stats) => stats(false),
            Err(RecvTimeoutError::Timeout) => {
                stats(true);
                due = next();
            }
            Ok(Signal::Stop) | Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(feature = "relay")]
mod relay {
    use std::net::SocketAddr;
    use std::sync::mpsc;
    use std::thread::JoinHandle;

    use iroh_relay::server::{RelayConfig, Server, ServerConfig};
    use tokio::sync::oneshot;

    /// An iroh relay over plain http on its own thread, until dropped.
    pub struct Relay {
        pub addr: SocketAddr,
        stop: Option<oneshot::Sender<()>>,
        thread: Option<JoinHandle<()>>,
    }

    impl Relay {
        pub fn start(bind: SocketAddr) -> Result<Relay, String> {
            let (tx, rx) = mpsc::channel();
            let (stop, stop_rx) = oneshot::channel::<()>();
            let thread = std::thread::Builder::new()
                .name("sb-relay".into())
                .spawn(move || {
                    let rt = match tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                    {
                        Ok(rt) => rt,
                        Err(e) => return drop(tx.send(Err(e.to_string()))),
                    };
                    rt.block_on(async move {
                        let mut c = ServerConfig::default();
                        c.relay = Some(RelayConfig::new(bind));
                        let server = match Server::spawn(c).await {
                            Ok(s) => s,
                            Err(e) => return drop(tx.send(Err(e.to_string()))),
                        };
                        let _ = tx.send(server.http_addr().ok_or("no http address".into()));
                        let _ = stop_rx.await;
                        let _ = server.shutdown().await;
                    });
                })
                .map_err(|e| e.to_string())?;
            match rx.recv() {
                Ok(Ok(addr)) => Ok(Relay {
                    addr,
                    stop: Some(stop),
                    thread: Some(thread),
                }),
                r => {
                    let _ = thread.join();
                    let e = r.map_or_else(|_| "relay thread died".into(), |r| r.unwrap_err());
                    Err(format!("relay: cannot start on {bind}: {e}"))
                }
            }
        }
    }

    impl Drop for Relay {
        fn drop(&mut self) {
            let _ = self.stop.take().map(|s| s.send(()));
            let _ = self.thread.take().map(|t| t.join());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_dates() {
        assert_eq!(utc(0, 0), "1970-01-01T00:00:00.000Z");
        assert_eq!(utc(951_782_400, 5), "2000-02-29T00:00:00.005Z");
        assert_eq!(utc(1_795_010_675, 999), "2026-11-18T14:04:35.999Z");
    }

    #[test]
    fn values() {
        assert_eq!(dur("250ms"), Ok(Duration::from_millis(250)));
        assert_eq!(dur("10s"), Ok(Duration::from_secs(10)));
        assert_eq!(dur("5m"), Ok(Duration::from_secs(300)));
        assert_eq!(dur("7"), Ok(Duration::from_secs(7)));
        assert!(dur("x").is_err() && dur("-1").is_err());
        assert_eq!(
            rate("8/2"),
            Ok(Rate {
                burst: 8,
                per_sec: 2
            })
        );
        assert!(rate("8").is_err());
        assert!(unhex32(&"+a".repeat(32)).is_none());
    }

    /// SIGUSR1 stats lines do not push the periodic one back: under a steady stream of them
    /// (faster than the interval) periodic lines still come.
    #[test]
    fn stats_signals_do_not_restart_the_interval() {
        use std::sync::mpsc::channel;
        let (tx, rx) = channel();
        let spam = std::thread::spawn(move || {
            let end = Instant::now() + Duration::from_millis(500);
            while Instant::now() < end {
                tx.send(Signal::Stats).unwrap();
                std::thread::sleep(Duration::from_millis(20));
            }
            tx.send(Signal::Stop).unwrap();
        });
        let (mut periodic, mut asked) = (0, 0);
        wait(Duration::from_millis(100), &rx, |p| match p {
            true => periodic += 1,
            false => asked += 1,
        });
        spam.join().unwrap();
        assert!(periodic >= 2 && asked >= 10, "{periodic} {asked}");
        // zero interval: only on request; a dropped sender ends it
        let (tx, rx) = channel();
        tx.send(Signal::Stats).unwrap();
        drop(tx);
        let mut seen = vec![];
        wait(Duration::ZERO, &rx, |p| seen.push(p));
        assert_eq!(seen, [false]);
    }

    #[test]
    fn reasons_list() {
        let mut s = String::new();
        reasons(&mut s, "x", &[("a", 0), ("b", 2), ("c", 0), ("d", 1)]);
        reasons(&mut s, "y", &[("a", 0)]);
        assert_eq!(s, " x=b:2,d:1 y=none");
    }
}
