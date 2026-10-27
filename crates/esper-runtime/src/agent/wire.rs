//! The supervisor's activity payloads, one Waymaker record each
//! (SPEC §21.5).
//!
//! On the device every record is staged through one 512-byte scratch
//! page (`waymaker_core::budget::SCRATCH_PAGE_BYTES`), so every input
//! and every answer here has a compile-time maximum no larger than
//! [`PAGE_RECORD_MAX`]. Encoders write into caller-owned buffers;
//! decoders read borrowed bytes; nothing allocates.
//!
//! | Kind | Input | Answer |
//! |---|---|---|
//! | [`SENSE`] | the beat (`u64`) | [`Sense`]: persistent-clock reading, goals with input waiting, operator [`Command`]s |
//! | [`RUN`] | [`RunRequest`]: spawn (full seed + brief) or resume | [`RunReport`]: parked, or settled with refund and harvest count |
//! | [`HARVEST`] | run id + entry index | one [`MemoryEntry`] |
//! | [`STORE`] | checkpoint digest + item index + item | empty acknowledgement |
//! | [`LOAD`] | checkpoint digest + item index | one checkpoint item |
//!
//! Operator text crosses the device boundary in [`SENSE`] answers, which
//! are durable records — so the dispatcher masks it **before** it
//! answers ([`Command::add_goal`], [`Command::remember`] call
//! [`esper_core::memory::ingest`]). Decoding re-validates every field
//! and fails closed.

use esper_core::beat::BeatSpan;
use esper_core::brief::{BRIEF_CAP, Brief};
use esper_core::checkpoint::{MAX_ENTRY_RECORD_BYTES, MAX_ITEM_BYTES, decode_entry, encode_entry};
use esper_core::compact::{NOTE_MAX, Note};
use esper_core::goal::{Cadence, Envelope, GoalEvent, GoalId, MAX_GOALS};
use esper_core::ids::{Digest, RunId};
use esper_core::memory::{MemoryEntry, MemoryKey, ingest};
use esper_core::registry::{Capabilities, MAX_PINS_PER_SET, MAX_SENSORS_PER_SET};
use esper_core::state::TerminalStatus;
use thiserror::Error;
use waymaker_core::ActivityKind;
use waymaker_core::budget::SCRATCH_PAGE_BYTES;
use waymaker_embassy::Decode;

use crate::backend::InferenceSettings;
use crate::seed::RunSeed;

/// The largest record payload the supervisor stages: one scratch page,
/// less the flash frame overhead and seal headroom.
pub const PAGE_RECORD_MAX: usize = SCRATCH_PAGE_BYTES - 32;

/// Sense the world: clock, waiting input, operator commands.
pub const SENSE: ActivityKind = ActivityKind(0x0100);
/// Drive one goal run to its terminal result or its next `Ask`.
pub const RUN: ActivityKind = ActivityKind(0x0101);
/// Read one harvested memory entry of a settled run.
pub const HARVEST: ActivityKind = ActivityKind(0x0102);
/// Store one checkpoint item.
pub const STORE: ActivityKind = ActivityKind(0x0103);
/// Load one checkpoint item.
pub const LOAD: ActivityKind = ActivityKind(0x0104);

/// Operator commands one [`Sense`] answer may carry.
pub const MAX_COMMANDS: usize = 3;

/// Why a payload does not decode. Replay hands the same bytes back, so
/// a decode failure is a workflow fault, never retried.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WireError {
    /// The payload ended before a field completed.
    #[error("payload truncated")]
    Truncated,
    /// Bytes follow the last field.
    #[error("trailing bytes after the payload")]
    Trailing,
    /// A tag or field value is outside its closed set.
    #[error("payload field out of range")]
    Invalid,
    /// An encoder's output buffer is too short.
    #[error("output buffer too small")]
    BufferTooSmall,
    /// A core rule refused a decoded value.
    #[error("core rule refused a payload value: {0}")]
    Core(#[from] esper_core::Error),
}

