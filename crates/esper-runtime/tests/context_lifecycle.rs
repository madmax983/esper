//! E4 context lifecycle: masking, rollover, lineage, reboot, corruption.
//!
//! These tests drive the runtime through `drive_segment` (one segment
//! per driver lifetime) and prove the E4 wiring: secrets are masked at
//! the observation boundary, the 80% context trigger folds the segment
//! and continues as a child with narrowed budgets, failed paths are
//! never re-dispatched, a reboot after rollover resumes and finishes,
//! and a corrupt snapshot fails closed.

use esper_core::ids::RunId;
use esper_core::snapshot::decode;
use esper_core::state::TerminalStatus;
use esper_runtime::{
    Capabilities, CrashPoint, FakeDevice, FaultPlan, InferenceSettings, InputPlan, Journal,
    ModelBackend, RunSeed, ScriptedBackend, SegmentOutcome, drive_segment,
};

/// Build the scripted backend for one run.
fn make_backend(script: Vec<&str>) -> ScriptedBackend {
    ScriptedBackend::new(
        script
            .into_iter()
            .map(|line| line.as_bytes().to_vec())
            .collect(),
        InferenceSettings::default_settings(),
    )
}

/// Build a default seed with this run id, bound to the backend.
fn seed(backend: &ScriptedBackend, id: u64) -> RunSeed {
    RunSeed {
        id: RunId::new(id),
        model_bundle: backend.bundle_id().0,
        ..RunSeed::default_slice()
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

fn pin(n: u8) -> esper_core::ids::Pin {
    esper_core::ids::Pin::new(n).expect("bad pin in test")
}

/// Drive one segment; panic on engine error (a `Halt::Crash` never
/// escapes `drive_segment`).
#[allow(clippy::too_many_arguments)]
fn segment(
    seed: &RunSeed,
    journal: &mut Journal,
    backend: &mut ScriptedBackend,
    device: &mut FakeDevice,
    faults: &mut FaultPlan,
    inputs: &mut InputPlan,
    crash: Option<CrashPoint>,
    handoff: Option<esper_runtime::RolloverHandoff>,
) -> SegmentOutcome {
    drive_segment(
        seed, journal, backend, device, faults, inputs, crash, handoff,
    )
    .expect("segment failed")
}

/// Masking through the public path: the trace carries one digest per
/// committed observation and the prompt-byte meter runs. (The
/// byte-level proof — a `sk-live-...` secret plus an email redacted
/// before the journal, the prompt, and the cursor — lives in
/// `engine::tests::masking_boundary_redacts_secret_and_pii_before_journal_and_prompt`.)
#[test]
fn masking_trace_carries_per_observation_digests_and_prompt_bytes() {
    let mut backend = make_backend(vec![
        "CALL gpio_pin_read {\"pin\": 4}",
        "FINISH {\"status\": \"completed\", \"summary\": \"done\"}",
    ]);
    let seed = seed(&backend, 1);
    let mut journal = Journal::new();
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());
    let outcome = segment(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
        None,
    );
    let SegmentOutcome::Completed(trace) = outcome else {
        panic!("expected the run to complete");
    };
    // One committed observation, no secret in the canned outcome.
    assert_eq!(trace.secret_digests(), &[0]);
    // The meter ran: prompts were built.
    assert!(trace.prompt_bytes_used() > 0, "prompt bytes were metered");
}

/// Rollover at 80%: the trigger fires mid-task, the lineage names the
/// parent run and the cut frame, the child gets the remaining budgets
/// (never widened), and the child finishes the task.
#[test]
fn rollover_at_80_percent_folds_and_continues_as_child() {
    let mut backend = make_backend(vec![
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "FINISH {\"status\": \"completed\", \"summary\": \"done\"}",
    ]);
    let seed = RunSeed {
        context_budget_bytes: Some(10000),
        model_turns: 20,
        ..seed(&backend, 2)
    };
    let mut journal = Journal::new();
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());

    let outcome = segment(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
        None,
    );
    let SegmentOutcome::Rollover(trace, handoff) = outcome else {
        panic!("expected a rollover at 80% of the context budget");
    };
    // The trigger fired at or past 80% of the 10000-byte budget.
    assert!(
        trace.prompt_bytes_used() >= 8000,
        "prompt bytes {} below the 80% trigger",
        trace.prompt_bytes_used()
    );
    // The parent segment is a root: no lineage of its own.
    assert!(trace.lineage().is_none());
    // The lineage binds the child to this run, at the cut frame, with
    // the remaining budgets.
    assert_eq!(handoff.lineage.parent_run, seed.id);
    assert!(
        handoff.lineage.budgets_remaining.model_turns < seed.model_turns,
        "the child must not recover spent turns"
    );
    assert_eq!(
        handoff.prompt_bytes,
        trace.prompt_bytes_used(),
        "the handoff carries the segment meter"
    );

    // The child seed narrows the consumables (remaining budgets, never
    // widened) and links the parent; the context budget is per-segment
    // capacity, so the child inherits the full budget, not the dregs.
    let child = handoff.child_seed(&seed);
    assert!(child.parent.is_some(), "the child names its parent");
    assert_eq!(
        child.context_budget_bytes,
        Some(10000),
        "the child's context budget is the same per-segment capacity"
    );
    assert_eq!(
        child.model_turns,
        handoff.lineage.budgets_remaining.model_turns
    );

    // The child boots from the verified handoff on a fresh journal and
    // finishes the task.
    let mut child_journal = Journal::new();
    let outcome = segment(
        &child,
        &mut child_journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
        Some(handoff.clone()),
    );
    let SegmentOutcome::Completed(child_trace) = outcome else {
        panic!("expected the child to complete");
    };
    assert_eq!(
        child_trace.terminal_status(),
        Some(TerminalStatus::Completed)
    );
    assert_eq!(
        child_trace.lineage(),
        Some(handoff.lineage),
        "the child trace names the continuation"
    );
}

