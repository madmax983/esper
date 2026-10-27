//! Memory: bounded, masked, provenance-tagged knowledge that survives
//! across runs (SPEC §21.3).
//!
//! The always-on supervisor remembers what its runs saw and what the
//! operator told it, and hands the freshest of it to each new run's
//! prompt ([`recall`] → [`crate::brief::Brief`]).
//!
//! Three rules shape the store:
//!
//! * **Mask before durability.** [`ingest`] masks the *full* input
//!   ([`crate::mask::mask_report`]) before it truncates to
//!   [`NOTE_MAX`]: truncating first could cut a secret below its
//!   shape threshold and leak its prefix.
//! * **The model cannot write memory.** Entries come from committed
//!   run evidence (harvested by the runtime) or from the operator —
//!   never from model prose (§8.1).
//! * **Pinned entries are never evicted.** Operator notes are pinned
//!   and capped at [`MAX_PINNED`]; harvested entries evict
//!   oldest-first. Observations can be re-observed; an operator's
//!   instruction cannot be re-invented (the §20.2 discipline).

use crate::beat::Beat;
use crate::compact::{NOTE_MAX, Note};
use crate::error::Error;
use crate::goal::GoalId;
use crate::ids::{Digest, RunId, ToolId};
use crate::mask::{mask_bound, mask_report};

/// Maximum entries the store holds.
pub const MAX_MEMORY: usize = 32;
/// Maximum pinned (operator) entries; the rest of the store is always
/// available to harvest.
pub const MAX_PINNED: usize = 8;
/// Maximum raw input bytes for one label, value, or objective. Longer
/// inputs fail closed: masking needs the whole input in one window.
pub const MEMORY_INPUT_MAX: usize = 512;
/// Maximum entries one recall returns.
pub const RECALL_K: usize = 4;

/// The masking scratch size for one [`ingest`] call.
const INGEST_SCRATCH: usize = mask_bound(MEMORY_INPUT_MAX);

/// Mask, bound, and sanitize operator or harvest text into a [`Note`]
/// (SPEC §21.3 rule 1).
///
/// Order is fixed: mask the full input, truncate to [`NOTE_MAX`], then
/// replace control bytes (`< 0x20`, `0x7f`) with spaces so a value can
/// never forge a prompt line.
///
/// # Errors
///
/// [`Error::MemoryInputTooLong`] when `raw` exceeds
/// [`MEMORY_INPUT_MAX`] bytes.
pub fn ingest(raw: &[u8]) -> Result<Note, Error> {
    if raw.len() > MEMORY_INPUT_MAX {
        return Err(Error::MemoryInputTooLong);
    }
    let mut scratch = [0u8; INGEST_SCRATCH];
    let report = mask_report(raw, &mut scratch)?;
    let masked = scratch.get(..report.output_len).unwrap_or(&[]);
    let mut bounded = [0u8; NOTE_MAX];
    let take = masked.len().min(NOTE_MAX);
    for (dst, src) in bounded.iter_mut().zip(masked.iter()) {
        *dst = if *src < 0x20 || *src == 0x7f {
            b' '
        } else {
            *src
        };
    }
    Note::from_bytes(bounded.get(..take).unwrap_or(&[]))
}

/// The exact identity of a memory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryKey {
    /// What a tool call with these exact arguments last returned.
    /// Harvested.
    Observation {
        /// The tool.
        tool: ToolId,
        /// FNV-1a-64 over the argument bytes as emitted.
        args_digest: Digest,
    },
    /// How a goal's last run ended. Harvested.
    Outcome {
        /// The goal.
        goal: GoalId,
    },
    /// An operator note, keyed by its name's digest. Pinned.
    Note {
        /// FNV-1a-64 over the note's name.
        name_digest: Digest,
    },
}

impl MemoryKey {
    /// Whether entries under this key are pinned (never evicted).
    #[must_use]
    pub const fn is_pinned(&self) -> bool {
        matches!(self, Self::Note { .. })
    }

    /// The stable tag byte (checkpoint encoding).
    #[must_use]
    pub const fn tag(&self) -> u8 {
        match self {
            Self::Observation { .. } => 0,
            Self::Outcome { .. } => 1,
            Self::Note { .. } => 2,
        }
    }

