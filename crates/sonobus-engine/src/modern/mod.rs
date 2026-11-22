//! Protocol v2 sessions over `sonobus_net` (iroh/QUIC): hosting (mesh or broadcast), joining by
//! invite link, by a LAN-discovered session plus PIN/password, or by name plus password through
//! a rendezvous server. The engine owns a [`Net`] only while such a session is active.
//! A password/PIN/LAN/name joiner (or one with a mesh invite) learns from the host's WELCOME
//! whether it reached a broadcast hub, and is then a listener exactly as by a broadcast invite.
//!
//! Audio: one shared [`TxStream`] (stream [`MAIN_STREAM`], two encodings: main and low) carries
//! our send mix to every peer that subscribes: each block is encoded once per encoding and the
//! datagrams are cloned to the subscribers. A peer that needs different audio (patchbay routes,
//! a channel override, or a send format that is neither encoding) gets its own [`TxStream`]
//! ([`PRIVATE_STREAM`]). Every remote stream plays through an [`RxStream`] into the peer's mixer
//! slot. Per subscriber, [`Adapt`] turns the receiver's REPORTs into its encoding and redundancy
//! (the transport runs a fixed congestion window: this is the only back-off); the jitter
//! controller drives `RxStream::set_buffer_ms` from the stream's events.
//!
//! Stream conventions on top of the wire (engine to engine):
//! - FORMAT of stream 0 or 1 from a peer: that is the stream it sends us now; we play it and
//!   SUBSCRIBE `{stream, 0, 0}` (encoding [`UNSUBSCRIBE`] ends the subscription).
//! - QUALITY_REQ `preferred_format` = [`FORMAT_REQ`] | table index asks the sender for that
//!   send format ([`crate::formats::FORMATS`]); `0xff` clears the request.
//! - APP_OSC carries the `/sb/...` messages unchanged, plus [`EVICT`] (`/sbx/evict`, blob =
//!   endpoint id) from the session host after it removed a member.
//!
//! Every peer is identified by its authenticated connection: datagrams by the per-connection
//! sink, control frames by the connection they arrived on. No message body names a sender
//! (chat, latency and group suggestions carry the connection's name); everything a peer sends
//! goes through the same clamps as legacy (`proto::Proto::handle_sb`). A broadcast hub's
//! replies never reveal its other listeners. Nothing a peer sends makes the engine join
//! anything: suggestions are notifications; [`EVICT`] only disconnects, and only from the
//! endpoint whose key rotations we accept (`Net::authority`: the invite's host, or whoever a
//! password join learned the key from), which then blocks the evicted id (`Net::block`).
//! Eviction does not depend on the notice: the host removes the member with `Net::evict`,
//! whose REKEY names it, and every member blocks it in the transport before it installs or
//! passes on the new key (down any chain of password joins); `sonobus_net` then cuts off
//! every connection whose peer cannot prove the new key
//! ([`sonobus_net::rt::KEY_PROOF_TIMEOUT`]). The notice only makes the members that hear it
//! from their authority drop the member at once and list it as blocked.
//!
//! Identity is the endpoint id, never the display name: the peer cache and remembered solo
//! flags are keyed by it (`sbid:<hex id>`), chat can target ids
//! (`Engine::try_send_chat_to`), the hub answers latency requests by connection. A display
//! name already in use (by a peer or by us) gets the new endpoint's id fingerprint appended.
//!
//! Remote quality requests (QUALITY_REQ with [`FORMAT_REQ`]) are followed only while the
//! user honours them (`options::HONOUR_QUALITY_REQUESTS`), capped at "Max Send Quality",
//! at most one change per [`REQUEST_INTERVAL_MS`] and peer; a broadcast hub maps them to its
//! shared encodings, a mesh member gives at most [`MAX_PRIVATE_STREAMS`] peers their own
//! encoder.
//!
//! Congestion (`sonobus_net::adapt`): `k = 0` and one level down; a private single-Opus
//! stream walks a bit-rate ladder ([`ladder`]); a subscriber congested at the lowest level
//! of what it gets is moved to a private Opus stream at most half that rate per channel
//! (within [`MAX_PRIVATE_STREAMS`]), and back after [`FALLBACK_MIN_MS`] once it has room.
//! NACK resends pause while congested and are budgeted per subscriber (`TxStream`).
//!
//! Engine API (`Engine::host_session`, `join_invite`, `join_lan`, `join_by_name`,
//! `leave_session`, `invite_link`, `rotate_session_key`, `session_status`,
//! `poll_session_event`, `lan_browse` / `lan_sessions`, `identity_key` /
//! `set_identity_key`, ...). The app-facing types the legacy API already has are kept as they
//! are (other crates build `EngineConfig` and `Snapshot` and match `Event` / `Notification`
//! exhaustively), so the v2 additions live beside them:
//! - the device identity (endpoint secret key) is `Engine::identity_key`, generated at engine
//!   start; the app persists it and restores it with `Engine::set_identity_key`;
//! - session-level state is `Engine::session_status` ([`SessionStatus`]: role, invite link,
//!   PIN, addresses, relay), session-level notifications a separate queue
//!   (`Engine::poll_session_event`, [`SessionEvent`]); per peer, `PeerStatus::net`;
//! - peers still raise the ordinary `Event::PeerJoined` / `PeerLeft` / `PeerChanged` / `Chat`.
//!
//! Synchronised playout (broadcast only, [`HostOptions::playout_ms`] /
//! `Engine::set_playout_sync`): every listener plays each host sample at the same moment,
//! the host's capture time plus the host's delay. The host sends PLAYOUT on the control stream
//! (its stream's block index ↔ its clock, and the delay) once per [`PLAYOUT_INTERVAL_MS`]; a
//! listener that got one estimates its clock offset to the host from PING/PONG datagrams
//! (NTP-style, the sample with the smallest round trip of the last [`CLOCK_SAMPLES`]; an
//! asymmetric path skews it by half the asymmetry) and gives its receive stream the due time
//! minus its own output latency (one block: the device's is not known) to steer to
//! (`RxStream::set_playout`). A listener whose jitter buffer needs more than the delay plays
//! late, as without sync; one without PLAYOUT (an older host, sync off) is unchanged.
//! Latency Match / Receive Sync still set the listener's jitter buffer: a buffer above what the
//! delay leaves makes it late, below it changes nothing audible.
//! Whose PLAYOUT counts (`Session::playout_source`): only a broadcast listener's, and only
//! from the host it joined (its key authority: the invite's host, or the hub a password, PIN,
//! LAN or name join reached, whose WELCOME made us a listener). Mesh members ignore PLAYOUT.
//! Re-checked every tick; when it stops counting, sync is off and the clock samples are
//! dropped. The offset is used once [`CLOCK_SAMPLES`] samples at most [`CLOCK_MAX_AGE_MS`] old
//! are in. A PLAYOUT only changes how we play its sender's own stream, by at most
//! [`MAX_PLAYOUT_MS`], and makes us ping that sender; nobody else's audio.
//!
//! Not in modern sessions: the measured latency test (`Engine::start_latency_test` returns
//! false; the latency estimate from RTT, buffers and codec is shown instead), legacy IP block
//! lists (blocking is by endpoint id, for the session: `Engine::blocked_endpoints`), the
//! dynamic-resampling toggle on the send side (fixed at stream creation; the receive side
//! follows it). PR #202 packet redundancy becomes a floor for the adaptation's redundant
//! blocks per datagram, except on a congested path.

mod links;

use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use aoo::codec::PcmBitDepth;
use sonobus_core::sbmsg::{self, SbMessage};
use sonobus_net::adapt::{Adapt, Decision};
use sonobus_net::auth::{Id, Invite};
use sonobus_net::lan::{Advert, Lan};
use sonobus_net::rt::{CloseReason, PeerInfo};
use sonobus_net::stream::{
    CODEC_OPUS, Device, Encoding, RxEvent, RxOptions, RxStream, StreamFormat, TxOptions, TxStream,
};
use sonobus_net::wire::{
    Datagram, Frame, KIND_AUDIO, Ping, Playout, Pong, QualityReq, Role, Subscribe,
};
use sonobus_net::{Bytes, ConnId, Event as NetEvent, Net, PathInfo, PeerTx};

pub(crate) use links::{LinkCmd, Links};
pub use sonobus_net::lan::{AuthKind, DiscoveredSession};
pub use sonobus_net::{PeerAddr, RelayChoice, SessionMode};

use crate::formats::{FORMATS, SendCodec};
use crate::jitter::Change;
use crate::mixer::{AudioConfig, MAX_CHANNELS};
use crate::proto::{Common, PeerCore, Proto};
use crate::{AudioCmd, Connection, Event, Host, PeerId, PeerNet, PeerRole, lock, options};

/// Our shared send stream.
pub const MAIN_STREAM: u8 = 0;
/// A peer's private send stream.
pub const PRIVATE_STREAM: u8 = 1;
/// SUBSCRIBE encoding that ends a subscription.
pub const UNSUBSCRIBE: u8 = 0xff;
/// QUALITY_REQ `preferred_format` flag: the low bits are a send format table index.
pub const FORMAT_REQ: u8 = 0x80;
/// APP_OSC address of the host's eviction notice.
pub const EVICT: &str = "/sbx/evict";
/// Ping estimate, `/sb/pinfo` and jitter controller cadence (the legacy `/sb/ping` interval).
const PING_INTERVAL_MS: f64 = 2000.0;
/// RTT samples for the adaptation.
const RTT_INTERVAL_MS: f64 = 100.0;
/// How often our address (relay URL) is checked for invite and LAN updates.
const ADDR_POLL_MS: f64 = 250.0;
/// Opus frame sizes at 48 kHz, largest first.
const OPUS_FRAMES: [u16; 4] = [960, 480, 240, 120];
/// Peers with their own encoder at most, besides the shared stream (mesh requests and
/// congestion fallbacks; the local user's own per-peer choices are not limited).
pub const MAX_PRIVATE_STREAMS: usize = 4;
/// A peer's quality requests change our format at most this often.
pub const REQUEST_INTERVAL_MS: f64 = 1000.0;
/// A congestion fallback stays at least this long, then ends once the path has room.
pub const FALLBACK_MIN_MS: f64 = 60_000.0;
/// The datagram queue towards a peer counts as depleted below this many free bytes.
const SEND_BUFFER_LOW: usize = sonobus_net::rt::DATAGRAM_SEND_BUFFER / 4;
/// Lowest rung of a bit-rate ladder, per channel.
const LADDER_FLOOR: u32 = 8000;
/// Playout sync: a sensible delay for a host to turn it on with, and the largest accepted.
pub const DEFAULT_PLAYOUT_MS: u16 = 150;
pub const MAX_PLAYOUT_MS: u16 = 1000;
/// The host repeats PLAYOUT this often.
const PLAYOUT_INTERVAL_MS: f64 = 1000.0;
/// A synced listener pings the host this often; the first [`SYNC_PINGS_FAST`] go every
/// [`SYNC_PING_FIRST_MS`] (until it has [`CLOCK_SAMPLES`]), so that an answer within
/// [`MAX_PING_RTT_US`] always finds its PING still outstanding ([`PINGS_OUTSTANDING`]).
const SYNC_PING_MS: f64 = 1000.0;
const SYNC_PING_FIRST_MS: f64 = 200.0;
const SYNC_PINGS_FAST: u32 = 16;
const PINGS_OUTSTANDING: usize = 16;
const CLOCK_SAMPLES: usize = 8;
/// Clock samples older than this (ms) no longer count (pinging paused, sync was off).
const CLOCK_MAX_AGE_MS: f64 = 15_000.0;
/// PONGs with a longer round trip, or held longer by the host, are not clock samples (µs).
const MAX_PING_RTT_US: i128 = 2_000_000;
const MAX_PONG_HOLD_US: i128 = 1_000_000;
/// [`SessionEvent::Ended`] reasons.
pub const END_REMOVED: &str = "removed";
pub const END_BLOCKED: &str = "blocked";
pub const END_HOST_LEFT: &str = "the host left";
pub const END_FULL: &str = "session full";

/// Where the session's endpoint binds and which relays it uses.
#[derive(Debug, Clone)]
pub struct NetOptions {
    /// Our display name (self-declared, as peers see it).
    pub name: String,
    pub bind_ip: IpAddr,
    /// UDP port; taken → an ephemeral one. 0 = ephemeral.
    pub port: u16,
    /// The shipped app's default is n0's public relays ([`RelayChoice::N0Default`]).
    pub relay: RelayChoice,
}

