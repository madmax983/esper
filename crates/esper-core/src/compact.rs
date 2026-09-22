//! Bounded context compaction (SPEC §20.2).
//!
//! When the runtime's context pressure reaches the policy trigger,
//! [`compact`] folds a slice of [`FrameSummary`] views into the
//! run's [`CompactState`]: the durable compact state from design §8.
//! The runtime keeps the last [`CompactionPolicy::tail_keep`] frames
//! verbatim in the prompt; everything the new run (or a rebooted one)
//! must not lose lives in the state.
//!
//! Two preservation invariants hold by construction:
//!
//! * (a) **Failed paths are never dropped.** Every `(tool,
//!   args_digest, error)` failure tuple from the folded frames is
//!   recorded in [`CompactState::failed_paths`]; the store is
//!   append-with-dedupe and [`compact`] fails closed with
//!   [`Error::CompactStateFull`] rather than dropping one, so the
//!   resumed agent can never retry a ruled-out path.
//! * (b) **Open obligations are never dropped.** Pending approvals
//!   and verifications land in [`CompactState::pending`] under the
//!   same fail-closed store discipline, and leave only through
//!   [`CompactState::resolve_pending`].
//!
//! Positive knowledge (facts, completed subgoals, accepted decisions)
//! is bounded institutional memory: when its lists fill, the oldest
//! entry is dropped. That is deliberate — (a) and (b) are the
//! must-preserve sets; facts can be re-observed, obligations cannot
//! be re-invented.
//!
//! [`compact`] assumes frame payloads are already masked (see
//! [`crate::mask`]): masking is the runtime's job at ingestion,
//! before durability (design §9).

use crate::budget::ResourceBudget;
use crate::error::{Error, ErrorCode};
use crate::ids::{Digest, ToolId};
use crate::mask::fnv1a64;
use crate::monitor::{ProgressDelta, StepOutcome};
use crate::records::RecordKind;

/// Maximum completed-subgoal notes retained.
pub const MAX_COMPLETED: usize = 8;
/// Maximum open-subgoal notes retained.
pub const MAX_OPEN: usize = 8;
/// Maximum accepted-decision digests retained.
pub const MAX_ACCEPTED_DECISIONS: usize = 8;
/// Maximum facts retained.
pub const MAX_FACTS: usize = 16;
/// Maximum failed-path tuples retained (never dropped while recording).
pub const MAX_FAILED_PATHS: usize = 16;
/// Maximum open obligations retained (never dropped while recording).
pub const MAX_PENDING: usize = 8;
/// Maximum recent event fingerprints retained (loop detection window).
pub const MAX_FINGERPRINTS: usize = 16;
/// Maximum bytes of one note / fact / subgoal text.
pub const NOTE_MAX: usize = 64;

/// When and how much to compact.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CompactionPolicy {
    /// Compact when context bytes reach this percent of the run's
    /// context budget (0–100; values above 100 are treated as 100).
    pub trigger_pct: u8,
    /// Recent frames the runtime keeps verbatim in the prompt.
    pub tail_keep: u8,
}

/// The design §8 defaults: compact at 80% of context budget, keep the
/// last 5 tool interactions verbatim (evaluation hypothesis, not a
/// protocol constant).
pub const DEFAULT_POLICY: CompactionPolicy = CompactionPolicy {
    trigger_pct: 80,
    tail_keep: 5,
};

/// Whether `context_bytes` has reached the policy's trigger percent of
/// `context_budget_bytes`.
#[must_use]
pub const fn should_compact(
    context_bytes: u64,
    context_budget_bytes: u64,
    policy: &CompactionPolicy,
) -> bool {
    let pct = if policy.trigger_pct > 100 {
        100
    } else {
        policy.trigger_pct
    };
    (context_bytes as u128) * 100 >= (context_budget_bytes as u128) * (pct as u128)
}

/// Which kind of open obligation a [`PendingItem`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PendingKind {
    /// Awaiting human input on an `Ask`.
    Approval,
    /// Awaiting the independent post-mutation read-back.
    Verification,
}

