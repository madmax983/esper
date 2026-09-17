//! Exact trace comparison: the runtime's committed trace against the
//! fixture's expected trace.
//!
//! Every runtime [`TraceEvent`] is decoded into a semantic
//! `ActualEvent` — model lines go back through
//! [`esper_core::decision::decode_line`], tool ids resolve to canonical
//! names, and every JSON payload parses through the eval's own parser
//! for canonical comparison. The expected and actual event sequences
//! are then walked in lockstep: same length, same kinds in the same
//! order, same fields.
//!
//! Absolute [`EffectId`](esper_core::ids::EffectId) values are ignored —
//! the allocator mints them — but identity relationships are checked:
//! an observation or read-back must name the effect id of a committed
//! intent for the same tool, and `same_effect_id_as_attempt` must name
//! the exact earlier attempt's id. The first mismatch (and every later
//! one, up to a cap) renders as a readable event-by-event diff.
//!
//! Two fixture annotations are parsed but not asserted against the
//! runtime, because no public API exposes them: `progress` (the
//! [`RunTrace`] and [`Journal`](esper_runtime::Journal) carry no
//! progress deltas — they live in the engine's in-memory monitor) and
//! the free-text `violation` note on `InvalidArgs` decisions (there is
//! no canonical violation vocabulary to match it against; the check
//! asserts the line did fail to decode and names the tool it named).
//!
//! // HOST-ONLY (E0/E1): heap-allocated comparison, for the host runner.

use esper_core::decision::{decode_line, Decision};
use esper_core::error::RepairVariant;
use esper_core::registry::lookup_by_id;
use esper_core::state::TerminalStatus;
use esper_runtime::{journal::DecisionClass, RunTrace, TraceEvent};
use std::fmt::Write as _;

use crate::fixture::{ExpectedDecision, ExpectedEvent, FixtureError};
use crate::json::{self, Value};

/// Cap on reported per-event mismatches; the count line always shows
/// the true total.
const MAX_DIFF_LINES: usize = 12;

/// Compare the fixture's expected trace with the runtime's trace.
///
/// # Errors
///
/// Returns [`FixtureError::TraceDiff`] with a readable event-by-event
/// diff when the sequences differ in length, order, kind, or any
/// compared field.
pub fn compare_trace(expected: &[ExpectedEvent], trace: &RunTrace) -> Result<(), FixtureError> {
    let actual = actual_events(trace)?;
    // HOST-ONLY (E0/E1)
    let mut diffs: Vec<String> = Vec::new();
    if actual.len() != expected.len() {
        diffs.push(format!(
            "event count differs: expected {} events, the trace holds {}",
            expected.len(),
            actual.len()
        ));
    }
    let mut ctx = CmpCtx::new(&actual);
    for (index, (want, got)) in expected.iter().zip(actual.iter()).enumerate() {
        if let Some(line) = compare_event(index, want, got, &ctx) {
            diffs.push(line);
        }
        // Record the attempt after comparing, so `earlier` means
        // strictly earlier for `same_effect_id_as_attempt`.
        ctx.observe_actual(got);
    }
    if diffs.is_empty() {
        Ok(())
    } else {
        // HOST-ONLY (E0/E1)
        let mut rendered = String::from("trace mismatch:\n");
        for line in diffs.iter().take(MAX_DIFF_LINES) {
            rendered.push_str("  ");
            rendered.push_str(line);
            rendered.push('\n');
        }
        if diffs.len() > MAX_DIFF_LINES {
            let _ = writeln!(rendered, "  ... and {} more", diffs.len() - MAX_DIFF_LINES);
        }
        Err(FixtureError::TraceDiff { diff: rendered })
    }
}

// ---------------------------------------------------------------------------
// Actual events: the runtime trace decoded into comparable values.
// ---------------------------------------------------------------------------

/// One runtime trace event, decoded.
#[derive(Debug)]
enum ActualEvent {
    /// A committed model line, re-decoded.
    ModelDecision(ActualDecision),
    /// A committed tool intent.
    ToolRequest {
        /// The stable effect sequence.
        seq: u32,
        /// The canonical tool name.
        tool: String,
        /// The validated arguments, as JSON.
        args: Value,
    },
    /// A committed tool outcome.
    ToolObservation {
        /// The canonical tool name, resolved through the intent's id.
        tool: String,
        /// The effect id this outcome belongs to.
        seq: u32,
        /// Whether the dispatch succeeded.
        ok: bool,
        /// The result payload, as JSON (`None` for a transient failure).
        payload: Option<Value>,
    },
    /// A committed independent read-back.
    Verification {
        /// The canonical tool name, resolved through the intent's id.
        tool: String,
        /// The effect id that was verified.
        seq: u32,
        /// The expected pin state, as JSON.
        expected: Value,
        /// The observed pin state, as JSON.
        observed: Value,
        /// Whether read-back matched.
        passed: bool,
    },
    /// The engine asked for typed human input.
    ApprovalRequest {
        /// The prompt bytes.
        prompt: Vec<u8>,
        /// The response schema id.
        schema: u8,
    },
    /// Typed human input arrived and committed.
    ApprovalDecision {
        /// The input payload, as JSON.
        input: Value,
    },
    /// The run's one logical terminal result.
    Terminal {
        /// The terminal status.
        status: TerminalStatus,
        /// The machine-readable reason bytes.
        reason: Vec<u8>,
        /// The run summary bytes.
        summary: Vec<u8>,
    },
}

