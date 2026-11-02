//! DWARF location descriptions: expressions (`DW_AT_location` exprlocs) and location lists,
//! evaluated with gimli's `Evaluation` against a frame's registers, its CFA and frame base, and
//! memory. Composite locations (`DW_OP_piece`) are assembled into bytes.
//!
//! The requirement loop follows BugStalker's `dwarf/eval.rs` (MIT, see `THIRD_PARTY_NOTICES.md`),
//! with every memory read going through hopper's `Memory` trait instead of ptrace.

use std::sync::Arc;

use gimli::{AttributeValue, Evaluation, EvaluationResult, Location, Value as GValue};
use hopper_core::target::Memory;
use hopper_core::{HopperError, Reg, RegisterSet, Result};
use hopper_symbols::{DieRef, DwarfIndex, Reader, UnitInfo};

/// Where a value's bytes are.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ValueLocation {
    /// In memory at this address.
    Memory { addr: u64 },
    /// Entirely in one register.
    Register { reg: Reg },
    /// Known bytes with no single home: an implicit value, a computed result, or pieces
    /// assembled from several places.
    Bytes(Arc<[u8]>),
    /// The compiler dropped the value at this PC.
    OptimisedOut,
}

/// The frame a location is evaluated in.
#[derive(Clone, Debug)]
pub struct FrameContext<'a> {
    pub regs: &'a RegisterSet,
    pub cfa: Option<u64>,
    /// The value of the subprogram's `DW_AT_frame_base`, when it has one.
    pub frame_base: Option<u64>,
    /// The module's load bias (added to `DW_OP_addr`).
    pub bias: u64,
    /// The PC as a file address in the module, for location lists.
    pub file_pc: u64,
}

/// Evaluate a location attribute (an exprloc or a location-list reference).
pub fn evaluate_attr(
    dwarf: &DwarfIndex,
    unit: &UnitInfo,
    attr: AttributeValue<Reader>,
    frame: &FrameContext<'_>,
    memory: &mut dyn Memory,
) -> Result<ValueLocation> {
    if let AttributeValue::Exprloc(expr) = attr {
        return evaluate_expr(dwarf, unit, expr, frame, memory);
    }
    let Some(mut list) = dwarf
        .dwarf()
        .attr_locations(&unit.unit, attr)
        .map_err(eval_err)?
    else {
        return Err(HopperError::Eval(
            "location attribute is neither an expression nor a list".into(),
        ));
    };
    while let Some(entry) = list.next().map_err(eval_err)? {
        if entry.range.begin <= frame.file_pc && frame.file_pc < entry.range.end {
            return evaluate_expr(dwarf, unit, entry.data, frame, memory);
        }
    }
    Ok(ValueLocation::OptimisedOut)
}

/// The location of the variable (or parameter) `die`: its `DW_AT_location`, or the bytes of
/// `DW_AT_const_value`. Missing both means optimised out.
pub fn variable_location(
    dwarf: &DwarfIndex,
    die: DieRef,
    frame: &FrameContext<'_>,
    memory: &mut dyn Memory,
) -> Result<ValueLocation> {
    let (unit, entry) = dwarf.entry(die)?;
    if let Some(attr) = entry.attr_value(gimli::DW_AT_location) {
        return evaluate_attr(dwarf, unit, attr, frame, memory);
    }
    if let Some(v) = entry.attr_value(gimli::DW_AT_const_value) {
        return Ok(match v {
            AttributeValue::Block(b) => {
                use gimli::Reader as _;
                ValueLocation::Bytes(Arc::from(b.to_slice().map_err(eval_err)?.into_owned()))
            }
            AttributeValue::Sdata(s) => ValueLocation::Bytes(Arc::from(s.to_le_bytes().to_vec())),
            other => match other.udata_value() {
                Some(u) => ValueLocation::Bytes(Arc::from(u.to_le_bytes().to_vec())),
                None => ValueLocation::OptimisedOut,
            },
        });
    }
    Ok(ValueLocation::OptimisedOut)
}

