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

use esper_core::decision::{decode_line, Decision, Level};
use esper_core::error::{ErrorCode, RepairVariant};
use esper_core::ids::{Digest, Pin, ToolId};
use esper_core::monitor::{Monitor, MonitorVerdict, ProgressDelta, StepRecord};
use esper_core::registry::{authorize_capability, lookup_by_id, PermissionClass};
use esper_core::state::{transition, Event, State, TerminalStatus};
use esper_core::ResourceBudget;
use waymaker_core::{
    ActivityKind, EffectIdAllocator, EffectSeq, RecordRef, ReplayCursor, RunId as WaymakerRunId,
};

use crate::error::{CrashPoint, Halt, RuntimeError};
use crate::journal::{DecisionClass, Frame, Journal};
use crate::seed::{RunSeed, ESPER_WORKFLOW_KIND};
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
        /// The target pin.
        pin: u8,
        /// The requested level (writes only).
        level_high: Option<bool>,
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
    /// The target pin.
    pin: u8,
    /// The requested level (writes only).
    level_high: Option<bool>,
}

/// A committed intent with no terminal outcome yet.
#[derive(Clone)]
struct PendingIntent {
    /// The stable effect identity.
    seq: EffectSeq,
    /// The tool's numeric id.
    tool: u8,
    /// The decode-time digest of the argument bytes.
    digest: u64,
    /// The target pin.
    pin: u8,
    /// Whether the tool mutates the world.
    write: bool,
    /// The requested level (writes only).
    level_high: bool,
    /// Attempts so far under this effect id.
    attempt: u32,
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
        seed_input: [0u8; 32],
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
    seed_input: [u8; 32],
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
                pin,
                write,
                level_high,
            } => self.replay_intent(*seq, *tool, args, *digest, *pin, *write, *level_high),
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
        match Self::classify_output(output)? {
            Classified::Call {
                tool,
                args,
                digest,
                pin,
                level_high,
            } => {
                if class != DecisionClass::Call {
                    return Err(RuntimeError::JournalCorrupt);
                }
                self.pending_call = Some(PendingCall {
                    tool,
                    args,
                    digest,
                    pin,
                    level_high,
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
    /// effect id, and the cursor replays the schedule record.
    #[allow(clippy::too_many_arguments)]
    fn replay_intent(
        &mut self,
        seq: u32,
        tool: u8,
        args: &[u8],
        digest: u64,
        pin: u8,
        write: bool,
        level_high: bool,
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
        self.pending_call = None;
        self.pending_intent = Some(PendingIntent {
            seq: EffectSeq(seq),
            tool,
            digest,
            pin,
            write,
            level_high,
            attempt: 0,
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
        let classified = Self::classify_output(line).map_err(Halt::Error)?;
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
                pin,
                level_high,
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
                    pin,
                    level_high,
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
    fn do_authorize(&mut self) -> Result<(), Halt> {
        let call = self
            .pending_call
            .take()
            .ok_or(RuntimeError::IllegalTransition)?;
        self.fire(CrashPoint::BeforeAuthorize)?;
        let entry = lookup_by_id(ToolId::new(call.tool)).ok_or(RuntimeError::IllegalTransition)?;
        let pin = Pin::new(call.pin).map_err(RuntimeError::Core)?;
        let is_write = matches!(entry.permission, PermissionClass::IdempotentWrite);
        if authorize_capability(&self.seed.capabilities(), entry, pin).is_err() {
            let (reason, summary) = if is_write {
                (
                    "pin_not_in_write_capabilities",
                    // HOST-ONLY (E0/E1)
                    format!("write to pin {} denied by policy", pin.get()),
                )
            } else {
                (
                    "pin_not_in_read_capabilities",
                    format!("read of pin {} denied by policy", pin.get()),
                )
            };
            self.advance(Event::Denied)?;
            let reason = reason.as_bytes().to_vec();
            let summary = summary.into_bytes();
            return self.enter_degraded(TerminalStatus::Denied, &reason, &summary);
        }
        if is_write && self.device.direction(pin) == crate::world::Direction::Input {
            let summary = format!("write to pin {} denied by device direction", pin.get());
            self.advance(Event::Denied)?;
            let summary = summary.into_bytes();
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
            pin: call.pin,
            write: is_write,
            level_high: call.level_high.unwrap_or(false),
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
            digest: call.digest,
            pin: call.pin,
            write: is_write,
            level_high: call.level_high.unwrap_or(false),
            attempt: 0,
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
        let pin = Pin::new(intent.pin).map_err(RuntimeError::Core)?;
        if self.faults.consume(intent.tool, intent.pin) {
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
        let outcome = if intent.write {
            self.device
                .dispatch_write(pin, intent.level_high, intent.seq, intent.digest)
                .map_err(map_world)?
        } else {
            self.device
                .dispatch_read(pin, intent.seq, intent.digest)
                .map_err(map_world)?
        };
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

    /// `Verify`: the independent read-back every mutation must pass.
    ///
    /// The verifier reads the pin's physical level directly —
    /// independent of the dispatch path — and the mutation completes
    /// only when read-back matches the requested state.
    fn do_verify(&mut self) -> Result<(), Halt> {
        let intent = self
            .pending_intent
            .clone()
            .ok_or(RuntimeError::IllegalTransition)?;
        self.fire(CrashPoint::AfterObservationBeforeVerify)?;
        let pin = Pin::new(intent.pin).map_err(RuntimeError::Core)?;
        let observed_high = self.device.verify_read(pin);
        let passed = observed_high == intent.level_high;
        // HOST-ONLY (E0/E1)
        let expected = pin_level_json(intent.pin, intent.level_high);
        let observed = pin_level_json(intent.pin, observed_high);
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
                let (pin, level) = intent.map_or((0, "high"), |intent| {
                    (intent.pin, if intent.level_high { "high" } else { "low" })
                });
                (
                    b"three_identical_verification_failures".to_vec(),
                    // HOST-ONLY (E0/E1)
                    format!("pin {pin} did not reach {level} after 3 attempts").into_bytes(),
                )
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
    fn classify_output(line: &[u8]) -> Result<Classified, RuntimeError> {
        match decode_line(line) {
            Ok(Decision::Call(call)) => {
                let pin = call.pin().map_err(RuntimeError::Core)?.get();
                let level_high = match call.level() {
                    Ok(Level::High) => Some(true),
                    Ok(Level::Low) => Some(false),
                    Err(_) => None,
                };
                Ok(Classified::Call {
                    tool: call.tool().get(),
                    // HOST-ONLY (E0/E1)
                    args: call.args().to_vec(),
                    digest: call.args_digest().get(),
                    pin,
                    level_high,
                })
            }
            Ok(Decision::Ask(ask)) => Ok(Classified::Ask {
                // HOST-ONLY (E0/E1)
                prompt: ask.prompt.to_vec(),
                schema: ask.response_schema_id,
            }),
            Ok(Decision::Finish(answer)) => Ok(Classified::Finish {
                // HOST-ONLY (E0/E1)
                summary: answer.summary.to_vec(),
            }),
            Err(error) => {
                let variant = error.repair_variant().unwrap_or(RepairVariant::Malformed);
                Ok(Classified::Invalid(variant))
            }
        }
    }
}

/// Map a world-double failure to the runtime error vocabulary.
///
/// An argument mismatch under a committed effect id means the replayed
/// history disagrees with the journal: fail closed as divergence.
const fn map_world(error: WorldError) -> RuntimeError {
    match error {
        WorldError::EffectArgsMismatch => RuntimeError::ReplayDiverged,
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
    use super::{fnv1a32, pin_level_json, validate_ask_input};

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
}
