//! A deterministic, in-memory [`Debugger`] over a scripted recording of
//! `fixtures/worked-example`, for building and testing front ends (the GUI, the CLI, MCP and DAP)
//! without rr.
//!
//! The fake behaves like the engine will:
//! - Navigation (`step`, `run`, `seek`, `back`, `forward`) moves one cursor and records
//!   navigation history; every query takes an explicit [`Moment`] and never moves the cursor.
//! - Values are typed and laid out as rustc lays them out, so `memory`, `last_write`, `locals`
//!   and `value_history` agree with each other. `a` is reported `OwnershipState::MovedOut` from
//!   the move at line 13 on.
//! - `origin` returns the plan's six-hop, ownership-labelled chain for `n`/`*n` at line 17-18
//!   (and its suffixes for `v`, `c`, `b`, `a` and `name`), with allocation ids numbered in the
//!   order the program allocates: #1 `"alpha"` in `String::from`, #2 the clone's buffer, #3 the
//!   Vec's buffer.
//! - Test controls ([`FakeConfig`]): a latency added to every call, and an index that completes
//!   over a number of `index_status` polls or a wall-clock duration. While it is incomplete,
//!   `calls`, `line_hits` and `value_history` cover only the indexed prefix of the recording and
//!   say so with [`Completeness::Partial`].
//!
//! The scripted machine code is not a real x86 encoding: `memory` and `disassemble` agree on
//! its bytes, but the bytes do not decode.

mod script;
mod values;

use std::cmp::Ordering;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use hopper_core::api::{
    AllocationInfo, BreakLocation, BreakpointId, Budget, CallFilter, CallNode, Completeness,
    Debugger, Frame, IndexStatus, Instruction, OriginChain, OriginEnd, OriginHop, Page, PageReq,
    PathElem, ProcessInfo, RecordingInfo, StepKind, Stop, StopCause, TimelineEvent, Until,
    UserBreakpoint, ValueHandle, ValueTarget, ValueView, WriteEvent,
};
use hopper_core::target::Direction;
use hopper_core::{
    FrameId, HopperError, Moment, MomentRange, Place, ProcessId, Reg, RegisterSet, Result,
    SourceLoc, Tid,
};

pub use script::{EXECUTABLE, MAIN_RS, PID, RECORDING, TID};

use script::{FrameSnap, Script, StepRec};
use values::{Prefix, Resolved, parse_expr};

/// The fixture's source, as the fake serves it from [`Debugger::source_text`].
pub const WORKED_EXAMPLE_SOURCE: &str =
    include_str!("../../../fixtures/worked-example/src/main.rs");

/// How the fake's background index progresses.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub enum IndexSchedule {
    /// The index is complete from the start.
    #[default]
    Complete,
    /// The index completes after this many `index_status` calls (deterministic, for tests).
    AfterPolls(u32),
    /// The index completes this long after the fake was created.
    AfterDuration(Duration),
}

/// Test controls.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default)]
pub struct FakeConfig {
    /// Added to every call except `index_status`, to exercise front ends' job handling.
    pub latency: Option<Duration>,
    pub index: IndexSchedule,
}

#[derive(Debug)]
struct State {
    cursor: usize,
    cause: StopCause,
    back: Vec<usize>,
    forward: Vec<usize>,
    breakpoints: Vec<UserBreakpoint>,
    next_breakpoint: u32,
    polls: u32,
}

/// The fake engine. Cheap to create; internally synchronised.
#[derive(Debug)]
pub struct FakeDebugger {
    script: Script,
    config: FakeConfig,
    created: Instant,
    state: Mutex<State>,
}

/// The index phases, in the order the fake completes them.
const PHASES: [&str; 4] = ["calls", "lines", "allocations", "values"];
const PHASE_CALLS: usize = 0;
const PHASE_LINES: usize = 1;
const PHASE_VALUES: usize = 3;

impl Default for FakeDebugger {
    fn default() -> Self {
        FakeDebugger::new()
    }
}

impl FakeDebugger {
    /// A fake with no latency and a complete index.
    pub fn new() -> Self {
        FakeDebugger::with_config(FakeConfig::default())
    }

    pub fn with_config(config: FakeConfig) -> Self {
        FakeDebugger {
            script: script::worked_example(),
            config,
            created: Instant::now(),
            state: Mutex::new(State {
                cursor: 0,
                cause: StopCause::HistoryStart,
                back: Vec::new(),
                forward: Vec::new(),
                breakpoints: Vec::new(),
                next_breakpoint: 1,
                polls: 0,
            }),
        }
    }

    /// Convenience: the fake as the trait object front ends take.
    pub fn shared() -> Arc<dyn Debugger> {
        Arc::new(FakeDebugger::new())
    }

