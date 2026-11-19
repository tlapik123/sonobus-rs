//! JUCE XML, base64 and XML-in-binary encodings, byte-exact with JUCE 7.0.8 as bundled in
//! SonoBus 1.7.2 (`deps/juce/modules`).
//!
//! - [`XmlElement::to_xml_string`]: `XmlElement::writeTo`/`toString` (juce_XmlElement.cpp:170-420):
//!   attributes in insertion order, `\r\n` newlines, 2-space indent, attributes wrapped once a line
//!   passes 60 bytes, every char outside a small ASCII set written as `&#N;`.
//! - [`XmlElement::parse`]: a port of the lenient `XmlDocument` parser (juce_XmlDocument.cpp:194-760).
//!   It is hand-written rather than a crate because JUCE writes binary vars as `base64:name`
//!   attributes, which namespace-aware parsers reject as an undeclared prefix.
//! - [`ValueTree::to_xml`]/[`ValueTree::from_xml`]: `ValueTree::createXml`/`fromXml`
//!   (juce_ValueTree.cpp:367-377, 995-1024) and `NamedValueSet::copyToXmlAttributes`/
//!   `setFromXmlAttributes` (juce_NamedValueSet.cpp:264-304). XML keeps no types: every property
//!   reads back as [`Var::String`], except `base64:`-prefixed attributes, which become
//!   [`Var::Binary`]. Parsing is linear in the input: repeated attribute names are kept, as JUCE
//!   keeps them, and lookups return the first.
//! - [`to_base64`]/[`from_base64`]: `MemoryBlock::toBase64Encoding`/`fromBase64Encoding`
//!   (juce_MemoryBlock.cpp:354-411): NOT RFC 4648. `<byte count>.` then 6-bit groups taken
//!   LSB-first from the bytes, alphabet `.A-Za-z0-9+`.
//! - [`copy_xml_to_binary`]/[`get_xml_from_binary`]: `AudioProcessor::copyXmlToBinary`/
//!   `getXmlFromBinary` (juce_AudioProcessor.cpp:924-953).

use crate::valuetree::{MAX_DEPTH, ValueTree, Var, set_all};
use std::fmt;

/// Errors from parsing any of the JUCE text/file formats in this crate.
#[derive(Debug, Clone, PartialEq)]
pub enum Error {
    /// Malformed XML; the text says where JUCE's parser would have given up.
    Xml(&'static str),
    /// Elements nested deeper than [`MAX_DEPTH`].
    TooDeep,
    /// Not JUCE base64 (no `.`, negative or impossible byte count).
    Base64,
    /// Data without the `copyXmlToBinary` magic number and length.
    NotXmlBinary,
    /// A file whose root element is not the one the format requires.
    WrongRoot(String),
    /// A binary `ValueTree` that does not decode.
    ValueTree(crate::valuetree::Error),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Xml(e) => write!(f, "malformed XML: {e}"),
            Error::TooDeep => f.write_str("XML nested too deeply"),
            Error::Base64 => f.write_str("not JUCE base64"),
            Error::NotXmlBinary => f.write_str("not a JUCE XML binary block"),
            Error::WrongRoot(r) => write!(f, "unexpected root element <{r}>"),
            Error::ValueTree(e) => e.fmt(f),
        }
    }
}

impl std::error::Error for Error {}

impl From<crate::valuetree::Error> for Error {
    fn from(e: crate::valuetree::Error) -> Self {
        Error::ValueTree(e)
    }
}

/// A child of an [`XmlElement`].
#[derive(Debug, Clone, PartialEq)]
pub enum XmlNode {
    Element(XmlElement),
    Text(String),
}

/// A JUCE `XmlElement` (non-text).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct XmlElement {
    pub name: String,
    /// In insertion order. [`XmlElement::set_attribute`] keeps names unique; a parsed element keeps
    /// repeated names as JUCE's parser does, and [`XmlElement::attribute`] returns the first.
    pub attributes: Vec<(String, String)>,
    pub children: Vec<XmlNode>,
}

