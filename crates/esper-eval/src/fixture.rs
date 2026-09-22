//! Golden fixture model and parser.
//!
//! A fixture is the JSON document from `spec/trajectories/*.json`
//! parsed into typed values the runner can drive. Every shape problem
//! becomes a [`FixtureError::Schema`] naming the JSON path, so a
//! malformed fixture fails with a clear error, never a panic.
//!
//! Two normalizations happen at parse time:
//!
//! - adjacent `VerificationRequest` + `VerificationResult` pairs merge
//!   into one [`ExpectedEvent::Verification`]: the runtime commits a
//!   single verification frame per read-back (SPEC §10.2's separate
//!   `VerificationRequest` record is not a journal frame in this
//!   slice), so the pair is the fixture's spelling of one event;
//! - `radio_bytes` must be `0` (SPEC §7.1: always 0 in the slice).
//!
//! // HOST-ONLY (E0/E1): heap-allocated fixture model, for the host
//! runner. Firmware code never sees fixtures.

use esper_core::state::TerminalStatus;
use esper_runtime::{CrashPoint, Direction, WORKFLOW_VERSION, journal::DecisionClass};
use thiserror::Error;

use crate::checks::{
    ForbiddenClause, Invariant, parse_forbidden, parse_invariant, terminal_status_by_name,
};
use crate::json::{self, Value};

/// What can go wrong loading or running a fixture.
#[derive(Debug, Error)]
pub enum FixtureError {
    /// The fixture file could not be read.
    #[error("cannot read fixture file `{path}`: {source}")]
    Io {
        /// The file that could not be read.
        path: String,
        /// The underlying I/O error.
        source: std::io::Error,
    },
    /// The file is not valid JSON.
    #[error("invalid fixture JSON: {detail}")]
    Json {
        /// The parser's message, with line and column.
        detail: String,
    },
    /// The JSON is valid but not a fixture: a missing field, a wrong
    /// type, or an unknown name, with the JSON path pinpointed.
    #[error("fixture schema error at `{path}`: {message}")]
    Schema {
        /// Dotted JSON path, e.g. `expected.trace[3].tool`.
        path: String,
        /// What was expected and what was found.
        message: String,
    },
    /// The runtime driver itself failed.
    #[error("the run driver failed: {0}")]
    Runtime(#[from] esper_runtime::RuntimeError),
    /// The committed trace did not match `expected.trace`.
    #[error("trace mismatch:\n{diff}")]
    TraceDiff {
        /// The readable event-by-event diff.
        diff: String,
    },
    /// A terminal-status, reason, summary, or budget assertion failed.
    #[error("assertion failed: {detail}")]
    AssertionFailed {
        /// What was asserted and what was observed.
        detail: String,
    },
    /// A SPEC §11.3 crash-oracle invariant failed.
    #[error("invariant `{invariant}` failed: {detail}")]
    InvariantFailed {
        /// The invariant's fixture spelling.
        invariant: &'static str,
        /// What was observed.
        detail: String,
    },
    /// A `forbidden` clause was violated.
    #[error("forbidden clause violated (`{clause}`): {detail}")]
    ForbiddenViolated {
        /// The clause's fixture spelling.
        clause: String,
        /// What was observed.
        detail: String,
    },
}

/// A parsed golden fixture: the whole red test in one value.
#[derive(Debug, Clone)]
pub struct Fixture {
    /// The fixture id, e.g. `success-read-write-verify-finish`.
    pub id: String,
    /// The human title.
    pub title: String,
    /// The run seed: budgets, capabilities, versions.
    pub seed: SeedSpec,
    /// The scripted model lines, one per inference turn, as bytes.
    pub script: Vec<Vec<u8>>,
    /// The fake device's initial pins and fault plan.
    pub device: DeviceSpec,
    /// Typed human inputs, in `after_decision` order.
    pub input_events: Vec<InputEvent>,
    /// Resets to inject, in plan order.
    pub crash_plan: Vec<CrashEntry>,
    /// Everything the run must produce.
    pub expected: Expected,
}

/// The run seed section of a fixture.
#[derive(Debug, Clone)]
pub struct SeedSpec {
    /// Must equal the build's [`WORKFLOW_VERSION`].
    pub workflow_version: u16,
    /// Model turns granted (repairs included).
    pub model_turns: u16,
    /// Mutating tool intents granted.
    pub mutations: u16,
    /// Context bytes granted (carried, not metered in E0/E1).
    pub input_tokens: u32,
    /// Model output bytes granted (carried, not metered in E0/E1).
    pub output_tokens: u32,
    /// Host monotonic milliseconds granted (carried, not metered).
    pub elapsed_ms: u64,
    /// Pins the model may read.
    pub read_pins: Vec<u8>,
    /// Pins the model may write.
    pub write_pins: Vec<u8>,
    /// Sensor channels the model may sample, `0..=3` (E2). Absent in
    /// the fixture means denied — fail closed (SPEC §17.7).
    pub sensors: Vec<u8>,
    /// Whether the timer tools are permitted (E2). Absent means
    /// denied — fail closed (SPEC §17.7).
    pub allow_timer: bool,
    /// Whether the status tool is permitted (E2). Absent means
    /// denied — fail closed (SPEC §17.7).
    pub allow_status: bool,
    /// The context budget in bytes that arms compaction/rollover
    /// (SPEC §20.2, `should_compact` trigger). Absent or null means
    /// the fixture does not exercise rollover: the runner maps it to
    /// the runtime seed's unset default.
    pub context_budget_bytes: Option<u64>,
}

/// The fake device section of a fixture.
#[derive(Debug, Clone)]
pub struct DeviceSpec {
    /// Explicitly configured pins; unlisted pins default to input/low
    /// (SPEC §14.1).
    pub pins: Vec<PinSpec>,
    /// Deterministic device behaviors.
    pub faults: Vec<DeviceFault>,
}

/// One explicitly configured pin.
#[derive(Debug, Clone, Copy)]
pub struct PinSpec {
    /// The pin number, `0..=7`.
    pub pin: u8,
    /// The direction the device reports.
    pub direction: Direction,
    /// The initial level (`true` is high).
    pub level_high: bool,
}

/// A deterministic device behavior from `device.faults`.
#[derive(Debug, Clone)]
pub enum DeviceFault {
    /// The tool fails transiently this many times for the resource,
    /// then behaves. Fires before the device is touched. The `tool`
    /// filter is optional: absent means the fault applies to any tool
    /// (SPEC §17.7).
    Transient {
        /// The canonical tool name, when the fault is tool-filtered.
        tool: Option<String>,
        /// The device resource the fault matches (SPEC §17.7).
        resource: FaultResource,
        /// How many dispatches fail before one succeeds.
        failures: u32,
    },
    /// The pin's actuator is stuck: writes are acknowledged but the
    /// level never moves, so the independent verifier sees the truth.
    /// A stuck actuator is a GPIO concept, so the resource is always a
    /// pin.
    Stuck {
        /// The canonical tool name, when the fault is tool-filtered.
        tool: Option<String>,
        /// The stuck pin.
        pin: u8,
        /// The level the pin is stuck at (`true` is high).
        level_high: bool,
    },
}

/// The device resource a fault matches (SPEC §17.7): GPIO tools match
/// on `pin`, `sensor_sample_read` on `sensor`, timer and status tools
/// on resource 0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultResource {
    /// A GPIO pin, `0..=7`.
    Pin(u8),
    /// A sensor channel, `0..=3`.
    Sensor(u8),
    /// Resource 0: the timer and status tools.
    Clock,
}

