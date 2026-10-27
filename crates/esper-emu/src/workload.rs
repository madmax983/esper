//! The run and the supervisor on the core: a fixed-capacity Waymaker
//! journal, an 8-pin device and a scripted model, all `no_std`, behind
//! Esper's product dispatchers.
//!
//! The journal, the pins and the model are this image's doubles for the
//! board's flash, GPIO and model. Everything they are plugged into is
//! product code: the run workflow ([`esper_runtime::run::run`]), the
//! supervisor ([`esper_runtime::supervise`]), the run activities'
//! dispatcher ([`RunDispatcher`]) and the board dispatcher ([`Device`]).

use core::pin::pin;
use core::task::{Context, Poll, Waker};

use esper_core::beat::Beat;
use esper_core::decision::{Level, ToolArgs};
use esper_core::goal::{Cadence, Envelope, GoalId};
use esper_core::ids::{Pin, RunId, ToolId};
use esper_runtime::agent::device::{Device, DevicePorts, RunStore, StoreFault};
use esper_runtime::agent::wire::{Command, PAGE_RECORD_MAX, Sense, WireError};
use esper_runtime::agent::{AgentSeed, AgentState, MAX_START_BYTES, Pulse, PulseSink, supervise};
use esper_runtime::run::device::{Attempt, RunDispatcher, RunPorts};
use esper_runtime::run::wire::{self, Check, Fault, MAX_RUN_START_BYTES, RunStart, ToolRequest};
use esper_runtime::run::{RunState, run};
use esper_runtime::{BackendError, BundleId, ModelBackend, RunSeed, TokenUsage};
use waymaker_core::timer::{ClockKind, TimerSpec};
use waymaker_core::{ActivityKind, EffectId, EffectSeq, Outcome, RunId as WaymakerRunId};
use waymaker_embassy::ctx::Conclusion;
use waymaker_embassy::{Answer, Ctx, Halted, Handoff, Journal, NoAlarm};

/// Records one journal holds: ample for a short run or a few beats.
const MAX_RECORDS: usize = 32;

/// What one record slot holds.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SlotKind {
    Scheduled,
    Resolved,
    Timer,
}

/// One record slot.
#[derive(Clone, Copy)]
struct Slot {
    kind: SlotKind,
    activity: u16,
    seq: u32,
    completed: bool,
    len: usize,
    bytes: [u8; PAGE_RECORD_MAX],
}

impl Slot {
    const EMPTY: Self = Self {
        kind: SlotKind::Timer,
        activity: 0,
        seq: 0,
        completed: false,
        len: 0,
        bytes: [0; PAGE_RECORD_MAX],
    };

    fn bytes(&self) -> &[u8] {
        self.bytes.get(..self.len).unwrap_or(&[])
    }
}

/// A fixed-capacity in-RAM Waymaker journal that survives the image's
/// simulated resets. It can arm one reset — after a mutating tool acts
/// and before its answer commits — and it lets a fixed number of
/// heartbeat deadlines pass, then stops the boot.
pub struct EmuJournal {
    slots: [Slot; MAX_RECORDS],
    len: usize,
    pos: usize,
    run: u64,
    input: [u8; PAGE_RECORD_MAX],
    input_len: usize,
    deadlines: u32,
    armed: bool,
    /// The armed reset fired this boot.
    pub crashed: bool,
}

impl EmuJournal {
    const fn new(run: u64, deadlines: u32, armed: bool) -> Self {
        Self {
            slots: [Slot::EMPTY; MAX_RECORDS],
            len: 0,
            pos: 0,
            run,
            input: [0; PAGE_RECORD_MAX],
            input_len: 0,
            deadlines,
            armed,
            crashed: false,
        }
    }

    const fn boot(&mut self) {
        self.pos = 0;
        self.crashed = false;
    }

    fn push(&mut self, slot: &Slot) -> Result<(), Halted> {
        let dst = self.slots.get_mut(self.len).ok_or(Halted)?;
        *dst = *slot;
        self.len += 1;
        self.pos += 1;
        Ok(())
    }

    fn last_scheduled(&self) -> Option<&Slot> {
        self.slots
            .get(..self.len)?
            .iter()
            .rev()
            .find(|slot| slot.kind == SlotKind::Scheduled)
    }

