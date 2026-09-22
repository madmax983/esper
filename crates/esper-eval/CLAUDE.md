# `esper-eval` — the golden-fixture evaluation runner (E0/E1/E2)

## What this crate is

The host-only harness that turns SPEC §14's trajectory fixtures
(`spec/trajectories/*.json`) into executable tests. For each fixture
it parses the JSON, builds the `RunSeed`, scripted model, fake device,
fault plan, and input plan the fixture describes, drives
`esper-runtime` to `End` via `drive_run` — injecting every planned
reset and letting the engine recover by journal replay — and then
asserts the exact expected trace, the remaining budgets, the SPEC
§11.3 crash-oracle invariants, and every `forbidden` clause.

One `#[test]` exists per fixture file, generated at compile time by
`build.rs` from the fixture directory: adding a fixture file adds a
test with no source change (verified: a scratch ninth file produced
`trajectory_z_temp_ninth`, then was removed).

E2 adds the three new tool families (sensor, timer, status) to the
fixture schema, the runner, and the checks — see "E2 surface" below —
plus an adversarial meta-test (`tests/adversarial.rs`) that proves
every adversarial fixture expects zero `ToolRequest` frames.

## Where things live

- `src/lib.rs` — crate root; re-exports `parse_fixture`,
  `run_fixture`, `run_fixture_file`, `Fixture`, `FixtureError`,
  `FixtureReport`.
- `src/json.rs` — small host-only RFC 8259 parser (line/column
  errors, duplicate-key rejection, canonical object equality and
  rendering). No serde dependency, by design.
- `src/fixture.rs` — typed fixture model and path-aware parser.
  Schema failures name their JSON path (`$.expected.trace[3].tool`);
  unknown tools, statuses, crash points, invariants, and forbidden
  clauses fail closed. Adjacent `VerificationRequest` +
  `VerificationResult` records normalize into one runtime
  verification event.
- `src/runner.rs` — builds the deterministic `RunId` (from the
  fixture id), seed, model, device, fault plan, inputs, journal, and
  crash injection; drives the runtime; checks terminal status,
  budgets, trace, invariants, and forbidden clauses. Returns the
  documented `FixtureReport`.
- `src/compare.rs` — exact trace comparison. Runtime traces are
  decoded semantically: model lines go back through
  `esper_core::decision::decode_line`, JSON fields compare
  canonically, tool ids resolve to canonical names, and effect-id
  *relationships* are checked without comparing absolute ids (the
  allocator mints those). The first mismatch renders as a readable
  event-by-event diff.
- `src/checks.rs` — SPEC §11.3 invariants as typed checks, plus the
  closed-vocabulary `forbidden`-clause parser and evaluator.
- `build.rs` — scans `../../spec/trajectories/*.json`, emits one
  integration test per file into `$OUT_DIR/fixture_tests.rs`.
- `tests/fixtures.rs` — includes the generated tests.
- `tests/adversarial.rs` — meta-test: every fixture whose file name
  contains `adversarial` must expect zero `ToolRequest` frames in
  its `expected.trace`. Static, document-level, independent of trace
  equality.

## E2 surface

- `capabilities` gains `sensors: [0..=3]` (default `[]`),
  `allow_timer: bool` (default `false`), `allow_status: bool`
  (default `false`). Missing E2 fields mean denied — fail closed,
  so the E0/E1 fixtures keep their meaning unchanged (SPEC §17.7).
- Tool names resolve against the E2 contract catalog via
  `esper_protocol::contract::lookup_by_name` — the one table every
  tool name comes from (SPEC §17.1) — so the eval vocabulary is
  exactly the six catalog names.
- `device.faults[].tool` is optional: absent means the fault applies
  to any tool (SPEC §17.7). The fault resource comes from
  `match_args` per §17.7 — `pin` for GPIO tools, `sensor` for
  `sensor_sample_read`, neither for timer/status tools (resource 0) —
  and a tool filter that disagrees with the resource fails closed.
  A `stuck_level` fault on a non-pin resource fails closed (a stuck
  actuator is a GPIO concept).
