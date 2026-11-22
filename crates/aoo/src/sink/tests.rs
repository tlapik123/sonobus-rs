use super::*;
use crate::VERSION;
use crate::message::parse;

const SALT: i32 = 77;
const BS: usize = 64;

fn addr(p: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], p))
}

/// Sink 5 at 48 kHz mono, one PCM float32 source 3 at `addr(1)` with 64-frame blocks.
struct H {
    sink: Sink,
    audio: SinkAudio,
    t: f64,
}

impl H {
    fn new(buffersize_ms: i32) -> Self {
        let setup = Setup {
            samplerate: 48000,
            blocksize: BS as i32,
            nchannels: 1,
        };
        let opts = SinkOptions {
            buffersize_ms,
            dynamic_resampling: false,
            ..SinkOptions::default()
        };
        let (sink, audio) = Sink::new(5, setup, opts);
        let mut h = Self {
            sink,
            audio,
            t: 1000.0,
        };
        h.format(SALT);
        h
    }

    fn now(&self) -> TimeTag {
        TimeTag::from_secs(self.t)
    }

    fn feed(&mut self, msg: &Message) -> bool {
        let now = self.now();
        self.sink.handle_message(addr(1), msg, now)
    }

    fn format(&mut self, salt: i32) {
        self.feed(&Message::SinkFormat {
            sink: 5,
            src: 3,
            version: VERSION,
            salt,
            nchannels: 1,
            samplerate: 48000,
            blocksize: BS as i32,
            codec: "pcm",
            options: &[0, 0, 0, 2],
            user_format: Some(b"layout"),
        });
    }

    fn block(&mut self, seq: i32, v: f32) {
        let bytes = block_bytes(&[v; BS]);
        self.feed(&data(seq, 0, 1, &bytes, 256));
    }

    fn play(&mut self) -> Vec<f32> {
        let mut buf = vec![9.0; BS];
        let now = self.now();
        self.audio.process(&mut [&mut buf], now);
        self.t += BS as f64 / 48000.0;
        buf
    }

    fn sent(&mut self) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        let now = self.now();
        self.sink.send(now, |a, p| {
            assert_eq!(a, addr(1));
            v.push(p.to_vec())
        });
        v
    }

    /// Counter and state events are collected by `send`.
    fn events(&mut self) -> Vec<SinkEventKind> {
        let now = self.now();
        self.sink.send(now, |_, _| {});
        std::iter::from_fn(|| self.sink.poll_event())
            .map(|e| e.kind)
            .collect()
    }

    fn src(&self) -> &SourceDesc {
        &self.sink.sources[0]
    }
}

fn block_bytes(samples: &[f32]) -> Vec<u8> {
    samples.iter().flat_map(|x| x.to_be_bytes()).collect()
}

fn data(seq: i32, frame: i32, nframes: i32, bytes: &[u8], total: i32) -> Message<'_> {
    data_from(3, SALT, seq, frame, nframes, bytes, total)
}

fn data_from(
    src: i32,
    salt: i32,
    seq: i32,
    frame: i32,
    nframes: i32,
    bytes: &[u8],
    total: i32,
) -> Message<'_> {
    Message::SinkData {
        sink: 5,
        src,
        salt,
        seq,
        samplerate: 48000.0,
        channel: 0,
        total_size: total,
        nframes,
        frame,
        data: bytes,
    }
}

fn requests(packets: &[Vec<u8>]) -> Vec<(i32, i32)> {
    packets
        .iter()
        .filter_map(|p| match parse(p).unwrap() {
            Message::SourceData { requests, salt, .. } => {
                assert_eq!(salt, SALT);
                Some(requests.iter().collect::<Vec<_>>())
            }
            _ => None,
        })
        .flatten()
        .collect()
}

#[test]
fn format_creates_source_and_reports() {
    let mut h = H::new(0);
    let ev = h.events();
    assert_eq!(ev[0], SinkEventKind::SourceAdded);
    match &ev[1] {
        SinkEventKind::FormatChanged {
            format,
            user_format,
        } => {
            assert_eq!((format.blocksize, format.codec_name()), (64, "pcm"));
            assert_eq!(user_format, b"layout");
        }
        e => panic!("{e:?}"),
    }
    // wrong sink id is not for us; wildcard is
    let m = Message::SinkPing {
        sink: 6,
        src: 3,
        tt: 1,
    };
    assert!(!h.feed(&m));
    let m = Message::SinkPing {
        sink: -1,
        src: 3,
        tt: 1,
    };
    assert!(h.feed(&m));
}

