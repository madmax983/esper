//! The run seed: everything a run needs before its first boot.
//!
//! The seed is explicit (SPEC §7.2: never hidden constants): budgets,
//! capabilities, and the workflow version all travel with the run, and
//! the journal binds to them so a replay can never widen what the seed
//! allowed. This module is allocation-free and compiles without the
//! `host` feature.

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
    /// The continuation binding the journal is bound to (E4,
    /// SPEC §20.6); `None` for root runs.
    pub parent: Option<Lineage>,
}

impl SeedSnapshot {
    /// The canonical 133-byte encoding carried as the Waymaker
    /// `RunStarted` record's input. Fixed layout, little-endian; the
    /// replay cursor compares it byte for byte.
    ///
    /// Layout: `id` (8), `elapsed_ms` (8), `input_tokens` (4),
    /// `output_tokens` (4), `model_turns` (2), `mutations` (2),
    /// `workflow_version` (2), `read_pins` (8), `read_count` (1),
    /// `write_pins` (8), `write_count` (1), `sensors` (4),
    /// `sensor_count` (1), `allow_timer` (1), `allow_status` (1),
    /// `model_bundle` (8), `inference` (4: `max_prompt_bytes`,
    /// `max_output_bytes`), parent tag (1), parent lineage (65).
    /// Every identity-bearing field of the seed is bound, including
    /// the continuation binding: a child seed's snapshot differs
    /// from its parent's, and two children continued at different
    /// frames differ from each other.
    ///
    /// The lineage encoding is `parent_run` (8),
    /// `continued_at_frame` (4), `budgets_remaining` (25:
    /// `model_turns` (2), `input_tokens` (4), `output_tokens` (4),
    /// `elapsed_ms` (8), `radio_bytes` (4), `mutations` (2),
    /// `consecutive_errors` (1)), `versions` (28: `workflow` (4),
    /// `model` (8), `catalog` (8), `policy` (8)). A `None` parent
    /// encodes the tag `0` with 65 zero bytes.
    #[must_use]
    pub const fn input_bytes(&self) -> [u8; 133] {
        let mut out = [0u8; 133];
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
        match self.parent {
            Some(lineage) => {
                let encoded = lineage_bytes(&lineage);
                let mut k = 0;
                while k < 65 {
                    out[68 + k] = encoded[k];
                    k += 1;
                }
            }
            None => {}
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

impl RunSeed {
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
        if let Err(reason) = self.inference.validate() {
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
    pub const fn snapshot(&self) -> SeedSnapshot {
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
            parent: self.parent,
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
    /// 133-byte canonical encoding: changing any one of them changes
    /// the bytes the Waymaker `RunStarted` record carries.
    #[test]
    fn input_bytes_binds_every_field() {
        let base = RunSeed::default_slice().snapshot();
        let base_bytes = base.input_bytes();
        assert_eq!(base_bytes.len(), 133);

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
}
