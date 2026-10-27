//! The durable `ReAct` run as a Waymaker workflow (SPEC §22).
//!
//! [`run`] is an `async fn` over [`waymaker_embassy::Ctx`]: `no_std`,
//! allocation-free, polled by Embassy's executor on the device. It walks
//! `esper-core`'s normative state machine ([`State`]) through
//! [`transition`], so an illegal step fails closed.
//!
//! * **Every world touch is an activity.** Inference ([`wire::INFER`]),
//!   the device business rule ([`wire::PROBE`]), each tool dispatch
//!   attempt ([`wire::TOOL`]), the independent read-back
//!   ([`wire::VERIFY`]) and operator input ([`wire::ASK`]) are recorded
//!   in Waymaker's journal before the workflow sees them. Replay hands
//!   the same answers back, so a reboot rebuilds budgets, the monitor,
//!   the effect allocator and the machine position by re-running the
//!   workflow — there is no second journal.
//! * **Intent before effect.** An Esper effect id is allocated when a
//!   call is authorized; every dispatch attempt carries it, so the
//!   device deduplicates a redelivered attempt. A transient fault is a
//!   recorded attempt, and the retry is a new activity under the same
//!   Esper effect id.
//! * **Suspension is an unserviceable activity.** [`wire::ASK`] stays
//!   outstanding until the operator's input arrives; the boot that has
//!   it completes the activity.
//! * **Rollover is `continue_as_new`.** The compaction fold is built as
//!   frames commit; at 80% of the context budget it is encoded, stored
//!   chunk by chunk ([`wire::SNAP_STORE`], one page each), and the run
//!   continues as new over a [`RunStart`] naming the child seed and the
//!   stored fold. The child loads and verifies it before its first step.
//! * **State is caller-owned.** [`RunState`] is passed by `&mut`, so on
//!   the device it lives in a `static`, not in the future.
//!
//! The dispatcher behind the activities is the device's; [`device`]
//! holds the product half every dispatcher shares (prompt rendering,
//! masking, argument binding). On the host, `host` drives this workflow
//! over the test doubles.

pub mod device;
#[cfg(feature = "host")]
pub mod host;
pub mod wire;

use core::convert::Infallible;
use core::fmt::{self, Write as _};

use esper_core::ResourceBudget;
use esper_core::beat::Beat;
use esper_core::checkpoint::{MAX_ENTRY_RECORD_BYTES, decode_entry, encode_entry};
use esper_core::compact::{
    CompactState, DEFAULT_POLICY, FrameSummary, VersionSet, compact, should_compact,
};
use esper_core::decision::{Decision, Level, ToolArgs, decode_line};
use esper_core::error::{ErrorCode, RepairVariant};
use esper_core::goal::GoalId;
use esper_core::harvest::{HarvestSet, Harvester, MAX_HARVEST};
use esper_core::ids::{Digest, ToolId};
use esper_core::lineage::{Lineage, continue_as_new};
use esper_core::memory::Provenance;
use esper_core::monitor::{Monitor, MonitorVerdict, ProgressDelta, StepOutcome, StepRecord};
use esper_core::records::RecordKind;
use esper_core::registry::{authorize, lookup_by_id};
use esper_core::snapshot::{decode, encode, verify};
use esper_core::state::{Event, State, TerminalStatus, transition};
use esper_protocol::PermissionClass;
use waymaker_core::{EffectIdAllocator, RunId as WaymakerRunId};
use waymaker_embassy::ctx::Failure;
use waymaker_embassy::{ActivityDispatcher, Ctx, Journal};

use crate::agent::wire::{PAGE_RECORD_MAX, WireError};
use crate::backend::{OUTPUT_CAP, PriorCtx, RepairHint, fnv1a64};
use crate::error::RuntimeError;
use crate::seed::RunSeed;
use wire::{
    AskInput, AskRequest, Check, Chunk, HARVEST_ITEM_BASE, InferRequest, Inferred,
    LAST_OBSERVATION_CAP, MAX_REASON, Observed, Probed, RunStart, SNAPSHOT_CHUNK,
    SNAPSHOT_KEY_BYTES, SnapshotRef, Stored, TerminalRecord, ToolRequest, Verified, VerifyRequest,
    fault_error, snapshot_key,
};

/// Invalid model lines before the run ends `ModelInvalid` (SPEC §4.4).
pub const MAX_REPAIRS: u8 = 3;

/// Dispatch attempts under one effect id: the first try plus two
/// retries (SPEC §6).
pub const MAX_TRANSIENT_ATTEMPTS: u8 = 3;

/// The largest encoded rollover snapshot: six [`SNAPSHOT_CHUNK`]s,
/// enough for a compact state with every list full (pinned by test).
pub const MAX_SNAPSHOT_BYTES: usize = 6 * SNAPSHOT_CHUNK;

// ---------------------------------------------------------------------
// Bounded bytes and text

/// Up to `N` bytes, inline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Buf<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> Buf<N> {
    /// Empty.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    /// A copy of `bytes`, or `None` when they exceed `N`.
    #[must_use]
    pub fn of(bytes: &[u8]) -> Option<Self> {
        let mut buf = Self::new();
        buf.bytes.get_mut(..bytes.len())?.copy_from_slice(bytes);
        buf.len = bytes.len();
        Some(buf)
    }

    /// A copy of at most the first `N` bytes of `bytes`.
    #[must_use]
    pub fn prefix(bytes: &[u8]) -> Self {
        Self::of(bytes.get(..bytes.len().min(N)).unwrap_or(&[])).unwrap_or_default()
    }

    /// The bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl<const N: usize> Default for Buf<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> fmt::Write for Buf<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let end = self.len.checked_add(s.len()).ok_or(fmt::Error)?;
        self.bytes
            .get_mut(self.len..end)
            .ok_or(fmt::Error)?
            .copy_from_slice(s.as_bytes());
        self.len = end;
        Ok(())
    }
}

/// Format into a summary-sized buffer, failing closed on overflow.
fn text(args: fmt::Arguments<'_>) -> Result<Buf<OUTPUT_CAP>, RuntimeError> {
    let mut buf = Buf::new();
    buf.write_fmt(args).map_err(|_| RuntimeError::BadFrame)?;
    Ok(buf)
}