/// Frames arrive out of order and duplicated; the block is reassembled exactly.
#[test]
fn reassembly_out_of_order_and_duplicates() {
    let mut h = H::new(0); // one-block ring
    assert!(h.play().iter().all(|&x| x == 0.0)); // drains the prefilled silence
    let samples: Vec<f32> = (0..BS).map(|i| i as f32 / 100.0).collect();
    let bytes = block_bytes(&samples);
    let frames: Vec<&[u8]> = bytes.chunks(80).collect(); // 80, 80, 80, 16
    for i in [3, 1, 1, 0, 3] {
        h.feed(&data(0, i, 4, frames[i as usize], 256));
        assert_eq!(h.src().queue.len(), 1);
    }
    h.feed(&data(0, 2, 4, frames[2], 256));
    assert!(h.src().queue.is_empty());
    let out = h.play();
    // the first block after a reset is faded in
    for (i, (o, s)) in out.iter().zip(&samples).enumerate() {
        let gain = (0..i).fold(0.0f32, |g, _| g + 1.0 / 64.0);
        assert_eq!(*o, s * gain);
    }
    // frames that do not fit the announced block are rejected
    h.feed(&data(1, 1, 3, &bytes[..80], 100)); // 80..160 > 100
    h.feed(&data(1, 2, 3, &bytes[..120], 100)); // last frame larger than the block
    h.feed(&data(1, 5, 2, &bytes[..10], 20)); // frame out of range
    h.feed(&data(1, 0, 65, &bytes[..10], 650)); // more frames than the bitmap holds
    assert!(h.src().queue.is_empty());
}

/// A hole triggers whole-block resend requests, limited by interval and count; a late block
/// counts as resent, a stray old one as reordered.
#[test]
fn resend_requests_and_counters() {
    let mut h = H::new(10); // 8-block ring, prefilled: nothing is decoded until we play
    for seq in [0, 1, 3, 4] {
        h.block(seq, 0.1);
    }
    assert_eq!(requests(&h.sent()), [(2, -1)]);
    h.block(5, 0.1); // same time: interval not elapsed
    assert!(requests(&h.sent()).is_empty());
    for k in 0..6 {
        h.t += 0.011;
        h.block(6 + k, 0.1);
    }
    // 4 more attempts (limit 5 in total), then the block is given up
    assert_eq!(requests(&h.sent()), [(2, -1); 4]);
    h.block(2, 0.2); // arrives after all
    let ev = h.events();
    assert!(
        ev.contains(&SinkEventKind::BlocksResent { count: 1 }),
        "{ev:?}"
    );
    // incomplete blocks: missing frames are requested individually
    let bytes = block_bytes(&[0.3; BS]);
    h.t += 0.011;
    h.feed(&data(12, 0, 4, &bytes[..80], 256));
    h.feed(&data(12, 3, 4, &bytes[240..], 256));
    h.block(13, 0.1);
    h.block(14, 0.1);
    assert_eq!(requests(&h.sent()), [(12, 1), (12, 2)]);
}

#[test]
fn resend_batches_respect_packet_size() {
    let mut h = H::new(1000); // big queue
    h.block(0, 0.0);
    h.block(200, 0.0); // 199 missing, limited to 16 frames per check
    h.block(201, 0.0);
    let pkts = h.sent();
    assert_eq!(requests(&pkts).len(), 16);
    assert_eq!(pkts.len(), 1);
    let mut opts = h.sink.options();
    opts.resend_maxnumframes = 100;
    opts.packetsize = 512;
    h.sink.set_options(opts);
    h.t += 0.011;
    h.block(202, 0.0);
    let pkts = h.sent();
    assert_eq!(requests(&pkts).len(), 100);
    assert_eq!(pkts.len(), 3); // 46 pairs per packet at packet size 512
    assert!(pkts.iter().all(|p| p.len() <= 512));
}

