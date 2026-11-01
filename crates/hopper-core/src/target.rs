//! What a replay offers: the seam between the engine/indexer and rr (or a live gdbserver).
//!
//! One `Target` is one serial debugging connection to one process. rr replicas implement it in
//! `hopper-rr`; a live `gdbserver` implements it too (forward only), which is how the RSP client,
//! the evaluator and the engine are tested on machines without hardware performance counters.

use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::ids::{CheckpointId, ProcessId, RecordingId, Tid};
use crate::moment::Moment;
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
        Breakpoint { kind: BreakKind::Software, addr, len: 1 }
    }
    pub fn write_watch(addr: u64, len: u64) -> Self {
        Breakpoint { kind: BreakKind::WriteWatch, addr, len }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    /// A code breakpoint at this address.
    Breakpoint { addr: u64 },
    /// A watchpoint fired. `addr` is the watched address the target reported.
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
        Err(crate::HopperError::UnreadableMemory { addr, len: buf.len() as u64 })
    }
}
