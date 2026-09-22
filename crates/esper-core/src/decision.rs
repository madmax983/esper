//! The `Decision` union and the `VERB <json>` line decoder (§4, §17.5).
//!
//! Exactly three outcomes per turn: [`Decision::Call`], [`Decision::Ask`],
//! [`Decision::Finish`]. The decoder is fixed-buffer and allocation-free:
//! it borrows the input line and validates with the strict JSON parser in
//! [`crate::json`]. `CALL` arguments are validated by
//! `esper_protocol::validate` against the contract table — the single
//! schema source (SPEC §17) — and bound to the typed dispatch shape
//! [`ToolArgs`]; capability policy and device business rules belong to
//! the runtime's `Authorize` step (§4.5).
//!
//! JSON string contents (prompt, summary) are borrowed as their raw
//! content with escapes preserved verbatim: the decoded form can only
//! shrink, so bounding the raw content is conservative.

use esper_protocol::{BoundArgs, Scalar, ValidationError, validate};

use crate::error::{Error, Field};
use crate::ids::{Digest, Pin, SENSOR_COUNT, ToolId};
use crate::json::{self, JsonError, JsonValue};
use crate::registry::{
    self, DEVICE_STATUS_REPORT_ID, GPIO_PIN_READ_ID, GPIO_PIN_WRITE_ID, SENSOR_SAMPLE_READ_ID,
    TIMER_DELAY_WAIT_ID, TIMER_UPTIME_READ_ID,
};

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
    /// Call one tool with validated, bound arguments.
    Call(ToolCall<'a>),
    /// Suspend durably for typed human input.
    Ask(InputRequest<'a>),
    /// End the run with a terminal answer.
    Finish(FinalAnswer<'a>),
}

/// A single tool call: stable numeric id, typed bound arguments, the
/// borrowed raw argument bytes, and the digest computed at decode time.
#[derive(Debug, PartialEq, Eq)]
pub struct ToolCall<'a> {
    /// The called tool's stable numeric id.
    tool: ToolId,
    /// The validated arguments in dispatch shape.
    args: ToolArgs,
    /// The raw validated JSON argument bytes (as emitted, not
    /// canonicalized), borrowed.
    args_bytes: &'a [u8],
    /// FNV-1a digest of [`Self::args_bytes`], computed at decode time.
    args_digest: Digest,
}

impl<'a> ToolCall<'a> {
    /// The called tool's stable numeric id.
    #[must_use]
    pub const fn tool(&self) -> ToolId {
        self.tool
    }

    /// The validated arguments in dispatch shape.
    #[must_use]
    pub const fn args(&self) -> &ToolArgs {
        &self.args
    }

    /// The raw validated JSON argument bytes (as emitted, not canonicalized).
    #[must_use]
    pub const fn args_bytes(&self) -> &'a [u8] {
        self.args_bytes
    }

    /// The digest computed at decode time.
    #[must_use]
    pub const fn args_digest(&self) -> Digest {
        self.args_digest
    }
}

/// The typed dispatch shape of one tool call's arguments (SPEC §17.5).
///
/// Built by [`ToolArgs::bind`] from the [`BoundArgs`] that
/// `esper_protocol::validate` produced, so every range is already
/// proven; this only reorganizes into the shape dispatch matches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ToolArgs {
    /// `gpio_pin_read`.
    GpioPinRead {
        /// The pin to sample.
        pin: Pin,
    },
    /// `gpio_pin_write`.
    GpioPinWrite {
        /// The pin to drive.
        pin: Pin,
        /// The level to set.
        level: Level,
    },
    /// `sensor_sample_read`.
    SensorSampleRead {
        /// The sensor channel, `0..4` (range proven by `validate`).
        sensor: u8,
    },
    /// `timer_uptime_read`.
    TimerUptimeRead,
    /// `timer_delay_wait`.
    TimerDelayWait {
        /// Milliseconds to advance the clock, `1..=5000`.
        ms: u16,
    },
    /// `device_status_report`.
    DeviceStatusReport {
        /// How much detail to report.
        detail: StatusDetail,
    },
}

