//! The portable phase-B producer: single-step a target and decode every instruction.
//!
//! **Which threads.** With [`SteppedThreads::All`] and a target that follows a fixed schedule
//! ([`Target::scheduled_thread`]: an rr replay, which runs one thread at a time in the recorded
//! order), each step steps the thread the target runs next, so every thread's instructions are
//! stepped in execution order and the window covers every thread. A live process
//! (gdbserver) has no such schedule, and phase B does not impose one: stepping its threads in
//! turn would stop the others whenever the stepped one blocks in a system call (a futex wait on
//! a lock another thread holds never returns while that thread is stopped), and letting the
//! others run during such a step (`vCont;s:<t>;c`) would leave their writes unseen. So on a
//! live target phase B steps one thread, and the window covers that thread only (unless it is
//! the only one).

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

/// Which threads a window steps.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SteppedThreads {
    /// Only the window's thread; the others' writes are not seen.
    One,
    /// Every thread in execution order, when the target follows a fixed schedule (see the
    /// module docs); otherwise the window's thread only.
    All,
}

/// A window to log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StepWindow {
    /// The window's thread (whose PC [`WindowEnd::Pc`] watches, and the one stepped under
    /// [`SteppedThreads::One`]); `None`: the target's current thread.
    pub tid: Option<Tid>,
    pub threads: SteppedThreads,
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
            threads: SteppedThreads::All,
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

