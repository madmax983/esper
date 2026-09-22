//! The single contract source for the E2 typed native tool catalog (SPEC §17).
//!
//! [`CATALOG`] is the one table everything else derives from: the
//! allocation-free validator, the compact model-facing signatures, the
//! JSON Schema export, and the golden argument fixtures. Tool ids 1 and
//! 2 keep their E0/E1 assignments; the table never reorders or renumbers
//! an entry (stable ids are part of run identity via `tool_catalog_hash`).

/// Permission classes. Variant names are identical to `esper-core`'s
/// `registry::PermissionClass`; the core crew adopts these in E2 (SPEC
/// §17.4) instead of keeping a second copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionClass {
    /// Read-only observation; no verification needed.
    ReadOnly,
    /// Set-operation writes; re-dispatch with the same args is safe.
    IdempotentWrite,
    /// Reserved; no E2 tool carries it, and no E2 capability set grants it.
    SensitiveWrite,
    /// Reserved; no E2 tool carries it, and no E2 capability set grants it.
    Irreversible,
}

impl PermissionClass {
    /// The stable name used in signatures and reason bytes.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read_only",
            Self::IdempotentWrite => "idempotent_write",
            Self::SensitiveWrite => "sensitive_write",
            Self::Irreversible => "irreversible",
        }
    }
}

/// How a mutating tool's effect is independently confirmed (SPEC §9).
/// Variant names are identical to `esper-core`'s
/// `registry::VerificationStrategy`; the core crew adopts these in E2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerificationStrategy {
    /// Read-only tools: nothing to verify.
    None,
    /// Independent read-back of the target state.
    ReadBack,
}

impl VerificationStrategy {
    /// The stable name used in signatures.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ReadBack => "read_back",
        }
    }
}

/// How re-dispatch under the same `EffectId` behaves (SPEC §10.3).
/// Variant names are identical to `esper-core`'s
/// `registry::IdempotencyStrategy`; the core crew adopts these in E2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdempotencyStrategy {
    /// Read-only tools: no effect to de-duplicate.
    NotApplicable,
    /// Reapplication is a set-operation; safe under at-least-once.
    SetOperation,
}

impl IdempotencyStrategy {
    /// The stable name.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::NotApplicable => "n/a",
            Self::SetOperation => "set_operation",
        }
    }
}

/// The kind of one tool argument.
///
/// The set is minimal on purpose: every E2 tool's schema is expressible
/// with bounded integers and closed string enums. Strings, arrays, and
/// nested objects are not model inputs in E2.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArgKind {
    /// An integer in the closed range `lo..=hi`, spelled as a plain JSON number.
    U8 {
        /// Smallest accepted value.
        lo: u8,
        /// Largest accepted value.
        hi: u8,
    },
    /// An integer in the closed range `lo..=hi`, spelled as a plain JSON number.
    U16 {
        /// Smallest accepted value.
        lo: u16,
        /// Largest accepted value.
        hi: u16,
    },
    /// A string that must equal one entry of `options` byte-for-byte.
    /// The bound value is the matching option's index.
    Enum {
        /// The allowed spellings, in stable order.
        options: &'static [&'static str],
    },
}

/// One argument of a tool contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArgSpec {
    /// The JSON field name.
    pub name: &'static str,
    /// The accepted type and range.
    pub kind: ArgKind,
    /// Whether the field must be present. All E2 tools require all args.
    pub required: bool,
}

/// One static tool contract: the normative source of truth (SPEC §17).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolContract {
    /// Stable numeric id. Ids 1-2 keep their E0/E1 assignments.
    pub id: u8,
    /// Canonical registry name (`domain_resource_verb`).
    pub name: &'static str,
    /// Entry schema version.
    pub schema_version: u8,
    /// Permission class.
    pub permission: PermissionClass,
    /// Verification strategy for mutating tools.
    pub verification: VerificationStrategy,
    /// Idempotency strategy for re-dispatch.
    pub idempotency: IdempotencyStrategy,
    /// Maximum result payload bytes.
    pub result_bound: u16,
    /// Compact model-facing description (also rendered into the signature).
    pub description: &'static str,
    /// The argument schema, in stable order.
    pub args: &'static [ArgSpec],
    /// Golden valid argument JSON. Must validate (asserted by tests).
    pub example_ok: &'static str,
    /// Golden invalid argument JSONs. Each must fail (asserted by tests).
    pub example_bad: &'static [&'static str],
}

