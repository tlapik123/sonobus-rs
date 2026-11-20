//! Connection ("rendezvous") server of the legacy AOO net protocol (`server.cpp`): sans-IO
//! [`Server`] over connection ids, plus a std driver [`run`] (one UDP socket and a TCP listener
//! on the same port, a reader and a writer thread per TCP connection).
//!
//! Hardened beyond the C++ for exposure to the internet, all bounds in [`Limits`]: connection
//! caps (global and per IP), login and idle deadlines, length caps on names and passwords, group
//! caps (global, created per IP, joined per connection), rate-limited group joins, coalesced
//! public group updates, and a per-IP byte budget for public group lists. "Per IP" means per
//! IPv4 address or per IPv6 /64 (one host usually owns a whole /64).
//!
//! Trust model of the relayed addresses (as in the C++): a member's public and local address are
//! the ones its login claims. The server cannot check them: the UDP address it saw belongs to
//! another socket, and an honest client's TCP and UDP traffic may leave through different public
//! IPs (carrier NAT, multi-WAN, a phone with TCP on cellular and UDP on Wi-Fi, TCP-only proxies),
//! so substituting the TCP peer IP would break those clients. Consequence: a group member can make
//! every other member send handshake pings to two addresses of its choice: one 28-byte
//! `/aoo/peer/ping` to each every 100 ms for 5 s (`client.hpp:20-21`), i.e. at most ~51 x 2
//! datagrams per member per join, and joins are rate-limited. This is inherent to the legacy
//! protocol; only a group password limits who can do it. The new AOO protocol fixes it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::{self, Read};
use std::net::{IpAddr, Ipv6Addr, Shutdown, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::client::{THREAD_STACK_SIZE, TcpWriter, lock, to_family, unmap};
use crate::message::{self, Message};
use crate::{MAX_PACKET_SIZE, slip};

/// Identifies one TCP connection.
pub type ConnId = u64;

/// Not in the C++: longest user or group name in bytes ("... name too long"). SonoBus' UI has no
/// limit; real names are a few dozen characters. With it a `/aoo/client/group/public/add` is at
/// most 172 bytes, 239 SLIP-framed (a UTF-8 name can need at most every other byte escaped).
pub const MAX_NAME_LEN: usize = 128;
/// Not in the C++: longest password in bytes ("password too long"). Stock clients send a 32
/// character MD5 hex digest.
pub const MAX_PASSWORD_LEN: usize = 64;
/// Longest IP string in a login (`INET6_ADDRSTRLEN` is 46 incl. NUL, plus a zone id).
const MAX_IP_LEN: usize = 64;
/// Default [`Limits::login_timeout`]. Stock clients log in after the UDP handshake, which itself
/// gives up after 5 s.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(10);
/// Default [`Limits::idle_timeout`]: no complete valid message for this long (stock clients ping
/// over TCP every 10 s; the C++ uses TCP keepalive with a 29 s timeout, `server.cpp:911-954`).
pub const IDLE_TIMEOUT: Duration = Duration::from_secs(30);
/// Driver: a TCP write blocked this long drops that connection (it stopped reading).
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);

/// Token bucket: up to `burst` at once, refilled at `per_sec`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate {
    pub burst: u32,
    pub per_sec: u32,
}

impl Rate {
    /// No limit (tests, trusted networks).
    pub const UNLIMITED: Self = Self {
        burst: u32::MAX,
        per_sec: u32::MAX,
    };
}

/// Server limits. Two presets: [`Limits::embedded`] (the [`Default`], used by [`run`]) for a
/// session hosted by a phone or desktop app, [`Limits::standalone`] for a dedicated server.
///
/// Memory: per connection the driver holds at most `send_queue_bytes` of queued output (one
/// contiguous buffer whose growth is clamped to that; frame overhead does not exist), an 8 KiB
/// write chunk and 4 KiB read buffer on the thread stacks, a 4 KiB SLIP decoder, two thread stacks
/// of 128 KiB address space each (a few KiB touched), and the kernel's socket buffers, which std
/// cannot size (Linux autotunes the send buffer up to `net.ipv4.tcp_wmem`, 4 MiB by default). A
/// watcher also remembers what it was told, at most `max_groups` x 24 bytes. File descriptors:
/// one per connection (reader, writer and shutdown share it), plus the UDP socket and listener.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Logged-in users are a subset of the connections, so this also caps the user table.
    pub max_connections: usize,
    pub max_connections_per_ip: usize,
    /// From [`Server::open`] to a successful login.
    pub login_timeout: Duration,
    /// Since the last complete, parseable message, once logged in.
    pub idle_timeout: Duration,
    /// Groups on the server ("too many groups on server").
    pub max_groups: usize,
    /// Groups created by connections from one IP that still exist ("too many groups from your
    /// address"); a group counts against its creator until it is empty.
    pub max_groups_per_ip: usize,
    /// Groups one connection is in at once ("too many groups").
    pub max_groups_per_conn: usize,
    /// Driver: a connection with more unsent bytes than this is dropped (it does not read).
    /// Must hold the biggest burst of output: a full public group list (`max_groups` x 239
    /// bytes) or the peer joins of one group join (up to ~600 bytes per member).
    pub send_queue_bytes: usize,
    /// Group join requests per connection ("too many requests, try again later"). Each join
    /// fans out to the group's members and the public group watchers; a leave needs a join
    /// first, so this bounds leaves too.
    pub join_rate: Rate,
    /// Group join requests of all connections from one IP.
    pub join_rate_per_ip: Rate,
    /// Bytes of public group list traffic (full lists and updates) toward one IP. A watcher
    /// waits while its IP is over budget; the list it then gets is the current state, so
    /// waiting only coalesces more. One list may overdraw the budget.
    pub list_rate_per_ip: Rate,
    /// Public group changes reach watchers at most this often (latest counts only). Stock
    /// clients treat `public/add` as an upsert, so they cannot tell.
    pub public_update_interval: Duration,
}

impl Limits {
    /// A session hosted by a phone or desktop app: 32 connections (34 fds, well below the
    /// 256-fd soft limit of macOS/iOS), at most ~32 x (64 + 16) KiB = 2.5 MiB of buffers.
    /// 8 per IP fits a band behind one NAT; 32 groups is plenty for one host's sessions, a
    /// quarter of them per IP; a SonoBus client is in one group at a time, 4 leaves room for
    /// other AOO clients and a join overlapping the previous leave. Joins: a burst of 4 per
    /// connection (8 per IP) for a human clicking around, then 1/s (2/s per IP). List budget:
    /// the worst list is 32 x 239 = 7.6 KB, so 16 KiB covers two at once, then 4 KiB/s.
    pub const fn embedded() -> Self {
        Self {
            max_connections: 32,
            max_connections_per_ip: 8,
            login_timeout: LOGIN_TIMEOUT,
            idle_timeout: IDLE_TIMEOUT,
            max_groups: 32,
            max_groups_per_ip: 8,
            max_groups_per_conn: 4,
            send_queue_bytes: 64 * 1024,
            join_rate: Rate {
                burst: 4,
                per_sec: 1,
            },
            join_rate_per_ip: Rate {
                burst: 8,
                per_sec: 2,
            },
            list_rate_per_ip: Rate {
                burst: 16 * 1024,
                per_sec: 4 * 1024,
            },
            public_update_interval: Duration::from_secs(1),
        }
    }

    /// A dedicated server: 256 connections (258 fds, 512 threads), 16 per IP, so it takes 16
    /// hosts to fill it. 512 groups, 32 created per IP (16 IPs to exhaust them). Queue: the
    /// worst public list is 512 x 239 = 122 KB and the worst peer-join burst ~154 KB, so 192
    /// KiB; true worst case 256 x (192 + 16) KiB = 52 MiB plus kernel socket buffers. List
    /// budget: one worst list at once, then 32 KiB/s per IP.
    pub const fn standalone() -> Self {
        Self {
            max_connections: 256,
            max_connections_per_ip: 16,
            max_groups: 512,
            max_groups_per_ip: 32,
            send_queue_bytes: 192 * 1024,
            list_rate_per_ip: Rate {
                burst: 128 * 1024,
                per_sec: 32 * 1024,
            },
            ..Self::embedded()
        }
    }
}

impl Default for Limits {
    fn default() -> Self {
        Self::embedded()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// SLIP-framed bytes for this connection.
    Tcp(ConnId, Vec<u8>),
    Udp(SocketAddr, Vec<u8>),
    /// Close this TCP connection (limit hit); the server has already forgotten it.
    Close(ConnId),
}

#[derive(Debug, Clone, Copy)]
struct Bucket {
    tokens: f64,
    at: Instant,
}

impl Bucket {
    fn new(r: Rate, now: Instant) -> Self {
        Self {
            tokens: r.burst.into(),
            at: now,
        }
    }

