//! Harvest: what a settled run teaches the agent's memory (SPEC §21.3).
//!
//! A [`Harvester`] is fed a run's committed events in journal order —
//! tool requests, tool observations, verifications — and then its
//! terminal status. It yields at most [`MAX_HARVEST`] memory entries:
//! the latest successful observation per `(tool, args)` key, then one
//! `Outcome` entry for the goal.
//!
//! Streaming and allocation-free. Esper dispatches one call per turn,
//! so at most one effect is open at a time: pairing a request with its
//! observation and verification needs one slot, not a table. The run
//! engine can feed it straight from its journal on the device.
//!
//! The rules (SPEC §21.3):
//!
//! * **Committed evidence only.** Nothing here reads model prose
//!   except the terminal summary, which is masked like everything else.
//! * **Never remember an unverified mutation (§9.3).** A tool whose
//!   permission class is not read-only — or a tool the catalog does
//!   not know, failing closed — is remembered only when a passing
//!   verification committed for the same effect.
//! * **Mask before durability.** Labels and values go through
//!   [`crate::memory::ingest`]; inputs over `MEMORY_INPUT_MAX` are
//!   skipped, never truncated unmasked.

use crate::compact::Note;
use crate::error::Error;
use crate::goal::GoalId;
use crate::ids::{Digest, ToolId};
use crate::memory::{MEMORY_INPUT_MAX, MemoryEntry, MemoryKey, Provenance, ingest};
use crate::registry::lookup_by_id;
use crate::state::TerminalStatus;
use esper_protocol::PermissionClass;

/// Maximum entries one settled run yields: observations plus the one
/// `Outcome` entry. Each is one Waymaker record on the device.
pub const MAX_HARVEST: usize = 6;

/// Observation entries kept (the rest of [`MAX_HARVEST`] is the outcome).
const MAX_OBSERVATIONS: usize = MAX_HARVEST - 1;

/// The one open effect: requested, maybe observed, maybe verified.
#[derive(Debug, Clone, Copy)]
struct Open {
    seq: u32,
    key: MemoryKey,
    /// `None` when the label could not be ingested (overlong).
    label: Option<Note>,
    mutating: bool,
    /// The latest successful observation, ingested.
    value: Option<Note>,
    verified: bool,
}

/// Streams a run's committed events into memory entries.
#[derive(Debug, Clone, Copy)]
pub struct Harvester {
    provenance: Provenance,
    open: Option<Open>,
    kept: [Option<MemoryEntry>; MAX_OBSERVATIONS],
}

impl Harvester {
    /// A harvester for `provenance`'s run.
    #[must_use]
    pub const fn new(provenance: Provenance) -> Self {
        Self {
            provenance,
            open: None,
            kept: [None; MAX_OBSERVATIONS],
        }
    }

    /// Resume a harvester from entries an earlier segment of the same
    /// run kept ([`Harvester::kept`] after [`Harvester::seal`]), oldest
    /// first. A run continued as new carries its harvest this way.
    ///
    /// # Errors
    ///
    /// [`Error::MemoryFull`] for more entries than a harvester keeps.
    pub fn resume(provenance: Provenance, entries: &[MemoryEntry]) -> Result<Self, Error> {
        let mut harvester = Self::new(provenance);
        if entries.len() > MAX_OBSERVATIONS {
            return Err(Error::MemoryFull);
        }
        for (slot, entry) in harvester.kept.iter_mut().zip(entries) {
            *slot = Some(*entry);
        }
        Ok(harvester)
    }

    /// Close the open effect now. Its outcome is fully committed (a run
    /// seals before it continues as new), so the result is the same as
    /// closing it at the next request.
    pub fn seal(&mut self) {
        self.close();
    }

    /// The entries kept so far, oldest first.
    pub fn kept(&self) -> impl Iterator<Item = &MemoryEntry> {
        self.kept.iter().flatten()
    }