    /// Every moment the fake can stop at, in order.
    pub fn moments(&self) -> Vec<Moment> {
        self.script.steps.iter().map(|s| s.moment).collect()
    }

    /// The first moment at which `line` of `main.rs` starts executing in `function` (a substring
    /// of the function name), if any.
    pub fn first_moment_at(&self, function: &str, line: u32) -> Option<Moment> {
        self.script
            .steps
            .iter()
            .find(|s| s.top().line == line && s.top().function.contains(function))
            .map(|s| s.moment)
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn delay(&self) {
        if let Some(d) = self.config.latency {
            std::thread::sleep(d);
        }
    }

    fn steps(&self) -> &[StepRec] {
        &self.script.steps
    }

    fn last(&self) -> usize {
        self.script.steps.len() - 1
    }

    /// The index of the latest step at or before `m`.
    fn index_at(&self, m: &Moment) -> Result<usize> {
        if m.recording != RECORDING {
            return Err(HopperError::BadMoment(format!(
                "moment {m} belongs to recording {}, not {RECORDING}",
                m.recording
            )));
        }
        let key = m.order_key();
        let after = self
            .steps()
            .partition_point(|s| s.moment.order_key() <= key);
        if after == 0 {
            return Err(HopperError::BadMoment(format!(
                "moment {m} is before the recording"
            )));
        }
        Ok(after - 1)
    }

    fn check_process(&self, process: ProcessId) -> Result<()> {
        if process != self.script.process() {
            return Err(HopperError::NotFound(format!("process {}", process.pid)));
        }
        Ok(())
    }

    fn check_tid(&self, tid: Tid) -> Result<()> {
        if tid != TID {
            return Err(HopperError::NotFound(format!("thread {tid}")));
        }
        Ok(())
    }

    // ------------------------------------------------------------ index progress

    fn progress(&self, polls: u32) -> f32 {
        match self.config.index {
            IndexSchedule::Complete => 1.0,
            IndexSchedule::AfterPolls(0) => 1.0,
            IndexSchedule::AfterPolls(n) => (polls as f32 / n as f32).min(1.0),
            IndexSchedule::AfterDuration(d) if d.is_zero() => 1.0,
            IndexSchedule::AfterDuration(d) => {
                (self.created.elapsed().as_secs_f32() / d.as_secs_f32()).min(1.0)
            }
        }
    }

    fn phase_progress(&self, polls: u32, phase: usize) -> f32 {
        (self.progress(polls) * PHASES.len() as f32 - phase as f32).clamp(0.0, 1.0)
    }

    /// How many steps (from the start) the given phase has indexed, and the completeness to
    /// report for it.
    fn coverage(&self, phase: usize) -> (usize, Completeness) {
        let polls = self.lock().polls;
        let p = self.phase_progress(polls, phase);
        let n = self.steps().len();
        let covered = ((p * n as f32).floor() as usize).min(n);
        if covered == n {
            (n, Completeness::Complete)
        } else {
            let pending = MomentRange {
                start: Some(self.steps()[covered].moment),
                end: None,
            };
            (
                covered,
                Completeness::Partial {
                    pending: vec![pending],
                },
            )
        }
    }

    // ------------------------------------------------------------ navigation

    fn stop_at(&self, index: usize, cause: StopCause) -> Stop {
        let s = &self.steps()[index];
        Stop {
            at: s.moment,
            tid: TID,
            pc: s.pc,
            cause,
            function: Some(s.top().function.clone()),
            location: Some(s.top().location()),
        }
    }

    /// Move the cursor, recording navigation history when it actually moves.
    fn move_to(&self, index: usize, cause: StopCause) -> Stop {
        let mut st = self.lock();
        if index != st.cursor {
            let from = st.cursor;
            st.back.push(from);
            st.forward.clear();
            st.cursor = index;
        }
        st.cause = cause.clone();
        drop(st);
        self.stop_at(index, cause)
    }

    /// The first step of the source line `index` is on, skipping deeper calls made by that line.
    fn line_start(&self, index: usize) -> usize {
        let steps = self.steps();
        let key = steps[index].line_key();
        let depth = steps[index].depth();
        let mut best = index;
        let mut k = index;
        while k > 0 {
            let p = &steps[k - 1];
            if p.depth() > depth {
                k -= 1;
            } else if p.line_key() == key {
                k -= 1;
                best = k;
            } else {
                break;
            }
        }
        best
    }

    /// The step a `step` command lands on; `None` at the end of history in that direction.
    fn step_target(&self, from: usize, kind: StepKind, dir: Direction) -> Option<usize> {
        let steps = self.steps();
        let here = &steps[from];
        let depth = here.depth();
        match (kind, dir) {
            (StepKind::Instruction, Direction::Forward) => (from < self.last()).then_some(from + 1),
            (StepKind::Instruction, Direction::Reverse) => from.checked_sub(1),
            (StepKind::Into, Direction::Forward) => {
                (from + 1..steps.len()).find(|&j| steps[j].line_key() != here.line_key())
            }
            (StepKind::Over, Direction::Forward) => (from + 1..steps.len())
                .find(|&j| steps[j].depth() <= depth && steps[j].line_key() != here.line_key()),
            (StepKind::Out, Direction::Forward) => {
                (from + 1..steps.len()).find(|&j| steps[j].depth() < depth)
            }
            (StepKind::Into, Direction::Reverse) => {
                let start = self.line_start(from);
                if start < from && steps[start].depth() == depth {
                    return Some(start);
                }
                let prev = from.checked_sub(1)?;
                Some(self.line_start_any(prev))
            }
            (StepKind::Over, Direction::Reverse) => {
                let start = self.line_start(from);
                if start < from {
                    return Some(start);
                }
                let prev = (0..start).rev().find(|&j| steps[j].depth() <= depth)?;
                Some(self.line_start(prev))
            }
            (StepKind::Out, Direction::Reverse) => {
                let call = here.top().call;
                let entry = steps.iter().position(|s| s.top().call == call)?;
                entry.checked_sub(1)
            }
        }
    }

    /// The first step of the contiguous run of `index`'s (call, line) at its own depth.
    fn line_start_any(&self, index: usize) -> usize {
        let steps = self.steps();
        let key = steps[index].line_key();
        let mut k = index;
        while k > 0 && steps[k - 1].line_key() == key {
            k -= 1;
        }
        k
    }

    /// The breakpoint that stops execution at `index`, if any.
    fn breakpoint_at(&self, bps: &[UserBreakpoint], index: usize) -> Option<BreakpointId> {
        let s = &self.steps()[index];
        bps.iter()
            .find(|b| b.enabled && b.addresses.contains(&s.pc))
            .map(|b| b.id)
    }

    /// Addresses a breakpoint location resolves to.
    fn resolve(&self, location: &BreakLocation) -> Vec<u64> {
        let steps = self.steps();
        let mut out: Vec<u64> = match location {
            BreakLocation::Address(a) => vec![*a],
            BreakLocation::Function(name) => self
                .script
                .calls
                .iter()
                .filter(|c| function_matches(&c.function, name))
                .filter_map(|c| steps.iter().find(|s| s.moment == c.entry).map(|s| s.pc))
                .collect(),
            BreakLocation::Source(loc) => (0..steps.len())
                .filter(|&j| {
                    let f = steps[j].top();
                    f.line == loc.line
                        && SourceLoc::new(f.file.clone(), f.line).file_matches(&loc.file)
                        && self.line_start(j) == j
                })
                .map(|j| steps[j].pc)
                .collect(),
        };
        out.sort_unstable();
        out.dedup();
        out
    }

    // ------------------------------------------------------------ values

    fn frame_at(&self, index: usize, frame: FrameId) -> Result<&FrameSnap> {
        self.check_tid(frame.tid)?;
        self.steps()[index]
            .frames
            .get(frame.depth as usize)
            .ok_or_else(|| {
                HopperError::NotFound(format!(
                    "frame #{} at {}",
                    frame.depth,
                    self.steps()[index].moment
                ))
            })
    }

    fn eval_in(&self, frame: &FrameSnap, expr: &str) -> Result<Resolved> {
        let parsed = parse_expr(expr)?;
        let local = frame
            .locals
            .iter()
            .find(|l| l.name == parsed.root)
            .ok_or_else(|| {
                HopperError::Eval(format!(
                    "no variable `{}` in {}",
                    parsed.root, frame.function
                ))
            })?;
        let mut r = Resolved {
            name: local.name.clone(),
            val: local.value.clone(),
            ty: local.value.type_name(&local.ty),
            addr: Some(local.addr),
            size: local.size,
            ownership: local.ownership.clone(),
        };
        for elem in &parsed.path {
            r = r.project(elem)?;
        }
        for prefix in parsed.prefixes.iter().rev() {
            r = match prefix {
                Prefix::Deref => r.project(&PathElem::Deref)?,
                Prefix::Borrow => r.borrow()?,
            };
        }
        r.name = expr.trim().to_string();
        Ok(r)
    }

    /// Evaluate a handle's target and path.
    fn resolve_handle(&self, h: &ValueHandle) -> Result<Resolved> {
        self.check_process(h.process)?;
        let index = self.index_at(&h.at)?;
        let frame_id = h.frame.unwrap_or(FrameId { tid: TID, depth: 0 });
        let mut r = match &h.target {
            ValueTarget::Expr(e) => self.eval_in(self.frame_at(index, frame_id)?, e)?,
            ValueTarget::Place { place, .. } => self.local_at_place(index, place)?,
        };
        for elem in &h.path {
            r = r.project(elem)?;
        }
        Ok(r)
    }

    fn local_at_place(&self, index: usize, place: &Place) -> Result<Resolved> {
        let Place::Memory { addr, .. } = place else {
            return Err(HopperError::Unsupported(
                "register places in the fake".into(),
            ));
        };
        for frame in &self.steps()[index].frames {
            if let Some(l) = frame.locals.iter().find(|l| l.addr == *addr) {
                return self.eval_in(frame, &l.name);
            }
        }
        Err(HopperError::Eval(format!("no variable at {addr:#x}")))
    }

    fn view_of(&self, handle: ValueHandle, r: Resolved) -> ValueView {
        ValueView {
            name: r.name.clone(),
            type_name: r.ty.clone(),
            summary: r.val.summary(),
            child_count: r.val.child_count(),
            place: r
                .addr
                .map(|a| Place::memory(self.script.process(), a, r.size)),
            ownership: r.ownership.clone(),
            handle,
        }
    }

    // ------------------------------------------------------------ memory

    fn read_bytes(&self, index: usize, addr: u64, len: u64) -> Result<Vec<u8>> {
        let end = addr
            .checked_add(len)
            .ok_or(HopperError::UnreadableMemory { addr, len })?;
        let region = [script::TEXT, script::RODATA, script::HEAP, script::STACK]
            .into_iter()
            .find(|(lo, hi)| addr >= *lo && end <= *hi)
            .ok_or(HopperError::UnreadableMemory { addr, len })?;
        let mut out = vec![0u8; len as usize];
        if region == script::TEXT {
            out.fill(0xcc);
            for ins in &self.script.instructions {
                copy_overlap(&mut out, addr, ins.addr, &ins.bytes);
            }
        } else if region == script::RODATA {
            let image = script::rodata_image();
            let off = (addr - region.0) as usize;
            out.copy_from_slice(&image[off..off + len as usize]);
        }
        for s in &self.steps()[..=index] {
            for w in &s.writes {
                if let Place::Memory { addr: wa, .. } = w.place {
                    copy_overlap(&mut out, addr, wa, &w.value);
                }
            }
        }
        Ok(out)
    }

    fn registers_at(&self, index: usize) -> RegisterSet {
        let s = &self.steps()[index];
        let top = s.top();
        let mut rs = RegisterSet::default();
        let sp = top.cfa - 0x60 - 8 * index as u64 % 0x20;
        let seed = index as u64;
        for n in 0u16..=15 {
            let v = match Reg(n) {
                Reg::RSP => sp,
                Reg::RBP => top.cfa - 0x10,
                Reg::RDI => s.frames.last().map(|f| f.cfa - 0x160).unwrap_or(0),
                _ => {
                    (seed + 1)
                        .wrapping_mul(0x0100_0000_01b3)
                        .wrapping_add(n as u64 * 0x11)
                        & 0xffff_ffff
                }
            };
            rs.set(Reg(n), v);
        }
        rs.set(Reg::RIP, s.pc);
        rs.set(Reg::RFLAGS, 0x246);
        rs.set(Reg::CS, 0x33);
        rs.set(Reg::SS, 0x2b);
        rs.raw = rs
            .values
            .iter()
            .flat_map(|(_, b)| b.iter().copied())
            .collect();
        rs
    }
}

/// Copy the part of `src` (which starts at `src_addr`) that overlaps `out` (which starts at
/// `out_addr`).
fn copy_overlap(out: &mut [u8], out_addr: u64, src_addr: u64, src: &[u8]) {
    let lo = out_addr.max(src_addr);
    let hi = (out_addr + out.len() as u64).min(src_addr + src.len() as u64);
    if lo >= hi {
        return;
    }
    let n = (hi - lo) as usize;
    let o = (lo - out_addr) as usize;
    let s = (lo - src_addr) as usize;
    out[o..o + n].copy_from_slice(&src[s..s + n]);
}

/// A function query matches by exact name, by final path segments (`load`, `Vec::push`) or by
/// substring.
fn function_matches(function: &str, query: &str) -> bool {
    function == query || function.ends_with(&format!("::{query}")) || function.contains(query)
}

fn page<T: Clone>(items: &[T], req: PageReq, completeness: Completeness) -> Page<T> {
    let start = (req.offset as usize).min(items.len());
    let end = start.saturating_add(req.limit as usize).min(items.len());
    Page {
        items: items[start..end].to_vec(),
        total: Some(items.len() as u64),
        completeness,
    }
}

impl Debugger for FakeDebugger {
    fn recording(&self) -> Result<RecordingInfo> {
        self.delay();
        Ok(RecordingInfo {
            id: RECORDING,
            path: "fake:worked-example".into(),
            rr_version: Some("5.9.0 (fake)".into()),
            start: Some(self.steps()[0].moment),
            end: Some(self.steps()[self.last()].moment),
            processes: vec![ProcessInfo {
                id: self.script.process(),
                executable: EXECUTABLE.into(),
                threads: vec![TID],
            }],
            reference_run: None,
        })
    }

