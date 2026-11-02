//! What a replay offers: the seam between the engine/indexer and rr (or a live gdbserver).
//!
//! One `Target` is one serial debugging connection to one process. rr replicas implement it in
//! `hopper-rr`; a live `gdbserver` implements it too (forward only), which is how the RSP client,
//! the evaluator and the engine are tested on machines without hardware performance counters.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::{HopperError, Result};
use crate::ids::{CheckpointId, ProcessId, RecordingId, Tid};
use crate::moment::{Moment, RegionVisit, StopMoment};
use crate::regs::RegisterSet;

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Direction {
    Forward,
    Reverse,
}

/// How to resume.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resume {
    /// Run until something stops the target.
    Continue,
    /// Execute exactly one instruction of `tid` (in the given direction).
    Step { tid: Tid },
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BreakKind {
    Software,
    Hardware,
    WriteWatch,
    ReadWatch,
    AccessWatch,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Breakpoint {
    pub kind: BreakKind,
    pub addr: u64,
    /// Instruction length hint for code breakpoints (gdb uses 1 on x86), byte length for watches.
    pub len: u64,
}

impl Breakpoint {
    pub fn code(addr: u64) -> Self {
        Breakpoint {
            kind: BreakKind::Software,
            addr,
            len: 1,
        }
    }
    pub fn write_watch(addr: u64, len: u64) -> Self {
        Breakpoint {
            kind: BreakKind::WriteWatch,
            addr,
            len,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// A code breakpoint at this address.
    Breakpoint { addr: u64 },
    /// A watchpoint fired. `addr` is the watched address the target reported. Running forward
    /// the target stops just after the access; running in reverse, just before it (the
    /// accessing instruction is the next one forward), as rr and gdb do. So repeating a
    /// continue in either direction moves on to the next access, and after a reverse stop
    /// the watched place still holds its value from before the write.
    Watchpoint { kind: BreakKind, addr: u64 },
    /// A single step finished.
    Step,
    /// The thread got a signal (Linux numbering).
    Signal { signo: u8 },
    /// The process exited.
    Exited { code: u8 },
    /// The process was killed by a signal.
    Killed { signo: u8 },
    /// Reverse execution reached the start of the recording.
    HistoryStart,
    /// Forward execution reached the end of the recording.
    HistoryEnd,
    /// The target moved by a seek or a checkpoint restore rather than by executing: it is at
    /// the requested position and nothing stopped it.
    Arrived,
    /// Anything else the target reported, kept verbatim.
    Other { packet: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StopEvent {
    pub tid: Tid,
    pub reason: StopReason,
}

/// Static facts about a target.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TargetInfo {
    pub recording: RecordingId,
    pub process: ProcessId,
    /// The executable the process runs, as the target reports it.
    pub executable: PathBuf,
    pub can_reverse: bool,
    pub can_seek: bool,
    /// rr's version string, when the target is rr; `None` for other servers.
    pub rr_version: Option<String>,
}

/// A thread appearing or going away.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThreadChange {
    Started,
    Exited,
}

/// When a [`ThreadEvent`] happened.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThreadEventTime {
    /// The server reported the event as it happened; this is the moment of that report.
    At(Moment),
    /// Seen by comparing thread lists: after the previous stop, at or before the stop the target
    /// is at now.
    ByCurrentStop,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ThreadEvent {
    pub tid: Tid,
    pub change: ThreadChange,
    pub time: ThreadEventTime,
}

/// How a target observes thread starts and exits ([`Target::enable_thread_events`]).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ThreadEventSource {
    /// The server reports each start and exit when it happens (gdbserver's `QThreadEvents`).
    ServerReports,
    /// The target compares the thread list at every forward stop: a thread that starts and
    /// exits between two stops is not seen (rr, which has no thread events).
    ThreadListDiff,
    /// The target does not observe them.
    Unavailable,
}

/// One serial debugging connection to one process.
pub trait Target: Send + fmt::Debug {
    fn info(&self) -> &TargetInfo;
    fn threads(&mut self) -> Result<Vec<Tid>>;
    fn current_thread(&mut self) -> Result<Tid>;
    fn registers(&mut self, tid: Tid) -> Result<RegisterSet>;
    /// Fill `buf` from `addr`. Fails if any byte is unreadable.
    fn read_memory(&mut self, addr: u64, buf: &mut [u8]) -> Result<()>;
    fn insert(&mut self, bp: Breakpoint) -> Result<()>;
    fn remove(&mut self, bp: Breakpoint) -> Result<()>;
    fn resume(&mut self, how: Resume, dir: Direction) -> Result<StopEvent>;
    /// The current moment.
    fn now(&mut self) -> Result<Moment>;
    /// Move to `to`. Targets without seeking return `Unsupported`.
    fn seek(&mut self, to: &Moment) -> Result<StopEvent>;
    fn checkpoint(&mut self) -> Result<CheckpointId>;
    fn restore(&mut self, id: CheckpointId) -> Result<StopEvent>;
    /// The raw auxiliary vector (`qXfer:auxv:read`), used to find the load bias.
    fn auxv(&mut self) -> Result<Vec<u8>>;

    /// The current moment, cheaply when the target can name it without measuring: an rr replica
    /// stopped at a breakpoint answers with a [`RegionVisit`] instead of counting `sub` (which
    /// costs a seek). Callers that keep the answer resolve visits later, in bulk, with
    /// [`Target::resolve_visits`]. The default is [`Target::now`].
    fn now_at_stop(&mut self) -> Result<StopMoment> {
        self.now().map(StopMoment::Exact)
    }

    /// The moments of `visits`, in the same order. The target ends where it was before the call
    /// (an rr replica past the end of its recording ends at the last visit instead). The default
    /// resolves nothing: targets that never answer [`Target::now_at_stop`] with a visit need no
    /// resolution.
    fn resolve_visits(&mut self, visits: &[RegionVisit]) -> Result<Vec<Moment>> {
        if visits.is_empty() {
            Ok(Vec::new())
        } else {
            Err(HopperError::Unsupported(
                "this target does not name moments by region visits".into(),
            ))
        }
    }

    /// Start observing thread starts and exits; say how. Afterwards
    /// [`Target::take_thread_events`] returns what each resume saw. Seeks, restores and reverse
    /// execution produce no events (the thread list is re-read as the new baseline).
    fn enable_thread_events(&mut self) -> Result<ThreadEventSource> {
        Ok(ThreadEventSource::Unavailable)
    }

    /// The thread events observed since the last call, oldest first.
    fn take_thread_events(&mut self) -> Result<Vec<ThreadEvent>> {
        Ok(Vec::new())
    }

    /// The thread the target executes next when every thread may run, when it follows a fixed
    /// schedule (an rr replay runs one thread at a time in the recorded order). Single-stepping
    /// this thread each time steps every thread's instructions in execution order. `None` when
    /// the target has no such schedule (a live process).
    fn scheduled_thread(&mut self) -> Result<Option<Tid>> {
        Ok(None)
    }
}

/// Memory as the evaluator and unwinder see it.
pub trait Memory {
    /// Fill `buf` from `addr`. Fails if any byte is unreadable.
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<()>;

    fn read_u64(&mut self, addr: u64) -> Result<u64> {
        let mut b = [0u8; 8];
        self.read(addr, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }

    fn read_vec(&mut self, addr: u64, len: usize) -> Result<Vec<u8>> {
        let mut v = vec![0u8; len];
        self.read(addr, &mut v)?;
        Ok(v)
    }
}

/// Reads memory straight from a target.
#[derive(Debug)]
pub struct TargetMemory<'a>(pub &'a mut dyn Target);

impl Memory for TargetMemory<'_> {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        self.0.read_memory(addr, buf)
    }
}

/// A flat byte image, for tests.
#[derive(Debug, Clone, Default)]
pub struct ImageMemory {
    pub regions: Vec<(u64, Vec<u8>)>,
}

impl ImageMemory {
    pub fn with(mut self, addr: u64, bytes: Vec<u8>) -> Self {
        self.regions.push((addr, bytes));
        self
    }
}

impl Memory for ImageMemory {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        for (base, bytes) in &self.regions {
            let end = base + bytes.len() as u64;
            if addr >= *base && addr + buf.len() as u64 <= end {
                let off = (addr - base) as usize;
                buf.copy_from_slice(&bytes[off..off + buf.len()]);
                return Ok(());
            }
        }
        Err(crate::HopperError::UnreadableMemory {
            addr,
            len: buf.len() as u64,
        })
    }
}
