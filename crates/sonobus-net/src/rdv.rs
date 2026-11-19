//! Rendezvous for session name + password joins over the internet (ALPN [`RDV_ALPN`]).
//!
//! Endpoints that are in (or host) a session register `REGISTER{tag, own address}`; the server
//! pushes `PEERS{entries}` (the other registrations of that tag, oldest first, at most
//! [`Limits::listed`]) to every registrant of the tag whenever it changes; closing the
//! connection unregisters; clients send `PING` as keep-alive. A joiner registers, dials a
//! listed peer, runs the normal PAKE join with the password and then the roster mesh
//! (`Net::join_by_name`); it stays registered so later joiners find a member.
//!
//! Frames: `u16 len | u8 type | body` (as the control stream); little-endian.
//! * REGISTER (1): `tag[16] | peer`
//! * PEERS (2): `u8 n (≤ MAX_LISTED) | n × peer`
//! * PING (3): empty
//!
//! `peer` = `id[32] | u8 n (≤ MAX_ADDRS) | n × addr | u8 len | relay URL`, `addr` as in the
//! control frames. The REGISTER id must be the connection's TLS-proven endpoint id; the server
//! appends the address it sees the registrant at (its NAT mapping) to the listed addresses.
//!
//! # Threat notes
//!
//! * The server learns, for every registrant: its session tag, endpoint id, addresses and IP.
//!   It never sees the password or the session key: registration carries no secret and proves
//!   nothing about knowing the password; the PAKE between the peers does that.
//! * A tag is `SHA-256("sb2 tag" ‖ name)[..16]`: anyone (the server or any client) can compute
//!   the tags of guessable names ("jam", "band practice") and so learn which guessable names
//!   are in use. Worse, any client that registers a guessed tag receives the PEERS of that
//!   session: the ids and IP addresses of its members (not the password, not the audio). Use
//!   an unguessable session name when member IPs must stay private, or invite links.
//! * Registrations are unauthenticated, so an attacker can register junk for a tag and make
//!   joiners waste dials. Bounded by the per-IP and per-tag caps and the listing order
//!   (oldest first: a session's long-standing members stay at the front).
//! * Limits (see [`Limits`]): connections in total and per IP (IPv4 address / IPv6 /64), one
//!   registration per connection, registrations per tag, a REGISTER token bucket per IP, an
//!   idle timeout and REGISTER deadline, and PEERS fan-out coalesced to at most one push per
//!   [`Limits::fanout_interval`] per connection (latest list wins; nothing queues up), and at
//!   most [`FRAMES_PER_SEC`] frames read per second and connection (excess waits, so QUIC
//!   flow control throttles the sender; [`rt::FLOOD_SECONDS`] seconds over it close the
//!   connection). Every map is bounded by the connection cap (rate-limit state by a multiple
//!   of it; a new source is refused while that is full of buckets in use).

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};
use std::sync::{Arc, Mutex, mpsc as smpsc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iroh::endpoint::{Connection, IncomingAddr, RecvStream, SendStream};
use iroh::{Endpoint, TransportAddr};
use tokio::sync::{oneshot, watch};
use tokio::time::timeout;

use crate::auth;
use crate::rt::{self, CLOSE_AUTH, CLOSE_FULL, CLOSE_RETRY_LATER, PeerAddr, RelayChoice, lock};
use crate::wire::{self, Error, MAX_ADDRS, MAX_RELAY_URL, Rd};

pub const RDV_ALPN: &[u8] = b"sonobus/rdv/1";
pub const T_REGISTER: u8 = 1;
pub const T_PEERS: u8 = 2;
pub const T_PING: u8 = 3;
/// Most entries one PEERS frame carries.
pub const MAX_LISTED: usize = 32;
const PEER_MAX: usize = 32 + 1 + MAX_ADDRS * 19 + 1 + MAX_RELAY_URL;
/// Clients send PING this often.
pub const PING_INTERVAL: Duration = Duration::from_secs(20);
/// Frames read per second and connection (a client sends one per [`PING_INTERVAL`]); each is
/// at most ~1 KiB ([`max_body`]), so this is the byte budget too.
pub const FRAMES_PER_SEC: u32 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Register { tag: [u8; 16], addr: PeerAddr },
    Peers(Vec<PeerAddr>),
    Ping,
    /// A type this version does not know (body skipped).
    Unknown(u8),
}

