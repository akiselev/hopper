//! Omniscient queries answered without an index, by replaying on background replicas: the line
//! hits a frame has executed (for ownership), every execution of a line, the calls of a
//! function, a value's history, the last write to a place and a value's origin.
//!
//! Each replay is bounded by [`crate::SessionOptions::query_budget`] resumes; a replay that runs
//! out says [`Completeness::Partial`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hopper_core::api::{
    Budget, CallFilter, CallId, CallNode, Completeness, OriginChain, Page, PageReq, ProcessInfo,
    ValueHandle, ValueTarget, ValueView, WriteEvent,
};
use hopper_core::target::{
    BreakKind, Breakpoint, Direction, Memory, Resume, StopReason, Target, TargetMemory,
};
use hopper_core::{FrameId, HopperError, Moment, MomentRange, Place, Result, SourceLoc, Tid};
use hopper_symbols::{StackFrame, SymbolSet};

use crate::ownership::{self, LineHit};
use crate::provenance::Reverse;
use crate::stepped::{StepRecording, StepReplica, WindowEnd, WindowStart};
use crate::view::MomentView;
use crate::{Mode, Session, Shared, at_moment, lock, watch_pieces};

/// What identifies one frame instance across moments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameKey {
    pub tid: Tid,
    pub cfa: u64,
    /// The physical function's linkage name (or its name), as the sidecar keys it.
    pub linkage: String,
    pub entry: u64,
    /// The line the frame is on (its call site, for a caller).
    pub line: Option<u32>,
}

impl FrameKey {
    pub fn of(syms: &SymbolSet, f: &StackFrame) -> Option<FrameKey> {
        let func = syms.function_at(f.lookup_pc)?;
        Some(FrameKey {
            tid: f.frame.id.tid,
            cfa: f.cfa?,
            linkage: func.linkage_name.clone().unwrap_or(func.name.clone()),
            entry: func.entry,
            line: f.frame.location.as_ref().map(|l| l.line),
        })
    }
}

/// A budget counter for one replay.
pub(crate) struct Spend {
    used: u64,
    limit: u64,
}

impl Spend {
    pub(crate) fn new(limit: u64) -> Spend {
        Spend { used: 0, limit }
    }

    fn resume(&mut self, t: &mut dyn Target, how: Resume, dir: Direction) -> Result<StopReason> {
        self.used += 1;
        if self.used > self.limit {
            return Err(HopperError::BudgetExhausted { steps: self.used });
        }
        Ok(t.resume(how, dir)?.reason)
    }
}

/// Insert code breakpoints; remove them when dropped.
struct Inserted<'a> {
    t: &'a mut dyn Target,
    bps: Vec<Breakpoint>,
}

impl<'a> Inserted<'a> {
    fn new(t: &'a mut dyn Target) -> Inserted<'a> {
        Inserted { t, bps: Vec::new() }
    }

    fn add(&mut self, bp: Breakpoint) -> Result<()> {
        if !self.bps.contains(&bp) {
            self.t.insert(bp)?;
            self.bps.push(bp);
        }
        Ok(())
    }

    fn remove(&mut self, bp: Breakpoint) {
        if let Some(i) = self.bps.iter().position(|b| *b == bp) {
            let _ = self.t.remove(bp);
            self.bps.remove(i);
        }
    }
}

impl Drop for Inserted<'_> {
    fn drop(&mut self) {
        for bp in self.bps.drain(..) {
            let _ = self.t.remove(bp);
        }
    }
}

/// Run back to the entry of the frame instance `key` (stack pointer CFA - 8). `false` when the
/// recording starts first.
pub(crate) fn reverse_to_entry(
    t: &mut dyn Target,
    key: &FrameKey,
    spend: &mut Spend,
) -> Result<bool> {
    let bp = Breakpoint::code(key.entry);
    let mut ins = Inserted::new(t);
    ins.add(bp)?;
    loop {
        match spend.resume(ins.t, Resume::Continue, Direction::Reverse)? {
            StopReason::Breakpoint { addr } if addr == key.entry => {
                let tid = ins.t.current_thread()?;
                if tid == key.tid && ins.t.registers(tid)?.sp() == Some(key.cfa - 8) {
                    return Ok(true);
                }
            }
            StopReason::HistoryStart => return Ok(false),
            _ => {}
        }
    }
}

