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

use hopper_core::api::{Budget, MachineKind, OriginChain, OriginEnd, OriginHop, WriteEvent};
use hopper_core::target::{
    BreakKind, Breakpoint, Direction, Memory, Resume, StopReason, Target, TargetMemory,
};
use hopper_core::{FrameId, HopperError, Moment, Place, Result, SourceLoc, Tid};
use hopper_eval::{EvalContext, FrameRef};
use hopper_sidecar::{
    Access, DwarfInline, FrameInfo, Join, MachineEffect, PcInfo, Sidecar, SourceEvent, UserFrame,
};
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
/// Levels of fields and elements below a local searched for the place holding a write.
const LANDING_DEPTH: usize = 4;
/// Instructions stepped after a call returns, looking for the variable it initialised to come
/// into scope.
const MAX_SCOPE_STEPS: usize = 64;

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

/// How one hop's bytes got their value.
enum Source {
    /// Copied unchanged from memory at `from`, which held them at moment `at`.
    Copied { from: u64, at: Moment },
    /// The chain ends here.
    End {
        machine: MachineKind,
        end: OriginEnd,
        /// What an arithmetic end computed the bytes from.
        operands: Vec<Place>,
    },
}

impl Source {
    fn end(machine: MachineKind, end: OriginEnd) -> Source {
        Source::End {
            machine,
            end,
            operands: Vec::new(),
        }
    }
}

/// What the walk saw at a hop's writing instruction, for the hop's label and for lifting it to a
/// user-level event once the chain is known.
struct WriterSite {
    tid: Tid,
    /// The writer's stack, innermost first, grouped into physical frames like [`PcInfo`]: for
    /// each, its outermost source frame (the physical function).
    physical: Vec<StackFrame>,
    /// The join's view of the writer, with the observed effect.
    pc: PcInfo,
    join: Join,
    /// The user's own frame the writer belongs to.
    user: Option<UserFrame>,
    /// The written bytes named in the user frame's places, at the writer.
    landing: Option<String>,
    /// The physical depth of the user frame's caller that is user code too.
    caller: Option<usize>,
    /// The written bytes named in that caller's places (for a returned value).
    caller_landing: Option<String>,
}

impl WriterSite {
    /// Hops with the same key are one source-level event: same user frame instance, same line.
    fn key(&self) -> Option<(Option<u64>, usize, Option<u32>)> {
        let user = self.user?;
        let frame = self.physical.get(user.depth)?;
        let line = match user.depth {
            0 => self.pc.frame.location.as_ref(),
            n => self.pc.callers.get(n - 1)?.location.as_ref(),
        }
        .map(|l| l.line);
        Some((frame.cfa, user.function, line))
    }
}

/// A machine hop and what was seen at its writer.
struct Traced {
    hop: OriginHop,
    site: Option<WriterSite>,
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

    /// From the writing instruction (the replica is just before it), how the byte `watched` got
    /// its value: copied from somewhere the walk continues at, or an end of the chain.
    fn source_of(&mut self, w: &FoundWrite) -> Result<Source> {
        let Some(insn) = w.insn else {
            return Ok(Source::end(
                MachineKind::Unknown,
                OriginEnd::Unknown {
                    reason: format!("cannot decode the writer at {:#x}", w.event.pc),
                },
            ));
        };
        let regs = self.t.registers(w.tid)?;
        Ok(match decode::classify_write(&insn, &regs, w.watched) {
            WriteSource::Memory { addr } => Source::Copied {
                from: addr,
                at: self.t.now()?,
            },
            WriteSource::Immediate => Source::end(MachineKind::Immediate, OriginEnd::Literal),
            WriteSource::Syscall => Source::end(
                MachineKind::Syscall,
                OriginEnd::Input {
                    syscall: Some(decode::text(&insn)),
                },
            ),
            WriteSource::Arithmetic { operands } => Source::End {
                machine: MachineKind::Arithmetic,
                end: OriginEnd::Computed,
                operands: self.operand_places(w.tid, &operands),
            },
            WriteSource::Register { reg, offset } => self.register_source(w.tid, reg, offset)?,
        })
    }

