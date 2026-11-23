//! Audio streaming against the upstream C++ source and sink (SonoBus 1.7.2 `deps/aoo`), in
//! lockstep over pipes (no sockets, deterministic). `#[ignore]`d; build the host once:
//!
//! ```sh
//! cd .locki/tmp/aoo-ref && mkdir -p stream_obj && cd stream_obj   # peer_main.cpp is one level up
//! R=../../ref/up-main/deps/aoo
//! D="-DUSE_CODEC_OPUS=1 -DAOO_TIMEFILTER_CHECK=0 -DAOO_STATIC"     # as SonoBus builds it
//! g++ -std=c++17 -O1 -w $D -I$R/lib -I$R/lib/src -I$R/deps -I$R/deps/oscpack -c \
//!     $R/lib/src/{source,sink,common,codec_pcm,codec_opus,time,sync}.cpp \
//!     $R/deps/oscpack/osc/{OscOutboundPacketStream,OscReceivedElements,OscTypes}.cpp
//! g++ -std=c++17 -O1 -w $D -I$R/lib -o ../aoo_peer ../peer_main.cpp *.o -lopus -lpthread
//! ```
//!
//! then run (system libopus on both sides, so Opus output must match bit for bit too):
//!
//! ```sh
//! AOO_CPP_PEER=$PWD/.locki/tmp/aoo-ref/aoo_peer mise exec -- cargo test -p aoo --test interop -- --ignored
//! ```
//!
//! - (a) `cpp_source_to_rust_sink`: C++ source packets go to a C++ sink and a Rust sink; their
//!   outputs must be identical (PCM additionally equals the input after the buffer delay).
//! - (b) `rust_source_to_cpp_sink`: the same with a Rust source, dropping packets (resends from
//!   the Rust source's history) and changing the format mid-stream.
//! - `stock_sink_follows_sequence_wrap`: a C++ sink keeps playing a Rust source across the
//!   `i32::MAX` sequence restart.
//! - (c) `source_packets_byte_identical`: C++ and Rust sources fed the same audio and times emit
//!   the same packets byte for byte (salt masked).

use std::io::{BufReader, BufWriter, Read, Write};
use std::net::SocketAddr;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use aoo::codec::{
    Codec, Format, OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_SIGNAL_MUSIC, OpusOptions,
    PcmBitDepth,
};
use aoo::sink::{Sink, SinkAudio, SinkOptions};
use aoo::source::{Source, SourceOptions};
use aoo::{Message, Setup, TimeTag, message};

const PEER: SocketAddr = SocketAddr::new(std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST), 9);
const SR: i32 = 48000;
const EPOCH: f64 = 1000.0;
const AOO_BLOCK_RESENT_EVENT: i32 = 10;

struct Cpp {
    child: Child,
    w: BufWriter<ChildStdin>,
    r: BufReader<ChildStdout>,
}

impl Cpp {
    fn spawn(args: &[String]) -> Self {
        let path = std::env::var("AOO_CPP_PEER").expect("set AOO_CPP_PEER to the aoo_peer binary");
        let mut child = Command::new(path)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let w = BufWriter::new(child.stdin.take().unwrap());
        let r = BufReader::new(child.stdout.take().unwrap());
        Self { child, w, r }
    }

    fn u32(&mut self) -> u32 {
        let mut b = [0; 4];
        self.r.read_exact(&mut b).unwrap();
        u32::from_ne_bytes(b)
    }

    fn msg(&mut self, p: &[u8]) {
        self.w.write_all(b"M").unwrap();
        self.w.write_all(&(p.len() as u32).to_ne_bytes()).unwrap();
        self.w.write_all(p).unwrap();
    }

    fn process_source(&mut self, t: TimeTag, planar: &[Vec<f32>]) {
        self.w.write_all(b"P").unwrap();
        self.w.write_all(&t.0.to_ne_bytes()).unwrap();
        self.w
            .write_all(&(planar[0].len() as u32).to_ne_bytes())
            .unwrap();
        for c in planar {
            for x in c {
                self.w.write_all(&x.to_ne_bytes()).unwrap();
            }
        }
        self.w.flush().unwrap();
        self.u32();
    }