    /// A tool intent committed: close the previous effect, open this one.
    ///
    /// The label is `<tool_name> <args>`, masked.
    pub fn request(&mut self, seq: u32, tool: ToolId, args: &[u8]) {
        self.close();
        let contract = lookup_by_id(tool);
        // Unknown tools need a verification: fail closed.
        let mutating = contract.is_none_or(|c| c.permission != PermissionClass::ReadOnly);
        let name = contract.map_or("tool", |c| c.name).as_bytes();
        let mut raw = [0u8; MEMORY_INPUT_MAX];
        let label = (name.len() + 1 + args.len() <= MEMORY_INPUT_MAX)
            .then(|| {
                let (head, rest) = raw.split_at_mut(name.len());
                head.copy_from_slice(name);
                let (space, tail) = rest.split_at_mut(1);
                space.copy_from_slice(b" ");
                tail.get_mut(..args.len())?.copy_from_slice(args);
                ingest(raw.get(..name.len() + 1 + args.len())?).ok()
            })
            .flatten();
        self.open = Some(Open {
            seq,
            key: MemoryKey::Observation {
                tool,
                args_digest: Digest::of_bytes(args),
            },
            label,
            mutating,
            value: None,
            verified: false,
        });
    }

    /// A tool outcome committed. Transient failures are ignored: a later
    /// attempt under the same effect may succeed.
    pub fn observation(&mut self, seq: u32, ok: bool, payload: &[u8]) {
        if let Some(open) = self.open.as_mut().filter(|open| open.seq == seq && ok) {
            open.value = ingest(payload).ok();
        }
    }

    /// An independent read-back committed for `seq`.
    pub fn verification(&mut self, seq: u32, passed: bool) {
        if let Some(open) = self.open.as_mut().filter(|open| open.seq == seq) {
            open.verified = passed;
        }
    }

    /// Keep the open effect if it earned a memory entry.
    fn close(&mut self) {
        let Some(open) = self.open.take() else { return };
        let (Some(label), Some(value)) = (open.label, open.value) else {
            return;
        };
        if open.mutating && !open.verified {
            // SPEC §21.3 rule 3: never remember an unverified mutation.
            return;
        }
        let entry = MemoryEntry {
            key: open.key,
            label,
            value,
            provenance: self.provenance,
            stamp: 0,
        };
        // Supersede the same key (drop the old one), make room by
        // dropping the oldest when full, then append: storage stays in
        // arrival order, oldest first, `None`s only at the tail.
        if let Some(index) = self
            .kept
            .iter()
            .position(|slot| slot.is_some_and(|e| e.key == entry.key))
        {
            self.remove(index);
        }
        if self.kept.iter().all(Option::is_some) {
            self.remove(0);
        }
        if let Some(slot) = self.kept.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(entry);
        }
    }

    /// Remove slot `index`, shifting later entries left.
    fn remove(&mut self, index: usize) {
        if let Some(tail) = self.kept.get_mut(index..) {
            tail.rotate_left(1);
            if let Some(last) = tail.last_mut() {
                *last = None;
            }
        }
    }

    /// The run ended with `status` and `summary`: close the open effect
    /// and add the goal's `Outcome` entry.
    ///
    /// # Errors
    ///
    /// Never for well-formed input; [`Error::MemoryInputTooLong`] cannot
    /// escape (an overlong summary falls back to the status name).
    pub fn finish(
        mut self,
        goal: GoalId,
        status: TerminalStatus,
        summary: &[u8],
    ) -> Result<HarvestSet, Error> {
        self.close();
        let mut set = HarvestSet {
            entries: [None; MAX_HARVEST],
        };
        for (dst, src) in set.entries.iter_mut().zip(self.kept.iter().flatten()) {
            *dst = Some(*src);
        }
        let mut label_buf = [0u8; 10];
        let label = goal_label(goal, &mut label_buf);
        let name = status.name().as_bytes();
        let mut raw = [0u8; MEMORY_INPUT_MAX];
        let value_len = if summary.is_empty() || name.len() + 1 + summary.len() > MEMORY_INPUT_MAX {
            name.len()
        } else {
            name.len() + 1 + summary.len()
        };
        if let Some(dst) = raw.get_mut(..name.len()) {
            dst.copy_from_slice(name);
        }
        if value_len > name.len() {
            if let Some(space) = raw.get_mut(name.len()) {
                *space = b' ';
            }
            if let Some(dst) = raw.get_mut(name.len() + 1..value_len) {
                dst.copy_from_slice(summary);
            }
        }
        let outcome = MemoryEntry::new(
            MemoryKey::Outcome { goal },
            label,
            raw.get(..value_len).unwrap_or(name),
            self.provenance,
        )?;
        let slot = set
            .entries
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(Error::MemoryFull)?;
        *slot = Some(outcome);
        Ok(set)
    }
}

