//! `hopper-source`: the source pane. A read-only quire editor (skelter_quire's `SkelterEditor`
//! over a quire `Document`) with hopper's own gutter column to its left, drawn by this kind:
//! hit counts, breakpoints and the execution arrow, in step with the editor's scroll because both are recorded
//! from the same scroll offset in the same pass. The stopped line gets a whole-line band (quire
//! draws its current-line band only while focused, and has no host line decorations).
//!
//! skelter_quire's own `code-editor` kind keeps its editor private, so a host cannot reveal a line
//! or read the scroll; this kind owns the editor instead (as cadatomic's `code-editor` does), and
//! the gutter and decoration APIs quire lacks are requests (`docs/requests.md`).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;

use quire_core::{Document, EditorView};
use skelter_base::change::ChangeClass;
use skelter_base::display::DisplayList;
use skelter_base::geom::{Point, Rect, RoundedRect, Size, Vec2};
use skelter_base::ids::{NodeId, RunId};
use skelter_base::text::GlyphRun;
use skelter_base::time::Time;
use skelter_core::{
    Event, EventCx, FocusReason, Handled, Kind, KindId, KindSpec, MeasureCx, PrepareCx,
    PropDefault, PropId, PropSpec, RecordCx, ValueType,
};
use skelter_quire::editor::{EditorOptions, SkelterEditor};
use skelter_quire::layers::translate_prim;
use skelter_quire::scene::RunSink;
use skelter_quire::theme::EditorTheme;
use skelter_widgets::paint::{Align, raw};
use skelter_widgets::table::text_painter::{TextStyle, record_core};

use crate::gutter::{Arrow, ArrowAt, GutterZone, HitCounts, marks, zone_at};
use crate::style;

pub const KIND: KindId = KindId::of("hopper-source");
pub const EPOCH: PropId = PropId::of("hopper-source", "epoch", "int");

/// Width of hopper's column (hit counts and the arrow), left of quire's gutter.
pub const COLUMN_W: f32 = 58.0;

/// What the host shows in one source pane, and what the pane reports back.
#[derive(Debug, Default)]
pub struct SourcePane {
    /// The file's text; `None` shows `message`.
    pub doc: Option<Rc<RefCell<Document>>>,
    /// Bumped whenever `doc` is replaced.
    pub generation: u64,
    pub message: String,
    pub arrows: Vec<ArrowAt>,
    pub hits: HitCounts,
    /// The 1-based lines with a user breakpoint.
    pub breakpoints: Vec<u32>,
    /// Centre this 1-based line on the next frame.
    pub reveal: Option<u32>,
    /// Out: the 1-based lines the editor showed in its last layout.
    pub visible: Vec<u32>,
    /// Out: the 1-based line of the editor's caret, while the editor has focus and the user has
    /// placed the caret.
    pub caret_line: Option<u32>,
}

/// What the user did in a source pane.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SourceEvent {
    /// A press in hopper's column on a 1-based line, in one of its zones.
    GutterClick {
        node: NodeId,
        line: u32,
        zone: GutterZone,
    },
    /// The editor asked to copy its selection.
    Copy { text: String },
}

/// The document's service: fonts and theme for the editors, the panes by node, the outbox.
pub struct SourceHost {
    pub fonts: Arc<skelter_text::FontStore>,
    pub theme: EditorTheme,
    pub panes: HashMap<NodeId, SourcePane>,
    pub events: Vec<SourceEvent>,
}

impl std::fmt::Debug for SourceHost {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceHost")
            .field("panes", &self.panes.len())
            .finish_non_exhaustive()
    }
}

impl SourceHost {
    pub fn new(fonts: Arc<skelter_text::FontStore>) -> Self {
        let mut theme = EditorTheme::new(true);
        theme.font_family = style::MONO_FONT.into();
        theme.font_size = 13.0;
        SourceHost {
            fonts,
            theme,
            panes: HashMap::new(),
            events: Vec::new(),
        }
    }
}

static PROPS: [PropSpec; 1] = [PropSpec::new("epoch", ValueType::I64, ChangeClass::Paint)
    .default(PropDefault::I64(0))
    .doc("Bumped by the host after the pane changed.")];

static SPEC: KindSpec = KindSpec {
    props: &PROPS,
    role: skelter_base::accesskit::Role::Document,
    focusable: true,
    doc: "hopper's source view: quire read-only, with hit counts and the execution arrow.",
    ..KindSpec::new("hopper-source")
};

