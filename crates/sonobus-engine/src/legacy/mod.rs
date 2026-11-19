//! The legacy protocol, wire-compatible with stock SonoBus 1.7.x: AOO 2.0-pre sources and
//! sinks per peer, the connection-server client, and the `/sb/...` peer messages, all on one
//! UDP socket. This is the only module that knows about AOO or `/sb`; it drives the
//! protocol-neutral [`Model`](crate::model) and talks to the audio thread through
//! [`LinkCmd`]s (transport halves) and mixer commands.
//!
//! Upstream references are to `Source/SonobusPluginProcessor.cpp` ("P.cpp").
//!
//! Peer handshake (P.cpp:3455-3741, 4407-4477): each side gives the peer one sink and one
//! source with the same id (`ourId`, smallest free >= 1) and invites the remote's global
//! dummy source 0; the dummy's invite handler adds the inviting sink to the real source and
//! invites back the remote source with the remote's id. Each peer also has a mono echo pair
//! (`ourId + 40000`: whatever a peer's latency source sends our echo sink goes back out of our
//! echo source to whoever invited it) and a latency-test pair (`ourId + 20000`: our test sends
//! pulses to their echo sink and invites their echo source back to our latency sink), as
//! upstream (P.cpp:246-268, 5762-5818). The echo/latency streams use plain (non-compact) data
//! messages addressed by sink id, as upstream's `add_sink` without protocol flags gives.

mod links;
#[cfg(test)]
mod stock_tests;

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use aoo::client::{Client, Event as ClientEvent, State as ClientState, TcpDriver};
use aoo::codec::{
    Codec, Format, OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_SIGNAL_MUSIC, OpusOptions,
    PcmBitDepth,
};
use aoo::sink::{Sink, SinkEvent, SinkEventKind, SinkOptions};
use aoo::source::{Source, SourceEvent, SourceEventKind, SourceOptions};
use aoo::{ID_NONE, ID_WILDCARD, Message, Setup, TimeTag, message};
use sonobus_core::sbmsg::{self, SbMessage};
use sonobus_core::state::ServerConnectionInfo;
use sonobus_core::valuetree::ValueTree;

pub(crate) use links::{LinkCmd, Links, PeerLink};

use crate::formats::{FORMATS, SendCodec};
use crate::jitter::{Change, JitterMode};
use crate::latency::{ECHO_ID_OFFSET, LATENCY_ID_OFFSET, LatencyMeasurer, LatencyShared};
use crate::options;
use crate::proto::{Common, PeerCore, Proto};
use crate::{AudioCmd, Connection, Event, Host, Notification, PeerId, lock};

/// `PEER_PING_INTERVAL_MS` (P.cpp:42).
const PING_INTERVAL_MS: f64 = 2000.0;
/// AOO packet size SonoBus sources use (`RemotePeer::packetsize`).
const PACKET_SIZE: i32 = 600;
/// Stock receivers read the AOO user format into a buffer of this size (P.cpp:3767).
const USER_FORMAT_MAX: usize = sonobus_core::layout::USER_FORMAT_MAX_LEN;
/// Sinks per main source (SonoBus needs one: the peer's sink; a few allow re-invites from a
/// rebuilt remote peer before the old entry is uninvited).
const MAIN_MAX_SINKS: usize = 4;
/// Sources per echo / latency sink (see `connect_peer`).
const SIDE_MAX_SOURCES: usize = 4;
/// "access denied" retries of the original user name, and their spacing (PR #292).
const NAME_RETRIES: u32 = 3;
const NAME_RETRY_DELAY: Duration = Duration::from_millis(2500);
/// Reconnect backoff after an unexpected server loss (adopted from Commsbus/CanDuet).
const RECONNECT_MIN: Duration = Duration::from_secs(2);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// The send formats as AOO formats (`formatInfoToAooFormat`, P.cpp:6313-6341).
pub(crate) fn aoo_format(index: usize, nch: usize, sample_rate: u32, block: usize) -> Format {
    let f = &FORMATS[index.min(FORMATS.len() - 1)];
    let nch = nch.max(1) as i32;
    let codec = match f.codec {
        SendCodec::Opus {
            bitrate_per_channel,
        } => Codec::Opus(OpusOptions {
            bitrate: bitrate_per_channel * nch,
            complexity: 10,
            signal_type: OPUS_SIGNAL_MUSIC,
            application_type: OPUS_APPLICATION_RESTRICTED_LOWDELAY,
        }),
        SendCodec::Pcm { bytes } => Codec::Pcm(match bytes {
            3 => PcmBitDepth::Int24,
            4 => PcmBitDepth::Float32,
            _ => PcmBitDepth::Int16,
        }),
    };
    Format {
        nchannels: nch,
        samplerate: sample_rate as i32,
        blocksize: (block as i32).max(f.min_blocksize),
        codec,
    }
}

/// The table index of a received or changed format, if it is one of ours.
pub(crate) fn format_index(f: &Format) -> Option<usize> {
    crate::formats::find(match f.codec {
        Codec::Opus(o) => SendCodec::Opus {
            bitrate_per_channel: o.bitrate / f.nchannels.max(1),
        },
        Codec::Pcm(d) => SendCodec::Pcm {
            bytes: match d {
                PcmBitDepth::Int16 => 2,
                PcmBitDepth::Int24 => 3,
                PcmBitDepth::Float32 => 4,
                PcmBitDepth::Float64 => 8,
            },
        },
    })
}

/// `generateNewUsername` (SonobusPluginEditor.cpp:3835-3858): bump or append a trailing number.
pub(crate) fn next_user_name(name: &str) -> String {
    let bumped = name.rsplit_once(' ').and_then(|(head, last)| {
        // JUCE getIntValue: leading digits
        let digits: String = last.chars().take_while(char::is_ascii_digit).collect();
        let v: i64 = digits.parse().ok().filter(|&v| v > 0)?;
        Some(format!("{head} {}", v + 1))
    });
    bumped
        .unwrap_or_else(|| format!("{name} 2"))
        .trim()
        .to_owned()
}

/// One remote peer: network halves of its AOO pair and the per-peer protocol state.
pub(crate) struct Peer {
    pub c: PeerCore,
    our_id: i32,
    pub addr: SocketAddr,
    remote_sink_id: i32,
    remote_source_id: i32,
    /// Locked on its own: the send thread encodes under it without holding `Core`.
    source: Arc<Mutex<Source>>,
    sink: Sink,
    /// Our sink still invites the remote dummy source 0.
    inviting_dummy: bool,
    /// A stream format arrived since the server last announced the peer: its source knows
    /// us. A format recorded before a re-announce may be from a stream the peer has dropped.
    got_format: bool,
    pub recv_format: Option<Format>,
    last_ping_ms: f64,
    sent_first_info: bool,
    /// Echo service and latency-test halves (`ourId + 40000`, `ourId + 20000`).
    echo_sink: Sink,
    echo_source: Source,
    lat_sink: Sink,
    lat_source: Source,
    lat: Arc<LatencyShared>,
    lat_state_seen: i32,
}