impl NetOptions {
    /// All interfaces, port 10999, n0's relays.
    pub fn new(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            bind_ip: std::net::Ipv4Addr::UNSPECIFIED.into(),
            port: sonobus_net::rt::DEFAULT_PORT,
            relay: RelayChoice::N0Default,
        }
    }
}

/// Where a hosted session is advertised for LAN discovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LanScope {
    Off,
    /// mDNS on every interface.
    Network,
    /// mDNS on IPv4 loopback only (this machine; tests).
    Loopback,
}

#[derive(Debug, Clone)]
pub struct HostOptions {
    pub session: String,
    pub mode: SessionMode,
    /// Invite seed (session key = HKDF(seed)); generated when `None`.
    pub seed: Option<[u8; 16]>,
    /// Accept password joins (LAN, rendezvous) with this password.
    pub password: Option<String>,
    /// Accept PIN joins with a random 6-digit PIN ([`SessionStatus::pin`]); replaces `password`.
    pub pin: bool,
    pub lan: LanScope,
    /// Serve name + password joins (rendezvous) on our own endpoint.
    pub rendezvous: bool,
    /// Broadcast: listeners admitted at most (capped at the mixer's peer slots).
    pub max_listeners: Option<usize>,
    /// Stay registered under the session name at this rendezvous server.
    pub register_at: Option<PeerAddr>,
    /// Broadcast: synchronised playout across listeners with this delay, ms (at most
    /// [`MAX_PLAYOUT_MS`]; [`DEFAULT_PLAYOUT_MS`] suits most links). `None`: off.
    pub playout_ms: Option<u16>,
}

