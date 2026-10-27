//! HOST-ONLY: drive the run workflow over the test doubles.
//!
//! Nothing here is product logic. [`super::run`] is `no_std` and runs on
//! the device; these doubles stand in for the device's half:
//!
//! * a `waymaker_embassy::Journal` over [`Journal`]'s in-memory records
//!   — replay from the start on every boot, redelivery of an intent with
//!   no outcome under its original id — that injects one reset at the
//!   Waymaker boundary each [`CrashPoint`] names;
//! * a dispatcher over the [`FakeDevice`], the [`FaultPlan`], the
//!   [`InputPlan`] and a model backend, built on [`super::device`];
//! * [`drive_segment`] and [`drive_run`], which poll the workflow with
//!   a no-op waker the way an Embassy executor would, reboot after an
//!   injected reset, write the terminal record the workflow concludes,
//!   and derive the frame view the trace and fixtures read.
//!
//! # Where each crash point lands
//!
//! | [`CrashPoint`] | Boundary |
//! |---|---|
//! | `ModelIntent` | after an `INFER` intent commits |
//! | `BeforeDecisionCommit` | after inference, before its answer commits (the model unemits) |
//! | `BeforeAuthorize` | after a `CALL` decision's answer commits |
//! | `ToolIntent` | after a first `TOOL` attempt's intent commits |
//! | `AfterPhysicalBeforeObservation` | after the device acted, before the `TOOL` answer commits |
//! | `AfterObservationBeforeVerify` | after a mutation's `TOOL` answer commits |
//! | `BeforeVerificationCommit` | after the read-back, before the `VERIFY` answer commits |
//! | `AfterVerification` | after the `VERIFY` answer commits |
//! | `AwaitInput` | after an `ASK` intent commits |
//! | `BeforeTerminalCommit` | after the workflow concludes, before the terminal record |
//! | `AfterTerminalCommit` | after the terminal record |

use core::cell::Cell;
use core::pin::pin;
use core::task::{Context, Poll, Waker};

use esper_core::decision::{Decision, Level, ToolArgs, decode_line};
use esper_core::error::RepairVariant;
use esper_core::lineage::Lineage;
use waymaker_core::timer::{ClockKind, TimerSpec};
use waymaker_core::{ActivityKind, EffectId, EffectSeq, Outcome, RunId as WaymakerRunId};
use waymaker_embassy::ctx::Conclusion;
use waymaker_embassy::{Answer, Ctx, Decode, Halted, Handoff, Journal as WaymakerJournal};

use super::RunState;
use super::device::{Attempt, RunDispatcher, RunPorts};
use super::wire::{
    self, AskInput, AskRequest, Check, Fault, Inferred, Observed, RunStart, SnapshotRef,
    TerminalRecord, ToolRequest, Verified, VerifyRequest,
};
use crate::agent::wire::PAGE_RECORD_MAX;
use crate::backend::{ModelBackend, fnv1a64};
use crate::error::{CrashPoint, RuntimeError};
use crate::journal::{DecisionClass, Frame, Journal, Record};
use crate::seed::RunSeed;
use crate::trace::RunTrace;
use crate::world::{FakeDevice, FaultPlan, InputPlan, WorldError};

/// Boots before the driver gives up: every injected reset disarms
/// itself, so a segment needs at most a handful.
const MAX_BOOTS: u32 = 32;

/// The verified handoff from a rolled-over segment to its child (E4,
/// SPEC §20.6): what the segment's `continue_as_new` carried, plus the
/// host-held observability the trace reports.
#[derive(Debug, Clone)]
pub struct RolloverHandoff {
    /// The encoded parent fold, as the segment stored it.
    pub snapshot: Vec<u8>,
    /// The lineage binding the child to this segment.
    pub lineage: Lineage,
    /// This segment's cumulative built-prompt bytes.
    pub prompt_bytes: u64,
    /// The masking digest stream so far, in commit order: one digest
    /// per observation, `0` where it carried no secret.
    pub mask_digests: Vec<u64>,
    /// The harvest the segment carried over: one encoded memory entry
    /// per item, oldest first.
    pub harvest: Vec<Vec<u8>>,
}

impl RolloverHandoff {
    /// The child run's seed: `parent` continued at this lineage (see
    /// [`RunSeed::child`]).
    #[must_use]
    pub const fn child_seed(&self, parent: &RunSeed) -> RunSeed {
        parent.child(self.lineage)
    }
}

/// What one driven segment produced (E4, SPEC §20.4).
pub enum SegmentOutcome {
    /// The run reached its terminal result.
    Completed(RunTrace),
    /// The run suspended awaiting human input (durable; resumable).
    Suspended(RunTrace),
    /// The segment hit 80% of its context budget and continued as new:
    /// the trace covers this segment, and the handoff starts the child.
    Rollover(RunTrace, RolloverHandoff),
}

/// Why the last poll stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stop {
    Crash,
    Diverged,
}

/// The boundary a crash point names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Boundary {
    AfterSchedule,
    BeforeResolve,
    AfterResolve,
}

