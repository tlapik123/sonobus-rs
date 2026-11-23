//! Secure sessions (protocol v2, `sonobus_engine::modern`): what the app keeps about them, the
//! forms, and the small pure helpers the controller and the UI share (identity fingerprints,
//! relay names, invite previews, the password hint, the QR code of an invite link).
//!
//! What is persisted, and where: only in our own settings file (`SonoBus.settings`, 0600), as
//! properties outside the engine state (so never in setup files, share links, plugin projects
//! or the OSC state): the device identity key, the relay choice, the last rendezvous address,
//! the broadcast listener sync (on/off, delay) and the remembered sessions. A remembered session keeps its invite link (a secret: whoever
//! has it can join) only when the user ticked "Remember this session".

use std::net::{IpAddr, Ipv4Addr};

use sonobus_core::files::PropertiesFile;
use sonobus_engine::modern::{
    DEFAULT_PLAYOUT_MS, DiscoveredSession, LanScope, MAX_PLAYOUT_MS, PeerAddr, RelayChoice,
    SessionMode,
};
use sonobus_net::RdvAddr;
use sonobus_net::auth::Invite;

pub const IDENTITY_KEY: &str = "sonobusRsIdentityKey";
const RELAY_KEY: &str = "sonobusRsSecureRelay";
const RENDEZVOUS_KEY: &str = "sonobusRsSecureRendezvous";
const RECENTS_KEY: &str = "sonobusRsSecureRecents";
const PLAYOUT_SYNC_KEY: &str = "sonobusRsSecurePlayoutSync";
const PLAYOUT_MS_KEY: &str = "sonobusRsSecurePlayoutMs";
/// Shortest listener sync delay offered: below this hardly any link's jitter buffer fits.
pub const MIN_PLAYOUT_MS: u16 = 20;
const MAX_RECENTS: usize = 12;

/// The online-guessing budget a password join faces, from the transport review: the
/// throttles admit about this many guesses per day in total.
pub const GUESSES_PER_DAY: f64 = 720.0;

/// How a remembered session was reached.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    Hosted,
    Invite,
    Lan,
    Name,
}

impl How {
    fn code(self) -> &'static str {
        match self {
            How::Hosted => "host",
            How::Invite => "invite",
            How::Lan => "lan",
            How::Name => "name",
        }
    }

    fn from_code(s: &str) -> Option<How> {
        [How::Hosted, How::Invite, How::Lan, How::Name]
            .into_iter()
            .find(|h| h.code() == s)
    }
}

/// A remembered secure session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecureRecent {
    pub name: String,
    pub broadcast: bool,
    pub how: How,
    /// Only with "Remember this session": the invite link (hosted: its seed keeps the link
    /// valid when the session is hosted again).
    pub invite: Option<String>,
    /// Name joins: the rendezvous server.
    pub server: Option<String>,
}

/// Secure-session preferences (persisted, see the module docs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecurePrefs {
    pub relay: RelayChoice,
    /// Last rendezvous server address (`sonobus2://rdv/...`).
    pub rendezvous: String,
    pub recents: Vec<SecureRecent>,
    /// Hosting "Share sound": listeners play in sync, `playout_ms` after the host captured it.
    pub playout_sync: bool,
    /// [`MIN_PLAYOUT_MS`]..=[`MAX_PLAYOUT_MS`].
    pub playout_ms: u16,
}

impl Default for SecurePrefs {
    fn default() -> Self {
        // the shipped app's default: n0's public relays
        SecurePrefs {
            relay: RelayChoice::N0Default,
            rendezvous: String::new(),
            recents: Vec::new(),
            playout_sync: false,
            playout_ms: DEFAULT_PLAYOUT_MS,
        }
    }
}

/// Field separator in the recents property: tabs and line breaks never survive [`crate::clean`],
/// and invite links and rendezvous addresses have none.
const SEP: char = '\t';

