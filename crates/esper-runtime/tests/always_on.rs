//! E4+aon: the always-on supervisor end to end (SPEC §21).
//!
//! Every test drives real runs through the real engine: a scripted
//! model (wrapped to record prompts), the fake device, and the
//! supervisor's store. The crash matrix replays one mixed workload
//! with a reset injected at every agent crash point on every beat, and
//! at every run-level crash point, and demands the crash-free result
//! byte for byte (SPEC §21.5 guarantee 7).

use std::time::Duration;

use esper_core::beat::{Beat, BeatSpan, SchedulePolicy};
use esper_core::decision::Level;
use esper_core::goal::{Cadence, Envelope, GoalEvent, GoalId, GoalSpec, GoalStatus};
use esper_core::ids::{Digest, Pin, ToolId};
use esper_core::memory::MemoryKey;
use esper_core::registry::Capabilities;
use esper_core::state::TerminalStatus;
use esper_runtime::{
    AgentCrash, AgentCrashPoint, AgentError, AgentFrame, AgentSeed, AgentStore, AgentWorld,
    BackendError, BeatAction, BundleId, CrashPoint, Direction, FakeDevice, FaultPlan, Inbox,
    InferenceSettings, ModelBackend, Pulse, ScriptedBackend, Supervisor, TokenUsage, WriteRecord,
    drive_beat, halt_beat_at, run_heartbeat,
};

// ---------------------------------------------------------------------
// The world

/// The scripted model, recording every prompt it was shown.
struct Recorder {
    inner: ScriptedBackend,
    prompts: Vec<Vec<u8>>,
}

impl ModelBackend for Recorder {
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        let written = self.inner.infer(prompt, out)?;
        self.prompts.push(prompt.to_vec());
        Ok(written)
    }

    fn bundle_id(&self) -> BundleId {
        self.inner.bundle_id()
    }

    fn last_usage(&self) -> TokenUsage {
        self.inner.last_usage()
    }

    fn unemit(&mut self) {
        self.inner.unemit();
        self.prompts.pop();
    }
}

/// Everything that survives a supervisor crash.
struct World {
    model: Recorder,
    device: FakeDevice,
    faults: FaultPlan,
    inbox: Inbox,
}

impl World {
    fn new(script: &[&str]) -> Self {
        let mut device = FakeDevice::new();
        for pin in 0..8u8 {
            device.set_direction(Pin::new(pin).expect("pin"), Direction::Output);
        }
        Self {
            model: Recorder {
                inner: ScriptedBackend::new(
                    script.iter().map(|line| line.as_bytes().to_vec()).collect(),
                    InferenceSettings::briefed_settings(),
                ),
                prompts: Vec::new(),
            },
            device,
            faults: FaultPlan::new(),
            inbox: Inbox::new(),
        }
    }

    fn view(&mut self) -> AgentWorld<'_> {
        AgentWorld {
            model: &mut self.model,
            device: &mut self.device,
            faults: &mut self.faults,
            inbox: &mut self.inbox,
        }
    }

    fn prompt_text(&self, index: usize) -> String {
        String::from_utf8(self.model.prompts[index].clone()).expect("ASCII prompt")
    }
}

fn agent_seed(world: &World) -> AgentSeed {
    let mut seed = AgentSeed::default_slice();
    seed.template.model_bundle = world.model.bundle_id().0;
    seed
}

const fn every(n: u32) -> Cadence {
    Cadence::Every(BeatSpan::new(n).expect("span"))
}

const fn envelope(runs: u16) -> Envelope {
    Envelope {
        per_run_turns: 6,
        per_run_mutations: 2,
        turns: 120,
        mutations: 40,
        runs,
    }
}

const fn spec(objective: &[u8], priority: u8, cadence: Cadence, runs: u16) -> GoalSpec<'_> {
    GoalSpec {
        objective,
        priority,
        cadence,
        envelope: envelope(runs),
        capabilities: AgentSeed::default_slice().template.capabilities,
    }
}

fn beat(seed: &AgentSeed, store: &mut AgentStore, world: &mut World) -> Pulse {
    drive_beat(seed, store, &mut world.view(), None).expect("beat")
}

