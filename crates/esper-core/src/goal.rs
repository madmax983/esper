//! Goals: standing objectives and their ledger (SPEC §21.2).
//!
//! A goal outlives its runs. The supervisor spawns runs to serve it,
//! and each run's terminal status feeds back through [`settle`]. The
//! goal status machine is **data**, like the run machine of §3.2:
//! [`transition`] is total over `(GoalStatus, GoalEvent)` and accepts
//! exactly the 11 edges of [`TRANSITIONS`].
//!
//! Budgets are identity one level up (ADR 10): every goal carries a
//! cumulative [`Envelope`]. A spawn *reserves* the run's grant before
//! the run exists; settlement *refunds* what the run left unspent. A
//! crash between the two holds the reservation, so the envelope can
//! never widen — [`Goal::check_envelope`] proves it after every
//! mutation of the ledger.
//!
//! Every ledger operation follows the journal discipline the
//! supervisor needs: compute (pure) → commit (runtime) → apply. Replay
//! applies the same committed inputs in the same order and reaches
//! the same ledger.

use core::fmt::{Display, Formatter, Result as FmtResult};

use crate::beat::{Beat, BeatSpan, SchedulePolicy};
use crate::compact::Note;
use crate::error::Error;
use crate::ids::RunId;
use crate::memory::ingest;
use crate::registry::Capabilities;
use crate::state::TerminalStatus;

/// Maximum goals the ledger holds at once.
pub const MAX_GOALS: usize = 8;

/// Identifies one goal. Nonzero; assigned by the ledger, monotonic,
/// never reused (a reaped goal's id stays retired).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GoalId(u16);

impl GoalId {
    /// The first id the ledger assigns.
    pub const MIN: Self = Self(1);

    /// Build a `GoalId`, rejecting zero.
    #[must_use]
    pub const fn new(id: u16) -> Option<Self> {
        if id == 0 { None } else { Some(Self(id)) }
    }

    /// Return the raw value (always `>= 1`).
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }
}

impl Display for GoalId {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "goal#{}", self.0)
    }
}

/// How often a goal wants a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Cadence {
    /// One completed run satisfies the goal.
    Once,
    /// A standing goal: every completed run reschedules it this many
    /// beats later, until its envelope runs dry or it is retired.
    Every(BeatSpan),
}

/// The goal status (SPEC §21.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GoalStatus {
    /// Eligible to run once its next beat is due.
    Active,
    /// A run is in flight.
    Running,
    /// The run is parked on `Ask`, awaiting typed human input.
    Waiting,
    /// The operator paused the goal.
    Paused,
    /// A `Once` goal's run completed. Terminal.
    Satisfied,
    /// A non-retryable status, or too many consecutive failures. Terminal.
    Failed,
    /// The envelope cannot fund another run. Terminal.
    Exhausted,
    /// The operator retired the goal. Terminal.
    Retired,
}

impl GoalStatus {
    /// Every status, in code order.
    pub const ALL: [Self; 8] = [
        Self::Active,
        Self::Running,
        Self::Waiting,
        Self::Paused,
        Self::Satisfied,
        Self::Failed,
        Self::Exhausted,
        Self::Retired,
    ];

    /// The stable numeric code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Active => 0,
            Self::Running => 1,
            Self::Waiting => 2,
            Self::Paused => 3,
            Self::Satisfied => 4,
            Self::Failed => 5,
            Self::Exhausted => 6,
            Self::Retired => 7,
        }
    }

    /// Decode a numeric code, failing closed on unknown values.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Active),
            1 => Some(Self::Running),
            2 => Some(Self::Waiting),
            3 => Some(Self::Paused),
            4 => Some(Self::Satisfied),
            5 => Some(Self::Failed),
            6 => Some(Self::Exhausted),
            7 => Some(Self::Retired),
            _ => None,
        }
    }

    /// The stable name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Paused => "paused",
            Self::Satisfied => "satisfied",
            Self::Failed => "failed",
            Self::Exhausted => "exhausted",
            Self::Retired => "retired",
        }
    }

    /// Whether the status has no outgoing edge.
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Satisfied | Self::Failed | Self::Exhausted | Self::Retired
        )
    }

    /// Whether a run is in flight or parked for this goal.
    #[must_use]
    pub const fn is_busy(self) -> bool {
        matches!(self, Self::Running | Self::Waiting)
    }
}

