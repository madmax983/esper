//! Versioned, integrity-checked checkpoints of the always-on
//! supervisor's derived state (SPEC §21.6).
//!
//! An always-on journal grows without bound; a checkpoint folds it.
//! Byte layout, little-endian, the §20.3 discipline:
//!
//! ```text
//! [version: 1][payload][integrity: u64 LE]
//!
//! payload:
//!   last_beat u64, spawn_seq u32, next_goal_id u16, next_stamp u32
//!   goals:  count u8, then per goal:
//!     id u16, objective (len u8 + bytes), priority u8,
//!     cadence (tag u8: 0 once / 1 every, span u32),
//!     envelope (per_run_turns u16, per_run_mutations u16,
//!               turns u32, mutations u32, runs u16),
//!     remaining (turns u32, mutations u32, runs u16),
//!     reserved (turns u16, mutations u16),
//!     capabilities (read_pins [8], read_count u8, write_pins [8],
//!                   write_count u8, sensors [4], sensor_count u8,
//!                   allow_timer u8, allow_status u8),
//!     status u8, next_due u64, failures u8, runs_started u32,
//!     last_status u8 (0xff = none), waiting_since u64,
//!     run (tag u8, id u64)
//!   memory: count u8, then per entry:
//!     key (tag u8, then: observation tool u8 + digest u64 |
//!          outcome goal u16 | note digest u64),
//!     label (len u8 + bytes), value (len u8 + bytes),
//!     provenance (goal u16, 0 = none; run tag u8 + id u64; beat u64),
//!     stamp u32
//! ```
//!
//! [`decode`] fails closed in the §20.3 order — truncated, then
//! version, then checksum, then corrupt — reusing the `Snapshot*`
//! error variants. Structural invariants (unique ids and keys, the
//! envelope never widened, no goal mid-run) are re-proven on decode by
//! [`GoalLedger::from_parts`] and [`Memory::from_parts`].

use crate::beat::{Beat, BeatSpan};
use crate::compact::{NOTE_MAX, Note};
use crate::error::Error;
use crate::goal::{
    Cadence, Envelope, Goal, GoalId, GoalLedger, GoalStatus, Grant, MAX_GOALS, Remaining,
};
use crate::ids::{Digest, RunId, ToolId};
use crate::mask::fnv1a64;
use crate::memory::{MAX_MEMORY, Memory, MemoryEntry, MemoryKey, Provenance};
use crate::registry::{Capabilities, MAX_PINS_PER_SET, MAX_SENSORS_PER_SET};
use crate::state::TerminalStatus;

/// Checkpoint format version. Bumped only with a migration path;
/// unknown versions fail closed.
pub const CHECKPOINT_VERSION: u8 = 1;

/// Length of the trailing integrity field.
const CHECKSUM_LEN: usize = 8;

/// Largest encoded goal.
const MAX_GOAL_BYTES: usize =
    2 + (1 + NOTE_MAX) + 1 + 5 + 14 + 10 + 4 + 25 + 1 + 8 + 1 + 4 + 1 + 8 + 9;
/// Largest encoded memory entry.
const MAX_ENTRY_BYTES: usize = 10 + (1 + NOTE_MAX) * 2 + 2 + 9 + 8 + 4;

/// The largest checkpoint [`encode`] can produce: a buffer of this
/// size always suffices.
pub const MAX_CHECKPOINT_BYTES: usize = 1
    + (8 + 4 + 2 + 4)
    + 1
    + MAX_GOALS * MAX_GOAL_BYTES
    + 1
    + MAX_MEMORY * MAX_ENTRY_BYTES
    + CHECKSUM_LEN;

/// The supervisor's derived state at a beat boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCheckpoint {
    /// The last completed beat.
    pub last_beat: Beat,
    /// The agent-wide spawn counter.
    pub spawn_seq: u32,
    /// The goal ledger (its id allocator included).
    pub ledger: GoalLedger,
    /// The memory store (its stamp allocator included).
    pub memory: Memory,
}

