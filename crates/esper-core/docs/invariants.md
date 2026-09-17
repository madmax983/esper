# esper-core invariants

Machine-checked where noted; the rest are code-review invariants. All
reference `SPEC.md` at the workspace root.

## State machine (§3.2)

- `End` has no outgoing transitions. (`transition_is_total` test:
  `End × Event` is always `Err`.)
- Only `Finalize`, `Degraded`, and `SafeStop` reach `End`
  (`FinalizeCommitted`, `DegradedCommitted`, `SafeStopCommitted` are the
  only edges into `End`).
- `Infer` cannot reach `Observe`: its four edges go to `Authorize`,
  `AwaitInput`, `Finalize`, or `Repair` — never directly to `Observe`,
  which is reachable only from `Authorize`.
- `AwaitInput` exits only through committed input to `Account`
  (`InputArrived` is its sole legal event).
- Exactly 25 legal `(from, event)` pairs; `transition` rejects anything
  else with `Error::IllegalTransition`. (`transition_is_total_and_
  rejects_every_unlisted_pair` pins the count.)
- `committed_records` names the journal writes of every legal edge, and
  commits nothing for illegal pairs. The two `&[]` arms sharing a body
  is intentional (see the `#[allow]` comment on the function): the table
  must enumerate every legal edge explicitly for §3.2 audit.
- §3.2 conditions that need runtime data (budget checks, repair counts,
  transient attempt counts) are **not** decided here: the caller derives
  them from committed outcomes and picks the matching `Event`.

## Decisions (§4.2–4.3)

- `ToolCall { tool, args, args_digest }`, `InputRequest { prompt,
  response_schema_id }`, and `FinalAnswer { status, summary }` keep
  **public** fields, exactly as SPEC §4.1 shows them.
- `args` is *validated borrowed JSON bytes*, not canonical JSON: the
  decoder validates strictly (depth ≤ 3, ≤ 8 keys, no duplicates, no
  trailing bytes) and digests the bytes as emitted (FNV-1a). Whitespace
  and escape forms are preserved, so equal digests mean byte-equal
  inputs.
- Two JSON bounds, deliberately different: `MAX_ARGS_BYTES = 256` is
  §4.3's literal bound for `CALL` argument blocks; `ASK`/`FINISH`
  envelopes use `MAX_JSON_BYTES = 384` because the envelope must contain
  a bounded 256-byte field (`MAX_PROMPT_BYTES` / `MAX_SUMMARY_BYTES`) —
  a 256-byte envelope could never hold one. Overlong envelopes are the
  `JsonTooLong` rejection (class `Malformed`); overlong `CALL` args stay
  `ArgsTooLong`. Do not merge the bounds.
- The line grammar is strict: `VERB` + single spaces, exactly one JSON
  value, no trailing bytes (`TrailingBytes`), one decision per turn.

## Registry and authorization (§5)

- The catalog is static: exactly `gpio_pin_read` (id 1) and
  `gpio_pin_write` (id 2). Unknown names fail at decode; unknown ids
  fail at lookup. There is no dynamic tool installation in E0/E1.
- `authorize_capability` is the static half of `Authorize`: allowlist,
  pin range (proven by holding a `Pin`), capability-set membership — in
  that order, so denials never touch hardware. Device business rules
  (pin direction) belong to the runtime's `Authorize` step.
- Every check fails closed: unknown tool, out-of-range pin, missing
  capability, or malformed counts → `Err`, never a default-allow.

## Budgets (§7)

- `ResourceBudget::meet` is field-wise monotonic: meeting a requirement
  consumes from the current budget and never increases any unit.
- `check_no_widen` guarantees a derived budget never exceeds the run
  identity on any guarded unit. The identity itself is part of the run
  seed; widening it is a version-binding violation, not an upgrade.
- `radio_bytes: 0` is rest, not exhaustion: charging a positive amount
  against zero fails, but zero itself is a legal state.

## Monitor (§8.3)

- Rule order in `observe` is load-bearing: budget guard →
  five-consecutive-`NoProgress` → three identical failures →
  three-of-five error majority. Earlier rules preempt later ones.
- The no-progress streak counts **any** step with
  `ProgressDelta::NoProgress`, whatever its outcome; any other progress
  variant resets it.
- The error-majority rule needs a full five-event window before it can
  fire; the identical-failure rule needs three consecutive identical
  `(tool_id, args_digest, error_class)` tuples.
- Monitor state is bounded: a five-record ring plus saturating counters.
  No allocation, no history growth.
- Verification failure never auto-compensates: `VerifyFail` commits a
  `VerificationResult` and returns to the loop; the monitor bounds how
  many retries the model may attempt (fixture
  `spec/trajectories/e-repeated-identical-failure-then-stuck.json`).

## Records (§10)

- `RecordKind` codes and `max_len` bounds are stable and compile-time;
  terminal reasons ≤ 128 bytes, summaries ≤ 256 bytes, all UTF-8.
- The terminal result is emitted once logically per run.

## Repair-budget note (open SPEC wording)

SPEC §3.2 says `repairs_used < 2` retries while `>= 2` degrades, and
elsewhere "the third invalid output" degrades after "two bounded repair
turns". The fixture
`spec/trajectories/b-invalid-output-repair-then-modelinvalid.json` is
the normative behavior: two repair turns are allowed, the third invalid
output degrades. The counting itself lives in the runtime (derived from
committed outcomes); the state machine only exposes the
`RepairAvailable` / `RepairExhausted` events.
