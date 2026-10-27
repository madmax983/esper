//! The device's half of the supervisor: the board dispatcher (SPEC §23).
//!
//! [`Device`] serves every activity [`super::supervise`] schedules —
//! [`SENSE`], [`RUN`], [`HARVEST`], [`STORE`], [`LOAD`] — and is product
//! code: `no_std`, allocation-free, compiled into the firmware. A board
//! supplies two things:
//!
//! * a [`RunStore`]: one Waymaker journal per run. On the board each is
//!   flash-backed: `waymaker_drive::Driver` owns the boot, and lends the
//!   run's journal as `waymaker_bridge::Bridge` over its boundary. The
//!   dispatcher sees only the [`Journal`] trait, through [`RunBody`] —
//!   the store calls the run, not the other way round, because a flash
//!   driver owns its boot loop.
//! * [`DevicePorts`]: the run's ports ([`RunPorts`]: model, pins, clock,
//!   operator input, snapshot store) plus the supervisor's (sensing and
//!   the checkpoint item store).
//!
//! # `RUN` is a nested workflow
//!
//! A `RUN` dispatch drives [`crate::run::run`] over the selected run's own
//! journal, inside the supervisor's activity. The run's future is built
//! fresh on every poll and replays its journal — Waymaker's futures are
//! disposable by design — so a redelivered `RUN` (the supervisor's
//! intent survived a reset, its answer did not) re-drives the *same* run
//! from its own history and never starts a second one. A segment that
//! continues as new is followed into the next; the run's answer is
//! recorded only once the run concludes or parks:
//!
//! | The run… | The `RUN` answer |
//! |---|---|
//! | concludes | `Settled`: its status, unspent turns and mutations, harvest count; the terminal record is written (or, if already written, must match) |
//! | waits for operator input | `Parked` |
//! | halted (its journal stopped, or the world is still working) | nothing: `Poll::Pending`, the answer stays outstanding |
//! | faults (a recorded fault replay would hit again) | failed: the supervisor settles it as a storage fault |
//!
//! `HARVEST` replays the concluded run to its end and reads the
//! workflow's own harvest (SPEC §21.3), so it needs nothing but the
//! run's journal.

use core::convert::Infallible;
use core::pin::pin;
use core::task::{Context, Poll};

use esper_core::beat::Beat;
use esper_core::goal::GoalId;
use esper_core::harvest::HarvestSet;
use esper_core::ids::RunId;
use waymaker_core::timer::{ClockKind, TimerSpec};
use waymaker_core::{ActivityKind, EffectId, Outcome};
use waymaker_embassy::ctx::Conclusion;
use waymaker_embassy::dispatch::Produced;
use waymaker_embassy::{ActivityDispatcher, Ctx, Journal};

use super::goal_of;
use super::wire::{
    HARVEST, HarvestEntry, LOAD, PAGE_RECORD_MAX, RUN, RunReport, RunRequest, SENSE, STORE, Sense,
    WireError, read_harvest_query, read_item_key,
};
use crate::error::RuntimeError;
use crate::run::device::{RunDispatcher, RunPorts};
use crate::run::wire::{MAX_RUN_START_BYTES, RunStart, TerminalRecord};
use crate::run::{RunState, run};

/// Segments one `RUN` dispatch follows before it gives up: a run that
/// keeps continuing as new without concluding is a fault.
pub const MAX_SEGMENTS: usize = 64;

/// Why the run store could not serve a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum StoreFault {
    /// There is no journal for this run and nothing to create it from.
    #[error("no journal for this run")]
    Unknown,
    /// The store is full.
    #[error("the run store is full")]
    Full,
    /// The media stopped this boot (a reset, a refused write): the
    /// answer stays outstanding.
    #[error("the run store halted")]
    Halted,
}

/// How one boot of the nested run ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodyEnd {
    /// The run ended: completed (else failed), its terminal record the
    /// first `len` bytes of the store's `out`.
    Ended {
        /// Whether the run completed.
        completed: bool,
        /// The terminal record's length.
        len: usize,
    },
    /// The run faulted, and a replay would fault again: it is over, with
    /// no terminal record of its own. A store whose driver records
    /// endings records it failed, with no payload.
    Faulted,
    /// The run stopped at a boundary: a wait, an answer still owed, or a
    /// halted journal. The next boot carries on.
    Stopped,
}

