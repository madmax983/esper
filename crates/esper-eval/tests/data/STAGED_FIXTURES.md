# Staged E4 fixtures (eval crew)

These four fixtures are **staged, not golden**: they are complete fixture
JSONs in the `spec/trajectories/` schema, but they live in
`crates/esper-eval/tests/data/` so `build.rs` does not generate fixture
tests for them. Each has a top-level `staging_note` naming exactly what
is pending. The runtime crew's rollover commit has to land first; then
the finalizing crew moves each file to `spec/trajectories/` (letters
`v`, `w`, `x`, `y` — `t` and `u` are already taken), runs it through
the runner, verifies every trace event against the SPEC contract, and
only then sets the final expectations.

| File | Future letter | Covers |
|---|---|---|
| `staged-v-rollover-mid-task.json` | `v` | rollover mid-task on a small context budget → continuation trace, terminal success |
| `staged-w-rollover-reboot-ask.json` | `w` | rollover + reboot with a pending Ask → open obligation survives via the compact state |
| `staged-x-negative-info.json` | `x` | failed path recorded before rollover → identical re-attempt after reboot must not redispatch (forbidden: `a second ToolRequest for the retried write`) |
| `staged-y-masking.json` | `y` | secret-bearing ask → trace shows only opaque markers; exact-trace equality is the raw-value ban |

Measurement-only trajectories (not fixtures at all) also live here:

| File | Used by |
|---|---|
| `m-long-success.json` | `tests/e4_measure.rs` — 38-frame successful run (full 1163 B, masked tail 194 B, compact 269 B) |
| `m-secret-bearing.json` | `tests/e4_measure.rs` — 9-frame ask/write/finish with a synthetic secret + email (full 407 B, masked tail 199 B, compact 165 B) |

Byte numbers are the §20.7 results-table inputs (SPEC §20.7). The
`failure-heavy` measurement trajectory is the existing golden fixture
`spec/trajectories/e-repeated-identical-failure-then-stuck.json`
(14 frames; full 513 B, masked tail 223 B, compact 223 B).