    /// The key for an operator note named `name`.
    #[must_use]
    pub fn note(name: &[u8]) -> Self {
        Self::Note {
            name_digest: Digest::of_bytes(name),
        }
    }
}

/// Where an entry came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Provenance {
    /// The goal whose run produced it (`None` for operator notes).
    pub goal: Option<GoalId>,
    /// The run that produced it (`None` for operator notes).
    pub run: Option<RunId>,
    /// The beat it was written at.
    pub beat: Beat,
}

/// One remembered item.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MemoryEntry {
    /// The exact identity.
    pub key: MemoryKey,
    /// The masked, bounded display label.
    pub label: Note,
    /// The masked, bounded value.
    pub value: Note,
    /// Where it came from.
    pub provenance: Provenance,
    /// The write stamp: monotonic per store, assigned by
    /// [`Memory::write`]. Recency and eviction order.
    pub stamp: u32,
}

impl MemoryEntry {
    /// Build an entry from raw label and value bytes, masking both
    /// ([`ingest`]). The stamp is assigned on write.
    ///
    /// # Errors
    ///
    /// [`Error::MemoryInputTooLong`] when either input is overlong.
    pub fn new(
        key: MemoryKey,
        label: &[u8],
        value: &[u8],
        provenance: Provenance,
    ) -> Result<Self, Error> {
        Ok(Self {
            key,
            label: ingest(label)?,
            value: ingest(value)?,
            provenance,
            stamp: 0,
        })
    }

    /// Whether the entry is pinned.
    #[must_use]
    pub const fn pinned(&self) -> bool {
        self.key.is_pinned()
    }
}

/// The memory store: at most [`MAX_MEMORY`] entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Memory {
    slots: [Option<MemoryEntry>; MAX_MEMORY],
    next_stamp: u32,
}

impl Default for Memory {
    fn default() -> Self {
        Self::new()
    }
}

impl Memory {
    /// An empty store.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            slots: [None; MAX_MEMORY],
            next_stamp: 1,
        }
    }

    /// The stamp the next write will get.
    #[must_use]
    pub const fn next_stamp(&self) -> u32 {
        self.next_stamp
    }

    /// The entries, in slot order.
    pub fn entries(&self) -> impl Iterator<Item = &MemoryEntry> {
        self.slots.iter().flatten()
    }

    /// The number of entries held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries().count()
    }

    /// Whether the store is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The number of pinned entries held.
    #[must_use]
    pub fn pinned_count(&self) -> usize {
        self.entries().filter(|entry| entry.pinned()).count()
    }

    /// The entry under `key`.
    #[must_use]
    pub fn get(&self, key: &MemoryKey) -> Option<&MemoryEntry> {
        self.entries().find(|entry| entry.key == *key)
    }

    /// Write `entry` (SPEC §21.3): supersede the same key in place, else
    /// take a free slot, else evict the oldest unpinned entry. Assigns
    /// the next stamp.
    ///
    /// # Errors
    ///
    /// [`Error::MemoryFull`] when a new pinned entry would exceed
    /// [`MAX_PINNED`], or no unpinned entry can be evicted;
    /// [`Error::MemoryStampsExhausted`] when the stamp space is spent.
    pub fn write(&mut self, mut entry: MemoryEntry) -> Result<(), Error> {
        let stamp = self.next_stamp;
        let next = stamp.checked_add(1).ok_or(Error::MemoryStampsExhausted)?;
        entry.stamp = stamp;
        if let Some(existing) = self
            .slots
            .iter_mut()
            .flatten()
            .find(|existing| existing.key == entry.key)
        {
            *existing = entry;
            self.next_stamp = next;
            return Ok(());
        }
        if entry.pinned() && self.pinned_count() >= MAX_PINNED {
            return Err(Error::MemoryFull);
        }
        let index = if let Some(free) = self.slots.iter().position(Option::is_none) {
            free
        } else {
            self.slots
                .iter()
                .enumerate()
                .filter_map(|(i, slot)| slot.map(|e| (i, e)))
                .filter(|(_, e)| !e.pinned())
                .min_by_key(|(_, e)| e.stamp)
                .map(|(i, _)| i)
                .ok_or(Error::MemoryFull)?
        };
        let slot = self.slots.get_mut(index).ok_or(Error::MemoryFull)?;
        *slot = Some(entry);
        self.next_stamp = next;
        Ok(())
    }

    /// Remove the entry under `key`. Returns whether one existed.
    ///
    /// The survivors keep their order and move to the front: storage
    /// stays canonical (no holes), so a checkpoint decodes to exactly
    /// the store it encodes and later writes land in the same slots
    /// whether or not the agent checkpointed.
    pub fn forget(&mut self, key: &MemoryKey) -> bool {
        if self.get(key).is_none() {
            return false;
        }
        let mut kept = [None; MAX_MEMORY];
        for (dst, entry) in kept
            .iter_mut()
            .zip(self.entries().filter(|entry| entry.key != *key))
        {
            *dst = Some(*entry);
        }
        self.slots = kept;
        true
    }

    /// Rebuild a store from decoded parts (checkpoint restore),
    /// validating every structural invariant.
    ///
    /// # Errors
    ///
    /// [`Error::SnapshotCorrupt`] for too many entries, duplicate keys
    /// or stamps, a stamp at or past `next_stamp`, or too many pinned
    /// entries.
    pub fn from_parts(entries: &[MemoryEntry], next_stamp: u32) -> Result<Self, Error> {
        if entries.len() > MAX_MEMORY {
            return Err(Error::SnapshotCorrupt);
        }
        let mut memory = Self {
            slots: [None; MAX_MEMORY],
            next_stamp,
        };
        for (i, entry) in entries.iter().enumerate() {
            if entry.stamp == 0 || entry.stamp >= next_stamp {
                return Err(Error::SnapshotCorrupt);
            }
            if entries
                .iter()
                .skip(i + 1)
                .any(|other| other.key == entry.key || other.stamp == entry.stamp)
            {
                return Err(Error::SnapshotCorrupt);
            }
        }
        for (slot, entry) in memory.slots.iter_mut().zip(entries.iter()) {
            *slot = Some(*entry);
        }
        if memory.pinned_count() > MAX_PINNED {
            return Err(Error::SnapshotCorrupt);
        }
        Ok(memory)
    }
}