    fn timeline(&self, range: MomentRange, req: PageReq) -> Result<Page<TimelineEvent>> {
        self.delay();
        let items: Vec<TimelineEvent> = self
            .script
            .events
            .iter()
            .filter(|e| range.contains(&e.at))
            .cloned()
            .collect();
        Ok(page(&items, req, Completeness::Complete))
    }

    fn index_status(&self) -> IndexStatus {
        let polls = {
            let mut st = self.lock();
            st.polls = st.polls.saturating_add(1);
            st.polls
        };
        let phases: Vec<(String, f32)> = PHASES
            .iter()
            .enumerate()
            .map(|(i, name)| (name.to_string(), self.phase_progress(polls, i)))
            .collect();
        let done = phases.iter().all(|(_, p)| *p >= 1.0);
        IndexStatus {
            phases,
            done,
            error: None,
        }
    }

    fn cursor(&self) -> Result<Stop> {
        self.delay();
        let st = self.lock();
        let (i, cause) = (st.cursor, st.cause.clone());
        drop(st);
        Ok(self.stop_at(i, cause))
    }

    fn step(&self, kind: StepKind, dir: Direction) -> Result<Stop> {
        self.delay();
        let from = self.lock().cursor;
        Ok(match self.step_target(from, kind, dir) {
            Some(to) => self.move_to(to, StopCause::Step),
            None => match dir {
                Direction::Forward => self.move_to(self.last(), StopCause::HistoryEnd),
                Direction::Reverse => self.move_to(0, StopCause::HistoryStart),
            },
        })
    }

