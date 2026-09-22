//! The stable error vocabulary (§6) and decoder rejection types.
//!
//! [`ErrorCode`] is the wire vocabulary: stable numeric codes shared with
//! tool adapters and the monitor. Unknown codes fail closed. [`enum@Error`] is
//! the crate's operational error type (thiserror, `no_std`-compatible);
//! every variant is `Copy` so errors stay cheap on firmware.

use core::fmt::{Display, Formatter, Result as FmtResult};

use thiserror::Error;

use crate::ids::ToolId;
use crate::json::JsonError;
use crate::state::{Event, State};

/// Stable numeric error codes (§6). Unknown codes fail closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum ErrorCode {
    /// Success.
    Ok = 0,
    /// Args violate the static schema; exact violations returned.
    InvalidArgs = 1,
    /// Permission or policy refused; illegal device state.
    Denied = 2,
    /// Reserved; unreachable in the slice.
    ApprovalRequired = 3,
    /// Transport or device hiccup; retryable at most twice.
    Transient = 4,
    /// Deterministic failure; the model may choose another legal action.
    Permanent = 5,
    /// Read-back mismatch; expected versus observed recorded.
    VerificationFailed = 6,
    /// Adapter exceeded its declared result bound.
    OutputExhausted = 7,
    /// Decoder rejection of malformed model output.
    ModelMalformed = 8,
    /// A resource guard fired; the run ends.
    BudgetExceeded = 9,
    /// Durable state is untrustworthy; the run ends.
    StorageFault = 10,
}

impl ErrorCode {
    /// The stable numeric code.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Decode a numeric code, failing closed on unknown values.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Ok),
            1 => Some(Self::InvalidArgs),
            2 => Some(Self::Denied),
            3 => Some(Self::ApprovalRequired),
            4 => Some(Self::Transient),
            5 => Some(Self::Permanent),
            6 => Some(Self::VerificationFailed),
            7 => Some(Self::OutputExhausted),
            8 => Some(Self::ModelMalformed),
            9 => Some(Self::BudgetExceeded),
            10 => Some(Self::StorageFault),
            _ => None,
        }
    }

    /// Whether the workflow may retry the failed activity.
    ///
    /// Only [`ErrorCode::Transient`] is retryable, and the retry budget
    /// (at most two further attempts under the same [`EffectId`](crate::ids::EffectId))
    /// is enforced by the workflow, not by this flag.
    #[must_use]
    pub const fn retryable(self) -> bool {
        matches!(self, Self::Transient)
    }

    /// The stable name of the code.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::InvalidArgs => "invalid_args",
            Self::Denied => "denied",
            Self::ApprovalRequired => "approval_required",
            Self::Transient => "transient",
            Self::Permanent => "permanent",
            Self::VerificationFailed => "verification_failed",
            Self::OutputExhausted => "output_exhausted",
            Self::ModelMalformed => "model_malformed",
            Self::BudgetExceeded => "budget_exceeded",
            Self::StorageFault => "storage_fault",
        }
    }
}

impl Display for ErrorCode {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}

/// A named argument field, used to pinpoint schema violations without
/// borrowing field names from model input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Field {
    /// `ASK` prompt text.
    Prompt,
    /// `ASK` response schema id.
    Schema,
    /// `FINISH` status value.
    Status,
    /// `FINISH` summary text.
    Summary,
    /// Tool `pin` argument.
    Pin,
    /// `gpio_pin_write` `level` argument.
    Level,
    /// `sensor_sample_read` `sensor` argument.
    Sensor,
    /// `timer_delay_wait` `ms` argument.
    Ms,
    /// `device_status_report` `detail` argument.
    Detail,
}

impl Field {
    /// The field name as it appears in the JSON grammar.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Schema => "schema",
            Self::Status => "status",
            Self::Summary => "summary",
            Self::Pin => "pin",
            Self::Level => "level",
            Self::Sensor => "sensor",
            Self::Ms => "ms",
            Self::Detail => "detail",
        }
    }
}

impl Display for Field {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}

/// Which repair class a decoder rejection belongs to (§4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RepairVariant {
    /// The envelope or grammar is broken.
    Malformed,
    /// The JSON is well-formed but violates the static arg schema.
    InvalidArgs,
}