/// Largest valid body of a frame of type `ty` (checked before the body is read).
pub fn max_body(ty: u8) -> usize {
    match ty {
        T_REGISTER => 16 + PEER_MAX,
        T_PEERS => 1 + MAX_LISTED * PEER_MAX,
        T_PING => 0,
        _ => 1024,
    }
}

fn put_peer(out: &mut Vec<u8>, p: &PeerAddr) -> Result<(), Error> {
    out.extend_from_slice(&p.id);
    wire::put_addrs(out, &p.addrs)?;
    wire::put_blob(
        out,
        p.relay.as_deref().unwrap_or("").as_bytes(),
        MAX_RELAY_URL,
    )
}

fn get_peer(r: &mut Rd) -> Result<PeerAddr, Error> {
    let id = r.arr()?;
    let addrs = r.addrs()?;
    let relay = r.string(MAX_RELAY_URL)?;
    Ok(PeerAddr {
        id,
        addrs,
        relay: (!relay.is_empty()).then_some(relay),
    })
}

impl Msg {
    /// Append the whole frame; on error `out` is unchanged.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let start = out.len();
        let ty = match self {
            Msg::Register { .. } => T_REGISTER,
            Msg::Peers(_) => T_PEERS,
            Msg::Ping => T_PING,
            Msg::Unknown(t) => *t,
        };
        out.extend_from_slice(&[0, 0, ty]);
        let r = match self {
            Msg::Register { tag, addr } => {
                out.extend_from_slice(tag);
                put_peer(out, addr)
            }
            Msg::Peers(p) if p.len() > MAX_LISTED => Err(Error::TooLong),
            Msg::Peers(p) => {
                out.push(p.len() as u8);
                p.iter().try_for_each(|p| put_peer(out, p))
            }
            Msg::Ping | Msg::Unknown(_) => Ok(()),
        };
        if let Err(e) = r {
            out.truncate(start);
            return Err(e);
        }
        let len = (out.len() - start - wire::FRAME_HEADER) as u16;
        out[start..start + 2].copy_from_slice(&len.to_le_bytes());
        Ok(())
    }

    pub fn decode_body(ty: u8, body: &[u8]) -> Result<Msg, Error> {
        let mut r = Rd(body);
        let m = match ty {
            T_REGISTER => Msg::Register {
                tag: r.arr()?,
                addr: get_peer(&mut r)?,
            },
            T_PEERS => {
                let n = r.u8()? as usize;
                if n > MAX_LISTED {
                    return Err(Error::TooLong);
                }
                Msg::Peers((0..n).map(|_| get_peer(&mut r)).collect::<Result<_, _>>()?)
            }
            T_PING => Msg::Ping,
            t => return Ok(Msg::Unknown(t)),
        };
        if !r.0.is_empty() {
            return Err(Error::Invalid);
        }
        Ok(m)
    }
}

// ---------------------------------------------------------------------------------------------
// Server core (sans-IO)

/// Server limits; [`Default`] is sized for a small public instance.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Connections at once.
    pub max_connections: usize,
    /// Connections per source (IPv4 address / IPv6 /64).
    pub per_ip: usize,
    /// Registrations per tag.
    pub per_tag: usize,
    /// Entries in one PEERS (≤ [`MAX_LISTED`]).
    pub listed: usize,
    /// REGISTER token bucket per source: burst...
    pub register_burst: u32,
    /// ...and one more token every this long.
    pub register_refill: Duration,
    /// A connection must REGISTER within this.
    pub register_timeout: Duration,
    /// A connection silent (no frame) this long is closed.
    pub idle: Duration,
    /// PEERS go to one connection at most this often (the latest list wins).
    pub fanout_interval: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Limits {
            max_connections: 4096,
            per_ip: 16,
            per_tag: 256,
            listed: MAX_LISTED,
            register_burst: 10,
            register_refill: Duration::from_secs(6),
            register_timeout: Duration::from_secs(5),
            idle: PING_INTERVAL * 3,
            fanout_interval: Duration::from_millis(250),
        }
    }
}

/// Why the server refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// Global connection cap.
    Busy,
    /// Per-source connection cap.
    PerIp,
    /// REGISTER rate limit; retry after this.
    RateLimited(Duration),
    /// Registrations per tag.
    TagFull,
    /// A second REGISTER on one connection, or a REGISTER naming another endpoint.
    Protocol,
}

