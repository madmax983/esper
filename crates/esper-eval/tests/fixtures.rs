//! Golden trajectory fixtures: one test per `*.json` file, generated
//! at compile time by `build.rs` from `spec/trajectories/`.
//!
//! Adding a ninth fixture file adds a ninth test with no code change.
//! Each test parses its fixture, drives `esper-runtime` to the
//! terminal result (injecting the planned reset), and asserts the
//! expected trace, the remaining budgets, the SPEC §11.3 invariants,
//! and every `forbidden` clause.

include!(concat!(env!("OUT_DIR"), "/fixture_tests.rs"));