    fn run(&self, dir: Direction, until: Until) -> Result<Stop> {
        self.delay();
        let (from, bps) = {
            let st = self.lock();
            (st.cursor, st.breakpoints.clone())
        };
        let end = || match dir {
            Direction::Forward => (self.last(), StopCause::HistoryEnd),
            Direction::Reverse => (0, StopCause::HistoryStart),
        };
        let candidates: Box<dyn Iterator<Item = usize>> = match dir {
            Direction::Forward => Box::new(from + 1..self.steps().len()),
            Direction::Reverse => Box::new((0..from).rev()),
        };
        let (to, cause) = match until {
            Until::EndOfTrace => end(),
            Until::Moment(m) => (self.index_at(&m)?, StopCause::Jump),
            Until::Breakpoints => {
                let mut found = None;
                for j in candidates {
                    if let Some(id) = self.breakpoint_at(&bps, j) {
                        found = Some((j, StopCause::Breakpoint { id }));
                        break;
                    }
                }
                found.unwrap_or_else(end)
            }
            Until::Watch(place) => {
                let mut found = None;
                for j in candidates {
                    if self.steps()[j]
                        .writes
                        .iter()
                        .any(|w| w.place.overlaps(&place))
                    {
                        found = Some((j, StopCause::Watchpoint { place }));
                        break;
                    }
                }
                found.unwrap_or_else(end)
            }
        };
        Ok(self.move_to(to, cause))
    }