/// A committed model line, re-decoded exactly the way the engine saw it.
#[derive(Debug)]
enum ActualDecision {
    /// A well-formed tool call.
    Call {
        /// The canonical tool name.
        tool: String,
        /// The validated arguments, as JSON.
        args: Value,
    },
    /// A well-formed human-input request.
    Ask {
        /// The prompt bytes.
        prompt: Vec<u8>,
        /// The response schema id.
        schema: u8,
    },
    /// A well-formed terminal answer.
    Finish {
        /// The status word, e.g. `completed`.
        status: String,
        /// The summary bytes.
        summary: Vec<u8>,
    },
    /// The line broke the output grammar.
    Malformed,
    /// The JSON was well-formed but violated the static arg schema.
    InvalidArgs {
        /// The tool token after `CALL`, when the line names one.
        tool_token: Option<String>,
    },
}

/// Decode every trace event; JSON payloads that do not parse become a
/// diff error (the runtime committed bytes the comparison cannot read).
fn actual_events(trace: &RunTrace) -> Result<Vec<ActualEvent>, FixtureError> {
    // First pass: decode every event. Observation and verification
    // tool names resolve in the second pass, through the intent table.
    // HOST-ONLY (E0/E1)
    let mut events = Vec::new();
    for event in trace.events() {
        events.push(match event {
            TraceEvent::ModelDecision { line, .. } => {
                ActualEvent::ModelDecision(decode_actual_decision(line)?)
            }
            TraceEvent::ToolRequest { seq, tool, args } => ActualEvent::ToolRequest {
                seq: *seq,
                tool: tool_name(*tool)?,
                args: parse_json("tool args", args)?,
            },
            TraceEvent::ToolObservation { seq, ok, payload, .. } => {
                ActualEvent::ToolObservation {
                    tool: String::new(),
                    seq: *seq,
                    ok: *ok,
                    payload: if *ok {
                        Some(parse_json("observation payload", payload)?)
                    } else {
                        if !payload.is_empty() {
                            return Err(FixtureError::TraceDiff {
                                // HOST-ONLY (E0/E1)
                                diff: "trace mismatch:\n  a transient observation carries a non-empty payload\n"
                                    .to_owned(),
                            });
                        }
                        None
                    },
                }
            }
            TraceEvent::Verification {
                seq,
                passed,
                expected,
                observed,
                ..
            } => ActualEvent::Verification {
                tool: String::new(),
                seq: *seq,
                expected: parse_json("verification expected", expected)?,
                observed: parse_json("verification observed", observed)?,
                passed: *passed,
            },
            TraceEvent::ApprovalRequest { prompt, schema } => ActualEvent::ApprovalRequest {
                // HOST-ONLY (E0/E1)
                prompt: prompt.clone(),
                schema: *schema,
            },
            TraceEvent::ApprovalDecision { input } => ActualEvent::ApprovalDecision {
                input: parse_json("approval input", input)?,
            },
            TraceEvent::Terminal {
                status,
                reason,
                summary,
            } => ActualEvent::Terminal {
                status: *status,
                // HOST-ONLY (E0/E1)
                reason: reason.clone(),
                summary: summary.clone(),
            },
        });
    }
    // Second pass: every outcome and read-back names its intent's tool.
    // HOST-ONLY (E0/E1)
    let mut intents: Vec<(u32, String)> = Vec::new();
    for event in &events {
        if let ActualEvent::ToolRequest { seq, tool, .. } = event {
            intents.push((*seq, tool.clone()));
        }
    }
    for event in &mut events {
        let (tool_field, seq) = match event {
            ActualEvent::ToolObservation { tool, seq, .. }
            | ActualEvent::Verification { tool, seq, .. } => (tool, *seq),
            _ => continue,
        };
        *tool_field = intents.iter().find(|(id, _)| *id == seq).map_or_else(
            // HOST-ONLY (E0/E1)
            || format!("<no intent for seq {seq}>"),
            |(_, name)| name.clone(),
        );
    }
    Ok(events)
}