impl HostOptions {
    pub fn new(session: &str) -> Self {
        Self {
            session: session.to_owned(),
            mode: SessionMode::Mesh,
            seed: None,
            password: None,
            pin: false,
            lan: LanScope::Off,
            rendezvous: false,
            max_listeners: None,
            register_at: None,
            playout_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionRole {
    Host,
    Member,
    /// Broadcast listener: receives the host only, sends nothing. By a broadcast invite, or by
    /// a password, PIN, LAN or name join (or a mesh invite) that reached a broadcast hub (until
    /// its WELCOME says so, such a joiner shows as [`SessionRole::Member`] of a mesh).
    Listener,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SessionStatus {
    pub session: String,
    pub mode: SessionMode,
    pub role: SessionRole,
    /// Our endpoint id (derived from the identity key).
    pub endpoint_id: [u8; 32],
    pub addrs: Vec<SocketAddr>,
    /// Our home relay, once the relay connection is up.
    pub relay: Option<String>,
    /// Host: the invite link (URL form), refreshed when the relay URL becomes known or the key
    /// is rotated.
    pub invite: Option<String>,
    /// Host in PIN mode: the PIN to show.
    pub pin: Option<String>,
    /// Host serving rendezvous: where name + password joiners connect.
    pub rendezvous: Option<PeerAddr>,
    pub lan_advertised: bool,
    /// Failed or unfinished password/PIN joins on our session so far.
    pub failed_joins: u32,
    pub peers: usize,
}

/// Modern-session notifications, polled with `Engine::poll_session_event`.
#[derive(Debug, Clone, PartialEq)]
pub enum SessionEvent {
    /// Our join attempt failed (`"authentication failed"`, unreachable, ...); `retry_after` when
    /// the host throttled us.
    JoinFailed {
        reason: String,
        retry_after: Option<Duration>,
    },
    /// The broadcast host is at its listener cap.
    SessionFull,
    /// Host: the invite link changed (relay URL now known, key rotated).
    InviteChanged(String),
    PathChanged {
        id: PeerId,
        direct: bool,
        relay: Option<String>,
        rtt_ms: f32,
    },
    /// A password/PIN join on our session failed; failed (or unfinished) attempts so far.
    PasswordJoinFailed { failed_total: u32 },
    /// Incoming connections refused or failed since the last report.
    IncomingFailed { count: u32, last_reason: String },
    /// PIN mode: too many wrong guesses; set a new PIN (`Engine::new_session_pin`).
    PinInvalidated,
    /// Password joins refused for a while, from `ip` or (None) from everyone.
    Throttled {
        ip: Option<IpAddr>,
        retry_after: Duration,
    },
    RendezvousFailed {
        reason: String,
        retry_after: Duration,
    },
    /// We hold a new session key (rotated by the host, or learned by a password join).
    SessionKeyChanged,
    /// The session is over for us: [`END_REMOVED`] (the host removed us, or we could not
    /// prove the rotated key), [`END_BLOCKED`] (the host blocked us), [`END_HOST_LEFT`]
    /// (a broadcast host left), [`END_FULL`] (the broadcast host is full); also when the last
    /// peer removes or blocks us.
    Ended { reason: String },
}

/// Send format table entry `fi` as a stream for `nch` channels on `cfg`, with the `low`
/// entry as second encoding. Opus: 48 kHz, coupled (stereo pairs) with the table's per-channel
/// bitrate times the channels, the largest frame not above the table's block (at least 2.5 ms);
/// PCM: device rate and block.
pub(crate) fn stream_format(
    fi: usize,
    low: Option<usize>,
    nch: usize,
    cfg: &AudioConfig,
) -> StreamFormat {
    let nch = nch.clamp(1, MAX_CHANNELS) as u8;
    let enc = |i: usize| match FORMATS[i].codec {
        SendCodec::Opus {
            bitrate_per_channel,
        } => Encoding::Opus {
            bitrate: bitrate_per_channel as u32 * u32::from(nch),
            complexity: 10,
        },
        SendCodec::Pcm { bytes } => Encoding::Pcm(match bytes {
            3 => PcmBitDepth::Int24,
            4 => PcmBitDepth::Float32,
            _ => PcmBitDepth::Int16,
        }),
    };
    let encodings: Vec<Encoding> = std::iter::once(fi).chain(low).map(enc).collect();
    let f = &FORMATS[fi];
    let opus = |encodings| {
        let want = cfg.block_size.max(f.min_blocksize as usize);
        StreamFormat {
            sample_rate: 48000,
            block_size: OPUS_FRAMES
                .into_iter()
                .find(|&b| usize::from(b) <= want)
                .unwrap_or(120),
            channels: nch,
            voice: false,
            encodings,
        }
    };
    if matches!(f.codec, SendCodec::Opus { .. }) {
        return opus(encodings);
    }
    let mut block = cfg.block_size.clamp(16, 8192) as u16;
    loop {
        let sf = StreamFormat {
            sample_rate: cfg.sample_rate,
            block_size: block,
            channels: nch,
            voice: false,
            encodings: encodings.clone(),
        };
        if sf.validate().is_ok() {
            return sf;
        }
        if block <= 32 {
            // a device rate PCM cannot carry: the default Opus format instead
            return stream_format(crate::formats::DEFAULT_FORMAT, None, nch.into(), cfg);
        }
        block /= 2;
    }
}

/// The second (low) encoding offered next to `fi` on the shared stream.
pub(crate) fn low_of(fi: usize) -> Option<usize> {
    match FORMATS[fi].codec {
        SendCodec::Opus { .. } => Some(fi.saturating_sub(2)).filter(|&l| l != fi),
        SendCodec::Pcm { bytes } if bytes > 2 => {
            crate::formats::find(SendCodec::Pcm { bytes: 2 })
        }
        SendCodec::Pcm { .. } => None,
    }
}

/// The table entry a received stream's encoding `e` corresponds to.
fn format_of(sf: &StreamFormat, e: usize) -> Option<usize> {
    crate::formats::find(match *sf.encodings.get(e)? {
        Encoding::Opus { bitrate, .. } => SendCodec::Opus {
            bitrate_per_channel: (bitrate / u32::from(sf.channels.max(1))) as i32,
        },
        Encoding::Pcm(d) => SendCodec::Pcm {
            bytes: d.sample_size() as i32,
        },
    })
}

/// Bit rates of a stream's encodings, best first (the adaptation's levels).
fn levels(sf: &StreamFormat) -> Vec<u32> {
    sf.encodings
        .iter()
        .map(|e| match *e {
            Encoding::Opus { bitrate, .. } => bitrate,
            Encoding::Pcm(d) => {
                (d.sample_size() as u32 * 8)
                    .saturating_mul(sf.sample_rate)
                    .saturating_mul(u32::from(sf.channels))
            }
        })
        .collect()
}

fn hex(id: &Id) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// A single-Opus-encoding stream's bit-rate ladder from `bitrate` (all channels) down to
/// a quarter of it, not below [`LADDER_FLOOR`] per channel.
pub(crate) fn ladder(bitrate: u32, nch: usize) -> Vec<u32> {
    let floor = LADDER_FLOOR * nch.max(1) as u32;
    let mut v: Vec<u32> = [8, 6, 4, 3, 2]
        .iter()
        .map(|&k| (u64::from(bitrate) * k / 8) as u32)
        .filter(|&b| b >= floor)
        .collect();
    v.dedup();
    if v.is_empty() {
        v.push(bitrate);
    }
    v
}

/// The Opus table entry a subscriber congested at `rate` bit/s over `nch` channels falls
/// back to: the best one at most half that rate per channel.
fn fallback_format(rate: u32, nch: usize) -> Option<usize> {
    let half = rate / 2 / nch.max(1) as u32;
    (0..FORMATS.len()).rev().find(|&i| {
        matches!(FORMATS[i].codec, SendCodec::Opus { bitrate_per_channel } if bitrate_per_channel as u32 <= half)
    })
}

/// The entry of `formats` nearest to `want` in the table (bit-rate order), the cheaper on a
/// tie.
fn nearest(formats: &[usize], want: usize) -> usize {
    formats
        .iter()
        .copied()
        .min_by_key(|&f| (f.abs_diff(want), f))
        .unwrap_or(want)
}

/// One of our send streams; locked on its own so the send thread encodes without `Core`.
struct Tx {
    tx: Arc<Mutex<TxStream>>,
    /// Table entries of its encodings (0 main, 1 low).
    formats: Vec<usize>,
    nch: usize,
}

impl Tx {
    fn new(
        stream: u8,
        fi: usize,
        low: Option<usize>,
        nch: usize,
        cfg: &AudioConfig,
        dynres: bool,
    ) -> (Self, sonobus_net::stream::TxAudio) {
        let opts = TxOptions {
            dynamic_resampling: dynres,
            ..TxOptions::default()
        };
        let sf = stream_format(fi, low, nch, cfg);
        let (tx, audio) = TxStream::new(stream, device(cfg, nch), sf, opts)
            .or_else(|_| {
                let sf = stream_format(crate::formats::DEFAULT_FORMAT, None, nch, cfg);
                TxStream::new(stream, device(cfg, nch), sf, opts)
            })
            .expect("the default format is valid");
        let formats = std::iter::once(fi).chain(low).collect();
        (
            Self {
                tx: Arc::new(Mutex::new(tx)),
                formats,
                nch,
            },
            audio,
        )
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TxStream> {
        lock(&self.tx)
    }
}

fn device(cfg: &AudioConfig, nch: usize) -> Device {
    Device {
        sample_rate: cfg.sample_rate,
        block_size: cfg.block_size,
        channels: nch.clamp(1, MAX_CHANNELS),
    }
}

/// A peer's subscription to one of our streams.
struct Sub {
    stream: u8,
    adapt: Adapt,
    decision: Decision,
    /// The adaptation's levels (bit/s, best first).
    levels: Vec<u32>,
    /// The levels are a bit-rate ladder of encoding 0 (a private single-Opus stream).
    ladder: bool,
}

impl Sub {
    /// Still a subscription to `stream` with these levels (no format change since).
    fn fits(&self, stream: u8, levels: &[u32]) -> bool {
        self.stream == stream && self.levels == levels
    }

    /// The redundancy sent: the adaptation's, raised to our `floor` except on a congested
    /// path, then held within the receiver's bit-rate cap.
    fn k(&self, floor: u8) -> u8 {
        if self.adapt.congested() {
            return 0;
        }
        let d = self.decision;
        d.redundancy.max(floor).min(self.adapt.max_redundancy(d.level))
    }
}

/// A listener's clock offset to its host (host minus ours, µs) from PING/PONG: the sample with
/// the smallest round trip of the last [`CLOCK_SAMPLES`].
#[derive(Debug, Default)]
struct Clock {
    /// `(round trip µs, offset µs, when: the engine's ms)`.
    samples: [(u64, i64, f64); CLOCK_SAMPLES],
    n: usize,
    ping_id: u16,
    last_ping_ms: f64,
    /// Our PINGs still unanswered: `(id, t1)`.
    sent: [Option<(u16, u64)>; PINGS_OUTSTANDING],
    /// PINGs sent so far.
    pings: u32,
    /// When the last refill burst of fast PINGs began (the engine's ms).
    burst_ms: Option<f64>,
}

impl Clock {
    /// PINGs go fast while the window is short of samples: the first [`SYNC_PINGS_FAST`], and
    /// later (lost PONGs let samples age out) a refill burst as long as those take, at most one
    /// per [`CLOCK_MAX_AGE_MS`] and only while answers come (an answer within the burst's
    /// length). So a host that does not answer, or answers too little to fill the window, is
    /// pinged at most about twice a second on average.
    fn fast(&self, ms: f64) -> bool {
        let burst = f64::from(SYNC_PINGS_FAST) * SYNC_PING_FIRST_MS;
        self.fresh(ms).count() < CLOCK_SAMPLES
            && (self.pings < SYNC_PINGS_FAST
                || self.fresh(ms).any(|x| ms - x.2 < burst)
                    && self
                        .burst_ms
                        .is_none_or(|b| ms - b < burst || ms - b >= CLOCK_MAX_AGE_MS))
    }

    /// Time (`ms`, the engine's clock) for the next PING ([`Self::fast`] or one a second).
    fn ping_due(&self, ms: f64) -> bool {
        let every = if self.fast(ms) { SYNC_PING_FIRST_MS } else { SYNC_PING_MS };
        ms - self.last_ping_ms >= every
    }

    /// The next PING, sent at `t1` (our clock) and `ms` (the engine's).
    fn ping(&mut self, t1: u64, ms: f64) -> Ping {
        let new_burst = self.burst_ms.is_none_or(|b| ms - b >= CLOCK_MAX_AGE_MS);
        if self.pings >= SYNC_PINGS_FAST && new_burst && self.fast(ms) {
            self.burst_ms = Some(ms);
        }
        self.last_ping_ms = ms;
        self.pings = self.pings.saturating_add(1);
        self.ping_id = self.ping_id.wrapping_add(1);
        self.sent[usize::from(self.ping_id) % self.sent.len()] = Some((self.ping_id, t1));
        Ping {
            id: self.ping_id,
            t1,
        }
    }

    /// A PONG received at `t4` (our clock), `ms` (the engine's). Only the answer to an
    /// outstanding PING of ours (once), with a plausible round trip and hold time, counts.
    fn on_pong(&mut self, p: &Pong, t4: u64, ms: f64) {
        let Some(slot) = self.sent.iter_mut().find(|s| **s == Some((p.id, p.t1))) else {
            return;
        };
        *slot = None;
        let (t1, t2, t3, t4) = (p.t1 as i128, p.t2 as i128, p.t3 as i128, t4 as i128);
        let rtt = (t4 - t1) - (t3 - t2);
        if t1 > t4 || t2 > t3 || !(0..=MAX_PING_RTT_US).contains(&rtt) || t3 - t2 > MAX_PONG_HOLD_US
        {
            return;
        }
        let off = ((t2 - t1) + (t3 - t4)) / 2;
        let off = off.clamp(i64::MIN.into(), i64::MAX.into()) as i64;
        self.samples[self.n % CLOCK_SAMPLES] = (rtt as u64, off, ms);
        self.n += 1;
    }

    /// Samples at most [`CLOCK_MAX_AGE_MS`] old at `ms`.
    fn fresh(&self, ms: f64) -> impl Iterator<Item = &(u64, i64, f64)> {
        let s = &self.samples[..self.n.min(CLOCK_SAMPLES)];
        s.iter().filter(move |x| ms - x.2 <= CLOCK_MAX_AGE_MS)
    }

    /// The offset once a whole window of [`CLOCK_SAMPLES`] fresh samples is in: acting on the
    /// first few (one may have queued both ways) and correcting later would be heard.
    fn offset(&self, ms: f64) -> Option<i64> {
        if self.fresh(ms).count() < CLOCK_SAMPLES {
            return None;
        }
        self.fresh(ms).min_by_key(|x| x.0).map(|x| x.1)
    }
}

/// When a host sample captured at `time_us` (host clock) is due at our output: on our clock
/// (`us = time since epoch + skew`, host = ours + `off`), plus `delay`, less our output
/// latency. `None` when out of range (a hostile or broken host: no target).
fn due_at(
    epoch: Instant,
    skew: u64,
    time_us: u64,
    off: i64,
    delay: Duration,
    out_latency: Duration,
) -> Option<Instant> {
    let since_epoch = i128::from(time_us) - i128::from(off) - i128::from(skew);
    let d = Duration::from_micros(u64::try_from(since_epoch.unsigned_abs()).ok()?);
    let t = if since_epoch >= 0 {
        epoch.checked_add(d)?
    } else {
        epoch.checked_sub(d)?
    };
    t.checked_add(delay)?.checked_sub(out_latency)
}

/// The PLAYOUT a broadcast host sends for `stream` with sync at `delay_ms` (`None`: off) and
/// the stream's anchor `(generation, seq, time_us)`: on needs the anchor, off does not.
fn playout_frame(
    delay_ms: Option<u16>,
    stream: u8,
    anchor: Option<(u8, u32, u64)>,
) -> Option<Playout> {
    let off = Playout {
        stream,
        generation: 0,
        seq: 0,
        time_us: 0,
        delay_ms: 0,
    };
    match (delay_ms, anchor) {
        (None, _) => Some(off),
        (Some(delay_ms), Some((generation, seq, time_us))) => Some(Playout {
            generation,
            seq,
            time_us,
            delay_ms,
            ..off
        }),
        (Some(_), None) => None,
    }
}

/// A host's PLAYOUT as we act on it: the delay capped at [`MAX_PLAYOUT_MS`]; `None` = off.
fn accept_playout(pl: Playout) -> Option<Playout> {
    let delay_ms = pl.delay_ms.min(MAX_PLAYOUT_MS);
    (delay_ms > 0).then_some(Playout { delay_ms, ..pl })
}

/// A shared encoder's Opus expected loss from its subscribers': the lower median, so one
/// receiver's bad path does not set it for everyone.
fn shared_loss(mut v: Vec<u8>) -> u8 {
    v.sort_unstable();
    v.get(v.len().saturating_sub(1) / 2).copied().unwrap_or(0)
}

struct Rx {
    rx: RxStream,
    stream: u8,
    format: Option<StreamFormat>,
}

pub(crate) struct Peer {
    pub c: PeerCore,
    conn: ConnId,
    info: PeerInfo,
    tx: PeerTx,
    rx: Option<Rx>,
    sub: Option<Sub>,
    private: Option<Tx>,
    /// The receiver's QUALITY_REQ bit-rate cap.
    max_bitrate: u32,
    /// Its latest format request, waiting for [`REQUEST_INTERVAL_MS`], and the last change.
    pending_req: Option<usize>,
    req_ms: f64,
    /// Congestion fallback: a private Opus stream in this format, since when.
    fallback: Option<usize>,
    fallback_ms: f64,
    path: Option<PathInfo>,
    playing: bool,
    last_rtt_ms: f64,
    last_ping_ms: f64,
    /// Listener: the host's latest PLAYOUT (sync on) and our clock offset to it.
    playout: Option<Playout>,
    clock: Clock,
}

impl Peer {
    fn send_control(&self, net: &Net, f: Frame) {
        let _ = net.send_control(self.conn, f);
    }
}

pub(crate) struct Session {
    pub c: Common,
    pub peers: Vec<Peer>,
    net: Net,
    /// Set when the session ends: its network thread stops.
    pub quit: Arc<AtomicBool>,
    pub thread: Option<JoinHandle<()>>,
    mode: SessionMode,
    pub role: SessionRole,
    session: String,
    /// Host: the invite seed.
    seed: Option<[u8; 16]>,
    invite: Option<String>,
    pin: Option<String>,
    lan: Option<(Lan, AuthKind)>,
    rendezvous: bool,
    /// Our own join dials (their failures are reported).
    dials: Vec<ConnId>,
    main: Option<Tx>,
    blocked: Vec<Id>,
    failed_joins: u32,
    /// PR #202 packet redundancy as a floor for every subscriber's `k`.
    red_floor: u8,
    last_addr_ms: f64,
    last_relay: Option<String>,
    last_blocks: u64,
    /// Test hook: drop this many of every 100 received AUDIO datagrams.
    pub loss_pct: u32,
    loss_seed: u32,
    /// Received datagrams dropped because their connection's queue was full.
    pub dropped: Arc<AtomicU64>,
    /// Time base of PING/PONG and PLAYOUT times.
    epoch: Instant,
    /// Test hook: our PING/PONG/PLAYOUT clock reads this much ahead (µs).
    pub clock_skew_us: u64,
    /// Broadcast host: the playout sync delay (ms); PLAYOUT due to everyone (a change).
    playout_ms: Option<u16>,
    playout_due: bool,
    last_playout_ms: f64,
    /// Test hook: hold received AUDIO datagrams this long.
    pub rx_delay: Duration,
    delayed: std::collections::VecDeque<(Instant, ConnId, Bytes)>,
}

/// How a session starts.
pub(crate) enum Start {
    Host(HostOptions),
    Invite(Invite),
    Lan(DiscoveredSession, String),
    Name(PeerAddr, String, String),
}

impl Session {
    /// The `sonobus_net` configuration for `start`.
    pub fn config(
        net: &NetOptions,
        start: &Start,
        secret_key: [u8; 32],
    ) -> Result<sonobus_net::Config, String> {
        let mut c = sonobus_net::Config {
            bind_ip: net.bind_ip,
            port: net.port,
            secret_key: Some(secret_key),
            name: crate::proto::clean_name(&net.name),
            relay: net.relay.clone(),
            ..sonobus_net::Config::default()
        };
        match start {
            Start::Host(o) => {
                c.session = o.session.clone();
                c.role = Role::Host;
                c.mode = o.mode;
                let seed = o.seed.unwrap_or_else(sonobus_net::auth::random_seed);
                c.key = Some(sonobus_net::auth::key_from_seed(&seed));
                c.password = o.password.clone().filter(|_| !o.pin);
                c.max_listeners = o.max_listeners.map(|n| n.min(crate::mixer::MAX_PEERS));
                if o.mode == SessionMode::Broadcast && c.max_listeners.is_none() {
                    c.max_listeners = Some(crate::mixer::MAX_PEERS);
                }
            }
            Start::Invite(inv) => {
                c.session = inv.session.clone();
                c.key = Some(inv.key());
                c.host = Some(inv.host);
                c.mode = inv.mode;
                if inv.mode == SessionMode::Broadcast {
                    c.role = Role::Listener;
                }
            }
            Start::Lan(s, _) => c.session = s.session.clone(),
            Start::Name(_, name, _) => c.session = name.clone(),
        }
        Ok(c)
    }

    /// The session around a started `net`; joins are dialed here.
    pub fn new(h: &mut Host, net: Net, start: Start, opts: &NetOptions) -> Result<Self, String> {
        let cfg = h.cfg;
        let (mut seed, mut mode, mut role) = (None, SessionMode::Mesh, SessionRole::Member);
        let session: String;
        let (mut pin, mut lan, mut rendezvous, mut dials) = (None, None, false, Vec::new());
        let mut playout_ms = None;
        match start {
            Start::Host(o) => {
                // the key `config` derived must come from this seed: regenerate both here
                seed = o.seed;
                mode = o.mode;
                playout_ms = o.playout_ms;
                role = SessionRole::Host;
                session = o.session.clone();
                if o.pin {
                    pin = Some(net.enable_pin());
                }
                if o.rendezvous {
                    net.serve_rendezvous(sonobus_net::rdv::Limits::default());
                    rendezvous = true;
                }
                let auth = if o.pin {
                    Some(AuthKind::Pin)
                } else {
                    o.password.as_ref().map(|_| AuthKind::Password)
                };
                if let Some(auth) = auth.filter(|_| o.lan != LanScope::Off) {
                    let l = match o.lan {
                        LanScope::Loopback => Lan::loopback(),
                        _ => Lan::new(),
                    }
                    .map_err(|e| format!("LAN discovery: {e}"))?;
                    lan = Some((l, auth));
                }
            }
            Start::Invite(inv) => {
                mode = inv.mode;
                if mode == SessionMode::Broadcast {
                    role = SessionRole::Listener;
                }
                session = inv.session.clone();
                let to = PeerAddr {
                    id: inv.host,
                    addrs: inv.addrs.clone(),
                    relay: inv.relay.clone(),
                };
                dials.push(net.connect(to, sonobus_net::Join::Key));
            }
            Start::Lan(s, secret) => {
                session = s.session.clone();
                dials.push(net.join_discovered(&s, &secret));
            }
            Start::Name(server, name, password) => {
                session = name.clone();
                net.join_by_name(server, &name, &password)
                    .map_err(|e| e.to_string())?;
            }
        }
        let mut c = Common::new();
        c.user = crate::proto::clean_name(&opts.name);
        c.group = Some(session.clone());
        let main = (role != SessionRole::Listener).then(|| {
            let p = &h.model.state.params;
            let fi = crate::formats::checked(p.def_send_qual);
            let nch = main_channels(h);
            let (tx, audio) = Tx::new(MAIN_STREAM, fi, low_of(fi), nch, &cfg, p.dynamic_resampling);
            h.tx.send(AudioCmd::Modern(LinkCmd::Main(Some(Box::new(audio)))));
            tx
        });
        let mut s = Self {
            c,
            peers: Vec::new(),
            net,
            quit: Arc::new(AtomicBool::new(false)),
            thread: None,
            mode,
            role,
            session,
            seed,
            invite: None,
            pin,
            lan,
            rendezvous,
            dials,
            main,
            blocked: Vec::new(),
            failed_joins: 0,
            red_floor: red_floor(options::packet_redundancy(&h.model.state.extra)),
            last_addr_ms: f64::NEG_INFINITY,
            last_relay: None,
            last_blocks: 0,
            loss_pct: 0,
            loss_seed: 0x1234_5678,
            dropped: Arc::default(),
            epoch: Instant::now(),
            clock_skew_us: 0,
            playout_ms: None,
            playout_due: false,
            last_playout_ms: f64::NEG_INFINITY,
            rx_delay: Duration::ZERO,
            delayed: Default::default(),
        };
        s.set_playout_ms(playout_ms);
        s.refresh_addr(h, true);
        Ok(s)
    }

    pub fn connection(&self) -> Connection {
        if self.role == SessionRole::Host || !self.peers.is_empty() {
            Connection::Connected
        } else {
            Connection::Connecting
        }
    }

    pub fn status(&self) -> SessionStatus {
        let me = self.net.local_addr();
        SessionStatus {
            session: self.session.clone(),
            mode: self.mode,
            role: self.role,
            endpoint_id: me.id,
            addrs: me.addrs.clone(),
            relay: me.relay.clone(),
            invite: self.invite.clone(),
            pin: self.pin.clone(),
            rendezvous: self.rendezvous.then_some(me),
            lan_advertised: self.lan.is_some(),
            failed_joins: self.failed_joins,
            peers: self.peers.len(),
        }
    }

    pub fn invite(&self) -> Option<String> {
        self.invite.clone()
    }

    /// Broadcast host: playout sync with this delay (ms, capped at [`MAX_PLAYOUT_MS`]), or off;
    /// the listeners hear of it right away. Ignored elsewhere.
    pub fn set_playout_ms(&mut self, ms: Option<u16>) {
        if self.role != SessionRole::Host || self.mode != SessionMode::Broadcast {
            return;
        }
        let ms = ms.filter(|&m| m > 0).map(|m| m.min(MAX_PLAYOUT_MS));
        self.playout_due |= ms != self.playout_ms;
        self.playout_ms = ms;
    }

    /// Our PING/PONG/PLAYOUT clock: µs since [`Self::epoch`] (plus the test skew).
    fn us(&self, t: Instant) -> u64 {
        (t.saturating_duration_since(self.epoch).as_micros() as u64).saturating_add(self.clock_skew_us)
    }

    /// Broadcast host: PLAYOUT to every listener, for the stream it gets (no anchor yet:
    /// skipped until the next round). Turning sync off needs no anchor: everyone gets it.
    fn send_playout(&self) {
        for (i, p) in self.peers.iter().enumerate() {
            let (stream, t) = match &p.private {
                Some(t) => (PRIVATE_STREAM, Some(t)),
                None => (MAIN_STREAM, self.main.as_ref()),
            };
            let anchor = t
                .and_then(|t| t.lock().playout_anchor())
                .map(|(g, seq, at)| (g, seq, self.us(at)));
            if let Some(f) = playout_frame(self.playout_ms, stream, anchor) {
                self.peers[i].send_control(&self.net, Frame::Playout(f));
            }
        }
    }

    /// Peer `i` is our broadcast host, so its PLAYOUT counts: we are a broadcast listener
    /// and it is the host we joined, our key authority (the host our invite named, or the hub
    /// our password join reached). Re-checked every tick. It steers only how we play its own
    /// stream.
    fn playout_source(&self, i: usize) -> bool {
        self.role == SessionRole::Listener && self.net.authority() == Some(self.peers[i].info.id)
    }

    /// Listener: ping the host while it asks for sync, and steer the receive stream to its
    /// target once the clock offset is known.
    fn sync_playout(&mut self, h: &Host, i: usize, now: Instant) {
        let ms = h.now_ms();
        let out_latency = Duration::from_secs_f32(h.cfg.block_ms() * 1e-3);
        let t1 = self.us(now);
        let (epoch, skew) = (self.epoch, self.clock_skew_us);
        let source = self.playout_source(i);
        let p = &mut self.peers[i];
        if !source && p.playout.is_some() {
            // no longer our host's: as if it turned sync off
            p.playout = None;
            p.clock = Clock::default();
        }
        let Some(pl) = p.playout else {
            if let Some(r) = &mut p.rx {
                r.rx.set_playout(None);
            }
            return;
        };
        if p.clock.ping_due(ms) {
            let mut b = Vec::new();
            p.clock.ping(t1, ms).encode(&mut b);
            p.c.bytes_sent += b.len() as u64;
            let _ = p.tx.send(b.into());
        }
        let (Some(off), Some(r)) = (p.clock.offset(ms), &mut p.rx) else {
            return;
        };
        if r.stream != pl.stream {
            r.rx.set_playout(None);
            return;
        }
        let delay = Duration::from_millis(pl.delay_ms.into());
        let due = due_at(epoch, skew, pl.time_us, off, delay, out_latency);
        r.rx.set_playout(due.map(|due| sonobus_net::stream::Playout {
            generation: pl.generation,
            seq: pl.seq,
            due,
            span: delay,
        }));
    }

    /// Our address changed (relay URL now known): new invite link, new LAN advertisement.
    fn refresh_addr(&mut self, h: &mut Host, force: bool) {
        let me = self.net.local_addr();
        if !force && me.relay == self.last_relay {
            return;
        }
        self.last_relay = me.relay.clone();
        if self.role == SessionRole::Host {
            if self.seed.is_none() {
                return;
            }
            let inv = Invite {
                seed: self.seed.unwrap_or_default(),
                host: me.id,
                addrs: me.addrs.clone(),
                relay: me.relay.clone(),
                session: self.session.clone(),
                mode: self.mode,
            };
            if let Ok(url) = inv.to_url()
                && self.invite.as_ref() != Some(&url)
            {
                self.invite = Some(url.clone());
                h.session_event(SessionEvent::InviteChanged(url));
            }
            if let Some((lan, auth)) = &self.lan {
                let ad = Advert {
                    session: self.session.clone(),
                    auth: *auth,
                };
                if let Err(e) = lan.advertise(&me, &ad) {
                    h.event(Event::Error(format!("LAN advertisement: {e}")));
                }
            }
        }
    }

    /// Host: a new random PIN (PIN mode), e.g. after [`SessionEvent::PinInvalidated`].
    pub fn new_pin(&mut self) -> Option<String> {
        if self.role != SessionRole::Host {
            return None;
        }
        let pin = self.net.enable_pin();
        self.pin = Some(pin.clone());
        Some(pin)
    }

    pub fn register_by_name(&self, server: PeerAddr) {
        self.net.register_by_name(server);
    }

    pub fn set_password(&mut self, password: Option<String>) -> Result<(), String> {
        self.net.set_password(password).map_err(|e| e.to_string())
    }

    /// Host: remove `evict` (if any) everywhere, then rotate the session key so that it cannot
    /// come back with the old one. Returns the new invite link; `None`, with nothing changed,
    /// when `evict` is not a current peer.
    pub fn rotate_key(&mut self, h: &mut Host, evict: Option<PeerId>) -> Option<String> {
        if self.role != SessionRole::Host {
            return None;
        }
        // A member that is gone under this id (left, or reconnected under a new one) is not
        // turned into a plain key change: that would hand it the new key and block nobody.
        if evict.is_some_and(|id| self.peer_index(id).is_none()) {
            return None;
        }
        let mut evict_id = None;
        if let Some(i) = evict.and_then(|id| self.peer_index(id)) {
            let id = self.peers[i].info.id;
            evict_id = Some(id);
            let mut m = Vec::new();
            aoo::osc::encode(&mut m, EVICT, [aoo::osc::Arg::Blob(&id)]);
            for (j, p) in self.peers.iter().enumerate() {
                if j != i {
                    p.send_control(&self.net, Frame::AppOsc(m.clone()));
                }
            }
            self.close_peer_at(h, i, Some(CloseReason::Removed));
            // also refuses a reconnect racing the rotation
            self.block_id(id);
        }
        // nothing more is queued to the closed link, and the REKEY names the evicted member:
        // every member blocks it before passing the new key on
        self.seed = Some(match evict_id {
            Some(id) => self.net.evict(id),
            None => self.net.rotate_session_key(Duration::ZERO),
        });
        self.invite = None;
        self.refresh_addr(h, true);
        self.invite.clone()
    }

    /// Block `id` here and in the transport (which closes any connection to it left: close
    /// with the reason it should see first).
    fn block_id(&mut self, id: Id) {
        if !self.blocked.contains(&id) {
            self.blocked.push(id);
        }
        self.net.block(id);
    }

    /// Endpoint ids blocked for this session (not persisted).
    pub fn blocked(&self) -> Vec<Id> {
        self.blocked.clone()
    }

    pub fn unblock(&mut self, id: &Id) {
        self.blocked.retain(|b| b != id);
        self.net.unblock(id);
    }

    /// Peers with a private stream.
    fn private_count(&self) -> usize {
        self.peers.iter().filter(|p| p.private.is_some()).count()
    }

    /// Remove peer `i`; its connection closes after the frames queued for it, telling it
    /// `reason`.
    fn close_peer_at(&mut self, h: &mut Host, i: usize, reason: Option<CloseReason>) {
        let p = self.peers.remove(i);
        for t in self.main.iter().chain(&p.private) {
            t.lock().unsubscribe(p.conn);
        }
        match reason {
            Some(r) => self.net.close(p.conn, r),
            None => self.net.disconnect(p.conn),
        }
        h.tx.send(AudioCmd::Modern(LinkCmd::Remove(p.c.slot)));
        self.forget_peer(h, &p.c);
    }

    pub fn encode_count(&self) -> u64 {
        let main = self.main.iter();
        let privates = self.peers.iter().filter_map(|p| p.private.as_ref());
        main.chain(privates).map(|t| t.lock().encode_count()).sum()
    }

    fn find_conn(&self, conn: ConnId) -> Option<usize> {
        self.peers.iter().position(|p| p.conn == conn)
    }

    /// The send thread's work: every send stream and every peer's datagram sender.
    pub fn sources(&self, streams: &mut Vec<Arc<Mutex<TxStream>>>, subs: &mut Vec<(ConnId, PeerTx)>) {
        streams.extend(self.main.iter().map(|t| t.tx.clone()));
        streams.extend(self.peers.iter().filter_map(|p| p.private.as_ref().map(|t| t.tx.clone())));
        subs.extend(self.peers.iter().map(|p| (p.conn, p.tx.clone())));
    }

    /// What the send thread sent to a connection.
    pub fn add_sent(&mut self, conn: ConnId, bytes: u64, packets: i64) {
        if let Some(i) = self.find_conn(conn) {
            let p = &mut self.peers[i].c;
            p.bytes_sent += bytes;
            p.packets_sent += packets;
        }
    }

    // ---- network events ----------------------------------------------------------------------

    pub fn on_events(&mut self, h: &mut Host) {
        while let Ok(e) = self.net.events().try_recv() {
            self.on_event(h, e);
        }
    }

    fn on_event(&mut self, h: &mut Host, e: NetEvent) {
        match e {
            NetEvent::Connected { conn, peer, tx, .. } => self.on_connected(h, conn, peer, tx),
            NetEvent::ConnectFailed {
                conn,
                reason,
                retry_after,
            } => {
                // ours, or (name joins: the runtime dials the listed members) any before we
                // are in
                let joining = self.peers.is_empty() && self.role != SessionRole::Host;
                if self.dials.contains(&conn) || joining {
                    if reason == END_FULL {
                        h.session_event(SessionEvent::SessionFull);
                        h.session_event(SessionEvent::Ended {
                            reason: END_FULL.into(),
                        });
                    } else {
                        h.session_event(SessionEvent::JoinFailed {
                            reason,
                            retry_after,
                        });
                    }
                }
            }
            NetEvent::IncomingFailed { count, last_reason } => {
                h.session_event(SessionEvent::IncomingFailed { count, last_reason })
            }
            NetEvent::Control { conn, frame } => {
                if let Some(i) = self.find_conn(conn) {
                    self.on_control(h, i, frame);
                }
            }
            NetEvent::Closed { conn, reason } => {
                if let Some(i) = self.find_conn(conn) {
                    let id = self.peers[i].info.id;
                    let host = self.peers[i].info.is_invite_host || self.net.authority() == Some(id);
                    self.remove_peer_at(h, i, false);
                    // the reasons a peer sends (`CloseReason`); local reasons never match
                    let reason = match reason.as_str() {
                        r @ (END_REMOVED | END_BLOCKED | END_FULL) if host || self.peers.is_empty() => Some(r),
                        _ if self.role == SessionRole::Listener && host => Some(END_HOST_LEFT),
                        _ => None,
                    };
                    if let Some(r) = reason {
                        h.session_event(SessionEvent::Ended { reason: r.into() });
                    }
                }
            }
            NetEvent::PathChanged { conn, path } => {
                if let Some(i) = self.find_conn(conn) {
                    let p = &mut self.peers[i];
                    // relayed <-> direct: another RTT, not a queue
                    if p.path.as_ref().is_some_and(|o| o.direct != path.direct)
                        && let Some(s) = &mut p.sub
                    {
                        s.adapt.reset_baseline();
                    }
                    h.session_event(SessionEvent::PathChanged {
                        id: p.c.id,
                        direct: path.direct,
                        relay: path.relay.clone(),
                        rtt_ms: path.rtt.as_secs_f32() * 1000.0,
                    });
                    p.path = Some(path);
                    h.event(Event::PeerChanged { id: p.c.id });
                }
            }
            NetEvent::SessionKey(_) => h.session_event(SessionEvent::SessionKeyChanged),
            // the host we joined through (password join, or a mesh invite) is a broadcast hub:
            // a listener from now on, as by a broadcast invite
            // (the transport refuses connections and dials nobody; it comes before Connected,
            // so no peer has subscribed to our send stream)
            NetEvent::JoinedBroadcast if self.role == SessionRole::Member => {
                (self.mode, self.role) = (SessionMode::Broadcast, SessionRole::Listener);
                if self.main.take().is_some() {
                    h.tx.send(AudioCmd::Modern(LinkCmd::Main(None)));
                }
            }
            NetEvent::JoinedBroadcast => {}
            NetEvent::JoinFailed { failed_total } => {
                self.failed_joins = failed_total;
                h.session_event(SessionEvent::PasswordJoinFailed { failed_total });
            }
            NetEvent::RendezvousFailed {
                reason,
                retry_after,
            } => h.session_event(SessionEvent::RendezvousFailed {
                reason,
                retry_after,
            }),
            NetEvent::PinInvalidated => h.session_event(SessionEvent::PinInvalidated),
            NetEvent::Throttled { ip, retry_after } => {
                h.session_event(SessionEvent::Throttled { ip, retry_after })
            }
            NetEvent::LocalRelay(_) => self.refresh_addr(h, false),
        }
    }

    fn on_connected(&mut self, h: &mut Host, conn: ConnId, info: PeerInfo, tx: PeerTx) {
        if self.blocked.contains(&info.id) {
            self.net.disconnect(conn);
            return;
        }
        // the same endpoint again (it reconnected): the new connection replaces the old one
        if let Some(i) = self.peers.iter().position(|p| p.info.id == info.id) {
            let old = self.peers[i].conn;
            self.remove_peer_at(h, i, false);
            self.net.disconnect(old);
        }
        // the name is only a label: one already in use (by a peer or by us) gets the endpoint's
        // fingerprint, never merges with it; settings follow the endpoint id
        let fp = &hex(&info.id)[..8];
        let mut user = crate::proto::clean_name(&info.name);
        if user.is_empty() {
            user = format!("peer {fp}");
        } else if user == self.c.user || self.peers.iter().any(|p| p.c.user == user) {
            user = format!("{user} #{fp}");
        }
        let key = format!("{}{}", crate::model::ID_KEY_PREFIX, hex(&info.id));
        let group = self.session.clone();
        let Some(mut c) = PeerCore::add_keyed(h, self.peers.len(), &user, &key, &group, &user)
        else {
            self.net.disconnect(conn);
            return;
        };
        c.connected = true;
        // as legacy's connect: we send unless the main send mute is on
        c.send_active = c.send_allow;
        h.tx.send(AudioCmd::Modern(LinkCmd::Add(c.slot, Box::default())));
        let now_ms = h.now_ms();
        self.peers.push(Peer {
            c,
            conn,
            info,
            tx,
            rx: None,
            sub: None,
            private: None,
            max_bitrate: 0,
            pending_req: None,
            req_ms: f64::NEG_INFINITY,
            fallback: None,
            fallback_ms: 0.0,
            path: None,
            playing: false,
            last_rtt_ms: now_ms,
            last_ping_ms: now_ms,
            playout: None,
            clock: Clock::default(),
        });
        // a new listener gets the playout target right away
        self.playout_due |= self.playout_ms.is_some();
        let i = self.peers.len() - 1;
        self.peer_added(h, i);
        self.route(h, i);
        if self.private_or_main(i).is_some() {
            self.announce(i);
        }
        self.send_layout(h, i);
        self.send_peer_info(h, i);
    }

    /// FORMAT of the stream peer `i` gets from us now.
    fn announce(&self, i: usize) {
        if let Some(t) = self.private_or_main(i) {
            let f = t.lock().wire_format();
            self.peers[i].send_control(&self.net, Frame::Format(f));
        }
    }

    fn private_or_main(&self, i: usize) -> Option<&Tx> {
        self.peers[i].private.as_ref().or(self.main.as_ref())
    }

    fn stream_of(&self, i: usize, stream: u8) -> Option<&Tx> {
        match stream {
            MAIN_STREAM => self.main.as_ref(),
            PRIVATE_STREAM => self.peers[i].private.as_ref(),
            _ => None,
        }
    }

    fn on_control(&mut self, h: &mut Host, i: usize, frame: Frame) {
        match frame {
            Frame::Format(f) => self.on_format(h, i, &f),
            Frame::Subscribe(s) => self.on_subscribe(h, i, s),
            Frame::QualityReq(q) => {
                self.peers[i].max_bitrate = q.max_bitrate;
                let fi = usize::from(q.preferred_format & !FORMAT_REQ);
                if q.preferred_format != 0xff
                    && q.preferred_format & FORMAT_REQ != 0
                    && fi < FORMATS.len()
                {
                    // as a legacy codec change request: our send format for it follows,
                    // within what we allow
                    self.peers[i].pending_req = Some(fi);
                    self.apply_request(h, i);
                }
                self.resubscribe(i);
            }
            Frame::AppOsc(raw) => self.on_app_osc(h, i, &raw),
            Frame::Playout(pl) if self.playout_source(i) => {
                let p = &mut self.peers[i];
                p.playout = accept_playout(pl);
                if p.playout.is_none() {
                    // off: the next time starts from fresh clock samples
                    p.clock = Clock::default();
                }
            }
            // the roster mesh is the transport's
            _ => {}
        }
    }

    /// Peer `i`'s pending format request, once [`REQUEST_INTERVAL_MS`] passed since the last
    /// one took effect: capped and mapped to what we allow.
    fn apply_request(&mut self, h: &mut Host, i: usize) {
        let ms = h.now_ms();
        let p = &mut self.peers[i];
        let Some(want) = p.pending_req.filter(|_| ms - p.req_ms >= REQUEST_INTERVAL_MS) else {
            return;
        };
        p.pending_req = None;
        let Some(mut fi) = options::requested_format(&h.model.state.extra, want) else {
            return;
        };
        // a hub serves its shared encodings only; a member a few private streams
        if let Some(main) = &self.main {
            let shared_only = self.mode == SessionMode::Broadcast
                || (self.peers[i].private.is_none()
                    && self.private_count() >= MAX_PRIVATE_STREAMS);
            if shared_only && !main.formats.contains(&fi) {
                fi = nearest(&main.formats, fi);
            }
        }
        if fi != self.peers[i].c.format_index {
            self.peers[i].req_ms = ms;
            self.set_send_format(h, i, fi);
            h.event(Event::PeerChanged {
                id: self.peers[i].c.id,
            });
        }
    }

    fn on_app_osc(&mut self, h: &mut Host, i: usize, raw: &[u8]) {
        if raw.starts_with(EVICT.as_bytes()) {
            let from = self.peers[i].info.id;
            if self.net.authority() != Some(from) {
                return;
            }
            let Ok(m) = aoo::osc::decode(raw) else { return };
            if let Some(aoo::osc::Arg::Blob(b)) = m.args().next()
                && let Ok(id) = <Id>::try_from(b)
            {
                if id == self.net.local_addr().id {
                    // we are the one removed (the host closes on us too)
                    h.session_event(SessionEvent::Ended {
                        reason: END_REMOVED.into(),
                    });
                    return;
                }
                if let Some(j) = self.peers.iter().position(|p| p.info.id == id) {
                    let conn = self.peers[j].conn;
                    self.remove_peer_at(h, j, false);
                    self.net.disconnect(conn);
                }
                // (the REKEY behind this notice names it too: the transport blocks it before
                // passing the new key on, whether or not we handled this first)
                self.block_id(id);
            }
            return;
        }
        match sbmsg::parse(raw) {
            // `/sb/ping`: the transport's RTT replaces it
            Ok(Some(SbMessage::Ping { .. } | SbMessage::PingAck { .. })) | Err(_) | Ok(None) => {}
            Ok(Some(m)) => self.handle_sb(h, i, m, raw),
        }
    }

    /// The stream peer `i` sends us now.
    fn on_format(&mut self, h: &mut Host, i: usize, f: &sonobus_net::wire::Format) {
        if f.stream != MAIN_STREAM && f.stream != PRIVATE_STREAM {
            return;
        }
        let Ok(sf) = StreamFormat::from_wire(f) else {
            return;
        };
        let cfg = h.cfg;
        let dynres = h.model.state.params.dynamic_resampling;
        let p = &mut self.peers[i];
        let slot = p.c.slot;
        if p.rx.as_ref().is_none_or(|r| r.rx.format().is_none() || stream_id(r) != f.stream) {
            let opts = RxOptions {
                buffer_ms: p.c.jitter.buffer_ms,
                dynamic_resampling: dynres,
                ..RxOptions::default()
            };
            let (rx, audio) = RxStream::new(f.stream, device(&cfg, MAX_CHANNELS), opts);
            h.tx.send(AudioCmd::Modern(LinkCmd::Rx(slot, Box::new(audio))));
            p.rx = Some(Rx {
                rx,
                stream: f.stream,
                format: None,
            });
            p.playing = false;
        }
        let Some(r) = p.rx.as_mut() else { return };
        if r.rx.set_format(f).is_err() {
            return;
        }
        r.format = Some(sf.clone());
        if h.model
            .set_recv_channels(slot, usize::from(sf.channels), false)
        {
            h.publish_peer(slot);
        }
        p.c.connected = true;
        if p.c.recv_allow {
            p.send_control(
                &self.net,
                Frame::Subscribe(Subscribe {
                    stream: f.stream,
                    encoding: 0,
                    redundancy: 0,
                }),
            );
        }
        h.event(Event::PeerChanged { id: p.c.id });
    }

    fn on_subscribe(&mut self, h: &mut Host, i: usize, s: Subscribe) {
        if s.encoding == UNSUBSCRIBE {
            let p = &mut self.peers[i];
            if let Some(sub) = p.sub.take() {
                self.unsubscribe_from(i, sub.stream);
            }
            return;
        }
        let Some(t) = self.stream_of(i, s.stream) else {
            return;
        };
        // a private single-Opus stream walks a bit-rate ladder; otherwise the encodings
        let (lv, is_ladder) = {
            let tx = t.lock();
            let f = tx.format();
            match f.encodings[..] {
                [Encoding::Opus { bitrate, .. }] if s.stream == PRIVATE_STREAM => {
                    (ladder(bitrate, usize::from(f.channels)), true)
                }
                _ => (levels(f), false),
            }
        };
        let now = Instant::now();
        let p = &mut self.peers[i];
        // a format change since makes a fresh adaptation: the old levels may name encodings
        // that are gone
        let old = p.sub.take().filter(|o| o.fits(s.stream, &lv));
        p.sub = Some(old.unwrap_or(Sub {
            stream: s.stream,
            adapt: Adapt::new(now, lv.clone()),
            decision: Decision::default(),
            levels: lv,
            ladder: is_ladder,
        }));
        p.c.connected = true;
        let other = if s.stream == MAIN_STREAM {
            PRIVATE_STREAM
        } else {
            MAIN_STREAM
        };
        self.unsubscribe_from(i, other);
        self.resubscribe(i);
        h.event(Event::PeerChanged {
            id: self.peers[i].c.id,
        });
    }

    fn unsubscribe_from(&self, i: usize, stream: u8) {
        if let Some(t) = self.stream_of(i, stream) {
            t.lock().unsubscribe(self.peers[i].conn);
        }
    }

    /// The pin a peer's send format puts on the shared stream's level (it asked for low).
    fn pin(&self, i: usize, stream: u8) -> u8 {
        match self.stream_of(i, stream) {
            Some(t) if t.formats.get(1) == Some(&self.peers[i].c.format_index) => 1,
            _ => 0xff,
        }
    }

    /// Apply peer `i`'s subscription (the adaptation's level and redundancy, its requests, our
    /// redundancy floor) to the stream it subscribed to, or stop it while sending is off.
    fn resubscribe(&mut self, i: usize) {
        let floor = self.red_floor;
        let Some(stream) = self.peers[i].sub.as_ref().map(|s| s.stream) else {
            return;
        };
        let pin = self.pin(i, stream);
        let p = &mut self.peers[i];
        let (conn, active, max_bitrate) = (p.conn, p.c.send_active, p.max_bitrate);
        let Some(sub) = p.sub.as_mut() else { return };
        let req = (pin != 0xff || max_bitrate > 0).then_some(QualityReq {
            max_bitrate,
            preferred_format: pin,
        });
        sub.adapt.set_request(req);
        let d = sub.adapt.decision();
        sub.decision = d;
        let congested = sub.adapt.congested();
        let k = sub.k(floor);
        let rung = sub.ladder.then(|| sub.levels[d.level.min(sub.levels.len() - 1)]);
        let Some(t) = self.stream_of(i, stream) else { return };
        let mut tx = t.lock();
        if active {
            match rung {
                Some(b) => {
                    tx.subscribe(conn, 0, k);
                    tx.set_bitrate(0, b);
                }
                None => {
                    tx.subscribe(conn, d.level as u8, k);
                }
            }
            tx.set_resend_paused(conn, congested);
        } else {
            tx.unsubscribe(conn);
        }
    }

    /// Peer `i` is congested at the lowest level of what it gets: a cheaper private stream,
    /// if there is one and a private stream to spare.
    fn try_fallback(&mut self, h: &mut Host, i: usize) {
        let p = &self.peers[i];
        let (Some(sub), Some(t)) = (&p.sub, self.private_or_main(i)) else {
            return;
        };
        let floor = sub.levels.last().copied().unwrap_or(0);
        let Some(fi) = fallback_format(floor, t.nch) else {
            return;
        };
        if p.private.is_none() && self.private_count() >= MAX_PRIVATE_STREAMS {
            return;
        }
        let p = &mut self.peers[i];
        p.fallback = Some(fi);
        p.fallback_ms = h.now_ms();
        self.route(h, i);
    }

    /// Whether peer `i` needs its own encoder, and for what: (format, channels).
    fn private_needs(&self, h: &Host, i: usize) -> Option<(usize, usize)> {
        let main = self.main.as_ref()?;
        if let Some(fi) = self.peers[i].fallback {
            return Some((fi, self.peers[i].c.send_channels));
        }
        let p = &self.peers[i].c;
        let routed = h.model.routed_to[p.slot] != 0;
        let fits = main.formats.contains(&p.format_index);
        (routed || p.send_channels != main.nch || !fits).then_some((p.format_index, p.send_channels))
    }

    /// Give peer `i` the shared stream or a private one, as it needs; a change announces the
    /// new stream (the peer re-subscribes to it).
    fn route(&mut self, h: &mut Host, i: usize) {
        let need = self.private_needs(h, i);
        let have = self.peers[i]
            .private
            .as_ref()
            .map(|t| (t.formats[0], t.nch));
        if need == have {
            self.resubscribe(i);
            return;
        }
        let cfg = h.cfg;
        let slot = self.peers[i].c.slot;
        let conn = self.peers[i].conn;
        match need {
            Some((fi, nch)) => {
                let dynres = h.model.state.params.dynamic_resampling;
                let (t, audio) = Tx::new(PRIVATE_STREAM, fi, None, nch, &cfg, dynres);
                h.tx.send(AudioCmd::Modern(LinkCmd::Tx(slot, Some(Box::new(audio)))));
                // a new private stream gets a fresh adaptation once the peer subscribes to it
                let p = &mut self.peers[i];
                if p.sub.as_ref().is_some_and(|s| s.stream == PRIVATE_STREAM) {
                    p.sub = None;
                }
                if let Some(old) = self.peers[i].private.replace(t) {
                    // a replaced private stream: the new one starts a new generation for it
                    old.lock().unsubscribe(conn);
                    let g = old.lock().wire_format().generation;
                    let new = self.peers[i].private.as_ref().map(|t| t.tx.clone());
                    if let Some(new) = new {
                        let mut n = lock(&new);
                        while n.wire_format().generation == g {
                            let f = n.format().clone();
                            let _ = n.set_format(f);
                        }
                    }
                }
            }
            None => {
                h.tx.send(AudioCmd::Modern(LinkCmd::Tx(slot, None)));
                if let Some(old) = self.peers[i].private.take() {
                    old.lock().unsubscribe(conn);
                }
            }
        }
        self.announce(i);
    }

    /// The shared stream follows the default send format and the send mode's channel count.
    fn sync_main(&mut self, h: &mut Host) {
        let fi = crate::formats::checked(h.model.state.params.def_send_qual);
        let nch = main_channels(h);
        let cfg = h.cfg;
        let Some(m) = self.main.as_mut() else { return };
        if m.formats[0] == fi && m.nch == nch {
            return;
        }
        let low = low_of(fi);
        let mut tx = lock(&m.tx);
        if m.nch != nch {
            let audio = tx.setup(device(&cfg, nch));
            h.tx.send(AudioCmd::Modern(LinkCmd::Main(Some(Box::new(audio)))));
        }
        if tx.set_format(stream_format(fi, low, nch, &cfg)).is_ok() {
            m.formats = std::iter::once(fi).chain(low).collect();
        }
        m.nch = nch;
    }

    // ---- datagrams and timers ------------------------------------------------------------------

    pub fn on_datagram(&mut self, h: &mut Host, conn: ConnId, d: &Bytes, now: Instant) {
        if !self.rx_delay.is_zero() && d.first().is_some_and(|k| k & 0x0f == KIND_AUDIO) {
            self.delayed.push_back((now + self.rx_delay, conn, d.clone()));
            return;
        }
        self.handle_datagram(h, conn, d, now);
    }

    fn handle_datagram(&mut self, h: &mut Host, conn: ConnId, d: &Bytes, now: Instant) {
        let Some(i) = self.find_conn(conn) else {
            return;
        };
        self.peers[i].c.bytes_received += d.len() as u64;
        match Datagram::decode(d) {
            Ok(Datagram::Audio(a)) => {
                if self.loss_pct > 0 {
                    self.loss_seed = self.loss_seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                    if (self.loss_seed >> 8) % 100 < self.loss_pct {
                        return;
                    }
                }
                let ms = h.now_ms();
                let p = &mut self.peers[i];
                if let Some(r) = &mut p.rx
                    && a.header.stream == stream_id(r)
                {
                    r.rx.handle_audio(now, &a);
                    p.c.got_data(ms);
                }
            }
            Ok(Datagram::Nack(n)) => {
                if let Some(t) = self.stream_of(i, n.stream) {
                    let p = &self.peers[i];
                    let (mut bytes, mut packets) = (0u64, 0i64);
                    t.lock().handle_nack_at(now, conn, &n, |_, b| {
                        bytes += b.len() as u64;
                        packets += 1;
                        let _ = p.tx.send(b);
                    });
                    self.add_sent(conn, bytes, packets);
                }
            }
            Ok(Datagram::Report(r)) => {
                let p = &mut self.peers[i];
                let Some(sub) = p.sub.as_mut().filter(|s| s.stream == r.stream) else {
                    return;
                };
                let was = sub.adapt.congested();
                let d = sub.adapt.on_report(now, &r);
                let loss = sub.adapt.opus_loss_percent();
                let exhausted = sub.adapt.exhausted();
                if d != sub.decision || was != sub.adapt.congested() {
                    self.resubscribe(i);
                    let loss = if r.stream == MAIN_STREAM {
                        let subs = self.peers.iter().filter_map(|p| p.sub.as_ref());
                        let main = subs.filter(|s| s.stream == MAIN_STREAM);
                        shared_loss(main.map(|s| s.adapt.opus_loss_percent()).collect())
                    } else {
                        loss
                    };
                    if let Some(t) = self.stream_of(i, r.stream) {
                        t.lock().set_expected_loss(loss);
                    }
                }
                if exhausted {
                    self.try_fallback(h, i);
                }
            }
            Ok(Datagram::Ping(ping)) if self.role == SessionRole::Host => {
                let t = self.us(now);
                let mut b = Vec::new();
                Pong {
                    id: ping.id,
                    t1: ping.t1,
                    t2: t,
                    t3: self.us(Instant::now()).max(t),
                }
                .encode(&mut b);
                let p = &mut self.peers[i];
                p.c.bytes_sent += b.len() as u64;
                let _ = p.tx.send(b.into());
            }
            Ok(Datagram::Pong(pong)) => {
                let t4 = self.us(now);
                let ms = h.now_ms();
                self.peers[i].clock.on_pong(&pong, t4, ms);
            }
            _ => {}
        }
    }

    /// Timer work, every few ms: receive streams (concealment, NACK, REPORT, events → jitter
    /// controller), RTT samples, ping estimate, stats, layouts, the peer cache, FORMAT changes.
    pub fn tick(&mut self, h: &mut Host, blocks: u64, now: Instant) {
        while let Some(&(t, conn, _)) = self.delayed.front()
            && t <= now
        {
            let (_, _, d) = self.delayed.pop_front().expect("front");
            self.handle_datagram(h, conn, &d, now);
        }
        let ms = h.now_ms();
        let block_ms = h.cfg.block_ms();
        let thresh = h.model.state.extra.auto_drop_rate_thresh;
        let new_blocks = blocks.saturating_sub(self.last_blocks).min(256);
        self.last_blocks = blocks;
        for i in 0..self.peers.len() {
            self.apply_request(h, i);
            if self.role != SessionRole::Host {
                self.sync_playout(h, i, now);
            }
            let p = &mut self.peers[i];
            let rtt = p.path.as_ref().map_or(Duration::from_millis(1), |x| x.rtt);
            let mut change = Change::None;
            let mut resub = false;
            if let Some(r) = &mut p.rx {
                r.rx.tick(now, rtt);
                while let Some(d) = r.rx.poll_transmit(now) {
                    p.c.bytes_sent += d.len() as u64;
                    let _ = p.tx.send(d);
                }
                while let Some(e) = r.rx.poll_event() {
                    match e {
                        RxEvent::StateChanged { playing } => {
                            p.playing = playing;
                            p.c.recv_active = p.c.recv_allow && playing;
                            p.c.sync_shared();
                        }
                        RxEvent::BlocksLost { count } => {
                            let n = count.min(i32::MAX as u32) as i32;
                            if p.c.jitter.on_blocks_lost(n, ms, block_ms, thresh) == Change::Buffer {
                                change = Change::Buffer;
                            }
                        }
                        RxEvent::BlocksResent { count } | RxEvent::BlocksRecovered { count } => {
                            p.c.jitter.resent += i64::from(count)
                        }
                        RxEvent::BlocksLate { .. }
                        | RxEvent::BlocksReordered { .. }
                        | RxEvent::BlocksGap { .. } => {}
                    }
                }
                p.c.jitter.lift_ms = r.rx.lift_ms();
                if let Some(f) = r.rx.buffer_fill_ratio() {
                    for _ in 0..new_blocks {
                        p.c.jitter.push_fill(f);
                    }
                }
            }
            let mut unfallback = false;
            if ms - p.last_rtt_ms >= RTT_INTERVAL_MS {
                p.last_rtt_ms = ms;
                let path = p.tx.path();
                // the hard safety net: our datagram queue to it stays (nearly) full
                let depleted = p.tx.send_buffer_space() < SEND_BUFFER_LOW;
                if let Some(s) = &mut p.sub {
                    if let Some(path) = &path {
                        s.adapt.on_rtt(now, path.rtt);
                    }
                    let was = s.adapt.congested();
                    resub = s.adapt.on_send_buffer(now, depleted) != s.decision
                        || was != s.adapt.congested();
                    unfallback = p.fallback.is_some()
                        && ms - p.fallback_ms >= FALLBACK_MIN_MS
                        && s.stream == PRIVATE_STREAM
                        && s.adapt.has_headroom(now);
                }
                if path.is_some() {
                    p.path = path;
                }
            }
            if ms - p.last_ping_ms >= PING_INTERVAL_MS {
                p.last_ping_ms = ms;
                if let Some(path) = &p.path {
                    p.c.ping.push(path.rtt.as_secs_f64() * 1000.0);
                    p.c.ping.new_style = true;
                }
                // the legacy sink's ping event: the jitter controller's slow clock
                if p.playing && p.c.jitter.on_ping(ms, block_ms) == Change::Buffer {
                    change = Change::Buffer;
                }
                p.c.sync_shared();
            }
            p.c.tick(h, ms);
            if change == Change::Buffer {
                self.apply_buffer(h, i);
            }
            if resub {
                self.resubscribe(i);
            }
            if unfallback {
                self.peers[i].fallback = None;
                self.route(h, i);
            }
        }
        // a send stream that changed format announces it to everyone who may subscribe
        let main_change = self.main.as_ref().and_then(|t| t.lock().take_format_change());
        // a new generation needs a new anchor
        let due = self.playout_ms.is_some()
            && (main_change.is_some() || ms - self.last_playout_ms >= PLAYOUT_INTERVAL_MS);
        if self.playout_due || due {
            self.playout_due = false;
            self.last_playout_ms = ms;
            self.send_playout();
        }
        for i in 0..self.peers.len() {
            let private = self.peers[i].private.as_ref().map(|t| t.lock().take_format_change());
            match private {
                Some(Some(f)) => self.peers[i].send_control(&self.net, Frame::Format(f)),
                Some(None) => {}
                None => {
                    if let Some(f) = &main_change {
                        self.peers[i].send_control(&self.net, Frame::Format(f.clone()));
                    }
                }
            }
        }
        self.send_layout_if_due(h, ms);
        self.commit_caches_if_due(h, ms);
        if ms - self.last_addr_ms >= ADDR_POLL_MS {
            self.last_addr_ms = ms;
            self.refresh_addr(h, false);
        }
    }

    /// Before the session goes: every peer out of the mixer (a host tells them it leaves),
    /// the shared stream off the audio thread.
    pub fn close(&mut self, h: &mut Host) {
        let reason = (self.role == SessionRole::Host).then_some(CloseReason::HostLeft);
        while let Some(i) = self.peers.len().checked_sub(1) {
            self.close_peer_at(h, i, reason);
        }
        h.tx.send(AudioCmd::Modern(LinkCmd::Main(None)));
        if let Some((lan, _)) = &self.lan {
            lan.stop_advertising();
        }
    }
}

impl Drop for Session {
    /// Stops the session thread, then (dropping `net`) says BYE to everyone.
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Release);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn stream_id(r: &Rx) -> u8 {
    r.stream
}

/// Channels of the shared stream: the send mode's.
fn main_channels(h: &Host) -> usize {
    let nominal = h.model.state.params.send_channels;
    if nominal <= 0 {
        h.model.total_send_channels().max(1)
    } else {
        nominal as usize
    }
}

/// PR #202 copies per packet (1..=5) as redundant blocks per datagram.
fn red_floor(n: i32) -> u8 {
    (n.clamp(1, 5) - 1).min(i32::from(sonobus_net::stream::MAX_REDUNDANCY)) as u8
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
        let mut b = Vec::new();
        if m.encode(&mut b).is_ok() {
            let p = &mut self.peers[i];
            p.c.bytes_sent += b.len() as u64;
            p.send_control(&self.net, Frame::AppOsc(b));
        }
    }

    fn send_layout(&mut self, h: &Host, i: usize) {
        let m = SbMessage::ChannelLayoutInfo {
            source_id: 0,
            layouts: h.model.send_layout(),
        };
        self.send_sb(i, &m);
    }

    fn push_buffer(&mut self, i: usize) {
        let p = &mut self.peers[i];
        if let Some(r) = &mut p.rx {
            r.rx.set_buffer_ms(p.c.jitter.buffer_ms);
        }
    }

    fn push_send_format(&mut self, h: &mut Host, i: usize) {
        self.sync_main(h);
        self.route(h, i);
    }

    fn set_send_active(&mut self, i: usize, active: bool) {
        let p = &mut self.peers[i].c;
        p.send_active = active;
        if active {
            p.send_allow = true;
            p.send_allow_cache = true;
        }
        self.resubscribe(i);
    }

    fn set_recv_active(&mut self, i: usize, active: bool) {
        let p = &mut self.peers[i];
        if active {
            p.c.recv_allow = true;
            p.c.recv_allow_cache = true;
        }
        let Some(stream) = p.rx.as_ref().map(stream_id) else {
            return;
        };
        let s = Subscribe {
            stream,
            encoding: if active { 0 } else { UNSUBSCRIBE },
            redundancy: 0,
        };
        p.send_control(&self.net, Frame::Subscribe(s));
    }

    fn update_send_channels(&mut self, h: &mut Host) {
        self.sync_main(h);
        let nominal = h.model.state.params.send_channels;
        for i in 0..self.peers.len() {
            self.peers[i].c.nominal_send_channels = nominal;
            let n = self.compute_send_channels(h, i, nominal);
            crate::proto::set_model_send_channels(h, self.peers[i].c.slot, n);
            self.peers[i].c.send_channels = n;
            self.route(h, i);
        }
        self.c.layout_due = true;
    }

    fn remove_peer_at(&mut self, h: &mut Host, i: usize, block: bool) {
        if block {
            let user = self.c.user.clone();
            let m = SbMessage::BlockedInfo {
                from: &user,
                blocked: true,
            };
            self.send_sb(i, &m);
        }
        self.close_peer_at(h, i, block.then_some(CloseReason::Blocked));
    }

    /// The user removes a peer: it is told so ([`END_REMOVED`] when we are its host).
    fn remove_peer(&mut self, h: &mut Host, id: PeerId, block: bool) {
        if let Some(i) = self.peer_index(id) {
            if block {
                self.remove_peer_at(h, i, true);
            } else {
                self.close_peer_at(h, i, Some(CloseReason::Removed));
            }
        }
    }

    /// By endpoint id, for the rest of the session.
    fn block_peer_at(&mut self, h: &mut Host, i: usize) {
        let id = self.peers[i].info.id;
        self.remove_peer_at(h, i, true);
        self.block_id(id);
    }

    fn request_format(&mut self, _h: &Host, i: usize, index: Option<usize>) {
        let p = &mut self.peers[i];
        p.c.requested_format = index;
        let preferred_format = index
            .filter(|&f| f < FORMATS.len())
            .map_or(0xff, |f| FORMAT_REQ | f as u8);
        let q = QualityReq {
            max_bitrate: 0,
            preferred_format,
        };
        p.send_control(&self.net, Frame::QualityReq(q));
    }

    fn set_dynamic_resampling(&mut self, on: bool) {
        for p in &mut self.peers {
            if let Some(r) = &mut p.rx {
                r.rx.set_dynamic_resampling(on);
            }
        }
    }

    fn set_redundancy(&mut self, n: i32) {
        self.red_floor = red_floor(n);
        for i in 0..self.peers.len() {
            self.resubscribe(i);
        }
    }

    /// Not available in modern sessions (the estimate is shown instead).
    fn start_latency_test(&mut self, _i: usize) -> bool {
        false
    }

    fn stop_latency_test(&mut self, _i: usize) {}

    fn recv_opus(&self, i: usize) -> bool {
        let f = self.peers[i].rx.as_ref().and_then(|r| r.format.as_ref());
        f.is_some_and(|f| f.codec() == CODEC_OPUS)
    }

    fn recv_format_name(&self, i: usize) -> Option<String> {
        let f = self.peers[i].rx.as_ref()?.format.as_ref()?;
        Some(format_of(f, 0).map_or_else(
            || if f.codec() == CODEC_OPUS { "Opus" } else { "PCM" }.to_owned(),
            |fi| FORMATS[fi].name.to_owned(),
        ))
    }

    fn addr(&self, i: usize) -> SocketAddr {
        let p = &self.peers[i];
        p.path
            .as_ref()
            .and_then(|x| x.remote)
            .or_else(|| p.info.addrs.first().copied())
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    fn peer_net(&self, i: usize) -> Option<PeerNet> {
        let p = &self.peers[i];
        Some(PeerNet {
            endpoint_id: p.info.id,
            is_invite_host: p.info.is_invite_host,
            role: match p.info.role {
                Role::Host => PeerRole::Host,
                Role::Peer => PeerRole::Peer,
                Role::Listener => PeerRole::Listener,
            },
            direct: p.path.as_ref().is_some_and(|x| x.direct),
            relay: p.path.as_ref().and_then(|x| x.relay.clone()),
            remote: p.path.as_ref().and_then(|x| x.remote),
            rtt_ms: p.path.as_ref().map_or(0.0, |x| x.rtt.as_secs_f32() * 1000.0),
            send_level: p
                .sub
                .as_ref()
                .filter(|_| p.c.send_active)
                .map(|s| (s.decision.level, s.k(self.red_floor))),
            playout_error_ms: p.rx.as_ref().and_then(|r| r.rx.playout_error_ms()),
        })
    }

    fn hides_peers(&self) -> bool {
        self.mode == SessionMode::Broadcast && self.role == SessionRole::Host
    }
}

/// Send-thread pump of every send stream: encode what the audio thread queued, send each
/// subscriber its datagrams, count them per connection into `sent`.
pub(crate) fn pump(
    streams: &[Arc<Mutex<TxStream>>],
    subs: &[(ConnId, PeerTx)],
    sent: &mut Vec<(ConnId, u64, i64)>,
) {
    for s in streams {
        lock(s).pump(|id, b| {
            if let Some((_, tx)) = subs.iter().find(|(c, _)| *c == id) {
                let n = b.len() as u64;
                if tx.send(b).is_ok() {
                    match sent.iter_mut().find(|(c, ..)| *c == id) {
                        Some(e) => {
                            e.1 += n;
                            e.2 += 1;
                        }
                        None => sent.push((id, n, 1)),
                    }
                }
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The congestion ladders and fallbacks: a quarter of the rate at most, never below the
    /// floor; a fallback at most half the congested rate per channel (none below the table).
    #[test]
    fn ladders_fallbacks_and_nearest_shared_format() {
        assert_eq!(ladder(96_000, 1), [96_000, 72_000, 48_000, 36_000, 24_000]);
        assert_eq!(ladder(32_000, 2), [32_000, 24_000, 16_000]);
        assert_eq!(ladder(4_000, 1), [4_000]);
        // PCM 16 mono (768 kbit/s): the best Opus at most 384 kbit/s per channel, 256
        assert_eq!(fallback_format(768_000, 1), Some(7));
        // 96 kbps/ch stereo low encoding (48 x 2): 24 kbps/ch
        assert_eq!(fallback_format(96_000, 2), Some(1));
        assert_eq!(fallback_format(24_000, 1), None, "nothing below 16 kbps/ch");
        // a hub maps a PCM float request to its main encoding, a 24k one to its low one
        assert_eq!(nearest(&[4, 2], 10), 4);
        assert_eq!(nearest(&[4, 2], 1), 2);
        assert_eq!(nearest(&[4, 2], 3), 2, "a tie goes to the cheaper one");
    }

    /// A shared encoder's Opus expected loss: one receiver's bad path does not set it for all.
    #[test]
    fn shared_loss_is_the_lower_median() {
        assert_eq!(shared_loss(vec![]), 0);
        assert_eq!(shared_loss(vec![40]), 40);
        assert_eq!(shared_loss(vec![100, 0]), 0);
        assert_eq!(shared_loss(vec![0, 100, 3]), 3);
        assert_eq!(shared_loss(vec![100, 5, 10, 100]), 10);
    }

    /// NTP offset from PONGs: the least-delayed sample of the window wins, a queued (slow,
    /// asymmetric) one does not skew it; used only once a whole window of fresh samples is in;
    /// only answers to our own outstanding PINGs count, once, with plausible times; samples
    /// age out.
    #[test]
    fn clock_offset_takes_the_fastest_round_trip() {
        let mut c = Clock::default();
        // host clock = ours + 5000 µs; `up`/`down` one-way delays; `ms` the engine's clock
        let exchange = |c: &mut Clock, t1: u64, up: u64, down: u64| {
            let id = c.ping(t1, 0.0).id;
            let t2 = t1 + up + 5000;
            let p = Pong { id, t1, t2, t3: t2 + 10 };
            c.on_pong(&p, t2 + 10 - 5000 + down, t1 as f64 / 1000.0);
            p
        };
        let ms = |c: &Clock| c.samples[(c.n - 1) % CLOCK_SAMPLES].2;
        // 9 ms stuck on the way back: offset off by 4.5 ms, but a longer round trip
        exchange(&mut c, 100, 1000, 9000);
        let first = exchange(&mut c, 100_000, 1000, 1000);
        for k in 2..CLOCK_SAMPLES as u64 - 1 {
            exchange(&mut c, k * 100_000, 3000, 9000);
            assert_eq!(c.offset(ms(&c)), None, "not before a whole window");
        }
        exchange(&mut c, 800_000, 3000, 9000);
        assert_eq!((c.n, c.offset(ms(&c))), (CLOCK_SAMPLES, Some(5000)));
        // the window forgets: eight slow samples later the fast one is gone
        for k in 0..CLOCK_SAMPLES as u64 {
            exchange(&mut c, 1_000_000 * (k + 1), 1000, 3000);
        }
        assert_eq!(c.offset(ms(&c)), Some(4000));
        exchange(&mut c, 9_000_000, 1000, 5000);
        assert_eq!(c.offset(ms(&c)), Some(4000), "a slower sample does not win");
        let n = c.n;
        // and by time: 15 s without answers (sync off, pinging paused) and none counts
        assert_eq!(c.offset(ms(&c) + 1000.0), Some(4000));
        assert_eq!(c.offset(ms(&c) + CLOCK_MAX_AGE_MS + 1.0), None);

        // forged or broken PONGs change nothing:
        // a replay of an answered one, one for a PING never sent, one with the wrong echo
        let at = ms(&c);
        c.on_pong(&first, 10_000_000, at);
        c.on_pong(&Pong { id: 999, t1: 1, t2: u64::MAX / 2, t3: u64::MAX / 2 }, 1, at);
        let id = c.ping(20_000_000, 0.0).id;
        c.on_pong(&Pong { id, t1: 20_000_001, t2: 1, t3: 1 }, 20_000_001, at);
        // zero round trip, but a hold time beyond reason; a round trip beyond reason
        let id = c.ping(30_000_000, 0.0).id;
        c.on_pong(&Pong { id, t1: 30_000_000, t2: 1, t3: 3_000_001 }, 33_000_000, at);
        let id = c.ping(40_000_000, 0.0).id;
        c.on_pong(&Pong { id, t1: 40_000_000, t2: 7, t3: 7 }, 45_000_000, at);
        // nonsense order
        let id = c.ping(50, 0.0).id;
        c.on_pong(&Pong { id, t1: 50, t2: 20, t3: 15 }, 60, at);
        assert_eq!((c.n, c.offset(at)), (n, Some(4000)));
    }

    /// A 1.5 s round trip (within [`MAX_PING_RTT_US`]) still gives samples at the fast rate:
    /// every answer finds its PING outstanding. A host that never answers gets the fast PINGs
    /// once, then one a second.
    #[test]
    fn slow_round_trips_count_and_silence_backs_off() {
        // host clock = ours + 7 ms; 1.5 s round trip; time steps of 10 ms
        let mut c = Clock::default();
        let mut flight = std::collections::VecDeque::new();
        let mut synced_at = None;
        for step in 0..2000u64 {
            let ms = step as f64 * 10.0;
            let us = step * 10_000;
            if c.ping_due(ms) {
                let p = c.ping(us, ms);
                let t2 = us + 750_000 + 7_000;
                flight.push_back((us + 1_500_000, Pong { id: p.id, t1: p.t1, t2, t3: t2 }));
            }
            while flight.front().is_some_and(|f| f.0 <= us) {
                let (t4, pong) = flight.pop_front().unwrap();
                c.on_pong(&pong, t4, ms);
            }
            if synced_at.is_none() && c.offset(ms).is_some() {
                synced_at = Some(ms);
            }
        }
        assert_eq!(c.offset(20_000.0), Some(7_000));
        // eight answers to PINGs 200 ms apart, the first after 1.5 s
        assert!(synced_at.is_some_and(|t| t < 3500.0), "{synced_at:?}");
        assert!(c.pings < 16 + 20, "{}", c.pings);

        let mut silent = Clock::default();
        for step in 0..6000u64 {
            let ms = step as f64 * 10.0;
            if silent.ping_due(ms) {
                silent.ping(step * 10_000, ms);
            }
        }
        // 16 fast ones (3.2 s), then one a second for the rest of the minute
        assert_eq!(silent.pings, 16 + 56, "{}", silent.pings);
    }

    /// PINGs every 10 ms steps of the engine clock for `secs`, `loss` (of 10) PONGs lost
    /// (50 ms round trip), answers stopping at `quiet` s: the share of the time from 10 s to
    /// `quiet` without an offset, the PINGs sent until `quiet` and after.
    fn pinging(loss: u64, secs: u64, quiet: f64) -> (f64, u32, u32) {
        let mut c = Clock::default();
        let mut flight = std::collections::VecDeque::new();
        let mut rng = 0x9e37_79b9_7f4a_7c15u64;
        let (mut short, mut steps, mut at_quiet) = (0, 0, None);
        for step in 0..secs * 100 {
            let ms = step as f64 * 10.0;
            let us = step * 10_000;
            let answers = ms < quiet * 1e3;
            if !answers && at_quiet.is_none() {
                at_quiet = Some(c.pings);
            }
            if c.ping_due(ms) {
                let p = c.ping(us, ms);
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                if answers && rng % 10 >= loss {
                    let t2 = us + 25_000 + 7_000;
                    flight.push_back((us + 50_000, Pong { id: p.id, t1: p.t1, t2, t3: t2 }));
                }
            }
            while flight.front().is_some_and(|f| f.0 <= us) {
                let (t4, pong) = flight.pop_front().unwrap();
                c.on_pong(&pong, t4, ms);
            }
            if ms >= 10_000.0 && answers {
                steps += 1;
                short += usize::from(c.offset(ms).is_none());
            }
        }
        let q = at_quiet.unwrap_or(c.pings);
        (short as f64 / f64::from(steps.max(1)), q, c.pings - q)
    }

    /// 30 % of the PONGs lost (50 ms round trip): samples age out faster than one PING a
    /// second refills them, so whenever the window falls short a burst of fast PINGs refills
    /// it. The offset is there all but a sliver of the time (at one PING a second it was
    /// missing 3.9 % of the time), at about the slow rate's PINGs. A host that answered and
    /// then stopped gets fast PINGs for at most 3.2 s more.
    #[test]
    fn lost_pongs_bring_the_fast_rate_back() {
        let (missing, pings, after) = pinging(3, 600, 590.0);
        eprintln!("30 % lost: offset missing {:.3} % of the time, {pings} PINGs in 590 s", missing * 100.0);
        assert!(missing < 0.005, "{missing}");
        assert!(pings < 2 * 590, "{pings}");
        assert!(after <= 16 + 10, "{after}");
    }

    /// Most PONGs lost (70 %, 90 %): the window rarely fills, and the fast bursts come at most
    /// one per 15 s: about two PINGs a second on average, not five.
    #[test]
    fn mostly_lost_pongs_do_not_keep_the_fast_rate() {
        for loss in [7, 9] {
            let (missing, pings, _) = pinging(loss, 600, 600.0);
            eprintln!("{loss}0 % lost: offset missing {:.1} % of the time, {pings} PINGs in 600 s", missing * 100.0);
            assert!(pings <= 600 + 16 * (600 / 15 + 1), "{pings}");
        }
    }

    /// The due time from a host PLAYOUT time: our clock is the host's minus the offset (a sign
    /// error here moves every target by twice the offset), the skew is our clock's own; absurd
    /// values give no target instead of panicking.
    #[test]
    fn due_times_and_hostile_values() {
        let epoch = Instant::now();
        let ms = Duration::from_millis;
        // host clock 3 s ahead of ours, our clock 1 s ahead of `epoch`
        let off = 3_000_000;
        let due = due_at(epoch, 1_000_000, 4_500_000 + off as u64, off, ms(200), ms(5));
        assert_eq!(due, Some(epoch + ms(3500 + 200 - 5)));
        // the host's time before our epoch
        let due = due_at(epoch + ms(10_000), 0, 1_000_000, 2_000_000, ms(100), ms(0));
        assert_eq!(due, Some(epoch + ms(9000 + 100)));
        for (t, off) in [(u64::MAX, i64::MIN), (u64::MAX, 0), (0, i64::MAX), (0, 0)] {
            let _ = due_at(epoch, u64::MAX, t, off, ms(1000), ms(5));
        }
        assert_eq!(due_at(epoch, 0, u64::MAX, i64::MIN, ms(1), ms(0)), None);

        // turning sync off needs no anchor (a format change or device setup just cleared it)
        let off = playout_frame(None, 1, None).expect("off goes out without an anchor");
        assert_eq!((off.stream, off.delay_ms, accept_playout(off)), (1, 0, None));
        assert_eq!(playout_frame(Some(150), 0, None), None);
        let on = playout_frame(Some(150), 0, Some((3, 7, 99)));
        assert_eq!(on.map(|p| (p.generation, p.seq, p.time_us, p.delay_ms)), Some((3, 7, 99, 150)));
        let pl = |delay_ms| Playout { stream: 0, generation: 1, seq: 2, time_us: 3, delay_ms };
        assert_eq!(accept_playout(pl(0)), None);
        assert_eq!(accept_playout(pl(150)), Some(pl(150)));
        assert_eq!(accept_playout(pl(u16::MAX)), Some(pl(MAX_PLAYOUT_MS)));
    }

    /// A subscription is kept across a re-SUBSCRIBE only while its levels are still those of
    /// the stream (a format change rebuilds it: stale levels would name encodings that are gone).
    #[test]
    fn a_subscription_with_stale_levels_is_rebuilt() {
        let sub = |levels: Vec<u32>| Sub {
            stream: MAIN_STREAM,
            adapt: Adapt::new(Instant::now(), levels.clone()),
            decision: Decision::default(),
            levels,
            ladder: false,
        };
        let s = sub(vec![96_000, 32_000]);
        assert!(s.fits(MAIN_STREAM, &[96_000, 32_000]));
        assert!(!s.fits(MAIN_STREAM, &[128_000]));
        assert!(!s.fits(PRIVATE_STREAM, &[96_000, 32_000]));
    }
}
