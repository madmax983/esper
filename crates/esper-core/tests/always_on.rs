//! Always-on primitives (SPEC §21): goals, memory, brief, checkpoint.
//!
//! Exhaustive where the space is small (the 8×10 goal transition
//! table, every terminal status × cadence), property-style where it is
//! not (a seeded xorshift drives random spawn/settle sequences against
//! the envelope's never-widen invariant). No external proptest crate:
//! the generator is deterministic, so every failure reproduces.

use esper_core::beat::{Beat, BeatSpan, SchedulePolicy};
use esper_core::brief::{BRIEF_CAP, Brief};
use esper_core::checkpoint::{
    AgentCheckpoint, CHECKPOINT_VERSION, MAX_CHECKPOINT_BYTES, decode, encode, encoded_len,
};
use esper_core::compact::{NOTE_MAX, Note};
use esper_core::error::Error;
use esper_core::goal::{
    Cadence, Envelope, Goal, GoalEvent, GoalId, GoalLedger, GoalSpec, GoalStatus, Grant, MAX_GOALS,
    Pick, TRANSITIONS, transition,
};
use esper_core::ids::{Digest, RunId, ToolId};
use esper_core::mask::fnv1a64;
use esper_core::memory::{
    MAX_MEMORY, MAX_PINNED, MEMORY_INPUT_MAX, Memory, MemoryEntry, MemoryKey, Provenance, RECALL_K,
    ingest, recall,
};
use esper_core::registry::Capabilities;
use esper_core::state::TerminalStatus;

// ---------------------------------------------------------------------
// Helpers

const ALL_STATUSES: [TerminalStatus; 9] = [
    TerminalStatus::Completed,
    TerminalStatus::NeedsInput,
    TerminalStatus::Denied,
    TerminalStatus::BudgetExhausted,
    TerminalStatus::Stuck,
    TerminalStatus::ToolUnavailable,
    TerminalStatus::ModelInvalid,
    TerminalStatus::StorageFault,
    TerminalStatus::Incompatible,
];

const fn full_caps() -> Capabilities {
    Capabilities {
        read_pins: [0, 1, 2, 3, 4, 5, 6, 7],
        read_count: 8,
        write_pins: [0, 1, 2, 3, 4, 5, 6, 7],
        write_count: 8,
        sensors: [0, 1, 2, 3],
        sensor_count: 4,
        allow_timer: true,
        allow_status: true,
    }
}

const fn envelope(per_run_turns: u16, turns: u32, runs: u16) -> Envelope {
    Envelope {
        per_run_turns,
        per_run_mutations: 2,
        turns,
        mutations: 20,
        runs,
    }
}

const fn spec(objective: &[u8], priority: u8, cadence: Cadence) -> GoalSpec<'_> {
    GoalSpec {
        objective,
        priority,
        cadence,
        envelope: envelope(4, 40, 10),
        capabilities: full_caps(),
    }
}

const fn every(n: u32) -> Cadence {
    Cadence::Every(BeatSpan::new(n).expect("nonzero span"))
}

fn add(ledger: &mut GoalLedger, spec: &GoalSpec<'_>, now: u64) -> GoalId {
    let goal = ledger
        .admit(spec, &full_caps(), Beat::new(now))
        .expect("admit");
    let id = goal.id;
    ledger.insert(goal).expect("insert");
    id
}

const fn run_id(n: u64) -> RunId {
    RunId::new(n)
}

/// Spawn `id` with the reservation the ledger computes.
fn spawn(ledger: &mut GoalLedger, id: GoalId, run: u64) -> Grant {
    let grant = ledger.reservation(id).expect("reservation");
    ledger.spawn(id, run_id(run), grant).expect("spawn");
    grant
}

/// A seeded xorshift64: deterministic property-test input.
struct Rng(u64);