// ---------------------------------------------------------------------
// State

/// A committed call waiting for authorization.
#[derive(Debug, Clone, Copy)]
struct Call {
    tool: u8,
    args: Buf<OUTPUT_CAP>,
    digest: u64,
    tool_args: ToolArgs,
}

/// An authorized intent with no terminal outcome yet.
#[derive(Debug, Clone, Copy)]
struct Intent {
    /// The Esper effect sequence.
    seq: u32,
    tool: u8,
    args: Buf<OUTPUT_CAP>,
    digest: u64,
    tool_args: ToolArgs,
    /// Mutating: consumed a mutation and routes through `Verify`.
    write: bool,
    /// Dispatch attempts so far.
    attempt: u8,
    /// The pre-dispatch clock reading of a committed delay, derived
    /// from its observation (`uptime_ms - waited_ms`).
    t0: Option<u64>,
}

/// A committed ask with no input yet.
#[derive(Debug, Clone, Copy)]
struct Ask {
    prompt: Buf<OUTPUT_CAP>,
    schema: u8,
}

/// The run workflow's derived state. Caller-owned, so on the device it
/// lives in a `static`; [`run`] resets and rebuilds it on every boot by
/// replay.
#[derive(Debug)]
pub struct RunState {
    seed: RunSeed,
    sm: State,
    budget: ResourceBudget,
    monitor: Monitor,
    allocator: EffectIdAllocator,
    repair_used: u8,
    turns_used: u32,
    mutations_used: u32,
    last_observation: Option<Buf<LAST_OBSERVATION_CAP>>,
    last_repair_variant: Option<&'static str>,
    call: Option<Call>,
    intent: Option<Intent>,
    step: Option<StepRecord>,
    ask: Option<Ask>,
    finish: Option<Buf<OUTPUT_CAP>>,
    /// The parent's verified fold, for a continued run.
    compact: Option<CompactState>,
    /// This segment's fold, built frame by frame.
    fold: CompactState,
    /// A fold that refused a frame: surfaced at rollover, where the old
    /// whole-journal fold would have refused it.
    fold_fault: Option<esper_core::Error>,
    /// What the run teaches memory, fed as effects commit (SPEC §21.3)
    /// and carried across a rollover.
    harvester: Harvester,
    /// The next frame's index in the derived view (`RunStarted` is 0).
    frame_seq: u32,
    /// This segment's cumulative built-prompt bytes.
    prompt_bytes: u64,
    io: [u8; PAGE_RECORD_MAX],
    snapshot: [u8; MAX_SNAPSHOT_BYTES],
}

/// Harvested entries carry no provenance of their own: the supervisor,
/// which alone knows the goal and the beat, stamps it.
const NO_PROVENANCE: Provenance = Provenance {
    goal: None,
    run: None,
    beat: Beat::ZERO,
};

const fn version_set(seed: &RunSeed) -> VersionSet {
    // The slice does not version the tool catalog or the compaction
    // policy; zeros mark them unversioned rather than inventing
    // versions.
    VersionSet {
        workflow: seed.workflow_version as u32,
        model: Digest::new(seed.model_bundle),
        catalog: Digest::new(0),
        policy: Digest::new(0),
    }
}

impl RunState {
    /// An empty state.
    #[must_use]
    pub const fn new() -> Self {
        let seed = RunSeed::default_slice();
        Self {
            seed,
            sm: State::Recover,
            budget: seed.starting_budget(),
            monitor: Monitor::new(),
            allocator: EffectIdAllocator::for_run(WaymakerRunId(0)),
            repair_used: 0,
            turns_used: 0,
            mutations_used: 0,
            last_observation: None,
            last_repair_variant: None,
            call: None,
            intent: None,
            step: None,
            ask: None,
            finish: None,
            compact: None,
            fold: CompactState::new(Digest::new(0), seed.starting_budget(), version_set(&seed)),
            fold_fault: None,
            harvester: Harvester::new(NO_PROVENANCE),
            frame_seq: 0,
            prompt_bytes: 0,
            io: [0; PAGE_RECORD_MAX],
            snapshot: [0; MAX_SNAPSHOT_BYTES],
        }
    }

    /// Reset for a boot of `seed`'s run.
    fn reset(&mut self, seed: &RunSeed) {
        let seed = *seed;
        *self = Self {
            seed,
            budget: seed.starting_budget(),
            allocator: EffectIdAllocator::for_run(WaymakerRunId(seed.id.get())),
            fold: CompactState::new(
                Digest::new(seed.id.get()),
                seed.starting_budget(),
                version_set(&seed),
            ),
            ..Self::new()
        };
        // Frame 0 is the run's start.
        self.fold_frame(FrameSummary {
            seq: 0,
            kind: RecordKind::RunSeed,
            tool: ToolId::new(0),
            args_digest: Digest::new(0),
            outcome: StepOutcome::Ok,
            progress: ProgressDelta::NoProgress,
            payload: &[],
            pending_approval: false,
            pending_verification: false,
        });
    }

    /// The seed this state was last reset for.
    #[must_use]
    pub const fn seed(&self) -> &RunSeed {
        &self.seed
    }

    /// The remaining budget.
    #[must_use]
    pub const fn budget(&self) -> &ResourceBudget {
        &self.budget
    }

    /// This segment's cumulative built-prompt bytes.
    #[must_use]
    pub const fn prompt_bytes(&self) -> u64 {
        self.prompt_bytes
    }

    /// What the concluded run teaches memory: the kept observations and
    /// the goal's `Outcome` entry (SPEC §21.3).
    ///
    /// # Errors
    ///
    /// [`RuntimeError::Core`] when the outcome entry cannot be built.
    pub fn harvest(
        &self,
        goal: GoalId,
        status: TerminalStatus,
        summary: &[u8],
    ) -> Result<HarvestSet, RuntimeError> {
        Ok(self.harvester.finish(goal, status, summary)?)
    }

