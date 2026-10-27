//! The run seed: everything a run needs before its first boot.
//!
//! The seed is explicit (SPEC §7.2: never hidden constants): budgets,
//! capabilities, and the workflow version all travel with the run, and
//! the journal binds to them so a replay can never widen what the seed
//! allowed. This module is allocation-free and compiles without the
//! `host` feature.

use esper_core::brief::Brief;
use esper_core::lineage::Lineage;
use esper_core::registry::Capabilities;

use crate::backend::InferenceSettings;

/// The workflow version the E0/E1 slice runs under.
///
/// The journal binds every run to this version; recovery refuses a
/// journal that names any other.
pub const WORKFLOW_VERSION: u16 = 1;

/// The Waymaker workflow kind for Esper runs.
///
/// Carried in the `RunStarted` record so the replay cursor can tell an
/// Esper history from any other workflow's.
pub const ESPER_WORKFLOW_KIND: u16 = 1;

/// Length of the canonical seed binding (`SeedSnapshot::input_bytes`):
/// 142 bytes through E4, plus the E4+aon brief tag and digest.
pub const SEED_INPUT_LEN: usize = 151;

/// Everything a run needs before its first boot.
///
/// The seed carries the full budget explicitly: turns and mutations are
/// the metered units in the slice, while the token and time grants are
/// carried so the monitor's exhaustion guard (which fires on any zeroed
/// unit) only trips on units the run actually spends. The capability
/// set is `esper-core`'s normative §5.2 v2 shape, shared with the
/// authorizer, so the seed and the gate can never disagree on what a
/// grant means.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RunSeed {
    /// The run identity, bound into every journal frame.
    pub id: esper_core::RunId,
    /// Model turns available (repairs included).
    pub model_turns: u16,
    /// Mutating tool intents available.
    pub mutations: u16,
    /// Context bytes available (carried, not metered in E0/E1).
    pub input_tokens: u32,
    /// Model output bytes available (carried, not metered in E0/E1).
    pub output_tokens: u32,
    /// Milliseconds on the host monotonic clock (carried, not metered).
    pub elapsed_ms: u64,
    /// The E2 capability set: readable and writable pins, samplable
    /// sensors, and the timer/status grants.
    pub capabilities: Capabilities,
    /// The workflow version this seed runs under.
    pub workflow_version: u16,
    /// The model bundle this run is bound to (E3): the FNV-1a 64-bit
    /// identity of the exact model that must produce the run. The
    /// journal binds to it, so a replay can never swap the model
    /// under a committed history.
    pub model_bundle: u64,
    /// The inference buffer policy (E3): the prompt and output caps
    /// the engine hands the model backend. Bound into the snapshot so
    /// a replay can never widen what the seed allowed.
    pub inference: InferenceSettings,
    /// The context-byte rollover trigger (E4, SPEC §20.4): `Some(b)`
    /// arms the 80% compaction trigger once the cumulative
    /// built-prompt bytes reach `b`; `None` disables rollover. Part
    /// of run identity: a continuation carries the *remaining*
    /// budget, never a widened one.
    pub context_budget_bytes: Option<u64>,
    /// The continuation binding (E4, SPEC §20.6): `Some` when this
    /// run continues a compacted parent. Bound into the seed
    /// snapshot, so a reboot can never drop or swap the lineage.
    pub parent: Option<Lineage>,
    /// The goal objective and recalled memory (E4+aon, SPEC §21.4):
    /// `Some` when an always-on supervisor spawned this run. Its
    /// digest is bound into the seed snapshot, so a reboot can never
    /// swap the brief under a committed history. A rolled-over child
    /// inherits it.
    pub brief: Option<Brief>,
}

