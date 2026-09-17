//! The `Decision` union and the `VERB <json>` line decoder (§4).
//!
//! Exactly three outcomes per turn: [`Decision::Call`], [`Decision::Ask`],
//! [`Decision::Finish`]. The decoder is fixed-buffer and allocation-free:
//! it borrows the input line and validates with the strict JSON parser in
//! [`crate::json`]. Argument schemas are validated here, at decode time
//! (§4.5); capability policy and device business rules belong to the
//! runtime's `Authorize` step.
//!
//! JSON string contents (prompt, summary) are borrowed as their raw
//! content with escapes preserved verbatim: the decoded form can only
//! shrink, so bounding the raw content is conservative.

use crate::error::{Error, Field};
use crate::ids::{Digest, Pin, ToolId};
use crate::json::{self, JsonError, JsonValue, ObjectCursor};
use crate::registry;

/// Maximum bytes of one model output line (§4.2: the scripted backend's
/// fixed buffer).
pub const MAX_LINE_BYTES: usize = 512;

/// Maximum bytes of a `CALL` argument JSON block (§4.3: "`<json-args>` ...
/// max 256 bytes").
///
/// This is the SPEC-literal bound for tool arguments. `ASK`/`FINISH`
/// envelopes carry bounded 256-byte *fields* inside a larger JSON
/// object, so they use [`MAX_JSON_BYTES`] instead: the two bounds are
/// deliberately different, and conflating them would silently weaken
/// the argument bound the SPEC states.
pub const MAX_ARGS_BYTES: usize = 256;

/// Maximum bytes of an `ASK`/`FINISH` JSON block.
///
/// Sized to hold the largest valid encoding (a `FINISH` with a 256-byte
/// summary is 294 bytes) with headroom, and below the 512-byte line
/// bound so overlong blocks are still named precisely. §4.3's "max
/// 256 bytes" is satisfiable for `CALL` args but not for an envelope
/// that must contain a 256-byte field, so the envelope gets its own
/// bound rather than an impossible one.
pub const MAX_JSON_BYTES: usize = 384;

/// Maximum bytes of an `ASK` prompt (§4.3).
pub const MAX_PROMPT_BYTES: usize = 256;

/// Maximum bytes of a `FINISH` summary (§4.3).
pub const MAX_SUMMARY_BYTES: usize = 256;

/// The slice's only `ASK` response schema: pin selection.
pub const PIN_SELECT_SCHEMA: u8 = 1;

/// Exactly three outcomes per turn (ADR: one call per turn).
#[derive(Debug, PartialEq, Eq)]
pub enum Decision<'a> {
    /// Call one tool with validated JSON arguments.
    Call(ToolCall<'a>),
    /// Suspend durably for typed human input.
    Ask(InputRequest<'a>),
    /// End the run with a terminal answer.
    Finish(FinalAnswer<'a>),
}

/// A single tool call: stable numeric id, borrowed validated JSON args,
/// and the digest computed at decode time.
#[derive(Debug, PartialEq, Eq)]
pub struct ToolCall<'a> {
    /// The called tool's stable numeric id.
    pub tool: ToolId,
    /// The validated borrowed JSON argument bytes (as emitted, not
    /// canonicalized).
    pub args: &'a [u8],
    /// FNV-1a digest of [`Self::args`], computed at decode time.
    pub args_digest: Digest,
}

impl<'a> ToolCall<'a> {
    /// The called tool's stable numeric id.
    #[must_use]
    pub const fn tool(&self) -> ToolId {
        self.tool
    }

    /// The validated borrowed JSON argument bytes (as emitted, not canonicalized).
    #[must_use]
    pub const fn args(&self) -> &'a [u8] {
        self.args
    }

    /// The digest computed at decode time.
    #[must_use]
    pub const fn args_digest(&self) -> Digest {
        self.args_digest
    }

    /// Extract the `pin` argument.
    ///
    /// The arguments were strictly validated at decode time; this
    /// re-reads them leniently to hand the runtime typed values.
    ///
    /// # Errors
    ///
    /// Returns an [`Error`] when the `pin` field is missing or invalid
    /// (unreachable for decoder-produced calls).
    pub fn pin(&self) -> Result<Pin, Error> {
        let mut parser = json::Parser::new(self.args);
        let JsonValue::Object(cursor) = parser.parse_value().map_err(Error::Json)? else {
            return Err(Error::ArgsNotObject);
        };
        let mut pin: Option<Pin> = None;
        let mut cursor = cursor;
        while let Some((key, value)) = cursor.next_entry().map_err(Error::Json)? {
            if key == b"pin".as_slice() {
                pin = Some(parse_pin_value(value)?);
            }
            // Other keys were validated at decode time; ignore them here.
        }
        pin.ok_or(Error::MissingField(Field::Pin))
    }

    /// Extract the `level` argument of `gpio_pin_write`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::MissingField`] when the `level` field is absent
    /// (always the case for `gpio_pin_read`), or an [`Error`] when it is
    /// invalid (unreachable for decoder-produced calls).
    pub fn level(&self) -> Result<Level, Error> {
        let mut parser = json::Parser::new(self.args);
        let JsonValue::Object(cursor) = parser.parse_value().map_err(Error::Json)? else {
            return Err(Error::ArgsNotObject);
        };
        let mut level: Option<Level> = None;
        let mut cursor = cursor;
        while let Some((key, value)) = cursor.next_entry().map_err(Error::Json)? {
            if key == b"level".as_slice() {
                level = Some(parse_level_value(value)?);
            }
        }
        level.ok_or(Error::MissingField(Field::Level))
    }
}