    /// The normative machine's position.
    #[must_use]
    pub const fn position(&self) -> State {
        self.sm
    }

    /// Fold one committed frame into this segment's compact state. Only
    /// a run that can roll over keeps a fold.
    fn fold_frame(&mut self, summary: FrameSummary<'_>) {
        if self.seed.context_budget_bytes.is_some()
            && self.fold_fault.is_none()
            && let Err(error) = compact(&[summary], &DEFAULT_POLICY, &mut self.fold)
        {
            self.fold_fault = Some(error);
        }
        self.frame_seq = self.frame_seq.saturating_add(1);
    }

    /// Fold a frame with the common shape: `kind`, `tool`, `digest`.
    fn fold_simple(
        &mut self,
        kind: RecordKind,
        tool: u8,
        digest: Digest,
        outcome: StepOutcome,
        progress: ProgressDelta,
        payload: &[u8],
    ) {
        let seq = self.frame_seq;
        self.fold_frame(FrameSummary {
            seq,
            kind,
            tool: ToolId::new(tool),
            args_digest: digest,
            outcome,
            progress,
            payload,
            pending_approval: false,
            pending_verification: false,
        });
    }

    fn advance(&mut self, event: Event) -> Result<(), RuntimeError> {
        self.sm = transition(self.sm, event).map_err(|_| RuntimeError::IllegalTransition)?;
        Ok(())
    }

    fn repair_hint(&self) -> Option<RepairHint> {
        self.repair_used.checked_sub(1)?;
        Some(RepairHint {
            attempt: self.repair_used,
            variant: self.last_repair_variant?,
        })
    }

    fn prior_ctx(&self) -> Option<PriorCtx> {
        let lineage = self.seed.parent?;
        let compact = self.compact.as_ref()?;
        Some(PriorCtx {
            epoch: lineage.continued_at_frame,
            failed_paths: u8::try_from(compact.failed_paths().len()).unwrap_or(u8::MAX),
            pending: u8::try_from(compact.pending().len()).unwrap_or(u8::MAX),
            facts: u8::try_from(compact.facts().len()).unwrap_or(u8::MAX),
        })
    }

    fn budget_exhausted_summary(&self, unit: &str) -> Result<Buf<OUTPUT_CAP>, RuntimeError> {
        match unit {
            "model_turns" => text(format_args!(
                "budget exhausted after {} model turns",
                self.turns_used
            )),
            "mutations" => text(format_args!(
                "budget exhausted after {} mutations",
                self.mutations_used
            )),
            _ => text(format_args!("budget exhausted: {unit}")),
        }
    }
}

impl Default for RunState {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------
// The workflow

/// The durable `ReAct` run workflow (SPEC §22).
///
/// `input` is this run's [`RunStart`]: the root run's, or what the
/// previous segment's `continue_as_new` carried. `state` is reset and
/// rebuilt by replay on every boot. The future ends the run through
/// [`Ctx::complete`] or [`Ctx::fail`] with a [`TerminalRecord`], or
/// continues it as new at a rollover; it returns only on a fault that
/// replay would hit again.
///
/// # Errors
///
/// [`RuntimeError`] when the input is malformed or the seed invalid, a
/// recorded answer does not decode, a dispatcher recorded a fault, the
/// parent fold does not verify, or the machine reaches an illegal step.
// Embassy runs tasks on a single-core executor: the workflow need not
// be `Send`, and requiring it would forbid `Cell`-based journals for
// no benefit.
#[allow(clippy::future_not_send)]
pub async fn run<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    input: &[u8],
    state: &mut RunState,
) -> Result<Infallible, RuntimeError> {
    let start = RunStart::decode(input)?;
    let seed = start.seed;
    seed.validate().map_err(RuntimeError::SeedInvalid)?;
    if seed.parent.is_some() != start.snapshot.is_some() {
        return Err(RuntimeError::JournalCorrupt);
    }
    state.reset(&seed);
    if let Some(snapshot) = start.snapshot {
        load_snapshot(ctx, snapshot, state).await?;
    }
    state.advance(Event::JournalValid)?;
    loop {
        match state.sm {
            State::Gather => gather(ctx, state).await?,
            State::Infer => infer(ctx, state).await?,
            State::Repair => repair(ctx, state).await?,
            State::Authorize => authorize_call(ctx, state).await?,
            State::AwaitInput => await_input(ctx, state).await?,
            State::Observe => observe(ctx, state).await?,
            State::Verify => verify_intent(ctx, state).await?,
            State::Account => account(ctx, state).await?,
            State::Finalize => {
                let summary = state.finish.take().ok_or(RuntimeError::IllegalTransition)?;
                state.advance(Event::FinalizeCommitted)?;
                terminate(
                    ctx,
                    state,
                    TerminalStatus::Completed,
                    b"model_finished",
                    summary.as_bytes(),
                )
                .await?;
            }
            State::End | State::Recover | State::Degraded | State::SafeStop => {
                return Err(RuntimeError::IllegalTransition);
            }
        }
    }
}

/// Map an activity failure to the runtime error it surfaces as.
fn failed<D: ActivityDispatcher, J: Journal>(
    ctx: &Ctx<'_, D, J>,
    failure: Failure<WireError>,
) -> RuntimeError {
    match failure {
        Failure::Activity { .. } => fault_error(ctx.payload()),
        Failure::Decode(error) => error.into(),
    }
}

/// End the run with `status`. Never resolves: the run is over.
#[allow(clippy::future_not_send)]
async fn terminate<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
    status: TerminalStatus,
    reason: &[u8],
    summary: &[u8],
) -> Result<(), RuntimeError> {
    let record = TerminalRecord {
        status,
        reason: reason.get(..reason.len().min(MAX_REASON)).unwrap_or(&[]),
        summary,
    };
    let n = record.encode(&mut state.io)?;
    let bytes = state.io.get(..n).ok_or(RuntimeError::BadFrame)?;
    let _ = if status == TerminalStatus::Completed {
        ctx.complete::<Infallible>(bytes).await
    } else {
        ctx.fail::<Infallible>(bytes).await
    };
    Ok(())
}

