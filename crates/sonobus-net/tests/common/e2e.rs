//! Glue for end-to-end runs over the real transport: a [`Source`] streams a test signal
//! through `TxStream` to every subscriber's `PeerTx` (NACK resends, REPORT → `adapt` →
//! subscription changes), a [`Listener`] feeds one `RxStream` from a `Net` sink and plays it on
//! a wall-clock "device". A lossy shim (drop / delay) sits in front of the listener's
//! `RxStream`. Both are stepped from one loop thread; nothing here runs on an audio thread.

use std::collections::{HashMap, VecDeque};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use bytes::Bytes;
use sonobus_net::adapt::{Adapt, Decision};
use sonobus_net::stream::{
    Device, RxAudio, RxEvent, RxOptions, RxStream, StreamFormat, TxAudio, TxOptions, TxStream,
};
use sonobus_net::wire::{self, Datagram, Frame};
use sonobus_net::{ConnId, Net, PeerInfo, PeerTx, Sink};

pub type Inbox = Receiver<(ConnId, Bytes)>;

/// A sink factory that forwards every datagram (tagged with its connection) to the returned
/// channel.
pub fn channel_sinks() -> (impl FnMut(ConnId, &PeerInfo) -> Sink + Send + 'static, Inbox) {
    let (tx, rx) = channel();
    let f = move |conn: ConnId, _: &PeerInfo| {
        let tx = tx.clone();
        Box::new(move |d: Bytes| {
            let _ = tx.send((conn, d));
        }) as Sink
    };
    (f, rx)
}

pub fn sine(n: usize, c: usize) -> f32 {
    (n as f32 * 0.01 + c as f32).sin() * 0.8
}

pub fn period(d: &Device) -> Duration {
    Duration::from_secs_f64(d.block_size as f64 / f64::from(d.sample_rate))
}

struct Sub {
    peer: PeerTx,
    adapt: Adapt,
    decision: Decision,
    rtt_at: Instant,
}

/// One outgoing stream (id 0) of `signal`, device callbacks on the wall clock.
pub struct Source {
    pub tx: TxStream,
    audio: TxAudio,
    dev: Device,
    frames: usize,
    next: Option<Instant>,
    signal: fn(usize, usize) -> f32,
    subs: HashMap<ConnId, Sub>,
    /// Adaptation results applied, in order.
    pub changes: Vec<(ConnId, Decision)>,
    pub nacks: usize,
    pub reports: Vec<(ConnId, wire::Report)>,
    /// Callbacks the audio ring refused (network side behind).
    pub dropped_callbacks: usize,
}

impl Source {
    pub fn new(dev: Device, format: StreamFormat, signal: fn(usize, usize) -> f32) -> Self {
        let opts = TxOptions {
            dynamic_resampling: false,
            ..TxOptions::default()
        };
        let (mut tx, audio) = TxStream::new(0, dev, format, opts).expect("format");
        tx.take_format_change();
        Self {
            tx,
            audio,
            dev,
            frames: 0,
            next: None,
            signal,
            subs: HashMap::new(),
            changes: vec![],
            nacks: 0,
            reports: vec![],
            dropped_callbacks: 0,
        }
    }

    /// A new subscriber: announce the format, subscribe at the best level without redundancy.
    pub fn add(&mut self, net: &Net, conn: ConnId, peer: PeerTx, now: Instant) {
        let _ = net.send_control(conn, Frame::Format(self.tx.wire_format()));
        self.tx.subscribe(conn, 0, 0);
        let levels = vec![1; self.tx.format().encodings.len()];
        self.subs.insert(
            conn,
            Sub {
                peer,
                adapt: Adapt::new(now, levels),
                decision: Decision::default(),
                rtt_at: now,
            },
        );
    }

    pub fn remove(&mut self, conn: ConnId) {
        self.tx.unsubscribe(conn);
        self.subs.remove(&conn);
    }

    /// Current (level, redundancy) of a subscriber.
    pub fn decision(&self, conn: ConnId) -> Option<Decision> {
        self.subs.get(&conn).map(|s| s.decision)
    }

    /// Run the device callbacks due at `now` and send what they produced.
    pub fn step(&mut self, now: Instant) {
        let p = period(&self.dev);
        let next = self.next.get_or_insert(now);
        let mut chans = vec![vec![0.0f32; self.dev.block_size]; self.dev.channels];
        while *next <= now {
            for (c, ch) in chans.iter_mut().enumerate() {
                for (i, x) in ch.iter_mut().enumerate() {
                    *x = (self.signal)(self.frames + i, c);
                }
            }
            let refs: Vec<&[f32]> = chans.iter().map(|c| &c[..]).collect();
            // Pump after every callback, as a network thread running alongside would: a
            // catch-up burst after a stall must not overflow the audio ring.
            self.dropped_callbacks += usize::from(!self.audio.process(&refs, *next));
            self.frames += self.dev.block_size;
            *next += p;
            let subs = &self.subs;
            self.tx.pump(|id, b| {
                if let Some(s) = subs.get(&id) {
                    let _ = s.peer.send(b);
                }
            });
        }
        for s in self.subs.values_mut() {
            if now.saturating_duration_since(s.rtt_at) >= Duration::from_millis(100) {
                s.rtt_at = now;
                if let Some(p) = s.peer.path() {
                    s.adapt.on_rtt(now, p.rtt);
                }
            }
        }
    }

