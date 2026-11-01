//! Stack unwinding: CFI (`.eh_frame`, `.debug_frame`) with gimli's `UnwindContext`, falling back
//! to the `rbp` chain where a module has no CFI for the PC. Physical frames are then expanded into
//! source frames, one per inlined call.

use gimli::{CfaRule, Encoding, Evaluation, EvaluationResult, Location, RegisterRule, UnwindContext};
use hopper_core::api::Frame;
use hopper_core::target::Memory;
use hopper_core::{FrameId, HopperError, Reg, RegisterSet, Result, Tid};

use crate::cfi::FdeSource;
use crate::dwarf::{DieRef, Reader};
use crate::module::{ModuleId, SymbolSet};

/// How a physical frame's registers were recovered.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum UnwindMethod {
    /// The innermost frame: the thread's own registers.
    Initial,
    /// From call-frame information.
    CallFrameInfo,
    /// From the saved-`rbp` chain.
    FramePointer,
}

/// One machine frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalFrame {
    /// The frame's PC: the thread's PC for the innermost frame, the return address otherwise.
    pub pc: u64,
    /// The canonical frame address, when CFI (or the frame pointer) gave one.
    pub cfa: Option<u64>,
    /// The registers this frame can be trusted to have. Caller-saved registers are dropped when
    /// unwinding past a call, unless the CFI says where they were saved.
    pub regs: RegisterSet,
    pub method: UnwindMethod,
}

impl PhysicalFrame {
    /// The address to look up symbols, lines and CFI for: one byte back from a return address, so
    /// a call at the very end of a function still maps to that function and to the call's line.
    pub fn lookup_pc(&self) -> u64 {
        match self.method {
            UnwindMethod::Initial => self.pc,
            _ => self.pc.saturating_sub(1),
        }
    }
}

/// A source-level frame with what the evaluator needs to read its variables.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StackFrame {
    pub frame: Frame,
    /// The physical frame's registers (shared by its inlined frames).
    pub regs: RegisterSet,
    pub cfa: Option<u64>,
    /// See [`PhysicalFrame::lookup_pc`].
    pub lookup_pc: u64,
    pub module: Option<ModuleId>,
    /// The frame's `DW_TAG_subprogram` or `DW_TAG_inlined_subroutine`.
    pub die: Option<DieRef>,
    /// Index of the physical frame this source frame belongs to.
    pub physical: u32,
}

/// Registers the x86-64 System V ABI makes callee-saved: they keep their value across a call
/// unless CFI says otherwise.
const CALLEE_SAVED: [Reg; 6] = [Reg::RBX, Reg::RBP, Reg(12), Reg(13), Reg(14), Reg(15)];

/// The deepest stack hopper unwinds.
pub const MAX_FRAMES: usize = 512;

impl SymbolSet {
    /// The machine frames of a thread whose registers are `regs`, innermost first.
    pub fn unwind(&self, memory: &mut dyn Memory, regs: &RegisterSet) -> Result<Vec<PhysicalFrame>> {
        let pc = regs.pc().ok_or_else(|| HopperError::Invalid("register set has no rip".into()))?;
        let mut frames = vec![PhysicalFrame {
            pc,
            cfa: None,
            regs: regs.clone(),
            method: UnwindMethod::Initial,
        }];
        while frames.len() < MAX_FRAMES {
            let last = frames.last_mut().expect("non-empty");
            let step = self.step(memory, last);
            last.cfa = step.cfa;
            let Some(next) = step.caller else { break };
            // A caller's stack pointer is above its callee's; anything else is a corrupt stack.
            let (Some(sp), Some(next_sp)) = (last.regs.sp(), next.regs.sp()) else { break };
            if next_sp <= sp || next.pc == 0 {
                break;
            }
            frames.push(next);
        }
        Ok(frames)
    }