/// Take the degraded path: the event that leads there has been taken.
#[allow(clippy::future_not_send)]
async fn degrade<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
    status: TerminalStatus,
    reason: &[u8],
    summary: &[u8],
) -> Result<(), RuntimeError> {
    state.advance(Event::DegradedCommitted)?;
    terminate(ctx, state, status, reason, summary).await
}

/// Load and verify the parent's fold.
#[allow(clippy::future_not_send)]
async fn load_snapshot<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    snapshot: SnapshotRef,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    let len = usize::from(snapshot.len);
    if len > MAX_SNAPSHOT_BYTES {
        return Err(RuntimeError::JournalCorrupt);
    }
    for index in 0..snapshot.chunks() {
        let key = snapshot_key(
            snapshot.digest,
            u8::try_from(index).map_err(|_| RuntimeError::JournalCorrupt)?,
        );
        let chunk = ctx
            .activity::<Chunk>(wire::SNAP_LOAD, &key)
            .await
            .map_err(|failure| failed(ctx, failure))?;
        let at = index * SNAPSHOT_CHUNK;
        let bytes = chunk.as_bytes();
        let expected = (len - at).min(SNAPSHOT_CHUNK);
        if bytes.len() != expected {
            return Err(RuntimeError::JournalCorrupt);
        }
        state
            .snapshot
            .get_mut(at..at + expected)
            .ok_or(RuntimeError::JournalCorrupt)?
            .copy_from_slice(bytes);
    }
    let bytes = state
        .snapshot
        .get(..len)
        .ok_or(RuntimeError::JournalCorrupt)?;
    if fnv1a64(bytes) != snapshot.digest {
        return Err(RuntimeError::JournalCorrupt);
    }
    let fold = decode(bytes)?;
    // The parent's harvest continues here.
    let count = usize::from(snapshot.harvest);
    if count > MAX_HARVEST {
        return Err(RuntimeError::JournalCorrupt);
    }
    let mut carried = [None; MAX_HARVEST];
    for (index, slot) in (0..snapshot.harvest).zip(carried.iter_mut()) {
        let key = snapshot_key(
            snapshot.digest,
            HARVEST_ITEM_BASE
                .checked_add(index)
                .ok_or(RuntimeError::JournalCorrupt)?,
        );
        let chunk = ctx
            .activity::<Chunk>(wire::SNAP_LOAD, &key)
            .await
            .map_err(|failure| failed(ctx, failure))?;
        *slot = Some(decode_entry(chunk.as_bytes())?);
    }
    if let Some(first) = carried.first().copied().flatten() {
        let mut entries = [first; MAX_HARVEST];
        for (dst, src) in entries.iter_mut().zip(carried.iter().flatten()) {
            *dst = *src;
        }
        state.harvester = Harvester::resume(
            NO_PROVENANCE,
            entries.get(..count).ok_or(RuntimeError::JournalCorrupt)?,
        )?;
    }
    state.compact = Some(fold);
    // The fold is cumulative across generations: this segment folds
    // into the parent's state, so ruled-out paths survive every
    // rollover.
    state.fold = fold;
    Ok(())
}

/// `Gather`: the budget gate every step passes through, and the
/// rollover trigger.
#[allow(clippy::future_not_send)]
async fn gather<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    if state.seed.context_budget_bytes == Some(0) {
        state.advance(Event::BudgetExhausted)?;
        return degrade(
            ctx,
            state,
            TerminalStatus::BudgetExhausted,
            b"context_budget_exhausted",
            b"context budget exhausted; no rollover possible",
        )
        .await;
    }
    if let Some(unit) = state.budget.exhausted_unit() {
        state.advance(Event::BudgetExhausted)?;
        let name = unit.name();
        let mut reason: Buf<MAX_REASON> = Buf::new();
        reason
            .write_fmt(format_args!("{name}_exhausted"))
            .map_err(|_| RuntimeError::BadFrame)?;
        let summary = state.budget_exhausted_summary(name)?;
        return degrade(
            ctx,
            state,
            TerminalStatus::BudgetExhausted,
            reason.as_bytes(),
            summary.as_bytes(),
        )
        .await;
    }
    state.advance(Event::BudgetsClear)?;
    if let Some(budget) = state.seed.context_budget_bytes
        && should_compact(state.prompt_bytes, budget, &DEFAULT_POLICY)
    {
        return rollover(ctx, state).await;
    }
    Ok(())
}

