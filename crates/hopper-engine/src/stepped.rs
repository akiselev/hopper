//! Stepped windows: a recording made without rr, by single-stepping a gdbserver run through a
//! window, keeping every state's registers and an undo log of each instruction's memory writes
//! (iced-x86's memory operands, read before and after the step), plus a snapshot of the
//! process's memory at the window's end. A [`StepReplica`] replays the window in both
//! directions, with breakpoints, write watchpoints and seeks, the way an rr replica does.
//!
//! A live session uses one to answer `origin` and `last_write`, which need reverse execution: a
//! second run of the program goes to the entry of the stopped thread's outermost user frame,
//! then is stepped until its registers equal the live process's at the asked moment (gdbserver
//! disables address randomisation, so a deterministic program reaches the same state). The
//! window's moments are its own run's ([`hopper_core::api::ReferenceRun`]); history before the
//! window's start is not in it. Tests use the same windows where rr cannot run.
//!
//! Watchpoint semantics follow rr's: a write watchpoint stops just after the writing
//! instruction, in either direction; a reverse continue started from a watchpoint stop does not
//! report that same write again.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hopper_core::target::{
    BreakKind, Breakpoint, Direction, Resume, StopEvent, StopReason, Target, TargetInfo,
};
use hopper_core::{
    CheckpointId, HopperError, Moment, ProcessId, RecordingId, Reg, RegisterSet, Result, Tid,
};
use hopper_rr::{GdbserverConfig, GdbserverTarget, ReplicaFactory, ReplicaRole};
use iced_x86::{Decoder, DecoderOptions, InstructionInfoFactory, OpAccess, Register};

/// One instruction's write: where, the bytes before, the bytes after.
#[derive(Clone, Debug)]
struct Write {
    addr: u64,
    old: Vec<u8>,
    new: Vec<u8>,
}

#[derive(Debug)]
pub struct StepRecording {
    recording: RecordingId,
    process: ProcessId,
    exe: PathBuf,
    auxv: Vec<u8>,
    tid: Tid,
    states: Vec<RegisterSet>,
    /// `writes[j]`: what the instruction at state `j` wrote (state `j` to `j + 1`).
    writes: Vec<Vec<Write>>,
    /// The process's memory at the last state, by mapping.
    end_memory: Vec<(u64, Vec<u8>)>,
}

fn reg_value(regs: &RegisterSet, r: Register) -> Option<u64> {
    match r {
        Register::FS => return regs.get(Reg::FS_BASE),
        Register::GS => return regs.get(Reg::GS_BASE),
        Register::None | Register::ES | Register::CS | Register::SS | Register::DS => {
            return Some(0);
        }
        _ => {}
    }
    let full = r.full_register();
    let gpr = [
        Register::RAX,
        Register::RDX,
        Register::RCX,
        Register::RBX,
        Register::RSI,
        Register::RDI,
        Register::RBP,
        Register::RSP,
        Register::R8,
        Register::R9,
        Register::R10,
        Register::R11,
        Register::R12,
        Register::R13,
        Register::R14,
        Register::R15,
        Register::RIP,
    ];
    let i = gpr.iter().position(|g| *g == full)?;
    let v = regs.get(Reg(i as u16))?;
    Some(match r.size() {
        4 => v & 0xffff_ffff,
        2 => v & 0xffff,
        1 => v & 0xff,
        _ => v,
    })
}

/// The memory an instruction writes, from its registers before it runs.
fn written_ranges(t: &mut GdbserverTarget, regs: &RegisterSet) -> Vec<(u64, u64)> {
    let pc = regs.pc().unwrap_or(0);
    let mut b = [0u8; 15];
    let mut n = b.len();
    while n > 0 && t.read_memory(pc, &mut b[..n]).is_err() {
        n -= 1;
    }
    let mut d = Decoder::with_ip(64, &b[..n], pc, DecoderOptions::NONE);
    let insn = d.decode();
    if insn.is_invalid() {
        return Vec::new();
    }
    let mut info = InstructionInfoFactory::new();
    let mut out = Vec::new();
    for m in info.info(&insn).used_memory() {
        if !matches!(
            m.access(),
            OpAccess::Write | OpAccess::ReadWrite | OpAccess::CondWrite | OpAccess::ReadCondWrite
        ) {
            continue;
        }
        let Some(addr) = m.virtual_address(0, |r, _, _| reg_value(regs, r)) else {
            continue;
        };
        // xsave and friends have no fixed size: keep a generous area.
        let size = match m.memory_size().size() {
            0 => 4096,
            s => s as u64,
        };
        out.push((addr, size));
    }
    out
}

