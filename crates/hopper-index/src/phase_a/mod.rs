//! Phase A: the breakpoint-only index.
//!
//! [`PhaseA::run`] replays a background target from where it is to the end, stopping only at
//! breakpoints:
//! - **Function entries** (true entry, before the prologue) of the selected functions. At an
//!   entry the canonical frame address is `rsp + 8` and the return address is `[rsp]`; phase A
//!   arms a breakpoint at the return address and matches the return by thread and CFA
//!   (`rsp == cfa` when it hits), so recursion and the same return site on several threads are
//!   told apart. When an outer call's return matches, the inner pending frames it skipped
//!   (unwinding) end at the same moment.
//! - **Line statements**: per selected file and line, the lowest statement address in each
//!   function, past the prologue at a function's entry (gdb's `break file:line`).
//! - **Allocator entry points** (see [`allocator_function`] for the names): the arguments are
//!   read at entry and the result at the matched return. Only the outermost allocator call of a
//!   thread is recorded: `__rust_alloc` → `__rdl_alloc` → `malloc` is one allocation, made by
//!   `__rust_alloc`, while a direct `malloc` from C code is recorded as `malloc`. The requester
//!   is the first unwound frame that is not an allocator layer
//!   ([`IndexConfig::allocator_frames`]). realloc frees its old block and records the new one
//!   with the old one as predecessor.
//!
//!   **Which allocator symbols, and why.** Rust code allocates through the allocator shim
//!   `__rust_alloc`/`__rust_alloc_zeroed`/`__rust_realloc`/`__rust_dealloc` (rustc 1.97 mangles
//!   them as `__rustc::__rust_*`; older compilers export them unmangled). The shim forwards to
//!   `__rdl_*` (the default `System` allocator) or, with a `#[global_allocator]`, to `__rg_*`;
//!   `__rdl_*` then calls libc. Breaking on the shim names the allocation at the boundary Rust
//!   code crossed; breaking on libc's `malloc`, `calloc`, `realloc`, `reallocarray`, `free`,
//!   `posix_memalign`, `aligned_alloc` and `memalign` (in `libc.so.6`, found from `link_map`)
//!   catches C code and std's own direct libc calls. The `__rdl_*`/`__rg_*` breakpoints are kept
//!   for binaries whose shim was inlined away (LTO), and the outermost-call rule keeps them from
//!   double counting. On the worked example and threads fixtures (rustc 1.97, debug, no global
//!   allocator) the main binary has the four `__rust_*` and four `__rdl_*` symbols and no
//!   `__rg_*`. libc names match only unqualified C symbols, never a Rust `my::free`. Not covered:
//!   `valloc`/`pvalloc`, `mmap`-based allocators and arenas inside the program (bump allocators
//!   are invisible at the allocator boundary).
//! - **Timeline**: thread entries ([`IndexConfig::thread_entries`]) start a thread and their
//!   matched return ends it; a thread first seen without one starts where it was first seen;
//!   the main thread starts at the first moment; process exit ends the process and every thread
//!   still running; [`IndexConfig::anchors`] add named anchors.
//! - **Shared libraries**: the dynamic linker's `link_map` is read at the start, at the main
//!   executable's entry point and at every hit of `r_debug.r_brk`; libraries not in the symbol
//!   set are added and planned (allocators, anchors, thread entries, matching functions).
//!
//! **Honest completeness.** A breakpoint address that reaches
//! [`IndexConfig::max_hits_per_address`] is removed and recorded as capped from that moment;
//! queries that could involve it are `Partial`. Coverage advances with each committed batch, so
//! a cancelled or crashed run keeps what it finished.
//!
//! **Resuming.** Every commit stores a snapshot of phase A's state (open frames, live blocks,
//! ids, hit counts). A later run on a target that can seek continues from the snapshot's moment;
//! a target that cannot seek (gdbserver) starts over, clearing the partial phase-A data first.

mod libraries;
mod plan;

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use hopper_core::api::TimelineKind;
use hopper_core::target::{
    Breakpoint, Direction, Memory, Resume, StopReason, Target, TargetMemory,
};
use hopper_core::{HopperError, Moment, ProcessId, Reg, RegisterSet, Result, SourceLoc, Tid};
use hopper_symbols::{Binary, Module, ModuleId, SymbolSet};
use serde::{Deserialize, Serialize};

