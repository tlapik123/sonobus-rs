//! Join authentication (sans-IO). Every join method ends with both sides holding the 32-byte
//! session key `K` and having proved it, bound to the QUIC connection through the TLS exporter
//! `E = export_keying_material(32, EXPORTER_LABEL, session_tag)`.
//!
//! * KEY (invite links, mesh connections): connector C sends `mac_C = HMAC(K, "C"‖E‖id_C‖id_A)`
//!   in HELLO, acceptor A answers `mac_A = HMAC(K, "A"‖E‖id_A‖id_C)` in WELCOME.
//! * PAKE (session name + password, LAN PIN): SPAKE2 (Ed25519 group; C = side A, identities =
//!   the two endpoint ids). HELLO carries `X`, PAKE2 carries `Y` only. `Kc = HKDF(spake_out ‖
//!   E)`. C confirms first: PAKE3 = `conf_C = HMAC(Kc, "C"‖E‖X‖Y)`; A checks it and answers
//!   WELCOME with `K` wrapped under `Kc` (`K ⊕ HKDF(Kc,"pad") ‖ HMAC(HKDF(Kc,"mac"), E‖ct)`)
//!   plus `mac_A`. The wrap MAC is A's key confirmation: C accepts nothing before it verifies.
//!
//! All verifications are constant-time and fail with the one opaque [`AuthFailed`].
//!
//! # Why the connector confirms first
//!
//! Each PAKE run lets the peer test exactly one password guess, and the guess is *complete* at
//! the moment the other side reveals a confirmation value. If A sent `conf_A` in PAKE2 (the
//! original design), every HELLO answered with PAKE2 would be a finished guess even if the
//! attacker hangs up right away, so abandoned attempts would have to count against a PIN and
//! five HELLOs would kill it. With C confirming first, `Y` alone reveals nothing: an attempt
//! that never sends PAKE3 has tested nothing and is not counted toward PIN invalidation (it is
//! still charged to the source address and the global budget, see [`Lockout`]). The price is
//! that an impostor *acceptor* (someone the joiner dialed by mistake) gets one guess at the
//! joiner's password per join attempt, which is the same one-guess-per-run bound SPAKE2 always
//! gives, now on the other side.
//!
//! # Online guessing bounds ([`Lockout`])
//!
//! Primary limiter, per source address (IPv4 address, IPv6 /64): [`FREE_FAILURES`] failures,
//! then a back-off from [`BACKOFF_MIN`] doubling to [`BACKOFF_MAX`]: about 21 guesses in the
//! first hour, 12 per hour after that, and it never affects other addresses. Backstop, global:
//! a bucket of [`GLOBAL_BURST`] attempts refilled one per [`GLOBAL_REFILL`] (≤ 40 attempts in
//! the first hour, 30 per hour sustained) shared by all addresses, so an attacker with any
//! number of addresses still gets at most ~30 guesses per hour. Successful joins refund both.
//!
//! * 6-digit PIN ([`PIN_FAILURES`] = 5 completed wrong guesses invalidate it): success
//!   probability ≤ 5 / 10⁶ per PIN, whatever the attacker's resources.
//! * Password without PIN semantics: ≤ 30 guesses/hour ≈ 720/day. A password from a
//!   10 000-word list falls in about a week on average under a sustained distributed attack, a
//!   6-digit number in ~4 years, a random 8-character lowercase one never in practice.
//!
//! The global bucket is a denial-of-service lever: whoever exhausts it makes *password* joins
//! wait (they get "retry later", never "wrong password"). KEY joins (invite links, mesh) are
//! never subject to any of this. Attempts are charged whether or not the HELLO named the right
//! session, so the throttle state does not reveal whether a session name is valid.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

use hkdf::Hkdf;
use hmac::{Hmac, KeyInit, Mac};
use icu_normalizer::ComposingNormalizerBorrowed;
use sha2::{Digest, Sha256};
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use crate::wire::{self, MAX_ADDRS, MAX_NAME, MAX_RELAY_URL, Rd, SessionMode};

pub type Key = [u8; 32];
/// An iroh endpoint id (ed25519 public key bytes).
pub type Id = [u8; 32];
/// TLS exporter output for this connection.
pub type Exporter = [u8; 32];

pub const EXPORTER_LABEL: &[u8] = b"sonobus/2 auth";

/// Authentication failed (deliberately says nothing about why).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuthFailed;

impl std::fmt::Display for AuthFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("authentication failed")
    }
}

impl std::error::Error for AuthFailed {}

/// Which end of the QUIC connection: the dialer (C) or the listener (A).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Connector,
    Acceptor,
}

impl Side {
    fn label(self) -> &'static [u8] {
        match self {
            Side::Connector => b"C",
            Side::Acceptor => b"A",
        }
    }
}

/// `SHA-256("sb2 tag" ‖ session name)[..16]`: names the session in HELLO and the exporter.
pub fn session_tag(session: &str) -> [u8; 16] {
    let h = Sha256::new()
        .chain_update(b"sb2 tag")
        .chain_update(session.as_bytes())
        .finalize();
    h[..16].try_into().unwrap()
}

/// A fresh random session key.
pub fn random_key() -> Key {
    let mut k = [0; 32];
    getrandom::fill(&mut k).expect("OS RNG");
    k
}

/// A uniformly random 6-digit PIN (leading zeros kept).
pub fn random_pin() -> String {
    loop {
        let mut b = [0; 4];
        getrandom::fill(&mut b).expect("OS RNG");
        let v = u32::from_le_bytes(b);
        // 4 294 000 000 is the largest multiple of 10^6 that fits: no modulo bias.
        if v < 4_294_000_000 {
            return format!("{:06}", v % 1_000_000);
        }
    }
}

/// A fresh random invite seed (see [`key_from_seed`]).
pub fn random_seed() -> [u8; 16] {
    let mut s = [0; 16];
    getrandom::fill(&mut s).expect("OS RNG");
    s
}

/// Session key of an invite seed: `HKDF(seed, "sb2 invite key")`.
pub fn key_from_seed(seed: &[u8; 16]) -> Key {
    *hkdf(&[seed], b"sb2 invite key")
}

fn mac(key: &[u8], parts: &[&[u8]]) -> Hmac<Sha256> {
    let mut m = <Hmac<Sha256> as KeyInit>::new_from_slice(key).unwrap();
    for p in parts {
        m.update(p);
    }
    m
}

fn mac_out(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    mac(key, parts).finalize().into_bytes().into()
}

fn mac_check(key: &[u8], parts: &[&[u8]], tag: &[u8]) -> Result<(), AuthFailed> {
    mac(key, parts).verify_slice(tag).map_err(|_| AuthFailed)
}

fn hkdf(ikm: &[&[u8]], info: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut out = Zeroizing::new([0; 32]);
    let ikm = Zeroizing::new(ikm.concat());
    Hkdf::<Sha256>::new(None, &ikm)
        .expand(info, &mut *out)
        .unwrap();
    out
}

/// KEY proof sent by `me` (on `side`) to `peer`.
pub fn key_proof(k: &Key, side: Side, e: &Exporter, me: &Id, peer: &Id) -> [u8; 32] {
    mac_out(k, &[side.label(), e, me, peer])
}