/// Where a stepped window starts: the run continues to the first stop that matches.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum WindowStart {
    /// The first time the PC reaches this address.
    Address(u64),
    /// A function's entry when its frame will have this CFA (the stack pointer is `cfa - 8` at
    /// the entry, before the prologue).
    FrameEntry { entry: u64, cfa: u64 },
}

/// Where a stepped window ends: the first state that matches.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowEnd {
    /// The PC reaches this address.
    Address(u64),
    /// The general registers and `rip` equal these.
    Registers(RegisterSet),
}

/// The general registers and rip agree (what identifies one state of a deterministic run).
fn same_state(a: &RegisterSet, b: &RegisterSet) -> bool {
    (0..=Reg::RIP.0).all(|r| a.get(Reg(r)) == b.get(Reg(r)))
}

impl WindowEnd {
    fn reached(&self, regs: &RegisterSet) -> bool {
        match self {
            WindowEnd::Address(a) => regs.pc() == Some(*a),
            WindowEnd::Registers(r) => same_state(regs, r),
        }
    }
}

impl StepRecording {
    /// Run `program` under its own gdbserver to `start`, then single-step it to `end`, at most
    /// `max_steps` instructions.
    pub fn capture(
        config: &GdbserverConfig,
        program: &Path,
        args: &[OsString],
        start: WindowStart,
        end: &WindowEnd,
        max_steps: usize,
    ) -> Result<StepRecording> {
        let mut t = GdbserverTarget::launch(config, program, args)?;
        let auxv = t.auxv()?;
        let entry = match start {
            WindowStart::Address(a) | WindowStart::FrameEntry { entry: a, .. } => a,
        };
        t.insert(Breakpoint::code(entry))?;
        loop {
            let ev = t.resume(Resume::Continue, Direction::Forward)?;
            match ev.reason {
                StopReason::Breakpoint { addr } if addr == entry => {
                    let sp = t.registers(ev.tid)?.sp();
                    match start {
                        WindowStart::Address(_) => break,
                        WindowStart::FrameEntry { cfa, .. } if sp == Some(cfa - 8) => break,
                        WindowStart::FrameEntry { .. } => {}
                    }
                }
                StopReason::Exited { .. } | StopReason::Killed { .. } | StopReason::HistoryEnd => {
                    return Err(HopperError::NotFound(format!(
                        "the run ended before reaching the window's start ({start:?})"
                    )));
                }
                _ => {}
            }
        }
        t.remove(Breakpoint::code(entry))?;
        let tid = t.current_thread()?;
        let mut states = Vec::new();
        let mut writes = Vec::new();
        loop {
            let regs = t.registers(tid)?;
            states.push(regs.clone());
            if end.reached(&regs) {
                break;
            }
            if states.len() > max_steps {
                return Err(HopperError::BudgetExhausted {
                    steps: states.len() as u64,
                });
            }
            let ranges = written_ranges(&mut t, &regs);
            let mut olds = Vec::new();
            for (a, len) in &ranges {
                let mut b = vec![0u8; *len as usize];
                olds.push(t.read_memory(*a, &mut b).ok().map(|()| b));
            }
            let ev = t.resume(Resume::Step { tid }, Direction::Forward)?;
            if ev.reason != StopReason::Step {
                return Err(HopperError::Protocol(format!(
                    "a step stopped with {ev:?} before the window's end"
                )));
            }
            let mut w = Vec::new();
            for ((a, len), old) in ranges.into_iter().zip(olds) {
                let mut new = vec![0u8; len as usize];
                if let (Some(old), Ok(())) = (old, t.read_memory(a, &mut new)) {
                    w.push(Write { addr: a, old, new });
                }
            }
            writes.push(w);
        }
        let pid = t.info().process.pid;
        let end_memory = snapshot(&mut t, pid)?;
        Ok(StepRecording {
            recording: t.info().recording,
            process: t.info().process,
            exe: program.to_path_buf(),
            auxv,
            tid,
            states,
            writes,
            end_memory,
        })
    }

    /// The window's recording id (its run's).
    pub fn recording(&self) -> RecordingId {
        self.recording
    }

