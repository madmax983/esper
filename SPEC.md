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

---

## 18. Rung E3: model adapters

Rung E3 puts a real model interface behind the engine's `Infer`
step. Two backends implement one object-safe trait: the scripted host
backend (the E0/E1 `ScriptedModel` behavior, formalized) and the tiny
local backend (a distillation stand-in: a recorded prompt→line table
keyed by the prompt fingerprint, ADR 0001). The engine crew owns the rewiring of
`engine.rs` to this interface; this section is the contract they code
against. Nothing here changes settled sections; deltas are called out
as ambiguity resolutions in §18.10.

Reconciled 2026-09-22 against ADR 0001
(`docs/adr/0001-e3-tiny-backend-distillation-standin.md`) and the
shipped `backend.rs`: where the foundation draft below differed from
the implementation, the implementation governs, and each delta is
marked inline.

### 18.1 The backend contract

`esper_runtime::backend::ModelBackend` is the single model interface.
It is object-safe (no generics, no `Self` returns) so the engine holds
`&mut dyn ModelBackend`:

```rust
pub trait ModelBackend {
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError>;
    fn bundle_id(&self) -> BundleId;
    fn last_usage(&self) -> TokenUsage;
    fn unemit(&mut self);
}
```

- `infer` writes one decision line into the caller-owned `out`
  buffer and returns the bytes written. The caller sizes `out` at
  `OUTPUT_CAP` (256); a line that does not fit is
  `BackendError::OutputTooLong`, never a truncation. The prompt is
  read-only; the backend must not retain it beyond the call.
- `bundle_id` returns the backend's identity (§18.7). It is stable
  for the backend's lifetime.
- `last_usage` returns the token accounting of the most recent
  `infer` call (`TokenUsage { input_tokens, output_tokens }`,
  `Default` is zero). It changes only on a successful `infer`.
- `unemit` rewinds one emission for crash-before-commit recovery:
  the engine calls it when a line was taken but never committed, so
  the next boot must see the same line again. `TinyBackend::unemit`
  is a no-op by construction (§18.3); `ScriptedBackend::unemit`
  rewinds its cursor with `saturating_sub`.

Open item: the trait carries no `Send` bound, and the engine's async
driver holds `&mut dyn ModelBackend` across an await — clippy's
`future_not_send` (nursery) fires on it. Either the trait gains
`: Send` or the engine crew contains the backend outside the `Send`
future. See §18.10.

Error vocabulary (`BackendError`, via `thiserror`, `no_std`
compatible):

| Variant | Meaning |
|---|---|
| `Exhausted` | the backend has no more outputs to emit (scripted script consumed) |
| `OutputTooLong` | the emitted line does not fit the caller buffer |
| `UnknownPrompt` | no distillation entry matches the prompt fingerprint (tiny backend) |
| `PolicyMismatch` | the entry failed its integrity check (tiny backend) |

Inference settings travel with the seed, never as hidden constants:

```rust
pub struct InferenceSettings {
    pub max_prompt_bytes: u16,
    pub max_output_bytes: u16,
}
impl InferenceSettings {
    pub const fn default_settings() -> Self; // 1024 / 256
    pub const fn validate(&self) -> Result<(), &'static str>; // each in 1..=cap
    pub const fn bytes(&self) -> [u8; 4]; // little-endian, for hashing
}
```

`PROMPT_CAP` is 1024 and `OUTPUT_CAP` is 256. `validate` fails closed
on 0 or on either bound above its cap; the seed's `validate` maps the
reason through unchanged.

### 18.2 Scripted host backend

`ScriptedBackend` (host-only, `#[cfg(feature = "host")]`) formalizes
the E0/E1 `ScriptedModel` as a `ModelBackend`:

```rust
pub struct ScriptedBackend {
    lines: Vec<Vec<u8>>,      // HOST-ONLY
    index: usize,
    settings: InferenceSettings,
    bundle: BundleId,
    last_usage: TokenUsage,
}
impl ScriptedBackend {
    pub fn new(lines: Vec<Vec<u8>>, settings: InferenceSettings) -> Self;
    pub const fn lines_consumed(&self) -> usize;
}
```

- `new` binds the bundle at construction: `fnv1a64` over the domain
  tag `esper-scripted-v1`, then each line length-prefixed, in order
  (§18.7). Two scripts that differ in any byte have different bundle
  ids. (Change from the foundation draft, ADR 0001: the settings
  bytes are not hashed into the scripted bundle; the seed snapshot
  binds the settings separately, so no coverage is lost.)
- `infer` emits `lines[index]` into `out` (copied, never aliased):
  exhausted is `Exhausted`, too long for `out` is `OutputTooLong`.
  On success it sets `last_usage` to `{ prompt.len(), n }`
  (saturating to `u32::MAX`, unreachable in practice) and advances
  the index.
- `unemit` rewinds the index with `saturating_sub(1)` — the same
  crash-before-commit semantic `ScriptedModel::unemit` had.

`world::ScriptedModel` was retired by the engine crew during the E3
rewiring; `ScriptedBackend` is its replacement — the engine no longer
pulls canned lines from a script object.

### 18.3 Tiny local backend

The tiny backend is a **distillation stand-in** for a trained
on-device model (ADR 0001): it answers each inference prompt from a
caller-owned `DistillEntry` table keyed by the prompt fingerprint —
a table of `(prompt, line)` pairs recorded from the scripted
backend's own runs. No trained weights exist anywhere in this rung.

```rust
pub fn fingerprint_prompt(prompt: &[u8]) -> u64; // fnv1a64(prompt)
pub const TINY_PARAMS_BYTES: usize = 2156; // declared parameter budget

pub struct DistillEntry<'a> {
    pub prompt_fp: u64,   // expected fingerprint_prompt(prompt)
    pub line_hash: u64,   // fnv1a64(line): integrity of the entry
    pub line: &'a [u8],   // the decision line to emit
}

pub struct TinyBackend<'a> {
    table: &'a [DistillEntry<'a>],
    usage: TokenUsage,
    bundle: BundleId,
}
impl<'a> TinyBackend<'a> {
    pub fn new(table: &'a [DistillEntry<'a>]) -> Self;
}
```