/// The nested run's half of one boot of its journal.
///
/// [`RunStore::boot`] calls it with the run's journal lent for exactly
/// that boot. Object-safe, so a store needs no type parameter for the
/// run it drives.
pub trait RunBody {
    /// Drive the run once over `journal`; an ending's terminal record is
    /// written to `out`. The store records what [`BodyEnd`] says (a flash
    /// driver writes it as the run's terminal record).
    fn drive(&mut self, journal: &mut dyn Journal, out: &mut [u8]) -> BodyEnd;
}

/// A lent `&mut dyn Journal`, as a sized [`Journal`] a [`Ctx`] can hold.
pub struct Lent<'j>(pub &'j mut dyn Journal);

impl Journal for Lent<'_> {
    fn schedule(
        &mut self,
        kind: ActivityKind,
        input: &[u8],
    ) -> Result<waymaker_embassy::Handoff<'_>, waymaker_embassy::Halted> {
        self.0.schedule(kind, input)
    }

    fn resolve(
        &mut self,
        answer: waymaker_embassy::Answer<'_>,
    ) -> Result<Outcome<'_>, waymaker_embassy::Halted> {
        self.0.resolve(answer)
    }

    fn wait(&mut self, spec: TimerSpec) -> Result<(), waymaker_embassy::Halted> {
        self.0.wait(spec)
    }

    fn continue_as_new(&mut self, input: &[u8]) -> waymaker_embassy::Halted {
        self.0.continue_as_new(input)
    }

    fn deadline_remaining(&self) -> Option<(ClockKind, u64)> {
        self.0.deadline_remaining()
    }
}

/// Where the device keeps its runs' Waymaker journals.
///
/// On the board each run's journal is flash-backed; Waymaker's
/// `continue_as_new` retires a segment and installs the next one's input
/// in the same journal.
pub trait RunStore {
    /// Select `run`. The first select of a run creates its journal over
    /// `start` (its encoded [`RunStart`]); a later select finds it,
    /// whatever `start` says.
    ///
    /// # Errors
    ///
    /// [`StoreFault::Unknown`] for an unknown run with no `start`;
    /// [`StoreFault::Full`] or [`StoreFault::Halted`] from the media.
    fn select(&mut self, run: RunId, start: Option<&[u8]>) -> Result<(), StoreFault>;

    /// The selected run's current input — its `RunStart`, or what its
    /// last `continue_as_new` installed — copied into `out`.
    ///
    /// # Errors
    ///
    /// [`StoreFault::Unknown`] when nothing is selected or `out` is short.
    fn input(&self, out: &mut [u8]) -> Result<usize, StoreFault>;

    /// Boot the selected run's journal once — a fresh replay from its
    /// first record — and lend it to `body` for that boot. A store whose
    /// driver records terminal records itself records the one `body`
    /// returns.
    ///
    /// # Errors
    ///
    /// [`StoreFault::Halted`] when the media stopped this boot (the
    /// answer stays outstanding); [`StoreFault::Unknown`] when nothing is
    /// selected or the journal refused the run.
    fn boot(&mut self, body: &mut dyn RunBody) -> Result<(), StoreFault>;

    /// The selected run's terminal record, if one was written:
    /// completed (else failed) and its length in `out`.
    fn terminal(&self, out: &mut [u8]) -> Option<(bool, usize)>;

    /// Write the selected run's terminal record.
    ///
    /// # Errors
    ///
    /// [`StoreFault::Halted`] when the media stopped.
    fn conclude(&mut self, completed: bool, record: &[u8]) -> Result<(), StoreFault>;
}

/// The device ports the supervisor's activities reach.
pub trait DevicePorts: RunPorts {
    /// What the world says at `beat`: the persistent-clock reading, the
    /// goals whose operator input arrived, and the operator's commands —
    /// already masked ([`super::wire::Command`]'s constructors mask).
    ///
    /// # Errors
    ///
    /// [`WireError`] when the answer does not fit.
    fn sense(&mut self, beat: Beat) -> Result<Sense, WireError>;

    /// Route operator input to `goal`'s inbox while its run is driven.
    fn select_goal(&mut self, goal: GoalId);

    /// Store one checkpoint item.
    ///
    /// # Errors
    ///
    /// [`StoreFault`] when the store refuses.
    fn store_item(&mut self, digest: u64, index: u16, item: &[u8]) -> Result<(), StoreFault>;

