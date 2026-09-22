//! The data-driven fixture runner: parse, drive, assert.
//!
//! [`run_fixture`] turns one parsed [`Fixture`] into a live run: it
//! builds the [`RunSeed`] (with a run id derived deterministically
//! from the fixture id), the scripted model, the fake device, the
//! transient-fault plan, and the input queue the fixture describes,
//! drives [`drive_run`] to its terminal
//! result — injecting the planned reset and letting the engine recover
//! by journal replay — and then asserts, in order:
//!
//! 1. the terminal status,
//! 2. the remaining budgets,
//! 3. the exact expected trace ([`crate::compare`]),
//! 4. every listed SPEC §11.3 invariant ([`crate::checks`]),
//! 5. every `forbidden` clause ([`crate::checks`]).
//!
//! Where the runtime's public API cannot express something the fixture
//! format allows (several crash points, an initially-high pin, two
//! transient faults on one pin), the runner fails closed with a clear
//! [`FixtureError::AssertionFailed`] naming the gap instead of
//! silently approximating it. None of the shipped fixtures need those
//! shapes.
//!
//! // HOST-ONLY (E0/E1): heap-allocated orchestration, for the host runner.

use esper_core::decision::Level;
use esper_core::ids::{Digest, Pin, RunId};
use esper_core::registry::Capabilities;
use esper_runtime::{
    Direction, FakeDevice, FaultPlan, InputPlan, Journal, RunSeed, RunTrace, ScriptedModel,
    drive_run,
};

use crate::checks::{CheckCtx, pin_levels};
use crate::compare::compare_trace;
use crate::fixture::{
    CrashEntry, DeviceFault, DeviceSpec, FaultResource, Fixture, FixtureError, SeedSpec,
    parse_fixture,
};

/// The outcome of running one fixture: the evidence, plus counts of
/// what was checked.
#[derive(Debug)]
pub struct FixtureReport {
    /// The fixture id that was run.
    pub fixture_id: String,
    /// The fixture title.
    pub title: String,
    /// The derived run trace.
    pub trace: RunTrace,
    /// How many expected trace events were compared.
    pub events_compared: usize,
    /// How many §11.3 invariants were checked.
    pub invariants_checked: usize,
    /// How many `forbidden` clauses were checked.
    pub forbidden_checked: usize,
}

/// Run one parsed fixture to its terminal result and assert everything
/// the fixture expects.
///
/// # Errors
///
/// Returns [`FixtureError`] when the fixture is malformed, the driver
/// fails, or any assertion — terminal, budgets, trace, invariant, or
/// forbidden clause — does not hold.
pub fn run_fixture(fixture: &Fixture) -> Result<FixtureReport, FixtureError> {
    let seed = build_seed(fixture)?;
    let mut model = ScriptedModel::new(fixture.script.clone());
    let (mut device, mut faults) = build_device(&fixture.device)?;
    let initial_levels = pin_levels(&device);
    let mut inputs = build_inputs(fixture);
    let mut journal = Journal::new();
    let crash = crash_point(&fixture.crash_plan)?;

    let trace = drive_run(
        &seed,
        &mut journal,
        &mut model,
        &mut device,
        &mut faults,
        &mut inputs,
        crash,
    )?;

    if trace.suspended() {
        return Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: "the run suspended awaiting human input with none queued; \
                     the fixture's input_events do not answer every Ask"
                .to_owned(),
        });
    }
    let want_terminal = fixture.expected.terminal_status;
    if trace.terminal_status() != Some(want_terminal) {
        return Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: format!(
                "terminal status differs: expected `{}`, the run ended `{}`",
                want_terminal.name(),
                trace
                    .terminal_status()
                    .map_or_else(|| "<none>".to_owned(), |status| status.name().to_owned()),
            ),
        });
    }
    assert_budgets(fixture, &trace)?;
    compare_trace(&fixture.expected.trace, &trace)?;

    let ctx = CheckCtx {
        trace: &trace,
        seed: &seed,
        device: &device,
        initial_levels,
    };
    for invariant in &fixture.expected.invariants {
        invariant.check(&ctx)?;
    }
    for clause in &fixture.expected.forbidden {
        clause.check(&ctx)?;
    }

    Ok(FixtureReport {
        // HOST-ONLY (E0/E1)
        fixture_id: fixture.id.clone(),
        title: fixture.title.clone(),
        trace,
        events_compared: fixture.expected.trace.len(),
        invariants_checked: fixture.expected.invariants.len(),
        forbidden_checked: fixture.expected.forbidden.len(),
    })
}