    fn process_sink(&mut self, t: TimeTag, n: usize, nch: usize) -> Vec<Vec<f32>> {
        self.w.write_all(b"P").unwrap();
        self.w.write_all(&t.0.to_ne_bytes()).unwrap();
        self.w.write_all(&(n as u32).to_ne_bytes()).unwrap();
        self.w.flush().unwrap();
        self.u32();
        (0..nch)
            .map(|_| (0..n).map(|_| f32::from_bits(self.u32())).collect())
            .collect()
    }

    fn send(&mut self) -> Vec<Vec<u8>> {
        self.w.write_all(b"S").unwrap();
        self.w.flush().unwrap();
        let mut v = Vec::new();
        loop {
            let n = self.u32() as usize;
            if n == 0 {
                return v;
            }
            let mut p = vec![0; n];
            self.r.read_exact(&mut p).unwrap();
            v.push(p);
        }
    }

    fn events(&mut self) -> Vec<(i32, i32)> {
        self.w.write_all(b"E").unwrap();
        self.w.flush().unwrap();
        let mut v = Vec::new();
        loop {
            let t = self.u32() as i32;
            if t == -1 {
                return v;
            }
            v.push((t, self.u32() as i32));
        }
    }
}

impl Drop for Cpp {
    fn drop(&mut self) {
        let _ = self.w.write_all(b"Q");
        let _ = self.w.flush();
        let _ = self.child.wait();
    }
}

#[derive(Clone, Copy, Debug)]
struct Case {
    codec: Codec,
    nch: i32,
    bs: i32,
    dynamic: bool,
}

impl Case {
    fn format(&self) -> Format {
        Format {
            nchannels: self.nch,
            samplerate: SR,
            blocksize: self.bs,
            codec: self.codec,
        }
    }

    fn cpp_source_args(&self) -> Vec<String> {
        let (name, param) = match self.codec {
            Codec::Pcm(d) => ("pcm", d as i32),
            Codec::Opus(o) => ("opus", o.bitrate),
        };
        let a = [
            "source".into(),
            "1".into(),
            SR.to_string(),
            self.bs.to_string(),
            self.nch.to_string(),
            name.into(),
            param.to_string(),
            "600".into(),
            (self.dynamic as i32).to_string(),
            "7".into(),
            "1".into(),
        ];
        a.to_vec()
    }

    fn cpp_sink_args(&self) -> Vec<String> {
        let a = [
            "sink".into(),
            "7".into(),
            SR.to_string(),
            self.bs.to_string(),
            self.nch.to_string(),
            "60".into(),
            (self.dynamic as i32).to_string(),
            "1".into(),
        ];
        a.to_vec()
    }

    fn setup(&self) -> Setup {
        Setup {
            samplerate: SR,
            blocksize: self.bs,
            nchannels: self.nch,
        }
    }

    fn source_opts(&self) -> SourceOptions {
        SourceOptions {
            dynamic_resampling: self.dynamic,
            ..SourceOptions::default()
        }
    }

    fn rust_sink(&self) -> (Sink, SinkAudio) {
        let opts = SinkOptions {
            buffersize_ms: 60,
            dynamic_resampling: self.dynamic,
            ..SinkOptions::default()
        };
        Sink::new(7, self.setup(), opts)
    }
}

fn opus(nch: i32) -> Codec {
    Codec::Opus(OpusOptions {
        bitrate: 96_000 * nch,
        complexity: 10,
        signal_type: OPUS_SIGNAL_MUSIC,
        application_type: OPUS_APPLICATION_RESTRICTED_LOWDELAY,
    })
}