    const fn id(&self, seq: u32) -> EffectId {
        EffectId {
            run: WaymakerRunId(self.run),
            seq: EffectSeq(seq),
        }
    }
}

impl Journal for EmuJournal {
    fn schedule(&mut self, kind: ActivityKind, input: &[u8]) -> Result<Handoff<'_>, Halted> {
        if self.pos < self.len {
            let slot = self.slots.get(self.pos).ok_or(Halted)?;
            if slot.kind != SlotKind::Scheduled || slot.activity != kind.0 || slot.bytes() != input
            {
                return Err(Halted);
            }
            let seq = slot.seq;
            self.pos += 1;
            if self.pos == self.len {
                // An intent with no outcome: redeliver under its id.
                return Ok(Handoff::Dispatch {
                    id: self.id(seq),
                    result_bytes: PAGE_RECORD_MAX,
                });
            }
            let at = self.pos;
            self.pos += 1;
            let slot = self.slots.get(at).ok_or(Halted)?;
            if slot.kind != SlotKind::Resolved {
                return Err(Halted);
            }
            let bytes = slot.bytes.get(..slot.len).ok_or(Halted)?;
            return Ok(Handoff::Replayed(if slot.completed {
                Outcome::Completed(bytes)
            } else {
                Outcome::Failed(bytes)
            }));
        }
        let seq = u32::try_from(self.len + 1).map_err(|_| Halted)?;
        let mut slot = Slot::EMPTY;
        slot.kind = SlotKind::Scheduled;
        slot.activity = kind.0;
        slot.seq = seq;
        slot.len = input.len();
        slot.bytes
            .get_mut(..input.len())
            .ok_or(Halted)?
            .copy_from_slice(input);
        self.push(&slot)?;
        Ok(Handoff::Dispatch {
            id: self.id(seq),
            result_bytes: PAGE_RECORD_MAX,
        })
    }

    fn resolve(&mut self, answer: Answer<'_>) -> Result<Outcome<'_>, Halted> {
        if self.armed {
            let write = self.last_scheduled().is_some_and(|slot| {
                ActivityKind(slot.activity) == wire::TOOL
                    && ToolRequest::decode(slot.bytes()).is_ok_and(|request| request.write)
            });
            if write {
                // The device acted; its answer is lost with the reset.
                self.armed = false;
                self.crashed = true;
                return Err(Halted);
            }
        }
        let (completed, bytes) = match answer {
            Answer::Completed(bytes) => (true, bytes),
            Answer::Failed(bytes) => (false, bytes),
            Answer::Exhausted => (false, &[][..]),
        };
        let mut slot = Slot::EMPTY;
        slot.kind = SlotKind::Resolved;
        slot.completed = completed;
        slot.len = bytes.len();
        slot.bytes
            .get_mut(..bytes.len())
            .ok_or(Halted)?
            .copy_from_slice(bytes);
        self.push(&slot)?;
        let slot = self.slots.get(self.len - 1).ok_or(Halted)?;
        let bytes = slot.bytes.get(..slot.len).ok_or(Halted)?;
        Ok(if slot.completed {
            Outcome::Completed(bytes)
        } else {
            Outcome::Failed(bytes)
        })
    }

    fn wait(&mut self, _spec: TimerSpec) -> Result<(), Halted> {
        if self.pos < self.len {
            let slot = self.slots.get(self.pos).ok_or(Halted)?;
            if slot.kind != SlotKind::Timer {
                return Err(Halted);
            }
            self.pos += 1;
            return Ok(());
        }
        // The image runs a fixed number of beats, then stops the boot.
        if self.deadlines == 0 {
            return Err(Halted);
        }
        self.deadlines -= 1;
        self.push(&Slot::EMPTY)
    }

    fn continue_as_new(&mut self, input: &[u8]) -> Halted {
        if let Some(dst) = self.input.get_mut(..input.len()) {
            dst.copy_from_slice(input);
            self.input_len = input.len();
            self.len = 0;
            self.pos = 0;
        }
        Halted
    }

    fn deadline_remaining(&self) -> Option<(ClockKind, u64)> {
        None
    }
}

/// A model that answers with its script, one line per turn.
pub struct EmuModel {
    lines: &'static [&'static [u8]],
    next: usize,
}

impl ModelBackend for EmuModel {
    fn infer(&mut self, _prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        let line = self.lines.get(self.next).ok_or(BackendError::Exhausted)?;
        out.get_mut(..line.len())
            .ok_or(BackendError::OutputTooLong)?
            .copy_from_slice(line);
        self.next += 1;
        Ok(line.len())
    }

