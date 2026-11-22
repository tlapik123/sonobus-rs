//! A scripted stand-in for a stock SonoBus 1.7.2 peer, built from `aoo` and `sbmsg`
//! primitives, following the stock handshake (P.cpp:3455-3741, 4407-4477): on its server
//! announcement it makes one sink and one source with its own id, invites our dummy source 0
//! once (no repeats), sends `/sb/blockedinfo F`, and sets no user format. An invite to its
//! dummy from a peer it does not know is ignored (it is "in a group", P.cpp:3511-3515); from
//! a known peer it adds the inviting sink to its source and invites our source back, once.
//!
//! The engine under test is a real [`Engine`] whose server is simulated: the tests inject the
//! server's peer announcements into its session, so every ordering is deterministic.
//!
//! A stock latency test against us (P.cpp:5762-5794): its latency sink (`ourId + 20000`)
//! invites our echo source (our id + 40000), its latency source sends to our echo sink.

use std::collections::VecDeque;
use std::net::{SocketAddr, UdpSocket};
use std::time::{Duration, Instant};

use aoo::codec::{Codec, Format, PcmBitDepth};
use aoo::sink::{Sink, SinkAudio, SinkEventKind, SinkOptions};
use aoo::source::{Source, SourceAudio, SourceEventKind, SourceOptions};
use aoo::{Message, Setup, TimeTag, message};
use sonobus_core::sbmsg::{self, LatEntry, PeerInfo, SbMessage};
use sonobus_core::state::{AppState, ChannelGroup};

use super::ConnState;
use crate::latency::LatencyMeasurer;
use crate::{AudioConfig, Engine, EngineAudio, EngineConfig};
use aoo::client::Event as ClientEvent;

const SR: u32 = 48000;
const BLOCK: usize = 256;
const STOCK_FREQ: f64 = 880.0;
const OUR_FREQ: f64 = 440.0;
const AMP: f32 = 0.5;

fn setup() -> Setup {
    Setup {
        samplerate: SR as i32,
        blocksize: BLOCK as i32,
        nchannels: 1,
    }
}

/// Amplitude of `freq` in `x` (Hann-windowed DFT bin).
fn tone(x: &VecDeque<f32>, freq: f64) -> f32 {
    let n = x.len().max(1);
    let (mut re, mut im, mut wsum) = (0.0f64, 0.0f64, 0.0f64);
    for (i, &v) in x.iter().enumerate() {
        let w = 0.5 - 0.5 * (std::f64::consts::TAU * i as f64 / n as f64).cos();
        let a = std::f64::consts::TAU * freq * i as f64 / f64::from(SR);
        re += w * f64::from(v) * a.cos();
        im -= w * f64::from(v) * a.sin();
        wsum += w;
    }
    (2.0 * (re * re + im * im).sqrt() / wsum.max(1.0)) as f32
}

fn keep(ring: &mut VecDeque<f32>, x: &[f32]) {
    for &v in x {
        if ring.len() == SR as usize / 2 {
            ring.pop_front();
        }
        ring.push_back(v);
    }
}

fn send_msg(sock: &UdpSocket, to: SocketAddr, m: &Message) {
    let mut b = Vec::new();
    m.encode(&mut b);
    let _ = sock.send_to(&b, to);
}

/// The stock side's latency-test pair and measurer.
struct StockLat {
    sink: Sink,
    sink_audio: SinkAudio,
    source: Source,
    source_audio: SourceAudio,
    measurer: LatencyMeasurer,
}

/// Its latency pair's id (`ourId + LATENCY_ID_OFFSET`).
const STOCK_LAT_ID: i32 = 1 + 20000;

/// The stock side's objects for its one remote peer (us).
struct StockLink {
    /// Our id for them (their `remoteSourceId`), learnt from our invite of their dummy.
    remote_id: Option<i32>,
    our_id: i32,
    source: Source,
    source_audio: SourceAudio,
    sink: Sink,
    sink_audio: SinkAudio,
}

struct StockPeer {
    sock: UdpSocket,
    dummy: Source,
    link: Option<StockLink>,
    /// The engine's address (as the server would announce it).
    engine: SocketAddr,
    phase: f64,
    heard: VecDeque<f32>,
    /// Dummy invites dropped because we were not known yet.
    pub ignored_invites: usize,
    lat: Option<StockLat>,
    /// The last `/sb/pinfo` the engine sent and the last stream format its sink saw.
    pinfo: Option<PeerInfo>,
    format: Option<Format>,
}

impl StockPeer {
    fn new(engine: SocketAddr) -> Self {
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_nonblocking(true).unwrap();
        Self {
            sock,
            dummy: Source::new(0, setup(), SourceOptions::default()).0,
            link: None,
            engine,
            phase: 0.0,
            heard: VecDeque::new(),
            ignored_invites: 0,
            lat: None,
            pinfo: None,
            format: None,
        }
    }

