//! The always-on supervisor: a Waymaker workflow that serves standing
//! goals with ordinary runs (E4+aon, SPEC §21).
//!
//! A run is durable but finite. The supervisor is what stays on, and it
//! is built the way the rest of Esper is: as an `async fn` over
//! [`waymaker_embassy::Ctx`], `no_std`, allocation-free, polled by
//! Embassy's executor on the device.
//!
//! * **The heartbeat is a durable deadline.** Each beat awaits
//!   [`Ctx::timer`]: [`Heartbeat::Boot`] waits boot-clock ticks (a reset
//!   restarts the interval — liveness, not a schedule), and
//!   [`Heartbeat::Persistent`] waits for an RTC reading, so the cadence
//!   survives power loss. Missed beats are not owed: the next deadline
//!   is computed from the later of the old deadline and the sensed
//!   clock.
//! * **Everything the world says is an activity.** The beat's
//!   [`wire::SENSE`] answer carries the clock, which parked goals have
//!   input, and operator commands — so replay sees exactly what the
//!   live boot saw. Goal runs are [`wire::RUN`] activities whose
//!   committed input is the full spawn seed, brief included; harvested
//!   memory arrives one [`wire::HARVEST`] record at a time.
//! * **Waymaker's journal is the store.** Ledger, memory, beat counter:
//!   all derived by replaying this workflow. A reset mid-beat replays
//!   the beat's recorded boundaries and finishes the *same* beat.
//! * **History is bounded with `continue_as_new`.** Every
//!   [`AgentSeed::checkpoint_every`] beats, the state is stored item by
//!   item ([`wire::STORE`], each record one scratch page) and the run
//!   continues as new with only a [`Start`] header. The next run loads
//!   the items back ([`wire::LOAD`]) and verifies their digest.
//! * **The state is caller-owned.** [`AgentState`] is passed in by
//!   `&mut`, so on the device it lives in a `static`, not in the
//!   future.
//!
//! The dispatcher behind the activities is the device's: on the board
//! [`wire::RUN`] drives [`crate::run::run`]; on the host,
//! [`host::HostAgent`] drives [`crate::drive_segment`] over the test
//! doubles.

pub mod wire;

#[cfg(feature = "host")]
pub mod host;

use core::convert::Infallible;

use esper_core::beat::{Beat, BeatSpan, SchedulePolicy};
use esper_core::brief::Brief;
use esper_core::checkpoint::{AgentCheckpoint, HEADER_BYTES, Header, Restore};
use esper_core::goal::{GoalCounts, GoalEvent, GoalId, GoalSpec, Grant, Pick};
use esper_core::harvest::MAX_HARVEST;
use esper_core::ids::{Digest, RunId};
use esper_core::memory::{MemoryEntry, MemoryKey, Provenance, recall};
use esper_core::state::TerminalStatus;
use thiserror::Error;
use waymaker_core::timer::TimerSpec;
use waymaker_embassy::ctx::Failure;
use waymaker_embassy::{ActivityDispatcher, Alarm, Ctx, Journal};

use crate::backend::InferenceSettings;
use crate::seed::RunSeed;
use wire::{
    Ack, Command, HarvestEntry, Item, MAX_RUN_REQUEST_BYTES, MAX_STORE_BYTES, RunReport,
    RunRequest, Sense, WireError,
};

/// How the heartbeat is clocked (SPEC §21.1). Part of the agent's
/// identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heartbeat {
    /// Beat every `ticks` of the boot clock. A reset restarts the
    /// interval: the cadence is liveness, not a schedule. Needs no RTC.
    Boot {
        /// Boot-clock ticks per beat (`>= 1`).
        ticks: u64,
    },
    /// Beat every `period` of a persistent clock (an RTC): the deadline
    /// survives power loss. A firmware without a persistent clock is
    /// refused by Waymaker rather than downgraded.
    Persistent {
        /// Persistent-clock units per beat (`>= 1`).
        period: u64,
    },
}

