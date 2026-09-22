//! Host-only world doubles for the E0/E1 slice.
//!
//! `FakeDevice` is an 8-pin GPIO double with idempotent redelivery
//! plus four fixed sensor channels and a shared virtual millisecond
//! clock (SPEC §15.13–§15.14), `FaultPlan` injects transient device
//! hiccups, and `InputPlan` delivers typed human input for `Ask`
//! suspension. The scripted model backend lives in
//! [`crate::backend`]: [`crate::backend::ScriptedBackend`] emits the
//! canned lines the harness used to pull from `ScriptedModel`.
//!
//! The verifier's clock handle ([`FakeDevice::clock_read`]) is a
//! separate `&self` method from the dispatch path: the verifier reads
//! the clock register directly, never a flag the dispatcher set
//! (SPEC §9.1, §17.6).
//!
//! The doubles model the *physical world*, not runtime memory: the
//! [`FaultPlan`] and the [`FakeDevice`]'s pin state survive simulated
//! crashes, exactly as a real device would. Only the engine's
//! in-memory state (budgets, monitor, allocator, cursor) is rebuilt
//! from the journal on recovery.
//!
//! Effect identity uses Waymaker's [`EffectId`]: the device
//! deduplicates redelivery on the full (run, sequence) identity, so a
//! redelivered intent can never double-execute — and a continued run's
//! restarted sequence space can never collide with its parent's.
//!
//! // HOST-ONLY (E0/E1): heap-allocated doubles for the host test
//! harness. The firmware port (E2) replaces these with the real model
//! backend and GPIO driver behind the same engine boundary.

use esper_core::decision::Level;
use esper_core::ids::Pin;
use waymaker_core::EffectId;

use esper_core::decision::StatusDetail;

/// A GPIO pin's direction, as reported by the device.
///
/// The runtime checks direction *after* capability authorization
/// (SPEC §5.2): a write to a pin the device reports as input is denied
/// without touching hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    /// The pin is an input; writes are refused.
    Input,
    /// The pin is an output; writes are allowed.
    Output,
}

/// What a world double can report that the engine cannot fix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorldError {
    /// A redelivered intent's arguments disagree with the committed
    /// intent under the same effect id. History is unambiguous, so the
    /// redelivery is refused rather than applied.
    EffectArgsMismatch,
    /// A dispatch named a resource the device does not have.
    /// Unreachable: the decoder proves every range before dispatch.
    UnknownResource,
}

/// The fixed raw reading per sensor channel (SPEC §15.13): no noise,
/// no drift.
pub const SENSOR_VALUES: [u16; 4] = [210, 315, 1800, 42];

/// The fixed raw reading of one sensor channel.
const fn sensor_value(sensor: u8) -> Option<u16> {
    match sensor {
        0 => Some(SENSOR_VALUES[0]),
        1 => Some(SENSOR_VALUES[1]),
        2 => Some(SENSOR_VALUES[2]),
        3 => Some(SENSOR_VALUES[3]),
        _ => None,
    }
}

/// The transient-fault injector: a scripted device hiccup plan.
///
/// Each planned failure fires once for a matching `(tool, resource)`
/// pair, then the device behaves. A `None` tool filter matches any
/// tool (preserving the E0/E1 pin-only behavior). The plan is world
/// state: it survives simulated crashes, so a reboot never replays a
/// hiccup the first boot already consumed.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    /// `(tool filter, resource, failures remaining)` entries.
    // HOST-ONLY (E0/E1)
    transient: Vec<(Option<u8>, u8, u32)>,
}

