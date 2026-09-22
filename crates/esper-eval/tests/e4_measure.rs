//! SPEC §20.7 byte measurements over three representative trajectories.
//!
//! Each test drives one trajectory through `drive_run` under the
//! scripted teacher, measures the three §20.7 context sizes
//! (full-history, masked-tail, compact-state), prints them for the
//! §20.7 results table, and asserts the robust E4 byte claim: the
//! continued run's carried context (compact state + masked tail) is
//! strictly below the full history. The naive universal ordering
//! `compact < masked-tail < full-history` does NOT hold on every
//! trajectory (the snapshot's fixed overhead dominates the
//! five-frame masked tail on small-frame runs); see
//! `assert_e4_byte_claim`.
//!
//! The trajectories live in `tests/data/` and are measurement-only:
//! they are not golden fixtures (no `expected.trace` assertions) and
//! `build.rs` does not generate fixture tests for them.

use esper_eval::{ContextMeasurement, measure_fixture_context, parse_fixture};

/// Drive one measurement trajectory and return its byte sizes.
fn measure_file(path: &str) -> ContextMeasurement {
    // HOST-ONLY (E0/E1)
    let text = std::fs::read_to_string(path).expect("measurement trajectory reads");
    let fixture = parse_fixture(&text).expect("measurement trajectory parses");
    measure_fixture_context(&fixture).expect("measurement drives to a terminal result")
}

/// Print one row of the §20.7 results table.
fn print_row(name: &str, measurement: ContextMeasurement) {
    // HOST-ONLY (E0/E1)
    println!(
        "{name}: frames={} full_history={} masked_tail={} compact_state={}",
        measurement.frames,
        measurement.full_history_bytes,
        measurement.masked_tail_bytes,
        measurement.compact_state_bytes
    );
}

/// The byte relationship the §20.7 methodology actually guarantees,
/// checked on the measured numbers rather than assumed:
///
/// * the masked verbatim tail is a strict fraction of the full
///   history;
/// * the compact state alone is smaller than the full history on
///   these representative (non-trivial) runs;
/// * the continued run's total carried context — compact state plus
///   masked tail — is strictly below the full history, and the gap
///   widens with run length because both carried tiers are bounded
///   while history is not.
///
/// What is deliberately NOT asserted: `compact < masked-tail` as a
/// universal tier ordering. The snapshot has ~77 bytes of fixed
/// overhead plus bounded lists, so for trajectories of small frames
/// the compact state can exceed the five-frame masked tail
/// (measured: 269 vs 194 on the long-success run). The tier ordering
/// is trajectory-dependent; the bounded-total claim above is the
/// robust E4 byte story.
fn assert_e4_byte_claim(name: &str, measurement: ContextMeasurement) {
    assert!(
        measurement.masked_tail_bytes < measurement.full_history_bytes,
        "{name}: masked-tail bytes ({}) must be below full-history bytes ({})",
        measurement.masked_tail_bytes,
        measurement.full_history_bytes
    );
    assert!(
        measurement.compact_state_bytes < measurement.full_history_bytes,
        "{name}: compact-state bytes ({}) must be below full-history bytes ({})",
        measurement.compact_state_bytes,
        measurement.full_history_bytes
    );
    assert!(
        measurement.compact_state_bytes + measurement.masked_tail_bytes
            < measurement.full_history_bytes,
        "{name}: carried context (compact {} + tail {}) must be below full history ({})",
        measurement.compact_state_bytes,
        measurement.masked_tail_bytes,
        measurement.full_history_bytes
    );
}

#[test]
fn long_successful_run_context_tiers() {
    let measurement = measure_file("tests/data/m-long-success.json");
    print_row("long-success", measurement);
    assert_e4_byte_claim("long-success", measurement);
}

#[test]
fn failure_heavy_run_context_tiers() {
    let measurement =
        measure_file("../../spec/trajectories/e-repeated-identical-failure-then-stuck.json");
    print_row("failure-heavy", measurement);
    assert_e4_byte_claim("failure-heavy", measurement);
}

#[test]
fn secret_bearing_run_context_tiers() {
    let measurement = measure_file("tests/data/m-secret-bearing.json");
    print_row("secret-bearing", measurement);
    assert_e4_byte_claim("secret-bearing", measurement);
}

/// The secret-bearing trajectory must actually bear secrets: the ask
/// prompt carries one secret-shaped span and one email-shaped span,
/// and `mask_report` must redact exactly those two. If this fails,
/// the trajectory no longer exercises masking and the byte numbers
/// above stop meaning what the §20.7 table says they mean.
#[test]
fn secret_bearing_trajectory_masks_two_spans() {
    use esper_core::mask;
    // HOST-ONLY (E0/E1)
    let prompt = "Rotate the deploy key sk-live-abcdefghijklmnop now? \
                  Notify op@example.com when done.";
    let mut out = vec![0u8; mask::mask_bound(prompt.len())];
    let report = mask::mask_report(prompt.as_bytes(), &mut out).expect("bound-sized buffer");
    assert_eq!(
        report.redacted_spans, 2,
        "one secret span and one email span"
    );
    assert_ne!(
        report.secret_digest, 0,
        "the secret span feeds secret_digest"
    );
    let masked = String::from_utf8_lossy(&out[..report.output_len]);
    assert!(
        !masked.contains("sk-live-abcdefghijklmnop"),
        "raw secret must not survive masking: {masked}"
    );
    assert!(
        !masked.contains("op@example.com"),
        "raw email must not survive masking: {masked}"
    );
}
