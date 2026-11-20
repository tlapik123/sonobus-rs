//! SonoBus session and audio engine (stage 1): join a group or connect directly, stream audio
//! to and from every peer, and mix it with the local inputs as SonoBus 1.7.2 does.
//!
//! [`Engine::new`] returns the two halves:
//!
//! - [`Engine`] — the control half for the UI thread: commands (connect, groups, direct
//!   connect, per-peer controls, chat, every parameter via [`Engine::update_state`]) and a
//!   polled [`Engine::snapshot`] / [`Engine::poll_event`] API.
//! - [`EngineAudio`] — the real-time half: [`EngineAudio::process`] takes any number of
//!   frames and runs them in blocks of the configured size. It never allocates, frees, locks
//!   or logs (see `tests/rt_alloc.rs`).
//!
//! Threads: a receive thread (blocking `recv_from` with a 20 ms timeout) and a send/timer
//! thread share one UDP socket, as upstream. The send thread runs the protocol (client I/O,
//! timers, pings, events) under `Mutex<Core>`, releases it, then encodes and sends each
//! peer's stream under that peer's own source lock. So the receive thread (packets, jitter
//! buffers) never waits for an encode, and [`Engine::snapshot`] holds `Core` only to copy
//! status. The audio thread wakes the send thread once per callback that queued audio
//! (`Thread::unpark`: an atomic swap, plus a `futex` wake only when the thread sleeps; no
//! lock, no allocation); otherwise it wakes every 20 ms for its timers. Upstream likewise
//! signals its send thread from `processBlock`.
//!
//! Audio/network sharing: the audio thread owns all its state. Peers, parameter snapshots
//! and replaced stream halves reach it through one SPSC command ring; the control side keeps
//! only the latest pending command per kind and slot (`AudioTx`), so the queue is bounded by
//! construction, and hands over the next batch only once the audio thread took the last one.
//! The audio thread applies at most [`CMDS_PER_BLOCK`] per callback. Everything it replaces goes
//! back through a garbage ring and is dropped on the send thread. Per-peer flags the network
//! flips (receive active, safety mute) and the meters are atomics. Upstream uses a read-write
//! lock the audio thread takes per block; this design has no lock at all on the audio side.
//!
//! Protocol seam: a session is either legacy (wire-compatible with SonoBus 1.7: AOO + `/sb` over
//! one UDP socket, the private `legacy` module) or, with the `modern` cargo feature (default
//! on), protocol v2 over iroh/QUIC ([`modern`]); which one is decided when connecting, and
//! starting one ends the other. Both keep their peers in the shape of the private `proto`
//! module: a protocol-neutral `PeerCore` per peer (mixer slot, jitter controller, latency
//! estimate, stats, peer cache) and the `Proto` trait, whose provided methods hold everything
//! the two share (the `/sb/...` handlers, chat, jitter buffer control, latency match, receive
//! sync, recording and cache bookkeeping, status). The [`mixer`], the parameter model,
//! recording, playback and the public API do not depend on either protocol; this file's glue
//! (`Core`, `AudioCmd`, `EngineAudio`) is the only place they meet. Without the `modern`
//! feature the engine builds without tokio or iroh.

pub mod formats;
pub mod jitter;
pub mod latency;
mod legacy;
pub mod mixer;
mod model;
#[cfg(feature = "modern")]
pub mod modern;
pub mod options;
pub mod playback;
mod proto;
pub mod recording;
mod wav;

mod fpmode;