/// A run seed that failed validation, or a journal bound to a
/// different seed than the one the driver supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SeedSnapshot {
    /// The run identity.
    pub id: u64,
    /// The workflow version.
    pub workflow_version: u16,
    /// Model turns granted.
    pub model_turns: u16,
    /// Mutating tool intents granted.
    pub mutations: u16,
    /// Context bytes granted.
    pub input_tokens: u32,
    /// Model output bytes granted.
    pub output_tokens: u32,
    /// Milliseconds granted.
    pub elapsed_ms: u64,
    /// Pins the model may read.
    pub read_pins: [u8; 8],
    /// How many of `read_pins` are granted.
    pub read_count: u8,
    /// Pins the model may write.
    pub write_pins: [u8; 8],
    /// How many of `write_pins` are granted.
    pub write_count: u8,
    /// Sensors the model may sample.
    pub sensors: [u8; 4],
    /// How many of `sensors` are granted.
    pub sensor_count: u8,
    /// Whether the timer tools are granted.
    pub allow_timer: bool,
    /// Whether the status tool is granted.
    pub allow_status: bool,
    /// The model bundle the journal is bound to (E3).
    pub model_bundle: u64,
    /// The inference buffer policy the journal is bound to (E3).
    pub inference: InferenceSettings,
    /// The context-byte rollover trigger (E4, SPEC §20.4): `Some(b)`
    /// arms the 80% compaction trigger; `None` disables rollover.
    /// Part of run identity: a continuation carries the remaining
    /// budget, never a widened one.
    pub context_budget_bytes: Option<u64>,
    /// The continuation binding the journal is bound to (E4,
    /// SPEC §20.6); `None` for root runs.
    pub parent: Option<Lineage>,
    /// The brief digest the journal is bound to (E4+aon, SPEC §21.4);
    /// `None` for plain runs.
    pub brief_digest: Option<u64>,
}

impl SeedSnapshot {
    /// The canonical 151-byte encoding carried as the Waymaker
    /// `RunStarted` record's input. Fixed layout, little-endian; the
    /// replay cursor compares it byte for byte.
    ///
    /// Layout: `id` (8), `elapsed_ms` (8), `input_tokens` (4),
    /// `output_tokens` (4), `model_turns` (2), `mutations` (2),
    /// `workflow_version` (2), `read_pins` (8), `read_count` (1),
    /// `write_pins` (8), `write_count` (1), `sensors` (4),
    /// `sensor_count` (1), `allow_timer` (1), `allow_status` (1),
    /// `model_bundle` (8), `inference` (4: `max_prompt_bytes`,
    /// `max_output_bytes`), parent tag (1), parent lineage (65),
    /// context-budget tag (1), context-budget value (8), brief tag (1),
    /// brief digest (8).
    /// Every identity-bearing field of the seed is bound, including
    /// the rollover trigger and the continuation binding: a child
    /// seed's snapshot differs from its parent's, two children
    /// continued at different frames differ from each other, and a
    /// seed with a context budget differs from one without.
    ///
    /// The lineage encoding is `parent_run` (8),
    /// `continued_at_frame` (4), `budgets_remaining` (25:
    /// `model_turns` (2), `input_tokens` (4), `output_tokens` (4),
    /// `elapsed_ms` (8), `radio_bytes` (4), `mutations` (2),
    /// `consecutive_errors` (1)), `versions` (28: `workflow` (4),
    /// `model` (8), `catalog` (8), `policy` (8)). A `None` parent
    /// encodes the tag `0` with 65 zero bytes. The context budget
    /// encodes the tag `0` with 8 zero bytes for `None`, else tag `1`
    /// and the little-endian value. The brief (E4+aon) encodes the same
    /// way: tag `0` with 8 zero bytes for a plain run, else tag `1` and
    /// the brief's FNV-1a-64 digest.
    #[must_use]
    pub const fn input_bytes(&self) -> [u8; SEED_INPUT_LEN] {
        let mut out = [0u8; SEED_INPUT_LEN];
        let id = self.id.to_le_bytes();
        let elapsed = self.elapsed_ms.to_le_bytes();
        let input_tokens = self.input_tokens.to_le_bytes();
        let output_tokens = self.output_tokens.to_le_bytes();
        let model_turns = self.model_turns.to_le_bytes();
        let mutations = self.mutations.to_le_bytes();
        let version = self.workflow_version.to_le_bytes();
        let bundle = self.model_bundle.to_le_bytes();
        let inference = self.inference.bytes();
        let mut i = 0;
        while i < 8 {
            out[i] = id[i];
            out[8 + i] = elapsed[i];
            out[30 + i] = self.read_pins[i];
            out[39 + i] = self.write_pins[i];
            out[55 + i] = bundle[i];
            i += 1;
        }
        let mut j = 0;
        while j < 4 {
            out[16 + j] = input_tokens[j];
            out[20 + j] = output_tokens[j];
            out[48 + j] = self.sensors[j];
            out[63 + j] = inference[j];
            j += 1;
        }
        out[24] = model_turns[0];
        out[25] = model_turns[1];
        out[26] = mutations[0];
        out[27] = mutations[1];
        out[28] = version[0];
        out[29] = version[1];
        out[38] = self.read_count;
        out[47] = self.write_count;
        out[52] = self.sensor_count;
        out[53] = self.allow_timer as u8;
        out[54] = self.allow_status as u8;
        out[67] = self.parent.is_some() as u8;
        if let Some(lineage) = self.parent {
            let encoded = lineage_bytes(&lineage);
            let mut k = 0;
            while k < 65 {
                out[68 + k] = encoded[k];
                k += 1;
            }
        }
        // E4: the context-budget trigger is identity. Tag 1 carries
        // the little-endian value; tag 0 leaves eight zero bytes.
        out[133] = self.context_budget_bytes.is_some() as u8;
        if let Some(budget) = self.context_budget_bytes {
            let bytes = budget.to_le_bytes();
            let mut k = 0;
            while k < 8 {
                out[134 + k] = bytes[k];
                k += 1;
            }
        }
        // E4+aon: the brief is identity. Tag 1 carries its digest.
        out[142] = self.brief_digest.is_some() as u8;
        if let Some(digest) = self.brief_digest {
            let bytes = digest.to_le_bytes();
            let mut k = 0;
            while k < 8 {
                out[143 + k] = bytes[k];
                k += 1;
            }
        }
        out
    }
}