/// Fold, store, and continue as new (SPEC §20.4-20.6).
#[allow(clippy::future_not_send)]
async fn rollover<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    if let Some(error) = state.fold_fault {
        return Err(error.into());
    }
    // A root segment's fold records the budget it handed on; a child
    // keeps its parent's binding.
    if state.compact.is_none() {
        state.fold.budgets = state.budget;
    }
    let len = encode(&state.fold, &mut state.snapshot)?;
    let bytes = state.snapshot.get(..len).ok_or(RuntimeError::BadFrame)?;
    // Read-back before anything leaves the run.
    verify(bytes)?;
    let digest = fnv1a64(bytes);
    let lineage = Lineage {
        parent_run: state.seed.id,
        continued_at_frame: state.frame_seq.saturating_sub(1),
        budgets_remaining: state.budget,
        versions: version_set(&state.seed),
    };
    // The no-widen check: the child must not exceed what this segment
    // proved.
    let continued = continue_as_new(&state.fold, &lineage)?;
    let chunks = len.div_ceil(SNAPSHOT_CHUNK);
    for index in 0..chunks {
        let at = index * SNAPSHOT_CHUNK;
        let end = (at + SNAPSHOT_CHUNK).min(len);
        let key = snapshot_key(
            digest,
            u8::try_from(index).map_err(|_| RuntimeError::BadFrame)?,
        );
        let (head, tail) = state.io.split_at_mut(SNAPSHOT_KEY_BYTES);
        head.copy_from_slice(&key);
        let chunk = state.snapshot.get(at..end).ok_or(RuntimeError::BadFrame)?;
        tail.get_mut(..chunk.len())
            .ok_or(RuntimeError::BadFrame)?
            .copy_from_slice(chunk);
        let input = state
            .io
            .get(..SNAPSHOT_KEY_BYTES + chunk.len())
            .ok_or(RuntimeError::BadFrame)?;
        ctx.activity::<Stored>(wire::SNAP_STORE, input)
            .await
            .map_err(|failure| failed(ctx, failure))?;
    }
    // The harvest continues in the child: seal the last effect (its
    // outcome is committed) and store what was kept, one item each.
    state.harvester.seal();
    let mut harvest: u8 = 0;
    let kept = state.harvester;
    for entry in kept.kept() {
        let key = snapshot_key(
            digest,
            HARVEST_ITEM_BASE
                .checked_add(harvest)
                .ok_or(RuntimeError::BadFrame)?,
        );
        let (head, tail) = state.io.split_at_mut(SNAPSHOT_KEY_BYTES);
        head.copy_from_slice(&key);
        let n = encode_entry(
            entry,
            tail.get_mut(..MAX_ENTRY_RECORD_BYTES)
                .ok_or(RuntimeError::BadFrame)?,
        )?;
        let input = state
            .io
            .get(..SNAPSHOT_KEY_BYTES + n)
            .ok_or(RuntimeError::BadFrame)?;
        ctx.activity::<Stored>(wire::SNAP_STORE, input)
            .await
            .map_err(|failure| failed(ctx, failure))?;
        harvest += 1;
    }
    let start = RunStart {
        seed: state.seed.child(continued.lineage),
        snapshot: Some(SnapshotRef {
            digest,
            len: u16::try_from(len).map_err(|_| RuntimeError::BadFrame)?,
            harvest,
        }),
    };
    let n = start.encode(&mut state.io)?;
    let input = state.io.get(..n).ok_or(RuntimeError::BadFrame)?;
    match ctx.continue_as_new(input).await {}
}

/// A decoded model line, with every borrowed piece copied out.
enum Classified {
    Call(Call),
    Ask(Ask),
    Finish(Buf<OUTPUT_CAP>),
    Invalid(RepairVariant),
}

fn classify(line: &[u8]) -> Classified {
    match decode_line(line) {
        Ok(Decision::Call(call)) => Buf::of(call.args_bytes()).map_or(
            Classified::Invalid(RepairVariant::InvalidArgs),
            |args| {
                Classified::Call(Call {
                    tool: call.tool().get(),
                    args,
                    digest: call.args_digest().get(),
                    tool_args: *call.args(),
                })
            },
        ),
        Ok(Decision::Ask(ask)) => {
            Buf::of(ask.prompt()).map_or(Classified::Invalid(RepairVariant::Malformed), |prompt| {
                Classified::Ask(Ask {
                    prompt,
                    schema: ask.response_schema_id(),
                })
            })
        }
        Ok(Decision::Finish(answer)) => Buf::of(answer.summary()).map_or(
            Classified::Invalid(RepairVariant::Malformed),
            Classified::Finish,
        ),
        Err(error) => {
            Classified::Invalid(error.repair_variant().unwrap_or(RepairVariant::Malformed))
        }
    }
}

/// `Infer`: one recorded inference, then the decision.
#[allow(clippy::future_not_send)]
async fn infer<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    let repair = state.repair_hint();
    let prior = state.prior_ctx();
    let request = InferRequest {
        settings: state.seed.inference,
        turns_left: state.budget.model_turns,
        mutations_left: state.budget.mutations,
        last_observation: state.last_observation.as_ref().map(Buf::as_bytes),
        repair,
        prior,
        brief: state
            .seed
            .brief
            .as_ref()
            .map(esper_core::brief::Brief::as_bytes),
    };
    let n = request.encode(&mut state.io)?;
    let input = state.io.get(..n).ok_or(RuntimeError::BadFrame)?;
    let answer = ctx
        .activity::<Inferred>(wire::INFER, input)
        .await
        .map_err(|failure| failed(ctx, failure))?;
    state.prompt_bytes = state
        .prompt_bytes
        .saturating_add(u64::from(answer.prompt_bytes));
    let line = answer.line();
    // Every path into `Infer` passed a budget gate; a failure here
    // means history over-spent.
    state
        .budget
        .consume_turn()
        .map_err(|_| RuntimeError::JournalCorrupt)?;
    state.turns_used += 1;
    state.fold_simple(
        RecordKind::ModelDecision,
        0,
        Digest::of_bytes(line),
        StepOutcome::Ok,
        ProgressDelta::NoProgress,
        &[],
    );
    match classify(line) {
        Classified::Call(call) => {
            state.call = Some(call);
            state.advance(Event::DecodeCall)
        }
        Classified::Ask(ask) => {
            state.ask = Some(ask);
            state.advance(Event::DecodeAsk)
        }
        Classified::Finish(summary) => {
            state.finish = Some(summary);
            state.advance(Event::DecodeFinish)
        }
        Classified::Invalid(variant) => {
            state.repair_used += 1;
            state.last_repair_variant = Some(match variant {
                RepairVariant::Malformed => "malformed",
                RepairVariant::InvalidArgs => "invalid_args",
            });
            state.advance(Event::DecodeMalformed)
        }
    }
}

/// `Repair`: grant another turn while repairs and turns remain.
#[allow(clippy::future_not_send)]
async fn repair<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    if state.repair_used < MAX_REPAIRS && state.budget.model_turns > 0 {
        return state.advance(Event::RepairAvailable);
    }
    state.advance(Event::RepairExhausted)?;
    if state.budget.model_turns == 0 {
        let summary = state.budget_exhausted_summary("model_turns")?;
        degrade(
            ctx,
            state,
            TerminalStatus::BudgetExhausted,
            b"model_turns_exhausted",
            summary.as_bytes(),
        )
        .await
    } else {
        let summary = text(format_args!(
            "model output invalid after {} repair turns",
            state.repair_used.saturating_sub(1)
        ))?;
        degrade(
            ctx,
            state,
            TerminalStatus::ModelInvalid,
            b"repair_budget_exhausted",
            summary.as_bytes(),
        )
        .await
    }
}