use std::collections::VecDeque;
use std::io;
use std::net::{IpAddr, SocketAddr, ToSocketAddrs, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{self, JoinHandle, Thread};
use std::time::{Duration, Instant};

use aoo::TimeTag;
use aoo::source::Source;
use rtrb::{Consumer, Producer, RingBuffer};

pub use jitter::{JitterMode, LatencyInfo};
pub use mixer::{AudioConfig, HostTransport, MeterLevels};
pub use playback::{FileStatus, SampleKey, VoiceStatus};
pub use recording::RecordingStatus;
pub use sonobus_core::files::Soundboards;
pub use sonobus_core::sbmsg::{LatEntry, SuggestGroup};
pub use sonobus_core::state::{AppState, ChannelGroup};
pub use sonobus_files::LocalTime;

use legacy::{LinkCmd, Links, Session};
use proto::Proto;
use mixer::{DelayTarget, Garbage, MAX_CHANNELS, MAX_GROUPS, MAX_PEERS, MAX_VOICES, Meters, Mixer};
use model::Model;
use sonobus_core::valuetree::Var;

/// Commands the audio thread applies per `process` call at most.
pub const CMDS_PER_BLOCK: usize = 32;
/// Capacity of the command ring: at least the largest batch `AudioTx` can hold (globals, one
/// delay line per group and the metronome, the file deck, the soundboard voices, the recorder,
/// six commands per peer slot; modern sessions: three more per slot and the shared send half).
const CMD_RING: usize = 512;
const _: () = assert!(CMD_RING >= MAX_GROUPS + 3 + MAX_VOICES + 2 + 9 * MAX_PEERS);
/// UDP socket buffers we ask for (upstream: 1 MiB both ways; at least 512 KB send).
const SOCKET_SEND_BUFFER: usize = 1 << 20;
const SOCKET_RECV_BUFFER: usize = 1 << 20;

pub struct EngineConfig {
    pub audio: AudioConfig,
    /// Local UDP port; 0 = any. A busy port is retried on the next 99 (upstream).
    pub udp_port: u16,
    /// Settings to start from (parameters, input groups, peer cache, recents, ...).
    pub state: AppState,
}

/// Stable identity of a connected peer for the lifetime of its connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct PeerId(pub u64);

#[derive(Debug, Clone, PartialEq)]
pub struct ChatMessage {
    /// Sent by us.
    pub own: bool,
    pub group: String,
    pub from: String,
    /// `|`-separated user names; empty = everyone.
    pub targets: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// Logged in to the server as `user` (which may differ from the requested name).
    Connected {
        user: String,
    },
    ConnectFailed {
        reason: String,
    },
    /// The server said "access denied" for this name: retrying it, or trying this new one.
    UserNameInUse {
        retrying: bool,
        user: String,
    },
    /// `error` is `None` after a deliberate disconnect.
    Disconnected {
        error: Option<String>,
    },
    /// Retrying the server after losing it (peers keep streaming meanwhile).
    Reconnecting,
    GroupJoined {
        group: String,
    },
    GroupJoinFailed {
        group: String,
        error: String,
    },
    GroupLeft {
        group: String,
    },
    PublicGroupsChanged,
    PeerJoined {
        id: PeerId,
        user: String,
        group: String,
    },
    /// Hole punching failed (firewall/NAT).
    PeerJoinFailed {
        user: String,
        group: String,
    },
    /// A peer from our block list joined the group; it was told it is blocked.
    PeerJoinBlocked {
        user: String,
        group: String,
        addr: SocketAddr,
    },
    PeerLeft {
        id: PeerId,
        user: String,
    },
    /// Format, layout or connection state of a peer changed.
    PeerChanged {
        id: PeerId,
    },
    BlockedByPeer {
        id: PeerId,
        user: String,
        blocked: bool,
    },
    Chat(ChatMessage),
    Error(String),
}

/// Stage-2 events (recording, playback, latency test and match, group suggestions), polled
/// with [`Engine::poll_notification`]. Kept apart from [`Event`] so that existing exhaustive
/// matches on it stay valid.
#[derive(Debug, Clone, PartialEq)]
pub enum Notification {
    RecordingStarted {
        path: PathBuf,
    },
    /// The recording ended: by [`Engine::stop_recording`] or because a file could not be
    /// written (`error`, e.g. disk full).
    RecordingStopped {
        files: Vec<PathBuf>,
        error: Option<String>,
    },
    /// A file of the running recording could not be created (a peer joining), or could not
    /// be written (disk full, device gone): then the whole recording stops, and
    /// [`Notification::RecordingStopped`] with the error follows.
    RecordingError {
        error: String,
    },
    /// The disk could not keep up: this many frames (all files together) were lost so far.
    RecordingDroppedFrames {
        frames: u64,
    },
    /// The file transport reached the end and went back to the start.
    FileFinished,
    SampleFinished {
        board: usize,
        sample: usize,
    },
    PlaybackError {
        error: String,
    },
    /// A latency test measured (again): round trip through the peer's echo service, and our
    /// estimate from ping and buffers at that moment.
    LatencyMeasured {
        id: PeerId,
        round_trip_ms: f32,
        estimated_ms: f32,
    },
    /// Every peer answered `/sb/reqlatinfo` ([`Engine::latency_match_info`] is complete).
    LatencyMatchReady,
    /// `/sb/suggestlat`: a peer asks everyone to match this latency; accept with
    /// [`Engine::commit_latency_match`]. `from`: the sending peer's user name (as the server
    /// reported it; the message body's claimed name is ignored).
    LatencyMatchRequested {
        from: String,
        latency_ms: f32,
    },
    /// `/sb/suggestgroup`: a peer invites us to another group; accept with
    /// [`Engine::join_group`] (after leaving the current one, as upstream's "Connect To Group").
    /// Its `user` is the sending peer's user name, not the body's.
    GroupSuggested(SuggestGroup),
}

impl Notification {
    /// What remote peers can trigger at will (latency and group suggestions): the first to
    /// go when the queue is full.
    pub fn is_low_priority(&self) -> bool {
        matches!(
            self,
            Notification::LatencyMatchRequested { .. } | Notification::GroupSuggested(_)
        )
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Disconnected,
    Connecting,
    Connected,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PeerStatus {
    pub id: PeerId,
    pub user: String,
    pub group: String,
    pub addr: SocketAddr,
    pub connected: bool,
    pub send_active: bool,
    pub recv_active: bool,
    pub send_allow: bool,
    pub recv_allow: bool,
    pub soloed: bool,
    pub level: f32,
    pub safety_muted: bool,
    pub blocked_us: bool,
    pub remote_recording: bool,
    pub send_channels: usize,
    pub recv_channels: usize,
    /// Index into [`formats::FORMATS`].
    pub send_format: usize,
    /// What the peer sends us, by table name when it is one of the table's formats.
    pub recv_format: Option<String>,
    pub requested_format: Option<usize>,
    /// PR #201: the [`formats::FORMATS`] indices the peer can decode (from its `/sb/pinfo`
    /// `supportedcodecs`; all of them for a peer that sends none, like stock 1.7.2).
    /// [`Engine::set_peer_send_format`] only picks among these.
    pub supported_formats: Vec<usize>,
    pub jitter_mode: JitterMode,
    pub buffer_ms: f32,
    pub auto_init_done: bool,
    pub fill_ratio: f32,
    pub packets_received: i64,
    pub packets_sent: i64,
    pub packets_dropped: i64,
    pub packets_resent: i64,
    pub send_bps: f64,
    pub recv_bps: f64,
    pub latency: LatencyInfo,
    /// A latency test is running ([`Engine::start_latency_test`]).
    pub latency_test_active: bool,
    /// The last latency test result: measured round trip in ms.
    pub measured_round_trip_ms: Option<f32>,
    /// [`Engine::set_peer_send_channels_override`].
    pub send_channels_override: Option<usize>,
    pub meter: MeterLevels,
    /// The channel groups the peer's audio is mixed with.
    pub groups: Vec<ChannelGroup>,
    /// Protocol v2 (modern session) details; `None` for legacy peers.
    pub net: Option<PeerNet>,
}

/// What a protocol v2 peer's connection looks like.
#[derive(Debug, Clone, PartialEq)]
pub struct PeerNet {
    /// The peer's endpoint id, proven by TLS: the one identity that can be trusted (names and
    /// roles are self-declared).
    pub endpoint_id: [u8; 32],
    /// The peer is the host named by the invite link we joined with (verified, not claimed).
    pub is_invite_host: bool,
    /// Self-declared role (host, peer, listener).
    pub role: PeerRole,
    /// The current path is direct UDP (else relayed).
    pub direct: bool,
    /// The relay forwarding the path, when relayed.
    pub relay: Option<String>,
    /// The peer's address on a direct path.
    pub remote: Option<SocketAddr>,
    pub rtt_ms: f32,
    /// What it gets from us: level (on the shared stream the encoding, 0 main / 1 low; on a
    /// private Opus stream the bit-rate ladder rung, 0 best) and redundant blocks per
    /// datagram, as the adaptation decided from its reports; `None` while it does not
    /// subscribe.
    pub send_level: Option<(usize, u8)>,
    /// Broadcast listener with playout sync on: how early (positive) or late its output is
    /// against the host's target, ms; `None` while sync is off or not yet measured.
    pub playout_error_ms: Option<f32>,
}

/// A protocol v2 peer's self-declared role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PeerRole {
    Host,
    Peer,
    Listener,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub connection: Connection,
    pub user: String,
    pub group: Option<String>,
    pub public_groups: Vec<(String, i32)>,
    pub peers: Vec<PeerStatus>,
    pub input: MeterLevels,
    pub send: MeterLevels,
    pub output: MeterLevels,
    pub udp_port: u16,
}

/// Group to join right after logging in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupJoin {
    pub name: String,
    pub password: String,
    pub public: bool,
}

pub(crate) enum AudioCmd {
    Mix(mixer::Cmd),
    Link(LinkCmd),
    #[cfg(feature = "modern")]
    Modern(modern::LinkCmd),
}

/// The latest pending commands of one peer slot.
#[derive(Default)]
struct SlotPending {
    /// `AddPeer` / `RemovePeer`
    mix: Option<mixer::Cmd>,
    /// `Peer` (parameters)
    params: Option<mixer::Cmd>,
    /// `Add` / `Remove`
    link: Option<LinkCmd>,
    sink: Option<LinkCmd>,
    source: Option<LinkCmd>,
    /// `UserRec`
    user_rec: Option<mixer::Cmd>,
    /// Modern stream halves: `Add` / `Remove`, `Rx`, `Tx`.
    #[cfg(feature = "modern")]
    mlink: Option<modern::LinkCmd>,
    #[cfg(feature = "modern")]
    mrx: Option<modern::LinkCmd>,
    #[cfg(feature = "modern")]
    mtx: Option<modern::LinkCmd>,
}

/// Producer side of the audio command ring. Commands wait here, coalesced to the latest one
/// per kind and slot (a new peer in a slot also drops the slot's older parameter and stream
/// commands), until the audio thread has taken the previous batch: bounded however long the
/// audio callback does not run, and a flood of snapshots costs the audio thread one each.
pub(crate) struct AudioTx {
    tx: Producer<AudioCmd>,
    garbage: Consumer<Garbage>,
    globals: Option<mixer::Cmd>,
    /// Per input group, then the metronome, file playback and soundboard.
    delays: Vec<Option<mixer::Cmd>>,
    file: Option<mixer::Cmd>,
    voices: Vec<Option<mixer::Cmd>>,
    rec: Option<mixer::Cmd>,
    slots: Vec<SlotPending>,
    /// The modern shared send stream's half.
    #[cfg(feature = "modern")]
    mmain: Option<modern::LinkCmd>,
}

impl AudioTx {
    fn new(tx: Producer<AudioCmd>, garbage: Consumer<Garbage>) -> Self {
        Self {
            tx,
            garbage,
            globals: None,
            delays: (0..MAX_GROUPS + 3).map(|_| None).collect(),
            file: None,
            voices: (0..MAX_VOICES).map(|_| None).collect(),
            rec: None,
            slots: (0..MAX_PEERS).map(|_| SlotPending::default()).collect(),
            #[cfg(feature = "modern")]
            mmain: None,
        }
    }

    pub fn send(&mut self, c: AudioCmd) {
        use mixer::Cmd as M;
        match c {
            AudioCmd::Mix(c) => match c {
                M::Globals(_) => self.globals = Some(c),
                M::Delay(t, ..) => {
                    let i = match t {
                        DelayTarget::Input(i) => i,
                        DelayTarget::Metronome => MAX_GROUPS,
                        DelayTarget::File => MAX_GROUPS + 1,
                        DelayTarget::Soundboard => MAX_GROUPS + 2,
                    };
                    if let Some(d) = self.delays.get_mut(i) {
                        *d = Some(c);
                    }
                }
                M::AddPeer(s, _) | M::RemovePeer(s) => {
                    if let Some(p) = self.slots.get_mut(s) {
                        p.params = None;
                        p.mix = Some(c);
                    }
                }
                M::Peer(s, _) => {
                    if let Some(p) = self.slots.get_mut(s) {
                        p.params = Some(c);
                    }
                }
                M::File(_) => self.file = Some(c),
                M::Voice(i, _) => {
                    if let Some(v) = self.voices.get_mut(i) {
                        *v = Some(c);
                    }
                }
                M::Rec(_) => self.rec = Some(c),
                M::UserRec(s, _) => {
                    if let Some(p) = self.slots.get_mut(s) {
                        p.user_rec = Some(c);
                    }
                }
            },
            AudioCmd::Link(c) => match c {
                LinkCmd::Add(s, _) | LinkCmd::Remove(s) => {
                    if let Some(p) = self.slots.get_mut(s) {
                        p.sink = None;
                        p.source = None;
                        p.link = Some(c);
                    }
                }
                LinkCmd::Sink(s, _) => {
                    if let Some(p) = self.slots.get_mut(s) {
                        p.sink = Some(c);
                    }
                }
                LinkCmd::Source(s, _) => {
                    if let Some(p) = self.slots.get_mut(s) {
                        p.source = Some(c);
                    }
                }
            },
            #[cfg(feature = "modern")]
            AudioCmd::Modern(c) => {
                use modern::LinkCmd as L;
                match c {
                    L::Add(s, _) | L::Remove(s) => {
                        if let Some(p) = self.slots.get_mut(s) {
                            p.mrx = None;
                            p.mtx = None;
                            p.mlink = Some(c);
                        }
                    }
                    L::Rx(s, _) => {
                        if let Some(p) = self.slots.get_mut(s) {
                            p.mrx = Some(c);
                        }
                    }
                    L::Tx(s, _) => {
                        if let Some(p) = self.slots.get_mut(s) {
                            p.mtx = Some(c);
                        }
                    }
                    L::Main(_) => self.mmain = Some(c),
                }
            }
        }
    }

    pub fn mix(&mut self, c: mixer::Cmd) {
        self.send(AudioCmd::Mix(c));
    }

    /// Hand the pending commands over once the ring is empty, in dependency order: delay
    /// lines, globals, then per slot membership, parameters, stream halves.
    fn flush(&mut self) {
        while self.garbage.pop().is_ok() {}
        if self.tx.slots() < self.tx.buffer().capacity() {
            return;
        }
        let tx = &mut self.tx;
        // cannot fail: an empty ring holds the largest batch (`CMD_RING`)
        let mut push = |c: AudioCmd| {
            let _ = tx.push(c);
        };
        for d in self.delays.iter_mut().filter_map(Option::take) {
            push(AudioCmd::Mix(d));
        }
        if let Some(g) = self.globals.take() {
            push(AudioCmd::Mix(g));
        }
        let media = [self.file.take(), self.rec.take()];
        for c in media
            .into_iter()
            .chain(self.voices.iter_mut().map(Option::take))
            .flatten()
        {
            push(AudioCmd::Mix(c));
        }
        for p in &mut self.slots {
            let mix = [p.mix.take(), p.params.take(), p.user_rec.take()];
            let link = [p.link.take(), p.sink.take(), p.source.take()];
            mix.into_iter()
                .flatten()
                .for_each(|c| push(AudioCmd::Mix(c)));
            link.into_iter()
                .flatten()
                .for_each(|c| push(AudioCmd::Link(c)));
            #[cfg(feature = "modern")]
            [p.mlink.take(), p.mrx.take(), p.mtx.take()]
                .into_iter()
                .flatten()
                .for_each(|c| push(AudioCmd::Modern(c)));
        }
        #[cfg(feature = "modern")]
        if let Some(c) = self.mmain.take() {
            push(AudioCmd::Modern(c));
        }
    }
}

/// What the protocol side needs from the engine: the model, the audio ring, the event queue.
pub(crate) struct Host {
    pub cfg: AudioConfig,
    pub model: Model,
    pub tx: AudioTx,
    events: VecDeque<Event>,
    notes: VecDeque<Notification>,
    epoch: Instant,
    pub rec: recording::Recorder,
    pub file: Option<playback::FileTransport>,
    pub sb: playback::SoundboardPlayer,
    /// The last [`PeerId`] handed out (ids are unique across sessions of both protocols).
    last_peer_id: u64,
    #[cfg(feature = "modern")]
    sevents: VecDeque<modern::SessionEvent>,
}

const MAX_EVENTS: usize = 1024;

impl Host {
    pub fn event(&mut self, e: Event) {
        if self.events.len() >= MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(e);
    }

    /// Queue a notification. When the queue is full the oldest peer-triggered one
    /// ([`Notification::is_low_priority`]) makes room; recording, playback and error
    /// notifications are only evicted by each other, and a new low-priority one is dropped
    /// when nothing of its kind can go.
    pub fn note(&mut self, n: Notification) {
        if self.notes.len() >= MAX_EVENTS {
            match self.notes.iter().position(Notification::is_low_priority) {
                Some(i) => {
                    self.notes.remove(i);
                }
                None if n.is_low_priority() => return,
                None => {
                    self.notes.pop_front();
                }
            }
        }
        self.notes.push_back(n);
    }

    #[cfg(feature = "modern")]
    pub fn session_event(&mut self, e: modern::SessionEvent) {
        if self.sevents.len() >= MAX_EVENTS {
            self.sevents.pop_front();
        }
        self.sevents.push_back(e);
    }

    pub fn new_peer_id(&mut self) -> PeerId {
        self.last_peer_id += 1;
        PeerId(self.last_peer_id)
    }

    /// Monotonic milliseconds since the engine started.
    pub fn now_ms(&self) -> f64 {
        self.epoch.elapsed().as_secs_f64() * 1000.0
    }

    /// The recorder and what it works with; notifications it raises are queued.
    pub fn with_rec<R>(
        &mut self,
        f: impl FnOnce(&mut recording::Recorder, &mut recording::RecCtx<'_>) -> R,
    ) -> R {
        let mut notes = Vec::new();
        let r = {
            let Host {
                cfg,
                model,
                tx,
                rec,
                ..
            } = self;
            let mut c = recording::RecCtx {
                cfg: *cfg,
                state: &model.state,
                tx,
                notes: &mut notes,
            };
            f(rec, &mut c)
        };
        for n in notes {
            self.note(n);
        }
        r
    }

    pub fn publish(&mut self) {
        let tx = &mut self.tx;
        self.model.publish(&mut |c| tx.mix(c));
    }

    pub fn publish_peer(&mut self, slot: usize) {
        let tx = &mut self.tx;
        self.model.publish_peer(slot, &mut |c| tx.mix(c));
    }
}

struct Core {
    host: Host,
    /// The legacy protocol (its socket and threads live as long as the engine).
    legacy: Session,
    /// The modern session, while one is active (then the legacy one has no peers).
    #[cfg(feature = "modern")]
    modern: Option<modern::Session>,
}

impl Core {
    /// The host and the session in use.
    fn active(&mut self) -> (&mut Host, &mut dyn Proto) {
        #[cfg(feature = "modern")]
        if let Some(m) = &mut self.modern {
            return (&mut self.host, m);
        }
        (&mut self.host, &mut self.legacy)
    }

    /// The modern session's connection state, while one is active.
    fn modern_connection(&self) -> Option<Connection> {
        #[cfg(feature = "modern")]
        if let Some(m) = &self.modern {
            return Some(m.connection());
        }
        None
    }
}

struct Shared {
    core: Mutex<Core>,
    quit: AtomicBool,
    blocks: Arc<AtomicU64>,
    meters: Arc<Meters>,
}

pub(crate) fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Ask for large socket buffers (std cannot) and return what the OS granted (Linux reports
/// twice the usable size and caps the request at `net.core.{w,r}mem_max`).
fn set_socket_buffers(sock: &UdpSocket) -> io::Result<(usize, usize)> {
    let s = socket2::SockRef::from(sock);
    // a refused size is not fatal: the OS default applies
    let _ = s.set_send_buffer_size(SOCKET_SEND_BUFFER);
    let _ = s.set_recv_buffer_size(SOCKET_RECV_BUFFER);
    Ok((s.send_buffer_size()?, s.recv_buffer_size()?))
}

/// Binding stays std's (IPv4 any-address, no address reuse); `socket2` only sizes the buffers.
fn bind(port: u16) -> io::Result<UdpSocket> {
    if port == 0 {
        return UdpSocket::bind(("0.0.0.0", 0));
    }
    let mut last = None;
    for p in port..=port.saturating_add(99) {
        match UdpSocket::bind(("0.0.0.0", p)) {
            Ok(s) => return Ok(s),
            Err(e) => last = Some(e),
        }
    }
    Err(last.expect("at least one port tried"))
}

/// Control half. Dropping it stops the network threads (after leaving the server).
pub struct Engine {
    shared: Arc<Shared>,
    sender: Thread,
    threads: Vec<JoinHandle<()>>,
    udp_port: u16,
    cfg: AudioConfig,
    socket_buffers: (usize, usize),
    final_notes: NotificationHandle,
    /// Device identity for modern sessions (endpoint secret key).
    #[cfg(feature = "modern")]
    identity: Mutex<[u8; 32]>,
    /// LAN discovery while browsing.
    #[cfg(feature = "modern")]
    lan: Mutex<Option<(sonobus_net::lan::Lan, sonobus_net::lan::LanBrowser)>>,
}

/// Real-time half: call [`Self::process`] from the audio callback.
pub struct EngineAudio {
    mixer: Mixer,
    links: Links,
    #[cfg(feature = "modern")]
    mlinks: modern::Links,
    rx: Consumer<AudioCmd>,
    garbage: Producer<Garbage>,
    blocks: Arc<AtomicU64>,
    cfg: AudioConfig,
    sender: Thread,
    /// One block of (sanitised) device input and of output; `pos` frames of the current block
    /// are filled/played once `buffered`.
    inb: Vec<Vec<f32>>,
    outb: Vec<Vec<f32>>,
    pos: usize,
    /// A callback size that is not a multiple of the block size was seen: from then on the
    /// audio runs one block late through `inb`/`outb`.
    buffered: bool,
    /// Per-peer outputs (plugin aux buses): `peer_out_ch` channels per peer slot, slot-major.
    peer_outb: Vec<Vec<f32>>,
    peer_out_ch: usize,
}

impl EngineAudio {
    /// Process one callback: `inputs` and `outputs` hold one slice per device channel, of any
    /// (equal) length. The engine runs in blocks of the configured size: while every callback
    /// is a multiple of it there is no added latency; after the first one that is not, all
    /// audio goes through a one-block FIFO (one block of latency, every block still whole, so
    /// the streams, ramps, meters and effects see the same blocks either way). Device channels
    /// past the configured counts are ignored (inputs) or silent (outputs); missing input
    /// channels are silence. Input samples are sanitised (non-finite -> 0, clamped to
    /// [`mixer::SAMPLE_LIMIT`]).
    ///
    /// Real-time safe: no allocation, lock or log; denormals are flushed to zero while it runs.
    /// The system calls are the clock read (`clock_gettime`, a vDSO call on Linux) that stamps
    /// the AOO streams and at most one send-thread wake-up per callback.
    pub fn process(&mut self, inputs: &[&[f32]], outputs: &mut [&mut [f32]]) {
        self.process_with_peer_outputs(inputs, outputs, &mut []);
    }

    /// Sets up per-peer outputs (upstream's plugin "Aux N Out" buses): `channels` per peer slot
    /// for the first `peers` slots. Allocates: call off the audio thread, before processing.
    pub fn set_peer_outputs(&mut self, peers: usize, channels: usize) {
        let peers = peers.min(MAX_PEERS);
        self.peer_out_ch = channels.min(MAX_CHANNELS);
        self.peer_outb = vec![vec![0.0; self.cfg.block_size]; peers * self.peer_out_ch];
    }

    /// The host transport for the following blocks (`None`: no play head). Real-time safe.
    pub fn set_host_transport(&mut self, t: Option<HostTransport>) {
        self.mixer.host = t;
    }

    /// [`Self::process`], also writing each peer's received audio (before its level and
    /// effects, as upstream's per-user buses) to `peer_outputs`: peer slot `k` gets the
    /// channels `k * channels ..` of [`Self::set_peer_outputs`]. Extra or missing slices are
    /// ignored; a slot without a (receiving) peer is silent.
    pub fn process_with_peer_outputs(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        peer_outputs: &mut [&mut [f32]],
    ) {
        let _fp = fpmode::NoDenormals::new();
        #[cfg(test)]
        tests::probe_fp_mode();
        let mut now = TimeTag::now();
        // the modern streams' clock (another vDSO clock read)
        let mut inow = Instant::now();
        let block = self.cfg.block_size;
        let istep = Duration::from_secs_f64(block as f64 / f64::from(self.cfg.sample_rate));
        // NTP fraction units per block, for the stamps of further blocks in this callback
        let step = (block as f64 / f64::from(self.cfg.sample_rate) * 4_294_967_296.0) as u64;
        let mut applied = 0;
        while applied < CMDS_PER_BLOCK && self.garbage.slots() > 0 {
            let Ok(cmd) = self.rx.pop() else { break };
            let garbage = &mut self.garbage;
            let mut put = |g: Garbage| {
                let _ = garbage.push(g);
            };
            match cmd {
                AudioCmd::Mix(c) => self.mixer.apply(c, &mut put),
                AudioCmd::Link(c) => self.links.apply(c, &mut put),
                #[cfg(feature = "modern")]
                AudioCmd::Modern(c) => self.mlinks.apply(c, &mut put),
            }
            applied += 1;
        }
        let n = outputs
            .iter()
            .map(|o| o.len())
            .min()
            .or_else(|| inputs.iter().map(|i| i.len()).min())
            .unwrap_or(0);
        let mut queued = false;
        let mut done = 0;
        while done < n {
            if !self.buffered {
                if n - done >= block {
                    self.load(inputs, done, 0, block);
                    queued |= self.run(now, inow);
                    now = TimeTag(now.0.wrapping_add(step));
                    inow += istep;
                    store(&self.outb, outputs, done, 0, block);
                    store(&self.peer_outb, peer_outputs, done, 0, block);
                    done += block;
                    continue;
                }
                self.buffered = true;
                self.pos = 0;
                for c in &mut self.outb {
                    c.fill(0.0);
                }
            }
            let k = (n - done).min(block - self.pos);
            self.load(inputs, done, self.pos, k);
            store(&self.outb, outputs, done, self.pos, k);
            store(&self.peer_outb, peer_outputs, done, self.pos, k);
            self.pos += k;
            done += k;
            if self.pos == block {
                self.pos = 0;
                queued |= self.run(now, inow);
                now = TimeTag(now.0.wrapping_add(step));
                inow += istep;
            }
        }
        for o in outputs.iter_mut() {
            o[n..].fill(0.0);
        }
        for o in peer_outputs.iter_mut() {
            if let Some(rest) = o.get_mut(n..) {
                rest.fill(0.0);
            }
        }
        if queued {
            self.sender.unpark();
        }
    }

    /// `k` frames of the device input at `from` into the block buffer at `at`.
    fn load(&mut self, inputs: &[&[f32]], from: usize, at: usize, k: usize) {
        for (c, dst) in self.inb.iter_mut().enumerate() {
            let dst = &mut dst[at..at + k];
            match inputs.get(c).and_then(|x| x.get(from..from + k)) {
                Some(src) => {
                    dst.copy_from_slice(src);
                    mixer::sanitize(dst);
                }
                None => dst.fill(0.0),
            }
        }
    }


    /// One whole block from `inb` to `outb`. Returns whether any stream queued audio.
    fn run(&mut self, now: TimeTag, inow: Instant) -> bool {
        #[cfg(not(feature = "modern"))]
        let _ = inow;
        let n = self.cfg.block_size;
        let sample_rate = self.cfg.sample_rate as i32;
        let Self {
            mixer,
            links,
            #[cfg(feature = "modern")]
            mlinks,
            inb,
            outb,
            blocks,
            peer_outb,
            peer_out_ch,
            ..
        } = self;
        for c in peer_outb.iter_mut() {
            c[..n].fill(0.0);
        }
        let ins: [&[f32]; MAX_CHANNELS] =
            std::array::from_fn(|c| inb.get(c).map_or(&[][..], |v| &v[..]));
        mixer.process_input(&ins[..inb.len()], n);
        for slot in 0..MAX_PEERS {
            if let Some(buf) = mixer.peer_buffer(slot, n) {
                links.receive(slot, buf, n, now);
                #[cfg(feature = "modern")]
                mlinks.receive(slot, buf, n, inow);
                let outs = peer_outb.chunks_mut((*peer_out_ch).max(1)).nth(slot);
                if let (Some(outs), Some((work, nch))) = (outs, mixer.peer_received(slot)) {
                    for (dst, src) in outs.iter_mut().zip(&work[..nch]) {
                        dst[..n].copy_from_slice(&src[..n]);
                        mixer::sanitize(&mut dst[..n]);
                    }
                }
                mixer.process_peer(slot, n);
            }
        }
        // step 15: each peer's send (with patchbay cross-routes), then the echo service and
        // our latency tests
        let mut queued = false;
        for slot in 0..MAX_PEERS {
            let mix = mixer.peer_send(slot, n);
            queued |= links.send(slot, mix, n, now);
            #[cfg(feature = "modern")]
            if mlinks.has_private(slot) {
                queued |= mlinks.send(slot, mix, n, inow);
            }
        }
        #[cfg(feature = "modern")]
        {
            queued |= mlinks.send_main(mixer.send_mix(), n, inow);
        }
        for slot in 0..MAX_PEERS {
            queued |= links.echo_and_latency(slot, n, now, sample_rate, mixer.latency_monitor());
        }
        let nout = outb.len();
        let mut it = outb.iter_mut();
        let mut outs: [&mut [f32]; MAX_CHANNELS] =
            std::array::from_fn(|_| it.next().map_or(&mut [][..], |v| &mut v[..]));
        mixer.process_output(&mut outs[..nout], n);
        blocks.fetch_add(1, Ordering::Relaxed);
        // the next block starts this much later in the host's timeline
        if let Some(h) = &mut mixer.host
            && let Some(ppq) = &mut h.ppq
            && h.playing
        {
            *ppq += h.bpm.max(0.0) / 60.0 * n as f64 / f64::from(sample_rate);
        }
        queued
    }
}

/// `k` frames of the block buffers `src` at `at` into the callback's `outputs` at `to`;
/// outputs past `src` are silent.
fn store(src: &[Vec<f32>], outputs: &mut [&mut [f32]], to: usize, at: usize, k: usize) {
    for (c, dst) in outputs.iter_mut().enumerate() {
        let Some(dst) = dst.get_mut(to..to + k) else { continue };
        match src.get(c) {
            Some(src) => dst.copy_from_slice(&src[at..at + k]),
            None => dst.fill(0.0),
        }
    }
}

/// Timer fallback of the send thread (the audio thread wakes it while streams flow).
const SEND_TICK: Duration = Duration::from_millis(20);

/// Send-thread housekeeping of the stage-2 media: file end, soundboard sample ends (and
/// "next", whose files are queued in `next` to be opened off the lock), recording failures
/// and drops. A recording that failed comes back to be finished off the lock.
fn media_tick(
    h: &mut Host,
    s: &mut dyn Proto,
    next: &mut Vec<playback::PendingSample>,
) -> Option<recording::Stopped> {
    let mut notes = Vec::new();
    if let Some(f) = &h.file
        && f.poll(&mut |n| notes.push(n))
        && h.model.state.params.sync_met_file
    {
        h.model.state.params.met_enabled = false;
        h.publish();
    }
    {
        let Host { sb, tx, .. } = &mut *h;
        sb.poll(&mut |c| tx.mix(c), &mut |n| notes.push(n), next);
    }
    for n in notes {
        h.note(n);
    }
    if !h.rec.is_recording() {
        return None;
    }
    if !h.with_rec(|r, c| r.poll(c)) {
        return None;
    }
    let peers = s.peer_recs(h);
    let stopped = h.with_rec(|r, c| r.stop(c, &peers));
    s.send_peer_info_all(h);
    stopped
}

fn send_loop(s: &Shared, sock: &UdpSocket, rate: u32) {
    // per peer: its source, and what it sent (applied under `Core` on the next round)
    let mut sources: Vec<(PeerId, Arc<Mutex<Source>>)> = Vec::new();
    let mut sent: Vec<(PeerId, u64, i64)> = Vec::new();
    let mut failed: Option<recording::Stopped> = None;
    // soundboard "next" samples: queued under `Core`, opened without it, installed next round
    let mut next: Vec<playback::PendingSample> = Vec::new();
    let mut opened: Vec<io::Result<playback::OpenedSample>> = Vec::new();
    let mut unused: Vec<playback::OpenedSample> = Vec::new();
    #[cfg(feature = "modern")]
    let (mut mstreams, mut msubs, mut msent) = (Vec::new(), Vec::new(), Vec::new());
    while !s.quit.load(Ordering::Acquire) {
        // a recording that failed is finalized without holding `Core`
        let failed_note = failed.take().map(|st| {
            let (files, err) = st.finish();
            (files, err.map_or_else(|| "recording failed".to_owned(), |e| e.to_string()))
        });
        {
            let mut c = lock(&s.core);
            if let Some((files, error)) = failed_note {
                let host = &mut c.host;
                host.note(Notification::RecordingError {
                    error: error.clone(),
                });
                host.note(Notification::RecordingStopped {
                    files,
                    error: Some(error.clone()),
                });
                host.rec.failed = Some(error);
            }
            for (id, bytes, packets) in sent.drain(..) {
                c.legacy.add_sent(id, bytes, packets);
            }
            #[cfg(feature = "modern")]
            if let Some(m) = &mut c.modern {
                for (conn, bytes, packets) in msent.drain(..) {
                    m.add_sent(conn, bytes, packets);
                }
                m.sources(&mut mstreams, &mut msubs);
            }
            let Core { host, legacy, .. } = &mut *c;
            for o in opened.drain(..) {
                let Host { sb, tx, .. } = &mut *host;
                let r = o.and_then(|o| {
                    let (r, u) = sb.install(o, &mut |c| tx.mix(c));
                    unused.extend(u);
                    r
                });
                if let Err(e) = r {
                    host.note(Notification::PlaybackError {
                        error: e.to_string(),
                    });
                }
            }
            host.tx.flush();
            legacy.tick(host, s.blocks.load(Ordering::Relaxed));
            legacy.sources(&mut sources);
            let (host, session) = c.active();
            failed = media_tick(host, session, &mut next);
            host.tx.flush();
        }
        // encoding happens outside `Core`: receiving and snapshots go on meanwhile
        #[cfg(feature = "modern")]
        {
            modern::pump(&mstreams, &msubs, &mut msent);
            mstreams.clear();
            msubs.clear();
        }
        for (id, src) in sources.drain(..) {
            let (mut bytes, mut packets) = (0, 0);
            lock(&src).send(|a, b| {
                legacy::send_to(sock, a, b, &mut bytes);
                packets += 1;
            });
            sent.push((id, bytes, packets));
        }
        unused.clear();
        opened.extend(
            next.drain(..)
                .map(|p| playback::SoundboardPlayer::open(p, rate)),
        );
        if !opened.is_empty() {
            continue; // installed right away
        }
        thread::park_timeout(SEND_TICK);
    }
}

fn recv_loop(s: &Shared, sock: &UdpSocket) {
    let mut buf = [0u8; aoo::MAX_PACKET_SIZE];
    while !s.quit.load(Ordering::Acquire) {
        // errors: read timeouts, and ICMP errors from earlier sends to closed ports
        if let Ok((n, from)) = sock.recv_from(&mut buf) {
            let mut c = lock(&s.core);
            #[cfg(feature = "modern")]
            if c.modern.is_some() {
                continue; // a session is either legacy or modern
            }
            let Core { host, legacy, .. } = &mut *c;
            legacy.on_packet(host, from, &buf[..n]);
        }
    }
}

impl Engine {
    pub fn new(config: EngineConfig) -> io::Result<(Engine, EngineAudio)> {
        let a = config.audio;
        if a.sample_rate == 0 || a.block_size == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sample rate and block size must be positive",
            ));
        }
        let sock = bind(config.udp_port)?;
        sock.set_read_timeout(Some(Duration::from_millis(20)))?;
        let socket_buffers = set_socket_buffers(&sock)?;
        eprintln!(
            "sonobus-engine: UDP socket buffers granted: send {} B, receive {} B",
            socket_buffers.0, socket_buffers.1
        );
        let udp_port = sock.local_addr()?.port();
        let sock = Arc::new(sock);
        let model = Model::new(a, config.state);
        let meters = Arc::new(Meters::default());
        let mixer = Mixer::new(a, model.globals(), meters.clone());
        let (tx, rx) = RingBuffer::new(CMD_RING);
        let (gtx, grx) = RingBuffer::new(1024);
        let mut host = Host {
            cfg: a,
            model,
            tx: AudioTx::new(tx, grx),
            events: VecDeque::new(),
            notes: VecDeque::new(),
            epoch: Instant::now(),
            rec: recording::Recorder::new(meters.clone()),
            file: None,
            sb: playback::SoundboardPlayer::new(),
            last_peer_id: 0,
            #[cfg(feature = "modern")]
            sevents: VecDeque::new(),
        };
        host.publish();
        let session = Session::new(sock.clone(), &host);
        let blocks = Arc::new(AtomicU64::new(0));
        let shared = Arc::new(Shared {
            core: Mutex::new(Core {
                host,
                legacy: session,
                #[cfg(feature = "modern")]
                modern: None,
            }),
            quit: AtomicBool::new(false),
            blocks: blocks.clone(),
            meters,
        });
        let send = {
            let (s, sock) = (shared.clone(), sock.clone());
            thread::Builder::new()
                .name("sonobus-send".into())
                .spawn(move || send_loop(&s, &sock, a.sample_rate))?
        };
        let sender = send.thread().clone();
        let recv = {
            let s = shared.clone();
            thread::Builder::new()
                .name("sonobus-recv".into())
                .spawn(move || recv_loop(&s, &sock))?
        };
        let buf = |ch: usize| vec![vec![0.0; a.block_size]; ch.min(MAX_CHANNELS)];
        let audio = EngineAudio {
            mixer,
            links: Links::new(),
            #[cfg(feature = "modern")]
            mlinks: modern::Links::new(),
            rx,
            garbage: gtx,
            blocks,
            cfg: a,
            sender: sender.clone(),
            inb: buf(a.input_channels),
            outb: buf(a.output_channels),
            pos: 0,
            buffered: false,
            peer_outb: Vec::new(),
            peer_out_ch: 0,
        };
        Ok((
            Engine {
                shared,
                sender,
                threads: vec![send, recv],
                udp_port,
                cfg: a,
                socket_buffers,
                final_notes: NotificationHandle::default(),
                #[cfg(feature = "modern")]
                identity: Mutex::new(sonobus_net::generate_secret_key()),
                #[cfg(feature = "modern")]
                lan: Mutex::new(None),
            },
            audio,
        ))
    }

    fn with_core<R>(&self, f: impl FnOnce(&mut Core) -> R) -> R {
        let r = {
            let mut c = lock(&self.shared.core);
            let r = f(&mut c);
            c.host.tx.flush();
            r
        };
        self.sender.unpark();
        r
    }

    /// `f` on the session in use (legacy or modern).
    fn with<R>(&self, f: impl FnOnce(&mut Host, &mut dyn Proto) -> R) -> R {
        self.with_core(|c| {
            let (h, s) = c.active();
            f(h, s)
        })
    }

    /// `f` on the legacy session.
    fn with_legacy<R>(&self, f: impl FnOnce(&mut Host, &mut Session) -> R) -> R {
        self.with_core(|c| f(&mut c.host, &mut c.legacy))
    }

    fn with_peer(&self, id: PeerId, f: impl FnOnce(&mut Host, &mut dyn Proto, usize)) {
        self.with(|h, s| {
            if let Some(i) = s.peer_index(id) {
                f(h, s, i);
            }
        });
    }

    /// End the modern session, if any: its peers leave the mixer under `Core`, then (outside
    /// it) its thread stops and its endpoint says BYE.
    fn end_modern(&self) {
        #[cfg(feature = "modern")]
        {
            let old = self.with_core(|c| {
                let Core { host, modern, .. } = c;
                if let Some(m) = modern.as_mut() {
                    m.close(host);
                }
                modern.take()
            });
            drop(old);
        }
    }

    pub fn audio_config(&self) -> AudioConfig {
        self.cfg
    }

    pub fn local_port(&self) -> u16 {
        self.udp_port
    }

    /// UDP socket buffer sizes the OS granted: (send, receive) in bytes.
    pub fn socket_buffer_sizes(&self) -> (usize, usize) {
        self.socket_buffers
    }

    // ---- session ----------------------------------------------------------------------------

    /// Log in to a connection server, then join `group` (or watch the public groups). Resolves
    /// `host` on the calling thread.
    pub fn connect(
        &self,
        host: &str,
        port: u16,
        user: &str,
        password: &str,
        group: Option<GroupJoin>,
    ) -> io::Result<()> {
        let addr = (host, port)
            .to_socket_addrs()?
            .find(SocketAddr::is_ipv4)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "no IPv4 address for server"))?;
        let group = group.map(|g| (g.name, g.password, g.public));
        self.end_modern();
        self.with_legacy(|h, s| s.connect(h, addr, host, port, user, password, group));
        Ok(())
    }

    pub fn disconnect(&self) {
        self.with_legacy(|h, s| s.disconnect(h));
    }

    pub fn join_group(&self, group: &str, password: &str, public: bool) {
        self.with_legacy(|h, s| s.join_group(h, group, password, public));
    }

    pub fn leave_group(&self) {
        self.with_legacy(|_, s| s.leave_group());
    }

    pub fn watch_public_groups(&self, watch: bool) {
        self.with_legacy(|_, s| s.watch_public_groups(watch));
    }

    /// Serverless connection to a peer's UDP address (`connectRemotePeer`).
    pub fn connect_direct(&self, addr: SocketAddr) {
        self.end_modern();
        self.with_legacy(|h, s| {
            let reciprocate = !h.model.state.params.main_recv_mute;
            s.connect_peer(h, addr, "", "", reciprocate, false);
        });
    }

    // ---- peers ------------------------------------------------------------------------------

    pub fn remove_peer(&self, id: PeerId) {
        self.with(|h, s| s.remove_peer(h, id, false));
    }

    /// Block the peer: it is removed, told so, and ignored from now on. Legacy peers are
    /// blocked by IP address ([`Self::blocked_addresses`]), modern ones by endpoint id for the
    /// rest of the session.
    pub fn block_peer(&self, id: PeerId) {
        self.with_peer(id, |h, s, i| s.block_peer_at(h, i));
    }

    /// The block list (persisted by the app in `GlobalState.xml`).
    pub fn blocked_addresses(&self) -> Vec<IpAddr> {
        self.with_legacy(|_, s| s.blocked.clone())
    }

    pub fn set_blocked_addresses(&self, ips: Vec<IpAddr>) {
        self.with_legacy(|_, s| s.blocked = ips);
    }

    fn edit_peer_mix(&self, id: PeerId, f: impl FnOnce(&mut model::PeerModel)) {
        self.with_peer(id, |h, s, i| {
            let slot = s.peer(i).slot;
            if let Some(p) = h.model.peers[slot].as_mut() {
                f(p);
            }
            h.publish_peer(slot);
        });
    }

    /// Peer level, 0..2 (upstream applies it twice, so the gain is `level²`).
    pub fn set_peer_level(&self, id: PeerId, level: f32) {
        self.edit_peer_mix(id, |p| p.params.gain = level);
    }

    pub fn set_peer_soloed(&self, id: PeerId, soloed: bool) {
        self.edit_peer_mix(id, |p| p.params.soloed = soloed);
    }

    /// Edit the channel groups a peer is mixed with (gain, mute, pan, effects, solo flags).
    /// Marks them as user-modified: later remote layouts with the same channel total no
    /// longer replace them.
    pub fn update_peer_groups(
        &self,
        id: PeerId,
        f: impl FnOnce(&mut Vec<ChannelGroup>, &mut Vec<bool>),
    ) {
        self.with_peer(id, |h, s, i| {
            let slot = s.peer(i).slot;
            h.model.edit_peer_groups(slot, f);
            h.publish_peer(slot);
        });
    }

    /// Stop/resume sending to this peer ("Disable Sending").
    pub fn set_peer_send_muted(&self, id: PeerId, muted: bool) {
        self.with_peer(id, |_, s, i| {
            if muted {
                s.set_send_allow(i, false)
            } else {
                s.set_send_active(i, true)
            }
        });
    }

    /// Stop/resume receiving from this peer (uninvites/invites its stream).
    pub fn set_peer_recv_muted(&self, id: PeerId, muted: bool) {
        self.with_peer(id, |_, s, i| {
            if muted {
                s.set_recv_allow(i, false)
            } else {
                s.set_recv_active(i, true)
            }
        });
    }

    /// Send format for one peer (index into [`formats::FORMATS`]).
    pub fn set_peer_send_format(&self, id: PeerId, index: usize) {
        self.with_peer(id, |h, s, i| s.set_send_format(h, i, index));
    }

    /// "Change for all": every peer's send format and the default.
    pub fn set_send_format_for_all(&self, index: usize) {
        self.with(|h, s| {
            h.model.state.params.def_send_qual = index as i32;
            for i in 0..s.len() {
                s.set_send_format(h, i, index);
            }
            h.publish();
        });
    }

    /// Ask the peer to send us this format (`None`: no preference).
    pub fn request_peer_send_format(&self, id: PeerId, index: Option<usize>) {
        self.with_peer(id, |h, s, i| s.request_format(h, i, index));
    }

    /// Jitter buffer in ms; 0 = reset (the sink always buffers at least one block).
    pub fn set_peer_buffer_ms(&self, id: PeerId, ms: f32) {
        self.with_peer(id, |h, s, i| s.set_buffer(h, i, ms));
    }

    pub fn set_peer_jitter_mode(&self, id: PeerId, mode: JitterMode) {
        self.with_peer(id, |h, s, i| s.set_jitter_mode(h, i, mode));
    }

    pub fn reset_peer_drop_count(&self, id: PeerId) {
        self.with_peer(id, |h, s, i| s.reset_drops(h, i));
    }

    /// "Reset All Jitter Buffers".
    pub fn reset_all_jitter_buffers(&self) {
        self.with(|h, s| {
            for i in 0..s.len() {
                s.set_buffer(h, i, 0.0);
            }
        });
    }

    // ---- chat -------------------------------------------------------------------------------

    /// Send to the named users, or to everyone when `targets` is empty. A message too large
    /// for one packet is not sent and not recorded: an [`Event::Error`] is queued instead (and
    /// the returned message is the unsent one). [`Self::try_send_chat`] reports it directly.
    pub fn send_chat(&self, targets: &[&str], message: &str) -> ChatMessage {
        self.with(|h, s| {
            let m = s.chat_message(targets, message);
            if let Err(e) = s.send_chat(&m) {
                h.event(Event::Error(e));
            }
            m
        })
    }

    /// [`Self::send_chat`], with the error (message too large) returned instead of queued.
    pub fn try_send_chat(&self, targets: &[&str], message: &str) -> Result<ChatMessage, String> {
        self.with(|_, s| {
            let m = s.chat_message(targets, message);
            s.send_chat(&m).map(|()| m)
        })
    }

    /// Send to the given peers (by id: in modern sessions display names are only labels), or
    /// to everyone when `ids` is empty. The message's `targets` lists their names. Errors as
    /// [`Self::try_send_chat`].
    pub fn try_send_chat_to(&self, ids: &[PeerId], message: &str) -> Result<ChatMessage, String> {
        self.with(|_, s| {
            let idx: Vec<usize> = ids.iter().filter_map(|&id| s.peer_index(id)).collect();
            if !ids.is_empty() && idx.is_empty() {
                return Err("no such peer".into());
            }
            let names: Vec<String> = idx.iter().map(|&i| s.peer(i).user.clone()).collect();
            let names: Vec<&str> = names.iter().map(String::as_str).collect();
            let m = s.chat_message(&names, message);
            s.send_chat_where(&m, &|i| idx.is_empty() || idx.contains(&i))
                .map(|()| m)
        })
    }

    /// The last 500 chat messages, oldest first.
    pub fn chat_history(&self) -> Vec<ChatMessage> {
        self.with(|_, s| s.common().chat.iter().cloned().collect())
    }

    // ---- settings ---------------------------------------------------------------------------

    /// The current settings, with every connected peer's settings committed to the peer cache.
    pub fn state(&self) -> AppState {
        self.with(|h, s| {
            s.commit_caches(h);
            let solo = options::flags_to_string(&h.model.input_solo);
            let e = &mut h.model.state.extra;
            options::set(e, options::INPUT_SOLO_STATES, Var::String(solo));
            h.model.state.clone()
        })
    }

    /// Change settings (parameters, input groups, metronome group, options) and apply the
    /// side effects upstream's `parameterChanged` has (send layout, mutes, formats, ...).
    pub fn update_state(&self, f: impl FnOnce(&mut AppState)) {
        self.with(|h, s| {
            let old_shape = h.model.send_layout_shape();
            let old = h.model.state.params.clone();
            f(&mut h.model.state);
            h.model.state.input_groups.truncate(mixer::MAX_GROUPS);
            let n = h.model.state.input_groups.len();
            h.model.input_solo.resize(n, false);
            let p = h.model.state.params.clone();
            // structure (groups, channel counts, names, send mode), not pans: upstream sends
            // the layout only on structural edits, and a pan change sends nothing
            if h.model.send_layout_shape() != old_shape {
                s.update_send_channels(h);
            }
            if p.def_send_qual != old.def_send_qual && h.model.state.extra.change_qual_for_all {
                let fi = formats::checked(p.def_send_qual);
                for i in 0..s.len() {
                    s.set_send_format(h, i, fi);
                }
            }
            let changed = |a: bool, b: bool| (a != b).then_some(b);
            s.on_main_mutes(
                changed(old.main_send_mute, p.main_send_mute),
                changed(old.main_recv_mute, p.main_recv_mute),
            );
            if old.dynamic_resampling != p.dynamic_resampling {
                s.set_dynamic_resampling(p.dynamic_resampling);
            }
            h.publish();
        });
    }

    /// Input group solo (not persisted upstream; here kept in the state behind "Remember Local
    /// Track Solo", PR #291).
    pub fn set_input_soloed(&self, group: usize, soloed: bool) {
        self.with(|h, _| {
            if let Some(v) = h.model.input_solo.get_mut(group) {
                *v = soloed;
                h.publish();
            }
        });
    }

    // ---- options (stage 2) ------------------------------------------------------------------

    /// PR #202 "Packet redundancy": every packet to every peer goes out `n` times (1..=5;
    /// multiplies the upload and the peers' download). Persisted.
    pub fn set_packet_redundancy(&self, n: i32) {
        let n = n.clamp(1, 5);
        self.with(|h, s| {
            options::set(
                &mut h.model.state.extra,
                options::PACKET_REDUNDANCY,
                Var::Int(n),
            );
            s.set_redundancy(n);
        });
    }

    /// "Max Send Quality": the best send format (table index) a remote peer's quality request
    /// can get from us (`None`: no cap). Both protocols; persisted
    /// ([`options::MAX_SEND_QUALITY`]). Our own format choices are not capped.
    pub fn set_max_send_quality(&self, index: Option<usize>) {
        let v = index.filter(|&i| i < formats::FORMATS.len()).map_or(-1, |i| i as i32);
        self.with(|h, _| options::set(&mut h.model.state.extra, options::MAX_SEND_QUALITY, Var::Int(v)));
    }

    pub fn max_send_quality(&self) -> Option<usize> {
        self.with(|h, _| options::max_send_quality(&h.model.state.extra))
    }

    /// Follow remote peers' requests for our send format (default on, as upstream). Off:
    /// requests are ignored. Persisted ([`options::HONOUR_QUALITY_REQUESTS`]).
    pub fn set_honour_quality_requests(&self, on: bool) {
        self.with(|h, _| {
            options::set(&mut h.model.state.extra, options::HONOUR_QUALITY_REQUESTS, Var::Bool(on))
        });
    }

    pub fn honour_quality_requests(&self) -> bool {
        self.with(|h, _| options::honour_quality_requests(&h.model.state.extra))
    }

    pub fn packet_redundancy(&self) -> i32 {
        self.with(|h, _| options::packet_redundancy(&h.model.state.extra))
    }

    /// PR #291 "Remember Local Track Solo" / "Remember Peer Solo" (both default on; they take
    /// effect on the next load of the state and on peers joining).
    pub fn set_remember_solo(&self, input: bool, peer: bool) {
        self.with(|h, _| {
            let e = &mut h.model.state.extra;
            options::set(e, options::REMEMBER_INPUT_SOLO, Var::Bool(input));
            options::set(e, options::REMEMBER_PEER_SOLO, Var::Bool(peer));
        });
    }

    /// Metronome beats per bar, 1..=12 (1: no bar accent, upstream's fixed value). Persisted.
    pub fn set_met_beats_per_bar(&self, n: i32) {
        self.with(|h, _| {
            let e = &mut h.model.state.extra;
            options::set(e, options::MET_BEATS_PER_BAR, Var::Int(n.clamp(1, 12)));
            h.publish();
        });
    }

    /// PR #78: the default recording options (1 mix, 2 self, 4 mix minus self, 8 each user);
    /// a mask without anything or with only "each user" gets the full mix. Returns the stored
    /// mask.
    pub fn set_recording_options(&self, mask: i32) -> i32 {
        let m = recording::normalize_options(mask);
        self.with(|h, _| h.model.state.extra.default_recording_options = m);
        m
    }

    // ---- recording --------------------------------------------------------------------------

    /// Upstream's default name: `<group or SonoBusSession>_<date>_<time>` at local time `t`.
    pub fn recording_base_name(&self, t: LocalTime) -> String {
        let group = self.with(|_, s| s.common().group.clone().unwrap_or_default());
        sonobus_files::session_base_name(&group, t)
    }

    /// Start recording into `dir` (created if missing) with the options, format and bit depth
    /// of `AppState.extra` (`DefaultRecording*`, `RecordSelf*`). Returns the file (mix only) or
    /// the folder. Peers see the recording flag in `/sb/pinfo`.
    pub fn start_recording(&self, dir: &Path, base_name: &str) -> io::Result<PathBuf> {
        self.with(|h, s| {
            let peers = s.peer_recs(h);
            let r = h.with_rec(|r, c| r.start(c, dir, base_name, &peers));
            if r.is_ok() {
                s.send_peer_info_all(h);
            }
            r
        })
    }

    /// Stop and finalize every file (waits for the disk threads). The files written; the first
    /// write error, if any, as the error. A recording that already stopped by itself because
    /// a file could not be written ([`Notification::RecordingError`]) gives that error here,
    /// once.
    pub fn stop_recording(&self) -> io::Result<Vec<PathBuf>> {
        let (stopped, failed) = self.with(|h, s| {
            let peers = s.peer_recs(h);
            let st = h.with_rec(|r, c| r.stop(c, &peers));
            s.send_peer_info_all(h);
            (st, h.rec.failed.take())
        });
        let Some(st) = stopped else {
            return failed.map_or(Ok(Vec::new()), |e| Err(io::Error::other(e)));
        };
        let (files, err) = st.finish();
        let error = err.as_ref().map(ToString::to_string);
        self.with(|h, _| {
            h.note(Notification::RecordingStopped {
                files: files.clone(),
                error,
            })
        });
        err.map_or(Ok(files), Err)
    }

    pub fn recording_status(&self) -> Option<RecordingStatus> {
        self.with(|h, _| h.rec.status())
    }

    // ---- file playback ----------------------------------------------------------------------

    /// Load a file into the transport (stopped, at the start); replaces any loaded one. Level,
    /// monitor level, routing: `AppState.file_playback_group`; send to peers:
    /// `params.send_file_audio` (both through [`Self::update_state`]).
    pub fn load_file(&self, path: &Path) -> io::Result<FileStatus> {
        // the file is opened without the engine lock; the replaced one is closed after it
        let (f, deck) = playback::FileTransport::open(path, self.cfg.sample_rate)?;
        let (st, old) = self.with(|h, s| {
            let shape = h.model.send_layout_shape();
            h.tx.mix(mixer::Cmd::File(Some(deck)));
            h.model.state.file_playback_group.num_channels = f.channels() as i32;
            let st = f.status();
            let old = h.file.replace(f);
            if h.model.send_layout_shape() != shape {
                s.update_send_channels(h);
            }
            h.publish();
            (st, old)
        });
        drop(old);
        Ok(st)
    }

    pub fn close_file(&self) {
        self.with(|h, s| {
            if h.file.take().is_some() {
                let shape = h.model.send_layout_shape();
                h.tx.mix(mixer::Cmd::File(None));
                h.model.state.file_playback_group.num_channels = 2;
                if h.model.send_layout_shape() != shape {
                    s.update_send_channels(h);
                }
                h.publish();
            }
        });
    }

    /// Play/pause. With `syncMetFile` the metronome starts and stops with the transport, on
    /// the file's beat grid (P.cpp:7695-7724).
    pub fn set_file_playing(&self, playing: bool) {
        self.with(|h, _| {
            let Some(f) = &h.file else { return };
            f.set_playing(playing);
            if h.model.state.params.sync_met_file {
                h.model.state.params.met_enabled = playing;
                h.publish();
            }
        });
    }

    pub fn seek_file(&self, secs: f64) {
        self.with(|h, _| h.file.as_ref().map(|f| f.seek(secs)));
    }

    pub fn set_file_looping(&self, on: bool) {
        self.with(|h, _| h.file.as_ref().map(|f| f.set_looping(on)));
    }

    /// Loop region (seconds; length 0 = whole file), at least 2048 frames long.
    pub fn set_file_loop_range(&self, start_secs: f64, len_secs: f64) {
        self.with(|h, _| {
            h.file
                .as_ref()
                .map(|f| f.set_loop_range(start_secs, len_secs))
        });
    }

    pub fn file_status(&self) -> Option<FileStatus> {
        self.with(|h, _| h.file.as_ref().map(playback::FileTransport::status))
    }

    // ---- soundboard -------------------------------------------------------------------------

    /// The soundboards to play from (the app loads and saves `soundboards.xml`). Playing
    /// samples whose entry changed stop; the others take the new gain and loop setting.
    pub fn set_soundboards(&self, boards: Soundboards) {
        self.with(|h, _| {
            let Host { sb, tx, .. } = h;
            sb.set_boards(boards, &mut |c| tx.mix(c));
        });
    }

    pub fn soundboards(&self) -> Soundboards {
        self.with(|h, _| h.sb.boards.clone())
    }

    /// A sample button goes down (or a programmatic trigger by index, for OSC/MIDI): toggle
    /// buttons stop a playing sample, everything else (re)starts it. Level, monitor level and
    /// send: `AppState.soundboard_group` and `params.send_soundboard_audio`.
    pub fn soundboard_press(&self, key: SampleKey) -> io::Result<()> {
        let pending = self.with(|h, _| {
            let Host { sb, tx, .. } = h;
            sb.press(key, &mut |c| tx.mix(c))
        })?;
        let Some(p) = pending else { return Ok(()) };
        // the file is opened without the engine lock
        let o = playback::SoundboardPlayer::open(p, self.cfg.sample_rate)?;
        let (r, unused) = self.with(|h, _| {
            let Host { sb, tx, .. } = h;
            sb.install(o, &mut |c| tx.mix(c))
        });
        drop(unused);
        r
    }

    /// A sample button goes up: hold buttons stop their sample.
    pub fn soundboard_release(&self, key: SampleKey) {
        self.with(|h, _| {
            let Host { sb, tx, .. } = h;
            sb.release(key, &mut |c| tx.mix(c));
        });
    }

    pub fn stop_sample(&self, key: SampleKey) {
        self.with(|h, _| {
            let Host { sb, tx, .. } = h;
            sb.stop(key, &mut |c| tx.mix(c));
        });
    }

    pub fn stop_all_samples(&self) {
        self.with(|h, _| {
            let Host { sb, tx, .. } = h;
            sb.stop_all(&mut |c| tx.mix(c));
        });
    }

    pub fn soundboard_status(&self) -> Vec<VoiceStatus> {
        self.with(|h, _| h.sb.status())
    }

    // ---- latency ----------------------------------------------------------------------------

    /// Round-trip test through the peer's echo service (stock clients have one too); results
    /// arrive as [`Notification::LatencyMeasured`] until [`Self::stop_latency_test`]. False
    /// while the peer is not connected.
    pub fn start_latency_test(&self, id: PeerId) -> bool {
        let mut ok = false;
        self.with_peer(id, |_, s, i| ok = s.start_latency_test(i));
        ok
    }

    pub fn stop_latency_test(&self, id: PeerId) {
        self.with_peer(id, |_, s, i| s.stop_latency_test(i));
    }

    /// Group latency match, step 1: collect every peer's incoming latencies
    /// ([`Notification::LatencyMatchReady`] once complete).
    pub fn begin_latency_match(&self) {
        self.with(|h, s| s.begin_latency_match(h));
    }

    /// The entries collected so far, whether the table is complete, and the default target
    /// (the largest latency).
    pub fn latency_match_info(&self) -> (Vec<LatEntry>, bool, f32) {
        self.with(|_, s| {
            let lat = &s.common().lat_info;
            let target = latency::match_target(lat.iter().map(|e| e.latency_ms as f32));
            (lat.clone(), s.latency_match_ready(), target)
        })
    }

    /// Ask everyone to match `latency_ms` (`/sb/suggestlat`) and apply it here.
    pub fn send_latency_match(&self, latency_ms: f32) {
        self.with(|h, s| s.send_latency_match(h, latency_ms));
    }

    /// Accept a [`Notification::LatencyMatchRequested`]: pad our receive buffers.
    pub fn commit_latency_match(&self, latency_ms: f32) {
        self.with(|h, s| s.commit_latency_match(h, latency_ms));
    }

    /// Receive Sync: equal total receive latency from every peer, plus `padding_ms` (0..500).
    /// Idempotent. The padding is remembered ([`Self::receive_sync_padding`]).
    pub fn receive_sync(&self, padding_ms: f32) {
        self.with(|h, s| {
            Self::store_sync_padding(h, padding_ms);
            s.receive_sync(h, padding_ms);
        });
    }

    /// The Receive Sync padding last used or set (ms; 0 by default). Persisted in
    /// `AppState.extra` as `ReceiveSyncPadding`, a key stock clients ignore and keep.
    pub fn receive_sync_padding(&self) -> f32 {
        self.with(|h, _| options::receive_sync_padding(&h.model.state.extra))
    }

    /// Remember a padding (clamped to 0..500 ms) without syncing now.
    pub fn set_receive_sync_padding(&self, padding_ms: f32) {
        self.with(|h, _| Self::store_sync_padding(h, padding_ms));
    }

    fn store_sync_padding(h: &mut Host, padding_ms: f32) {
        let ms = if padding_ms.is_finite() { padding_ms.clamp(0.0, 500.0) } else { 0.0 };
        let e = &mut h.model.state.extra;
        options::set(e, options::RECEIVE_SYNC_PADDING, Var::Double(f64::from(ms)));
    }

    /// Invite the named users to another group (`/sb/suggestgroup`).
    pub fn suggest_group(&self, group: &str, password: &str, public: bool, users: &[&str]) {
        self.with(|_, s| s.suggest_group(group, password, public, users));
    }

    // ---- routing ----------------------------------------------------------------------------

    /// Patchbay: send `from`'s audio (after its level and effects) to `to` as well. A peer
    /// that gets cross-routed audio is sent as many channels as we have outputs.
    pub fn set_patch(&self, from: PeerId, to: PeerId, on: bool) {
        self.with(|h, s| {
            let (Some(a), Some(b)) = (s.peer_index(from), s.peer_index(to)) else {
                return;
            };
            let (fs, ts) = (s.peer(a).slot, s.peer(b).slot);
            let r = &mut h.model.routed_to[ts];
            if on {
                *r |= 1 << fs;
            } else {
                *r &= !(1 << fs);
            }
            h.publish();
            s.update_send_channels(h);
        });
    }

    pub fn patch(&self, from: PeerId, to: PeerId) -> bool {
        self.with(|h, s| match (s.peer_index(from), s.peer_index(to)) {
            (Some(a), Some(b)) => h.model.routed_to[s.peer(b).slot] & (1 << s.peer(a).slot) != 0,
            _ => false,
        })
    }

    /// Send this peer exactly `channels` channels (capped at our output count) whatever the
    /// send mode; `None` follows the send mode again.
    pub fn set_peer_send_channels_override(&self, id: PeerId, channels: Option<usize>) {
        self.with_peer(id, |h, s, i| s.set_send_override(h, i, channels));
    }

    // ---- status -----------------------------------------------------------------------------

    pub fn poll_event(&self) -> Option<Event> {
        lock(&self.shared.core).host.events.pop_front()
    }

    pub fn poll_notification(&self) -> Option<Notification> {
        lock(&self.shared.core).host.notes.pop_front()
    }

    pub fn snapshot(&self) -> Snapshot {
        let m = &self.shared.meters;
        let mut snap = self.with_core(|c| {
            let (connection, public_groups, udp_port) = (
                c.legacy.connection(),
                c.legacy.public_groups.clone(),
                c.legacy.local_port,
            );
            let connection = c.modern_connection().unwrap_or(connection);
            let (h, s) = c.active();
            Snapshot {
                connection,
                user: s.common().user.clone(),
                group: s.common().group.clone(),
                public_groups,
                peers: (0..s.len()).map(|i| s.peer_status(h, i)).collect(),
                input: MeterLevels::default(),
                send: MeterLevels::default(),
                output: MeterLevels::default(),
                udp_port,
            }
        });
        (snap.input, snap.send, snap.output) =
            (m.input.levels(), m.send.levels(), m.output.levels());
        snap
    }
}

