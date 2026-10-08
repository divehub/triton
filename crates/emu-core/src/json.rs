//! Minimal JSON value type with a strict parser and a writer (no serde).
//!
//! Used for state snapshots, `rtc-state.json`, `inputs.json`, `led-colors.json`
//! and the WebAssembly API. Design points:
//!
//! * Integers keep full 64-bit precision (`Int` for the `i64` range, `UInt` above
//!   it); only numbers with a fraction or exponent become `Float`.
//! * Objects keep insertion order (deterministic output, matches Python's
//!   `json.dump` for dictionaries built in order). Lookup is linear; objects in
//!   this project are small.
//! * The parser follows RFC 8259 strictly: no trailing commas, no comments, no
//!   leading zeros, no raw control characters in strings, no lone surrogate
//!   escapes, bounded nesting depth. Duplicate keys are accepted (last wins)
//!   unless `ParseOptions::reject_duplicate_keys` is set, which mirrors the
//!   `object_pairs_hook` used by `emulation/rtc_persistence.py`.
//! * `WriteOptions` can reproduce Python's `json.dump(..., indent=2)` layout and
//!   `ensure_ascii` escaping.

use std::fmt;
use std::fmt::Write as _;

/// Maximum nesting depth accepted by the parser (guards the WebAssembly stack).
pub const MAX_DEPTH: usize = 128;

/// A JSON value. `PartialEq` is structural: object member order matters and
/// `Int(1) != Float(1.0)`.
#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    /// Integer within the `i64` range.
    Int(i64),
    /// Integer above `i64::MAX` (produced by the parser and by `From<u64>` only when needed).
    UInt(u64),
    Float(f64),
    Str(String),
    Array(Vec<Json>),
    /// Members in insertion order.
    Object(Vec<(String, Json)>),
}

/// Parse error with a 1-based line and column.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JsonError {
    pub message: String,
    /// Byte offset into the input.
    pub offset: usize,
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for JsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "JSON error at line {} column {}: {}", self.line, self.column, self.message)
    }
}

impl std::error::Error for JsonError {}

/// Parser options.
#[derive(Clone, Copy, Debug, Default)]
pub struct ParseOptions {
    /// Fail when an object contains the same key twice.
    pub reject_duplicate_keys: bool,
}

/// Writer options.
#[derive(Clone, Copy, Debug, Default)]
pub struct WriteOptions {
    /// `None` writes compact JSON (no whitespace); `Some(n)` pretty-prints with `n` spaces
    /// per level (`": "` after keys, one element per line, `[]`/`{}` for empty containers).
    pub indent: Option<usize>,
    /// Emit object members sorted by key (UTF-8 byte order = code point order).
    pub sort_keys: bool,
    /// Escape every non-ASCII character as `\uXXXX` (Python's default `ensure_ascii=True`).
    pub ensure_ascii: bool,
}

impl WriteOptions {
    pub const fn compact() -> Self {
        Self { indent: None, sort_keys: false, ensure_ascii: false }
    }

    pub const fn pretty(indent: usize) -> Self {
        Self { indent: Some(indent), sort_keys: false, ensure_ascii: false }
    }

    /// Layout of Python's `json.dump(value, file, indent=2)`.
    pub const fn python_indent2() -> Self {
        Self { indent: Some(2), sort_keys: false, ensure_ascii: true }
    }
}

impl Json {
    // ---- construction -------------------------------------------------

    pub fn object() -> Json {
        Json::Object(Vec::new())
    }

    pub fn array() -> Json {
        Json::Array(Vec::new())
    }

    /// Object from `(key, value)` pairs, keeping order; later duplicates replace earlier ones.
    pub fn from_pairs<K, V, I>(pairs: I) -> Json
    where
        K: Into<String>,
        V: Into<Json>,
        I: IntoIterator<Item = (K, V)>,
    {
        let mut object = Json::object();
        for (key, value) in pairs {
            object.insert(key, value);
        }
        object
    }

    /// Array from items.
    pub fn from_items<V, I>(items: I) -> Json
    where
        V: Into<Json>,
        I: IntoIterator<Item = V>,
    {
        Json::Array(items.into_iter().map(Into::into).collect())
    }