/// How much detail `device_status_report` returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StatusDetail {
    /// The small summary object.
    Summary,
    /// Summary plus pin and sensor detail.
    Full,
}

impl ToolArgs {
    /// Bind validated arguments to the typed shape. The `BoundArgs`
    /// came from `esper_protocol::validate`, so ranges are already
    /// proven; this only reorganizes into the dispatch shape.
    ///
    /// # Errors
    ///
    /// Returns `Error::InvalidArgs`-family rejections when a field is
    /// absent or has the wrong shape, and [`Error::UnknownToolName`]
    /// when the tool id is unknown — fail closed. Unreachable when the
    /// decoder ran `validate` first.
    pub fn bind(tool_id: ToolId, args: &BoundArgs) -> Result<Self, Error> {
        match tool_id.get() {
            GPIO_PIN_READ_ID => Ok(Self::GpioPinRead {
                pin: pin_arg(args)?,
            }),
            GPIO_PIN_WRITE_ID => Ok(Self::GpioPinWrite {
                pin: pin_arg(args)?,
                level: level_arg(args)?,
            }),
            SENSOR_SAMPLE_READ_ID => Ok(Self::SensorSampleRead {
                sensor: sensor_arg(args)?,
            }),
            TIMER_UPTIME_READ_ID => Ok(Self::TimerUptimeRead),
            TIMER_DELAY_WAIT_ID => Ok(Self::TimerDelayWait { ms: ms_arg(args)? }),
            DEVICE_STATUS_REPORT_ID => Ok(Self::DeviceStatusReport {
                detail: detail_arg(args)?,
            }),
            // Unreachable via the decoder: the tool id came from the
            // static catalog. Fail closed.
            _ => Err(Error::UnknownToolName),
        }
    }
}

/// Fetch one bound scalar by contract field name.
fn scalar(args: &BoundArgs, field: Field, name: &'static str) -> Result<Scalar, Error> {
    args.get(name).ok_or(Error::MissingField(field))
}

/// Bind the `pin` argument. `Pin::new` re-proves the range so direct
/// callers cannot smuggle an out-of-range pin past `bind`.
fn pin_arg(args: &BoundArgs) -> Result<Pin, Error> {
    match scalar(args, Field::Pin, "pin")? {
        Scalar::U8(n) => Pin::new(n),
        _ => Err(Error::BadFieldType {
            field: Field::Pin,
            expected: "integer 0..=7",
        }),
    }
}

/// Bind the `level` argument from its enum index.
fn level_arg(args: &BoundArgs) -> Result<Level, Error> {
    match scalar(args, Field::Level, "level")? {
        Scalar::Enum(0) => Ok(Level::Low),
        Scalar::Enum(1) => Ok(Level::High),
        Scalar::Enum(_) => Err(Error::BadFieldValue {
            field: Field::Level,
        }),
        _ => Err(Error::BadFieldType {
            field: Field::Level,
            expected: "\"low\" | \"high\"",
        }),
    }
}

/// Bind the `sensor` argument. The range is proven by `validate`; the
/// check below only fails closed for direct `bind` callers.
fn sensor_arg(args: &BoundArgs) -> Result<u8, Error> {
    match scalar(args, Field::Sensor, "sensor")? {
        Scalar::U8(n) if n < SENSOR_COUNT => Ok(n),
        Scalar::U8(_) => Err(Error::BadFieldValue {
            field: Field::Sensor,
        }),
        _ => Err(Error::BadFieldType {
            field: Field::Sensor,
            expected: "integer 0..=3",
        }),
    }
}

/// Bind the `ms` argument. The range is proven by `validate`; the check
/// below only fails closed for direct `bind` callers.
fn ms_arg(args: &BoundArgs) -> Result<u16, Error> {
    match scalar(args, Field::Ms, "ms")? {
        Scalar::U16(n) if (1..=5000).contains(&n) => Ok(n),
        Scalar::U16(_) => Err(Error::BadFieldValue { field: Field::Ms }),
        _ => Err(Error::BadFieldType {
            field: Field::Ms,
            expected: "integer 1..=5000",
        }),
    }
}