/// Exact bytes [`encode`] writes for `state`.
#[must_use]
pub fn encoded_len(state: &AgentCheckpoint) -> usize {
    let mut sink = Sink::counting();
    // Counting never fails: every write only advances the cursor.
    let _ = put_state(&mut sink, state);
    sink.pos + CHECKSUM_LEN
}

/// Encode `state` into `out`, returning the bytes written.
///
/// # Errors
///
/// [`Error::SnapshotBufferTooSmall`] when `out` is shorter than
/// [`encoded_len`].
pub fn encode(state: &AgentCheckpoint, out: &mut [u8]) -> Result<usize, Error> {
    let mut sink = Sink::writing(out);
    put_state(&mut sink, state)?;
    let end = sink.pos;
    let integrity = fnv1a64(sink.out.get(..end).ok_or(Error::SnapshotBufferTooSmall)?);
    sink.put_u64(integrity)?;
    Ok(sink.pos)
}

/// Decode and re-validate a checkpoint.
///
/// # Errors
///
/// In order: [`Error::SnapshotTruncated`], then
/// [`Error::SnapshotVersionMismatch`], then
/// [`Error::SnapshotChecksumMismatch`], then [`Error::SnapshotCorrupt`]
/// when the payload passes the checksum but does not form a valid
/// state.
pub fn decode(bytes: &[u8]) -> Result<AgentCheckpoint, Error> {
    verify(bytes)?;
    let mut r = Reader {
        bytes,
        pos: 1,
        end: bytes.len() - CHECKSUM_LEN,
    };
    let last_beat = Beat::new(r.u64()?);
    let spawn_seq = r.u32()?;
    let next_goal_id = r.u16()?;
    let next_stamp = r.u32()?;

    let mut goals = [placeholder_goal(); MAX_GOALS];
    let count = r.count(MAX_GOALS)?;
    for slot in goals.iter_mut().take(count) {
        *slot = take_goal(&mut r)?;
    }
    let ledger = GoalLedger::from_parts(goals.get(..count).unwrap_or(&[]), next_goal_id)?;

    let mut entries = [placeholder_entry(); MAX_MEMORY];
    let count = r.count(MAX_MEMORY)?;
    for slot in entries.iter_mut().take(count) {
        *slot = take_entry(&mut r)?;
    }
    let memory = Memory::from_parts(entries.get(..count).unwrap_or(&[]), next_stamp)?;

    if r.pos != r.end {
        return Err(Error::SnapshotCorrupt);
    }
    Ok(AgentCheckpoint {
        last_beat,
        spawn_seq,
        ledger,
        memory,
    })
}