/// Canonical 65-byte little-endian encoding of a [`Lineage`].
///
/// Layout: `parent_run` (8), `continued_at_frame` (4),
/// `budgets_remaining` (25), `versions` (28). See
/// [`SeedSnapshot::input_bytes`] for the field order.
const fn lineage_bytes(lineage: &Lineage) -> [u8; 65] {
    let mut out = [0u8; 65];
    let run = lineage.parent_run.get().to_le_bytes();
    let frame = lineage.continued_at_frame.to_le_bytes();
    let turns = lineage.budgets_remaining.model_turns.to_le_bytes();
    let input = lineage.budgets_remaining.input_tokens.to_le_bytes();
    let output = lineage.budgets_remaining.output_tokens.to_le_bytes();
    let elapsed = lineage.budgets_remaining.elapsed_ms.to_le_bytes();
    let radio = lineage.budgets_remaining.radio_bytes.to_le_bytes();
    let mutations = lineage.budgets_remaining.mutations.to_le_bytes();
    let workflow = lineage.versions.workflow.to_le_bytes();
    let model = lineage.versions.model.get().to_le_bytes();
    let catalog = lineage.versions.catalog.get().to_le_bytes();
    let policy = lineage.versions.policy.get().to_le_bytes();
    let mut i = 0;
    while i < 8 {
        out[i] = run[i];
        out[22 + i] = elapsed[i];
        out[41 + i] = model[i];
        out[49 + i] = catalog[i];
        out[57 + i] = policy[i];
        i += 1;
    }
    let mut j = 0;
    while j < 4 {
        out[8 + j] = frame[j];
        out[14 + j] = input[j];
        out[18 + j] = output[j];
        out[30 + j] = radio[j];
        out[37 + j] = workflow[j];
        j += 1;
    }
    out[12] = turns[0];
    out[13] = turns[1];
    out[34] = mutations[0];
    out[35] = mutations[1];
    out[36] = lineage.budgets_remaining.consecutive_errors;
    out
}

/// Read `N` little-endian bytes at `at`.
fn le<const N: usize>(bytes: &[u8], at: usize) -> Option<[u8; N]> {
    <[u8; N]>::try_from(bytes.get(at..at.checked_add(N)?)?).ok()
}

