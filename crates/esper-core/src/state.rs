//! The workflow state machine (§3).
//!
//! The transition table is **data**: [`TRANSITIONS`] is the normative
//! §3.2 table, [`transition`] is total over `(State, Event)` and returns
//! [`Error::IllegalTransition`] for unlisted pairs, and [`is_legal`]
//! answers the reachability question the runtime asks before dispatching.
//! [`committed_records`] names the Waymaker record committed on each edge.
//!
//! Conditions from §3.2 (budget checks, repair counts, transient attempt
//! counts) are the caller's job: the caller derives them from committed
//! outcomes and picks the matching [`Event`]. The table itself stays pure.

use core::fmt::{Display, Formatter, Result as FmtResult};

use crate::error::Error;
use crate::records::RecordKind;

/// Workflow states (§3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    /// Validate the run seed; replay the committed prefix. Entry state.
    Recover,
    /// Rebuild budgets and working state from committed outcomes only.
    Gather,
    /// Run the model activity; decode bytes to a `Decision`.
    Infer,
    /// Bounded repair of malformed output or invalid arguments.
    Repair,
    /// Permission policy and device business rules for a `Call`.
    Authorize,
    /// Durably suspended on `Ask`; waits for typed human input.
    AwaitInput,
    /// Dispatch the tool; normalize and bound the observation.
    Observe,
    /// Independent read-back of a mutating tool's target state.
    Verify,
    /// Budget accounting, loop detection, progress delta.
    Account,
    /// The model chose `Finish`; commit the terminal result.
    Finalize,
    /// A guard fired; commit the terminal result with the failing status.
    Degraded,
    /// Journal corrupt or incompatible; best-effort terminal.
    SafeStop,
    /// Workflow-final state. Exactly one logical terminal precedes it.
    End,
}

impl State {
    /// The state's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Recover => "Recover",
            Self::Gather => "Gather",
            Self::Infer => "Infer",
            Self::Repair => "Repair",
            Self::Authorize => "Authorize",
            Self::AwaitInput => "AwaitInput",
            Self::Observe => "Observe",
            Self::Verify => "Verify",
            Self::Account => "Account",
            Self::Finalize => "Finalize",
            Self::Degraded => "Degraded",
            Self::SafeStop => "SafeStop",
            Self::End => "End",
        }
    }

    /// Whether this is the workflow-final state (`End` only, §3.4).
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::End)
    }

    /// Whether this state commits a terminal result and then moves only
    /// to `End` (`Finalize`, `Degraded`, `SafeStop`).
    #[must_use]
    pub const fn is_pre_terminal(self) -> bool {
        matches!(self, Self::Finalize | Self::Degraded | Self::SafeStop)
    }
}

impl Display for State {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}

/// Events that drive the state machine.
///
/// Each §3.2 table row whose condition is a disjunction becomes one
/// event per disjunct (for example `Gather`'s "budget exhausted or
/// version binding invalid" becomes [`Event::BudgetExhausted`] and
/// [`Event::IncompatibleVersion`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Event {
    /// Journal valid; versions and hashes validate (`Recover`).
    JournalValid,
    /// Journal corrupt or version-incompatible (`Recover`).
    JournalCorrupt,
    /// Budgets and guards clear (`Gather`).
    BudgetsClear,
    /// A budget is already exhausted (`Gather`).
    BudgetExhausted,
    /// Version binding invalid at resume (`Gather`).
    IncompatibleVersion,
    /// Bytes decode to a `Call` (`Infer`).
    DecodeCall,
    /// Bytes decode to an `Ask` (`Infer`).
    DecodeAsk,
    /// Bytes decode to a `Finish` (`Infer`).
    DecodeFinish,
    /// Bytes malformed or args violate the static schema (`Infer`).
    DecodeMalformed,
    /// Repair allowance remains (`Repair`).
    RepairAvailable,
    /// Repair allowance exhausted (`Repair`).
    RepairExhausted,
    /// Permission granted and intent committed (`Authorize`).
    Authorized,
    /// Permission or policy denied (`Authorize`).
    Denied,
    /// Typed human input arrived and validated (`AwaitInput`).
    InputArrived,
    /// Mutating tool returned `ok`; verification is mandatory (`Observe`).
    ObserveOkMutating,
    /// Observation needs no verification (`Observe`).
    ObserveDone,
    /// Transient failure, attempts remain; same `EffectId` (`Observe`).
    TransientRetry,
    /// Transient attempts exhausted (`Observe`).
    TransientExhausted,
    /// Read-back matches expected state (`Verify`).
    VerifyPass,
    /// Read-back mismatches (`Verify`).
    VerifyFail,
    /// All guards clear (`Account`).
    GuardsClear,
    /// A monitor or budget guard fired (`Account`).
    GuardFired,
    /// Terminal result committed (`Finalize`).
    FinalizeCommitted,
    /// Terminal result committed (`Degraded`).
    DegradedCommitted,
    /// Best-effort terminal result committed (`SafeStop`).
    SafeStopCommitted,
}