impl RepairVariant {
    /// The name used in the structured repair error.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Malformed => "malformed",
            Self::InvalidArgs => "invalid_args",
        }
    }
}

impl Display for RepairVariant {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}

/// The bounded structured error fed back to the model on a repair turn
/// (§4.4): variant, field, expected constraint, received category. It
/// never contains a parser trace, raw model bytes, or secrets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RepairHint {
    /// Whether the rejection was `malformed` or `invalid_args`.
    pub variant: RepairVariant,
    /// The offending field, or the envelope part (`"verb"`, `"args"`).
    pub field: &'static str,
    /// The constraint that was violated, in plain words.
    pub expected: &'static str,
    /// The category of what was received (never the raw bytes).
    pub received: &'static str,
}

/// Operational error type for `esper-core`. All variants are `Copy`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Error {
    /// The state machine has no such transition (§3.3).
    #[error("illegal transition: {from} cannot take event {event}")]
    IllegalTransition {
        /// The state the workflow was in.
        from: State,
        /// The event it tried to apply.
        event: Event,
    },

    // --- Decoder: malformed envelope (§4.3) ---
    /// The model output line is empty.
    #[error("model output line is empty")]
    EmptyLine,
    /// The verb is not `CALL`, `ASK`, or `FINISH`.
    #[error("unknown decision verb")]
    UnknownVerb,
    /// Token separators are not single spaces.
    #[error("bad spacing in decision line")]
    BadSpacing,
    /// `CALL` carries no tool name.
    #[error("CALL is missing the tool name")]
    MissingToolName,
    /// `CALL` carries no JSON arguments.
    #[error("CALL is missing JSON arguments")]
    MissingArgs,
    /// The tool name is not in the static catalog.
    #[error("unknown tool name")]
    UnknownToolName,
    /// The `CALL` argument JSON exceeds 256 bytes.
    #[error("argument JSON exceeds the 256 byte bound")]
    ArgsTooLong,
    /// An `ASK`/`FINISH` JSON block exceeds the 384 byte envelope bound.
    #[error("JSON block exceeds the 384 byte envelope bound")]
    JsonTooLong,
    /// A whole model line exceeds the 512 byte script buffer bound.
    #[error("model output line exceeds the 512 byte bound")]
    LineTooLong,
    /// Strict JSON syntax violation.
    #[error("JSON syntax error: {0}")]
    Json(JsonError),
    /// Bytes follow the top-level JSON value.
    #[error("trailing bytes after the JSON value")]
    TrailingBytes,
    /// The top-level JSON value is not an object.
    #[error("top-level JSON value is not an object")]
    ArgsNotObject,

    // --- Decoder: invalid arguments (static schema, §4.5) ---
    /// A required field is absent.
    #[error("missing required field: {0}")]
    MissingField(Field),
    /// A field outside the static schema is present.
    #[error("unexpected field in arguments")]
    UnexpectedField,
    /// A field has the wrong JSON type.
    #[error("field {field} has the wrong type: expected {expected}")]
    BadFieldType {
        /// The offending field.
        field: Field,
        /// The expected JSON type, in plain words.
        expected: &'static str,
    },
    /// A field value is outside its allowed set.
    #[error("field {field} has a value outside its allowed set")]
    BadFieldValue {
        /// The offending field.
        field: Field,
    },
    /// A bounded string field exceeds its bound.
    #[error("field {field} exceeds {max} bytes")]
    FieldTooLong {
        /// The offending field.
        field: Field,
        /// The bound in bytes.
        max: usize,
    },

    // --- Budget (§7) ---
    /// A resource guard fired; carries the exhausted unit.
    #[error("budget exhausted: {0}")]
    BudgetExhausted(crate::budget::BudgetUnit),
    /// Restoring state would widen the run's budget identity.
    #[error("budget would widen run identity: {0}")]
    BudgetWidened(crate::budget::BudgetUnit),

    // --- Identifiers ---
    /// A pin number outside `0..=7`.
    #[error("pin {pin} is outside 0..=7")]
    PinOutOfRange {
        /// The rejected pin number.
        pin: u8,
    },

    // --- Policy (§5.2) ---
    /// The capability set refuses this call; terminal `Denied`.
    #[error("capability denied: {tool} on resource {resource}")]
    PermissionDenied {
        /// The refused tool.
        tool: ToolId,
        /// The refused resource (§15.16): the pin for GPIO tools, the
        /// sensor id for `sensor_sample_read`, 0 for timer and status
        /// tools.
        resource: u8,
    },

    // --- Records (§10.1) ---
    /// A terminal-result field exceeds its bound.
    #[error("terminal result field exceeds its bound")]
    TerminalFieldTooLong,
    /// A terminal-result field is not valid UTF-8.
    #[error("terminal result field is not valid UTF-8")]
    TerminalFieldNotUtf8,
    /// Unknown logical record kind code.
    #[error("unknown record kind code: {code}")]
    UnknownRecordKind {
        /// The rejected code.
        code: u8,
    },
    /// Unknown terminal status code.
    #[error("unknown terminal status code: {code}")]
    UnknownStatusCode {
        /// The rejected code.
        code: u8,
    },
}