impl PendingKind {
    /// The stable numeric code.
    #[must_use]
    pub const fn code(self) -> u8 {
        match self {
            Self::Approval => 0,
            Self::Verification => 1,
        }
    }

    /// Decode a numeric code, failing closed on unknown values.
    #[must_use]
    pub const fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(Self::Approval),
            1 => Some(Self::Verification),
            _ => None,
        }
    }
}

/// Bounded text: a subgoal, fact, or decision note (SPEC §20.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Note {
    bytes: [u8; NOTE_MAX],
    len: u8,
}

impl Note {
    /// The empty note, for fixed-array initialization.
    pub const EMPTY: Self = Self {
        bytes: [0; NOTE_MAX],
        len: 0,
    };

    /// Build a note, failing closed when `bytes` exceeds [`NOTE_MAX`].
    ///
    /// # Errors
    ///
    /// Returns [`Error::CompactFieldTooLong`] when `bytes` is longer
    /// than [`NOTE_MAX`].
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, Error> {
        if bytes.len() > NOTE_MAX {
            return Err(Error::CompactFieldTooLong);
        }
        let len = u8::try_from(bytes.len()).map_err(|_| Error::CompactFieldTooLong)?;
        let mut note = Self::EMPTY;
        for (dst, src) in note.bytes.iter_mut().zip(bytes.iter()) {
            *dst = *src;
        }
        note.len = len;
        Ok(note)
    }

    /// Build a note, truncating to [`NOTE_MAX`] bytes. The compaction
    /// path uses this so a long observation can never fail compaction;
    /// callers who need strictness use [`Self::from_bytes`].
    #[must_use]
    pub fn truncated_from(bytes: &[u8]) -> Self {
        let mut note = Self::EMPTY;
        let take = bytes.len().min(NOTE_MAX);
        if let Some(src) = bytes.get(..take) {
            for (dst, b) in note.bytes.iter_mut().zip(src.iter()) {
                *dst = *b;
            }
            if let Ok(len) = u8::try_from(take) {
                note.len = len;
            }
        }
        note
    }

    /// The note bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        self.bytes.get(..usize::from(self.len)).unwrap_or(&[])
    }
}

/// A retained fact with its source frame (design §8: "relevant facts
/// with source IDs").
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fact {
    /// The fact text (bounded).
    pub text: Note,
    /// The frame sequence number the fact was observed in.
    pub source_seq: u32,
}

/// A ruled-out path: tool, argument digest, and error class (the
/// negative information design §8 makes first-class).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FailedPath {
    /// The tool that failed.
    pub tool: ToolId,
    /// The digest of the arguments it failed with.
    pub args_digest: Digest,
    /// The §6 error class it failed with.
    pub error: ErrorCode,
}

/// An open obligation: a pending approval or verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PendingItem {
    /// Which kind of obligation this is.
    pub kind: PendingKind,
    /// The frame sequence number that opened it.
    pub seq: u32,
    /// Correlation digest (FNV-1a-64 over the redacted prompt or
    /// expected-state bytes that opened it).
    pub digest: Digest,
}

/// The version bindings a continuation carries (design §8).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct VersionSet {
    /// Workflow version.
    pub workflow: u32,
    /// Model bundle hash.
    pub model: Digest,
    /// Tool catalog hash.
    pub catalog: Digest,
    /// Policy hash.
    pub policy: Digest,
}

/// The durable compact state (design §8, SPEC §20.2).
///
/// Bounded and fixed-capacity: no heap, safe to snapshot. Scalar
/// fields are public; the lists are append-managed through the
/// `record_*` methods so the never-drop invariants for failed paths
/// and obligations hold by construction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactState {
    /// Digest of the objective and immutable constraints.
    pub objective_digest: Digest,
    /// Remaining resource budgets (part of run identity, ADR 10).
    pub budgets: ResourceBudget,
    /// Catalog, policy, workflow, and model versions.
    pub versions: VersionSet,
    completed: [Note; MAX_COMPLETED],
    completed_len: u8,
    open: [Note; MAX_OPEN],
    open_len: u8,
    accepted: [Digest; MAX_ACCEPTED_DECISIONS],
    accepted_len: u8,
    facts: [Fact; MAX_FACTS],
    facts_len: u8,
    failed: [FailedPath; MAX_FAILED_PATHS],
    failed_len: u8,
    pending: [PendingItem; MAX_PENDING],
    pending_len: u8,
    fingerprints: [u64; MAX_FINGERPRINTS],
    fingerprints_len: u8,
}

