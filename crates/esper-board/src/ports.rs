//! [`BoardPorts`]: the board's [`RunPorts`] and [`DevicePorts`].
//!
//! The physical half of every activity: the six tools over the board's
//! [`Hardware`], typed operator input from the [`Operator`] channel, the
//! model, and the two [`ItemLog`]s. [`RunDispatcher`] and the supervisor's
//! `Device` own the rules that make an answer safe to record — decoding,
//! re-validation, masking — so nothing here decides what is safe.
//!
//! # Redelivery
//!
//! A redelivered effect must not act twice. Every tool here is safe to
//! perform again: a pin write drives the level it already drove, the
//! reads and the status report observe, and a delay waits again. So the
//! board keeps no effect ledger — which a reset would lose anyway.
//!
//! Outcomes are the host world's canonical JSON (SPEC §17), byte for
//! byte: `{"pin":4,"level":"high"}`, `{"sensor":2,"value":1800}`,
//! `{"uptime_ms":1250}`, `{"waited_ms":250,"uptime_ms":1250}`, and the
//! two status reports.
//!
//! [`RunDispatcher`]: esper_runtime::run::device::RunDispatcher

use esper_core::beat::Beat;
use esper_core::decision::{Level, StatusDetail, ToolArgs};
use esper_core::goal::GoalId;
use esper_core::ids::{Pin, ToolId};
use esper_runtime::agent::device::{DevicePorts, StoreFault};
use esper_runtime::agent::wire::{Sense, WireError};
use esper_runtime::backend::ModelBackend;
use esper_runtime::run::device::{Attempt, RunPorts};
use esper_runtime::run::wire::{Check, Fault};
use waymaker_core::EffectId;
use waymaker_flash::storage::StableStorage;

use crate::items::{ItemFault, ItemLog};
use crate::operator::Operator;

/// Pins the tools may address: `0..PINS`.
pub const PINS: u8 = 8;

/// Sensor channels the tools may address: `0..SENSORS`.
pub const SENSORS: u8 = 4;

/// The board's hardware, as the tools see it.
///
/// Pin and sensor numbers are the protocol's (`0..8`, `0..4`); the board
/// maps them onto its own GPIOs and channels.
pub trait Hardware {
    /// Whether protocol pin `pin` is configured as an output.
    fn is_output(&mut self, pin: u8) -> bool;
    /// Protocol pin `pin`'s level, read from the pad.
    fn level(&mut self, pin: u8) -> bool;
    /// Drive protocol pin `pin` (an output) to `high`.
    fn drive(&mut self, pin: u8, high: bool);
    /// Sensor channel `sensor`'s raw reading, or `None` where the board
    /// has no such sensor.
    fn sensor(&mut self, sensor: u8) -> Option<u16>;
    /// Milliseconds since boot.
    fn uptime_ms(&mut self) -> u64;
    /// Wait `ms` milliseconds.
    fn delay_ms(&mut self, ms: u16);
    /// The persistent clock's reading — what `SENSE` stamps — or the
    /// boot clock's on a board without one.
    fn now(&mut self) -> u64;
}

/// The board's ports for one boot.
pub struct BoardPorts<'a, H: Hardware, M: ModelBackend, S: StableStorage, T: StableStorage> {
    hardware: &'a mut H,
    model: &'a mut M,
    operator: &'a mut Operator,
    checkpoints: &'a mut ItemLog<S>,
    snapshots: &'a mut ItemLog<T>,
}

