//! E4 negative tests: the context-lifecycle machinery fails closed.
//!
//! * every single-bit flip of an encoded snapshot is rejected by
//!   both [`esper_core::snapshot::verify`] and
//!   [`esper_core::snapshot::decode`];
//! * a tampered version byte reads as
//!   [`Error::SnapshotVersionMismatch`] — a version problem, never a
//!   corruption problem (§20.3 orders the checks);
//! * truncation at representative boundary lengths reads as
//!   [`Error::SnapshotTruncated`];
//! * an undersized mask buffer fails with
//!   [`Error::MaskOutputTooSmall`] instead of truncating;
//! * [`continue_as_new`] refuses a lineage that widens any budget
//!   unit (ADR 10).
//!
//! The stale-reference rule (§20.5) through `drive_run` with a tiny
//! context budget is not tested here: it needs the runtime crew's
//! rollover commit (journal rollover does not exist yet), and the
//! task forbids inventing a workaround.

use esper_core::budget::ResourceBudget;
use esper_core::compact::{
    CompactState, Fact, FailedPath, Note, PendingItem, PendingKind, VersionSet,
};
use esper_core::error::{Error, ErrorCode};
use esper_core::ids::{Digest, RunId, ToolId};
use esper_core::lineage::{Lineage, continue_as_new};
use esper_core::{mask, snapshot};

/// A compact state with every list populated, so the snapshot under
/// test exercises all payload regions.
fn populated_state() -> CompactState {
    // HOST-ONLY (E0/E1)
    let mut state = CompactState::new(
        Digest::new(0x1234),
        ResourceBudget::new(7, 1000, 500, 60_000, 0, 3, 1),
        VersionSet {
            workflow: 1,
            model: Digest::new(1),
            catalog: Digest::new(2),
            policy: Digest::new(3),
        },
    );
    state.record_completed(Note::truncated_from(b"subgoal: pins verified"));
    state.record_decision(Digest::new(0x99));
    state.record_fact(Fact {
        text: Note::from_bytes(b"pin 4 is high").expect("short note"),
        source_seq: 12,
    });
    state
        .record_failed_path(FailedPath {
            tool: ToolId::new(2),
            args_digest: Digest::new(0xabcd),
            error: ErrorCode::Permanent,
        })
        .expect("room for a failed path");
    state
        .record_pending(PendingItem {
            kind: PendingKind::Approval,
            seq: 9,
            digest: Digest::new(0x55),
        })
        .expect("room for an obligation");
    state.record_fingerprint(0xdead_beef);
    state
}

/// The encoded bytes of the populated state.
fn encoded_snapshot() -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    let state = populated_state();
    let mut bytes = vec![0u8; snapshot::encoded_len(&state)];
    let written = snapshot::encode(&state, &mut bytes).expect("encode fits its bound");
    assert_eq!(written, bytes.len());
    // The unmodified snapshot verifies and decodes: the negative
    // tests below only break it.
    snapshot::verify(&bytes).expect("pristine snapshot verifies");
    let round_trip = snapshot::decode(&bytes).expect("pristine snapshot decodes");
    assert_eq!(round_trip, state);
    bytes
}

#[test]
fn every_single_bit_flip_fails_closed() {
    // HOST-ONLY (E0/E1)
    let bytes = encoded_snapshot();
    println!("snapshot under test: {} bytes", bytes.len());
    for byte_index in 0..bytes.len() {
        for bit in 0..8 {
            let mut flipped = bytes.clone();
            flipped[byte_index] ^= 1 << bit;
            assert!(
                snapshot::verify(&flipped).is_err(),
                "verify accepted a flip at byte {byte_index} bit {bit}"
            );
            assert!(
                snapshot::decode(&flipped).is_err(),
                "decode accepted a flip at byte {byte_index} bit {bit}"
            );
        }
    }
}

