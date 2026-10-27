//! Versioned, integrity-checked checkpoints of the always-on
//! supervisor's derived state, in page-sized items (SPEC §21.6).
//!
//! An always-on journal grows without bound; a checkpoint folds it.
//! On the device every Waymaker record must fit one scratch page, so a
//! checkpoint is never one blob: it is a fixed [`Header`] plus one
//! **item** per goal and per memory entry, each at most
//! [`MAX_ITEM_BYTES`]. The supervisor stores the items through
//! activities, retires its run with `continue_as_new` carrying only
//! the header, and the next run loads the items back into a
//! caller-owned [`AgentCheckpoint`] through [`Restore`] — no buffer
//! the size of the whole state ever exists.
//!
//! ```text
//! header (HEADER_BYTES = 37), little-endian:
//!   version u8, last_beat u64, spawn_seq u32, next_deadline u64,
//!   next_goal_id u16, next_stamp u32, goals u8, entries u8,
//!   digest u64   — FNV-1a-64 over every item's bytes, in order
//!
//! item: tag u8 (0 goal / 1 entry), then
//!   goal:  id u16, objective (len u8 + bytes), priority u8,
//!          cadence (tag u8: 0 once / 1 every, span u32),
//!          envelope (per_run_turns u16, per_run_mutations u16,
//!                    turns u32, mutations u32, runs u16),
//!          remaining (turns u32, mutations u32, runs u16),
//!          reserved (turns u16, mutations u16),
//!          capabilities (read_pins [8], read_count u8, write_pins [8],
//!                        write_count u8, sensors [4], sensor_count u8,
//!                        allow_timer u8, allow_status u8),
//!          status u8, next_due u64, failures u8, runs_started u32,
//!          last_status u8 (0xff = none), waiting_since u64,
//!          run (tag u8, id u64)
//!   entry: key (tag u8, then: observation tool u8 + digest u64 |
//!               outcome goal u16 | note digest u64),
//!          label (len u8 + bytes), value (len u8 + bytes),
//!          provenance (goal u16, 0 = none; run tag u8 + id u64;
//!                      beat u64),
//!          stamp u32
//! ```
//!
//! Goals come first, then entries, each in storage order. [`encode`] /
//! [`decode`] are the same layout as one blob with a trailing
//! FNV-1a-64 integrity field, for host tooling and tests. Decoding
//! fails closed in the §20.3 order — truncated, then version, then
//! checksum, then corrupt — reusing the `Snapshot*` error variants.
//! Structural invariants (unique ids, keys and stamps, the envelope
//! never widened, no goal mid-run) are re-proven per item by
//! [`GoalLedger::restore_goal`] and [`Memory::restore_entry`].

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

/// Length of the trailing integrity field of the blob form.
const CHECKSUM_LEN: usize = 8;

/// Exact bytes of an encoded [`Header`].
pub const HEADER_BYTES: usize = 1 + 8 + 4 + 8 + 2 + 4 + 1 + 1 + 8;

/// Largest encoded goal (without its item tag).
const MAX_GOAL_BYTES: usize =
    2 + (1 + NOTE_MAX) + 1 + 5 + 14 + 10 + 4 + 25 + 1 + 8 + 1 + 4 + 1 + 8 + 9;
/// Largest encoded memory entry (without its item tag).
const MAX_ENTRY_BYTES: usize = 10 + (1 + NOTE_MAX) * 2 + 2 + 9 + 8 + 4;

/// The largest item [`AgentCheckpoint::encode_item`] produces: one tag
/// byte plus the larger of a goal and an entry. Well under one
/// 512-byte Waymaker scratch page.
pub const MAX_ITEM_BYTES: usize = 1 + if MAX_GOAL_BYTES > MAX_ENTRY_BYTES {
    MAX_GOAL_BYTES
} else {
    MAX_ENTRY_BYTES
};

/// The largest blob [`encode`] can produce: a buffer of this size
/// always suffices. Host tooling only — the device stores items.
pub const MAX_CHECKPOINT_BYTES: usize = HEADER_BYTES
    + MAX_GOALS * (1 + MAX_GOAL_BYTES)
    + MAX_MEMORY * (1 + MAX_ENTRY_BYTES)
    + CHECKSUM_LEN;

/// Item tag: a goal.
const TAG_GOAL: u8 = 0;
/// Item tag: a memory entry.
const TAG_ENTRY: u8 = 1;

