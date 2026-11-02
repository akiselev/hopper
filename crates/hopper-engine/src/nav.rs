//! Navigation on one replica, the way gdb drives a remote stub.
//!
//! - **Line steps** run inside the current line's address range ([`SymbolSet::line_range_at`]).
//!   Before each instruction the driver decodes it: a call is stepped over by a temporary
//!   breakpoint at its return address that only counts when the stack pointer is back where it
//!   was (frame identity, so recursion does not stop early), or, for step-into with debug
//!   information at the callee, entered and run to the callee's post-prologue address. A return
//!   ends the step in the caller. The step ends at the first instruction of a different line.
//! - **Reverse line steps** (rr) single-step backwards; a step back that lands on a callee's `ret`
//!   is undone over the whole call with a breakpoint at the callee's entry (stack pointer checked)
//!   and one more step back onto the call. When the step leaves the line it is moving to, the
//!   driver runs forward again to the last instruction that was still in it, which is that line's
//!   first instruction.
//! - **Step out** runs to the return address with the frame's CFA as the stack pointer; reverse
//!   step out runs back to the function's entry (stack pointer CFA - 8) and one instruction more.
//! - Tracepoints (the lines the ownership sidecar names) are breakpoints that record a hit and
//!   never stop navigation.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use hopper_core::api::{BreakpointId, StepKind};
use hopper_core::target::{
    BreakKind, Breakpoint, Direction, Memory, Resume, StopEvent, StopReason, Target, TargetMemory,
};
use hopper_core::{HopperError, Place, RegisterSet, Result, SourceLoc, Tid};

use crate::Shared;
use crate::decode::{self, Flow};
use crate::ownership::LineHit;
use crate::symbols::ProcessSymbols;

/// How a navigation command ended, before it becomes a [`hopper_core::api::Stop`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Halt {
    /// The step finished normally.
    Stepped,
    /// A seek or restore reached its target.
    Arrived,
    Breakpoint(BreakpointId),
    Watchpoint(Place),
    Signal(u8),
    Exited(u8),
    HistoryStart,
    HistoryEnd,
}

/// Whether a temporary breakpoint was reached.
enum Reach {
    Arrived,
    Halted(Halt),
}

/// Split `[addr, addr+len)` into at most four naturally aligned pieces of 1, 2, 4 or 8 bytes (the
/// x86 debug registers). The second value is false when the range needed more than four.
pub fn watch_pieces(addr: u64, len: u64) -> (Vec<(u64, u64)>, bool) {
    let mut out = Vec::new();
    let mut a = addr;
    let end = addr.saturating_add(len.max(1));
    while a < end {
        if out.len() == 4 {
            return (out, false);
        }
        let size = [8u64, 4, 2, 1]
            .into_iter()
            .find(|s| a.is_multiple_of(*s) && a + s <= end)
            .unwrap_or(1);
        out.push((a, size));
        a += size;
    }
    (out, true)
}

/// One navigation's view of the cursor replica.
pub(crate) struct Driver<'a> {
    pub t: &'a mut dyn Target,
    pub shared: &'a Shared,
    syms: Arc<ProcessSymbols>,
    /// Code breakpoints that stay inserted between commands: user breakpoints and tracepoints.
    resident: HashSet<u64>,
    user: HashMap<u64, BreakpointId>,
    /// Watchpoints this navigation inserted, by piece.
    watches: Vec<(Breakpoint, Place)>,
    resumes: u64,
    budget: u64,
    pub hits: Vec<LineHit>,
}

