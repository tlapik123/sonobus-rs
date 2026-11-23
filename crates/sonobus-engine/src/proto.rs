//! The seam between the engine and its session protocols. A session is either the legacy one
//! (`legacy`: AOO + `/sb` over one UDP socket, wire-compatible with SonoBus 1.7) or, with the
//! `modern` feature, the protocol v2 one (`modern`: `sonobus_net` over iroh/QUIC). Both keep
//! their peers as a [`PeerCore`] (what the mixer slot, the jitter controller, the latency maths,
//! recording, the peer cache and the UI need) plus their transport's own state, and implement
//! [`Proto`]: the transport primitives as required methods, everything protocol-neutral (the
//! `/sb` message handlers, chat, jitter buffer control, latency match, receive sync, peer cache,
//! status) as provided methods written once here.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use sonobus_core::sbmsg::{self, LatEntry, PeerInfo, SbMessage};
use sonobus_core::valuetree::ValueTree;

use crate::formats::{CODEC_IDS, FORMATS, SendCodec};
use crate::jitter::{self, Jitter, JitterMode, Ping, RemoteInfo};
use crate::latency::{match_buffer, receive_sync};
use crate::mixer::{MAX_CHANNELS, MAX_PEERS, PeerShared};
use crate::options::{self, PeerSolo};
use crate::recording::PeerRec;
use crate::{ChatMessage, Event, Host, Notification, PeerId, PeerNet, PeerStatus};

/// Chat history kept in memory (adopted from Commsbus).
pub(crate) const CHAT_HISTORY: usize = 500;
/// `/sb/latinfo` entries kept at most (a peer flooding replies cannot grow it further).
const LAT_INFO_MAX: usize = 4096;
/// A peer's latency-match and group suggestions reach the app at most this often each.
const SUGGEST_INTERVAL_MS: f64 = 1000.0;
/// Our send layout goes out at most this often (bursts of structural edits coalesce).
const LAYOUT_SEND_INTERVAL_MS: f64 = 250.0;
/// A peer's received layout reaches its mixer strip at most this often.
const LAYOUT_APPLY_INTERVAL_MS: f64 = 100.0;
/// Connected peers' settings go to the peer cache this often (PR #291: not only on leaving).
const CACHE_COMMIT_INTERVAL_MS: f64 = 1000.0;

/// Session-level state both protocols keep.
pub(crate) struct Common {
    /// Our name as peers see it.
    pub user: String,
    /// Group (legacy) or session (modern) name.
    pub group: Option<String>,
    pub chat: VecDeque<ChatMessage>,
    /// Group latency match: the `/sb/latinfo` entries collected (ours first).
    pub lat_info: Vec<LatEntry>,
    pub lat_match_pending: bool,
    /// Our layout changed and goes out to every peer (rate-limited).
    pub layout_due: bool,
    pub last_layout_ms: f64,
    pub last_cache_commit_ms: f64,
}

impl Common {
    pub fn new() -> Self {
        Self {
            user: String::new(),
            group: None,
            chat: VecDeque::new(),
            lat_info: Vec::new(),
            lat_match_pending: false,
            layout_due: false,
            last_layout_ms: f64::NEG_INFINITY,
            last_cache_commit_ms: 0.0,
        }
    }
}

/// One peer as the engine sees it, whatever carries its audio.
pub(crate) struct PeerCore {
    pub id: PeerId,
    pub slot: usize,
    pub user: String,
    pub group: String,
    pub send_allow: bool,
    pub recv_allow: bool,
    pub send_allow_cache: bool,
    pub recv_allow_cache: bool,
    pub send_active: bool,
    pub recv_active: bool,
    pub connected: bool,
    /// Our send format for it ([`FORMATS`] index).
    pub format_index: usize,
    pub requested_format: Option<usize>,
    /// PR #201 `supportedcodecs` from its last `/sb/pinfo` (`None`: none sent, legacy set).
    pub codecs: Option<Vec<String>>,
    pub send_channels: usize,
    pub nominal_send_channels: i32,
    /// `sendChannelsOverride`.
    pub send_override: Option<usize>,
    pub jitter: Jitter,
    pub ping: Ping,
    pub remote: RemoteInfo,
    pub blocked_us: bool,
    pub packets_received: i64,
    pub packets_sent: i64,
    pub bytes_sent: u64,
    pub bytes_received: u64,
    rate_mark: (f64, u64, u64),
    pub send_bps: f64,
    pub recv_bps: f64,
    /// A received layout waits for its rate-limited publish (`LAYOUT_APPLY_INTERVAL_MS`).
    pub layout_due: bool,
    last_layout_ms: f64,
    pub shared: Arc<PeerShared>,
    pub lat_test_active: bool,
    /// Last latency-test result (round trip ms).
    pub measured_ms: Option<f32>,
    /// Buffer added by the last receive sync (taken out again before the next one).
    pub sync_pad_ms: f32,
    /// We sent it `/sb/reqlatinfo` and wait for its one `/sb/latinfo`.
    pub lat_requested: bool,
    /// When its last `/sb/suggestlat` / `/sb/suggestgroup` became a notification (ms).
    last_suggest_ms: [f64; 2],
}

impl PeerCore {
    /// A new peer in a free mixer slot (`doAddRemotePeerIfNecessary`, P.cpp:5959-6136): jitter
    /// mode, buffer and send format from the peer cache or the defaults, a mixer strip.
    /// `None` (and an error event naming `what`) when every slot is taken.
    pub fn add(h: &mut Host, npeers: usize, user: &str, group: &str, what: &str) -> Option<Self> {
        Self::add_keyed(h, npeers, user, user, group, what)
    }