// ---------------------------------------------------------------------
// Bounded writer and reader

/// A bounded writer over a caller-owned buffer.
pub(crate) struct Writer<'a> {
    out: &'a mut [u8],
    pos: usize,
}

impl<'a> Writer<'a> {
    pub(crate) const fn new(out: &'a mut [u8]) -> Self {
        Self { out, pos: 0 }
    }

    pub(crate) const fn len(&self) -> usize {
        self.pos
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) -> Result<(), WireError> {
        let end = self.pos.saturating_add(bytes.len());
        self.out
            .get_mut(self.pos..end)
            .ok_or(WireError::BufferTooSmall)?
            .copy_from_slice(bytes);
        self.pos = end;
        Ok(())
    }

    pub(crate) fn u8(&mut self, v: u8) -> Result<(), WireError> {
        self.bytes(&[v])
    }

    pub(crate) fn u16(&mut self, v: u16) -> Result<(), WireError> {
        self.bytes(&v.to_le_bytes())
    }

    pub(crate) fn u32(&mut self, v: u32) -> Result<(), WireError> {
        self.bytes(&v.to_le_bytes())
    }

    pub(crate) fn u64(&mut self, v: u64) -> Result<(), WireError> {
        self.bytes(&v.to_le_bytes())
    }

    pub(crate) fn note(&mut self, note: &Note) -> Result<(), WireError> {
        let bytes = note.as_bytes();
        self.u8(u8::try_from(bytes.len()).map_err(|_| WireError::Invalid)?)?;
        self.bytes(bytes)
    }

    pub(crate) fn capabilities(&mut self, caps: &Capabilities) -> Result<(), WireError> {
        self.bytes(&caps.read_pins)?;
        self.u8(caps.read_count)?;
        self.bytes(&caps.write_pins)?;
        self.u8(caps.write_count)?;
        self.bytes(&caps.sensors)?;
        self.u8(caps.sensor_count)?;
        self.u8(u8::from(caps.allow_timer))?;
        self.u8(u8::from(caps.allow_status))
    }
}

/// A bounded reader over borrowed bytes.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    pub(crate) fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        let end = self.pos.saturating_add(n);
        let slice = self.bytes.get(self.pos..end).ok_or(WireError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    pub(crate) fn array<const N: usize>(&mut self) -> Result<[u8; N], WireError> {
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| WireError::Truncated)
    }

    pub(crate) fn u8(&mut self) -> Result<u8, WireError> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    pub(crate) fn u16(&mut self) -> Result<u16, WireError> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    pub(crate) fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    pub(crate) fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    pub(crate) fn flag(&mut self) -> Result<bool, WireError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(WireError::Invalid),
        }
    }

    pub(crate) fn note(&mut self) -> Result<Note, WireError> {
        let len = usize::from(self.u8()?);
        if len > NOTE_MAX {
            return Err(WireError::Invalid);
        }
        Ok(Note::from_bytes(self.take(len)?)?)
    }

    fn goal(&mut self) -> Result<GoalId, WireError> {
        GoalId::new(self.u16()?).ok_or(WireError::Invalid)
    }

    fn capabilities(&mut self) -> Result<Capabilities, WireError> {
        let caps = Capabilities {
            read_pins: self.array::<MAX_PINS_PER_SET>()?,
            read_count: self.u8()?,
            write_pins: self.array::<MAX_PINS_PER_SET>()?,
            write_count: self.u8()?,
            sensors: self.array::<MAX_SENSORS_PER_SET>()?,
            sensor_count: self.u8()?,
            allow_timer: self.flag()?,
            allow_status: self.flag()?,
        };
        let pins_ok = |set: &[u8], count: u8| {
            set.get(..usize::from(count))
                .is_some_and(|valid| valid.iter().all(|pin| *pin < 8))
        };
        let sensors_ok = caps
            .sensors
            .get(..usize::from(caps.sensor_count))
            .is_some_and(|valid| valid.iter().all(|sensor| *sensor < 4));
        if !pins_ok(&caps.read_pins, caps.read_count)
            || !pins_ok(&caps.write_pins, caps.write_count)
            || !sensors_ok
        {
            return Err(WireError::Invalid);
        }
        Ok(caps)
    }

    pub(crate) const fn end(&self) -> Result<(), WireError> {
        if self.pos == self.bytes.len() {
            Ok(())
        } else {
            Err(WireError::Trailing)
        }
    }
}