    /// `startRemotePeerLatencyTest` as stock does it.
    fn start_latency_test(&mut self) {
        let remote = self
            .link
            .as_ref()
            .and_then(|l| l.remote_id)
            .expect("connected");
        let echo = remote + 40000;
        let opts = SinkOptions {
            buffersize_ms: 60,
            dynamic_resampling: false,
            ..SinkOptions::default()
        };
        let (mut sink, sink_audio) = Sink::new(STOCK_LAT_ID, setup(), opts);
        sink.invite_source(self.engine, echo);
        let sopts = SourceOptions {
            dynamic_resampling: false,
            ..SourceOptions::default()
        };
        let (mut source, source_audio) = Source::new(STOCK_LAT_ID, setup(), sopts);
        source
            .set_format(Format {
                nchannels: 1,
                samplerate: SR as i32,
                blocksize: BLOCK as i32,
                codec: Codec::Pcm(PcmBitDepth::Int16),
            })
            .unwrap();
        source.add_sink(self.engine, echo, 0);
        source.start();
        let mut measurer = LatencyMeasurer::new();
        measurer.start_peer_test();
        self.lat = Some(StockLat {
            sink,
            sink_audio,
            source,
            source_audio,
            measurer,
        });
    }

    fn addr(&self) -> SocketAddr {
        self.sock.local_addr().unwrap()
    }

    /// Its own peer state for us, without the dummy invite (`doAddRemotePeerIfNecessary`).
    /// Dynamic resampling follows stock's default setting: off (P.cpp:6073-6074; the aoo
    /// objects' own default is on). With it on here and off in the engine (also the default),
    /// its source reports the DLL-measured rate of the test thread's paced "device", which
    /// lags the wall clock whenever the thread runs late, and the engine plays that rate
    /// against its nominal one (as stock does): the tone drifts with the lag (880 -> 872 Hz
    /// at 1 %), which a 2 Hz-resolution tone measurement reads as near silence.
    fn know(&mut self) {
        let opts = SourceOptions {
            dynamic_resampling: false,
            ..SourceOptions::default()
        };
        let (mut source, source_audio) = Source::new(1, setup(), opts);
        source
            .set_format(Format {
                nchannels: 1,
                samplerate: SR as i32,
                blocksize: BLOCK as i32,
                codec: Codec::Pcm(PcmBitDepth::Int16),
            })
            .unwrap();
        let opts = SinkOptions {
            buffersize_ms: 60,
            dynamic_resampling: false,
            ..SinkOptions::default()
        };
        let (sink, sink_audio) = Sink::new(1, setup(), opts);
        self.link = Some(StockLink {
            remote_id: None,
            our_id: 1,
            source,
            source_audio,
            sink,
            sink_audio,
        });
    }

    /// Its server announcement of us (`connectRemotePeerRaw`): peer state, one dummy invite,
    /// `/sb/blockedinfo F`.
    fn announce(&mut self) {
        if self.link.is_none() {
            self.know();
        }
        self.invite_dummy();
        let mut b = Vec::new();
        let m = SbMessage::BlockedInfo {
            from: "stock",
            blocked: false,
        };
        m.encode(&mut b).unwrap();
        let _ = self.sock.send_to(&b, self.engine);
    }

    fn invite_dummy(&mut self) {
        let id = self.link.as_ref().unwrap().our_id;
        let m = Message::SourceInvite {
            src: 0,
            sink: id,
            flags: Some(0),
        };
        send_msg(&self.sock, self.engine, &m);
    }

    /// It lost us (`PeerLeft` after our one-sided server loss): everything is gone.
    fn forget(&mut self) {
        self.link = None;
    }