/// Timer of the modern session thread when no datagram arrives.
#[cfg(feature = "modern")]
const MODERN_TICK: Duration = Duration::from_millis(2);
/// Received datagrams one connection may have waiting for the modern session thread (about
/// a second of audio at 2.5 ms blocks with fragments); beyond that its datagrams are dropped
/// on the network thread, so a flooding peer (or a stall on `Core`) neither grows memory nor
/// pushes other peers' datagrams back by more than this many.
#[cfg(feature = "modern")]
const MODERN_QUEUE_PER_PEER: usize = 1024;

/// One received datagram and its connection's queue count.
#[cfg(feature = "modern")]
type Received = (
    sonobus_net::ConnId,
    sonobus_net::Bytes,
    Arc<std::sync::atomic::AtomicUsize>,
);

/// A connection's datagram sink (on the network thread): into the session thread's queue,
/// at most [`MODERN_QUEUE_PER_PEER`] waiting; beyond that dropped and counted in `drops`.
#[cfg(feature = "modern")]
fn modern_sink(
    conn: sonobus_net::ConnId,
    tx: std::sync::mpsc::Sender<Received>,
    drops: Arc<AtomicU64>,
) -> sonobus_net::Sink {
    let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    Box::new(move |d| {
        if queued.fetch_add(1, Ordering::Relaxed) >= MODERN_QUEUE_PER_PEER {
            queued.fetch_sub(1, Ordering::Relaxed);
            drops.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let _ = tx.send((conn, d, queued.clone()));
    })
}

/// The modern session's thread: its datagrams (from the per-connection sinks), network events
/// and timers, under `Core`; ends when the session is taken out of `Core` or `quit` is set.
#[cfg(feature = "modern")]
fn modern_loop(s: &Shared, quit: &Arc<AtomicBool>, rx: std::sync::mpsc::Receiver<Received>) {
    use std::sync::mpsc::RecvTimeoutError;
    let mut batch = Vec::new();
    while !quit.load(Ordering::Acquire) && !s.quit.load(Ordering::Acquire) {
        match rx.recv_timeout(MODERN_TICK) {
            Ok(d) => batch.push(d),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        while batch.len() < 256 {
            match rx.try_recv() {
                Ok(d) => batch.push(d),
                Err(_) => break,
            }
        }
        let now = Instant::now();
        let mut c = lock(&s.core);
        let Core { host, modern, .. } = &mut *c;
        let Some(m) = modern.as_mut().filter(|m| Arc::ptr_eq(&m.quit, quit)) else {
            break;
        };
        for (conn, d, queued) in batch.drain(..) {
            queued.fetch_sub(1, Ordering::Relaxed);
            m.on_datagram(host, conn, &d, now);
        }
        m.on_events(host);
        m.tick(host, s.blocks.load(Ordering::Relaxed), now);
        host.tx.flush();
    }
}

/// Modern (protocol v2) sessions. See [`modern`] for the protocol conventions; a session is
/// either legacy (`connect`, `connect_direct`) or modern (these): starting one ends the other.
#[cfg(feature = "modern")]
impl Engine {
    /// This device's identity key for modern sessions (its endpoint id derives from it).
    /// Generated at engine start; persist it and give it back with
    /// [`Self::set_identity_key`] so invite links naming us keep working.
    pub fn identity_key(&self) -> [u8; 32] {
        *lock(&self.identity)
    }

    /// The identity for the next modern session.
    pub fn set_identity_key(&self, key: [u8; 32]) {
        *lock(&self.identity) = key;
    }

    fn start_modern(&self, net: &modern::NetOptions, mut start: modern::Start) -> io::Result<()> {
        let invalid = |e: String| io::Error::new(io::ErrorKind::InvalidInput, e);
        if let modern::Start::Host(o) = &mut start {
            o.seed.get_or_insert_with(sonobus_net::auth::random_seed);
        }
        let cfg = modern::Session::config(net, &start, self.identity_key()).map_err(invalid)?;
        self.end_modern();
        self.with_legacy(|h, s| s.disconnect(h));
        let (dg_tx, dg_rx) = std::sync::mpsc::channel::<Received>();
        let dropped = Arc::new(AtomicU64::new(0));
        let drops = dropped.clone();
        let n = sonobus_net::Net::start(cfg, move |conn, _| {
            modern_sink(conn, dg_tx.clone(), drops.clone())
        })?;
        let quit = self.with_core(|c| {
            let mut m = modern::Session::new(&mut c.host, n, start, net)?;
            m.dropped = dropped;
            let q = m.quit.clone();
            c.modern = Some(m);
            Ok::<_, String>(q)
        });
        let quit = quit.map_err(invalid)?;
        let (s, q) = (self.shared.clone(), quit.clone());
        let t = thread::Builder::new()
            .name("sonobus-modern".into())
            .spawn(move || modern_loop(&s, &q, dg_rx))?;
        self.with_core(|c| match &mut c.modern {
            Some(m) if Arc::ptr_eq(&m.quit, &quit) => m.thread = Some(t),
            // already ended: the thread sees it and stops
            _ => {}
        });
        Ok(())
    }

    /// Host a modern session (mesh or broadcast). The invite link and the PIN are in
    /// [`Self::session_status`] (the link changes once our relay URL is known:
    /// [`modern::SessionEvent::InviteChanged`]).
    pub fn host_session(
        &self,
        net: &modern::NetOptions,
        opts: modern::HostOptions,
    ) -> io::Result<modern::SessionStatus> {
        let register = opts.register_at.clone();
        self.start_modern(net, modern::Start::Host(opts))?;
        if let Some(server) = register {
            self.with_core(|c| c.modern.as_ref().map(|m| m.register_by_name(server)));
        }
        self.session_status()
            .ok_or_else(|| io::Error::other("session ended"))
    }

    /// Join with an invite link (`sonobus2://join/...` or the bare code). A broadcast invite
    /// joins as a listener.
    pub fn join_invite(&self, net: &modern::NetOptions, link: &str) -> io::Result<()> {
        let inv = sonobus_net::auth::Invite::parse(link)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, format!("invite: {e}")))?;
        self.start_modern(net, modern::Start::Invite(inv))
    }

    /// Join a session found by [`Self::lan_sessions`] with its PIN or password.
    pub fn join_lan(
        &self,
        net: &modern::NetOptions,
        session: &modern::DiscoveredSession,
        secret: &str,
    ) -> io::Result<()> {
        let start = modern::Start::Lan(session.clone(), secret.to_owned());
        self.start_modern(net, start)
    }

    /// Join session `name` with `password` through the rendezvous server at `server` (a host
    /// serving rendezvous gives its address in [`modern::SessionStatus::rendezvous`]).
    pub fn join_by_name(
        &self,
        net: &modern::NetOptions,
        server: modern::PeerAddr,
        name: &str,
        password: &str,
    ) -> io::Result<()> {
        let start = modern::Start::Name(server, name.to_owned(), password.to_owned());
        self.start_modern(net, start)
    }

    /// Leave the modern session (no-op without one).
    pub fn leave_session(&self) {
        self.end_modern();
    }

    pub fn session_status(&self) -> Option<modern::SessionStatus> {
        self.with_core(|c| c.modern.as_ref().map(modern::Session::status))
    }

    /// Host: the invite link (URL form).
    pub fn invite_link(&self) -> Option<String> {
        self.with_core(|c| c.modern.as_ref().and_then(modern::Session::invite))
    }

    pub fn poll_session_event(&self) -> Option<modern::SessionEvent> {
        lock(&self.shared.core).host.sevents.pop_front()
    }

    /// Host: remove `remove` (if any) from the whole session and rotate the session key, so
    /// that it cannot come back with the old invite link. Returns the new invite link.
    pub fn rotate_session_key(&self, remove: Option<PeerId>) -> Option<String> {
        self.with_core(|c| {
            let Core { host, modern, .. } = c;
            modern.as_mut()?.rotate_key(host, remove)
        })
    }

    /// Host in PIN mode: a fresh PIN (after [`modern::SessionEvent::PinInvalidated`]).
    pub fn new_session_pin(&self) -> Option<String> {
        self.with_core(|c| c.modern.as_mut()?.new_pin())
    }

    /// Host: the password accepted for new password joins (`None`: none).
    pub fn set_session_password(&self, password: Option<String>) -> io::Result<()> {
        self.with_core(|c| match c.modern.as_mut() {
            Some(m) => m.set_password(password),
            None => Err("no session".into()),
        })
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))
    }

    /// Start (or restart) browsing for LAN sessions.
    pub fn lan_browse(&self, scope: modern::LanScope) -> io::Result<()> {
        let lan = match scope {
            modern::LanScope::Off => {
                self.stop_lan_browse();
                return Ok(());
            }
            modern::LanScope::Loopback => sonobus_net::lan::Lan::loopback()?,
            modern::LanScope::Network => sonobus_net::lan::Lan::new()?,
        };
        let browser = lan.browse()?;
        let old = lock(&self.lan).replace((lan, browser));
        drop(old);
        Ok(())
    }

    /// The sessions found so far (empty while not browsing).
    pub fn lan_sessions(&self) -> Vec<modern::DiscoveredSession> {
        let mut g = lock(&self.lan);
        let Some((_, b)) = g.as_mut() else {
            return Vec::new();
        };
        while b.recv_timeout(Duration::ZERO).is_some() {}
        b.sessions().cloned().collect()
    }

    pub fn stop_lan_browse(&self) {
        let old = lock(&self.lan).take();
        drop(old);
    }

    /// Encoder invocations of every modern send stream (one per block and encoding).
    #[doc(hidden)]
    pub fn modern_encode_count(&self) -> u64 {
        self.with_core(|c| c.modern.as_ref().map_or(0, modern::Session::encode_count))
    }

    /// Datagrams the modern session dropped on arrival because their connection's queue was
    /// full (a flooding peer, or the session thread stalled).
    #[doc(hidden)]
    pub fn modern_dropped_datagrams(&self) -> u64 {
        self.with_core(|c| {
            c.modern
                .as_ref()
                .map_or(0, |m| m.dropped.load(Ordering::Relaxed))
        })
    }

    /// Endpoint ids blocked in this modern session ([`Self::block_peer`],
    /// [`Self::rotate_session_key`]); not persisted, the list ends with the session.
    pub fn blocked_endpoints(&self) -> Vec<[u8; 32]> {
        self.with_core(|c| c.modern.as_ref().map(modern::Session::blocked).unwrap_or_default())
    }

    /// Let a blocked endpoint connect again (it still needs the current session key).
    pub fn unblock_endpoint(&self, id: [u8; 32]) {
        self.with_core(|c| {
            if let Some(m) = &mut c.modern {
                m.unblock(&id);
            }
        });
    }

    /// Broadcast host: play out in sync across all listeners, `ms` after capture (see
    /// [`modern::HostOptions::playout_ms`]); `None` turns it off. Ignored unless we host a
    /// broadcast session.
    pub fn set_playout_sync(&self, ms: Option<u16>) {
        self.with_core(|c| {
            if let Some(m) = &mut c.modern {
                m.set_playout_ms(ms);
            }
        });
    }

    /// Test hook: hold every AUDIO datagram the modern session receives `ms` longer.
    #[doc(hidden)]
    pub fn set_modern_receive_delay(&self, ms: u32) {
        self.with_core(|c| {
            if let Some(m) = &mut c.modern {
                m.rx_delay = Duration::from_millis(ms.into());
            }
        });
    }

    /// Test hook: the modern session's PING/PONG/PLAYOUT clock reads `ms` ahead (as another
    /// device's clock would).
    #[doc(hidden)]
    pub fn set_modern_clock_skew(&self, ms: u32) {
        self.with_core(|c| {
            if let Some(m) = &mut c.modern {
                m.clock_skew_us = u64::from(ms) * 1000;
            }
        });
    }

    /// Test hook: drop `percent` of the AUDIO datagrams the modern session receives.
    #[doc(hidden)]
    pub fn set_modern_receive_loss(&self, percent: u32) {
        self.with_core(|c| {
            if let Some(m) = &mut c.modern {
                m.loss_pct = percent.min(100);
            }
        });
    }
}

