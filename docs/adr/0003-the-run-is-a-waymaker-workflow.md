# ADR 0003: The run is a Waymaker workflow

- Status: proposed, pending Mark's review
- Date: 2026-10-27

## Context

The E0–E4 run engine (`engine.rs`) was a synchronous host-only loop
with its own `Frame` journal, heap buffers, and a Waymaker
`ReplayCursor` cross-check beside it. The firmware-first rule (root
`CLAUDE.md`) named it migration debt: it could not run on the device,
and it kept a second journal next to Waymaker's. The always-on
supervisor (ADR 0002) already ran as a `no_std` `Ctx` workflow but its
`RUN` activity had nothing to drive on the board.

## Decision

1. **The run is `run::run`, an `async fn` over `waymaker_embassy::Ctx`**,
   `no_std`, no allocation, state in a caller-owned `RunState`.
2. **Waymaker's journal is the only store.** Inference, the device
   business rule, every tool attempt, the verifier's read-back and
   operator input are activities with page-bounded payloads. The
   `Frame` journal retires; `Frame` survives as a host-only view derived
   from the records for fixtures, eval and traces.
3. **Transient retries are new activities carrying the same Esper
   effect id**, so the device deduplicates redelivery.
4. **Suspension is an unserviceable `ASK`; rollover is
   `continue_as_new`** over a `RunStart` with the fold stored in
   page-sized chunks.
5. **Masking happens in the dispatcher, before the answer exists**
   (`run::device::observe`), because the answer is the durable record.

## Consequences

- All 23 golden fixtures × 2 backends, the crash matrix, the context
  lifecycle and the invariants pass against the workflow; an exhaustive
  reset sweep proves recovery at every Waymaker boundary.
- The exhausting transient attempt is now recorded (one more failed
  observation in that trace).
- A device outcome must mask into 448 B (raw outcomes ≤ 256 B); a larger
  one fails closed.
- `RunState` is 10,808 B on the host; the snapshot buffer (2,688 B) and
  two compact folds dominate.
- The emulated image runs the workflow on two ARM cores. The ESP32-S3
  and the flash-backed journal remain unexercised in CI.
