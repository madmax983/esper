//! Decoder golden vectors: valid lines plus every invalid class (§4.3).

use esper_core::decision::{
    decode_line, Decision, FinishStatus, Level, MAX_ARGS_BYTES, PIN_SELECT_SCHEMA,
};
use esper_core::error::{Error, RepairVariant};
use esper_core::ids::Digest;
use esper_core::registry::{GPIO_PIN_READ_ID, GPIO_PIN_WRITE_ID};

const fn is_call(decision: &Decision<'_>) -> bool {
    matches!(decision, Decision::Call(_))
}

#[test]
fn decodes_call_read() {
    let line = b"CALL gpio_pin_read {\"pin\": 4}";
    match decode_line(line).expect("valid CALL") {
        Decision::Call(call) => {
            assert_eq!(call.tool().get(), GPIO_PIN_READ_ID);
            assert_eq!(call.args(), b"{\"pin\": 4}".as_slice());
            assert_eq!(call.args_digest(), Digest::of_bytes(b"{\"pin\": 4}"));
            assert_eq!(call.pin().expect("pin").get(), 4);
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn decodes_call_write_with_level() {
    let line = b"CALL gpio_pin_write {\"pin\": 4, \"level\": \"high\"}";
    match decode_line(line).expect("valid CALL") {
        Decision::Call(call) => {
            assert_eq!(call.tool().get(), GPIO_PIN_WRITE_ID);
            assert_eq!(call.pin().expect("pin").get(), 4);
            assert_eq!(call.level().expect("level"), Level::High);
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn decodes_call_write_low_level() {
    let line = b"CALL gpio_pin_write {\"level\":\"low\",\"pin\":0}";
    match decode_line(line).expect("valid CALL") {
        Decision::Call(call) => {
            assert_eq!(call.level().expect("level"), Level::Low);
            assert_eq!(call.pin().expect("pin").get(), 0);
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn strips_one_trailing_newline() {
    for line in [
        b"CALL gpio_pin_read {\"pin\": 4}\n".as_slice(),
        b"CALL gpio_pin_read {\"pin\": 4}\r\n".as_slice(),
    ] {
        assert!(is_call(&decode_line(line).expect("newline stripped")));
    }
}

#[test]
fn decodes_ask() {
    let line = b"ASK {\"prompt\": \"which pin?\", \"schema\": 1}";
    match decode_line(line).expect("valid ASK") {
        Decision::Ask(ask) => {
            assert_eq!(ask.prompt(), b"which pin?".as_slice());
            assert_eq!(ask.response_schema_id(), PIN_SELECT_SCHEMA);
        }
        other => panic!("expected Ask, got {other:?}"),
    }
}

#[test]
fn decodes_finish() {
    let line = b"FINISH {\"status\": \"completed\", \"summary\": \"pin 4 high\"}";
    match decode_line(line).expect("valid FINISH") {
        Decision::Finish(answer) => {
            assert_eq!(answer.status(), FinishStatus::Completed);
            assert_eq!(answer.summary(), b"pin 4 high".as_slice());
        }
        other => panic!("expected Finish, got {other:?}"),
    }
}

#[test]
fn digest_is_deterministic_and_input_sensitive() {
    let first = decode_line(b"CALL gpio_pin_read {\"pin\": 4}").expect("valid");
    let second = decode_line(b"CALL gpio_pin_read {\"pin\": 4}").expect("valid");
    let third = decode_line(b"CALL gpio_pin_read {\"pin\": 5}").expect("valid");
    let (digest_a, digest_b, digest_c) = match (&first, &second, &third) {
        (Decision::Call(x), Decision::Call(y), Decision::Call(z)) => {
            (x.args_digest(), y.args_digest(), z.args_digest())
        }
        _ => panic!("expected calls"),
    };
    assert_eq!(digest_a, digest_b, "same bytes hash the same");
    assert_ne!(digest_a, digest_c, "different bytes hash differently");
}

// --- Invalid classes: each asserts the exact rejection and its repair
// variant (Malformed consumes a repair turn for envelope faults,
// InvalidArgs for schema faults). ---

fn variant_of(line: &[u8]) -> RepairVariant {
    match decode_line(line) {
        Ok(d) => panic!("expected rejection, decoded {d:?}"),
        Err(e) => e.repair_variant().expect("decoder errors have a variant"),
    }
}

#[test]
fn rejects_empty_line() {
    assert_eq!(decode_line(b""), Err(Error::EmptyLine));
    assert_eq!(variant_of(b""), RepairVariant::Malformed);
}

#[test]
fn rejects_unknown_verb() {
    assert_eq!(decode_line(b"JUMP {\"pin\": 4}"), Err(Error::UnknownVerb));
    assert_eq!(
        decode_line(b"call gpio_pin_read {\"pin\": 4}"),
        Err(Error::UnknownVerb),
        "verbs are uppercase-only"
    );
}

#[test]
fn rejects_bare_verbs() {
    assert_eq!(decode_line(b"CALL"), Err(Error::MissingToolName));
    assert_eq!(decode_line(b"ASK"), Err(Error::MissingArgs));
    assert_eq!(decode_line(b"FINISH"), Err(Error::MissingArgs));
}

#[test]
fn rejects_call_without_args() {
    assert_eq!(decode_line(b"CALL gpio_pin_read"), Err(Error::MissingArgs));
}

#[test]
fn rejects_unknown_tool_name() {
    assert_eq!(
        decode_line(b"CALL gpio_frobnicator {\"pin\": 4}"),
        Err(Error::UnknownToolName)
    );
    assert_eq!(
        variant_of(b"CALL gpio_frobnicator {\"pin\": 4}"),
        RepairVariant::Malformed
    );
}

#[test]
fn rejects_args_over_256_bytes() {
    let mut line = b"CALL gpio_pin_read {\"pin\": 4, \"pad\": \"".to_vec();
    line.extend(core::iter::repeat_n(b'x', MAX_ARGS_BYTES));
    line.extend(b"\"}");
    assert_eq!(decode_line(&line), Err(Error::ArgsTooLong));
}

#[test]
fn call_args_bound_is_the_spec_literal_256_not_the_envelope() {
    // ~300 bytes of args would have fit the old single 384 bound; the
    // CALL bound is §4.3's literal 256.
    let mut line = b"CALL gpio_pin_read {\"pin\": 4, \"pad\": \"".to_vec();
    line.extend(core::iter::repeat_n(b'x', 278));
    line.extend(b"\"}");
    assert_eq!(decode_line(&line), Err(Error::ArgsTooLong));
}

#[test]
fn ask_json_block_over_384_bytes_rejected() {
    use esper_core::decision::MAX_JSON_BYTES;
    let mut line = b"ASK {\"prompt\": \"".to_vec();
    line.extend(core::iter::repeat_n(b'p', MAX_JSON_BYTES));
    line.extend(b"\", \"schema\": 1}");
    assert_eq!(decode_line(&line), Err(Error::JsonTooLong));
    let hint = Error::JsonTooLong.repair_hint().expect("decoder rejection");
    assert_eq!(hint.variant, RepairVariant::Malformed);
    assert_eq!(hint.field, "json");
    assert_eq!(hint.expected, "<= 384 bytes");
}

#[test]
fn finish_json_block_over_384_bytes_rejected() {
    use esper_core::decision::MAX_JSON_BYTES;
    let mut line = b"FINISH {\"status\": \"completed\", \"summary\": \"".to_vec();
    line.extend(core::iter::repeat_n(b's', MAX_JSON_BYTES));
    line.extend(b"\"}");
    assert_eq!(decode_line(&line), Err(Error::JsonTooLong));
}

#[test]
fn rejects_line_over_512_bytes() {
    let mut line = b"ASK {\"prompt\": \"".to_vec();
    line.extend(core::iter::repeat_n(b'y', 600));
    line.extend(b"\", \"schema\": 1}");
    assert_eq!(decode_line(&line), Err(Error::LineTooLong));
}

#[test]
fn rejects_json_syntax_errors() {
    // Unterminated object.
    assert!(matches!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 4"),
        Err(Error::Json(_))
    ));
    // Bad token.
    assert!(matches!(
        decode_line(b"CALL gpio_pin_read {\"pin\": }"),
        Err(Error::Json(_))
    ));
    // Single quotes are not JSON.
    assert!(matches!(
        decode_line(b"CALL gpio_pin_read {'pin': 4}"),
        Err(Error::Json(_))
    ));
    for line in [
        b"CALL gpio_pin_read {\"pin\": 4".as_slice(),
        b"CALL gpio_pin_read {\"pin\": }".as_slice(),
        b"CALL gpio_pin_read {'pin': 4}".as_slice(),
    ] {
        assert_eq!(variant_of(line), RepairVariant::Malformed);
    }
}

#[test]
fn rejects_trailing_bytes() {
    // A second CALL on the line is trailing bytes, not a second command.
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 4} CALL gpio_pin_read {\"pin\": 5}"),
        Err(Error::TrailingBytes)
    );
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 4} "),
        Err(Error::TrailingBytes),
        "even one trailing space is malformed"
    );
}

#[test]
fn rejects_duplicate_keys() {
    assert!(matches!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 4, \"pin\": 5}"),
        Err(Error::Json(_))
    ));
}

#[test]
fn rejects_excessive_depth() {
    assert!(
        matches!(
            decode_line(b"CALL gpio_pin_read {\"pin\": {\"a\": {\"b\": {\"c\": 1}}}}"),
            Err(Error::Json(_))
        ),
        "depth 4 exceeds the max of 3"
    );
    // Depth exactly 3 parses (then fails schema: pin must be an integer).
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": {\"a\": {\"b\": 1}}}"),
        Err(Error::BadFieldType {
            field: esper_core::error::Field::Pin,
            expected: "integer 0..=7",
        })
    );
}

