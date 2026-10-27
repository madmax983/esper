//! HOST-ONLY (E4+aon): test doubles for the supervisor workflow.
//!
//! Nothing here is product logic. The supervisor ([`super::supervise`])
//! is `no_std` and runs on the device; these doubles stand in for the
//! device's half so the host can drive it:
//!
//! * [`MemJournal`] — an in-memory `waymaker_embassy::Journal`: replay
//!   from the start on every boot, redelivery of an intent with no
//!   outcome under its original `EffectId`, boot-clock and
//!   persistent-clock deadlines, `continue_as_new`, and fault injection
//!   at four boundary kinds ([`CrashAt`]). The device's journal is
//!   Waymaker's flash-backed driver.
//! * [`HostAgent`] — the dispatcher: senses scheduled operator
//!   commands and input deliveries, drives goal runs through
//!   [`crate::drive_segment`] over the world doubles, harvests with
//!   [`esper_core::harvest::Harvester`], and keeps checkpoint items.
//! * [`drive`] — polls the workflow with a no-op waker, the way an
//!   Embassy executor would, advancing the clocks whenever the
//!   heartbeat deadline has not passed, and rebooting on an injected
//!   crash or a `continue_as_new`.

use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use esper_core::beat::Beat;
use esper_core::goal::GoalId;
use esper_core::harvest::{HarvestSet, Harvester};
use esper_core::ids::{RunId as EsperRunId, ToolId};
use esper_core::memory::Provenance;
use waymaker_core::timer::{ClockKind, TimerSpec};
use waymaker_core::{ActivityKind, EffectId, EffectSeq, Outcome, RunId};
use waymaker_embassy::dispatch::Produced;
use waymaker_embassy::{ActivityDispatcher, Answer, Ctx, Halted, Handoff, Journal, NoAlarm};

use super::wire::{self, Command, HarvestEntry, PAGE_RECORD_MAX, RunReport, RunRequest, Sense};
use super::{AgentError, AgentSeed, AgentState, Pulse, PulseSink, supervise};
use crate::backend::ModelBackend;
use crate::engine::{RolloverHandoff, SegmentOutcome, drive_segment};
use crate::error::CrashPoint;
use crate::journal::Journal as RunJournal;
use crate::seed::RunSeed;
use crate::trace::{RunTrace, TraceEvent};
use crate::world::{FakeDevice, FaultPlan, InputPlan};

/// Why the last poll of the workflow stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stop {
    /// The heartbeat deadline has not passed yet.
    Timer,
    /// An injected crash fired: the boot is over.
    Crash,
    /// The run continued as new: reboot with its new input.
    Continued,
    /// History disagrees with what the workflow asked for.
    Diverged,
}

/// The clocks and the stop signal, shared by the journal double, the
/// dispatcher double, and the driver.
#[derive(Debug, Default)]
pub struct Signals {
    boot: Cell<u64>,
    persistent: Cell<u64>,
    stop: Cell<Option<Stop>>,
}

impl Signals {
    /// Fresh clocks at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            boot: Cell::new(0),
            persistent: Cell::new(0),
            stop: Cell::new(None),
        }
    }

    /// The persistent clock's reading.
    #[must_use]
    pub const fn persistent(&self) -> u64 {
        self.persistent.get()
    }

    /// Advance both clocks by `ticks`.
    pub fn advance(&self, ticks: u64) {
        self.boot.set(self.boot.get().saturating_add(ticks));
        self.persistent
            .set(self.persistent.get().saturating_add(ticks));
    }

    fn stop(&self, why: Stop) {
        // The first reason wins: a crash during a timer wait is a crash.
        if self.stop.get().is_none() {
            self.stop.set(Some(why));
        }
    }
}

/// Where to inject one reset, by boundary kind and the `n`-th new
/// commit of that kind (1-based; replayed boundaries do not count).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CrashAt {
    /// After an activity intent commits, before dispatch.
    AfterSchedule(usize),
    /// After dispatch, before the outcome commits: the effect happened
    /// and its outcome is lost.
    BeforeResolve(usize),
    /// After the outcome commits, before the workflow sees it.
    AfterResolve(usize),
    /// After a heartbeat deadline is armed.
    AfterTimerArmed(usize),
}

