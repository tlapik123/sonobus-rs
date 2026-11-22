//! JUCE `ValueTree` binary stream format, byte-exact with JUCE 7.0.8 as bundled in SonoBus 1.7.2.
//!
//! Tree (`juce_ValueTree.cpp:379-408, 1040-1082`): `type` string, compressed-int property count,
//! per property `name` string + [`Var`], compressed-int child count, children. An invalid tree is
//! written as `00 00 00`. Strings are UTF-8 + NUL. [`Var`] (`juce_Variant.cpp:26-36, 821-877`):
//! compressed-int byte length (0 = void), then a marker byte and the payload. Compressed int
//! (`juce_OutputStream.cpp:121-141`): one byte holding the count of following bytes (top bit =
//! negative), then the magnitude little-endian with leading zero bytes dropped.
//!
//! Reading mirrors JUCE's tolerance (negative counts, unknown var markers, an invalid child ending
//! the child list) but returns an error where JUCE would read past the end of the data.

use crate::juce_xml::{juce_double, juce_int, juce_str_bool};
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::fmt;

/// Maximum nesting of trees and arrays accepted by [`ValueTree::read`].
pub const MAX_DEPTH: usize = 64;

const MARKER_INT: u8 = 1;
const MARKER_BOOL_TRUE: u8 = 2;
const MARKER_BOOL_FALSE: u8 = 3;
const MARKER_DOUBLE: u8 = 4;
const MARKER_STRING: u8 = 5;
const MARKER_INT64: u8 = 6;
const MARKER_ARRAY: u8 = 7;
const MARKER_BINARY: u8 = 8;
const MARKER_UNDEFINED: u8 = 9;

/// A JUCE `var` as far as it can be streamed (objects and methods cannot).
#[derive(Debug, Clone, PartialEq, Default)]
pub enum Var {
    #[default]
    Void,
    /// Written as marker 9. JUCE itself reads it back as void; we keep it distinct.
    Undefined,
    Int(i32),
    Int64(i64),
    Bool(bool),
    Double(f64),
    String(String),
    Binary(Vec<u8>),
    Array(Vec<Var>),
}

impl Var {
    /// JUCE `operator int()` (juce_Variant.cpp:136-300): strings through `getIntValue` (optional
    /// `-`, then the leading digits: "1.0" and "12abc" give 1 and 12), doubles truncated
    /// (saturating where C++ is undefined), 64-bit ints truncated; anything else 0.
    pub fn to_i32(&self) -> i32 {
        match self {
            Var::Double(v) => *v as i32,
            v => v.to_i64() as i32,
        }
    }

    /// JUCE `operator int64()`.
    pub fn to_i64(&self) -> i64 {
        match self {
            Var::Int(v) => i64::from(*v),
            Var::Int64(v) => *v,
            Var::Double(v) => *v as i64,
            Var::Bool(v) => i64::from(*v),
            Var::String(s) => juce_int(s),
            _ => 0,
        }
    }

    /// JUCE `operator double()`: strings through `getDoubleValue` (longest numeric prefix).
    pub fn to_f64(&self) -> f64 {
        match self {
            Var::Int(v) => f64::from(*v),
            Var::Int64(v) => *v as f64,
            Var::Double(v) => *v,
            Var::Bool(v) => f64::from(u8::from(*v)),
            Var::String(s) => juce_double(s),
            _ => 0.0,
        }
    }

    /// JUCE `operator bool()`: strings are true when `getIntValue() != 0` or they trim to
    /// "true"/"yes" (any case); arrays are true; binary, void and undefined are false.
    pub fn to_bool(&self) -> bool {
        match self {
            Var::Int(v) => *v != 0,
            Var::Int64(v) => *v != 0,
            Var::Double(v) => *v != 0.0,
            Var::Bool(v) => *v,
            Var::String(s) => juce_str_bool(s),
            Var::Array(_) => true,
            _ => false,
        }
    }

    /// JUCE `toString()`, approximated for doubles (JUCE uses its own float formatting).
    pub fn to_string_lossy(&self) -> String {
        match self {
            Var::String(s) => s.clone(),
            Var::Int(v) => v.to_string(),
            Var::Int64(v) => v.to_string(),
            Var::Bool(v) => (*v as i32).to_string(),
            Var::Double(v) => v.to_string(),
            Var::Undefined => "undefined".into(),
            _ => String::new(),
        }
    }

