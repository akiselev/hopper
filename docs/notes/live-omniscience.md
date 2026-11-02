# Omniscient queries in a live session

A live session (`Session::live`, `hopper live`, `hopper-gui --live`) runs one process under
gdbserver, forward only. It cannot be replayed, so it has no index of its own and cannot reverse.
hopper answers its omniscient queries from **other runs of the same program** and marks their
moments as those runs', rather than trying to make the moments comparable.

## The choice: mark, don't translate

A live moment is a synthetic stop counter of the live session (`frame_time` counts stops). A second
run has its own counter, advanced by different stops (phase A's breakpoints, single steps), so no
arithmetic maps one onto the other. A translation would need an alignment of the two runs (by
line-hit counts, say), which is easy to get subtly wrong and would hand the user positions that
look seekable but name a different state.

So every answer that comes from another run keeps that run's moments, with that run's recording id.
`RecordingInfo::reference_runs` lists those runs (id and description), and
`RecordingInfo::is_reference_moment` tells a front end which moments are not positions in this
session:

- the CLI prints "(moments of the reference run, …: not positions in this session)" under calls
  and line hits;
- MCP adds `reference_run` (recording, description, note) to `calls` and `line_hits` results;
- DAP adds `referenceRun` to `hopper/calls` and `hopper/lineHits` responses;
- the GUI shows such moments with "(reference run)" and offers no seek on them (`seek` refuses a
  moment of another recording anyway).

Values computed from the other run (allocations, addresses, origin chains) are meaningful for the
live process when the program is deterministic: gdbserver disables address randomisation, so a
deterministic program has the same stack and heap addresses in both runs. A program that reads
input, races threads or depends on time can differ, and its answers can be wrong; the description
of each run says what it is.

## Which run answers what

| Query | Source | Notes |
|---|---|---|
| `calls`, `line_hits`, `timeline` | the **reference run**: a second gdbserver run started with the session (same program, same arguments), indexed by phase A in the background (`LiveIndexPolicy::ReferenceRun`, the default) | `index_status` reports its phases with "(reference run)". A `line_hits` range bounded by live moments has no counterpart and falls back to the stops this session saw |
| `allocation` | the reference run's allocation lineage, by address | an address the reference run allocated more than once is refused, not guessed |
| `origin`, `last_write`, `value_history` | a **stepped window** of a further run (`stepped`): it runs to the entry of the stopped thread's outermost user frame (normally `main`), then single-steps until its registers equal the live process's at the asked moment | made once per moment and cached; history before the window's start is not in it; each window is listed in `reference_runs` |

The program's side effects happen once per extra run (the reference run and each window). Set
`SessionOptions::live_index` to `LiveIndexPolicy::Off` for a program whose second run would do
harm; `origin` still starts its own windows when asked.
