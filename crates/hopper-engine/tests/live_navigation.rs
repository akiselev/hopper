//! The engine on a live gdbserver: breakpoints, forward steps, frames, values and disassembly on
//! the worked example.

mod support;

use hopper_core::api::{BreakLocation, Debugger, PageReq, StepKind, StopCause, Until};
use hopper_core::target::Direction;
use hopper_core::{HopperError, SourceLoc};
use support::*;

#[test]
fn breakpoint_at_main_rs_14() {
    let Some(d) = live("worked-example") else {
        return;
    };
    let bp = d
        .add_breakpoint(BreakLocation::Source(SourceLoc::new("main.rs", 14)))
        .unwrap();
    assert_eq!(bp.addresses.len(), 1);
    let stop = d.run(Direction::Forward, Until::Breakpoints).unwrap();
    assert_eq!(stop.cause, StopCause::Breakpoint { id: bp.id });
    assert_eq!(line_of(&stop), Some(14));
    assert_eq!(stop.function.as_deref(), Some("worked_example::main"));
    assert_eq!(stop.pc, bp.addresses[0]);
    assert_eq!(d.cursor().unwrap(), stop);
    assert_eq!(d.breakpoints().unwrap().len(), 1);
    d.remove_breakpoint(bp.id).unwrap();
    assert!(d.breakpoints().unwrap().is_empty());
    // Without breakpoints the program runs to its exit.
    let end = d.run(Direction::Forward, Until::Breakpoints).unwrap();
    assert_eq!(end.cause, StopCause::Exited { code: 0 });
}

#[test]
fn steps_into_over_and_out_of_the_worked_example() {
    let Some(d) = live("worked-example") else {
        return;
    };
    let s = run_to_line(&d, "main.rs", 12);
    assert_eq!(line_of(&s), Some(12));
    // Step into load(): its first line, past the prologue.
    let s = d.step(StepKind::Into, Direction::Forward).unwrap();
    assert_eq!(s.cause, StopCause::Step);
    assert_eq!(s.function.as_deref(), Some("worked_example::load"));
    assert_eq!(line_of(&s), Some(7));
    // Over String::from.
    let s = d.step(StepKind::Over, Direction::Forward).unwrap();
    assert_eq!(line_of(&s), Some(8));
    assert_eq!(s.function.as_deref(), Some("worked_example::load"));
    // Out of load(), back in main.
    let s = d.step(StepKind::Out, Direction::Forward).unwrap();
    assert_eq!(s.function.as_deref(), Some("worked_example::main"));
    let after_out = line_of(&s).unwrap();
    assert!(after_out == 12 || after_out == 13, "line {after_out}");
    // Over the remaining lines of main, one at a time, as gdb's `next` does.
    let mut lines = Vec::new();
    for _ in 0..5 {
        let s = d.step(StepKind::Over, Direction::Forward).unwrap();
        assert_eq!(s.function.as_deref(), Some("worked_example::main"));
        lines.push(line_of(&s).unwrap());
    }
    let expected_from = if after_out == 12 { 13 } else { 14 };
    let expected: Vec<u32> = (expected_from..expected_from + 5).collect();
    assert_eq!(lines, expected);
    // An instruction step stays on the line and advances the pc.
    let before = d.cursor().unwrap();
    let s = d.step(StepKind::Instruction, Direction::Forward).unwrap();
    assert_ne!(s.pc, before.pc);
    assert!(before.at.is_before(&s.at));
}

#[test]
fn reverse_and_seeking_back_are_refused() {
    let Some(d) = live("worked-example") else {
        return;
    };
    let start = d.cursor().unwrap();
    let s = run_to_line(&d, "main.rs", 13);
    assert!(matches!(
        d.step(StepKind::Over, Direction::Reverse),
        Err(HopperError::Unsupported(_))
    ));
    assert!(matches!(
        d.run(Direction::Reverse, Until::Breakpoints),
        Err(HopperError::Unsupported(_))
    ));
    assert!(matches!(d.seek(start.at), Err(HopperError::Unsupported(_))));
    assert!(matches!(d.back(), Err(HopperError::Unsupported(_))));
    // Seeking to where the cursor is, is fine.
    assert_eq!(d.seek(s.at).unwrap().at, s.at);
}

#[test]
fn frames_values_children_and_disassembly() {
    let Some(d) = live("worked-example") else {
        return;
    };
    let s = run_to_line(&d, "main.rs", 17);
    let frames = d.frames(s.at, s.tid).unwrap();
    assert_eq!(frames[0].function.as_deref(), Some("worked_example::main"));
    assert_eq!(frames[0].location.as_ref().map(|l| l.line), Some(17));
    assert!(frames[0].cfa.is_some());
    assert!(
        frames.iter().any(|f| f
            .function
            .as_deref()
            .is_some_and(|n| n.contains("lang_start"))),
        "{frames:?}"
    );

    let locals = d.locals(s.at, top(&s)).unwrap();
    for n in ["a", "b", "c", "v"] {
        by_name(&locals, n);
    }
    assert_eq!(
        by_name(&locals, "b").summary,
        "worked_example::Config {name: \"alpha\", retries: 3}"
    );
    assert!(by_name(&locals, "v").summary.starts_with("Vec(size=1)"));

    let e = d.eval(s.at, top(&s), "v[0].name").unwrap();
    assert_eq!(e.summary, "\"alpha\"");
    assert_eq!(d.eval(s.at, top(&s), "b.retries").unwrap().summary, "3");
    let v = d.eval(s.at, top(&s), "v").unwrap();
    assert_eq!(v.child_count, Some(1));
    let kids = d.children(&v.handle, PageReq::first(10)).unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].name, "[0]");
    let fields = d.children(&kids[0].handle, PageReq::first(10)).unwrap();
    assert_eq!(names(&fields), ["name", "retries"]);
    // A handle re-asked gives the same view.
    assert_eq!(d.view(&fields[0].handle).unwrap().summary, "\"alpha\"");

    let regs = d.registers(s.at, s.tid).unwrap();
    assert_eq!(regs.pc(), Some(s.pc));
    let insns = d
        .disassemble(s.at, d.recording().unwrap().processes[0].id, s.pc, 4)
        .unwrap();
    assert_eq!(insns.len(), 4);
    assert_eq!(insns[0].addr, s.pc);
    assert_eq!(insns[0].location.as_ref().map(|l| l.line), Some(17));
    let mem = d
        .memory(s.at, d.recording().unwrap().processes[0].id, s.pc, 4)
        .unwrap();
    let code: Vec<u8> = insns
        .iter()
        .flat_map(|i| i.bytes.iter().copied())
        .take(4)
        .collect();
    assert_eq!(mem, code);
}

#[test]
fn past_moments_answer_from_what_was_read() {
    let Some(d) = live("worked-example") else {
        return;
    };
    let s13 = run_to_line(&d, "main.rs", 13);
    let locals13 = d.locals(s13.at, top(&s13)).unwrap();
    let _ = run_to_line(&d, "main.rs", 16);
    // The process has moved on; what was read at line 13 is still answerable.
    assert_eq!(d.locals(s13.at, top(&s13)).unwrap(), locals13);
    assert_eq!(
        d.frames(s13.at, s13.tid).unwrap()[0]
            .location
            .as_ref()
            .map(|l| l.line),
        Some(13)
    );
    // Memory never read there is refused honestly.
    assert!(matches!(
        d.memory(s13.at, d.recording().unwrap().processes[0].id, 0x10, 8),
        Err(HopperError::Unsupported(_))
    ));
}
