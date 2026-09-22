//! Runtime invariants, beyond the golden trajectories.
//!
//! These pin down the durability contract: no dispatch without a
//! committed intent, stable effect identity across retries, no budget
//! or capability widening across reboots, the verifier gate before
//! completion, exactly one logical terminal, and `Ask` suspension that
//! survives a reset.

use esper_core::decision::Level;
use esper_core::ids::{Pin, RunId};
use esper_core::state::TerminalStatus;
use esper_runtime::{
    Capabilities, CrashPoint, Direction, FakeDevice, FaultPlan, InferenceSettings, InputPlan,
    Journal, ModelBackend, RunSeed, ScriptedBackend, TraceEvent, drive_run,
};

/// Build the scripted backend for one run.
fn backend(lines: Vec<Vec<u8>>) -> ScriptedBackend {
    ScriptedBackend::new(lines, InferenceSettings::default_settings())
}

/// The seed bound to this run's model bundle (E3): the journal binds
/// the exact model that must produce the run.
fn backend_seed(backend: &ScriptedBackend) -> RunSeed {
    RunSeed {
        model_bundle: backend.bundle_id().0,
        ..RunSeed::default_slice()
    }
}

/// Build a default seed with this run id, bound to the backend.
fn seed(backend: &ScriptedBackend, id: u64) -> RunSeed {
    RunSeed {
        id: RunId::new(id),
        ..backend_seed(backend)
    }
}

/// A capability set granting only pin 5 for writing; every other
/// grant stays at the default.
const fn write_only_pin_5() -> Capabilities {
    Capabilities {
        write_pins: [5, 0, 0, 0, 0, 0, 0, 0],
        write_count: 1,
        ..RunSeed::default_slice().capabilities
    }
}

fn lines(script: &[&str]) -> Vec<Vec<u8>> {
    script.iter().map(|line| line.as_bytes().to_vec()).collect()
}

fn pin(n: u8) -> Pin {
    Pin::new(n).expect("bad pin in test")
}

/// No dispatch without a committed intent: crashing before the intent
/// commits leaves the device untouched, and the redriven run issues
/// exactly one intent and one physical write.
#[test]
fn no_dispatch_without_committed_intent() {
    let mut backend = backend(lines(&[
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}",
    ]));
    let seed = seed(&backend, 201);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        Some(CrashPoint::ToolIntent),
    )
    .expect("run failed");
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.tool_requests(), 1);
    assert_eq!(device.physical_writes(), 1);
    assert_eq!(device.read(pin(4)), Level::High);
}

/// Effect identity is stable across a transient retry: one request,
/// two observations, all under sequence 0.
#[test]
fn effect_identity_stable_across_retry() {
    let mut backend = backend(lines(&[
        "CALL gpio_pin_read {\"pin\": 4}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 is low\"}",
    ]));
    let seed = seed(&backend, 202);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    faults.fail_transient(pin(4), 1);
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    )
    .expect("run failed");
    let mut request_seqs = Vec::new();
    let mut observation_seqs = Vec::new();
    for event in trace.events() {
        match event {
            TraceEvent::ToolRequest { seq, .. } => request_seqs.push(*seq),
            TraceEvent::ToolObservation { seq, .. } => observation_seqs.push(*seq),
            _ => {}
        }
    }
    assert_eq!(request_seqs, vec![0]);
    assert_eq!(observation_seqs, vec![0, 0]);
}

