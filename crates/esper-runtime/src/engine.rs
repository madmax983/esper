//! The durable `ReAct` engine: one run, many boots.
//!
//! The driver walks `esper-core`'s normative state machine
//! ([`State`]) via [`transition`]: every edge the engine takes is
//! checked against the table, so an illegal control-flow step is an
//! engine bug that fails closed instead of a silent divergence.
//!
//! Durability comes from the [`Journal`]: every boundary crossing
//! commits a frame *before* the world is touched, and every effect
//! boundary also advances Waymaker's [`ReplayCursor`] over a record
//! derived from the frame. A simulated crash drops all in-memory state
//! (budgets, monitor, allocator, cursor, machine position); the next
//! boot rebuilds it by replaying the journal, and the run resumes from
//! the committed prefix. Injected crashes are transparent to the
//! caller: [`drive_run`] reboots internally and returns the final
//! trace. Only durable suspension ([`State::AwaitInput`] with no input
//! queued) returns early, so the caller can resume the same run — with
//! the same journal — on a later boot.
//!
//! // HOST-ONLY (E0/E1): the engine is heap-allocated and synchronous.
//! [`drive_run_async`] runs it on a Tokio worker via `block_in_place`.
//! The firmware port (E2) keeps the boundary vocabulary and replaces
//! the doubles.

use esper_core::ResourceBudget;
use esper_core::decision::{Decision, Level, ToolArgs, decode_line};
use esper_core::error::{ErrorCode, RepairVariant};
use esper_core::ids::{Digest, ToolId};
use esper_core::monitor::{Monitor, MonitorVerdict, ProgressDelta, StepRecord};
use esper_core::registry::{authorize, lookup_by_id};
use esper_core::state::{Event, State, TerminalStatus, transition};
use esper_protocol::PermissionClass;
use waymaker_core::{
    ActivityKind, EffectIdAllocator, EffectSeq, RecordRef, ReplayCursor, RunId as WaymakerRunId,
};

use crate::error::{CrashPoint, Halt, RuntimeError};
use crate::journal::{DecisionClass, Frame, Journal};
use crate::seed::{ESPER_WORKFLOW_KIND, RunSeed};
use crate::trace::RunTrace;
use crate::world::{FakeDevice, FaultPlan, InputPlan, ScriptedModel, WorldError};

/// Boots before the driver gives up instead of rebooting forever.
///
/// Each injected crash disarms itself, so a run needs at most twelve
/// boots (eleven crash points plus the final pass); the cap is
/// defense in depth against an engine livelock.
const MAX_BOOTS: u32 = 32;

/// Invalid model lines before the run ends `ModelInvalid` (SPEC §4.4).
const MAX_REPAIRS: u8 = 3;

/// Dispatch attempts under one effect id: the first try plus two
/// retries (SPEC §6: transient faults are retryable at most twice).
const MAX_TRANSIENT_ATTEMPTS: u32 = 3;

/// A decoded model line, with every borrowed piece copied out.
///
/// `decode_line` borrows the line; the engine commits owned copies so
/// the journal never borrows the model's buffer.
enum Classified {
    /// A tool call.
    Call {
        /// The tool's numeric id.
        tool: u8,
        /// The validated argument bytes.
        // HOST-ONLY (E0/E1)
        args: Vec<u8>,
        /// The decode-time digest of `args`.
        digest: u64,
        /// The typed dispatch shape, bound from the validated bytes.
        tool_args: ToolArgs,
    },
    /// A human-input request.
    Ask {
        /// The prompt bytes.
        // HOST-ONLY (E0/E1)
        prompt: Vec<u8>,
        /// The response schema id.
        schema: u8,
    },
    /// A terminal answer.
    Finish {
        /// The run summary.
        // HOST-ONLY (E0/E1)
        summary: Vec<u8>,
    },
    /// A decoder rejection, with its repair class.
    Invalid(RepairVariant),
}

/// A committed call waiting for authorization.
#[derive(Clone)]
struct PendingCall {
    /// The tool's numeric id.
    tool: u8,
    /// The validated argument bytes.
    // HOST-ONLY (E0/E1)
    args: Vec<u8>,
    /// The decode-time digest of `args`.
    digest: u64,
    /// The typed dispatch shape, bound from the validated bytes.
    tool_args: ToolArgs,
}

/// A committed intent with no terminal outcome yet.
#[derive(Clone)]
struct PendingIntent {
    /// The stable effect identity.
    seq: EffectSeq,
    /// The tool's numeric id.
    tool: u8,
    /// The typed dispatch shape, re-derived on replay via
    /// `ToolArgs::bind` over the committed argument bytes.
    tool_args: ToolArgs,
    /// The decode-time digest of the argument bytes.
    digest: u64,
    /// Whether the tool mutates the world: consumes one `mutations`
    /// unit and routes through `Verify`. Kept in the frame (not
    /// re-derived from the permission class) because the mutation was
    /// accounted at commit time; replay must reproduce the accounting,
    /// not recompute it.
    write: bool,
    /// Attempts so far under this effect id.
    attempt: u32,
    /// The committed observation bytes, when the outcome committed.
    /// The delay verifier derives the pre-dispatch clock reading from
    /// them.
    // HOST-ONLY (E0/E1)
    outcome: Option<Vec<u8>>,
}

/// A committed ask with no approval yet.
#[derive(Clone)]
struct PendingAsk {
    /// The prompt bytes.
    // HOST-ONLY (E0/E1)
    prompt: Vec<u8>,
    /// The response schema id.
    schema: u8,
}

/// Where the normative machine stands after replaying the journal.
enum ResumePoint {
    /// Fresh run, or returning through the budget gate.
    Gather,
    /// An invalid line committed; the repair budget decides.
    Repair,
    /// A call committed; authorize it.
    Authorize,
    /// An intent committed; dispatch or redeliver it.
    Observe,
    /// A write outcome committed; verify it.
    Verify,
    /// A step completed; account it with the monitor.
    Account,
    /// An ask committed; await (or consume) the input.
    AwaitInput,
    /// A finish committed; commit the terminal result.
    Finalize,
    /// The terminal result committed; the run is over.
    End,
}

/// Drive one run to its terminal result (or to durable suspension).
///
/// The seed is validated, then the engine boots: it replays the
/// journal into fresh budgets, monitor, effect allocator, Waymaker
/// cursor, and state-machine position, and drives forward. An injected
/// crash (the `crash` point) drops all in-memory state and reboots from
/// the committed prefix — transparently, inside this call — with the
/// crash point disarmed so it fires at most once. When the run
/// suspends awaiting human input and none is queued, the suspension is
/// durable: this returns a trace with [`RunTrace::suspended`] set, and
/// a later call with the same journal and a queued input resumes the
/// same run.
///
/// # Errors
///
/// Returns [`RuntimeError`] on a malformed seed, a corrupt or
/// foreign journal, a world-double failure, or an engine bug. An
/// injected crash never surfaces here: it reboots internally.
pub fn drive_run(
    seed: &RunSeed,
    journal: &mut Journal,
    model: &mut ScriptedModel,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    crash: Option<CrashPoint>,
) -> Result<RunTrace, RuntimeError> {
    // HOST-ONLY (E0/E1)
    seed.validate().map_err(RuntimeError::SeedInvalid)?;
    let run = WaymakerRunId(seed.id.get());
    let mut driver = Driver {
        seed,
        journal,
        model,
        device,
        faults,
        inputs,
        crash,
        sm: State::Recover,
        budget: seed.starting_budget(),
        monitor: Monitor::new(),
        allocator: EffectIdAllocator::for_run(run),
        cursor: ReplayCursor::new(run),
        seed_input: [0u8; 55],
        repair_used: 0,
        turns_used: 0,
        mutations_used: 0,
        pending_call: None,
        pending_intent: None,
        pending_step: None,
        pending_ask: None,
        pending_finish: None,
        ask_request_committed: false,
    };
    let mut boots = 0u32;
    loop {
        boots += 1;
        if boots > MAX_BOOTS {
            return Err(RuntimeError::TooManyBoots);
        }
        // Reboot never fires crash hooks, so a `Crash` halt is an
        // engine bug.
        driver.reboot().map_err(|halt| match halt {
            Halt::Crash(_) => RuntimeError::IllegalTransition,
            Halt::Error(error) => error,
        })?;
        match driver.main_loop() {
            Ok(trace) => return Ok(trace),
            Err(Halt::Error(error)) => return Err(error),
            Err(Halt::Crash(_)) => {}
        }
    }
}