/// The parts of `XmlElement::TextFormat` SonoBus uses (newlines are always `\r\n`, wrap 60).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextFormat {
    pub header: bool,
    pub multi_line: bool,
}

impl TextFormat {
    /// `TextFormat()`: header, indented, `\r\n`. Used by `toXmlString()` and `writeTo(file)`.
    pub const DEFAULT: TextFormat = TextFormat { header: true, multi_line: true };
    /// `TextFormat().singleLine()` (used by `copyXmlToBinary`).
    pub const SINGLE_LINE: TextFormat = TextFormat { header: true, multi_line: false };
    /// `TextFormat().singleLine().withoutHeader()` (how `PropertySet` stores XML values).
    pub const SINGLE_LINE_NO_HEADER: TextFormat = TextFormat { header: false, multi_line: false };
}

const NEWLINE: &str = "\r\n";
const WRAP: usize = 60;

impl XmlElement {
    pub fn new(name: impl Into<String>) -> Self {
        XmlElement { name: name.into(), ..Default::default() }
    }

    pub fn attribute(&self, name: &str) -> Option<&str> {
        self.attributes.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }

    /// JUCE `setAttribute`: replaces in place or appends (linear in the attribute count).
    pub fn set_attribute(&mut self, name: &str, value: impl Into<String>) {
        let value = value.into();
        match self.attributes.iter_mut().find(|(n, _)| n == name) {
            Some(a) => a.1 = value,
            None => self.attributes.push((name.into(), value)),
        }
    }

    pub fn child_elements(&self) -> impl Iterator<Item = &XmlElement> {
        self.children.iter().filter_map(|c| match c {
            XmlNode::Element(e) => Some(e),
            XmlNode::Text(_) => None,
        })
    }

    /// `XmlElement::toString(format)`.
    pub fn to_xml_string(&self, format: TextFormat) -> String {
        let mut out = String::new();
        if format.header {
            out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>");
            out.push_str(if format.multi_line { "\r\n\r\n" } else { " " });
        }
        self.write(&mut out, if format.multi_line { 0 } else { -1 });
        if format.multi_line {
            out.push_str(NEWLINE);
        }
        out
    }

    /// `XmlElement::writeElementAsText`; `indent < 0` means single line.
    fn write(&self, out: &mut String, indent: i32) {
        spaces(out, indent);
        out.push('<');
        out.push_str(&self.name);
        let att_indent = indent + self.name.chars().count() as i32 + 1;
        let mut line_len = 0;
        for (name, value) in &self.attributes {
            if line_len > WRAP && indent >= 0 {
                out.push_str(NEWLINE);
                spaces(out, att_indent);
                line_len = 0;
            }
            let start = out.len();
            out.push(' ');
            out.push_str(name);
            out.push_str("=\"");
            escape(out, value, true);
            out.push('"');
            line_len += out.len() - start;
        }
        if self.children.is_empty() {
            out.push_str("/>");
            return;
        }
        out.push('>');
        let mut last_was_text = false;
        for child in &self.children {
            match child {
                XmlNode::Text(t) => {
                    escape(out, t, false);
                    last_was_text = true;
                }
                XmlNode::Element(e) => {
                    if indent >= 0 && !last_was_text {
                        out.push_str(NEWLINE);
                    }
                    let child_indent = if last_was_text {
                        0
                    } else if indent >= 0 {
                        indent + 2
                    } else {
                        indent
                    };
                    e.write(out, child_indent);
                    last_was_text = false;
                }
            }
        }
        if indent >= 0 && !last_was_text {
            out.push_str(NEWLINE);
            spaces(out, indent);
        }
        out.push_str("</");
        out.push_str(&self.name);
        out.push('>');
    }

