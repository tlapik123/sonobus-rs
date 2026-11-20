//! Shared test helpers: `Net` setup over loopback, event waiting, and a hostile raw iroh
//! client that speaks the control-stream framing by hand. Relays are disabled everywhere and
//! every socket binds a 127.x address.
#![allow(dead_code)]

pub mod e2e;

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;
use iroh::endpoint::{Connection, ConnectionError, RecvStream, SendStream, presets};
use iroh::{Endpoint, EndpointAddr, EndpointId, RelayMode, TransportAddr};
use sonobus_net::auth::{self, Key, Side};
use sonobus_net::wire::{self, Frame, Hello, Method, Role};
use sonobus_net::{Config, ConnId, Event, Net, PeerAddr, PeerInfo, PeerTx, RelayChoice, Sink};

pub const SESSION: &str = "test session";
pub const WAIT: Duration = Duration::from_secs(15);

pub fn lo(n: u8) -> Ipv4Addr {
    Ipv4Addr::new(127, 0, 0, n)
}

pub fn cfg(name: &str, key: Option<Key>, password: Option<&str>) -> Config {
    Config {
        bind_ip: Ipv4Addr::LOCALHOST.into(),
        port: 0,
        session: SESSION.into(),
        key,
        password: password.map(Into::into),
        name: name.into(),
        role: Role::Peer,
        relay: RelayChoice::None,
        ..Config::default()
    }
}

/// Net whose sinks count datagrams into `count`.
pub fn net(c: Config, count: Arc<AtomicUsize>) -> Net {
    Net::start(c, move |_, _| {
        let count = count.clone();
        Box::new(move |_| {
            count.fetch_add(1, Ordering::Relaxed);
        }) as Sink
    })
    .unwrap()
}

pub fn simple(c: Config) -> Net {
    net(c, Arc::new(AtomicUsize::new(0)))
}

pub fn kind(e: &Event) -> String {
    match e {
        Event::Connected { conn, .. } => format!("Connected({conn})"),
        Event::ConnectFailed { reason, .. } => format!("ConnectFailed({reason})"),
        Event::IncomingFailed { count, last_reason } => {
            format!("IncomingFailed({count}, {last_reason})")
        }
        Event::Control { .. } => "Control".into(),
        Event::Closed { conn, reason } => format!("Closed({conn}, {reason})"),
        Event::SessionKey(_) => "SessionKey".into(),
        Event::JoinedBroadcast => "JoinedBroadcast".into(),
        Event::JoinFailed { .. } => "JoinFailed".into(),
        Event::PinInvalidated => "PinInvalidated".into(),
        Event::Throttled { .. } => "Throttled".into(),
        Event::PathChanged { conn, path } => format!("PathChanged({conn}, {path:?})"),
        Event::RendezvousFailed { reason, .. } => format!("RendezvousFailed({reason})"),
        Event::LocalRelay(r) => format!("LocalRelay({r})"),
    }
}

/// Next event that `f` accepts; other events are skipped.
pub fn wait<T>(net: &Net, mut f: impl FnMut(Event) -> Option<T>) -> T {
    let end = Instant::now() + WAIT;
    loop {
        let left = end.saturating_duration_since(Instant::now());
        let e = net
            .events()
            .recv_timeout(left)
            .expect("timed out waiting for event");
        if let Some(t) = f(e) {
            return t;
        }
    }
}

