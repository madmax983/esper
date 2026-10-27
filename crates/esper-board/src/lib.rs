//! The ESP32-S3 board's product half: Esper's supervisor and its runs on
//! flash, behind the two traits SPEC §23 says a board supplies.
//!
//! Everything here is `no_std`, allocation-free and chip-independent: it
//! is written against `embedded-storage`'s [`NorFlash`] and the small
//! [`Hardware`] trait, so it is tested on the host over a RAM chip and
//! linked unchanged into the S3 image (`boards/esp32s3`), whose only job
//! is to hand it the chip's flash, pins, clock and UART.
//!
//! | Module | Owns |
//! |---|---|
//! | [`flash`] | [`Partition`]: Waymaker's storage contract over a window of a shared NOR chip |
//! | [`slot`] | a bank pair's first install, and reading its authority and terminal record |
//! | [`runs`] | [`FlashRuns`]: the board's `RunStore`, one slot per run |
//! | [`agent`] | [`Supervisor`]: the supervisor booted on its own slot |
//! | [`items`] | [`ItemLog`]: checkpoint items and snapshot chunks on flash |
//! | [`operator`] | [`Operator`]: the UART line protocol |
//! | [`ports`] | [`BoardPorts`]: `RunPorts` + `DevicePorts` over [`Hardware`] |
//! | [`plan`] | [`Plan`]: where each of those lives on the chip |
//! | [`demo`] | the goal the board answers, and its distilled model |
//!
//! Waymaker is the durable store: every journal here is a Waymaker bank
//! pair driven by `waymaker_drive::Driver`, and every workflow reaches it
//! through `waymaker_bridge::Bridge` (ADR 0005). The item logs hold what
//! an activity hands the world to keep across a `continue_as_new`.
//!
//! [`NorFlash`]: embedded_storage::nor_flash::NorFlash

#![no_std]
#![deny(unsafe_code)]
#![warn(missing_docs)]

pub mod agent;
pub mod demo;
pub mod flash;
pub mod items;
pub mod operator;
pub mod plan;
pub mod ports;
#[cfg(feature = "host")]
pub mod ram;
pub mod runs;
pub mod slot;

pub use agent::{BoardFault, Booted, Supervisor};
pub use flash::Partition;
pub use items::ItemLog;
pub use operator::Operator;
pub use plan::Plan;
pub use ports::{BoardPorts, Hardware};
pub use runs::FlashRuns;

#[cfg(all(test, feature = "host"))]
mod tests {
    use core::cell::RefCell;

    use waymaker_core::timer::{ClockCapability, ClockKind};
    use waymaker_drive::Clocks;
    use waymaker_flash::storage::{Geometry, StableStorage};

    use super::*;
    use crate::flash::{LayoutFault, PartitionError};
    use crate::plan::RUN_SLOTS;
    use crate::ram::RamNor;

    struct NoClock;

    impl Clocks for NoClock {
        fn capability(&self) -> ClockCapability {
            ClockCapability::BootOnly
        }
        fn now(&mut self, _kind: ClockKind) -> Option<u64> {
            None
        }
    }

    type Chip = RamNor<{ 64 * 1024 }>;

    /// Board state is caller-owned (a `static` on the S3) and RAM is
    /// measured: these pin what each piece costs.
    #[test]
    fn the_board_state_sizes_are_pinned() {
        assert_eq!(size_of::<Operator>(), SIZES.0);
        assert_eq!(size_of::<Supervisor<'static, Chip, NoClock>>(), SIZES.1);
        assert_eq!(
            size_of::<FlashRuns<'static, Chip, NoClock, RUN_SLOTS>>(),
            SIZES.2
        );
        assert_eq!(size_of::<ItemLog<Partition<'static, Chip>>>(), SIZES.3);
    }

    /// [`Operator`], [`Supervisor`], [`FlashRuns`], [`ItemLog`] — on a 64-bit host. The
    /// two drivers' scratch (a 1 KiB page and a 480 B result each) is most
    /// of the middle two; the operator is its line buffer and four held
    /// inputs.
    const SIZES: (usize, usize, usize, usize) = (1384, 1600, 1736, 32);

    #[test]
    fn a_partition_refuses_a_window_the_chip_cannot_hold() {
        let chip = RefCell::new(Chip::new());
        let geometry = Geometry::new(8 * 1024, 4096, 4, 4).expect("legal");
        assert!(Partition::new(&chip, 56 * 1024, geometry).is_ok());
        assert_eq!(
            Partition::new(&chip, 60 * 1024, geometry).err(),
            Some(LayoutFault::Bounds),
            "runs past the chip"
        );
        assert_eq!(
            Partition::new(&chip, 2048, geometry).err(),
            Some(LayoutFault::Bounds),
            "not on an erase block"
        );
        let coarse = Geometry::new(8 * 1024, 4096, 2, 2).expect("legal");
        assert_eq!(
            Partition::new(&chip, 0, coarse).err(),
            Some(LayoutFault::Units),
            "a program unit the chip cannot write"
        );
    }

    #[test]
    fn a_partition_offsets_validates_and_refuses_a_lent_chip() {
        let chip = RefCell::new(Chip::new());
        let geometry = Geometry::new(8 * 1024, 4096, 4, 4).expect("legal");
        let mut low = Partition::new(&chip, 0, geometry).expect("fits");
        let mut high = Partition::new(&chip, 8 * 1024, geometry).expect("fits");
        high.program(0, &[0x12, 0x34, 0x56, 0x78])
            .expect("programs");
        let mut out = [0u8; 4];
        low.read(0, &mut out).expect("reads");
        assert_eq!(out, [0xFF; 4], "the windows do not overlap");
        high.read(0, &mut out).expect("reads");
        assert_eq!(out, [0x12, 0x34, 0x56, 0x78]);
        assert_eq!(chip.borrow().image()[8 * 1024], 0x12, "offset by the base");
        assert!(matches!(
            high.program(2, &[0; 4]),
            Err(PartitionError::Geometry(_))
        ));
        let _lent = chip.borrow_mut();
        assert_eq!(low.read(0, &mut out), Err(PartitionError::Busy));
    }
}
