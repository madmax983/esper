//! SPEC §11.3 crash-oracle invariants and `forbidden`-clause checks.
//!
//! The fixtures name invariants (`no_dispatch_without_intent`, ...) and
//! prohibitions (`"a second physical write to pin 4"`, ...) as
//! human-readable strings. This module parses both closed vocabularies
//! into checkable values. An unknown name or clause is a schema error at
//! parse time — never a silently skipped check (SPEC §12 gate 7: missing
//! measurement fails the gate rather than reporting success).
//!
//! // HOST-ONLY (E0/E1): heap-allocated check state, for the host runner.

use esper_core::decision::Level;
use esper_core::ids::Pin;
use esper_core::registry::{GPIO_PIN_WRITE_ID, TIMER_DELAY_WAIT_ID, lookup_by_name};
use esper_core::state::TerminalStatus;
use esper_runtime::journal::DecisionClass;
use esper_runtime::{FakeDevice, RunSeed, RunTrace, TraceEvent};

use crate::fixture::FixtureError;

/// The evidence one check runs against.
pub struct CheckCtx<'a> {
    /// The derived run trace.
    pub trace: &'a RunTrace,
    /// The seed the run was driven with.
    pub seed: &'a RunSeed,
    /// The fake device after the run.
    pub device: &'a FakeDevice,
    /// Pin levels before the run, for change detection.
    pub initial_levels: [bool; 8],
}

/// A SPEC §11.3 crash-oracle invariant, by fixture spelling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Invariant {
    /// §11.3.1: no effect dispatched without durable intent.
    NoDispatchWithoutIntent,
    /// §11.3.4: budgets never increase after reboot; permissions never
    /// widen; version bindings never change.
    BudgetsNeverWiden,
    /// §11.3.3: the first unresolved effect keeps its original identity.
    EffectIdentityStable,
    /// §11.3.6: the terminal result is emitted once logically.
    SingleLogicalTerminal,
    /// §11.3.5: a mutation is never reported complete without its
    /// committed passing verifier.
    MutationNeverCompleteWithoutVerifier,
}

impl Invariant {
    /// The fixture spelling of this invariant.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NoDispatchWithoutIntent => "no_dispatch_without_intent",
            Self::BudgetsNeverWiden => "budgets_never_widen",
            Self::EffectIdentityStable => "effect_identity_stable",
            Self::SingleLogicalTerminal => "single_logical_terminal",
            Self::MutationNeverCompleteWithoutVerifier => {
                "mutation_never_complete_without_verifier"
            }
        }
    }

    /// Run the invariant against the evidence.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::InvariantFailed`] naming the invariant
    /// and what was observed.
    pub fn check(self, ctx: &CheckCtx) -> Result<(), FixtureError> {
        let result = match self {
            Self::NoDispatchWithoutIntent => check_no_dispatch_without_intent(ctx),
            Self::BudgetsNeverWiden => check_budgets_never_widen(ctx),
            Self::EffectIdentityStable => check_effect_identity_stable(ctx),
            Self::SingleLogicalTerminal => check_single_logical_terminal(ctx),
            Self::MutationNeverCompleteWithoutVerifier => {
                check_mutation_never_complete_without_verifier(ctx)
            }
        };
        result.map_err(|detail| FixtureError::InvariantFailed {
            invariant: self.name(),
            detail,
        })
    }
}

/// Parse an invariant by its fixture spelling.
#[must_use]
pub fn parse_invariant(name: &str) -> Option<Invariant> {
    match name {
        "no_dispatch_without_intent" => Some(Invariant::NoDispatchWithoutIntent),
        "budgets_never_widen" => Some(Invariant::BudgetsNeverWiden),
        "effect_identity_stable" => Some(Invariant::EffectIdentityStable),
        "single_logical_terminal" => Some(Invariant::SingleLogicalTerminal),
        "mutation_never_complete_without_verifier" => {
            Some(Invariant::MutationNeverCompleteWithoutVerifier)
        }
        _ => None,
    }
}