impl Display for GoalStatus {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}

/// An event on the goal status machine (SPEC §21.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GoalEvent {
    /// A run was spawned.
    Spawn,
    /// The run suspended on `Ask`.
    Park,
    /// Input arrived; the parked run resumes.
    Resume,
    /// The run settled; the goal is rescheduled.
    Recur,
    /// A `Once` goal's run completed.
    Satisfy,
    /// The goal failed for good.
    Fail,
    /// The envelope cannot fund another run.
    Exhaust,
    /// Operator pause.
    Pause,
    /// Operator resume.
    Unpause,
    /// Operator retire.
    Retire,
}

impl GoalEvent {
    /// Every event, in code order.
    pub const ALL: [Self; 10] = [
        Self::Spawn,
        Self::Park,
        Self::Resume,
        Self::Recur,
        Self::Satisfy,
        Self::Fail,
        Self::Exhaust,
        Self::Pause,
        Self::Unpause,
        Self::Retire,
    ];

    /// The stable numeric code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Spawn => 0,
            Self::Park => 1,
            Self::Resume => 2,
            Self::Recur => 3,
            Self::Satisfy => 4,
            Self::Fail => 5,
            Self::Exhaust => 6,
            Self::Pause => 7,
            Self::Unpause => 8,
            Self::Retire => 9,
        }
    }

    /// Decode a numeric code, failing closed on unknown values.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Spawn),
            1 => Some(Self::Park),
            2 => Some(Self::Resume),
            3 => Some(Self::Recur),
            4 => Some(Self::Satisfy),
            5 => Some(Self::Fail),
            6 => Some(Self::Exhaust),
            7 => Some(Self::Pause),
            8 => Some(Self::Unpause),
            9 => Some(Self::Retire),
            _ => None,
        }
    }

    /// The stable name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Spawn => "spawn",
            Self::Park => "park",
            Self::Resume => "resume",
            Self::Recur => "recur",
            Self::Satisfy => "satisfy",
            Self::Fail => "fail",
            Self::Exhaust => "exhaust",
            Self::Pause => "pause",
            Self::Unpause => "unpause",
            Self::Retire => "retire",
        }
    }

    /// Whether the operator may apply this event directly.
    #[must_use]
    pub const fn is_operator(self) -> bool {
        matches!(self, Self::Pause | Self::Unpause | Self::Retire)
    }
}

impl Display for GoalEvent {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}

/// The normative goal transition table (SPEC §21.2): exactly 11 edges.
pub const TRANSITIONS: [(GoalStatus, GoalEvent, GoalStatus); 11] = [
    (GoalStatus::Active, GoalEvent::Spawn, GoalStatus::Running),
    (GoalStatus::Running, GoalEvent::Park, GoalStatus::Waiting),
    (GoalStatus::Waiting, GoalEvent::Resume, GoalStatus::Running),
    (GoalStatus::Running, GoalEvent::Recur, GoalStatus::Active),
    (
        GoalStatus::Running,
        GoalEvent::Satisfy,
        GoalStatus::Satisfied,
    ),
    (GoalStatus::Running, GoalEvent::Fail, GoalStatus::Failed),
    (
        GoalStatus::Running,
        GoalEvent::Exhaust,
        GoalStatus::Exhausted,
    ),
    (GoalStatus::Active, GoalEvent::Pause, GoalStatus::Paused),
    (GoalStatus::Paused, GoalEvent::Unpause, GoalStatus::Active),
    (GoalStatus::Active, GoalEvent::Retire, GoalStatus::Retired),
    (GoalStatus::Paused, GoalEvent::Retire, GoalStatus::Retired),
];

/// Apply `event` to `from`.
///
/// # Errors
///
/// Returns [`Error::GoalIllegalTransition`] for every pair outside
/// [`TRANSITIONS`].
pub const fn transition(from: GoalStatus, event: GoalEvent) -> Result<GoalStatus, Error> {
    let mut i = 0;
    while i < TRANSITIONS.len() {
        let (src, ev, dst) = TRANSITIONS[i];
        if src.code() == from.code() && ev.code() == event.code() {
            return Ok(dst);
        }
        i += 1;
    }
    Err(Error::GoalIllegalTransition { from, event })
}

