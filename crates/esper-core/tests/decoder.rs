//! Decoder golden vectors: valid lines plus every invalid class (§4.3, §17.5).

use esper_core::decision::{
    Decision, FinishStatus, Level, MAX_ARGS_BYTES, PIN_SELECT_SCHEMA, StatusDetail, ToolArgs,
    decode_line,
};
use esper_core::error::{Error, Field, RepairVariant};
use esper_core::ids::{Digest, Pin, ToolId};
use esper_core::registry::{
    DEVICE_STATUS_REPORT_ID, GPIO_PIN_READ_ID, GPIO_PIN_WRITE_ID, SENSOR_SAMPLE_READ_ID,
    TIMER_DELAY_WAIT_ID, TIMER_UPTIME_READ_ID, catalog,
};
use esper_protocol::{BoundArgs, BoundField, MAX_ARGS_PER_TOOL, Scalar};

const fn is_call(decision: &Decision<'_>) -> bool {
    matches!(decision, Decision::Call(_))
}

fn decode_call_args(line: &[u8], args: &[u8]) -> ToolArgs {
    match decode_line(line).expect("valid CALL") {
        Decision::Call(call) => {
            assert_eq!(call.args_bytes(), args);
            *call.args()
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn decodes_call_read() {
    let line = b"CALL gpio_pin_read {\"pin\": 4}";
    match decode_line(line).expect("valid CALL") {
        Decision::Call(call) => {
            assert_eq!(call.tool().get(), GPIO_PIN_READ_ID);
            assert_eq!(call.args_bytes(), b"{\"pin\": 4}".as_slice());
            assert_eq!(call.args_digest(), Digest::of_bytes(b"{\"pin\": 4}"));
            assert_eq!(
                call.args(),
                &ToolArgs::GpioPinRead {
                    pin: Pin::new(4).expect("pin")
                }
            );
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
            assert_eq!(
                call.args(),
                &ToolArgs::GpioPinWrite {
                    pin: Pin::new(4).expect("pin"),
                    level: Level::High,
                }
            );
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn decodes_call_write_low_level() {
    let line = b"CALL gpio_pin_write {\"level\":\"low\",\"pin\":0}";
    match decode_line(line).expect("valid CALL") {
        Decision::Call(call) => {
            assert_eq!(
                call.args(),
                &ToolArgs::GpioPinWrite {
                    pin: Pin::new(0).expect("pin"),
                    level: Level::Low,
                }
            );
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn decodes_sensor_sample_read() {
    assert_eq!(
        decode_call_args(
            b"CALL sensor_sample_read {\"sensor\": 2}",
            b"{\"sensor\": 2}"
        ),
        ToolArgs::SensorSampleRead { sensor: 2 }
    );
}

#[test]
fn decodes_timer_uptime_read() {
    let line = b"CALL timer_uptime_read {}";
    match decode_line(line).expect("valid CALL") {
        Decision::Call(call) => {
            assert_eq!(call.tool().get(), TIMER_UPTIME_READ_ID);
            assert_eq!(call.args(), &ToolArgs::TimerUptimeRead);
        }
        other => panic!("expected Call, got {other:?}"),
    }
}

#[test]
fn decodes_timer_delay_wait() {
    assert_eq!(
        decode_call_args(b"CALL timer_delay_wait {\"ms\": 250}", b"{\"ms\": 250}"),
        ToolArgs::TimerDelayWait { ms: 250 }
    );
}

#[test]
fn decodes_device_status_report() {
    assert_eq!(
        decode_call_args(
            b"CALL device_status_report {\"detail\": \"full\"}",
            b"{\"detail\": \"full\"}"
        ),
        ToolArgs::DeviceStatusReport {
            detail: StatusDetail::Full
        }
    );
    assert_eq!(
        decode_call_args(
            b"CALL device_status_report {\"detail\":\"summary\"}",
            b"{\"detail\":\"summary\"}"
        ),
        ToolArgs::DeviceStatusReport {
            detail: StatusDetail::Summary
        }
    );
}

/// Bind exhaustiveness: every catalog tool decodes its own `example_ok`
/// to the matching `ToolArgs` shape (§17.1: the table drives the decoder).
#[test]
fn every_catalog_tool_binds_its_example_ok() {
    for entry in catalog() {
        let mut line = b"CALL ".to_vec();
        line.extend(entry.name.as_bytes());
        line.push(b' ');
        line.extend(entry.example_ok.as_bytes());
        match decode_line(&line).expect("example_ok must decode") {
            Decision::Call(call) => {
                assert_eq!(call.tool(), ToolId::new(entry.id));
                assert_eq!(call.args_bytes(), entry.example_ok.as_bytes());
                let shape_ok = matches!(
                    (entry.id, call.args()),
                    (GPIO_PIN_READ_ID, ToolArgs::GpioPinRead { .. })
                        | (GPIO_PIN_WRITE_ID, ToolArgs::GpioPinWrite { .. })
                        | (SENSOR_SAMPLE_READ_ID, ToolArgs::SensorSampleRead { .. })
                        | (TIMER_UPTIME_READ_ID, ToolArgs::TimerUptimeRead)
                        | (TIMER_DELAY_WAIT_ID, ToolArgs::TimerDelayWait { .. })
                        | (DEVICE_STATUS_REPORT_ID, ToolArgs::DeviceStatusReport { .. })
                );
                assert!(shape_ok, "tool {} bound to the wrong shape", entry.name);
            }
            other => panic!("expected Call, got {other:?}"),
        }
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

// --- `ToolArgs::bind` unit tests: the fail-closed paths unreachable via
// the decoder (which always runs `validate` first). ---

const fn bound_single(name: &'static str, value: Scalar) -> BoundArgs {
    let mut fields = [None; MAX_ARGS_PER_TOOL];
    fields[0] = Some(BoundField { name, value });
    BoundArgs { fields, len: 1 }
}

const fn bound_pair(
    name_a: &'static str,
    value_a: Scalar,
    name_b: &'static str,
    value_b: Scalar,
) -> BoundArgs {
    let mut fields = [None; MAX_ARGS_PER_TOOL];
    fields[0] = Some(BoundField {
        name: name_a,
        value: value_a,
    });
    fields[1] = Some(BoundField {
        name: name_b,
        value: value_b,
    });
    BoundArgs { fields, len: 2 }
}

const fn bound_empty() -> BoundArgs {
    BoundArgs {
        fields: [None; MAX_ARGS_PER_TOOL],
        len: 0,
    }
}

#[test]
fn bind_rejects_unknown_tool_id() {
    assert_eq!(
        ToolArgs::bind(ToolId::new(7), &bound_empty()),
        Err(Error::UnknownToolName)
    );
    assert_eq!(
        ToolArgs::bind(ToolId::new(0), &bound_empty()),
        Err(Error::UnknownToolName)
    );
}

#[test]
fn bind_fails_closed_on_absent_fields() {
    let empty = bound_empty();
    assert_eq!(
        ToolArgs::bind(ToolId::new(GPIO_PIN_READ_ID), &empty),
        Err(Error::MissingField(Field::Pin))
    );
    assert_eq!(
        ToolArgs::bind(ToolId::new(GPIO_PIN_WRITE_ID), &empty),
        Err(Error::MissingField(Field::Pin))
    );
    assert_eq!(
        ToolArgs::bind(ToolId::new(SENSOR_SAMPLE_READ_ID), &empty),
        Err(Error::MissingField(Field::Sensor))
    );
    assert_eq!(
        ToolArgs::bind(ToolId::new(TIMER_DELAY_WAIT_ID), &empty),
        Err(Error::MissingField(Field::Ms))
    );
    assert_eq!(
        ToolArgs::bind(ToolId::new(DEVICE_STATUS_REPORT_ID), &empty),
        Err(Error::MissingField(Field::Detail))
    );
    // The argless tool binds the empty shape.
    assert_eq!(
        ToolArgs::bind(ToolId::new(TIMER_UPTIME_READ_ID), &empty),
        Ok(ToolArgs::TimerUptimeRead)
    );
}

#[test]
fn bind_rejects_wrong_scalar_kinds() {
    // `validate` would never produce these; `bind` still fails closed.
    assert_eq!(
        ToolArgs::bind(
            ToolId::new(GPIO_PIN_READ_ID),
            &bound_single("pin", Scalar::U16(4))
        ),
        Err(Error::BadFieldType {
            field: Field::Pin,
            expected: "integer 0..=7",
        })
    );
    assert_eq!(
        ToolArgs::bind(
            ToolId::new(TIMER_DELAY_WAIT_ID),
            &bound_single("ms", Scalar::U8(5))
        ),
        Err(Error::BadFieldType {
            field: Field::Ms,
            expected: "integer 1..=5000",
        })
    );
    assert_eq!(
        ToolArgs::bind(
            ToolId::new(DEVICE_STATUS_REPORT_ID),
            &bound_single("detail", Scalar::U8(0))
        ),
        Err(Error::BadFieldType {
            field: Field::Detail,
            expected: "\"summary\" | \"full\"",
        })
    );
}

#[test]
fn bind_rejects_out_of_range_values_defensively() {
    // `validate` proves the ranges, so these only fire for direct
    // `bind` callers — but they must fail closed, never smuggle.
    assert_eq!(
        ToolArgs::bind(
            ToolId::new(GPIO_PIN_READ_ID),
            &bound_single("pin", Scalar::U8(9))
        ),
        Err(Error::PinOutOfRange { pin: 9 })
    );
    assert_eq!(
        ToolArgs::bind(
            ToolId::new(SENSOR_SAMPLE_READ_ID),
            &bound_single("sensor", Scalar::U8(4))
        ),
        Err(Error::BadFieldValue {
            field: Field::Sensor
        })
    );
    assert_eq!(
        ToolArgs::bind(
            ToolId::new(TIMER_DELAY_WAIT_ID),
            &bound_single("ms", Scalar::U16(0))
        ),
        Err(Error::BadFieldValue { field: Field::Ms })
    );
    assert_eq!(
        ToolArgs::bind(
            ToolId::new(GPIO_PIN_WRITE_ID),
            &bound_pair("pin", Scalar::U8(4), "level", Scalar::Enum(2))
        ),
        Err(Error::BadFieldValue {
            field: Field::Level
        })
    );
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
    // E2 note: where a scalar is expected, a nested composite is a
    // schema violation either way. The contract validator reports the
    // mistyped value (`WrongType`) without descending into it, so depth
    // faults in scalar position classify as `InvalidArgs`, not
    // `Malformed`. The input is still rejected and still consumes repair
    // allowance; depth is enforced as `Json` wherever the parser (or the
    // ASK/FINISH envelope drain) actually descends.
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": {\"a\": {\"b\": {\"c\": 1}}}}"),
        Err(Error::BadFieldType {
            field: esper_core::error::Field::Pin,
            expected: "integer 0..=7",
        })
    );
    assert_eq!(
        variant_of(b"CALL gpio_pin_read {\"pin\": {\"a\": {\"b\": {\"c\": 1}}}}"),
        RepairVariant::InvalidArgs
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
    // Out of range: the contract validator reports the value fault, not
    // the pin number (E2 change: the hand-written `PinOutOfRange` path
    // is gone from the decoder; `bind` still produces it defensively).
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 9}"),
        Err(Error::BadFieldValue { field: Field::Pin })
    );
    assert_eq!(
        decode_line(b"CALL gpio_pin_read {\"pin\": 8}"),
        Err(Error::BadFieldValue { field: Field::Pin })
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
fn rejects_new_tool_schema_violations_as_invalid_args() {
    // Sensor out of range.
    assert_eq!(
        decode_line(b"CALL sensor_sample_read {\"sensor\": 4}"),
        Err(Error::BadFieldValue {
            field: Field::Sensor
        })
    );
    assert_eq!(
        decode_line(b"CALL sensor_sample_read {}"),
        Err(Error::MissingField(Field::Sensor))
    );
    assert_eq!(
        decode_line(b"CALL sensor_sample_read {\"sensor\": \"0\"}"),
        Err(Error::BadFieldType {
            field: Field::Sensor,
            expected: "integer 0..=3",
        })
    );
    // Delay bounds: 1..=5000.
    assert_eq!(
        decode_line(b"CALL timer_delay_wait {\"ms\": 0}"),
        Err(Error::BadFieldValue { field: Field::Ms })
    );
    assert_eq!(
        decode_line(b"CALL timer_delay_wait {\"ms\": 5001}"),
        Err(Error::BadFieldValue { field: Field::Ms })
    );
    assert_eq!(
        decode_line(b"CALL timer_delay_wait {}"),
        Err(Error::MissingField(Field::Ms))
    );
    // The argless tool rejects any field.
    assert_eq!(
        decode_line(b"CALL timer_uptime_read {\"ms\": 1}"),
        Err(Error::UnexpectedField)
    );
    // Bad enum value for detail.
    assert_eq!(
        decode_line(b"CALL device_status_report {\"detail\": \"verbose\"}"),
        Err(Error::BadFieldValue {
            field: Field::Detail
        })
    );
    assert_eq!(
        decode_line(b"CALL device_status_report {}"),
        Err(Error::MissingField(Field::Detail))
    );
    for line in [
        b"CALL sensor_sample_read {\"sensor\": 4}".as_slice(),
        b"CALL timer_delay_wait {\"ms\": 0}".as_slice(),
        b"CALL timer_uptime_read {\"ms\": 1}".as_slice(),
        b"CALL device_status_report {\"detail\": \"verbose\"}".as_slice(),
    ] {
        assert_eq!(variant_of(line), RepairVariant::InvalidArgs);
    }
}

#[test]
fn rejects_bad_ask_shape() {
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
    assert_eq!(
        decode_line(b"FINISH {\"status\": \"done\", \"summary\": \"x\"}"),
        Err(Error::BadFieldValue {
            field: Field::Status
        })
    );
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

    let hint = Error::BadFieldValue { field: Field::Ms }
        .repair_hint()
        .expect("decoder rejection");
    assert_eq!(hint.variant, RepairVariant::InvalidArgs);
    assert_eq!(hint.field, "ms");
    assert_eq!(hint.expected, "1..=5000");

    // Non-decoder errors carry no repair hint.
    assert!(
        Error::BudgetExhausted(esper_core::budget::BudgetUnit::ModelTurns)
            .repair_hint()
            .is_none()
    );
}

#[test]
fn finish_status_spelling() {
    assert_eq!(
        FinishStatus::from_bytes(b"completed"),
        Ok(FinishStatus::Completed)
    );
    assert!(FinishStatus::from_bytes(b"done").is_err());
}
