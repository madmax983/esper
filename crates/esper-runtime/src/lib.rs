//! `esper-runtime`: the durable `ReAct` runtime for the Esper E0/E1 slice.
//!
//! The engine runs one scripted `ReAct` trajectory at a time: the model
//! emits one line per turn, the engine decodes it with the E2 contract
//! validator, authorizes tool calls against the seed's capabilities,
//! dispatches them to the world doubles, verifies mutating tools with
//! an independent read-back, and accounts every committed step with the
//! deterministic monitor. Every boundary crossing appends a journal
//! frame first; a simulated crash at any [`CrashPoint`] drops all
//! in-memory state and replays the journal, so recovery is just replay.
//!
//! The driver entry point is [`drive_run`] (plus [`drive_run_async`]
//! under the `host` feature).

#![deny(unsafe_code)]
#![cfg_attr(not(feature = "host"), no_std)]

// Declared per SPEC §2 and intentionally linked: the E0/E1 engine
// drives `waymaker-core` directly, and the E2 async model backend
// integrates through the Embassy facade.
#[allow(unused_extern_crates)]
extern crate waymaker_embassy as _;

pub mod backend;
pub mod error;
pub mod seed;

#[cfg(feature = "host")]
pub mod engine;
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
    BackendError, BundleId, DistillEntry, InferenceSettings, ModelBackend, OUTPUT_CAP, PROMPT_CAP,
    PromptCtx, RepairHint, TINY_PARAMS_BYTES, TinyBackend, TokenUsage, build_prompt,
    fingerprint_prompt, fnv1a64,
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
pub use seed::{ESPER_WORKFLOW_KIND, RunSeed, WORKFLOW_VERSION};

#[cfg(feature = "host")]
pub use engine::{drive_run, drive_run_async};
#[cfg(feature = "host")]
pub use journal::{Frame, Journal};
#[cfg(feature = "host")]
pub use trace::{RunTrace, TraceEvent};
#[cfg(feature = "host")]
pub use world::{Direction, FakeDevice, FaultPlan, InputPlan, WorldError, WriteRecord};