/// A request for typed human input: a bounded prompt plus the id of the
/// response schema the input must validate against.
#[derive(Debug, PartialEq, Eq)]
pub struct InputRequest<'a> {
    /// The bounded human-input prompt (≤ 256 bytes), borrowed.
    pub prompt: &'a [u8],
    /// The response schema id; the slice defines schema 1 = pin select.
    pub response_schema_id: u8,
}

impl<'a> InputRequest<'a> {
    /// The prompt shown to the human (raw JSON string content, ≤256 B).
    #[must_use]
    pub const fn prompt(&self) -> &'a [u8] {
        self.prompt
    }

    /// The response schema id the input must validate against.
    #[must_use]
    pub const fn response_schema_id(&self) -> u8 {
        self.response_schema_id
    }
}

/// The model's terminal answer. The slice admits only `Completed`.
#[derive(Debug, PartialEq, Eq)]
pub struct FinalAnswer<'a> {
    /// The terminal status; the slice only admits `Completed`.
    pub status: FinishStatus,
    /// The bounded run summary (≤ 256 bytes), borrowed.
    pub summary: &'a [u8],
}

impl<'a> FinalAnswer<'a> {
    /// The finish status (always `Completed` in the slice).
    #[must_use]
    pub const fn status(&self) -> FinishStatus {
        self.status
    }

    /// The bounded human-facing summary (raw JSON string content).
    #[must_use]
    pub const fn summary(&self) -> &'a [u8] {
        self.summary
    }
}

/// Finish statuses. The slice defines only `Completed`; the grammar
/// rejects anything else.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum FinishStatus {
    /// The objective is met.
    Completed,
}

impl FinishStatus {
    /// The grammar spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
        }
    }

    /// Parse the grammar spelling.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BadFieldValue`] for any other value.
    pub const fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        match bytes {
            b"completed" => Ok(Self::Completed),
            _ => Err(Error::BadFieldValue {
                field: Field::Status,
            }),
        }
    }
}

/// A GPIO pin logic level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Level {
    /// Logic low.
    Low,
    /// Logic high.
    High,
}

impl Level {
    /// The grammar spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::High => "high",
        }
    }

    /// Parse the grammar spelling.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BadFieldValue`] for any other value.
    pub const fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        match bytes {
            b"low" => Ok(Self::Low),
            b"high" => Ok(Self::High),
            _ => Err(Error::BadFieldValue {
                field: Field::Level,
            }),
        }
    }
}

/// Decode one model output line into a [`Decision`].
///
/// Grammar (§4.3): `CALL <tool_name> <json-args>`, `ASK <json>`, or
/// `FINISH <json>`, with single-space separators and one optional
/// trailing line terminator. Strict: unknown verbs, unknown tool names,
/// JSON syntax violations, trailing bytes, duplicate keys, depth beyond
/// 3, and any second `CALL` on the line are `Malformed`; well-formed JSON
/// that violates the tool's static schema is `InvalidArgs`. A `CALL`
/// inside JSON string content is opaque data, not a second command.
///
/// # Errors
///
/// Returns an [`Error`] describing the rejection; see
/// [`Error::repair_hint`] for the structured repair error.
pub fn decode_line(line: &[u8]) -> Result<Decision<'_>, Error> {
    if line.len() > MAX_LINE_BYTES {
        return Err(Error::LineTooLong);
    }
    let line = strip_newline(line);
    if line.is_empty() {
        return Err(Error::EmptyLine);
    }
    let (verb, rest) = match line.iter().position(|byte| *byte == b' ') {
        Some(i) => {
            let (verb, rest) = line.split_at(i);
            let rest = rest.get(1..).ok_or(Error::BadSpacing)?;
            (verb, rest)
        }
        None => {
            return match line {
                b"CALL" => Err(Error::MissingToolName),
                b"ASK" | b"FINISH" => Err(Error::MissingArgs),
                _ => Err(Error::UnknownVerb),
            };
        }
    };
    match verb {
        b"CALL" => decode_call(rest),
        b"ASK" => decode_ask(rest),
        b"FINISH" => decode_finish(rest),
        _ => Err(Error::UnknownVerb),
    }
}

