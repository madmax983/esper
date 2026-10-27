# Esper — workspace rules

Esper is a durable agent harness **for firmware**: the ReAct loop as a
Waymaker workflow, shipped on the device (target: ESP32-S3,
`xtensa-esp32s3-none-elf`). Read `README.md` for the field guide and
`SPEC.md` for the contract; each crate has its own `CLAUDE.md`.

## Firmware-first (non-negotiable)

- **New runtime behavior ships on the device.** It must compile under
  `cargo check -p esper-runtime --no-default-features`, with no `alloc`
  and no `std`. If a feature only exists behind the `host` feature, it
  does not exist.
- **Build on Waymaker's primitives, not beside them.** Workflows are
  `async fn`s over `waymaker_embassy::Ctx`: effects are `ctx.activity`,
  waits are `ctx.timer` (durable deadlines; `AtPersistentTime` needs an
  RTC, `AfterBoot` restarts after a reset), history is bounded with
  `ctx.continue_as_new`. Waymaker's journal is the durable store — do
  not invent a second one.
- **Every record fits a scratch page.** Waymaker states device RAM with
  a 512-byte scratch page (`waymaker_core::budget::SCRATCH_PAGE_BYTES`).
  Activity inputs, activity answers, and `continue_as_new` inputs must
  each fit one page; large state is split into page-sized items. Pin
  every encoder's maximum with a test.
- **Bounded RAM, measured.** Fixed capacities, no heap. Large state is
  caller-owned (so it can live in a `static`), not hidden inside a
  future. Pin the size of new public state types with a test.
- **No async runtime in product code.** Embassy's executor polls plain
  `core::future::Future`s. Tokio is allowed only in host-only drivers
  and tests.
- **`host` is for doubles and harnesses only.** Scripted models, the
  fake device, fault injectors, in-memory journals, fixture runners —
  each allocation site marked `// HOST-ONLY`. Product logic never
  lives behind `host`.
- **The run and the supervisor are both workflows,** joined by a
  product board dispatcher. `run::run` and `agent::supervise` are
  `async fn`s over `Ctx` in the firmware build, and `agent::device::Device`
  drives one from the other over a board's `RunStore` and `DevicePorts`
  (SPEC §21–23, ADR 0002–0004). A board supplies those two traits —
  never its own dispatcher. The ESP32-S3's are `crates/esper-board`
  (SPEC §24, ADR 0005): every journal a Waymaker bank pair driven by
  `waymaker_drive::Driver` and lent through `waymaker_bridge::Bridge`;
  `boards/esp32s3` is glue only. `journal.rs` (the derived `Frame`
  view), `world.rs` and `run::host` / `agent::host` are host-only
  doubles and harnesses — never product logic.

## Engineering law

SPEC → proof/tests (RED) → implementation (GREEN) → refactor. No
`unwrap`/`expect` outside tests; strong newtypes; `thiserror`; doc
comments on every public item; `const fn` where possible.

## Gates (all must pass before a push)

```bash
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings      # pedantic + nursery
cargo test --workspace
cargo check -p esper-runtime --no-default-features         # the firmware build
cargo check -p esper-board --no-default-features           # the board, without its RAM chip
RUSTDOCFLAGS="-D missing-docs" cargo doc --workspace --no-deps
cargo xtask emulate                                        # links and boots the image under QEMU
```

CI (`.github/workflows/ci.yml`) runs these plus release-mode tests and
bare-metal builds for `thumbv6m`/`thumbv7m`/`thumbv7em-none-eabi`. The
emulated boot needs `qemu-system-arm`; it boots `crates/esper-emu` on a
Cortex-M3 and a Cortex-M4 and requires both to agree with the host.
CI's `board` job installs Espressif's toolchain with `espup` and builds
`boards/esp32s3` for `xtensa-esp32s3-none-elf`; locally, without that
toolchain, lint the image against the ESP32-C3 on the pinned one:

```bash
cd boards/esp32s3
cargo +1.97 clippy --no-default-features --features esp32c3 --target riscv32imc-unknown-none-elf -- -D warnings
```

Every Waymaker crate comes from crates.io, `waymaker-drive` and
`waymaker-bridge` included. Never mix a git source with a registry one:
the types would not unify. Bump them together, in the root workspace and
in `boards/esp32s3` (a workspace of its own).

Do not publish any crate (Mark's call).