// ---------------------------------------------------------------------
// SENSE

/// One operator command, already masked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Add a goal. The ledger assigns its id.
    AddGoal {
        /// The masked, bounded objective.
        objective: Note,
        /// Higher runs first.
        priority: u8,
        /// One-shot or standing.
        cadence: Cadence,
        /// The cumulative budget envelope.
        envelope: Envelope,
        /// What the goal's runs may touch.
        capabilities: Capabilities,
    },
    /// Pause, unpause, or retire a goal.
    Control {
        /// The goal.
        goal: GoalId,
        /// An operator event (`Pause`, `Unpause`, `Retire`).
        event: GoalEvent,
    },
    /// Remember an operator note (pinned).
    Remember {
        /// FNV-1a-64 of the note's name.
        name_digest: Digest,
        /// The masked name, as the label.
        label: Note,
        /// The masked value.
        value: Note,
    },
    /// Forget an operator note.
    Forget {
        /// FNV-1a-64 of the note's name.
        name_digest: Digest,
    },
}

/// The largest encoded [`Command`]: a `Remember`.
const MAX_COMMAND_BYTES: usize = 1 + 8 + 2 * (1 + NOTE_MAX);

impl Command {
    /// An `AddGoal` from raw operator text: the objective is masked
    /// here, before it can reach a durable record.
    ///
    /// # Errors
    ///
    /// [`WireError::Core`] when the objective is overlong.
    pub fn add_goal(
        objective: &[u8],
        priority: u8,
        cadence: Cadence,
        envelope: Envelope,
        capabilities: Capabilities,
    ) -> Result<Self, WireError> {
        Ok(Self::AddGoal {
            objective: ingest(objective)?,
            priority,
            cadence,
            envelope,
            capabilities,
        })
    }

    /// A `Remember` from raw operator text: name and value are masked
    /// here, before they can reach a durable record.
    ///
    /// # Errors
    ///
    /// [`WireError::Core`] when either input is overlong.
    pub fn remember(name: &[u8], value: &[u8]) -> Result<Self, WireError> {
        Ok(Self::Remember {
            name_digest: Digest::of_bytes(name),
            label: ingest(name)?,
            value: ingest(value)?,
        })
    }

    /// A `Forget` for the note named `name`.
    #[must_use]
    pub fn forget(name: &[u8]) -> Self {
        Self::Forget {
            name_digest: Digest::of_bytes(name),
        }
    }

    /// The memory key a `Remember` / `Forget` addresses.
    #[must_use]
    pub const fn note_key(name_digest: Digest) -> MemoryKey {
        MemoryKey::Note { name_digest }
    }

    fn put(&self, w: &mut Writer<'_>) -> Result<(), WireError> {
        match self {
            Self::AddGoal {
                objective,
                priority,
                cadence,
                envelope,
                capabilities,
            } => {
                w.u8(0)?;
                w.note(objective)?;
                w.u8(*priority)?;
                match cadence {
                    Cadence::Once => {
                        w.u8(0)?;
                        w.u32(0)?;
                    }
                    Cadence::Every(span) => {
                        w.u8(1)?;
                        w.u32(span.get())?;
                    }
                }
                w.u16(envelope.per_run_turns)?;
                w.u16(envelope.per_run_mutations)?;
                w.u32(envelope.turns)?;
                w.u32(envelope.mutations)?;
                w.u16(envelope.runs)?;
                w.capabilities(capabilities)
            }
            Self::Control { goal, event } => {
                w.u8(1)?;
                w.u16(goal.get())?;
                w.u8(event.code())
            }
            Self::Remember {
                name_digest,
                label,
                value,
            } => {
                w.u8(2)?;
                w.u64(name_digest.get())?;
                w.note(label)?;
                w.note(value)
            }
            Self::Forget { name_digest } => {
                w.u8(3)?;
                w.u64(name_digest.get())
            }
        }
    }

