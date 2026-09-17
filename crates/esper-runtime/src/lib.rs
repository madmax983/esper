//! `esper-runtime`: the durable `ReAct` runtime for the Esper E0/E1 slice.
//!
//! The engine runs one scripted `ReAct` trajectory at a time: the model
//! emits one line per turn, the engine decodes it with `esper-core`,
//! authorizes tool calls against the seed's capabilities, dispatches
//! them to the world doubles, verifies mutating tools with an
//! independent read-back, and accounts every committed step with the
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

pub mod error;
pub mod seed;

#[cfg(feature = "host")]
pub mod engine;
#[cfg(feature = "host")]
pub mod journal;
#[cfg(feature = "host")]
pub mod trace;
#[cfg(feature = "host")]
pub mod world;

pub use error::{CrashPoint, Halt, RuntimeError};
pub use seed::{RunSeed, ESPER_WORKFLOW_KIND, WORKFLOW_VERSION};

#[cfg(feature = "host")]
pub use engine::{drive_run, drive_run_async};
#[cfg(feature = "host")]
pub use journal::{Frame, Journal};
#[cfg(feature = "host")]
pub use trace::{RunTrace, TraceEvent};
#[cfg(feature = "host")]
pub use world::{
    Direction, FakeDevice, FaultPlan, InputPlan, ScriptedModel, WorldError, WriteRecord,
};
