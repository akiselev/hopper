//! `hopper-timeline`: the recording as a strip on a skelter-canvas canvas, with event marks and the
//! cursor; a press or a drag seeks.
//!
//! The canvas (`skelter_canvas::Canvas`) holds the marks as typed instances in world space; the
//! kind records one `Prim::Canvas` over its box (the compositor draws it with skelter-canvas's
//! renderer) and draws the axis labels itself on top. World space is the kind's own points with
//! the origin at the box's centre, so the canvas's default camera shows it one to one. The kind
//! redraws the canvas when the model's version or its size changes and flags the change in the
//! [`TimelineHost`]; the app tells the compositor (`canvas_changed`) after the frame.

use std::sync::Arc;

use skelter_base::change::ChangeClass;
use skelter_base::display::{CanvasPrim, LineCap, Prim};
use skelter_base::geom::{Rect, Size};
use skelter_base::ids::{CanvasId, NodeId};
use skelter_canvas::{Canvas, CanvasFonts, ChunkBuilder, DVec2, Plane, WorldRect};
use skelter_core::{
    Event, EventCx, Handled, Kind, KindId, KindSpec, MeasureCx, PrepareCx, PropDefault, PropId,
    PropSpec, RecordCx, ValueType,
};
use skelter_widgets::paint::{Align, raw};
use skelter_widgets::table::text_painter::{TextStyle, record_core};

use crate::style;
use crate::timeline_model::{MarkKind, TimelineModel};

pub const KIND: KindId = KindId::of("hopper-timeline");
pub const EPOCH: PropId = PropId::of("hopper-timeline", "epoch", "int");
/// The one timeline canvas's id.
pub const CANVAS: CanvasId = CanvasId(0x4F_7001);

/// Left and right margin of the strip inside the box.
const MARGIN: f32 = 12.0;

/// What a press or drag on the strip asked for.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum TimelineInput {
    /// Seek near this fraction of the recording (a press, or a drag's latest position).
    SeekNear { node: NodeId, fraction: f64 },
}

/// The document's service: the model, the canvas and the outbox.
#[derive(Debug)]
pub struct TimelineHost {
    pub model: TimelineModel,
    pub canvas: Arc<Canvas>,
    pub events: Vec<TimelineInput>,
    /// The canvas was redrawn since the app last told the compositor.
    pub changed: bool,
    drawn: Option<(u64, Size)>,
}

impl TimelineHost {
    pub fn new() -> Self {
        TimelineHost {
            model: TimelineModel::default(),
            canvas: Canvas::new(CANVAS, CanvasFonts::new()),
            events: Vec::new(),
            changed: false,
            drawn: None,
        }
    }
}

impl Default for TimelineHost {
    fn default() -> Self {
        Self::new()
    }
}

fn mark_color(k: MarkKind) -> skelter_base::color::Color {
    match k {
        MarkKind::Syscall => style::rgb(0x6A9955),
        MarkKind::Signal => style::ERROR,
        MarkKind::Process => style::rgb(0xC586C0),
        MarkKind::Thread => style::rgb(0x569CD6),
        MarkKind::Anchor => style::WARNING,
    }
}

/// The strip's x for `fraction` in a box `w` wide (kind-local points).
pub fn x_of(fraction: f64, w: f32) -> f32 {
    MARGIN + fraction as f32 * (w - 2.0 * MARGIN).max(1.0)
}

/// The fraction under kind-local `x`.
pub fn fraction_at(x: f32, w: f32) -> f64 {
    (((x - MARGIN) / (w - 2.0 * MARGIN).max(1.0)) as f64).clamp(0.0, 1.0)
}

/// Draws the model into the canvas for a box of `size`.
pub fn draw(canvas: &Canvas, model: &TimelineModel, size: Size) {
    let (w, h) = (size.w, size.h);
    // kind-local point → world (origin at the box centre)
    let world = |x: f32, y: f32| DVec2::new(f64::from(x - w / 2.0), f64::from(y - h / 2.0));
    let track_y = h * 0.55;
    canvas.edit(|e| {
        e.clear(Plane::Static);
        e.clear(Plane::Overlay);
        let mut b = ChunkBuilder::new(DVec2::new(0.0, 0.0));
        let a = world(MARGIN, track_y - 3.0);
        let z = world(w - MARGIN, track_y + 3.0);
        b.rect(
            WorldRect::new(a.x, a.y, z.x, z.y),
            3.0,
            0.0,
            style::rgb(0x3C3C3C),
        );
        for (f, k) in model.marks() {
            let x = x_of(f, w);
            b.segment(
                world(x, track_y - 10.0),
                world(x, track_y + 10.0),
                2.0,
                LineCap::Butt,
                mark_color(k),
            );
        }
        if !b.is_empty() {
            e.insert(Plane::Static, b.build());
        }
        if let Some(f) = model.cursor.as_ref().and_then(|c| model.fraction(c)) {
            let x = x_of(f, w);
            let mut o = ChunkBuilder::new(DVec2::new(0.0, 0.0));
            o.segment(
                world(x, 4.0),
                world(x, h - 4.0),
                2.0,
                LineCap::Butt,
                style::EXEC_ARROW,
            );
            o.circle(world(x, track_y), 5.0, 0.0, style::EXEC_ARROW);
            e.insert(Plane::Overlay, o.build());
        }
    });
}

