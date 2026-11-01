//! The window's skelter nodes, the berth dock that holds the panes, and the sync from the
//! [`Viewer`] into them once per frame.

use std::collections::HashMap;
use std::sync::Arc;

use berth_core::{AllowAll, Dock, DockCommand, Layout, LogicalPx, PaneId, PaneInfo, PaneLocation, Side, Target, WindowId as DockWindow};
use hopper_core::api::{StepKind};
use hopper_core::target::Direction;
use skelter_base::geom::{Rect, Size};
use skelter_base::ids::NodeId;
use skelter_core::common::{ANCHOR, ARIA_LABEL, DISABLED, TITLE};
use skelter_core::{Document, KindId, ListenFlags, PropId, SignalId, StyleInput, Value};
use skelter_berth::{DockView, PaneCx, PaneHost};

use crate::app::{QueryLine, UrlBox, Viewer, value_summary};
use crate::nav::Command;
use crate::rows::{Disclosure, RowCell, RowLine, RowList, RowsHost, Tone, Width};
use crate::source::SourceHost;
use crate::timeline::TimelineHost;
use crate::tree::{Expansion, Row};

pub mod w {
    use skelter_core::{KindId, PropId, SignalId};
    pub const BOX: KindId = KindId::of("box");
    pub const LABEL: KindId = KindId::of("label");
    pub const BUTTON: KindId = KindId::of("button");
    pub const TEXT_EDIT: KindId = KindId::of("text-edit");
    pub const TABLE: KindId = KindId::of("table");
    pub const LABEL_TEXT: PropId = PropId::of("label", "text", "string");
    pub const LABEL_SELECTABLE: PropId = PropId::of("label", "selectable", "bool");
    pub const BUTTON_TEXT: PropId = PropId::of("button", "text", "string");
    pub const BUTTON_SMALL: PropId = PropId::of("button", "small", "bool");
    pub const TEXT: PropId = PropId::of("text-edit", "text", "string");
    pub const TEXT_HINT: PropId = PropId::of("text-edit", "hint-text", "string");
    pub const CLICKED: SignalId = SignalId::of("button", "clicked", "void");
    pub const SUBMIT: SignalId = SignalId::of("text-edit", "submit", "string");
    pub const CANCEL: SignalId = SignalId::of("text-edit", "cancel", "void");
}

/// Toolbar and status bar heights (points); the dock fills the rest.
pub const TOOLBAR_H: f32 = 36.0;
pub const STATUS_H: f32 = 24.0;

/// The panes, by berth id, with their titles.
pub const PANES: [(&str, &str); 6] =
    [("source", "Source"), ("stack", "Stack"), ("locals", "Locals"), ("timeline", "Timeline"), ("calls", "Calls"), ("query", "Query")];

