//! The always-on supervisor (E4+aon, SPEC §21).
//!
//! A run is durable but finite. The supervisor is what stays on: it
//! holds standing [goals](esper_core::goal), [memory](esper_core::memory)
//! that outlives runs, and a durable [heartbeat](esper_core::beat). Each
//! beat it picks at most one goal, spawns (or resumes) one ordinary run
//! for it through [`drive_segment`], settles the run's terminal result
//! back into the goal, and harvests what the run saw into memory.
//!
//! The durability discipline is the run's, one level up:
//!
//! * The [`AgentStore`] is the only durable state: the supervisor
//!   journal ([`AgentFrame`]s) plus the run journals it owns.
//!   Everything in [`Supervisor`] is derived by replaying the frames.
//! * Every change commits its frame **before** it applies: the beat
//!   intent before scheduling, the spawn intent (with the run's full
//!   seed and envelope reservation) before the run's first boot, the
//!   settlement (with its harvest) before the beat completes.
//! * A crash anywhere ([`AgentCrashPoint`]) drops the supervisor; the
//!   next boot replays the store and finishes the *same* beat. A run
//!   that already reached its terminal result is re-driven only to
//!   replay that result (SPEC §11.3 item 6), never to re-execute it.
//!
//! [`checkpoint`](Supervisor::checkpoint) folds the journal into a
//! verified [`esper_core::checkpoint`] and drops settled run journals,
//! so an agent that runs forever stores a bounded amount.
//!
//! // HOST-ONLY (E4+aon): the store is heap-allocated; the firmware port
//! keeps the frame vocabulary and moves it to flash.

use std::collections::BTreeMap;
use std::time::Duration;

use esper_core::beat::{Beat, SchedulePolicy};
use esper_core::brief::Brief;
use esper_core::checkpoint::{self, AgentCheckpoint, MAX_CHECKPOINT_BYTES};
use esper_core::goal::{Goal, GoalCounts, GoalEvent, GoalId, GoalLedger, GoalSpec, Grant, Pick};
use esper_core::ids::{Digest, RunId, ToolId};
use esper_core::memory::{MEMORY_INPUT_MAX, Memory, MemoryEntry, MemoryKey, Provenance, recall};
use esper_core::registry::lookup_by_id;
use esper_core::state::TerminalStatus;
use esper_protocol::PermissionClass;
use thiserror::Error;

use crate::backend::{InferenceSettings, ModelBackend};
use crate::engine::{RolloverHandoff, SegmentOutcome, drive_segment};
use crate::error::{CrashPoint, RuntimeError};
use crate::journal::Journal;
use crate::seed::RunSeed;
use crate::trace::{RunTrace, TraceEvent};
use crate::world::{FakeDevice, FaultPlan, InputPlan};

/// Boots before [`drive_beat`] gives up: each injected crash disarms
/// itself, so one beat needs at most a handful.
const MAX_BOOTS: u32 = 16;

/// The always-on agent's identity: bound into the first journal frame,
/// so a reboot can never swap the template or the policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSeed {
    /// The agent identity.
    pub id: u64,
    /// The per-run template: workflow version, model bundle, inference
    /// settings, token and time grants, the rollover trigger, and the
    /// capability **ceiling** no goal may widen. The supervisor
    /// overrides the run id, turns, mutations, capabilities, and brief
    /// per spawn.
    pub template: RunSeed,
    /// Failure handling and the liveness threshold.
    pub policy: SchedulePolicy,
}

impl AgentSeed {
    /// A default agent: the E0/E1 slice template with the briefed
    /// prompt buffer, and the default scheduling policy.
    #[must_use]
    pub const fn default_slice() -> Self {
        let mut template = RunSeed::default_slice();
        template.inference = InferenceSettings::briefed_settings();
        Self {
            id: 1,
            template,
            policy: SchedulePolicy::DEFAULT,
        }
    }

    /// Validate the seed before the first boot.
    ///
    /// # Errors
    ///
    /// [`AgentError::SeedInvalid`] when the template carries a lineage
    /// or a brief (the supervisor owns both), fails run-seed validation,
    /// or the policy is malformed.
    pub fn validate(&self) -> Result<(), AgentError> {
        if self.template.parent.is_some() {
            return Err(AgentError::SeedInvalid("template must not carry a lineage"));
        }
        if self.template.brief.is_some() {
            return Err(AgentError::SeedInvalid("template must not carry a brief"));
        }
        self.template
            .inference
            .validate_briefed()
            .map_err(AgentError::SeedInvalid)?;
        // Validate everything else as a plain seed (capability shapes,
        // workflow version); the inference bound was checked above.
        RunSeed {
            model_turns: 1,
            inference: InferenceSettings::default_settings(),
            ..self.template
        }
        .validate()
        .map_err(AgentError::SeedInvalid)?;
        self.policy
            .validate()
            .map_err(|_| AgentError::SeedInvalid("schedule policy is malformed"))
    }
}

