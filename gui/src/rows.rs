//! `hopper-rows`: a virtual list of rows with indentation, disclosure triangles, per-cell tones and
//! a selection. The locals (with greyed moved-out values), the stack and the query pane use it.
//!
//! The data lives in the document's [`RowsHost`] service by node, and the kind draws only the
//! rows in view. Input goes back to the host as [`RowsEvent`]s in the service's outbox: a press
//! selects, a press on the triangle (or Left/Right, or a double click on a parent) toggles, a
//! double click or Enter activates. The host bumps the node's `epoch` prop after changing a list.

use std::cell::Cell;
use std::collections::HashMap;

use skelter_base::change::ChangeClass;
use skelter_base::event::{KeyState, ScrollDelta};
use skelter_base::geom::{Point, Rect};
use skelter_base::ids::NodeId;
use skelter_base::key::Key;
use skelter_core::{
    Event, EventCx, FocusReason, Handled, Kind, KindId, KindSpec, MeasureCx, PrepareCx, PropDefault, PropId, PropSpec,
    RecordCx, ValueType,
};
use skelter_widgets::paint::{Align, raw};
use skelter_widgets::table::text_painter::{TextStyle, record_core};

use crate::style;

pub const KIND: KindId = KindId::of("hopper-rows");
/// Bumped by the host after it changed the node's list.
pub const EPOCH: PropId = PropId::of("hopper-rows", "epoch", "int");

pub const ROW_H: f32 = 20.0;
const INDENT: f32 = 14.0;
const PAD: f32 = 6.0;

/// How a cell's text is drawn.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Normal,
    /// Stale or secondary (a moved-out value, a type name).
    Muted,
    Accent,
    Warning,
    Error,
    Heading,
}

impl Tone {
    fn color(self) -> skelter_base::color::Color {
        match self {
            Tone::Normal => style::FG,
            Tone::Muted => style::MUTED,
            Tone::Accent => style::ACCENT,
            Tone::Warning => style::WARNING,
            Tone::Error => style::ERROR,
            Tone::Heading => style::HEADING,
        }
    }
}

/// A row's triangle.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Disclosure {
    #[default]
    None,
    Collapsed,
    Expanded,
}

/// One cell: its text, tone and font.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowCell {
    pub text: String,
    pub tone: Tone,
    pub mono: bool,
}

impl RowCell {
    pub fn new(text: impl Into<String>, tone: Tone) -> Self {
        RowCell { text: text.into(), tone, mono: false }
    }
    pub fn mono(text: impl Into<String>, tone: Tone) -> Self {
        RowCell { text: text.into(), tone, mono: true }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowLine {
    pub depth: u32,
    pub disclosure: Disclosure,
    pub cells: Vec<RowCell>,
}

/// A column's width: points, or what is left (shared by every `Fill` column).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Width {
    Fixed(f32),
    Fill,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct RowList {
    pub columns: Vec<Width>,
    pub rows: Vec<RowLine>,
    pub selected: Option<usize>,
    /// Scroll this row into view on the next frame (the host sets it; the kind clears it).
    pub reveal: Option<usize>,
    /// Text shown when there are no rows.
    pub empty: String,
}

/// What a press or key did to a list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowsEvent {
    Select { node: NodeId, row: usize },
    Toggle { node: NodeId, row: usize },
    Activate { node: NodeId, row: usize },
}

/// The lists by node, and the outbox the host drains.
#[derive(Debug, Default)]
pub struct RowsHost {
    pub lists: HashMap<NodeId, RowList>,
    pub events: Vec<RowsEvent>,
}

static PROPS: [PropSpec; 1] =
    [PropSpec::new("epoch", ValueType::I64, ChangeClass::Paint).default(PropDefault::I64(0)).doc("Bumped by the host after the list changed.")];

static SPEC: KindSpec = KindSpec {
    props: &PROPS,
    role: skelter_base::accesskit::Role::List,
    focusable: true,
    doc: "hopper's virtual rows (locals, stack, query results).",
    ..KindSpec::new("hopper-rows")
};

/// The kind.
#[derive(Debug, Default)]
pub struct RowsKind {
    scroll: Cell<f32>,
    height: f32,
    focused: bool,
}

/// The x of each column's left edge and its width, for a box `w` wide.
pub fn column_spans(columns: &[Width], w: f32) -> Vec<(f32, f32)> {
    let fixed: f32 = columns.iter().map(|c| if let Width::Fixed(x) = c { *x } else { 0.0 }).sum();
    let fills = columns.iter().filter(|c| matches!(c, Width::Fill)).count().max(1) as f32;
    let fill = ((w - fixed) / fills).max(40.0);
    let mut x = 0.0;
    columns
        .iter()
        .map(|c| {
            let width = match c {
                Width::Fixed(v) => *v,
                Width::Fill => fill,
            };
            let span = (x, width);
            x += width;
            span
        })
        .collect()
}

/// The first row and how many fit in a viewport `h` tall scrolled to `scroll`.
pub fn visible_rows(scroll: f32, h: f32, len: usize) -> std::ops::Range<usize> {
    let first = (scroll / ROW_H).floor().max(0.0) as usize;
    let last = (((scroll + h) / ROW_H).ceil().max(0.0) as usize).min(len);
    first.min(last)..last
}

/// The scroll that shows `row` in a viewport `h` tall, from `scroll`.
pub fn scroll_to_show(row: usize, scroll: f32, h: f32) -> f32 {
    let top = row as f32 * ROW_H;
    if top < scroll {
        top
    } else if top + ROW_H > scroll + h {
        (top + ROW_H - h).max(0.0)
    } else {
        scroll
    }
}

impl RowsKind {
    fn clamp(&self, len: usize) {
        let max = (len as f32 * ROW_H - self.height).max(0.0);
        self.scroll.set(self.scroll.get().clamp(0.0, max));
    }
}

impl Kind for RowsKind {
    fn spec() -> &'static KindSpec {
        &SPEC
    }