    /// Refill up to `now`; true if `cost` (at most a full bucket) can be taken.
    fn ready(&mut self, r: Rate, cost: f64, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.at).as_secs_f64();
        self.tokens = (self.tokens + dt * f64::from(r.per_sec)).min(r.burst.into());
        self.at = self.at.max(now);
        self.tokens >= cost.min(r.burst.into())
    }
}

/// Everything accounted per IP (IPv4 address or IPv6 /64).
#[derive(Debug)]
struct IpState {
    conns: usize,
    /// Existing groups created from this IP.
    groups: usize,
    joins: Bucket,
    lists: Bucket,
}

#[derive(Debug)]
struct Conn {
    /// [`ip_key`] of the TCP peer.
    ip: IpAddr,
    opened: Instant,
    last_frame: Instant,
    joins: Bucket,
}

/// A watcher of public groups.
#[derive(Debug, Default)]
struct Watch {
    /// (name, count) of every public group it was told about, sorted.
    known: Vec<(Arc<str>, i32)>,
    /// `known` is the current state (as of the last coalesced update).
    synced: bool,
}

#[derive(Debug)]
struct User {
    name: String,
    public_ip: String,
    public_port: i32,
    local_ip: String,
    local_port: i32,
    token: i64,
    watch: Option<Watch>,
    groups: Vec<String>,
}

#[derive(Debug)]
struct Group {
    name: Arc<str>,
    password: String,
    public: bool,
    members: Vec<ConnId>,
    /// [`ip_key`] of the connection that created it.
    creator: IpAddr,
}

fn frame(m: &Message) -> Vec<u8> {
    let mut raw = Vec::new();
    m.encode(&mut raw);
    let mut framed = Vec::with_capacity(raw.len() + 8);
    slip::encode(&raw, &mut framed);
    framed
}

fn unmap_ip(ip: IpAddr) -> IpAddr {
    unmap(SocketAddr::new(ip, 0)).ip()
}

/// Per-IP accounting key: the IPv4 address (also when IPv4-mapped), or the IPv6 /64 prefix.
fn ip_key(ip: IpAddr) -> IpAddr {
    match unmap_ip(ip) {
        IpAddr::V6(v6) => Ipv6Addr::from(u128::from(v6) & (u128::MAX << 64)).into(),
        v4 => v4,
    }
}

/// A claimed public IP worth relaying: parses, and is not multicast, broadcast or unspecified.
fn public_ip(s: &str) -> Option<IpAddr> {
    let ip = unmap_ip(s.parse().ok()?);
    let bad = match ip {
        IpAddr::V4(a) => a.is_multicast() || a.is_broadcast() || a.is_unspecified(),
        IpAddr::V6(a) => a.is_multicast() || a.is_unspecified(),
    };
    (!bad).then_some(ip)
}

/// `public/add` for every group in `now` that `known` lacks or counts differently, `public/del`
/// for every group in `known` that `now` lacks. Both sorted by name.
fn public_diff(known: &[(Arc<str>, i32)], now: &[(Arc<str>, i32)]) -> Vec<Vec<u8>> {
    let find = |v: &[(Arc<str>, i32)], name: &Arc<str>| {
        v.binary_search_by(|(n, _)| n.cmp(name))
            .ok()
            .map(|i| v[i].1)
    };
    let adds = now.iter().filter(|(n, c)| find(known, n) != Some(*c)).map(|(n, c)| {
        frame(&Message::ClientGroupPublicAdd {
            group: n,
            users: *c,
        })
    });
    let dels = known
        .iter()
        .filter(|(n, _)| find(now, n).is_none())
        .map(|(n, _)| frame(&Message::ClientGroupPublicDel { group: n }));
    adds.chain(dels).collect()
}

/// Cumulative connections dropped and requests refused since the server started, by reason.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Refusals {
    /// Connections refused: [`Limits::max_connections`].
    pub global_cap: u64,
    /// Connections refused: [`Limits::max_connections_per_ip`].
    pub per_ip_cap: u64,
    /// Connections closed: no login within [`Limits::login_timeout`].
    pub login_timeout: u64,
    /// Connections closed: [`Limits::idle_timeout`].
    pub idle_timeout: u64,
    /// Connections shut down because their send queue was full (counted once per connection).
    pub send_queue_full: u64,
    /// Group joins refused by [`Limits::join_rate`] / [`Limits::join_rate_per_ip`].
    pub join_rate_limited: u64,
    /// Group creations refused: [`Limits::max_groups`].
    pub groups_cap: u64,
    /// Group creations refused: [`Limits::max_groups_per_ip`].
    pub groups_per_ip_cap: u64,
    /// Group joins refused: [`Limits::max_groups_per_conn`].
    pub groups_per_conn_cap: u64,
    /// Logins refused: bad public/local address or port.
    pub bad_address: u64,
    /// Logins and joins refused: user or group name too long.
    pub name_too_long: u64,
    /// Logins and joins refused: password too long.
    pub password_too_long: u64,
}

/// [`ServerHandle::stats`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Stats {
    /// Open TCP connections (logged in or not).
    pub connections: usize,
    pub users: usize,
    pub groups: usize,
    pub refused: Refusals,
}

#[derive(Debug, Default)]
pub struct Server {
    limits: Limits,
    refused: Refusals,
    conns: BTreeMap<ConnId, Conn>,
    /// Logged-in connections. Users exist only while logged in, as in the C++ (`server.cpp:682-688`).
    users: BTreeMap<ConnId, User>,
    groups: Vec<Group>,
    /// Every IP with a connection or a group, or with a bucket not yet refilled.
    ips: HashMap<IpAddr, IpState>,
    /// Public groups changed since watchers were last updated.
    public_dirty: bool,
    last_public_update: Option<Instant>,
    out: VecDeque<Output>,
}

impl Server {
    /// [`Limits::embedded`].
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_limits(limits: Limits) -> Self {
        Self {
            limits,
            ..Self::default()
        }
    }

    pub fn poll_output(&mut self) -> Option<Output> {
        self.out.pop_front()
    }

    pub fn user_count(&self) -> usize {
        self.users.len()
    }

    pub fn group_count(&self) -> usize {
        self.groups.len()
    }

    /// Open TCP connections (logged in or not).
    pub fn connection_count(&self) -> usize {
        self.conns.len()
    }

    pub fn refusals(&self) -> Refusals {
        self.refused
    }

    fn send(&mut self, to: ConnId, m: &Message) {
        self.out.push_back(Output::Tcp(to, frame(m)));
    }

    /// A TCP connection from `peer` was accepted. `false`: over a connection limit, close it
    /// (and do not call [`Server::close`]). Frames from connections not opened are ignored.
    pub fn open(&mut self, conn: ConnId, peer: IpAddr, now: Instant) -> bool {
        let ip = ip_key(peer);
        let l = self.limits;
        if self.conns.contains_key(&conn) {
            return false;
        }
        if self.conns.len() >= l.max_connections {
            self.refused.global_cap += 1;
            return false;
        }
        let s = self.ips.entry(ip).or_insert_with(|| IpState {
            conns: 0,
            groups: 0,
            joins: Bucket::new(l.join_rate_per_ip, now),
            lists: Bucket::new(l.list_rate_per_ip, now),
        });
        if s.conns >= l.max_connections_per_ip {
            self.refused.per_ip_cap += 1;
            return false;
        }
        s.conns += 1;
        let c = Conn {
            ip,
            opened: now,
            last_frame: now,
            joins: Bucket::new(l.join_rate, now),
        };
        self.conns.insert(conn, c);
        true
    }

    /// Deadlines, coalesced public group updates and lists waiting for budget; call every
    /// 100 ms or so.
    pub fn poll(&mut self, now: Instant) {
        let l = self.limits;
        let expired: Vec<ConnId> = self
            .conns
            .iter()
            .filter(|(id, c)| match self.users.contains_key(id) {
                true => now.duration_since(c.last_frame) > l.idle_timeout,
                false => now.duration_since(c.opened) > l.login_timeout,
            })
            .map(|(&id, _)| id)
            .collect();
        for id in expired {
            match self.users.contains_key(&id) {
                true => self.refused.idle_timeout += 1,
                false => self.refused.login_timeout += 1,
            }
            self.close(id);
            self.out.push_back(Output::Close(id));
        }
        // a full bucket is the same as a new one
        self.ips.retain(|_, s| {
            s.conns > 0
                || s.groups > 0
                || !s.joins.ready(l.join_rate_per_ip, f64::INFINITY, now)
                || !s.lists.ready(l.list_rate_per_ip, f64::INFINITY, now)
        });
        self.update_watchers(now);
    }

