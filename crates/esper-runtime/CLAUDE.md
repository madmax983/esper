# `esper-runtime` — the durable ReAct runtime (E0/E1/E2/E3/E4)

## What this crate is

Esper's agent harness: a durable ReAct loop (one tool call, one
typed human input, or finish per turn) built on `esper-core`,
`esper-protocol`, and Waymaker 0.1.0. E3: the model interface is the
object-safe `backend::ModelBackend` trait (`infer` / `bundle_id` /
`last_usage` / `unemit`) with two backends — `ScriptedBackend`
(host-only canned lines, the E0/E1 `ScriptedModel` formalized) and
`TinyBackend` (a distillation stand-in: a caller-owned prompt→line
table keyed by `fingerprint_prompt`, no trained weights — see
`docs/adr/0001-e3-tiny-backend-distillation-standin.md`). The fake
device (GPIO + sensors + virtual clock), fault injector, and
human-input queue are host-only doubles; the run workflow (SPEC §22,
`run::run`) and its durability protocol are the real slice, in the
firmware build. E2: six typed tools from the
static protocol registry; authorization before dispatch; independent
read-back verification for every mutation; effect-ID dedup on all
dispatches. E4: context lifecycle — masking at the observation
boundary, 80% rollover with `continue_as_new`, snapshot
verification, per-segment journal ownership, and the
`drive_segment` / `SegmentOutcome` / `RolloverHandoff` API.

## Where things live

- `src/lib.rs` — crate root, driver entry points (`drive_run`,
  `drive_run_async`, `drive_segment`).
- `src/backend.rs` — the E3 model interface: `ModelBackend` trait,
  `ScriptedBackend` (host-only), `TinyBackend` (distillation
  stand-in), `PromptCtx` / `RepairHint` / `build_prompt`,
  `InferenceSettings`, `BundleId` / `fnv1a64`, `BackendError`,
  `TokenUsage`, `DistillEntry`, `PROMPT_CAP` / `OUTPUT_CAP`,
  `TINY_PARAMS_BYTES`. Not host-gated; re-exported from the crate
  root (`ScriptedBackend` only under `cfg(feature = "host")`).
  E4: `PriorCtx` (the folded-history summary) and the
  `[stale:folded@epoch=N]` marker rendering.
- `src/jev/mod.rs` (+ `src/jev/https.rs`) — the E3+jev `System One`
  adapter (host-only): `JevBackend` over the `JevTransport` trait,
  `MockTransport` (request-hash cassettes) / `LiveTransport` (real
  blocking HTTPS POST via `rustls` + Mozilla roots; caller-supplied
  key, zeroized on drop; `unconfigured()` still fails honestly as
  `LiveDeferred`), `build_request_json`, typed response parsing with
  the `Noul` ask gate, deterministic arg templates, `JevReceipt`
  per-turn evidence, `Prob` / `ScoreBand` / `RecordedAnswer` /
  `synthesize_response`. SPEC §19 (live-verified 2026-09-22:
  `api.typesafe.ai`, top-level `{state, model, questions}`,
  `jev-1.13.0`, ~880–950 ms/turn).
- `src/run/mod.rs` — SPEC §22: **the run, in the firmware build.**
  `run` is an `async fn` over `waymaker_embassy::Ctx` walking the
  normative machine; every world touch is an activity; state is the
  caller-owned `RunState` (pinned ≤ 12 KiB; it carries the run's
  `Harvester`, SPEC §23.3). The 80% rollover folds
  frame by frame, stores the snapshot in chunks and
  `continue_as_new`s; the failed-path guard is in `authorize_call`.
- `src/run/wire.rs` — the run's activity kinds (`INFER`, `PROBE`,
  `TOOL`, `VERIFY`, `ASK`, `SNAP_STORE`, `SNAP_LOAD`), `RunStart`,
  `TerminalRecord`, `Fault`; every maximum `const`-asserted ≤ 480.
- `src/run/device.rs` — the run's dispatcher: `RunDispatcher` over a
  board's `RunPorts` (model, one tool attempt, pin direction,
  read-back, operator input, snapshot chunks) — it alone decodes,
  re-validates (`bind`) and masks (`observe`) — plus `infer`,
  `validate_ask_input`, `parse_delay_outcome`.
- `src/run/host.rs` — HOST-ONLY: the journal double with the crash
  point → boundary table, the dispatcher over the world doubles,
  `drive_segment` / `drive_run` / `drive_run_with_reset`, the derived
  frame view.
