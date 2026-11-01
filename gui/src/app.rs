//! [`Viewer`]: the GUI's state over one `Arc<dyn Debugger>`.
//!
//! Every engine call is a job ([`crate::jobs`]); a completion updates this state and marks what
//! changed ([`Changed`]), and [`crate::ui`] pushes the changed parts into the skelter document once
//! per frame. Results that belong to an older stop (a locals page that arrives after the next F10)
//! are dropped by comparing the stop generation they were asked for.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use hopper_core::api::{
    Budget, CallFilter, CallId, Debugger, Frame, IndexStatus, OriginChain, OriginEnd, OwnershipKind, OwnershipState,
    Page, RecordingInfo, Stop, StopCause, TimelineEvent, Until, ValueHandle, ValueView,
};
use hopper_core::api::{Completeness, Confidence, MachineKind, PageReq};
use hopper_core::target::Direction;
use hopper_core::{FrameId, HopperError, Moment, MomentRange, PositionUrl, SourceLoc};

use crate::calls::CallTree;
use crate::gutter::{Arrow, ArrowAt, HitCount, HitCounts};
use crate::jobs::{Jobs, Lane};
use crate::nav::{Command, Move, NavState, move_of};
use crate::rows::{RowCell, Tone};
use crate::timeline_model::TimelineModel;
use crate::tree::{Fetch, LazyTree};

/// Children fetched per page in the locals and the call tree.
pub const PAGE: u32 = 100;
/// How often the index status is polled while the index builds.
pub const INDEX_POLL: Duration = Duration::from_millis(300);

pub type LocalsTree = LazyTree<ValueHandle, ValueView>;

/// Which parts of the view changed since the UI last synced.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Changed {
    pub source: bool,
    pub stack: bool,
    pub locals: bool,
    pub calls: bool,
    pub timeline: bool,
    pub query: bool,
    pub status: bool,
    pub url_box: bool,
}

impl Changed {
    pub fn all() -> Self {
        Changed { source: true, stack: true, locals: true, calls: true, timeline: true, query: true, status: true, url_box: true }
    }
    pub fn any(&self) -> bool {
        *self != Changed::default()
    }
}

/// What clicking a query-pane row does.
#[derive(Clone, Debug, PartialEq)]
pub enum QueryAction {
    Seek(Moment),
}

/// One row of the query pane.
#[derive(Clone, Debug, PartialEq)]
pub struct QueryLine {
    pub depth: u32,
    pub cells: Vec<RowCell>,
    pub action: Option<QueryAction>,
}

impl QueryLine {
    fn text(depth: u32, text: impl Into<String>, tone: Tone) -> Self {
        QueryLine { depth, cells: vec![RowCell::new(text, tone)], action: None }
    }
}

/// The source pane's state.
#[derive(Debug, Default)]
pub struct SourceView {
    /// The file shown (as the debug info names it).
    pub file: Option<String>,
    pub doc: Option<Rc<RefCell<quire_core::Document>>>,
    pub generation: u64,
    pub message: String,
    /// Centre this line on the next frame.
    pub reveal: Option<u32>,
    pub arrows: Vec<ArrowAt>,
    /// Hit counts per file.
    pub hits: HashMap<String, HitCounts>,
    texts: HashMap<String, Arc<str>>,
    loading: Option<String>,
}

/// The position-URL box.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum UrlBox {
    #[default]
    Closed,
    /// Open, prefilled with this text.
    Open(String),
}

/// A navigation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NavRequest {
    Command(Command),
    Seek(Moment),
}

/// The GUI's state.
pub struct Viewer {
    pub jobs: Jobs<dyn Debugger, Viewer>,
    pub recording: Option<RecordingInfo>,
    pub stop: Option<Stop>,
    /// Bumped by every stop; jobs asked before it are stale.
    pub generation: u64,
    pub nav: NavState,
    /// Navigation jobs in flight.
    pub navigating: usize,
    pub frames: Vec<Frame>,
    pub selected_frame: usize,
    pub source: SourceView,
    pub locals: LocalsTree,
    pub locals_selected: Option<usize>,
    pub calls: Rc<RefCell<CallTree>>,
    pub calls_selected: Option<usize>,
    pub timeline: TimelineModel,
    /// A timeline seek is in flight; the latest wanted target waits here.
    seek_in_flight: bool,
    seek_next: Option<Moment>,
    pub query: Vec<QueryLine>,
    pub index: Option<IndexStatus>,
    pub message: Option<(String, Tone)>,
    pub url_box: UrlBox,
    /// Text the UI copies to the clipboard at its next chance.
    pub copy: Option<String>,
    pub changed: Changed,
}