impl CrashAt {
    const fn kind(self) -> usize {
        match self {
            Self::AfterSchedule(_) => 0,
            Self::BeforeResolve(_) => 1,
            Self::AfterResolve(_) => 2,
            Self::AfterTimerArmed(_) => 3,
        }
    }

    const fn nth(self) -> usize {
        match self {
            Self::AfterSchedule(n)
            | Self::BeforeResolve(n)
            | Self::AfterResolve(n)
            | Self::AfterTimerArmed(n) => n,
        }
    }
}

/// One record of the in-memory journal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    /// An activity intent.
    Scheduled {
        /// The activity kind.
        kind: u16,
        /// The activity input.
        // HOST-ONLY (E4+aon)
        input: Vec<u8>,
        /// The effect sequence.
        seq: u32,
    },
    /// An activity outcome.
    Resolved {
        /// Whether the effect completed (else failed).
        completed: bool,
        /// The outcome bytes.
        // HOST-ONLY (E4+aon)
        bytes: Vec<u8>,
    },
    /// A deadline intent.
    TimerArmed {
        /// The deadline.
        spec: TimerSpec,
    },
    /// The deadline passed.
    TimerFired,
}

/// An in-memory `waymaker_embassy::Journal` double. What it holds
/// survives a "reset"; only the cursor and the boot-clock arming are
/// RAM.
#[derive(Debug)]
pub struct MemJournal<'s> {
    signals: &'s Signals,
    run: u64,
    // HOST-ONLY (E4+aon)
    input: Vec<u8>,
    // HOST-ONLY (E4+aon)
    records: Vec<Record>,
    continued: usize,
    crash: Option<CrashAt>,
    commits: [usize; 4],
    // Per-boot RAM.
    pos: usize,
    armed_at: Option<u64>,
    remaining: Option<(ClockKind, u64)>,
}

impl<'s> MemJournal<'s> {
    /// A journal whose first run starts over `input`.
    #[must_use]
    pub fn new(signals: &'s Signals, input: &[u8]) -> Self {
        Self {
            signals,
            run: 1,
            input: input.to_vec(),
            records: Vec::new(),
            continued: 0,
            crash: None,
            commits: [0; 4],
            pos: 0,
            armed_at: None,
            remaining: None,
        }
    }

    /// Arm one crash.
    pub const fn crash_at(&mut self, crash: CrashAt) {
        self.crash = Some(crash);
    }

    /// New commits so far, per [`CrashAt`] kind, in declaration order.
    #[must_use]
    pub const fn commits(&self) -> [usize; 4] {
        self.commits
    }

    /// The current run's input.
    #[must_use]
    pub fn input(&self) -> &[u8] {
        &self.input
    }

    /// The current run's records.
    #[must_use]
    pub fn records(&self) -> &[Record] {
        &self.records
    }

    /// How many runs `continue_as_new` retired.
    #[must_use]
    pub const fn continued(&self) -> usize {
        self.continued
    }

    /// Reset the RAM half: the cursor, the boot clock, the arming.
    pub fn boot(&mut self) {
        self.pos = 0;
        self.armed_at = None;
        self.remaining = None;
        self.signals.boot.set(0);
    }

    fn next_seq(&self) -> u32 {
        let issued = self
            .records
            .iter()
            .filter(|r| matches!(r, Record::Scheduled { .. } | Record::TimerArmed { .. }))
            .count();
        u32::try_from(issued + 1).unwrap_or(u32::MAX)
    }

    /// Count a new commit of `kind`; fire the armed crash on its `n`-th.
    fn fire(&mut self, kind: usize) -> bool {
        if let Some(count) = self.commits.get_mut(kind) {
            *count += 1;
        }
        let hit = self
            .crash
            .is_some_and(|c| c.kind() == kind && self.commits.get(kind) == Some(&c.nth()));
        if hit {
            self.crash = None;
            self.signals.stop(Stop::Crash);
        }
        hit
    }

    fn diverged(&self) -> Halted {
        self.signals.stop(Stop::Diverged);
        Halted
    }

    const fn id(&self, seq: u32) -> EffectId {
        EffectId {
            run: RunId(self.run),
            seq: EffectSeq(seq),
        }
    }
}

