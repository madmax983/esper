//! Esper firmware that starts under an emulator, runs, and says what it
//! did.
//!
//! A crate that compiles for a target is not a crate that runs on one.
//! `cargo build --lib` for a firmware target produces an rlib and never
//! links — no global allocator is required, so an `extern crate alloc`
//! anywhere underneath compiles clean. This image closes that: it is
//! linked with a reset vector, a vector table, and a memory map for two
//! QEMU machines (Cortex-M3 and Cortex-M4), started, and required to run
//! the workload in [`boot`] and report a digest the harness compares
//! across cores and against the same workload run on the host. `cargo xtask emulate` is the harness.
//!
//! It is not a board. No GPIO, no flash part, no RTC, no supply to
//! remove: a green emulated boot proves the firmware links, starts, and
//! computes the same answer on two instruction sets — never that a board
//! works.

#![no_std]
#![no_main]

mod boot;

#[cfg(target_arch = "arm")]
mod arm;

/// The prefix every line this image writes carries. The harness fails a
/// boot whose output lacks the lines it expects, so a guest that exits
/// zero having printed nothing is a failure, not a pass.
pub const PREFIX: &str = "esper-emu:";

/// Park the core: reached only where the semihosting exit did not exit.
pub fn halt() -> ! {
    loop {
        core::hint::spin_loop();
    }
}
