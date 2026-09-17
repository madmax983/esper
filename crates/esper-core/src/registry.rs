//! The static tool catalog, pin capabilities, and the policy gate (§5).
//!
//! The catalog is fixed at build time (ADR: static capability catalog).
//! The decoder validates arguments against each entry's static schema;
//! [`authorize_capability`] enforces the capability-set half of
//! `Authorize` (allowlist + pin membership). Device business rules —
//! e.g. whether the device reports a pin as output-capable — need the
//! device handle and stay in the runtime.

use crate::error::Error;
use crate::ids::{Pin, ToolId};

/// `ToolId` of `gpio_pin_read`.
pub const GPIO_PIN_READ_ID: u8 = 1;
/// `ToolId` of `gpio_pin_write`.
pub const GPIO_PIN_WRITE_ID: u8 = 2;
/// Canonical registry name of the read tool.
pub const GPIO_PIN_READ_NAME: &str = "gpio_pin_read";
/// Canonical registry name of the write tool.
pub const GPIO_PIN_WRITE_NAME: &str = "gpio_pin_write";
/// Result bound of the slice tools (§5.1): 64 bytes, never truncated.
pub const RESULT_BOUND_BYTES: u16 = 64;
/// Schema version of the slice catalog entries.
pub const CATALOG_SCHEMA_VERSION: u8 = 1;
/// Maximum pins in one capability set (§5.2).
pub const MAX_PINS_PER_SET: usize = 8;

/// Permission classes. `SensitiveWrite` and `Irreversible` exist as
/// variants, but no slice tool carries them and no capability set can
/// grant them, so `AwaitApproval` is unreachable in the slice (§1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PermissionClass {
    /// Read-only observation; no verification needed.
    ReadOnly,
    /// Set-operation writes; re-dispatch with the same args is safe.
    IdempotentWrite,
    /// Reserved; unreachable in the slice.
    SensitiveWrite,
    /// Reserved; unreachable in the slice.
    Irreversible,
}

impl PermissionClass {
    /// The stable name.
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

/// How a mutating tool's effect is independently confirmed (§9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerificationStrategy {
    /// Read-only tools: nothing to verify.
    None,
    /// Independent read-back of the target state.
    ReadBack,
}

/// How re-dispatch under the same `EffectId` behaves (§10.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum IdempotencyStrategy {
    /// Read-only tools: no effect to de-duplicate.
    NotApplicable,
    /// Reapplication is a set-operation; safe under at-least-once.
    SetOperation,
}

/// One static catalog entry (§5.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolEntry {
    /// Stable numeric id.
    pub id: ToolId,
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
    /// Compact model-facing description.
    pub description: &'static str,
}

/// The slice's static catalog: exactly two tools.
pub const CATALOG: [ToolEntry; 2] = [
    ToolEntry {
        id: ToolId::new(GPIO_PIN_READ_ID),
        name: GPIO_PIN_READ_NAME,
        schema_version: CATALOG_SCHEMA_VERSION,
        permission: PermissionClass::ReadOnly,
        verification: VerificationStrategy::None,
        idempotency: IdempotencyStrategy::NotApplicable,
        result_bound: RESULT_BOUND_BYTES,
        description: "Read the logic level of a GPIO pin.",
    },
    ToolEntry {
        id: ToolId::new(GPIO_PIN_WRITE_ID),
        name: GPIO_PIN_WRITE_NAME,
        schema_version: CATALOG_SCHEMA_VERSION,
        permission: PermissionClass::IdempotentWrite,
        verification: VerificationStrategy::ReadBack,
        idempotency: IdempotencyStrategy::SetOperation,
        result_bound: RESULT_BOUND_BYTES,
        description: "Set the logic level of a GPIO pin. Set-operation: re-dispatch is safe.",
    },
];

/// Look up a catalog entry by id.
#[must_use]
pub fn lookup_by_id(id: ToolId) -> Option<&'static ToolEntry> {
    CATALOG.iter().find(|entry| entry.id == id)
}

