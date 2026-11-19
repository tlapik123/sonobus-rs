//! The tokio side: one `sb-net` OS thread running a current-thread runtime owns the iroh
//! endpoint and every connection. The public API is synchronous: commands go in through
//! [`Net`], [`Event`]s come out of [`Events`], datagrams are sent with the cloneable [`PeerTx`]
//! from any thread and received by a per-connection [`Sink`] that runs on the runtime thread.
//!
//! Endpoint: iroh `Minimal` preset plus the relays of [`Config::relay`], never any address
//! lookup (no pkarr/DNS publishing or resolving: peers are dialed by the addresses and relay
//! URL we were given), fixed congestion window. With relays on, iroh connects through whichever
//! of relay and direct addresses works first and keeps hole-punching: the selected path moves
//! from relayed to direct when possible (and back when direct breaks), reported as
//! [`Event::PathChanged`]. Relays only forward end-to-end encrypted QUIC; they see both ends' IP
//! addresses and endpoint ids.
//!
//! # Identity
//!
//! The only authenticated identity of a peer is its endpoint id ([`PeerInfo::id`]): TLS proves
//! it on every connection and it cannot be spoofed. Names and roles (HELLO, WELCOME, ROSTER)
//! are self-declared labels; this crate grants nothing by them. A joiner that used an invite
//! link knows the host's id from the link ([`Config::host`]): [`PeerInfo::is_invite_host`]
//! marks that peer, and only that peer (or, after a password join, the endpoint that handed us
//! the session key) may rotate the session key ([`Net::authority`]).
//!
//! After every key change ([`Net::rotate_session_key`], or a REKEY from that authority) each
//! side of every connection sends KEY_PROOF (`auth::rekey_proof` under the new key) and
//! expects one back: a connection whose peer has not proven the current key within
//! `max(grace, KEY_PROOF_TIMEOUT)` is closed with [`CloseReason::Removed`] (a connection whose
//! handshake proved an older key counts as one that saw the key change). A member that
//! accepts a REKEY from its authority passes that same REKEY on, in the same step, to the
//! members that learned the key from it by password.
//!
//! Removal ([`Net::evict`]): the host blocks the member (its links close, nothing more is
//! queued to them) and rotates with no grace; the REKEY names the evicted id. Every member
//! that accepts it from its authority blocks that id first, then installs the key and passes
//! the REKEY on, all under one lock: down any chain of password joins, the removed member's
//! link is closing before the key could be queued to it. Whoever cannot prove the new key
//! is then cut off by everyone at the proof deadline.
//!
//! # Closing
//!
//! A local close ([`Net::disconnect`], [`Net::close`], [`Net::block`], shutdown) first writes
//! the control frames queued before it (at most [`FLUSH_TIMEOUT`]; nothing queued after it is
//! sent), then CLOSE (the [`CloseReason`], when there is one) and BYE. The peer reports [`CloseReason::as_str`] as [`Event::Closed`]'s
//! reason ("bye" without one). A host shutting down closes with [`CloseReason::HostLeft`].
//!
//! # Abuse limits
//!
//! * Incoming connections: QUIC Retry (address validation) for every unvalidated source, then
//!   at most [`MAX_PENDING`] unauthenticated connections in total and [`MAX_PENDING_PER_IP`]
//!   per source (IPv4 address / IPv6 /64), refused before any TLS work; the whole handshake
//!   must finish within [`AUTH_TIMEOUT`]. Handshake frames are length-checked per type before
//!   their body is read, and bodies are read as they arrive (no up-front allocation).
//! * Password/PIN guesses: [`auth::Lockout`]. KEY joins are never throttled.
//! * Outward failure signals are only [`CLOSE_AUTH`] (empty reason) and [`CLOSE_RETRY_LATER`]
//!   (reason = seconds, ASCII). Unknown session, missing password and wrong password all run
//!   the same PAKE path (a dummy password when there is no real one) and fail at PAKE3.
//! * Authenticated peers: at most [`CONTROL_FRAMES_PER_SEC`] frames and
//!   [`CONTROL_BYTES_PER_SEC`] per second are read (excess waits, so QUIC flow control throttles
//!   the sender); [`FLOOD_SECONDS`] consecutive seconds over the limit close the connection.
//!   At most [`EVENT_QUEUE_BYTES`] of a connection's control frames wait in the event queue;
//!   beyond that its reader stops. Our own outgoing control queue holds at most
//!   [`CONTROL_QUEUE_BYTES`] per connection; overflowing it closes the connection ("peer not
//!   reading control stream").
//! * Failures of incoming handshakes are reported as counts ([`Event::IncomingFailed`]) at
//!   most once per second.

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc as smpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bytes::Bytes;
use iroh::endpoint::{
    Connection, ConnectionError, Controller, ControllerFactory, Incoming, IncomingAddr,
    QuicTransportConfig, RecvStream, SendDatagramError, SendStream, presets,
};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, RelayUrl, SecretKey, TransportAddr};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::timeout;
use zeroize::Zeroizing;

use crate::auth::{
    self, Exporter, Id, Key, Lockout, PakeAcceptor, PakeConnector, RdvAddr, SecretError, Side,
};
use crate::rdv;
pub use crate::wire::CloseReason;
use crate::wire::{
    self, Format, Frame, Hello, KIND_AUDIO, MAX_ADDRS, MAX_ROSTER, Method, Pake2, PeerEntry, Rekey,
    Role, SessionMode, Welcome,
};

pub const ALPN: &[u8] = b"sonobus/2";
/// UDP port tried first; an ephemeral port is used when it is taken.
pub const DEFAULT_PORT: u16 = 10999;
/// Whole handshake (QUIC incl. Retry + HELLO..WELCOME) must finish within this. That is about
/// five round trips (Retry, QUIC, HELLO/PAKE2, PAKE3/WELCOME): 0.25 s at a 50 ms WAN RTT,
/// 2.5 s on a 500 ms satellite/relayed path, leaving room for one loss recovery there. It also
/// bounds how long an unauthenticated connection holds one of the [`MAX_PENDING`] slots.
pub const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
/// Cap on each graceful-close wait at shutdown.
pub const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);
/// A closing connection writes its queued control frames, CLOSE and BYE within this.
pub const FLUSH_TIMEOUT: Duration = Duration::from_millis(250);
/// After a key change, peers prove the new key within this (or the grace period, if longer).
pub const KEY_PROOF_TIMEOUT: Duration = Duration::from_secs(5);
/// Fixed congestion window. Loss-based control (Cubic) also throttles datagrams and was
/// measured at p50 2.65 s latency under 5 % loss; audio is paced by the codec, not by CC.
pub const CONGESTION_WINDOW: u64 = 1 << 20;
/// Small datagram queue: when the socket stalls, stale audio is dropped (oldest first).
pub const DATAGRAM_SEND_BUFFER: usize = 32 * 1024;
/// Unauthenticated connections held at once, in total...
pub const MAX_PENDING: usize = 64;
/// ...and per source address (IPv4 address / IPv6 /64): a household behind one NAT joining
/// together fits, one host cannot take more than an eighth of the total.
pub const MAX_PENDING_PER_IP: usize = 8;
/// Our queued, unsent control bytes per connection (the peer is not reading beyond this).
pub const CONTROL_QUEUE_BYTES: usize = 1 << 20;
/// A connection's control frames waiting in the event queue (bytes, +64 per frame).
pub const EVENT_QUEUE_BYTES: usize = 256 << 10;
/// Control frames read per second and connection before reading pauses.
pub const CONTROL_FRAMES_PER_SEC: u32 = 100;
pub const CONTROL_BYTES_PER_SEC: usize = 256 << 10;
/// Consecutive seconds over the control rate that close the connection.
pub const FLOOD_SECONDS: u32 = 5;
/// Largest frame body accepted from an unauthenticated peer (any type; HELLO is ≤ 301 B).
const HANDSHAKE_FRAME_MAX: usize = 512;
/// A member we learn of from a ROSTER frame is dialed after this long if nobody connected us
/// by then: the newcomer normally dials first (it has the WELCOME roster); this only closes the
/// gaps when two newcomers join at the same moment and miss each other's WELCOME.
pub const MESH_DIAL_DELAY: Duration = Duration::from_secs(1);
/// How often [`Net::local_addr`] (addresses, home relay) is refreshed.
pub const ADDR_REFRESH: Duration = Duration::from_millis(250);
/// How often a connection's selected path is checked for [`Event::PathChanged`].
pub const PATH_POLL: Duration = Duration::from_millis(250);

/// QUIC application close codes.
pub const CLOSE_BYE: u32 = 0;
/// Authentication failed (empty reason).
pub const CLOSE_AUTH: u32 = 1;
/// Throttled; the reason is the retry delay in whole seconds (ASCII digits).
pub const CLOSE_RETRY_LATER: u32 = 2;
/// Broadcast host at its listener cap (sent to authenticated joiners only).
pub const CLOSE_FULL: u32 = 3;

pub type ConnId = u64;
/// Receives every datagram of one authenticated connection, on the runtime thread. It must not
/// block for long: it stalls all connections. A panic closes the connection
/// (`Closed { reason: "sink panicked" }`).
pub type Sink = Box<dyn FnMut(Bytes) + Send>;
type SinkFactory = Box<dyn FnMut(ConnId, &PeerInfo) -> Sink + Send>;

/// Which relay servers the endpoint uses (they help connect through NATs and forward traffic
/// when no direct path exists).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum RelayChoice {
    /// Direct UDP only.
    None,
    /// iroh's public relays run by n0 (documented as development/test grade, rate limited).
    /// Only the relays: nothing is published to n0's DNS/pkarr address lookup.
    #[default]
    N0Default,
    /// Our own relay servers (`https://...` or, for tests and LANs, `http://...`).
    Custom(Vec<String>),
}

/// The iroh relay mode for `c` (invalid or empty custom URL lists are an error).
pub fn relay_mode(c: &RelayChoice) -> Result<RelayMode, String> {
    Ok(match c {
        RelayChoice::None => RelayMode::Disabled,
        // Not `default_relay_mode()`: that one follows IROH_FORCE_STAGING_RELAYS.
        RelayChoice::N0Default => RelayMode::Default,
        RelayChoice::Custom(urls) if urls.is_empty() => return Err("no relay URL".into()),
        RelayChoice::Custom(urls) => RelayMode::custom(
            urls.iter()
                .map(|u| u.parse::<RelayUrl>().map_err(|e| format!("relay URL {u}: {e}")))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    })
}

/// A new random endpoint secret key (device identity) for [`Config::secret_key`].
pub fn generate_secret_key() -> [u8; 32] {
    auth::random_key()
}

/// Startup configuration. (No `Debug`: it holds secrets. The crate zeroises its own copies of
/// keys and passwords; this struct is the caller's.)
#[derive(Clone)]
pub struct Config {
    pub bind_ip: IpAddr,
    /// 0 = ephemeral.
    pub port: u16,
    pub session: String,
    /// The endpoint secret key: this device's identity (its endpoint id is derived from it).
    /// Persist it ([`generate_secret_key`]); `None` makes a fresh identity on every start, which
    /// changes our id and breaks invite links that name it.
    pub secret_key: Option<[u8; 32]>,
    /// Session key. Generated when absent but a password is set (hosting a PAKE session).
    pub key: Option<Key>,
    /// Accept PAKE joins with this password/PIN (normalised, see [`auth::normalize_secret`]).
    pub password: Option<String>,
    /// PIN semantics: digits only; [`auth::PIN_FAILURES`] completed wrong guesses invalidate it
    /// until [`Net::set_password`].
    pub pin_mode: bool,
    /// Self-declared label sent to peers (unauthenticated).
    pub name: String,
    /// Self-declared role sent to peers (unauthenticated).
    pub role: Role,
    /// The session host's endpoint id as known out of band (from the invite link). Marks that
    /// peer ([`PeerInfo::is_invite_host`]) and is the only peer whose session-key rotation we
    /// accept (besides the endpoint a password join learned the key from).
    pub host: Option<Id>,
    /// [`SessionMode::Broadcast`]: with `role` [`Role::Host`] we are the hub: everyone who
    /// joins is a listener that gets no roster, is never announced to anyone, cannot publish
    /// (its FORMAT and ROSTER frames and AUDIO datagrams are dropped) and counts against
    /// `max_listeners`. With any other role we are a listener: we accept no incoming
    /// connections and dial nobody but whom the app connects to.
    pub mode: SessionMode,
    /// Broadcast host: listeners beyond this many are refused with [`CLOSE_FULL`].
    pub max_listeners: Option<usize>,
    /// Relay servers; the default is n0's public relays.
    pub relay: RelayChoice,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_ip: Ipv4Addr::UNSPECIFIED.into(),
            port: DEFAULT_PORT,
            session: String::new(),
            secret_key: None,
            key: None,
            password: None,
            pin_mode: false,
            name: String::new(),
            role: Role::Peer,
            host: None,
            mode: SessionMode::Mesh,
            max_listeners: None,
            relay: RelayChoice::default(),
        }
    }
}