    fn seek(&self, to: Moment) -> Result<Stop> {
        self.delay();
        let index = self.index_at(&to)?;
        Ok(self.move_to(index, StopCause::Jump))
    }

    fn back(&self) -> Result<Option<Stop>> {
        self.delay();
        let mut st = self.lock();
        let Some(to) = st.back.pop() else {
            return Ok(None);
        };
        let from = st.cursor;
        st.forward.push(from);
        st.cursor = to;
        st.cause = StopCause::Jump;
        drop(st);
        Ok(Some(self.stop_at(to, StopCause::Jump)))
    }

    fn forward(&self) -> Result<Option<Stop>> {
        self.delay();
        let mut st = self.lock();
        let Some(to) = st.forward.pop() else {
            return Ok(None);
        };
        let from = st.cursor;
        st.back.push(from);
        st.cursor = to;
        st.cause = StopCause::Jump;
        drop(st);
        Ok(Some(self.stop_at(to, StopCause::Jump)))
    }

    fn add_breakpoint(&self, location: BreakLocation) -> Result<UserBreakpoint> {
        self.delay();
        let addresses = self.resolve(&location);
        if addresses.is_empty() {
            return Err(HopperError::NotFound(format!("no code at {location:?}")));
        }
        let mut st = self.lock();
        let id = BreakpointId(st.next_breakpoint);
        st.next_breakpoint += 1;
        let bp = UserBreakpoint {
            id,
            location,
            addresses,
            enabled: true,
        };
        st.breakpoints.push(bp.clone());
        Ok(bp)
    }

