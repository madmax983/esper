# esper-s3 — the ESP32-S3 image

Glue only: `src/main.rs` hands `esper-board` the chip's SPI flash
(`esp-storage`), GPIO4–7 and GPIO15–18 as protocol pins 0–7, the boot
clock, and UART0 (GPIO43/44) as the operator channel, then loops:
boot the supervisor, sleep out its beat pumping the UART, boot again.
Logs and panics go to USB-Serial-JTAG. Anything with logic belongs in
`esper-board`, where the host tests reach it.

A workspace of its own on Espressif's `esp` toolchain
(`rust-toolchain.toml`; install with `espup install --targets esp32s3`).

```bash
. ~/export-esp.sh
cargo build --release             # xtensa-esp32s3-none-elf
cargo run --release               # espflash flash --monitor
```

Without that toolchain, the same glue compiles against the ESP32-C3 on
the root's pinned toolchain — a compile check, never flashed:

```bash
cargo +1.97 clippy --no-default-features --features esp32c3 --target riscv32imc-unknown-none-elf -- -D warnings
```

The plan starts at 4 MiB (`PLAN_BASE`) and takes 448 KiB; it assumes the
default 8 MiB partition layout leaves that region unused. Keep the
`[patch.crates-io]` revision equal to the root manifest's.