/// `Authorize`: the capability gate, the device business rule, the
/// mutation budget, the parent fold's ruled-out paths — then the intent.
/// Denials never touch hardware.
#[allow(clippy::future_not_send)]
async fn authorize_call<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    let call = state.call.take().ok_or(RuntimeError::IllegalTransition)?;
    let entry = lookup_by_id(ToolId::new(call.tool)).ok_or(RuntimeError::IllegalTransition)?;
    let write = matches!(entry.permission, PermissionClass::IdempotentWrite);
    if let Err(error) = authorize(&state.seed.capabilities, entry, &call.tool_args) {
        // The gate's only refusal is `PermissionDenied`; anything else
        // is an engine bug, failed closed.
        if !matches!(error, esper_core::error::Error::PermissionDenied { .. }) {
            return Err(RuntimeError::IllegalTransition);
        }
        state.advance(Event::Denied)?;
        let summary = denial_summary(call.tool_args)?;
        return degrade(
            ctx,
            state,
            TerminalStatus::Denied,
            denial_reason(call.tool_args),
            summary.as_bytes(),
        )
        .await;
    }
    // Device business rule (SPEC §5.2 step 3): a written pin must be
    // output-capable. The device answers, so the answer is recorded.
    if let ToolArgs::GpioPinWrite { pin, .. } = call.tool_args {
        let probed = ctx
            .activity::<Probed>(wire::PROBE, &[pin.get()])
            .await
            .map_err(|failure| failed(ctx, failure))?;
        if !probed.output {
            state.advance(Event::Denied)?;
            let summary = text(format_args!(
                "write to pin {} denied by device direction",
                pin.get()
            ))?;
            return degrade(
                ctx,
                state,
                TerminalStatus::Denied,
                b"pin_direction_denied",
                summary.as_bytes(),
            )
            .await;
        }
    }
    if write && state.budget.mutations == 0 {
        state.advance(Event::Denied)?;
        return degrade(
            ctx,
            state,
            TerminalStatus::BudgetExhausted,
            b"mutations_exhausted",
            b"mutation budget exhausted",
        )
        .await;
    }
    // Never re-dispatch a path the parent fold ruled out (SPEC §20.5).
    if ruled_out(state, &call) {
        state.advance(Event::Denied)?;
        let summary = text(format_args!(
            "parent fold ruled out tool {} with args digest {:x}",
            call.tool, call.digest
        ))?;
        return degrade(
            ctx,
            state,
            TerminalStatus::Stuck,
            b"failed_path_ruled_out",
            summary.as_bytes(),
        )
        .await;
    }
    let effect = state.allocator.allocate().map_err(RuntimeError::Waymaker)?;
    if write {
        state
            .budget
            .consume_mutation()
            .map_err(|_| RuntimeError::JournalCorrupt)?;
        state.mutations_used += 1;
    }
    state.fold_simple(
        RecordKind::ToolRequest,
        call.tool,
        Digest::new(call.digest),
        StepOutcome::Ok,
        ProgressDelta::NoProgress,
        &[],
    );
    state
        .harvester
        .request(effect.seq.0, ToolId::new(call.tool), call.args.as_bytes());
    state.intent = Some(Intent {
        seq: effect.seq.0,
        tool: call.tool,
        args: call.args,
        digest: call.digest,
        tool_args: call.tool_args,
        write,
        attempt: 0,
        t0: None,
    });
    state.advance(Event::Authorized)
}

/// Whether the parent fold already ruled out `call`'s exact path.
fn ruled_out(state: &RunState, call: &Call) -> bool {
    let digest = Digest::new(call.digest);
    state.compact.as_ref().is_some_and(|compact| {
        compact
            .failed_paths()
            .iter()
            .any(|path| path.tool == ToolId::new(call.tool) && path.args_digest == digest)
    })
}

/// `AwaitInput`: ask, and wait (durably) for the operator.
#[allow(clippy::future_not_send)]
async fn await_input<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    let ask = state.ask.ok_or(RuntimeError::IllegalTransition)?;
    state.fold_simple(
        RecordKind::ApprovalRequest,
        0,
        Digest::of_bytes(ask.prompt.as_bytes()),
        StepOutcome::Ok,
        ProgressDelta::NoProgress,
        ask.prompt.as_bytes(),
    );
    let request = AskRequest {
        schema: ask.schema,
        prompt: ask.prompt.as_bytes(),
    };
    let n = request.encode(&mut state.io)?;
    let input = state.io.get(..n).ok_or(RuntimeError::BadFrame)?;
    let answer = ctx
        .activity::<AskInput>(wire::ASK, input)
        .await
        .map_err(|failure| failed(ctx, failure))?;
    let bytes = answer.as_bytes();
    device::validate_ask_input(bytes, ask.schema).map_err(RuntimeError::World)?;
    let digest = Digest::of_bytes(bytes);
    state.fold_simple(
        RecordKind::ApprovalDecision,
        0,
        digest,
        StepOutcome::Ok,
        ProgressDelta::InputReceived,
        &[],
    );
    state.step = Some(StepRecord::ok(
        ToolId::new(0),
        digest,
        ProgressDelta::InputReceived,
    ));
    state.ask = None;
    state.advance(Event::InputArrived)
}

