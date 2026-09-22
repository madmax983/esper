//! Crash matrix: every crash point from SPEC §14, one reboot each.
//!
//! Each test drives the success trajectory (a) — or the Ask trajectory
//! (h) for `cp_await_input`, the only point that needs a suspension —
//! with exactly one injected crash, and asserts the run still reaches
//! the identical terminal result with identical budgets and exactly
//! one physical write. Recovery must prove:
//!
//! - no dispatch without a committed intent,
//! - committed outcomes replay byte-identically,
//! - unresolved effect identity stays stable,
//! - budgets and permissions never widen,
//! - one logical terminal result.

use esper_core::ids::RunId;
use esper_core::state::TerminalStatus;
use esper_runtime::{
    CrashPoint, FakeDevice, FaultPlan, InferenceSettings, InputPlan, Journal, ModelBackend,
    RunSeed, ScriptedBackend, drive_run,
};

/// The success script from trajectory (a).
fn success_script() -> Vec<Vec<u8>> {
    vec![
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}",
    ]
    .into_iter()
    .map(|line| line.as_bytes().to_vec())
    .collect()
}

/// The Ask script from trajectory (h).
fn ask_script() -> Vec<Vec<u8>> {
    vec![
        "ASK {\"prompt\": \"which pin?\", \"schema\": 1}",
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high per operator\"}",
    ]
    .into_iter()
    .map(|line| line.as_bytes().to_vec())
    .collect()
}

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

/// Drive one run with one injected crash; return the trace and the
/// device's physical write count.
fn drive_once(
    id: u64,
    script: Vec<Vec<u8>>,
    inputs: Vec<Vec<u8>>,
    crash: CrashPoint,
) -> (esper_runtime::RunTrace, u32) {
    let mut backend = backend(script);
    let seed = RunSeed {
        id: RunId::new(id),
        ..backend_seed(&backend)
    };
    let mut journal = Journal::new();
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut input_plan = InputPlan::new(inputs);
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut input_plan,
        Some(crash),
    )
    .expect("run failed");
    (trace, device.physical_writes())
}

/// Assert the success trajectory's exact terminal evidence.
fn assert_success(trace: &esper_runtime::RunTrace, physical_writes: u32) {
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.reason(), Some(b"model_finished".as_slice()));
    assert_eq!(trace.summary(), Some(b"pin 4 high".as_slice()));
    assert_eq!(trace.turns_remaining(), 7);
    assert_eq!(trace.mutations_remaining(), 3);
    assert_eq!(physical_writes, 1);
    // Exactly one terminal event: one logical terminal result.
    let terminals = trace
        .events()
        .iter()
        .filter(|event| matches!(event, esper_runtime::TraceEvent::Terminal { .. }))
        .count();
    assert_eq!(terminals, 1);
}

/// Assert the Ask trajectory's exact terminal evidence.
fn assert_ask_success(trace: &esper_runtime::RunTrace, physical_writes: u32) {
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.summary(), Some(b"pin 4 high per operator".as_slice()));
    assert_eq!(trace.turns_remaining(), 7);
    assert_eq!(trace.mutations_remaining(), 3);
    assert_eq!(physical_writes, 1);
    let terminals = trace
        .events()
        .iter()
        .filter(|event| matches!(event, esper_runtime::TraceEvent::Terminal { .. }))
        .count();
    assert_eq!(terminals, 1);
}

#[test]
fn crash_cp_model_intent() {
    let (trace, writes) = drive_once(101, success_script(), Vec::new(), CrashPoint::ModelIntent);
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_before_decision_commit() {
    let (trace, writes) = drive_once(
        102,
        success_script(),
        Vec::new(),
        CrashPoint::BeforeDecisionCommit,
    );
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_before_authorize() {
    let (trace, writes) = drive_once(
        103,
        success_script(),
        Vec::new(),
        CrashPoint::BeforeAuthorize,
    );
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_tool_intent() {
    let (trace, writes) = drive_once(104, success_script(), Vec::new(), CrashPoint::ToolIntent);
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_after_physical_before_observation() {
    let (trace, writes) = drive_once(
        105,
        success_script(),
        Vec::new(),
        CrashPoint::AfterPhysicalBeforeObservation,
    );
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_after_observation_before_verify() {
    let (trace, writes) = drive_once(
        106,
        success_script(),
        Vec::new(),
        CrashPoint::AfterObservationBeforeVerify,
    );
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_before_verification_commit() {
    let (trace, writes) = drive_once(
        107,
        success_script(),
        Vec::new(),
        CrashPoint::BeforeVerificationCommit,
    );
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_after_verification() {
    let (trace, writes) = drive_once(
        108,
        success_script(),
        Vec::new(),
        CrashPoint::AfterVerification,
    );
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_await_input() {
    let (trace, writes) = drive_once(
        109,
        ask_script(),
        vec![b"{\"pin\": 4}".to_vec()],
        CrashPoint::AwaitInput,
    );
    assert_ask_success(&trace, writes);
}

#[test]
fn crash_cp_before_terminal_commit() {
    let (trace, writes) = drive_once(
        110,
        success_script(),
        Vec::new(),
        CrashPoint::BeforeTerminalCommit,
    );
    assert_success(&trace, writes);
}

#[test]
fn crash_cp_after_terminal_commit() {
    let (trace, writes) = drive_once(
        111,
        success_script(),
        Vec::new(),
        CrashPoint::AfterTerminalCommit,
    );
    assert_success(&trace, writes);
}

/// Every crash point has a fixture spelling and the matrix covers all
/// eleven.
#[test]
fn all_crash_points_have_fixture_spellings() {
    let names: Vec<&'static str> = CrashPoint::all().iter().map(|point| point.name()).collect();
    assert_eq!(
        names,
        vec![
            "cp_model_intent",
            "cp_before_decision_commit",
            "cp_before_authorize",
            "cp_tool_intent",
            "cp_after_physical_before_observation",
            "cp_after_observation_before_verify",
            "cp_before_verification_commit",
            "cp_after_verification",
            "cp_await_input",
            "cp_before_terminal_commit",
            "cp_after_terminal_commit",
        ]
    );
}
