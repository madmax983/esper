# ADR 0002: Always-on lives in a supervisor above the run, paced by a durable beat counter

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

- The run's guarantees (§11.3) are the product. Nothing here may
  weaken them or change a golden fixture.
- There is no persistent clock across power loss (§7.3, Waymaker
  durable-time rule). "Every 10 minutes" cannot be a durable claim.
- Budgets are run identity and never widen (ADR 10). An agent that
  spawns runs forever needs the same property one level up, or it is
  an unbounded spender.
- Model output is untrusted prose (§8.1). A model that writes its own
  memory can launder hallucinations into "facts" the next run trusts.

## Decision

1. **A supervisor above the run, not a bigger run.** The supervisor
   has its own journal (`AgentJournal`) and drives ordinary runs
   through `drive_segment`. The run state machine is untouched. A
   long-lived run with an "idle" state was rejected: it would put an
   unbounded loop inside the bounded one and break §11.3 item 6 (one
   logical terminal).
2. **Heartbeats are a durable counter, not a clock.** `Beat(u64)` is
   committed before each beat's work, never skips, never repeats.
   Cadences and backoffs are in beats. The host maps beats to wall
   time and owns that claim.
3. **One unit of work per beat.** At most one run spawns or resumes
   per beat — the heartbeat twin of one call per turn (ADR 2). This
   bounds per-beat cost and keeps the scheduler deterministic.
4. **Goals carry a cumulative envelope, reserve-on-intent,
   refund-on-outcome.** The spawn reserves the run's grant before the
   run exists; settlement refunds the unspent part. A crash between
   the two holds the reservation, so the envelope can never widen.
5. **Memory is written by the harness from committed evidence, and by
   the operator — never by the model.** Harvest reads the settled
   run's committed trace: successful observations (mutations only
   with a passing verification) and the terminal outcome. Values are
   masked before they become durable. A `memory_write` tool was
   rejected for now: it changes the §17 catalog and gives the model a
   write path into its own future prompts.
6. **The brief is identity.** The goal objective and recalled memory
   reach the prompt as a pre-rendered, bounded `Brief` whose digest is
   bound into the seed snapshot (151 bytes), so a reboot cannot swap
   it. Unbriefed runs keep byte-identical prompts and seeds.
7. **A checkpoint bounds storage.** Between beats the supervisor
   folds its journal into a versioned, integrity-checked checkpoint
   and drops settled run journals.

## Consequences

- Every existing fixture, distill table, and Jev cassette is
  unchanged (unbriefed seeds and prompts are byte-identical).
- The prompt buffer grows to `BRIEFED_PROMPT_CAP` = 1536 bytes for
  briefed runs (+512 B of stack on the board). `PROMPT_CAP` and the
  default `InferenceSettings` stay 1024.
- Recall is recency-ordered, not relevance-ranked. That is honest
  for a 32-entry store and a 256-byte brief; semantic recall is a
  later rung's question.
- A goal whose run waits on human input is "overdue" after a policy
  number of beats — a signal, never a timeout. The supervisor never
  abandons a parked run.
- Proof posture: like E0–E4, invariants are enforced by exhaustive
  and property-style tests (all 8×10 goal transition pairs, the
  envelope never-widen sequence, the agent crash matrix), not by a
  mechanized prover — no Verus toolchain exists in this workspace.
