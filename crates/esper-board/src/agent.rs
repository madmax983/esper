//! [`Supervisor`]: the always-on supervisor, booted on its own bank pair.
//!
//! One [`Supervisor::boot`] is one boot of [`supervise`] on flash:
//! `waymaker_drive::Driver::at_bank` replays the supervisor's journal,
//! [`waymaker_bridge::Bridge`] lends it to the workflow's `Ctx`, and the
//! product dispatcher [`Device`] serves every activity — `RUN` drives the
//! nested run through [`FlashRuns`](crate::runs::FlashRuns) on its own
//! slot. The boot ends where the workflow stops: at a beat's durable
//! timer (`Progress::WaitingUntil`, which the board sleeps out), at an
//! answer the world has not given yet (`Progress::Waiting`), or at a
//! `continue_as_new` (`Progress::Migrated`, which the next boot follows).
//! A journal Waymaker refuses is reinstalled from its segment's input
//! ([`Booted::Reinstalled`]) rather than left to stop the board.
//!
//! The first boot of an erased pair commissions it with the seed's
//! [`Start`](esper_runtime::agent::Start); every later boot reads the run
//! input from the authoritative bank's header, so a checkpoint's
//! `continue_as_new` input is what the next segment starts from.

use core::pin::pin;
use core::task::{Context, Poll, Waker};

use embedded_storage::nor_flash::NorFlash;
use esper_runtime::agent::device::{Device, DevicePorts, RunStore};
use esper_runtime::agent::wire::PAGE_RECORD_MAX;
use esper_runtime::agent::{
    AgentError, AgentSeed, AgentState, MAX_START_BYTES, PulseSink, supervise,
};
use esper_runtime::run::RunState;
use waymaker_bridge::Bridge;
use waymaker_core::version::VersionRange;
use waymaker_core::{Outcome, RunId};
use waymaker_drive::{
    Boundary, Clocks, DriveError, Driver, Identity, Progress, Scratch, Suspended, Workflow,
};
use waymaker_embassy::{Ctx, NoAlarm};
use waymaker_flash::bank::{BankHeader, BankLayout};
use waymaker_flash::capacity::Reserve;

use crate::flash::{Partition, PartitionError};
use crate::runs::{Bridged, DRIVER_PAGE, refused_journal};
use crate::slot::{self, HEADER_PAGE, SlotFault};

/// The workflow kind the supervisor's header records.
pub const SUPERVISOR_WORKFLOW_KIND: u16 = 0x0E00;

/// The workflow version the supervisor is driven at.
pub const SUPERVISOR_WORKFLOW_VERSION: u16 = 1;

/// What one supervisor boot did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Booted {
    /// The driver ran the supervisor to where it stopped.
    Ran(Progress),
    /// Waymaker refused the supervisor's journal, and the segment was
    /// reinstalled from its own input ([`slot::reinstall`]): boot again.
    Reinstalled(RunId),
}

/// Why a supervisor boot could not go on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BoardFault {
    /// The supervisor's bank pair could not be read or commissioned.
    #[error("the supervisor's slot refused")]
    Slot(SlotFault<PartitionError>),
    /// The driver refused the boot.
    #[error("the driver refused the boot")]
    Drive(DriveError<PartitionError>),
    /// The supervisor workflow faulted; a replay would fault again.
    #[error("the supervisor faulted")]
    Agent(AgentError),
}

/// The supervisor workflow, as one driver boot sees it.
struct Supervising<'b, R: RunStore, P: DevicePorts> {
    seed: &'b AgentSeed,
    input: &'b [u8],
    runs: &'b mut R,
    ports: &'b mut P,
    run_state: &'b mut RunState,
    state: &'b mut AgentState,
    sink: &'b mut dyn PulseSink,
    error: Option<AgentError>,
}

impl<R: RunStore, P: DevicePorts> Workflow for Supervising<'_, R, P> {
    fn identity(&self) -> Identity<'_> {
        Identity {
            kind: SUPERVISOR_WORKFLOW_KIND,
            versions: VersionRange::exact(SUPERVISOR_WORKFLOW_VERSION),
            input: self.input,
        }
    }

    fn run(&mut self, boundary: &mut dyn Boundary) -> Result<Outcome<'_>, Suspended> {
        let suspended = {
            let mut bridge = Bridge::over(boundary);
            let mut device = Device::new(&mut *self.runs, &mut *self.ports, &mut *self.run_state);
            let mut out = [0u8; PAGE_RECORD_MAX];
            let polled = {
                let mut ctx = Ctx::new(&mut bridge, &mut device, &mut out);
                let mut alarm = NoAlarm;
                let mut workflow = pin!(supervise(
                    &mut ctx,
                    self.seed,
                    self.input,
                    &mut alarm,
                    &mut *self.state,
                    &mut *self.sink,
                ));
                workflow
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
            };
            if let Poll::Ready(Err(error)) = polled {
                self.error = Some(error);
            }
            bridge.take_suspended()
        };
        // The supervisor never ends: every boot stops at a boundary.
        Err(suspended.unwrap_or_else(Suspended::awaiting_dispatch))
    }
}

