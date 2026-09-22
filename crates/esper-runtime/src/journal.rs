//! The run journal: the only durable state.
//!
//! Every boundary crossing appends one [`Frame`]; everything else —
//! budgets, the monitor, the pending-effect set, the normative state
//! machine position — is derived by replaying the frames in order.
//! Crash recovery is therefore just replay: the committed prefix is
//! intact, and the engine resumes from it.
//!
//! The frame vocabulary mirrors the golden-trace events one to one,
//! except that [`Frame::RunStarted`] carries the seed binding (which
//! the trace does not show) and no frame is emitted for the monitor's
//! `Account` step (accounting is recomputed deterministically from the
//! committed steps during replay; see `engine::Driver::reboot`).
//!
//! // HOST-ONLY (E0/E1): the journal is heap-allocated. The firmware
//! port (E2) replaces this with the flash-backed journal behind the
//! same frame vocabulary.

use esper_core::state::TerminalStatus;

use crate::seed::SeedSnapshot;

/// The class of a committed model line, in the golden-trace vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionClass {
    /// A well-formed tool call.
    Call,
    /// A well-formed human-input request.
    Ask,
    /// A well-formed terminal answer.
    Finish,
    /// The line broke the output grammar.
    Malformed,
    /// The JSON was well-formed but violated the static arg schema.
    InvalidArgs,
}

/// One committed boundary record, in journal order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    /// The run booted: binds the journal to the seed. Always first.
    RunStarted {
        /// The seed snapshot this journal is bound to.
        seed: SeedSnapshot,
    },
    /// One model line committed. Every committed line — valid or
    /// invalid, repair or not — consumes exactly one model turn.
    ModelDecision {
        /// The exact bytes the model emitted.
        // HOST-ONLY (E0/E1)
        output: Vec<u8>,
        /// The decision class, agreed with a re-decode on replay.
        class: DecisionClass,
        /// One-based repair number for invalid lines, else `0`.
        repair_index: u8,
        /// The measured input tokens for this turn (E3). Recorded,
        /// never metered: SPEC §18 defers token budgeting, so these
        /// numbers are evidence only.
        input_tokens: u32,
        /// The measured output tokens for this turn (E3).
        output_tokens: u32,
    },
    /// A tool intent committed: the durable promise to dispatch.
    /// Dispatch may run at least once under this intent's effect id;
    /// the device deduplicates redelivery.
    ToolIntent {
        /// The effect sequence; stable across redelivery.
        seq: u32,
        /// The tool's numeric id.
        tool: u8,
        /// The validated argument bytes.
        // HOST-ONLY (E0/E1)
        args: Vec<u8>,
        /// The decode-time digest of `args`.
        digest: u64,
        /// Whether the tool mutates the world. Committed (not
        /// re-derived from the permission class) because the
        /// `mutations` budget was consumed against this flag at commit
        /// time; replay must reproduce the accounting exactly. The
        /// typed dispatch shape is re-derived on replay from `args`
        /// via `ToolArgs::bind` (deterministic over validated bytes),
        /// so the frame carries no per-tool replay data.
        write: bool,
    },
    /// A tool outcome committed. A transient failure keeps the intent
    /// pending: the engine redispatches under the same effect id.
    ToolObservation {
        /// The effect sequence this outcome belongs to.
        seq: u32,
        /// The one-based attempt number under this effect id.
        attempt: u32,
        /// Whether the dispatch failed transiently.
        transient: bool,
        /// The result bytes (empty for a transient failure).
        // HOST-ONLY (E0/E1)
        outcome: Vec<u8>,
    },
    /// An independent read-back committed (mutating tools only). The
    /// mutation is complete only when `passed` is true.
    Verification {
        /// The effect sequence that was verified.
        seq: u32,
        /// The expected post-effect world state, as JSON
        /// (the requested pin level, or the delay's target clock).
        // HOST-ONLY (E0/E1)
        expected: Vec<u8>,
        /// The independently observed world state, as JSON
        /// (the pin's physical level, or the clock reading).
        // HOST-ONLY (E0/E1)
        observed: Vec<u8>,
        /// Whether read-back matched the requested state.
        passed: bool,
    },
    /// The engine asked for typed human input. The run suspends here
    /// until an [`Frame::ApprovalDecision`] commits.
    ApprovalRequest {
        /// The prompt, as the model emitted it.
        // HOST-ONLY (E0/E1)
        prompt: Vec<u8>,
        /// The response schema id.
        schema: u8,
    },
    /// Typed human input arrived and committed. The run resumes.
    ApprovalDecision {
        /// The input bytes, as delivered.
        // HOST-ONLY (E0/E1)
        input: Vec<u8>,
    },
    /// The run's one logical terminal result. Always last.
    Terminal {
        /// The terminal status.
        status: TerminalStatus,
        /// The machine-readable reason.
        // HOST-ONLY (E0/E1)
        reason: Vec<u8>,
        /// The run summary.
        // HOST-ONLY (E0/E1)
        summary: Vec<u8>,
    },
}

/// The append-only journal: the run's durable state.
///
/// The caller owns the journal, so suspension and crash recovery work
/// across separate `drive_run` calls: the same journal crosses every
/// boot, the way flash would carry it.
#[derive(Debug, Clone, Default)]
pub struct Journal {
    /// The committed frames, in order.
    // HOST-ONLY (E0/E1)
    frames: Vec<Frame>,
}

impl Journal {
    /// An empty journal: the next boot binds it to its seed.
    #[must_use]
    pub const fn new() -> Self {
        // HOST-ONLY (E0/E1)
        Self { frames: Vec::new() }
    }

    /// Whether no frame has committed yet.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// The number of committed frames.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.frames.len()
    }

    /// Append one committed boundary record.
    pub fn push(&mut self, frame: Frame) {
        // HOST-ONLY (E0/E1)
        self.frames.push(frame);
    }

    /// The committed frames, in journal order.
    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }
}
