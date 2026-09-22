//! Rung E4: context lifecycle — masking, compaction, snapshots, lineage.
//!
//! RED suite for the E4 foundation contract (SPEC §20). These tests pin the
//! public API the runtime and eval crews build against:
//! `esper_core::{mask, compact, snapshot, lineage}`.

use esper_core::budget::ResourceBudget;
use esper_core::compact::{
    CompactState, CompactionPolicy, CompactionReport, DEFAULT_POLICY, Fact, FailedPath,
    FrameSummary, MAX_FACTS, MAX_FAILED_PATHS, MAX_PENDING, NOTE_MAX, Note, PendingItem,
    PendingKind, VersionSet, compact, should_compact,
};
use esper_core::error::{Error, ErrorCode};
use esper_core::ids::{Digest, RunId, ToolId};
use esper_core::lineage::{Lineage, continue_as_new};
use esper_core::mask::{fnv1a64, mask_bound, mask_bytes, mask_report};
use esper_core::monitor::{ProgressDelta, StepOutcome};
use esper_core::records::RecordKind;
use esper_core::snapshot::{SNAPSHOT_VERSION, decode, encode, encoded_len, verify};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

const fn test_budget() -> ResourceBudget {
    ResourceBudget::new(10, 4000, 1000, 60_000, 0, 4, 0)
}

const fn test_versions() -> VersionSet {
    VersionSet {
        workflow: 3,
        model: Digest::new(11),
        catalog: Digest::new(22),
        policy: Digest::new(33),
    }
}

fn test_state() -> CompactState {
    CompactState::new(
        Digest::of_bytes(b"read pin 4"),
        test_budget(),
        test_versions(),
    )
}

#[allow(clippy::too_many_arguments)]
const fn frame(
    seq: u32,
    tool: u8,
    outcome: StepOutcome,
    progress: ProgressDelta,
    payload: &'static [u8],
    args_seed: u64,
    pending_approval: bool,
    pending_verification: bool,
) -> FrameSummary<'static> {
    FrameSummary {
        seq,
        kind: RecordKind::ToolObservation,
        tool: ToolId::new(tool),
        args_digest: Digest::new(args_seed),
        outcome,
        progress,
        payload,
        pending_approval,
        pending_verification,
    }
}

fn ok_frame(seq: u32, payload: &'static [u8]) -> FrameSummary<'static> {
    frame(
        seq,
        1,
        StepOutcome::Ok,
        ProgressDelta::NewEvidence,
        payload,
        1000 + u64::from(seq),
        false,
        false,
    )
}

fn failed_frame(seq: u32, error: ErrorCode) -> FrameSummary<'static> {
    frame(
        seq,
        2,
        StepOutcome::Failed(error),
        ProgressDelta::NoProgress,
        b"failed",
        2000 + u64::from(seq),
        false,
        false,
    )
}

fn mask_to_vec(input: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; mask_bound(input.len())];
    let n = mask_bytes(input, &mut out).expect("mask must succeed");
    out.truncate(n);
    out
}

fn mask_to_string(input: &[u8]) -> String {
    String::from_utf8(mask_to_vec(input)).expect("mask output is ASCII")
}

// ---------------------------------------------------------------------------
// mask.rs
// ---------------------------------------------------------------------------

#[test]
fn fnv1a64_matches_the_standard_vectors() {
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
}

#[test]
fn mask_redacts_each_secret_class() {
    let secrets: &[&[u8]] = &[
        b"sk-proj-abcdefghijklmnop123456",
        b"sk_live_abcdefghijklmnop123456",
        b"sk_test_abcdefghijklmnop123456",
        b"github_pat_abcdefghijklmnop1234567890",
        b"AKIAIOSFODNN7EXAMPLE",
    ];
    for secret in secrets {
        let mut input = b"token=".to_vec();
        input.extend_from_slice(secret);
        input.extend_from_slice(b" done");
        let text = mask_to_string(&input);
        assert!(
            text.contains("[redacted:secret#1]"),
            "secret marker missing for {secret:?}: {text:?}"
        );
        let secret_str = std::str::from_utf8(secret).expect("secret is ASCII");
        assert!(
            !text.contains(secret_str),
            "secret bytes leaked for {secret:?}: {text:?}"
        );
    }
}

