//! Control-side mixer model: the authoritative settings (an [`AppState`]) and the per-peer
//! receive mixes, published to the audio thread as snapshots ([`Cmd`]). Protocol-neutral:
//! the transport tells it about peers and their channel layouts.

use std::sync::Arc;

use sonobus_core::layout::{ChannelLayout, ChannelLayouts};
use sonobus_core::state::{AppState, ChannelGroup, PeerStateCache};
use sonobus_core::valuetree::ValueTree;
use sonobus_dsp::MonitorDelay;

use crate::mixer::{
    AudioConfig, Cmd, DelayTarget, Globals, MAX_CHANNELS, MAX_GROUPS, MAX_PEERS, PeerMix,
    PeerParams, PeerShared,
};
use crate::options;

/// Peer cache keys of modern peers: this prefix plus the endpoint id in hex.
pub(crate) const ID_KEY_PREFIX: &str = "sbid:";

/// One remote peer's receive mix and layout bookkeeping (`RemotePeer` channel-group fields).
pub(crate) struct PeerModel {
    pub user: String,
    /// Peer cache and remembered-solo key: the user name (legacy), or the endpoint id
    /// (modern, [`ID_KEY_PREFIX`]).
    pub key: String,
    pub params: PeerParams,
    /// The user edited the groups: remote layouts no longer overwrite them.
    pub modified_groups: bool,
    modified_multi: bool,
    /// A `/sb/clayinfo` arrived.
    pub recvd_layout: bool,
    orig: Vec<ChannelGroup>,
    last_multi: Vec<ChannelGroup>,
    pub shared: Arc<PeerShared>,
}

pub(crate) struct Model {
    pub cfg: AudioConfig,
    pub state: AppState,
    pub input_solo: Vec<bool>,
    pub peers: Vec<Option<PeerModel>>,
    /// Channels of the provisioned monitor delay line per input group, and the metronome's,
    /// file playback's and soundboard's.
    delays: Vec<usize>,
    met_delay: usize,
    file_delay: usize,
    sb_delay: usize,
    /// Patchbay by slot: bit `j` of `routed_to[i]` sends peer `j`'s audio to peer `i`.
    pub routed_to: [u32; MAX_PEERS],
}

fn group_channels(groups: &[ChannelGroup]) -> usize {
    groups.iter().map(|g| g.num_channels.max(0) as usize).sum()
}

/// Overlay a `Layout` tree on a group (`setFromChannelLayoutValueTree`).
fn overlay_layout(g: &mut ChannelGroup, t: &ValueTree) {
    let mut l = ChannelLayout {
        chan_start: g.chan_start,
        num_channels: g.num_channels,
        name: g.name.clone(),
        pan_stereo: g.pan_stereo,
        pan: std::array::from_fn(|i| g.pans.get(i).copied().unwrap_or(0.0)),
    };
    l.update_from_tree(t);
    g.chan_start = l.chan_start;
    g.num_channels = l.num_channels;
    g.name = l.name;
    g.pan_stereo = l.pan_stereo;
    g.pans = l.pan[..l.num_channels.clamp(0, MAX_CHANNELS as i32) as usize].to_vec();
}

fn layout_of(g: &ChannelGroup, chan_start: i32) -> ChannelLayout {
    ChannelLayout {
        chan_start,
        num_channels: g.num_channels,
        name: g.name.clone(),
        pan_stereo: g.pan_stereo,
        pan: std::array::from_fn(|i| g.pans.get(i).copied().unwrap_or(0.0)),
    }
}

