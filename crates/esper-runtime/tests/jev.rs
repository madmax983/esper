//! The Jev host-backend adapter, end to end.
//!
//! Typed System One answers drive real runs through the mock
//! transport: the happy path, a denied write, the `Noul` ask gate
//! with suspend/resume, and a crash before the decision commit.
//!
//! The cassettes live in `spec/jev-cassettes/` as reviewed fixtures.
//! They are authored by record mode: run with
//! `ESPER_JEV_RECORD=1`, which drives each scenario through a
//! directing transport (scripted [`RecordedAnswer`]s, no key) and
//! writes the request/response pairs. A normal run replays the
//! committed cassettes through [`MockTransport`].

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use esper_core::ids::{Pin, RunId};
use esper_core::state::TerminalStatus;
use esper_runtime::{
    CrashPoint, Direction, FakeDevice, FaultPlan, InputPlan, JEV_ENDPOINT, JEV_MODEL_VERSION,
    JevBackend, JevError, JevTransport, Journal, LiveTransport, MockTransport, ModelBackend,
    RecordedAnswer, RunSeed, drive_run, fnv1a64, synthesize_response,
};

/// The record-mode capture log: `(request_hash, response)` pairs.
type CaptureLog = Arc<Mutex<Vec<(u64, Vec<u8>)>>>;

/// A directing transport for record mode: pops scripted answers and
/// captures `(request_hash, response)` pairs for the cassette.
struct DirectingTransport {
    script: VecDeque<RecordedAnswer>,
    captured: CaptureLog,
}

impl DirectingTransport {
    fn new(script: Vec<RecordedAnswer>, captured: CaptureLog) -> Self {
        Self {
            script: script.into(),
            captured,
        }
    }
}

impl JevTransport for DirectingTransport {
    fn ask(&mut self, request_json: &[u8]) -> Result<Vec<u8>, JevError> {
        let answer = self
            .script
            .pop_front()
            .expect("the directing script is exhausted: the run asked for more turns");
        let response = synthesize_response(&answer, JEV_MODEL_VERSION);
        self.captured
            .lock()
            .expect("capture lock")
            .push((fnv1a64(request_json), response.clone()));
        Ok(response)
    }
}

/// Build one scripted answer.
const fn ans(
    choice: &'static str,
    choice_probs_bps: [u16; 8],
    noul_p_bps: u16,
    score_milli: u16,
    score_probs_bps: [u16; 4],
) -> RecordedAnswer {
    RecordedAnswer {
        choice,
        choice_probs_bps,
        noul_p_bps,
        score_milli,
        score_probs_bps,
    }
}

/// Read pin 4, drive it high, finish — the typed mirror of the
/// scripted happy-path trajectory.
fn happy_script() -> Vec<RecordedAnswer> {
    vec![
        ans(
            "call_gpio_pin_read",
            [9_000, 200, 200, 200, 200, 100, 50, 50],
            100,
            3_100,
            [500, 1_000, 5_500, 3_000],
        ),
        ans(
            "call_gpio_pin_write",
            [200, 9_000, 200, 200, 200, 100, 50, 50],
            50,
            3_300,
            [300, 700, 6_000, 3_000],
        ),
        ans(
            "finish",
            [50, 50, 100, 100, 100, 100, 50, 9_450],
            50,
            3_800,
            [100, 300, 2_000, 7_600],
        ),
    ]
}

/// The crash scenario re-infers the first turn after the crash, so the
/// directing script carries it twice. The mock serves both infers
/// from the one recorded pair.
fn crash_script() -> Vec<RecordedAnswer> {
    let mut script = happy_script();
    script.insert(0, script[0].clone());
    script
}

/// A write the device will deny.
fn denied_script() -> Vec<RecordedAnswer> {
    vec![ans(
        "call_gpio_pin_write",
        [200, 9_000, 200, 200, 200, 100, 50, 50],
        50,
        3_300,
        [300, 700, 6_000, 3_000],
    )]
}

/// A read the `Noul` gate overrides with `ask`, then a finish after
/// the operator answers.
fn ask_script() -> Vec<RecordedAnswer> {
    vec![
        ans(
            "call_gpio_pin_read",
            [9_000, 200, 200, 200, 200, 100, 50, 50],
            8_500,
            3_100,
            [500, 1_000, 5_500, 3_000],
        ),
        ans(
            "finish",
            [50, 50, 100, 100, 100, 100, 50, 9_450],
            50,
            3_800,
            [100, 300, 2_000, 7_600],
        ),
    ]
}