    fn take(r: &mut Reader<'_>) -> Result<Self, WireError> {
        match r.u8()? {
            0 => {
                let objective = r.note()?;
                let priority = r.u8()?;
                let cadence = match (r.u8()?, r.u32()?) {
                    (0, 0) => Cadence::Once,
                    (1, span) => Cadence::Every(BeatSpan::new(span).ok_or(WireError::Invalid)?),
                    _ => return Err(WireError::Invalid),
                };
                let envelope = Envelope {
                    per_run_turns: r.u16()?,
                    per_run_mutations: r.u16()?,
                    turns: r.u32()?,
                    mutations: r.u32()?,
                    runs: r.u16()?,
                };
                let capabilities = r.capabilities()?;
                Ok(Self::AddGoal {
                    objective,
                    priority,
                    cadence,
                    envelope,
                    capabilities,
                })
            }
            1 => {
                let goal = r.goal()?;
                let event = GoalEvent::from_code(r.u8()?)
                    .filter(|event| event.is_operator())
                    .ok_or(WireError::Invalid)?;
                Ok(Self::Control { goal, event })
            }
            2 => Ok(Self::Remember {
                name_digest: Digest::new(r.u64()?),
                label: r.note()?,
                value: r.note()?,
            }),
            3 => Ok(Self::Forget {
                name_digest: Digest::new(r.u64()?),
            }),
            _ => Err(WireError::Invalid),
        }
    }
}

/// The world, sensed once per beat. The [`SENSE`] answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sense {
    /// The persistent clock's reading (`0` without one).
    pub now: u64,
    ready: [Option<GoalId>; MAX_GOALS],
    commands: [Option<Command>; MAX_COMMANDS],
}

/// The largest encoded [`Sense`].
pub const MAX_SENSE_BYTES: usize = 8 + 1 + 2 * MAX_GOALS + 1 + MAX_COMMANDS * MAX_COMMAND_BYTES;

impl Sense {
    /// Nothing sensed: the answer when the [`SENSE`] activity failed.
    pub const EMPTY: Self = Self {
        now: 0,
        ready: [None; MAX_GOALS],
        commands: [None; MAX_COMMANDS],
    };

    /// A sense reading at `now`.
    #[must_use]
    pub const fn at(now: u64) -> Self {
        Self { now, ..Self::EMPTY }
    }

    /// Mark `goal` as having input waiting.
    ///
    /// # Errors
    ///
    /// [`WireError::Invalid`] past [`MAX_GOALS`] ready goals.
    pub fn ready(&mut self, goal: GoalId) -> Result<(), WireError> {
        if self.is_ready(goal) {
            return Ok(());
        }
        let slot = self
            .ready
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(WireError::Invalid)?;
        *slot = Some(goal);
        Ok(())
    }

    /// Add an operator command.
    ///
    /// # Errors
    ///
    /// [`WireError::Invalid`] past [`MAX_COMMANDS`].
    pub fn command(&mut self, command: Command) -> Result<(), WireError> {
        let slot = self
            .commands
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(WireError::Invalid)?;
        *slot = Some(command);
        Ok(())
    }

    /// Whether `goal` has input waiting.
    #[must_use]
    pub fn is_ready(&self, goal: GoalId) -> bool {
        self.ready.contains(&Some(goal))
    }

    /// The operator commands, in order.
    pub fn commands(&self) -> impl Iterator<Item = &Command> {
        self.commands.iter().flatten()
    }

    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        w.u64(self.now)?;
        let ready = self.ready.iter().flatten().count();
        w.u8(u8::try_from(ready).map_err(|_| WireError::Invalid)?)?;
        for goal in self.ready.iter().flatten() {
            w.u16(goal.get())?;
        }
        let commands = self.commands().count();
        w.u8(u8::try_from(commands).map_err(|_| WireError::Invalid)?)?;
        for command in self.commands() {
            command.put(&mut w)?;
        }
        Ok(w.len())
    }
}

