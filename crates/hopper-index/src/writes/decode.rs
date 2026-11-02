//! Which bytes an x86-64 instruction writes, from its encoding and the registers before it ran.
//!
//! iced-x86's instruction-info factory lists every memory operand with its access. For each one
//! that is written (`Write`, `CondWrite`, `ReadWrite`, `ReadCondWrite`) the effective address
//! comes from the registers before the instruction (segment registers resolve to their base:
//! `fs_base`/`gs_base`, 0 for the others in 64-bit mode). RIP-relative operands are already
//! absolute in iced's operand list. Implicit stack writes (`push`, `call`) appear there as
//! `[rsp-8]` and so are covered too.
//!
//! A `rep` string instruction writes one element per iteration; the number of iterations is
//! `rcx` before minus `rcx` after (1 when only the before-state is known), and the direction
//! flag decides whether the run grows up or down from `rdi`.
//!
//! Conditional writes (masked stores, `cmpxchg`) are reported as writes: a "maybe written" place
//! is a candidate last writer the caller checks against the bytes.

use hopper_core::{Reg, RegisterSet};
use iced_x86::{
    Code, Decoder, DecoderOptions, Instruction, InstructionInfoFactory, Mnemonic, OpAccess, OpKind,
    Register,
};

/// The longest x86 instruction.
pub const MAX_INSTRUCTION_LEN: usize = 15;

/// One memory range an instruction writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteSite {
    pub addr: u64,
    pub len: u64,
    /// The written bytes when they follow from the registers alone (a store of a general
    /// register or an immediate, `push`, `call`); `None` otherwise.
    pub value: Option<Vec<u8>>,
}

/// Decode the instruction at `pc` from `bytes` (which start at `pc`).
pub fn decode_at(bytes: &[u8], pc: u64) -> Option<Instruction> {
    let mut d = Decoder::with_ip(64, bytes, pc, DecoderOptions::NONE);
    if !d.can_decode() {
        return None;
    }
    let insn = d.decode();
    (!insn.is_invalid()).then_some(insn)
}

/// The DWARF register holding iced's 64-bit general register `r`.
fn dwarf_gpr(r: Register) -> Option<Reg> {
    Some(match r {
        Register::RAX => Reg::RAX,
        Register::RDX => Reg::RDX,
        Register::RCX => Reg::RCX,
        Register::RBX => Reg::RBX,
        Register::RSI => Reg::RSI,
        Register::RDI => Reg::RDI,
        Register::RBP => Reg::RBP,
        Register::RSP => Reg::RSP,
        Register::R8 => Reg(8),
        Register::R9 => Reg(9),
        Register::R10 => Reg(10),
        Register::R11 => Reg(11),
        Register::R12 => Reg(12),
        Register::R13 => Reg(13),
        Register::R14 => Reg(14),
        Register::R15 => Reg(15),
        Register::RIP => Reg::RIP,
        _ => return None,
    })
}

/// The value of iced register `r` (a general register of any width, or a segment register as
/// its base) in `regs`.
pub fn register_value(regs: &RegisterSet, r: Register) -> Option<u64> {
    match r {
        Register::FS => return regs.get(Reg::FS_BASE).or(Some(0)),
        Register::GS => return regs.get(Reg::GS_BASE).or(Some(0)),
        Register::ES | Register::CS | Register::SS | Register::DS => return Some(0),
        _ => {}
    }
    if !r.is_gpr() && r != Register::RIP && r != Register::EIP {
        return None;
    }
    let full = if r == Register::EIP {
        Register::RIP
    } else {
        r.full_register()
    };
    let v = regs.get(dwarf_gpr(full)?)?;
    Some(match r.size() {
        1 if matches!(r, Register::AH | Register::CH | Register::DH | Register::BH) => {
            (v >> 8) & 0xff
        }
        1 => v & 0xff,
        2 => v & 0xffff,
        4 => v & 0xffff_ffff,
        _ => v,
    })
}

/// The element size of a string store (`stos`, `movs`), which iced reports as an unknown
/// memory size when the instruction has a `rep` prefix.
fn string_store_element(code: Code) -> Option<u64> {
    Some(match code.mnemonic() {
        Mnemonic::Stosb | Mnemonic::Movsb => 1,
        Mnemonic::Stosw | Mnemonic::Movsw => 2,
        Mnemonic::Stosd | Mnemonic::Movsd => 4,
        Mnemonic::Stosq | Mnemonic::Movsq => 8,
        _ => return None,
    })
}