/// Resolve a numeric tool id to its canonical name.
fn tool_name(id: u8) -> Result<String, FixtureError> {
    lookup_by_id(esper_core::ids::ToolId::new(id)).map_or_else(
        || {
            Err(FixtureError::TraceDiff {
                // HOST-ONLY (E0/E1)
                diff: format!("trace mismatch:\n  the trace names unknown tool id {id}\n"),
            })
        },
        // HOST-ONLY (E0/E1)
        |entry| Ok(entry.name.to_owned()),
    )
}

/// Parse runtime-emitted JSON bytes for canonical comparison.
fn parse_json(what: &str, bytes: &[u8]) -> Result<Value, FixtureError> {
    let text = core::str::from_utf8(bytes).map_err(|_| FixtureError::TraceDiff {
        // HOST-ONLY (E0/E1)
        diff: format!("trace mismatch:\n  {what} is not valid UTF-8\n"),
    })?;
    json::parse(text).map_err(|err| FixtureError::TraceDiff {
        // HOST-ONLY (E0/E1)
        diff: format!("trace mismatch:\n  {what} does not parse as JSON: {err}\n"),
    })
}

/// Re-decode a committed model line exactly the way the engine did.
fn decode_actual_decision(line: &[u8]) -> Result<ActualDecision, FixtureError> {
    match decode_line(line) {
        Ok(Decision::Call(call)) => Ok(ActualDecision::Call {
            tool: tool_name(call.tool().get())?,
            args: parse_json("call args", call.args())?,
        }),
        Ok(Decision::Ask(ask)) => Ok(ActualDecision::Ask {
            // HOST-ONLY (E0/E1)
            prompt: ask.prompt().to_vec(),
            schema: ask.response_schema_id(),
        }),
        Ok(Decision::Finish(finish)) => Ok(ActualDecision::Finish {
            // HOST-ONLY (E0/E1)
            status: finish.status().name().to_owned(),
            summary: finish.summary().to_vec(),
        }),
        Err(err) => {
            // InvalidArgs lines name a tool but fail argument validation;
            // anything else (or no classified variant) is Malformed.
            if err.repair_variant() == Some(RepairVariant::InvalidArgs) {
                Ok(ActualDecision::InvalidArgs {
                    tool_token: tool_token(line),
                })
            } else {
                Ok(ActualDecision::Malformed)
            }
        }
    }
}

/// The tool token of a `CALL <tool> ...` line, for the fixture's
/// `InvalidArgs` tool annotation. Returns `None` when the line does
/// not name a tool.
fn tool_token(line: &[u8]) -> Option<String> {
    let text = core::str::from_utf8(line).ok()?;
    let mut parts = text.split(' ');
    if parts.next()? != "CALL" {
        return None;
    }
    // HOST-ONLY (E0/E1)
    parts.next().map(str::to_owned)
}

// ---------------------------------------------------------------------------
// Comparison state: intent table and per-tool attempt history.
// ---------------------------------------------------------------------------

/// Identity context while walking the two sequences.
struct CmpCtx {
    /// `(seq, tool name)` for every actual tool intent, in order.
    requests: Vec<(u32, String)>,
    /// Seqs of strictly earlier actual observations per tool, in order.
    observations: Vec<(String, Vec<u32>)>,
}

impl CmpCtx {
    /// Build the intent table from the decoded trace.
    fn new(actual: &[ActualEvent]) -> Self {
        // HOST-ONLY (E0/E1)
        let mut requests = Vec::new();
        for event in actual {
            if let ActualEvent::ToolRequest { seq, tool, .. } = event {
                requests.push((*seq, tool.clone()));
            }
        }
        Self {
            requests,
            // HOST-ONLY (E0/E1)
            observations: Vec::new(),
        }
    }

    /// Record an actual observation's attempt for `same_effect_id_as_attempt`.
    fn observe_actual(&mut self, event: &ActualEvent) {
        if let ActualEvent::ToolObservation { tool, seq, .. } = event {
            match self.observations.iter_mut().find(|(name, _)| name == tool) {
                // HOST-ONLY (E0/E1)
                Some((_, seqs)) => seqs.push(*seq),
                None => self.observations.push((tool.clone(), vec![*seq])),
            }
        }
    }

    /// Whether some committed intent for the tool carries this id.
    fn intent_exists(&self, tool: &str, seq: u32) -> bool {
        self.requests
            .iter()
            .any(|(id, name)| *id == seq && name == tool)
    }

    /// The seqs of strictly earlier observations for the tool, in order.
    fn earlier_observation_seqs(&self, tool: &str) -> &[u32] {
        self.observations
            .iter()
            .find(|(name, _)| name == tool)
            .map_or(&[], |(_, seqs)| seqs.as_slice())
    }
}

/// Render one event-pair mismatch.
fn mismatch(index: usize, detail: &str, want: &ExpectedEvent, got: &ActualEvent) -> String {
    // HOST-ONLY (E0/E1)
    format!(
        "event[{index}]: {detail}\n           expected {exp}\n           actual   {act}",
        exp = render_expected(want),
        act = render_actual(got),
    )
}