fn matrix() -> Vec<(&'static str, Case)> {
    let pcm = |d| Codec::Pcm(d);
    vec![
        (
            "pcm int16",
            Case {
                codec: pcm(PcmBitDepth::Int16),
                nch: 2,
                bs: 256,
                dynamic: false,
            },
        ),
        // 512 frames * 2 ch * 4 bytes = 4096 bytes = 8 frames of 520
        (
            "pcm float32 multi-frame",
            Case {
                codec: pcm(PcmBitDepth::Float32),
                nch: 2,
                bs: 512,
                dynamic: false,
            },
        ),
        (
            "opus mono",
            Case {
                codec: opus(1),
                nch: 1,
                bs: 240,
                dynamic: true,
            },
        ),
        (
            "opus stereo",
            Case {
                codec: opus(2),
                nch: 2,
                bs: 240,
                dynamic: true,
            },
        ),
        (
            "opus 4ch",
            Case {
                codec: opus(4),
                nch: 4,
                bs: 240,
                dynamic: true,
            },
        ),
    ]
}

fn input(k: usize, bs: usize, nch: usize) -> Vec<Vec<f32>> {
    (0..nch)
        .map(|c| {
            (0..bs)
                .map(|i| {
                    let n = (k * bs + i) as f32;
                    (n * 0.013 * (1.0 + c as f32 * 0.37)).sin() * 0.6 + (n * 0.0011).sin() * 0.3
                })
                .collect()
        })
        .collect()
}

fn time(k: usize, bs: i32) -> TimeTag {
    TimeTag::from_secs(EPOCH + k as f64 * bs as f64 / SR as f64)
}

fn rust_play(audio: &mut SinkAudio, t: TimeTag, n: usize, nch: usize) -> Vec<Vec<f32>> {
    let mut bufs = vec![vec![0.0f32; n]; nch];
    let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
    audio.process(&mut refs, t);
    bufs
}

fn deliver(p: &[u8], cpp: &mut Cpp, sink: &mut Sink, t: TimeTag) {
    cpp.msg(p);
    sink.handle_message(PEER, &message::parse(p).unwrap(), t);
}

fn assert_same(name: &str, k: usize, rust: &[Vec<f32>], cpp: &[Vec<f32>]) {
    for (c, (r, p)) in rust.iter().zip(cpp).enumerate() {
        if r != p {
            let i = r
                .iter()
                .zip(p)
                .position(|(a, b)| a.to_bits() != b.to_bits())
                .unwrap();
            panic!(
                "{name}: block {k} channel {c} sample {i}: rust {} cpp {}",
                r[i], p[i]
            );
        }
    }
}

const BLOCKS: usize = 600;

#[test]
#[ignore]
fn cpp_source_to_rust_sink() {
    for (name, case) in matrix() {
        let (bs, nch) = (case.bs as usize, case.nch as usize);
        let mut src = Cpp::spawn(&case.cpp_source_args());
        let mut csink = Cpp::spawn(&case.cpp_sink_args());
        let (mut sink, mut audio) = case.rust_sink();
        let mut outputs = vec![Vec::new(); nch];
        let mut ins = vec![Vec::new(); nch];
        for k in 0..BLOCKS {
            let t = time(k, case.bs);
            let x = input(k, bs, nch);
            for (i, c) in ins.iter_mut().zip(&x) {
                i.extend_from_slice(c);
            }
            src.process_source(t, &x);
            let mut pkts = src.send();
            for _ in 0..3 {
                for p in &pkts {
                    deliver(p, &mut csink, &mut sink, t);
                }
                sink.send(t, |_, _| {});
                let replies = csink.send();
                for r in &replies {
                    src.msg(r);
                }
                pkts = src.send();
            }
            let r = rust_play(&mut audio, t, bs, nch);
            let c = csink.process_sink(t, bs, nch);
            assert_same(name, k, &r, &c);
            for (o, b) in outputs.iter_mut().zip(&r) {
                o.extend_from_slice(b);
            }
        }
        let energy: f32 = outputs[0].iter().map(|x| x * x).sum();
        assert!(energy > 10.0, "{name}: silent");
        if let Codec::Pcm(d) = case.codec {
            // no resampling: the output is the quantised input after the buffer delay
            let q = |x: f32| match d {
                PcmBitDepth::Int16 => ((x * 32767.0 + 0.5) as i32) as f32 / 32768.0,
                _ => x,
            };
            let probe = BLOCKS * bs / 2;
            let d0 = (0..BLOCKS * bs / 4)
                .find(|&d| (0..32).all(|i| outputs[0][probe + i] == q(ins[0][probe + i - d])))
                .unwrap_or_else(|| panic!("{name}: no alignment"));
            for c in 0..nch {
                for n in d0 + 2 * bs..outputs[c].len() {
                    assert_eq!(outputs[c][n], q(ins[c][n - d0]), "{name} ch {c} sample {n}");
                }
            }
        }
        eprintln!("{name}: {BLOCKS} blocks identical");
    }
}

