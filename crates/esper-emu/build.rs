//! Link the emulated image with `cortex-m-rt`'s `link.x`, which `INCLUDE`s
//! this crate's `memory.x`.
//!
//! Emitted for the binary only, and only on ARM: a workspace-wide
//! `rustflags` entry would put the script on every firmware build.

use std::env;

fn main() {
    println!("cargo:rerun-if-changed=memory.x");
    println!("cargo:rerun-if-changed=build.rs");
    if env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm") {
        if let Ok(dir) = env::var("CARGO_MANIFEST_DIR") {
            println!("cargo:rustc-link-search={dir}");
        }
        println!("cargo:rustc-link-arg-bins=-Tlink.x");
    }
}