    /// Load one checkpoint item into `out`.
    ///
    /// # Errors
    ///
    /// [`StoreFault::Unknown`] when there is none.
    fn load_item(&mut self, digest: u64, index: u16, out: &mut [u8]) -> Result<usize, StoreFault>;
}

/// How far a drive of the selected run got.
enum Driven {
    /// Waiting for operator input.
    Parked,
    /// Concluded; the terminal record is in [`Device::terminal`].
    Concluded,
}

/// One boot of one segment of the nested run: what [`Device::drive`]
/// lends the store, and what the boot left behind.
struct Segment<'s, 't> {
    ports: &'s mut dyn RunPorts,
    state: &'s mut RunState,
    terminal: &'s mut [u8; PAGE_RECORD_MAX],
    input: &'s [u8],
    run: RunId,
    task: &'s mut Context<'t>,
    /// The run ended this boot: completed (else failed), and its
    /// terminal record's length in `terminal`.
    ended: Option<(bool, usize)>,
    /// The run faulted.
    error: Option<RuntimeError>,
    /// The run is waiting on an answer the world cannot give yet.
    unserviceable: bool,
}

impl RunBody for Segment<'_, '_> {
    fn drive(&mut self, journal: &mut dyn Journal, out: &mut [u8]) -> BodyEnd {
        let mut journal = Lent(journal);
        let mut page = [0u8; PAGE_RECORD_MAX];
        let mut dispatcher = RunDispatcher::new(&mut *self.ports, self.run);
        let mut ctx = Ctx::new(&mut journal, &mut dispatcher, &mut page);
        let polled = {
            let mut workflow = pin!(run(&mut ctx, self.input, &mut *self.state));
            workflow.as_mut().poll(self.task)
        };
        self.unserviceable = ctx.unserviceable();
        if let Poll::Ready(Err(error)) = polled {
            self.error = Some(error);
            return BodyEnd::Faulted;
        }
        let (completed, bytes) = match ctx.conclusion() {
            None => return BodyEnd::Stopped,
            Some(Conclusion::Ended(Outcome::Completed(bytes))) => (true, bytes),
            Some(Conclusion::Ended(Outcome::Failed(bytes))) => (false, bytes),
            Some(Conclusion::Refused) => {
                self.error = Some(RuntimeError::BadTerminal);
                return BodyEnd::Faulted;
            }
        };
        let (Some(kept), Some(lent)) = (
            self.terminal.get_mut(..bytes.len()),
            out.get_mut(..bytes.len()),
        ) else {
            self.error = Some(RuntimeError::BadTerminal);
            return BodyEnd::Faulted;
        };
        kept.copy_from_slice(bytes);
        lent.copy_from_slice(bytes);
        self.ended = Some((completed, bytes.len()));
        BodyEnd::Ended {
            completed,
            len: bytes.len(),
        }
    }
}

/// The board dispatcher: the supervisor's activities over a board's
/// [`RunStore`] and [`DevicePorts`]. Build one per boot (or per poll);
/// it holds nothing that must survive a reset.
pub struct Device<'a, S: RunStore, P: DevicePorts> {
    runs: &'a mut S,
    ports: &'a mut P,
    /// The caller-owned state the nested run replays into (a `static`
    /// on the device).
    state: &'a mut RunState,
    error: Option<RuntimeError>,
    /// The concluded run's terminal record and its length.
    terminal: [u8; PAGE_RECORD_MAX],
    terminal_len: usize,
}

impl<'a, S: RunStore, P: DevicePorts> Device<'a, S, P> {
    /// A dispatcher over the board's runs and ports.
    pub const fn new(runs: &'a mut S, ports: &'a mut P, state: &'a mut RunState) -> Self {
        Self {
            runs,
            ports,
            state,
            error: None,
            terminal: [0; PAGE_RECORD_MAX],
            terminal_len: 0,
        }
    }

    /// Why the last activity failed, for the board's log. The failure
    /// itself is recorded without a reason: the supervisor never
    /// branches on it.
    #[must_use]
    pub const fn last_error(&self) -> Option<&RuntimeError> {
        self.error.as_ref()
    }

    const fn failed(&mut self, error: RuntimeError) -> Produced {
        self.error = Some(error);
        Produced::Failed(0)
    }

