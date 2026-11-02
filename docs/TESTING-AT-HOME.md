# Testing hopper on a machine with rr

Everything that can run without hardware performance counters was built and tested in a
cloud container: 364 tests pass there, including real gdbserver runs of the fixtures. What was
never run is anything that needs rr to record or replay: 18 `#[ignore]`d tests named `rr_*`, the
M0 experiments, and the GUI on a real recording. This page is the order to run them in.

## 0. Setup

- Linux on bare metal, or a VM that exposes the PMU (rr's wiki: "Will rr work on my system").
  Intel works as is; AMD Zen needs rr's `scripts/zen_workaround.py` after every boot.
- rr 5.9 or 5.10 (`rr --version`); `kernel.perf_event_paranoid` ≤ 2 on kernel 6.10+, else ≤ 1.
- gdb with `rust-gdb` and gdbserver (the M1 comparison and the live mode use them).
- Rust 1.97 (the repo pins it), plus `nightly-2026-05-01` with `rustc-dev`, `llvm-tools-preview`
  and `rust-src` for the ownership sidecar (`hopper-build/rust-toolchain.toml`).
- For the GUI: the sibling checkouts `../svelters`, `../berth` and `../quire` next to this repo, and
  Vulkan (any working driver; `gui/README.md` lists what the container needed).

## 1. Experiments (M0)

```sh
scripts/experiments/run-all.sh            # E1, E2, E5, E7; reports in docs/experiments/results/
scripts/experiments/e8-wine-parasolid.sh <parasolid-rs test .exe>   # E8, needs Wine
```

E1 says whether rr works at all; if it fails, nothing below will.

## 2. The rr tests, crate by crate

Build the fixtures first: `scripts/build-fixtures.sh`.

```sh
cargo test -p hopper-rsp    -- --ignored rr_    # rr packet helpers against a real rr
cargo test -p hopper-rr     -- --ignored rr_    # record, replay, reverse-step byte identity,
                                                # seek, checkpoints, reverse watchpoints,
                                                # cheap moments vs measured moments
cargo test -p hopper-index  -- --ignored rr_    # phase A on rr, resume, rerun shards
cargo test -p hopper-engine -- --ignored rr_    # the plan's exit tests M1, M3, M4, M5
cargo test -p hopper-cli    -- --ignored rr_    # hopper record, then show and info
```

Run them in that order: each layer assumes the one below it works. The engine's exit tests are
the plan's milestones:

| Test | Milestone exit test |
|---|---|
| `rr_m1_locals_match_rust_gdb_and_step_back_then_step_is_byte_identical` | M1: 20 stops match rust-gdb; step back then step gives identical registers |
| `rr_m3_call_counts_match_gdb_and_line_hits_do_not_move_the_cursor` | M3 |
| `rr_m4_origin_reaches_the_rodata_literal_and_mcp_reproduces_it` | M4 |
| `rr_m5_six_source_hops_and_a_moved_out_after_its_move` | M5: the worked example's six ownership hops |

Behaviours inferred from rr's source but never observed (the tests above check them; if one
fails, these are the first suspects):

- rr drops all breakpoints on a `vRun` seek, and answers the first resume after it with a
  signal-0 stop without moving (hopper-rr re-inserts and absorbs).
- A ticks-only seek within the same event lands where event+ticks would.
- Continuing from a pc that has a breakpoint traps immediately (cheap moments rely on it).
- A reverse-continue from a seeked-to moment reports a write made by the instruction just before.

## 3. Try it by hand

```sh
cargo run -p hopper-cli -- record -o /tmp/we -- target/fixtures/worked-example/debug/worked-example
cargo run -p hopper-cli -- info /tmp/we
cargo run -p hopper-cli -- repl /tmp/we          # b main.rs:17, c, locals, origin *n, rs, back
cargo run -p hopper-cli -- show /tmp/we --at main.rs:14

# With ownership labels: build with the sidecar first.
cargo run -p hopper-cli -- record --build fixtures/worked-example -o /tmp/we2

# The GUI (from gui/):
cargo run --release -- /tmp/we2
cargo run --release -- --live -- ../target/fixtures/worked-example/debug/worked-example
cargo run --release -- --fake

# Agents and editors:
cargo run -p hopper-cli -- mcp /tmp/we2      # MCP on stdio
cargo run -p hopper-cli -- dap               # DAP on stdio; launch {"recording": "/tmp/we2"}
```

The live mode (`hopper live -- prog`, `hopper-gui --live -- prog`) needs no PMU: it runs the
program under gdbserver, forward only, and answers queries about the past from extra runs of the
same program (labelled as such; see `docs/notes/live-omniscience.md`).