pub const CSS: &str = r#"
.hop-root { display: flex; flex-direction: column; width: 100%; height: 100%; background-color: #1f1f1f; color: #cccccc; font-size: 12px; }
.hop-bar { display: flex; flex-direction: row; align-items: center; gap: 4px; padding: 0 8px; height: 36px; flex-shrink: 0; background-color: #181818; border-bottom: 1px solid #2b2b2b; }
.hop-sep { width: 1px; height: 20px; background-color: #3c3c3c; margin: 0 6px; }
.hop-pos { margin-left: 12px; color: #9d9d9d; flex-shrink: 1; min-width: 0; }
.hop-url { width: 520px; }
.hop-dock { flex-grow: 1; min-height: 0; position: relative; }
.hop-status { display: flex; flex-direction: row; align-items: center; gap: 16px; padding: 0 10px; height: 24px; flex-shrink: 0; background-color: #007acc; color: #ffffff; }
.hop-status-error { background-color: #a1260d; }
.hop-grow { flex-grow: 1; }
.hop-fill { width: 100%; height: 100%; }
.hop-query { display: flex; flex-direction: column; width: 100%; height: 100%; }
.hop-console { margin: 4px; flex-shrink: 0; }
"#;

/// The panes' content nodes, mounted into berth's slots.
#[derive(Debug, Default)]
pub struct Panes {
    pub content: HashMap<PaneId, NodeId>,
}

impl PaneHost for Panes {
    fn mount(&mut self, cx: &mut PaneCx<'_>) {
        if let Some(&n) = self.content.get(cx.pane) {
            if let Some(p) = cx.doc.parent(n) {
                let _ = cx.doc.remove(p, n);
            }
            let _ = cx.doc.insert(cx.slot, n, None, None);
        }
    }

    fn pane_info(&self, pane: &PaneId) -> Option<PaneInfo> {
        let title = PANES.iter().find(|(id, _)| *id == pane.as_str())?.1;
        Some(PaneInfo { title: title.to_owned(), closable: false, ..PaneInfo::default() })
    }
}

/// The default layout: source in the centre with locals and the stack to its right, the
/// timeline under them, the calls and the query pane at the bottom.
pub fn default_layout() -> Dock {
    let mut dock = Dock::new(Layout::new());
    let p = |s: &str| PaneId::new(s);
    let main = DockWindow::main();
    let group_of = |dock: &Dock, pane: &str| match dock.layout().locate(&PaneId::new(pane)) {
        Some(PaneLocation::Group { group, .. }) => Some(group),
        _ => None,
    };
    let apply = |dock: &mut Dock, cmd: DockCommand| {
        if let Err(e) = dock.apply(cmd, &AllowAll) {
            eprintln!("hopper-gui: default layout: {e:?}");
        }
    };
    apply(&mut dock, DockCommand::Open { pane: p("source"), at: None, initial_size: None });
    if let Some(g) = group_of(&dock, "source") {
        apply(&mut dock, DockCommand::Split { group: g, side: Side::Right, pane: Some(p("locals")), initial_size: Some(LogicalPx(420)) });
    }
    if let Some(g) = group_of(&dock, "locals") {
        apply(&mut dock, DockCommand::Split { group: g, side: Side::Bottom, pane: Some(p("stack")), initial_size: Some(LogicalPx(220)) });
    }
    apply(&mut dock, DockCommand::Open { pane: p("timeline"), at: Some(Target::WindowEdge { window: main.clone(), side: Side::Bottom }), initial_size: Some(LogicalPx(64)) });
    apply(&mut dock, DockCommand::Open { pane: p("calls"), at: Some(Target::WindowEdge { window: main.clone(), side: Side::Bottom }), initial_size: Some(LogicalPx(230)) });
    if let Some(g) = group_of(&dock, "calls") {
        apply(&mut dock, DockCommand::Split { group: g, side: Side::Right, pane: Some(p("query")), initial_size: Some(LogicalPx(640)) });
    }
    if let Some(g) = group_of(&dock, "source") {
        apply(&mut dock, DockCommand::FocusGroup { window: main, to: berth_core::FocusTarget::Group(g) });
    }
    dock
}

/// What a widget event on one of the window's own nodes means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Command(Command),
    UrlSubmit,
    UrlCancel,
    ConsoleSubmit,
    CallsActivated,
    CallsSelected,
}

/// The window's nodes.
pub struct Ui {
    pub root: NodeId,
    pub dock_space: NodeId,
    pub url: NodeId,
    pub position: NodeId,
    pub status: NodeId,
    pub status_text: NodeId,
    pub index_text: NodeId,
    pub back: NodeId,
    pub forward: NodeId,
    pub source: NodeId,
    pub stack: NodeId,
    pub locals: NodeId,
    pub timeline: NodeId,
    pub calls: NodeId,
    pub query: NodeId,
    pub console: NodeId,
    pub actions: HashMap<NodeId, Action>,
    pub panes: Panes,
    pub dock: Dock,
    pub view: DockView,
    epoch: i64,
    url_shown: bool,
}

impl std::fmt::Debug for Ui {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Ui").field("root", &self.root).finish_non_exhaustive()
    }
}

fn style(doc: &mut Document, n: NodeId, classes: &[&str], inline: &str) {
    doc.set_style(n, StyleInput { classes: classes.iter().map(|c| (*c).into()).collect(), inline: (!inline.is_empty()).then(|| inline.into()), ..Default::default() });
}

fn node(doc: &mut Document, parent: Option<NodeId>, kind: KindId, classes: &[&str], inline: &str) -> NodeId {
    let n = doc.create(kind);
    style(doc, n, classes, inline);
    if let Some(p) = parent {
        let _ = doc.insert(p, n, None, None);
    }
    n
}

fn set(doc: &mut Document, n: NodeId, p: PropId, v: Value) {
    if doc.get_prop(n, p).as_ref() != Some(&v)
        && let Err(e) = doc.set_prop(n, p, v)
    {
        eprintln!("hopper-gui: {p:?}: {e:?}");
    }
}

fn listen(doc: &mut Document, n: NodeId, s: SignalId) {
    doc.listen(n, s, ListenFlags::default());
}

fn signal(doc: &Document, kind: KindId, name: &str) -> Option<SignalId> {
    doc.registry().get(kind).and_then(|e| e.spec.signal_named(name))
}

/// The toolbar's buttons: label, tooltip, command.
fn buttons() -> Vec<(&'static str, &'static str, Command)> {
    use Direction::{Forward, Reverse};
    vec![
        ("Back", "Back (Alt+Left)", Command::Back),
        ("Forward", "Forward (Alt+Right)", Command::Forward),
        ("|", "", Command::CopyUrl),
        ("Rev Continue", "Reverse continue (Shift+F5)", Command::Continue { dir: Reverse }),
        ("Rev Out", "Reverse step out (Ctrl+Shift+F11)", Command::Step { kind: StepKind::Out, dir: Reverse }),
        ("Rev Over", "Reverse step over (Ctrl+F10)", Command::Step { kind: StepKind::Over, dir: Reverse }),
        ("Rev Into", "Reverse step into (Ctrl+F11)", Command::Step { kind: StepKind::Into, dir: Reverse }),
        ("Into", "Step into (F11)", Command::Step { kind: StepKind::Into, dir: Forward }),
        ("Over", "Step over (F10)", Command::Step { kind: StepKind::Over, dir: Forward }),
        ("Out", "Step out (Shift+F11)", Command::Step { kind: StepKind::Out, dir: Forward }),
        ("Continue", "Continue (F5)", Command::Continue { dir: Forward }),
        ("|", "", Command::CopyUrl),
        ("Copy URL", "Copy the position URL (Ctrl+Shift+C)", Command::CopyUrl),
        ("Open URL", "Open a position URL (Ctrl+L)", Command::OpenUrlBox),
    ]
}

impl Ui {
    /// Builds the window's tree into `doc` (whose registry has skelter-widgets', berth's and
    /// hopper's kinds), with the services the panes read.
    pub fn build(doc: &mut Document, fonts: Arc<skelter_text::FontStore>) -> Ui {
        doc.set_service(RowsHost::default());
        doc.set_service(SourceHost::new(fonts.clone()));
        doc.set_service(TimelineHost::new());
        let root = node(doc, None, w::BOX, &["hop-root"], "");
        set(doc, root, ANCHOR, Value::Str("hopper".into()));
        let bar = node(doc, Some(root), w::BOX, &["hop-bar"], "");
        let mut actions = HashMap::new();
        let mut back = root;
        let mut forward = root;
        for (text, tip, cmd) in buttons() {
            if text == "|" {
                node(doc, Some(bar), w::BOX, &["hop-sep"], "");
                continue;
            }
            let b = node(doc, Some(bar), w::BUTTON, &[], "");
            set(doc, b, w::BUTTON_TEXT, Value::Str(text.into()));
            set(doc, b, TITLE, Value::Str(tip.into()));
            set(doc, b, ANCHOR, Value::Str(format!("button:{}", text.to_lowercase().replace(' ', "-")).into()));
            listen(doc, b, w::CLICKED);
            actions.insert(b, Action::Command(cmd));
            match cmd {
                Command::Back => back = b,
                Command::Forward => forward = b,
                _ => {}
            }
        }
        let url = node(doc, Some(bar), w::TEXT_EDIT, &["hop-url"], "display: none");
        set(doc, url, w::TEXT_HINT, Value::Str("hopper://<recording>/<event>.<tid>.<ticks>.<sub>".into()));
        set(doc, url, ARIA_LABEL, Value::Str("Position URL".into()));
        set(doc, url, ANCHOR, Value::Str("url-box".into()));
        let submit = signal(doc, w::TEXT_EDIT, "submit").unwrap_or(w::SUBMIT);
        let cancel = signal(doc, w::TEXT_EDIT, "cancel").unwrap_or(w::CANCEL);
        listen(doc, url, submit);
        listen(doc, url, cancel);
        actions.insert(url, Action::UrlSubmit);
        let position = node(doc, Some(bar), w::LABEL, &["hop-pos"], "");
        set(doc, position, w::LABEL_SELECTABLE, Value::Bool(true));
        set(doc, position, ANCHOR, Value::Str("position".into()));
        let dock_space = node(doc, Some(root), w::BOX, &["hop-dock"], "");
        let status = node(doc, Some(root), w::BOX, &["hop-status"], "");
        let status_text = node(doc, Some(status), w::LABEL, &["hop-grow"], "");
        set(doc, status_text, ANCHOR, Value::Str("status".into()));
        let index_text = node(doc, Some(status), w::LABEL, &[], "");
        set(doc, index_text, ANCHOR, Value::Str("index".into()));

        // the panes' content, mounted into berth's slots
        let source = node(doc, None, crate::source::KIND, &["hop-fill"], "");
        set(doc, source, ANCHOR, Value::Str("pane:source".into()));
        let stack = node(doc, None, crate::rows::KIND, &["hop-fill"], "");
        set(doc, stack, ANCHOR, Value::Str("pane:stack".into()));
        let locals = node(doc, None, crate::rows::KIND, &["hop-fill"], "");
        set(doc, locals, ANCHOR, Value::Str("pane:locals".into()));
        let timeline = node(doc, None, crate::timeline::KIND, &["hop-fill"], "");
        set(doc, timeline, ANCHOR, Value::Str("pane:timeline".into()));
        let calls = node(doc, None, w::TABLE, &["hop-fill"], "");
        set(doc, calls, ANCHOR, Value::Str("pane:calls".into()));
        set(doc, calls, ARIA_LABEL, Value::Str("Calls".into()));
        if let Some(s) = signal(doc, w::TABLE, "activated") {
            listen(doc, calls, s);
        }
        if let Some(s) = signal(doc, w::TABLE, "changed") {
            listen(doc, calls, s);
        }
        let query_box = node(doc, None, w::BOX, &["hop-query"], "");
        set(doc, query_box, ANCHOR, Value::Str("pane:query".into()));
        let query = node(doc, Some(query_box), crate::rows::KIND, &[], "flex-grow: 1; min-height: 0; width: 100%");
        let console = node(doc, Some(query_box), w::TEXT_EDIT, &["hop-console"], "");
        set(doc, console, w::TEXT_HINT, Value::Str("Evaluate an expression in the selected frame".into()));
        set(doc, console, ARIA_LABEL, Value::Str("Console".into()));
        set(doc, console, ANCHOR, Value::Str("console".into()));
        listen(doc, console, submit);
        actions.insert(console, Action::ConsoleSubmit);

        let mut panes = Panes::default();
        for (id, n) in [("source", source), ("stack", stack), ("locals", locals), ("timeline", timeline), ("calls", calls), ("query", query_box)] {
            panes.content.insert(PaneId::new(id), n);
        }
        if let Some(h) = doc.service::<RowsHost>() {
            for (n, cols, empty) in [
                (stack, vec![Width::Fixed(36.0), Width::Fill, Width::Fill], "No stack yet"),
                (locals, vec![Width::Fixed(170.0), Width::Fill, Width::Fixed(150.0)], "No locals"),
                (query, vec![Width::Fixed(170.0), Width::Fill, Width::Fixed(110.0), Width::Fixed(80.0)], ""),
            ] {
                h.lists.insert(n, RowList { columns: cols, empty: empty.into(), ..RowList::default() });
            }
        }
        if let Some(h) = doc.service::<SourceHost>() {
            h.panes.insert(source, crate::source::SourcePane::default());
        }
        let theme = skelter_widgets::theme::Theme::dark_modern();
        let measure = Box::new(skelter_text::TextSystem::new(fonts));
        let view = DockView::new(measure).with_painter(skelter_berth::SkelterChrome::from_theme(&theme)).auto_apply(true);
        Ui {
            root,
            dock_space,
            url,
            position,
            status,
            status_text,
            index_text,
            back,
            forward,
            source,
            stack,
            locals,
            timeline,
            calls,
            query,
            console,
            actions,
            panes,
            dock: default_layout(),
            view,
            epoch: 0,
            url_shown: false,
        }
    }

    /// Attaches the call table's model (once the table node exists in a window's tree is fine;
    /// the model service only needs the node id).
    pub fn attach_calls(&self, doc: &mut Document, v: &Viewer) {
        let model = Arc::new(crate::calls::CallRows::new(v.calls.clone()));
        if let Err(e) = skelter_widgets::table::model::TableModels::attach(doc, self.calls, model) {
            eprintln!("hopper-gui: the call table: {e:?}");
        }
    }

    /// Places and syncs the dock in a window of `size` (points). Returns whether the dock queued
    /// intents that need another frame.
    pub fn dock_frame(&mut self, doc: &mut Document, surface: skelter_base::ids::WindowId, size: Size) -> bool {
        let area = Rect::new(0.0, 0.0, size.w, (size.h - TOOLBAR_H - STATUS_H).max(0.0));
        let main = DockWindow::main();
        if self.view.root_of(&main).is_some() {
            self.view.set_area(&main, area);
        } else {
            self.view.attach_window(doc, &main, surface, self.dock_space, area);
        }
        let logic = self.view.logic(&mut self.dock, &mut self.panes);
        let out = self.view.sync(doc, &self.dock, &mut self.panes);
        !out.intents.is_empty() || !logic.intents.is_empty()
    }

    pub fn bump(&mut self, doc: &mut Document, n: NodeId, epoch: PropId) {
        self.epoch += 1;
        let _ = doc.set_prop(n, epoch, Value::I64(self.epoch));
    }

    /// Pushes what changed in `v` into the document.
    pub fn apply(&mut self, v: &mut Viewer, doc: &mut Document) {
        let ch = std::mem::take(&mut v.changed);
        if ch.source {
            let file = v.source.file.clone();
            if let Some(h) = doc.service::<SourceHost>()
                && let Some(p) = h.panes.get_mut(&self.source)
            {
                p.doc = v.source.doc.clone();
                p.generation = v.source.generation;
                p.message = v.source.message.clone();
                p.arrows = v.source.arrows.clone();
                p.hits = file.as_ref().and_then(|f| v.source.hits.get(f).cloned()).unwrap_or_default();
                if let Some(r) = v.source.reveal.take() {
                    p.reveal = Some(r);
                }
            }
            self.bump(doc, self.source, crate::source::EPOCH);
        }
        if ch.stack {
            let rows = stack_rows(v);
            self.set_rows(doc, self.stack, rows, (!v.frames.is_empty()).then_some(v.selected_frame));
        }
        if ch.locals {
            let rows = locals_rows(v);
            self.set_rows(doc, self.locals, rows, v.locals_selected);
        }
        if ch.calls {
            let _ = skelter_widgets::table::model::TableModels::refresh(doc, self.calls);
        }
        if ch.timeline {
            if let Some(h) = doc.service::<TimelineHost>() {
                h.model = v.timeline.clone();
            }
            self.bump(doc, self.timeline, crate::timeline::EPOCH);
        }
        if ch.query {
            let rows = query_rows(&v.query);
            let last = rows.len().checked_sub(1);
            self.set_rows(doc, self.query, rows, None);
            if let Some(h) = doc.service::<RowsHost>()
                && let Some(l) = h.lists.get_mut(&self.query)
            {
                l.reveal = last;
            }
        }
        if ch.status {
            let (text, tone) = v.status_text();
            let busy = if v.navigating > 0 { "  · running…" } else { "" };
            set(doc, self.status_text, w::LABEL_TEXT, Value::Str(format!("{text}{busy}").into()));
            let classes: &[&str] = if tone == Tone::Error { &["hop-status", "hop-status-error"] } else { &["hop-status"] };
            style(doc, self.status, classes, "");
            set(doc, self.index_text, w::LABEL_TEXT, Value::Str(v.index_text().into()));
            let pos = v.stop.as_ref().map(|s| s.at.url().to_string()).unwrap_or_default();
            set(doc, self.position, w::LABEL_TEXT, Value::Str(pos.into()));
            set(doc, self.back, DISABLED, Value::Bool(!v.nav.can_back()));
            set(doc, self.forward, DISABLED, Value::Bool(!v.nav.can_forward()));
        }
        if ch.url_box {
            match &v.url_box {
                UrlBox::Open(text) => {
                    style(doc, self.url, &["hop-url"], "");
                    set(doc, self.url, w::TEXT, Value::Str(text.as_str().into()));
                    style(doc, self.position, &["hop-pos"], "display: none");
                    doc.focus(self.url, skelter_core::FocusReason::Keyboard);
                    self.url_shown = true;
                }
                UrlBox::Closed => {
                    style(doc, self.url, &["hop-url"], "display: none");
                    style(doc, self.position, &["hop-pos"], "");
                    if self.url_shown {
                        doc.focus(self.source, skelter_core::FocusReason::Programmatic);
                    }
                    self.url_shown = false;
                }
            }
        }
        if let Some(text) = v.copy.take() {
            let copied = doc
                .service::<Box<dyn skelter_platform::Clipboard>>()
                .is_some_and(|c| c.set_text(skelter_platform::ClipboardKind::Clipboard, &text));
            if !copied {
                // the clipboard is lent only during dispatch; keep it for the next one
                v.copy = Some(text);
            }
        }
    }

    fn set_rows(&mut self, doc: &mut Document, n: NodeId, rows: Vec<RowLine>, selected: Option<usize>) {
        if let Some(h) = doc.service::<RowsHost>()
            && let Some(l) = h.lists.get_mut(&n)
        {
            l.rows = rows;
            l.selected = selected.filter(|s| *s < l.rows.len());
            if l.selected.is_some() {
                l.reveal = l.selected;
            }
        }
        self.bump(doc, n, crate::rows::EPOCH);
    }
}

fn stack_rows(v: &Viewer) -> Vec<RowLine> {
    v.frames
        .iter()
        .enumerate()
        .map(|(i, f)| {
            let func = f.function.clone().unwrap_or_else(|| format!("{:#x}", f.pc));
            let loc = f.location.as_ref().map(|l| format!("{}:{}", crate::app::short_file(&l.file), l.line)).unwrap_or_default();
            let inl = if f.inlined { " (inlined)" } else { "" };
            RowLine {
                depth: 0,
                disclosure: Disclosure::None,
                cells: vec![
                    RowCell::new(format!("#{i}"), Tone::Muted),
                    RowCell::new(format!("{func}{inl}"), if i == 0 { Tone::Heading } else { Tone::Normal }),
                    RowCell::new(loc, Tone::Muted),
                ],
            }
        })
        .collect()
}

fn locals_rows(v: &Viewer) -> Vec<RowLine> {
    let t = &v.locals;
    t.rows()
        .iter()
        .map(|r| match r {
            Row::Node { key, depth } => {
                let Some(val) = t.item(key) else { return RowLine::default() };
                let disclosure = match t.expansion(key) {
                    Some(Expansion::Collapsed) => Disclosure::Collapsed,
                    Some(Expansion::Expanded) => Disclosure::Expanded,
                    _ => Disclosure::None,
                };
                let (summary, tone) = value_summary(val);
                let name_tone = if tone == Tone::Muted { Tone::Muted } else { Tone::Normal };
                RowLine {
                    depth: *depth,
                    disclosure,
                    cells: vec![RowCell::new(val.name.clone(), name_tone), RowCell::mono(summary, tone), RowCell::new(val.type_name.clone(), Tone::Muted)],
                }
            }
            Row::Loading { depth, .. } => RowLine { depth: *depth, disclosure: Disclosure::None, cells: vec![RowCell::new("loading…", Tone::Muted)] },
            Row::More { depth, loaded, total, .. } => {
                let more = total.map(|t| format!("… {} more (double-click)", t.saturating_sub(*loaded))).unwrap_or_else(|| "… more (double-click)".into());
                RowLine { depth: *depth, disclosure: Disclosure::None, cells: vec![RowCell::new(more, Tone::Accent)] }
            }
        })
        .collect()
}

fn query_rows(q: &[QueryLine]) -> Vec<RowLine> {
    q.iter().map(|l| RowLine { depth: l.depth, disclosure: Disclosure::None, cells: l.cells.clone() }).collect()
}
