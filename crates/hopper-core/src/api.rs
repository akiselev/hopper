//! The one engine API.
//!
//! [`Debugger`] is what every front end talks to: the GUI, the CLI, the MCP server and the DAP
//! adapter. `hopper-engine` implements it over rr; `hopper-fake` implements it over canned data
//! so the front ends can be built and tested without a recording.
//!
//! Rules this API keeps:
//! - Every position is a [`Moment`]. Nothing returns "the current position" implicitly except
//!   [`Debugger::cursor`].
//! - A [`ValueHandle`] never expires. It names "this expression (or place), in this frame, at this
//!   moment", so it can be re-asked at another moment ([`Debugger::value_history`]).
//! - Omniscient queries say whether their answer is complete ([`Completeness`]): the index may
//!   still be building.

use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::{FrameId, ProcessId, RecordingId, Tid};
use crate::moment::{Moment, MomentRange};
use crate::place::Place;
use crate::regs::RegisterSet;
use crate::source::SourceLoc;

// ---------------------------------------------------------------- recording and processes

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingInfo {
    pub id: RecordingId,
    /// The rr trace directory, or a description for live targets.
    pub path: String,
    pub rr_version: Option<String>,
    /// The first and last moments of the recording, when known.
    pub start: Option<Moment>,
    pub end: Option<Moment>,
    pub processes: Vec<ProcessInfo>,
    /// Other runs of the same program that answer the omniscient queries of a session whose own
    /// run cannot: a live process runs forward only, so its call tree and line hits come from an
    /// indexed reference run, and its origins from stepped windows of further runs. Moments in
    /// those answers carry the other run's recording id, not [`Self::id`]: they name states of
    /// that run, cannot be sought here, and are shown as such.
    #[serde(default)]
    pub reference_runs: Vec<ReferenceRun>,
}

/// A run, other than the session's own, whose index answers the session's queries.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReferenceRun {
    /// The recording id the reference run's moments carry.
    pub id: RecordingId,
    /// What the run is ("a second gdbserver run of <program> with the same arguments").
    pub description: String,
}

impl RecordingInfo {
    /// Whether `m` is a moment of a reference run rather than of this recording.
    pub fn is_reference_moment(&self, m: &Moment) -> bool {
        m.recording != self.id && self.reference_run_of(m).is_some()
    }