static PROPS: [PropSpec; 1] = [PropSpec::new("epoch", ValueType::I64, ChangeClass::Paint)
    .default(PropDefault::I64(0))
    .doc("Bumped by the host after the model changed.")];

static SPEC: KindSpec = KindSpec {
    props: &PROPS,
    role: skelter_base::accesskit::Role::Slider,
    focusable: true,
    doc: "hopper's timeline: event marks and the cursor; press or drag to seek.",
    ..KindSpec::new("hopper-timeline")
};

#[derive(Debug, Default)]
pub struct TimelineKind {
    pressed: bool,
}

impl Kind for TimelineKind {
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
        let size = cx.size();
        let Some(host) = cx.service::<TimelineHost>() else {
            return;
        };
        let key = (host.model.version, size);
        if host.drawn != Some(key) && size.w > 0.0 && size.h > 0.0 {
            draw(&host.canvas, &host.model, size);
            host.drawn = Some(key);
            host.changed = true;
        }
    }

    fn record(&self, cx: &mut RecordCx) {
        let size = cx.node.size();
        let r = Rect::new(0.0, 0.0, size.w, size.h);
        let (labels, has) = match cx.service::<TimelineHost>() {
            Some(h) => {
                let m = &h.model;
                let first = m
                    .start
                    .map(|s| format!("event {}", s.frame_time))
                    .unwrap_or_default();
                let last = m
                    .end
                    .map(|s| format!("event {}", s.frame_time))
                    .unwrap_or_default();
                let cur = m
                    .cursor
                    .map(|c| format!("cursor {}", c.frame_time))
                    .unwrap_or_default();
                let n = format!("{} events", m.events.len());
                (
                    [first, cur, last, n],
                    m.start.is_some() || !m.events.is_empty(),
                )
            }
            None => (Default::default(), false),
        };
        record_core(cx, |_, list| raw::fill(list, r, style::BG, 0.0));
        cx.push(
            Prim::Canvas(CanvasPrim {
                canvas: CANVAS,
                rect: r,
            }),
            r,
        );
        record_core(cx, |p, list| {
            let st = TextStyle {
                font: skelter_text::TextStyle {
                    family: style::UI_FONT.into(),
                    size: 11.0,
                    weight: 400,
                    italic: false,
                    ..skelter_text::TextStyle::default()
                },
                color: style::MUTED,
            };
            if !has {
                p.draw(
                    list,
                    "No timeline yet",
                    &st,
                    Rect::new(MARGIN, 2.0, size.w - MARGIN, 18.0),
                    Align::Start,
                    true,
                );
                return;
            }
            let top = Rect::new(MARGIN, 2.0, size.w - MARGIN, 16.0);
            p.draw(list, &labels[0], &st, top, Align::Start, true);
            p.draw(
                list,
                &labels[1],
                &st.with_color(style::EXEC_ARROW),
                top,
                Align::Center,
                true,
            );
            p.draw(list, &labels[2], &st, top, Align::End, true);
            let bottom = Rect::new(MARGIN, size.h - 16.0, size.w - MARGIN, size.h - 2.0);
            p.draw(list, &labels[3], &st, bottom, Align::Start, true);
        });
    }

    fn event(&mut self, cx: &mut EventCx, ev: &Event) -> Handled {
        let node = cx.node();
        let w = cx.document().world_rect(node).map_or(1.0, |r| r.width());
        let seek = |x: f32| TimelineInput::SeekNear {
            node,
            fraction: fraction_at(x, w),
        };
        let out = match ev {
            Event::PointerDown(p) => {
                self.pressed = true;
                cx.capture_pointer();
                Some(seek(p.local.x))
            }
            Event::PointerMove(p) if self.pressed => Some(seek(p.local.x)),
            Event::PointerUp(_) | Event::PointerCancel => {
                self.pressed = false;
                cx.release_pointer();
                None
            }
            _ => return Handled::No,
        };
        if let (Some(e), Some(h)) = (out, cx.service::<TimelineHost>()) {
            h.events.push(e);
        }
        Handled::Yes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x_and_fraction_invert() {
        let w = 424.0;
        assert_eq!(x_of(0.0, w), MARGIN);
        assert_eq!(x_of(1.0, w), w - MARGIN);
        for f in [0.0, 0.25, 0.5, 1.0] {
            assert!((fraction_at(x_of(f, w), w) - f).abs() < 1e-6);
        }
        assert_eq!(fraction_at(-50.0, w), 0.0);
        assert_eq!(fraction_at(9999.0, w), 1.0);
    }
}