    /// A datagram from subscriber `conn` (NACK, REPORT; anything else is ignored).
    pub fn on_datagram(&mut self, conn: ConnId, now: Instant, d: &[u8]) {
        let Some(s) = self.subs.get_mut(&conn) else {
            return;
        };
        match Datagram::decode(d) {
            Ok(Datagram::Nack(n)) => {
                self.nacks += 1;
                let peer = &s.peer;
                self.tx.handle_nack(conn, &n, |_, b| {
                    let _ = peer.send(b);
                });
            }
            Ok(Datagram::Report(r)) => {
                self.reports.push((conn, r));
                let d = s.adapt.on_report(now, &r);
                if d != s.decision {
                    s.decision = d;
                    self.tx.subscribe(conn, d.level as u8, d.redundancy);
                    self.tx.set_expected_loss(s.adapt.opus_loss_percent());
                    self.changes.push((conn, d));
                }
            }
            _ => {}
        }
    }
}

/// Drops the AUDIO datagrams for which it returns true (lossy shim).
pub type DropFn = Box<dyn FnMut(&wire::Audio) -> bool + Send>;

/// One incoming stream (id 0) from `host`, played on a wall-clock device.
pub struct Listener {
    pub rx: RxStream,
    audio: RxAudio,
    dev: Device,
    next: Option<Instant>,
    host: PeerTx,
    rtt: Duration,
    rtt_at: Option<Instant>,
    /// Keep the played output (per channel).
    pub record: bool,
    pub out: Vec<Vec<f32>>,
    pub events: Vec<(Instant, RxEvent)>,
    pub nacks: usize,
    /// Shim: drop decision and extra one-way delay before the `RxStream`.
    pub drop: DropFn,
    pub delay: Duration,
    delayed: VecDeque<(Instant, Bytes)>,
    pub dropped: usize,
    /// AUDIO datagrams that carried redundant blocks.
    pub with_red: usize,
}

impl Listener {
    pub fn new(dev: Device, opts: RxOptions, host: PeerTx) -> Self {
        let (rx, audio) = RxStream::new(0, dev, opts);
        Self {
            rx,
            audio,
            dev,
            next: None,
            host,
            rtt: Duration::from_millis(1),
            rtt_at: None,
            record: false,
            out: vec![vec![]; dev.channels],
            events: vec![],
            nacks: 0,
            drop: Box::new(|_| false),
            delay: Duration::ZERO,
            delayed: VecDeque::new(),
            dropped: 0,
            with_red: 0,
        }
    }

    pub fn host(&self) -> &PeerTx {
        &self.host
    }

    pub fn on_control(&mut self, f: &Frame) {
        if let Frame::Format(f) = f {
            self.rx.set_format(f).expect("format");
        }
    }

    pub fn on_datagram(&mut self, now: Instant, d: Bytes) {
        if let Ok(Datagram::Audio(a)) = Datagram::decode(&d)
            && (self.drop)(&a)
        {
            self.dropped += 1;
            return;
        }
        self.delayed.push_back((now + self.delay, d));
    }

    /// Release due datagrams, run due device callbacks and timer work, send NACK/REPORT.
    pub fn step(&mut self, now: Instant) {
        while let Some((t, _)) = self.delayed.front()
            && *t <= now
        {
            let (t, d) = self.delayed.pop_front().unwrap();
            if let Ok(Datagram::Audio(a)) = Datagram::decode(&d) {
                self.with_red += usize::from(!a.redundant.is_empty());
                self.rx.handle_audio(t, &a);
            }
        }
        if self.rtt_at.is_none_or(|t| now.saturating_duration_since(t) >= Duration::from_millis(100))
        {
            self.rtt_at = Some(now);
            if let Some(p) = self.host.path() {
                self.rtt = p.rtt + self.delay;
            }
        }
        let p = period(&self.dev);
        let next = self.next.get_or_insert(now);
        let mut bufs = vec![vec![0.0f32; self.dev.block_size]; self.dev.channels];
        while *next <= now {
            let mut refs: Vec<&mut [f32]> = bufs.iter_mut().map(|b| &mut b[..]).collect();
            self.audio.process(&mut refs, *next);
            if self.record {
                for (o, b) in self.out.iter_mut().zip(&bufs) {
                    o.extend_from_slice(b);
                }
            }
            self.rx.tick(*next, self.rtt);
            *next += p;
        }
        while let Some(b) = self.rx.poll_transmit(now) {
            if matches!(Datagram::decode(&b), Ok(Datagram::Nack(_))) {
                self.nacks += 1;
            }
            let _ = self.host.send(b);
        }
        while let Some(e) = self.rx.poll_event() {
            self.events.push((now, e));
        }
    }

    /// Sum of an event's counts since `after`.
    pub fn count(&self, after: Instant, f: impl Fn(&RxEvent) -> Option<u32>) -> u32 {
        self.events
            .iter()
            .filter(|(t, _)| *t >= after)
            .filter_map(|(_, e)| f(e))
            .sum()
    }
}

pub fn lost(e: &RxEvent) -> Option<u32> {
    match e {
        RxEvent::BlocksLost { count } => Some(*count),
        _ => None,
    }
}

pub fn resent(e: &RxEvent) -> Option<u32> {
    match e {
        RxEvent::BlocksResent { count } => Some(*count),
        _ => None,
    }
}

pub fn recovered(e: &RxEvent) -> Option<u32> {
    match e {
        RxEvent::BlocksRecovered { count } => Some(*count),
        _ => None,
    }
}

/// Find `d` with `out[d + n] == input(n)` over 64 samples starting at `from`.
pub fn find_delay(out: &[f32], input: impl Fn(usize) -> f32, from: usize, max: usize) -> Option<usize> {
    (0..max).find(|&d| (from..from + 64).all(|n| out.get(d + n) == Some(&input(n))))
}
