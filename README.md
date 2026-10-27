# Esper — E4+aon: Always-On Primitives

Esper is a durable agent harness: a small ReAct loop that survives
power loss. The harness runs one tool call per turn, commits its
intent to a durable journal **before** any effect is dispatched, and
recovers a legal committed prefix after every reset.

Rungs E0 (executable specification) + E1 (durable single loop)
shipped as one vertical slice; rung E2 added the typed native tool
registry; rung E3 (this tree) replaces the scripted model with a real
inference boundary. The full contract lives in
[SPEC.md](SPEC.md); this file is the field guide.

## Always on: heartbeats, goals, memory (E4+aon)

A run is durable but finite. The always-on layer is a **supervisor**
above the run — itself a Waymaker workflow, `no_std` and
allocation-free, compiled into the firmware build (SPEC §21,
[ADR 0002](docs/adr/0002-always-on-supervisor-above-the-run.md)):

- **Heartbeats** — each beat awaits a durable Waymaker deadline
  (`ctx.timer`): `Heartbeat::Boot { ticks }` (no RTC needed; a reset
  restarts the interval) or `Heartbeat::Persistent { period }` (an RTC
  deadline that survives power loss). Beats are counted by replay, so
  they never skip or repeat; a late wake yields one beat, never a
  burst. One beat does at most one unit of work. Each beat emits a
  `Pulse` to a `PulseSink`.
- **Goals** — standing objectives (`Once` or `Every(n beats)`) with a
  priority, a least-privilege capability set, and a cumulative budget
  envelope: reserve on spawn, refund on settlement, never widened.
  The status machine is data (11 legal edges). Failures back off
  exponentially; `Denied` / `Incompatible` / `StorageFault` fail the
  goal at once. A goal parked on `Ask` never blocks other goals and is
  never abandoned (overdue is a signal, not a timeout).