(Change from the foundation draft, ADR 0001: the draft described a
`TinyParams` parameter block and a pseudo-random neural forward pass
producing the fingerprint. The shipped backend keys on
`fnv1a64(prompt)` directly — the fingerprint contract the trained
backend will keep is "same prompt bytes in, byte-identical output
on replay", not the projection — and keeps `TINY_PARAMS_BYTES =
2156` as a declared parameter budget the reference table distills
to, pinned by test. Rationale: shipping pseudo-weights would lie
about the backend's capability; a table replay gives the §18.9
byte-identical replay by construction, which a pseudo-random
"model" could not.)

`infer` semantics, in fixed order:

1. `fp = fingerprint_prompt(prompt)`; linear scan for
   `prompt_fp == fp`, else `UnknownPrompt`. The backend never
   guesses: an unrecorded prompt is a hard failure.
2. `fnv1a64(entry.line) == entry.line_hash`, else `PolicyMismatch`.
   The table is data, not code: a corrupt or tampered entry fails
   closed here, before any byte reaches the decoder.
3. `entry.line` fits `out`, else `OutputTooLong`; copy it in.
4. Set `last_usage = { prompt.len(), n }` (saturating `u32`); return
   `n`.

`unemit` is a no-op: the lookup is pure — no cursor, no hidden
state — so crash-before-commit replays the same prompt and gets the
same line. Idempotent by construction. (This is the documented
reason, not an omission.)

Bundle identity: `bundle_id` is `fnv1a64` over the domain tag
`esper-tiny-v1`, then each entry's `(prompt_fp, line_hash)` as
little-endian `u64`, in table order (§18.7). The line bytes are
bound indirectly through `line_hash`; two tables that answer the
same prompts with the same lines share one bundle id — the identity
is about behavior, not about which bytes were distilled when.

**Honesty statement.** The distillation table is a stand-in for
trained weights: it replays lines recorded from the scripted backend
(§18.9). No training has happened and no gradient step has run. Any
claim that the tiny backend "learned" a trajectory is false. What is
real: the fingerprint contract, the table lookup, the integrity
check, and the bundle binding are the exact shapes the
trained-weights backend will keep; only the lookup source changes.

### 18.4 Prompt construction

`build_prompt(ctx: &PromptCtx, out: &mut [u8]) -> usize` renders one
prompt into the caller buffer. The prompt opens with `TOOLS`, renders
all six catalog tool signatures via
`esper_protocol::render_signature` (the single contract source,
SPEC §17.3), then a compact `STATE` line, an optional `REPAIR` line,
an optional `LAST` observation line, and closes with the `EMIT`
trailer:

```text
TOOLS
<render_signature of each of the 6 catalog tools>
STATE turns=<n> muts=<n>
REPAIR <attempt>:<variant> <grammar one-liner>
LAST <observation bytes>
EMIT one decision line.
```

The model sees the signatures, never a bare tool list: the tiny
backend keys on exact prompt bytes, and the scripted backend ignores
the prompt, so the prompt is data for the fingerprint first and
readable context second. The `REPAIR` line appears only after an
invalid line burned a turn; it carries the one-based repair attempt,
the §4.4 `variant` vocabulary (`malformed` | `invalid_args`), and the
grammar one-liner derived from `esper_core::decision::decode_line`
(`CALL <tool> <json> | ASK <json> | FINISH <json>`), so a model that
just burned a repair turn sees the exact output shape again. The
`LAST` line appears only after a tool reported; it carries the
observation bytes raw (already bounded machine JSON, capped at 128
bytes by the engine).

Supporting types:

```rust
pub struct RepairHint { pub attempt: u8, pub variant: &'static str }
pub struct PromptCtx<'a> {
    pub turns_left: u16,
    pub mutations_left: u16,
    pub last_observation: Option<&'a [u8]>,
    pub repair: Option<RepairHint>,
}
```

- `turns_left` / `mutations_left` render as decimal.
- Truncation rule: the `TOOLS` block, the `STATE` line, the `REPAIR`
  line, and the `EMIT` trailer are fixed — they are never truncated.
  Only the `LAST` observation tail may truncate, and then it ends with
  `[truncated]`. The framing around the signatures is abbreviated
  (`turns=`/`muts=`) so the fixed prompt — six exact signatures plus
  framing — always fits `PROMPT_CAP`; a unit test pins this budget, so
  signature growth fails loudly instead of silently squeezing the
  observation. (A narrowed `max_prompt_bytes` truncates
  deterministically head-first.)
- Replay: every prompt input comes from durable or rebuilt state, so
  a post-reboot inference builds the byte-identical prompt — the tiny
  backend's prompt→line lookup depends on it. The engine restores the
  prompt inputs from the journal for exactly this reason.

### 18.5 Model-agnostic JSON repair policy

Repair is identical under both backends: the engine builds the same
`PromptCtx` (same REPAIR line shape) whatever backend produced the
bad line. Backends differ only in how they produce the next line,
never in the hint they receive.

- Allowance: **3 bounded repair turns.** On the `k`-th repair
  (`k` = 1, 2, 3) the prompt carries
  `REPAIR <k>:<variant> <grammar one-liner>` (§18.4). `variant` is the
  §4.4 vocabulary (`malformed` | `invalid_args`). When the allowance
  is exhausted the run terminates `ModelInvalid` with a bounded
  summary — clean, never a hang. (Resolution vs §4.4/§3.2's allowance
  of 2: see §18.10.)
- The REPAIR line re-states the decision grammar (`CALL <tool>
  <json> | ASK <json> | FINISH <json>`, derived from
  `esper_core::decision::decode_line`): the model that just burned a
  repair turn sees the exact output shape again. The decoder's
  field-level detail (§4.4's field and received category) travels in
  the observation at the engine crew's discretion.
- Repair turns are ordinary model turns: they consume `model_turn`
  budget and their output re-enters `Infer` through the sole gate
  (§18.6). Denials stay terminal `Denied`, never repair turns
  (§4.5).

### 18.6 Grammar enforcement

`esper_core::decode_line` is the sole gate between a backend and the
engine. Backends emit bytes; the decoder admits `Decision`s. There is
no backend-specific parsing, no second grammar, and no lenient path:
a line the tiny backend emits faces exactly the validator the
scripted lines faced. This is what makes the §18.9 replay
meaningful — identical bytes in, identical decisions out.

### 18.7 Bundle identity

`BundleId(pub u64)` names a backend's model identity. Hash: FNV-1a
64-bit, offset basis `0xcbf29ce484222325`, prime `0x100000001b3`
(`pub const fn fnv1a64(bytes: &[u8]) -> u64`).

- Scripted: `fnv1a64("esper-scripted-v1" ++ le_len(line_0) ++
  line_0 ++ …)` — each line length-prefixed, in order.
- Tiny: `fnv1a64("esper-tiny-v1" ++ le64(prompt_fp) ++
  le64(line_hash) per entry, table order)` — the line bytes are
  bound indirectly through `line_hash`.

(Change from the foundation draft, ADR 0001: the scripted bundle no
longer hashes the settings bytes, and the tiny bundle hashes the
table entries rather than a parameter block. The seed snapshot binds
the settings separately, so no coverage is lost.)

Seed wiring: `RunSeed` gains `pub model_bundle: u64` (the
`BundleId`'s inner value) and `pub inference: InferenceSettings`.
The journal-binding snapshot grows to a **67-byte** canonical
encoding:

| bytes | field | encoding |
|---|---|---|
| 0..8 | `id` | `u64` le |
| 8..16 | `elapsed_ms` | `u64` le |
| 16..20 | `input_tokens` | `u32` le |
| 20..24 | `output_tokens` | `u32` le |
| 24..26 | `model_turns` | `u16` le |
| 26..28 | `mutations` | `u16` le |
| 28..30 | `workflow_version` | `u16` le |
| 30..38 | `read_pins` | 8 bytes |
| 38 | `read_count` | `u8` |
| 39..47 | `write_pins` | 8 bytes |
| 47 | `write_count` | `u8` |
| 48..52 | `sensors` | 4 bytes |
| 52 | `sensor_count` | `u8` |
| 53 | `allow_timer` | `u8` |
| 54 | `allow_status` | `u8` |
| 55..63 | `model_bundle` | `u64` le |
| 63..65 | `max_prompt_bytes` | `u16` le |
| 65..67 | `max_output_bytes` | `u16` le |

The `RunStarted` record carries these 67 bytes; recovery compares
them byte for byte, so a reboot can never swap the model, the
prompt/output caps, or any budget or capability the seed granted.
`RunSeed::validate` additionally runs `inference.validate()` and
maps its reason through unchanged.

### 18.8 Resource budgets

Normative caps for the E3 slice (measured numbers are the
measurement crew's pass — stated here as targets, not claims):

| Budget | Cap | Notes |
|---|---|---|
| Prompt buffer | 1024 B (`PROMPT_CAP`) | caller-owned; `build_prompt` never exceeds it |
| Single decision line | 256 B (`OUTPUT_CAP`) | `infer`'s `out`; longer is `OutputTooLong` |
| `max_prompt_bytes` | 1..=1024 | seed-bound, in the 67-byte snapshot |
| `max_output_bytes` | 1..=256 | seed-bound, in the 67-byte snapshot |
| `TINY_PARAMS_BYTES` | 2156 B declared | parameter budget the reference distill table distills to; pinned by test — parameter growth is always deliberate |
| Fingerprint stack | trivial | `fingerprint_prompt` is `fnv1a64` over the prompt bytes — no arrays, no heap |
| Distillation table | caller-owned | host builds it; the largest fixture table (`i-sensor-timer-status-happy`, 5 entries) is 292 B by the §18.3 entry layout (line bytes + 16 B/entry); flash is caller-chosen (rodata or RAM) |
| Tiny-backend RAM | 1,312 B projected | 1024 B prompt buffer (`PROMPT_CAP`) + 256 B output buffer (`OUTPUT_CAP`) + 24 B `TinyBackend` struct (32-bit Xtensa by construction; 32 B on the 64-bit host) + 8 B fingerprint digest state — see the projection below |
| Repair allowance | 3 turns | §18.5; consumes `model_turn` budget |
| `TokenUsage` | `u32` counters | saturating; reported per `infer` |
| Tiny `infer` latency (host) | mean ≈ 0.6 µs | 1,000-iteration loop, `fingerprint_prompt` + `infer`, x86_64 host; max observed 7.5 ms on one preempted iteration (loaded shared VM) — measurement only, never a gate; see `crates/esper-runtime/tests/resource_measure.rs` |
| Target-board code size (measured) | see projection | Espressif `esp` toolchain, `rustc 1.97.0-nightly (8ea53bcd7 2026-07-08)`, `xtensa-esp32s3-none-elf`, `-Z build-std=core,compiler_builtins`, release; `.text` + `.rodata` per crate rlib; see the projection below |
| Energy | unmeasured | no energy proxy exists for the target board; the SPEC §16 open question stays open — nothing in E3 stands in for it |

#### 18.8.1 ESP32-S3 projection (measured vs projected)

Method: every "measured-target" row is `size -A` over the release
rlib built with the toolchain above. The stock tree does **not** build
for this target as-is: `thiserror = "2"` defaults to its `std`
feature, and the target has no precompiled `std`; the measurement
used a temporary `thiserror = { version = "2", default-features =
false }` override, reverted after measuring. A real firmware port
must carry that one-line change.

| Component | Bytes | Kind |
|---|---|---|
| `esper-protocol` `.text`+`.rodata` | 9,942 | measured-target |
| `esper-core` `.text`+`.rodata` | 12,626 | measured-target |
| `esper-runtime` (no `host`) `.text`+`.rodata` | 4,598 | measured-target |
| `waymaker-core` `.text`+`.rodata` | 3,693 | measured-target |
| `thiserror` (no default features) `.text`+`.rodata` | 170 | measured-target |
| `TINY_PARAMS_BYTES` (declared budget) | 2,156 | declared |
| Largest distill table (§18.3 layout) | 292 | measured-host (fixture-derived) |
| **Flash subtotal (crates + params + table)** | **33,477** | projected |
| `core` from `-Z build-std` (full rlib) | 161,538 | measured-target, upper bound — a linked image keeps only referenced items |
| Prompt buffer + output buffer + `TinyBackend` + fingerprint state | 1,312 | exact by construction (1024 + 256 + 24 + 8) |
| **RAM subtotal (inference working set)** | **1,312** | projected |

What the projection does **not** claim: a linked firmware image
(needs a board HAL, linker script, and the E4+ runtime surface — none
exist in this rung), the `core` share that actually links (only the
referenced items survive linking; the 161,538 B full-rlib number is an
upper bound), or any energy figure. The `core` rlib's full
`.text`+`.rodata` is reported so a later link map can be checked
against it.

### 18.9 Both-backend exit criterion

E3 exits when every golden trajectory in `spec/trajectories/`
replays byte-identical under both backends. Procedure:

1. **Record.** Run each fixture under `ScriptedBackend`; capture
   every `(prompt_bytes, emitted_line)` pair the run produces.
2. **Distill.** Build the `DistillEntry` table: `prompt_fp =
   fingerprint_prompt(prompt)`, `line_hash = fnv1a64(line)`,
   `line` verbatim. The table is a build artifact next to the
   fixtures, reviewed like one.
3. **Replay.** Run each fixture under `TinyBackend` with the
   distilled table. Pass means: same terminal status, same committed
   decision sequence, same journal bytes — the §18.6 gate makes
   "same bytes in" mean "same run out".

A trajectory the tiny backend cannot replay (an `UnknownPrompt` on
the path) is a distillation gap, not a backend bug: re-record with
the missing prompt covered. The eval crew owns the replay harness;
the exit criterion is per-trajectory, all green.

### 18.10 Ambiguity resolutions

1. **`TINY_PARAMS_BYTES`.** The brief named 2156; the foundation
   draft computed 2192 from a `TinyParams` layout that ADR 0001 then
   removed. 2156 stands as a declared parameter budget the reference
   distill table distills to, pinned by test — parameter growth is
   always deliberate.
2. **Repair allowance.** §4.4/§3.2 settle an allowance of 2 for the
   E0/E1 slice. E3 normatively sets **3** for backend-driven runs:
   the tiny backend's outputs are coarser than hand-written script
   lines, so one extra repair turn keeps the exit criterion
   reachable while the bound stays small and explicit. The engine
   crew updates the `Repair` → `Degraded` guard from
   `repairs_used >= 2` to `repairs_used >= 3`; §4.4's text is left
   untouched as the slice's history.
3. **Fingerprint design (superseded).** The foundation draft
   described byte tokenization feeding a pseudo-random neural
   forward pass. ADR 0001 replaced it: `fingerprint_prompt` is
   `fnv1a64` over the exact prompt bytes. The draft's resolution
   text is retired with the design.
4. **REPAIR line vs §4.4's structured error.** The shipped prompt's
   REPAIR line carries the attempt, the §4.4 `variant` vocabulary,
   **and** the grammar one-liner derived from
   `esper_core::decision::decode_line` (§18.4) — the model that just
   burned a repair turn sees the exact output shape again. The
   decoder's field-level detail (§4.4's field and received category)
   travels in the observation at the engine crew's discretion. Both
   backends see the same shape either way.
5. **`unemit` asymmetry.** `ScriptedBackend::unemit` rewinds a
   cursor; `TinyBackend::unemit` is a no-op. Not an omission: the
   table lookup is pure, so replaying the prompt after a crash
   re-derives the same line — idempotent by construction.
6. **ADR 0001 override.** The foundation draft's §18.3 (parameter
   block, neural fingerprint) was deliberately replaced by the E3
   crews with the distillation stand-in, documented in
   `docs/adr/0001-e3-tiny-backend-distillation-standin.md`. The
   draft's §18.4 flat machine prompt was likewise replaced — in the
   other direction: the shipped prompt restores the draft's
   TOOLS-block shape (six `render_signature` lines, compact STATE,
   REPAIR with grammar one-liner, truncatable LAST, EMIT trailer)
   per the E3 TOOLS/signature requirement. The implementation
   governs; the draft text is superseded where it disagrees.
7. **`Send` bound (resolved).** `ModelBackend` now carries a `Send`
   supertrait: the engine's async driver holds `&mut dyn
   ModelBackend` across an await, and clippy's `future_not_send`
   (nursery) requires it. All shipped backends are `Send`
   (`TinyBackend` borrows only `&[u8]` and holds integer values),
   proven by test.
8. **Scripted bundle length prefix (resolved).** The scripted bundle
   digest now hashes the line lengths as little-endian `u64` and
   includes `InferenceSettings::bytes()` in the digest, so the bundle
   id is platform-independent and settings-sensitive. The earlier
   `usize`-width observation is retired.

## 19. Rung E3+jev: the Jev host-backend adapter

TypeSafe AI's Jev ("System One model", launched 2026-09-15) answers
typed, probabilistic questions instead of generating text. The
adapter puts Jev behind the §18 `ModelBackend` trait as a host-only
network client: one ReAct turn is one `POST` to
`https://api.typesafe.ai/v1/systemone`, and one parallel pass returns
the turn's decision as data, not prose.

### 19.1 The typed request contract

The adapter sends, per turn, a JSON body with three fields:

- `state`: the engine's rendered prompt, verbatim (the same compact
  prompt §18 builds: TOOLS block, STATE line, optional REPAIR line,
  truncatable LAST observation, EMIT trailer).