    fn write(&self, out: &mut Vec<u8>) {
        match self {
            Var::Void => write_cint(out, 0),
            Var::Undefined => {
                write_cint(out, 1);
                out.push(MARKER_UNDEFINED);
            }
            Var::Int(v) => {
                write_cint(out, 5);
                out.push(MARKER_INT);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Var::Int64(v) => {
                write_cint(out, 9);
                out.push(MARKER_INT64);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Var::Bool(v) => {
                write_cint(out, 1);
                out.push(if *v {
                    MARKER_BOOL_TRUE
                } else {
                    MARKER_BOOL_FALSE
                });
            }
            Var::Double(v) => {
                write_cint(out, 9);
                out.push(MARKER_DOUBLE);
                out.extend_from_slice(&v.to_le_bytes());
            }
            Var::String(s) => {
                let s = juce_str(s);
                write_len(out, s.len() + 2);
                out.push(MARKER_STRING);
                out.extend_from_slice(s.as_bytes());
                out.push(0);
            }
            Var::Binary(b) => {
                write_len(out, b.len() + 1);
                out.push(MARKER_BINARY);
                out.extend_from_slice(b);
            }
            Var::Array(items) => {
                let mut buf = Vec::new();
                write_len(&mut buf, items.len());
                for i in items {
                    i.write(&mut buf);
                }
                write_len(out, buf.len() + 1);
                out.push(MARKER_ARRAY);
                out.extend_from_slice(&buf);
            }
        }
    }

    fn read(r: &mut Reader, depth: usize) -> Result<Var, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        let n = r.cint()?;
        if n <= 0 {
            return Ok(Var::Void);
        }
        let payload = n as usize - 1; // only meaningful for variable-length markers, like JUCE
        Ok(match r.byte()? {
            MARKER_INT => Var::Int(i32::from_le_bytes(r.array()?)),
            MARKER_INT64 => Var::Int64(i64::from_le_bytes(r.array()?)),
            MARKER_BOOL_TRUE => Var::Bool(true),
            MARKER_BOOL_FALSE => Var::Bool(false),
            MARKER_DOUBLE => Var::Double(f64::from_le_bytes(r.array()?)),
            MARKER_STRING => {
                let b = r.take(payload)?;
                let b = b.split(|&c| c == 0).next().unwrap_or_default();
                Var::String(String::from_utf8_lossy(b).into_owned())
            }
            MARKER_BINARY => Var::Binary(r.take(payload)?.to_vec()),
            MARKER_ARRAY => {
                let mut items = Vec::new(); // grows only as elements are actually read
                for _ in 0..r.cint()?.max(0) {
                    items.push(Var::read(r, depth + 1)?);
                }
                Var::Array(items)
            }
            MARKER_UNDEFINED => {
                r.take(payload)?;
                Var::Undefined
            }
            _ => {
                r.take(payload)?;
                Var::Void
            }
        })
    }
}

/// A JUCE `ValueTree`. An empty `type_name` is JUCE's invalid tree.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ValueTree {
    pub type_name: String,
    /// In insertion order. Names are unique except in a tree read from XML that repeats an
    /// attribute: JUCE keeps those too (`setFromXmlAttributes` appends) and [`ValueTree::get`]
    /// returns the first, as `getProperty` does.
    pub properties: Vec<(String, Var)>,
    pub children: Vec<ValueTree>,
}

impl ValueTree {
    pub fn new(type_name: impl Into<String>) -> Self {
        ValueTree {
            type_name: type_name.into(),
            ..Default::default()
        }
    }

    pub fn is_valid(&self) -> bool {
        !self.type_name.is_empty()
    }

    pub fn get(&self, name: &str) -> Option<&Var> {
        self.properties
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }

    /// Sets a property, replacing an existing one in place (JUCE `NamedValueSet::set`). Linear in
    /// the number of properties; the parsers do not use it.
    pub fn set(&mut self, name: impl Into<String>, value: Var) {
        let name = name.into();
        match self.properties.iter_mut().find(|(n, _)| *n == name) {
            Some(p) => p.1 = value,
            None => self.properties.push((name, value)),
        }
    }

    pub fn child_with_name(&self, type_name: &str) -> Option<&ValueTree> {
        self.children.iter().find(|c| c.type_name == type_name)
    }