impl SecurePrefs {
    pub fn load(p: &PropertiesFile) -> SecurePrefs {
        let relay = match p.get(RELAY_KEY).unwrap_or("n0") {
            "none" => RelayChoice::None,
            "n0" | "" => RelayChoice::N0Default,
            url => RelayChoice::Custom(vec![url.to_owned()]),
        };
        let recents = p
            .get(RECENTS_KEY)
            .unwrap_or_default()
            .lines()
            .filter_map(|l| {
                let f: Vec<&str> = l.split(SEP).collect();
                let opt = |i: usize| f.get(i).filter(|s| !s.is_empty()).map(|s| (*s).to_owned());
                Some(SecureRecent {
                    name: f.first().filter(|s| !s.is_empty())?.to_string(),
                    broadcast: *f.get(1)? == "broadcast",
                    how: How::from_code(f.get(2)?)?,
                    invite: opt(3),
                    server: opt(4),
                })
            })
            .collect();
        SecurePrefs {
            relay,
            rendezvous: p.get(RENDEZVOUS_KEY).unwrap_or_default().to_owned(),
            recents,
            playout_sync: p.get(PLAYOUT_SYNC_KEY) == Some("1"),
            playout_ms: p
                .get(PLAYOUT_MS_KEY)
                .and_then(|v| v.parse().ok())
                .map_or(DEFAULT_PLAYOUT_MS, clamp_playout_ms),
        }
    }

    /// The listener sync delay to host with, if sync is on.
    pub fn playout(&self) -> Option<u16> {
        self.playout_sync.then_some(self.playout_ms)
    }

    pub fn store(&self, p: &mut PropertiesFile) {
        let relay = match &self.relay {
            RelayChoice::None => "none".to_owned(),
            RelayChoice::N0Default => "n0".to_owned(),
            RelayChoice::Custom(v) => v.first().cloned().unwrap_or_else(|| "n0".into()),
        };
        p.set(RELAY_KEY, relay);
        p.set(RENDEZVOUS_KEY, self.rendezvous.clone());
        let lines: Vec<String> = self
            .recents
            .iter()
            .map(|r| {
                let mode = if r.broadcast { "broadcast" } else { "mesh" };
                let f = [
                    r.name.as_str(),
                    mode,
                    r.how.code(),
                    r.invite.as_deref().unwrap_or(""),
                    r.server.as_deref().unwrap_or(""),
                ];
                f.map(|x| x.replace([SEP, '\n', '\r'], " "))
                    .join(&SEP.to_string())
            })
            .collect();
        p.set(RECENTS_KEY, lines.join("\n"));
        p.set(PLAYOUT_SYNC_KEY, if self.playout_sync { "1" } else { "0" });
        p.set(PLAYOUT_MS_KEY, self.playout_ms.to_string());
    }

    /// Adds `r` first (replacing an entry for the same session and way of joining).
    pub fn remember(&mut self, r: SecureRecent) {
        self.recents
            .retain(|x| !(x.name == r.name && x.how == r.how));
        self.recents.insert(0, r);
        self.recents.truncate(MAX_RECENTS);
    }
}

pub fn clamp_playout_ms(ms: u16) -> u16 {
    ms.clamp(MIN_PLAYOUT_MS, MAX_PLAYOUT_MS)
}