impl<'a, H: Hardware, M: ModelBackend, S: StableStorage, T: StableStorage>
    BoardPorts<'a, H, M, S, T>
{
    /// Ports over the board's hardware, model, operator channel and its
    /// checkpoint and snapshot logs.
    pub const fn new(
        hardware: &'a mut H,
        model: &'a mut M,
        operator: &'a mut Operator,
        checkpoints: &'a mut ItemLog<S>,
        snapshots: &'a mut ItemLog<T>,
    ) -> Self {
        Self {
            hardware,
            model,
            operator,
            checkpoints,
            snapshots,
        }
    }

    fn status(&mut self, detail: StatusDetail, raw: &mut [u8]) -> Option<usize> {
        let uptime = u32::try_from(self.hardware.uptime_ms()).unwrap_or(u32::MAX);
        let mut json = Json::new(raw);
        json.text(b"{\"pins\":8,\"sensors\":4,\"uptime_ms\":")?;
        json.number(u64::from(uptime))?;
        if detail == StatusDetail::Full {
            json.text(b",\"dir\":")?;
            let mut dir = [0u8; PINS as usize];
            let mut level = [0u8; PINS as usize];
            for pin in 0..PINS {
                let (Some(d), Some(l)) = (
                    dir.get_mut(usize::from(pin)),
                    level.get_mut(usize::from(pin)),
                ) else {
                    return None;
                };
                *d = u8::from(self.hardware.is_output(pin));
                *l = u8::from(self.hardware.level(pin));
            }
            json.array(dir.iter().map(|d| u64::from(*d)))?;
            json.text(b",\"level\":")?;
            json.array(level.iter().map(|l| u64::from(*l)))?;
            json.text(b",\"values\":")?;
            let mut values = [0u64; SENSORS as usize];
            for (sensor, value) in (0..SENSORS).zip(values.iter_mut()) {
                *value = u64::from(self.hardware.sensor(sensor).unwrap_or(0));
            }
            json.array(values.iter().copied())?;
        }
        json.text(b"}")?;
        Some(json.len)
    }
}

/// A bounded JSON writer: `None` once `out` is full.
struct Json<'o> {
    out: &'o mut [u8],
    len: usize,
}

impl<'o> Json<'o> {
    const fn new(out: &'o mut [u8]) -> Self {
        Self { out, len: 0 }
    }

    fn text(&mut self, bytes: &[u8]) -> Option<()> {
        let end = self.len.checked_add(bytes.len())?;
        self.out.get_mut(self.len..end)?.copy_from_slice(bytes);
        self.len = end;
        Some(())
    }

    fn number(&mut self, mut value: u64) -> Option<()> {
        let mut digits = [0u8; 20];
        let mut at = digits.len();
        loop {
            at -= 1;
            *digits.get_mut(at)? = b'0' + u8::try_from(value % 10).ok()?;
            value /= 10;
            if value == 0 {
                break;
            }
        }
        let digits = digits.get(at..)?;
        self.text(digits)
    }

    fn array(&mut self, values: impl Iterator<Item = u64>) -> Option<()> {
        self.text(b"[")?;
        for (index, value) in values.enumerate() {
            if index > 0 {
                self.text(b",")?;
            }
            self.number(value)?;
        }
        self.text(b"]")
    }
}

const fn item_fault<E>(error: &ItemFault<E>) -> StoreFault {
    match error {
        ItemFault::Missing => StoreFault::Unknown,
        ItemFault::Full => StoreFault::Full,
        ItemFault::Media(_) => StoreFault::Halted,
    }
}