/// The cumulative budget envelope a goal is granted (SPEC §21.2).
///
/// Immutable once the goal is admitted; the goal's [`Remaining`] and
/// [`Grant`] reservation are checked against it after every change.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Envelope {
    /// Model turns granted to each run (`>= 1`).
    pub per_run_turns: u16,
    /// Mutations granted to each run (`>= 1`: the run guard treats a
    /// zeroed unit as exhausted, §7.2).
    pub per_run_mutations: u16,
    /// Model turns across every run.
    pub turns: u32,
    /// Mutations across every run.
    pub mutations: u32,
    /// Runs the goal may spawn.
    pub runs: u16,
}

impl Envelope {
    /// Validate the envelope: every grant nonzero, so the goal can
    /// fund at least one run.
    ///
    /// # Errors
    ///
    /// Returns [`Error::GoalSpecInvalid`] naming the zero grant.
    pub const fn validate(&self) -> Result<(), Error> {
        if self.per_run_turns == 0 {
            return Err(Error::GoalSpecInvalid("per_run_turns must be at least 1"));
        }
        if self.per_run_mutations == 0 {
            return Err(Error::GoalSpecInvalid(
                "per_run_mutations must be at least 1",
            ));
        }
        if self.turns == 0 || self.mutations == 0 || self.runs == 0 {
            return Err(Error::GoalSpecInvalid(
                "the envelope must fund at least one run",
            ));
        }
        Ok(())
    }
}

/// What remains of a goal's envelope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Remaining {
    /// Model turns not yet reserved.
    pub turns: u32,
    /// Mutations not yet reserved.
    pub mutations: u32,
    /// Runs not yet spawned.
    pub runs: u16,
}

impl Remaining {
    /// Whether another run can be funded: at least one turn, one
    /// mutation, and one run (SPEC §21.2).
    #[must_use]
    pub const fn fundable(&self) -> bool {
        self.turns >= 1 && self.mutations >= 1 && self.runs >= 1
    }
}

/// A per-run grant: the reservation a spawn takes, or the refund a
/// settlement returns.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Grant {
    /// Model turns.
    pub turns: u16,
    /// Mutations.
    pub mutations: u16,
}

impl Grant {
    /// The empty grant.
    pub const ZERO: Self = Self {
        turns: 0,
        mutations: 0,
    };
}

/// What the operator asks for (SPEC §21.2). The objective is raw: the
/// ledger masks it before it becomes durable.
#[derive(Debug, Clone, Copy)]
pub struct GoalSpec<'a> {
    /// The objective text, as typed.
    pub objective: &'a [u8],
    /// Higher runs first.
    pub priority: u8,
    /// One-shot or standing.
    pub cadence: Cadence,
    /// The cumulative budget envelope.
    pub envelope: Envelope,
    /// What the goal's runs may touch; must not widen the agent's.
    pub capabilities: Capabilities,
}

/// One goal in the ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Goal {
    /// The ledger-assigned id.
    pub id: GoalId,
    /// The masked, bounded objective.
    pub objective: Note,
    /// Higher runs first.
    pub priority: u8,
    /// One-shot or standing.
    pub cadence: Cadence,
    /// The granted envelope (immutable).
    pub envelope: Envelope,
    /// What remains of the envelope.
    pub remaining: Remaining,
    /// The in-flight run's reservation ([`Grant::ZERO`] when idle).
    pub reserved: Grant,
    /// What the goal's runs may touch.
    pub capabilities: Capabilities,
    /// The current status.
    pub status: GoalStatus,
    /// The first beat the goal may run again.
    pub next_due: Beat,
    /// Consecutive failed runs.
    pub failures: u8,
    /// Runs spawned so far.
    pub runs_started: u32,
    /// The last settled run's terminal status.
    pub last_status: Option<TerminalStatus>,
    /// The beat the goal parked on, while `Waiting`.
    pub waiting_since: Beat,
    /// The in-flight or parked run.
    pub run: Option<RunId>,
}

