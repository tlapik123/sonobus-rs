//! The application controller: owns the engine, the audio driver, the embedded server and the
//! settings; turns engine events into a plain-data [`ViewModel`]. Platform-neutral and
//! window-free: call [`AppController::tick`] regularly (every UI frame, or every ~50 ms
//! headless) and issue commands through its methods or [`AppController::engine`].
//!
//! Nothing here blocks on the network: connects (DNS) run on short-lived threads and report
//! back through a channel; device I/O runs on the audio manager thread.
//!
//! Sessions are either compatible (legacy, `connect`) or secure (protocol v2: `host_secure`,
//! `join_secure`); starting one ends the other. The device identity of secure sessions lives in
//! the controller (from our settings, [`crate::secure`]) and is given to every engine it runs.
//!
//! Two modes share everything but the engine's owner:
//! - standalone ([`AppController::new`]): builds the engine, drives it with an
//!   [`AudioManager`] (devices or null audio), loads and autosaves the settings files;
//! - hosted ([`AppController::hosted`], the plugin): the host builds and processes the engine
//!   and rebuilds it ([`AppController::swap_engine`]); no audio manager, no settings files:
//!   parameters and soundboards persist through the host's [`Host`].

use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use sonobus_core::state::{AppState, ChannelGroup, Params, ServerConnectionInfo};
use sonobus_engine::modern::{DiscoveredSession, HostOptions, LanScope, NetOptions, PeerAddr, RelayChoice, SessionEvent, SessionMode, SessionRole, SessionStatus};
use sonobus_engine::{
    ChatMessage, Connection, Engine, EngineConfig, Event, FileStatus, GroupJoin, LatEntry, LocalTime, MeterLevels,
    Notification, PeerId, RecordingStatus, Snapshot, Soundboards, SuggestGroup, VoiceStatus,
};

use crate::audio::{AudioManager, AudioStatus, DeviceSetup, ExternalSlot, ExternalSpec, SYSTEM_SOUND_CHANNELS};
use crate::links::{self, DEFAULT_SERVER_HOST, Link};
use crate::osc_control::{self, Osc, OscConfig, OscStatus};
use crate::secure::{self, HostForm, How, InvitePreview, JoinMethod, NetPlace, SecurePrefs, SecureRecent};
use crate::settings::{self, Store};
use crate::tr;

/// Autosave period (fork feature: settings survive a crash).
pub const AUTOSAVE: Duration = Duration::from_secs(30);
/// The standalone app's built-in server port (E.cpp:1307-1310).
pub const DEFAULT_HOST_PORT: u16 = 10999;

pub struct ControllerConfig {
    /// Settings directory; `None` = nothing is loaded or saved.
    pub settings_dir: Option<PathBuf>,
    /// false = never open audio devices (null audio), whatever the saved setup says.
    pub use_devices: bool,
}

/// What a host that owns the engine (the plugin) provides in place of devices and settings
/// files. Everything else the host reads from the engine when it saves its project.
pub trait Host: Send {
    /// The host's automatable parameter values now (automation moves them); the view model
    /// follows them every tick.
    fn params(&self) -> Params;
    /// An edit through the controller (UI, OSC) changed automatable parameters from `old` to
    /// `new`: tell the host (one gesture per changed parameter).
    fn params_edited(&mut self, old: &Params, new: &Params);
    /// The soundboards changed: persist them.
    fn soundboards_changed(&mut self, boards: &Soundboards) -> io::Result<()>;
    /// "Save as default plugin options": the host stores `state` as its defaults.
    fn save_default(&mut self, state: &AppState) -> io::Result<PathBuf>;
    /// "Reset default plugin options".
    fn reset_default(&mut self) -> io::Result<()>;
}

/// What the connect form holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub user_password: String,
    /// Empty: log in and browse the public groups.
    pub group: String,
    pub group_password: String,
    pub public: bool,
}

impl Default for ConnectRequest {
    fn default() -> Self {
        ConnectRequest {
            host: DEFAULT_SERVER_HOST.into(),
            port: sonobus_core::state::DEFAULT_SERVER_PORT as u16,
            user: String::new(),
            user_password: String::new(),
            group: String::new(),
            group_password: String::new(),
            public: false,
        }
    }
}

impl ConnectRequest {
    pub fn from_recent(r: &ServerConnectionInfo) -> Self {
        ConnectRequest {
            host: r.server_host.clone(),
            port: u16::try_from(r.server_port).unwrap_or(10998),
            user: r.user_name.clone(),
            user_password: r.user_password.clone(),
            group: r.group_name.clone(),
            group_password: r.group_password.clone(),
            public: r.group_is_public,
        }
    }

    /// Fills in what a link carries.
    pub fn apply_link(&mut self, l: &Link) {
        if let Some((h, p)) = &l.server {
            (self.host, self.port) = (h.clone(), *p);
        }
        if let Some(g) = &l.group {
            self.group = g.clone();
            self.group_password = l.password.clone();
            self.public = l.public;
        }
    }
}

/// A connection server hosted on this device.
pub struct HostedServer {
    _handle: aoo::server::ServerHandle,
    pub port: u16,
    /// Addresses others on the LAN enter as the server ("host:port").
    pub addresses: Vec<IpAddr>,
}

/// Plain data for the UI.
#[derive(Debug, Clone)]
pub struct ViewModel {
    pub snapshot: Snapshot,
    /// Settings as last read from / written to the engine.
    pub state: AppState,
    /// Status line ("" = nothing to say).
    pub status: String,
    pub status_is_error: bool,
    /// A connect (DNS + login) is in flight.
    pub connecting: bool,
    pub reconnecting: bool,
    /// The last connect request (server, user, group).
    pub request: Option<ConnectRequest>,
    pub chat: Vec<ChatMessage>,
    pub unread_chat: usize,
    pub audio: AudioStatus,
    pub audio_setup: DeviceSetup,
    /// Port and LAN addresses of the session hosted here.
    pub hosting: Option<(u16, Vec<IpAddr>)>,
    pub blocked: Vec<IpAddr>,
    /// Devices blocked in the running secure session (endpoint ids; until it ends).
    pub blocked_endpoints: Vec<[u8; 32]>,
    pub settings_dir: Option<PathBuf>,
    pub recent_setups: Vec<String>,
    /// The running recording.
    pub recording: Option<RecordingStatus>,
    /// Files of the last finished recording.
    pub last_recording: Vec<PathBuf>,
    /// The file loaded for playback.
    pub file: Option<FileStatus>,
    /// Recently loaded audio files, newest first.
    pub recent_files: Vec<String>,
    pub soundboards: Soundboards,
    /// Playing soundboard samples.
    pub voices: Vec<VoiceStatus>,
    /// Latency test results per peer: (measured round trip, estimate at that moment), ms.
    pub latency_tests: Vec<(PeerId, f32, f32)>,
    /// The latency match dialog is open: entries so far, complete, default target.
    pub latency_match: Option<(Vec<LatEntry>, bool, f32)>,
    /// A peer asks to match this latency: (user, ms).
    pub latency_request: Option<(String, f32)>,
    /// A peer invites us to another group.
    pub group_suggestion: Option<SuggestGroup>,
    /// Input group solo flags (the engine's; persisted with "Remember Local Track Solo").
    pub input_solo: Vec<bool>,
    /// OSC remote control.
    pub osc: OscStatus,
    /// A link from the command line or the OS names a server we never used: connect only once
    /// the user confirms ([`AppController::answer_pending_link`]).
    pub pending_link: Option<ConnectRequest>,
    /// The engine could not be rebuilt as asked; a fallback engine runs (any UDP port, or the
    /// default audio setup). [`AppController::retry_engine`] tries again.
    pub engine_error: Option<String>,
    /// Another instance holds the settings directory: nothing is saved here.
    pub settings_read_only: bool,
    /// The secure session we are in (names made safe to show).
    pub secure: Option<SessionStatus>,
    /// A secure session is being started (binding, resolving a rendezvous address).
    pub secure_starting: bool,
    pub secure_prefs: SecurePrefs,
    /// Our last join attempt was refused for a while: try again after this.
    pub secure_retry_at: Option<Instant>,
    /// Host in PIN mode: when wrong guesses invalidated the PIN.
    pub pin_invalidated: Option<Instant>,
    /// An invite link from the command line or the OS: join only once the user confirms
    /// ([`AppController::answer_pending_invite`]).
    pub pending_invite: Option<InvitePreview>,
    /// Secure sessions found on this network (while browsing).
    pub lan_sessions: Vec<DiscoveredSession>,
    pub lan_browsing: bool,
}

/// A secure session as started (network options, a rendezvous address still to resolve), to
/// start it again on a new engine (audio setup change).
type SecureStart = (NetOptions, Option<sonobus_net::RdvAddr>, Started);

#[derive(Clone)]
enum Started {
    Host(HostOptions),
    Invite(String),
    Lan(DiscoveredSession, String),
    Name(PeerAddr, String, String),
}

fn start_secure(e: &Engine, net: &NetOptions, s: &Started) -> io::Result<()> {
    match s {
        Started::Host(o) => e.host_session(net, o.clone()).map(drop),
        Started::Invite(link) => e.join_invite(net, link),
        Started::Lan(d, secret) => e.join_lan(net, d, secret),
        Started::Name(server, name, pw) => e.join_by_name(net, server.clone(), name, pw),
    }
}

enum Job {
    Connected(io::Result<()>),
    /// A secure session started (or not).
    /// A secure session start (its generation) failed, or not.
    Secure(u64, io::Result<()>),
    Direct(io::Result<SocketAddr>),
    /// A recording start (file creation) failed in this folder.
    RecordFailed(PathBuf, io::Error),
}

pub struct AppController {
    engine: Option<Arc<Engine>>,
    /// Standalone only.
    audio: Option<AudioManager>,
    /// Hosted only.
    host: Option<Box<dyn Host>>,
    /// The UI language (standalone: the process language; hosted: this instance's).
    lang: Option<&'static str>,
    use_devices: bool,
    setup: DeviceSetup,
    server: Option<HostedServer>,
    store: Option<Store>,
    jobs: (Sender<Job>, Receiver<Job>),
    last_save: Instant,
    vm: ViewModel,
    /// Push-to-talk: the mutes to restore on release.
    ptt: Option<(bool, bool)>,
    /// Input channels the engine was built with.
    engine_inputs: usize,
    latency_match_open: bool,
    osc: Osc,
    /// The settings directory's instance lock (standalone, when this instance holds it).
    _lock: Option<settings::InstanceLock>,
    /// The audio manager's "engine lost" on the last tick (a rebuild per loss, not per tick).
    engine_lost: bool,
    /// Keep the hosted server running after "Disconnect".
    pub keep_hosting: bool,
    /// An autosave writing on a worker thread.
    saving: Option<std::thread::JoinHandle<io::Result<()>>>,
    /// The device identity of secure sessions (each new engine gets it).
    identity: Option<[u8; 32]>,
    /// Where secure sessions bind and how LAN discovery runs (tests: loopback, port 0).
    pub net_place: NetPlace,
    /// The secure session as started (or being started), and its generation.
    secure_start: Option<SecureStart>,
    secure_gen: u64,
    /// Remembered once the session is up; `true`: keep its invite link in it.
    pending_recent: Option<(SecureRecent, bool)>,
    /// Hosting a remembered session: its recents entry follows the invite link.
    remember_invite: Option<String>,
}

