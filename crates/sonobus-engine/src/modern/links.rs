//! Audio-thread halves of the protocol v2 streams, by mixer slot: each peer's receive stream
//! (and its private send stream, when it gets audio the shared stream does not carry), plus the
//! one shared send stream every other peer subscribes to (encoded once on the network side).

use std::time::Instant;

use sonobus_net::stream::{RxAudio, TxAudio};

use crate::mixer::{Garbage, MAX_CHANNELS, MAX_PEERS};

#[derive(Default)]
pub(crate) struct Link {
    pub rx: Option<Box<RxAudio>>,
    pub tx: Option<Box<TxAudio>>,
}

pub(crate) enum LinkCmd {
    Add(usize, Box<Link>),
    Remove(usize),
    /// New receive half (the peer switched to another of its streams).
    Rx(usize, Box<RxAudio>),
    /// The peer's private send half (`None`: it takes the shared stream again).
    Tx(usize, Option<Box<TxAudio>>),
    /// The shared send stream's audio half (`None`: no session, or a broadcast listener).
    Main(Option<Box<TxAudio>>),
}

pub(crate) struct Links {
    slots: Vec<Option<Box<Link>>>,
    main: Option<Box<TxAudio>>,
}

impl Links {
    pub fn new() -> Self {
        Self {
            slots: (0..MAX_PEERS).map(|_| None).collect(),
            main: None,
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
            LinkCmd::Rx(slot, rx) => match self.slots.get_mut(slot).and_then(Option::as_mut) {
                Some(l) => {
                    if let Some(old) = l.rx.replace(rx) {
                        drop_later(old);
                    }
                }
                None => drop_later(rx),
            },
            LinkCmd::Tx(slot, tx) => match self.slots.get_mut(slot).and_then(Option::as_mut) {
                Some(l) => {
                    if let Some(old) = std::mem::replace(&mut l.tx, tx) {
                        drop_later(old);
                    }
                }
                None => {
                    if let Some(tx) = tx {
                        drop_later(tx);
                    }
                }
            },
            LinkCmd::Main(tx) => {
                if let Some(old) = std::mem::replace(&mut self.main, tx) {
                    drop_later(old);
                }
            }
        }
    }

    /// Play the peer's stream into `buf` (all [`MAX_CHANNELS`] channels, `n` frames).
    pub fn receive(&mut self, slot: usize, buf: &mut [Vec<f32>], n: usize, now: Instant) {
        let Some(Some(l)) = self.slots.get_mut(slot) else {
            return;
        };
        let Some(rx) = &mut l.rx else { return };
        let mut it = buf.iter_mut();
        let mut chans: [&mut [f32]; MAX_CHANNELS] =
            std::array::from_fn(|_| it.next().map_or(&mut [][..], |c| &mut c[..n]));
        rx.process(&mut chans, now);
    }

    /// Queue `n` frames of the shared send mix. Returns whether anything was queued.
    pub fn send_main(&mut self, mix: &[Vec<f32>], n: usize, now: Instant) -> bool {
        match &mut self.main {
            Some(tx) => process(tx, mix, n, now),
            None => false,
        }
    }

    /// The peer's private send stream, if it has one.
    pub fn has_private(&self, slot: usize) -> bool {
        matches!(self.slots.get(slot), Some(Some(l)) if l.tx.is_some())
    }

    /// Queue `n` frames of the peer's own send mix into its private stream.
    pub fn send(&mut self, slot: usize, mix: &[Vec<f32>], n: usize, now: Instant) -> bool {
        match self.slots.get_mut(slot).and_then(Option::as_mut) {
            Some(l) => l.tx.as_mut().is_some_and(|tx| process(tx, mix, n, now)),
            None => false,
        }
    }
}

fn process(tx: &mut TxAudio, mix: &[Vec<f32>], n: usize, now: Instant) -> bool {
    let chans: [&[f32]; MAX_CHANNELS] =
        std::array::from_fn(|i| mix.get(i).map_or(&[][..], |c| &c[..n]));
    tx.process(&chans, now)
}
