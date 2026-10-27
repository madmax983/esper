//! The run as a Waymaker workflow (SPEC §22): recovery at every
//! boundary, page-bounded records, and the crash points landing where
//! they claim to.
//!
//! The golden fixtures, the crash matrix and the context lifecycle
//! suites already drive the workflow through [`drive_run`]. These tests
//! add what a per-point matrix cannot see:
//!
//! - **Every crash point fires.** A crash point that never matched would
//!   pass its matrix test vacuously; each must cost exactly one extra
//!   boot.
//! - **A reset at any boundary is invisible.** For several shapes of
//!   run, a reset is injected before and after every record the run
//!   commits; each must reproduce the uncrashed run's trace, budgets and
//!   physical writes exactly.
//! - **Every record fits a scratch page.**
use esper_core::ids::{Pin, RunId};
use esper_core::state::TerminalStatus;
use esper_runtime::agent::wire::PAGE_RECORD_MAX;
use esper_runtime::{
    CrashPoint, Direction, FakeDevice, FaultPlan, InferenceSettings, InputPlan, Journal,
    ModelBackend, Record, Reset, RunSeed, RunTrace, ScriptedBackend, TraceEvent, drive_run,
    drive_run_with_reset,
};

/// One shape of run: a script, the operator's inputs, the seed and the
/// device it runs against.
struct Shape {
    name: &'static str,
    script: &'static [&'static str],
    inputs: &'static [&'static str],
    context_budget: Option<u64>,
    turns: u16,
    write_pin_input: Option<u8>,
    stuck_low: Option<u8>,
}

const SHAPES: &[Shape] = &[
    Shape {
        name: "read-write-verify-finish",
        script: &[
            "CALL gpio_pin_read {\"pin\": 4}",
            "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
            "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}",
        ],
        inputs: &[],
        context_budget: None,
        turns: 10,
        write_pin_input: None,
        stuck_low: None,
    },
    Shape {
        name: "ask-then-write",
        script: &[
            "ASK {\"prompt\": \"which pin?\", \"schema\": 1}",
            "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
            "FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high per operator\"}",
        ],
        inputs: &["{\"pin\": 4}"],
        context_budget: None,
        turns: 10,
        write_pin_input: None,
        stuck_low: None,
    },
    Shape {
        name: "repair-then-model-invalid",
        script: &[
            "CALL gpio_pin_read {\"pin\": 9}",
            "nonsense",
            "CALL sensor_sample_read {\"sensor\": 9}",
            "CALL gpio_pin_read {\"pin\": 4}",
        ],
        inputs: &[],
        context_budget: None,
        turns: 10,
        write_pin_input: None,
        stuck_low: None,
    },
    Shape {
        name: "delay-verified-by-clock",
        script: &[
            "CALL timer_delay_wait {\"ms\": 100}",
            "FINISH {\"status\": \"completed\", \"summary\": \"delay done once\"}",
        ],
        inputs: &[],
        context_budget: None,
        turns: 10,
        write_pin_input: None,
        stuck_low: None,
    },
    Shape {
        name: "write-denied-by-direction",
        script: &["CALL gpio_pin_write {\"pin\": 2, \"level\": \"high\"}"],
        inputs: &[],
        context_budget: None,
        turns: 10,
        write_pin_input: Some(2),
        stuck_low: None,
    },
    Shape {
        name: "stuck-actuator-three-failures",
        script: &[
            "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
            "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
            "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}",
        ],
        inputs: &[],
        context_budget: None,
        turns: 10,
        write_pin_input: None,
        stuck_low: Some(5),
    },
    Shape {
        name: "rollover-then-finish",
        script: &[
            "CALL gpio_pin_read {\"pin\": 4}",
            "CALL gpio_pin_read {\"pin\": 5}",
            "CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}",
            "CALL gpio_pin_read {\"pin\": 6}",
            "FINISH {\"status\": \"completed\", \"summary\": \"rolled and done\"}",
        ],
        inputs: &[],
        context_budget: Some(512),
        turns: 20,
        write_pin_input: None,
        stuck_low: None,
    },
];

