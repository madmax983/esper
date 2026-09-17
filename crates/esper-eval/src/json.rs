//! A minimal JSON parser for golden fixtures.
//!
//! The fixtures are small, trusted, hand-written JSON documents. This
//! parser covers exactly JSON (RFC 8259): objects, arrays, strings with
//! escapes, numbers, and literals. It reports errors with line and
//! column so a malformed fixture fails with a clear message, never a
//! panic.
//!
//! // HOST-ONLY (E0/E1): heap-allocated values, for the host fixture
//! runner. Firmware code never parses JSON.

use std::fmt::Write as _;

/// A parsed JSON value.
///
/// Objects keep insertion order in a vector of pairs; duplicate keys
/// are rejected at parse time so fixture rot is loud, not silent.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// JSON `null`.
    Null,
    /// JSON `true` / `false`.
    Bool(bool),
    /// An integer that fits in `i64`.
    Int(i64),
    /// A non-integer number.
    Float(f64),
    /// A JSON string.
    Str(String),
    /// A JSON array.
    Array(Vec<Self>),
    /// A JSON object: ordered key/value pairs, keys unique.
    Object(Vec<(String, Self)>),
}

/// A JSON syntax error with its source position.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    /// 1-based line number.
    pub line: usize,
    /// 1-based column number.
    pub col: usize,
    /// What went wrong, in plain words.
    pub message: String,
}

impl std::fmt::Display for ParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}: {}", self.line, self.col, self.message)
    }
}

impl std::error::Error for ParseError {}

impl Value {
    /// The JSON type name, for error messages.
    #[must_use]
    pub const fn kind_name(&self) -> &'static str {
        match self {
            Self::Null => "null",
            Self::Bool(_) => "boolean",
            Self::Int(_) | Self::Float(_) => "number",
            Self::Str(_) => "string",
            Self::Array(_) => "array",
            Self::Object(_) => "object",
        }
    }

    /// Borrow as an object, if it is one.
    #[must_use]
    pub fn as_object(&self) -> Option<&[(String, Self)]> {
        match self {
            Self::Object(pairs) => Some(pairs),
            _ => None,
        }
    }

    /// Borrow as an array, if it is one.
    #[must_use]
    pub fn as_array(&self) -> Option<&[Self]> {
        match self {
            Self::Array(items) => Some(items),
            _ => None,
        }
    }

    /// Borrow as a string, if it is one.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::Str(text) => Some(text),
            _ => None,
        }
    }

    /// Look up a key in an object, if this is an object with that key.
    #[must_use]
    pub fn get(&self, key: &str) -> Option<&Self> {
        // HOST-ONLY (E0/E1)
        self.as_object()?
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    }

    /// Structural equality with objects compared order-independently.
    ///
    /// Key order in JSON objects is insignificant, so `{"a":1,"b":2}`
    /// equals `{"b":2,"a":1}`. Arrays stay order-sensitive.
    #[must_use]
    pub fn equals_canonical(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Int(a), Self::Int(b)) => a == b,
            (Self::Float(a), Self::Float(b)) => a.to_bits() == b.to_bits(),
            (Self::Str(a), Self::Str(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => {
                a.len() == b.len() && a.iter().zip(b.iter()).all(|(x, y)| x.equals_canonical(y))
            }
            (Self::Object(a), Self::Object(b)) => {
                a.len() == b.len()
                    && a.iter().all(|(key, value)| {
                        b.iter()
                            .find(|(other_key, _)| other_key == key)
                            .is_some_and(|(_, other_value)| value.equals_canonical(other_value))
                    })
            }
            _ => false,
        }
    }

    /// Render canonical compact JSON: object keys sorted, no whitespace.
    ///
    /// Used for readable trace diffs and for re-serializing fixture
    /// inputs (typed human input) into bytes for the input plan.
    #[must_use]
    pub fn render(&self) -> String {
        // HOST-ONLY (E0/E1)
        let mut out = String::new();
        render_into(&mut out, self);
        out
    }
}

