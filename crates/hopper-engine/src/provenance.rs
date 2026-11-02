//! Where bytes came from, by reverse execution on a background replica (rr).
//!
//! [`last_write`]: from the moment asked about, put write watchpoints on the place and reverse-
//! continue. rr stops just after the write; one reverse step reaches the writing instruction. A
//! guard watches for the entry of the frame that owns a stack slot (a breakpoint at its function's
//! entry, counted only when the stack pointer is the frame's CFA - 8): reaching it first means the
//! slot was not written during that frame's life, and an older write is a different variable's.
//! Writes from any thread count; the event names the writer's thread.
//!
//! [`origin`]: repeat. Classify the writing instruction ([`crate::decode::classify_write`]): a
//! memory copy continues at its source bytes; a store from a register walks back, one
//! instruction at a time and into callees, to the instruction that defined the register (a load
//! continues at the loaded bytes, `memcpy`'s SIMD loads included). The walk ends at constants,
//! read-only data, the kernel, arithmetic, the start of history or the budget. Each hop is
//! labelled through the sidecar join at the writing instruction.

use std::sync::Arc;

use hopper_core::api::{
    Budget, MachineKind, OriginChain, OriginEnd, OriginHop, OwnershipLabel, WriteEvent,
};
use hopper_core::target::{
    BreakKind, Breakpoint, Direction, Memory, Resume, StopReason, Target, TargetMemory,
};
use hopper_core::{FrameId, HopperError, Moment, Place, Result, SourceLoc, Tid};
use hopper_eval::{EvalContext, FrameRef};
use hopper_sidecar::{Access, DwarfInline, FrameInfo, MachineEffect, PcInfo};
use hopper_symbols::{StackFrame, SymbolSet};
use iced_x86::Register;

use crate::Shared;
use crate::decode::{self, RegisterDefinition, WriteSource};
use crate::nav::watch_pieces;
use crate::symbols::ProcessSymbols;

/// Most bytes one hop follows (four debug registers of eight bytes).
const MAX_WATCH: u64 = 32;
/// Instructions a register's definition is searched back over.
const MAX_REGISTER_WALK: u64 = 4096;

/// A write found by reverse execution.
#[derive(Clone, Debug)]
pub struct FoundWrite {
    pub event: WriteEvent,
    /// The thread that wrote.
    pub tid: Tid,
    /// The byte of the place the write hit first (the watched piece's start).
    pub watched: u64,
    /// How many bytes of the place, from `watched`, the hop follows.
    pub len: u64,
    /// The writing instruction.
    pub insn: Option<iced_x86::Instruction>,
}

/// Why no write was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoWrite {
    /// Nothing wrote the place since the recording started.
    HistoryStart,
    /// The frame that owns the stack slot was entered before any write to it.
    FrameEntered,
}

/// Reverse-execution work on one replica, with a step budget.
pub struct Reverse<'a> {
    pub t: &'a mut dyn Target,
    pub shared: &'a Shared,
    pub steps: u64,
    pub budget: u64,
}

impl<'a> Reverse<'a> {
    fn spend(&mut self) -> Result<()> {
        self.steps += 1;
        if self.steps > self.budget {
            return Err(HopperError::BudgetExhausted { steps: self.steps });
        }
        Ok(())
    }

    fn syms(&mut self) -> Result<Arc<ProcessSymbols>> {
        self.shared.symbols_live(self.t)
    }

