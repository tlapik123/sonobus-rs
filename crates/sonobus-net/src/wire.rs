//! Protocol v2 wire formats (sans-IO): QUIC datagrams, block fragmentation/reassembly and the
//! control-stream frames. All integers are little-endian. Decoders never panic: any truncated,
//! out-of-range or over-long input is an [`Error`].
//!
//! Datagram header, shared by every kind: byte 0 = kind (low 4 bits) | flags (high 4 bits).
//! AUDIO continues with byte 1 = stream id (low 3 bits) | format generation (high 5 bits),
//! bytes 2-3 = drift (i16, sender DLL deviation in 1/8 ppm), bytes 4-7 = sequence (u32, block
//! index). The other kinds carry no stream byte (their bodies name the stream where needed)
//! and must have all flags clear.

use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::time::{Duration, Instant};

/// Largest datagram this crate ever produces (minimum-MTU QUIC datagram is 1162 B).
pub const MAX_DATAGRAM: usize = 1150;
/// AUDIO header without the optional fragment/redundancy fields.
pub const AUDIO_HEADER: usize = 8;
/// Payload bytes of every fragment but the last: one fragment is always a full
/// [`MAX_DATAGRAM`], so every receiver gets byte-identical datagrams.
pub const FRAG_PAYLOAD: usize = MAX_DATAGRAM - AUDIO_HEADER - 2;
/// Largest block [`fragment`] accepts (255 fragments).
pub const MAX_BLOCK: usize = 255 * FRAG_PAYLOAD;

pub const KIND_AUDIO: u8 = 0;
pub const KIND_PING: u8 = 1;
pub const KIND_PONG: u8 = 2;
pub const KIND_NACK: u8 = 3;
pub const KIND_REPORT: u8 = 4;
/// AUDIO flag: fragment index/count follow the header.
pub const FLAG_FRAGMENT: u8 = 0x10;
/// AUDIO flag: redundant previous blocks follow.
pub const FLAG_REDUNDANCY: u8 = 0x20;
/// AUDIO flag: the sender's stream restarted (decoder reset, no concealment across it).
pub const FLAG_DISCONTINUITY: u8 = 0x40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Input ended early (for a stream reader: wait for more bytes).
    Truncated,
    /// A field is out of range, a tag is unknown or a string is not UTF-8.
    Invalid,
    /// A field or the whole message exceeds its length cap.
    TooLong,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Error::Truncated => "truncated",
            Error::Invalid => "invalid",
            Error::TooLong => "too long",
        })
    }
}

impl std::error::Error for Error {}

// ---------------------------------------------------------------------------------------------
// Byte reader / writer helpers (shared with `auth` for the invite link).

pub(crate) struct Rd<'a>(pub(crate) &'a [u8]);

impl<'a> Rd<'a> {
    pub(crate) fn bytes(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if self.0.len() < n {
            return Err(Error::Truncated);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    pub(crate) fn arr<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.bytes(N)?.try_into().unwrap())
    }
    pub(crate) fn u8(&mut self) -> Result<u8, Error> {
        Ok(self.arr::<1>()?[0])
    }
    pub(crate) fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.arr()?))
    }
    pub(crate) fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.arr()?))
    }
    pub(crate) fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.arr()?))
    }
    /// `u8 len | bytes`, `len <= cap`.
    pub(crate) fn blob(&mut self, cap: usize) -> Result<&'a [u8], Error> {
        let n = self.u8()? as usize;
        if n > cap {
            return Err(Error::TooLong);
        }
        self.bytes(n)
    }
    pub(crate) fn string(&mut self, cap: usize) -> Result<String, Error> {
        let b = self.blob(cap)?;
        String::from_utf8(b.to_vec()).map_err(|_| Error::Invalid)
    }
    /// `u8 family (4|6) | ip | u16 port`.
    pub(crate) fn addr(&mut self) -> Result<SocketAddr, Error> {
        let ip = match self.u8()? {
            4 => IpAddr::V4(Ipv4Addr::from(self.arr::<4>()?)),
            6 => IpAddr::V6(Ipv6Addr::from(self.arr::<16>()?)),
            _ => return Err(Error::Invalid),
        };
        Ok(SocketAddr::new(ip, self.u16()?))
    }
    pub(crate) fn addrs(&mut self) -> Result<Vec<SocketAddr>, Error> {
        let n = self.count(MAX_ADDRS)?;
        (0..n).map(|_| self.addr()).collect()
    }
    fn count(&mut self, cap: usize) -> Result<usize, Error> {
        let n = self.u8()? as usize;
        if n > cap { Err(Error::TooLong) } else { Ok(n) }
    }
}

pub(crate) fn put_blob(out: &mut Vec<u8>, b: &[u8], cap: usize) -> Result<(), Error> {
    if b.len() > cap.min(255) {
        return Err(Error::TooLong);
    }
    out.push(b.len() as u8);
    out.extend_from_slice(b);
    Ok(())
}

pub(crate) fn put_addrs(out: &mut Vec<u8>, addrs: &[SocketAddr]) -> Result<(), Error> {
    put_count(out, addrs.len(), MAX_ADDRS)?;
    for a in addrs {
        match a.ip() {
            IpAddr::V4(ip) => {
                out.push(4);
                out.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                out.push(6);
                out.extend_from_slice(&ip.octets());
            }
        }
        out.extend_from_slice(&a.port().to_le_bytes());
    }
    Ok(())
}

fn put_count(out: &mut Vec<u8>, n: usize, cap: usize) -> Result<(), Error> {
    if n > cap {
        return Err(Error::TooLong);
    }
    out.push(n as u8);
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Datagrams

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AudioHeader {
    /// 0..8
    pub stream: u8,
    /// Format generation, 0..32 (bumped on every FORMAT change of the stream).
    pub generation: u8,
    /// Sender DLL deviation from nominal rate, 1/8 ppm.
    pub drift: i16,
    /// Block index.
    pub seq: u32,
    pub discontinuity: bool,
}

/// A decoded AUDIO datagram (borrows the input).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Audio<'a> {
    pub header: AudioHeader,
    /// `(index, count)` when the payload is one fragment of a block.
    pub fragment: Option<(u8, u8)>,
    pub redundant: Redundant<'a>,
    /// The primary block (or fragment) payload.
    pub payload: &'a [u8],
}

/// The `k` redundant previous blocks of an AUDIO datagram, oldest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Redundant<'a> {
    lens: &'a [u8],
    data: &'a [u8],
}

