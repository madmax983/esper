//! [`FlashRuns`]: the board's [`RunStore`], one Waymaker bank pair per run.
//!
//! Each run lives in a [`slot`]. A slot's journal is driven
//! by `waymaker_drive::Driver::at_bank`, which boots the authoritative
//! bank's run, follows `continue_as_new` from bank to bank, and writes the
//! terminal record the run ends with. The nested run reaches its journal
//! through `waymaker_bridge::Bridge`, lent by [`RunStore::boot`] for one
//! boot — the driver owns the boot loop, so the store calls the run.
//!
//! # Which slot holds which run
//!
//! A slot is commissioned with the supervisor's run id as its Waymaker
//! `RunId`. [`esper_runtime::agent::run_id`] leaves the low 16 bits of a
//! supervised run id zero, and the driver numbers each `continue_as_new`
//! segment as its predecessor's successor, so every segment of a run
//! keeps the run's id in its top 48 bits — the store finds a run after
//! any number of rollovers without keeping a directory of its own.
//!
//! A new run takes, in order: the slot of its goal's previous run (a goal
//! spawns again only after its last run settled), an empty slot, or any
//! slot whose run has a terminal record. A pair with none of those is
//! [`StoreFault::Full`].

use embedded_storage::nor_flash::NorFlash;
use esper_core::ids::RunId as EsperRunId;
use esper_runtime::agent::device::{BodyEnd, RunBody, RunStore, StoreFault};
use esper_runtime::agent::goal_of;
use esper_runtime::agent::wire::PAGE_RECORD_MAX;
use waymaker_bridge::Bridge;
use waymaker_core::version::VersionRange;
use waymaker_core::{Outcome, RunId};
use waymaker_drive::{
    Activities, Boundary, CheckedDispatch, Clocks, DriveError, Driver, Identity, Performed,
    Scratch, Suspended, Workflow,
};
use waymaker_flash::bank::{BankHeader, BankLayout};
use waymaker_flash::capacity::Reserve;
use waymaker_flash::recovery::RecoveryError;

use crate::flash::{Partition, PartitionError};
use crate::slot::{self, HEADER_PAGE, SlotFault};

/// The workflow kind a run slot's header records.
pub const RUN_WORKFLOW_KIND: u16 = 0x0E01;

/// The workflow version every run slot is driven at.
pub const RUN_WORKFLOW_VERSION: u16 = 1;

/// The page a driver stages one record in.
pub const DRIVER_PAGE: usize = 1024;

/// The bits of a supervised run id a segment number may use.
const SEGMENT_MASK: u64 = 0xFFFF;

/// The world a bridged driver is booted with.
///
/// A bridged workflow dispatches through its `Ctx`'s dispatcher, never
/// through the driver's [`Activities`], so [`perform`](Activities::perform)
/// is never asked; if it were, `Pending` keeps the effect outstanding
/// rather than inventing an answer. The clocks are the board's.
pub struct Bridged<'c, C: Clocks>(pub &'c mut C);

impl<C: Clocks> Activities for Bridged<'_, C> {
    fn perform(&mut self, _dispatch: CheckedDispatch<'_>, _out: &mut [u8]) -> Performed {
        Performed::Pending
    }
}

impl<C: Clocks> Clocks for Bridged<'_, C> {
    fn capability(&self) -> waymaker_core::timer::ClockCapability {
        self.0.capability()
    }

    fn now(&mut self, kind: waymaker_core::timer::ClockKind) -> Option<u64> {
        self.0.now(kind)
    }
}

/// Whether Waymaker refused the journal itself.
///
/// A tail with no append point, or a record that does not decode —
/// rather than the media or the run. [`slot::reinstall`] is the board's
/// answer to exactly these.
pub const fn refused_journal<E>(error: &DriveError<E>) -> bool {
    matches!(
        error,
        DriveError::NoAppendPoint | DriveError::Recovery(RecoveryError::Decode(_))
    )
}

/// A drive error as the store reports it: a media refusal halts this
/// boot and leaves the answer outstanding; anything else is a fault.
pub const fn store_fault<E>(error: &DriveError<E>) -> StoreFault {
    match error {
        DriveError::Append(_) | DriveError::Recovery(_) | DriveError::SwapStep(_) => {
            StoreFault::Halted
        }
        _ => StoreFault::Unknown,
    }
}

const fn slot_fault<E>(error: &SlotFault<E>) -> StoreFault {
    match error {
        SlotFault::Media(_) => StoreFault::Halted,
        SlotFault::Layout | SlotFault::Ambiguous => StoreFault::Unknown,
    }
}