- `model`: the pinned version string (see §19.5).
- `questions`: three typed questions —
  - `decision`: a `Choice` over eight options — one per §17 catalog
    tool plus `ask` and `finish` — with up to 255 options supported
    by the primitive and eight used by the schema.
  - `should_ask`: a `Noul` — P(the run cannot proceed safely
    without human input).
  - `confidence`: a `Score` over four ordered levels
    (1 = guessing, 2 = uncertain, 3 = confident, 4 = certain).

Field order is fixed so the request bytes are deterministic: the
mock cassette keys are `fnv1a64` over the exact request JSON.

### 19.2 The design mapping

- **Choice → the turn's decision.** Jev selects one of the eight
  options; the adapter renders the decision line from a deterministic
  template (§19.4). The response's `choice` names the option and its
  `probabilities` carry the eight-way distribution, recorded on the
  receipt.
- **Noul → the ask gate.** At or above 0.50 (`NOUL_GATE_BPS`) the
  adapter overrides the choice with `ask`, whatever Jev chose: the
  run cannot proceed safely without human input. The receipt records
  both the raw choice and the gated outcome, so the override is
  auditable.
- **Score → calibrated confidence.** The fractional score (thousandths
  of a level) and the four per-level probabilities are recorded on
  every receipt. The documented escalation bands are: below 1.5 →
  human review; 1.5–3.0 → proceed with extra scrutiny; above 3.0 →
  act. The deterministic monitor does not consume these yet — the
  mapping is the escalation policy a later rung wires in.