#[test]
#[ignore]
fn rust_source_to_cpp_sink() {
    for (name, case) in matrix() {
        let (bs, nch) = (case.bs as usize, case.nch as usize);
        let (mut src, mut src_audio) = Source::new(1, case.setup(), case.source_opts());
        src.set_format(case.format()).unwrap();
        src.add_sink(PEER, 7, 1);
        src.start();
        let mut csink = Cpp::spawn(&case.cpp_sink_args());
        let (mut sink, mut audio) = case.rust_sink();
        let mut ndata = 0usize;
        let mut dropped = 0;
        for k in 0..BLOCKS {
            let t = time(k, case.bs);
            if k == BLOCKS / 2 {
                // format change mid-stream: new codec settings, new salt, sinks re-buffer
                let f = match case.codec {
                    Codec::Pcm(PcmBitDepth::Int16) => Codec::Pcm(PcmBitDepth::Int24),
                    Codec::Pcm(_) => Codec::Pcm(PcmBitDepth::Float64),
                    Codec::Opus(o) => Codec::Opus(OpusOptions {
                        bitrate: o.bitrate / 2,
                        ..o
                    }),
                };
                src.set_format(Format {
                    codec: f,
                    ..case.format()
                })
                .unwrap();
            }
            src_audio.process(
                &input(k, bs, nch).iter().map(|c| &c[..]).collect::<Vec<_>>(),
                t,
            );
            let mut pkts = Vec::new();
            src.send(|_, p| pkts.push(p.to_vec()));
            for p in &pkts {
                // drop every 9th data packet on the way in (both sinks lose the same ones)
                let is_data = p.starts_with(b"/d\0") || p.starts_with(b"/aoo/sink/7/data");
                if is_data {
                    ndata += 1;
                    if ndata.is_multiple_of(9) && k < BLOCKS - 20 {
                        dropped += 1;
                        continue;
                    }
                }
                deliver(p, &mut csink, &mut sink, t);
            }
            // requests from both sinks, answered immediately and delivered to both
            for _ in 0..3 {
                for r in csink.send() {
                    src.handle_message(PEER, &message::parse(&r).unwrap(), t);
                }
                let mut rr = Vec::new();
                sink.send(t, |_, p| rr.push(p.to_vec()));
                for r in &rr {
                    src.handle_message(PEER, &message::parse(r).unwrap(), t);
                }
                let mut again = Vec::new();
                src.send(|_, p| again.push(p.to_vec()));
                for p in &again {
                    deliver(p, &mut csink, &mut sink, t);
                }
            }
            while src.poll_event().is_some() {}
            let r = rust_play(&mut audio, t, bs, nch);
            let c = csink.process_sink(t, bs, nch);
            assert_same(name, k, &r, &c);
        }
        let resent: i32 = csink
            .events()
            .iter()
            .filter(|(t, _)| *t == AOO_BLOCK_RESENT_EVENT)
            .map(|(_, n)| n)
            .sum();
        assert!(
            dropped > 20 && resent > 0,
            "{name}: dropped {dropped}, C++ sink resent {resent}"
        );
        eprintln!(
            "{name}: {BLOCKS} blocks identical, {dropped} dropped, C++ sink counted {resent} resent"
        );
    }
}

/// Offset of the salt argument in a source packet, if it has one.
fn salt_offset(p: &[u8]) -> Option<usize> {
    let pad = |n: usize| (n + 4) & !3;
    let addr_len = pad(p.iter().position(|&b| b == 0)?);
    let tags_len = pad(p[addr_len..].iter().position(|&b| b == 0)?);
    let args = addr_len + tags_len;
    match message::parse(p).ok()? {
        Message::SinkFormat { .. } => Some(args + 8),
        Message::SinkData { .. } => Some(args + 4),
        Message::CompactData { .. } => Some(args),
        _ => None,
    }
}

