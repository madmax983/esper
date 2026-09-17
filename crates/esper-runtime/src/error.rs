//! The runtime's error vocabulary.
//!
//! [`RuntimeError`] is the crate's single error type. Engine-internal
//! control flow (injected crashes) travels as [`Halt`], never as an
//! error, so a crash is never confused with a failure.

use thiserror::Error;
use waymaker_core::KernelError;

/// Crash points where the fault injector may reset the run.
///
/// Each variant names a durable boundary: the crash fires after the
/// records named "before" are committed and before anything named
/// "after" happens. Recovery must therefore find a committed prefix
/// and redeliver (or replay) from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum CrashPoint {
    /// After the model schedule commits, before model inference.
    ModelIntent,
    /// After model inference, before the model outcome commits.
    BeforeDecisionCommit,
    /// After the model outcome commits, before authorization.
    BeforeAuthorize,
    /// After the tool schedule commits, before dispatch.
    ToolIntent,
    /// After the physical dispatch, before the tool outcome commits.
    AfterPhysicalBeforeObservation,
    /// After the tool outcome commits, before the verify schedule.
    AfterObservationBeforeVerify,
    /// After the verifier reads, before the verification outcome commits.
    BeforeVerificationCommit,
    /// After the verification outcome commits.
    AfterVerification,
    /// While suspended awaiting typed human input.
    AwaitInput,
    /// After the terminal result is decided, before it commits.
    BeforeTerminalCommit,
    /// After the terminal result commits.
    AfterTerminalCommit,
}

impl CrashPoint {
    /// Every crash point, in boundary order.
    #[must_use]
    pub const fn all() -> [Self; 11] {
        [
            Self::ModelIntent,
            Self::BeforeDecisionCommit,
            Self::BeforeAuthorize,
            Self::ToolIntent,
            Self::AfterPhysicalBeforeObservation,
            Self::AfterObservationBeforeVerify,
            Self::BeforeVerificationCommit,
            Self::AfterVerification,
            Self::AwaitInput,
            Self::BeforeTerminalCommit,
            Self::AfterTerminalCommit,
        ]
    }

    /// The fixture spelling (`cp_...`) of this crash point.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ModelIntent => "cp_model_intent",
            Self::BeforeDecisionCommit => "cp_before_decision_commit",
            Self::BeforeAuthorize => "cp_before_authorize",
            Self::ToolIntent => "cp_tool_intent",
            Self::AfterPhysicalBeforeObservation => "cp_after_physical_before_observation",
            Self::AfterObservationBeforeVerify => "cp_after_observation_before_verify",
            Self::BeforeVerificationCommit => "cp_before_verification_commit",
            Self::AfterVerification => "cp_after_verification",
            Self::AwaitInput => "cp_await_input",
            Self::BeforeTerminalCommit => "cp_before_terminal_commit",
            Self::AfterTerminalCommit => "cp_after_terminal_commit",
        }
    }
}

/// Engine-internal halt: either an injected crash or a real error.
///
/// A crash is not an error: the committed prefix is intact and the
/// driver reboots the engine over it. Only [`Halt::Error`] escapes
/// `drive_run` as a failure.
#[derive(Debug)]
pub enum Halt {
    /// The fault injector fired at this crash point.
    Crash(CrashPoint),
    /// A genuine runtime failure.
    Error(RuntimeError),
}

impl From<RuntimeError> for Halt {
    fn from(error: RuntimeError) -> Self {
        Self::Error(error)
    }
}

/// What can go wrong running a durable `ReAct` workflow.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// The Waymaker kernel refused a record or a boundary call.
    #[error("waymaker kernel refused the journal: {0:?}")]
    Waymaker(#[from] KernelError),
    /// An `esper-core` type rejected a value the engine built.
    #[error("esper-core rejected a runtime value: {0:?}")]
    Core(#[from] esper_core::Error),
    /// A journal frame was longer than the 1 KiB frame cap, or a
    /// payload did not decode.
    #[error("journal frame invalid")]
    BadFrame,
    /// The journal's first record is not a `RunStarted`, or a later
    /// record cannot legally follow its prefix.
    #[error("journal is corrupt or not an Esper run")]
    JournalCorrupt,
    /// The seed failed validation: it names a workflow version this
    /// build does not run, or it grants no model turn and could never
    /// act.
    #[error("run seed is malformed: {0}")]
    SeedInvalid(&'static str),
    /// The journal's `RunStarted` names a different run seed than the
    /// one the driver supplied.
    #[error("journal belongs to a different run seed")]
    SeedMismatch,
    /// The journal's workflow version does not match the seed's.
    #[error("journal workflow version is incompatible with this build")]
    IncompatibleVersion,
    /// The world backend failed (script exhausted, malformed harness
    /// input, device fault the doubles cannot represent).
    #[error("world backend failed: {0}")]
    World(&'static str),
    /// The engine reached a state its own state machine forbids.
    /// This is always an engine bug, never a model or device fault.
    #[error("engine bug: illegal workflow transition")]
    IllegalTransition,
    /// The run suspended awaiting input, and no input arrived after a
    /// full reboot with no journal progress. The input plan is missing
    /// a delivery.
    #[error("run suspended forever: no input arrived")]
    SuspendedForever,
    /// The boot/reboot budget was exceeded; the run is livelocked.
    #[error("too many boots without the run completing")]
    TooManyBoots,
    /// A terminal payload named an unknown status code.
    #[error("terminal record has an unknown status code")]
    BadTerminal,
    /// The async driver task failed to join.
    #[error("async driver task failed")]
    Join,
    /// Recovery's Waymaker replay disagrees with the runtime's own
    /// pending-effect set. The committed history says one thing about
    /// which effects are unresolved and the journal says another; the
    /// run cannot continue without risking a double dispatch.
    #[error("waymaker replay diverged from the journal's pending effects")]
    ReplayDiverged,
}