/// Drive one run asynchronously on the Tokio host.
///
/// // HOST-ONLY (E0/E1): the engine itself is synchronous; this entry
/// point runs it via `tokio::task::block_in_place` so the Tokio
/// reactor never stalls. Requires a multi-threaded Tokio runtime.
///
/// # Errors
///
/// Same as [`drive_run`].
#[allow(clippy::unused_async)]
pub async fn drive_run_async(
    seed: &RunSeed,
    journal: &mut Journal,
    model: &mut ScriptedModel,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    crash: Option<CrashPoint>,
) -> Result<RunTrace, RuntimeError> {
    // HOST-ONLY (E0/E1)
    tokio::task::block_in_place(|| drive_run(seed, journal, model, device, faults, inputs, crash))
}

/// The per-boot engine state: everything a crash drops.
struct Driver<'a> {
    /// The validated run seed.
    seed: &'a RunSeed,
    /// The caller-owned journal (survives crashes).
    journal: &'a mut Journal,
    /// The model backend.
    model: &'a mut ScriptedModel,
    /// The device (survives crashes).
    device: &'a mut FakeDevice,
    /// The fault plan (survives crashes).
    faults: &'a mut FaultPlan,
    /// The input plan (survives crashes).
    inputs: &'a mut InputPlan,
    /// The still-armed crash point, if any.
    crash: Option<CrashPoint>,
    /// The normative state machine position.
    sm: State,
    /// The remaining budget, rebuilt from the journal per boot.
    budget: ResourceBudget,
    /// The deterministic monitor, re-observed per boot.
    monitor: Monitor,
    /// The effect-id allocator, replayed per boot.
    allocator: EffectIdAllocator,
    /// The Waymaker replay cursor over the derived records.
    cursor: ReplayCursor,
    /// The canonical seed bytes for the `RunStarted` record.
    // HOST-ONLY (E0/E1)
    seed_input: [u8; 55],
    /// Invalid lines committed this run.
    repair_used: u8,
    /// Model turns consumed this run.
    turns_used: u32,
    /// Mutations consumed this run.
    mutations_used: u32,
    /// A committed call awaiting authorization.
    pending_call: Option<PendingCall>,
    /// A committed intent awaiting its terminal outcome.
    pending_intent: Option<PendingIntent>,
    /// A completed step awaiting monitor accounting.
    pending_step: Option<StepRecord>,
    /// A committed ask awaiting approval.
    pending_ask: Option<PendingAsk>,
    /// A committed finish awaiting its terminal commit.
    // HOST-ONLY (E0/E1)
    pending_finish: Option<Vec<u8>>,
    /// Whether the approval request already committed.
    ask_request_committed: bool,
}