/// One typed human input delivery.
#[derive(Debug, Clone)]
pub struct InputEvent {
    /// The 0-based index of the `Ask` model decision this answers.
    pub after_decision: usize,
    /// The typed input payload, as JSON.
    pub input: Value,
}

/// One crash-plan entry: reset at this §11.1 point, this many times.
#[derive(Debug, Clone, Copy)]
pub struct CrashEntry {
    /// Where to reset.
    pub point: CrashPoint,
    /// How many times to arm the reset.
    pub times: u32,
}

/// Everything the run must produce.
#[derive(Debug, Clone)]
pub struct Expected {
    /// The terminal status the run must end with.
    pub terminal_status: TerminalStatus,
    /// The exact committed record sequence.
    pub trace: Vec<ExpectedEvent>,
    /// Model turns that must remain.
    pub model_turns_remaining: u16,
    /// Mutations that must remain.
    pub mutations_remaining: u16,
    /// The §11.3 oracle properties to check, by fixture relevance.
    pub invariants: Vec<Invariant>,
    /// Human-readable prohibitions, parsed into checkable clauses that
    /// keep their original spelling for error reports.
    pub forbidden: Vec<ForbiddenClause>,
}

/// One expected committed record.
#[derive(Debug, Clone)]
pub enum ExpectedEvent {
    /// A committed model line.
    ModelDecision(ExpectedDecision),
    /// A committed tool intent.
    ToolRequest {
        /// The canonical tool name.
        tool: String,
        /// The validated argument object.
        args: Value,
    },
    /// A committed tool outcome.
    ToolObservation {
        /// The canonical tool name.
        tool: String,
        /// Whether the dispatch succeeded (`false` is transient).
        ok: bool,
        /// The result payload; required when `ok`, absent when transient.
        payload: Option<Value>,
        /// The annotated progress delta, if any.
        progress: Option<String>,
        /// When set, this observation must share its effect id with the
        /// n-th (1-based) earlier observation of the same tool.
        same_effect_id_as_attempt: Option<usize>,
    },
    /// A committed independent read-back (merged from the fixture's
    /// `VerificationRequest` + `VerificationResult` pair).
    Verification {
        /// The canonical tool name.
        tool: String,
        /// The expected pin state.
        expected: Value,
        /// The observed pin state.
        observed: Value,
        /// Whether read-back matched.
        passed: bool,
        /// The annotated progress delta, if any.
        progress: Option<String>,
    },
    /// The engine asked for typed human input.
    ApprovalRequest {
        /// The prompt, as the model emitted it.
        prompt: String,
        /// The response schema id.
        schema: u8,
    },
    /// Typed human input arrived and committed.
    ApprovalDecision {
        /// The input payload, as JSON.
        input: Value,
        /// The annotated progress delta, if any.
        progress: Option<String>,
    },
    /// The run's one logical terminal result.
    TerminalResult {
        /// The terminal status.
        status: TerminalStatus,
        /// The machine-readable reason.
        reason: String,
        /// The run summary.
        summary: String,
    },
}

/// One expected model decision, by class.
#[derive(Debug, Clone)]
pub enum ExpectedDecision {
    /// A well-formed tool call.
    Call {
        /// The canonical tool name.
        tool: String,
        /// The argument object.
        args: Value,
    },
    /// A well-formed human-input request.
    Ask {
        /// The prompt.
        prompt: String,
        /// The response schema id.
        schema: u8,
    },
    /// A well-formed terminal answer.
    Finish {
        /// The finish status word, e.g. `completed`.
        status: String,
        /// The run summary.
        summary: String,
    },
    /// The line broke the output grammar.
    Malformed,
    /// The JSON was well-formed but violated the static arg schema.
    InvalidArgs {
        /// The tool named on the line, if the fixture records it.
        tool: Option<String>,
        /// The human annotation of the violation, if any.
        violation: Option<String>,
    },
}

impl ExpectedDecision {
    /// The committed decision class this expectation matches.
    #[must_use]
    pub const fn class(&self) -> DecisionClass {
        match self {
            Self::Call { .. } => DecisionClass::Call,
            Self::Ask { .. } => DecisionClass::Ask,
            Self::Finish { .. } => DecisionClass::Finish,
            Self::Malformed => DecisionClass::Malformed,
            Self::InvalidArgs { .. } => DecisionClass::InvalidArgs,
        }
    }
}

/// Parse a fixture document.
///
/// # Errors
///
/// Returns [`FixtureError::Json`] for invalid JSON and
/// [`FixtureError::Schema`] — with the JSON path — for any shape
/// problem: missing fields, wrong types, unknown crash points,
/// statuses, invariants, or forbidden clauses.
pub fn parse_fixture(text: &str) -> Result<Fixture, FixtureError> {
    let value = json::parse(text).map_err(|err| FixtureError::Json {
        // HOST-ONLY (E0/E1)
        detail: err.to_string(),
    })?;
    let root = Cursor::root(&value);
    let id = root.field("id")?.as_str()?.to_owned();
    let title = root.field("title")?.as_str()?.to_owned();
    let spec_version = root.field("spec_version")?.as_u64()?;
    if spec_version != 1 {
        return Err(root.field("spec_version")?.schema_err(
            // HOST-ONLY (E0/E1)
            format!("unsupported spec_version {spec_version}; this runner reads version 1"),
        ));
    }
    let seed = parse_seed(&root.field("run_seed")?)?;
    let script = parse_script(&root.field("script")?)?;
    let device = parse_device(&root.field("device")?)?;
    let input_events = parse_input_events(&root.field("input_events")?)?;
    let crash_plan = parse_crash_plan(&root.field("crash_plan")?)?;
    let expected = parse_expected(&root.field("expected")?)?;
    Ok(Fixture {
        // HOST-ONLY (E0/E1)
        id,
        title,
        seed,
        script,
        device,
        input_events,
        crash_plan,
        expected,
    })
}