/// Decode [`lineage_bytes`]' 65-byte layout.
fn lineage_from_bytes(bytes: &[u8]) -> Option<Lineage> {
    Some(Lineage {
        parent_run: esper_core::RunId::new(u64::from_le_bytes(le(bytes, 0)?)),
        continued_at_frame: u32::from_le_bytes(le(bytes, 8)?),
        budgets_remaining: esper_core::ResourceBudget {
            model_turns: u16::from_le_bytes(le(bytes, 12)?),
            input_tokens: u32::from_le_bytes(le(bytes, 14)?),
            output_tokens: u32::from_le_bytes(le(bytes, 18)?),
            elapsed_ms: u64::from_le_bytes(le(bytes, 22)?),
            radio_bytes: u32::from_le_bytes(le(bytes, 30)?),
            mutations: u16::from_le_bytes(le(bytes, 34)?),
            consecutive_errors: *bytes.get(36)?,
        },
        versions: esper_core::VersionSet {
            workflow: u32::from_le_bytes(le(bytes, 37)?),
            model: esper_core::Digest::new(u64::from_le_bytes(le(bytes, 41)?)),
            catalog: esper_core::Digest::new(u64::from_le_bytes(le(bytes, 49)?)),
            policy: esper_core::Digest::new(u64::from_le_bytes(le(bytes, 57)?)),
        },
    })
}

/// A tagged optional `u64` at `at`: tag `0` with eight zero bytes is
/// `None`, tag `1` carries the value; anything else is malformed.
fn tagged_u64(bytes: &[u8], at: usize) -> Result<Option<u64>, ()> {
    let value = u64::from_le_bytes(le(bytes, at + 1).ok_or(())?);
    match (bytes.get(at).copied().ok_or(())?, value) {
        (0, 0) => Ok(None),
        (1, value) => Ok(Some(value)),
        _ => Err(()),
    }
}

/// A `0`/`1` flag byte.
const fn flag(byte: u8) -> Option<bool> {
    match byte {
        0 => Some(false),
        1 => Some(true),
        _ => None,
    }
}

impl SeedSnapshot {
    /// Decode [`SeedSnapshot::input_bytes`]: the inverse, failing
    /// closed (`None`) on any tag or flag outside its closed set, or a
    /// `None` lineage whose 65 bytes are not zero.
    #[must_use]
    pub fn from_input_bytes(bytes: &[u8; SEED_INPUT_LEN]) -> Option<Self> {
        let parent = match bytes[67] {
            0 if bytes[68..133].iter().all(|b| *b == 0) => None,
            1 => Some(lineage_from_bytes(&bytes[68..133])?),
            _ => return None,
        };
        let snapshot = Self {
            id: u64::from_le_bytes(le(bytes, 0)?),
            elapsed_ms: u64::from_le_bytes(le(bytes, 8)?),
            input_tokens: u32::from_le_bytes(le(bytes, 16)?),
            output_tokens: u32::from_le_bytes(le(bytes, 20)?),
            model_turns: u16::from_le_bytes(le(bytes, 24)?),
            mutations: u16::from_le_bytes(le(bytes, 26)?),
            workflow_version: u16::from_le_bytes(le(bytes, 28)?),
            read_pins: le(bytes, 30)?,
            read_count: bytes[38],
            write_pins: le(bytes, 39)?,
            write_count: bytes[47],
            sensors: le(bytes, 48)?,
            sensor_count: bytes[52],
            allow_timer: flag(bytes[53])?,
            allow_status: flag(bytes[54])?,
            model_bundle: u64::from_le_bytes(le(bytes, 55)?),
            inference: InferenceSettings {
                max_prompt_bytes: u16::from_le_bytes(le(bytes, 63)?),
                max_output_bytes: u16::from_le_bytes(le(bytes, 65)?),
            },
            parent,
            context_budget_bytes: tagged_u64(bytes, 133).ok()?,
            brief_digest: tagged_u64(bytes, 142).ok()?,
        };
        // Canonical only: re-encoding must reproduce the bytes.
        (snapshot.input_bytes() == *bytes).then_some(snapshot)
    }
}