impl Model {
    pub fn new(cfg: AudioConfig, mut state: AppState) -> Self {
        if state.input_groups.is_empty() {
            // prepareToPlay's first-time setup (P.cpp:6991-6998)
            let mut g = ChannelGroup::input_default(0, state.is_plugin);
            g.num_channels = cfg.input_channels.max(1) as i32;
            g.pans = vec![0.0; g.num_channels as usize];
            g.mon_dest_channels = cfg.output_channels.min(2) as i32;
            state.input_groups.push(g);
        }
        // upstream never restores these (P.cpp:8778-8786)
        state.params.met_enabled = false;
        state.params.main_monitor_solo = false;
        // the soundboard bus is always stereo; a file's group is 2 until a file is loaded
        state.soundboard_group.num_channels = 2;
        state.file_playback_group.num_channels = 2;
        // PR #78: a default recording never lacks the full mix when it would be empty or only
        // per-user files
        state.extra.default_recording_options =
            crate::recording::normalize_options(state.extra.default_recording_options);
        let mut input_solo = vec![false; state.input_groups.len()];
        if options::get_bool(&state.extra, options::REMEMBER_INPUT_SOLO, true) {
            let saved = options::flags_from_str(&options::get_string(
                &state.extra,
                options::INPUT_SOLO_STATES,
            ));
            for (s, v) in input_solo.iter_mut().zip(saved) {
                *s = v;
            }
        }
        Self {
            cfg,
            input_solo,
            state,
            peers: (0..MAX_PEERS).map(|_| None).collect(),
            delays: vec![0; MAX_GROUPS],
            met_delay: 0,
            file_delay: 0,
            sb_delay: 0,
            routed_to: [0; MAX_PEERS],
        }
    }

    pub fn globals(&self) -> Box<Globals> {
        let mut input_solo = self.input_solo.clone();
        input_solo.resize(self.state.input_groups.len(), false);
        Box::new(Globals {
            params: self.state.params.clone(),
            input_groups: self.state.input_groups.clone(),
            input_solo,
            metronome_group: self.state.metronome_group.clone(),
            file_group: self.state.file_playback_group.clone(),
            soundboard_group: self.state.soundboard_group.clone(),
            routed_to: self.routed_to,
            beats_per_bar: options::beats_per_bar(&self.state.extra),
        })
    }

    /// Send the global snapshot, after provisioning any monitor delay line it needs.
    pub fn publish(&mut self, out: &mut dyn FnMut(Cmd)) {
        let sr = self.cfg.sample_rate;
        let mut provision = |g: &ChannelGroup, have: &mut usize, target: DelayTarget| {
            let nch = g.num_channels.clamp(0, MAX_CHANNELS as i32) as usize;
            if g.monitor_delay.enabled && nch > 0 && *have != nch {
                *have = nch;
                out(Cmd::Delay(
                    target,
                    Box::new(MonitorDelay::new(sr, nch)),
                    nch,
                ));
            }
        };
        for (i, g) in self.state.input_groups.iter().take(MAX_GROUPS).enumerate() {
            provision(g, &mut self.delays[i], DelayTarget::Input(i));
        }
        provision(
            &self.state.metronome_group,
            &mut self.met_delay,
            DelayTarget::Metronome,
        );
        // upstream applies the monitor delay to the file playback and soundboard monitors too
        // (P.cpp:1709-1719, SoundboardChannelProcessor.cpp:258-265)
        provision(
            &self.state.file_playback_group,
            &mut self.file_delay,
            DelayTarget::File,
        );
        provision(
            &self.state.soundboard_group,
            &mut self.sb_delay,
            DelayTarget::Soundboard,
        );
        out(Cmd::Globals(self.globals()));
    }

    pub fn publish_peer(&self, slot: usize, out: &mut dyn FnMut(Cmd)) {
        if let Some(p) = &self.peers[slot] {
            out(Cmd::Peer(slot, Box::new(p.params.clone())));
        }
    }