// ---------------------------------------------------------------------------
// Event-by-event comparison.
// ---------------------------------------------------------------------------

/// Compare one expected/actual pair; `None` when they agree.
fn compare_event(
    index: usize,
    want: &ExpectedEvent,
    got: &ActualEvent,
    ctx: &CmpCtx,
) -> Option<String> {
    match (want, got) {
        (ExpectedEvent::ModelDecision(w), ActualEvent::ModelDecision(g)) => {
            compare_decision(w, g).map(|detail| mismatch(index, &detail, want, got))
        }
        (
            ExpectedEvent::ToolRequest { tool, args },
            ActualEvent::ToolRequest {
                tool: actual_tool,
                args: actual_args,
                ..
            },
        ) => compare_tool_request(index, want, got, tool, args, actual_tool, actual_args),
        (
            ExpectedEvent::ToolObservation {
                tool,
                ok,
                payload,
                same_effect_id_as_attempt,
                ..
            },
            ActualEvent::ToolObservation {
                tool: actual_tool,
                seq,
                ok: actual_ok,
                payload: actual_payload,
                ..
            },
        ) => compare_tool_observation(
            index,
            want,
            got,
            ctx,
            tool,
            *ok,
            payload.as_ref(),
            same_effect_id_as_attempt.as_ref(),
            actual_tool,
            *seq,
            *actual_ok,
            actual_payload.as_ref(),
        ),
        (
            ExpectedEvent::Verification {
                tool,
                expected,
                observed,
                passed,
                ..
            },
            ActualEvent::Verification {
                tool: actual_tool,
                seq,
                expected: actual_expected,
                observed: actual_observed,
                passed: actual_passed,
                ..
            },
        ) => compare_verification(
            index,
            want,
            got,
            ctx,
            tool,
            expected,
            observed,
            *passed,
            actual_tool,
            *seq,
            actual_expected,
            actual_observed,
            *actual_passed,
        ),
        _ => compare_event_tail(index, want, got, ctx),
    }
}

/// Compare the approval and terminal pairs; `None` when they agree.
///
/// Split from [`compare_event`] so each dispatcher stays readable.
fn compare_event_tail(
    index: usize,
    want: &ExpectedEvent,
    got: &ActualEvent,
    ctx: &CmpCtx,
) -> Option<String> {
    // HOST-ONLY (E0/E1): `ctx` is unused here today; the signature stays
    // uniform so future approval checks can consult committed intents.
    let _ = ctx;
    match (want, got) {
        (
            ExpectedEvent::ApprovalRequest { prompt, schema },
            ActualEvent::ApprovalRequest {
                prompt: actual_prompt,
                schema: actual_schema,
            },
        ) => compare_approval_request(
            index,
            want,
            got,
            prompt,
            *schema,
            actual_prompt,
            *actual_schema,
        ),
        (
            ExpectedEvent::ApprovalDecision { input, .. },
            ActualEvent::ApprovalDecision {
                input: actual_input,
            },
        ) => compare_approval_decision(index, want, got, input, actual_input),
        (
            ExpectedEvent::TerminalResult {
                status,
                reason,
                summary,
            },
            ActualEvent::Terminal {
                status: actual_status,
                reason: actual_reason,
                summary: actual_summary,
            },
        ) => compare_terminal(
            index,
            want,
            got,
            *status,
            reason,
            summary,
            *actual_status,
            actual_reason,
            actual_summary,
        ),
        _ => Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!(
                "kind differs: `{}` vs `{}`",
                expected_kind(want),
                actual_kind(got)
            ),
            want,
            got,
        )),
    }
}

/// Compare a tool-intent pair; `None` when they agree.
#[allow(clippy::too_many_arguments)]
fn compare_tool_request(
    index: usize,
    want: &ExpectedEvent,
    got: &ActualEvent,
    tool: &String,
    args: &Value,
    actual_tool: &String,
    actual_args: &Value,
) -> Option<String> {
    if tool != actual_tool {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!("tool differs: `{tool}` vs `{actual_tool}`"),
            want,
            got,
        ));
    }
    if !args.equals_canonical(actual_args) {
        return Some(mismatch(index, "args differ", want, got));
    }
    None
}