impl Driver<'_> {
    /// Rebuild all per-boot state from the journal.
    ///
    /// Binds a fresh journal to the seed, validates a used journal's
    /// binding (version, then full seed equality), replays every frame
    /// — advancing the Waymaker cursor over each derived record and
    /// re-observing every accounted step into the monitor — and lands
    /// the normative machine on the resume point.
    ///
    /// Never fires a crash hook; a crash halt here would be an engine
    /// bug.
    fn reboot(&mut self) -> Result<(), Halt> {
        self.budget = self.seed.starting_budget();
        self.monitor = Monitor::new();
        let run = WaymakerRunId(self.seed.id.get());
        self.allocator = EffectIdAllocator::for_run(run);
        self.cursor = ReplayCursor::new(run);
        self.repair_used = 0;
        self.turns_used = 0;
        self.mutations_used = 0;
        self.pending_call = None;
        self.pending_intent = None;
        self.pending_step = None;
        self.pending_ask = None;
        self.pending_finish = None;
        self.ask_request_committed = false;
        self.sm = State::Recover;

        // HOST-ONLY (E0/E1): the journal is cloned once per boot so
        // replay can mutate driver state while reading frames.
        let frames: Vec<Frame> = self.journal.frames().to_vec();
        let snapshot = match frames.first() {
            Some(Frame::RunStarted { seed }) => *seed,
            Some(_) | None => {
                if self.journal.is_empty() {
                    let snapshot = self.seed.snapshot();
                    self.journal.push(Frame::RunStarted { seed: snapshot });
                    snapshot
                } else {
                    return Err(RuntimeError::JournalCorrupt.into());
                }
            }
        };
        if snapshot.workflow_version != self.seed.workflow_version {
            return Err(RuntimeError::IncompatibleVersion.into());
        }
        if snapshot != self.seed.snapshot() {
            return Err(RuntimeError::SeedMismatch.into());
        }
        self.seed_input = snapshot.input_bytes();
        let seed_input = self.seed_input;
        self.advance_cursor(RecordRef::RunStarted {
            workflow_kind: ESPER_WORKFLOW_KIND,
            workflow_version: snapshot.workflow_version,
            input: &seed_input,
        })?;
        self.advance(Event::JournalValid)?;

        // Replay the committed prefix. Steps are collected in order;
        // every step but a trailing unaccounted one is re-observed
        // into the monitor, exactly as the original boots observed
        // them. (A step is unaccounted only when the journal ends right
        // after its completion frame — the crash landed between the
        // commit and `Account` — and the forward pass will account it.)
        // HOST-ONLY (E0/E1)
        let mut steps: Vec<StepRecord> = Vec::new();
        let mut resume = ResumePoint::Gather;
        let rest = if frames.is_empty() { &[] } else { &frames[1..] };
        for frame in rest {
            resume = self.replay_frame(frame, &mut steps)?;
        }
        let pending = matches!(resume, ResumePoint::Account);
        let accounted = if pending {
            steps.len().saturating_sub(1)
        } else {
            steps.len()
        };
        for step in steps.iter().take(accounted) {
            let _ = self.monitor.observe(*step, &self.budget);
        }
        if pending {
            self.pending_step = steps.last().copied();
        }
        self.sm = match resume {
            ResumePoint::Gather => State::Gather,
            ResumePoint::Repair => State::Repair,
            ResumePoint::Authorize => State::Authorize,
            ResumePoint::Observe => State::Observe,
            ResumePoint::Verify => State::Verify,
            ResumePoint::Account => State::Account,
            ResumePoint::AwaitInput => State::AwaitInput,
            ResumePoint::Finalize => State::Finalize,
            ResumePoint::End => State::End,
        };
        Ok(())
    }

    /// Replay one committed frame into the per-boot state.
    ///
    /// Returns the resume point the frame leaves the run at. Any frame
    /// that cannot legally follow its prefix fails closed with
    /// [`RuntimeError::JournalCorrupt`] (structural) or
    /// [`RuntimeError::ReplayDiverged`] (effect-history mismatch).
    fn replay_frame(
        &mut self,
        frame: &Frame,
        steps: &mut Vec<StepRecord>,
    ) -> Result<ResumePoint, RuntimeError> {
        match frame {
            Frame::RunStarted { .. } => Err(RuntimeError::JournalCorrupt),
            Frame::ModelDecision {
                output,
                class,
                repair_index,
            } => self.replay_decision(output, *class, *repair_index),
            Frame::ToolIntent {
                seq,
                tool,
                args,
                digest,
                write,
            } => self.replay_intent(*seq, *tool, args, *digest, *write),
            Frame::ToolObservation {
                seq,
                attempt,
                transient,
                outcome,
            } => self.replay_observation(*seq, *attempt, *transient, outcome, steps),
            Frame::Verification {
                seq,
                expected: _,
                observed: _,
                passed,
            } => self.replay_verification(*seq, *passed, steps),
            Frame::ApprovalRequest { prompt, schema } => {
                // HOST-ONLY (E0/E1)
                self.pending_ask = Some(PendingAsk {
                    prompt: prompt.clone(),
                    schema: *schema,
                });
                self.ask_request_committed = true;
                Ok(ResumePoint::AwaitInput)
            }
            Frame::ApprovalDecision { input } => {
                steps.push(StepRecord::ok(
                    ToolId::new(0),
                    Digest::of_bytes(input),
                    ProgressDelta::InputReceived,
                ));
                self.pending_ask = None;
                self.ask_request_committed = false;
                Ok(ResumePoint::Account)
            }
            Frame::Terminal {
                status,
                reason,
                summary,
            } => self.replay_terminal(*status, reason, summary),
        }
    }

    /// Replay a committed model decision: re-spend the turn, restore
    /// the decoded classification, and check the journal's class tag.
    fn replay_decision(
        &mut self,
        output: &[u8],
        class: DecisionClass,
        repair_index: u8,
    ) -> Result<ResumePoint, RuntimeError> {
        self.budget
            .consume_turn()
            .map_err(|_| RuntimeError::JournalCorrupt)?;
        self.turns_used += 1;
        match Self::classify_output(output) {
            Classified::Call {
                tool,
                args,
                digest,
                tool_args,
            } => {
                if class != DecisionClass::Call {
                    return Err(RuntimeError::JournalCorrupt);
                }
                self.pending_call = Some(PendingCall {
                    tool,
                    args,
                    digest,
                    tool_args,
                });
                Ok(ResumePoint::Authorize)
            }
            Classified::Ask { prompt, schema } => {
                if class != DecisionClass::Ask {
                    return Err(RuntimeError::JournalCorrupt);
                }
                self.pending_ask = Some(PendingAsk { prompt, schema });
                self.ask_request_committed = false;
                Ok(ResumePoint::AwaitInput)
            }
            Classified::Finish { summary } => {
                if class != DecisionClass::Finish {
                    return Err(RuntimeError::JournalCorrupt);
                }
                self.pending_finish = Some(summary);
                Ok(ResumePoint::Finalize)
            }
            Classified::Invalid(variant) => {
                let expected = match variant {
                    RepairVariant::Malformed => DecisionClass::Malformed,
                    RepairVariant::InvalidArgs => DecisionClass::InvalidArgs,
                };
                if class != expected {
                    return Err(RuntimeError::JournalCorrupt);
                }
                self.repair_used += 1;
                if self.repair_used != repair_index {
                    return Err(RuntimeError::JournalCorrupt);
                }
                Ok(ResumePoint::Repair)
            }
        }
    }

    /// Replay a committed intent: the allocator must mint the same
    /// effect id, and the cursor replays the schedule record. The
    /// typed dispatch shape is re-derived deterministically from the
    /// committed argument bytes via `ToolArgs::bind`: the bytes were
    /// validated at commit, so bind cannot fail on a consistent
    /// journal.
    fn replay_intent(
        &mut self,
        seq: u32,
        tool: u8,
        args: &[u8],
        digest: u64,
        write: bool,
    ) -> Result<ResumePoint, RuntimeError> {
        let effect = self.allocator.allocate().map_err(RuntimeError::Waymaker)?;
        if effect.seq != EffectSeq(seq) {
            return Err(RuntimeError::ReplayDiverged);
        }
        if write {
            self.budget
                .consume_mutation()
                .map_err(|_| RuntimeError::JournalCorrupt)?;
            self.mutations_used += 1;
        }
        self.advance_cursor_replay(RecordRef::EffectScheduled {
            seq: EffectSeq(seq),
            kind: ActivityKind(u16::from(tool)),
            input_len: u16::try_from(args.len()).map_err(|_| RuntimeError::JournalCorrupt)?,
            input_crc: fnv1a32(args),
        })?;
        // Re-derive the typed dispatch shape from the committed bytes.
        let entry = lookup_by_id(ToolId::new(tool)).ok_or(RuntimeError::JournalCorrupt)?;
        let bound =
            esper_protocol::validate(entry, args).map_err(|_| RuntimeError::JournalCorrupt)?;
        let tool_args =
            ToolArgs::bind(ToolId::new(tool), &bound).map_err(|_| RuntimeError::JournalCorrupt)?;
        self.pending_call = None;
        self.pending_intent = Some(PendingIntent {
            seq: EffectSeq(seq),
            tool,
            tool_args,
            digest,
            write,
            attempt: 0,
            outcome: None,
        });
        Ok(ResumePoint::Observe)
    }

    /// Replay a committed observation: a transient attempt is a
    /// sub-effect retry (no cursor record); a terminal outcome
    /// resolves the effect in the cursor.
    fn replay_observation(
        &mut self,
        seq: u32,
        attempt: u32,
        transient: bool,
        outcome: &[u8],
        steps: &mut Vec<StepRecord>,
    ) -> Result<ResumePoint, RuntimeError> {
        let (is_write, tool, digest) = {
            let intent = self
                .pending_intent
                .as_mut()
                .ok_or(RuntimeError::JournalCorrupt)?;
            if intent.seq != EffectSeq(seq) {
                return Err(RuntimeError::ReplayDiverged);
            }
            intent.attempt = attempt;
            if !transient {
                // Stash the committed outcome: the delay verifier
                // derives the pre-dispatch clock reading from it.
                // HOST-ONLY (E0/E1)
                intent.outcome = Some(outcome.to_vec());
            }
            (intent.write, intent.tool, intent.digest)
        };
        if transient {
            // A transient attempt is a sub-effect retry, not an
            // effect boundary: it commits in the journal (so the
            // attempt count survives a crash) but produces no
            // Waymaker record. Only the terminal outcome of the
            // effect is a boundary the cursor replays.
            Ok(ResumePoint::Observe)
        } else {
            self.advance_cursor_replay(RecordRef::EffectCompleted {
                seq: EffectSeq(seq),
                result: outcome,
            })?;
            if is_write {
                Ok(ResumePoint::Verify)
            } else {
                steps.push(StepRecord::ok(
                    ToolId::new(tool),
                    Digest::new(digest),
                    ProgressDelta::NewEvidence,
                ));
                self.pending_intent = None;
                Ok(ResumePoint::Account)
            }
        }
    }

    /// Replay a committed verification: rebuild the accounted step.
    fn replay_verification(
        &self,
        seq: u32,
        passed: bool,
        steps: &mut Vec<StepRecord>,
    ) -> Result<ResumePoint, RuntimeError> {
        let intent = self
            .pending_intent
            .as_ref()
            .ok_or(RuntimeError::JournalCorrupt)?;
        if intent.seq != EffectSeq(seq) {
            return Err(RuntimeError::ReplayDiverged);
        }
        let step = if passed {
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
        };
        steps.push(step);
        Ok(ResumePoint::Account)
    }

    /// Replay the terminal frame: the cursor records the run outcome.
    fn replay_terminal(
        &mut self,
        status: TerminalStatus,
        reason: &[u8],
        summary: &[u8],
    ) -> Result<ResumePoint, RuntimeError> {
        let record = match status {
            TerminalStatus::Completed => RecordRef::RunCompleted { result: summary },
            _ => RecordRef::RunFailed { error: reason },
        };
        self.advance_cursor_replay(record)?;
        Ok(ResumePoint::End)
    }
    /// The forward pass: walk the normative machine to a terminal
    /// result or durable suspension.
    fn main_loop(&mut self) -> Result<RunTrace, Halt> {
        loop {
            match self.sm {
                State::Gather => self.do_gather()?,
                State::Infer => self.do_infer()?,
                State::Repair => self.do_repair()?,
                State::Authorize => self.do_authorize()?,
                State::AwaitInput => {
                    if self.do_await_input()? {
                        return RunTrace::from_journal(self.journal, self.seed)
                            .map_err(Halt::Error);
                    }
                }
                State::Observe => self.do_observe()?,
                State::Verify => self.do_verify()?,
                State::Account => self.do_account()?,
                State::Finalize => self.do_finalize()?,
                State::End => {
                    return RunTrace::from_journal(self.journal, self.seed).map_err(Halt::Error);
                }
                State::Recover | State::Degraded | State::SafeStop => {
                    return Err(RuntimeError::IllegalTransition.into());
                }
            }
        }
    }

    /// `Gather`: the budget gate every step passes through.
    fn do_gather(&mut self) -> Result<(), Halt> {
        if let Some(unit) = self.budget.exhausted_unit() {
            self.advance(Event::BudgetExhausted)?;
            let name = unit.name();
            // HOST-ONLY (E0/E1)
            let reason = format!("{name}_exhausted").into_bytes();
            let summary = self.budget_exhausted_summary(name).into_bytes();
            self.enter_degraded(TerminalStatus::BudgetExhausted, &reason, &summary)?;
        } else {
            self.advance(Event::BudgetsClear)?;
        }
        Ok(())
    }

    /// `Infer`: take one model line and commit the decision.
    ///
    /// The pre-commit region (model emission through the decision
    /// commit) unemits the line on a crash, so the next boot sees it
    /// again: an uncommitted decision is never lost and never applied.
    fn do_infer(&mut self) -> Result<(), Halt> {
        let line = self
            .model
            .next_line()
            .ok_or(RuntimeError::World("model script exhausted"))?;
        let outcome = self.infer_pre_commit(&line);
        if matches!(outcome, Err(Halt::Crash(_))) {
            self.model.unemit();
        }
        outcome
    }

    /// The pre-commit half of `Infer`.
    fn infer_pre_commit(&mut self, line: &[u8]) -> Result<(), Halt> {
        self.fire(CrashPoint::ModelIntent)?;
        let classified = Self::classify_output(line);
        self.fire(CrashPoint::BeforeDecisionCommit)?;
        // Unreachable in a consistent run: every path into `Infer`
        // passes a budget gate (`Gather`, or `Repair` which checks
        // below). A failure here means the journal over-spent.
        self.budget
            .consume_turn()
            .map_err(|_| RuntimeError::JournalCorrupt)?;
        self.turns_used += 1;
        match classified {
            Classified::Call {
                tool,
                args,
                digest,
                tool_args,
            } => {
                // HOST-ONLY (E0/E1)
                self.journal.push(Frame::ModelDecision {
                    output: line.to_vec(),
                    class: DecisionClass::Call,
                    repair_index: 0,
                });
                self.pending_call = Some(PendingCall {
                    tool,
                    args,
                    digest,
                    tool_args,
                });
                self.advance(Event::DecodeCall)?;
            }
            Classified::Ask { prompt, schema } => {
                self.journal.push(Frame::ModelDecision {
                    output: line.to_vec(),
                    class: DecisionClass::Ask,
                    repair_index: 0,
                });
                self.pending_ask = Some(PendingAsk { prompt, schema });
                self.ask_request_committed = false;
                self.advance(Event::DecodeAsk)?;
            }
            Classified::Finish { summary } => {
                self.journal.push(Frame::ModelDecision {
                    output: line.to_vec(),
                    class: DecisionClass::Finish,
                    repair_index: 0,
                });
                self.pending_finish = Some(summary);
                self.advance(Event::DecodeFinish)?;
            }
            Classified::Invalid(variant) => {
                let class = match variant {
                    RepairVariant::Malformed => DecisionClass::Malformed,
                    RepairVariant::InvalidArgs => DecisionClass::InvalidArgs,
                };
                self.repair_used += 1;
                self.journal.push(Frame::ModelDecision {
                    output: line.to_vec(),
                    class,
                    repair_index: self.repair_used,
                });
                self.advance(Event::DecodeMalformed)?;
            }
        }
        Ok(())
    }

    /// `Repair`: grant another turn while repair allowance and model
    /// turns remain.
    fn do_repair(&mut self) -> Result<(), Halt> {
        // A repair spends a model turn; with none left the repair
        // budget is exhausted even if repairs remain nominally.
        if self.repair_used >= MAX_REPAIRS || self.budget.model_turns == 0 {
            self.advance(Event::RepairExhausted)?;
            if self.budget.model_turns == 0 {
                let reason = b"model_turns_exhausted".to_vec();
                let summary = self.budget_exhausted_summary("model_turns").into_bytes();
                self.enter_degraded(TerminalStatus::BudgetExhausted, &reason, &summary)?;
            } else {
                let reason = b"repair_budget_exhausted".to_vec();
                // HOST-ONLY (E0/E1)
                let summary = format!(
                    "model output invalid after {} repair turns",
                    self.repair_used.saturating_sub(1)
                )
                .into_bytes();
                self.enter_degraded(TerminalStatus::ModelInvalid, &reason, &summary)?;
            }
        } else {
            self.advance(Event::RepairAvailable)?;
        }
        Ok(())
    }

    /// `Authorize`: capability check, then device business rules, then
    /// the durable intent commit. Denials never touch hardware.
    ///
    /// The normative §17.5 gate runs in order: the allowlist is
    /// implicit (the tool name resolved against the static catalog at
    /// decode), then `authorize` checks the seed's capability set,
    /// then the device's business rules run. Only a call that clears
    /// all three is dispatched.
    fn do_authorize(&mut self) -> Result<(), Halt> {
        let call = self
            .pending_call
            .take()
            .ok_or(RuntimeError::IllegalTransition)?;
        self.fire(CrashPoint::BeforeAuthorize)?;
        let entry = lookup_by_id(ToolId::new(call.tool)).ok_or(RuntimeError::IllegalTransition)?;
        let is_write = matches!(entry.permission, PermissionClass::IdempotentWrite);
        if let Err(error) = authorize(&self.seed.capabilities, entry, &call.tool_args) {
            // The gate's only refusal is `PermissionDenied`; any other
            // error is an engine bug, failed closed.
            if !matches!(error, esper_core::error::Error::PermissionDenied { .. }) {
                return Err(RuntimeError::IllegalTransition.into());
            }
            self.advance(Event::Denied)?;
            let reason = denial_reason(call.tool_args);
            // HOST-ONLY (E0/E1)
            let summary = denial_summary(call.tool_args).into_bytes();
            return self.enter_degraded(TerminalStatus::Denied, reason, &summary);
        }
        // Device business rules (SPEC §5.2 step 3): only `gpio_pin_write`
        // carries one in E2 — the pin must be output-capable. The other
        // tools have no device rule.
        if let ToolArgs::GpioPinWrite { pin, .. } = call.tool_args
            && self.device.direction(pin) == crate::world::Direction::Input
        {
            // HOST-ONLY (E0/E1)
            let summary =
                format!("write to pin {} denied by device direction", pin.get()).into_bytes();
            self.advance(Event::Denied)?;
            return self.enter_degraded(TerminalStatus::Denied, b"pin_direction_denied", &summary);
        }
        if is_write && self.budget.mutations == 0 {
            self.advance(Event::Denied)?;
            return self.enter_degraded(
                TerminalStatus::BudgetExhausted,
                b"mutations_exhausted",
                b"mutation budget exhausted",
            );
        }
        let effect = self.allocator.allocate().map_err(RuntimeError::Waymaker)?;
        let seq = effect.seq;
        self.fire(CrashPoint::ToolIntent)?;
        if is_write {
            self.budget
                .consume_mutation()
                .map_err(|_| RuntimeError::JournalCorrupt)?;
            self.mutations_used += 1;
        }
        // HOST-ONLY (E0/E1)
        let args = call.args.clone();
        self.journal.push(Frame::ToolIntent {
            seq: seq.0,
            tool: call.tool,
            args: args.clone(),
            digest: call.digest,
            write: is_write,
        });
        self.advance_cursor(RecordRef::EffectScheduled {
            seq,
            kind: ActivityKind(u16::from(call.tool)),
            input_len: u16::try_from(args.len()).map_err(|_| RuntimeError::BadFrame)?,
            input_crc: fnv1a32(&args),
        })?;
        // The committed intent must be the cursor's unresolved effect:
        // identity stays stable from commit through redelivery.
        match self.cursor.pending() {
            Some(pending) if pending.id.seq == seq => {}
            _ => return Err(RuntimeError::ReplayDiverged.into()),
        }
        self.pending_intent = Some(PendingIntent {
            seq,
            tool: call.tool,
            tool_args: call.tool_args,
            digest: call.digest,
            write: is_write,
            attempt: 0,
            outcome: None,
        });
        self.advance(Event::Authorized)?;
        Ok(())
    }

    /// `AwaitInput`: commit the approval request, then either consume a
    /// queued input or suspend durably.
    ///
    /// Returns true when the run suspended (no input queued).
    fn do_await_input(&mut self) -> Result<bool, Halt> {
        let ask = self
            .pending_ask
            .clone()
            .ok_or(RuntimeError::IllegalTransition)?;
        if !self.ask_request_committed {
            // HOST-ONLY (E0/E1)
            self.journal.push(Frame::ApprovalRequest {
                prompt: ask.prompt.clone(),
                schema: ask.schema,
            });
            self.ask_request_committed = true;
        }
        self.fire(CrashPoint::AwaitInput)?;
        match self.inputs.take() {
            Some(input) => {
                validate_ask_input(&input, ask.schema).map_err(RuntimeError::World)?;
                // HOST-ONLY (E0/E1)
                let digest_input = input.clone();
                self.journal.push(Frame::ApprovalDecision { input });
                self.pending_step = Some(StepRecord::ok(
                    ToolId::new(0),
                    Digest::of_bytes(&digest_input),
                    ProgressDelta::InputReceived,
                ));
                self.pending_ask = None;
                self.ask_request_committed = false;
                self.advance(Event::InputArrived)?;
                Ok(false)
            }
            None => Ok(true),
        }
    }

    /// `Observe`: dispatch the committed intent (or redeliver it).
    ///
    /// The transient-fault plan is consulted before the device, so a
    /// hiccup never reaches hardware. Redelivery under a committed
    /// effect id deduplicates in the device: at-least-once dispatch
    /// without double execution.
    fn do_observe(&mut self) -> Result<(), Halt> {
        let mut intent = self
            .pending_intent
            .take()
            .ok_or(RuntimeError::IllegalTransition)?;
        intent.attempt += 1;
        if self
            .faults
            .consume(intent.tool, dispatch_resource(intent.tool_args))
        {
            if intent.attempt >= MAX_TRANSIENT_ATTEMPTS {
                self.advance(Event::TransientExhausted)?;
                // HOST-ONLY (E0/E1)
                let summary = format!(
                    "transient faults exhausted after {} attempts",
                    intent.attempt
                )
                .into_bytes();
                return self.enter_degraded(
                    TerminalStatus::Stuck,
                    b"transient_attempts_exhausted",
                    &summary,
                );
            }
            self.fire(CrashPoint::AfterPhysicalBeforeObservation)?;
            self.journal.push(Frame::ToolObservation {
                seq: intent.seq.0,
                attempt: intent.attempt,
                transient: true,
                // HOST-ONLY (E0/E1)
                outcome: Vec::new(),
            });
            // No Waymaker record: the transient attempt is not an
            // effect boundary; the cursor sees only the scheduled
            // effect and its terminal outcome.
            self.pending_intent = Some(intent);
            self.advance(Event::TransientRetry)?;
            return Ok(());
        }
        // HOST-ONLY (E0/E1)
        let outcome = self.dispatch_intent(&intent).map_err(map_world)?;
        self.fire(CrashPoint::AfterPhysicalBeforeObservation)?;
        let record_outcome = outcome.clone();
        self.journal.push(Frame::ToolObservation {
            seq: intent.seq.0,
            attempt: intent.attempt,
            transient: false,
            outcome,
        });
        self.advance_cursor(RecordRef::EffectCompleted {
            seq: intent.seq,
            result: &record_outcome,
        })?;
        if intent.write {
            // Stash the committed outcome: the delay verifier derives
            // the pre-dispatch clock reading from it.
            intent.outcome = Some(record_outcome);
            self.pending_intent = Some(intent);
            self.advance(Event::ObserveOkMutating)?;
        } else {
            self.pending_step = Some(StepRecord::ok(
                ToolId::new(intent.tool),
                Digest::new(intent.digest),
                ProgressDelta::NewEvidence,
            ));
            self.advance(Event::ObserveDone)?;
        }
        Ok(())
    }

    /// Dispatch the committed intent against the fake device, routing
    /// by the typed arguments (SPEC §17.6).
    fn dispatch_intent(&mut self, intent: &PendingIntent) -> Result<Vec<u8>, WorldError> {
        let seq = intent.seq;
        let digest = intent.digest;
        match intent.tool_args {
            ToolArgs::GpioPinRead { pin } => self.device.dispatch_read(pin, seq, digest),
            ToolArgs::GpioPinWrite { pin, level } => {
                self.device
                    .dispatch_write(pin, level == Level::High, seq, digest)
            }
            ToolArgs::SensorSampleRead { sensor } => {
                self.device.dispatch_sensor_read(sensor, seq, digest)
            }
            ToolArgs::TimerUptimeRead => self.device.dispatch_uptime_read(seq, digest),
            ToolArgs::TimerDelayWait { ms } => self.device.dispatch_delay_wait(ms, seq, digest),
            ToolArgs::DeviceStatusReport { detail } => {
                self.device.dispatch_status_report(detail, seq, digest)
            }
        }
    }

    /// `Verify`: the independent read-back every mutation must pass.
    ///
    /// Reads never reach this step (`Observe` routes them straight to
    /// `Account`): only the two idempotent-write tools do.
    fn do_verify(&mut self) -> Result<(), Halt> {
        let intent = self
            .pending_intent
            .clone()
            .ok_or(RuntimeError::IllegalTransition)?;
        self.fire(CrashPoint::AfterObservationBeforeVerify)?;
        let (expected, observed, passed) = self.verify_intent(&intent)?;
        self.fire(CrashPoint::BeforeVerificationCommit)?;
        self.journal.push(Frame::Verification {
            seq: intent.seq.0,
            expected,
            observed,
            passed,
        });
        self.fire(CrashPoint::AfterVerification)?;
        let step = if passed {
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
        };
        self.pending_step = Some(step);
        self.advance(if passed {
            Event::VerifyPass
        } else {
            Event::VerifyFail
        })?;
        Ok(())
    }

    /// Run the independent read-back for a mutating tool (SPEC §9).
    ///
    /// Returns the expected and observed JSON plus the pass flag.
    /// Reads never reach this step; any other tool here is an engine
    /// bug.
    fn verify_intent(&self, intent: &PendingIntent) -> Result<(Vec<u8>, Vec<u8>, bool), Halt> {
        match intent.tool_args {
            ToolArgs::GpioPinWrite { pin, level } => {
                // The verifier reads the pin's physical level directly —
                // independent of the dispatch path — and the mutation
                // completes only when read-back matches.
                let expected_high = level == Level::High;
                let observed_high = self.device.verify_read(pin);
                // HOST-ONLY (E0/E1)
                let expected = pin_level_json(pin.get(), expected_high);
                let observed = pin_level_json(pin.get(), observed_high);
                Ok((expected, observed, observed_high == expected_high))
            }
            ToolArgs::TimerDelayWait { ms } => {
                // The committed intent proves the requested `ms`; the
                // committed observation proves the pre-dispatch reading
                // (`t0 = uptime_ms - waited_ms`). The verifier reads the
                // clock through its own handle — never a flag the
                // dispatcher set — and checks the advance (SPEC §15.11).
                let outcome = intent
                    .outcome
                    .as_ref()
                    .ok_or(RuntimeError::IllegalTransition)?;
                let (t0, _post) = parse_delay_outcome(outcome)?;
                let target = t0.saturating_add(u64::from(ms));
                let observed = self.device.clock_read();
                // HOST-ONLY (E0/E1)
                let expected = target_json(target);
                let observed_json = observed_clock_json(observed);
                Ok((expected, observed_json, observed >= target))
            }
            _ => Err(RuntimeError::IllegalTransition.into()),
        }
    }

    /// `Account`: run the deterministic monitor over the completed step.
    fn do_account(&mut self) -> Result<(), Halt> {
        let step = self
            .pending_step
            .take()
            .ok_or(RuntimeError::IllegalTransition)?;
        // Kept for the degraded summary (e.g. which pin never reached
        // its level); the intent itself is consumed here.
        let intent = self.pending_intent.take();
        match self.monitor.observe(step, &self.budget) {
            MonitorVerdict::Continue => {
                self.advance(Event::GuardsClear)?;
            }
            MonitorVerdict::Degrade { status, reason } => {
                self.advance(Event::GuardFired)?;
                let (reason, summary) = self.degrade_text(status, reason, intent.as_ref());
                self.enter_degraded(status, &reason, &summary)?;
            }
        }
        Ok(())
    }

    /// `Finalize`: commit the model's terminal answer.
    fn do_finalize(&mut self) -> Result<(), Halt> {
        let summary = self
            .pending_finish
            .take()
            .ok_or(RuntimeError::IllegalTransition)?;
        self.fire(CrashPoint::BeforeTerminalCommit)?;
        self.push_terminal(TerminalStatus::Completed, b"model_finished", &summary)?;
        self.advance(Event::FinalizeCommitted)?;
        Ok(())
    }

    /// Commit a terminal result: journal frame plus Waymaker record.
    fn push_terminal(
        &mut self,
        status: TerminalStatus,
        reason: &[u8],
        summary: &[u8],
    ) -> Result<(), Halt> {
        // HOST-ONLY (E0/E1)
        self.journal.push(Frame::Terminal {
            status,
            reason: reason.to_vec(),
            summary: summary.to_vec(),
        });
        let record = match status {
            TerminalStatus::Completed => RecordRef::RunCompleted { result: summary },
            _ => RecordRef::RunFailed { error: reason },
        };
        self.advance_cursor(record)?;
        self.fire(CrashPoint::AfterTerminalCommit)?;
        Ok(())
    }

    /// Enter the degraded path: decide the terminal result and commit it.
    fn enter_degraded(
        &mut self,
        status: TerminalStatus,
        reason: &[u8],
        summary: &[u8],
    ) -> Result<(), Halt> {
        self.fire(CrashPoint::BeforeTerminalCommit)?;
        self.push_terminal(status, reason, summary)?;
        self.advance(Event::DegradedCommitted)?;
        Ok(())
    }

    /// Advance the Waymaker cursor over a derived record.
    fn advance_cursor(&mut self, record: RecordRef<'_>) -> Result<(), Halt> {
        self.cursor
            .advance(record)
            .map_err(|_| RuntimeError::ReplayDiverged)?;
        Ok(())
    }

    /// Advance the cursor while replaying, where the driver speaks
    /// [`RuntimeError`].
    ///
    /// Replay never fires a crash hook, so a crash halt here is an
    /// engine bug.
    fn advance_cursor_replay(&mut self, record: RecordRef<'_>) -> Result<(), RuntimeError> {
        self.advance_cursor(record).map_err(|halt| match halt {
            Halt::Crash(_) => RuntimeError::IllegalTransition,
            Halt::Error(error) => error,
        })
    }

    /// Take one normative edge, failing closed on an illegal pair.
    fn advance(&mut self, event: Event) -> Result<(), Halt> {
        self.sm = transition(self.sm, event).map_err(|_| RuntimeError::IllegalTransition)?;
        Ok(())
    }

    /// Fire the armed crash point, disarming it so it fires once.
    fn fire(&mut self, point: CrashPoint) -> Result<(), Halt> {
        if self.crash == Some(point) {
            self.crash = None;
            return Err(Halt::Crash(point));
        }
        Ok(())
    }

    /// The monitor's degrade verdict in the golden-trace vocabulary.
    fn degrade_text(
        &self,
        status: TerminalStatus,
        monitor_reason: &'static str,
        intent: Option<&PendingIntent>,
    ) -> (Vec<u8>, Vec<u8>) {
        match status {
            TerminalStatus::BudgetExhausted => {
                // HOST-ONLY (E0/E1)
                let reason = format!("{monitor_reason}_exhausted").into_bytes();
                let summary = self.budget_exhausted_summary(monitor_reason).into_bytes();
                (reason, summary)
            }
            TerminalStatus::Stuck if monitor_reason.contains("three identical") => {
                let summary = match intent.map(|intent| intent.tool_args) {
                    Some(ToolArgs::GpioPinWrite { pin, level }) => {
                        // HOST-ONLY (E0/E1)
                        format!(
                            "pin {} did not reach {} after 3 attempts",
                            pin.get(),
                            level.name()
                        )
                        .into_bytes()
                    }
                    Some(ToolArgs::TimerDelayWait { ms }) => {
                        // HOST-ONLY (E0/E1)
                        format!("clock did not advance by {ms}ms after 3 attempts").into_bytes()
                    }
                    _ => {
                        // HOST-ONLY (E0/E1)
                        b"verified mutation did not take effect after 3 attempts".to_vec()
                    }
                };
                (b"three_identical_verification_failures".to_vec(), summary)
            }
            _ => {
                // HOST-ONLY (E0/E1)
                let reason = monitor_reason.replace(' ', "_");
                (reason.clone().into_bytes(), reason.into_bytes())
            }
        }
    }

    /// The budget-exhaustion summary for the exhausted unit.
    fn budget_exhausted_summary(&self, unit: &str) -> String {
        // HOST-ONLY (E0/E1)
        match unit {
            "model_turns" => format!("budget exhausted after {} model turns", self.turns_used),
            "mutations" => format!("budget exhausted after {} mutations", self.mutations_used),
            _ => format!("budget exhausted: {unit}"),
        }
    }

    /// Decode one model line into owned pieces.
    ///
    /// The whole line decodes through `esper-core`'s normative
    /// decoder: a `CALL` arrives with the tool's numeric id, the
    /// already-bound typed arguments, the raw validated argument
    /// bytes, and the decode-time digest; `ASK` and `FINISH` keep
    /// their E0/E1 decoding byte-identical.
    fn classify_output(line: &[u8]) -> Classified {
        match decode_line(line) {
            Ok(Decision::Call(call)) => Classified::Call {
                tool: call.tool().get(),
                // HOST-ONLY (E0/E1)
                args: call.args_bytes().to_vec(),
                digest: call.args_digest().get(),
                tool_args: *call.args(),
            },
            Ok(Decision::Ask(ask)) => Classified::Ask {
                // HOST-ONLY (E0/E1)
                prompt: ask.prompt().to_vec(),
                schema: ask.response_schema_id(),
            },
            Ok(Decision::Finish(answer)) => Classified::Finish {
                // HOST-ONLY (E0/E1)
                summary: answer.summary().to_vec(),
            },
            Err(error) => {
                let variant = error.repair_variant().unwrap_or(RepairVariant::Malformed);
                Classified::Invalid(variant)
            }
        }
    }
}

