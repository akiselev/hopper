//! The portable phase-B producer: single-step a target and decode every instruction.

use std::time::Instant;

use hopper_core::target::Memory;
use hopper_core::target::{Direction, Resume, StopReason, Target, TargetMemory};
use hopper_core::{HopperError, Moment, Place, RegisterSet, Result, Tid};

use super::decode::{MAX_INSTRUCTION_LEN, decode_at, instruction_writes};
use super::{WriteLogSummary, WriteValue};
use crate::control::IndexControl;
use crate::index::{Index, IndexKind};
use crate::store::{Batch, CoverageScope, CoveredInterval, Record, Stamp};

/// Where a stepping window ends (the first condition reached, checked after each step).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowEnd {
    /// At or past this moment.
    Moment(Moment),
    /// When the stepped thread's PC reaches this address.
    Pc(u64),
    /// After this many instructions.
    Steps(u64),
    /// When the process exits (or the recording ends).
    ProcessExit,
}

/// A window to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepWindow {
    /// The thread to step; `None`: the target's current thread.
    pub tid: Option<Tid>,
    /// Seek here first (targets that can seek); `None`: start where the target is. A target
    /// that cannot seek must already be there.
    pub start: Option<Moment>,
    pub end: WindowEnd,
    /// A hard bound on steps whatever `end` says; reaching it ends the window early (the log
    /// is still correct up to there, and coverage says so).
    pub max_steps: u64,
    /// Writes buffered before a chunk is committed.
    pub flush_every: usize,
}

impl StepWindow {
    pub fn new(end: WindowEnd) -> Self {
        StepWindow {
            tid: None,
            start: None,
            end,
            max_steps: 10_000_000,
            flush_every: 4096,
        }
    }
}

/// The largest single write whose bytes are read back (bigger ones, such as `rep stos` runs,
/// are logged with [`WriteValue::Unrecorded`]).
const MAX_READ_BACK: u64 = 4096;

fn read_code(target: &mut dyn Target, pc: u64) -> Option<Vec<u8>> {
    // The instruction may end just before an unmapped page: try shorter reads.
    for len in (1..=MAX_INSTRUCTION_LEN).rev() {
        let mut buf = vec![0u8; len];
        if target.read_memory(pc, &mut buf).is_ok() {
            return Some(buf);
        }
        if len > 1 && (pc & 0xfff) as usize + len <= 0x1000 {
            // Inside one page: a shorter read fails too.
            break;
        }
    }
    None
}

struct Run<'a> {
    index: &'a Index,
    pending: Vec<Record>,
    interval: CoveredInterval,
    flush_every: usize,
}

impl Run<'_> {
    fn flush(&mut self, through: Option<Moment>) -> Result<()> {
        if let Some(t) = through {
            self.interval.through = Some(Stamp::of(&t));
        }
        let mut cov = self.index.coverage(IndexKind::Writes)?;
        // Replace this run's interval (identified by its start and scope) or add it.
        match cov
            .intervals
            .iter_mut()
            .find(|iv| iv.from == self.interval.from && iv.scope == self.interval.scope)
        {
            Some(iv) => *iv = self.interval,
            None => cov.intervals.push(self.interval),
        }
        self.index.commit(Batch {
            chunks: vec![(IndexKind::Writes, std::mem::take(&mut self.pending))],
            coverage: vec![(IndexKind::Writes, cov)],
            resume: None,
        })
    }
}

