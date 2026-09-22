//! esper-protocol: the single contract source for Esper's typed native tools.
//!
//! Rung E2 (SPEC §17). Everything the harness knows about a tool — its
//! stable id, name, argument schema, permission class, verification and
//! idempotency strategies, result bound, valid and invalid argument
//! fixtures — lives in one constant table ([`contract::CATALOG`]).
//! From that table this crate derives:
//!
//! - an allocation-free argument validator ([`validate()`]);
//! - the compact model-facing signature for the prompt ([`render_signature`]);
//! - a minimal JSON Schema for training and host interop ([`render_json_schema`]);
//! - the golden valid/invalid argument fixtures (each entry's
//!   `example_ok` / `example_bad`, asserted by the in-crate tests).
//!
//! This is the design §7 schema pipeline, with the on-device validator as
//! the normative artifact and the JSON Schema as a host-side concern.
//!
//! ## Firmware cleanliness
//!
//! This crate is `#![no_std]` with no `alloc` and depends on nothing but
//! `core` (plus `thiserror` for the error vocabulary, which is
//! `no_std`-compatible). The strict JSON parser lives here
//! ([`json`], moved from `esper-core` in E2 so both crates share one
//! implementation without a dependency cycle).
//!
//! ## Modules
//!
//! - [`contract`]: the normative six-tool table and its schema types.
//! - [`json`]: strict, allocation-free JSON validation (moved from `esper-core`).
//! - [`mod@validate`]: the generic validator and the bound-argument types.
//! - [`render`]: the signature and JSON Schema renderers.

#![no_std]
#![deny(unsafe_code)]
#![deny(missing_docs)]

pub mod contract;
pub mod json;
pub mod render;
pub mod validate;

// Convenience re-exports: the normative API surface other crews code against (SPEC §17.3).
pub use contract::{
    ArgKind, ArgSpec, CATALOG, IdempotencyStrategy, PermissionClass, ToolContract,
    VerificationStrategy, catalog, lookup_by_id, lookup_by_name,
};
pub use render::{render_json_schema, render_signature};
pub use validate::{BoundArgs, BoundField, MAX_ARGS_PER_TOOL, Scalar, ValidationError, validate};