/// The normative §5.2 reason bytes for a refused call, derived from
/// the call's shape: the capability gate reports the tool and the
/// resource, and these bytes are the run's terminal-reason vocabulary
/// for that refusal.
const fn denial_reason(args: ToolArgs) -> &'static [u8] {
    match args {
        ToolArgs::GpioPinRead { .. } => b"pin_not_in_read_capabilities",
        ToolArgs::GpioPinWrite { .. } => b"pin_not_in_write_capabilities",
        ToolArgs::SensorSampleRead { .. } => b"sensor_not_in_capabilities",
        ToolArgs::TimerUptimeRead | ToolArgs::TimerDelayWait { .. } => b"timer_not_permitted",
        ToolArgs::DeviceStatusReport { .. } => b"status_not_permitted",
    }
}

/// The human-readable denial summary naming the refused resource.
fn denial_summary(args: ToolArgs) -> String {
    // HOST-ONLY (E0/E1)
    match args {
        ToolArgs::GpioPinRead { pin } => format!("read of pin {} denied by policy", pin.get()),
        ToolArgs::GpioPinWrite { pin, .. } => {
            format!("write to pin {} denied by policy", pin.get())
        }
        ToolArgs::SensorSampleRead { sensor } => {
            format!("read of sensor {sensor} denied by policy")
        }
        ToolArgs::TimerUptimeRead => "uptime read denied by policy".to_string(),
        ToolArgs::TimerDelayWait { .. } => "delay wait denied by policy".to_string(),
        ToolArgs::DeviceStatusReport { .. } => "status report denied by policy".to_string(),
    }
}