/// The entries recalled for one goal, best first.
#[derive(Debug, Clone, Copy)]
pub struct Recall<'a> {
    items: [Option<&'a MemoryEntry>; RECALL_K],
}

impl<'a> Recall<'a> {
    /// The recalled entries, in order.
    pub fn entries(&self) -> impl Iterator<Item = &'a MemoryEntry> + '_ {
        self.items.iter().flatten().copied()
    }

    /// The number of recalled entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries().count()
    }

    /// Whether nothing was recalled.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Recall up to [`RECALL_K`] entries for `goal` (SPEC §21.3).
///
/// Order: the goal's own outcome; pinned notes, newest first;
/// observations from the goal's runs, newest first; other
/// observations, newest first. Recency, not relevance.
#[must_use]
pub fn recall(memory: &Memory, goal: GoalId) -> Recall<'_> {
    let mut items: [Option<&MemoryEntry>; RECALL_K] = [None; RECALL_K];
    let mut len = 0;
    // Each tier is a filter; within a tier, newest stamp first.
    let tiers: [&dyn Fn(&MemoryEntry) -> bool; 4] = [
        &|e| e.key == MemoryKey::Outcome { goal },
        &MemoryEntry::pinned,
        &|e| matches!(e.key, MemoryKey::Observation { .. }) && e.provenance.goal == Some(goal),
        &|e| matches!(e.key, MemoryKey::Observation { .. }) && e.provenance.goal != Some(goal),
    ];
    for tier in tiers {
        // Selection by descending stamp, without allocation: repeatedly
        // take the newest entry below the previous one's stamp.
        let mut below = u32::MAX;
        while len < RECALL_K {
            let next = memory
                .entries()
                .filter(|e| tier(e) && e.stamp < below)
                .max_by_key(|e| e.stamp);
            let Some(entry) = next else { break };
            if let Some(slot) = items.get_mut(len) {
                *slot = Some(entry);
            }
            len += 1;
            below = entry.stamp;
        }
    }
    Recall { items }
}