/// One committed supervisor record (SPEC §21.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentFrame {
    /// First boot: binds the journal to the agent seed.
    AgentStarted {
        /// The seed this journal is bound to.
        seed: AgentSeed,
    },
    /// The folded state of everything before it (SPEC §21.6). Only
    /// ever directly after [`AgentFrame::AgentStarted`].
    Checkpoint {
        /// The verified checkpoint bytes.
        // HOST-ONLY (E4+aon)
        bytes: Vec<u8>,
    },
    /// The operator added a goal (already masked and validated).
    GoalAdded {
        /// The admitted goal.
        goal: Goal,
    },
    /// The operator paused, unpaused, or retired a goal.
    GoalControlled {
        /// The goal.
        goal: GoalId,
        /// The operator event.
        event: GoalEvent,
        /// The beat the control takes effect at.
        beat: Beat,
    },
    /// The operator wrote a note to memory (already masked).
    MemoryNoted {
        /// The entry.
        entry: MemoryEntry,
    },
    /// The operator removed a note from memory.
    MemoryForgotten {
        /// The removed key.
        key: MemoryKey,
    },
    /// Beat intent, before scheduling.
    BeatStarted {
        /// The beat.
        beat: Beat,
    },
    /// Spawn intent, before the run's first boot.
    RunSpawned {
        /// The beat.
        beat: Beat,
        /// The goal served.
        goal: GoalId,
        /// The run's full seed, brief included: the run is always
        /// driven under this committed seed, never a re-composed one.
        seed: RunSeed,
        /// The envelope reservation (equals the seed's grants).
        reservation: Grant,
    },
    /// Resume intent, before the parked run's next boot.
    RunResumed {
        /// The beat.
        beat: Beat,
        /// The goal.
        goal: GoalId,
        /// The parked run.
        run: RunId,
    },
    /// The run suspended on `Ask`.
    RunParked {
        /// The beat.
        beat: Beat,
        /// The goal.
        goal: GoalId,
        /// The parked run.
        run: RunId,
    },
    /// The run reached its terminal result.
    RunSettled {
        /// The beat.
        beat: Beat,
        /// The goal.
        goal: GoalId,
        /// The settled run.
        run: RunId,
        /// Its terminal status.
        status: TerminalStatus,
        /// The unspent units returned to the envelope.
        refund: Grant,
        /// What the run taught the agent, committed so replay applies
        /// it and never recomputes it.
        // HOST-ONLY (E4+aon)
        harvest: Vec<MemoryEntry>,
    },
    /// The beat's work is durable.
    BeatCompleted {
        /// The beat.
        beat: Beat,
    },
}

/// One run segment the supervisor owns: the seed it boots under, its
/// journal, and (for a rolled-over child) the verified parent handoff.
#[derive(Debug, Clone)]
pub struct Segment {
    /// The segment's seed.
    pub seed: RunSeed,
    /// The segment's run journal.
    pub journal: Journal,
    /// The handoff that started this segment; `None` for the root.
    pub handoff: Option<RolloverHandoff>,
}

/// Every segment of one spawned run, root first.
#[derive(Debug, Clone)]
pub struct RunRecord {
    /// The goal the run serves.
    pub goal: GoalId,
    /// The segments, root first; the last one is driven next.
    // HOST-ONLY (E4+aon)
    pub segments: Vec<Segment>,
}

/// The supervisor's durable state: its journal plus the run journals
/// it owns. The caller owns the store, the way flash would carry it
/// across power loss.
#[derive(Debug, Clone, Default)]
pub struct AgentStore {
    // HOST-ONLY (E4+aon)
    frames: Vec<AgentFrame>,
    // HOST-ONLY (E4+aon)
    runs: BTreeMap<u64, RunRecord>,
}

impl AgentStore {
    /// An empty store: the first boot binds it to its seed.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            frames: Vec::new(),
            runs: BTreeMap::new(),
        }
    }

    /// The committed supervisor frames, in order.
    #[must_use]
    pub fn frames(&self) -> &[AgentFrame] {
        &self.frames
    }

    /// The run with this id.
    #[must_use]
    pub fn run(&self, id: RunId) -> Option<&RunRecord> {
        self.runs.get(&id.get())
    }

    /// The ids of every run whose journal the store still holds.
    pub fn run_ids(&self) -> impl Iterator<Item = RunId> + '_ {
        self.runs.keys().map(|id| RunId::new(*id))
    }

    fn commit(&mut self, frame: AgentFrame) {
        // HOST-ONLY (E4+aon)
        self.frames.push(frame);
    }

    /// Drop a run's journal the way a torn write would: power lost
    /// after `RunSpawned` committed but before the run record was
    /// stored. Recovery rebuilds the record from the committed frame.
    /// Fault-injection hook; returns whether a record was dropped.
    pub fn lose_run(&mut self, run: RunId) -> bool {
        self.runs.remove(&run.get()).is_some()
    }
}

/// Per-goal input queues: typed human input addressed to a goal's
/// parked run. Part of the world, like [`InputPlan`]: it survives
/// crashes; delivery is consumed when the run commits it.
#[derive(Debug, Clone, Default)]
pub struct Inbox {
    // HOST-ONLY (E4+aon)
    plans: BTreeMap<u16, InputPlan>,
}

impl Inbox {
    /// An empty inbox.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            plans: BTreeMap::new(),
        }
    }

    /// Queue `input` for `goal`'s run.
    pub fn deliver(&mut self, goal: GoalId, input: Vec<u8>) {
        // HOST-ONLY (E4+aon)
        self.plans.entry(goal.get()).or_default().push(input);
    }

    /// Whether undelivered input is waiting for `goal`.
    #[must_use]
    pub fn has_input(&self, goal: GoalId) -> bool {
        self.plans
            .get(&goal.get())
            .is_some_and(InputPlan::has_pending)
    }

    fn plan(&mut self, goal: GoalId) -> &mut InputPlan {
        // HOST-ONLY (E4+aon)
        self.plans.entry(goal.get()).or_default()
    }
}

/// The world a beat acts on: the model, the device, its faults, and
/// the inbox. All of it survives a supervisor crash.
pub struct AgentWorld<'a> {
    /// The model backend every spawned run shares.
    pub model: &'a mut dyn ModelBackend,
    /// The device.
    pub device: &'a mut FakeDevice,
    /// The device fault plan.
    pub faults: &'a mut FaultPlan,
    /// Per-goal human input.
    pub inbox: &'a mut Inbox,
}