- **Memory** — 32 masked, provenance-tagged entries that outlive runs.
  A streaming `Harvester` takes them from committed evidence
  (successful observations; mutations only with a passing
  verification; each run's outcome); the operator adds pinned notes.
  **The model cannot write memory.** Operator text is masked before it
  becomes a durable record.
- **The brief** — each spawned run's prompt gains `GOAL <objective>`
  and up to four `MEM <label> = <value>` lines (≤ 256 bytes), bound
  into the seed snapshot (now 151 bytes). Unbriefed runs are
  byte-identical to E4.
- **Bounded history** — every `checkpoint_every` beats the state is
  stored item by item and the workflow continues as new with a 46-byte
  header; the next run restores the items and checks their digest.

Every record the supervisor writes fits one 512-byte scratch page
(`SENSE` ≤ 443 B, `RUN` ≤ 337 B, items ≤ 164 B), and its state is
caller-owned so it can live in a `static`:

```rust
// On the device: an Embassy task over the flash journal and the
// device dispatcher. `AGENT` is a `static`; the future never
// completes, it beats forever.
let mut ctx = Ctx::new(&mut journal, &mut dispatcher, &mut page);
supervise(&mut ctx, &SEED, run_input, &mut rtc_alarm, &mut AGENT, &mut telemetry).await
```

The crash matrix (`tests/always_on.rs`) resets at every Waymaker
boundary kind — intent committed, effect done with its outcome lost,
outcome committed, deadline armed — on every occurrence of a mixed
workload, with and without `continue_as_new` (174 cases), plus every
run-level crash point inside a supervised run, and demands the
crash-free state, pulses, prompts, and device history.

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
| `esper-core` | Pure types: `Decision` decoder (now validating through the protocol contracts into typed `ToolArgs`), resource budget, `ProgressDelta`, runtime monitor, terminal statuses, state machine, v2 `Capabilities`, `authorize()`; E4 context lifecycle; E4+aon `beat`, `goal`, `memory`, `brief`, `harvest`, `checkpoint` | `#![no_std]`, allocation-free |
| `esper-runtime` | The ReAct workflow over Waymaker: intent-before-dispatch commit boundaries, the E3 model backends (`backend.rs`: `ModelBackend`, `ScriptedBackend`, `TinyBackend`, prompt construction, bundle identity), fake device (GPIO + sensors + virtual clock), independent verifier, crash hooks; E4+aon always-on supervisor (`agent/`: the `supervise` workflow and its page-bounded payloads, in the firmware build; `agent/host.rs` doubles behind `host`) | `#![no_std]` engine core and supervisor without the `host` feature |
| `esper-eval` | Host-only fixture runner, trace comparison, trajectory scoring; E3 runs every fixture under both backends (record → distill → replay) | **Host only** — must never be linked into firmware |

Layering is strict and checked: `esper-eval → esper-runtime →
esper-core`, with `esper-core` and `esper-runtime` both depending on
`esper-protocol`; `esper-core`/`esper-protocol` depend only on `core`
(+ `thiserror`). Every allocation site in the host-only doubles
carries a `// HOST-ONLY` marker.

## How to run the gates

All commands run from this directory. All gates pass on this tree
(E4+aon; verified 2026-10-27 with `rustc 1.97.0`).

```bash
cargo fmt --all -- --check                              # 1. format
cargo clippy --workspace --all-targets -- -D warnings   # 2. lint (pedantic + nursery)
cargo test --workspace                                  # 3. full test suite
cargo check -p esper-runtime --no-default-features      # 4. no_std firmware core
RUSTDOCFLAGS="-D missing-docs" cargo doc --workspace --no-deps   # 5. public API docs
cargo test -p esper-runtime --test crash_matrix          # 6. crash matrix
cargo test -p esper-eval                                # 7. fixture runner
cargo xtask emulate                                     # 8. link + boot under QEMU
```

Gate 8 needs `qemu-system-arm`. It links `crates/esper-emu` (a
`cortex-m-rt` image with 512 KiB of RAM, about the ESP32-S3's SRAM)
for a Cortex-M3 and a Cortex-M4, boots both, and requires each core to
pass every stage — briefed prompt, tiny-backend inference, decode,
secret masking, item-wise checkpoint restore, `SENSE` codec, and the
run workflow itself through a reset with exactly one physical write — with the
same digest the workload computes on the host. CI
(`.github/workflows/ci.yml`) runs all eight, release-mode tests, and
bare-metal builds of the firmware crates for
`thumbv6m`/`thumbv7m`/`thumbv7em-none-eabi`. The toolchain and targets
are pinned in `rust-toolchain.toml`. QEMU is not an S3: the Xtensa
target needs the Espressif fork toolchain, which no clean runner has.

Test counts (E4+aon): 443 hand-written test functions — 195 in
`esper-core` (45 of them E4+aon: the exhaustive 8×10 goal
transition table, every terminal status × cadence, a seeded
property test of the never-widening envelope, memory masking and
eviction, the harvester, the brief bound and round trip, checkpoint
truncation, bit-flip, and item-wise restore sweeps), 163 in
`esper-runtime` (21 of them E4+aon, including the 17-test supervisor
suite and its crash matrix), 63 in `esper-eval`, 22 in
`esper-protocol` — plus one generated test per golden fixture;
`cargo test --workspace` runs 489 (E4+aon plus the SPEC §22 run port: the reset sweep, crash-point and page-bound tests, the run codecs). Zero warnings from clippy
(pedantic + nursery, denied workspace-wide). All crates target Rust
edition 2024.

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
- **E4 (this tree):** context compaction / `Compact` state /
  `continue_as_new`; masking; rollover. SPEC §20.
- **E4+aon (this tree):** always-on primitives — a firmware Waymaker
  workflow with durable heartbeat deadlines, standing goals with
  never-widening envelopes, masked cross-run memory, the GOAL/MEM
  brief, and item-wise checkpoints across `continue_as_new`. SPEC §21.
  Not yet published.
- **E5:** network tools; remote model backend and the MCP gateway;
  the radio-byte budget becomes real.
- **E6:** bounded parallel calls (the one-call-per-turn rule is
  relaxed by design, not by accident).

## Honest footnotes

Things this environment cannot verify, stated plainly:

- **E4+aon on the board.** The supervisor and the run (SPEC §22) are
  both `no_std` Waymaker workflows in the firmware build, and the run
  boots on two emulated ARM cores; the board's dispatcher that wires
  `RUN` to `run::run` over flash is not written yet.
  A boot-clock heartbeat restarts its interval after a reset; only a
  persistent-clock heartbeat survives power loss. Recall is
  recency-ordered, not relevance-ranked. One run at a time.
- **E4+aon RAM is measured on the host only.** `AgentState` is
  7,384 B and the `supervise` future 2,280 B on `x86_64` (pinned by
  test). Briefed runs use a 1536-byte prompt buffer (+512 B);
  `memory::ingest` uses a ~2.4 KiB masking scratch on the stack. None
  of these is measured on the target board yet.

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
  `cargo-tarpaulin` are not installed (on 2026-10-27 the install was
  not permitted in the sandbox). The SPEC's ≥85% gate is therefore
  unmeasured here. The suite runs 489 tests and the contract surface
  is exercised by the golden trajectories and the agent crash matrix,
  but line coverage is an open number.
- **No mechanized proofs.** The user's SPEC-PROOF step names Verus; no
  Verus toolchain exists in this workspace. Invariants are enforced by
  exhaustive and seeded property tests instead (ADR 0002).
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

## License

Licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