### 19.3 The response contract and fail-closed parsing

A System One response carries `model`, `answers` (the three typed
answers, each with its distribution), and informational `usage`. The
adapter requires: the `model` equals the configured version
(§19.5); every question present with the right `type`; the choice
inside the eight-option schema; every probability in 0..=1; the
score in 1..=4 with all four level probabilities present. Anything
else — malformed JSON, a missing answer, an unknown option, an
out-of-range probability — fails closed to `BackendError`
(`PolicyMismatch` for integrity failures, `UnknownPrompt` for
out-of-schema answers), never to a guessed line.

The adapter parses responses with its own small host-side JSON
reader, not the `no_std` protocol parser: a response nests four deep
(`answers` → `decision` → `probabilities`), past the protocol
parser's `MAX_JSON_DEPTH = 3` denial-of-service bound, which stays
untouched. Duplicate keys are rejected; trailing bytes are rejected.

### 19.4 The cascade and its hard limitation

Jev selects *which* decision to take; deterministic arg templates
supply the concrete arguments. The template reads the turn's last
observation for numeric `pin`/`sensor` fields (defaulting to 0) and
renders the byte-exact grammar line; every other argument is a fixed
constant (`"high"`, `100`, `{}`, `"summary"`).

The limitation is structural and must be stated plainly: **Jev
cannot emit arbitrary values.** It never sees or produces a pin
number, a URL, or a JSON blob — every concrete value in an emitted
line comes from the caller-enumerated template or the last
observation. A decision outside the template space (a pin Jev
"chose", a free-form summary) is inexpressible through this backend,
loudly, by construction. Where the scripted backend can emit any
line (including adversarial ones) and the tiny backend looks up any
recorded line, the Jev backend can only ever emit the eight template
lines. A truncated observation fails closed to the defaults: the
templates do not act on partial evidence.