/// The fault-plan resource for a call (SPEC §17.7): the pin for the
/// GPIO tools, the sensor id for `sensor_sample_read`, 0 for the
/// timer and status tools.
const fn dispatch_resource(args: ToolArgs) -> u8 {
    match args {
        ToolArgs::GpioPinRead { pin } | ToolArgs::GpioPinWrite { pin, .. } => pin.get(),
        ToolArgs::SensorSampleRead { sensor } => sensor,
        ToolArgs::TimerUptimeRead
        | ToolArgs::TimerDelayWait { .. }
        | ToolArgs::DeviceStatusReport { .. } => 0,
    }
}

/// The canonical JSON for a delay target: `{"target_ms":250}`.
fn target_json(target_ms: u64) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    format!("{{\"target_ms\":{target_ms}}}").into_bytes()
}

/// The canonical JSON for an observed clock reading:
/// `{"uptime_ms":250}`.
fn observed_clock_json(uptime_ms: u64) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    format!("{{\"uptime_ms\":{uptime_ms}}}").into_bytes()
}

/// Parse the machine-generated delay outcome
/// `{"waited_ms":M,"uptime_ms":N}` into the pre-dispatch reading `t0`
/// (`N - M`) and the post-dispatch reading `N`.
///
/// The outcome bytes were committed by the device, so any shape
/// violation is journal corruption, failed closed.
///
/// # Errors
///
/// Returns [`RuntimeError::JournalCorrupt`] when the outcome is not
/// the canonical delay shape.
fn parse_delay_outcome(outcome: &[u8]) -> Result<(u64, u64), RuntimeError> {
    use esper_protocol::json::{JsonValue, Parser};
    let mut parser = Parser::new(outcome);
    let JsonValue::Object(mut cursor) = parser
        .parse_value()
        .map_err(|_| RuntimeError::JournalCorrupt)?
    else {
        return Err(RuntimeError::JournalCorrupt);
    };
    let mut waited: Option<u64> = None;
    let mut uptime: Option<u64> = None;
    while let Some((key, value)) = cursor
        .next_entry()
        .map_err(|_| RuntimeError::JournalCorrupt)?
    {
        let JsonValue::Number(raw) = value else {
            return Err(RuntimeError::JournalCorrupt);
        };
        let number = parse_u64_digits(raw).ok_or(RuntimeError::JournalCorrupt)?;
        match key {
            b"waited_ms" => waited = Some(number),
            b"uptime_ms" => uptime = Some(number),
            _ => return Err(RuntimeError::JournalCorrupt),
        }
    }
    parser.finish().map_err(|_| RuntimeError::JournalCorrupt)?;
    let (Some(waited), Some(uptime)) = (waited, uptime) else {
        return Err(RuntimeError::JournalCorrupt);
    };
    let t0 = uptime
        .checked_sub(waited)
        .ok_or(RuntimeError::JournalCorrupt)?;
    Ok((t0, uptime))
}

