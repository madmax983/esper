//! The ESP32-S3 image: Esper's supervisor and its runs on the chip.
//!
//! Glue only. [`esper_board`] is the product logic — the supervisor on
//! its own Waymaker bank pair, the runs on theirs, the item logs, the
//! operator line protocol, the six tools — and this file hands it the
//! chip: `esp-storage`'s flash, eight GPIOs, the boot clock and UART0.
//!
//! | Board | Use |
//! |---|---|
//! | SPI flash at [`PLAN_BASE`] | [`Plan`]: supervisor slot, run slots, item logs |
//! | GPIO4–7, GPIO15–18 | protocol pins 0–7, all outputs |
//! | UART0 (GPIO43 TX, GPIO44 RX) | the operator channel: one command per line, replies and pulses |
//! | USB-Serial-JTAG | logs and panics (`esp-println`) |
//!
//! The boot clock is the only clock: a power cut restarts it, so beats
//! are `AfterBoot` deadlines (`ClockCapability::BootOnly`). A devkit's
//! RTC is not backed, and declaring a persistent clock the board does
//! not have is the one substitution Waymaker's clock contract forbids.
//!
//! The main loop boots the supervisor, sleeps out its beat while pumping
//! the UART into the operator channel, and boots it again. Every boot
//! replays from flash; nothing the loop holds in RAM is needed after a
//! reset.

#![no_std]
#![no_main]

use core::cell::RefCell;

use esp_backtrace as _;
use esp_hal::delay::Delay;
use esp_hal::gpio::{Level, Output, OutputConfig};
use esp_hal::main;
use esp_hal::time::Instant;
use esp_hal::uart::{Config as UartConfig, Uart};
use esp_println::println;
use esp_storage::FlashStorage;
use esper_board::demo;
use esper_board::items::ItemLog;
use esper_board::operator::{Operator, Reply};
use esper_board::plan::{Plan, RUN_SLOTS};
use esper_board::ports::{BoardPorts, Hardware};
use esper_board::{Booted, FlashRuns, Supervisor};
use esper_core::beat::BeatSpan;
use esper_core::goal::Envelope;
use esper_runtime::agent::{AgentSeed, AgentState, Heartbeat, Pulse, PulseSink};
use esper_runtime::run::RunState;
use waymaker_core::timer::{ClockCapability, ClockKind};
use waymaker_drive::{Clocks, Progress};

esp_bootloader_esp_idf::esp_app_desc!();

/// Where the plan starts on the flash chip: 4 MiB in, past the
/// application partition of the default 8 MiB layout.
const PLAN_BASE: u32 = 0x40_0000;

/// One Waymaker bank: 32 KiB, so a slot is 64 KiB and the plan 448 KiB.
const BANK_BYTES: u32 = 32 * 1024;

/// A beat every second.
const BEAT_MS: u64 = 1000;

/// The longest the loop sleeps before it looks at the UART again.
const PUMP_MS: u64 = 20;

/// Milliseconds since boot.
fn millis() -> u64 {
    Instant::now().duration_since_epoch().as_millis()
}

/// The boot clock, the only clock this board has.
#[derive(Clone, Copy)]
struct BootClock;

impl Clocks for BootClock {
    fn capability(&self) -> ClockCapability {
        ClockCapability::BootOnly
    }

    fn now(&mut self, kind: ClockKind) -> Option<u64> {
        (kind == ClockKind::AFTER_BOOT).then(millis)
    }
}

/// The eight protocol pins and the delay.
struct Board<'d> {
    pins: [Output<'d>; 8],
    delay: Delay,
}

impl Hardware for Board<'_> {
    fn is_output(&mut self, pin: u8) -> bool {
        usize::from(pin) < self.pins.len()
    }

    fn level(&mut self, pin: u8) -> bool {
        self.pins
            .get(usize::from(pin))
            .is_some_and(Output::is_set_high)
    }

    fn drive(&mut self, pin: u8, high: bool) {
        if let Some(output) = self.pins.get_mut(usize::from(pin)) {
            output.set_level(if high { Level::High } else { Level::Low });
        }
    }

    fn sensor(&mut self, _sensor: u8) -> Option<u16> {
        // No sensor is wired: `sensor_sample_read` answers UnknownResource.
        None
    }

    fn uptime_ms(&mut self) -> u64 {
        millis()
    }

    fn delay_ms(&mut self, ms: u16) {
        self.delay.delay_millis(u32::from(ms));
    }

    fn now(&mut self) -> u64 {
        millis()
    }
}

