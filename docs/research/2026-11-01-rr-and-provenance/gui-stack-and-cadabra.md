# GUI stack readiness for the hopper viewer, and cadabra3's decision traces

Scope: a reading task, done on 2026-11-01. Nothing was built or run. Sources are local checkouts, cited as `path:line`
(file links). "Verified in code" means I read the source lines cited. "Stated in a doc" means a STATUS file, README or
lane record says it and I did not confirm it in source.

Repository shas read (`git -C <repo> rev-parse HEAD`):

| Repo | HEAD | Commit date |
|---|---|---|
| svelters | `1d4ef5859756f47784a0146e3435e4dcc5a43fe6` | 2026-11-01 |
| cadatomic | `c3ae53faa6f89bf5c728d56df3663f128ad881a7` | 2026-11-01 |
| berth | `8ac0cce7e2bbea463f3e850e336f1feeef7039b5` | 2026-10-28 |
| quire | `f2208478faa1466061af03374639ed40e9842eff` (branch `claude/debugger-snapshot-gui-miic4i`, same sha as `master`) | 2026-10-28 |
| cadabra3 | `7d9cf791e20eea953e0497b490fa5c493622228c` | 2026-11-01 |
| parasolid-re | `b8549c36ee295f8bb6a8a4049066ebb2b444cdab` | 2026-10-22 |
| parasolid-rs | `4a3a7acf3a3dd328301bfcbd086952a967cc40ae` | 2026-10-21 |