impl std::fmt::Debug for Viewer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Viewer").field("stop", &self.stop).field("generation", &self.generation).finish_non_exhaustive()
    }
}

fn err_text(e: &HopperError) -> String {
    e.to_string()
}

impl Viewer {
    pub fn new(dbg: Arc<dyn Debugger>) -> Self {
        Viewer {
            jobs: Jobs::new(dbg),
            recording: None,
            stop: None,
            generation: 0,
            nav: NavState::default(),
            navigating: 0,
            frames: Vec::new(),
            selected_frame: 0,
            source: SourceView { message: "Opening the recording…".into(), ..SourceView::default() },
            locals: LocalsTree::new(PAGE),
            locals_selected: None,
            calls: Rc::new(RefCell::new(CallTree::new(PAGE))),
            calls_selected: None,
            timeline: TimelineModel::default(),
            seek_in_flight: false,
            seek_next: None,
            query: vec![QueryLine::text(0, "Click a value in Locals to see where it came from.", Tone::Muted)],
            index: None,
            message: None,
            url_box: UrlBox::Closed,
            copy: None,
            changed: Changed::all(),
        }
    }

    /// Runs the completions of every finished job.
    pub fn drain(&mut self) {
        for c in self.jobs.drain() {
            c(self);
        }
    }

    pub fn is_idle(&self) -> bool {
        self.jobs.is_idle()
    }

    fn say(&mut self, text: impl Into<String>, tone: Tone) {
        self.message = Some((text.into(), tone));
        self.changed.status = true;
    }

    // ---------------------------------------------------------------- start

    /// The first questions: the recording, the cursor, the timeline, the call roots, the index.
    pub fn start(&mut self) {
        self.jobs.submit(Lane::Replay, |d| d.recording(), |v, r| match r {
            Ok(info) => {
                v.timeline.start = info.start;
                v.timeline.end = info.end;
                v.timeline.version += 1;
                v.recording = Some(info);
                v.changed.timeline = true;
                v.changed.status = true;
            }
            Err(e) => v.say(format!("recording: {}", err_text(&e)), Tone::Error),
        });
        self.jobs.submit(Lane::Replay, |d| d.cursor(), |v, r| match r {
            Ok(stop) => {
                v.nav.start(stop.at);
                v.on_stop(stop);
            }
            Err(e) => {
                v.source.message = format!("No cursor: {}", err_text(&e));
                v.changed.source = true;
                v.say(format!("cursor: {}", err_text(&e)), Tone::Error);
            }
        });
        self.jobs.submit(
            Lane::Replay,
            |d| d.timeline(MomentRange::all(), PageReq::first(10_000)),
            |v, r: hopper_core::Result<Page<TimelineEvent>>| match r {
                Ok(page) => {
                    v.timeline.events = page.items;
                    v.timeline.version += 1;
                    v.changed.timeline = true;
                }
                Err(e) => v.say(format!("timeline: {}", err_text(&e)), Tone::Error),
            },
        );
        let fetch = self.calls.borrow_mut().reset();
        self.fetch_calls(fetch);
        self.poll_index(None);
    }

    fn poll_index(&mut self, delay: Option<Duration>) {
        self.jobs.submit_after(Lane::Status, delay, |d| d.index_status(), |v, s: IndexStatus| {
            let was_done = v.index.as_ref().is_some_and(|i| i.done);
            let done = s.done;
            v.index = Some(s);
            v.changed.status = true;
            if done && !was_done {
                // counts and calls asked while the index was partial are asked again
                for h in v.source.hits.values_mut() {
                    h.forget_incomplete();
                }
                v.changed.source = true;
            }
            if !done {
                v.poll_index(Some(INDEX_POLL));
            }
        });
    }

    // ---------------------------------------------------------------- navigation

