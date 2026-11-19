//! The audio-thread halves (`SourceAudio::process`, `SinkAudio::process`) never allocate or
//! free, including while a new per-source slot is installed and across stop/start.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::net::SocketAddr;

use aoo::codec::{Codec, Format, OPUS_APPLICATION_RESTRICTED_LOWDELAY, OPUS_SIGNAL_MUSIC, OpusOptions, PcmBitDepth};
use aoo::sink::{Sink, SinkOptions};
use aoo::source::{Source, SourceOptions};
use aoo::{Setup, TimeTag, message};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}

fn note() {
    let _ = ARMED.try_with(|a| {
        if a.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
}

struct Counting;

// SAFETY: forwards every call unchanged to the system allocator; the bookkeeping only touches
// const-initialised thread-locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note();
        // SAFETY: same contract as the caller's.
        unsafe { System.realloc(p, l, n) }
    }
}

#[global_allocator]
static ALLOC: Counting = Counting;

fn on_audio_thread<R>(f: impl FnOnce() -> R) -> R {
    ARMED.with(|a| a.set(true));
    let r = f();
    ARMED.with(|a| a.set(false));
    r
}

#[test]
fn audio_halves_do_not_allocate() {
    let addr = SocketAddr::from(([127, 0, 0, 1], 1));
    let setup = Setup {
        samplerate: 48000,
        blocksize: 256,
        nchannels: 2,
    };
    let (mut src, mut src_audio) = Source::new(1, setup, SourceOptions::default());
    let pcm = Format {
        nchannels: 2,
        samplerate: 48000,
        blocksize: 256,
        codec: Codec::Pcm(PcmBitDepth::Int16),
    };
    src.set_format(pcm).unwrap();
    src.add_sink(addr, 7, 1);
    src.start();
    let (mut sink, mut sink_audio) = Sink::new(7, setup, SinkOptions::default());
    let input = vec![0.25f32; 256];
    let mut out = [vec![0.0f32; 256], vec![0.0f32; 256]];
    for k in 0..600 {
        let t = TimeTag::from_secs(1000.0 + k as f64 * 256.0 / 48000.0);
        match k {
            200 => {
                let opus = Codec::Opus(OpusOptions {
                    bitrate: 128_000,
                    complexity: 5,
                    signal_type: OPUS_SIGNAL_MUSIC,
                    application_type: OPUS_APPLICATION_RESTRICTED_LOWDELAY,
                });
                src.set_format(Format { codec: opus, ..pcm }).unwrap();
            }
            350 => src.stop(),
            400 => src.start(),
            _ => {}
        }
        on_audio_thread(|| src_audio.process(&[&input, &input], t));
        let mut pkts = Vec::new();
        src.send(|_, p| pkts.push(p.to_vec()));
        for p in &pkts {
            sink.handle_message(addr, &message::parse(p).unwrap(), t);
        }
        sink.send(t, |_, _| {});
        let [l, r] = &mut out;
        on_audio_thread(|| sink_audio.process(&mut [&mut l[..], &mut r[..]], t));
    }
    assert!(out[0].iter().any(|&x| x != 0.0), "nothing played");
    assert_eq!(COUNT.with(|c| c.get()), 0, "allocations on the audio thread");
}
