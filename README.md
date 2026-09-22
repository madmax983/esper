# Esper — E3: Model Adapters

Esper is a durable agent harness: a small ReAct loop that survives
power loss. The harness runs one tool call per turn, commits its
intent to a durable journal **before** any effect is dispatched, and
recovers a legal committed prefix after every reset.

Rungs E0 (executable specification) + E1 (durable single loop)
shipped as one vertical slice; rung E2 added the typed native tool
registry; rung E3 (this tree) replaces the scripted model with a real
inference boundary. The full contract lives in
[SPEC.md](SPEC.md); this file is the field guide.

## What it is

- One ReAct workflow with a fixed state machine: `Recover → Gather →
  Infer → Repair → Authorize → Observe → Verify → Account → … →
  Finalize/Degraded/SafeStop → End`.
- The engine's only window into the model is the `ModelBackend`
  trait (`&mut dyn ModelBackend`): `infer` reads a bounded prompt and
  writes one decision line, with measured `TokenUsage` and a `BundleId`
  bound into the seed's 67-byte snapshot. Three backends ship —
  `ScriptedBackend` (host test double, canned lines in script order),
  `TinyBackend` (deterministic distilled stand-in: a caller-owned
  prompt→line table keyed by `fingerprint_prompt`, no allocation, no
  guessing — an unknown prompt is a hard `UnknownPrompt`), and
  `JevBackend` (host-only `System One` client: typed Choice/Score/Noul
  answers mapped through deterministic arg templates, recorded
  cassettes in `spec/jev-cassettes/`, live calls deferred — no API
  key; see SPEC §19). Neither the scripted nor the tiny backend is a
  trained model; see the honest footnotes and
  [ADR 0001](docs/adr/0001-e3-tiny-backend-distillation-standin.md).
- No real device yet. A fake device stands in for `esper-device`:
  8 GPIO pins, 4 fixed sensor channels, and a virtual millisecond
  clock. Six tools, from one static registry (`esper-protocol`'s
  `CATALOG` is the single contract source — compact model signatures,
  argument validators, and JSON Schemas are all generated from it):
  `gpio_pin_read`, idempotent `gpio_pin_write`, `sensor_sample_read`,
  `timer_uptime_read`, idempotent `timer_delay_wait` (advances the
  virtual clock, consumes one mutation), `device_status_report`.
  No network capability exists; network is rung E5.
- No verifier trusts the tool. `gpio_pin_write` gets an independent
  read-back by a separate code path; a mutation is never reported
  complete without a committed passing verification.
- Budgets (turns, tokens, mutations, error counters) are part of run
  identity: committed in the `RunSeed`, never widened after reboot.

## The four crates

| Crate | Role | Runtime shape |
|---|---|---|
| `esper-protocol` | The single contract source: the six-tool `CATALOG`, the allocation-free argument validator, the compact signature renderer, the JSON Schema renderer | `#![no_std]`, allocation-free, `core` + `thiserror` only |
| `esper-core` | Pure types: `Decision` decoder (now validating through the protocol contracts into typed `ToolArgs`), resource budget, `ProgressDelta`, runtime monitor, terminal statuses, state machine, v2 `Capabilities`, `authorize()` | `#![no_std]`, allocation-free |
| `esper-runtime` | The ReAct workflow over Waymaker: intent-before-dispatch commit boundaries, the E3 model backends (`backend.rs`: `ModelBackend`, `ScriptedBackend`, `TinyBackend`, prompt construction, bundle identity), fake device (GPIO + sensors + virtual clock), independent verifier, crash hooks | `#![no_std]` engine core without the `host` feature |
| `esper-eval` | Host-only fixture runner, trace comparison, trajectory scoring; E3 runs every fixture under both backends (record → distill → replay) | **Host only** — must never be linked into firmware |

Layering is strict and checked: `esper-eval → esper-runtime →
esper-core`, with `esper-core` and `esper-runtime` both depending on
`esper-protocol`; `esper-core`/`esper-protocol` depend only on `core`
(+ `thiserror`). Every allocation site in the host-only doubles
carries a `// HOST-ONLY` marker.

## How to run the gates

All commands run from this directory. All gates pass on this tree
(E3 shipped; verified 2026-09-22).

```bash
cargo fmt --all -- --check                              # 1. format
cargo clippy --workspace --all-targets -- -D warnings   # 2. lint (pedantic + nursery)
cargo test --workspace                                  # 3. full test suite
cargo check -p esper-runtime --no-default-features      # 4. no_std firmware core
RUSTDOCFLAGS="-D missing-docs" cargo doc --workspace --no-deps   # 5. public API docs
cargo test -p esper-runtime --test crash_matrix          # 6. crash matrix
cargo test -p esper-eval                                # 7. fixture runner
```

