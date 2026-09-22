//! Run lineage for `continue_as_new` (SPEC §20.4).
//!
//! When context or journal pressure forces a history rollover, the
//! runtime persists the [`CompactState`]
//! through an idempotent snapshot activity, verifies it by reading it
//! back, and begins a new Waymaker run. [`Lineage`] is the new run's
//! link to its parent: which run it continues, at which frame, with
//! which remaining budgets, and under which versions.
//!
//! Budgets are carried as *remaining* and never widened (ADR 10):
//! [`continue_as_new`] fails closed with [`Error::BudgetWidened`]
//! when the lineage's remaining budgets exceed what the compacted
//! state holds. Version compatibility is *not* gated here — the
//! runtime's `RunSeed` validation owns the version gate (design §4);
//! the lineage carries the versions so the new seed can state them.

use crate::budget::ResourceBudget;
use crate::compact::{CompactState, VersionSet};
use crate::error::Error;
use crate::ids::RunId;

/// The new run's link to the run it continues.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lineage {
    /// The run being continued.
    pub parent_run: RunId,
    /// The parent frame the continuation was cut at.
    pub continued_at_frame: u32,
    /// Budgets remaining for the new run: carried, never widened.
    pub budgets_remaining: ResourceBudget,
    /// Version bindings the new run's seed states.
    pub versions: VersionSet,
}

/// The new run's initial state: the carried compact state plus its
/// lineage. The runtime writes the lineage into the new `RunSeed`
/// and starts the workflow from `state`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContinuedRun {
    /// The compact state the new run starts from.
    pub state: CompactState,
    /// The new run's link to its parent.
    pub lineage: Lineage,
}

/// Begin a new run from a compacted state and its lineage.
///
/// The new run inherits the compact state with its budgets replaced
/// by the lineage's remaining budgets. Fingerprints, failed paths,
/// obligations, facts, and versions ride along untouched: the new run
/// continues the same task, so its loop detector keeps recent
/// history.
///
/// # Errors
///
/// Returns [`Error::BudgetWidened`] when `lineage.budgets_remaining`
/// grants more than `state.budgets` in any unit — the continuation
/// must never widen the run's budget identity (ADR 10).
pub const fn continue_as_new(
    state: &CompactState,
    lineage: &Lineage,
) -> Result<ContinuedRun, Error> {
    match lineage.budgets_remaining.check_no_widen(&state.budgets) {
        Ok(()) => {}
        Err(err) => return Err(err),
    }
    let mut next = *state;
    next.budgets = lineage.budgets_remaining;
    Ok(ContinuedRun {
        state: next,
        lineage: *lineage,
    })
}