/// The tracepoint hits of frame instance `key` before `at`.
pub fn frame_hits(
    shared: &Arc<Shared>,
    at: Moment,
    key: &FrameKey,
    function: usize,
) -> Option<Arc<Vec<LineHit>>> {
    if shared.is_live() {
        // The cursor has traced every sidecar line since the process started.
        let log = lock(&shared.trace);
        return Some(Arc::new(ownership::frame_hits(
            &log, &at, key.tid, key.cfa, function,
        )));
    }
    if let Ok(ix) = &shared.index
        && let Some(h) = crate::indexed::frame_hits(shared, ix, at, key, function)
    {
        return Some(Arc::new(ownership::frame_hits(
            &h, &at, key.tid, key.cfa, function,
        )));
    }
    let cache_key = (at, key.tid, key.cfa);
    if let Some(h) = lock(&shared.replay_hits).get(&cache_key) {
        return h.clone();
    }
    let hits = replay_frame_hits(shared, at, key.clone(), function)
        .ok()
        .flatten();
    lock(&shared.replay_hits).insert(cache_key, hits.clone());
    hits
}

fn replay_frame_hits(
    shared: &Arc<Shared>,
    at: Moment,
    key: FrameKey,
    function: usize,
) -> Result<Option<Arc<Vec<LineHit>>>> {
    let lease = shared.pool.lease()?;
    let s = shared.clone();
    lease.run(move |t| -> Result<Option<Arc<Vec<LineHit>>>> {
        let mut spend = Spend::new(s.options.query_budget);
        if t.now().ok() != Some(at) {
            t.seek(&at)?;
        }
        let pc_at = t.registers(at.tid)?.pc().unwrap_or(0);
        if !reverse_to_entry(t, &key, &mut spend)? {
            return Ok(None);
        }
        let points: HashMap<u64, ownership::TracePoint> = s
            .sidecar_lines
            .iter()
            .filter(|(_, p)| p.function == function)
            .map(|(a, p)| (*a, *p))
            .collect();
        let syms = s.symbols_live(t)?;
        let mut hits = Vec::new();
        let mut ins = Inserted::new(t);
        for a in points.keys() {
            ins.add(Breakpoint::code(*a))?;
        }
        ins.add(Breakpoint::code(pc_at))?;
        // The entry itself is the first hit when the post-prologue tracepoint is the entry.
        let mut first = true;
        loop {
            if !first {
                match spend.resume(ins.t, Resume::Continue, Direction::Forward)? {
                    StopReason::Breakpoint { .. } => {}
                    StopReason::HistoryEnd | StopReason::Exited { .. } => break,
                    _ => continue,
                }
            }
            first = false;
            let tid = ins.t.current_thread()?;
            let regs = ins.t.registers(tid)?;
            let pc = regs.pc().unwrap_or(0);
            let now = ins.t.now()?;
            if !now.is_before(&at) {
                break;
            }
            let (Some(p), true) = (points.get(&pc), tid == key.tid) else {
                continue;
            };
            let cfa = {
                let mut mem = TargetMemory(&mut *ins.t);
                syms.set
                    .unwind(&mut mem as &mut dyn Memory, &regs)
                    .ok()
                    .and_then(|f| f.first().and_then(|f| f.cfa))
            };
            if cfa == Some(key.cfa) {
                hits.push(LineHit {
                    at: now,
                    tid,
                    cfa: key.cfa,
                    function,
                    mark: p.mark,
                });
            }
        }
        Ok(Some(Arc::new(hits)))
    })?
}

/// The processes of the recording.
pub fn processes(shared: &Arc<Shared>) -> Result<Vec<ProcessInfo>> {
    if let Some(p) = shared.processes.get() {
        return Ok(p.clone());
    }
    let threads = shared.pool.cursor(|t| t.threads())?.unwrap_or_default();
    let mut out = vec![ProcessInfo {
        id: shared.process,
        executable: shared.exe.display().to_string(),
        threads,
    }];
    if let Mode::Recording { trace_dir, rr } = &shared.mode
        && let Ok(rows) = rr.ps(trace_dir)
    {
        for r in rows {
            if r.pid != shared.process.pid {
                out.push(ProcessInfo {
                    id: hopper_core::ProcessId::new(r.pid),
                    executable: r.command,
                    threads: Vec::new(),
                });
            }
        }
    }
    // A live process's threads change; remember only a recording's.
    if !shared.is_live() {
        let _ = shared.processes.set(out.clone());
    }
    Ok(out)
}

