//! JSON parser tests: strictness rules the decoder relies on.

use esper_core::json::{JsonError, JsonValue, Parser, parse_u8};

fn parse_all(input: &[u8]) -> Result<(), JsonError> {
    let mut parser = Parser::new(input);
    let value = parser.parse_value()?;
    drain(value)?;
    parser.finish()
}

fn drain(value: JsonValue<'_, '_>) -> Result<(), JsonError> {
    match value {
        JsonValue::Array(mut cursor) => {
            while let Some(v) = cursor.next_value()? {
                drain(v)?;
            }
        }
        JsonValue::Object(mut cursor) => {
            while let Some((_, v)) = cursor.next_entry()? {
                drain(v)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[test]
fn parses_all_scalar_kinds() {
    for input in [
        b"null".as_slice(),
        b"true".as_slice(),
        b"false".as_slice(),
        b"0".as_slice(),
        b"-12".as_slice(),
        b"3.14".as_slice(),
        b"1e3".as_slice(),
        b"\"hello\"".as_slice(),
        b"\"esc \\\" \\\\ \\/ \\b \\f \\n \\r \\t \\u00e9\"".as_slice(),
    ] {
        assert!(parse_all(input).is_ok(), "{input:?}");
    }
}

#[test]
fn object_entries_come_back_in_order() {
    let mut parser = Parser::new(b"{\"a\": 1, \"b\": \"x\"}");
    match parser.parse_value().expect("valid") {
        JsonValue::Object(mut cursor) => {
            let (k1, _) = cursor.next_entry().expect("entry").expect("first");
            let (k2, _) = cursor.next_entry().expect("entry").expect("second");
            assert_eq!(k1, b"a");
            assert_eq!(k2, b"b");
            assert!(cursor.next_entry().expect("end").is_none());
        }
        _ => panic!("expected object"),
    }
    parser.finish().expect("no trailing bytes");
}

#[test]
fn string_content_is_raw_between_quotes() {
    let mut parser = Parser::new(b"{\"s\": \"a\\nb\"}");
    match parser.parse_value().expect("valid") {
        JsonValue::Object(mut cursor) => {
            let (_, v) = cursor.next_entry().expect("entry").expect("one");
            match v {
                JsonValue::Str(raw) => assert_eq!(raw, b"a\\nb"),
                _ => panic!("expected string"),
            }
        }
        _ => panic!("expected object"),
    }
}

#[test]
fn depth_exactly_three_is_allowed() {
    assert!(parse_all(b"{\"a\": {\"b\": {\"c\": 1}}}").is_ok());
}

#[test]
fn depth_four_is_rejected() {
    assert_eq!(
        parse_all(b"{\"a\": {\"b\": {\"c\": {\"d\": 1}}}}"),
        Err(JsonError::DepthExceeded)
    );
    assert_eq!(parse_all(b"[[[[1]]]]"), Err(JsonError::DepthExceeded));
}

#[test]
fn duplicate_keys_are_rejected() {
    assert_eq!(
        parse_all(b"{\"a\": 1, \"a\": 2}"),
        Err(JsonError::DuplicateKey)
    );
}

#[test]
fn more_than_eight_keys_are_rejected() {
    assert_eq!(
        parse_all(b"{\"1\":1,\"2\":2,\"3\":3,\"4\":4,\"5\":5,\"6\":6,\"7\":7,\"8\":8,\"9\":9}"),
        Err(JsonError::TooManyKeys)
    );
    assert!(
        parse_all(b"{\"1\":1,\"2\":2,\"3\":3,\"4\":4,\"5\":5,\"6\":6,\"7\":7,\"8\":8}").is_ok()
    );
}

#[test]
fn trailing_bytes_are_rejected() {
    assert_eq!(parse_all(b"{} "), Err(JsonError::TrailingBytes));
    assert_eq!(parse_all(b"{}x"), Err(JsonError::TrailingBytes));
    assert_eq!(parse_all(b"{}[]"), Err(JsonError::TrailingBytes));
}

#[test]
fn bad_escapes_are_rejected() {
    assert_eq!(parse_all(b"\"\\x\""), Err(JsonError::BadEscape));
    assert_eq!(parse_all(b"\"\\u12\""), Err(JsonError::BadEscape));
    assert_eq!(parse_all(b"\"\\u12zz\""), Err(JsonError::BadEscape));
}

#[test]
fn unterminated_values_are_rejected() {
    assert_eq!(parse_all(b"\"abc"), Err(JsonError::UnterminatedString));
    assert_eq!(parse_all(b"{\"a\": 1"), Err(JsonError::UnexpectedEnd));
    assert_eq!(parse_all(b""), Err(JsonError::UnexpectedEnd));
}

#[test]
fn bad_numbers_are_rejected() {
    for input in [
        b"01".as_slice(),
        b"1.".as_slice(),
        b".5".as_slice(),
        b"1e".as_slice(),
        b"--1".as_slice(),
        b"+1".as_slice(),
    ] {
        assert_eq!(parse_all(input), Err(JsonError::BadNumber), "{input:?}");
    }
}

#[test]
fn unescaped_control_characters_are_rejected() {
    assert_eq!(parse_all(b"\"a\x01b\""), Err(JsonError::UnexpectedByte));
}

#[test]
fn parse_u8_accepts_0_to_255() {
    assert_eq!(parse_u8(b"0"), Ok(0));
    assert_eq!(parse_u8(b"4"), Ok(4));
    assert_eq!(parse_u8(b"255"), Ok(255));
    assert_eq!(parse_u8(b"256"), Err(JsonError::BadNumber));
    assert_eq!(parse_u8(b""), Err(JsonError::BadNumber));
    assert_eq!(parse_u8(b"-1"), Err(JsonError::BadNumber));
    assert_eq!(parse_u8(b"1.0"), Err(JsonError::BadNumber));
    assert_eq!(parse_u8(b"1234"), Err(JsonError::BadNumber));
}

#[test]
fn whitespace_between_tokens_is_allowed() {
    assert!(parse_all(b"{\n  \"a\"\t:\r\n [1, 2] }").is_ok());
}
