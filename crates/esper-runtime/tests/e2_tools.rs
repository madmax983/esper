//! The E2 typed tools end to end: happy paths, exact payloads, and
//! idempotency across crashes.
//!
//! These tests pin the SPEC §17.6 runtime mapping: the fixed sensor
//! values, the uptime/delay payload shapes, the status report shapes,
//! the delay verifier's independent clock read, and the rule that a
//! redelivered intent under one effect id executes exactly once.

use esper_core::ids::{Pin, RunId};
use esper_core::state::TerminalStatus;
use esper_runtime::{
    CrashPoint, FakeDevice, FaultPlan, InferenceSettings, InputPlan, Journal, ModelBackend,
    RunSeed, ScriptedBackend, TraceEvent, drive_run,
};
use waymaker_core::{EffectId, EffectSeq, RunId as WaymakerRunId};

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

/// Drive a run, optionally crashing once at `crash`.
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

/// The committed (non-transient) observation payloads, in order.
fn observation_payloads(trace: &esper_runtime::RunTrace) -> Vec<Vec<u8>> {
    trace
        .events()
        .iter()
        .filter_map(|event| match event {
            TraceEvent::ToolObservation {
                ok: true, payload, ..
            } => Some(payload.clone()),
            _ => None,
        })
        .collect()
}

/// The committed verifications, in order.
fn verifications(trace: &esper_runtime::RunTrace) -> Vec<(bool, Vec<u8>, Vec<u8>)> {
    trace
        .events()
        .iter()
        .filter_map(|event| match event {
            TraceEvent::Verification {
                passed,
                expected,
                observed,
                ..
            } => Some((*passed, expected.clone(), observed.clone())),
            _ => None,
        })
        .collect()
}