impl Error {
    /// The repair class of a decoder rejection, or `None` when the error
    /// is not a decoder rejection at all.
    #[must_use]
    pub const fn repair_variant(self) -> Option<RepairVariant> {
        match self {
            Self::EmptyLine
            | Self::UnknownVerb
            | Self::BadSpacing
            | Self::MissingToolName
            | Self::MissingArgs
            | Self::UnknownToolName
            | Self::ArgsTooLong
            | Self::JsonTooLong
            | Self::LineTooLong
            | Self::Json(_)
            | Self::TrailingBytes
            | Self::ArgsNotObject => Some(RepairVariant::Malformed),
            Self::MissingField(_)
            | Self::UnexpectedField
            | Self::BadFieldType { .. }
            | Self::BadFieldValue { .. }
            | Self::FieldTooLong { .. }
            | Self::PinOutOfRange { .. } => Some(RepairVariant::InvalidArgs),
            _ => None,
        }
    }

    /// Build the bounded structured repair hint (§4.4), or `None` when
    /// the error is not a decoder rejection.
    #[must_use]
    pub fn repair_hint(self) -> Option<RepairHint> {
        let variant = self.repair_variant()?;
        let (field, expected, received) = match self {
            Self::EmptyLine => ("line", "one VERB <json> line", "empty"),
            Self::UnknownVerb => ("verb", "CALL | ASK | FINISH", "unknown-verb"),
            Self::BadSpacing => ("line", "single-space separators", "bad-spacing"),
            Self::MissingToolName => ("tool", "catalog tool name", "missing"),
            Self::MissingArgs => ("args", "JSON object", "missing"),
            Self::UnknownToolName => ("tool", "catalog tool name", "unknown-tool"),
            Self::ArgsTooLong => ("args", "<= 256 bytes", "too-long"),
            Self::JsonTooLong => ("json", "<= 384 bytes", "too-long"),
            Self::LineTooLong => ("line", "<= 512 bytes", "too-long"),
            Self::Json(_) => ("args", "strict JSON", "syntax-error"),
            Self::TrailingBytes => ("args", "no bytes after the JSON value", "trailing-bytes"),
            Self::ArgsNotObject => ("args", "top-level JSON object", "wrong-type"),
            Self::MissingField(f) => (f.name(), "required field present", "missing"),
            Self::UnexpectedField => ("args", "schema fields only", "unexpected-field"),
            Self::BadFieldType { field, expected } => (field.name(), expected, "wrong-type"),
            Self::BadFieldValue { field } => {
                (field.name(), Self::allowed_values(field), "bad-value")
            }
            Self::FieldTooLong { field, .. } => (field.name(), "within bound", "too-long"),
            Self::PinOutOfRange { .. } => ("pin", "0..=7", "out-of-range"),
            _ => return None,
        };
        Some(RepairHint {
            variant,
            field,
            expected,
            received,
        })
    }

    /// Plain-words allowed set per field, for repair hints.
    const fn allowed_values(field: Field) -> &'static str {
        match field {
            Field::Prompt | Field::Summary => "string <= 256 bytes",
            Field::Schema => "1",
            Field::Status => "\"completed\"",
            Field::Pin => "0..=7",
            Field::Level => "\"low\" | \"high\"",
            Field::Sensor => "0..=3",
            Field::Ms => "1..=5000",
            Field::Detail => "\"summary\" | \"full\"",
        }
    }
}
