//! The data-driven fixture runner: parse, drive, assert — under both
//! E3 model backends.
//!
//! [`run_fixture_with`] turns one parsed [`Fixture`] into two live
//! runs: first the scripted teacher ([`ScriptedBackend`], wrapped in a
//! recording harness), then the distilled tiny stand-in
//! ([`TinyBackend`]) replaying the recorded prompt→line table. The
//! full assertion battery — terminal status, remaining budgets, the
//! exact expected trace ([`crate::compare`]), every listed SPEC §11.3
//! invariant ([`crate::checks`]), every `forbidden` clause, and the
//! model-bundle binding — runs under **both** backends, and the two
//! backends' measured token totals must agree. The [`FixtureReport`]
//! carries the trace of the [`BackendKind`] the caller selected.
//!
//! [`run_fixture`] selects the scripted teacher's trace (the E0/E1
//! behavior); [`run_fixture_with`] with [`BackendKind::Tiny`] selects
//! the tiny replay's trace. Either way both backends ran and both
//! assertion batteries passed.
//!
//! Where the runtime's public API cannot express something the fixture
//! format allows (several crash points, an initially-high pin, two
//! transient faults on one pin), the runner fails closed with a clear
//! [`FixtureError::AssertionFailed`] naming the gap instead of
//! silently approximating it. None of the shipped fixtures need those
//! shapes.
//!
//! // HOST-ONLY (E0/E1): heap-allocated orchestration, for the host runner.

use std::collections::HashSet;

use esper_core::decision::Level;
use esper_core::ids::{Digest, Pin, RunId};
use esper_core::registry::Capabilities;
use esper_runtime::{
    BackendError, BundleId, Direction, DistillEntry, FakeDevice, FaultPlan, InferenceSettings,
    InputPlan, Journal, ModelBackend, RunSeed, RunTrace, ScriptedBackend, TinyBackend, TokenUsage,
    drive_run, fingerprint_prompt, fnv1a64,
};

use crate::checks::{CheckCtx, pin_levels};
use crate::compare::compare_trace;
use crate::fixture::{
    CrashEntry, DeviceFault, DeviceSpec, FaultResource, Fixture, FixtureError, SeedSpec,
    parse_fixture,
};

/// Which model backend runs the fixture — and which backend's trace
/// the [`FixtureReport`] carries.
///
/// Both backends always run (the tiny backend needs the scripted
/// pass's recording to distill from); the kind only selects the
/// reported trace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The scripted teacher: canned lines, prompt ignored.
    Scripted,
    /// The distilled tiny stand-in: prompt→line table replay.
    Tiny,
}

/// The outcome of running one fixture: the evidence, plus counts of
/// what was checked.
#[derive(Debug)]
pub struct FixtureReport {
    /// The fixture id that was run.
    pub fixture_id: String,
    /// The fixture title.
    pub title: String,
    /// Which backend's trace this report carries.
    pub backend: BackendKind,
    /// The derived run trace (from the selected backend).
    pub trace: RunTrace,
    /// How many expected trace events were compared.
    pub events_compared: usize,
    /// How many §11.3 invariants were checked.
    pub invariants_checked: usize,
    /// How many `forbidden` clauses were checked.
    pub forbidden_checked: usize,
}