/// The device identity key from our settings.
pub fn load_identity(p: &PropertiesFile) -> Option<[u8; 32]> {
    let s = p.get(IDENTITY_KEY)?;
    if s.len() != 64 {
        return None;
    }
    let mut k = [0u8; 32];
    for (i, b) in k.iter_mut().enumerate() {
        *b = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(k)
}

pub fn store_identity(p: &mut PropertiesFile, key: &[u8; 32]) {
    p.set(
        IDENTITY_KEY,
        key.iter().map(|b| format!("{b:02x}")).collect::<String>(),
    );
}

/// The short form of an endpoint id people compare by eye: the first 6 bytes, hex, in three
/// groups ("3f2a-91c0-07be").
pub fn short_id(id: &[u8; 32]) -> String {
    id[..6]
        .chunks(2)
        .map(|c| format!("{:02x}{:02x}", c[0], c[1]))
        .collect::<Vec<_>>()
        .join("-")
}

/// What the user reads when a secure session ends for them ([`SessionEvent::Ended`]).
///
/// [`SessionEvent::Ended`]: sonobus_engine::modern::SessionEvent::Ended
pub fn ended_text(reason: &str) -> String {
    use crate::tr;
    use sonobus_engine::modern::{END_BLOCKED, END_FULL, END_HOST_LEFT, END_REMOVED};
    match reason {
        END_REMOVED => tr("You were removed from the session").to_owned(),
        END_BLOCKED => tr("A member blocked you").to_owned(),
        END_HOST_LEFT => tr("The host ended the session").to_owned(),
        END_FULL => tr("The session is full").to_owned(),
        r => format!("{}: {r}", tr("The session ended")),
    }
}

/// The host part of a relay URL ("https://euw1-1.relay.example./" -> "euw1-1.relay.example").
pub fn relay_host(url: &str) -> String {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let host = rest.split(['/', '?']).next().unwrap_or(rest);
    let host = match host.find(']') {
        Some(i) if host.starts_with('[') => &host[..=i],
        _ => host.split(':').next().unwrap_or(host),
    };
    crate::clean(host.trim_end_matches('.'), 80)
}

/// The rendezvous address others type to join by name through `p` (a host serving
/// rendezvous: [`sonobus_engine::modern::SessionStatus::rendezvous`]).
pub fn rdv_text(p: &PeerAddr) -> String {
    RdvAddr {
        id: p.id,
        hosts: p.addrs.iter().map(ToString::to_string).collect(),
        relay: p.relay.clone(),
    }
    .to_string()
}

/// What a link shows before joining: session, host fingerprint, relay, mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvitePreview {
    pub link: String,
    pub session: String,
    pub host: String,
    pub relay: Option<String>,
    pub broadcast: bool,
}

impl InvitePreview {
    /// A `sonobus2://join/...` link or the bare code (anything else: `None`).
    pub fn parse(text: &str) -> Option<InvitePreview> {
        let text = text.trim();
        let inv = Invite::parse(text).ok()?;
        Some(InvitePreview {
            link: inv.to_url().ok()?,
            session: crate::clean(&inv.session, 100),
            host: short_id(&inv.host),
            relay: inv.relay.as_deref().map(relay_host),
            broadcast: inv.mode == SessionMode::Broadcast,
        })
    }
}

/// The host form ("Secure" > "Host").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostForm {
    pub session: String,
    /// "Share sound" (broadcast) instead of "Jam" (mesh).
    pub broadcast: bool,
    /// Broadcast: most listeners admitted (0 = no limit).
    pub max_listeners: u32,
    /// Empty: no password joins.
    pub password: String,
    /// A 6-digit PIN for people on this network (replaces the password).
    pub pin: bool,
    /// Advertise on the local network.
    pub lan: bool,
    /// Serve name + password joins on this device.
    pub rendezvous_here: bool,
    /// Register at this rendezvous server (`sonobus2://rdv/...`; empty: none).
    pub register_at: String,
    /// Remember the session with its invite link.
    pub remember: bool,
}

impl Default for HostForm {
    fn default() -> Self {
        HostForm {
            session: String::new(),
            broadcast: false,
            max_listeners: 0,
            password: String::new(),
            pin: false,
            lan: true,
            rendezvous_here: false,
            register_at: String::new(),
            remember: false,
        }
    }
}

impl HostForm {
    /// What is wrong with the form, if anything (shown before hosting).
    pub fn problem(&self) -> Option<&'static str> {
        let secret = self.pin || !self.password.is_empty();
        if self.session.trim().is_empty() {
            Some("Give the session a name.")
        } else if self.lan && !secret {
            Some("Advertising on the local network needs a PIN or a password.")
        } else if (self.rendezvous_here || !self.register_at.trim().is_empty())
            && self.password.is_empty()
        {
            Some("Joining by name needs a password.")
        } else {
            None
        }
    }
}

