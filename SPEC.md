# Esper E0+E1 executable specification

**Slice:** Rung E0 (executable specification) + Rung E1 (durable single loop),
built as one vertical slice per §16 of the design document.
**Status:** SPEC — no implementation exists. Golden trajectories in
`spec/trajectories/` are the failing fixtures the implementation crew turns
into red tests (§14).
**Authority order:** this spec > design doc (draft v0.1) where they conflict;
every conflict is logged in §15.

Design-doc shorthand: "design §N" refers to
`~/workspace/your_files/esper-harness-design/Esper Harness Design.md`.

---

## 1. Scope

### In scope (the slice proves)

1. `esper-core`: `Decision` decoder, resource budget, `ProgressDelta`,
   runtime monitor, terminal statuses. `no_std`, allocation-free.
2. `esper-runtime`: one ReAct workflow through Waymaker. Scripted model
   activity. Two tools against a fake device: `gpio_pin_read`,
   idempotent `gpio_pin_write`. Independent read-back verifier.
3. Waymaker integration: model decisions and tool observations recorded as
   bounded activity outcomes. Intent committed before any effect. Outcome
   recorded after.
4. Fault matrix: reset at every model, tool, and verification boundary.
   Every injected crash recovers a legal committed prefix, preserves
   budgets, and never dispatches an effect without durable intent.
5. Golden trajectories (§14): success, invalid output, denied pin,
   transient read failure, repeated identical failure, budget exhaustion,
   reset after physical write before committed outcome, Ask suspend/resume.

### Out of scope (deferred, with the rung that owns them)

| Deferred item | Rung | Slice behavior |
|---|---|---|
| Context compaction / `Compact` state / `continue_as_new` | E4 | Omitted from the state machine. `Gather` enforces a context budget guard; scripted contexts are bounded by construction, so the guard never fires in the slice. |
| Human approval flow (`AwaitApproval`) | E2+ | Omitted. No tool in the slice catalog has permission class `SensitiveWrite`; the capability set cannot express it. |
| `esper-protocol` crate (generated schemas/encodings) | E2 — built | The E2 contract source (§17): one table drives the validator, signatures, and JSON Schema. |
| Sensor, timer, status tools | E2 — built | Six-tool catalog (§17.2); tool ids 1–2 keep their slice assignments. |
| Network tools | E5 | Explicitly out of E2: no tool name, description, or argument suggests network access; an anti-network test guards the catalog (§17.2). |
| Remote model backend, MCP gateway | E3/E5 | Scripted backend only. |
| `SubgoalClosed` progress emission | E4 | Defined, never emitted in the slice (no explicit subgoal model yet). |
| Bounded parallel calls | E6 | Rejected: one call per turn, enforced by the decoder. |

### Rung E2 scope

E2 builds on the E0/E1 slice without changing its state machine, budget
model, or durability contract:

1. `esper-protocol`: the single contract source for the typed native tool
   catalog (§17) — one constant table driving the allocation-free
   validator, the compact model-facing signatures, the JSON Schema
   export, and the golden argument fixtures.
2. The catalog grows from two tools to six: `gpio_pin_read` (id 1) and
   `gpio_pin_write` (id 2) keep their E0/E1 ids; `sensor_sample_read`
   (3), `timer_uptime_read` (4), `timer_delay_wait` (5), and
   `device_status_report` (6) join.
3. Permission model v2 (§5.2): capability sets gain sensor, timer, and
   status grants; authorization is per-tool (allowlist → capability →
   device business rules), and denials still happen before dispatch.
4. The strict JSON parser moves from `esper-core` to `esper-protocol`
   (§17.4); `esper-core` re-exports it, so no E0/E1 code path changes
   meaning.
5. Generic network access is explicitly out of E2 and stays deferred to
   E5.

### Host-only versus firmware

The slice runs host-only. Firmware builds of `esper-core` and
`esper-runtime` must be allocation-free. The following parts are
**host-side, E0/E1-only, and must never enter a firmware image**; every
allocation site in them carries a `// HOST-ONLY (E0/E1)` marker:

- scripted model backend (script storage, output staging);
- fake GPIO device (pin table, fault plan);
- fault injector and crash harness;
- fixture runner (JSON parsing, trace comparison);
- trajectory scoring and eval reporting.

Enforcement: `esper-core` and `esper-runtime` are `#![no_std]` with no
`extern crate alloc`; a CI layering gate fails the build if `alloc`
appears in their dependency closure. `esper-eval` is host-only and no
firmware crate may depend on it.

---

## 2. Crates and layering

```text
esper/
  SPEC.md
  spec/trajectories/*.json        golden fixtures (the red tests)
  crates/
    esper-protocol/   the E2 contract source: tool table, validator, renderers (§17)
    esper-core/       pure types, decoder, budgets, monitor, policy gates
    esper-runtime/    ReAct workflow, Waymaker integration, scripted model,
                      fake device, verifier
    esper-eval/       host-only fixture runner, fault injector, scoring
```

Dependency direction (strict, CI-enforced):

```text
esper-eval --> esper-runtime --> esper-core --> esper-protocol
esper-eval --> esper-core
esper-runtime --> waymaker-core, waymaker-embassy, waymaker-flash (0.1.0, crates.io)
```

Rules:

- `esper-protocol` depends on nothing in the workspace: only `core`
  (plus `thiserror`, `no_std`-compatible). It owns the static tool
  table, the strict JSON parser (moved from `esper-core` in E2, §17.4),
  the argument validator, and the signature / JSON Schema renderers.
- `esper-core` depends on `esper-protocol` (for the shared `json`
  module), never the reverse: a protocol→core edge is a layering
  violation.

- `esper-core` depends on nothing but `core` and `esper-protocol`. No I/O, no clocks, no
  logging, no serialization framework, no Waymaker.
- `esper-runtime` depends only on `esper-core` and the Waymaker surfaces
  named in §10. It owns orchestration but not physical device authority
  (the fake device is an E0/E1 test double standing in for `esper-device`).
- `esper-eval` is host-only. It must never be linked into firmware.
- **Layering gate:** `esper-core` must not depend on `esper-runtime`
  (in either direction of the mistake). CI fails on violation.
- Strong ID newtypes everywhere (`RunId`, `EffectId`, `ToolId`,
  `Pin`, `Digest`). No `unwrap` in non-test code. No indexing in
  production paths (bounds-checked access with typed errors).

---

## 3. State machine

### 3.1 States

| State | Meaning |
|---|---|
| `Recover` | Validate run seed; replay the committed prefix. Entry state. |
| `Gather` | Rebuild budgets and working state from committed outcomes only; assemble the bounded context. |
| `Infer` | Run the model activity; decode bytes to `Decision`. |
| `Repair` | Bounded repair of malformed output or invalid arguments. Counts attempts. |
| `Authorize` | Permission policy and device business rules for a `Call`. Commits tool intent on allow. |
| `AwaitInput` | Durably suspended on `Ask`. Waits for typed human input. |
| `Observe` | Dispatch the tool; normalize and bound the observation; commit it. Retries transient failures with the same effect identity. |
| `Verify` | Independent read-back of a mutating tool's target state. Commits the verification result. |
| `Account` | Apply budget accounting, loop detection, and progress delta from the committed trajectory. |
| `Finalize` | Model chose `Finish`. Commit the terminal result. |
| `Degraded` | A guard fired. Commit the terminal result with the failing status. |
| `SafeStop` | Journal corrupt or incompatible at `Recover`. Best-effort terminal through reserved capacity. |
| `End` | Workflow final state. Exactly one logical terminal result precedes it. |

### 3.2 Transition table