    /// The source frames of a thread: each physical frame expanded into its inlined calls, with
    /// depths counted across both.
    pub fn stack(
        &self,
        memory: &mut dyn Memory,
        regs: &RegisterSet,
        tid: Tid,
    ) -> Result<Vec<StackFrame>> {
        let physical = self.unwind(memory, regs)?;
        let mut out = Vec::new();
        for (k, p) in physical.iter().enumerate() {
            let lookup = p.lookup_pc();
            let module = self.module_at(lookup).map(|(id, _)| id);
            let inline = self.frames_at(lookup);
            if inline.is_empty() {
                out.push(StackFrame {
                    frame: Frame {
                        id: FrameId { tid, depth: out.len() as u32 },
                        pc: p.pc,
                        cfa: p.cfa,
                        function: None,
                        location: None,
                        inlined: false,
                    },
                    regs: p.regs.clone(),
                    cfa: p.cfa,
                    lookup_pc: lookup,
                    module,
                    die: None,
                    physical: k as u32,
                });
                continue;
            }
            for f in inline {
                out.push(StackFrame {
                    frame: Frame {
                        id: FrameId { tid, depth: out.len() as u32 },
                        pc: p.pc,
                        cfa: p.cfa,
                        function: f.function,
                        location: f.location,
                        inlined: f.inlined,
                    },
                    regs: p.regs.clone(),
                    cfa: p.cfa,
                    lookup_pc: lookup,
                    module,
                    die: f.die,
                    physical: k as u32,
                });
            }
        }
        Ok(out)
    }

    /// Unwind one frame: its CFA and, unless it is the outermost, its caller.
    fn step(&self, memory: &mut dyn Memory, frame: &PhysicalFrame) -> Step {
        let lookup = frame.lookup_pc();
        if let Some((_, m)) = self.module_at(lookup) {
            let cfi = m.binary().cfi();
            let file_pc = m.to_file(lookup);
            if let Some((source, fde)) = cfi.fde_for(file_pc) {
                let mut ctx = UnwindContext::new();
                let row = match source {
                    FdeSource::EhFrame => cfi.eh_frame.as_ref().and_then(|s| {
                        fde.unwind_info_for_address(s, &cfi.bases, &mut ctx, file_pc).ok().cloned()
                    }),
                    FdeSource::DebugFrame => cfi.debug_frame.as_ref().and_then(|s| {
                        fde.unwind_info_for_address(s, &cfi.bases, &mut ctx, file_pc).ok().cloned()
                    }),
                };
                if let Some(row) = row {
                    let section_data = match source {
                        FdeSource::EhFrame => cfi.eh_frame.as_ref().map(|s| expr_section(s)),
                        FdeSource::DebugFrame => cfi.debug_frame.as_ref().map(|s| expr_section(s)),
                    };
                    let ra = fde.cie().return_address_register();
                    if let Some(step) =
                        apply_row(memory, frame, &row, ra, section_data.as_ref(), m.bias())
                    {
                        return step;
                    }
                }
            }
        }
        frame_pointer_step(memory, frame)
    }
}

struct Step {
    cfa: Option<u64>,
    caller: Option<PhysicalFrame>,
}

/// A CFI section, kept so rule expressions can be read out of it.
enum ExprSection {
    Eh(gimli::EhFrame<Reader>),
    Debug(gimli::DebugFrame<Reader>),
}

trait IntoExprSection {
    fn into_expr_section(&self) -> ExprSection;
}

impl IntoExprSection for gimli::EhFrame<Reader> {
    fn into_expr_section(&self) -> ExprSection {
        ExprSection::Eh(self.clone())
    }
}

impl IntoExprSection for gimli::DebugFrame<Reader> {
    fn into_expr_section(&self) -> ExprSection {
        ExprSection::Debug(self.clone())
    }
}

fn expr_section<S: IntoExprSection>(s: &S) -> ExprSection {
    s.into_expr_section()
}

impl ExprSection {
    fn expression(&self, e: &gimli::UnwindExpression<usize>) -> Option<gimli::Expression<Reader>> {
        match self {
            ExprSection::Eh(s) => e.get(s).ok(),
            ExprSection::Debug(s) => e.get(s).ok(),
        }
    }
}