/// Where to dial an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerAddr {
    pub id: Id,
    pub addrs: Vec<SocketAddr>,
    /// Its home relay URL.
    pub relay: Option<String>,
}

impl PeerAddr {
    pub(crate) fn endpoint_addr(&self) -> Result<EndpointAddr, String> {
        let id = EndpointId::from_bytes(&self.id).map_err(|e| e.to_string())?;
        let relay = self.relay.as_ref().and_then(|r| r.parse::<RelayUrl>().ok());
        Ok(EndpointAddr::from_parts(
            id,
            self.addrs
                .iter()
                .copied()
                .map(TransportAddr::Ip)
                .chain(relay.map(TransportAddr::Relay)),
        ))
    }
}

/// Its addresses as IP literal hosts (nothing to resolve).
impl From<PeerAddr> for RdvAddr {
    fn from(p: PeerAddr) -> RdvAddr {
        RdvAddr {
            id: p.id,
            hosts: p.addrs.iter().map(ToString::to_string).collect(),
            relay: p.relay,
        }
    }
}

impl RdvAddr {
    /// Resolve the hosts into a dialable [`PeerAddr`] (deduplicated, at most
    /// [`MAX_ADDRS`]). Host names take a blocking DNS lookup: call this off the UI thread.
    pub fn resolve(&self) -> Result<PeerAddr, String> {
        use std::net::ToSocketAddrs;
        let mut all = vec![];
        for h in &self.hosts {
            all.extend(h.to_socket_addrs().map_err(|e| format!("{h}: {e}"))?);
        }
        self.with_addrs(all)
    }

    /// [`resolve`](Self::resolve) on the runtime (IP literals never leave it).
    async fn resolve_async(&self) -> Result<PeerAddr, String> {
        let mut all = vec![];
        for h in &self.hosts {
            let found = tokio::net::lookup_host(h.as_str()).await;
            all.extend(found.map_err(|e| format!("{h}: {e}"))?);
        }
        self.with_addrs(all)
    }

    fn with_addrs(&self, all: Vec<SocketAddr>) -> Result<PeerAddr, String> {
        let mut addrs = Vec::new();
        for a in all {
            if !addrs.contains(&a) {
                addrs.push(a);
            }
        }
        addrs.truncate(MAX_ADDRS);
        if addrs.is_empty() && self.relay.is_none() {
            return Err("no address".into());
        }
        Ok(PeerAddr {
            id: self.id,
            addrs,
            relay: self.relay.clone(),
        })
    }
}

/// How to prove membership when dialing.
pub enum Join {
    /// With the session key we hold.
    Key,
    /// With the session password (learns the key).
    Password(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerInfo {
    /// The peer's endpoint id, proven by TLS: the one identity that can be trusted.
    pub id: Id,
    /// Self-declared, unauthenticated label.
    pub name: String,
    /// Self-declared, unauthenticated role: do not grant anything by it.
    pub role: Role,
    /// Where the peer says it can be dialed, plus the address we saw it at (incoming).
    pub addrs: Vec<SocketAddr>,
    /// The peer's home relay URL, as it says.
    pub relay: Option<String>,
    /// The peer dialed us.
    pub incoming: bool,
    /// `id` equals [`Config::host`] (the host named by our invite link): verifiably the host.
    pub is_invite_host: bool,
}

pub enum Event {
    /// Authenticated both ways. `roster`/`formats` are the acceptor's WELCOME contents (the
    /// acceptor lists itself first); empty on the accepting side, where `peer` carries the
    /// connector's HELLO information.
    Connected {
        conn: ConnId,
        peer: PeerInfo,
        tx: PeerTx,
        roster: Vec<PeerEntry>,
        formats: Vec<Format>,
    },
    /// One of our dials failed. `retry_after` is set when the acceptor throttled us.
    ConnectFailed {
        conn: ConnId,
        reason: String,
        retry_after: Option<Duration>,
    },
    /// Incoming connections refused or failed since the last report (at most one per second).
    IncomingFailed { count: u32, last_reason: String },
    /// A control frame from an authenticated peer (never HELLO/PAKE*/WELCOME/BYE/REKEY).
    Control { conn: ConnId, frame: Frame },
    /// An authenticated connection ended. When the peer closed it giving a reason, `reason`
    /// is [`CloseReason::as_str`] ("removed", "blocked", "host left", "session full"); a
    /// plain BYE is "bye"; local closes and errors have their own text.
    Closed { conn: ConnId, reason: String },
    /// The connection's selected path changed (relayed ↔ direct, or another address).
    PathChanged { conn: ConnId, path: PathInfo },
    /// We have a new session key (learned by a password join, rotated by the host or by
    /// [`Net::rotate_session_key`]); it is now used for KEY proofs. Persist it if needed.
    SessionKey(Key),
    /// A password join attempt on our session failed; failed (or unfinished) attempts so far.
    JoinFailed { failed_total: u32 },
    /// The rendezvous server could not be reached or dropped us; retrying after `retry_after`.
    RendezvousFailed {
        reason: String,
        retry_after: Duration,
    },
    /// PIN mode: too many completed wrong guesses; PIN joins fail until a new PIN is set.
    PinInvalidated,
    /// Password joins are being refused for a while: from `ip` (IPv4 / IPv6 /64), or from
    /// everyone when `ip` is `None` (global budget). Reported once per throttling episode.
    Throttled {
        ip: Option<IpAddr>,
        retry_after: Duration,
    },
    /// Our home relay URL ([`Net::local_addr`]): sent once when it first becomes known and
    /// again whenever it changes.
    LocalRelay(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendError {
    TooLarge,
    Closed,
}

/// Why [`Net::send_control`] did not queue a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlError {
    /// No authenticated connection with this id (not yet connected, or closed).
    NotConnected,
    /// Handshake/BYE/REKEY/unknown frames are the runtime's own.
    NotAllowed,
    /// The frame does not encode (a field over its cap); the connection is untouched.
    Invalid(wire::Error),
    /// Our queue for this peer is full: it is not reading. The connection is being closed.
    Overflow,
}

/// Snapshot of the connection's selected path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathInfo {
    /// Direct UDP (as opposed to relayed).
    pub direct: bool,
    pub remote: Option<SocketAddr>,
    /// The relay forwarding this path (when not direct).
    pub relay: Option<String>,
    pub rtt: Duration,
    pub congestion_events: u64,
}

/// Datagram sender for one authenticated peer. Cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct PeerTx {
    conn: Connection,
}

impl PeerTx {
    /// Queue one datagram (at most [`wire::MAX_DATAGRAM`] bytes). Never waits for the network:
    /// iroh appends it to a bounded queue ([`DATAGRAM_SEND_BUFFER`], oldest dropped when full)
    /// and wakes the runtime thread. It does take iroh's per-connection mutex, which the
    /// runtime thread also holds briefly, and the wake-up is a syscall, so this is NOT
    /// real-time safe: call it from a send thread, never from the audio callback.
    /// Fails with [`SendError::Closed`] once the connection is closed.
    pub fn send(&self, datagram: Bytes) -> Result<(), SendError> {
        if datagram.len() > wire::MAX_DATAGRAM {
            return Err(SendError::TooLarge);
        }
        self.conn.send_datagram(datagram).map_err(|e| match e {
            SendDatagramError::TooLarge => SendError::TooLarge,
            _ => SendError::Closed,
        })
    }

    /// Free bytes in the datagram send queue.
    pub fn send_buffer_space(&self) -> usize {
        self.conn.datagram_send_buffer_space()
    }

    pub fn peer_id(&self) -> Id {
        *self.conn.remote_id().as_bytes()
    }

    pub fn path(&self) -> Option<PathInfo> {
        selected_path(&self.conn)
    }
}

fn selected_path(conn: &Connection) -> Option<PathInfo> {
    let paths = conn.paths();
    let p = paths.iter().find(|p| p.is_selected())?;
    let (remote, relay) = match p.remote_addr() {
        TransportAddr::Ip(a) => (Some(*a), None),
        TransportAddr::Relay(u) => (None, Some(u.to_string())),
        _ => (None, None),
    };
    Some(PathInfo {
        direct: p.is_ip(),
        remote,
        relay,
        rtt: p.rtt(),
        congestion_events: p.stats().congestion_events,
    })
}

/// The event queue. Control frames hold part of their connection's [`EVENT_QUEUE_BYTES`]
/// budget until received here, so a slow reader throttles chatty peers instead of growing.
pub struct Events(smpsc::Receiver<(Event, Option<OwnedSemaphorePermit>)>);

impl Events {
    pub fn recv(&self) -> Result<Event, smpsc::RecvError> {
        self.0.recv().map(|(e, _)| e)
    }
    pub fn recv_timeout(&self, d: Duration) -> Result<Event, smpsc::RecvTimeoutError> {
        self.0.recv_timeout(d).map(|(e, _)| e)
    }
    pub fn try_recv(&self) -> Result<Event, smpsc::TryRecvError> {
        self.0.try_recv().map(|(e, _)| e)
    }
}

#[derive(Clone)]
struct EventTx(smpsc::Sender<(Event, Option<OwnedSemaphorePermit>)>);

impl EventTx {
    fn send(&self, e: Event) {
        let _ = self.0.send((e, None));
    }
}

enum Cmd {
    Connect(ConnId, PeerAddr, Join, LinkRx),
    /// Mesh: dial a roster member with the session key unless already connected/dialing.
    Dial(PeerAddr),
    SetPassword(Option<Zeroizing<String>>),
    SetPin(Zeroizing<String>),
    SetSession(String),
    /// Password for accepting PAKE joins once we hold the session key (no key generated).
    SetJoinPassword(Zeroizing<String>),
    DialPassword(PeerAddr, Zeroizing<String>),
    ServeRendezvous(rdv::Limits),
    Rendezvous(RdvAddr, Option<Zeroizing<String>>),
    StopRendezvous,
    SetFormats(Vec<Format>),
    /// New key, grace, the member it evicts.
    Rekey(Zeroizing<Key>, Duration, Option<Id>),
    Shutdown,
    /// Test hook: stop without closing anything (simulates a crash).
    Abort,
}

/// Encoded control bytes holding their share of the connection's queue budget.
struct Out {
    bytes: Vec<u8>,
    _budget: OwnedSemaphorePermit,
}

/// Why a connection is being closed locally: the local reason and what the peer is told.
type Stop = Option<(String, Option<CloseReason>)>;

/// One connection's handles, shared by [`Net`] (app thread) and the runtime.
struct Link {
    out: mpsc::UnboundedSender<Out>,
    budget: Arc<Semaphore>,
    stop: Arc<watch::Sender<Stop>>,
    /// The peer's endpoint id, once authenticated.
    peer: Option<Id>,
}

struct LinkRx {
    out: mpsc::UnboundedReceiver<Out>,
    stop: watch::Receiver<Stop>,
}

impl Link {
    fn new() -> (Link, LinkRx) {
        let (out, out_rx) = mpsc::unbounded_channel();
        let (stop, stop_rx) = watch::channel(None);
        (
            Link {
                out,
                budget: Arc::new(Semaphore::new(CONTROL_QUEUE_BYTES)),
                stop: Arc::new(stop),
                peer: None,
            },
            LinkRx {
                out: out_rx,
                stop: stop_rx,
            },
        )
    }

    /// End the connection (the first reason wins).
    fn stop(&self, reason: &str) {
        self.stop_with(reason, None);
    }

    fn stop_with(&self, reason: &str, code: Option<CloseReason>) {
        self.stop.send_if_modified(|s| {
            s.is_none() && {
                *s = Some((reason.into(), code));
                true
            }
        });
    }