    /// Sets `key` on an object (replacing an existing member in place, otherwise appending).
    /// Does nothing on other value kinds. Returns `self` for chaining.
    pub fn insert(&mut self, key: impl Into<String>, value: impl Into<Json>) -> &mut Json {
        if let Json::Object(members) = self {
            let key = key.into();
            let value = value.into();
            if let Some(slot) = members.iter_mut().find(|(k, _)| *k == key) {
                slot.1 = value;
            } else {
                members.push((key, value));
            }
        } else {
            debug_assert!(false, "Json::insert on a non-object");
        }
        self
    }

    /// Appends to an array. Does nothing on other value kinds. Returns `self` for chaining.
    pub fn push(&mut self, value: impl Into<Json>) -> &mut Json {
        if let Json::Array(items) = self {
            items.push(value.into());
        } else {
            debug_assert!(false, "Json::push on a non-array");
        }
        self
    }

    /// Builder-style `insert`.
    pub fn with(mut self, key: impl Into<String>, value: impl Into<Json>) -> Json {
        self.insert(key, value);
        self
    }

    /// Removes and returns an object member.
    pub fn remove(&mut self, key: &str) -> Option<Json> {
        if let Json::Object(members) = self {
            let index = members.iter().position(|(k, _)| k == key)?;
            Some(members.remove(index).1)
        } else {
            None
        }
    }

