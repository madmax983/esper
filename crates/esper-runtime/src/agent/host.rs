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
//! * [`HostAgent`] — the product board dispatcher
//!   ([`super::device::Device`]) over host doubles: [`HostRuns`] (one
//!   in-memory journal per run, the run harness's crash points armed in
//!   the next spawned run) and [`HostPortsDevice`] (scheduled operator
//!   commands and input deliveries, the world doubles, the item store).
//! * [`drive`] — polls the workflow with a no-op waker, the way an
//!   Embassy executor would, advancing the clocks whenever the
//!   heartbeat deadline has not passed, and rebooting on an injected
//!   crash or a `continue_as_new`.

use core::convert::Infallible;
use core::pin::pin;
use core::task::{Context, Poll, Waker};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};

use esper_core::beat::Beat;
use esper_core::decision::ToolArgs;
use esper_core::goal::GoalId;
use esper_core::ids::{Pin, RunId as EsperRunId, ToolId};
use waymaker_core::timer::{ClockKind, TimerSpec};
use waymaker_core::{ActivityKind, EffectId, EffectSeq, Outcome, RunId};
use waymaker_embassy::dispatch::Produced;
use waymaker_embassy::{ActivityDispatcher, Answer, Ctx, Halted, Handoff, Journal, NoAlarm};

use super::device::{Device, DevicePorts, RunBody, RunStore, StoreFault};
use super::wire::{Command, PAGE_RECORD_MAX, Sense, WireError};
use super::{AgentError, AgentSeed, AgentState, Pulse, PulseSink, supervise};
use crate::backend::ModelBackend;
use crate::error::CrashPoint;
use crate::journal::Record as RunRecord;
use crate::run::RunState;
use crate::run::device::{Attempt, RunPorts};
use crate::run::host::{Boundary, HostPorts, crash_point};
use crate::run::wire::{Check, Fault, RunStart};
use crate::seed::RunSeed;
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

/// One supervised run's journal on the host: its current segment's
/// Waymaker records, every segment's seed, and its terminal record.
#[derive(Debug, Clone)]
pub struct HostRun {
    /// The goal served.
    pub goal: GoalId,
    /// Every segment's seed, root first: one more per `continue_as_new`.
    // HOST-ONLY (E4+aon)
    pub segments: Vec<RunSeed>,
    // HOST-ONLY (E4+aon)
    input: Vec<u8>,
    // HOST-ONLY (E4+aon)
    records: Vec<RunRecord>,
    // HOST-ONLY (E4+aon)
    terminal: Option<(bool, Vec<u8>)>,
    crash: Option<CrashPoint>,
}

/// The host's [`RunStore`]: one in-memory Waymaker journal per run, with
/// the run harness's crash points armed in the next spawned run. Its
/// own [`Journal`] implementation serves the selected run.
pub struct HostRuns<'s> {
    signals: &'s Signals,
    // HOST-ONLY (E4+aon)
    runs: BTreeMap<u64, HostRun>,
    selected: Option<u64>,
    pos: usize,
    armed: Option<CrashPoint>,
    /// An inference's answer was lost to a reset: the model re-emits.
    unemit: bool,
    /// Crashes fired inside runs.
    fired: usize,
}

impl HostRuns<'_> {
    fn run(&self) -> Option<&HostRun> {
        self.runs.get(&self.selected?)
    }

    fn run_mut(&mut self) -> Option<&mut HostRun> {
        self.runs.get_mut(&self.selected?)
    }

    /// Fire the selected run's armed crash if `point` matches it.
    fn fire(&mut self, point: Option<CrashPoint>) -> bool {
        let Some(run) = self.run_mut() else {
            return false;
        };
        if point.is_none() || run.crash != point {
            return false;
        }
        run.crash = None;
        self.fired += 1;
        if point == Some(CrashPoint::BeforeDecisionCommit) {
            self.unemit = true;
        }
        self.signals.stop(Stop::Crash);
        true
    }

    fn records(&self) -> &[RunRecord] {
        self.run().map_or(&[], |run| run.records.as_slice())
    }
}