impl Event {
    /// The event's name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::JournalValid => "JournalValid",
            Self::JournalCorrupt => "JournalCorrupt",
            Self::BudgetsClear => "BudgetsClear",
            Self::BudgetExhausted => "BudgetExhausted",
            Self::IncompatibleVersion => "IncompatibleVersion",
            Self::DecodeCall => "DecodeCall",
            Self::DecodeAsk => "DecodeAsk",
            Self::DecodeFinish => "DecodeFinish",
            Self::DecodeMalformed => "DecodeMalformed",
            Self::RepairAvailable => "RepairAvailable",
            Self::RepairExhausted => "RepairExhausted",
            Self::Authorized => "Authorized",
            Self::Denied => "Denied",
            Self::InputArrived => "InputArrived",
            Self::ObserveOkMutating => "ObserveOkMutating",
            Self::ObserveDone => "ObserveDone",
            Self::TransientRetry => "TransientRetry",
            Self::TransientExhausted => "TransientExhausted",
            Self::VerifyPass => "VerifyPass",
            Self::VerifyFail => "VerifyFail",
            Self::GuardsClear => "GuardsClear",
            Self::GuardFired => "GuardFired",
            Self::FinalizeCommitted => "FinalizeCommitted",
            Self::DegradedCommitted => "DegradedCommitted",
            Self::SafeStopCommitted => "SafeStopCommitted",
        }
    }
}

impl Display for Event {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}

/// Every workflow state, for exhaustive testing.
pub const ALL_STATES: [State; 13] = [
    State::Recover,
    State::Gather,
    State::Infer,
    State::Repair,
    State::Authorize,
    State::AwaitInput,
    State::Observe,
    State::Verify,
    State::Account,
    State::Finalize,
    State::Degraded,
    State::SafeStop,
    State::End,
];

/// Every machine event, for exhaustive testing.
pub const ALL_EVENTS: [Event; 25] = [
    Event::JournalValid,
    Event::JournalCorrupt,
    Event::BudgetsClear,
    Event::BudgetExhausted,
    Event::IncompatibleVersion,
    Event::DecodeCall,
    Event::DecodeAsk,
    Event::DecodeFinish,
    Event::DecodeMalformed,
    Event::RepairAvailable,
    Event::RepairExhausted,
    Event::Authorized,
    Event::Denied,
    Event::InputArrived,
    Event::ObserveOkMutating,
    Event::ObserveDone,
    Event::TransientRetry,
    Event::TransientExhausted,
    Event::VerifyPass,
    Event::VerifyFail,
    Event::GuardsClear,
    Event::GuardFired,
    Event::FinalizeCommitted,
    Event::DegradedCommitted,
    Event::SafeStopCommitted,
];

/// The normative §3.2 transition table, as data: `(from, event, to)`.
pub const TRANSITIONS: [(State, Event, State); 25] = [
    (State::Recover, Event::JournalValid, State::Gather),
    (State::Recover, Event::JournalCorrupt, State::SafeStop),
    (State::Gather, Event::BudgetsClear, State::Infer),
    (State::Gather, Event::BudgetExhausted, State::Degraded),
    (State::Gather, Event::IncompatibleVersion, State::Degraded),
    (State::Infer, Event::DecodeCall, State::Authorize),
    (State::Infer, Event::DecodeAsk, State::AwaitInput),
    (State::Infer, Event::DecodeFinish, State::Finalize),
    (State::Infer, Event::DecodeMalformed, State::Repair),
    (State::Repair, Event::RepairAvailable, State::Infer),
    (State::Repair, Event::RepairExhausted, State::Degraded),
    (State::Authorize, Event::Authorized, State::Observe),
    (State::Authorize, Event::Denied, State::Degraded),
    (State::AwaitInput, Event::InputArrived, State::Account),
    (State::Observe, Event::ObserveOkMutating, State::Verify),
    (State::Observe, Event::ObserveDone, State::Account),
    (State::Observe, Event::TransientRetry, State::Observe),
    (State::Observe, Event::TransientExhausted, State::Degraded),
    (State::Verify, Event::VerifyPass, State::Account),
    (State::Verify, Event::VerifyFail, State::Account),
    (State::Account, Event::GuardsClear, State::Gather),
    (State::Account, Event::GuardFired, State::Degraded),
    (State::Finalize, Event::FinalizeCommitted, State::End),
    (State::Degraded, Event::DegradedCommitted, State::End),
    (State::SafeStop, Event::SafeStopCommitted, State::End),
];

/// Apply one event to a state: the total transition function.
///
/// # Errors
///
/// Returns [`Error::IllegalTransition`] for any `(from, event)` pair
/// absent from [`TRANSITIONS`] — including every §3.3 hazard such as
/// `Infer → Observe` (the authorization bypass).
pub fn transition(from: State, event: Event) -> Result<State, Error> {
    for (f, e, t) in TRANSITIONS {
        if f == from && e == event {
            return Ok(t);
        }
    }
    Err(Error::IllegalTransition { from, event })
}

