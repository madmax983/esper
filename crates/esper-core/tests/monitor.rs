//! Runtime monitor rules (§8.3), including the verification-failure
//! path (§9.2): repeated `VerificationFailed` observations end the run
//! `Stuck` with no auto-compensation.

use esper_core::budget::ResourceBudget;
use esper_core::error::ErrorCode;
use esper_core::ids::{Digest, ToolId};
use esper_core::monitor::{Monitor, MonitorVerdict, ProgressDelta, StepOutcome, StepRecord};
use esper_core::state::TerminalStatus;

const fn budget() -> ResourceBudget {
    ResourceBudget::new(10, 4000, 1000, 60_000, 0, 4, 0)
}

const fn tool() -> ToolId {
    ToolId::new(2)
}

fn digest() -> Digest {
    Digest::of_bytes(b"{\"pin\": 4, \"level\": \"high\"}")
}

fn ok_record() -> StepRecord {
    StepRecord::ok(tool(), digest(), ProgressDelta::NewEvidence)
}

fn failed_record(code: ErrorCode) -> StepRecord {
    StepRecord::failed(tool(), digest(), code, ProgressDelta::NoProgress)
}

#[test]
fn happy_path_continues() {
    let mut monitor = Monitor::new();
    for _ in 0..4 {
        assert_eq!(
            monitor.observe(ok_record(), &budget()),
            MonitorVerdict::Continue
        );
    }
}

#[test]
fn three_identical_verification_failures_end_stuck() {
    // §9.2: no auto-compensation; the monitor bounds model retries.
    let mut monitor = Monitor::new();
    let b = budget();
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::VerificationFailed), &b),
        MonitorVerdict::Continue
    );
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::VerificationFailed), &b),
        MonitorVerdict::Continue
    );
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::VerificationFailed), &b),
        MonitorVerdict::Degrade {
            status: TerminalStatus::Stuck,
            reason: "three identical (tool, args, error) failures",
        }
    );
}

#[test]
fn identical_failure_streak_breaks_on_a_different_error() {
    let mut monitor = Monitor::new();
    let b = budget();
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::VerificationFailed), &b),
        MonitorVerdict::Continue
    );
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::VerificationFailed), &b),
        MonitorVerdict::Continue
    );
    // A different error class breaks the streak...
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::Transient), &b),
        MonitorVerdict::Continue
    );
    // ...so the restarted streak is not yet terminal. Four events keep
    // the error-majority rule (§8.3, tested separately) out of this
    // test's scope.
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::VerificationFailed), &b),
        MonitorVerdict::Continue
    );
}

#[test]
fn identical_failure_streak_breaks_on_success() {
    let mut monitor = Monitor::new();
    let b = budget();
    for _ in 0..2 {
        assert_eq!(
            monitor.observe(failed_record(ErrorCode::Permanent), &b),
            MonitorVerdict::Continue
        );
    }
    assert_eq!(
        monitor.observe(ok_record(), &b),
        MonitorVerdict::Continue,
        "a success resets the identical-failure streak"
    );
    // Four events keep the error-majority rule (§8.3, tested separately)
    // out of this test's scope; the restarted streak is not yet terminal.
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::Permanent), &b),
        MonitorVerdict::Continue
    );
}

#[test]
fn identical_failures_need_same_tool_and_args() {
    let mut monitor = Monitor::new();
    let b = budget();
    let other_digest = Digest::of_bytes(b"{\"pin\": 5, \"level\": \"high\"}");
    let other_tool = ToolId::new(1);
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::Transient), &b),
        MonitorVerdict::Continue
    );
    assert_eq!(
        monitor.observe(
            StepRecord::failed(
                other_tool,
                digest(),
                ErrorCode::Transient,
                ProgressDelta::NoProgress
            ),
            &b
        ),
        MonitorVerdict::Continue,
        "different tool breaks the streak"
    );
    assert_eq!(
        monitor.observe(failed_record(ErrorCode::Transient), &b),
        MonitorVerdict::Continue
    );
    assert_eq!(
        monitor.observe(
            StepRecord::failed(
                tool(),
                other_digest,
                ErrorCode::Transient,
                ProgressDelta::NoProgress
            ),
            &b
        ),
        MonitorVerdict::Continue,
        "different args break the streak"
    );
    // Four events: the identical-streak rule is tested here, the
    // error-majority rule (§8.3) separately.
}

