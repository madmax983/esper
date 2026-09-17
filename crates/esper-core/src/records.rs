//! Logical journal record kinds (§10.1) and the terminal result type.
//!
//! Esper payloads live inside Waymaker's schedule and outcome records.
//! Each kind has a stable numeric code, an explicit version (carried in
//! the payload by the runtime), a compile-time maximum length, and a
//! canonical byte encoding. Strings are bounded UTF-8; IDs are newtypes;
//! unknown enum values fail closed. Raw provider responses, secrets,
//! stack traces, and chain of thought are never journal payloads.

use crate::decision::Level;
use crate::error::{Error, ErrorCode};
use crate::ids::EffectId;
use crate::state::TerminalStatus;

/// Maximum bytes of a terminal reason (§8.2).
pub const TERMINAL_REASON_MAX: usize = 128;
/// Maximum bytes of a terminal summary (§8.2).
pub const TERMINAL_SUMMARY_MAX: usize = 256;

/// Logical record kinds (§10.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RecordKind {
    /// Run creation: versions, hashes, budget, capabilities.
    RunSeed = 1,
    /// `Gather → Infer`: inference intent (turn index, context digest).
    ModelRequest = 2,
    /// `Infer`: typed decision outcome.
    ModelDecision = 3,
    /// `Authorize → Observe`: tool intent, stable `EffectId`, args digest.
    ToolRequest = 4,
    /// `Observe`: bounded outcome, error class, retry class.
    ToolObservation = 5,
    /// `Observe → Verify`: expected state for read-back.
    VerificationRequest = 6,
    /// `Verify`: pass/fail with expected versus observed.
    VerificationResult = 7,
    /// `Infer → AwaitInput`: the `Ask` prompt plus response schema id.
    ApprovalRequest = 8,
    /// `AwaitInput → Account`: typed human input.
    ApprovalDecision = 9,
    /// `Finalize`/`Degraded`/`SafeStop → End`.
    TerminalResult = 10,
}

impl RecordKind {
    /// The stable numeric code.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Decode a numeric code, failing closed on unknown values.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownRecordKind`] for codes outside `1..=10`.
    pub const fn from_code(code: u8) -> Result<Self, Error> {
        match code {
            1 => Ok(Self::RunSeed),
            2 => Ok(Self::ModelRequest),
            3 => Ok(Self::ModelDecision),
            4 => Ok(Self::ToolRequest),
            5 => Ok(Self::ToolObservation),
            6 => Ok(Self::VerificationRequest),
            7 => Ok(Self::VerificationResult),
            8 => Ok(Self::ApprovalRequest),
            9 => Ok(Self::ApprovalDecision),
            10 => Ok(Self::TerminalResult),
            _ => Err(Error::UnknownRecordKind { code }),
        }
    }

    /// The compile-time maximum payload length in bytes (§10.1).
    #[must_use]
    pub const fn max_len(self) -> u16 {
        match self {
            Self::RunSeed | Self::ModelDecision | Self::ToolObservation | Self::TerminalResult => {
                512
            }
            Self::ModelRequest | Self::ToolRequest => 256,
            Self::VerificationRequest | Self::VerificationResult => 128,
            Self::ApprovalRequest | Self::ApprovalDecision => 320,
        }
    }

    /// The kind's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::RunSeed => "RunSeed",
            Self::ModelRequest => "ModelRequest",
            Self::ModelDecision => "ModelDecision",
            Self::ToolRequest => "ToolRequest",
            Self::ToolObservation => "ToolObservation",
            Self::VerificationRequest => "VerificationRequest",
            Self::VerificationResult => "VerificationResult",
            Self::ApprovalRequest => "ApprovalRequest",
            Self::ApprovalDecision => "ApprovalDecision",
            Self::TerminalResult => "TerminalResult",
        }
    }
}

/// The single logical terminal result (§8.2): a machine-readable status,
/// a bounded machine-readable reason, and a bounded user-facing summary.
/// Only [`TerminalStatus::Completed`] claims the objective was met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TerminalResult {
    status: TerminalStatus,
    reason: [u8; TERMINAL_REASON_MAX],
    reason_len: usize,
    summary: [u8; TERMINAL_SUMMARY_MAX],
    summary_len: usize,
}

