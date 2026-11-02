//! hopper's locals and expressions against gdb's own output (with Rust's pretty printers, as
//! rust-gdb prints) on the same core files of the fixture programs.
//!
//! Every test skips, with a message, when gdb or the fixtures are unavailable.

use hopper_core::api::{PageReq, PathElem, ValueTarget, ValueView};
use hopper_core::{Moment, Place, ProcessId, RecordingId, Tid};
use hopper_eval::{EvalContext, Evaluator, FrameRef};
use hopper_symbols::corefile::CoreFile;
use hopper_symbols::fixtures::{GdbPrinting, core_at, gdb_print};
use hopper_symbols::{StackFrame, SymbolSet};

struct Stopped {
    binary: std::path::PathBuf,
    core_path: std::path::PathBuf,
    core: CoreFile,
    symbols: SymbolSet,
    stack: Vec<StackFrame>,
}

fn stopped(fixture: &str, location: &str) -> Option<Stopped> {
    let (binary, core_path) = core_at(fixture, location)?;
    let core = CoreFile::open(&core_path).expect("core parses");
    let symbols = core.symbol_set();
    let thread = core.threads()[0].clone();
    let mut mem = core.memory();
    let stack = symbols
        .stack(&mut mem, &thread.regs, thread.tid)
        .expect("unwinds");
    Some(Stopped {
        binary,
        core_path,
        core,
        symbols,
        stack,
    })
}

fn moment() -> Moment {
    Moment {
        recording: RecordingId([0; 16]),
        frame_time: 0,
        tid: Tid(1),
        ticks: 0,
        sub: 0,
    }
}

impl Stopped {
    fn frame_index(&self, function: &str) -> usize {
        self.stack
            .iter()
            .position(|f| f.frame.function.as_deref() == Some(function))
            .unwrap_or_else(|| panic!("no frame {function} in {:?}", self.names()))
    }

    fn names(&self) -> Vec<Option<String>> {
        self.stack
            .iter()
            .map(|f| f.frame.function.clone())
            .collect()
    }

    fn with<T>(&self, frame: usize, f: impl FnOnce(&Evaluator, &mut EvalContext<'_>) -> T) -> T {
        let mut mem = self.core.memory();
        let mut cx = EvalContext {
            symbols: &self.symbols,
            memory: &mut mem,
            frame: FrameRef::from_stack(&self.stack[frame]),
            at: moment(),
            process: ProcessId::new(self.core.threads()[0].tid.0),
        };
        let ev = Evaluator::new();
        f(&ev, &mut cx)
    }

    fn gdb(&self, frame: usize, exprs: &[&str], printing: GdbPrinting) -> Vec<Option<String>> {
        gdb_print(&self.binary, &self.core_path, frame as u32, exprs, printing)
    }
}

/// gdb prints `print &x` as `(*mut T) 0x7ffd...`; take the address.
fn gdb_address(text: &str) -> Option<u64> {
    let i = text.rfind("0x")?;
    let hex: String = text[i + 2..]
        .chars()
        .take_while(|c| c.is_ascii_hexdigit())
        .collect();
    u64::from_str_radix(&hex, 16).ok()
}

/// gdb prefixes a top-level pointer with its type (`(*mut values::Point) 0x...`); hopper's
/// summary is the bare address, with the type in `type_name`.
fn without_pointer_cast(text: &str) -> &str {
    if (text.starts_with("(*") || text.starts_with("(&"))
        && let Some(i) = text.find(") 0x")
    {
        return &text[i + 2..];
    }
    text
}

fn by_name<'a>(views: &'a [ValueView], name: &str) -> &'a ValueView {
    views
        .iter()
        .find(|v| v.name == name)
        .unwrap_or_else(|| panic!("no local {name}"))
}