#[test]
fn tampered_version_byte_is_a_version_problem() {
    // HOST-ONLY (E0/E1)
    let bytes = encoded_snapshot();
    // §20.3: version is checked before the checksum, so a foreign
    // version byte reads as a version problem even though the
    // checksum no longer matches either.
    for found in [0u8, 2, 42, 255] {
        let mut tampered = bytes.clone();
        tampered[0] = found;
        assert_eq!(
            snapshot::verify(&tampered),
            Err(Error::SnapshotVersionMismatch { found }),
            "version byte {found}"
        );
        assert_eq!(
            snapshot::decode(&tampered),
            Err(Error::SnapshotVersionMismatch { found }),
            "version byte {found}"
        );
    }
}

#[test]
fn truncation_at_boundary_lengths_fails_closed() {
    // HOST-ONLY (E0/E1)
    let bytes = encoded_snapshot();
    let len = bytes.len();
    // Below version+checksum bytes: truncated. At or above it, the
    // last 8 bytes read as the checksum and cannot match the
    // truncated body: checksum mismatch. Both fail closed; the
    // boundary documents the implementation's rule (§20.3 orders the
    // checks: length, then version, then integrity).
    for cut in [0usize, 1, 8] {
        assert_eq!(
            snapshot::verify(&bytes[..cut]),
            Err(Error::SnapshotTruncated),
            "truncated to {cut} of {len} bytes"
        );
        assert_eq!(
            snapshot::decode(&bytes[..cut]),
            Err(Error::SnapshotTruncated),
            "truncated to {cut} of {len} bytes"
        );
    }
    for cut in [9, len.saturating_sub(9), len.saturating_sub(1)] {
        assert_eq!(
            snapshot::verify(&bytes[..cut]),
            Err(Error::SnapshotChecksumMismatch),
            "truncated to {cut} of {len} bytes"
        );
        assert_eq!(
            snapshot::decode(&bytes[..cut]),
            Err(Error::SnapshotChecksumMismatch),
            "truncated to {cut} of {len} bytes"
        );
    }
}

#[test]
fn mask_fails_closed_on_undersized_output() {
    // HOST-ONLY (E0/E1)
    let input = b"rotate key sk-live-abcdefghijklmnop for op@example.com";
    let mut tiny = [0u8; 4];
    assert_eq!(
        mask::mask_bytes(input, &mut tiny),
        Err(Error::MaskOutputTooSmall)
    );
    // And the bound-sized buffer succeeds, so the failure above is
    // the size, not the input.
    let mut sized = vec![0u8; mask::mask_bound(input.len())];
    let written = mask::mask_bytes(input, &mut sized).expect("bound-sized buffer");
    assert!(written > 0);
}

#[test]
fn continue_as_new_rejects_widened_budgets() {
    // HOST-ONLY (E0/E1)
    let state = populated_state();
    let versions = VersionSet {
        workflow: 1,
        model: Digest::new(1),
        catalog: Digest::new(2),
        policy: Digest::new(3),
    };
    // One more model turn than the compacted state holds: widened.
    let widened = Lineage {
        parent_run: RunId::new(0x42),
        continued_at_frame: 30,
        budgets_remaining: ResourceBudget::new(8, 1000, 500, 60_000, 0, 3, 1),
        versions,
    };
    assert!(
        matches!(
            continue_as_new(&state, &widened),
            Err(Error::BudgetWidened(_))
        ),
        "a lineage granting 8 turns over a 7-turn state must fail closed"
    );
    // Equal-or-narrower budgets continue, with budgets replaced and
    // the never-drop sets riding along untouched.
    let narrower = Lineage {
        budgets_remaining: ResourceBudget::new(6, 900, 500, 60_000, 0, 3, 1),
        ..widened
    };
    let continued = continue_as_new(&state, &narrower).expect("narrower budgets continue");
    assert_eq!(continued.state.budgets.model_turns, 6);
    assert_eq!(continued.state.budgets.input_tokens, 900);
    assert_eq!(continued.lineage.parent_run, RunId::new(0x42));
    assert_eq!(continued.lineage.continued_at_frame, 30);
    assert_eq!(continued.state, {
        let mut expected = state;
        expected.budgets = narrower.budgets_remaining;
        expected
    });
}
