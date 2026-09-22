//! State machine tests, derived from the §3.2 transition table.
//!
//! The expected table below is transcribed from SPEC §3.2 by hand, so it
//! is an independent check on the crate's `TRANSITIONS` data: any typo on
//! either side fails loudly.

use esper_core::Error;
use esper_core::records::RecordKind;
use esper_core::state::{
    ALL_EVENTS, ALL_STATES, Event, State, TerminalStatus, committed_records, is_legal, transition,
};

/// The §3.2 table, transcribed independently of the crate's data.
const EXPECTED: [(State, Event, State); 25] = [
    (State::Recover, Event::JournalValid, State::Gather),
    (State::Recover, Event::JournalCorrupt, State::SafeStop),
    (State::Gather, Event::BudgetsClear, State::Infer),
    (State::Gather, Event::BudgetExhausted, State::Degraded),
    (State::Gather, Event::IncompatibleVersion, State::Degraded),
    (State::Infer, Event::DecodeCall, State::Authorize),
    (State::Infer, Event::DecodeAsk, State::AwaitInput),
    (State::Infer, Event::DecodeFinish, State::Finalize),
    (State::Infer, Event::DecodeMalformed, State::Repair),
    (State::Repair, Event::RepairAvailable, State::Infer),
    (State::Repair, Event::RepairExhausted, State::Degraded),
    (State::Authorize, Event::Authorized, State::Observe),
    (State::Authorize, Event::Denied, State::Degraded),
    (State::AwaitInput, Event::InputArrived, State::Account),
    (State::Observe, Event::ObserveOkMutating, State::Verify),
    (State::Observe, Event::ObserveDone, State::Account),
    (State::Observe, Event::TransientRetry, State::Observe),
    (State::Observe, Event::TransientExhausted, State::Degraded),
    (State::Verify, Event::VerifyPass, State::Account),
    (State::Verify, Event::VerifyFail, State::Account),
    (State::Account, Event::GuardsClear, State::Gather),
    (State::Account, Event::GuardFired, State::Degraded),
    (State::Finalize, Event::FinalizeCommitted, State::End),
    (State::Degraded, Event::DegradedCommitted, State::End),
    (State::SafeStop, Event::SafeStopCommitted, State::End),
];

fn expected_to(from: State, event: Event) -> Option<State> {
    EXPECTED
        .iter()
        .find(|(f, e, _)| *f == from && *e == event)
        .map(|(_, _, t)| *t)
}

#[test]
fn every_legal_triple_transitions_to_the_tabled_state() {
    for (from, event, to) in EXPECTED {
        assert_eq!(
            transition(from, event),
            Ok(to),
            "transition({from}, {event}) should reach {to}"
        );
        assert!(is_legal(from, to), "is_legal({from}, {to}) should hold");
    }
}

#[test]
fn table_covers_all_states_and_events() {
    assert_eq!(ALL_STATES.len(), 13, "13 workflow states");
    assert_eq!(ALL_EVENTS.len(), 25, "25 machine events");
    // Every state appears as a `from` except End; every event appears.
    for state in ALL_STATES {
        if state != State::End {
            assert!(
                EXPECTED.iter().any(|(f, _, _)| *f == state),
                "{state} has no outgoing edge"
            );
        }
    }
    for event in ALL_EVENTS {
        assert!(
            EXPECTED.iter().any(|(_, e, _)| *e == event),
            "{event} appears in no row"
        );
    }
}

#[test]
fn transition_is_total_and_rejects_every_unlisted_pair() {
    let mut legal = 0;
    for from in ALL_STATES {
        for event in ALL_EVENTS {
            if let Some(to) = expected_to(from, event) {
                legal += 1;
                assert_eq!(transition(from, event), Ok(to));
            } else {
                assert_eq!(
                    transition(from, event),
                    Err(Error::IllegalTransition { from, event }),
                    "({from}, {event}) must be rejected with a typed error"
                );
                assert!(
                    !is_legal_pair_in_expected(from, event),
                    "is_legal consistency"
                );
            }
        }
    }
    assert_eq!(legal, 25, "exactly the 25 tabled triples are legal");
}

