//! Minimal OSC 1.0 message codec for the tags AOO/SonoBus use: `i h f d s b t T F`.
//!
//! Bundles are not supported (AOO never sends them). The decoder borrows from its input and
//! never reads past it; the encoder appends to a caller-owned buffer.

use crate::Error;
use std::fmt::Display;
use std::io::Write;

/// One OSC argument. Strings and blobs borrow from the decoded packet.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Arg<'a> {
    /// `i`
    Int(i32),
    /// `h`
    Long(i64),
    /// `d`
    Double(f64),
    /// `f`
    Float(f32),
    /// `s`
    Str(&'a str),
    /// `b`
    Blob(&'a [u8]),
    /// `t`, raw 64-bit NTP timetag
    Time(u64),
    /// `T` / `F`, no payload
    Bool(bool),
}

impl Arg<'_> {
    fn tag(&self) -> u8 {
        match self {
            Arg::Int(_) => b'i',
            Arg::Long(_) => b'h',
            Arg::Double(_) => b'd',
            Arg::Float(_) => b'f',
            Arg::Str(_) => b's',
            Arg::Blob(_) => b'b',
            Arg::Time(_) => b't',
            Arg::Bool(true) => b'T',
            Arg::Bool(false) => b'F',
        }
    }
}

fn pad(out: &mut Vec<u8>, start: usize) {
    while !(out.len() - start).is_multiple_of(4) {
        out.push(0);
    }
}

/// OSC strings cannot hold NUL; anything from the first NUL on is dropped so the packet stays
/// well formed.
fn put_str(out: &mut Vec<u8>, start: usize, s: &[u8]) {
    let n = s.iter().position(|&b| b == 0).unwrap_or(s.len());
    out.extend_from_slice(&s[..n]);
    out.push(0);
    pad(out, start);
}

/// Append one OSC message to `out`. `addr` is anything printable (a `&str` or `format_args!`), so
/// addresses with ids are built without allocating. Arguments are iterated twice.
pub fn encode<'a, I>(out: &mut Vec<u8>, addr: impl Display, args: I)
where
    I: IntoIterator<Item = Arg<'a>>,
    I::IntoIter: Clone,
{
    let start = out.len();
    let args = args.into_iter();
    // Writing to a Vec cannot fail.
    let _ = write!(out, "{addr}");
    if let Some(n) = out[start..].iter().position(|&b| b == 0) {
        out.truncate(start + n);
    }
    out.push(0);
    pad(out, start);

    out.push(b',');
    out.extend(args.clone().map(|a| a.tag()));
    out.push(0);
    pad(out, start);

    for a in args {
        match a {
            Arg::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
            Arg::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
            Arg::Double(v) => out.extend_from_slice(&v.to_be_bytes()),
            Arg::Float(v) => out.extend_from_slice(&v.to_be_bytes()),
            Arg::Time(v) => out.extend_from_slice(&v.to_be_bytes()),
            Arg::Str(s) => put_str(out, start, s.as_bytes()),
            Arg::Blob(b) => {
                out.extend_from_slice(&(b.len() as u32).to_be_bytes());
                out.extend_from_slice(b);
                pad(out, start);
            }
            Arg::Bool(_) => {}
        }
    }
}

/// A decoded, fully validated OSC message.
#[derive(Debug, Clone, Copy)]
pub struct OscMessage<'a> {
    pub addr: &'a str,
    /// Type tags without the leading `,`.
    tags: &'a [u8],
    data: &'a [u8],
}

impl<'a> OscMessage<'a> {
    /// Type tags without the leading `,` (ASCII, validated).
    pub fn tags(&self) -> &'a str {
        // Only known ASCII tags survive `decode`.
        std::str::from_utf8(self.tags).unwrap_or("")
    }

    pub fn args(&self) -> Args<'a> {
        Args {
            tags: self.tags,
            data: self.data,
        }
    }
}

/// Iterator over the arguments of an [`OscMessage`].
#[derive(Debug, Clone)]
pub struct Args<'a> {
    tags: &'a [u8],
    data: &'a [u8],
}

impl<'a> Args<'a> {
    /// Remaining tags and argument bytes.
    pub(crate) fn rest(&self) -> (&'a [u8], &'a [u8]) {
        (self.tags, self.data)
    }

    fn try_next(&mut self) -> Result<Option<Arg<'a>>, Error> {
        let Some((&tag, tags)) = self.tags.split_first() else {
            return Ok(None);
        };
        let arg = match tag {
            b'i' => Arg::Int(i32::from_be_bytes(take(&mut self.data)?)),
            b'h' => Arg::Long(i64::from_be_bytes(take(&mut self.data)?)),
            b'd' => Arg::Double(f64::from_be_bytes(take(&mut self.data)?)),
            b'f' => Arg::Float(f32::from_be_bytes(take(&mut self.data)?)),
            b't' => Arg::Time(u64::from_be_bytes(take(&mut self.data)?)),
            b's' => Arg::Str(read_str(&mut self.data)?),
            b'b' => Arg::Blob(read_blob(&mut self.data)?),
            b'T' => Arg::Bool(true),
            b'F' => Arg::Bool(false),
            _ => return Err(Error::BadTypeTag),
        };
        self.tags = tags;
        Ok(Some(arg))
    }
}

impl<'a> Iterator for Args<'a> {
    type Item = Arg<'a>;

    fn next(&mut self) -> Option<Arg<'a>> {
        // The message was validated by `decode`, so errors cannot happen here.
        self.try_next().ok().flatten()
    }
}

fn take<const N: usize>(data: &mut &[u8]) -> Result<[u8; N], Error> {
    let (head, rest) = data.split_first_chunk::<N>().ok_or(Error::Truncated)?;
    *data = rest;
    Ok(*head)
}