impl Rng {
    const fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    const fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

fn obs_key(tool: u8, args: &[u8]) -> MemoryKey {
    MemoryKey::Observation {
        tool: ToolId::new(tool),
        args_digest: Digest::of_bytes(args),
    }
}

fn prov(goal: Option<GoalId>, beat: u64) -> Provenance {
    Provenance {
        goal,
        run: goal.map(|g| run_id(u64::from(g.get()) << 48)),
        beat: Beat::new(beat),
    }
}

fn entry(key: MemoryKey, label: &[u8], value: &[u8], goal: Option<GoalId>) -> MemoryEntry {
    MemoryEntry::new(key, label, value, prov(goal, 1)).expect("entry")
}

// ---------------------------------------------------------------------
// Goal status machine (SPEC §21.2)

#[test]
fn goal_transition_table_is_exactly_the_eleven_edges() {
    let mut legal = 0;
    for from in GoalStatus::ALL {
        for event in GoalEvent::ALL {
            let listed = TRANSITIONS
                .iter()
                .find(|(src, ev, _)| *src == from && *ev == event);
            match (transition(from, event), listed) {
                (Ok(to), Some((_, _, expected))) => {
                    assert_eq!(to, *expected);
                    legal += 1;
                }
                (Err(Error::GoalIllegalTransition { from: f, event: e }), None) => {
                    assert_eq!((f, e), (from, event));
                }
                (got, listed) => panic!("{from} × {event}: {got:?} vs table {listed:?}"),
            }
        }
    }
    assert_eq!(legal, 11);
}

#[test]
fn terminal_goal_statuses_have_no_outgoing_edge() {
    for from in GoalStatus::ALL.into_iter().filter(|s| s.is_terminal()) {
        for event in GoalEvent::ALL {
            assert!(transition(from, event).is_err(), "{from} took {event}");
        }
    }
    // And every non-terminal status has at least one way forward.
    for from in GoalStatus::ALL.into_iter().filter(|s| !s.is_terminal()) {
        assert!(GoalEvent::ALL.iter().any(|e| transition(from, *e).is_ok()));
    }
}

#[test]
fn goal_codes_round_trip_and_unknown_codes_fail_closed() {
    for status in GoalStatus::ALL {
        assert_eq!(GoalStatus::from_code(status.code()), Some(status));
    }
    for event in GoalEvent::ALL {
        assert_eq!(GoalEvent::from_code(event.code()), Some(event));
    }
    assert_eq!(GoalStatus::from_code(8), None);
    assert_eq!(GoalEvent::from_code(10), None);
    assert_eq!(GoalId::new(0), None);
}

// ---------------------------------------------------------------------
// Admission

#[test]
fn admit_validates_the_spec() {
    let ledger = GoalLedger::new();
    let caps = full_caps();
    let now = Beat::new(1);
    let mut bad = spec(b"read pin 3", 1, Cadence::Once);
    for broken in [
        Envelope {
            per_run_turns: 0,
            ..bad.envelope
        },
        Envelope {
            per_run_mutations: 0,
            ..bad.envelope
        },
        Envelope {
            turns: 0,
            ..bad.envelope
        },
        Envelope {
            mutations: 0,
            ..bad.envelope
        },
        Envelope {
            runs: 0,
            ..bad.envelope
        },
    ] {
        bad.envelope = broken;
        assert!(matches!(
            ledger.admit(&bad, &caps, now),
            Err(Error::GoalSpecInvalid(_))
        ));
    }

    let empty = spec(b"", 1, Cadence::Once);
    assert!(matches!(
        ledger.admit(&empty, &caps, now),
        Err(Error::GoalSpecInvalid(_))
    ));

    let long = [b'a'; MEMORY_INPUT_MAX + 1];
    let overlong = spec(&long, 1, Cadence::Once);
    assert_eq!(
        ledger.admit(&overlong, &caps, now),
        Err(Error::MemoryInputTooLong)
    );

    // A goal may narrow the agent's capabilities but never widen them.
    let narrow = Capabilities {
        read_pins: [3, 0, 0, 0, 0, 0, 0, 0],
        read_count: 1,
        ..Capabilities::empty()
    };
    let narrow_spec = GoalSpec {
        capabilities: narrow,
        ..spec(b"read pin 3", 1, Cadence::Once)
    };
    assert!(ledger.admit(&narrow_spec, &caps, now).is_ok());
    assert!(matches!(
        ledger.admit(&spec(b"x", 1, Cadence::Once), &narrow, now),
        Err(Error::GoalSpecInvalid(_))
    ));
}

#[test]
fn admit_masks_the_objective_before_it_becomes_durable() {
    let ledger = GoalLedger::new();
    let raw = b"call ops with Bearer abcdefghijklmnopqrstuvwxyz0123 then mail bob@example.com";
    let goal = ledger
        .admit(&spec(raw, 1, Cadence::Once), &full_caps(), Beat::ZERO)
        .expect("admit");
    let text = goal.objective.as_bytes();
    assert!(text.len() <= NOTE_MAX);
    assert!(!contains(text, b"abcdefghijklmnop"), "secret leaked");
    assert!(contains(text, b"[redacted:secret#1]"));
}

#[test]
fn ids_are_assigned_in_order_and_never_reused() {
    let mut ledger = GoalLedger::new();
    let first = add(&mut ledger, &spec(b"a", 1, Cadence::Once), 0);
    assert_eq!(first, GoalId::MIN);
    // A goal built for one id cannot be inserted twice.
    let goal = ledger
        .admit(&spec(b"b", 1, Cadence::Once), &full_caps(), Beat::ZERO)
        .expect("admit");
    ledger.insert(goal).expect("insert");
    assert_eq!(ledger.insert(goal), Err(Error::GoalOutOfOrder));

    // Retire the first goal, reap it: its id stays retired.
    ledger
        .control(first, GoalEvent::Retire, Beat::ZERO)
        .expect("retire");
    assert_eq!(ledger.reap(), 1);
    let third = add(&mut ledger, &spec(b"c", 1, Cadence::Once), 0);
    assert_eq!(third.get(), 3);
    assert!(ledger.get(first).is_none());
}

#[test]
fn ledger_fills_and_fails_closed() {
    let mut ledger = GoalLedger::new();
    for _ in 0..MAX_GOALS {
        add(&mut ledger, &spec(b"g", 1, Cadence::Once), 0);
    }
    assert_eq!(
        ledger.admit(
            &spec(b"one more", 1, Cadence::Once),
            &full_caps(),
            Beat::ZERO
        ),
        Err(Error::GoalLedgerFull)
    );
}

// ---------------------------------------------------------------------
// Scheduling

#[test]
fn pick_prefers_resumable_obligations_then_priority_then_due_then_id() {
    let mut ledger = GoalLedger::new();
    let low = add(&mut ledger, &spec(b"low", 1, every(5)), 0);
    let high = add(&mut ledger, &spec(b"high", 9, every(5)), 0);
    let high_later = add(&mut ledger, &spec(b"high later", 9, every(5)), 3);
    let parked = add(&mut ledger, &spec(b"parked", 0, Cadence::Once), 0);
    spawn(&mut ledger, parked, 99);
    ledger.park(parked, Beat::new(1)).expect("park");

    let no_input = |_: GoalId| false;
    // Priority first, then the earlier due beat.
    assert_eq!(ledger.pick(Beat::new(1), no_input), Pick::Spawn(high));
    assert_eq!(ledger.pick(Beat::new(5), no_input), Pick::Spawn(high));
    // Input for the parked goal beats every due goal, even at priority 0.
    assert_eq!(
        ledger.pick(Beat::new(5), |g| g == parked),
        Pick::Resume(parked)
    );
    // Input for an Active goal means nothing: only Waiting goals resume.
    assert_eq!(ledger.pick(Beat::new(5), |g| g == low), Pick::Spawn(high));

    // Same priority and due beat: the lower id wins.
    let tie_a = add(&mut ledger, &spec(b"tie", 200, every(1)), 7);
    let _tie_b = add(&mut ledger, &spec(b"tie", 200, every(1)), 7);
    assert_eq!(ledger.pick(Beat::new(7), no_input), Pick::Spawn(tie_a));
    // Not yet due: nothing at that priority runs early.
    assert_eq!(ledger.pick(Beat::new(2), no_input), Pick::Spawn(high));
    assert_ne!(ledger.pick(Beat::new(2), no_input), Pick::Spawn(high_later));

    // Paused goals never run.
    let mut quiet = GoalLedger::new();
    let only = add(&mut quiet, &spec(b"only", 1, every(1)), 0);
    quiet
        .control(only, GoalEvent::Pause, Beat::ZERO)
        .expect("pause");
    assert_eq!(quiet.pick(Beat::new(100), no_input), Pick::Idle);
    quiet
        .control(only, GoalEvent::Unpause, Beat::new(50))
        .expect("unpause");
    assert_eq!(quiet.get(only).map(|g| g.next_due), Some(Beat::new(50)));
    assert_eq!(quiet.pick(Beat::new(50), no_input), Pick::Spawn(only));
}

// ---------------------------------------------------------------------
// Envelope: reserve on intent, refund on outcome (SPEC §21.2)

#[test]
fn reservation_is_min_of_per_run_and_remaining() {
    let mut ledger = GoalLedger::new();
    let id = add(
        &mut ledger,
        &GoalSpec {
            envelope: Envelope {
                per_run_turns: 4,
                per_run_mutations: 3,
                turns: 6,
                mutations: 4,
                runs: 5,
            },
            ..spec(b"g", 1, every(1))
        },
        0,
    );
    let first = spawn(&mut ledger, id, 1);
    assert_eq!(
        first,
        Grant {
            turns: 4,
            mutations: 3
        }
    );
    // Spend everything: refund nothing.
    ledger
        .settle(
            id,
            TerminalStatus::Completed,
            Grant::ZERO,
            Beat::new(1),
            &SchedulePolicy::DEFAULT,
        )
        .expect("settle");
    let second = ledger.reservation(id).expect("reservation");
    assert_eq!(
        second,
        Grant {
            turns: 2,
            mutations: 1
        }
    );
    // A grant that is not the computed reservation is a replay divergence.
    assert_eq!(
        ledger.spawn(
            id,
            run_id(2),
            Grant {
                turns: 3,
                mutations: 1
            }
        ),
        Err(Error::GoalOutOfOrder)
    );
}

#[test]
fn refund_larger_than_reservation_fails_closed() {
    let mut ledger = GoalLedger::new();
    let id = add(&mut ledger, &spec(b"g", 1, Cadence::Once), 0);
    let grant = spawn(&mut ledger, id, 1);
    let greedy = Grant {
        turns: grant.turns + 1,
        mutations: 0,
    };
    assert_eq!(
        ledger.settle(
            id,
            TerminalStatus::Completed,
            greedy,
            Beat::new(1),
            &SchedulePolicy::DEFAULT
        ),
        Err(Error::RefundExceedsReservation)
    );
}

/// Property: under any sequence of spawn, park, resume, and settle with
/// any legal refund, remaining + reserved never exceeds the grant, and
/// what the goal has spent equals the sum of (reservation − refund).
#[test]
fn envelope_never_widens_under_random_lifecycles() {
    let policy = SchedulePolicy {
        max_consecutive_failures: 4,
        ..SchedulePolicy::DEFAULT
    };
    for seed in 1..=300u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1);
        let total_turns = 1 + u32::try_from(rng.below(60)).expect("small");
        let total_mutations = 1 + u32::try_from(rng.below(30)).expect("small");
        let runs = 1 + u16::try_from(rng.below(12)).expect("small");
        let env = Envelope {
            per_run_turns: 1 + u16::try_from(rng.below(8)).expect("small"),
            per_run_mutations: 1 + u16::try_from(rng.below(4)).expect("small"),
            turns: total_turns,
            mutations: total_mutations,
            runs,
        };
        let mut ledger = GoalLedger::new();
        let id = add(
            &mut ledger,
            &GoalSpec {
                envelope: env,
                ..spec(b"prop", 1, every(1))
            },
            0,
        );
        let (mut spent_turns, mut spent_mutations, mut beat) = (0u32, 0u32, 1u64);
        loop {
            let goal = *ledger.get(id).expect("goal");
            goal.check_envelope().expect("never widened");
            assert_eq!(goal.remaining.turns + spent_turns, total_turns);
            assert_eq!(goal.remaining.mutations + spent_mutations, total_mutations);
            if goal.status.is_terminal() {
                break;
            }
            beat += 1;
            if goal.next_due > Beat::new(beat) {
                beat = goal.next_due.get();
            }
            let grant = spawn(&mut ledger, id, beat);
            if rng.below(3) == 0 {
                ledger.park(id, Beat::new(beat)).expect("park");
                ledger
                    .get(id)
                    .expect("goal")
                    .check_envelope()
                    .expect("parked");
                ledger.resume(id).expect("resume");
            }
            let refund = Grant {
                turns: u16::try_from(rng.below(u64::from(grant.turns) + 1)).expect("small"),
                mutations: u16::try_from(rng.below(u64::from(grant.mutations) + 1)).expect("small"),
            };
            spent_turns += u32::from(grant.turns - refund.turns);
            spent_mutations += u32::from(grant.mutations - refund.mutations);
            let status = ALL_STATUSES[usize::try_from(rng.below(9)).expect("small")];
            ledger
                .settle(id, status, refund, Beat::new(beat), &policy)
                .expect("settle");
        }
        let goal = ledger.get(id).expect("goal");
        assert!(goal.runs_started <= u32::from(runs));
        assert_eq!(goal.reserved, Grant::ZERO);
    }
}