### 19.5 Model and evidence identity

- **Bundle identity.** `JevBackend::bundle_id` digests the adapter
  tag (`esper-jev-v1`), the model version string, and the endpoint.
  The seed binds this bundle exactly like §18's: a version change is
  a different bundle, and a response naming a different version than
  the backend was constructed with fails closed (`VersionMismatch`).
  The adapter pins `jev-1.13.0` so the digest is reproducible;
  the value is what public material resolves `jev-latest` to
  (2026-09-18), unverified against official documentation — the pin
  is a label, not a discovery.
- **Per-turn evidence.** Every inference appends a `JevReceipt`:
  the parsed answer (raw choice, gated choice, the gate bit, the
  eight choice probabilities, the Noul probability, the fractional
  score, the four level probabilities) plus the measured usage
  (input bytes read; output tokens are always zero — Jev generates
  no text and output is unmetered). The receipts are the run-record
  home for the returned probabilities.
- **Crash consistency.** `unemit` pops the last receipt. A crash
  before the decision commit re-infers the identical prompt, the
  mock serves the identical recorded response by request hash, and
  the run completes with exactly one receipt per turn.
- **Determinism caveat.** Jev answers are reproducible to about
  0.02, not bit-identical. The receipts record what was actually
  returned; the mock path is exact, the live path carries the
  caveat. Replay of a live run is evidence review, not
  bit-reproduction.

### 19.6 Transports: mock now, live deferred

- **`MockTransport`** replays recorded System One responses from a
  cassette: `{"model_version", "endpoint",
  "pairs": [{"request_hash": "<decimal u64>",
  "response": {…}}, …]}`. The hash is a decimal *string* because
  request hashes are full-width `u64`s, past what a JSON number
  carries exactly. An unrecorded request fails closed
  (`UnknownPrompt`) — the mock never guesses, the same rule as the
  tiny backend's distill table. Cassettes live in
  `spec/jev-cassettes/` as reviewed fixtures, authored by record
  mode (`ESPER_JEV_RECORD=1` drives the scenarios through a
  directing transport and writes the pairs).