`-->` shows the committed Waymaker record, if any. Deterministic checks
(parsing, validation, accounting, prompt assembly) are workflow code and
are never separate effects.

| From | Condition | To | Committed record |
|---|---|---|---|
| `Recover` | journal valid; workflow/model/catalog/policy versions and hashes validate | `Gather` | — |
| `Recover` | journal corrupt or version-incompatible | `SafeStop` | best-effort `TerminalResult` |
| `Gather` | budgets and guards clear | `Infer` | `ModelRequest` (inference intent) |
| `Gather` | a budget already exhausted, or version binding invalid at resume | `Degraded` | `TerminalResult` (`BudgetExhausted` / `Incompatible`) |
| `Infer` | bytes decode to `Call` | `Authorize` | `ModelDecision` (class `Call`) |
| `Infer` | bytes decode to `Ask` | `AwaitInput` | `ModelDecision` (class `Ask`) + `ApprovalRequest` |
| `Infer` | bytes decode to `Finish` | `Finalize` | `ModelDecision` (class `Finish`) |
| `Infer` | bytes malformed or args violate the static schema | `Repair` | `ModelDecision` (class `Malformed` or `InvalidArgs`; raw bytes never stored) |
| `Repair` | `repairs_used < 2` | `Infer` | — (`repairs_used` is derived from committed malformed/invalid-arg decisions; defined below) |
| `Repair` | `repairs_used >= 2` | `Degraded` | `TerminalResult` (`ModelInvalid`) |
| `Authorize` | allowlisted, permission granted, device business rules pass; intent committed | `Observe` | `ToolRequest` (stable `EffectId` + args digest) |
| `Authorize` | permission or policy denied, or illegal device state | `Degraded` | `TerminalResult` (`Denied`) |
| `AwaitInput` | typed input arrives and validates against the response schema | `Account` | `ApprovalDecision` |
| `Observe` | dispatch returned; observation normalized | `Verify` | `ToolObservation` — only if the tool is mutating and the observation class is `ok` |
| `Observe` | dispatch returned; observation normalized | `Account` | `ToolObservation` — read-only tools, or mutating tools whose observation class is not `ok` |
| `Observe` | transient failure and `transient_attempts(effect) < 2` | `Observe` | `ToolObservation` (class `Transient`), same `EffectId` |
| `Observe` | transient attempts exhausted | `Degraded` | `ToolObservation` (class `Transient`) then `TerminalResult` (`ToolUnavailable`) |
| `Verify` | read-back matches expected state | `Account` | `VerificationResult` (`pass`) |
| `Verify` | read-back mismatches | `Account` | `VerificationResult` (`fail`, expected vs observed) |
| `Account` | all guards clear | `Gather` | — |
| `Account` | any guard fired | `Degraded` | `TerminalResult` (status per §8) |
| `Finalize` | — | `End` | `TerminalResult` (`Completed`) |
| `Degraded` | — | `End` | `TerminalResult` (failing status + reason) |
| `SafeStop` | — | `End` | best-effort `TerminalResult` (`StorageFault` / `Incompatible`) |

Repair budget semantics: the default repair allowance is 2 (design §5).
`repairs_used` is derived, not stored: it equals the count of committed
`ModelDecision` records with class `Malformed` or `InvalidArgs` in this
run. The third invalid output therefore transitions `Repair → Degraded`
with status `ModelInvalid`. A repair turn re-invokes the model with a
bounded structured error (§4.4); each re-invocation consumes one
`model_turn` and its output bytes (§6).

`AwaitInput` suspension: the run stays in `AwaitInput` until a typed
input arrives; suspension is durable (the committed `ApprovalRequest`
is the proof). No timeout, no terminal transition from waiting. A reset
while suspended recovers to `AwaitInput`, never to `Gather`.

### 3.3 Illegal transitions

Any transition not present in §3.2 is illegal and must be rejected by
construction (the transition function is total over `(State, Event)` and
returns a typed error for unlisted pairs). The following are explicitly
called out because each is a real safety hazard:

- `Infer → Observe` (skips authorization — the cardinal bypass);
- `Authorize → Infer`, `Authorize → Gather` (no re-validation loops);
- `Observe → Infer`, `Observe → Authorize`, `Verify → Observe`
  (no backward edges that could double-dispatch);
- `Account → Authorize`, `Account → Observe`, `Account → Verify`,
  `Account → Infer` (accounting must complete before the next turn);
- `Gather → Observe`, `Gather → Verify`, `Gather → Account`;
- `Repair → Observe`, `Repair → Authorize`, `Repair → Finalize`;
- `AwaitInput → Gather` (input must be committed first),
  `AwaitInput → Infer`, `AwaitInput → Observe`;
- `Recover →` anything except `Gather` / `SafeStop`;
- `Finalize`, `Degraded`, `SafeStop →` anything except `End`;
- `End →` anything (a new run requires a new `RunId` and `RunSeed`);
- **any dispatch of inference, tool, or verification without a
  preceding committed intent record** (see §10).

### 3.4 Terminal states

`End` is the single workflow-final state. The outcome is carried by one
`TerminalResult` with a status from §8. The terminal result is emitted
once logically: redelivery after a crash replays the committed record
and must not produce a second logical terminal (§11, crash oracle).

## 4. `Decision` type and scripted-model contract

### 4.1 The `Decision` union

Exactly three outcomes per turn (ADR: one call per turn; design §3):

```rust
pub enum Decision<'a> {
    Call(ToolCall<'a>),
    Ask(InputRequest<'a>),
    Finish(FinalAnswer<'a>),
}

pub struct ToolCall<'a> {
    pub tool: ToolId,          // stable numeric id, not a name string
    pub args: ToolArgs,        // typed dispatch shape (E2, §17.5; Copy)
    pub args_bytes: &'a [u8],  // raw validated JSON bytes, borrowed
    pub args_digest: Digest,   // computed at decode time
}

pub struct InputRequest<'a> {
    pub prompt: &'a [u8],            // bounded, max 256 bytes
    pub response_schema_id: u8,     // slice defines schema 1 = PinSelect
}

pub struct FinalAnswer<'a> {
    pub status: FinishStatus,  // slice: only `Completed`
    pub summary: &'a [u8],     // bounded, max 256 bytes
}
```

No chain of thought is persisted (ADR). The model may reason internally;
the durable contract is the typed decision plus its outcome.

### 4.2 Scripted backend contract (E0/E1 model stand-in)

The slice has no real model. The scripted backend is a deterministic
test double with this contract:

- **Input:** a script: an ordered list of output lines, one per committed
  inference turn, indexed by turn number. `script[turn]` must return
  byte-identical output on every call, including after reboot replay.
- **Output:** writes bytes into a caller-owned fixed buffer
  (max 512 bytes). Never allocates.
- **Token accounting:** `output_tokens` charged = bytes emitted;
  `input_tokens` charged = bytes of the assembled context for the turn.
  Both are deterministic.
- **Replay rule:** committed `ModelDecision` outcomes replay; the
  backend is never re-invoked for a committed turn. Re-invocation with
  the same turn index after a crash (before the decision commits) must
  return the same bytes — this is what makes crash recovery deterministic.

The scripted backend exists so the harness is testable without inference
hardware (design §1, goal 6). It lives in `esper-runtime` (host build)
and is marked `HOST-ONLY (E0/E1)`.

### 4.3 Output grammar

One line per turn. Strict; anything else is malformed.

```text
CALL <tool_name> <json-args>
ASK  <json>
FINISH <json>
```

Rules:

- `<tool_name>` is the canonical registry name (`gpio_pin_read`,
  `gpio_pin_write`). Unknown names are rejected at decode
  (class `Malformed`).