#[test]
fn rejects_non_object_args() {
    assert_eq!(
        decode_line(b"CALL gpio_pin_read [4]"),
        Err(Error::ArgsNotObject)
    );
    assert_eq!(
        decode_line(b"CALL gpio_pin_read 4"),
        Err(Error::ArgsNotObject)
    );
}

#[test]
fn rejects_schema_violations_as_invalid_args() {
    use esper_core::error::Field;
    // Missing field.
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {}"),
        Err(Error::MissingField(Field::Pin))
    );
    assert_eq!(
        decode_line(b"CALL gpio_pin_write {\"pin\": 4}"),
        Err(Error::MissingField(Field::Level))
    );
    // Unexpected field.
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 4, \"level\": \"high\"}"),
        Err(Error::UnexpectedField)
    );
    // Wrong type.
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": \"4\"}"),
        Err(Error::BadFieldType {
            field: Field::Pin,
            expected: "integer 0..=7",
        })
    );
    // Pin out of range.
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 9}"),
        Err(Error::PinOutOfRange { pin: 9 })
    );
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 8}"),
        Err(Error::PinOutOfRange { pin: 8 })
    );
    // Bad enum value.
    assert_eq!(
        decode_line(b"CALL gpio_pin_write {\"pin\": 4, \"level\": \"medium\"}"),
        Err(Error::BadFieldValue {
            field: Field::Level
        })
    );
    for line in [
        b"CALL gpio_pin_read {}".as_slice(),
        b"CALL gpio_pin_read {\"pin\": 9}".as_slice(),
        b"CALL gpio_pin_write {\"pin\": 4, \"level\": \"medium\"}".as_slice(),
        b"CALL gpio_pin_read {\"pin\": 4, \"extra\": 1}".as_slice(),
    ] {
        assert_eq!(variant_of(line), RepairVariant::InvalidArgs);
    }
}