/// Map a `PascalCase` status spelling to its status.
pub(crate) fn terminal_status_by_name(name: &str) -> Option<TerminalStatus> {
    match name {
        "Completed" => Some(TerminalStatus::Completed),
        "NeedsInput" => Some(TerminalStatus::NeedsInput),
        "Denied" => Some(TerminalStatus::Denied),
        "BudgetExhausted" => Some(TerminalStatus::BudgetExhausted),
        "Stuck" => Some(TerminalStatus::Stuck),
        "ToolUnavailable" => Some(TerminalStatus::ToolUnavailable),
        "ModelInvalid" => Some(TerminalStatus::ModelInvalid),
        "StorageFault" => Some(TerminalStatus::StorageFault),
        "Incompatible" => Some(TerminalStatus::Incompatible),
        _ => None,
    }
}

/// One parsed `forbidden` clause with its original spelling.
#[derive(Debug, Clone)]
pub struct ForbiddenClause {
    /// The clause as written in the fixture.
    pub text: String,
    /// The checkable form.
    pub check: Forbidden,
}

/// A checkable prohibition.
#[derive(Debug, Clone)]
pub enum Forbidden {
    /// No `ToolRequest` event may exist.
    NoToolRequests,
    /// No `ToolObservation` event may exist.
    NoToolObservations,
    /// No physical write and no pin level change.
    NoDeviceChange,
    /// At most this many committed model decisions.
    MaxModelTurns(u32),
    /// At most this many physical writes to the pin.
    MaxPhysicalWritesToPin {
        /// The pin.
        pin: u8,
        /// The maximum allowed physical writes.
        max: u32,
    },
    /// At most this many physical write dispatches in total.
    MaxWriteDispatches(u32),
    /// At most this many virtual-clock advances in total (the timer
    /// twin of [`Forbidden::MaxPhysicalWritesToPin`]).
    MaxClockAdvances(u32),
    /// At most this many tool intents for the tool.
    ToolRequestsForTool {
        /// The tool's numeric id.
        tool: u8,
        /// The maximum allowed intents.
        max: u32,
    },
    /// Effect ids stay stable (same as the invariant).
    StableEffectIds,
    /// The terminal status must not be this.
    TerminalNot(TerminalStatus),
    /// No `Finish` decision may be committed.
    NoFinishDecision,
    /// A `Completed` terminal requires every write intent to have a
    /// passing verification.
    CompletedRequiresVerifiedWrites,
    /// Budgets never widen (same as the invariant).
    BudgetsNeverWiden,
    /// No dispatch before intent (same as the invariant).
    NoDispatchBeforeIntent,
    /// Every approval request is followed by a decision.
    SuspendedHasDecision,
    /// Each approval decision consumes one pending request.
    MaxApprovalDecisionsPerRequest,
    /// Every tool intent commits after the last approval decision.
    ToolRequestAfterApprovalDecision,
}

impl ForbiddenClause {
    /// Run the clause against the evidence.
    ///
    /// # Errors
    ///
    /// Returns [`FixtureError::ForbiddenViolated`] with the clause's
    /// original spelling and what was observed.
    pub fn check(&self, ctx: &CheckCtx) -> Result<(), FixtureError> {
        check_forbidden(&self.check, ctx).map_err(|detail| FixtureError::ForbiddenViolated {
            // HOST-ONLY (E0/E1)
            clause: self.text.clone(),
            detail,
        })
    }
}