- `<json-args>` / `<json>` is strict JSON, max 256 bytes, no trailing
  bytes after the closing brace, no duplicate keys. Depth max 3.
- `ASK` JSON shape: `{"prompt": "<<=256B>", "schema": 1}`.
- `FINISH` JSON shape: `{"status": "completed", "summary": "<<=256B>"}`.
- Exactly one call per turn: a second `CALL` on the line, or any
  `CALL` inside `ASK`/`FINISH` JSON, is malformed.
- All bounds are compile-time constants in `esper-core`.

Rationale for a line grammar over full JSON documents: the decoder is
fixed-buffer and allocation-free; tool args reuse the same strict JSON
validator the registry uses. The design's open encoding question (§14)
is deferred to E2/`esper-protocol`; this grammar is the slice's answer.

### 4.4 Repair protocol

Malformed output becomes a bounded structured error fed back to the
model on the repair turn. It contains exactly: variant (`malformed` |
`invalid_args`), field, expected constraint, received category.
It never contains a parser trace, raw model bytes, or secrets.

Repair turns are ordinary model turns: they consume `model_turn` budget
and their output re-enters `Infer`. After the allowance is exhausted
(`repairs_used >= 2`), the run terminates `ModelInvalid` with a bounded
summary — a clean termination, never a hang, never a silent drop.

### 4.5 Decoder validation split

- **Decoder (`Infer`, in `esper-core`):** envelope grammar, tool name
  known, args are well-formed JSON within bounds, args satisfy the
  tool's static schema (types, ranges, required fields). Violations
  consume repair allowance (§3.2).
- **Authorize (`esper-runtime`):** permission policy from the run's
  capability set and device business rules (e.g. pin direction). Denial
  is terminal `Denied`, never a repair turn (see §15, ambiguity 1).

---

## 5. Tool registry contract

### 5.1 Static catalog

The catalog is fixed at build time (ADR: static capability catalog).
The E0/E1 slice catalog held exactly two entries. Rung E2 extends it to
six (§17.2); tool ids 1–2 keep their slice assignments and are never
renumbered:

| # | `ToolId` | Name | Permission class | Args schema | Result bound | Verification | Idempotency |
|---|---|---|---|---|---|---|---|
| 1 | `1` | `gpio_pin_read` | `ReadOnly` | `{"pin": u8 0..=7}` | 64 B | `None` (read-only) | n/a (no effect) |
| 2 | `2` | `gpio_pin_write` | `IdempotentWrite` | `{"pin": u8 0..=7, "level": "low"\|"high"}` | 64 B | `ReadBack` | set-operation on `(pin)` keyed by stable `EffectId` |
| 3 | `3` | `sensor_sample_read` | `ReadOnly` | `{"sensor": u8 0..=3}` | 64 B | `None` (read-only) | n/a (no effect) |
| 4 | `4` | `timer_uptime_read` | `ReadOnly` | `{}` (empty object valid) | 64 B | `None` (read-only) | n/a (no effect) |
| 5 | `5` | `timer_delay_wait` | `IdempotentWrite` | `{"ms": u16 1..=5000}` | 64 B | `ReadBack` | set-operation: the effect is "clock ≥ t0+ms"; redelivery under the same `EffectId` never double-advances |
| 6 | `6` | `device_status_report` | `ReadOnly` | `{"detail": "summary"\|"full"}` | 128 B | `None` (read-only) | n/a (no effect) |

Each registry entry contains: `ToolId`, name, schema version, compact
model-facing description, the argument schema (the `ArgSpec` table the
validator, the compact signature, and the JSON Schema all derive from),
golden `example_ok` / `example_bad` argument fixtures, permission class,
business-rule validator, dispatch function, result bound, verification
strategy, idempotency strategy.

Naming follows `domain_resource_verb`. `gpio_pin_write` is a set
operation, never a toggle: re-dispatch with the same arguments is safe
(ADR: at-least-once effects).

### 5.2 Permission model (v2, rung E2)

The `RunSeed` carries a fixed capability set:

```rust
pub struct Capabilities {
    pub read_pins: [u8; 8], pub read_count: u8,
    pub write_pins: [u8; 8], pub write_count: u8,
    pub sensors: [u8; 4], pub sensor_count: u8,
    pub allow_timer: bool,
    pub allow_status: bool,
}
```

(The E0/E1 struct held only the two pin sets; `sensors`,
`allow_timer`, and `allow_status` are the E2 extension. The count
discipline is unchanged: counts beyond the storage fail closed wherever
they are read.)

Authorization rule for a `Call`:

1. Tool allowlisted in the current workflow phase (E2: all six tools,
   all phases).
2. Arguments validate against the tool's contract (decoder, §4.5;
   violations consume repair allowance, never terminal).
3. Capability check, by tool:
   - `gpio_pin_read`: `pin` ∈ `read_pins`;
   - `gpio_pin_write`: `pin` ∈ `write_pins` **and** the device reports
     the pin as output-capable;
   - `sensor_sample_read`: `sensor` ∈ `sensors`;
   - `timer_uptime_read`, `timer_delay_wait`: `allow_timer`;
   - `device_status_report`: `allow_status`.
4. Device business rules for the tool's resource (pin direction for
   writes, sensor present, clock sane). Both capability and device truth
   must hold; the model cannot grant itself resources.
5. The capability set is part of run identity: it can never widen after
   reboot (§10, crash oracle). Absent fields in a fixture (`sensors`,
   `allow_timer`, `allow_status`) mean denied — fail closed (§17.7).

A refusal fails **before dispatch** with terminal status `Denied`
(fixture c pattern). The check order above is normative: allowlist,
then contract validation, then capability, then device truth, so
denials never touch hardware.

Permission classes: `ReadOnly`, `IdempotentWrite`, `SensitiveWrite`,
`Irreversible`. The latter two still have no E2 tool and no grant path,
so `AwaitApproval` stays unreachable.

Denial reason vocabulary (normative terminal reason bytes): keep
`pin_not_in_write_capabilities`, `pin_not_in_read_capabilities`,
`pin_direction_denied`; add `sensor_not_in_capabilities`,
`timer_not_permitted`, `status_not_permitted`.

### 5.3 Result envelope

Every tool returns the same bounded envelope:

```text
outcome class   ok | error
error class     one of §7 (when error)
payload         compact bytes, <= result bound
truncated       bool  (slice tools never truncate; bound is enforced)
retry class     none | transient | permanent
```

Tool adapters translate device errors into the §7 vocabulary. They never
return unbounded logs or raw internals.

---

## 6. Error vocabulary

Stable numeric codes; unknown codes fail closed.

| Code | Name | Meaning | Retry | Consumes |
|---|---|---|---|---|
| 0 | `Ok` | success | — | — |
| 1 | `InvalidArgs` | args violate the static schema; exact violations returned | never; repair turn | repair allowance |
| 2 | `Denied` | permission/policy refused, or illegal device state | never; terminal | run ends |
| 3 | `ApprovalRequired` | reserved; unreachable in the slice | — | — |
| 4 | `Transient` | transport/device hiccup | yes, ≤2, same `EffectId` | transient attempts |
| 5 | `Permanent` | deterministic failure; model may choose another legal action | never | — |
| 6 | `VerificationFailed` | read-back mismatch; expected vs observed recorded | via model retry, monitor-bounded | error counters |
| 7 | `OutputExhausted` | adapter exceeded its declared result bound | never | — |
| 8 | `ModelMalformed` | decoder rejection | repair turn | repair allowance |
| 9 | `BudgetExceeded` | a resource guard fired | never; terminal | run ends |
| 10 | `StorageFault` | durable state untrustworthy | never; terminal | run ends |