- **`LiveTransport`** builds the documented `POST` (bearer auth,
  JSON body) but defers the send: without an API key — and without
  TLS in this host profile — every call fails with `LiveDeferred`.
  The request bytes it *would* send are exactly what
  `build_request_json` produces, pinned by golden tests, so the day
  a key arrives only the socket layer is new. **No API key is
  available; live verification is deferred until Mark provides one,
  and he is not being asked for it now.**

### 19.7 Why the 19 byte-exact fixtures stay scripted+tiny

The §14 fixture corpus (19 trajectories) asserts byte-exact model
lines, including adversarial ones: invalid arguments, unknown tools,
trailing garbage, NUL bytes, malformed verbs. A typed Jev mock can
only emit the eight template lines — it cannot produce, and must
not pretend to produce, the adversarial stream. So the 19 fixtures
keep running under Scripted and Tiny only, and the Jev suite
(`crates/esper-runtime/tests/jev.rs`) mirrors the key scenarios in
typed form instead: the happy path (read → write → finish), a
denied write, the Noul ask gate with suspend/resume, and a crash
before the decision commit. The §18 JSON repair policy is
unreachable-by-construction through the Jev backend (there is no
text to repair) and is retained as defense in depth.

### 19.8 Host-only boundary and resource notes

The adapter is a network client behind the `host` feature. Nothing
in it enters the `no_std`/`no_alloc` crates, the firmware story, or
the §18 target measurements: there is no Xtensa size, no RAM
budget, and no energy claim for a cloud call. Per-call cost follows
the Jev pricing ($0.042/MTok input, output unmetered) against the
measured input bytes on the receipts; stated latency is 70–500 ms
per the docs, unmeasured here. `cargo check/test -p esper-runtime
--no-default-features` stays green, and no Jev symbol is reachable
without `host`.

## 20. Rung E4: context lifecycle

The design doc's tiered context policy (§8 normalize/redact at
ingestion, mask superseded observations, retain the last five
interactions, compact at 80%, continue as new; §9 redaction before
durability; ADR 4 no persisted chain of thought; ADR 10 budgets are
run identity) becomes `esper-core` machinery in four modules —
`mask`, `compact`, `snapshot`, `lineage` — plus
`crates/esper-core/tests/context_lifecycle.rs` (30 tests). Everything
in this section is `no_std`, no allocation, core-only, edition 2024.

### 20.1 Masking: classes, boundaries, and the bound

`mask::mask_bytes(input, output) -> Result<usize, Error>` redacts in
a single pass and fails closed with `Error::MaskOutputTooSmall` when
`output` is smaller than `mask::mask_bound(input.len())`. A companion,
`mask::mask_report`, additionally returns a `MaskReport`
(`redacted_spans`, `secret_digest`, `output_len`) — the byte-level
function cannot carry the report, so both exist.

**Secret class** (`[redacted:secret#N]`), shape-based, ASCII:

- `sk-`, `sk_live_`, `sk_test_` prefixes plus a token tail of
  alphanumerics and `._~+/=-` of at least 16 characters.
- `github_pat_` plus a tail of at least 16 token characters.
- `AKIA` plus exactly 16 alphanumerics (AWS access-key-ID shape).
- `Bearer ` (case-sensitive; a lowercase `bearer ` is also matched)
  plus a token tail of at least 16 characters.
- PEM blocks: from `-----BEGIN ` through the end of the
  `-----END ...` line, with the closing marker required inside a
  2048-byte window — an unclosed `-----BEGIN ` is ordinary text.

**PII class** (`[redacted:pii#N]`), shape-based, ASCII:

- Email-shaped values: local part of alphanumerics plus
  `._%+-`, `@`, and a domain of alphanumerics, `.`, `-` with at
  least 4 characters and at least one dot; trailing dots are
  sentence punctuation, not the domain. Matching runs at each `@`;
  the output is rewound over the already-copied local part only
  when the output tail provably equals the input's local part (a
  secret match ending right before the `@` declines the email).
  Addresses abutting without separators (`a@b.coa@b.co`) are
  scanned greedily: the local part is always consumed into a
  redacted span, so no local part is ever left visible — only
  inert domain residue remains.
- Phone-like digit runs: optional `+` then digits with ` .-()`
  separators, 7–15 digits total. A span with no separators must be
  at most 11 digits (timestamps are not phone numbers); anything
  separator-shaped in the 7–15 range is redacted.

Marker numbering is 1-based per call, shared across both classes.
Non-ASCII bytes pass through untouched (UTF-8 multibyte sequences
never match an ASCII class). Matching is shape-based, not semantic:
anything shaped like a secret or like PII is redacted, and the eval
crew measures the false-positive rate (§20.7).

**The output bound** is a proof, not a guess. Every input byte is
either copied (1 byte) or belongs to a redacted span of at least 6
bytes (`a@b.co`, the shortest accepted shape) replaced by a marker
of at most 28 bytes (`[redacted:secret#` + up to 10 decimal digits
+ `]`), so

```text
mask_bound(n) = n + (28 − 6) · (n / 6)
```

saturating. The 10-digit term assumes span indices stay under ten
decimal digits (inputs below ~60 GiB); `mask_bytes` still fails
closed if the bound ever proves insufficient.

**Durable replacement record.** What the journal keeps instead of
the raw bytes is the opaque marker plus digests: `MaskReport`
carries `secret_digest`, FNV-1a-64 over the concatenated redacted
*secret* bytes in redaction order (`0` when no secret span was
redacted; PII bytes are excluded), so a later crew can correlate
"the same secret appeared again" without ever storing the secret.
The compacted frame view (§20.2) records the tool ID, the argument
digest, the outcome class, and the digest of the redacted payload
bytes — never raw secrets. `mask::fnv1a64` delegates to
`Digest::of_bytes` so the algorithm is not duplicated.

### 20.2 Compaction: the durable compact state

