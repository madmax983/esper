//! The run workflow on the core: a fixed-capacity Waymaker journal, a
//! GPIO device and a scripted model, all `no_std`, driving
//! [`esper_runtime::run::run`] through an injected reset.
//!
//! The journal, the device and the model are this image's doubles for
//! the board's flash, pins and model. The thing under test is the
//! workflow itself, which is product code: its decisions, its intents,
//! its replay, and the device half every dispatcher shares
//! ([`esper_runtime::run::device`]).

use core::pin::pin;
use core::task::{Context, Poll, Waker};

use esper_core::decision::{Level, ToolArgs};
use esper_core::ids::RunId;
use esper_runtime::agent::wire::PAGE_RECORD_MAX;
use esper_runtime::run::wire::{
    self, Check, Fault, MAX_RUN_START_BYTES, Probed, RunStart, ToolRequest, Verified, VerifyRequest,
};
use esper_runtime::run::{RunState, device, run};
use esper_runtime::{BackendError, BundleId, ModelBackend, RunSeed, TokenUsage};
use waymaker_core::timer::{ClockKind, TimerSpec};
use waymaker_core::{ActivityKind, EffectId, EffectSeq, Outcome, RunId as WaymakerRunId};
use waymaker_embassy::ctx::Conclusion;
use waymaker_embassy::dispatch::Produced;
use waymaker_embassy::{ActivityDispatcher, Answer, Ctx, Halted, Handoff, Journal};

/// Records the journal holds: ample for a short run.
const MAX_RECORDS: usize = 32;

/// One record slot.
#[derive(Clone, Copy)]
struct Slot {
    scheduled: bool,
    kind: u16,
    seq: u32,
    completed: bool,
    len: usize,
    bytes: [u8; PAGE_RECORD_MAX],
}