/// Schema version of the E2 catalog entries.
pub const SCHEMA_VERSION: u8 = 1;

/// The E2 static catalog: exactly six tools (SPEC §17.2).
pub const CATALOG: [ToolContract; 6] = [
    ToolContract {
        id: 1,
        name: "gpio_pin_read",
        schema_version: SCHEMA_VERSION,
        permission: PermissionClass::ReadOnly,
        verification: VerificationStrategy::None,
        idempotency: IdempotencyStrategy::NotApplicable,
        result_bound: 64,
        description: "Read the logic level of a GPIO pin.",
        args: &[ArgSpec {
            name: "pin",
            kind: ArgKind::U8 { lo: 0, hi: 7 },
            required: true,
        }],
        example_ok: "{\"pin\":3}",
        example_bad: &[
            "{\"pin\":\"3\"}",
            "{\"pin\":8}",
            "{}",
            "{\"pin\":3,\"level\":\"high\"}",
            "{\"pin\":-1}",
        ],
    },
    ToolContract {
        id: 2,
        name: "gpio_pin_write",
        schema_version: SCHEMA_VERSION,
        permission: PermissionClass::IdempotentWrite,
        verification: VerificationStrategy::ReadBack,
        idempotency: IdempotencyStrategy::SetOperation,
        result_bound: 64,
        description: "Set the logic level of a GPIO pin. Re-dispatch is safe (set-operation).",
        args: &[
            ArgSpec {
                name: "pin",
                kind: ArgKind::U8 { lo: 0, hi: 7 },
                required: true,
            },
            ArgSpec {
                name: "level",
                kind: ArgKind::Enum {
                    options: &["low", "high"],
                },
                required: true,
            },
        ],
        example_ok: "{\"pin\":4,\"level\":\"high\"}",
        example_bad: &[
            "{\"pin\":4}",
            "{\"pin\":4,\"level\":\"HIGH\"}",
            "{\"pin\":9,\"level\":\"low\"}",
            "{\"pin\":4,\"level\":\"low\",\"extra\":1}",
            "{\"level\":\"high\",\"pin\":4,\"pin\":4}",
        ],
    },
    ToolContract {
        id: 3,
        name: "sensor_sample_read",
        schema_version: SCHEMA_VERSION,
        permission: PermissionClass::ReadOnly,
        verification: VerificationStrategy::None,
        idempotency: IdempotencyStrategy::NotApplicable,
        result_bound: 64,
        description: "Read one sensor channel. The value is the channel's fixed raw reading.",
        args: &[ArgSpec {
            name: "sensor",
            kind: ArgKind::U8 { lo: 0, hi: 3 },
            required: true,
        }],
        example_ok: "{\"sensor\":2}",
        example_bad: &[
            "{}",
            "{\"sensor\":4}",
            "{\"sensor\":\"0\"}",
            "{\"pin\":0}",
            "{\"sensor\":1.5}",
        ],
    },
    ToolContract {
        id: 4,
        name: "timer_uptime_read",
        schema_version: SCHEMA_VERSION,
        permission: PermissionClass::ReadOnly,
        verification: VerificationStrategy::None,
        idempotency: IdempotencyStrategy::NotApplicable,
        result_bound: 64,
        description: "Read the monotonic uptime clock, in milliseconds.",
        args: &[],
        example_ok: "{}",
        example_bad: &[
            "{\"ms\":1}",
            "[]",
            "null",
            "{\"a\":1,\"b\":2,\"c\":3,\"d\":4,\"e\":5,\"f\":6,\"g\":7,\"h\":8,\"i\":9}",
        ],
    },
    ToolContract {
        id: 5,
        name: "timer_delay_wait",
        schema_version: SCHEMA_VERSION,
        permission: PermissionClass::IdempotentWrite,
        verification: VerificationStrategy::ReadBack,
        idempotency: IdempotencyStrategy::SetOperation,
        result_bound: 64,
        description: "Advance the virtual clock by the given milliseconds. Redelivery under the same effect id does not advance the clock twice.",
        args: &[ArgSpec {
            name: "ms",
            kind: ArgKind::U16 { lo: 1, hi: 5000 },
            required: true,
        }],
        example_ok: "{\"ms\":250}",
        example_bad: &[
            "{\"ms\":0}",
            "{\"ms\":5001}",
            "{\"ms\":\"250\"}",
            "{}",
            "{\"ms\":100.5}",
        ],
    },
    ToolContract {
        id: 6,
        name: "device_status_report",
        schema_version: SCHEMA_VERSION,
        permission: PermissionClass::ReadOnly,
        verification: VerificationStrategy::None,
        idempotency: IdempotencyStrategy::NotApplicable,
        result_bound: 128,
        description: "Report device state. Summary is a small object; full adds pin and sensor detail.",
        args: &[ArgSpec {
            name: "detail",
            kind: ArgKind::Enum {
                options: &["summary", "full"],
            },
            required: true,
        }],
        example_ok: "{\"detail\":\"full\"}",
        example_bad: &[
            "{}",
            "{\"detail\":\"verbose\"}",
            "{\"detail\":0}",
            "{\"detail\":\"summary\",\"x\":null}",
        ],
    },
];