    /// A key or a toolbar button.
    pub fn command(&mut self, cmd: Command) {
        match cmd {
            Command::OpenUrlBox => {
                let url = self.stop.as_ref().map(|s| s.at.url().to_string()).unwrap_or_default();
                self.url_box = UrlBox::Open(url);
                self.changed.url_box = true;
            }
            Command::CopyUrl => match &self.stop {
                Some(s) => {
                    let url = s.at.url().to_string();
                    self.say(format!("Copied {url}"), Tone::Normal);
                    self.copy = Some(url);
                }
                None => self.say("No position to copy yet", Tone::Warning),
            },
            _ => self.navigate(NavRequest::Command(cmd)),
        }
    }

    pub fn navigate(&mut self, req: NavRequest) {
        self.navigating += 1;
        self.changed.status = true;
        self.jobs.submit(
            Lane::Replay,
            move |d| -> hopper_core::Result<Option<Stop>> {
                match req {
                    NavRequest::Command(Command::Step { kind, dir }) => d.step(kind, dir).map(Some),
                    NavRequest::Command(Command::Continue { dir }) => d.run(dir, Until::Breakpoints).map(Some),
                    NavRequest::Command(Command::Back) => d.back(),
                    NavRequest::Command(Command::Forward) => d.forward(),
                    NavRequest::Command(Command::OpenUrlBox | Command::CopyUrl) => Ok(None),
                    NavRequest::Seek(m) => d.seek(m).map(Some),
                }
            },
            move |v, r| {
                v.navigating -= 1;
                v.changed.status = true;
                if let NavRequest::Seek(_) = req {
                    v.seek_in_flight = false;
                }
                match r {
                    Ok(Some(stop)) => {
                        let how = match req {
                            NavRequest::Command(c) => move_of(c),
                            NavRequest::Seek(_) => Move::New,
                        };
                        v.nav.moved(stop.at, how);
                        v.on_stop(stop);
                    }
                    Ok(None) => {
                        let what = match req {
                            NavRequest::Command(Command::Back) => "Nothing to go back to",
                            _ => "Nothing to go forward to",
                        };
                        v.say(what, Tone::Muted);
                    }
                    Err(e) => v.say(err_text(&e), Tone::Error),
                }
                if let Some(next) = v.seek_next.take()
                    && v.stop.as_ref().is_none_or(|s| s.at != next)
                {
                    v.seek(next);
                }
            },
        );
    }

    /// Seeks, coalescing a drag's stream of targets to one seek in flight.
    pub fn seek(&mut self, to: Moment) {
        if self.seek_in_flight {
            self.seek_next = Some(to);
            return;
        }
        self.seek_in_flight = true;
        self.navigate(NavRequest::Seek(to));
    }

    /// The timeline was pressed or dragged at `fraction`.
    pub fn timeline_seek(&mut self, fraction: f64) {
        let Some(target) = self.timeline.seek_target(fraction) else { return };
        let current = if self.seek_in_flight { self.seek_next } else { self.stop.as_ref().map(|s| s.at) };
        if current != Some(target) {
            self.seek(target);
        }
    }

    /// Opens a pasted position URL.
    pub fn open_url(&mut self, text: &str) {
        match text.trim().parse::<PositionUrl>() {
            Ok(PositionUrl(m)) => {
                if let Some(r) = &self.recording
                    && r.id != m.recording
                {
                    self.say(format!("That URL is for recording {}, not {}", m.recording, r.id), Tone::Error);
                    return;
                }
                self.url_box = UrlBox::Closed;
                self.changed.url_box = true;
                self.seek(m);
            }
            Err(e) => self.say(err_text(&e), Tone::Error),
        }
    }

    pub fn close_url_box(&mut self) {
        self.url_box = UrlBox::Closed;
        self.changed.url_box = true;
    }