/// Compare a tool-outcome pair, including effect-id relationships.
#[allow(clippy::too_many_arguments)]
fn compare_tool_observation(
    index: usize,
    want: &ExpectedEvent,
    got: &ActualEvent,
    ctx: &CmpCtx,
    tool: &String,
    ok: bool,
    payload: Option<&Value>,
    same_effect_id_as_attempt: Option<&usize>,
    actual_tool: &String,
    seq: u32,
    actual_ok: bool,
    actual_payload: Option<&Value>,
) -> Option<String> {
    if tool != actual_tool {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!("tool differs: `{tool}` vs `{actual_tool}`"),
            want,
            got,
        ));
    }
    if !ctx.intent_exists(tool, seq) {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!("observation seq {seq} matches no committed intent for `{tool}`"),
            want,
            got,
        ));
    }
    if ok != actual_ok {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!("ok differs: `{ok}` vs `{actual_ok}`"),
            want,
            got,
        ));
    }
    match (payload, actual_payload) {
        (Some(want_payload), Some(got_payload)) if want_payload.equals_canonical(got_payload) => {}
        (Some(_), Some(_)) => {
            return Some(mismatch(index, "payload differs", want, got));
        }
        (None, None) => {}
        (Some(_), None) => {
            return Some(mismatch(
                index,
                "expected a payload, the trace has none",
                want,
                got,
            ));
        }
        (None, Some(_)) => {
            return Some(mismatch(
                index,
                "expected no payload, the trace has one",
                want,
                got,
            ));
        }
    }
    if let Some(attempt) = same_effect_id_as_attempt {
        let Some(previous) = attempt.checked_sub(1) else {
            return Some(mismatch(
                index,
                "same_effect_id_as_attempt must be at least 1",
                want,
                got,
            ));
        };
        let earlier = ctx.earlier_observation_seqs(tool);
        match earlier.get(previous) {
            Some(first) if *first == seq => {}
            Some(first) => {
                return Some(mismatch(
                    index,
                    // HOST-ONLY (E0/E1)
                    &format!(
                        "effect id differs: attempt {attempt} shares seq {first}, not seq {seq}"
                    ),
                    want,
                    got,
                ));
            }
            None => {
                return Some(mismatch(
                    index,
                    // HOST-ONLY (E0/E1)
                    &format!(
                        "same_effect_id_as_attempt={attempt} but only {} earlier observation(s) of `{tool}` exist",
                        earlier.len()
                    ),
                    want,
                    got,
                ));
            }
        }
    }
    None
}

/// Compare an independent read-back pair.
#[allow(clippy::too_many_arguments)]
fn compare_verification(
    index: usize,
    want: &ExpectedEvent,
    got: &ActualEvent,
    ctx: &CmpCtx,
    tool: &String,
    expected: &Value,
    observed: &Value,
    passed: bool,
    actual_tool: &String,
    seq: u32,
    actual_expected: &Value,
    actual_observed: &Value,
    actual_passed: bool,
) -> Option<String> {
    if tool != actual_tool {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!("tool differs: `{tool}` vs `{actual_tool}`"),
            want,
            got,
        ));
    }
    if !ctx.intent_exists(tool, seq) {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!("verification seq {seq} matches no committed intent for `{tool}`"),
            want,
            got,
        ));
    }
    if !expected.equals_canonical(actual_expected) {
        return Some(mismatch(index, "expected pin state differs", want, got));
    }
    if !observed.equals_canonical(actual_observed) {
        return Some(mismatch(index, "observed pin state differs", want, got));
    }
    if passed != actual_passed {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!("passed differs: `{passed}` vs `{actual_passed}`"),
            want,
            got,
        ));
    }
    None
}

/// Compare an approval prompt pair.
fn compare_approval_request(
    index: usize,
    want: &ExpectedEvent,
    got: &ActualEvent,
    prompt: &String,
    schema: u8,
    actual_prompt: &[u8],
    actual_schema: u8,
) -> Option<String> {
    if prompt.as_bytes() != actual_prompt {
        return Some(mismatch(index, "prompt differs", want, got));
    }
    if schema != actual_schema {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!("schema differs: `{schema}` vs `{actual_schema}`"),
            want,
            got,
        ));
    }
    None
}

/// Compare a typed human-input pair.
fn compare_approval_decision(
    index: usize,
    want: &ExpectedEvent,
    got: &ActualEvent,
    input: &Value,
    actual_input: &Value,
) -> Option<String> {
    if !input.equals_canonical(actual_input) {
        return Some(mismatch(index, "input differs", want, got));
    }
    None
}

/// Compare the run's terminal result.
#[allow(clippy::too_many_arguments)]
fn compare_terminal(
    index: usize,
    want: &ExpectedEvent,
    got: &ActualEvent,
    status: TerminalStatus,
    reason: &String,
    summary: &String,
    actual_status: TerminalStatus,
    actual_reason: &[u8],
    actual_summary: &[u8],
) -> Option<String> {
    if status != actual_status {
        return Some(mismatch(
            index,
            // HOST-ONLY (E0/E1)
            &format!(
                "status differs: `{}` vs `{}`",
                status.name(),
                actual_status.name()
            ),
            want,
            got,
        ));
    }
    if reason.as_bytes() != actual_reason {
        return Some(mismatch(index, "reason differs", want, got));
    }
    if summary.as_bytes() != actual_summary {
        return Some(mismatch(index, "summary differs", want, got));
    }
    None
}

