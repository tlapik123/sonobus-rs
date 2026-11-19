//! Client <-> server over localhost with the real std drivers.
//!
//! The `#[ignore]`d tests run against the upstream C++ (SonoBus 1.7.2 `deps/aoo`). Build it once:
//!
//! ```sh
//! cd .locki/tmp/aoo-ref   # holds server_main.cpp and client_main.cpp
//! R=../ref/up-main/deps/aoo
//! g++ -std=c++17 -O1 -w -I$R/lib -I$R/lib/src -I$R/deps -I$R/deps/oscpack -c \
//!     $R/lib/src/{server,client,net_utils,time,sync}.cpp \
//!     $R/deps/oscpack/osc/{OscOutboundPacketStream,OscReceivedElements,OscTypes}.cpp
//! gcc -O1 -w -c $R/deps/md5/md5.c
//! g++ -std=c++17 -w -I$R/lib -o aoo_server server_main.cpp *.o -lpthread
//! g++ -std=c++17 -w -I$R/lib -o aoo_client client_main.cpp *.o -lpthread
//! ```
//!
//! Then run (each test starts its own C++ process on a free port; the binaries default to
//! `.locki/tmp/aoo-ref/`, override with `AOO_CPP_SERVER` / `AOO_CPP_CLIENT`):
//!
//! ```sh
//! mise exec -- cargo test -p aoo --test net -- --ignored --test-threads=1
//! ```

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use aoo::client::{Client, Event, State, TcpDriver};
use aoo::server::{Limits, Rate};
use aoo::{Message, message, server};

struct Node {
    client: Client,
    udp: UdpSocket,
    drv: TcpDriver,
    events: Vec<Event>,
    /// Argument-less `/aoo/peer/ping` received (keepalives).
    keepalives: usize,
}

impl Node {
    fn new(server: SocketAddr, user: &str, seed: u64) -> Self {
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        udp.set_nonblocking(true).unwrap();
        let mut client = Client::new(udp.local_addr().unwrap().port(), seed);
        client.connect(server, user, "");
        Self {
            client,
            udp,
            drv: TcpDriver::default(),
            events: Vec::new(),
            keepalives: 0,
        }
    }

    fn addr(&self) -> SocketAddr {
        self.udp.local_addr().unwrap()
    }

    fn step(&mut self) {
        let now = Instant::now();
        let mut buf = [0u8; 4096];
        while let Ok((n, from)) = self.udp.recv_from(&mut buf) {
            if let Ok(m) = message::parse(&buf[..n]) {
                if m == (Message::PeerPing { token: None }) {
                    self.keepalives += 1;
                }
                self.client.handle_udp(from, &m, now);
            }
        }
        let events = &mut self.events;
        self.drv
            .pump(&mut self.client, &self.udp, now, |e| events.push(e));
    }

    fn has(&self, f: impl Fn(&Event) -> bool) -> bool {
        self.events.iter().any(f)
    }
}