---

## 7. Resource budget model

### 7.1 Units

```rust
pub struct ResourceBudget {
    pub model_turns: u16,        // inference activities, incl. repairs
    pub input_tokens: u32,      // context bytes assembled per turn
    pub output_tokens: u32,     // model output bytes per turn
    pub elapsed_ms: u64,        // host monotonic clock (see §7.3)
    pub radio_bytes: u32,       // slice: always 0; field exists for E5
    pub mutations: u16,         // mutating tool intents committed
    pub consecutive_errors: u8, // resets on any committed success
}
```

### 7.2 Identity and accounting rules

- The budget is part of run identity: it is committed in the
  `RunSeed` and **can never widen after reboot** (ADR 10; crash oracle
  asserts this at every crash point).
- `Account` decrements counters from committed outcomes only.
  A `model_turn` is consumed per committed `ModelDecision` with class
  `Call`/`Ask`/`Finish`/`Malformed`/`InvalidArgs` — repairs included.
  A `mutation` is consumed per committed mutating `ToolRequest`.
- Exhaustion is checked at `Gather` (resume) and at `Account`
  (after each turn). Firing any guard transitions to `Degraded` with
  `BudgetExhausted` and a reason naming the exhausted unit.
- Deployment values are explicit in `RunSeed`, never hidden constants.
  Fixture defaults are test values, not protocol values.

### 7.3 Clock semantics

`elapsed_ms` uses the host monotonic clock in the slice. Esper makes no
claim about elapsed time across power loss (Waymaker durable-time rule:
no persistent clock, no offline elapsed-time claim). A firmware clock
backend is deferred with `esper-device`.

---

## 8. Progress signal and terminal statuses

### 8.1 `ProgressDelta`

Emitted per committed step from the trajectory, never from model prose
(a model cannot mark its own action successful):

| Variant | Fires when |
|---|---|
| `NewEvidence` | a read returned a new fact or state value |
| `StateChanged` | a verified mutation changed the world as intended |
| `SubgoalClosed` | reserved; not emitted in the slice (E4) |
| `InputReceived` | requested human input arrived and committed |
| `NoProgress` | none of the above |

### 8.2 Terminal statuses

```rust
pub enum TerminalStatus {  // numeric codes stable
    Completed = 0,       // objective met (model's Finish; harness-verified steps)
    NeedsInput = 1,      // required human data unavailable (reserved E2+)
    Denied = 2,          // policy or permission refused the action
    BudgetExhausted = 3, // a deterministic resource guard fired
    Stuck = 4,           // loop detector found no useful progress
    ToolUnavailable = 5, // required capability remained unavailable
    ModelInvalid = 6,    // repair allowance exhausted
    StorageFault = 7,    // durable state could not be trusted
    Incompatible = 8,    // version/hash binding cannot replay safely
}
```

Every terminal carries `TerminalResult { status, reason: [u8; 128],
summary: [u8; 256] }` — a machine-readable reason plus a bounded
user-facing summary. Only `Completed` claims the objective was met.
In the slice, `NeedsInput` is defined but never emitted (no path
produces it yet).

### 8.3 Runtime monitor (loop and guard rules)

The monitor owns the `Account → Degraded` edge. Initial syntactic rules
(design §9; tuning defaults, changeable only against trajectory data):

- three identical `(tool_id, args_digest, error_class)` failures → `Stuck`;
- more than half of the last five committed events are errors → `Stuck`;
- five consecutive `NoProgress` deltas → `Stuck`;
- any hard budget limit reached → `BudgetExhausted`;
- a denied permission attempt is terminal `Denied` immediately
  (via `Authorize`, §3.2).

The monitor consumes committed outcomes and `ProgressDelta` values. It
never inspects model prose.

## 9. Verifier contract

For `gpio_pin_write(pin, level)` the declared verification strategy is
`ReadBack` (ADR 7: a mutating tool cannot certify its own success).

### 9.1 Independence rule

The verifier is a **separate activity and a separate code path** from
the write adapter:

- it must not share mutable state with the write adapter; the only
  shared surface is the device itself;
- it observes the pin through a read-only device handle (the same
  `gpio_pin_read` semantics, invoked by the harness, not by the tool);
- it compares expected state (`level` from the committed `ToolRequest`)
  against observed state and commits `VerificationResult { pass |
  fail { expected, observed } }` as its own Waymaker outcome.

A verifier that reads a flag the writer set is not a verifier. The
fake device exposes no such flag; the read path samples the pin table.

### 9.2 Failure policy

A failed verification records expected versus observed state and
surfaces it to the model as an observation with error class
`VerificationFailed` (`Verify → Account → Gather → Infer`). The slice
defines **no automatic compensation** — Esper never invents rollback
for a physical action. The model may retry the idempotent write; the
monitor's three-identical-failures rule bounds the retries and ends
the run `Stuck` (fixture e). This is the slice's explicit policy under
design §7's "stop or compensate according to explicit policy".

### 9.3 Ordering guarantee

A mutation is never reported complete without its declared verifier:
`Observe → Verify` is mandatory for mutating tools with observation
class `ok`, and `Finalize`/`Degraded` must not emit `Completed` for a
run whose last mutation lacks a committed passing `VerificationResult`.
The crash oracle asserts this (§11).

---

## 10. Waymaker integration contract

Waymaker crates: `waymaker-core`, `waymaker-embassy`, `waymaker-flash`,
all `0.1.0`, from crates.io. **No path dependencies.** `esper-runtime`
uses only Waymaker's public run/activity/journal surfaces; Esper adds
no authority to the Waymaker wire format.

### 10.1 Logical records

Esper payloads live inside Waymaker's existing schedule and outcome
records. Each has a stable numeric kind, an explicit version, a
compile-time maximum length, and a canonical byte encoding. Strings are
bounded UTF-8; IDs are newtypes; unknown enum values fail closed.
Raw provider responses, secrets, stack traces, and chain of thought are
never journal payloads.

| Kind | Code | Max | Committed at |
|---|---|---|---|
| `RunSeed` | 1 | 512 B | run creation: `workflow_version`, `model_bundle_hash`, `tool_catalog_hash`, `policy_hash`, `ResourceBudget`, `Capabilities` |
| `ModelRequest` | 2 | 256 B | `Gather → Infer`: inference intent (turn index, context digest) |
| `ModelDecision` | 3 | 512 B | `Infer`: typed decision outcome, class `Call`/`Ask`/`Finish`/`Malformed`/`InvalidArgs` |
| `ToolRequest` | 4 | 256 B | `Authorize → Observe`: tool intent, stable `EffectId`, args digest |
| `ToolObservation` | 5 | 512 B | `Observe`: bounded outcome, error class, retry class |
| `VerificationRequest` | 6 | 128 B | `Observe → Verify`: expected state for read-back |
| `VerificationResult` | 7 | 128 B | `Verify`: pass/fail with expected vs observed |
| `ApprovalRequest` | 8 | 320 B | `Infer → AwaitInput`: the `Ask` prompt + response schema id |
| `ApprovalDecision` | 9 | 320 B | `AwaitInput → Account`: typed human input |
| `TerminalResult` | 10 | 512 B | `Finalize`/`Degraded`/`SafeStop → End` |

### 10.2 Commit boundaries (normative)

For a model turn:

```text
commit ModelRequest intent
--> run inference (scripted backend, caller-owned buffers)
--> decode to bounded Decision (deterministic workflow code)
--> commit ModelDecision outcome
--> expose Decision to the workflow
```

For a tool call:

```text
commit ToolRequest intent with stable EffectId and argument digest
--> dispatch tool with that EffectId as idempotency key
--> normalize to the bounded observation envelope
--> commit ToolObservation outcome
--> expose observation to the workflow
```

For a mutation:

```text
committed ToolObservation (class ok)
--> commit VerificationRequest, run independent read-back
--> compare expected and observed state
--> commit VerificationResult
```

Intent is committed **before** any dispatch — including read-only
tools. No effect is dispatched without durable intent (§3.3).

### 10.3 Effect identity and idempotency

- `EffectId = (RunId, EffectSeq)`. `EffectSeq` increments per tool or
  verification activity. Redelivery after a crash reuses the original
  `EffectId`; it is never re-minted.
- The fake device deduplicates on `EffectId`: a redelivered
  `gpio_pin_write` with a seen id is acknowledged without a second
  physical change (and the write is a set-operation, so reapplication
  is safe regardless).
- Transient retries reuse the same `EffectId`; each attempt commits
  its own `ToolObservation` so the retry count is derivable after
  reboot (§3.2).

### 10.4 Resume rules

On boot, `esper-runtime`:

1. lets Waymaker recover the single authoritative committed prefix;
2. validates `workflow_version`, `model_bundle_hash`,
   `tool_catalog_hash`, `policy_hash` from the `RunSeed`;
3. rebuilds budgets, capabilities, and working state by replaying the
   workflow from the start over committed outcomes only;
4. redelivers only the first unresolved effect with its original
   identity.

A version or hash mismatch never silently changes semantics: the run
goes `Recover → SafeStop` with `Incompatible` (or `StorageFault` for a
corrupt journal).

### 10.5 Budgets, permissions, versions after reboot

They are run identity. After reboot they are restored from the
`RunSeed` and the committed outcomes, never reset, never widened.
The crash matrix asserts this at every crash point (§11).

---

## 11. Fault-injection contract

### 11.1 Crash points

A reset may be injected at each of these points, and the slice must
recover correctly from all of them:

| ID | Point |
|---|---|
| `cp_model_intent` | after `ModelRequest` commit, before inference |
| `cp_before_decision_commit` | after inference, before `ModelDecision` commit |
| `cp_before_authorize` | after `ModelDecision` commit, before `Authorize` |
| `cp_tool_intent` | after `ToolRequest` commit, before tool dispatch |
| `cp_after_physical_before_observation` | after the physical effect, before `ToolObservation` commit |
| `cp_after_observation_before_verify` | after `ToolObservation` commit, before verification |
| `cp_before_verification_commit` | after read-back, before `VerificationResult` commit |
| `cp_after_verification` | after `VerificationResult` commit, before next `Gather` |
| `cp_await_input` | while suspended in `AwaitInput`, before input arrives |
| `cp_before_terminal_commit` | after the terminal decision, before `TerminalResult` commit |
| `cp_after_terminal_commit` | after `TerminalResult` commit (redelivery path) |

### 11.2 Legal committed prefix

The legal committed prefix is the longest prefix of the activity
sequence in which every entry is a fully committed Waymaker outcome
and every outcome is preceded by its committed intent. "Legal" means:
replaying the deterministic workflow against exactly those outcomes
reaches a state whose next action is either (a) redelivery of the
first unresolved effect with its original `EffectId`, or (b) a fresh
effect whose intent is committed before dispatch. Anything past the
prefix — partial writes, uncommitted observations, volatile counters —
is discarded, never reconstructed from memory.

### 11.3 What `recover` must guarantee

1. No effect is dispatched without durable intent.
2. Committed outcomes replay byte-identically.
3. The first unresolved effect retains its original identity.
4. Budgets never increase after reboot; permissions never widen;
   version bindings never change.
5. A mutation is never reported complete without its committed
   passing verifier.
6. The terminal result is emitted once logically, even if its delivery
   is retried (`cp_after_terminal_commit` replays the committed
   `TerminalResult`; no second logical terminal).
7. `AwaitInput` suspension survives reset: the run resumes waiting,
   and a late input still commits exactly one `ApprovalDecision`.

### 11.4 Crash oracle

The `esper-eval` fault harness asserts §11.3 at every crash point in
§11.1, for every golden fixture. The matrix is green only if all
points pass on all fixtures. Missing measurement or injection tooling
fails the gate rather than reporting success.

## 12. CI gates

All gates run on every commit. A gate that cannot run fails the build;
it never reports success by omission.