/// The world one drive of a shape runs against.
struct World {
    backend: ScriptedBackend,
    device: FakeDevice,
    faults: FaultPlan,
    inputs: InputPlan,
    seed: RunSeed,
}

fn world(shape: &Shape, id: u64) -> World {
    let backend = ScriptedBackend::new(
        shape
            .script
            .iter()
            .map(|line| line.as_bytes().to_vec())
            .collect(),
        InferenceSettings::default_settings(),
    );
    let mut device = FakeDevice::new();
    if let Some(pin) = shape.write_pin_input {
        device.set_direction(Pin::new(pin).unwrap(), Direction::Input);
    }
    if let Some(pin) = shape.stuck_low {
        device.set_stuck(
            Pin::new(pin).unwrap(),
            Some(esper_core::decision::Level::Low),
        );
    }
    let seed = RunSeed {
        id: RunId::new(id),
        model_bundle: backend.bundle_id().0,
        model_turns: shape.turns,
        context_budget_bytes: shape.context_budget,
        ..RunSeed::default_slice()
    };
    World {
        backend,
        device,
        faults: FaultPlan::new(),
        inputs: InputPlan::new(
            shape
                .inputs
                .iter()
                .map(|input| input.as_bytes().to_vec())
                .collect(),
        ),
        seed,
    }
}

/// Everything a run's evidence says, in comparable form.
#[derive(Debug, PartialEq, Eq)]
struct Evidence {
    events: Vec<TraceEvent>,
    status: Option<TerminalStatus>,
    reason: Option<Vec<u8>>,
    summary: Option<Vec<u8>>,
    turns: u16,
    mutations: u16,
    suspended: bool,
    secrets: Vec<u64>,
    writes: Vec<(u8, bool)>,
    clock: u64,
}

fn evidence(trace: &RunTrace, device: &FakeDevice) -> Evidence {
    Evidence {
        events: trace.events().to_vec(),
        status: trace.terminal_status(),
        reason: trace.reason().map(<[u8]>::to_vec),
        summary: trace.summary().map(<[u8]>::to_vec),
        turns: trace.turns_remaining(),
        mutations: trace.mutations_remaining(),
        suspended: trace.suspended(),
        secrets: trace.secret_digests().to_vec(),
        writes: device
            .write_ledger()
            .iter()
            .map(|w| (w.pin, w.level_high))
            .collect(),
        clock: device.clock_read(),
    }
}

/// Drive a shape with no reset: its evidence and how many records its
/// first segment committed.
fn baseline(shape: &Shape) -> (Evidence, usize, Journal) {
    let mut w = world(shape, 900);
    let mut journal = Journal::new();
    let trace = drive_run(
        &w.seed,
        &mut journal,
        &mut w.backend,
        &mut w.device,
        &mut w.faults,
        &mut w.inputs,
        None,
    )
    .unwrap_or_else(|e| panic!("{}: baseline failed: {e}", shape.name));
    assert_eq!(
        journal.boots(),
        1,
        "{}: an uncrashed run boots once",
        shape.name
    );
    (evidence(&trace, &w.device), journal.commits(), journal)
}

#[test]
fn every_shape_reaches_a_terminal() {
    for shape in SHAPES {
        let (evidence, commits, _) = baseline(shape);
        assert!(
            evidence.status.is_some(),
            "{}: the run must end",
            shape.name
        );
        assert!(commits > 0, "{}", shape.name);
    }
}