    fn remove_breakpoint(&self, id: BreakpointId) -> Result<()> {
        self.delay();
        let mut st = self.lock();
        let before = st.breakpoints.len();
        st.breakpoints.retain(|b| b.id != id);
        if st.breakpoints.len() == before {
            return Err(HopperError::NotFound(format!("breakpoint {}", id.0)));
        }
        Ok(())
    }

    fn breakpoints(&self) -> Result<Vec<UserBreakpoint>> {
        self.delay();
        Ok(self.lock().breakpoints.clone())
    }

    fn frames(&self, at: Moment, tid: Tid) -> Result<Vec<Frame>> {
        self.delay();
        self.check_tid(tid)?;
        let index = self.index_at(&at)?;
        Ok(self.steps()[index]
            .frames
            .iter()
            .enumerate()
            .map(|(depth, f)| Frame {
                id: FrameId {
                    tid,
                    depth: depth as u32,
                },
                pc: f.pc,
                cfa: Some(f.cfa),
                function: Some(f.function.clone()),
                location: Some(f.location()),
                inlined: false,
            })
            .collect())
    }

    fn registers(&self, at: Moment, tid: Tid) -> Result<RegisterSet> {
        self.delay();
        self.check_tid(tid)?;
        Ok(self.registers_at(self.index_at(&at)?))
    }

    fn memory(&self, at: Moment, process: ProcessId, addr: u64, len: u64) -> Result<Vec<u8>> {
        self.delay();
        self.check_process(process)?;
        if len > 1 << 20 {
            return Err(HopperError::Invalid(format!(
                "memory read of {len} bytes is too large"
            )));
        }
        self.read_bytes(self.index_at(&at)?, addr, len)
    }

    fn disassemble(
        &self,
        at: Moment,
        process: ProcessId,
        addr: u64,
        count: u32,
    ) -> Result<Vec<Instruction>> {
        self.delay();
        self.check_process(process)?;
        self.index_at(&at)?;
        if !(script::TEXT.0..script::TEXT.1).contains(&addr) {
            return Err(HopperError::UnreadableMemory { addr, len: 1 });
        }
        let ins = &self.script.instructions;
        let start = ins.partition_point(|i| i.addr + i.bytes.len() as u64 <= addr);
        Ok(ins[start..].iter().take(count as usize).cloned().collect())
    }

    fn locals(&self, at: Moment, frame: FrameId) -> Result<Vec<ValueView>> {
        self.delay();
        let index = self.index_at(&at)?;
        let f = self.frame_at(index, frame)?;
        f.locals
            .iter()
            .map(|l| {
                let handle = ValueHandle {
                    at,
                    process: self.script.process(),
                    frame: Some(frame),
                    target: ValueTarget::Expr(l.name.clone()),
                    path: Vec::new(),
                };
                Ok(self.view_of(handle, self.eval_in(f, &l.name)?))
            })
            .collect()
    }

    fn eval(&self, at: Moment, frame: FrameId, expr: &str) -> Result<ValueView> {
        self.delay();
        let index = self.index_at(&at)?;
        let r = self.eval_in(self.frame_at(index, frame)?, expr)?;
        let handle = ValueHandle {
            at,
            process: self.script.process(),
            frame: Some(frame),
            target: ValueTarget::Expr(expr.trim().to_string()),
            path: Vec::new(),
        };
        Ok(self.view_of(handle, r))
    }

    fn view(&self, value: &ValueHandle) -> Result<ValueView> {
        self.delay();
        let r = self.resolve_handle(value)?;
        Ok(self.view_of(value.clone(), r))
    }

    fn children(&self, value: &ValueHandle, req: PageReq) -> Result<Vec<ValueView>> {
        self.delay();
        let r = self.resolve_handle(value)?;
        let kids: Vec<ValueView> = r
            .children()
            .into_iter()
            .map(|(elem, child)| self.view_of(value.child(elem), child))
            .collect();
        Ok(page(&kids, req, Completeness::Complete).items)
    }