- New forbidden clause: `a {ordinal} clock advance`
  (`Forbidden::MaxClockAdvances`) — the timer twin of the
  physical-write bound. It fails closed until the fake device
  exposes a clock-advance ledger (runtime API gap).
- The runner fails closed when a fixture grants E2 capabilities the
  runtime `RunSeed` cannot carry yet, or targets a fault at a
  sensor/clock resource the `FaultPlan` cannot match by — both gaps
  are named, never silently approximated.

## Authority order

1. `~/workspace/esper/SPEC.md` (§§11, 14.3, 17)
2. `~/workspace/esper/spec/trajectories/` (the fixtures are the test
   list and the expected values)
3. existing `esper-core` / `esper-runtime` APIs — never change them;
   report gaps instead of working around them silently

## How a fixture run works

1. **Parse** — `parse_fixture` validates the whole document against
   the closed fixture schema. Anything unknown fails closed with a
   path.
2. **Build** — deterministic `RunId` from the fixture id; `RunSeed`
   from `run_seed`; `ScriptedModel` from `script`; `FakeDevice` with
   all eight pins defaulted to input/low, then explicit fixture
   directions applied; `FaultPlan` from `device.faults`; `InputPlan`
   from `input_events` in `after_decision` order.
3. **Drive** — `drive_run` with the single planned `CrashPoint`
   (at most one per fixture; see gaps). The engine reboots and
   replays; the eval just observes the committed trace.
4. **Assert** — terminal status string, exact event trace (kinds in
   order, fields canonically equal), remaining model-turn and
   mutation budgets, every §11.3 invariant, every `forbidden` clause.

## Rules for working here

- `// HOST-ONLY (E0/E1)` on every host-only allocation/code site —
  this entire crate is host-only and must never enter a firmware
  image.
- No `unwrap`/`expect`/`todo!`/`unimplemented!` (or `unwrap_or` /
  `unwrap_or_else` / `map_or`-identity) outside `#[cfg(test)]`
  modules. Test code may use `expect`.
- `#![deny(unsafe_code)]`; doc comments on every public API.
- `cargo fmt --check`, workspace
  `cargo clippy --all-targets -- -D warnings` (pedantic + nursery),
  and `cargo test --workspace` must all pass.
- Fixture files are the contract: terminal strings, reasons, and
  remaining budgets must match `spec/trajectories/` exactly. If a
  fixture and the runtime disagree, the fixture wins and the
  divergence is reported — never silently absorbed.

## Known runtime/API gaps (do not paper over)

These are `esper-runtime` limitations the eval works around by
failing closed; report them, don't hide them:

- `drive_run` takes one `Option<CrashPoint>` and fires it once.
  Fixtures with several crashes or `times > 1` are rejected at parse
  time.
- `FakeDevice` has no initial-level setter: an initially-high pin
  fails closed (all shipped fixtures start low).
- `FaultPlan::consume` matches by pin, ignoring the tool: two
  transient faults on one pin fail closed.
- `InputPlan` is only a queue: arbitrary `after_decision` schedules
  can't be enforced (fixture h's one Ask / one input works).
- The trace exposes remaining turns and mutations only — input/output
  token and elapsed-time budgets are carried but not metered or
  exposed per reboot.
- Reboot checkpoints are internal to `drive_run`; effect stability
  is inferred from journal/trace relationships, not sampled per
  reboot.
- The trace/journal carry no progress deltas and no canonical
  invalid-args violation vocabulary, so the `progress` annotation
  and the free-text `violation` note are parsed but not asserted
  (documented in `compare.rs`).
- E2 wiring (landed with crews B/C): the runner builds the E2
  `RunSeed` (`readable_sensors` bitmask, `allow_timer`,
  `allow_status`) straight from the fixture capabilities, and installs
  transient faults as `(tool filter, resource)` pairs via
  `fail_transient_for` per SPEC §17.7. Two faults whose targets
  overlap fail closed (first-match consumption), as does a
  filter-less fault on a sensor/clock resource (the plan cannot tell
  resource 1 the sensor from resource 1 the pin).
- `MaxClockAdvances` counts committed `timer_delay_wait` intents in
  the trace: each one dispatches exactly one physical advance
  (redeliveries dedup and never re-commit).