/// Compare a model decision pair; `None` when they agree.
fn compare_decision(want: &ExpectedDecision, got: &ActualDecision) -> Option<String> {
    if want.class() != actual_class(got) {
        // HOST-ONLY (E0/E1)
        return Some(format!(
            "decision class differs: `{}` vs `{}`",
            decision_class_name(want.class()),
            decision_class_name(actual_class(got))
        ));
    }
    match (want, got) {
        (
            ExpectedDecision::Call { tool, args },
            ActualDecision::Call {
                tool: actual_tool,
                args: actual_args,
            },
        ) => {
            if tool != actual_tool {
                // HOST-ONLY (E0/E1)
                return Some(format!("call tool differs: `{tool}` vs `{actual_tool}`"));
            }
            if !args.equals_canonical(actual_args) {
                return Some("call args differ".to_owned());
            }
            None
        }
        (
            ExpectedDecision::Ask { prompt, schema },
            ActualDecision::Ask {
                prompt: actual_prompt,
                schema: actual_schema,
            },
        ) => {
            if prompt.as_bytes() != actual_prompt.as_slice() {
                return Some("ask prompt differs".to_owned());
            }
            if schema != actual_schema {
                // HOST-ONLY (E0/E1)
                return Some(format!(
                    "ask schema differs: `{schema}` vs `{actual_schema}`"
                ));
            }
            None
        }
        (
            ExpectedDecision::Finish { status, summary },
            ActualDecision::Finish {
                status: actual_status,
                summary: actual_summary,
            },
        ) => {
            if status != actual_status {
                // HOST-ONLY (E0/E1)
                return Some(format!(
                    "finish status differs: `{status}` vs `{actual_status}`"
                ));
            }
            if summary.as_bytes() != actual_summary.as_slice() {
                return Some("finish summary differs".to_owned());
            }
            None
        }
        (
            ExpectedDecision::InvalidArgs { tool, .. },
            ActualDecision::InvalidArgs { tool_token },
        ) => {
            // The fixture annotates the tool the broken line named;
            // the free-text `violation` note has no canonical
            // vocabulary, so only the tool token is checked.
            if tool.as_deref() != tool_token.as_deref() {
                let (want, got) = match (tool.as_deref(), tool_token.as_deref()) {
                    (Some(want), Some(got)) => (want, got),
                    (Some(want), None) => (want, "<none>"),
                    (None, Some(got)) => ("<none>", got),
                    (None, None) => ("<none>", "<none>"),
                };
                // HOST-ONLY (E0/E1)
                return Some(format!(
                    "invalid-args tool annotation differs: `{want}` vs `{got}`"
                ));
            }
            None
        }
        (ExpectedDecision::Malformed, ActualDecision::Malformed) => None,
        _ => Some("decision shape differs".to_owned()),
    }
}

/// The committed class of a decoded actual decision.
const fn actual_class(decision: &ActualDecision) -> DecisionClass {
    match decision {
        ActualDecision::Call { .. } => DecisionClass::Call,
        ActualDecision::Ask { .. } => DecisionClass::Ask,
        ActualDecision::Finish { .. } => DecisionClass::Finish,
        ActualDecision::Malformed => DecisionClass::Malformed,
        ActualDecision::InvalidArgs { .. } => DecisionClass::InvalidArgs,
    }
}

/// The fixture spelling of a decision class.
const fn decision_class_name(class: DecisionClass) -> &'static str {
    match class {
        DecisionClass::Call => "Call",
        DecisionClass::Ask => "Ask",
        DecisionClass::Finish => "Finish",
        DecisionClass::Malformed => "Malformed",
        DecisionClass::InvalidArgs => "InvalidArgs",
    }
}

// ---------------------------------------------------------------------------
// Rendering for diffs.
// ---------------------------------------------------------------------------

/// The fixture kind word of an expected event.
const fn expected_kind(event: &ExpectedEvent) -> &'static str {
    match event {
        ExpectedEvent::ModelDecision(_) => "ModelDecision",
        ExpectedEvent::ToolRequest { .. } => "ToolRequest",
        ExpectedEvent::ToolObservation { .. } => "ToolObservation",
        ExpectedEvent::Verification { .. } => "Verification",
        ExpectedEvent::ApprovalRequest { .. } => "ApprovalRequest",
        ExpectedEvent::ApprovalDecision { .. } => "ApprovalDecision",
        ExpectedEvent::TerminalResult { .. } => "TerminalResult",
    }
}

/// The trace kind word of an actual event.
const fn actual_kind(event: &ActualEvent) -> &'static str {
    match event {
        ActualEvent::ModelDecision(_) => "ModelDecision",
        ActualEvent::ToolRequest { .. } => "ToolRequest",
        ActualEvent::ToolObservation { .. } => "ToolObservation",
        ActualEvent::Verification { .. } => "Verification",
        ActualEvent::ApprovalRequest { .. } => "ApprovalRequest",
        ActualEvent::ApprovalDecision { .. } => "ApprovalDecision",
        ActualEvent::Terminal { .. } => "TerminalResult",
    }
}

