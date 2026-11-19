//! SonoBus peer-to-peer app messages (`/sb/...`), byte-compatible with SonoBus 1.7.2.
//!
//! They are plain OSC datagrams on the AOO socket; [`aoo::message::parse`] hands them over as
//! `Message::NotAoo`. Upstream (Source/SonobusPluginProcessor.cpp, "P.cpp" below) dispatches them
//! in `handleOtherMessage` (P.cpp:2561-2859) after `sonobusOscParsePattern` (P.cpp:2476-2559)
//! matched the raw packet bytes.
//!
//! | address           | tags  | send            | receive        |
//! |-------------------|-------|-----------------|----------------|
//! | `/sb/ping`        | `t`   | P.cpp:3411      | P.cpp:2575     |
//! | `/sb/pngack`      | `tt`  | P.cpp:2589      | P.cpp:2603     |
//! | `/sb/pinfo`       | `b`   | P.cpp:3174-3222 | P.cpp:2615, 3136-3172 |
//! | `/sb/clayinfo`    | `ib`  | P.cpp:6420-6466 | P.cpp:2648     |
//! | `/sb/chat`        | `sssss` | P.cpp:2862-2902 | P.cpp:2693   |
//! | `/sb/reqlatinfo`  | none  | P.cpp:2974-2997 | P.cpp:2713     |
//! | `/sb/latinfo`     | `b`   | P.cpp:2720-2743, 2929-2950 | P.cpp:2749, 2904-2927 |
//! | `/sb/suggestlat`  | `sf`  | P.cpp:2999-3027 | P.cpp:2771     |
//! | `/sb/blockedinfo` | `sT` / `sF` | P.cpp:2952-2971 | P.cpp:2816 |
//! | `/sb/suggestgroup`| `b`   | P.cpp:3089-3133 | P.cpp:2783     |
//!
//! Receivers read arguments with oscpack's typed accessors, so the leading tags must match exactly;
//! extra trailing arguments are ignored. JSON payloads are written by JUCE
//! `JSON::toString(v, allOnOneLine = true)` and read by `JSON::parse`; the private `json` module
//! documents the exact format reproduced here. Values of an unexpected JSON type are coerced as
//! JUCE's `var` does (strings through `getIntValue`/`getDoubleValue`, so "2.9" is 2 and true).

use crate::layout::ChannelLayouts;
use crate::valuetree::ValueTree;
use aoo::osc::{self, Arg};

/// Largest datagram upstream builds (its `OutboundPacketStream` buffer is `AOO_MAXPACKETSIZE`).
pub const MAX_PACKET_SIZE: usize = aoo::MAX_PACKET_SIZE;
/// Upstream refuses to send a JSON or ValueTree payload larger than this
/// (P.cpp:2727, 3109, 3201, 6440: `> AOO_MAXPACKETSIZE - 100`).
pub const MAX_PAYLOAD: usize = MAX_PACKET_SIZE - 100;

/// `nettype` values in `/sb/pinfo` (P.cpp:238-243). Stock 1.7.2 never sends the key.
pub const NET_TYPE_UNKNOWN: i32 = 0;
pub const NET_TYPE_ETHERNET: i32 = 1;
pub const NET_TYPE_WIFI: i32 = 2;
pub const NET_TYPE_MOBILE_DATA: i32 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Malformed OSC.
    Osc(aoo::Error),
    /// Missing or wrongly typed arguments (oscpack would throw).
    BadArgs,
    /// JSON payload upstream's `JSON::parse` would reject, or one of the wrong shape.
    BadJson,
    /// `/sb/clayinfo` tree unreadable or invalid.
    BadLayout,
    /// Encoding: payload above [`MAX_PAYLOAD`] or message above [`MAX_PACKET_SIZE`]; upstream
    /// silently does not send these.
    TooBig,
}

impl From<aoo::Error> for Error {
    fn from(e: aoo::Error) -> Self {
        Error::Osc(e)
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self, f)
    }
}

impl std::error::Error for Error {}

/// `/sb/pinfo` JSON. Every key is optional: the receiver only updates what is present
/// (`hasProperty` checks, P.cpp:3144-3168) and marks the peer as having remote info even when the
/// payload is empty or not an object.
///
/// Extension point: stock receivers look up these names only (`hasProperty`/`getProperty`,
/// P.cpp:3144-3168) and ignore every other key, so a Rust client can advertise extra
/// capabilities here as new keys. The 3996-byte payload limit still applies.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PeerInfo {
    /// `jitbuf`: sender's receive (jitter) buffer for this peer in ms, at least one block.
    pub jitbuf_ms: Option<f64>,
    /// `inlat`: upstream sends one block in ms.
    pub in_latency_ms: Option<f64>,
    /// `outlat`: upstream sends one block in ms.
    pub out_latency_ms: Option<f64>,
    /// `rec`: sender is recording to file.
    pub recording: Option<bool>,
    /// `nettype`: `NET_TYPE_*`. Read by stock receivers, never sent by them.
    pub net_type: Option<i32>,
    /// `supportedcodecs` (PR #201, not in stock 1.7.2): the codec UUIDs the sender can decode,
    /// as JUCE `Uuid::toString()` writes them (32 lowercase hex digits, no dashes). `None`: the
    /// key is absent (written so, our output stays byte-identical to stock); the receiver then
    /// assumes the legacy 8 Opus + 3 PCM formats. Parsing keeps at most
    /// [`MAX_SUPPORTED_CODECS`] string entries, normalized with [`codec_id`]; a value that is
    /// not an array is `None`.
    pub supported_codecs: Option<Vec<String>>,
}

/// Entries of `supportedcodecs` kept when parsing.
pub const MAX_SUPPORTED_CODECS: usize = 64;

/// A codec UUID in the form JUCE `Uuid::toString()` writes: 32 lowercase hex digits. Dashed or
/// upper-case UUIDs map to that; anything else is kept as given (an unknown codec).
pub fn codec_id(s: &str) -> String {
    let hex: String = s.chars().filter(|&c| c != '-').collect();
    if hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        hex.to_ascii_lowercase()
    } else {
        s.to_owned()
    }
}

/// One `/sb/latinfo` entry: measured one-way latency from `src_name` to `dest_name`.
///
/// Extension point: receivers read only these three keys per entry (P.cpp:2917-2919).
#[derive(Debug, Clone, PartialEq)]
pub struct LatEntry {
    /// `srcname`: the remote peer.
    pub src_name: String,
    /// `destname`: the sender itself.
    pub dest_name: String,
    /// `latms`: incoming latency in ms.
    pub latency_ms: f64,
}