fn is_legal_pair_in_expected(from: State, event: Event) -> bool {
    // `is_legal` is over (from, to); find whether any tabled `to` exists
    // for this (from, event) — there is none by construction here.
    EXPECTED
        .iter()
        .any(|(f, e, t)| *f == from && *e == event && is_legal(from, *t))
}

#[test]
fn is_legal_agrees_with_transition_everywhere() {
    for from in ALL_STATES {
        for to in ALL_STATES {
            let via_table = EXPECTED.iter().any(|(f, _, t)| *f == from && *t == to);
            assert_eq!(is_legal(from, to), via_table, "is_legal({from}, {to})");
            let via_events = ALL_EVENTS.iter().any(|e| transition(from, *e) == Ok(to));
            assert_eq!(via_table, via_events, "({from}, {to}) consistency");
        }
    }
}

#[test]
fn end_has_no_outgoing_transitions() {
    for event in ALL_EVENTS {
        assert!(
            transition(State::End, event).is_err(),
            "End must accept no event, not even {event}"
        );
        assert!(!is_legal(State::End, State::End));
    }
    assert!(State::End.is_terminal());
    for state in ALL_STATES {
        assert_eq!(state.is_terminal(), state == State::End);
    }
}

#[test]
fn pre_terminal_states_only_reach_end() {
    for state in [State::Finalize, State::Degraded, State::SafeStop] {
        assert!(state.is_pre_terminal(), "{state} is pre-terminal");
        for to in ALL_STATES {
            assert_eq!(
                is_legal(state, to),
                to == State::End,
                "{state} must only reach End"
            );
        }
    }
    for state in ALL_STATES {
        if !matches!(state, State::Finalize | State::Degraded | State::SafeStop) {
            assert!(!state.is_pre_terminal(), "{state} is not pre-terminal");
        }
    }
}

#[test]
fn recover_only_reaches_gather_or_safestop() {
    for to in ALL_STATES {
        let legal = to == State::Gather || to == State::SafeStop;
        assert_eq!(is_legal(State::Recover, to), legal, "Recover -> {to}");
    }
}

// --- §3.3 named hazards: each is a real safety hazard, so each gets a
// named test even though the exhaustive test covers them. ---

#[test]
fn hazard_infer_cannot_skip_authorization() {
    // The cardinal bypass: dispatching a tool without committed intent.
    for event in ALL_EVENTS {
        assert_ne!(
            transition(State::Infer, event),
            Ok(State::Observe),
            "Infer must never reach Observe on {event}"
        );
    }
    assert!(!is_legal(State::Infer, State::Observe));
}

#[test]
fn hazard_no_backward_edges_into_dispatch() {
    // Observe/Verify must not loop back to Infer/Authorize (double dispatch).
    for from in [State::Observe, State::Verify] {
        for to in [State::Infer, State::Authorize] {
            assert!(!is_legal(from, to), "{from} -> {to} is a backward edge");
        }
    }
    assert!(!is_legal(State::Verify, State::Observe));
}

#[test]
fn hazard_account_must_complete_before_next_turn() {
    for to in [
        State::Authorize,
        State::Observe,
        State::Verify,
        State::Infer,
    ] {
        assert!(!is_legal(State::Account, to), "Account -> {to}");
    }
}

#[test]
fn hazard_gather_cannot_reach_dispatch_states() {
    for to in [State::Observe, State::Verify, State::Account] {
        assert!(!is_legal(State::Gather, to), "Gather -> {to}");
    }
}

#[test]
fn hazard_repair_cannot_reach_dispatch_or_terminal() {
    for to in [State::Observe, State::Authorize, State::Finalize] {
        assert!(!is_legal(State::Repair, to), "Repair -> {to}");
    }
}