    // ---- access -------------------------------------------------------

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(members) => members.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Json> {
        match self {
            Json::Object(members) => members.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn at(&self, index: usize) -> Option<&Json> {
        match self {
            Json::Array(items) => items.get(index),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Json::Null)
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// Integer value; a `Float` is accepted only when it is integral and in range.
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(i) => Some(*i),
            Json::UInt(u) => i64::try_from(*u).ok(),
            Json::Float(f) if f.fract() == 0.0 && *f >= -9.223372036854775808e18 && *f < 9.223372036854775808e18 => {
                Some(*f as i64)
            }
            _ => None,
        }
    }

    /// Non-negative integer value; a `Float` is accepted only when it is integral and in range.
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Int(i) => u64::try_from(*i).ok(),
            Json::UInt(u) => Some(*u),
            Json::Float(f) if f.fract() == 0.0 && *f >= 0.0 && *f < 1.8446744073709552e19 => Some(*f as u64),
            _ => None,
        }
    }

    /// Any number as `f64` (large integers lose precision).
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Json::Int(i) => Some(*i as f64),
            Json::UInt(u) => Some(*u as f64),
            Json::Float(f) => Some(*f),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Json]> {
        match self {
            Json::Array(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_object(&self) -> Option<&[(String, Json)]> {
        match self {
            Json::Object(members) => Some(members),
            _ => None,
        }
    }

    /// Element count of an array or object (0 for every other kind).
    pub fn len(&self) -> usize {
        match self {
            Json::Array(items) => items.len(),
            Json::Object(members) => members.len(),
            _ => 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn type_name(&self) -> &'static str {
        match self {
            Json::Null => "null",
            Json::Bool(_) => "boolean",
            Json::Int(_) | Json::UInt(_) | Json::Float(_) => "number",
            Json::Str(_) => "string",
            Json::Array(_) => "array",
            Json::Object(_) => "object",
        }
    }

    // ---- parsing ------------------------------------------------------

    pub fn parse(text: &str) -> Result<Json, JsonError> {
        Json::parse_with(text, ParseOptions::default())
    }

    pub fn parse_with(text: &str, options: ParseOptions) -> Result<Json, JsonError> {
        let mut parser = Parser { text, bytes: text.as_bytes(), pos: 0, options };
        parser.skip_whitespace();
        let value = parser.parse_value(0)?;
        parser.skip_whitespace();
        if parser.pos != parser.bytes.len() {
            return Err(parser.error("unexpected trailing characters"));
        }
        Ok(value)
    }

    // ---- writing ------------------------------------------------------

    /// Appends the serialized value to `out`.
    pub fn write_to(&self, out: &mut String, options: &WriteOptions) {
        self.write_value(out, options, 0);
    }

    pub fn to_string_with(&self, options: &WriteOptions) -> String {
        let mut out = String::new();
        self.write_to(&mut out, options);
        out
    }

    /// Pretty-printed with two-space indentation.
    pub fn to_pretty_string(&self) -> String {
        self.to_string_with(&WriteOptions::pretty(2))
    }

    fn write_value(&self, out: &mut String, options: &WriteOptions, depth: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(i) => {
                let _ = write!(out, "{i}");
            }
            Json::UInt(u) => {
                let _ = write!(out, "{u}");
            }
            Json::Float(f) => {
                if f.is_finite() {
                    // `{:?}` is the shortest representation that round-trips and always
                    // carries a '.' or exponent, so the value re-parses as a float.
                    let _ = write!(out, "{f:?}");
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(s) => write_string(out, s, options.ensure_ascii),
            Json::Array(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    write_newline(out, options, depth + 1);
                    item.write_value(out, options, depth + 1);
                }
                write_newline(out, options, depth);
                out.push(']');
            }
            Json::Object(members) => {
                if members.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push('{');
                let mut order: Vec<usize> = (0..members.len()).collect();
                if options.sort_keys {
                    order.sort_by(|&a, &b| members[a].0.as_bytes().cmp(members[b].0.as_bytes()));
                }
                for (position, &index) in order.iter().enumerate() {
                    if position > 0 {
                        out.push(',');
                    }
                    write_newline(out, options, depth + 1);
                    let (key, value) = &members[index];
                    write_string(out, key, options.ensure_ascii);
                    out.push(':');
                    if options.indent.is_some() {
                        out.push(' ');
                    }
                    value.write_value(out, options, depth + 1);
                }
                write_newline(out, options, depth);
                out.push('}');
            }
        }
    }
}

fn write_newline(out: &mut String, options: &WriteOptions, depth: usize) {
    if let Some(indent) = options.indent {
        out.push('\n');
        for _ in 0..indent * depth {
            out.push(' ');
        }
    }
}

fn write_string(out: &mut String, text: &str, ensure_ascii: bool) {
    out.push('"');
    let bytes = text.as_bytes();
    let mut run_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        let needs_escape = b == b'"' || b == b'\\' || b < 0x20 || (ensure_ascii && b >= 0x80);
        if !needs_escape {
            i += 1;
            continue;
        }
        // Flush the run of characters that can be copied verbatim (cut at ASCII or char boundaries).
        out.push_str(&text[run_start..i]);
        let c = text[i..].chars().next().expect("index is on a char boundary");
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => {
                // Only reachable with ensure_ascii and a non-ASCII character.
                let mut units = [0u16; 2];
                for unit in c.encode_utf16(&mut units) {
                    let _ = write!(out, "\\u{:04x}", unit);
                }
            }
        }
        i += c.len_utf8();
        run_start = i;
    }
    out.push_str(&text[run_start..]);
    out.push('"');
}

impl fmt::Display for Json {
    /// Compact serialization.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_string_with(&WriteOptions::compact()))
    }
}

// ---- conversions ------------------------------------------------------

impl From<bool> for Json {
    fn from(v: bool) -> Json {
        Json::Bool(v)
    }
}

macro_rules! from_signed {
    ($($t:ty),*) => {$(
        impl From<$t> for Json {
            fn from(v: $t) -> Json {
                Json::Int(v as i64)
            }
        }
    )*};
}

macro_rules! from_unsigned_small {
    ($($t:ty),*) => {$(
        impl From<$t> for Json {
            fn from(v: $t) -> Json {
                Json::Int(v as i64)
            }
        }
    )*};
}

from_signed!(i8, i16, i32, i64, isize);
from_unsigned_small!(u8, u16, u32);

impl From<u64> for Json {
    fn from(v: u64) -> Json {
        match i64::try_from(v) {
            Ok(i) => Json::Int(i),
            Err(_) => Json::UInt(v),
        }
    }
}