    fn poll(&mut self) {
        let mut buf = [0u8; aoo::MAX_PACKET_SIZE];
        let now = TimeTag::now();
        while let Ok((n, from)) = self.sock.recv_from(&mut buf) {
            let Ok(msg) = message::parse(&buf[..n]) else {
                continue;
            };
            use Message as M;
            match msg {
                M::SourceInvite { src: 0, .. } | M::SourceUninvite { src: 0, .. } => {
                    self.dummy.handle_message(from, &msg, now);
                }
                M::SinkFormat { sink, .. }
                | M::SinkData { sink, .. }
                | M::SinkPing { sink, .. }
                    if sink == STOCK_LAT_ID =>
                {
                    if let Some(l) = &mut self.lat {
                        l.sink.handle_message(from, &msg, now);
                    }
                }
                M::SourceFormat { src, .. }
                | M::SourceData { src, .. }
                | M::SourcePing { src, .. }
                | M::SourceInvite { src, .. }
                | M::SourceUninvite { src, .. }
                | M::SourceCodecChange { src, .. }
                    if src == STOCK_LAT_ID =>
                {
                    if let Some(l) = &mut self.lat {
                        l.source.handle_message(from, &msg, now);
                    }
                }
                M::SourceFormat { .. }
                | M::SourceData { .. }
                | M::SourcePing { .. }
                | M::SourceInvite { .. }
                | M::SourceUninvite { .. }
                | M::SourceCodecChange { .. } => {
                    if let Some(l) = &mut self.link {
                        l.source.handle_message(from, &msg, now);
                    }
                }
                M::CompactData { .. }
                | M::SinkFormat { .. }
                | M::SinkData { .. }
                | M::SinkPing { .. } => {
                    if let Some(l) = &mut self.link {
                        l.sink.handle_message(from, &msg, now);
                    }
                }
                M::NotAoo(raw) => {
                    if let Ok(Some(SbMessage::PeerInfo(p))) = sbmsg::parse(raw) {
                        self.pinfo = Some(p);
                    }
                }
                _ => {}
            }
        }
        while let Some(e) = self.dummy.poll_event() {
            let SourceEventKind::Invite { flags } = e.kind else {
                continue;
            };
            let Some(l) = &mut self.link else {
                // "Not reacting to invite from a peer not known in the group"
                self.ignored_invites += 1;
                continue;
            };
            l.source.add_sink(e.addr, e.id, flags);
            l.source.start();
            l.remote_id = Some(e.id);
            // invite them back at their source with the same id, once
            let m = Message::SourceInvite {
                src: e.id,
                sink: l.our_id,
                flags: Some(0),
            };
            send_msg(&self.sock, e.addr, &m);
        }
        if let Some(l) = &mut self.link {
            while let Some(e) = l.source.poll_event() {
                match e.kind {
                    SourceEventKind::Invite { flags } => {
                        l.source.add_sink(e.addr, e.id, flags);
                        l.source.start();
                    }
                    SourceEventKind::Uninvite => {
                        l.source.remove_sink(e.addr, e.id);
                    }
                    _ => {}
                }
            }
            while let Some(e) = l.sink.poll_event() {
                if let SinkEventKind::FormatChanged { format, .. } = e.kind {
                    self.format = Some(format);
                }
            }
        }
        if let Some(l) = &mut self.lat {
            while l.sink.poll_event().is_some() {}
            while l.source.poll_event().is_some() {}
        }
    }

    fn send(&mut self) {
        let (sock, t) = (&self.sock, TimeTag::now());
        if let Some(l) = &mut self.link {
            l.source.send(|a, b| {
                let _ = sock.send_to(b, a);
            });
            l.sink.send(t, |a, b| {
                let _ = sock.send_to(b, a);
            });
        }
        self.dummy.send(|_, _| {});
        if let Some(l) = &mut self.lat {
            l.source.send(|a, b| {
                let _ = sock.send_to(b, a);
            });
            l.sink.send(t, |a, b| {
                let _ = sock.send_to(b, a);
            });
        }
    }

    fn audio(&mut self) {
        let now = TimeTag::now();
        let mut x = [0.0f32; BLOCK];
        for v in &mut x {
            *v = AMP * (self.phase * std::f64::consts::TAU).sin() as f32;
            self.phase = (self.phase + STOCK_FREQ / f64::from(SR)).fract();
        }
        let mut y = [0.0f32; BLOCK];
        if let Some(l) = &mut self.link {
            l.source_audio.process(&[&x[..]], now);
            l.sink_audio.process(&mut [&mut y[..]], now);
        }
        keep(&mut self.heard, &y);
        if let Some(l) = &mut self.lat {
            let mut b = [0.0f32; BLOCK];
            l.sink_audio.process(&mut [&mut b[..]], now);
            l.measurer.process_input(&b, SR as i32);
            l.measurer.process_output(&mut b);
            l.source_audio.process(&[&b[..]], now);
        }
    }
}

struct Rig {
    e: Engine,
    a: EngineAudio,
    stock: StockPeer,
    phase: f64,
    heard: VecDeque<f32>,
}

/// The engine under test: mono in and out, 40 ms buffers, "in a group".
fn engine() -> (Engine, EngineAudio) {
    let mut s = AppState::new(false);
    s.params.dry = 0.0;
    s.params.def_net_buf = 0.04;
    let mut g = ChannelGroup::input_default(0, false);
    g.num_channels = 1;
    g.pans = vec![0.0];
    s.input_groups = vec![g];
    let audio = AudioConfig {
        sample_rate: SR,
        block_size: BLOCK,
        input_channels: 1,
        output_channels: 1,
    };
    let (e, a) = Engine::new(EngineConfig {
        audio,
        udp_port: 0,
        state: s,
    })
    .unwrap();
    // "in a group": only announced peers may connect
    e.with_legacy(|_, s| s.conn = ConnState::Connected);
    (e, a)
}

impl Rig {
    fn new() -> Self {
        let (e, a) = engine();
        let engine = SocketAddr::from(([127, 0, 0, 1], e.local_port()));
        Self {
            e,
            a,
            stock: StockPeer::new(engine),
            phase: 0.0,
            heard: VecDeque::new(),
        }
    }

    /// The server announces the stock peer to us.
    fn announce_to_us(&self) {
        let addr = self.stock.addr();
        self.e.with_legacy(|h, s| {
            let ev = ClientEvent::PeerJoined {
                group: "g".into(),
                user: "stock".into(),
                addr,
                token: 0,
            };
            s.on_client_event(h, ev, Instant::now());
        });
    }

    fn run(&mut self, secs: f64) {
        self.run_with(secs, || {});
    }

