//! [`HopperDocument`]: the window's tree as skelter's app runner drives it. It wraps skelter's
//! `CoreDocument`, takes the debugger keys before any pane sees them, routes widget events to
//! the [`Viewer`], and once per frame drains the jobs, syncs the view and the dock, and runs the
//! document's frame.

use std::sync::Arc;

use skelter::seams::{Dispatched, DocumentApi};
use skelter::CoreDocument;
use skelter_base::accesskit::ActionRequest;
use skelter_base::anim::AnimationEvent;
use skelter_base::commit::{Commit, FastCommit};
use skelter_base::event::{InputEnvelope, InputEvent, KeyState, WindowEvent};
use skelter_base::geom::{Point, Rect, Scale, Size, Vec2};
use skelter_base::ids::{CanvasId, PropNodeId, Revision, WindowId};
use skelter_base::input::InputPurpose;
use skelter_base::time::{Clock, Time};
use skelter_core::{Document, JsEvent, KindId, KindRegistry, Value};

use crate::app::Viewer;
use crate::gutter::GutterZone;
use crate::nav::command_for_key;
use crate::rows::{RowsEvent, RowsHost};
use crate::source::{SourceEvent, SourceHost};
use crate::timeline::{TimelineHost, TimelineInput};
use crate::ui::{Action, Ui, w};

/// Every kind the window uses: skelter-widgets', berth's, hopper's.
pub fn registry() -> Arc<KindRegistry> {
    let mut r = KindRegistry::new();
    skelter_widgets::host::register_tier0(&mut r).expect("skelter-widgets' kinds register once");
    r.register::<skelter_widgets::text::kind::TextEdit>().expect("the text field registers once");
    skelter_widgets::views::register_all(&mut r).expect("the views register once");
    skelter_widgets::table::kinds::register(&mut r).expect("the table registers once");
    skelter_berth::register_kinds(&mut r).expect("berth's kinds register once");
    r.register::<crate::rows::RowsKind>().expect("hopper-rows registers once");
    r.register::<crate::source::SourceKind>().expect("hopper-source registers once");
    r.register::<crate::timeline::TimelineKind>().expect("hopper-timeline registers once");
    Arc::new(r)
}

/// The window's document over `fonts`, with the theme and the widgets' services.
pub fn new_document(fonts: &Arc<skelter_text::FontStore>) -> Document {
    let mut doc = Document::new(registry(), Box::new(skelter_text::TextSystem::new(fonts.clone())));
    doc.set_text_shaper(Box::new(skelter_text::TextSystem::new(fonts.clone())));
    let theme = skelter_widgets::theme::Theme::dark_modern();
    doc.add_sheet(&theme.css());
    doc.add_sheet(skelter_widgets::theme::BASE_CSS);
    doc.add_sheet(crate::ui::CSS);
    skelter_widgets::host::install(&mut doc, skelter_text::TextSystem::new(fonts.clone()), skelter_widgets::host::os_policy(), theme);
    doc
}

/// Tells the compositor a canvas changed.
pub type CanvasNotify = Box<dyn Fn(WindowId, CanvasId)>;

pub struct HopperDocument {
    core: CoreDocument<Document>,
    pub viewer: Viewer,
    pub ui: Ui,
    window: Option<(WindowId, Size)>,
    dirty: bool,
    canvas_notify: Option<CanvasNotify>,
}

impl std::fmt::Debug for HopperDocument {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HopperDocument").field("window", &self.window).field("viewer", &self.viewer).finish_non_exhaustive()
    }
}

impl HopperDocument {
    /// The document over `viewer` (not started yet: [`HopperDocument::start`]).
    pub fn new(viewer: Viewer, fonts: Arc<skelter_text::FontStore>) -> Self {
        let mut doc = new_document(&fonts);
        let ui = Ui::build(&mut doc, fonts);
        ui.attach_calls(&mut doc, &viewer);
        HopperDocument { core: CoreDocument::new(doc, KindId::of("box")), viewer, ui, window: None, dirty: true, canvas_notify: None }
    }

    /// Asks the engine its first questions.
    pub fn start(&mut self) {
        self.viewer.start();
        self.dirty = true;
    }

    pub fn doc(&self) -> &Document {
        &self.core.doc
    }

    pub fn doc_mut(&mut self) -> &mut Document {
        &mut self.core.doc
    }

    /// The timeline's canvas, for the compositor to register.
    pub fn timeline_canvas(&mut self) -> Option<Arc<skelter_canvas::Canvas>> {
        self.core.doc.service::<TimelineHost>().map(|h| h.canvas.clone())
    }

    pub fn set_canvas_notify(&mut self, f: CanvasNotify) {
        self.canvas_notify = Some(f);
    }