/// The six E2 tool contracts, in stable id order.
#[must_use]
pub const fn catalog() -> &'static [ToolContract] {
    &CATALOG
}

/// Look up a contract by stable numeric id.
#[must_use]
pub fn lookup_by_id(id: u8) -> Option<&'static ToolContract> {
    CATALOG.iter().find(|entry| entry.id == id)
}

/// Look up a contract by canonical name.
#[must_use]
pub fn lookup_by_name(name: &str) -> Option<&'static ToolContract> {
    CATALOG.iter().find(|entry| entry.name == name)
}

#[cfg(test)]
mod tests {
    use super::{CATALOG, catalog, lookup_by_id, lookup_by_name};
    use crate::validate::validate;

    #[test]
    fn catalog_holds_six_tools_with_stable_ids() {
        assert_eq!(CATALOG.len(), 6);
        let names = [
            "gpio_pin_read",
            "gpio_pin_write",
            "sensor_sample_read",
            "timer_uptime_read",
            "timer_delay_wait",
            "device_status_report",
        ];
        for (index, name) in names.iter().enumerate() {
            let entry = &CATALOG[index];
            assert_eq!(entry.name, *name);
            let expected_id = u8::try_from(index + 1).expect("catalog index fits in u8");
            assert_eq!(entry.id, expected_id);
            assert_eq!(lookup_by_id(expected_id).expect("id lookup").name, *name);
            assert_eq!(lookup_by_name(name).expect("name lookup").id, expected_id);
        }
    }

    #[test]
    fn gpio_ids_keep_e0_e1_assignments() {
        assert_eq!(lookup_by_name("gpio_pin_read").expect("read tool").id, 1);
        assert_eq!(lookup_by_name("gpio_pin_write").expect("write tool").id, 2);
    }

    #[test]
    fn every_example_ok_validates() {
        for tool in catalog() {
            validate(tool, tool.example_ok.as_bytes())
                .expect("the contract's own example_ok must validate");
        }
    }

    #[test]
    fn every_example_bad_fails() {
        for tool in catalog() {
            for bad in tool.example_bad {
                assert!(
                    validate(tool, bad.as_bytes()).is_err(),
                    "tool {} accepted its own bad example: {bad}",
                    tool.name
                );
            }
        }
    }

    #[test]
    fn each_tool_lists_at_least_four_bad_examples() {
        for tool in catalog() {
            assert!(
                tool.example_bad.len() >= 4,
                "tool {} needs at least four bad examples",
                tool.name
            );
        }
    }

    #[test]
    fn no_generic_network_access_in_catalog() {
        for tool in catalog() {
            for needle in ["http", "network", "socket", "url"] {
                assert!(
                    !tool.name.contains(needle),
                    "tool name {} looks like network access",
                    tool.name
                );
                assert!(
                    !tool.description.contains(needle),
                    "tool {} description looks like network access",
                    tool.name
                );
            }
        }
    }
}
