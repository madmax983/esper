//! Identifier newtypes: construction, ranges, display.

use esper_core::Error;
use esper_core::ids::{Digest, EffectId, EffectSeq, PIN_COUNT, Pin, RunId, ToolId};

#[test]
fn run_id_round_trips() {
    let id = RunId::new(42);
    assert_eq!(id.get(), 42);
    assert_eq!(id, RunId::new(42));
    assert_ne!(id, RunId::new(43));
    assert_eq!(alloc_format(id), "run-42");
}

#[test]
fn effect_seq_next_overflow_is_an_error_not_a_wrap() {
    assert_eq!(EffectSeq::new(3).next(), Some(EffectSeq::new(4)));
    assert_eq!(EffectSeq::new(u32::MAX).next(), None);
}

#[test]
fn effect_id_composes_run_and_seq() {
    let effect = EffectId::new(RunId::new(7), EffectSeq::new(3));
    assert_eq!(effect.run(), RunId::new(7));
    assert_eq!(effect.seq(), EffectSeq::new(3));
    assert_eq!(alloc_format(effect), "7:3");
}

#[test]
fn tool_id_round_trips() {
    assert_eq!(ToolId::new(1).get(), 1);
    assert_eq!(alloc_format(ToolId::new(2)), "tool-2");
}

#[test]
fn pin_rejects_out_of_range() {
    assert_eq!(PIN_COUNT, 8);
    for pin in 0..8u8 {
        assert_eq!(Pin::new(pin).expect("valid pin").get(), pin);
    }
    assert_eq!(Pin::new(8), Err(Error::PinOutOfRange { pin: 8 }));
    assert_eq!(Pin::new(255), Err(Error::PinOutOfRange { pin: 255 }));
}

#[test]
fn digest_is_stable() {
    let a = Digest::of_bytes(b"hello");
    let b = Digest::of_bytes(b"hello");
    assert_eq!(a, b);
    assert_eq!(a.get(), b.get());
    // FNV-1a-64 of the empty string is the offset basis.
    assert_eq!(Digest::of_bytes(b"").get(), 0xcbf2_9ce4_8422_2325);
}

// `std` is available in tests (the lib itself stays allocation-free).
fn alloc_format(value: impl core::fmt::Display) -> String {
    std::format!("{value}")
}
