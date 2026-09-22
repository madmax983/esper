//! Strict, allocation-free JSON validation (§4.3).
//!
//! The decoder needs JSON without an allocator: this module is a small
//! recursive-descent parser over borrowed bytes. It enforces the slice's
//! strictness rules — maximum depth 3, no duplicate keys, no trailing
//! bytes after the top-level value — and exposes values as borrowed
//! slices or streaming cursors, never owned data.
//!
//! E2 move: this module lived in `esper-core` for the E0/E1 slice. It now
//! lives in `esper-protocol` so the contract validator and the decoder
//! share one parser without a dependency cycle; `esper-core` re-exports
//! it (`pub use esper_protocol::json;`, SPEC §17.4), so every existing
//! `esper_core::json::…` path keeps working.
//!
//! String values are returned as their **raw content** (escapes preserved
//! verbatim): the decoded form can only shrink, so enforcing bounds on
//! the raw content is conservative and keeps the decoder borrow-only.
//! Enum matching in the contract validator therefore compares raw
//! content: an escaped spelling of an option (e.g. `"h\u0069gh"`) is
//! rejected, exactly like the E0/E1 decoder's `Level::from_bytes`.

use thiserror::Error;

/// Maximum nesting depth of JSON values (§4.3).
pub const MAX_JSON_DEPTH: u8 = 3;

/// Maximum number of keys in one JSON object.
///
/// The slice's schemas need at most two; eight leaves headroom while
/// keeping duplicate detection in a fixed-size table.
pub const MAX_OBJECT_KEYS: usize = 8;

/// Strict JSON syntax violations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum JsonError {
    /// A byte appeared where no JSON value can start, or inside a value
    /// where it cannot appear (including unescaped control characters).
    #[error("unexpected byte in JSON input")]
    UnexpectedByte,
    /// Input ended mid-value.
    #[error("unexpected end of JSON input")]
    UnexpectedEnd,
    /// A `\` escape is not one of the eight legal escapes, or `\u` is
    /// not followed by four hex digits.
    #[error("bad string escape")]
    BadEscape,
    /// A string was never closed.
    #[error("unterminated string")]
    UnterminatedString,
    /// A number violates the JSON number grammar.
    #[error("bad number")]
    BadNumber,
    /// Bytes follow the top-level value.
    #[error("trailing bytes after the JSON value")]
    TrailingBytes,
    /// Nesting exceeds [`MAX_JSON_DEPTH`].
    #[error("JSON nesting exceeds depth 3")]
    DepthExceeded,
    /// An object repeats a key.
    #[error("duplicate object key")]
    DuplicateKey,
    /// An object holds more than [`MAX_OBJECT_KEYS`] keys.
    #[error("object has more than 8 keys")]
    TooManyKeys,
}

/// A parsed JSON value.
///
/// Composite values are streaming cursors that borrow the parser; scalar
/// values borrow the input buffer directly (`'a`).
#[derive(Debug)]
pub enum JsonValue<'p, 'a> {
    /// `null`.
    Null,
    /// `true` or `false`.
    Bool(bool),
    /// The raw number bytes (strict JSON number grammar).
    Number(&'a [u8]),
    /// The raw string content between the quotes (escapes preserved).
    Str(&'a [u8]),
    /// An array, read entry by entry.
    Array(ArrayCursor<'p, 'a>),
    /// An object, read entry by entry with duplicate-key detection.
    Object(ObjectCursor<'p, 'a>),
}

/// Streaming JSON parser over borrowed bytes. Single-use per value:
/// create a fresh parser for each document.
#[derive(Debug)]
pub struct Parser<'a> {
    buf: &'a [u8],
    pos: usize,
    depth: u8,
}

impl<'a> Parser<'a> {
    /// Create a parser over `buf`.
    #[must_use]
    pub const fn new(buf: &'a [u8]) -> Self {
        Self {
            buf,
            pos: 0,
            depth: 0,
        }
    }

    /// Parse one JSON value at the current position.
    ///
    /// # Errors
    ///
    /// Returns a [`JsonError`] on any strictness violation.
    pub fn parse_value(&mut self) -> Result<JsonValue<'_, 'a>, JsonError> {
        self.skip_ws();
        match self.peek() {
            None => Err(JsonError::UnexpectedEnd),
            Some(b'n') => self.parse_literal(b"null", JsonValue::Null),
            Some(b't') => self.parse_literal(b"true", JsonValue::Bool(true)),
            Some(b'f') => self.parse_literal(b"false", JsonValue::Bool(false)),
            Some(b'"') => {
                let content = self.parse_string()?;
                Ok(JsonValue::Str(content))
            }
            Some(b'[') => {
                self.enter_composite()?;
                self.pos += 1;
                Ok(JsonValue::Array(ArrayCursor::new(self)))
            }
            Some(b'{') => {
                self.enter_composite()?;
                self.pos += 1;
                Ok(JsonValue::Object(ObjectCursor::new(self)))
            }
            Some(c) if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() => {
                // `+` and `.` are not valid JSON number starts; routing
                // them here reports `BadNumber` instead of
                // `UnexpectedByte`.
                let raw = self.parse_number()?;
                Ok(JsonValue::Number(raw))
            }
            Some(_) => Err(JsonError::UnexpectedByte),
        }
    }

