//! The static tool catalog, capabilities, and the policy gate (§5, §17).
//!
//! The catalog is fixed at build time (ADR: static capability catalog).
//! It lives in `esper-protocol` — the single contract source (SPEC
//! §17) — and is re-exported here, so exactly one definition of each
//! catalog type exists. The decoder validates arguments against the
//! contract table; [`authorize`] enforces the capability half of
//! `Authorize` (allowlist → capability, §5.2 v2). Device business rules
//! — e.g. whether the device reports a pin as output-capable — need the
//! device handle and stay in the runtime.

use crate::decision::ToolArgs;
use crate::error::Error;
use crate::ids::{Pin, ToolId};

// The E2 contract source: one table drives the validator, the
// signatures, and the JSON Schema (SPEC §17.1). Re-exported so the
// rest of the workspace names one catalog.
pub use esper_protocol::{
    ArgKind, ArgSpec, CATALOG, IdempotencyStrategy, PermissionClass, ToolContract,
    VerificationStrategy, catalog,
};

/// `ToolId` of `gpio_pin_read`.
pub const GPIO_PIN_READ_ID: u8 = 1;
/// `ToolId` of `gpio_pin_write`.
pub const GPIO_PIN_WRITE_ID: u8 = 2;
/// `ToolId` of `sensor_sample_read`.
pub const SENSOR_SAMPLE_READ_ID: u8 = 3;
/// `ToolId` of `timer_uptime_read`.
pub const TIMER_UPTIME_READ_ID: u8 = 4;
/// `ToolId` of `timer_delay_wait`.
pub const TIMER_DELAY_WAIT_ID: u8 = 5;
/// `ToolId` of `device_status_report`.
pub const DEVICE_STATUS_REPORT_ID: u8 = 6;
/// Canonical registry name of the read tool.
pub const GPIO_PIN_READ_NAME: &str = "gpio_pin_read";
/// Canonical registry name of the write tool.
pub const GPIO_PIN_WRITE_NAME: &str = "gpio_pin_write";
/// Canonical registry name of the sensor tool.
pub const SENSOR_SAMPLE_READ_NAME: &str = "sensor_sample_read";
/// Canonical registry name of the uptime tool.
pub const TIMER_UPTIME_READ_NAME: &str = "timer_uptime_read";
/// Canonical registry name of the delay tool.
pub const TIMER_DELAY_WAIT_NAME: &str = "timer_delay_wait";
/// Canonical registry name of the status tool.
pub const DEVICE_STATUS_REPORT_NAME: &str = "device_status_report";
/// Maximum pins in one capability set (§5.2).
pub const MAX_PINS_PER_SET: usize = 8;
/// Maximum sensors in one capability set (§5.2 v2).
pub const MAX_SENSORS_PER_SET: usize = 4;

/// Look up a catalog entry by id.
#[must_use]
pub fn lookup_by_id(id: ToolId) -> Option<&'static ToolContract> {
    esper_protocol::lookup_by_id(id.get())
}

/// Look up a catalog entry by canonical name.
///
/// The name arrives as bytes from the line grammar; it must be UTF-8 to
/// match a catalog name, and anything else is simply unknown.
#[must_use]
pub fn lookup_by_name(name: &[u8]) -> Option<&'static ToolContract> {
    let name = core::str::from_utf8(name).ok()?;
    esper_protocol::lookup_by_name(name)
}

/// The fixed capability set carried by the `RunSeed` (§5.2 v2).
///
/// Pins and sensors are stored with explicit counts
/// (`read_pins[..read_count]`); counts beyond the storage fail closed
/// wherever they are read. Absent grants mean denied: a capability set
/// without `sensors`, `allow_timer`, or `allow_status` refuses those
/// tools (§17.7).
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
    /// Sensors the run may sample.
    pub sensors: [u8; MAX_SENSORS_PER_SET],
    /// How many of `sensors` are valid.
    pub sensor_count: u8,
    /// Whether the run may use the timer tools.
    pub allow_timer: bool,
    /// Whether the run may use the status tool.
    pub allow_status: bool,
}