// ---------------------------------------------------------------------
// Settlement table (SPEC §21.2)

#[test]
fn settlement_follows_the_table_for_every_status_and_cadence() {
    let policy = SchedulePolicy::DEFAULT;
    for status in ALL_STATUSES {
        for cadence in [Cadence::Once, every(7)] {
            let mut ledger = GoalLedger::new();
            let id = add(&mut ledger, &spec(b"g", 1, cadence), 0);
            let grant = spawn(&mut ledger, id, 1);
            let event = ledger
                .settle(id, status, grant, Beat::new(10), &policy)
                .expect("settle");
            let goal = ledger.get(id).expect("goal");
            // The §21.2 table, one arm per row: two rows share an
            // outcome on purpose, so the arms stay explicit.
            #[allow(clippy::match_same_arms)]
            let expected = match (status, cadence) {
                (TerminalStatus::Completed, Cadence::Once) => GoalEvent::Satisfy,
                (TerminalStatus::Completed, Cadence::Every(_)) => GoalEvent::Recur,
                (
                    TerminalStatus::Denied
                    | TerminalStatus::Incompatible
                    | TerminalStatus::StorageFault,
                    _,
                ) => GoalEvent::Fail,
                _ => GoalEvent::Recur,
            };
            assert_eq!(event, expected, "{status} under {cadence:?}");
            assert_eq!(goal.last_status, Some(status));
            assert_eq!(goal.run, None);
            match (status, event) {
                (TerminalStatus::Completed, GoalEvent::Recur) => {
                    assert_eq!(goal.next_due, Beat::new(17));
                    assert_eq!(goal.failures, 0);
                }
                (_, GoalEvent::Recur) => {
                    // First failure: base backoff of one beat.
                    assert_eq!(goal.next_due, Beat::new(11));
                    assert_eq!(goal.failures, 1);
                }
                _ => {}
            }
        }
    }
}