/// The supervisor's bank pair and the driver scratch it boots with.
pub struct Supervisor<'f, F: NorFlash, C: Clocks> {
    slot: Partition<'f, F>,
    layout: BankLayout,
    reserve: Reserve,
    clocks: C,
    page: [u8; DRIVER_PAGE],
    result: [u8; PAGE_RECORD_MAX],
}

impl<'f, F: NorFlash, C: Clocks> Supervisor<'f, F, C> {
    /// The supervisor on `slot`, a bank pair shaped `layout` priced by
    /// `reserve`, with the board's `clocks`.
    pub const fn new(
        slot: Partition<'f, F>,
        layout: BankLayout,
        reserve: Reserve,
        clocks: C,
    ) -> Self {
        Self {
            slot,
            layout,
            reserve,
            clocks,
            page: [0; DRIVER_PAGE],
            result: [0; PAGE_RECORD_MAX],
        }
    }

    /// The board's clocks, for the loop that sleeps out a beat.
    pub const fn clocks(&mut self) -> &mut C {
        &mut self.clocks
    }

    /// Commission the pair for `seed` if no run was ever installed, and
    /// copy the current run input into `input`.
    fn input(&mut self, seed: &AgentSeed, input: &mut [u8]) -> Result<usize, BoardFault> {
        if let Some(held) =
            slot::authority(&mut self.slot, self.layout, input).map_err(BoardFault::Slot)?
        {
            return Ok(held.input_len);
        }
        let mut start = [0u8; MAX_START_BYTES];
        let n = seed.start().encode(&mut start).map_err(BoardFault::Agent)?;
        let start = start.get(..n).ok_or(BoardFault::Slot(SlotFault::Layout))?;
        let header = BankHeader {
            run: RunId(seed.id << 16),
            align: self.layout.align(),
            workflow_kind: SUPERVISOR_WORKFLOW_KIND,
            workflow_version: SUPERVISOR_WORKFLOW_VERSION,
            input_schema: 0,
            input: start,
        };
        slot::commission(&mut self.slot, self.layout, &header).map_err(BoardFault::Slot)?;
        input
            .get_mut(..n)
            .ok_or(BoardFault::Slot(SlotFault::Layout))?
            .copy_from_slice(start);
        Ok(n)
    }

    /// One boot of the supervisor.
    ///
    /// # Errors
    ///
    /// [`BoardFault`] when the slot or the driver refused, or the
    /// supervisor faulted.
    pub fn boot<R: RunStore, P: DevicePorts>(
        &mut self,
        seed: &AgentSeed,
        runs: &mut R,
        ports: &mut P,
        run_state: &mut RunState,
        state: &mut AgentState,
        sink: &mut dyn PulseSink,
    ) -> Result<Booted, BoardFault> {
        let mut input = [0u8; HEADER_PAGE];
        let n = self.input(seed, &mut input)?;
        let input = input.get(..n).ok_or(BoardFault::Slot(SlotFault::Layout))?;
        let mut workflow = Supervising {
            seed,
            input,
            runs,
            ports,
            run_state,
            state,
            sink,
            error: None,
        };
        let driver: Driver = Driver::at_bank(self.layout, self.reserve);
        let progress = driver.boot(
            &mut self.slot,
            &mut Bridged(&mut self.clocks),
            &mut workflow,
            Scratch {
                page: &mut self.page,
                result: &mut self.result,
            },
        );
        if let Some(error) = workflow.error {
            return Err(BoardFault::Agent(error));
        }
        match progress {
            Ok(progress) => Ok(Booted::Ran(progress)),
            Err(error) if refused_journal(&error) => slot::reinstall(&mut self.slot, self.layout)
                .map(Booted::Reinstalled)
                .map_err(BoardFault::Slot),
            Err(error) => Err(BoardFault::Drive(error)),
        }
    }
}
