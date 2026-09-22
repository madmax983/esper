//! The generic allocation-free argument validator (SPEC §17.3).
//!
//! [`validate`] checks model-supplied JSON bytes against one
//! [`ToolContract`] and returns the bound
//! arguments ([`BoundArgs`]) the runtime dispatches on. It reuses the
//! strict parser in [`crate::json`]: depth over 3 and duplicate keys are
//! rejected by the parser itself, not by this module.
//!
//! Validation order is fixed: syntax first, then top-level object shape,
//! then per-field checks in JSON order, then required-field presence in
//! contract order. The first violation wins.

use thiserror::Error;

use crate::contract::{ArgKind, ArgSpec, ToolContract};
use crate::json::{self, JsonError, JsonValue};

/// Maximum arguments one tool may declare. The E2 catalog needs at most
/// two; four leaves headroom while keeping [`BoundArgs`] tiny.
pub const MAX_ARGS_PER_TOOL: usize = 4;

/// A bound argument value. All variants are `Copy`; nothing here borrows
/// the model input, so a `BoundArgs` stays valid after the input buffer
/// is reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    /// A validated `u8` in the spec's range.
    U8(u8),
    /// A validated `u16` in the spec's range.
    U16(u16),
    /// The index into the spec's `options` that the input spelled.
    Enum(u8),
}

/// One bound argument: the contract's field name and its checked value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundField {
    /// The field name, from the contract (never from model input).
    pub name: &'static str,
    /// The checked value.
    pub value: Scalar,
}

/// The validated arguments of one tool call, in contract order.
///
/// Fixed capacity ([`MAX_ARGS_PER_TOOL`]); the validator fills at most
/// one slot per declared argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundArgs {
    /// Bound fields; only `fields[..len]` is meaningful.
    pub fields: [Option<BoundField>; MAX_ARGS_PER_TOOL],
    /// How many of `fields` are bound.
    pub len: u8,
}

impl BoundArgs {
    /// Look up a bound value by field name.
    #[must_use]
    pub fn get(&self, name: &str) -> Option<Scalar> {
        self.fields
            .iter()
            .take(usize::from(self.len))
            .find_map(|slot| match slot {
                Some(field) if field.name == name => Some(field.value),
                _ => None,
            })
    }
}

