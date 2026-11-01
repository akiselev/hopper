# hopper: instructions for agents

hopper is a time-travel debugger: record with rr, index every value, explore the recording in a
native GUI, and answer "where did this value come from" with Rust ownership labels. The plan is
`docs/plan/2026-11-01-first-implementation.md` (milestones M0–M7). Status is in `STATUS.md`.

## Layout

- `crates/hopper-core`: the shared vocabulary (`Moment`, `Place`, `RegisterSet`, ids, errors), the
  `target::Target` trait (one serial replay connection) and the `api::Debugger` trait (the one
  engine API). **One type per concept:** if core has it, use it; if core lacks something, add it
  to core rather than a private copy in your crate.
- `crates/hopper-rsp` (GDB remote protocol client), `hopper-rr` (rr process control, replicas,
  the gdbserver live target), `hopper-symbols` (ELF/DWARF, unwinding), `hopper-eval` (values),
  `hopper-sidecar` (the `.hsc` ownership-sidecar format and its join), `hopper-index` (background
  index), `hopper-engine` (implements `Debugger`), `hopper-fake` (a canned `Debugger` for front-end
  tests), `hopper-cli`, `hopper-mcp`, `hopper-dap`.
- `hopper-build/`: its own workspace on the pinned nightly (`rustc_plugin`), writes `.hsc` files.
- `gui/`: its own workspace; depends on the sibling checkouts `../svelters`, `../berth`, `../quire`.
- `fixtures/`: debuggee programs, each its own cargo project. `scripts/build-fixtures.sh` builds
  them into `target/fixtures/<name>/debug/<name>`. The comments in `fixtures/worked-example` give
  the line numbers tests rely on; never move those lines.

## Rules

- **Idiomatic Rust.** Enums that carry exactly the valid combinations; no sentinel values, integer
  modes, bool flag parameters or `&mut` out-parameters. `unsafe_code` is forbidden workspace-wide.
- **Names say what the code computes.** No issue numbers or plan ids in identifiers.
- **Tests check outputs.** Unit tests next to the code; integration tests in `tests/`.
- **rr cannot record or replay in cloud containers** (no PMU: `rr record` fails with "Unable to
  open performance counter"). Code that needs rr gets tests in two forms: transcript or mock tests
  that run everywhere, and `#[ignore]`d tests named `rr_*` that run on a PMU host with
  `cargo test -- --ignored rr_`. **gdbserver works here** and is the live forward-only target for
  integration tests of the RSP client, symbols, evaluator and engine.
- Build and test only what you touched (`cargo test -p <crate>`); run clippy on your crates.
- Don't edit other repositories (svelters, berth, quire, cadabra3, ...). A change one needs goes
  in `docs/requests.md` with the reason.