/// The last moment of an rr recording, found once by replaying to the end.
pub fn end_moment(shared: &Arc<Shared>) -> Option<Moment> {
    *shared.end.get_or_init(|| {
        let lease = shared.pool.lease().ok()?;
        lease
            .run(|t| -> Result<Moment> {
                loop {
                    match t.resume(Resume::Continue, Direction::Forward)?.reason {
                        StopReason::HistoryEnd | StopReason::Exited { .. } => return t.now(),
                        _ => {}
                    }
                }
            })
            .ok()?
            .ok()
    })
}

/// The contents of a source file the debug information names. Paths under `/rustc/<hash>/`
/// are looked up in the toolchain's `rust-src` when it is installed.
pub fn source_text(file: &str) -> Result<Arc<str>> {
    let mut candidates = vec![PathBuf::from(file)];
    if let Some(rest) = file.strip_prefix("/rustc/")
        && let Some((_, inner)) = rest.split_once('/')
        && let Some(sysroot) = sysroot()
    {
        candidates.push(sysroot.join("lib/rustlib/src/rust").join(inner));
    }
    for c in candidates {
        if let Ok(text) = std::fs::read_to_string(&c) {
            return Ok(Arc::from(text));
        }
    }
    Err(HopperError::NotFound(format!("source {file}")))
}

fn sysroot() -> Option<PathBuf> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| {
        let out = std::process::Command::new("rustc")
            .args(["--print", "sysroot"])
            .output()
            .ok()?;
        let s = String::from_utf8(out.stdout).ok()?;
        Some(Path::new(s.trim()).to_path_buf())
    })
    .clone()
}

fn page_of<T: Clone>(items: &[T], page: PageReq, completeness: Completeness) -> Page<T> {
    let start = (page.offset as usize).min(items.len());
    let end = start.saturating_add(page.limit as usize).min(items.len());
    Page {
        items: items[start..end].to_vec(),
        total: Some(items.len() as u64),
        completeness,
    }
}

/// The line hits a live session has seen: the stops the cursor made on that line. The rest of
/// history is not known.
pub fn live_line_hits(
    session: &Session,
    loc: &SourceLoc,
    range: MomentRange,
    page: PageReq,
) -> Page<Moment> {
    let shared = session.shared();
    let st = lock(&shared.state);
    let items: Vec<Moment> = st
        .seen
        .iter()
        .filter(|(m, l)| {
            range.contains(m)
                && l.as_ref()
                    .is_some_and(|l| l.line == loc.line && l.file_matches(&loc.file))
        })
        .map(|(m, _)| *m)
        .collect();
    let pending = MomentRange {
        start: Some(st.stop.at),
        end: None,
    };
    page_of(
        &items,
        page,
        Completeness::Partial {
            pending: vec![pending],
        },
    )
}

/// Seek a fresh replica to the start of the recording.
pub(crate) fn to_start(shared: &Shared, t: &mut dyn Target) -> Result<()> {
    if t.now().ok() != Some(shared.start.at) {
        t.seek(&shared.start.at)?;
    }
    Ok(())
}