#[test]
fn mask_redacts_bearer_tokens() {
    for scheme in [b"Bearer ".as_slice(), b"bearer ".as_slice()] {
        let mut input = b"Authorization: ".to_vec();
        input.extend_from_slice(scheme);
        input.extend_from_slice(b"abcdefghijklmnop123");
        let text = mask_to_string(&input);
        assert!(
            text.contains("[redacted:secret#1]"),
            "bearer marker missing: {text:?}"
        );
        assert!(
            !text.contains("abcdefghijklmnop123"),
            "bearer token leaked: {text:?}"
        );
    }
}

#[test]
fn mask_redacts_pem_blocks() {
    let input = b"key:\n-----BEGIN PRIVATE KEY-----\nMIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgw\n-----END PRIVATE KEY-----\nend";
    let text = mask_to_string(input);
    assert!(
        text.contains("[redacted:secret#1]"),
        "PEM marker missing: {text:?}"
    );
    assert!(
        !text.contains("PRIVATE KEY"),
        "PEM material leaked: {text:?}"
    );
    assert!(
        !text.contains("MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgw"),
        "PEM body leaked: {text:?}"
    );
}

#[test]
fn mask_redacts_pii_classes() {
    let text = mask_to_string(b"reach ada@example.com or call +1 (555) 123-4567, ref 1234567");
    assert!(
        text.contains("[redacted:pii#"),
        "PII marker missing: {text:?}"
    );
    assert!(!text.contains("ada@example.com"), "email leaked: {text:?}");
    assert!(!text.contains("555"), "phone digits leaked: {text:?}");
    assert!(!text.contains("1234567"), "digit run leaked: {text:?}");
    // The benign words survive.
    assert!(text.contains("reach"), "benign text lost: {text:?}");
    assert!(text.contains("or call"), "benign text lost: {text:?}");
}

#[test]
fn mask_leaves_benign_text_alone() {
    for benign in [
        b"set pin 3 to high".as_slice(),
        b"sk-".as_slice(),
        b"Bearer".as_slice(),
        b"user said hello".as_slice(),
        b"".as_slice(),
    ] {
        let out = mask_to_vec(benign);
        assert_eq!(out, benign, "benign input altered: {benign:?}");
    }
}

#[test]
fn mask_report_counts_spans_and_digests() {
    let input =
        b"a sk-proj-abcdefghijklmnop123456 and ada@example.com then sk_live_abcdefghijklmnop123456";
    let mut out = vec![0u8; mask_bound(input.len())];
    let report = mask_report(input, &mut out).expect("mask must succeed");
    assert_eq!(report.redacted_spans, 3);
    let mut tmp = vec![0u8; mask_bound(input.len())];
    let n = mask_bytes(input, &mut tmp).expect("mask must succeed");
    assert_eq!(report.output_len, n);
    assert_ne!(report.secret_digest, 0, "secret digest must be nonzero");

    // Deterministic across calls.
    let mut out2 = vec![0u8; mask_bound(input.len())];
    let report2 = mask_report(input, &mut out2).expect("mask must succeed");
    assert_eq!(report.secret_digest, report2.secret_digest);
    assert_eq!(report.redacted_spans, report2.redacted_spans);

    // PII-only input: spans counted, secret digest stays zero.
    let pii_only = b"mail ada@example.com";
    let mut out3 = vec![0u8; mask_bound(pii_only.len())];
    let report3 = mask_report(pii_only, &mut out3).expect("mask must succeed");
    assert_eq!(report3.redacted_spans, 1);
    assert_eq!(report3.secret_digest, 0);
}