/// Cumulative refusals and drops since the server started, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refusals {
    /// Connections refused: global cap ([`Limits::max_connections`]).
    pub busy: u64,
    /// Connections refused: per-source cap ([`Limits::per_ip`]).
    pub per_ip: u64,
    /// TLS handshake failed or timed out.
    pub handshake: u64,
    /// No REGISTER within [`Limits::register_timeout`].
    pub register_timeout: u64,
    /// REGISTERs refused by the per-source rate limit.
    pub rate_limited: u64,
    /// REGISTERs refused: [`Limits::per_tag`] reached.
    pub tag_full: u64,
    /// A REGISTER naming another endpoint than the connection's.
    pub id_mismatch: u64,
    /// A second REGISTER, another frame where REGISTER was due, or an undecodable frame.
    pub protocol: u64,
    /// Registrations closed after [`Limits::idle`] without a frame.
    pub idle: u64,
    /// Registrations closed for sending over [`FRAMES_PER_SEC`] for [`rt::FLOOD_SECONDS`].
    pub flood: u64,
}

/// A snapshot of [`Core`]'s state ([`Core::stats`], [`RendezvousServer::counts`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Counts {
    pub connections: usize,
    /// Distinct sources (IPv4 address / IPv6 /64).
    pub sources: usize,
    pub tags: usize,
    pub registrations: usize,
    pub refused: Refusals,
}

pub type ConnKey = u64;

struct Conn {
    ip: IpAddr,
    reg: Option<([u8; 16], PeerAddr)>,
}

/// Registrations and limits, no I/O. Keys come from [`admit`](Core::admit).
pub struct Core {
    limits: Limits,
    next: ConnKey,
    conns: HashMap<ConnKey, Conn>,
    /// Registrants per tag, oldest first.
    tags: HashMap<[u8; 16], Vec<ConnKey>>,
    ips: HashMap<IpAddr, usize>,
    /// `(tokens, refilled at)` per source.
    buckets: HashMap<IpAddr, (u32, Instant)>,
    registrations: usize,
    /// Counted here for refusals `Core` decides, by the I/O side for the rest.
    pub(crate) refused: Refusals,
}