/// The always-on agent's identity: bound into every run of the
/// supervisor workflow by digest ([`Start`]), so a reboot can never swap
/// the template, the policy, or the heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentSeed {
    /// The agent identity.
    pub id: u64,
    /// The per-run template: workflow version, model bundle, inference
    /// settings, token and time grants, the rollover trigger, and the
    /// capability **ceiling** no goal may widen. Each spawn overrides
    /// the run id, turns, mutations, capabilities, and brief.
    pub template: RunSeed,
    /// Failure handling and the liveness threshold.
    pub policy: SchedulePolicy,
    /// How beats are clocked.
    pub heartbeat: Heartbeat,
    /// Checkpoint and `continue_as_new` every this many beats.
    pub checkpoint_every: BeatSpan,
}

impl AgentSeed {
    /// A default agent: the slice template with the briefed prompt
    /// buffer, the default policy, a 1-tick boot-clock heartbeat, and a
    /// checkpoint every 64 beats. Test values, not protocol values.
    #[must_use]
    pub const fn default_slice() -> Self {
        let mut template = RunSeed::default_slice();
        template.inference = InferenceSettings::briefed_settings();
        Self {
            id: 1,
            template,
            policy: SchedulePolicy::DEFAULT,
            heartbeat: Heartbeat::Boot { ticks: 1 },
            checkpoint_every: match BeatSpan::new(64) {
                Some(span) => span,
                None => BeatSpan::ONE,
            },
        }
    }

    /// Validate the seed.
    ///
    /// # Errors
    ///
    /// [`AgentError::SeedInvalid`] when the template carries a lineage
    /// or a brief (the supervisor owns both), fails run-seed validation,
    /// the policy is malformed, or the heartbeat interval is zero.
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
        RunSeed {
            model_turns: 1,
            inference: InferenceSettings::default_settings(),
            ..self.template
        }
        .validate()
        .map_err(AgentError::SeedInvalid)?;
        self.policy
            .validate()
            .map_err(|_| AgentError::SeedInvalid("schedule policy is malformed"))?;
        match self.heartbeat {
            Heartbeat::Boot { ticks: 0 } | Heartbeat::Persistent { period: 0 } => Err(
                AgentError::SeedInvalid("heartbeat interval must be at least 1"),
            ),
            _ => Ok(()),
        }
    }

    /// FNV-1a-64 over the canonical encoding: id, the template's
    /// 151-byte seed binding, the policy, the heartbeat, and the
    /// checkpoint interval.
    #[must_use]
    pub fn digest(&self) -> u64 {
        let mut bytes = [0u8; 8 + 151 + 13 + 9 + 4];
        let mut w = wire::Writer::new(&mut bytes);
        // The buffer is sized exactly; every write fits.
        let _ = w.bytes(&self.id.to_le_bytes());
        let _ = w.bytes(&self.template.snapshot().input_bytes());
        let _ = w.bytes(&[self.policy.max_consecutive_failures]);
        let _ = w.bytes(&self.policy.backoff_base.get().to_le_bytes());
        let _ = w.bytes(&self.policy.backoff_cap.get().to_le_bytes());
        let _ = w.bytes(&self.policy.overdue_after.get().to_le_bytes());
        let (tag, value) = match self.heartbeat {
            Heartbeat::Boot { ticks } => (0u8, ticks),
            Heartbeat::Persistent { period } => (1u8, period),
        };
        let _ = w.bytes(&[tag]);
        let _ = w.bytes(&value.to_le_bytes());
        let _ = w.bytes(&self.checkpoint_every.get().to_le_bytes());
        Digest::of_bytes(&bytes).get()
    }

    /// The input of the agent's first run.
    #[must_use]
    pub fn start(&self) -> Start {
        Start {
            seed_digest: self.digest(),
            header: None,
        }
    }
}

/// A supervisor run's input: the agent seed it is bound to, and — for a
/// run continued from a checkpoint — the checkpoint header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Start {
    /// The agent seed's digest; a mismatch fails closed.
    pub seed_digest: u64,
    /// The checkpoint to restore, for a continued run.
    pub header: Option<Header>,
}

/// The largest encoded [`Start`].
pub const MAX_START_BYTES: usize = 1 + 8 + HEADER_BYTES;

