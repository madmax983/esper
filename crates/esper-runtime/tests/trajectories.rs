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
    drive_run, CrashPoint, FakeDevice, FaultPlan, InputPlan, Journal, RunSeed, ScriptedModel,
};

/// Build a default seed with this run id.
const fn seed(id: u64) -> RunSeed {
    RunSeed {
        id: RunId::new(id),
        ..RunSeed::default_slice()
    }
}

/// Drive a run with no crash injection.
fn drive(
    seed: &RunSeed,
    journal: &mut Journal,
    script: Vec<&str>,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    crash: Option<CrashPoint>,
) -> esper_runtime::RunTrace {
    let mut model = ScriptedModel::new(
        script
            .into_iter()
            .map(|line| line.as_bytes().to_vec())
            .collect(),
    );
    drive_run(seed, journal, &mut model, device, faults, inputs, crash).expect("run failed")
}

fn pin(n: u8) -> Pin {
    Pin::new(n).expect("bad pin in test")
}

/// a: read pin 4, write pin 4 high, verify, finish — crash once at
/// `cp_tool_intent`.
#[test]
fn trajectory_a_success_read_write_verify_finish() {
    let seed = seed(1);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "CALL gpio_pin_read {\"pin\": 4}",
            "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
            "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}",
        ],
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
    let seed = seed(2);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "do a flip",
            "CALL gpio_pin_read {\"pin\": 99}",
            "CALLL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        ],
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
    let seed = RunSeed {
        writable_pins: 1 << 5,
        ..seed(3)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec!["CALL gpio_pin_write {\"pin\": 0, \"level\": \"high\"}"],
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
    let seed = RunSeed {
        mutations: 2,
        ..seed(4)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    faults.fail_transient(pin(4), 1);
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "CALL gpio_pin_read {\"pin\": 4}",
            "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 is low\"}",
        ],
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
    let seed = seed(5);
    let mut device = FakeDevice::new();
    device.set_stuck(pin(5), Some(esper_core::decision::Level::Low));
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
            "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
            "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
        ],
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
    let seed = RunSeed {
        model_turns: 2,
        ..seed(6)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut model = ScriptedModel::new(vec![
        b"CALL gpio_pin_read {\"pin\": 4}".to_vec(),
        b"CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}".to_vec(),
        b"FINISH {\"status\": \"completed\", \"summary\": \"unreached\"}".to_vec(),
    ]);
    let mut journal = Journal::new();
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut model,
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
    assert_eq!(model.lines_consumed(), 2);
}

/// g: crash after the physical write, before the observation — the
/// redelivered intent deduplicates and the write happens exactly once.
#[test]
fn trajectory_g_reset_after_write_before_outcome() {
    let seed = seed(7);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
            "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}",
        ],
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
    let seed = seed(8);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(vec![b"{\"pin\": 4}".to_vec()]);
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "ASK {\"prompt\": \"which pin?\", \"schema\": 1}",
            "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
            "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high per operator\"}",
        ],
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
