//! Flash partitions: Waymaker's storage contract over a board's NOR.
//!
//! The board owns one SPI flash chip and lends windows of it to several
//! Waymaker drivers at once — the supervisor's bank pair, each run slot's
//! bank pair, and the item logs. A driver for a run boots *inside* the
//! supervisor driver's activity, so two drivers hold a partition at the
//! same time; they never touch the media at the same time, because a
//! driver only reaches storage from inside its own calls. [`Partition`]
//! therefore shares the chip through a [`RefCell`] and borrows it per
//! operation; a borrow that finds it already lent is a fault
//! ([`PartitionError::Busy`]), never a wait.
//!
//! # The barrier
//!
//! Waymaker's §12 contract: after [`StableStorage::barrier`] returns,
//! every earlier successful mutation survives reset. `embedded-storage`'s
//! [`NorFlash::write`] and [`NorFlash::erase`] return once the operation
//! has finished — on the ESP32-S3 the ROM routines poll the chip's
//! write-in-progress bit before returning — so a mutation that returned
//! `Ok` is already durable and the barrier has nothing left to order.
//! That is a property of the driver the board links, stated here as the
//! board's precondition; a write-back driver would have to flush in
//! [`Partition::barrier`](StableStorage::barrier).

use core::cell::RefCell;

use embedded_storage::nor_flash::{NorFlash, NorFlashError, NorFlashErrorKind};
use waymaker_flash::storage::{Geometry, GeometryError, StableStorage};

/// Why a partition refused an operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PartitionError {
    /// The request was misaligned or out of the partition's bounds.
    #[error("the request does not fit the partition's geometry")]
    Geometry(GeometryError),
    /// The chip refused or failed the operation.
    #[error("the flash chip refused the operation")]
    Media(NorFlashErrorKind),
    /// Another partition was mid-operation on the same chip.
    #[error("the flash chip is already lent")]
    Busy,
}

/// A window of a shared NOR chip, seen as Waymaker's [`StableStorage`].
///
/// Offsets are partition-relative; `base` is where the window starts on
/// the chip. The geometry's units must be multiples of the chip's own
/// (`F::ERASE_SIZE`, `F::WRITE_SIZE`, `F::READ_SIZE`) — [`Partition::new`]
/// checks.
pub struct Partition<'f, F: NorFlash> {
    flash: &'f RefCell<F>,
    base: u32,
    geometry: Geometry,
}

// By hand: a derive would ask `F: Copy`, and a partition is a borrow of
// the chip plus two numbers whatever the chip is.
impl<F: NorFlash> Clone for Partition<'_, F> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<F: NorFlash> Copy for Partition<'_, F> {}

/// Why a partition could not be laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LayoutFault {
    /// The window's units are not multiples of the chip's.
    #[error("the window's units do not nest in the chip's")]
    Units,
    /// The window runs past the end of the chip, or does not start on
    /// an erase block.
    #[error("the window does not fit the chip")]
    Bounds,
}

impl<'f, F: NorFlash> Partition<'f, F> {
    /// The window of `flash` that starts at `base` and is shaped `geometry`.
    ///
    /// # Errors
    ///
    /// [`LayoutFault`] when the window does not fit the chip or its units
    /// do not nest in the chip's.
    pub fn new(flash: &'f RefCell<F>, base: u32, geometry: Geometry) -> Result<Self, LayoutFault> {
        let nests = |unit: u32, chip: usize| {
            u32::try_from(chip).is_ok_and(|chip| chip != 0 && unit.is_multiple_of(chip))
        };
        if !nests(geometry.erase_size(), F::ERASE_SIZE)
            || !nests(geometry.program_size(), F::WRITE_SIZE)
            || !nests(geometry.read_size(), F::READ_SIZE)
        {
            return Err(LayoutFault::Units);
        }
        let capacity = flash
            .try_borrow()
            .map_err(|_| LayoutFault::Bounds)?
            .capacity();
        let end = base
            .checked_add(geometry.capacity())
            .ok_or(LayoutFault::Bounds)?;
        let aligned = u32::try_from(F::ERASE_SIZE).is_ok_and(|erase| base.is_multiple_of(erase));
        if !aligned || usize::try_from(end).map_or(true, |end| end > capacity) {
            return Err(LayoutFault::Bounds);
        }
        Ok(Self {
            flash,
            base,
            geometry,
        })
    }

    /// Where the window starts on the chip.
    #[must_use]
    pub const fn base(&self) -> u32 {
        self.base
    }

    fn at(&self, offset: u32) -> Result<u32, PartitionError> {
        self.base
            .checked_add(offset)
            .ok_or(PartitionError::Geometry(GeometryError::OutOfBounds))
    }

    fn chip(&self) -> Result<core::cell::RefMut<'f, F>, PartitionError> {
        self.flash
            .try_borrow_mut()
            .map_err(|_| PartitionError::Busy)
    }
}

fn media<E: NorFlashError>(error: &E) -> PartitionError {
    PartitionError::Media(error.kind())
}

fn length(bytes: usize) -> Result<u32, PartitionError> {
    u32::try_from(bytes).map_err(|_| PartitionError::Geometry(GeometryError::OutOfBounds))
}

impl<F: NorFlash> StableStorage for Partition<'_, F> {
    type Error = PartitionError;

    fn geometry(&self) -> Geometry {
        self.geometry
    }

    fn read(&mut self, offset: u32, dst: &mut [u8]) -> Result<(), PartitionError> {
        self.geometry
            .validate_read(offset, length(dst.len())?)
            .map_err(PartitionError::Geometry)?;
        let at = self.at(offset)?;
        self.chip()?.read(at, dst).map_err(|error| media(&error))
    }

    fn program(&mut self, offset: u32, src: &[u8]) -> Result<(), PartitionError> {
        self.geometry
            .validate_program(offset, length(src.len())?)
            .map_err(PartitionError::Geometry)?;
        let at = self.at(offset)?;
        self.chip()?.write(at, src).map_err(|error| media(&error))
    }

    fn erase(&mut self, offset: u32, len: u32) -> Result<(), PartitionError> {
        self.geometry
            .validate_erase(offset, len)
            .map_err(PartitionError::Geometry)?;
        let from = self.at(offset)?;
        let to = from
            .checked_add(len)
            .ok_or(PartitionError::Geometry(GeometryError::OutOfBounds))?;
        self.chip()?.erase(from, to).map_err(|error| media(&error))
    }

    fn barrier(&mut self) -> Result<(), PartitionError> {
        // See the module documentation: a `NorFlash` mutation that returned
        // `Ok` has completed on the chip.
        Ok(())
    }
}
