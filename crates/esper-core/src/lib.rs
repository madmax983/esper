//! esper-core: pure types, decision decoder, resource budgets, runtime monitor.
//!
//! This crate is the foundation of the Esper E0/E1 vertical slice (see
//! `SPEC.md` at the workspace root). It is `#![no_std]` with no `alloc`
//! and depends on nothing but `core` (plus `thiserror` for the error
//! vocabulary, which is `no_std`-compatible). It performs no I/O, owns no
//! clocks, and never logs.
//!
//! ## Firmware cleanliness
//!
//! Nothing in this crate is host-only: every item here is safe to link
//! into a firmware image. Host-side doubles (scripted model backend, fake
//! device, fault injector, fixture runner) live in `esper-runtime` and
//! `esper-eval`, where each allocation site carries a
//! `// HOST-ONLY (E0/E1)` marker. A CI layering gate fails the build if
//! `alloc` appears in this crate's dependency closure.
//!
//! ## Modules
//!
//! - [`ids`]: strong newtypes (`RunId`, `EffectId`, `ToolId`, `Pin`, `Digest`).
//! - [`state`]: the workflow state machine as data, plus terminal statuses.
//! - [`error`]: the stable §6 error vocabulary and decoder rejection types.
//! - [`json`]: strict, allocation-free JSON validation used by the decoder.
//! - [`decision`]: the `Decision` union and the `VERB <json>` line decoder.
//! - [`registry`]: the static tool catalog, pin capabilities, policy gate.
//! - [`budget`]: the resource budget model (part of run identity).
//! - [`monitor`]: the deterministic runtime monitor (loop and guard rules).
//! - [`records`]: logical journal record kinds and the terminal result type.

#![no_std]
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod budget;
pub mod decision;
pub mod error;
pub mod ids;
pub mod json;
pub mod monitor;
pub mod records;
pub mod registry;
pub mod state;

// Convenience re-exports of the types the runtime crew reaches for most.
pub use budget::ResourceBudget;
pub use decision::Decision;
pub use error::{Error, ErrorCode};
pub use ids::{Digest, EffectId, EffectSeq, Pin, RunId, ToolId};
pub use state::{committed_records, is_legal, transition, Event, State, TerminalStatus};
