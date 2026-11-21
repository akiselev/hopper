# Calls at scale: the PT call tree, the profile query and the Profile pane

Status: plan, 2026-11-02. Motivation: a debug build of a real program (a cadabra3 Boolean,
`Edit::boolean`) makes 13.6M calls into 81,522 functions. Phase A, which stops at every function
entry and return, does not finish on it, and even a finished call tree of 13.6M rows is unreadable.
Three parts fix that.

Measurements behind the choices (i7-8850H, rr 5.10.0, the cadabra recording):

| Approach | Throughput | Whole cadabra run |
|---|---|---|
| Phase A (a breakpoint stop per entry and return) | far below 1M calls/hour | no progress report after 2 min; does not finish |
| `rr rerun --singlestep` | ~14K instructions/s | days |
| `perf record -e intel_pt//u -- rr replay -a <trace>` | native speed | 2 s to record, 2 min to decode the calls (`perf script --call-trace`) |

PT on the replay sees the replayed process (`comm` `rr:<name>`) with symbols from the binary
copies in the trace directory; its decoded calls under `Edit::boolean` match a native run of the
same program.

## Part 1: the call tree from Intel PT on the replay

A new producer of `IndexKind::Calls` records for rr recordings, used when the host has Intel PT
(`/sys/bus/event_source/devices/intel_pt` exists and `perf` is on PATH). Phase A keeps producing
lines, allocations and the timeline; with the PT producer active it plants no function-entry
breakpoints (its allocator and thread-entry frames stay, they are not `Calls`).

Pipeline (`hopper-rr`: process control; `hopper-index`: decoding and records):
1. `perf record -e intel_pt//u -o <work>/pt.data -- rr replay -a -q <trace>`, in a work directory
   the index owns (beside the trace, `<trace>.hopper-index/pt/`), deleted after decoding.
2. `perf script -i pt.data --itrace=b -F pid,tid,ip,addr,flags --ns` (or `perf script` with a
   format that gives every *taken* branch with its source, target and kind), streamed, never
   written to disk; only the replayed process's pid is kept.
3. A streaming decoder turns taken branches into calls with exact positions:
   - Between two taken branches the thread ran straight-line code from the first's target to the
     second's source. Decode that run with iced (bytes from the trace's binary copies, the
     existing `CodeSource`) and count its conditional branches (each not-taken `jcc` is a retired
     conditional branch, a tick) and its instructions. Cache the counts per (target, source) run.
   - Ticks per thread = taken `jcc` + not-taken `jcc` in the runs. `sub` = instructions since
     the tick count last changed. This gives each call's entry and exit (ticks, sub) exactly, if
     the counts agree with rr's (verify: below).
   - `frame_time` (rr's `when`) comes from the trace's own events: each event has a tid and that
     thread's ticks (`rr dump`, or hopper-rr's trace reader), and a thread runs between its
     events, so (tid, ticks) maps to the last event completed before that point.
   - Thread ids: the replay's real tids differ from the recorded ones; map them by creation order
     (perf's task events against the trace's clone events).
   - Calls: a `call` branch opens a frame (callee = target, return address = source + length),
     a `return` closes the innermost frame whose return address it lands on (unwinding inner
     frames, as Phase A does). Tail jumps into another function are not calls.
   - Emit `Function` (once per function, name and location from `SymbolSet`) and
     `CallEnter`/`CallExit` records with the same shape Phase A writes, so `Index::calls` and
     everything above it is unchanged.
