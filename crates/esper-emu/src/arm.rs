//! The ARM startup: `cortex-m-rt`'s reset vector, semihosting output, and
//! the semihosting exit QEMU turns into a process status.

use core::panic::PanicInfo;

use cortex_m_rt::entry;
use cortex_m_semihosting::{debug, hprintln};

use crate::boot::{self, Report};
use crate::{PREFIX, halt};

/// Run the workload, report, and exit through semihosting.
#[entry]
fn main() -> ! {
    match boot::run() {
        Ok(report) => {
            print(&report);
            hprintln!("{} ok", PREFIX);
            debug::exit(debug::EXIT_SUCCESS);
        }
        Err(failure) => {
            hprintln!("{} failed {}", PREFIX, failure);
            debug::exit(debug::EXIT_FAILURE);
        }
    }
    // `debug::exit` does not return under QEMU. Reached only where
    // semihosting is absent, where hanging is the honest answer: exiting
    // zero would report a pass nobody could read.
    halt()
}

/// The lines the harness parses: one per stage, then the digest every
/// core must agree on.
fn print(report: &Report) {
    for stage in report.stages() {
        hprintln!("{} stage {}", PREFIX, stage);
    }
    hprintln!("{} digest {:016x}", PREFIX, report.digest);
}

/// A panic is a failed boot, said out loud.
#[panic_handler]
fn panic(info: &PanicInfo<'_>) -> ! {
    hprintln!("{} failed panic {}", PREFIX, info);
    debug::exit(debug::EXIT_FAILURE);
    halt()
}
