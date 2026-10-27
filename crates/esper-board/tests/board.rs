//! The board end to end on a RAM chip: the supervisor and its run on
//! flash, through Waymaker's driver and bridge, across power cuts.
//!
//! Everything that runs here is the product code the S3 image links:
//! [`Supervisor`], [`FlashRuns`], [`BoardPorts`], [`ItemLog`], the
//! operator channel, and the runtime's `Device` and `run`. Only the chip
//! ([`RamNor`]), the pins and the clocks are doubles.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use esper_board::agent::{Booted, Supervisor};
use esper_board::demo::{self, DISTILL, GOAL, SCRIPT};
use esper_board::items::ItemLog;
use esper_board::operator::{Operator, Reply};
use esper_board::plan::{Plan, RUN_SLOTS};
use esper_board::ports::{BoardPorts, Hardware};
use esper_board::ram::{Cut, RamNor};
use esper_board::runs::FlashRuns;
use esper_core::goal::Envelope;
use esper_runtime::agent::{AgentSeed, AgentState, Pulse, PulseSink};
use esper_runtime::backend::{
    BackendError, BundleId, ModelBackend, TokenUsage, fingerprint_prompt, fnv1a64,
};
use esper_runtime::run::RunState;
use waymaker_core::timer::{ClockCapability, ClockKind};
use waymaker_drive::{Clocks, Progress};

/// The plan's chip: banks of 32 KiB, seven windows.
const BANK: u32 = 32 * 1024;
const CHIP: usize = 7 * 2 * 32 * 1024;

/// The boot clock, in milliseconds; a reset puts it back to zero.
#[derive(Clone)]
struct BootClock(Rc<Cell<u64>>);

impl Clocks for BootClock {
    fn capability(&self) -> ClockCapability {
        ClockCapability::BootOnly
    }

    fn now(&mut self, kind: ClockKind) -> Option<u64> {
        (kind == ClockKind::AFTER_BOOT).then(|| self.0.get())
    }
}

/// Eight pins, all outputs, the boot clock, and a count of the writes
/// that changed a level.
struct Pins {
    levels: [bool; 8],
    changes: u32,
    clock: BootClock,
}

impl Hardware for Pins {
    fn is_output(&mut self, _pin: u8) -> bool {
        true
    }

    fn level(&mut self, pin: u8) -> bool {
        self.levels.get(usize::from(pin)).copied().unwrap_or(false)
    }

    fn drive(&mut self, pin: u8, high: bool) {
        if let Some(level) = self.levels.get_mut(usize::from(pin)) {
            if *level != high {
                self.changes += 1;
            }
            *level = high;
        }
    }

    fn sensor(&mut self, _sensor: u8) -> Option<u16> {
        None
    }

    fn uptime_ms(&mut self) -> u64 {
        self.clock.0.get()
    }

    fn delay_ms(&mut self, ms: u16) {
        self.clock.0.set(self.clock.0.get() + u64::from(ms));
    }

    fn now(&mut self) -> u64 {
        self.clock.0.get()
    }
}

/// A prompt-keyed model that answers [`SCRIPT`] in order and records
/// what it was asked: the distillation [`DISTILL`] is checked against.
#[derive(Default)]
struct Recorder {
    seen: Vec<(u64, &'static [u8])>,
}

impl ModelBackend for Recorder {
    fn infer(&mut self, prompt: &[u8], out: &mut [u8]) -> Result<usize, BackendError> {
        let fingerprint = fingerprint_prompt(prompt);
        let line = if let Some((_, line)) = self.seen.iter().find(|(seen, _)| *seen == fingerprint)
        {
            *line
        } else {
            let line = *SCRIPT.get(self.seen.len()).ok_or(BackendError::Exhausted)?;
            self.seen.push((fingerprint, line));
            line
        };
        out.get_mut(..line.len())
            .ok_or(BackendError::OutputTooLong)?
            .copy_from_slice(line);
        Ok(line.len())
    }