fn apply_row(
    memory: &mut dyn Memory,
    frame: &PhysicalFrame,
    row: &gimli::UnwindTableRow<usize>,
    ra_reg: gimli::Register,
    section: Option<&ExprSection>,
    bias: u64,
) -> Option<Step> {
    let regs = &frame.regs;
    let cfa = match row.cfa() {
        CfaRule::RegisterAndOffset { register, offset } => {
            regs.get(Reg(register.0))?.wrapping_add_signed(*offset)
        }
        CfaRule::Expression(e) => {
            let expr = section?.expression(e)?;
            match eval_unwind_expr(memory, regs, expr, None, bias)? {
                ExprResult::Address(a) | ExprResult::Value(a) => a,
            }
        }
    };
    let mut next = RegisterSet::default();
    for r in CALLEE_SAVED {
        if let Some(b) = regs.get_bytes(r) {
            next.set_bytes(r, b.to_vec());
        }
    }
    for &(reg, ref rule) in row.registers() {
        let r = Reg(reg.0);
        match rule_value(memory, regs, rule, cfa, section, bias) {
            RuleValue::Value(v) => next.set(r, v),
            RuleValue::Undefined => next.values.retain(|(x, _)| *x != r),
            RuleValue::Same => {
                if let Some(b) = regs.get_bytes(r) {
                    next.set_bytes(r, b.to_vec());
                }
            }
            RuleValue::Unreadable => next.values.retain(|(x, _)| *x != r),
        }
    }
    let ra_rule = row.register(ra_reg).unwrap_or(RegisterRule::Undefined);
    let ra = match rule_value(memory, regs, &ra_rule, cfa, section, bias) {
        RuleValue::Value(v) => v,
        RuleValue::Same => regs.get(Reg(ra_reg.0))?,
        // `.cfi_undefined rip` marks the outermost frame (`_start`, thread entry).
        RuleValue::Undefined | RuleValue::Unreadable => {
            return Some(Step { cfa: Some(cfa), caller: None });
        }
    };
    next.values.retain(|(x, _)| *x != Reg::RIP);
    next.set(Reg::RIP, ra);
    next.set(Reg::RSP, cfa);
    Some(Step {
        cfa: Some(cfa),
        caller: Some(PhysicalFrame {
            pc: ra,
            cfa: None,
            regs: next,
            method: UnwindMethod::CallFrameInfo,
        }),
    })
}

enum RuleValue {
    Value(u64),
    Same,
    Undefined,
    Unreadable,
}

fn rule_value(
    memory: &mut dyn Memory,
    regs: &RegisterSet,
    rule: &RegisterRule<usize>,
    cfa: u64,
    section: Option<&ExprSection>,
    bias: u64,
) -> RuleValue {
    let read = |memory: &mut dyn Memory, a: u64| match memory.read_u64(a) {
        Ok(v) => RuleValue::Value(v),
        Err(_) => RuleValue::Unreadable,
    };
    match rule {
        RegisterRule::Undefined => RuleValue::Undefined,
        RegisterRule::SameValue => RuleValue::Same,
        RegisterRule::Offset(n) => read(memory, cfa.wrapping_add_signed(*n)),
        RegisterRule::ValOffset(n) => RuleValue::Value(cfa.wrapping_add_signed(*n)),
        RegisterRule::Register(r) => match regs.get(Reg(r.0)) {
            Some(v) => RuleValue::Value(v),
            None => RuleValue::Unreadable,
        },
        RegisterRule::Expression(e) | RegisterRule::ValExpression(e) => {
            let Some(expr) = section.and_then(|s| s.expression(e)) else {
                return RuleValue::Unreadable;
            };
            match eval_unwind_expr(memory, regs, expr, Some(cfa), bias) {
                Some(ExprResult::Address(a)) if matches!(rule, RegisterRule::Expression(_)) => {
                    read(memory, a)
                }
                Some(ExprResult::Address(v) | ExprResult::Value(v)) => RuleValue::Value(v),
                None => RuleValue::Unreadable,
            }
        }
        RegisterRule::Constant(v) => RuleValue::Value(*v),
        RegisterRule::Architectural => RuleValue::Unreadable,
    }
}

enum ExprResult {
    Address(u64),
    Value(u64),
}