    fn measure(
        &self,
        _cx: &mut MeasureCx,
        known: skelter_core::taffy::Size<Option<f32>>,
        _avail: skelter_core::taffy::Size<skelter_core::taffy::AvailableSpace>,
    ) -> skelter_core::taffy::Size<f32> {
        skelter_core::taffy::Size { width: known.width.unwrap_or(0.0), height: known.height.unwrap_or(0.0) }
    }

    fn prepare(&mut self, cx: &mut PrepareCx) {
        self.height = cx.size().h;
        let node = cx.node.id();
        let Some(host) = cx.service::<RowsHost>() else { return };
        let Some(list) = host.lists.get_mut(&node) else { return };
        if let Some(r) = list.reveal.take() {
            self.scroll.set(scroll_to_show(r, self.scroll.get(), self.height));
        }
        let len = list.rows.len();
        self.clamp(len);
    }

    fn record(&self, cx: &mut RecordCx) {
        let size = cx.node.size();
        let node = cx.node.id();
        let scroll = self.scroll.get();
        let (rows, columns, selected, empty) = match cx.service::<RowsHost>().and_then(|h| h.lists.get(&node)) {
            Some(l) => {
                let range = visible_rows(scroll, size.h, l.rows.len());
                (range.clone().map(|i| (i, l.rows[i].clone())).collect::<Vec<_>>(), l.columns.clone(), l.selected, l.empty.clone())
            }
            None => (Vec::new(), Vec::new(), None, String::new()),
        };
        let focused = self.focused;
        record_core(cx, |p, list| {
            raw::fill(list, Rect::new(0.0, 0.0, size.w, size.h), style::BG, 0.0);
            if rows.is_empty() && !empty.is_empty() {
                let st = TextStyle { family: style::UI_FONT.into(), size: 12.0, weight: 400, italic: true, color: style::MUTED };
                p.draw(list, &empty, &st, Rect::new(PAD, 4.0, size.w - PAD, 4.0 + ROW_H), Align::Start, true);
                return;
            }
            let spans = column_spans(&columns, size.w);
            for (i, row) in &rows {
                let y = *i as f32 * ROW_H - scroll;
                let band = Rect::new(0.0, y, size.w, y + ROW_H);
                if selected == Some(*i) {
                    raw::fill(list, band, if focused { style::SELECTION_FOCUSED } else { style::SELECTION }, 0.0);
                } else if i % 2 == 1 {
                    raw::fill(list, band, style::ROW_STRIPE, 0.0);
                }
                let indent = PAD + row.depth as f32 * INDENT;
                let mid = Point::new(indent + 5.0, y + ROW_H / 2.0);
                match row.disclosure {
                    Disclosure::Collapsed => {
                        raw::line(list, Point::new(mid.x - 2.0, mid.y - 4.0), Point::new(mid.x + 2.0, mid.y), 1.5, style::DISCLOSURE);
                        raw::line(list, Point::new(mid.x + 2.0, mid.y), Point::new(mid.x - 2.0, mid.y + 4.0), 1.5, style::DISCLOSURE);
                    }
                    Disclosure::Expanded => {
                        raw::line(list, Point::new(mid.x - 4.0, mid.y - 2.0), Point::new(mid.x, mid.y + 2.0), 1.5, style::DISCLOSURE);
                        raw::line(list, Point::new(mid.x, mid.y + 2.0), Point::new(mid.x + 4.0, mid.y - 2.0), 1.5, style::DISCLOSURE);
                    }
                    Disclosure::None => {}
                }
                // a row of one cell spans every column
                let whole = [(0.0, size.w)];
                let spans: &[(f32, f32)] = if row.cells.len() == 1 { &whole } else { &spans };
                for (c, cell) in row.cells.iter().enumerate() {
                    let Some(&(x, w)) = spans.get(c) else { break };
                    // the first column is indented under the triangle
                    let left = if c == 0 { x + indent + 14.0 } else { x + PAD };
                    let right = x + w - PAD;
                    if right <= left {
                        continue;
                    }
                    let family = if cell.mono { style::MONO_FONT } else { style::UI_FONT };
                    let weight = if cell.tone == Tone::Heading { 600 } else { 400 };
                    let st = TextStyle { family: family.into(), size: 12.0, weight, italic: false, color: cell.tone.color() };
                    p.draw(list, &cell.text, &st, Rect::new(left, y, right, y + ROW_H), Align::Start, true);
                }
            }
        });
    }