/// The crash point a new commit at `boundary` of `records`' last
/// scheduled activity fires, given its answer (the table in the module
/// docs). Shared with the supervisor's host double.
pub(crate) fn crash_point(
    records: &[Record],
    boundary: Boundary,
    answer: Option<&[u8]>,
) -> Option<CrashPoint> {
    let Some(Record::Scheduled { kind, input, .. }) = records
        .iter()
        .rev()
        .find(|r| matches!(r, Record::Scheduled { .. }))
    else {
        return None;
    };
    let kind = ActivityKind(*kind);
    match (kind, boundary) {
        (wire::INFER, Boundary::AfterSchedule) => Some(CrashPoint::ModelIntent),
        (wire::INFER, Boundary::BeforeResolve) => Some(CrashPoint::BeforeDecisionCommit),
        (wire::INFER, Boundary::AfterResolve) => {
            let line = Inferred::decode(answer?).ok()?;
            matches!(decode_line(line.line()), Ok(Decision::Call(_)))
                .then_some(CrashPoint::BeforeAuthorize)
        }
        (wire::TOOL, Boundary::AfterSchedule) => {
            let request = ToolRequest::decode(input).ok()?;
            (request.attempt == 1).then_some(CrashPoint::ToolIntent)
        }
        (wire::TOOL, Boundary::BeforeResolve) => Some(CrashPoint::AfterPhysicalBeforeObservation),
        (wire::TOOL, Boundary::AfterResolve) => {
            let request = ToolRequest::decode(input).ok()?;
            let observed = Observed::decode(answer?).ok()?;
            (request.write && !observed.transient)
                .then_some(CrashPoint::AfterObservationBeforeVerify)
        }
        (wire::VERIFY, Boundary::BeforeResolve) => Some(CrashPoint::BeforeVerificationCommit),
        (wire::VERIFY, Boundary::AfterResolve) => Some(CrashPoint::AfterVerification),
        (wire::ASK, Boundary::AfterSchedule) => Some(CrashPoint::AwaitInput),
        _ => None,
    }
}

/// The run journal double: Waymaker's protocol over [`Journal`]'s
/// records, with one armed reset.
struct RunJournal<'j> {
    journal: &'j mut Journal,
    run: u64,
    crash: &'j Cell<Option<CrashPoint>>,
    reset: &'j Cell<Option<Reset>>,
    /// Set when a reset dropped the record of an operator input the
    /// world already delivered.
    lost: &'j Cell<bool>,
    stop: &'j Cell<Option<Stop>>,
    unemit: &'j Cell<bool>,
    pos: usize,
}

impl RunJournal<'_> {
    /// Count one new record; say whether the armed [`Reset`] fires
    /// before it commits (`Some(true)`), after (`Some(false)`), or not.
    fn count(&mut self) -> Option<bool> {
        self.journal.commits += 1;
        let reset = self.reset.get()?;
        if reset.nth != self.journal.commits {
            return None;
        }
        self.reset.set(None);
        if self.stop.get().is_none() {
            self.stop.set(Some(Stop::Crash));
        }
        Some(reset.before)
    }

    fn diverged(&self) -> Halted {
        self.stop.set(Some(Stop::Diverged));
        Halted
    }

    /// Fire the armed crash if `point` matches it.
    fn fire(&self, point: CrashPoint) -> bool {
        if self.crash.get() == Some(point) {
            self.crash.set(None);
            if self.stop.get().is_none() {
                self.stop.set(Some(Stop::Crash));
            }
            true
        } else {
            false
        }
    }

    /// The kind of the last scheduled activity.
    fn last_kind(&self) -> Option<ActivityKind> {
        self.journal.records.iter().rev().find_map(|r| match r {
            Record::Scheduled { kind, .. } => Some(ActivityKind(*kind)),
            Record::Resolved { .. } => None,
        })
    }

    /// The crash point a new commit at `boundary` of the last
    /// scheduled activity fires, given its answer.
    fn points(&self, boundary: Boundary, answer: Option<&[u8]>) -> Option<CrashPoint> {
        crash_point(&self.journal.records, boundary, answer)
    }

    const fn id(&self, seq: u32) -> EffectId {
        EffectId {
            run: WaymakerRunId(self.run),
            seq: EffectSeq(seq),
        }
    }
}