Test counts (E3+jev): 315 `#[test]` functions total — 120 in
`esper-core`, 109 in `esper-runtime` (83 baseline + 22 Jev unit +
4 Jev end-to-end, plus the 14 backend unit tests, the crash matrix,
and the resource-measurement tests), 64 in `esper-eval` (43 unit +
2 adversarial + 19 trajectory fixtures, each fixture executed under
both byte-exact backends), 22 in `esper-protocol`. Zero warnings
from clippy (pedantic + nursery, denied workspace-wide). All crates
target Rust edition 2024.

## The golden trajectories

The fixtures in `spec/trajectories/` are the red tests. Each one is a
JSON script: a `RunSeed`, a model output script, a fake-device fault
plan, planned crash injections, and the exact expected committed
trace. They become `trajectory_*` tests:

| Fixture | Proves |
|---|---|
| a — success-read-write-verify-finish | read → idempotent write → independent verify → `Completed`; a crash before tool dispatch redelivers with the same `EffectId` |
| b — invalid-output-repair-then-modelinvalid | malformed lines → two bounded repair turns → clean `ModelInvalid` termination; no tool ever dispatched |
| c — denied-pin-fails-before-dispatch | write to a pin outside the capability set → terminal `Denied`; the device is never touched |
| d — transient-read-failure-retry-success | transient read failure → retry with the same `EffectId` → success; both attempts committed |
| e — repeated-identical-failure-then-stuck | deterministic verification failure three times → the loop monitor fires → terminal `Stuck` |
| f — budget-exhaustion-durable-terminal | `model_turns` exhausted mid-run → durable `BudgetExhausted` terminal; the prefix is preserved |
| g — reset-after-write-before-outcome | reset after the physical write but before `ToolObservation` commits → recovery redelivers with the original intent, dedups on `EffectId`, verifies, completes |
| h — ask-suspend-resume | `Ask` suspends durably, survives a reset while suspended, resumes with typed input, completes |
| i — sensor-timer-status-happy | sensor read + uptime read + status report on a fully-capable seed; exact payload bytes |
| j — delay-crash-no-double-advance | `timer_delay_wait` then a crash before observation commits → redelivery with the same effect ID does **not** advance the virtual clock twice |
| k — sensor-denied | sampling a sensor outside the capability set → terminal `Denied` before dispatch |
| l — timer-not-permitted | timer tools on a seed without `allow_timer` → `Denied` before dispatch |
| m — status-not-permitted | status tool on a seed without `allow_status` → `Denied` before dispatch |
| p — invalid-args-suite | data-driven valid/invalid argument cases per tool, generated from the catalog contracts |
| q — status-full-exact | full status report byte-pinned, including `uptime_ms` saturation at `u32::MAX` |
| r — unknown-tool-adversarial | unknown tool id/name → rejected; **zero** tool requests dispatched |
| s — write-escalation-adversarial | model attempts to escalate a read into a write → denied; **zero** tool requests dispatched |
| t — trailing-garbage-adversarial | trailing garbage after JSON → malformed → bounded repair → clean finish; the decoder never accepts a prefix |
| u — empty-and-nul-adversarial | empty line and NUL-containing line → malformed → bounded repair → clean finish; no free-form bytes escape |

All 19 pass under **both** byte-exact model backends — every fixture
runs the scripted backend and the tiny distilled backend with the full
assertion battery, and the token totals must agree. The Jev backend
cannot emit the adversarial byte streams, so it mirrors the key
scenarios in typed form instead (`tests/jev.rs`: happy path, denied
write, Noul ask gate, crash before commit). A new production incident
becomes a new fixture — that is the rule for later rungs.

## The crash matrix

A reset may be injected at 11 points (§11.1 of the spec), and the
recovery contract is the same at every one:

1. No effect is dispatched without durable intent.
2. Committed outcomes replay byte-identically.
3. The first unresolved effect keeps its original identity.
4. Budgets never increase after reboot; permissions never widen;
   version bindings never change.
5. A mutation is never reported complete without its committed
   passing verifier.
6. The terminal result is emitted once logically, even if its
   delivery is retried.
7. `AwaitInput` suspension survives reset.

The points, in run order: after `ModelRequest` commit ·
after inference, before `ModelDecision` commit · after `ModelDecision`
commit, before `Authorize` · after `ToolRequest` commit, before
dispatch · after the physical effect, before `ToolObservation` commit ·
after `ToolObservation` commit, before verification · after read-back,
before `VerificationResult` commit · after `VerificationResult` commit,
before next `Gather` · while suspended in `AwaitInput` · after the
terminal decision, before `TerminalResult` commit · after
`TerminalResult` commit (redelivery path).

12/12 green: all 11 points plus a fixture-spelling check.

## HOST-ONLY vs firmware-clean

The slice runs host-only. These parts are host-side, E0/E1-only, and
must never enter a firmware image:

- scripted model backend (script storage, output staging);
- fake GPIO device (pin table, fault plan);
- fault injector and crash harness;
- fixture runner (JSON parsing, trace comparison);
- trajectory scoring and eval reporting.