/// Step all nodes until `done` holds; panics after `secs`.
fn wait(nodes: &mut [&mut Node], secs: u64, what: &str, done: impl Fn(&[&mut Node]) -> bool) {
    let end = Instant::now() + Duration::from_secs(secs);
    while !done(nodes) {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        for n in nodes.iter_mut() {
            n.step();
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// Private group: join, wrong password, hole punching, keepalive, leave on disconnect.
fn private_group_scenario(srv: SocketAddr, tag: &str) {
    let (an, bn, cn) = (
        format!("alice{tag}"),
        format!("bob{tag}"),
        format!("carol{tag}"),
    );
    let mut a = Node::new(srv, &an, 1 << 20);
    let mut b = Node::new(srv, &bn, 2 << 20);
    let mut c = Node::new(srv, &cn, 3 << 20);
    wait(&mut [&mut a, &mut b, &mut c], 5, "login", |n| {
        n.iter().all(|n| n.client.state() == State::Connected)
    });
    let g = format!("g{tag}");
    a.client.join_group(&g, "pw", false);
    wait(&mut [&mut a], 5, "a joins", |n| {
        n[0].has(|e| matches!(e, Event::GroupJoined { .. }))
    });
    b.client.join_group(&g, "pw", false);
    c.client.join_group(&g, "nope", false);
    let (aa, ba) = (a.addr(), b.addr());
    wait(&mut [&mut a, &mut b, &mut c], 5, "peers", |n| {
        let has_peer = |n: &Node, u: &str, addr| {
            n.has(|e| matches!(e, Event::PeerJoined { user, addr: x, group, .. } if user == u && *x == addr && *group == g))
        };
        has_peer(n[0], &bn, ba)
            && has_peer(n[1], &an, aa)
            && n[2].has(|e| {
                *e == Event::GroupJoinFailed {
                    group: g.clone(),
                    error: "wrong password".into(),
                }
            })
    });
    // the immediate argument-less ping after the handshake reached both sides
    wait(&mut [&mut a, &mut b], 2, "keepalive pings", |n| {
        n[0].keepalives > 0 && n[1].keepalives > 0
    });
    assert!(!a.has(|e| matches!(e, Event::PeerJoinFailed { .. })));
    // tokens travel through the server
    let bt = b.client.token();
    assert!(a.has(|e| matches!(e, Event::PeerJoined { token, .. } if *token == bt)));

    b.client.disconnect();
    wait(&mut [&mut a, &mut b], 5, "peer left", |n| {
        n[0].has(|e| matches!(e, Event::PeerLeft { user, .. } if *user == bn))
    });
    assert!(b.has(|e| *e == Event::Disconnected { error: None }));
}

/// Public group watch: add with count, count changes, del when empty.
fn public_group_scenario(srv: SocketAddr, tag: &str) {
    let mut w = Node::new(srv, &format!("watcher{tag}"), 4 << 20);
    let mut p = Node::new(srv, &format!("pub{tag}"), 5 << 20);
    let mut q = Node::new(srv, &format!("pub2{tag}"), 6 << 20);
    wait(&mut [&mut w, &mut p, &mut q], 5, "login", |n| {
        n.iter().all(|n| n.client.state() == State::Connected)
    });
    let g = format!("public{tag}");
    w.client.watch_public_groups(true);
    p.client.join_group(&g, "", true);
    let added = |n: &Node, k| {
        n.has(|e| {
            *e == Event::PublicGroupAdded {
                group: g.clone(),
                users: k,
            }
        })
    };
    wait(&mut [&mut w, &mut p], 5, "public add 1", |n| added(n[0], 1));
    q.client.join_group(&g, "", true);
    wait(&mut [&mut w, &mut q], 5, "public add 2", |n| added(n[0], 2));
    // one leaves, the other's TCP connection drops
    q.client.leave_group(&g);
    wait(&mut [&mut w, &mut q], 5, "public count 1", |n| {
        n[1].has(|e| matches!(e, Event::GroupLeft { .. }))
            && n[0]
                .events
                .iter()
                .filter(|e| {
                    **e == Event::PublicGroupAdded {
                        group: g.clone(),
                        users: 1,
                    }
                })
                .count()
                == 2
    });
    drop(p); // closes its TCP connection without a goodbye
    // (the C++ server sends a count of 0 first; ours coalesces both into the del)
    wait(&mut [&mut w], 5, "public del", |n| {
        n[0].has(|e| *e == Event::PublicGroupRemoved { group: g.clone() })
    });
}

#[test]
fn rust_server_private_group() {
    let srv = server::run("127.0.0.1:0".parse().unwrap()).unwrap();
    private_group_scenario(srv.local_addr(), "");
    // all nodes are dropped now: users and groups are purged
    let end = Instant::now() + Duration::from_secs(2);
    while srv.counts() != (0, 0) {
        assert!(Instant::now() < end, "left over: {:?}", srv.counts());
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn rust_server_public_groups() {
    let srv = server::run("127.0.0.1:0".parse().unwrap()).unwrap();
    public_group_scenario(srv.local_addr(), "");
}

#[test]
fn connect_failures() {
    let srv = server::run("127.0.0.1:0".parse().unwrap()).unwrap();
    let mut a = Node::new(srv.local_addr(), "same", 1);
    wait(&mut [&mut a], 5, "login", |n| {
        n[0].client.state() == State::Connected
    });
    let mut b = Node::new(srv.local_addr(), "same", 2);
    wait(&mut [&mut a, &mut b], 5, "access denied", |n| {
        n[1].has(|e| {
            *e == Event::ConnectFailed {
                reason: "access denied".into(),
            }
        })
    });
    // nothing listens on the port any more
    let addr = srv.local_addr();
    drop(srv);
    wait(&mut [&mut a], 5, "server gone", |n| {
        n[0].has(|e| matches!(e, Event::Disconnected { error: Some(_) }))
    });
    let mut c = Node::new(addr, "x", 3);
    wait(&mut [&mut c], 6, "refused", |n| {
        n[0].has(|e| matches!(e, Event::ConnectFailed { .. }))
    });
}

#[test]
fn server_drops_oversize_frames() {
    let srv = server::run("127.0.0.1:0".parse().unwrap()).unwrap();
    let mut s = std::net::TcpStream::connect(srv.local_addr()).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    // 1 MiB of garbage without a frame end, then a ping: only the ping is answered
    s.write_all(&vec![0x41; 1 << 20]).unwrap();
    s.write_all(&[aoo::slip::END]).unwrap();
    let mut ping = Vec::new();
    aoo::slip::encode(b"/aoo/server/ping\0\0\0\0,\0\0\0", &mut ping);
    s.write_all(&ping).unwrap();
    let mut buf = [0u8; 64];
    let n = s.read(&mut buf).unwrap();
    let mut frames = Vec::new();
    aoo::slip::Decoder::new(64).feed(&buf[..n], |f| frames.push(f.to_vec()));
    assert_eq!(message::parse(&frames[0]).unwrap(), Message::ClientPing);
}

fn frame(m: Message) -> Vec<u8> {
    let (mut raw, mut out) = (Vec::new(), Vec::new());
    m.encode(&mut raw);
    aoo::slip::encode(&raw, &mut out);
    out
}

fn login_frame(user: &str) -> Vec<u8> {
    frame(Message::ServerLogin {
        user,
        password: &aoo::password_hash(""),
        public_ip: "127.0.0.1",
        public_port: 1,
        local_ip: "127.0.0.1",
        local_port: 1,
        token: Some(1),
    })
}

/// Reads whatever is pending; true once the server has closed the connection.
fn is_closed(s: &mut TcpStream) -> bool {
    s.set_read_timeout(Some(Duration::from_millis(300)))
        .unwrap();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => return true,
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return false;
            }
            Err(_) => return true, // reset
        }
    }
}

/// Decoded frames from `s`, read on a thread until it closes.
fn frames_of(s: &TcpStream) -> std::sync::mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let mut s = s.try_clone().unwrap();
    std::thread::spawn(move || {
        let mut dec = aoo::slip::Decoder::new(aoo::MAX_PACKET_SIZE);
        let mut buf = [0u8; 65536];
        while let Ok(n @ 1..) = s.read(&mut buf) {
            dec.feed(&buf[..n], |f| drop(tx.send(f.to_vec())));
        }
    });
    rx
}

/// Logs in on a new connection; the connection if the server accepted the login.
fn try_login(addr: SocketAddr, user: &str) -> Option<TcpStream> {
    let mut s = TcpStream::connect(addr).ok()?;
    s.write_all(&login_frame(user)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    // the reply is all the server sends before our next request
    let mut dec = aoo::slip::Decoder::new(aoo::MAX_PACKET_SIZE);
    let (mut buf, mut reply) = ([0u8; 256], None);
    while reply.is_none() {
        let n = s.read(&mut buf).ok().filter(|&n| n > 0)?;
        dec.feed(&buf[..n], |f| reply = Some(f.to_vec()));
    }
    s.set_read_timeout(None).ok()?;
    let ok = matches!(
        message::parse(&reply?),
        Ok(Message::ClientLogin { result: 1, .. })
    );
    ok.then_some(s)
}

/// Public group watchers that never read, while another connection churns a public group,
/// must not stall the server (it once wrote to clients under its global lock): the UDP side
/// keeps answering at once, and the watchers are dropped when their send queues fill, with
/// full cleanup: peer/leave to their groups, their groups purged, their slots free again.
#[test]
fn non_reading_watchers_do_not_stall_others() {
    let limits = Limits {
        max_connections_per_ip: 6,
        // churn as fast as the connection allows
        join_rate: Rate::UNLIMITED,
        join_rate_per_ip: Rate::UNLIMITED,
        list_rate_per_ip: Rate::UNLIMITED,
        public_update_interval: Duration::ZERO,
        ..Limits::embedded()
    };
    let srv = server::run_with("127.0.0.1:0".parse().unwrap(), limits).unwrap();
    let addr = srv.local_addr();
    let join = |g: &str, public| {
        frame(Message::ServerGroupJoin {
            group: g,
            password: "",
            is_public: Some(public),
        })
    };
    let observer = try_login(addr, "observer").unwrap();
    let seen = frames_of(&observer);
    (&observer).write_all(&join("p", false)).unwrap();
    let watchers: Vec<TcpStream> = (0..3)
        .map(|i| {
            let mut w = TcpStream::connect(addr).unwrap();
            w.write_all(&login_frame(&format!("w{i}"))).unwrap();
            w.write_all(&join(&format!("own{i}"), false)).unwrap();
            w.write_all(&join("p", false)).unwrap();
            w.write_all(&frame(Message::ServerGroupPublic { watch: true }))
                .unwrap();
            w
        })
        .collect();
    let churner = try_login(addr, "churner").unwrap();
    let _replies = frames_of(&churner);
    let g = "x".repeat(120);
    let mut round = join(&g, true);
    round.extend(frame(Message::ServerGroupLeave { group: &g }));
    // small bursts (~5 KB of replies), well within the churner's send queue
    let rounds = round.repeat(16);
    let mut c = churner.try_clone().unwrap();
    std::thread::spawn(move || while c.write_all(&rounds).is_ok() {});

    let u = UdpSocket::bind("127.0.0.1:0").unwrap();
    u.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    let mut req = Vec::new();
    Message::ServerRequest.encode(&mut req);
    let mut buf = [0u8; 256];
    let (mut worst, mut probes) = (Duration::ZERO, 0);
    let mut left = std::collections::BTreeSet::new();
    let end = Instant::now() + Duration::from_secs(20);
    while left.len() < 3 || probes < 200 {
        assert!(Instant::now() < end, "watchers not dropped: {left:?} {:?} {probes}", srv.counts());
        let t = Instant::now();
        u.send_to(&req, addr).unwrap();
        let n = u.recv(&mut buf).expect("UDP reply lost");
        assert!(matches!(
            message::parse(&buf[..n]),
            Ok(Message::ClientReply { .. })
        ));
        worst = worst.max(t.elapsed());
        probes += 1;
        while let Ok(f) = seen.try_recv() {
            if let Ok(Message::ClientPeerLeave { group: "p", user }) = message::parse(&f) {
                left.insert(user.to_owned());
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(worst < Duration::from_millis(500), "UDP reply took {worst:?}");
    let _ = churner.shutdown(std::net::Shutdown::Both); // unless the server dropped it too
    // left: the observer in "p"; the watchers' own groups and the churned one are purged
    let end = Instant::now() + Duration::from_secs(5);
    while srv.counts() != (1, 1) {
        assert!(Instant::now() < end, "left over: {:?}", srv.counts());
        std::thread::sleep(Duration::from_millis(10));
    }
    // the four slots are free again (the server may still be closing the churner)
    let end = Instant::now() + Duration::from_secs(5);
    let mut fresh = Vec::new();
    while fresh.len() < 5 {
        assert!(Instant::now() < end, "slots not freed: {} of 5", fresh.len());
        fresh.extend(try_login(addr, &format!("new{}", fresh.len())));
    }
    let st = srv.stats();
    assert!(st.refused.send_queue_full >= 3, "{st:?}");
    drop(watchers);
}

#[test]
fn driver_connection_limits() {
    let limits = Limits {
        max_connections_per_ip: 2,
        ..Limits::default()
    };
    let srv = server::run_with("127.0.0.1:0".parse().unwrap(), limits).unwrap();
    let mut a = TcpStream::connect(srv.local_addr()).unwrap();
    let mut b = TcpStream::connect(srv.local_addr()).unwrap();
    let mut c = TcpStream::connect(srv.local_addr()).unwrap();
    assert!(is_closed(&mut c), "third connection from one IP");
    assert!(!is_closed(&mut a) && !is_closed(&mut b));
    drop(a);
    // the slot is free again once the server noticed
    let end = Instant::now() + Duration::from_secs(2);
    loop {
        let mut d = TcpStream::connect(srv.local_addr()).unwrap();
        if !is_closed(&mut d) {
            break;
        }
        assert!(Instant::now() < end, "slot not freed");
    }
    let st = srv.stats();
    assert!(st.refused.per_ip_cap >= 1 && st.refused.global_cap == 0, "{st:?}");

    let limits = Limits {
        max_connections: 2,
        ..Limits::default()
    };
    let srv = server::run_with("127.0.0.1:0".parse().unwrap(), limits).unwrap();
    let mut a = TcpStream::connect(srv.local_addr()).unwrap();
    let mut b = TcpStream::connect(srv.local_addr()).unwrap();
    let mut c = TcpStream::connect(srv.local_addr()).unwrap();
    assert!(is_closed(&mut c), "over MAX_CONNECTIONS");
    assert!(!is_closed(&mut a) && !is_closed(&mut b));
    let st = srv.stats();
    assert_eq!((st.connections, st.refused.global_cap), (2, 1), "{st:?}");
}

#[test]
fn driver_login_deadline_and_idle_timeout() {
    let limits = Limits {
        login_timeout: Duration::from_millis(500),
        idle_timeout: Duration::from_millis(800),
        ..Limits::default()
    };
    let srv = server::run_with("127.0.0.1:0".parse().unwrap(), limits).unwrap();
    let conn = || TcpStream::connect(srv.local_addr()).unwrap();
    // never logs in, but keeps sending bytes
    let mut never = conn();
    // logs in, then only bytes that never make a frame
    let mut dribble = conn();
    dribble.write_all(&login_frame("dribble")).unwrap();
    // logs in and pings like a stock client (just faster)
    let mut good = conn();
    good.write_all(&login_frame("good")).unwrap();
    let ping = frame(Message::ServerPing);
    let t0 = Instant::now();
    while t0.elapsed() < Duration::from_millis(2000) {
        let _ = never.write_all(b"A");
        let _ = dribble.write_all(b"A");
        good.write_all(&ping).unwrap();
        assert!(!is_closed(&mut good));
    }
    assert!(is_closed(&mut never), "login deadline");
    assert!(is_closed(&mut dribble), "idle timeout");
    assert!(!is_closed(&mut good));
    assert_eq!(srv.counts(), (1, 0));
    let st = srv.stats();
    assert_eq!((st.connections, st.users, st.groups), (1, 1, 0));
    assert_eq!(
        st.refused,
        server::Refusals {
            login_timeout: 1,
            idle_timeout: 1,
            ..Default::default()
        }
    );
}

/// Path of a C++ helper binary; panics with a hint if it was not built.
fn cpp_exe(var: &str, name: &str) -> PathBuf {
    let p = std::env::var_os(var).map_or_else(
        || {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../.locki/tmp/aoo-ref")
                .join(name)
        },
        PathBuf::from,
    );
    assert!(
        p.is_file(),
        "C++ {name} not found at {} (set {var}, build steps at the top of tests/net.rs)",
        p.display()
    );
    p
}

/// A port free for both UDP and TCP on localhost.
fn free_port() -> u16 {
    loop {
        let u = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = u.local_addr().unwrap().port();
        if TcpListener::bind(("127.0.0.1", port)).is_ok() {
            return port;
        }
    }
}

/// Killed on drop, so a failing test does not leave it running.
struct Proc(std::process::Child);

impl Drop for Proc {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The C++ server on a free port.
fn cpp_server() -> (Proc, SocketAddr) {
    let exe = cpp_exe("AOO_CPP_SERVER", "aoo_server");
    let addr: SocketAddr = ([127, 0, 0, 1], free_port()).into();
    let child = std::process::Command::new(exe)
        .arg(addr.port().to_string())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let p = Proc(child);
    let end = Instant::now() + Duration::from_secs(5);
    while TcpStream::connect(addr).is_err() {
        assert!(Instant::now() < end, "C++ server did not start on {addr}");
        std::thread::sleep(Duration::from_millis(20));
    }
    (p, addr)
}

#[test]
#[ignore = "needs the C++ AOO server, see top of file"]
fn cpp_server_private_group() {
    let (_p, addr) = cpp_server();
    private_group_scenario(addr, "");
}

#[test]
#[ignore = "needs the C++ AOO server, see top of file"]
fn cpp_server_public_groups() {
    let (_p, addr) = cpp_server();
    public_group_scenario(addr, "");
}

#[test]
#[ignore = "needs the C++ AOO client, see top of file"]
fn cpp_client_against_rust_server() {
    let exe = cpp_exe("AOO_CPP_CLIENT", "aoo_client");
    let srv = server::run("127.0.0.1:0".parse().unwrap()).unwrap();
    let port = srv.local_addr().port().to_string();
    let child = std::process::Command::new(exe)
        .args(["127.0.0.1", &port, "cpp", "g", "pw", "3"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut r = Node::new(srv.local_addr(), "rust", 9);
    wait(&mut [&mut r], 5, "login", |n| {
        n[0].client.state() == State::Connected
    });
    r.client.join_group("g", "pw", false);
    wait(&mut [&mut r], 5, "cpp peer", |n| {
        n[0].has(|e| matches!(e, Event::PeerJoined { user, .. } if user == "cpp"))
    });
    // the C++ client disconnects after 3 s
    wait(&mut [&mut r], 6, "cpp left", |n| {
        n[0].has(|e| matches!(e, Event::PeerLeft { user, .. } if user == "cpp"))
    });
    let out = child.wait_with_output().unwrap();
    let out = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = out.lines().map(str::trim_end).collect();
    let want = format!("PEER_JOIN g rust {}", r.addr());
    for l in ["CONNECT 1", "GROUP_JOIN g 1", &want, "DISCONNECT 1"] {
        assert!(lines.contains(&l), "C++ client output lacks {l:?}:\n{out}");
    }
}