    fn bundle_id(&self) -> BundleId {
        BundleId(0)
    }

    fn last_usage(&self) -> TokenUsage {
        TokenUsage {
            input_tokens: 0,
            output_tokens: 0,
        }
    }

    fn unemit(&mut self) {
        self.next = self.next.saturating_sub(1);
    }
}

/// `{"pin":N,"level":"high"}` into `out`.
fn pin_json(pin: u8, high: bool, out: &mut [u8]) -> Option<usize> {
    let level: &[u8] = if high { b"high" } else { b"low" };
    let mut len = 0;
    for part in [
        &b"{\"pin\":"[..],
        &[b'0' + pin % 10],
        b",\"level\":\"",
        level,
        b"\"}",
    ] {
        out.get_mut(len..len + part.len())?.copy_from_slice(part);
        len += part.len();
    }
    Some(len)
}

/// The image's device ports: eight output pins with a write counter,
/// deduplicating a redelivered effect the way a board's actuator driver
/// must, a scripted model, and the operator's one command.
pub struct EmuPorts {
    model: EmuModel,
    levels: [bool; 8],
    /// Physical writes executed.
    pub writes: u32,
    seen: [Option<(u64, u32)>; 8],
    /// The operator command [`DevicePorts::sense`] delivers at beat 1.
    command: Option<Command>,
}

impl EmuPorts {
    const fn new(lines: &'static [&'static [u8]], command: Option<Command>) -> Self {
        Self {
            model: EmuModel { lines, next: 0 },
            levels: [false; 8],
            writes: 0,
            seen: [None; 8],
            command,
        }
    }

    /// A pin's level.
    #[must_use]
    pub fn level(&self, pin: u8) -> bool {
        self.levels.get(usize::from(pin)).copied().unwrap_or(false)
    }
}

impl RunPorts for EmuPorts {
    fn model(&mut self) -> &mut dyn ModelBackend {
        &mut self.model
    }

    fn tool(
        &mut self,
        effect: EffectId,
        _tool: ToolId,
        _digest: u64,
        args: ToolArgs,
        raw: &mut [u8],
    ) -> Result<Attempt, Fault> {
        let (pin, high) = match args {
            ToolArgs::GpioPinRead { pin } => (pin.get(), self.level(pin.get())),
            ToolArgs::GpioPinWrite { pin, level } => {
                let high = level == Level::High;
                let key = (effect.run.0, effect.seq.0);
                if !self.seen.contains(&Some(key)) {
                    if let Some(slot) = self.seen.iter_mut().find(|slot| slot.is_none()) {
                        *slot = Some(key);
                    }
                    if let Some(level) = self.levels.get_mut(usize::from(pin.get())) {
                        *level = high;
                        self.writes += 1;
                    }
                }
                (pin.get(), high)
            }
            _ => return Err(Fault::UnknownResource),
        };
        pin_json(pin, high, raw)
            .map(Attempt::Done)
            .ok_or(Fault::OutcomeTooLarge)
    }

    fn pin_is_output(&mut self, _pin: Pin) -> bool {
        true
    }

    fn read_back(&mut self, check: Check) -> u64 {
        match check {
            Check::Pin { pin, .. } => u64::from(self.level(pin)),
            Check::Clock { .. } => 0,
        }
    }

    fn operator_input(&mut self, _out: &mut [u8]) -> Result<Option<usize>, Fault> {
        Ok(None)
    }

    fn store_chunk(&mut self, _digest: u64, _index: u8, _chunk: &[u8]) -> Result<(), Fault> {
        // This image has no snapshot store: its runs never roll over.
        Err(Fault::BadRequest)
    }

    fn load_chunk(&mut self, _digest: u64, _index: u8, _out: &mut [u8]) -> Result<usize, Fault> {
        Err(Fault::SnapshotMissing)
    }
}

impl DevicePorts for EmuPorts {
    fn sense(&mut self, beat: Beat) -> Result<Sense, WireError> {
        let mut sense = Sense::at(0);
        if beat.get() == 1
            && let Some(command) = self.command
        {
            sense.command(command)?;
        }
        Ok(sense)
    }

    fn select_goal(&mut self, _goal: GoalId) {}

