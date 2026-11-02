//! Reading one x86-64 instruction for the engine: whether it calls or returns (for stepping), and,
//! for provenance, how the bytes it wrote got their value.
//!
//! A write is classified by the instruction that made it ([`WriteSource`]):
//! - a string move (`rep movs`) copies from memory: the source of written byte `W` is
//!   `W + (rsi - rdi)`, with the registers as they are before the iteration that wrote it;
//! - a store from a register (`mov [m], r`, `movups [m], xmm`, `push r`) copies a register, whose
//!   own definition is found by walking back ([`RegisterDefinition`]): a load (`mov r, [m]`,
//!   `vmovdqu ymm, [m]`), another register, an immediate, or a computation;
//! - an immediate store, `call` (the return address) and zeroing idioms are constants;
//! - `syscall` is the kernel;
//! - anything else that writes is arithmetic, reported with its operands.

use hopper_core::{Reg, RegisterSet};
use iced_x86::{
    Decoder, DecoderOptions, FlowControl, Formatter, Instruction, InstructionInfoFactory,
    IntelFormatter, Mnemonic, OpAccess, OpKind, Register,
};

/// The longest x86 instruction.
pub const MAX_INSN: usize = 15;

/// Decode the instruction at `ip` from `bytes` (which start at `ip`). `None` when invalid.
pub fn decode(bytes: &[u8], ip: u64) -> Option<Instruction> {
    let mut d = Decoder::with_ip(64, bytes, ip, DecoderOptions::NONE);
    let insn = d.decode();
    (!insn.is_invalid()).then_some(insn)
}

/// Intel-syntax text of an instruction.
pub fn text(insn: &Instruction) -> String {
    let mut f = IntelFormatter::new();
    let mut s = String::new();
    f.format(insn, &mut s);
    s
}

/// What an instruction does to control flow, as stepping needs it.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Flow {
    /// A call; `next` is the return address.
    Call {
        next: u64,
    },
    Return,
    Other,
}

pub fn flow(insn: &Instruction) -> Flow {
    match insn.flow_control() {
        FlowControl::Call | FlowControl::IndirectCall => Flow::Call {
            next: insn.next_ip(),
        },
        FlowControl::Return => Flow::Return,
        _ => Flow::Other,
    }
}

/// hopper's DWARF register for an iced register's full 64-bit register.
pub fn dwarf_reg(r: Register) -> Option<Reg> {
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
    if let Some(i) = gpr.iter().position(|g| *g == full) {
        return Some(Reg(i as u16));
    }
    // xmm/ymm/zmm n share DWARF number 17 + n (hopper models the low 16 bytes).
    let n = match full {
        f if (Register::ZMM0..=Register::ZMM15).contains(&f) => f as u32 - Register::ZMM0 as u32,
        _ => return None,
    };
    Some(Reg(17 + n as u16))
}

/// The value of an iced register (as an address input) from a register set.
fn reg_value(regs: &RegisterSet, r: Register) -> Option<u64> {
    match r {
        Register::FS => return regs.get(Reg::FS_BASE),
        Register::GS => return regs.get(Reg::GS_BASE),
        Register::ES | Register::CS | Register::SS | Register::DS => return Some(0),
        Register::None => return Some(0),
        _ => {}
    }
    let v = regs.get(dwarf_reg(r)?)?;
    Some(match r.size() {
        8 => v,
        4 => v & 0xffff_ffff,
        2 => v & 0xffff,
        1 => v & 0xff,
        _ => v,
    })
}

/// The effective address of memory operand `op`, with the registers as they are before the
/// instruction runs.
pub fn effective_address(insn: &Instruction, op: u32, regs: &RegisterSet) -> Option<u64> {
    insn.virtual_address(op, 0, |r, _, _| reg_value(regs, r))
}