    fn bundle_id(&self) -> BundleId {
        BundleId(0)
    }

    fn last_usage(&self) -> TokenUsage {
        TokenUsage::default()
    }

    fn unemit(&mut self) {}
}

/// The last pulse that settled a run.
#[derive(Default)]
struct Settled(Option<Pulse>);

impl PulseSink for Settled {
    fn pulse(&mut self, pulse: &Pulse) {
        if pulse.settled.is_some() {
            self.0 = Some(*pulse);
        }
    }
}

const fn envelope() -> Envelope {
    Envelope {
        per_run_turns: 6,
        per_run_mutations: 2,
        turns: 40,
        mutations: 10,
        runs: 1,
    }
}

/// What a whole agent life on the chip ended with.
struct Life {
    /// The pulse that settled the run, or `None` when the supervisor's
    /// own journal was refused (and why).
    pulse: Result<Pulse, String>,
    pin3: bool,
    /// Writes that changed pin 3's level.
    changes: u32,
    boots: u32,
    cuts: u32,
}

/// Run the agent on `chip` until a run settles, cutting power at the
/// `cut`th mutation (counted from the first boot) if one is given.
fn live<M: ModelBackend>(
    chip: &RefCell<Box<RamNor<CHIP>>>,
    model: &mut M,
    cut: Option<(u32, Cut)>,
) -> Result<Life, String> {
    live_from(chip, model, cut, false)
}

/// [`live`], with pin 3 starting `high`.
#[expect(
    clippy::too_many_lines,
    reason = "one power cycle is one readable loop: build RAM, boot until a pulse or a cut"
)]
fn live_from<M: ModelBackend>(
    chip: &RefCell<Box<RamNor<CHIP>>>,
    model: &mut M,
    cut: Option<(u32, Cut)>,
    high: bool,
) -> Result<Life, String> {
    let seed = AgentSeed::default_slice();
    let mut levels = [false; 8];
    levels[3] = high;
    let mut changes = 0;
    let mut boots = 0;
    if let Some((at, how)) = cut {
        chip.borrow_mut().cut_after(at, how);
    }
    // One power cycle per iteration: everything RAM holds is rebuilt.
    for cuts in 0..8 {
        let flash = RefCell::new(DerefNor(chip));
        let plan = Plan::new(&flash, 0, BANK).map_err(|e| format!("plan: {e}"))?;
        let clock = BootClock(Rc::new(Cell::new(0)));
        let mut supervisor = Supervisor::new(
            plan.supervisor,
            plan.layout,
            plan.supervisor_reserve,
            clock.clone(),
        );
        let mut runs: FlashRuns<'_, _, _, RUN_SLOTS> =
            FlashRuns::new(plan.runs, plan.layout, plan.run_reserve, clock.clone());
        let mut checkpoints = ItemLog::new(plan.checkpoints);
        let mut snapshots = ItemLog::new(plan.snapshots);
        let mut operator = Operator::new(envelope(), seed.template.capabilities);
        let mut pins = Pins {
            levels,
            changes,
            clock: clock.clone(),
        };
        let mut run_state = RunState::new();
        let mut state = AgentState::new();
        let mut sink = Settled::default();
        let mut typed = false;
        for _step in 0..64 {
            let progress = {
                let mut ports = BoardPorts::new(
                    &mut pins,
                    model,
                    &mut operator,
                    &mut checkpoints,
                    &mut snapshots,
                );
                boots += 1;
                supervisor.boot(
                    &seed,
                    &mut runs,
                    &mut ports,
                    &mut run_state,
                    &mut state,
                    &mut sink,
                )
            };
            if let Some(pulse) = sink.0 {
                return Ok(Life {
                    pulse: Ok(pulse),
                    pin3: pins.levels[3],
                    changes: pins.changes,
                    boots,
                    cuts,
                });
            }
            // The operator types the goal until the agent has it: a line
            // a reset took before any `SENSE` recorded it is gone.
            if state.checkpoint.ledger.is_empty() && !typed {
                for byte in GOAL.iter().chain(b"\n") {
                    if let Some(reply) = operator.feed(*byte) {
                        assert_eq!(reply, Reply::Queued);
                    }
                }
                typed = true;
            }
            match progress {
                Ok(Booted::Reinstalled(_)) => {}
                Ok(Booted::Ran(Progress::WaitingUntil { remaining, .. })) => {
                    clock.0.set(clock.0.get() + remaining.max(1));
                }
                Ok(Booted::Ran(Progress::Waiting { .. } | Progress::Migrated { .. })) => {
                    clock.0.set(clock.0.get() + 1);
                }
                Ok(Booted::Ran(Progress::Finished { .. })) => {
                    return Err("the supervisor ended".into());
                }
                Err(error) => {
                    if chip.borrow().is_off() {
                        break;
                    }
                    // Powered, and the supervisor's journal refused: the
                    // board stops and says so.
                    return Ok(Life {
                        pulse: Err(format!("{error:?}")),
                        pin3: pins.levels[3],
                        changes: pins.changes,
                        boots,
                        cuts,
                    });
                }
            }
        }
        if !chip.borrow().is_off() {
            return Err("no run settled".into());
        }
        // Power comes back: media keeps what it had, pins keep their
        // levels (the pad is driven until the next reset of the GPIO
        // block, which this model does not take), RAM is lost.
        levels = pins.levels;
        changes = pins.changes;
        chip.borrow_mut().power_on();
    }
    Err("too many power cycles".into())
}