    fn store_item(&mut self, _digest: u64, _index: u16, _item: &[u8]) -> Result<(), StoreFault> {
        // The image never checkpoints: its agent beats fewer times than
        // `checkpoint_every`.
        Err(StoreFault::Full)
    }

    fn load_item(
        &mut self,
        _digest: u64,
        _index: u16,
        _out: &mut [u8],
    ) -> Result<usize, StoreFault> {
        Err(StoreFault::Unknown)
    }
}

/// The image's run store: one run's journal and terminal record.
pub struct EmuRuns {
    journal: EmuJournal,
    selected: Option<u64>,
    terminal: Option<(bool, [u8; PAGE_RECORD_MAX], usize)>,
}

impl RunStore for EmuRuns {
    type Journal = EmuJournal;

    fn select(&mut self, run: RunId, start: Option<&[u8]>) -> Result<(), StoreFault> {
        match self.selected {
            Some(selected) if selected == run.get() => Ok(()),
            Some(_) => Err(StoreFault::Full),
            None => {
                let start = start.ok_or(StoreFault::Unknown)?;
                self.journal
                    .input
                    .get_mut(..start.len())
                    .ok_or(StoreFault::Full)?
                    .copy_from_slice(start);
                self.journal.input_len = start.len();
                self.journal.run = run.get();
                self.selected = Some(run.get());
                Ok(())
            }
        }
    }

    fn input(&self, out: &mut [u8]) -> Result<usize, StoreFault> {
        let input = self
            .journal
            .input
            .get(..self.journal.input_len)
            .ok_or(StoreFault::Unknown)?;
        out.get_mut(..input.len())
            .ok_or(StoreFault::Unknown)?
            .copy_from_slice(input);
        Ok(input.len())
    }

    fn boot(&mut self) -> &mut EmuJournal {
        self.journal.pos = 0;
        &mut self.journal
    }

    fn terminal(&self, out: &mut [u8]) -> Option<(bool, usize)> {
        let (completed, bytes, len) = self.terminal.as_ref()?;
        out.get_mut(..*len)?.copy_from_slice(bytes.get(..*len)?);
        Some((*completed, *len))
    }

    fn conclude(&mut self, completed: bool, record: &[u8]) -> Result<(), StoreFault> {
        let mut bytes = [0u8; PAGE_RECORD_MAX];
        bytes
            .get_mut(..record.len())
            .ok_or(StoreFault::Full)?
            .copy_from_slice(record);
        self.terminal = Some((completed, bytes, record.len()));
        Ok(())
    }
}

/// What the run did on the core.
pub struct RunOutcome {
    /// Whether the run completed (else failed).
    pub completed: bool,
    /// The terminal record's bytes.
    pub terminal: [u8; PAGE_RECORD_MAX],
    /// Their length.
    pub len: usize,
    /// Boots it took.
    pub boots: u32,
    /// Physical writes.
    pub writes: u32,
    /// Records the journal holds.
    pub records: u32,
    /// Whether pin 3 reads high.
    pub pin3_high: bool,
}

/// The script both stages run: read pin 3, drive it high, finish.
pub const SCRIPT: &[&[u8]] = &[
    br#"CALL gpio_pin_read {"pin": 3}"#,
    br#"CALL gpio_pin_write {"pin": 3, "level": "high"}"#,
    br#"FINISH {"status": "completed", "summary": "pin 3 high"}"#,
];