/// KEY_PROOF frame body: `me` shows `peer` (over an established connection with exporter `e`)
/// that it holds `k` after a rotation. Bound to the direction, so it cannot be echoed back.
pub fn rekey_proof(k: &Key, e: &Exporter, me: &Id, peer: &Id) -> [u8; 32] {
    mac_out(k, &[b"sb2 rekey proof", e, me, peer])
}

/// Check a KEY_PROOF that `peer` sent to `me`.
pub fn rekey_verify(k: &Key, e: &Exporter, peer: &Id, me: &Id, proof: &[u8]) -> bool {
    mac_check(k, &[b"sb2 rekey proof", e, peer, me], proof).is_ok()
}

/// Check a KEY proof that `peer` (on `peer_side`) sent to `me`.
pub fn key_verify(
    k: &Key,
    peer_side: Side,
    e: &Exporter,
    peer: &Id,
    me: &Id,
    proof: &[u8],
) -> Result<(), AuthFailed> {
    mac_check(k, &[peer_side.label(), e, peer, me], proof)
}

// ---------------------------------------------------------------------------------------------
// Secret normalisation

/// Why a password/PIN was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecretError {
    /// Nothing left after trimming.
    Empty,
    /// PIN mode: the PIN must be ASCII digits (spaces and dashes are ignored).
    NotDigits,
}

impl std::fmt::Display for SecretError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            SecretError::Empty => "empty password",
            SecretError::NotDigits => "a PIN must be digits",
        })
    }
}

impl std::error::Error for SecretError {}

/// Canonical form of a typed password: Unicode NFC with surrounding whitespace trimmed. A
/// secret made only of ASCII digits, spaces and dashes (a PIN as typed: `"123 456"`,
/// `"123-456"`) keeps only its digits. Both sides apply this, so it needs no PIN/password flag.
pub fn normalize_secret(s: &str) -> Result<Zeroizing<String>, SecretError> {
    let nfc = Zeroizing::new(
        ComposingNormalizerBorrowed::new_nfc()
            .normalize(s)
            .into_owned(),
    );
    let t = nfc.trim();
    let pin_like = t
        .bytes()
        .all(|c| c.is_ascii_digit() || c == b' ' || c == b'-')
        && t.bytes().any(|c| c.is_ascii_digit());
    let out = Zeroizing::new(if pin_like {
        t.chars().filter(char::is_ascii_digit).collect()
    } else {
        t.to_string()
    });
    if out.is_empty() {
        return Err(SecretError::Empty);
    }
    Ok(out)
}

/// [`normalize_secret`], and the result must be ASCII digits.
pub fn normalize_pin(s: &str) -> Result<Zeroizing<String>, SecretError> {
    let n = normalize_secret(s)?;
    if !n.bytes().all(|c| c.is_ascii_digit()) {
        return Err(SecretError::NotDigits);
    }
    Ok(n)
}

// ---------------------------------------------------------------------------------------------
// PAKE

/// PAKE key `Kc` (zeroised on drop), held by both sides once `conf_C` matched.
pub struct PakeKey {
    kc: Zeroizing<[u8; 32]>,
    e: Exporter,
}

impl PakeKey {
    /// Acceptor: wrap the session key for the confirmed joiner: `K ⊕ HKDF(Kc, "pad") ‖
    /// HMAC(HKDF(Kc, "mac"), E ‖ ct)`. Kc is fresh per connection, so the pad is one-time.
    pub fn wrap(&self, k: &Key) -> [u8; 64] {
        let pad = hkdf(&[&*self.kc], b"sb2 wrap pad");
        let mut out = [0; 64];
        for i in 0..32 {
            out[i] = k[i] ^ pad[i];
        }
        let t = mac_out(
            &*hkdf(&[&*self.kc], b"sb2 wrap mac"),
            &[&self.e, &out[..32]],
        );
        out[32..].copy_from_slice(&t);
        out
    }

    /// Connector: verify the acceptor (wrap MAC) and recover the session key from WELCOME.
    pub fn unwrap(&self, w: &[u8; 64]) -> Result<Key, AuthFailed> {
        mac_check(
            &*hkdf(&[&*self.kc], b"sb2 wrap mac"),
            &[&self.e, &w[..32]],
            &w[32..],
        )?;
        let pad = hkdf(&[&*self.kc], b"sb2 wrap pad");
        Ok(std::array::from_fn(|i| w[i] ^ pad[i]))
    }
}

fn kc(spake_out: Vec<u8>, e: &Exporter) -> Zeroizing<[u8; 32]> {
    let out = Zeroizing::new(spake_out);
    hkdf(&[&out, e], b"sb2 kc")
}

fn spake_ids(id_c: &Id, id_a: &Id) -> (Identity, Identity) {
    (Identity::new(id_c), Identity::new(id_a))
}

/// PAKE, connector side, after sending `X` in HELLO.
pub struct PakeConnector {
    spake: Spake2<Ed25519Group>,
    x: Vec<u8>,
    e: Exporter,
}

impl PakeConnector {
    /// Returns the state and `X` (33 bytes) for HELLO. `password` should be normalised
    /// ([`normalize_secret`]).
    pub fn start(password: &str, e: &Exporter, id_c: &Id, id_a: &Id) -> (Self, Vec<u8>) {
        let (ic, ia) = spake_ids(id_c, id_a);
        let pw = Password::new(password.as_bytes());
        Self::from_spake(Spake2::start_a(&pw, &ic, &ia), e)
    }

    #[cfg(test)]
    fn start_with_rng(
        password: &str,
        e: &Exporter,
        id_c: &Id,
        id_a: &Id,
        rng: impl rand_core::CryptoRng,
    ) -> (Self, Vec<u8>) {
        let (ic, ia) = spake_ids(id_c, id_a);
        let pw = Password::new(password.as_bytes());
        Self::from_spake(Spake2::start_a_with_rng(&pw, &ic, &ia, rng), e)
    }

    fn from_spake((spake, x): (Spake2<Ed25519Group>, Vec<u8>), e: &Exporter) -> (Self, Vec<u8>) {
        (
            Self {
                spake,
                x: x.clone(),
                e: *e,
            },
            x,
        )
    }

    /// On PAKE2: returns the PAKE key and `conf_C` for PAKE3. The acceptor is not confirmed yet:
    /// that happens when WELCOME's wrapped key passes [`PakeKey::unwrap`].
    pub fn finish(self, y: &[u8]) -> Result<(PakeKey, [u8; 32]), AuthFailed> {
        let out = self.spake.finish(y).map_err(|_| AuthFailed)?;
        let kc = kc(out, &self.e);
        let conf_c = mac_out(&*kc, &[b"C", &self.e, &self.x, y]);
        Ok((PakeKey { kc, e: self.e }, conf_c))
    }
}

/// PAKE, acceptor side, after sending PAKE2.
pub struct PakeAcceptor {
    key: PakeKey,
    x: Vec<u8>,
    y: Vec<u8>,
}

