//! `esper-runtime`: the durable `ReAct` runtime, for firmware.
//!
//! The run ([`fn@run`]) is a Waymaker workflow: an `async fn` over
//! `waymaker_embassy::Ctx`, `no_std` and allocation-free. The model
//! emits one line per turn as a recorded inference activity; the
//! workflow decodes it with the E2 contract validator, authorizes tool
//! calls against the seed's capabilities, dispatches each attempt as a
//! recorded activity, verifies mutating tools with an independent
//! read-back, and accounts every step with the deterministic monitor.
//! Waymaker's journal is the only store: a reset replays the recorded
//! answers and the workflow resumes from the committed prefix.
//!
//! Under the `host` feature, [`drive_run`] drives the workflow over the
//! test doubles, injecting a reset at any [`CrashPoint`].
//!
//! E4+aon adds the always-on layer above the run ([`agent`]): the
//! [`supervise`] workflow, `no_std` over `waymaker_embassy::Ctx`, paced
//! by durable heartbeat deadlines, serving standing goals with ordinary
//! runs and carrying masked memory between them (SPEC §21).

#![deny(unsafe_code)]
#![cfg_attr(not(feature = "host"), no_std)]

pub mod agent;
pub mod backend;

// Firmware-first guard (CLAUDE.md): the always-on supervisor ships on
// the device. This item exists only in the firmware build and names the
// supervisor's public surface, so moving `agent` behind the `host`
// feature breaks `cargo check -p esper-runtime --no-default-features`
// instead of silently excluding the supervisor from the firmware.
#[cfg(not(feature = "host"))]
const _: () = {
    let _ = agent::AgentSeed::default_slice;
    let _ = agent::wire::PAGE_RECORD_MAX;
    let _: fn(&agent::Start, &mut [u8]) -> Result<usize, agent::AgentError> = agent::Start::encode;
    // The run workflow ships on the device too.
    let _ = run::RunState::new;
    let _ = run::wire::RunStart::decode;
    let _ = run::device::observe;
};
pub mod error;
pub mod run;
pub mod seed;

#[cfg(feature = "host")]
pub mod jev;
#[cfg(feature = "host")]
pub mod journal;
#[cfg(feature = "host")]
pub mod trace;
#[cfg(feature = "host")]
pub mod world;

pub use error::{CrashPoint, Halt, RuntimeError};
// The E3 model-backend vocabulary: the inference boundary types live
// in `backend` and are re-exported here. `ScriptedBackend` is the
// host-only canned-line double; `TinyBackend` is allocation-free and
// available without the `host` feature.
#[cfg(feature = "host")]
pub use backend::ScriptedBackend;
// The E3+jev System One adapter: a typed host backend. The transport
// vocabulary it names is public so cassettes stay authorable.
pub use backend::{
    BRIEFED_PROMPT_CAP, BackendError, BundleId, DistillEntry, InferenceSettings, ModelBackend,
    OUTPUT_CAP, PROMPT_CAP, PriorCtx, PromptCtx, RepairHint, TINY_PARAMS_BYTES, TinyBackend,
    TokenUsage, build_prompt, fingerprint_prompt, fnv1a64,
};
#[cfg(feature = "host")]
pub use jev::{
    JEV_ENDPOINT, JEV_MODEL_VERSION, JevAnswer, JevBackend, JevError, JevReceipt, JevTransport,
    LiveTransport, MockTransport, Prob, RecordedAnswer, ScoreBand, build_request_json, score_band,
    synthesize_response,
};
// The E2 typed-tool vocabulary lives in `esper-core`; the runtime
// re-exports the three shapes its public API names (seed capabilities,
// the status detail level, and the typed dispatch arguments).
pub use esper_core::decision::{StatusDetail, ToolArgs};
pub use esper_core::registry::Capabilities;
pub use seed::{ESPER_WORKFLOW_KIND, RunSeed, SEED_INPUT_LEN, WORKFLOW_VERSION};

// E4+aon: the always-on supervisor (SPEC §21) — a `no_std` Waymaker
// workflow, compiled into the firmware build.
pub use agent::{
    AgentError, AgentSeed, AgentState, BeatAction, Heartbeat, NoPulse, Pulse, PulseSink,
    Settlement, Start, supervise,
};
// The run workflow (SPEC §22): `no_std`, compiled into the firmware.
#[cfg(feature = "host")]
pub use journal::{Frame, Journal, Record};
#[cfg(feature = "host")]
pub use run::host::{
    Reset, RolloverHandoff, SegmentOutcome, drive_run, drive_run_async, drive_run_with_reset,
    drive_segment,
};
pub use run::{RunState, run};
#[cfg(feature = "host")]
pub use trace::{RunTrace, TraceEvent};
#[cfg(feature = "host")]
pub use world::{Direction, FakeDevice, FaultPlan, InputPlan, WorldError, WriteRecord};