/// Every execution of a source line, by replaying the recording with breakpoints at its
/// addresses.
pub fn line_hits(
    shared: &Arc<Shared>,
    loc: SourceLoc,
    range: MomentRange,
    page: PageReq,
) -> Result<Page<Moment>> {
    if let Ok(ix) = &shared.index
        && let Some(p) = crate::indexed::complete(ix.index.line_hits(&loc, range, page))
    {
        return Ok(p);
    }
    let cached = lock(&shared.line_hit_cache).get(&loc).cloned();
    let all = match cached {
        Some(a) => a,
        None => {
            let lease = shared.pool.lease()?;
            let s = shared.clone();
            let l = loc.clone();
            let found = lease.run(move |t| -> Result<Vec<Moment>> {
                to_start(&s, t)?;
                // The line may be in a library loaded later; the main program's lines are
                // known from the start.
                let syms = s.symbols_live(t)?;
                let addrs: Vec<u64> = syms
                    .set
                    .addresses_for_line(&l.file, l.line)
                    .into_iter()
                    .filter(|a| a.location.line == l.line)
                    .map(|a| a.address)
                    .collect();
                if addrs.is_empty() {
                    return Err(HopperError::NotFound(format!("no code at {l}")));
                }
                let mut spend = Spend::new(s.options.query_budget);
                let mut ins = Inserted::new(t);
                for a in &addrs {
                    ins.add(Breakpoint::code(*a))?;
                }
                let mut out = Vec::new();
                loop {
                    match spend.resume(ins.t, Resume::Continue, Direction::Forward)? {
                        StopReason::Breakpoint { addr } if addrs.contains(&addr) => {
                            out.push(ins.t.now()?);
                        }
                        StopReason::HistoryEnd | StopReason::Exited { .. } => break,
                        _ => {}
                    }
                }
                Ok(out)
            })??;
            let a = Arc::new(found);
            lock(&shared.line_hit_cache).insert(loc, a.clone());
            a
        }
    };
    let items: Vec<Moment> = all.iter().filter(|m| range.contains(m)).copied().collect();
    Ok(page_of(&items, page, Completeness::Complete))
}

/// The calls of the functions a name matches, by replaying with breakpoints at their entries
/// and at each call's return address.
pub fn calls(shared: &Arc<Shared>, filter: CallFilter, page: PageReq) -> Result<Page<CallNode>> {
    if let Ok(ix) = &shared.index {
        let answer = ix.index.calls(&filter, page);
        if let Some(p) = crate::indexed::complete(answer.clone()) {
            return Ok(p);
        }
        // Still building: a named function can be replayed now; the tree waits for the index.
        if filter.function.is_none() {
            return answer;
        }
    }
    let Some(name) = filter.function.clone() else {
        return Err(HopperError::Unsupported(
            "the whole call tree comes from hopper-index; without it, give a function name".into(),
        ));
    };
    if let Some(p) = filter.process
        && p != shared.process
    {
        return Err(HopperError::NotFound(format!("process {}", p.pid)));
    }
    let lease = shared.pool.lease()?;
    let s = shared.clone();
    let (mut nodes, complete) = lease.run(move |t| -> Result<(Vec<CallNode>, bool)> {
        to_start(&s, t)?;
        let syms = s.symbols_live(t)?;
        let functions = syms.set.find_functions(&name);
        if functions.is_empty() {
            return Err(HopperError::NotFound(format!("function {name}")));
        }
        let entries: HashMap<u64, (String, Option<SourceLoc>)> = functions
            .iter()
            .map(|f| (f.entry, (f.name.clone(), f.decl.clone())))
            .collect();
        let mut spend = Spend::new(s.options.query_budget);
        let mut ins = Inserted::new(t);
        for a in entries.keys() {
            ins.add(Breakpoint::code(*a))?;
        }
        let mut nodes: Vec<CallNode> = Vec::new();
        // Calls that have not returned: (return address, stack pointer after return, node).
        let mut open: Vec<(u64, u64, usize)> = Vec::new();
        let complete = loop {
            let reason = match spend.resume(ins.t, Resume::Continue, Direction::Forward) {
                Ok(r) => r,
                Err(HopperError::BudgetExhausted { .. }) => break false,
                Err(e) => return Err(e),
            };
            let StopReason::Breakpoint { addr } = reason else {
                if matches!(reason, StopReason::HistoryEnd | StopReason::Exited { .. }) {
                    break true;
                }
                continue;
            };
            let tid = ins.t.current_thread()?;
            let sp = ins.t.registers(tid)?.sp().unwrap_or(0);
            if let Some(i) = open
                .iter()
                .rposition(|(ret, after, n)| *ret == addr && *after == sp && nodes[*n].tid == tid)
            {
                let (ret, _, n) = open.remove(i);
                nodes[n].exit = Some(ins.t.now()?);
                if !open.iter().any(|(r, _, _)| *r == ret) && !entries.contains_key(&ret) {
                    ins.remove(Breakpoint::code(ret));
                }
            }
            if let Some((fname, decl)) = entries.get(&addr) {
                let mut b = [0u8; 8];
                let ret = ins
                    .t
                    .read_memory(sp, &mut b)
                    .ok()
                    .map(|()| u64::from_le_bytes(b));
                let parent = open
                    .iter()
                    .rev()
                    .find(|(_, _, n)| nodes[*n].tid == tid)
                    .map(|(_, _, n)| *n);
                let depth = parent.map_or(0, |p| nodes[p].depth + 1);
                if let Some(p) = parent {
                    nodes[p].child_count += 1;
                }
                nodes.push(CallNode {
                    id: CallId(nodes.len() as u64),
                    parent: parent.map(|p| CallId(p as u64)),
                    tid,
                    function: fname.clone(),
                    entry: ins.t.now()?,
                    exit: None,
                    depth,
                    child_count: 0,
                    location: decl.clone(),
                });
                if let Some(ret) = ret {
                    ins.add(Breakpoint::code(ret))?;
                    open.push((ret, sp + 8, nodes.len() - 1));
                }
            }
        };
        Ok((nodes, complete))
    })??;
    if let Some(tid) = filter.tid {
        nodes.retain(|n| n.tid == tid);
    }
    if let Some(p) = filter.parent {
        nodes.retain(|n| n.parent == Some(p));
    }
    let completeness = if complete {
        Completeness::Complete
    } else {
        Completeness::Partial {
            pending: vec![MomentRange {
                start: nodes.last().map(|n| n.entry),
                end: None,
            }],
        }
    };
    Ok(page_of(&nodes, page, completeness))
}