    /// The frame whose stack area holds `addr` on any thread: (thread, function entry, CFA).
    fn slot_owner(&mut self, addr: u64) -> Result<Option<(Tid, u64, u64)>> {
        let syms = self.syms()?;
        for tid in self.t.threads()? {
            let regs = self.t.registers(tid)?;
            let Some(sp) = regs.sp() else { continue };
            if addr < sp {
                continue;
            }
            let frames = {
                let mut mem = TargetMemory(&mut *self.t);
                syms.set.unwind(&mut mem as &mut dyn Memory, &regs)?
            };
            for f in &frames {
                let (Some(cfa), Some(fsp)) = (f.cfa, f.regs.sp()) else {
                    continue;
                };
                if addr >= fsp && addr < cfa {
                    let entry = syms.set.function_at(f.lookup_pc()).map(|i| i.entry);
                    return Ok(entry.map(|e| (tid, e, cfa)));
                }
            }
        }
        Ok(None)
    }

    /// The last write to `place` before the replica's current moment.
    pub fn last_write(&mut self, place: Place) -> Result<std::result::Result<FoundWrite, NoWrite>> {
        let Place::Memory { process, addr, len } = place else {
            return Err(HopperError::Unsupported(
                "the last write to a register; give a memory place".into(),
            ));
        };
        let watched_len = len.clamp(1, MAX_WATCH);
        let (pieces, _) = watch_pieces(addr, watched_len);
        let guard = self.slot_owner(addr)?;
        let mut inserted = Vec::new();
        for (a, l) in &pieces {
            let bp = Breakpoint {
                kind: BreakKind::WriteWatch,
                addr: *a,
                len: *l,
            };
            self.t.insert(bp)?;
            inserted.push(bp);
        }
        if let Some((_, entry, _)) = guard {
            let bp = Breakpoint::code(entry);
            self.t.insert(bp)?;
            inserted.push(bp);
        }
        let found = self.reverse_to_write(&pieces, guard);
        for bp in inserted {
            let _ = self.t.remove(bp);
        }
        let (tid, watched, piece_len) = match found? {
            Ok(x) => x,
            Err(no) => return Ok(Err(no)),
        };
        let after = self.t.now()?;
        let piece_end = (watched + piece_len).min(addr + watched_len);
        let follow = piece_end.max(watched + 1) - watched;
        let mut value = vec![0u8; len.min(4096) as usize];
        let _ = self.t.read_memory(addr, &mut value);
        // One instruction back: the writer.
        self.spend()?;
        self.t.resume(Resume::Step { tid }, Direction::Reverse)?;
        let regs = self.t.registers(tid)?;
        let pc = regs.pc().unwrap_or(0);
        let insn = {
            let mut b = [0u8; decode::MAX_INSN];
            let n = (0..=b.len())
                .rev()
                .step_by(1)
                .find(|n| self.t.read_memory(pc, &mut b[..*n]).is_ok())
                .unwrap_or(0);
            decode::decode(&b[..n], pc)
        };
        let syms = self.syms()?;
        let (function, location) = describe(&syms.set, pc);
        Ok(Ok(FoundWrite {
            event: WriteEvent {
                at: after,
                pc,
                place: Place::memory(process, addr, len),
                value,
                function,
                location,
            },
            tid,
            watched,
            len: follow,
            insn,
        }))
    }

    fn reverse_to_write(
        &mut self,
        pieces: &[(u64, u64)],
        guard: Option<(Tid, u64, u64)>,
    ) -> Result<std::result::Result<(Tid, u64, u64), NoWrite>> {
        loop {
            self.spend()?;
            let ev = self.t.resume(Resume::Continue, Direction::Reverse)?;
            match ev.reason {
                StopReason::Watchpoint { addr, .. } => {
                    let (a, l) = pieces
                        .iter()
                        .copied()
                        .find(|(a, l)| addr >= *a && addr < a + l)
                        .unwrap_or((addr, 1));
                    return Ok(Ok((ev.tid, a, l)));
                }
                StopReason::Breakpoint { addr } => {
                    if let Some((tid, entry, cfa)) = guard
                        && addr == entry
                        && ev.tid == tid
                        && self.t.registers(tid)?.sp() == Some(cfa - 8)
                    {
                        return Ok(Err(NoWrite::FrameEntered));
                    }
                }
                StopReason::HistoryStart => return Ok(Err(NoWrite::HistoryStart)),
                StopReason::Exited { .. } | StopReason::Killed { .. } | StopReason::HistoryEnd => {
                    return Err(HopperError::Protocol(format!(
                        "reverse execution reported {:?}",
                        ev.reason
                    )));
                }
                _ => {}
            }
        }
    }