impl RunStore for HostRuns<'_> {
    fn select(&mut self, run: EsperRunId, start: Option<&[u8]>) -> Result<(), StoreFault> {
        if !self.runs.contains_key(&run.get()) {
            let start = start.ok_or(StoreFault::Unknown)?;
            let seed = RunStart::decode(start)
                .map_err(|_| StoreFault::Unknown)?
                .seed;
            let goal = super::goal_of(run).ok_or(StoreFault::Unknown)?;
            // HOST-ONLY (E4+aon)
            self.runs.insert(
                run.get(),
                HostRun {
                    goal,
                    segments: vec![seed],
                    input: start.to_vec(),
                    records: Vec::new(),
                    terminal: None,
                    crash: self.armed.take(),
                },
            );
        }
        self.selected = Some(run.get());
        Ok(())
    }

    fn input(&self, out: &mut [u8]) -> Result<usize, StoreFault> {
        let input = &self.run().ok_or(StoreFault::Unknown)?.input;
        out.get_mut(..input.len())
            .ok_or(StoreFault::Unknown)?
            .copy_from_slice(input);
        Ok(input.len())
    }

    fn boot(&mut self, body: &mut dyn RunBody) -> Result<(), StoreFault> {
        self.pos = 0;
        let mut out = [0u8; PAGE_RECORD_MAX];
        // This store records terminal records itself, through `conclude`:
        // what the body returns is the dispatcher's to settle.
        let _end = body.drive(self, &mut out);
        Ok(())
    }

    fn terminal(&self, out: &mut [u8]) -> Option<(bool, usize)> {
        let (completed, bytes) = self.run()?.terminal.as_ref()?;
        out.get_mut(..bytes.len())?.copy_from_slice(bytes);
        Some((*completed, bytes.len()))
    }

    fn conclude(&mut self, completed: bool, record: &[u8]) -> Result<(), StoreFault> {
        if self.fire(Some(CrashPoint::BeforeTerminalCommit)) {
            return Err(StoreFault::Halted);
        }
        // HOST-ONLY (E4+aon)
        self.run_mut().ok_or(StoreFault::Unknown)?.terminal = Some((completed, record.to_vec()));
        if self.fire(Some(CrashPoint::AfterTerminalCommit)) {
            return Err(StoreFault::Halted);
        }
        Ok(())
    }
}

impl Journal for HostRuns<'_> {
    fn schedule(&mut self, kind: ActivityKind, input: &[u8]) -> Result<Handoff<'_>, Halted> {
        let run = self.selected.ok_or(Halted)?;
        let pos = self.pos;
        let records = self.records();
        if pos < records.len() {
            let Some(RunRecord::Scheduled {
                kind: k,
                input: i,
                seq,
            }) = records.get(pos)
            else {
                self.signals.stop(Stop::Diverged);
                return Err(Halted);
            };
            if *k != kind.0 || i != input {
                self.signals.stop(Stop::Diverged);
                return Err(Halted);
            }
            let seq = *seq;
            self.pos += 1;
            let id = EffectId {
                run: RunId(run),
                seq: EffectSeq(seq),
            };
            return match self.records().get(self.pos) {
                None => Ok(Handoff::Dispatch {
                    id,
                    result_bytes: PAGE_RECORD_MAX,
                }),
                Some(RunRecord::Resolved { .. }) => {
                    let at = self.pos;
                    self.pos += 1;
                    match self.records().get(at) {
                        Some(RunRecord::Resolved {
                            completed: true,
                            bytes,
                        }) => Ok(Handoff::Replayed(Outcome::Completed(bytes))),
                        Some(RunRecord::Resolved { bytes, .. }) => {
                            Ok(Handoff::Replayed(Outcome::Failed(bytes)))
                        }
                        _ => Err(Halted),
                    }
                }
                Some(RunRecord::Scheduled { .. }) => {
                    self.signals.stop(Stop::Diverged);
                    Err(Halted)
                }
            };
        }
        let seq = u32::try_from(
            records
                .iter()
                .filter(|r| matches!(r, RunRecord::Scheduled { .. }))
                .count()
                + 1,
        )
        .unwrap_or(u32::MAX);
        let record = self.run_mut().ok_or(Halted)?;
        // HOST-ONLY (E4+aon)
        record.records.push(RunRecord::Scheduled {
            kind: kind.0,
            input: input.to_vec(),
            seq,
        });
        self.pos += 1;
        let point = crash_point(self.records(), Boundary::AfterSchedule, None);
        if self.fire(point) {
            return Err(Halted);
        }
        Ok(Handoff::Dispatch {
            id: EffectId {
                run: RunId(run),
                seq: EffectSeq(seq),
            },
            result_bytes: PAGE_RECORD_MAX,
        })
    }

    fn resolve(&mut self, answer: Answer<'_>) -> Result<Outcome<'_>, Halted> {
        let point = crash_point(self.records(), Boundary::BeforeResolve, None);
        if self.fire(point) {
            return Err(Halted);
        }
        let (completed, bytes) = match answer {
            Answer::Completed(bytes) => (true, bytes.to_vec()),
            Answer::Failed(bytes) => (false, bytes.to_vec()),
            Answer::Exhausted => (false, Vec::new()),
        };
        let record = self.run_mut().ok_or(Halted)?;
        // HOST-ONLY (E4+aon)
        record.records.push(RunRecord::Resolved {
            completed,
            bytes: bytes.clone(),
        });
        self.pos += 1;
        if completed {
            let point = crash_point(self.records(), Boundary::AfterResolve, Some(&bytes));
            if self.fire(point) {
                return Err(Halted);
            }
        }
        match self.records().last() {
            Some(RunRecord::Resolved {
                completed: true,
                bytes,
            }) => Ok(Outcome::Completed(bytes)),
            Some(RunRecord::Resolved { bytes, .. }) => Ok(Outcome::Failed(bytes)),
            _ => Err(Halted),
        }
    }

    fn wait(&mut self, _spec: TimerSpec) -> Result<(), Halted> {
        // A run keeps no deadlines.
        self.signals.stop(Stop::Diverged);
        Err(Halted)
    }

    fn continue_as_new(&mut self, input: &[u8]) -> Halted {
        let seed = RunStart::decode(input).map(|start| start.seed);
        if let (Some(run), Ok(seed)) = (self.run_mut(), seed) {
            // HOST-ONLY (E4+aon)
            run.records.clear();
            run.input = input.to_vec();
            run.segments.push(seed);
        }
        self.pos = 0;
        Halted
    }

    fn deadline_remaining(&self) -> Option<(ClockKind, u64)> {
        None
    }
}