/// Pulses, written to the operator's UART as one line each.
struct Pulses<'u, 'd> {
    uart: &'u mut Uart<'d, esp_hal::Blocking>,
}

impl PulseSink for Pulses<'_, '_> {
    fn pulse(&mut self, pulse: &Pulse) {
        let mut line = [0u8; 96];
        let len = render_pulse(pulse, &mut line);
        write_line(self.uart, line.get(..len).unwrap_or_default());
    }
}

/// `pulse beat=<n> settled=<status> active=<n> waiting=<n> done=<n>`.
fn render_pulse(pulse: &Pulse, out: &mut [u8]) -> usize {
    let mut cursor = Cursor { out, len: 0 };
    cursor.text(b"pulse beat=");
    cursor.number(pulse.beat.get());
    if let Some(settled) = pulse.settled {
        cursor.text(b" settled=goal");
        cursor.number(u64::from(settled.goal.get()));
        cursor.text(
            if matches!(settled.status, esper_core::state::TerminalStatus::Completed) {
                b":completed"
            } else {
                b":ended"
            },
        );
    }
    if pulse.parked {
        cursor.text(b" parked");
    }
    cursor.text(b" active=");
    cursor.number(u64::from(pulse.counts.active));
    cursor.text(b" waiting=");
    cursor.number(u64::from(pulse.counts.waiting));
    cursor.text(b" done=");
    cursor.number(u64::from(pulse.counts.done));
    cursor.len
}

/// A bounded writer that drops what does not fit.
struct Cursor<'o> {
    out: &'o mut [u8],
    len: usize,
}

impl Cursor<'_> {
    fn text(&mut self, bytes: &[u8]) {
        for byte in bytes {
            if let Some(slot) = self.out.get_mut(self.len) {
                *slot = *byte;
                self.len += 1;
            }
        }
    }

    fn number(&mut self, mut value: u64) {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            if let Some(digit) = digits.get_mut(at) {
                *digit = b'0' + (value % 10) as u8;
            }
            value /= 10;
            if value == 0 || at == 0 {
                break;
            }
        }
        let digits = digits;
        self.text(digits.get(at..).unwrap_or_default());
    }
}

fn write_line(uart: &mut Uart<'_, esp_hal::Blocking>, line: &[u8]) {
    let _ = uart.write(line);
    let _ = uart.write(b"\r\n");
}

/// Feed every byte the UART holds into the operator channel, answering
/// each finished line.
fn pump(uart: &mut Uart<'_, esp_hal::Blocking>, operator: &mut Operator) {
    let mut byte = [0u8; 1];
    while uart.read_ready() {
        if !matches!(uart.read(&mut byte), Ok(1)) {
            break;
        }
        let Some(reply) = operator.feed(byte[0]) else {
            continue;
        };
        match reply {
            Reply::Queued => write_line(uart, b"ok"),
            Reply::Held(_) => write_line(uart, b"ok held"),
            Reply::Refused(why) => {
                let _ = uart.write(b"err ");
                write_line(uart, why.as_bytes());
            }
        }
    }
}

/// The agent this image runs: the slice template, one beat a second.
const fn seed() -> AgentSeed {
    let mut seed = AgentSeed::default_slice();
    seed.heartbeat = Heartbeat::Boot { ticks: BEAT_MS };
    if let Some(span) = BeatSpan::new(16) {
        seed.checkpoint_every = span;
    }
    seed
}