/// Read a fixture file, parse it, and run it.
///
/// # Errors
///
/// Returns [`FixtureError`] for I/O, JSON, schema, driver, or
/// assertion failures.
pub fn run_fixture_file(path: &str) -> Result<FixtureReport, FixtureError> {
    let text = std::fs::read_to_string(path).map_err(|source| FixtureError::Io {
        // HOST-ONLY (E0/E1)
        path: path.to_owned(),
        source,
    })?;
    run_fixture(&parse_fixture(&text)?)
}

// ---------------------------------------------------------------------------
// World construction.
// ---------------------------------------------------------------------------

/// Build the run seed: budgets and capabilities from the fixture,
/// with a run id derived deterministically from the fixture id so the
/// same fixture always names the same run.
///
/// The E2 capabilities ride along as the runtime's capability set:
/// the pin lists, the sensor list, and the timer/status grants (SPEC
/// §17.7). Absent fixture fields parsed as denied, so E0/E1 fixtures
/// keep their meaning unchanged.
///
/// # Errors
///
/// Returns [`FixtureError::AssertionFailed`] when a capability list
/// is longer than the set it fills.
fn build_seed(fixture: &Fixture) -> Result<RunSeed, FixtureError> {
    Ok(RunSeed {
        id: RunId::new(Digest::of_bytes(fixture.id.as_bytes()).get()),
        model_turns: fixture.seed.model_turns,
        mutations: fixture.seed.mutations,
        input_tokens: fixture.seed.input_tokens,
        output_tokens: fixture.seed.output_tokens,
        elapsed_ms: fixture.seed.elapsed_ms,
        capabilities: build_capabilities(&fixture.seed)?,
        workflow_version: fixture.seed.workflow_version,
    })
}

/// Build the runtime capability set from the fixture's capability
/// lists.
///
/// # Errors
///
/// Returns [`FixtureError::AssertionFailed`] when a list is longer
/// than the capability set it fills (pins and sensors are proven
/// in-range at parse time).
fn build_capabilities(seed: &SeedSpec) -> Result<Capabilities, FixtureError> {
    let mut capabilities = Capabilities::empty();
    capabilities.read_count = fill_set(&seed.read_pins, &mut capabilities.read_pins)?;
    capabilities.write_count = fill_set(&seed.write_pins, &mut capabilities.write_pins)?;
    capabilities.sensor_count = fill_set(&seed.sensors, &mut capabilities.sensors)?;
    capabilities.allow_timer = seed.allow_timer;
    capabilities.allow_status = seed.allow_status;
    Ok(capabilities)
}

/// Copy a fixture capability list into its fixed-size set, returning
/// the valid count.
///
/// # Errors
///
/// Returns [`FixtureError::AssertionFailed`] when the list is longer
/// than the set.
fn fill_set<const N: usize>(list: &[u8], set: &mut [u8; N]) -> Result<u8, FixtureError> {
    if list.len() > N {
        return Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: "a capability list is longer than the capability set it fills".to_owned(),
        });
    }
    for (slot, value) in set.iter_mut().zip(list.iter()) {
        *slot = *value;
    }
    u8::try_from(list.len()).map_err(|_| FixtureError::AssertionFailed {
        // HOST-ONLY (E0/E1)
        detail: "a capability list length does not fit in a u8".to_owned(),
    })
}

/// A validated pin number as a [`Pin`]; the fixture parser already
/// rejected anything outside `0..=7`, so this is unreachable.
fn must_pin(pin: u8) -> Result<Pin, FixtureError> {
    Pin::new(pin).map_err(|_| FixtureError::AssertionFailed {
        // HOST-ONLY (E0/E1)
        detail: format!("pin {pin} is out of range 0..=7"),
    })
}

