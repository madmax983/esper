//! The host run trace: the eval harness's evidence.
//!
//! The trace is derived by replaying the [`Journal`];
//! it is not durable state itself. Every assertion the golden
//! trajectories make — terminal status, reason, summary, remaining
//! budgets, request and observation counts, suspension — reads from
//! here.
//!
//! // HOST-ONLY (E0/E1): the trace is heap-allocated and for the host
//! eval harness. Firmware builds do not carry it.

use esper_core::state::TerminalStatus;

use crate::error::RuntimeError;
use crate::journal::{DecisionClass, Frame, Journal};
use crate::seed::RunSeed;

/// One trace event per durable boundary, in journal order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceEvent {
    /// A model line was committed.
    ModelDecision {
        /// The decision class.
        class: DecisionClass,
        /// The exact bytes the model emitted.
        // HOST-ONLY (E0/E1)
        line: Vec<u8>,
    },
    /// A tool intent was committed.
    ToolRequest {
        /// The stable effect sequence.
        seq: u32,
        /// The tool's numeric id.
        tool: u8,
        /// The validated argument bytes.
        // HOST-ONLY (E0/E1)
        args: Vec<u8>,
    },
    /// A tool outcome was committed.
    ToolObservation {
        /// The effect sequence this outcome belongs to.
        seq: u32,
        /// Whether the dispatch succeeded.
        ok: bool,
        /// The result bytes (empty for a transient failure).
        // HOST-ONLY (E0/E1)
        payload: Vec<u8>,
    },
    /// An independent read-back was committed.
    Verification {
        /// The effect sequence that was verified.
        seq: u32,
        /// Whether read-back matched the requested state.
        passed: bool,
        /// The expected state, as JSON: the requested pin level for a
        /// GPIO write, the target clock reading for a delay.
        // HOST-ONLY (E0/E1)
        expected: Vec<u8>,
        /// The observed state, as JSON: the pin's physical level for a
        /// GPIO write, the clock reading for a delay.
        // HOST-ONLY (E0/E1)
        observed: Vec<u8>,
    },
    /// The engine asked for typed human input.
    ApprovalRequest {
        /// The prompt, as the model emitted it.
        // HOST-ONLY (E0/E1)
        prompt: Vec<u8>,
        /// The response schema id.
        schema: u8,
    },
    /// Typed human input arrived and committed.
    ApprovalDecision {
        /// The input bytes, as delivered.
        // HOST-ONLY (E0/E1)
        input: Vec<u8>,
    },
    /// The run's one logical terminal result.
    Terminal {
        /// The terminal status.
        status: TerminalStatus,
        /// The machine-readable reason.
        // HOST-ONLY (E0/E1)
        reason: Vec<u8>,
        /// The run summary.
        // HOST-ONLY (E0/E1)
        summary: Vec<u8>,
    },
}

/// The derived evidence for one run: events, terminal, budgets.
#[derive(Debug, Clone)]
pub struct RunTrace {
    /// The trace events, in journal order.
    // HOST-ONLY (E0/E1)
    events: Vec<TraceEvent>,
    /// The terminal status, if the run ended.
    terminal: Option<TerminalStatus>,
    /// The machine-readable terminal reason.
    // HOST-ONLY (E0/E1)
    reason: Option<Vec<u8>>,
    /// The run summary.
    // HOST-ONLY (E0/E1)
    summary: Option<Vec<u8>>,
    /// Model turns remaining.
    turns_remaining: u16,
    /// Mutations remaining.
    mutations_remaining: u16,
    /// Whether the run is durably suspended awaiting input.
    suspended: bool,
    /// The model bundle the run is bound to (E3): the seed's
    /// `model_bundle`, so the evidence names the exact model that
    /// produced it.
    model_bundle: u64,
    /// Measured input tokens across all committed decisions (E3).
    input_tokens_used: u32,
    /// Measured output tokens across all committed decisions (E3).
    output_tokens_used: u32,
}

impl RunTrace {
    /// Derive the trace by replaying the journal against the seed's
    /// starting budget.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::JournalCorrupt`] when the journal spends
    /// more than the seed granted, commits two terminals, or ends with
    /// a terminal that is not its last frame.
    pub fn from_journal(journal: &Journal, seed: &RunSeed) -> Result<Self, RuntimeError> {
        let mut acc = Accumulator::new(seed);
        for frame in journal.frames() {
            acc.apply(frame)?;
        }
        acc.finish(journal)
    }

    /// The trace events, in journal order.
    #[must_use]
    pub fn events(&self) -> &[TraceEvent] {
        &self.events
    }

    /// The terminal status, or `None` when the run has not ended.
    #[must_use]
    pub const fn terminal_status(&self) -> Option<TerminalStatus> {
        self.terminal
    }

    /// The machine-readable terminal reason.
    #[must_use]
    pub fn reason(&self) -> Option<&[u8]> {
        self.reason.as_deref()
    }

    /// The run summary.
    #[must_use]
    pub fn summary(&self) -> Option<&[u8]> {
        self.summary.as_deref()
    }

    /// Model turns remaining.
    #[must_use]
    pub const fn turns_remaining(&self) -> u16 {
        self.turns_remaining
    }

    /// Mutations remaining.
    #[must_use]
    pub const fn mutations_remaining(&self) -> u16 {
        self.mutations_remaining
    }

    /// Whether the run is durably suspended awaiting human input.
    #[must_use]
    pub const fn suspended(&self) -> bool {
        self.suspended
    }

    /// The model bundle the run is bound to (E3): the exact model
    /// that produced this trace.
    #[must_use]
    pub const fn model_bundle(&self) -> u64 {
        self.model_bundle
    }