#[test]
fn hazard_await_input_exits_only_through_account() {
    // Suspension is durable: input must be committed before progress.
    for to in ALL_STATES {
        assert_eq!(
            is_legal(State::AwaitInput, to),
            to == State::Account,
            "AwaitInput -> {to}"
        );
    }
    assert!(!is_legal(State::AwaitInput, State::Gather));
    assert!(!is_legal(State::AwaitInput, State::Infer));
    assert!(!is_legal(State::AwaitInput, State::Observe));
}

#[test]
fn authorize_has_no_revalidation_loops() {
    assert!(!is_legal(State::Authorize, State::Infer));
    assert!(!is_legal(State::Authorize, State::Gather));
}

#[test]
fn committed_records_match_the_table() {
    use RecordKind as K;
    let cases: [(State, Event, &[RecordKind]); 25] = [
        (State::Recover, Event::JournalValid, &[]),
        (State::Recover, Event::JournalCorrupt, &[K::TerminalResult]),
        (State::Gather, Event::BudgetsClear, &[K::ModelRequest]),
        (State::Gather, Event::BudgetExhausted, &[K::TerminalResult]),
        (
            State::Gather,
            Event::IncompatibleVersion,
            &[K::TerminalResult],
        ),
        (State::Infer, Event::DecodeCall, &[K::ModelDecision]),
        (
            State::Infer,
            Event::DecodeAsk,
            &[K::ModelDecision, K::ApprovalRequest],
        ),
        (State::Infer, Event::DecodeFinish, &[K::ModelDecision]),
        (State::Infer, Event::DecodeMalformed, &[K::ModelDecision]),
        (State::Repair, Event::RepairAvailable, &[]),
        (State::Repair, Event::RepairExhausted, &[K::TerminalResult]),
        (State::Authorize, Event::Authorized, &[K::ToolRequest]),
        (State::Authorize, Event::Denied, &[K::TerminalResult]),
        (
            State::AwaitInput,
            Event::InputArrived,
            &[K::ApprovalDecision],
        ),
        (
            State::Observe,
            Event::ObserveOkMutating,
            &[K::ToolObservation],
        ),
        (State::Observe, Event::ObserveDone, &[K::ToolObservation]),
        (State::Observe, Event::TransientRetry, &[K::ToolObservation]),
        (
            State::Observe,
            Event::TransientExhausted,
            &[K::ToolObservation, K::TerminalResult],
        ),
        (State::Verify, Event::VerifyPass, &[K::VerificationResult]),
        (State::Verify, Event::VerifyFail, &[K::VerificationResult]),
        (State::Account, Event::GuardsClear, &[]),
        (State::Account, Event::GuardFired, &[K::TerminalResult]),
        (
            State::Finalize,
            Event::FinalizeCommitted,
            &[K::TerminalResult],
        ),
        (
            State::Degraded,
            Event::DegradedCommitted,
            &[K::TerminalResult],
        ),
        (
            State::SafeStop,
            Event::SafeStopCommitted,
            &[K::TerminalResult],
        ),
    ];
    for (from, event, kinds) in cases {
        assert_eq!(
            committed_records(from, event),
            kinds,
            "committed_records({from}, {event})"
        );
    }
}

#[test]
fn terminal_status_codes_are_stable() {
    let codes = [
        (TerminalStatus::Completed, 0),
        (TerminalStatus::NeedsInput, 1),
        (TerminalStatus::Denied, 2),
        (TerminalStatus::BudgetExhausted, 3),
        (TerminalStatus::Stuck, 4),
        (TerminalStatus::ToolUnavailable, 5),
        (TerminalStatus::ModelInvalid, 6),
        (TerminalStatus::StorageFault, 7),
        (TerminalStatus::Incompatible, 8),
    ];
    for (status, code) in codes {
        assert_eq!(status.code(), code);
        assert_eq!(TerminalStatus::from_code(code), Ok(status));
    }
    assert!(TerminalStatus::from_code(9).is_err());
    assert!(TerminalStatus::from_code(255).is_err());
}

#[test]
fn only_completed_claims_success() {
    for code in 0..=8u8 {
        let status = TerminalStatus::from_code(code).expect("valid code");
        assert_eq!(status.is_success(), code == 0, "{status}");
    }
}