#[test]
fn five_consecutive_no_progress_ends_stuck() {
    let mut monitor = Monitor::new();
    let b = budget();
    // Use distinct digests so the identical-failure rule cannot fire.
    for i in 0..4u8 {
        let record = StepRecord::ok(tool(), Digest::new(u64::from(i)), ProgressDelta::NoProgress);
        assert_eq!(monitor.observe(record, &b), MonitorVerdict::Continue);
        assert_eq!(monitor.no_progress_streak(), i + 1);
    }
    let record = StepRecord::ok(tool(), Digest::new(4), ProgressDelta::NoProgress);
    assert_eq!(
        monitor.observe(record, &b),
        MonitorVerdict::Degrade {
            status: TerminalStatus::Stuck,
            reason: "five consecutive NoProgress deltas",
        }
    );
}

#[test]
fn progress_resets_the_no_progress_streak() {
    let mut monitor = Monitor::new();
    let b = budget();
    for i in 0..4u8 {
        let record = StepRecord::ok(tool(), Digest::new(u64::from(i)), ProgressDelta::NoProgress);
        assert_eq!(monitor.observe(record, &b), MonitorVerdict::Continue);
    }
    let record = StepRecord::ok(tool(), Digest::new(99), ProgressDelta::StateChanged);
    assert_eq!(monitor.observe(record, &b), MonitorVerdict::Continue);
    assert_eq!(monitor.no_progress_streak(), 0);
    // Four more NoProgress deltas are fine again.
    for i in 0..4u8 {
        let record = StepRecord::ok(
            tool(),
            Digest::new(u64::from(100 + i)),
            ProgressDelta::NoProgress,
        );
        assert_eq!(monitor.observe(record, &b), MonitorVerdict::Continue);
    }
}

#[test]
fn error_majority_in_a_full_window_ends_stuck() {
    let mut monitor = Monitor::new();
    let b = budget();
    // Three errors out of five, with distinct digests/codes to dodge the
    // identical-failure rule and progress to dodge the NoProgress rule.
    let codes = [
        ErrorCode::Transient,
        ErrorCode::Permanent,
        ErrorCode::VerificationFailed,
    ];
    for i in 0..5u8 {
        let record = if i < 3 {
            StepRecord::failed(
                ToolId::new(i + 10),
                Digest::new(u64::from(i)),
                codes[i as usize],
                ProgressDelta::NewEvidence,
            )
        } else {
            StepRecord::ok(
                ToolId::new(i + 10),
                Digest::new(u64::from(i)),
                ProgressDelta::NewEvidence,
            )
        };
        let verdict = monitor.observe(record, &b);
        if i < 4 {
            assert_eq!(verdict, MonitorVerdict::Continue, "event {i}");
        } else {
            assert_eq!(
                verdict,
                MonitorVerdict::Degrade {
                    status: TerminalStatus::Stuck,
                    reason: "error majority in the last five events",
                },
                "3 of 5 errors is a strict majority"
            );
        }
    }
}

#[test]
fn five_consecutive_failures_trip_the_error_majority() {
    // §8.3: more than half of the last five committed events are
    // errors → Stuck, even when no three failures are identical (the
    // streak tests above stay at four events to keep this rule out).
    // `NewEvidence` progress keeps the NoProgress rule out of scope.
    let mut monitor = Monitor::new();
    let b = budget();
    let codes = [ErrorCode::Transient, ErrorCode::Permanent];
    for i in 0..5u8 {
        let record = StepRecord::failed(
            tool(),
            Digest::new(u64::from(i)),
            codes[usize::from(i) % 2],
            ProgressDelta::NewEvidence,
        );
        let verdict = monitor.observe(record, &b);
        if i < 4 {
            assert_eq!(verdict, MonitorVerdict::Continue, "event {i}");
        } else {
            assert_eq!(
                verdict,
                MonitorVerdict::Degrade {
                    status: TerminalStatus::Stuck,
                    reason: "error majority in the last five events",
                },
                "5 of 5 errors"
            );
        }
    }
}