/// The host's [`DevicePorts`]: the world doubles, the operator's
/// schedule, and the item and snapshot stores.
pub struct HostPortsDevice<'w> {
    signals: &'w Signals,
    model: &'w mut dyn ModelBackend,
    device: &'w mut FakeDevice,
    faults: &'w mut FaultPlan,
    // HOST-ONLY (E4+aon)
    inbox: BTreeMap<u16, InputPlan>,
    operator: BTreeMap<u64, Vec<Command>>,
    deliveries: BTreeMap<u64, Vec<(GoalId, Vec<u8>)>>,
    delivered: BTreeSet<u64>,
    items: BTreeMap<(u64, u16), Vec<u8>>,
    chunks: BTreeMap<(u64, u8), Vec<u8>>,
    goal: Option<GoalId>,
    /// An empty inbox, for a run whose goal has none.
    none: InputPlan,
}

impl HostPortsDevice<'_> {
    /// The run harness's ports over the same doubles, with the selected
    /// goal's inbox.
    fn ports(&mut self) -> HostPorts<'_> {
        let inputs = match self.goal {
            Some(goal) => self.inbox.entry(goal.get()).or_default(),
            None => &mut self.none,
        };
        HostPorts {
            model: &mut *self.model,
            device: &mut *self.device,
            faults: &mut *self.faults,
            inputs,
            store: &mut self.chunks,
            #[cfg(test)]
            injected_outcome: None,
        }
    }
}

impl RunPorts for HostPortsDevice<'_> {
    fn model(&mut self) -> &mut dyn ModelBackend {
        &mut *self.model
    }

    fn tool(
        &mut self,
        effect: EffectId,
        tool: ToolId,
        digest: u64,
        args: ToolArgs,
        raw: &mut [u8],
    ) -> Result<Attempt, Fault> {
        self.ports().tool(effect, tool, digest, args, raw)
    }

    fn pin_is_output(&mut self, pin: Pin) -> bool {
        self.ports().pin_is_output(pin)
    }

    fn read_back(&mut self, check: Check) -> u64 {
        self.ports().read_back(check)
    }

    fn operator_input(&mut self, out: &mut [u8]) -> Result<Option<usize>, Fault> {
        self.ports().operator_input(out)
    }

    fn store_chunk(&mut self, digest: u64, index: u8, chunk: &[u8]) -> Result<(), Fault> {
        self.ports().store_chunk(digest, index, chunk)
    }

    fn load_chunk(&mut self, digest: u64, index: u8, out: &mut [u8]) -> Result<usize, Fault> {
        self.ports().load_chunk(digest, index, out)
    }
}