    /// JUCE `parseXML(text)`: header, DTD, comments and processing instructions are skipped,
    /// whitespace-only text is dropped, content after the root element is ignored. The text ends
    /// at the first NUL, as a JUCE `String` would.
    pub fn parse(text: &str) -> Result<XmlElement, Error> {
        let text = text.split('\0').next().unwrap_or_default();
        let text = text.strip_prefix('\u{feff}').unwrap_or(text);
        let mut p = Parser { s: text, pos: 0 };
        p.skip_ws();
        if p.rest().starts_with("<?xml") {
            let end = p.rest().find("?>").ok_or(Error::Xml("malformed header"))?;
            p.pos += end + 2;
        }
        p.skip_ws();
        if p.rest().starts_with("<!DOCTYPE") {
            p.pos += 9;
            let mut depth = 1;
            while depth > 0 {
                match p.next().ok_or(Error::Xml("malformed DTD"))? {
                    '<' => depth += 1,
                    '>' => depth -= 1,
                    _ => {}
                }
            }
        }
        p.read_element(0)
    }
}

fn spaces(out: &mut String, n: i32) {
    for _ in 0..n.max(0) {
        out.push(' ');
    }
}

/// `XmlOutputFunctions::escapeIllegalXmlChars`.
pub(crate) fn escape(out: &mut String, text: &str, change_new_lines: bool) {
    for c in text.chars() {
        let legal = c.is_ascii_alphanumeric() || " .,;:-()_+=?!$#@[]/|*%~{}'\\".contains(c);
        match c {
            _ if legal => out.push(c),
            '&' => out.push_str("&amp;"),
            '"' => out.push_str("&quot;"),
            '>' => out.push_str("&gt;"),
            '<' => out.push_str("&lt;"),
            '\n' | '\r' if !change_new_lines => out.push(c),
            _ => {
                out.push_str("&#");
                out.push_str(&(c as u32).to_string());
                out.push(';');
            }
        }
    }
}

struct Parser<'a> {
    s: &'a str,
    pos: usize,
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '-' | ':' | '.')
}