fn empty_snapshot() -> Snapshot {
    Snapshot {
        connection: Connection::Disconnected,
        user: String::new(),
        group: None,
        public_groups: Vec::new(),
        peers: Vec::new(),
        input: MeterLevels::default(),
        send: MeterLevels::default(),
        output: MeterLevels::default(),
        udp_port: 0,
    }
}

/// The default displayed name: the last one used, else the OS user name.
fn default_user(state: &AppState) -> String {
    if !state.extra.last_username.is_empty() {
        return state.extra.last_username.clone();
    }
    std::env::var("USER").or_else(|_| std::env::var("USERNAME")).unwrap_or_else(|_| "sonobus-user".into())
}

/// IPv4 LAN addresses of this device for "host a session": the primary one (the route to a
/// public address; UDP connect sends nothing) first, then every other interface's.
pub fn lan_addresses() -> Vec<IpAddr> {
    let probe = || -> io::Result<IpAddr> {
        let s = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        s.connect((Ipv4Addr::new(192, 0, 2, 1), 9))?; // TEST-NET-1, never contacted
        Ok(s.local_addr()?.ip())
    };
    let mut v: Vec<IpAddr> = probe().ok().into_iter().collect();
    for i in if_addrs::get_if_addrs().unwrap_or_default() {
        if !v.contains(&i.ip()) {
            v.push(i.ip());
        }
    }
    v.retain(|ip| ip.is_ipv4() && !ip.is_loopback() && !ip.is_unspecified());
    v
}

/// Local wall-clock time for recording names, as upstream.
pub fn now_time() -> LocalTime {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    local_time(secs)
}

/// The local civil time of a Unix time (`TZ` is re-read each call); UTC if the OS cannot say.
#[cfg(unix)]
pub fn local_time(secs: u64) -> LocalTime {
    unsafe extern "C" {
        // POSIX; not in the libc crate
        fn tzset();
    }
    let Ok(t) = libc::time_t::try_from(secs) else { return utc_time(secs) };
    // SAFETY: tzset only reads the environment; localtime_r writes only `tm`, which is a
    // plain C struct for which all-zero is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        tzset();
        !libc::localtime_r(&t, &mut tm).is_null()
    };
    if !ok {
        return utc_time(secs);
    }
    LocalTime {
        year: (tm.tm_year + 1900).clamp(0, 9999) as u16,
        month: (tm.tm_mon + 1) as u8,
        day: tm.tm_mday as u8,
        hour: tm.tm_hour as u8,
        minute: tm.tm_min as u8,
        second: tm.tm_sec.min(59) as u8, // a leap second
    }
}

/// The local civil time of a Unix time (the current time zone rules); UTC if Windows cannot say.
#[cfg(windows)]
pub fn local_time(secs: u64) -> LocalTime {
    use windows_sys::Win32::Foundation::SYSTEMTIME;
    use windows_sys::Win32::System::Time::SystemTimeToTzSpecificLocalTime;
    let u = utc_time(secs);
    let utc = SYSTEMTIME {
        wYear: u.year,
        wMonth: u.month.into(),
        wDayOfWeek: 0,
        wDay: u.day.into(),
        wHour: u.hour.into(),
        wMinute: u.minute.into(),
        wSecond: u.second.into(),
        wMilliseconds: 0,
    };
    // SAFETY: both pointers are valid SYSTEMTIMEs; null = the current time zone.
    let mut l: SYSTEMTIME = unsafe { std::mem::zeroed() };
    if unsafe { SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut l) } == 0 {
        return u;
    }
    LocalTime { year: l.wYear, month: l.wMonth as u8, day: l.wDay as u8, hour: l.wHour as u8, minute: l.wMinute as u8, second: l.wSecond as u8 }
}

#[cfg(not(any(unix, windows)))]
pub fn local_time(secs: u64) -> LocalTime {
    utc_time(secs)
}