/// `/sb/suggestgroup` JSON: an invitation to move to another group.
///
/// Extension point: receivers read only these keys (P.cpp:2800-2804).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SuggestGroup {
    /// `user`: the inviting user.
    pub user: String,
    /// `group`.
    pub group: String,
    /// `group_pass`: plain-text group password.
    pub group_password: String,
    /// `public`.
    pub public: bool,
    /// `others`: usernames being invited.
    pub others: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SbMessage<'a> {
    /// `/sb/ping t`: sent every 2 s; answer with [`SbMessage::PingAck`] at once.
    Ping { origin_time: u64 },
    /// `/sb/pngack tt`: the ping's time echoed, then the responder's clock.
    PingAck { origin_time: u64, reply_time: u64 },
    /// `/sb/pinfo b`.
    PeerInfo(PeerInfo),
    /// `/sb/clayinfo ib`: the sender's outgoing channel groups. `source_id` is the sender's
    /// `remoteSinkId` for this peer, i.e. the receiver's own sink id (P.cpp:2652, 6451).
    /// Groups are read starting from [`ChannelLayout::default`](crate::layout::ChannelLayout);
    /// upstream overlays them on the receiver's current params instead.
    ChannelLayoutInfo {
        source_id: i32,
        layouts: ChannelLayouts,
    },
    /// `/sb/chat sssss`. `targets` is `|`-separated usernames, empty for everyone; `tags` is
    /// always empty in 1.7.2.
    Chat {
        group: &'a str,
        from: &'a str,
        targets: &'a str,
        tags: &'a str,
        message: &'a str,
    },
    /// `/sb/reqlatinfo`: answer with [`SbMessage::LatInfo`].
    ReqLatInfo,
    /// `/sb/latinfo b`: JSON array. Parsing drops entries with an empty name, as the receiver
    /// does (P.cpp:2921), and yields an empty list for a payload that is not an array.
    LatInfo(Vec<LatEntry>),
    /// `/sb/suggestlat sf`: `from` asks everyone to match `latency_ms`.
    SuggestLatency { from: &'a str, latency_ms: f32 },
    /// `/sb/blockedinfo s(T|F)`: `from` has (un)blocked us.
    BlockedInfo { from: &'a str, blocked: bool },
    /// `/sb/suggestgroup b`. Parsing fails for a payload that is not an object (the receiver
    /// ignores those, P.cpp:2799).
    SuggestGroup(SuggestGroup),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Ping,
    PingAck,
    PeerInfo,
    Layout,
    Chat,
    ReqLatInfo,
    LatInfo,
    SuggestLat,
    BlockedInfo,
    SuggestGroup,
}

/// `sonobusOscParsePattern` (P.cpp:2418-2559): `memcmp` of the raw packet after `/sb`, first match
/// wins. Three of upstream's `_LEN` constants count the terminating NUL, so those addresses must
/// match exactly; the rest are prefixes (`/sb/pingfoo` is a ping).
const PATTERNS: [(&[u8], Kind); 10] = [
    (b"/ping", Kind::Ping),
    (b"/pngack", Kind::PingAck),
    (b"/pinfo", Kind::PeerInfo),
    (b"/clayinfo", Kind::Layout),
    (b"/chat", Kind::Chat),
    (b"/reqlatinfo\0", Kind::ReqLatInfo),
    (b"/latinfo", Kind::LatInfo),
    (b"/suggestlat", Kind::SuggestLat),
    (b"/blockedinfo\0", Kind::BlockedInfo),
    (b"/suggestgroup\0", Kind::SuggestGroup),
];

fn kind(packet: &[u8]) -> Option<Kind> {
    let rest = packet.strip_prefix(b"/sb")?;
    PATTERNS
        .iter()
        .find(|(p, _)| rest.starts_with(p))
        .map(|&(_, k)| k)
}

impl SbMessage<'_> {
    /// Append the datagram to `out`. On error `out` is left unchanged.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        let start = out.len();
        let r = self.encode_inner(out).and_then(|()| {
            if out.len() - start > MAX_PACKET_SIZE {
                Err(Error::TooBig)
            } else {
                Ok(())
            }
        });
        if r.is_err() {
            out.truncate(start);
        }
        r
    }

    fn encode_inner(&self, out: &mut Vec<u8>) -> Result<(), Error> {
        use SbMessage as M;
        match self {
            M::Ping { origin_time } => osc::encode(out, "/sb/ping", [Arg::Time(*origin_time)]),
            M::PingAck {
                origin_time,
                reply_time,
            } => osc::encode(
                out,
                "/sb/pngack",
                [Arg::Time(*origin_time), Arg::Time(*reply_time)],
            ),
            M::PeerInfo(p) => {
                // Key order of sendRemotePeerInfoUpdate: inlat, outlat, rec, then jitbuf
                // (P.cpp:3180-3197); PR #201 puts supportedcodecs after rec.
                let num = |k: &str, v: Option<f64>| v.map(|v| (k.to_owned(), json::Json::Num(v)));
                let codecs = p.supported_codecs.as_ref().map(|c| {
                    let ids = c.iter().cloned().map(json::Json::Str).collect();
                    ("supportedcodecs".to_owned(), json::Json::Arr(ids))
                });
                let props = [
                    num("inlat", p.in_latency_ms),
                    num("outlat", p.out_latency_ms),
                    p.recording.map(|v| ("rec".to_owned(), json::Json::Bool(v))),
                    codecs,
                    num("jitbuf", p.jitbuf_ms),
                    p.net_type
                        .map(|v| ("nettype".to_owned(), json::Json::Int(v.into()))),
                ];
                json_blob(
                    out,
                    "/sb/pinfo",
                    &json::Json::Obj(props.into_iter().flatten().collect()),
                )?
            }
            M::ChannelLayoutInfo { source_id, layouts } => {
                let mut tree = Vec::new();
                layouts.to_tree().write(&mut tree);
                if tree.len() > MAX_PAYLOAD {
                    return Err(Error::TooBig);
                }
                osc::encode(
                    out,
                    "/sb/clayinfo",
                    [Arg::Int(*source_id), Arg::Blob(&tree)],
                );
            }
            M::Chat {
                group,
                from,
                targets,
                tags,
                message,
            } => osc::encode(
                out,
                "/sb/chat",
                [group, from, targets, tags, message].map(|s| Arg::Str(s)),
            ),
            M::ReqLatInfo => osc::encode(out, "/sb/reqlatinfo", []),
            M::LatInfo(entries) => {
                // Upstream writes "null" for an empty list (a void var, P.cpp:2931), which its own
                // receiver rejects; "[]" means the same and parses.
                let list = entries
                    .iter()
                    .map(|e| {
                        json::Json::Obj(vec![
                            ("srcname".into(), json::Json::Str(e.src_name.clone())),
                            ("destname".into(), json::Json::Str(e.dest_name.clone())),
                            ("latms".into(), json::Json::Num(e.latency_ms)),
                        ])
                    })
                    .collect();
                json_blob(out, "/sb/latinfo", &json::Json::Arr(list))?
            }
            M::SuggestLatency { from, latency_ms } => osc::encode(
                out,
                "/sb/suggestlat",
                [Arg::Str(from), Arg::Float(*latency_ms)],
            ),
            M::BlockedInfo { from, blocked } => osc::encode(
                out,
                "/sb/blockedinfo",
                [Arg::Str(from), Arg::Bool(*blocked)],
            ),
            M::SuggestGroup(g) => {
                // Key order of suggestNewGroupToPeers (P.cpp:3096-3101).
                let obj = json::Json::Obj(vec![
                    ("user".into(), json::Json::Str(g.user.clone())),
                    ("group".into(), json::Json::Str(g.group.clone())),
                    (
                        "group_pass".into(),
                        json::Json::Str(g.group_password.clone()),
                    ),
                    ("public".into(), json::Json::Bool(g.public)),
                    (
                        "others".into(),
                        json::Json::Arr(g.others.iter().cloned().map(json::Json::Str).collect()),
                    ),
                ]);
                json_blob(out, "/sb/suggestgroup", &obj)?
            }
        }
        Ok(())
    }
}

fn json_blob(out: &mut Vec<u8>, addr: &str, v: &json::Json) -> Result<(), Error> {
    let mut s = String::new();
    v.write(&mut s);
    if s.len() > MAX_PAYLOAD {
        return Err(Error::TooBig);
    }
    osc::encode(out, addr, [Arg::Blob(s.as_bytes())]);
    Ok(())
}

