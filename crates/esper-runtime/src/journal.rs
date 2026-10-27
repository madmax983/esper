//! HOST-ONLY: the in-memory run journal and its derived frame view.
//!
//! The durable store of a run is Waymaker's journal: the run's input,
//! one schedule and one outcome record per activity, and the terminal
//! record. On the device that is Waymaker's flash-backed driver. Here
//! [`Journal`] holds the same records in memory so the host harness can
//! drive [`crate::run::run`] across injected resets.
//!
//! [`Frame`] is not a store. It is a **derived view** of those records
//! in the golden-trace vocabulary — one frame per committed boundary,
//! [`Frame::RunStarted`] carrying the seed binding — rebuilt after every
//! boot so fixtures, evaluation and the trace read the same history the
//! workflow replays.

use std::collections::BTreeMap;

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
        /// The built prompt's byte length for this turn (E4, SPEC
        /// §20.4). Summed across the journal, this rebuilds the
        /// segment's prompt meter on boot — the meter is journal
        /// state, so suspend/resume across driver calls keeps the 80%
        /// trigger exact.
        prompt_bytes: u64,
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
        /// The masking digest of the raw outcome (E4, SPEC §20.2):
        /// `0` when the outcome carried no secret, else the FNV-1a
        /// digest of the redacted span. Stored so a reboot rebuilds
        /// the digest stream without the raw secret bytes, which the
        /// masker removed before the journal.
        secret_digest: u64,
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

/// One record of a run's Waymaker journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// An activity intent.
    Scheduled {
        /// The activity kind.
        kind: u16,
        /// The activity input.
        // HOST-ONLY
        input: Vec<u8>,
        /// The Waymaker effect sequence.
        seq: u32,
    },
    /// An activity outcome.
    Resolved {
        /// Whether the effect completed (else failed).
        completed: bool,
        /// The outcome bytes.
        // HOST-ONLY
        bytes: Vec<u8>,
    },
}

/// A run segment's journal: its Waymaker records, the snapshot chunks
/// its dispatcher stored, and the frame view derived from them.
///
/// The caller owns the journal, so suspension and crash recovery work
/// across separate `drive_run` calls: the same journal crosses every
/// boot, the way flash would carry it.
#[derive(Debug, Clone, Default)]
pub struct Journal {
    /// The run's input (an encoded [`crate::run::wire::RunStart`]).
    // HOST-ONLY
    pub(crate) input: Vec<u8>,
    /// Activity records, in commit order.
    // HOST-ONLY
    pub(crate) records: Vec<Record>,
    /// The terminal record: completed (else failed) and its payload.
    // HOST-ONLY
    pub(crate) terminal: Option<(bool, Vec<u8>)>,
    /// The input `continue_as_new` installed, once the segment rolled
    /// over.
    // HOST-ONLY
    pub(crate) continued: Option<Vec<u8>>,
    /// Snapshot chunks, keyed by snapshot digest and chunk index.
    // HOST-ONLY
    pub(crate) store: BTreeMap<(u64, u8), Vec<u8>>,
    /// The derived frame view.
    // HOST-ONLY
    pub(crate) frames: Vec<Frame>,
    /// Boots driven over this journal (an observation, not state).
    pub(crate) boots: u32,
    /// New records committed over this journal's life (an
    /// observation, not state): the crash sweep's counter.
    pub(crate) commits: usize,
}

impl Journal {
    /// An empty journal.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            input: Vec::new(),
            records: Vec::new(),
            terminal: None,
            continued: None,
            store: BTreeMap::new(),
            frames: Vec::new(),
            boots: 0,
            commits: 0,
        }
    }

    /// Boots driven over this journal: one per injected reset, plus the
    /// boot that settled each drive.
    #[must_use]
    pub const fn boots(&self) -> u32 {
        self.boots
    }

    /// New records committed over this journal's life.
    #[must_use]
    pub const fn commits(&self) -> usize {
        self.commits
    }

    /// Whether nothing has been committed.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.input.is_empty()
    }

    /// The number of derived frames.
    #[must_use]
    pub const fn len(&self) -> usize {
        self.frames.len()
    }

    /// The derived frame view, in commit order.
    #[must_use]
    pub fn frames(&self) -> &[Frame] {
        &self.frames
    }

    /// The Waymaker activity records, in commit order.
    #[must_use]
    pub fn records(&self) -> &[Record] {
        &self.records
    }
}