impl<'a> Driver<'a> {
    pub fn new(
        t: &'a mut dyn Target,
        shared: &'a Shared,
        user: HashMap<u64, BreakpointId>,
    ) -> Result<Driver<'a>> {
        let syms = shared.symbols_live(t)?;
        let mut resident: HashSet<u64> = user.keys().copied().collect();
        resident.extend(shared.tracepoints.keys().copied());
        Ok(Driver {
            t,
            shared,
            syms,
            resident,
            user,
            watches: Vec::new(),
            resumes: 0,
            budget: shared.options.navigation_budget,
            hits: Vec::new(),
        })
    }

    fn regs(&mut self, tid: Tid) -> Result<RegisterSet> {
        self.t.registers(tid)
    }

    fn pc(&mut self, tid: Tid) -> Result<u64> {
        self.regs(tid)?
            .pc()
            .ok_or_else(|| HopperError::Protocol("no rip in the register set".into()))
    }

    fn sp(&mut self, tid: Tid) -> Result<u64> {
        self.regs(tid)?
            .sp()
            .ok_or_else(|| HopperError::Protocol("no rsp in the register set".into()))
    }

    /// The symbols, re-read when `pc` is in no known module (a library was loaded).
    fn syms_for(&mut self, pc: u64) -> Arc<ProcessSymbols> {
        if self.syms.set.module_at(pc).is_none()
            && let Ok(s) = self.shared.symbols_live(self.t)
        {
            self.syms = s;
        }
        self.syms.clone()
    }

    fn insn_at(&mut self, pc: u64) -> Option<iced_x86::Instruction> {
        let mut buf = [0u8; decode::MAX_INSN];
        // The tail of the buffer may be unmapped; shrink until it reads.
        let mut n = buf.len();
        while n > 0 {
            if self.t.read_memory(pc, &mut buf[..n]).is_ok() {
                return decode::decode(&buf[..n], pc);
            }
            n -= 4.min(n);
        }
        None
    }

    fn resume(&mut self, how: Resume, dir: Direction) -> Result<StopEvent> {
        self.resumes += 1;
        if self.resumes > self.budget {
            return Err(HopperError::BudgetExhausted {
                steps: self.resumes,
            });
        }
        self.t.resume(how, dir)
    }

    fn is_tracepoint(&self, addr: u64) -> bool {
        self.shared.tracepoints.contains_key(&addr)
    }

    /// Record a tracepoint hit when `tid` sits on one.
    fn note_trace(&mut self, tid: Tid) -> Result<()> {
        if self.shared.tracepoints.is_empty() {
            return Ok(());
        }
        let regs = self.regs(tid)?;
        let Some(pc) = regs.pc() else { return Ok(()) };
        let Some(tp) = self.shared.tracepoints.get(&pc).cloned() else {
            return Ok(());
        };
        let at = self.t.now()?;
        if self.hits.last().is_some_and(|h| h.at == at) {
            return Ok(());
        }
        let syms = self.syms_for(pc);
        let cfa = {
            let mut mem = TargetMemory(&mut *self.t);
            syms.set
                .unwind(&mut mem as &mut dyn Memory, &regs)
                .ok()
                .and_then(|f| f.first().and_then(|f| f.cfa))
        };
        if let Some(cfa) = cfa {
            self.hits.push(LineHit {
                at,
                tid,
                cfa,
                function: tp.function,
                mark: tp.mark,
            });
        }
        Ok(())
    }

    /// Map a target stop to how navigation ends.
    fn halt_of(&mut self, ev: &StopEvent) -> Halt {
        match &ev.reason {
            StopReason::Breakpoint { addr } => match self.user.get(addr) {
                Some(id) => Halt::Breakpoint(*id),
                None => Halt::Stepped,
            },
            StopReason::Watchpoint { addr, .. } => {
                let place = self
                    .watches
                    .iter()
                    .find(|(b, _)| *addr >= b.addr && *addr < b.addr + b.len)
                    .map(|(_, p)| *p)
                    .unwrap_or(Place::memory(self.shared.process, *addr, 1));
                Halt::Watchpoint(place)
            }
            StopReason::Step | StopReason::Arrived | StopReason::Other { .. } => Halt::Stepped,
            StopReason::Signal { signo } | StopReason::Killed { signo } => Halt::Signal(*signo),
            StopReason::Exited { code } => Halt::Exited(*code),
            StopReason::HistoryStart => Halt::HistoryStart,
            StopReason::HistoryEnd => Halt::HistoryEnd,
        }
    }

    /// Continue in `dir` until a stop that is not a tracepoint (unless the tracepoint address
    /// is in `wanted`, or a user breakpoint).
    fn cont(&mut self, dir: Direction, wanted: &[u64]) -> Result<StopEvent> {
        loop {
            let ev = self.resume(Resume::Continue, dir)?;
            if let StopReason::Breakpoint { addr } = ev.reason
                && self.is_tracepoint(addr)
            {
                self.note_trace(ev.tid)?;
                if !self.user.contains_key(&addr) && !wanted.contains(&addr) {
                    continue;
                }
            }
            return Ok(ev);
        }
    }

    /// One instruction of `tid`.
    pub fn step_instruction(&mut self, tid: Tid, dir: Direction) -> Result<Halt> {
        let ev = self.resume(Resume::Step { tid }, dir)?;
        if ev.reason == StopReason::Step {
            self.note_trace(tid)?;
            return Ok(Halt::Stepped);
        }
        Ok(self.halt_of(&ev))
    }

    /// One step whose stop is not a plain step is a halt; `Ok(None)` when it stepped.
    fn step_once(&mut self, tid: Tid, dir: Direction) -> Result<Option<Halt>> {
        match self.step_instruction(tid, dir)? {
            Halt::Stepped => Ok(None),
            other => Ok(Some(other)),
        }
    }

    /// Run in `dir` to `addr` on `tid` with the stack pointer equal to `sp` (when given).
    fn run_to(&mut self, addr: u64, tid: Tid, sp: Option<u64>, dir: Direction) -> Result<Reach> {
        let bp = Breakpoint::code(addr);
        let temporary = !self.resident.contains(&addr);
        if temporary {
            self.t.insert(bp)?;
        }
        let result = self.run_to_inserted(addr, tid, sp, dir);
        if temporary {
            let removed = self.t.remove(bp);
            if result.is_ok() {
                // A target that has ended cannot remove anything; that is not an error here.
                if let Err(e) = removed
                    && !matches!(e, HopperError::Disconnected(_))
                {
                    return Err(e);
                }
            }
        }
        result
    }

    fn run_to_inserted(
        &mut self,
        addr: u64,
        tid: Tid,
        sp: Option<u64>,
        dir: Direction,
    ) -> Result<Reach> {
        loop {
            let ev = self.cont(dir, &[addr])?;
            match ev.reason {
                StopReason::Breakpoint { addr: a } if a == addr => {
                    let here = ev.tid == tid && sp.is_none_or(|s| self.sp(tid).ok() == Some(s));
                    if here {
                        return Ok(Reach::Arrived);
                    }
                    if self.user.contains_key(&a) {
                        return Ok(Reach::Halted(self.halt_of(&ev)));
                    }
                }
                StopReason::Breakpoint { addr: a } if self.user.contains_key(&a) => {
                    return Ok(Reach::Halted(self.halt_of(&ev)));
                }
                StopReason::Breakpoint { .. } => {}
                _ => return Ok(Reach::Halted(self.halt_of(&ev))),
            }
        }
    }

    fn line_at(&mut self, pc: u64) -> Option<hopper_symbols::LineRange> {
        self.syms_for(pc).set.line_range_at(pc)
    }

    /// Whether code at `pc` has source lines (a function the user can step into).
    fn has_lines(&mut self, pc: u64) -> bool {
        let syms = self.syms_for(pc);
        syms.set.location(pc).is_some_and(|l| l.line != 0)
            && syms.set.function_at(pc).is_some_and(|f| f.die().is_some())
    }

    /// Step one source line forward (`Into` or `Over`).
    pub fn step_line_forward(&mut self, tid: Tid, into: bool) -> Result<Halt> {
        let pc = self.pc(tid)?;
        let Some(start) = self.line_at(pc) else {
            return self.step_to_lines(tid);
        };
        let start_loc = start.location.clone();
        let mut range = start.range;
        loop {
            let pc = self.pc(tid)?;
            let insn = self.insn_at(pc);
            match insn.as_ref().map(decode::flow) {
                Some(Flow::Call { next }) => {
                    let sp = self.sp(tid)?;
                    if into {
                        if let Some(h) = self.step_once(tid, Direction::Forward)? {
                            return Ok(h);
                        }
                        let callee = self.pc(tid)?;
                        if self.has_lines(callee) {
                            let syms = self.syms_for(callee);
                            if let Some(f) = syms.set.function_at(callee) {
                                let body = syms.set.breakpoint_address(&f);
                                if body != callee
                                    && let Reach::Halted(h) =
                                        self.run_to(body, tid, None, Direction::Forward)?
                                {
                                    return Ok(h);
                                }
                            }
                            return Ok(Halt::Stepped);
                        }
                        // No source for the callee (a PLT stub, libc): finish it.
                        if let Reach::Halted(h) =
                            self.run_to(next, tid, Some(sp), Direction::Forward)?
                        {
                            return Ok(h);
                        }
                    } else if let Reach::Halted(h) =
                        self.run_to(next, tid, Some(sp), Direction::Forward)?
                    {
                        return Ok(h);
                    }
                }
                Some(Flow::Return) => {
                    if let Some(h) = self.step_once(tid, Direction::Forward)? {
                        return Ok(h);
                    }
                    // Back in the caller, usually in the middle of the calling line: gdb stops
                    // here and reports the caller.
                    return Ok(Halt::Stepped);
                }
                _ => {
                    if let Some(h) = self.step_once(tid, Direction::Forward)? {
                        return Ok(h);
                    }
                }
            }
            let pc = self.pc(tid)?;
            if range.contains(&pc) {
                continue;
            }
            let Some(lr) = self.line_at(pc) else {
                // Code without line information inside a line: keep going.
                continue;
            };
            if same_line(&lr.location, &start_loc) {
                range = lr.range;
                continue;
            }
            if pc == lr.range.start {
                return Ok(Halt::Stepped);
            }
            // The middle of another line (a jump into it): finish that line too, as gdb does.
            range = lr.range;
        }
    }

    /// From code without line information, step (over calls) until code with lines.
    fn step_to_lines(&mut self, tid: Tid) -> Result<Halt> {
        loop {
            let pc = self.pc(tid)?;
            if let Some(Flow::Call { next }) = self.insn_at(pc).as_ref().map(decode::flow) {
                let sp = self.sp(tid)?;
                if let Reach::Halted(h) = self.run_to(next, tid, Some(sp), Direction::Forward)? {
                    return Ok(h);
                }
            } else if let Some(h) = self.step_once(tid, Direction::Forward)? {
                return Ok(h);
            }
            let pc = self.pc(tid)?;
            if self.line_at(pc).is_some() {
                return Ok(Halt::Stepped);
            }
        }
    }

    /// The innermost frame's CFA and the address of the instruction after its call (the return
    /// address), from unwinding.
    fn frame_of(&mut self, tid: Tid) -> Result<(u64, Option<u64>)> {
        let regs = self.regs(tid)?;
        let pc = regs.pc().unwrap_or(0);
        let syms = self.syms_for(pc);
        let mut mem = TargetMemory(&mut *self.t);
        let frames = syms.set.unwind(&mut mem as &mut dyn Memory, &regs)?;
        let cfa = frames
            .first()
            .and_then(|f| f.cfa)
            .ok_or_else(|| HopperError::NotFound(format!("no frame address at {pc:#x}")))?;
        Ok((cfa, frames.get(1).map(|f| f.pc)))
    }

    /// Run until the current function returns (forward) or was called (reverse).
    pub fn step_out(&mut self, tid: Tid, dir: Direction) -> Result<Halt> {
        let (cfa, ret) = self.frame_of(tid)?;
        match dir {
            Direction::Forward => {
                let ret = ret.ok_or_else(|| {
                    HopperError::NotFound("the outermost frame has no caller".into())
                })?;
                match self.run_to(ret, tid, Some(cfa), Direction::Forward)? {
                    Reach::Arrived => Ok(Halt::Stepped),
                    Reach::Halted(h) => Ok(h),
                }
            }
            Direction::Reverse => {
                let pc = self.pc(tid)?;
                let entry = self
                    .syms_for(pc)
                    .set
                    .function_at(pc)
                    .map(|f| f.entry)
                    .ok_or_else(|| HopperError::NotFound(format!("no function at {pc:#x}")))?;
                if pc != entry
                    && let Reach::Halted(h) =
                        self.run_to(entry, tid, Some(cfa - 8), Direction::Reverse)?
                {
                    return Ok(h);
                }
                Ok(self
                    .step_once(tid, Direction::Reverse)?
                    .unwrap_or(Halt::Stepped))
            }
        }
    }

    /// One instruction back. When that lands on a callee's `ret` and `enter` is false (or the
    /// callee has no lines), the whole call is undone: the result is the call instruction.
    /// Returns whether it entered a callee.
    fn back_one(&mut self, tid: Tid, enter: bool) -> Result<std::result::Result<bool, Halt>> {
        let sp_before = self.sp(tid)?;
        if let Some(h) = self.step_once(tid, Direction::Reverse)? {
            return Ok(Err(h));
        }
        let pc = self.pc(tid)?;
        let sp = self.sp(tid)?;
        let is_ret = self.insn_at(pc).as_ref().map(decode::flow) == Some(Flow::Return);
        if !(is_ret && sp < sp_before) {
            return Ok(Ok(false));
        }
        if enter && self.has_lines(pc) {
            return Ok(Ok(true));
        }
        let entry = self.syms_for(pc).set.function_at(pc).map(|f| f.entry);
        let Some(entry) = entry else {
            return Ok(Ok(false));
        };
        // At the callee's entry the stack pointer is the caller's minus the return address.
        if let Reach::Halted(h) =
            self.run_to(entry, tid, Some(sp_before - 8), Direction::Reverse)?
        {
            return Ok(Err(h));
        }
        if let Some(h) = self.step_once(tid, Direction::Reverse)? {
            return Ok(Err(h));
        }
        Ok(Ok(false))
    }

    /// Step one source line backwards (`Into` or `Over`).
    pub fn step_line_reverse(&mut self, tid: Tid, into: bool) -> Result<Halt> {
        let pc = self.pc(tid)?;
        let Some(here) = self.line_at(pc) else {
            return Ok(self
                .step_once(tid, Direction::Reverse)?
                .unwrap_or(Halt::Stepped));
        };
        // The line to move to the start of: this one when the cursor is inside it, else the
        // line executed before.
        let mut target = if pc != here.range.start {
            here
        } else {
            loop {
                if let Err(h) = self.back_one(tid, into)? {
                    return Ok(h);
                }
                let pc = self.pc(tid)?;
                if let Some(lr) = self.line_at(pc) {
                    break lr;
                }
            }
        };
        loop {
            let last_pc = self.pc(tid)?;
            let last_sp = self.sp(tid)?;
            let entered = match self.back_one(tid, into)? {
                Err(h) => return Ok(h),
                Ok(e) => e,
            };
            let pc = self.pc(tid)?;
            if entered {
                // Stepped back into a callee's last line: move to its start instead.
                match self.line_at(pc) {
                    Some(lr) => {
                        target = lr;
                        continue;
                    }
                    None => continue,
                }
            }
            let still_in = target.range.contains(&pc)
                || self
                    .line_at(pc)
                    .is_some_and(|lr| same_line(&lr.location, &target.location));
            if still_in && pc != last_pc {
                continue;
            }
            // Left the line: forward again to its first instruction.
            return match self.run_to(last_pc, tid, Some(last_sp), Direction::Forward)? {
                Reach::Arrived => Ok(Halt::Stepped),
                Reach::Halted(h) => Ok(h),
            };
        }
    }

    pub fn step(&mut self, tid: Tid, kind: StepKind, dir: Direction) -> Result<Halt> {
        match (kind, dir) {
            (StepKind::Instruction, _) => self.step_instruction(tid, dir),
            (StepKind::Into, Direction::Forward) => self.step_line_forward(tid, true),
            (StepKind::Over, Direction::Forward) => self.step_line_forward(tid, false),
            (StepKind::Into, Direction::Reverse) => self.step_line_reverse(tid, true),
            (StepKind::Over, Direction::Reverse) => self.step_line_reverse(tid, false),
            (StepKind::Out, _) => self.step_out(tid, dir),
        }
    }

    /// Continue to the next user breakpoint (or the end of history).
    pub fn continue_to_breakpoints(&mut self, dir: Direction) -> Result<Halt> {
        loop {
            let ev = self.cont(dir, &[])?;
            match ev.reason {
                StopReason::Breakpoint { addr } if !self.user.contains_key(&addr) => continue,
                _ => return Ok(self.halt_of(&ev)),
            }
        }
    }

    /// Continue, passing user breakpoints, until the end (or start) of history.
    pub fn continue_to_end(&mut self, dir: Direction) -> Result<Halt> {
        loop {
            let ev = self.cont(dir, &[])?;
            match ev.reason {
                StopReason::Breakpoint { .. } => continue,
                _ => return Ok(self.halt_of(&ev)),
            }
        }
    }

    /// Continue until something writes `place`.
    pub fn continue_to_write(&mut self, place: Place, dir: Direction) -> Result<Halt> {
        let Place::Memory { addr, len, .. } = place else {
            return Err(HopperError::Unsupported(
                "watching a register; give a memory place".into(),
            ));
        };
        let (pieces, whole) = watch_pieces(addr, len);
        if !whole {
            return Err(HopperError::Unsupported(format!(
                "watching {len} bytes at {addr:#x}: at most four aligned pieces of up to 8 bytes"
            )));
        }
        for (a, l) in &pieces {
            let bp = Breakpoint {
                kind: BreakKind::WriteWatch,
                addr: *a,
                len: *l,
            };
            self.t.insert(bp)?;
            self.watches.push((bp, place));
        }
        let result = loop {
            match self.cont(dir, &[]) {
                Ok(ev) => match ev.reason {
                    StopReason::Breakpoint { .. } => continue,
                    _ => break Ok(self.halt_of(&ev)),
                },
                Err(e) => break Err(e),
            }
        };
        for (bp, _) in std::mem::take(&mut self.watches) {
            let _ = self.t.remove(bp);
        }
        result
    }

    /// The innermost source frame's function and location at `pc`.
    pub fn describe(&mut self, pc: u64) -> (Option<String>, Option<SourceLoc>) {
        let syms = self.syms_for(pc);
        let f = syms.set.frames_at(pc).into_iter().next();
        match f {
            Some(f) => (f.function, f.location),
            None => (syms.set.symbol_at(pc).map(|s| s.label()), None),
        }
    }

    pub fn pc_of(&mut self, tid: Tid) -> Result<u64> {
        self.pc(tid)
    }
}

fn same_line(a: &SourceLoc, b: &SourceLoc) -> bool {
    a.line == b.line && a.file == b.file
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_pieces_are_aligned() {
        assert_eq!(watch_pieces(0x1000, 8), (vec![(0x1000, 8)], true));
        assert_eq!(
            watch_pieces(0x1003, 5),
            (vec![(0x1003, 1), (0x1004, 4)], true)
        );
        assert_eq!(
            watch_pieces(0x1000, 32),
            (
                vec![(0x1000, 8), (0x1008, 8), (0x1010, 8), (0x1018, 8)],
                true
            )
        );
        assert!(!watch_pieces(0x1000, 33).1);
        assert_eq!(watch_pieces(0x1001, 1), (vec![(0x1001, 1)], true));
    }
}
