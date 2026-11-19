//! Name + password joins through a rendezvous server (standalone and embedded in a `Net`), and
//! the server's limits against a raw client. Loopback only, relays disabled.

mod common;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use bytes::Bytes;
use common::*;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::{Endpoint, EndpointAddr, EndpointId, TransportAddr};
use sonobus_net::auth::{self, Id};
use sonobus_net::rdv::{Limits, Msg, RDV_ALPN};
use sonobus_net::rt::CLOSE_AUTH;
use sonobus_net::wire::Role;
use sonobus_net::rdv::Refusals;
use sonobus_net::{
    Config, ConnId, Event, Net, PeerAddr, PeerInfo, RdvAddr, RelayChoice, RendezvousServer,
};

const NAME: &str = "Friday jam";
const PW: &str = "open sesame";

struct Node {
    net: Net,
    live: HashMap<ConnId, PeerInfo>,
    failed: Vec<String>,
}

impl Node {
    fn new(net: Net) -> Node {
        Node {
            net,
            live: HashMap::new(),
            failed: vec![],
        }
    }
    fn id(&self) -> Id {
        self.net.local_addr().id
    }
    fn pump(&mut self) {
        while let Ok(e) = self.net.events().try_recv() {
            match e {
                Event::Connected { conn, peer, .. } => {
                    self.live.insert(conn, peer);
                }
                Event::Closed { conn, .. } => {
                    self.live.remove(&conn);
                }
                Event::ConnectFailed { reason, .. } => self.failed.push(reason),
                _ => {}
            }
        }
    }
    fn peers(&self) -> Vec<Id> {
        let mut v: Vec<Id> = self.live.values().map(|p| p.id).collect();
        v.sort();
        v
    }
}

