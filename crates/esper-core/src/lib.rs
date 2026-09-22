//! esper-core: pure types, decision decoder, resource budgets, runtime monitor.
//!
//! This crate is the foundation of the Esper E0/E1/E2 vertical slice (see
//! `SPEC.md` at the workspace root). It is `#![no_std]` with no `alloc`
//! and depends on nothing but `core`, `esper-protocol` (the E2 contract
//! source, which owns the strict JSON parser re-exported here and the
//! tool contract table the decoder validates against), and
//! `thiserror` for the error vocabulary (which is `no_std`-compatible).
//! It performs no I/O, owns no clocks, and never logs.
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
//! - [`json`]: strict, allocation-free JSON validation, re-exported from
//!   `esper-protocol` (moved there in E2; SPEC §17.4).
//! - [`decision`]: the `Decision` union, the typed [`decision::ToolArgs`]
//!   dispatch shape, and the `VERB <json>` line decoder.
//! - [`registry`]: the static tool catalog (re-exported from
//!   `esper-protocol`), capabilities, and the policy gate.
//! - [`budget`]: the resource budget model (part of run identity).
//! - [`compact`]: bounded context compaction into a durable compact state.
//! - [`monitor`]: the deterministic runtime monitor (loop and guard rules).
//! - [`records`]: logical journal record kinds and the terminal result type.
//! - [`lineage`]: run lineage for `continue_as_new`.
//! - [`mask`]: secret and PII masking before prompts, durability, logging.
//! - [`snapshot`]: versioned, integrity-checked compact-state snapshots.

#![no_std]
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod budget;
pub mod compact;
pub mod decision;
pub mod error;
pub mod ids;
// E2: the strict JSON parser moved to `esper-protocol` so the contract
// validator and the decoder share one implementation without a
// dependency cycle (SPEC §17.4). Re-exported here, so every existing
// `crate::json::…` and `esper_core::json::…` path keeps working.
pub use esper_protocol::json;
pub mod lineage;
pub mod mask;
pub mod monitor;
pub mod records;
pub mod registry;
pub mod snapshot;
pub mod state;

// Convenience re-exports of the types the runtime crew reaches for most.
pub use budget::ResourceBudget;
pub use compact::{
    compact, should_compact, CompactState, CompactionPolicy, CompactionReport, FailedPath, Fact,
    FrameSummary, Note, PendingItem, PendingKind, VersionSet, DEFAULT_POLICY,
};
pub use decision::{Decision, StatusDetail, ToolArgs};
pub use error::{Error, ErrorCode};
pub use ids::{Digest, EffectId, EffectSeq, Pin, RunId, ToolId};
pub use lineage::{continue_as_new, ContinuedRun, Lineage};
pub use mask::{fnv1a64, mask_bound, mask_bytes, mask_report, MaskClass, MaskReport};
pub use snapshot::{decode, encode, encoded_len, verify, SNAPSHOT_VERSION};
pub use state::{Event, State, TerminalStatus, committed_records, is_legal, transition};
