//! Connection-server client of the legacy AOO net protocol (`client.cpp`), as a sans-IO state
//! machine plus a small std TCP driver ([`TcpDriver`]).
//!
//! The UDP socket is the application's (it also carries audio): route every datagram whose
//! address starts with `/aoo/client/` or `/aoo/peer/` to [`Client::handle_udp`], call
//! [`Client::poll`] every few ms (SonoBus: 20 ms), and execute the [`Output`]s.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpStream, UdpSocket};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::message::{self, Message};
use crate::{MAX_PACKET_SIZE, password_hash, slip};

/// Server and peer keepalive ping interval (`client.hpp:19`).
pub const PING_INTERVAL: Duration = Duration::from_secs(10);
/// UDP server request and peer handshake ping interval (`client.hpp:20`).
pub const REQUEST_INTERVAL: Duration = Duration::from_millis(100);
/// Server handshake and peer handshake timeout (`client.hpp:21`).
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// TCP connect timeout (`client.cpp:635`).
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Deviation: the C++ times out only the UDP handshake and then waits for the login reply
/// forever. We give the login its own deadline, counted from sending it (not from the start of
/// the handshake), so a slow but working server still gets the full time.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Not in the C++ (it only reacts to TCP errors): nothing heard from the server on either
/// transport for this long while connected (three missed ping answers) = link dead.
pub const SERVER_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Logged in.
    Connected,
    /// TCP connect, UDP handshake or login failed; the client is disconnected again.
    /// `reason` is the server's login error text, "timed out", or the socket error.
    ConnectFailed {
        reason: String,
    },
    /// Connection ended. `error` is `None` after [`Client::disconnect`].
    Disconnected {
        error: Option<String>,
    },
    GroupJoined {
        group: String,
    },
    GroupJoinFailed {
        group: String,
        error: String,
    },
    /// Left the group; its peers were dropped without [`Event::PeerLeft`] (as the C++ does).
    GroupLeft {
        group: String,
    },
    GroupLeaveFailed {
        group: String,
        error: String,
    },
    /// A public group appeared or its user count changed (needs [`Client::watch_public_groups`]).
    PublicGroupAdded {
        group: String,
        users: i32,
    },
    PublicGroupRemoved {
        group: String,
    },
    /// Hole punching succeeded; `addr` is where the peer answered from.
    PeerJoined {
        group: String,
        user: String,
        addr: SocketAddr,
        token: i64,
    },
    /// No answer within [`REQUEST_TIMEOUT`]; a late ping still produces [`Event::PeerJoined`].
    PeerJoinFailed {
        group: String,
        user: String,
    },
    /// `addr` is the resolved address, or the announced public one if the handshake never finished.
    PeerLeft {
        group: String,
        user: String,
        addr: Option<SocketAddr>,
    },
    /// Undecodable or unexpected TCP message from the server (ignored otherwise).
    ServerError(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Output {
    /// Open the TCP connection (timeout [`CONNECT_TIMEOUT`]), then call
    /// [`Client::tcp_connected`] or [`Client::tcp_closed`].
    TcpConnect(SocketAddr),
    /// Write these bytes (already SLIP-framed) to the TCP connection.
    Tcp(Vec<u8>),
    /// Close the TCP connection; do not call [`Client::tcp_closed`] for it.
    TcpClose,
    /// Send this datagram from the shared UDP socket.
    Udp(SocketAddr, Vec<u8>),
    Event(Event),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Disconnected,
    /// Waiting for the TCP connection.
    Connecting,
    /// Sending `/aoo/server/request` until the server tells us our public address.
    Handshake,
    /// Login sent.
    Login,
    Connected,
}

#[derive(Debug)]
struct Peer {
    group: String,
    user: String,
    public: Option<SocketAddr>,
    local: Option<SocketAddr>,
    token: i64,
    real: Option<SocketAddr>,
    start: Instant,
    last_ping: Option<Instant>,
    timed_out: bool,
}

#[derive(Debug)]
pub struct Client {
    token: i64,
    udp_port: u16,
    state: State,
    server: Option<SocketAddr>,
    user: String,
    password: String,
    local_ip: String,
    /// Start of the handshake, then the time the login was sent.
    since: Instant,
    last_request: Option<Instant>,
    last_tcp_ping: Instant,
    last_udp_ping: Instant,
    last_rx: Instant,
    peers: Vec<Peer>,
    out: VecDeque<Output>,
}

/// `[::ffff:a.b.c.d]:p` -> `a.b.c.d:p`, so dual-stack sockets compare equal to announced IPv4.
pub(crate) fn unmap(a: SocketAddr) -> SocketAddr {
    match a {
        SocketAddr::V6(v6) => match v6.ip().to_ipv4_mapped() {
            Some(ip) => SocketAddr::new(IpAddr::V4(ip), v6.port()),
            None => a,
        },
        v4 => v4,
    }
}

/// `a` in the address family of a UDP socket bound to `local`: IPv4 becomes IPv4-mapped IPv6 on
/// an IPv6 socket, since BSD-derived stacks reject IPv4 destinations there (Linux accepts them).
/// Addresses are kept unmapped everywhere else and only converted right before `send_to`.
/// ponytail: on Windows an IPv6 socket is v6-only unless IPV6_V6ONLY=0, which std cannot set, so
/// mapped destinations still fail there; bind IPv4 (`0.0.0.0`) unless IPv6 peers matter.
pub(crate) fn to_family(a: SocketAddr, local: SocketAddr) -> SocketAddr {
    match (a, local) {
        (SocketAddr::V4(v4), SocketAddr::V6(_)) => {
            SocketAddr::new(v4.ip().to_ipv6_mapped().into(), v4.port())
        }
        _ => a,
    }
}

/// Announced `ip, port` from the server; IPv4 dotted only, like `inet_addr` in `net_utils.hpp:51`.
fn announced(ip: &str, port: i32) -> Option<SocketAddr> {
    Some(SocketAddr::new(
        ip.parse::<Ipv4Addr>().ok()?.into(),
        u16::try_from(port).ok()?,
    ))
}

fn encode(m: &Message) -> Vec<u8> {
    let mut v = Vec::new();
    m.encode(&mut v);
    v
}

fn udp(out: &mut VecDeque<Output>, to: SocketAddr, m: &Message) {
    out.push_back(Output::Udp(to, encode(m)));
}

impl Client {
    /// `udp_port`: local port of the shared UDP socket (sent as the local candidate address).
    /// `random`: entropy for our peer token (e.g. [`random_seed`]).
    pub fn new(udp_port: u16, random: u64) -> Self {
        let now = Instant::now(); // placeholder, every timer is reset before use
        Self {
            // uniform int64 >= 1 (`client.cpp:173-177`)
            token: ((random >> 1) as i64).max(1),
            udp_port,
            state: State::Disconnected,
            server: None,
            user: String::new(),
            password: String::new(),
            local_ip: String::new(),
            since: now,
            last_request: None,
            last_tcp_ping: now,
            last_udp_ping: now,
            last_rx: now,
            peers: Vec::new(),
            out: VecDeque::new(),
        }
    }

    pub fn token(&self) -> i64 {
        self.token
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// The server address of the current or last connection.
    pub fn server_addr(&self) -> Option<SocketAddr> {
        self.server
    }

    /// Next output to execute, in order.
    pub fn poll_output(&mut self) -> Option<Output> {
        self.out.pop_front()
    }

    fn event(&mut self, e: Event) {
        self.out.push_back(Output::Event(e));
    }

    fn tcp(&mut self, m: &Message) {
        let mut v = Vec::new();
        slip::encode(&encode(m), &mut v);
        self.out.push_back(Output::Tcp(v));
    }

    /// Ignored unless disconnected (`client.cpp:264-275`). `password` is plaintext.
    pub fn connect(&mut self, server: SocketAddr, user: &str, password: &str) {
        if self.state != State::Disconnected {
            return;
        }
        self.server = Some(unmap(server));
        self.user = user.to_owned();
        self.password = password_hash(password);
        self.state = State::Connecting;
        self.out.push_back(Output::TcpConnect(unmap(server)));
    }

    /// Close the connection and drop all peers; emits `Disconnected { error: None }`.
    pub fn disconnect(&mut self) {
        if self.state != State::Disconnected {
            self.close(Event::Disconnected { error: None });
        }
    }

    fn close(&mut self, e: Event) {
        self.out.push_back(Output::TcpClose);
        self.peers.clear();
        self.state = State::Disconnected;
        self.event(e);
    }

    /// Ends the connection with `ConnectFailed` or `Disconnected` depending on the phase.
    fn fail(&mut self, reason: &str) {
        let e = if self.state == State::Connected {
            Event::Disconnected {
                error: Some(reason.to_owned()),
            }
        } else {
            Event::ConnectFailed {
                reason: reason.to_owned(),
            }
        };
        self.close(e);
    }

    /// `password` is plaintext.
    pub fn join_group(&mut self, group: &str, password: &str, public: bool) {
        if self.state != State::Connected {
            return self.event(Event::GroupJoinFailed {
                group: group.to_owned(),
                error: "not connected".into(),
            });
        }
        self.tcp(&Message::ServerGroupJoin {
            group,
            password: &password_hash(password),
            is_public: Some(public),
        });
    }

    pub fn leave_group(&mut self, group: &str) {
        if self.state != State::Connected {
            return self.event(Event::GroupLeaveFailed {
                group: group.to_owned(),
                error: "not connected".into(),
            });
        }
        self.tcp(&Message::ServerGroupLeave { group });
    }

    /// Ignored when not connected.
    pub fn watch_public_groups(&mut self, watch: bool) {
        if self.state == State::Connected {
            self.tcp(&Message::ServerGroupPublic { watch });
        }
    }

    /// The TCP connection is up; `local_ip` is its local address (`getsockname`, `client.cpp:641-649`).
    pub fn tcp_connected(&mut self, local_ip: IpAddr, now: Instant) {
        if self.state != State::Connecting {
            return;
        }
        self.local_ip = match local_ip {
            IpAddr::V6(v6) => v6
                .to_ipv4_mapped()
                .map_or(v6.to_string(), |v4| v4.to_string()),
            v4 => v4.to_string(),
        };
        self.state = State::Handshake;
        self.since = now;
        self.last_request = None;
        self.poll(now);
    }

    /// The TCP connection failed or was closed by the other side.
    pub fn tcp_closed(&mut self, reason: &str, _now: Instant) {
        if self.state != State::Disconnected {
            self.fail(reason);
        }
    }

    /// Timers: server requests, pings, timeouts and peer hole punching.
    pub fn poll(&mut self, now: Instant) {
        match self.state {
            State::Disconnected | State::Connecting => {}
            State::Handshake | State::Login => {
                let limit = match self.state {
                    State::Handshake => REQUEST_TIMEOUT,
                    _ => LOGIN_TIMEOUT,
                };
                if now.duration_since(self.since) > limit {
                    return self.fail("timed out");
                }
                if self.state == State::Handshake
                    && self
                        .last_request
                        .is_none_or(|t| now.duration_since(t) >= REQUEST_INTERVAL)
                {
                    self.last_request = Some(now);
                    let to = self.server.expect("set by connect");
                    udp(&mut self.out, to, &Message::ServerRequest);
                }
            }
            State::Connected => {
                if now.duration_since(self.last_rx) > SERVER_TIMEOUT {
                    return self.fail("timed out");
                }
                if now.duration_since(self.last_tcp_ping) >= PING_INTERVAL {
                    self.last_tcp_ping = now;
                    self.tcp(&Message::ServerPing);
                }
                if now.duration_since(self.last_udp_ping) >= PING_INTERVAL {
                    self.last_udp_ping = now;
                    let to = self.server.expect("set by connect");
                    udp(&mut self.out, to, &Message::ServerPing);
                }
                self.poll_peers(now);
            }
        }
    }

    fn poll_peers(&mut self, now: Instant) {
        let token = self.token;
        for p in &mut self.peers {
            let due = |iv| p.last_ping.is_none_or(|t| now.duration_since(t) >= iv);
            if let Some(real) = p.real {
                if due(PING_INTERVAL) {
                    p.last_ping = Some(now);
                    udp(&mut self.out, real, &Message::PeerPing { token: None });
                }
            } else if !p.timed_out {
                if now.duration_since(p.start) > REQUEST_TIMEOUT {
                    p.timed_out = true;
                    self.out.push_back(Output::Event(Event::PeerJoinFailed {
                        group: p.group.clone(),
                        user: p.user.clone(),
                    }));
                } else if due(REQUEST_INTERVAL) {
                    p.last_ping = Some(now);
                    let m = Message::PeerPing { token: Some(token) };
                    for a in [p.local, p.public].into_iter().flatten() {
                        udp(&mut self.out, a, &m);
                    }
                }
            }
        }
    }

    /// A `/aoo/client/...` or `/aoo/peer/...` datagram arrived on the shared UDP socket.
    pub fn handle_udp(&mut self, from: SocketAddr, msg: &Message, now: Instant) {
        let from = unmap(from);
        if Some(from) == self.server {
            if self.state == State::Disconnected {
                return;
            }
            self.last_rx = now;
            if let (
                State::Handshake,
                Message::ClientReply {
                    public_ip,
                    public_port,
                },
            ) = (self.state, msg)
            {
                self.state = State::Login;
                self.since = now;
                self.tcp(&Message::ServerLogin {
                    user: &self.user.clone(),
                    password: &self.password.clone(),
                    public_ip,
                    public_port: *public_port,
                    local_ip: &self.local_ip.clone(),
                    local_port: self.udp_port.into(),
                    token: Some(self.token),
                });
            }
            return;
        }
        let Message::PeerPing { token } = *msg else {
            return;
        };
        let token = token.unwrap_or(0);
        // A user in several groups is several peers on one address (`client.cpp:390-394`).
        for p in &mut self.peers {
            let hit = match p.real {
                Some(real) => real == from,
                None if p.public == Some(from) || p.local == Some(from) => true,
                // symmetric NAT: unknown address but the token the server gave us (`client.cpp:406-418`)
                None if token > 0 && token == p.token => {
                    p.public = Some(from);
                    true
                }
                None => false,
            };
            if hit && p.real.is_none() {
                p.real = Some(from);
                p.last_ping = Some(now);
                // answer at once so the other side completes too (`client.cpp:1403-1404`)
                udp(&mut self.out, from, &Message::PeerPing { token: None });
                self.out.push_back(Output::Event(Event::PeerJoined {
                    group: p.group.clone(),
                    user: p.user.clone(),
                    addr: from,
                    token: p.token,
                }));
            }
        }
    }

    /// One decoded SLIP frame from the server.
    pub fn handle_tcp_frame(&mut self, frame: &[u8], now: Instant) {
        if self.state == State::Disconnected {
            return;
        }
        self.last_rx = now;
        let msg = match message::parse(frame) {
            Ok(m) => m,
            Err(e) => return self.event(Event::ServerError(format!("bad message: {e}"))),
        };
        let s = |x: &str| x.to_owned();
        let err = |e: Option<&str>| e.unwrap_or("unknown error").to_owned();
        match msg {
            Message::ClientPing | Message::ClientGroupPublic { .. } => {}
            Message::ClientLogin { result, error } => {
                if self.state != State::Login {
                    return;
                }
                if result > 0 {
                    self.state = State::Connected;
                    self.last_tcp_ping = now;
                    self.last_udp_ping = now;
                    self.event(Event::Connected);
                } else {
                    self.fail(&err(error));
                }
            }
            Message::ClientGroupJoin {
                group,
                result,
                error,
            } => self.event(if result > 0 {
                Event::GroupJoined { group: s(group) }
            } else {
                Event::GroupJoinFailed {
                    group: s(group),
                    error: err(error),
                }
            }),
            Message::ClientGroupLeave {
                group,
                result,
                error,
            } => {
                if result > 0 {
                    self.peers.retain(|p| p.group != group);
                    self.event(Event::GroupLeft { group: s(group) });
                } else {
                    self.event(Event::GroupLeaveFailed {
                        group: s(group),
                        error: err(error),
                    });
                }
            }
            Message::ClientGroupPublicAdd { group, users } => self.event(Event::PublicGroupAdded {
                group: s(group),
                users,
            }),
            Message::ClientGroupPublicDel { group } => {
                self.event(Event::PublicGroupRemoved { group: s(group) })
            }
            Message::ClientPeerJoin {
                group,
                user,
                public_ip,
                public_port,
                local_ip,
                local_port,
                token,
            } => {
                if self
                    .peers
                    .iter()
                    .any(|p| p.group == group && p.user == user)
                {
                    return; // "already added" (`client.cpp:1138-1144`)
                }
                self.peers.push(Peer {
                    group: s(group),
                    user: s(user),
                    public: announced(public_ip, public_port),
                    local: announced(local_ip, local_port),
                    token: token.unwrap_or(0),
                    real: None,
                    start: now,
                    last_ping: None,
                    timed_out: false,
                });
                self.poll_peers(now);
            }
            Message::ClientPeerLeave { group, user } => {
                if let Some(i) = self
                    .peers
                    .iter()
                    .position(|p| p.group == group && p.user == user)
                {
                    let p = self.peers.remove(i);
                    self.event(Event::PeerLeft {
                        group: p.group,
                        user: p.user,
                        addr: p.real.or(p.public),
                    });
                }
            }
            other => self.event(Event::ServerError(format!("unexpected message {other:?}"))),
        }
    }
}

/// Random bits from std's per-process hasher keys, for [`Client::new`].
pub fn random_seed() -> u64 {
    use std::hash::BuildHasher;
    std::collections::hash_map::RandomState::new().hash_one(Instant::now())
}

/// Stack size of every network thread (client and server). They hold an 8 KiB write chunk or a
/// 4 KiB read buffer and little else; the 2 MiB default adds up with hundreds of server
/// connections on a phone.
pub(crate) const THREAD_STACK_SIZE: usize = 128 * 1024;
/// Driver: a TCP write blocked this long fails the connection (the server stopped reading).
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
/// Driver: more than this many unsent bytes fail the connection. Client messages are < 1 KiB.
const SEND_QUEUE_BYTES: usize = 64 * 1024;
/// Driver: decoded server frames waiting for [`TcpDriver::pump`]. When full, the reader stops
/// reading, so TCP flow control throttles the server: at most 64 x 4 KiB buffered.
const LINK_QUEUE: usize = 64;
/// Driver: link events handled per [`TcpDriver::pump`] call; the rest wait for the next call,
/// so a server sending at line rate cannot keep `pump` from returning.
const PUMP_BUDGET: usize = 128;
/// Writer thread: bytes written per `write` call (a stack buffer).
const WRITE_CHUNK: usize = 8 * 1024;

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct WriteQueue {
    buf: VecDeque<u8>,
    /// Taken out of `buf` and being written.
    inflight: usize,
    /// No more sends: the owner closed the queue or a write failed.
    closed: bool,
}

/// Writes to a TCP socket from its own thread, so the caller never blocks on the socket.
/// Queued bytes live in one buffer that never grows past `cap` (plus one `WRITE_CHUNK` on the
/// thread's stack), so the memory bound is real, not a payload count.
pub(crate) struct TcpWriter {
    q: Arc<(Mutex<WriteQueue>, Condvar)>,
    cap: usize,
    thread: Option<JoinHandle<()>>,
}

impl TcpWriter {
    /// `stream` should have a write timeout. On a write error the thread calls `on_error`, then
    /// shuts the socket down (which also ends a reader blocked on it). `keep` lives as long as
    /// the thread.
    pub(crate) fn spawn(
        name: &str,
        stream: Arc<TcpStream>,
        cap: usize,
        keep: impl Send + 'static,
        on_error: impl FnOnce(io::Error) + Send + 'static,
    ) -> io::Result<Self> {
        let q = Arc::new((Mutex::new(WriteQueue::default()), Condvar::new()));
        let tq = q.clone();
        let thread = thread::Builder::new()
            .name(name.into())
            .stack_size(THREAD_STACK_SIZE)
            .spawn(move || {
                let _keep = keep;
                let (m, cv) = &*tq;
                let mut chunk = [0u8; WRITE_CHUNK];
                loop {
                    let n = {
                        let mut g = lock(m);
                        while g.buf.is_empty() {
                            if g.closed {
                                return;
                            }
                            g = cv.wait(g).unwrap_or_else(|e| e.into_inner());
                        }
                        let n = g.buf.len().min(WRITE_CHUNK);
                        for (d, s) in chunk.iter_mut().zip(g.buf.drain(..n)) {
                            *d = s;
                        }
                        g.inflight = n;
                        // an idle connection does not keep a once-full buffer
                        if g.buf.is_empty() && g.buf.capacity() > WRITE_CHUNK {
                            g.buf = VecDeque::new();
                        }
                        n
                    };
                    if let Err(e) = (&*stream).write_all(&chunk[..n]) {
                        lock(m).closed = true;
                        on_error(e);
                        let _ = stream.shutdown(Shutdown::Both);
                        return;
                    }
                    lock(m).inflight = 0;
                }
            })?;
        Ok(Self {
            q,
            cap,
            thread: Some(thread),
        })
    }

    /// The writer thread has failed (or the queue was closed): further sends fail.
    pub(crate) fn closed(&self) -> bool {
        lock(&self.q.0).closed
    }

    /// Queue `bytes`. `Err` once more than `cap` bytes would be waiting (the peer does not
    /// read) or the writer thread has failed; the connection should then be closed.
    pub(crate) fn send(&self, bytes: &[u8]) -> Result<(), ()> {
        let (m, cv) = &*self.q;
        let mut g = lock(m);
        let len = g.buf.len() + bytes.len();
        if g.closed || g.inflight + len > self.cap {
            return Err(());
        }
        if len > g.buf.capacity() {
            // double, but never past `cap`
            let want = len.max(2 * g.buf.capacity()).min(self.cap - g.inflight);
            let extra = want - g.buf.len();
            g.buf.reserve_exact(extra);
        }
        g.buf.extend(bytes);
        cv.notify_one();
        Ok(())
    }

    /// Close the queue and wait for the thread. Shut the socket down first, or this waits for
    /// the queued data (bounded by the write timeout).
    pub(crate) fn join(mut self) {
        self.close();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }

    fn close(&self) {
        let (m, cv) = &*self.q;
        lock(m).closed = true;
        cv.notify_one();
    }
}

impl Drop for TcpWriter {
    /// The thread writes what is queued, then exits.
    fn drop(&mut self) {
        self.close();
    }
}

/// Counts a live thread of one [`TcpDriver`] (tests check that none outlive their link).
struct Live(Arc<AtomicUsize>);

impl Live {
    fn new(n: &Arc<AtomicUsize>) -> Self {
        n.fetch_add(1, Ordering::SeqCst);
        Self(n.clone())
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

enum LinkEvent {
    Connected(IpAddr, TcpWriter),
    Frame(Vec<u8>),
    Closed(String),
}

/// `closed` is set when the link is dropped; `stream` is the connected socket, for `shutdown`.
/// The link thread checks `closed` and stores `stream` under the same lock, so whichever of
/// thread and drop comes second ends the thread: no interleaving leaks it.
#[derive(Default)]
struct LinkCtl {
    closed: bool,
    stream: Option<Arc<TcpStream>>,
}

/// Test knobs: each switches off one of the two mechanisms that end a link thread, so tests can
/// show that the other one alone suffices.
#[derive(Clone, Copy)]
struct LinkOpts {
    /// 1 s read timeout, after which the reader checks `closed`.
    backstop: bool,
    /// The `closed` check before storing the stream.
    check_closed: bool,
    /// Pause between connecting and that check.
    setup_delay: Duration,
}

impl Default for LinkOpts {
    fn default() -> Self {
        Self {
            backstop: true,
            check_closed: true,
            setup_delay: Duration::ZERO,
        }
    }
}

/// One TCP connection to the server: a thread connects and then reads SLIP frames; a second
/// thread ([`TcpWriter`]) writes. Both share the socket (one fd). Each link has its own bounded
/// event channel, so events of a replaced link can never reach the next one.
struct TcpLink {
    rx: mpsc::Receiver<LinkEvent>,
    ctl: Arc<Mutex<LinkCtl>>,
    writer: Option<TcpWriter>,
    /// A send failed; reported as the link's close on the next event drain.
    broken: Option<String>,
}

impl TcpLink {
    fn connect(addr: SocketAddr, live: &Arc<AtomicUsize>, opts: LinkOpts) -> io::Result<Self> {
        let (tx, rx) = mpsc::sync_channel(LINK_QUEUE);
        let ctl = Arc::new(Mutex::new(LinkCtl::default()));
        let (c, live) = (ctl.clone(), live.clone());
        let guard = Live::new(&live);
        thread::Builder::new()
            .name("aoo-client-rx".into())
            .stack_size(THREAD_STACK_SIZE)
            .spawn(move || {
                let _guard = guard;
                link_thread(addr, &tx, &c, &live, opts);
            })?;
        Ok(Self {
            rx,
            ctl,
            writer: None,
            broken: None,
        })
    }
}

fn link_thread(
    addr: SocketAddr,
    tx: &mpsc::SyncSender<LinkEvent>,
    ctl: &Mutex<LinkCtl>,
    live: &Arc<AtomicUsize>,
    opts: LinkOpts,
) {
    let closed = |e: &dyn std::fmt::Display| drop(tx.send(LinkEvent::Closed(e.to_string())));
    // bounded by CONNECT_TIMEOUT, so a dropped link's thread lingers at most that long
    let s = match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
        Ok(s) => Arc::new(s),
        Err(e) => return closed(&format_args!("{addr}: {e}")),
    };
    let setup = || -> io::Result<_> {
        s.set_nodelay(true)?;
        s.set_write_timeout(Some(SEND_TIMEOUT))?;
        if opts.backstop {
            // backstop for the `closed` check below; dropping the link normally wakes the read
            s.set_read_timeout(Some(Duration::from_secs(1)))?;
        }
        let etx = tx.clone();
        let on_error = move |e: io::Error| {
            let reason = match e.kind() {
                io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut => {
                    "server not reading (send timed out)".into()
                }
                _ => e.to_string(),
            };
            // never blocks; if the queue is full the reader reports the shutdown instead
            drop(etx.try_send(LinkEvent::Closed(reason)));
        };
        let w = TcpWriter::spawn(
            "aoo-client-tx",
            s.clone(),
            SEND_QUEUE_BYTES,
            Live::new(live),
            on_error,
        )?;
        Ok((s.local_addr()?.ip(), w))
    };
    let (ip, w) = match setup() {
        Ok(x) => x,
        Err(e) => return closed(&e),
    };
    thread::sleep(opts.setup_delay);
    {
        let mut c = lock(ctl);
        if c.closed && opts.check_closed {
            return; // link dropped while connecting; dropping `w` ends the writer
        }
        c.stream = Some(s.clone());
    }
    if tx.send(LinkEvent::Connected(ip, w)).is_err() {
        return;
    }
    let mut dec = slip::Decoder::new(MAX_PACKET_SIZE);
    let mut buf = [0u8; MAX_PACKET_SIZE];
    // Dropping the link shuts the socket down, which ends the read (and a blocked send fails
    // once the receiver is gone).
    loop {
        match (&*s).read(&mut buf) {
            Ok(0) => return closed(&"disconnected from server"),
            Ok(n) => dec.feed(&buf[..n], |f| drop(tx.send(LinkEvent::Frame(f.to_vec())))),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if lock(ctl).closed {
                    return;
                }
            }
            Err(e) => return closed(&e),
        }
    }
}

impl Drop for TcpLink {
    fn drop(&mut self) {
        let mut c = lock(&self.ctl);
        c.closed = true;
        if let Some(s) = &c.stream {
            let _ = s.shutdown(Shutdown::Both); // ends the reader; a blocked write fails too
        }
        // `writer` is dropped after this: its queue closes and the thread exits
    }
}

/// Minimal std driver for the TCP side of a [`Client`]; the UDP socket stays the caller's.
/// Never blocks: connecting, reading and writing happen on two threads per connection, which
/// share one socket (one fd).
#[derive(Default)]
pub struct TcpDriver {
    link: Option<TcpLink>,
    /// Live threads of this driver's links.
    live: Arc<AtomicUsize>,
    opts: LinkOpts,
}

impl TcpDriver {
    /// Feed TCP input into `client` (at most 128 frames per call), run its timers,
    /// and execute its outputs. Call every few ms.
    pub fn pump(
        &mut self,
        client: &mut Client,
        udp_socket: &UdpSocket,
        now: Instant,
        mut on_event: impl FnMut(Event),
    ) {
        // Outputs first: a disconnect()/connect() made since the last pump replaces the link
        // before any event of the old one is read, so they cannot reach the new session.
        self.execute(client, udp_socket, now, &mut on_event);
        for _ in 0..PUMP_BUDGET {
            let Some(l) = &mut self.link else { break };
            let ev = match l.broken.take() {
                Some(reason) => LinkEvent::Closed(reason),
                None => match l.rx.try_recv() {
                    Ok(ev) => ev,
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        LinkEvent::Closed("connection thread ended".into())
                    }
                },
            };
            match ev {
                LinkEvent::Connected(ip, w) => {
                    l.writer = Some(w);
                    client.tcp_connected(ip, now);
                }
                LinkEvent::Frame(f) => client.handle_tcp_frame(&f, now),
                LinkEvent::Closed(reason) => {
                    self.link = None;
                    client.tcp_closed(&reason, now);
                }
            }
        }
        client.poll(now);
        self.execute(client, udp_socket, now, &mut on_event);
    }

    fn execute(
        &mut self,
        client: &mut Client,
        udp_socket: &UdpSocket,
        now: Instant,
        on_event: &mut impl FnMut(Event),
    ) {
        let mut local = None;
        while let Some(o) = client.poll_output() {
            match o {
                Output::TcpConnect(a) => {
                    self.link = None;
                    match TcpLink::connect(a, &self.live, self.opts) {
                        Ok(l) => self.link = Some(l),
                        Err(e) => client.tcp_closed(&format!("can't start thread: {e}"), now),
                    }
                }
                Output::Tcp(bytes) => match &mut self.link {
                    // Tcp outputs exist only after `tcp_connected`, which needs a writer
                    Some(TcpLink {
                        writer: Some(w),
                        broken: broken @ None,
                        ..
                    }) => {
                        if w.send(&bytes).is_err() {
                            *broken = Some("server not reading (send queue full)".into());
                        }
                    }
                    Some(TcpLink {
                        broken: Some(_), ..
                    }) => {}
                    Some(l) => l.broken = Some("internal error: write before connect".into()),
                    None => client.tcp_closed("internal error: write without connection", now),
                },
                Output::TcpClose => self.link = None,
                Output::Udp(to, bytes) => {
                    let local = *local.get_or_insert_with(|| udp_socket.local_addr().ok());
                    let to = local.map_or(to, |l| to_family(to, l));
                    let _ = udp_socket.send_to(&bytes, to);
                }
                Output::Event(e) => on_event(e),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRV: &str = "10.0.0.1:10998";

    fn srv() -> SocketAddr {
        SRV.parse().unwrap()
    }

    fn outs(c: &mut Client) -> Vec<Output> {
        std::iter::from_fn(|| c.poll_output()).collect()
    }

    fn events(o: &[Output]) -> Vec<Event> {
        o.iter()
            .filter_map(|o| match o {
                Output::Event(e) => Some(e.clone()),
                _ => None,
            })
            .collect()
    }

    fn udp_sent(o: &[Output]) -> Vec<(SocketAddr, Message<'_>)> {
        o.iter()
            .filter_map(|o| match o {
                Output::Udp(a, b) => Some((*a, message::parse(b).unwrap())),
                _ => None,
            })
            .collect()
    }

    fn tcp_sent(o: &[Output]) -> Vec<Vec<u8>> {
        let mut frames = Vec::new();
        let mut d = slip::Decoder::new(MAX_PACKET_SIZE);
        for o in o {
            if let Output::Tcp(b) = o {
                d.feed(b, |f| frames.push(f.to_vec()));
            }
        }
        frames
    }

    fn frame(m: Message) -> Vec<u8> {
        encode(&m)
    }

    /// Client in `Login` state after TCP connect + reply; returns (client, t0, outputs so far).
    fn logging_in() -> (Client, Instant, Vec<Output>) {
        let t0 = Instant::now();
        let mut c = Client::new(5000, 42 << 1);
        assert_eq!(c.token(), 42);
        c.connect(srv(), "alice", "secret");
        assert_eq!(outs(&mut c), [Output::TcpConnect(srv())]);
        c.tcp_connected("192.168.1.5".parse().unwrap(), t0);
        let o = outs(&mut c);
        assert_eq!(udp_sent(&o), [(srv(), Message::ServerRequest)]);
        let reply = Message::ClientReply {
            public_ip: "1.2.3.4",
            public_port: 6000,
        };
        c.handle_udp(srv(), &reply, t0 + Duration::from_millis(50));
        assert_eq!(c.state(), State::Login);
        (c, t0, o)
    }

    fn connected() -> (Client, Instant) {
        let (mut c, t0, _) = logging_in();
        let o = outs(&mut c);
        let login = tcp_sent(&o);
        assert_eq!(
            message::parse(&login[0]).unwrap(),
            Message::ServerLogin {
                user: "alice",
                password: &password_hash("secret"),
                public_ip: "1.2.3.4",
                public_port: 6000,
                local_ip: "192.168.1.5",
                local_port: 5000,
                token: Some(42),
            }
        );
        let ok = frame(Message::ClientLogin {
            result: 1,
            error: Some(""),
        });
        c.handle_tcp_frame(&ok, t0);
        assert_eq!(events(&outs(&mut c)), [Event::Connected]);
        (c, t0)
    }

    fn peer_join(c: &mut Client, user: &str, public: &str, local: &str, token: i64, now: Instant) {
        let (p, l): (SocketAddr, SocketAddr) = (public.parse().unwrap(), local.parse().unwrap());
        let m = Message::ClientPeerJoin {
            group: "g",
            user,
            public_ip: &p.ip().to_string(),
            public_port: p.port().into(),
            local_ip: &l.ip().to_string(),
            local_port: l.port().into(),
            token: Some(token),
        };
        c.handle_tcp_frame(&frame(m), now);
    }

    #[test]
    fn request_loop_and_connect_timeout() {
        let t0 = Instant::now();
        let mut c = Client::new(5000, 7);
        c.connect(srv(), "u", "");
        c.tcp_connected(Ipv4Addr::LOCALHOST.into(), t0);
        let mut requests = udp_sent(&outs(&mut c)).len();
        // poll every 10 ms for 5 s: one request per 100 ms
        for ms in (10..=5000).step_by(10) {
            c.poll(t0 + Duration::from_millis(ms));
            let o = outs(&mut c);
            assert!(events(&o).is_empty(), "no event before the timeout");
            requests += udp_sent(&o).len();
        }
        assert_eq!(requests, 51);
        // replies from anyone else are ignored
        let reply = Message::ClientReply {
            public_ip: "1.2.3.4",
            public_port: 1,
        };
        c.handle_udp("10.0.0.2:10998".parse().unwrap(), &reply, t0);
        assert_eq!(c.state(), State::Handshake);
        c.poll(t0 + Duration::from_millis(5001));
        let o = outs(&mut c);
        assert_eq!(o[0], Output::TcpClose);
        assert_eq!(
            events(&o),
            [Event::ConnectFailed {
                reason: "timed out".into()
            }]
        );
        assert_eq!(c.state(), State::Disconnected);
    }

    #[test]
    fn tcp_connect_failure_and_login_failure() {
        let mut c = Client::new(5000, 7);
        c.connect(srv(), "u", "");
        outs(&mut c);
        c.tcp_closed("Connection refused", Instant::now());
        assert_eq!(
            events(&outs(&mut c)),
            [Event::ConnectFailed {
                reason: "Connection refused".into()
            }]
        );

        let (mut c, t0, _) = logging_in();
        outs(&mut c);
        let denied = frame(Message::ClientLogin {
            result: 0,
            error: Some("access denied"),
        });
        c.handle_tcp_frame(&denied, t0);
        let o = outs(&mut c);
        assert!(o.contains(&Output::TcpClose));
        assert_eq!(
            events(&o),
            [Event::ConnectFailed {
                reason: "access denied".into()
            }]
        );
        assert_eq!(c.state(), State::Disconnected);
        // connect works again afterwards
        c.connect(srv(), "u", "");
        assert_eq!(c.state(), State::Connecting);
    }

    #[test]
    fn keepalive_cadence_and_server_timeout() {
        let (mut c, t0) = connected();
        let mut tcp = 0;
        let mut udp = 0;
        // 25 s of polling every 100 ms; the server answers nothing
        for ms in (100..=25_000).step_by(100) {
            c.poll(t0 + Duration::from_millis(ms));
            let o = outs(&mut c);
            for f in tcp_sent(&o) {
                assert_eq!(message::parse(&f).unwrap(), Message::ServerPing);
                tcp += 1;
            }
            for (a, m) in udp_sent(&o) {
                assert_eq!((a, m), (srv(), Message::ServerPing));
                udp += 1;
            }
        }
        assert_eq!((tcp, udp), (2, 2)); // at 10 s and 20 s
        // a UDP ping answer keeps the link alive
        c.handle_udp(srv(), &Message::ClientPing, t0 + Duration::from_secs(25));
        c.poll(t0 + Duration::from_secs(54));
        assert!(events(&outs(&mut c)).is_empty());
        c.poll(t0 + Duration::from_millis(55_001));
        let o = outs(&mut c);
        assert_eq!(
            events(&o),
            [Event::Disconnected {
                error: Some("timed out".into())
            }]
        );
        assert_eq!(o[0], Output::TcpClose);
    }

    #[test]
    fn group_events() {
        let (mut c, t0) = connected();
        c.join_group("g", "pw", false);
        let f = tcp_sent(&outs(&mut c));
        assert_eq!(
            message::parse(&f[0]).unwrap(),
            Message::ServerGroupJoin {
                group: "g",
                password: &password_hash("pw"),
                is_public: Some(false),
            }
        );
        let fail = frame(Message::ClientGroupJoin {
            group: "g",
            result: 0,
            error: Some("wrong password"),
        });
        c.handle_tcp_frame(&fail, t0);
        let add = frame(Message::ClientGroupPublicAdd {
            group: "p",
            users: 3,
        });
        c.handle_tcp_frame(&add, t0);
        c.handle_tcp_frame(&frame(Message::ClientGroupPublicDel { group: "p" }), t0);
        assert_eq!(
            events(&outs(&mut c)),
            [
                Event::GroupJoinFailed {
                    group: "g".into(),
                    error: "wrong password".into()
                },
                Event::PublicGroupAdded {
                    group: "p".into(),
                    users: 3
                },
                Event::PublicGroupRemoved { group: "p".into() },
            ]
        );
    }

    #[test]
    fn handshake_via_public_address() {
        let (mut c, t0) = connected();
        peer_join(&mut c, "bob", "5.6.7.8:7000", "192.168.1.9:7001", 99, t0);
        let o = outs(&mut c);
        let pub_: SocketAddr = "5.6.7.8:7000".parse().unwrap();
        let loc: SocketAddr = "192.168.1.9:7001".parse().unwrap();
        let ping = Message::PeerPing { token: Some(42) };
        assert_eq!(udp_sent(&o), [(loc, ping), (pub_, ping)]);
        // 100 ms cadence to both
        c.poll(t0 + Duration::from_millis(50));
        assert!(udp_sent(&outs(&mut c)).is_empty());
        c.poll(t0 + Duration::from_millis(100));
        assert_eq!(udp_sent(&outs(&mut c)).len(), 2);
        // bob answers from his public address
        let t1 = t0 + Duration::from_millis(150);
        c.handle_udp(pub_, &Message::PeerPing { token: Some(99) }, t1);
        let o = outs(&mut c);
        assert_eq!(udp_sent(&o), [(pub_, Message::PeerPing { token: None })]);
        assert_eq!(
            events(&o),
            [Event::PeerJoined {
                group: "g".into(),
                user: "bob".into(),
                addr: pub_,
                token: 99
            }]
        );
        // then keepalive every 10 s to that address only, and pings are answered silently
        c.handle_udp(pub_, &Message::PeerPing { token: None }, t1);
        c.poll(t1 + Duration::from_secs(9));
        assert!(outs(&mut c).is_empty());
        c.poll(t1 + Duration::from_secs(10));
        let o = outs(&mut c);
        assert!(udp_sent(&o).contains(&(pub_, Message::PeerPing { token: None })));
        // peer leaves
        let leave = frame(Message::ClientPeerLeave {
            group: "g",
            user: "bob",
        });
        c.handle_tcp_frame(&leave, t1);
        assert_eq!(
            events(&outs(&mut c)),
            [Event::PeerLeft {
                group: "g".into(),
                user: "bob".into(),
                addr: Some(pub_)
            }]
        );
    }

    #[test]
    fn handshake_via_local_address() {
        let (mut c, t0) = connected();
        peer_join(&mut c, "bob", "5.6.7.8:7000", "192.168.1.9:7001", 99, t0);
        outs(&mut c);
        let loc: SocketAddr = "192.168.1.9:7001".parse().unwrap();
        c.handle_udp(loc, &Message::PeerPing { token: Some(99) }, t0);
        let o = outs(&mut c);
        assert!(matches!(events(&o)[..], [Event::PeerJoined { addr, .. }] if addr == loc));
    }

    #[test]
    fn handshake_via_token_from_unexpected_address() {
        let (mut c, t0) = connected();
        peer_join(&mut c, "bob", "5.6.7.8:7000", "192.168.1.9:7001", 99, t0);
        peer_join(&mut c, "carol", "5.6.7.9:7000", "192.168.1.10:7001", 77, t0);
        outs(&mut c);
        let nat: SocketAddr = "5.6.7.8:41234".parse().unwrap();
        // wrong token or no token: ignored
        c.handle_udp(nat, &Message::PeerPing { token: Some(5) }, t0);
        c.handle_udp(nat, &Message::PeerPing { token: None }, t0);
        assert!(outs(&mut c).is_empty());
        c.handle_udp(nat, &Message::PeerPing { token: Some(99) }, t0);
        let o = outs(&mut c);
        assert_eq!(
            events(&o),
            [Event::PeerJoined {
                group: "g".into(),
                user: "bob".into(),
                addr: nat,
                token: 99
            }]
        );
        // once resolved, the token no longer moves the address
        let other: SocketAddr = "5.6.7.8:9999".parse().unwrap();
        c.handle_udp(other, &Message::PeerPing { token: Some(99) }, t0);
        assert!(outs(&mut c).is_empty());
    }

    #[test]
    fn handshake_timeout_then_late_ping() {
        let (mut c, t0) = connected();
        peer_join(&mut c, "bob", "5.6.7.8:7000", "192.168.1.9:7001", 99, t0);
        outs(&mut c);
        c.poll(t0 + Duration::from_millis(5000));
        assert!(events(&outs(&mut c)).is_empty());
        c.poll(t0 + Duration::from_millis(5001));
        let failed = Event::PeerJoinFailed {
            group: "g".into(),
            user: "bob".into(),
        };
        assert_eq!(events(&outs(&mut c)), [failed]);
        // pinging stopped
        c.poll(t0 + Duration::from_millis(5200));
        assert!(udp_sent(&outs(&mut c)).is_empty());
        // a late ping from a known address still joins
        let pub_: SocketAddr = "5.6.7.8:7000".parse().unwrap();
        c.handle_udp(
            pub_,
            &Message::PeerPing { token: Some(99) },
            t0 + Duration::from_secs(6),
        );
        assert!(matches!(
            events(&outs(&mut c))[..],
            [Event::PeerJoined { .. }]
        ));
    }

    #[test]
    fn group_leave_drops_peers_and_disconnect() {
        let (mut c, t0) = connected();
        peer_join(&mut c, "bob", "5.6.7.8:7000", "192.168.1.9:7001", 99, t0);
        let left = frame(Message::ClientGroupLeave {
            group: "g",
            result: 1,
            error: Some(""),
        });
        c.handle_tcp_frame(&left, t0);
        outs(&mut c);
        c.poll(t0 + Duration::from_millis(200));
        assert!(
            udp_sent(&outs(&mut c)).is_empty(),
            "no pings to dropped peer"
        );
        c.disconnect();
        let o = outs(&mut c);
        assert_eq!(o[0], Output::TcpClose);
        assert_eq!(events(&o), [Event::Disconnected { error: None }]);
        c.join_group("g", "", false);
        assert!(matches!(
            events(&outs(&mut c))[..],
            [Event::GroupJoinFailed { .. }]
        ));
    }

    #[test]
    fn login_has_its_own_deadline() {
        // handshake answered late (4.9 s): the login still gets LOGIN_TIMEOUT from its sending
        let t0 = Instant::now();
        let at = |ms| t0 + Duration::from_millis(ms);
        let mut c = Client::new(5000, 7);
        c.connect(srv(), "u", "");
        c.tcp_connected(Ipv4Addr::LOCALHOST.into(), t0);
        let reply = Message::ClientReply {
            public_ip: "1.2.3.4",
            public_port: 1,
        };
        c.handle_udp(srv(), &reply, at(4900));
        assert_eq!(c.state(), State::Login);
        outs(&mut c);
        c.poll(at(4900 + 5000));
        assert!(events(&outs(&mut c)).is_empty());
        assert_eq!(c.state(), State::Login);
        c.poll(at(4900 + 5001));
        assert_eq!(
            events(&outs(&mut c)),
            [Event::ConnectFailed {
                reason: "timed out".into()
            }]
        );
    }

    fn join_msg<'a>(group: &'a str, user: &'a str, ip: &'a str, token: i64) -> Vec<u8> {
        frame(Message::ClientPeerJoin {
            group,
            user,
            public_ip: ip,
            public_port: 7000,
            local_ip: ip,
            local_port: 7001,
            token: Some(token),
        })
    }

    #[test]
    fn one_user_in_two_groups_resolves_both() {
        let (mut c, t0) = connected();
        c.handle_tcp_frame(&join_msg("g1", "bob", "5.6.7.8", 99), t0);
        c.handle_tcp_frame(&join_msg("g2", "bob", "5.6.7.8", 99), t0);
        outs(&mut c);
        let a: SocketAddr = "5.6.7.8:7000".parse().unwrap();
        c.handle_udp(a, &Message::PeerPing { token: Some(99) }, t0);
        let o = outs(&mut c);
        let groups: Vec<_> = events(&o)
            .into_iter()
            .map(|e| match e {
                Event::PeerJoined { group, addr, .. } if addr == a => group,
                e => panic!("{e:?}"),
            })
            .collect();
        assert_eq!(groups, ["g1", "g2"]);
    }

    #[test]
    fn duplicate_peer_join_is_ignored() {
        let (mut c, t0) = connected();
        c.handle_tcp_frame(&join_msg("g", "bob", "5.6.7.8", 99), t0);
        assert_eq!(udp_sent(&outs(&mut c)).len(), 2);
        c.handle_tcp_frame(&join_msg("g", "bob", "6.6.6.6", 98), t0);
        assert!(outs(&mut c).is_empty());
        c.poll(t0 + Duration::from_millis(100));
        assert_eq!(udp_sent(&outs(&mut c)).len(), 2, "still one peer");
    }

    #[test]
    fn unparseable_announced_ip_resolves_by_token_only() {
        let (mut c, t0) = connected();
        c.handle_tcp_frame(&join_msg("g", "bob", "not-an-ip", 99), t0);
        // nothing to ping
        assert!(outs(&mut c).is_empty());
        let a: SocketAddr = "5.6.7.8:41234".parse().unwrap();
        c.handle_udp(a, &Message::PeerPing { token: None }, t0);
        c.handle_udp(a, &Message::PeerPing { token: Some(98) }, t0);
        assert!(outs(&mut c).is_empty());
        c.handle_udp(a, &Message::PeerPing { token: Some(99) }, t0);
        assert!(matches!(
            events(&outs(&mut c))[..],
            [Event::PeerJoined { addr, token: 99, .. }] if addr == a
        ));
    }

    #[test]
    fn udp_destination_in_socket_family() {
        let v4: SocketAddr = "1.2.3.4:5".parse().unwrap();
        let v6: SocketAddr = "[::1]:5".parse().unwrap();
        let any4: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let any6: SocketAddr = "[::]:0".parse().unwrap();
        assert_eq!(to_family(v4, any4), v4);
        assert_eq!(to_family(v4, any6), "[::ffff:1.2.3.4]:5".parse().unwrap());
        assert_eq!(unmap(to_family(v4, any6)), v4);
        assert_eq!(to_family(v6, any6), v6);
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// Events of a link that is replaced between two pumps never reach the new session.
    #[test]
    fn stale_link_events_are_dropped() {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let new_srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let (mut c, t0) = connected();
        // the old link has a close, a login reply and a peer join queued but not yet pumped
        let (tx, rx) = mpsc::channel();
        let mut drv = TcpDriver {
            link: Some(TcpLink {
                rx,
                ctl: Default::default(),
                writer: None,
                broken: None,
            }),
            live: Default::default(),
            opts: Default::default(),
        };
        tx.send(LinkEvent::Frame(join_msg("g", "bob", "5.6.7.8", 99)))
            .unwrap();
        tx.send(LinkEvent::Frame(frame(Message::ClientLogin {
            result: 1,
            error: Some(""),
        })))
        .unwrap();
        tx.send(LinkEvent::Closed("old error".into())).unwrap();
        c.disconnect();
        c.connect(new_srv.local_addr().unwrap(), "alice", "");
        let mut ev = Vec::new();
        drv.pump(&mut c, &udp, t0, |e| ev.push(e));
        assert_eq!(ev, [Event::Disconnected { error: None }]);
        assert_eq!(c.state(), State::Connecting);
        // the new link really goes to the new server and completes
        let (_s, _) = new_srv.accept().unwrap();
        wait_for("new link", || {
            drv.pump(&mut c, &udp, Instant::now(), |e| ev.push(e));
            c.state() == State::Handshake
        });
        assert_eq!(ev.len(), 1);
    }

    /// No link thread outlives its link: connect/drop many times against a silent server.
    #[test]
    fn link_threads_exit() {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = srv.local_addr().unwrap();
        // accepts and keeps every connection, never says anything
        thread::spawn(move || {
            let mut keep = Vec::new();
            for s in srv.incoming() {
                keep.push(s);
            }
        });
        let mut drv = TcpDriver::default();
        let live = drv.live.clone();
        let mut c = Client::new(5000, 7);
        for i in 0..100u64 {
            c.connect(addr, "u", "");
            drv.pump(&mut c, &udp, Instant::now(), drop);
            // vary the moment of the drop: before, during and after the connect
            thread::sleep(Duration::from_micros(i * 37 % 1500));
            drv.pump(&mut c, &udp, Instant::now(), drop);
            c.disconnect();
            drv.pump(&mut c, &udp, Instant::now(), drop);
        }
        wait_for("link threads to exit", || live.load(Ordering::SeqCst) == 0);
        // and one that stays up has a reader and a writer
        c.connect(addr, "u", "");
        wait_for("connected", || {
            drv.pump(&mut c, &udp, Instant::now(), drop);
            c.state() == State::Handshake
        });
        assert_eq!(live.load(Ordering::SeqCst), 2);
        drop(drv);
        wait_for("threads after driver drop", || {
            live.load(Ordering::SeqCst) == 0
        });
    }

    /// Drops a link while its thread is between connecting and storing the stream, keeping the
    /// event channel open (as when the drop races the thread's first send). True if a link
    /// thread outlives it.
    fn leaks_after_early_drop(opts: LinkOpts) -> bool {
        let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = srv.local_addr().unwrap();
        // accepts and keeps one connection, never says anything
        let keep = thread::spawn(move || srv.accept());
        let live = Arc::new(AtomicUsize::new(0));
        let mut link = TcpLink::connect(addr, &live, opts).unwrap();
        let (_tx, dummy) = mpsc::sync_channel(1);
        let rx = std::mem::replace(&mut link.rx, dummy);
        drop(link); // well within `setup_delay`
        thread::sleep(opts.setup_delay + Duration::from_millis(1500)); // past the backstop
        drop(rx); // releases a writer sitting in the channel
        let end = Instant::now() + Duration::from_secs(2);
        while live.load(Ordering::SeqCst) > 0 && Instant::now() < end {
            thread::sleep(Duration::from_millis(5));
        }
        drop(keep);
        live.load(Ordering::SeqCst) > 0
    }

    #[test]
    fn closed_check_alone_ends_an_early_dropped_link() {
        let opts = LinkOpts {
            backstop: false,
            setup_delay: Duration::from_millis(200),
            ..LinkOpts::default()
        };
        assert!(!leaks_after_early_drop(opts));
    }

    #[test]
    fn read_timeout_alone_ends_an_early_dropped_link() {
        let opts = LinkOpts {
            check_closed: false,
            setup_delay: Duration::from_millis(200),
            ..LinkOpts::default()
        };
        assert!(!leaks_after_early_drop(opts));
    }

    #[test]
    fn writer_queue_is_bounded() {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let s = Arc::new(TcpStream::connect(l.local_addr().unwrap()).unwrap());
        let (_peer, _) = l.accept().unwrap(); // never reads
        s.set_write_timeout(Some(Duration::from_secs(30))).unwrap();
        // not a power of two, so unclamped doubling would overshoot it
        let cap = 100_000;
        let w = TcpWriter::spawn("test-tx", s.clone(), cap, (), drop).unwrap();
        // kernel buffers fill first, then the queue: send must fail long before 256 MiB
        let full = (0..256 * 1024).position(|_| {
            let r = w.send(&[0; 1024]);
            thread::sleep(Duration::from_micros(20));
            r.is_err()
        });
        assert!(full.is_some());
        // the writer is stuck on a full socket: what is left of the queue takes exactly that
        // many one-byte frames (no per-frame overhead), and the buffer stays within `cap`
        let room = {
            let q = lock(&w.q.0);
            cap - q.buf.len() - q.inflight
        };
        assert!((0..room).all(|_| w.send(&[0]).is_ok()));
        assert!(w.send(&[0]).is_err());
        assert!(lock(&w.q.0).buf.capacity() <= cap);
        s.shutdown(Shutdown::Both).unwrap();
        w.join(); // the blocked write ends with the shutdown
    }

    /// One pump handles at most PUMP_BUDGET frames; the rest come with the next pumps.
    #[test]
    fn pump_has_a_budget() {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let (mut c, _) = connected();
        let (tx, rx) = mpsc::sync_channel(1000);
        let mut drv = TcpDriver {
            link: Some(TcpLink {
                rx,
                ctl: Default::default(),
                writer: None,
                broken: None,
            }),
            live: Default::default(),
            opts: Default::default(),
        };
        let add = frame(Message::ClientGroupPublicAdd {
            group: "p",
            users: 1,
        });
        for _ in 0..PUMP_BUDGET + 10 {
            tx.send(LinkEvent::Frame(add.clone())).unwrap();
        }
        let mut n = 0;
        drv.pump(&mut c, &udp, Instant::now(), |_| n += 1);
        assert_eq!(n, PUMP_BUDGET);
        drv.pump(&mut c, &udp, Instant::now(), |_| n += 1);
        assert_eq!(n, PUMP_BUDGET + 10);
    }

    /// A client that is not pumped stops reading: the server's writes block once the kernel
    /// buffers are full, instead of the reader buffering without bound.
    #[test]
    fn unpumped_link_pushes_back_on_the_server() {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let srv = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut c = Client::new(5000, 7);
        let mut drv = TcpDriver::default();
        c.connect(srv.local_addr().unwrap(), "u", "");
        drv.pump(&mut c, &udp, Instant::now(), drop);
        let (mut s, _) = srv.accept().unwrap();
        wait_for("connected", || {
            drv.pump(&mut c, &udp, Instant::now(), drop);
            c.state() == State::Handshake
        });
        // no more pumps; the server sends 1 KiB frames until a write blocks for 1 s
        s.set_write_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut f = Vec::new();
        slip::encode(&[b'x'; 1024], &mut f);
        let mut sent = 0usize;
        while s.write_all(&f).is_ok() {
            sent += f.len();
            assert!(sent < 64 << 20, "client buffers without bound");
        }
        drop(drv);
    }
}
