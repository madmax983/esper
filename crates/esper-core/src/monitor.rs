//! The deterministic runtime monitor: progress signal and guard rules.
//!
//! [`ProgressDelta`] is emitted per committed step from the trajectory,
//! never from model prose. [`Monitor`] owns the `Account → Degraded`
//! edge: it consumes committed step records plus the budget and applies
//! the §8.3 rules. It never inspects model prose.
//!
//! Initial rules (tuning defaults; changeable only against trajectory
//! data):
//!
//! - three consecutive identical `(tool_id, args_digest, error_class)`
//!   failures → `Stuck` (this is what ends a run after repeated
//!   [`ErrorCode::VerificationFailed`]
//!   observations — there is no auto-compensation, §9.2);
//! - at least three errors in a full five-event window → `Stuck`;
//! - five consecutive [`ProgressDelta::NoProgress`] deltas → `Stuck`;
//! - any hard budget limit reached → `BudgetExhausted` naming the unit;
//! - a denied permission attempt is terminal `Denied` immediately via
//!   `Authorize`, before the monitor ever sees it.

use crate::budget::ResourceBudget;
use crate::error::ErrorCode;
use crate::ids::{Digest, ToolId};
use crate::state::TerminalStatus;

/// Progress emitted per committed step (§8.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ProgressDelta {
    /// A read returned a new fact or state value.
    NewEvidence,
    /// A verified mutation changed the world as intended.
    StateChanged,
    /// Reserved; never emitted in the slice (E4).
    SubgoalClosed,
    /// Requested human input arrived and committed.
    InputReceived,
    /// None of the above.
    NoProgress,
}

/// The outcome of one committed step, as the monitor sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StepOutcome {
    /// The step succeeded.
    Ok,
    /// The step failed with this §6 error class.
    Failed(ErrorCode),
}

impl StepOutcome {
    /// Whether the outcome is a failure of any class.
    #[must_use]
    pub const fn is_error(self) -> bool {
        matches!(self, Self::Failed(_))
    }
}

/// One committed step, in the monitor's vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StepRecord {
    /// The tool involved (or the tool being verified).
    pub tool: ToolId,
    /// The digest of the step's arguments.
    pub args_digest: Digest,
    /// Success or the §6 error class.
    pub outcome: StepOutcome,
    /// Progress derived from the committed trajectory.
    pub progress: ProgressDelta,
}

impl StepRecord {
    /// Build a successful step record.
    #[must_use]
    pub const fn ok(tool: ToolId, args_digest: Digest, progress: ProgressDelta) -> Self {
        Self {
            tool,
            args_digest,
            outcome: StepOutcome::Ok,
            progress,
        }
    }

    /// Build a failed step record.
    #[must_use]
    pub const fn failed(
        tool: ToolId,
        args_digest: Digest,
        error: ErrorCode,
        progress: ProgressDelta,
    ) -> Self {
        Self {
            tool,
            args_digest,
            outcome: StepOutcome::Failed(error),
            progress,
        }
    }
}

/// Number of recent events the majority rule examines.
pub const MONITOR_WINDOW: usize = 5;

/// Consecutive identical failures that end a run `Stuck`.
pub const IDENTICAL_FAILURE_LIMIT: u8 = 3;

/// Consecutive `NoProgress` deltas that end a run `Stuck`.
pub const NO_PROGRESS_LIMIT: u8 = 5;

/// Minimum errors in a full window for the majority rule.
pub const MAJORITY_ERROR_LIMIT: u8 = 3;

/// The monitor's verdict on one observed step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MonitorVerdict {
    /// The run continues.
    Continue,
    /// The run degrades with this terminal status and reason.
    Degrade {
        /// The terminal status to commit.
        status: TerminalStatus,
        /// Plain-words reason (names the guard or unit).
        reason: &'static str,
    },
}

/// The deterministic runtime monitor.
///
/// The monitor keeps a bounded ring of the last [`MONITOR_WINDOW`]
/// committed step records plus a `NoProgress` streak counter. It owns the
/// `Account → Degraded` edge: [`Monitor::observe`] applies every §8.3
/// rule, budget guards included.
#[derive(Debug, Clone, Copy)]
pub struct Monitor {
    window: [Option<StepRecord>; MONITOR_WINDOW],
    next: usize,
    recorded: u8,
    no_progress_streak: u8,
}

impl Default for Monitor {
    /// A fresh monitor with no history.
    fn default() -> Self {
        Self::new()
    }
}