    /// Require end of input (after optional insignificant whitespace is
    /// **not** skipped: the slice grammar allows no trailing bytes at
    /// all).
    ///
    /// # Errors
    ///
    /// Returns [`JsonError::TrailingBytes`] when any byte remains.
    pub const fn finish(&self) -> Result<(), JsonError> {
        if self.pos == self.buf.len() {
            Ok(())
        } else {
            Err(JsonError::TrailingBytes)
        }
    }

    /// Bytes consumed so far (for diagnostics and tests).
    #[must_use]
    pub const fn pos(self) -> usize {
        self.pos
    }

    fn peek(&self) -> Option<u8> {
        self.buf.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        Some(byte)
    }

    fn skip_ws(&mut self) {
        while let Some(byte) = self.peek() {
            if byte == b' ' || byte == b'\t' || byte == b'\n' || byte == b'\r' {
                self.pos += 1;
            } else {
                break;
            }
        }
    }

    const fn enter_composite(&mut self) -> Result<(), JsonError> {
        if self.depth >= MAX_JSON_DEPTH {
            return Err(JsonError::DepthExceeded);
        }
        self.depth += 1;
        Ok(())
    }

    fn parse_literal<'p>(
        &mut self,
        literal: &[u8],
        value: JsonValue<'p, 'a>,
    ) -> Result<JsonValue<'p, 'a>, JsonError> {
        for expected in literal {
            match self.bump() {
                Some(byte) if byte == *expected => {}
                _ => return Err(JsonError::UnexpectedByte),
            }
        }
        Ok(value)
    }

    fn parse_string(&mut self) -> Result<&'a [u8], JsonError> {
        // Consume the opening quote; the caller checked it with `peek`.
        self.pos += 1;
        let start = self.pos;
        loop {
            match self.bump() {
                None => return Err(JsonError::UnterminatedString),
                Some(b'"') => {
                    return self
                        .buf
                        .get(start..self.pos - 1)
                        .ok_or(JsonError::UnexpectedEnd);
                }
                Some(b'\\') => self.parse_escape()?,
                Some(c) if c < 0x20 => return Err(JsonError::UnexpectedByte),
                Some(_) => {}
            }
        }
    }

    fn parse_escape(&mut self) -> Result<(), JsonError> {
        match self.bump() {
            Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => Ok(()),
            Some(b'u') => {
                for _ in 0..4 {
                    match self.bump() {
                        Some(c) if c.is_ascii_hexdigit() => {}
                        _ => return Err(JsonError::BadEscape),
                    }
                }
                Ok(())
            }
            _ => Err(JsonError::BadEscape),
        }
    }

    fn parse_number(&mut self) -> Result<&'a [u8], JsonError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        match self.bump() {
            Some(b'0') => {
                // Strict JSON: no leading zeros.
                if matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    return Err(JsonError::BadNumber);
                }
            }
            Some(c) if c.is_ascii_digit() => {
                while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                    self.pos += 1;
                }
            }
            _ => return Err(JsonError::BadNumber),
        }
        if self.peek() == Some(b'.') {
            self.pos += 1;
            match self.bump() {
                Some(c) if c.is_ascii_digit() => {}
                _ => return Err(JsonError::BadNumber),
            }
            while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.pos += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.pos += 1;
            }
            match self.bump() {
                Some(c) if c.is_ascii_digit() => {}
                _ => return Err(JsonError::BadNumber),
            }
            while matches!(self.peek(), Some(d) if d.is_ascii_digit()) {
                self.pos += 1;
            }
        }
        self.buf
            .get(start..self.pos)
            .ok_or(JsonError::UnexpectedEnd)
    }
}

/// Streaming cursor over a JSON object's entries.
///
/// Keys are checked for duplicates against a fixed table of at most
/// [`MAX_OBJECT_KEYS`] borrowed key slices; a repeated key is
/// [`JsonError::DuplicateKey`].
#[derive(Debug)]
pub struct ObjectCursor<'p, 'a> {
    parser: &'p mut Parser<'a>,
    keys: [Option<&'a [u8]>; MAX_OBJECT_KEYS],
    len: usize,
    first: bool,
    done: bool,
}

impl<'p, 'a> ObjectCursor<'p, 'a> {
    const fn new(parser: &'p mut Parser<'a>) -> Self {
        Self {
            parser,
            keys: [None; MAX_OBJECT_KEYS],
            len: 0,
            first: true,
            done: false,
        }
    }