`compact::CompactState` is the design §8 durable compact state:
objective digest, remaining `ResourceBudget`, `VersionSet`
(workflow / model / catalog / policy), plus bounded lists —
completed subgoals (8), open subgoals (8), accepted-decision
digests (8), facts with source frame IDs (16), failed paths (16),
pending approvals/verifications (8), and recent event fingerprints
(16). Notes and facts are bounded text (`NOTE_MAX` = 64 bytes);
`Note::from_bytes` fails closed on overlong input while
`Note::truncated_from` truncates, so a long observation can never
fail a compaction.

`compact::compact(frames, policy, state)` folds `FrameSummary`
views (already masked — masking is the runtime's job at ingestion,
before durability, §9) into the state and returns a
`CompactionReport` (`frames_in`, `frames_compacted`, `tail_kept`,
`bytes_saved_estimate`). Trigger: `should_compact` fires at
`trigger_pct` of the context budget; `DEFAULT_POLICY` is 80% with a
5-interaction verbatim tail. The tail is a prompt-construction
optimization for the current run, not durable state: facts,
completed subgoals, and accepted decisions fold from *all* frames
so the compact state stands alone at a `continue_as_new` boundary.
Folding is idempotent across overlapping windows — failed paths,
obligations, and facts dedupe, so re-feeding frames is safe.

**Preservation invariant (a): failed paths are never dropped.**
Every `(tool, args_digest, error)` failure tuple is recorded;
`record_failed_path` fails closed with `Error::CompactStateFull`
rather than dropping a new path. A resumed run can never retry a
ruled-out path.

**Preservation invariant (b): open obligations are never
dropped.** Pending approvals and verifications record under the
same fail-closed discipline and leave only through
`resolve_pending`.

Positive knowledge is bounded institutional memory with different
discipline: facts, completed subgoals, and decisions drop the
*oldest* entry when full. That is deliberate — (a) and (b) are the
must-preserve sets; facts can be re-observed, obligations cannot
be re-invented.

Event fingerprints are FNV-1a-64 over frame identity fields
(sequence, record kind, tool, argument digest, outcome, progress) —
never over payload bytes — feeding the §5 loop detector across
compactions.

### 20.3 Snapshots: versioned, integrity-checked encoding

`snapshot::encode(state, out)` writes the canonical little-endian
layout, exactly `snapshot::encoded_len(state)` bytes:

```text
[version: 1][payload][integrity: u64 LE]
```

The payload encodes, in fixed order: objective digest (`u64`);
budgets (`model_turns u16`, `input_tokens u32`,
`output_tokens u32`, `elapsed_ms u64`, `radio_bytes u32`,
`mutations u16`, `consecutive_errors u8`); versions
(`workflow u32`, `model/catalog/policy u64`); then each bounded
list as `count u8` followed by its entries (notes as `len u8` +
bytes; facts add `source_seq u32`; failed paths as
`tool u8, args_digest u64, error u8`; pending items as
`kind u8, seq u32, digest u64`; fingerprints as `u64`).
`integrity` is FNV-1a-64 over `version || payload`.

Decoding fails closed, in order: `Error::SnapshotTruncated` for
short input (including a short checksum), then
`Error::SnapshotVersionMismatch { found }` for a foreign version
byte — version is checked before the checksum, so a version bump
reads as a version problem, never a corruption problem — then
`Error::SnapshotChecksumMismatch`, then
`Error::SnapshotCorrupt` when the payload passes the checksum but
does not decode to a valid state (overlong note, count beyond a
list's capacity, unknown obligation kind or error code, trailing
bytes). `snapshot::verify` performs the length/version/integrity
checks without building the state, for the runtime's read-back
verification after the snapshot activity writes. `encode` fails
closed with `Error::SnapshotBufferTooSmall` when `out` is short.

### 20.4 Lineage: `continue_as_new` and never-widen budgets