impl CompactState {
    /// An empty compact state for a run with this objective, budget,
    /// and version set.
    #[must_use]
    pub const fn new(
        objective_digest: Digest,
        budgets: ResourceBudget,
        versions: VersionSet,
    ) -> Self {
        Self {
            objective_digest,
            budgets,
            versions,
            completed: [Note::EMPTY; MAX_COMPLETED],
            completed_len: 0,
            open: [Note::EMPTY; MAX_OPEN],
            open_len: 0,
            accepted: [Digest::new(0); MAX_ACCEPTED_DECISIONS],
            accepted_len: 0,
            facts: [Fact {
                text: Note::EMPTY,
                source_seq: 0,
            }; MAX_FACTS],
            facts_len: 0,
            failed: [FailedPath {
                tool: ToolId::new(0),
                args_digest: Digest::new(0),
                error: ErrorCode::Ok,
            }; MAX_FAILED_PATHS],
            failed_len: 0,
            pending: [PendingItem {
                kind: PendingKind::Approval,
                seq: 0,
                digest: Digest::new(0),
            }; MAX_PENDING],
            pending_len: 0,
            fingerprints: [0; MAX_FINGERPRINTS],
            fingerprints_len: 0,
        }
    }

    /// Completed-subgoal notes, oldest first.
    #[must_use]
    pub fn completed(&self) -> &[Note] {
        self.completed
            .get(..usize::from(self.completed_len))
            .unwrap_or(&[])
    }

    /// Open-subgoal notes (obligations), oldest first.
    #[must_use]
    pub fn open(&self) -> &[Note] {
        self.open.get(..usize::from(self.open_len)).unwrap_or(&[])
    }

    /// Accepted-decision digests, oldest first.
    #[must_use]
    pub fn accepted_decisions(&self) -> &[Digest] {
        self.accepted
            .get(..usize::from(self.accepted_len))
            .unwrap_or(&[])
    }

    /// Retained facts, oldest first.
    #[must_use]
    pub fn facts(&self) -> &[Fact] {
        self.facts.get(..usize::from(self.facts_len)).unwrap_or(&[])
    }

    /// Ruled-out `(tool, args_digest, error)` tuples. Never drops.
    #[must_use]
    pub fn failed_paths(&self) -> &[FailedPath] {
        self.failed
            .get(..usize::from(self.failed_len))
            .unwrap_or(&[])
    }

    /// Open obligations. Never drops except via [`Self::resolve_pending`].
    #[must_use]
    pub fn pending(&self) -> &[PendingItem] {
        self.pending
            .get(..usize::from(self.pending_len))
            .unwrap_or(&[])
    }

    /// Recent event fingerprints for loop detection, oldest first.
    #[must_use]
    pub fn fingerprints(&self) -> &[u64] {
        self.fingerprints
            .get(..usize::from(self.fingerprints_len))
            .unwrap_or(&[])
    }

    /// Record a completed subgoal (drops the oldest when full).
    pub fn record_completed(&mut self, note: Note) {
        push_bounded(&mut self.completed, &mut self.completed_len, note);
    }

    /// Record an open subgoal / obligation (drops the oldest when full).
    pub fn record_open(&mut self, note: Note) {
        push_bounded(&mut self.open, &mut self.open_len, note);
    }

    /// Record an accepted decision digest (drops the oldest when full).
    pub fn record_decision(&mut self, digest: Digest) {
        push_bounded(&mut self.accepted, &mut self.accepted_len, digest);
    }

    /// Record a fact (drops the oldest when full; dedupes identical
    /// text from the same source).
    pub fn record_fact(&mut self, fact: Fact) {
        let at = usize::from(self.facts_len);
        if self.facts.iter().take(at).any(|f| *f == fact) {
            return;
        }
        push_bounded(&mut self.facts, &mut self.facts_len, fact);
    }