    /// Read the next `(key, value)` entry, or `None` at the closing `}`.
    ///
    /// The returned value may borrow the parser; finish using it before
    /// calling `next_entry` again.
    ///
    /// # Errors
    ///
    /// Returns a [`JsonError`] on any strictness violation, including
    /// duplicate keys.
    pub fn next_entry(&mut self) -> Result<Option<(&'a [u8], JsonValue<'_, 'a>)>, JsonError> {
        if self.done {
            return Ok(None);
        }
        if self.first {
            self.parser.skip_ws();
            if self.parser.peek() == Some(b'}') {
                return Ok(self.close());
            }
        } else {
            self.parser.skip_ws();
            match self.parser.peek() {
                None => return Err(JsonError::UnexpectedEnd),
                Some(b',') => {
                    self.parser.pos += 1;
                    self.parser.skip_ws();
                }
                // `close` consumes the `}` itself.
                Some(b'}') => return Ok(self.close()),
                Some(_) => return Err(JsonError::UnexpectedByte),
            }
        }
        match self.parser.peek() {
            None => return Err(JsonError::UnexpectedEnd),
            Some(b'"') => {}
            Some(_) => return Err(JsonError::UnexpectedByte),
        }
        let key = self.parser.parse_string()?;
        for slot in self.keys.iter().take(self.len) {
            if *slot == Some(key) {
                return Err(JsonError::DuplicateKey);
            }
        }
        let slot = self.keys.get_mut(self.len).ok_or(JsonError::TooManyKeys)?;
        *slot = Some(key);
        self.len += 1;
        self.parser.skip_ws();
        match self.parser.bump() {
            Some(b':') => {}
            _ => return Err(JsonError::UnexpectedByte),
        }
        let value = self.parser.parse_value()?;
        self.first = false;
        Ok(Some((key, value)))
    }

    const fn close(&mut self) -> Option<(&'a [u8], JsonValue<'_, 'a>)> {
        // Consume the `}`; the caller peeked it.
        self.parser.pos += 1;
        self.parser.depth -= 1;
        self.done = true;
        None
    }
}

/// Streaming cursor over a JSON array's values.
#[derive(Debug)]
pub struct ArrayCursor<'p, 'a> {
    parser: &'p mut Parser<'a>,
    first: bool,
    done: bool,
}

impl<'p, 'a> ArrayCursor<'p, 'a> {
    const fn new(parser: &'p mut Parser<'a>) -> Self {
        Self {
            parser,
            first: true,
            done: false,
        }
    }

    /// Read the next value, or `None` at the closing `]`.
    ///
    /// # Errors
    ///
    /// Returns a [`JsonError`] on any strictness violation.
    pub fn next_value(&mut self) -> Result<Option<JsonValue<'_, 'a>>, JsonError> {
        if self.done {
            return Ok(None);
        }
        if self.first {
            self.parser.skip_ws();
            if self.parser.peek() == Some(b']') {
                return Ok(self.close());
            }
        } else {
            self.parser.skip_ws();
            match self.parser.peek() {
                None => return Err(JsonError::UnexpectedEnd),
                Some(b',') => {
                    self.parser.pos += 1;
                    self.parser.skip_ws();
                }
                // `close` consumes the `]` itself.
                Some(b']') => return Ok(self.close()),
                Some(_) => return Err(JsonError::UnexpectedByte),
            }
        }
        let value = self.parser.parse_value()?;
        self.first = false;
        Ok(Some(value))
    }

    const fn close(&mut self) -> Option<JsonValue<'_, 'a>> {
        // Consume the `]`; the caller peeked it.
        self.parser.pos += 1;
        self.parser.depth -= 1;
        self.done = true;
        None
    }
}

/// Parse an ASCII digit string as `u8`.
///
/// The input must already be a strict JSON number body (the parser
/// rejects signs, leading zeros, fractions, and exponents before this
/// runs); this only checks the digit range.
///
/// # Errors
///
/// Returns [`JsonError::BadNumber`] for empty input, non-digit bytes, or
/// values above 255.
pub fn parse_u8(digits: &[u8]) -> Result<u8, JsonError> {
    if digits.is_empty() || digits.len() > 3 {
        return Err(JsonError::BadNumber);
    }
    let mut value: u16 = 0;
    for byte in digits {
        let byte = *byte;
        if !byte.is_ascii_digit() {
            return Err(JsonError::BadNumber);
        }
        value = value * 10 + u16::from(byte - b'0');
        if value > 255 {
            return Err(JsonError::BadNumber);
        }
    }
    u8::try_from(value).map_err(|_| JsonError::BadNumber)
}

/// Parse an ASCII digit string as `u16`.
///
/// The same strictness contract as [`parse_u8`], extended to the full
/// `u16` range. Added in E2 for the `timer_delay_wait` `ms` argument
/// (`1..=5000`, SPEC §17).
///
/// # Errors
///
/// Returns [`JsonError::BadNumber`] for empty input, non-digit bytes, or
/// values above 65535.
pub fn parse_u16(digits: &[u8]) -> Result<u16, JsonError> {
    if digits.is_empty() || digits.len() > 5 {
        return Err(JsonError::BadNumber);
    }
    let mut value: u32 = 0;
    for byte in digits {
        let byte = *byte;
        if !byte.is_ascii_digit() {
            return Err(JsonError::BadNumber);
        }
        value = value * 10 + u32::from(byte - b'0');
        if value > u32::from(u16::MAX) {
            return Err(JsonError::BadNumber);
        }
    }
    u16::try_from(value).map_err(|_| JsonError::BadNumber)
}
