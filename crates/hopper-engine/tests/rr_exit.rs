//! The plan's exit tests M1, M3, M4 and M5, on rr recordings. They need rr and a hardware PMU
//! (`rr record` fails in cloud containers), so they are ignored by default:
//! `cargo test -p hopper-engine --test rr_exit -- --ignored rr_`.
//!
//! Everything they exercise except rr itself also runs here: the live tests (M1 in live mode,
//! ownership) and the step-recording tests (reverse execution, `last_write`, `origin`, the
//! index) drive the same engine code.

mod support;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hopper_core::api::{
    Budget, CallFilter, Completeness, Confidence, Debugger, MachineKind, OriginEnd, OwnershipKind,
    OwnershipState, PageReq, StepKind,
};
use hopper_core::target::Direction;
use hopper_core::{MomentRange, SourceLoc};
use hopper_engine::{Session, SessionOptions, source_hops};
use hopper_rr::{RecordOptions, Rr};
use support::gdb::{GdbRun, M1_STOPS, compare_stop, gdb_stop};
use support::*;

fn rr() -> Rr {
    Rr::detect().expect("rr is installed (these tests need rr and a PMU)")
}

/// Record `binary` into a fresh directory; the guard keeps it alive.
fn record(binary: &Path) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("trace");
    let rec = rr()
        .record(
            binary,
            &[],
            &RecordOptions {
                output_dir: Some(trace.clone()),
                ..RecordOptions::default()
            },
        )
        .expect("rr record");
    assert!(
        rec.status.success(),
        "{}",
        String::from_utf8_lossy(&rec.stderr)
    );
    (dir, trace)
}

fn open(trace: &Path) -> Session {
    Session::open_rr(trace, SessionOptions::default()).expect("open the trace")
}