impl Start {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`AgentError::Wire`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, AgentError> {
        let tag = u8::from(self.header.is_some());
        let head = out.get_mut(..9).ok_or(WireError::BufferTooSmall)?;
        head.get_mut(..1)
            .ok_or(WireError::BufferTooSmall)?
            .copy_from_slice(&[tag]);
        head.get_mut(1..9)
            .ok_or(WireError::BufferTooSmall)?
            .copy_from_slice(&self.seed_digest.to_le_bytes());
        let Some(header) = self.header else {
            return Ok(9);
        };
        let rest = out.get_mut(9..).ok_or(WireError::BufferTooSmall)?;
        let n = header.encode(rest).map_err(WireError::from)?;
        Ok(9 + n)
    }

    /// Decode a supervisor run's input.
    ///
    /// # Errors
    ///
    /// [`AgentError::Wire`] for malformed bytes.
    pub fn decode(bytes: &[u8]) -> Result<Self, AgentError> {
        let tag = *bytes.first().ok_or(WireError::Truncated)?;
        let digest = bytes
            .get(1..9)
            .and_then(|b| <[u8; 8]>::try_from(b).ok())
            .ok_or(WireError::Truncated)?;
        let seed_digest = u64::from_le_bytes(digest);
        let rest = bytes.get(9..).unwrap_or(&[]);
        let header = match tag {
            0 if rest.is_empty() => None,
            1 => Some(Header::decode(rest).map_err(WireError::from)?),
            _ => return Err(WireError::Invalid.into()),
        };
        Ok(Self {
            seed_digest,
            header,
        })
    }
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
    /// The goal event it caused.
    pub event: GoalEvent,
    /// Memory entries harvested from the run.
    pub harvested: u8,
}

/// One beat's report (SPEC §21.1). Derived from recorded history, so a
/// replayed beat reports the same pulse again.
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
    /// Operator commands the ledger or memory refused.
    pub rejected: u8,
    /// Goal counts after the beat.
    pub counts: GoalCounts,
}

impl Pulse {
    const fn new(beat: Beat) -> Self {
        Self {
            beat,
            action: BeatAction::Idle,
            parked: false,
            settled: None,
            rejected: 0,
            counts: GoalCounts {
                active: 0,
                waiting: 0,
                overdue: 0,
                paused: 0,
                done: 0,
            },
        }
    }
}

/// Where pulses go: telemetry, a log, an LED. At least once: a replayed
/// beat emits its pulse again, so a sink keys by [`Pulse::beat`].
pub trait PulseSink {
    /// One beat completed (or replayed).
    fn pulse(&mut self, pulse: &Pulse);
}

/// A sink that drops every pulse.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoPulse;

impl PulseSink for NoPulse {
    fn pulse(&mut self, _pulse: &Pulse) {}
}

/// The supervisor's derived state. Caller-owned, so on the device it
/// lives in a `static`; [`supervise`] resets and rebuilds it on every
/// boot by replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentState {
    /// Ledger, memory, counters: the checkpointable state.
    pub checkpoint: AgentCheckpoint,
    /// The last completed beat's pulse.
    pub pulse: Option<Pulse>,
}

impl AgentState {
    /// An empty state.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            checkpoint: AgentCheckpoint::new(),
            pulse: None,
        }
    }
}

impl Default for AgentState {
    fn default() -> Self {
        Self::new()
    }
}