    /// Queue control bytes. A closing link takes nothing more: its writer flushes only what
    /// was queued before the stop. (`stop` is only ever set with the links lock held, which
    /// every caller of this holds too.)
    fn queue(&self, bytes: Vec<u8>) -> Result<(), ControlError> {
        if self.stop.borrow().is_some() {
            return Err(ControlError::NotConnected);
        }
        let n = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        let Ok(p) = self.budget.clone().try_acquire_many_owned(n) else {
            self.stop("peer not reading control stream");
            return Err(ControlError::Overflow);
        };
        self.out
            .send(Out { bytes, _budget: p })
            .map_err(|_| ControlError::NotConnected)
    }
}

type Links = Arc<Mutex<HashMap<ConnId, Link>>>;

/// Refuse endpoint `id` from now on and close its links (`why` locally, `code` to the peer;
/// a link already closing keeps its reason). Takes the links lock, then `refused`.
fn refuse(links: &Links, refused: &Mutex<HashSet<Id>>, id: Id, why: &str, code: CloseReason) {
    let links = lock(links);
    lock(refused).insert(id);
    for l in links.values().filter(|l| l.peer == Some(id)) {
        l.stop_with(why, Some(code));
    }
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Handle to the network thread. Dropping it says BYE to every peer, closes the endpoint
/// (each wait capped at [`CLOSE_TIMEOUT`]) and joins the thread.
pub struct Net {
    cmd: mpsc::UnboundedSender<Cmd>,
    events: Events,
    local: Arc<Mutex<PeerAddr>>,
    ids: Arc<AtomicU64>,
    links: Links,
    pin_mode: AtomicBool,
    authority: Arc<Mutex<Option<Id>>>,
    /// Blocked endpoint ids ([`Net::block`]). Locked inside the links lock, never the reverse.
    refused: Arc<Mutex<HashSet<Id>>>,
    thread: Option<JoinHandle<()>>,
}

fn normalize(pw: &str, pin_mode: bool) -> Result<Zeroizing<String>, SecretError> {
    if pin_mode {
        auth::normalize_pin(pw)
    } else {
        auth::normalize_secret(pw)
    }
}

impl Net {
    /// Bind the endpoint and start the `sb-net` thread. `sinks` is called on the runtime
    /// thread once per authenticated connection to create its datagram sink.
    pub fn start(
        mut cfg: Config,
        sinks: impl FnMut(ConnId, &PeerInfo) -> Sink + Send + 'static,
    ) -> std::io::Result<Net> {
        let password = cfg
            .password
            .take()
            .map(|p| normalize(&p, cfg.pin_mode))
            .transpose()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let pin_mode = cfg.pin_mode;
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let (ev_tx, ev_rx) = smpsc::channel();
        let (ready_tx, ready_rx) = smpsc::channel();
        let ids = Arc::new(AtomicU64::new(1));
        let links: Links = Arc::default();
        let authority: Arc<Mutex<Option<Id>>> = Arc::default();
        let refused: Arc<Mutex<HashSet<Id>>> = Arc::default();
        let local = Arc::new(Mutex::new(PeerAddr {
            id: [0; 32],
            addrs: vec![],
            relay: None,
        }));
        let rt_args = Runtime {
            local: local.clone(),
            cfg,
            password,
            sinks: Box::new(sinks),
            cmd: cmd_tx.downgrade(),
            events: EventTx(ev_tx),
            links: links.clone(),
            ids: ids.clone(),
            authority: authority.clone(),
            refused: refused.clone(),
        };
        let thread = std::thread::Builder::new()
            .name("sb-net".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => return drop(ready_tx.send(Err(e.to_string()))),
                };
                rt.block_on(run(rt_args, cmd_rx, ready_tx));
                rt.shutdown_timeout(CLOSE_TIMEOUT);
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Net {
                cmd: cmd_tx,
                events: Events(ev_rx),
                local,
                ids,
                links,
                pin_mode: AtomicBool::new(pin_mode),
                authority,
                refused,
                thread: Some(thread),
            }),
            r => {
                let _ = thread.join();
                let msg = r.map_or_else(|_| "network thread died".into(), |r| r.unwrap_err());
                Err(std::io::Error::other(msg))
            }
        }
    }

    /// Our endpoint id, advertised addresses (interface addresses; never `0.0.0.0`/`::`,
    /// loopback only when bound to loopback) and home relay URL, refreshed every
    /// [`ADDR_REFRESH`]. The relay URL appears once the relay connection is up (shortly after
    /// start); put this in invite links.
    pub fn local_addr(&self) -> PeerAddr {
        lock(&self.local).clone()
    }

    pub fn events(&self) -> &Events {
        &self.events
    }

    /// Dial `to`; the outcome arrives as [`Event::Connected`] or [`Event::ConnectFailed`].
    pub fn connect(&self, to: PeerAddr, join: Join) -> ConnId {
        let id = self.ids.fetch_add(1, Ordering::Relaxed);
        let (link, rx) = Link::new();
        lock(&self.links).insert(id, link);
        let _ = self.cmd.send(Cmd::Connect(id, to, join, rx));
        id
    }

    /// Queue a control frame to an authenticated connection. Only ROSTER, FORMAT, SUBSCRIBE,
    /// QUALITY_REQ and APP_OSC are allowed. Frames are validated here: one that does not
    /// encode is refused without touching the connection. A roster longer than
    /// [`wire::MAX_ROSTER`] goes out as several consecutive ROSTER frames (receivers must treat
    /// ROSTER as additive).
    pub fn send_control(&self, conn: ConnId, frame: Frame) -> Result<(), ControlError> {
        let mut bytes = Vec::new();
        match &frame {
            Frame::Roster(r) if r.len() > MAX_ROSTER => {
                for part in r.chunks(MAX_ROSTER) {
                    Frame::Roster(part.to_vec())
                        .encode(&mut bytes)
                        .map_err(ControlError::Invalid)?;
                }
            }
            Frame::Roster(_)
            | Frame::Format(_)
            | Frame::Subscribe(_)
            | Frame::QualityReq(_)
            | Frame::AppOsc(_) => frame.encode(&mut bytes).map_err(ControlError::Invalid)?,
            _ => return Err(ControlError::NotAllowed),
        }
        let links = lock(&self.links);
        let l = links
            .get(&conn)
            .filter(|l| l.peer.is_some())
            .ok_or(ControlError::NotConnected)?;
        l.queue(bytes)
    }

    /// Say BYE and close one connection (or cancel a dial). Control frames already queued
    /// are written first; bounded in time even when the peer stops reading (queued frames and
    /// BYE get [`FLUSH_TIMEOUT`] together).
    pub fn disconnect(&self, conn: ConnId) {
        if let Some(l) = lock(&self.links).get(&conn) {
            l.stop("closed locally");
        }
    }

    /// [`disconnect`](Self::disconnect), telling the peer why: its [`Event::Closed`] reason is
    /// `reason.as_str()`.
    pub fn close(&self, conn: ConnId, reason: CloseReason) {
        if let Some(l) = lock(&self.links).get(&conn) {
            l.stop_with("closed locally", Some(reason));
        }
    }

    /// Refuse endpoint `id` while this `Net` runs (until [`unblock`](Self::unblock)): its
    /// connections close now ([`CloseReason::Blocked`], unless already closing with another
    /// reason: close it with its own reason first), its handshakes fail before any key material
    /// is sent, and nothing more is queued to it (REKEY included). Also covers a reconnect that
    /// races a key rotation.
    pub fn block(&self, id: Id) {
        refuse(&self.links, &self.refused, id, "blocked", CloseReason::Blocked);
    }

    pub fn unblock(&self, id: &Id) {
        lock(&self.refused).remove(id);
    }

    /// Whose session-key rotations we accept: the host named by our invite link, or the
    /// endpoint a password join (LAN, rendezvous) learned the key from. `None` for the host.
    pub fn authority(&self) -> Option<Id> {
        *lock(&self.authority)
    }

    /// Change the password/PIN accepted for PAKE joins (normalised; in PIN mode it must be
    /// digits). Attempts already in flight fail; PIN invalidation is cleared.
    ///
    /// This stops NEW password joins only: everyone who ever joined holds the session key and
    /// can still join by KEY proof. To lock someone out, disconnect them and
    /// [`rotate_session_key`](Self::rotate_session_key).
    pub fn set_password(&self, password: Option<String>) -> Result<(), SecretError> {
        let pin = self.pin_mode.load(Ordering::Relaxed);
        let p = password.map(|p| normalize(&p, pin)).transpose()?;
        let _ = self.cmd.send(Cmd::SetPassword(p));
        Ok(())
    }

    /// Host: switch to PIN joins with a new random 6-digit PIN (returned, for display). PIN
    /// semantics from now on: [`auth::PIN_FAILURES`] completed wrong guesses invalidate it
    /// ([`Event::PinInvalidated`]); call this again for a fresh PIN. Not done automatically:
    /// a PIN regenerated on every invalidation would give an attacker unlimited guesses.
    pub fn enable_pin(&self) -> String {
        let pin = auth::random_pin();
        self.pin_mode.store(true, Ordering::Relaxed);
        let _ = self.cmd.send(Cmd::SetPin(Zeroizing::new(pin.clone())));
        pin
    }

    /// Join a session found on the LAN with its PIN or password. This endpoint takes the
    /// discovered session's name (for this and later joins); the outcome arrives as for
    /// [`connect`](Self::connect).
    pub fn join_discovered(&self, s: &crate::lan::DiscoveredSession, secret: &str) -> ConnId {
        let _ = self.cmd.send(Cmd::SetSession(s.session.clone()));
        self.connect(s.host.clone(), Join::Password(secret.into()))
    }

    /// Serve rendezvous ([`rdv::RDV_ALPN`]) on this endpoint, for name + password sessions on
    /// a LAN with one device hosting (or a small private server), and register ourselves
    /// there under the current session name so joiners find us. See [`rdv`] for what a
    /// rendezvous server learns.
    pub fn serve_rendezvous(&self, limits: rdv::Limits) {
        let _ = self.cmd.send(Cmd::ServeRendezvous(limits));
    }

    /// Stay registered at rendezvous server `server` under our session name (a host with a
    /// password, or any member) so name + password joiners find us; reconnects with back-off
    /// ([`Event::RendezvousFailed`]). Replaces an earlier registration. `server` is an
    /// [`RdvAddr`] (host names are resolved on the network thread, on every reconnect) or a
    /// [`PeerAddr`].
    pub fn register_by_name(&self, server: impl Into<RdvAddr>) {
        let _ = self.cmd.send(Cmd::Rendezvous(server.into(), None));
    }

    /// Leave the rendezvous server ([`register_by_name`](Self::register_by_name) /
    /// [`join_by_name`](Self::join_by_name)): the connection closes, so the server unlists us.
    /// Session connections stay. No-op when not registered.
    pub fn stop_rendezvous(&self) {
        let _ = self.cmd.send(Cmd::StopRendezvous);
    }

    /// Join session `name` with `password` through rendezvous server `server`: register under
    /// the name, dial a listed member (oldest first) with the password, then the roster mesh
    /// (outcomes as [`Event::Connected`] / [`Event::ConnectFailed`]). The registration stays
    /// (later joiners find us), and once we hold the session key we accept password joins
    /// with the same password. This endpoint takes the session's name. `server` as for
    /// [`register_by_name`](Self::register_by_name).
    pub fn join_by_name(
        &self,
        server: impl Into<RdvAddr>,
        name: &str,
        password: &str,
    ) -> Result<(), SecretError> {
        let pw = auth::normalize_secret(password)?;
        let _ = self.cmd.send(Cmd::SetSession(name.into()));
        let _ = self.cmd.send(Cmd::SetJoinPassword(pw.clone()));
        let _ = self.cmd.send(Cmd::Rendezvous(server.into(), Some(pw)));
        Ok(())
    }

    /// Stream formats sent in WELCOME to everyone who joins through us.
    pub fn set_formats(&self, formats: Vec<Format>) -> Result<(), wire::Error> {
        Frame::Welcome(Welcome {
            mac: [0; 32],
            key: None,
            roster: vec![],
            formats: formats.clone(),
        })
        .encode(&mut Vec::new())?;
        let _ = self.cmd.send(Cmd::SetFormats(formats));
        Ok(())
    }

    /// Host: replace the session key with `key_from_seed(seed)` for a new random seed (returned,
    /// for minting new invite links) and send it to every currently authenticated peer, which
    /// switches to it for future KEY proofs. Peers only accept it from the host they joined
    /// through ([`Config::host`] or the password join's acceptor). The old key stays valid for
    /// KEY proofs for `grace` (use zero when removing a member: anyone who connects with the old
    /// key during the grace period stays connected). Invite links minted from the old seed stop
    /// working once the grace period ends. Close members to be removed first: whoever is
    /// connected when this runs receives the new key, except over links already closing
    /// ([`close`](Self::close), [`disconnect`](Self::disconnect), [`block`](Self::block)).
    /// [`block`](Self::block) also refuses a reconnect racing the rotation. To remove a
    /// member, use [`evict`](Self::evict).
    pub fn rotate_session_key(&self, grace: Duration) -> [u8; 16] {
        self.rekey(grace, None)
    }

