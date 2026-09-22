//! Golden trajectories a–h from `spec/trajectories/`.
//!
//! Each test drives one scripted run through `drive_run` and asserts
//! the exact terminal status, reason, summary, and remaining budgets
//! the fixture file specifies. These tests were written before the
//! engine existed (RED); they failed to compile until the driver API
//! landed.

use esper_core::ids::{Pin, RunId};
use esper_core::state::TerminalStatus;
use esper_runtime::{
    Capabilities, CrashPoint, FakeDevice, FaultPlan, InferenceSettings, InputPlan, Journal,
    ModelBackend, RunSeed, ScriptedBackend, drive_run,
};

/// Build the scripted backend for one run.
fn backend(script: Vec<&str>) -> ScriptedBackend {
    ScriptedBackend::new(
        script
            .into_iter()
            .map(|line| line.as_bytes().to_vec())
            .collect(),
        InferenceSettings::default_settings(),
    )
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

/// Drive a run with no crash injection.
fn drive(
    seed: &RunSeed,
    journal: &mut Journal,
    backend: &mut ScriptedBackend,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    crash: Option<CrashPoint>,
) -> esper_runtime::RunTrace {
    drive_run(seed, journal, backend, device, faults, inputs, crash).expect("run failed")
}

fn pin(n: u8) -> Pin {
    Pin::new(n).expect("bad pin in test")
}

/// a: read pin 4, write pin 4 high, verify, finish — crash once at
/// `cp_tool_intent`.
#[test]
fn trajectory_a_success_read_write_verify_finish() {
    let mut backend = backend(vec![
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}",
    ]);
    let seed = seed(&backend, 1);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        Some(CrashPoint::ToolIntent),
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.reason(), Some(b"model_finished".as_slice()));
    assert_eq!(trace.summary(), Some(b"pin 4 high".as_slice()));
    assert_eq!(trace.turns_remaining(), 7);
    assert_eq!(trace.mutations_remaining(), 3);
    assert_eq!(device.physical_writes(), 1);
    assert_eq!(device.read(pin(4)), esper_core::decision::Level::High);
}

/// b: three invalid lines — repair twice, then `ModelInvalid`.
#[test]
fn trajectory_b_invalid_output_repair_then_modelinvalid() {
    let mut backend = backend(vec![
        "do a flip",
        "CALL gpio_pin_read {\"pin\": 99}",
        "CALLL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
    ]);
    let seed = seed(&backend, 2);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::ModelInvalid));
    assert_eq!(trace.reason(), Some(b"repair_budget_exhausted".as_slice()));
    assert_eq!(
        trace.summary(),
        Some(b"model output invalid after 2 repair turns".as_slice())
    );
    assert_eq!(trace.turns_remaining(), 7);
    assert_eq!(trace.mutations_remaining(), 4);
    assert_eq!(trace.tool_requests(), 0);
    assert_eq!(device.physical_writes(), 0);
}

/// c: write pin 0 with only pin 5 writable — denied before dispatch.
#[test]
fn trajectory_c_denied_pin_fails_before_dispatch() {
    let mut backend = backend(vec![
        "CALL gpio_pin_write {\"pin\": 0, \"level\": \"high\"}",
    ]);
    let seed = RunSeed {
        capabilities: write_only_pin_5(),
        ..seed(&backend, 3)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(
        trace.reason(),
        Some(b"pin_not_in_write_capabilities".as_slice())
    );
    assert_eq!(
        trace.summary(),
        Some(b"write to pin 0 denied by policy".as_slice())
    );
    assert_eq!(trace.turns_remaining(), 9);
    assert_eq!(trace.mutations_remaining(), 4);
    assert_eq!(trace.tool_requests(), 0);
    assert_eq!(device.physical_writes(), 0);
}

/// d: transient read failure retries under the same effect id.
#[test]
fn trajectory_d_transient_read_failure_retry_success() {
    // The fixture grants 2 mutations, not the default 4.
    let mut backend = backend(vec![
        "CALL gpio_pin_read {\"pin\": 4}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 is low\"}",
    ]);
    let seed = RunSeed {
        mutations: 2,
        ..seed(&backend, 4)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    faults.fail_transient(pin(4), 1);
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.summary(), Some(b"pin 4 is low".as_slice()));
    assert_eq!(trace.turns_remaining(), 8);
    assert_eq!(trace.mutations_remaining(), 2);
    assert_eq!(trace.tool_requests(), 1);
    assert_eq!(trace.tool_observations(), 2);
}

/// e: three verified-failed writes — `Stuck`, never `Completed`.
#[test]
fn trajectory_e_repeated_identical_failure_then_stuck() {
    let mut backend = backend(vec![
        "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
        "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
        "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
    ]);
    let seed = seed(&backend, 5);
    let mut device = FakeDevice::new();
    device.set_stuck(pin(5), Some(esper_core::decision::Level::Low));
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Stuck));
    assert_eq!(
        trace.reason(),
        Some(b"three_identical_verification_failures".as_slice())
    );
    assert_eq!(
        trace.summary(),
        Some(b"pin 5 did not reach high after 3 attempts".as_slice())
    );
    assert_eq!(trace.turns_remaining(), 7);
    assert_eq!(trace.mutations_remaining(), 1);
}

/// f: two model turns granted — durable `BudgetExhausted`, no third
/// inference.
#[test]
fn trajectory_f_budget_exhaustion_durable_terminal() {
    let mut backend = backend(vec![
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"unreached\"}",
    ]);
    let seed = RunSeed {
        model_turns: 2,
        ..seed(&backend, 6)
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
        None,
    )
    .expect("run failed");
    assert_eq!(
        trace.terminal_status(),
        Some(TerminalStatus::BudgetExhausted)
    );
    assert_eq!(trace.reason(), Some(b"model_turns_exhausted".as_slice()));
    assert_eq!(
        trace.summary(),
        Some(b"budget exhausted after 2 model turns".as_slice())
    );
    assert_eq!(trace.turns_remaining(), 0);
    assert_eq!(trace.mutations_remaining(), 3);
    // No third inference ever happened.
    assert_eq!(backend.lines_consumed(), 2);
}

/// g: crash after the physical write, before the observation — the
/// redelivered intent deduplicates and the write happens exactly once.
#[test]
fn trajectory_g_reset_after_write_before_outcome() {
    let mut backend = backend(vec![
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}",
    ]);
    let seed = seed(&backend, 7);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        Some(CrashPoint::AfterPhysicalBeforeObservation),
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.summary(), Some(b"pin 4 high".as_slice()));
    assert_eq!(trace.turns_remaining(), 8);
    assert_eq!(trace.mutations_remaining(), 3);
    assert_eq!(device.physical_writes(), 1);
}

/// h: `Ask` suspends durably; crash while awaiting input; input
/// arrives after the reboot and the run completes.
#[test]
fn trajectory_h_ask_suspend_resume() {
    let mut backend = backend(vec![
        "ASK {\"prompt\": \"which pin?\", \"schema\": 1}",
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high per operator\"}",
    ]);
    let seed = seed(&backend, 8);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(vec![b"{\"pin\": 4}".to_vec()]);
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        Some(CrashPoint::AwaitInput),
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.summary(), Some(b"pin 4 high per operator".as_slice()));
    assert_eq!(trace.turns_remaining(), 7);
    assert_eq!(trace.mutations_remaining(), 3);
    let approvals = trace
        .events()
        .iter()
        .filter(|event| matches!(event, esper_runtime::TraceEvent::ApprovalDecision { .. }))
        .count();
    assert_eq!(approvals, 1);
    assert_eq!(device.physical_writes(), 1);
}