impl Core {
    pub fn new(mut limits: Limits) -> Core {
        limits.listed = limits.listed.min(MAX_LISTED);
        Core {
            limits,
            next: 1,
            conns: HashMap::new(),
            tags: HashMap::new(),
            ips: HashMap::new(),
            buckets: HashMap::new(),
            registrations: 0,
            refused: Refusals::default(),
        }
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// A new connection from `ip`, before any TLS work.
    pub fn admit(&mut self, ip: IpAddr) -> Result<ConnKey, Refused> {
        let ip = auth::ip_key(ip);
        if self.conns.len() >= self.limits.max_connections {
            self.refused.busy += 1;
            return Err(Refused::Busy);
        }
        let n = self.ips.entry(ip).or_default();
        if *n >= self.limits.per_ip {
            self.refused.per_ip += 1;
            return Err(Refused::PerIp);
        }
        *n += 1;
        let key = self.next;
        self.next += 1;
        self.conns.insert(key, Conn { ip, reg: None });
        Ok(key)
    }

    /// Register connection `key`. Returns the connections whose PEERS changed (all registrants
    /// of the tag, `key` included).
    pub fn register(
        &mut self,
        key: ConnKey,
        now: Instant,
        tag: [u8; 16],
        addr: PeerAddr,
    ) -> Result<Vec<ConnKey>, Refused> {
        let Some(c) = self.conns.get(&key).filter(|c| c.reg.is_none()) else {
            self.refused.protocol += 1;
            return Err(Refused::Protocol);
        };
        let ip = c.ip;
        let refill = self.limits.register_refill.max(Duration::from_nanos(1));
        let burst = self.limits.register_burst;
        // Token bucket per source, at most 4 x the connection cap of them: when full, the
        // fully replenished ones go, and a new source waits if none did.
        let cap = 4 * self.limits.max_connections;
        if self.buckets.len() >= cap && !self.buckets.contains_key(&ip) {
            self.buckets.retain(|_, (t, at)| {
                (*t as u128 + now.saturating_duration_since(*at).as_nanos() / refill.as_nanos())
                    < burst as u128
            });
            if self.buckets.len() >= cap {
                self.refused.rate_limited += 1;
                return Err(Refused::RateLimited(refill));
            }
        }
        let (tokens, at) = self.buckets.entry(ip).or_insert((burst, now));
        let n = (now.saturating_duration_since(*at).as_nanos() / refill.as_nanos())
            .min(burst as u128) as u32;
        if n > 0 {
            *tokens = (*tokens + n).min(burst);
            *at = if *tokens == burst { now } else { *at + refill * n };
        }
        if *tokens == 0 {
            self.refused.rate_limited += 1;
            return Err(Refused::RateLimited(
                (*at + refill).saturating_duration_since(now),
            ));
        }
        *tokens -= 1;
        let members = self.tags.entry(tag).or_default();
        if members.len() >= self.limits.per_tag {
            self.refused.tag_full += 1;
            return Err(Refused::TagFull);
        }
        members.push(key);
        let affected = members.clone();
        if let Some(c) = self.conns.get_mut(&key) {
            c.reg = Some((tag, addr));
        }
        self.registrations += 1;
        Ok(affected)
    }

    /// Connection `key` ended. Returns the connections whose PEERS changed.
    pub fn close(&mut self, key: ConnKey) -> Vec<ConnKey> {
        let Some(c) = self.conns.remove(&key) else {
            return vec![];
        };
        if let Some(n) = self.ips.get_mut(&c.ip) {
            *n -= 1;
            if *n == 0 {
                self.ips.remove(&c.ip);
            }
        }
        let Some((tag, _)) = c.reg else {
            return vec![];
        };
        self.registrations -= 1;
        let Some(members) = self.tags.get_mut(&tag) else {
            return vec![];
        };
        members.retain(|&k| k != key);
        let left = members.clone();
        if left.is_empty() {
            self.tags.remove(&tag);
        }
        left
    }

    /// What `key` should be told: the other registrants of its tag, oldest first.
    pub fn peers(&self, key: ConnKey) -> Vec<PeerAddr> {
        let Some((tag, _)) = self.conns.get(&key).and_then(|c| c.reg.as_ref()) else {
            return vec![];
        };
        self.tags
            .get(tag)
            .into_iter()
            .flatten()
            .filter(|&&k| k != key)
            .filter_map(|k| self.conns.get(k)?.reg.as_ref().map(|(_, a)| a.clone()))
            .take(self.limits.listed)
            .collect()
    }

    /// Connections, distinct sources, tags (for tests and monitoring).
    pub fn counts(&self) -> (usize, usize, usize) {
        (self.conns.len(), self.ips.len(), self.tags.len())
    }

    /// Current counts and cumulative refusals. O(1).
    pub fn stats(&self) -> Counts {
        Counts {
            connections: self.conns.len(),
            sources: self.ips.len(),
            tags: self.tags.len(),
            registrations: self.registrations,
            refused: self.refused,
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Server runner (shared by the standalone server and the one embedded in `Net`)

/// Core plus each registered connection's PEERS outbox (latest list only).
pub(crate) struct State {
    core: Core,
    out: HashMap<ConnKey, watch::Sender<Vec<PeerAddr>>>,
}

pub(crate) type StateRef = Arc<Mutex<State>>;

pub(crate) fn state(limits: Limits) -> StateRef {
    Arc::new(Mutex::new(State {
        core: Core::new(limits),
        out: HashMap::new(),
    }))
}

fn notify(st: &State, keys: &[ConnKey]) {
    for k in keys {
        if let Some(tx) = st.out.get(k) {
            tx.send_replace(st.core.peers(*k));
        }
    }
}

/// A registration of the server's own endpoint (embedded server: the hosting device itself is
/// findable). Lives until the state is dropped.
pub(crate) fn register_local(st: &StateRef, tag: [u8; 16], addr: PeerAddr) {
    let mut s = lock(st);
    let Ok(key) = s.core.admit(Ipv4Addr::UNSPECIFIED.into()) else {
        return;
    };
    if let Ok(affected) = s.core.register(key, Instant::now(), tag, addr) {
        notify(&s, &affected);
    }
}

pub(crate) fn source_ip(a: &IncomingAddr) -> IpAddr {
    match a {
        IncomingAddr::Ip(a) => a.ip(),
        _ => Ipv4Addr::UNSPECIFIED.into(),
    }
}

/// Unregisters (and tells the tag) when dropped.
struct Admitted(StateRef, ConnKey);

impl Drop for Admitted {
    fn drop(&mut self) {
        let mut s = lock(&self.0);
        s.out.remove(&self.1);
        let affected = s.core.close(self.1);
        notify(&s, &affected);
    }
}

fn refuse(conn: &Connection, r: Refused) -> String {
    match r {
        Refused::RateLimited(d) => {
            let secs = (d.as_secs() + 1).to_string();
            conn.close(CLOSE_RETRY_LATER.into(), secs.as_bytes());
        }
        Refused::Protocol => conn.close(CLOSE_AUTH.into(), b""),
        _ => conn.close(CLOSE_FULL.into(), b""),
    }
    format!("{r:?}")
}

/// Admit a connection that is already established (embedded server).
pub(crate) async fn serve_new(st: StateRef, conn: Connection, ip: IpAddr) -> Result<(), String> {
    let key = lock(&st).core.admit(ip);
    match key {
        Ok(key) => serve(st, key, conn).await,
        Err(r) => Err(refuse(&conn, r)),
    }
}

/// One admitted connection until it ends.
pub(crate) async fn serve(st: StateRef, key: ConnKey, conn: Connection) -> Result<(), String> {
    let _admitted = Admitted(st.clone(), key);
    let limits = lock(&st).core.limits.clone();
    let first = timeout(limits.register_timeout, async {
        let (send, mut recv) = conn.accept_bi().await.map_err(|e| e.to_string())?;
        let (ty, body) = rt::read_raw(&mut recv, max_body).await?;
        Ok::<_, String>((send, recv, Msg::decode_body(ty, &body)))
    })
    .await;
    let note = |f: fn(&mut Refusals) -> &mut u64| *f(&mut lock(&st).core.refused) += 1;
    let (mut send, mut recv, msg) = match first {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            note(|r| &mut r.register_timeout);
            conn.close(CLOSE_AUTH.into(), b"");
            return Err("no REGISTER".into());
        }
    };
    let Ok(Msg::Register { tag, mut addr }) = msg else {
        note(|r| &mut r.protocol);
        return Err(refuse(&conn, Refused::Protocol));
    };
    if addr.id != *conn.remote_id().as_bytes() {
        note(|r| &mut r.id_mismatch);
        return Err(refuse(&conn, Refused::Protocol));
    }
    // Where we see it: its NAT mapping, useful for hole punching.
    if let Some(TransportAddr::Ip(a)) = conn
        .paths()
        .iter()
        .find(|p| p.is_selected())
        .map(|p| p.remote_addr().clone())
        && !addr.addrs.contains(&a)
    {
        addr.addrs.truncate(MAX_ADDRS - 1);
        addr.addrs.push(a);
    }
    let (tx, mut rx) = watch::channel(vec![]);
    {
        let mut s = lock(&st);
        let affected = match s.core.register(key, Instant::now(), tag, addr) {
            Ok(a) => a,
            Err(r) => return Err(refuse(&conn, r)),
        };
        s.out.insert(key, tx);
        notify(&s, &affected);
    }
    let reader = async {
        let sec = Duration::from_secs(1);
        let (mut win, mut n, mut over, mut flood) = (Instant::now(), 0u32, false, 0);
        loop {
            let Ok(r) = timeout(limits.idle, rt::read_raw(&mut recv, max_body)).await else {
                note(|r| &mut r.idle);
                return Err::<(), _>("idle".into());
            };
            let (ty, body) = r?;
            let now = Instant::now();
            if now.duration_since(win) >= sec {
                if !over {
                    flood = 0;
                }
                (win, n, over) = (now, 0, false);
            }
            n += 1;
            if !over && n > FRAMES_PER_SEC {
                over = true;
                flood += 1;
                if flood >= rt::FLOOD_SECONDS {
                    note(|r| &mut r.flood);
                    return Err(refuse(&conn, Refused::Protocol));
                }
                tokio::time::sleep_until((win + sec).into()).await;
            }
            match Msg::decode_body(ty, &body) {
                Ok(Msg::Ping) | Ok(Msg::Unknown(_)) => {}
                _ => {
                    note(|r| &mut r.protocol);
                    return Err(refuse(&conn, Refused::Protocol));
                }
            }
        }
    };
    let writer = async {
        loop {
            rx.changed().await.map_err(|e| e.to_string())?;
            let peers = rx.borrow_and_update().clone();
            write(&mut send, &Msg::Peers(peers)).await?;
            tokio::time::sleep(limits.fanout_interval).await;
        }
    };
    let r = tokio::select! {
        r = reader => r,
        r = writer => r,
    };
    conn.close(0u32.into(), b"bye");
    r
}

pub(crate) async fn write(w: &mut SendStream, m: &Msg) -> Result<(), String> {
    let mut b = Vec::new();
    m.encode(&mut b).map_err(|e| e.to_string())?;
    w.write_all(&b).await.map_err(|e| rt::chain(&e))
}

pub(crate) async fn read(r: &mut RecvStream) -> Result<Msg, String> {
    let (ty, body) = rt::read_raw(r, max_body).await?;
    Msg::decode_body(ty, &body).map_err(|e| e.to_string())
}

/// A standalone rendezvous server on its own iroh endpoint and `sb-rdv` thread (what a server
/// binary wraps). Stops when dropped.
pub struct RendezvousServer {
    addr: Arc<Mutex<PeerAddr>>,
    st: StateRef,
    stop: Option<oneshot::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl RendezvousServer {
    /// Bind `bind_ip:port` (0 = ephemeral) with identity `secret_key` (persist it: clients
    /// need the server's endpoint id) and serve. When `port` is taken it falls back to an
    /// ephemeral one, as a client does; see [`start_strict`](Self::start_strict).
    pub fn start(
        bind_ip: IpAddr,
        port: u16,
        secret_key: Option<[u8; 32]>,
        relay: RelayChoice,
        limits: Limits,
    ) -> std::io::Result<RendezvousServer> {
        Self::start_on(bind_ip, port, false, secret_key, relay, limits)
    }

    /// [`start`](Self::start), but a `port` that cannot be bound is an error ("cannot bind UDP
    /// <ip:port>: <cause>"), never an ephemeral port: for a server whose port is part of the
    /// address users have.
    pub fn start_strict(
        bind_ip: IpAddr,
        port: u16,
        secret_key: Option<[u8; 32]>,
        relay: RelayChoice,
        limits: Limits,
    ) -> std::io::Result<RendezvousServer> {
        Self::start_on(bind_ip, port, true, secret_key, relay, limits)
    }

    fn start_on(
        bind_ip: IpAddr,
        port: u16,
        port_required: bool,
        secret_key: Option<[u8; 32]>,
        relay: RelayChoice,
        limits: Limits,
    ) -> std::io::Result<RendezvousServer> {
        let relay = rt::relay_mode(&relay)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let (ready_tx, ready_rx) = smpsc::channel();
        let (stop, stop_rx) = oneshot::channel();
        let addr = Arc::new(Mutex::new(PeerAddr {
            id: [0; 32],
            addrs: vec![],
            relay: None,
        }));
        let addr2 = addr.clone();
        let st = state(limits);
        let st2 = st.clone();
        let thread = std::thread::Builder::new()
            .name("sb-rdv".into())
            .spawn(move || {
                let rt = match tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                {
                    Ok(rt) => rt,
                    Err(e) => return drop(ready_tx.send(Err(e.to_string()))),
                };
                rt.block_on(async move {
                    let bound =
                        rt::bind(bind_ip, port, port_required, secret_key, relay, RDV_ALPN).await;
                    let ep = match bound {
                        Ok(ep) => ep,
                        Err(e) => return drop(ready_tx.send(Err(e))),
                    };
                    *lock(&addr2) = rt::local_peer(&ep, bind_ip);
                    let _ = ready_tx.send(Ok(()));
                    run_server(ep, bind_ip, st2, addr2, stop_rx).await;
                });
                rt.shutdown_timeout(rt::CLOSE_TIMEOUT);
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(RendezvousServer {
                addr,
                st,
                stop: Some(stop),
                thread: Some(thread),
            }),
            r => {
                let _ = thread.join();
                let msg = r.map_or_else(|_| "rendezvous thread died".into(), |r| r.unwrap_err());
                Err(std::io::Error::other(msg))
            }
        }
    }

    /// Where clients reach it (endpoint id, addresses, home relay), refreshed as `Net`'s.
    pub fn addr(&self) -> PeerAddr {
        lock(&self.addr).clone()
    }

    /// Connections, sources, tags, registrations and cumulative refusals (one brief lock).
    pub fn counts(&self) -> Counts {
        lock(&self.st).core.stats()
    }
}

impl Drop for RendezvousServer {
    fn drop(&mut self) {
        let _ = self.stop.take().map(|s| s.send(()));
        let _ = self.thread.take().map(|t| t.join());
    }
}

async fn run_server(
    ep: Endpoint,
    bind_ip: IpAddr,
    st: StateRef,
    addr: Arc<Mutex<PeerAddr>>,
    mut stop: oneshot::Receiver<()>,
) {
    let mut tasks = tokio::task::JoinSet::new();
    let mut refresh = tokio::time::interval(rt::ADDR_REFRESH);
    loop {
        tokio::select! {
            _ = &mut stop => break,
            _ = refresh.tick() => *lock(&addr) = rt::local_peer(&ep, bind_ip),
            Some(_) = tasks.join_next() => {}
            inc = ep.accept() => {
                let Some(inc) = inc else { break };
                let inc = if inc.remote_addr_validated() {
                    inc
                } else {
                    match inc.retry() {
                        Ok(()) => continue,
                        Err(e) => e.into_incoming(),
                    }
                };
                // Connection caps before any TLS work.
                let key = lock(&st).core.admit(source_ip(&inc.remote_addr()));
                let Ok(key) = key else {
                    inc.refuse();
                    continue;
                };
                let st = st.clone();
                tasks.spawn(async move {
                    let conn = timeout(rt::AUTH_TIMEOUT, async { inc.accept().ok()?.await.ok() });
                    match conn.await {
                        Ok(Some(c)) => drop(serve(st, key, c).await),
                        _ => {
                            lock(&st).core.refused.handshake += 1;
                            drop(Admitted(st, key))
                        }
                    }
                });
            }
        }
    }
    tasks.abort_all();
    let _ = timeout(rt::CLOSE_TIMEOUT, ep.close()).await;
}

// ---------------------------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn peer(i: u8) -> PeerAddr {
        PeerAddr {
            id: [i; 32],
            addrs: vec![SocketAddr::from(([10, 0, 0, i], 10999))],
            relay: None,
        }
    }

    #[test]
    fn frames_golden_and_round_trip() {
        let mut b = Vec::new();
        Msg::Register {
            tag: [0xaa; 16],
            addr: PeerAddr {
                id: [1; 32],
                addrs: vec!["10.0.0.1:258".parse().unwrap()],
                relay: Some("http://r/".into()),
            },
        }
        .encode(&mut b)
        .unwrap();
        let mut want = vec![0, 0, T_REGISTER];
        want.extend_from_slice(&[0xaa; 16]);
        want.extend_from_slice(&[1; 32]);
        want.extend_from_slice(&[1, 4, 10, 0, 0, 1, 2, 1]);
        want.extend_from_slice(&[9, b'h', b't', b't', b'p', b':', b'/', b'/', b'r', b'/']);
        want[0] = (want.len() - 3) as u8;
        assert_eq!(b, want);
        let mut p = Vec::new();
        Msg::Ping.encode(&mut p).unwrap();
        assert_eq!(p, [0, 0, T_PING]);
        let all = [
            Msg::Register {
                tag: [3; 16],
                addr: peer(4),
            },
            Msg::Peers(vec![]),
            Msg::Peers((0..MAX_LISTED as u8).map(peer).collect()),
            Msg::Ping,
        ];
        for m in all {
            let mut b = Vec::new();
            m.encode(&mut b).unwrap();
            assert!(b.len() - 3 <= max_body(b[2]), "{m:?}");
            assert_eq!(Msg::decode_body(b[2], &b[3..]), Ok(m.clone()));
            for n in 0..b.len() - 3 {
                assert!(Msg::decode_body(b[2], &b[3..3 + n]).is_err(), "{m:?} {n}");
            }
            // trailing garbage is refused
            let mut long = b[3..].to_vec();
            long.push(0);
            assert!(Msg::decode_body(b[2], &long).is_err());
        }
        assert_eq!(
            Msg::Peers((0..MAX_LISTED as u8 + 1).map(peer).collect()).encode(&mut Vec::new()),
            Err(Error::TooLong)
        );
        assert_eq!(Msg::decode_body(9, &[1, 2]), Ok(Msg::Unknown(9)));
    }

    fn lim() -> Limits {
        Limits {
            per_ip: 3,
            per_tag: 4,
            listed: 2,
            register_burst: 3,
            register_refill: Duration::from_secs(10),
            ..Limits::default()
        }
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 1, 0, n))
    }

    #[test]
    fn core_lists_others_oldest_first_and_unregisters_on_close() {
        let mut c = Core::new(lim());
        let t = Instant::now();
        let (a, b, x) = (
            c.admit(ip(1)).unwrap(),
            c.admit(ip(2)).unwrap(),
            c.admit(ip(3)).unwrap(),
        );
        assert_eq!(c.register(a, t, [1; 16], peer(1)), Ok(vec![a]));
        assert_eq!(c.register(x, t, [2; 16], peer(9)), Ok(vec![x]));
        assert_eq!(c.register(b, t, [1; 16], peer(2)), Ok(vec![a, b]));
        assert_eq!((c.peers(a), c.peers(b)), (vec![peer(2)], vec![peer(1)]));
        assert!(c.peers(x).is_empty(), "other tags are never listed");
        // Listing cap: oldest first.
        let d = c.admit(ip(4)).unwrap();
        let e = c.admit(ip(5)).unwrap();
        c.register(d, t, [1; 16], peer(4)).unwrap();
        c.register(e, t, [1; 16], peer(5)).unwrap();
        assert_eq!(c.peers(e), vec![peer(1), peer(2)]);
        assert_eq!(c.peers(a), vec![peer(2), peer(4)]);
        // Per-tag cap.
        let f = c.admit(ip(6)).unwrap();
        assert_eq!(c.register(f, t, [1; 16], peer(6)), Err(Refused::TagFull));
        let s = c.stats();
        assert_eq!((s.connections, s.sources, s.tags, s.registrations), (6, 6, 2, 5));
        assert_eq!(
            s.refused,
            Refusals {
                tag_full: 1,
                ..Refusals::default()
            }
        );
        // Close: the rest of the tag is told; maps shrink back to nothing.
        assert_eq!(c.close(a), vec![b, d, e]);
        assert_eq!(c.peers(e), vec![peer(2), peer(4)]);
        assert!(c.close(a).is_empty());
        for k in [b, d, e, f, x] {
            c.close(k);
        }
        assert_eq!(c.counts(), (0, 0, 0));
        assert_eq!(c.stats().registrations, 0);
    }

    #[test]
    fn core_limits() {
        let mut c = Core::new(lim());
        let t = Instant::now();
        // One registration per connection.
        let a = c.admit(ip(1)).unwrap();
        c.register(a, t, [1; 16], peer(1)).unwrap();
        assert_eq!(c.register(a, t, [2; 16], peer(1)), Err(Refused::Protocol));
        // Per-source connections (IPv6 /64 counts as one source).
        let v6 = |s: &str| s.parse::<IpAddr>().unwrap();
        for i in 0..3 {
            assert!(c.admit(v6(&format!("2001:db8::{i}"))).is_ok());
        }
        assert_eq!(c.admit(v6("2001:db8::ff")), Err(Refused::PerIp));
        assert!(c.admit(v6("2001:db8:0:1::1")).is_ok());
        // REGISTER rate per source: burst 3 (one used above), then one per 10 s.
        let mut keys = vec![];
        for _ in 0..2 {
            let k = c.admit(ip(1)).unwrap();
            c.register(k, t, [5; 16], peer(1)).unwrap();
            keys.push(k);
        }
        for k in keys.drain(..) {
            c.close(k);
        }
        let k = c.admit(ip(1)).unwrap();
        assert_eq!(
            c.register(k, t + Duration::from_secs(3), [5; 16], peer(1)),
            Err(Refused::RateLimited(Duration::from_secs(7)))
        );
        assert!(c.register(k, t + Duration::from_secs(10), [5; 16], peer(1)).is_ok());
        assert_eq!(
            c.stats().refused,
            Refusals {
                protocol: 1,
                per_ip: 1,
                rate_limited: 1,
                ..Refusals::default()
            }
        );
        // Global connection cap.
        let mut c = Core::new(Limits {
            max_connections: 2,
            ..lim()
        });
        c.admit(ip(1)).unwrap();
        c.admit(ip(2)).unwrap();
        assert_eq!(c.admit(ip(3)), Err(Refused::Busy));
        assert_eq!(c.stats().refused.busy, 1);
    }

    /// The REGISTER rate-limit state has a hard bound (4 x the connection cap): full of
    /// buckets still in use, it refuses new sources (known ones are still served) until
    /// buckets replenish and are dropped.
    #[test]
    fn rate_limit_state_is_bounded() {
        let mut c = Core::new(Limits {
            max_connections: 2,
            ..lim()
        });
        let t = Instant::now();
        let reg = |c: &mut Core, n: u8, at: Instant| {
            let k = c.admit(ip(n)).unwrap();
            let r = c.register(k, at, [1; 16], peer(n));
            c.close(k);
            r.map(|_| ())
        };
        for n in 0..8 {
            assert_eq!(reg(&mut c, n, t), Ok(()));
        }
        assert!(matches!(reg(&mut c, 100, t), Err(Refused::RateLimited(_))));
        assert_eq!(c.buckets.len(), 8);
        assert_eq!(reg(&mut c, 0, t), Ok(()), "a known source");
        // burst 3, one token per 10 s: replenished after 20 s at most
        assert_eq!(reg(&mut c, 101, t + Duration::from_secs(30)), Ok(()));
        assert!(c.buckets.len() <= 8);
    }
}