impl PakeAcceptor {
    /// On HELLO{PAKE, X}: returns the state and `Y` for PAKE2.
    pub fn respond(
        password: &str,
        e: &Exporter,
        id_c: &Id,
        id_a: &Id,
        x: &[u8],
    ) -> Result<(Self, Vec<u8>), AuthFailed> {
        let (ic, ia) = spake_ids(id_c, id_a);
        let pw = Password::new(password.as_bytes());
        Self::from_spake(Spake2::start_b(&pw, &ic, &ia), e, x)
    }

    #[cfg(test)]
    fn respond_with_rng(
        password: &str,
        e: &Exporter,
        id_c: &Id,
        id_a: &Id,
        x: &[u8],
        rng: impl rand_core::CryptoRng,
    ) -> Result<(Self, Vec<u8>), AuthFailed> {
        let (ic, ia) = spake_ids(id_c, id_a);
        let pw = Password::new(password.as_bytes());
        Self::from_spake(Spake2::start_b_with_rng(&pw, &ic, &ia, rng), e, x)
    }

    fn from_spake(
        (spake, y): (Spake2<Ed25519Group>, Vec<u8>),
        e: &Exporter,
        x: &[u8],
    ) -> Result<(Self, Vec<u8>), AuthFailed> {
        let out = spake.finish(x).map_err(|_| AuthFailed)?;
        Ok((
            Self {
                key: PakeKey {
                    kc: kc(out, e),
                    e: *e,
                },
                x: x.to_vec(),
                y: y.clone(),
            },
            y,
        ))
    }

    /// On PAKE3: check `conf_C`. A failure here is a completed wrong guess.
    pub fn confirm(self, conf_c: &[u8]) -> Result<PakeKey, AuthFailed> {
        mac_check(
            &*self.key.kc,
            &[b"C", &self.key.e, &self.x, &self.y],
            conf_c,
        )?;
        Ok(self.key)
    }
}

// ---------------------------------------------------------------------------------------------
// Failure accounting

/// Failures per source address before its back-off starts.
pub const FREE_FAILURES: u32 = 3;
pub const BACKOFF_MIN: Duration = Duration::from_secs(2);
pub const BACKOFF_MAX: Duration = Duration::from_secs(300);
/// Global backstop: attempts available at once...
pub const GLOBAL_BURST: u32 = 10;
/// ...and one more every this long.
pub const GLOBAL_REFILL: Duration = Duration::from_secs(120);
/// PIN mode: completed wrong guesses that invalidate the PIN.
pub const PIN_FAILURES: u32 = 5;
/// Per-address state is forgotten after this long without attempts.
const IP_FORGET: Duration = Duration::from_secs(3600);

/// The address a failure budget belongs to: the IPv4 address (also for IPv4-mapped IPv6), or
/// the IPv6 /64 (one subscriber typically owns a whole /64).
pub fn ip_key(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => match v6.to_ipv4_mapped() {
            Some(v4) => v4.into(),
            None => Ipv6Addr::from_bits(v6.to_bits() & !((1u128 << 64) - 1)).into(),
        },
        v4 => v4,
    }
}

/// A PAKE attempt was refused before any PAKE work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Denied {
    pub retry_after: Duration,
    /// The global budget refused it (else the source address's).
    pub global: bool,
    /// First refusal since this budget last admitted an attempt (report it once).
    pub newly: bool,
}

/// An admitted attempt; hand it back to [`Lockout::succeeded`] or [`Lockout::wrong_guess`].
#[derive(Debug)]
pub struct Attempt {
    ip: IpAddr,
    generation: u64,
}

impl Attempt {
    /// Password generation the attempt started under.
    pub fn generation(&self) -> u64 {
        self.generation
    }
}

#[derive(Debug, Clone)]
struct IpState {
    consecutive: u32,
    last: Instant,
    denied: bool,
}

/// PAKE failure budgets: per source address (primary) and global (backstop); see the module
/// docs for the numbers. An attempt is charged when it starts and refunded only when it
/// succeeds, so concurrent and abandoned attempts cost the same as wrong ones. PIN invalidation
/// counts only completed wrong guesses ([`wrong_guess`](Self::wrong_guess)).
#[derive(Debug, Clone, Default)]
pub struct Lockout {
    ips: HashMap<IpAddr, IpState>,
    /// `(tokens, refilled at)`; `None` = full.
    global: Option<(u32, Instant)>,
    global_denied: bool,
    failed_total: u32,
    pin_mode: bool,
    pin_failures: u32,
    generation: u64,
}

impl Lockout {
    pub fn new(pin_mode: bool) -> Self {
        Self {
            pin_mode,
            ..Self::default()
        }
    }

    /// Start an attempt from `ip` at `now` (charged as a failure until `succeeded`).
    pub fn begin(&mut self, ip: IpAddr, now: Instant) -> Result<Attempt, Denied> {
        let ip = ip_key(ip);
        self.ips
            .retain(|_, s| now.saturating_duration_since(s.last) < IP_FORGET);
        if let Some(s) = self.ips.get_mut(&ip)
            && let Some(n) = s.consecutive.checked_sub(FREE_FAILURES)
        {
            let wait = BACKOFF_MIN.saturating_mul(1 << n.min(16)).min(BACKOFF_MAX);
            let ready = s.last + wait;
            if now < ready {
                let newly = !std::mem::replace(&mut s.denied, true);
                return Err(Denied {
                    retry_after: ready - now,
                    global: false,
                    newly,
                });
            }
        }
        let (mut tokens, mut at) = self.global.unwrap_or((GLOBAL_BURST, now));
        let refills = (now.saturating_duration_since(at).as_nanos() / GLOBAL_REFILL.as_nanos())
            .min(GLOBAL_BURST as u128) as u32;
        tokens = (tokens + refills).min(GLOBAL_BURST);
        at = if tokens == GLOBAL_BURST {
            now
        } else {
            at + GLOBAL_REFILL * refills
        };
        if tokens == 0 {
            self.global = Some((0, at));
            let newly = !std::mem::replace(&mut self.global_denied, true);
            return Err(Denied {
                retry_after: (at + GLOBAL_REFILL).saturating_duration_since(now),
                global: true,
                newly,
            });
        }
        self.global = Some((tokens - 1, at));
        self.global_denied = false;
        let s = self.ips.entry(ip).or_insert(IpState {
            consecutive: 0,
            last: now,
            denied: false,
        });
        s.consecutive += 1;
        s.last = now;
        s.denied = false;
        self.failed_total += 1;
        Ok(Attempt {
            ip,
            generation: self.generation,
        })
    }

    /// The attempt delivered a wrong `conf_C` (a completed guess). Returns true when this
    /// invalidated the PIN; that also ends every attempt still in flight (a new generation),
    /// so at most [`PIN_FAILURES`] completed guesses ever test one PIN. Attempts from an older
    /// password generation are not counted.
    pub fn wrong_guess(&mut self, a: &Attempt) -> bool {
        if !self.pin_mode || a.generation != self.generation {
            return false;
        }
        self.pin_failures += 1;
        let invalidated = self.pin_failures == PIN_FAILURES;
        if invalidated {
            self.generation += 1;
        }
        invalidated
    }