    fn on_stop(&mut self, stop: Stop) {
        self.generation += 1;
        self.timeline.cursor = Some(stop.at);
        self.timeline.version += 1;
        self.changed.timeline = true;
        let cause = match &stop.cause {
            StopCause::Step => None,
            StopCause::Breakpoint { id } => Some(format!("breakpoint {}", id.0)),
            StopCause::Watchpoint { .. } => Some("watchpoint".to_string()),
            StopCause::Signal { signo } => Some(format!("signal {signo}")),
            StopCause::Exited { code } => Some(format!("exited with {code}")),
            StopCause::HistoryStart => Some("start of the recording".to_string()),
            StopCause::HistoryEnd => Some("end of the recording".to_string()),
            StopCause::Jump => None,
        };
        match cause {
            Some(c) => self.say(c, Tone::Muted),
            None => {
                self.message = None;
                self.changed.status = true;
            }
        }
        let (at, tid) = (stop.at, stop.tid);
        self.frames.clear();
        self.selected_frame = 0;
        self.changed.stack = true;
        // until the frames arrive, the stop's own location is the source's
        let loc = stop.location.clone();
        self.stop = Some(stop);
        self.show_location(loc.as_ref(), Arrow::Execution);
        let generation = self.generation;
        self.jobs.submit(Lane::Replay, move |d| d.frames(at, tid), move |v, r| {
            if v.generation != generation {
                return;
            }
            match r {
                Ok(frames) => {
                    v.frames = frames;
                    v.changed.stack = true;
                    v.load_locals();
                }
                Err(e) => {
                    v.say(format!("frames: {}", err_text(&e)), Tone::Error);
                    v.locals.set_roots(Vec::new());
                    v.changed.locals = true;
                }
            }
        });
    }

    /// Selects stack frame `i`: its location in the source, its locals.
    pub fn select_frame(&mut self, i: usize) {
        if i >= self.frames.len() || i == self.selected_frame {
            return;
        }
        self.selected_frame = i;
        self.changed.stack = true;
        let loc = self.frames[i].location.clone();
        self.show_location(loc.as_ref(), if i == 0 { Arrow::Execution } else { Arrow::Frame });
        self.load_locals();
    }

    fn arrows(&self) -> Vec<ArrowAt> {
        let file = self.source.file.as_deref();
        let mut out = Vec::new();
        let here = |f: &Frame| f.location.as_ref().filter(|l| Some(l.file.as_str()) == file).map(|l| l.line);
        if let Some(line) = self.frames.first().and_then(here).or_else(|| {
            self.stop.as_ref().and_then(|s| s.location.as_ref()).filter(|l| Some(l.file.as_str()) == file).map(|l| l.line)
        }) {
            out.push(ArrowAt { line, arrow: Arrow::Execution });
        }
        if self.selected_frame > 0
            && let Some(line) = self.frames.get(self.selected_frame).and_then(here)
        {
            out.push(ArrowAt { line, arrow: Arrow::Frame });
        }
        out
    }

    fn show_location(&mut self, loc: Option<&SourceLoc>, _arrow: Arrow) {
        self.changed.source = true;
        let Some(loc) = loc else {
            self.source.arrows = self.arrows();
            if self.source.file.is_none() {
                self.source.message = "No source for this position".into();
            }
            return;
        };
        self.source.reveal = Some(loc.line);
        if self.source.file.as_deref() != Some(loc.file.as_str()) {
            self.open_file(&loc.file);
        }
        self.source.arrows = self.arrows();
    }

    fn open_file(&mut self, file: &str) {
        if let Some(text) = self.source.texts.get(file).cloned() {
            self.set_source(file, &text);
            return;
        }
        if self.source.loading.as_deref() == Some(file) {
            return;
        }
        self.source.loading = Some(file.to_string());
        let f = file.to_string();
        self.jobs.submit(Lane::Replay, move |d| (f.clone(), d.source_text(&f)), |v, (f, r)| {
            if v.source.loading.as_deref() == Some(f.as_str()) {
                v.source.loading = None;
            }
            match r {
                Ok(text) => {
                    v.source.texts.insert(f.clone(), text.clone());
                    // show it if it is still the file the cursor (or selected frame) is in
                    let wanted = v
                        .frames
                        .get(v.selected_frame)
                        .and_then(|fr| fr.location.as_ref())
                        .or_else(|| v.stop.as_ref().and_then(|s| s.location.as_ref()))
                        .map(|l| l.file.clone());
                    if wanted.as_deref() == Some(f.as_str()) || wanted.is_none() {
                        v.set_source(&f, &text);
                    }
                }
                Err(e) => {
                    v.source.file = None;
                    v.source.doc = None;
                    v.source.generation += 1;
                    v.source.message = format!("{f}: {}", err_text(&e));
                    v.changed.source = true;
                }
            }
        });
    }