    /// One datagram on the server's UDP socket (`server.cpp:778-808`).
    /// Amplification: the reply is at most 44 bytes for an IPv4 sender (68 for IPv6) against a
    /// 24-byte request, so 1.8x (2.8x). Do not add fields to it.
    pub fn handle_udp(&mut self, from: SocketAddr, packet: &[u8]) {
        let from = unmap(from);
        let reply = match message::parse(packet) {
            Ok(Message::ServerPing) => Message::ClientPing,
            Ok(Message::ServerRequest) => Message::ClientReply {
                public_ip: &from.ip().to_string(),
                public_port: from.port().into(),
            },
            _ => return,
        };
        let mut b = Vec::new();
        reply.encode(&mut b);
        self.out.push_back(Output::Udp(from, b));
    }

    /// One decoded SLIP frame from connection `conn` (`server.cpp:1110-1145`).
    pub fn handle_tcp_frame(&mut self, conn: ConnId, packet: &[u8], now: Instant) {
        let Some(c) = self.conns.get_mut(&conn) else {
            return;
        };
        // bad or unknown messages are logged and dropped by the C++; they do not count as activity
        let Ok(msg) = message::parse(packet) else {
            return;
        };
        c.last_frame = now;
        match msg {
            Message::ServerPing => self.send(conn, &Message::ClientPing),
            Message::ServerLogin {
                user,
                password,
                public_ip: claimed_ip,
                public_port,
                local_ip,
                local_port,
                token,
            } => {
                let port_ok = |p: i32| (1..=65535).contains(&p);
                // The C++ also compares the password of an existing *logged-out* user, but
                // users are purged on logout, so "wrong password" never comes from login.
                let public = (claimed_ip.len() <= MAX_IP_LEN)
                    .then(|| public_ip(claimed_ip))
                    .flatten();
                let err = if self.users.contains_key(&conn) {
                    "already logged in"
                } else if user.len() > MAX_NAME_LEN {
                    "user name too long"
                } else if password.len() > MAX_PASSWORD_LEN {
                    "password too long"
                } else if local_ip.len() > MAX_IP_LEN
                    || public.is_none()
                    || !port_ok(public_port)
                    || !port_ok(local_port)
                {
                    "bad address"
                } else if self.users.values().any(|u| u.name == user) {
                    "access denied"
                } else {
                    let u = User {
                        name: user.to_owned(),
                        // the claim, as the C++ does: see the trust model at the top
                        public_ip: public.expect("checked above").to_string(),
                        public_port,
                        local_ip: local_ip.to_owned(),
                        local_port,
                        token: token.unwrap_or(0),
                        watch: None,
                        groups: Vec::new(),
                    };
                    self.users.insert(conn, u);
                    ""
                };
                self.count_refusal(err);
                let result = err.is_empty().into();
                self.send(
                    conn,
                    &Message::ClientLogin {
                        result,
                        error: Some(err),
                    },
                );
            }
            Message::ServerGroupJoin {
                group,
                password,
                is_public,
            } => {
                let err = self.join(conn, group, password, is_public.unwrap_or(false), now);
                self.count_refusal(err.err().unwrap_or(""));
                let m = Message::ClientGroupJoin {
                    group,
                    result: err.is_ok().into(),
                    error: Some(err.err().unwrap_or("")),
                };
                self.send(conn, &m);
                self.update_watchers(now);
            }
            Message::ServerGroupLeave { group } => {
                let err = self.leave(conn, group);
                let m = Message::ClientGroupLeave {
                    group,
                    result: err.is_ok().into(),
                    error: Some(err.err().unwrap_or("")),
                };
                self.send(conn, &m);
                self.update_watchers(now);
            }
            Message::ServerGroupPublic { watch } => {
                let error = match self.users.get_mut(&conn) {
                    Some(u) => {
                        // a new watch gets the full list (the client clears its list)
                        u.watch = watch.then(Watch::default);
                        ""
                    }
                    None => "not logged in",
                };
                self.update_watchers(now);
                // result is always 0, even on success (`server.cpp:1283-1308`)
                let m = Message::ClientGroupPublic {
                    watch,
                    result: 0,
                    error: Some(error),
                };
                self.send(conn, &m);
            }
            _ => {}
        }
    }

    /// Count a refused login or join by its reply text (the ones [`Refusals`] tracks).
    fn count_refusal(&mut self, err: &str) {
        let r = &mut self.refused;
        let n = match err {
            "user name too long" | "group name too long" => &mut r.name_too_long,
            "password too long" => &mut r.password_too_long,
            "bad address" => &mut r.bad_address,
            "too many requests, try again later" => &mut r.join_rate_limited,
            "too many groups" => &mut r.groups_per_conn_cap,
            "too many groups on server" => &mut r.groups_cap,
            "too many groups from your address" => &mut r.groups_per_ip_cap,
            _ => return,
        };
        *n += 1;
    }

    /// Bring watchers up to date: after a change once [`Limits::public_update_interval`] has
    /// passed since the last update (else [`Server::poll`] does it later), and a new watcher right
    /// away; each one only when its IP has list budget left (else a later call retries).
    fn update_watchers(&mut self, now: Instant) {
        let l = self.limits;
        if self.public_dirty
            && self
                .last_public_update
                .is_none_or(|t| now.duration_since(t) >= l.public_update_interval)
        {
            self.public_dirty = false;
            self.last_public_update = Some(now);
            for w in self.users.values_mut().filter_map(|u| u.watch.as_mut()) {
                w.synced = false;
            }
        }
        let behind = |u: &User| u.watch.as_ref().is_some_and(|w| !w.synced);
        if !self.users.values().any(behind) {
            return;
        }
        let mut state: Vec<(Arc<str>, i32)> = self
            .groups
            .iter()
            .filter(|g| g.public)
            .map(|g| (g.name.clone(), g.members.len() as i32))
            .collect();
        state.sort();
        for (id, u) in &mut self.users {
            let Some(w) = u.watch.as_mut().filter(|w| !w.synced) else {
                continue;
            };
            let frames = public_diff(&w.known, &state);
            let cost = frames.iter().map(Vec::len).sum::<usize>() as f64;
            let ip = self.conns[id].ip;
            let budget = &mut self.ips.get_mut(&ip).expect("open() adds it").lists;
            if !budget.ready(l.list_rate_per_ip, cost, now) {
                continue;
            }
            budget.tokens -= cost;
            self.out
                .extend(frames.into_iter().map(|f| Output::Tcp(*id, f)));
            w.known.clone_from(&state);
            w.synced = true;
        }
    }

