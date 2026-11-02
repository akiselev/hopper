# Index API (hopper-index, lane G, 2026-11-02)

The public surface of `crates/hopper-index`, for the engine (lane H). It is stable from here on:
later changes add to it, they do not change it. `cargo doc -p hopper-index` has the details; this
note is the outline and the intended wiring.

## Opening

```rust
use hopper_index::{Index, RecordingMeta, IndexKind, Answer, IndexControl};

let index = Arc::new(Index::open(&dir, RecordingMeta {
    recording: info.id,                       // RecordingId
    build_ids: vec![main_build_id_hex, ...],  // lowercase hex, engine's order
})?);
```

- `dir` is beside the trace (for example `<trace>.hopper-index/`), never inside it: `open`
  refuses a directory inside an rr trace (one holding `events` and `mmaps`), a catalogue of
  another recording or other build ids, and another index format version (`HopperError::Index`).
- `Index` is `Send + Sync`; share one `Arc<Index>` between the phase-A job, phase-B jobs and the
  `Debugger` methods. Queries see what has been committed so far.
- Reopening the same directory reuses the finished work.

## Phase A (call tree, line hits, allocator lineage, timeline)

```rust
use hopper_index::{PhaseA, IndexConfig, IndexControl};

let job = PhaseA::new(symbols.clone() /* SymbolSet with the main module as ModuleId(0) */,
                      IndexConfig::default())
    .with_expected_end(last_event);           // optional, for progress
let control = IndexControl::new();            // clone it: cancel() / status()
// on a background replica's worker thread, target at the recording start:
let summary: PhaseASummary = job.run(&mut target /* &mut dyn Target */, &index, &control)?;
```

- `run` returns at once with `summary.reused == true` when the index already covers the
  recording. A seekable target resumes an unfinished run from its last commit; a gdbserver target
  starts over.
- `control.status()` is an `IndexStatus` (phase `"phase-a"`), for `Debugger::index_status`.
  `control.cancel()` stops the job at its next stop; it commits what it has and returns
  `HopperError::Cancelled`.
- `IndexConfig` fields: `lines: SourceSelection` (default `CrateFiles`: the binary's own crate
  files, not std or registry crates), `functions: FunctionSelection` (default `CrateFiles`),
  `allocator: AllocatorTracking`, `anchors: Vec<NamePattern>` (for cadabra3:
  `NamePattern::PathSuffix("trace_log::push_cmp".into())` and `"trace_log::record_decision"`),
  `thread_entries`, `allocator_frames`, `max_hits_per_address` (default 100 000), `flush_every`,
  `requester_unwind_depth`, `start: StartPoint` (`RecordingStart` or `Here`).
- `job.plan()` lists the functions, lines, allocator symbols and anchors before running.
- The symbol set needs only the main executable: shared libraries (libc's malloc family) are
  found from the dynamic linker's `link_map` during the run.

## Queries (match the `Debugger` methods)

| `Debugger` method | `Index` call | notes |
|---|---|---|
| `calls(filter, page)` | `index.calls(&filter, page) -> Result<Page<CallNode>>` | no parent + function filter searches the whole tree |
| — | `index.call(CallId) -> Result<Option<CallNode>>` | |
| `line_hits(loc, range, page)` | `index.line_hits(&loc, range, page) -> Result<Page<Moment>>` | file by path suffix, exact line, column ignored |
| `allocation(process, addr, at)` | `index.allocation_at(process, addr, at) -> Result<Answer<Option<AllocationInfo>>>` | `.value` for the trait; `.completeness` for display |
| — | `index.allocation(AllocationId)`, `index.allocations(range, page)` | lineage: follow `predecessor` |
| `timeline(range, page)` | `index.timeline(range, page) -> Result<Page<TimelineEvent>>` | ThreadStart/ThreadExit/ProcessExit/Anchor |
| `last_write(place, before)` | `index.last_write(place, before) -> Result<Answer<Option<LoggedWrite>>>` | phase-B data only |
| — | `index.writes_in(place, range, page) -> Result<Page<LoggedWrite>>` | |
| `index_status()` | `control.status()` | |

Every `Page` carries `Completeness::Partial { pending }` when the index has not covered part of
the range, or when a breakpoint address in scope hit its cap. `index.coverage(kind)` and
`index.is_complete(kind)` expose the raw coverage.

`LoggedWrite { at, pc, place, value: WriteValue }` is the stored row. `value` is
`WriteValue::Bytes(..)` or `WriteValue::Unrecorded` (the rr rerun path when the bytes did not
come from a register). `logged.to_event(&symbols) -> Option<WriteEvent>` gives the API's
`WriteEvent` (with function and location); for `Unrecorded`, read the place at the moment after
`at` and call `logged.event_with_value(&symbols, bytes)`.

## Phase B (windowed write log)

```rust
use hopper_index::writes::{log_writes_by_stepping, StepWindow, WindowEnd};

let mut w = StepWindow::new(WindowEnd::Moment(end));   // or Pc(addr), Steps(n), ProcessExit
w.start = Some(start);                                 // seekable targets seek; others must be there
let summary = log_writes_by_stepping(&mut target, &w, &index, &control)?;
```

and on a PMU host, in parallel shards:

```rust
use hopper_index::writes::rerun::{RerunJob, ShardStart, plan_shards, run_rerun_shards};
let job = RerunJob { rr, trace_dir, recording, process,
                     shards: plan_shards(start_event, end_event, n), start: ShardStart::Replay,
                     flush_every: 4096 };
run_rerun_shards(&job, &symbols /* CodeSource */, &index, &control)?;
```

## Divergence (M7)

```rust
use hopper_index::divergence::{load_jsonl_file, first_divergence};
let d = first_divergence(&load_jsonl_file(a)?, &load_jsonl_file(b)?);
// d.first_payload_difference: Option<PayloadDifference { left, right: EventRef }>
// d.control_flow: Option<ControlFlowDivergence { left, right: Option<EventRef> }>
```

## Facts measured on the fixtures (gdbserver, cloud container)

- Allocator symbols found (rustc 1.97, debug, no `#[global_allocator]`): in the main binary
  `__rustc::__rust_alloc`, `__rust_alloc_zeroed`, `__rust_realloc`, `__rust_dealloc` and the four
  `__rustc::__rdl_*`; no `__rg_*`. In `libc.so.6` (found from `link_map`): `malloc`, `calloc`,
  `realloc`, `reallocarray`, `free`, `posix_memalign`, `aligned_alloc`, `memalign`. Only the
  outermost allocator call per thread is recorded, so `__rust_alloc` → `__rdl_alloc` → `malloc` is
  one allocation.
- The derived `<Config as Clone>::clone` is named `worked_example::{impl#0}::clone` (DWARF's
  name) in `AllocationInfo::requested_by` and call nodes.
- Phase B by single-stepping gdbserver: about 800–930 instructions per second (cloud container,
  not a gate).
