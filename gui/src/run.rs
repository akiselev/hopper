//! Opening the window ([`run`]) and the headless shot ([`shot`]).

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use hopper_core::api::Debugger;
use skelter::seams::{CompositorApi, DocumentApi};

use skelter::{App, AppConfig, SpawnCompositor};
use skelter_base::event::{InputEnvelope, WindowEvent};
use skelter_base::geom::Size;
use skelter_base::ids::WindowId;
use skelter_base::time::{Span, Time};
use skelter_compositor::CompositorHandle;
use skelter_platform::{A11yEvent, AppHandler, PlatformCx, WindowDesc};

use crate::app::Viewer;
use crate::document::HopperDocument;

/// The fonts: the system's (the UI and code fonts in `style` fall back through fontconfig).
pub fn fonts() -> Arc<skelter_text::FontStore> {
    skelter_text::FontStore::new(skelter_text::FontConfig::default())
}

fn main_window() -> WindowDesc {
    let mut desc = WindowDesc::new("hopper", Size::new(1440.0, 900.0));
    desc.min_size = Some(Size::new(900.0, 560.0));
    desc.app_id = Some("hopper".into());
    desc
}

/// The platform's handler around the app runner: wakes from jobs, canvas registration, frames.
pub struct HopperApp {
    app: App<HopperDocument, CompositorHandle>,
}

impl std::fmt::Debug for HopperApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HopperApp").finish_non_exhaustive()
    }
}

impl HopperApp {
    pub fn new(doc: HopperDocument, spawn: SpawnCompositor<CompositorHandle>) -> Self {
        let cfg = AppConfig { windows: vec![main_window()], exit_on_last_window: true, ..AppConfig::default() };
        HopperApp { app: App::new(cfg, doc, spawn) }
    }

    /// After every callback: ask for frames for what changed outside input.
    fn sync(&mut self) {
        let windows: Vec<WindowId> = self.app.windows().collect();
        if let Some(c) = self.app.compositor() {
            for w in windows {
                if self.app.doc().needs_frame(w) {
                    c.request_ui_frame(w);
                }
            }
        }
    }
}

impl AppHandler for HopperApp {
    fn init(&mut self, cx: &mut dyn PlatformCx) {
        self.app.doc_mut().set_clock(cx.clock());
        let waker = cx.waker();
        self.app.doc().set_waker(Arc::new(move || waker.wake()));
        self.app.init(cx);
        if let Some(c) = self.app.compositor().cloned() {
            if let Some(canvas) = self.app.doc_mut().timeline_canvas() {
                c.register_canvas(canvas);
            }
            self.app.doc_mut().set_canvas_notify(Box::new(move |w, id| c.canvas_changed(w, id)));
        }
        self.app.doc_mut().start();
        self.sync();
    }

    fn input(&mut self, cx: &mut dyn PlatformCx, ev: InputEnvelope) {
        self.app.input(cx, ev);
        self.sync();
    }

    fn window_event(&mut self, cx: &mut dyn PlatformCx, window: WindowId, ev: WindowEvent) {
        self.app.window_event(cx, window, ev);
        self.sync();
    }

    fn wake(&mut self, cx: &mut dyn PlatformCx) {
        self.app.wake(cx);
        self.sync();
    }

    fn timer(&mut self, cx: &mut dyn PlatformCx, now: Time) {
        self.app.timer(cx, now);
        self.sync();
    }

    fn a11y_event(&mut self, cx: &mut dyn PlatformCx, window: WindowId, ev: A11yEvent) {
        self.app.a11y_event(cx, window, ev);
        self.sync();
    }

    fn exiting(&mut self, cx: &mut dyn PlatformCx) {
        self.app.exiting(cx);
    }
}

/// Opens the window over `dbg` and runs until it closes.
pub fn run(dbg: Arc<dyn Debugger>) -> Result<(), String> {
    let fonts = fonts();
    let doc = HopperDocument::new(Viewer::new(dbg), fonts.clone());
    let glyphs: Arc<dyn skelter_base::text::GlyphRasterizer> = Arc::new(skelter_text::FontRasterizer::new(fonts));
    let spawn = skelter::compositor(|clock| skelter_compositor::CompositorConfig::new(clock, Span::from_micros(16_667)), glyphs);
    skelter::run(Box::new(HopperApp::new(doc, spawn))).map(|_| ()).map_err(|e| format!("skelter: {e}"))
}

/// The window in skelter-test's headless harness.
pub type Harness = skelter_test::Harness<HopperDocument>;

/// A headless window over `dbg` at `size`, started and settled (every job answered).
pub fn harness(dbg: Arc<dyn Debugger>, size: (f32, f32)) -> Harness {
    let fonts = fonts();
    let mut doc = HopperDocument::new(Viewer::new(dbg), fonts.clone());
    doc.start();
    let canvas = doc.timeline_canvas();
    let glyphs: Arc<dyn skelter_base::text::GlyphRasterizer> = Arc::new(skelter_text::FontRasterizer::new(fonts));
    let mut h = skelter_test::Harness::<HopperDocument>::builder().with_size(Size::new(size.0, size.1)).with_glyphs(glyphs).build_unsettled(doc);
    if let Some(canvas) = canvas {
        h.set_textures(move |_, gpu| gpu.renderer.register_canvas(canvas.clone()));
    }
    settle(&mut h);
    h
}

/// Steps the harness until the document is settled: every job answered and drawn.
pub fn settle(h: &mut Harness) {
    for _ in 0..500 {
        h.run_ok();
        if h.state().settled() {
            // one more pass: the last frame may have asked for hit counts
            h.run_ok();
            if h.state().settled() {
                return;
            }
        }
        h.state().viewer.jobs.wait_landed(Duration::from_secs(5));
    }
}

/// Renders the window headlessly (the environment's wgpu adapter, lavapipe in a container) to
/// `out` as a PNG, after `keys` (debugger shortcuts such as `F10`, `Alt+Left`).
pub fn shot(dbg: Arc<dyn Debugger>, size: (f32, f32), keys: &[String], out: &Path) -> Result<(), String> {
    let mut h = harness(dbg, size);
    for k in keys {
        press(&mut h, k)?;
    }
    let img = h.render().map_err(|e| e.0)?;
    img.save_png(out).map_err(|e| format!("{}: {e}", out.display()))
}

/// Presses a key chord like `F10`, `Ctrl+F10`, `Shift+F11`, `Alt+Left` and settles.
pub fn press(h: &mut Harness, chord: &str) -> Result<(), String> {
    use skelter_base::event::Modifiers;
    use skelter_base::key::Key;
    let mut mods = Modifiers::NONE;
    let mut key = None;
    for part in chord.split('+') {
        match part {
            "Ctrl" => mods = mods.union(Modifiers::CTRL),
            "Shift" => mods = mods.union(Modifiers::SHIFT),
            "Alt" => mods = mods.union(Modifiers::ALT),
            "Left" => key = Some(Key::ArrowLeft),
            "Right" => key = Some(Key::ArrowRight),
            "F5" => key = Some(Key::F5),
            "F10" => key = Some(Key::F10),
            "F11" => key = Some(Key::F11),
            "L" => key = Some(Key::L),
            "C" => key = Some(Key::C),
            other => return Err(format!("unknown key {other:?} in {chord:?}")),
        }
    }
    let key = key.ok_or_else(|| format!("no key in {chord:?}"))?;
    h.key_press_modifiers(mods, key);
    settle(h);
    Ok(())
}