/// `&RefCell<Box<RamNor>>` as one chip a `Plan` can borrow afresh after
/// every power cycle.
struct DerefNor<'c>(&'c RefCell<Box<RamNor<CHIP>>>);

impl embedded_storage::nor_flash::ErrorType for DerefNor<'_> {
    type Error = esper_board::ram::RamError;
}

impl embedded_storage::nor_flash::ReadNorFlash for DerefNor<'_> {
    const READ_SIZE: usize = 1;

    fn read(&mut self, offset: u32, bytes: &mut [u8]) -> Result<(), Self::Error> {
        self.0.borrow_mut().read(offset, bytes)
    }

    fn capacity(&self) -> usize {
        CHIP
    }
}

impl embedded_storage::nor_flash::NorFlash for DerefNor<'_> {
    const WRITE_SIZE: usize = 4;
    const ERASE_SIZE: usize = 4096;

    fn erase(&mut self, from: u32, to: u32) -> Result<(), Self::Error> {
        self.0.borrow_mut().erase(from, to)
    }

    fn write(&mut self, offset: u32, bytes: &[u8]) -> Result<(), Self::Error> {
        self.0.borrow_mut().write(offset, bytes)
    }
}

fn chip() -> RefCell<Box<RamNor<CHIP>>> {
    RefCell::new(Box::new(RamNor::new()))
}

#[test]
fn the_board_runs_the_goal_on_flash_and_drives_pin_3_high() {
    let chip = chip();
    let mut model = Recorder::default();
    let life = live(&chip, &mut model, None).expect("the run settles");
    assert!(life.pin3, "the run drove pin 3 high");
    assert_eq!(life.changes, 1);
    let settled = life
        .pulse
        .expect("the supervisor ran")
        .settled
        .expect("settled");
    assert_eq!(
        format!("{:?}", settled.status),
        "Completed",
        "the run completed: {settled:?}"
    );
    assert_eq!(life.cuts, 0);
    assert!(life.boots >= 2, "the beat timer ended at least one boot");
}