/// Check length, version, and integrity without decoding the payload.
///
/// # Errors
///
/// [`Error::SnapshotTruncated`], [`Error::SnapshotVersionMismatch`],
/// or [`Error::SnapshotChecksumMismatch`].
pub fn verify(bytes: &[u8]) -> Result<(), Error> {
    if bytes.len() < 1 + CHECKSUM_LEN {
        return Err(Error::SnapshotTruncated);
    }
    let version = *bytes.first().ok_or(Error::SnapshotTruncated)?;
    if version != CHECKPOINT_VERSION {
        return Err(Error::SnapshotVersionMismatch { found: version });
    }
    let end = bytes.len() - CHECKSUM_LEN;
    let stored = bytes
        .get(end..)
        .and_then(|tail| <[u8; CHECKSUM_LEN]>::try_from(tail).ok())
        .ok_or(Error::SnapshotTruncated)?;
    if u64::from_le_bytes(stored) != fnv1a64(bytes.get(..end).ok_or(Error::SnapshotTruncated)?) {
        return Err(Error::SnapshotChecksumMismatch);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Encoding

/// A bounded writer; in counting mode it only advances the cursor.
struct Sink<'a> {
    out: &'a mut [u8],
    pos: usize,
    counting: bool,
}

impl<'a> Sink<'a> {
    const fn counting() -> Self {
        Self {
            out: &mut [],
            pos: 0,
            counting: true,
        }
    }

    const fn writing(out: &'a mut [u8]) -> Self {
        Self {
            out,
            pos: 0,
            counting: false,
        }
    }

    fn put_bytes(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let end = self.pos.saturating_add(bytes.len());
        if !self.counting {
            self.out
                .get_mut(self.pos..end)
                .ok_or(Error::SnapshotBufferTooSmall)?
                .copy_from_slice(bytes);
        }
        self.pos = end;
        Ok(())
    }

    fn put_u8(&mut self, v: u8) -> Result<(), Error> {
        self.put_bytes(&[v])
    }

    fn put_u16(&mut self, v: u16) -> Result<(), Error> {
        self.put_bytes(&v.to_le_bytes())
    }

    fn put_u32(&mut self, v: u32) -> Result<(), Error> {
        self.put_bytes(&v.to_le_bytes())
    }

    fn put_u64(&mut self, v: u64) -> Result<(), Error> {
        self.put_bytes(&v.to_le_bytes())
    }

    fn put_note(&mut self, note: &Note) -> Result<(), Error> {
        let bytes = note.as_bytes();
        self.put_u8(u8::try_from(bytes.len()).map_err(|_| Error::SnapshotCorrupt)?)?;
        self.put_bytes(bytes)
    }

    fn put_run(&mut self, run: Option<RunId>) -> Result<(), Error> {
        self.put_u8(u8::from(run.is_some()))?;
        self.put_u64(run.map_or(0, RunId::get))
    }

    fn put_count(&mut self, n: usize) -> Result<(), Error> {
        self.put_u8(u8::try_from(n).map_err(|_| Error::SnapshotCorrupt)?)
    }
}

fn put_state(s: &mut Sink<'_>, state: &AgentCheckpoint) -> Result<(), Error> {
    s.put_u8(CHECKPOINT_VERSION)?;
    s.put_u64(state.last_beat.get())?;
    s.put_u32(state.spawn_seq)?;
    s.put_u16(state.ledger.next_id())?;
    s.put_u32(state.memory.next_stamp())?;
    s.put_count(state.ledger.len())?;
    for goal in state.ledger.goals() {
        put_goal(s, goal)?;
    }
    s.put_count(state.memory.len())?;
    for entry in state.memory.entries() {
        put_entry(s, entry)?;
    }
    Ok(())
}

fn put_goal(s: &mut Sink<'_>, g: &Goal) -> Result<(), Error> {
    s.put_u16(g.id.get())?;
    s.put_note(&g.objective)?;
    s.put_u8(g.priority)?;
    match g.cadence {
        Cadence::Once => {
            s.put_u8(0)?;
            s.put_u32(0)?;
        }
        Cadence::Every(span) => {
            s.put_u8(1)?;
            s.put_u32(span.get())?;
        }
    }
    s.put_u16(g.envelope.per_run_turns)?;
    s.put_u16(g.envelope.per_run_mutations)?;
    s.put_u32(g.envelope.turns)?;
    s.put_u32(g.envelope.mutations)?;
    s.put_u16(g.envelope.runs)?;
    s.put_u32(g.remaining.turns)?;
    s.put_u32(g.remaining.mutations)?;
    s.put_u16(g.remaining.runs)?;
    s.put_u16(g.reserved.turns)?;
    s.put_u16(g.reserved.mutations)?;
    let caps = &g.capabilities;
    s.put_bytes(&caps.read_pins)?;
    s.put_u8(caps.read_count)?;
    s.put_bytes(&caps.write_pins)?;
    s.put_u8(caps.write_count)?;
    s.put_bytes(&caps.sensors)?;
    s.put_u8(caps.sensor_count)?;
    s.put_u8(u8::from(caps.allow_timer))?;
    s.put_u8(u8::from(caps.allow_status))?;
    s.put_u8(g.status.code())?;
    s.put_u64(g.next_due.get())?;
    s.put_u8(g.failures)?;
    s.put_u32(g.runs_started)?;
    s.put_u8(g.last_status.map_or(0xff, TerminalStatus::code))?;
    s.put_u64(g.waiting_since.get())?;
    s.put_run(g.run)
}

fn put_entry(s: &mut Sink<'_>, e: &MemoryEntry) -> Result<(), Error> {
    s.put_u8(e.key.tag())?;
    match e.key {
        MemoryKey::Observation { tool, args_digest } => {
            s.put_u8(tool.get())?;
            s.put_u64(args_digest.get())?;
        }
        MemoryKey::Outcome { goal } => s.put_u16(goal.get())?,
        MemoryKey::Note { name_digest } => s.put_u64(name_digest.get())?,
    }
    s.put_note(&e.label)?;
    s.put_note(&e.value)?;
    s.put_u16(e.provenance.goal.map_or(0, GoalId::get))?;
    s.put_run(e.provenance.run)?;
    s.put_u64(e.provenance.beat.get())?;
    s.put_u32(e.stamp)
}

// ---------------------------------------------------------------------
// Decoding

/// A bounded reader over the payload region.
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
    end: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], Error> {
        let next = self.pos.saturating_add(n);
        if next > self.end {
            return Err(Error::SnapshotTruncated);
        }
        let slice = self
            .bytes
            .get(self.pos..next)
            .ok_or(Error::SnapshotTruncated)?;
        self.pos = next;
        Ok(slice)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N], Error> {
        <[u8; N]>::try_from(self.take(N)?).map_err(|_| Error::SnapshotTruncated)
    }

    fn u8(&mut self) -> Result<u8, Error> {
        Ok(u8::from_le_bytes(self.array()?))
    }

    fn u16(&mut self) -> Result<u16, Error> {
        Ok(u16::from_le_bytes(self.array()?))
    }

    fn u32(&mut self) -> Result<u32, Error> {
        Ok(u32::from_le_bytes(self.array()?))
    }

    fn u64(&mut self) -> Result<u64, Error> {
        Ok(u64::from_le_bytes(self.array()?))
    }

    fn flag(&mut self) -> Result<bool, Error> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(Error::SnapshotCorrupt),
        }
    }

    fn count(&mut self, capacity: usize) -> Result<usize, Error> {
        let n = usize::from(self.u8()?);
        if n > capacity {
            return Err(Error::SnapshotCorrupt);
        }
        Ok(n)
    }

    fn note(&mut self) -> Result<Note, Error> {
        let len = usize::from(self.u8()?);
        if len > NOTE_MAX {
            return Err(Error::SnapshotCorrupt);
        }
        Note::from_bytes(self.take(len)?).map_err(|_| Error::SnapshotCorrupt)
    }

    fn run(&mut self) -> Result<Option<RunId>, Error> {
        let present = self.flag()?;
        let id = self.u64()?;
        match (present, id) {
            (true, id) => Ok(Some(RunId::new(id))),
            (false, 0) => Ok(None),
            (false, _) => Err(Error::SnapshotCorrupt),
        }
    }
}