/// What a settled run yields: observations (oldest first), then the
/// goal's outcome. At most [`MAX_HARVEST`] entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HarvestSet {
    entries: [Option<MemoryEntry>; MAX_HARVEST],
}

impl HarvestSet {
    /// The entries, in write order.
    pub fn entries(&self) -> impl Iterator<Item = &MemoryEntry> {
        self.entries.iter().flatten()
    }

    /// The number of entries.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries().count()
    }

    /// Whether the set is empty (never, once finished: the outcome is
    /// always there).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Entry `index`, in write order.
    #[must_use]
    pub fn get(&self, index: usize) -> Option<&MemoryEntry> {
        self.entries().nth(index)
    }
}

/// Render `goal#<id>` into `buf` (at most 10 bytes) without allocation.
fn goal_label(goal: GoalId, buf: &mut [u8; 10]) -> &[u8] {
    let mut digits = [0u8; 5];
    let mut id = goal.get();
    let mut n = 0;
    loop {
        if let Some(d) = digits.get_mut(n) {
            *d = b'0' + u8::try_from(id % 10).unwrap_or(0);
        }
        n += 1;
        id /= 10;
        if id == 0 {
            break;
        }
    }
    if let Some(prefix) = buf.get_mut(..5) {
        prefix.copy_from_slice(b"goal#");
    }
    for (dst, src) in buf.iter_mut().skip(5).zip(digits.iter().take(n).rev()) {
        *dst = *src;
    }
    buf.get(..5 + n).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::beat::Beat;

    /// A harvest sealed, carried as its kept entries and resumed, ends
    /// exactly like one harvester that saw every event.
    #[test]
    fn a_sealed_and_resumed_harvest_matches_an_unbroken_one() {
        let provenance = Provenance {
            goal: None,
            run: None,
            beat: Beat::ZERO,
        };
        let read = |h: &mut Harvester, seq: u32, pin: &[u8], value: &[u8]| {
            h.request(seq, ToolId::new(1), pin);
            h.observation(seq, true, value);
        };
        let mut whole = Harvester::new(provenance);
        read(
            &mut whole,
            1,
            br#"{"pin": 3}"#,
            br#"{"pin":3,"level":"low"}"#,
        );
        read(
            &mut whole,
            2,
            br#"{"pin": 4}"#,
            br#"{"pin":4,"level":"high"}"#,
        );
        read(
            &mut whole,
            3,
            br#"{"pin": 3}"#,
            br#"{"pin":3,"level":"high"}"#,
        );

        let mut first = Harvester::new(provenance);
        read(
            &mut first,
            1,
            br#"{"pin": 3}"#,
            br#"{"pin":3,"level":"low"}"#,
        );
        read(
            &mut first,
            2,
            br#"{"pin": 4}"#,
            br#"{"pin":4,"level":"high"}"#,
        );
        first.seal();
        let mut carried = [first.kept[0].unwrap(); 2];
        for (dst, src) in carried.iter_mut().zip(first.kept()) {
            *dst = *src;
        }
        assert_eq!(first.kept().count(), 2);
        let mut second = Harvester::resume(provenance, &carried).unwrap();
        read(
            &mut second,
            1,
            br#"{"pin": 3}"#,
            br#"{"pin":3,"level":"high"}"#,
        );

        let goal = GoalId::new(1).unwrap();
        let whole = whole
            .finish(goal, TerminalStatus::Completed, b"done")
            .unwrap();
        let resumed = second
            .finish(goal, TerminalStatus::Completed, b"done")
            .unwrap();
        assert_eq!(whole, resumed);
    }

    #[test]
    fn resume_refuses_more_than_it_keeps() {
        let provenance = Provenance {
            goal: None,
            run: None,
            beat: Beat::ZERO,
        };
        let entry = MemoryEntry::new(
            MemoryKey::Outcome {
                goal: GoalId::new(1).unwrap(),
            },
            b"l",
            b"v",
            provenance,
        )
        .unwrap();
        assert!(Harvester::resume(provenance, &[entry; MAX_OBSERVATIONS]).is_ok());
        assert_eq!(
            Harvester::resume(provenance, &[entry; MAX_OBSERVATIONS + 1]).unwrap_err(),
            Error::MemoryFull
        );
    }
}