// ---------------------------------------------------------------------------
// Path-tracked JSON cursor: every shape problem names its JSON path.
// ---------------------------------------------------------------------------

/// A JSON value with its dotted path, for precise schema errors.
struct Cursor<'a> {
    /// The value at this path.
    value: &'a Value,
    /// Dotted path, e.g. `$.expected.trace[3].tool`.
    path: String,
}

impl<'a> Cursor<'a> {
    /// The document root.
    fn root(value: &'a Value) -> Self {
        Self {
            value,
            // HOST-ONLY (E0/E1)
            path: "$".to_owned(),
        }
    }

    /// Build a schema error at this path.
    fn schema_err(&self, message: String) -> FixtureError {
        FixtureError::Schema {
            // HOST-ONLY (E0/E1)
            path: self.path.clone(),
            message,
        }
    }

    /// "Expected X, found Y" at this path.
    fn expected(&self, want: &str) -> FixtureError {
        self.schema_err(
            // HOST-ONLY (E0/E1)
            format!("expected {want}, found {}", self.value.kind_name()),
        )
    }

    /// Descend into a required object field.
    fn field(&self, key: &str) -> Result<Self, FixtureError> {
        self.value.get(key).map_or_else(
            || {
                Err(self.schema_err(
                    // HOST-ONLY (E0/E1)
                    format!("missing required field `{key}`"),
                ))
            },
            |value| {
                Ok(Self {
                    value,
                    // HOST-ONLY (E0/E1)
                    path: format!("{}.{key}", self.path),
                })
            },
        )
    }

    /// Descend into an optional object field.
    fn opt(&self, key: &str) -> Option<Self> {
        self.value.get(key).map(|value| Self {
            value,
            // HOST-ONLY (E0/E1)
            path: format!("{}.{key}", self.path),
        })
    }

    /// Iterate an array with per-index paths.
    fn items(&self) -> Result<Vec<Self>, FixtureError> {
        self.value.as_array().map_or_else(
            || Err(self.expected("an array")),
            |array| {
                Ok(array
                    .iter()
                    .enumerate()
                    .map(|(i, value)| Self {
                        value,
                        // HOST-ONLY (E0/E1)
                        path: format!("{}[{i}]", self.path),
                    })
                    // HOST-ONLY (E0/E1)
                    .collect())
            },
        )
    }

    /// Read a string.
    fn as_str(&self) -> Result<&'a str, FixtureError> {
        self.value.as_str().ok_or_else(|| self.expected("a string"))
    }

    /// Read an integer field as `u8`.
    fn as_u8(&self) -> Result<u8, FixtureError> {
        match self.value {
            Value::Int(n) => u8::try_from(*n).map_err(|_| self.expected("an integer 0..=255")),
            _ => Err(self.expected("an integer 0..=255")),
        }
    }

    /// Read an integer field as `u16`.
    fn as_u16(&self) -> Result<u16, FixtureError> {
        match self.value {
            Value::Int(n) => u16::try_from(*n).map_err(|_| self.expected("an integer 0..=65535")),
            _ => Err(self.expected("an integer 0..=65535")),
        }
    }

    /// Read an integer field as `u32`.
    fn as_u32(&self) -> Result<u32, FixtureError> {
        match self.value {
            Value::Int(n) => u32::try_from(*n).map_err(|_| self.expected("an integer 0..=2^32-1")),
            _ => Err(self.expected("an integer 0..=2^32-1")),
        }
    }

    /// Read an integer field as `u64`.
    fn as_u64(&self) -> Result<u64, FixtureError> {
        match self.value {
            Value::Int(n) => u64::try_from(*n).map_err(|_| self.expected("a non-negative integer")),
            _ => Err(self.expected("a non-negative integer")),
        }
    }

    /// Read an integer field as `usize`.
    fn as_usize(&self) -> Result<usize, FixtureError> {
        match self.value {
            Value::Int(n) => {
                usize::try_from(*n).map_err(|_| self.expected("a non-negative integer"))
            }
            _ => Err(self.expected("a non-negative integer")),
        }
    }

    /// Read a boolean field.
    fn as_bool(&self) -> Result<bool, FixtureError> {
        match self.value {
            Value::Bool(flag) => Ok(*flag),
            _ => Err(self.expected("a boolean")),
        }
    }

    /// Read the raw value (for args/payload/input passthrough).
    const fn raw(&self) -> &'a Value {
        self.value
    }
}

// ---------------------------------------------------------------------------
// Section parsers.
// ---------------------------------------------------------------------------

/// Parse `run_seed`.
fn parse_seed(cursor: &Cursor) -> Result<SeedSpec, FixtureError> {
    let workflow_version = cursor.field("workflow_version")?.as_u16()?;
    if workflow_version != WORKFLOW_VERSION {
        return Err(cursor.field("workflow_version")?.schema_err(
            // HOST-ONLY (E0/E1)
            format!(
                "unsupported workflow_version {workflow_version}; this runner builds version {WORKFLOW_VERSION}"
            ),
        ));
    }
    let budget = cursor.field("budget")?;
    let radio_bytes = budget.field("radio_bytes")?.as_u32()?;
    if radio_bytes != 0 {
        return Err(budget.field("radio_bytes")?.schema_err(
            // HOST-ONLY (E0/E1)
            format!("radio_bytes must be 0 in the slice (SPEC §7.1), found {radio_bytes}"),
        ));
    }
    let capabilities = cursor.field("capabilities")?;
    let sensors = match capabilities.opt("sensors") {
        // Absent means denied: fail closed (SPEC §17.7).
        None => Vec::new(),
        Some(list) => parse_sensor_list(&list)?,
    };
    let allow_timer = match capabilities.opt("allow_timer") {
        // HOST-ONLY (E0/E1)
        None => false,
        Some(flag) => flag.as_bool()?,
    };
    let allow_status = match capabilities.opt("allow_status") {
        // HOST-ONLY (E0/E1)
        None => false,
        Some(flag) => flag.as_bool()?,
    };
    // Optional: absent or explicit null means the fixture does not
    // exercise rollover (SPEC §20). A present non-integer fails
    // closed like every other schema violation.
    let context_budget_bytes = match cursor.opt("context_budget_bytes") {
        // HOST-ONLY (E0/E1)
        None => None,
        Some(value) if matches!(value.value, Value::Null) => None,
        Some(value) => Some(value.as_u64()?),
    };
    Ok(SeedSpec {
        workflow_version,
        model_turns: budget.field("model_turns")?.as_u16()?,
        mutations: budget.field("mutations")?.as_u16()?,
        input_tokens: budget.field("input_tokens")?.as_u32()?,
        output_tokens: budget.field("output_tokens")?.as_u32()?,
        elapsed_ms: budget.field("elapsed_ms")?.as_u64()?,
        read_pins: parse_pin_list(&capabilities.field("read_pins")?)?,
        write_pins: parse_pin_list(&capabilities.field("write_pins")?)?,
        sensors,
        allow_timer,
        allow_status,
        context_budget_bytes,
    })
}