/// What stops the supervisor. Every variant is a fault the replay would
/// hit again: the workflow ends rather than guess.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum AgentError {
    /// A core rule refused a step (envelope, ledger, memory, beat).
    #[error("core rule refused the step: {0}")]
    Core(#[from] esper_core::Error),
    /// A recorded payload does not decode.
    #[error("recorded payload does not decode: {0}")]
    Wire(#[from] WireError),
    /// The agent seed is malformed.
    #[error("agent seed is malformed: {0}")]
    SeedInvalid(&'static str),
    /// The run's input is bound to a different agent seed.
    #[error("run input belongs to a different agent seed")]
    SeedMismatch,
    /// A checkpoint item could not be loaded: the run refuses to start
    /// from a state it cannot rebuild.
    #[error("checkpoint item missing")]
    CheckpointMissing,
}

/// The always-on supervisor workflow (SPEC §21).
///
/// `input` is this run's input: [`AgentSeed::start`] encoded for the
/// first run, or what the previous run's `continue_as_new` carried.
/// `state` is reset and rebuilt by replay on every boot. The future
/// never completes successfully: it beats forever, continuing as new
/// every [`AgentSeed::checkpoint_every`] beats. It returns only on a
/// fault that replay would hit again.
///
/// # Errors
///
/// [`AgentError`] when the seed is invalid or does not match the run's
/// input, a recorded payload does not decode, a checkpoint item is
/// missing, or a core rule refuses a recorded step.
// Embassy runs tasks on a single-core executor: the workflow need not
// be `Send`, and requiring it would forbid `Cell`-based journals and
// sinks for no benefit.
#[allow(clippy::future_not_send)]
pub async fn supervise<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    seed: &AgentSeed,
    input: &[u8],
    alarm: &mut dyn Alarm,
    state: &mut AgentState,
    sink: &mut dyn PulseSink,
) -> Result<Infallible, AgentError> {
    seed.validate()?;
    let start = Start::decode(input)?;
    if start.seed_digest != seed.digest() {
        return Err(AgentError::SeedMismatch);
    }
    *state = AgentState::new();
    if let Some(header) = start.header {
        restore(ctx, header, state).await?;
    }
    loop {
        beat(ctx, seed, alarm, state, sink).await?;
        let last = state.checkpoint.last_beat.get();
        if last.is_multiple_of(u64::from(seed.checkpoint_every.get()))
            && let Some(header) = store_checkpoint(ctx, state).await?
        {
            let mut buf = [0u8; MAX_START_BYTES];
            let next = Start {
                seed_digest: start.seed_digest,
                header: Some(header),
            };
            let n = next.encode(&mut buf)?;
            match ctx.continue_as_new(buf.get(..n).unwrap_or(&[])).await {}
        }
    }
}

/// Load a checkpoint's items back into `state` and verify them.
// Embassy runs tasks on a single-core executor: the workflow need not
// be `Send`, and requiring it would forbid `Cell`-based journals and
// sinks for no benefit.
#[allow(clippy::future_not_send)]
async fn restore<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    header: Header,
    state: &mut AgentState,
) -> Result<(), AgentError> {
    let mut restore = Restore::begin(header, &mut state.checkpoint);
    let mut index: u16 = 0;
    while restore.remaining() > 0 {
        let key = wire::item_key(header.digest, index);
        let item = match ctx.activity::<Item>(wire::LOAD, &key).await {
            Ok(item) => item,
            Err(Failure::Activity { .. }) => return Err(AgentError::CheckpointMissing),
            Err(Failure::Decode(e)) => return Err(e.into()),
        };
        restore.push(&mut state.checkpoint, item.as_bytes())?;
        index = index.saturating_add(1);
    }
    restore.finish()?;
    Ok(())
}

/// Store every checkpoint item. Returns the header when every store
/// succeeded; `None` (try again at the next interval) when one failed.
// Embassy runs tasks on a single-core executor: the workflow need not
// be `Send`, and requiring it would forbid `Cell`-based journals and
// sinks for no benefit.
#[allow(clippy::future_not_send)]
async fn store_checkpoint<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    state: &AgentState,
) -> Result<Option<Header>, AgentError> {
    let header = state.checkpoint.header();
    let mut buf = [0u8; MAX_STORE_BYTES];
    for index in 0..state.checkpoint.item_count() {
        let key = wire::item_key(
            header.digest,
            u16::try_from(index).map_err(|_| WireError::Invalid)?,
        );
        let (head, tail) = buf.split_at_mut(key.len());
        head.copy_from_slice(&key);
        let n = state.checkpoint.encode_item(index, tail)?;
        let input = buf.get(..key.len() + n).ok_or(WireError::BufferTooSmall)?;
        match ctx.activity::<Ack>(wire::STORE, input).await {
            Ok(Ack) => {}
            Err(Failure::Activity { .. }) => return Ok(None),
            Err(Failure::Decode(e)) => return Err(e.into()),
        }
    }
    Ok(Some(header))
}

