//! SPEC §20.7 byte-measurement methodology (host-only).
//!
//! The E4 exit criterion is behavioral, but the §20.7 table needs
//! numbers. This module implements the measurement side: drive a
//! trajectory through [`esper_runtime::drive_run`] under the scripted
//! teacher (no crash injection — the methodology measures context
//! size, not recovery), then compute the three context sizes the
//! table reports:
//!
//! * **full-history bytes**: the sum, over every committed journal
//!   frame, of that frame's payload bytes — the byte-carrying fields
//!   of [`esper_runtime::Frame`] (model output, tool args, observation
//!   outcome, verification expected/observed, ask prompt, input,
//!   terminal reason/summary). This is what the continued run would
//!   have to carry without E4.
//! * **masked-tail bytes**: [`esper_core::mask::mask_bytes`] applied to
//!   the retained verbatim tail — the last
//!   [`esper_core::compact::DEFAULT_POLICY`] `tail_keep` frames — and
//!   summed. This is what the continued run keeps verbatim (masking
//!   is the runtime's job at ingestion, design §9).
//! * **compact-state bytes**: [`esper_core::snapshot::encoded_len`] of
//!   the [`esper_core::compact::CompactState`] after
//!   [`esper_core::compact::compact`] folds [`FrameSummary`] views of
//!   the (already masked) frames. This is the durable state the new
//!   run resumes from.
//!
//! Two documented simplifications versus the production runtime path:
//!
//! * every frame's progress maps to [`ProgressDelta::NoProgress`], so
//!   no facts or completed subgoals are recorded — the measured
//!   compact state is a lower bound on what the runtime would keep;
//! * identity fields (tool ids on frames that carry none, digests)
//!   are derived deterministically from the payload; they only feed
//!   fingerprints and failed-path tuples, never byte counts.
//!
//! // HOST-ONLY (E0/E1): heap-allocated measurement, for the host
//! test harness. Firmware code never measures.

use esper_core::budget::ResourceBudget;
use esper_core::compact::{
    self, CompactState, DEFAULT_POLICY, FrameSummary, PendingKind, VersionSet,
};
use esper_core::error::ErrorCode;
use esper_core::ids::{Digest, ToolId};
use esper_core::mask;
use esper_core::monitor::{ProgressDelta, StepOutcome};
use esper_core::records::RecordKind;
use esper_core::snapshot;
use esper_runtime::{
    Frame, InferenceSettings, Journal, ModelBackend, ScriptedBackend, drive_run,
};

use crate::fixture::{Fixture, FixtureError};
use crate::runner::{build_device, build_inputs, build_seed};

/// The three measured context sizes for one trajectory, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextMeasurement {
    /// Sum of journal frame payload bytes before compaction.
    pub full_history_bytes: u64,
    /// `mask_bytes` over the retained verbatim tail payloads.
    pub masked_tail_bytes: u64,
    /// `snapshot::encoded_len` of the state after `compact()`.
    pub compact_state_bytes: u64,
    /// Committed journal frames measured.
    pub frames: usize,
}

/// Drive `fixture` to its terminal result under the scripted teacher
/// (no crash injection) and measure its three §20.7 context sizes.
///
/// # Errors
///
/// Returns [`FixtureError`] when the fixture cannot be built, the
/// driver fails, masking fails on a bound-sized buffer (which would
/// mean the [`mask::mask_bound`] proof is wrong), or the trajectory
/// overflows the compact state's never-drop stores.
pub fn measure_fixture_context(
    fixture: &Fixture,
) -> Result<ContextMeasurement, FixtureError> {
    // HOST-ONLY (E0/E1)
    let mut backend = ScriptedBackend::new(
        fixture.script.clone(),
        InferenceSettings::default_settings(),
    );
    let seed = build_seed(fixture, backend.bundle_id().0)?;
    let (mut device, mut faults) = build_device(&fixture.device)?;
    let mut inputs = build_inputs(fixture);
    let mut journal = Journal::new();
    drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    )?;
    measure_journal(journal.frames())
}

