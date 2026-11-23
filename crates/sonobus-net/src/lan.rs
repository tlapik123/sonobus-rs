//! LAN session discovery over mDNS / DNS-SD (`mdns-sd`, its own thread, no tokio).
//!
//! A host advertises service `_sonobus2._udp.local.`: instance name = its endpoint id
//! (lowercase base32), SRV/A/AAAA = its QUIC addresses, TXT `sb` = the [`Advert`]
//! (`v1;<session name>;<tag hex>;<pin|pw>`) and optionally TXT `relay` = its home relay URL.
//! Nothing in it is derived from the password, PIN or session key; the tag is a hash of the
//! (public) session name only. Joiners [`browse`](Lan::browse) for that service and get
//! [`LanEvent`]s; joining then runs the normal PAKE with the typed PIN or password
//! (`Net::join_discovered`). Anyone on the LAN sees advertised session names and host
//! addresses: that is what advertising means.
//!
//! [`Advert`] and [`Browser`] are sans-IO; [`Lan`] drives them with an mDNS daemon.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::time::{Duration, Instant};

use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};

use crate::auth::{self, Id};
use crate::rt::PeerAddr;
use crate::wire::{MAX_ADDRS, MAX_NAME, MAX_RELAY_URL};

pub const SERVICE_TYPE: &str = "_sonobus2._udp.local.";
/// TXT key of the [`Advert`].
pub const TXT_ADVERT: &str = "sb";
/// TXT key of the host's home relay URL.
pub const TXT_RELAY: &str = "relay";
/// Sessions a [`Browser`] tracks at once (more are ignored until some disappear).
pub const MAX_SESSIONS: usize = 256;

/// What a joiner must type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthKind {
    /// A 6-digit PIN shown on the host (PIN semantics: a few wrong guesses invalidate it).
    Pin,
    /// The session password.
    Password,
}

/// The TXT advertisement of a hosted session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Advert {
    /// Display session name (also what the PAKE's session tag is computed from).
    pub session: String,
    pub auth: AuthKind,
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

impl Advert {
    /// `v1;<session>;<tag hex>;<pin|pw>`; `None` when the name is over [`MAX_NAME`] bytes.
    pub fn encode(&self) -> Option<String> {
        if self.session.len() > MAX_NAME {
            return None;
        }
        let auth = match self.auth {
            AuthKind::Pin => "pin",
            AuthKind::Password => "pw",
        };
        let tag = hex(&auth::session_tag(&self.session));
        Some(format!("v1;{};{tag};{auth}", self.session))
    }

    /// Inverse of [`encode`](Self::encode). The tag must match the name (the name may contain
    /// `;`); unknown versions and auth kinds are refused.
    pub fn decode(s: &str) -> Option<Advert> {
        let rest = s.strip_prefix("v1;")?;
        let mut it = rest.rsplitn(3, ';');
        let (auth, tag, session) = (it.next()?, it.next()?, it.next()?);
        let auth = match auth {
            "pin" => AuthKind::Pin,
            "pw" => AuthKind::Password,
            _ => return None,
        };
        (session.len() <= MAX_NAME && tag == hex(&auth::session_tag(session))).then(|| Advert {
            session: session.into(),
            auth,
        })
    }
}

/// A session advertised on the LAN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSession {
    pub session: String,
    pub auth: AuthKind,
    /// The host: endpoint id (proven by TLS when connecting), addresses, relay.
    pub host: PeerAddr,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LanEvent {
    /// A session appeared or its advertisement changed.
    Found(DiscoveredSession),
    /// The host stopped advertising (or its record expired).
    Lost(Id),
}

/// Endpoint id of an instance name (lowercase base32 of the id).
fn instance_id(instance: &str) -> Option<Id> {
    auth::base32_decode(instance).ok()?.try_into().ok()
}

/// Instance name of an endpoint id.
pub fn instance_name(id: &Id) -> String {
    auth::base32_encode(id)
}

/// Browse state machine: resolved/removed mDNS records in, de-duplicated [`LanEvent`]s out.
#[derive(Debug, Default)]
pub struct Browser {
    sessions: HashMap<Id, DiscoveredSession>,
}

impl Browser {
    /// A resolved record of `instance` with TXT `sb` = `advert` and TXT `relay`.
    pub fn resolved(
        &mut self,
        instance: &str,
        advert: Option<&str>,
        mut addrs: Vec<SocketAddr>,
        relay: Option<&str>,
    ) -> Option<LanEvent> {
        let id = instance_id(instance)?;
        let Some(ad) = advert.and_then(Advert::decode) else {
            // A record that no longer carries a valid advert ends the session.
            return self.removed(instance);
        };
        if !self.sessions.contains_key(&id) && self.sessions.len() >= MAX_SESSIONS {
            return None;
        }
        addrs.sort();
        addrs.dedup();
        addrs.truncate(MAX_ADDRS);
        let s = DiscoveredSession {
            session: ad.session,
            auth: ad.auth,
            host: PeerAddr {
                id,
                addrs,
                relay: relay
                    .filter(|r| !r.is_empty() && r.len() <= MAX_RELAY_URL)
                    .map(Into::into),
            },
        };
        if self.sessions.get(&id) == Some(&s) {
            return None;
        }
        self.sessions.insert(id, s.clone());
        Some(LanEvent::Found(s))
    }

