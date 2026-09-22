# esper-core

Pure types, decision decoder, resource budgets, the deterministic runtime
monitor, and the E4 context lifecycle (masking, compaction, snapshots,
lineage) for Esper. Authority: `SPEC.md` at the workspace root
(§3–§8, §10, §17, §20).

## Non-negotiables

- `#![no_std]`, no `alloc`, `#![deny(unsafe_code)]`, `#![deny(missing_docs)]`.
  Every public item needs docs; every `pub` field is deliberate (SPEC shows
  them public — keep them public).
- Firmware-clean: no I/O, no clocks, no logging, no runtime or Waymaker
  dependency. Host-side doubles (scripted model, fake device, fault
  injector, fixture runner) live in `esper-runtime` / `esper-eval`, never
  here.
- Dependencies are `thiserror` (no_std-compatible) and `esper-protocol`
  (the single E2 contract source). A CI layering gate fails the build if
  `alloc` appears in this crate's dependency closure.
- Engineering law (per `~/memory/coding-philosophy.md`): SPEC-PROOF →
  RED → GREEN → REFACTOR. Strong newtypes over primitives. `thiserror`
  for the error vocabulary. No `unwrap`/`expect` in non-test source —
  tests may use `expect` with a message. `cargo fmt --check` clean,
  `cargo clippy --all-targets` with workspace lints (pedantic + nursery
  deny, `-D warnings`) clean.

## Module map

| Module | SPEC | What it owns |
|---|---|---|
| `ids` | §3.1 | Strong newtypes: `RunId`, `EffectId`, `EffectSeq`, `ToolId`, `Pin` (0..=7, `PIN_COUNT = 8`), `Digest` (FNV-1a over borrowed bytes) |
| `state` | §3.2 | `State` (11 states), `Event` (one per §3.2 disjunct), `transition` / `is_legal`, `committed_records` (every legal edge's journal writes), `TerminalStatus` |
| `error` | §6 | `ErrorCode` (stable numeric codes 0–10), `Error` (decoder rejection vocabulary, all class `Malformed`), `repair_hint()` for repair turns |
| `json` | §4.3 | Strict allocation-free JSON validator: depth ≤ 3, ≤ 8 object keys, no duplicate keys, no trailing bytes, borrowed slices only |
| `decision` | §4.2–4.3 | `Decision::{Call,Ask,Finish}` and the `VERB <json>` line decoder. `ToolCall { tool, args, args_digest }` etc. have **public** fields per SPEC |
| `registry` | §5 | Static 2-tool catalog (`gpio_pin_read` id 1, `gpio_pin_write` id 2), `Capabilities` pin sets, `authorize_capability` (fail-closed static half of `Authorize`) |
| `budget` | §7 | `ResourceBudget`: consume/check per unit, field-wise `meet`, `check_no_widen` against run identity |
| `monitor` | §8.3 | `Monitor`: bounded 5-event ring; budget guard → 5×`NoProgress` → 3× identical failure → 3-of-5 error majority |
| `records` | §10 | `RecordKind` (stable codes + max lengths), `TerminalResult`, `VerificationResult` |
| `mask` | §20.1 | `mask_bytes` / `mask_report`: secret + PII shape scanners, opaque `[redacted:*#N]` markers, `mask_bound` proof, `fnv1a64` (delegates to `Digest::of_bytes`) |
| `compact` | §20.2 | `CompactState`: durable compact state; `compact` folds `FrameSummary` views; failed paths and obligations are never dropped (fail closed); `should_compact` (80% default) |
| `snapshot` | §20.3 | `encode` / `decode` / `verify`: canonical LE layout, version byte, FNV-1a-64 integrity; `encoded_len` |
| `lineage` | §20.4 | `Lineage`, `continue_as_new`: parent link, remaining budgets carried never widened (`BudgetWidened` on widen) |

## Key invariants (see `docs/invariants.md` for the full list)
- `End` has no outgoing transitions; only `Finalize`, `Degraded`, `SafeStop` reach it.
- `Infer` cannot reach `Observe`; `AwaitInput` exits only via committed input to `Account`.
- Budget `meet` is field-wise monotonic and cannot widen identity (`check_no_widen`).
- `Pin` is always `0..=7`; monitor state is bounded (5 records, counters).
- Verification failure never auto-compensates.
- `args` is *validated borrowed JSON bytes*, not canonicalized JSON — the
  digest is FNV-1a over the bytes as emitted.
- E4 (§20): masking runs before durability, never after — the journal
  keeps markers plus digests, never raw secrets. Failed paths and open
  obligations are never dropped by compaction (fail closed with
  `CompactStateFull`); facts/decisions drop oldest-first. Snapshot
  version is checked before the checksum; unknown versions refuse
  loudly. `continue_as_new` never widens budgets.

## The two JSON bounds (read before touching)

`MAX_ARGS_BYTES = 256` is §4.3's literal bound and applies **only** to
`CALL` argument blocks. `ASK`/`FINISH` envelopes must contain a bounded
256-byte *field* (`MAX_PROMPT_BYTES` / `MAX_SUMMARY_BYTES`), so they get
their own envelope bound, `MAX_JSON_BYTES = 384` (largest valid encoding is
294 bytes), with the `JsonTooLong` rejection. Do not merge these — the
split is what keeps the SPEC-literal argument bound honest. Overlong
envelopes are still class `Malformed`.

## State machine as data

`transition(from, event) -> Result<State, Error>` encodes exactly the 25
legal §3.2 edges; anything else is `Err`. `is_legal(from, to)` answers
reachability. `committed_records(from, event)` names the journal records
each legal edge commits (illegal pairs commit nothing — the runtime must
not be there). §3.2 conditions that need runtime data (budget checks,
repair counts, transient attempt counts) are the caller's job: derive
them from committed outcomes, then pick the matching `Event`.

## Monitor rule order (§8.3)

`observe(record, budget)` checks, in order: (1) hard budget limit reached
→ `BudgetExhausted`; (2) five consecutive `NoProgress` deltas → `Stuck`;
(3) three consecutive identical `(tool, args_digest, error)` failures →
`Stuck`; (4) >half of the last five events are errors → `Stuck`. The
no-progress streak counts failures with `NoProgress` too — any progress
variant except `NoProgress` resets it. Rule (4) needs a full window of
five events before it can fire.

## Conventions

- `const fn` wherever the body allows it (clippy `missing_const_for_fn`
  is deny-by-workspace-lints).
- Newtypes carry their own validation (`Pin::new`, `ToolId::new`); keep
  range checks at construction, not at use.
- Decoder rejections use the dedicated `Error` variant with a
  `repair_hint()` — never a bare string.
- Keep the `committed_records` table exhaustive and auditable: the
  `#[allow(clippy::match_same_arms)]` there is intentional (the no-commit
  edges share a body with the illegal-pair fallback by design).

## Gates

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd ~/workspace/esper
cargo test -p esper-core
cargo fmt -p esper-core -- --check
cargo clippy -p esper-core --all-targets -- -D warnings
```

Do not publish this crate (Mark's call). Do not touch
`~/workspace/your_files/`.