/// The committed cassette for a scenario.
fn cassette(name: &str) -> &'static str {
    match name {
        "happy" => include_str!("../../../spec/jev-cassettes/happy.json"),
        "crash" => include_str!("../../../spec/jev-cassettes/crash.json"),
        "denied" => include_str!("../../../spec/jev-cassettes/denied.json"),
        "ask" => include_str!("../../../spec/jev-cassettes/ask.json"),
        _ => panic!("unknown Jev cassette"),
    }
}

/// Build the backend: the directing transport in record mode, the
/// mock cassette otherwise. Returns the backend plus the capture log
/// (empty outside record mode).
fn backend_for(name: &str, script: Vec<RecordedAnswer>) -> (JevBackend, CaptureLog) {
    let captured: CaptureLog = Arc::new(Mutex::new(Vec::new()));
    let backend = if std::env::var("ESPER_JEV_RECORD").as_deref() == Ok("1") {
        let transport = DirectingTransport::new(script, Arc::clone(&captured));
        JevBackend::new(Box::new(transport), JEV_MODEL_VERSION, JEV_ENDPOINT)
    } else {
        let transport =
            MockTransport::from_cassette(cassette(name)).expect("the cassette must parse");
        JevBackend::new(Box::new(transport), JEV_MODEL_VERSION, JEV_ENDPOINT)
    };
    (backend, captured)
}

/// The seed bound to this run's Jev bundle.
fn seed_for(backend: &JevBackend, id: u64) -> RunSeed {
    RunSeed {
        id: RunId::new(id),
        model_bundle: backend.bundle_id().0,
        ..RunSeed::default_slice()
    }
}

/// Write the cassette in record mode. Duplicate request hashes (the
/// crash scenario re-infers) keep their first response.
fn maybe_record(name: &str, captured: &[(u64, Vec<u8>)]) {
    if std::env::var("ESPER_JEV_RECORD").as_deref() != Ok("1") {
        return;
    }
    let dir = "../../spec/jev-cassettes";
    std::fs::create_dir_all(dir).expect("the cassette dir must be creatable");
    let mut seen: Vec<u64> = Vec::new();
    let mut out = String::from("{\"model_version\":\"");
    out.push_str(JEV_MODEL_VERSION);
    out.push_str("\",\"endpoint\":\"");
    out.push_str(JEV_ENDPOINT);
    out.push_str("\",\"pairs\":[");
    let mut first = true;
    for (hash, response) in captured {
        if seen.contains(hash) {
            continue;
        }
        seen.push(*hash);
        if !first {
            out.push(',');
        }
        first = false;
        out.push_str("{\"request_hash\":\"");
        out.push_str(&hash.to_string());
        out.push_str("\",\"response\":");
        out.push_str(core::str::from_utf8(response).expect("responses are valid UTF-8"));
        out.push('}');
    }
    out.push_str("]}");
    std::fs::write(format!("{dir}/{name}.json"), out).expect("the cassette must be writable");
}

fn pin(n: u8) -> Pin {
    Pin::new(n).expect("bad pin in test")
}

/// The typed happy path: read, write, finish — all through Jev
/// `Choice` answers, with the returned probabilities on the receipts.
#[test]
fn jev_happy_path() {
    let (mut backend, captured) = backend_for("happy", happy_script());
    let seed = seed_for(&backend, 301);
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
        None,
    )
    .expect("run failed");
    maybe_record("happy", &captured.lock().expect("capture lock"));
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(trace.summary(), Some(b"Run complete.".as_slice()));
    assert_eq!(device.physical_writes(), 1);

    assert_eq!(backend.model_version(), JEV_MODEL_VERSION);
    let receipts = backend.receipts();
    assert_eq!(receipts.len(), 3);
    // Turn 1: a confident read, ungated.
    assert_eq!(receipts[0].answer.raw_choice, 0);
    assert_eq!(receipts[0].answer.choice, 0);
    assert!(!receipts[0].answer.gated);
    assert_eq!(receipts[0].answer.noul_p_bps, 100);
    assert_eq!(receipts[0].answer.score_milli, 3_100);
    // Turn 3: finish.
    assert_eq!(receipts[2].answer.choice, 7);
    // Jev generates no text: output is unmetered, input is measured.
    let usage = backend.last_usage();
    assert!(usage.input_tokens > 0);
    assert_eq!(usage.output_tokens, 0);
}