/// How the bytes an instruction wrote got their value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteSource {
    /// Copied from memory: the written byte at the watched address came from `addr`.
    Memory { addr: u64 },
    /// Copied from a register; the byte at the watched address is byte `offset` of it.
    Register { reg: Register, offset: u64 },
    /// A constant: an immediate operand, a return address, a zeroing idiom.
    Immediate,
    /// The kernel.
    Syscall,
    /// Computed from these operands.
    Arithmetic { operands: Vec<Operand> },
}

/// One input of a computation.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Operand {
    Register(Register),
    /// Bytes read from memory (the effective address with the registers before the
    /// instruction).
    Memory {
        addr: u64,
        len: u64,
    },
}

/// The explicit operands an instruction reads, in operand order: registers it reads (a
/// read-modify-write destination included) and the memory it loads. Immediates are constants and
/// are left out; registers used only to form an address are not operands.
pub fn operands(insn: &Instruction, regs: &RegisterSet) -> Vec<Operand> {
    let mut factory = InstructionInfoFactory::new();
    let info = factory.info(insn);
    let reads = |a: OpAccess| {
        matches!(
            a,
            OpAccess::Read | OpAccess::ReadWrite | OpAccess::CondRead | OpAccess::ReadCondWrite
        )
    };
    let mut out = Vec::new();
    for i in 0..insn.op_count() {
        if !reads(info.op_access(i)) {
            continue;
        }
        match insn.op_kind(i) {
            OpKind::Register => out.push(Operand::Register(insn.op_register(i))),
            OpKind::Memory => {
                if let Some(addr) = effective_address(insn, i, regs) {
                    let len = insn.memory_size().size() as u64;
                    out.push(Operand::Memory { addr, len });
                }
            }
            _ => {}
        }
    }
    out
}

/// Classify the write the instruction made to the byte at `watched`. `regs` are the registers
/// before it ran.
pub fn classify_write(insn: &Instruction, regs: &RegisterSet, watched: u64) -> WriteSource {
    let m = insn.mnemonic();
    if matches!(m, Mnemonic::Syscall | Mnemonic::Sysenter | Mnemonic::Int) {
        return WriteSource::Syscall;
    }
    if matches!(
        m,
        Mnemonic::Movsb | Mnemonic::Movsw | Mnemonic::Movsd | Mnemonic::Movsq
    ) && insn.op_count() == 2
        && insn.op0_kind() == OpKind::MemoryESRDI
    {
        let (Some(rsi), Some(rdi)) = (regs.get(Reg::RSI), regs.get(Reg::RDI)) else {
            return WriteSource::Arithmetic {
                operands: operands(insn, regs),
            };
        };
        return WriteSource::Memory {
            addr: watched.wrapping_add(rsi).wrapping_sub(rdi),
        };
    }
    if matches!(m, Mnemonic::Call) {
        return WriteSource::Immediate;
    }
    if matches!(m, Mnemonic::Push) {
        return match insn.op0_kind() {
            OpKind::Register => {
                let sp = regs.get(Reg::RSP).unwrap_or(0).wrapping_sub(8);
                WriteSource::Register {
                    reg: insn.op0_register(),
                    offset: watched.wrapping_sub(sp),
                }
            }
            OpKind::Memory => match effective_address(insn, 0, regs) {
                Some(ea) => {
                    let sp = regs.get(Reg::RSP).unwrap_or(0).wrapping_sub(8);
                    WriteSource::Memory {
                        addr: ea + watched.wrapping_sub(sp),
                    }
                }
                None => WriteSource::Arithmetic {
                    operands: operands(insn, regs),
                },
            },
            _ => WriteSource::Immediate,
        };
    }
    let is_move = matches!(
        m,
        Mnemonic::Mov
            | Mnemonic::Movups
            | Mnemonic::Movaps
            | Mnemonic::Movupd
            | Mnemonic::Movapd
            | Mnemonic::Movdqu
            | Mnemonic::Movdqa
            | Mnemonic::Vmovups
            | Mnemonic::Vmovaps
            | Mnemonic::Vmovdqu
            | Mnemonic::Vmovdqa
            | Mnemonic::Vmovdqu8
            | Mnemonic::Vmovdqu64
            | Mnemonic::Movq
            | Mnemonic::Vmovq
            | Mnemonic::Movd
            | Mnemonic::Vmovd
            | Mnemonic::Movsd
            | Mnemonic::Vmovsd
            | Mnemonic::Movss
            | Mnemonic::Vmovss
            | Mnemonic::Movlps
            | Mnemonic::Movhps
            | Mnemonic::Movnti
            | Mnemonic::Movntdq
            | Mnemonic::Vmovntdq
    );
    if is_move && insn.op_count() == 2 && insn.op0_kind() == OpKind::Memory {
        let ea = effective_address(insn, 0, regs).unwrap_or(watched);
        let offset = watched.wrapping_sub(ea);
        return match insn.op1_kind() {
            OpKind::Register => WriteSource::Register {
                reg: insn.op1_register(),
                offset: if matches!(m, Mnemonic::Movhps) {
                    offset + 8
                } else {
                    offset
                },
            },
            _ => WriteSource::Immediate,
        };
    }
    WriteSource::Arithmetic {
        operands: operands(insn, regs),
    }
}