/// FNV-1a-64 offset basis: the digest of no bytes.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
/// FNV-1a-64 prime.
const FNV_PRIME: u64 = 0x0100_0000_01b3;

/// Fold `bytes` into a running FNV-1a-64 state. Streaming, so the
/// digest over every item never needs the items in one buffer.
fn fnv_update(mut hash: u64, bytes: &[u8]) -> u64 {
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

/// The supervisor's derived state at a beat boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AgentCheckpoint {
    /// The last completed beat.
    pub last_beat: Beat,
    /// The agent-wide spawn counter.
    pub spawn_seq: u32,
    /// The next heartbeat deadline on the persistent clock (`0` for a
    /// boot-clock heartbeat, or before the first beat).
    pub next_deadline: u64,
    /// The goal ledger (its id allocator included).
    pub ledger: GoalLedger,
    /// The memory store (its stamp allocator included).
    pub memory: Memory,
}

impl AgentCheckpoint {
    /// A fresh agent: no beat, no goals, no memory.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            last_beat: Beat::ZERO,
            spawn_seq: 0,
            next_deadline: 0,
            ledger: GoalLedger::new(),
            memory: Memory::new(),
        }
    }

    /// Items in this checkpoint: one per goal, then one per entry.
    #[must_use]
    pub fn item_count(&self) -> usize {
        self.ledger.len() + self.memory.len()
    }

    /// Encode item `index` into `out`, returning the bytes written
    /// (at most [`MAX_ITEM_BYTES`]).
    ///
    /// # Errors
    ///
    /// [`Error::SnapshotCorrupt`] for an index past [`Self::item_count`];
    /// [`Error::SnapshotBufferTooSmall`] when `out` is short.
    pub fn encode_item(&self, index: usize, out: &mut [u8]) -> Result<usize, Error> {
        let mut sink = Sink::writing(out);
        self.put_item(&mut sink, index)?;
        Ok(sink.pos)
    }

    fn put_item(&self, sink: &mut Sink<'_>, index: usize) -> Result<(), Error> {
        let goals = self.ledger.len();
        if let Some(goal) = self.ledger.goals().nth(index) {
            sink.put_u8(TAG_GOAL)?;
            return put_goal(sink, goal);
        }
        let entry = self
            .memory
            .entries()
            .nth(index - goals)
            .ok_or(Error::SnapshotCorrupt)?;
        sink.put_u8(TAG_ENTRY)?;
        put_entry(sink, entry)
    }

    /// The header for this checkpoint: counters, allocators, item
    /// counts, and the digest over every item's bytes.
    #[must_use]
    pub fn header(&self) -> Header {
        let mut digest = FNV_OFFSET;
        let mut item = [0u8; MAX_ITEM_BYTES];
        for index in 0..self.item_count() {
            // Every in-range item fits MAX_ITEM_BYTES by construction.
            if let Ok(len) = self.encode_item(index, &mut item) {
                digest = fnv_update(digest, item.get(..len).unwrap_or(&[]));
            }
        }
        Header {
            last_beat: self.last_beat,
            spawn_seq: self.spawn_seq,
            next_deadline: self.next_deadline,
            next_goal_id: self.ledger.next_id(),
            next_stamp: self.memory.next_stamp(),
            goals: u8::try_from(self.ledger.len()).unwrap_or(u8::MAX),
            entries: u8::try_from(self.memory.len()).unwrap_or(u8::MAX),
            digest,
        }
    }
}

impl Default for AgentCheckpoint {
    fn default() -> Self {
        Self::new()
    }
}

/// A checkpoint's fixed header: everything but the items, plus the
/// digest that binds them. The `continue_as_new` input on the device.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Header {
    /// The last completed beat.
    pub last_beat: Beat,
    /// The agent-wide spawn counter.
    pub spawn_seq: u32,
    /// The next persistent-clock heartbeat deadline.
    pub next_deadline: u64,
    /// The goal id allocator.
    pub next_goal_id: u16,
    /// The memory stamp allocator.
    pub next_stamp: u32,
    /// Goal items that follow.
    pub goals: u8,
    /// Entry items that follow the goals.
    pub entries: u8,
    /// FNV-1a-64 over every item's bytes, in order.
    pub digest: u64,
}

impl Header {
    /// Items the header promises.
    #[must_use]
    pub const fn items(&self) -> usize {
        self.goals as usize + self.entries as usize
    }