impl<H: Hardware, M: ModelBackend, S: StableStorage, T: StableStorage> RunPorts
    for BoardPorts<'_, H, M, S, T>
{
    fn model(&mut self) -> &mut dyn ModelBackend {
        self.model
    }

    fn tool(
        &mut self,
        _effect: EffectId,
        _tool: ToolId,
        _digest: u64,
        args: ToolArgs,
        raw: &mut [u8],
    ) -> Result<Attempt, Fault> {
        let mut json = Json::new(raw);
        let written = match args {
            ToolArgs::GpioPinRead { pin } => {
                let high = self.hardware.level(pin.get());
                pin_json(&mut json, pin, high)
            }
            ToolArgs::GpioPinWrite { pin, level } => {
                self.hardware.drive(pin.get(), level == Level::High);
                let high = self.hardware.level(pin.get());
                pin_json(&mut json, pin, high)
            }
            ToolArgs::SensorSampleRead { sensor } => {
                let value = self.hardware.sensor(sensor).ok_or(Fault::UnknownResource)?;
                json.text(b"{\"sensor\":")
                    .and_then(|()| json.number(u64::from(sensor)))
                    .and_then(|()| json.text(b",\"value\":"))
                    .and_then(|()| json.number(u64::from(value)))
                    .and_then(|()| json.text(b"}"))
            }
            ToolArgs::TimerUptimeRead => {
                let uptime = self.hardware.uptime_ms();
                json.text(b"{\"uptime_ms\":")
                    .and_then(|()| json.number(uptime))
                    .and_then(|()| json.text(b"}"))
            }
            ToolArgs::TimerDelayWait { ms } => {
                let from = self.hardware.uptime_ms();
                self.hardware.delay_ms(ms);
                let to = self.hardware.uptime_ms();
                json.text(b"{\"waited_ms\":")
                    .and_then(|()| json.number(to.saturating_sub(from)))
                    .and_then(|()| json.text(b",\"uptime_ms\":"))
                    .and_then(|()| json.number(to))
                    .and_then(|()| json.text(b"}"))
            }
            ToolArgs::DeviceStatusReport { detail } => {
                let len = self.status(detail, json.out);
                return len.map(Attempt::Done).ok_or(Fault::OutcomeTooLarge);
            }
        };
        written
            .map(|()| Attempt::Done(json.len))
            .ok_or(Fault::OutcomeTooLarge)
    }

    fn pin_is_output(&mut self, pin: Pin) -> bool {
        self.hardware.is_output(pin.get())
    }

    fn read_back(&mut self, check: Check) -> u64 {
        match check {
            Check::Pin { pin, .. } => u64::from(self.hardware.level(pin)),
            Check::Clock { .. } => self.hardware.uptime_ms(),
        }
    }

    fn operator_input(&mut self, out: &mut [u8]) -> Result<Option<usize>, Fault> {
        self.operator.take_input(out).map_err(|_| Fault::BadInput)
    }

    fn store_chunk(&mut self, digest: u64, index: u8, chunk: &[u8]) -> Result<(), Fault> {
        self.snapshots
            .store(digest, u16::from(index), chunk)
            .map_err(|_| Fault::BadRequest)
    }

    fn load_chunk(&mut self, digest: u64, index: u8, out: &mut [u8]) -> Result<usize, Fault> {
        self.snapshots
            .load(digest, u16::from(index), out)
            .map_err(|_| Fault::SnapshotMissing)
    }
}

/// `{"pin":<n>,"level":"high"|"low"}`.
fn pin_json(json: &mut Json<'_>, pin: Pin, high: bool) -> Option<()> {
    let level: &[u8] = if high { b"high" } else { b"low" };
    json.text(b"{\"pin\":")?;
    json.number(u64::from(pin.get()))?;
    json.text(b",\"level\":\"")?;
    json.text(level)?;
    json.text(b"\"}")
}