#[test]
fn consecutive_failures_back_off_then_fail_and_success_resets() {
    let policy = SchedulePolicy::DEFAULT; // fail at 3, backoff 1, 2, …
    let mut ledger = GoalLedger::new();
    let id = add(&mut ledger, &spec(b"flaky", 1, every(10)), 0);
    let mut beat = 1;
    let mut dues = [0u64; 2];
    for due in &mut dues {
        let grant = spawn(&mut ledger, id, beat);
        ledger
            .settle(id, TerminalStatus::Stuck, grant, Beat::new(beat), &policy)
            .expect("settle");
        *due = ledger.get(id).expect("goal").next_due.get() - beat;
        beat = ledger.get(id).expect("goal").next_due.get();
    }
    assert_eq!(dues, [1, 2], "exponential backoff");
    // A success resets the streak.
    let grant = spawn(&mut ledger, id, beat);
    ledger
        .settle(
            id,
            TerminalStatus::Completed,
            grant,
            Beat::new(beat),
            &policy,
        )
        .expect("settle");
    assert_eq!(ledger.get(id).expect("goal").failures, 0);
    // Three failures in a row end the goal.
    for _ in 0..3 {
        beat = ledger.get(id).expect("goal").next_due.get();
        let grant = spawn(&mut ledger, id, beat);
        ledger
            .settle(id, TerminalStatus::Stuck, grant, Beat::new(beat), &policy)
            .expect("settle");
    }
    assert_eq!(ledger.get(id).expect("goal").status, GoalStatus::Failed);
}

#[test]
fn an_unfundable_goal_exhausts_instead_of_recurring() {
    let mut ledger = GoalLedger::new();
    let id = add(
        &mut ledger,
        &GoalSpec {
            envelope: Envelope {
                per_run_turns: 5,
                per_run_mutations: 1,
                turns: 100,
                mutations: 100,
                runs: 2,
            },
            ..spec(b"two runs", 1, every(1))
        },
        0,
    );
    for beat in 1..=2 {
        let grant = spawn(&mut ledger, id, beat);
        ledger
            .settle(
                id,
                TerminalStatus::Completed,
                grant,
                Beat::new(beat),
                &SchedulePolicy::DEFAULT,
            )
            .expect("settle");
    }
    let goal = ledger.get(id).expect("goal");
    assert_eq!(goal.status, GoalStatus::Exhausted);
    assert_eq!(goal.runs_started, 2);
    assert_eq!(ledger.pick(Beat::new(99), |_| false), Pick::Idle);
}

