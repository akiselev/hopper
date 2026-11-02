//! hopper's engine: [`Session`] implements [`Debugger`] over rr recordings and live processes.
//!
//! - [`Session::open_rr`] opens an rr trace with a cursor replica (which follows the user) and
//!   background replicas (which answer questions about other moments, and run the reverse
//!   searches of `last_write`, `origin`, `value_history`, `line_hits` and `calls`), each on its own
//!   worker thread ([`hopper_rr::ReplicaPool`]).
//! - [`Session::live`] runs a program under gdbserver: forward only, a pool of one. Moving
//!   backwards is [`HopperError::Unsupported`]; a question about a moment the process has left is
//!   answered from what the session read while it was there, or refused honestly.
//!
//! State at a moment is read cache first ([`cache::MomentCache`]: memory chunks, registers and
//! stacks by moment), then from the cursor replica when it is at that moment, then from a
//! background replica seeked there. Queries never move the user's cursor.
//!
//! Ownership: when the binary has a matching `<binary>.hsc` sidecar, every local carries an
//! [`OwnershipState`] from the sidecar's events and the lines its frame has executed
//! ([`ownership`]).

pub mod cache;
pub mod decode;
mod indexed;
mod nav;
pub mod ownership;
mod provenance;
mod queries;
pub mod stepped;
pub mod symbols;
mod view;
pub mod writes;

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use hopper_core::api::{
    AllocationId, AllocationInfo, BreakLocation, BreakpointId, Budget, CallFilter, CallNode,
    Completeness, Debugger, Frame, IndexStatus, Instruction, OriginChain, OwnershipState, Page,
    PageReq, ProcessInfo, RecordingInfo, StepKind, Stop, StopCause, TimelineEvent, Until,
    UserBreakpoint, ValueHandle, ValueView, WriteEvent,
};
use hopper_core::target::{Breakpoint, Direction, Memory, Target, TargetMemory};
use hopper_core::{
    FrameId, HopperError, Moment, MomentRange, Place, ProcessId, RecordingId, RegisterSet, Result,
    SourceLoc, Tid,
};
use hopper_eval::Evaluator;
use hopper_rr::{
    GdbserverConfig, GdbserverFactory, ReplayOptions, ReplicaFactory, ReplicaPool, ReplicaRole, Rr,
    RrReplicaFactory,
};
use hopper_sidecar::{Sidecar, sidecar_path};
use hopper_symbols::{Binary, Module, SymbolSet};

pub use indexed::{IndexPolicy, LiveIndexPolicy};
pub use nav::watch_pieces;
use nav::{Driver, Halt};
use ownership::{LineHit, TracePoint};
pub use provenance::source_hops;
use symbols::{ProcessSymbols, SymbolCache};
use view::MomentView;

/// Whether to use an ownership sidecar.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum SidecarPolicy {
    /// `<executable>.hsc` when it exists and its build id matches.
    #[default]
    BesideBinary,
    /// This file (its build id must match).
    File(PathBuf),
    /// No ownership labels.
    Off,
}

/// How a session runs.
#[derive(Clone, Debug)]
pub struct SessionOptions {
    /// Background replicas for an rr session (a live session has none).
    pub background_replicas: usize,
    /// The rr binary; `None` finds `rr` on `PATH`.
    pub rr: Option<PathBuf>,
    pub replay: ReplayOptions,
    pub gdbserver: GdbserverConfig,
    pub sidecar: SidecarPolicy,
    pub index: IndexPolicy,
    /// Memory chunks ([`cache::CHUNK`] bytes each) the moment cache keeps.
    pub cache_chunks: usize,
    /// Resumes one navigation command may spend.
    pub navigation_budget: u64,
    /// Resumes one on-demand replay query (line hits, calls, value history) may spend.
    pub query_budget: u64,
    /// Whether queries about moments the write log does not cover start phase-B windows.
    pub write_log: writes::WriteLogPolicy,
    /// Whether a live session indexes a reference run of its program.
    pub live_index: LiveIndexPolicy,
}

impl Default for SessionOptions {
    fn default() -> Self {
        SessionOptions {
            background_replicas: 2,
            rr: None,
            replay: ReplayOptions::default(),
            gdbserver: GdbserverConfig::default(),
            sidecar: SidecarPolicy::default(),
            index: IndexPolicy::default(),
            cache_chunks: 16_384,
            navigation_budget: 2_000_000,
            query_budget: 100_000,
            write_log: writes::WriteLogPolicy::default(),
            live_index: LiveIndexPolicy::default(),
        }
    }
}

/// Whether the session has ownership labels, and why not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SidecarStatus {
    Loaded(PathBuf),
    Absent,
    Disabled,
    /// The sidecar was found but refused (stale build id, unreadable).
    Rejected {
        path: PathBuf,
        reason: String,
    },
}

/// A boxed factory, so one pool type serves rr, gdbserver and tests.
struct Factory(Box<dyn ReplicaFactory>);

impl ReplicaFactory for Factory {
    fn start(&self, role: ReplicaRole) -> Result<Box<dyn Target>> {
        self.0.start(role)
    }
}

#[derive(Debug)]
enum Mode {
    Recording {
        trace_dir: PathBuf,
        rr: Rr,
    },
    /// A reverse-capable replay from another source (tests, transcripts).
    Replay {
        description: String,
    },
    Live {
        program: PathBuf,
        args: Vec<OsString>,
    },
}