#[test]
fn rejects_bad_ask_shape() {
    use esper_core::error::Field;
    assert_eq!(
        decode_line(b"ASK {\"prompt\": \"x\", \"schema\": 2}"),
        Err(Error::BadFieldValue {
            field: Field::Schema
        })
    );
    assert_eq!(
        decode_line(b"ASK {\"schema\": 1}"),
        Err(Error::MissingField(Field::Prompt))
    );
    assert_eq!(
        decode_line(b"ASK {\"prompt\": \"x\"}"),
        Err(Error::MissingField(Field::Schema))
    );
}

#[test]
fn rejects_overlong_prompt_and_summary() {
    use esper_core::error::Field;
    let mut ask = b"ASK {\"prompt\": \"".to_vec();
    ask.extend(core::iter::repeat_n(b'p', 257));
    ask.extend(b"\", \"schema\": 1}");
    assert_eq!(
        decode_line(&ask),
        Err(Error::FieldTooLong {
            field: Field::Prompt,
            max: 256
        })
    );
    let mut finish = b"FINISH {\"status\": \"completed\", \"summary\": \"".to_vec();
    finish.extend(core::iter::repeat_n(b's', 257));
    finish.extend(b"\"}");
    assert_eq!(
        decode_line(&finish),
        Err(Error::FieldTooLong {
            field: Field::Summary,
            max: 256
        })
    );
}

#[test]
fn prompt_and_summary_at_exactly_256_bytes_are_accepted() {
    let mut ask = b"ASK {\"prompt\": \"".to_vec();
    ask.extend(core::iter::repeat_n(b'p', 256));
    ask.extend(b"\", \"schema\": 1}");
    assert!(decode_line(&ask).is_ok());
}

#[test]
fn rejects_bad_finish_status() {
    use esper_core::error::Field;
    assert_eq!(
        decode_line(b"FINISH {\"status\": \"done\", \"summary\": \"x\"}"),
        Err(Error::BadFieldValue {
            field: Field::Status
        })
    );
}

#[test]
fn level_accessor_rejects_read_calls() {
    use esper_core::error::Field;
    match decode_line(b"CALL gpio_pin_read {\"pin\": 4}").expect("valid") {
        Decision::Call(call) => {
            assert_eq!(call.level(), Err(Error::MissingField(Field::Level)));
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn repair_hints_are_bounded_and_categorized() {
    let hint = Error::UnknownToolName
        .repair_hint()
        .expect("decoder rejection");
    assert_eq!(hint.variant, RepairVariant::Malformed);
    assert_ne!(hint.field, "");
    assert_ne!(hint.expected, "");
    assert_ne!(hint.received, "");

    let hint = Error::PinOutOfRange { pin: 9 }
        .repair_hint()
        .expect("decoder rejection");
    assert_eq!(hint.variant, RepairVariant::InvalidArgs);
    assert_eq!(hint.field, "pin");
    assert_eq!(hint.expected, "0..=7");

    // Non-decoder errors carry no repair hint.
    assert!(
        Error::BudgetExhausted(esper_core::budget::BudgetUnit::ModelTurns)
            .repair_hint()
            .is_none()
    );
}

#[test]
fn finish_status_and_level_spellings() {
    assert_eq!(
        FinishStatus::from_bytes(b"completed"),
        Ok(FinishStatus::Completed)
    );
    assert!(FinishStatus::from_bytes(b"done").is_err());
    assert_eq!(Level::from_bytes(b"low"), Ok(Level::Low));
    assert_eq!(Level::from_bytes(b"high"), Ok(Level::High));
    assert!(Level::from_bytes(b"LOW").is_err());
}