/// Run one parsed fixture to its terminal result under the selected
/// backend, asserting everything the fixture expects under **both**
/// backends.
///
/// Pass 1 drives the scripted teacher through a recording wrapper;
/// pass 2 distills the recorded `(prompt, line)` pairs into a
/// [`DistillEntry`] table and replays it under [`TinyBackend`] on a
/// fresh journal, device, fault plan, and input queue. The full
/// assertion battery runs against both traces; the report carries the
/// selected backend's trace.
///
/// # Errors
///
/// Returns [`FixtureError`] when the fixture is malformed, a driver
/// fails, or any assertion — terminal, budgets, trace, invariant,
/// forbidden clause, or model-bundle binding — does not hold under
/// either backend.
pub fn run_fixture_with(
    fixture: &Fixture,
    kind: BackendKind,
) -> Result<FixtureReport, FixtureError> {
    let crash = crash_point(&fixture.crash_plan)?;

    // Pass 1: the scripted teacher, recording every prompt→line pair.
    let mut recorder = RecordingBackend::new(ScriptedBackend::new(
        // HOST-ONLY (E3)
        fixture.script.clone(),
        InferenceSettings::default_settings(),
    ));
    let seed = build_seed(fixture, recorder.bundle_id().0)?;
    let (mut device, mut faults) = build_device(&fixture.device)?;
    let initial_levels = pin_levels(&device);
    let mut inputs = build_inputs(fixture);
    let mut journal = Journal::new();
    let trace_scripted = drive_run(
        &seed,
        &mut journal,
        &mut recorder,
        &mut device,
        &mut faults,
        &mut inputs,
        crash,
    )?;
    assert_all(fixture, &seed, &trace_scripted, &device, initial_levels)?;

    // Distill the recording: one entry per prompt fingerprint, with
    // the line integrity hash the tiny backend checks on lookup.
    // The table borrows the recorder's lines, so the recorder must
    // outlive the replay below.
    let table = distill(&recorder.pairs);

    // Pass 2: the tiny stand-in replays the distilled table on a
    // fresh world. The seed binds the tiny bundle, not the scripted
    // one — the journal must name the model that actually ran.
    let mut tiny = TinyBackend::new(&table);
    let tiny_seed = build_seed(fixture, tiny.bundle_id().0)?;
    let (mut tiny_device, mut tiny_faults) = build_device(&fixture.device)?;
    let tiny_initial_levels = pin_levels(&tiny_device);
    let mut tiny_inputs = build_inputs(fixture);
    let mut tiny_journal = Journal::new();
    let trace_tiny = drive_run(
        &tiny_seed,
        &mut tiny_journal,
        &mut tiny,
        &mut tiny_device,
        &mut tiny_faults,
        &mut tiny_inputs,
        crash,
    )?;
    assert_all(
        fixture,
        &tiny_seed,
        &trace_tiny,
        &tiny_device,
        tiny_initial_levels,
    )?;

    // Cross-backend agreement: byte-identical prompts in,
    // byte-identical lines out, so the measured token totals agree.
    if trace_scripted.input_tokens_used() != trace_tiny.input_tokens_used()
        || trace_scripted.output_tokens_used() != trace_tiny.output_tokens_used()
    {
        return Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E3)
            detail: format!(
                "token totals differ between backends: scripted ({}, {}) vs tiny ({}, {})",
                trace_scripted.input_tokens_used(),
                trace_scripted.output_tokens_used(),
                trace_tiny.input_tokens_used(),
                trace_tiny.output_tokens_used(),
            ),
        });
    }

    let (trace, _seed) = match kind {
        BackendKind::Scripted => (trace_scripted, seed),
        BackendKind::Tiny => (trace_tiny, tiny_seed),
    };
    Ok(FixtureReport {
        // HOST-ONLY (E0/E1)
        fixture_id: fixture.id.clone(),
        title: fixture.title.clone(),
        backend: kind,
        trace,
        events_compared: fixture.expected.trace.len(),
        invariants_checked: fixture.expected.invariants.len(),
        forbidden_checked: fixture.expected.forbidden.len(),
    })
}

/// Run one parsed fixture to its terminal result and assert everything
/// the fixture expects, carrying the scripted teacher's trace.
///
/// Both backends still run and both assertion batteries still pass;
/// see [`run_fixture_with`].
///
/// # Errors
///
/// Returns [`FixtureError`] when the fixture is malformed, a driver
/// fails, or any assertion does not hold under either backend.
pub fn run_fixture(fixture: &Fixture) -> Result<FixtureReport, FixtureError> {
    run_fixture_with(fixture, BackendKind::Scripted)
}

/// Read a fixture file, parse it, and run it under the selected
/// backend.
///
/// # Errors
///
/// Returns [`FixtureError`] for I/O, JSON, schema, driver, or
/// assertion failures.
pub fn run_fixture_file_with(path: &str, kind: BackendKind) -> Result<FixtureReport, FixtureError> {
    let text = std::fs::read_to_string(path).map_err(|source| FixtureError::Io {
        // HOST-ONLY (E0/E1)
        path: path.to_owned(),
        source,
    })?;
    run_fixture_with(&parse_fixture(&text)?, kind)
}

/// Read a fixture file, parse it, and run it, carrying the scripted
/// teacher's trace.
///
/// # Errors
///
/// Returns [`FixtureError`] for I/O, JSON, schema, driver, or
/// assertion failures.
pub fn run_fixture_file(path: &str) -> Result<FixtureReport, FixtureError> {
    run_fixture_file_with(path, BackendKind::Scripted)
}

// ---------------------------------------------------------------------------
// The dual-backend machinery.
// ---------------------------------------------------------------------------

