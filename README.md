# Esper — E2: Typed Native Tools

Esper is a durable agent harness: a small ReAct loop that survives
power loss. The harness runs one tool call per turn, commits its
intent to a durable journal **before** any effect is dispatched, and
recovers a legal committed prefix after every reset.

Rungs E0 (executable specification) + E1 (durable single loop)
shipped as one vertical slice; rung E2 (this tree) adds the typed
native tool registry on top. The full contract lives in
[SPEC.md](SPEC.md); this file is the field guide.

## What it is

- One ReAct workflow with a fixed state machine: `Recover → Gather →
  Infer → Repair → Authorize → Observe → Verify → Account → … →
  Finalize/Degraded/SafeStop → End`.
- No real model yet. A scripted backend emits byte-identical output
  per turn, including after reboot replay — the harness is testable
  without inference hardware.
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

## The three crates

| Crate | Role | Runtime shape |
|---|---|---|
| `esper-protocol` | **New in E2.** The single contract source: the six-tool `CATALOG`, the allocation-free argument validator, the compact signature renderer, the JSON Schema renderer | `#![no_std]`, allocation-free, `core` + `thiserror` only |
| `esper-core` | Pure types: `Decision` decoder (now validating through the protocol contracts into typed `ToolArgs`), resource budget, `ProgressDelta`, runtime monitor, terminal statuses, state machine, v2 `Capabilities`, `authorize()` | `#![no_std]`, allocation-free |
| `esper-runtime` | The ReAct workflow over Waymaker: intent-before-dispatch commit boundaries, scripted model, fake device (GPIO + sensors + virtual clock), independent verifier, crash hooks | `#![no_std]` engine core without the `host` feature |
| `esper-eval` | Host-only fixture runner, trace comparison, trajectory scoring | **Host only** — must never be linked into firmware |

Layering is strict and checked: `esper-eval → esper-runtime →
esper-core`, with `esper-core` and `esper-runtime` both depending on
`esper-protocol`; `esper-core`/`esper-protocol` depend only on `core`
(+ `thiserror`). Every allocation site in the host-only doubles
carries a `// HOST-ONLY` marker.

## How to run the gates

All commands run from this directory. They all pass.

```bash
cargo fmt --all -- --check                              # 1. format
cargo clippy --workspace --all-targets -- -D warnings   # 2. lint (pedantic + nursery)
cargo test --workspace                                  # 3. full test suite
cargo check -p esper-runtime --no-default-features      # 4. no_std firmware core
RUSTDOCFLAGS="-D missing-docs" cargo doc --workspace --no-deps   # 5. public API docs
cargo test -p esper-runtime --test crash_matrix          # 6. crash matrix
cargo test -p esper-eval                                # 7. fixture runner
```

Test counts: 266 total — 120 in `esper-core`, 67 in
`esper-runtime`, 57 in `esper-eval`, 22 in `esper-protocol`. Zero
warnings from clippy (pedantic + nursery, denied workspace-wide).
All crates target Rust edition 2024.

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

All 17 pass. A new production incident becomes a new fixture — that is
the rule for later rungs.

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

## What comes next

- **E2 (this tree):** typed native tools — static six-tool registry,
  one contract source, permission classes, independent read-back
  verification, idempotency, adversarial fixtures. Shipped locally;
  not published.
- **E3:** the local tiny-model backend; hard RAM/flash caps once the
  target board is fixed (the slice only measures: see honest
  footnotes).
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
- **No real model.** The scripted backend is a deterministic stand-in.
  The line-grammar decoder is proven against scripts, not against a
  language model's real output. Whether the grammar survives contact
  with the E3 tiny model is an open question (§16).
- **No cross-reset clock.** `elapsed_ms` uses the host monotonic
  clock, and E2's virtual millisecond clock is fake-device state,
  journaled like everything else. Esper makes no claim about elapsed
  time across power loss (Waymaker durable-time rule).
- **Coverage tooling unavailable.** `cargo-llvm-cov` and
  `cargo-tarpaulin` are not installed; the install did not finish in
  the sandbox. The SPEC's ≥85% gate is therefore unmeasured here.
  The suite is 266 tests and the contract surface is fully exercised
  by the 17 trajectories, but line coverage is an open number.
- **Size gate is measure-only.** Release `no_std` builds (x86_64 host
  target): `esper-core` ≈ 25.7 KiB code / 504 B data / 0 BSS;
  `esper-runtime` ≈ 4.9 KiB code / 400 B data / 0 BSS. Hard caps are
  set at E3 when the board is fixed; these numbers are host-target
  rlibs (with metadata), not firmware image sizes.
- **Spec conformance note.** SPEC §2/§10 lists `waymaker-flash` as a
  direct dependency of `esper-runtime`; in the tree it arrives
  transitively via `waymaker-embassy` (0.1.0, crates.io). Behavior is
  identical for the slice; the direct declaration is an E2 detail.
- **`NeedsInput`** is defined but never emitted in the slice (no path
  produces it yet); **`SubgoalClosed`** is defined but never emitted
  (E4). Both are reserved variants, not missing features.