    fn join(
        &mut self,
        conn: ConnId,
        name: &str,
        pw: &str,
        public: bool,
        now: Instant,
    ) -> Result<(), &'static str> {
        let l = self.limits;
        let user = self.users.get_mut(&conn).ok_or("not logged in")?;
        let c = self.conns.get_mut(&conn).expect("users are open");
        let ip = self.ips.get_mut(&c.ip).expect("open() adds it");
        if !(c.joins.ready(l.join_rate, 1.0, now) && ip.joins.ready(l.join_rate_per_ip, 1.0, now)) {
            return Err("too many requests, try again later");
        }
        c.joins.tokens -= 1.0;
        ip.joins.tokens -= 1.0;
        if name.len() > MAX_NAME_LEN {
            return Err("group name too long");
        }
        if pw.len() > MAX_PASSWORD_LEN {
            return Err("password too long");
        }
        let existing = self.groups.iter().position(|g| &*g.name == name);
        if let Some(g) = existing.map(|i| &self.groups[i]) {
            if g.public != public {
                return Err("permission denied");
            }
            if g.password != pw {
                return Err("wrong password");
            }
        }
        if user.groups.iter().any(|g| g == name) {
            return Err("already a group member");
        }
        if user.groups.len() >= l.max_groups_per_conn {
            return Err("too many groups");
        }
        if existing.is_none() {
            if self.groups.len() >= l.max_groups {
                return Err("too many groups on server");
            }
            if ip.groups >= l.max_groups_per_ip {
                return Err("too many groups from your address");
            }
            ip.groups += 1;
        }
        user.groups.push(name.to_owned());
        let gi = existing.unwrap_or_else(|| {
            self.groups.push(Group {
                name: name.into(),
                password: pw.to_owned(),
                public,
                members: Vec::new(),
                creator: c.ip,
            });
            self.groups.len() - 1
        });
        // peer/join both ways for every existing member (`server.cpp:398-426`)
        let others = self.groups[gi].members.clone();
        self.groups[gi].members.push(conn);
        for other in others {
            let (to_new, to_old) = (
                frame(&self.peer_join(name, other)),
                frame(&self.peer_join(name, conn)),
            );
            self.out.push_back(Output::Tcp(conn, to_new));
            self.out.push_back(Output::Tcp(other, to_old));
        }
        self.public_dirty |= public;
        Ok(())
    }

    fn peer_join<'a>(&'a self, group: &'a str, conn: ConnId) -> Message<'a> {
        let u = &self.users[&conn];
        Message::ClientPeerJoin {
            group,
            user: &u.name,
            public_ip: &u.public_ip,
            public_port: u.public_port,
            local_ip: &u.local_ip,
            local_port: u.local_port,
            token: Some(u.token),
        }
    }

    fn leave(&mut self, conn: ConnId, name: &str) -> Result<(), &'static str> {
        let user = self.users.get_mut(&conn).ok_or("not logged in")?;
        let gi = self
            .groups
            .iter()
            .position(|g| &*g.name == name)
            .ok_or("couldn't find group")?;
        let i = user
            .groups
            .iter()
            .position(|g| g == name)
            .ok_or("not a group member")?;
        user.groups.remove(i);
        self.left_group(conn, gi);
        self.purge();
        Ok(())
    }

    /// Remove `conn` from group `gi` and tell the rest (`server.cpp:437-460`).
    fn left_group(&mut self, conn: ConnId, gi: usize) {
        self.groups[gi].members.retain(|&c| c != conn);
        let user = self.users[&conn].name.clone();
        let group = self.groups[gi].name.clone();
        for other in self.groups[gi].members.clone() {
            let m = Message::ClientPeerLeave {
                group: &group,
                user: &user,
            };
            self.send(other, &m);
        }
        self.public_dirty |= self.groups[gi].public;
    }

    /// Drop empty groups (`server.cpp:689-701`); watchers hear of public ones with the next
    /// update. The C++ only runs this after a disconnect or a *public* group leave, so an empty
    /// private group (and its password) lingers until then; we purge right away.
    fn purge(&mut self) {
        let ips = &mut self.ips;
        let mut dirty = false;
        self.groups.retain(|g| {
            if !g.members.is_empty() {
                return true;
            }
            dirty |= g.public;
            if let Some(s) = ips.get_mut(&g.creator) {
                s.groups -= 1;
            }
            false
        });
        self.public_dirty |= dirty;
    }

    /// The TCP connection is gone: leave all groups and log out (`server.cpp:820-832`).
    pub fn close(&mut self, conn: ConnId) {
        let Some(c) = self.conns.remove(&conn) else {
            return;
        };
        if let Some(s) = self.ips.get_mut(&c.ip) {
            s.conns -= 1;
        }
        let Some(user) = self.users.get_mut(&conn) else {
            return;
        };
        user.watch = None;
        for name in std::mem::take(&mut user.groups) {
            if let Some(gi) = self.groups.iter().position(|g| *g.name == *name) {
                self.left_group(conn, gi);
            }
        }
        self.users.remove(&conn);
        self.purge();
    }
}

/// Driver side of one connection.
struct ConnIo {
    writer: TcpWriter,
    /// The socket (shared with the reader and writer thread), for `shutdown`.
    stream: Arc<TcpStream>,
    /// A send failed: being shut down, send nothing more.
    overflowed: bool,
}

struct Shared {
    server: Server,
    conns: HashMap<ConnId, ConnIo>,
}

struct Ctx {
    state: Mutex<Shared>,
    udp: UdpSocket,
    /// Bound address (UDP and TCP share the port).
    local: SocketAddr,
    stop: AtomicBool,
    send_queue_bytes: usize,
}

/// Execute pending outputs and release the lock. No socket write happens under the lock: TCP
/// bytes go to the connection's writer thread (a connection whose queue is full is shut down,
/// so it stalls nobody but itself) and UDP is sent after unlocking. `shutdown` does not block.
fn flush(ctx: &Ctx, mut sh: MutexGuard<'_, Shared>) {
    let mut udp = Vec::new();
    while let Some(o) = sh.server.poll_output() {
        match o {
            Output::Tcp(id, bytes) => {
                let sh = &mut *sh;
                if let Some(c) = sh.conns.get_mut(&id)
                    && !c.overflowed
                    && c.writer.send(&bytes).is_err()
                {
                    c.overflowed = true;
                    if !c.writer.closed() {
                        sh.server.refused.send_queue_full += 1;
                    }
                    let _ = c.stream.shutdown(Shutdown::Both); // its reader thread cleans up
                }
            }
            Output::Close(id) => {
                if let Some(c) = sh.conns.get(&id) {
                    let _ = c.stream.shutdown(Shutdown::Both);
                }
            }
            Output::Udp(to, bytes) => udp.push((to_family(to, ctx.local), bytes)),
        }
    }
    drop(sh);
    for (to, bytes) in udp {
        let _ = ctx.udp.send_to(&bytes, to);
    }
}

/// A running server; dropping it stops and joins all threads.
pub struct ServerHandle {
    ctx: Arc<Ctx>,
    threads: Vec<JoinHandle<()>>,
}

impl ServerHandle {
    /// Bound address (UDP and TCP share the port).
    pub fn local_addr(&self) -> SocketAddr {
        self.ctx.local
    }

    /// (logged-in users, groups)
    pub fn counts(&self) -> (usize, usize) {
        let sh = lock(&self.ctx.state);
        (sh.server.user_count(), sh.server.group_count())
    }