/// Every local of `values::main`, compared with rust-gdb's print of the same variable, and its
/// address and size with gdb's `&x` and `sizeof(x)`.
#[test]
fn values_main_locals_match_gdb() {
    let Some(st) = stopped("values", "values::stop_here") else {
        return;
    };
    let frame = st.frame_index("values::main");
    let locals = st.with(frame, |ev, cx| ev.locals(cx)).expect("locals");
    let names: Vec<&str> = locals.iter().map(|v| v.name.as_str()).collect();
    let expected = [
        "small", "neg", "flag", "pi", "ch", "text", "owned", "nums", "pt", "tuple", "arr", "some",
        "none", "boxed", "ok", "err", "shapes", "map", "tree", "deque", "rc", "rc2", "arc", "cell",
        "slice",
    ];
    assert_eq!(
        names, expected,
        "locals in declaration order; `r` is not in scope yet"
    );

    let gdb = st.gdb(frame, &expected, GdbPrinting::PrettyPrinters);
    for (view, theirs) in locals.iter().zip(&gdb) {
        let theirs = theirs
            .as_ref()
            .unwrap_or_else(|| panic!("gdb could not print {}", view.name));
        assert_eq!(
            view.summary,
            without_pointer_cast(theirs),
            "local {}",
            view.name
        );
    }

    let addr_exprs: Vec<String> = expected.iter().map(|n| format!("&{n}")).collect();
    let size_exprs: Vec<String> = expected.iter().map(|n| format!("sizeof({n})")).collect();
    let refs: Vec<&str> = addr_exprs
        .iter()
        .chain(&size_exprs)
        .map(String::as_str)
        .collect();
    let gdb = st.gdb(frame, &refs, GdbPrinting::Raw);
    let pid = ProcessId::new(st.core.threads()[0].tid.0);
    for (i, view) in locals.iter().enumerate() {
        let addr = gdb[i]
            .as_deref()
            .and_then(gdb_address)
            .expect("gdb gives an address");
        let size: u64 = gdb[expected.len() + i]
            .as_deref()
            .unwrap()
            .parse()
            .expect("gdb gives a size");
        assert_eq!(
            view.place,
            Some(Place::Memory {
                process: pid,
                addr,
                len: size
            }),
            "place of {}",
            view.name
        );
    }
}

/// Type names are the qualified DWARF names gdb uses.
#[test]
fn values_type_names_match_gdb_whatis() {
    let Some(st) = stopped("values", "values::stop_here") else {
        return;
    };
    let frame = st.frame_index("values::main");
    let locals = st.with(frame, |ev, cx| ev.locals(cx)).expect("locals");
    let mut cmd = std::process::Command::new("gdb");
    cmd.args([
        "-batch",
        "-nx",
        "-iex",
        "set auto-load python-scripts off",
        "-ex",
        &format!("frame {frame}"),
    ]);
    for v in &locals {
        cmd.args(["-ex", &format!("whatis {}", v.name)]);
    }
    let out = cmd
        .arg(&st.binary)
        .arg(&st.core_path)
        .output()
        .expect("gdb runs");
    let text = String::from_utf8_lossy(&out.stdout);
    let gdb_types: Vec<&str> = text
        .lines()
        .filter_map(|l| l.strip_prefix("type = "))
        .collect();
    assert_eq!(gdb_types.len(), locals.len(), "{text}");
    for (v, t) in locals.iter().zip(gdb_types) {
        // gdb names `Box<T>` (a DWARF pointer type) by its pointee; hopper keeps the DWARF name.
        if v.name == "boxed" {
            assert_eq!(
                v.type_name,
                "alloc::boxed::Box<values::Point, alloc::alloc::Global>"
            );
            assert_eq!(t, "*mut values::Point");
            continue;
        }
        assert_eq!(v.type_name, t, "type of {}", v.name);
    }
}

/// Expressions over the values fixture, against gdb's print of the same expression.
#[test]
fn values_expressions_match_gdb() {
    let Some(st) = stopped("values", "values::stop_here") else {
        return;
    };
    let frame = st.frame_index("values::main");
    let exprs = [
        "pt.x",
        "pt.y",
        "tuple.0",
        "tuple.1",
        "tuple.2",
        "arr[2]",
        "nums[1]",
        "*boxed",
        "boxed.x",
        "(*boxed).y",
        "shapes[1]",
        "shapes[0]",
        "slice[0]",
        "deque[3]",
        "owned",
        "text",
        "small",
        "-7",
        "arr[pt.x]",
        "nums.len",
        "cell.value.value",
        "rc.ptr",
    ];
    let gdb = st.gdb(frame, &exprs, GdbPrinting::PrettyPrinters);
    st.with(frame, |ev, cx| {
        for (e, theirs) in exprs.iter().zip(&gdb) {
            let ours = ev.eval(cx, e).unwrap_or_else(|err| panic!("{e}: {err}"));
            let Some(theirs) = theirs else {
                // gdb's Rust expressions cannot index a Vec, for example.
                eprintln!("gdb could not evaluate {e}; hopper says {}", ours.summary);
                continue;
            };
            assert_eq!(ours.summary, without_pointer_cast(theirs), "expression {e}");
        }
        let pt_addr = match by_name(&ev.locals(cx).unwrap(), "pt").place {
            Some(Place::Memory { addr, .. }) => format!("{addr:#x}"),
            other => panic!("pt is in memory: {other:?}"),
        };
        // Values hopper reads through the std layout table, checked by content.
        let checks = [
            ("rc.value", "\"shared\""),
            ("arc.strong", "1"),
            ("rc2.strong", "2"),
            ("map[0]", "1"),
            ("tree[1]", "99 'c'"),
            ("*rc", "\"shared\""),
            ("some.0", "5"),
            ("err.0", "\"bad\""),
            ("shapes[0].r", "1.5"),
            ("&pt", pt_addr.as_str()),
            ("*&pt", "values::Point {x: 3, y: -4}"),
        ];
        for (e, want) in checks {
            let v = ev.eval(cx, e).unwrap_or_else(|err| panic!("{e}: {err}"));
            assert_eq!(v.summary, want, "expression {e}");
        }
        for bad in ["nosuchvar", "pt.z", "none.0", "arr[9]", "pt[0]", "1 +"] {
            assert!(ev.eval(cx, bad).is_err(), "{bad} should fail");
        }
    });
}