/// Parse a pin list like `[4]`; every pin must be `0..=7`.
fn parse_pin_list(cursor: &Cursor) -> Result<Vec<u8>, FixtureError> {
    // HOST-ONLY (E0/E1)
    let mut pins = Vec::new();
    for item in cursor.items()? {
        let pin = item.as_u8()?;
        if pin > 7 {
            return Err(item.schema_err(
                // HOST-ONLY (E0/E1)
                format!("pin {pin} is out of range 0..=7"),
            ));
        }
        pins.push(pin);
    }
    Ok(pins)
}

/// Parse a sensor list like `[1]`; every sensor must be `0..=3`.
fn parse_sensor_list(cursor: &Cursor) -> Result<Vec<u8>, FixtureError> {
    // HOST-ONLY (E0/E1)
    let mut sensors = Vec::new();
    for item in cursor.items()? {
        let sensor = item.as_u8()?;
        if sensor > 3 {
            return Err(item.schema_err(
                // HOST-ONLY (E0/E1)
                format!("sensor {sensor} is out of range 0..=3"),
            ));
        }
        sensors.push(sensor);
    }
    Ok(sensors)
}
/// Parse the scripted model lines.
fn parse_script(cursor: &Cursor) -> Result<Vec<Vec<u8>>, FixtureError> {
    // HOST-ONLY (E0/E1)
    let mut lines: Vec<Vec<u8>> = Vec::new();
    for item in cursor.items()? {
        // HOST-ONLY (E0/E1)
        lines.push(item.as_str()?.as_bytes().to_vec());
    }
    if lines.is_empty() {
        return Err(cursor.schema_err("the model script must hold at least one line".to_owned()));
    }
    Ok(lines)
}

/// Parse `device`: explicit pins plus the fault plan.
fn parse_device(cursor: &Cursor) -> Result<DeviceSpec, FixtureError> {
    let pins_cursor = cursor.field("pins")?;
    let pairs = pins_cursor
        .raw()
        .as_object()
        .ok_or_else(|| pins_cursor.expected("an object"))?;
    // HOST-ONLY (E0/E1)
    let mut pins: Vec<PinSpec> = Vec::new();
    for (name, value) in pairs {
        let pin: u8 = name.parse().map_err(|_| {
            pins_cursor.schema_err(
                // HOST-ONLY (E0/E1)
                format!("pin key `{name}` is not a number 0..=7"),
            )
        })?;
        if pin > 7 {
            return Err(pins_cursor.schema_err(
                // HOST-ONLY (E0/E1)
                format!("pin key `{name}` is out of range 0..=7"),
            ));
        }
        let entry_cursor = Cursor {
            value,
            // HOST-ONLY (E0/E1)
            path: format!("{}.{name}", pins_cursor.path),
        };
        let direction = match entry_cursor.field("direction")?.as_str()? {
            "in" => Direction::Input,
            "out" => Direction::Output,
            other => {
                return Err(entry_cursor.field("direction")?.schema_err(
                    // HOST-ONLY (E0/E1)
                    format!("unknown direction `{other}`; expected `in` or `out`"),
                ));
            }
        };
        let level_high = match entry_cursor.field("level")?.as_str()? {
            "low" => false,
            "high" => true,
            other => {
                return Err(entry_cursor.field("level")?.schema_err(
                    // HOST-ONLY (E0/E1)
                    format!("unknown level `{other}`; expected `low` or `high`"),
                ));
            }
        };
        pins.push(PinSpec {
            pin,
            direction,
            level_high,
        });
    }
    // HOST-ONLY (E0/E1)
    let mut faults: Vec<DeviceFault> = Vec::new();
    for item in cursor.field("faults")?.items()? {
        faults.push(parse_device_fault(&item)?);
    }
    Ok(DeviceSpec { pins, faults })
}

/// Parse one `device.faults` entry.
///
/// The `tool` filter is optional (absent means the fault applies to
/// any tool, SPEC §17.7). The resource comes from `match_args`:
/// `pin` for GPIO tools, `sensor` for `sensor_sample_read`, neither
/// for timer/status tools (resource 0). A filter that disagrees with
/// the resource fails closed.
fn parse_device_fault(cursor: &Cursor) -> Result<DeviceFault, FixtureError> {
    let tool = cursor
        .opt("tool")
        .map(|name| parse_tool_name(&name))
        .transpose()?;
    let resource = parse_fault_resource(cursor, tool.as_deref())?;
    let transient = cursor
        .opt("class")
        .is_some_and(|class| class.as_str().is_ok_and(|text| text == "transient"));
    let stuck = cursor.opt("stuck_level");
    match (transient, stuck, resource) {
        (true, None, resource) => Ok(DeviceFault::Transient {
            tool,
            resource,
            failures: cursor.field("failures_before_success")?.as_u32()?,
        }),
        (false, Some(level), FaultResource::Pin(pin)) => {
            let level_high = match level.as_str()? {
                "low" => false,
                "high" => true,
                other => {
                    return Err(level.schema_err(
                        // HOST-ONLY (E0/E1)
                        format!("unknown stuck_level `{other}`; expected `low` or `high`"),
                    ));
                }
            };
            Ok(DeviceFault::Stuck {
                tool,
                pin,
                level_high,
            })
        }
        (false, Some(_), _) => Err(cursor.schema_err(
            "stuck_level is a GPIO actuator fault: `match_args` must name a `pin`".to_owned(),
        )),
        _ => Err(cursor.schema_err(
            "a fault needs exactly one of `class: \"transient\"` (+ `failures_before_success`) or `stuck_level`".to_owned(),
        )),
    }
}