/// Strip one trailing `\n` or `\r\n` (host convenience; the grammar is
/// one line per turn).
fn strip_newline(line: &[u8]) -> &[u8] {
    if let Some(stripped) = line.strip_suffix(b"\r\n".as_slice()) {
        return stripped;
    }
    if let Some(stripped) = line.strip_suffix(b"\n".as_slice()) {
        return stripped;
    }
    line
}

/// Require end of input after the top-level JSON value. Trailing
/// bytes get the dedicated [`Error::TrailingBytes`] variant: a second
/// `CALL` on the line is malformed trailing data, not a second command.
const fn finish_args(parser: &json::Parser<'_>) -> Result<(), Error> {
    match parser.finish() {
        Ok(()) => Ok(()),
        Err(JsonError::TrailingBytes) => Err(Error::TrailingBytes),
        Err(other) => Err(Error::Json(other)),
    }
}

/// Drain a composite value, enforcing the depth and duplicate-key
/// rules inside nested structures the schema never descends into.
fn drain_value(value: JsonValue<'_, '_>) -> Result<(), JsonError> {
    match value {
        JsonValue::Object(mut cursor) => {
            while let Some((_, entry)) = cursor.next_entry()? {
                drain_value(entry)?;
            }
            Ok(())
        }
        JsonValue::Array(mut cursor) => {
            while let Some(entry) = cursor.next_value()? {
                drain_value(entry)?;
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

/// A composite where a scalar was expected: drain it first so depth
/// and duplicate-key violations inside are still reported as
/// [`Error::Json`], then report the schema type error.
fn reject_composite(value: JsonValue<'_, '_>, field: Field, expected: &'static str) -> Error {
    match drain_value(value) {
        Ok(()) => Error::BadFieldType { field, expected },
        Err(json_err) => Error::Json(json_err),
    }
}

fn decode_call(rest: &[u8]) -> Result<Decision<'_>, Error> {
    if rest.is_empty() {
        return Err(Error::MissingToolName);
    }
    let (name, args) = match rest.iter().position(|byte| *byte == b' ') {
        Some(i) => {
            let (name, args) = rest.split_at(i);
            let args = args.get(1..).ok_or(Error::BadSpacing)?;
            (name, args)
        }
        None => return Err(Error::MissingArgs),
    };
    if name.is_empty() {
        return Err(Error::MissingToolName);
    }
    if args.is_empty() {
        return Err(Error::MissingArgs);
    }
    let entry = registry::lookup_by_name(name).ok_or(Error::UnknownToolName)?;
    if args.len() > MAX_ARGS_BYTES {
        return Err(Error::ArgsTooLong);
    }
    let mut parser = json::Parser::new(args);
    let JsonValue::Object(cursor) = parser.parse_value().map_err(Error::Json)? else {
        return Err(Error::ArgsNotObject);
    };
    match entry.id.get() {
        registry::GPIO_PIN_READ_ID => {
            read_pin_arg(cursor)?;
        }
        registry::GPIO_PIN_WRITE_ID => {
            read_pin_and_level_arg(cursor)?;
        }
        // Unreachable and fail-closed: `entry` came from the static
        // catalog, whose ids are exactly the two above. A future catalog
        // entry needs a decoder schema before it can be called.
        _ => return Err(Error::UnknownToolName),
    }
    finish_args(&parser)?;
    Ok(Decision::Call(ToolCall {
        tool: entry.id,
        args,
        args_digest: Digest::of_bytes(args),
    }))
}

fn decode_ask(rest: &[u8]) -> Result<Decision<'_>, Error> {
    if rest.is_empty() {
        return Err(Error::MissingArgs);
    }
    if rest.len() > MAX_JSON_BYTES {
        return Err(Error::JsonTooLong);
    }
    let mut parser = json::Parser::new(rest);
    let JsonValue::Object(cursor) = parser.parse_value().map_err(Error::Json)? else {
        return Err(Error::ArgsNotObject);
    };
    let mut prompt: Option<&[u8]> = None;
    let mut schema: Option<u8> = None;
    let mut cursor = cursor;
    while let Some((key, value)) = cursor.next_entry().map_err(Error::Json)? {
        match key {
            b"prompt" => {
                let raw = match value {
                    JsonValue::Str(raw) => raw,
                    other => {
                        return Err(reject_composite(
                            other,
                            Field::Prompt,
                            "string <= 256 bytes",
                        ));
                    }
                };
                if raw.len() > MAX_PROMPT_BYTES {
                    return Err(Error::FieldTooLong {
                        field: Field::Prompt,
                        max: MAX_PROMPT_BYTES,
                    });
                }
                prompt = Some(raw);
            }
            b"schema" => {
                let raw = match value {
                    JsonValue::Number(raw) => raw,
                    other => {
                        return Err(reject_composite(other, Field::Schema, "integer"));
                    }
                };
                schema = Some(json::parse_u8(raw).map_err(Error::Json)?);
            }
            _ => return Err(Error::UnexpectedField),
        }
    }
    let prompt = prompt.ok_or(Error::MissingField(Field::Prompt))?;
    let schema = schema.ok_or(Error::MissingField(Field::Schema))?;
    if schema != PIN_SELECT_SCHEMA {
        return Err(Error::BadFieldValue {
            field: Field::Schema,
        });
    }
    finish_args(&parser)?;
    Ok(Decision::Ask(InputRequest {
        prompt,
        response_schema_id: schema,
    }))
}

fn decode_finish(rest: &[u8]) -> Result<Decision<'_>, Error> {
    if rest.is_empty() {
        return Err(Error::MissingArgs);
    }
    if rest.len() > MAX_JSON_BYTES {
        return Err(Error::JsonTooLong);
    }
    let mut parser = json::Parser::new(rest);
    let JsonValue::Object(cursor) = parser.parse_value().map_err(Error::Json)? else {
        return Err(Error::ArgsNotObject);
    };
    let mut status: Option<FinishStatus> = None;
    let mut summary: Option<&[u8]> = None;
    let mut cursor = cursor;
    while let Some((key, value)) = cursor.next_entry().map_err(Error::Json)? {
        match key {
            b"status" => {
                let raw = match value {
                    JsonValue::Str(raw) => raw,
                    other => {
                        return Err(reject_composite(other, Field::Status, "\"completed\""));
                    }
                };
                status = Some(FinishStatus::from_bytes(raw)?);
            }
            b"summary" => {
                let raw = match value {
                    JsonValue::Str(raw) => raw,
                    other => {
                        return Err(reject_composite(
                            other,
                            Field::Summary,
                            "string <= 256 bytes",
                        ));
                    }
                };
                if raw.len() > MAX_SUMMARY_BYTES {
                    return Err(Error::FieldTooLong {
                        field: Field::Summary,
                        max: MAX_SUMMARY_BYTES,
                    });
                }
                summary = Some(raw);
            }
            _ => return Err(Error::UnexpectedField),
        }
    }
    let status = status.ok_or(Error::MissingField(Field::Status))?;
    let summary = summary.ok_or(Error::MissingField(Field::Summary))?;
    finish_args(&parser)?;
    Ok(Decision::Finish(FinalAnswer { status, summary }))
}

/// Validate `{"pin": u8 0..8}` with no other fields.
fn read_pin_arg(cursor: ObjectCursor<'_, '_>) -> Result<Pin, Error> {
    let mut pin: Option<Pin> = None;
    let mut cursor = cursor;
    while let Some((key, value)) = cursor.next_entry().map_err(Error::Json)? {
        match key {
            b"pin" => {
                pin = Some(parse_pin_value(value)?);
            }
            _ => return Err(Error::UnexpectedField),
        }
    }
    pin.ok_or(Error::MissingField(Field::Pin))
}

/// Validate `{"pin": u8 0..8, "level": "low"|"high"}` with no other fields.
fn read_pin_and_level_arg(cursor: ObjectCursor<'_, '_>) -> Result<(Pin, Level), Error> {
    let mut pin: Option<Pin> = None;
    let mut level: Option<Level> = None;
    let mut cursor = cursor;
    while let Some((key, value)) = cursor.next_entry().map_err(Error::Json)? {
        match key {
            b"pin" => {
                pin = Some(parse_pin_value(value)?);
            }
            b"level" => {
                level = Some(parse_level_value(value)?);
            }
            _ => return Err(Error::UnexpectedField),
        }
    }
    let pin = pin.ok_or(Error::MissingField(Field::Pin))?;
    let level = level.ok_or(Error::MissingField(Field::Level))?;
    Ok((pin, level))
}

fn parse_pin_value(value: JsonValue<'_, '_>) -> Result<Pin, Error> {
    let raw = match value {
        JsonValue::Number(raw) => raw,
        other => return Err(reject_composite(other, Field::Pin, "integer 0..=7")),
    };
    let n = json::parse_u8(raw).map_err(Error::Json)?;
    Pin::new(n)
}

fn parse_level_value(value: JsonValue<'_, '_>) -> Result<Level, Error> {
    let raw = match value {
        JsonValue::Str(raw) => raw,
        other => return Err(reject_composite(other, Field::Level, "\"low\" | \"high\"")),
    };
    Level::from_bytes(raw)
}
