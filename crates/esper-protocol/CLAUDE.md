# esper-protocol

The single contract source for Esper's E2 typed native tool catalog.
Authority: `SPEC.md` at the workspace root (§17).

## Non-negotiables

- `#![no_std]`, no `alloc`, `#![deny(unsafe_code)]`, `#![deny(missing_docs)]`.
  Every public item needs docs; every `pub` field is deliberate (SPEC §17
  shows them public — keep them public).
- Firmware-clean: no I/O, no clocks, no logging. Only dependency is
  `thiserror` (no_std-compatible). A CI layering gate fails the build if
  `alloc` appears in this crate's dependency closure.
- `esper-protocol` depends on **nothing** in the workspace. `esper-core`
  depends on `esper-protocol` (for the shared `json` module); never the
  reverse. A cycle here is a layering violation.
- Engineering law (per `~/memory/coding-philosophy.md`): SPEC-PROOF →
  RED → GREEN → REFACTOR. Strong newtypes over primitives (where a
  primitive would cross an API unchecked). `thiserror` for the error
  vocabulary. No `unwrap`/`expect` in non-test source — tests may use
  `expect` with a message. `cargo fmt --check` clean,
  `cargo clippy --all-targets` with workspace lints (pedantic + nursery
  deny, `-D warnings`) clean.

## Module map

| Module | SPEC | What it owns |
|---|---|---|
| `contract` | §17.2–17.3 | The normative six-tool `CATALOG` (`ToolContract` / `ArgSpec` / `ArgKind`), `PermissionClass` / `VerificationStrategy` / `IdempotencyStrategy` (variant names identical to `esper-core`'s copies; core adopts these), `catalog()` / `lookup_by_id` / `lookup_by_name`. Tool ids 1–2 keep their E0/E1 assignments; never renumber |
| `json` | §4.3, §17.4 | Strict allocation-free JSON parser, moved here from `esper-core` in E2 (`git mv`; `esper-core` re-exports it). Plus `parse_u16`, the `u16` sibling of `parse_u8`, added for the `timer_delay_wait` `ms` argument |
| `validate` | §17.3 | `validate(contract, json) -> Result<BoundArgs, ValidationError>`: top-level object, no unknown fields, all required present, per-kind type/range/enum checks. `BoundArgs` / `BoundField` / `Scalar` are fixed-capacity and `Copy`; `BoundArgs::get(name)` is the only reader |
| `render` | §17.3 | `render_signature` (the compact line the model sees in the prompt, e.g. `gpio_pin_write(pin:u8[0-7], level:low|high) -- … [idempotent_write, verify:read_back, bound:64B]`) and `render_json_schema` (minimal JSON Schema for training/host interop). Both write into a caller-owned `&mut dyn core::fmt::Write` |

## Key invariants

- One table (`CATALOG`) is the source of truth: the validator, both
  renderers, and the golden fixtures all derive from it. Never
  hand-write a second schema for a catalog tool.
- `example_ok` validates and every `example_bad` fails, for every tool —
  asserted by the in-crate tests. A contract edit that breaks its own
  examples is a red build.
- Rendered output is byte-stable: the in-crate tests assert the exact
  signature and JSON Schema strings of every tool. Any wording change is
  a deliberate, reviewed diff.
- Enum matching compares raw JSON string content (escapes preserved),
  exactly like the E0/E1 decoder's `Level::from_bytes`: an escaped
  spelling of an option is rejected.
- `BoundArgs` fields are emitted in **contract** order, not JSON key
  order, so dispatch sees deterministic order whatever the model spells.
- The catalog holds no network tools (E2 excludes generic network
  access; that's E5) — asserted by the `no_generic_network_access_in_catalog`
  test.

## Conventions

- `const fn` wherever the body allows it (clippy `missing_const_for_fn`
  is deny-by-workspace-lints).
- Bounds-checked access, never indexing, in validation paths
  (`get`/`get_mut` with typed errors); `saturating_add` for the tiny
  fixed counters.
- `let…else` for single-variant matches (clippy nursery).
- Renderers take `&mut dyn core::fmt::Write`; tests use the in-module
  fixed-buffer `Sink`, never `std`.

## Gates

```bash
export PATH="$HOME/.cargo/bin:$PATH"
cd ~/workspace/esper
cargo test -p esper-protocol
cargo fmt -p esper-protocol -- --check
cargo clippy -p esper-protocol --all-targets -- -D warnings
```

Do not publish this crate (Mark's call). Do not touch
`~/workspace/your_files/`.