impl FaultPlan {
    /// An empty plan: the device never hiccups.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            // HOST-ONLY (E0/E1)
            transient: Vec::new(),
        }
    }

    /// Plan `times` transient failures for this pin's tool calls,
    /// whichever tool names it.
    pub fn fail_transient(&mut self, pin: Pin, times: u32) {
        // HOST-ONLY (E0/E1)
        self.transient.push((None, pin.get(), times));
    }

    /// Plan `times` transient failures for one tool's resource: the
    /// pin for the GPIO tools, the sensor id for `sensor_sample_read`,
    /// 0 for the timer and status tools (SPEC §17.7).
    pub fn fail_transient_for(&mut self, tool: u8, resource: u8, times: u32) {
        // HOST-ONLY (E0/E1)
        self.transient.push((Some(tool), resource, times));
    }

    /// Consume one planned failure for this dispatch, if any remains.
    ///
    /// Returns true when the dispatch must fail transiently. The
    /// engine consults the plan *before* touching the device, so a
    /// transient failure never reaches hardware.
    pub fn consume(&mut self, tool: u8, resource: u8) -> bool {
        // HOST-ONLY (E0/E1)
        for entry in &mut self.transient {
            let tool_matches = entry.0.is_none_or(|filter| filter == tool);
            if tool_matches && entry.1 == resource && entry.2 > 0 {
                entry.2 -= 1;
                return true;
            }
        }
        false
    }
}

/// The typed-human-input plan: queued `Ask` deliveries.
///
/// The caller owns the plan; a delivery queued after a suspension
/// resumes the same run on the next boot.
#[derive(Debug, Clone, Default)]
pub struct InputPlan {
    /// Queued input payloads, in delivery order.
    // HOST-ONLY (E0/E1)
    inputs: Vec<Vec<u8>>,
    /// How many have been delivered.
    next: usize,
}

impl InputPlan {
    /// Build a plan from queued input payloads, in delivery order.
    #[must_use]
    pub const fn new(inputs: Vec<Vec<u8>>) -> Self {
        Self { inputs, next: 0 }
    }

    /// Take the next queued input, or `None` when nothing arrived.
    pub fn take(&mut self) -> Option<Vec<u8>> {
        // HOST-ONLY (E0/E1)
        let input = self.inputs.get(self.next)?.clone();
        self.next += 1;
        Some(input)
    }
}

/// One physical write, for the harness ledger.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WriteRecord {
    /// The driven pin.
    pub pin: u8,
    /// The driven level.
    pub level_high: bool,
    /// The effect sequence that caused it.
    pub seq: u32,
}

/// A committed effect outcome, for redelivery deduplication.
#[derive(Debug, Clone)]
struct EffectRecord {
    /// The stable effect identity: the run that minted it and its
    /// sequence in that run's history. Keying on the full identity
    /// (not the bare sequence) keeps continued runs — which restart
    /// their sequence space on a fresh journal — from colliding in
    /// the shared device's redelivery cache (E4).
    id: EffectId,
    /// The digest of the committed intent's arguments.
    digest: u64,
    /// The outcome bytes to replay on redelivery.
    // HOST-ONLY (E0/E1)
    outcome: Vec<u8>,
}

/// The fake device: GPIO, sensors, and a virtual clock with idempotent
/// redelivery.
///
/// Dispatch is keyed by ([`EffectId`], argument digest): a redelivered
/// intent replays its committed outcome without touching the device,
/// and a redelivery whose arguments disagree with the committed intent
/// is refused. At-least-once dispatch is therefore safe (SPEC ADR:
/// at-least-once effects).
#[derive(Debug, Clone)]
pub struct FakeDevice {
    /// Per pin: (direction, level high?).
    pins: [(Direction, bool); 8],
    /// Per pin: stuck actuator level override, if any.
    stuck: [Option<bool>; 8],
    /// The virtual millisecond clock shared by the timer and status
    /// tools (SPEC §15.14).
    clock_ms: u64,
    /// Committed effect outcomes, for redelivery dedup.
    // HOST-ONLY (E0/E1)
    effects: Vec<EffectRecord>,
    /// Physical write ledger, for the harness.
    // HOST-ONLY (E0/E1)
    ledger: Vec<WriteRecord>,
}