/// Budgets never widen across reboots: crashing at every boundary of a
/// mutating run still leaves exactly the golden remaining budgets.
#[test]
fn budgets_never_widen_across_reboots() {
    let script = &[
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}",
    ];
    for (i, point) in CrashPoint::all().into_iter().enumerate() {
        // `cp_await_input` never fires without an Ask; the run still
        // completes with identical budgets.
        let mut backend = backend(lines(script));
        let seed = seed(&backend, 210 + i as u64);
        let mut device = FakeDevice::new();
        let mut faults = FaultPlan::new();
        let mut inputs = InputPlan::new(Vec::new());
        let mut journal = Journal::new();
        let trace = drive_run(
            &seed,
            &mut journal,
            &mut backend,
            &mut device,
            &mut faults,
            &mut inputs,
            Some(point),
        )
        .expect("run failed");
        assert_eq!(
            trace.terminal_status(),
            Some(TerminalStatus::Completed),
            "crash at {}",
            point.name()
        );
        assert_eq!(trace.turns_remaining(), 7, "crash at {}", point.name());
        assert_eq!(trace.mutations_remaining(), 3, "crash at {}", point.name());
    }
}

/// Permissions never widen across reboots: a denied write stays denied
/// after a crash at the authorization boundary, and the device is
/// untouched.
#[test]
fn permissions_never_widen_across_reboots() {
    let mut backend = backend(lines(&[
        "CALL gpio_pin_write {\"pin\": 0, \"level\": \"high\"}",
    ]));
    let seed = RunSeed {
        capabilities: write_only_pin_5(),
        ..seed(&backend, 220)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        Some(CrashPoint::BeforeAuthorize),
    )
    .expect("run failed");
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(device.physical_writes(), 0);
}

/// A mutation cannot complete without a passing verifier: with the
/// actuator stuck, the run ends `Stuck` — never `Completed` — even
/// though every tool call reported success.
#[test]
fn mutation_cannot_complete_without_passing_verifier() {
    let mut backend = backend(lines(&[
        "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
        "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
        "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"unreached\"}",
    ]));
    let seed = seed(&backend, 221);
    let mut device = FakeDevice::new();
    device.set_stuck(pin(5), Some(Level::Low));
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    )
    .expect("run failed");
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Stuck));
    // The fourth line was never consumed: the run ended at the third attempt.
    assert_eq!(backend.lines_consumed(), 3);
}

/// Exactly one logical terminal: lines after `FINISH` are never
/// consumed and only one terminal event exists.
#[test]
fn one_logical_terminal_result() {
    let mut backend = backend(lines(&[
        "FINISH {\"status\": \"completed\", \"summary\": \"done\"}",
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
    ]));
    let seed = seed(&backend, 222);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        Some(CrashPoint::AfterTerminalCommit),
    )
    .expect("run failed");
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(backend.lines_consumed(), 1);
    let terminals = trace
        .events()
        .iter()
        .filter(|event| matches!(event, TraceEvent::Terminal { .. }))
        .count();
    assert_eq!(terminals, 1);
}

/// `Ask` stays suspended across a reset when no input has arrived: the
/// trace reports suspension, not a terminal, and a later delivery
/// resumes the same run.
#[test]
fn ask_remains_suspended_across_reset() {
    let mut backend = backend(lines(&[
        "ASK {\"prompt\": \"which pin?\", \"schema\": 1}",
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high per operator\"}",
    ]));
    let seed = seed(&backend, 223);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        Some(CrashPoint::AwaitInput),
    )
    .expect("run failed");
    assert!(trace.suspended());
    assert_eq!(trace.terminal_status(), None);

    // A later boot with the delivery resumes and completes. The journal
    // is the durable state: the same journal crosses both boots, the
    // way flash would carry it.
    let mut inputs = InputPlan::new(vec![b"{\"pin\": 4}".to_vec()]);
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    )
    .expect("run failed");
    assert!(!trace.suspended());
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.summary(), Some(b"pin 4 high per operator".as_slice()));
}

/// A write to a pin the device reports as input is denied after
/// capability authorization, without touching hardware.
#[test]
fn device_direction_denied_after_capability_check() {
    let mut backend = backend(lines(&[
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
    ]));
    let seed = seed(&backend, 224);
    let mut device = FakeDevice::new();
    device.set_direction(pin(4), Direction::Input);
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    )
    .expect("run failed");
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(trace.reason(), Some(b"pin_direction_denied".as_slice()));
    assert_eq!(device.physical_writes(), 0);
}