    /// From the writing instruction (the replica is just before it), find where the byte
    /// `watched` got its value: the source place, how the machine made it, and the moment at
    /// which the source bytes are read. `None` source ends the chain with `end`.
    fn source_of(
        &mut self,
        w: &FoundWrite,
    ) -> Result<(MachineKind, Option<(u64, Moment)>, Option<OriginEnd>)> {
        let Some(insn) = w.insn else {
            return Ok((
                MachineKind::Unknown,
                None,
                Some(OriginEnd::Unknown {
                    reason: format!("cannot decode the writer at {:#x}", w.event.pc),
                }),
            ));
        };
        let regs = self.t.registers(w.tid)?;
        match decode::classify_write(&insn, &regs, w.watched) {
            WriteSource::Memory { addr } => Ok((MachineKind::Copy, Some((addr, self.t.now()?)), None)),
            WriteSource::Immediate => Ok((MachineKind::Immediate, None, Some(OriginEnd::Literal))),
            WriteSource::Syscall => Ok((
                MachineKind::Syscall,
                None,
                Some(OriginEnd::Input {
                    syscall: Some(decode::text(&insn)),
                }),
            )),
            WriteSource::Arithmetic { .. } => {
                Ok((MachineKind::Arithmetic, None, Some(OriginEnd::Computed)))
            }
            WriteSource::Register { reg, offset } => self.register_source(w.tid, reg, offset),
        }
    }

    fn register_source(
        &mut self,
        tid: Tid,
        mut reg: Register,
        offset: u64,
    ) -> Result<(MachineKind, Option<(u64, Moment)>, Option<OriginEnd>)> {
        for _ in 0..MAX_REGISTER_WALK {
            self.spend()?;
            let ev = self.t.resume(Resume::Step { tid }, Direction::Reverse)?;
            if ev.reason == StopReason::HistoryStart {
                return Ok((MachineKind::Copy, None, Some(OriginEnd::HistoryStart)));
            }
            let regs = self.t.registers(tid)?;
            let pc = regs.pc().unwrap_or(0);
            let mut b = [0u8; decode::MAX_INSN];
            if self.t.read_memory(pc, &mut b).is_err() {
                continue;
            }
            let Some(insn) = decode::decode(&b, pc) else {
                continue;
            };
            match decode::register_definition(&insn, &regs, reg, offset) {
                RegisterDefinition::Untouched => {}
                RegisterDefinition::Copy { reg: r } => reg = r,
                RegisterDefinition::Load { addr } => {
                    return Ok((MachineKind::Copy, Some((addr, self.t.now()?)), None));
                }
                RegisterDefinition::Immediate => {
                    return Ok((MachineKind::Immediate, None, Some(OriginEnd::Literal)));
                }
                RegisterDefinition::Syscall => {
                    return Ok((
                        MachineKind::Syscall,
                        None,
                        Some(OriginEnd::Input {
                            syscall: Some("syscall".into()),
                        }),
                    ));
                }
                RegisterDefinition::Arithmetic { .. } => {
                    return Ok((MachineKind::Arithmetic, None, Some(OriginEnd::Computed)));
                }
            }
        }
        Ok((
            MachineKind::Copy,
            None,
            Some(OriginEnd::Unknown {
                reason: format!("no definition of {reg:?} within {MAX_REGISTER_WALK} instructions"),
            }),
        ))
    }