/// Append the canonical rendering of `value` to `out`.
fn render_into(out: &mut String, value: &Value) {
    // HOST-ONLY (E0/E1)
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Int(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Float(n) => {
            let _ = write!(out, "{n}");
        }
        Value::Str(text) => render_string(out, text),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render_into(out, item);
            }
            out.push(']');
        }
        Value::Object(pairs) => {
            // HOST-ONLY (E0/E1)
            let mut sorted: Vec<&(String, Value)> = pairs.iter().collect();
            sorted.sort_by(|a, b| a.0.cmp(&b.0));
            out.push('{');
            for (i, (key, val)) in sorted.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                render_string(out, key);
                out.push(':');
                render_into(out, val);
            }
            out.push('}');
        }
    }
}

/// Append a JSON-quoted string with minimal escaping.
fn render_string(out: &mut String, text: &str) {
    // HOST-ONLY (E0/E1)
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Maximum nesting depth: fixtures are shallow, and the cap keeps a
/// hostile input from overflowing the call stack.
const MAX_DEPTH: usize = 64;

/// Parse one JSON document.
///
/// # Errors
///
/// Returns [`ParseError`] with a line and column when the input is not
/// valid JSON, has trailing bytes, or nests deeper than `MAX_DEPTH`.
pub fn parse(input: &str) -> Result<Value, ParseError> {
    let mut parser = Parser {
        // HOST-ONLY (E0/E1)
        bytes: input.as_bytes(),
        pos: 0,
        line: 1,
        col: 1,
    };
    let value = parser.parse_value(0)?;
    parser.skip_ws();
    if parser.pos < parser.bytes.len() {
        return Err(parser.error("trailing bytes after the JSON document"));
    }
    Ok(value)
}

/// The recursive-descent parser state.
struct Parser<'a> {
    /// The input bytes.
    bytes: &'a [u8],
    /// The current byte offset.
    pos: usize,
    /// 1-based line number of `pos`.
    line: usize,
    /// 1-based column number of `pos`.
    col: usize,
}

