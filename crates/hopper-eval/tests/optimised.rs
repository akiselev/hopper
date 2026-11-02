//! An optimised build (`tests/programs/optimised`, opt-level 2): variables in callee-saved
//! registers recovered by CFI, location lists, and composite locations with an optimised-out
//! piece, compared with gdb on the same core.

use std::path::Path;

use hopper_core::{Moment, Place, ProcessId, RecordingId, Tid};
use hopper_eval::{EvalContext, Evaluator, FrameRef};
use hopper_symbols::corefile::CoreFile;
use hopper_symbols::fixtures::{GdbPrinting, build_program, core_of, gdb_print};

#[test]
fn optimised_frame_matches_gdb() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/optimised");
    let Some(binary) = build_program(&dir, "optimised", None) else { return };
    let Some(core_path) = core_of(&binary, "optimised::stop_here", "optimised") else { return };
    let core = CoreFile::open(&core_path).unwrap();
    let symbols = core.symbol_set();
    let thread = core.threads()[0].clone();
    let mut mem = core.memory();
    let stack = symbols.stack(&mut mem, &thread.regs, thread.tid).unwrap();
    let frame = stack
        .iter()
        .position(|f| f.frame.function.as_deref() == Some("optimised::mix"))
        .expect("mix is on the stack");
    let ev = Evaluator::new();
    let mut cx = EvalContext {
        symbols: &symbols,
        memory: &mut mem,
        frame: FrameRef::from_stack(&stack[frame]),
        at: Moment { recording: RecordingId([0; 16]), frame_time: 0, tid: Tid(1), ticks: 0, sub: 0 },
        process: ProcessId::new(thread.tid.0),
    };
    let locals = ev.locals(&mut cx).unwrap();
    let names: Vec<&str> = locals.iter().map(|v| v.name.as_str()).collect();
    for want in ["seed", "rounds", "name", "acc"] {
        assert!(names.contains(&want), "{want} in {names:?}");
    }
    let gdb = gdb_print(&binary, &core_path, frame as u32, &names, GdbPrinting::PrettyPrinters);
    let mut in_registers = 0;
    for (v, theirs) in locals.iter().zip(&gdb) {
        // gdb's providers raise on a partly optimised-out `&str` instead of printing it.
        let theirs = match theirs.as_deref() {
            Some(t) if t.starts_with("Python Exception") => "<optimized out>",
            Some(t) => t,
            None => "<optimized out>",
        };
        let theirs = theirs.replace("<optimized out>", "<optimised out>");
        assert_eq!(v.summary, theirs, "{}", v.name);
        if matches!(v.place, Some(Place::Register { .. })) {
            in_registers += 1;
        }
    }
    assert!(in_registers >= 2, "seed and acc are in callee-saved registers: {locals:#?}");
    // The register place names the frame, so the engine reads the unwound register.
    let seed = locals.iter().find(|v| v.name == "seed").unwrap();
    let mix_id = stack[frame].frame.id;
    assert!(matches!(seed.place, Some(Place::Register { frame, .. }) if frame == mix_id), "{seed:?}");
    assert_eq!(ev.eval(&mut cx, "seed").unwrap().summary, seed.summary);
}