/// What an earlier instruction did to a register the walk back is following.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterDefinition {
    /// It does not write the register.
    Untouched,
    /// Loaded from memory: byte `offset` of the register came from `addr`.
    Load { addr: u64 },
    /// Copied from another register.
    Copy { reg: Register },
    /// A constant (immediate, zeroing idiom, address of read-only code).
    Immediate,
    /// The kernel wrote it (the syscall result).
    Syscall,
    /// Computed from these operands.
    Arithmetic { operands: Vec<Operand> },
}

/// How `insn` defines byte `offset` of register `target` (full register compared), with `regs`
/// the registers before it ran.
pub fn register_definition(
    insn: &Instruction,
    regs: &RegisterSet,
    target: Register,
    offset: u64,
) -> RegisterDefinition {
    let full = target.full_register();
    // The kernel's result: iced does not list `syscall`'s write of rax among its registers.
    if insn.mnemonic() == Mnemonic::Syscall && full == Register::RAX {
        return RegisterDefinition::Syscall;
    }
    let mut info = InstructionInfoFactory::new();
    let writes = info.info(insn).used_registers().iter().any(|u| {
        u.register().full_register() == full
            && matches!(
                u.access(),
                OpAccess::Write
                    | OpAccess::ReadWrite
                    | OpAccess::CondWrite
                    | OpAccess::ReadCondWrite
            )
    });
    if !writes {
        return RegisterDefinition::Untouched;
    }
    let m = insn.mnemonic();
    if matches!(m, Mnemonic::Syscall) {
        return RegisterDefinition::Syscall;
    }
    let op0_is_target = insn.op_count() >= 1
        && insn.op0_kind() == OpKind::Register
        && insn.op0_register().full_register() == full;
    // Zeroing idioms.
    if matches!(
        m,
        Mnemonic::Xor | Mnemonic::Pxor | Mnemonic::Vpxor | Mnemonic::Xorps
    ) && insn.op_count() >= 2
        && insn.op1_kind() == OpKind::Register
        && insn.op0_kind() == OpKind::Register
        && insn.op_register(insn.op_count() - 1) == insn.op_register(insn.op_count() - 2)
    {
        return RegisterDefinition::Immediate;
    }
    let loads = matches!(
        m,
        Mnemonic::Mov
            | Mnemonic::Movzx
            | Mnemonic::Movsx
            | Mnemonic::Movsxd
            | Mnemonic::Movups
            | Mnemonic::Movaps
            | Mnemonic::Movupd
            | Mnemonic::Movapd
            | Mnemonic::Movdqu
            | Mnemonic::Movdqa
            | Mnemonic::Vmovups
            | Mnemonic::Vmovaps
            | Mnemonic::Vmovdqu
            | Mnemonic::Vmovdqa
            | Mnemonic::Vmovdqu8
            | Mnemonic::Vmovdqu64
            | Mnemonic::Lddqu
            | Mnemonic::Vlddqu
            | Mnemonic::Movq
            | Mnemonic::Vmovq
            | Mnemonic::Movd
            | Mnemonic::Vmovd
            | Mnemonic::Movsd
            | Mnemonic::Vmovsd
            | Mnemonic::Movss
            | Mnemonic::Vmovss
            | Mnemonic::Pop
    );
    if loads && (op0_is_target || m == Mnemonic::Pop) {
        if m == Mnemonic::Pop {
            return match regs.get(Reg::RSP) {
                Some(sp) => RegisterDefinition::Load { addr: sp + offset },
                None => RegisterDefinition::Arithmetic {
                    operands: operands(insn, regs),
                },
            };
        }
        let src = insn.op_count() - 1;
        return match insn.op_kind(src) {
            OpKind::Memory => match effective_address(insn, src, regs) {
                Some(ea) => RegisterDefinition::Load { addr: ea + offset },
                None => RegisterDefinition::Arithmetic {
                    operands: operands(insn, regs),
                },
            },
            OpKind::Register => RegisterDefinition::Copy {
                reg: insn.op_register(src),
            },
            _ => RegisterDefinition::Immediate,
        };
    }
    if m == Mnemonic::Lea
        && op0_is_target
        && insn.memory_base() == Register::RIP
        && insn.memory_index() == Register::None
    {
        // The address of something in the image: a constant.
        return RegisterDefinition::Immediate;
    }
    RegisterDefinition::Arithmetic {
        operands: operands(insn, regs),
    }
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

    fn insn(bytes: &[u8]) -> Instruction {
        decode(bytes, 0x1000).expect("decodes")
    }

    #[test]
    fn calls_and_returns() {
        // call rel32
        assert_eq!(
            flow(&insn(&[0xe8, 0, 0, 0, 0])),
            Flow::Call { next: 0x1005 }
        );
        // call qword ptr [rip+0]
        assert_eq!(
            flow(&insn(&[0xff, 0x15, 0, 0, 0, 0])),
            Flow::Call { next: 0x1006 }
        );
        assert_eq!(flow(&insn(&[0xc3])), Flow::Return);
        assert_eq!(flow(&insn(&[0x90])), Flow::Other);
    }

    #[test]
    fn arithmetic_reports_its_operands() {
        // add rax, [rbx+8]
        let i = insn(&[0x48, 0x03, 0x43, 0x08]);
        let r = regs(&[(Reg::RBX, 0x7000), (Reg::RAX, 1)]);
        assert_eq!(
            operands(&i, &r),
            vec![
                Operand::Register(Register::RAX),
                Operand::Memory {
                    addr: 0x7008,
                    len: 8
                }
            ]
        );
        assert_eq!(
            register_definition(&i, &r, Register::RAX, 0),
            RegisterDefinition::Arithmetic {
                operands: operands(&i, &r)
            }
        );
        // imul ecx, edx, 3: reads edx only (ecx is written, 3 is a constant).
        let i = insn(&[0x6b, 0xca, 0x03]);
        assert_eq!(operands(&i, &r), vec![Operand::Register(Register::EDX)]);
        // add dword ptr [rsp+4], eax: the stored sum reads both.
        let i = insn(&[0x01, 0x44, 0x24, 0x04]);
        let r = regs(&[(Reg::RSP, 0x8000)]);
        match classify_write(&i, &r, 0x8004) {
            WriteSource::Arithmetic { operands } => assert_eq!(
                operands,
                vec![
                    Operand::Memory {
                        addr: 0x8004,
                        len: 4
                    },
                    Operand::Register(Register::EAX)
                ]
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn rep_movs_copies_from_the_source_buffer() {
        // rep movsb
        let i = insn(&[0xf3, 0xa4]);
        let r = regs(&[(Reg::RSI, 0x5000), (Reg::RDI, 0x9000)]);
        assert_eq!(
            classify_write(&i, &r, 0x9000),
            WriteSource::Memory { addr: 0x5000 }
        );
    }

    #[test]
    fn stores_from_registers_and_immediates() {
        // mov qword ptr [rbp-0x10], rax
        let i = insn(&[0x48, 0x89, 0x45, 0xf0]);
        let r = regs(&[(Reg::RBP, 0x7000)]);
        assert_eq!(
            classify_write(&i, &r, 0x6ff4),
            WriteSource::Register {
                reg: Register::RAX,
                offset: 4
            }
        );
        // mov dword ptr [rbp-0x10], 3
        let i = insn(&[0xc7, 0x45, 0xf0, 3, 0, 0, 0]);
        assert_eq!(classify_write(&i, &r, 0x6ff0), WriteSource::Immediate);
        // vmovdqu ymmword ptr [rdi], ymm0
        let i = insn(&[0xc5, 0xfe, 0x7f, 0x07]);
        let r = regs(&[(Reg::RDI, 0x8000)]);
        assert_eq!(
            classify_write(&i, &r, 0x8003),
            WriteSource::Register {
                reg: Register::YMM0,
                offset: 3
            }
        );
        // add qword ptr [rax], rcx
        let i = insn(&[0x48, 0x01, 0x08]);
        assert!(matches!(
            classify_write(&i, &RegisterSet::default(), 0),
            WriteSource::Arithmetic { .. }
        ));
        assert_eq!(
            classify_write(&insn(&[0x0f, 0x05]), &r, 0),
            WriteSource::Syscall
        );
    }

    #[test]
    fn register_definitions() {
        let r = regs(&[(Reg::RSI, 0x5000), (Reg::RSP, 0x7000)]);
        // vmovdqu ymm0, ymmword ptr [rsi+0x20]
        let i = insn(&[0xc5, 0xfe, 0x6f, 0x46, 0x20]);
        assert_eq!(
            register_definition(&i, &r, Register::YMM0, 3),
            RegisterDefinition::Load { addr: 0x5023 }
        );
        // mov rax, rcx
        let i = insn(&[0x48, 0x89, 0xc8]);
        assert_eq!(
            register_definition(&i, &r, Register::RAX, 0),
            RegisterDefinition::Copy { reg: Register::RCX }
        );
        assert_eq!(
            register_definition(&i, &r, Register::RDX, 0),
            RegisterDefinition::Untouched
        );
        // mov eax, 5 defines rax
        let i = insn(&[0xb8, 5, 0, 0, 0]);
        assert_eq!(
            register_definition(&i, &r, Register::RAX, 0),
            RegisterDefinition::Immediate
        );
        // xor eax, eax
        assert_eq!(
            register_definition(&insn(&[0x31, 0xc0]), &r, Register::RAX, 0),
            RegisterDefinition::Immediate
        );
        // pop rbx
        assert_eq!(
            register_definition(&insn(&[0x5b]), &r, Register::RBX, 0),
            RegisterDefinition::Load { addr: 0x7000 }
        );
        // add rax, rcx
        assert!(matches!(
            register_definition(&insn(&[0x48, 0x01, 0xc8]), &r, Register::RAX, 0),
            RegisterDefinition::Arithmetic { .. }
        ));
        // syscall writes rax
        assert_eq!(
            register_definition(&insn(&[0x0f, 0x05]), &r, Register::RAX, 0),
            RegisterDefinition::Syscall
        );
        assert_eq!(dwarf_reg(Register::EAX), Some(Reg::RAX));
        assert_eq!(dwarf_reg(Register::YMM3), Some(Reg(20)));
    }
}