impl Journal for MemJournal<'_> {
    fn schedule(&mut self, kind: ActivityKind, input: &[u8]) -> Result<Handoff<'_>, Halted> {
        self.remaining = None;
        if self.pos < self.records.len() {
            let Some(Record::Scheduled {
                kind: k,
                input: i,
                seq,
            }) = self.records.get(self.pos)
            else {
                return Err(self.diverged());
            };
            if *k != kind.0 || i != input {
                return Err(self.diverged());
            }
            let seq = *seq;
            self.pos += 1;
            return match self.records.get(self.pos) {
                // An intent with no outcome: redeliver under the same id.
                None => Ok(Handoff::Dispatch {
                    id: self.id(seq),
                    result_bytes: PAGE_RECORD_MAX,
                }),
                Some(Record::Resolved { .. }) => {
                    let at = self.pos;
                    self.pos += 1;
                    match self.records.get(at) {
                        Some(Record::Resolved {
                            completed: true,
                            bytes,
                        }) => Ok(Handoff::Replayed(Outcome::Completed(bytes))),
                        Some(Record::Resolved { bytes, .. }) => {
                            Ok(Handoff::Replayed(Outcome::Failed(bytes)))
                        }
                        _ => Err(Halted),
                    }
                }
                Some(_) => Err(self.diverged()),
            };
        }
        let seq = self.next_seq();
        // HOST-ONLY (E4+aon)
        self.records.push(Record::Scheduled {
            kind: kind.0,
            input: input.to_vec(),
            seq,
        });
        self.pos += 1;
        if self.fire(0) {
            return Err(Halted);
        }
        Ok(Handoff::Dispatch {
            id: self.id(seq),
            result_bytes: PAGE_RECORD_MAX,
        })
    }

    fn resolve(&mut self, answer: Answer<'_>) -> Result<Outcome<'_>, Halted> {
        if self.fire(1) {
            return Err(Halted);
        }
        let (completed, bytes) = match answer {
            Answer::Completed(bytes) => (true, bytes.to_vec()),
            Answer::Failed(bytes) => (false, bytes.to_vec()),
            Answer::Exhausted => (false, Vec::new()),
        };
        // HOST-ONLY (E4+aon)
        self.records.push(Record::Resolved { completed, bytes });
        self.pos += 1;
        if self.fire(2) {
            return Err(Halted);
        }
        match self.records.last() {
            Some(Record::Resolved {
                completed: true,
                bytes,
            }) => Ok(Outcome::Completed(bytes)),
            Some(Record::Resolved { bytes, .. }) => Ok(Outcome::Failed(bytes)),
            _ => Err(Halted),
        }
    }

    fn wait(&mut self, spec: TimerSpec) -> Result<(), Halted> {
        self.remaining = None;
        if self.pos < self.records.len() {
            // The cursor stays on an armed, unfired deadline: a retained
            // timer future asks again on every poll.
            if self.records.get(self.pos) != Some(&Record::TimerArmed { spec }) {
                return Err(self.diverged());
            }
            match self.records.get(self.pos + 1) {
                Some(Record::TimerFired) => {
                    self.pos += 2;
                    return Ok(());
                }
                None => {}
                Some(_) => return Err(self.diverged()),
            }
        } else {
            // HOST-ONLY (E4+aon)
            self.records.push(Record::TimerArmed { spec });
            self.armed_at = None;
            if self.fire(3) {
                return Err(Halted);
            }
        }
        let remaining = match spec {
            TimerSpec::AfterBoot { ticks } => {
                let now = self.signals.boot.get();
                let armed = *self.armed_at.get_or_insert(now);
                armed.saturating_add(ticks).saturating_sub(now)
            }
            TimerSpec::AtPersistentTime { instant } => {
                instant.saturating_sub(self.signals.persistent.get())
            }
        };
        if remaining == 0 {
            // HOST-ONLY (E4+aon)
            self.records.push(Record::TimerFired);
            self.pos += 2;
            self.armed_at = None;
            return Ok(());
        }
        self.remaining = Some((spec.clock_kind(), remaining));
        self.signals.stop(Stop::Timer);
        Err(Halted)
    }

    fn continue_as_new(&mut self, input: &[u8]) -> Halted {
        // HOST-ONLY (E4+aon)
        self.records.clear();
        self.input = input.to_vec();
        self.run += 1;
        self.continued += 1;
        self.pos = 0;
        self.signals.stop(Stop::Continued);
        Halted
    }

    fn deadline_remaining(&self) -> Option<(ClockKind, u64)> {
        self.remaining
    }
}