/// A write the device denies: the typed choice still renders a
/// concrete line, and authorization still fails closed.
#[test]
fn jev_denied_write() {
    let (mut backend, captured) = backend_for("denied", denied_script());
    let seed = seed_for(&backend, 302);
    let mut device = FakeDevice::new();
    // The template renders pin 0 on a turn with no observation.
    device.set_direction(pin(0), Direction::Input);
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
        None,
    )
    .expect("run failed");
    maybe_record("denied", &captured.lock().expect("capture lock"));
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Denied));
    assert_eq!(trace.reason(), Some(b"pin_direction_denied".as_slice()));
    assert_eq!(device.physical_writes(), 0);
    let receipts = backend.receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].answer.choice, 1);
}

/// The `Noul` gate overrides a confident read with `ask`: the run
/// suspends, the receipt records the override, and the operator's
/// answer resumes it to completion.
#[test]
fn jev_ask_gate_suspend_resume() {
    let (mut backend, captured) = backend_for("ask", ask_script());
    let seed = seed_for(&backend, 303);
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
        Some(CrashPoint::AwaitInput),
    )
    .expect("run failed");
    assert!(trace.suspended());
    assert_eq!(trace.terminal_status(), None);
    let receipts = backend.receipts();
    assert_eq!(receipts.len(), 1);
    assert_eq!(receipts[0].answer.raw_choice, 0);
    assert_eq!(receipts[0].answer.choice, 6);
    assert!(receipts[0].answer.gated);
    assert_eq!(receipts[0].answer.noul_p_bps, 8_500);

    let mut inputs = InputPlan::new(vec![b"{\"pin\": 0}".to_vec()]);
    let trace = drive_run(
        &seed,
        &mut journal,
        &mut backend,
        &mut device,
        &mut faults,
        &mut inputs,
        None,
    )
    .expect("run failed");
    maybe_record("ask", &captured.lock().expect("capture lock"));
    assert!(!trace.suspended());
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(backend.receipts().len(), 2);
}

/// A crash before the decision commit re-infers the same prompt; the
/// mock serves the same recorded response and the run completes with
/// exactly one receipt per turn.
#[test]
fn jev_crash_before_commit() {
    let (mut backend, captured) = backend_for("crash", crash_script());
    let seed = seed_for(&backend, 304);
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
        Some(CrashPoint::BeforeDecisionCommit),
    )
    .expect("run failed");
    maybe_record("crash", &captured.lock().expect("capture lock"));
    assert_eq!(trace.terminal_status(), Some(TerminalStatus::Completed));
    assert_eq!(device.physical_writes(), 1);
    assert_eq!(backend.receipts().len(), 3);
}

/// One real System One turn through the full adapter: request,
/// HTTPS, parse, gate, template line. Runs only when `JEV_BEARER`
/// holds a bearer key; otherwise it skips. Never runs in CI or by
/// default — a live call spends a real turn against the API.
#[test]
fn jev_live_turn_end_to_end() {
    let key = std::env::var("JEV_BEARER").unwrap_or_default();
    if key.trim().is_empty() {
        eprintln!("skipping jev_live_turn_end_to_end: JEV_BEARER is not set");
        return;
    }
    let transport = LiveTransport::new(JEV_ENDPOINT, key.trim());
    let mut backend = JevBackend::new(Box::new(transport), JEV_MODEL_VERSION, JEV_ENDPOINT);
    let prompt = b"TOOLS call_sensor_sample_read\nSTATE 1\nLAST {\"sensor\": 0}\nEMIT one line\n";
    let mut out = [0u8; 4096];
    let written = backend
        .infer(prompt, &mut out)
        .expect("the live turn should answer");
    let line = &out[..written];
    assert_eq!(backend.receipts().len(), 1);
    let receipt = &backend.receipts()[0];
    assert_eq!(receipt.answer.choice_probs_bps.len(), 8);
    assert!(receipt.answer.noul_p_bps <= 10_000);
    assert!((1_000..=4_000).contains(&receipt.answer.score_milli));
    let text = core::str::from_utf8(line).expect("the line is ASCII");
    assert!(
        text.starts_with("CALL ") || text.starts_with("ASK ") || text.starts_with("FINISH "),
        "unexpected line: {text}"
    );
}