/// Async variant (polls) for tests that also drive a raw client on the same thread.
pub async fn wait_async<T>(net: &Net, mut f: impl FnMut(Event) -> Option<T>) -> T {
    let end = Instant::now() + WAIT;
    loop {
        while let Ok(e) = net.events().try_recv() {
            if let Some(t) = f(e) {
                return t;
            }
        }
        assert!(Instant::now() < end, "timed out waiting for event");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

pub fn connected(net: &Net) -> (ConnId, PeerTx, PeerInfo) {
    wait(net, |e| match e {
        Event::Connected { conn, tx, peer, .. } => Some((conn, tx, peer)),
        Event::ConnectFailed { reason, .. } => panic!("connect failed: {reason}"),
        _ => None,
    })
}

pub fn failed(net: &Net) -> (String, Option<Duration>) {
    wait(net, |e| match e {
        Event::ConnectFailed {
            reason,
            retry_after,
            ..
        } => Some((reason, retry_after)),
        Event::Connected { .. } => panic!("unexpectedly connected"),
        _ => None,
    })
}

pub fn incoming_failed(net: &Net) -> String {
    wait(net, |e| match e {
        Event::IncomingFailed { last_reason, .. } => Some(last_reason),
        Event::Connected { .. } => panic!("unexpectedly connected"),
        _ => None,
    })
}

pub fn drain(net: &Net, d: Duration) -> Vec<String> {
    let end = Instant::now() + d;
    let mut v = Vec::new();
    while let Ok(e) = net
        .events()
        .recv_timeout(end.saturating_duration_since(Instant::now()))
    {
        v.push(kind(&e));
    }
    v
}

pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(f)
}

// ---------------------------------------------------------------------------------------------
// Raw client

pub async fn raw_endpoint(ip: Ipv4Addr) -> Endpoint {
    Endpoint::builder(presets::Minimal)
        .relay_mode(RelayMode::Disabled)
        .clear_ip_transports()
        .bind_addr(SocketAddr::from((ip, 0)))
        .unwrap()
        .alpns(vec![sonobus_net::rt::ALPN.to_vec()])
        .bind()
        .await
        .unwrap()
}

pub async fn dial(ep: &Endpoint, to: &PeerAddr) -> Result<Connection, String> {
    let id = EndpointId::from_bytes(&to.id).unwrap();
    let addr = EndpointAddr::from_parts(id, to.addrs.iter().copied().map(TransportAddr::Ip));
    ep.connect(addr, sonobus_net::rt::ALPN)
        .await
        .map_err(|e| e.to_string())
}

pub async fn wf(s: &mut SendStream, f: &Frame) -> Result<(), String> {
    let mut b = Vec::new();
    f.encode(&mut b).unwrap();
    s.write_all(&b).await.map_err(|e| e.to_string())
}

pub async fn rf(r: &mut RecvStream) -> Result<Frame, String> {
    let mut h = [0u8; 3];
    r.read_exact(&mut h).await.map_err(|e| e.to_string())?;
    let mut body = vec![0; u16::from_le_bytes([h[0], h[1]]) as usize];
    r.read_exact(&mut body).await.map_err(|e| e.to_string())?;
    Frame::decode_body(h[2], &body).map_err(|e| e.to_string())
}

pub fn hello(method: Method, tag: [u8; 16], proof: Vec<u8>) -> Frame {
    Frame::Hello(Hello {
        method,
        tag,
        role: Role::Host,
        name: "evil".into(),
        proof,
        addrs: vec![],
        relay: None,
    })
}

pub fn exporter(c: &Connection, tag: &[u8; 16]) -> [u8; 32] {
    let mut e = [0; 32];
    c.export_keying_material(&mut e, auth::EXPORTER_LABEL, tag)
        .unwrap();
    e
}

/// The peer's application close code and reason, once closed.
pub fn close_code(c: &Connection) -> Option<(u64, Bytes)> {
    match c.close_reason()? {
        ConnectionError::ApplicationClosed(a) => Some((a.error_code.into_inner(), a.reason)),
        _ => None,
    }
}

/// Wait until the peer closes `c`; its application close code and reason.
pub async fn closed_by_peer(c: &Connection) -> Option<(u64, Bytes)> {
    let _ = tokio::time::timeout(WAIT, c.closed()).await;
    close_code(c)
}

pub struct RawConn {
    pub conn: Connection,
    pub send: SendStream,
    pub recv: RecvStream,
    pub welcome: wire::Welcome,
}

/// Dial `to` and complete a KEY join with `key` (session [`SESSION`]).
pub async fn key_join(ep: &Endpoint, to: &PeerAddr, key: &Key) -> Result<RawConn, String> {
    let conn = dial(ep, to).await?;
    let (mut send, mut recv) = conn.open_bi().await.map_err(|e| e.to_string())?;
    let tag = auth::session_tag(SESSION);
    let e = exporter(&conn, &tag);
    let me = *ep.id().as_bytes();
    let proof = auth::key_proof(key, Side::Connector, &e, &me, &to.id).to_vec();
    wf(&mut send, &hello(Method::Key, tag, proof)).await?;
    match rf(&mut recv).await? {
        Frame::Welcome(w) => {
            auth::key_verify(key, Side::Acceptor, &e, &to.id, &me, &w.mac)
                .map_err(|e| e.to_string())?;
            Ok(RawConn {
                conn,
                send,
                recv,
                welcome: w,
            })
        }
        f => Err(format!("unexpected {f:?}")),
    }
}

/// Outcome of a raw PAKE attempt.
#[derive(Debug, PartialEq, Eq)]
pub enum Pake {
    /// Got PAKE2 (Y of this length) and stopped there (connection dropped).
    Abandoned(usize),
    /// Sent PAKE3; the peer's close code/reason (None: it answered WELCOME).
    Completed(Option<(u64, Bytes)>),
    /// Closed before PAKE2 (code, reason).
    Refused(Option<(u64, Bytes)>),
}

/// A raw PAKE attempt with `pw` for session `session`, stopping after PAKE2 unless `complete`.
pub async fn raw_pake(
    ep: &Endpoint,
    to: &PeerAddr,
    session: &str,
    pw: &str,
    complete: bool,
) -> Pake {
    let conn = dial(ep, to).await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    let tag = auth::session_tag(session);
    let e = exporter(&conn, &tag);
    let me = *ep.id().as_bytes();
    let (c, x) = auth::PakeConnector::start(pw, &e, &me, &to.id);
    wf(&mut send, &hello(Method::Pake, tag, x)).await.unwrap();
    let y = match rf(&mut recv).await {
        Ok(Frame::Pake2(p)) => p.y,
        Ok(f) => panic!("unexpected {f:?}"),
        Err(_) => return Pake::Refused(closed_by_peer(&conn).await),
    };
    if !complete {
        conn.close(0u32.into(), b"");
        return Pake::Abandoned(y.len());
    }
    let conf = match c.finish(&y) {
        Ok((_, conf)) => conf,
        Err(_) => [0; 32],
    };
    wf(&mut send, &Frame::Pake3 { conf }).await.unwrap();
    match rf(&mut recv).await {
        Ok(Frame::Welcome(_)) => Pake::Completed(None),
        _ => Pake::Completed(closed_by_peer(&conn).await),
    }
}

/// An AUDIO datagram.
pub fn audio() -> Bytes {
    let mut b = Vec::new();
    wire::encode_audio(&wire::AudioHeader::default(), &[], b"x", &mut b).unwrap();
    Bytes::from(b)
}