impl Slot {
    const EMPTY: Self = Self {
        scheduled: false,
        kind: 0,
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
/// simulated resets, with one armed reset: after a mutating tool acts
/// and before its answer commits.
pub struct EmuJournal {
    slots: [Slot; MAX_RECORDS],
    len: usize,
    pos: usize,
    run: u64,
    armed: bool,
    /// The reset fired this boot.
    pub crashed: bool,
}

impl EmuJournal {
    const fn new(run: u64, armed: bool) -> Self {
        Self {
            slots: [Slot::EMPTY; MAX_RECORDS],
            len: 0,
            pos: 0,
            run,
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
            .find(|slot| slot.scheduled)
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
            if !slot.scheduled || slot.kind != kind.0 || slot.bytes() != input {
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
            if slot.scheduled {
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
        slot.scheduled = true;
        slot.kind = kind.0;
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
                ActivityKind(slot.kind) == wire::TOOL
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
        Err(Halted)
    }

    fn continue_as_new(&mut self, _input: &[u8]) -> Halted {
        Halted
    }

    fn deadline_remaining(&self) -> Option<(ClockKind, u64)> {
        None
    }
}

/// Eight output pins and a write counter, deduplicating a redelivered
/// effect on its Esper sequence the way a board's actuator driver must.
pub struct EmuDevice {
    levels: [bool; 8],
    /// Physical writes executed.
    pub writes: u32,
    seen: [Option<u32>; 8],
}

impl EmuDevice {
    const fn new() -> Self {
        Self {
            levels: [false; 8],
            writes: 0,
            seen: [None; 8],
        }
    }

    /// A pin's level.
    #[must_use]
    pub fn level(&self, pin: u8) -> bool {
        self.levels.get(usize::from(pin)).copied().unwrap_or(false)
    }

    fn write(&mut self, seq: u32, pin: u8, high: bool) {
        if self.seen.contains(&Some(seq)) {
            return;
        }
        if let Some(slot) = self.seen.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(seq);
        }
        if let Some(level) = self.levels.get_mut(usize::from(pin)) {
            *level = high;
            self.writes += 1;
        }
    }
}

/// `{"pin":N,"level":"high"}` into `out`.
fn pin_json(pin: u8, high: bool, out: &mut [u8; 32]) -> &[u8] {
    let level: &[u8] = if high { b"high" } else { b"low" };
    let mut len = 0;
    for part in [
        &b"{\"pin\":"[..],
        &[b'0' + pin % 10],
        b",\"level\":\"",
        level,
        b"\"}",
    ] {
        if let Some(dst) = out.get_mut(len..len + part.len()) {
            dst.copy_from_slice(part);
            len += part.len();
        }
    }
    out.get(..len).unwrap_or(&[])
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

/// The image's dispatcher: the device half of every run activity.
struct EmuDispatcher<'a> {
    model: &'a mut EmuModel,
    device: &'a mut EmuDevice,
}

impl EmuDispatcher<'_> {
    fn tool(&mut self, input: &[u8], out: &mut [u8]) -> Produced {
        let Ok(request) = ToolRequest::decode(input) else {
            return device::fail(Fault::BadRequest, out);
        };
        let args = match device::bind(&request) {
            Ok(args) => args,
            Err(fault) => return device::fail(fault, out),
        };
        let mut raw = [0u8; 32];
        let raw = match args {
            ToolArgs::GpioPinRead { pin } => {
                pin_json(pin.get(), self.device.level(pin.get()), &mut raw)
            }
            ToolArgs::GpioPinWrite { pin, level } => {
                let high = level == Level::High;
                self.device.write(request.seq, pin.get(), high);
                pin_json(pin.get(), high, &mut raw)
            }
            _ => return device::fail(Fault::UnknownResource, out),
        };
        device::observe(raw, out)
    }
}

impl ActivityDispatcher for EmuDispatcher<'_> {
    type Error = ();

    fn poll_dispatch(
        &mut self,
        _task: &mut Context<'_>,
        _id: EffectId,
        kind: ActivityKind,
        input: &[u8],
        out: &mut [u8],
    ) -> Poll<Result<Produced, ()>> {
        Poll::Ready(Ok(match kind {
            wire::INFER => device::infer(self.model, input, out),
            wire::PROBE => device::produced(
                (Probed { output: true }).encode(out),
                Fault::BadRequest,
                out,
            ),
            wire::TOOL => self.tool(input, out),
            wire::VERIFY => match VerifyRequest::decode(input) {
                Ok(VerifyRequest {
                    check: Check::Pin { pin, .. },
                    ..
                }) => device::produced(
                    (Verified {
                        observed: u64::from(self.device.level(pin)),
                    })
                    .encode(out),
                    Fault::BadRequest,
                    out,
                ),
                _ => device::fail(Fault::BadRequest, out),
            },
            _ => Produced::Unserviceable,
        }))
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

/// The script the image runs: read pin 3, drive it high, finish.
pub const SCRIPT: &[&[u8]] = &[
    br#"CALL gpio_pin_read {"pin": 3}"#,
    br#"CALL gpio_pin_write {"pin": 3, "level": "high"}"#,
    br#"FINISH {"status": "completed", "summary": "pin 3 high"}"#,
];

/// Drive [`SCRIPT`] through the run workflow, with one reset after the
/// write acts and before its answer commits.
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
    let mut journal = EmuJournal::new(seed.id.get(), true);
    let mut model = EmuModel {
        lines: SCRIPT,
        next: 0,
    };
    let mut device = EmuDevice::new();
    let mut state = RunState::new();
    for boots in 1..=4u32 {
        journal.boot();
        let mut out = [0u8; PAGE_RECORD_MAX];
        let mut dispatcher = EmuDispatcher {
            model: &mut model,
            device: &mut device,
        };
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
                writes: device.writes,
                records: u32::try_from(journal.len).map_err(|_| "run: records")?,
                pin3_high: device.level(3),
            });
        }
        if !journal.crashed {
            return Err("run: stopped without a conclusion or a reset");
        }
    }
    Err("run: too many boots")
}