/// Drive [`SCRIPT`] through the run workflow and the product run
/// dispatcher, with one reset after the write acts and before its answer
/// commits.
///
/// # Errors
///
/// A static reason when the run faults, never concludes, or the record
/// does not fit.
pub fn drive() -> Result<RunOutcome, &'static str> {
    let seed = RunSeed {
        id: RunId::new(7),
        ..RunSeed::default_slice()
    };
    let mut input = [0u8; MAX_RUN_START_BYTES];
    let n = RunStart {
        seed,
        snapshot: None,
    }
    .encode(&mut input)
    .map_err(|_| "run: input")?;
    let input = input.get(..n).ok_or("run: input")?;
    let mut journal = EmuJournal::new(seed.id.get(), 0, true);
    let mut ports = EmuPorts::new(SCRIPT, None);
    let mut state = RunState::new();
    for boots in 1..=4u32 {
        journal.boot();
        let mut out = [0u8; PAGE_RECORD_MAX];
        let mut dispatcher = RunDispatcher::new(&mut ports, seed.id);
        let mut ctx = Ctx::new(&mut journal, &mut dispatcher, &mut out);
        let polled = {
            let mut workflow = pin!(run(&mut ctx, input, &mut state));
            workflow
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        };
        if let Poll::Ready(Err(_)) = polled {
            return Err("run: the workflow faulted");
        }
        if let Some(Conclusion::Ended(outcome)) = ctx.conclusion() {
            let (completed, bytes) = match outcome {
                Outcome::Completed(bytes) => (true, bytes),
                Outcome::Failed(bytes) => (false, bytes),
            };
            let mut terminal = [0u8; PAGE_RECORD_MAX];
            terminal
                .get_mut(..bytes.len())
                .ok_or("run: terminal")?
                .copy_from_slice(bytes);
            let len = bytes.len();
            return Ok(RunOutcome {
                completed,
                terminal,
                len,
                boots,
                writes: ports.writes,
                records: u32::try_from(journal.len).map_err(|_| "run: records")?,
                pin3_high: ports.level(3),
            });
        }
        if !journal.crashed {
            return Err("run: stopped without a conclusion or a reset");
        }
    }
    Err("run: too many boots")
}

/// The last pulse a beat emitted.
struct LastPulse(Option<Pulse>);

impl PulseSink for LastPulse {
    fn pulse(&mut self, pulse: &Pulse) {
        self.0 = Some(*pulse);
    }
}

/// What the agent did on the core.
pub struct AgentOutcome {
    /// The beat-1 pulse.
    pub pulse: Pulse,
    /// The supervisor's state after the beat.
    pub state: AgentState,
    /// Boots it took.
    pub boots: u32,
    /// Physical writes.
    pub writes: u32,
    /// Whether pin 3 reads high.
    pub pin3_high: bool,
}

/// One heartbeat of the always-on agent on the board dispatcher: the
/// operator adds a goal, the supervisor spawns its run, the run reads,
/// writes and finishes — through a reset after the write acted and
/// before its answer was recorded — and the beat settles and harvests.
///
/// # Errors
///
/// A static reason when the agent faults or never completes the beat.
pub fn drive_agent() -> Result<AgentOutcome, &'static str> {
    let seed = AgentSeed::default_slice();
    let goal = Command::add_goal(
        b"drive pin 3 high",
        1,
        Cadence::Once,
        Envelope {
            per_run_turns: 6,
            per_run_mutations: 2,
            turns: 40,
            mutations: 10,
            runs: 1,
        },
        seed.template.capabilities,
    )
    .map_err(|_| "agent: command")?;
    let mut start = [0u8; MAX_START_BYTES];
    let n = seed
        .start()
        .encode(&mut start)
        .map_err(|_| "agent: start")?;
    let start = start.get(..n).ok_or("agent: start")?;
    let mut journal = EmuJournal::new(1, 1, false);
    let mut runs = EmuRuns {
        journal: EmuJournal::new(0, 0, true),
        selected: None,
        terminal: None,
    };
    let mut ports = EmuPorts::new(SCRIPT, Some(goal));
    let mut run_state = RunState::new();
    let mut state = AgentState::new();
    for boots in 1..=4u32 {
        journal.boot();
        // A reset loses RAM, never media: the run store keeps its
        // journal, and its next drive rewinds it.
        runs.journal.crashed = false;
        let mut sink = LastPulse(None);
        let mut out = [0u8; PAGE_RECORD_MAX];
        let mut device = Device::new(&mut runs, &mut ports, &mut run_state);
        let mut ctx = Ctx::new(&mut journal, &mut device, &mut out);
        let polled = {
            let mut alarm = NoAlarm;
            let mut workflow = pin!(supervise(
                &mut ctx, &seed, start, &mut alarm, &mut state, &mut sink
            ));
            workflow
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
        };
        if let Poll::Ready(Err(_)) = polled {
            return Err("agent: the supervisor faulted");
        }
        if let Some(pulse) = sink.0 {
            return Ok(AgentOutcome {
                pulse,
                state,
                boots,
                writes: ports.writes,
                pin3_high: ports.level(3),
            });
        }
        if !runs.journal.crashed {
            return Err("agent: the beat stopped without a reset");
        }
    }
    Err("agent: too many boots")
}
