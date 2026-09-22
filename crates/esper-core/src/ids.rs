//! Strong identifier newtypes. Raw primitives never cross an API boundary.
//!
//! Every identifier in the journal vocabulary is a newtype so that a
//! `RunId` can never be confused with a `ToolId`, a pin number can never
//! be used as an effect sequence, and so on. All types are `Copy`.

use core::fmt::{Display, Formatter, Result as FmtResult};

use crate::error::Error;

/// Number of GPIO pins in the slice device: pins are numbered `0..8`.
pub const PIN_COUNT: u8 = 8;

/// Number of sensor channels in the E2 device: sensors are numbered
/// `0..4`, with the fixed raw values from SPEC §15.13.
pub const SENSOR_COUNT: u8 = 4;

/// Identifies one workflow run. A new run requires a new `RunId` (and a
/// new `RunSeed`); run identity never changes after creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RunId(u64);

impl RunId {
    /// Build a `RunId` from its raw value.
    #[must_use]
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    /// Return the raw value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Display for RunId {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "run-{}", self.0)
    }
}

/// Per-run sequence number of a tool or verification activity. Together
/// with the [`RunId`] it forms the [`EffectId`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectSeq(u32);

impl EffectSeq {
    /// Build an `EffectSeq` from its raw value.
    #[must_use]
    pub const fn new(seq: u32) -> Self {
        Self(seq)
    }

    /// Return the raw value.
    #[must_use]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// The next sequence number, or `None` on overflow.
    ///
    /// Overflow of the sequence space is a hard error, never a silent
    /// wrap: the workflow must end the run instead of reusing an id.
    #[must_use]
    pub const fn next(self) -> Option<Self> {
        match self.0.checked_add(1) {
            Some(n) => Some(Self(n)),
            None => None,
        }
    }
}

impl Display for EffectSeq {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}", self.0)
    }
}

/// Stable identity of one dispatched effect: `(RunId, EffectSeq)`.
///
/// Redelivery after a crash reuses the original `EffectId`; it is never
/// re-minted. Transient retries share the `EffectId` of the first
/// attempt, so the retry count stays derivable after reboot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EffectId {
    run: RunId,
    seq: EffectSeq,
}

impl EffectId {
    /// Build an `EffectId` from its run and sequence parts.
    #[must_use]
    pub const fn new(run: RunId, seq: EffectSeq) -> Self {
        Self { run, seq }
    }

    /// The run this effect belongs to.
    #[must_use]
    pub const fn run(self) -> RunId {
        self.run
    }

    /// The per-run sequence number.
    #[must_use]
    pub const fn seq(self) -> EffectSeq {
        self.seq
    }
}

impl Display for EffectId {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{}:{}", self.run.get(), self.seq.get())
    }
}

/// Stable numeric tool identifier (not a name string). The slice catalog
/// assigns `1` to `gpio_pin_read` and `2` to `gpio_pin_write`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ToolId(u8);

impl ToolId {
    /// Build a `ToolId` from its raw value.
    #[must_use]
    pub const fn new(id: u8) -> Self {
        Self(id)
    }

    /// Return the raw value.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Display for ToolId {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "tool-{}", self.0)
    }
}

/// A GPIO pin number. Construction enforces the `0..8` range with a
/// typed error, so code holding a `Pin` never needs a second check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Pin(u8);

impl Pin {
    /// Build a `Pin`, rejecting numbers outside `0..=7`.
    ///
    /// # Errors
    ///
    /// Returns [`Error::PinOutOfRange`] when `pin` is `8` or greater.
    pub const fn new(pin: u8) -> Result<Self, Error> {
        if pin < PIN_COUNT {
            Ok(Self(pin))
        } else {
            Err(Error::PinOutOfRange { pin })
        }
    }

    /// Return the raw pin number.
    #[must_use]
    pub const fn get(self) -> u8 {
        self.0
    }
}

impl Display for Pin {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "pin-{}", self.0)
    }
}

/// Content digest used to correlate retries and feed the monitor's
/// identical-failure rule. Computed at decode time over the raw argument
/// bytes with FNV-1a-64.
///
/// The digest is a correlation key, not a cryptographic commitment: equal
/// input bytes always give equal digests, and the slice's scripted model
/// replays byte-identical lines, so retries of one turn share a digest.
/// A general canonicalization is deferred to `esper-protocol` (E2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest(u64);

impl Digest {
    /// Build a `Digest` from a raw hash value.
    #[must_use]
    pub const fn new(hash: u64) -> Self {
        Self(hash)
    }

    /// Return the raw hash value.
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    /// Compute the FNV-1a-64 digest of `bytes`.
    #[must_use]
    pub fn of_bytes(bytes: &[u8]) -> Self {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x0100_0000_01b3);
        }
        Self(hash)
    }
}

impl Display for Digest {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        write!(f, "{:016x}", self.0)
    }
}