    /// Drive the selected run until it concludes or parks, following
    /// `continue_as_new` from segment to segment.
    fn drive(&mut self, task: &mut Context<'_>) -> Poll<Result<Driven, RuntimeError>> {
        for _ in 0..MAX_SEGMENTS {
            let mut input = [0u8; MAX_RUN_START_BYTES];
            let n = match self.runs.input(&mut input) {
                Ok(n) => n,
                Err(StoreFault::Halted) => return Poll::Pending,
                Err(_) => return Poll::Ready(Err(RuntimeError::JournalCorrupt)),
            };
            let Some(input) = input.get(..n) else {
                return Poll::Ready(Err(RuntimeError::JournalCorrupt));
            };
            let start = match RunStart::decode(input) {
                Ok(start) => start,
                Err(error) => return Poll::Ready(Err(error.into())),
            };
            let mut segment = Segment {
                ports: &mut *self.ports,
                state: &mut *self.state,
                terminal: &mut self.terminal,
                input,
                run: start.seed.id,
                task: &mut *task,
                ended: None,
                error: None,
                unserviceable: false,
            };
            match self.runs.boot(&mut segment) {
                Ok(()) => {}
                Err(StoreFault::Halted) => return Poll::Pending,
                Err(_) => return Poll::Ready(Err(RuntimeError::JournalCorrupt)),
            }
            let Segment {
                ended,
                error,
                unserviceable,
                ..
            } = segment;
            if let Some(error) = error {
                return Poll::Ready(Err(error));
            }
            if let Some((completed, len)) = ended {
                self.terminal_len = len;
                return self.settle(completed);
            }
            if unserviceable {
                return Poll::Ready(Ok(Driven::Parked));
            }
            // Did the segment continue as new? Then drive the next one.
            let mut after = [0u8; MAX_RUN_START_BYTES];
            match self.runs.input(&mut after) {
                Ok(m) if after.get(..m) != Some(input) => {}
                Ok(_) | Err(StoreFault::Halted) => return Poll::Pending,
                Err(_) => return Poll::Ready(Err(RuntimeError::JournalCorrupt)),
            }
        }
        Poll::Ready(Err(RuntimeError::TooManyBoots))
    }

    /// Record (or confirm) the run's terminal record.
    fn settle(&mut self, completed: bool) -> Poll<Result<Driven, RuntimeError>> {
        let Some(bytes) = self.terminal.get(..self.terminal_len) else {
            return Poll::Ready(Err(RuntimeError::BadTerminal));
        };
        let mut prior = [0u8; PAGE_RECORD_MAX];
        match self.runs.terminal(&mut prior) {
            // Replay reached the recorded ending again: it must be the
            // same one.
            Some((was, n)) => {
                if was != completed || prior.get(..n) != Some(bytes) {
                    return Poll::Ready(Err(RuntimeError::ReplayDiverged));
                }
            }
            None => match self.runs.conclude(completed, bytes) {
                Ok(()) => {}
                Err(StoreFault::Halted) => return Poll::Pending,
                Err(_) => return Poll::Ready(Err(RuntimeError::JournalCorrupt)),
            },
        }
        Poll::Ready(Ok(Driven::Concluded))
    }

    /// The concluded run's report and harvest.
    fn report(&self, goal: GoalId) -> Result<(RunReport, HarvestSet), RuntimeError> {
        let record = self
            .terminal
            .get(..self.terminal_len)
            .ok_or(RuntimeError::BadTerminal)?;
        let terminal = TerminalRecord::decode(record)?;
        let harvest = self
            .state
            .harvest(goal, terminal.status, terminal.summary)?;
        let budget = self.state.budget();
        Ok((
            RunReport::Settled {
                status: terminal.status,
                refund_turns: budget.model_turns,
                refund_mutations: budget.mutations,
                harvest: u8::try_from(harvest.len()).unwrap_or(u8::MAX),
            },
            harvest,
        ))
    }