/// One heartbeat: wait, sense, apply operator commands, run at most one
/// goal, settle, harvest, report.
// Embassy runs tasks on a single-core executor: the workflow need not
// be `Send`, and requiring it would forbid `Cell`-based journals and
// sinks for no benefit.
#[allow(clippy::future_not_send)]
async fn beat<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    seed: &AgentSeed,
    alarm: &mut dyn Alarm,
    state: &mut AgentState,
    sink: &mut dyn PulseSink,
) -> Result<(), AgentError> {
    let spec = match seed.heartbeat {
        Heartbeat::Boot { ticks } => TimerSpec::AfterBoot { ticks },
        Heartbeat::Persistent { .. } => TimerSpec::AtPersistentTime {
            instant: state.checkpoint.next_deadline,
        },
    };
    ctx.timer(spec, alarm).await;
    let beat = state.checkpoint.last_beat.next()?;
    let mut pulse = Pulse::new(beat);

    let sense = match ctx
        .activity::<Sense>(wire::SENSE, &beat.get().to_le_bytes())
        .await
    {
        Ok(sense) => sense,
        Err(Failure::Activity { .. }) => Sense::EMPTY,
        Err(Failure::Decode(e)) => return Err(e.into()),
    };
    if let Heartbeat::Persistent { period } = seed.heartbeat {
        // Missed beats are not owed: count from the later of the old
        // deadline and the sensed clock.
        let from = state.checkpoint.next_deadline.max(sense.now);
        state.checkpoint.next_deadline = from
            .checked_add(period)
            .ok_or(esper_core::Error::BeatOverflow)?;
    }
    for command in sense.commands() {
        if apply_command(seed, state, command, beat).is_err() {
            pulse.rejected = pulse.rejected.saturating_add(1);
        }
    }

    match state
        .checkpoint
        .ledger
        .pick(beat, |goal| sense.is_ready(goal))
    {
        Pick::Idle => {}
        Pick::Spawn(goal) => {
            let reservation = state.checkpoint.ledger.reservation(goal)?;
            let run = RunId::new(
                (u64::from(goal.get()) << 48) | (u64::from(state.checkpoint.spawn_seq) << 16),
            );
            let run_seed = spawn_seed(seed, state, goal, reservation, run)?;
            state.checkpoint.ledger.spawn(goal, run, reservation)?;
            state.checkpoint.spawn_seq = state
                .checkpoint
                .spawn_seq
                .checked_add(1)
                .ok_or(esper_core::Error::BeatOverflow)?;
            pulse.action = BeatAction::Spawned { goal, run };
            let request = RunRequest::Spawn {
                goal,
                seed: run_seed,
            };
            drive(ctx, &request, state, &mut pulse, beat, &seed.policy).await?;
        }
        Pick::Resume(goal) => {
            let run = state
                .checkpoint
                .ledger
                .get(goal)
                .and_then(|g| g.run)
                .ok_or_else(|| esper_core::Error::GoalUnknown { goal: goal.get() })?;
            state.checkpoint.ledger.resume(goal)?;
            pulse.action = BeatAction::Resumed { goal, run };
            let request = RunRequest::Resume { goal, run };
            drive(ctx, &request, state, &mut pulse, beat, &seed.policy).await?;
        }
    }

    state.checkpoint.last_beat = beat;
    pulse.counts = state.checkpoint.ledger.counts(beat, &seed.policy);
    state.pulse = Some(pulse);
    sink.pulse(&pulse);
    Ok(())
}

/// The seed a spawn of `goal` takes: the template with the run id, the
/// reservation, the goal's capabilities, and its brief.
fn spawn_seed(
    seed: &AgentSeed,
    state: &AgentState,
    goal: GoalId,
    reservation: Grant,
    run: RunId,
) -> Result<RunSeed, AgentError> {
    let entry = state
        .checkpoint
        .ledger
        .get(goal)
        .ok_or_else(|| esper_core::Error::GoalUnknown { goal: goal.get() })?;
    let brief = Brief::compose(&entry.objective, &recall(&state.checkpoint.memory, goal));
    let run_seed = RunSeed {
        id: run,
        model_turns: reservation.turns,
        mutations: reservation.mutations,
        capabilities: entry.capabilities,
        brief: Some(brief),
        parent: None,
        ..seed.template
    };
    run_seed.validate().map_err(AgentError::SeedInvalid)?;
    Ok(run_seed)
}