impl Capabilities {
    /// An empty capability set: nothing may be read, written, sampled,
    /// timed, or reported.
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            read_pins: [0; MAX_PINS_PER_SET],
            read_count: 0,
            write_pins: [0; MAX_PINS_PER_SET],
            write_count: 0,
            sensors: [0; MAX_SENSORS_PER_SET],
            sensor_count: 0,
            allow_timer: false,
            allow_status: false,
        }
    }

    /// Whether `pin` is in the read set.
    #[must_use]
    pub fn can_read(&self, pin: Pin) -> bool {
        set_contains(&self.read_pins, self.read_count, pin.get())
    }

    /// Whether `pin` is in the write set.
    #[must_use]
    pub fn can_write(&self, pin: Pin) -> bool {
        set_contains(&self.write_pins, self.write_count, pin.get())
    }

    /// Whether `sensor` is in the sample set.
    #[must_use]
    pub fn can_sample(&self, sensor: u8) -> bool {
        set_contains(&self.sensors, self.sensor_count, sensor)
    }

    /// Whether `self` grants anything `baseline` does not.
    ///
    /// After reboot the restored capabilities must never widen the
    /// `RunSeed`'s set; the crash oracle asserts `!restored.widens(seed)`.
    #[must_use]
    pub fn widens(&self, baseline: &Self) -> bool {
        set_widens(
            &self.read_pins,
            self.read_count,
            &baseline.read_pins,
            baseline.read_count,
        ) || set_widens(
            &self.write_pins,
            self.write_count,
            &baseline.write_pins,
            baseline.write_count,
        ) || set_widens(
            &self.sensors,
            self.sensor_count,
            &baseline.sensors,
            baseline.sensor_count,
        ) || (self.allow_timer && !baseline.allow_timer)
            || (self.allow_status && !baseline.allow_status)
    }
}

/// The valid prefix of a counted set, or `None` when `count` claims
/// more entries than the storage holds (fail closed at every reader).
fn valid_set(set: &[u8], count: u8) -> Option<&[u8]> {
    set.get(..usize::from(count))
}

/// Membership test over a counted set.
fn set_contains(set: &[u8], count: u8, value: u8) -> bool {
    valid_set(set, count).is_some_and(|slice| slice.contains(&value))
}

/// True when `set` holds any entry absent from `baseline`.
fn set_widens(set: &[u8], count: u8, baseline: &[u8], baseline_count: u8) -> bool {
    match (valid_set(set, count), valid_set(baseline, baseline_count)) {
        (Some(current), Some(base)) => current.iter().any(|value| !base.contains(value)),
        // Malformed counts are treated as widening: fail closed.
        _ => true,
    }
}

/// Allowlist → capability (§5.2 v2) → device business rules, in the
/// normative check order. Denial is terminal `Denied`, never a repair turn.
///
/// This gate does the allowlist and capability steps only: it never
/// sees the device, so device business rules (pin direction, sensor
/// present, clock sane) are the runtime's `Authorize` step, composed
/// after this one. Both capability and device truth must hold; the
/// model cannot grant itself resources.
///
/// # Errors
///
/// Returns [`Error::PermissionDenied`] when the capability set refuses
/// the call.
pub fn authorize(caps: &Capabilities, entry: &ToolContract, args: &ToolArgs) -> Result<(), Error> {
    // Allowlist: the six E2 tools, all phases (§5.2). Each arm pairs a
    // stable tool id with its `ToolArgs` shape; an unknown id, or args
    // that do not belong to the tool, fails closed. (Unreachable when
    // the decoder built both halves.)
    let granted = match (entry.id, args) {
        (GPIO_PIN_READ_ID, ToolArgs::GpioPinRead { pin }) => caps.can_read(*pin),
        (GPIO_PIN_WRITE_ID, ToolArgs::GpioPinWrite { pin, .. }) => caps.can_write(*pin),
        (SENSOR_SAMPLE_READ_ID, ToolArgs::SensorSampleRead { sensor }) => caps.can_sample(*sensor),
        (TIMER_UPTIME_READ_ID, ToolArgs::TimerUptimeRead)
        | (TIMER_DELAY_WAIT_ID, ToolArgs::TimerDelayWait { .. }) => caps.allow_timer,
        (DEVICE_STATUS_REPORT_ID, ToolArgs::DeviceStatusReport { .. }) => caps.allow_status,
        _ => false,
    };
    // Permission classes with no grant path deny by construction, so
    // `AwaitApproval` stays unreachable (§5.2).
    let granted = granted
        && !matches!(
            entry.permission,
            PermissionClass::SensitiveWrite | PermissionClass::Irreversible
        );
    if granted {
        Ok(())
    } else {
        Err(Error::PermissionDenied {
            tool: ToolId::new(entry.id),
            resource: resource_of(*args),
        })
    }
}

/// The resource a denial names (§15.16): the pin for GPIO tools, the
/// sensor id for `sensor_sample_read`, 0 for timer and status tools.
const fn resource_of(args: ToolArgs) -> u8 {
    match args {
        ToolArgs::GpioPinRead { pin } | ToolArgs::GpioPinWrite { pin, .. } => pin.get(),
        ToolArgs::SensorSampleRead { sensor } => sensor,
        ToolArgs::TimerUptimeRead
        | ToolArgs::TimerDelayWait { .. }
        | ToolArgs::DeviceStatusReport { .. } => 0,
    }
}