    /// The attempt proved the password. Returns false (and refunds nothing) when the password
    /// changed, or the PIN was invalidated, since the attempt began: the caller must then
    /// fail it.
    pub fn succeeded(&mut self, a: Attempt) -> bool {
        if a.generation != self.generation {
            return false;
        }
        if let Some(s) = self.ips.get_mut(&a.ip) {
            s.consecutive = 0;
        }
        if let Some((t, _)) = &mut self.global {
            *t = (*t + 1).min(GLOBAL_BURST);
        }
        self.failed_total = self.failed_total.saturating_sub(1);
        true
    }

    /// Switch PIN semantics on or off (takes effect with the next [`new_secret`](Self::new_secret)).
    pub fn set_pin_mode(&mut self, on: bool) {
        self.pin_mode = on;
    }

    /// PIN mode and [`PIN_FAILURES`] completed wrong guesses since the last new secret.
    pub fn pin_invalidated(&self) -> bool {
        self.pin_mode && self.pin_failures >= PIN_FAILURES
    }

    /// Failed (or still unfinished) attempts since creation.
    pub fn failed_total(&self) -> u32 {
        self.failed_total
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// The host set a new password/PIN: a new generation (attempts in flight will fail) and
    /// the PIN counter restarts. Address and global back-offs stay: they track the attackers,
    /// not the secret.
    pub fn new_secret(&mut self) {
        self.generation += 1;
        self.pin_failures = 0;
    }
}

// ---------------------------------------------------------------------------------------------
// Invite links

pub const INVITE_VERSION: u8 = 1;
pub const INVITE_URL_PREFIX: &str = "sonobus2://join/";
/// Longest accepted link text (the largest valid invite encodes to ~750 characters).
pub const MAX_INVITE_TEXT: usize = 1024;

/// What an invite link carries: enough to dial the host and KEY-prove membership. The link
/// stops working when the host rotates the session key (`Net::rotate_session_key`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invite {
    /// Session key seed; `K = HKDF(seed)`.
    pub seed: [u8; 16],
    pub host: Id,
    pub addrs: Vec<SocketAddr>,
    pub relay: Option<String>,
    pub session: String,
    /// Encoded as one optional trailing byte (absent = mesh).
    pub mode: SessionMode,
}

impl Invite {
    pub fn key(&self) -> Key {
        key_from_seed(&self.seed)
    }

    /// Unpadded lowercase base32 (RFC 4648 alphabet).
    pub fn encode(&self) -> Result<String, wire::Error> {
        let mut b = vec![INVITE_VERSION];
        b.extend_from_slice(&self.seed);
        b.extend_from_slice(&self.host);
        wire::put_addrs(&mut b, &self.addrs)?;
        wire::put_blob(
            &mut b,
            self.relay.as_deref().unwrap_or("").as_bytes(),
            MAX_RELAY_URL,
        )?;
        wire::put_blob(&mut b, self.session.as_bytes(), MAX_NAME)?;
        if self.mode != SessionMode::Mesh {
            b.push(self.mode as u8);
        }
        Ok(base32_encode(&b))
    }

    pub fn to_url(&self) -> Result<String, wire::Error> {
        Ok(format!("{INVITE_URL_PREFIX}{}", self.encode()?))
    }

    /// Accepts the bare base32 text or the `sonobus2://join/` URL; case-insensitive, `-` and
    /// whitespace ignored. Rejects overlong text, unknown versions and trailing bytes.
    pub fn parse(s: &str) -> Result<Self, wire::Error> {
        if s.len() > MAX_INVITE_TEXT {
            return Err(wire::Error::TooLong);
        }
        let s = s.trim();
        let s = match s.get(..INVITE_URL_PREFIX.len()) {
            Some(p) if p.eq_ignore_ascii_case(INVITE_URL_PREFIX) => &s[INVITE_URL_PREFIX.len()..],
            _ => s,
        };
        let b = base32_decode(s)?;
        let mut r = Rd(&b);
        if r.u8()? != INVITE_VERSION {
            return Err(wire::Error::Invalid);
        }
        let inv = Invite {
            seed: r.arr()?,
            host: r.arr()?,
            addrs: r.addrs()?,
            relay: Some(r.string(MAX_RELAY_URL)?).filter(|s| !s.is_empty()),
            session: r.string(MAX_NAME)?,
            mode: match r.0 {
                [] => SessionMode::Mesh,
                [1] => SessionMode::Broadcast,
                _ => return Err(wire::Error::Invalid),
            },
        };
        Ok(inv)
    }
}

// ---------------------------------------------------------------------------------------------
// Rendezvous server addresses

pub const RDV_URL_PREFIX: &str = "sonobus2://rdv/";

/// Where a rendezvous server is, in the text form users paste:
/// `sonobus2://rdv/<endpoint id: 64 hex digits>@<host:port>[,<host:port>...][?relay=<url>]`
/// ([`Display`](std::fmt::Display) / [`FromStr`](std::str::FromStr), or
/// [`to_url`](Self::to_url) / [`parse`](Self::parse)). The endpoint id is iroh's own text form
/// of the server's public key.
///
/// Hosts are kept as written (IP literals or DNS names): parsing never resolves.
/// `RdvAddr::resolve` (blocking DNS: run it off the UI thread) turns it into a dialable
/// `PeerAddr`; `Net::join_by_name` / `Net::register_by_name` take an `RdvAddr` and resolve on
/// the network thread, again on every reconnect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RdvAddr {
    pub id: Id,
    /// `host:port` each (an IPv6 literal in brackets).
    pub hosts: Vec<String>,
    /// The server's home relay URL.
    pub relay: Option<String>,
}

impl RdvAddr {
    pub fn to_url(&self) -> String {
        self.to_string()
    }

    /// The `sonobus2://rdv/` prefix is optional; prefix and hex digits are case-insensitive.
    /// Each host must look like `host:port`; at least one host or a relay is required.
    pub fn parse(s: &str) -> Result<Self, String> {
        if s.len() > MAX_INVITE_TEXT {
            return Err("too long".into());
        }
        let s = s.trim();
        let s = match s.get(..RDV_URL_PREFIX.len()) {
            Some(p) if p.eq_ignore_ascii_case(RDV_URL_PREFIX) => &s[RDV_URL_PREFIX.len()..],
            _ => s,
        };
        let (s, relay) = match s.split_once("?relay=") {
            Some((_, r)) if r.is_empty() || r.len() > MAX_RELAY_URL => {
                return Err(format!("bad relay URL {r:?}"));
            }
            Some((a, r)) => (a, Some(r.to_string())),
            None => (s, None),
        };
        let (id, hosts) = s.split_once('@').unwrap_or((s, ""));
        let id = unhex32(id).ok_or("endpoint id must be 64 hex digits")?;
        let hosts: Vec<String> = hosts
            .split(',')
            .filter(|h| !h.is_empty())
            .map(Into::into)
            .collect();
        for h in &hosts {
            let ok = h.rsplit_once(':').is_some_and(|(n, p)| {
                !n.is_empty() && !n.contains(char::is_whitespace) && p.parse::<u16>().is_ok()
            });
            if !ok {
                return Err(format!("not host:port: {h:?}"));
            }
        }
        if hosts.is_empty() && relay.is_none() {
            return Err("no address".into());
        }
        Ok(RdvAddr { id, hosts, relay })
    }
}

