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
    Capabilities, CrashPoint, FakeDevice, FaultPlan, InputPlan, Journal, RunSeed, ScriptedModel,
    drive_run,
};

/// Build a default seed with this run id.
const fn seed(id: u64) -> RunSeed {
    RunSeed {
        id: RunId::new(id),
        ..RunSeed::default_slice()
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
    script: Vec<&str>,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
) -> esper_runtime::RunTrace {
    let mut model = ScriptedModel::new(
        script
            .into_iter()
            .map(|line| line.as_bytes().to_vec())
            .collect(),
    );
    drive_run(seed, journal, &mut model, device, faults, inputs, None).expect("run failed")
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
    let seed = RunSeed {
        capabilities: Capabilities {
            sensors: [0; 4],
            sensor_count: 0,
            ..full_caps()
        },
        ..seed(1)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec!["CALL sensor_sample_read {\"sensor\": 0}"],
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
    let seed = RunSeed {
        capabilities: Capabilities {
            allow_timer: false,
            ..full_caps()
        },
        ..seed(2)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec!["CALL timer_delay_wait {\"ms\": 250}"],
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
    let seed = RunSeed {
        capabilities: Capabilities {
            allow_timer: false,
            ..full_caps()
        },
        ..seed(3)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec!["CALL timer_uptime_read {}"],
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
    let seed = RunSeed {
        capabilities: Capabilities {
            allow_status: false,
            ..full_caps()
        },
        ..seed(4)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec!["CALL device_status_report {\"detail\": \"summary\"}"],
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
    let seed = seed(5);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "CALL http_get {\"url\": \"http://example.com\"}",
            "CALL http_get {\"url\": \"http://example.com\"}",
            "CALL http_get {\"url\": \"http://example.com\"}",
        ],
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
    let seed = RunSeed {
        capabilities: Capabilities {
            write_pins: [0; 8],
            write_count: 0,
            ..full_caps()
        },
        ..seed(6)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec!["CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}"],
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
    let seed = RunSeed {
        capabilities: Capabilities {
            read_pins: [0; 8],
            read_count: 0,
            ..full_caps()
        },
        ..seed(7)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec!["CALL gpio_pin_read {\"pin\": 4}"],
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
    let seed = seed(8);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "CALL sensor_sample_read {\"sensor\": 9}",
            "CALL sensor_sample_read {\"sensor\": 9}",
            "CALL sensor_sample_read {\"sensor\": 9}",
        ],
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
    let seed = RunSeed {
        capabilities: Capabilities {
            allow_timer: false,
            ..full_caps()
        },
        ..seed(9)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let trace = drive(
        &seed,
        &mut journal,
        vec![
            "CALL sensor_sample_read {\"sensor\": 1}",
            "CALL timer_delay_wait {\"ms\": 100}",
        ],
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
    let seed = RunSeed {
        capabilities: Capabilities {
            sensors: [0; 4],
            sensor_count: 0,
            ..full_caps()
        },
        ..seed(10)
    };
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let mut journal = Journal::new();
    let mut model = ScriptedModel::new(vec![b"CALL sensor_sample_read {\"sensor\": 0}".to_vec()]);
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut model,
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