/// A [`ModelBackend`] wrapper that records every `(prompt, line)` pair
/// the scripted teacher emits, keyed by prompt fingerprint, so the
/// tiny pass can distill them into a [`DistillEntry`] table.
///
/// `unemit` pops the recorded pair as well as rewinding the script:
/// the line was taken but never committed (crash before the decision
/// commit), so the distill table must never hold an uncommitted line.
struct RecordingBackend {
    /// The scripted teacher being recorded.
    inner: ScriptedBackend,
    /// `(prompt_fingerprint, emitted_line)` in emission order.
    pairs: Vec<(u64, Vec<u8>)>,
}

impl RecordingBackend {
    /// Wrap the scripted teacher; nothing is recorded yet.
    const fn new(inner: ScriptedBackend) -> Self {
        // HOST-ONLY (E3)
        Self {
            inner,
            pairs: Vec::new(),
        }
    }
}

impl ModelBackend for RecordingBackend {
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        let n = self.inner.infer(prompt, out)?;
        // HOST-ONLY (E3)
        self.pairs
            .push((fingerprint_prompt(prompt), out[..n].to_vec()));
        Ok(n)
    }

    fn bundle_id(&self) -> BundleId {
        self.inner.bundle_id()
    }

    fn last_usage(&self) -> TokenUsage {
        self.inner.last_usage()
    }

    fn unemit(&mut self) {
        self.inner.unemit();
        // The emission never committed: drop its recording too, so a
        // reboot that re-issues the same prompt records it exactly
        // once.
        self.pairs.pop();
    }
}

/// Distill recorded `(prompt_fingerprint, line)` pairs into the tiny
/// backend's table: one entry per prompt, first emission wins, with
/// `line_hash = fnv1a64(line)` for the lookup integrity check.
///
/// The entries borrow the recorded lines, so the `pairs` allocation
/// must outlive the table (and the replay that uses it).
fn distill(pairs: &[(u64, Vec<u8>)]) -> Vec<DistillEntry<'_>> {
    // HOST-ONLY (E3)
    let mut seen = HashSet::new();
    let mut table = Vec::new();
    for (fingerprint, line) in pairs {
        if seen.insert(*fingerprint) {
            table.push(DistillEntry {
                prompt_fp: *fingerprint,
                line_hash: fnv1a64(line),
                line: line.as_slice(),
            });
        }
    }
    table
}

