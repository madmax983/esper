//! Esper's task runner. Host-only: nothing here ships on the device.
//!
//! ```text
//! cargo xtask emulate    link the firmware image, boot it under QEMU on
//!                        every machine, require every core to agree
//! ```

// The emulated image's workload, compiled for the host as well: the
// oracle every core's digest must equal.
#[path = "../../crates/esper-emu/src/boot.rs"]
mod boot;
mod emulate;
#[path = "../../crates/esper-emu/src/workload.rs"]
mod workload;

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("emulate") => match emulate::run() {
            Ok(summary) => {
                println!("{summary}");
                ExitCode::SUCCESS
            }
            Err(reason) => {
                eprintln!("xtask emulate: {reason}");
                ExitCode::FAILURE
            }
        },
        other => {
            eprintln!(
                "usage: cargo xtask emulate (got {})",
                other.unwrap_or("no task")
            );
            ExitCode::FAILURE
        }
    }
}