impl Monitor {
    /// A fresh monitor with no history.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            window: [None; MONITOR_WINDOW],
            next: 0,
            recorded: 0,
            no_progress_streak: 0,
        }
    }

    /// Record one committed step and return the verdict.
    ///
    /// The budget guard runs first (§3.3: resource exhaustion must
    /// surface even when other stop conditions are also present). After
    /// recording, the trip rules fire in §3.3 order: five consecutive
    /// `NoProgress` deltas, three consecutive identical failure tuples,
    /// then the five-window three-of-five error majority.
    ///
    /// The `NoProgress` streak counts consecutive steps with
    /// [`ProgressDelta::NoProgress`], whatever their outcome: a failure
    /// that made no progress is still no progress. Any other progress
    /// variant resets the streak.
    pub fn observe(&mut self, record: StepRecord, budget: &ResourceBudget) -> MonitorVerdict {
        self.push(record);

        if let Some(unit) = budget.exhausted_unit() {
            return MonitorVerdict::Degrade {
                status: TerminalStatus::BudgetExhausted,
                reason: unit.name(),
            };
        }
        if self.no_progress_streak >= NO_PROGRESS_LIMIT {
            return MonitorVerdict::Degrade {
                status: TerminalStatus::Stuck,
                reason: "five consecutive NoProgress deltas",
            };
        }
        if self.consecutive_identical_failures() >= usize::from(IDENTICAL_FAILURE_LIMIT) {
            return MonitorVerdict::Degrade {
                status: TerminalStatus::Stuck,
                reason: "three identical (tool, args, error) failures",
            };
        }
        if self.error_majority_in_window() {
            return MonitorVerdict::Degrade {
                status: TerminalStatus::Stuck,
                reason: "error majority in the last five events",
            };
        }
        MonitorVerdict::Continue
    }

    /// The current consecutive `NoProgress` streak (any outcome).
    #[must_use]
    pub const fn no_progress_streak(&self) -> u8 {
        self.no_progress_streak
    }
}

impl Monitor {
    /// Record a step into the bounded ring and maintain the
    /// successful-`NoProgress` streak.
    fn push(&mut self, record: StepRecord) {
        self.window[self.next] = Some(record);
        self.next = (self.next + 1) % MONITOR_WINDOW;
        self.recorded = self.recorded.saturating_add(1);
        // Any progress variant except `NoProgress` resets the streak;
        // failures carrying `NoProgress` count just like successes do.
        if record.progress == ProgressDelta::NoProgress {
            self.no_progress_streak = self.no_progress_streak.saturating_add(1);
        } else {
            self.no_progress_streak = 0;
        }
    }

    /// Steps in the window, oldest first.
    fn ordered(&self) -> [Option<StepRecord>; MONITOR_WINDOW] {
        let mut out: [Option<StepRecord>; MONITOR_WINDOW] = [None; MONITOR_WINDOW];
        let len = self.len();
        for (i, slot) in out.iter_mut().enumerate().take(len) {
            let idx = (self.next + MONITOR_WINDOW - len + i) % MONITOR_WINDOW;
            *slot = self.window[idx];
        }
        out
    }

    /// Number of valid entries (at most [`MONITOR_WINDOW`]).
    fn len(&self) -> usize {
        usize::from(self.recorded).min(MONITOR_WINDOW)
    }

    /// Length of the trailing run of identical `(tool, digest, error)`
    /// failure tuples. A success or a differing tuple breaks the run
    /// (§3.3).
    fn consecutive_identical_failures(&self) -> usize {
        let ordered = self.ordered();
        let mut run = 0;
        let mut last: Option<(ToolId, Digest, ErrorCode)> = None;
        for record in ordered.iter().take(self.len()).rev() {
            let tuple = match record {
                Some(step) => match step.outcome {
                    StepOutcome::Failed(error) => (step.tool, step.args_digest, error),
                    StepOutcome::Ok => break,
                },
                None => break,
            };
            if last.is_none_or(|prev| prev == tuple) {
                last = Some(tuple);
                run += 1;
            } else {
                break;
            }
        }
        run
    }

    /// True when the window is full and at least three of the five
    /// entries are tool failures (§3.3).
    fn error_majority_in_window(&self) -> bool {
        if self.len() < MONITOR_WINDOW {
            return false;
        }
        let errors = self
            .window
            .iter()
            .filter(|record| {
                matches!(
                    record,
                    Some(StepRecord {
                        outcome: StepOutcome::Failed(_),
                        ..
                    })
                )
            })
            .count();
        errors >= usize::from(MAJORITY_ERROR_LIMIT)
    }
}
