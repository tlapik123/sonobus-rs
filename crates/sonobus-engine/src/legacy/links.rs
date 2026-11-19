//! Audio-thread halves of the per-peer AOO sinks and sources, indexed by mixer slot: the main
//! pair, the echo service pair and the latency-test pair (all mono, P.cpp:8027-8063).

use std::sync::Arc;
use std::sync::atomic::Ordering;

use aoo::TimeTag;
use aoo::sink::SinkAudio;
use aoo::source::SourceAudio;

use crate::latency::{LatencyMeasurer, LatencyShared};
use crate::mixer::{Garbage, MAX_CHANNELS, MAX_PEERS};

pub(crate) struct PeerLink {
    pub sink: Box<SinkAudio>,
    pub source: Box<SourceAudio>,
    pub echo_sink: SinkAudio,
    pub echo_source: SourceAudio,
    pub lat_sink: SinkAudio,
    pub lat_source: SourceAudio,
    pub measurer: LatencyMeasurer,
    pub lat: Arc<LatencyShared>,
    pub lat_seen: u32,
    /// One block, mono.
    pub scratch: Vec<f32>,
}

pub(crate) enum LinkCmd {
    Add(usize, Box<PeerLink>),
    Remove(usize),
    /// New sink audio half after `Sink::setup` (receive channel count changed).
    Sink(usize, Box<SinkAudio>),
    /// New source audio half after `Source::setup` (send channel count changed).
    Source(usize, Box<SourceAudio>),
}

pub(crate) struct Links {
    slots: Vec<Option<Box<PeerLink>>>,
}

impl Links {
    pub fn new() -> Self {
        Self {
            slots: (0..MAX_PEERS).map(|_| None).collect(),
        }
    }

    /// Produces at most one [`Garbage`] box.
    pub fn apply(&mut self, cmd: LinkCmd, drop_later: &mut dyn FnMut(Garbage)) {
        match cmd {
            LinkCmd::Add(slot, l) => {
                if let Some(old) = self.slots.get_mut(slot).and_then(|s| s.replace(l)) {
                    drop_later(old);
                }
            }
            LinkCmd::Remove(slot) => {
                if let Some(old) = self.slots.get_mut(slot).and_then(Option::take) {
                    drop_later(old);
                }
            }
            LinkCmd::Sink(slot, s) => match self.slots.get_mut(slot).and_then(Option::as_mut) {
                Some(l) => {
                    let old: Box<SinkAudio> = std::mem::replace(&mut l.sink, s);
                    drop_later(old);
                }
                None => drop_later(s),
            },
            LinkCmd::Source(slot, s) => match self.slots.get_mut(slot).and_then(Option::as_mut) {
                Some(l) => {
                    let old: Box<SourceAudio> = std::mem::replace(&mut l.source, s);
                    drop_later(old);
                }
                None => drop_later(s),
            },
        }
    }

    /// Decode the peer's stream into `buf` (all [`MAX_CHANNELS`] channels, `n` frames).
    pub fn receive(&mut self, slot: usize, buf: &mut [Vec<f32>], n: usize, now: TimeTag) {
        let Some(Some(l)) = self.slots.get_mut(slot) else {
            return;
        };
        let mut it = buf.iter_mut();
        let mut chans: [&mut [f32]; MAX_CHANNELS] =
            std::array::from_fn(|_| it.next().map_or(&mut [][..], |c| &mut c[..n]));
        l.sink.process(&mut chans, now);
    }

    /// The echo service (whatever reaches our echo sink goes straight back out of our echo
    /// source) and, while our latency test runs, the measurer between our latency sink and
    /// source. `hear` (`hearlatencytest`): what comes back goes to output channel 1, the
    /// pulse to channel 0. Returns whether anything was queued.
    pub fn echo_and_latency(
        &mut self,
        slot: usize,
        n: usize,
        now: TimeTag,
        sample_rate: i32,
        hear: Option<&mut [Vec<f32>]>,
    ) -> bool {
        let Some(Some(l)) = self.slots.get_mut(slot) else {
            return false;
        };
        let buf = &mut l.scratch[..n];
        let mut queued = false;
        if l.echo_sink.process(&mut [&mut *buf], now) {
            queued |= l.echo_source.process(&[&*buf], now);
        }
        if !l.lat.active.load(Ordering::Acquire) {
            return queued;
        }
        let restart = l.lat.restart.load(Ordering::Acquire);
        if restart != l.lat_seen {
            l.lat_seen = restart;
            l.measurer.start_peer_test();
        }
        l.lat_sink.process(&mut [&mut *buf], now);
        let mut hear = hear;
        if let Some(h) = hear.as_deref_mut() {
            let ch = if h.len() > 1 { 1 } else { 0 };
            for (o, x) in h[ch][..n].iter_mut().zip(buf.iter()) {
                *o += x;
            }
        }
        l.measurer.process_input(buf, sample_rate);
        l.measurer.process_output(buf);
        if let Some(h) = hear {
            for (o, x) in h[0][..n].iter_mut().zip(buf.iter()) {
                *o += x;
            }
        }
        queued |= l.lat_source.process(&[&*buf], now);
        l.lat
            .latency_bits
            .store(l.measurer.latency_ms.to_bits(), Ordering::Relaxed);
        l.lat.state.store(l.measurer.state, Ordering::Release);
        l.lat.generation.store(l.lat_seen, Ordering::Release);
        queued
    }

    /// Queue `n` frames of the send mix for the peer (the source takes as many channels as
    /// its format has). Returns whether anything was queued.
    pub fn send(&mut self, slot: usize, mix: &[Vec<f32>], n: usize, now: TimeTag) -> bool {
        let Some(Some(l)) = self.slots.get_mut(slot) else {
            return false;
        };
        let chans: [&[f32]; MAX_CHANNELS] =
            std::array::from_fn(|i| mix.get(i).map_or(&[][..], |c| &c[..n]));
        l.source.process(&chans, now)
    }
}