    /// Encode into `out` (exactly [`HEADER_BYTES`]).
    ///
    /// # Errors
    ///
    /// [`Error::SnapshotBufferTooSmall`] when `out` is short.
    pub fn encode(&self, out: &mut [u8]) -> Result<usize, Error> {
        let mut sink = Sink::writing(out);
        self.put(&mut sink)?;
        Ok(sink.pos)
    }

    fn put(&self, s: &mut Sink<'_>) -> Result<(), Error> {
        s.put_u8(CHECKPOINT_VERSION)?;
        s.put_u64(self.last_beat.get())?;
        s.put_u32(self.spawn_seq)?;
        s.put_u64(self.next_deadline)?;
        s.put_u16(self.next_goal_id)?;
        s.put_u32(self.next_stamp)?;
        s.put_u8(self.goals)?;
        s.put_u8(self.entries)?;
        s.put_u64(self.digest)
    }

    /// Decode exactly [`HEADER_BYTES`]. The version is checked before
    /// anything else, so a version bump reads as a version problem.
    ///
    /// # Errors
    ///
    /// [`Error::SnapshotVersionMismatch`] for a foreign version byte;
    /// [`Error::SnapshotTruncated`] for a short header;
    /// [`Error::SnapshotCorrupt`] for trailing bytes or item counts past
    /// the ledger's or the store's capacity.
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let version = *bytes.first().ok_or(Error::SnapshotTruncated)?;
        if version != CHECKPOINT_VERSION {
            return Err(Error::SnapshotVersionMismatch { found: version });
        }
        let mut r = Reader {
            bytes,
            pos: 1,
            end: bytes.len(),
        };
        let header = Self::take(&mut r)?;
        if r.pos != r.end {
            return Err(Error::SnapshotCorrupt);
        }
        Ok(header)
    }

    fn take(r: &mut Reader<'_>) -> Result<Self, Error> {
        let header = Self {
            last_beat: Beat::new(r.u64()?),
            spawn_seq: r.u32()?,
            next_deadline: r.u64()?,
            next_goal_id: r.u16()?,
            next_stamp: r.u32()?,
            goals: r.u8()?,
            entries: r.u8()?,
            digest: r.u64()?,
        };
        if usize::from(header.goals) > MAX_GOALS || usize::from(header.entries) > MAX_MEMORY {
            return Err(Error::SnapshotCorrupt);
        }
        Ok(header)
    }
}

/// An item-by-item restore into a caller-owned [`AgentCheckpoint`].
///
/// Holds only the header, a running digest, and a count — the state
/// itself lives in the target the caller owns, so restoring needs no
/// second copy of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Restore {
    header: Header,
    digest: u64,
    received: usize,
}

impl Restore {
    /// Begin restoring `header` into `target`, which is reset to the
    /// header's counters and empty, allocator-positioned stores.
    pub const fn begin(header: Header, target: &mut AgentCheckpoint) -> Self {
        *target = AgentCheckpoint {
            last_beat: header.last_beat,
            spawn_seq: header.spawn_seq,
            next_deadline: header.next_deadline,
            ledger: GoalLedger::restoring(header.next_goal_id),
            memory: Memory::restoring(header.next_stamp),
        };
        Self {
            header,
            digest: FNV_OFFSET,
            received: 0,
        }
    }

    /// Items received so far.
    #[must_use]
    pub const fn received(&self) -> usize {
        self.received
    }

    /// Items still owed.
    #[must_use]
    pub const fn remaining(&self) -> usize {
        self.header.items().saturating_sub(self.received)
    }

    /// Apply the next item. Goals must come before entries, exactly as
    /// many of each as the header promised.
    ///
    /// # Errors
    ///
    /// [`Error::SnapshotCorrupt`] for an item past the promised count,
    /// the wrong kind in this position, trailing bytes, or any
    /// structural violation; [`Error::SnapshotTruncated`] for a short
    /// item.
    pub fn push(&mut self, target: &mut AgentCheckpoint, item: &[u8]) -> Result<(), Error> {
        let mut r = Reader {
            bytes: item,
            pos: 0,
            end: item.len(),
        };
        self.take_into(target, &mut r)?;
        if r.pos != r.end {
            return Err(Error::SnapshotCorrupt);
        }
        Ok(())
    }

