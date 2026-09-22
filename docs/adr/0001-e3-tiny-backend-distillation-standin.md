# ADR 0001: The E3 tiny backend distills the scripted teacher instead of shipping trained weights

- Status: accepted by the E3 implementation crews, pending Mark's review
- Date: 2026-09-22
- Deciders: the E3 implementation crews (not Mark — he commissioned the
  rung but did not take this decision)

> Correction note (2026-09-22): prompt construction follows the E3
> TOOLS/signature requirement — the prompt opens `TOOLS`, renders all
> six catalog tool signatures via `esper_protocol::render_signature`,
> then a compact STATE line, an optional REPAIR line (attempt, variant,
> grammar one-liner from `esper_core::decision::decode_line`), an
> optional LAST observation line, and closes `EMIT one decision line.`
> Only the LAST observation tail may truncate.

## Context

Rung E3 replaces the scripted model with a real inference boundary
(`ModelBackend`) and ships two backends: `ScriptedBackend` (the host
test double) and `TinyBackend` (the on-device stand-in). The long-term
goal is a trained tiny model on the ESP32-S3 (see the llm-train-infra
context: a ~2.77M-parameter int8 model at ~3.4 MB — **unverified**,
cited here as context only, not as a measured number). No trained
weights exist today, and no training run has happened.

The E3 exit criterion (SPEC §18.9) needs a second backend that replays
every golden trajectory byte-identical, so the engine, journal
binding, and resource gates can be proven against two backends *now*,
without waiting for training.

## Decision

The tiny backend is a **distillation stand-in**: it answers each
inference prompt from a caller-owned `DistillEntry` table keyed by
`fingerprint_prompt(prompt)` — a table of `(prompt_bytes,
emitted_line)` pairs recorded from the scripted backend's own runs.
The fingerprint pipeline (`fingerprint_prompt`), the table lookup,
the per-entry integrity check (`line_hash`), the output-cap
discipline, and the bundle binding are the exact shapes the
trained-weights backend will keep; only the lookup *source* is
distilled data rather than computed weights.

Why this shape and not something simpler:

- **No trained weights exist.** Shipping pseudo-weights would lie
  about the backend's capability. The table makes the stand-in
  explicit: what is real (fingerprint, lookup, integrity check,
  bundle binding) and what is not (any generalization).
- **Determinism across crash recovery.** The lookup is pure — no
  cursor, no hidden state — so a crash between inference and commit
  re-issues the same prompt and gets the same line. `unemit` is a
  no-op by construction, and the crash-matrix proofs keep holding.
- **Byte-identical replay.** SPEC §18.9 demands the same terminal
  status, decision sequence, and journal bytes under both backends.
  A table replay gives this by construction; a pseudo-random "model"
  could not.
- **Zero allocation, target-portable.** The table is caller-owned
  memory (flash/rodata or RAM, the caller's choice); the backend
  itself is 24 bytes of state.

## What the bundle hash binds

`BundleId` for the tiny backend is FNV-1a 64 over the domain tag
`esper-tiny-v1`, then each entry's `(prompt_fp, line_hash)` in table
order (implementation: `tiny_bundle` in `backend.rs`). The seed's
67-byte snapshot binds `model_bundle` and `InferenceSettings`, so a
reboot can never swap the model, widen the prompt/output caps, or
replay under a different table. Two tables that answer the same
prompts with the same lines share one bundle id — the identity is
about *behavior*, not about which bytes were distilled when.

Note: the implemented hash covers the table, not a 2192-byte
parameter block. SPEC §18.3 as drafted described a `TinyParams`
parameter block and a pseudo-random forward pass; the shipped
implementation (see the E3 crews' note in §18.10) keys on
`fnv1a64(prompt)` directly and keeps `TINY_PARAMS_BYTES = 2156` as a
declared parameter budget the reference table distills to. The
measurement crew's projection (SPEC §18.8) treats the table bytes as
the parameter bytes.

## What changes when a trained model lands

1. The `DistillEntry` table is replaced by real weights behind the
   same `ModelBackend` trait: `infer` keeps its signature, its
   `OutputTooLong` discipline, and its `TokenUsage` accounting.
2. `fingerprint_prompt` may be replaced by (or extended with) the
   model's real tokenizer + forward pass — but the *contract* stays:
   same prompt bytes in, byte-identical output on replay.
3. `UnknownPrompt` disappears as a concept: the trained backend
   answers every prompt (possibly badly — that is the trainer's
   problem, not the harness's). The integrity check moves from
   "table entry" to "weight-bundle hash".
4. The bundle hash binds the weight bytes instead of the table. The
   seed snapshot format does not change; only the hashed bytes do.
5. The distillation table and its record/replay tooling stay as the
   regression harness: the trained backend must replay the golden
   trajectories before it ships, with the table as the oracle.

## Honest limits

- **The table cannot generalize.** A prompt not in the table is
  `UnknownPrompt` — a hard failure, never a guess. The backend is
  a lookup, not a model.
- **A real head replaces the table.** The fingerprint-indexed lookup
  is the stand-in for a trained head; the adapter shape
  (`ModelBackend`, `build_prompt`, `InferenceSettings`, bundle
  binding) is what must survive training.
- **Token counts are measured, not metered.** `TokenUsage` reports
  prompt bytes read and output bytes written per inference; the
  engine records them in the journal but never spends budget against
  them (SPEC §7 and §18).
- **Energy is unmeasured.** No energy proxy exists for the target
  board; the SPEC §16/§14 open question on energy measurement stays
  open. Nothing in E3 stands in for it.
