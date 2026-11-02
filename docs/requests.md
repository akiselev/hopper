# Requests to other repositories

Changes hopper needs in repositories it does not own. Each says who owns the code, what is asked,
why, and what hopper does meanwhile. hopper never edits these repositories.

## GUI (lane E, 2026-11-01)

Read against svelters `1d4ef585`, berth `8ac0cce7`, quire `f2208478`.

### quire (owner: quire's owner, via the cadatomic coordinator)

**Q1. A host gutter-decoration API.** A host-defined gutter column (or per-line gutter marks):
text such as a hit count, a glyph such as an execution arrow or a breakpoint dot, with a colour
and a click callback per line. Today `skelter_quire`'s gutter holds only line numbers, fold markers
and diagnostic marks (`crates/skelter_quire/src/scene.rs`, `record_gutter`).
*Meanwhile:* hopper's `hopper-source` kind owns a `SkelterEditor` and draws its own 58-point column
left of quire's gutter from the same scroll offset in the same record pass
(`gui/src/source.rs`, `gui/src/gutter.rs`).

**Q2. Whole-line decorations.** Monaco's `isWholeLine` decorations: a background band on chosen
lines, independent of focus. quire draws its current-line band only while the editor is focused
(`scene.rs`, "current line"). *Meanwhile:* hopper pushes its own band between quire's background
and content lists.

**Q3. Host access to the `code-editor` kind's editor.** `skelter_quire::kind::CodeEditorKind`
keeps its `SkelterEditor` and `EditorView` private and has no methods, so a host cannot reveal a
line, read the scroll offset or the visible lines. cadatomic and hopper each re-implement the kind
to get them. Ask: `revealLine`/`setSelection` methods and a read of the visible line range (or a
`with_editor` accessor). *Meanwhile:* hopper's own kind (`gui/src/source.rs`).

**Q4. A Rust grammar in quire-syntax** (`tree-sitter-rust` behind a feature). The source pane shows
plain text until then.

### svelters / skelter (owner: the svelters coordinator)

**S1. A lazy tree over a `RowModel` in skelter-widgets.** `tree-view` re-flattens every visible
row and clones every visible label on each expand (`skelter-widgets/src/views/tree.rs`, `flatten`
and `set_labels`), which cannot hold a call tree of 10^6 rows. Ask: a tree view whose rows come
from a Rust `RowModel` (as the `table`'s do) with host-answered `len`/`row(i)`/`depth(i)` and
expand/collapse requests to the host, plus a disclosure triangle drawn by the view.
*Meanwhile:* hopper's call tree is the virtual `table` over a `RowModel` whose rows are a
`LazyTree` (`gui/src/tree.rs`, `gui/src/calls.rs`); column 0 draws the indentation and triangle as
text and activating a row (double click, Enter) toggles it.

**S2. Tree disclosure in the `table`.** `RowModel::depth` exists, but the table draws no triangle
and takes no expand/collapse input (no click target, no Left/Right). With S1 this may be the same
work.

**S3. Per-row (or per-cell) text colour in the `table`.** A `RowModel` cell has no style, so a table
cannot grey a stale row (hopper greys moved-out values). Ask: an optional `Cell` style or a
`RowModel::row_tone(row)`. *Meanwhile:* the locals, stack and query panes use hopper's own
`hopper-rows` kind (`gui/src/rows.rs`), which draws through skelter-widgets' public
`table::text_painter::record_core`.

**S4. Which column a table click hit.** The table's `changed`/`activated` signals carry rows only;
a host cannot make one column a link. Ask: the column in `activated` (or a `cell-clicked` signal).

**S5. A documented pure-Rust app path.** `docs/skelter/` covers the Svelte surface only. hopper and
cadatomic both learned the Rust-only setup (a `KindRegistry` of `register_tier0` + views + table +
`TextEdit`, `host::install`, `theme.css()` + `BASE_CSS`, a `DocumentApi` wrapping `CoreDocument`, a
platform handler that asks for frames after wakes) from the gallery app's and cadatomic's sources.
A short page would save the next app the reading.

### berth (owner: the cadatomic coordinator; berth sessions)

**B1. Initial sizes that survive later edge insertions.** `DockCommand::Open { at: WindowEdge,
initial_size }` sizes the new group against the area hint, but a second `WindowEdge` insertion on
the same side re-shares the split, so the first group's size is lost (hopper's timeline came out
about 30 points tall from a 150-point request). *Meanwhile:* hopper inserts the timeline last.

**B2. A non-closable pane shows no close button.** `PaneInfo { closable: false }` panes still draw
the tab's × (seen in the headless render). *Meanwhile:* hopper's dock policy refuses user closes
(`FixedPanes` in `gui/src/ui.rs`, as cadatomic's `HostCloses` does), so the × does nothing.

## Index (lane G, 2026-11-02)

Read against cadabra3 at the checkout in `/home/user/cadabra3` (`crates/cadabra-decide/src/trace_log.rs`)
and rr's `RerunCommand.cc` and `TraceField.cc` (rr sources in the session scratchpad).

### cadabra3 (owner: the cadabra3 coordinator)

**C1. A serialised `decision-trace` stream, one JSON object per line.** `trace_log::capture` keeps
events in memory only (`Trace { events: Vec<Event> }`); nothing writes them out, so hopper cannot
compare two runs' decisions without reading them from a recording. Ask: an opt-in writer (for
example `trace_log::write_jsonl(&Trace, impl Write)`, or an env-var-selected sink under the
`decision-trace` feature) that emits exactly this shape, which `hopper_index::divergence::load_jsonl`
reads:

```json
{"site":"intersect.plane_cylinder.classify","kind":"cmp","cmp":"Lt","lhs":0.5,"rhs":1.0,"outcome":true,"role":3,"file":"crates/x/src/a.rs","line":12,"column":9}
{"site":"decide/0x1f3a","kind":"decision","expr":"0x1f3a","role":2,"first":7,"second":9,"rung":1,"band":1e-9,"lo":-2e-10,"hi":3e-10,"class":"Within"}
```

- `site` (required, string): the `SiteName` text for a `CmpEvent`. A `DecisionEvent` has no
  `SiteName` today; until it carries one, `decide/<ExprId>` keeps decisions with the same predicate
  on one site. hopper aligns events by (site, per-site ordinal), so `site` must be stable across
  runs and must not contain addresses.
- Every other member is the payload, compared by JSON value. Floats must be written with
  round-trip precision (serde_json's default) so that a 1-ulp difference is a difference. `file`,
  `line` and `column` are debug information; they belong in the payload only if a moved line should
  count as a divergence (hopper would prefer them under a `debug` member it can ignore later).
- `at` (optional): a `hopper://` position URL, which hopper fills in when it reads events from a
  recording; cadabra3 leaves it out.
- *Meanwhile:* hopper's phase-A index can anchor the emitters' entries on the timeline
  (`IndexConfig::anchors` = `PathSuffix("trace_log::push_cmp")`, `PathSuffix("trace_log::record_decision")`),
  which gives where and how often, but not the payloads.

### hopper-symbols (owner: lane B)

**SY1. Public enumeration of functions and line-table rows.** `LineTable::rows`, `DwarfIndex::line_table`
and the function index are `pub(crate)`, so `hopper-index` walks gimli's line programs itself and
re-implements `render_file` (`crates/hopper-index/src/phase_a/plan.rs`) to plan line breakpoints,
and reads functions from the ELF symbol table. Ask: `SymbolSet::functions(module) -> impl Iterator<Item = FunctionInfo>`
and `SymbolSet::statement_rows(module) -> impl Iterator<Item = (u64, SourceLoc)>` (runtime
addresses, `is_stmt` rows). *Meanwhile:* the private copy, which follows hopper-symbols' rendering
rule exactly.
*Done (lane I, 2026-11-02):* `SymbolSet::functions(module)` and `SymbolSet::statement_rows(module)`;
phase A plans its line breakpoints from `statement_rows`, and its copy of `render_file` and the
gimli walk are gone. Function sites still come from the symbol table: allocator, anchor and
thread-entry matching is by linkage symbol (libc has no DWARF), and the call tree keeps its names.

### hopper-rr (owner: lane A)

**RR1. A cheap moment at breakpoint stops.** `RrReplica::now` measures `sub` after every continue
by seeking to (event, ticks) and single-stepping until the registers match
(`replica.rs`, `measure_sub`). Phase A asks for the moment at every breakpoint hit, so on rr each
hit costs a seek plus up to `sub` steps; for 10^5–10^6 hits that dominates the index. Ask: a way to
get the moment of a breakpoint stop without re-stepping, for example `sub` counted by an rr-side
instruction counter, or a lazily resolved moment (event, ticks, rip and a register fingerprint)
that the index stores and resolves only when asked. *Meanwhile:* phase A calls `Target::now` and is
correct but slow on rr; not measured here (no PMU).
*Done (lane I, 2026-11-02):* `Target::now_at_stop` returns a `StopMoment` (core): an rr replica
names a breakpoint stop by a `RegionVisit` (region start + PC + visit number) counted as it runs,
without seeking; `Target::resolve_visits` turns a batch of visits into moments in one forward
sweep. Phase A resolves each batch before committing it, so stored moments stay exact. Design in
`hopper_rr::replica`'s module docs; fake-rr tests in `replica_cheap_moments.rs`; on a PMU host
`rr_cheap_moments_at_breakpoints_resolve_to_the_measured_ones` and
`rr_phase_a_cheap_moments_seek_to_their_stops` (not run here).

**RR2. Thread events.** Neither target enables `QThreadEvents`, so phase A finds thread starts and
exits from std's `thread_start` entry and return, and from the first stop on an unknown thread.
Non-Rust threads that never hit a breakpoint do not appear on the timeline. Ask: `QThreadEvents:1`
on gdbserver and `StopReason` variants for thread create and exit, if rr reports them.
*Done (lane I, 2026-11-02):* `Target::enable_thread_events` / `take_thread_events` (core
`ThreadEvent`, no new `StopReason`): gdbserver uses `QThreadEvents:1` and the target absorbs the
`create:` stops and `w` replies (hopper-rsp parses both); rr ignores `QThreadEvents` and reports
only the last thread's exit (rr `ca20dcb`, `GdbServerConnection::set_var`,
`GdbServer::maybe_notify_stop`), so rr replicas compare thread lists at every forward stop. Phase A
uses the events, keeping the `thread_start` heuristic when the server does not report them.