impl Decode for Sense {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let mut sense = Self::at(r.u64()?);
        let ready = usize::from(r.u8()?);
        if ready > MAX_GOALS {
            return Err(WireError::Invalid);
        }
        for _ in 0..ready {
            sense.ready(r.goal()?)?;
        }
        let commands = usize::from(r.u8()?);
        if commands > MAX_COMMANDS {
            return Err(WireError::Invalid);
        }
        for _ in 0..commands {
            sense.command(Command::take(&mut r)?)?;
        }
        r.end()?;
        Ok(sense)
    }
}

// ---------------------------------------------------------------------
// RUN

/// What the [`RUN`] activity is asked to do.
///
/// `Spawn` carries a whole seed and `Resume` two ids; the size gap is
/// accepted rather than boxed — there is no heap, and a request lives on
/// the stack only while it is encoded or decoded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)]
pub enum RunRequest {
    /// Start a new run under this exact seed (brief included). The seed
    /// travels in the committed input, so a redelivery drives the same
    /// run, and the dispatcher never composes one of its own.
    Spawn {
        /// The goal served.
        goal: GoalId,
        /// The run's seed.
        seed: RunSeed,
    },
    /// Resume a parked run: its input arrived.
    Resume {
        /// The goal served.
        goal: GoalId,
        /// The parked run.
        run: RunId,
    },
}

/// The largest encoded [`RunRequest`]: a spawn with a full brief.
pub const MAX_RUN_REQUEST_BYTES: usize =
    1 + 2 + 8 + 2 + 2 + 4 + 4 + 8 + 25 + 2 + 8 + 4 + 9 + 2 + BRIEF_CAP;

impl RunRequest {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short;
    /// [`WireError::Invalid`] for a seed this protocol cannot carry (a
    /// lineage — spawns are always root runs — or no brief).
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        match self {
            Self::Spawn { goal, seed } => {
                let Some(brief) = seed.brief.as_ref().filter(|_| seed.parent.is_none()) else {
                    return Err(WireError::Invalid);
                };
                w.u8(0)?;
                w.u16(goal.get())?;
                w.u64(seed.id.get())?;
                w.u16(seed.model_turns)?;
                w.u16(seed.mutations)?;
                w.u32(seed.input_tokens)?;
                w.u32(seed.output_tokens)?;
                w.u64(seed.elapsed_ms)?;
                w.capabilities(&seed.capabilities)?;
                w.u16(seed.workflow_version)?;
                w.u64(seed.model_bundle)?;
                w.bytes(&seed.inference.bytes())?;
                w.u8(u8::from(seed.context_budget_bytes.is_some()))?;
                w.u64(seed.context_budget_bytes.unwrap_or(0))?;
                let bytes = brief.as_bytes();
                w.u16(u16::try_from(bytes.len()).map_err(|_| WireError::Invalid)?)?;
                w.bytes(bytes)?;
            }
            Self::Resume { goal, run } => {
                w.u8(1)?;
                w.u16(goal.get())?;
                w.u64(run.get())?;
            }
        }
        Ok(w.len())
    }

    /// Decode and re-validate a request: the dispatcher's side, which
    /// must not trust what it was handed.
    ///
    /// # Errors
    ///
    /// [`WireError`] for malformed bytes, a malformed brief, or a seed
    /// that fails [`RunSeed::validate`].
    pub fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let request = match r.u8()? {
            0 => {
                let goal = r.goal()?;
                let id = RunId::new(r.u64()?);
                let model_turns = r.u16()?;
                let mutations = r.u16()?;
                let input_tokens = r.u32()?;
                let output_tokens = r.u32()?;
                let elapsed_ms = r.u64()?;
                let capabilities = r.capabilities()?;
                let workflow_version = r.u16()?;
                let model_bundle = r.u64()?;
                let inference = InferenceSettings {
                    max_prompt_bytes: r.u16()?,
                    max_output_bytes: r.u16()?,
                };
                let budget_present = r.flag()?;
                let budget = r.u64()?;
                let context_budget_bytes = match (budget_present, budget) {
                    (true, value) => Some(value),
                    (false, 0) => None,
                    (false, _) => return Err(WireError::Invalid),
                };
                let brief_len = usize::from(r.u16()?);
                let brief = Brief::from_bytes(r.take(brief_len)?)?;
                let seed = RunSeed {
                    id,
                    model_turns,
                    mutations,
                    input_tokens,
                    output_tokens,
                    elapsed_ms,
                    capabilities,
                    workflow_version,
                    model_bundle,
                    inference,
                    context_budget_bytes,
                    parent: None,
                    brief: Some(brief),
                };
                seed.validate().map_err(|_| WireError::Invalid)?;
                Self::Spawn { goal, seed }
            }
            1 => Self::Resume {
                goal: r.goal()?,
                run: RunId::new(r.u64()?),
            },
            _ => return Err(WireError::Invalid),
        };
        r.end()?;
        Ok(request)
    }

    /// The goal served.
    #[must_use]
    pub const fn goal(&self) -> GoalId {
        match self {
            Self::Spawn { goal, .. } | Self::Resume { goal, .. } => *goal,
        }
    }

    /// The run driven.
    #[must_use]
    pub const fn run(&self) -> RunId {
        match self {
            Self::Spawn { seed, .. } => seed.id,
            Self::Resume { run, .. } => *run,
        }
    }
}