impl Parser<'_> {
    fn rest(&self) -> &str {
        &self.s[self.pos..]
    }

    fn peek(&self) -> Option<char> {
        self.rest().chars().next()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += c.len_utf8();
        Some(c)
    }

    fn skip_to(&mut self, end: &str, err: &'static str) -> Result<(), Error> {
        let i = self.rest().find(end).ok_or(Error::Xml(err))?;
        self.pos += i + end.len();
        Ok(())
    }

    /// `skipNextWhiteSpace`: whitespace, comments and `<?...?>`.
    fn skip_ws(&mut self) {
        loop {
            let r = self.rest();
            let t = r.trim_start();
            self.pos += r.len() - t.len();
            let r = self.rest();
            let closing = if r.starts_with("<!--") {
                "-->"
            } else if r.starts_with("<?") {
                "?>"
            } else {
                return;
            };
            match r[2..].find(closing) {
                Some(i) => self.pos += 2 + i + closing.len(),
                None => {
                    self.pos = self.s.len();
                    return;
                }
            }
        }
    }

    fn token(&mut self) -> &str {
        let start = self.pos;
        while self.peek().is_some_and(is_ident) {
            self.next();
        }
        &self.s[start..self.pos]
    }

    fn read_element(&mut self, depth: usize) -> Result<XmlElement, Error> {
        if depth > MAX_DEPTH {
            return Err(Error::TooDeep);
        }
        self.skip_ws();
        if self.next() != Some('<') {
            return Err(Error::Xml("expected an element"));
        }
        let mut name = self.token().to_string();
        if name.is_empty() {
            self.skip_ws();
            name = self.token().to_string();
            if name.is_empty() {
                return Err(Error::Xml("tag name missing"));
            }
        }
        let mut el = XmlElement::new(name);
        loop {
            self.skip_ws();
            let r = self.rest();
            if r.starts_with("/>") {
                self.pos += 2;
                return Ok(el);
            }
            if r.starts_with('>') {
                self.pos += 1;
                self.read_children(&mut el, depth)?;
                return Ok(el);
            }
            let att = self.token().to_string();
            if att.is_empty() {
                return Err(Error::Xml("illegal character in element"));
            }
            self.skip_ws();
            if self.next() != Some('=') {
                return Err(Error::Xml("expected '=' after attribute"));
            }
            self.skip_ws();
            let quote = match self.next() {
                Some(q @ ('"' | '\'')) => q,
                _ => return Err(Error::Xml("expected quoted attribute value")),
            };
            let mut value = String::new();
            loop {
                match self.next().ok_or(Error::Xml("unmatched quotes"))? {
                    c if c == quote => break,
                    '&' => self.read_entity(&mut value)?,
                    c => value.push(c),
                }
            }
            el.attributes.push((att, value)); // XmlDocument appends without checking

        }
    }

    /// `readChildElements`, including the closing tag (whose name JUCE does not check).
    fn read_children(&mut self, parent: &mut XmlElement, depth: usize) -> Result<(), Error> {
        loop {
            let before_ws = self.pos;
            self.skip_ws();
            let r = self.rest();
            if r.is_empty() {
                return Err(Error::Xml("unmatched tags"));
            }
            if r.starts_with("</") {
                return self.skip_to(">", "unmatched tags");
            }
            if r.starts_with("<![CDATA[") {
                self.pos += 9;
                let end = self.rest().find("]]>").ok_or(Error::Xml("unterminated CDATA section"))?;
                parent.children.push(XmlNode::Text(self.rest()[..end].to_string()));
                self.pos += end + 3;
            } else if r.starts_with('<') {
                parent.children.push(XmlNode::Element(self.read_element(depth + 1)?));
            } else {
                self.pos = before_ws;
                let mut text = String::new();
                loop {
                    let r = self.rest();
                    if r.starts_with("<!--") {
                        self.skip_to("-->", "unterminated comment")?;
                        continue;
                    }
                    match self.next() {
                        None => return Err(Error::Xml("unmatched tags")),
                        Some('<') => {
                            self.pos -= 1;
                            break;
                        }
                        Some('&') => self.read_entity(&mut text)?,
                        Some('\r') => {
                            if self.peek() != Some('\n') {
                                text.push('\n');
                            }
                        }
                        Some(c) => text.push(c),
                    }
                }
                if !text.trim().is_empty() {
                    parent.children.push(XmlNode::Text(text));
                }
            }
        }
    }

    /// `readEntity`, after the `&`.
    fn read_entity(&mut self, out: &mut String) -> Result<(), Error> {
        let semi = self.rest().find(';').ok_or(Error::Xml("unterminated entity"))?;
        let ent = &self.rest()[..semi];
        let c = match ent.to_ascii_lowercase().as_str() {
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            "lt" => '<',
            "gt" => '>',
            e if e.starts_with("#x") => num_entity(&e[2..], 16)?,
            e if e.starts_with('#') => num_entity(&e[1..], 10)?,
            _ => {
                // JUCE would try a DTD here; SonoBus files have none, so keep the text.
                out.push('&');
                out.push_str(ent);
                out.push(';');
                self.pos += semi + 1;
                return Ok(());
            }
        };
        out.push(c);
        self.pos += semi + 1;
        Ok(())
    }
}

fn num_entity(digits: &str, radix: u32) -> Result<char, Error> {
    let code = u32::from_str_radix(digits, radix).map_err(|_| Error::Xml("illegal escape sequence"))?;
    Ok(char::from_u32(code).unwrap_or(char::REPLACEMENT_CHARACTER))
}

/// JUCE `var::toString()`.
pub fn var_to_string(v: &Var) -> String {
    match v {
        Var::Void => String::new(),
        Var::Undefined => "undefined".into(),
        Var::Int(i) => i.to_string(),
        Var::Int64(i) => i.to_string(),
        Var::Bool(b) => if *b { "1" } else { "0" }.into(),
        Var::Double(d) => serialise_double(*d),
        Var::String(s) => s.clone(),
        Var::Binary(b) => to_base64(b),
        Var::Array(_) => "[Array]".into(),
    }
}

