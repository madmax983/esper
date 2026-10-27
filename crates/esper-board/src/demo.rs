//! The goal the board ships able to answer, and its distilled model.
//!
//! The board's model is [`TinyBackend`] — a distillation stand-in, not a
//! trained model (ADR 0001): a table from prompt fingerprint to decision
//! line. It answers exactly the prompts it was distilled on and refuses
//! every other with `UnknownPrompt`, which the run records as a model
//! failure. [`DISTILL`] is the table for [`GOAL`]: the prompts the run
//! renders while reading pin 3, driving it high, and finishing — from a
//! low pin, and from a pin already high, which is what a run segment
//! replayed after a journal reinstall finds — recorded by
//! `tests/board.rs`, which fails if the table and the prompts drift.

use esper_runtime::backend::{DistillEntry, TinyBackend};

/// The operator line the table answers.
pub const GOAL: &[u8] = b"goal 1 drive pin 3 high";

/// The decision lines the table was distilled from, in order.
pub const SCRIPT: &[&[u8]] = &[
    br#"CALL gpio_pin_read {"pin": 3}"#,
    br#"CALL gpio_pin_write {"pin": 3, "level": "high"}"#,
    br#"FINISH {"status": "completed", "summary": "pin 3 high"}"#,
];

/// The distilled prompt→line table for [`GOAL`].
pub const DISTILL: &[DistillEntry<'static>] = &[
    DistillEntry {
        prompt_fp: 0x43bd_edb7_bb23_a840,
        line_hash: 0x8b22_eef7_803f_b52b,
        line: br#"CALL gpio_pin_read {"pin": 3}"#,
    },
    DistillEntry {
        prompt_fp: 0x2ef5_f91f_3fba_e4e1,
        line_hash: 0x49f9_9b09_fa89_a086,
        line: br#"CALL gpio_pin_write {"pin": 3, "level": "high"}"#,
    },
    DistillEntry {
        prompt_fp: 0xaac8_c586_e3bd_77ad,
        line_hash: 0xf99a_0394_889f_7811,
        line: br#"FINISH {"status": "completed", "summary": "pin 3 high"}"#,
    },
    // The same goal finding pin 3 already high: a run segment replayed
    // after its journal was reinstalled reads the level it left.
    DistillEntry {
        prompt_fp: 0x0f7d_16d5_d4c1_336d,
        line_hash: 0x49f9_9b09_fa89_a086,
        line: br#"CALL gpio_pin_write {"pin": 3, "level": "high"}"#,
    },
];

/// The board's model over [`DISTILL`].
#[must_use]
pub fn model() -> TinyBackend<'static> {
    TinyBackend::new(DISTILL)
}