`lineage::Lineage` links the new run to its parent: `parent_run`
(`RunId`), `continued_at_frame`, `budgets_remaining`, and the
`VersionSet` the new run's seed states. `continue_as_new(state,
lineage)` inherits the compact state with budgets replaced by the
lineage's remaining budgets; fingerprints, failed paths,
obligations, facts, and versions ride along untouched — the new run
continues the same task, so its loop detector keeps recent history.

Budgets are *remaining* and never widened (ADR 10): the
continuation fails closed with `Error::BudgetWidened` when the
lineage grants more than the compacted state holds in any unit
(`ResourceBudget::check_no_widen`, E2 §17.4). Version
compatibility is *not* gated here — the runtime's `RunSeed`
validation owns the version gate (design §4); the lineage carries
the versions so the new seed can state them.

### 20.5 The stale-reference rule

Superseded frame payloads are replaced by a **stale-payload
compact marker**: the runtime swaps a folded frame's full bytes for
a compact marker naming the compact epoch it was folded into
(e.g. the folded frame's sequence range), and the marker is what
any later reference resolves to. Concretely:

- A reference to a compacted frame's payload never resolves to
  the original bytes — they are gone from the context by
  construction.
- A reference to a compacted frame's *identity* (sequence number,
  tool, outcome class, argument digest) resolves through the
  compact state: failed paths via `failed_paths()`, obligations
  via `pending()`, decisions via `accepted_decisions()`.
- A reference to a frame inside the verbatim tail resolves to the
  tail's masked bytes as usual.

There is no dangling reference: every lookup either hits the
tail, the compact state, or the stale-payload marker, and the
marker itself carries the epoch so a crew can say "folded at
compaction N" instead of chasing bytes that no longer exist.

### 20.6 Version mismatch and the migration boundary

`SNAPSHOT_VERSION` is 1. A snapshot whose version byte differs
fails closed with `SnapshotVersionMismatch { found }` — the new
run refuses to start from a state it cannot interpret. Bumping the
version requires a migration path written, tested, and specified
in this section *before* the bump lands; until then, the boundary
is: unknown version ⇒ refuse, loudly, with the found version in
the error. No silent reinterpretation, no best-effort parse.

### 20.7 Measurement methodology (partial results 2026-09-22)

The E4 exit criterion is behavioral: long tasks survive journal
rollover and reboot without repeated failed paths or lost
obligations. The eval crew (a later rung) measures, per
trajectory:

| Metric | Method | Result |
|---|---|---|
| Full-history context bytes | sum of frame payload bytes before compaction | long-success: 1163 B (38 frames); failure-heavy: 513 B (14 frames); secret-bearing: 407 B (9 frames) |
| Masked-tail context bytes | bytes after `mask_bytes` over the folded region | long-success: 194 B; failure-heavy: 223 B; secret-bearing: 199 B |
| Compact-state bytes | `snapshot::encoded_len` of the resulting state | long-success: 269 B; failure-heavy: 223 B; secret-bearing: 165 B |
| Failed-path repeats after rollover | count of retried `(tool, args_digest)` in `failed_paths()` | — (pending runtime rollover; fixture `x` staged) |
| Lost obligations after rollover | `pending()` items unresolved at the new run's first ask gate | — (pending runtime rollover; fixture `w` staged) |
| Masking false-positive rate | redacted spans over benign-shape inputs, human-judged | 10% — 2 of 20 benign inputs redacted (`{"uptime_ms": 1234567}`, an ISO date) |

Note on the byte tiers: `compact < masked-tail` is
trajectory-dependent, not universal — the snapshot's fixed overhead
dominates the five-frame masked tail on small-frame runs
(long-success: 269 B compact vs 194 B tail; failure-heavy: 223 B vs
223 B). The robust claim the harness asserts is carried context
(compact state + masked tail) strictly below full history, with the
gap widening as runs lengthen.

The methodology is specified here; byte and false-positive
results are filled as measured, and the rollover rows await the
runtime integration. Byte-measurement tests live in
`crates/esper-core/tests/context_lifecycle.rs` (compaction byte
savings, snapshot exact length); corruption, stale-reference, and
version-mismatch behavior are tested there too (single-bit flips
across payload and checksum regions, truncation at every
boundary class, foreign version byte). The §20.7 methodology
harness — three representative trajectories with printed table
rows, the false-positive probe, and the negative tests — lives in
`crates/esper-eval` (`src/measure.rs`, `tests/e4_measure.rs`,
`tests/e4_fp_probe.rs`, `tests/e4_negative.rs`). What E4 does *not* claim:
no reboot test yet (the runtime owns journal rollover), no
measured false-positive rate, no firmware size numbers for the
new modules — those arrive with the runtime integration and the
eval crew.

### 20.8 Runtime wiring (E4)

The runtime integrates the E4 context lifecycle (masking,
compaction, snapshot verification, `continue_as_new`) into the
durable ReAct loop. The public API is `drive_segment` (one driver
lifetime), `SegmentOutcome` (`Completed` / `Suspended` /
`Rollover`), and `RolloverHandoff` (the verified parent fold).
`drive_run` chains segments; each child owns a fresh journal.

**Seed identity.** The 142-byte canonical seed snapshot binds
`context_budget_bytes` (the rollover trigger) as a tagged
`Option<u64>` alongside the parent lineage. A seed with a budget
differs from one without; a child differs from its parent; two
children folded at different frames differ from each other.
Recovery compares the snapshot byte for byte — a reboot can never
widen the budget or swap the model.

**Masking.** `do_observe` masks the tool outcome before it reaches
the journal, the Waymaker cursor, `last_observation`, or the next
prompt. Each `ToolObservation` frame carries its `secret_digest`
(`0` for no secret); the trace accumulates one digest per
observation in journal order. A reboot rebuilds the real digest
stream from the frames — the raw secret bytes are gone by design,
but the host's digest record survives.

**Rollover.** `do_gather` fires the 80% trigger when the seed
carries a context budget. The fold summarizes frames, compacts with
`DEFAULT_POLICY`, encodes, read-back verifies, and runs
`continue_as_new` (the no-widen proof) before returning
`SegmentOutcome::Rollover`. The handoff carries the verified
snapshot bytes, the lineage (parent run, cut frame, remaining
budgets, versions), the segment's prompt bytes, and the digest
stream.

**Continuation.** `RolloverHandoff::child_seed` derives the child
with narrowed budgets, a shrunken context budget
(`saturating_sub` — the parent's spend never widens the child), and
the parent lineage. The child boots on a fresh journal; the
snapshot re-verifies on every boot, and corrupt bytes fail closed
before a single frame replays. `drive_segment` enforces the
pairing: a child seed requires a handoff; a root forbids one.

**Failed paths.** The fold carries ruled-out `(tool, args_digest)`
pairs. `do_authorize` refuses an identical call before it burns a
turn or touches hardware — the run degrades with
`failed_path_ruled_out` instead of re-proving the failure.

**Prompt metering.** Each `ModelDecision` frame carries its
prompt's byte length. The meter sums the journal on boot, so
suspend/resume across `drive_segment` calls keeps the 80% trigger
exact. A new segment starts at zero; the child's budget is the
parent's minus the parent's spend.

**Reference posture.** A continued prompt never carries raw
folded bytes: any reference to a compacted frame's payload renders
as `[stale:folded@epoch=N]`, naming the fold epoch. The `PriorCtx`
summarizes the inherited fold (failed/pending/fact counts) for the
child's prompt. The full three-case resolver (compacted payload →
stale marker; compact identity → `CompactState` lookup; active-tail
payload → masked bytes; never invent content) is specified but not
yet implemented as a unified function — the current wiring covers
the PRIOR summary and the stale marker.

**What E4 does not claim.** The masking scope is tool observations
only; model-emitted secrets in decisions are not yet redacted at
ingestion. Multi-rollover chains (grandchild segments) are
untested. The eval crew's staged fixtures (`w`, `x`) exercise the
runtime through the harness; the byte-measurement rows in §20.7
await the integrated run.