    pub fn set_waker(&self, f: Arc<dyn Fn() + Send + Sync>) {
        self.viewer.jobs.set_waker(f);
    }

    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.core.set_clock(clock);
    }

    /// No job is queued, running or undrained and nothing waits for a frame (tests).
    pub fn settled(&self) -> bool {
        self.viewer.is_idle() && !self.dirty && !self.viewer.changed.any()
    }

    /// Routes the widget events and the panes' outboxes to the viewer.
    fn route(&mut self, events: &[JsEvent]) {
        for ev in events {
            let Some(&action) = self.ui.actions.get(&ev.target) else { continue };
            let name = self.core.doc.registry().get(self.core.doc.kind_of(ev.target).unwrap_or(w::BOX)).and_then(|e| {
                e.spec.signals.iter().enumerate().find(|(i, _)| e.spec.signal_id(*i) == ev.signal).map(|(_, s)| s.name)
            });
            let text = match &ev.arg {
                Some(Value::Str(s)) => s.to_string(),
                _ => String::new(),
            };
            match (action, name) {
                (Action::Command(c), Some("clicked")) => self.viewer.command(c),
                (Action::UrlSubmit, Some("submit")) => self.viewer.open_url(&text),
                (Action::UrlSubmit, Some("cancel")) => self.viewer.close_url_box(),
                (Action::MemorySubmit, Some("submit")) => self.viewer.memory_address(&text),
                (Action::ConsoleSubmit, Some("submit")) => {
                    self.viewer.eval(&text);
                    let _ = self.core.doc.set_prop(self.ui.console, w::TEXT, Value::Str("".into()));
                }
                _ => {}
            }
        }
        // the call table's signals
        for ev in events.iter().filter(|e| e.target == self.ui.calls) {
            let name = self.core.doc.registry().get(w::TABLE).and_then(|e| {
                e.spec.signals.iter().enumerate().find(|(i, _)| e.spec.signal_id(*i) == ev.signal).map(|(_, s)| s.name)
            });
            match (name, &ev.arg) {
                (Some("activated"), Some(Value::I64(row))) => self.viewer.activate_call(*row as usize),
                (Some("activated"), Some(Value::Record(r))) => {
                    if let Some((_, Value::I64(row))) = r.first() {
                        self.viewer.activate_call(*row as usize);
                    }
                }
                (Some("changed"), arg) => {
                    let first = match arg {
                        Some(Value::List(ids)) => ids.first().cloned(),
                        Some(Value::Record(r)) => match r.first() {
                            Some((_, Value::List(ids))) => ids.first().cloned(),
                            _ => None,
                        },
                        _ => None,
                    };
                    if let Some(id) = first.and_then(|v| match v {
                        Value::I64(i) => Some(i as u64),
                        Value::F64(f) => Some(f as u64),
                        _ => None,
                    }) {
                        let tree = self.viewer.calls.borrow();
                        let row = (0..tree.len()).find(|&i| tree.row_key(i) == Some(id));
                        drop(tree);
                        if let Some(row) = row {
                            self.viewer.select_call(row);
                        }
                    }
                }
                _ => {}
            }
        }
        let rows = self.core.doc.service::<RowsHost>().map(|h| std::mem::take(&mut h.events)).unwrap_or_default();
        for e in rows {
            let (node, row) = match e {
                RowsEvent::Select { node, row } | RowsEvent::Toggle { node, row } | RowsEvent::Activate { node, row } => (node, row),
            };
            if node == self.ui.stack {
                self.viewer.select_frame(row);
            } else if node == self.ui.locals {
                match e {
                    RowsEvent::Select { .. } => self.viewer.select_local(row),
                    RowsEvent::Toggle { .. } | RowsEvent::Activate { .. } => self.viewer.toggle_local(row),
                }
            } else if node == self.ui.query {
                if let RowsEvent::Activate { .. } = e {
                    self.viewer.query_activate(row);
                }
                if let Some(h) = self.core.doc.service::<RowsHost>()
                    && let Some(l) = h.lists.get_mut(&node)
                {
                    l.selected = Some(row);
                }
                self.ui.bump(&mut self.core.doc, node, crate::rows::EPOCH);
            }
        }
        let source = self.core.doc.service::<SourceHost>().map(|h| std::mem::take(&mut h.events)).unwrap_or_default();
        for e in source {
            match e {
                SourceEvent::GutterClick { line, zone: GutterZone::HitCount, .. } => self.viewer.line_executions(line),
                SourceEvent::GutterClick { line, zone: GutterZone::Marker, .. } => self.viewer.toggle_breakpoint(line),
                SourceEvent::Copy { text } => self.viewer.copy = Some(text),
            }
        }
        let timeline = self.core.doc.service::<TimelineHost>().map(|h| std::mem::take(&mut h.events)).unwrap_or_default();
        for TimelineInput::SeekNear { fraction, .. } in timeline {
            self.viewer.timeline_seek(fraction);
        }
    }

    /// After the document's frame: what the panes reported in layout.
    fn after_frame(&mut self, window: WindowId) {
        let pane = self.core.doc.service::<SourceHost>().and_then(|h| h.panes.get(&self.ui.source)).map(|p| (p.visible.clone(), p.caret_line));
        if let Some((v, caret)) = pane {
            self.viewer.source.caret_line = caret;
            self.viewer.lines_shown(&v);
        }
        let changed = self.core.doc.service::<TimelineHost>().is_some_and(|h| std::mem::take(&mut h.changed));
        if changed && let Some(f) = &self.canvas_notify {
            f(window, crate::timeline::CANVAS);
        }
    }
}