    /// Walk back from a store of `reg` to the instruction that defined it.
    fn register_source(&mut self, tid: Tid, mut reg: Register, offset: u64) -> Result<Source> {
        for _ in 0..MAX_REGISTER_WALK {
            self.spend()?;
            let ev = self.t.resume(Resume::Step { tid }, Direction::Reverse)?;
            if ev.reason == StopReason::HistoryStart {
                return Ok(Source::end(MachineKind::Copy, OriginEnd::HistoryStart));
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
            let end = |machine, end| Ok(Source::end(machine, end));
            match decode::register_definition(&insn, &regs, reg, offset) {
                RegisterDefinition::Untouched => {}
                RegisterDefinition::Copy { reg: r } => reg = r,
                RegisterDefinition::Load { addr } => {
                    return Ok(Source::Copied {
                        from: addr,
                        at: self.t.now()?,
                    });
                }
                RegisterDefinition::Immediate => {
                    return end(MachineKind::Immediate, OriginEnd::Literal);
                }
                RegisterDefinition::Syscall => {
                    return end(
                        MachineKind::Syscall,
                        OriginEnd::Input {
                            syscall: Some("syscall".into()),
                        },
                    );
                }
                RegisterDefinition::Arithmetic { operands } => {
                    return Ok(Source::End {
                        machine: MachineKind::Arithmetic,
                        end: OriginEnd::Computed,
                        operands: self.operand_places(tid, &operands),
                    });
                }
            }
        }
        Ok(Source::end(
            MachineKind::Copy,
            OriginEnd::Unknown {
                reason: format!("no definition of {reg:?} within {MAX_REGISTER_WALK} instructions"),
            },
        ))
    }

    /// An arithmetic instruction's operands as places: registers of the computing thread's
    /// innermost frame, memory of the process.
    fn operand_places(&self, tid: Tid, operands: &[decode::Operand]) -> Vec<Place> {
        operands
            .iter()
            .filter_map(|o| match *o {
                decode::Operand::Register(r) => Some(Place::Register {
                    frame: FrameId { tid, depth: 0 },
                    reg: decode::dwarf_reg(r)?,
                }),
                decode::Operand::Memory { addr, len } => {
                    Some(Place::memory(self.shared.process, addr, len))
                }
            })
            .collect()
    }

    /// What the join and the lifting need from the writer (the replica is just before it): its
    /// stack, the user frame, and where the written bytes are in the user frame's places.
    fn writer_site(&mut self, w: &FoundWrite) -> Option<(WriterSite, Option<String>)> {
        let sidecar = self.shared.sidecar.clone()?;
        let syms = self.syms().ok()?;
        let regs = self.t.registers(w.tid).ok()?;
        let at = self.t.now().ok()?;
        let stack = {
            let mut mem = TargetMemory(&mut *self.t);
            syms.set
                .stack(&mut mem as &mut dyn Memory, &regs, w.tid)
                .ok()?
        };
        let pc = pc_info(&syms.set, &stack);
        let physical = physical_frames(&stack);
        let user = sidecar.user_frame(&pc);
        let written_name = self.place_name(
            &syms,
            &stack,
            at,
            Place::memory(self.shared.process, w.watched, w.len),
        );
        let landing = user.and_then(|u| {
            let frame = physical.get(u.depth)?.clone();
            self.landing_name(&syms, &frame, at, w.watched)
        });
        let caller = user.and_then(|u| caller_depth(&sidecar, &pc, u));
        let caller_landing = caller.and_then(|depth| {
            let frame = physical.get(depth)?.clone();
            self.landing_name(&syms, &frame, at, w.watched)
        });
        let join = sidecar.join(&pc);
        Some((
            WriterSite {
                tid: w.tid,
                physical,
                pc,
                join,
                user,
                landing,
                caller,
                caller_landing,
            },
            written_name,
        ))
    }

    /// Join the writer with the machine effect the walk observed: the written bytes' place (named
    /// at the writer) and the read bytes' place (named where they were read, the replica's
    /// position now).
    fn join_effect(
        &mut self,
        site: &mut WriterSite,
        w: &FoundWrite,
        written_name: Option<String>,
        read: Option<Place>,
    ) {
        let Some(sidecar) = self.shared.sidecar.clone() else {
            return;
        };
        let read_name = read.and_then(|p| {
            let syms = self.syms().ok()?;
            let tid = self.t.current_thread().ok()?;
            let regs = self.t.registers(tid).ok()?;
            let at = self.t.now().ok()?;
            let stack = {
                let mut mem = TargetMemory(&mut *self.t);
                syms.set
                    .stack(&mut mem as &mut dyn Memory, &regs, tid)
                    .ok()?
            };
            self.place_name(&syms, &stack, at, p)
        });
        let size = |p: Option<Place>| match p {
            Some(Place::Memory { len, .. }) => len,
            _ => 0,
        };
        site.pc.effect = Some(MachineEffect {
            read: read.map(|p| Access {
                size: size(Some(p)),
                place: read_name,
            }),
            written: Some(Access {
                size: w.len,
                place: written_name,
            }),
        });
        site.join = sidecar.join(&site.pc);
    }

    /// The shallowest place of `frame` (a local, then a field or element of one, owned data
    /// before anything behind a reference) whose bytes contain `addr`.
    fn landing_name(
        &mut self,
        syms: &ProcessSymbols,
        frame: &StackFrame,
        at: Moment,
        addr: u64,
    ) -> Option<String> {
        let contains = |v: &hopper_core::api::ValueView| match v.place {
            Some(Place::Memory { addr: a, len, .. }) => addr >= a && addr < a + len.max(1),
            _ => false,
        };
        let mut mem = TargetMemory(&mut *self.t);
        let mut cx = EvalContext {
            symbols: &syms.set,
            memory: &mut mem,
            frame: FrameRef::from_stack(frame),
            at,
            process: self.shared.process,
        };
        let mut level: Vec<(String, hopper_core::api::ValueView)> = self
            .shared
            .evaluator
            .locals(&mut cx)
            .ok()?
            .into_iter()
            .map(|v| (v.name.clone(), v))
            .collect();
        for _ in 0..LANDING_DEPTH {
            if let Some((name, _)) = level.iter().find(|(_, v)| contains(v)) {
                return Some(name.clone());
            }
            let mut next = Vec::new();
            for (name, v) in &level {
                let behind_reference = v.type_name.starts_with('&') || v.type_name.starts_with('*');
                if behind_reference || v.child_count.unwrap_or(0) == 0 {
                    continue;
                }
                let Ok(kids) = self.shared.evaluator.children(
                    &mut cx,
                    &v.handle,
                    hopper_core::api::PageReq::first(64),
                ) else {
                    continue;
                };
                for k in kids {
                    let n = if k.name.starts_with('[') {
                        format!("{name}{}", k.name)
                    } else {
                        format!("{name}.{}", k.name)
                    };
                    next.push((n, k));
                }
            }
            level = next;
        }
        None
    }

    /// Where the bytes a callee wrote are once the user frame at `depth` gets control back:
    /// run forward from the write to the return into that frame and name them there. For
    /// writes into memory the frame does not hold on its stack (a `Vec`'s buffer).
    fn landing_after_return(
        &mut self,
        hop: &OriginHop,
        site: &WriterSite,
        depth: usize,
    ) -> Option<String> {
        let user = site.physical.get(depth)?.clone();
        let callee_cfa = site.physical.get(depth.checked_sub(1)?)?.cfa?;
        let Place::Memory { addr, .. } = hop.written else {
            return None;
        };
        let resume = user.frame.pc;
        if self.t.now().ok()? != hop.at {
            self.t.seek(&hop.at).ok()?;
        }
        let bp = Breakpoint::code(resume);
        self.t.insert(bp).ok()?;
        let arrived = loop {
            if self.spend().is_err() {
                break false;
            }
            let Ok(ev) = self.t.resume(Resume::Continue, Direction::Forward) else {
                break false;
            };
            match ev.reason {
                StopReason::Breakpoint { addr: a } if a == resume => {
                    if ev.tid == site.tid
                        && self.t.registers(site.tid).ok().and_then(|r| r.sp()) == Some(callee_cfa)
                    {
                        break true;
                    }
                }
                StopReason::Breakpoint { .. }
                | StopReason::Watchpoint { .. }
                | StopReason::Step => {}
                _ => break false,
            }
        };
        let _ = self.t.remove(bp);
        if !arrived {
            return None;
        }
        // A variable the call initialises comes into scope with the next line: step to it,
        // within the call's line of this frame.
        let syms = self.syms().ok()?;
        let line = syms.set.location(resume).map(|l| l.line);
        for _ in 0..MAX_SCOPE_STEPS {
            let regs = self.t.registers(site.tid).ok()?;
            let at = self.t.now().ok()?;
            let pc = regs.pc()?;
            if regs.sp() != Some(callee_cfa) {
                return None;
            }
            let stack = {
                let mut mem = TargetMemory(&mut *self.t);
                syms.set
                    .stack(&mut mem as &mut dyn Memory, &regs, site.tid)
                    .ok()?
            };
            let frame = physical_frames(&stack)
                .into_iter()
                .find(|f| f.cfa == user.cfa)?;
            if let Some(name) = self.landing_name(&syms, &frame, at, addr) {
                return Some(name);
            }
            if pc != resume && syms.set.location(pc).map(|l| l.line) != line {
                return None;
            }
            self.spend().ok()?;
            self.t
                .resume(Resume::Step { tid: site.tid }, Direction::Forward)
                .ok()?;
        }
        None
    }

    /// The user-level event the hop is part of: what the join chose in the user's own frame,
    /// what a call there did with the bytes, or, for a store into a user function's return
    /// place, the caller's call.
    fn lift(&mut self, hop: &OriginHop, site: &WriterSite) -> Option<SourceEvent> {
        let sidecar = self.shared.sidecar.clone()?;
        let user = site.user?;
        let event = if user.depth == 0 {
            SourceEvent {
                label: site.join.label()?.clone(),
                inner: Vec::new(),
            }
        } else {
            let landing = match &site.landing {
                Some(l) => Some(l.clone()),
                None => self.landing_after_return(hop, site, user.depth),
            };
            match sidecar.call_transfer(&site.pc, user.depth, landing.as_deref()) {
                Some(e) => e,
                None => SourceEvent {
                    label: site.join.label()?.clone(),
                    inner: Vec::new(),
                },
            }
        };
        let returned = event
            .label
            .dst
            .as_deref()
            .is_some_and(hopper_sidecar::source::is_return_place);
        if returned {
            let chosen = (user.depth == 0).then(|| site.join.event()).flatten();
            // A variable a call initialises is often out of scope at the call (its lexical
            // block starts after it): then it is named once the caller has the value.
            let landing = match (&site.caller_landing, site.caller) {
                (Some(l), _) => Some(l.clone()),
                (None, Some(depth)) => self.landing_after_return(hop, site, depth),
                (None, None) => None,
            };
            if let Some(mut outer) =
                sidecar.returned_value(&site.pc, user, chosen, &event.label, landing.as_deref())
            {
                outer.inner.extend(event.inner);
                return Some(outer);
            }
        }
        Some(event)
    }

    /// Give every hop the user-level event it is part of: consecutive hops in the same user
    /// frame at the same line are one event, decided by the newest of them (the one whose bytes
    /// the user's code sees).
    fn lift_all(&mut self, traced: &mut [Traced]) {
        let mut i = 0;
        while i < traced.len() {
            let key = traced[i].site.as_ref().and_then(WriterSite::key);
            let mut j = i + 1;
            while key.is_some()
                && j < traced.len()
                && traced[j].site.as_ref().and_then(WriterSite::key) == key
            {
                j += 1;
            }
            let mut event = None;
            for t in &traced[i..j] {
                if let Some(site) = &t.site
                    && let Some(e) = self.lift(&t.hop, site)
                {
                    event = Some(e);
                    break;
                }
            }
            if let Some(e) = event {
                for t in &mut traced[i..j] {
                    t.hop.ownership = Some(e.label.clone());
                    t.hop.inner = e.inner.clone();
                }
            }
            i = j;
        }
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
            let Some(v) = locals.iter().find(|v| contains(v)) else {
                continue;
            };
            // A field (or element) when one holds the bytes.
            let kids = self.shared.evaluator.children(
                &mut cx,
                &v.handle,
                hopper_core::api::PageReq::first(64),
            );
            let name = match kids.ok().and_then(|k| k.into_iter().find(|k| contains(k))) {
                Some(k) if k.name.starts_with('[') => format!("{}{}", v.name, k.name),
                Some(k) => format!("{}.{}", v.name, k.name),
                None => v.name.clone(),
            };
            return Some(name);
        }
        None
    }