#[test]
fn mask_secret_digest_distinguishes_secrets() {
    let a = b"key sk-proj-aaaaaaaaaaaaaaaaaaaaaaaa";
    let b = b"key sk-proj-bbbbbbbbbbbbbbbbbbbbbbbb";
    let mut out = vec![0u8; mask_bound(a.len()).max(mask_bound(b.len()))];
    let ra = mask_report(a, &mut out).expect("mask a");
    let rb = mask_report(b, &mut out).expect("mask b");
    assert_ne!(ra.secret_digest, rb.secret_digest);
}

#[test]
fn mask_bound_always_suffices() {
    // Adversarial: every 7 bytes is a minimal redacted span (separator-
    // delimited). Adjacent addresses *without* separators are scanned
    // greedily — the local part is always consumed into a redacted span —
    // so the worst case uses realistic separators.
    let input: Vec<u8> = b"a@b.co ".repeat(500);
    let mut out = vec![0u8; mask_bound(input.len())];
    let n = mask_bytes(&input, &mut out).expect("bound must suffice");
    assert!(n <= out.len());
    let text = std::str::from_utf8(&out[..n]).expect("output is ASCII");
    assert!(
        text.contains("[redacted:pii#500]"),
        "all spans kept: {text:?}"
    );

    // Mixed minimal spans.
    let mut mixed: Vec<u8> = Vec::new();
    for _ in 0..100 {
        mixed.extend_from_slice(b"a@b.co 1234567 ");
    }
    let mut out2 = vec![0u8; mask_bound(mixed.len())];
    mask_bytes(&mixed, &mut out2).expect("bound must suffice for mixed spans");
}

#[test]
fn mask_bytes_rejects_an_undersized_buffer() {
    let input = b"hello";
    let bound = mask_bound(input.len());
    assert!(bound >= input.len());
    let mut small = vec![0u8; bound - 1];
    assert_eq!(
        mask_bytes(input, &mut small),
        Err(Error::MaskOutputTooSmall)
    );
}

// ---------------------------------------------------------------------------
// compact.rs
// ---------------------------------------------------------------------------

#[test]
fn compact_keeps_a_five_frame_tail() {
    let frames: Vec<FrameSummary> = (0..8).map(|s| ok_frame(s, b"evidence")).collect();
    let mut state = test_state();
    let report: CompactionReport =
        compact(&frames, &DEFAULT_POLICY, &mut state).expect("compact must succeed");
    assert_eq!(report.frames_in, 8);
    assert_eq!(report.frames_compacted, 3);
    assert_eq!(report.tail_kept, 5);
}

#[test]
fn compact_with_fewer_frames_than_tail_compacts_nothing() {
    let frames: Vec<FrameSummary> = (0..3).map(|s| ok_frame(s, b"evidence")).collect();
    let mut state = test_state();
    let report = compact(&frames, &DEFAULT_POLICY, &mut state).expect("compact must succeed");
    assert_eq!(report.frames_in, 3);
    assert_eq!(report.frames_compacted, 0);
    assert_eq!(report.tail_kept, 3);
    assert_eq!(report.bytes_saved_estimate, 0);
}

#[test]
fn compact_preserves_every_failed_path() {
    // Invariant (a): every failed path in the compacted region MUST be
    // present in CompactState — compaction must never let the agent retry
    // a ruled-out path. The implementation is stronger: failures from the
    // verbatim tail are recorded too.
    let mut frames: Vec<FrameSummary> = Vec::new();
    frames.push(failed_frame(0, ErrorCode::Denied));
    frames.push(failed_frame(1, ErrorCode::VerificationFailed));
    for s in 2..8 {
        frames.push(ok_frame(s, b"evidence"));
    }
    // One more failure inside the verbatim tail.
    frames.push(failed_frame(8, ErrorCode::Permanent));

    let mut state = test_state();
    compact(&frames, &DEFAULT_POLICY, &mut state).expect("compact must succeed");

    for f in &frames {
        if let StepOutcome::Failed(error) = f.outcome {
            assert!(
                state.failed_paths().iter().any(|p| p.tool == f.tool
                    && p.args_digest == f.args_digest
                    && p.error == error),
                "failed path lost for seq {}: {error}",
                f.seq
            );
        }
    }
}