/// Build the fake device and the transient-fault plan.
///
/// Explicit pins get their direction; every shipped fixture starts its
/// pins low, and an initially-high pin fails closed here because
/// [`FakeDevice`] exposes no initial-level setter (runtime API gap).
/// Transient faults install as `(tool filter, resource)` pairs per
/// SPEC §17.7: the pin for GPIO tools, the sensor id for
/// `sensor_sample_read`, 0 for the timer and status tools. Two faults
/// whose targets overlap (same resource, non-disjoint filters) fail
/// closed because consumption is first-match, and a filter-less fault
/// on a sensor/clock resource fails closed because the plan cannot
/// tell resource 1 the sensor from resource 1 the pin (runtime API
/// gap).
///
/// # Errors
///
/// Returns [`FixtureError::AssertionFailed`] naming the gap when the
/// fixture needs device or fault-plan surface the runtime does not
/// expose.
fn build_device(spec: &DeviceSpec) -> Result<(FakeDevice, FaultPlan), FixtureError> {
    let mut device = FakeDevice::new();
    // Unlisted pins default to input/low per the fixture semantics, so
    // set every pin to input before applying explicit directions.
    for pin in 0..8 {
        device.set_direction(must_pin(pin)?, Direction::Input);
    }
    for pin in &spec.pins {
        let as_pin = must_pin(pin.pin)?;
        device.set_direction(as_pin, pin.direction);
        if pin.level_high {
            return Err(FixtureError::AssertionFailed {
                // HOST-ONLY (E0/E1)
                detail: format!(
                    "pin {} must start high, but FakeDevice exposes no initial-level setter",
                    pin.pin
                ),
            });
        }
    }
    let mut faults = FaultPlan::new();
    // HOST-ONLY (E0/E1)
    let mut transient_targets: Vec<(Option<u8>, u8)> = Vec::new();
    for fault in &spec.faults {
        match fault {
            DeviceFault::Transient {
                tool,
                resource,
                failures,
            } => {
                let tool_id = tool_filter_id(tool.as_deref())?;
                let resource_id = match resource {
                    FaultResource::Pin(pin) => *pin,
                    FaultResource::Sensor(sensor) => *sensor,
                    FaultResource::Clock => 0,
                };
                // Two transient faults conflict when they name the
                // same resource and their tool filters are not
                // disjoint: a filter-less fault overlaps every filter.
                // Overlapping plans make consumption order-dependent,
                // so they fail closed.
                let overlaps = transient_targets.iter().any(|(other_tool, other)| {
                    other == &resource_id
                        && (other_tool.is_none() || tool_id.is_none() || other_tool == &tool_id)
                });
                if overlaps {
                    return Err(FixtureError::AssertionFailed {
                        // HOST-ONLY (E0/E1)
                        detail: format!(
                            "two transient faults overlap on resource {resource_id}; \
                             the fault plan consumes the first matching entry"
                        ),
                    });
                }
                transient_targets.push((tool_id, resource_id));
                // A filter-less fault matches any tool, so its
                // resource must be a pin (see the gap note above).
                if let Some(id) = tool_id {
                    faults.fail_transient_for(id, resource_id, *failures);
                } else {
                    let FaultResource::Pin(pin) = resource else {
                        return Err(FixtureError::AssertionFailed {
                            // HOST-ONLY (E0/E1)
                            detail: "a transient fault without a tool filter targets a sensor \
                                     or timer resource, which the fault plan cannot tell apart \
                                     from the same-numbered pin (runtime API gap)"
                                .to_owned(),
                        });
                    };
                    faults.fail_transient(must_pin(*pin)?, *failures);
                }
            }
            DeviceFault::Stuck {
                tool: _,
                pin,
                level_high,
            } => {
                device.set_stuck(
                    must_pin(*pin)?,
                    Some(if *level_high { Level::High } else { Level::Low }),
                );
            }
        }
    }
    Ok((device, faults))
}

/// Resolve an optional fault tool filter to its catalog id. The
/// fixture parser already rejected unknown names, so a miss here is
/// unreachable — it fails closed anyway.
fn tool_filter_id(tool: Option<&str>) -> Result<Option<u8>, FixtureError> {
    tool.map(|name| {
        esper_protocol::contract::lookup_by_name(name)
            .map(|entry| entry.id)
            .ok_or_else(|| FixtureError::AssertionFailed {
                // HOST-ONLY (E0/E1)
                detail: format!("fault tool `{name}` is not in the E2 catalog"),
            })
    })
    .transpose()
}

/// Queue the typed inputs in `after_decision` order; the engine takes
/// them in order at each `AwaitInput`.
fn build_inputs(fixture: &Fixture) -> InputPlan {
    // HOST-ONLY (E0/E1)
    let payloads: Vec<Vec<u8>> = fixture
        .input_events
        .iter()
        .map(|event| event.input.render().into_bytes())
        .collect();
    InputPlan::new(payloads)
}

