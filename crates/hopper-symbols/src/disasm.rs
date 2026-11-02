//! x86-64 disassembly with iced-x86, in Intel syntax, annotating branch targets and RIP-relative
//! operands with the symbols they name (`call 0x55555557a2c0 <values::stop_here>`).

use hopper_core::Result;
use hopper_core::api::Instruction;
use hopper_core::target::Memory;
use iced_x86::{Decoder, DecoderOptions, Formatter, IntelFormatter};

use crate::module::SymbolSet;

/// The longest x86 instruction.
const MAX_INSN: usize = 15;

/// Decode `count` instructions from `addr`, reading through `memory`.
///
/// Stops early, without an error, when memory past the last decoded instruction is unreadable;
/// fails only when not even the first instruction can be read. Undecodable bytes become a
/// `(bad)` instruction of one byte so the listing keeps going.
pub fn disassemble(
    symbols: &SymbolSet,
    memory: &mut dyn Memory,
    addr: u64,
    count: u32,
) -> Result<Vec<Instruction>> {
    let bytes = read_prefix(memory, addr, count as usize * MAX_INSN)?;
    Ok(disassemble_bytes(symbols, &bytes, addr, count))
}

/// Decode up to `count` instructions from `bytes`, which start at `addr`.
pub fn disassemble_bytes(
    symbols: &SymbolSet,
    bytes: &[u8],
    addr: u64,
    count: u32,
) -> Vec<Instruction> {
    let mut decoder = Decoder::with_ip(64, bytes, addr, DecoderOptions::NONE);
    let mut formatter = IntelFormatter::new();
    let opts = formatter.options_mut();
    opts.set_hex_prefix("0x");
    opts.set_hex_suffix("");
    opts.set_uppercase_hex(false);
    opts.set_space_after_operand_separator(true);
    opts.set_branch_leading_zeros(false);
    opts.set_rip_relative_addresses(false);
    let mut out = Vec::new();
    let mut insn = iced_x86::Instruction::default();
    while out.len() < count as usize && decoder.can_decode() {
        let pos = decoder.position();
        decoder.decode_out(&mut insn);
        // A truncated instruction at the end of the buffer means the memory ran out.
        if insn.is_invalid() && pos + MAX_INSN > bytes.len() && pos + insn.len() >= bytes.len() {
            break;
        }
        let len = insn.len().max(1);
        let mut text = String::new();
        if insn.is_invalid() {
            text.push_str("(bad)");
        } else {
            formatter.format(&insn, &mut text);
            if let Some(target) = annotation_target(&insn)
                && let Some(sym) = symbols.symbol_at(target)
            {
                text.push_str(&format!(" <{}>", sym.label()));
            }
        }
        out.push(Instruction {
            addr: insn.ip(),
            bytes: bytes[pos..(pos + len).min(bytes.len())].to_vec(),
            text,
            location: symbols.location(insn.ip()),
        });
        if insn.is_invalid() {
            decoder.set_position(pos + 1).expect("in bounds");
            decoder.set_ip(insn.ip() + 1);
        }
    }
    out
}

/// The address an instruction refers to that is worth naming: a direct branch or call target, or
/// a RIP-relative memory operand.
fn annotation_target(insn: &iced_x86::Instruction) -> Option<u64> {
    use iced_x86::FlowControl;
    match insn.flow_control() {
        FlowControl::Call | FlowControl::UnconditionalBranch | FlowControl::ConditionalBranch
            if insn.op0_kind() == iced_x86::OpKind::NearBranch64 =>
        {
            Some(insn.near_branch_target())
        }
        _ if insn.is_ip_rel_memory_operand() => Some(insn.ip_rel_memory_address()),
        _ => None,
    }
}

/// Read as many of `len` bytes from `addr` as are readable, in page-sized pieces.
fn read_prefix(memory: &mut dyn Memory, addr: u64, len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len);
    while out.len() < len {
        let at = addr + out.len() as u64;
        let to_page_end = (0x1000 - (at & 0xfff)) as usize;
        let n = to_page_end.min(len - out.len());
        let mut buf = vec![0u8; n];
        if memory.read(at, &mut buf).is_ok() {
            out.extend_from_slice(&buf);
            continue;
        }
        // Find the readable prefix of this piece by halving.
        let (mut good, mut bad) = (0usize, n);
        while bad - good > 1 {
            let mid = (good + bad) / 2;
            if memory.read(at, &mut buf[..mid]).is_ok() {
                good = mid;
            } else {
                bad = mid;
            }
        }
        if good > 0 {
            memory.read(at, &mut buf[..good])?;
            out.extend_from_slice(&buf[..good]);
        }
        if out.is_empty() {
            return Err(hopper_core::HopperError::UnreadableMemory { addr, len: 1 });
        }
        break;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::target::ImageMemory;

    #[test]
    fn decodes_and_stops_at_unreadable_memory() {
        // push rbp; mov rbp, rsp; call +0; ret
        let code = vec![0x55, 0x48, 0x89, 0xe5, 0xe8, 0x00, 0x00, 0x00, 0x00, 0xc3];
        let mut mem = ImageMemory::default().with(0x1000, code);
        let insns = disassemble(&SymbolSet::new(), &mut mem, 0x1000, 10).unwrap();
        let texts: Vec<&str> = insns.iter().map(|i| i.text.as_str()).collect();
        assert_eq!(
            texts,
            vec!["push rbp", "mov rbp, rsp", "call 0x1009", "ret"]
        );
        assert_eq!(insns[2].bytes.len(), 5);
        assert!(disassemble(&SymbolSet::new(), &mut mem, 0x9000, 1).is_err());
    }
}