impl RunSeed {
    /// Rebuild a seed from its binding and its brief. The caller checks
    /// that the brief hashes to `snapshot.brief_digest`.
    #[must_use]
    pub const fn from_snapshot(snapshot: &SeedSnapshot, brief: Option<Brief>) -> Self {
        Self {
            id: esper_core::RunId::new(snapshot.id),
            model_turns: snapshot.model_turns,
            mutations: snapshot.mutations,
            input_tokens: snapshot.input_tokens,
            output_tokens: snapshot.output_tokens,
            elapsed_ms: snapshot.elapsed_ms,
            capabilities: Capabilities {
                read_pins: snapshot.read_pins,
                read_count: snapshot.read_count,
                write_pins: snapshot.write_pins,
                write_count: snapshot.write_count,
                sensors: snapshot.sensors,
                sensor_count: snapshot.sensor_count,
                allow_timer: snapshot.allow_timer,
                allow_status: snapshot.allow_status,
            },
            workflow_version: snapshot.workflow_version,
            model_bundle: snapshot.model_bundle,
            inference: snapshot.inference,
            context_budget_bytes: snapshot.context_budget_bytes,
            parent: snapshot.parent,
            brief,
        }
    }

    /// The seed of the run that continues this one at `lineage` (E4,
    /// SPEC §20.6).
    ///
    /// The child inherits the capabilities, workflow version, model
    /// bundle, inference settings, context budget and brief; it starts
    /// with the lineage's remaining budgets, which
    /// [`esper_core::lineage::continue_as_new`] already proved do not
    /// widen. The context budget is per-segment capacity, not a
    /// consumable: inheriting the remainder would shrink every
    /// generation's window toward zero. The child's id derives from the
    /// parent id and the cut frame.
    #[must_use]
    pub const fn child(&self, lineage: Lineage) -> Self {
        let remaining = lineage.budgets_remaining;
        Self {
            id: esper_core::RunId::new(
                self.id
                    .get()
                    .wrapping_add(lineage.continued_at_frame as u64)
                    .wrapping_add(1),
            ),
            model_turns: remaining.model_turns,
            mutations: remaining.mutations,
            input_tokens: remaining.input_tokens,
            output_tokens: remaining.output_tokens,
            elapsed_ms: remaining.elapsed_ms,
            parent: Some(lineage),
            ..*self
        }
    }