    fn source_text(&self, file: &str) -> Result<Arc<str>> {
        self.delay();
        if SourceLoc::new(MAIN_RS, 1).file_matches(file) || file == MAIN_RS {
            Ok(Arc::from(WORKED_EXAMPLE_SOURCE))
        } else {
            Err(HopperError::NotFound(format!("source {file}")))
        }
    }

    fn calls(&self, filter: CallFilter, req: PageReq) -> Result<Page<CallNode>> {
        self.delay();
        if let Some(p) = filter.process {
            self.check_process(p)?;
        }
        let (covered, completeness) = self.coverage(PHASE_CALLS);
        let horizon = self.steps().get(covered).map(|s| s.moment);
        let items: Vec<CallNode> = self
            .script
            .calls
            .iter()
            .filter(|c| horizon.is_none_or(|h| c.entry.is_before(&h)))
            .filter(|c| match (&filter.parent, &filter.function) {
                (Some(p), _) => c.parent == Some(*p),
                // A function filter without a parent searches the whole tree.
                (None, Some(_)) => true,
                (None, None) => c.parent.is_none(),
            })
            .filter(|c| {
                filter
                    .function
                    .as_deref()
                    .is_none_or(|f| c.function.contains(f))
            })
            .filter(|c| filter.tid.is_none_or(|t| t == c.tid))
            .cloned()
            .map(|mut c| {
                // Calls not yet indexed by the horizon do not count as children either.
                if let Some(h) = horizon {
                    c.exit = c.exit.filter(|e| e.is_before(&h));
                    c.child_count = self
                        .script
                        .calls
                        .iter()
                        .filter(|k| k.parent == Some(c.id) && k.entry.is_before(&h))
                        .count() as u64;
                }
                c
            })
            .collect();
        Ok(page(&items, req, completeness))
    }

    fn line_hits(&self, loc: SourceLoc, range: MomentRange, req: PageReq) -> Result<Page<Moment>> {
        self.delay();
        let (covered, completeness) = self.coverage(PHASE_LINES);
        let steps = self.steps();
        let items: Vec<Moment> = (0..covered)
            .filter(|&j| {
                let f = steps[j].top();
                f.line == loc.line
                    && SourceLoc::new(f.file.clone(), f.line).file_matches(&loc.file)
                    && self.line_start(j) == j
                    && range.contains(&steps[j].moment)
            })
            .map(|j| steps[j].moment)
            .collect();
        Ok(page(&items, req, completeness))
    }

    fn value_history(
        &self,
        value: &ValueHandle,
        range: MomentRange,
        req: PageReq,
    ) -> Result<Page<(Moment, ValueView)>> {
        self.delay();
        self.check_process(value.process)?;
        let origin = self.index_at(&value.at)?;
        // The handle names a frame by depth at its own moment; at other moments the same frame
        // is the one with the same canonical frame address.
        let frame_id = value.frame.unwrap_or(FrameId { tid: TID, depth: 0 });
        let cfa = self.frame_at(origin, frame_id)?.cfa;
        let (covered, completeness) = self.coverage(PHASE_VALUES);
        let mut items: Vec<(Moment, ValueView)> = Vec::new();
        for s in &self.steps()[..covered] {
            if !range.contains(&s.moment) {
                continue;
            }
            let Some(depth) = s.frames.iter().position(|f| f.cfa == cfa) else {
                continue;
            };
            let h = ValueHandle {
                at: s.moment,
                frame: Some(FrameId {
                    tid: frame_id.tid,
                    depth: depth as u32,
                }),
                ..value.clone()
            };
            let Ok(r) = self.resolve_handle(&h) else {
                continue;
            };
            let v = self.view_of(h, r);
            let changed = items
                .last()
                .is_none_or(|(_, prev)| prev.summary != v.summary || prev.ownership != v.ownership);
            if changed {
                items.push((s.moment, v));
            }
        }
        Ok(page(&items, req, completeness))
    }

    fn last_write(&self, place: Place, before: Moment) -> Result<Option<WriteEvent>> {
        self.delay();
        if let Place::Memory { process, .. } = place {
            self.check_process(process)?;
        } else {
            return Err(HopperError::Unsupported(
                "register writes are not indexed".into(),
            ));
        }
        let index = self.index_at(&before)?;
        Ok(self.steps()[..=index]
            .iter()
            .rev()
            .flat_map(|s| s.writes.iter().rev())
            .find(|w| w.place.overlaps(&place))
            .cloned())
    }

