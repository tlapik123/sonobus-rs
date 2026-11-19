//! The audio halves (`TxAudio::process`, `RxAudio::process`) never allocate or free, also
//! across format changes, buffer resizes and flushes. Plus a seeded hostile-input fuzz of
//! `RxStream`: no panic, and the live heap stays bounded.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::time::{Duration, Instant};

use aoo::codec::PcmBitDepth;
use sonobus_net::stream::{
    Device, Encoding, RxOptions, RxStream, StreamFormat, TxOptions, TxStream,
};
use sonobus_net::wire::{self, AudioHeader, Datagram, Format};

thread_local! {
    static ARMED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
    /// Live bytes allocated by this thread, and the high-water mark since the last reset.
    static LIVE: Cell<isize> = const { Cell::new(0) };
    static PEAK: Cell<isize> = const { Cell::new(0) };
}

fn note(delta: isize) {
    let _ = ARMED.try_with(|a| {
        if a.get() {
            COUNT.with(|c| c.set(c.get() + 1));
        }
    });
    let _ = LIVE.try_with(|l| {
        let v = l.get() + delta;
        l.set(v);
        PEAK.with(|p| p.set(p.get().max(v)));
    });
}

struct Counting;

// SAFETY: forwards every call unchanged to the system allocator; the bookkeeping only touches
// const-initialised thread-locals, which never allocate.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        note(l.size() as isize);
        // SAFETY: same contract as the caller's.
        unsafe { System.alloc(l) }
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        note(-(l.size() as isize));
        // SAFETY: same contract as the caller's.
        unsafe { System.dealloc(p, l) }
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        note(n as isize - l.size() as isize);
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

fn pcm(block: u16, depth: PcmBitDepth) -> StreamFormat {
    StreamFormat {
        sample_rate: 48000,
        block_size: block,
        channels: 2,
        voice: false,
        encodings: vec![Encoding::Pcm(depth)],
    }
}

#[test]
fn audio_halves_do_not_allocate() {
    let dev = Device {
        sample_rate: 48000,
        block_size: 256,
        channels: 2,
    };
    let (mut tx, mut tx_audio) =
        TxStream::new(0, dev, pcm(256, PcmBitDepth::Int16), TxOptions::default()).unwrap();
    tx.subscribe(1, 0, 1);
    let (mut rx, mut rx_audio) = RxStream::new(0, dev, RxOptions::default());
    let input = vec![0.25f32; 256];
    let mut out = [vec![0.0f32; 256], vec![0.0f32; 256]];
    let base = Instant::now();
    let mut played = 0;
    for k in 0..800u64 {
        let now = base + Duration::from_micros(k * 256 * 1_000_000 / 48000);
        match k {
            200 => {
                let opus = StreamFormat {
                    encodings: vec![Encoding::Opus {
                        bitrate: 128_000,
                        complexity: 5,
                    }],
                    ..pcm(240, PcmBitDepth::Int16)
                };
                tx.set_format(opus).unwrap();
            }
            400 => rx.set_buffer_ms(40.0),
            500 => rx.flush(),
            600 => tx.set_format(pcm(128, PcmBitDepth::Float32)).unwrap(),
            _ => {}
        }
        on_audio_thread(|| tx_audio.process(&[&input, &input], now));
        if let Some(f) = tx.take_format_change() {
            rx.set_format(&f).unwrap();
        }
        let mut dgs = Vec::new();
        tx.pump(|_, b| dgs.push(b));
        for b in &dgs {
            let Ok(Datagram::Audio(a)) = Datagram::decode(b) else {
                panic!()
            };
            rx.handle_audio(now, &a);
        }
        rx.tick(now, Duration::from_millis(5));
        while rx.poll_transmit(now).is_some() {}
        while rx.poll_event().is_some() {}
        let [l, r] = &mut out;
        if on_audio_thread(|| rx_audio.process(&mut [&mut l[..], &mut r[..]], now)) {
            played += 1;
        }
    }
    assert!(played > 600, "played {played}");
    assert!(out[0].iter().any(|&x| x != 0.0), "nothing played");
    assert_eq!(
        COUNT.with(|c| c.get()),
        0,
        "allocations on the audio thread"
    );
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
    fn bytes(&mut self, n: usize) -> Vec<u8> {
        (0..n).map(|_| self.next() as u8).collect()
    }
    /// Fewer than `max` random bytes.
    fn some_bytes(&mut self, max: u64) -> Vec<u8> {
        let n = self.below(max) as usize;
        self.bytes(n)
    }
}