struct Inner {
    doc: Rc<RefCell<Document>>,
    generation: u64,
    view: EditorView,
    editor: SkelterEditor,
    /// The view's selection version when it was made: the caret counts once the user moved it.
    initial_selection: u64,
}

/// The kind.
#[derive(Default)]
pub struct SourceKind {
    inner: RefCell<Option<Inner>>,
    now: Time,
}

impl std::fmt::Debug for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SourceKind").finish_non_exhaustive()
    }
}

struct CxSink<'c, 'a>(&'c mut RecordCx<'a>);

impl RunSink for CxSink<'_, '_> {
    fn add_run(&mut self, run: Arc<GlyphRun>) -> RunId {
        self.0.add_run(run)
    }
}

impl SourceKind {
    /// Rebuilds the editor when the pane's document was replaced.
    fn sync(&mut self, host: &SourceHost, pane: &SourcePane) {
        let current = self.inner.get_mut().as_ref().map(|i| i.generation);
        match &pane.doc {
            None => *self.inner.get_mut() = None,
            Some(doc) if current != Some(pane.generation) => {
                let options = EditorOptions {
                    read_only: true,
                    inlay_hints: true,
                    ..EditorOptions::default()
                };
                let editor = SkelterEditor::new(host.fonts.clone(), host.theme.clone(), options);
                let view = EditorView::new(&doc.borrow());
                let initial_selection = view.selection_version();
                *self.inner.get_mut() = Some(Inner {
                    doc: doc.clone(),
                    generation: pane.generation,
                    view,
                    editor,
                    initial_selection,
                });
            }
            Some(_) => {}
        }
    }
}

/// The editor's box inside the pane.
fn editor_size(size: Size) -> Size {
    Size::new((size.w - COLUMN_W).max(1.0), size.h.max(1.0))
}