impl From<usize> for Json {
    fn from(v: usize) -> Json {
        Json::from(v as u64)
    }
}

impl From<f64> for Json {
    fn from(v: f64) -> Json {
        Json::Float(v)
    }
}

impl From<f32> for Json {
    fn from(v: f32) -> Json {
        Json::Float(f64::from(v))
    }
}

impl From<&str> for Json {
    fn from(v: &str) -> Json {
        Json::Str(v.to_string())
    }
}

impl From<String> for Json {
    fn from(v: String) -> Json {
        Json::Str(v)
    }
}

impl From<&String> for Json {
    fn from(v: &String) -> Json {
        Json::Str(v.clone())
    }
}

impl<T: Into<Json>> From<Vec<T>> for Json {
    fn from(v: Vec<T>) -> Json {
        Json::Array(v.into_iter().map(Into::into).collect())
    }
}

impl<T: Into<Json>> From<Option<T>> for Json {
    fn from(v: Option<T>) -> Json {
        match v {
            Some(v) => v.into(),
            None => Json::Null,
        }
    }
}

// ---- parser -----------------------------------------------------------

struct Parser<'a> {
    text: &'a str,
    bytes: &'a [u8],
    pos: usize,
    options: ParseOptions,
}

impl<'a> Parser<'a> {
    fn error(&self, message: &str) -> JsonError {
        self.error_at(self.pos, message)
    }