    /// The ownership label of the hop at the writer (the replica is just before it).
    fn label(&mut self, w: &FoundWrite, read: Option<Place>) -> Option<OwnershipLabel> {
        let sidecar = self.shared.sidecar.as_ref()?;
        let syms = self.syms().ok()?;
        let regs = self.t.registers(w.tid).ok()?;
        let at = self.t.now().ok()?;
        let stack = {
            let mut mem = TargetMemory(&mut *self.t);
            syms.set.stack(&mut mem as &mut dyn Memory, &regs, w.tid).ok()?
        };
        let mut pc_info = pc_info(&syms.set, &stack);
        let size = |p: Option<Place>| match p {
            Some(Place::Memory { len, .. }) => len,
            _ => 0,
        };
        let written = Place::memory(self.shared.process, w.watched, w.len);
        let read_name = read.and_then(|p| self.place_name(&syms, &stack, at, p));
        let written_name = self.place_name(&syms, &stack, at, written);
        pc_info.effect = Some(MachineEffect {
            read: read.map(|p| Access {
                size: size(Some(p)),
                place: read_name,
            }),
            written: Some(Access {
                size: w.len,
                place: written_name,
            }),
        });
        sidecar.join(&pc_info).label().cloned()
    }

    /// The source-level place (a local, or one of its fields) whose bytes contain `p`.
    fn place_name(
        &mut self,
        syms: &ProcessSymbols,
        stack: &[StackFrame],
        at: Moment,
        p: Place,
    ) -> Option<String> {
        let Place::Memory { addr, .. } = p else {
            return None;
        };
        let contains = |v: &hopper_core::api::ValueView| match v.place {
            Some(Place::Memory { addr: a, len, .. }) => addr >= a && addr < a + len.max(1),
            _ => false,
        };
        for frame in stack.iter().take(8) {
            let mut mem = TargetMemory(&mut *self.t);
            let mut cx = EvalContext {
                symbols: &syms.set,
                memory: &mut mem,
                frame: FrameRef::from_stack(frame),
                at,
                process: self.shared.process,
            };
            let Ok(locals) = self.shared.evaluator.locals(&mut cx) else {
                continue;
            };
            for v in locals.iter().filter(|v| contains(v)) {
                let mut name = v.name.clone();
                if let Ok(kids) = self
                    .shared
                    .evaluator
                    .children(&mut cx, &v.handle, hopper_core::api::PageReq::first(64))
                    && let Some(k) = kids.iter().find(|k| contains(k))
                {
                    name = if k.name.starts_with('[') {
                        format!("{name}{}", k.name)
                    } else {
                        format!("{name}.{}", k.name)
                    };
                }
                return Some(name);
            }
        }
        None
    }