/// One-line rendering of an expected event.
fn render_expected(event: &ExpectedEvent) -> String {
    match event {
        ExpectedEvent::ModelDecision(decision) => match decision {
            // HOST-ONLY (E0/E1)
            ExpectedDecision::Call { tool, args } => {
                format!("ModelDecision Call {tool} {}", args.render())
            }
            ExpectedDecision::Ask { prompt, schema } => {
                format!("ModelDecision Ask schema={schema} {prompt:?}")
            }
            ExpectedDecision::Finish { status, summary } => {
                format!("ModelDecision Finish {status} {summary:?}")
            }
            ExpectedDecision::Malformed => "ModelDecision Malformed".to_owned(),
            ExpectedDecision::InvalidArgs { .. } => "ModelDecision InvalidArgs".to_owned(),
        },
        // HOST-ONLY (E0/E1)
        ExpectedEvent::ToolRequest { tool, args } => {
            format!("ToolRequest {tool} {}", args.render())
        }
        ExpectedEvent::ToolObservation {
            tool, ok, payload, ..
        } => {
            // HOST-ONLY (E0/E1)
            format!(
                "ToolObservation {tool} ok={ok} {}",
                payload
                    .as_ref()
                    .map_or_else(|| "<none>".to_owned(), Value::render)
            )
        }
        ExpectedEvent::Verification {
            tool,
            expected,
            observed,
            passed,
            ..
        } => format!(
            // HOST-ONLY (E0/E1)
            "Verification {tool} passed={passed} expected={} observed={}",
            expected.render(),
            observed.render()
        ),
        ExpectedEvent::ApprovalRequest { prompt, schema } => {
            format!("ApprovalRequest schema={schema} {prompt:?}")
        }
        ExpectedEvent::ApprovalDecision { input, .. } => {
            format!("ApprovalDecision {}", input.render())
        }
        ExpectedEvent::TerminalResult {
            status,
            reason,
            summary,
        } => {
            format!("TerminalResult {} {reason:?} {summary:?}", status.name())
        }
    }
}