fn read_str<'a>(data: &mut &'a [u8]) -> Result<&'a str, Error> {
    let n = data.iter().position(|&b| b == 0).ok_or(Error::Truncated)?;
    let padded = (n + 4) & !3;
    if padded > data.len() {
        return Err(Error::Truncated);
    }
    let s = std::str::from_utf8(&data[..n]).map_err(|_| Error::BadString)?;
    *data = &data[padded..];
    Ok(s)
}

fn read_blob<'a>(data: &mut &'a [u8]) -> Result<&'a [u8], Error> {
    let size = i32::from_be_bytes(take(data)?);
    let size = usize::try_from(size).map_err(|_| Error::BadBlob)?;
    let padded = size.checked_add(3).ok_or(Error::BadBlob)? & !3;
    if padded > data.len() {
        return Err(Error::BadBlob);
    }
    let blob = &data[..size];
    *data = &data[padded..];
    Ok(blob)
}

/// Decode and validate one OSC message. Bytes after the last argument are ignored.
pub fn decode(packet: &[u8]) -> Result<OscMessage<'_>, Error> {
    let mut data = packet;
    let addr = read_str(&mut data)?;
    if !addr.starts_with('/') {
        return Err(Error::BadAddress);
    }
    let tags = read_str(&mut data)?.as_bytes();
    let Some((b',', tags)) = tags.split_first() else {
        return Err(Error::BadTypeTag);
    };
    let mut args = Args { tags, data };
    while args.try_next()?.is_some() {}
    Ok(OscMessage { addr, tags, data })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_args() -> [Arg<'static>; 9] {
        [
            Arg::Int(-2),
            Arg::Long(i64::MIN + 5),
            Arg::Double(-0.5),
            Arg::Str("hello"),
            Arg::Blob(&[1, 2, 3, 4, 5]),
            Arg::Time(0x0102_0304_0506_0708),
            Arg::Bool(true),
            Arg::Bool(false),
            Arg::Str(""),
        ]
    }

    #[test]
    fn round_trip_all_tags() {
        let mut buf = vec![0xAA]; // pre-existing content must not disturb padding
        encode(&mut buf, "/x/y", all_args());
        let m = decode(&buf[1..]).unwrap();
        assert_eq!(m.addr, "/x/y");
        assert_eq!(m.tags(), "ihdsbtTFs");
        assert!(m.args().eq(all_args()));
        assert_eq!((buf.len() - 1) % 4, 0);
    }

    #[test]
    fn golden_all_tags() {
        let expected: &[u8] = b"/x\0\0,ihdsbtTFs\0\0\
            \xff\xff\xff\xfe\
            \x80\0\0\0\0\0\0\x05\
            \xbf\xe0\0\0\0\0\0\0\
            hello\0\0\0\
            \0\0\0\x05\x01\x02\x03\x04\x05\0\0\0\
            \x01\x02\x03\x04\x05\x06\x07\x08\
            \0\0\0\0";
        let mut buf = Vec::new();
        encode(&mut buf, "/x", all_args());
        assert_eq!(buf, expected);
    }

    #[test]
    fn float_golden_and_truncated() {
        let mut buf = Vec::new();
        encode(&mut buf, "/f", [Arg::Str("a"), Arg::Float(-1.5)]);
        assert_eq!(buf, b"/f\0\0,sf\0a\0\0\0\xbf\xc0\0\0");
        let m = decode(&buf).unwrap();
        assert_eq!(m.tags(), "sf");
        assert!(m.args().eq([Arg::Str("a"), Arg::Float(-1.5)]));
        for n in 1..4 {
            assert_eq!(decode(&buf[..buf.len() - n]).unwrap_err(), Error::Truncated);
        }
    }

    #[test]
    fn empty_tag_string() {
        let mut buf = Vec::new();
        encode(&mut buf, "/abc", []);
        assert_eq!(buf, b"/abc\0\0\0\0,\0\0\0");
        let m = decode(&buf).unwrap();
        assert_eq!(m.tags(), "");
        assert_eq!(m.args().count(), 0);
    }

    #[test]
    fn nul_in_string_is_cut() {
        let mut buf = Vec::new();
        encode(&mut buf, "/a\0b", [Arg::Str("x\0yz")]);
        assert_eq!(buf, b"/a\0\0,s\0\0x\0\0\0");
    }

    #[test]
    fn rejects_malformed() {
        use Error::*;
        let cases: &[(&[u8], Error)] = &[
            (b"", Truncated),
            (b"/abc", Truncated),                      // unterminated
            (b"/ab\0", Truncated),                     // no type tags
            (b"abc\0,\0\0\0", BadAddress),             // no leading slash
            (b"/ab\0i\0\0\0", BadTypeTag),             // no comma
            (b"/ab\0,x\0\0", BadTypeTag),              // unknown tag
            (b"/ab\0,i\0\0\0\0\0", Truncated),         // short int
            (b"/ab\0,s\0\0ab", Truncated),             // unterminated string arg
            (b"/\xff\0\0,\0\0\0", BadString),          // non-utf8 address
            (b"/ab\0,b\0\0\xff\xff\xff\xff", BadBlob), // negative blob size
            (b"/ab\0,b\0\0\0\0\0\x05abcd", BadBlob),   // blob longer than packet
            (b"/ab\0,b\0\0\x7f\xff\xff\xff", BadBlob), // huge blob size
            (b"/ab\0,ii\0\0\0\0\x01", Truncated),      // tag count > args
        ];
        for (input, err) in cases {
            assert_eq!(decode(input).unwrap_err(), *err, "{input:?}");
        }
        // trailing bytes after the last argument are ignored
        assert!(decode(b"/ab\0,s\0\0abc\0\xff").is_ok());
    }
}