#[test]
fn failed_no_progress_counts_toward_the_streak() {
    // Any progress variant except `NoProgress` resets the streak, so a
    // failure *carrying* `NoProgress` counts. Distinct digests and codes
    // keep the identical-failure rule out; the NoProgress rule fires
    // first (rule order) with five straight failures.
    let mut monitor = Monitor::new();
    let b = budget();
    let codes = [
        ErrorCode::Transient,
        ErrorCode::Permanent,
        ErrorCode::VerificationFailed,
        ErrorCode::OutputExhausted,
        ErrorCode::Transient,
    ];
    for i in 0..5u8 {
        let record = StepRecord::failed(
            ToolId::new(i + 30),
            Digest::new(u64::from(i)),
            codes[usize::from(i)],
            ProgressDelta::NoProgress,
        );
        let verdict = monitor.observe(record, &b);
        if i < 4 {
            assert_eq!(verdict, MonitorVerdict::Continue, "event {i}");
            assert_eq!(monitor.no_progress_streak(), i + 1);
        } else {
            assert_eq!(
                verdict,
                MonitorVerdict::Degrade {
                    status: TerminalStatus::Stuck,
                    reason: "five consecutive NoProgress deltas",
                },
                "failed NoProgress still trips the streak"
            );
        }
    }
}

#[test]
fn failure_with_progress_resets_the_streak() {
    let mut monitor = Monitor::new();
    let b = budget();
    for i in 0..3u8 {
        let record = StepRecord::failed(
            tool(),
            Digest::new(u64::from(i)),
            ErrorCode::Transient,
            ProgressDelta::NoProgress,
        );
        assert_eq!(monitor.observe(record, &b), MonitorVerdict::Continue);
    }
    assert_eq!(monitor.no_progress_streak(), 3);
    // A failure that made progress resets the streak like any other
    // non-`NoProgress` step. Four events keep the error-majority rule
    // out of this test's scope.
    let record = StepRecord::failed(
        tool(),
        Digest::new(99),
        ErrorCode::Transient,
        ProgressDelta::NewEvidence,
    );
    assert_eq!(monitor.observe(record, &b), MonitorVerdict::Continue);
    assert_eq!(monitor.no_progress_streak(), 0);
}

#[test]
fn two_errors_in_five_is_not_a_majority() {
    let mut monitor = Monitor::new();
    let b = budget();
    for i in 0..5u8 {
        let record = if i < 2 {
            StepRecord::failed(
                ToolId::new(i + 10),
                Digest::new(u64::from(i)),
                ErrorCode::Permanent,
                ProgressDelta::NewEvidence,
            )
        } else {
            StepRecord::ok(
                ToolId::new(i + 10),
                Digest::new(u64::from(i)),
                ProgressDelta::NewEvidence,
            )
        };
        assert_eq!(
            monitor.observe(record, &b),
            MonitorVerdict::Continue,
            "event {i}"
        );
    }
}

#[test]
fn majority_rule_needs_a_full_window() {
    // Four straight errors with fewer than five events: the identical
    // rule cannot fire (distinct codes) and the majority rule waits for
    // a full window, so the run continues.
    let mut monitor = Monitor::new();
    let b = budget();
    let codes = [
        ErrorCode::Transient,
        ErrorCode::Permanent,
        ErrorCode::VerificationFailed,
        ErrorCode::OutputExhausted,
    ];
    for (i, code) in (0u8..).zip(codes.iter()) {
        let record = StepRecord::failed(
            ToolId::new(20 + i),
            Digest::new(u64::from(i)),
            *code,
            ProgressDelta::NewEvidence,
        );
        assert_eq!(
            monitor.observe(record, &b),
            MonitorVerdict::Continue,
            "event {i}"
        );
    }
}

#[test]
fn exhausted_budget_degrades_with_the_unit_named() {
    let mut monitor = Monitor::new();
    let mut b = budget();
    b.model_turns = 0;
    assert_eq!(
        monitor.observe(ok_record(), &b),
        MonitorVerdict::Degrade {
            status: TerminalStatus::BudgetExhausted,
            reason: "model_turns",
        }
    );

    let mut monitor = Monitor::new();
    b = budget();
    b.mutations = 0;
    assert_eq!(
        monitor.observe(ok_record(), &b),
        MonitorVerdict::Degrade {
            status: TerminalStatus::BudgetExhausted,
            reason: "mutations",
        }
    );
}

#[test]
fn budget_check_runs_before_history_rules() {
    // Even with no history at all, an exhausted budget degrades.
    let mut monitor = Monitor::new();
    let mut b = budget();
    b.input_tokens = 0;
    assert_eq!(
        monitor.observe(ok_record(), &b),
        MonitorVerdict::Degrade {
            status: TerminalStatus::BudgetExhausted,
            reason: "input_tokens",
        }
    );
}

#[test]
fn step_outcome_error_flag() {
    assert!(!StepOutcome::Ok.is_error());
    assert!(StepOutcome::Failed(ErrorCode::Transient).is_error());
}