    fn error_at(&self, offset: usize, message: &str) -> JsonError {
        let offset = offset.min(self.bytes.len());
        let mut line = 1;
        let mut column = 1;
        for &b in &self.bytes[..offset] {
            if b == b'\n' {
                line += 1;
                column = 1;
            } else if (b & 0xC0) != 0x80 {
                // Count characters, not UTF-8 continuation bytes.
                column += 1;
            }
        }
        JsonError { message: message.to_string(), offset, line, column }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_whitespace(&mut self) {
        while let Some(b) = self.peek() {
            if b == b' ' || b == b'\t' || b == b'\n' || b == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    fn expect_literal(&mut self, literal: &str, value: Json) -> Result<Json, JsonError> {
        if self.bytes[self.pos..].starts_with(literal.as_bytes()) {
            self.pos += literal.len();
            Ok(value)
        } else {
            Err(self.error("invalid literal"))
        }
    }

    fn parse_value(&mut self, depth: usize) -> Result<Json, JsonError> {
        if depth > MAX_DEPTH {
            return Err(self.error("nesting too deep"));
        }
        match self.peek() {
            None => Err(self.error("unexpected end of input")),
            Some(b'n') => self.expect_literal("null", Json::Null),
            Some(b't') => self.expect_literal("true", Json::Bool(true)),
            Some(b'f') => self.expect_literal("false", Json::Bool(false)),
            Some(b'"') => Ok(Json::Str(self.parse_string()?)),
            Some(b'[') => self.parse_array(depth),
            Some(b'{') => self.parse_object(depth),
            Some(b'-') | Some(b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error("unexpected character")),
        }
    }

    fn parse_array(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.pos += 1; // '['
        let mut items = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Array(items));
        }
        loop {
            self.skip_whitespace();
            items.push(self.parse_value(depth + 1)?);
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Array(items));
                }
                Some(_) => return Err(self.error("expected ',' or ']'")),
                None => return Err(self.error("unterminated array")),
            }
        }
    }

    fn parse_object(&mut self, depth: usize) -> Result<Json, JsonError> {
        self.pos += 1; // '{'
        let mut members: Vec<(String, Json)> = Vec::new();
        self.skip_whitespace();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Object(members));
        }
        loop {
            self.skip_whitespace();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a string key"));
            }
            let key_offset = self.pos;
            let key = self.parse_string()?;
            self.skip_whitespace();
            if self.peek() != Some(b':') {
                return Err(self.error("expected ':'"));
            }
            self.pos += 1;
            self.skip_whitespace();
            let value = self.parse_value(depth + 1)?;
            if let Some(existing) = members.iter_mut().find(|(k, _)| *k == key) {
                if self.options.reject_duplicate_keys {
                    return Err(self.error_at(key_offset, &format!("duplicate key \"{key}\"")));
                }
                existing.1 = value;
            } else {
                members.push((key, value));
            }
            self.skip_whitespace();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Object(members));
                }
                Some(_) => return Err(self.error("expected ',' or '}'")),
                None => return Err(self.error("unterminated object")),
            }
        }
    }

    fn parse_hex4(&mut self) -> Result<u32, JsonError> {
        if self.pos + 4 > self.bytes.len() {
            return Err(self.error("truncated \\u escape"));
        }
        let mut value = 0u32;
        for i in 0..4 {
            let digit = match self.bytes[self.pos + i] {
                b @ b'0'..=b'9' => b - b'0',
                b @ b'a'..=b'f' => b - b'a' + 10,
                b @ b'A'..=b'F' => b - b'A' + 10,
                _ => return Err(self.error_at(self.pos + i, "invalid hex digit in \\u escape")),
            };
            value = value * 16 + u32::from(digit);
        }
        self.pos += 4;
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<String, JsonError> {
        self.pos += 1; // opening quote
        let mut out = String::new();
        let mut run_start = self.pos;
        loop {
            let b = match self.peek() {
                Some(b) => b,
                None => return Err(self.error("unterminated string")),
            };
            match b {
                b'"' => {
                    out.push_str(&self.text[run_start..self.pos]);
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    out.push_str(&self.text[run_start..self.pos]);
                    self.pos += 1;
                    let escape = match self.peek() {
                        Some(e) => e,
                        None => return Err(self.error("unterminated escape")),
                    };
                    self.pos += 1;
                    match escape {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let escape_start = self.pos - 2;
                            let first = self.parse_hex4()?;
                            let code = if (0xD800..0xDC00).contains(&first) {
                                if self.bytes[self.pos..].starts_with(b"\\u") {
                                    self.pos += 2;
                                    let second = self.parse_hex4()?;
                                    if !(0xDC00..0xE000).contains(&second) {
                                        return Err(self.error_at(escape_start, "high surrogate not followed by a low surrogate"));
                                    }
                                    0x10000 + ((first - 0xD800) << 10) + (second - 0xDC00)
                                } else {
                                    return Err(self.error_at(escape_start, "lone high surrogate"));
                                }
                            } else if (0xDC00..0xE000).contains(&first) {
                                return Err(self.error_at(escape_start, "lone low surrogate"));
                            } else {
                                first
                            };
                            match char::from_u32(code) {
                                Some(c) => out.push(c),
                                None => return Err(self.error_at(escape_start, "invalid code point")),
                            }
                        }
                        _ => return Err(self.error_at(self.pos - 1, "invalid escape")),
                    }
                    run_start = self.pos;
                }
                0x00..=0x1F => return Err(self.error("control character in string")),
                _ => self.pos += 1,
            }
        }
    }

    fn parse_number(&mut self) -> Result<Json, JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.peek() {
            Some(b'0') => {
                self.pos += 1;
                if matches!(self.peek(), Some(b'0'..=b'9')) {
                    return Err(self.error("leading zeros are not allowed"));
                }
            }
            Some(b'1'..=b'9') => {
                while matches!(self.peek(), Some(b'0'..=b'9')) {
                    self.pos += 1;
                }
            }
            _ => return Err(self.error("invalid number")),
        }
        let mut is_float = false;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.pos += 1;
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected digits after '.'"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e') | Some(b'E')) {
            is_float = true;
            self.pos += 1;
            if matches!(self.peek(), Some(b'+') | Some(b'-')) {
                self.pos += 1;
            }
            if !matches!(self.peek(), Some(b'0'..=b'9')) {
                return Err(self.error("expected digits in exponent"));
            }
            while matches!(self.peek(), Some(b'0'..=b'9')) {
                self.pos += 1;
            }
        }
        let literal = &self.text[start..self.pos];
        if !is_float {
            if let Ok(i) = literal.parse::<i64>() {
                return Ok(Json::Int(i));
            }
            if let Ok(u) = literal.parse::<u64>() {
                return Ok(Json::UInt(u));
            }
        }
        match literal.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(Json::Float(f)),
            _ => Err(self.error_at(start, "number out of range")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Json {
        Json::parse(text).unwrap_or_else(|e| panic!("{text:?}: {e}"))
    }

    fn fails(text: &str) -> JsonError {
        match Json::parse(text) {
            Ok(v) => panic!("{text:?} unexpectedly parsed as {v:?}"),
            Err(e) => e,
        }
    }

    #[test]
    fn parses_scalars() {
        assert_eq!(parse("null"), Json::Null);
        assert_eq!(parse(" true "), Json::Bool(true));
        assert_eq!(parse("false"), Json::Bool(false));
        assert_eq!(parse("0"), Json::Int(0));
        assert_eq!(parse("-0"), Json::Int(0));
        assert_eq!(parse("-17"), Json::Int(-17));
        assert_eq!(parse("1.5"), Json::Float(1.5));
        assert_eq!(parse("-2.5e3"), Json::Float(-2500.0));
        assert_eq!(parse("1E2"), Json::Float(100.0));
        assert_eq!(parse("\"hi\""), Json::Str("hi".into()));
    }

    #[test]
    fn integers_keep_64_bit_precision() {
        assert_eq!(parse("9223372036854775807"), Json::Int(i64::MAX));
        assert_eq!(parse("-9223372036854775808"), Json::Int(i64::MIN));
        assert_eq!(parse("9223372036854775808"), Json::UInt(9_223_372_036_854_775_808));
        assert_eq!(parse("18446744073709551615"), Json::UInt(u64::MAX));
        // Beyond u64 falls back to a float rather than failing.
        assert!(matches!(parse("18446744073709551616"), Json::Float(_)));
        assert_eq!(parse("18446744073709551615").as_u64(), Some(u64::MAX));
        assert_eq!(parse("18446744073709551615").as_i64(), None);
        assert_eq!(Json::from(u64::MAX).to_string(), "18446744073709551615");
        assert_eq!(Json::from(5u64), Json::Int(5));
    }

    #[test]
    fn floats_round_trip_and_stay_floats() {
        for value in [0.0, 1.0, -1.0, 0.1, 1e21, 1e-7, 123456.789, f64::MIN_POSITIVE, f64::MAX] {
            let text = Json::Float(value).to_string();
            match parse(&text) {
                Json::Float(parsed) => assert_eq!(parsed.to_bits(), value.to_bits(), "{text}"),
                other => panic!("{text} parsed as {other:?}"),
            }
        }
        assert_eq!(Json::Float(f64::NAN).to_string(), "null");
        assert_eq!(Json::Float(f64::INFINITY).to_string(), "null");
        assert_eq!(parse("2.0").as_u64(), Some(2));
        assert_eq!(parse("2.5").as_u64(), None);
        assert_eq!(parse("-1").as_u64(), None);
    }

    #[test]
    fn parses_containers() {
        let value = parse(r#" { "a": [1, 2, {"b": null}], "c": {}, "d": [] , "e": "x" } "#);
        assert_eq!(value.get("a").unwrap().len(), 3);
        assert_eq!(value.get("a").unwrap().at(2).unwrap().get("b"), Some(&Json::Null));
        assert_eq!(value.get("c"), Some(&Json::Object(vec![])));
        assert_eq!(value.get("d"), Some(&Json::Array(vec![])));
        assert_eq!(value.get("e").unwrap().as_str(), Some("x"));
        assert_eq!(value.get("missing"), None);
        // Member order is preserved.
        let keys: Vec<&str> = value.as_object().unwrap().iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(keys, ["a", "c", "d", "e"]);
    }

    #[test]
    fn string_escapes() {
        assert_eq!(parse(r#""a\"b\\c\/d\b\f\n\r\t""#).as_str().unwrap(), "a\"b\\c/d\u{8}\u{c}\n\r\t");
        assert_eq!(parse(r#""Aé€""#).as_str().unwrap(), "A\u{e9}\u{20ac}");
        // Surrogate pair (U+1F600).
        assert_eq!(parse(r#""😀""#).as_str().unwrap(), "\u{1F600}");
        // Raw non-ASCII passes through.
        assert_eq!(parse("\"caf\u{e9} \u{1F600}\"").as_str().unwrap(), "caf\u{e9} \u{1F600}");
        fails(r#""\ud83d""#);
        fails(r#""\ud83dA""#);
        fails(r#""\ude00""#);
        fails(r#""\x41""#);
        fails(r#""\u12G4""#);
        fails("\"abc\u{1}\"");
        fails("\"abc\ndef\"");
        fails("\"unterminated");
    }

    #[test]
    fn rejects_malformed_documents() {
        for text in [
            "", " ", "nul", "tru", "falsey", "[", "]", "[1,]", "[1 2]", "{", "{\"a\"}", "{\"a\":}", "{\"a\":1,}",
            "{a:1}", "{'a':1}", "01", "-", "+1", "1.", ".5", "1e", "1e+", "--1", "0x10", "1 2", "[] []", "\u{feff}[]",
            "NaN", "Infinity", "[1,,2]", "{\"a\":1 \"b\":2}", "// c\n1", "1e999",
        ] {
            fails(text);
        }
    }

    #[test]
    fn depth_is_bounded() {
        let ok = format!("{}1{}", "[".repeat(MAX_DEPTH), "]".repeat(MAX_DEPTH));
        parse(&ok);
        let deep = format!("{}1{}", "[".repeat(MAX_DEPTH + 2), "]".repeat(MAX_DEPTH + 2));
        assert!(fails(&deep).message.contains("deep"));
    }

    #[test]
    fn error_positions() {
        let e = fails("{\n  \"a\": ?\n}");
        assert_eq!((e.line, e.column), (2, 8));
        let e = fails("[1, 2");
        assert!(e.message.contains("unterminated"));
        assert!(e.to_string().contains("line 1"));
    }

    #[test]
    fn duplicate_keys() {
        let value = parse(r#"{"a":1,"a":2}"#);
        assert_eq!(value.get("a"), Some(&Json::Int(2)));
        assert_eq!(value.len(), 1);
        let strict = ParseOptions { reject_duplicate_keys: true };
        let e = Json::parse_with(r#"{"x":{"a":1,"a":2}}"#, strict).unwrap_err();
        assert!(e.message.contains("duplicate"), "{e}");
        assert!(Json::parse_with(r#"{"a":1,"b":{"a":2}}"#, strict).is_ok());
    }

    #[test]
    fn compact_writer() {
        let value = Json::object()
            .with("name", "x\"y\n")
            .with("n", 3)
            .with("f", 0.5)
            .with("list", Json::from_items([1, 2, 3]))
            .with("nothing", Json::Null)
            .with("flag", true)
            .with("empty", Json::object());
        assert_eq!(
            value.to_string(),
            r#"{"name":"x\"y\n","n":3,"f":0.5,"list":[1,2,3],"nothing":null,"flag":true,"empty":{}}"#
        );
        assert_eq!(Json::parse(&value.to_string()).unwrap(), value);
    }

    #[test]
    fn pretty_writer_matches_python_indent_2() {
        let value = Json::object()
            .with("a", 1)
            .with("b", Json::from_items([1, 2]))
            .with("c", Json::object())
            .with("d", Json::array())
            .with("e", Json::object().with("x", Json::from_items(Vec::<i32>::new())));
        let expected = "{\n  \"a\": 1,\n  \"b\": [\n    1,\n    2\n  ],\n  \"c\": {},\n  \"d\": [],\n  \"e\": {\n    \"x\": []\n  }\n}";
        assert_eq!(value.to_pretty_string(), expected);
        assert_eq!(value.to_string_with(&WriteOptions::python_indent2()), expected);
        assert_eq!(Json::parse(expected).unwrap(), value);
    }

    #[test]
    fn ensure_ascii_and_control_escapes() {
        let value = Json::from("caf\u{e9} \u{1F600} \u{1} \u{7f}");
        assert_eq!(value.to_string_with(&WriteOptions::compact()), "\"caf\u{e9} \u{1F600} \\u0001 \u{7f}\"");
        let ascii = WriteOptions { ensure_ascii: true, ..WriteOptions::compact() };
        assert_eq!(value.to_string_with(&ascii), "\"caf\\u00e9 \\ud83d\\ude00 \\u0001 \u{7f}\"");
        assert_eq!(Json::parse(&value.to_string_with(&ascii)).unwrap(), value);
    }

    #[test]
    fn random_strings_round_trip_in_every_mode() {
        let alphabet = [
            'a', 'Z', '0', ' ', '"', '\\', '/', '\n', '\r', '\t', '\u{8}', '\u{c}', '\u{1}', '\u{1f}', '\u{7f}', '\u{e9}', '\u{20ac}',
            '\u{ffff}', '\u{1F600}', '\u{10FFFF}', '\u{2028}',
        ];
        let mut state = 0x1234_5678_9abc_def0u64;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            (state >> 33) as usize
        };
        for _ in 0..2000 {
            let len = next() % 24;
            let text: String = (0..len).map(|_| alphabet[next() % alphabet.len()]).collect();
            let value = Json::object().with(text.as_str(), text.as_str());
            for options in [
                WriteOptions::compact(),
                WriteOptions::pretty(2),
                WriteOptions::python_indent2(),
                WriteOptions { ensure_ascii: true, sort_keys: true, indent: None },
            ] {
                let written = value.to_string_with(&options);
                assert!(options.ensure_ascii.then(|| written.is_ascii()).unwrap_or(true), "{written:?}");
                assert_eq!(Json::parse(&written).unwrap(), value, "{written:?}");
            }
        }
    }

    #[test]
    fn sorted_keys() {
        let value = Json::object().with("b", 1).with("a", Json::object().with("z", 1).with("y", 2)).with("B", 3);
        let sorted = WriteOptions { sort_keys: true, ..WriteOptions::compact() };
        assert_eq!(value.to_string_with(&sorted), r#"{"B":3,"a":{"y":2,"z":1},"b":1}"#);
        // Unsorted order is untouched.
        assert_eq!(value.to_string(), r#"{"b":1,"a":{"z":1,"y":2},"B":3}"#);
    }

    #[test]
    fn builder_and_accessors() {
        let mut value = Json::object();
        value.insert("a", 1).insert("b", "two").insert("a", 3);
        assert_eq!(value.len(), 2);
        assert_eq!(value.get("a").and_then(Json::as_i64), Some(3));
        assert_eq!(value.get("b").and_then(Json::as_str), Some("two"));
        *value.get_mut("a").unwrap() = Json::Bool(true);
        assert_eq!(value.get("a").and_then(Json::as_bool), Some(true));
        assert_eq!(value.remove("a"), Some(Json::Bool(true)));
        assert_eq!(value.remove("a"), None);
        let mut list = Json::array();
        list.push(1).push("x").push(Json::Null);
        assert_eq!(list.len(), 3);
        assert_eq!(list.at(1).and_then(Json::as_str), Some("x"));
        assert!(list.at(2).unwrap().is_null());
        assert_eq!(list.at(3), None);
        assert_eq!(Json::from(Some(4)), Json::Int(4));
        assert_eq!(Json::from(None::<i32>), Json::Null);
        assert_eq!(Json::from(vec!["a", "b"]).to_string(), r#"["a","b"]"#);
        assert_eq!(Json::Float(2.5).as_f64(), Some(2.5));
        assert_eq!(Json::Int(2).as_f64(), Some(2.0));
        assert_eq!(Json::Str("x".into()).type_name(), "string");
        assert_eq!(Json::from_pairs([("k", 1)]).to_string(), r#"{"k":1}"#);
    }

    #[test]
    fn round_trips_nested_document() {
        let text = r#"{"engine":"ngc-wasm/0.1.0","virtualTime":4.5,"ticks":172800000000,"inputs":{"a":[1,2.5,"x",null,true]},"big":18446744073709551615}"#;
        let value = parse(text);
        assert_eq!(value.to_string(), text);
        assert_eq!(parse(&value.to_pretty_string()), value);
    }
}
