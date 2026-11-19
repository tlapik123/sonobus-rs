//! Typed AOO messages (spec section 1) with byte-exact encode and zero-copy parse.
//!
//! Ids in `/aoo/sink/<id>/...` and `/aoo/src/<id>/...` addresses are `i32`; [`ID_WILDCARD`]
//! (`-1`) is written and read as `*`. Arguments that the C++ treats as optional on receive are
//! `Option`s; `None` is omitted on encode. Extra trailing arguments are ignored, as in the C++.

use crate::osc::{self, Arg};
use crate::{Error, ID_WILDCARD};
use std::fmt;

/// Resend requests of `/aoo/src/<id>/data`: `(sequence, frame)` pairs, frame `-1` = whole block.
#[derive(Debug, Clone, Copy)]
pub enum Requests<'a> {
    /// Built by the caller for encoding.
    Pairs(&'a [(i32, i32)]),
    /// Borrowed from a parsed packet: 8 bytes per pair, big-endian.
    Wire(&'a [u8]),
}

impl<'a> Requests<'a> {
    pub fn len(&self) -> usize {
        match self {
            Requests::Pairs(p) => p.len(),
            Requests::Wire(w) => w.len() / 8,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = (i32, i32)> + Clone + 'a {
        let this = *self;
        (0..this.len()).map(move |i| match this {
            Requests::Pairs(p) => p[i],
            Requests::Wire(w) => {
                let int = |o: usize| i32::from_be_bytes([w[o], w[o + 1], w[o + 2], w[o + 3]]);
                (int(i * 8), int(i * 8 + 4))
            }
        })
    }
}

impl PartialEq for Requests<'_> {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Message<'a> {
    // ---- source -> sink (UDP) ----
    /// `/aoo/sink/<sink>/format ,iiiiiisb[b]`
    SinkFormat {
        sink: i32,
        src: i32,
        version: u32,
        salt: i32,
        nchannels: i32,
        samplerate: i32,
        blocksize: i32,
        codec: &'a str,
        options: &'a [u8],
        /// Opaque app blob (SonoBus: JUCE ValueTree with the channel layout).
        user_format: Option<&'a [u8]>,
    },
    /// `/aoo/sink/<sink>/data ,iiidiiiib`
    SinkData {
        sink: i32,
        src: i32,
        salt: i32,
        seq: i32,
        samplerate: f64,
        channel: i32,
        total_size: i32,
        nframes: i32,
        frame: i32,
        data: &'a [u8],
    },
    /// `/d ,iib` or `/d ,iidb` (samplerate present).
    CompactData {
        salt: i32,
        seq: i32,
        samplerate: Option<f64>,
        data: &'a [u8],
    },
    /// `/aoo/sink/<sink>/ping ,it`
    SinkPing { sink: i32, src: i32, tt: u64 },

    // ---- sink -> source (UDP) ----
    /// `/aoo/src/<src>/format ,ii` (format request)
    SourceFormat { src: i32, sink: i32, version: u32 },
    /// `/aoo/src/<src>/data ,ii(ii)*` (resend request)
    SourceData {
        src: i32,
        sink: i32,
        salt: i32,
        requests: Requests<'a>,
    },
    /// `/aoo/src/<src>/ping ,itti`
    SourcePing {
        src: i32,
        sink: i32,
        tt1: u64,
        tt2: u64,
        lost_blocks: i32,
    },
    /// `/aoo/src/<src>/invite ,ii` (flags optional on receive, C++ treats missing as 0)
    SourceInvite {
        src: i32,
        sink: i32,
        flags: Option<i32>,
    },
    /// `/aoo/src/<src>/uninvite ,i`
    SourceUninvite { src: i32, sink: i32 },
    /// `/aoo/src/<src>/codecchange ,iiiisb`
    SourceCodecChange {
        src: i32,
        sink: i32,
        nchannels: i32,
        samplerate: i32,
        blocksize: i32,
        codec: &'a str,
        options: &'a [u8],
    },

    // ---- client -> server ----
    /// `/aoo/server/request ,` (UDP)
    ServerRequest,
    /// `/aoo/server/ping ,` (UDP and TCP)
    ServerPing,
    /// `/aoo/server/login ,sssisih` (TCP; token optional on receive)
    ServerLogin {
        user: &'a str,
        password: &'a str,
        public_ip: &'a str,
        public_port: i32,
        local_ip: &'a str,
        local_port: i32,
        token: Option<i64>,
    },
    /// `/aoo/server/group/join ,ssT|F` (TCP; flag optional on receive, missing = private)
    ServerGroupJoin {
        group: &'a str,
        password: &'a str,
        is_public: Option<bool>,
    },
    /// `/aoo/server/group/leave ,s` (TCP)
    ServerGroupLeave { group: &'a str },
    /// `/aoo/server/group/public ,T|F` (TCP)
    ServerGroupPublic { watch: bool },

    // ---- server -> client ----
    /// `/aoo/client/reply ,si` (UDP)
    ClientReply {
        public_ip: &'a str,
        public_port: i32,
    },
    /// `/aoo/client/ping ,` (UDP and TCP)
    ClientPing,
    /// `/aoo/client/login ,is` (error text optional on receive)
    ClientLogin { result: i32, error: Option<&'a str> },
    /// `/aoo/client/group/join ,sis` (error text optional on receive)
    ClientGroupJoin {
        group: &'a str,
        result: i32,
        error: Option<&'a str>,
    },
    /// `/aoo/client/group/leave ,sis` (error text optional on receive)
    ClientGroupLeave {
        group: &'a str,
        result: i32,
        error: Option<&'a str>,
    },
    /// `/aoo/client/group/public ,T|Fis` (the C++ client ignores it)
    ClientGroupPublic {
        watch: bool,
        result: i32,
        error: Option<&'a str>,
    },
    /// `/aoo/client/group/public/add ,si`
    ClientGroupPublicAdd { group: &'a str, users: i32 },
    /// `/aoo/client/group/public/del ,s`
    ClientGroupPublicDel { group: &'a str },
    /// `/aoo/client/peer/join ,sssisih` (token optional on receive)
    ClientPeerJoin {
        group: &'a str,
        user: &'a str,
        public_ip: &'a str,
        public_port: i32,
        local_ip: &'a str,
        local_port: i32,
        token: Option<i64>,
    },
    /// `/aoo/client/peer/leave ,ss`
    ClientPeerLeave { group: &'a str, user: &'a str },

    // ---- peer <-> peer (UDP) ----
    /// `/aoo/peer/ping ,h` during hole punching, `/aoo/peer/ping ,` as keepalive.
    PeerPing { token: Option<i64> },

    /// Not `/aoo/...` or `/d` (e.g. SonoBus `/sb/...`); the raw packet, unvalidated.
    NotAoo(&'a [u8]),
}

/// Address id: decimal, or `*` for the wildcard.
struct Id(i32);

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == ID_WILDCARD {
            f.write_str("*")
        } else {
            write!(f, "{}", self.0)
        }
    }
}

impl Message<'_> {
    /// Append the encoded packet to `out` (no allocation beyond growing `out`).
    pub fn encode(&self, out: &mut Vec<u8>) {
        use Arg::*;
        use Message as M;
        match *self {
            M::SinkFormat {
                sink,
                src,
                version,
                salt,
                nchannels,
                samplerate,
                blocksize,
                codec,
                options,
                user_format,
            } => osc::encode(
                out,
                format_args!("/aoo/sink/{}/format", Id(sink)),
                [
                    Int(src),
                    Int(version as i32),
                    Int(salt),
                    Int(nchannels),
                    Int(samplerate),
                    Int(blocksize),
                    Str(codec),
                    Blob(options),
                ]
                .into_iter()
                // C++ omits the blob when empty (source.cpp:831)
                .chain(user_format.filter(|b| !b.is_empty()).map(Blob)),
            ),
            M::SinkData {
                sink,
                src,
                salt,
                seq,
                samplerate,
                channel,
                total_size,
                nframes,
                frame,
                data,
            } => osc::encode(
                out,
                format_args!("/aoo/sink/{}/data", Id(sink)),
                [
                    Int(src),
                    Int(salt),
                    Int(seq),
                    Double(samplerate),
                    Int(channel),
                    Int(total_size),
                    Int(nframes),
                    Int(frame),
                    Blob(data),
                ],
            ),
            M::CompactData {
                salt,
                seq,
                samplerate,
                data,
            } => osc::encode(
                out,
                "/d",
                [Int(salt), Int(seq)]
                    .into_iter()
                    .chain(samplerate.map(Double))
                    .chain([Blob(data)]),
            ),
            M::SinkPing { sink, src, tt } => osc::encode(
                out,
                format_args!("/aoo/sink/{}/ping", Id(sink)),
                [Int(src), Time(tt)],
            ),
            M::SourceFormat { src, sink, version } => osc::encode(
                out,
                format_args!("/aoo/src/{}/format", Id(src)),
                [Int(sink), Int(version as i32)],
            ),
            M::SourceData {
                src,
                sink,
                salt,
                requests,
            } => osc::encode(
                out,
                format_args!("/aoo/src/{}/data", Id(src)),
                [Int(sink), Int(salt)]
                    .into_iter()
                    .chain(requests.iter().flat_map(|(s, f)| [Int(s), Int(f)])),
            ),
            M::SourcePing {
                src,
                sink,
                tt1,
                tt2,
                lost_blocks,
            } => osc::encode(
                out,
                format_args!("/aoo/src/{}/ping", Id(src)),
                [Int(sink), Time(tt1), Time(tt2), Int(lost_blocks)],
            ),
            M::SourceInvite { src, sink, flags } => osc::encode(
                out,
                format_args!("/aoo/src/{}/invite", Id(src)),
                [Int(sink)].into_iter().chain(flags.map(Int)),
            ),
            M::SourceUninvite { src, sink } => osc::encode(
                out,
                format_args!("/aoo/src/{}/uninvite", Id(src)),
                [Int(sink)],
            ),
            M::SourceCodecChange {
                src,
                sink,
                nchannels,
                samplerate,
                blocksize,
                codec,
                options,
            } => osc::encode(
                out,
                format_args!("/aoo/src/{}/codecchange", Id(src)),
                [
                    Int(sink),
                    Int(nchannels),
                    Int(samplerate),
                    Int(blocksize),
                    Str(codec),
                    Blob(options),
                ],
            ),
            M::ServerRequest => osc::encode(out, "/aoo/server/request", []),
            M::ServerPing => osc::encode(out, "/aoo/server/ping", []),
            M::ServerLogin {
                user,
                password,
                public_ip,
                public_port,
                local_ip,
                local_port,
                token,
            } => osc::encode(
                out,
                "/aoo/server/login",
                [
                    Str(user),
                    Str(password),
                    Str(public_ip),
                    Int(public_port),
                    Str(local_ip),
                    Int(local_port),
                ]
                .into_iter()
                .chain(token.map(Long)),
            ),
            M::ServerGroupJoin {
                group,
                password,
                is_public,
            } => osc::encode(
                out,
                "/aoo/server/group/join",
                [Str(group), Str(password)]
                    .into_iter()
                    .chain(is_public.map(Bool)),
            ),
            M::ServerGroupLeave { group } => {
                osc::encode(out, "/aoo/server/group/leave", [Str(group)])
            }
            M::ServerGroupPublic { watch } => {
                osc::encode(out, "/aoo/server/group/public", [Bool(watch)])
            }
            M::ClientReply {
                public_ip,
                public_port,
            } => osc::encode(out, "/aoo/client/reply", [Str(public_ip), Int(public_port)]),
            M::ClientPing => osc::encode(out, "/aoo/client/ping", []),
            M::ClientLogin { result, error } => osc::encode(
                out,
                "/aoo/client/login",
                [Int(result)].into_iter().chain(error.map(Str)),
            ),
            M::ClientGroupJoin {
                group,
                result,
                error,
            } => osc::encode(
                out,
                "/aoo/client/group/join",
                [Str(group), Int(result)].into_iter().chain(error.map(Str)),
            ),
            M::ClientGroupLeave {
                group,
                result,
                error,
            } => osc::encode(
                out,
                "/aoo/client/group/leave",
                [Str(group), Int(result)].into_iter().chain(error.map(Str)),
            ),
            M::ClientGroupPublic {
                watch,
                result,
                error,
            } => osc::encode(
                out,
                "/aoo/client/group/public",
                [Bool(watch), Int(result)].into_iter().chain(error.map(Str)),
            ),
            M::ClientGroupPublicAdd { group, users } => osc::encode(
                out,
                "/aoo/client/group/public/add",
                [Str(group), Int(users)],
            ),
            M::ClientGroupPublicDel { group } => {
                osc::encode(out, "/aoo/client/group/public/del", [Str(group)])
            }
            M::ClientPeerJoin {
                group,
                user,
                public_ip,
                public_port,
                local_ip,
                local_port,
                token,
            } => osc::encode(
                out,
                "/aoo/client/peer/join",
                [
                    Str(group),
                    Str(user),
                    Str(public_ip),
                    Int(public_port),
                    Str(local_ip),
                    Int(local_port),
                ]
                .into_iter()
                .chain(token.map(Long)),
            ),
            M::ClientPeerLeave { group, user } => {
                osc::encode(out, "/aoo/client/peer/leave", [Str(group), Str(user)])
            }
            M::PeerPing { token } => osc::encode(out, "/aoo/peer/ping", token.map(Long)),
            M::NotAoo(raw) => out.extend_from_slice(raw),
        }
    }
}

/// Typed argument reader; any missing or mistyped argument is [`Error::BadArgs`].
struct Reader<'a>(osc::Args<'a>);

impl<'a> Reader<'a> {
    fn get<T>(&mut self, f: fn(Arg<'a>) -> Option<T>) -> Result<T, Error> {
        self.0.next().and_then(f).ok_or(Error::BadArgs)
    }

    /// `None` if there are no more arguments; an argument of the wrong type is an error.
    fn opt<T>(&mut self, f: fn(Arg<'a>) -> Option<T>) -> Result<Option<T>, Error> {
        match self.0.next() {
            None => Ok(None),
            Some(a) => f(a).map(Some).ok_or(Error::BadArgs),
        }
    }
}

fn int(a: Arg) -> Option<i32> {
    if let Arg::Int(v) = a { Some(v) } else { None }
}
fn long(a: Arg) -> Option<i64> {
    if let Arg::Long(v) = a { Some(v) } else { None }
}
fn double(a: Arg) -> Option<f64> {
    if let Arg::Double(v) = a {
        Some(v)
    } else {
        None
    }
}
fn time(a: Arg) -> Option<u64> {
    if let Arg::Time(v) = a { Some(v) } else { None }
}
fn boolean(a: Arg) -> Option<bool> {
    if let Arg::Bool(v) = a { Some(v) } else { None }
}
fn string(a: Arg<'_>) -> Option<&str> {
    if let Arg::Str(v) = a { Some(v) } else { None }
}
fn blob(a: Arg<'_>) -> Option<&[u8]> {
    if let Arg::Blob(v) = a { Some(v) } else { None }
}

/// `/<int>` or `/*` followed by the sub-pattern (`aoo_parse_pattern`, `common.cpp:97-108`).
fn split_id(s: &str) -> Result<(i32, &str), Error> {
    let s = s.strip_prefix('/').ok_or(Error::BadAddress)?;
    if let Some(rest) = s.strip_prefix('*') {
        return Ok((ID_WILDCARD, rest));
    }
    let sign = usize::from(s.starts_with(['-', '+']));
    let end = s[sign..]
        .find(|c: char| !c.is_ascii_digit())
        .map_or(s.len(), |n| n + sign);
    let id = s[..end].parse().map_err(|_| Error::BadAddress)?;
    Ok((id, &s[end..]))
}

/// Parse one UDP datagram or SLIP frame.
pub fn parse(packet: &[u8]) -> Result<Message<'_>, Error> {
    if !(packet.starts_with(b"/aoo/") || packet.starts_with(b"/d\0")) {
        return Ok(Message::NotAoo(packet));
    }
    let msg = osc::decode(packet)?;
    let mut r = Reader(msg.args());
    let addr = msg.addr;

    if addr == "/d" {
        let salt = r.get(int)?;
        let seq = r.get(int)?;
        // `,iib` or `,iidb` (`sink.cpp:626-645`)
        return Ok(match r.get(Some)? {
            Arg::Double(sr) => Message::CompactData {
                salt,
                seq,
                samplerate: Some(sr),
                data: r.get(blob)?,
            },
            Arg::Blob(data) => Message::CompactData {
                salt,
                seq,
                samplerate: None,
                data,
            },
            _ => return Err(Error::BadArgs),
        });
    }

    let rest = &addr[4..]; // after "/aoo"
    if let Some(r2) = rest.strip_prefix("/sink") {
        let (sink, sub) = split_id(r2)?;
        return Ok(match sub {
            "/format" => Message::SinkFormat {
                sink,
                src: r.get(int)?,
                version: r.get(int)? as u32,
                salt: r.get(int)?,
                nchannels: r.get(int)?,
                samplerate: r.get(int)?,
                blocksize: r.get(int)?,
                codec: r.get(string)?,
                options: r.get(blob)?,
                user_format: r.opt(blob)?,
            },
            "/data" => Message::SinkData {
                sink,
                src: r.get(int)?,
                salt: r.get(int)?,
                seq: r.get(int)?,
                samplerate: r.get(double)?,
                channel: r.get(int)?,
                total_size: r.get(int)?,
                nframes: r.get(int)?,
                frame: r.get(int)?,
                data: r.get(blob)?,
            },
            "/ping" => Message::SinkPing {
                sink,
                src: r.get(int)?,
                tt: r.get(time)?,
            },
            _ => return Err(Error::UnknownMessage),
        });
    }
    if let Some(r2) = rest.strip_prefix("/src") {
        let (src, sub) = split_id(r2)?;
        return Ok(match sub {
            "/format" => Message::SourceFormat {
                src,
                sink: r.get(int)?,
                version: r.get(int)? as u32,
            },
            "/data" => {
                let sink = r.get(int)?;
                let salt = r.get(int)?;
                let (tags, data) = r.0.rest();
                if !tags.len().is_multiple_of(2) || tags.iter().any(|&t| t != b'i') {
                    return Err(Error::BadArgs);
                }
                // Validated by osc::decode: one 4-byte int per tag follows.
                let requests = Requests::Wire(&data[..tags.len() * 4]);
                Message::SourceData {
                    src,
                    sink,
                    salt,
                    requests,
                }
            }
            "/ping" => Message::SourcePing {
                src,
                sink: r.get(int)?,
                tt1: r.get(time)?,
                tt2: r.get(time)?,
                lost_blocks: r.get(int)?,
            },
            "/invite" => Message::SourceInvite {
                src,
                sink: r.get(int)?,
                flags: r.opt(int)?,
            },
            "/uninvite" => Message::SourceUninvite {
                src,
                sink: r.get(int)?,
            },
            "/codecchange" => Message::SourceCodecChange {
                src,
                sink: r.get(int)?,
                nchannels: r.get(int)?,
                samplerate: r.get(int)?,
                blocksize: r.get(int)?,
                codec: r.get(string)?,
                options: r.get(blob)?,
            },
            _ => return Err(Error::UnknownMessage),
        });
    }

    Ok(match rest {
        "/server/request" => Message::ServerRequest,
        "/server/ping" => Message::ServerPing,
        "/server/login" => Message::ServerLogin {
            user: r.get(string)?,
            password: r.get(string)?,
            public_ip: r.get(string)?,
            public_port: r.get(int)?,
            local_ip: r.get(string)?,
            local_port: r.get(int)?,
            token: r.opt(long)?,
        },
        "/server/group/join" => Message::ServerGroupJoin {
            group: r.get(string)?,
            password: r.get(string)?,
            is_public: r.opt(boolean)?,
        },
        "/server/group/leave" => Message::ServerGroupLeave {
            group: r.get(string)?,
        },
        "/server/group/public" => Message::ServerGroupPublic {
            watch: r.get(boolean)?,
        },
        "/client/reply" => Message::ClientReply {
            public_ip: r.get(string)?,
            public_port: r.get(int)?,
        },
        "/client/ping" => Message::ClientPing,
        "/client/login" => Message::ClientLogin {
            result: r.get(int)?,
            error: r.opt(string)?,
        },
        "/client/group/join" => Message::ClientGroupJoin {
            group: r.get(string)?,
            result: r.get(int)?,
            error: r.opt(string)?,
        },
        "/client/group/leave" => Message::ClientGroupLeave {
            group: r.get(string)?,
            result: r.get(int)?,
            error: r.opt(string)?,
        },
        "/client/group/public" => Message::ClientGroupPublic {
            watch: r.get(boolean)?,
            result: r.get(int)?,
            error: r.opt(string)?,
        },
        "/client/group/public/add" => Message::ClientGroupPublicAdd {
            group: r.get(string)?,
            users: r.get(int)?,
        },
        "/client/group/public/del" => Message::ClientGroupPublicDel {
            group: r.get(string)?,
        },
        "/client/peer/join" => Message::ClientPeerJoin {
            group: r.get(string)?,
            user: r.get(string)?,
            public_ip: r.get(string)?,
            public_port: r.get(int)?,
            local_ip: r.get(string)?,
            local_port: r.get(int)?,
            token: r.opt(long)?,
        },
        "/client/peer/leave" => Message::ClientPeerLeave {
            group: r.get(string)?,
            user: r.get(string)?,
        },
        "/peer/ping" => Message::PeerPing {
            token: r.opt(long)?,
        },
        _ => return Err(Error::UnknownMessage),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VERSION;

    const MD5_EMPTY: &str = "D41D8CD98F00B204E9800998ECF8427E";
    const OPUS_OPTS: &[u8] = &[
        0, 2, 0xee, 0, 0, 0, 0, 10, 0, 0, 0x0b, 0xba, 0, 0, 0x08, 0x03,
    ];

    /// Every message with its hand-derived bytes.
    fn goldens() -> Vec<(Message<'static>, &'static [u8])> {
        use Message as M;
        vec![
            (
                M::SinkFormat {
                    sink: 12,
                    src: 7,
                    version: VERSION,
                    salt: 0x1234_5678,
                    nchannels: 2,
                    samplerate: 48000,
                    blocksize: 256,
                    codec: "pcm",
                    options: &[0, 0, 0, 1],
                    user_format: None,
                },
                b"/aoo/sink/12/format\0,iiiiiisb\0\0\0\
                  \0\0\0\x07\x02\0\0\x01\x12\x34\x56\x78\0\0\0\x02\0\0\xbb\x80\0\0\x01\0\
                  pcm\0\0\0\0\x04\0\0\0\x01",
            ),
            (
                M::SinkFormat {
                    sink: 12,
                    src: 7,
                    version: VERSION,
                    salt: 0x1234_5678,
                    nchannels: 2,
                    samplerate: 48000,
                    blocksize: 256,
                    codec: "pcm",
                    options: &[0, 0, 0, 1],
                    user_format: Some(b"xyz"),
                },
                b"/aoo/sink/12/format\0,iiiiiisbb\0\0\
                  \0\0\0\x07\x02\0\0\x01\x12\x34\x56\x78\0\0\0\x02\0\0\xbb\x80\0\0\x01\0\
                  pcm\0\0\0\0\x04\0\0\0\x01\0\0\0\x03xyz\0",
            ),
            (
                M::SinkData {
                    sink: 12,
                    src: 7,
                    salt: 0x1234_5678,
                    seq: 5,
                    samplerate: 48000.0,
                    channel: 0,
                    total_size: 6,
                    nframes: 1,
                    frame: 0,
                    data: b"abcdef",
                },
                b"/aoo/sink/12/data\0\0\0,iiidiiiib\0\0\
                  \0\0\0\x07\x12\x34\x56\x78\0\0\0\x05\x40\xe7\x70\0\0\0\0\0\
                  \0\0\0\0\0\0\0\x06\0\0\0\x01\0\0\0\0\0\0\0\x06abcdef\0\0",
            ),
            (
                M::CompactData {
                    salt: 0x1234_5678,
                    seq: 9,
                    samplerate: None,
                    data: &[1, 2, 3, 4],
                },
                b"/d\0\0,iib\0\0\0\0\x12\x34\x56\x78\0\0\0\x09\0\0\0\x04\x01\x02\x03\x04",
            ),
            (
                M::CompactData {
                    salt: -2,
                    seq: 9,
                    samplerate: Some(44100.0),
                    data: &[1],
                },
                b"/d\0\0,iidb\0\0\0\xff\xff\xff\xfe\0\0\0\x09\x40\xe5\x88\x80\0\0\0\0\
                  \0\0\0\x01\x01\0\0\0",
            ),
            (
                M::SinkPing {
                    sink: 12,
                    src: 7,
                    tt: 0x0102_0304_0506_0708,
                },
                b"/aoo/sink/12/ping\0\0\0,it\0\0\0\0\x07\x01\x02\x03\x04\x05\x06\x07\x08",
            ),
            (
                M::SinkPing {
                    sink: ID_WILDCARD,
                    src: 7,
                    tt: 1,
                },
                b"/aoo/sink/*/ping\0\0\0\0,it\0\0\0\0\x07\0\0\0\0\0\0\0\x01",
            ),
            (
                M::SourceFormat {
                    src: 7,
                    sink: 12,
                    version: VERSION,
                },
                b"/aoo/src/7/format\0\0\0,ii\0\0\0\0\x0c\x02\0\0\x01",
            ),
            (
                M::SourceData {
                    src: 7,
                    sink: 12,
                    salt: 0x1234_5678,
                    requests: Requests::Pairs(&[(5, -1), (6, 2)]),
                },
                b"/aoo/src/7/data\0,iiiiii\0\0\0\0\x0c\x12\x34\x56\x78\
                  \0\0\0\x05\xff\xff\xff\xff\0\0\0\x06\0\0\0\x02",
            ),
            (
                M::SourceData {
                    src: 7,
                    sink: 12,
                    salt: 1,
                    requests: Requests::Pairs(&[]),
                },
                b"/aoo/src/7/data\0,ii\0\0\0\0\x0c\0\0\0\x01",
            ),
            (
                M::SourcePing {
                    src: 7,
                    sink: 12,
                    tt1: 0x0102_0304_0506_0708,
                    tt2: 0x1112_1314_1516_1718,
                    lost_blocks: 3,
                },
                b"/aoo/src/7/ping\0,itti\0\0\0\0\0\0\x0c\x01\x02\x03\x04\x05\x06\x07\x08\
                  \x11\x12\x13\x14\x15\x16\x17\x18\0\0\0\x03",
            ),
            (
                M::SourceInvite {
                    src: 0,
                    sink: 3,
                    flags: Some(1),
                },
                b"/aoo/src/0/invite\0\0\0,ii\0\0\0\0\x03\0\0\0\x01",
            ),
            (
                M::SourceInvite {
                    src: 7,
                    sink: 12,
                    flags: None,
                },
                b"/aoo/src/7/invite\0\0\0,i\0\0\0\0\0\x0c",
            ),
            (
                M::SourceUninvite { src: 7, sink: 12 },
                b"/aoo/src/7/uninvite\0,i\0\0\0\0\0\x0c",
            ),
            (
                M::SourceCodecChange {
                    src: 7,
                    sink: 12,
                    nchannels: 2,
                    samplerate: 48000,
                    blocksize: 480,
                    codec: "opus",
                    options: OPUS_OPTS,
                },
                b"/aoo/src/7/codecchange\0\0,iiiisb\0\
                  \0\0\0\x0c\0\0\0\x02\0\0\xbb\x80\0\0\x01\xe0opus\0\0\0\0\
                  \0\0\0\x10\0\x02\xee\0\0\0\0\x0a\0\0\x0b\xba\0\0\x08\x03",
            ),
            (M::ServerRequest, b"/aoo/server/request\0,\0\0\0"),
            (M::ServerPing, b"/aoo/server/ping\0\0\0\0,\0\0\0"),
            (
                M::ServerLogin {
                    user: "alice",
                    password: MD5_EMPTY,
                    public_ip: "1.2.3.4",
                    public_port: 50000,
                    local_ip: "192.168.1.2",
                    local_port: 50001,
                    token: Some(0x1122_3344_5566_7788),
                },
                b"/aoo/server/login\0\0\0,sssisih\0\0\0\0alice\0\0\0\
                  D41D8CD98F00B204E9800998ECF8427E\0\0\0\0\
                  1.2.3.4\0\0\0\xc3\x50192.168.1.2\0\0\0\xc3\x51\
                  \x11\x22\x33\x44\x55\x66\x77\x88",
            ),
            (
                M::ServerGroupJoin {
                    group: "band",
                    password: MD5_EMPTY,
                    is_public: Some(false),
                },
                b"/aoo/server/group/join\0\0,ssF\0\0\0\0band\0\0\0\0\
                  D41D8CD98F00B204E9800998ECF8427E\0\0\0\0",
            ),
            (
                M::ServerGroupJoin {
                    group: "band",
                    password: "",
                    is_public: Some(true),
                },
                b"/aoo/server/group/join\0\0,ssT\0\0\0\0band\0\0\0\0\0\0\0\0",
            ),
            (
                M::ServerGroupLeave { group: "band" },
                b"/aoo/server/group/leave\0,s\0\0band\0\0\0\0",
            ),
            (
                M::ServerGroupPublic { watch: true },
                b"/aoo/server/group/public\0\0\0\0,T\0\0",
            ),
            (
                M::ClientReply {
                    public_ip: "1.2.3.4",
                    public_port: 50000,
                },
                b"/aoo/client/reply\0\0\0,si\x001.2.3.4\0\0\0\xc3\x50",
            ),
            (M::ClientPing, b"/aoo/client/ping\0\0\0\0,\0\0\0"),
            (
                M::ClientLogin {
                    result: 1,
                    error: Some(""),
                },
                b"/aoo/client/login\0\0\0,is\0\0\0\0\x01\0\0\0\0",
            ),
            (
                M::ClientGroupJoin {
                    group: "band",
                    result: 0,
                    error: Some("wrong password"),
                },
                b"/aoo/client/group/join\0\0,sis\0\0\0\0band\0\0\0\0\0\0\0\0\
                  wrong password\0\0",
            ),
            (
                M::ClientGroupLeave {
                    group: "band",
                    result: 1,
                    error: Some(""),
                },
                b"/aoo/client/group/leave\0,sis\0\0\0\0band\0\0\0\0\0\0\0\x01\0\0\0\0",
            ),
            (
                M::ClientGroupPublic {
                    watch: false,
                    result: 0,
                    error: Some(""),
                },
                b"/aoo/client/group/public\0\0\0\0,Fis\0\0\0\0\0\0\0\0\0\0\0\0",
            ),
            (
                M::ClientGroupPublicAdd {
                    group: "band",
                    users: 3,
                },
                b"/aoo/client/group/public/add\0\0\0\0,si\0band\0\0\0\0\0\0\0\x03",
            ),
            (
                M::ClientGroupPublicDel { group: "band" },
                b"/aoo/client/group/public/del\0\0\0\0,s\0\0band\0\0\0\0",
            ),
            (
                M::ClientPeerJoin {
                    group: "band",
                    user: "bob",
                    public_ip: "5.6.7.8",
                    public_port: 40000,
                    local_ip: "10.0.0.5",
                    local_port: 40001,
                    token: Some(0x1122_3344_5566_7788),
                },
                b"/aoo/client/peer/join\0\0\0,sssisih\0\0\0\0band\0\0\0\0bob\0\
                  5.6.7.8\0\0\0\x9c\x4010.0.0.5\0\0\0\0\0\0\x9c\x41\
                  \x11\x22\x33\x44\x55\x66\x77\x88",
            ),
            (
                M::ClientPeerLeave {
                    group: "band",
                    user: "bob",
                },
                b"/aoo/client/peer/leave\0\0,ss\0band\0\0\0\0bob\0",
            ),
            (
                M::PeerPing {
                    token: Some(0x1122_3344_5566_7788),
                },
                b"/aoo/peer/ping\0\0,h\0\0\x11\x22\x33\x44\x55\x66\x77\x88",
            ),
            (M::PeerPing { token: None }, b"/aoo/peer/ping\0\0,\0\0\0"),
            (
                M::NotAoo(b"/sb/ping\0\0\0\0,\0\0\0"),
                b"/sb/ping\0\0\0\0,\0\0\0",
            ),
        ]
    }

    #[test]
    fn golden_encode() {
        for (msg, bytes) in goldens() {
            let mut out = Vec::new();
            msg.encode(&mut out);
            assert_eq!(out, bytes, "{msg:?}");
        }
    }

    #[test]
    fn golden_parse_round_trip() {
        let mut out = Vec::new();
        for (msg, bytes) in goldens() {
            let parsed = parse(bytes).unwrap_or_else(|e| panic!("{e} for {msg:?}"));
            assert_eq!(parsed, msg);
            out.clear();
            parsed.encode(&mut out);
            assert_eq!(out, bytes, "{msg:?}");
        }
    }

    #[test]
    fn requests_from_wire() {
        let Ok(Message::SourceData { requests, .. }) = parse(goldens()[8].1) else {
            panic!()
        };
        assert!(matches!(requests, Requests::Wire(_)));
        assert_eq!(requests.iter().collect::<Vec<_>>(), [(5, -1), (6, 2)]);
    }

    #[test]
    fn truncation_and_garbage_never_panic() {
        for (msg, bytes) in goldens() {
            for n in 0..bytes.len() {
                match parse(&bytes[..n]) {
                    Ok(Message::NotAoo(_)) | Err(_) => {}
                    Ok(m) => panic!("prefix {n} of {msg:?} parsed as {m:?}"),
                }
            }
        }
        // deterministic pseudo-random garbage, with and without an AOO-looking prefix
        let mut x: u32 = 0x1234_5678;
        let mut rnd = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        };
        for len in 0..200 {
            for prefix in [
                &b""[..],
                b"/aoo/sink/1/data\0\0\0\0,",
                b"/d\0\0,",
                b"/aoo/src/",
            ] {
                let mut p = prefix.to_vec();
                p.extend((0..len).map(|_| rnd()));
                let _ = parse(&p);
            }
        }
        // corrupt every byte of every golden packet
        for (_, bytes) in goldens() {
            for i in 0..bytes.len() {
                for v in [0u8, 0xff, 0x80, b'b', b'/'] {
                    let mut p = bytes.to_vec();
                    p[i] = v;
                    let _ = parse(&p);
                }
            }
        }
    }

    #[test]
    fn address_rules() {
        use Error::*;
        let cases: &[(&[u8], Result<(), Error>)] = &[
            (b"/aoo/sink/x/ping\0\0\0\0,\0\0\0", Err(BadAddress)),
            (b"/aoo/sink/99999999999/ping\0\0,\0\0\0", Err(BadAddress)),
            (b"/aoo/sink/1/pingx\0\0\0,\0\0\0", Err(UnknownMessage)),
            (b"/aoo/sink1/ping\0,\0\0\0", Err(BadAddress)),
            (b"/aoo/foo\0\0\0\0,\0\0\0", Err(UnknownMessage)),
            (
                b"/aoo/sink/1/ping\0\0\0\0,ii\0\0\0\0\x07\0\0\0\x01",
                Err(BadArgs),
            ),
            (b"/d\0\0,ii\0\0\0\0\x01\0\0\0\x02", Err(BadArgs)),
            (
                b"/aoo/src/1/data\0,iii\0\0\0\0\0\0\0\x01\0\0\0\x02\0\0\0\x03",
                Err(BadArgs),
            ),
        ];
        for (input, want) in cases {
            assert_eq!(parse(input).map(|_| ()), *want, "{input:?}");
        }
        // `/dx` and bundles are not AOO
        assert!(matches!(parse(b"/dx\0,\0\0\0"), Ok(Message::NotAoo(_))));
        assert!(matches!(parse(b"#bundle\0"), Ok(Message::NotAoo(_))));
        // negative ids and trailing extra arguments are accepted, as in the C++
        assert_eq!(
            parse(b"/aoo/src/-5/uninvite\0\0\0\0,ii\0\0\0\0\x01\0\0\0\x02"),
            Ok(Message::SourceUninvite { src: -5, sink: 1 })
        );
    }
}
