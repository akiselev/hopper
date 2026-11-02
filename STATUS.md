# hopper status

## Handoff (coordinator, 2026-11-02)

The plan (`docs/plan/2026-11-01-first-implementation.md`) is implemented through M7 in code. All
of it that can run without rr is tested in a cloud container; nothing has run under rr yet,
because the container has no PMU. **Next: run `docs/TESTING-AT-HOME.md` on a PMU host.**

Test counts (cloud container, branch `claude/debugger-snapshot-gui-miic4i`):
main workspace 319 passed, 0 failed, 18 ignored (`rr_*`); `gui/` 36 passed; `hopper-build/` 9
passed. `cargo clippy --workspace --all-targets` reports 0 warnings; `cargo fmt --check` is clean.

| Milestone | State |
|---|---|
| M0 experiments | Scripts in `scripts/experiments/`. E1 run in the cloud: no PMU. E1–E8 need a PMU host. |
| M1 thin slice | Live-mode M1 passes (20 stops vs rust-gdb). rr version: `rr_m1_*`, not run. |
| M2 GUI | Built on skelter/berth/quire; smoke tests in skelter-test; screenshots rendered on lavapipe. |
| M3 phase-A index | Passes on gdbserver (call counts, line hits, allocations). `rr_m3_*` not run. |
| M4 machine provenance + MCP | Passes on a step recording and in live mode. `rr_m4_*` not run. |
| M5 ownership sidecar | Six hops pass on the step recording and live; `rr_m5_*` not run. |
| M6 phase-B write log | Stepping path passes on gdbserver (~900 instr/s, cloud); rr rerun shards tested on synthetic records only. The "under 1 s on a cadabra3 Boolean case" exit test is not attempted. |
| M7 DAP + divergence | DAP passes against the fake and live; divergence aligns synthetic streams. cadabra3 has no decision-event serialisation yet (request C1). |

Known gaps worth knowing first: one process per session (no fork/exec following); inlined frames
are stepped as their physical frame; `origin` follows 32 bytes per hop; live-mode answers assume
the program is deterministic; quire has no Rust grammar (no highlighting).

Requests to other repositories: `docs/requests.md` (quire Q1–Q4, svelters S1–S5, berth B1–B2,
cadabra3 C1).

## User decisions

- 2026-11-01: hopper may depend on unreleased svelters, berth and quire code (sibling checkouts).
- Open: licence (no LICENSE file yet; adapted BugStalker and Rust code is MIT/Apache, see
  `THIRD_PARTY_NOTICES.md`); toolchain policy for sidecar builds (pinned nightly
  `nightly-2026-05-01`); where recording runs; whether recordings holding Parasolid memory may
  leave the user's machines; asking cadabra3 for a decision-event stream (request C1).