/// One goal run the host dispatcher owns: its segments and, once
/// settled, its harvest.
#[derive(Debug, Clone)]
pub struct HostRun {
    /// The goal served.
    pub goal: GoalId,
    /// `(seed, journal, handoff)` per segment, root first.
    // HOST-ONLY (E4+aon)
    pub segments: Vec<(RunSeed, RunJournal, Option<RolloverHandoff>)>,
    /// The harvest, once the run settled.
    pub harvest: Option<HarvestSet>,
}

/// The host dispatcher double: the device's half of the supervisor's
/// activities.
pub struct HostAgent<'w> {
    signals: &'w Signals,
    /// The model every goal run shares.
    pub model: &'w mut dyn ModelBackend,
    /// The device.
    pub device: &'w mut FakeDevice,
    /// The device fault plan.
    pub faults: &'w mut FaultPlan,
    // HOST-ONLY (E4+aon)
    inbox: BTreeMap<u16, InputPlan>,
    operator: BTreeMap<u64, Vec<Command>>,
    deliveries: BTreeMap<u64, Vec<(GoalId, Vec<u8>)>>,
    delivered: BTreeSet<u64>,
    runs: BTreeMap<u64, HostRun>,
    store: BTreeMap<(u64, u16), Vec<u8>>,
    run_crash: Option<CrashPoint>,
    errors: Vec<String>,
}

impl<'w> HostAgent<'w> {
    /// A dispatcher over the world doubles.
    pub fn new(
        signals: &'w Signals,
        model: &'w mut dyn ModelBackend,
        device: &'w mut FakeDevice,
        faults: &'w mut FaultPlan,
    ) -> Self {
        Self {
            signals,
            model,
            device,
            faults,
            inbox: BTreeMap::new(),
            operator: BTreeMap::new(),
            deliveries: BTreeMap::new(),
            delivered: BTreeSet::new(),
            runs: BTreeMap::new(),
            store: BTreeMap::new(),
            run_crash: None,
            errors: Vec::new(),
        }
    }

    /// Deliver `command` in the [`wire::SENSE`] answer of `beat`.
    pub fn command_at(&mut self, beat: u64, command: Command) {
        // HOST-ONLY (E4+aon)
        self.operator.entry(beat).or_default().push(command);
    }

    /// Queue `input` for `goal`'s parked run, arriving at `beat`.
    pub fn deliver_at(&mut self, beat: u64, goal: GoalId, input: &[u8]) {
        // HOST-ONLY (E4+aon)
        self.deliveries
            .entry(beat)
            .or_default()
            .push((goal, input.to_vec()));
    }

    /// Fire `point` inside the next goal run the dispatcher drives.
    pub const fn crash_next_run(&mut self, point: CrashPoint) {
        self.run_crash = Some(point);
    }

    /// The goal runs, by run id.
    #[must_use]
    pub fn run(&self, run: EsperRunId) -> Option<&HostRun> {
        self.runs.get(&run.get())
    }

    /// Runtime failures the dispatcher reported as failed activities.
    #[must_use]
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Stored checkpoint items.
    #[must_use]
    pub fn stored_items(&self) -> usize {
        self.store.len()
    }

    fn sense(&mut self, input: &[u8], out: &mut [u8]) -> Result<usize, String> {
        let beat =
            u64::from_le_bytes(<[u8; 8]>::try_from(input).map_err(|_| "sense input".to_string())?);
        // Deliveries land once, even if this SENSE is redelivered.
        if self.delivered.insert(beat) {
            for (goal, bytes) in self.deliveries.get(&beat).cloned().unwrap_or_default() {
                self.inbox.entry(goal.get()).or_default().push(bytes);
            }
        }
        let mut sense = Sense::at(self.signals.persistent());
        for (goal, plan) in &self.inbox {
            if plan.has_pending()
                && let Some(goal) = GoalId::new(*goal)
            {
                sense.ready(goal).map_err(|e| e.to_string())?;
            }
        }
        for command in self.operator.get(&beat).into_iter().flatten() {
            sense.command(*command).map_err(|e| e.to_string())?;
        }
        sense.encode(out).map_err(|e| e.to_string())
    }