#[derive(Debug)]
struct CursorState {
    stop: Stop,
    back: Vec<Moment>,
    forward: Vec<Moment>,
    breakpoints: Vec<UserBreakpoint>,
    next_breakpoint: u32,
    /// Every stop the cursor made (a live session's only line hits besides tracepoints).
    seen: Vec<(Moment, Option<SourceLoc>)>,
}

/// What every replica's jobs share.
pub(crate) struct Shared {
    pool: ReplicaPool<Factory>,
    mode: Mode,
    pub recording: RecordingId,
    pub process: ProcessId,
    pub exe: PathBuf,
    pub auxv: Vec<u8>,
    pub symbols: SymbolCache,
    pub evaluator: Evaluator,
    pub cache: cache::MomentCache,
    pub sidecar: Option<Arc<Sidecar>>,
    pub sidecar_status: SidecarStatus,
    /// Tracepoints kept inserted on the cursor replica (a live session with a sidecar).
    pub tracepoints: HashMap<u64, TracePoint>,
    /// Every sidecar tracepoint, for replays on background replicas.
    pub sidecar_lines: HashMap<u64, TracePoint>,
    pub options: SessionOptions,
    /// The index, when the session keeps one; why not otherwise.
    pub index: std::result::Result<indexed::Indexed, indexed::NoIndex>,
    /// The main executable's symbols alone (what the index was built against).
    pub main_symbols: Option<SymbolSet>,
    /// Phase-B windows started on demand.
    pub write_windows: writes::WriteWindows,
    /// A live session's reference-run index; why not otherwise.
    pub reference: std::result::Result<indexed::ReferenceIndex, indexed::NoIndex>,
    start: Stop,
    rr_version: Option<String>,
    state: Mutex<CursorState>,
    nav: Mutex<()>,
    trace: Mutex<Vec<LineHit>>,
    end: OnceLock<Option<Moment>>,
    processes: OnceLock<Vec<ProcessInfo>>,
    /// Frame-instance line hits found by replay, by (moment, thread, CFA).
    replay_hits: Mutex<HashMap<(Moment, Tid, u64), FrameHits>>,
    line_hit_cache: Mutex<HashMap<SourceLoc, Arc<Vec<Moment>>>>,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shared")
            .field("recording", &self.recording)
            .field("mode", &self.mode)
            .field("exe", &self.exe)
            .finish()
    }
}

/// A frame instance's tracepoint hits; `None` when they cannot be known.
type FrameHits = Option<Arc<Vec<LineHit>>>;

/// The ownership state of a frame's locals, by variable name.
type OwnershipLookup = Box<dyn Fn(&str) -> Option<OwnershipState>>;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

/// Whether a cursor move enters navigation history.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum History {
    Record,
    /// back/forward themselves.
    Untracked,
}

impl Shared {
    pub fn is_live(&self) -> bool {
        matches!(self.mode, Mode::Live { .. })
    }

    /// The symbols of the process in the state the target is in now.
    pub fn symbols_live(&self, t: &mut dyn Target) -> Result<Arc<ProcessSymbols>> {
        let mut mem = TargetMemory(t);
        self.symbols
            .process(&self.exe, &self.auxv, &mut mem as &mut dyn Memory)
    }

    fn check_moment(&self, m: &Moment) -> Result<()> {
        if m.recording != self.recording {
            return Err(HopperError::BadMoment(format!(
                "moment {m} belongs to recording {}, not {}",
                m.recording, self.recording
            )));
        }
        Ok(())
    }

    fn cursor_moment(&self) -> Moment {
        lock(&self.state).stop.at
    }

    /// The heap allocation holding `addr` at `at`, from the index.
    pub fn allocation_of(&self, addr: u64, at: &Moment) -> Option<AllocationId> {
        let ix = self.index.as_ref().ok()?;
        ix.index
            .allocation_at(self.process, addr, *at)
            .ok()?
            .value
            .map(|a| a.id)
    }

    fn live_refusal(&self, what: &str) -> HopperError {
        HopperError::Unsupported(format!(
            "{what} needs reverse execution; a live session runs forward only (record with rr)"
        ))
    }
}