/// How the controller joins.
#[derive(Debug, Clone)]
pub enum JoinMethod {
    Invite(String),
    /// A session found on this network, with its PIN or password.
    Lan(DiscoveredSession, String),
    /// Rendezvous server address (text), session name, password.
    Name {
        server: String,
        session: String,
        password: String,
    },
}

/// Where the secure endpoint binds and how LAN discovery runs (tests: loopback).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NetPlace {
    pub bind_ip: IpAddr,
    /// 0 = any.
    pub port: u16,
    /// What "advertise on the local network" and "On this network" use.
    pub lan: LanScope,
}

impl Default for NetPlace {
    fn default() -> Self {
        NetPlace {
            bind_ip: Ipv4Addr::UNSPECIFIED.into(),
            port: sonobus_net::rt::DEFAULT_PORT,
            lan: LanScope::Network,
        }
    }
}

/// The password hint: how long the online guessing budget ([`GUESSES_PER_DAY`]) takes to try
/// half of the password's space (characters drawn from the classes it uses).
pub fn password_hint(pw: &str) -> String {
    if pw.is_empty() {
        return String::new();
    }
    let classes = [
        pw.chars().any(|c| c.is_ascii_lowercase()),
        pw.chars().any(|c| c.is_ascii_uppercase()),
        pw.chars().any(|c| c.is_ascii_digit()),
        pw.chars().any(|c| !c.is_ascii_alphanumeric()),
    ];
    let alphabet: f64 = [26.0, 26.0, 10.0, 33.0]
        .iter()
        .zip(classes)
        .filter(|(_, on)| *on)
        .map(|(n, _)| n)
        .sum();
    let bits = pw.chars().count() as f64 * alphabet.max(2.0).log2();
    let days = 2f64.powf(bits - 1.0) / GUESSES_PER_DAY;
    let (label, span) = if days < 1.0 {
        (crate::tr("Weak"), crate::tr("less than a day"))
    } else if days < 365.0 {
        (crate::tr("Weak"), crate::tr("days to months"))
    } else if days < 365.0 * 100.0 {
        (crate::tr("Fair"), crate::tr("years"))
    } else {
        (crate::tr("Strong"), crate::tr("more than a century"))
    };
    format!(
        "{label}: {} {span}.",
        crate::tr("guessing it online would take")
    )
}

/// The text a QR code of `link` carries: the `sonobus2://join/` prefix as is (Android matches
/// URL schemes case-sensitively), the code upper case so it fits QR's compact alphanumeric
/// mode; links parse case-insensitively.
pub fn qr_text(link: &str) -> String {
    let prefix = sonobus_net::auth::INVITE_URL_PREFIX;
    match link.get(..prefix.len()) {
        Some(p) if p.eq_ignore_ascii_case(prefix) => {
            format!("{prefix}{}", link[prefix.len()..].to_ascii_uppercase())
        }
        _ => link.to_ascii_uppercase(),
    }
}

/// Modules of the QR code of `link` (row-major, true = dark) and its width in modules, without
/// the quiet zone ([`QUIET`] modules around it, drawn by the widget).
pub fn qr_modules(link: &str) -> Option<(usize, Vec<bool>)> {
    let code = qrcode::QrCode::new(qr_text(link).as_bytes()).ok()?;
    Some((
        code.width(),
        code.to_colors()
            .into_iter()
            .map(|c| c == qrcode::Color::Dark)
            .collect(),
    ))
}

/// Light modules around a QR code (the standard's minimum).
pub const QUIET: usize = 4;