    fn run_activity(&mut self, input: &[u8], out: &mut [u8]) -> Result<usize, String> {
        let request = RunRequest::decode(input).map_err(|e| e.to_string())?;
        let goal = request.goal();
        let id = request.run();
        if let RunRequest::Spawn { seed, .. } = request {
            // HOST-ONLY (E4+aon): a redelivered spawn finds its run.
            self.runs.entry(id.get()).or_insert_with(|| HostRun {
                goal,
                segments: vec![(seed, RunJournal::new(), None)],
                harvest: None,
            });
        }
        let record = self
            .runs
            .get_mut(&id.get())
            .ok_or_else(|| "resume of an unknown run".to_string())?;
        let inputs = self.inbox.entry(goal.get()).or_default();
        let mut crash = self.run_crash.take();
        let trace = loop {
            let (seed, journal, handoff) = record
                .segments
                .last_mut()
                .ok_or_else(|| "run without a segment".to_string())?;
            let outcome = drive_segment(
                seed,
                journal,
                &mut *self.model,
                self.device,
                self.faults,
                inputs,
                crash.take(),
                handoff.clone(),
            )
            .map_err(|e| e.to_string())?;
            match outcome {
                SegmentOutcome::Completed(trace) => break Some(trace),
                SegmentOutcome::Suspended(_) => break None,
                SegmentOutcome::Rollover(_, next) => {
                    let child = next.child_seed(seed);
                    // HOST-ONLY (E4+aon)
                    record.segments.push((child, RunJournal::new(), Some(next)));
                }
            }
        };
        let report = match trace {
            None => RunReport::Parked,
            Some(trace) => {
                let status = trace
                    .terminal_status()
                    .ok_or_else(|| "terminal without status".to_string())?;
                if record.harvest.is_none() {
                    record.harvest = Some(harvest(record, goal, id, &trace)?);
                }
                RunReport::Settled {
                    status,
                    refund_turns: trace.turns_remaining(),
                    refund_mutations: trace.mutations_remaining(),
                    harvest: record
                        .harvest
                        .as_ref()
                        .map_or(0, |h| u8::try_from(h.len()).unwrap_or(u8::MAX)),
                }
            }
        };
        report.encode(out).map_err(|e| e.to_string())
    }

    fn harvest_activity(&self, input: &[u8], out: &mut [u8]) -> Result<usize, String> {
        let (run, index) = wire::read_harvest_query(input).map_err(|e| e.to_string())?;
        let entry = self
            .runs
            .get(&run.get())
            .and_then(|r| r.harvest.as_ref())
            .and_then(|h| h.get(usize::from(index)))
            .ok_or_else(|| "no such harvest entry".to_string())?;
        HarvestEntry(*entry).encode(out).map_err(|e| e.to_string())
    }
}

/// Feed every segment's committed trace to a [`Harvester`].
fn harvest(
    record: &HostRun,
    goal: GoalId,
    run: EsperRunId,
    last: &RunTrace,
) -> Result<HarvestSet, String> {
    let mut harvester = Harvester::new(Provenance {
        goal: Some(goal),
        run: Some(run),
        beat: Beat::ZERO,
    });
    for (segment, (seed, journal, _)) in record.segments.iter().enumerate() {
        let trace = RunTrace::from_journal(journal, seed).map_err(|e| e.to_string())?;
        // Effect sequences restart per segment; keep them distinct.
        let base = u32::try_from(segment).unwrap_or(0) << 24;
        for event in trace.events() {
            match event {
                TraceEvent::ToolRequest { seq, tool, args } => {
                    harvester.request(base | seq, ToolId::new(*tool), args);
                }
                TraceEvent::ToolObservation { seq, ok, payload } => {
                    harvester.observation(base | seq, *ok, payload);
                }
                TraceEvent::Verification { seq, passed, .. } => {
                    harvester.verification(base | seq, *passed);
                }
                _ => {}
            }
        }
    }
    let status = last
        .terminal_status()
        .ok_or_else(|| "terminal without status".to_string())?;
    harvester
        .finish(goal, status, last.summary().unwrap_or(&[]))
        .map_err(|e| e.to_string())
}

