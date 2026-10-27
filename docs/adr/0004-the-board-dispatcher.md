# ADR 0004: The board dispatcher is product code over two traits

- Status: proposed, pending Mark's review
- Date: 2026-10-27

## Context

The supervisor (ADR 0002) and the run (ADR 0003) are `no_std` Waymaker
workflows, but nothing on the device connected them: the supervisor's
`RUN` activity was served only by a host double that re-ran segments
through `drive_segment` and harvested by re-reading every segment's
trace afterwards. On the device that is impossible — `continue_as_new`
retires a segment's records — and the real flash-backed `Journal`
(Waymaker's façade bridge over `waymaker-drive`) is not published to
crates.io, so Esper cannot depend on it.

## Decision

1. **`agent::device::Device` is the board dispatcher**, product code,
   generic over a `RunStore` (one Waymaker `Journal` per run) and
   `DevicePorts` (the run's `RunPorts` plus sensing and the item
   store). The board crate supplies the flash-backed store.
2. **`RUN` drives `run::run` as a nested workflow** over the run's own
   journal, rebuilding the run's future on every poll (Waymaker futures
   are disposable), following `continue_as_new`, answering `Settled`,
   `Parked`, or nothing (`Pending`) when the run halted.
3. **One run dispatcher.** `run::device::RunDispatcher` owns decoding,
   re-validation and masking; ports do only the physical work. The
   host double and the emulated image are thin `RunPorts`.
4. **Harvest is the run's.** The workflow feeds a `Harvester` and
   carries it across rollover as page-sized snapshot items
   (`Harvester::seal` / `kept` / `resume` in `esper-core`).
5. **Run ids carry their goal** (`agent::run_id` / `goal_of`), so
   `HARVEST` needs only the run's journal.

## Consequences

- The always-on suite, unchanged in substance, now exercises the
  product dispatcher; every run crash point is proven to fire inside a
  supervised run.
- `RunState` grows to 11,920 B (the harvester).
- The emulated image runs supervisor + board dispatcher + run on two
  ARM cores through a reset.
- The flash binding and the reclaim policy for settled runs' journals
  remain the board crate's.