impl WaymakerJournal for RunJournal<'_> {
    fn schedule(&mut self, kind: ActivityKind, input: &[u8]) -> Result<Handoff<'_>, Halted> {
        let records = &self.journal.records;
        if self.pos < records.len() {
            let Some(Record::Scheduled {
                kind: k,
                input: i,
                seq,
            }) = records.get(self.pos)
            else {
                return Err(self.diverged());
            };
            if *k != kind.0 || i != input {
                return Err(self.diverged());
            }
            let seq = *seq;
            self.pos += 1;
            return match self.journal.records.get(self.pos) {
                // An intent with no outcome: redeliver under its id.
                None => Ok(Handoff::Dispatch {
                    id: self.id(seq),
                    result_bytes: PAGE_RECORD_MAX,
                }),
                Some(Record::Resolved { completed, bytes }) => {
                    self.pos += 1;
                    Ok(Handoff::Replayed(if *completed {
                        Outcome::Completed(bytes)
                    } else {
                        Outcome::Failed(bytes)
                    }))
                }
                Some(Record::Scheduled { .. }) => Err(self.diverged()),
            };
        }
        let seq = u32::try_from(
            records
                .iter()
                .filter(|r| matches!(r, Record::Scheduled { .. }))
                .count()
                + 1,
        )
        .unwrap_or(u32::MAX);
        let reset = self.count();
        if reset == Some(true) {
            return Err(Halted);
        }
        // HOST-ONLY
        self.journal.records.push(Record::Scheduled {
            kind: kind.0,
            input: input.to_vec(),
            seq,
        });
        self.pos += 1;
        if reset == Some(false) {
            return Err(Halted);
        }
        if let Some(point) = self.points(Boundary::AfterSchedule, None)
            && self.fire(point)
        {
            return Err(Halted);
        }
        Ok(Handoff::Dispatch {
            id: self.id(seq),
            result_bytes: PAGE_RECORD_MAX,
        })
    }

    fn resolve(&mut self, answer: Answer<'_>) -> Result<Outcome<'_>, Halted> {
        if let Some(point) = self.points(Boundary::BeforeResolve, None)
            && self.fire(point)
        {
            if point == CrashPoint::BeforeDecisionCommit {
                // The emitted line was never committed: the model must
                // emit it again.
                self.unemit.set(true);
            }
            return Err(Halted);
        }
        let reset = self.count();
        if reset == Some(true) {
            match self.last_kind() {
                Some(wire::INFER) => self.unemit.set(true),
                // The operator's input was taken and its record lost:
                // the channel delivers it again.
                Some(wire::ASK) => self.lost.set(true),
                _ => {}
            }
            return Err(Halted);
        }
        let (completed, bytes) = match answer {
            Answer::Completed(bytes) => (true, bytes.to_vec()),
            Answer::Failed(bytes) => (false, bytes.to_vec()),
            Answer::Exhausted => (false, Vec::new()),
        };
        // HOST-ONLY
        self.journal.records.push(Record::Resolved {
            completed,
            bytes: bytes.clone(),
        });
        self.pos += 1;
        if reset == Some(false) {
            return Err(Halted);
        }
        if completed
            && let Some(point) = self.points(Boundary::AfterResolve, Some(&bytes))
            && self.fire(point)
        {
            return Err(Halted);
        }
        match self.journal.records.last() {
            Some(Record::Resolved {
                completed: true,
                bytes,
            }) => Ok(Outcome::Completed(bytes)),
            Some(Record::Resolved { bytes, .. }) => Ok(Outcome::Failed(bytes)),
            _ => Err(Halted),
        }
    }

    fn wait(&mut self, _spec: TimerSpec) -> Result<(), Halted> {
        // The run workflow keeps no deadlines.
        Err(self.diverged())
    }

    fn continue_as_new(&mut self, input: &[u8]) -> Halted {
        // HOST-ONLY: the segment's records stay readable for the trace;
        // the driver starts the child on a fresh journal.
        self.journal.continued = Some(input.to_vec());
        Halted
    }

    fn deadline_remaining(&self) -> Option<(ClockKind, u64)> {
        None
    }
}

/// The world doubles behind the run's activities: the device's half,
/// as [`RunPorts`] for the product [`RunDispatcher`].
pub(crate) struct HostPorts<'w> {
    pub(crate) model: &'w mut dyn ModelBackend,
    pub(crate) device: &'w mut FakeDevice,
    pub(crate) faults: &'w mut FaultPlan,
    pub(crate) inputs: &'w mut InputPlan,
    pub(crate) store: &'w mut std::collections::BTreeMap<(u64, u8), Vec<u8>>,
    #[cfg(test)]
    pub(crate) injected_outcome: Option<Vec<u8>>,
}

/// The fault-plan resource for a call (SPEC §17.7).
const fn dispatch_resource(args: ToolArgs) -> u8 {
    match args {
        ToolArgs::GpioPinRead { pin } | ToolArgs::GpioPinWrite { pin, .. } => pin.get(),
        ToolArgs::SensorSampleRead { sensor } => sensor,
        ToolArgs::TimerUptimeRead
        | ToolArgs::TimerDelayWait { .. }
        | ToolArgs::DeviceStatusReport { .. } => 0,
    }
}

const fn world_fault(error: WorldError) -> Fault {
    match error {
        WorldError::EffectArgsMismatch => Fault::EffectArgsMismatch,
        WorldError::UnknownResource => Fault::UnknownResource,
    }
}

/// Copy `bytes` into `out`, or fail with `fault`.
fn copy_out(bytes: &[u8], out: &mut [u8], fault: Fault) -> Result<usize, Fault> {
    out.get_mut(..bytes.len())
        .ok_or(fault)?
        .copy_from_slice(bytes);
    Ok(bytes.len())
}