/// Parse a datagram. `Ok(None)` when upstream's dispatcher would not claim it (no known `/sb/`
/// address prefix, see `PATTERNS`); such packets are not checked for OSC validity, as upstream.
pub fn parse(packet: &[u8]) -> Result<Option<SbMessage<'_>>, Error> {
    let Some(kind) = kind(packet) else {
        return Ok(None);
    };
    let msg = osc::decode(packet)?;
    let mut a = msg.args();
    let a = &mut a;
    Ok(Some(match kind {
        Kind::Ping => SbMessage::Ping {
            origin_time: next(a, time)?,
        },
        Kind::PingAck => SbMessage::PingAck {
            origin_time: next(a, time)?,
            reply_time: next(a, time)?,
        },
        Kind::PeerInfo => {
            let v = json::parse(next(a, blob)?)?;
            SbMessage::PeerInfo(PeerInfo {
                jitbuf_ms: v.get("jitbuf").map(json::Json::to_f64),
                in_latency_ms: v.get("inlat").map(json::Json::to_f64),
                out_latency_ms: v.get("outlat").map(json::Json::to_f64),
                recording: v.get("rec").map(json::Json::to_bool),
                net_type: v.get("nettype").map(json::Json::to_i32),
                supported_codecs: match v.get("supportedcodecs") {
                    Some(json::Json::Arr(items)) => Some(
                        items
                            .iter()
                            .filter_map(|e| match e {
                                json::Json::Str(s) => Some(codec_id(s)),
                                _ => None,
                            })
                            .take(MAX_SUPPORTED_CODECS)
                            .collect(),
                    ),
                    _ => None,
                },
            })
        }
        Kind::Layout => {
            let source_id = next(a, int)?;
            let tree = ValueTree::read(next(a, blob)?).map_err(|_| Error::BadLayout)?;
            SbMessage::ChannelLayoutInfo {
                source_id,
                layouts: ChannelLayouts::from_tree(&tree).ok_or(Error::BadLayout)?,
            }
        }
        Kind::Chat => SbMessage::Chat {
            group: next(a, string)?,
            from: next(a, string)?,
            targets: next(a, string)?,
            tags: next(a, string)?,
            message: next(a, string)?,
        },
        Kind::ReqLatInfo => SbMessage::ReqLatInfo,
        Kind::LatInfo => {
            let v = json::parse(next(a, blob)?)?;
            let items = match &v {
                json::Json::Arr(items) => items.as_slice(),
                _ => &[],
            };
            let field =
                |e: &json::Json, k| e.get(k).map(json::Json::to_juce_string).unwrap_or_default();
            SbMessage::LatInfo(
                items
                    .iter()
                    .map(|e| LatEntry {
                        src_name: field(e, "srcname"),
                        dest_name: field(e, "destname"),
                        latency_ms: e.get("latms").map_or(0.0, json::Json::to_f64),
                    })
                    .filter(|e| !e.src_name.is_empty() && !e.dest_name.is_empty())
                    .collect(),
            )
        }
        Kind::SuggestLat => SbMessage::SuggestLatency {
            from: next(a, string)?,
            latency_ms: next(a, float)?,
        },
        Kind::BlockedInfo => SbMessage::BlockedInfo {
            from: next(a, string)?,
            blocked: next(a, boolean)?,
        },
        Kind::SuggestGroup => {
            let v = json::parse(next(a, blob)?)?;
            if !matches!(v, json::Json::Obj(_)) {
                return Err(Error::BadJson);
            }
            let s = |k| v.get(k).map(json::Json::to_juce_string).unwrap_or_default();
            SbMessage::SuggestGroup(SuggestGroup {
                user: s("user"),
                group: s("group"),
                group_password: s("group_pass"),
                public: v.get("public").is_some_and(json::Json::to_bool),
                others: match v.get("others") {
                    Some(json::Json::Arr(items)) => {
                        items.iter().map(json::Json::to_juce_string).collect()
                    }
                    _ => Vec::new(),
                },
            })
        }
    }))
}

fn next<'a, T>(args: &mut osc::Args<'a>, f: fn(Arg<'a>) -> Option<T>) -> Result<T, Error> {
    args.next().and_then(f).ok_or(Error::BadArgs)
}

fn time(a: Arg<'_>) -> Option<u64> {
    if let Arg::Time(v) = a { Some(v) } else { None }
}

fn int(a: Arg<'_>) -> Option<i32> {
    if let Arg::Int(v) = a { Some(v) } else { None }
}

fn float(a: Arg<'_>) -> Option<f32> {
    if let Arg::Float(v) = a { Some(v) } else { None }
}

fn boolean(a: Arg<'_>) -> Option<bool> {
    if let Arg::Bool(v) = a { Some(v) } else { None }
}

fn string(a: Arg<'_>) -> Option<&str> {
    if let Arg::Str(v) = a { Some(v) } else { None }
}

fn blob(a: Arg<'_>) -> Option<&[u8]> {
    if let Arg::Blob(v) = a { Some(v) } else { None }
}

/// Minimal JSON reader/writer matching JUCE 7.0.8 `juce_JSON.cpp` as bundled with SonoBus 1.7.2.
///
/// Writer (`JSONFormatter`, `allOnOneLine = true`): `", "` between items, `": "` after keys, no
/// other whitespace. Strings escape `" \ \a \b \f \t \r \n`, keep ASCII 32-126 and write
/// everything else as lowercase `\uXXXX` (UTF-16 pairs above U+FFFF). Doubles use JUCE's
/// `serialiseDouble` (juce_String.cpp:2262-2302); upstream's `maximumDecimalPlaces = 6` argument
/// is ignored by it.
///
/// Reader (`JSONParser`): top level must be an object or array (empty text = null); trailing
/// bytes are ignored; strings may also use single quotes (values only); unknown escapes yield the
/// escaped character; an empty key is an error; duplicate keys keep the last value. Deviations:
/// nesting is capped at [`MAX_DEPTH`] and text at [`MAX_LEN`]; `\u` surrogate pairs are combined
/// and lone surrogates become U+FFFD (JUCE writes them as invalid UTF-8); invalid UTF-8 is
/// replaced with U+FFFD (JUCE falls back to Windows-1252); integer overflow becomes a double
/// (JUCE wraps).
mod json {
    use std::fmt::Write;

    pub const MAX_DEPTH: usize = 32;
    pub const MAX_LEN: usize = super::MAX_PACKET_SIZE;

    #[derive(Debug, Clone, PartialEq)]
    pub enum Json {
        Null,
        Bool(bool),
        Int(i64),
        Num(f64),
        Str(String),
        Arr(Vec<Json>),
        Obj(Vec<(String, Json)>),
    }

    impl Json {
        /// `var::getProperty` lookup; `None` for a missing key or a non-object.
        pub fn get(&self, key: &str) -> Option<&Json> {
            match self {
                Json::Obj(props) => props.iter().find(|(k, _)| k == key).map(|(_, v)| v),
                _ => None,
            }
        }

        /// JUCE `operator double()`.
        pub fn to_f64(&self) -> f64 {
            match self {
                Json::Bool(b) => f64::from(u8::from(*b)),
                Json::Int(v) => *v as f64,
                Json::Num(v) => *v,
                Json::Str(s) => crate::juce_xml::juce_double(s),
                _ => 0.0,
            }
        }

        /// JUCE `operator int()`.
        pub fn to_i32(&self) -> i32 {
            match self {
                Json::Bool(b) => i32::from(*b),
                Json::Int(v) => *v as i32,
                Json::Num(v) => *v as i32,
                Json::Str(s) => crate::juce_xml::juce_int(s) as i32,
                _ => 0,
            }
        }