/// The frame base of `subprogram` (`DW_AT_frame_base`): a register location yields the register's
/// value, a memory location its address.
pub fn frame_base(
    dwarf: &DwarfIndex,
    subprogram: DieRef,
    frame: &FrameContext<'_>,
    memory: &mut dyn Memory,
) -> Result<Option<u64>> {
    let (unit, entry) = dwarf.entry(subprogram)?;
    let Some(attr) = entry.attr_value(gimli::DW_AT_frame_base) else {
        return Ok(None);
    };
    Ok(match evaluate_attr(dwarf, unit, attr, frame, memory)? {
        ValueLocation::Memory { addr } => Some(addr),
        ValueLocation::Register { reg } => frame.regs.get(reg),
        ValueLocation::Bytes(b) => Some(le_u64(&b)),
        ValueLocation::OptimisedOut => None,
    })
}

fn le_u64(b: &[u8]) -> u64 {
    let mut buf = [0u8; 8];
    let n = b.len().min(8);
    buf[..n].copy_from_slice(&b[..n]);
    u64::from_le_bytes(buf)
}

/// Evaluate one DWARF expression.
pub fn evaluate_expr(
    dwarf: &DwarfIndex,
    unit: &UnitInfo,
    expr: gimli::Expression<Reader>,
    frame: &FrameContext<'_>,
    memory: &mut dyn Memory,
) -> Result<ValueLocation> {
    // An empty expression is a location description with no location (DWARF 5 §2.6.1.1.1).
    if gimli::Reader::is_empty(&expr.0) {
        return Ok(ValueLocation::OptimisedOut);
    }
    let mut eval: Evaluation<Reader> = expr.evaluation(unit.unit.encoding());
    let mut res = eval.evaluate().map_err(eval_err)?;
    loop {
        res = match res {
            EvaluationResult::Complete => break,
            EvaluationResult::RequiresRegister { register, .. } => {
                let Some(v) = frame.regs.get(Reg(register.0)) else {
                    // A register the unwinder could not recover in this frame.
                    return Ok(ValueLocation::OptimisedOut);
                };
                eval.resume_with_register(GValue::Generic(v))
                    .map_err(eval_err)?
            }
            EvaluationResult::RequiresMemory { address, size, .. } => {
                let mut buf = [0u8; 8];
                let n = (size as usize).min(8);
                memory.read(address, &mut buf[..n])?;
                eval.resume_with_memory(GValue::Generic(u64::from_le_bytes(buf)))
                    .map_err(eval_err)?
            }
            EvaluationResult::RequiresFrameBase => {
                let fb = frame
                    .frame_base
                    .ok_or_else(|| HopperError::Eval("the frame has no frame base".into()))?;
                eval.resume_with_frame_base(fb).map_err(eval_err)?
            }
            EvaluationResult::RequiresCallFrameCfa => {
                let cfa = frame
                    .cfa
                    .ok_or_else(|| HopperError::Eval("the frame has no CFA".into()))?;
                eval.resume_with_call_frame_cfa(cfa).map_err(eval_err)?
            }
            EvaluationResult::RequiresRelocatedAddress(a) => eval
                .resume_with_relocated_address(a.wrapping_add(frame.bias))
                .map_err(eval_err)?,
            EvaluationResult::RequiresIndexedAddress { index, relocate } => {
                let a = dwarf.dwarf().address(&unit.unit, index).map_err(eval_err)?;
                let a = if relocate {
                    a.wrapping_add(frame.bias)
                } else {
                    a
                };
                eval.resume_with_indexed_address(a).map_err(eval_err)?
            }
            EvaluationResult::RequiresBaseType(off) => {
                let entry = unit.unit.entry(off).map_err(eval_err)?;
                let enc = match entry.attr_value(gimli::DW_AT_encoding) {
                    Some(AttributeValue::Encoding(e)) => e,
                    _ => gimli::DW_ATE_unsigned,
                };
                let size = entry
                    .attr_value(gimli::DW_AT_byte_size)
                    .and_then(|v| v.udata_value())
                    .unwrap_or(8);
                let vt =
                    gimli::ValueType::from_encoding(enc, size).unwrap_or(gimli::ValueType::Generic);
                eval.resume_with_base_type(vt).map_err(eval_err)?
            }
            EvaluationResult::RequiresEntryValue(_) | EvaluationResult::RequiresParameterRef(_) => {
                // The value at function entry is gone unless call-site info says otherwise.
                return Ok(ValueLocation::OptimisedOut);
            }
            EvaluationResult::RequiresTls(_) => {
                return Err(HopperError::Unsupported(
                    "thread-local storage locations".into(),
                ));
            }
            EvaluationResult::RequiresAtLocation(_) => {
                return Err(HopperError::Unsupported("DW_OP_call* in locations".into()));
            }
            EvaluationResult::RequiresWasmLocal { .. }
            | EvaluationResult::RequiresWasmGlobal { .. }
            | EvaluationResult::RequiresWasmStack { .. } => {
                return Err(HopperError::Unsupported("WebAssembly locations".into()));
            }
        };
    }
    let pieces = eval.result();
    if pieces.is_empty() {
        return Ok(ValueLocation::OptimisedOut);
    }
    if pieces.len() == 1 && pieces[0].size_in_bits.is_none() {
        return Ok(match &pieces[0].location {
            Location::Empty => ValueLocation::OptimisedOut,
            Location::Address { address } => ValueLocation::Memory { addr: *address },
            Location::Register { register } => ValueLocation::Register {
                reg: Reg(register.0),
            },
            Location::Value { value } => ValueLocation::Bytes(Arc::from(value_bytes(*value))),
            Location::Bytes { value } => {
                use gimli::Reader as _;
                ValueLocation::Bytes(Arc::from(value.to_slice().map_err(eval_err)?.into_owned()))
            }
            Location::ImplicitPointer { .. } => {
                return Err(HopperError::Unsupported("implicit pointers".into()));
            }
        });
    }
    // Composite: assemble the pieces' bytes in order.
    let mut out = Vec::new();
    for p in &pieces {
        let bits = p.size_in_bits.unwrap_or(64);
        if bits % 8 != 0 || p.bit_offset.is_some_and(|b| b % 8 != 0) {
            return Err(HopperError::Unsupported("bit-sized location pieces".into()));
        }
        let n = (bits / 8) as usize;
        let skip = p.bit_offset.unwrap_or(0) as usize / 8;
        match &p.location {
            Location::Empty => return Ok(ValueLocation::OptimisedOut),
            Location::Address { address } => {
                let mut buf = vec![0u8; n];
                memory.read(address + skip as u64, &mut buf)?;
                out.extend_from_slice(&buf);
            }
            Location::Register { register } => {
                let Some(bytes) = frame.regs.get_bytes(Reg(register.0)) else {
                    return Ok(ValueLocation::OptimisedOut);
                };
                let mut buf = bytes.get(skip..).unwrap_or(&[]).to_vec();
                buf.resize(n, 0);
                out.extend_from_slice(&buf);
            }
            Location::Value { value } => {
                let mut buf = value_bytes(*value);
                buf.drain(..skip.min(buf.len()));
                buf.resize(n, 0);
                out.extend_from_slice(&buf);
            }
            Location::Bytes { value } => {
                use gimli::Reader as _;
                let bytes = value.to_slice().map_err(eval_err)?;
                let mut buf = bytes.get(skip..).unwrap_or(&[]).to_vec();
                buf.resize(n, 0);
                out.extend_from_slice(&buf);
            }
            Location::ImplicitPointer { .. } => {
                return Err(HopperError::Unsupported("implicit pointers".into()));
            }
        }
    }
    Ok(ValueLocation::Bytes(Arc::from(out)))
}

fn value_bytes(v: GValue) -> Vec<u8> {
    match v {
        GValue::Generic(x) | GValue::U64(x) => x.to_le_bytes().to_vec(),
        GValue::I8(x) => x.to_le_bytes().to_vec(),
        GValue::U8(x) => x.to_le_bytes().to_vec(),
        GValue::I16(x) => x.to_le_bytes().to_vec(),
        GValue::U16(x) => x.to_le_bytes().to_vec(),
        GValue::I32(x) => x.to_le_bytes().to_vec(),
        GValue::U32(x) => x.to_le_bytes().to_vec(),
        GValue::I64(x) => x.to_le_bytes().to_vec(),
        GValue::F32(x) => x.to_le_bytes().to_vec(),
        GValue::F64(x) => x.to_le_bytes().to_vec(),
    }
}

fn eval_err(e: gimli::Error) -> HopperError {
    HopperError::Eval(e.to_string())
}