    fn set_source(&mut self, file: &str, text: &str) {
        let config = quire_core::EditorConfig { read_only: true, ..quire_core::EditorConfig::default() };
        self.source.doc = Some(Rc::new(RefCell::new(quire_core::Document::new(text, config))));
        self.source.file = Some(file.to_string());
        self.source.generation += 1;
        self.source.arrows = self.arrows();
        self.changed.source = true;
    }

    /// The source pane laid out `visible` lines of the current file: ask for their hit counts.
    pub fn lines_shown(&mut self, visible: &[u32]) {
        let Some(file) = self.source.file.clone() else { return };
        let ask = self.source.hits.entry(file.clone()).or_default().take_unasked(visible.iter().copied());
        if ask.is_empty() {
            return;
        }
        self.jobs.submit(
            Lane::Replay,
            {
                let file = file.clone();
                move |d| {
                    ask.into_iter()
                        .map(|line| {
                            let count = match d.line_hits(SourceLoc::new(file.clone(), line), MomentRange::all(), PageReq::first(1)) {
                                Ok(p) => HitCount::Known {
                                    count: p.total.unwrap_or(p.items.len() as u64),
                                    partial: matches!(p.completeness, Completeness::Partial { .. }),
                                },
                                Err(_) => HitCount::Unavailable,
                            };
                            (line, count)
                        })
                        .collect::<Vec<_>>()
                }
            },
            move |v, counts| {
                let hits = v.source.hits.entry(file).or_default();
                for (line, c) in counts {
                    hits.set(line, c);
                }
                v.changed.source = true;
            },
        );
    }

    /// A press on a line's hit count: every execution of that line, in the query pane.
    pub fn line_executions(&mut self, line: u32) {
        let Some(file) = self.source.file.clone() else { return };
        let loc = SourceLoc::new(file, line);
        let l2 = loc.clone();
        self.jobs.submit(Lane::Replay, move |d| d.line_hits(l2, MomentRange::all(), PageReq::first(500)), move |v, r| {
            v.query.clear();
            match r {
                Ok(page) => {
                    let total = page.total.unwrap_or(page.items.len() as u64);
                    let partial = if matches!(page.completeness, Completeness::Partial { .. }) { " (index still building)" } else { "" };
                    v.query.push(QueryLine::text(0, format!("Executions of {loc}: {total}{partial}"), Tone::Heading));
                    for (i, m) in page.items.iter().enumerate() {
                        v.query.push(QueryLine {
                            depth: 1,
                            cells: vec![RowCell::new(format!("#{}", i + 1), Tone::Muted), RowCell::mono(m.to_string(), Tone::Accent)],
                            action: Some(QueryAction::Seek(*m)),
                        });
                    }
                    if page.items.is_empty() {
                        v.query.push(QueryLine::text(1, "never executed", Tone::Muted));
                    }
                }
                Err(e) => v.query.push(QueryLine::text(0, format!("{loc}: {}", err_text(&e)), Tone::Error)),
            }
            v.changed.query = true;
        });
    }

    // ---------------------------------------------------------------- locals

    fn load_locals(&mut self) {
        let Some(stop) = &self.stop else { return };
        let at = stop.at;
        let frame = self.frames.get(self.selected_frame).map(|f| f.id).unwrap_or(FrameId { tid: stop.tid, depth: 0 });
        let generation = self.generation;
        let depth = self.selected_frame;
        self.locals = LocalsTree::new(PAGE);
        self.locals_selected = None;
        self.changed.locals = true;
        self.jobs.submit(Lane::Replay, move |d| d.locals(at, frame), move |v, r| {
            if v.generation != generation || v.selected_frame != depth {
                return;
            }
            match r {
                Ok(vals) => v.locals.set_roots(vals.into_iter().map(|x| (x.handle.clone(), x)).collect()),
                Err(e) => {
                    v.locals.set_roots(Vec::new());
                    v.say(format!("locals: {}", err_text(&e)), Tone::Error);
                }
            }
            v.changed.locals = true;
        });
    }

    /// Toggles locals row `i` (lazily fetching the value's children).
    pub fn toggle_local(&mut self, i: usize) {
        if let Some(f) = self.locals.activate(i) {
            self.fetch_children(f);
        }
        self.changed.locals = true;
    }

