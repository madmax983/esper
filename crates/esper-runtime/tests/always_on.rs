//! E4+aon: the always-on supervisor end to end (SPEC §21).
//!
//! The supervisor under test is the `no_std` workflow the device runs
//! ([`supervise`]); only its surroundings are doubles: the in-memory
//! Waymaker journal, the host dispatcher driving real goal runs through
//! the real engine, the scripted model, the fake device. The crash
//! matrix resets at every Waymaker boundary kind — intent committed,
//! effect done with its outcome lost, outcome committed, deadline
//! armed — on every occurrence, including inside checkpoint stores and
//! loads across `continue_as_new`, and demands the crash-free result.

use std::mem::size_of;

use esper_core::beat::{Beat, BeatSpan, SchedulePolicy};
use esper_core::checkpoint::AgentCheckpoint;
use esper_core::decision::Level;
use esper_core::goal::{Cadence, Envelope, GoalEvent, GoalId, GoalStatus};
use esper_core::ids::{Digest, Pin, ToolId};
use esper_core::memory::MemoryKey;
use esper_core::registry::Capabilities;
use esper_core::state::TerminalStatus;
use esper_runtime::agent::host::{
    CrashAt, HostAgent, MemJournal, PulseLog, Record, Signals, drive,
};
use esper_runtime::agent::wire::{
    Command, MAX_RUN_REQUEST_BYTES, MAX_SENSE_BYTES, PAGE_RECORD_MAX,
};
use esper_runtime::{
    AgentError, AgentSeed, AgentState, BackendError, BeatAction, BundleId, CrashPoint, Direction,
    FakeDevice, FaultPlan, Heartbeat, InferenceSettings, ModelBackend, Pulse, RunSeed,
    ScriptedBackend, TokenUsage, WriteRecord,
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

fn recorder(script: &[&str]) -> Recorder {
    Recorder {
        inner: ScriptedBackend::new(
            script.iter().map(|line| line.as_bytes().to_vec()).collect(),
            InferenceSettings::briefed_settings(),
        ),
        prompts: Vec::new(),
    }
}

/// Everything one simulated agent lifetime produced.
struct Outcome {
    state: AgentState,
    pulses: Vec<Pulse>,
    writes: Vec<WriteRecord>,
    prompts: Vec<Vec<u8>>,
    commits: [usize; 4],
    continued: usize,
    records: Vec<Record>,
    errors: Vec<String>,
    spawned: Vec<RunSeed>,
    segments: Vec<usize>,
    pin4: Level,
}

/// What a scenario injects.
#[derive(Clone, Copy, Default)]
struct Faults {
    crash: Option<CrashAt>,
    run_crash: Option<CrashPoint>,
    stuck_pin4: bool,
}

/// Run `seed` for `target` beats over `script`, with operator input
/// scheduled by `plan`.
fn simulate(
    seed: &AgentSeed,
    script: &[&str],
    plan: &dyn Fn(&mut HostAgent<'_>),
    target: u64,
    tick: u64,
    faults: Faults,
) -> Result<Outcome, String> {
    let signals = Signals::new();
    let mut model = recorder(script);
    let mut device = FakeDevice::new();
    for pin in 0..8u8 {
        device.set_direction(Pin::new(pin).expect("pin"), Direction::Output);
    }
    if faults.stuck_pin4 {
        device.set_stuck(Pin::new(4).expect("pin"), Some(Level::Low));
    }
    let mut device_faults = FaultPlan::new();
    let mut start = [0u8; 64];
    let n = seed.start().encode(&mut start).expect("start");
    let mut journal = MemJournal::new(&signals, &start[..n]);
    if let Some(crash) = faults.crash {
        journal.crash_at(crash);
    }
    let mut state = AgentState::new();
    let log = PulseLog::new();
    let (errors, spawned, segments) = {
        let mut agent = HostAgent::new(&signals, &mut model, &mut device, &mut device_faults);
        plan(&mut agent);
        if let Some(point) = faults.run_crash {
            agent.crash_next_run(point);
        }
        drive(
            seed,
            &mut journal,
            &mut agent,
            &mut state,
            &log,
            target,
            tick,
        )
        .map_err(|e| format!("{e:?}"))?;
        let mut spawned = Vec::new();
        let mut segments = Vec::new();
        for goal in 1..=8u64 {
            for spawn in 0..64u64 {
                let run = esper_core::ids::RunId::new((goal << 48) | (spawn << 16));
                if let Some(record) = agent.run(run) {
                    spawned.push(record.segments[0].0);
                    segments.push(record.segments.len());
                }
            }
        }
        (agent.errors().to_vec(), spawned, segments)
    };
    Ok(Outcome {
        state,
        pulses: log.pulses(),
        writes: device.write_ledger().to_vec(),
        prompts: model.prompts,
        commits: journal.commits(),
        continued: journal.continued(),
        records: journal.records().to_vec(),
        errors,
        spawned,
        segments,
        pin4: device.read(Pin::new(4).expect("pin")),
    })
}

fn agent_seed() -> AgentSeed {
    let mut seed = AgentSeed::default_slice();
    seed.template.model_bundle = recorder(&[]).bundle_id().0;
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

fn add_goal(objective: &[u8], priority: u8, cadence: Cadence, runs: u16) -> Command {
    Command::add_goal(
        objective,
        priority,
        cadence,
        envelope(runs),
        AgentSeed::default_slice().template.capabilities,
    )
    .expect("command")
}

const fn goal(id: u16) -> GoalId {
    GoalId::new(id).expect("goal id")
}

fn status(out: &Outcome, id: u16) -> GoalStatus {
    out.state
        .checkpoint
        .ledger
        .get(goal(id))
        .map(|g| g.status)
        .expect("goal")
}

fn text(prompt: &[u8]) -> String {
    String::from_utf8(prompt.to_vec()).expect("ASCII prompt")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

fn read_key(pin: u8) -> MemoryKey {
    MemoryKey::Observation {
        tool: ToolId::new(1),
        args_digest: Digest::of_bytes(format!("{{\"pin\": {pin}}}").as_bytes()),
    }
}

const READ3: &str = r#"CALL gpio_pin_read {"pin": 3}"#;
const FINISH_READ: &str = r#"FINISH {"status": "completed", "summary": "pin 3 low"}"#;
const ASK: &str = r#"ASK {"prompt": "Which pin should be set high?", "schema": 1}"#;
const WRITE4: &str = r#"CALL gpio_pin_write {"pin": 4, "level": "high"}"#;
const FINISH_WRITE: &str = r#"FINISH {"status": "completed", "summary": "pin 4 high"}"#;

// ---------------------------------------------------------------------
// Firmware posture

// Pinned at compile time in `wire`; restated here as a readable
// contract with the 512-byte Waymaker scratch page.
const _: () = assert!(PAGE_RECORD_MAX == 480);
const _: () = assert!(MAX_SENSE_BYTES <= PAGE_RECORD_MAX);
const _: () = assert!(MAX_RUN_REQUEST_BYTES <= PAGE_RECORD_MAX);

/// RAM pin: the caller-owned state, and the supervisor future itself.
/// Growth here is a deliberate, reviewed diff (SPEC §21.8).
#[test]
fn supervisor_ram_is_bounded_and_pinned() {
    let state_bytes = size_of::<AgentState>();
    let checkpoint = size_of::<AgentCheckpoint>();
    assert!(
        state_bytes <= checkpoint + 64,
        "state {state_bytes} vs checkpoint {checkpoint}"
    );
    assert!(state_bytes <= 10 * 1024, "AgentState is {state_bytes} B");

    let signals = Signals::new();
    let mut model = recorder(&[]);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut journal = MemJournal::new(&signals, &[]);
    let mut agent = HostAgent::new(&signals, &mut model, &mut device, &mut faults);
    let mut out = [0u8; PAGE_RECORD_MAX];
    let mut ctx = waymaker_embassy::Ctx::new(&mut journal, &mut agent, &mut out);
    let seed = agent_seed();
    let mut alarm = waymaker_embassy::NoAlarm;
    let mut state = AgentState::new();
    let mut sink = esper_runtime::NoPulse;
    let future = esper_runtime::supervise(&mut ctx, &seed, &[], &mut alarm, &mut state, &mut sink);
    let future_bytes = core::mem::size_of_val(&future);
    println!(
        "RAM: AgentState {state_bytes} B (checkpoint {checkpoint} B), supervise future {future_bytes} B"
    );
    assert!(
        future_bytes <= 4096,
        "the supervise future is {future_bytes} B"
    );
}

// ---------------------------------------------------------------------
// Heartbeat + goals

#[test]
fn a_standing_goal_runs_on_cadence_and_its_memory_reaches_the_next_prompt() {
    let out = simulate(
        &agent_seed(),
        &[READ3, FINISH_READ, READ3, FINISH_READ],
        &|agent| agent.command_at(1, add_goal(b"watch pin 3", 1, every(3), 10)),
        4,
        1,
        Faults::default(),
    )
    .expect("simulate");
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let actions: Vec<bool> = out
        .pulses
        .iter()
        .map(|p| matches!(p.action, BeatAction::Spawned { .. }))
        .collect();
    assert_eq!(actions, vec![true, false, false, true], "cadence 3");
    let first = out.pulses[0].settled.expect("settled");
    assert_eq!(first.status, TerminalStatus::Completed);
    assert_eq!(first.event, GoalEvent::Recur);
    assert_eq!(first.harvested, 2, "one observation and one outcome");

    let first_prompt = text(&out.prompts[0]);
    assert!(first_prompt.contains("GOAL watch pin 3\n"));
    assert!(!first_prompt.contains("MEM "));
    let second_prompt = text(&out.prompts[2]);
    assert!(
        second_prompt.contains("MEM goal#1 = completed pin 3 low\n"),
        "{second_prompt}"
    );
    assert!(second_prompt.contains(r#"MEM gpio_pin_read {"pin": 3} = "#));

    let remembered = out
        .state
        .checkpoint
        .memory
        .get(&read_key(3))
        .expect("entry");
    assert_eq!(remembered.provenance.goal, Some(goal(1)));
    assert_eq!(
        remembered.provenance.beat,
        Beat::new(4),
        "the supervisor states the beat"
    );
}

#[test]
fn a_once_goal_is_satisfied_and_never_runs_again() {
    let out = simulate(
        &agent_seed(),
        &[READ3, FINISH_READ],
        &|agent| agent.command_at(1, add_goal(b"read pin 3 once", 1, Cadence::Once, 5)),
        6,
        1,
        Faults::default(),
    )
    .expect("simulate");
    assert_eq!(
        out.pulses[0].settled.map(|s| s.event),
        Some(GoalEvent::Satisfy)
    );
    assert!(out.pulses[1..].iter().all(|p| p.action == BeatAction::Idle));
    assert_eq!(status(&out, 1), GoalStatus::Satisfied);
}

#[test]
fn a_denied_run_fails_its_goal_at_once_under_least_privilege() {
    let read_only = Capabilities {
        read_pins: [3, 0, 0, 0, 0, 0, 0, 0],
        read_count: 1,
        ..Capabilities::empty()
    };
    let out = simulate(
        &agent_seed(),
        &[r#"CALL gpio_pin_write {"pin": 3, "level": "high"}"#],
        &|agent| {
            let command = Command::add_goal(b"read pin 3", 1, every(1), envelope(10), read_only)
                .expect("command");
            agent.command_at(1, command);
        },
        2,
        1,
        Faults::default(),
    )
    .expect("simulate");
    let settled = out.pulses[0].settled.expect("settled");
    assert_eq!(settled.status, TerminalStatus::Denied);
    assert_eq!(settled.event, GoalEvent::Fail);
    assert!(out.writes.is_empty(), "never dispatched");
    assert_eq!(status(&out, 1), GoalStatus::Failed);
    assert_eq!(
        out.spawned[0].capabilities, read_only,
        "the goal's grant, not the agent's"
    );
}

#[test]
fn retryable_failures_back_off_then_fail_the_goal() {
    let out = simulate(
        &agent_seed(),
        &["nonsense"; 12],
        &|agent| agent.command_at(1, add_goal(b"flaky", 1, every(1), 10)),
        8,
        1,
        Faults::default(),
    )
    .expect("simulate");
    let ran_at: Vec<u64> = out
        .pulses
        .iter()
        .filter(|p| p.settled.is_some())
        .map(|p| p.beat.get())
        .collect();
    assert_eq!(ran_at, vec![1, 2, 4], "runs at 1, then +1, then +2");
    assert!(
        out.pulses
            .iter()
            .filter_map(|p| p.settled)
            .all(|s| s.status == TerminalStatus::ModelInvalid)
    );
    assert_eq!(status(&out, 1), GoalStatus::Failed);
}

#[test]
fn the_envelope_exhausts_after_its_runs_and_refunds_unspent_units() {
    let out = simulate(
        &agent_seed(),
        &[READ3, FINISH_READ, READ3, FINISH_READ],
        &|agent| agent.command_at(1, add_goal(b"twice", 1, every(1), 2)),
        3,
        1,
        Faults::default(),
    )
    .expect("simulate");
    let events: Vec<_> = out
        .pulses
        .iter()
        .map(|p| p.settled.map(|s| s.event))
        .collect();
    assert_eq!(
        events,
        vec![Some(GoalEvent::Recur), Some(GoalEvent::Exhaust), None]
    );
    let g = out.state.checkpoint.ledger.get(goal(1)).expect("goal");
    assert_eq!(g.status, GoalStatus::Exhausted);
    assert_eq!(g.remaining.turns, 120 - 4, "each run spent 2 of 6 turns");
    assert_eq!(
        g.remaining.mutations, 40,
        "read-only runs refund their mutation"
    );
    g.check_envelope().expect("never widened");
}

// ---------------------------------------------------------------------
// The mixed workload: parking never blocks, obligations survive

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

fn mixed_seed(checkpoint_every: u32) -> AgentSeed {
    let mut seed = agent_seed();
    seed.policy = SchedulePolicy {
        overdue_after: BeatSpan::ONE,
        ..SchedulePolicy::DEFAULT
    };
    seed.checkpoint_every = BeatSpan::new(checkpoint_every).expect("span");
    seed
}

fn mixed_plan(agent: &mut HostAgent<'_>) {
    agent.command_at(1, add_goal(b"set pin 4 as told", 9, Cadence::Once, 3));
    agent.command_at(1, add_goal(b"watch pin 3", 1, every(2), 10));
    agent.command_at(
        1,
        Command::remember(b"site", b"greenhouse north").expect("note"),
    );
    agent.deliver_at(3, goal(1), br#"{"pin": 4}"#);
}

fn mixed(checkpoint_every: u32, faults: Faults) -> Result<Outcome, String> {
    simulate(
        &mixed_seed(checkpoint_every),
        &MIXED_SCRIPT,
        &mixed_plan,
        MIXED_BEATS,
        1,
        faults,
    )
}

#[test]
fn a_parked_ask_does_not_block_other_goals_and_resumes_on_input() {
    let out = mixed(64, Faults::default()).expect("simulate");
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let p = &out.pulses;
    // Beat 1: the Ask goal (priority 9) spawns and parks.
    assert!(matches!(p[0].action, BeatAction::Spawned { goal: g, .. } if g == goal(1)));
    assert!(p[0].parked);
    // Beat 2: the parked goal blocks nothing; it is overdue, never dropped.
    assert!(matches!(p[1].action, BeatAction::Spawned { goal: g, .. } if g == goal(2)));
    assert_eq!(p[1].counts.overdue, 1);
    // Beat 3: input arrived; the obligation outranks the due watcher.
    assert!(matches!(p[2].action, BeatAction::Resumed { goal: g, .. } if g == goal(1)));
    assert_eq!(
        p[2].settled.map(|s| (s.status, s.event)),
        Some((TerminalStatus::Completed, GoalEvent::Satisfy))
    );
    // Beat 4: the watcher's next due beat; beat 5: idle.
    assert!(matches!(p[3].action, BeatAction::Spawned { goal: g, .. } if g == goal(2)));
    assert_eq!(p[4].action, BeatAction::Idle);

    assert_eq!(out.pin4, Level::High);
    assert_eq!(out.writes.len(), 1);
    let write_key = MemoryKey::Observation {
        tool: ToolId::new(2),
        args_digest: Digest::of_bytes(br#"{"pin": 4, "level": "high"}"#),
    };
    assert!(
        out.state.checkpoint.memory.get(&write_key).is_some(),
        "verified write remembered"
    );
    assert!(
        out.state
            .checkpoint
            .memory
            .get(&MemoryKey::note(b"site"))
            .is_some()
    );
    assert!(text(&out.prompts[1]).contains("MEM site = greenhouse north\n"));
}

#[test]
fn operator_control_refuses_a_busy_goal_and_pauses_an_idle_one() {
    let out = simulate(
        &agent_seed(),
        &[ASK, READ3, FINISH_READ],
        &|agent| {
            agent.command_at(1, add_goal(b"ask", 9, Cadence::Once, 3));
            agent.command_at(1, add_goal(b"watch", 1, every(1), 10));
            agent.command_at(
                1,
                Command::Control {
                    goal: goal(2),
                    event: GoalEvent::Pause,
                },
            );
            // Parked at beat 1: retiring would drop the obligation.
            agent.command_at(
                2,
                Command::Control {
                    goal: goal(1),
                    event: GoalEvent::Retire,
                },
            );
            agent.command_at(
                3,
                Command::Control {
                    goal: goal(2),
                    event: GoalEvent::Unpause,
                },
            );
            // Already active: an illegal unpause changes nothing.
            agent.command_at(
                4,
                Command::Control {
                    goal: goal(2),
                    event: GoalEvent::Unpause,
                },
            );
        },
        4,
        1,
        Faults::default(),
    )
    .expect("simulate");
    let p = &out.pulses;
    assert!(p[0].parked);
    assert_eq!(
        (p[1].rejected, p[1].action),
        (1, BeatAction::Idle),
        "busy goal; watcher paused"
    );
    assert!(matches!(p[2].action, BeatAction::Spawned { goal: g, .. } if g == goal(2)));
    assert_eq!(p[3].rejected, 1);
    assert_eq!(status(&out, 1), GoalStatus::Waiting);
}

// ---------------------------------------------------------------------
// Memory rules

#[test]
fn an_unverified_mutation_is_never_remembered() {
    let out = simulate(
        &agent_seed(),
        &[WRITE4, FINISH_WRITE],
        &|agent| agent.command_at(1, add_goal(b"set pin 4 high", 1, Cadence::Once, 3)),
        1,
        1,
        Faults {
            stuck_pin4: true,
            ..Faults::default()
        },
    )
    .expect("simulate");
    assert!(out.pulses[0].settled.is_some());
    let write_key = MemoryKey::Observation {
        tool: ToolId::new(2),
        args_digest: Digest::of_bytes(br#"{"pin": 4, "level": "high"}"#),
    };
    assert!(out.state.checkpoint.memory.get(&write_key).is_none());
    assert!(
        out.state
            .checkpoint
            .memory
            .get(&MemoryKey::Outcome { goal: goal(1) })
            .is_some()
    );
}

/// Mask before durability, at the device boundary: operator text is
/// masked before the SENSE answer becomes a journal record.
#[test]
fn secrets_never_reach_the_journal_memory_or_the_prompt() {
    let secret = ["sk", "live", "ABCDEFGHIJKLMNOPQRSTUVWX"].join("_");
    let objective = format!("poll pin 3 using {secret}");
    let token = format!("Bearer {secret}");
    let out = simulate(
        &agent_seed(),
        &[READ3, FINISH_READ],
        &|agent| {
            agent.command_at(1, add_goal(objective.as_bytes(), 1, Cadence::Once, 2));
            agent.command_at(
                1,
                Command::remember(b"token", token.as_bytes()).expect("note"),
            );
        },
        1,
        1,
        Faults::default(),
    )
    .expect("simulate");
    let prompt = text(&out.prompts[0]);
    assert!(!prompt.contains("ABCDEFGHIJKLMNOP"), "{prompt}");
    assert!(prompt.contains("[redacted:secret#1]"));
    let journal = format!("{:?}", out.records);
    assert!(
        !journal.contains("ABCDEFGHIJKLMNOP"),
        "secret in a journal record"
    );
    assert!(!contains(
        journal.as_bytes(),
        b"65, 66, 67, 68, 69, 70, 71, 72"
    ));
    for entry in out.state.checkpoint.memory.entries() {
        assert!(!contains(entry.value.as_bytes(), b"ABCDEFGHIJKLMNOP"));
    }
}

// ---------------------------------------------------------------------
// Identity

#[test]
fn the_brief_travels_in_the_committed_spawn_and_is_bound_into_the_seed() {
    let out = simulate(
        &agent_seed(),
        &[READ3, FINISH_READ],
        &|agent| agent.command_at(1, add_goal(b"watch pin 3", 1, Cadence::Once, 2)),
        1,
        1,
        Faults::default(),
    )
    .expect("simulate");
    let seed = out.spawned[0];
    let brief = seed.brief.expect("brief");
    assert!(brief.as_bytes().starts_with(b"GOAL watch pin 3\n"));
    assert_eq!(seed.snapshot().brief_digest, Some(brief.digest()));
    assert_eq!(seed.id.get() >> 48, 1, "run id encodes the goal");
    // The RUN intent record carries the brief bytes themselves.
    let spawn = out
        .records
        .iter()
        .find_map(|r| match r {
            Record::Scheduled {
                kind: 0x0101,
                input,
                ..
            } => Some(input.clone()),
            _ => None,
        })
        .expect("RUN intent");
    assert!(contains(&spawn, brief.as_bytes()));
}

#[test]
fn a_journal_bound_to_another_agent_fails_closed() {
    let other = AgentSeed {
        id: 2,
        ..agent_seed()
    };
    let signals = Signals::new();
    let mut model = recorder(&[]);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut start = [0u8; 64];
    let n = other.start().encode(&mut start).expect("start");
    let mut journal = MemJournal::new(&signals, &start[..n]);
    let mut agent = HostAgent::new(&signals, &mut model, &mut device, &mut faults);
    let mut state = AgentState::new();
    let log = PulseLog::new();
    let error = drive(
        &agent_seed(),
        &mut journal,
        &mut agent,
        &mut state,
        &log,
        1,
        1,
    )
    .expect_err("mismatch");
    assert!(
        matches!(
            error,
            esper_runtime::agent::host::DriveError::Agent(AgentError::SeedMismatch)
        ),
        "{error:?}"
    );
}

// ---------------------------------------------------------------------
// Heartbeat clocks

/// A persistent-clock heartbeat: deadlines survive reboots, and missed
/// beats are not owed — a late tick yields one beat, never a burst.
#[test]
fn a_persistent_heartbeat_does_not_owe_missed_beats() {
    let mut seed = agent_seed();
    seed.heartbeat = Heartbeat::Persistent { period: 10 };
    // Each stop advances the clocks by 35: three periods and a half late.
    let out = simulate(&seed, &[], &|_| {}, 3, 35, Faults::default()).expect("simulate");
    let beats: Vec<u64> = out.pulses.iter().map(|p| p.beat.get()).collect();
    assert_eq!(beats, vec![1, 2, 3]);
    // Beat 1 at t=0 → 10; beat 2 at t=35 → 45; beat 3 at t=70 → 80.
    assert_eq!(out.state.checkpoint.next_deadline, 80);
}

// ---------------------------------------------------------------------
// Checkpoint and continue_as_new (SPEC §21.6)

#[test]
fn continue_as_new_bounds_history_and_changes_nothing_observable() {
    let long = mixed(64, Faults::default()).expect("no checkpoint");
    let short = mixed(2, Faults::default()).expect("checkpoint every 2 beats");
    assert_eq!(long.continued, 0);
    assert_eq!(short.continued, 2, "continued after beats 2 and 4");
    assert!(
        short.records.len() < long.records.len(),
        "history was folded"
    );
    assert_eq!(short.state.checkpoint, long.state.checkpoint);
    assert_eq!(short.writes, long.writes);
    assert_eq!(short.prompts, long.prompts);
    assert_eq!(short.pulses, long.pulses);
}

// ---------------------------------------------------------------------
// The crash matrix (SPEC §21.5)

fn assert_same(label: &str, got: &Outcome, want: &Outcome) {
    assert!(got.errors.is_empty(), "{label}: {:?}", got.errors);
    assert_eq!(
        got.state.checkpoint, want.state.checkpoint,
        "state: {label}"
    );
    assert_eq!(got.pulses, want.pulses, "pulses: {label}");
    assert_eq!(got.writes, want.writes, "device: {label}");
    assert_eq!(
        got.prompts, want.prompts,
        "the model saw different prompts: {label}"
    );
}

/// A reset at every Waymaker boundary kind, at every occurrence, with
/// and without `continue_as_new` in the run: the crash-free result.
#[test]
fn a_reset_at_every_boundary_recovers_the_crash_free_result() {
    let mut cases = 0;
    for checkpoint_every in [64, 2] {
        let baseline = mixed(checkpoint_every, Faults::default()).expect("baseline");
        let commits = baseline.commits;
        assert!(commits.iter().all(|n| *n > 0), "{commits:?}");
        for (kind, count) in commits.into_iter().enumerate() {
            for n in 1..=count {
                let crash = match kind {
                    0 => CrashAt::AfterSchedule(n),
                    1 => CrashAt::BeforeResolve(n),
                    2 => CrashAt::AfterResolve(n),
                    _ => CrashAt::AfterTimerArmed(n),
                };
                let label = format!("{crash:?} (checkpoint every {checkpoint_every})");
                let got = mixed(
                    checkpoint_every,
                    Faults {
                        crash: Some(crash),
                        ..Faults::default()
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_same(&label, &got, &baseline);
                cases += 1;
            }
        }
    }
    println!("crash cases: {cases}");
    assert!(cases > 60, "{cases} crash cases");
}

/// Every run-level crash point inside the first supervised run: the
/// engine recovers inside the RUN activity, invisibly to the agent.
#[test]
fn a_run_crash_inside_a_supervised_run_is_invisible_to_the_agent() {
    let baseline = mixed(64, Faults::default()).expect("baseline");
    for point in CrashPoint::all() {
        let got = mixed(
            64,
            Faults {
                run_crash: Some(point),
                ..Faults::default()
            },
        )
        .unwrap_or_else(|e| panic!("{}: {e}", point.name()));
        assert_same(point.name(), &got, &baseline);
    }
    assert_eq!(baseline.writes.len(), 1, "the physical write happened once");
}

// ---------------------------------------------------------------------
// Rollover inside a supervised run

#[test]
fn a_supervised_run_that_rolls_over_settles_once_with_every_segment_harvested() {
    let mut seed = agent_seed();
    seed.template.context_budget_bytes = Some(2_600);
    let out = simulate(
        &seed,
        &[
            READ3,
            r#"CALL gpio_pin_read {"pin": 4}"#,
            r#"CALL gpio_pin_read {"pin": 5}"#,
            r#"FINISH {"status": "completed", "summary": "read three pins"}"#,
        ],
        &|agent| agent.command_at(1, add_goal(b"read pins 3 to 5", 1, Cadence::Once, 2)),
        1,
        1,
        Faults::default(),
    )
    .expect("simulate");
    assert!(out.errors.is_empty(), "{:?}", out.errors);
    assert_eq!(
        out.pulses[0].settled.map(|s| s.status),
        Some(TerminalStatus::Completed)
    );
    assert!(out.segments[0] >= 2, "the run rolled over");
    for pin in [3, 4, 5] {
        assert!(
            out.state.checkpoint.memory.get(&read_key(pin)).is_some(),
            "pin {pin}"
        );
    }
}