fn truncated(v: u64, len: u64) -> Vec<u8> {
    v.to_le_bytes()[..(len.min(8) as usize)].to_vec()
}

/// The bytes a simple store writes, from the registers alone.
fn register_value_written(insn: &Instruction, before: &RegisterSet, len: u64) -> Option<Vec<u8>> {
    if len > 8 {
        return None;
    }
    match insn.mnemonic() {
        Mnemonic::Mov | Mnemonic::Movnti if insn.op0_kind() == OpKind::Memory => match insn
            .op1_kind()
        {
            OpKind::Register => Some(truncated(register_value(before, insn.op1_register())?, len)),
            OpKind::Immediate8
            | OpKind::Immediate16
            | OpKind::Immediate32
            | OpKind::Immediate32to64
            | OpKind::Immediate8to32
            | OpKind::Immediate8to64
            | OpKind::Immediate8to16 => Some(truncated(insn.immediate(1), len)),
            _ => None,
        },
        Mnemonic::Push => match insn.op0_kind() {
            OpKind::Register => Some(truncated(register_value(before, insn.op0_register())?, len)),
            OpKind::Immediate8to64
            | OpKind::Immediate32to64
            | OpKind::Immediate8to16
            | OpKind::Immediate16 => Some(truncated(insn.immediate(0), len)),
            _ => None,
        },
        Mnemonic::Call => Some(truncated(insn.next_ip(), len)),
        Mnemonic::Stosb | Mnemonic::Stosw | Mnemonic::Stosd | Mnemonic::Stosq
            if !insn.has_rep_prefix() =>
        {
            Some(truncated(before.get(Reg::RAX)?, len))
        }
        _ => None,
    }
}