/// Parse one `forbidden` clause into its checkable form.
///
/// # Errors
///
/// Returns the schema message for an unknown clause; the caller wraps
/// it with the JSON path.
pub fn parse_forbidden(clause: &str) -> Result<Forbidden, String> {
    // HOST-ONLY (E0/E1)
    let unknown = || {
        format!(
            "unknown forbidden clause `{clause}`; the runner only understands its closed vocabulary"
        )
    };
    match clause {
        "any ToolRequest" => Ok(Forbidden::NoToolRequests),
        "any ToolObservation" => Ok(Forbidden::NoToolObservations),
        "any device state change" => Ok(Forbidden::NoDeviceChange),
        "a FINISH decision committed beyond budget" => Ok(Forbidden::NoFinishDecision),
        "budget values larger than the run seed after any step" => Ok(Forbidden::BudgetsNeverWiden),
        "a new EffectId on retry" | "a new EffectId minted after the crash" => {
            Ok(Forbidden::StableEffectIds)
        }
        "tool dispatch before its committed ToolRequest" => Ok(Forbidden::NoDispatchBeforeIntent),
        "a ToolObservation committed before its ToolRequest on replay" => {
            Ok(Forbidden::NoDispatchBeforeIntent)
        }
        "leaving AwaitInput without a committed ApprovalDecision" => {
            Ok(Forbidden::SuspendedHasDecision)
        }
        "a second ApprovalDecision for one ApprovalRequest" => {
            Ok(Forbidden::MaxApprovalDecisionsPerRequest)
        }
        "dispatching the write before the input commits" => {
            Ok(Forbidden::ToolRequestAfterApprovalDecision)
        }
        "TerminalResult Completed without a passing VerificationResult"
        | "reporting the mutation complete without a passing VerificationResult" => {
            Ok(Forbidden::CompletedRequiresVerifiedWrites)
        }
        "a second model turn rephrasing the denied call" => Ok(Forbidden::MaxModelTurns(1)),
        _ => parse_parameterized_forbidden(clause).ok_or_else(unknown),
    }
}

/// Parse the parameterized clause shapes (`a second physical write to
/// pin 4`, `a fourth model turn`, `TerminalResult Stuck`, ...).
fn parse_parameterized_forbidden(clause: &str) -> Option<Forbidden> {
    if let Some(rest) = clause.strip_prefix("a second physical write to pin ") {
        let pin: u8 = rest.parse().ok()?;
        return (pin <= 7).then_some(Forbidden::MaxPhysicalWritesToPin { pin, max: 1 });
    }
    if let Some(rest) = clause.strip_prefix("a second ToolRequest for the retried ") {
        // The fixtures abbreviate the tool family (`read`, `write`).
        let name = match rest {
            "read" => "gpio_pin_read",
            "write" => "gpio_pin_write",
            other => other,
        };
        let tool = lookup_by_name(name.as_bytes())?;
        return Some(Forbidden::ToolRequestsForTool {
            tool: tool.id,
            max: 1,
        });
    }
    if let Some(rest) = clause.strip_prefix("TerminalResult ") {
        let status = terminal_status_by_name(rest)?;
        return Some(Forbidden::TerminalNot(status));
    }
    let words: Vec<&str> = clause.split_whitespace().collect();
    if words.len() == 4
        && (words[0] == "a" || words[0] == "an")
        && words[2] == "model"
        && words[3] == "turn"
    {
        let ordinal = parse_ordinal(words[1])?;
        return Some(Forbidden::MaxModelTurns(ordinal - 1));
    }
    if words.len() == 4 && words[0] == "a" && words[2] == "write" && words[3] == "dispatch" {
        let ordinal = parse_ordinal(words[1])?;
        return Some(Forbidden::MaxWriteDispatches(ordinal - 1));
    }
    if words.len() == 4 && words[0] == "a" && words[2] == "clock" && words[3] == "advance" {
        let ordinal = parse_ordinal(words[1])?;
        return Some(Forbidden::MaxClockAdvances(ordinal - 1));
    }
    None
}

/// Map an ordinal word to its number.
fn parse_ordinal(word: &str) -> Option<u32> {
    match word {
        "second" => Some(2),
        "third" => Some(3),
        "fourth" => Some(4),
        "fifth" => Some(5),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Event indexes: tiny helpers over the trace event slice.
// ---------------------------------------------------------------------------

/// `(event index, effect seq, tool id)` for every tool intent, in order.
fn tool_requests(events: &[TraceEvent]) -> Vec<(usize, u32, u8)> {
    // HOST-ONLY (E0/E1)
    let mut out = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if let TraceEvent::ToolRequest { seq, tool, .. } = event {
            out.push((index, *seq, *tool));
        }
    }
    out
}

/// `(event index, effect seq, ok)` for every tool outcome, in order.
fn observations(events: &[TraceEvent]) -> Vec<(usize, u32, bool)> {
    // HOST-ONLY (E0/E1)
    let mut out = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if let TraceEvent::ToolObservation { seq, ok, .. } = event {
            out.push((index, *seq, *ok));
        }
    }
    out
}