    /// Real-time paced blocks; `extra` runs once per block.
    fn run_with(&mut self, secs: f64, mut extra: impl FnMut()) {
        let block = Duration::from_secs_f64(BLOCK as f64 / f64::from(SR));
        let start = Instant::now();
        let mut next = start;
        while start.elapsed().as_secs_f64() < secs {
            let mut x = [0.0f32; BLOCK];
            for v in &mut x {
                *v = AMP * (self.phase * std::f64::consts::TAU).sin() as f32;
                self.phase = (self.phase + OUR_FREQ / f64::from(SR)).fract();
            }
            let mut y = [0.0f32; BLOCK];
            self.a.process(&[&x[..]], &mut [&mut y[..]]);
            keep(&mut self.heard, &y);
            self.stock.poll();
            self.stock.audio();
            self.stock.send();
            extra();
            next += block;
            match next.checked_duration_since(Instant::now()) {
                Some(d) => std::thread::sleep(d),
                None => next = Instant::now(),
            }
        }
    }

    /// Both directions carry their tone at full level (a mono output does not pan:
    /// mixer step 16, `1 / recv_channels`). The tone is measured at its exact frequency.
    fn two_way(&self) -> bool {
        let ours = tone(&self.heard, STOCK_FREQ);
        let theirs = tone(&self.stock.heard, OUR_FREQ);
        (ours - AMP).abs() < 0.15 * AMP && (theirs - AMP).abs() < 0.15 * AMP
    }

    fn until_two_way(&mut self, secs: f64) -> bool {
        let start = Instant::now();
        while start.elapsed().as_secs_f64() < secs {
            self.run(0.25);
            if self.two_way() {
                return true;
            }
        }
        eprintln!(
            "we hear {:.3}, stock hears {:.3}; stock ignored {} invites",
            tone(&self.heard, STOCK_FREQ),
            tone(&self.stock.heard, OUR_FREQ),
            self.stock.ignored_invites
        );
        false
    }
}

/// Their single dummy invite reaches us before the server announced them: we ignore it, so
/// the connection has to come from our (repeated) invite.
#[test]
fn stock_peer_their_invite_first() {
    let mut r = Rig::new();
    r.stock.announce();
    r.run(0.3);
    r.announce_to_us();
    assert!(r.until_two_way(6.0));
}

/// Our invite reaches them first; their own dummy invite follows later.
#[test]
fn stock_peer_our_invite_first() {
    let mut r = Rig::new();
    r.stock.know();
    r.announce_to_us();
    r.run(0.3);
    r.stock.invite_dummy();
    assert!(r.until_two_way(6.0));
}

/// They process their announcement of us late: our first invites are dropped.
#[test]
fn stock_peer_their_peer_joined_late() {
    let mut r = Rig::new();
    r.announce_to_us();
    r.run(1.5);
    assert!(
        r.stock.ignored_invites > 0,
        "our invites came before they knew us"
    );
    r.stock.announce();
    assert!(r.until_two_way(6.0));
}

/// One-sided server loss: we kept the peer, they got `PeerLeft` and dropped theirs. On our
/// rejoin the server announces them again while our sink still records their old stream's
/// format; they process their announcement of us a little later, so our first invite is
/// dropped. The invite must keep coming until their new stream arrives.
#[test]
fn stock_peer_rebuilt_after_our_server_loss() {
    let mut r = Rig::new();
    r.stock.know();
    r.announce_to_us();
    r.run(0.2);
    r.stock.invite_dummy();
    assert!(r.until_two_way(6.0), "first connection");
    r.stock.forget();
    r.run(0.5);
    r.heard.clear();
    r.stock.heard.clear();
    r.announce_to_us();
    let ignored = r.stock.ignored_invites;
    r.run(0.3);
    assert!(
        r.stock.ignored_invites > ignored,
        "our first invite was dropped"
    );
    r.stock.announce();
    assert!(r.until_two_way(8.0), "reconnected");
}

/// A third host streams at the engine's sink for the stock peer (and at the wildcard id):
/// nothing of it may reach the output.
#[test]
fn foreign_stream_cannot_be_injected_into_a_peer() {
    let mut r = Rig::new();
    r.stock.know();
    r.announce_to_us();
    r.run(0.2);
    r.stock.invite_dummy();
    assert!(r.until_two_way(6.0));
    let evil = UdpSocket::bind("127.0.0.1:0").unwrap();
    let engine = r.stock.engine;
    let (mut src, mut src_audio) = Source::new(7, setup(), SourceOptions::default());
    src.set_format(Format {
        nchannels: 1,
        samplerate: SR as i32,
        blocksize: BLOCK as i32,
        codec: Codec::Pcm(PcmBitDepth::Int16),
    })
    .unwrap();
    src.add_sink(engine, 1, 0);
    src.add_sink(engine, aoo::ID_WILDCARD, 0);
    src.start();
    let mut phase = 0.0f64;
    r.run_with(2.0, || {
        let mut x = [0.0f32; BLOCK];
        for v in &mut x {
            *v = (phase * std::f64::consts::TAU).sin() as f32;
            phase = (phase + 1500.0 / f64::from(SR)).fract();
        }
        src_audio.process(&[&x[..]], TimeTag::now());
        src.send(|a, b| {
            let _ = evil.send_to(b, a);
        });
    });
    assert!(tone(&r.heard, 1500.0) < 0.01, "{}", tone(&r.heard, 1500.0));
    assert!(
        r.two_way(),
        "the real peer is unaffected: {} {}",
        tone(&r.heard, STOCK_FREQ),
        tone(&r.stock.heard, OUR_FREQ)
    );
}