/// The memory `insn` writes when it runs from `before`. `after`, the registers once it ran,
/// gives the iteration count of a `rep` string instruction.
pub fn instruction_writes(
    insn: &Instruction,
    before: &RegisterSet,
    after: Option<&RegisterSet>,
) -> Vec<WriteSite> {
    let mut factory = InstructionInfoFactory::new();
    let info = factory.info(insn);
    let mut out = Vec::new();
    for mem in info.used_memory() {
        if !matches!(
            mem.access(),
            OpAccess::Write | OpAccess::CondWrite | OpAccess::ReadWrite | OpAccess::ReadCondWrite
        ) {
            continue;
        }
        let string_elem = string_store_element(insn.code()).filter(|_| insn.has_rep_prefix());
        let elem = string_elem.unwrap_or(mem.memory_size().size() as u64);
        if elem == 0 {
            continue;
        }
        if mem.vsib_size() != 0 {
            // Scatter stores: one element per vector lane; the lane registers are not in a
            // general register set.
            continue;
        }
        let Some(addr) = mem.virtual_address(0, |r, _, _| register_value(before, r)) else {
            continue;
        };
        if string_elem.is_some() {
            let iterations = match (before.get(Reg::RCX), after.and_then(|a| a.get(Reg::RCX))) {
                (Some(b), Some(a)) if b >= a => b - a,
                (Some(b), None) => b.min(1),
                _ => 1,
            };
            if iterations == 0 {
                continue;
            }
            let down = before.get(Reg::RFLAGS).is_some_and(|f| f & (1 << 10) != 0);
            let span = elem * iterations;
            let start = if down {
                addr.wrapping_sub(span - elem)
            } else {
                addr
            };
            out.push(WriteSite {
                addr: start,
                len: span,
                value: None,
            });
            continue;
        }
        out.push(WriteSite {
            addr,
            len: elem,
            value: register_value_written(insn, before, elem),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn regs(pairs: &[(Reg, u64)]) -> RegisterSet {
        let mut r = RegisterSet::default();
        for (k, v) in pairs {
            r.set(*k, *v);
        }
        r
    }

    fn writes(
        bytes: &[u8],
        pc: u64,
        before: &RegisterSet,
        after: Option<&RegisterSet>,
    ) -> Vec<WriteSite> {
        let insn = decode_at(bytes, pc).expect("decodes");
        instruction_writes(&insn, before, after)
    }

    #[test]
    fn plain_stores_and_loads() {
        let r = regs(&[
            (Reg::RBP, 0x7000),
            (Reg::RAX, 0x1122_3344_5566_7788),
            (Reg::RSP, 0x8000),
        ]);
        // mov qword ptr [rbp-0x10], rax
        let w = writes(&[0x48, 0x89, 0x45, 0xf0], 0x1000, &r, None);
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x6ff0,
                len: 8,
                value: Some(0x1122_3344_5566_7788u64.to_le_bytes().to_vec())
            }]
        );
        // mov dword ptr [rbp-4], 3
        let w = writes(&[0xc7, 0x45, 0xfc, 0x03, 0, 0, 0], 0x1000, &r, None);
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x6ffc,
                len: 4,
                value: Some(vec![3, 0, 0, 0])
            }]
        );
        // mov rax, qword ptr [rbp-0x10]: a load writes no memory.
        assert!(writes(&[0x48, 0x8b, 0x45, 0xf0], 0x1000, &r, None).is_empty());
        // add dword ptr [rbp-4], 1: read-write, value not from registers alone.
        let w = writes(&[0x83, 0x45, 0xfc, 0x01], 0x1000, &r, None);
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x6ffc,
                len: 4,
                value: None
            }]
        );
    }

    #[test]
    fn stack_writes_and_rip_relative() {
        let r = regs(&[(Reg::RSP, 0x8000), (Reg::RBX, 0xabcd)]);
        // push rbx
        let w = writes(&[0x53], 0x1000, &r, None);
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x7ff8,
                len: 8,
                value: Some(0xabcdu64.to_le_bytes().to_vec())
            }]
        );
        // call rel32 (+0): pushes the return address 0x1005.
        let w = writes(&[0xe8, 0, 0, 0, 0], 0x1000, &r, None);
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x7ff8,
                len: 8,
                value: Some(0x1005u64.to_le_bytes().to_vec())
            }]
        );
        // mov qword ptr [rip+0x10], rbx at 0x1000 (7 bytes): writes 0x1017.
        let w = writes(&[0x48, 0x89, 0x1d, 0x10, 0, 0, 0], 0x1000, &r, None);
        assert_eq!(w[0].addr, 0x1017);
        // mov qword ptr fs:[0x28], rbx
        let r2 = regs(&[(Reg::FS_BASE, 0x5000), (Reg::RBX, 1)]);
        let w = writes(
            &[0x64, 0x48, 0x89, 0x1c, 0x25, 0x28, 0, 0, 0],
            0x1000,
            &r2,
            None,
        );
        assert_eq!(w[0].addr, 0x5028);
    }

    #[test]
    fn vector_and_string_stores() {
        let r = regs(&[
            (Reg::RDI, 0x9000),
            (Reg::RCX, 10),
            (Reg::RAX, 0x41),
            (Reg::RFLAGS, 0),
        ]);
        // movups xmmword ptr [rdi], xmm0
        let w = writes(&[0x0f, 0x11, 0x07], 0x1000, &r, None);
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x9000,
                len: 16,
                value: None
            }]
        );
        // rep stosb with rcx 10 -> 4 after: six bytes from rdi.
        let after = regs(&[(Reg::RCX, 4)]);
        let w = writes(&[0xf3, 0xaa], 0x1000, &r, Some(&after));
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x9000,
                len: 6,
                value: None
            }]
        );
        // ... going down when DF is set.
        let down = regs(&[(Reg::RDI, 0x9000), (Reg::RCX, 3), (Reg::RFLAGS, 1 << 10)]);
        let w = writes(
            &[0xf3, 0x48, 0xab],
            0x1000,
            &down,
            Some(&regs(&[(Reg::RCX, 0)])),
        );
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x9000 - 16,
                len: 24,
                value: None
            }]
        );
        // A plain stosb writes al.
        let w = writes(&[0xaa], 0x1000, &r, None);
        assert_eq!(
            w,
            vec![WriteSite {
                addr: 0x9000,
                len: 1,
                value: Some(vec![0x41])
            }]
        );
    }
}