#[test]
fn operator_control_never_drops_an_open_obligation() {
    let mut ledger = GoalLedger::new();
    let id = add(&mut ledger, &spec(b"ask", 1, Cadence::Once), 0);
    spawn(&mut ledger, id, 1);
    for event in [GoalEvent::Pause, GoalEvent::Retire] {
        assert_eq!(
            ledger.control(id, event, Beat::new(1)),
            Err(Error::GoalBusy)
        );
    }
    ledger.park(id, Beat::new(2)).expect("park");
    assert_eq!(
        ledger.control(id, GoalEvent::Retire, Beat::new(3)),
        Err(Error::GoalBusy)
    );
    // Non-operator events are refused through the control path.
    let mut other = GoalLedger::new();
    let idle = add(&mut other, &spec(b"idle", 1, Cadence::Once), 0);
    assert!(matches!(
        other.control(idle, GoalEvent::Satisfy, Beat::ZERO),
        Err(Error::GoalIllegalTransition { .. })
    ));
}

#[test]
fn overdue_is_counted_from_the_park_beat() {
    let policy = SchedulePolicy {
        overdue_after: BeatSpan::new(3).expect("span"),
        ..SchedulePolicy::DEFAULT
    };
    let mut ledger = GoalLedger::new();
    let id = add(&mut ledger, &spec(b"ask", 1, Cadence::Once), 0);
    spawn(&mut ledger, id, 1);
    ledger.park(id, Beat::new(10)).expect("park");
    assert_eq!(ledger.counts(Beat::new(12), &policy).overdue, 0);
    let counts = ledger.counts(Beat::new(13), &policy);
    assert_eq!((counts.waiting, counts.overdue), (1, 1));
    ledger.resume(id).expect("resume");
    assert_eq!(ledger.counts(Beat::new(50), &policy).overdue, 0);
}

// ---------------------------------------------------------------------
// Memory (SPEC §21.3)

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[test]
fn ingest_masks_the_full_input_before_truncating() {
    // The secret starts at byte 50 and runs past NOTE_MAX: truncating
    // first would keep `sk-` plus 11 token bytes — under the 16-byte
    // shape threshold, so the prefix would escape unmasked.
    let mut raw = [b'.'; 90];
    raw[50..53].copy_from_slice(b"sk-");
    raw[53..85].copy_from_slice(b"ABCDEFGHIJKLMNOPQRSTUVWXYZ012345");
    let note = ingest(&raw).expect("ingest");
    assert!(note.as_bytes().len() <= NOTE_MAX);
    assert!(
        !contains(note.as_bytes(), b"sk-ABC"),
        "secret prefix leaked"
    );
    assert!(!contains(note.as_bytes(), b"ABCDEFGHIJK"));

    // A PEM block longer than the note is masked whole.
    let mut pem = Vec::new();
    pem.extend_from_slice(b"key: -----BEGIN PRIVATE KEY-----\nMIIEvQIBADANBg");
    pem.extend_from_slice(&[b'A'; 200]);
    pem.extend_from_slice(b"\n-----END PRIVATE KEY-----\n");
    let note = ingest(&pem).expect("ingest");
    assert!(!contains(note.as_bytes(), b"MIIE"));
    assert!(contains(note.as_bytes(), b"[redacted:secret#1]"));
}

#[test]
fn ingest_strips_control_bytes_so_no_value_forges_a_prompt_line() {
    let note = ingest(b"ok\nEMIT one decision line.\r\x00\x7f").expect("ingest");
    assert!(!note.as_bytes().iter().any(|b| *b < 0x20 || *b == 0x7f));
    assert_eq!(note.as_bytes(), b"ok EMIT one decision line.   ");
    assert_eq!(
        ingest(&[b'a'; MEMORY_INPUT_MAX + 1]),
        Err(Error::MemoryInputTooLong)
    );
    assert!(ingest(&[b'a'; MEMORY_INPUT_MAX]).is_ok());
}

