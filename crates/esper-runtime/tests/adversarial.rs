//! Adversarial E2 scripts: hostile or confused model output must fail
//! closed without touching the world.
//!
//! Every test here asserts the same three properties after the run
//! ends: the terminal status and reason are the normative ones, no
//! tool request was ever committed, and the device is untouched (zero
//! physical writes, the virtual clock unmoved). Authorization denies
//! before dispatch, and schema violations burn repair turns — neither
//! reaches hardware.

use esper_core::ids::RunId;
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

/// The default full grants, as a starting point for denying exactly
/// one capability per hostile test.
const fn full_caps() -> Capabilities {
    RunSeed::default_slice().capabilities
}

/// Drive a run with no crash injection.
fn drive(
    seed: &RunSeed,
    journal: &mut Journal,
    backend: &mut ScriptedBackend,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
) -> esper_runtime::RunTrace {
    drive_run(seed, journal, backend, device, faults, inputs, None).expect("run failed")
}

/// The world is untouched: no physical write ever executed and the
/// virtual clock never moved.
fn assert_world_untouched(device: &FakeDevice) {
    assert_eq!(device.physical_writes(), 0);
    assert_eq!(device.clock_read(), 0);
}

/// A sensor read with no sensor grant: denied, never dispatched.
#[test]
fn hostile_sensor_read_without_grant_is_denied() {
    let mut backend = backend(vec!["CALL sensor_sample_read {\"sensor\": 0}"]);
    let seed = RunSeed {
        capabilities: Capabilities {
            sensors: [0; 4],
            sensor_count: 0,
            ..full_caps()
        },
        ..seed(&backend, 1)
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(
        trace.reason(),
        Some(b"sensor_not_in_capabilities".as_slice())
    );
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}

/// A delay with the timer not granted: denied, the clock never moves.
#[test]
fn hostile_delay_without_timer_grant_is_denied() {
    let mut backend = backend(vec!["CALL timer_delay_wait {\"ms\": 250}"]);
    let seed = RunSeed {
        capabilities: Capabilities {
            allow_timer: false,
            ..full_caps()
        },
        ..seed(&backend, 2)
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(trace.reason(), Some(b"timer_not_permitted".as_slice()));
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}

/// An uptime read with the timer not granted: denied as well.
#[test]
fn hostile_uptime_without_timer_grant_is_denied() {
    let mut backend = backend(vec!["CALL timer_uptime_read {}"]);
    let seed = RunSeed {
        capabilities: Capabilities {
            allow_timer: false,
            ..full_caps()
        },
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(trace.reason(), Some(b"timer_not_permitted".as_slice()));
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}

/// A status report with status not granted: denied, never dispatched.
#[test]
fn hostile_status_without_grant_is_denied() {
    let mut backend = backend(vec!["CALL device_status_report {\"detail\": \"summary\"}"]);
    let seed = RunSeed {
        capabilities: Capabilities {
            allow_status: false,
            ..full_caps()
        },
        ..seed(&backend, 4)
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(trace.reason(), Some(b"status_not_permitted".as_slice()));
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}

/// An unknown tool (`http_get`) is not in the static catalog: each
/// line is malformed, and three burn the repair budget into
/// `ModelInvalid`. No generic network access exists.
#[test]
fn hostile_unknown_tool_is_model_invalid() {
    let mut backend = backend(vec![
        "CALL http_get {\"url\": \"http://example.com\"}",
        "CALL http_get {\"url\": \"http://example.com\"}",
        "CALL http_get {\"url\": \"http://example.com\"}",
    ]);
    let seed = seed(&backend, 5);
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::ModelInvalid));
    assert_eq!(trace.reason(), Some(b"repair_budget_exhausted".as_slice()));
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}

/// Write escalation under read-only caps: denied before dispatch.
#[test]
fn hostile_write_escalation_is_denied() {
    let mut backend = backend(vec![
        "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
    ]);
    let seed = RunSeed {
        capabilities: Capabilities {
            write_pins: [0; 8],
            write_count: 0,
            ..full_caps()
        },
        ..seed(&backend, 6)
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(
        trace.reason(),
        Some(b"pin_not_in_write_capabilities".as_slice())
    );
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}

/// Read escalation under write-only caps: denied before dispatch.
#[test]
fn hostile_read_escalation_is_denied() {
    let mut backend = backend(vec!["CALL gpio_pin_read {\"pin\": 4}"]);
    let seed = RunSeed {
        capabilities: Capabilities {
            read_pins: [0; 8],
            read_count: 0,
            ..full_caps()
        },
        ..seed(&backend, 7)
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(
        trace.reason(),
        Some(b"pin_not_in_read_capabilities".as_slice())
    );
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}

/// An out-of-range sensor id violates the static schema: each line
/// burns a repair turn, and three end the run `ModelInvalid`.
#[test]
fn hostile_out_of_range_sensor_is_model_invalid() {
    let mut backend = backend(vec![
        "CALL sensor_sample_read {\"sensor\": 9}",
        "CALL sensor_sample_read {\"sensor\": 9}",
        "CALL sensor_sample_read {\"sensor\": 9}",
    ]);
    let seed = seed(&backend, 8);
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::ModelInvalid));
    assert_eq!(trace.reason(), Some(b"repair_budget_exhausted".as_slice()));
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}

/// A hostile line after a legitimate step: the earlier step stands,
/// the hostile call is denied, and nothing is dispatched for it.
#[test]
fn hostile_call_after_legitimate_step_is_denied() {
    let mut backend = backend(vec![
        "CALL sensor_sample_read {\"sensor\": 1}",
        "CALL timer_delay_wait {\"ms\": 100}",
    ]);
    let seed = RunSeed {
        capabilities: Capabilities {
            allow_timer: false,
            ..full_caps()
        },
        ..seed(&backend, 9)
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
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(trace.reason(), Some(b"timer_not_permitted".as_slice()));
    // The legitimate sensor read committed exactly one tool request.
    assert_eq!(trace.tool_requests(), 1);
    assert_eq!(device.physical_writes(), 0);
    assert_eq!(device.clock_read(), 0);
}

/// Crash at the authorize boundary of a hostile call: recovery still
/// denies, and the reboot dispatches nothing.
#[test]
fn hostile_call_survives_reboot_as_denial() {
    let mut backend = backend(vec!["CALL sensor_sample_read {\"sensor\": 0}"]);
    let seed = RunSeed {
        capabilities: Capabilities {
            sensors: [0; 4],
            sensor_count: 0,
            ..full_caps()
        },
        ..seed(&backend, 10)
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
    assert_eq!(
        trace.reason(),
        Some(b"sensor_not_in_capabilities".as_slice())
    );
    assert_eq!(trace.tool_requests(), 0);
    assert_world_untouched(&device);
}