/// `(event index, effect seq, passed)` for every read-back, in order.
fn verifications(events: &[TraceEvent]) -> Vec<(usize, u32, bool)> {
    // HOST-ONLY (E0/E1)
    let mut out = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if let TraceEvent::Verification { seq, passed, .. } = event {
            out.push((index, *seq, *passed));
        }
    }
    out
}

/// `(event index, decision class)` for every model decision, in order.
fn decisions(events: &[TraceEvent]) -> Vec<(usize, DecisionClass)> {
    // HOST-ONLY (E0/E1)
    let mut out = Vec::new();
    for (index, event) in events.iter().enumerate() {
        if let TraceEvent::ModelDecision { class, .. } = event {
            out.push((index, *class));
        }
    }
    out
}

/// Event indexes of approval requests and decisions, in order.
fn approvals(events: &[TraceEvent]) -> (Vec<usize>, Vec<usize>) {
    // HOST-ONLY (E0/E1)
    let mut requests = Vec::new();
    let mut decisioned = Vec::new();
    for (index, event) in events.iter().enumerate() {
        match event {
            TraceEvent::ApprovalRequest { .. } => requests.push(index),
            TraceEvent::ApprovalDecision { .. } => decisioned.push(index),
            _ => {}
        }
    }
    (requests, decisioned)
}

/// Whether a `usize` event count exceeds a `u32` bound.
///
/// Saturates at `u32::MAX` instead of narrowing, so the comparison
/// stays exact for every representable bound.
fn exceeds_u32(count: usize, max: u32) -> bool {
    count.min(u32::MAX as usize) > max as usize
}

/// The pin levels of the device right now.
pub(crate) fn pin_levels(device: &FakeDevice) -> [bool; 8] {
    let mut levels = [false; 8];
    let mut pin: u8 = 0;
    while pin < 8 {
        if let Ok(as_pin) = Pin::new(pin) {
            levels[usize::from(pin)] = device.read(as_pin) == Level::High;
        }
        pin += 1;
    }
    levels
}

// ---------------------------------------------------------------------------
// Invariant checks. Each returns the human-readable violation detail.
// ---------------------------------------------------------------------------

/// §11.3.1: every outcome and read-back follows its committed intent,
/// and every physical write carries a committed write intent's id.
fn check_no_dispatch_without_intent(ctx: &CheckCtx) -> Result<(), String> {
    // HOST-ONLY (E0/E1)
    let events = ctx.trace.events();
    let requests = tool_requests(events);
    let obs = observations(events);
    for (index, seq, _ok) in &obs {
        let preceded = requests
            .iter()
            .any(|(request_index, request_seq, _)| request_seq == seq && request_index < index);
        if !preceded {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "ToolObservation at event {index} (seq {seq}) has no preceding ToolRequest"
            ));
        }
    }
    for (index, seq, _passed) in verifications(events) {
        let preceded = obs
            .iter()
            .any(|(obs_index, obs_seq, ok)| obs_seq == &seq && *ok && obs_index < &index);
        if !preceded {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "Verification at event {index} (seq {seq}) has no preceding successful ToolObservation"
            ));
        }
    }
    // HOST-ONLY (E0/E1)
    let mut seen: Vec<u32> = Vec::new();
    for record in ctx.device.write_ledger() {
        let intended = requests
            .iter()
            .any(|(_, request_seq, tool)| *request_seq == record.seq && *tool == GPIO_PIN_WRITE_ID);
        if !intended {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "physical write to pin {} (seq {}) has no committed gpio_pin_write intent",
                record.pin, record.seq
            ));
        }
        if seen.contains(&record.seq) {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "physical write ledger holds seq {} twice: redelivery was not deduplicated",
                record.seq
            ));
        }
        seen.push(record.seq);
    }
    Ok(())
}

/// §11.3.4: remaining budgets never exceed the seed's grants.
fn check_budgets_never_widen(ctx: &CheckCtx) -> Result<(), String> {
    // HOST-ONLY (E0/E1)
    let mut problems: Vec<String> = Vec::new();
    if ctx.trace.turns_remaining() > ctx.seed.model_turns {
        problems.push(format!(
            "model_turns remaining {} exceeds the seed grant {}",
            ctx.trace.turns_remaining(),
            ctx.seed.model_turns
        ));
    }
    if ctx.trace.mutations_remaining() > ctx.seed.mutations {
        problems.push(format!(
            "mutations remaining {} exceeds the seed grant {}",
            ctx.trace.mutations_remaining(),
            ctx.seed.mutations
        ));
    }
    if problems.is_empty() {
        Ok(())
    } else {
        Err(problems.join("; "))
    }
}