/// Parse plain ASCII digits into a `u64`, failing closed on anything
/// else.
fn parse_u64_digits(raw: &[u8]) -> Option<u64> {
    if raw.is_empty() {
        return None;
    }
    let mut number: u64 = 0;
    for byte in raw {
        if !byte.is_ascii_digit() {
            return None;
        }
        number = number
            .checked_mul(10)?
            .checked_add(u64::from(byte - b'0'))?;
    }
    Some(number)
}

/// Map a world-double failure to the runtime error vocabulary.
///
/// An argument mismatch under a committed effect id means the replayed
/// history disagrees with the journal: fail closed as divergence. An
/// unknown resource is unreachable (the decoder proves every range
/// before dispatch), so it is an engine bug.
const fn map_world(error: WorldError) -> RuntimeError {
    match error {
        WorldError::EffectArgsMismatch => RuntimeError::ReplayDiverged,
        WorldError::UnknownResource => RuntimeError::IllegalTransition,
    }
}

/// FNV-1a over 32 bits, for the Waymaker `input_crc` digest.
///
/// The journal's canonical intent bytes are hashed at commit and at
/// replay; equality is checked by the cursor's divergence rules.
const fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    let mut i = 0;
    while i < bytes.len() {
        hash ^= bytes[i] as u32;
        hash = hash.wrapping_mul(0x0100_0193);
        i += 1;
    }
    hash
}