    fn fetch_children(&mut self, f: Fetch<ValueHandle>) {
        let Some(parent) = f.parent else { return };
        let total = self.locals.item(&parent).and_then(|p| p.child_count);
        let generation = self.generation;
        let page = f.page;
        let p2 = parent.clone();
        self.jobs.submit(Lane::Replay, move |d| d.children(&p2, page), move |v, r| {
            if v.generation != generation {
                return;
            }
            match r {
                Ok(kids) => v.locals.children_loaded(Some(&parent), page.offset, kids.into_iter().map(|x| (x.handle.clone(), x)).collect(), total),
                Err(e) => {
                    v.locals.fetch_failed(Some(&parent));
                    v.say(format!("children: {}", err_text(&e)), Tone::Error);
                }
            }
            v.changed.locals = true;
        });
    }

    /// Selects locals row `i` and asks where its value came from.
    pub fn select_local(&mut self, i: usize) {
        self.locals_selected = Some(i);
        self.changed.locals = true;
        let Some(crate::tree::Row::Node { key, .. }) = self.locals.row(i).cloned() else {
            if let Some(f) = self.locals.activate(i) {
                self.fetch_children(f);
            }
            return;
        };
        let Some(view) = self.locals.item(&key).cloned() else { return };
        self.query = vec![QueryLine::text(0, format!("Where did {} come from?", view.name), Tone::Heading), QueryLine::text(1, "tracing…", Tone::Muted)];
        self.changed.query = true;
        self.jobs.submit(Lane::Replay, move |d| d.origin(&key, Budget::default()), move |v, r| {
            v.query = origin_lines(&view, r);
            v.changed.query = true;
        });
    }

    /// The query pane's row `i` was activated.
    pub fn query_activate(&mut self, i: usize) {
        if let Some(QueryLine { action: Some(QueryAction::Seek(m)), .. }) = self.query.get(i).cloned() {
            self.seek(m);
        }
    }

    /// The console's expression, evaluated in the selected frame at the cursor.
    pub fn eval(&mut self, expr: &str) {
        let expr = expr.trim().to_string();
        if expr.is_empty() {
            return;
        }
        let Some(stop) = &self.stop else { return };
        let at = stop.at;
        let frame = self.frames.get(self.selected_frame).map(|f| f.id).unwrap_or(FrameId { tid: stop.tid, depth: 0 });
        self.jobs.submit(Lane::Replay, {
            let expr = expr.clone();
            move |d| d.eval(at, frame, &expr)
        }, move |v, r| {
            match r {
                Ok(val) => {
                    let (summary, tone) = value_summary(&val);
                    v.query.push(QueryLine {
                        depth: 0,
                        cells: vec![RowCell::mono(format!("> {expr}"), Tone::Accent), RowCell::mono(summary, tone), RowCell::new(val.type_name.clone(), Tone::Muted)],
                        action: None,
                    });
                }
                Err(e) => v.query.push(QueryLine { depth: 0, cells: vec![RowCell::mono(format!("> {expr}"), Tone::Accent), RowCell::new(err_text(&e), Tone::Error)], action: None }),
            }
            v.changed.query = true;
        });
    }

    // ---------------------------------------------------------------- calls

    fn fetch_calls(&mut self, f: Fetch<CallId>) {
        let filter = CallFilter { parent: f.parent, ..CallFilter::default() };
        let page = f.page;
        let parent = f.parent;
        self.jobs.submit(Lane::Replay, move |d| d.calls(filter, page), move |v, r| {
            match r {
                Ok(p) => {
                    let items = p.items.into_iter().map(|c| (c.id, c)).collect();
                    v.calls.borrow_mut().children_loaded(parent.as_ref(), page.offset, items, p.total);
                }
                Err(e) => {
                    v.calls.borrow_mut().fetch_failed(parent.as_ref());
                    v.say(format!("calls: {}", err_text(&e)), Tone::Error);
                }
            }
            v.changed.calls = true;
        });
    }

    /// Activates call-tree row `i`: expands or collapses it, or loads the next page.
    pub fn activate_call(&mut self, i: usize) {
        let f = self.calls.borrow_mut().activate(i);
        if let Some(f) = f {
            self.fetch_calls(f);
        }
        self.changed.calls = true;
    }