impl<'a> Redundant<'a> {
    pub fn len(&self) -> usize {
        self.lens.len() / 2
    }
    pub fn is_empty(&self) -> bool {
        self.lens.is_empty()
    }
    /// Blocks oldest first.
    pub fn iter(&self) -> impl Iterator<Item = &'a [u8]> + use<'a> {
        let mut data = self.data;
        self.lens.as_chunks::<2>().0.iter().map(move |l| {
            let (b, rest) = data.split_at(u16::from_le_bytes(*l) as usize);
            data = rest;
            b
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ping {
    pub id: u16,
    /// Sender clock, µs.
    pub t1: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pong {
    pub id: u16,
    /// Echoed PING time.
    pub t1: u64,
    /// Responder receive time, µs (responder clock).
    pub t2: u64,
    /// Responder send time, µs (responder clock).
    pub t3: u64,
}

/// Lost fragments of one stream: `(seq, fragment index)` pairs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Nack<'a> {
    pub stream: u8,
    raw: &'a [u8],
}

impl<'a> Nack<'a> {
    pub fn iter(&self) -> impl Iterator<Item = (u32, u8)> + use<'a> {
        self.raw
            .as_chunks::<5>()
            .0
            .iter()
            .map(|c| (u32::from_le_bytes([c[0], c[1], c[2], c[3]]), c[4]))
    }
}

/// Receiver statistics, sent once a second per stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Report {
    pub stream: u8,
    pub highest_seq: u32,
    pub lost: u16,
    pub late: u16,
    pub jitter_ms: u16,
    pub buffer_ms: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Datagram<'a> {
    Audio(Audio<'a>),
    Ping(Ping),
    Pong(Pong),
    Nack(Nack<'a>),
    Report(Report),
}

/// Most NACK entries that fit in one datagram.
pub const MAX_NACK: usize = (MAX_DATAGRAM - 3) / 5;

impl<'a> Datagram<'a> {
    pub fn decode(b: &'a [u8]) -> Result<Self, Error> {
        if b.len() > MAX_DATAGRAM {
            return Err(Error::TooLong);
        }
        let mut r = Rd(b);
        let k = r.u8()?;
        let (kind, flags) = (k & 0x0f, k & 0xf0);
        if kind != KIND_AUDIO && flags != 0 {
            return Err(Error::Invalid);
        }
        let d = match kind {
            KIND_AUDIO => {
                if flags & 0x80 != 0 {
                    return Err(Error::Invalid);
                }
                let sg = r.u8()?;
                let header = AudioHeader {
                    stream: sg & 7,
                    generation: sg >> 3,
                    drift: r.u16()? as i16,
                    seq: r.u32()?,
                    discontinuity: flags & FLAG_DISCONTINUITY != 0,
                };
                let fragment = if flags & FLAG_FRAGMENT != 0 {
                    let (i, n) = (r.u8()?, r.u8()?);
                    if i >= n {
                        return Err(Error::Invalid);
                    }
                    Some((i, n))
                } else {
                    None
                };
                let mut redundant = Redundant::default();
                if flags & FLAG_REDUNDANCY != 0 {
                    let k = r.u8()? as usize;
                    let lens = r.bytes(2 * k)?;
                    let total: usize = lens
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|l| u16::from_le_bytes(*l) as usize)
                        .sum();
                    redundant = Redundant {
                        lens,
                        data: r.bytes(total)?,
                    };
                }
                Datagram::Audio(Audio {
                    header,
                    fragment,
                    redundant,
                    payload: r.0,
                })
            }
            KIND_PING => Datagram::Ping(Ping {
                id: r.u16()?,
                t1: r.u64()?,
            }),
            KIND_PONG => Datagram::Pong(Pong {
                id: r.u16()?,
                t1: r.u64()?,
                t2: r.u64()?,
                t3: r.u64()?,
            }),
            KIND_NACK => {
                let stream = stream_id(r.u8()?)?;
                let n = r.u8()? as usize;
                Datagram::Nack(Nack {
                    stream,
                    raw: r.bytes(5 * n)?,
                })
            }
            KIND_REPORT => Datagram::Report(Report {
                stream: stream_id(r.u8()?)?,
                highest_seq: r.u32()?,
                lost: r.u16()?,
                late: r.u16()?,
                jitter_ms: r.u16()?,
                buffer_ms: r.u16()?,
            }),
            _ => return Err(Error::Invalid),
        };
        Ok(d)
    }
}

fn stream_id(s: u8) -> Result<u8, Error> {
    if s < 8 { Ok(s) } else { Err(Error::Invalid) }
}

fn audio_header(h: &AudioHeader, flags: u8) -> Result<[u8; AUDIO_HEADER], Error> {
    if h.stream >= 8 || h.generation >= 32 {
        return Err(Error::Invalid);
    }
    let disc = if h.discontinuity {
        FLAG_DISCONTINUITY
    } else {
        0
    };
    let [d0, d1] = h.drift.to_le_bytes();
    let [s0, s1, s2, s3] = h.seq.to_le_bytes();
    Ok([
        KIND_AUDIO | flags | disc,
        h.stream | h.generation << 3,
        d0,
        d1,
        s0,
        s1,
        s2,
        s3,
    ])
}

fn check_size(out: &[u8]) -> Result<(), Error> {
    if out.len() > MAX_DATAGRAM {
        Err(Error::TooLong)
    } else {
        Ok(())
    }
}

/// One unfragmented AUDIO datagram into `out` (cleared first). `redundant` = previous blocks,
/// oldest first (at most 255). Fails with [`Error::TooLong`] above [`MAX_DATAGRAM`].
pub fn encode_audio(
    h: &AudioHeader,
    redundant: &[&[u8]],
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    out.clear();
    let flags = if redundant.is_empty() {
        0
    } else {
        FLAG_REDUNDANCY
    };
    out.extend_from_slice(&audio_header(h, flags)?);
    if !redundant.is_empty() {
        if redundant.len() > 255 || redundant.iter().any(|b| b.len() > MAX_DATAGRAM) {
            return Err(Error::TooLong);
        }
        out.push(redundant.len() as u8);
        for b in redundant {
            out.extend_from_slice(&(b.len() as u16).to_le_bytes());
        }
        check_size(out)?;
        for b in redundant {
            out.extend_from_slice(b);
            check_size(out)?;
        }
    }
    if out.len() + payload.len() > MAX_DATAGRAM {
        return Err(Error::TooLong);
    }
    out.extend_from_slice(payload);
    Ok(())
}

// Datagrams built by `fragment_at` on this thread (tests count copies).
#[cfg(test)]
thread_local! {
    pub(crate) static FRAGMENT_COPIES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Datagrams [`fragment`] makes of a block of `len` bytes.
pub fn fragment_count(len: usize) -> usize {
    if AUDIO_HEADER + len <= MAX_DATAGRAM {
        1
    } else {
        len.div_ceil(FRAG_PAYLOAD)
    }
}

/// Size of datagram `i` (< [`fragment_count`]) of a block of `len` bytes.
pub fn fragment_len(len: usize, i: usize) -> usize {
    if fragment_count(len) == 1 {
        AUDIO_HEADER + len
    } else {
        AUDIO_HEADER + 2 + (len - i * FRAG_PAYLOAD).min(FRAG_PAYLOAD)
    }
}

/// Split `block` into AUDIO datagrams handed to `emit` in order. A block that fits goes out as
/// one plain datagram; otherwise every fragment but the last is exactly [`MAX_DATAGRAM`] bytes.
/// No allocation (uses a stack buffer).
pub fn fragment(h: &AudioHeader, block: &[u8], mut emit: impl FnMut(&[u8])) -> Result<(), Error> {
    for i in 0..fragment_count(block.len()) {
        fragment_at(h, block, i, &mut emit)?;
    }
    Ok(())
}

/// Only datagram `i` of [`fragment`]'s output; `false` (nothing built) when there is none.
pub fn fragment_at(
    h: &AudioHeader,
    block: &[u8],
    i: usize,
    emit: impl FnOnce(&[u8]),
) -> Result<bool, Error> {
    if block.len() > MAX_BLOCK {
        return Err(Error::TooLong);
    }
    let count = fragment_count(block.len());
    if i >= count {
        return Ok(false);
    }
    #[cfg(test)]
    FRAGMENT_COPIES.with(|c| c.set(c.get() + 1));
    let mut buf = [0u8; MAX_DATAGRAM];
    if AUDIO_HEADER + block.len() <= MAX_DATAGRAM {
        buf[..AUDIO_HEADER].copy_from_slice(&audio_header(h, 0)?);
        buf[AUDIO_HEADER..][..block.len()].copy_from_slice(block);
        emit(&buf[..AUDIO_HEADER + block.len()]);
        return Ok(true);
    }
    let chunk = &block[i * FRAG_PAYLOAD..((i + 1) * FRAG_PAYLOAD).min(block.len())];
    buf[..AUDIO_HEADER].copy_from_slice(&audio_header(h, FLAG_FRAGMENT)?);
    buf[AUDIO_HEADER] = i as u8;
    buf[AUDIO_HEADER + 1] = count as u8;
    buf[AUDIO_HEADER + 2..][..chunk.len()].copy_from_slice(chunk);
    emit(&buf[..AUDIO_HEADER + 2 + chunk.len()]);
    Ok(true)
}

impl Ping {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(KIND_PING);
        out.extend_from_slice(&self.id.to_le_bytes());
        out.extend_from_slice(&self.t1.to_le_bytes());
    }
}

impl Pong {
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.clear();
        out.push(KIND_PONG);
        out.extend_from_slice(&self.id.to_le_bytes());
        for t in [self.t1, self.t2, self.t3] {
            out.extend_from_slice(&t.to_le_bytes());
        }
    }
}

impl Report {
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        out.clear();
        out.push(KIND_REPORT);
        out.push(stream_id(self.stream)?);
        out.extend_from_slice(&self.highest_seq.to_le_bytes());
        for v in [self.lost, self.late, self.jitter_ms, self.buffer_ms] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        Ok(())
    }
}

/// NACK for `entries` = `(seq, fragment index)`; at most [`MAX_NACK`] entries.
pub fn encode_nack(stream: u8, entries: &[(u32, u8)], out: &mut Vec<u8>) -> Result<(), Error> {
    if entries.len() > MAX_NACK {
        return Err(Error::TooLong);
    }
    out.clear();
    out.push(KIND_NACK);
    out.push(stream_id(stream)?);
    out.push(entries.len() as u8);
    for (seq, frag) in entries {
        out.extend_from_slice(&seq.to_le_bytes());
        out.push(*frag);
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Reassembly

/// Joins fragmented AUDIO blocks. Memory is fixed at construction (`slots` × `max_block`);
/// tolerant of loss, reordering and duplicates. A partial block older than `timeout` (measured
/// from its first fragment, against the caller's `now`) is dropped; when all slots are busy the
/// oldest one is evicted.
pub struct Reassembler {
    slots: Vec<Slot>,
    timeout: Duration,
}

struct Slot {
    /// `(stream, generation, seq)`
    key: Option<(u8, u8, u32)>,
    count: u8,
    have: [u64; 4],
    got: u8,
    len: usize,
    started: Instant,
    done: bool,
    buf: Box<[u8]>,
}

impl Reassembler {
    pub fn new(slots: usize, max_block: usize, timeout: Duration) -> Self {
        let now = Instant::now();
        let slots = (0..slots.max(1))
            .map(|_| Slot {
                key: None,
                count: 0,
                have: [0; 4],
                got: 0,
                len: 0,
                started: now,
                done: false,
                buf: vec![0; max_block.min(MAX_BLOCK)].into_boxed_slice(),
            })
            .collect();
        Self { slots, timeout }
    }

    /// Feed one fragment (`index`/`count` from [`Audio::fragment`]). Returns the whole block
    /// when this fragment completed it; duplicates of a completed block return `None`.
    pub fn push(
        &mut self,
        now: Instant,
        h: &AudioHeader,
        index: u8,
        count: u8,
        payload: &[u8],
    ) -> Option<&[u8]> {
        if index >= count {
            return None;
        }
        let last = index + 1 == count;
        let max = self.slots[0].buf.len();
        if payload.len() > FRAG_PAYLOAD
            || (!last && payload.len() != FRAG_PAYLOAD)
            || (count as usize - 1) * FRAG_PAYLOAD + if last { payload.len() } else { 1 } > max
        {
            return None;
        }
        let key = (h.stream, h.generation, h.seq);
        let expired = |s: &Slot| now.saturating_duration_since(s.started) > self.timeout;
        let i = match self
            .slots
            .iter()
            .position(|s| s.key == Some(key) && !expired(s))
        {
            Some(i) => i,
            None => {
                // Victim: empty, else expired, else completed, else the oldest partial.
                let i = (0..self.slots.len())
                    .min_by_key(|&i| {
                        let s = &self.slots[i];
                        let rank = if s.key.is_none() {
                            0
                        } else if expired(s) {
                            1
                        } else if s.done {
                            2
                        } else {
                            3
                        };
                        (rank, s.started)
                    })
                    .unwrap();
                let s = &mut self.slots[i];
                *s = Slot {
                    key: Some(key),
                    count,
                    have: [0; 4],
                    got: 0,
                    len: 0,
                    started: now,
                    done: false,
                    buf: std::mem::take(&mut s.buf),
                };
                i
            }
        };
        let s = &mut self.slots[i];
        let bit = 1u64 << (index % 64);
        if s.done || s.count != count || s.have[index as usize / 64] & bit != 0 {
            return None;
        }
        s.have[index as usize / 64] |= bit;
        s.got += 1;
        let off = index as usize * FRAG_PAYLOAD;
        s.buf[off..off + payload.len()].copy_from_slice(payload);
        if last {
            s.len = off + payload.len();
        }
        if s.got == count {
            s.done = true;
            return Some(&s.buf[..s.len]);
        }
        None
    }

    /// The fragment indices still missing from block `(stream, generation, seq)`, when at least
    /// one of its fragments arrived (so the count is known) and the block is neither complete
    /// nor expired at `now`; `None` otherwise (ask for the whole block).
    pub fn missing(
        &self,
        now: Instant,
        stream: u8,
        generation: u8,
        seq: u32,
    ) -> Option<impl Iterator<Item = u8> + '_> {
        let s = self.slots.iter().find(|s| {
            s.key == Some((stream, generation, seq))
                && !s.done
                && now.saturating_duration_since(s.started) <= self.timeout
        })?;
        Some((0..s.count).filter(|&i| s.have[i as usize / 64] & 1 << (i % 64) == 0))
    }
}

// ---------------------------------------------------------------------------------------------
// Control stream frames: `u16 body len | u8 type | body`. Trailing bytes after the known fields
// of a body are ignored (room for compatible extensions); unknown types decode to
// [`Frame::Unknown`] and are skipped by length.

pub const T_HELLO: u8 = 1;
pub const T_PAKE2: u8 = 2;
pub const T_PAKE3: u8 = 3;
pub const T_WELCOME: u8 = 4;
pub const T_ROSTER: u8 = 5;
pub const T_FORMAT: u8 = 6;
pub const T_SUBSCRIBE: u8 = 7;
pub const T_QUALITY_REQ: u8 = 8;
pub const T_APP_OSC: u8 = 9;
/// Playout sync (broadcast host to listener): [`Playout`].
pub const T_PLAYOUT: u8 = 10;
pub const T_BYE: u8 = 11;
/// Session key rotation (host to members, after authentication).
pub const T_REKEY: u8 = 12;
/// After a rotation: proof that the sender holds the new session key (`auth::rekey_proof`).
pub const T_KEY_PROOF: u8 = 13;
/// Why the sender is closing, right before its BYE (one [`CloseReason`] byte; older peers
/// skip it as an unknown frame).
pub const T_CLOSE: u8 = 14;

pub const FRAME_HEADER: usize = 3;
pub const MAX_NAME: usize = 64;
pub const MAX_ADDRS: usize = 8;
pub const MAX_RELAY_URL: usize = 200;
pub const MAX_ROSTER: usize = 64;
pub const MAX_FORMATS: usize = 8;
pub const MAX_CODEC_OPTIONS: usize = 64;
pub const MAX_ENCODINGS: usize = 16;
/// HELLO proof: a 32-byte MAC or a 33-byte SPAKE2 message.
pub const MAX_PROOF: usize = 64;
pub const MAX_REDUNDANCY: u8 = 8;
/// Largest frame body (u16 length).
pub const MAX_BODY: usize = u16::MAX as usize;
/// Largest valid HELLO body (method, tag, role, name, proof, advertised addresses, relay).
pub const MAX_HELLO_BODY: usize =
    1 + 16 + 1 + 1 + MAX_NAME + 1 + MAX_PROOF + 1 + MAX_ADDRS * 19 + 1 + MAX_RELAY_URL;
pub const MAX_PAKE2_BODY: usize = 1 + MAX_PROOF;
pub const PAKE3_BODY: usize = 32;
/// REKEY: key, grace, and the evicted member's id (trailing; absent = none).
pub const REKEY_BODY: usize = 32 + 4 + 32;
pub const KEY_PROOF_BODY: usize = 32;
/// CLOSE: the reason byte (longer bodies are accepted, the rest is ignored).
pub const CLOSE_BODY_MAX: usize = 16;
/// PLAYOUT: 16 bytes now; longer bodies (later extensions) are accepted up to this.
pub const PLAYOUT_BODY_MAX: usize = 64;

/// Largest body a valid frame of type `ty` can have: readers check the header against it
/// before reading (or allocating) the body. Unknown types get [`MAX_BODY`].
pub fn max_body(ty: u8) -> usize {
    match ty {
        T_HELLO => MAX_HELLO_BODY,
        T_PAKE2 => MAX_PAKE2_BODY,
        T_PAKE3 => PAKE3_BODY,
        T_SUBSCRIBE => 3,
        T_QUALITY_REQ => 5,
        T_BYE => 0,
        T_REKEY => REKEY_BODY,
        T_KEY_PROOF => KEY_PROOF_BODY,
        T_CLOSE => CLOSE_BODY_MAX,
        T_PLAYOUT => PLAYOUT_BODY_MAX,
        _ => MAX_BODY,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Key = 0,
    Pake = 1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Role {
    Host = 0,
    #[default]
    Peer = 1,
    Listener = 2,
}

impl Role {
    fn from_u8(v: u8) -> Result<Self, Error> {
        match v {
            0 => Ok(Role::Host),
            1 => Ok(Role::Peer),
            2 => Ok(Role::Listener),
            _ => Err(Error::Invalid),
        }
    }
}

/// How a session connects its members.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionMode {
    /// Everyone connects to everyone and may publish streams.
    #[default]
    Mesh = 0,
    /// Star: listeners connect only to the host, never learn of each other and cannot publish.
    Broadcast = 1,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hello {
    pub method: Method,
    /// `auth::session_tag(session name)`.
    pub tag: [u8; 16],
    pub role: Role,
    pub name: String,
    /// KEY: `mac_C`; PAKE: the SPAKE2 message `X`.
    pub proof: Vec<u8>,
    /// Where the connector can be dialed (trailing field; absent = empty).
    pub addrs: Vec<SocketAddr>,
    /// The connector's home relay URL (trailing field after `addrs`; absent = none).
    pub relay: Option<String>,
}

/// The acceptor's SPAKE2 message. Carries no confirmation: the connector confirms first
/// (PAKE3), so an attempt abandoned here has not tested a password guess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pake2 {
    pub y: Vec<u8>,
}

/// New session key from the session host, sent over an authenticated control stream.
#[derive(Clone, PartialEq, Eq)]
pub struct Rekey {
    pub key: [u8; 32],
    /// How long the previous key stays valid for KEY proofs.
    pub grace_ms: u32,
    /// The member this rotation removes (trailing 32 bytes, sent only when set: a peer from
    /// before this field refuses the longer frame and drops the connection). Receivers block
    /// it before they install the key or pass it on.
    pub evicted: Option<[u8; 32]>,
}

impl std::fmt::Debug for Rekey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Rekey {{ key: <redacted>, grace_ms: {}, evicted: {:?} }}",
            self.grace_ms, self.evicted
        )
    }
}