/// `Observe`: one dispatch attempt of the committed intent.
#[allow(clippy::future_not_send)]
async fn observe<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    let mut intent = state.intent.take().ok_or(RuntimeError::IllegalTransition)?;
    intent.attempt = intent.attempt.saturating_add(1);
    let request = ToolRequest {
        seq: intent.seq,
        attempt: intent.attempt,
        tool: intent.tool,
        write: intent.write,
        digest: intent.digest,
        args: intent.args.as_bytes(),
    };
    let n = request.encode(&mut state.io)?;
    let input = state.io.get(..n).ok_or(RuntimeError::BadFrame)?;
    let observed = ctx
        .activity::<Observed>(wire::TOOL, input)
        .await
        .map_err(|failure| failed(ctx, failure))?;
    let outcome = observed.outcome();
    let progress = if observed.transient || intent.write {
        ProgressDelta::NoProgress
    } else {
        ProgressDelta::NewEvidence
    };
    state.fold_simple(
        RecordKind::ToolObservation,
        intent.tool,
        Digest::new(intent.digest),
        StepOutcome::Ok,
        progress,
        outcome,
    );
    state
        .harvester
        .observation(intent.seq, !observed.transient, outcome);
    if observed.transient {
        if intent.attempt >= MAX_TRANSIENT_ATTEMPTS {
            state.advance(Event::TransientExhausted)?;
            let summary = text(format_args!(
                "transient faults exhausted after {} attempts",
                intent.attempt
            ))?;
            return degrade(
                ctx,
                state,
                TerminalStatus::Stuck,
                b"transient_attempts_exhausted",
                summary.as_bytes(),
            )
            .await;
        }
        state.intent = Some(intent);
        return state.advance(Event::TransientRetry);
    }
    state.last_observation = Some(Buf::prefix(outcome));
    if intent.write {
        if matches!(intent.tool_args, ToolArgs::TimerDelayWait { .. }) {
            intent.t0 = Some(device::parse_delay_outcome(outcome)?.0);
        }
        state.intent = Some(intent);
        state.advance(Event::ObserveOkMutating)
    } else {
        state.step = Some(StepRecord::ok(
            ToolId::new(intent.tool),
            Digest::new(intent.digest),
            ProgressDelta::NewEvidence,
        ));
        state.advance(Event::ObserveDone)
    }
}

/// `Verify`: the independent read-back every mutation must pass.
#[allow(clippy::future_not_send)]
async fn verify_intent<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    let intent = state.intent.ok_or(RuntimeError::IllegalTransition)?;
    let check = match intent.tool_args {
        ToolArgs::GpioPinWrite { pin, level } => Check::Pin {
            pin: pin.get(),
            high: level == Level::High,
        },
        ToolArgs::TimerDelayWait { ms } => Check::Clock {
            target_ms: intent
                .t0
                .ok_or(RuntimeError::IllegalTransition)?
                .saturating_add(u64::from(ms)),
        },
        _ => return Err(RuntimeError::IllegalTransition),
    };
    let request = VerifyRequest {
        seq: intent.seq,
        check,
    };
    let n = request.encode(&mut state.io)?;
    let input = state.io.get(..n).ok_or(RuntimeError::BadFrame)?;
    let verified = ctx
        .activity::<Verified>(wire::VERIFY, input)
        .await
        .map_err(|failure| failed(ctx, failure))?;
    let passed = check.passes(verified.observed);
    state.harvester.verification(intent.seq, passed);
    let (outcome, progress) = if passed {
        (StepOutcome::Ok, ProgressDelta::StateChanged)
    } else {
        (
            StepOutcome::Failed(ErrorCode::VerificationFailed),
            ProgressDelta::NoProgress,
        )
    };
    state.fold_simple(
        RecordKind::VerificationResult,
        intent.tool,
        Digest::new(intent.digest),
        outcome,
        progress,
        &[],
    );
    state.step = Some(if passed {
        StepRecord::ok(
            ToolId::new(intent.tool),
            Digest::new(intent.digest),
            ProgressDelta::StateChanged,
        )
    } else {
        StepRecord::failed(
            ToolId::new(intent.tool),
            Digest::new(intent.digest),
            ErrorCode::VerificationFailed,
            ProgressDelta::NoProgress,
        )
    });
    state.advance(if passed {
        Event::VerifyPass
    } else {
        Event::VerifyFail
    })
}

/// `Account`: the deterministic monitor over the completed step.
#[allow(clippy::future_not_send)]
async fn account<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &mut RunState,
) -> Result<(), RuntimeError> {
    let step = state.step.take().ok_or(RuntimeError::IllegalTransition)?;
    // Kept for the degraded summary; the intent is consumed here.
    let intent = state.intent.take();
    match state.monitor.observe(step, &state.budget) {
        MonitorVerdict::Continue => state.advance(Event::GuardsClear),
        MonitorVerdict::Degrade { status, reason } => {
            state.advance(Event::GuardFired)?;
            let (reason, summary) = degrade_text(state, status, reason, intent.as_ref())?;
            degrade(ctx, state, status, reason.as_bytes(), summary.as_bytes()).await
        }
    }
}

/// The monitor's degrade verdict in the golden-trace vocabulary.
fn degrade_text(
    state: &RunState,
    status: TerminalStatus,
    monitor_reason: &'static str,
    intent: Option<&Intent>,
) -> Result<(Buf<MAX_REASON>, Buf<OUTPUT_CAP>), RuntimeError> {
    let mut reason: Buf<MAX_REASON> = Buf::new();
    match status {
        TerminalStatus::BudgetExhausted => {
            reason
                .write_fmt(format_args!("{monitor_reason}_exhausted"))
                .map_err(|_| RuntimeError::BadFrame)?;
            Ok((reason, state.budget_exhausted_summary(monitor_reason)?))
        }
        TerminalStatus::Stuck if monitor_reason.contains("three identical") => {
            let summary = match intent.map(|intent| intent.tool_args) {
                Some(ToolArgs::GpioPinWrite { pin, level }) => text(format_args!(
                    "pin {} did not reach {} after 3 attempts",
                    pin.get(),
                    level.name()
                ))?,
                Some(ToolArgs::TimerDelayWait { ms }) => text(format_args!(
                    "clock did not advance by {ms}ms after 3 attempts"
                ))?,
                _ => Buf::of(b"verified mutation did not take effect after 3 attempts")
                    .ok_or(RuntimeError::BadFrame)?,
            };
            Ok((
                Buf::of(b"three_identical_verification_failures").ok_or(RuntimeError::BadFrame)?,
                summary,
            ))
        }
        _ => {
            let mut summary: Buf<OUTPUT_CAP> = Buf::new();
            for c in monitor_reason.chars() {
                let c = if c == ' ' { '_' } else { c };
                reason.write_char(c).map_err(|_| RuntimeError::BadFrame)?;
                summary.write_char(c).map_err(|_| RuntimeError::BadFrame)?;
            }
            Ok((reason, summary))
        }
    }
}

