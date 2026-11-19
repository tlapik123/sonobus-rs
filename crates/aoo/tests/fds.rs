//! File descriptors per server connection (own binary: other tests would skew the count).

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use aoo::{Message, server};

fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").unwrap().count()
}

/// One fd per connection on the server side (reader, writer and shutdown share the socket),
/// so 32 embedded connections stay far below macOS/iOS's 256-fd soft limit.
#[test]
fn one_fd_per_server_connection() {
    let srv = server::run("127.0.0.1:0".parse().unwrap()).unwrap();
    let before = open_fds();
    let n = 8;
    let mut conns: Vec<TcpStream> = (0..n)
        .map(|_| TcpStream::connect(srv.local_addr()).unwrap())
        .collect();
    let mut ping = Vec::new();
    let mut raw = Vec::new();
    Message::ServerPing.encode(&mut raw);
    aoo::slip::encode(&raw, &mut ping);
    // a ping answered on every connection: the server has set them all up
    for c in &mut conns {
        c.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
        c.write_all(&ping).unwrap();
        assert!(c.read(&mut [0u8; 64]).unwrap() > 0);
    }
    let end = Instant::now() + Duration::from_secs(2);
    let per_conn = loop {
        // our n client sockets plus the server's
        let server_side = open_fds() - before - n;
        if server_side == n || Instant::now() > end {
            break server_side as f64 / n as f64;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(per_conn, 1.0);
}