impl DevicePorts for HostPortsDevice<'_> {
    fn sense(&mut self, beat: Beat) -> Result<Sense, WireError> {
        let beat = beat.get();
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
                sense.ready(goal)?;
            }
        }
        for command in self.operator.get(&beat).into_iter().flatten() {
            sense.command(*command)?;
        }
        Ok(sense)
    }

    fn select_goal(&mut self, goal: GoalId) {
        self.goal = Some(goal);
    }

    fn store_item(&mut self, digest: u64, index: u16, item: &[u8]) -> Result<(), StoreFault> {
        // HOST-ONLY (E4+aon)
        self.items.insert((digest, index), item.to_vec());
        Ok(())
    }

    fn load_item(&mut self, digest: u64, index: u16, out: &mut [u8]) -> Result<usize, StoreFault> {
        let item = self
            .items
            .get(&(digest, index))
            .ok_or(StoreFault::Unknown)?;
        out.get_mut(..item.len())
            .ok_or(StoreFault::Unknown)?
            .copy_from_slice(item);
        Ok(item.len())
    }
}

/// The host dispatcher double: the product board dispatcher
/// ([`Device`]) over the host's [`HostRuns`] and [`HostPortsDevice`].
pub struct HostAgent<'w> {
    ports: HostPortsDevice<'w>,
    runs: HostRuns<'w>,
    // HOST-ONLY (E4+aon): a `static` on the device.
    state: Box<RunState>,
    // HOST-ONLY (E4+aon)
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
            ports: HostPortsDevice {
                signals,
                model,
                device,
                faults,
                inbox: BTreeMap::new(),
                operator: BTreeMap::new(),
                deliveries: BTreeMap::new(),
                delivered: BTreeSet::new(),
                items: BTreeMap::new(),
                chunks: BTreeMap::new(),
                goal: None,
                none: InputPlan::new(Vec::new()),
            },
            runs: HostRuns {
                signals,
                runs: BTreeMap::new(),
                selected: None,
                pos: 0,
                armed: None,
                unemit: false,
                fired: 0,
            },
            state: Box::new(RunState::new()),
            errors: Vec::new(),
        }
    }

    /// Deliver `command` in the [`super::wire::SENSE`] answer of `beat`.
    pub fn command_at(&mut self, beat: u64, command: Command) {
        // HOST-ONLY (E4+aon)
        self.ports.operator.entry(beat).or_default().push(command);
    }

    /// Queue `input` for `goal`'s parked run, arriving at `beat`.
    pub fn deliver_at(&mut self, beat: u64, goal: GoalId, input: &[u8]) {
        // HOST-ONLY (E4+aon)
        self.ports
            .deliveries
            .entry(beat)
            .or_default()
            .push((goal, input.to_vec()));
    }

    /// Fire `point` inside the next goal run the dispatcher spawns.
    pub const fn crash_next_run(&mut self, point: CrashPoint) {
        self.runs.armed = Some(point);
    }

    /// The goal runs, by run id.
    #[must_use]
    pub fn run(&self, run: EsperRunId) -> Option<&HostRun> {
        self.runs.runs.get(&run.get())
    }

    /// Why activities failed, as the board dispatcher reported it.
    #[must_use]
    pub fn errors(&self) -> &[String] {
        &self.errors
    }

    /// Crashes [`HostAgent::crash_next_run`] fired inside runs.
    #[must_use]
    pub const fn run_crashes(&self) -> usize {
        self.runs.fired
    }

    /// Stored checkpoint items.
    #[must_use]
    pub fn stored_items(&self) -> usize {
        self.ports.items.len()
    }
}