    /// Record a ruled-out path. Fails closed when the store is full —
    /// a failed path is never silently dropped (invariant (a)).
    ///
    /// # Errors
    ///
    /// Returns [`Error::CompactStateFull`] when a *new* path arrives
    /// and the store already holds [`MAX_FAILED_PATHS`].
    pub fn record_failed_path(&mut self, path: FailedPath) -> Result<(), Error> {
        let at = usize::from(self.failed_len);
        if self.failed.iter().take(at).any(|p| *p == path) {
            return Ok(());
        }
        if at >= MAX_FAILED_PATHS {
            return Err(Error::CompactStateFull);
        }
        push_bounded(&mut self.failed, &mut self.failed_len, path);
        Ok(())
    }

    /// Record an open obligation. Fails closed when the store is full —
    /// an obligation is never silently dropped (invariant (b)).
    ///
    /// # Errors
    ///
    /// Returns [`Error::CompactStateFull`] when a *new* obligation
    /// arrives and the store already holds [`MAX_PENDING`].
    pub fn record_pending(&mut self, item: PendingItem) -> Result<(), Error> {
        let at = usize::from(self.pending_len);
        if self
            .pending
            .iter()
            .take(at)
            .any(|p| p.kind == item.kind && p.seq == item.seq)
        {
            return Ok(());
        }
        if at >= MAX_PENDING {
            return Err(Error::CompactStateFull);
        }
        push_bounded(&mut self.pending, &mut self.pending_len, item);
        Ok(())
    }

    /// Acknowledge a resolved obligation; returns whether one was
    /// present. This is the only way obligations leave the state.
    pub fn resolve_pending(&mut self, kind: PendingKind, seq: u32) -> bool {
        let at = usize::from(self.pending_len);
        if let Some(pos) = self
            .pending
            .iter()
            .take(at)
            .position(|p| p.kind == kind && p.seq == seq)
        {
            self.pending.copy_within(pos + 1.., pos);
            self.pending_len = self.pending_len.saturating_sub(1);
            true
        } else {
            false
        }
    }

    /// Record a recent event fingerprint (drops the oldest when full).
    pub fn record_fingerprint(&mut self, fingerprint: u64) {
        push_bounded(
            &mut self.fingerprints,
            &mut self.fingerprints_len,
            fingerprint,
        );
    }
}

/// Push onto a fixed list, dropping the oldest entry when full.
fn push_bounded<T: Copy, const N: usize>(slot: &mut [T; N], len: &mut u8, item: T) {
    let at = usize::from(*len);
    if let Some(dst) = slot.get_mut(at) {
        *dst = item;
        *len = len.saturating_add(1);
    } else {
        slot.copy_within(1.., 0);
        if let Some(dst) = slot.last_mut() {
            *dst = item;
        }
    }
}

/// The minimal per-frame view the runtime feeds [`compact`].
///
/// Payloads must already be masked (see [`crate::mask`]). For
/// `ModelDecision` frames, `args_digest` carries the decision digest
/// (the compact state keeps accepted decisions as digests only — ADR 4:
/// no persisted chain of thought).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameSummary<'a> {
    /// Frame sequence number.
    pub seq: u32,
    /// Which logical record this frame was.
    pub kind: RecordKind,
    /// The tool involved.
    pub tool: ToolId,
    /// Digest of the frame's arguments (decision digest for decisions).
    pub args_digest: Digest,
    /// Success or the §6 error class.
    pub outcome: StepOutcome,
    /// Progress derived from the committed trajectory.
    pub progress: ProgressDelta,
    /// Masked payload bytes (replaced by a compact marker when superseded).
    pub payload: &'a [u8],
    /// This frame opened an obligation awaiting human input.
    pub pending_approval: bool,
    /// This frame opened an obligation awaiting verification.
    pub pending_verification: bool,
}

/// What [`compact`] did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CompactionReport {
    /// Frames folded.
    pub frames_in: u32,
    /// Frames in the compacted region (outside the verbatim tail).
    pub frames_compacted: u32,
    /// Frames the runtime keeps verbatim.
    pub tail_kept: u8,
    /// Sum of compacted-region payload bytes: the volatile bytes the
    /// runtime's compact markers replace (an estimate, not a proof).
    pub bytes_saved_estimate: u64,
}