/// Children: paged, named, and addressed by paths the evaluator can follow again.
#[test]
fn values_children_and_paths() {
    let Some(st) = stopped("values", "values::stop_here") else {
        return;
    };
    let frame = st.frame_index("values::main");
    st.with(frame, |ev, cx| {
        let locals = ev.locals(cx).unwrap();
        let nums = by_name(&locals, "nums");
        assert_eq!(nums.child_count, Some(3));
        let kids = ev.children(cx, &nums.handle, PageReq::first(10)).unwrap();
        let summaries: Vec<&str> = kids.iter().map(|k| k.summary.as_str()).collect();
        assert_eq!(summaries, ["1", "2", "3"]);
        assert_eq!(kids[1].name, "[1]");
        assert_eq!(kids[1].handle.path, vec![PathElem::Index(1)]);
        // Paging.
        let page = ev
            .children(
                cx,
                &nums.handle,
                PageReq {
                    offset: 2,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].summary, "3");
        // A child's handle rebuilds the same view.
        assert_eq!(ev.view(cx, &kids[1].handle).unwrap(), kids[1]);

        let pt = by_name(&locals, "pt");
        let fields = ev.children(cx, &pt.handle, PageReq::first(10)).unwrap();
        assert_eq!(
            fields.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(),
            ["x", "y"]
        );
        assert_eq!(fields[1].type_name, "i32");

        let some = by_name(&locals, "some");
        let inner = ev.children(cx, &some.handle, PageReq::first(10)).unwrap();
        assert_eq!(inner.len(), 1);
        assert_eq!(
            inner[0].handle.path,
            vec![
                PathElem::Variant("Some".into()),
                PathElem::Field("0".into())
            ]
        );
        assert_eq!(inner[0].summary, "5");
        assert_eq!(by_name(&locals, "none").child_count, None);

        let boxed = by_name(&locals, "boxed");
        let deref = ev.children(cx, &boxed.handle, PageReq::first(1)).unwrap();
        assert_eq!(deref[0].name, "*boxed");
        assert_eq!(deref[0].summary, "values::Point {x: 1, y: 2}");

        let map = by_name(&locals, "map");
        let entries = ev.children(cx, &map.handle, PageReq::first(10)).unwrap();
        let mut pairs: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.name.clone(), e.summary.clone()))
            .collect();
        pairs.sort();
        assert_eq!(
            pairs,
            [
                ("[\"one\"]".into(), "1".into()),
                ("[\"two\"]".into(), "2".into())
            ]
        );

        let rc = by_name(&locals, "rc");
        let rc_kids = ev.children(cx, &rc.handle, PageReq::first(10)).unwrap();
        assert_eq!(
            rc_kids.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(),
            ["value", "strong", "weak"]
        );
        assert_eq!(rc_kids[1].summary, "2");

        let deque = by_name(&locals, "deque");
        let d = ev.children(cx, &deque.handle, PageReq::first(10)).unwrap();
        assert_eq!(
            d.iter().map(|k| k.summary.as_str()).collect::<Vec<_>>(),
            ["1", "2", "3", "4"]
        );
        let tree = by_name(&locals, "tree");
        let t = ev.children(cx, &tree.handle, PageReq::first(10)).unwrap();
        assert_eq!(
            t.iter()
                .map(|k| (k.name.as_str(), k.summary.as_str()))
                .collect::<Vec<_>>(),
            [("[1]", "97 'a'"), ("[3]", "99 'c'")]
        );

        // Every child of every local has a handle that views back to the same thing.
        for local in &locals {
            let kids = ev.children(cx, &local.handle, PageReq::first(50)).unwrap();
            assert_eq!(
                kids.len() as u64,
                local.child_count.unwrap_or(0),
                "{}",
                local.name
            );
            for k in kids {
                assert_eq!(
                    ev.view(cx, &k.handle).unwrap(),
                    k,
                    "{} / {}",
                    local.name,
                    k.name
                );
            }
        }

        // A place handle: the same bytes, without the expression.
        let ev2 = ev.evaluate(cx, &pt.handle).unwrap();
        let place_handle = hopper_core::api::ValueHandle {
            target: ValueTarget::Place {
                place: ev2.place.unwrap(),
                ty: ev2.type_key,
            },
            ..pt.handle.clone()
        };
        let by_place = ev.view(cx, &place_handle).unwrap();
        assert_eq!(by_place.summary, pt.summary);
        assert_eq!(by_place.type_name, pt.type_name);
    });
}