fn boot(seed: &AgentSeed, store: &mut AgentStore) -> Supervisor {
    Supervisor::boot(seed, store).expect("boot")
}

fn status_of(seed: &AgentSeed, store: &mut AgentStore, goal: GoalId) -> GoalStatus {
    boot(seed, store)
        .ledger()
        .get(goal)
        .map(|g| g.status)
        .expect("goal")
}

fn read_key(pin: u8) -> MemoryKey {
    MemoryKey::Observation {
        tool: ToolId::new(1),
        args_digest: Digest::of_bytes(format!("{{\"pin\": {pin}}}").as_bytes()),
    }
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

const READ3: &str = r#"CALL gpio_pin_read {"pin": 3}"#;
const FINISH_READ: &str = r#"FINISH {"status": "completed", "summary": "pin 3 low"}"#;

// ---------------------------------------------------------------------
// Heartbeat + goals

#[test]
fn a_standing_goal_runs_on_cadence_and_its_memory_reaches_the_next_prompt() {
    let mut world = World::new(&[READ3, FINISH_READ, READ3, FINISH_READ]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    let goal = boot(&seed, &mut store)
        .add_goal(&mut store, &spec(b"watch pin 3", 1, every(3), 10))
        .expect("goal");

    let first = beat(&seed, &mut store, &mut world);
    assert_eq!(first.beat, Beat::new(1));
    assert!(matches!(first.action, BeatAction::Spawned { goal: g, .. } if g == goal));
    let settled = first.settled.expect("settled");
    assert_eq!(settled.status, TerminalStatus::Completed);
    assert_eq!(settled.event, GoalEvent::Recur);
    assert_eq!(settled.harvested, 2, "one observation and one outcome");

    // Cadence 3: beats 2 and 3 are idle, beat 4 runs again.
    for expected in [2, 3] {
        let idle = beat(&seed, &mut store, &mut world);
        assert_eq!(idle.beat, Beat::new(expected));
        assert_eq!(idle.action, BeatAction::Idle);
    }
    let fourth = beat(&seed, &mut store, &mut world);
    assert!(matches!(fourth.action, BeatAction::Spawned { .. }));

    // The first run saw only its goal; the second saw what the first
    // learned.
    let first_prompt = world.prompt_text(0);
    assert!(first_prompt.contains("GOAL watch pin 3\n"));
    assert!(!first_prompt.contains("MEM "));
    let second_prompt = world.prompt_text(2);
    assert!(second_prompt.contains("GOAL watch pin 3\n"));
    assert!(
        second_prompt.contains("MEM goal#1 = completed pin 3 low\n"),
        "{second_prompt}"
    );
    assert!(second_prompt.contains(r#"MEM gpio_pin_read {"pin": 3} = "#));

    let sup = boot(&seed, &mut store);
    let remembered = sup.memory().get(&read_key(3)).expect("observation");
    assert_eq!(remembered.provenance.goal, Some(goal));
    assert_eq!(remembered.provenance.beat, Beat::new(4));
    assert!(contains(remembered.value.as_bytes(), b"\"level\":\"low\""));
}

#[test]
fn a_once_goal_is_satisfied_and_never_runs_again() {
    let mut world = World::new(&[READ3, FINISH_READ]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    let goal = boot(&seed, &mut store)
        .add_goal(&mut store, &spec(b"read pin 3 once", 1, Cadence::Once, 5))
        .expect("goal");
    let pulse = beat(&seed, &mut store, &mut world);
    assert_eq!(pulse.settled.map(|s| s.event), Some(GoalEvent::Satisfy));
    assert_eq!(pulse.counts.done, 1);
    for _ in 0..5 {
        assert_eq!(beat(&seed, &mut store, &mut world).action, BeatAction::Idle);
    }
    assert_eq!(status_of(&seed, &mut store, goal), GoalStatus::Satisfied);
}

#[test]
fn a_denied_run_fails_its_goal_at_once_under_least_privilege() {
    // The agent may write any pin; this goal may only read pin 3, so
    // its run is denied before dispatch — and denial is not retried.
    let mut world = World::new(&[r#"CALL gpio_pin_write {"pin": 3, "level": "high"}"#]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    let read_only = Capabilities {
        read_pins: [3, 0, 0, 0, 0, 0, 0, 0],
        read_count: 1,
        ..Capabilities::empty()
    };
    let goal = boot(&seed, &mut store)
        .add_goal(
            &mut store,
            &GoalSpec {
                capabilities: read_only,
                ..spec(b"read pin 3", 1, every(1), 10)
            },
        )
        .expect("goal");
    let pulse = beat(&seed, &mut store, &mut world);
    let settled = pulse.settled.expect("settled");
    assert_eq!(settled.status, TerminalStatus::Denied);
    assert_eq!(settled.event, GoalEvent::Fail);
    assert!(world.device.write_ledger().is_empty(), "never dispatched");
    assert_eq!(status_of(&seed, &mut store, goal), GoalStatus::Failed);
    // The spawned seed carried the goal's capabilities, not the agent's.
    let spawned = store
        .frames()
        .iter()
        .find_map(|f| match f {
            AgentFrame::RunSpawned { seed, .. } => Some(*seed),
            _ => None,
        })
        .expect("spawn frame");
    assert_eq!(spawned.capabilities, read_only);
}

#[test]
fn retryable_failures_back_off_then_fail_the_goal() {
    // Four malformed lines per run end each run ModelInvalid.
    let garbage = ["nonsense"; 12];
    let mut world = World::new(&garbage);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    let goal = boot(&seed, &mut store)
        .add_goal(&mut store, &spec(b"flaky", 1, every(1), 10))
        .expect("goal");
    // Default policy: backoff 1, 2; the third failure ends the goal.
    let mut ran_at = Vec::new();
    for _ in 0..8 {
        let pulse = beat(&seed, &mut store, &mut world);
        if let Some(settled) = pulse.settled {
            assert_eq!(settled.status, TerminalStatus::ModelInvalid);
            ran_at.push(pulse.beat.get());
        }
    }
    assert_eq!(ran_at, vec![1, 2, 4], "runs at 1, then +1, then +2");
    assert_eq!(status_of(&seed, &mut store, goal), GoalStatus::Failed);
}

#[test]
fn the_envelope_exhausts_after_its_runs_and_refunds_unspent_units() {
    let mut world = World::new(&[READ3, FINISH_READ, READ3, FINISH_READ]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    let goal = boot(&seed, &mut store)
        .add_goal(&mut store, &spec(b"twice", 1, every(1), 2))
        .expect("goal");
    let events: Vec<_> = (0..3)
        .map(|_| beat(&seed, &mut store, &mut world))
        .map(|p| p.settled.map(|s| s.event))
        .collect();
    assert_eq!(
        events,
        vec![Some(GoalEvent::Recur), Some(GoalEvent::Exhaust), None]
    );
    let sup = boot(&seed, &mut store);
    let g = sup.ledger().get(goal).expect("goal");
    assert_eq!(g.status, GoalStatus::Exhausted);
    // Each run spent 2 of its 6 turns and none of its 2 mutations; the
    // rest came back.
    assert_eq!(g.remaining.turns, 120 - 4);
    assert_eq!(g.remaining.mutations, 40);
    g.check_envelope().expect("never widened");
}

// ---------------------------------------------------------------------
// Parking: obligations survive, and never block other goals

const ASK: &str = r#"ASK {"prompt": "Which pin should be set high?", "schema": 1}"#;
const WRITE4: &str = r#"CALL gpio_pin_write {"pin": 4, "level": "high"}"#;
const FINISH_WRITE: &str = r#"FINISH {"status": "completed", "summary": "pin 4 high"}"#;

/// The mixed workload: an Ask goal (priority 9) that parks, a standing
/// read goal (priority 1) that keeps running meanwhile, input arriving
/// before beat 3, and the parked run resuming to write pin 4.
const MIXED_SCRIPT: [&str; 7] = [
    ASK,
    READ3,
    FINISH_READ,
    WRITE4,
    FINISH_WRITE,
    READ3,
    FINISH_READ,
];
const MIXED_BEATS: u64 = 5;

fn mixed_setup(world: &World) -> (AgentSeed, AgentStore, GoalId, GoalId) {
    let mut seed = agent_seed(world);
    seed.policy = SchedulePolicy {
        overdue_after: BeatSpan::ONE,
        ..SchedulePolicy::DEFAULT
    };
    let mut store = AgentStore::new();
    let mut sup = boot(&seed, &mut store);
    let ask = sup
        .add_goal(&mut store, &spec(b"set pin 4 as told", 9, Cadence::Once, 3))
        .expect("ask goal");
    let watch = sup
        .add_goal(&mut store, &spec(b"watch pin 3", 1, every(2), 10))
        .expect("watch goal");
    sup.remember(&mut store, b"site", b"greenhouse north")
        .expect("note");
    (seed, store, ask, watch)
}

/// A finished mixed workload: the seed, the store, the world, and
/// every beat's pulse.
type Mixed = (AgentSeed, AgentStore, World, Vec<Pulse>);

/// Run the mixed workload, injecting `crash` on beat `crash_beat`.
fn run_mixed(crash_beat: u64, crash: Option<AgentCrash>) -> Mixed {
    let mut world = World::new(&MIXED_SCRIPT);
    let (seed, mut store, ask, _) = mixed_setup(&world);
    let mut pulses = Vec::new();
    for b in 1..=MIXED_BEATS {
        if b == 3 {
            world.inbox.deliver(ask, b"{\"pin\": 4}".to_vec());
        }
        let armed = if b == crash_beat { crash } else { None };
        let pulse = drive_beat(&seed, &mut store, &mut world.view(), armed).expect("beat");
        pulses.push(pulse);
    }
    (seed, store, world, pulses)
}

#[test]
fn a_parked_ask_does_not_block_other_goals_and_resumes_on_input() {
    let (seed, mut store, world, pulses) = run_mixed(0, None);
    let ask = GoalId::new(1).expect("id");
    let watch = GoalId::new(2).expect("id");

    // Beat 1: the Ask goal (priority 9) spawns and parks.
    assert!(matches!(pulses[0].action, BeatAction::Spawned { goal, .. } if goal == ask));
    assert!(pulses[0].parked);
    assert_eq!(pulses[0].settled, None);
    // Beat 2: the parked goal blocks nothing — the watcher runs. The
    // parked goal is overdue (threshold 1 beat) but never abandoned.
    assert!(matches!(pulses[1].action, BeatAction::Spawned { goal, .. } if goal == watch));
    assert_eq!(pulses[1].counts.overdue, 1);
    // Beat 3: input arrived; the obligation outranks the due watcher.
    assert!(matches!(pulses[2].action, BeatAction::Resumed { goal, .. } if goal == ask));
    assert_eq!(
        pulses[2].settled.map(|s| (s.status, s.event)),
        Some((TerminalStatus::Completed, GoalEvent::Satisfy))
    );
    // Beat 4: the watcher's next due beat (2 + 2).
    assert!(matches!(pulses[3].action, BeatAction::Spawned { goal, .. } if goal == watch));
    assert_eq!(pulses[4].action, BeatAction::Idle);

    assert_eq!(world.device.read(Pin::new(4).expect("pin")), Level::High);
    let sup = boot(&seed, &mut store);
    assert_eq!(
        sup.ledger().get(ask).map(|g| g.status),
        Some(GoalStatus::Satisfied)
    );
    // The verified write is remembered, with the operator note intact.
    let write_key = MemoryKey::Observation {
        tool: ToolId::new(2),
        args_digest: Digest::of_bytes(br#"{"pin": 4, "level": "high"}"#),
    };
    assert!(sup.memory().get(&write_key).is_some());
    assert!(sup.memory().get(&MemoryKey::note(b"site")).is_some());
    // The operator note reached the watcher's prompt.
    let watcher_prompt = world.prompt_text(1);
    assert!(watcher_prompt.contains("MEM site = greenhouse north\n"));
}

#[test]
fn operator_control_refuses_a_busy_goal_and_pauses_an_idle_one() {
    let mut world = World::new(&[ASK, READ3, FINISH_READ]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    let mut sup = boot(&seed, &mut store);
    let ask = sup
        .add_goal(&mut store, &spec(b"ask", 9, Cadence::Once, 3))
        .expect("goal");
    let watch = sup
        .add_goal(&mut store, &spec(b"watch", 1, every(1), 10))
        .expect("goal");
    sup.pause(&mut store, watch).expect("pause");
    assert!(beat(&seed, &mut store, &mut world).parked);
    // Parked: retiring would drop the obligation.
    let mut sup = boot(&seed, &mut store);
    assert!(matches!(
        sup.retire(&mut store, ask),
        Err(AgentError::Core(esper_core::Error::GoalBusy))
    ));
    // Paused: nothing runs, then unpause makes it due next beat.
    assert_eq!(beat(&seed, &mut store, &mut world).action, BeatAction::Idle);
    let mut sup = boot(&seed, &mut store);
    sup.unpause(&mut store, watch).expect("unpause");
    let pulse = beat(&seed, &mut store, &mut world);
    assert!(matches!(pulse.action, BeatAction::Spawned { goal, .. } if goal == watch));
    // An illegal control never commits a frame.
    let frames = store.frames().len();
    let mut sup = boot(&seed, &mut store);
    assert!(sup.unpause(&mut store, watch).is_err());
    assert_eq!(store.frames().len(), frames);
}

// ---------------------------------------------------------------------
// Memory rules

#[test]
fn an_unverified_mutation_is_never_remembered() {
    let mut world = World::new(&[WRITE4, FINISH_WRITE]);
    world
        .device
        .set_stuck(Pin::new(4).expect("pin"), Some(Level::Low));
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    let goal = boot(&seed, &mut store)
        .add_goal(&mut store, &spec(b"set pin 4 high", 1, Cadence::Once, 3))
        .expect("goal");
    let pulse = beat(&seed, &mut store, &mut world);
    assert!(pulse.settled.is_some());
    let sup = boot(&seed, &mut store);
    let write_key = MemoryKey::Observation {
        tool: ToolId::new(2),
        args_digest: Digest::of_bytes(br#"{"pin": 4, "level": "high"}"#),
    };
    assert!(
        sup.memory().get(&write_key).is_none(),
        "a write whose read-back failed must not be remembered"
    );
    assert!(sup.memory().get(&MemoryKey::Outcome { goal }).is_some());
}

#[test]
fn secrets_never_reach_memory_the_journal_or_the_prompt() {
    // A fake Stripe-shaped key, assembled at runtime so no
    // secret-shaped literal lives in the repository (push protection
    // scans source; the masker scans shapes).
    let secret = ["sk", "live", "ABCDEFGHIJKLMNOPQRSTUVWX"].join("_");
    let mut world = World::new(&[READ3, FINISH_READ]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    let mut sup = boot(&seed, &mut store);
    let objective = format!("poll pin 3 using {secret}");
    sup.add_goal(&mut store, &spec(objective.as_bytes(), 1, Cadence::Once, 2))
        .expect("goal");
    sup.remember(&mut store, b"token", format!("Bearer {secret}").as_bytes())
        .expect("note");
    beat(&seed, &mut store, &mut world);

    let prompt = world.prompt_text(0);
    assert!(!prompt.contains("ABCDEFGHIJKLMNOP"), "{prompt}");
    assert!(prompt.contains("[redacted:secret#1]"));
    let journal = format!("{:?}", store.frames());
    assert!(
        !journal.contains("ABCDEFGHIJKLMNOP"),
        "secret in the journal"
    );
    let sup = boot(&seed, &mut store);
    for entry in sup.memory().entries() {
        assert!(!contains(entry.value.as_bytes(), b"ABCDEFGHIJKLMNOP"));
    }
}

// ---------------------------------------------------------------------
// Identity

#[test]
fn the_brief_is_bound_into_the_spawned_seed() {
    let mut world = World::new(&[READ3, FINISH_READ]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    boot(&seed, &mut store)
        .add_goal(&mut store, &spec(b"watch pin 3", 1, Cadence::Once, 2))
        .expect("goal");
    let pulse = beat(&seed, &mut store, &mut world);
    let BeatAction::Spawned { run, .. } = pulse.action else {
        panic!("spawned");
    };
    let record = store.run(run).expect("run record");
    let run_seed = record.segments[0].seed;
    let brief = run_seed.brief.expect("brief");
    assert!(brief.as_bytes().starts_with(b"GOAL watch pin 3\n"));
    assert_eq!(
        run_seed.snapshot().brief_digest,
        Some(brief.digest()),
        "the run's journal binding names the brief"
    );
    assert!(world.prompt_text(0).contains("GOAL watch pin 3\n"));
    // The run id encodes the goal and the spawn counter.
    assert_eq!(run.get() >> 48, 1);
}

#[test]
fn a_journal_bound_to_another_agent_fails_closed() {
    let world = World::new(&[]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    boot(&seed, &mut store);
    let other = AgentSeed { id: 2, ..seed };
    assert!(matches!(
        Supervisor::boot(&other, &mut store),
        Err(AgentError::SeedMismatch)
    ));
    let bad = AgentSeed {
        policy: SchedulePolicy {
            max_consecutive_failures: 0,
            ..SchedulePolicy::DEFAULT
        },
        ..seed
    };
    assert!(matches!(
        Supervisor::boot(&bad, &mut AgentStore::new()),
        Err(AgentError::SeedInvalid(_))
    ));
}

// ---------------------------------------------------------------------
// The agent crash matrix (SPEC §21.5)

/// The device's physical history, for comparison.
fn writes(world: &World) -> Vec<WriteRecord> {
    world.device.write_ledger().to_vec()
}

#[test]
fn every_agent_crash_point_on_every_beat_recovers_the_crash_free_result() {
    let (_, baseline_store, baseline_world, baseline_pulses) = run_mixed(0, None);
    let mut fired = 0;
    for point in AgentCrashPoint::all() {
        for crash_beat in 1..=MIXED_BEATS {
            let (_, store, world, pulses) = run_mixed(crash_beat, Some(AgentCrash::Agent(point)));
            let label = format!("{} on beat {crash_beat}", point.name());
            assert_eq!(pulses, baseline_pulses, "pulses differ: {label}");
            assert_eq!(
                store.frames(),
                baseline_store.frames(),
                "journal differs: {label}"
            );
            assert_eq!(writes(&world), writes(&baseline_world), "device: {label}");
            assert_eq!(
                world.model.prompts, baseline_world.model.prompts,
                "the model saw different prompts: {label}"
            );
            fired += 1;
        }
    }
    assert_eq!(fired, 25);
}

#[test]
fn every_run_crash_point_inside_a_supervised_run_recovers_the_crash_free_result() {
    let (_, baseline_store, baseline_world, baseline_pulses) = run_mixed(0, None);
    // Beat 1 parks on Ask; beat 3 resumes, writes, verifies, finishes.
    for crash_beat in [1, 3] {
        for point in CrashPoint::all() {
            let (_, store, world, pulses) = run_mixed(crash_beat, Some(AgentCrash::Run(point)));
            let label = format!("{} on beat {crash_beat}", point.name());
            assert_eq!(pulses, baseline_pulses, "pulses differ: {label}");
            assert_eq!(store.frames(), baseline_store.frames(), "journal: {label}");
            assert_eq!(writes(&world), writes(&baseline_world), "device: {label}");
        }
    }
    // The physical write happened exactly once.
    assert_eq!(writes(&baseline_world).len(), 1);
}

// ---------------------------------------------------------------------
// Checkpoint (SPEC §21.6)

#[test]
fn a_checkpoint_folds_the_journal_and_the_agent_carries_on() {
    let mut world = World::new(&MIXED_SCRIPT);
    let (seed, mut store, ask, watch) = mixed_setup(&world);
    // Beat 1 parks the Ask goal; beat 2 runs the watcher.
    for _ in 0..2 {
        beat(&seed, &mut store, &mut world);
    }
    let before = boot(&seed, &mut store);
    let mut sup = boot(&seed, &mut store);
    let report = sup.checkpoint(&mut store).expect("checkpoint");
    assert_eq!(report.frames_after, 2);
    assert!(report.frames_before > report.frames_after);
    assert_eq!(
        report.runs_pruned, 1,
        "the watcher's settled run is dropped"
    );
    assert_eq!(store.run_ids().count(), 1, "the parked run is kept");

    // A fresh boot from the folded store sees the same agent.
    let after = boot(&seed, &mut store);
    assert_eq!(after.ledger(), before.ledger());
    assert_eq!(after.memory(), before.memory());
    assert_eq!(after.last_beat(), Beat::new(2));

    // The parked run resumes from its kept journal and completes.
    world.inbox.deliver(ask, b"{\"pin\": 4}".to_vec());
    let pulse = beat(&seed, &mut store, &mut world);
    assert_eq!(pulse.beat, Beat::new(3));
    assert_eq!(pulse.settled.map(|s| s.event), Some(GoalEvent::Satisfy));
    // A second checkpoint reaps the satisfied goal; its outcome stays
    // in memory, and its id is never reused.
    let mut sup = boot(&seed, &mut store);
    let report = sup.checkpoint(&mut store).expect("checkpoint");
    assert_eq!(report.goals_reaped, 1);
    let sup = boot(&seed, &mut store);
    assert!(sup.ledger().get(ask).is_none());
    assert!(sup.ledger().get(watch).is_some());
    assert!(
        sup.memory()
            .get(&MemoryKey::Outcome { goal: ask })
            .is_some()
    );
    let mut sup = boot(&seed, &mut store);
    let next = sup
        .add_goal(&mut store, &spec(b"new", 1, Cadence::Once, 1))
        .expect("goal");
    assert_eq!(next.get(), 3);
}

// ---------------------------------------------------------------------
// Rollover inside a supervised run

#[test]
fn a_supervised_run_that_rolls_over_settles_once_with_every_segment_harvested() {
    let script = [
        READ3,
        r#"CALL gpio_pin_read {"pin": 4}"#,
        r#"CALL gpio_pin_read {"pin": 5}"#,
        r#"FINISH {"status": "completed", "summary": "read three pins"}"#,
    ];
    let mut world = World::new(&script);
    let mut seed = agent_seed(&world);
    // A small context budget: the briefed prompts (~1.1 KiB each)
    // cross 80% within two turns.
    seed.template.context_budget_bytes = Some(2_600);
    let mut store = AgentStore::new();
    boot(&seed, &mut store)
        .add_goal(&mut store, &spec(b"read pins 3 to 5", 1, Cadence::Once, 2))
        .expect("goal");
    let pulse = beat(&seed, &mut store, &mut world);
    let settled = pulse.settled.expect("settled");
    assert_eq!(settled.status, TerminalStatus::Completed);
    let record = store.run(settled.run).expect("record");
    assert!(record.segments.len() >= 2, "the run rolled over");
    // Every child carries the parent's brief.
    let brief = record.segments[0].seed.brief;
    assert!(record.segments.iter().all(|s| s.seed.brief == brief));
    let sup = boot(&seed, &mut store);
    for pin in [3, 4, 5] {
        assert!(sup.memory().get(&read_key(pin)).is_some(), "pin {pin}");
    }
}

// ---------------------------------------------------------------------
// The Tokio heartbeat (SPEC §21.7)

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_tokio_heartbeat_paces_consecutive_beats() {
    let mut world = World::new(&[READ3, FINISH_READ, READ3, FINISH_READ]);
    let seed = agent_seed(&world);
    let mut store = AgentStore::new();
    boot(&seed, &mut store)
        .add_goal(&mut store, &spec(b"watch pin 3", 1, every(2), 10))
        .expect("goal");
    let pulses = run_heartbeat(
        &seed,
        &mut store,
        &mut world.view(),
        Duration::from_millis(1),
        4,
    )
    .await
    .expect("heartbeat");
    let beats: Vec<u64> = pulses.iter().map(|p| p.beat.get()).collect();
    assert_eq!(beats, vec![1, 2, 3, 4]);
    let ran: Vec<bool> = pulses
        .iter()
        .map(|p| matches!(p.action, BeatAction::Spawned { .. }))
        .collect();
    assert_eq!(ran, vec![true, false, true, false]);
}

// ---------------------------------------------------------------------
// Power loss across driver calls

/// Run the mixed workload, losing power at `point` on beat
/// `crash_beat` (the store is left as-is) and recovering on the next
/// driver call.
fn run_mixed_power_loss(crash_beat: u64, point: AgentCrashPoint) -> Mixed {
    let mut world = World::new(&MIXED_SCRIPT);
    let (seed, mut store, ask, _) = mixed_setup(&world);
    let mut pulses = Vec::new();
    for b in 1..=MIXED_BEATS {
        if b == 3 {
            world.inbox.deliver(ask, b"{\"pin\": 4}".to_vec());
        }
        if b == crash_beat {
            let fired = halt_beat_at(&seed, &mut store, &mut world.view(), point).expect("halt");
            if !fired || point == AgentCrashPoint::AfterBeatCompleted {
                // The beat finished (an idle beat never reaches the run
                // points; or power went after completion): its pulse is
                // in the journal, and the next call must not run it
                // again — it would start the next beat.
                let pulse = boot(&seed, &mut store).last_pulse().expect("pulse");
                assert_eq!(pulse.beat, Beat::new(b));
                pulses.push(pulse);
                continue;
            }
        }
        let pulse = drive_beat(&seed, &mut store, &mut world.view(), None)
            .unwrap_or_else(|e| panic!("{e} after {} on beat {crash_beat}", point.name()));
        pulses.push(pulse);
    }
    (seed, store, world, pulses)
}

#[test]
fn power_loss_at_every_agent_point_recovers_on_the_next_call() {
    let (_, baseline_store, baseline_world, baseline_pulses) = run_mixed(0, None);
    for point in AgentCrashPoint::all() {
        for crash_beat in 1..=MIXED_BEATS {
            let (_, store, world, pulses) = run_mixed_power_loss(crash_beat, point);
            let label = format!("{} on beat {crash_beat}", point.name());
            assert_eq!(pulses, baseline_pulses, "pulses: {label}");
            assert_eq!(store.frames(), baseline_store.frames(), "journal: {label}");
            assert_eq!(writes(&world), writes(&baseline_world), "device: {label}");
        }
    }
}

/// A torn spawn: power is lost after `RunSpawned` commits but before
/// the run record reaches the store. The run never booted, so recovery
/// rebuilds it from the committed seed, and the beat finishes exactly
/// like the crash-free one.
#[test]
fn a_spawn_whose_run_record_was_lost_recovers_from_the_committed_seed() {
    let (_, baseline_store, baseline_world, baseline_pulses) = run_mixed(0, None);

    let mut world = World::new(&MIXED_SCRIPT);
    let (seed, mut store, ask, _) = mixed_setup(&world);
    assert!(
        halt_beat_at(
            &seed,
            &mut store,
            &mut world.view(),
            AgentCrashPoint::AfterRunScheduled
        )
        .expect("halt")
    );
    let Some(AgentFrame::RunSpawned { seed: run_seed, .. }) = store.frames().last().cloned() else {
        panic!("the spawn frame is the last committed frame");
    };
    assert!(store.lose_run(run_seed.id), "tear the run record away");
    let mut pulses = vec![drive_beat(&seed, &mut store, &mut world.view(), None).expect("beat")];
    assert!(store.run(run_seed.id).is_some(), "rebuilt from the frame");
    for b in 2..=MIXED_BEATS {
        if b == 3 {
            world.inbox.deliver(ask, b"{\"pin\": 4}".to_vec());
        }
        pulses.push(drive_beat(&seed, &mut store, &mut world.view(), None).expect("beat"));
    }
    assert_eq!(pulses, baseline_pulses);
    assert_eq!(store.frames(), baseline_store.frames());
    assert_eq!(writes(&world), writes(&baseline_world));
}