    /// Channels a "match inputs" send carries: input groups plus the metronome, the file
    /// playback and the soundboard if sent (`updateRemotePeerSendChannels`, P.cpp:5214-5229).
    pub fn total_send_channels(&self) -> usize {
        let p = &self.state.params;
        let mut n = group_channels(&self.state.input_groups) + usize::from(p.send_met_audio);
        if p.send_file_audio {
            n += self.state.file_playback_group.num_channels.max(0) as usize;
        }
        if p.send_soundboard_audio {
            n += self.state.soundboard_group.num_channels.max(0) as usize;
        }
        n.min(MAX_CHANNELS)
    }

    /// `getSendUserFormatLayoutTree` (P.cpp:6359-6403).
    pub fn send_layout(&self) -> ChannelLayouts {
        let sc = self.state.params.send_channels;
        let mut groups = Vec::new();
        if sc == 1 || sc == 2 {
            let g = ChannelGroup {
                num_channels: sc,
                pans: vec![0.0; sc as usize],
                ..Default::default()
            };
            groups.push(layout_of(&g, 0));
        } else {
            let mut start = 0;
            for g in &self.state.input_groups {
                groups.push(layout_of(g, start));
                start += g.num_channels;
            }
            let p = &self.state.params;
            for (send, g) in [
                (p.send_met_audio, &self.state.metronome_group),
                (p.send_file_audio, &self.state.file_playback_group),
                (p.send_soundboard_audio, &self.state.soundboard_group),
            ] {
                if send {
                    groups.push(layout_of(g, start));
                    start += g.num_channels;
                }
            }
        }
        ChannelLayouts { groups }
    }

    /// [`Self::send_layout`] without the pans: what makes a layout change worth announcing.
    pub fn send_layout_shape(&self) -> ChannelLayouts {
        let mut l = self.send_layout();
        for g in &mut l.groups {
            g.pan = [0.0; MAX_CHANNELS];
            g.pan_stereo = [0.0; 2];
        }
        l
    }

    pub fn free_slot(&self) -> Option<usize> {
        self.peers.iter().position(Option::is_none)
    }

    /// New peer in `slot` (`doAddRemotePeerIfNecessary`): one group with no channels until
    /// the stream format arrives, level from the cache or `defPeerLevel`.
    pub fn add_peer(
        &mut self,
        slot: usize,
        user: &str,
        cache: Option<&PeerStateCache>,
        out: &mut dyn FnMut(Cmd),
    ) -> Arc<PeerShared> {
        let mut g = ChannelGroup {
            num_channels: 0,
            pans: Vec::new(),
            ..Default::default()
        };
        g.gain = 1.0;
        let mut p = PeerModel {
            user: user.to_owned(),
            key: user.to_owned(),
            params: PeerParams {
                gain: self.state.params.def_peer_level,
                groups: vec![g],
                group_solo: vec![false],
                ..Default::default()
            },
            modified_groups: false,
            modified_multi: false,
            recvd_layout: false,
            orig: Vec::new(),
            last_multi: Vec::new(),
            shared: Arc::new(PeerShared::default()),
        };
        if let Some(c) = cache {
            Self::load_cache(&mut p, c);
        }
        let shared = p.shared.clone();
        out(Cmd::AddPeer(
            slot,
            PeerMix::new(&self.cfg, Box::new(p.params.clone()), shared.clone()),
        ));
        self.peers[slot] = Some(p);
        shared
    }

    fn load_cache(p: &mut PeerModel, c: &PeerStateCache) {
        p.params.gain = c.level;
        if !c.channel_groups.is_empty() {
            p.params.groups = c.channel_groups.clone();
            p.params.group_solo = vec![false; p.params.groups.len()];
        }
        p.last_multi = c.multi_channel_groups.clone();
        p.modified_groups = c.modified_chan_groups;
        p.modified_multi = c.modified_chan_groups;
    }

    /// A name became known for an existing peer: load its cache entry if any.
    pub fn set_peer_user(&mut self, slot: usize, user: &str, cache: Option<&PeerStateCache>) {
        if let Some(p) = &mut self.peers[slot] {
            p.user = user.to_owned();
            p.key = user.to_owned();
            if let Some(c) = cache {
                Self::load_cache(p, c);
            }
        }
    }