    /// Selects call-tree row `i`: the call's entry and exit in the query pane, to jump to.
    pub fn select_call(&mut self, i: usize) {
        self.calls_selected = Some(i);
        let tree = self.calls.borrow();
        let Some(crate::tree::Row::Node { key, .. }) = tree.row(i).cloned() else { return };
        let Some(c) = tree.item(&key).cloned() else { return };
        drop(tree);
        let mut q = vec![QueryLine::text(0, format!("Call of {}", c.function), Tone::Heading)];
        if let Some(l) = &c.location {
            q.push(QueryLine::text(1, format!("at {l}"), Tone::Muted));
        }
        q.push(QueryLine { depth: 1, cells: vec![RowCell::new("entry", Tone::Normal), RowCell::mono(c.entry.to_string(), Tone::Accent)], action: Some(QueryAction::Seek(c.entry)) });
        match c.exit {
            Some(x) => q.push(QueryLine { depth: 1, cells: vec![RowCell::new("exit", Tone::Normal), RowCell::mono(x.to_string(), Tone::Accent)], action: Some(QueryAction::Seek(x)) }),
            None => q.push(QueryLine::text(1, "never returned inside the recording", Tone::Muted)),
        }
        q.push(QueryLine::text(1, format!("{} direct calls, thread {}", c.child_count, c.tid), Tone::Muted));
        q.push(QueryLine::text(1, "Double-click a row here to jump; double-click a call to expand it.", Tone::Muted));
        self.query = q;
        self.changed.query = true;
    }

    // ---------------------------------------------------------------- status

    /// The status bar's text.
    pub fn status_text(&self) -> (String, Tone) {
        if let Some((m, t)) = &self.message {
            return (m.clone(), *t);
        }
        match &self.stop {
            Some(s) => {
                let f = s.function.clone().unwrap_or_else(|| format!("{:#x}", s.pc));
                let at = s.location.as_ref().map(|l| format!(" at {l}")).unwrap_or_default();
                (format!("{f}{at}"), Tone::Normal)
            }
            None => ("Opening…".into(), Tone::Muted),
        }
    }

    /// The index progress, for the status bar.
    pub fn index_text(&self) -> String {
        match &self.index {
            None => String::new(),
            Some(s) if s.error.is_some() => format!("index failed: {}", s.error.as_deref().unwrap_or("")),
            Some(s) if s.done => "index complete".into(),
            Some(s) => {
                let phases: Vec<String> = s.phases.iter().map(|(n, p)| format!("{n} {:.0}%", p * 100.0)).collect();
                format!("indexing: {}", phases.join(", "))
            }
        }
    }

    pub fn dir_label(dir: Direction) -> &'static str {
        match dir {
            Direction::Forward => "",
            Direction::Reverse => "reverse ",
        }
    }
}

/// A value's one-line text and tone: greyed with its ownership state when it was moved out or
/// dropped.
pub fn value_summary(v: &ValueView) -> (String, Tone) {
    match &v.ownership {
        OwnershipState::Live | OwnershipState::Unknown => (v.summary.clone(), Tone::Normal),
        OwnershipState::MovedOut { to, at, .. } => {
            let to = to.as_deref().map(|t| format!(" to {t}")).unwrap_or_default();
            let at = at.as_ref().map(|l| format!(" at {}:{}", short_file(&l.file), l.line)).unwrap_or_default();
            (format!("{}  (moved out{to}{at})", v.summary), Tone::Muted)
        }
        OwnershipState::Dropped { at, .. } => {
            let at = at.as_ref().map(|l| format!(" at {}:{}", short_file(&l.file), l.line)).unwrap_or_default();
            (format!("{}  (dropped{at})", v.summary), Tone::Muted)
        }
    }
}

/// The last path component.
pub fn short_file(f: &str) -> &str {
    f.rsplit('/').next().unwrap_or(f)
}

pub fn ownership_kind(k: OwnershipKind) -> &'static str {
    match k {
        OwnershipKind::Move => "move",
        OwnershipKind::Copy => "copy",
        OwnershipKind::Clone => "clone",
        OwnershipKind::SharedBorrow => "shared borrow",
        OwnershipKind::MutBorrow => "mutable borrow",
        OwnershipKind::Projection => "projection",
        OwnershipKind::Drop => "drop",
        OwnershipKind::Aggregate => "aggregate",
        OwnershipKind::Literal => "literal",
        OwnershipKind::CallReturn => "call return",
        OwnershipKind::StorageDead => "storage dead",
    }
}

