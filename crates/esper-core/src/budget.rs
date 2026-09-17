//! The resource budget model (§7).
//!
//! The budget is part of run identity: committed in the `RunSeed`, it
//! can never widen after reboot (ADR 10). All fields are *remaining*
//! amounts; the `consume_*` family (for example
//! [`ResourceBudget::consume_turn`]) decrements them and fails with
//! [`Error::BudgetExhausted`] naming the exhausted unit. [`ResourceBudget::meet`]
//! is the field-wise minimum the crash oracle uses to prove the
//! never-widen rule; [`ResourceBudget::check_no_widen`] fails closed when
//! a restored budget would grant more than the seed.
//!
//! `consecutive_errors` is a plain counter: it increments (saturating) on
//! any committed error and resets on any committed success. It carries no
//! exhaustion guard; the monitor's rules consume committed event history.

use core::fmt::{Display, Formatter, Result as FmtResult};

use crate::error::Error;

/// Budget units with hard guards (§7.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BudgetUnit {
    /// Inference activities, repairs included.
    ModelTurns,
    /// Context bytes assembled per turn.
    InputTokens,
    /// Model output bytes per turn.
    OutputTokens,
    /// Host monotonic milliseconds (no cross-reboot claims, §7.3).
    ElapsedMs,
    /// Slice: always 0; the field exists for E5.
    RadioBytes,
    /// Committed mutating tool intents.
    Mutations,
}

impl BudgetUnit {
    /// The unit's name (used in `BudgetExhausted` reasons).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ModelTurns => "model_turns",
            Self::InputTokens => "input_tokens",
            Self::OutputTokens => "output_tokens",
            Self::ElapsedMs => "elapsed_ms",
            Self::RadioBytes => "radio_bytes",
            Self::Mutations => "mutations",
        }
    }
}

impl Display for BudgetUnit {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        f.write_str(self.name())
    }
}

/// Remaining resource amounts for one run (§7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceBudget {
    /// Inference activities remaining, repairs included.
    pub model_turns: u16,
    /// Context bytes remaining.
    pub input_tokens: u32,
    /// Model output bytes remaining.
    pub output_tokens: u32,
    /// Milliseconds remaining on the host monotonic clock.
    pub elapsed_ms: u64,
    /// Radio bytes remaining (slice: always 0).
    pub radio_bytes: u32,
    /// Mutating tool intents remaining.
    pub mutations: u16,
    /// Consecutive committed errors; resets on any committed success.
    pub consecutive_errors: u8,
}