/// Measure the three §20.7 context sizes over committed frames.
///
/// # Errors
///
/// Returns [`FixtureError::AssertionFailed`] when masking fails on a
/// bound-sized buffer or the frames overflow the compact state's
/// never-drop stores (both are measurement preconditions, not
/// fixture assertions).
pub fn measure_journal(frames: &[Frame]) -> Result<ContextMeasurement, FixtureError> {
    // HOST-ONLY (E0/E1)
    let raw: Vec<Vec<u8>> = frames.iter().map(frame_payload_bytes).collect();
    let full_history_bytes: u64 = raw.iter().map(|payload| payload.len() as u64).sum();

    // Mask every payload first: compact() assumes masked payloads
    // (SPEC §20.2), and the tail is measured masked.
    let mut masked: Vec<Vec<u8>> = Vec::with_capacity(raw.len());
    for payload in &raw {
        masked.push(mask_copy(payload)?);
    }

    // The retained verbatim tail: the last `tail_keep` frames, masked.
    let tail_keep = usize::from(DEFAULT_POLICY.tail_keep);
    let tail_start = masked.len().saturating_sub(tail_keep);
    let masked_tail_bytes: u64 = masked[tail_start..]
        .iter()
        .map(|payload| payload.len() as u64)
        .sum();

    // Fold masked summaries into the compact state, resolving
    // obligations the way the runtime does: a write intent opens a
    // pending verification keyed by its effect sequence, and the
    // matching verification frame resolves it; an ask opens a pending
    // approval resolved by the human's input. Feeding frames one at a
    // time is equivalent to one batch call — folding is idempotent
    // across overlapping windows (SPEC §20.2).
    let mut state = CompactState::new(
        Digest::new(0),
        ResourceBudget::new(0, 0, 0, 0, 0, 0, 0),
        VersionSet {
            workflow: 1,
            model: Digest::new(0),
            catalog: Digest::new(0),
            policy: Digest::new(0),
        },
    );
    // Effect sequence -> intent frame sequence, for open verifications.
    let mut open_writes: Vec<(u32, u32)> = Vec::new();
    // The frame sequence of the currently open ask, if any.
    let mut open_ask: Option<u32> = None;
    for (index, (frame, payload)) in frames.iter().zip(masked.iter()).enumerate() {
        let seq = u32::try_from(index).map_err(|_| FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: "measurement trajectory holds more than u32::MAX frames".to_owned(),
        })?;
        let summary = summarize(seq, frame, payload.as_slice());
        compact::compact(std::slice::from_ref(&summary), &DEFAULT_POLICY, &mut state).map_err(
            |error| FixtureError::AssertionFailed {
                // HOST-ONLY (E0/E1)
                detail: format!(
                    "measurement trajectory overflows the compact state's never-drop stores: {error:?}"
                ),
            },
        )?;
        match frame {
            Frame::ToolIntent {
                seq: effect, write, ..
            } if *write => open_writes.push((*effect, seq)),
            Frame::Verification { seq: effect, .. } => {
                if let Some(position) =
                    open_writes.iter().position(|(open, _)| open == effect)
                {
                    let (_, intent_seq) = open_writes.remove(position);
                    state.resolve_pending(PendingKind::Verification, intent_seq);
                }
            }
            Frame::ApprovalRequest { .. } => open_ask = Some(seq),
            Frame::ApprovalDecision { .. } => {
                if let Some(ask_seq) = open_ask.take() {
                    state.resolve_pending(PendingKind::Approval, ask_seq);
                }
            }
            _ => {}
        }
    }
    let compact_state_bytes = snapshot::encoded_len(&state) as u64;

    Ok(ContextMeasurement {
        full_history_bytes,
        masked_tail_bytes,
        compact_state_bytes,
        frames: frames.len(),
    })
}