impl Peer {
    fn src(&self) -> MutexGuard<'_, Source> {
        lock(&self.source)
    }

    fn recv_opus(&self) -> bool {
        matches!(self.recv_format.map(|f| f.codec), Some(Codec::Opus(_)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConnState {
    Disconnected,
    Connecting,
    Connected,
}

/// Reconnecting after a server loss, until the group is joined again, the server refuses
/// the group, or the user connects/disconnects.
struct Reconnect {
    delay: Duration,
    /// Next attempt; `None` while one is in progress.
    at: Option<Instant>,
}

pub(crate) struct Session {
    sock: Arc<UdpSocket>,
    pub local_port: u16,
    client: Client,
    tcp: TcpDriver,
    pub conn: ConnState,
    server: Option<(SocketAddr, String, u16)>,
    /// What both protocols keep: our name in use (may be auto-incremented), group, chat, ...
    pub c: Common,
    /// The name the user asked for.
    original_user: String,
    user_password: String,
    /// Group to join once logged in: (name, password, public).
    pending_join: Option<(String, String, bool)>,
    last_group: Option<(String, String, bool)>,
    pub public_groups: Vec<(String, i32)>,
    name_retries: u32,
    name_retry_at: Option<Instant>,
    /// Some while recovering from a server loss; the group to rejoin is `last_group`.
    reconnect: Option<Reconnect>,
    dummy: Source,
    pub peers: Vec<Peer>,
    pub blocked: Vec<IpAddr>,
    delayed_blocked_info: Vec<(Instant, SocketAddr)>,
    last_blocks: u64,
    source_events: Vec<SourceEvent>,
    buf: Vec<u8>,
    /// [`Self::same_host`]: whether an IPv4 address is one of this machine's.
    local_ips: HashMap<Ipv4Addr, bool>,
}

/// [`Session::same_host`] cache size (it is cleared when full).
const LOCAL_IPS_MAX: usize = 256;

pub(crate) fn send_to(sock: &UdpSocket, to: SocketAddr, bytes: &[u8], counter: &mut u64) {
    if sock.send_to(bytes, to).is_ok() {
        *counter += bytes.len() as u64;
    }
}

fn send_sb(sock: &UdpSocket, buf: &mut Vec<u8>, to: SocketAddr, m: &SbMessage, counter: &mut u64) {
    buf.clear();
    if m.encode(buf).is_ok() {
        send_to(sock, to, buf, counter);
    }
}

impl Session {
    pub fn new(sock: Arc<UdpSocket>, h: &Host) -> Self {
        let local_port = sock.local_addr().map_or(0, |a| a.port());
        let cfg = h.cfg;
        let setup = Setup {
            samplerate: cfg.sample_rate as i32,
            blocksize: cfg.block_size as i32,
            nchannels: cfg.input_channels.max(1) as i32,
        };
        // the dummy only ever receives invites; it is never started (P.cpp:918, 6980)
        let (dummy, _) = Source::new(0, setup, SourceOptions::default());
        Self {
            sock,
            local_port,
            client: Client::new(local_port, aoo::client::random_seed()),
            tcp: TcpDriver::default(),
            conn: ConnState::Disconnected,
            server: None,
            c: Common::new(),
            original_user: String::new(),
            user_password: String::new(),
            pending_join: None,
            last_group: None,
            public_groups: Vec::new(),
            name_retries: 0,
            name_retry_at: None,
            reconnect: None,
            dummy,
            peers: Vec::new(),
            blocked: Vec::new(),
            delayed_blocked_info: Vec::new(),
            local_ips: HashMap::new(),
            last_blocks: 0,
            source_events: Vec::new(),
            buf: Vec::with_capacity(aoo::MAX_PACKET_SIZE),
        }
    }

    /// Every peer's source, for the send thread to encode and send without `Core`.
    pub fn sources(&self, out: &mut Vec<(PeerId, Arc<Mutex<Source>>)>) {
        out.extend(self.peers.iter().map(|p| (p.c.id, p.source.clone())));
    }

    /// What the send thread sent for a peer.
    pub fn add_sent(&mut self, id: PeerId, bytes: u64, packets: i64) {
        if let Some(i) = self.peer_index(id) {
            let p = &mut self.peers[i].c;
            p.bytes_sent += bytes;
            p.packets_sent += packets;
        }
    }

    /// One address per peer socket on this machine: every local IPv4 address (loopback or an
    /// interface's) becomes 127.0.0.1, the port kept; other addresses are unchanged. A peer on
    /// this host has several addresses and the kernel picks our datagrams' source address by
    /// their destination: when a host joined its own server through its LAN address and a
    /// second client on the same machine through 127.0.0.1, each side's handshake may resolve
    /// the other through a different alias (whichever ping landed first), and every datagram
    /// then came from an address the receiving side did not know the peer by: listed, but no
    /// audio. Peer addresses and the sources of incoming datagrams go through this, so both
    /// aliases are one peer; 127.0.0.1 reaches any socket bound to all interfaces.
    fn same_host(&mut self, a: SocketAddr) -> SocketAddr {
        let SocketAddr::V4(v4) = a else { return a };
        let ip = *v4.ip();
        if ip == Ipv4Addr::LOCALHOST || ip.is_unspecified() || ip.is_broadcast() || ip.is_multicast()
        {
            return a;
        }
        if self.local_ips.len() >= LOCAL_IPS_MAX && !self.local_ips.contains_key(&ip) {
            self.local_ips.clear();
        }
        // only an address of this machine can be bound
        let local = ip.is_loopback()
            || *self
                .local_ips
                .entry(ip)
                .or_insert_with(|| UdpSocket::bind((ip, 0)).is_ok());
        if local {
            SocketAddr::from((Ipv4Addr::LOCALHOST, a.port()))
        } else {
            a
        }
    }

    fn find(&self, addr: SocketAddr) -> Option<usize> {
        self.peers.iter().position(|p| p.addr == addr)
    }

    /// `findRemotePeer(endpoint, ourId)`; a negative id matches any peer at the address.
    fn find_id(&self, addr: SocketAddr, our_id: i32) -> Option<usize> {
        self.peers
            .iter()
            .position(|p| p.addr == addr && (our_id < 0 || p.our_id == our_id))
    }

    fn find_by_remote_sink(&self, addr: SocketAddr, id: i32) -> Option<usize> {
        self.peers
            .iter()
            .position(|p| p.addr == addr && p.remote_sink_id == id)
    }


    pub fn connection(&self) -> Connection {
        match self.conn {
            ConnState::Disconnected => Connection::Disconnected,
            ConnState::Connecting => Connection::Connecting,
            ConnState::Connected => Connection::Connected,
        }
    }

    fn is_blocked(&self, addr: SocketAddr) -> bool {
        self.blocked.contains(&addr.ip())
    }

    // ---- server connection --------------------------------------------------------------

    #[allow(clippy::too_many_arguments)]
    pub fn connect(
        &mut self,
        h: &mut Host,
        addr: SocketAddr,
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        group: Option<(String, String, bool)>,
    ) {
        self.reconnect = None;
        self.name_retries = 0;
        self.name_retry_at = None;
        if self.client.state() != ClientState::Disconnected {
            self.client.disconnect();
        }
        // a fresh connection drops all peers (`connectToServer`, P.cpp:1131-1133)
        self.remove_all_peers(h);
        self.server = Some((addr, host.to_owned(), port));
        self.c.user = user.to_owned();
        self.original_user = user.to_owned();
        self.user_password = password.to_owned();
        self.pending_join = group.clone();
        self.last_group = group;
        self.conn = ConnState::Connecting;
        self.client.connect(addr, user, password);
    }

    /// Deliberate disconnect (`disconnectFromServer`): no reconnect, all peers removed.
    pub fn disconnect(&mut self, h: &mut Host) {
        self.reconnect = None;
        self.name_retry_at = None;
        self.pending_join = None;
        self.last_group = None;
        self.client.disconnect();
        self.remove_all_peers(h);
        self.conn = ConnState::Disconnected;
        self.c.group = None;
        self.public_groups.clear();
    }

    pub fn join_group(&mut self, h: &mut Host, group: &str, password: &str, public: bool) {
        self.last_group = Some((group.to_owned(), password.to_owned(), public));
        if self.conn == ConnState::Connected {
            self.client.watch_public_groups(false);
            self.client.join_group(group, password, public);
        } else {
            h.event(Event::GroupJoinFailed {
                group: group.to_owned(),
                error: "not connected".into(),
            });
        }
    }

    pub fn leave_group(&mut self) {
        if let Some(g) = self.c.group.take() {
            self.last_group = None;
            self.client.leave_group(&g);
        }
    }

    pub fn watch_public_groups(&mut self, watch: bool) {
        self.public_groups.clear();
        self.client.watch_public_groups(watch);
    }

    fn on_client_event(&mut self, h: &mut Host, e: ClientEvent, now: Instant) {
        match e {
            ClientEvent::Connected => {
                self.conn = ConnState::Connected;
                self.name_retries = 0;
                self.name_retry_at = None;
                h.event(Event::Connected {
                    user: self.c.user.clone(),
                });
                match self.pending_join.take() {
                    Some((g, pw, public)) => {
                        self.add_recent(h, &g, &pw, public);
                        self.client.watch_public_groups(false);
                        self.client.join_group(&g, &pw, public);
                    }
                    None => self.client.watch_public_groups(true),
                }
            }
            ClientEvent::ConnectFailed { reason } => {
                self.conn = ConnState::Disconnected;
                if reason.contains("access denied") && self.server.is_some() {
                    // PR #292: our own stale session holds the name after a drop; retry it
                    // a few times before taking another name
                    if self.c.user == self.original_user && self.name_retries < NAME_RETRIES {
                        self.name_retries += 1;
                        self.name_retry_at = Some(now + NAME_RETRY_DELAY);
                        h.event(Event::UserNameInUse {
                            retrying: true,
                            user: self.c.user.clone(),
                        });
                    } else {
                        self.c.user = next_user_name(&self.c.user);
                        h.event(Event::UserNameInUse {
                            retrying: false,
                            user: self.c.user.clone(),
                        });
                        self.name_retry_at = Some(now);
                    }
                    self.conn = ConnState::Connecting;
                    return;
                }
                self.name_retry_at = None;
                match &mut self.reconnect {
                    Some(r) => {
                        r.delay = (r.delay * 2).min(RECONNECT_MAX);
                        r.at = Some(now + r.delay);
                    }
                    None => self.pending_join = None,
                }
                h.event(Event::ConnectFailed { reason });
            }
            ClientEvent::Disconnected { error } => {
                self.conn = ConnState::Disconnected;
                self.public_groups.clear();
                let lost = error.is_some();
                // keep the peers: their audio is peer to peer (P.cpp:4204-4227). Like
                // upstream's `mCurrentJoinedGroup`, the group we want back outlives the loss:
                // a loss while already recovering (say before the rejoin went through)
                // schedules the next attempt
                let in_group = self.c.group.is_some() || self.reconnect.is_some();
                if lost && in_group && h.model.state.extra.reconnect_after_server_loss {
                    let delay = self
                        .reconnect
                        .as_ref()
                        .map_or(RECONNECT_MIN, |r| (r.delay * 2).min(RECONNECT_MAX));
                    self.reconnect = Some(Reconnect {
                        delay,
                        at: Some(now + delay),
                    });
                    self.pending_join = self.last_group.clone();
                }
                self.c.group = None;
                h.event(Event::Disconnected { error });
            }
            ClientEvent::GroupJoined { group } => {
                self.c.group = Some(group.clone());
                self.reconnect = None;
                h.event(Event::GroupJoined { group });
            }
            ClientEvent::GroupJoinFailed { group, error } => {
                // the server is back and refuses the group (wrong password, ...): retrying
                // cannot help; we stay logged in, the peers keep streaming (upstream never
                // retries a join either)
                self.reconnect = None;
                h.event(Event::GroupJoinFailed { group, error });
            }
            ClientEvent::GroupLeft { group } => {
                self.c.group = None;
                self.remove_all_peers(h);
                h.event(Event::GroupLeft { group });
            }
            ClientEvent::GroupLeaveFailed { group, error } => h.event(Event::Error(format!(
                "couldn't leave group {group}: {error}"
            ))),
            ClientEvent::PublicGroupAdded { group, users } => {
                match self.public_groups.iter_mut().find(|(g, _)| *g == group) {
                    Some(e) => e.1 = users,
                    None => self.public_groups.push((group, users)),
                }
                h.event(Event::PublicGroupsChanged);
            }
            ClientEvent::PublicGroupRemoved { group } => {
                self.public_groups.retain(|(g, _)| *g != group);
                h.event(Event::PublicGroupsChanged);
            }
            ClientEvent::PeerJoined {
                group, user, addr, ..
            } => {
                let addr = self.same_host(addr);
                // the one place peer names enter (see `clean_name`)
                let user = crate::proto::clean_name(&user);
                if self.is_blocked(addr) {
                    h.event(Event::PeerJoinBlocked { user, group, addr });
                    self.delayed_blocked_info
                        .push((now + Duration::from_millis(400), addr));
                } else {
                    let reciprocate = !h.model.state.params.main_recv_mute;
                    self.connect_peer(h, addr, &user, &group, reciprocate, true);
                }
            }
            ClientEvent::PeerJoinFailed { group, user } => {
                h.event(Event::PeerJoinFailed { user, group })
            }
            ClientEvent::PeerLeft { addr, .. } => {
                if let Some(addr) = addr.map(|a| self.same_host(a)) {
                    while let Some(i) = self.find(addr) {
                        self.remove_peer_at(h, i, false);
                    }
                }
            }
            ClientEvent::ServerError(e) => h.event(Event::Error(e)),
        }
    }

    fn add_recent(&mut self, h: &mut Host, group: &str, group_password: &str, public: bool) {
        let Some((_, host, port)) = &self.server else {
            return;
        };
        let info = ServerConnectionInfo {
            user_name: self.original_user.clone(),
            user_password: self.user_password.clone(),
            group_name: group.to_owned(),
            group_password: group_password.to_owned(),
            server_host: host.clone(),
            server_port: i32::from(*port),
            timestamp: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_millis() as i64),
            group_is_public: public,
        };
        let recents = &mut h.model.state.recents;
        recents.retain(|r| {
            ServerConnectionInfo {
                timestamp: info.timestamp,
                ..r.clone()
            } != info
        });
        recents.insert(0, info);
        recents.truncate(10);
    }

    fn run_timers(&mut self, h: &mut Host, now: Instant) {
        let due = |t: Option<Instant>| t.is_some_and(|t| now >= t);
        if due(self.name_retry_at) {
            self.name_retry_at = None;
            if let Some((addr, ..)) = self.server {
                self.client.connect(addr, &self.c.user, &self.user_password);
            }
        }
        if let Some(r) = &mut self.reconnect
            && due(r.at)
            && self.client.state() == ClientState::Disconnected
        {
            r.at = None;
            if let Some((addr, ..)) = self.server {
                self.c.user = self.original_user.clone();
                self.pending_join = self.last_group.clone();
                self.conn = ConnState::Connecting;
                self.client.connect(addr, &self.c.user, &self.user_password);
                h.event(Event::Reconnecting);
            }
        }
        let mut i = 0;
        while i < self.delayed_blocked_info.len() {
            if now >= self.delayed_blocked_info[i].0 {
                let (_, addr) = self.delayed_blocked_info.swap_remove(i);
                let m = SbMessage::BlockedInfo {
                    from: &self.c.user,
                    blocked: true,
                };
                send_sb(&self.sock, &mut self.buf, addr, &m, &mut 0);
            } else {
                i += 1;
            }
        }
    }

    // ---- peers --------------------------------------------------------------------------

    /// `connectRemotePeerRaw` (server peers, `raw`) / `connectRemotePeer` (direct).
    pub fn connect_peer(
        &mut self,
        h: &mut Host,
        addr: SocketAddr,
        user: &str,
        group: &str,
        reciprocate: bool,
        raw: bool,
    ) {
        let _ = reciprocate; // upstream stores it (`invitedPeer`) but never reads it
        let addr = self.same_host(addr);
        let Some(i) = self.add_peer_if_necessary(h, addr, user, group) else {
            return;
        };
        let p = &mut self.peers[i];
        p.c.recv_allow = !h.model.state.params.main_recv_mute;
        // our sink invites their dummy source 0 (special id), until a format of their real
        // stream arrives *after* this announcement: when the server re-announces a peer after
        // its own server loss, it may have rebuilt its side, and a stock peer drops invites
        // for peers it does not know yet (P.cpp:3511-3515)
        p.sink.invite_source(addr, 0);
        p.inviting_dummy = true;
        p.got_format = false;
        p.c.connected = true;
        if !h.model.state.params.main_send_mute {
            p.c.send_active = true;
            if !p.src().is_playing() {
                p.src().start();
            }
            self.send_layout(h, i);
            if raw {
                self.send_peer_info(h, i);
            }
        } else {
            p.c.send_active = false;
            p.src().stop();
        }
        if !raw {
            self.send_peer_info(h, i);
        }
        let p = &mut self.peers[i];
        let m = SbMessage::BlockedInfo {
            from: &self.c.user,
            blocked: false,
        };
        send_sb(&self.sock, &mut self.buf, addr, &m, &mut p.c.bytes_sent);
        h.event(Event::PeerChanged { id: p.c.id });
    }

    /// `doAddRemotePeerIfNecessary` (P.cpp:5959-6136).
    fn add_peer_if_necessary(
        &mut self,
        h: &mut Host,
        addr: SocketAddr,
        user: &str,
        group: &str,
    ) -> Option<usize> {
        if let Some(i) = self.find(addr) {
            let p = &mut self.peers[i].c;
            if !user.is_empty() && p.user.is_empty() {
                p.user = user.to_owned();
                p.group = group.to_owned();
                if let Some(c) = h.model.find_cache(user).cloned() {
                    h.model.set_peer_user(p.slot, user, Some(&c));
                    p.format_index = crate::formats::checked(c.send_format);
                    p.jitter.mode = JitterMode::from_i32(c.netbufauto);
                    p.jitter.buffer_ms = c.netbuf;
                    self.push_send_format(h, i);
                    self.apply_buffer(h, i);
                } else {
                    h.model.set_peer_user(p.slot, user, None);
                }
                let slot = self.peers[i].c.slot;
                crate::proto::restore_solo(h, slot, user);
                h.publish_peer(slot);
                let rec = self.peer_rec(h, i);
                h.with_rec(|r, c| r.peer_joined(c, &rec));
            }
            return Some(i);
        }
        let c = PeerCore::add(h, self.peers.len(), user, group, &addr.to_string())?;
        let mut our_id = 1;
        while self.peers.iter().any(|p| p.our_id == our_id) {
            our_id += 1;
        }
        let cfg = h.cfg;
        let block_ms = cfg.block_ms();
        let dynres = h.model.state.params.dynamic_resampling;
        let sink_setup = Setup {
            samplerate: cfg.sample_rate as i32,
            blocksize: cfg.block_size as i32,
            nchannels: cfg.output_channels.max(1) as i32,
        };
        let (sink, sink_audio) = Sink::new(
            our_id,
            sink_setup,
            SinkOptions {
                buffersize_ms: c.jitter.buffer_ms as i32,
                dynamic_resampling: dynres,
                ..SinkOptions::default()
            },
        );
        let src_setup = Setup {
            nchannels: c.send_channels as i32,
            ..sink_setup
        };
        let send_buf_ms = (2.0 * block_ms).max(10.0).ceil() as i32;
        let opts = SourceOptions {
            buffersize_ms: send_buf_ms,
            packetsize: PACKET_SIZE,
            dynamic_resampling: dynres,
            ping_interval_ms: 2000,
            respect_codec_change: true,
            redundancy: options::packet_redundancy(&h.model.state.extra),
            ..SourceOptions::default()
        };
        let (mut source, source_audio) = Source::new(our_id, src_setup, opts);
        // the echo and latency pairs: mono, never resampled (P.cpp:6037-6070)
        let mono = Setup {
            nchannels: 1,
            ..sink_setup
        };
        let side_sink = SinkOptions {
            buffersize_ms: c.jitter.buffer_ms as i32,
            dynamic_resampling: false,
            ..SinkOptions::default()
        };
        let side_source = SourceOptions {
            packetsize: PACKET_SIZE,
            dynamic_resampling: false,
            ping_interval_ms: 2000,
            respect_codec_change: true,
            ..SourceOptions::default()
        };
        let format = c.format_index;
        let mono_format = aoo_format(format, 1, cfg.sample_rate, cfg.block_size);
        let (mut echo_sink, echo_sink_audio) =
            Sink::new(our_id + ECHO_ID_OFFSET, mono, side_sink);
        let (mut lat_sink, lat_sink_audio) =
            Sink::new(our_id + LATENCY_ID_OFFSET, mono, side_sink);
        // each has one expected source (filtered in `on_packet`); a few in case the peer's
        // id for us changes while we keep the peer (entries are never removed)
        echo_sink.set_max_sources(SIDE_MAX_SOURCES);
        lat_sink.set_max_sources(SIDE_MAX_SOURCES);
        let echo_opts = SourceOptions {
            buffersize_ms: block_ms.ceil() as i32,
            ..side_source
        };
        let (mut echo_source, echo_source_audio) =
            Source::new(our_id + ECHO_ID_OFFSET, mono, echo_opts);
        let (mut lat_source, lat_source_audio) =
            Source::new(our_id + LATENCY_ID_OFFSET, mono, side_source);
        let _ = echo_source.set_format(mono_format);
        let _ = lat_source.set_format(mono_format);
        // one destination each (a new invite replaces it): the echo goes back 1:1, in our
        // mono format, to the one latency sink testing us
        echo_source.set_max_sinks(1);
        lat_source.set_max_sinks(1);
        // SonoBus sends a peer's stream to that peer's one sink; a few for re-invites
        source.set_max_sinks(MAIN_MAX_SINKS);
        let lat = Arc::new(LatencyShared::default());
        // no AOO user format: upstream never sets one (P.cpp:5241, 6036, 7118 are commented
        // out); the layout goes out as `/sb/clayinfo`, and changing a user format would
        // restart the stream
        let _ = source.set_format(aoo_format(
            format,
            c.send_channels,
            cfg.sample_rate,
            cfg.block_size,
        ));
        h.tx.send(AudioCmd::Link(LinkCmd::Add(
            c.slot,
            Box::new(PeerLink {
                sink: Box::new(sink_audio),
                source: Box::new(source_audio),
                echo_sink: echo_sink_audio,
                echo_source: echo_source_audio,
                lat_sink: lat_sink_audio,
                lat_source: lat_source_audio,
                measurer: LatencyMeasurer::new(),
                lat: lat.clone(),
                lat_seen: 0,
                scratch: vec![0.0; cfg.block_size],
            }),
        )));
        // first ping after half an interval
        let last_ping_ms = h.now_ms() - PING_INTERVAL_MS / 2.0;
        self.peers.push(Peer {
            c,
            our_id,
            addr,
            remote_sink_id: ID_NONE,
            remote_source_id: ID_NONE,
            source: Arc::new(Mutex::new(source)),
            sink,
            inviting_dummy: false,
            got_format: false,
            recv_format: None,
            last_ping_ms,
            sent_first_info: false,
            echo_sink,
            echo_source,
            lat_sink,
            lat_source,
            lat,
            lat_state_seen: 0,
        });
        let i = self.peers.len() - 1;
        self.peer_added(h, i);
        Some(i)
    }

    // ---- receive ------------------------------------------------------------------------

    /// One datagram from the shared socket (`doReceiveData`, P.cpp:2265-2413).
    ///
    /// Stream traffic is tied to the peer it belongs to (upstream hands it to any sink with
    /// the id): a peer's sink and source only see datagrams from that peer's address, the one
    /// the server's hole punching resolved (or that connected to us directly). A peer that
    /// moves is announced again by the server and becomes a new peer at its new address. So
    /// no other host can add a source to a peer's sink (and have it mixed into the peer's
    /// strip) or invite a peer's source. Only the dummy source 0 listens to everyone (that is
    /// how a serverless peer connects). Blocked addresses are dropped before parsing, except
    /// for the server's own address.
    pub fn on_packet(&mut self, h: &mut Host, raw_from: SocketAddr, data: &[u8]) {
        // the client matches the server and handshake pings by the addresses as announced
        let from = if self.client.server_addr() == Some(raw_from) {
            raw_from
        } else {
            self.same_host(raw_from)
        };
        if self.is_blocked(from) && self.client.server_addr() != Some(from) {
            return;
        }
        let now = TimeTag::now();
        let ms = h.now_ms();
        let peer = self.find(from);
        if let Some(i) = peer {
            self.peers[i].c.bytes_received += data.len() as u64;
        }
        let Ok(msg) = message::parse(data) else {
            return;
        };
        use Message as M;
        match msg {
            M::CompactData { .. } => {
                if let Some(i) = peer {
                    let p = &mut self.peers[i];
                    if p.sink.handle_message(from, &msg, now) {
                        Self::got_data(p, ms);
                    }
                }
            }
            M::SinkFormat { sink, .. } | M::SinkData { sink, .. } | M::SinkPing { sink, .. } => {
                if let Some(i) = self.find_id(from, if sink == ID_WILDCARD { -1 } else { sink }) {
                    let p = &mut self.peers[i];
                    if p.sink.handle_message(from, &msg, now) {
                        Self::got_data(p, ms);
                    }
                } else if let Some((i, echo)) = self.find_side(from, sink) {
                    let p = &mut self.peers[i];
                    // only the peer's own counterpart: its latency source into our echo
                    // sink, its echo source into our latency sink
                    let want = p.remote_source_id
                        + if echo {
                            LATENCY_ID_OFFSET
                        } else {
                            ECHO_ID_OFFSET
                        };
                    let src = match msg {
                        M::SinkFormat { src, .. }
                        | M::SinkData { src, .. }
                        | M::SinkPing { src, .. } => src,
                        _ => ID_NONE,
                    };
                    if p.remote_source_id != ID_NONE && src == want {
                        let s = if echo {
                            &mut p.echo_sink
                        } else {
                            &mut p.lat_sink
                        };
                        s.handle_message(from, &msg, now);
                    }
                }
            }
            M::SourceFormat { src, .. }
            | M::SourceData { src, .. }
            | M::SourcePing { src, .. }
            | M::SourceInvite { src, .. }
            | M::SourceUninvite { src, .. }
            | M::SourceCodecChange { src, .. } => {
                if src == 0 {
                    self.dummy.handle_message(from, &msg, now);
                } else if let Some((i, echo)) = self.find_side(from, src) {
                    self.side_source_message(i, echo, from, &msg, now);
                } else if let Some(i) =
                    self.find_id(from, if src == ID_WILDCARD { -1 } else { src })
                {
                    // the source's own events now, without touching the other sources (the
                    // send thread may be encoding them)
                    let our_id = self.peers[i].our_id;
                    let mut events = std::mem::take(&mut self.source_events);
                    {
                        let mut s = self.peers[i].src();
                        s.handle_message(from, &msg, now);
                        events.extend(std::iter::from_fn(|| s.poll_event()));
                    }
                    for e in events.drain(..) {
                        self.on_source_event(h, our_id, e);
                    }
                    self.source_events = events;
                }
            }
            M::NotAoo(raw) => return self.on_sb(h, from, raw),
            // server-bound messages are not ours; the rest is the client's
            M::ServerRequest
            | M::ServerPing
            | M::ServerLogin { .. }
            | M::ServerGroupJoin { .. }
            | M::ServerGroupLeave { .. }
            | M::ServerGroupPublic { .. } => {}
            _ => self.client.handle_udp(raw_from, &msg, Instant::now()),
        }
        self.process_events(h, false);
    }

    /// The peer at `addr` whose echo (`true`) or latency pair has this id.
    fn find_side(&self, addr: SocketAddr, id: i32) -> Option<(usize, bool)> {
        self.peers.iter().enumerate().find_map(|(i, p)| {
            if p.addr != addr {
                None
            } else if p.our_id + ECHO_ID_OFFSET == id {
                Some((i, true))
            } else if p.our_id + LATENCY_ID_OFFSET == id {
                Some((i, false))
            } else {
                None
            }
        })
    }

    /// Our echo or latency source got a message: an invite makes it send to the inviting
    /// sink, an uninvite stops it (P.cpp:3578-3593, 3636-3647; upstream dereferences the
    /// null `echopeer` in both latency branches, we use the latency peer).
    fn side_source_message(
        &mut self,
        i: usize,
        echo: bool,
        from: SocketAddr,
        msg: &Message,
        now: TimeTag,
    ) {
        let p = &mut self.peers[i];
        let src = if echo {
            &mut p.echo_source
        } else {
            &mut p.lat_source
        };
        src.handle_message(from, msg, now);
        while let Some(e) = src.poll_event() {
            match e.kind {
                SourceEventKind::Invite { .. } => {
                    src.remove_all();
                    src.add_sink(e.addr, e.id, 0);
                    src.start();
                }
                SourceEventKind::Uninvite => {
                    src.remove_sink(e.addr, e.id);
                    src.stop();
                }
                _ => {}
            }
        }
    }

    fn got_data(p: &mut Peer, ms: f64) {
        // their stream reaches us: their source knows us (see `connect_peer`)
        p.got_format = true;
        p.c.got_data(ms);
    }

    /// `/sb/...` (`handleOtherMessage`, P.cpp:2561-2859). Malformed input is dropped.
    ///
    /// Only current peers are heard, after the block list. Upstream needs a peer only for
    /// `pinfo`, `clayinfo`, `blockedinfo` and `pngack`; it answers `ping` and `reqlatinfo` and
    /// takes `chat`, `latinfo`, `suggestlat` and `suggestgroup` from any address that is not
    /// blocked, so any host could make it reflect traffic, read its peers' names and
    /// latencies, or flood its UI. Stock clients send all of these only to the peers in their
    /// own list, which the server announced to both sides at the same time: a message that
    /// arrives before our side has the peer (its handshake still running) is dropped, which
    /// costs at most one ping sample or, rarely, a chat line sent in the first second.
    /// Everything but the pings goes to the shared handler ([`Proto::handle_sb`]).
    fn on_sb(&mut self, h: &mut Host, from: SocketAddr, raw: &[u8]) {
        if self.is_blocked(from) {
            return;
        }
        let Some(pi) = self.find(from) else {
            return;
        };
        let Ok(Some(m)) = sbmsg::parse(raw) else {
            return;
        };
        match m {
            SbMessage::Ping { origin_time } => {
                let reply = SbMessage::PingAck {
                    origin_time,
                    reply_time: TimeTag::now().0,
                };
                self.send_sb(pi, &reply);
            }
            SbMessage::PingAck { origin_time, .. } => {
                let rtt = TimeTag::duration(TimeTag(origin_time), TimeTag::now()) * 1000.0;
                let p = &mut self.peers[pi].c;
                p.ping.push(rtt);
                p.ping.new_style = true;
            }
            m => self.handle_sb(h, pi, m, raw),
        }
    }

    // ---- AOO events -------------------------------------------------------------------------

    /// Events of the dummy, the sinks and (`sources`: on the send thread only, which does not
    /// encode meanwhile) the sources.
    fn process_events(&mut self, h: &mut Host, sources: bool) {
        while let Some(e) = self.dummy.poll_event() {
            self.on_dummy_event(h, e);
        }
        let mut i = 0;
        while i < self.peers.len() {
            let our_id = self.peers[i].our_id;
            if sources {
                loop {
                    let e = self.peers[i].src().poll_event();
                    let Some(e) = e else { break };
                    self.on_source_event(h, our_id, e);
                }
            }
            while let Some(e) = self.peers[i].sink.poll_event() {
                self.on_sink_event(h, our_id, e);
            }
            i += 1;
        }
    }

    /// Invite to the global source 0 (P.cpp:3499-3548).
    fn on_dummy_event(&mut self, h: &mut Host, e: SourceEvent) {
        let SourceEventKind::Invite { flags } = e.kind else {
            return;
        };
        let addr = e.addr;
        let i = match self.find_by_remote_sink(addr, e.id) {
            Some(i) => Some(i),
            // serverless: anyone may connect (we still honour our block list)
            None if self.conn != ConnState::Connected && !self.is_blocked(addr) => {
                self.add_peer_if_necessary(h, addr, "", "")
            }
            // in a group only peers the server announced
            None => self.find(addr),
        };
        let Some(i) = i else { return };
        let p = &mut self.peers[i];
        p.remote_sink_id = e.id;
        if !p.src().add_sink(addr, e.id, flags) {
            p.src().set_sink_protocol_flags(addr, e.id, flags);
        }
        if p.c.send_allow {
            if !p.src().is_playing() {
                p.src().start();
            }
            p.c.send_active = true;
        } else {
            p.src().stop();
            p.c.send_active = false;
        }
        // invite back their source with the same id
        p.remote_source_id = p.remote_sink_id;
        // once its stream reaches us it runs; inviting again would only make our sink repeat
        // the invitation (it stops repeating when a format arrives). A format recorded before
        // the peer was re-announced does not count: it may be from a stream they dropped
        if !p.got_format {
            p.sink.invite_source(addr, p.remote_source_id);
        }
        // they know us now (they invited us): our layout, with the id they will look us up
        // by. Upstream sends it only on an invite of the real source, which raises no event
        // once this handler has added their sink to it; and the one sent on connect is lost
        // when it overtakes their own server announcement
        self.send_layout(h, i);
    }

    fn on_source_event(&mut self, h: &mut Host, our_id: i32, e: SourceEvent) {
        let addr = e.addr;
        match e.kind {
            SourceEventKind::Invite { flags } => {
                // echo/latency source ids (stage 2) find no peer and are ignored
                let Some(i) = self.find_id(addr, our_id) else {
                    return;
                };
                let p = &mut self.peers[i];
                p.remote_sink_id = e.id;
                if !p.src().add_sink(addr, e.id, flags) {
                    p.src().set_sink_protocol_flags(addr, e.id, flags);
                }
                if p.c.send_allow {
                    if !p.src().is_playing() {
                        p.src().start();
                    }
                    p.c.send_active = true;
                } else {
                    p.c.send_active = false;
                    p.src().stop();
                }
                p.c.connected = true;
                self.send_layout(h, i);
                self.send_peer_info(h, i);
                h.event(Event::PeerChanged {
                    id: self.peers[i].c.id,
                });
            }
            SourceEventKind::Uninvite => {
                if let Some(i) = self.find_by_remote_sink(addr, e.id) {
                    let p = &mut self.peers[i];
                    p.src().remove_all();
                    p.c.send_active = false;
                    p.c.packets_sent = 0;
                    if !p.c.recv_active {
                        p.c.connected = false;
                    }
                    h.event(Event::PeerChanged { id: p.c.id });
                }
            }
            SourceEventKind::CodecChanged { format } => {
                if let Some(i) = self.find_by_remote_sink(addr, e.id)
                    && let Some(asked) = format_index(&format)
                {
                    // the source applied the peer's request; what we allow (requests
                    // honoured at all, "Max Send Quality") is put back in place
                    let p = &mut self.peers[i];
                    let fi = options::requested_format(&h.model.state.extra, asked)
                        .unwrap_or(p.c.format_index);
                    if fi != asked {
                        let (sr, block) = (h.cfg.sample_rate, h.cfg.block_size);
                        let f = aoo_format(fi, p.c.send_channels, sr, block);
                        let _ = p.src().set_format(f);
                    }
                    // the latency and echo sources follow the main one (P.cpp:3667-3673)
                    let mono = aoo_format(fi, 1, h.cfg.sample_rate, h.cfg.block_size);
                    let _ = p.echo_source.set_format(mono);
                    let _ = p.lat_source.set_format(mono);
                    self.peers[i].c.format_index = fi;
                    h.event(Event::PeerChanged {
                        id: self.peers[i].c.id,
                    });
                }
            }
            SourceEventKind::Ping { rtt, .. } => {
                if let Some(i) = self.find_id(addr, our_id)
                    && !self.peers[i].c.ping.new_style
                {
                    self.peers[i].c.ping.push(rtt * 1000.0);
                }
            }
            SourceEventKind::FormatRequested => {}
        }
    }

    fn on_sink_event(&mut self, h: &mut Host, our_id: i32, e: SinkEvent) {
        let Some(i) = self.find_id(e.addr, our_id) else {
            return;
        };
        let (addr, src) = (e.addr, e.id);
        let block_ms = h.cfg.block_ms();
        let ms = h.now_ms();
        let p = &mut self.peers[i];
        match e.kind {
            SinkEventKind::SourceAdded => {
                if src == 0 {
                    return; // our own invitation of their dummy
                }
                p.remote_source_id = src;
                if p.inviting_dummy {
                    p.sink.uninvite_source(addr, 0);
                    p.inviting_dummy = false;
                }
                if p.c.recv_allow {
                    // upstream invites once; the format may already be here, and inviting
                    // again would make our sink repeat the invitation until the next one
                    if p.sink.source_format(addr, src).is_none() {
                        p.sink.invite_source(addr, src);
                    }
                } else {
                    p.sink.uninvite_source(addr, src);
                    p.c.recv_active = false;
                }
                p.c.connected = true;
                p.c.sync_shared();
            }
            SinkEventKind::FormatChanged {
                format,
                user_format,
            } => {
                p.got_format = true;
                if p.inviting_dummy {
                    p.sink.uninvite_source(addr, 0);
                    p.inviting_dummy = false;
                }
                let slot = p.c.slot;
                let mut got_layout = false;
                if !user_format.is_empty()
                    && user_format.len() <= USER_FORMAT_MAX
                    && let Ok(tree) = ValueTree::read(&user_format)
                {
                    h.model.apply_layout(slot, &tree);
                    got_layout = true;
                }
                let nch = format.nchannels.clamp(0, crate::mixer::MAX_CHANNELS as i32) as usize;
                if h.model.set_recv_channels(slot, nch, got_layout) {
                    let cfg = h.cfg;
                    let setup = Setup {
                        samplerate: cfg.sample_rate as i32,
                        blocksize: cfg.block_size as i32,
                        nchannels: cfg.output_channels.max(nch).max(1) as i32,
                    };
                    let audio = p.sink.setup(setup);
                    h.tx.send(AudioCmd::Link(LinkCmd::Sink(slot, Box::new(audio))));
                }
                p.recv_format = Some(format);
                h.publish_peer(slot);
                h.event(Event::PeerChanged { id: p.c.id });
            }
            SinkEventKind::StateChanged { playing } => {
                p.c.recv_active = p.c.recv_allow && playing;
                p.c.connected = p.c.recv_active || p.c.send_active;
                p.c.sync_shared();
            }
            SinkEventKind::BlocksLost { count } => {
                let thresh = h.model.state.extra.auto_drop_rate_thresh;
                if p.c.jitter.on_blocks_lost(count, ms, block_ms, thresh) == Change::Buffer {
                    self.apply_buffer(h, i);
                }
            }
            SinkEventKind::BlocksResent { count } => p.c.jitter.resent += i64::from(count),
            SinkEventKind::Ping { .. } => {
                let change = p.c.jitter.on_ping(ms, block_ms);
                p.c.sync_shared();
                if change == Change::Buffer {
                    self.apply_buffer(h, i);
                }
            }
            SinkEventKind::BlocksReordered { .. } | SinkEventKind::BlocksGap { .. } => {}
        }
    }

    // ---- send thread --------------------------------------------------------------------

    /// Timers, client I/O, outgoing streams and pings, events, statistics (`doSendData` and
    /// `handleEvents`). `blocks` counts processed audio blocks (one fill-ratio sample each).
    pub fn tick(&mut self, h: &mut Host, blocks: u64) {
        let now = Instant::now();
        let t = TimeTag::now();
        let ms = h.now_ms();
        let mut events = Vec::new();
        self.tcp
            .pump(&mut self.client, &self.sock, now, |e| events.push(e));
        for e in events {
            self.on_client_event(h, e, now);
        }
        self.run_timers(h, now);
        self.send_layout_if_due(h, ms);

        // the streams themselves are encoded and sent by the caller, outside `Core`
        let sock = self.sock.clone();
        let new_blocks = blocks.saturating_sub(self.last_blocks).min(256);
        self.last_blocks = blocks;
        for i in 0..self.peers.len() {
            let p = &mut self.peers[i];
            p.c.tick(h, ms);
            p.sink
                .send(t, |a, b| send_to(&sock, a, b, &mut p.c.bytes_sent));
            for s in [&mut p.echo_sink, &mut p.lat_sink] {
                s.send(t, |a, b| send_to(&sock, a, b, &mut p.c.bytes_sent));
                while s.poll_event().is_some() {}
            }
            for s in [&mut p.echo_source, &mut p.lat_source] {
                s.send(|a, b| send_to(&sock, a, b, &mut p.c.bytes_sent));
                while s.poll_event().is_some() {}
            }
            if p.c.lat_test_active
                && let Some((state, ms)) = p.lat.measured()
                && state != p.lat_state_seen
            {
                p.lat_state_seen = state;
                p.c.measured_ms = Some(ms);
                let estimated_ms = p.c.latency(h.cfg.block_ms(), p.recv_opus()).round_trip_ms;
                h.note(Notification::LatencyMeasured {
                    id: p.c.id,
                    round_trip_ms: ms,
                    estimated_ms,
                });
            }
            // the dummy invite (re-sent on every server announcement) has done its job once
            // the real stream reaches us
            if p.inviting_dummy && p.got_format {
                p.sink.uninvite_source(p.addr, 0);
                p.inviting_dummy = false;
            }
            if ms > p.last_ping_ms + PING_INTERVAL_MS {
                p.last_ping_ms = ms;
                let ping = SbMessage::Ping {
                    origin_time: TimeTag::now().0,
                };
                self.send_sb(i, &ping);
                let p = &mut self.peers[i];
                if !p.sent_first_info {
                    p.sent_first_info = true;
                    self.send_peer_info(h, i);
                }
            }
            let p = &mut self.peers[i];
            if let Some(r) = p.sink.buffer_fill_ratio(p.addr, p.remote_source_id) {
                for _ in 0..new_blocks {
                    p.c.jitter.push_fill(r);
                }
            }
        }
        self.dummy.send(|_, _| {});
        self.process_events(h, true);
        self.commit_caches_if_due(h, ms);
    }

    /// On engine shutdown: leave the server cleanly.
    pub fn shutdown(&mut self, h: &mut Host) {
        self.disconnect(h);
        let mut sink = Vec::new();
        self.tcp
            .pump(&mut self.client, &self.sock, Instant::now(), |e| {
                sink.push(e)
            });
    }
}

impl Proto for Session {
    fn common(&self) -> &Common {
        &self.c
    }

    fn common_mut(&mut self) -> &mut Common {
        &mut self.c
    }

    fn len(&self) -> usize {
        self.peers.len()
    }

    fn peer(&self, i: usize) -> &PeerCore {
        &self.peers[i].c
    }

    fn peer_mut(&mut self, i: usize) -> &mut PeerCore {
        &mut self.peers[i].c
    }

    fn send_sb(&mut self, i: usize, m: &SbMessage<'_>) {
        let p = &mut self.peers[i];
        send_sb(&self.sock, &mut self.buf, p.addr, m, &mut p.c.bytes_sent);
    }

    /// `/sb/clayinfo` (`updateRemotePeerUserFormat`, P.cpp:6420-6466); not sent when the
    /// layout exceeds the payload limit, as upstream.
    fn send_layout(&mut self, h: &Host, i: usize) {
        let m = SbMessage::ChannelLayoutInfo {
            source_id: self.peers[i].remote_sink_id,
            layouts: h.model.send_layout(),
        };
        self.send_sb(i, &m);
    }

    fn push_buffer(&mut self, i: usize) {
        let p = &mut self.peers[i];
        let opts = SinkOptions {
            buffersize_ms: p.c.jitter.buffer_ms as i32,
            ..p.sink.options()
        };
        p.sink.set_options(opts);
        for s in [&mut p.echo_sink, &mut p.lat_sink] {
            let o = SinkOptions {
                buffersize_ms: opts.buffersize_ms,
                ..s.options()
            };
            s.set_options(o);
        }
    }

    fn push_send_format(&mut self, h: &mut Host, i: usize) {
        let p = &mut self.peers[i];
        let (sr, block) = (h.cfg.sample_rate, h.cfg.block_size);
        let _ = p
            .src()
            .set_format(aoo_format(p.c.format_index, p.c.send_channels, sr, block));
        // the latency and echo sources follow in mono (`setupSourceFormat(.., true)`)
        let mono = aoo_format(p.c.format_index, 1, sr, block);
        let _ = p.echo_source.set_format(mono);
        let _ = p.lat_source.set_format(mono);
    }

    fn set_send_active(&mut self, i: usize, active: bool) {
        let p = &mut self.peers[i];
        p.c.send_active = active;
        if active {
            p.c.send_allow = true;
            p.c.send_allow_cache = true;
            if !p.src().is_playing() {
                p.src().start();
            }
        } else {
            p.src().stop();
        }
    }

    /// (Un)invite the remote source.
    fn set_recv_active(&mut self, i: usize, active: bool) {
        let p = &mut self.peers[i];
        if active {
            p.c.recv_allow = true;
            p.c.recv_allow_cache = true;
        }
        if p.remote_source_id == ID_NONE {
            return;
        }
        if active {
            p.sink.invite_source(p.addr, p.remote_source_id);
        } else {
            p.sink.uninvite_source(p.addr, p.remote_source_id);
        }
    }

    /// Only a channel count change restarts a stream; the layout follows on the next tick.
    fn update_send_channels(&mut self, h: &mut Host) {
        let nominal = h.model.state.params.send_channels;
        for i in 0..self.peers.len() {
            self.peers[i].c.nominal_send_channels = nominal;
            let n = self.compute_send_channels(h, i, nominal);
            crate::proto::set_model_send_channels(h, self.peers[i].c.slot, n);
            let p = &mut self.peers[i];
            if p.c.send_channels != n {
                p.c.send_channels = n;
                let cfg = h.cfg;
                let audio = p.src().setup(Setup {
                    samplerate: cfg.sample_rate as i32,
                    blocksize: cfg.block_size as i32,
                    nchannels: n as i32,
                });
                h.tx.send(AudioCmd::Link(LinkCmd::Source(p.c.slot, Box::new(audio))));
                self.push_send_format(h, i);
            }
        }
        self.c.layout_due = true;
    }

    fn remove_peer_at(&mut self, h: &mut Host, i: usize, send_block: bool) {
        let mut p = self.peers.remove(i);
        if p.c.connected {
            // `disconnectRemotePeer`; we also flush the uninvites the C++ never got to send
            p.sink.uninvite_all();
            p.lat_sink.uninvite_all();
            p.src().remove_all();
            let (sock, c) = (&self.sock, &mut p.c.bytes_sent);
            let t = TimeTag::now();
            p.sink.send(t, |a, b| send_to(sock, a, b, c));
            p.lat_sink.send(t, |a, b| send_to(sock, a, b, c));
        }
        if send_block {
            let m = SbMessage::BlockedInfo {
                from: &self.c.user,
                blocked: true,
            };
            send_sb(&self.sock, &mut self.buf, p.addr, &m, &mut p.c.bytes_sent);
        }
        h.tx.send(AudioCmd::Link(LinkCmd::Remove(p.c.slot)));
        self.forget_peer(h, &p.c);
    }

    /// By IP address.
    fn block_peer_at(&mut self, h: &mut Host, i: usize) {
        let ip = self.peers[i].addr.ip();
        if !self.blocked.contains(&ip) {
            self.blocked.push(ip);
        }
        self.remove_peer_at(h, i, true);
    }

    /// `setRequestRemotePeerSendAudioCodecFormat`: ask the peer to send us this format.
    fn request_format(&mut self, h: &Host, i: usize, index: Option<usize>) {
        let p = &mut self.peers[i];
        p.c.requested_format = index;
        if let Some(fi) = index.filter(|&f| f < FORMATS.len()) {
            let f = aoo_format(fi, p.recv_channels(), h.cfg.sample_rate, h.cfg.block_size);
            p.sink.request_codec_change(p.addr, p.remote_source_id, f);
        }
    }

    fn set_dynamic_resampling(&mut self, on: bool) {
        for p in &mut self.peers {
            p.sink.set_options(SinkOptions {
                dynamic_resampling: on,
                ..p.sink.options()
            });
            p.src().set_options(SourceOptions {
                dynamic_resampling: on,
                ..p.src().options()
            });
        }
    }

    /// PR #202: every packet `n` times (1..=5), on every peer's main source.
    fn set_redundancy(&mut self, n: i32) {
        for p in &mut self.peers {
            let mut s = p.src();
            let o = s.options();
            s.set_options(SourceOptions { redundancy: n, ..o });
        }
    }

    /// `startRemotePeerLatencyTest` (P.cpp:5762-5794): our latency sink invites their echo
    /// source, our latency source sends to their echo sink. False before the peer connected.
    fn start_latency_test(&mut self, i: usize) -> bool {
        let p = &mut self.peers[i];
        if p.remote_source_id == ID_NONE || p.remote_sink_id == ID_NONE {
            return false;
        }
        if p.c.lat_test_active {
            return true;
        }
        p.lat_sink.uninvite_all();
        // an echo still buffered from the last test would be measured too early (P.cpp:5771)
        p.lat_sink.reset();
        p.lat_source.remove_all();
        p.lat_sink
            .invite_source(p.addr, p.remote_source_id + ECHO_ID_OFFSET);
        p.lat_source
            .add_sink(p.addr, p.remote_sink_id + ECHO_ID_OFFSET, 0);
        p.lat_source.start();
        p.lat_state_seen = 0;
        p.lat.state.store(0, Ordering::Relaxed);
        p.lat.restart.fetch_add(1, Ordering::AcqRel);
        p.lat.active.store(true, Ordering::Release);
        p.c.lat_test_active = true;
        true
    }

    /// `stopRemotePeerLatencyTest`; the last result stays.
    fn stop_latency_test(&mut self, i: usize) {
        let p = &mut self.peers[i];
        if p.c.lat_test_active {
            p.lat_sink.uninvite_all();
            p.lat_source.remove_all();
            p.lat_source.stop();
            p.lat.active.store(false, Ordering::Release);
            p.c.lat_test_active = false;
        }
    }

    fn recv_opus(&self, i: usize) -> bool {
        self.peers[i].recv_opus()
    }

    fn recv_format_name(&self, i: usize) -> Option<String> {
        self.peers[i].recv_format.map(|f| {
            format_index(&f).map_or_else(|| f.codec_name().to_owned(), |i| FORMATS[i].name.to_owned())
        })
    }

    fn addr(&self, i: usize) -> SocketAddr {
        self.peers[i].addr
    }

    fn layout_target(&self, i: usize, source_id: i32) -> Option<usize> {
        self.find_id(self.peers[i].addr, source_id)
    }
}

impl Peer {
    /// Channel count we ask the remote to encode when requesting a codec change (`recvChannels`).
    fn recv_channels(&self) -> usize {
        self.recv_format.map_or(1, |f| f.nchannels.max(1) as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioConfig;
    use sonobus_core::state::AppState;

    fn session() -> (Session, Host) {
        let cfg = AudioConfig {
            sample_rate: 48000,
            block_size: 256,
            input_channels: 1,
            output_channels: 2,
        };
        let h = crate::tests::host(cfg, AppState::new(false));
        let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").unwrap());
        (Session::new(sock, &h), h)
    }

    fn lose_server(s: &mut Session, h: &mut Host, now: Instant) {
        s.client.disconnect(); // the real client is back to `Disconnected`
        let error = Some("server gone".to_owned());
        s.on_client_event(h, ClientEvent::Disconnected { error }, now);
    }

    fn reconnecting(h: &mut Host) -> usize {
        let ev = crate::tests::events(h);
        ev.iter().filter(|e| **e == Event::Reconnecting).count()
    }

    /// Loss, reconnect, login, loss again before the group is joined: the reconnect must
    /// carry on (with backoff). Then the server is back but refuses the group: stop.
    #[test]
    fn reconnect_survives_a_second_loss_and_stops_on_a_refused_group() {
        let (mut s, mut h) = session();
        let srv: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let group = Some(("g".to_owned(), "pw".to_owned(), false));
        s.connect(&mut h, srv, "127.0.0.1", 9, "me", "", group);
        let t = Instant::now();
        s.on_client_event(&mut h, ClientEvent::Connected, t);
        let joined = ClientEvent::GroupJoined { group: "g".into() };
        s.on_client_event(&mut h, joined, t);
        assert_eq!(s.c.group.as_deref(), Some("g"));

        lose_server(&mut s, &mut h, t);
        assert_eq!(
            s.reconnect.as_ref().and_then(|r| r.at),
            Some(t + RECONNECT_MIN)
        );
        crate::tests::events(&mut h);
        s.run_timers(&mut h, t + Duration::from_secs(3));
        assert_eq!(reconnecting(&mut h), 1);
        s.on_client_event(&mut h, ClientEvent::Connected, t + Duration::from_secs(3));
        // lost again before `GroupJoined`
        let t2 = t + Duration::from_secs(4);
        lose_server(&mut s, &mut h, t2);
        let r = s.reconnect.as_ref().expect("still recovering");
        assert_eq!(
            r.at,
            Some(t2 + 2 * RECONNECT_MIN),
            "rescheduled with backoff"
        );
        s.run_timers(&mut h, t2 + Duration::from_secs(5));
        assert_eq!(reconnecting(&mut h), 1);
        assert_eq!(s.pending_join.as_ref().map(|g| g.0.as_str()), Some("g"));

        // back, but the group now has another password
        let t3 = t2 + Duration::from_secs(6);
        s.on_client_event(&mut h, ClientEvent::Connected, t3);
        let refused = ClientEvent::GroupJoinFailed {
            group: "g".into(),
            error: "wrong password".into(),
        };
        s.on_client_event(&mut h, refused, t3);
        assert!(s.reconnect.is_none());
        let ev = crate::tests::events(&mut h);
        assert!(ev.contains(&Event::GroupJoinFailed {
            group: "g".into(),
            error: "wrong password".into()
        }));
        // a later loss of this (group-less) connection does not start it again
        lose_server(&mut s, &mut h, t3);
        s.run_timers(&mut h, t3 + RECONNECT_MAX * 2);
        assert_eq!(reconnecting(&mut h), 0);
    }

    #[test]
    fn oversized_chat_is_not_sent_or_recorded() {
        let (mut s, _h) = session();
        let m = s.chat_message(&[], &"x".repeat(5000));
        assert!(s.send_chat(&m).unwrap_err().contains("too long"));
        assert!(s.c.chat.is_empty());
        let m = s.chat_message(&["bob", "amy"], "hi");
        assert_eq!(m.targets, "bob|amy");
        s.send_chat(&m).unwrap();
        assert_eq!(s.c.chat.len(), 1);
    }

    #[test]
    fn user_name_increments() {
        assert_eq!(next_user_name("bob"), "bob 2");
        assert_eq!(next_user_name("bob 2"), "bob 3");
        assert_eq!(next_user_name("Jo Smith"), "Jo Smith 2");
        assert_eq!(next_user_name("room 9x"), "room 10");
        assert_eq!(next_user_name("a 0"), "a 0 2");
    }

    #[test]
    fn format_table_round_trip() {
        for (i, fmt) in FORMATS.iter().enumerate() {
            for nch in [1, 2, 5] {
                let f = aoo_format(i, nch, 48000, 256);
                assert_eq!(format_index(&f), Some(i), "format {i} x{nch}");
                assert_eq!(f.blocksize, 256.max(fmt.min_blocksize));
            }
        }
        let f = aoo_format(crate::formats::DEFAULT_FORMAT, 2, 48000, 64);
        assert_eq!(f.blocksize, 120);
        assert!(
            matches!(f.codec, Codec::Opus(o) if o.bitrate == 192_000 && o.complexity == 10 && o.signal_type == OPUS_SIGNAL_MUSIC)
        );
        let f64 = Format {
            codec: Codec::Pcm(PcmBitDepth::Float64),
            ..f
        };
        assert_eq!(format_index(&f64), None);
    }
}