    pub fn remove_peer(&mut self, slot: usize, out: &mut dyn FnMut(Cmd)) {
        if self.peers[slot].take().is_some() {
            out(Cmd::RemovePeer(slot));
        }
    }

    /// `findAndLoadCacheForPeer`'s lookup: exact name, else the first cached name (in name
    /// order) starting with the name minus its last word. Identity keys
    /// ([`ID_KEY_PREFIX`]) match exactly only.
    pub fn find_cache(&self, user: &str) -> Option<&PeerStateCache> {
        if user.is_empty() {
            return None;
        }
        let cache = &self.state.peer_cache;
        let exact = cache.iter().find(|c| c.name == user);
        if user.starts_with(ID_KEY_PREFIX) {
            return exact;
        }
        exact.or_else(|| {
            let toks: Vec<&str> = user.split_whitespace().collect();
            let base = if toks.len() > 1 {
                toks[..toks.len() - 1].join(" ")
            } else {
                user.to_owned()
            };
            let base = base.trim();
            cache.iter().find(|c| c.name.starts_with(base))
        })
    }

    /// `commitCacheForPeer`; `net` = (jitter buffer ms, jitter mode, send format index).
    pub fn commit_cache(
        &mut self,
        slot: usize,
        (netbuf, netbufauto, send_format): (f32, i32, i32),
    ) {
        let Some(p) = &self.peers[slot] else { return };
        if p.key.is_empty() {
            return;
        }
        let entry = PeerStateCache {
            name: p.key.clone(),
            netbuf,
            netbufauto,
            send_format,
            level: p.params.gain,
            order_priority: -1,
            channel_groups: p.params.groups.iter().take(MAX_GROUPS).cloned().collect(),
            multi_channel_groups: p.last_multi.clone(),
            modified_chan_groups: p.modified_multi,
        };
        let cache = &mut self.state.peer_cache;
        match cache.iter_mut().find(|c| c.name == entry.name) {
            Some(c) => {
                let order = c.order_priority;
                *c = PeerStateCache {
                    order_priority: order,
                    ..entry
                };
            }
            None => {
                let at = cache.partition_point(|c| c.name < entry.name);
                cache.insert(at, entry);
            }
        }
    }

    /// The stream format announced `nch` channels (P.cpp:3784-3823). Returns whether the
    /// count changed (the transport then re-creates its receive buffers).
    pub fn set_recv_channels(&mut self, slot: usize, nch: usize, got_layout: bool) -> bool {
        let Some(p) = &mut self.peers[slot] else {
            return false;
        };
        let nch = nch.min(MAX_CHANNELS);
        if p.params.recv_channels == nch {
            return false;
        }
        p.params.recv_channels = nch;
        if !got_layout && !p.recvd_layout {
            let groups = &mut p.params.groups;
            if nch > 2 {
                if !p.modified_groups {
                    groups.resize_with(nch, ChannelGroup::default);
                    for (i, g) in groups.iter_mut().enumerate() {
                        g.chan_start = i as i32;
                        g.num_channels = 1;
                    }
                }
            } else {
                groups.truncate(1);
                if groups.is_empty() {
                    groups.push(ChannelGroup::default());
                }
                groups[0].num_channels = nch as i32;
            }
            p.params.group_solo.resize(p.params.groups.len(), false);
        }
        true
    }