/// Where a value handle's bytes are at its moment, their type, and the frame's identity.
struct Located {
    place: Place,
    ty: hopper_core::api::TypeKey,
    name: String,
    frame: Option<FrameKey>,
}

fn place_of(shared: &Arc<Shared>, h: &ValueHandle) -> Result<Located> {
    let handle = h.clone();
    let frame = Session::handle_frame(h);
    at_moment(shared, h.at, move |v| {
        let ev = v.with_eval(frame, |ev, cx| ev.evaluate(cx, &handle))?;
        let place = ev.place.ok_or_else(|| {
            HopperError::Unsupported("the value is not in memory (a register or a constant)".into())
        })?;
        let name = v.with_eval(frame, |ev, cx| ev.view(cx, &handle))?.name;
        let f = v.frame(frame)?;
        let syms = v.symbols()?;
        Ok(Located {
            place,
            ty: ev.type_key,
            name,
            frame: FrameKey::of(&syms.set, &f),
        })
    })
}

/// The last write to `place` before `before`.
pub fn last_write(
    shared: &Arc<Shared>,
    place: Place,
    before: Moment,
) -> Result<Option<WriteEvent>> {
    shared.check_moment(&before)?;
    crate::writes::request(shared, place, before);
    // The phase-B write log, where it covers the range and recorded the bytes.
    if let (Ok(ix), Some(main)) = (&shared.index, &shared.main_symbols)
        && let Ok(answer) = ix.index.last_write(place, before)
        && answer.completeness == Completeness::Complete
    {
        match answer.value {
            None => return Ok(None),
            Some(w) => {
                if let Some(ev) = w.to_event(main)
                    && let Some(ev) = narrowed(ev, place)
                {
                    return Ok(Some(ev));
                }
            }
        }
    }
    if shared.is_live() {
        let window = live_window(shared, before)?;
        let mut t = StepReplica::new(window.clone());
        t.seek(&window.end())?;
        let mut r = Reverse {
            t: &mut t,
            shared,
            steps: 0,
            budget: shared.options.query_budget,
        };
        return Ok(r.last_write(place)?.ok().map(|w| w.event));
    }
    let lease = shared.pool.lease()?;
    let s = shared.clone();
    lease.run(move |t| -> Result<Option<WriteEvent>> {
        if t.now().ok() != Some(before) {
            t.seek(&before)?;
        }
        let mut r = Reverse {
            t,
            shared: &s,
            steps: 0,
            budget: s.options.query_budget,
        };
        Ok(r.last_write(place)?.ok().map(|w| w.event))
    })?
}

