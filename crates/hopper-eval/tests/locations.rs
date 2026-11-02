//! DWARF location expressions, evaluated against a hand-made frame. Uses a fixture binary only
//! for a real unit's encoding; no gdb needed.

use std::sync::Arc;

use hopper_core::target::ImageMemory;
use hopper_core::{HopperError, Reg, RegisterSet};
use hopper_eval::ValueLocation;
use hopper_eval::location::{FrameContext, evaluate_expr};
use hopper_symbols::fixtures::fixture_binary;
use hopper_symbols::{Binary, Reader};

fn expr(bytes: &[u8]) -> gimli::Expression<Reader> {
    gimli::Expression(Reader::new(
        Arc::from(bytes.to_vec()),
        gimli::RunTimeEndian::Little,
    ))
}

#[test]
fn location_expressions() {
    let Some(path) = fixture_binary("values") else {
        return;
    };
    let binary = Binary::open(path).unwrap();
    let dwarf = binary.dwarf().unwrap();
    let unit = &dwarf.units()[0];

    let mut regs = RegisterSet::default();
    regs.set(Reg::RSP, 0x2000);
    regs.set(Reg::RAX, 0x1111_2222_3333_4444);
    regs.set(Reg::RDX, 0x5555_6666_7777_8888);
    regs.set(Reg::RBX, 0x9999);
    let frame = FrameContext {
        regs: &regs,
        cfa: Some(0x2040),
        frame_base: Some(0x1000),
        bias: 0x5000,
        file_pc: 0,
    };
    let mut stack = vec![0u8; 0x100];
    stack[..8].copy_from_slice(&0x3000u64.to_le_bytes());
    let mut mem = ImageMemory::default().with(0x2000, stack);
    let mut eval = |bytes: &[u8]| evaluate_expr(dwarf, unit, expr(bytes), &frame, &mut mem);

    // DW_OP_fbreg -8
    assert_eq!(
        eval(&[0x91, 0x78]).unwrap(),
        ValueLocation::Memory { addr: 0xff8 }
    );
    // DW_OP_breg7 (rsp) +16
    assert_eq!(
        eval(&[0x77, 0x10]).unwrap(),
        ValueLocation::Memory { addr: 0x2010 }
    );
    // DW_OP_reg3 (rbx)
    assert_eq!(
        eval(&[0x53]).unwrap(),
        ValueLocation::Register { reg: Reg::RBX }
    );
    // DW_OP_addr 0x1000: relocated by the bias.
    let mut addr = vec![0x03];
    addr.extend_from_slice(&0x1000u64.to_le_bytes());
    assert_eq!(eval(&addr).unwrap(), ValueLocation::Memory { addr: 0x6000 });
    // DW_OP_lit5 DW_OP_stack_value
    assert_eq!(
        eval(&[0x35, 0x9f]).unwrap(),
        ValueLocation::Bytes(Arc::from(5u64.to_le_bytes().to_vec()))
    );
    // DW_OP_call_frame_cfa
    assert_eq!(
        eval(&[0x9c]).unwrap(),
        ValueLocation::Memory { addr: 0x2040 }
    );
    // DW_OP_breg7 0; DW_OP_deref: the address stored at rsp.
    assert_eq!(
        eval(&[0x77, 0x00, 0x06]).unwrap(),
        ValueLocation::Memory { addr: 0x3000 }
    );
    // DW_OP_reg0 DW_OP_piece 4 DW_OP_reg1 DW_OP_piece 4: low halves of rax and rdx.
    let mut want = 0x3333_4444u32.to_le_bytes().to_vec();
    want.extend_from_slice(&0x7777_8888u32.to_le_bytes());
    assert_eq!(
        eval(&[0x50, 0x93, 0x04, 0x51, 0x93, 0x04]).unwrap(),
        ValueLocation::Bytes(Arc::from(want))
    );
    // DW_OP_piece 8 (empty) DW_OP_reg14 DW_OP_piece 8: half optimised out.
    assert_eq!(
        eval(&[0x93, 0x08, 0x5e, 0x93, 0x08]).unwrap(),
        ValueLocation::OptimisedOut
    );
    // DW_OP_entry_value(DW_OP_reg5) DW_OP_stack_value: the entry value is gone.
    assert_eq!(
        eval(&[0xa3, 0x01, 0x55, 0x9f]).unwrap(),
        ValueLocation::OptimisedOut
    );
    // A register the frame does not have.
    assert_eq!(eval(&[0x7c, 0x00]).unwrap(), ValueLocation::OptimisedOut);
    // DW_OP_breg7 0x80; DW_OP_deref past the readable stack.
    assert!(matches!(
        eval(&[0x77, 0x80, 0x02, 0x06]),
        Err(HopperError::UnreadableMemory { .. })
    ));
    // An empty expression describes no location.
    assert_eq!(eval(&[]).unwrap(), ValueLocation::OptimisedOut);

    // Without a frame base, DW_OP_fbreg is an error rather than a guess.
    let no_fb = FrameContext {
        frame_base: None,
        ..frame.clone()
    };
    assert!(
        evaluate_expr(
            dwarf,
            unit,
            expr(&[0x91, 0x78]),
            &no_fb,
            &mut ImageMemory::default()
        )
        .is_err()
    );
}