    /// Measured input tokens across all committed model decisions.
    /// Recorded, never metered (SPEC §18).
    #[must_use]
    pub const fn input_tokens_used(&self) -> u32 {
        self.input_tokens_used
    }

    /// Measured output tokens across all committed model decisions.
    /// Recorded, never metered (SPEC §18).
    #[must_use]
    pub const fn output_tokens_used(&self) -> u32 {
        self.output_tokens_used
    }

    /// How many tool intents committed.
    #[must_use]
    pub fn tool_requests(&self) -> usize {
        // HOST-ONLY (E0/E1)
        self.events
            .iter()
            .filter(|event| matches!(event, TraceEvent::ToolRequest { .. }))
            .count()
    }

    /// How many tool outcomes committed.
    #[must_use]
    pub fn tool_observations(&self) -> usize {
        // HOST-ONLY (E0/E1)
        self.events
            .iter()
            .filter(|event| matches!(event, TraceEvent::ToolObservation { .. }))
            .count()
    }
}

/// The per-frame fold behind [`RunTrace::from_journal`].
struct Accumulator {
    /// The budget being rebuilt from the journal.
    budget: esper_core::ResourceBudget,
    /// The events in journal order.
    // HOST-ONLY (E0/E1)
    events: Vec<TraceEvent>,
    /// The committed terminal status, if any.
    terminal: Option<TerminalStatus>,
    /// The committed terminal reason, if any.
    // HOST-ONLY (E0/E1)
    reason: Option<Vec<u8>>,
    /// The committed run summary, if any.
    // HOST-ONLY (E0/E1)
    summary: Option<Vec<u8>>,
    /// The model bundle the run is bound to (E3).
    model_bundle: u64,
    /// Measured input tokens across all committed decisions (E3).
    input_tokens_used: u32,
    /// Measured output tokens across all committed decisions (E3).
    output_tokens_used: u32,
}

impl Accumulator {
    /// Start from the seed's grants.
    const fn new(seed: &RunSeed) -> Self {
        // HOST-ONLY (E0/E1)
        Self {
            budget: seed.starting_budget(),
            events: Vec::new(),
            terminal: None,
            reason: None,
            summary: None,
            model_bundle: seed.model_bundle,
            input_tokens_used: 0,
            output_tokens_used: 0,
        }
    }

    /// Fold one frame into the trace state.
    fn apply(&mut self, frame: &Frame) -> Result<(), RuntimeError> {
        match frame {
            Frame::RunStarted { .. } => Ok(()),
            Frame::ModelDecision {
                output,
                class,
                input_tokens,
                output_tokens,
                ..
            } => {
                self.budget
                    .consume_turn()
                    .map_err(|_| RuntimeError::JournalCorrupt)?;
                // Measured, never metered: the token units ride along
                // as evidence (SPEC §18).
                self.input_tokens_used = self.input_tokens_used.saturating_add(*input_tokens);
                self.output_tokens_used = self.output_tokens_used.saturating_add(*output_tokens);
                // HOST-ONLY (E0/E1)
                self.events.push(TraceEvent::ModelDecision {
                    class: *class,
                    line: output.clone(),
                });
                Ok(())
            }
            Frame::ToolIntent {
                seq,
                tool,
                args,
                write,
                ..
            } => {
                if *write {
                    self.budget
                        .consume_mutation()
                        .map_err(|_| RuntimeError::JournalCorrupt)?;
                }
                self.events.push(TraceEvent::ToolRequest {
                    seq: *seq,
                    tool: *tool,
                    args: args.clone(),
                });
                Ok(())
            }
            Frame::ToolObservation {
                seq,
                transient,
                outcome,
                ..
            } => {
                self.events.push(TraceEvent::ToolObservation {
                    seq: *seq,
                    ok: !transient,
                    payload: outcome.clone(),
                });
                Ok(())
            }
            Frame::Verification {
                seq,
                expected,
                observed,
                passed,
            } => {
                self.events.push(TraceEvent::Verification {
                    seq: *seq,
                    passed: *passed,
                    expected: expected.clone(),
                    observed: observed.clone(),
                });
                Ok(())
            }
            Frame::ApprovalRequest { prompt, schema } => {
                self.events.push(TraceEvent::ApprovalRequest {
                    prompt: prompt.clone(),
                    schema: *schema,
                });
                Ok(())
            }
            Frame::ApprovalDecision { input } => {
                self.events.push(TraceEvent::ApprovalDecision {
                    input: input.clone(),
                });
                Ok(())
            }
            Frame::Terminal {
                status,
                reason,
                summary,
            } => {
                if self.terminal.is_some() {
                    return Err(RuntimeError::JournalCorrupt);
                }
                self.terminal = Some(*status);
                self.reason = Some(reason.clone());
                self.summary = Some(summary.clone());
                self.events.push(TraceEvent::Terminal {
                    status: *status,
                    reason: reason.clone(),
                    summary: summary.clone(),
                });
                Ok(())
            }
        }
    }

    /// Finish the trace: the terminal must be the last frame, and a
    /// run that ends on an approval request is suspended.
    fn finish(self, journal: &Journal) -> Result<RunTrace, RuntimeError> {
        if self.terminal.is_some()
            && !matches!(journal.frames().last(), Some(Frame::Terminal { .. }))
        {
            return Err(RuntimeError::JournalCorrupt);
        }
        let suspended = self.terminal.is_none()
            && matches!(journal.frames().last(), Some(Frame::ApprovalRequest { .. }));
        Ok(RunTrace {
            events: self.events,
            terminal: self.terminal,
            reason: self.reason,
            summary: self.summary,
            turns_remaining: self.budget.model_turns,
            mutations_remaining: self.budget.mutations,
            suspended,
            model_bundle: self.model_bundle,
            input_tokens_used: self.input_tokens_used,
            output_tokens_used: self.output_tokens_used,
        })
    }
}