    /// Connections, users, groups and cumulative refusals (one brief lock).
    pub fn stats(&self) -> Stats {
        let sh = lock(&self.ctx.state);
        Stats {
            connections: sh.server.connection_count(),
            users: sh.server.user_count(),
            groups: sh.server.group_count(),
            refused: sh.server.refusals(),
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        self.ctx.stop.store(true, Ordering::SeqCst);
        for c in lock(&self.ctx.state).conns.values() {
            let _ = c.stream.shutdown(Shutdown::Both);
        }
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

/// Bind UDP, then TCP on the same port (retrying a few OS-chosen ports if `bind` has port 0).
fn bind_pair(bind: SocketAddr) -> io::Result<(UdpSocket, TcpListener)> {
    let mut tries = if bind.port() == 0 { 10 } else { 1 };
    loop {
        let udp = match bind {
            SocketAddr::V4(_) => UdpSocket::bind(bind)?,
            SocketAddr::V6(_) => bind_dual_stack(bind, socket2::Type::DGRAM)?.into(),
        };
        let at = udp.local_addr()?;
        let tcp = match at {
            SocketAddr::V4(_) => TcpListener::bind(at),
            SocketAddr::V6(_) => bind_dual_stack(at, socket2::Type::STREAM).map(Into::into),
        };
        match tcp {
            Ok(tcp) => return Ok((udp, tcp)),
            Err(e) if tries > 1 && e.kind() == io::ErrorKind::AddrInUse => tries -= 1,
            Err(e) => return Err(e),
        }
    }
}

/// An IPv6 socket bound with IPV6_V6ONLY off, so IPv4 clients reach it as `::ffff:a.b.c.d`.
/// Linux defaults to that (`net.ipv6.bindv6only=0`), Windows to v6-only; std cannot set it.
/// A stream socket is also put in the listening state, as `TcpListener::bind` does.
fn bind_dual_stack(addr: SocketAddr, ty: socket2::Type) -> io::Result<socket2::Socket> {
    let s = socket2::Socket::new(socket2::Domain::IPV6, ty, None)?;
    s.set_only_v6(false)?;
    let stream = ty == socket2::Type::STREAM;
    // what std's TcpListener::bind does on Unix (rebinding while old connections linger)
    #[cfg(unix)]
    if stream {
        s.set_reuse_address(true)?;
    }
    s.bind(&addr.into())?;
    if stream {
        s.listen(128)?;
    }
    Ok(s)
}

fn spawn(name: &str, f: impl FnOnce() + Send + 'static) -> io::Result<JoinHandle<()>> {
    thread::Builder::new()
        .name(name.into())
        .stack_size(THREAD_STACK_SIZE)
        .spawn(f)
}

/// [`run_with`] [`Limits::embedded`]: the server a phone or desktop app hosts. A dedicated
/// server should use `run_with(bind, Limits::standalone())`.
pub fn run(bind: SocketAddr) -> io::Result<ServerHandle> {
    run_with(bind, Limits::embedded())
}

/// Start a server on `bind` (SonoBus' built-in server uses port 10999, the public one 10998).
/// An IPv6 `bind` (`[::]:port`) is dual-stack on every OS: IPv4 clients reach it too.
pub fn run_with(bind: SocketAddr, limits: Limits) -> io::Result<ServerHandle> {
    let (udp, listener) = bind_pair(bind)?;
    udp.set_read_timeout(Some(Duration::from_millis(100)))?;
    listener.set_nonblocking(true)?;
    let ctx = Arc::new(Ctx {
        state: Mutex::new(Shared {
            server: Server::with_limits(limits),
            conns: HashMap::new(),
        }),
        local: udp.local_addr()?,
        udp,
        stop: AtomicBool::new(false),
        send_queue_bytes: limits.send_queue_bytes,
    });
    // built first, so a failed spawn below stops the threads already running
    let mut h = ServerHandle {
        ctx: ctx.clone(),
        threads: Vec::new(),
    };
    let c = ctx.clone();
    h.threads.push(spawn("aoo-server-udp", move || {
        let mut buf = [0u8; MAX_PACKET_SIZE];
        while !c.stop.load(Ordering::SeqCst) {
            if let Ok((n, from)) = c.udp.recv_from(&mut buf) {
                let mut sh = lock(&c.state);
                sh.server.handle_udp(from, &buf[..n]);
                flush(&c, sh);
            }
        }
    })?);
    h.threads.push(spawn("aoo-server-accept", move || {
        accept_loop(&ctx, &listener)
    })?);
    Ok(h)
}

fn accept_loop(ctx: &Arc<Ctx>, listener: &TcpListener) {
    let mut readers: Vec<JoinHandle<()>> = Vec::new();
    let mut next_id: ConnId = 0;
    let mut next_poll = Instant::now();
    while !ctx.stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= next_poll {
            next_poll = now + Duration::from_millis(100);
            let mut sh = lock(&ctx.state);
            sh.server.poll(now);
            flush(ctx, sh);
        }
        let (stream, peer) = match listener.accept() {
            Ok(x) => x,
            Err(_) => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
        };
        readers.retain(|h| !h.is_finished());
        let id = next_id;
        next_id += 1;
        // refused: dropping `stream` closes it
        if lock(&ctx.state).server.open(id, peer.ip(), now) {
            readers.extend(start_conn(id, stream, ctx));
        }
    }
    // a connection may have been added after Drop's shutdown sweep
    for c in lock(&ctx.state).conns.values() {
        let _ = c.stream.shutdown(Shutdown::Both);
    }
    for r in readers {
        let _ = r.join();
    }
}

/// Writer and reader thread for an opened connection; on failure the connection is closed.
/// Both threads and the shutdown handle share the one socket: one fd per connection.
fn start_conn(id: ConnId, stream: TcpStream, ctx: &Arc<Ctx>) -> Option<JoinHandle<()>> {
    let stream = Arc::new(stream);
    let setup = || -> io::Result<_> {
        stream.set_nonblocking(false)?;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(WRITE_TIMEOUT))?;
        // backstop for the reader's exit checks; `shutdown` normally wakes it at once
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        TcpWriter::spawn(
            "aoo-server-tx",
            stream.clone(),
            ctx.send_queue_bytes,
            (),
            drop,
        )
    };
    let started = setup().and_then(|writer| {
        let io = ConnIo {
            writer,
            stream: stream.clone(),
            overflowed: false,
        };
        lock(&ctx.state).conns.insert(id, io);
        let c = ctx.clone();
        spawn("aoo-server-rx", move || serve_conn(id, &stream, &c))
    });
    match started {
        Ok(h) => Some(h),
        Err(_) => {
            end_conn(id, ctx);
            None
        }
    }
}

fn serve_conn(id: ConnId, mut stream: &TcpStream, ctx: &Ctx) {
    // Frames over MAX_PACKET_SIZE are dropped whole, so a hostile client cannot grow memory.
    let mut dec = slip::Decoder::new(MAX_PACKET_SIZE);
    let mut buf = [0u8; MAX_PACKET_SIZE];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => dec.feed(&buf[..n], |f| {
                let mut sh = lock(&ctx.state);
                sh.server.handle_tcp_frame(id, f, Instant::now());
                flush(ctx, sh);
            }),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                // read timeout: go on unless the server forgot us (limit) or is stopping
                if ctx.stop.load(Ordering::SeqCst)
                    || !lock(&ctx.state).server.conns.contains_key(&id)
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    end_conn(id, ctx);
}

/// Forget the connection and wait for its writer thread.
fn end_conn(id: ConnId, ctx: &Ctx) {
    let mut sh = lock(&ctx.state);
    let io = sh.conns.remove(&id);
    sh.server.close(id);
    flush(ctx, sh);
    if let Some(io) = io {
        let _ = io.stream.shutdown(Shutdown::Both);
        io.writer.join();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::password_hash;

    fn tcp_out(s: &mut Server) -> Vec<(ConnId, Vec<u8>)> {
        let mut v = Vec::new();
        while let Some(o) = s.poll_output() {
            if let Output::Tcp(c, b) = o {
                let mut d = slip::Decoder::new(MAX_PACKET_SIZE);
                d.feed(&b, |f| v.push((c, f.to_vec())));
            }
        }
        v
    }

    /// Decoded replies as (conn, message) with owned bytes kept alive by the caller.
    fn msgs(v: &[(ConnId, Vec<u8>)]) -> Vec<(ConnId, Message<'_>)> {
        v.iter()
            .map(|(c, f)| (*c, message::parse(f).unwrap()))
            .collect()
    }

    fn send(s: &mut Server, c: ConnId, m: Message) {
        send_at(s, c, m, Instant::now());
    }

    /// Opens `c` from 1.1.1.1 on first use.
    fn send_at(s: &mut Server, c: ConnId, m: Message, now: Instant) {
        if !s.conns.contains_key(&c) {
            assert!(s.open(c, [1, 1, 1, 1].into(), now));
        }
        let mut b = Vec::new();
        m.encode(&mut b);
        s.handle_tcp_frame(c, &b, now);
    }

    fn login_msg(user: &str, c: ConnId) -> Message<'_> {
        Message::ServerLogin {
            user,
            password: "D41D8CD98F00B204E9800998ECF8427E",
            public_ip: "1.1.1.1",
            public_port: 1000 + c as i32,
            local_ip: "10.0.0.1",
            local_port: 2000 + c as i32,
            token: Some(100 + c as i64),
        }
    }

    fn login_err(s: &mut Server, c: ConnId, m: Message) -> String {
        send(s, c, m);
        let out = tcp_out(s);
        match msgs(&out).pop() {
            Some((_, Message::ClientLogin { error, .. })) => error.unwrap().to_owned(),
            other => panic!("{other:?}"),
        }
    }

    fn login(s: &mut Server, c: ConnId, user: &str) {
        assert_eq!(password_hash(""), "D41D8CD98F00B204E9800998ECF8427E");
        send(s, c, login_msg(user, c));
    }

    fn join(s: &mut Server, c: ConnId, group: &str, pw: &str, public: bool) {
        let m = Message::ServerGroupJoin {
            group,
            password: pw,
            is_public: Some(public),
        };
        send(s, c, m);
    }

    fn join_reply<'a>(group: &'a str, err: &'a str) -> Message<'a> {
        Message::ClientGroupJoin {
            group,
            result: err.is_empty().into(),
            error: Some(err),
        }
    }

    #[test]
    fn udp_request_and_ping() {
        let mut s = Server::new();
        let from: SocketAddr = "[::ffff:5.6.7.8]:4321".parse().unwrap();
        let mut b = Vec::new();
        Message::ServerRequest.encode(&mut b);
        s.handle_udp(from, &b);
        b.clear();
        Message::ServerPing.encode(&mut b);
        s.handle_udp(from, &b);
        s.handle_udp(from, b"/sb/ping\0\0\0\0,\0\0\0");
        let to: SocketAddr = "5.6.7.8:4321".parse().unwrap();
        let mut reply = Vec::new();
        Message::ClientReply {
            public_ip: "5.6.7.8",
            public_port: 4321,
        }
        .encode(&mut reply);
        let mut ping = Vec::new();
        Message::ClientPing.encode(&mut ping);
        assert_eq!(s.poll_output(), Some(Output::Udp(to, reply)));
        assert_eq!(s.poll_output(), Some(Output::Udp(to, ping)));
        assert_eq!(s.poll_output(), None);
    }

    #[test]
    fn login_rules() {
        let mut s = Server::new();
        send(&mut s, 1, Message::ServerPing);
        login(&mut s, 1, "alice");
        login(&mut s, 1, "alice2");
        login(&mut s, 2, "alice");
        login(&mut s, 3, "bob");
        let out = tcp_out(&mut s);
        let ok = Message::ClientLogin {
            result: 1,
            error: Some(""),
        };
        let fail = |e| Message::ClientLogin {
            result: 0,
            error: Some(e),
        };
        assert_eq!(
            msgs(&out),
            [
                (1, Message::ClientPing),
                (1, ok),
                (1, fail("already logged in")),
                (2, fail("access denied")),
                (3, ok),
            ]
        );
        // name is free again after the connection closes
        s.close(1);
        login(&mut s, 2, "alice");
        assert_eq!(msgs(&tcp_out(&mut s)), [(2, ok)]);
    }

    #[test]
    fn group_rules_and_peer_fanout() {
        let mut s = Server::new();
        join(&mut s, 9, "g", "pw", false);
        send(&mut s, 9, Message::ServerGroupLeave { group: "g" });
        login(&mut s, 1, "alice");
        login(&mut s, 2, "bob");
        login(&mut s, 3, "carol");
        tcp_out(&mut s);
        let out = tcp_out(&mut s);
        assert!(out.is_empty());

        join(&mut s, 1, "g", "pw", false);
        join(&mut s, 1, "g", "pw", false);
        join(&mut s, 3, "g", "nope", false);
        join(&mut s, 3, "g", "pw", true);
        join(&mut s, 2, "g", "pw", false);
        let out = tcp_out(&mut s);
        let alice_join = Message::ClientPeerJoin {
            group: "g",
            user: "alice",
            public_ip: "1.1.1.1",
            public_port: 1001,
            local_ip: "10.0.0.1",
            local_port: 2001,
            token: Some(101),
        };
        let bob_join = Message::ClientPeerJoin {
            group: "g",
            user: "bob",
            public_ip: "1.1.1.1",
            public_port: 1002,
            local_ip: "10.0.0.1",
            local_port: 2002,
            token: Some(102),
        };
        assert_eq!(
            msgs(&out),
            [
                (1, join_reply("g", "")),
                (1, join_reply("g", "already a group member")),
                (3, join_reply("g", "wrong password")),
                (3, join_reply("g", "permission denied")),
                // peer/join both ways before the join reply (`server.cpp:1225-1245`)
                (2, alice_join),
                (1, bob_join),
                (2, join_reply("g", "")),
            ]
        );

        let leave = |g| Message::ServerGroupLeave { group: g };
        let leave_reply = |e: &'static str| Message::ClientGroupLeave {
            group: "g",
            result: e.is_empty().into(),
            error: Some(e),
        };
        send(&mut s, 3, leave("g"));
        send(&mut s, 3, leave("nope"));
        send(&mut s, 2, leave("g"));
        let out = tcp_out(&mut s);
        let mut nope = leave_reply("couldn't find group");
        if let Message::ClientGroupLeave { group, .. } = &mut nope {
            *group = "nope";
        }
        assert_eq!(
            msgs(&out),
            [
                (3, leave_reply("not a group member")),
                (3, nope),
                (
                    1,
                    Message::ClientPeerLeave {
                        group: "g",
                        user: "bob"
                    }
                ),
                (2, leave_reply("")),
            ]
        );
        // join before login
        let mut s2 = Server::new();
        join(&mut s2, 9, "g", "pw", false);
        send(&mut s2, 9, leave("g"));
        send(&mut s2, 9, Message::ServerGroupPublic { watch: true });
        let out = tcp_out(&mut s2);
        assert_eq!(
            msgs(&out),
            [
                (9, join_reply("g", "not logged in")),
                (
                    9,
                    Message::ClientGroupLeave {
                        group: "g",
                        result: 0,
                        error: Some("not logged in")
                    }
                ),
                (
                    9,
                    Message::ClientGroupPublic {
                        watch: true,
                        result: 0,
                        error: Some("not logged in")
                    }
                ),
            ]
        );
    }