    /// Appends JUCE `ValueTree::writeToStream` output to `out`.
    ///
    /// Things JUCE cannot represent are dropped as JUCE would: invalid children and empty
    /// property names are skipped, strings end at their first NUL.
    ///
    /// # Panics
    /// If a single string, binary or array is larger than `i32::MAX` bytes.
    pub fn write(&self, out: &mut Vec<u8>) {
        if !self.is_valid() {
            out.extend_from_slice(&[0, 0, 0]);
            return;
        }
        write_str(out, &self.type_name);
        let props: Vec<_> = self
            .properties
            .iter()
            .filter(|(n, _)| !juce_str(n).is_empty())
            .collect();
        write_len(out, props.len());
        for (name, value) in props {
            write_str(out, name);
            value.write(out);
        }
        let children: Vec<_> = self.children.iter().filter(|c| c.is_valid()).collect();
        write_len(out, children.len());
        for c in children {
            c.write(out);
        }
    }

    /// JUCE `ValueTree::readFromData`. Trailing bytes are ignored, as in JUCE.
    pub fn read(data: &[u8]) -> Result<ValueTree, Error> {
        Self::read_from(&mut Reader { data }, 0)
    }

    fn read_from(r: &mut Reader, depth: usize) -> Result<ValueTree, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        let type_name = r.string()?;
        if type_name.is_empty() {
            return Ok(ValueTree::default());
        }
        let mut v = ValueTree::new(type_name);
        let num_props = r.cint()?;
        if num_props < 0 {
            return Ok(v);
        }
        let mut props = Vec::new(); // grows only as properties are actually read
        for _ in 0..num_props {
            let name = r.string()?;
            // JUCE skips an empty name without reading its value (juce_ValueTree.cpp:1061).
            if !name.is_empty() {
                props.push((name, Var::read(r, depth + 1)?));
            }
        }
        // JUCE uses `properties.set` here: a repeated name takes the last value.
        v.properties = set_all(props);
        for _ in 0..r.cint()?.max(0) {
            let child = Self::read_from(r, depth + 1)?;
            if !child.is_valid() {
                return Ok(v); // JUCE stops at an invalid child (juce_ValueTree.cpp:1074)
            }
            v.children.push(child);
        }
        Ok(v)
    }
}

/// JUCE `NamedValueSet::set` / `XmlElement::setAttribute` / `StringPairArray::set` applied to
/// each pair in order (a repeated name keeps its first position and takes the last value), in
/// linear time.
pub(crate) fn set_all<V>(pairs: impl IntoIterator<Item = (String, V)>) -> Vec<(String, V)> {
    let mut out: Vec<(String, V)> = Vec::new();
    let mut index: HashMap<String, usize> = HashMap::new();
    for (name, value) in pairs {
        match index.entry(name) {
            Entry::Occupied(e) => out[*e.get()].1 = value,
            Entry::Vacant(e) => {
                out.push((e.key().clone(), value));
                e.insert(out.len() - 1);
            }
        }
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// The data ended before the structure it declares.
    Truncated,
    /// A compressed int declared more than 4 bytes.
    BadCompressedInt,
    /// Trees/arrays nested deeper than [`MAX_DEPTH`].
    TooDeep,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Error::Truncated => "ValueTree data truncated",
            Error::BadCompressedInt => "ValueTree compressed int longer than 4 bytes",
            Error::TooDeep => "ValueTree nested too deeply",
        })
    }
}

impl std::error::Error for Error {}

fn juce_str(s: &str) -> &str {
    s.split('\0').next().unwrap_or_default()
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(juce_str(s).as_bytes());
    out.push(0);
}

fn write_len(out: &mut Vec<u8>, n: usize) {
    write_cint(
        out,
        i32::try_from(n).expect("ValueTree element larger than i32::MAX bytes"),
    );
}

fn write_cint(out: &mut Vec<u8>, v: i32) {
    let mag = v.unsigned_abs().to_le_bytes();
    let n = mag.iter().rposition(|&b| b != 0).map_or(0, |i| i + 1);
    out.push(n as u8 | if v < 0 { 0x80 } else { 0 });
    out.extend_from_slice(&mag[..n]);
}