impl FakeDevice {
    /// All pins output, low, nothing stuck, clock at zero.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pins: [(Direction::Output, false); 8],
            stuck: [None; 8],
            clock_ms: 0,
            // HOST-ONLY (E0/E1)
            effects: Vec::new(),
            // HOST-ONLY (E0/E1)
            ledger: Vec::new(),
        }
    }

    /// Read a pin's current level.
    #[must_use]
    pub const fn read(&self, pin: Pin) -> Level {
        if self.pins[pin.get() as usize].1 {
            Level::High
        } else {
            Level::Low
        }
    }

    /// The direction the device reports for a pin.
    #[must_use]
    pub const fn direction(&self, pin: Pin) -> Direction {
        self.pins[pin.get() as usize].0
    }

    /// Set the direction the device reports for a pin.
    pub const fn set_direction(&mut self, pin: Pin, direction: Direction) {
        self.pins[pin.get() as usize].0 = direction;
    }

    /// Make a pin's actuator stuck: writes are acknowledged but the
    /// level never changes. `None` clears the fault.
    pub fn set_stuck(&mut self, pin: Pin, level: Option<Level>) {
        self.stuck[pin.get() as usize] = level.map(|l| l == Level::High);
    }

    /// Dispatch a read under this effect id, deduplicating redelivery.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EffectArgsMismatch`] when the same effect
    /// id arrives with different arguments than the committed intent.
    pub fn dispatch_read(
        &mut self,
        pin: Pin,
        id: EffectId,
        digest: u64,
    ) -> Result<Vec<u8>, WorldError> {
        if let Some(cached) = self.effect_outcome(id, digest)? {
            return Ok(cached);
        }
        let outcome = pin_level_json(pin.get(), self.pins[pin.get() as usize].1);
        // HOST-ONLY (E0/E1)
        self.effects.push(EffectRecord {
            id,
            digest,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// Dispatch an idempotent write under this effect id.
    ///
    /// A stuck actuator acknowledges the write without moving the pin;
    /// the independent verifier observes the mismatch.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EffectArgsMismatch`] when the same effect
    /// id arrives with different arguments than the committed intent.
    pub fn dispatch_write(
        &mut self,
        pin: Pin,
        level_high: bool,
        id: EffectId,
        digest: u64,
    ) -> Result<Vec<u8>, WorldError> {
        if let Some(cached) = self.effect_outcome(id, digest)? {
            return Ok(cached);
        }
        let index = pin.get() as usize;
        if self.stuck[index].is_none() {
            self.pins[index].1 = level_high;
        }
        // HOST-ONLY (E0/E1)
        self.ledger.push(WriteRecord {
            pin: pin.get(),
            level_high,
            seq: id.seq.0,
        });
        let outcome = pin_level_json(pin.get(), level_high);
        self.effects.push(EffectRecord {
            id,
            digest,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// The verifier's independent read-back: the pin's physical level,
    /// bypassing the effect cache.
    #[must_use]
    pub const fn verify_read(&self, pin: Pin) -> bool {
        self.pins[pin.get() as usize].1
    }

    /// The verifier's independent clock read (SPEC §9.1, §17.6).
    ///
    /// This reads the clock register directly — a separate handle
    /// from the dispatch path. It is `&self`: the verifier never
    /// mutates device state, and there is no flag the dispatcher sets
    /// for the verifier to read.
    #[must_use]
    pub const fn clock_read(&self) -> u64 {
        self.clock_ms
    }

    /// Dispatch a sensor read under this effect id, deduplicating
    /// redelivery.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EffectArgsMismatch`] when the same effect
    /// id arrives with different arguments than the committed intent,
    /// or [`WorldError::UnknownResource`] for a sensor the device does
    /// not have (unreachable: the decoder proves `0..=3`).
    pub fn dispatch_sensor_read(
        &mut self,
        sensor: u8,
        id: EffectId,
        digest: u64,
    ) -> Result<Vec<u8>, WorldError> {
        if let Some(cached) = self.effect_outcome(id, digest)? {
            return Ok(cached);
        }
        let value = sensor_value(sensor).ok_or(WorldError::UnknownResource)?;
        let outcome = sensor_json(sensor, value);
        // HOST-ONLY (E0/E1)
        self.effects.push(EffectRecord {
            id,
            digest,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// Dispatch an uptime read under this effect id, deduplicating
    /// redelivery. Read-only: the clock is not touched.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EffectArgsMismatch`] when the same effect
    /// id arrives with different arguments than the committed intent.
    pub fn dispatch_uptime_read(
        &mut self,
        id: EffectId,
        digest: u64,
    ) -> Result<Vec<u8>, WorldError> {
        if let Some(cached) = self.effect_outcome(id, digest)? {
            return Ok(cached);
        }
        let outcome = uptime_json(self.clock_ms);
        // HOST-ONLY (E0/E1)
        self.effects.push(EffectRecord {
            id,
            digest,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// Dispatch a delay under this effect id: advance the virtual
    /// clock to at least `t0 + ms`, where `t0` is the pre-dispatch
    /// reading (SPEC §15.14).
    ///
    /// Redelivery under the same effect id replays the cached outcome
    /// without advancing again: the committed effect is the
    /// set-operation "clock reaches at least `t0 + ms`", so executing
    /// it twice would change the world a second time.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EffectArgsMismatch`] when the same effect
    /// id arrives with different arguments than the committed intent.
    pub fn dispatch_delay_wait(
        &mut self,
        ms: u16,
        id: EffectId,
        digest: u64,
    ) -> Result<Vec<u8>, WorldError> {
        if let Some(cached) = self.effect_outcome(id, digest)? {
            return Ok(cached);
        }
        let t0 = self.clock_ms;
        let target = t0.saturating_add(u64::from(ms));
        self.clock_ms = target;
        let outcome = delay_json(target - t0, target);
        // HOST-ONLY (E0/E1)
        self.effects.push(EffectRecord {
            id,
            digest,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// Dispatch a status report under this effect id, deduplicating
    /// redelivery. Read-only: the device is not touched.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EffectArgsMismatch`] when the same effect
    /// id arrives with different arguments than the committed intent.
    pub fn dispatch_status_report(
        &mut self,
        detail: StatusDetail,
        id: EffectId,
        digest: u64,
    ) -> Result<Vec<u8>, WorldError> {
        if let Some(cached) = self.effect_outcome(id, digest)? {
            return Ok(cached);
        }
        let outcome = match detail {
            StatusDetail::Summary => status_summary_json(status_uptime(self.clock_ms)),
            StatusDetail::Full => status_full_json(
                status_uptime(self.clock_ms),
                self.pin_dir_array(),
                self.pin_level_array(),
            ),
        };
        // HOST-ONLY (E0/E1)
        self.effects.push(EffectRecord {
            id,
            digest,
            outcome: outcome.clone(),
        });
        Ok(outcome)
    }

    /// The pin-direction array for the full status report (SPEC
    /// §17.6): element `i` is 1 when pin `i` is an output, 0 for an
    /// input.
    const fn pin_dir_array(&self) -> [u8; 8] {
        let mut array = [0u8; 8];
        let mut pin: u8 = 0;
        while pin < 8 {
            if matches!(self.pins[pin as usize].0, Direction::Output) {
                array[pin as usize] = 1;
            }
            pin += 1;
        }
        array
    }

    /// The pin-level array for the full status report (SPEC §17.6):
    /// element `i` is 1 when pin `i` is high, 0 when low.
    const fn pin_level_array(&self) -> [u8; 8] {
        let mut array = [0u8; 8];
        let mut pin: u8 = 0;
        while pin < 8 {
            if self.pins[pin as usize].1 {
                array[pin as usize] = 1;
            }
            pin += 1;
        }
        array
    }

    /// How many physical writes have executed (redeliveries excluded).
    #[must_use]
    pub fn physical_writes(&self) -> u32 {
        // HOST-ONLY (E0/E1)
        u32::try_from(self.ledger.len()).unwrap_or(u32::MAX)
    }

    /// The physical write ledger, in execution order.
    #[must_use]
    pub fn write_ledger(&self) -> &[WriteRecord] {
        &self.ledger
    }

    /// Look up a committed outcome for redelivery deduplication.
    fn effect_outcome(&self, id: EffectId, digest: u64) -> Result<Option<Vec<u8>>, WorldError> {
        // HOST-ONLY (E0/E1)
        for record in &self.effects {
            if record.id == id {
                if record.digest != digest {
                    return Err(WorldError::EffectArgsMismatch);
                }
                return Ok(Some(record.outcome.clone()));
            }
        }
        Ok(None)
    }
}

impl Default for FakeDevice {
    fn default() -> Self {
        Self::new()
    }
}

/// The canonical JSON for a pin level: `{"pin":4,"level":"high"}`.
fn pin_level_json(pin: u8, level_high: bool) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    format!(
        "{{\"pin\":{pin},\"level\":\"{}\"}}",
        if level_high { "high" } else { "low" }
    )
    .into_bytes()
}

/// The canonical JSON for a sensor reading: `{"sensor":2,"value":1800}`.
fn sensor_json(sensor: u8, value: u16) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    format!("{{\"sensor\":{sensor},\"value\":{value}}}").into_bytes()
}

/// The canonical JSON for a clock reading: `{"uptime_ms":1250}`.
fn uptime_json(uptime_ms: u64) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    format!("{{\"uptime_ms\":{uptime_ms}}}").into_bytes()
}

/// The canonical JSON for a completed delay:
/// `{"waited_ms":250,"uptime_ms":1250}`.
fn delay_json(waited_ms: u64, uptime_ms: u64) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    format!("{{\"waited_ms\":{waited_ms},\"uptime_ms\":{uptime_ms}}}").into_bytes()
}

/// The clock reading the status payloads report, saturated at
/// `u32::MAX`: the status report is a bounded diagnostic (SPEC §17.6
/// sets a 128-byte result bound), and a 49-day virtual uptime is the
/// most a bounded payload can name. The timer tools report the raw
/// `u64` clock; only the status payload saturates.
fn status_uptime(clock_ms: u64) -> u32 {
    // HOST-ONLY (E0/E1)
    u32::try_from(clock_ms).unwrap_or(u32::MAX)
}

/// The canonical JSON for a status summary: `{"pins":8,"sensors":4,"uptime_ms":1250}`.
fn status_summary_json(uptime_ms: u32) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    format!("{{\"pins\":8,\"sensors\":4,\"uptime_ms\":{uptime_ms}}}").into_bytes()
}

/// The canonical JSON for a full status report (SPEC §17.6): the
/// summary plus the pin direction array (`dir`, element `i` is 1 when
/// pin `i` is an output), the pin level array (`level`, element `i`
/// is 1 when pin `i` is high), and the per-sensor raw values
/// (`values`). Stays within the 128-byte result bound by
/// construction: the clock saturates at `u32::MAX` (see
/// [`status_uptime`]).
fn status_full_json(uptime_ms: u32, dir: [u8; 8], level: [u8; 8]) -> Vec<u8> {
    // HOST-ONLY (E0/E1)
    let [s0, s1, s2, s3] = SENSOR_VALUES;
    let [d0, d1, d2, d3, d4, d5, d6, d7] = dir;
    let [l0, l1, l2, l3, l4, l5, l6, l7] = level;
    format!(
        "{{\"pins\":8,\"sensors\":4,\"uptime_ms\":{uptime_ms},\
         \"dir\":[{d0},{d1},{d2},{d3},{d4},{d5},{d6},{d7}],\
         \"level\":[{l0},{l1},{l2},{l3},{l4},{l5},{l6},{l7}],\
         \"values\":[{s0},{s1},{s2},{s3}]}}"
    )
    .into_bytes()
}

#[cfg(test)]
mod tests {
    use super::{status_full_json, status_summary_json};
    use waymaker_core::{EffectId, EffectSeq, RunId};

    use super::FakeDevice;

    /// A test effect identity on a fixed run: the device keys its
    /// redelivery cache on the full `(run, seq)` identity (E4), so
    /// tests name the run explicitly.
    fn eid(run: u64, seq: u32) -> EffectId {
        EffectId {
            run: RunId(run),
            seq: EffectSeq(seq),
        }
    }

    #[test]
    fn status_full_stays_within_128_bytes() {
        // The clock saturates at `u32::MAX` in the status payload (see
        // `status_uptime`); every pin an output and high is the widest
        // array encoding.
        let payload = status_full_json(u32::MAX, [1; 8], [1; 8]);
        assert!(
            payload.len() <= 128,
            "full status is {} bytes",
            payload.len()
        );
        assert_eq!(
            payload,
            br#"{"pins":8,"sensors":4,"uptime_ms":4294967295,"dir":[1,1,1,1,1,1,1,1],"level":[1,1,1,1,1,1,1,1],"values":[210,315,1800,42]}"#
        );
    }

    #[test]
    fn status_uptime_saturates_at_u32_max() {
        assert_eq!(super::status_uptime(0), 0);
        assert_eq!(super::status_uptime(1_250), 1_250);
        assert_eq!(super::status_uptime(u64::from(u32::MAX)), u32::MAX);
        assert_eq!(super::status_uptime(u64::MAX), u32::MAX);
    }

    #[test]
    fn status_summary_is_canonical() {
        assert_eq!(
            status_summary_json(1250),
            br#"{"pins":8,"sensors":4,"uptime_ms":1250}"#
        );
    }

    #[test]
    fn delay_redelivery_advances_the_clock_exactly_once() {
        let mut device = FakeDevice::new();
        let id = eid(7, 1);
        let first = device
            .dispatch_delay_wait(250, id, 7)
            .expect("first dispatch");
        let second = device.dispatch_delay_wait(250, id, 7).expect("redelivery");
        assert_eq!(first, second);
        assert_eq!(device.clock_read(), 250);
        // A redelivery with different arguments is refused, not applied.
        assert!(device.dispatch_delay_wait(100, id, 8).is_err());
        assert_eq!(device.clock_read(), 250);
        // E4: the same sequence from a DIFFERENT run is a different
        // effect, not a redelivery — continued runs restart their
        // sequence space on a fresh journal. (The outcome bytes differ
        // because the payload names the clock; the point is the
        // dispatch is accepted, not refused as an args mismatch.)
        device
            .dispatch_delay_wait(250, eid(11, 1), 9)
            .expect("other run's seq 1 dispatches cleanly");
        assert_eq!(device.clock_read(), 500);
    }

    #[test]
    fn uptime_read_does_not_touch_the_clock() {
        let mut device = FakeDevice::new();
        device
            .dispatch_delay_wait(250, eid(7, 1), 1)
            .expect("delay");
        let outcome = device.dispatch_uptime_read(eid(7, 2), 2).expect("uptime");
        assert_eq!(outcome, b"{\"uptime_ms\":250}");
        assert_eq!(device.clock_read(), 250);
    }

    #[test]
    fn sensor_read_returns_fixed_values() {
        let mut device = FakeDevice::new();
        for (sensor, value) in [(0u8, 210u16), (1, 315), (2, 1800), (3, 42)] {
            let outcome = device
                .dispatch_sensor_read(sensor, eid(7, u32::from(sensor) + 1), u64::from(sensor))
                .expect("sensor read");
            assert_eq!(
                outcome,
                format!("{{\"sensor\":{sensor},\"value\":{value}}}").into_bytes()
            );
        }
    }
}