/// Parse the fault's resource from `match_args` (SPEC §17.7) and check
/// it agrees with the optional tool filter.
fn parse_fault_resource(
    cursor: &Cursor,
    tool: Option<&str>,
) -> Result<FaultResource, FixtureError> {
    let match_args = cursor.field("match_args")?;
    let pin = match_args.opt("pin");
    let sensor = match_args.opt("sensor");
    let resource = match (pin, sensor) {
        (Some(entry), None) => {
            let pin = entry.as_u8()?;
            if pin > 7 {
                return Err(entry.schema_err(
                    // HOST-ONLY (E0/E1)
                    format!("pin {pin} is out of range 0..=7"),
                ));
            }
            FaultResource::Pin(pin)
        }
        (None, Some(entry)) => {
            let sensor = entry.as_u8()?;
            if sensor > 3 {
                return Err(entry.schema_err(
                    // HOST-ONLY (E0/E1)
                    format!("sensor {sensor} is out of range 0..=3"),
                ));
            }
            FaultResource::Sensor(sensor)
        }
        (None, None) => FaultResource::Clock,
        (Some(_), Some(_)) => {
            return Err(match_args.schema_err(
                "match_args holds both `pin` and `sensor`; name exactly one resource".to_owned(),
            ));
        }
    };
    if let Some(name) = tool {
        let agrees = matches!(
            (name, resource),
            ("gpio_pin_read" | "gpio_pin_write", FaultResource::Pin(_))
                | ("sensor_sample_read", FaultResource::Sensor(_))
                | (
                    "timer_uptime_read" | "timer_delay_wait" | "device_status_report",
                    FaultResource::Clock,
                )
        );
        if !agrees {
            return Err(cursor.schema_err(
                // HOST-ONLY (E0/E1)
                format!(
                    "fault tool `{name}` disagrees with the match_args resource \
                     (gpio tools match `pin`, sensor_sample_read matches `sensor`, \
                     timer/status tools match resource 0)"
                ),
            ));
        }
    }
    Ok(resource)
}

/// Resolve a canonical tool name against the E2 contract catalog (SPEC
/// §17.1: the one table every tool name comes from), rejecting
/// anything outside it. Returns the canonical name.
fn parse_tool_name(cursor: &Cursor) -> Result<String, FixtureError> {
    let name = cursor.as_str()?;
    esper_protocol::contract::lookup_by_name(name)
        .map(|entry| entry.name.to_owned())
        .ok_or_else(|| {
            cursor.schema_err(
                // HOST-ONLY (E0/E1)
                format!(
                    "unknown tool `{name}`; expected one of the six E2 catalog names (SPEC §17.2)"
                ),
            )
        })
}

/// Parse `input_events`.
fn parse_input_events(cursor: &Cursor) -> Result<Vec<InputEvent>, FixtureError> {
    // HOST-ONLY (E0/E1)
    let mut events: Vec<InputEvent> = Vec::new();
    for item in cursor.items()? {
        let after_decision = item.field("after_decision")?.as_usize()?;
        let input = item.field("input")?.raw().clone();
        if input.as_object().is_none() {
            return Err(item.field("input")?.expected("an object"));
        }
        events.push(InputEvent {
            after_decision,
            input,
        });
    }
    events.sort_by_key(|event| event.after_decision);
    Ok(events)
}

/// Parse `crash_plan`.
fn parse_crash_plan(cursor: &Cursor) -> Result<Vec<CrashEntry>, FixtureError> {
    // HOST-ONLY (E0/E1)
    let mut plan: Vec<CrashEntry> = Vec::new();
    for item in cursor.items()? {
        let at = item.field("at")?;
        let name = at.as_str()?;
        let point = crash_point_by_name(name).ok_or_else(|| {
            at.schema_err(
                // HOST-ONLY (E0/E1)
                format!(
                    "unknown crash point `{name}`; expected one of the cp_* names from SPEC §11.1"
                ),
            )
        })?;
        let times = item.field("times")?.as_u32()?;
        if times == 0 {
            return Err(item
                .field("times")?
                .schema_err("crash plan `times` must be at least 1".to_owned()));
        }
        plan.push(CrashEntry { point, times });
    }
    Ok(plan)
}

/// Map a fixture `cp_*` spelling to its crash point.
fn crash_point_by_name(name: &str) -> Option<CrashPoint> {
    Some(match name {
        "cp_model_intent" => CrashPoint::ModelIntent,
        "cp_before_decision_commit" => CrashPoint::BeforeDecisionCommit,
        "cp_before_authorize" => CrashPoint::BeforeAuthorize,
        "cp_tool_intent" => CrashPoint::ToolIntent,
        "cp_after_physical_before_observation" => CrashPoint::AfterPhysicalBeforeObservation,
        "cp_after_observation_before_verify" => CrashPoint::AfterObservationBeforeVerify,
        "cp_before_verification_commit" => CrashPoint::BeforeVerificationCommit,
        "cp_after_verification" => CrashPoint::AfterVerification,
        "cp_await_input" => CrashPoint::AwaitInput,
        "cp_before_terminal_commit" => CrashPoint::BeforeTerminalCommit,
        "cp_after_terminal_commit" => CrashPoint::AfterTerminalCommit,
        _ => return None,
    })
}

/// Parse `expected`.
fn parse_expected(cursor: &Cursor) -> Result<Expected, FixtureError> {
    let trace = parse_trace(&cursor.field("trace")?)?;
    let budget = cursor.field("budget_remaining")?;
    // HOST-ONLY (E0/E1)
    let mut invariants: Vec<Invariant> = Vec::new();
    for item in cursor.field("invariants")?.items()? {
        let name = item.as_str()?;
        invariants.push(parse_invariant(name).ok_or_else(|| {
            item.schema_err(
                // HOST-ONLY (E0/E1)
                format!("unknown invariant `{name}`; expected one of no_dispatch_without_intent, budgets_never_widen, effect_identity_stable, single_logical_terminal, mutation_never_complete_without_verifier"),
            )
        })?);
    }
    // HOST-ONLY (E0/E1)
    let mut forbidden: Vec<ForbiddenClause> = Vec::new();
    for item in cursor.field("forbidden")?.items()? {
        let text = item.as_str()?;
        let check = parse_forbidden(text).map_err(|message| item.schema_err(message))?;
        forbidden.push(ForbiddenClause {
            // HOST-ONLY (E0/E1)
            text: text.to_owned(),
            check,
        });
    }
    Ok(Expected {
        terminal_status: parse_terminal_status(&cursor.field("terminal_status")?)?,
        trace,
        model_turns_remaining: budget.field("model_turns")?.as_u16()?,
        mutations_remaining: budget.field("mutations")?.as_u16()?,
        invariants,
        forbidden,
    })
}

/// Parse a terminal status in the fixture's `PascalCase` spelling.
fn parse_terminal_status(cursor: &Cursor) -> Result<TerminalStatus, FixtureError> {
    let name = cursor.as_str()?;
    terminal_status_by_name(name).ok_or_else(|| {
        cursor.schema_err(
            // HOST-ONLY (E0/E1)
            format!("unknown terminal status `{name}`; expected one of Completed, NeedsInput, Denied, BudgetExhausted, Stuck, ToolUnavailable, ModelInvalid, StorageFault, Incompatible"),
        )
    })
}