/// Fold `frames` into `state` per `policy`.
///
/// Every frame contributes its event fingerprint, its failed path
/// (when [`StepOutcome::Failed`]), and its open obligations. Frames in
/// the compacted region (all but the last `tail_keep`) additionally
/// have their payload bytes counted as saved. Facts, completed
/// subgoals, and accepted decisions are folded from *all* frames so
/// the compact state stands alone at a `continue_as_new` boundary —
/// the verbatim tail is a prompt-construction optimization for the
/// current run, not durable state.
///
/// Folding is idempotent across overlapping windows: failed paths,
/// obligations, and facts dedupe, so re-feeding frames is safe.
///
/// # Errors
///
/// Returns [`Error::CompactStateFull`] when a new failed path or
/// obligation arrives and its never-drop store is full.
pub fn compact(
    frames: &[FrameSummary],
    policy: &CompactionPolicy,
    state: &mut CompactState,
) -> Result<CompactionReport, Error> {
    let n = frames.len();
    let tail_keep = usize::from(policy.tail_keep.min(u8::try_from(n).unwrap_or(u8::MAX)));
    let compacted = n.saturating_sub(tail_keep);
    let mut bytes_saved: u64 = 0;
    for (index, frame) in frames.iter().enumerate() {
        state.record_fingerprint(event_fingerprint(frame));
        if let StepOutcome::Failed(error) = frame.outcome {
            state.record_failed_path(FailedPath {
                tool: frame.tool,
                args_digest: frame.args_digest,
                error,
            })?;
        }
        if frame.pending_approval {
            state.record_pending(PendingItem {
                kind: PendingKind::Approval,
                seq: frame.seq,
                digest: Digest::of_bytes(frame.payload),
            })?;
        }
        if frame.pending_verification {
            state.record_pending(PendingItem {
                kind: PendingKind::Verification,
                seq: frame.seq,
                digest: Digest::of_bytes(frame.payload),
            })?;
        }
        if index < compacted {
            bytes_saved = bytes_saved.saturating_add(frame.payload.len() as u64);
        }
        match frame.progress {
            ProgressDelta::NewEvidence if frame.outcome == StepOutcome::Ok => {
                state.record_fact(Fact {
                    text: Note::truncated_from(frame.payload),
                    source_seq: frame.seq,
                });
            }
            ProgressDelta::SubgoalClosed => {
                state.record_completed(Note::truncated_from(frame.payload));
            }
            _ => {}
        }
        if frame.kind == RecordKind::ModelDecision {
            state.record_decision(frame.args_digest);
        }
    }
    Ok(CompactionReport {
        frames_in: u32::try_from(n).unwrap_or(u32::MAX),
        frames_compacted: u32::try_from(compacted).unwrap_or(u32::MAX),
        tail_kept: policy.tail_keep.min(u8::try_from(n).unwrap_or(u8::MAX)),
        bytes_saved_estimate: bytes_saved,
    })
}

/// The loop-detection fingerprint of one frame: FNV-1a-64 over the
/// frame's identity fields (never over payload bytes).
fn event_fingerprint(frame: &FrameSummary) -> u64 {
    let mut buf = [0u8; 16];
    buf[0..4].copy_from_slice(&frame.seq.to_le_bytes());
    buf[4] = frame.kind.code();
    buf[5] = frame.tool.get();
    buf[6..14].copy_from_slice(&frame.args_digest.get().to_le_bytes());
    buf[14] = match frame.outcome {
        StepOutcome::Ok => 0,
        StepOutcome::Failed(code) => code.code(),
    };
    buf[15] = match frame.progress {
        ProgressDelta::NewEvidence => 0,
        ProgressDelta::StateChanged => 1,
        ProgressDelta::SubgoalClosed => 2,
        ProgressDelta::InputReceived => 3,
        ProgressDelta::NoProgress => 4,
    };
    fnv1a64(&buf)
}
