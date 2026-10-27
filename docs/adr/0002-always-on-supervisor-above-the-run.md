# ADR 0002: Always-on is a firmware Waymaker workflow above the run

- Status: proposed, pending Mark's review
- Date: 2026-10-27
- Deciders: the E4+aon implementation crew

## Context

Esper's run (E0–E4) is durable but finite: it starts from a seed,
survives power loss, and ends with exactly one terminal result. An
always-on agent needs three things a run cannot give it:

1. something that **wakes it up** with no human in the loop
   (heartbeats);
2. something that **says what to wake up for** and persists across
   runs (goals);
3. something that **carries what it learned** into the next run
   (memory).

The forces:

- **Esper ships on the device.** It is a firmware harness (ESP32-S3);
  anything that exists only behind the `host` feature does not exist.
- **Waymaker is the durability layer.** It already provides durable
  activities, durable deadlines (`ctx.timer`, boot or persistent
  clock), and `continue_as_new`, as `no_std` futures an Embassy
  executor polls.
- **Every record fits a scratch page.** Waymaker states device RAM
  with a 512-byte scratch page.
- The run's guarantees (§11.3) are the product. Nothing here may
  weaken them or change a golden fixture.
- Budgets are run identity and never widen (ADR 10). An agent that
  spawns runs forever needs the same property one level up.
- Model output is untrusted prose (§8.1). A model that writes its own
  memory can launder hallucinations into "facts" the next run trusts.

## The rejected first design

The first cut of this rung built the supervisor as a **host-only**
layer: a heap-backed `AgentStore` journal, a hand-rolled replay, a
Tokio interval as the heartbeat, a 6.5 KB checkpoint blob, and
1 KB run reports. Every test passed and the firmware build compiled —
because the supervisor was excluded from it. It was rejected in
review: it duplicated what Waymaker already does, could not run on the
board, and its records could never stage through a 512-byte page. The
lesson is recorded in the root `CLAUDE.md` (firmware-first), and the
firmware build now carries a guard that fails if the supervisor is
ever gated behind `host` again.

## Decision

1. **The supervisor is a Waymaker workflow.** `agent::supervise` is an
   `async fn` over `waymaker_embassy::Ctx`: `no_std`, no allocation,
   no async runtime, compiled into the firmware build. Waymaker's
   journal is its only store; its state is derived by replay into a
   caller-owned `AgentState` (a `static` on the device).
2. **Heartbeats are durable deadlines, counted.** Each beat awaits
   `ctx.timer`; the clock (`AfterBoot` or `AtPersistentTime`) is a
   declared part of the agent's identity, never guessed. The beat
   counter is derived by replay, so it never skips or repeats. The
   workflow never reads a clock: the persistent reading arrives inside
   a recorded activity answer.
3. **Everything the world says is an activity.** Operator commands and
   input readiness arrive in a recorded `SENSE` answer; goal runs are
   `RUN` activities whose committed input is the full spawn seed and
   brief; harvested memory arrives one `HARVEST` record at a time.
   Replay sees exactly what the live boot saw.
4. **Every record fits one page.** Each payload has a compile-time
   maximum asserted `≤ 480` bytes. Large state is split into items:
   the checkpoint is a 37-byte header plus one item per goal and per
   memory entry, stored with `STORE`, carried across `continue_as_new`
   by the header alone, and restored item by item into the caller's
   state with a digest check.
5. **One unit of work per beat**, as one call per turn (ADR 2).
6. **Goals carry a cumulative envelope, reserve-on-intent,
   refund-on-outcome**, so the envelope can never widen.
7. **Memory is written by the harness from committed evidence, and by
   the operator — never by the model.** A streaming, allocation-free
   `Harvester` pairs requests, observations and verifications (one
   open effect at a time) and remembers mutations only with a passing
   verification. Operator text is masked by the dispatcher before the
   `SENSE` answer becomes a durable record.
8. **The brief is identity**, bound into the seed snapshot (151 bytes)
   and carried in the committed `RUN` intent; unbriefed runs stay
   byte-identical.

## Consequences

- Every existing fixture, distill table, and Jev cassette is
  unchanged.
- The supervisor is firmware-clean, and since ADR 0003 so is the run;
  the board's `RUN` dispatcher still has to be written. The host
  dispatcher double drives runs through `drive_segment`.
- RAM: `AgentState` 7,384 B, the `supervise` future 2,280 B (host,
  64-bit; pinned by test). Briefed runs use a 1536-byte prompt buffer
  (+512 B). Board figures are not yet measured.
- Recall is recency-ordered, not relevance-ranked.
- Proof posture: exhaustive and seeded property tests (all 8×10 goal
  transitions, the never-widening envelope, every byte flip of a
  restored checkpoint, 174 injected resets at Waymaker boundaries);
  no mechanized prover — no Verus toolchain exists in this workspace.
