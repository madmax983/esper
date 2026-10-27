# esper-board — the ESP32-S3 board's product half

SPEC §24, ADR 0005. `no_std`, allocation-free, chip-independent:
written against `embedded-storage`'s `NorFlash` and the `Hardware`
trait, tested on the host over `ram::RamNor` (HOST-ONLY, behind the
default `host` feature), linked unchanged into `boards/esp32s3`.

## Where things live

- `src/flash.rs` — `Partition`: Waymaker's `StableStorage` over a window
  of a shared chip (`RefCell`, borrowed per operation; `Busy` if lent).
  The barrier is a no-op because a `NorFlash` mutation that returned has
  completed — the board's stated precondition.
- `src/slot.rs` — a bank pair: `commission` (first install), `authority`
  (read the authoritative header), `reinstall` (a refused journal's
  segment, into the other bank at the next generation), `terminal`.
  Reads go through Waymaker's decoders only.
- `src/runs.rs` — `FlashRuns`: the `RunStore`. Slots keyed by the run
  id they were commissioned with (low 16 bits: the driver's segment
  numbering). `boot` runs `Driver::at_bank` and lends `Bridge` to the
  `RunBody`; a refused journal is reinstalled and the boot halts.
- `src/agent.rs` — `Supervisor`: `supervise` booted on its own slot,
  `Booted::{Ran, Reinstalled}`.
- `src/items.rs` — `ItemLog`: checkpoint items and snapshot chunks, two
  areas, the newest complete digest always kept.
- `src/operator.rs` — `Operator`: the UART line protocol.
- `src/ports.rs` — `BoardPorts`: `RunPorts` + `DevicePorts` over
  `Hardware`; tool outcomes are the host world's JSON byte for byte.
- `src/plan.rs` — `Plan`: every window on the chip, and the reserves.
- `src/demo.rs` — the demo goal and `DISTILL`, its tiny-backend table.
- `tests/board.rs` — the product stack on a RAM chip, swept with a power
  cut before and inside every storage mutation of a clean life.

## Rules

- **Waymaker is the store.** Journals are bank pairs driven by
  `waymaker-drive`; nothing here decides what a record means. The item
  logs hold only what an activity hands the world across a
  `continue_as_new`.
- **Every tool is safe to repeat.** A reinstall replays a segment, and
  redelivery repeats an effect; a tool that is not idempotent needs an
  effect ledger first.
- **`DISTILL` is generated.** `the_distilled_table_is_the_prompts_the_board_renders`
  prints the table when the prompts change; paste it, never hand-edit.
- **Pin new state's size** in `lib.rs`'s `the_board_state_sizes_are_pinned`.
- No `unwrap`/`expect` outside tests; `#![deny(unsafe_code)]`.

## Gates

```bash
cargo test -p esper-board
cargo clippy -p esper-board --all-targets -- -D warnings
cargo check -p esper-board --no-default-features
```
