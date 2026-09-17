//! Host-only world doubles for the E0/E1 slice.
//!
//! `ScriptedModel` emits canned model lines, `FakeDevice` is an 8-pin
//! GPIO double with idempotent redelivery, `FaultPlan` injects
//! transient device hiccups, and `InputPlan` delivers typed human input
//! for `Ask` suspension.
//!
//! The doubles model the *physical world*, not runtime memory: the
//! [`FaultPlan`] and the [`FakeDevice`]'s pin state survive simulated
//! crashes, exactly as a real device would. Only the engine's
//! in-memory state (budgets, monitor, allocator, cursor) is rebuilt
//! from the journal on recovery.
//!
//! Effect identity uses Waymaker's [`EffectSeq`]: the device
//! deduplicates redelivery on the committed sequence, so a redelivered
//! intent can never double-execute.
//!
//! // HOST-ONLY (E0/E1): heap-allocated doubles for the host test
//! harness. The firmware port (E2) replaces these with the real model
//! backend and GPIO driver behind the same engine boundary.

use esper_core::decision::Level;
use esper_core::ids::Pin;
use waymaker_core::EffectSeq;

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
}

/// The scripted model backend: emits canned lines, one per turn.
///
/// Repair lines are just later script lines; the script already
/// contains what the model would emit after a repair hint.
#[derive(Debug, Clone)]
pub struct ScriptedModel {
    /// The canned lines, in emission order.
    // HOST-ONLY (E0/E1)
    lines: Vec<Vec<u8>>,
    /// How many lines have been emitted.
    index: usize,
}

impl ScriptedModel {
    /// Build a model from canned lines, in emission order.
    #[must_use]
    pub const fn new(lines: Vec<Vec<u8>>) -> Self {
        Self { lines, index: 0 }
    }

    /// Emit the next line, or `None` when the script is exhausted.
    ///
    /// An exhausted script is a harness bug, not a run outcome; the
    /// engine surfaces it as [`crate::RuntimeError::World`].
    pub fn next_line(&mut self) -> Option<Vec<u8>> {
        // HOST-ONLY (E0/E1)
        let line = self.lines.get(self.index)?.clone();
        self.index += 1;
        Some(line)
    }

    /// Rewind one emission: the last line was taken but never
    /// committed (crash before the decision commit), so the next boot
    /// must see it again.
    pub const fn unemit(&mut self) {
        self.index = self.index.saturating_sub(1);
    }

    /// How many lines have been emitted so far.
    #[must_use]
    pub const fn lines_consumed(&self) -> usize {
        self.index
    }
}

/// The transient-fault injector: a scripted device hiccup plan.
///
/// Each planned failure fires once for a matching pin, then the
/// device behaves. The plan is world state: it survives simulated
/// crashes, so a reboot never replays a hiccup the first boot already
/// consumed.
#[derive(Debug, Clone, Default)]
pub struct FaultPlan {
    /// `(pin, failures remaining)` entries.
    // HOST-ONLY (E0/E1)
    transient: Vec<(u8, u32)>,
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

    /// Plan `times` transient failures for this pin's tool calls.
    pub fn fail_transient(&mut self, pin: Pin, times: u32) {
        // HOST-ONLY (E0/E1)
        self.transient.push((pin.get(), times));
    }

    /// Consume one planned failure for this dispatch, if any remains.
    ///
    /// Returns true when the dispatch must fail transiently. The
    /// engine consults the plan *before* touching the device, so a
    /// transient failure never reaches hardware.
    pub fn consume(&mut self, _tool: u8, pin: u8) -> bool {
        // HOST-ONLY (E0/E1)
        for entry in &mut self.transient {
            if entry.0 == pin && entry.1 > 0 {
                entry.1 -= 1;
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
    /// The stable effect identity.
    seq: EffectSeq,
    /// The digest of the committed intent's arguments.
    digest: u64,
    /// The outcome bytes to replay on redelivery.
    // HOST-ONLY (E0/E1)
    outcome: Vec<u8>,
}

/// The fake GPIO device: 8 pins with idempotent redelivery.
///
/// Dispatch is keyed by ([`EffectSeq`], argument digest): a redelivered
/// intent replays its committed outcome without touching the pins, and
/// a redelivery whose arguments disagree with the committed intent is
/// refused. At-least-once dispatch is therefore safe (SPEC ADR:
/// at-least-once effects).
#[derive(Debug, Clone)]
pub struct FakeDevice {
    /// Per pin: (direction, level high?).
    pins: [(Direction, bool); 8],
    /// Per pin: stuck actuator level override, if any.
    stuck: [Option<bool>; 8],
    /// Committed effect outcomes, for redelivery dedup.
    // HOST-ONLY (E0/E1)
    effects: Vec<EffectRecord>,
    /// Physical write ledger, for the harness.
    // HOST-ONLY (E0/E1)
    ledger: Vec<WriteRecord>,
}

impl FakeDevice {
    /// All pins output, low, nothing stuck.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            pins: [(Direction::Output, false); 8],
            stuck: [None; 8],
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
        seq: EffectSeq,
        digest: u64,
    ) -> Result<Vec<u8>, WorldError> {
        if let Some(cached) = self.effect_outcome(seq, digest)? {
            return Ok(cached);
        }
        let outcome = pin_level_json(pin.get(), self.pins[pin.get() as usize].1);
        // HOST-ONLY (E0/E1)
        self.effects.push(EffectRecord {
            seq,
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
        seq: EffectSeq,
        digest: u64,
    ) -> Result<Vec<u8>, WorldError> {
        if let Some(cached) = self.effect_outcome(seq, digest)? {
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
            seq: seq.0,
        });
        let outcome = pin_level_json(pin.get(), level_high);
        self.effects.push(EffectRecord {
            seq,
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
    fn effect_outcome(&self, seq: EffectSeq, digest: u64) -> Result<Option<Vec<u8>>, WorldError> {
        // HOST-ONLY (E0/E1)
        for record in &self.effects {
            if record.seq == seq {
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