impl RunPorts for HostPorts<'_> {
    fn model(&mut self) -> &mut dyn ModelBackend {
        &mut *self.model
    }

    fn tool(
        &mut self,
        effect: EffectId,
        tool: esper_core::ids::ToolId,
        digest: u64,
        args: ToolArgs,
        raw: &mut [u8],
    ) -> Result<Attempt, Fault> {
        // The transient-fault plan is consulted before the device, so a
        // hiccup never reaches hardware.
        if self.faults.consume(tool.get(), dispatch_resource(args)) {
            return Ok(Attempt::Transient);
        }
        #[cfg(test)]
        if let Some(outcome) = self.injected_outcome.take() {
            return copy_out(&outcome, raw, Fault::OutcomeTooLarge).map(Attempt::Done);
        }
        let outcome = match args {
            ToolArgs::GpioPinRead { pin } => self.device.dispatch_read(pin, effect, digest),
            ToolArgs::GpioPinWrite { pin, level } => {
                self.device
                    .dispatch_write(pin, level == Level::High, effect, digest)
            }
            ToolArgs::SensorSampleRead { sensor } => {
                self.device.dispatch_sensor_read(sensor, effect, digest)
            }
            ToolArgs::TimerUptimeRead => self.device.dispatch_uptime_read(effect, digest),
            ToolArgs::TimerDelayWait { ms } => self.device.dispatch_delay_wait(ms, effect, digest),
            ToolArgs::DeviceStatusReport { detail } => {
                self.device.dispatch_status_report(detail, effect, digest)
            }
        }
        .map_err(world_fault)?;
        copy_out(&outcome, raw, Fault::OutcomeTooLarge).map(Attempt::Done)
    }

    fn pin_is_output(&mut self, pin: esper_core::ids::Pin) -> bool {
        self.device.direction(pin) == crate::world::Direction::Output
    }

    fn read_back(&mut self, check: Check) -> u64 {
        match check {
            // The verifier reads the pin directly, independent of the
            // dispatch path.
            Check::Pin { pin, .. } => esper_core::ids::Pin::new(pin)
                .map_or(0, |pin| u64::from(self.device.verify_read(pin))),
            // The verifier's own clock handle.
            Check::Clock { .. } => self.device.clock_read(),
        }
    }

    fn operator_input(&mut self, out: &mut [u8]) -> Result<Option<usize>, Fault> {
        self.inputs.take().map_or(Ok(None), |bytes| {
            copy_out(&bytes, out, Fault::BadInput).map(Some)
        })
    }

    fn store_chunk(&mut self, digest: u64, index: u8, chunk: &[u8]) -> Result<(), Fault> {
        // HOST-ONLY
        self.store.insert((digest, index), chunk.to_vec());
        Ok(())
    }

    fn load_chunk(&mut self, digest: u64, index: u8, out: &mut [u8]) -> Result<usize, Fault> {
        let chunk = self
            .store
            .get(&(digest, index))
            .ok_or(Fault::SnapshotMissing)?;
        copy_out(chunk, out, Fault::BadRequest)
    }
}

/// The canonical JSON for a pin level: `{"pin":4,"level":"high"}`.
fn pin_level_json(pin: u8, high: bool) -> Vec<u8> {
    // HOST-ONLY
    format!(
        "{{\"pin\":{pin},\"level\":\"{}\"}}",
        if high { "high" } else { "low" }
    )
    .into_bytes()
}

/// The `ModelDecision` frame for one recorded inference; `repairs`
/// counts the invalid lines so far.
fn decision_frame(answer: &[u8], repairs: &mut u8) -> Result<Frame, RuntimeError> {
    let inferred = Inferred::decode(answer).map_err(|_| RuntimeError::JournalCorrupt)?;
    let line = inferred.line();
    let (class, repair_index) = match decode_line(line) {
        Ok(Decision::Call(_)) => (DecisionClass::Call, 0),
        Ok(Decision::Ask(_)) => (DecisionClass::Ask, 0),
        Ok(Decision::Finish(_)) => (DecisionClass::Finish, 0),
        Err(error) => {
            *repairs = repairs.saturating_add(1);
            let class = match error.repair_variant() {
                Some(RepairVariant::InvalidArgs) => DecisionClass::InvalidArgs,
                _ => DecisionClass::Malformed,
            };
            (class, *repairs)
        }
    };
    Ok(Frame::ModelDecision {
        output: line.to_vec(),
        class,
        repair_index,
        input_tokens: inferred.input_tokens,
        output_tokens: inferred.output_tokens,
        prompt_bytes: u64::from(inferred.prompt_bytes),
    })
}

/// The `Verification` frame for one recorded read-back, its expected
/// and observed JSON rendered from the request and the reading.
fn verification_frame(input: &[u8], answer: &[u8]) -> Result<Frame, RuntimeError> {
    let request = VerifyRequest::decode(input).map_err(|_| RuntimeError::JournalCorrupt)?;
    let verified = Verified::decode(answer).map_err(|_| RuntimeError::JournalCorrupt)?;
    // HOST-ONLY
    let (expected, observed) = match request.check {
        Check::Pin { pin, high } => (
            pin_level_json(pin, high),
            pin_level_json(pin, verified.observed != 0),
        ),
        Check::Clock { target_ms } => (
            format!("{{\"target_ms\":{target_ms}}}").into_bytes(),
            format!("{{\"uptime_ms\":{}}}", verified.observed).into_bytes(),
        ),
    };
    Ok(Frame::Verification {
        seq: request.seq,
        expected,
        observed,
        passed: request.check.passes(verified.observed),
    })
}