/// Parse `expected.trace`, merging each `VerificationRequest` +
/// `VerificationResult` pair into one verification expectation.
fn parse_trace(cursor: &Cursor) -> Result<Vec<ExpectedEvent>, FixtureError> {
    let items = cursor.items()?;
    // HOST-ONLY (E0/E1)
    let mut events: Vec<ExpectedEvent> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        let item = &items[i];
        let kind = item.field("kind")?.as_str()?;
        if kind == "VerificationRequest" {
            let request_tool = parse_tool_name(&item.field("tool")?)?;
            let request_expected = item.field("expected")?.raw().clone();
            let next = items.get(i + 1).ok_or_else(|| {
                item.schema_err(
                    "VerificationRequest without a following VerificationResult".to_owned(),
                )
            })?;
            if next.field("kind")?.as_str()? != "VerificationResult" {
                return Err(next.schema_err(
                    "expected VerificationResult after VerificationRequest".to_owned(),
                ));
            }
            events.push(parse_verification_result(
                next,
                &request_tool,
                &request_expected,
            )?);
            i += 2;
        } else if kind == "VerificationResult" {
            return Err(item.schema_err(
                "VerificationResult without a preceding VerificationRequest".to_owned(),
            ));
        } else {
            events.push(parse_expected_event(item)?);
            i += 1;
        }
    }
    Ok(events)
}

/// Build the merged verification expectation, checking the pair agrees.
///
/// The tool name rides on the `VerificationRequest`: the result does
/// not repeat it.
fn parse_verification_result(
    cursor: &Cursor,
    request_tool: &str,
    request_expected: &Value,
) -> Result<ExpectedEvent, FixtureError> {
    let expected = cursor.field("expected")?.raw();
    if !expected.equals_canonical(request_expected) {
        return Err(cursor.field("expected")?.schema_err(
            "VerificationResult `expected` does not match its VerificationRequest `expected`"
                .to_owned(),
        ));
    }
    let passed = match cursor.field("result")?.as_str()? {
        "pass" => true,
        "fail" => false,
        other => {
            return Err(cursor.field("result")?.schema_err(
                // HOST-ONLY (E0/E1)
                format!("unknown verification result `{other}`; expected `pass` or `fail`"),
            ));
        }
    };
    Ok(ExpectedEvent::Verification {
        // HOST-ONLY (E0/E1)
        tool: request_tool.to_owned(),
        expected: expected.clone(),
        observed: cursor.field("observed")?.raw().clone(),
        passed,
        progress: parse_opt_string(cursor, "progress")?,
    })
}

/// Parse one expected event by its `kind`.
fn parse_expected_event(cursor: &Cursor) -> Result<ExpectedEvent, FixtureError> {
    let kind = cursor.field("kind")?.as_str()?;
    match kind {
        "ModelDecision" => Ok(ExpectedEvent::ModelDecision(parse_decision(cursor)?)),
        "ToolRequest" => Ok(ExpectedEvent::ToolRequest {
            // HOST-ONLY (E0/E1)
            tool: parse_tool_name(&cursor.field("tool")?)?,
            args: cursor.field("args")?.raw().clone(),
        }),
        "ToolObservation" => parse_observation(cursor),
        "ApprovalRequest" => Ok(ExpectedEvent::ApprovalRequest {
            // HOST-ONLY (E0/E1)
            prompt: cursor.field("prompt")?.as_str()?.to_owned(),
            schema: cursor.field("schema")?.as_u8()?,
        }),
        "ApprovalDecision" => Ok(ExpectedEvent::ApprovalDecision {
            input: cursor.field("input")?.raw().clone(),
            progress: parse_opt_string(cursor, "progress")?,
        }),
        "TerminalResult" => Ok(ExpectedEvent::TerminalResult {
            status: parse_terminal_status(&cursor.field("status")?)?,
            // HOST-ONLY (E0/E1)
            reason: cursor.field("reason")?.as_str()?.to_owned(),
            summary: cursor.field("summary")?.as_str()?.to_owned(),
        }),
        _ => Err(cursor.field("kind")?.schema_err(
            // HOST-ONLY (E0/E1)
            format!("unknown trace kind `{kind}`; expected ModelDecision, ToolRequest, ToolObservation, VerificationRequest, ApprovalRequest, ApprovalDecision, or TerminalResult"),
        )),
    }
}

/// Parse an optional string field.
fn parse_opt_string(cursor: &Cursor, key: &str) -> Result<Option<String>, FixtureError> {
    cursor
        .opt(key)
        .map(|field| {
            // HOST-ONLY (E0/E1)
            field.as_str().map(str::to_owned)
        })
        .transpose()
}

/// Parse one expected model decision by its `class`.
fn parse_decision(cursor: &Cursor) -> Result<ExpectedDecision, FixtureError> {
    let class = cursor.field("class")?.as_str()?;
    match class {
        "Call" => Ok(ExpectedDecision::Call {
            // HOST-ONLY (E0/E1)
            tool: parse_tool_name(&cursor.field("tool")?)?,
            args: cursor.field("args")?.raw().clone(),
        }),
        "Ask" => Ok(ExpectedDecision::Ask {
            // HOST-ONLY (E0/E1)
            prompt: cursor.field("prompt")?.as_str()?.to_owned(),
            schema: cursor.field("schema")?.as_u8()?,
        }),
        "Finish" => {
            let finish = cursor.field("finish")?;
            Ok(ExpectedDecision::Finish {
                // HOST-ONLY (E0/E1)
                status: finish.field("status")?.as_str()?.to_owned(),
                summary: finish.field("summary")?.as_str()?.to_owned(),
            })
        }
        "Malformed" => Ok(ExpectedDecision::Malformed),
        "InvalidArgs" => Ok(ExpectedDecision::InvalidArgs {
            tool: parse_opt_string(cursor, "tool")?,
            violation: parse_opt_string(cursor, "violation")?,
        }),
        _ => Err(cursor.field("class")?.schema_err(
            // HOST-ONLY (E0/E1)
            format!("unknown decision class `{class}`; expected Call, Ask, Finish, Malformed, or InvalidArgs"),
        )),
    }
}