#[test]
fn compact_preserves_open_obligations() {
    // Invariant (b): every open obligation MUST be present.
    let mut frames: Vec<FrameSummary> = Vec::new();
    let mut approval = ok_frame(0, b"ask: which pin?");
    approval.kind = RecordKind::ApprovalRequest;
    approval.pending_approval = true;
    frames.push(approval);
    let mut verification = ok_frame(1, b"write pin 4 high");
    verification.pending_verification = true;
    frames.push(verification);
    for s in 2..7 {
        frames.push(ok_frame(s, b"evidence"));
    }

    let mut state = test_state();
    compact(&frames, &DEFAULT_POLICY, &mut state).expect("compact must succeed");

    let pending = state.pending();
    assert!(
        pending
            .iter()
            .any(|p| p.kind == PendingKind::Approval && p.seq == 0),
        "pending approval lost"
    );
    assert!(
        pending
            .iter()
            .any(|p| p.kind == PendingKind::Verification && p.seq == 1),
        "pending verification lost"
    );
}

#[test]
fn compact_dedupes_identical_failed_paths() {
    let f = failed_frame(0, ErrorCode::Denied);
    let frames = [f, f, f];
    let mut state = test_state();
    compact(&frames, &DEFAULT_POLICY, &mut state).expect("compact must succeed");
    assert_eq!(state.failed_paths().len(), 1);
}

#[test]
fn compact_failed_path_store_full_fails_closed() {
    let mut state = test_state();
    for i in 0..MAX_FAILED_PATHS {
        state
            .record_failed_path(FailedPath {
                tool: ToolId::new(2),
                args_digest: Digest::new(u64::try_from(i).expect("small")),
                error: ErrorCode::Denied,
            })
            .expect("store has room");
    }
    assert_eq!(
        state.record_failed_path(FailedPath {
            tool: ToolId::new(2),
            args_digest: Digest::new(9999),
            error: ErrorCode::Denied,
        }),
        Err(Error::CompactStateFull)
    );
}

#[test]
fn compact_pending_store_full_fails_closed() {
    let mut state = test_state();
    for i in 0..MAX_PENDING {
        state
            .record_pending(PendingItem {
                kind: PendingKind::Approval,
                seq: u32::try_from(i).expect("small"),
                digest: Digest::new(1),
            })
            .expect("store has room");
    }
    assert_eq!(
        state.record_pending(PendingItem {
            kind: PendingKind::Approval,
            seq: 9999,
            digest: Digest::new(1),
        }),
        Err(Error::CompactStateFull)
    );
}

#[test]
fn compact_records_facts_decisions_and_subgoals() {
    let frames = [
        ok_frame(0, b"pin 4 is high"),
        frame(
            1,
            2,
            StepOutcome::Ok,
            ProgressDelta::SubgoalClosed,
            b"calibrated",
            4242,
            false,
            false,
        ),
    ];
    let mut decision = ok_frame(2, b"");
    decision.kind = RecordKind::ModelDecision;
    let frames = [frames[0], frames[1], decision];

    let mut state = test_state();
    compact(&frames, &DEFAULT_POLICY, &mut state).expect("compact must succeed");

    assert!(
        state
            .facts()
            .iter()
            .any(|f| f.text.as_bytes() == b"pin 4 is high" && f.source_seq == 0),
        "fact lost"
    );
    assert!(
        state
            .completed()
            .iter()
            .any(|n| n.as_bytes() == b"calibrated"),
        "completed subgoal lost"
    );
    assert_eq!(state.accepted_decisions().len(), 1);
    // Every frame feeds the loop-detection fingerprints.
    assert_eq!(state.fingerprints().len(), 3);
}