impl ActivityDispatcher for HostAgent<'_> {
    type Error = Infallible;

    fn poll_dispatch(
        &mut self,
        task: &mut Context<'_>,
        id: EffectId,
        kind: ActivityKind,
        input: &[u8],
        out: &mut [u8],
    ) -> Poll<Result<Produced, Infallible>> {
        let (polled, error) = {
            let mut device = Device::new(&mut self.runs, &mut self.ports, &mut self.state);
            let polled = device.poll_dispatch(task, id, kind, input, out);
            (polled, device.last_error().map(ToString::to_string))
        };
        if let Some(error) = error {
            // HOST-ONLY (E4+aon)
            self.errors.push(error);
        }
        if self.runs.unemit {
            self.runs.unemit = false;
            self.ports.model.unemit();
        }
        polled
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::wire::{HarvestEntry, RunReport, RunRequest, harvest_query};
    use crate::agent::{AgentSeed, run_id};
    use crate::backend::{InferenceSettings, ScriptedBackend};
    use esper_core::brief::Brief;
    use esper_core::compact::Note;
    use esper_core::memory::{Memory, recall};
    use esper_core::state::TerminalStatus;
    use waymaker_embassy::Decode;

    const READ: &str = r#"CALL gpio_pin_read {"pin": 3}"#;
    const ASK: &str = r#"ASK {"prompt": "which pin?", "schema": 1}"#;
    const FINISH: &str = r#"FINISH {"status": "completed", "summary": "read pin 3"}"#;

    fn backend(script: &[&str]) -> ScriptedBackend {
        ScriptedBackend::new(
            script.iter().map(|line| line.as_bytes().to_vec()).collect(),
            InferenceSettings::briefed_settings(),
        )
    }

    fn spawn(goal: u16, model: &ScriptedBackend) -> Vec<u8> {
        let goal = GoalId::new(goal).unwrap();
        let mut seed = AgentSeed::default_slice().template;
        seed.id = run_id(goal, 0);
        seed.model_bundle = model.bundle_id().0;
        seed.brief = Some(Brief::compose(
            &Note::from_bytes(b"watch pin 3").unwrap(),
            &recall(&Memory::new(), goal),
        ));
        let mut buf = [0u8; PAGE_RECORD_MAX];
        let n = RunRequest::Spawn { goal, seed }.encode(&mut buf).unwrap();
        buf[..n].to_vec()
    }

    fn dispatch(agent: &mut HostAgent<'_>, kind: ActivityKind, input: &[u8]) -> Produced {
        let mut out = [0u8; PAGE_RECORD_MAX];
        let mut task = Context::from_waker(Waker::noop());
        let id = EffectId {
            run: RunId(1),
            seq: EffectSeq(1),
        };
        match agent.poll_dispatch(&mut task, id, kind, input, &mut out) {
            Poll::Ready(Ok(produced)) => produced,
            Poll::Ready(Err(never)) => match never {},
            Poll::Pending => panic!("the run halted"),
        }
    }

    fn report(agent: &mut HostAgent<'_>, input: &[u8]) -> RunReport {
        let mut out = [0u8; PAGE_RECORD_MAX];
        let mut task = Context::from_waker(Waker::noop());
        let id = EffectId {
            run: RunId(1),
            seq: EffectSeq(1),
        };
        let Poll::Ready(Ok(Produced::Completed(n))) =
            agent.poll_dispatch(&mut task, id, wire_run(), input, &mut out)
        else {
            panic!("RUN did not complete: {:?}", agent.errors());
        };
        RunReport::decode(&out[..n]).unwrap()
    }

    const fn wire_run() -> ActivityKind {
        crate::agent::wire::RUN
    }

    /// A redelivered `RUN` re-drives the same run from its own journal:
    /// the same report, with nothing dispatched again (the script holds
    /// one pass, so a second inference would fail).
    #[test]
    fn a_redelivered_run_settles_from_its_own_journal() {
        let signals = Signals::new();
        let mut model = backend(&[READ, FINISH]);
        let request = spawn(1, &model);
        let mut device = FakeDevice::new();
        let mut faults = FaultPlan::new();
        let mut agent = HostAgent::new(&signals, &mut model, &mut device, &mut faults);
        let first = report(&mut agent, &request);
        let RunReport::Settled {
            status, harvest, ..
        } = first
        else {
            panic!("expected a settled run");
        };
        assert_eq!(status, TerminalStatus::Completed);
        assert_eq!(harvest, 2, "one observation and the outcome");
        assert_eq!(report(&mut agent, &request), first);
        assert!(agent.errors().is_empty(), "{:?}", agent.errors());
    }

    /// `HARVEST` reads the concluded run's entries, in order; past the
    /// end it fails.
    #[test]
    fn harvest_reads_the_concluded_runs_entries() {
        let signals = Signals::new();
        let mut model = backend(&[READ, FINISH]);
        let request = spawn(1, &model);
        let mut device = FakeDevice::new();
        let mut faults = FaultPlan::new();
        let mut agent = HostAgent::new(&signals, &mut model, &mut device, &mut faults);
        let _ = report(&mut agent, &request);
        let run = run_id(GoalId::new(1).unwrap(), 0);
        let mut out = [0u8; PAGE_RECORD_MAX];
        let mut task = Context::from_waker(Waker::noop());
        let id = EffectId {
            run: RunId(1),
            seq: EffectSeq(1),
        };
        let mut keys = Vec::new();
        for index in 0..2 {
            let Poll::Ready(Ok(Produced::Completed(n))) = agent.poll_dispatch(
                &mut task,
                id,
                crate::agent::wire::HARVEST,
                &harvest_query(run, index),
                &mut out,
            ) else {
                panic!("HARVEST {index} failed");
            };
            keys.push(HarvestEntry::decode(&out[..n]).unwrap().0.key);
        }
        assert!(matches!(
            keys[0],
            esper_core::memory::MemoryKey::Observation { .. }
        ));
        assert!(matches!(
            keys[1],
            esper_core::memory::MemoryKey::Outcome { .. }
        ));
        assert_eq!(
            dispatch(
                &mut agent,
                crate::agent::wire::HARVEST,
                &harvest_query(run, 2)
            ),
            Produced::Failed(0)
        );
    }

    /// A parked run reports `Parked`; it has no harvest yet; once its
    /// input arrives, a `Resume` settles it.
    #[test]
    fn a_parked_run_resumes_when_its_input_arrives() {
        let signals = Signals::new();
        let mut model = backend(&[ASK, FINISH]);
        let request = spawn(2, &model);
        let mut device = FakeDevice::new();
        let mut faults = FaultPlan::new();
        let mut agent = HostAgent::new(&signals, &mut model, &mut device, &mut faults);
        assert_eq!(report(&mut agent, &request), RunReport::Parked);
        let goal = GoalId::new(2).unwrap();
        let run = run_id(goal, 0);
        assert_eq!(
            dispatch(
                &mut agent,
                crate::agent::wire::HARVEST,
                &harvest_query(run, 0)
            ),
            Produced::Failed(0),
            "a parked run has no harvest"
        );
        agent.deliver_at(1, goal, br#"{"pin": 4}"#);
        let _ = agent.ports.sense(Beat::new(1)).unwrap();
        let mut resume = [0u8; PAGE_RECORD_MAX];
        let n = RunRequest::Resume { goal, run }
            .encode(&mut resume)
            .unwrap();
        assert!(matches!(
            report(&mut agent, &resume[..n]),
            RunReport::Settled {
                status: TerminalStatus::Completed,
                ..
            }
        ));
    }

    /// Resuming a run the device never spawned fails, with a reason.
    #[test]
    fn resuming_an_unknown_run_fails() {
        let signals = Signals::new();
        let mut model = backend(&[]);
        let mut device = FakeDevice::new();
        let mut faults = FaultPlan::new();
        let mut agent = HostAgent::new(&signals, &mut model, &mut device, &mut faults);
        let goal = GoalId::new(3).unwrap();
        let mut resume = [0u8; PAGE_RECORD_MAX];
        let n = RunRequest::Resume {
            goal,
            run: run_id(goal, 0),
        }
        .encode(&mut resume)
        .unwrap();
        assert_eq!(
            dispatch(&mut agent, crate::agent::wire::RUN, &resume[..n]),
            Produced::Failed(0)
        );
        assert_eq!(agent.errors().len(), 1);
    }

    /// Replay that reaches a different ending than the one recorded
    /// fails closed instead of reporting either.
    #[test]
    fn a_terminal_that_disagrees_with_replay_fails_closed() {
        let signals = Signals::new();
        let mut model = backend(&[READ, FINISH]);
        let request = spawn(1, &model);
        let mut device = FakeDevice::new();
        let mut faults = FaultPlan::new();
        let mut agent = HostAgent::new(&signals, &mut model, &mut device, &mut faults);
        let _ = report(&mut agent, &request);
        let run = run_id(GoalId::new(1).unwrap(), 0);
        if let Some(record) = agent.runs.runs.get_mut(&run.get())
            && let Some((_, bytes)) = record.terminal.as_mut()
            && let Some(last) = bytes.last_mut()
        {
            *last ^= 0x01;
        }
        assert_eq!(
            dispatch(&mut agent, crate::agent::wire::RUN, &request),
            Produced::Failed(0)
        );
        assert!(
            agent.errors().iter().any(|e| e.contains("diverged")),
            "{:?}",
            agent.errors()
        );
    }
}