/// Whether any event moves `from` directly to `to`.
#[must_use]
pub fn is_legal(from: State, to: State) -> bool {
    TRANSITIONS.iter().any(|(f, _, t)| *f == from && *t == to)
}

/// The Waymaker records committed when `event` fires in `from` (§3.2).
///
/// Most edges commit exactly one record; `Infer → AwaitInput` commits
/// the decision and the approval request, and the exhausted-transient
/// edge commits the final observation before the terminal result.
// The two `&[]` arms are intentional: this table must enumerate every
// legal edge explicitly so it can be audited against §3.2, which means
// the no-commit edges share a body with the illegal-pair fallback by
// design. Do not "fix" this by folding the legal edges into `_`.
#[allow(clippy::match_same_arms)]
#[must_use]
pub const fn committed_records(from: State, event: Event) -> &'static [RecordKind] {
    use Event as E;
    use RecordKind as K;
    use State as S;
    match (from, event) {
        // Edges that commit nothing.
        (S::Recover, E::JournalValid)
        | (S::Repair, E::RepairAvailable)
        | (S::Account, E::GuardsClear) => &[],
        // The exhausted-transient edge commits the final observation
        // before the terminal result.
        (S::Observe, E::TransientExhausted) => &[K::ToolObservation, K::TerminalResult],
        // Ask commits the decision and the approval request.
        (S::Infer, E::DecodeAsk) => &[K::ModelDecision, K::ApprovalRequest],
        (S::Gather, E::BudgetsClear) => &[K::ModelRequest],
        (S::Infer, E::DecodeCall | E::DecodeFinish | E::DecodeMalformed) => &[K::ModelDecision],
        (S::Authorize, E::Authorized) => &[K::ToolRequest],
        (S::AwaitInput, E::InputArrived) => &[K::ApprovalDecision],
        (S::Verify, E::VerifyPass | E::VerifyFail) => &[K::VerificationResult],
        (S::Observe, E::ObserveOkMutating | E::ObserveDone | E::TransientRetry) => {
            &[K::ToolObservation]
        }
        // Every other legal edge ends the run with a terminal result.
        (S::Recover, E::JournalCorrupt)
        | (S::Gather, E::BudgetExhausted | E::IncompatibleVersion)
        | (S::Repair, E::RepairExhausted)
        | (S::Authorize, E::Denied)
        | (S::Account, E::GuardFired)
        | (S::Finalize, E::FinalizeCommitted)
        | (S::Degraded, E::DegradedCommitted)
        | (S::SafeStop, E::SafeStopCommitted) => &[K::TerminalResult],
        // Illegal pairs commit nothing; the runtime must not be here.
        _ => &[],
    }
}

/// Terminal statuses (§8.2). Numeric codes are stable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum TerminalStatus {
    /// Objective met (model's `Finish`; harness-verified steps).
    Completed = 0,
    /// Required human data unavailable (reserved; never emitted in slice).
    NeedsInput = 1,
    /// Policy or permission refused the action.
    Denied = 2,
    /// A deterministic resource guard fired.
    BudgetExhausted = 3,
    /// Loop detector found no useful progress.
    Stuck = 4,
    /// Required capability remained unavailable.
    ToolUnavailable = 5,
    /// Repair allowance exhausted.
    ModelInvalid = 6,
    /// Durable state could not be trusted.
    StorageFault = 7,
    /// Version or hash binding cannot replay safely.
    Incompatible = 8,
}

impl TerminalStatus {
    /// The stable numeric code.
    #[must_use]
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Decode a numeric code, failing closed on unknown values.
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnknownStatusCode`] for codes outside `0..=8`.
    pub const fn from_code(code: u8) -> Result<Self, Error> {
        match code {
            0 => Ok(Self::Completed),
            1 => Ok(Self::NeedsInput),
            2 => Ok(Self::Denied),
            3 => Ok(Self::BudgetExhausted),
            4 => Ok(Self::Stuck),
            5 => Ok(Self::ToolUnavailable),
            6 => Ok(Self::ModelInvalid),
            7 => Ok(Self::StorageFault),
            8 => Ok(Self::Incompatible),
            _ => Err(Error::UnknownStatusCode { code }),
        }
    }

    /// The stable name of the status.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::NeedsInput => "needs_input",
            Self::Denied => "denied",
            Self::BudgetExhausted => "budget_exhausted",
            Self::Stuck => "stuck",
            Self::ToolUnavailable => "tool_unavailable",
            Self::ModelInvalid => "model_invalid",
            Self::StorageFault => "storage_fault",
            Self::Incompatible => "incompatible",
        }
    }

    /// Whether this status claims the objective was met.
    ///
    /// Only [`TerminalStatus::Completed`] claims it (§8.2).
    #[must_use]
    pub const fn is_success(self) -> bool {
        matches!(self, Self::Completed)
    }
}

impl Display for TerminalStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}