/// One-line rendering of an actual event.
fn render_actual(event: &ActualEvent) -> String {
    match event {
        ActualEvent::ModelDecision(decision) => match decision {
            // HOST-ONLY (E0/E1)
            ActualDecision::Call { tool, args } => {
                format!("ModelDecision Call {tool} {}", args.render())
            }
            ActualDecision::Ask { prompt, schema } => {
                format!(
                    "ModelDecision Ask schema={schema} {}",
                    String::from_utf8_lossy(prompt)
                )
            }
            ActualDecision::Finish { status, summary } => {
                format!(
                    "ModelDecision Finish {status} {}",
                    String::from_utf8_lossy(summary)
                )
            }
            ActualDecision::Malformed => "ModelDecision Malformed".to_owned(),
            ActualDecision::InvalidArgs { .. } => "ModelDecision InvalidArgs".to_owned(),
        },
        // HOST-ONLY (E0/E1)
        ActualEvent::ToolRequest { tool, args, .. } => {
            format!("ToolRequest {tool} {}", args.render())
        }
        ActualEvent::ToolObservation {
            tool, ok, payload, ..
        } => format!(
            // HOST-ONLY (E0/E1)
            "ToolObservation {tool} ok={ok} {}",
            payload
                .as_ref()
                .map_or_else(|| "<none>".to_owned(), Value::render)
        ),
        ActualEvent::Verification {
            tool,
            expected,
            observed,
            passed,
            ..
        } => format!(
            // HOST-ONLY (E0/E1)
            "Verification {tool} passed={passed} expected={} observed={}",
            expected.render(),
            observed.render()
        ),
        ActualEvent::ApprovalRequest { prompt, schema } => {
            format!(
                "ApprovalRequest schema={schema} {}",
                String::from_utf8_lossy(prompt)
            )
        }
        ActualEvent::ApprovalDecision { input } => {
            format!("ApprovalDecision {}", input.render())
        }
        ActualEvent::Terminal {
            status,
            reason,
            summary,
        } => format!(
            "TerminalResult {} {} {}",
            status.name(),
            String::from_utf8_lossy(reason),
            String::from_utf8_lossy(summary)
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{compare_event, tool_token, ActualEvent, CmpCtx};
    use crate::fixture::ExpectedEvent;
    use crate::json;

    fn tool_request(seq: u32, tool: &str, args_json: &str) -> ActualEvent {
        ActualEvent::ToolRequest {
            seq,
            // HOST-ONLY (E0/E1)
            tool: tool.to_owned(),
            args: json::parse(args_json).expect("valid test JSON"),
        }
    }

    fn want_request(tool: &str, args_json: &str) -> ExpectedEvent {
        ExpectedEvent::ToolRequest {
            // HOST-ONLY (E0/E1)
            tool: tool.to_owned(),
            args: json::parse(args_json).expect("valid test JSON"),
        }
    }

    #[test]
    fn deliberate_arg_mismatch_reports_both_sides() {
        let got = tool_request(7, "gpio_pin_write", r#"{"pin": 4, "level": "low"}"#);
        let actual = [got];
        let ctx = CmpCtx::new(&actual);
        let want = want_request("gpio_pin_write", r#"{"level": "high", "pin": 4}"#);
        let line = compare_event(3, &want, &actual[0], &ctx).expect("must mismatch");
        assert!(line.contains("event[3]"), "got: {line}");
        assert!(line.contains("args differ"), "got: {line}");
        // Both sides render, canonically ordered.
        assert!(
            line.contains(r#"ToolRequest gpio_pin_write {"level":"high","pin":4}"#),
            "got: {line}"
        );
        assert!(
            line.contains(r#"ToolRequest gpio_pin_write {"level":"low","pin":4}"#),
            "got: {line}"
        );
    }

    #[test]
    fn kind_mismatch_names_both_kinds() {
        let got = tool_request(7, "gpio_pin_write", r#"{"pin": 4, "level": "high"}"#);
        let actual = [got];
        let ctx = CmpCtx::new(&actual);
        let want = ExpectedEvent::TerminalResult {
            status: esper_core::state::TerminalStatus::Completed,
            // HOST-ONLY (E0/E1)
            reason: "model_finished".to_owned(),
            summary: "s".to_owned(),
        };
        let line = compare_event(0, &want, &actual[0], &ctx).expect("must mismatch");
        assert!(line.contains("kind differs"), "got: {line}");
        assert!(
            line.contains("`TerminalResult` vs `ToolRequest`"),
            "got: {line}"
        );
    }

    #[test]
    fn observation_with_unknown_seq_fails_loudly() {
        let request = tool_request(7, "gpio_pin_read", r#"{"pin": 4}"#);
        let observation = ActualEvent::ToolObservation {
            // HOST-ONLY (E0/E1)
            tool: "gpio_pin_read".to_owned(),
            seq: 99,
            ok: true,
            payload: Some(json::parse(r#"{"pin": 4, "level": "low"}"#).expect("valid")),
        };
        let actual = [request, observation];
        let ctx = CmpCtx::new(&actual);
        let want = ExpectedEvent::ToolObservation {
            // HOST-ONLY (E0/E1)
            tool: "gpio_pin_read".to_owned(),
            ok: true,
            payload: Some(json::parse(r#"{"pin": 4, "level": "low"}"#).expect("valid")),
            progress: None,
            same_effect_id_as_attempt: None,
        };
        let line = compare_event(1, &want, &actual[1], &ctx).expect("must mismatch");
        assert!(line.contains("seq 99"), "got: {line}");
        assert!(line.contains("no committed intent"), "got: {line}");
    }

    #[test]
    fn same_effect_id_links_the_right_attempt() {
        let request = tool_request(7, "gpio_pin_read", r#"{"pin": 4}"#);
        let first = ActualEvent::ToolObservation {
            // HOST-ONLY (E0/E1)
            tool: "gpio_pin_read".to_owned(),
            seq: 7,
            ok: false,
            payload: None,
        };
        let retry = ActualEvent::ToolObservation {
            // HOST-ONLY (E0/E1)
            tool: "gpio_pin_read".to_owned(),
            seq: 7,
            ok: true,
            payload: Some(json::parse(r#"{"pin": 4, "level": "low"}"#).expect("valid")),
        };
        let actual = [request, first, retry];
        let mut ctx = CmpCtx::new(&actual);
        // Walk the first observation so the retry sees it as earlier.
        ctx.observe_actual(&actual[1]);
        let want = ExpectedEvent::ToolObservation {
            // HOST-ONLY (E0/E1)
            tool: "gpio_pin_read".to_owned(),
            ok: true,
            payload: Some(json::parse(r#"{"pin": 4, "level": "low"}"#).expect("valid")),
            progress: None,
            same_effect_id_as_attempt: Some(1),
        };
        assert!(compare_event(2, &want, &actual[2], &ctx).is_none());
    }

    #[test]
    fn extracts_the_call_tool_token() {
        assert_eq!(
            tool_token(b"CALL gpio_pin_read {\"pin\": 4}").as_deref(),
            Some("gpio_pin_read")
        );
        assert_eq!(tool_token(b"ASK {\"prompt\": \"hi\", \"schema\": 1}"), None);
        assert_eq!(tool_token(b"do a flip"), None);
    }

    #[test]
    fn rejects_non_utf8_tool_tokens() {
        assert_eq!(tool_token(b"CALL \xff\xfe {}"), None);
    }
}