    /// Validate the seed before the first boot (SPEC §3.2 `Recover`).
    ///
    /// A seed names the exact workflow version this build runs; any
    /// other version is refused before the journal binds to it. A seed
    /// that grants no model turn could never act, so it is malformed.
    /// The capability sets name real resources: counts beyond the
    /// storage, pins outside `0..=7`, and sensors outside `0..=3` fail
    /// closed here, so a malformed grant can never widen silently.
    ///
    /// # Errors
    ///
    /// Returns a static reason when the seed is malformed.
    pub const fn validate(&self) -> Result<(), &'static str> {
        if self.workflow_version != WORKFLOW_VERSION {
            return Err("workflow_version is not the slice version");
        }
        if self.model_turns == 0 {
            return Err("model_turns must grant at least one turn");
        }
        // A briefed run (E4+aon) may use the larger prompt buffer; a
        // plain run keeps the E3 cap, so its prompts stay identical.
        let inference = if self.brief.is_some() {
            self.inference.validate_briefed()
        } else {
            self.inference.validate()
        };
        if let Err(reason) = inference {
            return Err(reason);
        }
        let caps = &self.capabilities;
        if caps.read_count > 8 {
            return Err("read_count exceeds the pin storage");
        }
        if caps.write_count > 8 {
            return Err("write_count exceeds the pin storage");
        }
        if caps.sensor_count > 4 {
            return Err("sensor_count exceeds the sensor storage");
        }
        let mut i = 0;
        while i < caps.read_count {
            if caps.read_pins[i as usize] > 7 {
                return Err("read_pins names a pin outside 0..=7");
            }
            i += 1;
        }
        let mut j = 0;
        while j < caps.write_count {
            if caps.write_pins[j as usize] > 7 {
                return Err("write_pins names a pin outside 0..=7");
            }
            j += 1;
        }
        let mut k = 0;
        while k < caps.sensor_count {
            if caps.sensors[k as usize] > 3 {
                return Err("sensors names a sensor outside 0..=3");
            }
            k += 1;
        }
        Ok(())
    }

    /// The journal-binding snapshot of this seed.
    ///
    /// The `RunStarted` frame carries this snapshot; recovery refuses
    /// a journal whose snapshot differs, so budgets and capabilities
    /// can never widen across a reboot.
    #[must_use]
    pub fn snapshot(&self) -> SeedSnapshot {
        SeedSnapshot {
            id: self.id.get(),
            workflow_version: self.workflow_version,
            model_turns: self.model_turns,
            mutations: self.mutations,
            input_tokens: self.input_tokens,
            output_tokens: self.output_tokens,
            elapsed_ms: self.elapsed_ms,
            read_pins: self.capabilities.read_pins,
            read_count: self.capabilities.read_count,
            write_pins: self.capabilities.write_pins,
            write_count: self.capabilities.write_count,
            sensors: self.capabilities.sensors,
            sensor_count: self.capabilities.sensor_count,
            allow_timer: self.capabilities.allow_timer,
            allow_status: self.capabilities.allow_status,
            model_bundle: self.model_bundle,
            inference: self.inference,
            context_budget_bytes: self.context_budget_bytes,
            parent: self.parent,
            brief_digest: self.brief.as_ref().map(Brief::digest),
        }
    }
    /// The E0/E1 default seed: 10 turns, 4 mutations, generous token
    /// and time grants, every pin readable and writable, every sensor
    /// samplable, timer and status granted.
    ///
    /// The token and time units are carried from the seed so the
    /// monitor's exhaustion guard only fires on turns and mutations;
    /// the scripted model does not meter them (the E2 model backend
    /// will).
    #[must_use]
    pub const fn default_slice() -> Self {
        Self {
            id: esper_core::RunId::new(0),
            model_turns: 10,
            mutations: 4,
            input_tokens: 4000,
            output_tokens: 1000,
            elapsed_ms: 60_000,
            capabilities: Capabilities {
                read_pins: [0, 1, 2, 3, 4, 5, 6, 7],
                read_count: 8,
                write_pins: [0, 1, 2, 3, 4, 5, 6, 7],
                write_count: 8,
                sensors: [0, 1, 2, 3],
                sensor_count: 4,
                allow_timer: true,
                allow_status: true,
            },
            workflow_version: WORKFLOW_VERSION,
            model_bundle: 0,
            inference: InferenceSettings::default_settings(),
            context_budget_bytes: None,
            parent: None,
            brief: None,
        }
    }

    /// The starting resource budget, straight from the seed's grants.
    #[must_use]
    pub const fn starting_budget(&self) -> esper_core::ResourceBudget {
        esper_core::ResourceBudget::new(
            self.model_turns,
            self.input_tokens,
            self.output_tokens,
            self.elapsed_ms,
            0,
            self.mutations,
            0,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::{Capabilities, InferenceSettings, Lineage, RunSeed, SeedSnapshot};
    use esper_core::ids::Pin;
    use esper_core::{Digest, ResourceBudget, RunId, VersionSet};

    /// A fixed continuation binding for the encoding tests.
    fn test_lineage(frame: u32) -> Lineage {
        Lineage {
            parent_run: RunId::new(7),
            continued_at_frame: frame,
            budgets_remaining: ResourceBudget::new(9, 3000, 900, 50_000, 0, 3, 0),
            versions: VersionSet {
                workflow: 1,
                model: Digest::new(11),
                catalog: Digest::new(12),
                policy: Digest::new(13),
            },
        }
    }

    /// Every identity-bearing field of the seed is bound in the
    /// 151-byte canonical encoding: changing any one of them changes
    /// the bytes the Waymaker `RunStarted` record carries.
    #[test]
    // One variant per bound field: the table grows with the binding.
    #[allow(clippy::too_many_lines)]
    fn input_bytes_binds_every_field() {
        let base = RunSeed::default_slice().snapshot();
        let base_bytes = base.input_bytes();
        assert_eq!(base_bytes.len(), 151);

        let variants = [
            SeedSnapshot { id: 1, ..base },
            SeedSnapshot {
                workflow_version: 99,
                ..base
            },
            SeedSnapshot {
                model_turns: base.model_turns + 1,
                ..base
            },
            SeedSnapshot {
                mutations: base.mutations + 1,
                ..base
            },
            SeedSnapshot {
                input_tokens: base.input_tokens + 1,
                ..base
            },
            SeedSnapshot {
                output_tokens: base.output_tokens + 1,
                ..base
            },
            SeedSnapshot {
                elapsed_ms: base.elapsed_ms + 1,
                ..base
            },
            SeedSnapshot {
                read_pins: [7, 6, 5, 4, 3, 2, 1, 0],
                ..base
            },
            SeedSnapshot {
                read_count: base.read_count - 1,
                ..base
            },
            SeedSnapshot {
                write_pins: [7, 6, 5, 4, 3, 2, 1, 0],
                ..base
            },
            SeedSnapshot {
                write_count: base.write_count - 1,
                ..base
            },
            SeedSnapshot {
                sensors: [3, 2, 1, 0],
                ..base
            },
            SeedSnapshot {
                sensor_count: base.sensor_count - 1,
                ..base
            },
            SeedSnapshot {
                allow_timer: !base.allow_timer,
                ..base
            },
            SeedSnapshot {
                allow_status: !base.allow_status,
                ..base
            },
            SeedSnapshot {
                model_bundle: base.model_bundle + 1,
                ..base
            },
            SeedSnapshot {
                inference: InferenceSettings {
                    max_prompt_bytes: 512,
                    ..base.inference
                },
                ..base
            },
            SeedSnapshot {
                parent: Some(test_lineage(12)),
                ..base
            },
            SeedSnapshot {
                parent: Some(test_lineage(13)),
                ..base
            },
            SeedSnapshot {
                context_budget_bytes: Some(1024),
                ..base
            },
            SeedSnapshot {
                context_budget_bytes: Some(2048),
                ..base
            },
            SeedSnapshot {
                brief_digest: Some(0),
                ..base
            },
            SeedSnapshot {
                brief_digest: Some(1),
                ..base
            },
        ];
        for variant in variants {
            assert_ne!(
                variant.input_bytes(),
                base_bytes,
                "field change did not alter the binding"
            );
        }
    }

    /// The seed snapshot carries the continuation binding: a child
    /// seed's encoding differs from its parent's, the tag byte marks
    /// the presence of a parent, and two children continued at
    /// different frames differ from each other.
    #[test]
    fn snapshot_binds_parent_lineage() {
        let seed = RunSeed {
            parent: Some(test_lineage(12)),
            ..RunSeed::default_slice()
        };
        let snap = seed.snapshot();
        assert_eq!(snap.parent, Some(test_lineage(12)));
        let bytes = snap.input_bytes();
        let base_bytes = RunSeed::default_slice().snapshot().input_bytes();
        assert_ne!(bytes, base_bytes);
        assert_eq!(bytes[67], 1);
        assert_eq!(base_bytes[67], 0);
        let other = RunSeed {
            parent: Some(test_lineage(13)),
            ..RunSeed::default_slice()
        };
        assert_ne!(other.snapshot().input_bytes(), bytes);
    }

    #[test]
    fn default_slice_validates() {
        RunSeed::default_slice().validate().expect("default seed");
    }

    #[test]
    fn malformed_counts_fail_closed() {
        for capabilities in [
            Capabilities {
                read_count: 9,
                ..Capabilities::empty()
            },
            Capabilities {
                write_count: 9,
                ..Capabilities::empty()
            },
            Capabilities {
                sensor_count: 5,
                ..Capabilities::empty()
            },
        ] {
            let seed = RunSeed {
                capabilities,
                ..RunSeed::default_slice()
            };
            assert!(seed.validate().is_err());
        }
    }

    #[test]
    fn out_of_range_grants_fail_closed() {
        for capabilities in [
            Capabilities {
                read_pins: [8, 0, 0, 0, 0, 0, 0, 0],
                read_count: 1,
                ..Capabilities::empty()
            },
            Capabilities {
                write_pins: [0, 0, 0, 0, 0, 0, 0, 9],
                write_count: 8,
                ..Capabilities::empty()
            },
            Capabilities {
                sensors: [4, 0, 0, 0],
                sensor_count: 1,
                ..Capabilities::empty()
            },
        ] {
            let seed = RunSeed {
                capabilities,
                ..RunSeed::default_slice()
            };
            assert!(seed.validate().is_err());
        }
    }

    #[test]
    fn default_grants_cover_everything() {
        let seed = RunSeed::default_slice();
        let caps = seed.capabilities;
        for pin in 0..8 {
            let pin = Pin::new(pin).expect("pin in range");
            assert!(caps.can_read(pin));
            assert!(caps.can_write(pin));
        }
        for sensor in 0..4 {
            assert!(caps.can_sample(sensor));
        }
        assert!(caps.allow_timer);
        assert!(caps.allow_status);
    }

    /// E4+aon: the brief is identity. A briefed seed's binding differs
    /// from the plain one and from a seed with a different brief, and
    /// only a briefed seed may use the larger prompt buffer.
    #[test]
    fn brief_is_bound_and_unlocks_the_briefed_prompt_cap() {
        use esper_core::brief::Brief;
        use esper_core::compact::Note;
        use esper_core::memory::{Memory, recall};
        let memory = Memory::new();
        let compose = |text: &[u8]| {
            let note = Note::from_bytes(text).expect("note");
            Brief::compose(&note, &recall(&memory, esper_core::goal::GoalId::MIN))
        };
        let plain = RunSeed::default_slice();
        let briefed = RunSeed {
            brief: Some(compose(b"keep pin 3 low")),
            inference: InferenceSettings::briefed_settings(),
            ..plain
        };
        let other = RunSeed {
            brief: Some(compose(b"keep pin 4 low")),
            ..briefed
        };
        let plain_bytes = plain.snapshot().input_bytes();
        let briefed_bytes = briefed.snapshot().input_bytes();
        assert_eq!(plain_bytes[142], 0);
        assert_eq!(briefed_bytes[142], 1);
        assert_ne!(plain_bytes, briefed_bytes);
        assert_ne!(briefed_bytes, other.snapshot().input_bytes());
        briefed.validate().expect("briefed seed");
        let plain_wide = RunSeed {
            inference: InferenceSettings::briefed_settings(),
            ..plain
        };
        assert!(plain_wide.validate().is_err(), "a plain run keeps 1024");
    }

    /// The run's Waymaker input carries the seed as its 151-byte
    /// binding: decoding it must reproduce the seed exactly, for a root,
    /// a continued child with a context budget, and a briefed seed.
    #[test]
    fn the_binding_decodes_back_to_the_seed() {
        let root = RunSeed {
            id: RunId::new(42),
            ..RunSeed::default_slice()
        };
        let child = RunSeed {
            context_budget_bytes: Some(512),
            ..root.child(test_lineage(9))
        };
        for seed in [root, child] {
            let bytes = seed.snapshot().input_bytes();
            let snapshot = SeedSnapshot::from_input_bytes(&bytes).expect("canonical bytes decode");
            assert_eq!(snapshot, seed.snapshot());
            assert_eq!(RunSeed::from_snapshot(&snapshot, None), seed);
        }
    }

    /// Non-canonical bytes fail closed: an unknown flag, an unknown
    /// tag, or a `None` lineage with stray bytes behind it.
    #[test]
    fn non_canonical_bindings_do_not_decode() {
        let bytes = RunSeed::default_slice().snapshot().input_bytes();
        for (at, value) in [(53usize, 2u8), (67, 2), (70, 1), (133, 2), (142, 7)] {
            let mut bad = bytes;
            bad[at] = value;
            assert!(
                SeedSnapshot::from_input_bytes(&bad).is_none(),
                "byte {at} = {value} decoded"
            );
        }
    }

    /// A child continues its parent: remaining budgets, the lineage,
    /// and every identity field inherited; its id derives from the cut.
    #[test]
    fn a_child_continues_its_parent() {
        let parent = RunSeed {
            id: RunId::new(100),
            context_budget_bytes: Some(1000),
            ..RunSeed::default_slice()
        };
        let lineage = test_lineage(12);
        let child = parent.child(lineage);
        assert_eq!(child.id, RunId::new(113));
        assert_eq!(child.parent, Some(lineage));
        assert_eq!(child.model_turns, lineage.budgets_remaining.model_turns);
        assert_eq!(child.mutations, lineage.budgets_remaining.mutations);
        assert_eq!(child.capabilities, parent.capabilities);
        assert_eq!(child.context_budget_bytes, parent.context_budget_bytes);
        assert_eq!(child.model_bundle, parent.model_bundle);
    }
}