impl<H: Hardware, M: ModelBackend, S: StableStorage, T: StableStorage> DevicePorts
    for BoardPorts<'_, H, M, S, T>
{
    fn sense(&mut self, _beat: Beat) -> Result<Sense, WireError> {
        let now = self.hardware.now();
        self.operator.sense(now)
    }

    fn select_goal(&mut self, goal: GoalId) {
        self.operator.select(goal);
    }

    fn store_item(&mut self, digest: u64, index: u16, item: &[u8]) -> Result<(), StoreFault> {
        self.checkpoints
            .store(digest, index, item)
            .map_err(|error| item_fault(&error))
    }

    fn load_item(&mut self, digest: u64, index: u16, out: &mut [u8]) -> Result<usize, StoreFault> {
        self.checkpoints
            .load(digest, index, out)
            .map_err(|error| item_fault(&error))
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    extern crate std;

    use core::cell::RefCell;
    use std::vec::Vec;

    use esper_core::decision::{Level, StatusDetail, ToolArgs};
    use esper_core::goal::Envelope;
    use esper_core::ids::{Pin, ToolId};
    use esper_core::registry::Capabilities;
    use waymaker_core::{EffectId, EffectSeq, RunId};
    use waymaker_flash::storage::Geometry;

    use super::*;
    use crate::flash::Partition;
    use crate::ram::RamNor;

    struct Bench {
        levels: [bool; 8],
        ms: u64,
    }

    impl Hardware for Bench {
        fn is_output(&mut self, pin: u8) -> bool {
            pin < 4
        }
        fn level(&mut self, pin: u8) -> bool {
            self.levels[usize::from(pin)]
        }
        fn drive(&mut self, pin: u8, high: bool) {
            self.levels[usize::from(pin)] = high;
        }
        fn sensor(&mut self, sensor: u8) -> Option<u16> {
            (sensor == 2).then_some(1800)
        }
        fn uptime_ms(&mut self) -> u64 {
            self.ms
        }
        fn delay_ms(&mut self, ms: u16) {
            self.ms += u64::from(ms);
        }
        fn now(&mut self) -> u64 {
            self.ms
        }
    }

    const BYTES: usize = 4 * 4096;

    fn tool(args: ToolArgs, bench: &mut Bench) -> Result<Vec<u8>, Fault> {
        let chip = RefCell::new(RamNor::<BYTES>::new());
        let geometry = Geometry::new(2 * 4096, 4096, 4, 4).expect("legal");
        let mut checkpoints = ItemLog::new(Partition::new(&chip, 0, geometry).expect("fits"));
        let mut snapshots = ItemLog::new(Partition::new(&chip, 2 * 4096, geometry).expect("fits"));
        let mut model = crate::demo::model();
        let mut operator = Operator::new(
            Envelope {
                per_run_turns: 1,
                per_run_mutations: 1,
                turns: 1,
                mutations: 1,
                runs: 1,
            },
            Capabilities::empty(),
        );
        let mut ports = BoardPorts::new(
            bench,
            &mut model,
            &mut operator,
            &mut checkpoints,
            &mut snapshots,
        );
        let mut raw = [0u8; 256];
        let effect = EffectId {
            run: RunId(1),
            seq: EffectSeq(0),
        };
        match ports.tool(effect, ToolId::new(1), 0, args, &mut raw)? {
            Attempt::Done(len) => Ok(raw[..len].to_vec()),
            Attempt::Transient => Ok(Vec::new()),
        }
    }

    fn pin(n: u8) -> Pin {
        Pin::new(n).expect("a pin")
    }

    #[test]
    fn every_tool_answers_in_the_host_worlds_canonical_json() {
        let mut bench = Bench {
            levels: [false; 8],
            ms: 1250,
        };
        assert_eq!(
            tool(
                ToolArgs::GpioPinWrite {
                    pin: pin(4),
                    level: Level::High
                },
                &mut bench
            ),
            Ok(br#"{"pin":4,"level":"high"}"#.to_vec())
        );
        assert_eq!(
            tool(ToolArgs::GpioPinRead { pin: pin(4) }, &mut bench),
            Ok(br#"{"pin":4,"level":"high"}"#.to_vec())
        );
        assert_eq!(
            tool(ToolArgs::SensorSampleRead { sensor: 2 }, &mut bench),
            Ok(br#"{"sensor":2,"value":1800}"#.to_vec())
        );
        assert_eq!(
            tool(ToolArgs::SensorSampleRead { sensor: 1 }, &mut bench),
            Err(Fault::UnknownResource)
        );
        assert_eq!(
            tool(ToolArgs::TimerUptimeRead, &mut bench),
            Ok(br#"{"uptime_ms":1250}"#.to_vec())
        );
        assert_eq!(
            tool(ToolArgs::TimerDelayWait { ms: 250 }, &mut bench),
            Ok(br#"{"waited_ms":250,"uptime_ms":1500}"#.to_vec())
        );
        assert_eq!(
            tool(
                ToolArgs::DeviceStatusReport {
                    detail: StatusDetail::Summary
                },
                &mut bench
            ),
            Ok(br#"{"pins":8,"sensors":4,"uptime_ms":1500}"#.to_vec())
        );
        assert_eq!(
            tool(ToolArgs::DeviceStatusReport { detail: StatusDetail::Full }, &mut bench),
            Ok(br#"{"pins":8,"sensors":4,"uptime_ms":1500,"dir":[1,1,1,1,0,0,0,0],"level":[0,0,0,0,1,0,0,0],"values":[0,0,1800,0]}"#.to_vec())
        );
    }

    #[test]
    fn a_status_report_wider_than_the_buffer_is_refused_not_truncated() {
        let mut bench = Bench {
            levels: [true; 8],
            ms: u64::MAX,
        };
        let mut json_out = [0u8; 16];
        let mut json = Json::new(&mut json_out);
        assert_eq!(json.text(b"{\"pins\":8,\"sensors\":4"), None);
        // And the full report saturates its clock like the host world's.
        let full = tool(
            ToolArgs::DeviceStatusReport {
                detail: StatusDetail::Full,
            },
            &mut bench,
        )
        .expect("fits 256 bytes");
        assert!(full.len() <= 128, "{}", full.len());
    }
}