/// The nested run, as one driver boot's workflow.
struct BodyRun<'b> {
    body: &'b mut dyn RunBody,
    input: &'b [u8],
    out: [u8; PAGE_RECORD_MAX],
}

impl Workflow for BodyRun<'_> {
    fn identity(&self) -> Identity<'_> {
        Identity {
            kind: RUN_WORKFLOW_KIND,
            versions: VersionRange::exact(RUN_WORKFLOW_VERSION),
            input: self.input,
        }
    }

    fn run(&mut self, boundary: &mut dyn Boundary) -> Result<Outcome<'_>, Suspended> {
        let (ended, suspended) = {
            let mut bridge = Bridge::over(boundary);
            let ended = self.body.drive(&mut bridge, &mut self.out);
            (ended, bridge.take_suspended())
        };
        match ended {
            BodyEnd::Ended { completed, len } => {
                let bytes = self.out.get(..len).unwrap_or_default();
                Ok(if completed {
                    Outcome::Completed(bytes)
                } else {
                    Outcome::Failed(bytes)
                })
            }
            // The run is over without an ending of its own: Waymaker
            // records it failed, as its own examples record a workflow
            // that returned an error, so the slot is concluded and a
            // replay of it stops at that record.
            BodyEnd::Faulted => Ok(Outcome::Failed(&[])),
            BodyEnd::Stopped => Err(suspended.unwrap_or_else(Suspended::awaiting_dispatch)),
        }
    }
}

/// The board's run store over `N` slots of one flash chip.
pub struct FlashRuns<'f, F: NorFlash, C: Clocks, const N: usize> {
    slots: [Partition<'f, F>; N],
    layout: BankLayout,
    reserve: Reserve,
    clocks: C,
    selected: Option<usize>,
    reinstalled: u32,
    refused: Option<DriveError<PartitionError>>,
    page: [u8; DRIVER_PAGE],
    result: [u8; PAGE_RECORD_MAX],
}

impl<'f, F: NorFlash, C: Clocks, const N: usize> FlashRuns<'f, F, C, N> {
    /// A store over `slots`, each a bank pair shaped `layout` whose runs
    /// are priced by `reserve`, with the board's `clocks`.
    pub const fn new(
        slots: [Partition<'f, F>; N],
        layout: BankLayout,
        reserve: Reserve,
        clocks: C,
    ) -> Self {
        Self {
            slots,
            layout,
            reserve,
            clocks,
            selected: None,
            reinstalled: 0,
            refused: None,
            page: [0; DRIVER_PAGE],
            result: [0; PAGE_RECORD_MAX],
        }
    }

    /// Segments reinstalled after Waymaker refused their journal, since
    /// this store was built — for the board's log.
    #[must_use]
    pub const fn reinstalled(&self) -> u32 {
        self.reinstalled
    }

    /// The last error a run's driver refused a boot with, for the board's
    /// log: the dispatcher records a failure without its reason.
    #[must_use]
    pub const fn last_refusal(&self) -> Option<&DriveError<PartitionError>> {
        self.refused.as_ref()
    }

    /// The slot index the selected run lives in, for diagnostics.
    #[must_use]
    pub const fn selected(&self) -> Option<usize> {
        self.selected
    }

    /// The run id a slot holds, if a run slot was ever commissioned there.
    fn held(&self, index: usize) -> Result<Option<u64>, StoreFault> {
        let Some(mut slot) = self.slots.get(index).copied() else {
            return Ok(None);
        };
        let mut input = [0u8; HEADER_PAGE];
        match slot::authority(&mut slot, self.layout, &mut input) {
            Ok(Some(held)) if held.kind == RUN_WORKFLOW_KIND => {
                Ok(Some(held.run.0 & !SEGMENT_MASK))
            }
            Ok(_) => Ok(None),
            Err(error) => Err(slot_fault(&error)),
        }
    }

    fn concluded(&self, index: usize) -> Result<bool, StoreFault> {
        let Some(mut slot) = self.slots.get(index).copied() else {
            return Ok(false);
        };
        let mut out = [0u8; PAGE_RECORD_MAX];
        slot::terminal(&mut slot, self.layout, &mut out)
            .map(|terminal| terminal.is_some())
            .map_err(|error| slot_fault(&error))
    }

    /// The slot a new run `root` takes.
    fn vacancy(&self, root: u64) -> Result<usize, StoreFault> {
        let goal = goal_of(EsperRunId::new(root));
        let mut empty = None;
        let mut done = None;
        for index in 0..N {
            match self.held(index)? {
                Some(held) if goal.is_some() && goal_of(EsperRunId::new(held)) == goal => {
                    return Ok(index);
                }
                Some(_) => {
                    if done.is_none() && self.concluded(index)? {
                        done = Some(index);
                    }
                }
                None => {
                    if empty.is_none() {
                        empty = Some(index);
                    }
                }
            }
        }
        empty.or(done).ok_or(StoreFault::Full)
    }
}

impl<F: NorFlash, C: Clocks, const N: usize> RunStore for FlashRuns<'_, F, C, N> {
    fn select(&mut self, run: EsperRunId, start: Option<&[u8]>) -> Result<(), StoreFault> {
        let root = run.get();
        if root & SEGMENT_MASK != 0 {
            return Err(StoreFault::Unknown);
        }
        for index in 0..N {
            if self.held(index)? == Some(root) {
                self.selected = Some(index);
                return Ok(());
            }
        }
        let start = start.ok_or(StoreFault::Unknown)?;
        let index = self.vacancy(root)?;
        let mut slot = self.slots.get(index).copied().ok_or(StoreFault::Full)?;
        let header = BankHeader {
            run: RunId(root),
            align: self.layout.align(),
            workflow_kind: RUN_WORKFLOW_KIND,
            workflow_version: RUN_WORKFLOW_VERSION,
            input_schema: 0,
            input: start,
        };
        slot::commission(&mut slot, self.layout, &header).map_err(|error| slot_fault(&error))?;
        self.selected = Some(index);
        Ok(())
    }