/// Why a peer closes its connection to us (CLOSE frame).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseReason {
    /// Removed from the session (by the host, or because we lack its current key).
    Removed = 1,
    /// The peer blocked us.
    Blocked = 2,
    /// The session (the host) is ending.
    HostLeft = 3,
    /// The broadcast host is at its listener cap.
    Full = 4,
    /// Both sides dialed; this connection lost to the other one.
    Duplicate = 5,
}

impl CloseReason {
    pub fn from_u8(v: u8) -> Option<Self> {
        Some(match v {
            1 => CloseReason::Removed,
            2 => CloseReason::Blocked,
            3 => CloseReason::HostLeft,
            4 => CloseReason::Full,
            5 => CloseReason::Duplicate,
            _ => return None,
        })
    }

    /// The text [`crate::Event::Closed`] reports when a peer closed for this reason.
    pub fn as_str(self) -> &'static str {
        match self {
            CloseReason::Removed => "removed",
            CloseReason::Blocked => "blocked",
            CloseReason::HostLeft => "host left",
            CloseReason::Full => "session full",
            CloseReason::Duplicate => "duplicate connection",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Welcome {
    /// `mac_A` under the session key.
    pub mac: [u8; 32],
    /// PAKE path only: the session key wrapped under the PAKE key (`auth::PakeKey::wrap`).
    pub key: Option<[u8; 64]>,
    pub roster: Vec<PeerEntry>,
    pub formats: Vec<Format>,
    /// The acceptor's session mode (trailing byte, sent only for [`SessionMode::Broadcast`]:
    /// absent or 0 = mesh, 2..=255 refused; a peer from before this field ignores it). A
    /// broadcast hub's WELCOME tells the joiner whose authority it is that it joins as a
    /// listener.
    pub mode: SessionMode,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerEntry {
    /// iroh endpoint id (ed25519 public key).
    pub id: [u8; 32],
    pub addrs: Vec<SocketAddr>,
    pub relay: Option<String>,
    pub name: String,
    pub role: Role,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Format {
    pub stream: u8,
    pub generation: u8,
    pub codec: u8,
    pub sample_rate: u32,
    pub block_size: u16,
    pub channels: u8,
    pub options: Vec<u8>,
    /// Encoding ids a subscriber may pick with SUBSCRIBE.
    pub encodings: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Subscribe {
    pub stream: u8,
    pub encoding: u8,
    /// Redundant previous blocks per datagram, `<= MAX_REDUNDANCY`.
    pub redundancy: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QualityReq {
    /// bit/s
    pub max_bitrate: u32,
    pub preferred_format: u8,
}

/// Synchronised playout of one stream (PLAYOUT, broadcast host to listener): block `seq` of
/// `generation` was captured at `time_us` on the sender's clock (the clock of its PONGs); every
/// listener plays it `delay_ms` later on that clock. `delay_ms == 0` turns sync off. Body:
/// `u8 stream | u8 generation | u32 seq | u64 time_us | u16 delay_ms` (16 bytes; trailing bytes
/// ignored).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Playout {
    pub stream: u8,
    pub generation: u8,
    pub seq: u32,
    pub time_us: u64,
    pub delay_ms: u16,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Hello(Hello),
    Pake2(Pake2),
    Pake3 {
        conf: [u8; 32],
    },
    Welcome(Welcome),
    Roster(Vec<PeerEntry>),
    Format(Format),
    Subscribe(Subscribe),
    QualityReq(QualityReq),
    /// Opaque `/sb/...` OSC payload.
    AppOsc(Vec<u8>),
    Bye,
    Rekey(Rekey),
    KeyProof([u8; 32]),
    /// Reason byte of a closing peer ([`CloseReason`] values; others are possible).
    Close(u8),
    Playout(Playout),
    /// A frame type this version does not know (body skipped).
    Unknown(u8),
}

fn put_entry(out: &mut Vec<u8>, e: &PeerEntry) -> Result<(), Error> {
    out.extend_from_slice(&e.id);
    put_addrs(out, &e.addrs)?;
    put_blob(
        out,
        e.relay.as_deref().unwrap_or("").as_bytes(),
        MAX_RELAY_URL,
    )?;
    put_blob(out, e.name.as_bytes(), MAX_NAME)?;
    out.push(e.role as u8);
    Ok(())
}

fn get_entry(r: &mut Rd) -> Result<PeerEntry, Error> {
    let id = r.arr()?;
    let addrs = r.addrs()?;
    let relay = r.string(MAX_RELAY_URL)?;
    Ok(PeerEntry {
        id,
        addrs,
        relay: (!relay.is_empty()).then_some(relay),
        name: r.string(MAX_NAME)?,
        role: Role::from_u8(r.u8()?)?,
    })
}

fn put_roster(out: &mut Vec<u8>, roster: &[PeerEntry]) -> Result<(), Error> {
    put_count(out, roster.len(), MAX_ROSTER)?;
    roster.iter().try_for_each(|e| put_entry(out, e))
}

fn get_roster(r: &mut Rd) -> Result<Vec<PeerEntry>, Error> {
    let n = r.count(MAX_ROSTER)?;
    (0..n).map(|_| get_entry(r)).collect()
}

fn put_format(out: &mut Vec<u8>, f: &Format) -> Result<(), Error> {
    if f.stream >= 8 || f.generation >= 32 {
        return Err(Error::Invalid);
    }
    out.extend_from_slice(&[f.stream, f.generation, f.codec]);
    out.extend_from_slice(&f.sample_rate.to_le_bytes());
    out.extend_from_slice(&f.block_size.to_le_bytes());
    out.push(f.channels);
    put_blob(out, &f.options, MAX_CODEC_OPTIONS)?;
    put_blob(out, &f.encodings, MAX_ENCODINGS)
}

fn get_format(r: &mut Rd) -> Result<Format, Error> {
    let stream = stream_id(r.u8()?)?;
    let generation = r.u8()?;
    if generation >= 32 {
        return Err(Error::Invalid);
    }
    Ok(Format {
        stream,
        generation,
        codec: r.u8()?,
        sample_rate: r.u32()?,
        block_size: r.u16()?,
        channels: r.u8()?,
        options: r.blob(MAX_CODEC_OPTIONS)?.to_vec(),
        encodings: r.blob(MAX_ENCODINGS)?.to_vec(),
    })
}

impl Frame {
    pub fn type_byte(&self) -> u8 {
        match self {
            Frame::Hello(_) => T_HELLO,
            Frame::Pake2(_) => T_PAKE2,
            Frame::Pake3 { .. } => T_PAKE3,
            Frame::Welcome(_) => T_WELCOME,
            Frame::Roster(_) => T_ROSTER,
            Frame::Format(_) => T_FORMAT,
            Frame::Subscribe(_) => T_SUBSCRIBE,
            Frame::QualityReq(_) => T_QUALITY_REQ,
            Frame::AppOsc(_) => T_APP_OSC,
            Frame::Bye => T_BYE,
            Frame::Rekey(_) => T_REKEY,
            Frame::KeyProof(_) => T_KEY_PROOF,
            Frame::Close(_) => T_CLOSE,
            Frame::Playout(_) => T_PLAYOUT,
            Frame::Unknown(t) => *t,
        }
    }

    /// Append the whole frame (header + body) to `out`. Every cap is checked, so a frame that
    /// encodes is one the peer accepts. On error `out` is left as it was.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let start = out.len();
        out.extend_from_slice(&[0, 0, self.type_byte()]);
        let r = self.put_body(out);
        let len = out.len() - start - FRAME_HEADER;
        if r.is_err() || len > MAX_BODY {
            out.truncate(start);
            return r.and(Err(Error::TooLong));
        }
        out[start..start + 2].copy_from_slice(&(len as u16).to_le_bytes());
        Ok(())
    }

    fn put_body(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        match self {
            Frame::Hello(h) => {
                out.push(h.method as u8);
                out.extend_from_slice(&h.tag);
                out.push(h.role as u8);
                put_blob(out, h.name.as_bytes(), MAX_NAME)?;
                put_blob(out, &h.proof, MAX_PROOF)?;
                put_addrs(out, &h.addrs)?;
                if let Some(r) = &h.relay {
                    put_blob(out, r.as_bytes(), MAX_RELAY_URL)?;
                }
            }
            Frame::Pake2(p) => put_blob(out, &p.y, MAX_PROOF)?,
            Frame::Pake3 { conf } => out.extend_from_slice(conf),
            Frame::Welcome(w) => {
                out.extend_from_slice(&w.mac);
                match &w.key {
                    Some(k) => {
                        out.push(1);
                        out.extend_from_slice(k);
                    }
                    None => out.push(0),
                }
                put_roster(out, &w.roster)?;
                put_count(out, w.formats.len(), MAX_FORMATS)?;
                w.formats.iter().try_for_each(|f| put_format(out, f))?;
                if w.mode != SessionMode::Mesh {
                    out.push(w.mode as u8);
                }
            }
            Frame::Roster(r) => put_roster(out, r)?,
            Frame::Format(f) => put_format(out, f)?,
            Frame::Subscribe(s) => {
                if s.redundancy > MAX_REDUNDANCY {
                    return Err(Error::TooLong);
                }
                out.extend_from_slice(&[stream_id(s.stream)?, s.encoding, s.redundancy]);
            }
            Frame::QualityReq(q) => {
                out.extend_from_slice(&q.max_bitrate.to_le_bytes());
                out.push(q.preferred_format);
            }
            Frame::AppOsc(b) => out.extend_from_slice(b),
            Frame::Rekey(k) => {
                out.extend_from_slice(&k.key);
                out.extend_from_slice(&k.grace_ms.to_le_bytes());
                if let Some(id) = &k.evicted {
                    out.extend_from_slice(id);
                }
            }
            Frame::KeyProof(m) => out.extend_from_slice(m),
            Frame::Close(r) => out.push(*r),
            Frame::Playout(p) => {
                if p.stream >= 8 || p.generation >= 32 {
                    return Err(Error::Invalid);
                }
                out.extend_from_slice(&[p.stream, p.generation]);
                out.extend_from_slice(&p.seq.to_le_bytes());
                out.extend_from_slice(&p.time_us.to_le_bytes());
                out.extend_from_slice(&p.delay_ms.to_le_bytes());
            }
            Frame::Bye | Frame::Unknown(_) => {}
        }
        Ok(())
    }

    /// Decode the body of a frame of type `ty`.
    pub fn decode_body(ty: u8, body: &[u8]) -> Result<Frame, Error> {
        let mut r = Rd(body);
        Ok(match ty {
            T_HELLO => Frame::Hello(Hello {
                method: match r.u8()? {
                    0 => Method::Key,
                    1 => Method::Pake,
                    _ => return Err(Error::Invalid),
                },
                tag: r.arr()?,
                role: Role::from_u8(r.u8()?)?,
                name: r.string(MAX_NAME)?,
                proof: r.blob(MAX_PROOF)?.to_vec(),
                addrs: if r.0.is_empty() { vec![] } else { r.addrs()? },
                relay: if r.0.is_empty() {
                    None
                } else {
                    Some(r.string(MAX_RELAY_URL)?).filter(|s| !s.is_empty())
                },
            }),
            T_PAKE2 => Frame::Pake2(Pake2 {
                y: r.blob(MAX_PROOF)?.to_vec(),
            }),
            T_PAKE3 => Frame::Pake3 { conf: r.arr()? },
            T_WELCOME => {
                let mac = r.arr()?;
                let key = match r.u8()? {
                    0 => None,
                    1 => Some(r.arr()?),
                    _ => return Err(Error::Invalid),
                };
                let roster = get_roster(&mut r)?;
                let n = r.count(MAX_FORMATS)?;
                let formats = (0..n)
                    .map(|_| get_format(&mut r))
                    .collect::<Result<_, _>>()?;
                let mode = match r.0.first() {
                    None | Some(0) => SessionMode::Mesh,
                    Some(1) => SessionMode::Broadcast,
                    Some(_) => return Err(Error::Invalid),
                };
                Frame::Welcome(Welcome {
                    mac,
                    key,
                    roster,
                    formats,
                    mode,
                })
            }
            T_ROSTER => Frame::Roster(get_roster(&mut r)?),
            T_FORMAT => Frame::Format(get_format(&mut r)?),
            T_SUBSCRIBE => {
                let s = Subscribe {
                    stream: stream_id(r.u8()?)?,
                    encoding: r.u8()?,
                    redundancy: r.u8()?,
                };
                if s.redundancy > MAX_REDUNDANCY {
                    return Err(Error::TooLong);
                }
                Frame::Subscribe(s)
            }
            T_QUALITY_REQ => Frame::QualityReq(QualityReq {
                max_bitrate: r.u32()?,
                preferred_format: r.u8()?,
            }),
            T_APP_OSC => Frame::AppOsc(body.to_vec()),
            T_BYE => Frame::Bye,
            T_KEY_PROOF => Frame::KeyProof(r.arr()?),
            T_CLOSE => Frame::Close(r.u8()?),
            T_PLAYOUT => {
                let stream = stream_id(r.u8()?)?;
                let generation = r.u8()?;
                if generation >= 32 {
                    return Err(Error::Invalid);
                }
                Frame::Playout(Playout {
                    stream,
                    generation,
                    seq: r.u32()?,
                    time_us: r.u64()?,
                    delay_ms: r.u16()?,
                })
            }
            T_REKEY => Frame::Rekey(Rekey {
                key: r.arr()?,
                grace_ms: r.u32()?,
                evicted: if r.0.is_empty() { None } else { Some(r.arr()?) },
            }),
            t => Frame::Unknown(t),
        })
    }
}

/// Decode one frame from the front of `buf`; returns it and the bytes consumed.
/// [`Error::Truncated`] means `buf` holds less than a whole frame.
pub fn decode_frame(buf: &[u8]) -> Result<(Frame, usize), Error> {
    let mut r = Rd(buf);
    let len = r.u16()? as usize;
    let ty = r.u8()?;
    let body = r.bytes(len)?;
    Ok((Frame::decode_body(ty, body)?, FRAME_HEADER + len))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// xorshift64*, deterministic test randomness.
    pub(crate) struct Rng(pub u64);
    impl Rng {
        pub fn next(&mut self) -> u64 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            self.0.wrapping_mul(0x2545_f491_4f6c_dd1d)
        }
        pub fn below(&mut self, n: u64) -> u64 {
            self.next() % n
        }
    }

    fn hdr() -> AudioHeader {
        AudioHeader {
            stream: 3,
            generation: 17,
            drift: -2,
            seq: 0x0102_0304,
            discontinuity: true,
        }
    }

    fn enc(f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
        let mut v = Vec::new();
        f(&mut v);
        v
    }

    #[test]
    fn audio_golden_and_round_trip() {
        let b = enc(|o| encode_audio(&hdr(), &[], b"PCM", o).unwrap());
        // kind 0 | D, stream 3 | gen 17<<3 = 0x8b, drift -2 LE, seq LE, payload
        assert_eq!(
            b,
            [
                0x40, 0x8b, 0xfe, 0xff, 0x04, 0x03, 0x02, 0x01, b'P', b'C', b'M'
            ]
        );
        let Datagram::Audio(a) = Datagram::decode(&b).unwrap() else {
            panic!()
        };
        assert_eq!(
            (a.header, a.fragment, a.payload, a.redundant.len()),
            (hdr(), None, &b"PCM"[..], 0)
        );

        let h = AudioHeader {
            discontinuity: false,
            ..hdr()
        };
        let b = enc(|o| encode_audio(&h, &[b"a", b"bc"], b"Z", o).unwrap());
        assert_eq!(
            b,
            [
                0x20, 0x8b, 0xfe, 0xff, 4, 3, 2, 1, 2, 1, 0, 2, 0, b'a', b'b', b'c', b'Z'
            ]
        );
        let Datagram::Audio(a) = Datagram::decode(&b).unwrap() else {
            panic!()
        };
        assert_eq!(
            a.redundant.iter().collect::<Vec<_>>(),
            [&b"a"[..], &b"bc"[..]]
        );
        assert_eq!(a.payload, b"Z");
    }

    #[test]
    fn audio_fragment_golden() {
        let h = AudioHeader {
            stream: 1,
            generation: 0,
            drift: 0,
            seq: 7,
            discontinuity: false,
        };
        let block: Vec<u8> = (0..FRAG_PAYLOAD + 5).map(|i| i as u8).collect();
        let mut out = Vec::new();
        fragment(&h, &block, |d| out.push(d.to_vec())).unwrap();
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].len(), MAX_DATAGRAM);
        assert_eq!(out[0][..10], [0x10, 0x01, 0, 0, 7, 0, 0, 0, 0, 2]);
        assert_eq!(out[1][..10], [0x10, 0x01, 0, 0, 7, 0, 0, 0, 1, 2]);
        assert_eq!(out[1][10..], block[FRAG_PAYLOAD..]);
        let Datagram::Audio(a) = Datagram::decode(&out[1]).unwrap() else {
            panic!()
        };
        assert_eq!(a.fragment, Some((1, 2)));
        // A small block is one plain datagram.
        let mut n = 0;
        fragment(&h, b"xy", |d| {
            n += 1;
            assert_eq!(d, [0, 1, 0, 0, 7, 0, 0, 0, b'x', b'y']);
        })
        .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn control_datagrams_golden() {
        let b = enc(|o| {
            Ping {
                id: 0x0201,
                t1: 0x0a09_0807_0605_0403,
            }
            .encode(o)
        });
        assert_eq!(b, [1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10]);
        assert_eq!(
            Datagram::decode(&b),
            Ok(Datagram::Ping(Ping {
                id: 0x0201,
                t1: 0x0a09_0807_0605_0403
            }))
        );

        let p = Pong {
            id: 1,
            t1: 2,
            t2: 3,
            t3: 4,
        };
        let b = enc(|o| p.encode(o));
        let mut want = vec![2, 1, 0];
        for t in [2u64, 3, 4] {
            want.extend_from_slice(&t.to_le_bytes());
        }
        assert_eq!(b, want);
        assert_eq!(Datagram::decode(&b), Ok(Datagram::Pong(p)));

        let b = enc(|o| encode_nack(2, &[(5, 0), (0x0100_0000, 3)], o).unwrap());
        assert_eq!(b, [3, 2, 2, 5, 0, 0, 0, 0, 0, 0, 0, 1, 3]);
        let Datagram::Nack(n) = Datagram::decode(&b).unwrap() else {
            panic!()
        };
        assert_eq!(
            (n.stream, n.iter().collect::<Vec<_>>()),
            (2, vec![(5, 0), (0x0100_0000, 3)])
        );

        let r = Report {
            stream: 1,
            highest_seq: 9,
            lost: 2,
            late: 3,
            jitter_ms: 4,
            buffer_ms: 40,
        };
        let b = enc(|o| r.encode(o).unwrap());
        assert_eq!(b, [4, 1, 9, 0, 0, 0, 2, 0, 3, 0, 4, 0, 40, 0]);
        assert_eq!(Datagram::decode(&b), Ok(Datagram::Report(r)));
    }

    #[test]
    fn datagram_rejects() {
        let all = [
            enc(|o| encode_audio(&hdr(), &[b"a", b"bc"], b"Z", o).unwrap()),
            enc(|o| Ping { id: 1, t1: 2 }.encode(o)),
            enc(|o| {
                Pong {
                    id: 1,
                    t1: 2,
                    t2: 3,
                    t3: 4,
                }
                .encode(o)
            }),
            enc(|o| encode_nack(2, &[(5, 0)], o).unwrap()),
            enc(|o| Report::default().encode(o).unwrap()),
        ];
        for b in &all {
            // AUDIO's payload is the remainder, so only its fixed part must be complete.
            let min = if b[0] & 0x0f == KIND_AUDIO {
                b.len() - 1
            } else {
                b.len()
            };
            for n in 0..min {
                assert!(Datagram::decode(&b[..n]).is_err(), "{b:?} truncated to {n}");
            }
        }
        assert_eq!(Datagram::decode(&[0x15]), Err(Error::Invalid)); // flags on PING
        assert_eq!(Datagram::decode(&[9]), Err(Error::Invalid)); // unknown kind
        assert_eq!(
            Datagram::decode(&[0x80, 0, 0, 0, 0, 0, 0, 0]),
            Err(Error::Invalid)
        );
        assert_eq!(
            Datagram::decode(&[0x10, 0, 0, 0, 0, 0, 0, 0, 2, 2]),
            Err(Error::Invalid)
        );
        assert_eq!(
            Datagram::decode(&[4, 8, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(Error::Invalid)
        );
        assert_eq!(
            Datagram::decode(&[0; MAX_DATAGRAM + 1]),
            Err(Error::TooLong)
        );
        let mut rng = Rng(7);
        for _ in 0..20_000 {
            let n = rng.below(40) as usize;
            let b: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
            let _ = Datagram::decode(&b);
        }
    }

    #[test]
    fn size_bounds() {
        let mut out = Vec::new();
        let big = vec![0u8; MAX_DATAGRAM];
        assert_eq!(
            encode_audio(&hdr(), &[], &big[..MAX_DATAGRAM - 8], &mut out),
            Ok(())
        );
        assert_eq!(out.len(), MAX_DATAGRAM);
        assert_eq!(
            encode_audio(&hdr(), &[], &big[..MAX_DATAGRAM - 7], &mut out),
            Err(Error::TooLong)
        );
        assert_eq!(
            encode_audio(&hdr(), &[&big[..600]], &big[..600], &mut out),
            Err(Error::TooLong)
        );
        assert_eq!(
            encode_nack(0, &[(0, 0); MAX_NACK + 1], &mut out),
            Err(Error::TooLong)
        );
        encode_nack(0, &[(0, 0); MAX_NACK], &mut out).unwrap();
        assert!(out.len() <= MAX_DATAGRAM);
        assert_eq!(
            encode_audio(&AudioHeader { stream: 8, ..hdr() }, &[], b"", &mut out),
            Err(Error::Invalid)
        );
        for len in [
            0,
            1,
            1142,
            1143,
            FRAG_PAYLOAD * 3,
            FRAG_PAYLOAD * 3 + 1,
            MAX_BLOCK,
        ] {
            let block = vec![1u8; len];
            let mut total = 0;
            fragment(&hdr(), &block, |d| {
                assert!(d.len() <= MAX_DATAGRAM);
                total += 1;
            })
            .unwrap();
            assert!(total >= 1);
        }
        assert_eq!(
            fragment(&hdr(), &vec![0; MAX_BLOCK + 1], |_| {}),
            Err(Error::TooLong)
        );
    }

    fn frags(seq: u32, block: &[u8]) -> Vec<Vec<u8>> {
        let mut v = Vec::new();
        fragment(&AudioHeader { seq, ..hdr() }, block, |d| v.push(d.to_vec())).unwrap();
        v
    }

    fn feed(r: &mut Reassembler, now: Instant, d: &[u8]) -> Option<Vec<u8>> {
        let Datagram::Audio(a) = Datagram::decode(d).unwrap() else {
            panic!()
        };
        let (i, n) = a.fragment.unwrap();
        r.push(now, &a.header, i, n, a.payload).map(|b| b.to_vec())
    }

    #[test]
    fn reassembler_loss_reorder_duplicates() {
        let mut rng = Rng(42);
        let t0 = Instant::now();
        let mut r = Reassembler::new(4, 8 * FRAG_PAYLOAD, Duration::from_millis(100));
        let blocks: Vec<Vec<u8>> = (0..400)
            .map(|s| (0..2000 + s * 13).map(|i| (i * 7 + s) as u8).collect())
            .collect();
        // Interleave the fragments of a window of 3 blocks, shuffle, duplicate ~10 %, drop ~5 %.
        let mut complete = vec![0u32; blocks.len()];
        let mut lost_any = vec![false; blocks.len()];
        for w in (0..blocks.len()).step_by(3) {
            let mut pkts: Vec<(usize, Vec<u8>)> = Vec::new();
            for (s, b) in blocks.iter().enumerate().skip(w).take(3) {
                for f in frags(s as u32, b) {
                    if rng.below(100) < 5 {
                        lost_any[s] = true;
                        continue;
                    }
                    if rng.below(100) < 10 {
                        pkts.push((s, f.clone()));
                    }
                    pkts.push((s, f));
                }
            }
            for i in (1..pkts.len()).rev() {
                pkts.swap(i, rng.below(i as u64 + 1) as usize);
            }
            let now = t0 + Duration::from_millis(w as u64);
            for (s, p) in pkts {
                if let Some(b) = feed(&mut r, now, &p) {
                    assert_eq!(b, blocks[s]);
                    complete[s] += 1;
                }
            }
        }
        for s in 0..blocks.len() {
            assert_eq!(complete[s], u32::from(!lost_any[s]), "block {s}");
        }
        assert!(lost_any.iter().any(|&l| l) && complete.iter().sum::<u32>() > 200);
    }

    #[test]
    fn reassembler_reports_missing_fragments() {
        let t0 = Instant::now();
        let to = Duration::from_millis(50);
        let mut r = Reassembler::new(2, 8 * FRAG_PAYLOAD, to);
        let a: Vec<u8> = (0..4 * FRAG_PAYLOAD + 3).map(|i| i as u8).collect();
        let f = frags(5, &a);
        assert_eq!(f.len(), 5);
        let missing = |r: &Reassembler, now| {
            r.missing(now, hdr().stream, hdr().generation, 5)
                .map(|m| m.collect::<Vec<_>>())
        };
        assert_eq!(missing(&r, t0), None); // nothing arrived: count unknown
        for i in [0, 2, 4] {
            feed(&mut r, t0, &f[i]);
        }
        assert_eq!(missing(&r, t0), Some(vec![1, 3]));
        assert_eq!(missing(&r, t0 + to * 2), None); // expired
        feed(&mut r, t0, &f[3]);
        assert_eq!(missing(&r, t0), Some(vec![1]));
        assert_eq!(feed(&mut r, t0, &f[1]), Some(a));
        assert_eq!(missing(&r, t0), None); // complete
    }

    #[test]
    fn reassembler_timeout_eviction_and_bounds() {
        let t0 = Instant::now();
        let to = Duration::from_millis(50);
        let mut r = Reassembler::new(2, 4 * FRAG_PAYLOAD, to);
        let a: Vec<u8> = (0..3000).map(|i| i as u8).collect();
        let fa = frags(1, &a);
        assert_eq!(feed(&mut r, t0, &fa[0]), None);
        // After the deadline the partial block is gone: the rest alone does not complete it.
        let late = t0 + to * 2;
        assert_eq!(feed(&mut r, late, &fa[1]), None);
        assert_eq!(feed(&mut r, late, &fa[2]), None);
        assert_eq!(feed(&mut r, late, &fa[0]), Some(a.clone()));
        // A block larger than max_block is refused.
        let big = vec![0u8; 5 * FRAG_PAYLOAD];
        for f in frags(9, &big) {
            assert_eq!(feed(&mut r, late, &f), None);
        }
        // More concurrent partial blocks than slots: oldest evicted, memory stays fixed.
        let fb = frags(2, &a);
        let fc = frags(3, &a);
        let fd = frags(4, &a);
        feed(&mut r, late, &fb[0]);
        feed(&mut r, late + Duration::from_millis(1), &fc[0]);
        feed(&mut r, late + Duration::from_millis(2), &fd[0]); // evicts block 2
        let t = late + Duration::from_millis(3);
        assert_eq!(feed(&mut r, t, &fb[1]), None);
        assert_eq!(feed(&mut r, t, &fd[1]), None);
        assert_eq!(feed(&mut r, t, &fd[2]), Some(a.clone()));
        assert_eq!(r.slots.len(), 2);
        // A fragment claiming a different count for the same block is ignored.
        let Datagram::Audio(x) = Datagram::decode(&fc[1]).unwrap() else {
            panic!()
        };
        assert_eq!(r.push(t, &x.header, 1, 4, x.payload), None);
        // Short non-final fragment is rejected.
        assert_eq!(r.push(t, &x.header, 0, 3, &[0; 10]), None);
        // Out-of-range index/count through the public API (index 255 used to overflow).
        assert_eq!(r.push(t, &x.header, 255, 255, &[0; 10]), None);
        assert_eq!(r.push(t, &x.header, 255, 0, &[0; 10]), None);
        assert_eq!(r.push(t, &x.header, 0, 0, &[]), None);
        assert_eq!(r.push(t, &x.header, 254, 255, &[0; 10]), None); // block > max_block
    }

    fn entry() -> PeerEntry {
        PeerEntry {
            id: [7; 32],
            addrs: vec!["1.2.3.4:10999".parse().unwrap(), "[::1]:5".parse().unwrap()],
            relay: Some("https://r.example/".into()),
            name: "Ann".into(),
            role: Role::Host,
        }
    }

    fn format() -> Format {
        Format {
            stream: 1,
            generation: 2,
            codec: 3,
            sample_rate: 48000,
            block_size: 480,
            channels: 2,
            options: vec![9],
            encodings: vec![0, 1],
        }
    }

    fn all_frames() -> Vec<Frame> {
        vec![
            Frame::Hello(Hello {
                method: Method::Pake,
                tag: [1; 16],
                role: Role::Listener,
                name: "Bo".into(),
                proof: vec![5; 33],
                addrs: vec![
                    "10.0.0.1:258".parse().unwrap(),
                    "[fe80::2]:9".parse().unwrap(),
                ],
                relay: Some("http://127.0.0.1:3340/".into()),
            }),
            Frame::Pake2(Pake2 { y: vec![6; 33] }),
            Frame::Pake3 { conf: [3; 32] },
            Frame::Welcome(Welcome {
                mac: [4; 32],
                key: Some([5; 64]),
                roster: vec![entry()],
                formats: vec![format()],
                mode: SessionMode::Broadcast,
            }),
            Frame::Welcome(Welcome {
                mac: [4; 32],
                key: None,
                roster: vec![],
                formats: vec![],
                mode: SessionMode::Mesh,
            }),
            Frame::Roster(vec![
                entry(),
                PeerEntry {
                    relay: None,
                    addrs: vec![],
                    ..entry()
                },
            ]),
            Frame::Format(format()),
            Frame::Subscribe(Subscribe {
                stream: 2,
                encoding: 1,
                redundancy: 2,
            }),
            Frame::QualityReq(QualityReq {
                max_bitrate: 96_000,
                preferred_format: 1,
            }),
            Frame::AppOsc(b"/sb/x\0\0\0,\0\0\0".to_vec()),
            Frame::Bye,
            Frame::KeyProof([9; 32]),
            Frame::Close(CloseReason::Removed as u8),
            Frame::Rekey(Rekey {
                key: [8; 32],
                grace_ms: 30_000,
                evicted: None,
            }),
            Frame::Rekey(Rekey {
                key: [8; 32],
                grace_ms: 0,
                evicted: Some([5; 32]),
            }),
            Frame::Playout(Playout {
                stream: 1,
                generation: 31,
                seq: u32::MAX,
                time_us: u64::MAX - 1,
                delay_ms: 150,
            }),
        ]
    }

    #[test]
    fn frames_golden() {
        let b = enc(|o| {
            Frame::Hello(Hello {
                method: Method::Key,
                tag: [0xaa; 16],
                role: Role::Peer,
                name: "A".into(),
                proof: vec![1, 2],
                addrs: vec!["10.0.0.1:258".parse().unwrap()],
                relay: None,
            })
            .encode(o)
            .unwrap()
        });
        let mut want = vec![31, 0, T_HELLO, 0];
        want.extend_from_slice(&[0xaa; 16]);
        want.extend_from_slice(&[1, 1, b'A', 2, 1, 2, 1, 4, 10, 0, 0, 1, 2, 1]);
        assert_eq!(b, want);
        // An older HELLO without the trailing address list decodes with no addresses.
        let Frame::Hello(h) = Frame::decode_body(T_HELLO, &want[3..want.len() - 8]).unwrap() else {
            panic!()
        };
        assert!(h.addrs.is_empty());

        let b = enc(|o| {
            Frame::Roster(vec![PeerEntry {
                id: [9; 32],
                addrs: vec!["10.0.0.1:258".parse().unwrap()],
                relay: None,
                name: "N".into(),
                role: Role::Listener,
            }])
            .encode(o)
            .unwrap()
        });
        let mut want = vec![0, 0, T_ROSTER, 1];
        want.extend_from_slice(&[9; 32]);
        want.extend_from_slice(&[1, 4, 10, 0, 0, 1, 2, 1, 0, 1, b'N', 2]);
        want[0] = (want.len() - 3) as u8;
        assert_eq!(b, want);

        let b = enc(|o| Frame::Format(format()).encode(o).unwrap());
        assert_eq!(
            b,
            [
                15, 0, T_FORMAT, 1, 2, 3, 0x80, 0xbb, 0, 0, 0xe0, 1, 2, 1, 9, 2, 0, 1
            ]
        );
        let b = enc(|o| {
            Frame::Subscribe(Subscribe {
                stream: 2,
                encoding: 1,
                redundancy: 3,
            })
            .encode(o)
            .unwrap()
        });
        assert_eq!(b, [3, 0, T_SUBSCRIBE, 2, 1, 3]);
        let b = enc(|o| {
            Frame::QualityReq(QualityReq {
                max_bitrate: 0x0102_0304,
                preferred_format: 5,
            })
            .encode(o)
            .unwrap()
        });
        assert_eq!(b, [5, 0, T_QUALITY_REQ, 4, 3, 2, 1, 5]);
        let b = enc(|o| Frame::Pake3 { conf: [3; 32] }.encode(o).unwrap());
        assert_eq!(b[..3], [32, 0, T_PAKE3]);
        let b = enc(|o| Frame::Pake2(Pake2 { y: vec![6; 2] }).encode(o).unwrap());
        assert_eq!(b, [3, 0, T_PAKE2, 2, 6, 6]);
        let rekey = |evicted| Rekey {
            key: [7; 32],
            grace_ms: 0x0102_0304,
            evicted,
        };
        let b = enc(|o| Frame::Rekey(rekey(None)).encode(o).unwrap());
        assert_eq!(
            (&b[..3], &b[35..]),
            (&[36, 0, T_REKEY][..], &[4, 3, 2, 1][..])
        );
        // a REKEY from before the evicted field decodes with none
        assert_eq!(decode_frame(&b), Ok((Frame::Rekey(rekey(None)), 39)));
        let b = enc(|o| Frame::Rekey(rekey(Some([9; 32]))).encode(o).unwrap());
        assert_eq!(
            (&b[..3], &b[35..39], &b[39..]),
            (&[68, 0, T_REKEY][..], &[4, 3, 2, 1][..], &[9; 32][..])
        );
        assert_eq!(b.len() - FRAME_HEADER, max_body(T_REKEY));
        // a cut-off id is refused, not read as none
        assert!(Frame::decode_body(T_REKEY, &b[3..50]).is_err());
        assert!(!format!("{:?}", Frame::Rekey(rekey(None))).contains("7, 7"));
        let b = enc(|o| {
            Frame::Welcome(Welcome {
                mac: [4; 32],
                key: None,
                roster: vec![],
                formats: vec![],
                mode: SessionMode::Mesh,
            })
            .encode(o)
            .unwrap()
        });
        assert_eq!(
            (b.len(), &b[..3], &b[35..]),
            (38, &[35, 0, T_WELCOME][..], &[0, 0, 0][..])
        );
        // a broadcast hub's WELCOME: one trailing mode byte; a WELCOME without it is a mesh
        let welcome = |mode| Welcome {
            mac: [4; 32],
            key: None,
            roster: vec![],
            formats: vec![],
            mode,
        };
        let w = enc(|o| Frame::Welcome(welcome(SessionMode::Broadcast)).encode(o).unwrap());
        assert_eq!((&w[..3], &w[3..35], &w[35..]), (&[36, 0, T_WELCOME][..], &[4; 32][..], &[0, 0, 0, 1][..]));
        assert_eq!(decode_frame(&b), Ok((Frame::Welcome(welcome(SessionMode::Mesh)), 38)));
        // a later version's longer body decodes; an unknown mode is refused, not read as mesh
        let mut long = w.clone();
        long.extend_from_slice(&[0xee; 2]);
        long[0] = 38;
        assert_eq!(decode_frame(&long), Ok((Frame::Welcome(welcome(SessionMode::Broadcast)), 41)));
        assert!(Frame::decode_body(T_WELCOME, &[&w[3..38], [2].as_slice()].concat()).is_err());
        // an explicit mesh byte (a later version with more after it) is a mesh
        let mesh = Frame::decode_body(T_WELCOME, &[&w[3..38], [0, 0xee].as_slice()].concat());
        assert_eq!(mesh, Ok(Frame::Welcome(welcome(SessionMode::Mesh))));
        assert_eq!(
            enc(|o| Frame::AppOsc(vec![1, 2]).encode(o).unwrap()),
            [2, 0, T_APP_OSC, 1, 2]
        );
        assert_eq!(enc(|o| Frame::Bye.encode(o).unwrap()), [0, 0, T_BYE]);
        let b = enc(|o| Frame::KeyProof([9; 32]).encode(o).unwrap());
        assert_eq!((&b[..3], b.len()), (&[32, 0, T_KEY_PROOF][..], 35));
        assert_eq!(enc(|o| Frame::Close(2).encode(o).unwrap()), [1, 0, T_CLOSE, 2]);
        // a longer CLOSE body (a later version) still decodes; unknown reasons map to none
        assert_eq!(decode_frame(&[2, 0, T_CLOSE, 1, 7]), Ok((Frame::Close(1), 5)));
        assert_eq!(decode_frame(&[17, 0, T_CLOSE]).map(|x| x.0), Err(Error::Truncated));
        assert_eq!(max_body(T_CLOSE), CLOSE_BODY_MAX);
        assert_eq!(CloseReason::from_u8(1), Some(CloseReason::Removed));
        assert_eq!(CloseReason::from_u8(4).map(CloseReason::as_str), Some("session full"));
        assert_eq!(CloseReason::from_u8(9), None);

        let p = Playout {
            stream: 0,
            generation: 2,
            seq: 0x0102_0304,
            time_us: 0x0a09_0807_0605_0403,
            delay_ms: 0x0096,
        };
        let b = enc(|o| Frame::Playout(p).encode(o).unwrap());
        assert_eq!(
            b,
            [16, 0, T_PLAYOUT, 0, 2, 4, 3, 2, 1, 3, 4, 5, 6, 7, 8, 9, 10, 0x96, 0]
        );
        // a later version's longer body decodes (the tail is ignored); bad ids are refused
        let mut long = b.clone();
        long.extend_from_slice(&[0xee; 4]);
        long[0] = 20;
        assert_eq!(decode_frame(&long), Ok((Frame::Playout(p), 23)));
        assert!(Frame::decode_body(T_PLAYOUT, &[[8].as_slice(), &b[4..]].concat()).is_err());
        assert!(Frame::decode_body(T_PLAYOUT, &[&b[3..4], [32].as_slice(), &b[5..]].concat()).is_err());
        assert!(Frame::Playout(Playout { generation: 32, ..p }).encode(&mut vec![]).is_err());
    }

    #[test]
    fn frames_round_trip_and_truncation() {
        for f in all_frames() {
            let b = enc(|o| f.encode(o).unwrap());
            assert_eq!(decode_frame(&b), Ok((f.clone(), b.len())));
            for n in 0..b.len() {
                assert_eq!(
                    decode_frame(&b[..n]),
                    Err(Error::Truncated),
                    "{f:?} cut at {n}"
                );
            }
            // Truncating the body while fixing up the length is also an error (except opaque
            // or empty bodies).
            if !matches!(f, Frame::AppOsc(_) | Frame::Bye) {
                // HELLO's trailing address list and relay are optional: cutting exactly before
                // either is valid.
                let optional_tails = match &f {
                    Frame::Hello(h) => {
                        let len = |h: Hello| enc(|o| Frame::Hello(h).encode(o).unwrap()).len() - 3;
                        let bare = Hello {
                            addrs: vec![],
                            relay: None,
                            ..h.clone()
                        };
                        let no_relay = Hello {
                            relay: None,
                            ..h.clone()
                        };
                        vec![len(bare) - 1, len(no_relay)]
                    }
                    // REKEY's evicted id is optional too, and WELCOME's mode
                    Frame::Rekey(r) if r.evicted.is_some() => vec![36],
                    Frame::Welcome(w) if w.mode != SessionMode::Mesh => vec![b.len() - 4],
                    _ => vec![],
                };
                for n in 0..b.len() - 3 {
                    if !optional_tails.contains(&n) {
                        assert!(
                            Frame::decode_body(b[2], &b[3..3 + n]).is_err(),
                            "{f:?} body {n}"
                        );
                    }
                }
            }
            // Every valid frame fits the per-type cap readers enforce before reading the body.
            assert!(b.len() - 3 <= max_body(b[2]), "{f:?}");
        }
        // Unknown types are skipped by length.
        let b = [2, 0, 200, 1, 2, 0, 0, T_BYE];
        assert_eq!(decode_frame(&b), Ok((Frame::Unknown(200), 5)));
        assert_eq!(decode_frame(&b[5..]), Ok((Frame::Bye, 3)));
    }

    #[test]
    fn frame_caps_and_garbage() {
        let long = "x".repeat(MAX_NAME + 1);
        let h = Hello {
            method: Method::Key,
            tag: [0; 16],
            role: Role::Peer,
            name: long,
            proof: vec![],
            addrs: vec![],
            relay: None,
        };
        let mut out = vec![9];
        assert_eq!(Frame::Hello(h).encode(&mut out), Err(Error::TooLong));
        assert_eq!(out, [9]);
        assert_eq!(
            Frame::AppOsc(vec![0; MAX_BODY + 1]).encode(&mut out),
            Err(Error::TooLong)
        );
        assert_eq!(
            Frame::Roster(vec![entry(); MAX_ROSTER + 1]).encode(&mut out),
            Err(Error::TooLong)
        );
        let e = PeerEntry {
            addrs: vec!["1.1.1.1:1".parse().unwrap(); MAX_ADDRS + 1],
            ..entry()
        };
        assert_eq!(Frame::Roster(vec![e]).encode(&mut out), Err(Error::TooLong));
        assert_eq!(
            Frame::Subscribe(Subscribe {
                stream: 0,
                encoding: 0,
                redundancy: MAX_REDUNDANCY + 1
            })
            .encode(&mut out),
            Err(Error::TooLong)
        );
        // Decoder caps: name length byte over the cap, bad UTF-8, bad enum values.
        let mut b = vec![0];
        b.extend_from_slice(&[0; 16]);
        b.extend_from_slice(&[1, (MAX_NAME + 1) as u8]);
        assert_eq!(Frame::decode_body(T_HELLO, &b), Err(Error::TooLong));
        let n = b.len();
        b[n - 1] = 1;
        b.extend_from_slice(&[0xff, 0]);
        assert_eq!(Frame::decode_body(T_HELLO, &b), Err(Error::Invalid));
        b[0] = 2;
        assert_eq!(Frame::decode_body(T_HELLO, &b), Err(Error::Invalid));
        assert_eq!(
            Frame::decode_body(T_WELCOME, &[[0; 32].as_slice(), &[2]].concat()),
            Err(Error::Invalid)
        );
        let mut rng = Rng(99);
        for _ in 0..50_000 {
            let n = rng.below(120) as usize;
            let mut b: Vec<u8> = (0..n).map(|_| rng.next() as u8).collect();
            if n >= 3 {
                b[2] %= 12;
            }
            let _ = decode_frame(&b);
        }
    }
}