/// The normative §5.2 reason bytes for a refused call.
const fn denial_reason(args: ToolArgs) -> &'static [u8] {
    match args {
        ToolArgs::GpioPinRead { .. } => b"pin_not_in_read_capabilities",
        ToolArgs::GpioPinWrite { .. } => b"pin_not_in_write_capabilities",
        ToolArgs::SensorSampleRead { .. } => b"sensor_not_in_capabilities",
        ToolArgs::TimerUptimeRead | ToolArgs::TimerDelayWait { .. } => b"timer_not_permitted",
        ToolArgs::DeviceStatusReport { .. } => b"status_not_permitted",
    }
}

/// The denial summary naming the refused resource.
fn denial_summary(args: ToolArgs) -> Result<Buf<OUTPUT_CAP>, RuntimeError> {
    match args {
        ToolArgs::GpioPinRead { pin } => {
            text(format_args!("read of pin {} denied by policy", pin.get()))
        }
        ToolArgs::GpioPinWrite { pin, .. } => {
            text(format_args!("write to pin {} denied by policy", pin.get()))
        }
        ToolArgs::SensorSampleRead { sensor } => {
            text(format_args!("read of sensor {sensor} denied by policy"))
        }
        ToolArgs::TimerUptimeRead => text(format_args!("uptime read denied by policy")),
        ToolArgs::TimerDelayWait { .. } => text(format_args!("delay wait denied by policy")),
        ToolArgs::DeviceStatusReport { .. } => text(format_args!("status report denied by policy")),
    }
}

/// The pinned upper bound on [`RunState`]'s size (64-bit host).
#[cfg(test)]
const RUN_STATE_MAX_BYTES: usize = 12 * 1024;

#[cfg(test)]
mod tests {
    use super::*;
    use esper_core::compact::{Fact, FailedPath, MAX_FACTS, Note, PendingItem, PendingKind};
    use esper_core::decision::StatusDetail;
    use esper_core::ids::Pin;

    fn pin(n: u8) -> Pin {
        Pin::new(n).unwrap()
    }

    #[test]
    fn denial_summary_names_the_refused_resource() {
        let cases = [
            (
                ToolArgs::GpioPinRead { pin: pin(4) },
                "read of pin 4 denied by policy",
            ),
            (
                ToolArgs::GpioPinWrite {
                    pin: pin(4),
                    level: Level::High,
                },
                "write to pin 4 denied by policy",
            ),
            (
                ToolArgs::SensorSampleRead { sensor: 2 },
                "read of sensor 2 denied by policy",
            ),
            (
                ToolArgs::TimerDelayWait { ms: 250 },
                "delay wait denied by policy",
            ),
        ];
        for (args, expected) in cases {
            assert_eq!(
                denial_summary(args).unwrap().as_bytes(),
                expected.as_bytes()
            );
        }
    }

    #[test]
    fn denial_reason_is_the_normative_section_5_2_bytes() {
        assert_eq!(
            denial_reason(ToolArgs::GpioPinRead { pin: pin(4) }),
            b"pin_not_in_read_capabilities"
        );
        assert_eq!(
            denial_reason(ToolArgs::GpioPinWrite {
                pin: pin(4),
                level: Level::High
            }),
            b"pin_not_in_write_capabilities"
        );
        assert_eq!(
            denial_reason(ToolArgs::SensorSampleRead { sensor: 2 }),
            b"sensor_not_in_capabilities"
        );
        assert_eq!(
            denial_reason(ToolArgs::TimerUptimeRead),
            b"timer_not_permitted"
        );
        assert_eq!(
            denial_reason(ToolArgs::DeviceStatusReport {
                detail: StatusDetail::Summary
            }),
            b"status_not_permitted"
        );
    }

    #[test]
    fn text_fails_closed_instead_of_truncating() {
        let mut small: Buf<4> = Buf::new();
        assert!(small.write_str("abcd").is_ok());
        assert!(small.write_str("e").is_err());
        assert_eq!(small.as_bytes(), b"abcd");
        assert_eq!(Buf::<3>::prefix(b"abcdef").as_bytes(), b"abc");
        assert!(Buf::<3>::of(b"abcd").is_none());
    }

    /// The rollover snapshot of a compact state with every list full
    /// fits [`MAX_SNAPSHOT_BYTES`], so it always stores in six chunks.
    #[test]
    fn a_full_fold_fits_the_snapshot_bound() {
        let seed = RunSeed::default_slice();
        let mut state =
            CompactState::new(Digest::new(1), seed.starting_budget(), version_set(&seed));
        let note = Note::truncated_from(&[b'n'; 200]);
        for i in 0..32u32 {
            state.record_completed(note);
            state.record_open(note);
            state.record_decision(Digest::new(u64::from(i)));
            state.record_fact(Fact {
                text: Note::truncated_from(&[u8::try_from(i).unwrap(); 200]),
                source_seq: i,
            });
            state.record_fingerprint(u64::from(i));
        }
        for i in 0..16u8 {
            state
                .record_failed_path(FailedPath {
                    tool: ToolId::new(1),
                    args_digest: Digest::new(u64::from(i)),
                    error: ErrorCode::VerificationFailed,
                })
                .unwrap();
        }
        for i in 0..8u32 {
            state
                .record_pending(PendingItem {
                    kind: PendingKind::Approval,
                    seq: i,
                    digest: Digest::new(u64::from(i)),
                })
                .unwrap();
        }
        assert_eq!(state.facts().len(), MAX_FACTS);
        let len = esper_core::snapshot::encoded_len(&state);
        assert!(len <= MAX_SNAPSHOT_BYTES, "{len} > {MAX_SNAPSHOT_BYTES}");
    }

    /// The caller-owned state is bounded and measured: 64-bit host
    /// figure, dominated by the snapshot buffer (2,688 B), the parent
    /// fold and this segment's fold (two compact states).
    #[test]
    fn run_state_size_is_pinned() {
        let size = core::mem::size_of::<RunState>();
        eprintln!("RunState: {size} bytes");
        assert!(size <= RUN_STATE_MAX_BYTES, "RunState grew to {size} bytes");
    }
}
