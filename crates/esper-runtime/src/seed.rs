//! The run seed: everything a run needs before its first boot.
//!
//! The seed is explicit (SPEC §7.2: never hidden constants): budgets,
//! capabilities, and the workflow version all travel with the run, and
//! the journal binds to them so a replay can never widen what the seed
//! allowed. This module is allocation-free and compiles without the
//! `host` feature.

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
/// unit) only trips on units the run actually spends.
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
    /// Pins the model may read (bitmask over pins `0..=7`).
    pub readable_pins: u8,
    /// Pins the model may write (bitmask over pins `0..=7`).
    pub writable_pins: u8,
    /// The workflow version this seed runs under.
    pub workflow_version: u16,
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
    /// Pins the model may read (bitmask over pins `0..=7`).
    pub readable_pins: u8,
    /// Pins the model may write (bitmask over pins `0..=7`).
    pub writable_pins: u8,
}

impl SeedSnapshot {
    /// The canonical 32-byte encoding carried as the Waymaker
    /// `RunStarted` record's input. Fixed layout, little-endian; the
    /// replay cursor compares it byte for byte.
    ///
    /// Layout: `id` (8), `elapsed_ms` (8), `input_tokens` (4),
    /// `output_tokens` (4), `model_turns` (2), `mutations` (2),
    /// `workflow_version` (2), `readable_pins` (1), `writable_pins`
    /// (1). Every identity-bearing field of the seed is bound.
    #[must_use]
    pub const fn input_bytes(&self) -> [u8; 32] {
        let mut out = [0u8; 32];
        let id = self.id.to_le_bytes();
        let elapsed = self.elapsed_ms.to_le_bytes();
        let input_tokens = self.input_tokens.to_le_bytes();
        let output_tokens = self.output_tokens.to_le_bytes();
        let model_turns = self.model_turns.to_le_bytes();
        let mutations = self.mutations.to_le_bytes();
        let version = self.workflow_version.to_le_bytes();
        let mut i = 0;
        while i < 8 {
            out[i] = id[i];
            out[8 + i] = elapsed[i];
            i += 1;
        }
        let mut j = 0;
        while j < 4 {
            out[16 + j] = input_tokens[j];
            out[20 + j] = output_tokens[j];
            j += 1;
        }
        out[24] = model_turns[0];
        out[25] = model_turns[1];
        out[26] = mutations[0];
        out[27] = mutations[1];
        out[28] = version[0];
        out[29] = version[1];
        out[30] = self.readable_pins;
        out[31] = self.writable_pins;
        out
    }
}

impl RunSeed {
    /// Validate the seed before the first boot (SPEC §3.2 `Recover`).
    ///
    /// A seed names the exact workflow version this build runs; any
    /// other version is refused before the journal binds to it. A seed
    /// that grants no model turn could never act, so it is malformed.
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
            readable_pins: self.readable_pins,
            writable_pins: self.writable_pins,
        }
    }
    /// The E0/E1 default seed: 10 turns, 4 mutations, generous token
    /// and time grants, all pins readable and writable.
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
            readable_pins: 0xFF,
            writable_pins: 0xFF,
            workflow_version: WORKFLOW_VERSION,
        }
    }

    /// The capability set this seed grants, derived from the pin masks.
    #[must_use]
    pub const fn capabilities(&self) -> esper_core::registry::Capabilities {
        let mut caps = esper_core::registry::Capabilities::empty();
        let mut pin: u8 = 0;
        while pin < 8 {
            if self.readable_pins & (1 << pin) != 0 {
                caps.read_pins[caps.read_count as usize] = pin;
                caps.read_count += 1;
            }
            if self.writable_pins & (1 << pin) != 0 {
                caps.write_pins[caps.write_count as usize] = pin;
                caps.write_count += 1;
            }
            pin += 1;
        }
        caps
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
    use super::{RunSeed, SeedSnapshot};

    /// Every identity-bearing field of the seed is bound in the
    /// 32-byte canonical encoding: changing any one of them changes
    /// the bytes the Waymaker `RunStarted` record carries.
    #[test]
    fn input_bytes_binds_every_field() {
        let base = RunSeed::default_slice().snapshot();
        let base_bytes = base.input_bytes();
        assert_eq!(base_bytes.len(), 32);

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
                readable_pins: !base.readable_pins,
                ..base
            },
            SeedSnapshot {
                writable_pins: !base.writable_pins,
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

    #[test]
    fn default_slice_validates() {
        RunSeed::default_slice().validate().expect("default seed");
    }
}