fn masked(mut p: Vec<u8>) -> Vec<u8> {
    if let Some(o) = salt_offset(&p) {
        p[o..o + 4].fill(0);
    }
    p
}

#[test]
#[ignore]
fn source_packets_byte_identical() {
    let mut cases = matrix();
    cases.push((
        "pcm float32 dynamic",
        Case {
            codec: Codec::Pcm(PcmBitDepth::Float32),
            nch: 2,
            bs: 512,
            dynamic: true,
        },
    ));
    for (name, case) in cases {
        let (bs, nch) = (case.bs as usize, case.nch as usize);
        let mut cpp = Cpp::spawn(&case.cpp_source_args());
        let (mut src, mut src_audio) = Source::new(1, case.setup(), case.source_opts());
        src.set_format(case.format()).unwrap();
        src.add_sink(PEER, 7, 1);
        src.start();
        let (mut ours, mut theirs) = (Vec::new(), Vec::new());
        // 2.5 s: crosses a ping
        let blocks = (2.5 * SR as f64 / bs as f64) as usize;
        for k in 0..blocks {
            let t = time(k, case.bs);
            let x = input(k, bs, nch);
            cpp.process_source(t, &x);
            theirs.extend(cpp.send().into_iter().map(masked));
            src_audio.process(&x.iter().map(|c| &c[..]).collect::<Vec<_>>(), t);
            src.send(|_, p| ours.push(masked(p.to_vec())));
        }
        assert_eq!(ours.len(), theirs.len(), "{name}: packet count");
        for (i, (a, b)) in ours.iter().zip(&theirs).enumerate() {
            assert!(
                a == b,
                "{name}: packet {i} differs:\nrust {:?}\ncpp  {:?}",
                message::parse(a),
                message::parse(b)
            );
        }
        let pings = ours
            .iter()
            .filter(|p| p.starts_with(b"/aoo/sink/7/ping"))
            .count();
        assert!(pings >= 2, "{name}");
        eprintln!("{name}: {} packets byte-identical", ours.len());
    }
}

/// The Rust source restarts the stream (new salt, sequence 0, `/format`) instead of
/// overflowing at `i32::MAX`; a stock sink follows with a short re-buffer, over `/d`.
#[test]
#[ignore]
fn stock_sink_follows_sequence_wrap() {
    let case = matrix()[0].1;
    let (bs, nch) = (case.bs as usize, case.nch as usize);
    let (mut src, mut src_audio) = Source::new(1, case.setup(), case.source_opts());
    src.set_format(case.format()).unwrap();
    src.add_sink(PEER, 7, 1);
    src.start();
    let mut csink = Cpp::spawn(&case.cpp_sink_args());
    let (mut sink, mut audio) = case.rust_sink();
    let mut energy = Vec::new();
    let mut formats = 0;
    for k in 0..400 {
        let t = time(k, case.bs);
        if k == 100 {
            src.set_sequence_for_testing(i32::MAX - 50);
        }
        src_audio.process(
            &input(k, bs, nch).iter().map(|c| &c[..]).collect::<Vec<_>>(),
            t,
        );
        let mut pkts = Vec::new();
        src.send(|_, p| pkts.push(p.to_vec()));
        for p in &pkts {
            formats += p.starts_with(b"/aoo/sink/7/format") as usize;
            deliver(p, &mut csink, &mut sink, t);
        }
        let r = rust_play(&mut audio, t, bs, nch);
        let c = csink.process_sink(t, bs, nch);
        assert_same("wrap", k, &r, &c);
        energy.push(c[0].iter().map(|x| x * x).sum::<f32>());
    }
    assert_eq!(formats, 2, "start and restart");
    // the restart (block 151) costs one buffer of silence, then audio continues
    let silent = energy[110..].iter().filter(|&&e| e == 0.0).count();
    assert!(silent > 0 && silent < 20, "{silent} silent blocks");
    assert!(energy[300..].iter().all(|&e| e > 1.0));
}