/// §11.3.3: effect ids are minted once and never re-minted; every
/// outcome belongs to a committed intent's id.
fn check_effect_identity_stable(ctx: &CheckCtx) -> Result<(), String> {
    let events = ctx.trace.events();
    let requests = tool_requests(events);
    for pair in requests.windows(2) {
        if pair[1].1 <= pair[0].1 {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "effect ids not strictly increasing: seq {} then seq {}",
                pair[0].1, pair[1].1
            ));
        }
    }
    for (index, seq, _ok) in observations(events) {
        if !requests
            .iter()
            .any(|(_, request_seq, _)| request_seq == &seq)
        {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "ToolObservation at event {index} names seq {seq} with no ToolRequest"
            ));
        }
    }
    Ok(())
}

/// §11.3.6: exactly one terminal event, last, with reason and summary.
fn check_single_logical_terminal(ctx: &CheckCtx) -> Result<(), String> {
    let events = ctx.trace.events();
    // HOST-ONLY (E0/E1)
    let terminals: Vec<usize> = events
        .iter()
        .enumerate()
        .filter(|(_, event)| matches!(event, TraceEvent::Terminal { .. }))
        .map(|(index, _)| index)
        .collect();
    if terminals.len() != 1 {
        // HOST-ONLY (E0/E1)
        return Err(format!(
            "expected exactly one Terminal event, found {}",
            terminals.len()
        ));
    }
    if terminals[0] + 1 != events.len() {
        return Err("the Terminal event is not the last event".to_owned());
    }
    if ctx.trace.terminal_status().is_none() {
        return Err("trace has a Terminal event but no terminal status".to_owned());
    }
    if ctx.trace.reason().is_none() || ctx.trace.summary().is_none() {
        return Err("the terminal result is missing its reason or summary".to_owned());
    }
    Ok(())
}