/// Drive one run activity and fold its report into the state: park, or
/// settle and harvest.
// Embassy runs tasks on a single-core executor: the workflow need not
// be `Send`, and requiring it would forbid `Cell`-based journals and
// sinks for no benefit.
#[allow(clippy::future_not_send)]
async fn drive<D: ActivityDispatcher, J: Journal>(
    ctx: &mut Ctx<'_, D, J>,
    request: &RunRequest,
    state: &mut AgentState,
    pulse: &mut Pulse,
    beat: Beat,
    policy: &SchedulePolicy,
) -> Result<(), AgentError> {
    let goal = request.goal();
    let run = request.run();
    let mut buf = [0u8; MAX_RUN_REQUEST_BYTES];
    let n = request.encode(&mut buf)?;
    let input = buf.get(..n).ok_or(WireError::BufferTooSmall)?;
    let report = match ctx.activity::<RunReport>(wire::RUN, input).await {
        Ok(report) => report,
        // The dispatcher could not drive the run at all: settle it as a
        // storage fault (non-retryable), refunding nothing.
        Err(Failure::Activity { .. }) => RunReport::Settled {
            status: TerminalStatus::StorageFault,
            refund_turns: 0,
            refund_mutations: 0,
            harvest: 0,
        },
        Err(Failure::Decode(e)) => return Err(e.into()),
    };
    match report {
        RunReport::Parked => {
            state.checkpoint.ledger.park(goal, beat)?;
            pulse.parked = true;
        }
        RunReport::Settled {
            status,
            refund_turns,
            refund_mutations,
            harvest,
        } => {
            let refund = Grant {
                turns: refund_turns,
                mutations: refund_mutations,
            };
            let event = state
                .checkpoint
                .ledger
                .settle(goal, status, refund, beat, policy)?;
            let mut harvested: u8 = 0;
            let limit = u8::try_from(MAX_HARVEST).unwrap_or(u8::MAX);
            for index in 0..harvest.min(limit) {
                let query = wire::harvest_query(run, index);
                match ctx.activity::<HarvestEntry>(wire::HARVEST, &query).await {
                    Ok(HarvestEntry(entry)) => {
                        // The supervisor, not the dispatcher, states
                        // provenance: it alone knows the beat.
                        let entry = MemoryEntry {
                            provenance: Provenance {
                                goal: Some(goal),
                                run: Some(run),
                                beat,
                            },
                            ..entry
                        };
                        state.checkpoint.memory.write(entry)?;
                        harvested = harvested.saturating_add(1);
                    }
                    Err(Failure::Activity { .. }) => {}
                    Err(Failure::Decode(e)) => return Err(e.into()),
                }
            }
            pulse.settled = Some(Settlement {
                goal,
                run,
                status,
                event,
                harvested,
            });
        }
    }
    Ok(())
}

/// Apply one operator command. A refused command changes nothing: every
/// arm validates before it mutates.
fn apply_command(
    seed: &AgentSeed,
    state: &mut AgentState,
    command: &Command,
    beat: Beat,
) -> Result<(), AgentError> {
    let checkpoint = &mut state.checkpoint;
    match *command {
        Command::AddGoal {
            objective,
            priority,
            cadence,
            envelope,
            capabilities,
        } => {
            let spec = GoalSpec {
                objective: objective.as_bytes(),
                priority,
                cadence,
                envelope,
                capabilities,
            };
            let goal = checkpoint
                .ledger
                .admit(&spec, &seed.template.capabilities, beat)?;
            checkpoint.ledger.insert(goal)?;
        }
        Command::Control { goal, event } => checkpoint.ledger.control(goal, event, beat)?,
        Command::Remember {
            name_digest,
            label,
            value,
        } => checkpoint.memory.write(MemoryEntry {
            key: MemoryKey::Note { name_digest },
            label,
            value,
            provenance: Provenance {
                goal: None,
                run: None,
                beat,
            },
            stamp: 0,
        })?,
        Command::Forget { name_digest } => {
            if !checkpoint.memory.forget(&MemoryKey::Note { name_digest }) {
                return Err(WireError::Invalid.into());
            }
        }
    }
    Ok(())
}