    /// The chain of writes back from `place` at the replica's current moment.
    pub fn origin(&mut self, place: Place, budget: Budget) -> Result<OriginChain> {
        let mut traced = Vec::new();
        let end = self.walk(place, budget, &mut traced)?;
        self.lift_all(&mut traced);
        Ok(OriginChain {
            hops: traced.into_iter().map(|t| t.hop).collect(),
            end,
            references: Vec::new(),
        })
    }

    /// The write that made a reference's value (the replica is at the moment the reference is
    /// read): one hop, labelled like any other (`n = &v[0].name`), not followed further.
    pub fn reference_hop(&mut self, place: Place) -> Result<Option<OriginHop>> {
        let mut traced = Vec::new();
        let budget = Budget {
            max_hops: 1,
            max_steps: self.budget,
        };
        self.walk(place, budget, &mut traced)?;
        self.lift_all(&mut traced);
        Ok(traced.into_iter().next().map(|t| t.hop))
    }

    /// Follow the writes back from `place`, pushing one hop per write; returns how the chain
    /// ends.
    fn walk(
        &mut self,
        place: Place,
        budget: Budget,
        traced: &mut Vec<Traced>,
    ) -> Result<OriginEnd> {
        let Place::Memory { process, .. } = place else {
            return Err(HopperError::Unsupported(
                "the origin of a value held in a register".into(),
            ));
        };
        let mut current = place;
        loop {
            if traced.len() as u32 >= budget.max_hops {
                return Ok(OriginEnd::Budget);
            }
            let found = match self.last_write(current) {
                Ok(f) => f,
                Err(HopperError::BudgetExhausted { .. }) => return Ok(OriginEnd::Budget),
                Err(e) => return Err(e),
            };
            let w = match found {
                Ok(w) => w,
                Err(NoWrite::HistoryStart) => {
                    return Ok(match current {
                        Place::Memory { addr, .. } if self.read_only(addr) => OriginEnd::Literal,
                        _ => OriginEnd::HistoryStart,
                    });
                }
                Err(NoWrite::FrameEntered) => {
                    return Ok(OriginEnd::Unknown {
                        reason: "not written since its frame was entered (an uninitialised \
                                 or reused stack slot)"
                            .into(),
                    });
                }
            };
            let written = Place::memory(process, w.watched, w.len);
            // The writer's stack, before the walk to the bytes' source moves the replica.
            let mut site = self.writer_site(&w);
            let source = match self.source_of(&w) {
                Ok(x) => x,
                Err(HopperError::BudgetExhausted { .. }) => {
                    Source::end(MachineKind::Unknown, OriginEnd::Budget)
                }
                Err(e) => return Err(e),
            };
            let (machine, read, operands) = match &source {
                Source::Copied { from, .. } => (
                    MachineKind::Copy,
                    Some(Place::memory(process, *from, w.len)),
                    Vec::new(),
                ),
                Source::End {
                    machine, operands, ..
                } => (*machine, None, operands.clone()),
            };
            if let Some((s, written_name)) = site.as_mut() {
                let name = written_name.take();
                self.join_effect(s, &w, name, read);
            }
            let site = site.map(|(s, _)| s);
            let ownership = site.as_ref().and_then(|s| s.join.label().cloned());
            let allocation = self.shared.allocation_of(w.watched, &w.event.at);
            traced.push(Traced {
                hop: OriginHop {
                    at: w.event.at,
                    pc: w.event.pc,
                    function: w.event.function.clone(),
                    location: w.event.location.clone(),
                    read,
                    written,
                    machine,
                    ownership,
                    allocation,
                    inner: Vec::new(),
                    operands,
                },
                site,
            });
            let (src, moment) = match source {
                Source::End { end, .. } => return Ok(end),
                Source::Copied { from, at } => (from, at),
            };
            if self.read_only(src) {
                return Ok(OriginEnd::Literal);
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
            if let (Some(func), Some(site)) = (
                group[i - 1].frame.function.clone(),
                group[i].frame.location.clone(),
            ) {
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

/// The outermost source frame of each physical frame of `stack` (the physical function, whose
/// locals the user sees), innermost first: the same grouping as [`pc_info`].
fn physical_frames(stack: &[StackFrame]) -> Vec<StackFrame> {
    let mut out: Vec<StackFrame> = Vec::new();
    let mut last: Option<u32> = None;
    for f in stack {
        if last == Some(f.physical) {
            *out.last_mut().expect("a group was started") = f.clone();
        } else {
            out.push(f.clone());
            last = Some(f.physical);
        }
    }
    out
}

/// The physical depth of the user frame that called user frame `callee`.
fn caller_depth(sidecar: &Sidecar, pc: &PcInfo, callee: UserFrame) -> Option<usize> {
    let caller = sidecar.user_frame(&PcInfo {
        frame: pc.callers.get(callee.depth)?.clone(),
        callers: pc.callers[callee.depth + 1..].to_vec(),
        effect: None,
    })?;
    Some(callee.depth + 1 + caller.depth)
}

/// The frame of `stack` with this CFA and function, for re-finding a frame at another moment.
pub fn frame_with(
    stack: &[StackFrame],
    cfa: Option<u64>,
    function: Option<&str>,
) -> Option<FrameId> {
    stack
        .iter()
        .find(|f| f.cfa == cfa && f.frame.function.as_deref() == function)
        .map(|f| f.frame.id)
}

/// The source view of an origin chain: one hop per ownership event, as the plan's worked-example
/// table shows it. The references the value was reached through come first (the borrow that made
/// `n`, for `*n`), then the value's own hops. Each machine hop carries the user-level event it is
/// part of, so the several copies one move or one call makes (through MIR temporaries, call
/// arguments and a callee's stores) collapse into the first of them; hops without a label (no
/// sidecar event) are left out. The machine hops stay in the chain.
pub fn source_hops(chain: &OriginChain) -> Vec<OriginHop> {
    let mut out: Vec<OriginHop> = Vec::new();
    for h in chain.references.iter().chain(&chain.hops) {
        let Some(label) = &h.ownership else { continue };
        let same = out
            .last()
            .and_then(|p| p.ownership.as_ref())
            .is_some_and(|l| {
                (l.kind, &l.src, &l.dst, &l.span)
                    == (label.kind, &label.src, &label.dst, &label.span)
            });
        if !same {
            out.push(h.clone());
        }
    }
    out
}