pub fn confidence(c: Confidence) -> (&'static str, Tone) {
    match c {
        Confidence::Exact => ("exact", Tone::Accent),
        Confidence::Inferred => ("inferred", Tone::Warning),
        Confidence::Heuristic => ("heuristic", Tone::Warning),
    }
}

fn machine(k: MachineKind) -> &'static str {
    match k {
        MachineKind::Copy => "copy",
        MachineKind::Arithmetic => "arithmetic",
        MachineKind::Immediate => "immediate",
        MachineKind::Syscall => "syscall",
        MachineKind::Unknown => "unknown",
    }
}

fn place_text(p: &hopper_core::Place) -> String {
    match p {
        hopper_core::Place::Memory { addr, len, .. } => format!("{addr:#x}+{len}"),
        hopper_core::Place::Register { reg, frame } => format!("{} (frame {})", reg.name(), frame.depth),
    }
}

/// The query pane's rows for an origin answer: one row per hop (its ownership label and
/// confidence, or the machine copy when the sidecar labelled none), a machine detail row under
/// it, and how the chain ended. Each hop row seeks to its moment.
pub fn origin_lines(of: &ValueView, r: hopper_core::Result<OriginChain>) -> Vec<QueryLine> {
    let mut q = vec![QueryLine::text(0, format!("Where did {} = {} come from?", of.name, of.summary), Tone::Heading)];
    let chain = match r {
        Ok(c) => c,
        Err(e) => {
            q.push(QueryLine::text(1, err_text(&e), Tone::Error));
            return q;
        }
    };
    for (i, hop) in chain.hops.iter().enumerate() {
        let loc = hop.location.as_ref().map(|l| format!("{}:{}", short_file(&l.file), l.line)).unwrap_or_default();
        let cells = match &hop.ownership {
            Some(o) => {
                let flow = match (&o.dst, &o.src) {
                    (Some(d), Some(s)) => format!("{d} ← {s}"),
                    (Some(d), None) => d.clone(),
                    (None, Some(s)) => format!("← {s}"),
                    (None, None) => String::new(),
                };
                let (conf, tone) = confidence(o.confidence);
                vec![
                    RowCell::new(format!("{}  {}", i + 1, ownership_kind(o.kind)), Tone::Heading),
                    RowCell::mono(flow, Tone::Normal),
                    RowCell::new(loc, Tone::Muted),
                    RowCell::new(conf, tone),
                ]
            }
            None => vec![
                RowCell::new(format!("{}  {}", i + 1, machine(hop.machine)), Tone::Normal),
                RowCell::mono(format!("{} ← {}", place_text(&hop.written), hop.read.as_ref().map(place_text).unwrap_or_else(|| "?".into())), Tone::Normal),
                RowCell::new(loc, Tone::Muted),
                RowCell::new("machine", Tone::Muted),
            ],
        };
        q.push(QueryLine { depth: 1, cells, action: Some(QueryAction::Seek(hop.at)) });
        let func = hop.function.clone().unwrap_or_else(|| format!("{:#x}", hop.pc));
        let alloc = hop.allocation.map(|a| format!(", allocation #{}", a.0)).unwrap_or_default();
        let detail = format!("{} in {func}: wrote {}{alloc} at {}", machine(hop.machine), place_text(&hop.written), hop.at);
        q.push(QueryLine { depth: 2, cells: vec![RowCell::mono(detail, Tone::Muted)], action: Some(QueryAction::Seek(hop.at)) });
    }
    let end = match &chain.end {
        OriginEnd::Literal => "ends at a literal (read-only data)".to_string(),
        OriginEnd::Input { syscall } => format!("ends at input{}", syscall.as_deref().map(|s| format!(" ({s})")).unwrap_or_default()),
        OriginEnd::Computed => "ends at a computation (its operands are in the last hop)".to_string(),
        OriginEnd::HistoryStart => "ends at the start of the recording".to_string(),
        OriginEnd::Budget => "stopped: the search budget ran out".to_string(),
        OriginEnd::Unknown { reason } => format!("stopped: {reason}"),
    };
    q.push(QueryLine::text(1, end, Tone::Muted));
    q
}
