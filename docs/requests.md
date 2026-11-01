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