/// Single-step through `window` and log every memory write into `index`.
///
/// The window's coverage is recorded for every thread when every thread is stepped (see the
/// module docs) or the process has one thread at the start (nothing else can write), and for
/// the stepped thread only otherwise: other threads' writes in the window are not seen, and
/// queries report them as pending.
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
    let window_tid = match window.tid {
        Some(t) => t,
        None => target.current_thread()?,
    };
    let process = target.info().process;
    let first = target.now()?;
    let scheduled = match window.threads {
        SteppedThreads::All => target.scheduled_thread()?,
        SteppedThreads::One => None,
    };
    let every_thread = scheduled.is_some();
    let scope = if every_thread || target.threads()?.len() <= 1 {
        CoverageScope::AllThreads
    } else {
        CoverageScope::Thread(window_tid)
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
    let mut tid = scheduled.unwrap_or(window_tid);
    let mut before: RegisterSet = target.registers(tid)?;
    let mut decoded: std::collections::HashMap<u64, Option<iced_x86::Instruction>> =
        Default::default();
    loop {
        if control.is_cancelled() {
            run.flush(Some(at))?;
            return Err(HopperError::Cancelled);
        }
        if every_thread
            && let Some(next) = target.scheduled_thread()?
            && next != tid
        {
            tid = next;
            before = target.registers(tid)?;
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
            // The stepped thread exited under the step (gdbserver then refuses the step); with
            // every thread stepped, the next one goes on.
            Err(HopperError::TargetRefused { .. }) if !target.threads()?.contains(&tid) => {
                if every_thread && target.scheduled_thread()?.is_some_and(|t| t != tid) {
                    continue;
                }
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
            WindowEnd::Pc(p) => tid == window_tid && after.pc() == Some(*p),
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::RecordingMeta;
    use hopper_core::api::PageReq;
    use hopper_core::target::{Breakpoint, StopEvent, TargetInfo};
    use hopper_core::{CheckpointId, MomentRange, ProcessId, RecordingId, Reg};

    const REC: RecordingId = RecordingId([0x42; 16]);
    const A: Tid = Tid(10);
    const B: Tid = Tid(11);
    /// `mov qword ptr [rdi], rsi`, repeated.
    const STORE: [u8; 3] = [0x48, 0x89, 0x37];
    const CODE: u64 = 0x40_0000;

    /// Two threads, each storing an increasing counter (rsi) through its own pointer (rdi), run
    /// one instruction at a time in the order A, A, B, A, B, B, ... like an rr replay.
    #[derive(Debug)]
    struct TwoThreads {
        info: TargetInfo,
        regs: [RegisterSet; 2],
        memory: std::collections::HashMap<u64, u8>,
        schedule: Vec<Tid>,
        executed: usize,
        follows_schedule: bool,
    }

    impl TwoThreads {
        fn new(follows_schedule: bool) -> TwoThreads {
            let regs = |rdi: u64, rsi: u64| {
                let mut r = RegisterSet::default();
                r.set(Reg::RIP, CODE);
                r.set(Reg::RDI, rdi);
                r.set(Reg::RSI, rsi);
                r
            };
            TwoThreads {
                info: TargetInfo {
                    recording: REC,
                    process: ProcessId::new(10),
                    executable: Default::default(),
                    can_reverse: false,
                    can_seek: false,
                    rr_version: None,
                },
                regs: [regs(0x1000, 100), regs(0x2000, 200)],
                memory: Default::default(),
                schedule: [A, A, B, A, B, B, A, B].repeat(4),
                executed: 0,
                follows_schedule,
            }
        }

        fn slot(tid: Tid) -> usize {
            usize::from(tid == B)
        }
    }

    impl Target for TwoThreads {
        fn info(&self) -> &TargetInfo {
            &self.info
        }
        fn threads(&mut self) -> Result<Vec<Tid>> {
            Ok(vec![A, B])
        }
        fn current_thread(&mut self) -> Result<Tid> {
            Ok(self.schedule[self.executed.min(self.schedule.len() - 1)])
        }
        fn registers(&mut self, tid: Tid) -> Result<RegisterSet> {
            Ok(self.regs[Self::slot(tid)].clone())
        }
        fn read_memory(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
            for (i, b) in buf.iter_mut().enumerate() {
                let a = addr + i as u64;
                *b = if (CODE..CODE + 0x1000).contains(&a) {
                    STORE[((a - CODE) % 3) as usize]
                } else {
                    *self.memory.get(&a).unwrap_or(&0)
                };
            }
            Ok(())
        }
        fn insert(&mut self, _: Breakpoint) -> Result<()> {
            Ok(())
        }
        fn remove(&mut self, _: Breakpoint) -> Result<()> {
            Ok(())
        }
        fn resume(&mut self, how: Resume, _: Direction) -> Result<StopEvent> {
            let Resume::Step { tid } = how else {
                return Err(HopperError::Unsupported("continue".into()));
            };
            if self.executed >= self.schedule.len() {
                return Ok(StopEvent {
                    tid,
                    reason: StopReason::Exited { code: 0 },
                });
            }
            // Like rr: whatever was asked, the scheduled thread runs.
            let runs = self.schedule[self.executed];
            self.executed += 1;
            let r = &mut self.regs[Self::slot(runs)];
            let (rdi, rsi) = (r.get(Reg::RDI).unwrap(), r.get(Reg::RSI).unwrap());
            r.set(Reg::RIP, r.pc().unwrap() + 3);
            r.set(Reg::RSI, rsi + 1);
            for (i, b) in rsi.to_le_bytes().into_iter().enumerate() {
                self.memory.insert(rdi + i as u64, b);
            }
            Ok(StopEvent {
                tid: runs,
                reason: StopReason::Step,
            })
        }
        fn now(&mut self) -> Result<Moment> {
            Ok(Moment {
                recording: REC,
                frame_time: 1,
                tid: A,
                ticks: 0,
                sub: self.executed as u32,
            })
        }
        fn seek(&mut self, _: &Moment) -> Result<StopEvent> {
            Err(HopperError::Unsupported("seek".into()))
        }
        fn checkpoint(&mut self) -> Result<CheckpointId> {
            Err(HopperError::Unsupported("checkpoint".into()))
        }
        fn restore(&mut self, _: CheckpointId) -> Result<StopEvent> {
            Err(HopperError::Unsupported("restore".into()))
        }
        fn auxv(&mut self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn scheduled_thread(&mut self) -> Result<Option<Tid>> {
            Ok(self
                .follows_schedule
                .then(|| self.schedule[self.executed.min(self.schedule.len() - 1)]))
        }
    }

    fn logged(follows_schedule: bool, threads: SteppedThreads) -> (Vec<(u64, u64)>, Coverage) {
        let tmp = tempfile::tempdir().unwrap();
        let index = Index::open(
            tmp.path(),
            RecordingMeta {
                recording: REC,
                build_ids: vec![],
            },
        )
        .unwrap();
        let mut t = TwoThreads::new(follows_schedule);
        let mut w = StepWindow::new(WindowEnd::Steps(16));
        w.tid = Some(A);
        w.threads = threads;
        log_writes_by_stepping(&mut t, &w, &index, &IndexControl::new()).unwrap();
        let p = |addr| Place::memory(ProcessId::new(10), addr, 8);
        let mut out = Vec::new();
        for addr in [0x1000, 0x2000] {
            for lw in index
                .writes_in(p(addr), MomentRange::all(), PageReq::first(100))
                .unwrap()
                .items
            {
                let WriteValue::Bytes(b) = lw.value else {
                    panic!("bytes are read back")
                };
                out.push((addr, u64::from_le_bytes(b.try_into().unwrap())));
            }
        }
        (out, index.coverage(IndexKind::Writes).unwrap())
    }

    use crate::store::Coverage;

    #[test]
    fn steps_every_thread_in_schedule_order_where_the_target_has_one() {
        let (writes, cov) = logged(true, SteppedThreads::All);
        // 16 instructions of the schedule: A runs 8 of them, B 8.
        let a: Vec<u64> = writes
            .iter()
            .filter(|w| w.0 == 0x1000)
            .map(|w| w.1)
            .collect();
        let b: Vec<u64> = writes
            .iter()
            .filter(|w| w.0 == 0x2000)
            .map(|w| w.1)
            .collect();
        assert_eq!(a, (100..108).collect::<Vec<_>>());
        assert_eq!(b, (200..208).collect::<Vec<_>>());
        assert_eq!(cov.intervals[0].scope, CoverageScope::AllThreads);
    }

    #[test]
    fn steps_one_thread_without_a_schedule_or_when_asked() {
        for (follows, threads) in [(false, SteppedThreads::All), (true, SteppedThreads::One)] {
            let (_, cov) = logged(follows, threads);
            assert_eq!(cov.intervals[0].scope, CoverageScope::Thread(A));
        }
    }
}