/// What argument validation can reject.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ValidationError {
    /// Strict JSON syntax violation (depth, duplicates, bad numbers…).
    #[error("JSON syntax error: {0}")]
    Json(JsonError),
    /// The top-level JSON value is not an object.
    #[error("tool arguments are not a JSON object")]
    NotObject,
    /// A field outside the contract's schema is present.
    #[error("unexpected argument field")]
    UnknownArgument,
    /// A required contract argument is absent. Carries the contract's
    /// field name (never model bytes).
    #[error("missing required argument: {0}")]
    MissingArgument(&'static str),
    /// A field has the wrong JSON type for its kind.
    #[error("argument {0} has the wrong type")]
    WrongType(&'static str),
    /// A numeric field is outside its closed range.
    #[error("argument {0} is out of range")]
    OutOfRange(&'static str),
    /// A string field spells no entry of the enum's options.
    #[error("argument {0} is not one of the allowed values")]
    BadEnumValue(&'static str),
    /// The contract declares more arguments than [`MAX_ARGS_PER_TOOL`].
    /// Fail closed: a malformed contract never validates.
    #[error("tool declares too many arguments")]
    TooManyArguments,
}

/// Validate model-supplied JSON bytes against a tool contract.
///
/// Enforces: top-level object; no unknown fields; every required field
/// present; per-kind type, range, and enum checks. An empty object is
/// valid exactly when the contract declares no arguments. Duplicate keys
/// and nesting deeper than 3 are rejected by the strict parser underneath.
///
/// # Errors
///
/// Returns [`ValidationError`] for the first violation found, in the
/// fixed order: syntax, object shape, per-field checks (JSON order),
/// required presence (contract order).
pub fn validate(contract: &ToolContract, json: &[u8]) -> Result<BoundArgs, ValidationError> {
    if contract.args.len() > MAX_ARGS_PER_TOOL {
        return Err(ValidationError::TooManyArguments);
    }
    let mut parser = crate::json::Parser::new(json);
    let value = parser.parse_value().map_err(ValidationError::Json)?;
    let JsonValue::Object(mut cursor) = value else {
        return Err(ValidationError::NotObject);
    };
    // Scratch table: one (contract position, value) per known field.
    // Duplicate JSON keys are already rejected by the parser, so each
    // contract position appears at most once.
    let mut seen: [Option<(u8, Scalar)>; MAX_ARGS_PER_TOOL] = [None; MAX_ARGS_PER_TOOL];
    while let Some((key, value)) = cursor.next_entry().map_err(ValidationError::Json)? {
        let position = contract
            .args
            .iter()
            .position(|spec| spec.name.as_bytes() == key)
            .ok_or(ValidationError::UnknownArgument)?;
        let spec = &contract.args[position];
        let scalar = check_scalar(spec, &value)?;
        let slot = seen
            .iter_mut()
            .find(|slot| slot.is_none())
            .ok_or(ValidationError::TooManyArguments)?;
        let position_u8 = u8::try_from(position).map_err(|_| ValidationError::TooManyArguments)?;
        *slot = Some((position_u8, scalar));
    }
    parser.finish().map_err(ValidationError::Json)?;
    // Emit in contract order so dispatch sees deterministic field order
    // whatever order the model spelled the keys in.
    let mut bound = BoundArgs {
        fields: [None; MAX_ARGS_PER_TOOL],
        len: 0,
    };
    for (position, spec) in contract.args.iter().enumerate() {
        let found = seen.iter().find_map(|slot| match slot {
            Some((slot_position, value)) if usize::from(*slot_position) == position => Some(*value),
            _ => None,
        });
        match found {
            Some(value) => {
                let slot = bound
                    .fields
                    .get_mut(usize::from(bound.len))
                    .ok_or(ValidationError::TooManyArguments)?;
                *slot = Some(BoundField {
                    name: spec.name,
                    value,
                });
                bound.len = bound.len.saturating_add(1);
            }
            None if spec.required => {
                return Err(ValidationError::MissingArgument(spec.name));
            }
            None => {}
        }
    }
    Ok(bound)
}

/// Check one JSON value against one argument spec.
fn check_scalar(spec: &ArgSpec, value: &JsonValue<'_, '_>) -> Result<Scalar, ValidationError> {
    match spec.kind {
        ArgKind::U8 { lo, hi } => {
            let JsonValue::Number(raw) = value else {
                return Err(ValidationError::WrongType(spec.name));
            };
            let n = json::parse_u8(raw).map_err(ValidationError::Json)?;
            if n < lo || n > hi {
                return Err(ValidationError::OutOfRange(spec.name));
            }
            Ok(Scalar::U8(n))
        }
        ArgKind::U16 { lo, hi } => {
            let JsonValue::Number(raw) = value else {
                return Err(ValidationError::WrongType(spec.name));
            };
            let n = json::parse_u16(raw).map_err(ValidationError::Json)?;
            if n < lo || n > hi {
                return Err(ValidationError::OutOfRange(spec.name));
            }
            Ok(Scalar::U16(n))
        }
        ArgKind::Enum { options } => {
            let JsonValue::Str(raw) = value else {
                return Err(ValidationError::WrongType(spec.name));
            };
            // Raw-content comparison: escaped spellings of an option are
            // rejected, exactly like the E0/E1 decoder's Level::from_bytes.
            let index = options
                .iter()
                .position(|option| option.as_bytes() == *raw)
                .ok_or(ValidationError::BadEnumValue(spec.name))?;
            let index_u8 =
                u8::try_from(index).map_err(|_| ValidationError::BadEnumValue(spec.name))?;
            Ok(Scalar::Enum(index_u8))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{BoundArgs, MAX_ARGS_PER_TOOL, Scalar, ValidationError, validate};
    use crate::contract::{ToolContract, lookup_by_name};
    use crate::json::JsonError;

    fn tool(name: &str) -> &'static ToolContract {
        lookup_by_name(name).expect("test tool must exist")
    }

    #[test]
    fn bound_values_are_copy_and_lookup_by_name() {
        let bound = validate(tool("gpio_pin_write"), b"{\"pin\":4,\"level\":\"high\"}")
            .expect("valid args");
        assert_eq!(bound.get("pin"), Some(Scalar::U8(4)));
        assert_eq!(bound.get("level"), Some(Scalar::Enum(1)));
        assert_eq!(bound.get("nope"), None);
        assert_eq!(bound.len, 2);
    }

    #[test]
    fn field_order_follows_contract_not_json_order() {
        let bound =
            validate(tool("gpio_pin_write"), b"{\"level\":\"low\",\"pin\":0}").expect("valid args");
        let first = bound.fields[0].expect("first field").name;
        let second = bound.fields[1].expect("second field").name;
        assert_eq!((first, second), ("pin", "level"));
    }

    #[test]
    fn empty_object_validates_for_argless_tool() {
        let bound = validate(tool("timer_uptime_read"), b"{}").expect("empty object");
        assert_eq!(bound.len, 0);
        assert_eq!(bound.get("ms"), None);
    }

    #[test]
    fn empty_object_fails_where_args_are_required() {
        for name in ["gpio_pin_read", "gpio_pin_write", "sensor_sample_read"] {
            let err = validate(tool(name), b"{}").expect_err("must fail");
            assert!(
                matches!(err, ValidationError::MissingArgument(_)),
                "tool {name}: got {err:?}"
            );
        }
    }

    #[test]
    fn u16_range_edges_for_delay() {
        let delay = tool("timer_delay_wait");
        let ok = |bytes: &[u8]| validate(delay, bytes).expect("valid ms");
        assert_eq!(ok(b"{\"ms\":1}").get("ms"), Some(Scalar::U16(1)));
        assert_eq!(ok(b"{\"ms\":5000}").get("ms"), Some(Scalar::U16(5000)));
        for bad in [b"{\"ms\":0}".as_slice(), b"{\"ms\":5001}"] {
            assert_eq!(
                validate(delay, bad),
                Err(ValidationError::OutOfRange("ms")),
                "input {bad:?}"
            );
        }
    }

    #[test]
    fn enum_index_is_stable() {
        let status = tool("device_status_report");
        assert_eq!(
            validate(status, b"{\"detail\":\"summary\"}")
                .expect("valid")
                .get("detail"),
            Some(Scalar::Enum(0))
        );
        assert_eq!(
            validate(status, b"{\"detail\":\"full\"}")
                .expect("valid")
                .get("detail"),
            Some(Scalar::Enum(1))
        );
        assert_eq!(
            validate(status, b"{\"detail\":\"SUMMARY\"}"),
            Err(ValidationError::BadEnumValue("detail"))
        );
        // Escaped spellings are raw content, not the option.
        assert_eq!(
            validate(status, b"{\"detail\":\"\\u0073ummary\"}"),
            Err(ValidationError::BadEnumValue("detail"))
        );
    }

    #[test]
    fn wrong_types_rejected_per_kind() {
        let write = tool("gpio_pin_write");
        assert_eq!(
            validate(write, b"{\"pin\":\"4\",\"level\":\"high\"}"),
            Err(ValidationError::WrongType("pin"))
        );
        assert_eq!(
            validate(write, b"{\"pin\":4,\"level\":true}"),
            Err(ValidationError::WrongType("level"))
        );
        assert_eq!(
            validate(write, b"{\"pin\":null,\"level\":\"high\"}"),
            Err(ValidationError::WrongType("pin"))
        );
        assert_eq!(
            validate(write, b"{\"pin\":[4],\"level\":\"high\"}"),
            Err(ValidationError::WrongType("pin"))
        );
    }

    #[test]
    fn syntax_errors_surface_as_json_errors() {
        let read = tool("gpio_pin_read");
        assert_eq!(
            validate(read, b"{\"pin\":3"),
            Err(ValidationError::Json(JsonError::UnexpectedEnd))
        );
        assert_eq!(
            validate(read, b"{\"pin\":3, \"pin\":3}"),
            Err(ValidationError::Json(JsonError::DuplicateKey))
        );
        assert_eq!(
            validate(read, b"{\"pin\":3} trailing"),
            Err(ValidationError::Json(JsonError::TrailingBytes))
        );
        assert_eq!(
            validate(read, b"[{\"pin\":3}]"),
            Err(ValidationError::NotObject)
        );
    }

    #[test]
    fn unknown_fields_rejected_before_missing_check() {
        let write = tool("gpio_pin_write");
        // Unknown field wins over the missing `level`: JSON order first.
        assert_eq!(
            validate(write, b"{\"pin\":4,\"bogus\":1}"),
            Err(ValidationError::UnknownArgument)
        );
    }

    #[test]
    fn non_integer_numbers_rejected() {
        let read = tool("gpio_pin_read");
        assert_eq!(
            validate(read, b"{\"pin\":3.0}"),
            Err(ValidationError::Json(JsonError::BadNumber))
        );
        assert_eq!(
            validate(read, b"{\"pin\":03}"),
            Err(ValidationError::Json(JsonError::BadNumber))
        );
        let delay = tool("timer_delay_wait");
        assert_eq!(
            validate(delay, b"{\"ms\":-5}"),
            Err(ValidationError::Json(JsonError::BadNumber))
        );
    }

    #[test]
    fn bound_args_capacity_matches_spec() {
        assert_eq!(MAX_ARGS_PER_TOOL, 4);
        let bound = BoundArgs {
            fields: [None; MAX_ARGS_PER_TOOL],
            len: 0,
        };
        assert_eq!(bound.fields.len(), MAX_ARGS_PER_TOOL);
    }
}