/// Where the fault injector may reset the supervisor (SPEC §21.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentCrashPoint {
    /// After `BeatStarted` commits, before scheduling.
    AfterBeatStarted,
    /// After `RunSpawned` / `RunResumed` commits, before the run boots.
    AfterRunScheduled,
    /// After the run returned, before `RunParked` / `RunSettled`.
    AfterRunReturned,
    /// After `RunParked` / `RunSettled`, before `BeatCompleted`.
    AfterRunSettled,
    /// After `BeatCompleted` commits.
    AfterBeatCompleted,
}

impl AgentCrashPoint {
    /// Every agent crash point, in beat order.
    #[must_use]
    pub const fn all() -> [Self; 5] {
        [
            Self::AfterBeatStarted,
            Self::AfterRunScheduled,
            Self::AfterRunReturned,
            Self::AfterRunSettled,
            Self::AfterBeatCompleted,
        ]
    }

    /// The stable spelling.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::AfterBeatStarted => "acp_after_beat_started",
            Self::AfterRunScheduled => "acp_after_run_scheduled",
            Self::AfterRunReturned => "acp_after_run_returned",
            Self::AfterRunSettled => "acp_after_run_settled",
            Self::AfterBeatCompleted => "acp_after_beat_completed",
        }
    }
}

/// A crash to inject into one [`drive_beat`] call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentCrash {
    /// Reset the supervisor at this point.
    Agent(AgentCrashPoint),
    /// Reset the beat's run at this run-level point (SPEC §11.1); the
    /// engine reboots it internally.
    Run(CrashPoint),
}

/// What a beat did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BeatAction {
    /// Nothing was due.
    Idle,
    /// A new run was spawned for the goal.
    Spawned {
        /// The goal.
        goal: GoalId,
        /// The new run.
        run: RunId,
    },
    /// A parked run resumed with its input.
    Resumed {
        /// The goal.
        goal: GoalId,
        /// The resumed run.
        run: RunId,
    },
}

/// How a run settled its goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settlement {
    /// The goal.
    pub goal: GoalId,
    /// The run.
    pub run: RunId,
    /// The run's terminal status.
    pub status: TerminalStatus,
    /// The goal event it caused (`Recur`, `Satisfy`, `Fail`, `Exhaust`).
    pub event: GoalEvent,
    /// Memory entries harvested from the run.
    pub harvested: usize,
}

/// One beat's report (SPEC §21.1). Derived from the journal, so a beat
/// interrupted by a crash reports the same pulse after recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pulse {
    /// The beat.
    pub beat: Beat,
    /// What the beat did.
    pub action: BeatAction,
    /// Whether the beat's run parked on `Ask`.
    pub parked: bool,
    /// How the beat's run settled, when it did.
    pub settled: Option<Settlement>,
    /// Goal counts after the beat.
    pub counts: GoalCounts,
}

/// What [`Supervisor::checkpoint`] folded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CheckpointReport {
    /// Supervisor frames before the fold.
    pub frames_before: usize,
    /// Supervisor frames after the fold (always 2).
    pub frames_after: usize,
    /// Terminal goals reaped from the ledger.
    pub goals_reaped: usize,
    /// Settled run journals dropped from the store.
    pub runs_pruned: usize,
    /// The checkpoint's encoded size.
    pub bytes: usize,
}