/// §11.3.5: when the run reports `Completed`, every mutating intent has
/// a committed passing read-back. A run that never claimed completion
/// passes vacuously. The mutating tools are the idempotent writes:
/// `gpio_pin_write` and `timer_delay_wait` (SPEC §5.1).
fn check_mutation_never_complete_without_verifier(ctx: &CheckCtx) -> Result<(), String> {
    if ctx.trace.terminal_status() != Some(TerminalStatus::Completed) {
        return Ok(());
    }
    let events = ctx.trace.events();
    let vers = verifications(events);
    for (request_index, seq, tool) in tool_requests(events) {
        if tool != GPIO_PIN_WRITE_ID && tool != TIMER_DELAY_WAIT_ID {
            continue;
        }
        let verified = vers
            .iter()
            .any(|(index, vseq, passed)| vseq == &seq && *passed && index > &request_index);
        if !verified {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "mutating intent with seq {seq} is reported complete without a passing VerificationResult"
            ));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Forbidden-clause checks.
// ---------------------------------------------------------------------------

/// Run one parsed clause against the evidence; the caller wraps the
/// detail with the clause's original spelling.
fn check_forbidden(clause: &Forbidden, ctx: &CheckCtx) -> Result<(), String> {
    match clause {
        Forbidden::NoToolRequests => {
            if ctx.trace.tool_requests() != 0 {
                // HOST-ONLY (E0/E1)
                return Err(format!(
                    "{} ToolRequest events committed",
                    ctx.trace.tool_requests()
                ));
            }
            Ok(())
        }
        Forbidden::NoToolObservations => {
            if ctx.trace.tool_observations() != 0 {
                // HOST-ONLY (E0/E1)
                return Err(format!(
                    "{} ToolObservation events committed",
                    ctx.trace.tool_observations()
                ));
            }
            Ok(())
        }
        Forbidden::NoDeviceChange => check_no_device_change(ctx),
        Forbidden::MaxModelTurns(max) => {
            let count = decisions(ctx.trace.events()).len();
            if exceeds_u32(count, *max) {
                // HOST-ONLY (E0/E1)
                return Err(format!(
                    "{count} model turns committed, at most {max} allowed"
                ));
            }
            Ok(())
        }
        Forbidden::MaxPhysicalWritesToPin { pin, max } => {
            let count = ctx
                .device
                .write_ledger()
                .iter()
                .filter(|record| record.pin == *pin)
                .count();
            if exceeds_u32(count, *max) {
                // HOST-ONLY (E0/E1)
                return Err(format!(
                    "pin {pin} was physically written {count} times, at most {max} allowed"
                ));
            }
            Ok(())
        }
        Forbidden::MaxWriteDispatches(max) => {
            let count = ctx.device.write_ledger().len();
            if exceeds_u32(count, *max) {
                // HOST-ONLY (E0/E1)
                return Err(format!(
                    "{count} physical write dispatches, at most {max} allowed"
                ));
            }
            Ok(())
        }
        Forbidden::MaxClockAdvances(max) => check_max_clock_advances(ctx, *max),
        Forbidden::ToolRequestsForTool { tool, max } => {
            let count = tool_requests(ctx.trace.events())
                .iter()
                .filter(|(_, _, id)| id == tool)
                .count();
            if exceeds_u32(count, *max) {
                // HOST-ONLY (E0/E1)
                return Err(format!(
                    "{count} ToolRequest events for tool-{tool}, at most {max} allowed"
                ));
            }
            Ok(())
        }
        Forbidden::StableEffectIds => check_effect_identity_stable(ctx),
        Forbidden::TerminalNot(status) => {
            if ctx.trace.terminal_status() == Some(*status) {
                // HOST-ONLY (E0/E1)
                return Err(format!("terminal status is {}", status.name()));
            }
            Ok(())
        }
        Forbidden::NoFinishDecision => {
            if decisions(ctx.trace.events())
                .iter()
                .any(|(_, class)| *class == DecisionClass::Finish)
            {
                return Err("a Finish decision was committed".to_owned());
            }
            Ok(())
        }
        Forbidden::CompletedRequiresVerifiedWrites => {
            check_mutation_never_complete_without_verifier(ctx)
        }
        Forbidden::BudgetsNeverWiden => check_budgets_never_widen(ctx),
        Forbidden::NoDispatchBeforeIntent => check_no_dispatch_without_intent(ctx),
        Forbidden::SuspendedHasDecision => check_suspended_has_decision(ctx),
        Forbidden::MaxApprovalDecisionsPerRequest => check_approval_decisions_matched(ctx),
        Forbidden::ToolRequestAfterApprovalDecision => check_requests_after_approval(ctx),
    }
}

/// No physical write and no pin level moved from its initial state.
fn check_no_device_change(ctx: &CheckCtx) -> Result<(), String> {
    if ctx.device.physical_writes() != 0 {
        // HOST-ONLY (E0/E1)
        return Err(format!(
            "{} physical writes executed",
            ctx.device.physical_writes()
        ));
    }
    let now = pin_levels(ctx.device);
    for (pin, (before, after)) in ctx.initial_levels.iter().zip(now.iter()).enumerate() {
        if before != after {
            // HOST-ONLY (E0/E1)
            return Err(format!("pin {pin} changed level without a physical write"));
        }
    }
    Ok(())
}

/// At most `max` virtual-clock advances: the timer twin of the
/// physical-write bound.
///
/// Each committed `timer_delay_wait` intent dispatches exactly one
/// physical clock advance — redeliveries under the same `EffectId` hit
/// the device dedup cache and never re-commit the intent — so the
/// committed delay intents in the trace count the advances.
fn check_max_clock_advances(ctx: &CheckCtx, max: u32) -> Result<(), String> {
    let count = ctx
        .trace
        .events()
        .iter()
        .filter(|event| {
            matches!(
                event,
                TraceEvent::ToolRequest { tool, .. } if *tool == TIMER_DELAY_WAIT_ID
            )
        })
        .count();
    if exceeds_u32(count, max) {
        // HOST-ONLY (E0/E1)
        return Err(format!("{count} clock advances, at most {max} allowed"));
    }
    Ok(())
}

/// Every approval request is followed by exactly the decision that
/// answers it: no request is left hanging.
fn check_suspended_has_decision(ctx: &CheckCtx) -> Result<(), String> {
    let (requests, decisioned) = approvals(ctx.trace.events());
    for request in requests {
        if !decisioned.iter().any(|decision| decision > &request) {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "ApprovalRequest at event {request} was never answered by an ApprovalDecision"
            ));
        }
    }
    Ok(())
}