/// SPEC §14 under Waymaker: a reset before or after any record the run
/// commits — every intent, every answer, including an answer the world
/// gave and the reset lost — reproduces the uncrashed run exactly.
#[test]
fn a_reset_at_every_boundary_reproduces_the_run() {
    let mut sweeps = 0;
    for shape in SHAPES {
        let (expected, commits, _) = baseline(shape);
        for nth in 1..=commits {
            for before in [false, true] {
                let mut w = world(shape, 900);
                let mut journal = Journal::new();
                let trace = drive_run_with_reset(
                    &w.seed,
                    &mut journal,
                    &mut w.backend,
                    &mut w.device,
                    &mut w.faults,
                    &mut w.inputs,
                    Reset { nth, before },
                )
                .unwrap_or_else(|e| {
                    panic!(
                        "{}: reset at {nth} (before {before}) failed: {e}",
                        shape.name
                    )
                });
                assert_eq!(
                    journal.boots(),
                    2,
                    "{}: the reset at {nth} (before {before}) must fire once",
                    shape.name
                );
                assert_eq!(
                    evidence(&trace, &w.device),
                    expected,
                    "{}: reset at {nth} (before {before}) changed the run",
                    shape.name
                );
                sweeps += 1;
            }
        }
    }
    assert!(sweeps >= 100, "the sweep covered only {sweeps} resets");
}

/// Each crash point lands on a real boundary: it costs exactly one
/// extra boot, and the run is unchanged.
#[test]
fn every_crash_point_fires_once_and_changes_nothing() {
    for point in CrashPoint::all() {
        let shape = if point == CrashPoint::AwaitInput {
            &SHAPES[1]
        } else {
            &SHAPES[0]
        };
        let (expected, _, _) = baseline(shape);
        let mut w = world(shape, 900);
        let mut journal = Journal::new();
        let trace = drive_run(
            &w.seed,
            &mut journal,
            &mut w.backend,
            &mut w.device,
            &mut w.faults,
            &mut w.inputs,
            Some(point),
        )
        .unwrap_or_else(|e| panic!("{}: {e}", point.name()));
        assert_eq!(journal.boots(), 2, "{} never fired", point.name());
        assert_eq!(
            evidence(&trace, &w.device),
            expected,
            "{} changed the run",
            point.name()
        );
    }
}

/// Every record the run commits — its input, each activity input and
/// each answer — fits one scratch page.
#[test]
fn every_record_fits_a_scratch_page() {
    for shape in SHAPES {
        let (_, _, journal) = baseline(shape);
        for record in journal.records() {
            let len = match record {
                Record::Scheduled { input, .. } => input.len(),
                Record::Resolved { bytes, .. } => bytes.len(),
            };
            assert!(
                len <= PAGE_RECORD_MAX,
                "{}: a {len}-byte record",
                shape.name
            );
        }
    }
}

/// A transient fault is a recorded attempt: the third consecutive one
/// ends the run `Stuck`, with all three attempts in the trace and the
/// device never touched.
#[test]
fn transient_exhaustion_records_every_attempt() {
    let mut w = world(
        &Shape {
            name: "transient-exhaustion",
            script: &["CALL gpio_pin_read {\"pin\": 4}"],
            inputs: &[],
            context_budget: None,
            turns: 10,
            write_pin_input: None,
            stuck_low: None,
        },
        901,
    );
    w.faults.fail_transient(Pin::new(4).unwrap(), 5);
    let mut journal = Journal::new();
    let trace = drive_run(
        &w.seed,
        &mut journal,
        &mut w.backend,
        &mut w.device,
        &mut w.faults,
        &mut w.inputs,
        None,
    )
    .unwrap();
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Stuck));
    assert_eq!(
        trace.reason(),
        Some(b"transient_attempts_exhausted".as_slice())
    );
    assert_eq!(
        trace.summary(),
        Some(b"transient faults exhausted after 3 attempts".as_slice())
    );
    let failed = trace
        .events()
        .iter()
        .filter(|e| matches!(e, TraceEvent::ToolObservation { ok: false, .. }))
        .count();
    assert_eq!(failed, 3, "every attempt is recorded");
    assert_eq!(trace.tool_requests(), 1, "one intent, three attempts");
}