use crate::config::{IndexConfig, StartPoint};
use crate::control::IndexControl;
use crate::index::{Index, IndexKind};
use crate::store::{
    Batch, CapKey, Capped, Coverage, CoverageScope, CoveredInterval, Record, Stamp,
};

pub use plan::allocator_function;
use plan::{ModulePlan, plan_module};

/// An allocator entry point phase A understands, with its argument registers.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum AllocatorFunction {
    /// `__rust_alloc(size, align)` (also `__rdl_alloc`, `__rg_alloc`).
    RustAlloc,
    /// `__rust_alloc_zeroed(size, align)`.
    RustAllocZeroed,
    /// `__rust_realloc(ptr, old_size, align, new_size)`.
    RustRealloc,
    /// `__rust_dealloc(ptr, size, align)`.
    RustDealloc,
    /// `malloc(size)`.
    Malloc,
    /// `calloc(n, size)`.
    Calloc,
    /// `realloc(ptr, size)`.
    Realloc,
    /// `reallocarray(ptr, n, size)`.
    ReallocArray,
    /// `free(ptr)`.
    Free,
    /// `posix_memalign(&ptr, align, size)`: the block is written through the first argument.
    PosixMemalign,
    /// `aligned_alloc(align, size)`.
    AlignedAlloc,
    /// `memalign(align, size)`.
    Memalign,
}

impl AllocatorFunction {
    fn frees_at_entry(self) -> bool {
        matches!(
            self,
            AllocatorFunction::Free | AllocatorFunction::RustDealloc
        )
    }
}

/// What phase A will break on, before it runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhaseAPlan {
    pub functions: Vec<String>,
    pub lines: Vec<SourceLoc>,
    /// Allocator symbols found, with their kind, by module path.
    pub allocators: Vec<(String, AllocatorFunction)>,
    pub anchors: Vec<String>,
    pub thread_entries: usize,
    pub breakpoint_addresses: usize,
}

/// What a run did.
#[derive(Clone, Debug, PartialEq)]
pub struct PhaseASummary {
    /// Target stops handled.
    pub stops: u64,
    pub calls: u64,
    pub line_hits: u64,
    pub allocations: u64,
    pub frees: u64,
    pub timeline_events: u64,
    pub capped: Vec<CapKey>,
    /// Shared libraries added from the dynamic linker's list.
    pub libraries: Vec<String>,
    /// Allocator symbols with breakpoints (module-qualified).
    pub allocator_symbols: Vec<String>,
    /// The run reached the end of the process (or recording).
    pub reached_end: bool,
    /// The index was already complete and nothing ran.
    pub reused: bool,
    pub elapsed: Duration,
}