#[test]
fn compact_bytes_saved_estimate_covers_the_compacted_region() {
    let frames: Vec<FrameSummary> = (0..8).map(|s| ok_frame(s, b"12345678")).collect();
    let mut state = test_state();
    let report = compact(&frames, &DEFAULT_POLICY, &mut state).expect("compact must succeed");
    // Frames 0..3 are compacted; each payload is 8 bytes.
    assert_eq!(report.bytes_saved_estimate, 24);
}

#[test]
fn should_compact_fires_at_eighty_percent() {
    assert!(should_compact(800, 1000, &DEFAULT_POLICY));
    assert!(should_compact(1000, 1000, &DEFAULT_POLICY));
    assert!(!should_compact(799, 1000, &DEFAULT_POLICY));
    assert!(!should_compact(0, 1000, &DEFAULT_POLICY));
    let strict = CompactionPolicy {
        trigger_pct: 50,
        tail_keep: 5,
    };
    assert!(should_compact(500, 1000, &strict));
    assert!(!should_compact(499, 1000, &strict));
}

#[test]
fn resolve_pending_acks_an_obligation() {
    let mut state = test_state();
    state
        .record_pending(PendingItem {
            kind: PendingKind::Approval,
            seq: 7,
            digest: Digest::new(3),
        })
        .expect("store has room");
    assert!(state.resolve_pending(PendingKind::Approval, 7));
    assert_eq!(state.pending().len(), 0);
    assert!(!state.resolve_pending(PendingKind::Approval, 7));
}

#[test]
fn record_fact_drops_oldest_when_full() {
    let mut state = test_state();
    for i in 0..MAX_FACTS {
        let text = Note::truncated_from(format!("fact {i}").as_bytes());
        state.record_fact(Fact {
            text,
            source_seq: u32::try_from(i).expect("small"),
        });
    }
    assert_eq!(state.facts().len(), MAX_FACTS);
    state.record_fact(Fact {
        text: Note::truncated_from(b"newest"),
        source_seq: 999,
    });
    assert_eq!(state.facts().len(), MAX_FACTS);
    assert!(state.facts().iter().all(|f| f.source_seq != 0));
    assert!(state.facts().iter().any(|f| f.source_seq == 999));
}

#[test]
fn note_rejects_overlong_text() {
    let long = [b'x'; NOTE_MAX + 1];
    assert_eq!(Note::from_bytes(&long), Err(Error::CompactFieldTooLong));
    let exact = [b'x'; NOTE_MAX];
    assert!(Note::from_bytes(&exact).is_ok());
    assert_eq!(Note::truncated_from(&long).as_bytes().len(), NOTE_MAX);
}

// ---------------------------------------------------------------------------
// snapshot.rs
// ---------------------------------------------------------------------------

fn populated_state() -> CompactState {
    let mut state = test_state();
    state
        .record_failed_path(FailedPath {
            tool: ToolId::new(2),
            args_digest: Digest::new(7),
            error: ErrorCode::Denied,
        })
        .expect("room");
    state
        .record_pending(PendingItem {
            kind: PendingKind::Approval,
            seq: 3,
            digest: Digest::new(9),
        })
        .expect("room");
    state.record_fact(Fact {
        text: Note::from_bytes(b"pin 4 is high").expect("fits"),
        source_seq: 5,
    });
    state.record_completed(Note::from_bytes(b"calibrated").expect("fits"));
    state.record_open(Note::from_bytes(b"await approval").expect("fits"));
    state.record_decision(Digest::new(42));
    state.record_fingerprint(0xdead_beef);
    state.record_fingerprint(0xcafe_f00d);
    state
}

fn encode_state(state: &CompactState) -> Vec<u8> {
    let mut buf = vec![0u8; encoded_len(state)];
    let n = encode(state, &mut buf).expect("encode must succeed");
    assert_eq!(n, encoded_len(state));
    buf.truncate(n);
    buf
}

#[test]
fn snapshot_round_trip() {
    let state = populated_state();
    let bytes = encode_state(&state);
    assert_eq!(bytes[0], SNAPSHOT_VERSION);
    let back = decode(&bytes).expect("decode must succeed");
    assert_eq!(back, state);
    verify(&bytes).expect("verify must succeed");
}