/// Each approval decision consumes one pending request: a decision with
/// no pending request, or two decisions for one request, fails.
fn check_approval_decisions_matched(ctx: &CheckCtx) -> Result<(), String> {
    // HOST-ONLY (E0/E1)
    let mut ordered: Vec<(usize, bool)> = Vec::new(); // (index, is_request)
    for (index, event) in ctx.trace.events().iter().enumerate() {
        match event {
            TraceEvent::ApprovalRequest { .. } => ordered.push((index, true)),
            TraceEvent::ApprovalDecision { .. } => ordered.push((index, false)),
            _ => {}
        }
    }
    let mut pending = 0u32;
    for (index, is_request) in ordered {
        if is_request {
            pending += 1;
        } else if pending == 0 {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "ApprovalDecision at event {index} has no pending ApprovalRequest"
            ));
        } else {
            pending -= 1;
        }
    }
    Ok(())
}

/// Every tool intent commits after the last approval decision: the
/// model may not dispatch on a guess before the input commits.
fn check_requests_after_approval(ctx: &CheckCtx) -> Result<(), String> {
    let (requests, decisioned) = approvals(ctx.trace.events());
    let Some(last_decision) = decisioned.iter().max() else {
        return Ok(());
    };
    if requests.is_empty() {
        return Ok(());
    }
    for (index, _seq, _tool) in tool_requests(ctx.trace.events()) {
        if index < *last_decision {
            // HOST-ONLY (E0/E1)
            return Err(format!(
                "ToolRequest at event {index} committed before the ApprovalDecision at event {last_decision}"
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{Forbidden, parse_forbidden};

    #[test]
    fn parses_every_forbidden_shape() {
        let cases = [
            "any ToolRequest",
            "any ToolObservation",
            "any device state change",
            "a fourth model turn",
            "a third model turn",
            "a second physical write to pin 4",
            "a fourth write dispatch",
            "a second clock advance",
            "a second ToolRequest for the retried gpio_pin_read",
            "a second ToolRequest for the retried read",
            "a second ToolRequest for the retried write",
            "a new EffectId on retry",
            "a new EffectId minted after the crash",
            "TerminalResult Completed",
            "TerminalResult ToolUnavailable",
            "a FINISH decision committed beyond budget",
            "budget values larger than the run seed after any step",
            "tool dispatch before its committed ToolRequest",
            "a ToolObservation committed before its ToolRequest on replay",
            "leaving AwaitInput without a committed ApprovalDecision",
            "a second ApprovalDecision for one ApprovalRequest",
            "dispatching the write before the input commits",
            "TerminalResult Completed without a passing VerificationResult",
            "reporting the mutation complete without a passing VerificationResult",
            "a second model turn rephrasing the denied call",
        ];
        for clause in cases {
            assert!(
                parse_forbidden(clause).is_ok(),
                "clause did not parse: {clause}"
            );
        }
    }

    #[test]
    fn rejects_unknown_clauses_loudly() {
        let err = parse_forbidden("the moon is made of cheese").expect_err("unknown clause");
        assert!(err.contains("unknown forbidden clause"));
    }

    #[test]
    fn ordinal_clauses_carry_the_right_bound() {
        assert!(matches!(
            parse_forbidden("a fourth model turn"),
            Ok(Forbidden::MaxModelTurns(3))
        ));
        assert!(matches!(
            parse_forbidden("a third model turn"),
            Ok(Forbidden::MaxModelTurns(2))
        ));
        assert!(matches!(
            parse_forbidden("a fourth write dispatch"),
            Ok(Forbidden::MaxWriteDispatches(3))
        ));
        assert!(matches!(
            parse_forbidden("a second clock advance"),
            Ok(Forbidden::MaxClockAdvances(1))
        ));
        assert!(matches!(
            parse_forbidden("a third clock advance"),
            Ok(Forbidden::MaxClockAdvances(2))
        ));
    }
}
