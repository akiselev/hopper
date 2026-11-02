//! The std layout table and enum layouts on `tests/programs/std-types`, built with the pinned
//! toolchain and, when installed, rustc 1.95 and 1.96 (the layout table's version switch), and
//! compared with rust-gdb's output on the same core.
//!
//! Tests skip, with a message, when gdb or a toolchain is missing.

use std::path::Path;

use hopper_core::api::{PageReq, ValueView};
use hopper_core::{Moment, ProcessId, RecordingId, Tid};
use hopper_eval::{EvalContext, Evaluator, FrameRef};
use hopper_symbols::RustcVersion;
use hopper_symbols::corefile::CoreFile;
use hopper_symbols::fixtures::{GdbPrinting, build_program, core_of, gdb_print};

/// Locals whose rendering gdb's providers do not cover (no provider, or a provider that fails),
/// with what hopper shows.
const HOPPER_ONLY: &[(&str, &str)] = &[
    (
        "weak",
        "Weak(strong=1, weak=1) = {value = 10, strong = 1, weak = 1}",
    ),
    ("dangling", "Weak(dangling)"),
    (
        "aweak",
        "Weak(strong=1, weak=1) = {value = \"arc\", strong = 1, weak = 1}",
    ),
    (
        "borrowed",
        "Ref(borrow=1) = {*value = \"cell\", borrow = 1}",
    ),
    ("path", "\"/tmp/x.txt\""),
];

const LOCALS: &[&str] = &[
    "set",
    "bset",
    "big_tree",
    "big_map",
    "ring",
    "strong",
    "weak",
    "dangling",
    "shared",
    "aweak",
    "cell",
    "refcell",
    "borrowed",
    "nonzero",
    "os",
    "path",
    "colour",
    "unit",
    "pair",
    "opt_ref",
    "opt_box",
    "opt_nz",
    "tagged",
    "small",
    "zeros",
    "empty",
    "unit_value",
];

fn without_pointer_cast(text: &str) -> &str {
    if (text.starts_with("(*") || text.starts_with("(&"))
        && let Some(i) = text.find(") 0x")
    {
        return &text[i + 2..];
    }
    text
}

fn check_toolchain(toolchain: Option<&str>, expect_minor: Option<u32>) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/programs/std-types");
    let Some(binary) = build_program(&dir, "std-types", toolchain) else {
        return;
    };
    let tag = format!("std-types-{}", toolchain.unwrap_or("pinned"));
    let Some(core_path) = core_of(&binary, "std_types::stop_here", &tag) else {
        return;
    };
    let core = CoreFile::open(&core_path).unwrap();
    let symbols = core.symbol_set();
    let thread = core.threads()[0].clone();
    let mut mem = core.memory();
    let stack = symbols.stack(&mut mem, &thread.regs, thread.tid).unwrap();
    let frame = stack
        .iter()
        .position(|f| f.frame.function.as_deref() == Some("std_types::main"))
        .expect("main is on the stack");
    if let Some(minor) = expect_minor {
        let v = symbols
            .scope_at(stack[frame].lookup_pc, stack[frame].die)
            .unwrap()
            .rustc_version
            .expect("the program's unit names its rustc");
        assert_eq!(v, RustcVersion::new(1, minor, 0));
    }

    let ev = Evaluator::new();
    let mut cx = EvalContext {
        symbols: &symbols,
        memory: &mut mem,
        frame: FrameRef::from_stack(&stack[frame]),
        at: Moment {
            recording: RecordingId([0; 16]),
            frame_time: 0,
            tid: Tid(1),
            ticks: 0,
            sub: 0,
        },
        process: ProcessId::new(thread.tid.0),
    };
    let locals = ev.locals(&mut cx).unwrap();
    let names: Vec<&str> = locals.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, LOCALS, "{tag}");

    let gdb = gdb_print(
        &binary,
        &core_path,
        frame as u32,
        LOCALS,
        GdbPrinting::PrettyPrinters,
    );
    for (view, theirs) in locals.iter().zip(&gdb) {
        if let Some((_, want)) = HOPPER_ONLY.iter().find(|(n, _)| *n == view.name) {
            assert_eq!(view.summary, *want, "{tag}: {}", view.name);
            continue;
        }
        let theirs = theirs
            .as_deref()
            .unwrap_or_else(|| panic!("{tag}: gdb could not print {}", view.name));
        assert_eq!(
            view.summary,
            without_pointer_cast(theirs),
            "{tag}: {}",
            view.name
        );
    }

    // Statics, by name and by path.
    for (e, want) in [
        ("GREETING", "\"hi there\""),
        ("std_types::GREETING", "\"hi there\""),
        ("COUNTER", "41"),
    ] {
        assert_eq!(ev.eval(&mut cx, e).unwrap().summary, want, "{tag}: {e}");
    }
    let by = |name: &str| -> &ValueView { locals.iter().find(|v| v.name == name).unwrap() };
    // The B-tree has internal nodes at 40 entries: children come out in key order.
    let tree = ev
        .children(
            &mut cx,
            &by("big_tree").handle,
            PageReq {
                offset: 30,
                limit: 20,
            },
        )
        .unwrap();
    assert_eq!(tree.len(), 10);
    assert_eq!(tree[0].name, "[30]");
    assert_eq!(tree[9].summary, "1521");
    // The ring buffer wraps: logical order, not storage order.
    assert_eq!(ev.eval(&mut cx, "ring[0]").unwrap().summary, "3");
    assert_eq!(ev.eval(&mut cx, "ring[3]").unwrap().summary, "6");
    assert_eq!(ev.eval(&mut cx, "*opt_ref.0").unwrap().summary, "10");
    assert_eq!(ev.eval(&mut cx, "tagged.label").unwrap().summary, "\"lbl\"");
    assert!(
        ev.eval(&mut cx, "small.label").is_err(),
        "Small has no label"
    );
    assert_eq!(ev.eval(&mut cx, "pair.0").unwrap().summary, "-2");
    assert_eq!(ev.eval(&mut cx, "*borrowed").unwrap().summary, "\"cell\"");
    assert_eq!(ev.eval(&mut cx, "zeros[15]").unwrap().summary, "0");
    assert_eq!(by("empty").child_count, Some(0));
    assert_eq!(by("unit").child_count, Some(0));
    assert_eq!(by("colour").child_count, None);
    assert_eq!(by("dangling").child_count, Some(2));
}

#[test]
fn std_types_with_pinned_toolchain() {
    check_toolchain(None, Some(97));
}

#[test]
fn std_types_with_rustc_1_95() {
    check_toolchain(Some("1.95.0"), Some(95));
}

#[test]
fn std_types_with_rustc_1_96() {
    check_toolchain(Some("1.96.0"), Some(96));
}