/// A failed path survives the rollover in the fold, and the child
/// never re-dispatches it: the authorize gate refuses the identical
/// call before it burns a turn.
#[test]
fn failed_path_survives_rollover_and_is_never_redispatched() {
    let write_high = "CALL gpio_pin_write {\"pin\": 5, \"level\": \"high\"}";
    let mut backend = make_backend(vec![
        write_high,
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
    ]);
    let seed = RunSeed {
        capabilities: write_only_pin_5(),
        context_budget_bytes: Some(10000),
        ..seed(&backend, 3)
    };
    // Pin 5 is stuck low: the write verifies failed — a failed path.
    let mut device = FakeDevice::new();
    device.set_stuck(pin(5), Some(esper_core::decision::Level::Low));
    let mut journal = Journal::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());

    let outcome = segment(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
        None,
    );
    let SegmentOutcome::Rollover(_, handoff) = outcome else {
        panic!("expected a rollover after the failed write");
    };
    // The fold carries the ruled-out path.
    let state = decode(&handoff.snapshot).expect("handoff snapshot verifies");
    assert!(
        !state.failed_paths().is_empty(),
        "the fold must carry the failed path"
    );

    // The child retries the identical failed call: the gate refuses it
    // before dispatch — no second ToolIntent, no burned turn.
    let mut child_backend = make_backend(vec![write_high]);
    let child_seed = RunSeed {
        capabilities: write_only_pin_5(),
        model_bundle: child_backend.bundle_id().0,
        ..handoff.child_seed(&seed)
    };
    // The child seed must bind its own backend bundle.
    let mut child_journal = Journal::new();
    let outcome = segment(
        &child_seed,
        &mut child_journal,
        &mut child_backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
        Some(handoff),
    );
    let SegmentOutcome::Completed(child_trace) = outcome else {
        panic!("expected the child to terminate");
    };
    assert_eq!(child_trace.terminal_status(), Some(TerminalStatus::Stuck));
    assert_eq!(
        child_trace.reason(),
        Some(b"failed_path_ruled_out".as_slice()),
        "the fold's ruled-out path stopped the re-dispatch"
    );
    // No tool intent committed in the child: the refusal happened at
    // authorization, before dispatch.
    let intents = child_journal
        .frames()
        .iter()
        .filter(|frame| matches!(frame, esper_runtime::Frame::ToolIntent { .. }))
        .count();
    assert_eq!(intents, 0, "the failed call was never re-dispatched");
}