impl DocumentApi for HopperDocument {
    fn dispatch(&mut self, ev: &InputEnvelope) -> Dispatched {
        if let InputEvent::Key(k) = &ev.event
            && k.state == KeyState::Pressed
            && let Some(cmd) = k.key.and_then(|key| command_for_key(key, k.modifiers))
        {
            self.viewer.command(cmd);
            self.ui.apply(&mut self.viewer, &mut self.core.doc);
            self.dirty = true;
            return Dispatched { dirty: true, fast: self.core.doc.take_fast_commits() };
        }
        let d = self.core.dispatch(ev);
        let events = self.core.take_js_events();
        self.route(&events);
        // apply now too: a copy needs the clipboard the runner lends only during dispatch
        self.ui.apply(&mut self.viewer, &mut self.core.doc);
        let dirty = d.dirty || self.viewer.changed.any() || self.core.doc.needs_frame(ev.window);
        self.dirty |= dirty;
        Dispatched { dirty: dirty || self.dirty, fast: d.fast }
    }

    fn frame(&mut self, window: WindowId, target: Time) -> Option<Commit> {
        self.viewer.drain();
        let events = self.core.take_js_events();
        self.route(&events);
        self.ui.apply(&mut self.viewer, &mut self.core.doc);
        let size = self.window.map(|(_, s)| s).unwrap_or(Size::new(1280.0, 800.0));
        let again = self.ui.dock_frame(&mut self.core.doc, window, size);
        let c = self.core.frame(window, target);
        self.after_frame(window);
        self.dirty = again || self.viewer.changed.any();
        c
    }

    fn needs_frame(&self, window: WindowId) -> bool {
        if self.window.map(|(w, _)| w) != Some(window) {
            return false;
        }
        self.dirty || self.viewer.changed.any() || self.viewer.jobs.has_results() || self.core.needs_frame(window)
    }

    fn resize(&mut self, window: WindowId, size: Size, scale: Scale) {
        match self.window {
            Some((w, _)) if w == window => self.core.doc.resize_window(window, size, scale),
            Some(_) => return,
            None => {
                if let Err(e) = self.core.doc.attach_window(window, self.ui.root, size, scale) {
                    eprintln!("hopper-gui: cannot attach the window: {e:?}");
                    return;
                }
            }
        }
        self.window = Some((window, size));
        self.dirty = true;
    }

    fn close_window(&mut self, window: WindowId) {
        if self.window.map(|(w, _)| w) == Some(window) {
            self.core.close_window(window);
            self.window = None;
        }
    }

    fn next_wakeup(&self, window: WindowId) -> Option<Time> {
        self.core.next_wakeup(window)
    }
    fn hover(&mut self, window: WindowId, pos: Point) -> bool {
        self.core.hover(window, pos)
    }
    fn hover_at(&mut self, window: WindowId, pos: Point, now: Time) -> bool {
        self.core.hover_at(window, pos, now)
    }
    fn ime_state(&mut self, window: WindowId) -> Option<(Rect, InputPurpose)> {
        self.core.ime_state(window)
    }
    fn take_fast_commits(&mut self) -> Vec<FastCommit> {
        self.core.take_fast_commits()
    }
    fn take_smooth_scrolls(&mut self) -> Vec<skelter_core::SmoothScrollRequest> {
        self.core.take_smooth_scrolls()
    }
    fn caret_blink(&self) -> Option<Option<skelter_compositor::CaretBlink>> {
        self.core.caret_blink()
    }
    fn reduced_motion(&self) -> Option<bool> {
        self.core.reduced_motion()
    }
    fn menu_activated(&mut self, id: &str) -> bool {
        self.core.menu_activated(id)
    }
    fn take_menu_bar(&mut self) -> Option<Option<skelter_platform::MenuDesc>> {
        self.core.take_menu_bar()
    }
    fn menu_bar_native(&mut self, native: bool) {
        self.core.menu_bar_native(native)
    }
    fn install_clipboard(&mut self, clipboard: Box<dyn skelter_platform::Clipboard>) {
        self.core.install_clipboard(clipboard)
    }
    fn scroll_moved(&mut self, window: WindowId, node: PropNodeId, offset: Vec2) {
        self.core.scroll_moved(window, node, offset)
    }
    fn animation_event(&mut self, window: WindowId, ev: AnimationEvent) {
        self.core.animation_event(window, ev)
    }
    fn accessibility_action(&mut self, window: WindowId, req: &ActionRequest) -> bool {
        let handled = self.core.accessibility_action(window, req);
        let events = self.core.take_js_events();
        self.route(&events);
        self.dirty = true;
        handled
    }
    fn accessibility_activated(&mut self, window: WindowId) {
        self.core.accessibility_activated(window)
    }
    fn window_event(&mut self, window: WindowId, ev: &WindowEvent) {
        self.core.window_event(window, ev)
    }
    fn device_reset(&mut self) {
        self.core.device_reset()
    }
    fn commit_applied(&mut self, window: WindowId, revision: Revision) {
        self.core.commit_applied(window, revision)
    }
}