/// A stock user runs its latency test against us: our echo service answers. Expected round
/// trip: our echo sink buffer for them (40 ms) + their latency sink buffer (60 ms) + a few
/// blocks (send rings, block quantisation of four hops, the measurer's block): ~120 ms; we
/// accept 100..180 ms. The rig paces both ends by the wall clock on the test thread, so a
/// scheduler stall during the one measurement (other real-time tests run in parallel) adds
/// itself to the measured round trip: an out-of-range result is measured once more on a fresh
/// rig, and only two in a row fail.
#[test]
fn stock_peer_latency_test_against_our_echo() {
    let measure = || {
        let mut r = Rig::new();
        r.stock.know();
        r.announce_to_us();
        r.run(0.2);
        r.stock.invite_dummy();
        assert!(r.until_two_way(6.0));
        r.stock.start_latency_test();
        let start = Instant::now();
        while r.stock.lat.as_ref().unwrap().measurer.state < 3 && start.elapsed().as_secs() < 8 {
            r.run(0.1);
        }
        let m = &r.stock.lat.as_ref().unwrap().measurer;
        assert!(m.state >= 3, "measurements: {}", m.state);
        eprintln!("stock latency test against us: {:.1} ms", m.latency_ms);
        assert!(r.two_way(), "the main streams are unaffected");
        m.latency_ms
    };
    let ok = |ms: f32| (100.0..180.0).contains(&ms);
    let first = measure();
    if !ok(first) {
        let second = measure();
        assert!(ok(second), "{first} then {second}");
    }
}

/// PR #201: we advertise our 11 codecs in `/sb/pinfo`; a stock peer advertises nothing and
/// gets every format; a peer listing fewer only gets formats from its list (the nearest one
/// when asked for another), and the stream follows.
#[test]
fn codec_advertisement_with_a_stock_peer() {
    use crate::formats::{CODEC_IDS, FORMATS};
    let mut r = Rig::new();
    r.stock.know();
    r.announce_to_us();
    r.run(0.2);
    r.stock.invite_dummy();
    assert!(r.until_two_way(6.0));
    let ours = r
        .stock
        .pinfo
        .clone()
        .expect("our /sb/pinfo")
        .supported_codecs;
    assert_eq!(ours, Some(CODEC_IDS.map(str::to_owned).to_vec()));

    let peer = |r: &Rig| r.e.snapshot().peers[0].clone();
    let id = peer(&r).id;
    let all: Vec<usize> = (0..FORMATS.len()).collect();
    assert_eq!(
        peer(&r).supported_formats,
        all,
        "stock sends no list: legacy set"
    );
    r.e.set_peer_send_format(id, 9);
    assert_eq!(peer(&r).send_format, 9);

    // it now says it decodes 48k Opus and 96k Opus only (plus a codec we do not know)
    let send_info = |r: &Rig, codecs: Option<Vec<String>>| {
        let m = SbMessage::PeerInfo(PeerInfo {
            supported_codecs: codecs,
            ..PeerInfo::default()
        });
        let mut b = Vec::new();
        m.encode(&mut b).unwrap();
        r.stock.sock.send_to(&b, r.stock.engine).unwrap();
    };
    let list = vec![
        "DF40BA8A-3621-42D2-A9D7-B6FA0EF1BE84".to_owned(),
        CODEC_IDS[4].to_owned(),
        "pcm8-unknown".to_owned(),
    ];
    send_info(&r, Some(list));
    let start = Instant::now();
    while peer(&r).supported_formats.len() == FORMATS.len() && start.elapsed().as_secs() < 3 {
        r.run(0.05);
    }
    assert_eq!(peer(&r).supported_formats, [2, 4]);
    assert_eq!(
        peer(&r).send_format,
        4,
        "PCM 24 is not listed: nearest listed"
    );
    r.run(0.5);
    let opus = |f: Option<Format>| match f.map(|f| f.codec) {
        Some(Codec::Opus(o)) => o.bitrate,
        _ => 0,
    };
    assert_eq!(opus(r.stock.format), 96_000, "the stream follows");
    r.e.set_peer_send_format(id, 0);
    assert_eq!(peer(&r).send_format, 2);
    r.e.set_peer_send_format(id, 3);
    assert_eq!(peer(&r).send_format, 2, "a tie goes to the cheaper one");
    r.e.set_send_format_for_all(10);
    assert_eq!(peer(&r).send_format, 4);
    // nothing we know: the format stays
    send_info(&r, Some(vec!["pcm8-unknown".to_owned()]));
    let start = Instant::now();
    while !peer(&r).supported_formats.is_empty() && start.elapsed().as_secs() < 3 {
        r.run(0.05);
    }
    r.e.set_peer_send_format(id, 0);
    assert_eq!(peer(&r).send_format, 4);
    // a later /sb/pinfo without the key: legacy again
    send_info(&r, None);
    let start = Instant::now();
    while peer(&r).supported_formats.len() != FORMATS.len() && start.elapsed().as_secs() < 3 {
        r.run(0.05);
    }
    r.e.set_peer_send_format(id, 8);
    assert_eq!(peer(&r).send_format, 8);
    assert!(r.until_two_way(4.0), "still streaming both ways");
}