/// Dark module squares of a QR code `width` modules wide drawn at `module` points from
/// `origin`, quiet zone included: (x, y, size) in points.
pub fn qr_squares(
    width: usize,
    modules: &[bool],
    origin: (f32, f32),
    module: f32,
) -> Vec<(f32, f32, f32)> {
    modules
        .iter()
        .enumerate()
        .filter(|(_, dark)| **dark)
        .map(|(i, _)| {
            let (x, y) = (i % width + QUIET, i / width + QUIET);
            (
                origin.0 + x as f32 * module,
                origin.1 + y as f32 * module,
                module,
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefs_round_trip_and_cap() {
        let mut p = SecurePrefs {
            relay: RelayChoice::Custom(vec!["https://relay.example/".into()]),
            rendezvous: "sonobus2://rdv/x".into(),
            ..SecurePrefs::default()
        };
        for i in 0..15 {
            p.remember(SecureRecent {
                name: format!("s{i}"),
                broadcast: i % 2 == 0,
                how: How::Invite,
                invite: None,
                server: None,
            });
        }
        p.remember(SecureRecent {
            name: "tab\there".into(),
            broadcast: false,
            how: How::Name,
            invite: Some("sonobus2://join/abc".into()),
            server: Some("sonobus2://rdv/y".into()),
        });
        assert_eq!(p.recents.len(), MAX_RECENTS);
        let mut f = PropertiesFile::default();
        p.store(&mut f);
        let back = SecurePrefs::load(&f);
        assert_eq!(back.relay, p.relay);
        assert_eq!(back.rendezvous, p.rendezvous);
        assert_eq!(back.recents.len(), MAX_RECENTS);
        assert_eq!(
            back.recents[0].name, "tab here",
            "a tab cannot split the fields"
        );
        assert_eq!(
            back.recents[0].invite.as_deref(),
            Some("sonobus2://join/abc")
        );
        assert_eq!(back.recents[1], p.recents[1]);
        // defaults: n0's relays
        assert_eq!(
            SecurePrefs::load(&PropertiesFile::default()).relay,
            RelayChoice::N0Default
        );
        let mut f = PropertiesFile::default();
        SecurePrefs {
            relay: RelayChoice::None,
            ..SecurePrefs::default()
        }
        .store(&mut f);
        assert_eq!(SecurePrefs::load(&f).relay, RelayChoice::None);
    }

    #[test]
    fn playout_sync_round_trip_and_clamp() {
        let d = SecurePrefs::load(&PropertiesFile::default());
        assert_eq!(
            (d.playout_sync, d.playout_ms, d.playout()),
            (false, DEFAULT_PLAYOUT_MS, None)
        );
        let mut f = PropertiesFile::default();
        let p = SecurePrefs {
            playout_sync: true,
            playout_ms: 320,
            ..SecurePrefs::default()
        };
        p.store(&mut f);
        assert_eq!(SecurePrefs::load(&f), p);
        assert_eq!(p.playout(), Some(320));
        // off keeps the delay for next time
        SecurePrefs {
            playout_sync: false,
            ..p.clone()
        }
        .store(&mut f);
        assert_eq!(
            (
                SecurePrefs::load(&f).playout_sync,
                SecurePrefs::load(&f).playout_ms
            ),
            (false, 320)
        );
        // a hand-edited file is clamped
        for (text, want) in [
            ("5", MIN_PLAYOUT_MS),
            ("60000", MAX_PLAYOUT_MS),
            ("junk", DEFAULT_PLAYOUT_MS),
        ] {
            f.set(PLAYOUT_MS_KEY, text);
            assert_eq!(SecurePrefs::load(&f).playout_ms, want, "{text}");
        }
    }

    #[test]
    fn identity_and_names() {
        let mut f = PropertiesFile::default();
        assert_eq!(load_identity(&f), None);
        let k: [u8; 32] = std::array::from_fn(|i| i as u8 * 7);
        store_identity(&mut f, &k);
        assert_eq!(load_identity(&f), Some(k));
        f.set(IDENTITY_KEY, "zz");
        assert_eq!(load_identity(&f), None);
        let id: [u8; 32] = std::array::from_fn(|i| 0x3f + i as u8);
        assert_eq!(short_id(&id), "3f40-4142-4344");
        assert_eq!(
            relay_host("https://euw1-1.relay.example./"),
            "euw1-1.relay.example"
        );
        assert_eq!(relay_host("http://127.0.0.1:3340/"), "127.0.0.1");
        assert_eq!(relay_host("https://[::1]:443/x"), "[::1]");
    }

    #[test]
    fn host_form_checks_and_password_hint() {
        let mut f = HostForm {
            session: "jam".into(),
            lan: false,
            ..HostForm::default()
        };
        assert_eq!(f.problem(), None, "an invite link alone needs nothing else");
        f.lan = true;
        assert!(f.problem().is_some());
        f.pin = true;
        assert_eq!(f.problem(), None);
        f.rendezvous_here = true;
        assert!(f.problem().is_some(), "name joins need a password");
        f.password = "x".into();
        assert_eq!(f.problem(), None);
        assert!(password_hint("abc").starts_with("Weak"));
        assert!(password_hint("correct horse battery staple").starts_with("Strong"));
        assert_eq!(password_hint(""), "");
    }

    fn invite(session: &str, relay: Option<&str>) -> String {
        Invite {
            seed: [9; 16],
            host: [0xab; 32],
            addrs: vec!["192.0.2.7:10999".parse().unwrap()],
            relay: relay.map(Into::into),
            session: session.into(),
            mode: SessionMode::Broadcast,
        }
        .to_url()
        .unwrap()
    }

    #[test]
    fn invite_previews() {
        let link = invite("Radio\u{202e} night", Some("https://relay.example./"));
        let p = InvitePreview::parse(&format!("  {link}\n")).unwrap();
        assert_eq!(
            (
                p.session.as_str(),
                p.host.as_str(),
                p.relay.as_deref(),
                p.broadcast
            ),
            ("Radio night", "abab-abab-abab", Some("relay.example"), true)
        );
        assert_eq!(p.link, link);
        // the bare code and upper case parse too
        let bare = link.trim_start_matches(sonobus_net::auth::INVITE_URL_PREFIX);
        assert_eq!(InvitePreview::parse(bare).unwrap().link, link);
        assert_eq!(InvitePreview::parse(&qr_text(&link)).unwrap().link, link);
        assert!(InvitePreview::parse("sonobus://h:1/?g=x").is_none());
    }

    #[test]
    fn qr_code_of_an_invite() {
        let link = invite("Friday jam", Some("https://euw1-1.relay.n0.example.link./"));
        let text = qr_text(&link);
        assert!(
            text.starts_with("sonobus2://join/")
                && text[16..].chars().all(|c| !c.is_ascii_lowercase())
        );
        let (w, m) = qr_modules(&link).unwrap();
        // the encoder's matrix: a version-v code is 4v + 17 modules wide
        let code = qrcode::QrCode::new(text.as_bytes()).unwrap();
        let qrcode::Version::Normal(v) = code.version() else {
            panic!("micro QR")
        };
        assert_eq!(w, 4 * v as usize + 17);
        assert_eq!(
            m,
            code.to_colors()
                .into_iter()
                .map(|c| c == qrcode::Color::Dark)
                .collect::<Vec<_>>()
        );
        assert_eq!(m.len(), w * w);
        // the alphanumeric upper case is smaller than the link as is
        let plain = qrcode::QrCode::new(link.as_bytes()).unwrap();
        assert!(plain.width() > w, "{} vs {w}", plain.width());
        // finder patterns: the three corners are dark 7x7 frames
        for (x0, y0) in [(0, 0), (w - 7, 0), (0, w - 7)] {
            for k in 0..7 {
                for (x, y) in [
                    (x0 + k, y0),
                    (x0, y0 + k),
                    (x0 + 6, y0 + k),
                    (x0 + k, y0 + 6),
                ] {
                    assert!(m[y * w + x], "finder at {x0},{y0}");
                }
            }
        }
        // drawn squares: one per dark module, none in the quiet zone
        let sq = qr_squares(w, &m, (10.0, 20.0), 2.0);
        assert_eq!(sq.len(), m.iter().filter(|d| **d).count());
        let (lo, hi) = (QUIET as f32 * 2.0, (QUIET + w) as f32 * 2.0);
        assert!(sq.iter().all(|&(x, y, s)| x - 10.0 >= lo
            && y - 20.0 >= lo
            && x - 10.0 + s <= hi
            && y - 20.0 + s <= hi));
        assert_eq!(sq[0], (10.0 + lo, 20.0 + lo, 2.0), "top-left finder corner");
    }
}