impl ResourceBudget {
    /// Build a budget from explicit deployment values (§7.2: never hidden
    /// constants).
    #[must_use]
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        model_turns: u16,
        input_tokens: u32,
        output_tokens: u32,
        elapsed_ms: u64,
        radio_bytes: u32,
        mutations: u16,
        consecutive_errors: u8,
    ) -> Self {
        Self {
            model_turns,
            input_tokens,
            output_tokens,
            elapsed_ms,
            radio_bytes,
            mutations,
            consecutive_errors,
        }
    }

    /// Consume one model turn (every committed `ModelDecision`, repairs
    /// included).
    ///
    /// # Errors
    ///
    /// Returns [`Error::BudgetExhausted`] naming `model_turns` when none
    /// remain.
    pub fn consume_turn(&mut self) -> Result<(), Error> {
        self.model_turns = self
            .model_turns
            .checked_sub(1)
            .ok_or(Error::BudgetExhausted(BudgetUnit::ModelTurns))?;
        Ok(())
    }

    /// Consume one mutating tool intent.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BudgetExhausted`] naming `mutations` when none
    /// remain.
    pub fn consume_mutation(&mut self) -> Result<(), Error> {
        self.mutations = self
            .mutations
            .checked_sub(1)
            .ok_or(Error::BudgetExhausted(BudgetUnit::Mutations))?;
        Ok(())
    }

    /// Consume `n` input token bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BudgetExhausted`] naming `input_tokens` when fewer
    /// than `n` remain.
    pub fn consume_input_tokens(&mut self, n: u32) -> Result<(), Error> {
        self.input_tokens = self
            .input_tokens
            .checked_sub(n)
            .ok_or(Error::BudgetExhausted(BudgetUnit::InputTokens))?;
        Ok(())
    }

    /// Consume `n` output token bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BudgetExhausted`] naming `output_tokens` when
    /// fewer than `n` remain.
    pub fn consume_output_tokens(&mut self, n: u32) -> Result<(), Error> {
        self.output_tokens = self
            .output_tokens
            .checked_sub(n)
            .ok_or(Error::BudgetExhausted(BudgetUnit::OutputTokens))?;
        Ok(())
    }

    /// Consume `n` radio bytes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BudgetExhausted`] naming `radio_bytes` when fewer
    /// than `n` remain.
    pub fn consume_radio_bytes(&mut self, n: u32) -> Result<(), Error> {
        self.radio_bytes = self
            .radio_bytes
            .checked_sub(n)
            .ok_or(Error::BudgetExhausted(BudgetUnit::RadioBytes))?;
        Ok(())
    }

    /// Consume `n` milliseconds of host monotonic time.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BudgetExhausted`] naming `elapsed_ms` when fewer
    /// than `n` remain.
    pub fn consume_elapsed_ms(&mut self, n: u64) -> Result<(), Error> {
        self.elapsed_ms = self
            .elapsed_ms
            .checked_sub(n)
            .ok_or(Error::BudgetExhausted(BudgetUnit::ElapsedMs))?;
        Ok(())
    }

    /// Record a committed error: the consecutive-error counter increments,
    /// saturating instead of wrapping.
    pub const fn record_error(&mut self) {
        self.consecutive_errors = self.consecutive_errors.saturating_add(1);
    }

    /// Record a committed success: the consecutive-error counter resets.
    pub const fn record_success(&mut self) {
        self.consecutive_errors = 0;
    }

    /// The first exhausted unit in deterministic order, or `None` when
    /// every guard still holds.
    #[must_use]
    /// The first exhausted unit in deterministic order, if any.
    ///
    /// `radio_bytes` is intentionally not a rest-state exhaustion source:
    /// §7 grants it as 0 and the slice always uses 0, so a zero grant
    /// means "no radio allowed", not "radio exhausted". Over-charging it
    /// fails at [`Self::consume_radio_bytes`] with
    /// [`Error::BudgetExhausted`].
    pub const fn exhausted_unit(&self) -> Option<BudgetUnit> {
        if self.model_turns == 0 {
            return Some(BudgetUnit::ModelTurns);
        }
        if self.input_tokens == 0 {
            return Some(BudgetUnit::InputTokens);
        }
        if self.output_tokens == 0 {
            return Some(BudgetUnit::OutputTokens);
        }
        if self.elapsed_ms == 0 {
            return Some(BudgetUnit::ElapsedMs);
        }
        if self.mutations == 0 {
            return Some(BudgetUnit::Mutations);
        }
        None
    }

    /// Field-wise minimum of two budgets: the never-widen primitive.
    ///
    /// After reboot the runtime restores budgets with `meet`, so the
    /// restored budget can only stay equal or shrink relative to both
    /// inputs — never widen.
    #[must_use]
    pub const fn meet(self, other: Self) -> Self {
        Self {
            model_turns: min_u16(self.model_turns, other.model_turns),
            input_tokens: min_u32(self.input_tokens, other.input_tokens),
            output_tokens: min_u32(self.output_tokens, other.output_tokens),
            elapsed_ms: min_u64(self.elapsed_ms, other.elapsed_ms),
            radio_bytes: min_u32(self.radio_bytes, other.radio_bytes),
            mutations: min_u16(self.mutations, other.mutations),
            consecutive_errors: min_u8(self.consecutive_errors, other.consecutive_errors),
        }
    }

    /// Fail closed when `self` grants more than `identity` in any unit.
    ///
    /// # Errors
    ///
    /// Returns [`Error::BudgetWidened`] naming the first widened unit.
    pub const fn check_no_widen(&self, identity: &Self) -> Result<(), Error> {
        // Every guarded unit must not exceed the identity's. The
        // consecutive-error counter is accounting, not a grant, so it is
        // not part of the widen check.
        if self.model_turns > identity.model_turns {
            return Err(Error::BudgetWidened(BudgetUnit::ModelTurns));
        }
        if self.input_tokens > identity.input_tokens {
            return Err(Error::BudgetWidened(BudgetUnit::InputTokens));
        }
        if self.output_tokens > identity.output_tokens {
            return Err(Error::BudgetWidened(BudgetUnit::OutputTokens));
        }
        if self.elapsed_ms > identity.elapsed_ms {
            return Err(Error::BudgetWidened(BudgetUnit::ElapsedMs));
        }
        if self.radio_bytes > identity.radio_bytes {
            return Err(Error::BudgetWidened(BudgetUnit::RadioBytes));
        }
        if self.mutations > identity.mutations {
            return Err(Error::BudgetWidened(BudgetUnit::Mutations));
        }
        Ok(())
    }
}

/// `const`-compatible minimums (`core::cmp::min` is not const on all
/// toolchain versions this crate supports).
const fn min_u16(a: u16, b: u16) -> u16 {
    if a < b {
        a
    } else {
        b
    }
}

const fn min_u32(a: u32, b: u32) -> u32 {
    if a < b {
        a
    } else {
        b
    }
}

const fn min_u64(a: u64, b: u64) -> u64 {
    if a < b {
        a
    } else {
        b
    }
}

const fn min_u8(a: u8, b: u8) -> u8 {
    if a < b {
        a
    } else {
        b
    }
}