        /// JUCE `operator bool()`.
        pub fn to_bool(&self) -> bool {
            match self {
                Json::Null => false,
                Json::Bool(b) => *b,
                Json::Int(v) => *v != 0,
                Json::Num(v) => *v != 0.0,
                Json::Str(s) => crate::juce_xml::juce_str_bool(s),
                Json::Arr(_) | Json::Obj(_) => true,
            }
        }

        /// JUCE `var::toString()`. Containers give "" (JUCE: "[Array]" / "Object 0x...").
        pub fn to_juce_string(&self) -> String {
            match self {
                Json::Str(s) => s.clone(),
                Json::Int(v) => v.to_string(),
                Json::Num(v) => {
                    let mut s = String::new();
                    write_double(&mut s, *v);
                    s
                }
                Json::Bool(b) => if *b { "1" } else { "0" }.into(),
                Json::Null | Json::Arr(_) | Json::Obj(_) => String::new(),
            }
        }

        pub fn write(&self, out: &mut String) {
            match self {
                Json::Null => out.push_str("null"),
                Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
                Json::Int(v) => {
                    let _ = write!(out, "{v}");
                }
                Json::Num(v) => write_double(out, *v),
                Json::Str(s) => write_str(out, s),
                Json::Arr(items) => {
                    out.push('[');
                    for (i, v) in items.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        v.write(out);
                    }
                    out.push(']');
                }
                Json::Obj(props) => {
                    out.push('{');
                    for (i, (k, v)) in props.iter().enumerate() {
                        if i > 0 {
                            out.push_str(", ");
                        }
                        write_str(out, k);
                        out.push_str(": ");
                        v.write(out);
                    }
                    out.push('}');
                }
            }
        }
    }

    fn write_str(out: &mut String, s: &str) {
        out.push('"');
        for c in s.chars() {
            match c {
                '\0' => break, // a JUCE String ends here
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\x07' => out.push_str("\\a"),
                '\x08' => out.push_str("\\b"),
                '\x0c' => out.push_str("\\f"),
                '\t' => out.push_str("\\t"),
                '\r' => out.push_str("\\r"),
                '\n' => out.push_str("\\n"),
                ' '..='~' => out.push(c),
                _ => {
                    for unit in c.encode_utf16(&mut [0; 2]) {
                        let _ = write!(out, "\\u{unit:04x}");
                    }
                }
            }
        }
        out.push('"');
    }

    /// JUCE `serialiseDouble` (juce_String.cpp:2262-2302) followed by
    /// `reduceLengthOfFloatString`; non-finite values are written as `null` (juce_JSON.cpp:334-341).
    pub fn write_double(out: &mut String, d: f64) {
        if !d.is_finite() {
            out.push_str("null");
            return;
        }
        let a = d.abs();
        if a >= 1e6 || a <= 1e-5 {
            // "%.15e" with trailing mantissa zeros, '+' and exponent zeros dropped.
            let s = format!("{d:.15e}");
            let (m, e) = s.split_once('e').unwrap_or((&s, "0"));
            out.push_str(trim_zeros(m));
            if e != "0" {
                out.push('e');
                out.push_str(e);
            }
            return;
        }
        if d == f64::from(d as i32) {
            let _ = write!(out, "{d:.1}");
            return;
        }
        let places = if a < 1.0 {
            match a {
                _ if a >= 1e-1 => 16,
                _ if a >= 1e-2 => 17,
                _ if a >= 1e-3 => 18,
                _ if a >= 1e-4 => 19,
                _ => 20,
            }
        } else {
            match a {
                _ if a < 1e1 => 15,
                _ if a < 1e2 => 14,
                _ if a < 1e3 => 13,
                _ if a < 1e4 => 12,
                _ if a < 1e5 => 11,
                _ => 10,
            }
        };
        out.push_str(trim_zeros(&format!("{d:.places$}")));
    }

    /// Drop trailing zeros but keep one digit after the point.
    fn trim_zeros(s: &str) -> &str {
        let t = s.trim_end_matches('0');
        if t.ends_with('.') {
            &s[..t.len() + 1]
        } else {
            t
        }
    }

    /// JUCE `JSON::parse(text, result)` on a blob decoded like `String::createStringFromData`.
    pub fn parse(data: &[u8]) -> Result<Json, super::Error> {
        if data.len() > MAX_LEN {
            return Err(super::Error::BadJson);
        }
        let data = data.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(data);
        // JUCE strings end at NUL.
        let data = &data[..data.iter().position(|&b| b == 0).unwrap_or(data.len())];
        let text = String::from_utf8_lossy(data);
        let mut p = Parser {
            s: text.as_bytes(),
            i: 0,
        };
        p.ws();
        match p.peek() {
            None => Ok(Json::Null),
            Some(b'{' | b'[') => p.value(0),
            Some(_) => Err(()),
        }
        .map_err(|()| super::Error::BadJson)
    }

    struct Parser<'a> {
        s: &'a [u8],
        i: usize,
    }

    impl Parser<'_> {
        fn peek(&self) -> Option<u8> {
            self.s.get(self.i).copied()
        }

        fn bump(&mut self) -> Result<u8, ()> {
            let c = self.peek().ok_or(())?;
            self.i += 1;
            Ok(c)
        }

        fn eat(&mut self, c: u8) -> bool {
            let hit = self.peek() == Some(c);
            self.i += usize::from(hit);
            hit
        }

        fn lit(&mut self, rest: &[u8]) -> bool {
            let hit = self.s[self.i..].starts_with(rest);
            self.i += if hit { rest.len() } else { 0 };
            hit
        }

        fn ws(&mut self) {
            while matches!(
                self.peek(),
                Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
            ) {
                self.i += 1;
            }
        }

        fn digits(&mut self) {
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.i += 1;
            }
        }

        fn value(&mut self, depth: usize) -> Result<Json, ()> {
            if depth > MAX_DEPTH {
                return Err(());
            }
            self.ws();
            match self.bump()? {
                b'{' => self.object(depth),
                b'[' => self.array(depth),
                q @ (b'"' | b'\'') => self.string(q).map(Json::Str),
                b'-' => {
                    self.ws();
                    self.number(true)
                }
                b'0'..=b'9' => {
                    self.i -= 1;
                    self.number(false)
                }
                b't' if self.lit(b"rue") => Ok(Json::Bool(true)),
                b'f' if self.lit(b"alse") => Ok(Json::Bool(false)),
                b'n' if self.lit(b"ull") => Ok(Json::Null),
                _ => Err(()),
            }
        }

        fn number(&mut self, neg: bool) -> Result<Json, ()> {
            let start = self.i;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(());
            }
            self.digits();
            let float = |s: &[u8]| -> Result<f64, ()> {
                let v: f64 = std::str::from_utf8(s)
                    .map_err(|_| ())?
                    .parse()
                    .map_err(|_| ())?;
                Ok(if neg { -v } else { v })
            };
            match self.peek() {
                Some(b'.' | b'e' | b'E') => {
                    if self.eat(b'.') {
                        self.digits();
                    }
                    if self.eat(b'e') || self.eat(b'E') {
                        let _ = self.eat(b'+') || self.eat(b'-');
                        self.digits();
                    }
                    return float(&self.s[start..self.i]).map(Json::Num);
                }
                None | Some(b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c) => {}
                Some(_) => return Err(()),
            }
            let digits = &self.s[start..self.i];
            match std::str::from_utf8(digits).map_err(|_| ())?.parse::<i64>() {
                Ok(v) => Ok(Json::Int(if neg { -v } else { v })),
                Err(_) => float(digits).map(Json::Num),
            }
        }

        fn hex4(&mut self) -> Result<u32, ()> {
            let mut v = 0;
            for _ in 0..4 {
                v = v << 4 | char::from(self.bump()?).to_digit(16).ok_or(())?;
            }
            Ok(v)
        }

        fn string(&mut self, quote: u8) -> Result<String, ()> {
            let mut buf = Vec::new();
            loop {
                let c = self.bump()?;
                if c == quote {
                    break;
                }
                if c != b'\\' {
                    buf.push(c);
                    continue;
                }
                match self.bump()? {
                    b'a' => buf.push(7),
                    b'b' => buf.push(8),
                    b'f' => buf.push(0x0c),
                    b'n' => buf.push(b'\n'),
                    b'r' => buf.push(b'\r'),
                    b't' => buf.push(b'\t'),
                    b'u' => {
                        let mut cp = self.hex4()?;
                        if cp == 0 {
                            return Err(()); // JUCE: "Unexpected EOF in string constant"
                        }
                        if (0xd800..0xdc00).contains(&cp) && self.s[self.i..].starts_with(b"\\u") {
                            let save = self.i;
                            self.i += 2;
                            match self.hex4()? {
                                lo @ 0xdc00..0xe000 => {
                                    cp = 0x10000 + ((cp - 0xd800) << 10) + (lo - 0xdc00)
                                }
                                _ => self.i = save,
                            }
                        }
                        let ch = char::from_u32(cp).unwrap_or(char::REPLACEMENT_CHARACTER);
                        buf.extend_from_slice(ch.encode_utf8(&mut [0; 4]).as_bytes());
                    }
                    // `"`, `'`, `\`, `/` and unknown escapes: the character itself.
                    c => buf.push(c),
                }
            }
            // Only ASCII bytes were interpreted, so `buf` is still valid UTF-8.
            String::from_utf8(buf).map_err(|_| ())
        }

        fn object(&mut self, depth: usize) -> Result<Json, ()> {
            let mut props: Vec<(String, Json)> = Vec::new();
            loop {
                self.ws();
                match self.bump()? {
                    b'}' => break,
                    b'"' => {}
                    _ => return Err(()),
                }
                let key = self.string(b'"')?;
                if key.is_empty() {
                    return Err(()); // invalid juce::Identifier
                }
                self.ws();
                if !self.eat(b':') {
                    return Err(());
                }
                let v = self.value(depth + 1)?;
                match props.iter_mut().find(|(k, _)| *k == key) {
                    Some(slot) => slot.1 = v,
                    None => props.push((key, v)),
                }
                self.ws();
                if self.eat(b',') {
                    continue;
                }
                if self.eat(b'}') {
                    break;
                }
                return Err(());
            }
            Ok(Json::Obj(props))
        }

        fn array(&mut self, depth: usize) -> Result<Json, ()> {
            let mut items = Vec::new();
            loop {
                self.ws();
                if self.eat(b']') {
                    break;
                }
                items.push(self.value(depth + 1)?);
                self.ws();
                if self.eat(b',') {
                    continue;
                }
                if self.eat(b']') {
                    break;
                }
                return Err(());
            }
            Ok(Json::Arr(items))
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        fn fmt(d: f64) -> String {
            let mut s = String::new();
            write_double(&mut s, d);
            s
        }

        fn p(s: &str) -> Result<Json, crate::sbmsg::Error> {
            parse(s.as_bytes())
        }

        #[test]
        fn juce_double_formatting() {
            // JUCE's own expectations: juce_JSON.cpp:667-684 and juce_String.cpp:2920-2949.
            let cases: &[(f64, &str)] = &[
                (1.0, "1.0"),
                (1.1, "1.1"),
                (1.01, "1.01"),
                (0.76378, "0.76378"),
                (-10.0, "-10.0"),
                (10.01, "10.01"),
                (0.0123, "0.0123"),
                (-3.7e-27, "-3.7e-27"),
                (1e40, "1.0e40"),
                (-12345678901234567.0, "-1.234567890123457e16"),
                (192000.0, "192000.0"),
                (1234567.0, "1.234567e6"),
                (0.00006, "0.00006"),
                (0.000006, "6.0e-6"),
                (364.0, "364.0"),
                (1e7, "1.0e7"),
                (12345678901.0, "1.2345678901e10"),
                (123456.78901234567, "123456.7890123457"),
                (12.345678901234567, "12.34567890123457"),
                (0.12345678901234567, "0.1234567890123457"),
                (0.00012345678901234567, "0.0001234567890123457"),
                (0.000012345678901234567, "0.00001234567890123457"),
                (0.0000012345678901234567, "1.234567890123457e-6"),
                (0.0, "0.0"),
                (f64::NAN, "null"),
                (f64::INFINITY, "null"),
                // 1e3 * 128 / 48000, what upstream sends as inlat/outlat
                (1e3 * 128.0 / 48000.0, "2.666666666666667"),
            ];
            for &(d, s) in cases {
                assert_eq!(fmt(d), s, "{d:e}");
            }
        }

        #[test]
        fn string_escapes() {
            let mut s = String::new();
            Json::Str("a\"\\\x07\x08\x0c\t\r\n/\x7fé😀\0tail".into()).write(&mut s);
            assert_eq!(s, r#""a\"\\\a\b\f\t\r\n/\u007f\u00e9\ud83d\ude00""#);
            assert_eq!(
                p(&format!("[{s}]")).unwrap(),
                Json::Arr(vec![Json::Str("a\"\\\x07\x08\x0c\t\r\n/\x7fé😀".into())])
            );
        }

        #[test]
        fn juce_reader_quirks() {
            assert_eq!(p("").unwrap(), Json::Null);
            assert_eq!(p("  \r\n").unwrap(), Json::Null);
            assert!(p("null").is_err());
            assert!(p("12").is_err());
            assert!(p("\"x\"").is_err());
            // trailing garbage ignored, trailing comma tolerated, single quotes in values
            assert_eq!(
                p("{\"a\": 'x',} junk").unwrap(),
                Json::Obj(vec![("a".into(), Json::Str("x".into()))])
            );
            // duplicate keys: last wins, first position kept
            assert_eq!(
                p(r#"{"a": 1, "b": 2, "a": 3}"#).unwrap(),
                Json::Obj(vec![("a".into(), Json::Int(3)), ("b".into(), Json::Int(2))])
            );
            assert!(p(r#"{"": 1}"#).is_err());
            assert!(p("{'a': 1}").is_err());
            assert_eq!(
                p(r#"["\q\'\/"]"#).unwrap(),
                Json::Arr(vec![Json::Str("q'/".into())])
            );
            assert_eq!(
                p("[- 5, 007, 1.5e2, 2E-1, 3.]").unwrap(),
                Json::Arr(vec![
                    Json::Int(-5),
                    Json::Int(7),
                    Json::Num(150.0),
                    Json::Num(0.2),
                    Json::Num(3.0),
                ])
            );
            assert!(p("[12abc]").is_err());
            assert!(p("[tru]").is_err());
            // NUL ends the text; BOM skipped
            assert_eq!(
                parse(b"[1]\0garbage").unwrap(),
                Json::Arr(vec![Json::Int(1)])
            );
            assert_eq!(
                parse(b"\xEF\xBB\xBF[1]").unwrap(),
                Json::Arr(vec![Json::Int(1)])
            );
        }

        #[test]
        fn hostile_inputs() {
            assert!(p(&"[".repeat(1000)).is_err());
            assert!(
                p(&format!(
                    "{}{}",
                    "[".repeat(MAX_DEPTH + 2),
                    "]".repeat(MAX_DEPTH + 2)
                ))
                .is_err()
            );
            let ok_depth = MAX_DEPTH + 1;
            assert!(p(&format!("{}{}", "[".repeat(ok_depth), "]".repeat(ok_depth))).is_ok());
            assert_eq!(
                p("[1e999999, 99999999999999999999999, -99999999999999999999, 1e-99999]").unwrap(),
                Json::Arr(vec![
                    Json::Num(f64::INFINITY),
                    Json::Num(1e23),
                    Json::Num(-1e20),
                    Json::Num(0.0),
                ])
            );
            for bad in [
                r#"["\u12G4"]"#,
                r#"["\u12"]"#,
                r#"["\u0000"]"#,
                r#"["abc"#,
                r#"["\"#,
                "[1,",
                "{\"a\"",
                "{\"a\":",
                "[-]",
                "[-x]",
            ] {
                assert!(p(bad).is_err(), "{bad}");
            }
            assert_eq!(
                p(r#"["\ud800x", "\udc00", "\ud800\u0041"]"#).unwrap(),
                Json::Arr(vec![
                    Json::Str("\u{fffd}x".into()),
                    Json::Str("\u{fffd}".into()),
                    Json::Str("\u{fffd}A".into()),
                ])
            );
            assert_eq!(
                parse(b"[\"\xff\xc3\"]").unwrap(),
                Json::Arr(vec![Json::Str("\u{fffd}\u{fffd}".into())])
            );
            assert!(parse(&vec![b' '; MAX_LEN + 1]).is_err());
            // pseudo-random bytes from a JSON-ish alphabet: must never panic
            let alphabet = b"{}[]\",:'\\u0123456789abcdefnrtE.+- \xc3\xa9\xff";
            let mut x: u32 = 1;
            for _ in 0..20000 {
                let len = (x % 40) as usize;
                let mut s = vec![b'['];
                for _ in 0..len {
                    x = x.wrapping_mul(1_103_515_245).wrapping_add(12345);
                    s.push(alphabet[(x >> 16) as usize % alphabet.len()]);
                }
                let _ = parse(&s);
            }
        }

        #[test]
        fn coercions() {
            let v = p(r#"{"s": " 12.5 ", "i": 3, "n": 2.9, "t": true, "y": "Yes", "z": "0", "a": [], "o": {}, "x": null}"#).unwrap();
            let g = |k| v.get(k).unwrap();
            assert_eq!(g("s").to_f64(), 12.5);
            assert_eq!(g("n").to_i32(), 2);
            assert_eq!(g("t").to_f64(), 1.0);
            assert!(g("y").to_bool() && !g("z").to_bool() && g("i").to_bool() && g("a").to_bool());
            assert!(g("o").to_bool() && !g("x").to_bool());
            assert_eq!(g("i").to_juce_string(), "3");
            assert_eq!(g("n").to_juce_string(), "2.9");
            assert_eq!(g("t").to_juce_string(), "1");
            assert_eq!(g("x").to_juce_string(), "");
            assert_eq!(v.get("missing"), None);
            assert_eq!(Json::Arr(vec![]).get("s"), None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::ChannelLayout;

    /// Encode gives exactly `bytes`, `bytes` parses back to `msg`, and every truncation of
    /// `bytes` is rejected or unclaimed without panicking.
    fn golden(msg: SbMessage<'_>, bytes: &[u8]) {
        let mut out = vec![0xAA];
        msg.encode(&mut out).unwrap();
        assert_eq!(&out[1..], bytes, "{msg:?}");
        assert_eq!(parse(bytes).unwrap(), Some(msg));
        for n in 0..bytes.len() {
            assert!(
                !matches!(parse(&bytes[..n]), Ok(Some(_))),
                "prefix {n} of {bytes:?}"
            );
        }
    }

    fn blob_msg(addr: &str, json: &str) -> Vec<u8> {
        let mut v = Vec::new();
        osc::encode(&mut v, addr, [Arg::Blob(json.as_bytes())]);
        v
    }

    #[test]
    fn ping() {
        golden(
            SbMessage::Ping {
                origin_time: 0x0102_0304_0506_0708,
            },
            b"/sb/ping\0\0\0\0,t\0\0\x01\x02\x03\x04\x05\x06\x07\x08",
        );
    }

    #[test]
    fn ping_ack() {
        golden(
            SbMessage::PingAck {
                origin_time: 0x0102_0304_0506_0708,
                reply_time: 0xE000_0000_0000_0001,
            },
            b"/sb/pngack\0\0,tt\0\x01\x02\x03\x04\x05\x06\x07\x08\xe0\0\0\0\0\0\0\x01",
        );
    }

    // sendRemotePeerInfoUpdate at 128 samples / 48 kHz, buffer 20 ms, not recording.
    const PINFO_JSON: &str = r#"{"inlat": 2.666666666666667, "outlat": 2.666666666666667, "rec": false, "jitbuf": 20.0}"#;
    const PINFO_PRETTY: &str = "{\r\n  \"inlat\": 2.666666666666667,\r\n  \"outlat\": 2.666666666666667,\r\n  \"rec\": false,\r\n  \"jitbuf\": 20.0\r\n}";

    fn pinfo() -> PeerInfo {
        PeerInfo {
            jitbuf_ms: Some(20.0),
            // upstream computes 1e3 * 128 / 48000 = 2.6666666666666665, which JUCE writes as
            // below (see json::tests::juce_double_formatting); this is what reading it gives back
            in_latency_ms: Some(2.666666666666667),
            out_latency_ms: Some(2.666666666666667),
            recording: Some(false),
            net_type: None,
            supported_codecs: None,
        }
    }

    #[test]
    fn peer_info() {
        assert_eq!(PINFO_JSON.len(), 87);
        let bytes = [
            &b"/sb/pinfo\0\0\0,b\0\0\0\0\0\x57"[..],
            PINFO_JSON.as_bytes(),
            b"\0",
        ]
        .concat();
        golden(SbMessage::PeerInfo(pinfo()), &bytes);
        assert_eq!(
            parse(&blob_msg("/sb/pinfo", PINFO_PRETTY)).unwrap(),
            Some(SbMessage::PeerInfo(pinfo()))
        );
    }

    #[test]
    fn peer_info_lenient() {
        // nettype is read; unknown keys ignored; JUCE var coercions; missing keys stay None.
        let pkt = blob_msg(
            "/sb/pinfo",
            r#"{"jitbuf": "12.5", "rec": 1, "nettype": 2.9, "inlat": true, "x-rust-caps": {"a": [1]}}"#,
        );
        let m = parse(&pkt).unwrap();
        assert_eq!(
            m,
            Some(SbMessage::PeerInfo(PeerInfo {
                jitbuf_ms: Some(12.5),
                in_latency_ms: Some(1.0),
                out_latency_ms: None,
                recording: Some(true),
                net_type: Some(NET_TYPE_WIFI),
                supported_codecs: None,
            }))
        );
        // empty payload and arrays count as "no info"; broken JSON is rejected
        for empty in ["", "[]", "{}"] {
            assert_eq!(
                parse(&blob_msg("/sb/pinfo", empty)).unwrap(),
                Some(SbMessage::PeerInfo(PeerInfo::default()))
            );
        }
        assert_eq!(parse(&blob_msg("/sb/pinfo", "{")), Err(Error::BadJson));
        // strings coerce through getIntValue/getDoubleValue (numeric prefix), as JUCE's var does
        let pkt = blob_msg("/sb/pinfo", r#"{"nettype": "2.9", "rec": "2.9", "jitbuf": "12ms", "inlat": " yes"}"#);
        assert_eq!(
            parse(&pkt).unwrap(),
            Some(SbMessage::PeerInfo(PeerInfo {
                jitbuf_ms: Some(12.0),
                in_latency_ms: Some(0.0),
                out_latency_ms: None,
                recording: Some(true),
                net_type: Some(NET_TYPE_WIFI),
                supported_codecs: None,
            }))
        );
        // net_type is written last and round-trips
        let p = PeerInfo {
            net_type: Some(NET_TYPE_ETHERNET),
            ..PeerInfo::default()
        };
        let mut out = Vec::new();
        SbMessage::PeerInfo(p.clone()).encode(&mut out).unwrap();
        assert_eq!(out, blob_msg("/sb/pinfo", r#"{"nettype": 1}"#));
        assert_eq!(parse(&out).unwrap(), Some(SbMessage::PeerInfo(p)));
    }

    /// PR #201's `supportedcodecs`, as its `sendRemotePeerInfoUpdate` writes it: a JUCE
    /// `StringArray` of `Uuid::toString()` (undashed lowercase hex) right after `rec`.
    #[test]
    fn peer_info_supported_codecs() {
        let json = concat!(
            r#"{"inlat": 2.666666666666667, "outlat": 2.666666666666667, "rec": false, "#,
            r#""supportedcodecs": ["d30affc3db7640fd85bd6ba4f67d75f0", "9825290ef2cf41be9ff80c25f60ae8ad"], "#,
            r#""jitbuf": 20.0}"#
        );
        // 180 bytes: a multiple of 4, so the blob has no padding
        assert_eq!(json.len(), 180);
        let bytes = [&b"/sb/pinfo\0\0\0,b\0\0\0\0\0\xb4"[..], json.as_bytes()].concat();
        let p = PeerInfo {
            supported_codecs: Some(vec![
                "d30affc3db7640fd85bd6ba4f67d75f0".into(),
                "9825290ef2cf41be9ff80c25f60ae8ad".into(),
            ]),
            ..pinfo()
        };
        golden(SbMessage::PeerInfo(p.clone()), &bytes);
        // an empty list is a list (the peer decodes nothing we know), and round-trips
        let empty = PeerInfo {
            supported_codecs: Some(Vec::new()),
            ..PeerInfo::default()
        };
        let mut out = Vec::new();
        SbMessage::PeerInfo(empty.clone()).encode(&mut out).unwrap();
        assert_eq!(out, blob_msg("/sb/pinfo", r#"{"supportedcodecs": []}"#));
        assert_eq!(parse(&out).unwrap(), Some(SbMessage::PeerInfo(empty)));

        // dashed / upper-case UUIDs normalize; unknown strings stay; non-strings are skipped
        let pkt = blob_msg(
            "/sb/pinfo",
            r#"{"supportedcodecs": ["D30AFFC3-DB76-40FD-85BD-6BA4F67D75F0", "pcm8", 3, null, ["x"]]}"#,
        );
        let Ok(Some(SbMessage::PeerInfo(got))) = parse(&pkt) else {
            panic!()
        };
        assert_eq!(
            got.supported_codecs,
            Some(vec!["d30affc3db7640fd85bd6ba4f67d75f0".into(), "pcm8".into()])
        );
        // a value of the wrong type is no list (the legacy assumption applies)
        for v in [r#""abc""#, "1", "{}", "null", "true"] {
            let pkt = blob_msg("/sb/pinfo", &format!(r#"{{"supportedcodecs": {v}}}"#));
            let Ok(Some(SbMessage::PeerInfo(got))) = parse(&pkt) else {
                panic!("{v}")
            };
            assert_eq!(got.supported_codecs, None, "{v}");
        }
        // capped
        let many: Vec<String> = (0..200).map(|i| format!(r#""c{i}""#)).collect();
        let pkt = blob_msg(
            "/sb/pinfo",
            &format!(r#"{{"supportedcodecs": [{}]}}"#, many.join(", ")),
        );
        let Ok(Some(SbMessage::PeerInfo(got))) = parse(&pkt) else {
            panic!()
        };
        let got = got.supported_codecs.unwrap();
        assert_eq!(got.len(), MAX_SUPPORTED_CODECS);
        assert_eq!((got[0].as_str(), got[63].as_str()), ("c0", "c63"));
    }

    #[test]
    fn channel_layout_info() {
        let tree: &[u8] = b"ChannelLayouts\0\x00\x01\x01\
            Layout\0\x01\x05\
            chanstart\0\x01\x05\x01\0\0\0\0\
            numchan\0\x01\x05\x01\x01\0\0\0\
            name\0\x01\x02\x05\0\
            span1\0\x01\x09\x04\0\0\0\0\0\0\xf0\xbf\
            span2\0\x01\x09\x04\0\0\0\0\0\0\xf0\x3f\
            \x01\x01StereoPanners\0\x00\x01\x01\
            Panner\0\x01\x01pan\0\x01\x09\x04\0\0\0\0\0\0\0\0\x00";
        assert_eq!(tree.len(), 146);
        let bytes = [
            &b"/sb/clayinfo\0\0\0\0,ib\0\0\0\0\x07\0\0\0\x92"[..],
            tree,
            b"\0\0",
        ]
        .concat();
        golden(
            SbMessage::ChannelLayoutInfo {
                source_id: 7,
                layouts: ChannelLayouts {
                    groups: vec![ChannelLayout::default()],
                },
            },
            &bytes,
        );
        // an invalid tree is rejected, as upstream
        let mut v = Vec::new();
        osc::encode(&mut v, "/sb/clayinfo", [Arg::Int(1), Arg::Blob(b"\0\0\0")]);
        assert_eq!(parse(&v), Err(Error::BadLayout));
    }

    #[test]
    fn channel_layout_too_big() {
        let g = ChannelLayout {
            num_channels: 64,
            name: "x".repeat(2000),
            ..ChannelLayout::default()
        };
        let msg = SbMessage::ChannelLayoutInfo {
            source_id: 1,
            layouts: ChannelLayouts {
                groups: vec![g.clone(), g],
            },
        };
        let mut out = vec![1, 2];
        assert_eq!(msg.encode(&mut out), Err(Error::TooBig));
        assert_eq!(out, [1, 2]);
    }

    #[test]
    fn chat() {
        golden(
            SbMessage::Chat {
                group: "g",
                from: "alice",
                targets: "",
                tags: "",
                message: "hi",
            },
            b"/sb/chat\0\0\0\0,sssss\0\0g\0\0\0alice\0\0\0\0\0\0\0\0\0\0\0hi\0\0",
        );
        let long = "x".repeat(MAX_PACKET_SIZE);
        let mut out = Vec::new();
        let msg = SbMessage::Chat {
            group: "",
            from: "",
            targets: "",
            tags: "",
            message: &long,
        };
        assert_eq!(msg.encode(&mut out), Err(Error::TooBig));
        assert!(out.is_empty());
    }

    #[test]
    fn req_lat_info() {
        golden(SbMessage::ReqLatInfo, b"/sb/reqlatinfo\0\0,\0\0\0");
    }

    const LATINFO_JSON: &str = r#"[{"srcname": "bob", "destname": "alice", "latms": 12.5}, {"srcname": "carol", "destname": "alice", "latms": 7.0}]"#;
    const LATINFO_PRETTY: &str = "[\r\n  {\r\n    \"srcname\": \"bob\",\r\n    \"destname\": \"alice\",\r\n    \"latms\": 12.5\r\n  },\r\n  {\r\n    \"srcname\": \"carol\",\r\n    \"destname\": \"alice\",\r\n    \"latms\": 7.0\r\n  }\r\n]";

    fn latinfo() -> Vec<LatEntry> {
        let e = |s: &str, ms| LatEntry {
            src_name: s.into(),
            dest_name: "alice".into(),
            latency_ms: ms,
        };
        vec![e("bob", 12.5), e("carol", 7.0)]
    }

    #[test]
    fn lat_info() {
        assert_eq!(LATINFO_JSON.len(), 113);
        let bytes = [
            &b"/sb/latinfo\0,b\0\0\0\0\0\x71"[..],
            LATINFO_JSON.as_bytes(),
            b"\0\0\0",
        ]
        .concat();
        golden(SbMessage::LatInfo(latinfo()), &bytes);
        assert_eq!(
            parse(&blob_msg("/sb/latinfo", LATINFO_PRETTY)).unwrap(),
            Some(SbMessage::LatInfo(latinfo()))
        );
        // entries without both names are dropped; non-arrays give nothing; "null" is a parse error
        let pkt = blob_msg(
            "/sb/latinfo",
            r#"[{"srcname": "a"}, 5, {"srcname": "a", "destname": "b"}]"#,
        );
        let m = parse(&pkt).unwrap();
        assert_eq!(
            m,
            Some(SbMessage::LatInfo(vec![LatEntry {
                src_name: "a".into(),
                dest_name: "b".into(),
                latency_ms: 0.0
            }]))
        );
        assert_eq!(
            parse(&blob_msg("/sb/latinfo", "{}")).unwrap(),
            Some(SbMessage::LatInfo(vec![]))
        );
        assert_eq!(parse(&blob_msg("/sb/latinfo", "null")), Err(Error::BadJson));
        let mut out = Vec::new();
        SbMessage::LatInfo(vec![]).encode(&mut out).unwrap();
        assert_eq!(out, blob_msg("/sb/latinfo", "[]"));
    }

    #[test]
    fn suggest_latency() {
        golden(
            SbMessage::SuggestLatency {
                from: "alice",
                latency_ms: 42.5,
            },
            b"/sb/suggestlat\0\0,sf\0alice\0\0\0\x42\x2a\0\0",
        );
        // wrong tags are rejected
        assert_eq!(
            parse(b"/sb/suggestlat\0\0,si\0alice\0\0\0\x42\x2a\0\0"),
            Err(Error::BadArgs)
        );
    }

    #[test]
    fn blocked_info() {
        golden(
            SbMessage::BlockedInfo {
                from: "alice",
                blocked: true,
            },
            b"/sb/blockedinfo\0,sT\0alice\0\0\0",
        );
        golden(
            SbMessage::BlockedInfo {
                from: "",
                blocked: false,
            },
            b"/sb/blockedinfo\0,sF\0\0\0\0\0",
        );
        // an int is not a bool for oscpack's AsBool
        let mut v = Vec::new();
        osc::encode(&mut v, "/sb/blockedinfo", [Arg::Str("a"), Arg::Int(1)]);
        assert_eq!(parse(&v), Err(Error::BadArgs));
    }

    const GROUP_JSON: &str = r#"{"user": "alice", "group": "jam", "group_pass": "", "public": true, "others": ["bob", "carol"]}"#;
    const GROUP_PRETTY: &str = "{\r\n  \"user\": \"alice\",\r\n  \"group\": \"jam\",\r\n  \"group_pass\": \"\",\r\n  \"public\": true,\r\n  \"others\": [\r\n    \"bob\",\r\n    \"carol\"\r\n  ]\r\n}";

    fn group() -> SuggestGroup {
        SuggestGroup {
            user: "alice".into(),
            group: "jam".into(),
            group_password: String::new(),
            public: true,
            others: vec!["bob".into(), "carol".into()],
        }
    }

    #[test]
    fn suggest_group() {
        assert_eq!(GROUP_JSON.len(), 95);
        let bytes = [
            &b"/sb/suggestgroup\0\0\0\0,b\0\0\0\0\0\x5f"[..],
            GROUP_JSON.as_bytes(),
            b"\0",
        ]
        .concat();
        golden(SbMessage::SuggestGroup(group()), &bytes);
        assert_eq!(
            parse(&blob_msg("/sb/suggestgroup", GROUP_PRETTY)).unwrap(),
            Some(SbMessage::SuggestGroup(group()))
        );
        // missing keys take upstream's defaults, odd types are coerced
        assert_eq!(
            parse(&blob_msg(
                "/sb/suggestgroup",
                r#"{"group": 12, "others": "bob"}"#
            ))
            .unwrap(),
            Some(SbMessage::SuggestGroup(SuggestGroup {
                group: "12".into(),
                ..SuggestGroup::default()
            }))
        );
        assert_eq!(
            parse(&blob_msg("/sb/suggestgroup", "[]")),
            Err(Error::BadJson)
        );
        // non-ASCII is \u-escaped like JUCE; payload limit enforced
        let mut out = Vec::new();
        let g = SuggestGroup {
            user: "Zoë".into(),
            ..SuggestGroup::default()
        };
        SbMessage::SuggestGroup(g.clone()).encode(&mut out).unwrap();
        assert_eq!(
            out,
            blob_msg(
                "/sb/suggestgroup",
                r#"{"user": "Zo\u00eb", "group": "", "group_pass": "", "public": false, "others": []}"#
            )
        );
        assert_eq!(parse(&out).unwrap(), Some(SbMessage::SuggestGroup(g)));
        let big = SuggestGroup {
            others: vec!["x".repeat(100); 40],
            ..SuggestGroup::default()
        };
        assert_eq!(
            SbMessage::SuggestGroup(big).encode(&mut out),
            Err(Error::TooBig)
        );
    }

    #[test]
    fn dispatch_matches_upstream_prefixes() {
        let osc_msg = |addr: &str, args: &[Arg<'_>]| {
            let mut v = Vec::new();
            osc::encode(&mut v, addr, args.iter().copied());
            v
        };
        let t = [Arg::Time(5)];
        assert_eq!(
            parse(&osc_msg("/sb/pingpong", &t)).unwrap(),
            Some(SbMessage::Ping { origin_time: 5 })
        );
        assert!(matches!(
            parse(&osc_msg("/sb/chatroom", &[Arg::Str(""); 5])),
            Ok(Some(SbMessage::Chat { .. }))
        ));
        assert_eq!(parse(&osc_msg("/sb/reqlatinfo2", &[])).unwrap(), None);
        assert_eq!(
            parse(&osc_msg(
                "/sb/blockedinfox",
                &[Arg::Str(""), Arg::Bool(true)]
            ))
            .unwrap(),
            None
        );
        assert_eq!(
            parse(&osc_msg("/sb/suggestgroups", &[Arg::Blob(b"{}")])).unwrap(),
            None
        );
        assert_eq!(parse(&osc_msg("/sb/unknown", &t)).unwrap(), None);
        assert_eq!(parse(&osc_msg("/sbx/ping", &t)).unwrap(), None);
        assert_eq!(parse(&osc_msg("/aoo/sink/1/ping", &t)).unwrap(), None);
        let mut v = b"/sb/suggestlatency\0\0,sf\0".to_vec();
        v.extend_from_slice(b"a\0\0\0\x3f\x80\0\0");
        assert_eq!(
            parse(&v).unwrap(),
            Some(SbMessage::SuggestLatency {
                from: "a",
                latency_ms: 1.0
            })
        );
        // extra trailing arguments are ignored, missing or mistyped ones rejected
        assert_eq!(
            parse(&osc_msg("/sb/ping", &[Arg::Time(1), Arg::Int(2)])).unwrap(),
            Some(SbMessage::Ping { origin_time: 1 })
        );
        assert_eq!(parse(&osc_msg("/sb/ping", &[])), Err(Error::BadArgs));
        assert_eq!(parse(&osc_msg("/sb/pngack", &t)), Err(Error::BadArgs));
        assert_eq!(
            parse(&osc_msg("/sb/ping", &[Arg::Int(1)])),
            Err(Error::BadArgs)
        );
    }
}