Pin skew: cadatomic pins svelters `17b84308` (an ancestor of local HEAD), berth `8ac0cce` (equal to local HEAD) and
quire `9c84baf`. **`9c84baf` is not in the local quire checkout** (`git cat-file` fails), so cadatomic builds a quire
that the local checkout does not contain. My quire findings describe `f220847`, not what cadatomic ships.
([cadatomic/pins.toml](file:///home/user/cadatomic/pins.toml), [cadatomic/Cargo.toml:77-80](file:///home/user/cadatomic/Cargo.toml))

## skelter: backends on Linux, widget set, text, canvas, app structure

### Takeaway
skelter is not a layer over GTK or Qt. It is its own retained toolkit on wgpu, with its own platform backends:
Wayland, macOS, Win32, and a winit fallback. On Linux the default is **winit** (X11 or Wayland). The native Wayland
backend is opt-in. GTK, Qt, AppKit and WinUI are separate *svelters* backends that drive native widgets, not skelter
backends. skelter has a virtual list, a virtual tree, a virtual two-axis table, an f64 GPU canvas built for millions of
primitives, and a parley-based text stack. So most viewer needs have a starting point. Two caveats: the tree's model
cost is O(n) per expand, and CI has not verified the last two days of refactor merges.

### Cited Findings
**Backends (verified in code)**
- The skelter facade picks a backend at start: Win32 or AppKit on those OSes; own Wayland only with
  `SKELTER_BACKEND=wayland|native` and a non-empty `WAYLAND_DISPLAY`; otherwise winit, "X11, Wayland without the
  opt-in"; also winit when an own backend fails before `init`. "winit is the Wayland default" (coordinator, 2026-11-01).
  — [svelters/crates/skelter/src/run.rs:3-8, 29-49, 56-73](file:///home/user/svelters/crates/skelter/src/run.rs)
- The skelter platform crates are `skelter-platform` (winit, about 5.8k lines), `skelter-platform-wayland` (about
  5.8k lines: xkb, IME, portal, CSD, seat), `skelter-platform-macos` and `skelter-platform-win32`. None depends on GTK
  or Qt (dependency lists read from each `Cargo.toml`). — [svelters/crates/](file:///home/user/svelters/crates/)
- GTK (`svelters-gtk`), Qt (`svelters-qt-build`), AppKit and WinUI are svelters *native-widget* backends. Decided
  (coordinator, 2026-11-01): "GTK, AppKit and WinUI are active backends (GTK has full CI and is the main Linux backend);
  Qt is a maintained per-project leaf crate". — [svelters/STATUS.md:186-190](file:///home/user/svelters/STATUS.md)
  (stated in a doc)

**CI state (stated in a doc)**
- The six GitHub workflows (js, linux-gtk, linux-qt, macos-appkit, skelter, windows-winui) last ran green on QA11Y
  fixes `6dd49c0b`. — [svelters/STATUS.md:200-218](file:///home/user/svelters/STATUS.md)
- Refactor2 wave 1 was merged with "GitHub Actions is down (billing): no CI run on any wave-1 sha". Wave 2 is mostly
  unmerged, and its branches have open review findings (B09, B10, B13 CHANGES; B04 WIP). — [svelters/STATUS.md:292-327](file:///home/user/svelters/STATUS.md)
- svelters-egui is "frozen, not retired" (user, 2026-10-31). — [svelters/STATUS.md:188-190](file:///home/user/svelters/STATUS.md)

**Widget set (verified in code)**
- `skelter-widgets` is about 53k lines. Its modules are controls, dialogs, dnd, pickers, table, tabs, text, theme,
  views (scroll, split, menu, select, modal, popup, toast, tooltip, tree, virtual_list, status_bar, inner_window) and
  zoom. — [svelters/crates/skelter-widgets/src/lib.rs:7-21](file:///home/user/svelters/crates/skelter-widgets/src/lib.rs)
- `virtual-list` realises only the rows near the viewport (overscan 2 rows each side). Row offsets come from
  `RowHeights` (uniform rows, or a Fenwick tree once a row is measured). Slots are recycled. Selection is keyed by item
  id (none/single/multi/toggle/range) and survives splices and sorts. Calls are `splice`, `scrollTo`, `rowAt`.
  — [virtual_list.rs:1-26, 46-49](file:///home/user/svelters/crates/skelter-widgets/src/views/virtual_list.rs)
- The list stores `ids: Vec<u64>` and `labels: Vec<String>`. Cursor lookup `pos_of` is a linear scan.
  — [virtual_list.rs:50-52, 142-144](file:///home/user/svelters/crates/skelter-widgets/src/views/virtual_list.rs)
- `tree-view` is drawn by the list machinery. It keeps the model natively (`HashMap<u64, TreeNode>`, an expanded
  `HashSet`), flattens the visible rows into the list, and loads lazy children through `expand(id)`, which JS answers
  with `setChildren`. There is also a JS-flattened mode (`splice`/`setNodes`). — [tree.rs:1-20, 66-91](file:///home/user/svelters/crates/skelter-widgets/src/views/tree.rs)
- Each expand or collapse runs `flatten()` over every visible row, copies the old and new id vectors, and calls
  `set_labels` with a clone of **every** visible label. — [tree.rs:206-238](file:///home/user/svelters/crates/skelter-widgets/src/views/tree.rs)
- `table` is virtual on both axes, with a frozen header, sticky columns, sort, column resize, keyboard cursor, copy and
  cell editing. Rows come from a JS window (`range` → `set-rows`) or a Rust `RowModel` service. "Not here yet": the
  column menu, the scroll bars and edge shadows. — [table/mod.rs:1-22](file:///home/user/svelters/crates/skelter-widgets/src/table/mod.rs);
  the `RowModel` trait (`len`, `key(row)`, columns, cells) is at [skelter-base/src/rows.rs:184-199](file:///home/user/svelters/crates/skelter-base/src/rows.rs)

**Canvas (verified in code)**
- `skelter-canvas` holds "millions of primitives in `f64` world coordinates, culled and cut by level of detail per
  chunk, drawn by analytic shaders". Pan and zoom are one camera change on the compositor, with no UI commit and no
  re-upload. The static plane is cached to a texture; an overlay plane (hover, selection, rubber bands) redraws alone.
  Its primitives are rects, segments, circles, arcs, polygons and labels, with Slug GPU text.
  — [skelter-canvas/src/lib.rs:1-21](file:///home/user/svelters/crates/skelter-canvas/src/lib.rs)
- cadatomic uses it for its 2D canvases: "canvas2d's picture as a compositor texture, the overlay plane, pointer
  gestures". — [cadatomic-app/src/skelter/mod.rs:19](file:///home/user/cadatomic/crates/cadatomic-app/src/skelter/mod.rs)

**Text (verified in dependency lists and code)**
- `skelter-text` shapes with parley, finds fonts with fontique (fontconfig is loaded at runtime) and reads outlines
  with skrifa. `mono.rs` is a "fontless monospace layout", the test double for the shaping seam.
  — [skelter-text/Cargo.toml](file:///home/user/svelters/crates/skelter-text/Cargo.toml), [mono.rs:1-4](file:///home/user/svelters/crates/skelter-text/src/mono.rs)

**App structure (verified in code)**
- The `svelters-skelter` backend runs Svelte through QuickJS on skelter's tree. It has many Svelte roots in one host
  (`embed`), `use:` behaviours routed to Rust factories, and native templates.
  — [svelters-skelter/src/lib.rs:1-20](file:///home/user/svelters/crates/svelters-skelter/src/lib.rs)
- cadatomic's shell is the model of the pattern. skelter owns the window, the device and the frame loop. Panes,
  chrome and Svelte roots are native nodes. `ShellApp::frame` runs once per frame that needs it. The Svelte palette is
  "its own panel". — [cadatomic-app/src/skelter/mod.rs:1-25](file:///home/user/cadatomic/crates/cadatomic-app/src/skelter/mod.rs)

### Inferences
- **Timeline canvas.** `skelter-canvas` fits a scrubbable horizontal timeline with marks well: f64 world x (event
  index or tick count), per-chunk LOD, compositor pan and zoom, and an overlay plane for the playhead and hover. The
  work is the timeline's own interaction and chunking, roughly 1–2 weeks. It does not need new toolkit code.
- **Call tree over time with millions of nodes.** Not ready as is. Realising rows is virtual, but every expand clones
  every visible label and rebuilds the flat vector (tree.rs:206-238), and cursor lookup is a linear scan
  (virtual_list.rs:50-52). With 10^6 visible rows each expand would cost O(10^6) allocations. Options: use the
  JS-flattened mode with a windowed model, or add a Rust row-model tree (like the table's `RowModel`) where the host
  answers `len`/`row(i)` from an engine-side index. That is a medium change to skelter-widgets (about 1–2 weeks). It
  is svelters' code, so it goes as a request to the svelters coordinator.
- **Memory, watch and query tables.** The `table` with a Rust `RowModel` is the right base. Missing: scroll bars and a
  column menu. For a hex memory view, a monospace cell painter is needed (`table/text_painter.rs` exists; I did not
  read it). Small to medium.
- **Linux backend choice.** hopper on Linux would run on winit by default. The own Wayland backend is opt-in, and the
  GTK and Qt backends are irrelevant to a skelter app.

### Gaps
- No skelter test or CI result was run here, and the newest merges are CI-unverified. I cannot say the current master
  works on screen on Linux.
- I did not measure `virtual-list` or `table` with 10^6 rows. The cost claims above come from reading the code.
- I did not read `skelter-core::virt` (the `RowHeights` Fenwick tree) or `table/state.rs` in full.

## berth: docking features, and use outside cadatomic

### Takeaway
berth has a substantial headless docking model, `berth-core` (about 14k lines: tab groups, splits, edge regions,
floats, multiple windows, layout undo, a versioned layout file, profiles), and a skelter renderer, `skelter_berth`
(about 5k lines). Neither depends on cadatomic. Its lane record says `skelter_berth` was never run when it landed, and
several features are left out. cadatomic now links it at the pinned sha. It is usable from hopper after some
hardening work.

### Cited Findings
- berth is three crates: `berth-core` (headless; no egui, winit, wgpu or tokio; builds for wasm32), `egui_berth`, and
  `skelter_berth` ("unfinished, see `docs/SKELTER-BERTH.md`"). — [berth/README.md](file:///home/user/berth/README.md) (stated in a doc)
- `berth-core` modules: apply, area, avail, busy, check, command, dock, drag, drop, file, geom, host, ids, info, model,
  policy, query, solver, undo, workspace. — [berth/crates/berth-core/src/](file:///home/user/berth/crates/berth-core/src/) (verified: directory listing)
- Tests (stated in a doc): behaviour rules, 150 KDDW catalogue rows, golden layout files v1/v2 with migrations,
  proptests (undo/redo identity, save/load identity). — [berth/HANDOFF.md](file:///home/user/berth/HANDOFF.md)
- `skelter_berth`: the dock is a subtree of skelter nodes under a `berth-dock` root. The host implements `PaneHost`
  (`mount`, `update`, `unmount`) and fills pane slots with its own skelter nodes, an egui island or a Svelte root.
  Tear-off windows come back as `WindowRequest`s. It re-exports `berth_core` and `skelter_core`.
  — [skelter_berth/src/lib.rs:1-19](file:///home/user/berth/crates/skelter_berth/src/lib.rs) (verified in code)
- `skelter_berth` takes the skelter crates by git `branch = "master"`, not by rev.
  — [skelter_berth/Cargo.toml](file:///home/user/berth/crates/skelter_berth/Cargo.toml) (verified)
- The lane record (2026-10-25) says: built only with `cargo check`; tests not run; "No GPU or windowed run: nothing
  here has been seen on screen". Not done: keyboard carry mode and `DockKeys`, float resize handles, window placement
  memory, the tab-list context menu, auto-scroll while dragging. It also lists skelter gaps (no global pointer capture
  across windows, no screen-coordinate window rects or z-order).
  — [berth/docs/SKELTER-BERTH.md](file:///home/user/berth/docs/SKELTER-BERTH.md) (stated in a doc)
- cadatomic depends on `skelter_berth` at rev `8ac0cce…` and has a dock seam and layout file.
  — [cadatomic/Cargo.toml:54](file:///home/user/cadatomic/Cargo.toml), [cadatomic-app/src/dock/](file:///home/user/cadatomic/crates/cadatomic-app/src/dock/)
- Licence: "none yet (removed until decided; `publish = false`)". — [berth/README.md](file:///home/user/berth/README.md)

### Inferences
- A hopper shell can use `berth-core` plus `skelter_berth` directly, through the same `PaneHost` seam cadatomic uses.
  Copying cadatomic's `dock/seam.rs` and `layout_file.rs` as a reference is a few days' work.
- The `branch = "master"` git dependency means any build can pick up a different skelter. Pin it, or use a
  `[patch]`, in hopper.
- Since berth has no licence, hopper's licence is limited if hopper is ever published. That is a user decision.

### Gaps
- I could not confirm from the code whether cadatomic's later batches fixed the "not run" items in `skelter_berth`.
  That needs cadatomic's test records or a run.

## quire: a read-only source viewer with gutter marks, inline values and jump-to-line

### Takeaway
quire has both an egui widget (`egui_quire`) and a skelter widget (`skelter_quire`). cadatomic uses `skelter_quire`,
not egui. The quire `CLAUDE.md` line "egui 0.37" is out of date. Already there: read-only mode, reveal-to-position,
virtual scroll, inlay hints as injected text (usable for inline values), diagnostics, a current-line colour, and a
large-file policy. Missing for hopper: a host-defined gutter (hit counts, an execution arrow, breakpoints), arbitrary
line highlights, and a **Rust grammar**.

### Cited Findings
- quire's crates are quire-core (about 27k lines, headless), quire-text, quire-syntax, egui_quire (about 14k lines)
  and skelter_quire (about 8.6k lines). — [quire/crates/](file:///home/user/quire/crates/) (verified)
- `skelter_quire` draws an `EditorView` of a shared `Document`. It shapes the viewport lines plus overscan, has its
  own compositor layers (scroll and caret blink as fast commits), and is also a `code-editor` kind and an IME client.
  — [skelter_quire/src/lib.rs:1-29](file:///home/user/quire/crates/skelter_quire/src/lib.rs) (verified)
- cadatomic uses `skelter_quire` for its source tab: "on skelter, its editor (a `<code-editor>` node over the file's
  document)". — [cadatomic-app/src/ide/mod.rs:1-4](file:///home/user/cadatomic/crates/cadatomic-app/src/ide/mod.rs), [cadatomic/Cargo.toml:80](file:///home/user/cadatomic/Cargo.toml)
- `read_only: bool` is in the config. — [quire-core/src/config.rs:221](file:///home/user/quire/crates/quire-core/src/config.rs).
  The IME path checks it. — [skelter_quire/src/ime.rs:159,183](file:///home/user/quire/crates/skelter_quire/src/ime.rs)
- `SkelterEditor::reveal(doc, view, pos, center)` scrolls to a position, optionally centred.
  — [skelter_quire/src/editor.rs:723](file:///home/user/quire/crates/skelter_quire/src/editor.rs)
- The gutter list holds only "diagnostic marks, line numbers, fold markers"; `record_gutter` paints line numbers and
  fold chevrons. — [skelter_quire/src/scene.rs:7, 444-521](file:///home/user/quire/crates/skelter_quire/src/scene.rs)
- The theme has a `current_line` colour (the cursor's line). — [skelter_quire/src/theme.rs:32,83](file:///home/user/quire/crates/skelter_quire/src/theme.rs)
- Inlay hints are drawn as injected text: `InlayHint`, `set_inlay_hints` on the view, and inlay hint sources on the
  document. — [quire-core/src/inlay_hints.rs:118,614](file:///home/user/quire/crates/quire-core/src/inlay_hints.rs), [view.rs:666](file:///home/user/quire/crates/quire-core/src/view.rs), [document.rs:573](file:///home/user/quire/crates/quire-core/src/document.rs)
- Diagnostics come in through `Document::set_diagnostics`. — [quire-core/src/document.rs:604](file:///home/user/quire/crates/quire-core/src/document.rs)
- A large-file policy turns tokenisation, folding and bracket matching off past Monaco's thresholds.
  — [quire-core/src/large.rs:1-15](file:///home/user/quire/crates/quire-core/src/large.rs)
- quire-syntax grammars are Svelte, TypeScript, JavaScript, CSS and TOML. **There is no Rust, C or assembly grammar.**
  — [quire-syntax/Cargo.toml:3, 11-30](file:///home/user/quire/crates/quire-syntax/Cargo.toml) (verified)
- Performance (stated in a doc, QE4): typing p99 0.27 ms, scroll p99 0.65 ms, open 40 ms, a 1M-char line 0.48 ms. The
  last full `check.sh` passed with 603 tests and 6 ignored (`f220847`). — [quire/STATUS.md](file:///home/user/quire/STATUS.md), `git log -1`

### Inferences
- **Source view.** Use `skelter_quire` read-only, with `reveal` for jumping to a line.
- **Inline values.** Inlay hints can carry them (the "Rust analyzer"-style injected text). Small, mainly host work.
- **Gutter annotations** (hit counts, current execution line, breakpoints). These need a new gutter-decoration API in
  quire-core and painting in `record_gutter`, plus whole-line background decorations (Monaco's `isWholeLine`
  decorations). Medium, perhaps 1 week. It is quire's code, so it goes as a request to quire's owner.
- **Rust and disassembly highlighting.** Add `tree-sitter-rust` behind a new feature (small). Disassembly needs a
  grammar (for example tree-sitter-asm, if one fits) or semantic tokens fed by the engine. No disassembly view exists
  anywhere in these repos. Building it on quire (one line per instruction, inlays for addresses and bytes) or on the
  table is medium work.

### Gaps
- I could not read cadatomic's pinned quire `9c84baf`, because it is not in the local checkout.
- I did not confirm whether quire-core has general "decorations" (range highlights) under another name. A search for
  `decoration`, `glyph_margin` and `line_highlight` found nothing in quire-core or skelter_quire.

## cadatomic: what hopper can copy, and what it must not

### Takeaway
Worth copying: the skelter shell pattern, the job registry (std threads, no tokio, results drained once per frame,
cancellation, a real idle barrier), and the NDJSON JSON-RPC 2.0 framing over a Unix socket. Not worth copying: the
document, command and transaction machinery built around CAD documents, the Parasolid and kernel seams, and the
100k-line app crate. No navigation history (back/forward) exists to copy.

### Cited Findings
- Crate sizes: cadatomic-core about 99k lines, cadatomic-app about 102k, cadatomic-ui about 26k, cadatomic-cli about
  5k. — line counts from `find … | wc -l` over [cadatomic/crates/](file:///home/user/cadatomic/crates/)
- Jobs: "every executor call runs off the UI thread as a job". The `JobPool` trait has `StdThreadPool` (bounded std
  threads) and `DirectPool` (tests). The `Jobs` registry provides tethers, coalescing, cancellation, progress and
  publication through a channel. `IdleBarrier` is the real `wait_idle`. "No tokio, no egui: the app drains results in
  `App::logic` through `Jobs::drain`." — [cadatomic-core/src/jobs.rs:1-10](file:///home/user/cadatomic/crates/cadatomic-core/src/jobs.rs)
- The job registry exports `DocumentKey`, `HeadToken`, `Tether` and `EMPTY_DRAFT_FINGERPRINT`, so jobs are tied to
  document revisions. — [jobs.rs:24-28](file:///home/user/cadatomic/crates/cadatomic-core/src/jobs.rs)
- RPC: "newline-delimited JSON-RPC 2.0 over a Unix socket, std threads only". The socket directory is 0700, with a
  token handshake and no TCP. Methods are `commands.list`, `command.run`, `session.hello` and `wait_idle`.
  — [cadatomic-cli/src/server.rs:1-13](file:///home/user/cadatomic/crates/cadatomic-cli/src/server.rs).
  Framing caps a line at 4 MiB and uses standard error codes. — [cadatomic-cli/src/protocol.rs:1-20](file:///home/user/cadatomic/crates/cadatomic-cli/src/protocol.rs)
- Undo exists for document history (`Document::undo`) and text (`text.rs` `undo`).
  — [cadatomic-core/src/document.rs:478](file:///home/user/cadatomic/crates/cadatomic-core/src/document.rs), [text.rs:267](file:///home/user/cadatomic/crates/cadatomic-core/src/text.rs).
  A search of cadatomic-core for `NavHistory`, `go_back`, `navigate_back` and "navigation history" found nothing.
- The project rules make every document change a typed command through one registry and one transaction path, with
  "ids, never labels". Parasolid is a removable Wine subprocess. — [cadatomic/CLAUDE.md](file:///home/user/cadatomic/CLAUDE.md) (stated in a doc)

### Inferences
- **Copy:**
  - the shell skeleton in `cadatomic-app/src/skelter/` (handler, document, keys, palette, menus, status, panes);
  - the job registry's design (std threads, a drain per frame, cancel tokens, an idle barrier for tests);
  - the JSON-RPC framing and the socket-security pattern.
- **Turn around:** cadatomic is an RPC *server* for agents. hopper's UI is an RPC *client* of the engine. So the
  framing transfers, but the server and method set do not. One reader thread that feeds a channel drained per frame
  fits the jobs pattern.
- **Don't copy:**
  - the document, transaction and proposal model, and the `DocumentKey`/`HeadToken` tethers. A debugger's navigation
    is not a document edit. A navigation undo stack is a much simpler stack of positions (event, frame, address).
  - the kernel seam, Parasolid, Copperfield and ECAD crates;
  - the tour and scenario machinery, unless hopper wants scripted UI tests.
- **Effort.** Keyboard navigation comes from skelter (keys, focus owner) and quire's VS Code keymap. Navigation
  undo/redo is new, small, app-side code.

### Gaps
- I did not read the cadatomic command registry (`cadatomic-core/src/command/`) or `keymap.rs` in detail, so I did not
  judge whether its keymap layer is reusable on its own.

## cadabra3: what `decision-trace` records, and how two runs could be aligned

### Takeaway
cadabra3 records two things:
- the **decision ledger**, always on: one per operation, with near and band decisions kept in full and an
  order-independent digest;
- with feature `decision-trace`, an in-memory, thread-local, **call-ordered** list of `Event`s.

Each traced event is either a ported `f64` comparison (stable `SiteName`, kind, operands, outcome, role, with
file:line as debug only) or a `decide()` evaluation (expression id, role, entity pair, rung, band, enclosure, class).
There is no on-disk trace format. Consumers write their own TSV and JSON summaries. No tool diffs two runs event by
event today. The nearest alignment key is casegen's (site key, per-site call ordinal). CADabra vs Parasolid is
compared on outputs, plus RE-side gdb hardware-breakpoint traces. Nothing in cadabra3, parasolid-re or parasolid-rs
uses rr.

### Cited Findings
**Trace events (verified in code)**
- With `decision-trace` on, the comparison helpers and `decide()` push one `Event` per call into a thread-local
  collector, only while a `capture` is active. Off, the helpers compile to the bare comparison. "An event carries the
  `SiteName` the caller passed. The helper's caller (file, line, column, from `#[track_caller]`) is kept as debug
  information only." — [cadabra-decide/src/trace_log.rs:1-11](file:///home/user/cadabra3/crates/cadabra-decide/src/trace_log.rs)
- `CmpEvent { site, file, line, column, kind: CmpKind, lhs: f64, rhs: f64, outcome: bool, role: Option<u8> }`, with
  `margin()` (signed, flips at zero). — [trace_log.rs:20-50](file:///home/user/cadabra3/crates/cadabra-decide/src/trace_log.rs)
- `DecisionEvent { expr: ExprId, role: u8, first: u64, second: u64, rung: u8, band, lo, hi, class: Negative|Within|Positive }`.
  Memo hits are not recorded. — [trace_log.rs:52-96](file:///home/user/cadabra3/crates/cadabra-decide/src/trace_log.rs)
- `Trace { events: Vec<Event> }` holds the events "in call order". `capture`/`capture_unwind` nest, and a panic
  returns the events recorded before it. — [trace_log.rs:98-178](file:///home/user/cadabra3/crates/cadabra-decide/src/trace_log.rs)
- The public module `cadabra_decide::trace` exists only with the feature.
  — [cadabra-decide/src/lib.rs:100-106](file:///home/user/cadabra3/crates/cadabra-decide/src/lib.rs)
- The feature chain: `cadabra-decide/decision-trace = []`, enabled through intersect, ops and bench.
  — [cadabra-decide/Cargo.toml:25](file:///home/user/cadabra3/crates/cadabra-decide/Cargo.toml), [cadabra-ops/Cargo.toml:47](file:///home/user/cadabra3/crates/cadabra-ops/Cargo.toml), [cadabra-bench/Cargo.toml:33](file:///home/user/cadabra3/crates/cadabra-bench/Cargo.toml)

**SiteName (verified in code)**
- `SiteName(&'static str)` is a stable dotted name (lower-case `[a-z0-9_]` segments, at least two), validated at
  compile time by `site!`. "Moving, reformatting or renaming the code around it does not change it". Uniqueness is
  checked by `crates/cadabra/tests/site_names.rs`. — [cadabra-core/src/site_name.rs:3-13, 23-46, 67-73](file:///home/user/cadabra3/crates/cadabra-core/src/site_name.rs)

**Decision ledger (verified in code)**
- There is one ledger per operation. Far decisions are counts per role and rung. Near decisions (margin below k=4
  bands) and band decisions are kept in full (key, role, expression id, enclosure, band, rung) "so certification can
  replay them". The minimum margin is tracked. The digest "depends on the set of decisions, not on the order".
  — [cadabra-decide/src/ledger.rs:1-18, 118-150](file:///home/user/cadabra3/crates/cadabra-decide/src/ledger.rs)
- `LedgerEntry { site, role, band, rung: Option<Rung>, value: Enclosure, outcome: Sign|WithinTol|Compare|ToleranceSnap, dependence, witness }`.
  Rungs are Structural, Bound, DoubleDouble, Expansion and Rational. `ToleranceSnap` records an exact sign proved
  inside PK's band. — [ledger.rs:36-101](file:///home/user/cadabra3/crates/cadabra-decide/src/ledger.rs)
- The ledger has `entries()`, `digest()` and a `CanonicalEncode` for `LedgerEntry` (domain `VERSION = 3`). No serde
  serialization. — [ledger.rs:289, 377, 472, 551](file:///home/user/cadabra3/crates/cadabra-decide/src/ledger.rs)

**Consumers and formats (verified in code)**
- `cadabra-bench/tests/exact_layer_trace.rs` runs every corpus job in one `capture`. It writes
  `<corpus>-decisions.tsv` (evaluations per site per rung, 0–4, and class counts), `<corpus>-cmp.tsv` (calls per
  comparison site and calls within one band) and `<corpus>-cases.tsv`. These are aggregates, not event streams.
  — [exact_layer_trace.rs:1-18](file:///home/user/cadabra3/crates/cadabra-bench/tests/exact_layer_trace.rs)
- `cadabra-casegen` runs box and cylinder Booleans traced (`trace::capture_unwind`). It keys sites as
  `SiteKey::Cmp { name, kind }` or `SiteKey::Decision { expr, role }`, writes `coverage.tsv` and `cases.json`, and
  keeps a per-site call `ordinal` ("which call of the site in that run", 0-based) for its witnesses.
  — [casegen/src/scene.rs:10, 250-300](file:///home/user/cadabra3/oracle-tests/parasolid/casegen/src/scene.rs),
  [casegen/src/sites.rs:1-45](file:///home/user/cadabra3/oracle-tests/parasolid/casegen/src/sites.rs),
  [casegen/src/coverage.rs:30, 145-147](file:///home/user/cadabra3/oracle-tests/parasolid/casegen/src/coverage.rs)

**CADabra vs Parasolid today (verified in code and files)**
- The oracle workspace `oracle-tests/parasolid/tests/` has 60 entries (59 test files plus `support`). They compare
  outputs, for example Boolean records "numbers to 1e-9 relative", edge lengths 1e-6.
  — [box_cylinder_records_agree.rs:1-20](file:///home/user/cadabra3/oracle-tests/parasolid/tests/box_cylinder_records_agree.rs), [tests/](file:///home/user/cadabra3/oracle-tests/parasolid/tests/)
- Decision-level Parasolid evidence arrives as RE fixtures: 1150 vendored P1 JSON fixtures with bit-exact inputs as
  `{dec, hex}`, the binary sha256 and case ids, plus `cases.tsv` mapping cases to PK function addresses.
  — [cadabra-intersect/tests/data/p1/](file:///home/user/cadabra3/crates/cadabra-intersect/tests/data/p1/) (SOURCE: parasolid-re `27dbe8d`)
- `porting/ledger.tsv` (1919 lines) maps packet case → Rust module path → oracle test. It does not map case → `SiteName`.
  — [cadabra3/porting/ledger.tsv:1-2](file:///home/user/cadabra3/porting/ledger.tsv)

**rr, gdb and trace-diff search** (git-tracked files, all types, `git grep -I`; cadabra3 5914 files, parasolid-re
38112, parasolid-rs 1341)
- **rr:** no use in any of the three. Whole-word `rr` hits are all local variable names (for example
  `cadabra-ops/src/query/distance/pairs/edge_face.rs:282`, parasolid-re `replay_r124.py:175`). There are no hits for
  `reverse-continue`, `reverse-step`, `time travel` or `undodb`. cadabra3's only "record and replay" hit is a plan
  line about a typed `Operation` enum. — [cadabra3/docs/plan/11-public-kernel-api.md:306](file:///home/user/cadabra3/docs/plan/11-public-kernel-api.md)
- **gdb, parasolid-re:** used heavily (12,375 line hits; 896 tracked `.gdb` scripts). These are read-only
  hardware-breakpoint traces of `pskernel.dll` under Wine, with gdb as wine's parent. Breakpoints are rebased from the
  0x180000000 image base to the Wine mapping (0x6ffff6df0000); a run at another base counts as failed.
  — [parasolid-re/AGENTS.md](file:///home/user/parasolid-re/AGENTS.md) "Read-only hardware-breakpoint tracing";
  [r10/tools/trace.gdb:1-30](file:///home/user/parasolid-re/artifacts/port/P1-ssi-dispatch/20261016T214234Z-r10/tools/trace.gdb)
- **gdb, cadabra3:** 432 hits, all in vendored RE fixtures and `cases.tsv` (evidence grades), none in code.
  **parasolid-rs:** 0 hits.
- **Trace diffs (parasolid-re only):**
  - `replay_r124.py` replays a Python model from traced inputs "bit for bit" and prints `MISS` lines where model and
    trace disagree, a model-vs-trace diff. — [replay_r124.py:1-25, 175](file:///home/user/parasolid-re/artifacts/port/P5-ssi-marcher/20261023T023931Z-r124/tools/replay_r124.py)
  - `cmp_traced.py` (4 tracked copies) checks that traced and untraced PK output is identical ("26 runs compared, 0
    different"). — [c468/REPORT.md:46](file:///home/user/parasolid-re/artifacts/port/P1-ssi-dispatch/20261016T221706Z-c468/REPORT.md)
  - cadabra3 has no trace-diff tool. Its "divergence" hits are reviews, the divergence theorem, and a ulp-divergence
    note. — [cadabra3/STATUS.md:6340](file:///home/user/cadabra3/STATUS.md)

### Inferences
- **Divergence query for f64 vs exact.** The trace already holds what is needed: `rung` and `class` per decision,
  `outcome` and `margin` per comparison. A first divergence query is: align both runs' `Event` streams by
  `(SiteKey, per-site ordinal)` (casegen's key), and report the first event where `outcome` or `class` differs, with
  margins. Ordinal alignment breaks once control flow diverges, because later ordinals shift. So the query should
  report the *first* mismatch and stop, or align by call context. `file:line` is debug only and cannot be the key.
- **CADabra vs Parasolid.** There is no common event stream. Parasolid has addresses, not `SiteName`s, and RE's
  traces are per-breakpoint gdb logs. Aligning them needs a site → PK address table. `cases.tsv` gives case → address,
  but nothing gives case → `SiteName`, so that is new mapping work for the cadabra3 and RE lanes.
- **What hopper would need from cadabra3.** A serialised event stream (the `Event` types are `Copy`, plain fields, no
  serde) and a stable per-run scene id. The `&'static str` site names and `ExprId` serialize easily. Deterministic
  entity ids across runs need checking (`first`/`second` are u64 entity keys).
- **rr's role.** Nothing to reuse. hopper's rr integration would be the first in this ecosystem. The RE gdb scripts
  (Wine, hbreak, base rebasing) show the target environment: pskernel runs under Wine, where rr support is a separate
  question I did not research.

### Gaps
- I did not confirm whether entity ids (`first`, `second`) are deterministic across two runs of the same scene.
- I did not read `cadabra-decide/src/site.rs` (`CmpLog`, `note_cmp`), so I do not know if per-thread ordering holds
  across any parallelism in the Boolean driver.
- Whether rr can record Wine-hosted pskernel runs is outside this repo-reading scope.
