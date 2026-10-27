# esper-emu — the emulated boot image

A linked `no_std` firmware image (`cortex-m-rt` reset vector, 512 KiB
flash / 512 KiB RAM in `memory.x`, about the ESP32-S3's SRAM) that runs
Esper's firmware code paths on a real core under QEMU and reports over
semihosting. `cargo xtask emulate` builds it for Cortex-M3 and
Cortex-M4, boots both, and requires every stage and the digest to match
the same workload run on the host (`xtask` includes `src/boot.rs`
directly as its oracle).

## Rules

- **`src/workload.rs`** holds the doubles (a fixed-capacity journal, an
  8-pin device, a scripted model, a one-run `RunStore`) behind the
  product dispatchers: the `run` stage drives `run::run` through
  `RunDispatcher`; the `agent` stage drives `supervise` through the
  board `Device`. Both go through one reset. `xtask` includes it next
  to `boot.rs`; keep both target-independent.
- **`src/boot.rs` is target-independent.** It runs on the host as the
  oracle and on each core. No `cfg(target_arch)` in it; anything
  ARM-specific lives in `src/arm.rs`.
- **Every stage runs real product code.** A stage that checks a double
  of the thing it names proves nothing. Fail with a named reason
  (`"<stage>: <what was wrong>"`); never `unwrap`.
- **Adding a stage** means adding it to `STAGES`, folding its output into
  the digest, and updating `xtask`'s `the_host_oracle_passes_every_stage`.
  The digest is computed, never pinned.
- **No secret-shaped literals.** Assemble them at runtime (see the mask
  stage); push protection rejects them otherwise.
- The binary is behind `required-features = ["emu"]`, so host clippy
  never sees it; CI lints it with
  `cargo clippy -p esper-emu --target thumbv7em-none-eabi --features emu --bins`.

## Not a board

Neither QEMU machine is an ESP32-S3 (Xtensa LX7). A green boot here says
the firmware links and computes the same answers on two ARM encodings
and the host. It says nothing about the S3's flash, reset causes or
timing.