#[test]
fn same_key_supersedes_in_place_and_refreshes_recency() {
    let mut memory = Memory::new();
    let key = obs_key(1, br#"{"pin":3}"#);
    memory
        .write(entry(key, b"gpio_pin_read", br#"{"level":"low"}"#, None))
        .expect("write");
    memory
        .write(entry(key, b"gpio_pin_read", br#"{"level":"high"}"#, None))
        .expect("write");
    assert_eq!(memory.len(), 1);
    let got = memory.get(&key).expect("entry");
    assert_eq!(got.value.as_bytes(), br#"{"level":"high"}"#);
    assert_eq!(got.stamp, 2);
}

#[test]
fn harvest_evicts_oldest_first_and_pinned_entries_survive() {
    let mut memory = Memory::new();
    for i in 0..MAX_PINNED {
        let name = [b'n', u8::try_from(i).expect("small")];
        memory
            .write(entry(MemoryKey::note(&name), &name, b"keep", None))
            .expect("pinned");
    }
    assert_eq!(
        memory.write(entry(MemoryKey::note(b"ninth"), b"ninth", b"x", None)),
        Err(Error::MemoryFull),
        "pinned entries are capped"
    );
    // Fill the rest with observations, then overflow by ten.
    let extra = 10;
    for i in 0..(MAX_MEMORY - MAX_PINNED + extra) {
        let args = [b'{', u8::try_from(i).expect("small"), b'}'];
        memory
            .write(entry(obs_key(1, &args), b"read", b"v", None))
            .expect("harvest");
    }
    assert_eq!(memory.len(), MAX_MEMORY);
    assert_eq!(memory.pinned_count(), MAX_PINNED, "no pinned entry evicted");
    for i in 0..extra {
        let args = [b'{', u8::try_from(i).expect("small"), b'}'];
        assert!(memory.get(&obs_key(1, &args)).is_none(), "oldest evicted");
    }
    let newest = [
        b'{',
        u8::try_from(MAX_MEMORY - MAX_PINNED + extra - 1).expect("small"),
        b'}',
    ];
    assert!(memory.get(&obs_key(1, &newest)).is_some());
    // Superseding a pinned note at the cap is fine: it is not new.
    memory
        .write(entry(MemoryKey::note(b"n\x00"), b"n0", b"changed", None))
        .expect("supersede pinned");
    // Forget frees a pinned slot.
    assert!(memory.forget(&MemoryKey::note(b"n\x00")));
    assert!(!memory.forget(&MemoryKey::note(b"n\x00")));
    memory
        .write(entry(MemoryKey::note(b"ninth"), b"ninth", b"x", None))
        .expect("room again");
}

#[test]
fn recall_orders_outcome_then_pinned_then_own_then_other_observations() {
    let g1 = GoalId::new(1).expect("id");
    let g2 = GoalId::new(2).expect("id");
    let mut memory = Memory::new();
    let other_old = obs_key(1, b"{other-old}");
    let own_old = obs_key(1, b"{own-old}");
    let own_new = obs_key(1, b"{own-new}");
    let other_new = obs_key(1, b"{other-new}");
    memory
        .write(entry(other_old, b"o", b"1", Some(g2)))
        .expect("w");
    memory
        .write(entry(own_old, b"o", b"2", Some(g1)))
        .expect("w");
    memory
        .write(entry(
            MemoryKey::Outcome { goal: g2 },
            b"goal#2",
            b"x",
            Some(g2),
        ))
        .expect("w");
    memory
        .write(entry(MemoryKey::note(b"pref"), b"pref", b"low power", None))
        .expect("w");
    memory
        .write(entry(own_new, b"o", b"3", Some(g1)))
        .expect("w");
    memory
        .write(entry(other_new, b"o", b"4", Some(g2)))
        .expect("w");
    memory
        .write(entry(
            MemoryKey::Outcome { goal: g1 },
            b"goal#1",
            b"y",
            Some(g1),
        ))
        .expect("w");

    let got: Vec<MemoryKey> = recall(&memory, g1).entries().map(|e| e.key).collect();
    assert_eq!(got.len(), RECALL_K);
    assert_eq!(
        got,
        vec![
            MemoryKey::Outcome { goal: g1 },
            MemoryKey::note(b"pref"),
            own_new,
            own_old,
        ]
    );
    // Another goal never sees g1's outcome first, and never g1's
    // outcome at all (outcomes are per goal).
    let for_g2: Vec<MemoryKey> = recall(&memory, g2).entries().map(|e| e.key).collect();
    assert_eq!(for_g2.first(), Some(&MemoryKey::Outcome { goal: g2 }));
    assert!(!for_g2.contains(&MemoryKey::Outcome { goal: g1 }));
    assert!(recall(&Memory::new(), g1).is_empty());
}

#[test]
fn memory_from_parts_rejects_structural_corruption() {
    let a = MemoryEntry {
        stamp: 1,
        ..entry(obs_key(1, b"a"), b"a", b"a", None)
    };
    let b = MemoryEntry {
        stamp: 2,
        ..entry(obs_key(1, b"b"), b"b", b"b", None)
    };
    assert!(Memory::from_parts(&[a, b], 3).is_ok());
    assert!(
        Memory::from_parts(&[a, b], 2).is_err(),
        "stamp past allocator"
    );
    assert!(
        Memory::from_parts(&[a, MemoryEntry { stamp: 1, ..b }], 3).is_err(),
        "duplicate stamp"
    );
    assert!(
        Memory::from_parts(&[a, MemoryEntry { key: a.key, ..b }], 3).is_err(),
        "duplicate key"
    );
    assert!(
        Memory::from_parts(&[MemoryEntry { stamp: 0, ..a }], 3).is_err(),
        "unstamped"
    );
}

// ---------------------------------------------------------------------
// Brief (SPEC §21.4)

#[test]
fn brief_renders_goal_then_mem_lines() {
    let g = GoalId::new(1).expect("id");
    let mut memory = Memory::new();
    memory
        .write(entry(
            obs_key(1, br#"{"pin":3}"#),
            br#"gpio_pin_read {"pin":3}"#,
            br#"{"pin":3,"level":"low"}"#,
            Some(g),
        ))
        .expect("w");
    let objective = Note::from_bytes(b"keep pin 3 low").expect("note");
    let brief = Brief::compose(&objective, &recall(&memory, g));
    assert_eq!(
        brief.as_bytes(),
        b"GOAL keep pin 3 low\nMEM gpio_pin_read {\"pin\":3} = {\"pin\":3,\"level\":\"low\"}\n"
    );
    assert_eq!(brief.lines(), 2);
    assert_eq!(brief.digest(), fnv1a64(brief.as_bytes()));
}

#[test]
fn brief_is_bounded_and_drops_whole_lines_deterministically() {
    let g = GoalId::new(1).expect("id");
    let mut memory = Memory::new();
    for i in 0..RECALL_K {
        let args = [b'{', u8::try_from(i).expect("small"), b'}'];
        memory
            .write(entry(obs_key(1, &args), &[b'L'; 64], &[b'V'; 64], Some(g)))
            .expect("w");
    }
    let objective = Note::from_bytes(&[b'o'; NOTE_MAX]).expect("note");
    let brief = Brief::compose(&objective, &recall(&memory, g));
    assert!(brief.as_bytes().len() <= BRIEF_CAP);
    // GOAL (70) + one full MEM line (4 + 64 + 3 + 64 + 1 = 136) = 206;
    // a second MEM line would need 342 > 256, so exactly one fits.
    assert_eq!(brief.lines(), 2);
    assert_eq!(brief.as_bytes().len(), 206);
    assert!(brief.as_bytes().ends_with(b"\n"));
    assert!(brief.as_bytes().starts_with(b"GOAL "));
    // Same inputs, same bytes.
    assert_eq!(brief, Brief::compose(&objective, &recall(&memory, g)));
}

// ---------------------------------------------------------------------
// Checkpoint (SPEC §21.6)

/// A checkpoint exercising every encoded shape: once/every cadences,
/// a parked goal holding a run and reservation, a settled goal with a
/// last status, a narrowed capability set, and all three key kinds.
fn rich_checkpoint() -> AgentCheckpoint {
    let mut ledger = GoalLedger::new();
    let once = add(&mut ledger, &spec(b"once", 3, Cadence::Once), 0);
    let standing = add(
        &mut ledger,
        &GoalSpec {
            capabilities: Capabilities {
                read_pins: [3, 4, 0, 0, 0, 0, 0, 0],
                read_count: 2,
                sensors: [2, 0, 0, 0],
                sensor_count: 1,
                allow_status: true,
                ..Capabilities::empty()
            },
            ..spec(b"standing", 1, every(12))
        },
        0,
    );
    let grant = spawn(&mut ledger, standing, 7);
    ledger
        .settle(
            standing,
            TerminalStatus::Stuck,
            Grant { turns: 1, ..grant },
            Beat::new(5),
            &SchedulePolicy::DEFAULT,
        )
        .expect("settle");
    spawn(&mut ledger, once, 0x0001_0000_0001_0000);
    ledger.park(once, Beat::new(6)).expect("park");

    let mut memory = Memory::new();
    memory
        .write(entry(obs_key(1, b"{a}"), b"read a", b"low", Some(standing)))
        .expect("w");
    memory
        .write(entry(
            MemoryKey::Outcome { goal: standing },
            b"goal#2",
            b"stuck",
            Some(standing),
        ))
        .expect("w");
    memory
        .write(entry(
            MemoryKey::note(b"site"),
            b"site",
            b"greenhouse",
            None,
        ))
        .expect("w");
    AgentCheckpoint {
        last_beat: Beat::new(6),
        spawn_seq: 2,
        ledger,
        memory,
    }
}

fn encoded(state: &AgentCheckpoint) -> Vec<u8> {
    let mut buf = vec![0u8; MAX_CHECKPOINT_BYTES];
    let n = encode(state, &mut buf).expect("encode");
    assert_eq!(n, encoded_len(state));
    buf.truncate(n);
    buf
}

#[test]
fn checkpoint_round_trips_exactly() {
    let state = rich_checkpoint();
    let bytes = encoded(&state);
    assert_eq!(bytes[0], CHECKPOINT_VERSION);
    assert_eq!(decode(&bytes), Ok(state));
    let empty = AgentCheckpoint {
        last_beat: Beat::ZERO,
        spawn_seq: 0,
        ledger: GoalLedger::new(),
        memory: Memory::new(),
    };
    assert_eq!(decode(&encoded(&empty)), Ok(empty));
}

#[test]
fn checkpoint_fits_its_bound_when_everything_is_full() {
    let mut ledger = GoalLedger::new();
    for _ in 0..MAX_GOALS {
        let id = add(&mut ledger, &spec(&[b'o'; NOTE_MAX], 1, every(3)), 0);
        spawn(&mut ledger, id, u64::MAX);
        ledger.park(id, Beat::new(u64::MAX)).expect("park");
    }
    let mut memory = Memory::new();
    for i in 0..MAX_MEMORY {
        let args = [b'{', u8::try_from(i).expect("small"), b'}'];
        let goal = GoalId::new(u16::MAX);
        memory
            .write(entry(obs_key(255, &args), &[b'L'; 64], &[b'V'; 64], goal))
            .expect("w");
    }
    let state = AgentCheckpoint {
        last_beat: Beat::new(u64::MAX),
        spawn_seq: u32::MAX,
        ledger,
        memory,
    };
    let len = encoded_len(&state);
    assert!(
        len <= MAX_CHECKPOINT_BYTES,
        "{len} > {MAX_CHECKPOINT_BYTES}"
    );
    assert_eq!(decode(&encoded(&state)), Ok(state));
}

#[test]
fn checkpoint_encode_fails_closed_on_a_short_buffer() {
    let state = rich_checkpoint();
    let need = encoded_len(&state);
    let mut short = vec![0u8; need - 1];
    assert_eq!(
        encode(&state, &mut short),
        Err(Error::SnapshotBufferTooSmall)
    );
}

#[test]
fn every_truncation_and_every_bit_flip_fails_closed() {
    let bytes = encoded(&rich_checkpoint());
    for len in 0..bytes.len() {
        assert!(decode(&bytes[..len]).is_err(), "truncated at {len}");
    }
    for index in 0..bytes.len() {
        for bit in 0..8 {
            let mut flipped = bytes.clone();
            flipped[index] ^= 1 << bit;
            let err = decode(&flipped).expect_err("flip must fail");
            if index == 0 {
                assert!(matches!(err, Error::SnapshotVersionMismatch { .. }));
            } else {
                assert_eq!(err, Error::SnapshotChecksumMismatch, "byte {index}");
            }
        }
    }
}

#[test]
fn version_is_checked_before_the_checksum() {
    let mut bytes = encoded(&rich_checkpoint());
    bytes[0] = 9;
    assert_eq!(
        decode(&bytes),
        Err(Error::SnapshotVersionMismatch { found: 9 })
    );
}

/// Re-sign a mutated payload so it passes the checksum: structural
/// validation alone must reject it.
fn resign(mut bytes: Vec<u8>) -> Vec<u8> {
    let end = bytes.len() - 8;
    let sum = fnv1a64(&bytes[..end]).to_le_bytes();
    bytes[end..].copy_from_slice(&sum);
    bytes
}

#[test]
fn a_checksummed_but_invalid_payload_is_corrupt() {
    let bytes = encoded(&rich_checkpoint());
    // Header: version 1, last_beat 8, spawn_seq 4, next_goal_id 2,
    // next_stamp 4, goal count 1 → the first goal's id starts at 20.
    let mut zero_id = bytes.clone();
    zero_id[20] = 0;
    zero_id[21] = 0;
    assert_eq!(decode(&resign(zero_id)), Err(Error::SnapshotCorrupt));

    // A goal count beyond the ledger's capacity.
    let mut too_many = bytes.clone();
    too_many[19] = u8::try_from(MAX_GOALS + 1).expect("small");
    assert_eq!(decode(&resign(too_many)), Err(Error::SnapshotCorrupt));

    // The id allocator behind an existing goal id.
    let mut stale_alloc = bytes.clone();
    stale_alloc[13] = 1;
    stale_alloc[14] = 0;
    assert_eq!(decode(&resign(stale_alloc)), Err(Error::SnapshotCorrupt));

    // Trailing payload bytes.
    let mut trailing = bytes;
    let end = trailing.len() - 8;
    trailing.insert(end, 0);
    assert_eq!(decode(&resign(trailing)), Err(Error::SnapshotCorrupt));
}

#[test]
fn goal_ledger_from_parts_rejects_goals_mid_run() {
    let mut ledger = GoalLedger::new();
    let id = add(&mut ledger, &spec(b"g", 1, Cadence::Once), 0);
    spawn(&mut ledger, id, 1);
    let running: Goal = *ledger.get(id).expect("goal");
    assert_eq!(running.status, GoalStatus::Running);
    assert_eq!(
        GoalLedger::from_parts(&[running], 2),
        Err(Error::SnapshotCorrupt)
    );
    let widened = Goal {
        status: GoalStatus::Active,
        run: None,
        reserved: Grant::ZERO,
        remaining: esper_core::goal::Remaining {
            turns: running.envelope.turns + 1,
            ..running.remaining
        },
        ..running
    };
    assert_eq!(
        GoalLedger::from_parts(&[widened], 2),
        Err(Error::SnapshotCorrupt)
    );
}

/// Regression: `reap` and `forget` used to leave holes in the slot
/// arrays, while `from_parts` decodes into compacted slots — so a
/// checkpoint taken after either failed its own read-back, and later
/// inserts landed in different slots than in an agent that never
/// checkpointed. Removal now keeps storage canonical.
#[test]
fn checkpoint_round_trips_after_reap_and_forget() {
    let mut state = rich_checkpoint();
    let first = state.ledger.goals().next().expect("goal").id;
    // Settle the parked goal out of its obligation, then retire the
    // first-slot goal so reaping leaves a hole at slot 0.
    let parked = state
        .ledger
        .goals()
        .find(|g| g.status == GoalStatus::Waiting)
        .expect("parked")
        .id;
    state.ledger.resume(parked).expect("resume");
    state
        .ledger
        .settle(
            parked,
            TerminalStatus::Completed,
            Grant::ZERO,
            Beat::new(7),
            &SchedulePolicy::DEFAULT,
        )
        .expect("settle");
    assert_eq!(parked, first, "the parked goal sits in slot 0");
    assert_eq!(state.ledger.reap(), 1);
    // Forget an entry that is not the last one, leaving a hole.
    let oldest = state.memory.entries().next().expect("entry").key;
    assert!(state.memory.forget(&oldest));
    assert_eq!(decode(&encoded(&state)), Ok(state));

    // And the canonical layout means the next write lands where a
    // decoded store would put it.
    let mut live = state.memory;
    let mut restored = decode(&encoded(&state)).expect("decode").memory;
    let next = entry(obs_key(9, b"{z}"), b"z", b"z", None);
    live.write(next).expect("write");
    restored.write(next).expect("write");
    assert_eq!(live, restored);
}