    /// [`Self::add`] with the peer cache (and the remembered solo flags) kept under
    /// `cache_key` instead of `user` (a modern peer: its endpoint id).
    pub fn add_keyed(
        h: &mut Host,
        npeers: usize,
        user: &str,
        cache_key: &str,
        group: &str,
        what: &str,
    ) -> Option<Self> {
        let Some(slot) = h.model.free_slot().filter(|_| npeers < MAX_PEERS) else {
            h.event(Event::Error(format!("too many peers, ignoring {what}")));
            return None;
        };
        let params = &h.model.state.params;
        let cache = h.model.find_cache(cache_key).cloned();
        let (mode, buffer, format) = match &cache {
            Some(c) => (
                JitterMode::from_i32(c.netbufauto),
                c.netbuf,
                crate::formats::checked(c.send_format),
            ),
            None => (
                JitterMode::from_i32(params.def_net_auto),
                params.def_net_buf * 1000.0,
                crate::formats::checked(params.def_send_qual),
            ),
        };
        let now_ms = h.now_ms();
        let jitter = Jitter::new(mode, buffer, now_ms);
        let nominal = params.send_channels;
        let send_channels = if nominal <= 0 {
            h.model.total_send_channels().max(1)
        } else {
            nominal as usize
        };
        let (send_allow, recv_allow) = (!params.main_send_mute, !params.main_recv_mute);
        let shared = {
            let tx = &mut h.tx;
            h.model
                .add_peer(slot, user, cache.as_ref(), &mut |c| tx.mix(c))
        };
        if let Some(m) = h.model.peers[slot].as_mut() {
            m.key = cache_key.to_owned();
        }
        Some(Self {
            id: h.new_peer_id(),
            slot,
            user: user.to_owned(),
            group: group.to_owned(),
            send_allow,
            recv_allow,
            send_allow_cache: true,
            recv_allow_cache: true,
            send_active: false,
            recv_active: false,
            connected: false,
            format_index: format,
            requested_format: None,
            codecs: None,
            send_channels,
            nominal_send_channels: nominal,
            send_override: None,
            jitter,
            ping: Ping::default(),
            remote: RemoteInfo::default(),
            blocked_us: false,
            packets_received: 0,
            packets_sent: 0,
            bytes_sent: 0,
            bytes_received: 0,
            rate_mark: (now_ms, 0, 0),
            send_bps: 0.0,
            recv_bps: 0.0,
            layout_due: false,
            last_layout_ms: f64::NEG_INFINITY,
            shared,
            lat_test_active: false,
            measured_ms: None,
            sync_pad_ms: 0.0,
            lat_requested: false,
            last_suggest_ms: [f64::NEG_INFINITY; 2],
        })
    }

    /// The flags the audio thread reads.
    pub fn sync_shared(&self) {
        self.shared
            .recv_active
            .store(self.recv_active, Ordering::Relaxed);
        self.shared
            .safety_muted
            .store(self.jitter.safety_muted, Ordering::Relaxed);
    }

    pub fn latency(&self, block_ms: f32, recv_opus: bool) -> jitter::LatencyInfo {
        let send_opus = matches!(FORMATS[self.format_index].codec, SendCodec::Opus { .. });
        jitter::latency(
            &self.jitter,
            &self.ping,
            &self.remote,
            block_ms,
            send_opus,
            recv_opus,
        )
    }

    /// A data packet of its stream arrived.
    pub fn got_data(&mut self, ms: f64) {
        self.packets_received += 1;
        if self.recv_allow && !self.recv_active {
            self.recv_active = true;
        }
        if self.jitter.safety_muted {
            self.jitter.on_packet(ms);
        }
        self.sync_shared();
    }

    /// Timer work: a received layout reaches the mixer (rate-limited), bit rates.
    pub fn tick(&mut self, h: &mut Host, ms: f64) {
        if self.layout_due && ms - self.last_layout_ms >= LAYOUT_APPLY_INTERVAL_MS {
            self.layout_due = false;
            self.last_layout_ms = ms;
            h.publish_peer(self.slot);
            h.event(Event::PeerChanged { id: self.id });
        }
        let (t0, s0, r0) = self.rate_mark;
        if ms - t0 >= 1000.0 {
            let dt = (ms - t0) / 1000.0;
            self.send_bps = 8.0 * (self.bytes_sent - s0) as f64 / dt;
            self.recv_bps = 8.0 * (self.bytes_received - r0) as f64 / dt;
            self.rate_mark = (ms, self.bytes_sent, self.bytes_received);
        }
    }

    pub fn peer_rec(&self, h: &Host, recv_opus: bool) -> PeerRec {
        PeerRec {
            slot: self.slot,
            user: self.user.clone(),
            recv_channels: h.model.peers[self.slot]
                .as_ref()
                .map_or(0, |m| m.params.recv_channels),
            estimated_ms: self.latency(h.cfg.block_ms(), recv_opus).round_trip_ms,
            measured_ms: self.measured_ms,
        }
    }
}

/// A peer-supplied name as the engine keeps it: no control characters (upstream allows any
/// text; ours end up in file names, the latency sidecar's tab-separated lines and the
/// separator-delimited remembered-solo option).
pub(crate) fn clean_name(name: &str) -> String {
    name.chars().filter(|c| !c.is_control()).collect()
}

/// `commitCacheForPeer`, plus the solo flags (PR #291).
pub(crate) fn commit_cache_of(h: &mut Host, p: &PeerCore) {
    let net = (
        p.jitter.buffer_ms,
        p.jitter.mode as i32,
        p.format_index as i32,
    );
    h.model.commit_cache(p.slot, net);
    if let Some(m) = h.model.peers[p.slot].as_ref().filter(|m| !m.key.is_empty()) {
        let solo = PeerSolo {
            soloed: m.params.soloed,
            groups: m.params.group_solo.clone(),
        };
        let key = m.key.clone();
        options::set_peer_solo(&mut h.model.state.extra, &key, &solo);
    }
}