#[test]
fn snapshot_single_bit_flip_fails_closed() {
    let bytes = encode_state(&populated_state());
    // A spread of offsets: payload region, and the trailing checksum.
    let offsets = [1usize, 5, 20, 40, bytes.len() - 8, bytes.len() - 1];
    for offset in offsets {
        let mut bad = bytes.clone();
        bad[offset] ^= 0x01;
        assert!(
            decode(&bad).is_err(),
            "single-bit flip at offset {offset} must fail closed"
        );
        assert!(
            verify(&bad).is_err(),
            "verify must reject flip at offset {offset}"
        );
    }
    // A payload flip is specifically a checksum mismatch.
    let mut bad = bytes;
    bad[5] ^= 0x01;
    assert_eq!(decode(&bad), Err(Error::SnapshotChecksumMismatch));
}

#[test]
fn snapshot_truncated_input_fails() {
    let bytes = encode_state(&populated_state());
    assert_eq!(decode(&[]), Err(Error::SnapshotTruncated));
    assert_eq!(decode(&bytes[..3]), Err(Error::SnapshotTruncated));
    assert_eq!(verify(&bytes[..3]), Err(Error::SnapshotTruncated));
    // Losing the final byte keeps the length plausible but breaks integrity.
    assert_eq!(
        decode(&bytes[..bytes.len() - 1]),
        Err(Error::SnapshotChecksumMismatch)
    );
}

#[test]
fn snapshot_version_mismatch_is_distinct() {
    let mut bytes = encode_state(&populated_state());
    bytes[0] = SNAPSHOT_VERSION + 1;
    assert_eq!(
        decode(&bytes),
        Err(Error::SnapshotVersionMismatch {
            found: SNAPSHOT_VERSION + 1
        })
    );
    assert_eq!(
        verify(&bytes),
        Err(Error::SnapshotVersionMismatch {
            found: SNAPSHOT_VERSION + 1
        })
    );
}

#[test]
fn snapshot_encode_rejects_a_small_buffer() {
    let state = populated_state();
    let need = encoded_len(&state);
    assert!(need > 0);
    let mut small = vec![0u8; need - 1];
    assert_eq!(
        encode(&state, &mut small),
        Err(Error::SnapshotBufferTooSmall)
    );
}

// ---------------------------------------------------------------------------
// lineage.rs
// ---------------------------------------------------------------------------

#[test]
fn continue_as_new_carries_remaining_budgets() {
    let state = populated_state();
    let lineage = Lineage {
        parent_run: RunId::new(99),
        continued_at_frame: 12,
        budgets_remaining: ResourceBudget::new(7, 3000, 800, 50_000, 0, 3, 1),
        versions: test_versions(),
    };
    let continued = continue_as_new(&state, &lineage).expect("continuation must succeed");
    assert_eq!(continued.state.budgets.model_turns, 7);
    assert_eq!(continued.state.budgets.input_tokens, 3000);
    assert_eq!(continued.lineage.parent_run, RunId::new(99));
    assert_eq!(continued.lineage.continued_at_frame, 12);
    // Everything else rides along untouched.
    assert_eq!(continued.state.failed_paths(), state.failed_paths());
    assert_eq!(continued.state.pending(), state.pending());
    assert_eq!(continued.state.facts(), state.facts());
}

#[test]
fn continue_as_new_rejects_widened_budgets() {
    let state = populated_state();
    // 11 model turns remain in the lineage but the compacted state only
    // has 10 left: that would widen the run identity.
    let lineage = Lineage {
        parent_run: RunId::new(99),
        continued_at_frame: 12,
        budgets_remaining: ResourceBudget::new(11, 3000, 800, 50_000, 0, 3, 1),
        versions: test_versions(),
    };
    assert!(matches!(
        continue_as_new(&state, &lineage),
        Err(Error::BudgetWidened(_))
    ));
}