impl Goal {
    /// Prove the envelope has not widened: remaining plus reserved
    /// never exceeds the grant in any unit.
    ///
    /// # Errors
    ///
    /// Returns [`Error::EnvelopeWidened`] when it has.
    pub const fn check_envelope(&self) -> Result<(), Error> {
        let in_flight = if self.status.is_busy() { 1 } else { 0 };
        let turns = self.remaining.turns as u64 + self.reserved.turns as u64;
        let mutations = self.remaining.mutations as u64 + self.reserved.mutations as u64;
        let runs = self.remaining.runs as u32 + in_flight;
        if turns > self.envelope.turns as u64
            || mutations > self.envelope.mutations as u64
            || runs > self.envelope.runs as u32
        {
            return Err(Error::EnvelopeWidened);
        }
        if !self.status.is_busy() && (self.reserved.turns != 0 || self.reserved.mutations != 0) {
            return Err(Error::EnvelopeWidened);
        }
        Ok(())
    }

    /// Whether the goal is waiting and has waited at least
    /// `policy.overdue_after` beats by `now` (a signal, never a
    /// timeout).
    #[must_use]
    pub const fn is_overdue(&self, now: Beat, policy: &SchedulePolicy) -> bool {
        matches!(self.status, GoalStatus::Waiting)
            && now.since(self.waiting_since) >= policy.overdue_after.get() as u64
    }
}

/// How a run's terminal status settles its goal (SPEC §21.2), before
/// the envelope's say.
const fn retryable(status: TerminalStatus) -> bool {
    !matches!(
        status,
        TerminalStatus::Denied | TerminalStatus::Incompatible | TerminalStatus::StorageFault
    )
}

/// The settlement decision for a `Running` goal: the event to apply,
/// the next due beat, and the new failure streak (SPEC §21.2 table).
///
/// Pure: `goal.remaining` must already include the run's refund.
///
/// # Errors
///
/// Returns [`Error::BeatOverflow`] when the next due beat overflows.
pub fn settle(
    goal: &Goal,
    status: TerminalStatus,
    beat: Beat,
    policy: &SchedulePolicy,
) -> Result<(GoalEvent, Beat, u8), Error> {
    if status.is_success() {
        return match goal.cadence {
            Cadence::Once => Ok((GoalEvent::Satisfy, beat, 0)),
            Cadence::Every(span) => {
                if !goal.remaining.fundable() {
                    return Ok((GoalEvent::Exhaust, beat, 0));
                }
                match beat.after(span) {
                    Ok(due) => Ok((GoalEvent::Recur, due, 0)),
                    Err(e) => Err(e),
                }
            }
        };
    }
    let streak = goal.failures.saturating_add(1);
    if !retryable(status) || streak >= policy.max_consecutive_failures {
        return Ok((GoalEvent::Fail, beat, streak));
    }
    if !goal.remaining.fundable() {
        return Ok((GoalEvent::Exhaust, beat, streak));
    }
    match beat.after(policy.backoff(streak)) {
        Ok(due) => Ok((GoalEvent::Recur, due, streak)),
        Err(e) => Err(e),
    }
}

/// What the scheduler chose for a beat (SPEC §21.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pick {
    /// Nothing is due and nothing parked has input.
    Idle,
    /// Spawn a new run for this goal.
    Spawn(GoalId),
    /// Resume this goal's parked run: its input arrived.
    Resume(GoalId),
}

/// Goal counts for the heartbeat pulse.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GoalCounts {
    /// `Active` goals.
    pub active: u8,
    /// `Running` or `Waiting` goals.
    pub waiting: u8,
    /// Waiting goals past the overdue threshold.
    pub overdue: u8,
    /// `Paused` goals.
    pub paused: u8,
    /// Terminal goals still in the ledger.
    pub done: u8,
}

/// The goal ledger: at most [`MAX_GOALS`] goals plus the id allocator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GoalLedger {
    slots: [Option<Goal>; MAX_GOALS],
    next_id: u16,
}

impl Default for GoalLedger {
    fn default() -> Self {
        Self::new()
    }
}