fn wait_for_index(d: &Session) {
    let deadline = Instant::now() + Duration::from_secs(600);
    loop {
        let st = d.index_status();
        assert!(st.error.is_none(), "phase A failed: {st:?}");
        if st.done {
            return;
        }
        assert!(Instant::now() < deadline, "phase A did not finish: {st:?}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// M1: "On a fixture Rust program recorded on a PMU host, `hopper show --at <moment>` matches
/// `rr replay -d rust-gdb` for source line and every local at 20 scripted stops; `step back`
/// then `step` returns to a Moment with byte-identical registers."
#[test]
#[ignore = "needs rr and a PMU"]
fn rr_m1_locals_match_rust_gdb_and_step_back_then_step_is_byte_identical() {
    let mut failures = Vec::new();
    let mut traces = std::collections::HashMap::new();
    for (fixture, line) in M1_STOPS {
        let binary = fixture_binary(fixture).expect("fixture");
        let (_, trace) = traces.entry(fixture).or_insert_with(|| record(&binary));
        let gdb = gdb_stop(GdbRun::Rr(trace), line);
        let d = open(trace);
        let stop = run_to_line(&d, "main.rs", line);
        failures.extend(
            compare_stop(&d, &stop, &gdb)
                .into_iter()
                .map(|f| format!("{fixture} main.rs:{line}: {f}")),
        );
        // The stop is a moment: asked again from elsewhere, it is the same state.
        let regs = d.registers(stop.at, stop.tid).unwrap();
        d.step(StepKind::Instruction, Direction::Reverse).unwrap();
        let back = d.step(StepKind::Instruction, Direction::Forward).unwrap();
        assert_eq!(back.at, stop.at, "{fixture} main.rs:{line}");
        assert_eq!(d.registers(back.at, back.tid).unwrap().raw, regs.raw);
        // Line steps too: over the line and back to its start.
        let next = d.step(StepKind::Over, Direction::Forward).unwrap();
        if next.function == stop.function {
            let again = d.step(StepKind::Over, Direction::Reverse).unwrap();
            assert_eq!(again.at, stop.at, "{fixture} main.rs:{line}: reverse-next");
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// gdb's hit count for a breakpoint on `function` over the whole replay.
fn gdb_hits(trace: &Path, function: &str) -> u64 {
    let out = Command::new("rr")
        .args(["replay", "-d", "gdb"])
        .arg(trace)
        .args(["--", "-batch", "-nx"])
        .args(["-ex", &format!("break {function}")])
        .args(["-ex", "ignore 1 100000000", "-ex", "continue"])
        .args(["-ex", "info breakpoints"])
        .output()
        .expect("rr replay -d gdb");
    let text = String::from_utf8_lossy(&out.stdout);
    text.lines()
        .find_map(|l| {
            l.trim()
                .strip_prefix("breakpoint already hit ")
                .and_then(|r| r.split_whitespace().next())
                .and_then(|n| n.parse().ok())
        })
        .unwrap_or(0)
}

/// M3: "Call-tree entry counts equal function-breakpoint counts from gdb for every function in
/// the fixture; 'every execution of line N' returns all hits with no cursor movement."
#[test]
#[ignore = "needs rr and a PMU"]
fn rr_m3_call_counts_match_gdb_and_line_hits_do_not_move_the_cursor() {
    let binary = fixture_binary("threads").expect("fixture");
    let (_dir, trace) = record(&binary);
    let d = open(&trace);
    wait_for_index(&d);
    for function in ["threads::main", "threads::fib", "threads::bump"] {
        let calls = d
            .calls(
                CallFilter {
                    function: Some(function.into()),
                    ..CallFilter::default()
                },
                PageReq::first(100_000),
            )
            .unwrap();
        assert_eq!(calls.completeness, Completeness::Complete);
        let ours = calls
            .items
            .iter()
            .filter(|c| c.function == function)
            .count() as u64;
        assert_eq!(ours, gdb_hits(&trace, function), "{function}");
    }
    let before = d.cursor().unwrap();
    let hits = d
        .line_hits(
            SourceLoc::new("main.rs", 14),
            MomentRange::all(),
            PageReq::first(100),
        )
        .unwrap();
    assert_eq!(hits.completeness, Completeness::Complete);
    assert_eq!(hits.items.len(), 6, "two threads bump three times each");
    assert_eq!(d.cursor().unwrap(), before);
}

/// The worked example, built with `cargo hopper-build` when it is available (labels), else the
/// plain fixture.
fn worked_example_binary() -> PathBuf {
    hopper_built_worked_example()
        .or_else(|| fixture_binary("worked-example"))
        .expect("the worked example")
}

/// M4: "The worked example's click yields the correct machine chain to the `.rodata` literal; an
/// agent reproduces it through MCP using only `moment` values."
#[test]
#[ignore = "needs rr and a PMU"]
fn rr_m4_origin_reaches_the_rodata_literal_and_mcp_reproduces_it() {
    let (_dir, trace) = record(&worked_example_binary());
    let d = Arc::new(open(&trace));
    let s = run_to_line(d.as_ref(), "main.rs", 18);
    // The click: the first byte of "alpha", which `n` points to.
    let byte = d.eval(s.at, top(&s), "(*n).vec[0]").unwrap();
    assert_eq!(byte.summary, "97");
    let chain = d.origin(&byte.handle, Budget::default()).unwrap();
    assert_eq!(chain.end, OriginEnd::Literal, "{chain:?}");
    assert!(
        chain.hops.len() >= 2,
        "the clone's copy and String::from's: {chain:?}"
    );
    assert!(chain.hops.iter().all(|h| h.machine == MachineKind::Copy));
    for w in chain.hops.windows(2) {
        assert_eq!(
            w[0].read,
            Some(w[1].written),
            "each hop reads what the next wrote"
        );
        assert!(w[1].at.is_before(&w[0].at));
    }

    // The same through MCP, from moments alone.
    let lines = [
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"t","version":"0"}}}"#.to_string(),
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#.to_string(),
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"run_until","arguments":{"until":"location","location":"main.rs:18"}}}"#.to_string(),
    ];
    let mut out = Vec::new();
    hopper_mcp::serve(d.clone(), std::io::Cursor::new(lines.join("\n")), &mut out).unwrap();
    let replies: Vec<serde_json::Value> = String::from_utf8(out)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    let stop = &replies.last().unwrap()["result"]["structuredContent"];
    let moment = stop["moment"].as_str().expect("a moment").to_string();
    let call = serde_json::json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"origin","arguments":{"moment": moment, "expr": "(*n).vec[0]"}}});
    let mut out = Vec::new();
    hopper_mcp::serve(d.clone(), std::io::Cursor::new(call.to_string()), &mut out).unwrap();
    let reply: serde_json::Value =
        serde_json::from_slice(out.split(|b| *b == b'\n').next().unwrap()).unwrap();
    let mcp = &reply["result"]["structuredContent"];
    assert_eq!(
        mcp["hops"].as_array().map(Vec::len),
        Some(chain.hops.len()),
        "{reply}"
    );
    assert!(mcp.to_string().contains("Literal"), "{reply}");
}