    fn run_activity(
        &mut self,
        task: &mut Context<'_>,
        input: &[u8],
        out: &mut [u8],
    ) -> Poll<Produced> {
        let request = match RunRequest::decode(input) {
            Ok(request) => request,
            Err(error) => return Poll::Ready(self.failed(error.into())),
        };
        let mut start = [0u8; MAX_RUN_START_BYTES];
        let start = match request {
            RunRequest::Spawn { seed, .. } => {
                match (RunStart {
                    seed,
                    snapshot: None,
                })
                .encode(&mut start)
                {
                    Ok(n) => start.get(..n),
                    Err(error) => return Poll::Ready(self.failed(error.into())),
                }
            }
            RunRequest::Resume { .. } => None,
        };
        match self.runs.select(request.run(), start) {
            Ok(()) => {}
            Err(StoreFault::Halted) => return Poll::Pending,
            Err(fault) => return Poll::Ready(self.failed(store_error(fault))),
        }
        self.ports.select_goal(request.goal());
        let report = match self.drive(task) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => return Poll::Ready(self.failed(error)),
            Poll::Ready(Ok(Driven::Parked)) => RunReport::Parked,
            Poll::Ready(Ok(Driven::Concluded)) => match self.report(request.goal()) {
                Ok((report, _)) => report,
                Err(error) => return Poll::Ready(self.failed(error)),
            },
        };
        Poll::Ready(match report.encode(out) {
            Ok(len) => Produced::Completed(len),
            Err(error) => self.failed(error.into()),
        })
    }

    fn harvest_activity(
        &mut self,
        task: &mut Context<'_>,
        input: &[u8],
        out: &mut [u8],
    ) -> Poll<Produced> {
        let (run, index) = match read_harvest_query(input) {
            Ok(query) => query,
            Err(error) => return Poll::Ready(self.failed(error.into())),
        };
        let Some(goal) = goal_of(run) else {
            return Poll::Ready(self.failed(RuntimeError::JournalCorrupt));
        };
        match self.runs.select(run, None) {
            Ok(()) => {}
            Err(StoreFault::Halted) => return Poll::Pending,
            Err(fault) => return Poll::Ready(self.failed(store_error(fault))),
        }
        let harvest = match self.drive(task) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(error)) => return Poll::Ready(self.failed(error)),
            // Only a concluded run has a harvest.
            Poll::Ready(Ok(Driven::Parked)) => {
                return Poll::Ready(self.failed(RuntimeError::IllegalTransition));
            }
            Poll::Ready(Ok(Driven::Concluded)) => match self.report(goal) {
                Ok((_, harvest)) => harvest,
                Err(error) => return Poll::Ready(self.failed(error)),
            },
        };
        let Some(entry) = harvest.get(usize::from(index)) else {
            return Poll::Ready(self.failed(RuntimeError::World("no such harvest entry")));
        };
        Poll::Ready(match HarvestEntry(*entry).encode(out) {
            Ok(len) => Produced::Completed(len),
            Err(error) => self.failed(error.into()),
        })
    }

    fn sense(&mut self, input: &[u8], out: &mut [u8]) -> Produced {
        let Ok(beat) = <[u8; 8]>::try_from(input) else {
            return self.failed(RuntimeError::BadFrame);
        };
        match self
            .ports
            .sense(Beat::new(u64::from_le_bytes(beat)))
            .and_then(|sense| sense.encode(out))
        {
            Ok(len) => Produced::Completed(len),
            Err(error) => self.failed(error.into()),
        }
    }

    fn item(&mut self, kind: ActivityKind, input: &[u8], out: &mut [u8]) -> Produced {
        let (digest, index, item) = match read_item_key(input) {
            Ok(key) => key,
            Err(error) => return self.failed(error.into()),
        };
        let stored = if kind == STORE {
            self.ports.store_item(digest, index, item).map(|()| 0)
        } else {
            self.ports.load_item(digest, index, out)
        };
        match stored {
            Ok(len) => Produced::Completed(len),
            Err(fault) => self.failed(store_error(fault)),
        }
    }
}

/// A store refusal as the runtime's error.
const fn store_error(fault: StoreFault) -> RuntimeError {
    match fault {
        StoreFault::Unknown => RuntimeError::World("unknown run or item"),
        StoreFault::Full => RuntimeError::World("store full"),
        StoreFault::Halted => RuntimeError::World("store halted"),
    }
}

impl<S: RunStore, P: DevicePorts> ActivityDispatcher for Device<'_, S, P> {
    type Error = Infallible;

    fn poll_dispatch(
        &mut self,
        task: &mut Context<'_>,
        _id: EffectId,
        kind: ActivityKind,
        input: &[u8],
        out: &mut [u8],
    ) -> Poll<Result<Produced, Infallible>> {
        self.error = None;
        let produced = match kind {
            SENSE => Poll::Ready(self.sense(input, out)),
            RUN => self.run_activity(task, input, out),
            HARVEST => self.harvest_activity(task, input, out),
            STORE | LOAD => Poll::Ready(self.item(kind, input, out)),
            _ => Poll::Ready(Produced::Unserviceable),
        };
        produced.map(Ok)
    }
}
