# ADR 0005: The board runs every journal on a Waymaker bank pair

- Status: proposed, pending Mark's review
- Date: 2026-10-27

## Context

The supervisor and the run are `no_std` Waymaker workflows over `Ctx`
(ADR 0002, ADR 0003), joined by the product board dispatcher over a
`RunStore` and `DevicePorts` (ADR 0004). What the ESP32-S3 needed was
those two traits on its SPI flash. Waymaker's flash driver
(`waymaker-drive`) and its bridge to `Ctx` were test support; Waymaker's
ADR 0055 (madmax983/waymaker#213) made both layers, and both are now
published to crates.io beside the others.

Three facts shaped the board:

- The driver owns its boot loop: it calls the workflow with a boundary
  rather than handing out a journal. `RunStore::boot` returned a
  `&mut Journal`, which a flash driver cannot give.
- A swap installs a bank's *next* run, never its first.
- Waymaker refuses a journal whose tail it cannot read — a reset
  between a frame and its seal, or inside a frame — and its failure
  matrix asserts that happens. Recycling such a bank is
  `continue_as_new`'s job, which the driver does not do on a refusal.

## Decision

1. **`RunStore::boot(body)` lends the journal to the run** (`RunBody`,
   answering a `BodyEnd`), so the board boots the driver and lends
   `Bridge::over(boundary)` from inside it. The host and emulated stores
   lend their journals the same way.
2. **Every journal is a bank pair** (`slot`): the supervisor's, one per
   live run, and the item logs beside them (`plan::Plan`). The first
   run of a slot is installed by `slot::commission` — the five steps
   Waymaker's rig performs on a fresh part.
3. **A run slot is keyed by the run id it was commissioned with**, whose
   low 16 bits the driver's segment numbering uses; no directory.
4. **A refused journal is reinstalled from its segment's input**
   (`slot::reinstall`): the other bank, the next generation, the
   successor run id. The segment replays at-least-once.
5. **Faulted runs end `Failed`** with no payload in their journal.
6. **Items handed to the world live in two-area item logs**, the newest
   complete digest always on media.
7. **The image is glue** (`boards/esp32s3`, its own workspace on the
   `esp` toolchain); everything with logic is in `esper-board` and
   tested on the host, under a power cut before and inside every
   storage mutation.

## Consequences

- The supervisor and its runs are durable on the part's flash with no
  storage format of Esper's own beyond the item logs.
- A reset at an unlucky boundary costs a segment replay instead of the
  run; effects repeat (every board tool tolerates it) and operator input
  the discarded history held is lost.
- The image is built in CI but not booted: no part, no Xtensa QEMU.
- Every Waymaker crate comes from crates.io; the root workspace and the
  image's must move together.

## Alternatives considered

- **A journal of Esper's own on flash.** Rejected: Waymaker's journal is
  the store (root CLAUDE.md), and its crash windows are swept already.
- **Copy the driver into the board.** Rejected in Waymaker's ADR 0055.
- **Halt on a refused journal.** Rejected: an always-on agent that a
  single badly-timed reset stops for good is not always on. A refused
  *reinstall* (the media itself failing) is logged and retried each beat.