    #[test]
    fn empty_private_group_is_purged() {
        let mut s = Server::new();
        login(&mut s, 1, "alice");
        join(&mut s, 1, "g", "pw", false);
        send(&mut s, 1, Message::ServerGroupLeave { group: "g" });
        assert_eq!(s.group_count(), 0);
        join(&mut s, 1, "g", "other", true);
        tcp_out(&mut s);
        assert_eq!(s.group_count(), 1);
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn open_from(s: &mut Server, c: ConnId, ip: &str, now: Instant) {
        assert!(s.open(c, ip.parse().unwrap(), now), "open {c} from {ip}");
    }

    fn join_at(s: &mut Server, c: ConnId, group: &str, public: bool, now: Instant) {
        let m = Message::ServerGroupJoin {
            group,
            password: "",
            is_public: Some(public),
        };
        send_at(s, c, m, now);
    }

    fn leave_at(s: &mut Server, c: ConnId, group: &str, now: Instant) {
        send_at(s, c, Message::ServerGroupLeave { group }, now);
    }

    fn watch_at(s: &mut Server, c: ConnId, watch: bool, now: Instant) {
        send_at(s, c, Message::ServerGroupPublic { watch }, now);
    }

    /// Join replies to `c`, as (group, error).
    fn join_errors(out: &[(ConnId, Vec<u8>)], c: ConnId) -> Vec<(String, String)> {
        msgs(out)
            .into_iter()
            .filter_map(|(to, m)| match m {
                Message::ClientGroupJoin { group, error, .. } if to == c => {
                    Some((group.to_owned(), error.unwrap().to_owned()))
                }
                _ => None,
            })
            .collect()
    }

    fn add(group: &str, users: i32) -> Message<'_> {
        Message::ClientGroupPublicAdd { group, users }
    }