/// How a [`RUN`] activity ended. Its answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunReport {
    /// The run suspended on `Ask` and awaits typed input.
    Parked,
    /// The run reached its terminal result.
    Settled {
        /// The terminal status.
        status: TerminalStatus,
        /// Unspent model turns.
        refund_turns: u16,
        /// Unspent mutations.
        refund_mutations: u16,
        /// Harvested entries, read one by one with [`HARVEST`].
        harvest: u8,
    },
}

/// The largest encoded [`RunReport`].
pub const MAX_RUN_REPORT_BYTES: usize = 1 + 1 + 2 + 2 + 1;

impl RunReport {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::BufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        let mut w = Writer::new(out);
        match self {
            Self::Parked => w.u8(0)?,
            Self::Settled {
                status,
                refund_turns,
                refund_mutations,
                harvest,
            } => {
                w.u8(1)?;
                w.u8(status.code())?;
                w.u16(*refund_turns)?;
                w.u16(*refund_mutations)?;
                w.u8(*harvest)?;
            }
        }
        Ok(w.len())
    }
}

impl Decode for RunReport {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut r = Reader::new(bytes);
        let report = match r.u8()? {
            0 => Self::Parked,
            1 => Self::Settled {
                status: TerminalStatus::from_code(r.u8()?).map_err(|_| WireError::Invalid)?,
                refund_turns: r.u16()?,
                refund_mutations: r.u16()?,
                harvest: r.u8()?,
            },
            _ => return Err(WireError::Invalid),
        };
        r.end()?;
        Ok(report)
    }
}

// ---------------------------------------------------------------------
// HARVEST, STORE, LOAD

/// Bytes of a [`HARVEST`] input: run id + entry index.
pub const HARVEST_QUERY_BYTES: usize = 8 + 1;

/// Encode a [`HARVEST`] input.
#[must_use]
pub const fn harvest_query(run: RunId, index: u8) -> [u8; HARVEST_QUERY_BYTES] {
    let mut out = [0u8; HARVEST_QUERY_BYTES];
    let (id, idx) = out.split_at_mut(8);
    id.copy_from_slice(&run.get().to_le_bytes());
    idx.copy_from_slice(&[index]);
    out
}