/// Rebuild the golden-trace frame view from a segment's records.
///
/// # Errors
///
/// [`RuntimeError::JournalCorrupt`] when a record does not decode.
pub(crate) fn derive_frames(journal: &Journal) -> Result<Vec<Frame>, RuntimeError> {
    // HOST-ONLY
    let mut frames = Vec::new();
    if journal.input.is_empty() {
        return Ok(frames);
    }
    let start = RunStart::decode(&journal.input).map_err(|_| RuntimeError::JournalCorrupt)?;
    frames.push(Frame::RunStarted {
        seed: start.seed.snapshot(),
    });
    let corrupt = |_| RuntimeError::JournalCorrupt;
    let mut repairs: u8 = 0;
    let mut records = journal.records.iter().peekable();
    while let Some(record) = records.next() {
        let Record::Scheduled { kind, input, .. } = record else {
            return Err(RuntimeError::JournalCorrupt);
        };
        let answer = match records.peek() {
            Some(Record::Resolved { completed, bytes }) => {
                records.next();
                Some((*completed, bytes.as_slice()))
            }
            _ => None,
        };
        let completed = match answer {
            Some((true, bytes)) => Some(bytes),
            _ => None,
        };
        match ActivityKind(*kind) {
            wire::INFER => {
                if let Some(bytes) = completed {
                    frames.push(decision_frame(bytes, &mut repairs)?);
                }
            }
            wire::TOOL => {
                let request = ToolRequest::decode(input).map_err(corrupt)?;
                if request.attempt == 1 {
                    frames.push(Frame::ToolIntent {
                        seq: request.seq,
                        tool: request.tool,
                        args: request.args.to_vec(),
                        digest: request.digest,
                        write: request.write,
                    });
                }
                if let Some(bytes) = completed {
                    let observed = Observed::decode(bytes).map_err(corrupt)?;
                    frames.push(Frame::ToolObservation {
                        seq: request.seq,
                        attempt: u32::from(request.attempt),
                        transient: observed.transient,
                        outcome: observed.outcome().to_vec(),
                        secret_digest: observed.secret_digest,
                    });
                }
            }
            wire::VERIFY => {
                if let Some(bytes) = completed {
                    frames.push(verification_frame(input, bytes)?);
                }
            }
            wire::ASK => {
                let request = AskRequest::decode(input).map_err(corrupt)?;
                frames.push(Frame::ApprovalRequest {
                    prompt: request.prompt.to_vec(),
                    schema: request.schema,
                });
                if let Some(bytes) = completed {
                    let input = AskInput::decode(bytes).map_err(corrupt)?;
                    frames.push(Frame::ApprovalDecision {
                        input: input.as_bytes().to_vec(),
                    });
                }
            }
            _ => {}
        }
    }
    if let Some((_, bytes)) = &journal.terminal {
        let record = TerminalRecord::decode(bytes).map_err(corrupt)?;
        frames.push(Frame::Terminal {
            status: record.status,
            reason: record.reason.to_vec(),
            summary: record.summary.to_vec(),
        });
    }
    Ok(frames)
}

/// The segment's per-observation secret digests and prompt bytes, from
/// its frames.
fn segment_context(frames: &[Frame]) -> (Vec<u64>, u64) {
    let mut digests = Vec::new();
    let mut prompt_bytes: u64 = 0;
    for frame in frames {
        match frame {
            Frame::ToolObservation { secret_digest, .. } => digests.push(*secret_digest),
            Frame::ModelDecision {
                prompt_bytes: bytes,
                ..
            } => prompt_bytes = prompt_bytes.saturating_add(*bytes),
            _ => {}
        }
    }
    (digests, prompt_bytes)
}

/// Encode the run's input for `seed` (and its parent fold).
fn run_start(
    seed: &RunSeed,
    snapshot: Option<&[u8]>,
    harvest: usize,
) -> Result<Vec<u8>, RuntimeError> {
    let snapshot = match snapshot {
        Some(bytes) => Some(SnapshotRef {
            digest: fnv1a64(bytes),
            len: u16::try_from(bytes.len()).map_err(|_| RuntimeError::BadFrame)?,
            harvest: u8::try_from(harvest).map_err(|_| RuntimeError::BadFrame)?,
        }),
        None => None,
    };
    let mut buf = [0u8; wire::MAX_RUN_START_BYTES];
    let n = RunStart {
        seed: *seed,
        snapshot,
    }
    .encode(&mut buf)?;
    // HOST-ONLY
    Ok(buf.get(..n).ok_or(RuntimeError::BadFrame)?.to_vec())
}

/// One injected reset at the `nth` new record a segment commits.
///
/// Counted from 1; replayed records do not count. The reset lands after
/// the record commits, or — with `before` — instead of committing it.
/// For an outcome record, `before` is a reset after the world acted and
/// before its answer was recorded. Used by exhaustive crash sweeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reset {
    /// Which new record, from 1.
    pub nth: usize,
    /// Reset before the record commits instead of after.
    pub before: bool,
}

/// Test hooks for one segment.
#[derive(Debug, Default)]
struct Hooks {
    reset: Option<Reset>,
    #[cfg(test)]
    injected_outcome: Option<Vec<u8>>,
}

impl Hooks {
    const fn with_reset(reset: Option<Reset>) -> Self {
        Self {
            reset,
            #[cfg(test)]
            injected_outcome: None,
        }
    }
}

/// Drive one segment of a run (E4, SPEC §20.4).
///
/// Binds a fresh journal to the seed (or checks a used journal's
/// binding), then boots the workflow: every boot replays the journal's
/// records and drives forward until the run concludes, suspends on an
/// unserviceable `ASK`, or continues as new. An injected crash (the
/// `crash` point) drops all in-memory state and reboots — transparently,
/// inside this call — with the crash point disarmed so it fires once.
///
/// A continued run (`seed.parent.is_some()`) must present the handoff;
/// a root run must not. A mismatch fails closed.
///
/// # Errors
///
/// [`RuntimeError`] on a malformed seed, a journal bound to another
/// seed, a corrupt parent fold, a recorded dispatcher fault, a replay
/// divergence, or an engine bug. An injected crash never surfaces here.
#[allow(clippy::too_many_arguments)]
pub fn drive_segment(
    seed: &RunSeed,
    journal: &mut Journal,
    model: &mut dyn ModelBackend,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    crash: Option<CrashPoint>,
    handoff: Option<RolloverHandoff>,
) -> Result<SegmentOutcome, RuntimeError> {
    let mut world = World {
        model,
        device,
        faults,
        inputs,
    };
    drive_with(
        seed,
        journal,
        &mut world,
        crash,
        handoff,
        &mut Hooks::default(),
    )
}