/// A random, sometimes valid FORMAT for stream 0.
fn random_format(rng: &mut Rng) -> Format {
    let codec = rng.below(3) as u8;
    let n = 1 + rng.below(3) as usize;
    let mut options = vec![rng.below(2) as u8];
    for _ in 0..n {
        if codec == 1 {
            options.extend_from_slice(&(rng.below(600_000) as u32).to_le_bytes());
            options.push(rng.below(11) as u8);
        } else {
            options.push(rng.below(5) as u8);
        }
    }
    if rng.below(4) == 0 {
        options = rng.some_bytes(20);
    }
    let rates = [8000, 44100, 48000, 96000, 192_000, 0, u32::MAX];
    let blocks = [16, 120, 240, 256, 480, 960, 8192, 0, 9000, u16::MAX];
    Format {
        stream: 0,
        generation: rng.below(32) as u8,
        codec,
        sample_rate: rates[rng.below(rates.len() as u64) as usize],
        block_size: blocks[rng.below(blocks.len() as u64) as usize],
        channels: [1, 2, 8, 64, 65, 255, 0][rng.below(7) as usize],
        options,
        encodings: (0..n as u8).collect(),
    }
}

/// Seeded hostile datagrams (garbage, sequence jumps, absurd fragment counts and redundancy
/// lengths, generation flapping) and formats into one `RxStream`: no panic, bounded heap.
#[test]
fn hostile_input_bounded() {
    let dev = Device {
        sample_rate: 48000,
        block_size: 256,
        channels: 2,
    };
    let (mut rx, mut rx_audio) = RxStream::new(
        0,
        dev,
        RxOptions {
            buffer_ms: 100.0,
            ..RxOptions::default()
        },
    );
    let mut rng = Rng(0x5eed);
    let base = Instant::now();
    let mut seq: u32 = 0;
    let mut gen_ = 0u8;
    let mut buf = Vec::new();
    let mut out = [vec![0.0f32; 256], vec![0.0f32; 256]];
    let mut accepted = 0;
    let mut events = 0;
    PEAK.with(|p| p.set(LIVE.with(|l| l.get())));
    let start = LIVE.with(|l| l.get());
    for i in 0..300_000u64 {
        let now = base + Duration::from_micros(i * 50);
        if i % 3000 == 0 {
            let f = random_format(&mut rng);
            if rx.set_format(&f).is_ok() {
                gen_ = f.generation;
                accepted += 1;
            }
        }
        // mostly near the cursor, sometimes a jump; generation mostly current
        seq = match rng.below(100) {
            0 => rng.next() as u32,
            1..=5 => seq.wrapping_sub(rng.below(100) as u32),
            _ => seq.wrapping_add(rng.below(3) as u32),
        };
        let h = AudioHeader {
            stream: if rng.below(50) == 0 {
                rng.below(8) as u8
            } else {
                0
            },
            generation: if rng.below(20) == 0 {
                rng.below(32) as u8
            } else {
                gen_
            },
            drift: rng.next() as i16,
            seq,
            discontinuity: rng.below(200) == 0,
        };
        match rng.below(10) {
            0 => buf = rng.some_bytes(1200),
            1..=3 => {
                // fragment of a claimed huge block
                let n = 1 + rng.below(255) as u8;
                let idx = rng.below(u64::from(n)) as u8;
                let len = if idx + 1 == n {
                    rng.below(1141) as usize
                } else {
                    wire::FRAG_PAYLOAD
                };
                wire::encode_audio(&h, &[], &[], &mut buf).unwrap();
                buf[0] |= wire::FLAG_FRAGMENT;
                buf.extend_from_slice(&[idx, n]);
                buf.extend_from_slice(&rng.bytes(len));
            }
            _ => {
                let k = rng.below(6) as usize;
                let red: Vec<Vec<u8>> = (0..k).map(|_| rng.some_bytes(300)).collect();
                let refs: Vec<&[u8]> = red.iter().map(|r| &r[..]).collect();
                let payload = rng.some_bytes(1000);
                if wire::encode_audio(&h, &refs, &payload, &mut buf).is_err() {
                    wire::encode_audio(&h, &[], &payload[..payload.len().min(900)], &mut buf)
                        .unwrap();
                }
            }
        }
        if let Ok(Datagram::Audio(a)) = Datagram::decode(&buf) {
            rx.handle_audio(now, &a);
        }
        if i % 4 == 0 {
            rx.tick(now, Duration::from_millis(rng.below(200)));
            while rx.poll_transmit(now).is_some() {}
            while rx.poll_event().is_some() {
                events += 1;
            }
            let [l, r] = &mut out;
            rx_audio.process(&mut [&mut l[..], &mut r[..]], now);
        }
        if i % 1000 == 0 {
            // a rogue setter flapping the buffer
            rx.set_buffer_ms(rng.below(3000) as f32);
        }
    }
    let peak = PEAK.with(|p| p.get()) - start;
    println!(
        "accepted formats: {accepted}, events: {events}, heap high-water: {} KiB",
        peak / 1024
    );
    assert!(
        accepted > 5 && events > 1000,
        "{accepted} formats, {events} events"
    );
    // queue budget (8 MiB) + reassembler + rings + codec state for the largest legal format
    assert!(peak < 48 << 20, "heap high-water {} MiB", peak >> 20);
}