/// JUCE `serialiseDouble` (juce_String.cpp:2262-2300), how doubles appear in XML.
pub fn serialise_double(x: f64) -> String {
    if !x.is_finite() {
        return if x.is_nan() { "nan" } else if x > 0.0 { "inf" } else { "-inf" }.into();
    }
    let a = x.abs();
    if a >= 1.0e6 || a <= 1.0e-5 {
        // std::ostream scientific with precision 15: mantissa "d.ddd" + "e+XX" (>= 2 exp digits)
        let s = format!("{x:.15e}");
        let (mant, exp) = s.split_once('e').expect("Rust {:e} has an exponent");
        let exp: i32 = exp.parse().expect("Rust {:e} exponent is an integer");
        let sign = if exp < 0 { '-' } else { '+' };
        return reduce_float_string(&format!("{mant}e{sign}{:02}", exp.abs()));
    }
    if (x as i32) as f64 == x {
        return format!("{x:.1}");
    }
    let places = if a < 1.0 {
        match a {
            _ if a >= 1.0e-1 => 16,
            _ if a >= 1.0e-2 => 17,
            _ if a >= 1.0e-3 => 18,
            _ if a >= 1.0e-4 => 19,
            _ => 20,
        }
    } else {
        match a {
            _ if a < 1.0e1 => 15,
            _ if a < 1.0e2 => 14,
            _ if a < 1.0e3 => 13,
            _ if a < 1.0e4 => 12,
            _ if a < 1.0e5 => 11,
            _ => 10,
        }
    };
    reduce_float_string(&format!("{x:.places$}"))
}

/// `reduceLengthOfFloatString` (juce_String.cpp:2191-2259), ported index for index.
fn reduce_float_string(input: &str) -> String {
    let s = input.as_bytes();
    let end = s.len();
    let (mut trim_start, mut trim_end) = (end, end);
    let (mut exp_start, mut exp_end) = (end, end);
    let mut current = 0u8;
    for c in (1..end).rev() {
        current = s[c];
        if current == b'0' && c + 1 == trim_start {
            trim_start -= 1;
        } else if current == b'.' {
            if trim_start == c + 1 && trim_start != end && s[trim_start] == b'0' {
                trim_start += 1;
            }
            break;
        } else if current == b'e' || current == b'E' {
            let mut n = c + 1;
            if n != end {
                if s[n] == b'-' {
                    n += 1;
                }
                exp_start = n;
                if n != end && s[n] == b'+' {
                    n += 1;
                }
                exp_end = n;
            }
            while n != end {
                let ch = s[n];
                n += 1;
                if ch != b'0' {
                    break;
                }
                exp_end = n;
            }
            if exp_end == end {
                exp_start = c;
            }
            trim_start = c;
            trim_end = c;
        }
    }
    if (trim_start != trim_end && current == b'.') || exp_start != exp_end {
        let cat = |parts: &[&[u8]]| String::from_utf8(parts.concat()).expect("ASCII number");
        if trim_start == trim_end {
            return cat(&[&s[..exp_start], &s[exp_end..]]);
        }
        if exp_start == exp_end {
            return cat(&[&s[..trim_start], &s[trim_end..]]);
        }
        if trim_end == exp_start {
            return cat(&[&s[..trim_start], &s[exp_end..]]);
        }
        return cat(&[&s[..trim_start], &s[trim_end..exp_start], &s[exp_end..]]);
    }
    input.to_string()
}

const B64: &[u8; 64] = b".ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+";