impl std::fmt::Display for RdvAddr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(RDV_URL_PREFIX)?;
        for b in &self.id {
            write!(f, "{b:02x}")?;
        }
        write!(f, "@{}", self.hosts.join(","))?;
        match &self.relay {
            Some(r) => write!(f, "?relay={r}"),
            None => Ok(()),
        }
    }
}

impl std::str::FromStr for RdvAddr {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, String> {
        Self::parse(s)
    }
}

fn unhex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 || !s.bytes().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&s[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(out)
}

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";
const _: () =
    assert!(MAX_ADDRS * 19 + 1 + 16 + 32 + 4 + MAX_RELAY_URL + MAX_NAME < MAX_INVITE_TEXT * 5 / 8);

pub(crate) fn base32_encode(b: &[u8]) -> String {
    let (mut acc, mut bits, mut s) = (0u32, 0, String::new());
    for &x in b {
        acc = acc << 8 | x as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            s.push(B32[(acc >> bits) as usize & 31] as char);
        }
    }
    if bits > 0 {
        s.push(B32[(acc << (5 - bits)) as usize & 31] as char);
    }
    s
}

pub(crate) fn base32_decode(s: &str) -> Result<Vec<u8>, wire::Error> {
    let (mut acc, mut bits, mut out) = (0u32, 0, Vec::new());
    for c in s.bytes() {
        let v = match c.to_ascii_lowercase() {
            c @ b'a'..=b'z' => c - b'a',
            c @ b'2'..=b'7' => c - b'2' + 26,
            b'-' | b' ' | b'\n' | b'\r' | b'\t' => continue,
            _ => return Err(wire::Error::Invalid),
        };
        acc = acc << 5 | v as u32;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    // Canonical only: leftover must be < 5 bits and zero.
    if bits >= 5 || acc & ((1 << bits) - 1) != 0 {
        return Err(wire::Error::Invalid);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const IC: Id = [1; 32];
    const IA: Id = [2; 32];
    const E: Exporter = [9; 32];
    const K: Key = [5; 32];

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    /// Deterministic RNG for the PAKE known-answer test only.
    struct TestRng(u64);
    impl rand_core::TryRng for TestRng {
        type Error = std::convert::Infallible;
        fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
            Ok(self.try_next_u64()? as u32)
        }
        fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
            let mut r = crate::wire::tests::Rng(self.0);
            let v = r.next();
            self.0 = r.0;
            Ok(v)
        }
        fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
            for c in dst.chunks_mut(8) {
                let v = self.try_next_u64()?.to_le_bytes();
                c.copy_from_slice(&v[..c.len()]);
            }
            Ok(())
        }
    }
    impl rand_core::TryCryptoRng for TestRng {}

    #[test]
    fn tag_is_stable() {
        // SHA-256("sb2 tagjam")[..16], computed independently with `printf 'sb2 tagjam' | sha256sum`.
        let t = session_tag("jam");
        let want = [
            0x45, 0x66, 0xa9, 0xdc, 0x5a, 0x81, 0x84, 0x5d, 0xea, 0x84, 0xbb, 0x2d, 0xce, 0x61,
            0x40, 0xd5,
        ];
        assert_eq!(t, want);
        assert_ne!(t, session_tag("jam2"));
    }

    // Known-answer vectors, computed once with this implementation and frozen: any change of
    // labels, field order or derivation (even one applied consistently on both sides) fails.
    // The KEY proofs, seed key and wrap were re-derived independently with Python's
    // hmac/hashlib (HKDF-SHA256, empty salt).
    const KAT_KEY_PROOF_C: &str =
        "7fc7f485b1e2a82ceecebd2c8cd81a9e41b66fed30a9fe1acd7e14765d7b7e91";
    const KAT_KEY_PROOF_A: &str =
        "e69abde8f392d158be3ead54512931066f42c01e59f9358df4a89f95f84f1bfb";
    const KAT_SEED_KEY: &str = "62ffb0bc30bc72e658f548543b5e3c58a41eaba3e44616cd7e1002508d230558";
    const KAT_WRAP: &str = "4403b6f83c9469a42b556c879a9881af0554451e495da1559e98043e7bd7a0fcddb97263c4021d6463c384ba6e03c901d7e04610aa02cc7a1c3fb5f8e9e86ed7";
    const KAT_INVITE: &str = "agv2xk5lvov2xk5lvov2xk5lvov43tonzxg43tonzxg43tonzxg43tonzxg43tonzxg43tonzxg43ticatakqaic64van7uaaaaaaaaaaaaaaaaaaaaaaapxfilgq5duobztulzpojswyylzfzsxqylnobwgklykizzgszdbpeqguyln";
    const KAT_PAKE_X: &str = "41cae5728199446196bfb55729adff788eb2a0bb8ef520a1fa99029f6c1dc3f8ad";
    const KAT_PAKE_Y: &str = "42c0098721fa7100e50f0ab0b95293975abaad74c5f5e8e837cef9e8bb0b514464";
    const KAT_PAKE_CONF_C: &str =
        "2882049a109137b7be1f6284df76397963873a3f2e52c6c15660ba1635f38c7d";
    const KAT_PAKE_WRAP: &str = "9fb2f0b2cee8ae0aabf95a6245c594b652f083c5797c9eff75e1513bb4fb2723c5d4938441c8ef92557cd267f68cbd9671c6bad61b3cc300ccfd4f8998becc2a";

    #[test]
    fn known_answers() {
        let mut got = vec![
            (
                "KAT_KEY_PROOF_C",
                hex(&key_proof(&K, Side::Connector, &E, &IC, &IA)),
            ),
            (
                "KAT_KEY_PROOF_A",
                hex(&key_proof(&K, Side::Acceptor, &E, &IA, &IC)),
            ),
            ("KAT_SEED_KEY", hex(&key_from_seed(&[0xab; 16]))),
        ];
        let pk = PakeKey {
            kc: Zeroizing::new([7; 32]),
            e: E,
        };
        got.push(("KAT_WRAP", hex(&pk.wrap(&K))));
        got.push(("KAT_INVITE", invite().encode().unwrap()));
        // Full PAKE transcript with deterministic randomness on both sides.
        let (c, x) = PakeConnector::start_with_rng("hunter2", &E, &IC, &IA, TestRng(1));
        let (a, y) =
            PakeAcceptor::respond_with_rng("hunter2", &E, &IC, &IA, &x, TestRng(2)).unwrap();
        let (kc_c, conf_c) = c.finish(&y).unwrap();
        let kc_a = a.confirm(&conf_c).unwrap();
        let w = kc_a.wrap(&K);
        assert_eq!(kc_c.unwrap(&w), Ok(K));
        got.push(("KAT_PAKE_X", hex(&x)));
        got.push(("KAT_PAKE_Y", hex(&y)));
        got.push(("KAT_PAKE_CONF_C", hex(&conf_c)));
        got.push(("KAT_PAKE_WRAP", hex(&w)));
        let want = [
            KAT_KEY_PROOF_C,
            KAT_KEY_PROOF_A,
            KAT_SEED_KEY,
            KAT_WRAP,
            KAT_INVITE,
            KAT_PAKE_X,
            KAT_PAKE_Y,
            KAT_PAKE_CONF_C,
            KAT_PAKE_WRAP,
        ];
        for (name, g) in &got {
            println!("const {name}: &str = \"{g}\";");
        }
        for ((name, g), w) in got.iter().zip(want) {
            assert_eq!(g, w, "{name}");
        }
    }

    #[test]
    fn key_proof_paths() {
        let mac_c = key_proof(&K, Side::Connector, &E, &IC, &IA);
        assert_eq!(
            key_verify(&K, Side::Connector, &E, &IC, &IA, &mac_c),
            Ok(())
        );
        let mac_a = key_proof(&K, Side::Acceptor, &E, &IA, &IC);
        assert_eq!(key_verify(&K, Side::Acceptor, &E, &IA, &IC, &mac_a), Ok(()));
        // Wrong key.
        assert_eq!(
            key_verify(&[6; 32], Side::Connector, &E, &IC, &IA, &mac_c),
            Err(AuthFailed)
        );
        // Replay under another connection's exporter.
        assert_eq!(
            key_verify(&K, Side::Connector, &[8; 32], &IC, &IA, &mac_c),
            Err(AuthFailed)
        );
        // Role swap: C's mac is not A's.
        assert_eq!(
            key_verify(&K, Side::Acceptor, &E, &IC, &IA, &mac_c),
            Err(AuthFailed)
        );
        // Reflection: A cannot bounce C's mac back.
        assert_eq!(
            key_verify(&K, Side::Acceptor, &E, &IA, &IC, &mac_c),
            Err(AuthFailed)
        );
        // Wrong ids, truncated mac.
        assert_eq!(
            key_verify(&K, Side::Connector, &E, &IA, &IC, &mac_c),
            Err(AuthFailed)
        );
        assert_eq!(
            key_verify(&K, Side::Connector, &E, &IC, &IA, &mac_c[..31]),
            Err(AuthFailed)
        );
    }

    /// KEY_PROOF after a rotation: only the peer's own proof, on this connection, under this
    /// key verifies.
    #[test]
    fn rekey_proof_paths() {
        // C proves the key to A; A checks it
        let m = rekey_proof(&K, &E, &IC, &IA);
        assert!(rekey_verify(&K, &E, &IC, &IA, &m));
        // reflected: C's own proof sent back to C is not A's
        assert!(!rekey_verify(&K, &E, &IA, &IC, &m));
        assert_ne!(m, rekey_proof(&K, &E, &IA, &IC));
        // another connection's exporter
        assert!(!rekey_verify(&K, &[8; 32], &IC, &IA, &m));
        // another key
        assert!(!rekey_verify(&[6; 32], &E, &IC, &IA, &m));
        // not the handshake's KEY proof either
        assert!(!rekey_verify(
            &K,
            &E,
            &IC,
            &IA,
            &key_proof(&K, Side::Connector, &E, &IC, &IA)
        ));
        assert!(!rekey_verify(&K, &E, &IC, &IA, &m[..31]));
    }

    /// Run C and A with the given passwords up to A checking `conf_C`.
    fn pake(pw_c: &str, pw_a: &str) -> (PakeKey, Result<PakeKey, AuthFailed>) {
        let (c, x) = PakeConnector::start(pw_c, &E, &IC, &IA);
        assert_eq!(x.len(), 33);
        let (a, y) = PakeAcceptor::respond(pw_a, &E, &IC, &IA, &x).unwrap();
        let (kc_c, conf_c) = c.finish(&y).unwrap();
        (kc_c, a.confirm(&conf_c))
    }

    #[test]
    fn pake_success_and_key_wrap() {
        let (kc_c, kc_a) = pake("hunter2", "hunter2");
        let w = kc_a.unwrap().wrap(&K);
        assert_ne!(w[..32], K);
        assert_eq!(kc_c.unwrap(&w), Ok(K));
        let mut bad = w;
        bad[3] ^= 1;
        assert_eq!(kc_c.unwrap(&bad), Err(AuthFailed));
    }

    #[test]
    fn pake_wrong_password_fails_both_sides() {
        let (kc_c, r) = pake("hunter2", "hunter3");
        // Acceptor rejects conf_C...
        assert_eq!(r.err(), Some(AuthFailed));
        // ...and an acceptor with another password cannot produce a wrap the connector accepts.
        let (c, x) = PakeConnector::start("hunter2", &E, &IC, &IA);
        let (a, y) = PakeAcceptor::respond("hunter3", &E, &IC, &IA, &x).unwrap();
        let (kc_c2, _) = c.finish(&y).unwrap();
        assert_eq!(kc_c2.unwrap(&a.key.wrap(&K)), Err(AuthFailed));
        assert_eq!(kc_c.unwrap(&[0; 64]), Err(AuthFailed));
    }

    #[test]
    fn pake_tampering_and_binding() {
        // Every case: the acceptor rejects conf_C, or the connector rejects the wrap.
        let check = |x_tamper: bool, y_tamper: bool, e_a: Exporter, id_a2: Id| {
            let (c, mut x) = PakeConnector::start("pw", &E, &IC, &IA);
            if x_tamper {
                x[5] ^= 1;
            }
            let (a, mut y) = match PakeAcceptor::respond("pw", &e_a, &IC, &id_a2, &x) {
                Ok(v) => v,
                // Tampered X not on the curve: rejected outright.
                Err(e) => return assert!(x_tamper && e == AuthFailed),
            };
            if y_tamper {
                y[7] ^= 1;
            }
            let (kc_c, conf_c) = match c.finish(&y) {
                Ok(v) => v,
                Err(e) => return assert!(y_tamper && e == AuthFailed),
            };
            let wrap = a.key.wrap(&K);
            assert_eq!(kc_c.unwrap(&wrap), Err(AuthFailed), "connector accepted");
            assert!(a.confirm(&conf_c).is_err(), "acceptor accepted");
        };
        check(true, false, E, IA); // tampered X
        check(false, true, E, IA); // tampered Y
        check(false, false, [1; 32], IA); // relayed into another connection
        check(false, false, E, [3; 32]); // different identities
        // Role swap: the acceptor's own wrap MAC is not a conf_C.
        let (c, x) = PakeConnector::start("pw", &E, &IC, &IA);
        let (a, y) = PakeAcceptor::respond("pw", &E, &IC, &IA, &x).unwrap();
        assert!(c.finish(&y).is_ok());
        let w = a.key.wrap(&K);
        assert!(a.confirm(&w[32..]).is_err());
        // Garbage X.
        assert!(PakeAcceptor::respond("pw", &E, &IC, &IA, &[0x41; 10]).is_err());
        // The tamper cases must actually reach the checks most of the time.
        let mut reached = 0;
        for _ in 0..20 {
            let (_, mut x) = PakeConnector::start("pw", &E, &IC, &IA);
            x[5] ^= 1;
            reached += PakeAcceptor::respond("pw", &E, &IC, &IA, &x).is_ok() as u32;
        }
        assert!(reached > 0);
    }

    #[test]
    fn secrets_are_normalised() {
        let n = |s| normalize_secret(s).map(|z| z.to_string());
        // NFC: "é" precomposed vs e + combining acute.
        assert_eq!(n("caf\u{e9}"), n("cafe\u{301}"));
        assert_eq!(n("  hunter2\t\n").unwrap(), "hunter2");
        assert_eq!(n("123 456").unwrap(), "123456");
        assert_eq!(n(" 12-34-56 ").unwrap(), "123456");
        assert_eq!(n("12 ab").unwrap(), "12 ab");
        assert_eq!(n("  "), Err(SecretError::Empty));
        assert_eq!(n("---").unwrap(), "---");
        assert_eq!(normalize_pin("123-456").unwrap().as_str(), "123456");
        assert_eq!(normalize_pin("12a4").err(), Some(SecretError::NotDigits));
        assert_eq!(normalize_pin("١٢٣").err(), Some(SecretError::NotDigits));
    }

    #[test]
    fn ip_keys() {
        let k = |s: &str| ip_key(s.parse().unwrap());
        assert_eq!(k("10.1.2.3"), k("10.1.2.3"));
        assert_ne!(k("10.1.2.3"), k("10.1.2.4"));
        assert_eq!(k("2001:db8:1:2::1"), k("2001:db8:1:2:ffff::9"));
        assert_ne!(k("2001:db8:1:2::1"), k("2001:db8:1:3::1"));
        assert_eq!(k("::ffff:10.1.2.3"), k("10.1.2.3"));
    }

    const IP1: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 1));
    const IP2: IpAddr = IpAddr::V4(std::net::Ipv4Addr::new(10, 0, 0, 2));

    fn ip(n: u32) -> IpAddr {
        IpAddr::V4(std::net::Ipv4Addr::from_bits(0x0b00_0000 + n))
    }

    #[test]
    fn lockout_per_ip_backoff() {
        let t = Instant::now();
        let s = Duration::from_secs;
        let mut l = Lockout::new(false);
        for i in 0..3 {
            assert!(l.begin(IP1, t + s(i)).is_ok());
        }
        assert_eq!(l.failed_total(), 3);
        // Third failure at t+2: next attempt waits 2 s; only the first refusal is "newly".
        let d = l.begin(IP1, t + s(3)).unwrap_err();
        assert_eq!((d.retry_after, d.global, d.newly), (s(1), false, true));
        assert!(!l.begin(IP1, t + s(3)).unwrap_err().newly);
        // Another address is not affected.
        assert!(l.begin(IP2, t + s(3)).is_ok());
        assert!(l.begin(IP1, t + s(4)).is_ok());
        // Fourth failure: 4 s.
        assert_eq!(l.begin(IP1, t + s(7)).unwrap_err().retry_after, s(1));
        let a = l.begin(IP1, t + s(8)).unwrap();
        // Success clears the address's back-off and refunds the failure.
        let before = l.failed_total();
        assert!(l.succeeded(a));
        assert_eq!(l.failed_total(), before - 1);
        assert!(l.begin(IP1, t + s(8)).is_ok());
    }

    #[test]
    fn lockout_backoff_caps_at_max() {
        let t = Instant::now();
        let mut l = Lockout::new(false);
        let mut now = t;
        for _ in 0..20 {
            now += BACKOFF_MAX.max(GLOBAL_REFILL);
            assert!(l.begin(IP1, now).is_ok());
        }
        let d = l.begin(IP1, now + Duration::from_secs(299)).unwrap_err();
        assert_eq!(d.retry_after, Duration::from_secs(1));
    }

    #[test]
    fn lockout_global_backstop() {
        let t = Instant::now();
        let mut l = Lockout::new(false);
        // Many addresses, one attempt each: the global bucket runs dry after GLOBAL_BURST.
        for n in 0..GLOBAL_BURST {
            assert!(l.begin(ip(n), t).is_ok(), "{n}");
        }
        let d = l.begin(ip(100), t).unwrap_err();
        assert!(d.global && d.newly);
        assert_eq!(d.retry_after, GLOBAL_REFILL);
        assert!(!l.begin(ip(101), t).unwrap_err().newly);
        // One more per GLOBAL_REFILL.
        assert!(l.begin(ip(102), t + GLOBAL_REFILL).is_ok());
        assert!(l.begin(ip(103), t + GLOBAL_REFILL).is_err());
        // Distributed attacker over one hour: at most burst + 3600 s / refill attempts.
        let mut ok = 0;
        let mut now = t + GLOBAL_REFILL;
        let end = now + Duration::from_secs(3600);
        let mut n = 1000;
        while now <= end {
            n += 1;
            ok += l.begin(ip(n), now).is_ok() as u32;
            now += Duration::from_secs(1);
        }
        assert_eq!(ok, 3600 / GLOBAL_REFILL.as_secs() as u32);
        // A success refunds its token.
        let mut l = Lockout::new(false);
        for n in 0..GLOBAL_BURST * 3 {
            let a = l.begin(ip(n), t).unwrap();
            assert!(l.succeeded(a));
        }
    }

    #[test]
    fn lockout_pin_counts_only_completed_guesses() {
        let t = Instant::now();
        let mut l = Lockout::new(true);
        // Abandoned attempts (never wrong_guess) from many addresses: the PIN survives.
        for n in 0..8 {
            let _abandoned = l.begin(ip(n), t).unwrap();
        }
        assert!(!l.pin_invalidated());
        let mut t2 = t + GLOBAL_REFILL * 20;
        for i in 0..PIN_FAILURES {
            let a = l.begin(ip(50 + i), t2).unwrap();
            assert_eq!(l.wrong_guess(&a), i + 1 == PIN_FAILURES);
            t2 += GLOBAL_REFILL;
        }
        assert!(l.pin_invalidated());
        l.new_secret();
        assert!(!l.pin_invalidated());
        // Successful joins do not use up the PIN.
        let mut l = Lockout::new(true);
        for _ in 0..10 {
            let a = l.begin(IP1, t).unwrap();
            assert!(l.succeeded(a));
        }
        assert!(l.begin(IP1, t).is_ok() && !l.pin_invalidated());
        // Password mode never invalidates.
        let mut l = Lockout::new(false);
        let a = l.begin(IP1, t).unwrap();
        assert!(!l.wrong_guess(&a));
    }

    #[test]
    fn lockout_generation() {
        let t = Instant::now();
        let mut l = Lockout::new(true);
        let a = l.begin(IP1, t).unwrap();
        let b = l.begin(IP2, t).unwrap();
        l.new_secret();
        // An attempt that began under the old password fails closed and refunds nothing...
        let before = l.failed_total();
        assert!(!l.succeeded(a));
        assert_eq!(l.failed_total(), before);
        // ...and its wrong guess does not count against the new PIN.
        assert!(!l.wrong_guess(&b));
        assert_eq!(l.pin_failures, 0);
    }

    /// Six attempts in flight at once: the fifth completed wrong guess invalidates the PIN and
    /// the outstanding sixth, even with the right PIN, no longer succeeds.
    #[test]
    fn pin_invalidation_ends_attempts_in_flight() {
        let t = Instant::now();
        let mut l = Lockout::new(true);
        let mut attempts: Vec<Attempt> = (0..6).map(|n| l.begin(ip(n), t).unwrap()).collect();
        let last = attempts.pop().unwrap();
        for (i, a) in attempts.iter().enumerate() {
            assert_eq!(l.wrong_guess(a), i + 1 == PIN_FAILURES as usize);
        }
        assert!(l.pin_invalidated());
        assert!(
            !l.succeeded(last),
            "an attempt begun before the invalidation succeeded"
        );
        // a new PIN works again
        l.new_secret();
        let a = l.begin(ip(9), t).unwrap();
        assert!(l.succeeded(a));
    }

    fn invite() -> Invite {
        Invite {
            seed: [0xab; 16],
            host: [0xcd; 32],
            addrs: vec![
                "192.168.1.2:10999".parse().unwrap(),
                "[fe80::1]:10999".parse().unwrap(),
            ],
            relay: Some("https://relay.example/".into()),
            session: "Friday jam".into(),
            mode: SessionMode::Mesh,
        }
    }

    #[test]
    fn invite_round_trip() {
        let i = invite();
        let s = i.encode().unwrap();
        assert!(s.bytes().all(|c| B32.contains(&c)));
        assert_eq!(Invite::parse(&s), Ok(i.clone()));
        assert_eq!(Invite::parse(&s.to_uppercase()), Ok(i.clone()));
        let grouped: String = s
            .as_bytes()
            .chunks(5)
            .map(|c| std::str::from_utf8(c).unwrap())
            .collect::<Vec<_>>()
            .join("-");
        assert_eq!(Invite::parse(&grouped), Ok(i.clone()));
        let url = i.to_url().unwrap();
        assert!(url.starts_with("sonobus2://join/"));
        assert_eq!(Invite::parse(&url), Ok(i.clone()));
        let min = Invite {
            addrs: vec![],
            relay: None,
            ..i.clone()
        };
        assert_eq!(Invite::parse(&min.encode().unwrap()), Ok(min.clone()));
        assert_eq!(i.key(), min.key());
        // Broadcast adds one trailing byte; any other trailing byte is refused.
        let bc = Invite {
            mode: SessionMode::Broadcast,
            ..i.clone()
        };
        let bs = bc.encode().unwrap();
        assert_eq!(Invite::parse(&bs), Ok(bc.clone()));
        let mut raw = base32_decode(&bs).unwrap();
        assert_eq!(raw.last(), Some(&1));
        *raw.last_mut().unwrap() = 0;
        assert!(Invite::parse(&base32_encode(&raw)).is_err());
        assert_ne!(i.key(), Invite { seed: [0; 16], ..i }.key());
        // Golden base32 (RFC 4648 test vector, lowercase, unpadded).
        assert_eq!(base32_encode(b"foobar"), "mzxw6ytboi");
        assert_eq!(base32_decode("MZXW6YTBOI"), Ok(b"foobar".to_vec()));
    }

    #[test]
    fn invite_rejects() {
        let s = invite().encode().unwrap();
        for n in 0..s.len() {
            assert!(Invite::parse(&s[..n]).is_err(), "prefix {n}");
        }
        assert_eq!(
            Invite::parse(&"a".repeat(MAX_INVITE_TEXT + 1)),
            Err(wire::Error::TooLong)
        );
        assert!(Invite::parse(&format!("{s}aaaaaaaa")).is_err()); // trailing bytes
        assert!(Invite::parse("sonobus2://join/!!!").is_err());
        assert!(Invite::parse("https://example.com/").is_err());
        let mut v = base32_decode(&s).unwrap();
        v[0] = 2;
        assert_eq!(Invite::parse(&base32_encode(&v)), Err(wire::Error::Invalid));
        assert_eq!(base32_decode("mzxw6ytboj"), Err(wire::Error::Invalid)); // non-zero pad bits
        assert_eq!(base32_decode("a"), Err(wire::Error::Invalid));
        let mut rng = crate::wire::tests::Rng(5);
        for _ in 0..5000 {
            let n = rng.below(200) as usize;
            let t: String = (0..n)
                .map(|_| B32[rng.below(32) as usize] as char)
                .collect();
            let _ = Invite::parse(&t);
        }
    }

    #[test]
    fn rdv_addr_golden_and_round_trip() {
        let mut id = [0u8; 32];
        id[0] = 0xab;
        id[31] = 0x01;
        let hexid = format!("ab{}01", "0".repeat(60));
        let a = RdvAddr {
            id,
            hosts: vec![
                "192.0.2.7:10999".into(),
                "[2001:db8::1]:10999".into(),
                "rdv.example.org:10999".into(),
            ],
            relay: Some("https://relay.example/".into()),
        };
        let text = format!(
            "sonobus2://rdv/{hexid}@192.0.2.7:10999,[2001:db8::1]:10999,rdv.example.org:10999\
             ?relay=https://relay.example/"
        );
        assert_eq!(a.to_url(), text);
        assert_eq!(a.to_string(), text);
        assert_eq!(RdvAddr::parse(&text), Ok(a.clone()));
        assert_eq!(text.parse::<RdvAddr>(), Ok(a.clone()));
        // prefix optional and case-insensitive, upper-case hex, surrounding blanks
        let bare = format!(" {}@rdv.example.org:1\n", hexid.to_uppercase());
        let b = RdvAddr::parse(&bare).unwrap();
        assert_eq!(
            (b.id, &b.hosts[..], &b.relay),
            (id, &["rdv.example.org:1".to_string()][..], &None)
        );
        assert_eq!(
            RdvAddr::parse(&format!("SONOBUS2://RDV/{hexid}@h:1"))
                .unwrap()
                .id,
            id
        );
        // relay only
        let r = RdvAddr {
            hosts: vec![],
            ..a.clone()
        };
        assert_eq!(
            r.to_url(),
            format!("sonobus2://rdv/{hexid}@?relay=https://relay.example/")
        );
        assert_eq!(RdvAddr::parse(&r.to_url()), Ok(r));
        assert!(RdvAddr::parse(&format!("{hexid}?relay=http://r/")).is_ok());
        // refusals
        assert_eq!(RdvAddr::parse(&hexid), Err("no address".into()));
        assert!(RdvAddr::parse("abc@192.0.2.7:1").is_err(), "short id");
        assert!(
            RdvAddr::parse(&format!("{}x@h:1", &hexid[1..])).is_err(),
            "not hex"
        );
        for bad in ["192.0.2.7", ":1", "h:99999", "h:", "a b:1"] {
            assert!(RdvAddr::parse(&format!("{hexid}@{bad}")).is_err(), "{bad}");
        }
        assert!(RdvAddr::parse(&format!("{hexid}@h:1?relay=")).is_err());
        assert!(RdvAddr::parse(&format!("{hexid}@h:1?relay={}", "r".repeat(300))).is_err());
        assert!(RdvAddr::parse(&format!("{hexid}@{}", "h:1,".repeat(300))).is_err());
        // another prefix is not stripped
        assert!(RdvAddr::parse(&format!("sonobus2://join/{hexid}@h:1")).is_err());
    }
}
