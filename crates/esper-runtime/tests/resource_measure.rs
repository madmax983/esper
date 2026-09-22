//! E3 host resource-measurement harness.
//!
//! This test measures, it does not gate: every figure prints to stdout
//! (run with `-- --nocapture`) and only generous bounds are asserted, so
//! timing noise on a shared machine can never fail the suite. The numbers
//! feed SPEC §18.8 (measured-host rows) and the ESP32-S3 projection table.
//!
//! The record/replay procedure mirrors SPEC §18.9: fixture
//! `a-success-read-write-verify-finish` runs under a recording
//! [`ScriptedBackend`], the observed `(prompt, line)` pairs distill into a
//! [`DistillEntry`] table, and the fixture runs again under
//! [`TinyBackend`]. Byte-identical replay is asserted on the terminal
//! status and the measured token totals.
//!
//! Host-only: this file uses `std::time::Instant`, reads a fixture file,
//! and allocates. Nothing here ships to the target.

#![cfg(feature = "host")]

use std::collections::HashSet;
use std::time::Instant;

use esper_core::ids::{Digest, Pin, RunId};
use esper_core::registry::Capabilities;
use esper_core::state::TerminalStatus;
use esper_eval::fixture::{Fixture, parse_fixture};
use esper_runtime::{
    BackendError, BundleId, CrashPoint, Direction, DistillEntry, FakeDevice, FaultPlan,
    InferenceSettings, InputPlan, Journal, ModelBackend, OUTPUT_CAP, PROMPT_CAP, RunSeed, RunTrace,
    ScriptedBackend, TINY_PARAMS_BYTES, TinyBackend, TokenUsage, drive_run, fingerprint_prompt,
    fnv1a64,
};

/// The fixture the harness replays: read pin 4, write it high, verify,
/// finish. Three script lines, one injected crash (`cp_tool_intent`).
const FIXTURE_A: &str =
    include_str!("../../../spec/trajectories/a-success-read-write-verify-finish.json");

/// A [`ModelBackend`] wrapper that records every `(prompt, line)` pair
/// the engine produces, so the record phase of SPEC §18.9 can distill
/// the observed prompts into a [`DistillEntry`] table.
struct RecordingBackend {
    inner: ScriptedBackend,
    prompts: Vec<Vec<u8>>,
    lines: Vec<Vec<u8>>,
}

impl RecordingBackend {
    fn new(lines: Vec<Vec<u8>>) -> Self {
        Self {
            inner: ScriptedBackend::new(lines, InferenceSettings::default_settings()),
            prompts: Vec::new(),
            lines: Vec::new(),
        }
    }
}

impl ModelBackend for RecordingBackend {
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        let written = self.inner.infer(prompt, out)?;
        self.prompts.push(prompt.to_vec());
        self.lines.push(out[..written].to_vec());
        Ok(written)
    }

    fn bundle_id(&self) -> BundleId {
        self.inner.bundle_id()
    }

    fn last_usage(&self) -> TokenUsage {
        self.inner.last_usage()
    }

    fn unemit(&mut self) {
        self.inner.unemit();
        // The emission never committed (crash before the decision
        // commit): drop its recording too, so the distill table never
        // holds an uncommitted line. Post-crash re-inference records
        // the prompt exactly once.
        self.prompts.pop();
        self.lines.pop();
    }
}

/// The fixture-driven world, built the way `esper-eval`'s runner builds
/// it. The seed is built per run because the run binds the backend's
/// [`BundleId`].
struct Harness {
    fixture: Fixture,
    crash: Option<CrashPoint>,
    capabilities: Capabilities,
}