/// The parameter of `stop_here` in frame 0.
#[test]
fn stop_here_parameter() {
    let Some(st) = stopped("values", "values::stop_here") else {
        return;
    };
    let locals = st.with(0, |ev, cx| ev.locals(cx)).unwrap();
    assert_eq!(locals.len(), 1);
    assert_eq!(locals[0].name, "counter");
    assert_eq!(locals[0].summary, "7");
    assert_eq!(
        st.gdb(0, &["counter"], GdbPrinting::PrettyPrinters)[0].as_deref(),
        Some("7")
    );
}

fn worked_example_at(line: u32, expected: &[&str]) {
    let Some(st) = stopped("worked-example", &format!("main.rs:{line}")) else {
        return;
    };
    let frame = st.frame_index("worked_example::main");
    assert_eq!(frame, 0);
    let locals = st.with(frame, |ev, cx| ev.locals(cx)).expect("locals");
    let names: Vec<&str> = locals.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(names, expected, "locals at line {line}");
    let gdb = st.gdb(frame, expected, GdbPrinting::PrettyPrinters);
    for (view, theirs) in locals.iter().zip(&gdb) {
        let theirs = theirs
            .as_ref()
            .unwrap_or_else(|| panic!("gdb could not print {}", view.name));
        assert_eq!(&view.summary, theirs, "local {} at line {line}", view.name);
    }
    // gdb's own `info locals` lists the same names (innermost scope first).
    let out =
        hopper_symbols::fixtures::gdb_output(&st.binary, Some(&st.core_path), &["info locals"]);
    let mut gdb_names: Vec<&str> = out
        .lines()
        .filter_map(|l| l.split_once(" = ").map(|(n, _)| n))
        .filter(|n| n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
        .collect();
    gdb_names.reverse();
    assert_eq!(
        gdb_names, expected,
        "gdb info locals at line {line}:\n{out}"
    );
}

#[test]
fn worked_example_line_14() {
    worked_example_at(14, &["a", "b"]);
}

#[test]
fn worked_example_line_17() {
    worked_example_at(17, &["a", "b", "c", "v"]);
}

#[test]
fn worked_example_expressions_at_line_18() {
    let Some(st) = stopped("worked-example", "main.rs:18") else {
        return;
    };
    st.with(0, |ev, cx| {
        let n = ev.eval(cx, "n").unwrap();
        assert_eq!(n.type_name, "&alloc::string::String");
        let deref = ev.eval(cx, "*n").unwrap();
        assert_eq!(deref.summary, "\"alpha\"");
        assert_eq!(ev.eval(cx, "v[0].name").unwrap().summary, "\"alpha\"");
        assert_eq!(ev.eval(cx, "v[0].retries").unwrap().summary, "3");
        assert_eq!(ev.eval(cx, "b.retries").unwrap().summary, "3");
        // `n` points at the String inside the Vec's heap buffer.
        let elem_view = ev.eval(cx, "v[0].name").unwrap();
        let elem = ev.evaluate(cx, &elem_view.handle).unwrap();
        let n_val = ev.bytes(cx, &n.handle).unwrap();
        let n_addr = u64::from_le_bytes(n_val[..8].try_into().unwrap());
        assert_eq!(
            elem.place.map(|p| match p {
                Place::Memory { addr, .. } => addr,
                _ => 0,
            }),
            Some(n_addr)
        );
    });
    let gdb = st.gdb(0, &["*n", "b.retries"], GdbPrinting::PrettyPrinters);
    assert_eq!(gdb[0].as_deref(), Some("\"alpha\""));
    assert_eq!(gdb[1].as_deref(), Some("3"));
}