    /// Host: remove member `id` from the session. It is blocked here (its connections close
    /// with [`CloseReason::Removed`] unless already closing with another reason) and the key
    /// is rotated with no grace period, the REKEY naming `id`: every member that accepts it
    /// blocks `id` before it installs the key or passes it on to its password joiners, so the
    /// new key never reaches `id`, however long the chain of password joins. Returns the new
    /// seed, as [`rotate_session_key`](Self::rotate_session_key).
    pub fn evict(&self, id: Id) -> [u8; 16] {
        self.rekey(Duration::ZERO, Some(id))
    }

    fn rekey(&self, grace: Duration, evict: Option<Id>) -> [u8; 16] {
        let seed = auth::random_seed();
        let k = Zeroizing::new(auth::key_from_seed(&seed));
        let _ = self.cmd.send(Cmd::Rekey(k, grace, evict));
        seed
    }

    /// Stop abruptly without BYE or QUIC close, as if the process crashed (test hook).
    #[doc(hidden)]
    pub fn kill(mut self) {
        let _ = self.cmd.send(Cmd::Abort);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Net {
    fn drop(&mut self) {
        let _ = self.cmd.send(Cmd::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Runtime thread

#[derive(Debug)]
struct FixedWindow;

impl Controller for FixedWindow {
    fn on_congestion_event(&mut self, _: Instant, _: Instant, _: bool, _: bool, _: u64, _: u64) {}
    fn on_mtu_update(&mut self, _: u16) {}
    fn window(&self) -> u64 {
        CONGESTION_WINDOW
    }
    fn clone_box(&self) -> Box<dyn Controller> {
        Box::new(FixedWindow)
    }
    fn initial_window(&self) -> u64 {
        CONGESTION_WINDOW
    }
    fn into_any(self: Box<Self>) -> Box<dyn std::any::Any> {
        self
    }
}

impl ControllerFactory for FixedWindow {
    fn build(self: Arc<Self>, _: Instant, _: u16) -> Box<dyn Controller> {
        Box::new(FixedWindow)
    }
}

struct Member {
    conn: ConnId,
    outgoing: bool,
    entry: PeerEntry,
}

struct Shared {
    tag: [u8; 16],
    /// Our roster entry (addresses refreshed on use).
    me: PeerEntry,
    bind_ip: IpAddr,
    host: Option<Id>,
    mode: SessionMode,
    max_listeners: usize,
    key: Option<Zeroizing<Key>>,
    /// Previous key and until when it is still accepted.
    old_key: Option<(Zeroizing<Key>, Instant)>,
    /// Bumped on every key change; connections then exchange KEY_PROOF. With the grace period
    /// of the change.
    key_epoch: watch::Sender<(u64, Duration)>,
    /// Whose REKEY we accept ([`Net::authority`]).
    authority: Arc<Mutex<Option<Id>>>,
    /// [`Net::block`]ed ids.
    refused: Arc<Mutex<HashSet<Id>>>,
    /// Endpoints that learned the key from us by password: we are their key authority and
    /// pass rotations on to them.
    delegated: HashSet<Id>,
    /// The member the current key's rotation evicted (carried by every REKEY of this key).
    evicted: Option<Id>,
    password: Option<Zeroizing<String>>,
    lockout: Lockout,
    formats: Vec<Format>,
    /// Authenticated peers by endpoint id.
    members: HashMap<Id, Member>,
    /// Endpoint ids with a dial in progress.
    dialing: HashSet<Id>,
    /// Endpoint ids learned from ROSTER frames, waiting [`MESH_DIAL_DELAY`] (at most
    /// [`MAX_ROSTER`]).
    scheduled: HashSet<Id>,
    pending: usize,
    pending_ip: HashMap<IpAddr, usize>,
    /// Incoming failures not yet reported: count, last reason.
    incoming_failed: (u32, String),
    sinks: SinkFactory,
    events: EventTx,
    links: Links,
    cmd: mpsc::WeakUnboundedSender<Cmd>,
    /// Embedded rendezvous server ([`Net::serve_rendezvous`]).
    rdv: Option<rdv::StateRef>,
}

impl Shared {
    /// Broadcast hub: peers are listeners.
    fn hub(&self) -> bool {
        self.mode == SessionMode::Broadcast && self.me.role == Role::Host
    }

    /// Broadcast listener: talks to the host only.
    fn listener(&self) -> bool {
        self.mode == SessionMode::Broadcast && self.me.role != Role::Host
    }

    /// Hub at its listener cap (`peer` not counted when already a member).
    fn full(&self, peer: &Id) -> bool {
        self.hub() && !self.members.contains_key(peer) && self.members.len() >= self.max_listeners
    }

    fn incoming_failed(&mut self, reason: String) {
        self.incoming_failed.0 += 1;
        self.incoming_failed.1 = reason;
    }

    /// Keys valid for KEY proofs now: (current, unexpired previous).
    fn keys(&self) -> (Option<Zeroizing<Key>>, Option<Zeroizing<Key>>) {
        let old = self
            .old_key
            .as_ref()
            .filter(|(_, until)| Instant::now() < *until)
            .map(|(k, _)| k.clone());
        (self.key.clone(), old)
    }

    /// Switch to key `k`, rotated with `grace` and evicting `evicted`. Block the evicted
    /// member first: the caller passes the REKEY on right after.
    fn install_key(&mut self, k: Zeroizing<Key>, grace: Duration, evicted: Option<Id>) {
        if let Some(id) = evicted.filter(|id| *id != self.me.id) {
            refuse(&self.links, &self.refused, id, "evicted", CloseReason::Removed);
        }
        self.evicted = evicted;
        let old = self.key.replace(k.clone());
        self.old_key = old.map(|o| (o, Instant::now() + grace));
        self.key_epoch.send_modify(|e| *e = (e.0 + 1, grace));
        self.events.send(Event::SessionKey(*k));
    }

    /// Queue REKEY (the current key, grace and eviction) to the members `to` selects. Links
    /// that are closing take nothing ([`Link::queue`]).
    fn send_rekey(&self, to: impl Fn(&Member) -> bool) {
        let Some(k) = &self.key else { return };
        let grace_ms = u32::try_from(self.key_epoch.borrow().1.as_millis()).unwrap_or(u32::MAX);
        let mut b = Vec::new();
        let r = Rekey {
            key: **k,
            grace_ms,
            evicted: self.evicted,
        };
        let _ = Frame::Rekey(r).encode(&mut b);
        let b = Zeroizing::new(b);
        let links = lock(&self.links);
        for m in self.members.values().filter(|m| to(m)) {
            if let Some(l) = links.get(&m.conn).filter(|l| l.peer.is_some()) {
                let _ = l.queue(b.to_vec());
            }
        }
    }
}

type SharedRef = Arc<Mutex<Shared>>;

/// What `Net::start` hands the runtime thread.
struct Runtime {
    local: Arc<Mutex<PeerAddr>>,
    cfg: Config,
    password: Option<Zeroizing<String>>,
    sinks: SinkFactory,
    cmd: mpsc::WeakUnboundedSender<Cmd>,
    events: EventTx,
    links: Links,
    ids: Arc<AtomicU64>,
    authority: Arc<Mutex<Option<Id>>>,
    refused: Arc<Mutex<HashSet<Id>>>,
}

/// Bind `ip:port`; when that fails, an ephemeral port unless `port_required`.
pub(crate) async fn bind(
    ip: IpAddr,
    port: u16,
    port_required: bool,
    secret: Option<[u8; 32]>,
    relay: RelayMode,
    alpn: &[u8],
) -> Result<Endpoint, String> {
    let tc = QuicTransportConfig::builder()
        .congestion_controller_factory(Arc::new(FixedWindow))
        .datagram_send_buffer_size(DATAGRAM_SEND_BUFFER)
        .max_concurrent_bidi_streams(1u32.into())
        .max_concurrent_uni_streams(0u32.into())
        .build();
    let mut err = String::new();
    let ports = match port {
        0 => vec![0],
        _ if port_required => vec![port],
        _ => vec![port, 0],
    };
    for p in ports {
        // Minimal: no address lookup (unlike `presets::N0`, which adds n0's pkarr publisher and
        // resolver and DNS lookup on top of the same relays).
        let mut b = Endpoint::builder(presets::Minimal)
            .relay_mode(relay.clone())
            .clear_ip_transports()
            .bind_addr(SocketAddr::new(ip, p))
            .map_err(|e| e.to_string())?
            .alpns(vec![alpn.to_vec()])
            .transport_config(tc.clone());
        if let Some(k) = &secret {
            b = b.secret_key(SecretKey::from_bytes(k));
        }
        match b.bind().await {
            Ok(ep) => return Ok(ep),
            Err(e) if port_required => {
                return Err(format!("cannot bind UDP {}: {}", SocketAddr::new(ip, p), chain(&e)));
            }
            Err(e) => err = e.to_string(),
        }
    }
    Err(err)
}

/// Addresses to advertise: interface addresses of the bound socket(s), never unspecified,
/// loopback only when bound to loopback.
fn advertised(ep: &Endpoint, bind_ip: IpAddr) -> Vec<SocketAddr> {
    let ok = |a: &SocketAddr| {
        !a.ip().is_unspecified() && (bind_ip.is_loopback() || !a.ip().is_loopback())
    };
    let mut v: Vec<SocketAddr> = ep.addr().ip_addrs().copied().filter(ok).collect();
    if v.is_empty() {
        v = ep.bound_sockets().into_iter().filter(ok).collect();
    }
    v.truncate(MAX_ADDRS);
    v
}

/// How [`Net::local_addr`] reports itself.
pub(crate) fn local_peer(ep: &Endpoint, bind_ip: IpAddr) -> PeerAddr {
    PeerAddr {
        id: *ep.id().as_bytes(),
        addrs: advertised(ep, bind_ip),
        relay: home_relay(ep),
    }
}

fn home_relay(ep: &Endpoint) -> Option<String> {
    ep.addr().relay_urls().next().map(|u| u.to_string())
}

async fn run(
    args: Runtime,
    mut cmds: mpsc::UnboundedReceiver<Cmd>,
    ready: smpsc::Sender<Result<(), String>>,
) {
    let Runtime {
        local: local_out,
        cfg,
        password,
        sinks,
        cmd,
        events,
        links,
        ids,
        authority,
        refused,
    } = args;
    let bound = match relay_mode(&cfg.relay) {
        Ok(relay) => bind(cfg.bind_ip, cfg.port, false, cfg.secret_key, relay, ALPN).await,
        Err(e) => Err(e),
    };
    let ep = match bound {
        Ok(ep) => ep,
        Err(e) => return drop(ready.send(Err(e))),
    };
    let local = local_peer(&ep, cfg.bind_ip);
    *lock(&local_out) = local.clone();
    let key = cfg
        .key
        .or(password.as_ref().map(|_| auth::random_key()))
        .map(Zeroizing::new);
    let shared = Arc::new(Mutex::new(Shared {
        tag: auth::session_tag(&cfg.session),
        me: PeerEntry {
            id: local.id,
            addrs: local.addrs.clone(),
            relay: local.relay.clone(),
            name: cfg.name.clone(),
            role: cfg.role,
        },
        bind_ip: cfg.bind_ip,
        host: cfg.host,
        mode: cfg.mode,
        max_listeners: cfg.max_listeners.unwrap_or(usize::MAX),
        key,
        old_key: None,
        key_epoch: watch::channel((0, Duration::ZERO)).0,
        authority: {
            *lock(&authority) = cfg.host.filter(|h| *h != local.id);
            authority
        },
        refused,
        delegated: HashSet::new(),
        evicted: None,
        password,
        lockout: Lockout::new(cfg.pin_mode),
        formats: vec![],
        members: HashMap::new(),
        dialing: HashSet::new(),
        scheduled: HashSet::new(),
        pending: 0,
        pending_ip: HashMap::new(),
        incoming_failed: (0, String::new()),
        sinks,
        events: events.clone(),
        links: links.clone(),
        cmd,
        rdv: None,
    }));
    let bind_ip = cfg.bind_ip;
    drop(cfg);
    if let Some(r) = &local.relay {
        events.send(Event::LocalRelay(r.clone()));
    }
    let _ = ready.send(Ok(()));

    let mut tasks = JoinSet::new();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut rdv_stop: Option<watch::Sender<()>> = None;
    let mut addr_tick = tokio::time::interval(ADDR_REFRESH);
    addr_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let dial = |tasks: &mut JoinSet<()>, id, to: PeerAddr, join, rx| {
        lock(&shared).dialing.insert(to.id);
        tasks.spawn(conn_task(
            ep.clone(),
            shared.clone(),
            id,
            Dir::Out(to, join),
            None,
            rx,
        ));
    };
    loop {
        tokio::select! {
            cmd = cmds.recv() => match cmd {
                None | Some(Cmd::Shutdown) => break,
                Some(Cmd::Abort) => return,
                Some(Cmd::Connect(id, to, join, rx)) => dial(&mut tasks, id, to, join, rx),
                Some(Cmd::Dial(to)) => {
                    let known = {
                        let s = lock(&shared);
                        s.members.contains_key(&to.id)
                            || s.dialing.contains(&to.id)
                            || lock(&s.refused).contains(&to.id)
                    };
                    if !known {
                        let id = ids.fetch_add(1, Ordering::Relaxed);
                        let (link, rx) = Link::new();
                        lock(&links).insert(id, link);
                        dial(&mut tasks, id, to, Join::Key, rx);
                    }
                }
                Some(Cmd::SetPassword(p)) => {
                    let mut s = lock(&shared);
                    if s.key.is_none() && p.is_some() {
                        s.key = Some(Zeroizing::new(auth::random_key()));
                    }
                    s.password = p;
                    s.lockout.new_secret();
                }
                Some(Cmd::SetPin(pin)) => {
                    let mut s = lock(&shared);
                    if s.key.is_none() {
                        s.key = Some(Zeroizing::new(auth::random_key()));
                    }
                    s.password = Some(pin);
                    s.lockout.set_pin_mode(true);
                    s.lockout.new_secret();
                }
                Some(Cmd::SetSession(name)) => lock(&shared).tag = auth::session_tag(&name),
                Some(Cmd::SetJoinPassword(pw)) => {
                    let mut s = lock(&shared);
                    s.password = Some(pw);
                    s.lockout.new_secret();
                }
                Some(Cmd::DialPassword(to, pw)) => {
                    let known = {
                        let s = lock(&shared);
                        s.members.contains_key(&to.id) || s.dialing.contains(&to.id)
                    };
                    if !known {
                        let id = ids.fetch_add(1, Ordering::Relaxed);
                        let (link, rx) = Link::new();
                        lock(&links).insert(id, link);
                        dial(&mut tasks, id, to, Join::Password(pw.to_string()), rx);
                    }
                }
                Some(Cmd::ServeRendezvous(limits)) => {
                    let st = rdv::state(limits);
                    let tag = {
                        let mut s = lock(&shared);
                        s.rdv = Some(st.clone());
                        s.tag
                    };
                    ep.set_alpns(vec![ALPN.to_vec(), rdv::RDV_ALPN.to_vec()]);
                    rdv::register_local(&st, tag, local_peer(&ep, bind_ip));
                }
                Some(Cmd::Rendezvous(server, pw)) => {
                    let (stop, stop_rx) = watch::channel(());
                    if let Some(old) = rdv_stop.replace(stop) {
                        old.send_replace(());
                    }
                    tasks.spawn(rdv_client(ep.clone(), shared.clone(), server, pw, stop_rx));
                }
                Some(Cmd::StopRendezvous) => {
                    if let Some(old) = rdv_stop.take() {
                        old.send_replace(());
                    }
                }
                Some(Cmd::SetFormats(f)) => lock(&shared).formats = f,
                Some(Cmd::Rekey(k, grace, evict)) => {
                    let mut s = lock(&shared);
                    s.install_key(k, grace, evict);
                    s.send_rekey(|_| true);
                }
            },
            inc = ep.accept() => match inc {
                Some(inc) => {
                    if let Some((id, inc, ip, slot)) = admit(&shared, &ids, inc) {
                        let (link, rx) = Link::new();
                        lock(&links).insert(id, link);
                        tasks.spawn(conn_task(ep.clone(), shared.clone(), id, Dir::In(inc, ip), Some(slot), rx));
                    }
                },
                None => break,
            },
            Some(_) = tasks.join_next() => {}
            _ = addr_tick.tick() => {
                let now = local_peer(&ep, bind_ip);
                let mut out = lock(&local_out);
                if *out != now {
                    let mut s = lock(&shared);
                    if let Some(r) = now.relay.as_ref().filter(|&r| out.relay.as_ref() != Some(r)) {
                        s.events.send(Event::LocalRelay(r.clone()));
                    }
                    (s.me.addrs, s.me.relay) = (now.addrs.clone(), now.relay.clone());
                    *out = now;
                }
            }
            _ = tick.tick() => {
                let (count, last_reason) = std::mem::take(&mut lock(&shared).incoming_failed);
                if count > 0 {
                    events.send(Event::IncomingFailed { count, last_reason });
                }
            }
        }
    }
    // Every connection says BYE (bounded) and closes; a host tells everyone it is leaving.
    let code = (lock(&shared).me.role == Role::Host).then_some(CloseReason::HostLeft);
    for l in lock(&links).values() {
        l.stop_with("shutdown", code);
    }
    let _ = timeout(CLOSE_TIMEOUT, async {
        while tasks.join_next().await.is_some() {}
    })
    .await;
    tasks.abort_all();
    let _ = timeout(CLOSE_TIMEOUT, ep.close()).await;
}

/// Rendezvous registration (and, with `pw`, joining by name) until `stop` fires; reconnects
/// with back-off.
async fn rdv_client(
    ep: Endpoint,
    shared: SharedRef,
    server: RdvAddr,
    pw: Option<Zeroizing<String>>,
    mut stop: watch::Receiver<()>,
) {
    let events = lock(&shared).events.clone();
    let mut backoff = Duration::from_secs(2);
    let mut tried = HashSet::new();
    loop {
        let reason = tokio::select! {
            r = rdv_session(&ep, &shared, &server, pw.as_ref(), &mut tried, &mut backoff) => r,
            _ = stop.changed() => return,
        };
        events.send(Event::RendezvousFailed {
            reason,
            retry_after: backoff,
        });
        tokio::select! {
            _ = tokio::time::sleep(backoff) => {}
            _ = stop.changed() => return,
        }
        backoff = (backoff * 2).min(Duration::from_secs(60));
    }
}

/// One rendezvous connection; returns why it ended.
async fn rdv_session(
    ep: &Endpoint,
    shared: &SharedRef,
    server: &RdvAddr,
    pw: Option<&Zeroizing<String>>,
    tried: &mut HashSet<Id>,
    backoff: &mut Duration,
) -> String {
    let addr = match server.resolve_async().await.and_then(|a| a.endpoint_addr()) {
        Ok(a) => a,
        Err(e) => return e,
    };
    let conn = match timeout(AUTH_TIMEOUT, ep.connect(addr, rdv::RDV_ALPN)).await {
        Ok(Ok(c)) => c,
        Ok(Err(e)) => return chain(&e),
        Err(_) => return "timed out".into(),
    };
    let peers = Mutex::new(Vec::<PeerAddr>::new());
    let r: Result<(), String> = async {
        let (mut send, mut recv) = conn.open_bi().await.map_err(|e| chain(&e))?;
        let (tag, me) = {
            let s = lock(shared);
            let me = PeerAddr {
                id: s.me.id,
                addrs: s.me.addrs.clone(),
                relay: s.me.relay.clone(),
            };
            (s.tag, me)
        };
        rdv::write(&mut send, &rdv::Msg::Register { tag, addr: me }).await?;
        let reader = async {
            loop {
                match rdv::read(&mut recv).await? {
                    rdv::Msg::Peers(p) => {
                        *backoff = Duration::from_secs(2);
                        *lock(&peers) = p;
                    }
                    rdv::Msg::Ping | rdv::Msg::Unknown(_) => {}
                    rdv::Msg::Register { .. } => return Err::<(), _>("unexpected REGISTER".into()),
                }
            }
        };
        let timers = async {
            let mut ping = tokio::time::interval(rdv::PING_INTERVAL);
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            ping.tick().await;
            loop {
                tokio::select! {
                    _ = ping.tick() => rdv::write(&mut send, &rdv::Msg::Ping).await?,
                    _ = tick.tick() => if let Some(pw) = pw {
                        let peers = lock(&peers).clone();
                        join_step(shared, &peers, pw, tried);
                    },
                }
            }
        };
        tokio::select! {
            r = reader => r,
            r = timers => r,
        }
    }
    .await;
    match peer_verdict(&conn) {
        Some((reason, _)) => format!("rendezvous: {reason}"),
        None => r.err().unwrap_or_default(),
    }
}

/// Joining by name: while we are in no session and no dial is under way, dial the next listed
/// peer we have not tried (cycling once all were tried), at most one per call.
fn join_step(shared: &SharedRef, peers: &[PeerAddr], pw: &Zeroizing<String>, tried: &mut HashSet<Id>) {
    let s = lock(shared);
    let me = s.me.id;
    if !s.members.is_empty() || peers.iter().any(|p| s.dialing.contains(&p.id)) {
        return;
    }
    let Some(cmd) = s.cmd.upgrade() else { return };
    drop(s);
    let cands: Vec<&PeerAddr> = peers.iter().filter(|p| p.id != me).collect();
    if cands.iter().all(|p| tried.contains(&p.id)) || tried.len() > 4 * rdv::MAX_LISTED {
        tried.clear();
    }
    if let Some(p) = cands.into_iter().find(|p| !tried.contains(&p.id)) {
        tried.insert(p.id);
        let _ = cmd.send(Cmd::DialPassword(p.clone(), pw.clone()));
    }
}

/// Holds one unauthenticated-connection slot until dropped.
struct Pending(SharedRef, IpAddr);

impl Drop for Pending {
    fn drop(&mut self) {
        let mut s = lock(&self.0);
        s.pending -= 1;
        if let Some(n) = s.pending_ip.get_mut(&self.1) {
            *n -= 1;
            if *n == 0 {
                s.pending_ip.remove(&self.1);
            }
        }
    }
}

/// Admission control at the `Incoming` stage, before any TLS work: Retry for unvalidated
/// sources, then the pending caps.
fn admit(
    shared: &SharedRef,
    ids: &AtomicU64,
    inc: Incoming,
) -> Option<(ConnId, Box<Incoming>, IpAddr, Pending)> {
    let ip = match inc.remote_addr() {
        IncomingAddr::Ip(a) => auth::ip_key(a.ip()),
        _ => Ipv4Addr::UNSPECIFIED.into(),
    };
    let inc = if inc.remote_addr_validated() {
        inc
    } else {
        match inc.retry() {
            Ok(()) => return None,
            Err(e) => e.into_incoming(),
        }
    };
    let mut s = lock(shared);
    if s.listener() {
        inc.refuse();
        s.incoming_failed("broadcast listeners accept no connections".into());
        return None;
    }
    let per_ip = s.pending_ip.get(&ip).copied().unwrap_or(0);
    if s.pending >= MAX_PENDING || per_ip >= MAX_PENDING_PER_IP {
        inc.refuse();
        let why = if per_ip >= MAX_PENDING_PER_IP {
            format!("too many unauthenticated connections from {ip}")
        } else {
            "too many unauthenticated connections".to_string()
        };
        s.incoming_failed(why);
        return None;
    }
    s.pending += 1;
    *s.pending_ip.entry(ip).or_default() += 1;
    drop(s);
    let id = ids.fetch_add(1, Ordering::Relaxed);
    Some((id, Box::new(inc), ip, Pending(shared.clone(), ip)))
}

enum Dir {
    Out(PeerAddr, Join),
    In(Box<Incoming>, IpAddr),
}

/// Connection established and authenticated.
struct Authed {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
    peer: PeerInfo,
    roster: Vec<PeerEntry>,
    formats: Vec<Format>,
    exporter: Exporter,
    /// Key epoch of the key the handshake proved.
    epoch: u64,
    /// We accepted the peer by password (and are its key authority).
    by_password: bool,
}

/// Handshake failure: the local reason, and what the peer is told (close code + reason).
struct Failure {
    local: String,
    code: u32,
    outward: Vec<u8>,
    retry_after: Option<Duration>,
    /// Not ours: an incoming rendezvous connection to hand to the embedded server.
    rdv: Option<Box<Connection>>,
}

fn fail(local: impl std::fmt::Display) -> Failure {
    Failure {
        local: local.to_string(),
        code: CLOSE_AUTH,
        outward: vec![],
        retry_after: None,
        rdv: None,
    }
}

fn retry_later(after: Duration, local: String) -> Failure {
    let secs = after.as_secs() + 1;
    Failure {
        local,
        code: CLOSE_RETRY_LATER,
        outward: secs.to_string().into_bytes(),
        retry_after: Some(Duration::from_secs(secs)),
        rdv: None,
    }
}

/// The peer's verdict when it closed the connection during the handshake.
fn peer_verdict(conn: &Connection) -> Option<(String, Option<Duration>)> {
    let ConnectionError::ApplicationClosed(c) = conn.close_reason()? else {
        return None;
    };
    Some(match c.error_code.into_inner() {
        x if x == CLOSE_AUTH as u64 => ("authentication failed".into(), None),
        x if x == CLOSE_RETRY_LATER as u64 => {
            let secs = std::str::from_utf8(&c.reason)
                .ok()
                .and_then(|s| s.parse::<u64>().ok())
                .map(|s| Duration::from_secs(s.min(3600)));
            (
                format!("retry later ({} s)", secs.unwrap_or_default().as_secs()),
                secs,
            )
        }
        x if x == CLOSE_FULL as u64 => ("session full".into(), None),
        x if x == CLOSE_BYE as u64 && &c.reason[..] == b"duplicate" => {
            ("duplicate connection".into(), None)
        }
        x => (format!("closed by peer ({x})"), None),
    })
}

async fn conn_task(
    ep: Endpoint,
    shared: SharedRef,
    id: ConnId,
    dir: Dir,
    slot: Option<Pending>,
    rx: LinkRx,
) {
    let LinkRx {
        out: mut out_rx,
        stop: mut stop_rx,
    } = rx;
    let (events, links, refused) = {
        let s = lock(&shared);
        (s.events.clone(), s.links.clone(), s.refused.clone())
    };
    let dialed = match &dir {
        Dir::Out(to, _) => Some(to.id),
        Dir::In(..) => None,
    };
    let r = tokio::select! {
        r = timeout(AUTH_TIMEOUT, establish(&ep, &shared, dir)) => {
            r.unwrap_or_else(|_| Err(fail("handshake timed out")))
        }
        _ = stop_rx.wait_for(Option::is_some) => Err(fail("cancelled")),
    };
    drop(slot);
    if let Some(d) = dialed {
        lock(&shared).dialing.remove(&d);
    }
    let r = match r {
        Err(Failure {
            rdv: Some(conn), ..
        }) => {
            lock(&links).remove(&id);
            // The real source address (the admission key is a /64 or IPv4 address).
            let ip = match conn.paths().iter().find(|p| p.is_selected()).map(|p| p.remote_addr().clone()) {
                Some(TransportAddr::Ip(a)) => a.ip(),
                _ => Ipv4Addr::UNSPECIFIED.into(),
            };
            let st = lock(&shared).rdv.clone();
            match st {
                Some(st) => drop(rdv::serve_new(st, *conn, ip).await),
                None => conn.close(CLOSE_AUTH.into(), b""),
            }
            return;
        }
        r => r,
    };
    let failed = |f: Failure| {
        lock(&links).remove(&id);
        match dialed {
            Some(_) => events.send(Event::ConnectFailed {
                conn: id,
                reason: f.local,
                retry_after: f.retry_after,
            }),
            None => lock(&shared).incoming_failed(f.local),
        }
    };
    let a = match r {
        Ok(a) => a,
        Err(f) => return failed(f),
    };
    let Authed {
        conn,
        send,
        recv,
        peer,
        roster,
        formats,
        exporter,
        epoch,
        by_password,
    } = a;

    // One connection per pair of endpoints: keep the one dialed by the smaller id.
    let (keep, replaced, others, entry, full, mesh) = {
        let mut s = lock(&shared);
        let full = s.full(&peer.id);
        let mesh = s.mode == SessionMode::Mesh;
        let me = s.me.id;
        let initiator = |outgoing: bool| if outgoing { me } else { peer.id };
        let new_init = initiator(!peer.incoming);
        let (keep, replaced) = match s.members.get(&peer.id) {
            None => (true, None),
            Some(m) if initiator(m.outgoing) != new_init && new_init < initiator(m.outgoing) => {
                (true, Some(m.conn))
            }
            Some(_) => (false, None),
        };
        let entry = PeerEntry {
            id: peer.id,
            addrs: peer.addrs.clone(),
            relay: peer.relay.clone(),
            name: peer.name.clone(),
            role: peer.role,
        };
        let keep = keep && !full;
        let others: Vec<ConnId> = s
            .members
            .values()
            .filter(|m| mesh && m.entry.id != peer.id)
            .map(|m| m.conn)
            .collect();
        if keep {
            s.members.insert(
                peer.id,
                Member {
                    conn: id,
                    outgoing: !peer.incoming,
                    entry: entry.clone(),
                },
            );
        }
        (keep, replaced, others, entry, full, mesh)
    };
    if full {
        // Concurrent joins passed the acceptor's check together.
        conn.close(CLOSE_FULL.into(), b"");
        return failed(fail("session full"));
    }
    if !keep {
        conn.close(CLOSE_BYE.into(), b"duplicate");
        return failed(fail("duplicate connection"));
    }
    {
        let mut l = lock(&links);
        // checked under the links lock, as `Net::block` sets it: a block that came during the
        // handshake stops this link before anything can be queued to it
        let blocked = lock(&refused).contains(&peer.id);
        if let Some(r) = replaced.and_then(|r| l.get(&r)) {
            r.stop_with("duplicate connection", Some(CloseReason::Duplicate));
        }
        // Tell every other member about the new one: two newcomers that missed each other's
        // WELCOME still learn of each other through any member they share.
        let mut b = Vec::new();
        if replaced.is_none() && !blocked && Frame::Roster(vec![entry]).encode(&mut b).is_ok() {
            for o in others.iter().filter_map(|o| l.get(o)) {
                let _ = o.queue(b.clone());
            }
        }
        if let Some(me) = l.get_mut(&id) {
            if blocked {
                me.stop_with("blocked", Some(CloseReason::Blocked));
            }
            me.peer = Some(peer.id);
        }
    }
    let sink = catch_unwind(AssertUnwindSafe(|| (lock(&shared).sinks)(id, &peer)));
    let reason = match sink {
        Ok(sink) => {
            let tx = PeerTx { conn: conn.clone() };
            if mesh && !peer.incoming {
                // Mesh: the newcomer dials every roster member it is not connected to.
                if let Some(cmd) = lock(&shared).cmd.upgrade() {
                    for e in roster.iter().filter(|e| e.id != peer.id) {
                        let _ = cmd.send(Cmd::Dial(PeerAddr {
                            id: e.id,
                            addrs: e.addrs.clone(),
                            relay: e.relay.clone(),
                        }));
                    }
                }
            }
            events.send(Event::Connected {
                conn: id,
                peer: peer.clone(),
                tx,
                roster,
                formats,
            });
            session(
                &shared,
                id,
                &conn,
                (send, recv),
                &peer,
                &exporter,
                (epoch, by_password),
                sink,
                &mut out_rx,
                &mut stop_rx,
            )
            .await
        }
        Err(_) => {
            conn.close(CLOSE_BYE.into(), b"error");
            "sink factory panicked".into()
        }
    };
    {
        let mut s = lock(&shared);
        if s.members.get(&peer.id).is_some_and(|m| m.conn == id) {
            s.members.remove(&peer.id);
        }
    }
    lock(&links).remove(&id);
    events.send(Event::Closed { conn: id, reason });
}

/// KEY_PROOF and CLOSE bookkeeping of one connection.
struct Proof {
    /// Key epoch the peer has proven (the handshake proved the one current then).
    proven: u64,
    /// Its last proof that did not verify yet (it may hold a key we have not received yet).
    stash: Option<[u8; 32]>,
    deadline: Option<tokio::time::Instant>,
    /// Why the peer is closing (its CLOSE frame).
    peer_reason: Option<CloseReason>,
}

/// An authenticated connection until it ends; returns the reason. Always leaves the QUIC
/// connection closed.
#[allow(clippy::too_many_arguments)]
async fn session(
    shared: &SharedRef,
    id: ConnId,
    conn: &Connection,
    (mut send, mut recv): (SendStream, RecvStream),
    peer: &PeerInfo,
    exporter: &Exporter,
    (epoch, by_password): (u64, bool),
    mut sink: Sink,
    out_rx: &mut mpsc::UnboundedReceiver<Out>,
    stop_rx: &mut watch::Receiver<Stop>,
) -> String {
    let (events, hub, me, links, mut epochs) = {
        let s = lock(shared);
        let epochs = s.key_epoch.subscribe();
        (s.events.clone(), s.hub(), s.me.id, s.links.clone(), epochs)
    };
    let budget = Arc::new(Semaphore::new(EVENT_QUEUE_BYTES));
    let current = epochs.borrow_and_update().0;
    if epoch < current {
        // The key changed during the handshake, which proved the older key only: exchange
        // proofs of the current one now, as after any key change.
        epochs.mark_changed();
        // A peer that joined with the previous key within its grace period gets the new one,
        // as it would have had it been connected a moment earlier (if we are its authority);
        // so does one we just let in by password (that proof does not expire with a key).
        // A blocked peer's link is already closing and takes nothing.
        let s = lock(shared);
        if by_password || epoch + 1 == current && s.keys().1.is_some() {
            s.send_rekey(|m| m.conn == id);
        }
    }
    let proof = Mutex::new(Proof {
        proven: epoch,
        stash: None,
        deadline: None,
        peer_reason: None,
    });
    // The current key epoch, if `m` proves its key.
    let verify = |m: &[u8; 32]| {
        let s = lock(shared);
        let k = s.key.as_ref()?;
        auth::rekey_verify(k, exporter, &peer.id, &me, m).then(|| s.key_epoch.borrow().0)
    };
    enum End {
        PeerBye,
        Error(String),
        /// Closed locally; whether the queued frames went out.
        Local(bool),
    }
    let reader = async {
        let sec = Duration::from_secs(1);
        let (mut win, mut n, mut bytes, mut over, mut flood) = (Instant::now(), 0u32, 0, false, 0);
        loop {
            let (frame, size) = read_frame(&mut recv, wire::max_body).await?;
            let now = Instant::now();
            if now.duration_since(win) >= sec {
                if !over {
                    flood = 0;
                }
                (win, n, bytes, over) = (now, 0, 0, false);
            }
            n += 1;
            bytes += size;
            if !over && (n > CONTROL_FRAMES_PER_SEC || bytes > CONTROL_BYTES_PER_SEC) {
                over = true;
                flood += 1;
                if flood >= FLOOD_SECONDS {
                    return Err("control flood".to_string());
                }
                tokio::time::sleep_until((win + sec).into()).await;
            }
            match frame {
                Frame::Bye => return Ok(()),
                Frame::Rekey(r) => {
                    let mut s = lock(shared);
                    if *lock(&s.authority) == Some(peer.id) && s.key.as_deref() != Some(&r.key) {
                        let grace = Duration::from_millis(r.grace_ms.into());
                        // blocks the evicted member, whose closing links take nothing more...
                        s.install_key(Zeroizing::new(r.key), grace, r.evicted);
                        // ...then passes this key on, under the same lock, to those whose
                        // authority we are
                        s.send_rekey(|m| m.entry.id != peer.id && s.delegated.contains(&m.entry.id));
                    }
                }
                Frame::KeyProof(m) => {
                    let ok = verify(&m);
                    let mut p = lock(&proof);
                    match ok {
                        Some(epoch) => {
                            p.proven = p.proven.max(epoch);
                            p.deadline = None;
                            p.stash = None;
                        }
                        None => p.stash = Some(m),
                    }
                }
                Frame::Close(c) => lock(&proof).peer_reason = CloseReason::from_u8(c),
                Frame::Hello(_)
                | Frame::Pake2(_)
                | Frame::Pake3 { .. }
                | Frame::Welcome(_)
                | Frame::Unknown(_) => {}
                Frame::Format(_) | Frame::Roster(_) if hub => {}
                frame => {
                    if let Frame::Roster(r) = &frame {
                        schedule_dials(shared, r);
                    }
                    let cost = (size + 64).min(EVENT_QUEUE_BYTES) as u32;
                    let permit = budget.clone().acquire_many_owned(cost).await;
                    let _ = events
                        .0
                        .send((Event::Control { conn: id, frame }, permit.ok()));
                }
            }
        }
    };
    let datagrams = async {
        loop {
            let d = conn.read_datagram().await.map_err(|e| chain(&e))?;
            // Broadcast listeners do not publish.
            if hub && d.first().is_some_and(|k| k & 0x0f == KIND_AUDIO) {
                continue;
            }
            if catch_unwind(AssertUnwindSafe(|| sink(d))).is_err() {
                return Err::<(), _>("sink panicked".to_string());
            }
        }
    };
    // iroh switches paths on its own (relay → direct after hole punching, back on failure).
    let path_watch = async {
        let sel = |p: &PathInfo| (p.direct, p.remote, p.relay.clone());
        let mut last = selected_path(conn).as_ref().map(sel);
        let mut iv = tokio::time::interval(PATH_POLL);
        loop {
            iv.tick().await;
            if let Some(p) = selected_path(conn)
                && last.as_ref() != Some(&sel(&p))
            {
                last = Some(sel(&p));
                events.send(Event::PathChanged { conn: id, path: p });
            }
        }
    };
    // On every key change: prove ours, require theirs within the deadline.
    let rekey = async {
        loop {
            let deadline = lock(&proof).deadline;
            tokio::select! {
                r = epochs.changed() => {
                    if r.is_err() {
                        return std::future::pending::<()>().await;
                    }
                    let (epoch, grace) = *epochs.borrow_and_update();
                    let ours = lock(shared)
                        .key
                        .as_ref()
                        .map(|k| auth::rekey_proof(k, exporter, &me, &peer.id));
                    let mut b = Vec::new();
                    if let Some(m) = ours
                        && Frame::KeyProof(m).encode(&mut b).is_ok()
                        && let Some(l) = lock(&links).get(&id)
                    {
                        let _ = l.queue(b);
                    }
                    // a stashed proof may be for this key, or for a later one: kept until it
                    // verifies or the deadline passes
                    let stash = lock(&proof).stash;
                    let early = stash.and_then(|m| verify(&m));
                    let mut p = lock(&proof);
                    if let Some(e) = early {
                        p.proven = p.proven.max(e);
                        p.stash = None;
                    }
                    p.deadline = (p.proven < epoch).then(|| {
                        tokio::time::Instant::now() + grace.max(KEY_PROOF_TIMEOUT)
                    });
                }
                () = async {
                    match deadline {
                        Some(d) => tokio::time::sleep_until(d).await,
                        None => std::future::pending().await,
                    }
                } => {
                    // proven meanwhile (the reader cleared the deadline)?
                    if lock(&proof).deadline.take() != deadline {
                        continue;
                    }
                    if let Some(l) = lock(&links).get(&id) {
                        l.stop_with(
                            "removed: no proof of the current session key",
                            Some(CloseReason::Removed),
                        );
                    }
                }
            }
        }
    };
    // Writes queued frames; once a stop is requested, the ones already queued, then ends.
    let mut stop_w = stop_rx.clone();
    let writer = async {
        loop {
            let o = tokio::select! {
                biased;
                o = out_rx.recv() => o,
                _ = stop_w.wait_for(Option::is_some) => None,
            };
            let Some(o) = o else {
                return Ok::<(), String>(());
            };
            send.write_all(&o.bytes).await.map_err(|e| chain(&e))?;
        }
    };
    // A peer that does not read cannot hold a local close up for long.
    let mut stop_t = stop_rx.clone();
    let stuck = async {
        let _ = stop_t.wait_for(Option::is_some).await;
        tokio::time::sleep(FLUSH_TIMEOUT).await;
    };
    let end = tokio::select! {
        r = reader => r.map_or_else(End::Error, |()| End::PeerBye),
        r = datagrams => End::Error(r.unwrap_err()),
        r = writer => r.map_or_else(End::Error, |()| End::Local(true)),
        () = stuck => End::Local(false),
        () = path_watch => unreachable!(),
        () = rekey => unreachable!(),
    };
    let peer_reason = lock(&proof).peer_reason.map(|r| r.as_str().to_string());
    match end {
        End::Local(flushed) => {
            let (reason, code) = stop_rx
                .borrow()
                .clone()
                .unwrap_or_else(|| ("closed locally".into(), None));
            // CLOSE (with a reason) and BYE unless a frame was cut off mid-write; never wait
            // long for a stuck peer.
            if flushed {
                let bye = async {
                    if let Some(c) = code {
                        write_frame(&mut send, &Frame::Close(c as u8)).await?;
                    }
                    write_frame(&mut send, &Frame::Bye).await?;
                    send.finish().map_err(|e| e.to_string())
                };
                if let Ok(Ok(())) = timeout(FLUSH_TIMEOUT, bye).await {
                    let _ = timeout(CLOSE_TIMEOUT / 2, conn.closed()).await;
                }
            }
            conn.close(CLOSE_BYE.into(), b"bye");
            reason
        }
        End::PeerBye => {
            conn.close(CLOSE_BYE.into(), b"bye");
            peer_reason.unwrap_or_else(|| "bye".into())
        }
        End::Error(e) => {
            conn.close(CLOSE_BYE.into(), b"error");
            peer_reason.unwrap_or(e)
        }
    }
}

/// Mesh: dial roster entries we are not connected to after [`MESH_DIAL_DELAY`], unless one of
/// them connected us by then. At most [`MAX_ROSTER`] wait at once; entries without an address
/// are ignored.
fn schedule_dials(shared: &SharedRef, roster: &[PeerEntry]) {
    let mut s = lock(shared);
    let Some(cmd) = s.cmd.upgrade() else { return };
    if s.key.is_none() || s.mode != SessionMode::Mesh {
        return;
    }
    for e in roster {
        let known = e.id == s.me.id
            || s.members.contains_key(&e.id)
            || s.dialing.contains(&e.id)
            || s.scheduled.contains(&e.id);
        if known || (e.addrs.is_empty() && e.relay.is_none()) || s.scheduled.len() >= MAX_ROSTER {
            continue;
        }
        s.scheduled.insert(e.id);
        let (shared, cmd, to) = (
            shared.clone(),
            cmd.clone(),
            PeerAddr {
                id: e.id,
                addrs: e.addrs.clone(),
                relay: e.relay.clone(),
            },
        );
        tokio::spawn(async move {
            tokio::time::sleep(MESH_DIAL_DELAY).await;
            lock(&shared).scheduled.remove(&to.id);
            let _ = cmd.send(Cmd::Dial(to));
        });
    }
}

/// QUIC connect/accept, control stream, authentication. Datagrams arriving before it completes
/// are read and dropped.
async fn establish(ep: &Endpoint, shared: &SharedRef, dir: Dir) -> Result<Authed, Failure> {
    let me = *ep.id().as_bytes();
    let (conn, join, src) = match dir {
        Dir::Out(to, join) => {
            if to.id == me {
                return Err(fail("cannot connect to ourselves"));
            }
            let addr = to.endpoint_addr().map_err(fail)?;
            let conn = ep.connect(addr, ALPN).await.map_err(|e| fail(chain(&e)))?;
            (conn, Some((join, to)), None)
        }
        Dir::In(inc, ip) => {
            let conn = inc.accept().map_err(|e| fail(chain(&e)))?.await;
            let conn = conn.map_err(|e| fail(chain(&e)))?;
            if conn.alpn() == rdv::RDV_ALPN {
                return Err(Failure {
                    rdv: Some(Box::new(conn)),
                    ..fail("rendezvous")
                });
            }
            (conn, None, Some(ip))
        }
    };
    // Never completes: connection errors surface through the handshake itself.
    let discard = async {
        while conn.read_datagram().await.is_ok() {}
        std::future::pending::<()>().await
    };
    let r = tokio::select! {
        r = async {
            match (join, src) {
                (Some((join, dialed)), _) => {
                    let (mut send, mut recv) = conn.open_bi().await.map_err(fail)?;
                    connector(ep, &conn, &mut send, &mut recv, shared, me, join, dialed)
                        .await
                        .map(|a| (send, recv, a))
                }
                (None, src) => {
                    let (mut send, mut recv) = conn.accept_bi().await.map_err(fail)?;
                    let src = src.unwrap_or(Ipv4Addr::UNSPECIFIED.into());
                    acceptor(ep, &conn, &mut send, &mut recv, shared, me, src)
                        .await
                        .map(|a| (send, recv, a))
                }
            }
        } => r,
        _ = discard => unreachable!(),
    };
    match r {
        Ok((send, recv, (peer, roster, formats, exporter, epoch, by_password))) => Ok(Authed {
            conn,
            send,
            recv,
            peer,
            roster,
            formats,
            exporter,
            epoch,
            by_password,
        }),
        Err(mut f) => {
            if let Some((reason, retry)) = peer_verdict(&conn) {
                (f.local, f.retry_after) = (reason, retry);
            }
            conn.close(f.code.into(), &f.outward);
            Err(f)
        }
    }
}

fn exporter(conn: &Connection, tag: &[u8; 16]) -> Result<Exporter, Failure> {
    let mut e = [0; 32];
    conn.export_keying_material(&mut e, auth::EXPORTER_LABEL, tag)
        .map_err(|_| fail("exporter"))?;
    Ok(e)
}

/// Peer, WELCOME roster and formats, exporter, epoch of the key the handshake proved, whether
/// we accepted the peer by password.
type Welcomed = (PeerInfo, Vec<PeerEntry>, Vec<Format>, Exporter, u64, bool);

/// Frame-length cap for frames from a peer that has not authenticated yet.
fn handshake_cap(ty: u8) -> usize {
    wire::max_body(ty).min(HANDSHAKE_FRAME_MAX)
}

#[allow(clippy::too_many_arguments)]
async fn connector(
    ep: &Endpoint,
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    shared: &SharedRef,
    me: Id,
    join: Join,
    dialed: PeerAddr,
) -> Result<Welcomed, Failure> {
    let peer = *conn.remote_id().as_bytes();
    let (tag, name, role, key, mut epoch, addrs, host) = {
        let s = lock(shared);
        let addrs = advertised(ep, s.bind_ip);
        (
            s.tag,
            s.me.name.clone(),
            s.me.role,
            s.key.clone(),
            s.key_epoch.borrow().0,
            addrs,
            s.host,
        )
    };
    let relay = home_relay(ep);
    let e = exporter(conn, &tag)?;
    let hello = |method, proof| {
        Frame::Hello(Hello {
            method,
            tag,
            role,
            name: name.clone(),
            proof,
            addrs: addrs.clone(),
            relay: relay.clone(),
        })
    };
    let (key, w, pake) = match join {
        Join::Key => {
            let k = key.ok_or_else(|| fail("no session key"))?;
            let proof = auth::key_proof(&k, Side::Connector, &e, &me, &peer).to_vec();
            write_frame(send, &hello(Method::Key, proof))
                .await
                .map_err(fail)?;
            let Frame::Welcome(w) = read_auth_frame(recv, wire::max_body).await? else {
                return Err(fail("unexpected frame"));
            };
            (k, w, false)
        }
        Join::Password(pw) => {
            let pw = Zeroizing::new(pw);
            let pw = auth::normalize_secret(&pw).map_err(fail)?;
            let (pake, x) = PakeConnector::start(&pw, &e, &me, &peer);
            write_frame(send, &hello(Method::Pake, x))
                .await
                .map_err(fail)?;
            let Frame::Pake2(p2) = read_auth_frame(recv, handshake_cap).await? else {
                return Err(fail("unexpected frame"));
            };
            let (kc, conf) = pake.finish(&p2.y).map_err(fail)?;
            write_frame(send, &Frame::Pake3 { conf })
                .await
                .map_err(fail)?;
            let Frame::Welcome(w) = read_auth_frame(recv, wire::max_body).await? else {
                return Err(fail("unexpected frame"));
            };
            let k = kc
                .unwrap(w.key.as_ref().ok_or_else(|| fail("no key"))?)
                .map_err(fail)?;
            (Zeroizing::new(k), w, true)
        }
    };
    auth::key_verify(&key, Side::Acceptor, &e, &peer, &me, &w.mac).map_err(fail)?;
    if pake {
        let mut s = lock(shared);
        *lock(&s.authority) = Some(peer);
        if s.key.as_deref() != Some(&*key) {
            s.install_key(key, Duration::ZERO, None);
        }
        epoch = s.key_epoch.borrow().0;
    }
    let info = w.roster.iter().find(|r| r.id == peer);
    let peer = PeerInfo {
        id: peer,
        name: info.map(|r| r.name.clone()).unwrap_or_default(),
        role: info.map_or(Role::Peer, |r| r.role),
        addrs: info
            .map(|r| r.addrs.clone())
            .filter(|a| !a.is_empty())
            .unwrap_or(dialed.addrs),
        relay: info.and_then(|r| r.relay.clone()).or(dialed.relay),
        incoming: false,
        is_invite_host: host == Some(peer),
    };
    let roster = w.roster.into_iter().filter(|r| r.id != me).collect();
    Ok((peer, roster, w.formats, e, epoch, false))
}

/// Reports a PAKE attempt that did not succeed when dropped.
struct AttemptGuard<'a> {
    shared: &'a SharedRef,
    attempt: Option<auth::Attempt>,
    succeeded: bool,
}

impl Drop for AttemptGuard<'_> {
    fn drop(&mut self) {
        if !self.succeeded {
            let s = lock(self.shared);
            s.events.send(Event::JoinFailed {
                failed_total: s.lockout.failed_total(),
            });
        }
    }
}

async fn acceptor(
    ep: &Endpoint,
    conn: &Connection,
    send: &mut SendStream,
    recv: &mut RecvStream,
    shared: &SharedRef,
    me: Id,
    src: IpAddr,
) -> Result<Welcomed, Failure> {
    let peer = *conn.remote_id().as_bytes();
    let Frame::Hello(h) = read_auth_frame(recv, handshake_cap).await? else {
        return Err(fail("expected HELLO"));
    };
    let (tag, refused) = {
        let s = lock(shared);
        (s.tag, s.refused.clone())
    };
    if lock(&refused).contains(&peer) {
        return Err(fail("blocked"));
    }
    // Our tag even when the HELLO names another session: every path below does the same work.
    let e = exporter(conn, &tag)?;
    let tag_ok = h.tag == tag;
    // KEY: the key proved and its epoch; PAKE: the confirmed PAKE key (the session key is
    // taken below, after the block check)
    let (proved, pake) = match h.method {
        Method::Key => {
            let (cur, old, epoch) = {
                let s = lock(shared);
                let (cur, old) = s.keys();
                (cur, old, s.key_epoch.borrow().0)
            };
            let dummy = Zeroizing::new([0u8; 32]);
            let check = |k: &Option<Zeroizing<Key>>| {
                let ok = auth::key_verify(
                    k.as_ref().unwrap_or(&dummy),
                    Side::Connector,
                    &e,
                    &peer,
                    &me,
                    &h.proof,
                );
                ok.is_ok() && k.is_some()
            };
            let (ok_cur, ok_old) = (check(&cur), check(&old));
            match (tag_ok, cur, old) {
                (false, ..) => return Err(fail("unknown session")),
                (true, Some(k), _) if ok_cur => (Some((k, epoch)), None),
                (true, _, Some(k)) if ok_old => (Some((k, epoch.saturating_sub(1))), None),
                (true, None, _) => return Err(fail("no session key")),
                _ => return Err(fail("wrong session key")),
            }
        }
        Method::Pake => {
            let begun = lock(shared).lockout.begin(src, Instant::now());
            let attempt = match begun {
                Ok(a) => a,
                Err(d) => {
                    if d.newly {
                        lock(shared).events.send(Event::Throttled {
                            ip: (!d.global).then_some(src),
                            retry_after: d.retry_after,
                        });
                    }
                    let why = if d.global { "global" } else { "per-address" };
                    return Err(retry_later(
                        d.retry_after,
                        format!("password joins throttled ({why})"),
                    ));
                }
            };
            let mut guard = AttemptGuard {
                shared,
                attempt: Some(attempt),
                succeeded: false,
            };
            let generation = guard.attempt.as_ref().map(|a| a.generation());
            let (real, why) = {
                let s = lock(shared);
                let why = if !tag_ok {
                    "unknown session"
                } else if s.password.is_none() || s.key.is_none() {
                    "no password set"
                } else if s.lockout.pin_invalidated() {
                    "PIN invalidated"
                } else if Some(s.lockout.generation()) != generation {
                    "password changed during the attempt"
                } else {
                    ""
                };
                let real = if why.is_empty() {
                    s.password.clone()
                } else {
                    None
                };
                (real, why)
            };
            // No usable password: run the PAKE with a random one so the outcome (and the
            // work done) is the same as for a wrong guess.
            let pw = real.clone().unwrap_or_else(|| {
                Zeroizing::new(
                    auth::random_key()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect(),
                )
            });
            let (pa, y) = PakeAcceptor::respond(&pw, &e, &peer, &me, &h.proof)
                .map_err(|_| fail("malformed PAKE message"))?;
            write_frame(send, &Frame::Pake2(Pake2 { y }))
                .await
                .map_err(fail)?;
            let Frame::Pake3 { conf } = read_auth_frame(recv, handshake_cap).await? else {
                return Err(fail("expected PAKE3"));
            };
            let kc = match pa.confirm(&conf) {
                Ok(kc) => kc,
                Err(_) => {
                    if real.is_some() {
                        let a = guard.attempt.as_ref().expect("attempt");
                        let mut s = lock(shared);
                        if s.lockout.wrong_guess(a) {
                            s.events.send(Event::PinInvalidated);
                        }
                    }
                    return Err(fail(if real.is_some() {
                        "wrong password"
                    } else {
                        why
                    }));
                }
            };
            let a = guard.attempt.take().expect("attempt");
            if !lock(shared).lockout.succeeded(a) {
                return Err(fail("password changed during the attempt"));
            }
            guard.succeeded = true;
            (None, Some(kc))
        }
    };
    let (k, epoch, me_entry, members, formats, host) = {
        let mut s = lock(shared);
        // again: a block during the PAKE round trip
        if lock(&refused).contains(&peer) {
            return Err(fail("blocked"));
        }
        if s.full(&peer) {
            return Err(Failure {
                code: CLOSE_FULL,
                ..fail("session full")
            });
        }
        let (k, epoch) = match proved {
            Some(p) => p,
            // Password confirmed and not blocked: the key current now (a rotation during the
            // PAKE round trip included), and we are its authority from here on.
            None => {
                let k = s.key.clone().ok_or_else(|| fail("no session key"))?;
                s.delegated.insert(peer);
                (k, s.key_epoch.borrow().0)
            }
        };
        let me_entry = PeerEntry {
            addrs: advertised(ep, s.bind_ip),
            relay: home_relay(ep),
            ..s.me.clone()
        };
        // A broadcast hub lists only itself: listeners never learn of each other.
        let members: Vec<PeerEntry> = s
            .members
            .values()
            .filter(|m| !s.hub() && m.entry.id != peer)
            .map(|m| m.entry.clone())
            .collect();
        (k, epoch, me_entry, members, s.formats.clone(), s.host)
    };
    let mut roster = vec![me_entry];
    roster.extend(members.into_iter().take(MAX_ROSTER - 1));
    let mac = auth::key_proof(&k, Side::Acceptor, &e, &me, &peer);
    let welcome = Frame::Welcome(Welcome {
        mac,
        key: pake.as_ref().map(|kc| kc.wrap(&k)),
        roster,
        formats,
    });
    write_frame(send, &welcome).await.map_err(fail)?;
    let mut addrs = h.addrs;
    if let Some(TransportAddr::Ip(a)) = conn
        .paths()
        .iter()
        .find(|p| p.is_selected())
        .map(|p| p.remote_addr().clone())
        && !addrs.contains(&a)
        && addrs.len() < MAX_ADDRS
    {
        addrs.push(a);
    }
    Ok((
        PeerInfo {
            id: peer,
            name: h.name,
            role: h.role,
            addrs,
            relay: h.relay,
            incoming: true,
            is_invite_host: host == Some(peer),
        },
        vec![],
        vec![],
        e,
        epoch,
        pake.is_some(),
    ))
}

/// Next handshake frame within `cap`; unknown types are skipped.
async fn read_auth_frame(r: &mut RecvStream, cap: fn(u8) -> usize) -> Result<Frame, Failure> {
    loop {
        match read_frame(r, cap).await.map_err(fail)?.0 {
            Frame::Unknown(_) => {}
            f => return Ok(f),
        }
    }
}

/// Error text including its source chain (iroh's top-level messages omit the peer's reason).
pub(crate) fn chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(e) = src {
        s = format!("{s}: {e}");
        src = e.source();
    }
    s
}

/// One frame and its encoded size. The header's length is checked against `cap(type)` before
/// anything else is read, and the body is collected as it arrives.
async fn read_frame(r: &mut RecvStream, cap: fn(u8) -> usize) -> Result<(Frame, usize), String> {
    let (ty, body) = read_raw(r, cap).await?;
    let f = Frame::decode_body(ty, &body).map_err(|e| e.to_string())?;
    Ok((f, wire::FRAME_HEADER + body.len()))
}

/// One `u16 len | u8 type | body` frame, undecoded; the length is checked against `cap(type)`
/// before the body is read.
pub(crate) async fn read_raw(
    r: &mut RecvStream,
    cap: fn(u8) -> usize,
) -> Result<(u8, Vec<u8>), String> {
    let mut h = [0u8; wire::FRAME_HEADER];
    r.read_exact(&mut h).await.map_err(|e| chain(&e))?;
    let len = u16::from_le_bytes([h[0], h[1]]) as usize;
    if len > cap(h[2]) {
        return Err(format!("frame type {} too long ({len} B)", h[2]));
    }
    let mut body = Vec::with_capacity(len.min(4096));
    while body.len() < len {
        match r
            .read_chunk(len - body.len())
            .await
            .map_err(|e| chain(&e))?
        {
            Some(c) => body.extend_from_slice(&c),
            None => return Err("control stream ended".into()),
        }
    }
    Ok((h[2], body))
}

async fn write_frame(w: &mut SendStream, f: &Frame) -> Result<(), String> {
    let mut b = Vec::new();
    f.encode(&mut b).map_err(|e| e.to_string())?;
    w.write_all(&b).await.map_err(|e| chain(&e))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Configuration only: nothing here binds a relay-enabled endpoint, so nothing can reach
    /// n0's relays or any other host.
    #[test]
    fn relay_choice_maps_to_iroh_relay_mode() {
        assert_eq!(relay_mode(&RelayChoice::None), Ok(RelayMode::Disabled));
        // n0's production relays, not the env-switchable `default_relay_mode()`.
        let n0 = relay_mode(&RelayChoice::N0Default).unwrap();
        assert_eq!(n0, RelayMode::Default);
        let urls: Vec<RelayUrl> = n0.relay_map().urls();
        assert!(!urls.is_empty());
        assert!(
            urls.iter().all(|u| u.as_str().contains(".relay.n0.iroh")),
            "{urls:?}"
        );
        let custom = relay_mode(&RelayChoice::Custom(vec!["http://127.0.0.1:3340".into()]));
        let map = custom.unwrap().relay_map();
        assert_eq!(
            map.urls::<Vec<RelayUrl>>(),
            ["http://127.0.0.1:3340".parse::<RelayUrl>().unwrap()]
        );
        assert!(relay_mode(&RelayChoice::Custom(vec![])).is_err());
        assert!(relay_mode(&RelayChoice::Custom(vec!["not a url".into()])).is_err());
        assert_eq!(Config::default().relay, RelayChoice::N0Default);
    }

    /// The endpoint never gets an address lookup service (no pkarr/DNS publishing), whatever
    /// the relay choice: `bind` adds none. Bound with relays disabled to stay offline.
    #[test]
    fn endpoint_has_no_address_lookup() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let ep = bind(Ipv4Addr::LOCALHOST.into(), 0, false, None, RelayMode::Disabled, ALPN)
                .await
                .unwrap();
            assert!(ep.address_lookup().unwrap().is_empty());
            ep.close().await;
        });
    }

    #[test]
    fn rdv_addr_resolves_and_converts() {
        let p = PeerAddr {
            id: [7; 32],
            addrs: vec![
                "192.0.2.1:5".parse().unwrap(),
                "[2001:db8::1]:6".parse().unwrap(),
            ],
            relay: Some("http://r/".into()),
        };
        let r = RdvAddr::from(p.clone());
        assert_eq!(r.hosts, ["192.0.2.1:5", "[2001:db8::1]:6"]);
        assert_eq!(r.resolve(), Ok(p.clone()));
        // duplicates dropped; a name goes to the resolver (localhost: /etc/hosts)
        let n = RdvAddr {
            hosts: vec!["localhost:7".into(), "127.0.0.1:7".into()],
            ..r.clone()
        };
        let got = n.resolve().unwrap();
        assert!(got.addrs.contains(&"127.0.0.1:7".parse().unwrap()), "{got:?}");
        assert_eq!(got.addrs.iter().filter(|a| a.ip().is_ipv4()).count(), 1);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        assert_eq!(rt.block_on(n.resolve_async()), Ok(got));
        assert_eq!(rt.block_on(r.resolve_async()), Ok(p));
        let none = RdvAddr {
            hosts: vec![],
            relay: None,
            ..r
        };
        assert_eq!(none.resolve(), Err("no address".into()));
    }

    /// A taken port: clients fall back to an ephemeral one, `port_required` is an error that
    /// names the address.
    #[test]
    fn taken_port_falls_back_unless_required() {
        let held = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let taken = held.local_addr().unwrap();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let b = |strict| bind(taken.ip(), taken.port(), strict, None, RelayMode::Disabled, ALPN);
            let e = b(true).await.err().unwrap();
            assert!(e.starts_with(&format!("cannot bind UDP {taken}: ")), "{e}");
            let ep = b(false).await.unwrap();
            assert!(ep.bound_sockets().iter().all(|a| a.port() != taken.port()));
            ep.close().await;
        });
    }
}