    /// `applyLayoutFormatToPeer` (P.cpp:6493-6561) for a received `ChannelLayouts` tree.
    pub fn apply_layout(&mut self, slot: usize, tree: &ValueTree) {
        let n_out = self.cfg.output_channels as i32;
        let Some(p) = &mut self.peers[slot] else {
            return;
        };
        let children = &tree.children[..tree.children.len().min(MAX_GROUPS)];
        p.orig = children
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mut g = p.params.groups.get(i).cloned().unwrap_or_default();
                overlay_layout(&mut g, c);
                g
            })
            .collect();
        let origchans = group_channels(&p.orig);
        let modchans = group_channels(&p.params.groups);
        let mut apply = !p.modified_groups || modchans != origchans;
        if origchans <= 2 && modchans > 2 && p.modified_multi {
            p.last_multi = p.params.groups.clone();
        } else if origchans > 2
            && modchans <= 2
            && !p.last_multi.is_empty()
            && p.modified_multi
            && group_channels(&p.last_multi) == origchans
        {
            // upstream copies only the old group count before switching to the saved one
            // (P.cpp:6547-6551); we restore the whole saved layout
            p.params.groups = p.last_multi.clone();
            p.modified_groups = true;
            apply = false;
        }
        if apply {
            // restoreLayoutFormatForPeer (P.cpp:6468-6491)
            p.params.groups = p.orig.clone();
            for g in &mut p.params.groups {
                if g.num_channels > g.pan_dest_channels {
                    g.pan_dest_channels = g.num_channels.min(n_out);
                    g.pan_dest_start = g.pan_dest_start.min(n_out - g.pan_dest_channels).max(0);
                }
            }
            p.modified_groups = false;
        }
        p.params.group_solo.resize(p.params.groups.len(), false);
    }

    /// The user edited a peer's groups (`modifiedChanGroups`; multichannel streams also
    /// remember them for a later switch back).
    pub fn edit_peer_groups(
        &mut self,
        slot: usize,
        f: impl FnOnce(&mut Vec<ChannelGroup>, &mut Vec<bool>),
    ) {
        let Some(p) = &mut self.peers[slot] else {
            return;
        };
        f(&mut p.params.groups, &mut p.params.group_solo);
        p.params.groups.truncate(MAX_GROUPS);
        p.params.group_solo.resize(p.params.groups.len(), false);
        p.modified_groups = true;
        if p.params.recv_channels > 2 {
            p.modified_multi = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model() -> Model {
        let cfg = AudioConfig {
            sample_rate: 48000,
            block_size: 128,
            input_channels: 2,
            output_channels: 2,
        };
        Model::new(cfg, AppState::new(false))
    }

    fn tree(groups: &[(i32, i32)]) -> ValueTree {
        ChannelLayouts {
            groups: groups
                .iter()
                .map(|&(s, n)| ChannelLayout {
                    chan_start: s,
                    num_channels: n,
                    name: format!("g{s}"),
                    ..Default::default()
                })
                .collect(),
        }
        .to_tree()
    }

    /// Monitor delay lines are provisioned for file playback (the file's channels) and the
    /// soundboard too, once per channel count.
    #[test]
    fn delay_lines_for_file_and_soundboard() {
        let mut m = model();
        m.state.file_playback_group.monitor_delay.enabled = true;
        m.state.soundboard_group.monitor_delay.enabled = true;
        let lines = |m: &mut Model| {
            let mut v = Vec::new();
            m.publish(&mut |c| {
                if let Cmd::Delay(t, _, ch) = c {
                    v.push((t, ch));
                }
            });
            v
        };
        assert_eq!(
            lines(&mut m),
            [(DelayTarget::File, 2), (DelayTarget::Soundboard, 2)]
        );
        assert!(lines(&mut m).is_empty());
        m.state.file_playback_group.num_channels = 1; // a mono file was loaded
        assert_eq!(lines(&mut m), [(DelayTarget::File, 1)]);
    }

    #[test]
    fn default_input_group_and_send_layouts() {
        let mut m = model();
        assert_eq!(m.state.input_groups.len(), 1);
        assert_eq!(m.state.input_groups[0].num_channels, 2);
        assert_eq!(m.send_layout().groups.len(), 1); // standalone: mono send
        assert_eq!(m.send_layout().groups[0].num_channels, 1);
        m.state.params.send_channels = 0;
        m.state.params.send_met_audio = true;
        let l = m.send_layout();
        assert_eq!(
            l.groups
                .iter()
                .map(|g| (g.chan_start, g.num_channels))
                .collect::<Vec<_>>(),
            [(0, 2), (2, 1)]
        );
        assert_eq!(m.total_send_channels(), 3);
    }

    #[test]
    fn layouts_rebuild_groups_unless_modified() {
        let mut m = model();
        let mut cmds = Vec::new();
        m.add_peer(0, "bob", None, &mut |c| cmds.push(c));
        assert!(m.set_recv_channels(0, 4, false));
        assert_eq!(
            m.peers[0].as_ref().unwrap().params.groups.len(),
            4,
            "one group per channel without a layout"
        );
        m.apply_layout(0, &tree(&[(0, 2), (2, 2)]));
        let p = m.peers[0].as_ref().unwrap();
        assert_eq!(
            p.params
                .groups
                .iter()
                .map(|g| (g.chan_start, g.num_channels))
                .collect::<Vec<_>>(),
            [(0, 2), (2, 2)]
        );
        assert_eq!(p.params.groups[1].name, "g2");
        // a user edit survives a layout with the same channel total...
        m.edit_peer_groups(0, |g, _| g[0].gain = 0.25);
        m.apply_layout(0, &tree(&[(0, 1), (1, 3)]));
        assert_eq!(
            m.peers[0].as_ref().unwrap().params.groups[0].num_channels,
            2
        );
        // ...but not a change of the total
        m.apply_layout(0, &tree(&[(0, 1)]));
        let p = m.peers[0].as_ref().unwrap();
        assert_eq!(p.params.groups.len(), 1);
        assert_eq!(p.params.groups[0].gain, 0.25, "non-layout params are kept");
        // malformed counts are bounded
        m.apply_layout(0, &tree(&[(-5, 100_000)]));
        assert_eq!(
            m.peers[0].as_ref().unwrap().params.groups[0].pans.len(),
            MAX_CHANNELS
        );
    }

    #[test]
    fn input_solo_is_restored_only_when_remembered() {
        let cfg = model().cfg;
        let mut st = AppState::new(false);
        st.input_groups = vec![ChannelGroup::default(), ChannelGroup::default()];
        options::set(
            &mut st.extra,
            options::INPUT_SOLO_STATES,
            sonobus_core::valuetree::Var::String("0,1".into()),
        );
        assert_eq!(Model::new(cfg, st.clone()).input_solo, [false, true]);
        options::set(
            &mut st.extra,
            options::REMEMBER_INPUT_SOLO,
            sonobus_core::valuetree::Var::Bool(false),
        );
        assert_eq!(Model::new(cfg, st).input_solo, [false, false]);
        // PR #78 on load: an "each user only" default gets the mix
        let mut st = AppState::new(false);
        st.extra.default_recording_options = 8;
        assert_eq!(Model::new(cfg, st).state.extra.default_recording_options, 9);
    }

    #[test]
    fn peer_cache_commit_and_prefix_lookup() {
        let mut m = model();
        m.add_peer(1, "alice laptop", None, &mut |_| {});
        m.peers[1].as_mut().unwrap().params.gain = 0.7;
        m.commit_cache(1, (25.0, 1, 8));
        m.commit_cache(1, (30.0, 1, 8));
        assert_eq!(m.state.peer_cache.len(), 1);
        let c = m.find_cache("alice desktop").expect("prefix match");
        assert_eq!((c.netbuf, c.level, c.send_format), (30.0, 0.7, 8));
        assert!(m.find_cache("bob").is_none());
        let shared = m.add_peer(
            2,
            "alice",
            m.find_cache("alice").cloned().as_ref(),
            &mut |_| {},
        );
        assert_eq!(m.peers[2].as_ref().unwrap().params.gain, 0.7);
        drop(shared);
    }
}