/// A logged write as the answer about `place`: the place asked, and the bytes the write left in
/// it (a 16-byte store over a 4-byte field reports the field's 4). `None` when the write did not
/// cover the whole place (its other bytes are older): reverse execution answers then.
fn narrowed(ev: WriteEvent, place: Place) -> Option<WriteEvent> {
    let (Place::Memory { addr: wa, .. }, Place::Memory { addr, len, .. }) = (ev.place, place)
    else {
        return None;
    };
    let from = addr.checked_sub(wa)? as usize;
    let bytes = ev.value.get(from..from + len as usize)?.to_vec();
    Some(WriteEvent {
        place,
        value: bytes,
        ..ev
    })
}

/// The origin chain of a value, with the writes of the references it was reached through.
pub fn origin(shared: &Arc<Shared>, value: &ValueHandle, budget: Budget) -> Result<OriginChain> {
    let place = place_of(shared, value)?.place;
    crate::writes::request(shared, place, value.at);
    // The pointers on the way to the value, nearest first; one that is not in memory (a
    // register) has no write to find.
    let pointers: Vec<Place> = references_of(value)
        .iter()
        .filter_map(|h| place_of(shared, h).ok().map(|l| l.place))
        .filter(|p| matches!(p, Place::Memory { .. }))
        .collect();
    let at = value.at;
    if shared.is_live() {
        // A live process cannot reverse: a stepped window of another run, ending in the same
        // state, can.
        let window = live_window(shared, at)?;
        let mut t = StepReplica::new(window.clone());
        return origin_on(&mut t, shared, window.end(), place, &pointers, budget);
    }
    let lease = shared.pool.lease()?;
    let s = shared.clone();
    lease.run(move |t| origin_on(t, &s, at, place, &pointers, budget))?
}