/// Stop, saying why, and keep saying it.
fn halt(why: &str) -> ! {
    loop {
        println!("esper: halted: {why}");
        Delay::new().delay_millis(5000);
    }
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default());
    let flash = RefCell::new(FlashStorage::new(peripherals.FLASH));
    let mut board = Board {
        pins: board_pins!(peripherals),
        delay: Delay::new(),
    };
    let Ok(uart) = Uart::new(peripherals.UART0, UartConfig::default()) else {
        halt("UART0 refused its configuration");
    };
    let mut uart = uart_pins!(uart, peripherals);

    let Ok(plan) = Plan::new(&flash, PLAN_BASE, BANK_BYTES) else {
        halt("the flash plan does not fit this chip");
    };
    let seed = seed();
    let mut supervisor = Supervisor::new(
        plan.supervisor,
        plan.layout,
        plan.supervisor_reserve,
        BootClock,
    );
    let mut runs: FlashRuns<'_, _, _, RUN_SLOTS> =
        FlashRuns::new(plan.runs, plan.layout, plan.run_reserve, BootClock);
    let mut checkpoints = ItemLog::new(plan.checkpoints);
    let mut snapshots = ItemLog::new(plan.snapshots);
    let mut operator = Operator::new(
        Envelope {
            per_run_turns: 6,
            per_run_mutations: 2,
            turns: 40,
            mutations: 10,
            runs: 1,
        },
        seed.template.capabilities,
    );
    let mut model = demo::model();
    // Caller-owned state, alive for the whole program: the run's replay
    // state (about 12 KiB) and the supervisor's.
    let mut run_state = RunState::new();
    let mut state = AgentState::new();

    println!("esper: booted; type `goal 1 drive pin 3 high` on UART0");
    write_line(&mut uart, b"esper ready");
    loop {
        pump(&mut uart, &mut operator);
        let booted = {
            let mut ports = BoardPorts::new(
                &mut board,
                &mut model,
                &mut operator,
                &mut checkpoints,
                &mut snapshots,
            );
            let mut sink = Pulses { uart: &mut uart };
            supervisor.boot(
                &seed,
                &mut runs,
                &mut ports,
                &mut run_state,
                &mut state,
                &mut sink,
            )
        };
        let wait = match booted {
            Ok(Booted::Ran(Progress::WaitingUntil { remaining, .. })) => remaining,
            Ok(Booted::Ran(Progress::Waiting { .. })) => PUMP_MS,
            Ok(Booted::Ran(Progress::Migrated { .. }) | Booted::Reinstalled(_)) => 0,
            Ok(Booted::Ran(Progress::Finished { .. })) => halt("the supervisor ended"),
            Err(error) => {
                println!("esper: supervisor boot refused: {error:?}");
                BEAT_MS
            }
        };
        if runs.reinstalled() > 0 {
            println!(
                "esper: run segments reinstalled so far: {}",
                runs.reinstalled()
            );
        }
        let until = millis().saturating_add(wait);
        while millis() < until {
            pump(&mut uart, &mut operator);
            board.delay.delay_millis(1);
        }
    }
}

/// The eight protocol pins, chip by chip.
#[cfg(feature = "esp32s3")]
macro_rules! board_pins {
    ($p:ident) => {
        [
            Output::new($p.GPIO4, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO5, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO6, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO7, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO15, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO16, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO17, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO18, Level::Low, OutputConfig::default()),
        ]
    };
}

/// UART0's pins, chip by chip.
#[cfg(feature = "esp32s3")]
macro_rules! uart_pins {
    ($uart:ident, $p:ident) => {
        $uart.with_tx($p.GPIO43).with_rx($p.GPIO44)
    };
}

/// The compile-check chip's pins: never flashed.
#[cfg(feature = "esp32c3")]
macro_rules! board_pins {
    ($p:ident) => {
        [
            Output::new($p.GPIO2, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO3, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO4, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO5, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO6, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO7, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO8, Level::Low, OutputConfig::default()),
            Output::new($p.GPIO10, Level::Low, OutputConfig::default()),
        ]
    };
}

/// The compile-check chip's UART0 pins: never flashed.
#[cfg(feature = "esp32c3")]
macro_rules! uart_pins {
    ($uart:ident, $p:ident) => {
        $uart.with_tx($p.GPIO21).with_rx($p.GPIO20)
    };
}

use {board_pins, uart_pins};