/// Bind the `detail` argument from its enum index.
fn detail_arg(args: &BoundArgs) -> Result<StatusDetail, Error> {
    match scalar(args, Field::Detail, "detail")? {
        Scalar::Enum(0) => Ok(StatusDetail::Summary),
        Scalar::Enum(1) => Ok(StatusDetail::Full),
        Scalar::Enum(_) => Err(Error::BadFieldValue {
            field: Field::Detail,
        }),
        _ => Err(Error::BadFieldType {
            field: Field::Detail,
            expected: "\"summary\" | \"full\"",
        }),
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
}

/// Decode one model output line into a [`Decision`].
///
/// Grammar (§4.3): `CALL <tool_name> <json-args>`, `ASK <json>`, or
/// `FINISH <json>`, with single-space separators and one optional
/// trailing line terminator. Strict: unknown verbs, unknown tool names,
/// JSON syntax violations, trailing bytes, duplicate keys, and any
/// second `CALL` on the line are `Malformed`; depth faults are
/// `Malformed` wherever the decoder descends (the ASK/FINISH envelope
/// drain); well-formed JSON that violates the tool's static schema is
/// `InvalidArgs` — a composite where a scalar belongs is a schema fault
/// even when it also nests too deep. A `CALL` inside JSON string content
/// is opaque data, not a second command.
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
    // Schema validation is the contract table's job now (SPEC §17.5):
    // one validator serves the decoder and the contract's own fixtures.
    // Violations still consume repair allowance (§4.5).
    let bound = validate(entry, args).map_err(map_validation_error)?;
    let tool_args = ToolArgs::bind(ToolId::new(entry.id), &bound)?;
    Ok(Decision::Call(ToolCall {
        tool: ToolId::new(entry.id),
        args: tool_args,
        args_bytes: args,
        args_digest: Digest::of_bytes(args),
    }))
}

/// Translate a contract validation failure into the decoder's error
/// vocabulary. The repair classes are unchanged: syntax and envelope
/// faults are `Malformed`, schema faults are `InvalidArgs` (§4.5).
fn map_validation_error(err: ValidationError) -> Error {
    match err {
        ValidationError::Json(JsonError::TrailingBytes) => Error::TrailingBytes,
        ValidationError::Json(json_err) => Error::Json(json_err),
        ValidationError::NotObject => Error::ArgsNotObject,
        ValidationError::UnknownArgument | ValidationError::TooManyArguments => {
            Error::UnexpectedField
        }
        ValidationError::MissingArgument(name) => arg_error(name, Error::MissingField),
        ValidationError::WrongType(name) => arg_error(name, |field| Error::BadFieldType {
            field,
            expected: expected_type(name),
        }),
        ValidationError::OutOfRange(name) | ValidationError::BadEnumValue(name) => {
            arg_error(name, |field| Error::BadFieldValue { field })
        }
    }
}

/// Build the `InvalidArgs` rejection for a named contract argument.
///
/// `make` builds the specific rejection from the mapped [`Field`].
/// Names come from the static contract, never from model input; an
/// unknown name is a catalog bug and fails closed as
/// [`Error::UnexpectedField`] rather than blaming the wrong field.
fn arg_error(name: &str, make: impl Fn(Field) -> Error) -> Error {
    field_of(name).map_or(Error::UnexpectedField, make)
}

/// The decoder's [`Field`] for a contract argument name.
fn field_of(name: &str) -> Option<Field> {
    match name {
        "pin" => Some(Field::Pin),
        "level" => Some(Field::Level),
        "sensor" => Some(Field::Sensor),
        "ms" => Some(Field::Ms),
        "detail" => Some(Field::Detail),
        _ => None,
    }
}

/// Plain-words expected type per contract argument, for repair hints.
fn expected_type(name: &str) -> &'static str {
    match name {
        "pin" => "integer 0..=7",
        "level" => "\"low\" | \"high\"",
        "sensor" => "integer 0..=3",
        "ms" => "integer 1..=5000",
        "detail" => "\"summary\" | \"full\"",
        // Unreachable via the decoder (contract names only); a generic
        // expectation keeps the hint bounded either way.
        _ => "a valid argument",
    }
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
