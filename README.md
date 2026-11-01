# hopper

A time-travel debugger: record a program with [rr](https://rr-project.org), index every value it
produced, and explore the recording afterwards in a native desktop GUI. You can step backwards and
forwards, jump to any moment, and ask where any value came from. For Rust programs, the answer
includes ownership: which move, clone, borrow or drop carried the value to where it is.

Nothing is built yet. Start with:

- [`docs/plan/2026-11-01-first-implementation.md`](docs/plan/2026-11-01-first-implementation.md):
  the plan. It covers the architecture, crate layout, time model, index, ownership sidecar,
  milestones M0–M7 with exit tests, the first experiments, the risks and the open decisions.
- [`docs/research/2026-11-01-rr-and-provenance/`](docs/research/2026-11-01-rr-and-provenance/):
  the sourced research notes behind it.
- The earlier concept study, `devtools/time-travel-debugger.md` in the `ideas` repository, covers
  prior art, the market and ecosystem fit.

Recording needs hardware performance counters, so it runs on a real Linux machine or a VM that
exposes the PMU. Cloud containers without a PMU can build and unit-test the code, but they cannot
record.