/// The canonical JSON for a pin level: `{"pin":4,"level":"high"}`.
fn pin_level_json(pin: u8, level_high: bool) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    format!(
        "{{\"pin\":{pin},\"level\":\"{}\"}}",
        if level_high { "high" } else { "low" }
    )
    .into_bytes()
}

/// Skip ASCII whitespace.
fn skip_ws(mut bytes: &[u8]) -> &[u8] {
    while matches!(bytes.first(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
        bytes = &bytes[1..];
    }
    bytes
}

/// Validate typed human input against the ask schema.
///
/// Schema 1 is pin select: exactly `{"pin": N}` with `N` in `0..=7`,
/// optional surrounding whitespace. Anything else is a harness bug,
/// not a run outcome.
///
/// # Errors
///
/// Returns a static reason when the schema is unsupported or the input
/// does not match it.
fn validate_ask_input(input: &[u8], schema: u8) -> Result<(), &'static str> {
    if schema != 1 {
        return Err("unsupported ask schema");
    }
    let err = "ask input must be {\"pin\": N} with N in 0..=7";
    let mut rest = skip_ws(input);
    rest = rest.strip_prefix(b"{").ok_or(err)?;
    rest = skip_ws(rest);
    rest = rest.strip_prefix(b"\"pin\"").ok_or(err)?;
    rest = skip_ws(rest);
    rest = rest.strip_prefix(b":").ok_or(err)?;
    rest = skip_ws(rest);
    let digits = rest.iter().take_while(|byte| byte.is_ascii_digit()).count();
    if digits != 1 || rest[0] > b'7' {
        return Err(err);
    }
    rest = skip_ws(&rest[1..]);
    rest = rest.strip_prefix(b"}").ok_or(err)?;
    if !skip_ws(rest).is_empty() {
        return Err(err);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        denial_reason, denial_summary, dispatch_resource, fnv1a32, parse_delay_outcome,
        parse_u64_digits, pin_level_json, target_json, validate_ask_input,
    };
    use esper_core::decision::{Level, StatusDetail, ToolArgs};
    use esper_core::ids::Pin;

    fn pin(n: u8) -> Pin {
        Pin::new(n).expect("bad pin in test")
    }

    #[test]
    fn fnv1a_is_the_standard_hash() {
        // FNV-1a 32-bit test vectors.
        assert_eq!(fnv1a32(b""), 0x811c_9dc5);
        assert_eq!(fnv1a32(b"a"), 0xe40c_292c);
        assert_eq!(fnv1a32(b"foobar"), 0xbf9c_f968);
    }

    #[test]
    fn pin_level_json_is_canonical() {
        assert_eq!(pin_level_json(4, true), b"{\"pin\":4,\"level\":\"high\"}");
        assert_eq!(pin_level_json(0, false), b"{\"pin\":0,\"level\":\"low\"}");
    }

    #[test]
    fn ask_schema_1_accepts_exact_pin_select() {
        assert!(validate_ask_input(br#"{"pin": 4}"#, 1).is_ok());
        assert!(validate_ask_input(b" { \"pin\" : 7 } ", 1).is_ok());
        assert!(validate_ask_input(b"{\"pin\":0}", 1).is_ok());
    }

    #[test]
    fn ask_schema_1_rejects_anything_else() {
        for bad in [
            br#"{"pin": 8}"#.as_slice(),
            br#"{"pin": 10}"#.as_slice(),
            br#"{"pin": -1}"#.as_slice(),
            br#"{"pin":}"#.as_slice(),
            br#"{"pin": 4, "extra": 1}"#.as_slice(),
            b"",
            b"pin 4",
        ] {
            assert!(validate_ask_input(bad, 1).is_err(), "{bad:?}");
        }
        // Unsupported schemas fail even for well-formed input.
        assert!(validate_ask_input(br#"{"pin": 4}"#, 2).is_err());
        assert!(validate_ask_input(br#"{"pin": 4}"#, 0).is_err());
    }

    #[test]
    fn denial_summary_names_the_refused_resource() {
        assert_eq!(
            denial_summary(ToolArgs::GpioPinRead { pin: pin(4) }),
            "read of pin 4 denied by policy"
        );
        assert_eq!(
            denial_summary(ToolArgs::GpioPinWrite {
                pin: pin(4),
                level: Level::High
            }),
            "write to pin 4 denied by policy"
        );
        assert_eq!(
            denial_summary(ToolArgs::SensorSampleRead { sensor: 2 }),
            "read of sensor 2 denied by policy"
        );
        assert_eq!(
            denial_summary(ToolArgs::TimerDelayWait { ms: 250 }),
            "delay wait denied by policy"
        );
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
            denial_reason(ToolArgs::TimerDelayWait { ms: 250 }),
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
    fn dispatch_resource_follows_spec_17_7() {
        assert_eq!(dispatch_resource(ToolArgs::GpioPinRead { pin: pin(4) }), 4);
        assert_eq!(
            dispatch_resource(ToolArgs::SensorSampleRead { sensor: 2 }),
            2
        );
        assert_eq!(dispatch_resource(ToolArgs::TimerDelayWait { ms: 250 }), 0);
        assert_eq!(dispatch_resource(ToolArgs::TimerUptimeRead), 0);
    }

    #[test]
    fn target_json_is_canonical() {
        assert_eq!(target_json(250), b"{\"target_ms\":250}");
    }

    #[test]
    fn parse_delay_outcome_recovers_t0() {
        assert_eq!(
            parse_delay_outcome(br#"{"waited_ms":250,"uptime_ms":1250}"#).expect("canonical"),
            (1000, 1250)
        );
        assert!(parse_delay_outcome(br#"{"waited_ms":250}"#).is_err());
        assert!(parse_delay_outcome(br#"{"waited_ms":999,"uptime_ms":1}"#).is_err());
        assert!(parse_delay_outcome(b"not json").is_err());
    }

    #[test]
    fn parse_u64_digits_is_strict() {
        assert_eq!(parse_u64_digits(b"0"), Some(0));
        assert_eq!(parse_u64_digits(b"18446744073709551615"), Some(u64::MAX));
        assert_eq!(parse_u64_digits(b"18446744073709551616"), None);
        assert_eq!(parse_u64_digits(b""), None);
        assert_eq!(parse_u64_digits(b"12a"), None);
    }
}