    /// Read one item from `r` into `target`, folding its bytes into the
    /// running digest.
    fn take_into(&mut self, target: &mut AgentCheckpoint, r: &mut Reader<'_>) -> Result<(), Error> {
        if self.remaining() == 0 {
            return Err(Error::SnapshotCorrupt);
        }
        let start = r.pos;
        let want_goal = self.received < usize::from(self.header.goals);
        match (r.u8()?, want_goal) {
            (TAG_GOAL, true) => target.ledger.restore_goal(take_goal(r)?)?,
            (TAG_ENTRY, false) => target.memory.restore_entry(take_entry(r)?)?,
            _ => return Err(Error::SnapshotCorrupt),
        }
        let bytes = r.bytes.get(start..r.pos).ok_or(Error::SnapshotTruncated)?;
        self.digest = fnv_update(self.digest, bytes);
        self.received += 1;
        Ok(())
    }

    /// Finish: every promised item arrived and the digest matches.
    ///
    /// # Errors
    ///
    /// [`Error::SnapshotTruncated`] when items are still owed;
    /// [`Error::SnapshotChecksumMismatch`] when the items do not match
    /// the header's digest.
    pub const fn finish(&self) -> Result<(), Error> {
        if self.remaining() != 0 {
            return Err(Error::SnapshotTruncated);
        }
        if self.digest != self.header.digest {
            return Err(Error::SnapshotChecksumMismatch);
        }
        Ok(())
    }
}

/// The largest encoded memory entry ([`encode_entry`]): the harvest
/// activity's answer on the device.
pub const MAX_ENTRY_RECORD_BYTES: usize = MAX_ENTRY_BYTES;

/// Encode one memory entry (the item layout without its tag),
/// returning the bytes written.
///
/// # Errors
///
/// [`Error::SnapshotBufferTooSmall`] when `out` is short.
pub fn encode_entry(entry: &MemoryEntry, out: &mut [u8]) -> Result<usize, Error> {
    let mut sink = Sink::writing(out);
    put_entry(&mut sink, entry)?;
    Ok(sink.pos)
}

/// Decode exactly one memory entry written by [`encode_entry`].
///
/// # Errors
///
/// [`Error::SnapshotTruncated`] for short input; [`Error::SnapshotCorrupt`]
/// for trailing bytes or an invalid field.
pub fn decode_entry(bytes: &[u8]) -> Result<MemoryEntry, Error> {
    let mut r = Reader {
        bytes,
        pos: 0,
        end: bytes.len(),
    };
    let entry = take_entry(&mut r)?;
    if r.pos != r.end {
        return Err(Error::SnapshotCorrupt);
    }
    Ok(entry)
}

/// Exact bytes [`encode`] writes for `state`.
#[must_use]
pub fn encoded_len(state: &AgentCheckpoint) -> usize {
    let mut sink = Sink::counting();
    // Counting never fails: every write only advances the cursor.
    let _ = put_blob(&mut sink, state);
    sink.pos + CHECKSUM_LEN
}

/// Encode `state` as one blob (header, items, integrity), returning the
/// bytes written. Host tooling; the device stores items.
///
/// # Errors
///
/// [`Error::SnapshotBufferTooSmall`] when `out` is shorter than
/// [`encoded_len`].
pub fn encode(state: &AgentCheckpoint, out: &mut [u8]) -> Result<usize, Error> {
    let mut sink = Sink::writing(out);
    put_blob(&mut sink, state)?;
    let end = sink.pos;
    let integrity = fnv1a64(sink.out.get(..end).ok_or(Error::SnapshotBufferTooSmall)?);
    sink.put_u64(integrity)?;
    Ok(sink.pos)
}

fn put_blob(s: &mut Sink<'_>, state: &AgentCheckpoint) -> Result<(), Error> {
    state.header().put(s)?;
    for index in 0..state.item_count() {
        state.put_item(s, index)?;
    }
    Ok(())
}

/// Decode and re-validate a blob.
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
    let header = Header::take(&mut r)?;
    let mut state = AgentCheckpoint::new();
    let mut restore = Restore::begin(header, &mut state);
    while restore.remaining() > 0 {
        restore.take_into(&mut state, &mut r)?;
    }
    if r.pos != r.end {
        return Err(Error::SnapshotCorrupt);
    }
    // The blob integrity already passed, so a digest mismatch here is a
    // payload that checksums but lies about its items: corrupt.
    restore.finish().map_err(|_| Error::SnapshotCorrupt)?;
    Ok(state)
}

/// Check a blob's length, version, and integrity without decoding it.
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