    pub fn removed(&mut self, instance: &str) -> Option<LanEvent> {
        let id = instance_id(instance)?;
        self.sessions.remove(&id).map(|_| LanEvent::Lost(id))
    }

    pub fn sessions(&self) -> impl Iterator<Item = &DiscoveredSession> {
        self.sessions.values()
    }
}

fn io(e: mdns_sd::Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

/// An mDNS daemon for advertising our session and browsing for others.
pub struct Lan {
    daemon: ServiceDaemon,
    advertised: std::sync::Mutex<Option<String>>,
}

impl Lan {
    /// mDNS on every interface (the LAN).
    pub fn new() -> std::io::Result<Lan> {
        Ok(Lan {
            daemon: ServiceDaemon::new().map_err(io)?,
            advertised: Default::default(),
        })
    }

    /// mDNS on IPv4 loopback only: sessions on this machine (and tests). Needs a loopback
    /// interface that supports multicast.
    pub fn loopback() -> std::io::Result<Lan> {
        let lan = Lan::new()?;
        lan.daemon.disable_interface(IfKind::All).map_err(io)?;
        lan.daemon
            .enable_interface(IfKind::LoopbackV4)
            .map_err(io)?;
        Ok(lan)
    }

    /// Advertise a session hosted at `host` (its direct addresses; those on the first one's
    /// port), replacing any previous advertisement.
    pub fn advertise(&self, host: &PeerAddr, advert: &Advert) -> std::io::Result<()> {
        let invalid =
            |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidInput, m.to_string());
        let ad = advert
            .encode()
            .ok_or_else(|| invalid("session name too long"))?;
        let port = host
            .addrs
            .first()
            .ok_or_else(|| invalid("no direct address to advertise"))?
            .port();
        let ips: Vec<IpAddr> = host
            .addrs
            .iter()
            .filter(|a| a.port() == port)
            .map(|a| a.ip())
            .collect();
        let name = instance_name(&host.id);
        let mut txt = vec![(TXT_ADVERT, ad.as_str())];
        if let Some(r) = &host.relay {
            txt.push((TXT_RELAY, r.as_str()));
        }
        let info = ServiceInfo::new(
            SERVICE_TYPE,
            &name,
            &format!("{name}.local."),
            &ips[..],
            port,
            &txt[..],
        )
        .map_err(io)?;
        self.stop_advertising();
        let full = info.get_fullname().to_string();
        self.daemon.register(info).map_err(io)?;
        *self.advertised.lock().unwrap_or_else(|e| e.into_inner()) = Some(full);
        Ok(())
    }

    /// Withdraw the advertisement (browsers get a goodbye and report [`LanEvent::Lost`]).
    pub fn stop_advertising(&self) {
        let prev = self
            .advertised
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take();
        if let Some(full) = prev
            && let Ok(done) = self.daemon.unregister(&full)
        {
            let _ = done.recv_timeout(Duration::from_secs(1));
        }
    }

    /// Start browsing for sessions.
    pub fn browse(&self) -> std::io::Result<LanBrowser> {
        Ok(LanBrowser {
            rx: self.daemon.browse(SERVICE_TYPE).map_err(io)?,
            state: Browser::default(),
        })
    }
}

impl Drop for Lan {
    fn drop(&mut self) {
        self.stop_advertising();
        let _ = self.daemon.shutdown();
    }
}

/// Discovered sessions as they come and go.
pub struct LanBrowser {
    rx: mdns_sd::Receiver<ServiceEvent>,
    state: Browser,
}