    /// The chain of writes back from `place` at the replica's current moment.
    pub fn origin(&mut self, place: Place, budget: Budget) -> Result<OriginChain> {
        let Place::Memory { process, .. } = place else {
            return Err(HopperError::Unsupported(
                "the origin of a value held in a register".into(),
            ));
        };
        let mut hops = Vec::new();
        let mut current = place;
        loop {
            if hops.len() as u32 >= budget.max_hops {
                return Ok(OriginChain {
                    hops,
                    end: OriginEnd::Budget,
                });
            }
            let found = match self.last_write(current) {
                Ok(f) => f,
                Err(HopperError::BudgetExhausted { .. }) => {
                    return Ok(OriginChain {
                        hops,
                        end: OriginEnd::Budget,
                    });
                }
                Err(e) => return Err(e),
            };
            let w = match found {
                Ok(w) => w,
                Err(NoWrite::HistoryStart) => {
                    let end = match current {
                        Place::Memory { addr, .. } if self.read_only(addr) => OriginEnd::Literal,
                        _ => OriginEnd::HistoryStart,
                    };
                    return Ok(OriginChain { hops, end });
                }
                Err(NoWrite::FrameEntered) => {
                    return Ok(OriginChain {
                        hops,
                        end: OriginEnd::Unknown {
                            reason: "not written since its frame was entered (an uninitialised \
                                     or reused stack slot)"
                                .into(),
                        },
                    });
                }
            };
            let written = Place::memory(process, w.watched, w.len);
            let (machine, source, end) = match self.source_of(&w) {
                Ok(x) => x,
                Err(HopperError::BudgetExhausted { .. }) => (
                    MachineKind::Unknown,
                    None,
                    Some(OriginEnd::Budget),
                ),
                Err(e) => return Err(e),
            };
            let read = source.map(|(a, _)| Place::memory(process, a, w.len));
            let ownership = self.label(&w, read);
            let allocation = self.shared.allocation_of(w.watched, &w.event.at);
            hops.push(OriginHop {
                at: w.event.at,
                pc: w.event.pc,
                function: w.event.function.clone(),
                location: w.event.location.clone(),
                read,
                written,
                machine,
                ownership,
                allocation,
            });
            if let Some(end) = end {
                return Ok(OriginChain { hops, end });
            }
            let Some((src, moment)) = source else {
                return Ok(OriginChain {
                    hops,
                    end: OriginEnd::Unknown {
                        reason: "no source".into(),
                    },
                });
            };
            if self.read_only(src) {
                return Ok(OriginChain {
                    hops,
                    end: OriginEnd::Literal,
                });
            }
            current = Place::memory(process, src, w.len);
            // The replica is at (or just before) the reading instruction; continue from there.
            if self.t.now()? != moment {
                self.t.seek(&moment)?;
            }
        }
    }

    fn read_only(&mut self, addr: u64) -> bool {
        self.syms().is_ok_and(|s| s.is_read_only(addr))
    }
}

/// The innermost source frame's function and location at `pc`.
pub fn describe(syms: &SymbolSet, pc: u64) -> (Option<String>, Option<SourceLoc>) {
    match syms.frames_at(pc).into_iter().next() {
        Some(f) => (f.function, f.location),
        None => (syms.symbol_at(pc).map(|s| s.label()), None),
    }
}

/// The join's view of a stack: the innermost physical frame with its inline chain, and its
/// callers by call site.
pub fn pc_info(syms: &SymbolSet, stack: &[StackFrame]) -> PcInfo {
    let mut physical: Vec<Vec<&StackFrame>> = Vec::new();
    for f in stack {
        match physical.last_mut() {
            Some(group) if group[0].physical == f.physical => group.push(f),
            _ => physical.push(vec![f]),
        }
    }
    let frame_info = |group: &[&StackFrame]| -> FrameInfo {
        let outer = group.last().expect("non-empty group");
        let function = syms
            .function_at(outer.lookup_pc)
            .map(|f| f.linkage_name.unwrap_or(f.name))
            .or_else(|| outer.frame.function.clone())
            .unwrap_or_default();
        // Innermost first in the stack; the join wants outermost first.
        let mut chain = Vec::new();
        for i in (1..group.len()).rev() {
            if let (Some(func), Some(site)) =
                (group[i - 1].frame.function.clone(), group[i].frame.location.clone())
            {
                chain.push(DwarfInline {
                    function: func,
                    call_site: site,
                });
            }
        }
        FrameInfo {
            function,
            inline_chain: chain,
            location: group[0].frame.location.clone(),
        }
    };
    let mut frames = physical.iter().map(|g| frame_info(g));
    let frame = frames
        .next()
        .unwrap_or_else(|| FrameInfo::new(String::new(), None));
    PcInfo {
        frame,
        callers: frames.collect(),
        effect: None,
    }
}

/// The frame of `stack` with this CFA and function, for re-finding a frame at another moment.
pub fn frame_with(stack: &[StackFrame], cfa: Option<u64>, function: Option<&str>) -> Option<FrameId> {
    stack
        .iter()
        .find(|f| f.cfa == cfa && f.frame.function.as_deref() == function)
        .map(|f| f.frame.id)
}