/// The primary non-loopback IPv4 address of this machine, if any (a UDP connect sends nothing).
fn lan_ip() -> Option<std::net::IpAddr> {
    let s = UdpSocket::bind("0.0.0.0:0").ok()?;
    s.connect("192.0.2.1:9").ok()?;
    Some(s.local_addr().ok()?.ip()).filter(|ip| !ip.is_loopback() && !ip.is_unspecified())
}

/// Two engines on one host whose handshakes resolved each other through different aliases
/// of it: `a` reached `b` through the LAN address, `b` reached `a` through 127.0.0.1 (a host
/// joined through its LAN address, a joiner through loopback, and the handshake pings landed
/// in that order). Each one's datagrams then come from the alias the other did not resolve;
/// audio must still flow both ways.
#[test]
fn same_host_peers_resolved_through_different_aliases() {
    let Some(lan) = lan_ip() else {
        eprintln!("no LAN address: skipped");
        return;
    };
    let ((a, mut aa), (b, mut ba)) = (engine(), engine());
    let announce = |e: &Engine, user: &str, addr: SocketAddr| {
        e.with_legacy(|h, s| {
            let ev = ClientEvent::PeerJoined {
                group: "g".into(),
                user: user.into(),
                addr,
                token: 0,
            };
            s.on_client_event(h, ev, Instant::now());
        });
    };
    announce(&a, "b", SocketAddr::new(lan, b.local_port()));
    announce(&b, "a", SocketAddr::from(([127, 0, 0, 1], a.local_port())));
    let (mut heard_a, mut heard_b) = (VecDeque::new(), VecDeque::new());
    let mut phase = [0.0f64; 2];
    let block = Duration::from_secs_f64(BLOCK as f64 / f64::from(SR));
    let start = Instant::now();
    let mut next = start;
    let ok = |ha: &VecDeque<f32>, hb: &VecDeque<f32>| {
        // full level on a mono output (no pan law), as in `Rig::two_way`
        (tone(ha, STOCK_FREQ) - AMP).abs() < 0.15 * AMP
            && (tone(hb, OUR_FREQ) - AMP).abs() < 0.15 * AMP
    };
    while start.elapsed().as_secs_f64() < 8.0 && !ok(&heard_a, &heard_b) {
        for (k, (au, heard, freq)) in [
            (&mut aa, &mut heard_a, OUR_FREQ),
            (&mut ba, &mut heard_b, STOCK_FREQ),
        ]
        .into_iter()
        .enumerate()
        {
            let mut x = [0.0f32; BLOCK];
            for v in &mut x {
                *v = AMP * (phase[k] * std::f64::consts::TAU).sin() as f32;
                phase[k] = (phase[k] + freq / f64::from(SR)).fract();
            }
            let mut y = [0.0f32; BLOCK];
            au.process(&[&x[..]], &mut [&mut y[..]]);
            keep(heard, &y);
        }
        next += block;
        match next.checked_duration_since(Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = Instant::now(),
        }
    }
    assert!(
        ok(&heard_a, &heard_b),
        "a hears {:.3}, b hears {:.3}; a: {:?}, b: {:?}",
        tone(&heard_a, STOCK_FREQ),
        tone(&heard_b, OUR_FREQ),
        a.snapshot()
            .peers
            .first()
            .map(|p| (p.addr, p.connected, p.packets_received)),
        b.snapshot()
            .peers
            .first()
            .map(|p| (p.addr, p.connected, p.packets_received)),
    );
}

impl Rig {
    /// Connected both ways with a stock peer.
    fn connected() -> Self {
        let mut r = Rig::new();
        r.stock.know();
        r.announce_to_us();
        r.run(0.2);
        r.stock.invite_dummy();
        assert!(r.until_two_way(6.0));
        r
    }

    /// `m` from `sock` to the engine, then a few blocks.
    fn sb_from(&mut self, sock: &UdpSocket, m: &SbMessage) {
        let mut b = Vec::new();
        m.encode(&mut b).unwrap();
        sock.send_to(&b, self.stock.engine).unwrap();
        self.run(0.1);
    }

    fn notes(&self) -> Vec<crate::Notification> {
        std::iter::from_fn(|| self.e.poll_notification()).collect()
    }
}