/// `sensor_sample_read` returns the fixed channel value (SPEC §15.13).
#[test]
fn sensor_read_returns_fixed_values() {
    let mut backend = backend(vec![
        "CALL sensor_sample_read {\"sensor\": 0}",
        "CALL sensor_sample_read {\"sensor\": 2}",
        "FINISH {\"status\": \"completed\", \"summary\": \"sensors sampled\"}",
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
        None,
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(
        observation_payloads(&trace),
        vec![
            b"{\"sensor\":0,\"value\":210}".to_vec(),
            b"{\"sensor\":2,\"value\":1800}".to_vec(),
        ]
    );
    // Reads are evidence, not mutations: the mutation budget is intact.
    assert_eq!(trace.mutations_remaining(), 4);
    assert_eq!(device.physical_writes(), 0);
}

/// `timer_uptime_read` reports the virtual clock without moving it.
#[test]
fn uptime_read_reports_the_clock() {
    let mut backend = backend(vec![
        "CALL timer_uptime_read {}",
        "FINISH {\"status\": \"completed\", \"summary\": \"uptime read\"}",
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
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(
        observation_payloads(&trace),
        vec![b"{\"uptime_ms\":0}".to_vec(),]
    );
    assert_eq!(device.clock_read(), 0);
}

/// `timer_delay_wait` advances the clock, consumes one mutation, and
/// the independent verifier confirms the advance through its own
/// clock handle.
#[test]
fn delay_wait_advances_clock_and_verifies() {
    let mut backend = backend(vec![
        "CALL timer_delay_wait {\"ms\": 250}",
        "FINISH {\"status\": \"completed\", \"summary\": \"waited\"}",
    ]);
    let seed = seed(&backend, 3);
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
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(
        observation_payloads(&trace),
        vec![b"{\"waited_ms\":250,\"uptime_ms\":250}".to_vec(),]
    );
    // The delay is an idempotent write: one mutation consumed, and the
    // verifier passed on the independent clock read.
    assert_eq!(trace.mutations_remaining(), 3);
    assert_eq!(
        verifications(&trace),
        vec![(
            true,
            b"{\"target_ms\":250}".to_vec(),
            b"{\"uptime_ms\":250}".to_vec(),
        )]
    );
    assert_eq!(device.clock_read(), 250);
    assert_eq!(device.physical_writes(), 0);
}

/// Crash between the physical delay and the observation commit: the
/// redelivered intent replays the cached outcome without advancing
/// the clock a second time.
#[test]
fn delay_redelivery_across_crash_advances_exactly_once() {
    let mut backend = backend(vec![
        "CALL timer_delay_wait {\"ms\": 250}",
        "CALL timer_uptime_read {}",
        "FINISH {\"status\": \"completed\", \"summary\": \"waited once\"}",
    ]);
    let seed = seed(&backend, 4);
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
    // The clock advanced exactly once: the uptime read sees 250, not 500.
    assert_eq!(
        observation_payloads(&trace),
        vec![
            b"{\"waited_ms\":250,\"uptime_ms\":250}".to_vec(),
            b"{\"uptime_ms\":250}".to_vec(),
        ]
    );
    assert_eq!(device.clock_read(), 250);
    assert_eq!(trace.mutations_remaining(), 3);
}

/// `device_status_report` summary: counts plus the clock.
#[test]
fn status_summary_payload_is_canonical() {
    let mut backend = backend(vec![
        "CALL device_status_report {\"detail\": \"summary\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"status read\"}",
    ]);
    let seed = seed(&backend, 6);
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
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(
        observation_payloads(&trace),
        vec![b"{\"pins\":8,\"sensors\":4,\"uptime_ms\":0}".to_vec(),]
    );
}

/// `device_status_report` full: summary plus pin direction/level
/// masks and the per-sensor raw values.
#[test]
fn status_full_payload_is_canonical() {
    let mut backend = backend(vec![
        "CALL device_status_report {\"detail\": \"full\"}",
        "FINISH {\"status\": \"completed\", \"summary\": \"status read\"}",
    ]);
    let seed = seed(&backend, 7);
    let mut device = FakeDevice::new();
    device.set_direction(pin(4), esper_runtime::Direction::Input);
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
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    // Pin 4 is an input (element 4 of the direction array is 0); every
    // pin is low.
    assert_eq!(
        observation_payloads(&trace),
        vec![b"{\"pins\":8,\"sensors\":4,\"uptime_ms\":0,\"dir\":[1,1,1,1,0,1,1,1],\"level\":[0,0,0,0,0,0,0,0],\"values\":[210,315,1800,42]}".to_vec(),]
    );
}

/// Redelivered `gpio_pin_write` under one effect id executes exactly
/// one physical write (device-level idempotency).
#[test]
fn gpio_write_redelivery_executes_one_physical_write() {
    let mut device = FakeDevice::new();
    let id = EffectId {
        run: WaymakerRunId(7),
        seq: EffectSeq(1),
    };
    let digest = 99u64;
    let first = device
        .dispatch_write(pin(4), true, id, digest)
        .expect("first dispatch");
    let second = device
        .dispatch_write(pin(4), true, id, digest)
        .expect("redelivery");
    assert_eq!(first, second);
    assert_eq!(device.physical_writes(), 1);
    assert_eq!(
        device.write_ledger(),
        &[esper_runtime::WriteRecord {
            pin: 4,
            level_high: true,
            seq: 1
        }]
    );
}

/// A tool-targeted fault fires only for its tool: a `sensor_sample_read`
/// hiccup does not disturb a `gpio_pin_read` of the same resource.
#[test]
fn fault_plan_tool_filter_is_selective() {
    let mut faults = FaultPlan::new();
    // Tool 3 (sensor) on resource 1 fails twice; tool 1 (gpio) is unaffected.
    faults.fail_transient_for(3, 1, 2);
    assert!(faults.consume(3, 1));
    assert!(faults.consume(3, 1));
    assert!(!faults.consume(3, 1));
    assert!(!faults.consume(1, 1));
    // The unfiltered E0/E1 form still matches any tool for its pin.
    faults.fail_transient(pin(4), 1);
    assert!(faults.consume(1, 4));
    assert!(!faults.consume(2, 4));
}