- `src/agent/mod.rs` — E4+aon (SPEC §21): the always-on supervisor,
  **in the firmware build**. `supervise` is an `async fn` over
  `waymaker_embassy::Ctx` (`no_std`, no alloc): each beat awaits a
  durable `ctx.timer` (`Heartbeat::Boot` / `Persistent`), senses the
  world, runs at most one goal, settles, harvests, emits a `Pulse` to a
  `PulseSink`, and every `checkpoint_every` beats stores the state item
  by item and `continue_as_new`s. `AgentSeed` (identity, bound by
  digest in `Start`), caller-owned `AgentState`, `AgentError`.
- `src/agent/wire.rs` — the supervisor's activity kinds (`SENSE`,
  `RUN`, `HARVEST`, `STORE`, `LOAD`) and page-bounded codecs
  (`Sense`/`Command`, `RunRequest`/`RunReport`, `HarvestEntry`, item
  keys). Every maximum is `const`-asserted `≤ PAGE_RECORD_MAX` (480).
- `src/agent/device.rs` — SPEC §23: **the board dispatcher**, product
  code. `Device` serves `SENSE`/`RUN`/`HARVEST`/`STORE`/`LOAD` over a
  `RunStore` (one Waymaker journal per run; flash on the board) and
  `DevicePorts`; `RUN` drives `run::run` as a nested workflow.
- `src/agent/host.rs` — HOST-ONLY doubles: `MemJournal` (replay,
  redelivery, both clocks, `continue_as_new`, `CrashAt` injection),
  `HostAgent` (the product `Device` over `HostRuns` +
  `HostPortsDevice`, run crash points armed per spawn), `PulseLog`,
  `drive`.
- `src/journal.rs` — HOST-ONLY: `Journal` holds a segment's Waymaker
  records; `Frame` is the golden-trace view **derived** from them.
  Waymaker's journal is the durable state.
- `src/seed.rs` — `RunSeed`: explicit budgets, capabilities, version.
  E3: `model_bundle: u64` and `inference: InferenceSettings`; the
  journal-binding snapshot is a 67-byte canonical encoding
  (SPEC §18.7) — recovery compares it byte for byte, so a reboot can
  never swap the model or the prompt/output caps. E4: the snapshot
  is 142 bytes, binding `context_budget_bytes` (the rollover
  trigger) and the `parent` lineage; `child_seed` derives the
  continuation with narrowed budgets.
- `src/world.rs` — host doubles: `FakeDevice`, `FaultPlan`,
  `InputPlan`. (`ScriptedModel` was retired in the E3 rewiring; the
  scripted model backend now lives in `src/backend.rs`.)
- `src/trace.rs` — `RunTrace`: the eval harness's evidence, derived
  from the journal. E4: carries `secret_digests`, `prompt_bytes_used`,
  and the segment `lineage`.
- `src/error.rs` — `CrashPoint` (11 variants), `Halt`, `RuntimeError`.
- `tests/trajectories.rs` — golden trajectories a–h (SPEC §14).
- `tests/crash_matrix.rs` — one reboot per crash point.
- `tests/invariants.rs` — the durability contract.
- `tests/context_lifecycle.rs` — E4: masking, 80% rollover, lineage,
  failed-path survival, reboot after rollover, corrupt snapshot.
- `tests/always_on.rs` — E4+aon: cadence, satisfaction, least-privilege
  denial, backoff, envelope exhaustion, parking, operator control,
  memory rules, masking at the device boundary, brief transport and
  binding, the persistent heartbeat, `continue_as_new` equivalence,
  rollover under supervision, the RAM pin, and the crash matrix (every
  Waymaker boundary kind × every occurrence, with and without
  checkpoints, plus every run crash point inside a supervised run).

## Authority order

1. `~/workspace/esper/SPEC.md`
2. `~/workspace/esper/spec/trajectories/`
3. existing `esper-core` APIs (never change them; flag if one blocks you)
4. published Waymaker 0.1.0 APIs (`ReplayCursor`, not `ReplayMachine` —
   the latter does not exist in 0.1.0)

## How the engine works

One turn commits in this order, each step crash-injectable:

1. **Infer** — the engine builds the prompt with `build_prompt`
   from journal-restored inputs, takes one line from the
   `ModelBackend`, commits `ModelDecision`, consumes a turn, decodes.
   Malformed lines repair up to 3 times (E3; was 2 in E0/E1), then
   end `ModelInvalid`.
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