fn build_harness(fixture: Fixture) -> Harness {
    let crash = match fixture.crash_plan.as_slice() {
        [] => None,
        [entry] => Some(entry.point),
        plan => panic!(
            "the harness supports one crash point, the fixture names {}",
            plan.len()
        ),
    };
    let seed = &fixture.seed;
    let mut capabilities = Capabilities::empty();
    capabilities.read_count = fill_set(&seed.read_pins, &mut capabilities.read_pins);
    capabilities.write_count = fill_set(&seed.write_pins, &mut capabilities.write_pins);
    capabilities.sensor_count = fill_set(&seed.sensors, &mut capabilities.sensors);
    capabilities.allow_timer = seed.allow_timer;
    capabilities.allow_status = seed.allow_status;
    Harness {
        fixture,
        crash,
        capabilities,
    }
}

fn fill_set<const N: usize>(list: &[u8], set: &mut [u8; N]) -> u8 {
    assert!(
        list.len() <= N,
        "a capability list is longer than its fixed set"
    );
    for (slot, value) in set.iter_mut().zip(list.iter()) {
        *slot = *value;
    }
    u8::try_from(list.len()).expect("capability lists are short")
}

/// Build the device exactly the way the eval runner does: every pin
/// input first, then the fixture's explicit directions.
fn build_device(fixture: &Fixture) -> FakeDevice {
    let mut device = FakeDevice::new();
    for number in 0..8u8 {
        let pin = Pin::new(number).expect("pin numbers 0..8 are in range");
        device.set_direction(pin, Direction::Input);
    }
    for pin in &fixture.device.pins {
        let pin_number = Pin::new(pin.pin).expect("the fixture parser proved pins in range");
        device.set_direction(pin_number, pin.direction);
    }
    device
}

fn seed_for(harness: &Harness, model_bundle: u64) -> RunSeed {
    let fixture = &harness.fixture;
    RunSeed {
        id: RunId::new(Digest::of_bytes(fixture.id.as_bytes()).get()),
        model_turns: fixture.seed.model_turns,
        mutations: fixture.seed.mutations,
        input_tokens: fixture.seed.input_tokens,
        output_tokens: fixture.seed.output_tokens,
        elapsed_ms: fixture.seed.elapsed_ms,
        capabilities: harness.capabilities,
        workflow_version: fixture.seed.workflow_version,
        model_bundle,
        inference: InferenceSettings::default_settings(),
    }
}

/// Drive the fixture under this backend and return its trace.
fn drive_fixture(harness: &Harness, model: &mut dyn ModelBackend) -> RunTrace {
    let seed = seed_for(harness, model.bundle_id().0);
    let mut journal = Journal::new();
    let mut device = build_device(&harness.fixture);
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    drive_run(
        &seed,
        &mut journal,
        model,
        &mut device,
        &mut faults,
        &mut inputs,
        harness.crash,
    )
    .expect("fixture a drives to a terminal result")
}

/// Distill the recorded `(prompt, line)` pairs into a
/// [`DistillEntry`] table. The line bytes leak as `'static` — a
/// host-only test convenience, never a target pattern. Crash recovery
/// re-infers prompts, so pairs are deduplicated by fingerprint.
fn distill(pairs: &[(Vec<u8>, Vec<u8>)]) -> Vec<DistillEntry<'static>> {
    let mut seen = HashSet::new();
    let mut entries = Vec::new();
    for (prompt, line) in pairs {
        let prompt_fp = fingerprint_prompt(prompt);
        if !seen.insert(prompt_fp) {
            continue;
        }
        let line: &'static [u8] = Box::leak(line.clone().into_boxed_slice());
        entries.push(DistillEntry {
            prompt_fp,
            line_hash: fnv1a64(line),
            line,
        });
    }
    entries
}

/// What [`record`] returns: the harness, the scripted run's trace, and
/// the observed `(prompt, line)` pairs.
type Recorded = (Harness, RunTrace, Vec<(Vec<u8>, Vec<u8>)>);