/// `MemoryBlock::toBase64Encoding`.
pub fn to_base64(data: &[u8]) -> String {
    let bit = |b: usize| data.get(b / 8).is_some_and(|byte| byte >> (b % 8) & 1 == 1);
    let mut s = format!("{}.", data.len());
    for i in 0..(data.len() * 8).div_ceil(6) {
        let v = (0..6).fold(0, |acc, k| acc | (bit(i * 6 + k) as usize) << k);
        s.push(B64[v] as char);
    }
    s
}

/// JUCE `base64DecodingTable` (juce_MemoryBlock.cpp:376-381), indexed by `char - 43`.
const B64_DECODE: [u8; 80] = [
    63, 0, 0, 0, 0, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 0, 0, 0, 0, 0, 0, 0, //
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, //
    0, 0, 0, 0, 0, 0, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47, 48,
    49, 50, 51, 52,
];

/// `MemoryBlock::fromBase64Encoding`. As in JUCE, every char in 43..=122 (`+` to `z`) takes 6
/// bits, the ones outside the alphabet (`, - / : ; < = > ? @ [ \ ] ^ _` and backtick) as 0;
/// every other char (whitespace, `{`, non-ASCII) is skipped; the text ends at NUL. Unlike JUCE,
/// a byte count larger than the text could ever fill is an error rather than a zero-filled
/// allocation of that size.
pub fn from_base64(s: &str) -> Option<Vec<u8>> {
    let s = s.split('\0').next().unwrap_or_default();
    let (count, chars) = s.split_once('.')?;
    let n = usize::try_from(juce_int(count)).ok()?;
    if n > chars.len() {
        return None;
    }
    let mut out = vec![0u8; n];
    let mut pos = 0;
    for c in chars.chars() {
        let Some(&v) = (c as usize).checked_sub(43).and_then(|i| B64_DECODE.get(i)) else { continue };
        for k in 0..6 {
            let b = pos + k;
            if v >> k & 1 == 1 && b / 8 < n {
                out[b / 8] |= 1 << (b % 8);
            }
        }
        pos += 6;
    }
    Some(out)
}

/// JUCE `String::getIntValue`: leading whitespace, optional `-`, then digits (wrapping).
pub(crate) fn juce_int(s: &str) -> i64 {
    let s = s.trim_start();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, s),
    };
    let v = digits
        .bytes()
        .take_while(u8::is_ascii_digit)
        .fold(0u64, |v, d| v.wrapping_mul(10).wrapping_add(u64::from(d - b'0')));
    if neg { (v as i64).wrapping_neg() } else { v as i64 }
}

/// JUCE `var` string-to-bool (juce_Variant.cpp:266-271): `getIntValue() != 0`, or "true"/"yes"
/// after trimming, ignoring case.
pub(crate) fn juce_str_bool(s: &str) -> bool {
    let t = s.trim();
    juce_int(s) as i32 != 0 || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("yes")
}

/// JUCE `String::getDoubleValue`: the longest numeric prefix after whitespace, else 0.
pub(crate) fn juce_double(s: &str) -> f64 {
    let s = s.trim_start();
    let b = s.as_bytes();
    let digits = |mut i: usize| {
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
        i
    };
    let mut i = usize::from(matches!(b.first(), Some(b'-' | b'+')));
    let lower = s[i..].to_ascii_lowercase();
    if lower.starts_with("nan") {
        return f64::NAN;
    }
    if lower.starts_with("inf") {
        return if s.starts_with('-') { f64::NEG_INFINITY } else { f64::INFINITY };
    }
    i = digits(i);
    if b.get(i) == Some(&b'.') {
        i = digits(i + 1);
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'-' | b'+')) {
            j += 1;
        }
        let k = digits(j);
        if k > j {
            i = k;
        }
    }
    s[..i].parse().unwrap_or(0.0)
}