/// The single crash point the plan may name. [`drive_run`] injects one
/// point once, so a longer plan fails closed (runtime API gap).
fn crash_point(plan: &[CrashEntry]) -> Result<Option<esper_runtime::CrashPoint>, FixtureError> {
    match plan {
        [] => Ok(None),
        [entry] if entry.times == 1 => Ok(Some(entry.point)),
        [entry] => Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: format!(
                "crash plan fires {:?} {} times; the driver injects one crash point once",
                entry.point, entry.times
            ),
        }),
        _ => Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: format!(
                "crash plan holds {} entries; the driver accepts a single crash point",
                plan.len()
            ),
        }),
    }
}

/// Assert the remaining budgets equal the fixture's exact expectation.
fn assert_budgets(fixture: &Fixture, trace: &RunTrace) -> Result<(), FixtureError> {
    if trace.turns_remaining() != fixture.expected.model_turns_remaining {
        return Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: format!(
                "model turns remaining: expected {}, the run has {}",
                fixture.expected.model_turns_remaining,
                trace.turns_remaining()
            ),
        });
    }
    if trace.mutations_remaining() != fixture.expected.mutations_remaining {
        return Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E0/E1)
            detail: format!(
                "mutations remaining: expected {}, the run has {}",
                fixture.expected.mutations_remaining,
                trace.mutations_remaining()
            ),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{build_capabilities, build_seed, crash_point};
    use crate::fixture::{CrashEntry, SeedSpec, parse_fixture};
    use esper_runtime::CrashPoint;

    #[test]
    fn capabilities_fill_the_fixed_sets() {
        let seed = SeedSpec {
            workflow_version: 1,
            model_turns: 1,
            mutations: 1,
            input_tokens: 1,
            output_tokens: 1,
            elapsed_ms: 1,
            // HOST-ONLY (E0/E1)
            read_pins: vec![0, 4],
            write_pins: vec![7],
            sensors: vec![1, 3],
            allow_timer: true,
            allow_status: false,
        };
        let capabilities = build_capabilities(&seed).expect("in-range lists fill the sets");
        assert_eq!(&capabilities.read_pins[..2], &[0, 4]);
        assert_eq!(capabilities.read_count, 2);
        assert_eq!(&capabilities.write_pins[..1], &[7]);
        assert_eq!(capabilities.write_count, 1);
        assert_eq!(&capabilities.sensors[..2], &[1, 3]);
        assert_eq!(capabilities.sensor_count, 2);
        assert!(capabilities.allow_timer);
        assert!(!capabilities.allow_status);
    }

    #[test]
    fn overlong_capability_lists_fail_closed() {
        let seed = SeedSpec {
            workflow_version: 1,
            model_turns: 1,
            mutations: 1,
            input_tokens: 1,
            output_tokens: 1,
            elapsed_ms: 1,
            // HOST-ONLY (E0/E1)
            read_pins: vec![0; 9],
            write_pins: Vec::new(),
            sensors: Vec::new(),
            allow_timer: false,
            allow_status: false,
        };
        assert!(build_capabilities(&seed).is_err());
    }

    #[test]
    fn run_id_is_deterministic_per_fixture() {
        let a = parse_fixture(r#"{"id":"x","title":"t","spec_version":1,
            "run_seed":{"workflow_version":1,
             "budget":{"model_turns":1,"mutations":1,"input_tokens":1,"output_tokens":1,"elapsed_ms":1,"radio_bytes":0},
             "capabilities":{"read_pins":[],"write_pins":[]}},
            "script":["FINISH {\"status\": \"completed\", \"summary\": \"s\"}"],
            "device":{"pins":{},"faults":[]},"input_events":[],"crash_plan":[],
            "expected":{"terminal_status":"Completed","trace":[],
             "budget_remaining":{"model_turns":0,"mutations":1},
             "invariants":[],"forbidden":[]}}"#)
        .expect("test fixture parses");
        let first = build_seed(&a).expect("seed builds").id.get();
        let second = build_seed(&a).expect("seed builds").id.get();
        assert_eq!(first, second);
    }

    #[test]
    fn crash_plan_longer_than_one_fails_closed() {
        let plan = [
            CrashEntry {
                point: CrashPoint::ToolIntent,
                times: 1,
            },
            CrashEntry {
                point: CrashPoint::AwaitInput,
                times: 1,
            },
        ];
        assert!(crash_point(&plan).is_err());
        let repeated = [CrashEntry {
            point: CrashPoint::ToolIntent,
            times: 2,
        }];
        assert!(crash_point(&repeated).is_err());
        assert!(crash_point(&[]).expect("empty plan").is_none());
    }
}