    /// The window's last state.
    pub fn end(&self) -> Moment {
        self.moment(self.states.len() - 1)
    }

    /// Whether the window has no state (never: a capture keeps at least its start).
    pub fn is_empty(&self) -> bool {
        self.states.is_empty()
    }

    pub fn len(&self) -> usize {
        self.states.len()
    }

    pub fn moment(&self, i: usize) -> Moment {
        Moment {
            recording: self.recording,
            frame_time: 1,
            tid: self.tid,
            ticks: 0,
            sub: i as u32,
        }
    }

    /// Memory at state `i`: the end snapshot with every later write undone.
    fn read_at(&self, i: usize, addr: u64, buf: &mut [u8]) -> Result<()> {
        let len = buf.len() as u64;
        let region = self
            .end_memory
            .iter()
            .find(|(a, b)| addr >= *a && addr + len <= a + b.len() as u64)
            .ok_or(HopperError::UnreadableMemory { addr, len })?;
        let off = (addr - region.0) as usize;
        buf.copy_from_slice(&region.1[off..off + buf.len()]);
        for step in self.writes[i.min(self.writes.len())..].iter().rev() {
            for w in step {
                let lo = addr.max(w.addr);
                let hi = (addr + len).min(w.addr + w.old.len() as u64);
                if lo < hi {
                    let n = (hi - lo) as usize;
                    let (bo, wo) = ((lo - addr) as usize, (lo - w.addr) as usize);
                    buf[bo..bo + n].copy_from_slice(&w.old[wo..wo + n]);
                }
            }
        }
        Ok(())
    }
}

/// Every readable mapping of process `pid` (read through the target, page by page).
fn snapshot(t: &mut GdbserverTarget, pid: i32) -> Result<Vec<(u64, Vec<u8>)>> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps"))?;
    let mut out = Vec::new();
    for line in maps.lines() {
        if line.contains("[vvar") || line.contains("[vsyscall]") {
            continue;
        }
        let Some((range, _)) = line.split_once(' ') else {
            continue;
        };
        let Some((lo, hi)) = range.split_once('-') else {
            continue;
        };
        let (Ok(lo), Ok(hi)) = (u64::from_str_radix(lo, 16), u64::from_str_radix(hi, 16)) else {
            continue;
        };
        let mut start = lo;
        let mut bytes = Vec::new();
        let mut page = lo;
        while page < hi {
            let mut b = vec![0u8; 4096];
            match t.read_memory(page, &mut b) {
                Ok(()) => bytes.extend_from_slice(&b),
                Err(_) => {
                    if !bytes.is_empty() {
                        out.push((start, std::mem::take(&mut bytes)));
                    }
                    start = page + 4096;
                }
            }
            page += 4096;
        }
        if !bytes.is_empty() {
            out.push((start, bytes));
        }
    }
    Ok(out)
}

/// One replica of a [`StepRecording`].
#[derive(Debug)]
pub struct StepReplica {
    rec: Arc<StepRecording>,
    info: TargetInfo,
    pos: usize,
    breakpoints: Vec<Breakpoint>,
    /// The last stop was a watchpoint at `pos`: a reverse continue must not report it again.
    at_watch: bool,
}

impl StepReplica {
    pub fn new(rec: Arc<StepRecording>) -> StepReplica {
        let info = TargetInfo {
            recording: rec.recording,
            process: rec.process,
            executable: rec.exe.clone(),
            can_reverse: true,
            can_seek: true,
            rr_version: None,
        };
        StepReplica {
            rec,
            info,
            pos: 0,
            breakpoints: Vec::new(),
            at_watch: false,
        }
    }

    fn code_at(&self, k: usize) -> Option<u64> {
        let pc = self.rec.states[k].pc()?;
        self.breakpoints
            .iter()
            .any(|b| matches!(b.kind, BreakKind::Software | BreakKind::Hardware) && b.addr == pc)
            .then_some(pc)
    }

    /// A watchpoint the write of step `j` hits.
    fn watch_hit(&self, j: usize) -> Option<Breakpoint> {
        let writes = self.rec.writes.get(j)?;
        self.breakpoints.iter().copied().find(|b| {
            matches!(b.kind, BreakKind::WriteWatch | BreakKind::AccessWatch)
                && writes
                    .iter()
                    .any(|w| w.addr < b.addr + b.len && b.addr < w.addr + w.new.len() as u64)
        })
    }