    fn input(&self, out: &mut [u8]) -> Result<usize, StoreFault> {
        let index = self.selected.ok_or(StoreFault::Unknown)?;
        let mut slot = self.slots.get(index).copied().ok_or(StoreFault::Unknown)?;
        match slot::authority(&mut slot, self.layout, out) {
            Ok(Some(held)) => Ok(held.input_len),
            Ok(None) => Err(StoreFault::Unknown),
            Err(error) => Err(slot_fault(&error)),
        }
    }

    fn boot(&mut self, body: &mut dyn RunBody) -> Result<(), StoreFault> {
        let index = self.selected.ok_or(StoreFault::Unknown)?;
        let mut slot = self.slots.get(index).copied().ok_or(StoreFault::Unknown)?;
        let mut input = [0u8; HEADER_PAGE];
        let held = match slot::authority(&mut slot, self.layout, &mut input) {
            Ok(Some(held)) => held,
            Ok(None) => return Err(StoreFault::Unknown),
            Err(error) => return Err(slot_fault(&error)),
        };
        let input = input.get(..held.input_len).ok_or(StoreFault::Unknown)?;
        let mut workflow = BodyRun {
            body,
            input,
            out: [0; PAGE_RECORD_MAX],
        };
        let driver: Driver = Driver::at_bank(self.layout, self.reserve);
        let booted = driver.boot(
            &mut slot,
            &mut Bridged(&mut self.clocks),
            &mut workflow,
            Scratch {
                page: &mut self.page,
                result: &mut self.result,
            },
        );
        if let Err(error) = &booted {
            self.refused = Some(*error);
        }
        match booted {
            Ok(_) => Ok(()),
            Err(error) if refused_journal(&error) => {
                // The segment starts again from its own input; the
                // answer stays outstanding until it is driven again.
                slot::reinstall(&mut slot, self.layout).map_err(|error| slot_fault(&error))?;
                self.reinstalled = self.reinstalled.saturating_add(1);
                Err(StoreFault::Halted)
            }
            Err(error) => Err(store_fault(&error)),
        }
    }

    fn terminal(&self, out: &mut [u8]) -> Option<(bool, usize)> {
        let index = self.selected?;
        let mut slot = self.slots.get(index).copied()?;
        slot::terminal(&mut slot, self.layout, out).ok().flatten()
    }

    fn conclude(&mut self, completed: bool, record: &[u8]) -> Result<(), StoreFault> {
        // The driver writes the terminal record when the run returns its
        // ending; this only confirms it is on media.
        let mut out = [0u8; PAGE_RECORD_MAX];
        match self.terminal(&mut out) {
            Some((was, n)) if was == completed && out.get(..n) == Some(record) => Ok(()),
            Some(_) => Err(StoreFault::Unknown),
            None => Err(StoreFault::Halted),
        }
    }
}