struct Reader<'a> {
    data: &'a [u8],
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        if n > self.data.len() {
            return Err(Error::Truncated);
        }
        let (head, tail) = self.data.split_at(n);
        self.data = tail;
        Ok(head)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        Ok(self.take(N)?.try_into().expect("take returned N bytes"))
    }

    fn byte(&mut self) -> Result<u8, Error> {
        Ok(self.take(1)?[0])
    }

    fn cint(&mut self) -> Result<i32, Error> {
        let size = self.byte()?;
        let n = (size & 0x7f) as usize;
        if n > 4 {
            return Err(Error::BadCompressedInt);
        }
        let mut b = [0u8; 4];
        b[..n].copy_from_slice(self.take(n)?);
        let v = i32::from_le_bytes(b);
        Ok(if size & 0x80 != 0 {
            v.wrapping_neg()
        } else {
            v
        })
    }

    fn string(&mut self) -> Result<String, Error> {
        let end = self
            .data
            .iter()
            .position(|&c| c == 0)
            .ok_or(Error::Truncated)?;
        let s = String::from_utf8_lossy(&self.data[..end]).into_owned();
        self.data = &self.data[end + 1..];
        Ok(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected bytes from JUCE 7.0.8 OutputStream::writeCompressedInt (.locki/tmp/vt-ref/golden.txt).
    #[test]
    fn compressed_int_matches_juce() {
        let cases: &[(i32, &[u8])] = &[
            (0, &[0x00]),
            (1, &[0x01, 0x01]),
            (255, &[0x01, 0xff]),
            (256, &[0x02, 0x00, 0x01]),
            (65536, &[0x03, 0x00, 0x00, 0x01]),
            (i32::MAX, &[0x04, 0xff, 0xff, 0xff, 0x7f]),
            (-1, &[0x81, 0x01]),
            (-256, &[0x82, 0x00, 0x01]),
        ];
        for &(v, bytes) in cases {
            let mut out = Vec::new();
            write_cint(&mut out, v);
            assert_eq!(out, bytes, "encode {v}");
            assert_eq!(Reader { data: bytes }.cint(), Ok(v), "decode {v}");
        }
        assert_eq!(
            Reader {
                data: &[0x05, 0, 0, 0, 0, 0]
            }
            .cint(),
            Err(Error::BadCompressedInt)
        );
    }
}

#[cfg(test)]
mod conversion_tests {
    use super::*;

    // JUCE var conversions (juce_Variant.cpp:136-300): strings go through getIntValue /
    // getDoubleValue, which read a numeric prefix.
    #[test]
    fn var_conversions_match_juce() {
        let s = |x: &str| Var::String(x.into());
        assert_eq!(
            (
                s("1.0").to_i32(),
                s("12abc").to_i32(),
                s(" -3x").to_i32(),
                s("+3").to_i32()
            ),
            (1, 12, -3, 0)
        );
        assert_eq!(
            (s("12ms").to_f64(), s("1e3x").to_f64(), s("x").to_f64()),
            (12.0, 1000.0, 0.0)
        );
        assert_eq!(
            (
                Var::Double(2.9).to_i32(),
                Var::Double(-2.9).to_i32(),
                Var::Int64(1 << 32 | 5).to_i32()
            ),
            (2, -2, 5)
        );
        assert!(
            s("2.9").to_bool()
                && s(" Yes ").to_bool()
                && s("TRUE").to_bool()
                && !s("0.9").to_bool()
        );
        assert!(
            Var::Array(vec![]).to_bool()
                && !Var::Binary(vec![1]).to_bool()
                && !Var::Undefined.to_bool()
        );
        assert_eq!(s("99999999999").to_i64(), 99_999_999_999);
    }

    #[test]
    fn binary_read_duplicate_property_is_last_value_first_position() {
        // JUCE readFromStream uses NamedValueSet::set (juce_ValueTree.cpp:1062).
        let mut t = ValueTree::new("T");
        t.properties = vec![
            ("a".into(), Var::Int(1)),
            ("b".into(), Var::Int(2)),
            ("a".into(), Var::Int(3)),
        ];
        let mut bytes = Vec::new();
        t.write(&mut bytes);
        let back = ValueTree::read(&bytes).unwrap();
        assert_eq!(
            back.properties,
            vec![("a".into(), Var::Int(3)), ("b".into(), Var::Int(2))]
        );
    }
}