#[test]
fn the_distilled_table_is_the_prompts_the_board_renders() {
    // Two worlds: pin 3 low, as the goal finds it, and pin 3 already
    // high, as a run segment replayed after a reinstall finds it.
    let mut model = Recorder::default();
    live_from(&chip(), &mut model, None, false).expect("the run settles");
    let mut again = Recorder::default();
    live_from(&chip(), &mut again, None, true).expect("the run settles");
    for entry in again.seen {
        if !model.seen.iter().any(|(seen, _)| *seen == entry.0) {
            model.seen.push(entry);
        }
    }
    let rendered: Vec<String> = model
        .seen
        .iter()
        .map(|(fingerprint, line)| {
            format!(
                "    DistillEntry {{\n        prompt_fp: 0x{fingerprint:016x},\n        line_hash: 0x{:016x},\n        line: {},\n    }},",
                fnv1a64(line),
                rust_bytes(line)
            )
        })
        .collect();
    let table: Vec<(u64, u64, &[u8])> = DISTILL
        .iter()
        .map(|entry| (entry.prompt_fp, entry.line_hash, entry.line))
        .collect();
    let seen: Vec<(u64, u64, &[u8])> = model
        .seen
        .iter()
        .map(|(fingerprint, line)| (*fingerprint, fnv1a64(line), *line))
        .collect();
    assert_eq!(
        table,
        seen,
        "demo::DISTILL is stale; it should read:\n{}",
        rendered.join("\n")
    );
}

#[test]
fn the_distilled_model_runs_the_goal_without_the_script() {
    let chip = chip();
    let mut model = demo::model();
    let life = live(&chip, &mut model, None).expect("the run settles");
    assert!(life.pin3);
}

/// The mutations a clean life takes.
fn clean_mutations() -> u32 {
    let clean = chip();
    live(&clean, &mut demo::model(), None).expect("the clean run settles");
    let total = clean.borrow().mutations;
    assert!(total > 10, "a clean life writes the journal: {total}");
    total
}

#[test]
fn a_reset_between_any_two_storage_operations_settles_the_run_once() {
    for at in 1..=clean_mutations() {
        let chip = chip();
        let life = live(&chip, &mut demo::model(), Some((at, Cut::Before)))
            .unwrap_or_else(|error| panic!("reset before mutation {at}: {error}"));
        let pulse = life
            .pulse
            .unwrap_or_else(|error| panic!("reset before mutation {at}: refused: {error}"));
        let settled = pulse.settled.expect("settled");
        assert_eq!(
            format!("{:?}", settled.status),
            "Completed",
            "reset before mutation {at}: {settled:?}"
        );
        assert!(life.pin3, "reset before mutation {at}: pin 3 is high");
        assert_eq!(
            life.changes, 1,
            "reset before mutation {at}: one write changed the pin"
        );
        // A cut that lands after the settling beat's pulse needs no power
        // cycle for the run to have settled.
        assert!(life.cuts <= 1, "reset before mutation {at}: one cut");
    }
}

#[test]
fn a_torn_write_at_any_mutation_still_settles_the_run_once() {
    // A cut *inside* a program leaves a torn frame, and Waymaker refuses a
    // journal whose tail it cannot read (its ADR 0018). The board answers
    // by reinstalling the segment from its own input (`slot::reinstall`),
    // so a torn write costs a replay — never the run, never the agent,
    // and never a second change of the pin.
    for at in 1..=clean_mutations() {
        let chip = chip();
        let life = live(&chip, &mut demo::model(), Some((at, Cut::Torn)))
            .unwrap_or_else(|error| panic!("tear at mutation {at}: {error}"));
        let pulse = life
            .pulse
            .unwrap_or_else(|error| panic!("tear at mutation {at}: refused: {error}"));
        let settled = pulse.settled.expect("settled");
        assert_eq!(
            format!("{:?}", settled.status),
            "Completed",
            "tear at mutation {at}: {settled:?}"
        );
        assert!(life.pin3, "tear at mutation {at}: pin 3 is high");
        assert_eq!(
            life.changes, 1,
            "tear at mutation {at}: one write changed the pin"
        );
    }
}

/// `line` as a Rust byte-string literal.
fn rust_bytes(line: &[u8]) -> String {
    let text = String::from_utf8_lossy(line);
    format!("br#\"{text}\"#")
}