/// Parse one expected tool observation.
fn parse_observation(cursor: &Cursor) -> Result<ExpectedEvent, FixtureError> {
    let ok = match cursor.field("class")?.as_str()? {
        "ok" => true,
        "Transient" => false,
        other => {
            return Err(cursor.field("class")?.schema_err(
                // HOST-ONLY (E0/E1)
                format!("unknown observation class `{other}`; expected `ok` or `Transient`"),
            ));
        }
    };
    let payload = cursor.opt("payload").map(|field| field.raw().clone());
    if ok && payload.is_none() {
        return Err(cursor.schema_err("an `ok` observation needs a `payload`".to_owned()));
    }
    Ok(ExpectedEvent::ToolObservation {
        // HOST-ONLY (E0/E1)
        tool: parse_tool_name(&cursor.field("tool")?)?,
        ok,
        payload,
        progress: parse_opt_string(cursor, "progress")?,
        same_effect_id_as_attempt: cursor
            .opt("same_effect_id_as_attempt")
            .map(|field| field.as_usize())
            .transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::{FixtureError, parse_fixture};

    /// A minimal well-formed fixture; broken variants are derived by
    /// string surgery on the `[TRACE]`, invariant, and forbidden slots.
    const MINIMAL: &str = r#"{
        "id": "minimal", "title": "t", "spec_version": 1,
        "run_seed": {
            "workflow_version": 1,
            "budget": {"model_turns": 2, "mutations": 1, "input_tokens": 10,
                       "output_tokens": 10, "elapsed_ms": 1000, "radio_bytes": 0},
            "capabilities": {"read_pins": [], "write_pins": []}
        },
        "script": ["FINISH {\"status\": \"completed\", \"summary\": \"s\"}"],
        "device": {"pins": {}, "faults": []},
        "input_events": [], "crash_plan": [],
        "expected": {
            "terminal_status": "Completed",
            "trace": TRACE,
            "budget_remaining": {"model_turns": 1, "mutations": 1},
            "invariants": ["single_logical_terminal"],
            "forbidden": []
        }
    }"#;

    const FINISH_TRACE: &str = r#"[{"kind": "ModelDecision", "class": "Finish",
        "finish": {"status": "completed", "summary": "s"}},
        {"kind": "TerminalResult", "status": "Completed",
         "reason": "model_finished", "summary": "s"}]"#;

    fn fixture_with(trace: &str) -> String {
        // HOST-ONLY (E0/E1)
        MINIMAL.replace("TRACE", trace)
    }

    #[test]
    fn parses_the_minimal_fixture() {
        let fixture = parse_fixture(&fixture_with(FINISH_TRACE)).expect("minimal fixture");
        assert_eq!(fixture.id, "minimal");
        assert_eq!(fixture.expected.trace.len(), 2);
        assert_eq!(fixture.expected.invariants.len(), 1);
    }

    #[test]
    fn context_budget_bytes_defaults_to_none() {
        let fixture = parse_fixture(&fixture_with(FINISH_TRACE)).expect("minimal fixture");
        assert_eq!(fixture.seed.context_budget_bytes, None);
    }

    #[test]
    fn context_budget_bytes_parses_when_present() {
        let base = fixture_with(FINISH_TRACE);
        let with_budget = base.replace(
            "\"workflow_version\": 1,",
            "\"workflow_version\": 1, \"context_budget_bytes\": 2048,",
        );
        let fixture = parse_fixture(&with_budget).expect("fixture with context budget");
        assert_eq!(fixture.seed.context_budget_bytes, Some(2048));
    }

    #[test]
    fn context_budget_bytes_null_means_none() {
        let base = fixture_with(FINISH_TRACE);
        let with_null = base.replace(
            "\"workflow_version\": 1,",
            "\"workflow_version\": 1, \"context_budget_bytes\": null,",
        );
        let fixture = parse_fixture(&with_null).expect("fixture with null context budget");
        assert_eq!(fixture.seed.context_budget_bytes, None);
    }

    #[test]
    fn context_budget_bytes_rejects_a_string() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace(
            "\"workflow_version\": 1,",
            "\"workflow_version\": 1, \"context_budget_bytes\": \"lots\",",
        );
        let err = parse_fixture(&broken).expect_err("string context budget");
        assert!(
            err.to_string().contains("$.run_seed.context_budget_bytes"),
            "got {err:?}"
        );
    }

    #[test]
    fn truncated_json_is_a_clear_error_not_a_panic() {
        let cut = MINIMAL.len() - 10;
        let err = parse_fixture(&MINIMAL[..cut]).expect_err("truncated JSON");
        assert!(matches!(err, FixtureError::Json { .. }), "got {err:?}");
        assert!(err.to_string().contains("invalid fixture JSON"));
    }

    #[test]
    fn missing_field_names_its_json_path() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace("\"spec_version\": 1,", "");
        let err = parse_fixture(&broken).expect_err("missing spec_version");
        assert_eq!(
            err.to_string(),
            "fixture schema error at `$`: missing required field `spec_version`"
        );
    }

    #[test]
    fn wrong_spec_version_names_its_json_path() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace("\"spec_version\": 1", "\"spec_version\": 2");
        let err = parse_fixture(&broken).expect_err("bad spec_version");
        assert!(err.to_string().contains("$.spec_version"), "got {err}");
        assert!(err.to_string().contains("version 1"), "got {err}");
    }

    #[test]
    fn unknown_invariant_names_its_json_path() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace("single_logical_terminal", "no_such_invariant");
        let err = parse_fixture(&broken).expect_err("bad invariant");
        assert_eq!(
            err.to_string(),
            "fixture schema error at `$.expected.invariants[0]`: \
             unknown invariant `no_such_invariant`; expected one of \
             no_dispatch_without_intent, budgets_never_widen, effect_identity_stable, \
             single_logical_terminal, mutation_never_complete_without_verifier"
        );
    }

    #[test]
    fn unknown_forbidden_clause_names_its_json_path() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace(
            "\"forbidden\": []",
            "\"forbidden\": [\"the moon is made of cheese\"]",
        );
        let err = parse_fixture(&broken).expect_err("bad clause");
        assert!(
            err.to_string().contains("$.expected.forbidden[0]"),
            "got {err}"
        );
        assert!(
            err.to_string().contains("unknown forbidden clause"),
            "got {err}"
        );
    }

    #[test]
    fn unknown_trace_kind_names_its_json_path() {
        let broken = fixture_with(r#"[{"kind": "Nope"}]"#);
        let err = parse_fixture(&broken).expect_err("bad kind");
        assert!(
            err.to_string().contains("$.expected.trace[0].kind"),
            "got {err}"
        );
        assert!(
            err.to_string().contains("unknown trace kind `Nope`"),
            "got {err}"
        );
    }

    #[test]
    fn unknown_decision_class_names_its_json_path() {
        let broken = fixture_with(r#"[{"kind": "ModelDecision", "class": "Frobnicate"}]"#);
        let err = parse_fixture(&broken).expect_err("bad class");
        assert!(
            err.to_string().contains("$.expected.trace[0].class"),
            "got {err}"
        );
        assert!(
            err.to_string()
                .contains("unknown decision class `Frobnicate`"),
            "got {err}"
        );
    }

    #[test]
    fn lone_verification_request_is_rejected() {
        let broken = fixture_with(
            r#"[{"kind": "VerificationRequest", "tool": "gpio_pin_write",
                "expected": {"pin": 4, "level": "high"}}]"#,
        );
        let err = parse_fixture(&broken).expect_err("lone request");
        assert!(err.to_string().contains("$.expected.trace[0]"), "got {err}");
        assert!(
            err.to_string()
                .contains("without a following VerificationResult"),
            "got {err}"
        );
    }

    #[test]
    fn unknown_tool_in_trace_is_rejected() {
        let broken = fixture_with(
            r#"[{"kind": "ToolRequest", "tool": "laser_cannon", "args": {"pin": 4}}]"#,
        );
        let err = parse_fixture(&broken).expect_err("bad tool");
        assert!(
            err.to_string().contains("unknown tool `laser_cannon`"),
            "got {err}"
        );
    }

    #[test]
    fn unknown_crash_point_is_rejected() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace(
            "\"crash_plan\": []",
            "\"crash_plan\": [{\"at\": \"cp_nowhere\", \"times\": 1}]",
        );
        let err = parse_fixture(&broken).expect_err("bad crash point");
        assert!(err.to_string().contains("$.crash_plan[0].at"), "got {err}");
        assert!(
            err.to_string().contains("unknown crash point `cp_nowhere`"),
            "got {err}"
        );
    }

    #[test]
    fn absent_e2_capabilities_parse_as_denied() {
        // SPEC §17.7: E0/E1 fixtures without the E2 capability fields
        // keep their meaning — the fields parse as denied, fail closed.
        let fixture = parse_fixture(&fixture_with(FINISH_TRACE)).expect("minimal fixture");
        assert_eq!(fixture.seed.sensors, Vec::<u8>::new());
        assert!(!fixture.seed.allow_timer);
        assert!(!fixture.seed.allow_status);
    }

    #[test]
    fn present_e2_capabilities_parse() {
        let base = fixture_with(FINISH_TRACE);
        let with_caps = base.replace(
            "\"capabilities\": {\"read_pins\": [], \"write_pins\": []}",
            "\"capabilities\": {\"read_pins\": [4], \"write_pins\": [4], \
             \"sensors\": [0, 2], \"allow_timer\": true, \"allow_status\": true}",
        );
        let fixture = parse_fixture(&with_caps).expect("E2 capabilities");
        assert_eq!(fixture.seed.read_pins, vec![4]);
        assert_eq!(fixture.seed.write_pins, vec![4]);
        assert_eq!(fixture.seed.sensors, vec![0, 2]);
        assert!(fixture.seed.allow_timer);
        assert!(fixture.seed.allow_status);
    }

    #[test]
    fn sensor_out_of_range_is_rejected() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace(
            "\"capabilities\": {\"read_pins\": [], \"write_pins\": []}",
            "\"capabilities\": {\"read_pins\": [], \"write_pins\": [], \"sensors\": [4]}",
        );
        let err = parse_fixture(&broken).expect_err("sensor 4");
        assert!(
            err.to_string()
                .contains("$.run_seed.capabilities.sensors[0]"),
            "got {err}"
        );
        assert!(err.to_string().contains("out of range 0..=3"), "got {err}");
    }

    #[test]
    fn fault_tool_filter_and_pin_resource_parse() {
        use super::{DeviceFault, FaultResource};
        let base = fixture_with(FINISH_TRACE);
        let with_fault = base.replace(
            "\"faults\": []",
            "\"faults\": [{\"class\": \"transient\", \"tool\": \"gpio_pin_read\", \
             \"match_args\": {\"pin\": 4}, \"failures_before_success\": 2}]",
        );
        let fixture = parse_fixture(&with_fault).expect("tool-filtered fault");
        assert_eq!(fixture.device.faults.len(), 1);
        match &fixture.device.faults[0] {
            DeviceFault::Transient {
                tool,
                resource,
                failures,
            } => {
                assert_eq!(tool.as_deref(), Some("gpio_pin_read"));
                assert_eq!(*resource, FaultResource::Pin(4));
                assert_eq!(*failures, 2);
            }
            DeviceFault::Stuck { .. } => panic!("expected a transient fault, got Stuck"),
        }
    }

    #[test]
    fn fault_sensor_and_clock_resources_parse() {
        use super::{DeviceFault, FaultResource};
        let base = fixture_with(FINISH_TRACE);
        let with_faults = base.replace(
            "\"faults\": []",
            "\"faults\": [{\"class\": \"transient\", \"tool\": \"sensor_sample_read\", \
             \"match_args\": {\"sensor\": 2}, \"failures_before_success\": 1}, \
             {\"class\": \"transient\", \"tool\": \"timer_delay_wait\", \
             \"match_args\": {}, \"failures_before_success\": 1}]",
        );
        let fixture = parse_fixture(&with_faults).expect("sensor and clock faults");
        assert_eq!(fixture.device.faults.len(), 2);
        match &fixture.device.faults[0] {
            DeviceFault::Transient { resource, .. } => {
                assert_eq!(*resource, FaultResource::Sensor(2));
            }
            DeviceFault::Stuck { .. } => panic!("expected a sensor fault, got Stuck"),
        }
        match &fixture.device.faults[1] {
            DeviceFault::Transient { resource, .. } => {
                assert_eq!(*resource, FaultResource::Clock);
            }
            DeviceFault::Stuck { .. } => panic!("expected a clock fault, got Stuck"),
        }
    }

    #[test]
    fn fault_tool_filter_disagreeing_with_resource_is_rejected() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace(
            "\"faults\": []",
            "\"faults\": [{\"class\": \"transient\", \"tool\": \"sensor_sample_read\", \
             \"match_args\": {\"pin\": 4}, \"failures_before_success\": 1}]",
        );
        let err = parse_fixture(&broken).expect_err("filter/resource disagreement");
        assert!(err.to_string().contains("$.device.faults[0]"), "got {err}");
        assert!(err.to_string().contains("disagrees"), "got {err}");
    }

    #[test]
    fn unknown_fault_tool_is_rejected() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace(
            "\"faults\": []",
            "\"faults\": [{\"class\": \"transient\", \"tool\": \"laser_cannon\", \
             \"match_args\": {\"pin\": 4}, \"failures_before_success\": 1}]",
        );
        let err = parse_fixture(&broken).expect_err("unknown fault tool");
        assert!(
            err.to_string().contains("unknown tool `laser_cannon`"),
            "got {err}"
        );
    }

    #[test]
    fn stuck_fault_on_non_pin_resource_is_rejected() {
        let base = fixture_with(FINISH_TRACE);
        let broken = base.replace(
            "\"faults\": []",
            "\"faults\": [{\"stuck_level\": \"low\", \"match_args\": {\"sensor\": 1}}]",
        );
        let err = parse_fixture(&broken).expect_err("stuck sensor");
        assert!(err.to_string().contains("$.device.faults[0]"), "got {err}");
        assert!(
            err.to_string()
                .contains("stuck_level is a GPIO actuator fault"),
            "got {err}"
        );
    }
}