impl Parser<'_> {
    /// Build an error at the current position.
    fn error(&self, message: &str) -> ParseError {
        ParseError {
            line: self.line,
            col: self.col,
            // HOST-ONLY (E0/E1)
            message: message.to_owned(),
        }
    }

    /// The current byte, or `None` at end of input.
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    /// Consume the current byte and advance the position.
    fn bump(&mut self) -> Option<u8> {
        let byte = self.peek()?;
        self.pos += 1;
        if byte == b'\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(byte)
    }

    /// Expect this exact byte next.
    fn expect_byte(&mut self, want: u8, what: &str) -> Result<(), ParseError> {
        match self.bump() {
            Some(got) if got == want => Ok(()),
            _ => Err(self.error(&format!("expected {what}"))),
        }
    }

    /// Skip JSON whitespace.
    fn skip_ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.bump();
        }
    }

    /// Parse any JSON value.
    fn parse_value(&mut self, depth: usize) -> Result<Value, ParseError> {
        if depth > MAX_DEPTH {
            return Err(self.error("JSON nests too deeply"));
        }
        self.skip_ws();
        match self.peek() {
            Some(b'{') => self.parse_object(depth),
            Some(b'[') => self.parse_array(depth),
            Some(b'"') => Ok(Value::Str(self.parse_string()?)),
            Some(b't') => self.parse_literal("true", Value::Bool(true)),
            Some(b'f') => self.parse_literal("false", Value::Bool(false)),
            Some(b'n') => self.parse_literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            Some(_) => Err(self.error("unexpected character")),
            None => Err(self.error("unexpected end of input")),
        }
    }

    /// Parse `literal` and return `value` on success.
    fn parse_literal(&mut self, literal: &str, value: Value) -> Result<Value, ParseError> {
        for want in literal.bytes() {
            match self.bump() {
                Some(got) if got == want => {}
                _ => return Err(self.error("invalid literal")),
            }
        }
        // A literal must end at a value boundary (RFC 8259 §3): without
        // this, `truely` would lex as `true` plus trailing garbage.
        match self.peek() {
            Some(b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}') | None => {}
            _ => return Err(self.error("invalid literal")),
        }
        Ok(value)
    }

    /// Parse an object; the opening brace is current.
    fn parse_object(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.expect_byte(b'{', "'{'")?;
        // HOST-ONLY (E0/E1)
        let mut pairs: Vec<(String, Value)> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.bump();
            return Ok(Value::Object(pairs));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.error("expected a string key"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            self.expect_byte(b':', "':'")?;
            let value = self.parse_value(depth + 1)?;
            if pairs.iter().any(|(name, _)| name == &key) {
                return Err(self.error(&format!("duplicate key {key:?}")));
            }
            pairs.push((key, value));
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b'}') => break,
                _ => return Err(self.error("expected ',' or '}'")),
            }
        }
        Ok(Value::Object(pairs))
    }

    /// Parse an array; the opening bracket is current.
    fn parse_array(&mut self, depth: usize) -> Result<Value, ParseError> {
        self.expect_byte(b'[', "'['")?;
        // HOST-ONLY (E0/E1)
        let mut items: Vec<Value> = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.bump();
            return Ok(Value::Array(items));
        }
        loop {
            items.push(self.parse_value(depth + 1)?);
            self.skip_ws();
            match self.bump() {
                Some(b',') => {}
                Some(b']') => break,
                _ => return Err(self.error("expected ',' or ']'")),
            }
        }
        Ok(Value::Array(items))
    }

    /// Parse a JSON number into [`Value::Int`] or [`Value::Float`].
    fn parse_number(&mut self) -> Result<Value, ParseError> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.bump();
        }
        let mut is_float = false;
        self.parse_digits("expected digits")?;
        if self.peek() == Some(b'.') {
            is_float = true;
            self.bump();
            self.parse_digits("expected digits after '.'")?;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            is_float = true;
            self.bump();
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.bump();
            }
            self.parse_digits("expected digits in exponent")?;
        }
        // HOST-ONLY (E0/E1)
        let text = String::from_utf8_lossy(&self.bytes[start..self.pos]);
        if is_float {
            text.parse::<f64>()
                .map(Value::Float)
                .map_err(|_| self.error("invalid number"))
        } else {
            text.parse::<i64>()
                .map(Value::Int)
                .map_err(|_| self.error("number is out of range"))
        }
    }

    /// Parse one or more ASCII digits.
    fn parse_digits(&mut self, what: &str) -> Result<(), ParseError> {
        let mut count = 0usize;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.bump();
            count += 1;
        }
        if count == 0 {
            return Err(self.error(what));
        }
        Ok(())
    }

    /// Parse a string; the opening quote is current.
    ///
    /// Raw bytes accumulate so multi-byte UTF-8 passes through
    /// untouched; escape sequences contribute their decoded scalar.
    fn parse_string(&mut self) -> Result<String, ParseError> {
        self.expect_byte(b'"', "'\"'")?;
        // HOST-ONLY (E0/E1)
        let mut out: Vec<u8> = Vec::new();
        loop {
            match self.bump() {
                None => return Err(self.error("unterminated string")),
                Some(b'"') => break,
                Some(b'\\') => {
                    let ch = self.parse_escape_char()?;
                    // HOST-ONLY (E0/E1)
                    let mut encoded = [0u8; 4];
                    out.extend_from_slice(ch.encode_utf8(&mut encoded).as_bytes());
                }
                Some(byte) if byte < 0x20 => {
                    return Err(self.error("unescaped control character in string"));
                }
                Some(byte) => out.push(byte),
            }
        }
        String::from_utf8(out).map_err(|_| self.error("invalid UTF-8 in string"))
    }

    /// Parse one escape sequence (after the backslash) into its scalar.
    fn parse_escape_char(&mut self) -> Result<char, ParseError> {
        match self.bump() {
            Some(b'"') => Ok('"'),
            Some(b'\\') => Ok('\\'),
            Some(b'/') => Ok('/'),
            Some(b'b') => Ok('\u{0008}'),
            Some(b'f') => Ok('\u{000C}'),
            Some(b'n') => Ok('\n'),
            Some(b'r') => Ok('\r'),
            Some(b't') => Ok('\t'),
            Some(b'u') => self.parse_unicode_escape(),
            _ => Err(self.error("invalid escape sequence")),
        }
    }

    /// Parse a `\uXXXX` escape, including surrogate pairs.
    fn parse_unicode_escape(&mut self) -> Result<char, ParseError> {
        let high = self.parse_hex4()?;
        if (0xD800..0xDC00).contains(&high) {
            if self.bump() == Some(b'\\') && self.bump() == Some(b'u') {
                let low = self.parse_hex4()?;
                if (0xDC00..0xE000).contains(&low) {
                    let scalar = 0x1_0000 + ((high - 0xD800) << 10) + (low - 0xDC00);
                    return char::from_u32(scalar)
                        .ok_or_else(|| self.error("invalid Unicode scalar"));
                }
            }
            return Err(self.error("lone surrogate in string escape"));
        }
        if (0xDC00..0xE000).contains(&high) {
            return Err(self.error("lone surrogate in string escape"));
        }
        char::from_u32(high).ok_or_else(|| self.error("invalid Unicode escape"))
    }

    /// Parse exactly four hex digits into a `u32`.
    fn parse_hex4(&mut self) -> Result<u32, ParseError> {
        let mut value: u32 = 0;
        for _ in 0..4 {
            let byte = self
                .bump()
                .ok_or_else(|| self.error("expected a hex digit"))?;
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err(self.error("expected a hex digit")),
            };
            value = value * 16 + u32::from(digit);
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::parse;

    #[test]
    fn parses_the_fixture_shapes() {
        let value = parse(r#"{"a": [1, -2, 3.5, true, null, "x\ny"], "b": {}}"#).expect("valid");
        assert!(value.get("a").is_some_and(|v| v.as_array().is_some()));
        assert!(value.get("b").is_some_and(|v| v.as_object().is_some()));
    }

    #[test]
    fn rejects_trailing_bytes_and_duplicates() {
        assert!(parse("{} x").is_err());
        assert!(parse(r#"{"a": 1, "a": 2}"#).is_err());
    }

    #[test]
    fn reports_line_and_column() {
        let err = parse("{\n  \"a\": truely\n}").expect_err("bad literal");
        assert_eq!((err.line, err.col), (2, 12));
    }

    #[test]
    fn canonical_equality_ignores_key_order() {
        let a = parse(r#"{"x": 1, "y": [1, 2]}"#).expect("valid");
        let b = parse(r#"{"y": [1, 2], "x": 1}"#).expect("valid");
        assert!(a.equals_canonical(&b));
        let c = parse(r#"{"y": [2, 1], "x": 1}"#).expect("valid");
        assert!(!a.equals_canonical(&c));
    }

    #[test]
    fn renders_sorted_compact_json() {
        let value = parse(r#"{"b": 2, "a": "q\"q"}"#).expect("valid");
        assert_eq!(value.render(), r#"{"a":"q\"q","b":2}"#);
    }

    #[test]
    fn parses_unicode_escapes() {
        let value = parse(r#""Aé𝄞""#).expect("valid");
        assert_eq!(value.as_str(), Some("Aé𝄞"));
        assert!(parse(r#""\ud800""#).is_err());
        assert!(parse(r#""\udc00""#).is_err());
    }
}