    fn event(&mut self, cx: &mut EventCx, ev: &Event) -> Handled {
        let node = cx.node();
        let len = cx.service::<RowsHost>().and_then(|h| h.lists.get(&node)).map_or(0, |l| l.rows.len());
        let selected = cx.service::<RowsHost>().and_then(|h| h.lists.get(&node)).and_then(|l| l.selected);
        let row_at = |y: f32, scroll: f32| {
            let r = ((y + scroll) / ROW_H).floor();
            (r >= 0.0 && (r as usize) < len).then_some(r as usize)
        };
        let mut out = None;
        let handled = match ev {
            Event::PointerDown(p) => {
                cx.focus(FocusReason::Pointer);
                if let Some(row) = row_at(p.local.y, self.scroll.get()) {
                    let (depth, has) = cx
                        .service::<RowsHost>()
                        .and_then(|h| h.lists.get(&node))
                        .map(|l| (l.rows[row].depth, l.rows[row].disclosure != Disclosure::None))
                        .unwrap_or((0, false));
                    let tri = PAD + depth as f32 * INDENT;
                    out = Some(if has && p.local.x >= tri - 2.0 && p.local.x < tri + 14.0 {
                        RowsEvent::Toggle { node, row }
                    } else if p.count >= 2 {
                        RowsEvent::Activate { node, row }
                    } else {
                        RowsEvent::Select { node, row }
                    });
                }
                true
            }
            Event::Wheel(w) => {
                let dy = match w.delta {
                    ScrollDelta::Lines(v) => v.y * ROW_H * 3.0,
                    ScrollDelta::Pixels(v) => v.y,
                    ScrollDelta::Pages(v) => v.y * self.height,
                };
                self.scroll.set(self.scroll.get() - dy);
                self.clamp(len);
                true
            }
            Event::Key(k) if k.state == KeyState::Pressed => {
                let cur = selected.unwrap_or(0);
                let page = ((self.height / ROW_H) as usize).max(1);
                let to = match k.key {
                    Some(Key::ArrowDown) => Some((cur + usize::from(selected.is_some())).min(len.saturating_sub(1))),
                    Some(Key::ArrowUp) => Some(cur.saturating_sub(1)),
                    Some(Key::PageDown) => Some((cur + page).min(len.saturating_sub(1))),
                    Some(Key::PageUp) => Some(cur.saturating_sub(page)),
                    Some(Key::Home) => Some(0),
                    Some(Key::End) => Some(len.saturating_sub(1)),
                    _ => None,
                };
                if let Some(row) = to.filter(|_| len > 0) {
                    out = Some(RowsEvent::Select { node, row });
                    self.scroll.set(scroll_to_show(row, self.scroll.get(), self.height));
                    true
                } else if let (Some(row), Some(Key::ArrowLeft | Key::ArrowRight)) = (selected, k.key) {
                    out = Some(RowsEvent::Toggle { node, row });
                    true
                } else if let (Some(row), Some(Key::Enter)) = (selected, k.key) {
                    out = Some(RowsEvent::Activate { node, row });
                    true
                } else {
                    false
                }
            }
            Event::FocusIn(_) => {
                self.focused = true;
                false
            }
            Event::FocusOut => {
                self.focused = false;
                false
            }
            _ => false,
        };
        if let Some(e) = out
            && let Some(h) = cx.service::<RowsHost>()
        {
            h.events.push(e);
        }
        cx.request_paint();
        if handled { Handled::Yes } else { Handled::No }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_share_what_fixed_ones_leave() {
        assert_eq!(column_spans(&[Width::Fill, Width::Fixed(100.0), Width::Fill], 500.0), vec![(0.0, 200.0), (200.0, 100.0), (300.0, 200.0)]);
    }

    #[test]
    fn rows_in_view_and_the_scroll_that_reveals_one() {
        assert_eq!(visible_rows(0.0, 100.0, 3), 0..3);
        assert_eq!(visible_rows(30.0, 100.0, 50), 1..7);
        assert_eq!(scroll_to_show(10, 0.0, 100.0), 120.0);
        assert_eq!(scroll_to_show(2, 100.0, 100.0), 40.0);
        assert_eq!(scroll_to_show(6, 100.0, 100.0), 100.0);
    }
}