/// Single-step `window.tid` through `window` and log every memory write into `index`.
///
/// The window's coverage is recorded for every thread when the process has one thread at the
/// start (nothing else can write), and for the stepped thread only otherwise: other threads'
/// writes in the window are not seen, and queries report them as pending.
pub fn log_writes_by_stepping(
    target: &mut dyn Target,
    window: &StepWindow,
    index: &Index,
    control: &IndexControl,
) -> Result<WriteLogSummary> {
    let started = Instant::now();
    if let Some(start) = &window.start {
        if target.info().can_seek {
            target.seek(start)?;
        } else if target.now()? != *start {
            return Err(HopperError::Unsupported(format!(
                "the target cannot seek to {start}; position it at the window start first"
            )));
        }
    }
    let tid = match window.tid {
        Some(t) => t,
        None => target.current_thread()?,
    };
    let process = target.info().process;
    let first = target.now()?;
    let scope = if target.threads()?.len() <= 1 {
        CoverageScope::AllThreads
    } else {
        CoverageScope::Thread(tid)
    };
    let mut run = Run {
        index,
        pending: Vec::new(),
        interval: CoveredInterval {
            from: Some(Stamp::of(&first)),
            through: Some(Stamp::of(&first)),
            scope,
        },
        flush_every: window.flush_every.max(1),
    };
    let mut summary = WriteLogSummary {
        instructions: 0,
        writes: 0,
        undecoded: 0,
        first: Some(first),
        last: Some(first),
        elapsed: Default::default(),
    };
    let mut at = first;
    let mut before: RegisterSet = target.registers(tid)?;
    let mut decoded: std::collections::HashMap<u64, Option<iced_x86::Instruction>> =
        Default::default();
    loop {
        if control.is_cancelled() {
            run.flush(Some(at))?;
            return Err(HopperError::Cancelled);
        }
        let pc = before
            .pc()
            .ok_or_else(|| HopperError::Protocol("the register set has no rip".into()))?;
        // Code does not change inside a window (no JIT in the programs hopper targets), so each
        // address is read and decoded once.
        let insn = match decoded.get(&pc) {
            Some(i) => *i,
            None => {
                let i = read_code(target, pc).and_then(|b| decode_at(&b, pc));
                decoded.insert(pc, i);
                i
            }
        };
        let ev = match target.resume(Resume::Step { tid }, Direction::Forward) {
            Ok(ev) => ev,
            // The stepped thread exited under the step (gdbserver then refuses the step).
            Err(HopperError::TargetRefused { .. }) if !target.threads()?.contains(&tid) => {
                run.flush(Some(at))?;
                break;
            }
            Err(e) => return Err(e),
        };
        let ended = !matches!(ev.reason, StopReason::Step);
        summary.instructions += 1;
        let after_regs = if ended {
            None
        } else {
            Some(target.registers(tid)?)
        };
        match &insn {
            Some(insn) => {
                for site in instruction_writes(insn, &before, after_regs.as_ref()) {
                    let value = match (&site.value, ended) {
                        (_, false) if site.len <= MAX_READ_BACK => {
                            let mut mem = TargetMemory(target);
                            match mem.read_vec(site.addr, site.len as usize) {
                                Ok(b) => WriteValue::Bytes(b),
                                Err(_) => match &site.value {
                                    Some(v) => WriteValue::Bytes(v.clone()),
                                    None => WriteValue::Unrecorded,
                                },
                            }
                        }
                        (Some(v), _) => WriteValue::Bytes(v.clone()),
                        (None, _) => WriteValue::Unrecorded,
                    };
                    run.pending.push(Record::Write {
                        at: Stamp::of(&at),
                        pc,
                        place: Place::memory(process, site.addr, site.len),
                        value,
                    });
                    summary.writes += 1;
                }
            }
            None => summary.undecoded += 1,
        }
        let Some(after) = after_regs else {
            run.flush(None)?;
            break;
        };
        at = target.now()?;
        summary.last = Some(at);
        if run.pending.len() >= run.flush_every {
            run.flush(Some(at))?;
        }
        let done = match &window.end {
            WindowEnd::Moment(m) => !at.is_before(m),
            WindowEnd::Pc(p) => after.pc() == Some(*p),
            WindowEnd::Steps(n) => summary.instructions >= *n,
            WindowEnd::ProcessExit => false,
        };
        before = after;
        if done || summary.instructions >= window.max_steps {
            run.flush(Some(at))?;
            break;
        }
    }
    summary.elapsed = started.elapsed();
    Ok(summary)
}