/// M5: "The worked example, built at -O0, yields exactly the six source hops in the table above,
/// with `a` reported MovedOut after line 11; a release build degrades to graded labels and never
/// shows a wrong move." The fixture's lines are the plan's plus two. The step recording of the
/// same program (tests/step_recording.rs) asserts the same six hops without rr.
#[test]
#[ignore = "needs rr and a PMU"]
fn rr_m5_six_source_hops_and_a_moved_out_after_its_move() {
    let binary = hopper_built_worked_example().expect("cargo hopper-build's worked example");
    let (_dir, trace) = record(&binary);
    let d = open(&trace);
    let s14 = run_to_line(&d, "main.rs", 14);
    let locals = d.locals(s14.at, top(&s14)).unwrap();
    assert!(
        matches!(&by_name(&locals, "a").ownership, OwnershipState::MovedOut { to: Some(b), .. } if b == "b"),
        "{:?}",
        by_name(&locals, "a").ownership
    );
    let s = run_to_line(&d, "main.rs", 18);
    let n = d.eval(s.at, top(&s), "*n").unwrap();
    let chain = d.origin(&n.handle, Budget::default()).unwrap();
    assert_eq!(chain.end, OriginEnd::Literal, "{chain:?}");
    assert_six_source_hops(&chain);

    // Release: graded labels, and every move the chain names is one the sidecar has.
    let release = binary
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("release/worked-example");
    let sidecar =
        hopper_sidecar::Sidecar::open_for_binary(&hopper_sidecar::sidecar_path(&release), &release)
            .expect("the release sidecar");
    let (_rdir, rtrace) = record(&release);
    let d = open(&rtrace);
    let s = run_to_line(&d, "main.rs", 18);
    let Ok(n) = d.eval(s.at, top(&s), "*n") else {
        return; // optimised out: nothing to label
    };
    let chain = d.origin(&n.handle, Budget::default()).unwrap();
    for h in source_hops(&chain) {
        let o = h.ownership.unwrap();
        if o.kind == OwnershipKind::Move {
            // The sidecar has the move, at its line: as written (`a` -> `b`), or as the call
            // argument a receiver took (`c` -> `push(arg 1)`, shown as `c` -> `v[0]`).
            let line = o.span.as_ref().map(|l| l.line);
            assert!(
                sidecar
                    .functions()
                    .iter()
                    .flat_map(|f| &f.events)
                    .any(|e| e.kind == OwnershipKind::Move
                        && e.src == o.src
                        && Some(e.span.lo.line) == line
                        && (e.dst == o.dst
                            || e.dst.as_deref().is_some_and(|d| d.ends_with(')')))),
                "a move the sidecar does not have: {o:?}"
            );
            assert_ne!(
                o.confidence,
                Confidence::Exact,
                "release moves are graded: {o:?}"
            );
        }
    }
}
