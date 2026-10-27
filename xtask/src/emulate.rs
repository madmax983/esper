//! The emulated boot: Esper's firmware image linked for real cores and
//! started under QEMU.
//!
//! `cargo check --no-default-features` proves the firmware code
//! *compiles* without `std`; it never links, and it runs on the host
//! target, so it cannot see a missing `core` symbol, an `alloc` that crept
//! in through a dependency, or a value that differs between a 32-bit core
//! and a 64-bit host. This links `esper-emu` — a reset vector, a memory
//! map and the workload in `crates/esper-emu/src/boot.rs` — for each of
//! [`MACHINES`], boots it, and requires:
//!
//! - QEMU exits zero;
//! - the image printed every stage the workload has, in order, then a
//!   digest, then `ok`;
//! - every core printed the **same** digest as the same workload run on
//!   the host.
//!
//! The host run is the oracle: the expected stages and digest are
//! computed here from the same source file, never transcribed, so a change
//! to the workload cannot drift from what the gate expects.
//!
//! # How it fails closed
//!
//! QEMU missing, a target not pinned in `rust-toolchain.toml`, a build
//! failure, a non-zero exit, a boot that outlives [`TIMEOUT`], and a boot
//! that exited zero having printed nothing are all failures, never skips.
//!
//! # What it does not establish
//!
//! A board. Neither machine is an ESP32-S3: they are ARMv7-M and ARMv7E-M
//! cores with 512 KiB of RAM (about the S3's SRAM), chosen because a
//! clean CI runner can emulate them. The Xtensa target needs the
//! Espressif toolchain and QEMU fork, which no clean runner has.

use std::fmt::Write as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::boot;

/// The package that builds the image.
const PACKAGE: &str = "esper-emu";

/// The prefix the image puts on every line the harness reads.
const PREFIX: &str = "esper-emu:";

/// How long one boot may run. The workload finishes in well under a
/// second; a boot that runs this long has hung.
pub const TIMEOUT: Duration = Duration::from_mins(1);

/// One emulated core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Machine {
    /// What the report calls it.
    pub name: &'static str,
    /// The Rust target the image is built for.
    pub target: &'static str,
    /// QEMU's `-machine`.
    pub board: &'static str,
    /// QEMU's `-cpu`.
    pub cpu: &'static str,
}

/// The cores every run boots. Two encodings (ARMv7-M, ARMv7E-M with the
/// DSP extension), so agreement is between two code generators.
pub const MACHINES: [Machine; 2] = [
    Machine {
        name: "Cortex-M3",
        target: "thumbv7m-none-eabi",
        board: "mps2-an385",
        cpu: "cortex-m3",
    },
    Machine {
        name: "Cortex-M4",
        target: "thumbv7em-none-eabi",
        board: "mps2-an386",
        cpu: "cortex-m4",
    },
];

/// What one boot reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    /// The stages it passed, in order.
    pub stages: Vec<String>,
    /// The digest over every stage's output.
    pub digest: u64,
}

/// Read a boot's console.
///
/// # Errors
///
/// The image reported a failure, or its output is incomplete: no `ok`,
/// no digest, more than one digest, or anything after `ok`.
pub fn parse(output: &str) -> Result<Boot, String> {
    let mut stages = Vec::new();
    let mut digest = None;
    let mut ok = false;
    for line in output.lines() {
        let Some(rest) = line.trim_end().strip_prefix(PREFIX) else {
            continue;
        };
        let rest = rest.trim_start();
        if ok {
            return Err(format!("output after `ok`: {rest}"));
        }
        if let Some(reason) = rest.strip_prefix("failed ") {
            return Err(format!("the image failed: {reason}"));
        } else if let Some(stage) = rest.strip_prefix("stage ") {
            stages.push(stage.to_owned());
        } else if let Some(hex) = rest.strip_prefix("digest ") {
            if digest.is_some() {
                return Err("more than one digest".to_owned());
            }
            digest = Some(
                u64::from_str_radix(hex, 16).map_err(|_| format!("unreadable digest `{hex}`"))?,
            );
        } else if rest == "ok" {
            ok = true;
        } else {
            return Err(format!("unrecognised line: {rest}"));
        }
    }
    if !ok {
        return Err("the image never printed `ok`".to_owned());
    }
    let digest = digest.ok_or("the image printed no digest")?;
    Ok(Boot { stages, digest })
}