impl ActivityDispatcher for HostAgent<'_> {
    type Error = ();

    fn poll_dispatch(
        &mut self,
        _task: &mut Context<'_>,
        _id: EffectId,
        kind: ActivityKind,
        input: &[u8],
        out: &mut [u8],
    ) -> Poll<Result<Produced, ()>> {
        let answered = match kind {
            wire::SENSE => self.sense(input, out),
            wire::RUN => self.run_activity(input, out),
            wire::HARVEST => self.harvest_activity(input, out),
            wire::STORE => wire::read_item_key(input).map_err(|e| e.to_string()).map(
                |(digest, index, item)| {
                    // HOST-ONLY (E4+aon)
                    self.store.insert((digest, index), item.to_vec());
                    0
                },
            ),
            wire::LOAD => wire::read_item_key(input)
                .map_err(|e| e.to_string())
                .and_then(|(digest, index, _)| {
                    let item = self
                        .store
                        .get(&(digest, index))
                        .ok_or_else(|| "no such checkpoint item".to_string())?;
                    out.get_mut(..item.len())
                        .ok_or_else(|| "item over the page".to_string())?
                        .copy_from_slice(item);
                    Ok(item.len())
                }),
            _ => return Poll::Ready(Ok(Produced::Unserviceable)),
        };
        Poll::Ready(match answered {
            Ok(len) => Ok(Produced::Completed(len)),
            Err(error) => {
                // HOST-ONLY (E4+aon)
                self.errors.push(error);
                Err(())
            }
        })
    }
}

/// Every pulse, keyed by beat (a replayed beat overwrites itself with
/// the same pulse).
#[derive(Debug, Default)]
pub struct PulseLog(RefCell<BTreeMap<u64, Pulse>>);

impl PulseLog {
    /// An empty log.
    #[must_use]
    pub const fn new() -> Self {
        Self(RefCell::new(BTreeMap::new()))
    }

    /// The pulses, in beat order.
    #[must_use]
    pub fn pulses(&self) -> Vec<Pulse> {
        self.0.borrow().values().copied().collect()
    }

    /// The last completed beat.
    #[must_use]
    pub fn last_beat(&self) -> u64 {
        self.0.borrow().keys().next_back().copied().unwrap_or(0)
    }
}

impl PulseSink for &PulseLog {
    fn pulse(&mut self, pulse: &Pulse) {
        // HOST-ONLY (E4+aon)
        self.0.borrow_mut().insert(pulse.beat.get(), *pulse);
    }
}

/// Why [`drive`] could not reach its target beat.
#[derive(Debug)]
pub enum DriveError {
    /// The workflow ended with a fault.
    Agent(AgentError),
    /// History disagreed with the workflow.
    Diverged,
    /// The workflow stopped for no reason the driver knows (an
    /// unserviceable activity).
    Stalled,
    /// Too many boots without reaching the target.
    TooManyBoots,
}

/// Poll the supervisor until `target` beats have completed.
///
/// Boots again after every injected crash and every `continue_as_new`,
/// and advances both clocks by `tick` whenever the heartbeat deadline
/// has not passed.
///
/// # Errors
///
/// [`DriveError`] when the workflow faults, diverges, stalls, or needs
/// more than 64 boots.
pub fn drive(
    seed: &AgentSeed,
    journal: &mut MemJournal<'_>,
    agent: &mut HostAgent<'_>,
    state: &mut AgentState,
    log: &PulseLog,
    target: u64,
    tick: u64,
) -> Result<(), DriveError> {
    let signals = journal.signals;
    for _ in 0..64 {
        journal.boot();
        signals.stop.set(None);
        let input = journal.input().to_vec();
        let mut out = [0u8; PAGE_RECORD_MAX];
        let mut sink = log;
        let mut alarm = NoAlarm;
        let mut ctx = Ctx::new(journal, agent, &mut out);
        let mut workflow = pin!(supervise(
            &mut ctx, seed, &input, &mut alarm, state, &mut sink
        ));
        let mut task = Context::from_waker(Waker::noop());
        loop {
            match workflow.as_mut().poll(&mut task) {
                Poll::Ready(Err(error)) => return Err(DriveError::Agent(error)),
                Poll::Ready(Ok(never)) => match never {},
                Poll::Pending => match signals.stop.take() {
                    Some(Stop::Timer) => {
                        if log.last_beat() >= target {
                            return Ok(());
                        }
                        signals.advance(tick);
                    }
                    Some(Stop::Crash | Stop::Continued) => break,
                    Some(Stop::Diverged) => return Err(DriveError::Diverged),
                    None => return Err(DriveError::Stalled),
                },
            }
        }
    }
    Err(DriveError::TooManyBoots)
}