/// Reboot after rollover: the child crashes mid-segment, reboots from
/// its own journal (the verified snapshot re-verifies on every boot),
/// and finishes.
#[test]
fn reboot_after_rollover_resumes_and_finishes() {
    let mut backend = make_backend(vec![
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "FINISH {\"status\": \"completed\", \"summary\": \"done\"}",
    ]);
    let seed = RunSeed {
        context_budget_bytes: Some(10000),
        model_turns: 20,
        ..seed(&backend, 4)
    };
    let mut journal = Journal::new();
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());

    let outcome = segment(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
        None,
    );
    let SegmentOutcome::Rollover(_, handoff) = outcome else {
        panic!("expected a rollover");
    };
    let child = handoff.child_seed(&seed);

    // The child crashes after its first physical dispatch: the reboot
    // replays the child's journal, re-verifies the snapshot, and the
    // run finishes.
    let mut child_journal = Journal::new();
    let outcome = segment(
        &child,
        &mut child_journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        Some(CrashPoint::AfterPhysicalBeforeObservation),
        Some(handoff),
    );
    let SegmentOutcome::Completed(child_trace) = outcome else {
        panic!("expected the child to finish after the reboot");
    };
    assert_eq!(
        child_trace.terminal_status(),
        Some(TerminalStatus::Completed)
    );
}

/// A corrupt snapshot fails closed: the child never boots from bytes
/// that do not verify.
#[test]
fn corrupt_snapshot_fails_closed_before_boot() {
    let mut backend = make_backend(vec![
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "CALL gpio_pin_read {\"pin\": 4}",
        "FINISH {\"status\": \"completed\", \"summary\": \"done\"}",
    ]);
    let seed = RunSeed {
        context_budget_bytes: Some(10000),
        ..seed(&backend, 5)
    };
    let mut journal = Journal::new();
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());

    let outcome = segment(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
        None,
    );
    let SegmentOutcome::Rollover(_, mut handoff) = outcome else {
        panic!("expected a rollover");
    };
    let child = handoff.child_seed(&seed);

    // Corrupt a payload byte: the checksum no longer verifies.
    let mid = handoff.snapshot.len() / 2;
    handoff.snapshot[mid] ^= 0xff;
    let mut child_journal = Journal::new();
    let result = drive_segment(
        &child,
        &mut child_journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
        Some(handoff),
    );
    assert!(
        result.is_err(),
        "a corrupt snapshot must fail closed, not boot"
    );
}

/// Multi-rollover chains: every generation restarts its sequence space
/// on a fresh journal, but the device deduplicates on the full
/// `(run, seq)` effect identity — so a child's dispatches never collide
/// with its parent's in the shared device. Before the E4 fix the second
/// generation's first tool call died with `ReplayDiverged` (a
/// `WorldError::EffectArgsMismatch` underneath: same bare sequence,
/// different argument digest). The chain below rolls over repeatedly,
/// each child dispatching real tool calls against the same device, and
/// finishes with its lineage intact.
#[test]
fn multi_rollover_chain_dispatches_without_effect_identity_collision() {
    let mut lines: Vec<&str> = vec!["CALL gpio_pin_read {\"pin\": 4}"; 20];
    lines.push("FINISH {\"status\": \"completed\", \"summary\": \"chain done\"}");
    let mut backend = make_backend(lines);
    let mut device = FakeDevice::new();
    let mut faults = FaultPlan::new();
    let mut inputs = InputPlan::new(Vec::new());

    let mut seed = RunSeed {
        context_budget_bytes: Some(1000),
        model_turns: 60,
        ..seed(&backend, 7)
    };
    let mut handoff: Option<esper_runtime::RolloverHandoff> = None;
    let mut rollovers = 0u32;
    let trace = loop {
        let mut journal = Journal::new();
        let outcome = drive_segment(
            &seed,
            &mut journal,
            &mut backend,
            &mut device,
            &mut faults,
            &mut inputs,
            None,
            handoff.take(),
        )
        .expect("every chained segment must boot and dispatch");
        match outcome {
            SegmentOutcome::Rollover(_, next) => {
                rollovers += 1;
                assert!(
                    rollovers < 30,
                    "the chain should finish, not roll over forever"
                );
                let child = next.child_seed(&seed);
                assert_eq!(
                    child.parent.expect("child names its parent").parent_run,
                    seed.id,
                    "lineage links every generation to its parent"
                );
                seed = child;
                handoff = Some(next);
            }
            SegmentOutcome::Completed(trace) => break trace,
            SegmentOutcome::Suspended(_) => panic!("the script never asks"),
        }
    };

    assert!(
        rollovers >= 2,
        "the chain must roll over at least twice to exercise the fix"
    );
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
}