/// The workload run on the host: what every core must agree with.
///
/// # Errors
///
/// The workload itself fails on the host.
pub fn oracle() -> Result<Boot, String> {
    let report = boot::run().map_err(|reason| format!("the host oracle failed: {reason}"))?;
    Ok(Boot {
        stages: report.stages().map(str::to_owned).collect(),
        digest: report.digest,
    })
}

/// Every machine's target must be pinned in `rust-toolchain.toml`, so a
/// checkout installs what CI boots.
///
/// # Errors
///
/// The first target the toolchain file does not name.
pub fn check_pinned(toolchain: &str) -> Result<(), String> {
    for machine in MACHINES {
        let quoted = format!("\"{}\"", machine.target);
        if !toolchain.contains(&quoted) {
            return Err(format!(
                "{} is not pinned in rust-toolchain.toml; {} would boot nothing",
                machine.target, machine.name
            ));
        }
    }
    Ok(())
}

/// Judge one core's boot against the host's.
///
/// # Errors
///
/// The stages or the digest differ.
pub fn judge(machine: Machine, boot: &Boot, oracle: &Boot) -> Result<(), String> {
    if boot.stages != oracle.stages {
        return Err(format!(
            "{} passed stages {:?}, the host passed {:?}",
            machine.name, boot.stages, oracle.stages
        ));
    }
    if boot.digest != oracle.digest {
        return Err(format!(
            "{} computed digest {:016x}, the host computed {:016x}",
            machine.name, boot.digest, oracle.digest
        ));
    }
    Ok(())
}

/// Build and boot every machine.
///
/// # Errors
///
/// Any way the run declined to be a measurement; see the module docs.
pub fn run() -> Result<String, String> {
    let root = workspace_root()?;
    let toolchain = std::fs::read_to_string(root.join("rust-toolchain.toml"))
        .map_err(|e| format!("reading rust-toolchain.toml: {e}"))?;
    check_pinned(&toolchain)?;
    which_qemu()?;
    let oracle = oracle()?;
    let mut summary = format!(
        "host: {} stages, digest {:016x}",
        oracle.stages.len(),
        oracle.digest
    );
    for machine in MACHINES {
        let image = build(&root, machine)?;
        let output = start(machine, &image)?;
        let boot = parse(&output).map_err(|reason| format!("{}: {reason}", machine.name))?;
        judge(machine, &boot, &oracle)?;
        let _ = write!(
            summary,
            "\n{} ({} on {}): {} stages, digest {:016x}, agrees",
            machine.name,
            machine.target,
            machine.board,
            boot.stages.len(),
            boot.digest
        );
    }
    Ok(summary)
}

fn workspace_root() -> Result<PathBuf, String> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "xtask has no parent directory".to_owned())
}

fn which_qemu() -> Result<(), String> {
    let status = Command::new("qemu-system-arm")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| format!("qemu-system-arm is not installed: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("qemu-system-arm --version exited {status}"))
    }
}

fn build(root: &Path, machine: Machine) -> Result<PathBuf, String> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .current_dir(root)
        .args([
            "build",
            "--locked",
            "--release",
            "-p",
            PACKAGE,
            "--features",
            "emu",
            "--target",
            machine.target,
        ])
        .status()
        .map_err(|e| format!("spawning cargo for {}: {e}", machine.name))?;
    if !status.success() {
        return Err(format!("building the image for {} failed", machine.name));
    }
    let target_dir =
        std::env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    let image = target_dir
        .join(machine.target)
        .join("release")
        .join(PACKAGE);
    if image.is_file() {
        Ok(image)
    } else {
        Err(format!("the build left no image at {}", image.display()))
    }
}

