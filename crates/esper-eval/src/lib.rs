//! `esper-eval`: the host-only golden-fixture evaluation runner.
//!
//! This crate turns the SPEC's trajectory fixtures
//! (`spec/trajectories/*.json`) into executable, data-driven tests. For
//! each fixture it parses the JSON, builds the [`RunSeed`], scripted
//! model, fake device, fault plan, and input plan the fixture describes,
//! drives `esper-runtime` to `End` via
//! [`esper_runtime::drive_run`] — injecting every planned reset and
//! letting the engine recover by journal replay — and then asserts the
//! expected trace, the budget assertions, the SPEC §11.3 crash-oracle
//! invariants, and every `forbidden` clause.
//!
//! One `#[test]` exists per fixture file, generated at compile time by
//! `build.rs` from the fixture directory: adding a ninth fixture file
//! adds a ninth test with no code change.
//!
//! // HOST-ONLY (E0/E1): this entire crate is host-only. It parses JSON,
//! allocates freely, reads files, and formats diffs. It must never enter
//! a firmware image, and no firmware crate may depend on it (SPEC §1).
//! Every allocation site below carries the `// HOST-ONLY (E0/E1)` marker.

#![deny(unsafe_code)]

pub mod checks;
pub mod compare;
pub mod fixture;
pub mod json;
pub mod runner;

pub use esper_runtime::RunSeed;
pub use fixture::{Fixture, FixtureError, parse_fixture};
pub use runner::{FixtureReport, run_fixture, run_fixture_file};