4. Progress `calls (pt)` with fractions for record, decode; coverage of `Calls` is written by this
   producer only (split it from Phase A's single coverage value, see `already_finished`).

Verification gate (do this first; it decides whether moments are exact):
- On the `threads` and `worked-example` fixtures and on a cadabra recording, for a sample of
  calls, seek an `RrReplica` to the call's entry moment and check the PC is the callee entry and
  rr's `when`/`when-ticks` equal the computed ones.
- If ticks do not match exactly, keep exact (tid, ticks-region) where they do and fall back to a
  per-call `RegionVisit`-style lazy resolution (seek by function entry + hit count) for the rest;
  document the gap.

Fallback: no PT (AMD, most VMs) or perf missing: today's Phase A, unchanged.

Tests: unit tests of the decoder on synthetic branch streams (calls, returns, tail jumps,
recursion, longjmp-style unwinds, threads); a stand-in `perf` script like
`tests/rerun_shards.rs`'s stand-in rr; `#[ignore]`d `rr_*` tests (PMU + PT host) comparing the PT
call tree with Phase A's on the fixtures (same calls, same entry moments) and the moment check
above.

## Part 2: the profile query (calling-context tree with tick cost)

New core API (`hopper-core/src/api.rs`), answered from the `Calls` records whatever produced them
(Phase A, PT, a live reference run, the fake):

```rust
pub struct ProfileRequest {
    /// The call to profile; `None`: every root call.
    pub root: Option<CallId>,
    pub tid: Option<Tid>,
    pub scope: CodeScope,
    /// A node is shown when its inclusive cost is at least `of_root` of the root's, or at least
    /// `of_parent` of its parent's (keeps local structure inside small subtrees).
    pub threshold: Threshold,
    /// Calls active at these moments, and their ancestors, are shown whatever their cost
    /// (an origin chain's hops, a breakpoint's hits).
    pub pins: Vec<Moment>,
}
pub enum CodeScope { All, Own }
pub struct Threshold { pub of_root: f32, pub of_parent: f32 }
pub struct ProfileNode {
    pub id: ProfileNodeId,           // stable within one ProfileRequest
    pub parent: Option<ProfileNodeId>,
    pub function: String,
    pub location: Option<SourceLoc>,
    pub calls: u64,                   // calls merged into this node
    pub inclusive: u64,               // ticks
    pub self_cost: u64,               // ticks not in shown children
    pub pinned: bool,
    pub child_count: u64,             // shown children
    pub hidden: Hidden,               // children under the threshold: count and cost
    pub first_call: CallId,           // to jump to an example call
    pub recursion: u32,               // folded recursive depth (0: none)
}
pub struct Hidden { pub nodes: u64, pub cost: u64 }
fn profile(&self, req: &ProfileRequest, parent: Option<ProfileNodeId>, page: PageReq)
    -> Result<Page<ProfileNode>>;
```

Semantics:
- Cost of a call: `exit.ticks - entry.ticks` (same thread; a call that never returned costs up to
  the end of its coverage). Ticks are deterministic, unlike time in a replay.
- Merge: calls with the same path of shown functions from the root become one node (a calling-
  context tree). With `CodeScope::Own`, functions outside the recording's own crates are
  transparent: their cost stays in the nearest own ancestor and their own-code callees attach to
  it. Own = the function's source file is not a toolchain/registry path (`is_toolchain_path`,
  which also needs to recognise `~/.cargo/registry`); a function without a location is not own.
- Recursion: a function already on the path is folded into its first occurrence (`recursion`
  counts the extra depth) so a recursive algorithm is one node, not one per depth.
- Pruning by `threshold` then pins; `hidden` summarises what was cut below each node.
- `Completeness` follows the `Calls` coverage, as `calls` does.
- Built in `hopper-index` from the in-memory `CallCache` (one pass over the root's subtree, cached
  per request key); implemented by `Session` (engine), `FakeDebugger` (hopper-fake), and exposed
  by the CLI (`profile [fn|call-id] [--own] [--min <pct>]` in the REPL), MCP (`profile` tool) and
  nothing in DAP.

Tests: index unit tests on hand-built call trees (merging, own-code transparency, recursion
folding, both thresholds, pins, hidden totals, partial coverage); fake-debugger tests; one engine
test over a real recording path (gdbserver reference run or step recording).

## Part 3: the Profile pane in the GUI

A `Profile` pane tabbed with `Calls` (same dock group), a lazy table over `profile(...)` pages
like `calls.rs` over `calls(...)`:
- Columns: function (indent + disclosure), inclusive % of root, inclusive ticks, self %, calls,
  hidden (e.g. "+12 below 1%").
- Controls in the pane header: threshold (0.1 / 0.5 / 1 / 5 %), "My code" toggle, "Pin origin"
  (pins the hops of the origin chain last shown in the query pane), and the root: the call
  selected in Calls ("Profile this call"), or all roots.
- Activating a row selects its `first_call` in Calls and moves the cursor to its entry.
- Re-fetch when the index completes (the Calls pane does not today; fix both).
- Tests with `FakeDebugger` in `gui/tests/smoke.rs` and cell-formatting unit tests like
  `calls.rs`'s.

## Order and ownership

Part 2 and Part 1 are independent (Part 2 reads `Calls` records from any producer); Part 3 needs
Part 2's API. Part 1 is the riskiest (the tick-matching gate) and is developed on its own branch.