fn start(machine: Machine, image: &Path) -> Result<String, String> {
    let mut child = Command::new("qemu-system-arm")
        .args([
            "-nographic",
            "-machine",
            machine.board,
            "-cpu",
            machine.cpu,
            "-semihosting-config",
            "enable=on,target=native",
            "-kernel",
        ])
        .arg(image)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("starting QEMU for {}: {e}", machine.name))?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or("QEMU's stdout was not captured")?;
    let reader = std::thread::spawn(move || {
        let mut text = String::new();
        stdout.read_to_string(&mut text).map(|_| text)
    });
    let deadline = Instant::now() + TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .map_err(|e| format!("waiting on QEMU for {}: {e}", machine.name))?
        {
            break status;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "{} did not finish within {}s",
                machine.name,
                TIMEOUT.as_secs()
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    };
    let output = reader
        .join()
        .map_err(|_| "the console reader panicked".to_owned())?
        .map_err(|e| format!("reading {}'s console: {e}", machine.name))?;
    print!("{output}");
    if !status.success() {
        let mut stderr = String::new();
        if let Some(mut pipe) = child.stderr.take() {
            let _ = pipe.read_to_string(&mut stderr);
        }
        return Err(format!(
            "{} exited {status}: {}",
            machine.name,
            stderr.trim()
        ));
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn console(lines: &[&str]) -> String {
        lines.iter().fold(String::new(), |mut text, line| {
            let _ = writeln!(text, "{PREFIX} {line}");
            text
        })
    }

    #[test]
    fn a_complete_boot_parses() {
        let boot = parse(&console(&["stage a", "stage b", "digest 00ff", "ok"])).unwrap();
        assert_eq!(boot.stages, ["a", "b"]);
        assert_eq!(boot.digest, 0xff);
    }

    #[test]
    fn unprefixed_lines_are_ignored() {
        let text = format!("qemu noise\n{}", console(&["digest 1", "ok"]));
        assert_eq!(parse(&text).unwrap().digest, 1);
    }

    #[test]
    fn a_silent_boot_fails() {
        assert!(parse("").is_err());
        assert!(parse("booting\n").is_err());
    }

    #[test]
    fn a_failure_line_fails_with_its_reason() {
        let err = parse(&console(&["stage a", "failed mask: secret survived"])).unwrap_err();
        assert!(err.contains("mask: secret survived"), "{err}");
    }

    #[test]
    fn incomplete_or_ambiguous_output_fails() {
        assert!(parse(&console(&["stage a", "digest 1"])).is_err(), "no ok");
        assert!(parse(&console(&["stage a", "ok"])).is_err(), "no digest");
        assert!(parse(&console(&["digest 1", "digest 2", "ok"])).is_err());
        assert!(parse(&console(&["digest zz", "ok"])).is_err());
        assert!(parse(&console(&["digest 1", "ok", "stage late"])).is_err());
        assert!(parse(&console(&["digest 1", "mystery", "ok"])).is_err());
    }

    #[test]
    fn the_host_oracle_passes_every_stage() {
        let oracle = oracle().unwrap();
        assert_eq!(
            oracle.stages,
            [
                "prompt",
                "infer",
                "decode",
                "mask",
                "checkpoint",
                "wire",
                "run",
                "agent"
            ]
        );
    }

    #[test]
    fn the_oracle_is_deterministic() {
        assert_eq!(oracle().unwrap(), oracle().unwrap());
    }

    #[test]
    fn judge_requires_stage_and_digest_agreement() {
        let host = Boot {
            stages: vec!["a".to_owned()],
            digest: 7,
        };
        let core = MACHINES[0];
        assert!(judge(core, &host.clone(), &host).is_ok());
        let wrong_digest = Boot {
            digest: 8,
            ..host.clone()
        };
        assert!(judge(core, &wrong_digest, &host).is_err());
        let fewer = Boot {
            stages: Vec::new(),
            digest: host.digest,
        };
        assert!(judge(core, &fewer, &host).is_err());
    }

    #[test]
    fn every_machine_must_be_pinned() {
        let all = r#"targets = ["thumbv7m-none-eabi", "thumbv7em-none-eabi"]"#;
        assert!(check_pinned(all).is_ok());
        assert!(check_pinned(r#"targets = ["thumbv7m-none-eabi"]"#).is_err());
    }

    #[test]
    fn the_checked_in_toolchain_pins_every_machine() {
        let root = workspace_root().unwrap();
        let toolchain = std::fs::read_to_string(root.join("rust-toolchain.toml")).unwrap();
        check_pinned(&toolchain).unwrap();
    }
}