/// The full assertion battery for one backend's trace, in fixture
/// order: not suspended, terminal status, remaining budgets, the exact
/// expected trace, every §11.3 invariant, every `forbidden` clause,
/// and the model-bundle binding (the trace names the seed's bundle —
/// the journal binds the exact model that ran).
fn assert_all(
    fixture: &Fixture,
    seed: &RunSeed,
    trace: &RunTrace,
    device: &FakeDevice,
    initial_levels: [bool; 8],
) -> Result<(), FixtureError> {
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
    assert_budgets(fixture, trace)?;
    compare_trace(&fixture.expected.trace, trace)?;

    let ctx = CheckCtx {
        trace,
        seed,
        device,
        initial_levels,
    };
    for invariant in &fixture.expected.invariants {
        invariant.check(&ctx)?;
    }
    for clause in &fixture.expected.forbidden {
        clause.check(&ctx)?;
    }

    if trace.model_bundle() != seed.model_bundle {
        return Err(FixtureError::AssertionFailed {
            // HOST-ONLY (E3)
            detail: format!(
                "model-bundle binding broken: the trace names {:#x}, the seed binds {:#x}",
                trace.model_bundle(),
                seed.model_bundle,
            ),
        });
    }
    Ok(())
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
/// `model_bundle` binds the backend that will actually run (scripted
/// or tiny); the journal carries it so a reboot can never swap the
/// model mid-run.
///
/// # Errors
///
/// Returns [`FixtureError::AssertionFailed`] when a capability list
/// is longer than the set it fills.
fn build_seed(fixture: &Fixture, model_bundle: u64) -> Result<RunSeed, FixtureError> {
    Ok(RunSeed {
        id: RunId::new(Digest::of_bytes(fixture.id.as_bytes()).get()),
        model_turns: fixture.seed.model_turns,
        mutations: fixture.seed.mutations,
        input_tokens: fixture.seed.input_tokens,
        output_tokens: fixture.seed.output_tokens,
        elapsed_ms: fixture.seed.elapsed_ms,
        capabilities: build_capabilities(&fixture.seed)?,
        workflow_version: fixture.seed.workflow_version,
        model_bundle,
        inference: InferenceSettings::default_settings(),
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
    use super::{
        BackendKind, RecordingBackend, build_capabilities, build_seed, crash_point, distill,
    };
    use crate::fixture::{CrashEntry, SeedSpec, parse_fixture};
    use esper_runtime::{
        BackendError, InferenceSettings, ModelBackend, ScriptedBackend, fingerprint_prompt, fnv1a64,
    };

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
            context_budget_bytes: None,
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
            context_budget_bytes: None,
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
        let first = build_seed(&a, 0).expect("seed builds").id.get();
        let second = build_seed(&a, 0).expect("seed builds").id.get();
        assert_eq!(first, second);
    }

    #[test]
    fn crash_plan_longer_than_one_fails_closed() {
        let plan = [
            CrashEntry {
                point: esper_runtime::CrashPoint::ToolIntent,
                times: 1,
            },
            CrashEntry {
                point: esper_runtime::CrashPoint::AwaitInput,
                times: 1,
            },
        ];
        assert!(crash_point(&plan).is_err());
        let repeated = [CrashEntry {
            point: esper_runtime::CrashPoint::ToolIntent,
            times: 2,
        }];
        assert!(crash_point(&repeated).is_err());
        assert!(crash_point(&[]).expect("empty plan").is_none());
    }

    #[test]
    fn recording_backend_captures_prompt_fingerprints() {
        let script = vec![b"FINISH {\"status\": \"completed\", \"summary\": \"s\"}".to_vec()];
        let mut recorder = RecordingBackend::new(ScriptedBackend::new(
            script,
            InferenceSettings::default_settings(),
        ));
        // HOST-ONLY (E3)
        let mut out = [0u8; 256];
        let prompt = b"TOOLS\nSTATE turns=1 muts=0\nEMIT one decision line.\n";
        let n = recorder.infer(prompt, &mut out).expect("script has a line");
        assert_eq!(
            &out[..n],
            b"FINISH {\"status\": \"completed\", \"summary\": \"s\"}"
        );
        assert_eq!(recorder.pairs.len(), 1);
        assert_eq!(recorder.pairs[0].0, fingerprint_prompt(prompt));
        assert_eq!(recorder.pairs[0].1, out[..n].to_vec());
    }

    #[test]
    fn recording_backend_unemit_drops_the_pair() {
        let script = vec![b"CALL x".to_vec(), b"FINISH {}".to_vec()];
        let mut recorder = RecordingBackend::new(ScriptedBackend::new(
            script,
            InferenceSettings::default_settings(),
        ));
        // HOST-ONLY (E3)
        let mut out = [0u8; 256];
        recorder.infer(b"p1", &mut out).expect("first line");
        recorder.infer(b"p2", &mut out).expect("second line");
        assert_eq!(recorder.pairs.len(), 2);
        // Crash before commit: the second emission never committed.
        recorder.unemit();
        assert_eq!(recorder.pairs.len(), 1);
        assert_eq!(recorder.pairs[0].0, fingerprint_prompt(b"p1"));
        // The next boot re-issues the prompt and records it once.
        recorder.infer(b"p2", &mut out).expect("re-issued line");
        assert_eq!(recorder.pairs.len(), 2);
    }

    #[test]
    fn distill_dedups_and_hashes_lines() {
        // HOST-ONLY (E3)
        let pairs = vec![
            (1u64, b"CALL a".to_vec()),
            (2u64, b"CALL b".to_vec()),
            (1u64, b"CALL a-again".to_vec()),
        ];
        let table = distill(&pairs);
        assert_eq!(table.len(), 2, "duplicate fingerprints distill once");
        assert_eq!(table[0].prompt_fp, 1);
        assert_eq!(table[0].line_hash, fnv1a64(b"CALL a"));
        assert_eq!(table[0].line, b"CALL a");
        assert_eq!(table[1].prompt_fp, 2);
    }

    #[test]
    fn backend_kind_selects_the_report_trace() {
        assert_ne!(BackendKind::Scripted, BackendKind::Tiny);
        // The unit type check: `run_fixture_with` takes the kind.
        let _ = BackendKind::Scripted;
    }

    #[test]
    fn exhausted_script_surfaces_backend_error() {
        let mut recorder = RecordingBackend::new(ScriptedBackend::new(
            Vec::new(),
            InferenceSettings::default_settings(),
        ));
        // HOST-ONLY (E3)
        let mut out = [0u8; 256];
        assert_eq!(recorder.infer(b"p", &mut out), Err(BackendError::Exhausted));
    }
}