/// A known peer's remembered solo flags (by its cache key), if "Remember Peer Solo" is on.
pub(crate) fn restore_solo(h: &mut Host, slot: usize, user: &str) {
    if user.is_empty()
        || !options::get_bool(&h.model.state.extra, options::REMEMBER_PEER_SOLO, true)
    {
        return;
    }
    let Some((_, solo)) = options::peer_solos(&h.model.state.extra)
        .into_iter()
        .find(|(n, _)| n == user)
    else {
        return;
    };
    if let Some(m) = h.model.peers[slot].as_mut() {
        m.params.soloed = solo.soloed;
        for (g, s) in m.params.group_solo.iter_mut().zip(solo.groups) {
            *g = s;
        }
    }
}

pub(crate) fn set_model_send_channels(h: &mut Host, slot: usize, n: usize) {
    if let Some(m) = h.model.peers[slot].as_mut()
        && m.params.send_channels != n
    {
        m.params.send_channels = n;
        h.publish_peer(slot);
    }
}

/// What both protocols' peer tables offer the engine. Indices are positions in the peer table
/// and are only valid until the table changes.
pub(crate) trait Proto {
    // ---- transport primitives (required) ---------------------------------------------------

    fn common(&self) -> &Common;
    fn common_mut(&mut self) -> &mut Common;
    fn len(&self) -> usize;
    fn peer(&self, i: usize) -> &PeerCore;
    fn peer_mut(&mut self, i: usize) -> &mut PeerCore;
    /// An `/sb/...` message to peer `i` (its bytes counted as sent to it).
    fn send_sb(&mut self, i: usize, m: &SbMessage<'_>);
    /// Our send layout (`/sb/clayinfo`) to peer `i`.
    fn send_layout(&mut self, h: &Host, i: usize);
    /// Peer `i`'s jitter buffer (`jitter.buffer_ms`) to its receive stream(s).
    fn push_buffer(&mut self, i: usize);
    /// Peer `i`'s send format (`format_index`) to its send stream(s).
    fn push_send_format(&mut self, h: &mut Host, i: usize);
    /// `setRemotePeerSendActive`.
    fn set_send_active(&mut self, i: usize, active: bool);
    /// `setRemotePeerRecvActive`.
    fn set_recv_active(&mut self, i: usize, active: bool);
    /// `updateRemotePeerSendChannels` for every peer, then the layout to everyone.
    fn update_send_channels(&mut self, h: &mut Host);
    /// Remove peer `i` (telling it it is blocked when `block`): transport cleanup, then
    /// [`Proto::forget_peer`].
    fn remove_peer_at(&mut self, h: &mut Host, i: usize, block: bool);
    /// Remove peer `i`, tell it it is blocked, and ignore it from now on.
    fn block_peer_at(&mut self, h: &mut Host, i: usize);
    /// Ask peer `i` to send us this format (`None`: no preference).
    fn request_format(&mut self, h: &Host, i: usize, index: Option<usize>);
    fn set_dynamic_resampling(&mut self, on: bool);
    /// PR #202: send every packet `n` times.
    fn set_redundancy(&mut self, n: i32);
    fn start_latency_test(&mut self, i: usize) -> bool;
    fn stop_latency_test(&mut self, i: usize);
    /// Peer `i` sends us Opus (2.5 ms codec latency in the estimates).
    fn recv_opus(&self, i: usize) -> bool;
    /// What peer `i` sends us, by table name when it is one of the table's formats.
    fn recv_format_name(&self, i: usize) -> Option<String>;
    fn addr(&self, i: usize) -> SocketAddr;
    /// Protocol v2 details of peer `i`.
    fn peer_net(&self, _i: usize) -> Option<PeerNet> {
        None
    }
    /// The peer a `/sb/clayinfo` from peer `i` naming stream `source_id` is for.
    fn layout_target(&self, i: usize, _source_id: i32) -> Option<usize> {
        Some(i)
    }
    /// Broadcast hub: a peer's replies must not reveal the other peers.
    fn hides_peers(&self) -> bool {
        false
    }

    // ---- shared logic (provided) -------------------------------------------------------------

    fn peer_index(&self, id: PeerId) -> Option<usize> {
        (0..self.len()).find(|&i| self.peer(i).id == id)
    }

    /// The bookkeeping of a peer just added to the table at `i`.
    fn peer_added(&mut self, h: &mut Host, i: usize) {
        let p = self.peer(i);
        p.sync_shared();
        let (slot, id, user, group, n) = (
            p.slot,
            p.id,
            p.user.clone(),
            p.group.clone(),
            p.send_channels,
        );
        let key = h.model.peers[slot].as_ref().map(|m| m.key.clone());
        restore_solo(h, slot, &key.unwrap_or_default());
        set_model_send_channels(h, slot, n);
        if !user.is_empty() {
            let rec = self.peer_rec(h, i);
            h.with_rec(|r, c| r.peer_joined(c, &rec));
        }
        h.event(Event::PeerJoined { id, user, group });
    }

    /// The protocol-neutral part of removing a peer (already out of the table): cache, recorder,
    /// patchbay routes, mixer strip, event.
    fn forget_peer(&mut self, h: &mut Host, p: &PeerCore) {
        commit_cache_of(h, p);
        h.with_rec(|r, c| r.peer_left(c, p.slot));
        // patchbay: the slot's routes go (`adjustRemoteSendMatrix`, P.cpp:4578-4619)
        let bit = 1u32 << p.slot;
        if h.model.routed_to[p.slot] != 0 || h.model.routed_to.iter().any(|r| r & bit != 0) {
            h.model.routed_to[p.slot] = 0;
            for r in &mut h.model.routed_to {
                *r &= !bit;
            }
            h.publish();
            self.update_send_channels(h);
        }
        let tx = &mut h.tx;
        h.model.remove_peer(p.slot, &mut |c| tx.mix(c));
        h.event(Event::PeerLeft {
            id: p.id,
            user: p.user.clone(),
        });
    }

    fn remove_peer(&mut self, h: &mut Host, id: PeerId, block: bool) {
        if let Some(i) = self.peer_index(id) {
            self.remove_peer_at(h, i, block);
        }
    }

    fn remove_all_peers(&mut self, h: &mut Host) {
        while self.len() > 0 {
            self.remove_peer_at(h, self.len() - 1, false);
        }
    }

    /// The jitter buffer changed: tell the receive stream, the peer (`/sb/pinfo`) and the audio
    /// side.
    fn apply_buffer(&mut self, h: &mut Host, i: usize) {
        self.push_buffer(i);
        self.peer(i).sync_shared();
        self.send_peer_info(h, i);
    }

    /// `/sb/pinfo` (`sendRemotePeerInfoUpdate`, P.cpp:3174-3222).
    fn send_peer_info(&mut self, h: &Host, i: usize) {
        let block_ms = f64::from(h.cfg.block_ms());
        let info = PeerInfo {
            jitbuf_ms: Some(f64::from(self.peer(i).jitter.effective_ms(block_ms as f32))),
            in_latency_ms: Some(block_ms),
            out_latency_ms: Some(block_ms),
            recording: Some(h.rec.is_recording()),
            net_type: None,
            supported_codecs: Some(CODEC_IDS.map(str::to_owned).into()),
        };
        self.send_sb(i, &SbMessage::PeerInfo(info));
    }

    /// `/sb/pinfo` to everyone (recording started or stopped).
    fn send_peer_info_all(&mut self, h: &Host) {
        for i in 0..self.len() {
            self.send_peer_info(h, i);
        }
    }

    /// Our layout to everyone, at most every `LAYOUT_SEND_INTERVAL_MS`.
    fn send_layout_if_due(&mut self, h: &Host, ms: f64) {
        let c = self.common_mut();
        if c.layout_due && ms - c.last_layout_ms >= LAYOUT_SEND_INTERVAL_MS {
            c.layout_due = false;
            c.last_layout_ms = ms;
            for i in 0..self.len() {
                self.send_layout(h, i);
            }
        }
    }

    /// Every connected peer's settings to the cache, every `CACHE_COMMIT_INTERVAL_MS`.
    fn commit_caches_if_due(&mut self, h: &mut Host, ms: f64) {
        if ms - self.common().last_cache_commit_ms >= CACHE_COMMIT_INTERVAL_MS {
            self.common_mut().last_cache_commit_ms = ms;
            self.commit_caches(h);
        }
    }

    /// `setRemotePeerSendAllow` (non-cached).
    fn set_send_allow(&mut self, i: usize, allow: bool) {
        self.peer_mut(i).send_allow = allow;
        if !allow {
            self.set_send_active(i, false);
        }
    }

    /// `setRemotePeerRecvAllow` (non-cached).
    fn set_recv_allow(&mut self, i: usize, allow: bool) {
        self.peer_mut(i).recv_allow = allow;
        if !allow {
            self.set_recv_active(i, false);
        }
    }

    /// The master send/receive mutes (`parameterChanged`, P.cpp:6728-6803).
    fn on_main_mutes(&mut self, send: Option<bool>, recv: Option<bool>) {
        for i in 0..self.len() {
            let p = self.peer(i);
            if !p.connected && p.user.is_empty() {
                continue;
            }
            match send {
                Some(false) => {
                    if self.peer(i).send_allow_cache {
                        self.set_send_active(i, true);
                    } else {
                        self.set_send_allow(i, false);
                    }
                }
                Some(true) => {
                    let p = self.peer_mut(i);
                    p.send_allow_cache = p.send_allow;
                    self.set_send_allow(i, false);
                }
                None => {}
            }
            match recv {
                Some(false) => {
                    let cached = self.peer(i).recv_allow_cache;
                    self.set_recv_active(i, cached);
                }
                Some(true) => {
                    let p = self.peer_mut(i);
                    p.recv_allow_cache = p.recv_allow;
                    self.set_recv_allow(i, false);
                }
                None => {}
            }
        }
    }

    /// `setRemotePeerAudioCodecFormat`. PR #201: a format the peer did not list as decodable
    /// becomes the nearest one it did; none at all leaves the format as it is.
    fn set_send_format(&mut self, h: &mut Host, i: usize, index: usize) {
        let index = index.min(FORMATS.len() - 1);
        let p = self.peer_mut(i);
        let Some(index) = crate::formats::nearest_supported(p.codecs.as_deref(), index) else {
            return;
        };
        p.format_index = index;
        self.push_send_format(h, i);
    }

    fn set_buffer(&mut self, h: &mut Host, i: usize, ms: f32) {
        let now = h.now_ms();
        let p = self.peer_mut(i);
        p.sync_pad_ms = 0.0;
        p.jitter.set_buffer(ms.max(0.0), now, h.cfg.block_ms());
        self.apply_buffer(h, i);
    }

    fn set_jitter_mode(&mut self, h: &mut Host, i: usize, mode: JitterMode) {
        let now = h.now_ms();
        let _ = self
            .peer_mut(i)
            .jitter
            .set_mode(mode, now, h.cfg.block_ms());
        self.apply_buffer(h, i);
    }

    fn reset_drops(&mut self, h: &Host, i: usize) {
        let now = h.now_ms();
        self.peer_mut(i).jitter.reset_stats(now);
    }

    /// `setRemotePeerOverrideSendChannelCount`.
    fn set_send_override(&mut self, h: &mut Host, i: usize, n: Option<usize>) {
        self.peer_mut(i).send_override = n.filter(|&n| n > 0);
        self.update_send_channels(h);
    }

    /// `updateRemotePeerSendChannels` (P.cpp:5210-5235): the override (capped at our output
    /// channels), else our output channel count when the patchbay routes anything to the peer,
    /// else the send mode.
    fn compute_send_channels(&self, h: &Host, i: usize, nominal: i32) -> usize {
        let p = self.peer(i);
        let outs = h.cfg.output_channels.clamp(1, MAX_CHANNELS);
        if let Some(o) = p.send_override {
            o.clamp(1, outs)
        } else if h.model.routed_to[p.slot] != 0 {
            outs
        } else if nominal <= 0 {
            h.model.total_send_channels().max(1)
        } else {
            nominal as usize
        }
    }

    // ---- chat --------------------------------------------------------------------------------

    /// Our chat message to the named users (everyone when `targets` is empty).
    fn chat_message(&self, targets: &[&str], text: &str) -> ChatMessage {
        let c = self.common();
        ChatMessage {
            own: true,
            group: c.group.clone().unwrap_or_default(),
            from: c.user.clone(),
            targets: targets.join("|"),
            message: text.to_owned(),
        }
    }

    /// `sendChatEvent`, then the message goes into the history. A message that does not fit
    /// one packet is neither sent nor recorded (upstream records it although nothing went out).
    fn send_chat(&mut self, msg: &ChatMessage) -> Result<(), String> {
        let to_all = msg.targets.is_empty();
        let names: Vec<String> = (0..self.len()).map(|i| self.peer(i).user.clone()).collect();
        self.send_chat_where(msg, &|i| {
            to_all || msg.targets.split('|').any(|t| t == names[i])
        })
    }

    /// [`Self::send_chat`] to the peers `to` selects (by table index).
    fn send_chat_where(
        &mut self,
        msg: &ChatMessage,
        to: &dyn Fn(usize) -> bool,
    ) -> Result<(), String> {
        let m = SbMessage::Chat {
            group: &msg.group,
            from: &msg.from,
            targets: &msg.targets,
            tags: "",
            message: &msg.message,
        };
        if m.encode(&mut Vec::new()).is_err() {
            return Err(format!(
                "chat message too long ({} bytes), not sent",
                msg.message.len()
            ));
        }
        for i in 0..self.len() {
            if to(i) {
                self.send_sb(i, &m);
            }
        }
        self.push_chat(msg.clone());
        Ok(())
    }

    fn push_chat(&mut self, m: ChatMessage) {
        let chat = &mut self.common_mut().chat;
        if chat.len() >= CHAT_HISTORY {
            chat.pop_front();
        }
        chat.push_back(m);
    }

    // ---- /sb messages --------------------------------------------------------------------------

    /// An `/sb/...` message from peer `pi` (`handleOtherMessage`, P.cpp:2561-2859), the sender
    /// established by the transport (never by the message body). `raw` is the whole message.
    /// `/sb/ping` and `/sb/pngack` are the transport's.
    fn handle_sb(&mut self, h: &mut Host, pi: usize, m: SbMessage<'_>, raw: &[u8]) {
        match m {
            SbMessage::Ping { .. } | SbMessage::PingAck { .. } => {}
            SbMessage::PeerInfo(info) => {
                let p = self.peer_mut(pi);
                let r = &mut p.remote;
                for (v, d) in [
                    (info.jitbuf_ms, &mut r.jitter_buffer_ms),
                    (info.in_latency_ms, &mut r.in_latency_ms),
                    (info.out_latency_ms, &mut r.out_latency_ms),
                ] {
                    if let Some(v) = v.and_then(jitter::peer_ms) {
                        *d = v;
                    }
                }
                if let Some(v) = info.net_type {
                    r.net_type = v;
                }
                if let Some(v) = info.recording {
                    r.recording = v;
                }
                r.valid = true;
                // PR #201: what it decodes; our send format follows if it is not among it
                if p.codecs != info.supported_codecs {
                    p.codecs = info.supported_codecs;
                    let fi = p.format_index;
                    if !crate::formats::supports(p.codecs.as_deref(), fi) {
                        self.set_send_format(h, pi, fi);
                    }
                    h.event(Event::PeerChanged {
                        id: self.peer(pi).id,
                    });
                }
            }
            SbMessage::ChannelLayoutInfo { source_id, .. } => {
                // re-read the raw tree: upstream overlays it on the current groups
                let tree = aoo::osc::decode(raw)
                    .ok()
                    .and_then(|m| m.args().nth(1))
                    .and_then(|a| match a {
                        aoo::osc::Arg::Blob(b) => ValueTree::read(b).ok(),
                        _ => None,
                    });
                if let (Some(i), Some(tree)) = (self.layout_target(pi, source_id), tree) {
                    let slot = self.peer(i).slot;
                    if let Some(pm) = h.model.peers[slot].as_mut() {
                        pm.recvd_layout = true;
                    }
                    h.model.apply_layout(slot, &tree);
                    // the mixer snapshot (and the UI event) follow from the tick,
                    // rate-limited: a peer flooding layouts costs one snapshot per interval
                    self.peer_mut(i).layout_due = true;
                }
            }
            // the sender's name as the transport knows it (server-announced or
            // self-declared at connect), not the body's: a peer cannot pose as another
            SbMessage::Chat {
                group,
                from,
                targets,
                message,
                ..
            } => {
                let msg = ChatMessage {
                    own: false,
                    group: group.into(),
                    from: self.sender_name(pi, from),
                    targets: targets.into(),
                    message: message.into(),
                };
                self.push_chat(msg.clone());
                h.event(Event::Chat(msg));
            }
            SbMessage::ReqLatInfo => {
                let mut entries = self.own_lat_info(h);
                if self.hides_peers() {
                    // its own entry only (entries are by peer index, never by name)
                    entries = entries.into_iter().skip(pi).take(1).collect();
                }
                self.send_sb(pi, &SbMessage::LatInfo(entries));
            }
            SbMessage::BlockedInfo {
                from: user,
                blocked,
            } => {
                self.peer_mut(pi).blocked_us = blocked;
                // upstream also stops sending on an *un*block notice, which every stock
                // client sends right when it connects and which can race the invite that
                // starts our source; we only stop on a real block
                if blocked {
                    self.set_send_active(pi, false);
                }
                h.event(Event::BlockedByPeer {
                    id: self.peer(pi).id,
                    user: self.sender_name(pi, user),
                    blocked,
                });
            }
            // `handleLatInfo`: appended, no dedupe (as upstream); one reply per request
            SbMessage::LatInfo(entries) => {
                if !std::mem::take(&mut self.peer_mut(pi).lat_requested) {
                    return;
                }
                let c = self.common_mut();
                let room = LAT_INFO_MAX.saturating_sub(c.lat_info.len());
                let sane = entries.into_iter().filter_map(|e| {
                    let ms = jitter::peer_ms(e.latency_ms)?;
                    Some(LatEntry {
                        latency_ms: f64::from(ms),
                        ..e
                    })
                });
                c.lat_info.extend(sane.take(room));
                if self.common().lat_match_pending && self.latency_match_ready() {
                    self.common_mut().lat_match_pending = false;
                    h.note(Notification::LatencyMatchReady);
                }
            }
            // the sender is the peer the message came from (its name as the transport reported
            // it), not the name the message body claims: a peer cannot pose as another
            SbMessage::SuggestLatency { from, latency_ms } => {
                if let Some(latency_ms) = jitter::peer_ms(latency_ms.into())
                    && self.suggestion_due(h, pi, 0)
                {
                    h.note(Notification::LatencyMatchRequested {
                        from: self.sender_name(pi, from),
                        latency_ms,
                    });
                }
            }
            SbMessage::SuggestGroup(mut g) => {
                if self.suggestion_due(h, pi, 1) {
                    g.user = self.sender_name(pi, &g.user);
                    h.note(Notification::GroupSuggested(g));
                }
            }
        }
    }

    /// Who peer `i`'s message is from: the name its connection carries (server-announced, or
    /// self-declared and made unique at connect); only a connection without one (a legacy
    /// direct connection) falls back to the cleaned name in the body, else its address.
    fn sender_name(&self, i: usize, body: &str) -> String {
        let user = &self.peer(i).user;
        if !user.is_empty() {
            return user.clone();
        }
        let body = clean_name(body);
        if body.is_empty() {
            self.addr(i).to_string()
        } else {
            body
        }
    }

    /// Rate limit of peer `i`'s suggestions of `kind` (0 latency, 1 group).
    fn suggestion_due(&mut self, h: &Host, i: usize, kind: usize) -> bool {
        let now = h.now_ms();
        let last = &mut self.peer_mut(i).last_suggest_ms[kind];
        let due = now - *last >= SUGGEST_INTERVAL_MS;
        if due {
            *last = now;
        }
        due
    }

    // ---- latency match, receive sync, suggest group ------------------------------------------

    /// `getAllLatInfo` (P.cpp:2929-2950): our incoming latency from every peer.
    fn own_lat_info(&self, h: &Host) -> Vec<LatEntry> {
        let block_ms = h.cfg.block_ms();
        (0..self.len())
            .map(|i| {
                let p = self.peer(i);
                LatEntry {
                    src_name: p.user.clone(),
                    dest_name: self.common().user.clone(),
                    latency_ms: f64::from(p.latency(block_ms, self.recv_opus(i)).incoming_ms),
                }
            })
            .collect()
    }

    fn send_to_all(&mut self, m: &SbMessage<'_>, only: Option<&[&str]>) {
        if m.encode(&mut Vec::new()).is_err() {
            return;
        }
        for i in 0..self.len() {
            if only.is_none_or(|names| names.contains(&self.peer(i).user.as_str())) {
                self.send_sb(i, m);
            }
        }
    }

    /// `beginLatencyMatchProcedure`: our entries, then `/sb/reqlatinfo` to every peer.
    fn begin_latency_match(&mut self, h: &Host) {
        let entries = self.own_lat_info(h);
        let c = self.common_mut();
        c.lat_info = entries;
        c.lat_match_pending = true;
        for i in 0..self.len() {
            self.peer_mut(i).lat_requested = true;
        }
        self.send_to_all(&SbMessage::ReqLatInfo, None);
    }

    /// `isLatencyMatchProcedureReady`: an entry for every directed path of the mesh; a
    /// broadcast hub's star has two per listener (each listener answers with its own path).
    fn latency_match_ready(&self) -> bool {
        let n = self.len() + 1;
        let paths = if self.hides_peers() {
            2 * self.len()
        } else {
            n * (n - 1)
        };
        self.common().lat_info.len() >= paths
    }

    /// The match button: `sendLatencyMatchToAll` then `commitLatencyMatch` locally.
    fn send_latency_match(&mut self, h: &mut Host, latency_ms: f32) {
        let Some(latency_ms) = jitter::peer_ms(latency_ms.into()) else {
            return;
        };
        let user = self.common().user.clone();
        let m = SbMessage::SuggestLatency {
            from: &user,
            latency_ms,
        };
        self.send_to_all(&m, None);
        self.commit_latency_match(h, latency_ms);
    }

    /// The incoming latency of peer `i` with jitter buffer `buf_ms` (the new-style estimate's
    /// incoming part, P.cpp:3066-3074).
    fn incoming_with(&self, i: usize, buf_ms: f32) -> f32 {
        let p = self.peer(i);
        let codec = if self.recv_opus(i) { 2.5 } else { 0.0 };
        codec + p.remote.in_latency_ms + p.ping.smooth.mean * 0.5 + buf_ms
    }

    /// `commitLatencyMatch` (P.cpp:3059-3088): pad each peer's buffer up to `target_ms` of
    /// incoming latency; auto modes no longer shrink them.
    fn commit_latency_match(&mut self, h: &mut Host, target_ms: f32) {
        let Some(target_ms) = jitter::peer_ms(target_ms.into()) else {
            return;
        };
        let block_ms = h.cfg.block_ms();
        for i in 0..self.len() {
            let j = &self.peer(i).jitter;
            let base = if j.baseline_ms > 0.0 {
                j.baseline_ms
            } else {
                j.buffer_ms
            }
            .max(block_ms);
            if let Some(b) = match_buffer(self.incoming_with(i, base), base, target_ms) {
                self.set_buffer(h, i, b);
            }
            self.peer_mut(i).jitter.latency_matched = true;
        }
    }

    /// Receive Sync (Mendeni "Ear"): every peer's incoming latency made equal to the largest
    /// plus `padding_ms`. Repeating it changes nothing: the padding added last time is taken
    /// out before computing.
    fn receive_sync(&mut self, h: &mut Host, padding_ms: f32) {
        let block_ms = h.cfg.block_ms();
        let unpadded: Vec<f32> = (0..self.len())
            .map(|i| {
                let p = self.peer(i);
                (p.jitter.buffer_ms - p.sync_pad_ms).max(block_ms)
            })
            .collect();
        let bases: Vec<f32> = (0..self.len())
            .map(|i| self.incoming_with(i, unpadded[i]))
            .collect();
        let padding_ms = if padding_ms.is_finite() {
            padding_ms.clamp(0.0, 500.0)
        } else {
            0.0
        };
        for (i, pad) in receive_sync(&bases, padding_ms).into_iter().enumerate() {
            let pad = jitter::buffer_ms(pad);
            let want = jitter::buffer_ms(unpadded[i] + pad);
            if (want - self.peer(i).jitter.buffer_ms).abs() > 0.01 {
                self.set_buffer(h, i, want);
            }
            let p = self.peer_mut(i);
            p.sync_pad_ms = pad;
            p.jitter.latency_matched = true;
        }
    }

    /// `suggestNewGroupToPeers` (P.cpp:3089-3133): to the named peers only.
    fn suggest_group(&mut self, group: &str, password: &str, public: bool, users: &[&str]) {
        let m = SbMessage::SuggestGroup(sbmsg::SuggestGroup {
            user: self.common().user.clone(),
            group: group.to_owned(),
            group_password: password.to_owned(),
            public,
            others: users.iter().map(|u| (*u).to_owned()).collect(),
        });
        self.send_to_all(&m, Some(users));
    }

    // ---- recording, cache, status --------------------------------------------------------------

    fn peer_rec(&self, h: &Host, i: usize) -> PeerRec {
        self.peer(i).peer_rec(h, self.recv_opus(i))
    }

    fn peer_recs(&self, h: &Host) -> Vec<PeerRec> {
        (0..self.len()).map(|i| self.peer_rec(h, i)).collect()
    }

    /// Commit every connected peer's settings to the cache (the state-save path, and every
    /// second while connected: PR #291).
    fn commit_caches(&self, h: &mut Host) {
        for i in 0..self.len() {
            commit_cache_of(h, self.peer(i));
        }
    }

    fn peer_status(&self, h: &Host, i: usize) -> PeerStatus {
        let block_ms = h.cfg.block_ms();
        let p = self.peer(i);
        let pm = h.model.peers[p.slot].as_ref();
        PeerStatus {
            id: p.id,
            user: p.user.clone(),
            group: p.group.clone(),
            addr: self.addr(i),
            connected: p.connected,
            send_active: p.send_active,
            recv_active: p.recv_active,
            send_allow: p.send_allow,
            recv_allow: p.recv_allow,
            soloed: pm.is_some_and(|m| m.params.soloed),
            level: pm.map_or(0.0, |m| m.params.gain),
            safety_muted: p.jitter.safety_muted,
            blocked_us: p.blocked_us,
            remote_recording: p.remote.recording,
            send_channels: p.send_channels,
            recv_channels: pm.map_or(0, |m| m.params.recv_channels),
            send_format: p.format_index,
            recv_format: self.recv_format_name(i),
            requested_format: p.requested_format,
            supported_formats: (0..FORMATS.len())
                .filter(|&f| crate::formats::supports(p.codecs.as_deref(), f))
                .collect(),
            jitter_mode: p.jitter.mode,
            buffer_ms: p.jitter.effective_ms(block_ms),
            auto_init_done: p.jitter.init_completed,
            fill_ratio: p.jitter.fill.mean,
            packets_received: p.packets_received,
            packets_sent: p.packets_sent,
            packets_dropped: p.jitter.dropped,
            packets_resent: p.jitter.resent,
            send_bps: p.send_bps,
            recv_bps: p.recv_bps,
            latency: p.latency(block_ms, self.recv_opus(i)),
            latency_test_active: p.lat_test_active,
            measured_round_trip_ms: p.measured_ms,
            send_channels_override: p.send_override,
            meter: p.shared.meter.levels(),
            groups: pm.map(|m| m.params.groups.clone()).unwrap_or_default(),
            net: self.peer_net(i),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AudioConfig;
    use sonobus_core::state::AppState;

    /// A peer table without a transport: records what goes out.
    struct Mock {
        c: Common,
        peers: Vec<PeerCore>,
        sent: Vec<(usize, Vec<u8>)>,
    }

    impl Proto for Mock {
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
            &self.peers[i]
        }
        fn peer_mut(&mut self, i: usize) -> &mut PeerCore {
            &mut self.peers[i]
        }
        fn send_sb(&mut self, i: usize, m: &SbMessage<'_>) {
            let mut b = Vec::new();
            m.encode(&mut b).unwrap();
            self.sent.push((i, b));
        }
        fn send_layout(&mut self, _: &Host, _: usize) {}
        fn push_buffer(&mut self, _: usize) {}
        fn push_send_format(&mut self, _: &mut Host, _: usize) {}
        fn set_send_active(&mut self, i: usize, a: bool) {
            self.peers[i].send_active = a;
        }
        fn set_recv_active(&mut self, _: usize, _: bool) {}
        fn update_send_channels(&mut self, _: &mut Host) {}
        fn remove_peer_at(&mut self, h: &mut Host, i: usize, _: bool) {
            let p = self.peers.remove(i);
            self.forget_peer(h, &p);
        }
        fn block_peer_at(&mut self, h: &mut Host, i: usize) {
            self.remove_peer_at(h, i, true);
        }
        fn request_format(&mut self, _: &Host, _: usize, _: Option<usize>) {}
        fn set_dynamic_resampling(&mut self, _: bool) {}
        fn set_redundancy(&mut self, _: i32) {}
        fn start_latency_test(&mut self, _: usize) -> bool {
            false
        }
        fn stop_latency_test(&mut self, _: usize) {}
        fn recv_opus(&self, _: usize) -> bool {
            false
        }
        fn recv_format_name(&self, _: usize) -> Option<String> {
            None
        }
        fn addr(&self, _: usize) -> SocketAddr {
            SocketAddr::from(([0, 0, 0, 0], 0))
        }
    }

    fn feed(s: &mut Mock, h: &mut Host, i: usize, m: &SbMessage<'_>) {
        let mut raw = Vec::new();
        m.encode(&mut raw).unwrap();
        let parsed = sbmsg::parse(&raw).unwrap().unwrap();
        s.handle_sb(h, i, parsed, &raw);
    }

    /// What a peer sends goes through the same clamps whatever carries it, and the sender is
    /// the peer the transport says it is, never the name in the message.
    #[test]
    fn peer_numbers_are_clamped_and_names_come_from_the_transport() {
        let cfg = AudioConfig {
            sample_rate: 48000,
            block_size: 256,
            input_channels: 1,
            output_channels: 2,
        };
        let mut h = crate::tests::host(cfg, AppState::new(false));
        let mut s = Mock {
            c: Common::new(),
            peers: Vec::new(),
            sent: Vec::new(),
        };
        let name = clean_name("bob\u{7}\n");
        assert_eq!(name, "bob");
        let p = PeerCore::add(&mut h, 0, &name, "g", "bob").unwrap();
        s.peers.push(p);
        s.peer_added(&mut h, 0);
        let info = PeerInfo {
            jitbuf_ms: Some(1e12),
            in_latency_ms: Some(f64::NAN),
            out_latency_ms: Some(-5.0),
            ..PeerInfo::default()
        };
        feed(&mut s, &mut h, 0, &SbMessage::PeerInfo(info));
        let r = s.peers[0].remote;
        assert_eq!(r.jitter_buffer_ms, jitter::MAX_PEER_LATENCY_MS);
        assert_eq!((r.in_latency_ms, r.out_latency_ms), (0.0, 0.0));
        let chat = SbMessage::Chat {
            group: "g",
            from: "alice",
            targets: "",
            tags: "",
            message: "hi",
        };
        feed(&mut s, &mut h, 0, &chat);
        assert_eq!(s.c.chat.back().unwrap().from, "bob");
        feed(
            &mut s,
            &mut h,
            0,
            &SbMessage::SuggestLatency {
                from: "alice",
                latency_ms: f32::INFINITY,
            },
        );
        feed(
            &mut s,
            &mut h,
            0,
            &SbMessage::SuggestLatency {
                from: "alice",
                latency_ms: 1e9,
            },
        );
        let notes: Vec<_> = h.notes.drain(..).collect();
        assert_eq!(
            notes,
            [Notification::LatencyMatchRequested {
                from: "bob".into(),
                latency_ms: jitter::MAX_PEER_LATENCY_MS,
            }]
        );
        // a peer without a connection name (legacy direct connection): the body's cleaned
        // name, else its address; blocked notices name the sending peer the same way
        let p = PeerCore::add(&mut h, 1, "", "g", "direct").unwrap();
        s.peers.push(p);
        s.peer_added(&mut h, 1);
        let chat = |from| SbMessage::Chat {
            group: "",
            from,
            targets: "",
            tags: "",
            message: "yo",
        };
        feed(&mut s, &mut h, 1, &chat("carol\u{1b}"));
        assert_eq!(s.c.chat.back().unwrap().from, "carol");
        feed(&mut s, &mut h, 1, &chat(""));
        assert_eq!(s.c.chat.back().unwrap().from, "0.0.0.0:0");
        feed(
            &mut s,
            &mut h,
            0,
            &SbMessage::BlockedInfo {
                from: "mallory",
                blocked: true,
            },
        );
        assert!(
            crate::tests::events(&mut h).contains(&Event::BlockedByPeer {
                id: s.peers[0].id,
                user: "bob".into(),
                blocked: true,
            })
        );
        // an unrequested latency table is ignored
        feed(
            &mut s,
            &mut h,
            0,
            &SbMessage::LatInfo(vec![LatEntry {
                src_name: "x".into(),
                dest_name: "y".into(),
                latency_ms: 5.0,
            }]),
        );
        assert!(s.c.lat_info.is_empty());
    }
}
