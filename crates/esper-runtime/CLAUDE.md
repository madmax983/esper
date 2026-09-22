# `esper-runtime` — the durable ReAct runtime (E0/E1/E2)

## What this crate is

Esper's agent harness: a durable ReAct loop (one tool call, one
typed human input, or finish per turn) built on `esper-core`,
`esper-protocol`, and Waymaker 0.1.0. The model backend, fake device
(GPIO + sensors + virtual clock), fault injector, and human-input
queue are host-only doubles; the engine, journal, and durability
protocol are the real slice. E2: six typed tools from the static
protocol registry; authorization before dispatch; independent
read-back verification for every mutation; effect-ID dedup on all
dispatches.

## Where things live

- `src/lib.rs` — crate root, driver entry points (`drive_run`,
  `drive_run_async`).
- `src/engine.rs` — the boundary loop: turn → authorize → dispatch →
  verify → account, with crash recovery as journal replay.
- `src/journal.rs` — `Frame` / `Journal`: the only durable state.
  Everything else is derived by replay.
- `src/seed.rs` — `RunSeed`: explicit budgets, capabilities, version.
- `src/world.rs` — host doubles: `ScriptedModel`, `FakeDevice`,
  `FaultPlan`, `InputPlan`.
- `src/trace.rs` — `RunTrace`: the eval harness's evidence, derived
  from the journal.
- `src/error.rs` — `CrashPoint` (11 variants), `Halt`, `RuntimeError`.
- `tests/trajectories.rs` — golden trajectories a–h (SPEC §14).
- `tests/crash_matrix.rs` — one reboot per crash point.
- `tests/invariants.rs` — the durability contract.

## Authority order

1. `~/workspace/esper/SPEC.md`
2. `~/workspace/esper/spec/trajectories/`
3. existing `esper-core` APIs (never change them; flag if one blocks you)
4. published Waymaker 0.1.0 APIs (`ReplayCursor`, not `ReplayMachine` —
   the latter does not exist in 0.1.0)

## How the engine works

One turn commits in this order, each step crash-injectable:

1. **Infer** — take a scripted line, commit `ModelDecision`, consume a
   turn, decode. Malformed lines repair twice, then end `ModelInvalid`.
2. **Authorize** — capability check first, then device direction; denial
   is terminal `Denied` and never touches hardware.
3. **Dispatch** — commit `ToolIntent` (new effect id), then the physical
   effect, then commit `ToolObservation`. Transient failures retry
   under the same id (max 3 attempts).
4. **Verify** — mutating tools get an independent read-back; the
   verifier reads pin state, never writer bookkeeping.
5. **Account** — feed the monitor; its verdict (or the budget guard)
   decides `Gather` vs `Degraded`.

`Ask` commits the decision, suspends durably, and resumes when typed
input arrives — across reboots, because the journal is the only state
that crosses a boot.

## Crash model

`drive_run(..., crash: Some(point))` fires the crash once at that
boundary: all in-memory state is dropped and `recover()` replays the
journal — budgets, monitor, effect ids — then a `ReplayCursor` walks
the Waymaker record stream derived from the same frames and
cross-checks the pending-effect set. Any divergence is
`RuntimeError::ReplayDiverged`, never a silent retry.

The doubles model the physical world: `FaultPlan` consumption and
`FakeDevice` pin state survive crashes. Only runtime memory is lost.

## Rules for working here

- RED first: trajectories and crash tests before engine changes.
- `// HOST-ONLY (E0/E1)` on every host-only allocation site.
- No `unwrap`/`expect`/`todo!`/`unimplemented!` outside tests.
- `#![deny(unsafe_code)]`; doc comments on every public API.
- `cargo fmt --check`, workspace `cargo clippy --all-targets -- -D
  warnings` (pedantic + nursery), and
  `cargo check -p esper-runtime --no-default-features` must all pass.
- Golden fixtures are the contract: terminal strings, reasons, and
  remaining budgets must match `spec/trajectories/` exactly.
