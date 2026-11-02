# M0 experiments

The plan (`docs/plan/2026-11-01-first-implementation.md`, "Recording runs only on PMU hosts")
puts eight experiments before the code. The scripts for E1, E2, E5, E7 and E8 are in
`scripts/experiments/`; each writes its report to `docs/experiments/results/<name>.md`
(overwriting that machine's previous run) and keeps its scratch files under
`target/experiments/<name>/`. E3, E4 and E6 belong to other lanes (the sidecar driver and the
emulator).

All of them except E1's checks need a machine where rr can record: a working
retired-conditional-branch counter (bare-metal Intel; AMD Zen with the SpecLockMap workaround;
some cloud instance types). Cloud containers have no PMU: `rr record` fails with "Unable to open
performance counter". `results/e1-pmu-cloud-container.md` is that negative result.

| Script | Experiment | Needs | Report |
|---|---|---|---|
| `e1-pmu.sh` | E1: does rr work here? `rr record true`, `perf stat -e br_inst_retired.conditional true`, CPU, kernel, virtualisation, `perf_event_paranoid` | rr, perf (optional) | `results/e1-pmu.md` |
| `e2-rsp-latency.sh [samples]` | E2: latency of `vRun` seeks, `bc`, `bs`, forward steps and `qRRCmd:when-ticks` over hopper's own RSP client, on the `worked-example` and `threads` fixtures | rr, cargo, PMU | `results/e2-rsp-latency.md` |
| `e5-breakpoint-hits.sh` | E5: replay cost with 10^3, 10^4 and 10^5 breakpoint hits (a C program calling `hit` N times; full replay with and without a breakpoint) | rr, cargo, cc, PMU | `results/e5-breakpoint-hits.md` |
| `e7-rerun-singlestep.sh` | E7: `rr rerun --singlestep=event,icount,ip` throughput against native and `rr replay -a`, to calibrate the "roughly 1000x" estimate | rr, cc, bc, PMU | `results/e7-rerun-singlestep.md` |
| `e8-wine-parasolid.sh <test.exe> [args]` | E8: record a parasolid-rs Wine test under rr with `WINELOADERNOEXEC=1`; checks for the "partial write" error (Wine MR 7669), lists the trace's processes and replays it | rr, wine, the test exe with `pskernel.dll` beside it, PMU | `results/e8-wine-parasolid.md` |

`run-all.sh` runs E1, E2, E5 and E7 in order. E8 is run by hand because it needs the path of a
built parasolid-rs test executable (see parasolid-rs's `CLAUDE.md` for building it).

## Running at home

```bash
scripts/experiments/run-all.sh
scripts/experiments/e8-wine-parasolid.sh ~/projects/parasolid-rs/target/x86_64-pc-windows-gnu/debug/parasolid-test.exe
```

The E2 and E5 measurements come from two example programs in `hopper-rr`:
`examples/rsp_latency.rs` (`<trace-dir> [samples]`) and `examples/breakpoint_hits.rs`
(`<trace-dir> <executable> <symbol>`); both can be run on any trace directly.

Timings are only meaningful on a quiet machine. **E8's trace holds Parasolid DLL memory: keep it
on the machine that recorded it.** The report records only hashes, times and rr's output.

After a run, the same machine can also run the rr integration tests:
`cargo test -p hopper-rr -- --ignored rr_`.
