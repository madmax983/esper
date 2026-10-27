//! [`Plan`]: where the board's slots and logs live on the chip.
//!
//! ```text
//! base ─┬─ supervisor slot        2 banks
//!       ├─ run slots × RUN_SLOTS  2 banks each
//!       ├─ checkpoint item log    2 areas
//!       └─ snapshot item log      2 areas
//! ```
//!
//! Every window is whole erase blocks and starts on one. The sizes are
//! one number, `bank_bytes`: a slot is two banks, a log is two areas of
//! one bank each.

use core::cell::RefCell;

use embedded_storage::nor_flash::NorFlash;
use esper_runtime::agent::MAX_START_BYTES;
use esper_runtime::agent::wire::PAGE_RECORD_MAX;
use esper_runtime::run::wire::MAX_RUN_START_BYTES;
use waymaker_flash::bank::BankLayout;
use waymaker_flash::capacity::{Bounds, Reserve};
use waymaker_flash::storage::Geometry;

use crate::flash::{LayoutFault, Partition};

/// Run slots on the chip: one per goal that can hold a live run at once.
pub const RUN_SLOTS: usize = 4;

/// The erase block every window is made of.
pub const ERASE_BYTES: u32 = 4096;

/// The program unit every window writes in.
pub const PROGRAM_BYTES: u32 = 4;

/// The read unit every window reads in.
pub const READ_BYTES: u32 = 4;

/// What a run's records may be worth, for §10's reserve.
#[allow(clippy::cast_possible_truncation)] // every bound is const-asserted below u16::MAX
pub const RUN_BOUNDS: Bounds = Bounds {
    run_input_bytes: MAX_RUN_START_BYTES as u16,
    effect_result_bytes: PAGE_RECORD_MAX as u16,
    terminal_bytes: PAGE_RECORD_MAX as u16,
};

/// What the supervisor's records may be worth, for §10's reserve.
#[allow(clippy::cast_possible_truncation)] // every bound is const-asserted below u16::MAX
pub const SUPERVISOR_BOUNDS: Bounds = Bounds {
    run_input_bytes: MAX_START_BYTES as u16,
    effect_result_bytes: PAGE_RECORD_MAX as u16,
    terminal_bytes: PAGE_RECORD_MAX as u16,
};

const _: () = assert!(MAX_RUN_START_BYTES < u16::MAX as usize);
const _: () = assert!(MAX_START_BYTES < u16::MAX as usize);
const _: () = assert!(PAGE_RECORD_MAX < u16::MAX as usize);

/// Windows on the chip: a supervisor slot, the run slots, two logs.
#[allow(clippy::cast_possible_truncation)] // RUN_SLOTS is a small constant
const WINDOWS: u32 = 1 + RUN_SLOTS as u32 + 2;

/// Every window the board uses, carved out of one chip.
pub struct Plan<'f, F: NorFlash> {
    /// The supervisor's bank pair.
    pub supervisor: Partition<'f, F>,
    /// The run slots.
    pub runs: [Partition<'f, F>; RUN_SLOTS],
    /// The checkpoint item log.
    pub checkpoints: Partition<'f, F>,
    /// The snapshot item log.
    pub snapshots: Partition<'f, F>,
    /// The bank-pair layout every slot shares.
    pub layout: BankLayout,
    /// A run slot's reserve.
    pub run_reserve: Reserve,
    /// The supervisor slot's reserve.
    pub supervisor_reserve: Reserve,
}

impl<'f, F: NorFlash> Plan<'f, F> {
    /// Bytes the plan occupies for banks of `bank_bytes`.
    #[must_use]
    pub const fn bytes(bank_bytes: u32) -> u32 {
        // A supervisor slot, the run slots, and two logs: two banks each.
        WINDOWS * 2 * bank_bytes
    }

    /// The plan at `base` on `flash`, with banks of `bank_bytes`.
    ///
    /// # Errors
    ///
    /// [`LayoutFault`] when a window does not fit the chip, or the banks
    /// cannot hold a run's reserve.
    pub fn new(flash: &'f RefCell<F>, base: u32, bank_bytes: u32) -> Result<Self, LayoutFault> {
        let pair = bank_bytes.checked_mul(2).ok_or(LayoutFault::Bounds)?;
        let geometry = Geometry::new(pair, ERASE_BYTES, PROGRAM_BYTES, READ_BYTES)
            .map_err(|_| LayoutFault::Units)?;
        let layout = BankLayout::new(geometry).map_err(|_| LayoutFault::Units)?;
        let run_reserve =
            Reserve::for_layout(RUN_BOUNDS, layout).map_err(|_| LayoutFault::Bounds)?;
        let supervisor_reserve =
            Reserve::for_layout(SUPERVISOR_BOUNDS, layout).map_err(|_| LayoutFault::Bounds)?;
        let window = |index: u32| -> Result<Partition<'f, F>, LayoutFault> {
            let at = index
                .checked_mul(pair)
                .and_then(|offset| base.checked_add(offset))
                .ok_or(LayoutFault::Bounds)?;
            Partition::new(flash, at, geometry)
        };
        let mut runs = [window(1)?; RUN_SLOTS];
        for (index, slot) in (2..).zip(runs.iter_mut().skip(1)) {
            *slot = window(index)?;
        }
        let after = WINDOWS - 2;
        Ok(Self {
            supervisor: window(0)?,
            runs,
            checkpoints: window(after)?,
            snapshots: window(after + 1)?,
            layout,
            run_reserve,
            supervisor_reserve,
        })
    }
}