    /// The reference run `m` belongs to.
    pub fn reference_run_of(&self, m: &Moment) -> Option<&ReferenceRun> {
        self.reference_runs.iter().find(|r| r.id == m.recording)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProcessInfo {
    pub id: ProcessId,
    pub executable: String,
    pub threads: Vec<Tid>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TimelineKind {
    Syscall {
        name: String,
    },
    Signal {
        signo: u8,
    },
    Exec {
        executable: String,
    },
    ThreadStart,
    ThreadExit,
    ProcessExit {
        code: Option<i32>,
    },
    /// A named semantic event, such as a cadabra3 `site!` decision, found by the index.
    Anchor {
        name: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub at: Moment,
    pub kind: TimelineKind,
}

// ---------------------------------------------------------------- navigation

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum StepKind {
    /// One source line, entering calls.
    Into,
    /// One source line, stepping over calls.
    Over,
    /// Until the current function returns (forward) or is entered (reverse).
    Out,
    /// One machine instruction.
    Instruction,
}

pub use crate::target::Direction;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Until {
    /// The next user breakpoint (or the end of history).
    Breakpoints,
    /// The next write to this place.
    Watch(Place),
    /// Exactly this moment.
    Moment(Moment),
    /// The end (forward) or start (reverse) of the recording.
    EndOfTrace,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BreakpointId(pub u32);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BreakLocation {
    Source(SourceLoc),
    Function(String),
    Address(u64),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserBreakpoint {
    pub id: BreakpointId,
    pub location: BreakLocation,
    /// The addresses the location resolved to (several for inlined or generic code).
    pub addresses: Vec<u64>,
    pub enabled: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopCause {
    Step,
    Breakpoint {
        id: BreakpointId,
    },
    Watchpoint {
        place: Place,
    },
    Signal {
        signo: u8,
    },
    Exited {
        code: u8,
    },
    HistoryStart,
    HistoryEnd,
    /// The cursor moved by seek, back/forward or opening a position URL.
    Jump,
}

/// Where the cursor is after a navigation command.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stop {
    pub at: Moment,
    pub tid: Tid,
    pub pc: u64,
    pub cause: StopCause,
    pub function: Option<String>,
    pub location: Option<SourceLoc>,
}

// ---------------------------------------------------------------- frames and values

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    pub id: FrameId,
    pub pc: u64,
    /// Canonical frame address, when unwinding found one.
    pub cfa: Option<u64>,
    pub function: Option<String>,
    pub location: Option<SourceLoc>,
    /// The frame is an inlined call inside the next outer frame.
    pub inlined: bool,
}

/// Identifies a type in the debug information (opaque; stable within one binary).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct TypeKey(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PathElem {
    Field(String),
    Index(u64),
    Deref,
    Variant(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ValueTarget {
    /// A source expression, evaluated in the handle's frame.
    Expr(String),
    /// A fixed place of a known type.
    Place { place: Place, ty: TypeKey },
}

/// "This expression (or place), in this frame, at this moment". Never expires.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ValueHandle {
    pub at: Moment,
    pub process: ProcessId,
    pub frame: Option<FrameId>,
    pub target: ValueTarget,
    /// Path from the target to this value (empty for the target itself).
    pub path: Vec<PathElem>,
}

impl ValueHandle {
    /// The same value, asked at another moment.
    pub fn at(&self, at: Moment) -> ValueHandle {
        ValueHandle { at, ..self.clone() }
    }

    pub fn child(&self, elem: PathElem) -> ValueHandle {
        let mut h = self.clone();
        h.path.push(elem);
        h
    }
}

/// A value rendered for display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValueView {
    pub handle: ValueHandle,
    /// The variable, field or index name ("x", "name", "[3]").
    pub name: String,
    pub type_name: String,
    /// One-line rendering ("\"alpha\"", "42", "Vec(len=3)", "<optimised out>").
    pub summary: String,
    /// Number of children; `None` for leaves.
    pub child_count: Option<u64>,
    /// Where the value's bytes are, when they are in memory or one register.
    pub place: Option<Place>,
    pub ownership: OwnershipState,
}

// ---------------------------------------------------------------- ownership

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Confidence {
    /// The sidecar event matched the machine copy exactly (debug builds).
    Exact,
    /// The sidecar event was chosen among several by matching bytes or locations.
    Inferred,
    /// No machine copy exists; the label is a rename by source position only.
    Heuristic,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum OwnershipKind {
    Move,
    Copy,
    Clone,
    SharedBorrow,
    MutBorrow,
    /// A field, index or deref projection of another place.
    Projection,
    Drop,
    /// Built by an aggregate expression (`Config { .. }`, a tuple, an array).
    Aggregate,
    /// A literal or constant.
    Literal,
    /// The return value of a call.
    CallReturn,
    StorageDead,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OwnershipLabel {
    pub kind: OwnershipKind,
    /// Source-level place the value came from (`a`, `config.name`, `v[0]`).
    pub src: Option<String>,
    /// Source-level place it went to.
    pub dst: Option<String>,
    pub span: Option<SourceLoc>,
    pub confidence: Confidence,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OwnershipState {
    /// The variable holds a live value.
    Live,
    /// The value was moved out; the variable's bytes are stale.
    MovedOut {
        to: Option<String>,
        at: Option<SourceLoc>,
        moment: Option<Moment>,
    },
    /// The value was dropped.
    Dropped {
        at: Option<SourceLoc>,
        moment: Option<Moment>,
    },
    /// No ownership information (no sidecar, not Rust, or not a local).
    Unknown,
}

// ---------------------------------------------------------------- omniscient queries

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Completeness {
    Complete,
    /// The index has not covered these ranges yet; results there are missing.
    Partial {
        pending: Vec<MomentRange>,
    },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageReq {
    pub offset: u64,
    pub limit: u32,
}

impl PageReq {
    pub fn first(limit: u32) -> Self {
        PageReq { offset: 0, limit }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    /// Total number of items, when known.
    pub total: Option<u64>,
    pub completeness: Completeness,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CallId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallNode {
    pub id: CallId,
    pub parent: Option<CallId>,
    pub tid: Tid,
    pub function: String,
    pub entry: Moment,
    /// `None` when the call never returned inside the recording.
    pub exit: Option<Moment>,
    pub depth: u32,
    pub child_count: u64,
    pub location: Option<SourceLoc>,
}

#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct CallFilter {
    /// Children of this call. `None` means the roots, unless `function` is set: then the whole
    /// tree is searched.
    pub parent: Option<CallId>,
    /// Only calls of functions whose name contains this text. With `parent: None` this searches
    /// every call in the recording, not just the roots.
    pub function: Option<String>,
    pub tid: Option<Tid>,
    pub process: Option<ProcessId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WriteEvent {
    pub at: Moment,
    pub pc: u64,
    pub place: Place,
    /// The bytes the place held after the write.
    pub value: Vec<u8>,
    pub function: Option<String>,
    pub location: Option<SourceLoc>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum MachineKind {
    /// Bytes copied unchanged (mov, memcpy, push/pop).
    Copy,
    /// The written bytes were computed from the read operands.
    Arithmetic,
    /// The bytes came from an immediate operand or read-only data.
    Immediate,
    /// The kernel wrote the bytes (a syscall result or input).
    Syscall,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginHop {
    pub at: Moment,
    pub pc: u64,
    pub function: Option<String>,
    pub location: Option<SourceLoc>,
    /// Where the bytes were read from, when the instruction read them from one place.
    pub read: Option<Place>,
    pub written: Place,
    pub machine: MachineKind,
    /// The source-level ownership event this hop is part of (the hops one event makes, such as
    /// a move's copies through temporaries and a callee's stores, carry the same label).
    pub ownership: Option<OwnershipLabel>,
    /// The heap allocation `written` belongs to, when it is on the heap.
    pub allocation: Option<AllocationId>,
    /// The events underneath [`Self::ownership`], outermost first: for a call's return value,
    /// how the callee built it (`Config { name, .. }` with `name` moved in); for a call, its
    /// literal arguments.
    #[serde(default)]
    pub inner: Vec<OwnershipLabel>,
    /// For an arithmetic hop ([`MachineKind::Arithmetic`]): the places the instruction computed
    /// the written bytes from (registers of the writing frame, memory it read), in operand
    /// order. Empty for other kinds.
    #[serde(default)]
    pub operands: Vec<Place>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum OriginEnd {
    /// The chain reached a literal or read-only data.
    Literal,
    /// The chain reached bytes the kernel wrote (input).
    Input { syscall: Option<String> },
    /// The chain reached a computation; its operands are in the last hop.
    Computed,
    /// The chain reached the start of the recording.
    HistoryStart,
    /// The budget ran out.
    Budget,
    /// The chain could not continue (for example an untracked write).
    Unknown { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginChain {
    /// From the value back to its origin: `hops[0]` wrote the bytes the user clicked.
    pub hops: Vec<OriginHop>,
    pub end: OriginEnd,
    /// The writes that made the references the value was reached through, outermost first:
    /// for `*n`, the store of `n` (`n = &v[0].name`, a shared borrow). They wrote the pointer,
    /// not the value's bytes, so they are not in [`Self::hops`]; the source view puts them
    /// first.
    #[serde(default)]
    pub references: Vec<OriginHop>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Budget {
    pub max_hops: u32,
    /// Bound on target operations (reverse continues, steps) the search may spend.
    pub max_steps: u64,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_hops: 32,
            max_steps: 10_000,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct AllocationId(pub u64);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationInfo {
    pub id: AllocationId,
    pub process: ProcessId,
    pub addr: u64,
    pub size: u64,
    pub allocated: Moment,
    pub freed: Option<Moment>,
    /// The allocation this one was `realloc`ed from.
    pub predecessor: Option<AllocationId>,
    /// The function that requested it (the first non-allocator frame), when known.
    pub requested_by: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct IndexStatus {
    /// Phase names and their progress, for example ("calls", 0.4).
    pub phases: Vec<(String, f32)>,
    pub done: bool,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instruction {
    pub addr: u64,
    pub bytes: Vec<u8>,
    pub text: String,
    pub location: Option<SourceLoc>,
}

// ---------------------------------------------------------------- the trait

/// The engine API. All methods take `&self`: implementations are internally synchronised and
/// may be called from any thread. Calls that need the replay block until it answers.
pub trait Debugger: Send + Sync {
    // recording
    fn recording(&self) -> Result<RecordingInfo>;
    fn timeline(&self, range: MomentRange, page: PageReq) -> Result<Page<TimelineEvent>>;
    fn index_status(&self) -> IndexStatus;

    // the cursor
    fn cursor(&self) -> Result<Stop>;
    fn step(&self, kind: StepKind, dir: Direction) -> Result<Stop>;
    fn run(&self, dir: Direction, until: Until) -> Result<Stop>;
    fn seek(&self, to: Moment) -> Result<Stop>;
    /// Navigation history: undo the last cursor move.
    fn back(&self) -> Result<Option<Stop>>;
    /// Navigation history: redo.
    fn forward(&self) -> Result<Option<Stop>>;

    // breakpoints
    fn add_breakpoint(&self, location: BreakLocation) -> Result<UserBreakpoint>;
    fn remove_breakpoint(&self, id: BreakpointId) -> Result<()>;
    fn breakpoints(&self) -> Result<Vec<UserBreakpoint>>;

    // state at any moment
    fn frames(&self, at: Moment, tid: Tid) -> Result<Vec<Frame>>;
    fn registers(&self, at: Moment, tid: Tid) -> Result<RegisterSet>;
    fn memory(&self, at: Moment, process: ProcessId, addr: u64, len: u64) -> Result<Vec<u8>>;
    fn disassemble(
        &self,
        at: Moment,
        process: ProcessId,
        addr: u64,
        count: u32,
    ) -> Result<Vec<Instruction>>;
    fn locals(&self, at: Moment, frame: FrameId) -> Result<Vec<ValueView>>;
    fn eval(&self, at: Moment, frame: FrameId, expr: &str) -> Result<ValueView>;
    fn view(&self, value: &ValueHandle) -> Result<ValueView>;
    fn children(&self, value: &ValueHandle, page: PageReq) -> Result<Vec<ValueView>>;
    fn source_text(&self, file: &str) -> Result<Arc<str>>;

    // omniscient queries
    fn calls(&self, filter: CallFilter, page: PageReq) -> Result<Page<CallNode>>;
    fn line_hits(&self, loc: SourceLoc, range: MomentRange, page: PageReq) -> Result<Page<Moment>>;
    fn value_history(
        &self,
        value: &ValueHandle,
        range: MomentRange,
        page: PageReq,
    ) -> Result<Page<(Moment, ValueView)>>;
    fn last_write(&self, place: Place, before: Moment) -> Result<Option<WriteEvent>>;
    fn origin(&self, value: &ValueHandle, budget: Budget) -> Result<OriginChain>;
    fn allocation(
        &self,
        process: ProcessId,
        addr: u64,
        at: Moment,
    ) -> Result<Option<AllocationInfo>>;
}