/// Evaluate a CFI expression. Register-rule expressions start with the CFA pushed.
fn eval_unwind_expr(
    memory: &mut dyn Memory,
    regs: &RegisterSet,
    expr: gimli::Expression<Reader>,
    initial: Option<u64>,
    bias: u64,
) -> Option<ExprResult> {
    let encoding = Encoding { format: gimli::Format::Dwarf32, version: 4, address_size: 8 };
    let mut eval: Evaluation<Reader> = expr.evaluation(encoding);
    if let Some(v) = initial {
        eval.set_initial_value(v);
    }
    let mut res = eval.evaluate().ok()?;
    loop {
        res = match res {
            EvaluationResult::Complete => break,
            EvaluationResult::RequiresRegister { register, .. } => {
                let v = regs.get(Reg(register.0))?;
                eval.resume_with_register(gimli::Value::Generic(v)).ok()?
            }
            EvaluationResult::RequiresMemory { address, size, .. } => {
                let mut buf = [0u8; 8];
                let n = (size as usize).min(8);
                memory.read(address, &mut buf[..n]).ok()?;
                eval.resume_with_memory(gimli::Value::Generic(u64::from_le_bytes(buf))).ok()?
            }
            EvaluationResult::RequiresRelocatedAddress(a) => {
                eval.resume_with_relocated_address(a.wrapping_add(bias)).ok()?
            }
            _ => return None,
        };
    }
    let pieces = eval.result();
    match pieces.first()?.location {
        Location::Address { address } => Some(ExprResult::Address(address)),
        Location::Value { value } => Some(ExprResult::Value(value.to_u64(u64::MAX).ok()?)),
        _ => None,
    }
}

/// `rbp`-chain unwinding: the caller's CFA is `rbp + 16`, its return address is at `rbp + 8`, and
/// its `rbp` at `rbp`.
fn frame_pointer_step(memory: &mut dyn Memory, frame: &PhysicalFrame) -> Step {
    let none = Step { cfa: None, caller: None };
    let Some(rbp) = frame.regs.get(Reg::RBP) else { return none };
    if rbp == 0 || rbp % 8 != 0 {
        return none;
    }
    let cfa = rbp + 16;
    let (Ok(ra), Ok(saved_rbp)) = (memory.read_u64(rbp + 8), memory.read_u64(rbp)) else {
        return Step { cfa: Some(cfa), caller: None };
    };
    if ra == 0 {
        return Step { cfa: Some(cfa), caller: None };
    }
    let mut next = RegisterSet::default();
    for r in CALLEE_SAVED {
        if let Some(b) = frame.regs.get_bytes(r) {
            next.set_bytes(r, b.to_vec());
        }
    }
    next.set(Reg::RBP, saved_rbp);
    next.set(Reg::RSP, cfa);
    next.set(Reg::RIP, ra);
    Step {
        cfa: Some(cfa),
        caller: Some(PhysicalFrame { pc: ra, cfa: None, regs: next, method: UnwindMethod::FramePointer }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::target::ImageMemory;

    #[test]
    fn frame_pointer_chain_without_symbols() {
        // Two frames: rbp=0x1000 -> [0x1000]=0x1100 (saved rbp), [0x1008]=0x4444 (return).
        // Then rbp=0x1100 -> [0x1100]=0, [0x1108]=0x5555.
        let mut stack = vec![0u8; 0x200];
        stack[0..8].copy_from_slice(&0x1100u64.to_le_bytes());
        stack[8..16].copy_from_slice(&0x4444u64.to_le_bytes());
        stack[0x100..0x108].copy_from_slice(&0u64.to_le_bytes());
        stack[0x108..0x110].copy_from_slice(&0x5555u64.to_le_bytes());
        let mut mem = ImageMemory::default().with(0x1000, stack);
        let mut regs = RegisterSet::default();
        regs.set(Reg::RIP, 0x3333);
        regs.set(Reg::RSP, 0xff0);
        regs.set(Reg::RBP, 0x1000);
        let frames = SymbolSet::new().unwind(&mut mem, &regs).unwrap();
        let pcs: Vec<u64> = frames.iter().map(|f| f.pc).collect();
        assert_eq!(pcs, vec![0x3333, 0x4444, 0x5555]);
        assert_eq!(frames[0].cfa, Some(0x1010));
        assert_eq!(frames[1].method, UnwindMethod::FramePointer);
        assert_eq!(frames[1].regs.sp(), Some(0x1010));
    }
}