/// The world doubles one segment drives against.
struct World<'w> {
    model: &'w mut dyn ModelBackend,
    device: &'w mut FakeDevice,
    faults: &'w mut FaultPlan,
    inputs: &'w mut InputPlan,
}

/// Bind `journal` to `seed`: a fresh journal takes the run's input and
/// the parent fold's chunks; a used one must already carry them.
fn bind(
    seed: &RunSeed,
    journal: &mut Journal,
    snapshot: Option<&[u8]>,
    harvest: &[Vec<u8>],
) -> Result<Vec<u8>, RuntimeError> {
    let input = run_start(seed, snapshot, harvest.len())?;
    if journal.is_empty() {
        // HOST-ONLY
        journal.input.clone_from(&input);
        if let Some(snapshot) = snapshot {
            let digest = fnv1a64(snapshot);
            for (index, chunk) in snapshot.chunks(wire::SNAPSHOT_CHUNK).enumerate() {
                let index = u8::try_from(index).map_err(|_| RuntimeError::BadFrame)?;
                journal.store.insert((digest, index), chunk.to_vec());
            }
            for (index, item) in harvest.iter().enumerate() {
                let index = u8::try_from(index)
                    .ok()
                    .and_then(|i| wire::HARVEST_ITEM_BASE.checked_add(i))
                    .ok_or(RuntimeError::BadFrame)?;
                journal.store.insert((digest, index), item.clone());
            }
        }
    } else if journal.input != input {
        let bound = RunStart::decode(&journal.input).map_err(|_| RuntimeError::JournalCorrupt)?;
        return Err(if bound.seed.workflow_version == seed.workflow_version {
            RuntimeError::SeedMismatch
        } else {
            RuntimeError::IncompatibleVersion
        });
    }
    Ok(input)
}

/// How one boot ended.
enum Boot {
    /// An injected reset: boot again.
    Reset,
    /// The workflow concluded with this terminal record.
    Concluded(bool, Vec<u8>),
    /// The workflow continued as new, or suspended on `ASK`.
    Stopped,
}

/// Run one boot of the workflow: a fresh `RunState`, a replay of the
/// journal, and forward progress until the workflow stops.
fn boot(
    seed: &RunSeed,
    input: &[u8],
    journal: &mut Journal,
    world: &mut World<'_>,
    crash: &Cell<Option<CrashPoint>>,
    hooks: &mut Hooks,
) -> Result<Boot, RuntimeError> {
    let stop = Cell::new(None);
    let unemit = Cell::new(false);
    let reset = Cell::new(hooks.reset);
    let lost = Cell::new(false);
    let mut out = [0u8; PAGE_RECORD_MAX];
    // HOST-ONLY: the caller-owned state lives on the heap here; on the
    // device it is a `static`.
    let mut state = Box::new(RunState::new());
    // HOST-ONLY: the snapshot store survives a reset like flash; it is
    // lent to the dispatcher for the boot.
    let mut store = core::mem::take(&mut journal.store);
    journal.boots += 1;
    let mut records = RunJournal {
        journal: &mut *journal,
        run: seed.id.get(),
        crash,
        reset: &reset,
        lost: &lost,
        stop: &stop,
        unemit: &unemit,
        pos: 0,
    };
    let mut ports = HostPorts {
        model: &mut *world.model,
        device: &mut *world.device,
        faults: &mut *world.faults,
        inputs: &mut *world.inputs,
        store: &mut store,
        #[cfg(test)]
        injected_outcome: hooks.injected_outcome.take(),
    };
    let mut dispatcher = RunDispatcher::new(&mut ports, seed.id);
    let (polled, concluded, unserviceable) = {
        let mut ctx = Ctx::new(&mut records, &mut dispatcher, &mut out);
        let polled = {
            let mut workflow = pin!(super::run(&mut ctx, input, &mut state));
            let mut task = Context::from_waker(Waker::noop());
            workflow.as_mut().poll(&mut task)
        };
        let concluded = match ctx.conclusion() {
            Some(Conclusion::Ended(Outcome::Completed(bytes))) => Some(Ok((true, bytes.to_vec()))),
            Some(Conclusion::Ended(Outcome::Failed(bytes))) => Some(Ok((false, bytes.to_vec()))),
            Some(Conclusion::Refused) => Some(Err(RuntimeError::BadTerminal)),
            None => None,
        };
        (polled, concluded, ctx.unserviceable())
    };
    #[cfg(test)]
    {
        hooks.injected_outcome = ports.injected_outcome.take();
    }
    hooks.reset = reset.get();
    let requeue = lost.get();
    journal.store = store;
    if requeue {
        world.inputs.untake();
    }
    if unemit.get() {
        world.model.unemit();
    }
    if let Poll::Ready(Err(error)) = polled {
        return Err(error);
    }
    match stop.get() {
        Some(Stop::Diverged) => return Err(RuntimeError::ReplayDiverged),
        Some(Stop::Crash) => return Ok(Boot::Reset),
        None => {}
    }
    if let Some((completed, bytes)) = concluded.transpose()? {
        return Ok(Boot::Concluded(completed, bytes));
    }
    if journal.continued.is_some() || unserviceable {
        return Ok(Boot::Stopped);
    }
    Err(RuntimeError::IllegalTransition)
}