/// Record the fixture under the scripted backend: the trace plus the
/// distilled table storage.
fn record(fixture_a: &str) -> Recorded {
    let fixture = parse_fixture(fixture_a).expect("fixture a parses");
    let harness = build_harness(fixture);
    let mut recorder = RecordingBackend::new(harness.fixture.script.clone());
    let trace = drive_fixture(&harness, &mut recorder);
    let pairs: Vec<(Vec<u8>, Vec<u8>)> = recorder
        .prompts
        .iter()
        .zip(recorder.lines.iter())
        .map(|(prompt, line)| (prompt.clone(), line.clone()))
        .collect();
    (harness, trace, pairs)
}

#[test]
fn tiny_backend_latency_and_sizes() {
    let (_harness, _trace, pairs) = record(FIXTURE_A);
    assert!(!pairs.is_empty(), "the run must infer at least once");

    // The distillation table over the recorded lines: a few
    // representative prompts, the honest stand-in for the tiny model.
    let entries = distill(&pairs);
    let distinct = entries.len();
    let table: &'static [DistillEntry<'static>] = Box::leak(entries.into_boxed_slice());
    let mut backend = TinyBackend::new(table);
    println!(
        "distilled {distinct} table entries from {} recorded pairs",
        pairs.len()
    );

    // Measure one representative inference: fingerprint plus lookup.
    let (prompt, _line) = &pairs[0];
    let iterations: u32 = 1000;
    let mut worst: u128 = 0;
    let mut out = [0u8; OUTPUT_CAP];
    let start = Instant::now();
    for _ in 0..iterations {
        let iter_start = Instant::now();
        let _fingerprint = fingerprint_prompt(prompt);
        backend
            .infer(prompt, &mut out)
            .expect("the recorded prompt is in the table");
        let elapsed = iter_start.elapsed().as_micros();
        worst = worst.max(elapsed);
    }
    let elapsed = start.elapsed();
    let mean = elapsed.as_micros() / u128::from(iterations);
    let mean_ns = elapsed.as_nanos() / u128::from(iterations);
    println!(
        "tiny infer latency over {iterations} iterations: mean {mean} µs ({mean_ns} ns), max {worst} µs"
    );
    println!(
        "sizes: size_of::<TinyBackend>() = {} B, TINY_PARAMS_BYTES = {TINY_PARAMS_BYTES} B, \
         PROMPT_CAP = {PROMPT_CAP} B, OUTPUT_CAP = {OUTPUT_CAP} B",
        std::mem::size_of::<TinyBackend<'static>>(),
    );
    // Generous bound: the lookup is FNV over the prompt plus a linear
    // table scan; 10 ms mean leaves three orders of magnitude of
    // headroom for a host-class machine.
    assert!(
        mean < 10_000,
        "tiny infer mean latency {mean} µs exceeds the 10 ms generous bound"
    );
}

#[test]
fn fixture_a_replays_under_tiny_backend() {
    let (harness, scripted_trace, pairs) = record(FIXTURE_A);
    let entries = distill(&pairs);
    let table: &'static [DistillEntry<'static>] = Box::leak(entries.into_boxed_slice());
    let mut tiny = TinyBackend::new(table);
    let tiny_trace = drive_fixture(&harness, &mut tiny);

    assert_eq!(
        scripted_trace.terminal_status(),
        Some(TerminalStatus::Completed),
        "the scripted run must complete"
    );
    assert_eq!(
        tiny_trace.terminal_status(),
        Some(TerminalStatus::Completed),
        "the tiny run must replay and complete"
    );
    println!(
        "scripted run: input_tokens_used = {}, output_tokens_used = {}",
        scripted_trace.input_tokens_used(),
        scripted_trace.output_tokens_used()
    );
    println!(
        "tiny run:     input_tokens_used = {}, output_tokens_used = {}",
        tiny_trace.input_tokens_used(),
        tiny_trace.output_tokens_used()
    );
    assert_eq!(
        scripted_trace.input_tokens_used(),
        tiny_trace.input_tokens_used(),
        "identical prompts in means identical measured input tokens"
    );
    assert_eq!(
        scripted_trace.output_tokens_used(),
        tiny_trace.output_tokens_used(),
        "identical lines out means identical measured output tokens"
    );
}