/// Notifications that outlive the [`Engine`]: [`Engine::notification_handle`]. When the
/// engine is dropped it stops a running recording (finalizing its files) and moves every
/// notification still queued here, the final [`Notification::RecordingStopped`] included.
/// The app takes one before dropping or rebuilding an engine (say on a device or sample-rate
/// change) and polls it afterwards; a recording does not carry over to the new engine, the
/// app may start a new one.
#[derive(Clone, Default)]
pub struct NotificationHandle(Arc<Mutex<VecDeque<Notification>>>);

impl NotificationHandle {
    pub fn poll(&self) -> Option<Notification> {
        lock(&self.0).pop_front()
    }
}

impl Engine {
    /// See [`NotificationHandle`].
    pub fn notification_handle(&self) -> NotificationHandle {
        self.final_notes.clone()
    }
}

impl Drop for Engine {
    /// Stops a running recording (waits for its files; the audio thread's release of the
    /// writers is waited for at most [`recording`]'s `DETACH_WAIT`), leaves the server, ends the
    /// network threads, and hands the remaining notifications to [`NotificationHandle`]s.
    fn drop(&mut self) {
        let _ = self.stop_recording();
        self.end_modern();
        #[cfg(feature = "modern")]
        self.stop_lan_browse();
        {
            let mut c = lock(&self.shared.core);
            let Core { host, legacy, .. } = &mut *c;
            legacy.shutdown(host);
        }
        self.shared.quit.store(true, Ordering::Release);
        self.sender.unpark();
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
        let notes = std::mem::take(&mut lock(&self.shared.core).host.notes);
        lock(&self.final_notes.0).extend(notes);
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::cell::Cell;

    thread_local! {
        static FP_IN_PROCESS: Cell<Option<usize>> = const { Cell::new(None) };
    }

    pub fn probe_fp_mode() {
        FP_IN_PROCESS.with(|c| c.set(Some(fpmode::current())));
    }

    /// A `Host` with no engine around it (the audio ring is never drained).
    pub fn host(cfg: AudioConfig, state: AppState) -> Host {
        let (tx, _rx) = RingBuffer::new(CMD_RING);
        let (_gtx, grx) = RingBuffer::new(16);
        Host {
            cfg,
            model: Model::new(cfg, state),
            tx: AudioTx::new(tx, grx),
            events: VecDeque::new(),
            notes: VecDeque::new(),
            epoch: Instant::now(),
            rec: recording::Recorder::new(Arc::new(Meters::default())),
            file: None,
            sb: playback::SoundboardPlayer::new(),
            last_peer_id: 0,
            #[cfg(feature = "modern")]
            sevents: VecDeque::new(),
        }
    }

    pub fn events(h: &mut Host) -> Vec<Event> {
        h.events.drain(..).collect()
    }

    fn cfg(n_in: usize, n_out: usize) -> AudioConfig {
        AudioConfig {
            sample_rate: 48000,
            block_size: 256,
            input_channels: n_in,
            output_channels: n_out,
        }
    }

    fn engine(state: AppState, n_in: usize, n_out: usize) -> (Engine, EngineAudio) {
        Engine::new(EngineConfig {
            audio: cfg(n_in, n_out),
            udp_port: 0,
            state,
        })
        .unwrap()
    }

    /// Monitoring with effects and the main reverb, everything set from the start (no
    /// command timing in the way).
    fn fx_state() -> AppState {
        let mut s = AppState::new(false);
        s.params.dry = 1.0;
        s.params.main_reverb_enabled = true;
        let mut g = ChannelGroup::input_default(0, false);
        g.num_channels = 2;
        g.pans = vec![-1.0, 1.0];
        g.compressor.enabled = true;
        g.eq.enabled = true;
        g.mon_reverb_send = 0.5;
        s.input_groups = vec![g];
        s
    }

    fn signal(len: usize, ch: usize) -> Vec<Vec<f32>> {
        (0..ch)
            .map(|c| {
                let mut x = 7 + c as u32;
                (0..len)
                    .map(|i| {
                        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                        0.3 * (i as f32 * 0.01 * (c + 1) as f32).sin()
                            + 0.1 * ((x >> 8) as f32 / (1u32 << 24) as f32 - 0.5)
                    })
                    .collect()
            })
            .collect()
    }

    /// Feed `input` in callbacks of `sizes` (cycled) with `n_out` device outputs.
    fn render(
        a: &mut EngineAudio,
        input: &[Vec<f32>],
        sizes: &[usize],
        n_out: usize,
    ) -> Vec<Vec<f32>> {
        let len = input[0].len();
        let mut out = vec![vec![0.0f32; len]; n_out];
        let mut at = 0;
        for &k in sizes.iter().cycle() {
            if at >= len {
                break;
            }
            let k = k.min(len - at);
            let ins: Vec<&[f32]> = input.iter().map(|c| &c[at..at + k]).collect();
            let mut outs: Vec<&mut [f32]> = out.iter_mut().map(|c| &mut c[at..at + k]).collect();
            a.process(&ins, &mut outs);
            at += k;
        }
        out
    }

    #[test]
    fn any_callback_size_gives_the_same_audio() {
        let len = 256 * 40;
        let input = signal(len, 2);
        let reference = {
            let (_e, mut a) = engine(fx_state(), 2, 2);
            render(&mut a, &input, &[256], 2)
        };
        assert!(reference[0].iter().any(|&x| x.abs() > 0.1));
        for sizes in [
            &[64][..],
            &[256],
            &[257],
            &[512],
            &[1000],
            &[100, 300, 17, 512],
        ] {
            let (_e, mut a) = engine(fx_state(), 2, 2);
            let out = render(&mut a, &input, sizes, 2);
            // aligned: no delay; after a misaligned callback: exactly one block late
            let tail = 256 * 4..len;
            let lag = if out[0][tail.clone()] == reference[0][tail.clone()] {
                0
            } else {
                256
            };
            for c in 0..2 {
                assert_eq!(
                    &out[c][tail.clone()],
                    &reference[c][tail.start - lag..tail.end - lag],
                    "sizes {sizes:?} ch {c}"
                );
            }
            let want_lag = if sizes.iter().all(|s| s % 256 == 0) {
                0
            } else {
                256
            };
            assert_eq!(lag, want_lag, "sizes {sizes:?}");
        }
    }

    #[test]
    fn device_channels_beyond_or_short_of_the_configuration() {
        let input = signal(256 * 8, 1);
        // 2 configured inputs, the device gives 1: the second is silence; 3 device outputs
        // for 2 configured: the third is zeroed
        let (_e, mut a) = engine(fx_state(), 2, 2);
        let mut garbage = vec![vec![5.0f32; 256 * 8]; 3];
        {
            let ins: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
            let mut outs: Vec<&mut [f32]> = garbage.iter_mut().map(|c| &mut c[..]).collect();
            a.process(&ins, &mut outs);
        }
        assert!(garbage[2].iter().all(|&x| x == 0.0));
        assert!(garbage[0].iter().any(|&x| x != 0.0));
        // the stereo group panned hard: the right output only carries the (silent) second input
        // plus the reverb's stereo spread, so it is quieter than the left
        let e = |c: &[f32]| c.iter().map(|x| x * x).sum::<f32>();
        assert!(e(&garbage[1]) < e(&garbage[0]) * 0.5);
        // and a callback longer than the block with outputs longer than the inputs: frames
        // past the shortest output are zeroed, nothing panics
        let mut outs = [vec![5.0f32; 600], vec![5.0f32; 500]];
        let ins = [&input[0][..500], &input[0][..500]];
        let [o0, o1] = &mut outs;
        a.process(&ins, &mut [&mut o0[..], &mut o1[..]]);
        assert!(o0[500..].iter().all(|&x| x == 0.0));
    }

    #[test]
    fn non_finite_device_input_is_sanitised() {
        let (_e, mut a) = engine(fx_state(), 2, 2);
        let mut input = signal(256 * 20, 2);
        for c in &mut input {
            for (i, x) in c.iter_mut().enumerate() {
                if i % 7 == 0 {
                    *x = [f32::NAN, f32::INFINITY, -f32::INFINITY, 1e30][i % 4];
                }
            }
        }
        let out = render(&mut a, &input, &[256], 2);
        assert!(out.iter().flatten().all(|x| x.is_finite() && x.abs() < 1e4));
        let clean = signal(256 * 20, 2);
        let out = render(&mut a, &clean, &[256], 2);
        assert!(out.iter().flatten().all(|x| x.is_finite()));
        assert!(
            out[0][256 * 10..].iter().any(|x| x.abs() > 0.05),
            "audio is back"
        );
    }

    #[test]
    fn denormals_are_flushed_inside_process_only() {
        let (_e, mut a) = engine(AppState::new(false), 1, 2);
        let before = fpmode::current();
        let (i, mut o) = (vec![0.0f32; 256], vec![vec![0.0f32; 256]; 2]);
        let [o0, o1] = &mut o[..] else { unreachable!() };
        a.process(&[&i[..]], &mut [&mut o0[..], &mut o1[..]]);
        let inside = FP_IN_PROCESS.with(Cell::get).expect("probe ran");
        assert_eq!(inside & fpmode::BITS, fpmode::BITS);
        assert_eq!(fpmode::current(), before, "restored");
        if cfg!(any(target_arch = "x86_64", target_arch = "aarch64")) {
            assert_ne!(fpmode::BITS, 0);
            assert_ne!(before & fpmode::BITS, fpmode::BITS, "not set outside");
        }
    }

    fn popped(rx: &mut Consumer<AudioCmd>) -> Vec<String> {
        std::iter::from_fn(|| rx.pop().ok())
            .map(|c| match c {
                AudioCmd::Mix(mixer::Cmd::Globals(_)) => "globals".to_owned(),
                AudioCmd::Mix(mixer::Cmd::Delay(..)) => "delay".to_owned(),
                AudioCmd::Mix(mixer::Cmd::AddPeer(s, _)) => format!("add{s}"),
                AudioCmd::Mix(mixer::Cmd::RemovePeer(s)) => format!("remove{s}"),
                AudioCmd::Mix(mixer::Cmd::Peer(s, p)) => format!("peer{s}:{}", p.gain),
                AudioCmd::Mix(_) => "media".to_owned(),
                AudioCmd::Link(LinkCmd::Add(s, _)) => format!("link{s}"),
                AudioCmd::Link(LinkCmd::Remove(s)) => format!("unlink{s}"),
                AudioCmd::Link(LinkCmd::Sink(s, _)) => format!("sink{s}"),
                AudioCmd::Link(LinkCmd::Source(s, _)) => format!("source{s}"),
                #[cfg(feature = "modern")]
                AudioCmd::Modern(_) => "modern".to_owned(),
            })
            .collect()
    }

    /// Peer-triggered notifications cannot push recording or error ones out of a full queue.
    #[test]
    fn notification_queue_keeps_important_ones() {
        let mut h = host(cfg(1, 1), AppState::new(false));
        let low = || Notification::LatencyMatchRequested {
            from: "x".into(),
            latency_ms: 1.0,
        };
        let err = Notification::RecordingError { error: "e".into() };
        h.note(err.clone());
        for _ in 0..MAX_EVENTS * 2 {
            h.note(low());
        }
        assert_eq!(h.notes.len(), MAX_EVENTS);
        assert_eq!(h.notes[0], err);
        // full of important ones: a low one is dropped, an important one evicts the oldest
        for k in 0..MAX_EVENTS as u64 {
            h.note(Notification::RecordingDroppedFrames { frames: k });
        }
        h.note(low());
        assert!(!h.notes.iter().any(Notification::is_low_priority));
        assert_eq!(h.notes[0], Notification::RecordingDroppedFrames { frames: 0 });
        h.note(err.clone());
        assert_eq!(h.notes.back(), Some(&err));
        assert_eq!(h.notes[0], Notification::RecordingDroppedFrames { frames: 1 });
    }

    #[test]
    fn audio_commands_coalesce_and_stay_bounded() {
        let c = cfg(1, 2);
        let model = Model::new(c, AppState::new(false));
        let (tx, mut rx) = RingBuffer::new(CMD_RING);
        let (_gtx, grx) = RingBuffer::new(16);
        let mut atx = AudioTx::new(tx, grx);
        let peer = |gain: f32| {
            mixer::Cmd::Peer(
                3,
                Box::new(mixer::PeerParams {
                    gain,
                    ..Default::default()
                }),
            )
        };
        // a flood while the audio thread is away: the latest of each kind is all that waits
        for k in 0..10_000 {
            atx.mix(peer(k as f32));
            atx.mix(mixer::Cmd::Globals(model.globals()));
        }
        atx.flush();
        assert_eq!(popped(&mut rx), ["globals", "peer3:9999"]);
        // a peer leaving drops its pending parameters; a new peer in the slot comes first,
        // then its parameters; removal and re-add of another slot collapse to the add
        let shared = Arc::new(mixer::PeerShared::default());
        let strip = || mixer::PeerMix::new(&c, Box::default(), shared.clone());
        atx.mix(peer(1.0));
        atx.mix(mixer::Cmd::RemovePeer(3));
        atx.mix(mixer::Cmd::AddPeer(5, strip()));
        atx.mix(mixer::Cmd::RemovePeer(5));
        atx.mix(mixer::Cmd::AddPeer(5, strip()));
        atx.mix(mixer::Cmd::Peer(5, Box::default()));
        atx.flush();
        assert_eq!(popped(&mut rx), ["remove3", "add5", "peer5:0"]);
        // nothing new is handed over while the ring still holds the last batch
        atx.mix(peer(2.0));
        atx.flush();
        atx.mix(peer(3.0));
        atx.flush();
        assert_eq!(popped(&mut rx), ["peer3:2"]);
        atx.flush();
        assert_eq!(popped(&mut rx), ["peer3:3"]);
    }

    /// A flooding connection queues at most `MODERN_QUEUE_PER_PEER` datagrams (the rest is
    /// dropped and counted); another connection's datagrams still get in, and draining makes
    /// room again.
    #[cfg(feature = "modern")]
    #[test]
    fn modern_sink_queue_is_bounded_per_connection() {
        let (tx, rx) = std::sync::mpsc::channel();
        let drops = Arc::new(AtomicU64::new(0));
        let mut flood = modern_sink(1, tx.clone(), drops.clone());
        let mut other = modern_sink(2, tx, drops.clone());
        for _ in 0..3000 {
            flood(sonobus_net::Bytes::from_static(b"x"));
        }
        other(sonobus_net::Bytes::from_static(b"y"));
        assert_eq!(drops.load(Ordering::Relaxed), 3000 - MODERN_QUEUE_PER_PEER as u64);
        let got: Vec<Received> = rx.try_iter().collect();
        assert_eq!(got.len(), MODERN_QUEUE_PER_PEER + 1);
        assert_eq!(got.last().unwrap().0, 2, "the other connection got in");
        // the session thread takes one: room for one more
        got[0].2.fetch_sub(1, Ordering::Relaxed);
        flood(sonobus_net::Bytes::from_static(b"x"));
        flood(sonobus_net::Bytes::from_static(b"x"));
        assert_eq!(rx.try_iter().count(), 1);
        assert_eq!(drops.load(Ordering::Relaxed), 3001 - MODERN_QUEUE_PER_PEER as u64);
    }
}