    fn origin(&self, value: &ValueHandle, budget: Budget) -> Result<OriginChain> {
        self.delay();
        let r = self.resolve_handle(value)?;
        let index = self.index_at(&value.at)?;
        let frame = value.frame.unwrap_or(FrameId { tid: TID, depth: 0 });
        let function = self.frame_at(index, frame)?.function.clone();
        let (root, path) = origin_key(value, &r);
        let hops = &self.script.hops;
        let retries = path
            .iter()
            .any(|e| matches!(e, PathElem::Field(f) if f == "retries"));
        // A value reached through `n` starts with the borrow that made `n`, as a reference hop.
        let through_n = root == "n" && path.contains(&PathElem::Deref);
        let references: Vec<OriginHop> = if through_n {
            vec![hops.borrow_n.clone()]
        } else {
            Vec::new()
        };
        let chain: Option<Vec<&OriginHop>> = match (function.as_str(), root.as_str()) {
            (script::MAIN, "n") if through_n => Some(vec![
                &hops.move_into_vec,
                &hops.clone_name,
                &hops.move_a_to_b,
                &hops.load_return,
                &hops.alpha_literal,
            ]),
            (script::MAIN, "n") => Some(vec![&hops.borrow_n]),
            (script::MAIN, "v") if retries => Some(vec![
                &hops.move_into_vec,
                &hops.clone_retries,
                &hops.move_a_to_b,
                &hops.load_return,
                &hops.retries_literal,
            ]),
            (script::MAIN, "v") => Some(vec![
                &hops.move_into_vec,
                &hops.clone_name,
                &hops.move_a_to_b,
                &hops.load_return,
                &hops.alpha_literal,
            ]),
            (script::MAIN, "c") if retries => Some(vec![
                &hops.clone_retries,
                &hops.move_a_to_b,
                &hops.load_return,
                &hops.retries_literal,
            ]),
            (script::MAIN, "c") => Some(vec![
                &hops.clone_name,
                &hops.move_a_to_b,
                &hops.load_return,
                &hops.alpha_literal,
            ]),
            (script::MAIN, "b") if retries => Some(vec![
                &hops.move_a_to_b,
                &hops.load_return,
                &hops.retries_literal,
            ]),
            (script::MAIN, "b") => Some(vec![
                &hops.move_a_to_b,
                &hops.load_return,
                &hops.alpha_literal,
            ]),
            (script::MAIN, "a") if retries => Some(vec![&hops.load_return, &hops.retries_literal]),
            (script::MAIN, "a") => Some(vec![&hops.load_return, &hops.alpha_literal]),
            (script::LOAD, "name") => Some(vec![&hops.alpha_literal]),
            _ => None,
        };
        let Some(chain) = chain else {
            return Ok(OriginChain {
                hops: Vec::new(),
                end: OriginEnd::Unknown {
                    reason: format!("no recorded origin for `{}`", r.name),
                },
                references: Vec::new(),
            });
        };
        if chain[0].at.partial_cmp(&value.at) == Some(Ordering::Greater) {
            return Ok(OriginChain {
                hops: Vec::new(),
                end: OriginEnd::Unknown {
                    reason: format!("`{}` is not written yet", r.name),
                },
                references: Vec::new(),
            });
        }
        let max = budget.max_hops as usize;
        let end = if chain.len() > max {
            OriginEnd::Budget
        } else {
            OriginEnd::Literal
        };
        Ok(OriginChain {
            hops: chain.into_iter().take(max).cloned().collect(),
            end,
            references,
        })
    }

    fn allocation(
        &self,
        process: ProcessId,
        addr: u64,
        at: Moment,
    ) -> Result<Option<AllocationInfo>> {
        self.delay();
        self.check_process(process)?;
        self.index_at(&at)?;
        Ok(self
            .script
            .allocations
            .iter()
            .find(|a| {
                addr >= a.addr
                    && addr < a.addr + a.size
                    && !at.is_before(&a.allocated)
                    && a.freed.is_none_or(|f| at.is_before(&f))
            })
            .cloned())
    }
}

/// Which variable (and path below it) a handle names, for choosing its origin chain.
fn origin_key(value: &ValueHandle, r: &Resolved) -> (String, Vec<PathElem>) {
    match &value.target {
        ValueTarget::Expr(e) => match parse_expr(e) {
            Ok(p) => {
                // A `*` prefix dereferences the root before the rest of the path applies.
                let derefs = p
                    .prefixes
                    .iter()
                    .filter(|x| matches!(x, values::Prefix::Deref))
                    .map(|_| PathElem::Deref);
                let mut path: Vec<PathElem> = derefs.collect();
                path.extend(p.path);
                path.extend(value.path.iter().cloned());
                (p.root, path)
            }
            Err(_) => (r.name.clone(), value.path.clone()),
        },
        ValueTarget::Place { .. } => {
            let root = r.name.split(['.', '[']).next().unwrap_or("").to_string();
            (root, value.path.clone())
        }
    }
}

#[cfg(test)]
mod tests;