/// Latency figures from a peer cannot blow up a jitter buffer (review finding 1): absurd
/// `/sb/pinfo` latencies, a forged `/sb/pngack`, an infinite or huge match target. Every
/// buffer stays within `MAX_BUFFER_MS`.
#[test]
fn peer_latency_figures_are_bounded() {
    use crate::jitter::{MAX_BUFFER_MS, MAX_PEER_LATENCY_MS};
    let mut r = Rig::connected();
    let stock = r.stock.sock.try_clone().unwrap();
    let info = SbMessage::PeerInfo(PeerInfo {
        in_latency_ms: Some(-1e9),
        out_latency_ms: Some(1e300),
        jitbuf_ms: Some(-5.0),
        ..PeerInfo::default()
    });
    r.sb_from(&stock, &info);
    // a forged ping answer from the far past and one from the future
    for origin_time in [0, u64::MAX] {
        let m = SbMessage::PingAck {
            origin_time,
            reply_time: 0,
        };
        r.sb_from(&stock, &m);
    }
    let (rem, ping) =
        r.e.with_legacy(|_, s| (s.peers[0].c.remote, s.peers[0].c.ping.smooth.mean));
    assert_eq!(rem.in_latency_ms, 0.0);
    assert_eq!(rem.out_latency_ms, MAX_PEER_LATENCY_MS);
    assert_eq!(rem.jitter_buffer_ms, 0.0);
    assert!((0.0..600.0).contains(&ping), "{ping}");
    let buf = |r: &Rig| r.e.snapshot().peers[0].buffer_ms;
    r.e.commit_latency_match(50.0);
    assert!(buf(&r) <= 50.0, "{}", buf(&r));
    let before = buf(&r);
    r.e.commit_latency_match(f32::INFINITY);
    r.e.commit_latency_match(f32::NAN);
    assert_eq!(buf(&r), before);
    r.e.commit_latency_match(1e12);
    assert!(buf(&r) <= MAX_BUFFER_MS, "{}", buf(&r));
    r.e.receive_sync(f32::NAN);
    r.e.set_peer_buffer_ms(r.e.snapshot().peers[0].id, 1e9);
    assert_eq!(buf(&r), MAX_BUFFER_MS);
    r.e.set_peer_buffer_ms(r.e.snapshot().peers[0].id, 40.0);
    assert!(r.until_two_way(6.0), "still streaming");
}

/// `/sb` messages count only from current peers (review finding 2): a stranger gets no
/// `/sb/latinfo` or ping answer and cannot complete our latency table or reach the app;
/// a peer's `/sb/latinfo` counts once per request; suggestions are rate-limited and cannot
/// push a recording error out of the notification queue.
#[test]
fn sb_messages_only_from_peers() {
    let mut r = Rig::connected();
    let stranger = UdpSocket::bind("127.0.0.1:0").unwrap();
    stranger
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let stock = r.stock.sock.try_clone().unwrap();
    let huge = LatEntry {
        src_name: "x".into(),
        dest_name: "y".into(),
        latency_ms: 1e12,
    };
    r.e.begin_latency_match();
    r.sb_from(&stranger, &SbMessage::LatInfo(vec![huge.clone(); 3]));
    r.sb_from(&stranger, &SbMessage::ReqLatInfo);
    r.sb_from(&stranger, &SbMessage::Ping { origin_time: 1 });
    r.sb_from(
        &stranger,
        &SbMessage::SuggestLatency {
            from: "evil",
            latency_ms: 10.0,
        },
    );
    let mut b = [0u8; 2048];
    assert!(
        stranger.recv_from(&mut b).is_err(),
        "no answer to a stranger"
    );
    assert!(r.notes().is_empty());
    let (entries, ready, _) = r.e.latency_match_info();
    assert!(!ready && entries.len() == 1, "{entries:?}");

    // the peer's answer counts once, clamped
    r.sb_from(&stock, &SbMessage::LatInfo(vec![huge.clone()]));
    r.sb_from(&stock, &SbMessage::LatInfo(vec![huge.clone(); 5]));
    let (entries, ready, target) = r.e.latency_match_info();
    assert!(ready && entries.len() == 2, "{entries:?}");
    assert_eq!(target, crate::jitter::MAX_PEER_LATENCY_MS);
    assert!(r.notes().contains(&crate::Notification::LatencyMatchReady));

    // a flood of suggestions: one notification, and a recording error survives a full queue;
    // the sender shown is the peer's own name, whatever the body claims
    for _ in 0..30 {
        let m = SbMessage::SuggestLatency {
            from: "the-band-leader",
            latency_ms: 1e30,
        };
        let mut b = Vec::new();
        m.encode(&mut b).unwrap();
        stock.send_to(&b, r.stock.engine).unwrap();
    }
    r.run(0.3);
    let n = r.notes();
    assert_eq!(
        n,
        [crate::Notification::LatencyMatchRequested {
            from: "stock".into(),
            latency_ms: crate::jitter::MAX_PEER_LATENCY_MS
        }]
    );
}

