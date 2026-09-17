//! Record kinds, terminal results, and verification results (§10.1, §8.2, §9).

use esper_core::decision::Level;
use esper_core::error::ErrorCode;
use esper_core::ids::{EffectId, EffectSeq, RunId};
use esper_core::records::{
    RecordKind, TerminalResult, VerificationResult, TERMINAL_REASON_MAX, TERMINAL_SUMMARY_MAX,
};
use esper_core::state::TerminalStatus;
use esper_core::Error;

#[test]
fn record_kind_codes_are_stable() {
    let codes = [
        (RecordKind::RunSeed, 1u8, 512u16),
        (RecordKind::ModelRequest, 2, 256),
        (RecordKind::ModelDecision, 3, 512),
        (RecordKind::ToolRequest, 4, 256),
        (RecordKind::ToolObservation, 5, 512),
        (RecordKind::VerificationRequest, 6, 128),
        (RecordKind::VerificationResult, 7, 128),
        (RecordKind::ApprovalRequest, 8, 320),
        (RecordKind::ApprovalDecision, 9, 320),
        (RecordKind::TerminalResult, 10, 512),
    ];
    for (kind, code, max_len) in codes {
        assert_eq!(kind.code(), code);
        assert_eq!(RecordKind::from_code(code), Ok(kind));
        assert_eq!(kind.max_len(), max_len, "{kind:?}");
    }
    assert_eq!(
        RecordKind::from_code(0),
        Err(Error::UnknownRecordKind { code: 0 })
    );
    assert_eq!(
        RecordKind::from_code(11),
        Err(Error::UnknownRecordKind { code: 11 })
    );
}

#[test]
fn terminal_result_enforces_bounds() {
    let ok = TerminalResult::new(TerminalStatus::Completed, b"done", b"pin 4 high")
        .expect("valid terminal result");
    assert_eq!(ok.status(), TerminalStatus::Completed);
    assert_eq!(ok.reason(), b"done");
    assert_eq!(ok.summary(), b"pin 4 high");

    let long_reason = [b'r'; TERMINAL_REASON_MAX + 1];
    assert_eq!(
        TerminalResult::new(TerminalStatus::Stuck, &long_reason, b"x"),
        Err(Error::TerminalFieldTooLong)
    );
    let long_summary = [b's'; TERMINAL_SUMMARY_MAX + 1];
    assert_eq!(
        TerminalResult::new(TerminalStatus::Stuck, b"x", &long_summary),
        Err(Error::TerminalFieldTooLong)
    );

    // Exactly at the bound is fine.
    let reason = [b'r'; TERMINAL_REASON_MAX];
    let summary = [b's'; TERMINAL_SUMMARY_MAX];
    let at_bound =
        TerminalResult::new(TerminalStatus::Denied, &reason, &summary).expect("at bound");
    assert_eq!(at_bound.reason().len(), TERMINAL_REASON_MAX);
    assert_eq!(at_bound.summary().len(), TERMINAL_SUMMARY_MAX);
}

#[test]
fn terminal_result_rejects_non_utf8() {
    assert_eq!(
        TerminalResult::new(TerminalStatus::Stuck, b"\xff\xfe", b"ok"),
        Err(Error::TerminalFieldNotUtf8)
    );
}

#[test]
fn verification_result_pass_and_fail() {
    let effect = EffectId::new(RunId::new(7), EffectSeq::new(3));
    let pass = VerificationResult::pass(effect, Level::High);
    assert!(pass.passed());
    assert_eq!(pass.effect(), effect);
    assert_eq!(pass.expected(), Level::High);
    assert_eq!(pass.observed(), Level::High);
    assert_eq!(pass.error_code(), None);

    let fail = VerificationResult::fail(effect, Level::High, Level::Low);
    assert!(!fail.passed());
    assert_eq!(fail.expected(), Level::High);
    assert_eq!(fail.observed(), Level::Low);
    assert_eq!(fail.error_code(), Some(ErrorCode::VerificationFailed));
}