impl TerminalResult {
    /// Build a terminal result, enforcing the bounds and UTF-8.
    ///
    /// # Errors
    ///
    /// Returns [`Error::TerminalFieldTooLong`] when either field exceeds
    /// its bound, or [`Error::TerminalFieldNotUtf8`] when either field is
    /// not valid UTF-8.
    pub fn new(status: TerminalStatus, reason: &[u8], summary: &[u8]) -> Result<Self, Error> {
        if reason.len() > TERMINAL_REASON_MAX || summary.len() > TERMINAL_SUMMARY_MAX {
            return Err(Error::TerminalFieldTooLong);
        }
        core::str::from_utf8(reason).map_err(|_| Error::TerminalFieldNotUtf8)?;
        core::str::from_utf8(summary).map_err(|_| Error::TerminalFieldNotUtf8)?;
        let mut out = Self {
            status,
            reason: [0; TERMINAL_REASON_MAX],
            reason_len: reason.len(),
            summary: [0; TERMINAL_SUMMARY_MAX],
            summary_len: summary.len(),
        };
        copy_prefix(&mut out.reason, reason);
        copy_prefix(&mut out.summary, summary);
        Ok(out)
    }

    /// The terminal status.
    #[must_use]
    pub const fn status(&self) -> TerminalStatus {
        self.status
    }

    /// The bounded machine-readable reason.
    #[must_use]
    pub fn reason(&self) -> &[u8] {
        self.reason.get(..self.reason_len).unwrap_or(&[])
    }

    /// The bounded user-facing summary.
    #[must_use]
    pub fn summary(&self) -> &[u8] {
        self.summary.get(..self.summary_len).unwrap_or(&[])
    }
}

/// The committed outcome of the independent read-back verifier (§9.1).
///
/// The verifier compares the expected state (from the committed
/// `ToolRequest`) against the observed state through a read-only device
/// handle and commits exactly one of these per verification activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VerificationResult {
    effect: EffectId,
    passed: bool,
    expected: Level,
    observed: Level,
}

impl VerificationResult {
    /// A passing verification: read-back matched the expected level.
    #[must_use]
    pub const fn pass(effect: EffectId, level: Level) -> Self {
        Self {
            effect,
            passed: true,
            expected: level,
            observed: level,
        }
    }

    /// A failing verification: read-back mismatched.
    #[must_use]
    pub const fn fail(effect: EffectId, expected: Level, observed: Level) -> Self {
        Self {
            effect,
            passed: false,
            expected,
            observed,
        }
    }

    /// The verified effect's identity.
    #[must_use]
    pub const fn effect(&self) -> EffectId {
        self.effect
    }

    /// Whether read-back matched.
    #[must_use]
    pub const fn passed(&self) -> bool {
        self.passed
    }

    /// The expected pin level (from the committed `ToolRequest`).
    #[must_use]
    pub const fn expected(&self) -> Level {
        self.expected
    }

    /// The observed pin level (from the independent read-back).
    #[must_use]
    pub const fn observed(&self) -> Level {
        self.observed
    }

    /// The §6 error class a failure surfaces as.
    ///
    /// A failed verification becomes an observation with error class
    /// [`ErrorCode::VerificationFailed`]; the monitor's
    /// identical-failure rule bounds model retries and ends the run
    /// `Stuck`. There is no auto-compensation (§9.2).
    #[must_use]
    pub const fn error_code(&self) -> Option<ErrorCode> {
        if self.passed {
            None
        } else {
            Some(ErrorCode::VerificationFailed)
        }
    }
}

/// Copy `src` into the prefix of `dst`; both lengths were validated by
/// the caller, so the destination always fits.
fn copy_prefix<const N: usize>(dst: &mut [u8; N], src: &[u8]) {
    if let Some(slot) = dst.get_mut(..src.len()) {
        slot.copy_from_slice(src);
    }
}