impl ValueTree {
    /// `ValueTree::createXml`; `None` for an invalid tree. Invalid children are skipped.
    pub fn to_xml(&self) -> Option<XmlElement> {
        if !self.is_valid() {
            return None;
        }
        let mut xml = XmlElement::new(self.type_name.clone());
        xml.attributes = set_all(self.properties.iter().map(|(name, value)| match value {
            Var::Binary(b) => (format!("base64:{name}"), to_base64(b)),
            v => (name.clone(), var_to_string(v)),
        }));
        xml.children = self.children.iter().filter_map(|c| c.to_xml().map(XmlNode::Element)).collect();
        Some(xml)
    }

    /// `ValueTree::fromXml(const XmlElement&)`. Text children are skipped, as in JUCE.
    pub fn from_xml(xml: &XmlElement) -> ValueTree {
        let mut v = ValueTree::new(xml.name.clone());
        // `NamedValueSet::setFromXmlAttributes` appends: repeated names are kept.
        v.properties = xml
            .attributes
            .iter()
            .map(|(name, value)| match name.strip_prefix("base64:").and_then(|n| Some((n, from_base64(value)?))) {
                Some((n, bytes)) => (n.to_string(), Var::Binary(bytes)),
                None => (name.clone(), Var::String(value.clone())),
            })
            .collect();
        v.children = xml.child_elements().map(ValueTree::from_xml).collect();
        v
    }

    /// `ValueTree::toXmlString()` (default format); empty for an invalid tree.
    pub fn to_xml_string(&self) -> String {
        self.to_xml().map(|x| x.to_xml_string(TextFormat::DEFAULT)).unwrap_or_default()
    }

    /// `ValueTree::fromXml(const String&)`, with the parse error JUCE swallows.
    pub fn from_xml_str(text: &str) -> Result<ValueTree, Error> {
        Ok(ValueTree::from_xml(&XmlElement::parse(text)?))
    }
}

const MAGIC_XML: u32 = 0x2132_4356;

/// `AudioProcessor::copyXmlToBinary`: magic `0x21324356` LE, u32 LE length, single-line XML with
/// header, NUL.
pub fn copy_xml_to_binary(xml: &XmlElement) -> Vec<u8> {
    let text = xml.to_xml_string(TextFormat::SINGLE_LINE);
    let mut out = Vec::with_capacity(text.len() + 9);
    out.extend_from_slice(&MAGIC_XML.to_le_bytes());
    out.extend_from_slice(&(text.len() as u32).to_le_bytes());
    out.extend_from_slice(text.as_bytes());
    out.push(0);
    out
}

/// `AudioProcessor::getXmlFromBinary`.
pub fn get_xml_from_binary(data: &[u8]) -> Result<XmlElement, Error> {
    if data.len() <= 8 || data[..4] != MAGIC_XML.to_le_bytes() {
        return Err(Error::NotXmlBinary);
    }
    let len = i32::from_le_bytes(data[4..8].try_into().expect("4 bytes"));
    if len <= 0 {
        return Err(Error::NotXmlBinary);
    }
    let body = &data[8..data.len().min(8 + len as usize)];
    XmlElement::parse(&String::from_utf8_lossy(body))
}

#[cfg(test)]
mod tests {
    use super::*;

    // Expected strings from JUCE's own unit tests (juce_String.cpp:2900-2950,
    // juce_ValueTree.cpp:1219-1240).
    #[test]
    fn serialise_double_matches_juce_tests() {
        for (x, s) in [
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
            (0.00012345678901234567, "0.0001234567890123457"),
            (0.0000012345678901234567, "1.234567890123457e-6"),
            (0.0, "0.0"),
        ] {
            assert_eq!(serialise_double(x), s, "{x}");
        }
    }

    #[test]
    fn reduce_matches_juce_tests() {
        for (x, s) in [(0.76378, "7.6378e-1"), (-10.0, "-1.0e1"), (10691.01, "1.069101e4"), (0.0123, "1.23e-2")] {
            let sci = format!("{x:.15e}");
            let (m, e) = sci.split_once('e').unwrap();
            let e: i32 = e.parse().unwrap();
            let cpp = format!("{m}e{}{:02}", if e < 0 { '-' } else { '+' }, e.abs());
            assert_eq!(reduce_float_string(&cpp), s);
        }
    }