/// The origin chain of `place` on a replica that can seek to `at` and reverse from there.
fn origin_on(
    t: &mut dyn Target,
    shared: &Shared,
    at: Moment,
    place: Place,
    pointers: &[Place],
    budget: Budget,
) -> Result<OriginChain> {
    if t.now().ok() != Some(at) {
        t.seek(&at)?;
    }
    let mut r = Reverse {
        t,
        shared,
        steps: 0,
        budget: budget.max_steps,
    };
    let mut chain = r.origin(place, budget)?;
    for p in pointers {
        if r.t.now().ok() != Some(at) {
            r.t.seek(&at)?;
        }
        r.steps = 0;
        match r.reference_hop(*p) {
            Ok(Some(hop)) => chain.references.push(hop),
            Ok(None) | Err(HopperError::BudgetExhausted { .. }) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(chain)
}

/// Most instructions a live session's stepped window may take.
const LIVE_WINDOW_STEPS: usize = 2_000_000;

/// A live session's stepped window ending in the state the process had at `at` (see
/// [`crate::stepped`]), made once per moment. It starts at the entry of the stopped thread's
/// outermost frame in the user's own source (the program's `main`, normally), so history
/// before that is not in it.
pub fn live_window(shared: &Arc<Shared>, at: Moment) -> Result<Arc<StepRecording>> {
    if let Some(w) = lock(&shared.live_windows).get(&at) {
        return Ok(w.clone());
    }
    let Mode::Live { program, args } = &shared.mode else {
        return Err(HopperError::Unsupported(
            "stepped windows are for live sessions".into(),
        ));
    };
    let s = shared.clone();
    let (regs, entry, cfa) = at_moment(shared, at, move |v| {
        let stack = v.stack(at.tid)?;
        let regs = v.registers(at.tid)?;
        let syms = v.symbols()?;
        // The user's own code: a sidecar record of origin `Source` when there is a sidecar;
        // otherwise a Rust function (a path: rustc's C `main` shim is not one) whose file is not
        // std's (`/rustc/…`, or the toolchain's `rustlib/src` for generics instantiated in the
        // user's crate) or a registry crate's.
        let sidecar = s.sidecar.clone();
        let rust_path = |f: &StackFrame| {
            f.frame
                .function
                .as_deref()
                .is_some_and(|n| n.contains("::"))
        };
        let user_source = |f: &&StackFrame| match &sidecar {
            Some(sc) => {
                rust_path(f)
                    && syms
                        .set
                        .function_at(f.lookup_pc)
                        .and_then(|func| sc.function_named(&func.linkage_name.unwrap_or(func.name)))
                        .is_some_and(|i| {
                            sc.function(i).origin == hopper_sidecar::FunctionOrigin::Source
                        })
            }
            None => {
                rust_path(f)
                    && f.frame.location.as_ref().is_some_and(|l| {
                        !l.file.starts_with("/rustc/")
                            && !l.file.contains("/rustlib/src/")
                            && !l.file.contains("/.cargo/registry/")
                    })
            }
        };
        let frame = stack
            .iter()
            .rev()
            .find(user_source)
            .or(stack.first())
            .ok_or_else(|| HopperError::NotFound("a frame to start the window at".into()))?;
        let key = FrameKey::of(&syms.set, frame).ok_or_else(|| {
            HopperError::NotFound("the outermost user frame's function and CFA".into())
        })?;
        Ok((regs, key.entry, key.cfa))
    })?;
    let window = Arc::new(StepRecording::capture(
        &shared.options.gdbserver,
        program,
        args,
        WindowStart::FrameEntry { entry, cfa },
        &WindowEnd::Registers(regs),
        LIVE_WINDOW_STEPS,
    )?);
    lock(&shared.live_windows).insert(at, window.clone());
    Ok(window)
}

/// The handles of the pointers a value is reached through, nearest the value first: the
/// prefixes of its path that a `Deref` follows, then the pointers its expression dereferences
/// (`n` for `*n` and `(*n).vec[0]`).
fn references_of(value: &ValueHandle) -> Vec<ValueHandle> {
    let mut out = Vec::new();
    for (i, e) in value.path.iter().enumerate().rev() {
        if *e == hopper_core::api::PathElem::Deref {
            out.push(ValueHandle {
                path: value.path[..i].to_vec(),
                ..value.clone()
            });
        }
    }
    if let ValueTarget::Expr(src) = &value.target
        && let Ok(expr) = hopper_eval::expr::parse(src)
    {
        for p in expr.dereferenced() {
            out.push(ValueHandle {
                target: ValueTarget::Expr(p.to_string()),
                path: Vec::new(),
                ..value.clone()
            });
        }
    }
    out
}

/// How a value changed over a range: its value at the frame's entry (or the range's start)
/// and after every write to its bytes, until the frame returns or the range ends.
///
/// The value is read by place at each moment (its bytes, with its type): a write can happen
/// where the variable's name is not in scope yet, as in the instructions of `let b = a;`.
pub fn value_history(
    shared: &Arc<Shared>,
    value: &ValueHandle,
    range: MomentRange,
    page: PageReq,
) -> Result<Page<(Moment, ValueView)>> {
    let Located {
        place,
        ty,
        name,
        frame: key,
    } = place_of(shared, value)?;
    let Place::Memory { addr, len, .. } = place else {
        return Err(HopperError::Unsupported("history of a register".into()));
    };
    crate::writes::request(shared, place, value.at);
    let (pieces, whole) = watch_pieces(addr, len.min(32));
    let ask = HistoryAsk {
        place,
        ty,
        name,
        key,
        range,
        pieces,
    };
    let (items, complete) = if shared.is_live() {
        // A live process cannot go back to the frame's entry: a stepped window of another run,
        // ending in the same state, can. Its thread has its own id.
        let window = live_window(shared, value.at)?;
        let mut t = StepReplica::new(window.clone());
        let tid = t.current_thread()?;
        let ask = HistoryAsk {
            key: ask.key.map(|k| FrameKey { tid, ..k }),
            range: MomentRange::all(),
            ..ask
        };
        let h = ValueHandle {
            at: window.end(),
            ..value.clone()
        };
        history_on(&mut t, shared, &h, &ask)?
    } else {
        let lease = shared.pool.lease()?;
        let s = shared.clone();
        let h = value.clone();
        lease.run(move |t| history_on(t, &s, &h, &ask))??
    };
    let completeness = if complete && whole {
        Completeness::Complete
    } else {
        Completeness::Partial {
            pending: vec![MomentRange {
                start: items.last().map(|(m, _)| *m),
                end: range.end,
            }],
        }
    };
    Ok(page_of(&items, page, completeness))
}

/// What `value_history` asks a replica.
struct HistoryAsk {
    place: Place,
    ty: hopper_core::api::TypeKey,
    name: String,
    key: Option<FrameKey>,
    range: MomentRange,
    pieces: Vec<(u64, u64)>,
}

/// A value's history on a replica that can seek to `h.at` and reverse: its value at the frame's
/// entry (or the range's start) and after every write until the frame returns or the range ends;
/// whether the replay finished within the budget.
fn history_on(
    t: &mut dyn Target,
    s: &Shared,
    h: &ValueHandle,
    ask: &HistoryAsk,
) -> Result<(Vec<(Moment, ValueView)>, bool)> {
    let HistoryAsk {
        place,
        ty,
        name,
        key,
        range,
        pieces,
    } = ask;
    let (place, ty, range) = (*place, *ty, *range);
    let mut spend = Spend::new(s.options.query_budget);
    if t.now().ok() != Some(h.at) {
        t.seek(&h.at)?;
    }
    // The caller's return address ends a local's history.
    let ret = match &key {
        Some(k) => {
            let mut v = MomentView::on(s, h.at, &mut *t);
            let stack = v.stack(k.tid)?;
            stack
                .iter()
                .find(|f| f.cfa.is_some_and(|c| c > k.cfa))
                .map(|f| f.frame.pc)
        }
        None => None,
    };
    match (&key, range.start) {
        (_, Some(start)) => {
            t.seek(&start)?;
        }
        (Some(k), None) => {
            if !reverse_to_entry(t, k, &mut spend)? {
                to_start(s, t)?;
            }
        }
        (None, None) => to_start(s, t)?,
    }
    let view_here = |t: &mut dyn Target| -> Option<(Moment, ValueView)> {
        let at = t.now().ok()?;
        let mut v = MomentView::on(s, at, t);
        let frame = FrameId {
            tid: h.at.tid,
            depth: 0,
        };
        let handle = ValueHandle {
            at,
            process: h.process,
            frame: None,
            target: ValueTarget::Place { place, ty },
            path: Vec::new(),
        };
        let mut view = v.with_eval(frame, |ev, cx| ev.view(cx, &handle)).ok()?;
        view.name = name.clone();
        Some((at, view))
    };
    let mut items: Vec<(Moment, ValueView)> = Vec::new();
    let push = |items: &mut Vec<(Moment, ValueView)>, x: Option<(Moment, ValueView)>| {
        if let Some((m, v)) = x
            && items.last().is_none_or(|(_, p)| p.summary != v.summary)
        {
            items.push((m, v));
        }
    };
    push(&mut items, view_here(t));
    let mut ins = Inserted::new(t);
    for (a, l) in pieces {
        ins.add(Breakpoint {
            kind: BreakKind::WriteWatch,
            addr: *a,
            len: *l,
        })?;
    }
    if let Some(r) = ret {
        ins.add(Breakpoint::code(r))?;
    }
    let complete = loop {
        let reason = match spend.resume(ins.t, Resume::Continue, Direction::Forward) {
            Ok(r) => r,
            Err(HopperError::BudgetExhausted { .. }) => break false,
            Err(e) => return Err(e),
        };
        match reason {
            StopReason::Watchpoint { .. } => {
                let now = ins.t.now()?;
                if range.end.is_some_and(|e| !now.is_before(&e)) {
                    break true;
                }
                let x = view_here(ins.t);
                push(&mut items, x);
            }
            StopReason::Breakpoint { addr } if Some(addr) == ret => {
                let back_in_caller = match &key {
                    Some(k) => {
                        let tid = ins.t.current_thread()?;
                        tid == k.tid && ins.t.registers(tid)?.sp() == Some(k.cfa)
                    }
                    None => true,
                };
                if back_in_caller {
                    break true;
                }
            }
            StopReason::HistoryEnd | StopReason::Exited { .. } => break true,
            _ => {}
        }
    };
    Ok((items, complete))
}