/// Sequence numbers wrap from i32::MAX to i32::MIN without loss or reordering.
#[test]
fn sequence_wrap() {
    let mut h = H::new(0);
    h.play();
    let start = i32::MAX - 3;
    let mut played = Vec::new();
    for k in 0..8 {
        let seq = start.wrapping_add(k);
        h.block(seq, k as f32 + 1.0);
        played.push(h.play()[BS - 1]);
    }
    assert_eq!(played[1..], [2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
    assert!(played[0] < 1.0); // faded in
    let ev = h.events();
    assert!(
        !ev.iter().any(|e| matches!(
            e,
            SinkEventKind::BlocksLost { .. }
                | SinkEventKind::BlocksGap { .. }
                | SinkEventKind::BlocksReordered { .. }
        )),
        "{ev:?}"
    );
    // old blocks across the wrap are still "old"
    h.block(i32::MAX, 9.0);
    assert!(h.src().queue.is_empty());
}

#[test]
fn salt_change_requests_format() {
    let mut h = H::new(0);
    h.block(0, 0.1);
    assert!(h.sent().is_empty());
    let bytes = block_bytes(&[0.5; BS]);
    h.feed(&data_from(3, SALT + 1, 1, 0, 1, &bytes, 256));
    let p = h.sent();
    assert_eq!(
        parse(&p[0]).unwrap(),
        Message::SourceFormat {
            src: 3,
            sink: 5,
            version: VERSION
        }
    );
    // /d with an unknown salt: not ours, but the quiet source at that address is asked
    let d = Message::CompactData {
        salt: 999,
        seq: 0,
        samplerate: None,
        data: &bytes,
    };
    assert!(!h.feed(&d));
    assert!(h.sent().is_empty()); // source delivered data just now
    h.t += 1.0;
    assert!(!h.feed(&d));
    assert_eq!(h.sent().len(), 1);
    assert!(!h.feed(&d));
    assert!(h.sent().is_empty()); // rate limited
    // the new format makes the /d stream playable
    h.format(999);
    assert!(h.feed(&d));
    // unknown sources sending /data are created and asked for their format
    let m = data_from(4, SALT, 0, 0, 1, &bytes, 256);
    h.feed(&m);
    let p = h.sent();
    assert!(p.iter().any(|p| parse(p).unwrap()
        == Message::SourceFormat {
            src: 4,
            sink: 5,
            version: VERSION
        }));
    assert!(h.events().contains(&SinkEventKind::SourceAdded));
}

#[test]
fn gap_and_empty_blocks() {
    let mut h = H::new(10);
    h.block(0, 0.1);
    h.block(1, 0.1);
    h.block(100, 0.1); // > queue capacity ahead: reset
    let ev = h.events();
    assert!(
        ev.contains(&SinkEventKind::BlocksGap { count: 98 }),
        "{ev:?}"
    );
    assert!(
        ev.contains(&SinkEventKind::BlocksLost { count: 2 }),
        "{ev:?}"
    );
    assert_eq!(h.src().next, Some(100));
    // empty block (source xrun) skips ahead
    h.feed(&data(101, 0, 0, &[], 0));
    assert_eq!(h.src().next, Some(102));
    assert!(h.src().queue.is_empty()); // and flushes the queue, as the C++
}

#[test]
fn compact_data_and_samplerate_reuse() {
    let mut h = H::new(0);
    h.play();
    let bytes = block_bytes(&[0.25; BS]);
    let d = |seq| Message::CompactData {
        salt: SALT,
        seq,
        samplerate: None,
        data: &bytes,
    };
    assert!(h.feed(&d(0)));
    h.play();
    assert!(h.feed(&d(1)));
    assert_eq!(h.play(), [0.25; BS]);
}

#[test]
fn invite_repeats_until_format() {
    let setup = Setup {
        samplerate: 48000,
        blocksize: 64,
        nchannels: 1,
    };
    let (mut sink, _audio) = Sink::new(5, setup, SinkOptions::default());
    sink.invite_source(addr(1), 0);
    let count = |sink: &mut Sink, t: f64| {
        let mut n = 0;
        sink.send(TimeTag::from_secs(t), |_, p| {
            assert!(matches!(
                parse(p).unwrap(),
                Message::SourceInvite {
                    sink: 5,
                    flags: Some(1),
                    ..
                }
            ));
            n += 1;
        });
        n
    };
    assert_eq!(count(&mut sink, 10.0), 1);
    assert_eq!(count(&mut sink, 10.5), 0);
    assert_eq!(count(&mut sink, 11.0), 1);
    sink.uninvite_source(addr(1), 0);
    let mut msgs = Vec::new();
    sink.send(TimeTag::from_secs(12.0), |_, p| msgs.push(p.to_vec()));
    assert_eq!(
        parse(&msgs[0]).unwrap(),
        Message::SourceUninvite { src: 0, sink: 5 }
    );
    assert_eq!(count(&mut sink, 13.0), 0);
    // a format stops the repetition
    sink.invite_source(addr(1), 3);
    assert_eq!(count(&mut sink, 14.0), 1);
    sink.handle_message(
        addr(1),
        &Message::SinkFormat {
            sink: 5,
            src: 3,
            version: VERSION,
            salt: 1,
            nchannels: 1,
            samplerate: 48000,
            blocksize: 64,
            codec: "pcm",
            options: &[0, 0, 0, 0],
            user_format: None,
        },
        TimeTag::from_secs(14.1),
    );
    assert_eq!(count(&mut sink, 20.0), 0);
}

#[test]
fn ping_only_while_playing_and_state_events() {
    let mut h = H::new(0);
    h.feed(&Message::SinkPing {
        sink: 5,
        src: 3,
        tt: 42,
    });
    assert!(h.sent().is_empty());
    h.play(); // prefilled silence: playing
    h.block(0, 0.5); // decoded right away (ring has room), blocks are packet-driven
    h.play();
    assert!(
        h.events()
            .contains(&SinkEventKind::StateChanged { playing: true })
    );
    h.feed(&Message::SinkPing {
        sink: 5,
        src: 3,
        tt: 42,
    });
    let now = h.now();
    match parse(&h.sent()[0]).unwrap() {
        Message::SourcePing { tt1, tt2, .. } => assert_eq!((tt1, tt2), (42, now.0)),
        m => panic!("{m:?}"),
    }
    match h.events().last().unwrap() {
        SinkEventKind::Ping { tt1, tt2 } => assert_eq!((*tt1, *tt2), (TimeTag(42), now)),
        e => panic!("{e:?}"),
    }
    // nothing more arrives: underrun, stop event
    h.play();
    h.play();
    h.sent();
    assert!(
        h.events()
            .contains(&SinkEventKind::StateChanged { playing: false })
    );
}

/// No buffer size and no announced format makes a sink allocate more than its caps.
#[test]
fn buffer_size_is_capped() {
    let mut h = H::new(1_000_000_000);
    assert_eq!(h.sink.options().buffersize_ms, MAX_BUFFER_MS);
    let ring = |h: &H| {
        h.src()
            .samples
            .as_ref()
            .map_or(0, |s| s.buffer().capacity())
    };
    // 10 s at 48 kHz mono, rounded up to whole 64-frame blocks
    assert_eq!(ring(&h), 480_000);
    h.sink.set_options(SinkOptions {
        buffersize_ms: i32::MAX,
        ..h.sink.options()
    });
    assert_eq!(h.sink.options().buffersize_ms, MAX_BUFFER_MS);
    // the widest valid format: 255 channels at 768 kHz
    h.feed(&Message::SinkFormat {
        sink: 5,
        src: 3,
        version: VERSION,
        salt: SALT + 1,
        nchannels: 255,
        samplerate: 768_000,
        blocksize: BS as i32,
        codec: "pcm",
        options: &[0, 0, 0, 2],
        user_format: None,
    });
    assert!(
        ring(&h) <= MAX_RING_SAMPLES && ring(&h) >= 255 * BS,
        "{}",
        ring(&h)
    );
}

#[test]
fn source_cap() {
    let mut h = H::new(50);
    h.sink.set_max_sources(2);
    for src in [4, 5, 6] {
        h.feed(&Message::SinkFormat {
            sink: 5,
            src,
            version: VERSION,
            salt: SALT,
            nchannels: 1,
            samplerate: 48000,
            blocksize: BS as i32,
            codec: "pcm",
            options: &[0, 0, 0, 2],
            user_format: None,
        });
    }
    let ids: Vec<i32> = h.sink.sources.iter().map(|s| s.id).collect();
    assert_eq!(ids, [3, 4]);
}

/// `reset` drops buffered audio: the next blocks are the refilled silence.
#[test]
fn reset_drops_buffered_audio() {
    let mut h = H::new(0); // one-block ring
    assert!(h.play().iter().all(|&x| x == 0.0));
    h.block(0, 0.5);
    h.sink.reset();
    assert!(h.play().iter().all(|&x| x == 0.0), "stale block dropped");
    // and it plays on with new blocks
    h.block(1, 0.5);
    h.block(2, 0.5);
    let mut heard = false;
    for _ in 0..4 {
        heard |= h.play().iter().any(|&x| x != 0.0);
    }
    assert!(heard);
}