fn take_capabilities(r: &mut Reader<'_>) -> Result<Capabilities, Error> {
    let caps = Capabilities {
        read_pins: r.array::<MAX_PINS_PER_SET>()?,
        read_count: r.u8()?,
        write_pins: r.array::<MAX_PINS_PER_SET>()?,
        write_count: r.u8()?,
        sensors: r.array::<MAX_SENSORS_PER_SET>()?,
        sensor_count: r.u8()?,
        allow_timer: r.flag()?,
        allow_status: r.flag()?,
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
        return Err(Error::SnapshotCorrupt);
    }
    Ok(caps)
}

fn take_goal(r: &mut Reader<'_>) -> Result<Goal, Error> {
    let id = GoalId::new(r.u16()?).ok_or(Error::SnapshotCorrupt)?;
    let objective = r.note()?;
    let priority = r.u8()?;
    let cadence = match (r.u8()?, r.u32()?) {
        (0, 0) => Cadence::Once,
        (1, span) => Cadence::Every(BeatSpan::new(span).ok_or(Error::SnapshotCorrupt)?),
        _ => return Err(Error::SnapshotCorrupt),
    };
    let envelope = Envelope {
        per_run_turns: r.u16()?,
        per_run_mutations: r.u16()?,
        turns: r.u32()?,
        mutations: r.u32()?,
        runs: r.u16()?,
    };
    envelope.validate().map_err(|_| Error::SnapshotCorrupt)?;
    let remaining = Remaining {
        turns: r.u32()?,
        mutations: r.u32()?,
        runs: r.u16()?,
    };
    let reserved = Grant {
        turns: r.u16()?,
        mutations: r.u16()?,
    };
    let capabilities = take_capabilities(r)?;
    let status = GoalStatus::from_code(r.u8()?).ok_or(Error::SnapshotCorrupt)?;
    let next_due = Beat::new(r.u64()?);
    let failures = r.u8()?;
    let runs_started = r.u32()?;
    let last_status = match r.u8()? {
        0xff => None,
        code => Some(TerminalStatus::from_code(code).map_err(|_| Error::SnapshotCorrupt)?),
    };
    let waiting_since = Beat::new(r.u64()?);
    let run = r.run()?;
    // A goal holds a run exactly while it is busy.
    if status.is_busy() != run.is_some() {
        return Err(Error::SnapshotCorrupt);
    }
    Ok(Goal {
        id,
        objective,
        priority,
        cadence,
        envelope,
        remaining,
        reserved,
        capabilities,
        status,
        next_due,
        failures,
        runs_started,
        last_status,
        waiting_since,
        run,
    })
}