/// Look up a catalog entry by canonical name.
#[must_use]
pub fn lookup_by_name(name: &[u8]) -> Option<&'static ToolEntry> {
    CATALOG.iter().find(|entry| entry.name.as_bytes() == name)
}

/// The fixed capability set carried by the `RunSeed` (§5.2).
///
/// Pins are stored with explicit counts (`read_pins[..read_count]`);
/// counts beyond the storage fail closed wherever they are read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Capabilities {
    /// Pins the run may read.
    pub read_pins: [u8; MAX_PINS_PER_SET],
    /// How many of `read_pins` are valid.
    pub read_count: u8,
    /// Pins the run may write.
    pub write_pins: [u8; MAX_PINS_PER_SET],
    /// How many of `write_pins` are valid.
    pub write_count: u8,
}

impl Capabilities {
    /// An empty capability set: nothing may be read or written.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            read_pins: [0; MAX_PINS_PER_SET],
            read_count: 0,
            write_pins: [0; MAX_PINS_PER_SET],
            write_count: 0,
        }
    }

    /// Whether `pin` is in the read set.
    #[must_use]
    pub fn can_read(&self, pin: Pin) -> bool {
        set_contains(self.read_pins, self.read_count, pin.get())
    }

    /// Whether `pin` is in the write set.
    #[must_use]
    pub fn can_write(&self, pin: Pin) -> bool {
        set_contains(self.write_pins, self.write_count, pin.get())
    }

    /// Whether `self` grants anything `baseline` does not.
    ///
    /// After reboot the restored capabilities must never widen the
    /// `RunSeed`'s set; the crash oracle asserts `!restored.widens(seed)`.
    #[must_use]
    pub fn widens(&self, baseline: &Self) -> bool {
        set_widens(
            self.read_pins,
            self.read_count,
            baseline.read_pins,
            baseline.read_count,
        ) || set_widens(
            self.write_pins,
            self.write_count,
            baseline.write_pins,
            baseline.write_count,
        )
    }
}

/// The valid prefix of a pin set, or `None` when `count` claims more
/// pins than the storage holds (fail closed at every reader).
fn valid_set(set: &[u8; MAX_PINS_PER_SET], count: u8) -> Option<&[u8]> {
    set.get(..usize::from(count))
}
// (kept by reference: this one only reborrows for `get`)

/// Membership test over a counted pin set.
fn set_contains(set: [u8; MAX_PINS_PER_SET], count: u8, pin: u8) -> bool {
    valid_set(&set, count).is_some_and(|slice| slice.contains(&pin))
}

/// True when `set` holds any pin absent from `baseline`.
fn set_widens(
    set: [u8; MAX_PINS_PER_SET],
    count: u8,
    baseline: [u8; MAX_PINS_PER_SET],
    baseline_count: u8,
) -> bool {
    match (valid_set(&set, count), valid_set(&baseline, baseline_count)) {
        (Some(current), Some(base)) => current.iter().any(|pin| !base.contains(pin)),
        // Malformed counts are treated as widening: fail closed.
        _ => true,
    }
}

/// The static half of `Authorize`.
///
/// Allowlist, pin range (already proven by holding a [`Pin`]), and
/// capability-set membership, in the normative check order (§5.2:
/// capability first, then device truth, so denials never touch
/// hardware). Device business rules (pin direction as reported by the
/// device) are the runtime's `Authorize` step; this gate never sees the
/// device.
///
/// # Errors
///
/// Returns [`Error::PermissionDenied`] when the capability set refuses
/// the call — terminal `Denied`, never a repair turn.
pub fn authorize_capability(caps: &Capabilities, tool: &ToolEntry, pin: Pin) -> Result<(), Error> {
    let allowed = match tool.permission {
        PermissionClass::ReadOnly => caps.can_read(pin),
        PermissionClass::IdempotentWrite => caps.can_write(pin),
        // No capability set can grant these in the slice (§5.2): deny by
        // construction, keeping AwaitApproval unreachable.
        PermissionClass::SensitiveWrite | PermissionClass::Irreversible => false,
    };
    if allowed {
        Ok(())
    } else {
        Err(Error::PermissionDenied {
            tool: tool.id,
            pin: pin.get(),
        })
    }
}