/// Run `f` on the state at `at`: from the cache, else the cursor replica when it is there, else a
/// background replica seeked there (rr). `f` only reads.
fn at_moment<R, F>(shared: &Arc<Shared>, at: Moment, f: F) -> Result<R>
where
    R: Send + 'static,
    F: Fn(&mut MomentView<'_>) -> Result<R> + Send + Sync + 'static,
{
    shared.check_moment(&at)?;
    let f = Arc::new(f);
    // The cache alone first: a moment seen before needs no replica.
    let cached = {
        let mut v = MomentView::cache_only(shared, at);
        let r = f(&mut v);
        if !v.missed {
            return r;
        }
        r
    };
    if shared.cursor_moment() == at {
        let (s, g) = (shared.clone(), f.clone());
        let r = shared.pool.cursor(move |t| -> Result<Option<R>> {
            if t.now()? != at {
                return Ok(None);
            }
            let mut v = MomentView::on(&s, at, t);
            g(&mut v).map(Some)
        })??;
        if let Some(r) = r {
            return Ok(r);
        }
    }
    if shared.is_live() {
        // The process has left `at`: what the cache holds is all there is.
        return cached;
    }
    let lease = shared.pool.lease()?;
    let s = shared.clone();
    lease.run(move |t| {
        if t.now().ok() != Some(at) {
            t.seek(&at)?;
        }
        let mut v = MomentView::on(&s, at, t);
        f(&mut v)
    })?
}

/// A debugging session over an rr recording or a live process. Cheap to share: clone the
/// `Arc` the front ends hold. Internally synchronised.
#[derive(Debug)]
pub struct Session {
    inner: Arc<Shared>,
}

/// What the first job on a new cursor replica reports.
struct Opening {
    recording: RecordingId,
    process: ProcessId,
    exe: PathBuf,
    auxv: Vec<u8>,
    at: Moment,
    tid: Tid,
    pc: u64,
    rr_version: Option<String>,
}

impl Session {
    /// Open an rr trace directory.
    pub fn open_rr(trace_dir: &Path, options: SessionOptions) -> Result<Session> {
        let rr = match &options.rr {
            Some(p) => Rr::at(p)?,
            None => Rr::detect()?,
        };
        let recording = rr.recording_id(trace_dir)?;
        let factory = RrReplicaFactory {
            rr: rr.clone(),
            trace_dir: trace_dir.to_path_buf(),
            options: options.replay.clone(),
            recording,
        };
        let background = options.background_replicas;
        Session::start(
            Box::new(factory),
            background,
            Mode::Recording {
                trace_dir: trace_dir.to_path_buf(),
                rr,
            },
            options,
        )
    }

    /// A session over replicas from any factory whose targets can reverse and seek (the
    /// background replicas answer for other moments, as with rr). For replays that are not rr
    /// traces: recorded transcripts and test recordings.
    pub fn replay(
        factory: impl ReplicaFactory,
        background: usize,
        description: impl Into<String>,
        options: SessionOptions,
    ) -> Result<Session> {
        Session::start(
            Box::new(factory),
            background,
            Mode::Replay {
                description: description.into(),
            },
            options,
        )
    }

    /// Run `program` under gdbserver, stopped at its first instruction.
    pub fn live(program: &Path, args: &[OsString], options: SessionOptions) -> Result<Session> {
        let factory = GdbserverFactory {
            config: options.gdbserver.clone(),
            program: program.to_path_buf(),
            args: args.to_vec(),
        };
        Session::start(
            Box::new(factory),
            0,
            Mode::Live {
                program: program.to_path_buf(),
                args: args.to_vec(),
            },
            options,
        )
    }

    fn start(
        factory: Box<dyn ReplicaFactory>,
        background: usize,
        mode: Mode,
        options: SessionOptions,
    ) -> Result<Session> {
        let pool = ReplicaPool::new(Factory(factory), background)?;
        let open = pool.cursor(|t| -> Result<Opening> {
            let info = t.info().clone();
            let tid = t.current_thread()?;
            let pc = t.registers(tid)?.pc().unwrap_or(0);
            Ok(Opening {
                recording: info.recording,
                process: info.process,
                exe: info.executable.clone(),
                auxv: t.auxv()?,
                at: t.now()?,
                tid,
                pc,
                rr_version: info.rr_version.clone(),
            })
        })??;
        let exe = match &mode {
            Mode::Live { program, .. } if open.exe.as_os_str().is_empty() => program.clone(),
            _ => open.exe.clone(),
        };
        // The main executable alone places tracepoints and names the first stop.
        let main_syms = Binary::open(&exe)
            .and_then(|b| Module::main_from_auxv(b, &open.auxv))
            .map(|m| {
                let mut s = SymbolSet::new();
                s.add(m);
                s
            });
        let (sidecar, sidecar_status) = load_sidecar(&options.sidecar, &exe);
        let sidecar_lines = match (&sidecar, &main_syms) {
            (Some(sc), Ok(syms)) => ownership::tracepoints(sc, syms),
            _ => HashMap::new(),
        };
        let live = matches!(mode, Mode::Live { .. });
        let tracepoints = if live {
            sidecar_lines.clone()
        } else {
            HashMap::new()
        };
        let (function, location) = match &main_syms {
            Ok(s) => provenance::describe(s, open.pc),
            Err(_) => (None, None),
        };
        let index = open_index(&options.index, &mode, open.recording, &exe);
        let reference = match (&mode, &options.live_index, &options.index) {
            (Mode::Live { .. }, _, IndexPolicy::Off) => {
                Err(indexed::NoIndex::NotKept("the index is off".into()))
            }
            (Mode::Live { program, args }, LiveIndexPolicy::ReferenceRun, policy) => {
                indexed::start_reference_run(policy, &options.gdbserver, program, args)
            }
            (Mode::Live { .. }, LiveIndexPolicy::Off, _) => Err(indexed::NoIndex::NotKept(
                "the live session's reference run is off".into(),
            )),
            _ => Err(indexed::NoIndex::NotKept(
                "only a live session indexes a reference run".into(),
            )),
        };
        let start = Stop {
            at: open.at,
            tid: open.tid,
            pc: open.pc,
            cause: StopCause::HistoryStart,
            function,
            location,
        };
        let shared = Arc::new(Shared {
            pool,
            mode,
            recording: open.recording,
            process: open.process,
            exe,
            auxv: open.auxv,
            symbols: SymbolCache::default(),
            evaluator: Evaluator::new(),
            cache: cache::MomentCache::new(options.cache_chunks),
            sidecar,
            sidecar_status,
            tracepoints,
            sidecar_lines,
            options,
            index,
            main_symbols: main_syms.as_ref().ok().cloned(),
            write_windows: writes::WriteWindows::default(),
            reference,
            start: start.clone(),
            rr_version: open.rr_version,
            state: Mutex::new(CursorState {
                stop: start.clone(),
                back: Vec::new(),
                forward: Vec::new(),
                breakpoints: Vec::new(),
                next_breakpoint: 1,
                seen: vec![(start.at, start.location.clone())],
            }),
            nav: Mutex::new(()),
            trace: Mutex::new(Vec::new()),
            end: OnceLock::new(),
            processes: OnceLock::new(),
            replay_hits: Mutex::new(HashMap::new()),
            line_hit_cache: Mutex::new(HashMap::new()),
        });
        if !shared.tracepoints.is_empty() {
            let addrs: Vec<u64> = shared.tracepoints.keys().copied().collect();
            shared.pool.cursor(move |t| -> Result<()> {
                for a in addrs {
                    t.insert(Breakpoint::code(a))?;
                }
                Ok(())
            })??;
        }
        if let (Ok(ix), Some(main)) = (&shared.index, &shared.main_symbols) {
            indexed::start_phase_a(&shared, ix, main.clone())?;
        }
        Ok(Session { inner: shared })
    }

    /// Whether this session has ownership labels.
    pub fn sidecar_status(&self) -> &SidecarStatus {
        &self.inner.sidecar_status
    }

    /// Whether the session runs a live process (forward only).
    pub fn is_live(&self) -> bool {
        self.inner.is_live()
    }

    /// A live session's reference run and the range asked, in its moments: an open range, or
    /// one bounded by the reference run's own moments. A range bounded by the live run's
    /// moments has no counterpart there (`None`).
    fn reference_for(
        &self,
        range: MomentRange,
    ) -> Option<(&indexed::ReferenceIndex, MomentRange)> {
        let r = self.inner.reference.as_ref().ok()?;
        let ours = |m: &Option<Moment>| m.is_none_or(|m| m.recording == r.run.id);
        (ours(&range.start) && ours(&range.end)).then_some((r, range))
    }

    /// The heap allocation holding `addr` in a live session, from its reference run: gdbserver
    /// disables address randomisation, so a deterministic program's heap addresses match. When
    /// the reference run reused the address for several allocations, which one the live
    /// process is in cannot be told, and the answer says so.
    fn reference_allocation(&self, addr: u64) -> Result<Option<AllocationInfo>> {
        let r = self.inner.reference.as_ref().map_err(|why| {
            HopperError::Unsupported(format!(
                "allocation lineage comes from hopper-index, and this live session has none ({why})"
            ))
        })?;
        let page = r.indexed.index.allocations(
            MomentRange::all(),
            PageReq {
                offset: 0,
                limit: u32::MAX,
            },
        )?;
        let mut holding = page
            .items
            .into_iter()
            .filter(|a| addr >= a.addr && addr < a.addr + a.size.max(1));
        match (holding.next(), holding.next()) {
            (None, _) => Ok(None),
            (Some(one), None) => Ok(Some(one)),
            (Some(_), Some(_)) => Err(HopperError::Unsupported(format!(
                "the reference run allocated {addr:#x} more than once; a live session cannot \
                 tell which of them it is in"
            ))),
        }
    }

    /// The phase-B windows queries started (see [`writes`]).
    pub fn write_windows(&self) -> Vec<writes::WriteWindow> {
        self.inner.write_windows.windows()
    }

    /// Why this session has no index, when it has none.
    pub fn index_unavailable(&self) -> Option<String> {
        self.inner.index.as_ref().err().map(|e| e.to_string())
    }

    /// Run to the start of the program's `main` (gdb's `start`): a live session begins at the
    /// dynamic linker's first instruction.
    pub fn run_to_main(&self) -> Result<Stop> {
        let bp = self.add_breakpoint(BreakLocation::Function("main".into()))?;
        let stop = self.run(Direction::Forward, Until::Breakpoints);
        let removed = self.remove_breakpoint(bp.id);
        let stop = stop?;
        removed?;
        Ok(stop)
    }

    /// The same value at another moment: the handle's frame is found at `to` by its CFA and
    /// function (a frame's depth changes as calls come and go). `NotFound` when that frame does
    /// not exist at `to`.
    pub fn relocate(&self, handle: &ValueHandle, to: Moment) -> Result<ValueHandle> {
        let Some(frame) = handle.frame else {
            return Ok(handle.at(to));
        };
        let (cfa, function) = at_moment(self.shared(), handle.at, move |v| {
            let f = v.frame(frame)?;
            Ok((f.cfa, f.frame.function.clone()))
        })?;
        let tid = frame.tid;
        let found = at_moment(self.shared(), to, move |v| {
            let stack = v.stack(tid)?;
            Ok(provenance::frame_with(&stack, cfa, function.as_deref()))
        })?;
        let depth = found.ok_or_else(|| {
            HopperError::NotFound(format!("the frame of {:?} at {to}", handle.target))
        })?;
        Ok(ValueHandle {
            at: to,
            frame: Some(depth),
            ..handle.clone()
        })
    }

    /// The line hits the cursor has observed at tracepoints (live sessions with a sidecar).
    pub fn observed_line_hits(&self) -> Vec<LineHit> {
        lock(&self.inner.trace).clone()
    }

    fn shared(&self) -> &Arc<Shared> {
        &self.inner
    }

    /// Run one navigation command on the cursor replica.
    fn navigate<F>(&self, history: History, cause_on_arrival: StopCause, f: F) -> Result<Stop>
    where
        F: FnOnce(&mut Driver<'_>, Tid) -> Result<Halt> + Send + 'static,
    {
        let shared = self.shared().clone();
        let _nav = lock(&shared.nav);
        let user: HashMap<u64, BreakpointId> = {
            let st = lock(&shared.state);
            st.breakpoints
                .iter()
                .filter(|b| b.enabled)
                .flat_map(|b| b.addresses.iter().map(move |a| (*a, b.id)))
                .collect()
        };
        let s = shared.clone();
        let (outcome, hits) = shared.pool.cursor(move |t| {
            let mut d = match Driver::new(t, &s, user) {
                Ok(d) => d,
                Err(e) => return (Err(e), Vec::new()),
            };
            let tid = match d.t.current_thread() {
                Ok(t) => t,
                Err(e) => return (Err(e), Vec::new()),
            };
            let halt = f(&mut d, tid);
            let hits = std::mem::take(&mut d.hits);
            let stop = halt.and_then(|h| describe_stop(&mut d, h, cause_on_arrival));
            (stop, hits)
        })?;
        if !hits.is_empty() {
            lock(&shared.trace).extend(hits);
        }
        let stop = outcome?;
        let mut st = lock(&shared.state);
        if st.stop.at != stop.at && history == History::Record {
            let from = st.stop.at;
            st.back.push(from);
            st.forward.clear();
        }
        st.seen.push((stop.at, stop.location.clone()));
        st.stop = stop.clone();
        Ok(stop)
    }

    /// Move the cursor to `to` without touching navigation history (back/forward use it).
    fn jump(&self, to: Moment) -> Result<Stop> {
        let shared = self.shared().clone();
        shared.check_moment(&to)?;
        if shared.is_live() {
            return self.live_forward_to(to);
        }
        self.navigate(History::Untracked, StopCause::Jump, move |d, _| {
            d.t.seek(&to)?;
            Ok(Halt::Arrived)
        })
    }

    /// A live session reaches a later moment of the same stop by stepping instructions.
    fn live_forward_to(&self, to: Moment) -> Result<Stop> {
        let here = self.inner.cursor_moment();
        if here == to {
            let mut st = lock(&self.inner.state);
            st.stop.cause = StopCause::Jump;
            return Ok(st.stop.clone());
        }
        if to.frame_time == here.frame_time && to.tid == here.tid && to.sub > here.sub {
            let n = to.sub - here.sub;
            return self.navigate(History::Record, StopCause::Jump, move |d, tid| {
                for _ in 0..n {
                    match d.step_instruction(tid, Direction::Forward)? {
                        Halt::Stepped => {}
                        other => return Ok(other),
                    }
                }
                Ok(Halt::Arrived)
            });
        }
        Err(HopperError::Unsupported(format!(
            "a live session cannot move from {here} to {to}: it only runs forward, and only \
             steps within one stop"
        )))
    }

    fn resolve_breakpoint(&self, location: &BreakLocation) -> Result<Vec<u64>> {
        let s = self.shared().clone();
        let location = location.clone();
        self.shared().pool.cursor(move |t| -> Result<Vec<u64>> {
            let syms = s.symbols_live(t)?;
            let mut out: Vec<u64> = match &location {
                BreakLocation::Address(a) => vec![*a],
                BreakLocation::Source(loc) => syms
                    .set
                    .addresses_for_line(&loc.file, loc.line)
                    .into_iter()
                    .map(|a| a.address)
                    .collect(),
                BreakLocation::Function(name) => syms
                    .set
                    .find_functions(name)
                    .iter()
                    .map(|f| syms.set.breakpoint_address(f))
                    .collect(),
            };
            out.sort_unstable();
            out.dedup();
            Ok(out)
        })?
    }

    /// The frame identity a value's ownership needs, read at `at`.
    fn frame_key(&self, at: Moment, frame: FrameId) -> Result<Option<queries::FrameKey>> {
        if self.inner.sidecar.is_none() {
            return Ok(None);
        }
        at_moment(self.shared(), at, move |v| {
            let f = v.frame(frame)?;
            let syms = v.symbols()?;
            Ok(queries::FrameKey::of(&syms.set, &f))
        })
    }

    /// The ownership state of locals of `frame` at `at`, as a lookup by variable name.
    fn ownership_lookup(&self, at: Moment, frame: FrameId) -> Option<OwnershipLookup> {
        let sidecar = self.inner.sidecar.clone()?;
        let key = self.frame_key(at, frame).ok().flatten()?;
        let function = sidecar.function_named(&key.linkage)?;
        let hits = queries::frame_hits(self.shared(), at, &key, function)?;
        Some(Box::new(move |name: &str| {
            Some(ownership::local_state(
                &sidecar, function, name, &hits, key.line,
            ))
        }))
    }

    fn with_ownership(&self, at: Moment, frame: FrameId, views: &mut [ValueView]) {
        if let Some(lookup) = self.ownership_lookup(at, frame) {
            view::apply_ownership(views, &*lookup);
        }
    }

    fn handle_frame(h: &ValueHandle) -> FrameId {
        h.frame.unwrap_or(FrameId {
            tid: h.at.tid,
            depth: 0,
        })
    }
}

/// Open the index the policy names (rr and replays only: a live process cannot be replayed).
fn open_index(
    policy: &IndexPolicy,
    mode: &Mode,
    recording: RecordingId,
    exe: &Path,
) -> std::result::Result<indexed::Indexed, indexed::NoIndex> {
    use indexed::NoIndex;
    let dir = match (policy, mode) {
        (IndexPolicy::Off, _) => return Err(NoIndex::NotKept("the index is off".into())),
        (_, Mode::Live { .. }) => {
            return Err(NoIndex::NotKept(
                "a live session has no index: it cannot be replayed".into(),
            ));
        }
        (IndexPolicy::Dir(d), _) => d.clone(),
        (IndexPolicy::BesideTrace, Mode::Recording { trace_dir, .. }) => {
            PathBuf::from(format!("{}.hopper-index", trace_dir.display()))
        }
        (IndexPolicy::BesideTrace, Mode::Replay { .. }) => {
            return Err(NoIndex::NotKept(
                "this replay has no trace to keep an index beside".into(),
            ));
        }
    };
    let build_id = Binary::open(exe)
        .ok()
        .and_then(|b| b.build_id().map(|id| id.to_hex()));
    let meta = hopper_index::RecordingMeta {
        recording,
        build_ids: build_id.into_iter().collect(),
    };
    std::fs::create_dir_all(&dir)
        .map_err(|e| NoIndex::Failed(format!("index directory {}: {e}", dir.display())))?;
    indexed::Indexed::open(&dir, meta).map_err(|e| NoIndex::Failed(e.to_string()))
}

fn load_sidecar(policy: &SidecarPolicy, exe: &Path) -> (Option<Arc<Sidecar>>, SidecarStatus) {
    let path = match policy {
        SidecarPolicy::Off => return (None, SidecarStatus::Disabled),
        SidecarPolicy::File(p) => p.clone(),
        SidecarPolicy::BesideBinary => sidecar_path(exe),
    };
    if !path.is_file() {
        return (None, SidecarStatus::Absent);
    }
    match Sidecar::open_for_binary(&path, exe) {
        Ok(s) => (Some(Arc::new(s)), SidecarStatus::Loaded(path)),
        Err(e) => (
            None,
            SidecarStatus::Rejected {
                path,
                reason: e.to_string(),
            },
        ),
    }
}

/// The user-facing stop after a navigation command.
fn describe_stop(d: &mut Driver<'_>, halt: Halt, arrival: StopCause) -> Result<Stop> {
    let cause = match halt {
        Halt::Stepped => StopCause::Step,
        Halt::Arrived => arrival,
        Halt::Breakpoint(id) => StopCause::Breakpoint { id },
        Halt::Watchpoint(place) => StopCause::Watchpoint { place },
        Halt::Signal(signo) => StopCause::Signal { signo },
        Halt::Exited(code) => StopCause::Exited { code },
        Halt::HistoryStart => StopCause::HistoryStart,
        Halt::HistoryEnd => StopCause::HistoryEnd,
    };
    let at = d.t.now()?;
    let tid = d.t.current_thread().unwrap_or(at.tid);
    let pc = match cause {
        StopCause::Exited { .. } => 0,
        _ => d.pc_of(tid).unwrap_or(0),
    };
    let (function, location) = if pc == 0 {
        (None, None)
    } else {
        d.describe(pc)
    };
    Ok(Stop {
        at,
        tid,
        pc,
        cause,
        function,
        location,
    })
}

fn frame_of(f: &hopper_symbols::StackFrame) -> Frame {
    f.frame.clone()
}

impl Drop for Session {
    fn drop(&mut self) {
        // Phase A holds the shared state until it stops; stop it.
        if let Ok(ix) = &self.inner.index {
            ix.control.cancel();
        }
    }
}

impl Debugger for Session {
    fn recording(&self) -> Result<RecordingInfo> {
        let s = self.shared();
        let processes = queries::processes(s)?;
        let (path, end) = match &s.mode {
            Mode::Recording { trace_dir, .. } => {
                (trace_dir.display().to_string(), queries::end_moment(s))
            }
            Mode::Replay { description } => (description.clone(), queries::end_moment(s)),
            Mode::Live { program, .. } => (format!("live: {}", program.display()), None),
        };
        Ok(RecordingInfo {
            id: s.recording,
            path,
            rr_version: s.rr_version.clone(),
            start: Some(s.start.at),
            end,
            processes,
            reference_run: s.reference.as_ref().ok().map(|r| r.run.clone()),
        })
    }

    fn timeline(&self, range: MomentRange, page: PageReq) -> Result<Page<TimelineEvent>> {
        // Thread and process events come from the index; without it nothing is known.
        if let Ok(ix) = &self.inner.index {
            return ix.index.timeline(range, page);
        }
        if let Some((r, range)) = self.reference_for(range) {
            return r.indexed.index.timeline(range, page);
        }
        Ok(Page {
            items: Vec::new(),
            total: None,
            completeness: Completeness::Partial {
                pending: vec![MomentRange::all()],
            },
        })
    }

    fn index_status(&self) -> IndexStatus {
        if let Ok(r) = &self.inner.reference {
            let mut status = r.indexed.control.status();
            for (phase, _) in &mut status.phases {
                phase.push_str(" (reference run)");
            }
            return status;
        }
        match &self.inner.index {
            Ok(ix) => {
                let mut status = ix.control.status();
                // Phase-B windows run beside phase A, under their own phase name.
                if let Some((phase, done, error)) = self.inner.write_windows.status() {
                    status.phases.push(phase);
                    status.done &= done;
                    status.error = status.error.or(error);
                }
                status
            }
            // Nothing is building: queries replay on demand.
            Err(indexed::NoIndex::NotKept(_)) => IndexStatus {
                phases: Vec::new(),
                done: true,
                error: None,
            },
            Err(indexed::NoIndex::Failed(why)) => IndexStatus {
                phases: Vec::new(),
                done: true,
                error: Some(format!(
                    "the index could not be opened ({why}); queries replay on demand"
                )),
            },
        }
    }

    fn cursor(&self) -> Result<Stop> {
        Ok(lock(&self.inner.state).stop.clone())
    }

    fn step(&self, kind: StepKind, dir: Direction) -> Result<Stop> {
        if dir == Direction::Reverse && self.is_live() {
            return Err(self.inner.live_refusal("stepping backwards"));
        }
        self.navigate(History::Record, StopCause::Step, move |d, tid| {
            d.step(tid, kind, dir)
        })
    }

    fn run(&self, dir: Direction, until: Until) -> Result<Stop> {
        if dir == Direction::Reverse && self.is_live() {
            return Err(self.inner.live_refusal("running backwards"));
        }
        match until {
            Until::Breakpoints => self.navigate(History::Record, StopCause::Step, move |d, _| {
                d.continue_to_breakpoints(dir)
            }),
            Until::EndOfTrace => self.navigate(History::Record, StopCause::Step, move |d, _| {
                d.continue_to_end(dir)
            }),
            Until::Watch(place) => self.navigate(History::Record, StopCause::Step, move |d, _| {
                d.continue_to_write(place, dir)
            }),
            Until::Moment(m) => self.seek(m),
        }
    }

    fn seek(&self, to: Moment) -> Result<Stop> {
        self.inner.check_moment(&to)?;
        if self.is_live() {
            return self.live_forward_to(to);
        }
        self.navigate(History::Record, StopCause::Jump, move |d, _| {
            d.t.seek(&to)?;
            Ok(Halt::Arrived)
        })
    }

    fn back(&self) -> Result<Option<Stop>> {
        let (to, from) = {
            let st = lock(&self.inner.state);
            match st.back.last() {
                Some(m) => (*m, st.stop.at),
                None => return Ok(None),
            }
        };
        if self.is_live() {
            return Err(self.inner.live_refusal("going back in navigation history"));
        }
        let stop = self.jump(to)?;
        let mut st = lock(&self.inner.state);
        st.back.pop();
        st.forward.push(from);
        Ok(Some(stop))
    }

    fn forward(&self) -> Result<Option<Stop>> {
        let (to, from) = {
            let st = lock(&self.inner.state);
            match st.forward.last() {
                Some(m) => (*m, st.stop.at),
                None => return Ok(None),
            }
        };
        let stop = self.jump(to)?;
        let mut st = lock(&self.inner.state);
        st.forward.pop();
        st.back.push(from);
        Ok(Some(stop))
    }

    fn add_breakpoint(&self, location: BreakLocation) -> Result<UserBreakpoint> {
        let addresses = self.resolve_breakpoint(&location)?;
        if addresses.is_empty() {
            return Err(HopperError::NotFound(format!("no code at {location:?}")));
        }
        let addrs = addresses.clone();
        self.shared().pool.cursor(move |t| -> Result<()> {
            for a in addrs {
                t.insert(Breakpoint::code(a))?;
            }
            Ok(())
        })??;
        let mut st = lock(&self.inner.state);
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
        let (removed, still_used) = {
            let mut st = lock(&self.inner.state);
            let Some(i) = st.breakpoints.iter().position(|b| b.id == id) else {
                return Err(HopperError::NotFound(format!("breakpoint {}", id.0)));
            };
            let b = st.breakpoints.remove(i);
            let used: Vec<u64> = st
                .breakpoints
                .iter()
                .flat_map(|b| b.addresses.iter().copied())
                .collect();
            (b, used)
        };
        let tracepoints: Vec<u64> = self.inner.tracepoints.keys().copied().collect();
        let addrs: Vec<u64> = removed
            .addresses
            .into_iter()
            .filter(|a| !still_used.contains(a) && !tracepoints.contains(a))
            .collect();
        self.shared().pool.cursor(move |t| -> Result<()> {
            for a in addrs {
                match t.remove(Breakpoint::code(a)) {
                    Ok(()) | Err(HopperError::Disconnected(_)) => {}
                    Err(e) => return Err(e),
                }
            }
            Ok(())
        })?
    }

    fn breakpoints(&self) -> Result<Vec<UserBreakpoint>> {
        Ok(lock(&self.inner.state).breakpoints.clone())
    }

    fn frames(&self, at: Moment, tid: Tid) -> Result<Vec<Frame>> {
        at_moment(self.shared(), at, move |v| {
            Ok(v.stack(tid)?.iter().map(frame_of).collect())
        })
    }

    fn registers(&self, at: Moment, tid: Tid) -> Result<RegisterSet> {
        at_moment(self.shared(), at, move |v| v.registers(tid))
    }

    fn memory(&self, at: Moment, process: ProcessId, addr: u64, len: u64) -> Result<Vec<u8>> {
        if process != self.inner.process {
            return Err(HopperError::NotFound(format!("process {}", process.pid)));
        }
        if len > 1 << 24 {
            return Err(HopperError::Invalid(format!(
                "memory read of {len} bytes is too large"
            )));
        }
        at_moment(self.shared(), at, move |v| {
            v.memory().read_vec(addr, len as usize)
        })
    }

    fn disassemble(
        &self,
        at: Moment,
        process: ProcessId,
        addr: u64,
        count: u32,
    ) -> Result<Vec<Instruction>> {
        if process != self.inner.process {
            return Err(HopperError::NotFound(format!("process {}", process.pid)));
        }
        at_moment(self.shared(), at, move |v| {
            let syms = v.symbols()?;
            let mut mem = v.memory();
            let mut out = hopper_symbols::disasm::disassemble(&syms.set, &mut mem, addr, count)?;
            for i in &mut out {
                if i.location.is_none() {
                    i.location = syms.set.location(i.addr);
                }
            }
            Ok(out)
        })
    }

    fn locals(&self, at: Moment, frame: FrameId) -> Result<Vec<ValueView>> {
        let mut views = at_moment(self.shared(), at, move |v| {
            v.with_eval(frame, |ev, cx| ev.locals(cx))
        })?;
        self.with_ownership(at, frame, &mut views);
        Ok(views)
    }

    fn eval(&self, at: Moment, frame: FrameId, expr: &str) -> Result<ValueView> {
        let e = expr.to_string();
        let v = at_moment(self.shared(), at, move |v| {
            v.with_eval(frame, |ev, cx| ev.eval(cx, &e))
        })?;
        let mut views = [v];
        self.with_ownership(at, frame, &mut views);
        let [v] = views;
        Ok(v)
    }

    fn view(&self, value: &ValueHandle) -> Result<ValueView> {
        let h = value.clone();
        let frame = Session::handle_frame(value);
        let v = at_moment(self.shared(), value.at, move |v| {
            v.with_eval(frame, |ev, cx| ev.view(cx, &h))
        })?;
        let mut views = [v];
        self.with_ownership(value.at, frame, &mut views);
        let [v] = views;
        Ok(v)
    }

    fn children(&self, value: &ValueHandle, page: PageReq) -> Result<Vec<ValueView>> {
        let h = value.clone();
        let frame = Session::handle_frame(value);
        let mut views = at_moment(self.shared(), value.at, move |v| {
            v.with_eval(frame, |ev, cx| ev.children(cx, &h, page))
        })?;
        self.with_ownership(value.at, frame, &mut views);
        Ok(views)
    }

    fn source_text(&self, file: &str) -> Result<Arc<str>> {
        queries::source_text(file)
    }

    fn calls(&self, filter: CallFilter, page: PageReq) -> Result<Page<CallNode>> {
        if self.is_live() {
            return match &self.inner.reference {
                Ok(r) => r.indexed.index.calls(&filter, page),
                Err(_) => Err(self.inner.live_refusal("the call history")),
            };
        }
        queries::calls(self.shared(), filter, page)
    }

    fn line_hits(&self, loc: SourceLoc, range: MomentRange, page: PageReq) -> Result<Page<Moment>> {
        if self.is_live() {
            return match self.reference_for(range) {
                Some((r, range)) => r.indexed.index.line_hits(&loc, range, page),
                None => Ok(queries::live_line_hits(self, &loc, range, page)),
            };
        }
        queries::line_hits(self.shared(), loc, range, page)
    }

    fn value_history(
        &self,
        value: &ValueHandle,
        range: MomentRange,
        page: PageReq,
    ) -> Result<Page<(Moment, ValueView)>> {
        if self.is_live() {
            return Err(self.inner.live_refusal("a value's history"));
        }
        queries::value_history(self.shared(), value, range, page)
    }

    fn last_write(&self, place: Place, before: Moment) -> Result<Option<WriteEvent>> {
        if self.is_live() {
            return Err(self.inner.live_refusal("finding the last write"));
        }
        queries::last_write(self.shared(), place, before)
    }

    fn origin(&self, value: &ValueHandle, budget: Budget) -> Result<OriginChain> {
        if self.is_live() {
            return Err(self.inner.live_refusal("following a value's origin"));
        }
        queries::origin(self.shared(), value, budget)
    }

    fn allocation(
        &self,
        process: ProcessId,
        addr: u64,
        at: Moment,
    ) -> Result<Option<AllocationInfo>> {
        if self.is_live() {
            return self.reference_allocation(addr);
        }
        if process != self.inner.process {
            return Err(HopperError::NotFound(format!("process {}", process.pid)));
        }
        match &self.inner.index {
            Ok(ix) => Ok(ix.index.allocation_at(process, addr, at)?.value),
            Err(why) => Err(HopperError::Unsupported(format!(
                "allocation lineage comes from hopper-index, and this session has none ({why})"
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn _assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn session_is_send_and_sync() {
        _assert_send_sync::<Session>();
    }
}