fn until(nodes: &mut [&mut Node], done: impl Fn(&[&mut Node]) -> bool) {
    let end = Instant::now() + WAIT;
    loop {
        nodes.iter_mut().for_each(|n| n.pump());
        if done(nodes) {
            return;
        }
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn full_mesh(nodes: &[&mut Node]) -> bool {
    nodes.iter().all(|n| {
        let mut want: Vec<Id> = nodes.iter().map(|m| m.id()).filter(|&i| i != n.id()).collect();
        want.sort();
        n.peers() == want
    })
}

fn host(ip: u8) -> Node {
    Node::new(simple(Config {
        bind_ip: lo(ip).into(),
        session: NAME.into(),
        role: Role::Host,
        ..cfg("host", None, Some(PW))
    }))
}

/// A newcomer that knows only the name and password (started under another session name).
fn joiner(ip: u8) -> Node {
    Node::new(simple(Config {
        bind_ip: lo(ip).into(),
        ..cfg(&format!("j{ip}"), None, None)
    }))
}

#[test]
fn join_by_name_through_standalone_server() {
    let server = RendezvousServer::start(
        lo(1).into(),
        0,
        None,
        RelayChoice::None,
        Limits::default(),
    )
    .unwrap();
    let srv = server.addr();
    // The text form users paste, parsed back (no resolution involved for IP literals).
    let text = RdvAddr::from(srv.clone()).to_url();
    let rdv: RdvAddr = text.parse().unwrap();
    assert_eq!(rdv.resolve(), Ok(srv.clone()));
    let mut h = host(2);
    h.net.register_by_name(rdv.clone());
    let mut a = joiner(3);
    a.net.join_by_name(rdv, NAME, PW).unwrap();
    until(&mut [&mut h, &mut a], full_mesh);
    let mut b = joiner(4);
    b.net.join_by_name(srv.clone(), NAME, PW).unwrap();
    until(&mut [&mut h, &mut a, &mut b], full_mesh);
    let c = server.counts();
    assert_eq!((c.tags, c.registrations, c.refused), (1, 3, Refusals::default()), "{c:?}");
    // The host leaves; the members stay registered, so a newcomer still finds one of them.
    drop(h);
    let mut c = joiner(5);
    c.net.join_by_name(srv.clone(), NAME, PW).unwrap();
    until(&mut [&mut a, &mut b, &mut c], full_mesh);
    // A wrong password gets nowhere.
    let mut d = joiner(6);
    d.net.join_by_name(srv, NAME, "wrong").unwrap();
    until(&mut [&mut d], |n| !n[0].failed.is_empty());
    assert_eq!(d.failed[0], "authentication failed");
    assert!(d.live.is_empty());
}

#[test]
fn join_by_name_through_embedded_server() {
    let mut h = host(2);
    h.net.serve_rendezvous(Limits::default());
    let srv = h.net.local_addr();
    let mut a = joiner(3);
    a.net.join_by_name(srv.clone(), NAME, PW).unwrap();
    let mut b = joiner(4);
    b.net.join_by_name(srv, NAME, PW).unwrap();
    until(&mut [&mut h, &mut a, &mut b], full_mesh);
    // Session connections only: the rendezvous connections are not members.
    assert_eq!(h.live.len(), 2);
}

// ---------------------------------------------------------------------------------------------
// Raw client against the server's limits

struct Raw {
    conn: Connection,
    send: SendStream,
    recv: RecvStream,
}

async fn raw(ep: &Endpoint, to: &PeerAddr) -> Result<Raw, String> {
    let id = EndpointId::from_bytes(&to.id).unwrap();
    let addr = EndpointAddr::from_parts(id, to.addrs.iter().copied().map(TransportAddr::Ip));
    let conn = ep.connect(addr, RDV_ALPN).await.map_err(|e| e.to_string())?;
    let (send, recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    Ok(Raw { conn, send, recv })
}

async fn send(r: &mut Raw, m: &Msg) {
    let mut b = Vec::new();
    m.encode(&mut b).unwrap();
    r.send.write_all(&b).await.unwrap();
}

async fn recv(r: &mut Raw) -> Result<Msg, String> {
    let mut h = [0u8; 3];
    r.recv.read_exact(&mut h).await.map_err(|e| e.to_string())?;
    let mut body = vec![0; u16::from_le_bytes([h[0], h[1]]) as usize];
    r.recv.read_exact(&mut body).await.map_err(|e| e.to_string())?;
    Msg::decode_body(h[2], &body).map_err(|e| e.to_string())
}

fn me(ep: &Endpoint) -> PeerAddr {
    PeerAddr {
        id: *ep.id().as_bytes(),
        addrs: vec![],
        relay: None,
    }
}

#[test]
fn server_limits_against_raw_clients() {
    let limits = Limits {
        per_ip: 4,
        register_timeout: Duration::from_millis(500),
        idle: Duration::from_secs(2),
        fanout_interval: Duration::from_millis(300),
        ..Limits::default()
    };
    let server = RendezvousServer::start(lo(1).into(), 0, None, RelayChoice::None, limits).unwrap();
    let srv = server.addr();
    let tag = auth::session_tag(NAME);
    block_on(async {
        // A REGISTER naming another endpoint is refused.
        let ep = raw_endpoint(lo(20)).await;
        let mut r = raw(&ep, &srv).await.unwrap();
        let mut liar = me(&ep);
        liar.id = [9; 32];
        send(&mut r, &Msg::Register { tag, addr: liar }).await;
        assert_eq!(
            closed_by_peer(&r.conn).await,
            Some((CLOSE_AUTH as u64, Bytes::new()))
        );
        // Another frame instead of REGISTER: closed.
        let mut r = raw(&ep, &srv).await.unwrap();
        send(&mut r, &Msg::Ping).await;
        assert!(recv(&mut r).await.is_err());
        // Nothing in time: closed.
        let r = raw(&ep, &srv).await.unwrap();
        assert_eq!(
            closed_by_peer(&r.conn).await,
            Some((CLOSE_AUTH as u64, Bytes::new()))
        );
        // Registered: PEERS (self excluded), a second REGISTER is refused.
        let mut r = raw(&ep, &srv).await.unwrap();
        send(&mut r, &Msg::Register { tag, addr: me(&ep) }).await;
        assert_eq!(recv(&mut r).await, Ok(Msg::Peers(vec![])));
        send(&mut r, &Msg::Register { tag, addr: me(&ep) }).await;
        assert!(recv(&mut r).await.is_err());

        // Fan-out is coalesced: 3 registrations in a burst reach a watcher as few PEERS frames,
        // the last one complete; the watcher sees the server-observed address of each.
        let ep_w = raw_endpoint(lo(21)).await;
        let mut w = raw(&ep_w, &srv).await.unwrap();
        send(&mut w, &Msg::Register { tag, addr: me(&ep_w) }).await;
        assert_eq!(recv(&mut w).await, Ok(Msg::Peers(vec![])));
        let mut others = vec![];
        for n in 22..25 {
            let ep = raw_endpoint(lo(n)).await;
            let mut r = raw(&ep, &srv).await.unwrap();
            send(&mut r, &Msg::Register { tag, addr: me(&ep) }).await;
            others.push((ep, r));
        }
        let start = Instant::now();
        let mut frames = 0;
        let last = loop {
            let Ok(Msg::Peers(p)) = recv(&mut w).await else {
                panic!("watcher lost")
            };
            frames += 1;
            if p.len() == 3 {
                break p;
            }
        };
        assert!(frames <= 2, "{frames} PEERS frames for 3 registrations");
        assert!(start.elapsed() < Duration::from_secs(2));
        for (p, (ep, _)) in last.iter().zip(&others) {
            assert_eq!(p.id, *ep.id().as_bytes());
            assert!(p.addrs.iter().any(|a| a.ip() == ep.bound_sockets()[0].ip()), "{p:?}");
        }
        // Closing unregisters: the watcher (kept alive by PING) is told.
        send(&mut w, &Msg::Ping).await;
        others.pop().unwrap().1.conn.close(0u32.into(), b"");
        assert!(matches!(recv(&mut w).await, Ok(Msg::Peers(p)) if p.len() == 2));
        // A registrant that never sends PING is dropped after `idle`.
        let ep_i = raw_endpoint(lo(26)).await;
        let mut r = raw(&ep_i, &srv).await.unwrap();
        send(&mut r, &Msg::Register { tag, addr: me(&ep_i) }).await;
        let t = Instant::now();
        while recv(&mut r).await.is_ok() {}
        let idle = t.elapsed();
        assert!(
            (Duration::from_millis(1800)..Duration::from_secs(4)).contains(&idle),
            "{idle:?}"
        );

        // Per-source connection cap (4): the fifth from one address is refused.
        let ep = raw_endpoint(lo(30)).await;
        let mut held = vec![];
        for _ in 0..4 {
            let mut r = raw(&ep, &srv).await.unwrap();
            send(&mut r, &Msg::Register { tag, addr: me(&ep) }).await;
            assert!(matches!(recv(&mut r).await, Ok(Msg::Peers(_))));
            held.push(r);
        }
        // Refused before TLS: the dial itself fails.
        assert!(raw(&ep, &srv).await.is_err());
        // Another address still gets in.
        let ep2 = raw_endpoint(lo(31)).await;
        let mut r = raw(&ep2, &srv).await.unwrap();
        send(&mut r, &Msg::Register { tag, addr: me(&ep2) }).await;
        assert!(matches!(recv(&mut r).await, Ok(Msg::Peers(_))));
        // Every refusal above, by reason (the registrants that stopped pinging all went idle).
        let c = server.counts();
        assert!(c.refused.idle >= 1, "{c:?}");
        assert_eq!(
            c.refused,
            Refusals {
                id_mismatch: 1,
                protocol: 2,
                register_timeout: 1,
                idle: c.refused.idle,
                per_ip: 1,
                ..Refusals::default()
            },
            "{c:?}"
        );
        assert!(c.registrations >= 5 && c.sources >= 2, "{c:?}");
    });
}

#[test]
fn stop_rendezvous_unregisters_and_strict_port() {
    let server = RendezvousServer::start(
        lo(1).into(),
        0,
        None,
        RelayChoice::None,
        Limits::default(),
    )
    .unwrap();
    let srv = server.addr();
    let h = host(2);
    let regs = |n: usize| {
        let end = Instant::now() + WAIT;
        while server.counts().registrations != n {
            assert!(Instant::now() < end, "{:?}", server.counts());
            std::thread::sleep(Duration::from_millis(10));
        }
    };
    h.net.register_by_name(srv.clone());
    regs(1);
    h.net.stop_rendezvous();
    regs(0);
    h.net.stop_rendezvous(); // no-op
    // By host name: resolved on the network thread (/etc/hosts, no DNS server involved).
    let port = srv.addrs[0].port();
    h.net.register_by_name(RdvAddr {
        hosts: vec![format!("localhost:{port}")],
        ..srv.clone().into()
    });
    regs(1);
    // The port is taken: a strict server refuses it, a plain one falls back.
    let start = |strict: bool| {
        let f = match strict {
            true => RendezvousServer::start_strict,
            false => RendezvousServer::start,
        };
        f(lo(1).into(), port, None, RelayChoice::None, Limits::default())
    };
    let e = start(true).err().unwrap().to_string();
    assert!(e.starts_with(&format!("cannot bind UDP {}:{port}: ", lo(1))), "{e}");
    let other = start(false).unwrap();
    assert!(other.addr().addrs.iter().all(|a| a.port() != port));
}

/// A registrant flooding frames (PINGs reset the idle timeout) is throttled, then closed once
/// it stays over the per-connection budget; it does not keep the server busy.
#[test]
fn frame_flood_from_a_registrant_is_cut_off() {
    let server =
        RendezvousServer::start(lo(1).into(), 0, None, RelayChoice::None, Limits::default())
            .unwrap();
    let srv = server.addr();
    let tag = auth::session_tag(NAME);
    block_on(async {
        let ep = raw_endpoint(lo(40)).await;
        let mut r = raw(&ep, &srv).await.unwrap();
        send(&mut r, &Msg::Register { tag, addr: me(&ep) }).await;
        assert_eq!(recv(&mut r).await, Ok(Msg::Peers(vec![])));
        let Raw { conn, mut send, .. } = r;
        let mut pings = Vec::new();
        for _ in 0..100 {
            Msg::Ping.encode(&mut pings).unwrap();
        }
        let flood = async {
            while send.write_all(&pings).await.is_ok() {}
        };
        let t = Instant::now();
        let cut = tokio::time::timeout(Duration::from_secs(12), async {
            tokio::select! {
                () = flood => {}
                _ = conn.closed() => {}
            }
        })
        .await;
        assert!(cut.is_ok(), "still flooding after {:?}", t.elapsed());
        assert_eq!(close_code(&conn), Some((CLOSE_AUTH as u64, Bytes::new())));
        assert_eq!(server.counts().refused.flood, 1);
    });
}