/// The phase-A job: the symbols and configuration it plans from.
#[derive(Clone, Debug)]
pub struct PhaseA {
    symbols: SymbolSet,
    config: IndexConfig,
    expected_end: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Role {
    Entry(u32),
    Line(u32),
    Allocator(AllocatorFunction, String),
    Anchor(String),
    ThreadEntry,
    LibraryHook,
}

impl Role {
    fn cap_key(&self, st: &State) -> Option<CapKey> {
        match self {
            Role::Entry(f) => Some(CapKey::Function(st.functions[*f as usize].0.clone())),
            Role::Line(l) => Some(CapKey::Line(st.lines[*l as usize].clone())),
            Role::Allocator(_, n) => Some(CapKey::Allocator(n.clone())),
            Role::Anchor(n) => Some(CapKey::Anchor(n.clone())),
            Role::ThreadEntry | Role::LibraryHook => None,
        }
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Site {
    roles: Vec<Role>,
    hits: u64,
    /// Pending returns that wait on this address.
    return_refs: u32,
    inserted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
enum Purpose {
    Call {
        id: u64,
    },
    Allocator {
        kind: AllocatorFunction,
        args: [u64; 4],
        requested_by: Option<String>,
    },
    ThreadEntry,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct PendingReturn {
    cfa: u64,
    ret: u64,
    purpose: Purpose,
}

/// Everything phase A carries between stops; stored as the resume snapshot.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct State {
    process: Option<ProcessId>,
    functions: Vec<(String, Option<SourceLoc>)>,
    lines: Vec<SourceLoc>,
    emitted_functions: HashSet<u32>,
    emitted_lines: HashSet<u32>,
    sites: HashMap<u64, Site>,
    threads: HashMap<i32, Vec<PendingReturn>>,
    known_threads: HashSet<i32>,
    exited_threads: HashSet<i32>,
    live: HashMap<u64, u64>,
    next_call: u64,
    next_alloc: u64,
    capped: Vec<Capped>,
    libraries: Vec<(String, u64)>,
    r_debug: Option<u64>,
    allocator_symbols: Vec<String>,
    coverage_from: Option<Stamp>,
    last: Option<Stamp>,
    /// A hash of the configuration, so a snapshot is resumed only with the same plan.
    config_key: String,
}

#[derive(Debug, Default)]
struct Buffers {
    calls: Vec<Record>,
    lines: Vec<Record>,
    allocs: Vec<Record>,
    timeline: Vec<Record>,
}

impl Buffers {
    fn len(&self) -> usize {
        self.calls.len() + self.lines.len() + self.allocs.len() + self.timeline.len()
    }
}

struct Runner<'a> {
    job: &'a PhaseA,
    symbols: SymbolSet,
    index: &'a Index,
    st: State,
    buf: Buffers,
    summary: PhaseASummary,
}

fn config_key(c: &IndexConfig) -> String {
    serde_json::to_string(c).unwrap_or_default()
}

impl PhaseA {
    pub fn new(symbols: SymbolSet, config: IndexConfig) -> Self {
        PhaseA {
            symbols,
            config,
            expected_end: None,
        }
    }

    /// The last event of the recording, for progress reports (`frame_time` / this).
    pub fn with_expected_end(mut self, frame_time: u64) -> Self {
        self.expected_end = Some(frame_time);
        self
    }

    pub fn config(&self) -> &IndexConfig {
        &self.config
    }

    /// The breakpoints the run starts with, from the symbol set as given (shared libraries
    /// found at run time add theirs later).
    pub fn plan(&self) -> PhaseAPlan {
        let mut out = PhaseAPlan {
            functions: Vec::new(),
            lines: Vec::new(),
            allocators: Vec::new(),
            anchors: Vec::new(),
            thread_entries: 0,
            breakpoint_addresses: 0,
        };
        let mut addrs = HashSet::new();
        for (id, m) in self.symbols.modules() {
            let p = plan_module(&self.symbols, id, &self.config, id == ModuleId(0));
            let path = m.binary().path().display().to_string();
            out.functions
                .extend(p.functions.iter().map(|f| f.name.clone()));
            out.lines.extend(p.lines.iter().map(|l| l.location.clone()));
            out.allocators.extend(
                p.allocators
                    .iter()
                    .map(|(_, k, n)| (format!("{path}:{n}"), *k)),
            );
            out.anchors.extend(p.anchors.iter().map(|(_, n)| n.clone()));
            out.thread_entries += p.thread_entries.len();
            addrs.extend(p.functions.iter().map(|f| f.entry));
            addrs.extend(p.lines.iter().flat_map(|l| l.addresses.iter().copied()));
            addrs.extend(p.allocators.iter().map(|a| a.0));
            addrs.extend(p.anchors.iter().map(|a| a.0));
            addrs.extend(p.thread_entries.iter().copied());
        }
        out.breakpoint_addresses = addrs.len();
        out
    }

    /// Run phase A on `target` to the end, committing into `index` as it goes. Returns at once
    /// (with `reused`) when the index already covers the whole recording.
    pub fn run(
        &self,
        target: &mut dyn Target,
        index: &Index,
        control: &IndexControl,
    ) -> Result<PhaseASummary> {
        let started = Instant::now();
        let mut summary = PhaseASummary {
            stops: 0,
            calls: 0,
            line_hits: 0,
            allocations: 0,
            frees: 0,
            timeline_events: 0,
            capped: Vec::new(),
            libraries: Vec::new(),
            allocator_symbols: Vec::new(),
            reached_end: false,
            reused: false,
            elapsed: Duration::ZERO,
        };
        if IndexKind::PHASE_A
            .iter()
            .all(|k| index.is_complete(*k).unwrap_or(false))
            || self.already_finished(index)?
        {
            summary.reused = true;
            summary.reached_end = true;
            control.set_progress("phase-a", 1.0);
            control.finish(None);
            return Ok(summary);
        }
        let resume: Option<State> = index
            .resume_state()?
            .and_then(|b| serde_json::from_slice::<State>(&b).ok())
            .filter(|s| s.config_key == config_key(&self.config) && target.info().can_seek);
        let mut runner = Runner {
            job: self,
            symbols: self.symbols.clone(),
            index,
            st: State::default(),
            buf: Buffers::default(),
            summary,
        };
        let result = match resume {
            Some(state) => runner
                .resume(target, state)
                .and_then(|()| runner.run_loop(target, control)),
            None => {
                index.clear(&IndexKind::PHASE_A)?;
                runner
                    .start(target)
                    .and_then(|()| runner.run_loop(target, control))
            }
        };
        let flushed = runner.flush(result.as_ref().is_ok_and(|ended| *ended));
        runner.summary.elapsed = started.elapsed();
        runner.summary.capped = runner.st.capped.iter().map(|c| c.key.clone()).collect();
        runner.summary.libraries = runner.st.libraries.iter().map(|(p, _)| p.clone()).collect();
        runner.summary.allocator_symbols = runner.st.allocator_symbols.clone();
        let outcome = result.and_then(|ended| flushed.map(|()| ended));
        match outcome {
            Ok(ended) => {
                runner.summary.reached_end = ended;
                control.set_progress("phase-a", 1.0);
                control.finish(None);
                Ok(runner.summary)
            }
            Err(e) => {
                control.finish(Some(e.to_string()));
                Err(e)
            }
        }
    }

    fn already_finished(&self, index: &Index) -> Result<bool> {
        // A finished run whose capped addresses keep it partial is still finished.
        let cov = index.coverage(IndexKind::Calls)?;
        Ok(cov.intervals.iter().any(|iv| iv.through.is_none()) && index.resume_state()?.is_none())
    }
}

/// The CFA at a function's first instruction: the return address was just pushed.
fn entry_cfa(regs: &RegisterSet) -> Option<u64> {
    regs.sp().map(|sp| sp + 8)
}

impl Runner<'_> {
    fn config(&self) -> &IndexConfig {
        &self.job.config
    }

    fn add_role(&mut self, addr: u64, role: Role) {
        let site = self.st.sites.entry(addr).or_default();
        if !site.roles.contains(&role) {
            site.roles.push(role);
        }
    }

    fn apply_plan(&mut self, p: ModulePlan, module_path: &str) {
        for f in p.functions {
            let fid = self.st.functions.len() as u32;
            self.st.functions.push((f.name, f.location));
            self.add_role(f.entry, Role::Entry(fid));
        }
        for l in p.lines {
            let lid = self.st.lines.len() as u32;
            self.st.lines.push(l.location);
            for a in l.addresses {
                self.add_role(a, Role::Line(lid));
            }
        }
        for (a, kind, name) in p.allocators {
            self.st
                .allocator_symbols
                .push(format!("{module_path}:{name}"));
            self.add_role(a, Role::Allocator(kind, name));
        }
        for (a, name) in p.anchors {
            self.add_role(a, Role::Anchor(name));
        }
        for a in p.thread_entries {
            self.add_role(a, Role::ThreadEntry);
        }
    }

    fn insert_all(&mut self, target: &mut dyn Target) -> Result<()> {
        let mut addrs: Vec<u64> = self
            .st
            .sites
            .iter()
            .filter(|(_, s)| !s.inserted && (!s.roles.is_empty() || s.return_refs > 0))
            .map(|(a, _)| *a)
            .collect();
        addrs.sort_unstable();
        for a in addrs {
            match target.insert(Breakpoint::code(a)) {
                Ok(()) => {
                    if let Some(s) = self.st.sites.get_mut(&a) {
                        s.inserted = true;
                    }
                }
                // An address the target will not break on: leave it out rather than fail the
                // run, and say so, as a cap from now (the recording start before the first stop).
                Err(HopperError::TargetRefused { .. }) => {
                    let from = self.st.last.unwrap_or(Stamp(0, 0, 0, 0));
                    let roles = self
                        .st
                        .sites
                        .remove(&a)
                        .map(|s| s.roles)
                        .unwrap_or_default();
                    for r in roles {
                        if let Some(key) = r.cap_key(&self.st) {
                            self.st.capped.push(Capped { key, from });
                        }
                    }
                }
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    fn start(&mut self, target: &mut dyn Target) -> Result<()> {
        self.st.config_key = config_key(self.config());
        self.st.process = Some(target.info().process);
        let ids: Vec<(ModuleId, String)> = self
            .symbols
            .modules()
            .map(|(id, m)| (id, m.binary().path().display().to_string()))
            .collect();
        for (id, path) in ids {
            let p = plan_module(&self.symbols, id, self.config(), id == ModuleId(0));
            self.apply_plan(p, &path);
        }
        // The main executable's entry point: by then ld.so has mapped every needed library.
        if let Ok(auxv) = target.auxv()
            && let Ok(a) = hopper_rr::Auxv::parse(&auxv)
            && let Some(entry) = a.entry()
            && self
                .symbols
                .module(ModuleId(0))
                .is_some_and(|m| m.binary().contains(m.to_file(entry)))
        {
            self.add_role(entry, Role::LibraryHook);
        }
        self.scan_libraries(target)?;
        self.insert_all(target)?;
        let now = target.now()?;
        let stamp = Stamp::of(&now);
        self.st.coverage_from = match self.config().start {
            StartPoint::RecordingStart => None,
            StartPoint::Here => Some(stamp),
        };
        self.st.last = Some(stamp);
        let tid = target.current_thread()?;
        self.st.known_threads.insert(tid.0);
        self.timeline(stamp, TimelineKind::ThreadStart);
        Ok(())
    }

    fn resume(&mut self, target: &mut dyn Target, state: State) -> Result<()> {
        let at = state
            .last
            .ok_or_else(|| HopperError::Index("resume state without a moment".into()))?;
        // Rebuild the symbol set's libraries.
        for (path, bias) in &state.libraries {
            if let Ok(b) = Binary::open(path) {
                self.symbols.add(Module::new(b, *bias));
            }
        }
        self.st = state;
        for s in self.st.sites.values_mut() {
            s.inserted = false;
        }
        target.seek(&at.moment(target.info().recording))?;
        self.insert_all(target)
    }

    fn scan_libraries(&mut self, target: &mut dyn Target) -> Result<()> {
        let Some(main) = self.symbols.module(ModuleId(0)).cloned() else {
            return Ok(());
        };
        let Some(map) = libraries::read_link_map(target, &main)? else {
            return Ok(());
        };
        self.st.r_debug = Some(map.r_debug);
        if map.r_brk != 0 {
            self.add_role(map.r_brk, Role::LibraryHook);
        }
        let known: HashSet<String> = self
            .symbols
            .modules()
            .filter_map(|(_, m)| m.binary().path().canonicalize().ok())
            .map(|p| p.display().to_string())
            .collect();
        for lib in map.libraries {
            let canon = std::fs::canonicalize(&lib.path)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| lib.path.clone());
            if known.contains(&canon) || self.st.libraries.iter().any(|(p, _)| *p == lib.path) {
                continue;
            }
            let Ok(binary) = Binary::open(&lib.path) else {
                continue;
            };
            let id = self.symbols.add(Module::new(binary, lib.bias));
            self.st.libraries.push((lib.path.clone(), lib.bias));
            let p = plan_module(&self.symbols, id, self.config(), false);
            self.apply_plan(p, &lib.path);
        }
        Ok(())
    }

    fn timeline(&mut self, at: Stamp, kind: TimelineKind) {
        self.buf.timeline.push(Record::Timeline { at, kind });
        self.summary.timeline_events += 1;
    }

    fn arm_return(&mut self, target: &mut dyn Target, ret: u64) -> Result<()> {
        let site = self.st.sites.entry(ret).or_default();
        site.return_refs += 1;
        if !site.inserted {
            target.insert(Breakpoint::code(ret))?;
            site.inserted = true;
        }
        Ok(())
    }

    fn disarm_return(&mut self, target: &mut dyn Target, ret: u64) -> Result<()> {
        if let Some(site) = self.st.sites.get_mut(&ret) {
            site.return_refs = site.return_refs.saturating_sub(1);
            if site.return_refs == 0 && site.roles.is_empty() && site.inserted {
                site.inserted = false;
                self.st.sites.remove(&ret);
                target.remove(Breakpoint::code(ret))?;
            }
        }
        Ok(())
    }

    /// Returns `true` when the process (or the recording) ended.
    fn run_loop(&mut self, target: &mut dyn Target, control: &IndexControl) -> Result<bool> {
        loop {
            if control.is_cancelled() {
                return Err(HopperError::Cancelled);
            }
            let ev = target.resume(Resume::Continue, Direction::Forward)?;
            self.summary.stops += 1;
            match ev.reason {
                StopReason::Breakpoint { addr } => {
                    let now = target.now()?;
                    self.on_breakpoint(target, ev.tid, addr, &now)?;
                    if let Some(end) = self.job.expected_end
                        && end > 0
                    {
                        control.set_progress("phase-a", now.frame_time as f32 / end as f32);
                    }
                }
                StopReason::Exited { code } => {
                    self.on_end(target, ev.tid, Some(i32::from(code)))?;
                    return Ok(true);
                }
                StopReason::Killed { .. } | StopReason::HistoryEnd => {
                    self.on_end(target, ev.tid, None)?;
                    return Ok(true);
                }
                // Signals, steps and anything else: keep going. (gdbserver drops a signal that
                // is not passed back on resume; rr replays signals regardless.)
                _ => {}
            }
            if self.buf.len() >= self.config().flush_every {
                self.flush(false)?;
            }
        }
    }

    fn on_end(&mut self, target: &mut dyn Target, tid: Tid, code: Option<i32>) -> Result<()> {
        // A process that has exited has no clock to read; the end is just after the last stop.
        let at = match target.now() {
            Ok(m) => Stamp::of(&m),
            Err(_) => {
                let l = self.st.last.unwrap_or(Stamp(0, tid.0, 0, 0));
                Stamp(l.0 + 1, tid.0, 0, 0)
            }
        };
        let mut threads: Vec<i32> = self
            .st
            .known_threads
            .iter()
            .filter(|t| !self.st.exited_threads.contains(t))
            .copied()
            .collect();
        threads.sort_unstable();
        for t in threads {
            self.timeline(Stamp(at.0, t, at.2, at.3), TimelineKind::ThreadExit);
        }
        self.timeline(at, TimelineKind::ProcessExit { code });
        self.st.last = Some(at);
        Ok(())
    }

    fn see_thread(&mut self, tid: Tid, at: Stamp, starting: bool) {
        if self.st.known_threads.insert(tid.0) && !starting {
            self.timeline(at, TimelineKind::ThreadStart);
        }
    }

    fn on_breakpoint(
        &mut self,
        target: &mut dyn Target,
        tid: Tid,
        addr: u64,
        now: &Moment,
    ) -> Result<()> {
        let at = Stamp::of(now);
        self.st.last = Some(at);
        let Some(site) = self.st.sites.get(&addr).cloned() else {
            return Ok(());
        };
        let starts_thread = site.roles.contains(&Role::ThreadEntry);
        self.see_thread(tid, at, starts_thread);
        let regs = target.registers(tid)?;
        if site.return_refs > 0 {
            self.on_return(target, tid, addr, &regs, at)?;
        }
        if site.roles.is_empty() {
            return Ok(());
        }
        for role in &site.roles {
            match role {
                Role::Line(lid) => {
                    if self.st.emitted_lines.insert(*lid) {
                        self.buf.lines.push(Record::Line {
                            lid: *lid,
                            location: self.st.lines[*lid as usize].clone(),
                        });
                    }
                    self.buf.lines.push(Record::LineHit { lid: *lid, at });
                    self.summary.line_hits += 1;
                }
                Role::Entry(fid) => self.on_entry(target, tid, *fid, &regs, at)?,
                Role::Allocator(kind, _) => {
                    self.on_allocator_entry(target, tid, *kind, &regs, at)?
                }
                Role::Anchor(name) => {
                    self.timeline(at, TimelineKind::Anchor { name: name.clone() })
                }
                Role::ThreadEntry => {
                    self.timeline(at, TimelineKind::ThreadStart);
                    self.push_pending(target, tid, &regs, Purpose::ThreadEntry)?;
                }
                Role::LibraryHook => {
                    let consistent = match self.st.r_debug {
                        Some(r) => libraries::r_state(target, r)? == libraries::RT_CONSISTENT,
                        None => true,
                    };
                    if consistent {
                        self.scan_libraries(target)?;
                        self.insert_all(target)?;
                    }
                }
            }
        }
        self.count_hit(target, addr, at)
    }

    fn count_hit(&mut self, target: &mut dyn Target, addr: u64, at: Stamp) -> Result<()> {
        let cap = self.config().max_hits_per_address;
        let Some(site) = self.st.sites.get_mut(&addr) else {
            return Ok(());
        };
        if site
            .roles
            .iter()
            .all(|r| matches!(r, Role::LibraryHook | Role::ThreadEntry))
        {
            return Ok(());
        }
        site.hits += 1;
        if site.hits < cap {
            return Ok(());
        }
        let roles = std::mem::take(&mut site.roles);
        let (kept, capped): (Vec<Role>, Vec<Role>) = roles
            .into_iter()
            .partition(|r| matches!(r, Role::LibraryHook | Role::ThreadEntry));
        site.roles = kept;
        let remove = site.roles.is_empty() && site.return_refs == 0 && site.inserted;
        if remove {
            site.inserted = false;
        }
        for r in capped {
            if let Some(key) = r.cap_key(&self.st) {
                self.st.capped.push(Capped { key, from: at });
            }
        }
        if remove {
            target.remove(Breakpoint::code(addr))?;
        }
        Ok(())
    }

    fn push_pending(
        &mut self,
        target: &mut dyn Target,
        tid: Tid,
        regs: &RegisterSet,
        purpose: Purpose,
    ) -> Result<()> {
        let (Some(cfa), Some(sp)) = (entry_cfa(regs), regs.sp()) else {
            return Ok(());
        };
        let ret = TargetMemory(target).read_u64(sp)?;
        self.st
            .threads
            .entry(tid.0)
            .or_default()
            .push(PendingReturn { cfa, ret, purpose });
        self.arm_return(target, ret)
    }

    fn on_entry(
        &mut self,
        target: &mut dyn Target,
        tid: Tid,
        fid: u32,
        regs: &RegisterSet,
        at: Stamp,
    ) -> Result<()> {
        let stack = self.st.threads.entry(tid.0).or_default();
        let parent = stack.iter().rev().find_map(|p| match p.purpose {
            Purpose::Call { id } => Some(id),
            _ => None,
        });
        let depth = stack
            .iter()
            .filter(|p| matches!(p.purpose, Purpose::Call { .. }))
            .count() as u32;
        let id = self.st.next_call;
        self.st.next_call += 1;
        if self.st.emitted_functions.insert(fid) {
            let (name, location) = self.st.functions[fid as usize].clone();
            self.buf.calls.push(Record::Function {
                fid,
                name,
                location,
            });
        }
        self.buf.calls.push(Record::CallEnter {
            id,
            fid,
            parent,
            process: self.st.process.unwrap_or(ProcessId::new(0)),
            at,
            depth,
        });
        self.summary.calls += 1;
        self.push_pending(target, tid, regs, Purpose::Call { id })
    }

    fn requester(&self, target: &mut dyn Target, regs: &RegisterSet) -> Option<String> {
        let mut mem = TargetMemory(target);
        let frames = self.symbols.unwind(&mut mem, regs).ok()?;
        frames
            .iter()
            .skip(1)
            .take(self.config().requester_unwind_depth)
            .flat_map(|p| {
                let pc = p.lookup_pc();
                let inline = self.symbols.frames_at(pc);
                let names: Vec<String> = if inline.is_empty() {
                    self.symbols
                        .symbol_at(pc)
                        .map(|s| s.name)
                        .into_iter()
                        .collect()
                } else {
                    inline.into_iter().filter_map(|f| f.function).collect()
                };
                names
            })
            .find(|n| !self.config().is_allocator_frame(n))
    }

    fn on_allocator_entry(
        &mut self,
        target: &mut dyn Target,
        tid: Tid,
        kind: AllocatorFunction,
        regs: &RegisterSet,
        at: Stamp,
    ) -> Result<()> {
        let nested = self.st.threads.get(&tid.0).is_some_and(|s| {
            s.iter()
                .any(|p| matches!(p.purpose, Purpose::Allocator { .. }))
        });
        if nested {
            return Ok(());
        }
        let arg = |r: Reg| regs.get(r).unwrap_or(0);
        let args = [arg(Reg::RDI), arg(Reg::RSI), arg(Reg::RDX), arg(Reg::RCX)];
        if kind.frees_at_entry() {
            if let Some(id) = self.st.live.remove(&args[0]) {
                self.buf.allocs.push(Record::Freed { id, at });
                self.summary.frees += 1;
            }
            return self.push_pending(
                target,
                tid,
                regs,
                Purpose::Allocator {
                    kind,
                    args,
                    requested_by: None,
                },
            );
        }
        let requested_by = self.requester(target, regs);
        self.push_pending(
            target,
            tid,
            regs,
            Purpose::Allocator {
                kind,
                args,
                requested_by,
            },
        )
    }

    fn on_return(
        &mut self,
        target: &mut dyn Target,
        tid: Tid,
        addr: u64,
        regs: &RegisterSet,
        at: Stamp,
    ) -> Result<()> {
        let Some(sp) = regs.sp() else { return Ok(()) };
        let Some(stack) = self.st.threads.get_mut(&tid.0) else {
            return Ok(());
        };
        let Some(j) = stack.iter().rposition(|p| p.ret == addr && p.cfa == sp) else {
            return Ok(());
        };
        let popped: Vec<PendingReturn> = stack.drain(j..).collect();
        // `popped[0]` returned; the rest were unwound past.
        for p in popped.iter().rev() {
            self.disarm_return(target, p.ret)?;
            let returned = std::ptr::eq(p, &popped[0]);
            match &p.purpose {
                Purpose::Call { id } => self.buf.calls.push(Record::CallExit { id: *id, at }),
                Purpose::ThreadEntry => {
                    self.st.exited_threads.insert(tid.0);
                    self.timeline(at, TimelineKind::ThreadExit);
                }
                Purpose::Allocator {
                    kind,
                    args,
                    requested_by,
                } if returned => {
                    let rax = regs.get(Reg::RAX).unwrap_or(0);
                    self.on_allocator_return(target, *kind, *args, requested_by.clone(), rax, at)?;
                }
                Purpose::Allocator { .. } => {}
            }
        }
        Ok(())
    }

    fn record_alloc(
        &mut self,
        addr: u64,
        size: u64,
        predecessor: Option<u64>,
        requested_by: Option<String>,
        kind: AllocatorFunction,
        at: Stamp,
    ) {
        let id = self.st.next_alloc;
        self.st.next_alloc += 1;
        self.st.live.insert(addr, id);
        self.buf.allocs.push(Record::Allocated {
            id,
            process: self.st.process.unwrap_or(ProcessId::new(0)),
            at,
            addr,
            size,
            predecessor,
            requested_by,
            allocator: kind,
        });
        self.summary.allocations += 1;
    }

    fn on_allocator_return(
        &mut self,
        target: &mut dyn Target,
        kind: AllocatorFunction,
        args: [u64; 4],
        requested_by: Option<String>,
        rax: u64,
        at: Stamp,
    ) -> Result<()> {
        use AllocatorFunction as A;
        let (block, size, old) = match kind {
            A::RustAlloc | A::RustAllocZeroed | A::Malloc => (rax, args[0], None),
            A::Calloc => (rax, args[0].saturating_mul(args[1]), None),
            A::AlignedAlloc | A::Memalign => (rax, args[1], None),
            A::Realloc => (rax, args[1], Some(args[0])),
            A::ReallocArray => (rax, args[1].saturating_mul(args[2]), Some(args[0])),
            A::RustRealloc => (rax, args[3], Some(args[0])),
            A::PosixMemalign => {
                if rax != 0 {
                    return Ok(());
                }
                (TargetMemory(target).read_u64(args[0])?, args[2], None)
            }
            A::Free | A::RustDealloc => return Ok(()),
        };
        let predecessor = match old {
            Some(old) if old != 0 && (block != 0 || size == 0) => {
                let prev = self.st.live.remove(&old);
                if let Some(id) = prev {
                    self.buf.allocs.push(Record::Freed { id, at });
                    self.summary.frees += 1;
                }
                prev
            }
            _ => None,
        };
        if block != 0 {
            self.record_alloc(block, size, predecessor, requested_by, kind, at);
        }
        Ok(())
    }

    fn coverage(&self, ended: bool) -> Coverage {
        let through = if ended { None } else { self.st.last };
        Coverage {
            intervals: vec![CoveredInterval {
                from: self.st.coverage_from,
                through,
                scope: CoverageScope::AllThreads,
            }],
            capped: self.st.capped.clone(),
        }
    }

    fn flush(&mut self, ended: bool) -> Result<()> {
        if self.st.last.is_none() {
            return Ok(());
        }
        let cov = self.coverage(ended);
        let resume = if ended {
            None
        } else {
            Some(
                serde_json::to_vec(&self.st)
                    .map_err(|e| HopperError::Index(format!("resume state: {e}")))?,
            )
        };
        let b = std::mem::take(&mut self.buf);
        self.index.commit(Batch {
            chunks: vec![
                (IndexKind::Calls, b.calls),
                (IndexKind::Lines, b.lines),
                (IndexKind::Allocations, b.allocs),
                (IndexKind::Timeline, b.timeline),
            ],
            coverage: IndexKind::PHASE_A
                .iter()
                .map(|k| (*k, cov.clone()))
                .collect(),
            resume: Some(resume),
        })
    }
}