/// The payload bytes of one committed frame: every byte-carrying
/// field, concatenated in field order.
fn frame_payload_bytes(frame: &Frame) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    let mut out = Vec::new();
    match frame {
        Frame::RunStarted { .. } => {}
        Frame::ModelDecision { output, .. } => out.extend_from_slice(output),
        Frame::ToolIntent { args, .. } => out.extend_from_slice(args),
        Frame::ToolObservation { outcome, .. } => out.extend_from_slice(outcome),
        Frame::Verification {
            expected, observed, ..
        } => {
            out.extend_from_slice(expected);
            out.extend_from_slice(observed);
        }
        Frame::ApprovalRequest { prompt, .. } => out.extend_from_slice(prompt),
        Frame::ApprovalDecision { input, .. } => out.extend_from_slice(input),
        Frame::Terminal {
            reason, summary, ..
        } => {
            out.extend_from_slice(reason);
            out.extend_from_slice(summary);
        }
    }
    out
}

/// Mask one payload into a fresh buffer sized by
/// [`mask::mask_bound`]; the bound is a proof, so failure here fails
/// the measurement rather than truncating.
fn mask_copy(raw: &[u8]) -> Result<Vec<u8>, FixtureError> {
    // HOST-ONLY (E0/E1)
    let mut out = vec![0u8; mask::mask_bound(raw.len())];
    let written = mask::mask_bytes(raw, &mut out).map_err(|error| {
        FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: format!("mask_bytes failed on a mask_bound-sized buffer: {error:?}"),
        }
    })?;
    out.truncate(written);
    Ok(out)
}

/// Build the [`FrameSummary`] view of one frame over its (already
/// masked) payload. Progress is uniformly [`ProgressDelta::NoProgress`]
/// — a documented measurement simplification (see the module docs).
fn summarize<'a>(seq: u32, frame: &'a Frame, payload: &'a [u8]) -> FrameSummary<'a> {
    let (kind, tool, outcome, pending_approval, pending_verification) = match frame {
        Frame::RunStarted { .. } => (
            RecordKind::RunSeed,
            ToolId::new(0),
            StepOutcome::Ok,
            false,
            false,
        ),
        Frame::ModelDecision { .. } => (
            RecordKind::ModelDecision,
            ToolId::new(0),
            StepOutcome::Ok,
            false,
            false,
        ),
        Frame::ToolIntent { tool, write, .. } => (
            RecordKind::ToolRequest,
            ToolId::new(*tool),
            StepOutcome::Ok,
            false,
            *write,
        ),
        Frame::ToolObservation { transient, .. } => (
            RecordKind::ToolObservation,
            ToolId::new(0),
            if *transient {
                StepOutcome::Failed(ErrorCode::Transient)
            } else {
                StepOutcome::Ok
            },
            false,
            false,
        ),
        Frame::Verification { passed, .. } => (
            RecordKind::VerificationResult,
            ToolId::new(0),
            if *passed {
                StepOutcome::Ok
            } else {
                StepOutcome::Failed(ErrorCode::VerificationFailed)
            },
            false,
            false,
        ),
        Frame::ApprovalRequest { .. } => (
            RecordKind::ApprovalRequest,
            ToolId::new(0),
            StepOutcome::Ok,
            true,
            false,
        ),
        Frame::ApprovalDecision { .. } => (
            RecordKind::ApprovalDecision,
            ToolId::new(0),
            StepOutcome::Ok,
            false,
            false,
        ),
        Frame::Terminal { .. } => (
            RecordKind::TerminalResult,
            ToolId::new(0),
            StepOutcome::Ok,
            false,
            false,
        ),
    };
    FrameSummary {
        seq,
        kind,
        tool,
        args_digest: Digest::of_bytes(payload),
        outcome,
        progress: ProgressDelta::NoProgress,
        payload,
        pending_approval,
        pending_verification,
    }
}