1. `cargo fmt --check` clean.
2. `cargo clippy` with `pedantic` + `nursery` lints: zero warnings.
3. Test suite green (`cargo test`, all crates).
4. Coverage ≥ 85% on `esper-core` and `esper-runtime`
   (Mark's quality bar; `cargo-tarpaulin` or equivalent).
5. Layering gate: `esper-core` has no dependency path to
   `esper-runtime`; `esper-eval` is never in a firmware dependency
   closure; `alloc` is absent from the `esper-core`/`esper-runtime`
   closures. Implemented as an `xtask`-style check or
   `cargo-deny`-like graph assertion.
6. Size budget: `esper-core` + `esper-runtime` firmware builds report
   RAM (static) and code flash; the slice records the numbers in CI
   output. Hard caps are set at E3 (target board); the slice gate is
   measure-and-report, failing only if measurement tooling is absent.
7. Crash matrix green: §11.4 oracle passes at every crash point on
   every golden fixture.
8. No `TODO`/`FIXME`/stub markers in `esper-core`/`esper-runtime`
   (`grep` gate; Mark's pre-commit checklist).
9. Doc comments on all public APIs; Simplified Technical English.

---

## 13. ADRs honored

Design §13 decisions and where this spec enforces them:

1. **Single-agent default** — one workflow, one decision per turn; no
   multi-agent machinery anywhere in the slice.
2. **One call per turn** — decoder rejects multi-call output (§4.3);
   `ToolCall` holds a single `ToolId`.
3. **Model output is a durable activity result** — `ModelDecision`
   commits before exposure; replay never re-asks the model (§10.2).
4. **No persisted chain of thought** — journal vocabulary (§10.1)
   has no field for it; repair errors are structured, never traces.
5. **Static capability catalog** — build-time registry (§5.1);
   `tool_catalog_hash` in the `RunSeed`.
6. **MCP stays behind a curated gateway** — no MCP surface in the
   slice at all.
7. **Verification is independent** — §9: separate activity, separate
   code path, no shared mutable state with the tool adapter.
8. **Mask before compact** — compaction is E4; nothing to honor yet
   except not building summarization first.
9. **At-least-once effects** — intent-before-dispatch, stable
   `EffectId`, idempotent set-operations, device-side dedup (§10.3).
10. **Resource budgets are part of run identity** — `RunSeed`-bound,
    never widened after reboot (§7.2, §10.5, §11.3).

---

## 14. Golden trajectory fixtures

### 14.1 Format

Fixtures live in `spec/trajectories/*.json`, one file per trajectory.
JSON is chosen because the host-only fixture runner parses it once at
test time (a `HOST-ONLY (E0/E1)` allocation, never firmware code).

```jsonc
{
  "id": "success-read-write-verify-finish",
  "spec_version": 1,
  "title": "Read, idempotent write, verify, finish",
  "narrative": "What this trajectory proves, in one paragraph.",
  "run_seed": {
    "workflow_version": 1,
    "model_bundle": "scripted:success-v1",
    "tool_catalog_hash": "test-catalog-v1",
    "policy_hash": "test-policy-v1",
    "budget": {
      "model_turns": 10, "input_tokens": 4000, "output_tokens": 1000,
      "elapsed_ms": 60000, "radio_bytes": 0,
      "mutations": 4, "consecutive_errors": 3
    },
    "capabilities": { "read_pins": [4], "write_pins": [4] }
  },
  "script": [
    "CALL gpio_pin_read {\"pin\": 4}",
    "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
    "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}"
  ],
  "device": {
    "pins": { "4": { "direction": "out", "level": "low" } },
    "faults": []
  },
  "input_events": [],
  "crash_plan": [ { "at": "cp_tool_intent", "times": 1 } ],
  "expected": {
    "terminal_status": "Completed",
    "trace": [
      { "kind": "ModelDecision", "class": "Call", "tool": "gpio_pin_read",
        "args": { "pin": 4 } },
      { "kind": "ToolRequest", "tool": "gpio_pin_read",
        "args": { "pin": 4 } },
      { "kind": "ToolObservation", "tool": "gpio_pin_read",
        "class": "ok", "payload": { "pin": 4, "level": "low" },
        "progress": "NewEvidence" },
      "..."
    ],
    "budget_remaining": { "model_turns": 7, "mutations": 3 },
    "invariants": [
      "no_dispatch_without_intent",
      "budgets_never_widen",
      "effect_identity_stable",
      "single_logical_terminal",
      "mutation_never_complete_without_verifier"
    ],
    "forbidden": [ "second physical write to pin 4" ]
  }
}
```

Field rules:

- `script[i]` is the model output for inference turn `i`
  (0-indexed). Repair turns consume script entries too: the entry
  after a malformed line is the model's repair attempt.
- `device.pins`: the fake device's 8 pins (`0`–`7`); unlisted pins
  default to `{direction: "in", level: "low"}`.
- `device.faults`: deterministic device behaviors, e.g.
  `{"tool": "gpio_pin_read", "match_args": {"pin": 4},
  "failures_before_success": 1, "class": "transient"}` or
  `{"tool": "gpio_pin_write", "match_args": {"pin": 5},
  "stuck_level": "low"}` (write reports ok; read-back stays low).
- `input_events`: `[{"after_decision": n, "input": {...}}]` — typed
  human input delivered after model decision `n` (for `Ask`).
- `crash_plan`: resets injected at the §11.1 points; after each reset
  the runner replays from the Waymaker journal and continues.
- `expected.trace`: the exact committed record sequence. The runner
  compares kinds, classes, tools, args (canonicalized), payloads,
  progress deltas, and terminal status/reason. `EffectId`s are
  asserted stable across crashes, not asserted to specific values.
- `expected.budget_remaining`: exact assertions only for
  `model_turns` and `mutations`; token/time fields are asserted
  monotone-decreasing by the runner automatically.
- `expected.invariants`: the §11.3 oracle properties the runner must
  check on this fixture (subset by relevance).
- `expected.forbidden`: human-readable prohibitions the runner
  encodes as negative assertions.

### 14.2 The fixtures

| File | Proves |
|---|---|
| `a-success-read-write-verify-finish.json` | read → idempotent write → independent verify → `Completed`; one crash before tool dispatch redelivers with the same `EffectId` |
| `b-invalid-output-repair-then-modelinvalid.json` | malformed lines → two bounded repair turns → clean `ModelInvalid` termination; no tool ever dispatched |
| `c-denied-pin-fails-before-dispatch.json` | write to a pin outside `write_pins` → terminal `Denied`; the device is never touched |
| `d-transient-read-failure-retry-success.json` | transient read failure → retry with the same `EffectId` → success; both attempts committed |
| `e-repeated-failure-then-stuck.json` | deterministic verification failure three times → monitor fires → terminal `Stuck` |
| `f-budget-exhaustion-durable-terminal.json` | `model_turns` exhausted mid-run → durable `BudgetExhausted` terminal; prefix preserved |
| `g-reset-after-write-before-outcome.json` | reset after the physical write but before `ToolObservation` commits → recovery redelivers with the original intent, dedups on `EffectId`, verifies, completes |
| `h-ask-suspend-resume.json` | `Ask` suspends durably, survives a reset while suspended, resumes with typed input, completes |

### 14.3 How the fixtures become red tests

The implementation crew builds the `esper-eval` fixture runner, which
for each fixture:

1. parses the JSON (host-only);
2. constructs the `RunSeed`, scripted backend, fake device (with the
   fault plan), and crash injector;
3. drives `esper-runtime` to `End`, injecting each planned reset and
   replaying from the journal;
4. captures the committed record sequence and final budgets;
5. asserts equality with `expected.trace`, the budget assertions, every
   listed invariant, and every `forbidden` clause.

Each fixture becomes one `#[test]` (e.g.
`trajectory_a_success_read_write_verify_finish`). Before the runtime
exists, every test fails — that is the RED. The tests turn green only
when the runtime honors the full contract: state machine, budgets,
verifier independence, intent-before-dispatch, and crash recovery.
New production incidents later become new fixtures (§12 gate 8 covers
the stub scan; design §10 requires incident → regression fixture).

---

## 15. Ambiguities in the design doc and their resolutions

1. **Denial severity.** §7 says policy denial is "terminal for that
   call"; the §3 diagram shows `Authorize → Degraded` (run-terminal).
   Resolved: run-terminal `Denied`. The state diagram is the authority;
   a denial is a safety event, and "do not rephrase and retry" is
   enforced by ending the run.
2. **`Completed` name collision.** The §3 diagram's final node and the
   §3 terminal status share the name. Resolved: the workflow-final
   state is `End`; `Completed` is strictly a `TerminalResult` status.
3. **Verification failure has no status code.** Resolved: `Verify →
   Account` always; the failure becomes a `VerificationFailed`
   observation and the loop monitor bounds retries (3 identical →
   `Stuck`). No invented rollback, per design §9.
4. **Encoding left open (§14).** Resolved for the slice: line-based
   `VERB <json>` grammar (§4.3); the general encoding question moves
   to E2 with `esper-protocol`.
5. **Args validation placement.** Resolved: static schema in the
   decoder (consumes repair allowance); `Authorize` handles only
   capability policy and device business rules (§4.5).
6. **Transient retry bookkeeping.** Resolved: each attempt commits its
   own `ToolObservation` under the same `EffectId`, making the retry
   count derivable after reboot (§3.2, §10.3).
7. **`TimeBudget` clock semantics.** Resolved: host monotonic
   milliseconds; no cross-reboot elapsed claims (Waymaker
   durable-time rule, §7.3).
8. **`SubgoalClosed` without subgoals.** Resolved: defined, never
   emitted in the slice; explicit subgoals arrive with E4
   continuations (§8.1).
9. **`AwaitApproval` in the diagram.** Resolved: unreachable in the
   slice — no tool carries `SensitiveWrite` and no capability set can
   grant it (§5.2).
10. **`Compact` in the diagram.** Resolved: E4; omitted from the slice
    machine with the deferral recorded (§1).
11. **`timer_delay_wait` is `IdempotentWrite`, not a new class.**
    Resolved: the effect is "the virtual clock has advanced to at least
    t0+ms" — a set-operation on the clock. Redelivery under the same
    `EffectId` re-asserts the same target and must not double-advance,
    which is exactly the at-least-once machinery (intent before dispatch,
    stable `EffectId`, device-side dedup, §10.3). No new permission class
    was needed. The runtime verifies with `ReadBack`: the clock advanced
    by at least `ms` from its pre-dispatch reading.
12. **Delays consume the mutations budget.** Resolved: `timer_delay_wait`
    is an `IdempotentWrite`, so each committed intent consumes one
    `mutations` unit, exactly like `gpio_pin_write` (§7.2). Time passing
    is a world-state change.
13. **Sensor values are fixed per channel.** Resolved: the E2 fake device
    returns deterministic raw values per sensor id — normative table:
    sensor 0 → 210, 1 → 315, 2 → 1800, 3 → 42. Golden fixtures assert
    these bytes; no noise, no drift (§17.6).
14. **Status payload shapes.** Resolved: `detail: "summary"` returns
    `{"pins":8,"sensors":4,"uptime_ms":N}`; `"full"` adds the pin
    direction/level arrays and the per-sensor raw values, still within
    the 128-byte bound. `N` is the virtual clock the timer tools share
    (§17.6).
15. **Enum spellings compare raw JSON string content.** Resolved: the
    strict parser preserves escapes verbatim, so `"h\u0069gh"` does not
    spell the `high` option — exactly like the E0/E1 decoder's
    `Level::from_bytes`. One rule, documented in `esper-protocol`, not a
    second code path (§17.3).
16. **`Error::PermissionDenied` carries `resource`, not `pin`.**
    Resolved: the E0/E1 field `pin: u8` becomes `resource: u8` — the pin
    for GPIO tools, the sensor id for `sensor_sample_read`, 0 for timer
    and status tools. Core-crew change (§17.5).
17. **Encoding: keep the line grammar for E2.** Resolved: the `VERB
    <json>` grammar (§4.3) survives E2; `esper-protocol` does not replace
    it. Tradeoff: the line grammar is proven, token-cheap, and parsed by
    the strict parser that now also powers the contract validator — one
    parser for both jobs. What the model actually sees in the prompt is
    not JSON Schema but the compact per-tool signatures generated by
    `esper-protocol` (e.g.
    `gpio_pin_write(pin:u8[0-7], level:low|high) -- …`), which are
    cheaper than a schema dump and unambiguous. Full JSON-Schema
    validation stays a host/training-side concern (the renderer exports
    it for exactly that). Revisit at E3 only if the tiny-model backend
    needs a different wire shape.

---

## 16. Open questions for later rungs (not blockers)

- Whether the §4.3 line grammar survives contact with the local
  tiny-model backend (E3), or whether E2's `esper-protocol` replaces it.
  **Resolved for E2 (§15.17): keep the line grammar.** Revisit at E3
  only if the tiny-model backend needs a different wire shape.
- Firmware `TimeBudget` backend for `esper-device` (E2/E3).
- Whether tool-local circuits need durable state (design §14).
- Hard RAM/flash caps for `esper-core` + `esper-runtime` once the
  target board is fixed (E3); the slice only measures.

---

## 17. Rung E2: typed native tools

### 17.1 Contract-source design

One constant table — `esper_protocol::CATALOG`
(`crates/esper-protocol/src/contract.rs`) — is the single source of
truth for the six-tool catalog. From it the crate derives:

1. the allocation-free on-device argument validator (`validate`);
2. the compact model-facing signature per tool (`render_signature`) —
   the line the model sees in the prompt's tool list (design §8);
3. the minimal JSON Schema per tool (`render_json_schema`) — for
   training and host interop (design §7 schema-pipeline item 1);
4. the golden valid/invalid argument fixtures (each entry's
   `example_ok` / `example_bad`, asserted by in-crate tests: every
   `example_ok` validates, every `example_bad` fails, at least four bad
   cases per tool).

Nothing else in the workspace hand-writes a tool schema. The rendered
signature and JSON Schema strings of every tool are byte-pinned by
golden tests: any wording change is a deliberate, reviewed diff.

The crate is `#![no_std]`, `#![deny(unsafe_code)]`, no `alloc`,
core-only; its only dependency is `thiserror` (2), like `esper-core`.
It depends on nothing else in the workspace.

### 17.2 The six tools

The normative catalog is the §5.1 table: `gpio_pin_read` (1),
`gpio_pin_write` (2), `sensor_sample_read` (3), `timer_uptime_read` (4),
`timer_delay_wait` (5), `device_status_report` (6). Tool ids 1–2 keep
their E0/E1 assignments and are never renumbered.

E2 excludes generic network access (deferred to E5): no tool name,
description, or argument suggests network access, and the
`no_generic_network_access_in_catalog` test fails the build if any
entry's name or description contains "http", "network", "socket", or
"url".

### 17.3 Normative `esper-protocol` API

Other crews code against exactly this surface (delivered by Crew A):

```rust
pub struct ToolContract {
    pub id: u8,
    pub name: &'static str,
    pub schema_version: u8,
    pub permission: PermissionClass,
    pub verification: VerificationStrategy,
    pub idempotency: IdempotencyStrategy,
    pub result_bound: u16,
    pub description: &'static str,
    pub args: &'static [ArgSpec],
    pub example_ok: &'static str,
    pub example_bad: &'static [&'static str],
}

pub struct ArgSpec {
    pub name: &'static str,
    pub kind: ArgKind,
    pub required: bool,
}

pub enum ArgKind {
    U8 { lo: u8, hi: u8 },
    U16 { lo: u16, hi: u16 },
    Enum { options: &'static [&'static str] },
}

pub enum PermissionClass {
    ReadOnly, IdempotentWrite, SensitiveWrite, Irreversible,
}
pub enum VerificationStrategy { None, ReadBack }
pub enum IdempotencyStrategy { NotApplicable, SetOperation }
// each with `pub const fn name(self) -> &'static str`

pub const MAX_ARGS_PER_TOOL: usize = 4;

pub struct BoundArgs {
    pub fields: [Option<BoundField>; MAX_ARGS_PER_TOOL],
    pub len: u8,
}
pub struct BoundField {
    pub name: &'static str,
    pub value: Scalar,
}
pub enum Scalar {
    U8(u8),
    U16(u16),
    Enum(u8), // index into the spec's `options`
}
impl BoundArgs {
    pub fn get(&self, name: &str) -> Option<Scalar>;
}

pub enum ValidationError {
    Json(JsonError),
    NotObject,
    UnknownArgument,
    MissingArgument(&'static str),
    WrongType(&'static str),
    OutOfRange(&'static str),
    BadEnumValue(&'static str),
    TooManyArguments,
}

pub fn validate(contract: &ToolContract, json: &[u8]) -> Result<BoundArgs, ValidationError>;
pub fn render_signature(
    contract: &ToolContract,
    out: &mut dyn core::fmt::Write,
) -> core::fmt::Result;
pub fn render_json_schema(
    contract: &ToolContract,
    out: &mut dyn core::fmt::Write,
) -> core::fmt::Result;
pub const fn catalog() -> &'static [ToolContract];
pub fn lookup_by_id(id: u8) -> Option<&'static ToolContract>;
pub fn lookup_by_name(name: &str) -> Option<&'static ToolContract>;
```

Validator semantics (normative):

- Fixed check order: JSON syntax, then top-level object shape, then
  per-field checks in JSON key order, then required-field presence in
  contract order. The first violation wins.
- Top-level value must be an object (`NotObject` otherwise). An empty
  object is valid exactly when the contract declares no arguments
  (`timer_uptime_read`).
- Unknown fields are rejected (`UnknownArgument`); every required field
  must be present (`MissingArgument` carries the contract's field name,
  never model bytes).
- Per kind: `U8`/`U16` require a JSON number spelled as plain digits
  (the strict parser already rejects signs, fractions, exponents, and
  leading zeros) within the closed range (`WrongType` / `OutOfRange`);
  `Enum` requires a JSON string byte-equal to one option
  (`BadEnumValue`), compared against raw string content — escaped
  spellings are rejected (§15.15). The bound `Enum` value is the
  option's index.
- Depth over 3 and duplicate keys are rejected by the strict parser
  itself (`Json`); trailing bytes after the object are rejected too.
  Where the parser/drain does not descend into a value — a composite in
  a scalar argument position — the validator reports the schema
  violation instead (`WrongType`, surfacing as `InvalidArgs`); depth
  faults are `Json` only where the parser descends (ASK/FINISH
  envelopes).
- `BoundArgs` fields are emitted in contract order, not JSON key order,
  so dispatch sees deterministic order whatever the model spells.
  Values are `Copy` scalars; nothing borrows the model input.

Signature format (normative, byte-pinned by tests):

```text
gpio_pin_write(pin:u8[0-7], level:low|high) -- Set the logic level of a GPIO pin. Re-dispatch is safe (set-operation). [idempotent_write, verify:read_back, bound:64B]
```

i.e. `name(args) -- description [permission, verify:verification, bound:NB]`,
arguments as `name:u8[lo-hi]`, `name:u16[lo-hi]`, or `name:opt1|opt2`,
and `name()` for argument-less tools. This is the line the model sees
in the prompt's tool list (design §8).

JSON Schema format (normative, byte-pinned by tests): one compact line,
fields in fixed order —
`{"name":…,"type":"object","properties":{…},"required":[…],"additionalProperties":false}`;
integer kinds render with `minimum`/`maximum`, enums with an `enum`
array in option order, `required` lists required arguments in contract
order. Contract-table text is ASCII by construction; the renderers emit
it verbatim without string escaping.

### 17.4 The `json.rs` move (normative; executed once)

1. `crates/esper-core/src/json.rs` moved to
   `crates/esper-protocol/src/json.rs` via `git mv` — **done by Crew A;
   do not repeat.**
2. `esper-core` gained the dependency
   `esper-protocol = { path = "../esper-protocol", version = "0.1.0" }`,
   and `src/lib.rs` now has `pub use esper_protocol::json;` instead of
   `pub mod json;` — done by Crew A.
3. Every existing path (`crate::json::…`, `esper_core::json::…`, the
   `tests/json.rs` integration tests) keeps working through the
   re-export; no E0/E1 code path changes meaning.
4. Crew A added `json::parse_u16` (the `u16` sibling of `parse_u8`) for
   the `timer_delay_wait` `ms` argument; it lives with the parser.
5. There is exactly one implementation of this module. The core crew
   must not move, copy, or fork it.

Rationale: `esper-protocol` cannot depend on `esper-core` (the core
will depend on the protocol — a cycle), so the shared parser lives in
the protocol crate and the core re-exports it.

### 17.5 Normative `esper-core` adoption (core crew)

- Replace the local `PermissionClass`, `VerificationStrategy`, and
  `IdempotencyStrategy` in `registry.rs` with re-exports of
  `esper_protocol`'s. Variant names are identical, so this is
  mechanical; afterwards exactly one definition of each exists.
- Replace `registry::ToolEntry` / `CATALOG` (the two-tool E0/E1 table)
  with `esper_protocol::ToolContract` / `catalog()` (re-exported). The
  decoder validates `CALL` args with `esper_protocol::validate`
  instead of the hand-written per-tool schemas; schema violations keep
  consuming repair allowance (§4.5).
- Bind validated arguments to the typed dispatch shape:

```rust
pub enum ToolArgs {
    GpioPinRead { pin: Pin },
    GpioPinWrite { pin: Pin, level: Level },
    SensorSampleRead { sensor: u8 }, // 0..=3, range proven by validate
    TimerUptimeRead,
    TimerDelayWait { ms: u16 },
    DeviceStatusReport { detail: StatusDetail },
}

pub enum StatusDetail { Summary, Full }

impl ToolArgs {
    /// Bind validated arguments to the typed shape. The `BoundArgs`
    /// came from `esper_protocol::validate`, so ranges are already
    /// proven; this only reorganizes into the dispatch shape.
    ///
    /// # Errors
    ///
    /// Returns `Error::InvalidArgs`-family rejections when the tool id
    /// is unknown or a field is absent — fail closed. Unreachable when
    /// the decoder ran `validate` first.
    pub fn bind(tool_id: ToolId, args: &BoundArgs) -> Result<Self, Error>;
}
```

(`sensor` stays `u8` — the range is proven by the validator; the core
crew may add a `SensorId` newtype if it prefers.)

- Replace the E0/E1 `authorize_capability` with the full `Authorize`
  gate:

```rust
/// Allowlist → capability (§5.2 v2) → device business rules, in the
/// normative check order. Denial is terminal `Denied`, never a repair turn.
///
/// # Errors
///
/// Returns `Error::PermissionDenied` when the capability set or the
/// device business rules refuse the call.
pub fn authorize(
    caps: &Capabilities,
    entry: &ToolContract,
    args: &ToolArgs,
) -> Result<(), Error>;
```

- Rename `Error::PermissionDenied`'s field `pin: u8` to `resource: u8`:
  the pin for GPIO tools, the sensor id for `sensor_sample_read`, 0
  for timer and status tools (§15.16).

### 17.6 Normative `esper-runtime` mapping (runtime crew)

| Tool | Dispatch | Verification |
|---|---|---|
| `gpio_pin_read` | sample the pin level | `None` |
| `gpio_pin_write` | set the pin level; dedup on `EffectId`; set-operation | `ReadBack`: independent read of the pin vs the committed level (§9) |
| `sensor_sample_read` | return the channel's fixed raw value (§15.13 table) | `None` |
| `timer_uptime_read` | return the virtual-clock reading in ms | `None` |
| `timer_delay_wait` | advance the virtual clock to ≥ t0+ms, where t0 is the pre-dispatch reading; redelivery under the same `EffectId` does not re-advance (dedup + set-operation); consumes one `mutations` unit (§15.12) | `ReadBack`: the clock advanced by at least `ms` from its pre-dispatch reading |
| `device_status_report` | summary or full payload (§15.14) | `None` |

Timer semantics: the fake device owns a virtual `u64` millisecond
clock shared by the timer and status tools. `timer_delay_wait`'s
committed intent records the target; dispatch advances the clock to at
least t0+ms. The verifier is independent of the dispatch path (ADR 7):
it reads the clock through a separate handle and checks the advance —
a verifier that reads a flag the dispatcher set is not a verifier
(§9.1).

Sensor semantics: the fake device holds four channels with the fixed
raw values from §15.13. `sensor_sample_read` returns the channel's
value; there is no noise and no drift, so golden fixtures assert exact
bytes.

Status semantics: `detail: "summary"` returns
`{"pins":8,"sensors":4,"uptime_ms":N}`; `"full"` adds the pin
direction/level arrays and the per-sensor raw values, still within the
128-byte bound. `N` is the shared virtual clock (§15.14).

### 17.7 Normative `esper-eval` fixture deltas (eval crew)

- `run_seed.capabilities` gains `sensors: [...]` (array of sensor ids),
  `allow_timer: bool`, `allow_status: bool`. Absent means denied — fail
  closed. E0/E1 fixtures without these fields keep their meaning.
- `device.faults[]` entries gain an optional `tool: "<name>"` filter;
  absent means the fault applies to any tool (preserves E0/E1
  behavior).
- Fault resource matching: GPIO tools match on `pin`,
  `sensor_sample_read` on `sensor`, timer and status tools on resource
  0.
- New golden fixtures (eval crew writes, following §14.3): sensor read
  success; timer delay with read-back verification; status summary and
  full; denied sensor / timer / status (each terminal `Denied` with the
  §5.2 reason bytes); delay redelivery under the same `EffectId` after
  a crash between dispatch and observation commit does not
  double-advance the clock.
