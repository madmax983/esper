//! HOST-ONLY: a RAM-backed NOR chip with a power-cut injector, for the
//! board's tests. No allocation: the chip is a fixed array.

use embedded_storage::nor_flash::{
    ErrorType, NorFlash, NorFlashError, NorFlashErrorKind, ReadNorFlash,
};

/// Why the RAM chip refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RamError {
    /// Misaligned or out of bounds.
    Bounds,
    /// The injected power cut: this mutation and every later one fail
    /// until [`RamNor::power_on`].
    PowerCut,
}

impl NorFlashError for RamError {
    fn kind(&self) -> NorFlashErrorKind {
        match self {
            Self::Bounds => NorFlashErrorKind::OutOfBounds,
            Self::PowerCut => NorFlashErrorKind::Other,
        }
    }
}

/// How an injected power cut lands on the mutation it hits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cut {
    /// Power fails before the mutation starts: media is as the previous
    /// operation left it — a reset between two storage operations.
    Before,
    /// Power fails halfway through: half the program units of a write,
    /// or half the bytes of an erase, have changed.
    Torn,
}

/// `N` bytes of NOR: 4 KiB erase blocks, 4-byte writes, 1-byte reads; a
/// write clears bits and never sets them.
pub struct RamNor<const N: usize> {
    media: [u8; N],
    /// Mutations (writes and erases) performed so far.
    pub mutations: u32,
    /// The mutation number at which power is cut, and how, if armed.
    cut_at: Option<(u32, Cut)>,
    off: bool,
}

impl<const N: usize> RamNor<N> {
    /// An erased chip.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            media: [0xFF; N],
            mutations: 0,
            cut_at: None,
            off: false,
        }
    }

    /// Cut power at mutation number `at` (counted from now), landing as
    /// `cut`: that mutation fails, and so does everything after.
    pub const fn cut_after(&mut self, at: u32, cut: Cut) {
        self.cut_at = Some((self.mutations.saturating_add(at), cut));
    }

    /// Restore power after a cut; media keeps what it had.
    pub const fn power_on(&mut self) {
        self.off = false;
        self.cut_at = None;
    }

    /// Whether the injected cut has fired.
    #[must_use]
    pub const fn is_off(&self) -> bool {
        self.off
    }

    /// The raw media.
    #[must_use]
    pub const fn image(&self) -> &[u8; N] {
        &self.media
    }

    fn span(offset: u32, len: usize) -> Result<core::ops::Range<usize>, RamError> {
        let start = usize::try_from(offset).map_err(|_| RamError::Bounds)?;
        let end = start.checked_add(len).ok_or(RamError::Bounds)?;
        if end > N {
            return Err(RamError::Bounds);
        }
        Ok(start..end)
    }

    /// Count a mutation; `Ok(true)` when the cut tears it, `Err` when
    /// power is (or goes) off before it starts.
    fn mutate(&mut self) -> Result<bool, RamError> {
        if self.off {
            return Err(RamError::PowerCut);
        }
        self.mutations += 1;
        match self.cut_at {
            Some((at, cut)) if at == self.mutations => {
                self.off = true;
                if cut == Cut::Before {
                    return Err(RamError::PowerCut);
                }
                Ok(true)
            }
            _ => Ok(false),
        }
    }
}

impl<const N: usize> Default for RamNor<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ErrorType for RamNor<N> {
    type Error = RamError;
}

impl<const N: usize> ReadNorFlash for RamNor<N> {
    const READ_SIZE: usize = 1;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), RamError> {
        let span = Self::span(offset, bytes.len())?;
        bytes.copy_from_slice(self.media.get(span).ok_or(RamError::Bounds)?);
        Ok(())
    }

    fn capacity(&self) -> usize {
        N
    }
}

impl<const N: usize> NorFlash for RamNor<N> {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = 4096;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), RamError> {
        if !from.is_multiple_of(4096) || !to.is_multiple_of(4096) || to < from {
            return Err(RamError::Bounds);
        }
        let len = usize::try_from(to - from).map_err(|_| RamError::Bounds)?;
        let span = Self::span(from, len)?;
        let torn = self.mutate()?;
        let media = self.media.get_mut(span).ok_or(RamError::Bounds)?;
        if torn {
            // Half the range erased, the rest untouched.
            let half = media.len() / 2;
            media.get_mut(..half).unwrap_or_default().fill(0xFF);
            return Err(RamError::PowerCut);
        }
        media.fill(0xFF);
        Ok(())
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), RamError> {
        if !offset.is_multiple_of(4) || !bytes.len().is_multiple_of(4) {
            return Err(RamError::Bounds);
        }
        let span = Self::span(offset, bytes.len())?;
        let torn = self.mutate()?;
        let media = self.media.get_mut(span).ok_or(RamError::Bounds)?;
        // A tear lands on a program-unit boundary: half the units.
        let take = if torn {
            bytes.len() / 2 / 4 * 4
        } else {
            bytes.len()
        };
        for (cell, byte) in media.iter_mut().zip(bytes).take(take) {
            *cell &= *byte;
        }
        if torn {
            return Err(RamError::PowerCut);
        }
        Ok(())
    }
}