    /// Public list messages to `c`.
    fn list_msgs(out: &[(ConnId, Vec<u8>)], c: ConnId) -> Vec<Message<'_>> {
        msgs(out)
            .into_iter()
            .filter(|(to, m)| {
                *to == c
                    && matches!(
                        m,
                        Message::ClientGroupPublicAdd { .. } | Message::ClientGroupPublicDel { .. }
                    )
            })
            .map(|(_, m)| m)
            .collect()
    }

    fn unlimited_joins() -> Limits {
        Limits {
            join_rate: Rate::UNLIMITED,
            join_rate_per_ip: Rate::UNLIMITED,
            ..Limits::embedded()
        }
    }

    #[test]
    fn public_updates_are_coalesced() {
        let mut s = Server::with_limits(unlimited_joins());
        let t0 = Instant::now();
        let t = |ms| at(t0, ms);
        send_at(&mut s, 1, login_msg("alice", 1), t0);
        send_at(&mut s, 2, login_msg("bob", 2), t0);
        send_at(&mut s, 3, login_msg("watcher", 3), t0);
        join_at(&mut s, 1, "pub", true, t0);
        join_at(&mut s, 1, "priv", false, t0);
        tcp_out(&mut s);
        // a new watcher gets the list at once, before the reply
        watch_at(&mut s, 3, true, t0);
        let out = tcp_out(&mut s);
        let reply = Message::ClientGroupPublic {
            watch: true,
            result: 0,
            error: Some(""),
        };
        assert_eq!(msgs(&out), [(3, add("pub", 1)), (3, reply)]);
        // changes within the interval reach watchers once, with the latest count; groups
        // that come and go meanwhile not at all
        for i in 0..50 {
            join_at(&mut s, 2, "pub", true, t(10 + i));
            leave_at(&mut s, 2, "pub", t(10 + i));
            join_at(&mut s, 2, "flash", true, t(10 + i));
            leave_at(&mut s, 2, "flash", t(10 + i));
        }
        join_at(&mut s, 2, "pub", true, t(100));
        assert!(list_msgs(&tcp_out(&mut s), 3).is_empty());
        s.poll(t(999));
        assert!(list_msgs(&tcp_out(&mut s), 3).is_empty());
        s.poll(t(1000));
        assert_eq!(list_msgs(&tcp_out(&mut s), 3), [add("pub", 2)]);
        s.poll(t(5000));
        assert!(tcp_out(&mut s).is_empty());
        // bob's TCP drops: alice hears peer/leave at once
        s.close(2);
        let out = tcp_out(&mut s);
        let bye = Message::ClientPeerLeave {
            group: "pub",
            user: "bob",
        };
        assert_eq!(msgs(&out), [(1, bye)]);
        // alice drops too: the count change and the deletion are one del
        s.close(1);
        s.poll(t(5000));
        let out = tcp_out(&mut s);
        assert_eq!(
            list_msgs(&out, 3),
            [Message::ClientGroupPublicDel { group: "pub" }]
        );
        assert_eq!((s.user_count(), s.group_count()), (1, 0));
        // unwatching stops updates
        watch_at(&mut s, 3, false, t(6000));
        tcp_out(&mut s);
        send_at(&mut s, 4, login_msg("dave", 4), t(6000));
        join_at(&mut s, 4, "pub2", true, t(6000));
        s.poll(t(9000));
        assert!(tcp_out(&mut s).iter().all(|(c, _)| *c != 3));
        // per-IP state goes away with the last connection, once its buckets are full
        s.close(3);
        s.close(4);
        s.poll(t(60_000));
        assert!(s.ips.is_empty(), "{:?}", s.ips);
    }

    #[test]
    fn join_rate_limits() {
        let mut s = Server::new();
        let t0 = Instant::now();
        let busy = "too many requests, try again later";
        for c in [1, 2, 4] {
            send_at(&mut s, c, login_msg(&format!("u{c}"), c), t0);
        }
        open_from(&mut s, 3, "2.2.2.2", t0);
        send_at(&mut s, 3, login_msg("other", 3), t0);
        tcp_out(&mut s);
        // a connection's burst is 4; leaves are free
        for _ in 0..5 {
            join_at(&mut s, 1, "g", false, t0);
            leave_at(&mut s, 1, "g", t0);
        }
        let errs = join_errors(&tcp_out(&mut s), 1);
        assert_eq!(errs.iter().filter(|(_, e)| e.is_empty()).count(), 4);
        assert_eq!(errs[4].1, busy);
        // then one per second
        join_at(&mut s, 1, "g", false, at(t0, 999));
        join_at(&mut s, 1, "h", false, at(t0, 1000));
        let errs = join_errors(&tcp_out(&mut s), 1);
        assert_eq!(errs, [("g".into(), busy.into()), ("h".into(), "".into())]);
        // the IP's burst of 8 is shared: 4 of conn 1 and 1 refill, so 4 + 1 more
        let t1 = at(t0, 1000);
        for g in ["a", "b", "c", "d"] {
            join_at(&mut s, 2, g, false, t1);
        }
        join_at(&mut s, 4, "e", false, t1);
        join_at(&mut s, 4, "f", false, t1);
        let out = tcp_out(&mut s);
        assert!(join_errors(&out, 2).iter().all(|(_, e)| e.is_empty()));
        assert_eq!(
            join_errors(&out, 4),
            [("e".into(), "".into()), ("f".into(), busy.into())]
        );
        // another IP is not affected
        join_at(&mut s, 3, "g", false, t1);
        assert_eq!(
            join_errors(&tcp_out(&mut s), 3),
            [("g".into(), "".into())]
        );
        assert_eq!(
            s.refusals(),
            Refusals {
                join_rate_limited: 3,
                ..Refusals::default()
            }
        );
    }

    #[test]
    fn group_caps() {
        let mut s = Server::with_limits(Limits {
            max_groups: 6,
            max_groups_per_ip: 4,
            ..unlimited_joins()
        });
        let t0 = Instant::now();
        send_at(&mut s, 1, login_msg("a1", 1), t0);
        send_at(&mut s, 2, login_msg("a2", 2), t0);
        // IPv6 addresses of one /64 are one IP
        open_from(&mut s, 3, "2001:db8::1", t0);
        open_from(&mut s, 4, "2001:db8::2", t0);
        send_at(&mut s, 3, login_msg("b1", 3), t0);
        send_at(&mut s, 4, login_msg("b2", 4), t0);
        tcp_out(&mut s);
        for g in ["a0", "a1", "a2", "a3", "a4"] {
            join_at(&mut s, 1, g, false, t0);
        }
        assert_eq!(join_errors(&tcp_out(&mut s), 1)[4].1, "too many groups");
        join_at(&mut s, 2, "a4", false, t0);
        join_at(&mut s, 2, "a0", false, t0); // joining an existing group is fine
        let mine = "too many groups from your address";
        assert_eq!(
            join_errors(&tcp_out(&mut s), 2),
            [("a4".into(), mine.into()), ("a0".into(), "".into())]
        );
        join_at(&mut s, 3, "b0", false, t0);
        join_at(&mut s, 4, "b1", false, t0);
        join_at(&mut s, 4, "b2", false, t0);
        let out = tcp_out(&mut s);
        assert_eq!(join_errors(&out, 3), [("b0".into(), "".into())]);
        assert_eq!(
            join_errors(&out, 4),
            [
                ("b1".into(), "".into()),
                ("b2".into(), "too many groups on server".into())
            ]
        );
        // below the global cap again; a group counts against its creator until it is gone
        // (a0 still has a member)
        leave_at(&mut s, 3, "b0", t0);
        leave_at(&mut s, 1, "a0", t0);
        join_at(&mut s, 2, "a5", false, t0);
        leave_at(&mut s, 1, "a1", t0);
        join_at(&mut s, 2, "a5", false, t0);
        let errs = join_errors(&tcp_out(&mut s), 2);
        assert_eq!(errs, [("a5".into(), mine.into()), ("a5".into(), "".into())]);
        assert_eq!(s.group_count(), 5);
        assert_eq!(
            s.refusals(),
            Refusals {
                groups_cap: 1,
                groups_per_ip_cap: 2,
                groups_per_conn_cap: 1,
                ..Refusals::default()
            }
        );
    }

    #[test]
    fn connection_limits() {
        let mut s = Server::with_limits(Limits {
            max_connections: 6,
            max_connections_per_ip: 2,
            ..Limits::default()
        });
        let t = Instant::now();
        let a: IpAddr = [5, 5, 5, 5].into();
        let a6: IpAddr = "::ffff:5.5.5.5".parse().unwrap();
        assert!(s.open(1, a, t));
        assert!(s.open(2, a6, t));
        assert!(
            !s.open(3, a, t),
            "per-IP cap, IPv4-mapped counts as the same IP"
        );
        assert!(!s.open(2, [6, 6, 6, 6].into(), t), "id in use");
        assert!(s.open(3, [6, 6, 6, 6].into(), t));
        // one IPv6 /64 is one IP
        let v6 = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(s.open(4, v6("2001:db8::1"), t));
        assert!(s.open(5, v6("2001:db8::ffff:1"), t));
        s.close(5);
        assert!(s.open(5, v6("2001:db8::ffff:1"), t));
        assert!(!s.open(6, v6("2001:db8:0:0:8000::1"), t), "same /64");
        s.close(5);
        assert!(s.open(5, v6("2001:db8:0:1::1"), t), "next /64");
        assert!(s.open(6, [7, 7, 7, 7].into(), t));
        assert!(!s.open(7, [8, 8, 8, 8].into(), t), "global cap");
        s.close(1);
        assert!(s.open(7, a, t));
        assert_eq!(s.connection_count(), 6);
        assert_eq!(
            s.refusals(),
            Refusals {
                global_cap: 1,
                per_ip_cap: 2,
                ..Refusals::default()
            }
        );
    }
    #[test]
    fn login_and_idle_deadlines() {
        let mut s = Server::new();
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        assert!(s.open(1, [1, 1, 1, 1].into(), t0));
        assert!(s.open(2, [1, 1, 1, 1].into(), t0));
        // conn 1 dribbles garbage and never logs in; conn 2 logs in
        s.handle_tcp_frame(1, b"garbage", at(5000));
        send_at(&mut s, 2, login_msg("bob", 2), at(1000));
        tcp_out(&mut s);
        s.poll(at(10_000));
        assert_eq!(s.poll_output(), None);
        s.poll(at(10_001));
        assert_eq!(s.poll_output(), Some(Output::Close(1)));
        assert_eq!(s.poll_output(), None);
        // a closed connection is ignored, even a login
        let mut b = Vec::new();
        login_msg("alice", 1).encode(&mut b);
        s.handle_tcp_frame(1, &b, at(20_000));
        assert_eq!(s.poll_output(), None);
        assert_eq!(s.user_count(), 1);
        send_at(&mut s, 2, Message::ServerPing, at(20_000));
        tcp_out(&mut s);
        // idle time counts from the last parseable message, not from any bytes
        s.handle_tcp_frame(2, b"/aoo/server/ping\0\0\0\0", at(45_000));
        s.poll(at(50_000));
        assert_eq!(s.poll_output(), None);
        s.poll(at(50_001));
        assert_eq!(s.poll_output(), Some(Output::Close(2)));
        assert_eq!(s.user_count(), 0);
        assert!(s.conns.is_empty());
        assert_eq!(
            s.refusals(),
            Refusals {
                login_timeout: 1,
                idle_timeout: 1,
                ..Refusals::default()
            }
        );
    }

    #[test]
    fn name_password_and_address_caps() {
        let mut s = Server::new();
        let long = "x".repeat(MAX_NAME_LEN + 1);
        let max = "x".repeat(MAX_NAME_LEN);
        assert_eq!(
            login_err(&mut s, 1, login_msg(&long, 1)),
            "user name too long"
        );
        let mut m = login_msg("a", 1);
        if let Message::ServerLogin { password, .. } = &mut m {
            *password = &long[..MAX_PASSWORD_LEN + 1];
        }
        assert_eq!(login_err(&mut s, 1, m), "password too long");
        let bad_public = [
            "not-an-ip",
            "224.0.0.1",
            "255.255.255.255",
            "0.0.0.0",
            "::",
            "::ffff:0.0.0.0",
            "ff02::1",
            &long,
        ];
        let cases = [(0, 1, "1.2.3.4", "1.1.1.1"), (65536, 1, "1.2.3.4", "1.1.1.1")]
            .into_iter()
            .chain([(1, -1, "1.2.3.4", "1.1.1.1"), (1, 1, &*long, "1.1.1.1")])
            .chain(bad_public.map(|p| (1, 1, "1.2.3.4", p)));
        for (pp, lp, lip, pip) in cases {
            let mut m = login_msg("a", 1);
            if let Message::ServerLogin {
                public_ip,
                public_port,
                local_port,
                local_ip,
                ..
            } = &mut m
            {
                (*public_ip, *public_port, *local_port, *local_ip) = (pip, pp, lp, lip);
            }
            assert_eq!(login_err(&mut s, 1, m), "bad address", "{pp} {lp} {pip}");
        }
        assert_eq!(s.user_count(), 0);
        assert_eq!(login_err(&mut s, 1, login_msg(&max, 1)), "");
        join(&mut s, 1, &long, "", false);
        join(&mut s, 1, "g", &long[..MAX_PASSWORD_LEN + 1], false);
        let out = tcp_out(&mut s);
        assert_eq!(
            msgs(&out),
            [
                (1, join_reply(&long, "group name too long")),
                (1, join_reply("g", "password too long")),
            ]
        );
        assert_eq!(s.group_count(), 0);
        assert_eq!(
            s.refusals(),
            Refusals {
                name_too_long: 2,
                password_too_long: 2,
                bad_address: 12,
                ..Refusals::default()
            }
        );
    }


    /// The biggest bursts of output fit the send queue in both presets: a full public list
    /// (built here, worst case under the limits) and the peer joins for one group join.
    #[test]
    fn worst_bursts_fit_the_send_queue() {
        for l in [Limits::embedded(), Limits::standalone()] {
            let mut s = Server::with_limits(l);
            let mut now = Instant::now();
            // names of MAX_NAME_LEN bytes with every other byte 0xDB (SLIP-escaped)
            let name = |i: usize| -> String {
                let c = |k| char::from_u32(0x6C0 + k as u32).unwrap();
                std::iter::repeat_n(c(0), MAX_NAME_LEN / 2 - 2)
                    .chain([c(i / 64), c(i % 64)])
                    .collect()
            };
            assert_eq!(name(0).len(), MAX_NAME_LEN);
            let mut conn = 0;
            let per_ip = l.max_groups_per_ip / l.max_groups_per_conn;
            for i in 0..l.max_groups {
                if i % l.max_groups_per_conn == 0 {
                    conn += 1;
                    // as few IPs as the per-IP group cap allows
                    let ip = format!("10.0.{}.1", (conn - 1) / per_ip as u64);
                    open_from(&mut s, conn, &ip, now);
                    send_at(&mut s, conn, login_msg(&format!("u{conn}"), conn), now);
                }
                join_at(&mut s, conn, &name(i), true, now);
                now += Duration::from_secs(1); // join rates refill
            }
            tcp_out(&mut s);
            assert_eq!(s.group_count(), l.max_groups);
            // the cap holds (the next IP has nothing created yet)
            conn += 1;
            open_from(&mut s, conn, "10.1.0.1", now);
            send_at(&mut s, conn, login_msg("late", conn), now);
            join_at(&mut s, conn, "new", false, now);
            let out = tcp_out(&mut s);
            assert_eq!(join_errors(&out, conn)[0].1, "too many groups on server");
            // a new watcher gets the whole list at once
            now += Duration::from_secs(60);
            watch_at(&mut s, conn, true, now);
            let mut list = 0;
            while let Some(o) = s.poll_output() {
                if let Output::Tcp(c, b) = o
                    && c == conn
                {
                    assert!(b.len() <= 239, "{}", b.len());
                    list += b.len();
                }
            }
            assert!(list > l.max_groups * 230, "{list}");
            assert!(list <= l.max_groups * 239 + 64, "{list}");
            assert!(list <= l.send_queue_bytes, "{list} {l:?}");
            assert!(list as u32 <= l.list_rate_per_ip.burst, "{list} {l:?}");

            // the peer joins of joining a group of max_connections - 1 members
            let esc = |n: usize| -> String { std::iter::repeat_n('\u{6C0}', n / 2).collect() };
            let (g, u, lip) = (esc(MAX_NAME_LEN), esc(MAX_NAME_LEN), esc(MAX_IP_LEN));
            let pj = frame(&Message::ClientPeerJoin {
                group: &g,
                user: &u,
                public_ip: "ffff:ffff:ffff:ffff:ffff:ffff:ffff:ffff",
                public_port: 0xC0DB,
                local_ip: &lip,
                local_port: 0xC0DB,
                token: Some(i64::from_ne_bytes([0xC0; 8])),
            });
            let burst = (l.max_connections - 1) * pj.len();
            assert!(burst <= l.send_queue_bytes, "{burst} {l:?}");
        }
    }

    #[test]
    fn claimed_public_address_is_relayed() {
        let mut s = Server::new();
        let t = Instant::now();
        open_from(&mut s, 1, "::ffff:9.9.9.9", t);
        // claims another public address than its TCP peer's, e.g. UDP via another route
        let mut m = login_msg("mallory", 1);
        if let Message::ServerLogin { public_ip, .. } = &mut m {
            *public_ip = "::ffff:6.6.6.6";
        }
        send(&mut s, 1, m);
        login(&mut s, 2, "bob");
        join(&mut s, 1, "g", "", false);
        join(&mut s, 2, "g", "", false);
        let out = tcp_out(&mut s);
        let to_bob = msgs(&out)
            .into_iter()
            .find_map(|(c, m)| match m {
                Message::ClientPeerJoin {
                    public_ip,
                    public_port,
                    ..
                } if c == 2 => Some((public_ip.to_owned(), public_port)),
                _ => None,
            })
            .unwrap();
        assert_eq!(to_bob, ("6.6.6.6".to_owned(), 1001));
    }

    #[test]
    fn list_budget_per_ip() {
        let groups = ["pub-a", "pub-b", "pub-c", "pub-d"];
        let list: usize = groups.iter().map(|g| frame(&add(g, 1)).len()).sum();
        let budget = list as u32;
        let mut s = Server::with_limits(Limits {
            list_rate_per_ip: Rate {
                burst: budget * 5 / 2,
                per_sec: budget,
            },
            ..unlimited_joins()
        });
        let t0 = Instant::now();
        open_from(&mut s, 1, "3.3.3.3", t0);
        send_at(&mut s, 1, login_msg("owner", 1), t0);
        for g in groups {
            join_at(&mut s, 1, g, true, t0);
        }
        for c in [2, 3, 4] {
            send_at(&mut s, c, login_msg(&format!("w{c}"), c), t0);
        }
        open_from(&mut s, 5, "4.4.4.4", t0);
        send_at(&mut s, 5, login_msg("w5", 5), t0);
        tcp_out(&mut s);
        let lists = |out: &[(ConnId, Vec<u8>)]| -> Vec<(ConnId, usize)> {
            [2, 3, 4, 5]
                .into_iter()
                .map(|c| (c, list_msgs(out, c).len()))
                .filter(|(_, n)| *n > 0)
                .collect()
        };
        // two lists fit the burst of 2.5 lists; the third waits for the refill
        for c in [2, 3, 4, 5] {
            watch_at(&mut s, c, true, t0);
        }
        assert_eq!(lists(&tcp_out(&mut s)), [(2, 4), (3, 4), (5, 4)]);
        s.poll(at(t0, 499));
        assert!(tcp_out(&mut s).is_empty());
        s.poll(at(t0, 500));
        assert_eq!(lists(&tcp_out(&mut s)), [(4, 4)]);
        // toggling watch from one IP costs that IP's budget: one list per second
        let mut sent = 0;
        for ms in (600..10_600).step_by(50) {
            watch_at(&mut s, 2, ms % 100 == 0, at(t0, ms));
            s.poll(at(t0, ms));
            sent += list_msgs(&tcp_out(&mut s), 2).len();
        }
        assert!((40..=48).contains(&sent), "{sent}");
    }
}