fn drive_with(
    seed: &RunSeed,
    journal: &mut Journal,
    world: &mut World<'_>,
    crash: Option<CrashPoint>,
    handoff: Option<RolloverHandoff>,
    hooks: &mut Hooks,
) -> Result<SegmentOutcome, RuntimeError> {
    seed.validate().map_err(RuntimeError::SeedInvalid)?;
    if handoff.is_some() != seed.parent.is_some() {
        return Err(RuntimeError::JournalCorrupt);
    }
    let (inherited, snapshot, harvest) = handoff.map_or((Vec::new(), None, Vec::new()), |h| {
        (h.mask_digests, Some(h.snapshot), h.harvest)
    });
    let input = bind(seed, journal, snapshot.as_deref(), &harvest)?;
    let crash = Cell::new(crash);
    // After a reset that followed the terminal record, the next boot
    // replays the whole run and must conclude the same way.
    let mut confirm = false;
    let mut settled = false;
    for _ in 0..MAX_BOOTS {
        if journal.terminal.is_some() && !confirm {
            settled = true;
            break;
        }
        match boot(seed, &input, journal, world, &crash, hooks)? {
            Boot::Reset => {}
            Boot::Concluded(completed, bytes) => {
                if let Some(recorded) = journal.terminal.as_ref() {
                    if *recorded != (completed, bytes) {
                        return Err(RuntimeError::ReplayDiverged);
                    }
                    settled = true;
                    break;
                }
                if crash.get() == Some(CrashPoint::BeforeTerminalCommit) {
                    crash.set(None);
                    continue;
                }
                // HOST-ONLY
                journal.terminal = Some((completed, bytes));
                if crash.get() == Some(CrashPoint::AfterTerminalCommit) {
                    crash.set(None);
                    confirm = true;
                    continue;
                }
                settled = true;
                break;
            }
            Boot::Stopped => {
                settled = true;
                break;
            }
        }
    }
    if !settled {
        return Err(RuntimeError::TooManyBoots);
    }
    finish(seed, journal, inherited)
}

/// Derive the segment's frames and trace, and say how it ended.
fn finish(
    seed: &RunSeed,
    journal: &mut Journal,
    inherited: Vec<u64>,
) -> Result<SegmentOutcome, RuntimeError> {
    journal.frames = derive_frames(journal)?;
    let (digests, prompt_bytes) = segment_context(&journal.frames);
    let mut mask_digests = inherited;
    mask_digests.extend_from_slice(&digests);
    let mut trace = RunTrace::from_journal(journal, seed)?;
    trace.attach_context(mask_digests.clone(), prompt_bytes);
    if journal.terminal.is_some() {
        return Ok(SegmentOutcome::Completed(trace));
    }
    let Some(continued) = journal.continued.as_ref() else {
        return if journal_suspended(journal) {
            Ok(SegmentOutcome::Suspended(trace))
        } else {
            Err(RuntimeError::IllegalTransition)
        };
    };
    let start = RunStart::decode(continued)?;
    let lineage = start.seed.parent.ok_or(RuntimeError::JournalCorrupt)?;
    let snapshot = start.snapshot.ok_or(RuntimeError::JournalCorrupt)?;
    let mut bytes = Vec::with_capacity(usize::from(snapshot.len));
    for index in 0..snapshot.chunks() {
        let index = u8::try_from(index).map_err(|_| RuntimeError::JournalCorrupt)?;
        let chunk = journal
            .store
            .get(&(snapshot.digest, index))
            .ok_or(RuntimeError::JournalCorrupt)?;
        bytes.extend_from_slice(chunk);
    }
    let mut harvest = Vec::new();
    for index in 0..snapshot.harvest {
        let index = wire::HARVEST_ITEM_BASE
            .checked_add(index)
            .ok_or(RuntimeError::JournalCorrupt)?;
        let item = journal
            .store
            .get(&(snapshot.digest, index))
            .ok_or(RuntimeError::JournalCorrupt)?;
        harvest.push(item.clone());
    }
    Ok(SegmentOutcome::Rollover(
        trace,
        RolloverHandoff {
            snapshot: bytes,
            lineage,
            prompt_bytes,
            mask_digests,
            harvest,
        },
    ))
}

/// Whether the last record is an `ASK` intent with no outcome.
fn journal_suspended(journal: &Journal) -> bool {
    matches!(
        journal.records.last(),
        Some(Record::Scheduled { kind, .. }) if ActivityKind(*kind) == wire::ASK
    )
}

/// Drive one run to its terminal result (or to durable suspension).
///
/// Chains [`drive_segment`] calls: when a segment continues as new, the
/// child's seed is derived from the handoff and the next segment starts
/// on a fresh journal — with the remaining budgets, never widened —
/// until the run completes or suspends. The crash point is armed once
/// per segment.
///
/// # Errors
///
/// Same as [`drive_segment`].
pub fn drive_run(
    seed: &RunSeed,
    journal: &mut Journal,
    model: &mut dyn ModelBackend,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    crash: Option<CrashPoint>,
) -> Result<RunTrace, RuntimeError> {
    let mut world = World {
        model,
        device,
        faults,
        inputs,
    };
    chain(seed, journal, &mut world, crash, None)
}