/// Civil date of a Unix time (H. Hinnant's `civil_from_days`).
pub fn utc_time(secs: u64) -> LocalTime {
    let (days, rem) = ((secs / 86400) as i64, secs % 86400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    LocalTime {
        year: year as u16,
        month: month as u8,
        day: day as u8,
        hour: (rem / 3600) as u8,
        minute: (rem / 60 % 60) as u8,
        second: (rem % 60) as u8,
    }
}

/// Engine channel starts of system sound and the platform input in a setup: they follow the
/// device input's channels (what [`AudioManager`] lays out, device or null driver).
fn source_starts(s: &DeviceSetup) -> (Option<i32>, Option<i32>) {
    let dev = i32::from(s.device_input_channels());
    let sys = s.shares_sound().then_some(dev);
    let ext = s.external.map(|_| dev + sys.map_or(0, |_| i32::from(SYSTEM_SOUND_CHANNELS)));
    (sys, ext)
}

/// Keeps the input groups on the channels their audio arrives on when the setup changes from
/// `old` to `new`: groups of system sound / the platform input follow their source's start
/// (and go when it is switched off), device groups are cut to the device channels actually
/// opened (and go when none are left), new sources get a "System Sound" group (stereo, no
/// limiter, not monitored: the device already plays it), and a device that comes back gets
/// a group again. Without this, a mic opening fewer channels than before, or no mic at all,
/// left the capture inside the mic's group. Returns true when a source group was added.
pub fn fit_input_groups(groups: &mut Vec<ChannelGroup>, old: &DeviceSetup, new: &DeviceSetup) -> bool {
    let (od, nd) = (i32::from(old.device_input_channels()), i32::from(new.device_input_channels()));
    let ((old_sys, old_ext), (new_sys, new_ext)) = (source_starts(old), source_starts(new));
    let ext_ch = new.external.map_or(0, |e| i32::from(e.channels));
    let source_ranges = [new_sys.map(|s| (s, i32::from(SYSTEM_SOUND_CHANNELS))), new_ext.map(|s| (s, ext_ch))];
    let on_source = |g: &ChannelGroup| {
        source_ranges.iter().flatten().any(|&(s, n)| g.chan_start < s + n && s < g.chan_start + g.num_channels)
    };
    let mut keep = Vec::with_capacity(groups.len());
    for mut g in groups.drain(..) {
        let start = if Some(g.chan_start) == old_sys {
            new_sys
        } else if Some(g.chan_start) == old_ext {
            new_ext
        } else if g.chan_start < nd {
            // a device group: cut to the device channels actually opened
            g.num_channels = g.num_channels.min(nd - g.chan_start).max(1);
            g.pans.resize(g.num_channels as usize, 0.0);
            Some(g.chan_start)
        } else if g.chan_start < od || on_source(&g) {
            None // its device channels are gone, or a source's audio arrives there now
        } else {
            Some(g.chan_start) // a group on unused channels: leave it
        };
        if let Some(st) = start {
            g.chan_start = st;
            keep.push(g);
        }
    }
    *groups = keep;
    if nd > 0 && !groups.iter().any(|g| g.chan_start < nd) {
        let mut g = ChannelGroup::input_default(0, false);
        g.num_channels = nd;
        g.pans = vec![0.0; nd as usize];
        groups.insert(0, g);
    }
    let mut added = false;
    let sources = [(new_sys, SYSTEM_SOUND_CHANNELS), (new_ext, new.external.map_or(0, |e| e.channels))];
    for (start, n) in sources {
        let Some(start) = start else { continue };
        if n > 0 && !groups.iter().any(|g| g.chan_start == start) {
            let n = i32::from(n);
            let mut g = ChannelGroup::input_default(start as usize, false);
            g.name = "System Sound".into();
            g.num_channels = n;
            g.pans = vec![0.0; n as usize];
            g.limiter.enabled = false;
            g.monitor = 0.0;
            groups.push(g);
            added = true;
        }
    }
    if groups.is_empty() {
        groups.push(ChannelGroup::input_default(0, false)); // the engine needs one
    }
    added
}

/// Opens a file or folder with the OS (file manager, player); never waits for it.
pub fn open_in_os(path: &Path) -> io::Result<()> {
    let cmd = if cfg!(target_os = "macos") {
        "open"
    } else if cfg!(windows) {
        "explorer"
    } else {
        "xdg-open"
    };
    let mut child = std::process::Command::new(cmd)
        .arg(path)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    std::thread::spawn(move || child.wait()); // reap it
    Ok(())
}

/// The file of a finished recording to play back: the only one, else the full mix.
pub fn recording_main_file(files: &[PathBuf]) -> Option<&PathBuf> {
    if files.len() == 1 {
        return files.first();
    }
    files.iter().find(|f| f.file_stem().is_some_and(|s| s.to_string_lossy().ends_with("-MIX")))
}

const CHAT_LINES: usize = 500;
/// Display lengths of peer- and server-supplied text ([`crate::clean`]).
const NAME_LEN: usize = 100;
const CHAT_LEN: usize = 2000;

/// Peer and group names as the server and peers sent them, made safe to show.
fn clean_snapshot(s: &mut Snapshot) {
    s.user = crate::clean(&s.user, NAME_LEN);
    if let Some(g) = &mut s.group {
        *g = crate::clean(g, NAME_LEN);
    }
    for (g, _) in &mut s.public_groups {
        *g = crate::clean(g, NAME_LEN);
    }
    for p in &mut s.peers {
        p.user = crate::clean(&p.user, NAME_LEN);
    }
}


const SENTINEL: &str = "SENTINEL";

/// Crash sentinel (SonoStandaloneFilterApp.cpp:147-177): a `SENTINEL` left from the last launch
/// means it crashed before finishing start-up; the settings are moved aside to
/// `POSSIBLY_BAD_<time>`. Returns true then. Otherwise creates the sentinel.
fn crash_sentinel(dir: &Path) -> bool {
    let sentinel = dir.join(SENTINEL);
    if sentinel.exists() {
        let t = now_time();
        let name = format!("POSSIBLY_BAD_{:04}-{:02}-{:02}_{:02}.{:02}.{:02}", t.year, t.month, t.day, t.hour, t.minute, t.second);
        let _ = std::fs::rename(dir.join(settings::SETTINGS_FILE), dir.join(name));
        return true;
    }
    let _ = std::fs::create_dir_all(dir).and_then(|()| std::fs::write(&sentinel, b""));
    false
}

fn view_model(state: AppState, settings_dir: Option<PathBuf>) -> ViewModel {
    ViewModel {
        snapshot: empty_snapshot(),
        state,
        status: String::new(),
        status_is_error: false,
        connecting: false,
        reconnecting: false,
        request: None,
        chat: Vec::new(),
        unread_chat: 0,
        audio: AudioStatus::default(),
        audio_setup: DeviceSetup::default(),
        hosting: None,
        blocked: Vec::new(),
        blocked_endpoints: Vec::new(),
        settings_dir,
        recent_setups: Vec::new(),
        recording: None,
        last_recording: Vec::new(),
        file: None,
        recent_files: Vec::new(),
        soundboards: Soundboards::default(),
        voices: Vec::new(),
        latency_tests: Vec::new(),
        latency_match: None,
        latency_request: None,
        group_suggestion: None,
        input_solo: Vec::new(),
        osc: OscStatus::default(),
        pending_link: None,
        engine_error: None,
        settings_read_only: false,
        secure: None,
        secure_starting: false,
        secure_prefs: SecurePrefs::default(),
        secure_retry_at: None,
        pin_invalidated: None,
        pending_invite: None,
        lan_sessions: Vec::new(),
        lan_browsing: false,
    }
}

/// What a rebuild keeps from the old engine: blocked addresses and the group (or secure
/// session) to rejoin.
struct Carry {
    blocked: Vec<IpAddr>,
    rejoin: Option<ConnectRequest>,
    secure: Option<SecureStart>,
}

/// Starts the embedded connection server with the limits meant for an app-hosted session.
fn start_server(port: u16) -> io::Result<aoo::server::ServerHandle> {
    let bind = SocketAddr::from((Ipv4Addr::UNSPECIFIED, port));
    aoo::server::run_with(bind, aoo::server::Limits::embedded())
}

impl AppController {
    pub fn new(cfg: ControllerConfig) -> io::Result<Self> {
        // one instance writes a settings directory; a second one reads it and saves nothing
        // (its crash sentinel would move the first one's settings aside)
        let lock = cfg.settings_dir.as_deref().and_then(settings::InstanceLock::acquire);
        let read_only = cfg.settings_dir.is_some() && lock.is_none();
        let crashed = !read_only && cfg.settings_dir.as_deref().is_some_and(crash_sentinel);
        let (store, loaded) = match cfg.settings_dir.clone() {
            Some(dir) => {
                let (s, l) = Store::open(dir);
                (Some(s), Some(l))
            }
            None => (None, None),
        };
        let (state, mut setup, blocked, warnings) = match loaded {
            Some(l) => (l.state, l.setup, l.blocked, l.warnings),
            None => (AppState::new(false), DeviceSetup::default(), Vec::new(), Vec::new()),
        };
        if !cfg.use_devices {
            setup.enabled = false;
        }
        let blocked: Vec<IpAddr> = blocked.iter().filter_map(|b| b.parse().ok()).collect();
        let mut vm = view_model(state.clone(), cfg.settings_dir.clone());
        (vm.status, vm.status_is_error) = (warnings.join("; "), !warnings.is_empty());
        (vm.audio_setup, vm.blocked) = (setup.clone(), blocked.clone());
        let mut c = AppController {
            engine: None,
            audio: Some(AudioManager::new()),
            host: None,
            lang: crate::i18n::init(&state.extra.lang_override_code),
            use_devices: cfg.use_devices,
            setup,
            server: None,
            store,
            jobs: channel(),
            last_save: Instant::now(),
            vm,
            ptt: None,
            engine_inputs: 0,
            latency_match_open: false,
            osc: Osc::default(),
            _lock: lock,
            engine_lost: false,
            keep_hosting: false,
            saving: None,
            identity: None,
            net_place: NetPlace::default(),
            secure_start: None,
            secure_gen: 0,
            pending_recent: None,
            remember_invite: None,
        };
        let _lang = crate::i18n::scope(c.lang);
        c.vm.settings_read_only = read_only;
        if read_only {
            c.set_status(tr("Another SonoBus is using these settings: changes made here are not saved."), true);
        }
        if crashed {
            c.set_status(tr("Looks like you crashed on launch last time, restoring default settings!"), true);
        }
        if let Some(st) = &c.store {
            (c.vm.recent_setups, c.vm.recent_files) = (st.standalone.recent_setup_files.clone(), st.recent_files());
            c.vm.secure_prefs = SecurePrefs::load(&st.props);
            // a second instance gets a session identity of its own (not the first one's)
            c.identity = secure::load_identity(&st.props).filter(|_| !read_only);
        }
        if let Some(dir) = &cfg.settings_dir {
            match settings::load_soundboards(&dir.join(settings::SOUNDBOARDS_FILE)) {
                Ok(b) => c.vm.soundboards = b,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => c.set_status(format!("{}: {e}", settings::SOUNDBOARDS_FILE), true),
            }
        }
        c.start_engine(state)?;
        c.engine().set_blocked_addresses(blocked);
        if c.identity.is_none() && !read_only && let Some(st) = &mut c.store {
            // first start: keep the identity the engine made (invite links name it)
            let id = c.engine.as_deref().expect("engine running").identity_key();
            secure::store_identity(&mut st.props, &id);
            c.identity = Some(id);
            let _ = c.save();
        }
        c.started();
        // we got here: the launch did not crash (SonoStandaloneFilterApp.cpp:147-177)
        if let Some(dir) = cfg.settings_dir.as_ref().filter(|_| !read_only) {
            let _ = std::fs::remove_file(dir.join(SENTINEL));
        }
        Ok(c)
    }

    /// A controller over an engine the host owns and processes (the plugin). The language is
    /// this instance's (`langOverrideCode`), not the process's; nothing is read from or written
    /// to the settings directory.
    pub fn hosted(engine: Arc<Engine>, host: Box<dyn Host>, soundboards: Soundboards) -> Self {
        let state = engine.state();
        let mut vm = view_model(state.clone(), None);
        vm.soundboards = soundboards;
        engine.set_soundboards(vm.soundboards.clone());
        vm.audio_setup.enabled = false;
        let mut c = AppController {
            engine: Some(engine),
            audio: None,
            host: Some(host),
            lang: crate::i18n::pick(&state.extra.lang_override_code),
            use_devices: false,
            setup: vm.audio_setup.clone(),
            server: None,
            store: None,
            jobs: channel(),
            last_save: Instant::now(),
            vm,
            ptt: None,
            engine_inputs: 0,
            latency_match_open: false,
            osc: Osc::default(),
            _lock: None,
            engine_lost: false,
            keep_hosting: false,
            saving: None,
            identity: None,
            net_place: NetPlace::default(),
            secure_start: None,
            secure_gen: 0,
            pending_recent: None,
            remember_invite: None,
        };
        let _lang = crate::i18n::scope(c.lang);
        // no settings files: the identity lives as long as this controller
        c.identity = Some(c.engine().identity_key());
        c.started();
        c
    }

    /// The common end of both constructors: view model, OSC, reconnect.
    fn started(&mut self) {
        self.refresh_state();
        // the fork's editor sends the full state on start whenever OSC is enabled
        self.osc.configure(OscConfig::load(&self.vm.state.extra), true);
        self.vm.osc = self.osc.status();
        if self.vm.state.params.reconnect_last
            && let Some(r) = self.vm.state.recents.first()
        {
            let r = ConnectRequest::from_recent(r);
            self.connect(r);
        }
    }

    /// Hosted mode (the plugin): an outside host owns the engine.
    pub fn is_hosted(&self) -> bool {
        self.host.is_some()
    }

    /// The UI language (`None` = English).
    pub fn language(&self) -> Option<&'static str> {
        self.lang
    }

    /// Hosted: picks the language again from the settings (upstream's plugin applies a
    /// language change when the host reopens the plugin view). Standalone keeps the process
    /// language (a change needs a restart).
    pub fn reload_language(&mut self) {
        if self.is_hosted() {
            self.lang = crate::i18n::pick(&self.vm.state.extra.lang_override_code);
        }
    }

    fn start_engine(&mut self, mut state: AppState) -> io::Result<()> {
        let port = u16::try_from(state.extra.use_udp_port).unwrap_or(0);
        let audio = self.setup.audio_config();
        if state.input_groups.is_empty() {
            // the engine's first-time group would span the reserved channels too: one group
            // over the device input only (P.cpp:6991-6998)
            let mut g = ChannelGroup::input_default(0, state.is_plugin);
            g.num_channels = i32::from(self.setup.device_input_channels().max(1));
            g.pans = vec![0.0; g.num_channels as usize];
            g.mon_dest_channels = audio.output_channels.min(2) as i32;
            state.input_groups.push(g);
        }
        self.engine_inputs = audio.input_channels;
        let (engine, rt) = Engine::new(EngineConfig { audio, udp_port: port, state })?;
        engine.set_soundboards(self.vm.soundboards.clone());
        if let Some(id) = self.identity {
            engine.set_identity_key(id);
        }
        self.engine = Some(Arc::new(engine));
        if let Some(a) = &self.audio {
            a.start(rt, self.setup.clone());
        }
        Ok(())
    }

    /// The engine, for its per-peer and chat commands.
    pub fn engine(&self) -> &Engine {
        self.engine.as_deref().expect("engine running")
    }

    /// The engine, shared (threads, the host's own handle).
    pub fn shared_engine(&self) -> Arc<Engine> {
        self.engine.clone().expect("engine running")
    }

    pub fn vm(&self) -> &ViewModel {
        &self.vm
    }

    pub fn default_request(&self) -> ConnectRequest {
        let mut r = match self.vm.state.recents.first() {
            Some(r) => ConnectRequest::from_recent(r),
            None => ConnectRequest::default(),
        };
        r.user = default_user(&self.vm.state);
        r
    }

    /// Sets the status line (UI-side validation messages too).
    pub fn set_status(&mut self, s: impl Into<String>, error: bool) {
        // server errors and peer names end up here
        self.vm.status = crate::clean(&s.into(), 400);
        self.vm.status_is_error = error;
    }

    pub fn clear_status(&mut self) {
        self.set_status("", false);
    }

    /// Re-reads the settings from the engine (after the host replaced them, e.g. a project
    /// load).
    pub fn refresh_state(&mut self) {
        self.vm.state = self.engine().state();
        let solo = sonobus_engine::options::get_string(&self.vm.state.extra, sonobus_engine::options::INPUT_SOLO_STATES);
        self.vm.input_solo = sonobus_engine::options::flags_from_str(&solo);
        self.vm.input_solo.resize(self.vm.state.input_groups.len(), false);
    }

    // ---- ticking ------------------------------------------------------------------------------

    /// Polls engine events, background jobs, audio status and the autosave timer.
    pub fn tick(&mut self) {
        let _lang = crate::i18n::scope(self.lang);
        while let Ok(job) = self.jobs.1.try_recv() {
            match job {
                Job::Connected(Ok(())) => {}
                Job::Connected(Err(e)) => {
                    self.vm.connecting = false;
                    self.set_status(format!("{}{e}", tr("Connect failed: ")), true);
                }
                Job::Direct(Ok(addr)) => self.set_status(format!("{} {addr}", tr("Connecting...")), false),
                Job::Direct(Err(e)) => self.set_status(format!("{}{e}", tr("Connect failed: ")), true),
                Job::RecordFailed(dir, e) => {
                    self.set_status(format!("{} {}: {e}", tr("Error creating writer for "), dir.display()), true)
                }
                Job::Secure(g, res) if g == self.secure_gen => {
                    self.vm.secure_starting = false;
                    if let Err(e) = res {
                        (self.secure_start, self.pending_recent, self.remember_invite) = (None, None, None);
                        self.set_status(format!("{}: {e}", tr("Could not start the secure session")), true);
                    }
                }
                Job::Secure(..) => {} // an older start
            }
        }
        let mut state_changed = false;
        while let Some(e) = self.engine().poll_event() {
            state_changed |= self.on_event(e);
        }
        while let Some(n) = self.engine().poll_notification() {
            state_changed |= self.on_notification(n);
        }
        self.secure_tick();
        if state_changed {
            self.refresh_state();
        }
        let e = self.engine();
        (self.vm.recording, self.vm.file, self.vm.voices) = (e.recording_status(), e.file_status(), e.soundboard_status());
        if self.latency_match_open {
            self.vm.latency_match = Some(self.engine().latency_match_info());
        }
        self.vm.snapshot = self.engine().snapshot();
        clean_snapshot(&mut self.vm.snapshot);
        self.vm.blocked = self.engine().blocked_addresses();
        self.vm.blocked_endpoints = self.engine().blocked_endpoints();
        if self.vm.snapshot.connection == Connection::Connected {
            self.vm.connecting = false;
        }
        if let Some(a) = &self.audio {
            self.vm.audio = a.status();
            // rebuild once per loss: the flag stays up until the manager took the new engine
            let lost = self.vm.audio.engine_lost;
            if lost && !std::mem::replace(&mut self.engine_lost, lost) {
                let _ = self.rebuild_engine();
            } else {
                self.engine_lost = lost;
            }
        }
        if let Some(h) = &self.host {
            let p = h.params();
            if p != self.vm.state.params {
                self.vm.state.params = p; // host automation
            }
        }
        self.osc_tick();
        if let Some(h) = self.saving.take_if(|h| h.is_finished())
            && let Ok(Err(e)) = h.join()
        {
            self.set_status(format!("{}: {e}", tr("Error while saving")), true);
        }
        if self.store.is_some() && self.saving.is_none() && self.last_save.elapsed() >= AUTOSAVE {
            // the files are written on a worker: no disk wait on the UI thread
            self.last_save = Instant::now();
            if let Some(files) = self.save_files() {
                self.saving = Some(std::thread::spawn(move || files.into_iter().try_for_each(|(p, d)| settings::write_atomic(&p, &d))));
            }
        }
    }

    /// Returns true when the settings may have changed (recents on join).
    fn on_event(&mut self, e: Event) -> bool {
        match e {
            Event::Connected { user } => {
                let wanted = self.vm.request.as_ref().map(|r| r.user.as_str()).unwrap_or_default();
                if user != wanted && !wanted.is_empty() {
                    self.set_status(format!("{} {user}", tr("Already connected with this user name")), false);
                } else {
                    self.set_status(tr("Connected to server"), false);
                }
                self.vm.reconnecting = false;
                return true;
            }
            Event::ConnectFailed { reason } => {
                self.vm.connecting = false;
                self.set_status(format!("{}{reason}", tr("Connect failed: ")), true);
            }
            Event::UserNameInUse { retrying, user } => {
                let s = if retrying {
                    format!("{} - {user}", tr("Already connected with this user name"))
                } else {
                    format!("{} - {} {user}", tr("Already connected with this user name"), tr("trying"))
                };
                self.set_status(s, false);
            }
            Event::Disconnected { error } => {
                self.vm.connecting = false;
                match error {
                    Some(e) => self.set_status(format!("{}: {e}", tr("Disconnected from server")), true),
                    None => self.set_status(tr("Disconnected"), false),
                }
            }
            Event::Reconnecting => {
                self.vm.reconnecting = true;
                self.set_status(tr("Reconnecting..."), false);
            }
            Event::GroupJoined { group } => {
                self.vm.reconnecting = false;
                self.set_status(format!("{}{group}", tr("Joined Group: ")), false);
                return true;
            }
            Event::GroupJoinFailed { group, error } => {
                self.set_status(format!("{}{group}: {error}", tr("Failed to join group: ")), true);
            }
            Event::GroupLeft { group } => {
                self.set_status(format!("{}{group}", tr("Left Group: ")), false);
                self.end_latency_match();
            }
            Event::PeerJoinFailed { user, .. } => self.set_status(
                format!(
                    "{user}: {}",
                    tr("Could not connect with user, one or both of you may need to configure your internal firewall or network router to allow SonoBus to work between you. See the help documentation to enable port forwarding on your router.")
                ),
                true,
            ),
            Event::PeerJoinBlocked { addr, .. } => {
                self.set_status(format!("{}{}", tr("User BLOCKED from address: "), addr.ip()), false)
            }
            Event::BlockedByPeer { user, blocked: true, .. } => {
                self.set_status(format!("{user}: {}", tr("Other end BLOCKED us")), true)
            }
            Event::Chat(m) => {
                if !m.own {
                    self.vm.unread_chat += 1;
                }
                self.push_chat(m);
            }
            Event::Error(e) => self.set_status(e, true),
            Event::PeerLeft { user, .. } => {
                self.system_chat(format!("{user}{}", tr(" - left group")));
                return true;
            }
            Event::PeerJoined { user, .. } => self.system_chat(format!("{user}{}", tr(" - joined group"))),
            Event::PeerChanged { .. }
            | Event::BlockedByPeer { .. }
            | Event::PublicGroupsChanged => {}
        }
        false
    }

    /// Stage-2 notifications; true when the settings may have changed.
    fn on_notification(&mut self, n: Notification) -> bool {
        match n {
            Notification::RecordingStarted { path } => {
                self.set_status(format!("{}{}", tr("Recording audio to: "), path.display()), false)
            }
            Notification::RecordingStopped { files, error } => {
                match error {
                    Some(e) => self.set_status(format!("{}: {e}", tr("Recording stopped")), true),
                    None => {
                        let first = files.first().map(|f| f.display().to_string()).unwrap_or_default();
                        self.set_status(format!("{}{first}", tr("Last recorded file: ")), false);
                        if self.vm.state.extra.record_finish_open
                            && let Some(f) = recording_main_file(&files).cloned()
                            && let Err(e) = self.load_file(&f)
                        {
                            self.set_status(format!("{}: {e}", tr("Cannot play file")), true);
                        }
                    }
                }
                self.vm.last_recording = files;
            }
            Notification::RecordingError { error } => self.set_status(format!("{}: {error}", tr("Recording error")), true),
            Notification::RecordingDroppedFrames { frames } => {
                self.set_status(format!("{}: {frames}", tr("Recording: the disk could not keep up, frames lost")), true)
            }
            Notification::PlaybackError { error } => self.set_status(format!("{}: {error}", tr("Cannot play file")), true),
            Notification::FileFinished | Notification::SampleFinished { .. } => {}
            Notification::LatencyMeasured { id, round_trip_ms, estimated_ms } => {
                self.vm.latency_tests.retain(|t| t.0 != id);
                self.vm.latency_tests.push((id, round_trip_ms, estimated_ms));
            }
            Notification::LatencyMatchReady => {}
            Notification::LatencyMatchRequested { from, latency_ms } => {
                self.vm.latency_request = Some((crate::clean(&from, NAME_LEN), latency_ms));
            }
            // joining one would leave the secure session for an unencrypted group on whatever
            // server we last used: not offered from inside a secure session
            Notification::GroupSuggested(_) if self.vm.secure.is_some() => {}
            Notification::GroupSuggested(mut g) => {
                g.user = crate::clean(&g.user, NAME_LEN);
                g.group = crate::clean(&g.group, NAME_LEN);
                g.others.iter_mut().for_each(|o| *o = crate::clean(o, NAME_LEN));
                self.vm.group_suggestion = Some(g);
            }
        }
        false
    }

    // ---- connection ---------------------------------------------------------------------------

    /// Logs in (and joins `r.group` unless empty) without blocking: DNS runs on a thread.
    pub fn connect(&mut self, r: ConnectRequest) {
        if r.user.trim().is_empty() {
            return self.set_status(tr("You need to specify a user name!"), true);
        }
        if r.host.trim().is_empty() {
            return self.set_status(tr("Server address is invalid!"), true);
        }
        let user = r.user.trim().to_owned();
        self.edit(move |s| s.extra.last_username = user.clone());
        // the engine ends a secure session when it connects
        self.secure_start = None;
        self.vm.connecting = true;
        self.set_status(tr("Connecting..."), false);
        self.vm.request = Some(r.clone());
        let engine = self.engine.clone().expect("engine running");
        let tx = self.jobs.0.clone();
        std::thread::spawn(move || {
            let group = (!r.group.trim().is_empty()).then(|| GroupJoin {
                name: r.group.trim().to_owned(),
                password: r.group_password.clone(),
                public: r.public,
            });
            let res = engine.connect(r.host.trim(), r.port, r.user.trim(), &r.user_password, group);
            let _ = tx.send(Job::Connected(res));
        });
    }

    /// Joins a group on the current server (public group list click: `public = true`).
    pub fn join_group(&mut self, group: &str, password: &str, public: bool) {
        if let Some(r) = &mut self.vm.request {
            (r.group, r.group_password, r.public) = (group.into(), password.into(), public);
        }
        self.engine().join_group(group, password, public);
    }

    /// Disconnects (or leaves the secure session); also stops a session hosted here unless
    /// [`Self::keep_hosting`].
    pub fn disconnect(&mut self) {
        self.engine().disconnect();
        self.leave_secure();
        self.vm.connecting = false;
        self.vm.reconnecting = false;
        if !self.keep_hosting {
            self.server = None;
            self.vm.hosting = None;
        }
    }

    /// Serverless connection to `host:port` (resolved off-thread).
    pub fn connect_direct(&mut self, addr: &str) {
        self.secure_start = None;
        let addr = addr.trim().to_owned();
        let engine = self.engine.clone().expect("engine running");
        let tx = self.jobs.0.clone();
        std::thread::spawn(move || {
            let res = addr.to_socket_addrs().and_then(|mut a| {
                a.find(SocketAddr::is_ipv4)
                    .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no IPv4 address"))
            });
            if let Ok(a) = res {
                engine.connect_direct(a);
            }
            let _ = tx.send(Job::Direct(res));
        });
    }

    /// Opens a `sonobus://` / sblaunch link: fills `form` from it. Returns false if the text
    /// holds no link.
    pub fn apply_link(&mut self, text: &str, form: &mut ConnectRequest) -> bool {
        match links::parse_link(text) {
            Some(l) => {
                form.apply_link(&l);
                self.set_status(tr("Filled in Group from link! Press 'Connect to Group' to join..."), false);
                true
            }
            None => false,
        }
    }

    /// The share link of the current group, if any.
    pub fn group_link(&self) -> Option<String> {
        if self.vm.secure.is_some() {
            return None; // a secure session's link is its invite
        }
        let r = self.vm.request.as_ref()?;
        let group = self.vm.snapshot.group.as_ref()?;
        Some(links::share_link(&r.host, r.port, group, &r.group_password, r.public))
    }

    /// Acts on the command line (SonoStandaloneFilterApp.cpp:380-430): loads a setup, fills
    /// the connect form from a link and the options, then hosts (`--host-server`) or connects
    /// (a group was given). Returns the form for the connect page.
    pub fn apply_cli(&mut self, cli: &links::Cli) -> ConnectRequest {
        if let Some(f) = &cli.load_setup {
            let mut path = PathBuf::from(f);
            if !path.exists() && path.is_relative() {
                path = self.setup_folder().join(f);
            }
            if let Err(e) = self.load_setup(&path) {
                self.set_status(format!("{}: {} {e}", tr("Error while loading"), path.display()), true);
            }
        }
        let mut form = self.default_request();
        let mut connect = false;
        if let Some(l) = cli.url.as_deref().and_then(links::parse_link) {
            form.apply_link(&l);
            connect = l.group.is_some();
        }
        if let Some((h, p)) = &cli.server {
            (form.host, form.port) = (h.clone(), *p);
        }
        if let Some(g) = &cli.group {
            (form.group, connect) = (g.clone(), true);
        }
        if let Some(p) = &cli.group_password {
            form.group_password = p.clone();
        }
        if let Some(u) = &cli.user {
            form.user = u.clone();
        }
        if let Some(r) = cli.relay.as_deref().and_then(|r| links::relay_choice(r).ok()) {
            self.set_secure_relay(r);
        }
        if let Some(name) = &cli.secure_host {
            let f = HostForm {
                session: name.clone(),
                broadcast: cli.broadcast,
                password: cli.password.clone().unwrap_or_default(),
                pin: cli.pin,
                lan: cli.lan,
                ..HostForm::default()
            };
            self.host_secure(&form.user, &f);
            return form;
        }
        if let Some(link) = &cli.secure_join {
            // like a link to an unknown server: joining waits for the user's yes (headless:
            // the command line is that yes)
            if !self.open_invite(link) {
                self.set_status(tr("This is not a secure session invite link."), true);
            }
            return form;
        }
        if let Some(port) = cli.host_server {
            if form.group.is_empty() {
                form.group = "session".into();
            }
            if let Err(e) = self.host_session(port, form.clone()) {
                self.set_status(format!("{}: {e}", tr("Could not host a session")), true);
            }
        } else if connect && self.known_server(&form.host, form.port) {
            self.connect(form.clone());
        } else if connect {
            // a link can name any server: it would get our address and user name
            self.vm.pending_link = Some(form.clone());
        }
        form
    }

    /// The default server, or one in the recent connections.
    fn known_server(&self, host: &str, port: u16) -> bool {
        let d = ConnectRequest::default();
        (host.eq_ignore_ascii_case(&d.host) && port == d.port)
            || self.vm.state.recents.iter().any(|r| r.server_host.eq_ignore_ascii_case(host) && r.server_port == i32::from(port))
    }

    /// Answers the confirmation of a link to an unknown server: connect, or forget it.
    pub fn answer_pending_link(&mut self, connect: bool) {
        if let Some(r) = self.vm.pending_link.take()
            && connect
        {
            self.connect(r);
        }
    }

    // ---- hosting ------------------------------------------------------------------------------

    /// Hosts a connection server on `port` and joins it with `r` (its host/port are replaced by
    /// this device's LAN address, which the server relays to the other members).
    pub fn host_session(&mut self, port: u16, mut r: ConnectRequest) -> io::Result<()> {
        self.stop_hosting();
        let handle = start_server(port)?;
        let port = handle.local_addr().port();
        let addresses = lan_addresses();
        let local = addresses.first().copied().unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST));
        self.vm.hosting = Some((port, addresses.clone()));
        self.server = Some(HostedServer { _handle: handle, port, addresses });
        (r.host, r.port) = (local.to_string(), port);
        self.connect(r);
        Ok(())
    }

    pub fn stop_hosting(&mut self) {
        if self.server.take().is_some() {
            self.engine().disconnect();
            self.vm.connecting = false;
        }
        self.vm.hosting = None;
    }

    // ---- secure sessions ----------------------------------------------------------------------

    /// The network options of a new secure session: our name, where to bind, the relay choice.
    fn secure_net(&self, user: &str) -> NetOptions {
        let user = user.trim();
        NetOptions {
            name: if user.is_empty() { default_user(&self.vm.state) } else { user.to_owned() },
            bind_ip: self.net_place.bind_ip,
            port: self.net_place.port,
            relay: self.vm.secure_prefs.relay.clone(),
        }
    }

    /// What every start does first: the name is remembered, a running session (compatible or
    /// secure; the server hosted here, unless kept) ends.
    fn begin_secure(&mut self, user: &str) {
        let user = user.trim().to_owned();
        if !user.is_empty() {
            self.edit(move |s| s.extra.last_username = user.clone());
        }
        if !self.keep_hosting {
            self.stop_hosting();
        }
        self.leave_secure(); // the one running ends before the new one is remembered
        (self.vm.request, self.vm.connecting, self.vm.reconnecting) = (None, false, false);
        (self.vm.secure_retry_at, self.vm.pin_invalidated) = (None, None);
        self.set_status(tr("Starting the secure session..."), false);
    }

    /// Starts `started` on a worker (binding, a rendezvous address lookup): the outcome comes
    /// back as a job, the session shows in [`ViewModel::secure`].
    fn spawn_secure(&mut self, net: NetOptions, rdv: Option<sonobus_net::RdvAddr>, mut started: Started) {
        if let Started::Host(o) = &mut started {
            // chosen here, so a restart on a new engine keeps the invite link
            o.seed.get_or_insert_with(sonobus_net::auth::random_seed);
        }
        self.vm.secure_starting = true;
        self.secure_gen += 1;
        // known before it runs: an engine rebuild meanwhile starts it again on the new one
        self.secure_start = Some((net.clone(), rdv.clone(), started.clone()));
        let (engine, tx, generation) = (self.shared_engine(), self.jobs.0.clone(), self.secure_gen);
        std::thread::spawn(move || {
            let res = (|| {
                if let Some(a) = rdv {
                    let addr = a.resolve().map_err(io::Error::other)?;
                    match &mut started {
                        Started::Host(o) => o.register_at = Some(addr),
                        Started::Name(server, ..) => *server = addr,
                        _ => {}
                    }
                }
                start_secure(&engine, &net, &started)
            })();
            let _ = tx.send(Job::Secure(generation, res));
        });
    }

    fn rdv_addr(&mut self, text: &str) -> Result<Option<sonobus_net::RdvAddr>, ()> {
        match text.trim() {
            "" => Ok(None),
            t => sonobus_net::RdvAddr::parse(t).map(Some).map_err(|e| self.set_status(format!("{}: {e}", tr("Rendezvous server address")), true)),
        }
    }

    /// Hosts a secure session as the form says.
    pub fn host_secure(&mut self, user: &str, f: &HostForm) {
        if let Some(p) = f.problem() {
            return self.set_status(tr(p), true);
        }
        let Ok(rdv) = self.rdv_addr(&f.register_at) else { return };
        let name = crate::clean(f.session.trim(), NAME_LEN);
        let mut o = HostOptions::new(f.session.trim());
        o.mode = if f.broadcast { SessionMode::Broadcast } else { SessionMode::Mesh };
        o.password = (!f.password.is_empty()).then(|| f.password.clone());
        o.pin = f.pin;
        o.lan = if f.lan { self.net_place.lan } else { LanScope::Off };
        o.rendezvous = f.rendezvous_here;
        o.max_listeners = (f.broadcast && f.max_listeners > 0).then_some(f.max_listeners as usize);
        o.playout_ms = self.vm.secure_prefs.playout().filter(|_| f.broadcast);
        // a remembered session hosted again keeps its invite link (its seed, our identity)
        o.seed = self
            .vm
            .secure_prefs
            .recents
            .iter()
            .find(|r| r.how == How::Hosted && r.name == name)
            .and_then(|r| sonobus_net::auth::Invite::parse(r.invite.as_deref()?).ok())
            .map(|i| i.seed);
        if rdv.is_some() {
            self.vm.secure_prefs.rendezvous = f.register_at.trim().to_owned();
            self.save_secure_prefs();
        }
        self.begin_secure(user);
        let recent = SecureRecent { name, broadcast: f.broadcast, how: How::Hosted, invite: None, server: rdv.as_ref().map(ToString::to_string) };
        self.pending_recent = Some((recent, f.remember));
        let net = self.secure_net(user);
        self.spawn_secure(net, rdv, Started::Host(o));
    }

    /// Joins a secure session. `remember`: keep an invite link in the remembered sessions.
    pub fn join_secure(&mut self, user: &str, m: JoinMethod, remember: bool) {
        let (started, rdv, recent) = match m {
            JoinMethod::Invite(text) => {
                let Some(p) = InvitePreview::parse(&text) else {
                    return self.set_status(tr("This is not a secure session invite link."), true);
                };
                let r = SecureRecent { name: p.session, broadcast: p.broadcast, how: How::Invite, invite: remember.then(|| p.link.clone()), server: None };
                (Started::Invite(p.link), None, r)
            }
            JoinMethod::Lan(d, secret) => {
                let r = SecureRecent { name: crate::clean(&d.session, NAME_LEN), broadcast: false, how: How::Lan, invite: None, server: None };
                (Started::Lan(d, secret.trim().to_owned()), None, r)
            }
            JoinMethod::Name { server, session, password } => {
                let Ok(Some(a)) = self.rdv_addr(&server) else {
                    return self.set_status(tr("Enter the rendezvous server address."), true);
                };
                self.vm.secure_prefs.rendezvous = server.trim().to_owned();
                self.save_secure_prefs();
                let r = SecureRecent { name: crate::clean(session.trim(), NAME_LEN), broadcast: false, how: How::Name, invite: None, server: Some(a.to_string()) };
                let placeholder = PeerAddr { id: a.id, addrs: Vec::new(), relay: a.relay.clone() };
                (Started::Name(placeholder, session.trim().to_owned(), password), Some(a), r)
            }
        };
        self.begin_secure(user);
        self.pending_recent = Some((recent, remember));
        let net = self.secure_net(user);
        self.spawn_secure(net, rdv, started);
    }

    /// Leaves the secure session (no-op without one).
    pub fn leave_secure(&mut self) {
        self.engine().leave_session();
        (self.secure_start, self.pending_recent, self.remember_invite) = (None, None, None);
        (self.vm.secure, self.vm.pin_invalidated) = (None, None);
    }

    /// Host: a new invite link (earlier links stop working), removing `remove` from the session
    /// first if given. Returns the new link.
    pub fn rotate_invite(&mut self, remove: Option<PeerId>) -> Option<String> {
        let who = remove.and_then(|id| self.vm.snapshot.peers.iter().find(|p| p.id == id)).map(|p| p.user.clone());
        let link = self.engine().rotate_session_key(remove)?;
        let msg = match who {
            Some(u) => format!("{u}: {}", tr("removed from the session. The invite link changed: earlier links no longer work. If the session has a password or PIN, change it too: they still know it.")),
            None => tr("New invite link: earlier links no longer work.").to_owned(),
        };
        self.set_status(msg, false);
        self.vm.secure = self.engine().session_status();
        Some(link)
    }

    /// Host in PIN mode: a fresh PIN.
    pub fn new_pin(&mut self) {
        if self.engine().new_session_pin().is_some() {
            self.vm.pin_invalidated = None;
            self.vm.secure = self.engine().session_status();
        }
    }

    /// "Sync listeners" (hosting "Share sound"): remembered, and applied at once to a session
    /// we host (the engine ignores it otherwise).
    pub fn set_playout_sync(&mut self, on: bool, ms: u16) {
        let p = &mut self.vm.secure_prefs;
        (p.playout_sync, p.playout_ms) = (on, secure::clamp_playout_ms(ms));
        let playout = p.playout();
        self.save_secure_prefs();
        self.engine().set_playout_sync(playout);
    }

    pub fn set_secure_relay(&mut self, relay: RelayChoice) {
        self.vm.secure_prefs.relay = relay;
        self.save_secure_prefs();
    }

    /// Forgets a remembered secure session (and the invite link it may hold).
    pub fn forget_secure_recent(&mut self, i: usize) {
        if i < self.vm.secure_prefs.recents.len() {
            self.vm.secure_prefs.recents.remove(i);
            self.save_secure_prefs();
        }
    }

    fn save_secure_prefs(&mut self) {
        if let Some(st) = &mut self.store {
            self.vm.secure_prefs.store(&mut st.props);
        }
    }

    /// An invite link from outside (command line, the OS): shown for confirmation first.
    /// Returns false when `text` is no invite link.
    pub fn open_invite(&mut self, text: &str) -> bool {
        self.vm.pending_invite = InvitePreview::parse(text);
        self.vm.pending_invite.is_some()
    }

    /// Answers the confirmation of an invite link from outside: join it as `user`, or forget it.
    pub fn answer_pending_invite(&mut self, user: &str, join: bool) {
        if let Some(p) = self.vm.pending_invite.take()
            && join
        {
            self.join_secure(user, JoinMethod::Invite(p.link), false);
        }
    }

    /// Starts or stops looking for secure sessions on this network.
    pub fn lan_browse(&mut self, on: bool) {
        if !on {
            self.engine().stop_lan_browse();
            self.vm.lan_browsing = false;
            self.vm.lan_sessions.clear();
            return;
        }
        match self.engine().lan_browse(self.net_place.lan) {
            Ok(()) => self.vm.lan_browsing = true,
            Err(e) => self.set_status(format!("{}: {e}", tr("Cannot search this network")), false),
        }
    }

    /// A new device identity ("Reset device identity"): invite links naming this device stop
    /// working, others see a new device. Applies from the next secure session.
    pub fn reset_identity(&mut self) {
        let id = sonobus_net::generate_secret_key();
        self.identity = Some(id);
        self.engine().set_identity_key(id);
        if let Some(st) = &mut self.store {
            secure::store_identity(&mut st.props, &id);
            // invite links remembered as hosted name the old identity
            self.vm.secure_prefs.recents.iter_mut().filter(|r| r.how == How::Hosted).for_each(|r| r.invite = None);
            self.save_secure_prefs();
        }
        if let Err(e) = self.save() {
            self.set_status(format!("{}: {e}", tr("Error while saving")), true);
        } else {
            self.set_status(tr("New device identity: it applies from the next secure session."), false);
        }
    }

    fn secure_tick(&mut self) {
        let st = self.engine().session_status().map(|mut s| {
            s.session = crate::clean(&s.session, NAME_LEN);
            s
        });
        self.vm.secure = st;
        while let Some(e) = self.engine().poll_session_event() {
            self.on_session_event(e);
        }
        if self.vm.lan_browsing {
            self.vm.lan_sessions = self.engine().lan_sessions();
            for d in &mut self.vm.lan_sessions {
                d.session = crate::clean(&d.session, NAME_LEN);
            }
        }
        let Some(s) = self.vm.secure.clone() else {
            self.vm.pin_invalidated = None;
            self.remember_invite = None;
            return;
        };
        // remembered once it is up
        if (s.role == SessionRole::Host || s.peers > 0)
            && let Some((mut r, keep)) = self.pending_recent.take()
        {
            r.broadcast = s.mode == SessionMode::Broadcast;
            if r.how == How::Hosted && keep {
                self.remember_invite = Some(r.name.clone());
            }
            if !keep {
                r.invite = None;
            }
            self.vm.secure_prefs.remember(r);
            self.save_secure_prefs();
        }
        if let (Some(name), Some(inv)) = (&self.remember_invite, &s.invite)
            && let Some(r) = self.vm.secure_prefs.recents.iter_mut().find(|r| r.how == How::Hosted && &r.name == name)
            && r.invite.as_ref() != Some(inv)
        {
            r.invite = Some(inv.clone());
            self.save_secure_prefs();
        }
        if self.vm.status == tr("Starting the secure session...") {
            if s.role == SessionRole::Host {
                self.set_status(format!("{}{}", tr("Hosting secure session: "), s.session), false);
            } else if s.peers > 0 {
                self.set_status(format!("{}{}", tr("Joined secure session: "), s.session), false);
            }
        }
    }

    /// Session notices in the status line, calmly: none of them is our fault.
    fn on_session_event(&mut self, e: SessionEvent) {
        let secs = |d: Duration| format!("{} s", d.as_secs_f32().ceil() as u64);
        let joining = self.vm.secure.as_ref().is_some_and(|s| s.role != SessionRole::Host && s.peers == 0);
        let msg = match e {
            SessionEvent::JoinFailed { reason, retry_after } => {
                let mut m = if reason == "authentication failed" {
                    tr("Could not join: the invite link, PIN or password was not accepted (an invite link stops working when the host replaces it).").to_owned()
                } else {
                    format!("{}: {reason}", tr("Could not join"))
                };
                if let Some(d) = retry_after {
                    self.vm.secure_retry_at = Some(Instant::now() + d);
                    m += &format!(" {} {}.", tr("Try again in"), secs(d));
                }
                if joining {
                    self.leave_secure();
                }
                m
            }
            // `Ended` (session full) follows and says so
            SessionEvent::SessionFull => return,
            SessionEvent::InviteChanged(_) => return,
            SessionEvent::PathChanged { id, direct, relay, rtt_ms } => {
                let who = self.vm.snapshot.peers.iter().find(|p| p.id == id).map_or_else(String::new, |p| p.user.clone());
                let how = match (direct, relay) {
                    (true, _) => tr("direct connection").to_owned(),
                    (false, Some(r)) => format!("{} {}", tr("relayed via"), secure::relay_host(&r)),
                    (false, None) => tr("relayed").to_owned(),
                };
                format!("{who}: {how}, {rtt_ms:.0} ms")
            }
            SessionEvent::PasswordJoinFailed { failed_total } => {
                format!("{} {failed_total}", tr("Someone tried to join with a wrong PIN or password. Failed attempts so far:"))
            }
            SessionEvent::IncomingFailed { count, last_reason } => {
                format!("{} {count} ({last_reason})", tr("Connections refused:"))
            }
            SessionEvent::PinInvalidated => {
                self.vm.pin_invalidated = Some(Instant::now());
                tr("Too many wrong PIN guesses: this PIN no longer works. Set a new PIN to let more people join.").to_owned()
            }
            SessionEvent::Throttled { ip, retry_after } => {
                let from = ip.map_or_else(String::new, |ip| format!(" ({ip})"));
                format!("{}{from} {} {}.", tr("Join attempts are paused after wrong guesses"), tr("Try again in"), secs(retry_after))
            }
            SessionEvent::RendezvousFailed { reason, retry_after } => {
                format!("{}: {reason}. {} {}.", tr("The rendezvous server cannot be reached"), tr("Trying again in"), secs(retry_after))
            }
            // the host rotated it itself (and said so); a password join learns it on the way in
            SessionEvent::SessionKeyChanged if joining || self.vm.secure.as_ref().is_some_and(|s| s.role == SessionRole::Host) => return,
            SessionEvent::SessionKeyChanged => tr("The session has a new key: invite links shared before no longer work.").to_owned(),
            SessionEvent::Ended { reason } => {
                self.leave_secure();
                secure::ended_text(&reason)
            }
        };
        self.set_status(msg, false);
    }

    // ---- settings -----------------------------------------------------------------------------

    /// Changes the settings (UI edits): applied to the view model and to the engine.
    pub fn edit(&mut self, f: impl Fn(&mut AppState)) {
        let old = self.host.is_some().then(|| self.vm.state.params.clone());
        f(&mut self.vm.state);
        self.engine().update_state(f);
        if let (Some(h), Some(old)) = (&mut self.host, old)
            && old != self.vm.state.params
        {
            h.params_edited(&old, &self.vm.state.params);
        }
    }

    /// An engine setter that changes the settings (options such as packet redundancy), then
    /// the view model follows.
    pub fn engine_edit(&mut self, f: impl FnOnce(&Engine)) {
        f(self.engine());
        self.refresh_state();
    }

    /// Lets an address that was blocked connect again.
    pub fn unblock(&mut self, ip: IpAddr) {
        let mut b = self.engine().blocked_addresses();
        b.retain(|x| *x != ip);
        self.engine().set_blocked_addresses(b.clone());
        self.vm.blocked = b;
    }

    /// Lets a device blocked in this secure session connect again.
    pub fn unblock_endpoint(&mut self, id: [u8; 32]) {
        self.engine().unblock_endpoint(id);
        self.vm.blocked_endpoints = self.engine().blocked_endpoints();
    }

    /// Input group solo (monitoring only).
    pub fn set_input_soloed(&mut self, group: usize, soloed: bool) {
        self.engine().set_input_soloed(group, soloed);
        if let Some(v) = self.vm.input_solo.get_mut(group) {
            *v = soloed;
        }
    }

    // ---- OSC remote control -------------------------------------------------------------------

    /// Applies (and persists, under the fork's keys) an OSC configuration. Turning it on, or
    /// turning "send state on start" on while enabled, sends the full state, as the fork does.
    pub fn set_osc_config(&mut self, cfg: OscConfig) {
        let old = self.vm.osc.config.clone();
        let dump = cfg.enabled && cfg.send_state_on_start && (!old.enabled || !old.send_state_on_start);
        let stored = cfg.clone();
        self.edit(move |s| stored.store(&mut s.extra));
        self.osc.configure(cfg, dump);
        self.vm.osc = self.osc.status();
    }

    /// A one-off OSC message (button echo) with the next feedback.
    pub fn osc_event(&mut self, addr: &str, v: osc_control::Var) {
        self.osc.event(addr, v);
    }

    fn osc_tick(&mut self) {
        if !self.osc.enabled() {
            return;
        }
        for (addr, arg) in self.osc.take_inbound() {
            osc_control::apply(self, &addr, &arg);
        }
        let now = Instant::now();
        if self.osc.feedback_due(now) {
            let fb = osc_control::state(self, self.engine().receive_sync_padding(), self.ptt.is_some());
            self.osc.feedback(now, fb);
        }
        self.osc.meters(now, &self.vm.snapshot.peers);
    }

    /// Push-to-talk (`T`): while held, mute everyone else and unmute our send.
    pub fn push_to_talk(&mut self, down: bool) {
        match (down, self.ptt) {
            (true, None) => {
                let p = &self.vm.state.params;
                self.ptt = Some((p.main_recv_mute, p.main_send_mute));
                self.edit(|s| (s.params.main_recv_mute, s.params.main_send_mute) = (true, false));
            }
            (false, Some((recv, send))) => {
                self.ptt = None;
                self.edit(move |s| (s.params.main_recv_mute, s.params.main_send_mute) = (recv, send));
            }
            _ => {}
        }
    }

    pub fn add_input_group(&mut self) {
        self.edit(|s| {
            let n = s.input_groups.len();
            if n < sonobus_core::state::MAX_CHANGROUPS {
                let start = s.input_groups.last().map_or(0, |g| g.chan_start + g.num_channels);
                let mut g = ChannelGroup::input_default(start as usize, s.is_plugin);
                g.chan_start = start;
                s.input_groups.push(g);
            }
        });
    }

    pub fn remove_input_group(&mut self, i: usize) {
        self.edit(move |s| {
            if i < s.input_groups.len() && s.input_groups.len() > 1 {
                s.input_groups.remove(i);
            }
        });
    }

    pub fn audio_setup(&self) -> &DeviceSetup {
        &self.setup
    }

    /// Applies a new audio setup. A new rate or block size, or more input channels than the
    /// engine has, rebuilds the engine (rejoining the group); otherwise only the devices are
    /// reopened. Enabling system
    /// sound or a platform input adds an input group for it.
    pub fn set_audio_setup(&mut self, mut setup: DeviceSetup) -> io::Result<()> {
        if self.audio.is_none() {
            return Err(io::Error::new(io::ErrorKind::Unsupported, "the host does the audio"));
        }
        if !self.use_devices {
            setup.enabled = false;
        }
        let (old_setup, new_setup) = (self.setup.clone(), setup.clone());
        if old_setup.device_input_channels() != new_setup.device_input_channels()
            || old_setup.extra_input_channels() != new_setup.extra_input_channels()
            || old_setup.shares_sound() != new_setup.shares_sound()
        {
            self.edit(move |s| {
                if fit_input_groups(&mut s.input_groups, &old_setup, &new_setup) {
                    s.params.send_channels = sonobus_core::state::SendChannels::Stereo as i32;
                }
            });
        }
        let (new, old) = (setup.audio_config(), self.setup.audio_config());
        let rebuild = new.sample_rate != old.sample_rate
            || setup.block_size != self.setup.block_size
            || new.input_channels > self.engine_inputs;
        self.setup = setup.clone();
        self.vm.audio_setup = setup.clone();
        if rebuild {
            self.rebuild_engine()
        } else {
            if let Some(a) = &self.audio {
                a.set_setup(setup);
            }
            Ok(())
        }
    }


    /// Sets (or removes, `None`) the platform-pushed input, e.g. Android playback capture once
    /// its `AudioRecord` reports its format. No-op when unchanged.
    pub fn set_external_input(&mut self, spec: Option<ExternalSpec>) -> io::Result<()> {
        if self.setup.external == spec {
            return Ok(());
        }
        let setup = DeviceSetup { external: spec, ..self.setup.clone() };
        self.set_audio_setup(setup)
    }

    /// Where the platform pushes [`DeviceSetup::external`] audio (see
    /// [`crate::audio::push_external`]). Stays valid across engine rebuilds.
    /// Hosted: a slot nothing reads.
    pub fn external_input(&self) -> ExternalSlot {
        self.audio.as_ref().map(AudioManager::external_input).unwrap_or_default()
    }

    /// Drops the engine (freeing its UDP port, unless a connect job still holds it) and
    /// returns its settings and what the next one keeps.
    fn release_engine(&mut self) -> (AppState, Carry) {
        let state = self.engine().state();
        let blocked = self.engine().blocked_addresses();
        let mut secure = self.secure_start.take();
        if let Some((_, _, Started::Host(o))) = &mut secure
            && let Some(inv) = self.vm.secure.as_ref().and_then(|s| s.invite.as_deref()).and_then(|l| sonobus_net::auth::Invite::parse(l).ok())
        {
            o.seed = Some(inv.seed); // the current key (rotations included): links stay valid
        }
        let rejoin = (secure.is_none() && (self.vm.snapshot.connection != Connection::Disconnected || self.vm.connecting))
            .then(|| self.vm.request.clone())
            .flatten();
        // the old engine's last notifications (a recording it stopped) arrive through this
        let notes = self.engine().notification_handle();
        let recording = self.vm.recording.take().is_some();
        self.engine = None;
        while let Some(n) = notes.poll() {
            match n {
                Notification::RecordingStopped { files, error: None } if recording => {
                    let first = files.first().map(|f| f.display().to_string()).unwrap_or_default();
                    self.set_status(format!("{} {first}", tr("Recording stopped because the audio setup changed. Recorded:")), true);
                    self.vm.last_recording = files;
                }
                n => {
                    self.on_notification(n);
                }
            }
        }
        (state, Carry { blocked, rejoin, secure })
    }

    fn carry_over(&mut self, c: Carry) {
        self.engine().set_blocked_addresses(c.blocked);
        if let Some(id) = self.identity {
            self.engine().set_identity_key(id);
        }
        if let Some(r) = c.rejoin {
            self.connect(r);
        }
        if let Some((net, rdv, started)) = c.secure {
            self.spawn_secure(net, rdv, started);
        }
    }

    /// Replaces the engine (new rate/block size), keeping settings and rejoining the group.
    /// When the new one cannot be built, a fallback runs (any UDP port, then the default audio
    /// setup) and [`ViewModel::engine_error`] says why: the controller always has an engine.
    fn rebuild_engine(&mut self) -> io::Result<()> {
        let (state, carry) = self.release_engine();
        let res = self.start_engine(state.clone());
        if let Err(e) = &res {
            self.vm.engine_error = Some(format!("{}: {e}", tr("Audio engine unavailable")));
            let mut any_port = state.clone();
            any_port.extra.use_udp_port = 0;
            if self.start_engine(any_port.clone()).is_err() {
                self.setup = DeviceSetup { enabled: false, ..DeviceSetup::default() };
                self.vm.audio_setup = self.setup.clone();
                self.start_engine(any_port).map_err(|e| io::Error::other(format!("no engine: {e}")))?;
            }
            // the settings keep the wanted UDP port
            let port = state.extra.use_udp_port;
            self.engine().update_state(move |s| s.extra.use_udp_port = port);
        } else {
            self.vm.engine_error = None;
        }
        self.carry_over(carry);
        self.refresh_state();
        res
    }

    /// Tries the wanted engine again after [`ViewModel::engine_error`].
    pub fn retry_engine(&mut self) {
        if let Err(e) = self.rebuild_engine() {
            self.set_status(format!("{}: {e}", tr("Audio engine unavailable")), true);
        }
    }

    /// Hosted: the host rebuilds the engine (new sample rate, block size or layout). `build`
    /// gets the settings once the old engine is gone (its UDP port free) and returns the new
    /// one. The session stays: chat, status, blocked addresses, the connect request; a joined
    /// group is rejoined. On error the controller has no engine and must be dropped.
    pub fn swap_engine(&mut self, build: impl FnOnce(AppState) -> io::Result<Arc<Engine>>) -> io::Result<()> {
        let _lang = crate::i18n::scope(self.lang);
        let (state, carry) = self.release_engine();
        let engine = build(state)?;
        engine.set_soundboards(self.vm.soundboards.clone());
        self.engine = Some(engine);
        self.carry_over(carry);
        self.refresh_state();
        Ok(())
    }

    pub fn scan_devices(&self, host: Option<String>) {
        if let Some(a) = &self.audio {
            a.scan(host);
        }
    }

    pub fn retry_audio(&self) {
        if let Some(a) = &self.audio {
            a.retry_now();
        }
    }

    /// Writes `SonoBus.settings` and `GlobalState.xml` now (no-op without a settings dir).
    pub fn save(&mut self) -> io::Result<()> {
        self.last_save = Instant::now();
        if let Some(h) = self.saving.take() {
            let _ = h.join(); // an autosave in flight goes first
        }
        match self.save_files() {
            Some(files) => files.into_iter().try_for_each(|(p, d)| settings::write_atomic(&p, &d)),
            None => Ok(()),
        }
    }

    /// The settings files to write (`None`: none, or another instance holds the directory).
    fn save_files(&mut self) -> Option<Vec<(PathBuf, Vec<u8>)>> {
        if self.vm.settings_read_only {
            return None;
        }
        let store = self.store.as_mut()?;
        let engine = self.engine.as_deref().expect("engine running");
        let state = engine.state();
        let blocked = engine.blocked_addresses();
        let files = store.files(&state, &self.setup, blocked.iter().map(ToString::to_string).collect());
        self.vm.blocked = blocked;
        self.vm.state = state;
        self.vm.recent_setups = store.standalone.recent_setup_files.clone();
        Some(files)
    }

    /// Replaces the settings with stock SonoBus's (`path`, or its default location).
    pub fn import_upstream(&mut self, path: Option<&Path>) -> io::Result<()> {
        let path = match path {
            Some(p) => p.to_path_buf(),
            None => settings::upstream_settings_file()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no SonoBus settings location"))?,
        };
        let (mut state, setup) = settings::import_settings(&path)?;
        settings::keep_local(&mut state, &self.engine().state());
        let mut mine = self.setup.clone();
        (mine.sample_rate, mine.block_size) = (setup.sample_rate, setup.block_size);
        self.edit(move |s| *s = state.clone());
        self.set_audio_setup(mine)?;
        // stock keeps soundboards.xml next to its settings
        let boards = path.with_file_name(settings::SOUNDBOARDS_FILE);
        if boards.exists() {
            self.set_soundboards(settings::load_soundboards(&boards)?);
        }
        self.set_status(format!("{} {}", tr("Imported settings from"), path.display()), false);
        Ok(())
    }

    pub fn load_setup(&mut self, path: &Path) -> io::Result<()> {
        let local = self.engine().state();
        let mut state = local.clone();
        let mut setup = self.setup.clone();
        settings::load_setup_file(path, &mut state, &mut setup)?;
        // only audio, mix and session settings come from a setup file
        settings::keep_local(&mut state, &local);
        self.edit(move |s| *s = state.clone());
        self.set_audio_setup(setup)?;
        if let Some(st) = &mut self.store {
            st.add_recent_setup(path);
            self.vm.recent_setups = st.standalone.recent_setup_files.clone();
        }
        Ok(())
    }

    pub fn save_setup(&mut self, path: &Path) -> io::Result<()> {
        settings::save_setup_file(path, &self.engine().state(), &self.setup)?;
        if let Some(st) = &mut self.store {
            st.add_recent_setup(path);
            self.vm.recent_setups = st.standalone.recent_setup_files.clone();
        }
        Ok(())
    }

    /// The folder new setup files go to by default.
    pub fn setup_folder(&self) -> PathBuf {
        self.store
            .as_ref()
            .map(|s| &s.standalone.last_recents_setup_folder)
            .filter(|f| !f.is_empty())
            .map(PathBuf::from)
            .or_else(|| self.vm.settings_dir.clone())
            .unwrap_or_else(|| PathBuf::from("."))
    }

    // ---- recording ----------------------------------------------------------------------------

    /// Where recordings go: `DefaultRecordDirURL`, else upstream's default (`~/Music/SonoBus`),
    /// else (Android) `Recordings` in the settings directory.
    pub fn record_dir(&self) -> PathBuf {
        settings::url_path(&self.vm.state.extra.default_record_dir_url)
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(settings::default_record_dir)
            .or_else(|| self.vm.settings_dir.as_ref().map(|d| d.join("Recordings")))
            .unwrap_or_else(|| PathBuf::from("."))
    }

    pub fn set_record_dir(&mut self, dir: &Path) {
        let url = settings::file_url(dir);
        self.edit(move |s| s.extra.default_record_dir_url = url.clone());
    }

    /// PR #78 is the engine's: what it stores is what the options show.
    pub fn set_recording_options(&mut self, mask: i32) {
        self.engine().set_recording_options(mask);
        self.refresh_state();
    }

    /// Starts off the UI thread (creating the files waits for the disk); the recording shows
    /// on a later tick, a failure in the status.
    pub fn start_recording(&mut self) {
        let dir = self.record_dir();
        let base = self.engine().recording_base_name(now_time());
        let engine = self.shared_engine();
        let tx = self.jobs.0.clone();
        std::thread::spawn(move || {
            if let Err(e) = engine.start_recording(&dir, &base) {
                let _ = tx.send(Job::RecordFailed(dir, e));
            }
        });
    }

    /// Stops off the UI thread (finalizing waits for the disk); the files arrive as a
    /// notification.
    pub fn stop_recording(&mut self) {
        let engine = self.engine.clone().expect("engine running");
        std::thread::spawn(move || engine.stop_recording());
        self.vm.recording = None;
    }

    pub fn toggle_recording(&mut self) {
        if self.vm.recording.is_some() { self.stop_recording() } else { self.start_recording() }
    }

    // ---- file playback ------------------------------------------------------------------------

    pub fn load_file(&mut self, path: &Path) -> io::Result<()> {
        let st = self.engine().load_file(path)?;
        self.vm.file = Some(st);
        if let Some(store) = &mut self.store {
            store.add_recent_file(path);
            self.vm.recent_files = store.recent_files();
        }
        self.refresh_state();
        Ok(())
    }

    pub fn close_file(&mut self) {
        self.engine().close_file();
        self.vm.file = None;
        self.refresh_state();
    }

    // ---- soundboard ---------------------------------------------------------------------------

    /// Replaces the soundboards: played from, and saved to `soundboards.xml` right away.
    pub fn set_soundboards(&mut self, boards: Soundboards) {
        self.engine().set_soundboards(boards.clone());
        self.vm.soundboards = boards;
        let res = match (&mut self.host, &self.vm.settings_dir) {
            (Some(h), _) => h.soundboards_changed(&self.vm.soundboards),
            (None, Some(dir)) => settings::save_soundboards(&dir.join(settings::SOUNDBOARDS_FILE), &self.vm.soundboards),
            (None, None) => Ok(()),
        };
        if let Err(e) = res {
            self.set_status(format!("{}: {e}", tr("Error while saving")), true);
        }
    }

    /// Hosted: "Save as default plugin options" (P.cpp:8803-8811).
    pub fn save_plugin_default(&mut self) {
        let state = self.engine().state();
        let res = match &mut self.host {
            Some(h) => h.save_default(&state),
            None => return,
        };
        match res {
            Ok(p) => self.set_status(format!("{} {}", tr("Saved"), p.display()), false),
            Err(e) => self.set_status(format!("{}: {e}", tr("Error while saving")), true),
        }
    }

    /// Hosted: "Reset default plugin options" (P.cpp:8826-8831).
    pub fn reset_plugin_default(&mut self) {
        let Some(h) = &mut self.host else { return };
        match h.reset_default() {
            Ok(()) => self.clear_status(),
            Err(e) => self.set_status(format!("{}: {e}", tr("Error while saving")), true),
        }
    }

    /// Edits the soundboards in place (then as [`Self::set_soundboards`]).
    pub fn edit_soundboards(&mut self, f: impl FnOnce(&mut Soundboards)) {
        let mut b = self.vm.soundboards.clone();
        f(&mut b);
        if b != self.vm.soundboards {
            self.set_soundboards(b);
        }
    }

    // ---- latency ------------------------------------------------------------------------------

    /// Opens the group latency match: asks every peer for its latencies.
    pub fn begin_latency_match(&mut self) {
        self.engine().begin_latency_match();
        self.latency_match_open = true;
        self.vm.latency_match = Some(self.engine().latency_match_info());
    }

    pub fn end_latency_match(&mut self) {
        self.latency_match_open = false;
        self.vm.latency_match = None;
    }

    /// Asks everyone to match `ms` and applies it here.
    pub fn send_latency_match(&mut self, ms: f32) {
        self.engine().send_latency_match(ms);
        self.end_latency_match();
    }

    /// Answers a peer's latency match request.
    pub fn answer_latency_request(&mut self, accept: bool) {
        if let Some((_, ms)) = self.vm.latency_request.take()
            && accept
        {
            self.engine().commit_latency_match(ms);
        }
    }

    /// "Suggest and Connect to Group": invites `users`, then moves there ourselves.
    pub fn suggest_group(&mut self, group: &str, password: &str, public: bool, users: &[String]) {
        let u: Vec<&str> = users.iter().map(String::as_str).collect();
        self.engine().suggest_group(group, password, public, &u);
        self.move_to_group(group, password, public);
    }

    /// Answers an invitation: accept leaves this group and joins the suggested one.
    pub fn answer_group_suggestion(&mut self, accept: bool) {
        if let Some(g) = self.vm.group_suggestion.take()
            && accept
        {
            self.move_to_group(&g.group, &g.group_password, g.public);
        }
    }

    fn move_to_group(&mut self, group: &str, password: &str, public: bool) {
        self.engine().leave_group();
        self.join_group(group, password, public);
    }

    // ---- window -------------------------------------------------------------------------------

    /// Saved window position and size (x, y, w, h in points), when complete.
    pub fn window(&self) -> Option<[i32; 4]> {
        let w = self.store.as_ref()?.standalone.window;
        Some([w[0]?, w[1]?, w[2]?, w[3]?])
    }

    /// Remembers the window geometry (saved with the settings).
    pub fn set_window(&mut self, rect: [i32; 4]) {
        if let Some(st) = &mut self.store {
            st.standalone.window = rect.map(Some);
        }
    }

    // ---- chat ---------------------------------------------------------------------------------

    /// Sends to the peers `ids`, or to everyone when empty. Secure sessions address them by id
    /// (names are only labels there, and can repeat); compatible ones by user name, as upstream.
    /// A target that left is an error, never a message to everyone.
    pub fn send_chat(&mut self, ids: &[PeerId], text: &str) {
        let sent = if self.engine().session_status().is_some() {
            self.engine().try_send_chat_to(ids, text)
        } else {
            let peers = &self.vm.snapshot.peers;
            let names: Vec<&str> = ids.iter().filter_map(|id| peers.iter().find(|p| p.id == *id)).map(|p| p.user.as_str()).collect();
            if names.len() < ids.len() { Err(tr("That user has left").to_owned()) } else { self.engine().try_send_chat(&names, text) }
        };
        match sent {
            Ok(m) => self.push_chat(m),
            Err(e) => self.set_status(e, true),
        }
    }

    fn push_chat(&mut self, mut m: ChatMessage) {
        for t in [&mut m.from, &mut m.group, &mut m.targets] {
            *t = crate::clean(t, NAME_LEN);
        }
        m.message = crate::clean(&m.message, CHAT_LEN);
        if self.vm.chat.len() >= CHAT_LINES {
            self.vm.chat.remove(0);
        }
        self.vm.chat.push(m);
    }

    /// A local "system" line (upstream `SBChatEvent::SystemType`): `from` is empty. Only in
    /// private groups, as upstream.
    fn system_chat(&mut self, text: String) {
        if self.vm.secure.is_none() && self.vm.request.as_ref().is_some_and(|r| r.public) {
            return;
        }
        let group = self.vm.snapshot.group.clone().unwrap_or_default();
        self.push_chat(ChatMessage { own: false, group, from: String::new(), targets: String::new(), message: text });
    }

    pub fn mark_chat_read(&mut self) {
        self.vm.unread_chat = 0;
    }

    /// Saves settings; call before exit (dropping the controller then stops everything).
    pub fn shutdown(&mut self) {
        if self.osc.enabled() {
            // paced like the feedback, so a receiver's socket buffer keeps up
            for chunk in osc_control::closing_messages().chunks(osc_control::BURST) {
                for (a, v) in chunk {
                    self.osc.send(a, v);
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        if let Err(e) = self.save() {
            eprintln!("saving settings failed: {e}");
        }
        self.stop_hosting();
    }
}

impl Drop for AppController {
    fn drop(&mut self) {
        // engine first: it leaves the server and stops its threads; then the audio manager
        self.engine = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_time() {
        let t = utc_time(1_700_000_000);
        assert_eq!((t.year, t.month, t.day, t.hour, t.minute, t.second), (2023, 11, 14, 22, 13, 20));
        let t = utc_time(951_782_400); // 2000-02-29
        assert_eq!((t.year, t.month, t.day), (2000, 2, 29));
        assert_eq!(utc_time(0).year, 1970);
    }

    #[test]
    fn input_groups_follow_the_opened_channels() {
        use crate::audio::{DeviceChoice, ExternalSpec};
        let span = |gs: &[ChannelGroup]| gs.iter().map(|g| (g.name.clone(), g.chan_start, g.num_channels)).collect::<Vec<_>>();
        let mic = |n: i32| {
            let mut g = ChannelGroup::input_default(0, false);
            g.num_channels = n;
            g
        };
        let ss = |s: i32| ("System Sound".to_owned(), s, 2);
        let ext = Some(ExternalSpec { channels: 2, sample_rate: 48000 });
        let stereo = DeviceSetup::default();
        let mono = DeviceSetup { input: DeviceChoice::Id("mono".into()), input_channels: Some(1), external: ext, ..DeviceSetup::default() };
        let no_mic = DeviceSetup { input: DeviceChoice::None, external: ext, ..DeviceSetup::default() };
        // the mic opens one channel: the capture (channels 1-2) gets its own group, the mic's
        // is cut to channel 0
        let mut g = vec![mic(2)];
        assert!(fit_input_groups(&mut g, &stereo, &mono));
        assert_eq!(span(&g), [(String::new(), 0, 1), ss(1)]);
        // no mic: the capture is on channels 0-1, the mic's group goes
        let mut g = vec![mic(2)];
        assert!(fit_input_groups(&mut g, &stereo, &no_mic));
        assert_eq!(span(&g), [ss(0)]);
        // a mic group the engine made with no device input (one channel at 0) goes too
        let mut g = vec![mic(1)];
        let none = DeviceSetup { input: DeviceChoice::None, ..DeviceSetup::default() };
        fit_input_groups(&mut g, &none, &no_mic);
        assert_eq!(span(&g), [ss(0)]);
        // the mic comes back: the capture group moves behind it, the mic gets a group again
        let with_ext = DeviceSetup { external: ext, ..DeviceSetup::default() };
        assert!(!fit_input_groups(&mut g, &no_mic, &with_ext));
        assert_eq!(span(&g), [(String::new(), 0, 2), ss(2)]);
        // system sound on as well: it comes first, the platform input after it
        let both = DeviceSetup { system_sound: Some("out".into()), ..with_ext.clone() };
        assert!(fit_input_groups(&mut g, &with_ext, &both));
        assert_eq!(span(&g), [(String::new(), 0, 2), ss(4), ss(2)]);
        // capture off: its group goes; a user group on unused channels stays
        let mut extra = mic(1);
        extra.chan_start = 8;
        g.push(extra);
        let sys_only = DeviceSetup { system_sound: Some("out".into()), ..DeviceSetup::default() };
        fit_input_groups(&mut g, &both, &sys_only);
        assert_eq!(span(&g), [(String::new(), 0, 2), ss(2), (String::new(), 8, 1)]);
        // nothing left: the engine still gets a group
        let mut g = vec![mic(2)];
        fit_input_groups(&mut g, &stereo, &none);
        assert_eq!(span(&g), [(String::new(), 0, 1)]);
    }

    #[test]
    fn main_recording_file() {
        let one = [PathBuf::from("/r/S_1.flac")];
        assert_eq!(recording_main_file(&one), Some(&one[0]));
        let many = [PathBuf::from("/r/S/S-SELF.flac"), PathBuf::from("/r/S/S-MIX.flac")];
        assert_eq!(recording_main_file(&many), Some(&many[1]));
        assert_eq!(recording_main_file(&many[..1]), Some(&many[0]));
    }
}