    #[test]
    fn juce_number_parsing() {
        assert_eq!(juce_int(" -12abc"), -12);
        assert_eq!(juce_int("1.9"), 1);
        assert_eq!(juce_int("+3"), 0);
        assert_eq!(juce_double(" 0.5000000059604645"), 0.5000000059604645);
        assert_eq!(juce_double("-1.0e1x"), -10.0);
        assert_eq!(juce_double("2e"), 2.0);
        assert_eq!(juce_double("abc"), 0.0);
    }

    #[test]
    fn parser_rejects_garbage_without_panicking() {
        for s in ["", "abc", "<", "<a", "<a b", "<a b=", "<a b=\"x", "<a>", "<a><b></a", "<a>&amp", "<a>&#zz;</a>", "<?xml", "<!DOCTYPE x"] {
            assert!(XmlElement::parse(s).is_err(), "{s:?}");
        }
        let deep = "<a>".repeat(MAX_DEPTH + 2);
        assert_eq!(XmlElement::parse(&deep), Err(Error::TooDeep));
        assert_eq!(from_base64("x"), None);
        assert_eq!(from_base64("-1."), None);
        assert_eq!(from_base64("99999999999."), None);
        assert_eq!(get_xml_from_binary(&[0x56, 0x43, 0x32, 0x21, 0, 0, 0, 0, 0]), Err(Error::NotXmlBinary));
    }

    // juce_MemoryBlock.cpp:376-410: chars 43..=122 index the decoding table (the gaps decode as
    // 0 and still take 6 bits); everything else, whitespace included, is skipped.
    #[test]
    fn base64_decodes_like_juce() {
        for (s, b) in [("1.-A", 0x40), ("1.,A", 0x40), ("1.@A", 0x40), ("1.`A", 0x40), ("1._A", 0x40), ("1./A", 0x40)] {
            assert_eq!(from_base64(s), Some(vec![b]), "{s}");
        }
        for s in ["1. A", "1.\r\nA", "1.{A", "1.*A", "1.éA", "1.A"] {
            assert_eq!(from_base64(s), Some(vec![1]), "{s}");
        }
        assert_eq!(from_base64("2.+.9"), Some(vec![0x3f, 0xe0]));
        assert_eq!(from_base64("1.A\0B"), Some(vec![1]));
    }

    #[test]
    fn duplicate_attributes_first_wins() {
        // XmlDocument appends every attribute; getStringAttribute and NamedValueSet::getVarPointer
        // return the first match; setFromXmlAttributes keeps all (juce_NamedValueSet.cpp:264-283).
        let x = XmlElement::parse("<a x=\"1\" y=\"2\" x=\"3\"/>").unwrap();
        assert_eq!(x.attribute("x"), Some("1"));
        assert_eq!(x.attributes.len(), 3);
        let t = ValueTree::from_xml(&x);
        assert_eq!(t.get("x"), Some(&Var::String("1".into())));
        assert_eq!(t.properties.len(), 3);
        // createXml uses setAttribute: first position, last value
        assert_eq!(t.to_xml().unwrap().attributes, vec![("x".into(), "3".into()), ("y".into(), "2".into())]);
    }

    #[test]
    fn parser_handles_juce_leniency() {
        let x = XmlElement::parse("\u{feff}<?xml version=\"1.0\"?>\r\n<!-- c --><a  q='1' b=\"&lt;&#233;&#x41;&foo;\"><!--x-->\r\n  <c/>t&amp;\r\nu</wrong>trailing\0junk").unwrap();
        assert_eq!(x.attributes, vec![("q".into(), "1".into()), ("b".into(), "<éA&foo;".into())]);
        assert_eq!(x.children, vec![XmlNode::Element(XmlElement::new("c")), XmlNode::Text("t&\nu".into())]);
    }
}