Enforcement: `esper-core` and `esper-runtime` are `#![no_std]` with no
`extern crate alloc`; every crate has `#![deny(unsafe_code)]`;
`cargo check -p esper-runtime --no-default-features` compiles the
allocation-free engine core. No `unwrap`/`expect` in non-test code.

E3 addition: `TinyBackend` is allocation-free and firmware-portable —
the struct is 24 B on 32-bit Xtensa and the distill table is
caller-owned memory (flash/rodata or RAM, the caller's choice). Only
the `ScriptedBackend` and the recording/distilling tooling stay
host-only.

## What comes next

- **E2 (this tree):** typed native tools — static six-tool registry,
  one contract source, permission classes, independent read-back
  verification, idempotency, adversarial fixtures. Shipped locally;
  not published.
- **E3 (this tree):** model adapters — the `ModelBackend` trait, the
  scripted host backend, the tiny distilled stand-in, compact
  signature prompts, bundle identity in the run record, and the
  record → distill → replay both-backend exit criterion. Shipped
  locally; not published.
- **E3+jev (this tree):** the Jev host-backend adapter — TypeSafe AI's
  `System One` behind `ModelBackend`: typed Choice/Score/Noul answers,
  deterministic arg templates (the cascade), the Noul ask gate,
  per-turn probability receipts, request-hash mock cassettes, live
  transport deferred (no API key). SPEC §19. Shipped locally; not
  published.
- **E4:** context compaction / `Compact` state / `continue_as_new`;
  explicit subgoals and `SubgoalClosed`.
- **E5:** network tools; remote model backend and the MCP gateway;
  the radio-byte budget becomes real.
- **E6:** bounded parallel calls (the one-call-per-turn rule is
  relaxed by design, not by accident).

## Honest footnotes

Things this environment cannot verify, stated plainly:

- **No hardware.** Everything physical is the fake device: pin
  tables, sensor channels, the virtual clock, fault plans, read-back.
  `gpio_pin_write` and `timer_delay_wait` dedup on effect identity is
  proven against the test double, not against silicon. Real GPIO,
  sensor, and timer behavior is the firmware team's proving ground.
- **No trained model.** Both E3 backends are stand-ins. The scripted
  backend is a deterministic test double; the tiny backend replays
  lines distilled from the scripted teacher's own runs — its
  fingerprint-indexed table cannot generalize, and an unknown prompt
  is a hard failure, never a guess. The fingerprint pipeline, the
  table lookup, the integrity check, and the bundle binding are the
  exact shapes a trained-weights backend will keep (see ADR 0001).
  Whether the line grammar survives contact with a real trained model
  is an open question (§16).
- **No cross-reset clock.** `elapsed_ms` uses the host monotonic
  clock, and E2's virtual millisecond clock is fake-device state,
  journaled like everything else. Esper makes no claim about elapsed
  time across power loss (Waymaker durable-time rule).
- **Coverage tooling unavailable.** `cargo-llvm-cov` and
  `cargo-tarpaulin` are not installed; the install did not finish in
  the sandbox. The SPEC's ≥85% gate is therefore unmeasured here.
  The suite is 289 tests and the contract surface is fully exercised
  by the 19 trajectories, but line coverage is an open number.
- **Size gate is measure-only.** Target-board measurement (Espressif
  `esp` toolchain, `rustc 1.97.0-nightly (8ea53bcd7 2026-07-08)`,
  `xtensa-esp32s3-none-elf`, `-Z build-std=core,compiler_builtins`,
  release; `.text` + `.rodata` of the crate rlibs): `esper-core`
  12,626 B, `esper-protocol` 9,942 B, `esper-runtime` without the
  `host` feature 4,598 B, `waymaker-core` 3,693 B. Note: the tree's
  stock `thiserror = "2"` dependency pulls `std` on this target; the
  measurement used a temporary `default-features = false` override
  (reverted after measuring), which the firmware port must carry for
  real. Host measurement: tiny-backend `infer` ≈ 0.6 µs mean
  (1,000 iterations, `x86_64` host); `size_of::<TinyBackend>()` is
  32 B on the host (24 B on 32-bit Xtensa by construction);
  `TINY_PARAMS_BYTES` = 2156 B declared. Full method and the ESP32-S3
  projection table live in SPEC §18.8; the measurement harness is
  `crates/esper-runtime/tests/resource_measure.rs`.
- **Energy is unmeasured.** No energy proxy exists for the target
  board; the SPEC §16 open question stays open. Nothing in E3 stands
  in for it.
- **Spec conformance note.** SPEC §2/§10 lists `waymaker-flash` as a
  direct dependency of `esper-runtime`; in the tree it arrives
  transitively via `waymaker-embassy` (0.1.0, crates.io). Behavior is
  identical for the slice; the direct declaration is an E2 detail.
- **`NeedsInput`** is defined but never emitted in the slice (no path
  produces it yet); **`SubgoalClosed`** is defined but never emitted
  (E4). Both are reserved variants, not missing features.