impl LanBrowser {
    /// Next change within `timeout`.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Option<LanEvent> {
        let end = Instant::now() + timeout;
        loop {
            let left = end.saturating_duration_since(Instant::now());
            let e = self.rx.recv_timeout(left).ok()?;
            let instance = |full: &str| {
                full.strip_suffix(SERVICE_TYPE)
                    .and_then(|s| s.strip_suffix('.'))
                    .map(String::from)
            };
            let ev = match e {
                ServiceEvent::ServiceResolved(r) => instance(&r.fullname).and_then(|i| {
                    let addrs = r
                        .addresses
                        .iter()
                        .map(|a| SocketAddr::new(a.to_ip_addr(), r.port))
                        .collect();
                    self.state.resolved(
                        &i,
                        r.get_property_val_str(TXT_ADVERT),
                        addrs,
                        r.get_property_val_str(TXT_RELAY),
                    )
                }),
                ServiceEvent::ServiceRemoved(_, full) => {
                    instance(&full).and_then(|i| self.state.removed(&i))
                }
                _ => None,
            };
            if ev.is_some() {
                return ev;
            }
        }
    }

    /// Sessions currently known.
    pub fn sessions(&self) -> impl Iterator<Item = &DiscoveredSession> {
        self.state.sessions()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn advert_golden_and_round_trip() {
        let a = Advert {
            session: "jam".into(),
            auth: AuthKind::Pin,
        };
        // tag = SHA-256("sb2 tagjam")[..16], see auth::tests::tag_is_stable
        let s = a.encode().unwrap();
        assert_eq!(s, "v1;jam;4566a9dc5a81845dea84bb2dce6140d5;pin");
        assert_eq!(Advert::decode(&s), Some(a));
        let b = Advert {
            session: "a;b;c".into(),
            auth: AuthKind::Password,
        };
        assert_eq!(Advert::decode(&b.encode().unwrap()), Some(b));
        for bad in [
            "v2;jam;4566a9dc5a81845dea84bb2dce6140d5;pin",
            "v1;jam;4566a9dc5a81845dea84bb2dce6140d5;key",
            "v1;jan;4566a9dc5a81845dea84bb2dce6140d5;pin", // tag does not match the name
            "v1;jam;pin",
            "v1;",
            "",
        ] {
            assert_eq!(Advert::decode(bad), None, "{bad}");
        }
        let long = Advert {
            session: "x".repeat(MAX_NAME + 1),
            auth: AuthKind::Pin,
        };
        assert_eq!(long.encode(), None);
    }

    #[test]
    fn browser_state_machine() {
        let mut b = Browser::default();
        let id = [7u8; 32];
        let inst = instance_name(&id);
        assert_eq!(inst.len(), 52);
        let ad = |name: &str, auth| {
            Advert {
                session: name.into(),
                auth,
            }
            .encode()
            .unwrap()
        };
        let a1: SocketAddr = "192.168.1.5:10999".parse().unwrap();
        let a2: SocketAddr = "[fe80::5]:10999".parse().unwrap();
        // Appears (addresses sorted, duplicates dropped).
        let ev = b.resolved(
            &inst,
            Some(&ad("jam", AuthKind::Pin)),
            vec![a2, a1, a1],
            None,
        );
        let want = DiscoveredSession {
            session: "jam".into(),
            auth: AuthKind::Pin,
            host: PeerAddr {
                id,
                addrs: vec![a1, a2],
                relay: None,
            },
        };
        assert_eq!(ev, Some(LanEvent::Found(want.clone())));
        // The same record again: nothing new.
        assert_eq!(
            b.resolved(&inst, Some(&ad("jam", AuthKind::Pin)), vec![a1, a2], None),
            None
        );
        // Changed (PIN → password, relay added): reported again.
        let ev = b.resolved(
            &inst,
            Some(&ad("jam", AuthKind::Password)),
            vec![a1],
            Some("http://r/"),
        );
        let Some(LanEvent::Found(s)) = ev else {
            panic!("{ev:?}")
        };
        assert_eq!(
            (s.auth, s.host.relay.as_deref()),
            (AuthKind::Password, Some("http://r/"))
        );
        // Records that are not ours / not ids are ignored.
        assert_eq!(
            b.resolved("printer", Some(&ad("x", AuthKind::Pin)), vec![], None),
            None
        );
        let other = instance_name(&[8u8; 32]);
        assert_eq!(b.resolved(&other, Some("garbage"), vec![a1], None), None);
        assert_eq!(b.resolved(&other, None, vec![a1], None), None);
        // A known host whose advert turns invalid is lost; removal reports once.
        assert!(
            b.resolved(&other, Some(&ad("y", AuthKind::Pin)), vec![a1], None)
                .is_some()
        );
        assert_eq!(
            b.resolved(&other, Some("v1;bad"), vec![a1], None),
            Some(LanEvent::Lost([8u8; 32]))
        );
        assert_eq!(b.removed(&inst), Some(LanEvent::Lost(id)));
        assert_eq!(b.removed(&inst), None);
        assert_eq!(b.sessions().count(), 0);
        // Bounded.
        for i in 0..MAX_SESSIONS + 10 {
            let mut id = [0u8; 32];
            id[..8].copy_from_slice(&(i as u64).to_le_bytes());
            b.resolved(
                &instance_name(&id),
                Some(&ad("s", AuthKind::Pin)),
                vec![a1],
                None,
            );
        }
        assert_eq!(b.sessions().count(), MAX_SESSIONS);
    }
}