/// Decode a [`HARVEST`] input.
///
/// # Errors
///
/// [`WireError`] unless `bytes` is exactly a query.
pub fn read_harvest_query(bytes: &[u8]) -> Result<(RunId, u8), WireError> {
    let mut r = Reader::new(bytes);
    let run = RunId::new(r.u64()?);
    let index = r.u8()?;
    r.end()?;
    Ok((run, index))
}

/// A harvested entry: the [`HARVEST`] answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarvestEntry(pub MemoryEntry);

impl HarvestEntry {
    /// Encode into `out`, returning the bytes written.
    ///
    /// # Errors
    ///
    /// [`WireError::Core`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, WireError> {
        Ok(encode_entry(&self.0, out)?)
    }
}

impl Decode for HarvestEntry {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let entry = decode_entry(bytes)?;
        // The run harvests evidence, never operator notes, and the
        // supervisor stamps entries on write.
        if entry.pinned() || entry.stamp != 0 {
            return Err(WireError::Invalid);
        }
        Ok(Self(entry))
    }
}

/// Bytes of a [`LOAD`] input, and the prefix of a [`STORE`] input:
/// checkpoint digest + item index.
pub const ITEM_KEY_BYTES: usize = 8 + 2;

/// The largest [`STORE`] input: key plus one item.
pub const MAX_STORE_BYTES: usize = ITEM_KEY_BYTES + MAX_ITEM_BYTES;

/// Encode a checkpoint item key.
#[must_use]
pub const fn item_key(digest: u64, index: u16) -> [u8; ITEM_KEY_BYTES] {
    let mut out = [0u8; ITEM_KEY_BYTES];
    let (d, i) = out.split_at_mut(8);
    d.copy_from_slice(&digest.to_le_bytes());
    i.copy_from_slice(&index.to_le_bytes());
    out
}

/// Split a [`STORE`] input (or, with an empty item, a [`LOAD`] input)
/// into its key and item.
///
/// # Errors
///
/// [`WireError::Truncated`] for a short key; [`WireError::Invalid`] for
/// an item over [`MAX_ITEM_BYTES`].
pub fn read_item_key(bytes: &[u8]) -> Result<(u64, u16, &[u8]), WireError> {
    let mut r = Reader::new(bytes);
    let digest = r.u64()?;
    let index = r.u16()?;
    let item = bytes.get(ITEM_KEY_BYTES..).unwrap_or(&[]);
    if item.len() > MAX_ITEM_BYTES {
        return Err(WireError::Invalid);
    }
    Ok((digest, index, item))
}

/// The empty acknowledgement a [`STORE`] answers with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ack;

impl Decode for Ack {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        if bytes.is_empty() {
            Ok(Self)
        } else {
            Err(WireError::Trailing)
        }
    }
}

/// One checkpoint item, as [`LOAD`] answers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Item {
    bytes: [u8; MAX_ITEM_BYTES],
    len: usize,
}

impl Item {
    /// The item's bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

impl Decode for Item {
    type Error = WireError;

    fn decode(bytes: &[u8]) -> Result<Self, WireError> {
        let mut item = Self {
            bytes: [0; MAX_ITEM_BYTES],
            len: bytes.len(),
        };
        item.bytes
            .get_mut(..bytes.len())
            .ok_or(WireError::Invalid)?
            .copy_from_slice(bytes);
        Ok(item)
    }
}

// Every record fits one scratch page (CLAUDE.md, SPEC §21.5).
const _: () = assert!(MAX_SENSE_BYTES <= PAGE_RECORD_MAX);
const _: () = assert!(MAX_RUN_REQUEST_BYTES <= PAGE_RECORD_MAX);
const _: () = assert!(MAX_RUN_REPORT_BYTES <= PAGE_RECORD_MAX);
const _: () = assert!(MAX_ENTRY_RECORD_BYTES <= PAGE_RECORD_MAX);
const _: () = assert!(MAX_STORE_BYTES <= PAGE_RECORD_MAX);
const _: () = assert!(HARVEST_QUERY_BYTES <= PAGE_RECORD_MAX);