## E4: context lifecycle (SPEC §20)

**Masking** (SPEC §20.2): `do_observe` masks the tool outcome before
it reaches the journal, the Waymaker cursor, `last_observation`, or
the next prompt. The `MaskReport.secret_digest` is stored in the
`ToolObservation` frame and accumulated in the trace's digest stream
(one `u64` per observation, `0` for no secret). A reboot rebuilds
the real digests from the frames — the raw secret bytes are gone by
design, but the digest record survives.

**Rollover** (SPEC §20.4): `do_gather` checks the 80% trigger when
the seed carries `context_budget_bytes`. On trigger, the segment
folds: summarize frames, compact with `DEFAULT_POLICY`, encode,
read-back verify, `continue_as_new` (no-widen check), then return
`SegmentOutcome::Rollover(trace, handoff)`. The handoff carries the
verified snapshot, lineage, prompt bytes, and digest stream.

**Continuation** (SPEC §20.6): `RolloverHandoff::child_seed` derives
the child with narrowed budgets (never widened), shrunken context
budget (`saturating_sub`), and the parent lineage. The child boots on
a fresh journal with the handoff; the snapshot re-verifies on every
boot. `drive_segment` enforces the pairing: a child seed requires a
handoff, a root forbids one.

**Failed paths** (SPEC §20.5): the fold carries ruled-out
`(tool, args_digest)` pairs. `do_authorize` refuses an identical
call before dispatch — the run degrades with
`failed_path_ruled_out`, never re-proving the failure.

**Prompt metering** (SPEC §20.4): each `ModelDecision` frame carries
its prompt's byte length. The meter sums the journal on boot, so
suspend/resume across `drive_segment` calls keeps the 80% trigger
exact. A new segment (fresh journal) starts at zero.

**Segment ownership**: `drive_run` chains segments; each child gets
a fresh `Journal`. A segment never appends to its parent's frames.

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
- Backend rules (E3): the tiny backend is a distillation stand-in,
  not a trained model — never claim it learned anything;
  `UnknownPrompt` is a hard failure, never a guess. Prompt inputs
  must come from durable or rebuilt state so record and replay build
  byte-identical prompts. The `ModelBackend` trait is object-safe
  (`&mut dyn ModelBackend`) and carries a `Send` supertrait (SPEC
  §18.10 item 7 resolved; `TinyBackend` borrows only `&[u8]`).
- Prompt format (E3, SPEC §18.4): `TOOLS` + the six
  `render_signature` lines (never truncated) + compact
  `STATE turns=<n> muts=<n>` + optional
  `REPAIR <attempt>:<variant> <grammar one-liner>` + optional
  `LAST <observation>` (only this tail may truncate, ending
  `[truncated]`) + `EMIT one decision line.` The fixed part always
  fits `PROMPT_CAP` — pinned by test, so signature growth fails
  loudly.

## E4+aon: the supervisor (SPEC §21)

**Firmware-first.** `agent` is not behind `host`. A `cfg(not(host))`
guard in `lib.rs` names the supervisor's surface, so gating it out
breaks `cargo check --no-default-features`. Only `agent::host` (the
doubles) is host-only.

**Waymaker is the store.** No second journal: ledger, memory, and the
beat counter are derived by replaying the workflow into the
caller-owned `AgentState`. Everything the world says arrives as a
recorded activity answer — the workflow never reads a clock, an inbox,
or an operator queue directly.

**One page per record.** Every activity input and answer, and the
`continue_as_new` input, fits `PAGE_RECORD_MAX`. New payloads get a
`const` assertion next to their encoder.

**Masking at the boundary.** Operator text is masked by the dispatcher
(`Command::add_goal`, `Command::remember`) before the `SENSE` answer is
recorded. Harvested entries are masked by the `Harvester`.

**At least once.** A redelivered `RUN` names a run the dispatcher
already holds: `Device` re-drives that run from its own journal (replay
reaches the recorded terminal, which must match) — never a fresh one.

**Briefs.** A spawned seed carries `brief: Some(..)` and
`InferenceSettings::briefed_settings()` (1536-byte prompt). The brief
is bound into the 151-byte seed snapshot, travels in the committed
`RUN` intent, is re-validated by `Brief::from_bytes`, and is inherited
by rollover children. Plain seeds keep the 1024 cap.