    fn stop(&mut self, pos: usize, reason: StopReason) -> StopEvent {
        self.pos = pos;
        self.at_watch = matches!(reason, StopReason::Watchpoint { .. });
        StopEvent {
            tid: self.rec.tid,
            reason,
        }
    }
}

impl Target for StepReplica {
    fn info(&self) -> &TargetInfo {
        &self.info
    }
    fn threads(&mut self) -> Result<Vec<Tid>> {
        Ok(vec![self.rec.tid])
    }
    fn current_thread(&mut self) -> Result<Tid> {
        Ok(self.rec.tid)
    }
    fn registers(&mut self, _tid: Tid) -> Result<RegisterSet> {
        Ok(self.rec.states[self.pos].clone())
    }
    fn read_memory(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        self.rec.read_at(self.pos, addr, buf)
    }
    fn insert(&mut self, bp: Breakpoint) -> Result<()> {
        if !self.breakpoints.contains(&bp) {
            self.breakpoints.push(bp);
        }
        Ok(())
    }
    fn remove(&mut self, bp: Breakpoint) -> Result<()> {
        let n = self.breakpoints.len();
        self.breakpoints.retain(|b| *b != bp);
        if self.breakpoints.len() == n {
            return Err(HopperError::NotFound(format!("{bp:?}")));
        }
        Ok(())
    }
    fn resume(&mut self, how: Resume, dir: Direction) -> Result<StopEvent> {
        let last = self.rec.len() - 1;
        Ok(match (how, dir) {
            (Resume::Step { .. }, Direction::Forward) if self.pos < last => {
                self.stop(self.pos + 1, StopReason::Step)
            }
            (Resume::Step { .. }, Direction::Forward) => self.stop(last, StopReason::HistoryEnd),
            (Resume::Step { .. }, Direction::Reverse) if self.pos > 0 => {
                self.stop(self.pos - 1, StopReason::Step)
            }
            (Resume::Step { .. }, Direction::Reverse) => self.stop(0, StopReason::HistoryStart),
            (Resume::Continue, Direction::Forward) => {
                for k in self.pos + 1..=last {
                    if let Some(w) = self.watch_hit(k - 1) {
                        let reason = StopReason::Watchpoint {
                            kind: w.kind,
                            addr: w.addr,
                        };
                        return Ok(self.stop(k, reason));
                    }
                    if let Some(addr) = self.code_at(k) {
                        return Ok(self.stop(k, StopReason::Breakpoint { addr }));
                    }
                }
                self.stop(last, StopReason::HistoryEnd)
            }
            (Resume::Continue, Direction::Reverse) => {
                let from = self.pos;
                for k in (0..from).rev() {
                    let repeat = k + 1 == from && self.at_watch;
                    if !repeat && let Some(w) = self.watch_hit(k) {
                        let reason = StopReason::Watchpoint {
                            kind: w.kind,
                            addr: w.addr,
                        };
                        return Ok(self.stop(k + 1, reason));
                    }
                    if let Some(addr) = self.code_at(k) {
                        return Ok(self.stop(k, StopReason::Breakpoint { addr }));
                    }
                }
                self.stop(0, StopReason::HistoryStart)
            }
        })
    }
    fn now(&mut self) -> Result<Moment> {
        Ok(self.rec.moment(self.pos))
    }
    fn seek(&mut self, to: &Moment) -> Result<StopEvent> {
        if to.recording != self.rec.recording || to.sub as usize >= self.rec.len() {
            return Err(HopperError::BadMoment(format!("{to}")));
        }
        Ok(self.stop(to.sub as usize, StopReason::Arrived))
    }
    fn checkpoint(&mut self) -> Result<CheckpointId> {
        Err(HopperError::Unsupported("checkpoints".into()))
    }
    fn restore(&mut self, _id: CheckpointId) -> Result<StopEvent> {
        Err(HopperError::Unsupported("checkpoints".into()))
    }
    fn auxv(&mut self) -> Result<Vec<u8>> {
        Ok(self.rec.auxv.clone())
    }
}

/// Starts [`StepReplica`]s of one recording.
#[derive(Debug, Clone)]
pub struct StepFactory(pub Arc<StepRecording>);

impl ReplicaFactory for StepFactory {
    fn start(&self, _role: ReplicaRole) -> Result<Box<dyn Target>> {
        Ok(Box::new(StepReplica::new(self.0.clone())))
    }
}