fn take_entry(r: &mut Reader<'_>) -> Result<MemoryEntry, Error> {
    let key = match r.u8()? {
        0 => MemoryKey::Observation {
            tool: ToolId::new(r.u8()?),
            args_digest: Digest::new(r.u64()?),
        },
        1 => MemoryKey::Outcome {
            goal: GoalId::new(r.u16()?).ok_or(Error::SnapshotCorrupt)?,
        },
        2 => MemoryKey::Note {
            name_digest: Digest::new(r.u64()?),
        },
        _ => return Err(Error::SnapshotCorrupt),
    };
    let label = r.note()?;
    let value = r.note()?;
    let goal = match r.u16()? {
        0 => None,
        id => Some(GoalId::new(id).ok_or(Error::SnapshotCorrupt)?),
    };
    let run = r.run()?;
    let beat = Beat::new(r.u64()?);
    let stamp = r.u32()?;
    Ok(MemoryEntry {
        key,
        label,
        value,
        provenance: Provenance { goal, run, beat },
        stamp,
    })
}

/// Filler for the fixed decode arrays; never escapes [`decode`].
const fn placeholder_goal() -> Goal {
    Goal {
        id: GoalId::MIN,
        objective: Note::EMPTY,
        priority: 0,
        cadence: Cadence::Once,
        envelope: Envelope {
            per_run_turns: 0,
            per_run_mutations: 0,
            turns: 0,
            mutations: 0,
            runs: 0,
        },
        remaining: Remaining {
            turns: 0,
            mutations: 0,
            runs: 0,
        },
        reserved: Grant::ZERO,
        capabilities: Capabilities::empty(),
        status: GoalStatus::Active,
        next_due: Beat::ZERO,
        failures: 0,
        runs_started: 0,
        last_status: None,
        waiting_since: Beat::ZERO,
        run: None,
    }
}

/// Filler for the fixed decode arrays; never escapes [`decode`].
const fn placeholder_entry() -> MemoryEntry {
    MemoryEntry {
        key: MemoryKey::Note {
            name_digest: Digest::new(0),
        },
        label: Note::EMPTY,
        value: Note::EMPTY,
        provenance: Provenance {
            goal: None,
            run: None,
            beat: Beat::ZERO,
        },
        stamp: 0,
    }
}
