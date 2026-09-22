//! Masking false-positive rate (§20.7): benign-shape esper traffic
//! through `mask_report`, with human-judged verdicts.
//!
//! The corpus is ordinary journal payload text — tool args,
//! observations, prompts, summaries — containing no secrets and no
//! PII. Measured 2026-09-22: 2 of 20 benign inputs (10%) had at
//! least one span redacted, both digit-run phone shapes misjudged by
//! the shape-based matcher:
//!
//! * `{"uptime_ms": 1234567}` — a 7-digit device counter is not a
//!   phone number (§20.1 redacts bare 7–11 digit runs by design);
//! * `rotated on 2026-09-22 at noon` — an ISO date's 8 separator-
//!   shaped digits fall in the 7–15 phone range.
//!
//! Both are genuine false positives of shape-based matching (the
//! trade §20.1 names explicitly). The test pins the exact flagged
//! set: any change in masking behavior — an improvement that stops
//! flagging dates, or a regression that starts flagging more — fails
//! loudly and the verdicts get re-judged.

use esper_core::mask::{self, MaskReport};

/// Benign-shape inputs: realistic esper payload text with no secrets
/// and no PII. Each entry names the traffic it imitates.
const CORPUS: &[(&str, &str)] = &[
    ("tool args (read)", r#"{"pin": 4}"#),
    ("tool args (write)", r#"{"pin": 4, "level": "high"}"#),
    ("tool observation", r#"{"level":"low","pin":4}"#),
    ("verification expected", r#"{"level":"high","pin":4}"#),
    ("ask prompt", "Which pin should be set high?"),
    ("finish summary", "cycled pins 4-6 high then low, verified"),
    ("terminal reason", "model_finished"),
    ("status uptime", r#"{"uptime_ms": 1234567}"#),
    ("status clock", r#"{"clock_ms": 90061}"#),
    ("short sk prefix", "the sk-abc label is not a key"),
    ("bare bearer", "use bearer auth for the uplink"),
    ("short akia", "AKIAIOSFODNN7EX is truncated"),
    ("unclosed pem", "-----BEGIN FOO without an end marker"),
    ("localhost mail", "notify user@localhost on completion"),
    ("version string", "workflow 1, catalog v1.2.3"),
    ("read-back note", "read-back matched the expected state"),
    ("hex digest", "fingerprint deadbeefcafef00d recorded"),
    ("iso date", "rotated on 2026-09-22 at noon"),
    ("small counters", r#"{"turns": 3, "mutations": 1}"#),
    ("pin word", "pin 4 is the actuator pin"),
];

/// The human-judged false positives, by corpus name: inputs whose
/// redacted span is not a secret or PII.
const KNOWN_FALSE_POSITIVES: &[&str] = &["status uptime", "iso date"];

/// Mask one corpus input, returning the report and the masked text.
fn probe(input: &str) -> (MaskReport, String) {
    // HOST-ONLY (E0/E1)
    let mut out = vec![0u8; mask::mask_bound(input.len())];
    let report = mask::mask_report(input.as_bytes(), &mut out).expect("bound-sized buffer");
    let text = String::from_utf8_lossy(&out[..report.output_len]).into_owned();
    (report, text)
}

#[test]
fn benign_corpus_false_positive_rate() {
    // HOST-ONLY (E0/E1)
    let mut flagged: Vec<&str> = Vec::new();
    for (name, input) in CORPUS {
        let (report, masked) = probe(input);
        if report.redacted_spans > 0 {
            flagged.push(name);
            println!("FLAGGED [{name}]: {input:?} -> {masked:?}");
        }
    }
    println!(
        "false-positive rate: {} of {} benign inputs ({:.0}%)",
        flagged.len(),
        CORPUS.len(),
        100.0 * flagged.len() as f64 / CORPUS.len() as f64
    );
    assert_eq!(
        flagged, KNOWN_FALSE_POSITIVES,
        "the flagged set changed: re-judge every newly flagged input before updating the verdicts"
    );
}
