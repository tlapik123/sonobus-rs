//! AOO protocol as spoken by SonoBus 1.7.x (legacy compatibility mode).
//!
//! Wire layer: OSC codec ([`osc`]), typed AOO messages ([`message`]), SLIP framing ([`slip`]) and
//! the password hash. Connection layer: [`client`], [`server`]. Audio streaming: [`source`] and
//! [`sink`] with the PCM/Opus block codecs ([`codec`]). Nothing here owns a socket; audio and
//! network halves of a stream are separate handles (see [`source`], [`sink`]).

pub mod client;
pub mod codec;
pub mod message;
pub mod opus;
pub mod osc;
pub mod resampler;
pub mod server;
pub mod sink;
pub mod slip;
pub mod source;
pub mod time;

pub use message::Message;
pub use time::TimeTag;

/// Audio device configuration of a source or sink (`aoo_source_setup` / `aoo_sink_setup`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Setup {
    pub samplerate: i32,
    /// Frames per audio callback (callbacks may be shorter, never longer).
    pub blocksize: i32,
    pub nchannels: i32,
}

/// Encode `msg` into `buf` and hand it to `out` unless it exceeds [`MAX_PACKET_SIZE`].
pub(crate) fn emit(
    buf: &mut Vec<u8>,
    out: &mut impl FnMut(std::net::SocketAddr, &[u8]),
    to: std::net::SocketAddr,
    msg: &Message,
) {
    buf.clear();
    msg.encode(buf);
    if buf.len() <= MAX_PACKET_SIZE {
        out(to, buf);
    }
}

/// Wrap-safe sequence distance `a - b` (positive if `a` is later).
pub(crate) fn seq_diff(a: i32, b: i32) -> i32 {
    a.wrapping_sub(b)
}

/// AOO version this crate speaks (`aoo.h:14-16`).
pub const VERSION_MAJOR: u8 = 2;
pub const VERSION_MINOR: u8 = 0;
pub const VERSION_BUGFIX: u8 = 0;
/// Protocol flag bit 0: the peer understands the compact `/d` data message (`aoo.h:20`).
pub const PROTOCOL_FLAG_COMPACT_DATA: u8 = 0x1;
/// The version int every 1.7.x source sends: `0x02000001` (`source.cpp:828`).
pub const VERSION: u32 = make_version(PROTOCOL_FLAG_COMPACT_DATA);

/// Id that addresses every sink/source (`/aoo/sink/*/...`) (`aoo.h:133`).
pub const ID_WILDCARD: i32 = -1;
/// "No id" marker, used by the C++ for `/d` (`aoo.h:134`).
pub const ID_NONE: i32 = i32::MIN;

/// Largest OSC packet sent or accepted, UDP datagram and decoded SLIP frame (`aoo_types.h:39`).
pub const MAX_PACKET_SIZE: usize = 4096;
/// Largest codec options blob (`aoo.h:658`).
pub const CODEC_MAX_SETTINGS_SIZE: usize = 256;

/// `major << 24 | minor << 16 | bugfix << 8 | flags` (`common.cpp:33-37`).
pub const fn make_version(protocol_flags: u8) -> u32 {
    (VERSION_MAJOR as u32) << 24
        | (VERSION_MINOR as u32) << 16
        | (VERSION_BUGFIX as u32) << 8
        | protocol_flags as u32
}

/// Receivers only check the major version (`common.cpp:21-31`).
pub const fn check_version(version: u32) -> bool {
    (version >> 24) as u8 == VERSION_MAJOR
}

/// Low byte of the version int = the peer's protocol flags (`sink.cpp:830`, `source.cpp:1323`).
pub const fn version_flags(version: u32) -> u8 {
    version as u8
}

/// Password as sent on the wire: uppercase hex MD5 of the UTF-8 plaintext (`client.cpp:69-82`).
pub fn password_hash(plain: &str) -> String {
    use md5::{Digest, Md5};
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    let mut s = String::with_capacity(32);
    for b in Md5::digest(plain.as_bytes()) {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

/// Decode errors. Encoding never fails.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Packet ends before a string, blob, argument or the type tag string is complete.
    Truncated,
    /// String not valid UTF-8.
    BadString,
    /// Blob size negative or larger than the rest of the packet.
    BadBlob,
    /// Type tag string missing its leading `,`, or a tag this codec does not know.
    BadTypeTag,
    /// OSC address does not start with `/`, or an AOO address with a malformed id.
    BadAddress,
    /// AOO message with missing or wrongly typed arguments.
    BadArgs,
    /// `/aoo/...` address that is not part of the protocol.
    UnknownMessage,
    /// Codec options blob too short or with an unknown value.
    BadOptions,
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for Error {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_empty() {
        assert_eq!(password_hash(""), "D41D8CD98F00B204E9800998ECF8427E");
        assert_eq!(password_hash("abc"), "900150983CD24FB0D6963F7D28E17F72");
    }

    #[test]
    fn version() {
        assert_eq!(VERSION, 0x0200_0001);
        assert_eq!(VERSION as i32, 33554433);
        assert!(check_version(VERSION));
        assert!(check_version(0x02ff_ff00));
        assert!(!check_version(0x0100_0001));
        assert!(!check_version(0x0300_0001));
        assert_eq!(version_flags(VERSION), PROTOCOL_FLAG_COMPACT_DATA);
        assert_eq!(make_version(0), 0x0200_0000);
    }
}