/// A peer cannot multiply our echo upload or our decoding (review finding 3): invites from
/// many sink ids leave one destination on our echo source (the latest), and streams into our
/// echo sink from anything but its latency source are not decoded.
#[test]
fn echo_service_has_one_destination() {
    let mut r = Rig::connected();
    let ours = r.e.with_legacy(|_, s| s.peers[0].our_id);
    let stock = r.stock.sock.try_clone().unwrap();
    for sink in 0..50 {
        let m = Message::SourceInvite {
            src: ours + 40000,
            sink: 30000 + sink,
            flags: Some(0),
        };
        send_msg(&stock, r.stock.engine, &m);
    }
    r.run(0.3);
    let n = r.e.with_legacy(|_, s| s.peers[0].echo_source.sink_count());
    assert_eq!(n, 1);
    // a stream into our echo sink from another source id is ignored
    let (mut src, mut src_audio) = Source::new(9, setup(), SourceOptions::default());
    src.set_format(Format {
        nchannels: 1,
        samplerate: SR as i32,
        blocksize: BLOCK as i32,
        codec: Codec::Pcm(PcmBitDepth::Int16),
    })
    .unwrap();
    src.add_sink(r.stock.engine, ours + 40000, 0);
    src.start();
    r.run_with(0.5, || {
        src_audio.process(&[&[0.5f32; BLOCK][..]], TimeTag::now());
        src.send(|a, b| {
            let _ = stock.send_to(b, a);
        });
    });
    let known = r.e.with_legacy(|_, s| s.peers[0].echo_sink.source_count());
    assert_eq!(known, 0, "not even tracked");
}

/// Peer names lose control characters where they enter (review finding 4: they reach file
/// names, the latency sidecar and the remembered-solo option); a peer without a name records
/// as `user<slot>`.
#[test]
fn peer_names_are_cleaned_and_unnamed_peers_record() {
    let (e, _a) = engine();
    let announce = |user: &str, port: u16| {
        e.with_legacy(|h, s| {
            let ev = ClientEvent::PeerJoined {
                group: "g".into(),
                user: user.into(),
                addr: SocketAddr::from(([127, 0, 0, 1], port)),
                token: 0,
            };
            s.on_client_event(h, ev, Instant::now());
        });
    };
    announce("a\x1eb\x1f1\tx\n", 9);
    announce("", 10);
    let users: Vec<String> = e.snapshot().peers.iter().map(|p| p.user.clone()).collect();
    assert_eq!(users, ["ab1x", ""]);
    e.update_state(|s| {
        s.extra.default_recording_options = 8;
        s.extra.default_recording_format = 3;
    });
    let dir = std::env::temp_dir().join(format!("sb-names-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    e.start_recording(&dir, "take").unwrap();
    let files = e.stop_recording().unwrap();
    let names: Vec<String> = files
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert!(names.iter().any(|n| n == "take-ab1x.wav"), "{names:?}");
    assert!(names.iter().any(|n| n == "take-user2.wav"), "{names:?}");
    let side = std::fs::read_to_string(dir.join("take").join("take-latency.txt")).unwrap();
    assert_eq!(side.lines().count(), 4, "{side}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Review finding 3, legacy side: a stock peer's codec-change request is followed (as
/// upstream), capped at "Max Send Quality", and ignored while requests are not honoured.
#[test]
fn stock_codec_requests_respect_max_send_quality() {
    let mut r = Rig::connected();
    let ours = r.e.with_legacy(|_, s| s.peers[0].our_id);
    let engine = r.stock.engine;
    let ask = |r: &mut Rig, codec: Codec| {
        let f = Format {
            nchannels: 1,
            samplerate: SR as i32,
            blocksize: BLOCK as i32,
            codec,
        };
        assert!(
            r.stock
                .link
                .as_mut()
                .unwrap()
                .sink
                .request_codec_change(engine, ours, f)
        );
    };
    let fmt = |r: &Rig| r.e.snapshot().peers[0].send_format;
    let wait = |r: &mut Rig, want: usize| {
        let start = Instant::now();
        while fmt(r) != want && start.elapsed().as_secs() < 3 {
            r.run(0.05);
        }
        r.run(0.3);
        fmt(r)
    };
    ask(&mut r, Codec::Pcm(PcmBitDepth::Int16));
    assert_eq!(wait(&mut r, 8), 8, "followed, as upstream");
    r.e.set_max_send_quality(Some(4));
    ask(&mut r, Codec::Pcm(PcmBitDepth::Int24));
    assert_eq!(wait(&mut r, 4), 4, "capped at 96 kbps/ch Opus");
    let opus = |f: Option<Format>| match f.map(|f| f.codec) {
        Some(Codec::Opus(o)) => o.bitrate,
        _ => 0,
    };
    assert_eq!(
        opus(r.stock.format),
        96_000,
        "and the stream follows the cap"
    );
    r.e.set_honour_quality_requests(false);
    ask(&mut r, Codec::Pcm(PcmBitDepth::Int16));
    r.run(0.5);
    assert_eq!(fmt(&r), 4, "not honoured");
    assert!(r.until_two_way(4.0), "still streaming both ways");
}