impl GoalLedger {
    /// An empty ledger; the first goal gets id 1.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_GOALS],
            next_id: 1,
        }
    }

    /// The id the next admitted goal will get (`0` once exhausted).
    #[must_use]
    pub const fn next_id(&self) -> u16 {
        self.next_id
    }

    /// The goals, in slot order.
    pub fn goals(&self) -> impl Iterator<Item = &Goal> {
        self.slots.iter().flatten()
    }

    /// The number of goals held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.goals().count()
    }

    /// Whether the ledger holds no goal.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The goal with this id.
    #[must_use]
    pub fn get(&self, id: GoalId) -> Option<&Goal> {
        self.goals().find(|goal| goal.id == id)
    }

    fn get_mut(&mut self, id: GoalId) -> Result<&mut Goal, Error> {
        self.slots
            .iter_mut()
            .flatten()
            .find(|goal| goal.id == id)
            .ok_or_else(|| Error::GoalUnknown { goal: id.get() })
    }

    /// Build the goal `spec` would admit, without changing the ledger
    /// (compute → commit → [`Self::insert`]).
    ///
    /// The objective is masked and bounded ([`ingest`]); the envelope
    /// and capabilities are validated against the agent's `ceiling`.
    ///
    /// # Errors
    ///
    /// [`Error::GoalSpecInvalid`] for a bad envelope or a widening
    /// capability set; [`Error::MemoryInputTooLong`] for an overlong
    /// objective; [`Error::GoalLedgerFull`] / [`Error::GoalIdsExhausted`]
    /// when no slot or id remains.
    pub fn admit(
        &self,
        spec: &GoalSpec<'_>,
        ceiling: &Capabilities,
        now: Beat,
    ) -> Result<Goal, Error> {
        spec.envelope.validate()?;
        if spec.capabilities.widens(ceiling) {
            return Err(Error::GoalSpecInvalid(
                "capabilities widen the agent's ceiling",
            ));
        }
        if spec.objective.is_empty() {
            return Err(Error::GoalSpecInvalid("objective must not be empty"));
        }
        let objective = ingest(spec.objective)?;
        let id = GoalId::new(self.next_id).ok_or(Error::GoalIdsExhausted)?;
        if self.slots.iter().all(Option::is_some) {
            return Err(Error::GoalLedgerFull);
        }
        Ok(Goal {
            id,
            objective,
            priority: spec.priority,
            cadence: spec.cadence,
            envelope: spec.envelope,
            remaining: Remaining {
                turns: spec.envelope.turns,
                mutations: spec.envelope.mutations,
                runs: spec.envelope.runs,
            },
            reserved: Grant::ZERO,
            capabilities: spec.capabilities,
            status: GoalStatus::Active,
            next_due: now,
            failures: 0,
            runs_started: 0,
            last_status: None,
            waiting_since: Beat::ZERO,
            run: None,
        })
    }

    /// Insert a committed goal. It must carry exactly the next id —
    /// replay inserts the same goals in the same order.
    ///
    /// # Errors
    ///
    /// [`Error::GoalOutOfOrder`] for any other id, or a goal that is
    /// not freshly admitted; [`Error::GoalLedgerFull`] with no slot.
    pub fn insert(&mut self, goal: Goal) -> Result<(), Error> {
        if goal.id.get() != self.next_id || goal.status != GoalStatus::Active {
            return Err(Error::GoalOutOfOrder);
        }
        goal.check_envelope()?;
        let slot = self
            .slots
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::GoalLedgerFull)?;
        *slot = Some(goal);
        // `u16::MAX` is the last id; afterwards `next_id` is 0 and
        // `admit` fails closed with `GoalIdsExhausted`.
        self.next_id = self.next_id.checked_add(1).unwrap_or(0);
        Ok(())
    }

    /// Choose the beat's work (SPEC §21.2): a parked goal whose input
    /// arrived; else the best due active goal; else idle.
    #[must_use]
    pub fn pick(&self, beat: Beat, input_ready: impl Fn(GoalId) -> bool) -> Pick {
        let resume = self
            .goals()
            .filter(|g| g.status == GoalStatus::Waiting && input_ready(g.id))
            .min_by_key(|g| (core::cmp::Reverse(g.priority), g.id));
        if let Some(goal) = resume {
            return Pick::Resume(goal.id);
        }
        let spawn = self
            .goals()
            .filter(|g| g.status == GoalStatus::Active && g.next_due <= beat)
            .min_by_key(|g| (core::cmp::Reverse(g.priority), g.next_due, g.id));
        spawn.map_or(Pick::Idle, |goal| Pick::Spawn(goal.id))
    }

    /// The reservation a spawn of `id` would take: `min(per_run,
    /// remaining)` of each unit.
    ///
    /// # Errors
    ///
    /// [`Error::GoalUnknown`]; [`Error::GoalIllegalTransition`] when
    /// the goal is not `Active`; [`Error::EnvelopeWidened`] when the
    /// goal is not fundable (the scheduler never picks one).
    pub fn reservation(&self, id: GoalId) -> Result<Grant, Error> {
        let goal = self
            .get(id)
            .ok_or_else(|| Error::GoalUnknown { goal: id.get() })?;
        transition(goal.status, GoalEvent::Spawn)?;
        if !goal.remaining.fundable() {
            return Err(Error::EnvelopeWidened);
        }
        let turns = clamp_u16(goal.remaining.turns, goal.envelope.per_run_turns);
        let mutations = clamp_u16(goal.remaining.mutations, goal.envelope.per_run_mutations);
        Ok(Grant { turns, mutations })
    }

    /// Apply a committed spawn: `Active → Running`, reserve `grant`,
    /// record the run.
    ///
    /// # Errors
    ///
    /// Everything [`Self::reservation`] returns, plus
    /// [`Error::GoalOutOfOrder`] when `grant` is not the reservation
    /// the ledger computes (a replay divergence).
    pub fn spawn(&mut self, id: GoalId, run: RunId, grant: Grant) -> Result<(), Error> {
        if self.reservation(id)? != grant {
            return Err(Error::GoalOutOfOrder);
        }
        let goal = self.get_mut(id)?;
        goal.status = transition(goal.status, GoalEvent::Spawn)?;
        goal.remaining.turns -= u32::from(grant.turns);
        goal.remaining.mutations -= u32::from(grant.mutations);
        goal.remaining.runs -= 1;
        goal.reserved = grant;
        goal.runs_started = goal.runs_started.saturating_add(1);
        goal.run = Some(run);
        goal.check_envelope()
    }

    /// Apply a committed park: `Running → Waiting` at `beat`. The
    /// reservation stays held: the run is still alive.
    ///
    /// # Errors
    ///
    /// [`Error::GoalUnknown`] or [`Error::GoalIllegalTransition`].
    pub fn park(&mut self, id: GoalId, beat: Beat) -> Result<(), Error> {
        let goal = self.get_mut(id)?;
        goal.status = transition(goal.status, GoalEvent::Park)?;
        goal.waiting_since = beat;
        goal.check_envelope()
    }

    /// Apply a committed resume: `Waiting → Running`.
    ///
    /// # Errors
    ///
    /// [`Error::GoalUnknown`] or [`Error::GoalIllegalTransition`].
    pub fn resume(&mut self, id: GoalId) -> Result<(), Error> {
        let goal = self.get_mut(id)?;
        goal.status = transition(goal.status, GoalEvent::Resume)?;
        goal.waiting_since = Beat::ZERO;
        goal.check_envelope()
    }

    /// Apply a committed settlement: refund, decide ([`settle`]),
    /// transition. Returns the event applied.
    ///
    /// # Errors
    ///
    /// [`Error::GoalUnknown`]; [`Error::GoalIllegalTransition`] when
    /// the goal is not `Running`; [`Error::RefundExceedsReservation`]
    /// when `refund` is larger than the reservation in any unit.
    pub fn settle(
        &mut self,
        id: GoalId,
        status: TerminalStatus,
        refund: Grant,
        beat: Beat,
        policy: &SchedulePolicy,
    ) -> Result<GoalEvent, Error> {
        let goal = self.get_mut(id)?;
        if goal.status != GoalStatus::Running {
            return Err(Error::GoalIllegalTransition {
                from: goal.status,
                event: GoalEvent::Recur,
            });
        }
        if refund.turns > goal.reserved.turns || refund.mutations > goal.reserved.mutations {
            return Err(Error::RefundExceedsReservation);
        }
        goal.remaining.turns += u32::from(refund.turns);
        goal.remaining.mutations += u32::from(refund.mutations);
        goal.reserved = Grant::ZERO;
        let (event, due, streak) = settle(goal, status, beat, policy)?;
        goal.status = transition(goal.status, event)?;
        goal.next_due = due;
        goal.failures = streak;
        goal.last_status = Some(status);
        goal.run = None;
        goal.check_envelope()?;
        Ok(event)
    }

    /// Apply a committed operator event (pause, unpause, retire).
    ///
    /// # Errors
    ///
    /// [`Error::GoalBusy`] while a run is in flight or parked (an open
    /// obligation is never dropped); [`Error::GoalIllegalTransition`]
    /// for a non-operator event or an illegal pair.
    pub fn control(&mut self, id: GoalId, event: GoalEvent, now: Beat) -> Result<(), Error> {
        let goal = self.get_mut(id)?;
        if goal.status.is_busy() {
            return Err(Error::GoalBusy);
        }
        if !event.is_operator() {
            return Err(Error::GoalIllegalTransition {
                from: goal.status,
                event,
            });
        }
        goal.status = transition(goal.status, event)?;
        if event == GoalEvent::Unpause && goal.next_due < now {
            goal.next_due = now;
        }
        Ok(())
    }

    /// Remove terminal goals (checkpoint compaction, SPEC §21.6).
    /// Returns how many were reaped. Their ids stay retired.
    ///
    /// The survivors keep their order and move to the front: storage
    /// stays canonical (no holes), so a checkpoint decodes to exactly
    /// the ledger it encodes and later inserts land in the same slots
    /// whether or not the agent checkpointed.
    pub fn reap(&mut self) -> usize {
        let before = self.len();
        let mut kept = [None; MAX_GOALS];
        for (dst, goal) in kept
            .iter_mut()
            .zip(self.goals().filter(|goal| !goal.status.is_terminal()))
        {
            *dst = Some(*goal);
        }
        self.slots = kept;
        before - self.len()
    }

    /// Whether any goal has a run in flight (not parked).
    #[must_use]
    pub fn has_running(&self) -> bool {
        self.goals().any(|goal| goal.status == GoalStatus::Running)
    }

    /// Goal counts for the heartbeat pulse.
    #[must_use]
    pub fn counts(&self, now: Beat, policy: &SchedulePolicy) -> GoalCounts {
        let mut counts = GoalCounts::default();
        for goal in self.goals() {
            match goal.status {
                GoalStatus::Active => counts.active += 1,
                GoalStatus::Running | GoalStatus::Waiting => counts.waiting += 1,
                GoalStatus::Paused => counts.paused += 1,
                GoalStatus::Satisfied
                | GoalStatus::Failed
                | GoalStatus::Exhausted
                | GoalStatus::Retired => counts.done += 1,
            }
            if goal.is_overdue(now, policy) {
                counts.overdue += 1;
            }
        }
        counts
    }

    /// Rebuild a ledger from decoded parts (checkpoint restore),
    /// validating every structural invariant.
    ///
    /// # Errors
    ///
    /// [`Error::SnapshotCorrupt`] for duplicate ids, an id at or past
    /// the allocator, a goal in flight, or a widened envelope.
    pub fn from_parts(goals: &[Goal], next_id: u16) -> Result<Self, Error> {
        if goals.len() > MAX_GOALS {
            return Err(Error::SnapshotCorrupt);
        }
        let mut ledger = Self {
            slots: [None; MAX_GOALS],
            next_id,
        };
        for (slot, goal) in ledger.slots.iter_mut().zip(goals.iter()) {
            let past_allocator = next_id != 0 && goal.id.get() >= next_id;
            if past_allocator
                || goal.status == GoalStatus::Running
                || goal.check_envelope().is_err()
            {
                return Err(Error::SnapshotCorrupt);
            }
            *slot = Some(*goal);
        }
        for (i, goal) in goals.iter().enumerate() {
            if goals.iter().skip(i + 1).any(|other| other.id == goal.id) {
                return Err(Error::SnapshotCorrupt);
            }
        }
        Ok(ledger)
    }
}

/// `min(value, cap)` as `u16`.
fn clamp_u16(value: u32, cap: u16) -> u16 {
    u16::try_from(value.min(u32::from(cap))).unwrap_or(cap)
}
