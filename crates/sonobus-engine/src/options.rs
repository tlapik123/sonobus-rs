//! Engine options stock 1.7.2 does not have, persisted as extra `ExtraState` properties. Stock
//! reads that child with `getProperty(key, default)` and keeps unknown properties when it saves
//! (it updates the loaded tree in place), so these keys survive a round trip through a stock
//! client and do not disturb it.

use sonobus_core::state::ExtraState;
use sonobus_core::valuetree::Var;

/// PR #202: copies of every packet, 1..=5.
pub const PACKET_REDUNDANCY: &str = "PacketRedundancy";
/// PR #291: "Remember Local Track Solo" / "Remember Peer Solo" (default on).
pub const REMEMBER_INPUT_SOLO: &str = "RememberLocalSolo";
pub const REMEMBER_PEER_SOLO: &str = "RememberPeerSolo";
/// PR #291: input group solo flags, `,`-separated `0`/`1` per group.
pub const INPUT_SOLO_STATES: &str = "InputSoloStates";
/// PR #291: per user, `name \x1f peer-solo \x1f group-solo-flags`, entries separated by `\x1e`.
pub const PEER_SOLO_STATES: &str = "PeerSoloStates";
/// CanDuet: metronome beats per bar, 1..=12 (1: no accent; upstream never sets it).
pub const MET_BEATS_PER_BAR: &str = "MetBeatsPerBar";
/// Receive Sync: the padding (ms, 0..=500) last used.
pub const RECEIVE_SYNC_PADDING: &str = "ReceiveSyncPadding";
/// Fork "Max Send Quality": the best send format ([`crate::formats::FORMATS`] index) a remote
/// peer's quality request can get from us; -1 = no cap.
pub const MAX_SEND_QUALITY: &str = "MaxSendQuality";
/// Honour remote peers' requests for our send format (upstream always does: default on).
pub const HONOUR_QUALITY_REQUESTS: &str = "HonourQualityRequests";

fn get<'a>(e: &'a ExtraState, key: &str) -> Option<&'a Var> {
    e.unknown.iter().find(|(k, _)| k == key).map(|(_, v)| v)
}

pub fn get_i32(e: &ExtraState, key: &str, default: i32) -> i32 {
    get(e, key).map_or(default, Var::to_i32)
}

pub fn get_bool(e: &ExtraState, key: &str, default: bool) -> bool {
    get(e, key).map_or(default, Var::to_bool)
}

pub fn get_string(e: &ExtraState, key: &str) -> String {
    get(e, key).map(Var::to_string_lossy).unwrap_or_default()
}

pub fn set(e: &mut ExtraState, key: &str, v: Var) {
    match e.unknown.iter_mut().find(|(k, _)| k == key) {
        Some(entry) => entry.1 = v,
        None => e.unknown.push((key.to_owned(), v)),
    }
}

pub fn packet_redundancy(e: &ExtraState) -> i32 {
    get_i32(e, PACKET_REDUNDANCY, 1).clamp(1, 5)
}

pub fn receive_sync_padding(e: &ExtraState) -> f32 {
    get(e, RECEIVE_SYNC_PADDING).map_or(0.0, |v| v.to_f64() as f32).clamp(0.0, 500.0)
}

/// [`MAX_SEND_QUALITY`] as a table index, `None` when unset.
pub fn max_send_quality(e: &ExtraState) -> Option<usize> {
    usize::try_from(get_i32(e, MAX_SEND_QUALITY, -1))
        .ok()
        .filter(|&i| i < crate::formats::FORMATS.len())
}

pub fn honour_quality_requests(e: &ExtraState) -> bool {
    get_bool(e, HONOUR_QUALITY_REQUESTS, true)
}

/// The send format a remote request for `want` gets: none when requests are not honoured,
/// else `want` capped at [`MAX_SEND_QUALITY`] (the table is ordered by bit rate).
pub fn requested_format(e: &ExtraState, want: usize) -> Option<usize> {
    honour_quality_requests(e).then(|| max_send_quality(e).map_or(want, |m| want.min(m)))
}

pub fn beats_per_bar(e: &ExtraState) -> i32 {
    get_i32(e, MET_BEATS_PER_BAR, 1).clamp(1, 12)
}

pub fn flags_to_string(flags: &[bool]) -> String {
    let v: Vec<&str> = flags.iter().map(|&f| if f { "1" } else { "0" }).collect();
    v.join(",")
}

pub fn flags_from_str(s: &str) -> Vec<bool> {
    s.split(',')
        .filter(|t| !t.is_empty())
        .map(|t| t.trim() == "1")
        .collect()
}

/// One user's remembered solo state.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PeerSolo {
    pub soloed: bool,
    pub groups: Vec<bool>,
}

pub fn peer_solos(e: &ExtraState) -> Vec<(String, PeerSolo)> {
    get_string(e, PEER_SOLO_STATES)
        .split('\x1e')
        .filter_map(|entry| {
            let mut it = entry.split('\x1f');
            let name = it.next().filter(|n| !n.is_empty())?.to_owned();
            let soloed = it.next() == Some("1");
            let groups = it.next().map(flags_from_str).unwrap_or_default();
            Some((name, PeerSolo { soloed, groups }))
        })
        .collect()
}

/// Store `solo` for `name`; an entry without any solo is removed.
pub fn set_peer_solo(e: &mut ExtraState, name: &str, solo: &PeerSolo) {
    let mut all = peer_solos(e);
    all.retain(|(n, _)| n != name);
    if solo.soloed || solo.groups.iter().any(|&g| g) {
        all.push((name.to_owned(), solo.clone()));
    }
    let s: Vec<String> = all
        .iter()
        .map(|(n, p)| {
            format!(
                "{n}\x1f{}\x1f{}",
                u8::from(p.soloed),
                flags_to_string(&p.groups)
            )
        })
        .collect();
    set(e, PEER_SOLO_STATES, Var::String(s.join("\x1e")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use sonobus_core::state::AppState;
    use sonobus_core::valuetree::ValueTree;

    #[test]
    fn options_survive_a_state_round_trip() {
        let mut s = AppState::new(false);
        set(&mut s.extra, PACKET_REDUNDANCY, Var::Int(3));
        assert_eq!(receive_sync_padding(&s.extra), 0.0);
        set(&mut s.extra, RECEIVE_SYNC_PADDING, Var::Double(12.5));
        set(&mut s.extra, INPUT_SOLO_STATES, Var::String("0,1".into()));
        let solo = PeerSolo {
            soloed: true,
            groups: vec![false, true],
        };
        set_peer_solo(&mut s.extra, "bob smith", &solo);
        set_peer_solo(&mut s.extra, "amy", &PeerSolo::default());
        let mut bytes = Vec::new();
        s.to_tree().write(&mut bytes);
        let back = AppState::from_tree(&ValueTree::read(&bytes).unwrap());
        assert_eq!(packet_redundancy(&back.extra), 3);
        assert_eq!(
            flags_from_str(&get_string(&back.extra, INPUT_SOLO_STATES)),
            [false, true]
        );
        assert_eq!(peer_solos(&back.extra), vec![("bob smith".into(), solo)]);
        assert_eq!(beats_per_bar(&back.extra), 1);
        assert_eq!(receive_sync_padding(&back.extra), 12.5);
    }
}