/// Drive one run with one injected [`Reset`] in its first segment: the
/// exhaustive crash sweep's entry point.
///
/// # Errors
///
/// Same as [`drive_segment`].
pub fn drive_run_with_reset(
    seed: &RunSeed,
    journal: &mut Journal,
    model: &mut dyn ModelBackend,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    reset: Reset,
) -> Result<RunTrace, RuntimeError> {
    let mut world = World {
        model,
        device,
        faults,
        inputs,
    };
    chain(seed, journal, &mut world, None, Some(reset))
}

fn chain(
    seed: &RunSeed,
    journal: &mut Journal,
    world: &mut World<'_>,
    crash: Option<CrashPoint>,
    reset: Option<Reset>,
) -> Result<RunTrace, RuntimeError> {
    // HOST-ONLY
    let mut current: RunSeed = *seed;
    let mut handoff: Option<RolloverHandoff> = None;
    let mut child_journal = Journal::new();
    let mut hooks = Hooks::with_reset(reset);
    let mut first = true;
    loop {
        let target = if first {
            &mut *journal
        } else {
            &mut child_journal
        };
        first = false;
        match drive_with(&current, target, world, crash, handoff, &mut hooks)? {
            SegmentOutcome::Completed(trace) | SegmentOutcome::Suspended(trace) => {
                return Ok(trace);
            }
            SegmentOutcome::Rollover(_, next) => {
                current = next.child_seed(&current);
                handoff = Some(next);
                child_journal = Journal::new();
                hooks.reset = None;
            }
        }
    }
}

/// Drive one run on the Tokio host.
///
/// // HOST-ONLY: runs [`drive_run`] via `tokio::task::block_in_place`
/// so the reactor never stalls. Requires a multi-threaded runtime.
///
/// # Errors
///
/// Same as [`drive_run`].
#[allow(clippy::unused_async)]
pub async fn drive_run_async(
    seed: &RunSeed,
    journal: &mut Journal,
    model: &mut dyn ModelBackend,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    crash: Option<CrashPoint>,
) -> Result<RunTrace, RuntimeError> {
    tokio::task::block_in_place(|| drive_run(seed, journal, model, device, faults, inputs, crash))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend::{InferenceSettings, ScriptedBackend};
    use esper_core::mask::{mask_bound, mask_report};

    /// SPEC §20.1-20.2: a secret-bearing device outcome is masked at the
    /// dispatcher — the recorded answer, the derived frame and the next
    /// prompt all carry only masked bytes, and the trace records the
    /// secret's digest.
    #[test]
    fn masking_boundary_redacts_secret_and_pii_before_the_record() {
        let mut raw = Vec::new();
        for part in [
            &b"token="[..],
            b"sk",
            b"-live-",
            b"abc123def456xyz",
            b"; notify ",
        ] {
            raw.extend_from_slice(part);
        }
        raw.extend_from_slice(b"nurse@example.com");
        let mut backend = ScriptedBackend::new(
            vec![
                b"CALL gpio_pin_read {\"pin\": 4}".to_vec(),
                b"FINISH {\"status\": \"completed\", \"summary\": \"done\"}".to_vec(),
            ],
            InferenceSettings::default_settings(),
        );
        let seed = RunSeed {
            model_bundle: backend.bundle_id().0,
            ..RunSeed::default_slice()
        };
        let mut journal = Journal::new();
        let mut device = FakeDevice::new();
        let mut faults = FaultPlan::new();
        let mut inputs = InputPlan::new(Vec::new());
        let mut world = World {
            model: &mut backend,
            device: &mut device,
            faults: &mut faults,
            inputs: &mut inputs,
        };
        let mut hooks = Hooks {
            injected_outcome: Some(raw.clone()),
            ..Hooks::default()
        };
        let outcome = drive_with(&seed, &mut journal, &mut world, None, None, &mut hooks).unwrap();
        let SegmentOutcome::Completed(trace) = outcome else {
            panic!("expected the run to complete");
        };
        let mut buf = vec![0u8; mask_bound(raw.len())];
        let report = mask_report(&raw, &mut buf).unwrap();
        let expected = &buf[..report.output_len];
        let committed = journal
            .frames()
            .iter()
            .find_map(|frame| match frame {
                Frame::ToolObservation { outcome, .. } => Some(outcome),
                _ => None,
            })
            .unwrap();
        assert_eq!(committed, expected, "the record holds masked bytes only");
        // The second inference's recorded context carries the masked
        // observation, not the raw one.
        let infer_inputs: Vec<&Vec<u8>> = journal
            .records()
            .iter()
            .filter_map(|record| match record {
                Record::Scheduled { kind, input, .. } if ActivityKind(*kind) == wire::INFER => {
                    Some(input)
                }
                _ => None,
            })
            .collect();
        let second = wire::InferRequest::decode(infer_inputs[1]).unwrap();
        assert_eq!(
            second.last_observation,
            Some(&expected[..expected.len().min(128)])
        );
        assert_ne!(report.secret_digest, 0);
        assert_eq!(trace.secret_digests(), &[report.secret_digest]);
        for record in journal.records() {
            let bytes = match record {
                Record::Scheduled { input, .. } => input,
                Record::Resolved { bytes, .. } => bytes,
            };
            assert!(
                !bytes.windows(15).any(|w| w == b"abc123def456xyz"),
                "a raw secret reached a record"
            );
        }
    }
}
