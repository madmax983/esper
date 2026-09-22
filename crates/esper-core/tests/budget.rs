//! Budget arithmetic and the never-widen identity rule (§7).

use esper_core::Error;
use esper_core::budget::{BudgetUnit, ResourceBudget};

const fn full() -> ResourceBudget {
    ResourceBudget::new(10, 4000, 1000, 60_000, 0, 4, 0)
}

#[test]
fn consume_turn_decrements_and_fails_at_zero() {
    let mut budget = full();
    for remaining in (0..10u16).rev() {
        budget.consume_turn().expect("turn available");
        assert_eq!(budget.model_turns, remaining);
    }
    assert_eq!(
        budget.consume_turn(),
        Err(Error::BudgetExhausted(BudgetUnit::ModelTurns))
    );
    // Failed consumption leaves the counter at zero, never negative.
    assert_eq!(budget.model_turns, 0);
}

#[test]
fn consume_mutation_decrements_and_fails_at_zero() {
    let mut budget = full();
    for _ in 0..4 {
        budget.consume_mutation().expect("mutation available");
    }
    assert_eq!(budget.mutations, 0);
    assert_eq!(
        budget.consume_mutation(),
        Err(Error::BudgetExhausted(BudgetUnit::Mutations))
    );
}

#[test]
fn token_consumption_is_exact() {
    let mut budget = full();
    budget.consume_input_tokens(1500).expect("tokens");
    assert_eq!(budget.input_tokens, 2500);
    assert_eq!(
        budget.consume_input_tokens(2501),
        Err(Error::BudgetExhausted(BudgetUnit::InputTokens))
    );
    assert_eq!(budget.input_tokens, 2500, "failed charge changes nothing");

    budget.consume_output_tokens(1000).expect("exact fit");
    assert_eq!(budget.output_tokens, 0);
    assert_eq!(
        budget.consume_output_tokens(1),
        Err(Error::BudgetExhausted(BudgetUnit::OutputTokens))
    );
}

#[test]
fn exhausted_unit_names_the_first_empty_unit() {
    let mut budget = full();
    assert_eq!(budget.exhausted_unit(), None);

    budget.model_turns = 0;
    assert_eq!(budget.exhausted_unit(), Some(BudgetUnit::ModelTurns));

    // Deterministic order: model_turns wins over later units.
    budget.mutations = 0;
    assert_eq!(budget.exhausted_unit(), Some(BudgetUnit::ModelTurns));

    let mut budget = full();
    budget.elapsed_ms = 0;
    assert_eq!(budget.exhausted_unit(), Some(BudgetUnit::ElapsedMs));
}

#[test]
fn consecutive_error_counter_saturates_and_resets() {
    let mut budget = full();
    budget.record_error();
    budget.record_error();
    assert_eq!(budget.consecutive_errors, 2);
    budget.record_success();
    assert_eq!(budget.consecutive_errors, 0);

    budget.consecutive_errors = u8::MAX;
    budget.record_error();
    assert_eq!(budget.consecutive_errors, u8::MAX, "saturates, never wraps");
}

#[test]
fn meet_takes_the_fieldwise_minimum() {
    let a = ResourceBudget::new(10, 4000, 1000, 60_000, 0, 4, 1);
    let b = ResourceBudget::new(7, 5000, 800, 30_000, 0, 2, 3);
    let m = a.meet(b);
    assert_eq!(m.model_turns, 7);
    assert_eq!(m.input_tokens, 4000);
    assert_eq!(m.output_tokens, 800);
    assert_eq!(m.elapsed_ms, 30_000);
    assert_eq!(m.mutations, 2);
    assert_eq!(m.consecutive_errors, 1);
    // Symmetric: order does not matter.
    assert_eq!(a.meet(b), b.meet(a));
}

#[test]
fn meet_never_widens_either_input() {
    let a = ResourceBudget::new(10, 4000, 1000, 60_000, 0, 4, 1);
    let b = ResourceBudget::new(7, 5000, 800, 30_000, 0, 2, 3);
    let m = a.meet(b);
    assert!(m.check_no_widen(&a).is_ok());
    assert!(m.check_no_widen(&b).is_ok());
}

#[test]
fn check_no_widen_fails_closed() {
    let seed = full();
    let same = full();
    assert!(same.check_no_widen(&seed).is_ok());

    let mut restored = full();
    restored.model_turns = 11;
    assert_eq!(
        restored.check_no_widen(&seed),
        Err(Error::BudgetWidened(BudgetUnit::ModelTurns))
    );

    // A shrunk budget is fine: identity only forbids widening.
    let mut shrunk = full();
    shrunk.model_turns = 3;
    shrunk.mutations = 0;
    assert!(shrunk.check_no_widen(&seed).is_ok());
}

#[test]
fn budget_units_have_stable_names() {
    assert_eq!(BudgetUnit::ModelTurns.name(), "model_turns");
    assert_eq!(BudgetUnit::Mutations.name(), "mutations");
    assert_eq!(BudgetUnit::ElapsedMs.name(), "elapsed_ms");
}