/// What can go wrong running the always-on supervisor.
#[derive(Debug, Error)]
pub enum AgentError {
    /// An `esper-core` rule refused the operation (goal, memory,
    /// envelope, beat, or checkpoint invariants).
    #[error("core rule refused the operation: {0}")]
    Core(#[from] esper_core::Error),
    /// A spawned run failed in the runtime.
    #[error("run failed: {0}")]
    Runtime(#[from] RuntimeError),
    /// The agent seed is malformed.
    #[error("agent seed is malformed: {0}")]
    SeedInvalid(&'static str),
    /// The journal is bound to a different agent seed.
    #[error("journal belongs to a different agent seed")]
    SeedMismatch,
    /// The journal does not replay: a frame cannot legally follow its
    /// prefix, or a run it names is missing from the store.
    #[error("agent journal is corrupt: {0}")]
    JournalCorrupt(&'static str),
    /// Operator changes and checkpoints happen between beats only.
    #[error("a beat is in progress")]
    BeatInProgress,
    /// The boot budget was exceeded; the supervisor is livelocked.
    #[error("too many boots without the beat completing")]
    TooManyBoots,
}

/// Supervisor-internal halt: an injected crash or a real error.
enum Halt {
    Crash,
    Error(AgentError),
}

impl From<AgentError> for Halt {
    fn from(error: AgentError) -> Self {
        Self::Error(error)
    }
}

impl From<esper_core::Error> for Halt {
    fn from(error: esper_core::Error) -> Self {
        Self::Error(AgentError::Core(error))
    }
}

/// Where the open beat stands, derived from the committed frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// `BeatStarted` committed; nothing scheduled yet.
    Started,
    /// A run was spawned or resumed and has not parked or settled.
    Scheduled { goal: GoalId, run: RunId },
    /// The beat's run parked or settled; only `BeatCompleted` remains.
    Returned,
}

/// The supervisor's derived state. Rebuilt from the [`AgentStore`] on
/// every boot; nothing here survives a crash.
#[derive(Debug, Clone)]
pub struct Supervisor {
    seed: AgentSeed,
    ledger: GoalLedger,
    memory: Memory,
    last_beat: Beat,
    spawn_seq: u32,
    open: Option<(Beat, Phase)>,
    pulse: Option<Pulse>,
    last_pulse: Option<Pulse>,
}

impl Supervisor {
    /// Boot: validate the seed, bind or check the journal, and replay
    /// every committed frame.
    ///
    /// # Errors
    ///
    /// [`AgentError::SeedInvalid`], [`AgentError::SeedMismatch`], or
    /// [`AgentError::JournalCorrupt`] / [`AgentError::Core`] when a
    /// frame does not replay.
    pub fn boot(seed: &AgentSeed, store: &mut AgentStore) -> Result<Self, AgentError> {
        seed.validate()?;
        let mut sup = Self {
            seed: *seed,
            ledger: GoalLedger::new(),
            memory: Memory::new(),
            last_beat: Beat::ZERO,
            spawn_seq: 0,
            open: None,
            pulse: None,
            last_pulse: None,
        };
        match store.frames.first() {
            None => store.commit(AgentFrame::AgentStarted { seed: *seed }),
            Some(AgentFrame::AgentStarted { seed: bound }) if bound == seed => {}
            Some(AgentFrame::AgentStarted { .. }) => return Err(AgentError::SeedMismatch),
            Some(_) => {
                return Err(AgentError::JournalCorrupt(
                    "first frame is not AgentStarted",
                ));
            }
        }
        for (index, frame) in store.frames.iter().enumerate().skip(1) {
            sup.apply(index, frame)?;
        }
        Ok(sup)
    }

    /// The goal ledger.
    #[must_use]
    pub const fn ledger(&self) -> &GoalLedger {
        &self.ledger
    }

    /// The memory store.
    #[must_use]
    pub const fn memory(&self) -> &Memory {
        &self.memory
    }

    /// The last completed beat.
    #[must_use]
    pub const fn last_beat(&self) -> Beat {
        self.last_beat
    }

    /// The last completed beat's pulse.
    #[must_use]
    pub const fn last_pulse(&self) -> Option<Pulse> {
        self.last_pulse
    }

    /// The agent seed.
    #[must_use]
    pub const fn seed(&self) -> &AgentSeed {
        &self.seed
    }

    /// The beat the next goal or control takes effect at.
    fn next_beat(&self) -> Result<Beat, AgentError> {
        Ok(self.last_beat.next()?)
    }

    const fn between_beats(&self) -> Result<(), AgentError> {
        if self.open.is_some() {
            return Err(AgentError::BeatInProgress);
        }
        Ok(())
    }

    /// Prove `frame` applies to the derived state, then commit it — the
    /// one write path. A frame that would not replay is never
    /// committed; a committed frame is always the authority.
    fn commit(&mut self, store: &mut AgentStore, frame: AgentFrame) -> Result<(), AgentError> {
        let index = store.frames.len();
        self.apply(index, &frame)?;
        store.commit(frame);
        Ok(())
    }

    // -----------------------------------------------------------------
    // Operator API

    /// Add a goal (SPEC §21.2). The objective is masked before the
    /// `GoalAdded` frame commits; the goal is due at the next beat.
    ///
    /// # Errors
    ///
    /// [`AgentError::BeatInProgress`] mid-beat; [`AgentError::Core`]
    /// when the spec is invalid or the ledger is full.
    pub fn add_goal(
        &mut self,
        store: &mut AgentStore,
        spec: &GoalSpec<'_>,
    ) -> Result<GoalId, AgentError> {
        self.between_beats()?;
        let goal = self
            .ledger
            .admit(spec, &self.seed.template.capabilities, self.next_beat()?)?;
        let id = goal.id;
        self.commit(store, AgentFrame::GoalAdded { goal })?;
        Ok(id)
    }

    /// Pause a goal. Refused while it has a run in flight or parked.
    ///
    /// # Errors
    ///
    /// See [`Self::control`].
    pub fn pause(&mut self, store: &mut AgentStore, goal: GoalId) -> Result<(), AgentError> {
        self.control(store, goal, GoalEvent::Pause)
    }

    /// Resume a paused goal; it is due at the next beat.
    ///
    /// # Errors
    ///
    /// See [`Self::control`].
    pub fn unpause(&mut self, store: &mut AgentStore, goal: GoalId) -> Result<(), AgentError> {
        self.control(store, goal, GoalEvent::Unpause)
    }

    /// Retire a goal for good.
    ///
    /// # Errors
    ///
    /// See [`Self::control`].
    pub fn retire(&mut self, store: &mut AgentStore, goal: GoalId) -> Result<(), AgentError> {
        self.control(store, goal, GoalEvent::Retire)
    }

    /// Apply an operator event. The event is proven on a copy of the
    /// ledger first, so an illegal request never commits a frame.
    ///
    /// # Errors
    ///
    /// [`AgentError::BeatInProgress`] mid-beat; [`AgentError::Core`]
    /// with `GoalBusy` while a run is in flight or parked, or an
    /// illegal transition.
    pub fn control(
        &mut self,
        store: &mut AgentStore,
        goal: GoalId,
        event: GoalEvent,
    ) -> Result<(), AgentError> {
        self.between_beats()?;
        let beat = self.next_beat()?;
        let mut probe = self.ledger;
        probe.control(goal, event, beat)?;
        self.commit(store, AgentFrame::GoalControlled { goal, event, beat })
    }

    /// Remember an operator note (SPEC §21.3): pinned, masked, never
    /// evicted. A second note under the same name supersedes the first.
    ///
    /// # Errors
    ///
    /// [`AgentError::BeatInProgress`] mid-beat; [`AgentError::Core`]
    /// with `MemoryFull` past the pinned cap or `MemoryInputTooLong`.
    pub fn remember(
        &mut self,
        store: &mut AgentStore,
        name: &[u8],
        value: &[u8],
    ) -> Result<(), AgentError> {
        self.between_beats()?;
        let entry = MemoryEntry::new(
            MemoryKey::note(name),
            name,
            value,
            Provenance {
                goal: None,
                run: None,
                beat: self.last_beat,
            },
        )?;
        let mut probe = self.memory;
        probe.write(entry)?;
        self.commit(store, AgentFrame::MemoryNoted { entry })
    }

    /// Forget an operator note. Returns whether it existed; nothing
    /// commits when it did not.
    ///
    /// # Errors
    ///
    /// [`AgentError::BeatInProgress`] mid-beat.
    pub fn forget(&mut self, store: &mut AgentStore, name: &[u8]) -> Result<bool, AgentError> {
        self.between_beats()?;
        let key = MemoryKey::note(name);
        if self.memory.get(&key).is_none() {
            return Ok(false);
        }
        self.commit(store, AgentFrame::MemoryForgotten { key })?;
        Ok(true)
    }

    /// Fold the journal into a verified checkpoint (SPEC §21.6).
    ///
    /// Between beats only, and only with no run in flight. Terminal
    /// goals are reaped (their last outcome stays in memory); the
    /// checkpoint is encoded, decoded back, and compared before the
    /// journal is replaced by `[AgentStarted, Checkpoint]`; settled run
    /// journals are dropped, parked ones kept.
    ///
    /// # Errors
    ///
    /// [`AgentError::BeatInProgress`] mid-beat or with a run in
    /// flight; [`AgentError::Core`] when the read-back disagrees.
    pub fn checkpoint(&mut self, store: &mut AgentStore) -> Result<CheckpointReport, AgentError> {
        self.between_beats()?;
        if self.ledger.has_running() {
            return Err(AgentError::BeatInProgress);
        }
        let mut ledger = self.ledger;
        let goals_reaped = ledger.reap();
        let state = AgentCheckpoint {
            last_beat: self.last_beat,
            spawn_seq: self.spawn_seq,
            ledger,
            memory: self.memory,
        };
        // HOST-ONLY (E4+aon)
        let mut bytes = vec![0u8; MAX_CHECKPOINT_BYTES];
        let len = checkpoint::encode(&state, &mut bytes)?;
        bytes.truncate(len);
        // Read-back verification: the bytes must decode to exactly the
        // state they encode before they replace the journal.
        if checkpoint::decode(&bytes)? != state {
            return Err(AgentError::JournalCorrupt("checkpoint read-back mismatch"));
        }
        let frames_before = store.frames.len();
        let parked: Vec<u64> = ledger
            .goals()
            .filter_map(|g| g.run.map(RunId::get))
            .collect();
        let runs_before = store.runs.len();
        store.runs.retain(|id, _| parked.contains(id));
        store.frames = vec![
            AgentFrame::AgentStarted { seed: self.seed },
            AgentFrame::Checkpoint { bytes },
        ];
        self.ledger = ledger;
        Ok(CheckpointReport {
            frames_before,
            frames_after: store.frames.len(),
            goals_reaped,
            runs_pruned: runs_before - store.runs.len(),
            bytes: len,
        })
    }

    // -----------------------------------------------------------------
    // Replay

    /// Apply one committed frame to the derived state. The live path
    /// and replay share this function, so they cannot disagree.
    #[allow(clippy::too_many_lines)]
    fn apply(&mut self, index: usize, frame: &AgentFrame) -> Result<(), AgentError> {
        match frame {
            AgentFrame::AgentStarted { .. } => {
                return Err(AgentError::JournalCorrupt(
                    "AgentStarted after the first frame",
                ));
            }
            AgentFrame::Checkpoint { bytes } => {
                if index != 1 {
                    return Err(AgentError::JournalCorrupt(
                        "checkpoint is not the second frame",
                    ));
                }
                let state = checkpoint::decode(bytes)?;
                self.ledger = state.ledger;
                self.memory = state.memory;
                self.last_beat = state.last_beat;
                self.spawn_seq = state.spawn_seq;
            }
            AgentFrame::GoalAdded { goal } => {
                self.between_beats()?;
                self.ledger.insert(*goal)?;
            }
            AgentFrame::GoalControlled { goal, event, beat } => {
                self.between_beats()?;
                self.ledger.control(*goal, *event, *beat)?;
            }
            AgentFrame::MemoryNoted { entry } => {
                self.between_beats()?;
                if !entry.pinned() {
                    return Err(AgentError::JournalCorrupt("operator notes are pinned"));
                }
                self.memory.write(*entry)?;
            }
            AgentFrame::MemoryForgotten { key } => {
                self.between_beats()?;
                if !self.memory.forget(key) {
                    return Err(AgentError::JournalCorrupt("forgot an unknown note"));
                }
            }
            AgentFrame::BeatStarted { beat } => {
                self.between_beats()?;
                if *beat != self.last_beat.next()? {
                    return Err(AgentError::JournalCorrupt("beat skipped or repeated"));
                }
                self.open = Some((*beat, Phase::Started));
                self.pulse = Some(Pulse {
                    beat: *beat,
                    action: BeatAction::Idle,
                    parked: false,
                    settled: None,
                    counts: GoalCounts::default(),
                });
            }
            AgentFrame::RunSpawned {
                beat,
                goal,
                seed,
                reservation,
            } => {
                self.expect_phase(*beat, Phase::Started)?;
                if seed.id != self.next_run_id(*goal)
                    || seed.model_turns != reservation.turns
                    || seed.mutations != reservation.mutations
                    || *seed
                        != self.spawn_seed(*goal, *reservation, seed.brief.as_ref(), seed.id)?
                {
                    return Err(AgentError::JournalCorrupt(
                        "spawned seed disagrees with ledger",
                    ));
                }
                self.ledger.spawn(*goal, seed.id, *reservation)?;
                self.spawn_seq = self
                    .spawn_seq
                    .checked_add(1)
                    .ok_or(AgentError::JournalCorrupt("spawn counter overflow"))?;
                self.open = Some((
                    *beat,
                    Phase::Scheduled {
                        goal: *goal,
                        run: seed.id,
                    },
                ));
                self.set_action(BeatAction::Spawned {
                    goal: *goal,
                    run: seed.id,
                });
            }
            AgentFrame::RunResumed { beat, goal, run } => {
                self.expect_phase(*beat, Phase::Started)?;
                if self.ledger.get(*goal).and_then(|g| g.run) != Some(*run) {
                    return Err(AgentError::JournalCorrupt(
                        "resumed run is not the parked one",
                    ));
                }
                self.ledger.resume(*goal)?;
                self.open = Some((
                    *beat,
                    Phase::Scheduled {
                        goal: *goal,
                        run: *run,
                    },
                ));
                self.set_action(BeatAction::Resumed {
                    goal: *goal,
                    run: *run,
                });
            }
            AgentFrame::RunParked { beat, goal, run } => {
                self.expect_phase(
                    *beat,
                    Phase::Scheduled {
                        goal: *goal,
                        run: *run,
                    },
                )?;
                self.ledger.park(*goal, *beat)?;
                self.open = Some((*beat, Phase::Returned));
                if let Some(pulse) = &mut self.pulse {
                    pulse.parked = true;
                }
            }
            AgentFrame::RunSettled {
                beat,
                goal,
                run,
                status,
                refund,
                harvest,
            } => {
                self.expect_phase(
                    *beat,
                    Phase::Scheduled {
                        goal: *goal,
                        run: *run,
                    },
                )?;
                let policy = self.seed.policy;
                let event = self
                    .ledger
                    .settle(*goal, *status, *refund, *beat, &policy)?;
                for entry in harvest {
                    if entry.pinned() {
                        return Err(AgentError::JournalCorrupt("harvest wrote a pinned note"));
                    }
                    self.memory.write(*entry)?;
                }
                self.open = Some((*beat, Phase::Returned));
                if let Some(pulse) = &mut self.pulse {
                    pulse.settled = Some(Settlement {
                        goal: *goal,
                        run: *run,
                        status: *status,
                        event,
                        harvested: harvest.len(),
                    });
                }
            }
            AgentFrame::BeatCompleted { beat } => {
                let Some((open, phase)) = self.open else {
                    return Err(AgentError::JournalCorrupt("completed a beat never started"));
                };
                let idle = self.pulse.is_some_and(|p| p.action == BeatAction::Idle);
                if open != *beat || !(phase == Phase::Returned || (phase == Phase::Started && idle))
                {
                    return Err(AgentError::JournalCorrupt("beat completed out of order"));
                }
                self.last_beat = *beat;
                self.open = None;
                let counts = self.ledger.counts(*beat, &self.seed.policy);
                self.last_pulse = self.pulse.take().map(|pulse| Pulse { counts, ..pulse });
            }
        }
        Ok(())
    }

    fn expect_phase(&self, beat: Beat, phase: Phase) -> Result<(), AgentError> {
        if self.open != Some((beat, phase)) {
            return Err(AgentError::JournalCorrupt("frame out of beat order"));
        }
        Ok(())
    }

    const fn set_action(&mut self, action: BeatAction) {
        if let Some(pulse) = &mut self.pulse {
            pulse.action = action;
        }
    }

    /// The seed a spawn of `goal` takes: the template with the goal's
    /// run id, reservation, capabilities, and brief.
    fn spawn_seed(
        &self,
        goal: GoalId,
        reservation: Grant,
        brief: Option<&Brief>,
        id: RunId,
    ) -> Result<RunSeed, AgentError> {
        let capabilities = self
            .ledger
            .get(goal)
            .ok_or_else(|| esper_core::Error::GoalUnknown { goal: goal.get() })?
            .capabilities;
        Ok(RunSeed {
            id,
            model_turns: reservation.turns,
            mutations: reservation.mutations,
            capabilities,
            brief: brief.copied(),
            parent: None,
            ..self.seed.template
        })
    }

    /// The run id for the next spawn of `goal` (SPEC §21.5):
    /// `goal << 48 | spawn_seq << 16`. Rollover children add
    /// `frame + 1` inside the low 16 bits.
    fn next_run_id(&self, goal: GoalId) -> RunId {
        RunId::new((u64::from(goal.get()) << 48) | (u64::from(self.spawn_seq) << 16))
    }

    // -----------------------------------------------------------------
    // The beat

    /// Run (or finish) the open beat. Returns `Halt::Crash` when the
    /// armed agent crash point fires.
    fn step(
        &mut self,
        store: &mut AgentStore,
        world: &mut AgentWorld<'_>,
        agent_crash: &mut Option<AgentCrashPoint>,
        run_crash: &mut Option<CrashPoint>,
    ) -> Result<Pulse, Halt> {
        let beat = if let Some((beat, _)) = self.open {
            beat
        } else {
            let beat = self.last_beat.next().map_err(AgentError::from)?;
            self.commit(store, AgentFrame::BeatStarted { beat })?;
            fire(agent_crash, AgentCrashPoint::AfterBeatStarted)?;
            beat
        };
        if let Some((_, Phase::Started)) = self.open {
            match self.ledger.pick(beat, |goal| world.inbox.has_input(goal)) {
                Pick::Idle => {}
                Pick::Spawn(goal) => {
                    let reservation = self.ledger.reservation(goal)?;
                    let objective = self
                        .ledger
                        .get(goal)
                        .ok_or_else(|| esper_core::Error::GoalUnknown { goal: goal.get() })?
                        .objective;
                    let brief = Brief::compose(&objective, &recall(&self.memory, goal));
                    let id = self.next_run_id(goal);
                    let seed = self.spawn_seed(goal, reservation, Some(&brief), id)?;
                    seed.validate().map_err(AgentError::SeedInvalid)?;
                    self.commit(
                        store,
                        AgentFrame::RunSpawned {
                            beat,
                            goal,
                            seed,
                            reservation,
                        },
                    )?;
                    // HOST-ONLY (E4+aon)
                    store.runs.insert(
                        id.get(),
                        RunRecord {
                            goal,
                            segments: vec![Segment {
                                seed,
                                journal: Journal::new(),
                                handoff: None,
                            }],
                        },
                    );
                    fire(agent_crash, AgentCrashPoint::AfterRunScheduled)?;
                }
                Pick::Resume(goal) => {
                    let run = self
                        .ledger
                        .get(goal)
                        .and_then(|g| g.run)
                        .ok_or(AgentError::JournalCorrupt("waiting goal holds no run"))?;
                    self.commit(store, AgentFrame::RunResumed { beat, goal, run })?;
                    fire(agent_crash, AgentCrashPoint::AfterRunScheduled)?;
                }
            }
        }
        if let Some((_, Phase::Scheduled { goal, run })) = self.open {
            let end = drive_record(store, world, run, run_crash.take())?;
            fire(agent_crash, AgentCrashPoint::AfterRunReturned)?;
            let frame = match end {
                RunEnd::Parked => AgentFrame::RunParked { beat, goal, run },
                RunEnd::Terminal(trace) => {
                    let status = trace
                        .terminal_status()
                        .ok_or(AgentError::JournalCorrupt("terminal run without status"))?;
                    let record = store
                        .run(run)
                        .ok_or(AgentError::JournalCorrupt("settled run missing"))?;
                    let harvest = harvest(record, goal, run, beat, status, &trace)?;
                    AgentFrame::RunSettled {
                        beat,
                        goal,
                        run,
                        status,
                        refund: Grant {
                            turns: trace.turns_remaining(),
                            mutations: trace.mutations_remaining(),
                        },
                        harvest,
                    }
                }
            };
            self.commit(store, frame)?;
            fire(agent_crash, AgentCrashPoint::AfterRunSettled)?;
        }
        self.commit(store, AgentFrame::BeatCompleted { beat })?;
        fire(agent_crash, AgentCrashPoint::AfterBeatCompleted)?;
        self.last_pulse
            .ok_or(Halt::Error(AgentError::JournalCorrupt(
                "beat left no pulse",
            )))
    }
}

/// Fire `point` once if it is the armed crash.
fn fire(armed: &mut Option<AgentCrashPoint>, point: AgentCrashPoint) -> Result<(), Halt> {
    if *armed == Some(point) {
        *armed = None;
        return Err(Halt::Crash);
    }
    Ok(())
}

/// How a driven run ended for this beat.
enum RunEnd {
    /// Suspended on `Ask` with no input queued.
    Parked,
    /// Reached its terminal result (possibly replayed).
    Terminal(Box<RunTrace>),
}

/// Drive a run's newest segment until it parks or ends, appending a
/// segment on every rollover.
fn drive_record(
    store: &mut AgentStore,
    world: &mut AgentWorld<'_>,
    run: RunId,
    mut crash: Option<CrashPoint>,
) -> Result<RunEnd, AgentError> {
    restore_record(store, run)?;
    let record = store
        .runs
        .get_mut(&run.get())
        .ok_or(AgentError::JournalCorrupt(
            "scheduled run missing from store",
        ))?;
    let inputs = world.inbox.plan(record.goal);
    loop {
        let segment = record
            .segments
            .last_mut()
            .ok_or(AgentError::JournalCorrupt("run has no segment"))?;
        let outcome = drive_segment(
            &segment.seed,
            &mut segment.journal,
            &mut *world.model,
            world.device,
            world.faults,
            inputs,
            crash.take(),
            segment.handoff.clone(),
        )?;
        match outcome {
            SegmentOutcome::Completed(trace) => return Ok(RunEnd::Terminal(Box::new(trace))),
            SegmentOutcome::Suspended(_) => return Ok(RunEnd::Parked),
            SegmentOutcome::Rollover(_, handoff) => {
                let seed = handoff.child_seed(&segment.seed);
                // HOST-ONLY (E4+aon)
                record.segments.push(Segment {
                    seed,
                    journal: Journal::new(),
                    handoff: Some(handoff),
                });
            }
        }
    }
}

/// Rebuild a spawned run's record from its committed `RunSpawned`
/// frame when the store lost it.
///
/// The spawn frame commits before the record is stored; power lost in
/// between leaves a committed spawn with no run journal. The run never
/// booted (its journal did not exist yet), so the committed seed and an
/// empty journal are exactly its state — the committed frame is the
/// authority, never a re-composed seed.
fn restore_record(store: &mut AgentStore, run: RunId) -> Result<(), AgentError> {
    if store.runs.contains_key(&run.get()) {
        return Ok(());
    }
    let (goal, seed) = store
        .frames
        .iter()
        .find_map(|frame| match frame {
            AgentFrame::RunSpawned { goal, seed, .. } if seed.id == run => Some((*goal, *seed)),
            _ => None,
        })
        .ok_or(AgentError::JournalCorrupt(
            "scheduled run missing from store",
        ))?;
    // HOST-ONLY (E4+aon)
    store.runs.insert(
        run.get(),
        RunRecord {
            goal,
            segments: vec![Segment {
                seed,
                journal: Journal::new(),
                handoff: None,
            }],
        },
    );
    Ok(())
}

/// Harvest a settled run into memory entries (SPEC §21.3).
///
/// Walks every segment's committed trace in journal order: each
/// successful observation becomes an `Observation` entry — a mutating
/// tool's only with a passing verification for the same effect — and
/// the last write per key wins. One `Outcome` entry records the
/// terminal status and summary. Inputs over `MEMORY_INPUT_MAX` are
/// skipped (fail closed: not remembered), never truncated unmasked.
fn harvest(
    record: &RunRecord,
    goal: GoalId,
    run: RunId,
    beat: Beat,
    status: TerminalStatus,
    last: &RunTrace,
) -> Result<Vec<MemoryEntry>, AgentError> {
    let provenance = Provenance {
        goal: Some(goal),
        run: Some(run),
        beat,
    };
    // HOST-ONLY (E4+aon)
    let mut entries: Vec<MemoryEntry> = Vec::new();
    for segment in &record.segments {
        let trace = RunTrace::from_journal(&segment.journal, &segment.seed)?;
        let events = trace.events();
        for event in events {
            let TraceEvent::ToolRequest { seq, tool, args } = event else {
                continue;
            };
            let Some(contract) = lookup_by_id(ToolId::new(*tool)) else {
                continue;
            };
            let observed = events.iter().rev().find_map(|e| match e {
                TraceEvent::ToolObservation {
                    seq: s,
                    ok: true,
                    payload,
                } if s == seq => Some(payload),
                _ => None,
            });
            let Some(payload) = observed else { continue };
            let mutating = contract.permission != PermissionClass::ReadOnly;
            let verified = events.iter().any(
                |e| matches!(e, TraceEvent::Verification { seq: s, passed: true, .. } if s == seq),
            );
            if mutating && !verified {
                // SPEC §21.3 rule 3: never remember an unverified mutation.
                continue;
            }
            let mut label = contract.name.as_bytes().to_vec();
            label.push(b' ');
            label.extend_from_slice(args);
            if label.len() > MEMORY_INPUT_MAX || payload.len() > MEMORY_INPUT_MAX {
                continue;
            }
            let key = MemoryKey::Observation {
                tool: ToolId::new(*tool),
                args_digest: Digest::of_bytes(args),
            };
            let entry = MemoryEntry::new(key, &label, payload, provenance)?;
            entries.retain(|existing| existing.key != key);
            entries.push(entry);
        }
    }
    let label = format!("goal#{}", goal.get());
    let mut value = status.name().as_bytes().to_vec();
    if let Some(summary) = last.summary().filter(|s| !s.is_empty()) {
        value.push(b' ');
        value.extend_from_slice(summary);
    }
    if value.len() > MEMORY_INPUT_MAX {
        value = status.name().as_bytes().to_vec();
    }
    entries.push(MemoryEntry::new(
        MemoryKey::Outcome { goal },
        label.as_bytes(),
        &value,
        provenance,
    )?);
    Ok(entries)
}

/// Run one heartbeat (SPEC §21.1): boot the supervisor from `store`,
/// finish the beat a crash interrupted or start the next one, and
/// return its pulse.
///
/// An injected `crash` fires once; the supervisor reboots from the
/// store and finishes the same beat, transparently. A run-level crash
/// passes to the beat's run (the engine reboots it internally).
///
/// # Errors
///
/// [`AgentError`] on a malformed seed, a corrupt store, a core rule
/// violation, or a run failure. An injected crash never surfaces.
pub fn drive_beat(
    seed: &AgentSeed,
    store: &mut AgentStore,
    world: &mut AgentWorld<'_>,
    crash: Option<AgentCrash>,
) -> Result<Pulse, AgentError> {
    let (mut agent_crash, mut run_crash) = match crash {
        Some(AgentCrash::Agent(point)) => (Some(point), None),
        Some(AgentCrash::Run(point)) => (None, Some(point)),
        None => (None, None),
    };
    let mut target: Option<Beat> = None;
    for _ in 0..MAX_BOOTS {
        let mut sup = Supervisor::boot(seed, store)?;
        // The beat this call owns: the interrupted one, or the next.
        let beat = if let Some(beat) = target {
            beat
        } else {
            let beat = match sup.open {
                Some((open, _)) => open,
                None => sup.last_beat.next()?,
            };
            target = Some(beat);
            beat
        };
        if sup.last_beat >= beat {
            return sup
                .last_pulse
                .ok_or(AgentError::JournalCorrupt("completed beat left no pulse"));
        }
        match sup.step(store, world, &mut agent_crash, &mut run_crash) {
            Ok(pulse) => return Ok(pulse),
            Err(Halt::Crash) => {}
            Err(Halt::Error(error)) => return Err(error),
        }
    }
    Err(AgentError::TooManyBoots)
}

/// Run the next (or interrupted) beat until `point` and stop there,
/// leaving the store exactly as power loss would: no reboot inside
/// this call. The next [`drive_beat`] call is the recovery.
///
/// Fault injection for crash harnesses that model a reset *across*
/// driver calls; [`drive_beat`]'s `crash` argument models one
/// recovered *within* a call. Returns whether `point` was reached (an
/// idle beat never reaches the run points, and then completes).
///
/// # Errors
///
/// Same as [`drive_beat`].
pub fn halt_beat_at(
    seed: &AgentSeed,
    store: &mut AgentStore,
    world: &mut AgentWorld<'_>,
    point: AgentCrashPoint,
) -> Result<bool, AgentError> {
    let mut sup = Supervisor::boot(seed, store)?;
    match sup.step(store, world, &mut Some(point), &mut None) {
        Ok(_) => Ok(false),
        Err(Halt::Crash) => Ok(true),
        Err(Halt::Error(error)) => Err(error),
    }
}

/// Pace [`drive_beat`] with a Tokio interval for `beats` beats
/// (SPEC §21.7) and return the pulses.
///
/// Missed ticks are delayed, never burst (SPEC §21.1 rule 5): a late
/// tick does not queue catch-up beats. Each beat runs on a worker via
/// `block_in_place`, so the reactor never stalls; requires a
/// multi-threaded Tokio runtime. The period is the host's choice and
/// never identity.
///
/// # Errors
///
/// The first beat's [`AgentError`] stops the loop and is returned.
pub async fn run_heartbeat(
    seed: &AgentSeed,
    store: &mut AgentStore,
    world: &mut AgentWorld<'_>,
    period: Duration,
    beats: u64,
) -> Result<Vec<Pulse>, AgentError> {
    // HOST-ONLY (E4+aon)
    let mut interval = tokio::time::interval(period);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pulses = Vec::new();
    for _ in 0..beats {
        interval.tick().await;
        let pulse = tokio::task::block_in_place(|| drive_beat(seed, store, world, None))?;
        pulses.push(pulse);
    }
    Ok(pulses)
}