impl Kind for SourceKind {
    fn spec() -> &'static KindSpec {
        &SPEC
    }

    fn measure(
        &self,
        _cx: &mut MeasureCx,
        known: skelter_core::taffy::Size<Option<f32>>,
        _avail: skelter_core::taffy::Size<skelter_core::taffy::AvailableSpace>,
    ) -> skelter_core::taffy::Size<f32> {
        skelter_core::taffy::Size {
            width: known.width.unwrap_or(0.0),
            height: known.height.unwrap_or(0.0),
        }
    }

    fn prepare(&mut self, cx: &mut PrepareCx) {
        let node = cx.node.id();
        let size = editor_size(cx.size());
        let scale = cx.node.scale();
        let Some(host) = cx.service::<SourceHost>() else {
            return;
        };
        let Some(pane) = host.panes.remove(&node) else {
            return;
        };
        self.sync(host, &pane);
        let mut pane = pane;
        if let Some(i) = self.inner.get_mut() {
            let doc = i.doc.borrow();
            i.editor.layout(&doc, &mut i.view, size, scale);
            if let Some(line) = pane.reveal.take() {
                let t = doc.text();
                let l = (line.max(1) as usize - 1).min(t.len_lines().saturating_sub(1));
                let byte = t.line_to_byte(l);
                i.editor.reveal(&doc, &mut i.view, byte, true);
                i.editor.layout(&doc, &mut i.view, size, scale);
            }
            pane.visible = i.editor.visible_lines().map(|l| l as u32 + 1).collect();
            let placed = i.view.selection_version() != i.initial_selection;
            pane.caret_line = (i.editor.is_focused() && placed).then(|| {
                let sel = i.view.selection();
                let head = sel.ranges()[sel.primary_index()].head;
                doc.text().byte_to_line(head) as u32 + 1
            });
        } else {
            pane.visible.clear();
            pane.caret_line = None;
        }
        host.panes.insert(node, pane);
    }

    fn record(&self, cx: &mut RecordCx) {
        let size = cx.node.size();
        let node = cx.node.id();
        let (arrows, hits, breakpoints, message) =
            match cx.service::<SourceHost>().and_then(|h| h.panes.get(&node)) {
                Some(p) => (
                    p.arrows.clone(),
                    p.hits.clone(),
                    p.breakpoints.clone(),
                    p.message.clone(),
                ),
                None => (Vec::new(), HitCounts::default(), Vec::new(), String::new()),
            };
        let mut slot = self.inner.borrow_mut();
        let Some(i) = slot.as_mut() else {
            record_core(cx, |p, list| {
                raw::fill(list, Rect::new(0.0, 0.0, size.w, size.h), style::BG, 0.0);
                let st = TextStyle {
                    family: style::UI_FONT.into(),
                    size: 12.0,
                    weight: 400,
                    italic: true,
                    color: style::MUTED,
                };
                p.draw(
                    list,
                    &message,
                    &st,
                    Rect::new(12.0, 8.0, size.w - 12.0, 28.0),
                    Align::Start,
                    true,
                );
            });
            return;
        };
        let doc = i.doc.borrow();
        let sc = i.editor.record(&doc, &mut i.view, &mut CxSink(cx), None);
        // the editor's local space starts right of hopper's column
        let shift = Vec2::new(COLUMN_W, 0.0);
        let put = |cx: &mut RecordCx, list: &DisplayList, d: Vec2| {
            for p in &list.items {
                let (q, ink) = translate_prim(p, d + shift);
                cx.push(q, ink);
            }
        };
        // where each visible line's top is, in the pane's space
        let t = doc.text();
        let row_h = i.editor.geom().row_h;
        let rows: Vec<(u32, f32)> = i
            .editor
            .visible_lines()
            .map(|l| {
                let byte = t.line_to_byte(l);
                let (r, _) = i.editor.caret_content_rect(&doc, &mut i.view, byte, false);
                (l as u32 + 1, sc.text_origin.y - sc.scroll.y + r.min.y)
            })
            .collect();
        put(cx, &sc.background, Vec2::ZERO);
        // whole-line bands for the arrows' lines, under the text
        for a in &arrows {
            if let Some(&(_, y)) = rows.iter().find(|(l, _)| *l == a.line) {
                let color = match a.arrow {
                    Arrow::Execution => style::EXEC_LINE,
                    Arrow::Frame => style::FRAME_LINE,
                };
                let mut band = DisplayList::new();
                raw::fill(
                    &mut band,
                    Rect::new(COLUMN_W, y, size.w, y + row_h),
                    color,
                    0.0,
                );
                for p in band.items {
                    let ink = Rect::new(COLUMN_W, y, size.w, y + row_h);
                    cx.push(p, ink);
                }
            }
        }
        let content_d = Vec2::new(
            sc.text_origin.x - sc.scroll.x,
            sc.text_origin.y - sc.scroll.y,
        );
        cx.push_clip(RoundedRect {
            rect: sc.text_clip.translate(shift),
            radii: Default::default(),
        });
        put(cx, &sc.content, content_d);
        if i.editor.is_focused()
            && i.editor.caret_visible(self.now)
            && let Some((p, _)) = sc.caret_prim()
        {
            let (q, ink) = translate_prim(&p, content_d + shift);
            cx.push(q, ink);
        }
        cx.pop_clip();
        if sc.gutter_width > 0.0 {
            cx.push_clip(RoundedRect {
                rect: Rect::new(
                    COLUMN_W,
                    0.0,
                    COLUMN_W + sc.gutter_width,
                    sc.text_clip.max.y,
                ),
                radii: Default::default(),
            });
            put(
                cx,
                &sc.gutter,
                Vec2::new(0.0, sc.text_origin.y - sc.scroll.y),
            );
            cx.pop_clip();
        }
        put(cx, &sc.overlay, Vec2::ZERO);
        drop(doc);
        drop(slot);
        // hopper's column
        let m = marks(&rows, &hits, &arrows, &breakpoints);
        record_core(cx, |p, list| {
            raw::fill(
                list,
                Rect::new(0.0, 0.0, COLUMN_W, size.h),
                style::GUTTER_BG,
                0.0,
            );
            raw::fill(
                list,
                Rect::new(COLUMN_W - 1.0, 0.0, COLUMN_W, size.h),
                style::BORDER,
                0.0,
            );
            for mark in &m {
                let y = mark.y;
                if let Some(label) = &mark.label {
                    let st = TextStyle {
                        family: style::MONO_FONT.into(),
                        size: 11.0,
                        weight: 400,
                        italic: false,
                        color: style::HIT_COUNT,
                    };
                    p.draw(
                        list,
                        label,
                        &st,
                        Rect::new(2.0, y, COLUMN_W - 26.0, y + row_h),
                        Align::End,
                        true,
                    );
                }
                if mark.breakpoint {
                    let c = Point::new(COLUMN_W - 13.0, y + row_h / 2.0);
                    raw::fill(
                        list,
                        Rect::new(c.x - 5.5, c.y - 5.5, c.x + 5.5, c.y + 5.5),
                        style::BREAKPOINT,
                        5.5,
                    );
                }
                if let Some(a) = mark.arrow {
                    let color = match a {
                        Arrow::Execution => style::EXEC_ARROW,
                        Arrow::Frame => style::FRAME_ARROW,
                    };
                    draw_arrow(list, Point::new(COLUMN_W - 15.0, y + row_h / 2.0), color);
                }
            }
        });
    }

    fn event(&mut self, cx: &mut EventCx, ev: &Event) -> Handled {
        let node = cx.node();
        let now = cx.time();
        self.now = now;
        // presses in hopper's column are the host's
        if let Event::PointerDown(p) = ev
            && p.local.x < COLUMN_W
        {
            let line = self.inner.get_mut().as_mut().map(|i| {
                let doc = i.doc.borrow();
                let at = Point::new(i.editor.geom().gutter_w + 1.0, p.local.y);
                let (pos, _) = i.editor.pos_at(&doc, &mut i.view, at);
                doc.text().byte_to_line(pos) as u32 + 1
            });
            if let (Some(line), Some(h)) = (line, cx.service::<SourceHost>()) {
                h.events.push(SourceEvent::GutterClick {
                    node,
                    line,
                    zone: zone_at(p.local.x, COLUMN_W),
                });
            }
            cx.request_paint();
            return Handled::Yes;
        }
        let shifted = |p: &skelter_core::PointerInput| {
            let mut q = p.clone();
            q.local.x -= COLUMN_W;
            q
        };
        let handled = self
            .inner
            .get_mut()
            .as_mut()
            .map(|i| {
                i.editor.set_now(now);
                let mut doc = i.doc.borrow_mut();
                let (e, view) = (&mut i.editor, &mut i.view);
                match ev {
                    Event::PointerDown(p) => {
                        e.focus(&doc, view, true);
                        e.pointer_down(&mut doc, view, &shifted(p))
                    }
                    Event::PointerMove(p) => e.pointer_move(&mut doc, view, &shifted(p)),
                    Event::PointerUp(p) => e.pointer_up(&mut doc, view, &shifted(p)),
                    Event::Wheel(w) => e.wheel(&doc, view, w.delta, w.modifiers),
                    // read-only: the keymap still moves the caret, selects and copies
                    Event::Key(k) => e.key(&mut doc, view, k),
                    Event::FocusIn(_) => {
                        e.focus(&doc, view, true);
                        false
                    }
                    Event::FocusOut => {
                        e.focus(&doc, view, false);
                        false
                    }
                    Event::Timer => {
                        e.tick(&mut doc, view, now);
                        true
                    }
                    _ => false,
                }
            })
            .unwrap_or(false);
        if matches!(ev, Event::PointerDown(_)) {
            cx.focus(FocusReason::Pointer);
            cx.capture_pointer();
        }
        if matches!(ev, Event::PointerUp(_) | Event::PointerCancel) {
            cx.release_pointer();
        }
        let next = self.inner.get_mut().as_mut().and_then(|i| {
            let mut doc = i.doc.borrow_mut();
            i.editor.tick(&mut doc, &mut i.view, now)
        });
        match next {
            Some(t) => cx.wake_at(t),
            None => cx.cancel_wake(),
        }
        // the editor's copy requests go to the host's clipboard; the rest of its output is unused
        let copied = self.inner.get_mut().as_mut().and_then(|i| {
            i.editor
                .take_output()
                .events
                .into_iter()
                .rev()
                .find_map(|e| match e {
                    skelter_quire::EditorEvent::CopyText { text } => Some(text),
                    _ => None,
                })
        });
        if let (Some(text), Some(h)) = (copied, cx.service::<SourceHost>()) {
            h.events.push(SourceEvent::Copy { text });
        }
        cx.request_paint();
        if handled { Handled::Yes } else { Handled::No }
    }
}

/// A right-pointing arrow centred at `c`.
fn draw_arrow(list: &mut DisplayList, c: Point, color: skelter_base::color::Color) {
    raw::line(
        list,
        Point::new(c.x - 6.0, c.y),
        Point::new(c.x + 5.0, c.y),
        2.5,
        color,
    );
    raw::line(
        list,
        Point::new(c.x + 1.0, c.y - 4.5),
        Point::new(c.x + 6.0, c.y),
        2.5,
        color,
    );
    raw::line(
        list,
        Point::new(c.x + 1.0, c.y + 4.5),
        Point::new(c.x + 6.0, c.y),
        2.5,
        color,
    );
}
