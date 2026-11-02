//! Reverse execution, `last_write` and `origin` on a recording made by single-stepping a live
//! process (no rr here): the worked example from line 12 to line 18.

mod support;

/// An ownership label as (kind, source, destination, line).
type Label = (OwnershipKind, Option<String>, Option<String>, Option<u32>);

use std::sync::{Arc, OnceLock};

use hopper_core::Place;
use hopper_core::api::{
    Budget, Debugger, MachineKind, OriginEnd, OwnershipKind, PageReq, StepKind, StopCause, Until,
};
use hopper_core::target::Direction;
use hopper_engine::{Session, SessionOptions};
use support::step_recording::{StepFactory, StepRecording, WindowStart};
use support::*;

fn recording(hopper_built: bool) -> Option<Arc<StepRecording>> {
    static PLAIN: OnceLock<Option<Arc<StepRecording>>> = OnceLock::new();
    static BUILT: OnceLock<Option<Arc<StepRecording>>> = OnceLock::new();
    let cell = if hopper_built { &BUILT } else { &PLAIN };
    cell.get_or_init(|| {
        let bin = if hopper_built {
            hopper_built_worked_example()?
        } else {
            fixture_binary("worked-example")?
        };
        live_binary(&bin)?; // skips without gdbserver
        let rec = StepRecording::capture(&bin, WindowStart::Line(12), 18, 2_000_000)
            .expect("capture the window");
        eprintln!("recorded {} instructions", rec.len());
        Some(Arc::new(rec))
    })
    .clone()
}

fn session(rec: &Arc<StepRecording>) -> Session {
    Session::replay(
        StepFactory(rec.clone()),
        2,
        "single-stepped worked example",
        SessionOptions::default(),
    )
    .expect("open the replay")
}

#[test]
fn steps_back_and_forth_with_identical_registers() {
    let Some(rec) = recording(false) else { return };
    let d = session(&rec);
    let start = d.cursor().unwrap();
    assert_eq!(line_of(&start), Some(12));
    let s13 = d.step(StepKind::Over, Direction::Forward).unwrap();
    assert_eq!(line_of(&s13), Some(13));
    let s14 = d.step(StepKind::Over, Direction::Forward).unwrap();
    assert_eq!(line_of(&s14), Some(14));
    let regs14 = d.registers(s14.at, s14.tid).unwrap();
    // Back one line, to the start of line 13, then forward to exactly where we were.
    let back = d.step(StepKind::Over, Direction::Reverse).unwrap();
    assert_eq!(line_of(&back), Some(13));
    assert_eq!(
        back.at, s13.at,
        "reverse-next lands on the start of line 13"
    );
    let again = d.step(StepKind::Over, Direction::Forward).unwrap();
    assert_eq!(again.at, s14.at);
    assert_eq!(d.registers(again.at, again.tid).unwrap().raw, regs14.raw);
    // Instruction steps both ways.
    let i1 = d.step(StepKind::Instruction, Direction::Forward).unwrap();
    let i0 = d.step(StepKind::Instruction, Direction::Reverse).unwrap();
    assert_eq!(i0.at, s14.at);
    assert_ne!(i1.at, i0.at);
    // Into clone, then reverse out of it to the call.
    let into = d.step(StepKind::Into, Direction::Forward).unwrap();
    assert!(
        into.function
            .as_deref()
            .is_some_and(|f| f.contains("clone")),
        "{into:?}"
    );
    let out = d.step(StepKind::Out, Direction::Reverse).unwrap();
    assert_eq!(out.function.as_deref(), Some("worked_example::main"));
    assert_eq!(line_of(&out), Some(14));
    // Navigation history.
    let b = d.back().unwrap().unwrap();
    assert_eq!(b.at, into.at);
    let f = d.forward().unwrap().unwrap();
    assert_eq!(f.at, out.at);
    // The end and the start of the window.
    let end = d.run(Direction::Forward, Until::EndOfTrace).unwrap();
    assert_eq!(end.cause, StopCause::HistoryEnd);
    assert_eq!(line_of(&end), Some(18));
    let first = d.run(Direction::Reverse, Until::EndOfTrace).unwrap();
    assert_eq!(first.cause, StopCause::HistoryStart);
    assert_eq!(first.at, start.at);
}

#[test]
fn reverse_continue_to_a_breakpoint_and_seek() {
    let Some(rec) = recording(false) else { return };
    let d = session(&rec);
    let end = d.run(Direction::Forward, Until::EndOfTrace).unwrap();
    let bp = d
        .add_breakpoint(hopper_core::api::BreakLocation::Source(
            hopper_core::SourceLoc::new("main.rs", 15),
        ))
        .unwrap();
    let s = d.run(Direction::Reverse, Until::Breakpoints).unwrap();
    assert_eq!(s.cause, StopCause::Breakpoint { id: bp.id });
    assert_eq!(line_of(&s), Some(15));
    // Queries at other moments do not move the cursor.
    let frames = d.frames(end.at, end.tid).unwrap();
    assert_eq!(frames[0].location.as_ref().map(|l| l.line), Some(18));
    assert_eq!(d.cursor().unwrap().at, s.at);
    let seeked = d.seek(end.at).unwrap();
    assert_eq!(seeked.cause, StopCause::Jump);
    assert_eq!(seeked.at, end.at);
}

#[test]
fn last_write_of_b_is_the_move_on_line_13() {
    let Some(rec) = recording(false) else { return };
    let d = session(&rec);
    let end = d.run(Direction::Forward, Until::EndOfTrace).unwrap();
    let b = d.eval(end.at, top(&end), "b.retries").unwrap();
    let place = b.place.expect("b.retries is in memory");
    let w = d
        .last_write(place, end.at)
        .unwrap()
        .expect("written in the window");
    assert_eq!(w.function.as_deref(), Some("worked_example::main"));
    assert_eq!(w.location.as_ref().map(|l| l.line), Some(13), "{w:?}");
    assert_eq!(w.value, 3u32.to_le_bytes());
    assert!(w.at.is_before(&end.at));
    // b's history: one change, at the move.
    let hist = d
        .value_history(
            &b.handle,
            hopper_core::MomentRange::all(),
            PageReq::first(10),
        )
        .unwrap();
    assert!(hist.items.iter().any(|(_, v)| v.summary == "3"), "{hist:?}");
}

#[test]
fn origin_of_the_alpha_bytes_reaches_the_literal() {
    let Some(rec) = recording(true) else { return };
    let d = session(&rec);
    let end = d.run(Direction::Forward, Until::EndOfTrace).unwrap();
    // The bytes `n` points into: the String's heap buffer, as its `vec`'s elements.
    let n = d.eval(end.at, top(&end), "(*n).vec").unwrap();
    assert_eq!(n.summary, "Vec(size=5) = {97, 108, 112, 104, 97}");
    let kids = d.children(&n.handle, PageReq::first(8)).unwrap();
    let byte = kids
        .iter()
        .find(|k| matches!(k.place, Some(Place::Memory { .. })) && k.summary == "97")
        .expect("a character of the string with a place");
    let chain = d.origin(&byte.handle, Budget::default()).unwrap();
    // The plan's hops 3 and 6: the clone's copy of b's buffer into c's, then String::from's copy
    // of the literal; the moves of whole Configs move the pointer, not these bytes.
    assert_eq!(chain.end, OriginEnd::Literal, "{chain:?}");
    assert_eq!(chain.hops.len(), 2, "{chain:?}");
    assert!(chain.hops.iter().all(|h| h.machine == MachineKind::Copy));
    let clone = chain.hops[0]
        .ownership
        .as_ref()
        .expect("the clone hop is labelled");
    assert_eq!(clone.kind, OwnershipKind::Clone);
    assert_eq!(
        (clone.src.as_deref(), clone.dst.as_deref()),
        (Some("b"), Some("c"))
    );
    assert_eq!(clone.span.as_ref().map(|l| l.line), Some(14));
    let from = chain.hops[1]
        .ownership
        .as_ref()
        .expect("the String::from hop is labelled");
    assert_eq!(from.kind, OwnershipKind::CallReturn);
    assert_eq!(from.dst.as_deref(), Some("name"));
    assert_eq!(from.span.as_ref().map(|l| l.line), Some(7));
    // Each hop reads where the next one wrote; the last reads read-only data.
    assert_eq!(chain.hops[0].read, Some(chain.hops[1].written));
    assert!(chain.hops[1].at.is_before(&chain.hops[0].at));
    assert!(chain.hops[0].at.is_before(&end.at));
    // `last_write` agrees with the first hop.
    let w = d.last_write(byte.place.unwrap(), end.at).unwrap().unwrap();
    assert_eq!((w.at, w.pc), (chain.hops[0].at, chain.hops[0].pc));
    assert_eq!(w.value, b"a");
}

/// Ownership on a replay comes from replaying the frame from its entry on a background replica
/// (no cursor tracing): the window starts at `main`'s first instruction.
#[test]
fn ownership_by_replaying_the_frame() {
    let Some(bin) = hopper_built_worked_example() else {
        return;
    };
    if live_binary(&bin).is_none() {
        return;
    }
    let rec = Arc::new(
        StepRecording::capture(
            &bin,
            WindowStart::FunctionEntry("worked_example::main"),
            18,
            2_000_000,
        )
        .expect("capture"),
    );
    let d = session(&rec);
    assert!(matches!(
        d.sidecar_status(),
        hopper_engine::SidecarStatus::Loaded(_)
    ));
    let s14 = run_to_line(&d, "main.rs", 14);
    let locals = d.locals(s14.at, top(&s14)).unwrap();
    match &by_name(&locals, "a").ownership {
        hopper_core::api::OwnershipState::MovedOut { to, at, .. } => {
            assert_eq!(to.as_deref(), Some("b"));
            assert_eq!(at.as_ref().map(|l| l.line), Some(13));
        }
        other => panic!("a at line 14: {other:?}"),
    }
    assert_eq!(
        by_name(&locals, "b").ownership,
        hopper_core::api::OwnershipState::Live
    );
    // Asked again at an earlier moment (line 13), from the end: the move has not run yet.
    let end = d.run(Direction::Forward, Until::EndOfTrace).unwrap();
    let s13 = d
        .line_hits(
            hopper_core::SourceLoc::new("main.rs", 13),
            hopper_core::MomentRange::all(),
            PageReq::first(4),
        )
        .unwrap();
    assert_eq!(s13.items.len(), 1, "{s13:?}");
    let locals13 = d.locals(s13.items[0], top(&s14)).unwrap();
    assert_eq!(
        by_name(&locals13, "a").ownership,
        hopper_core::api::OwnershipState::Live
    );
    assert_eq!(
        d.cursor().unwrap().at,
        end.at,
        "queries never move the cursor"
    );
}

/// A window that starts inside the frame cannot say what ran before it: Unknown, not a guess.
#[test]
fn ownership_is_unknown_when_the_frame_began_before_the_recording() {
    let Some(rec) = recording(true) else { return };
    let d = session(&rec);
    let s14 = run_to_line(&d, "main.rs", 14);
    let locals = d.locals(s14.at, top(&s14)).unwrap();
    assert_eq!(
        by_name(&locals, "a").ownership,
        hopper_core::api::OwnershipState::Unknown
    );
}

/// The value `*n` (the String in the Vec), not its bytes: the machine chain follows its words
/// back through the Vec push, the clone, the move from `a` and `load`'s return to `String::from`,
/// and the source view names the ownership events in order.
#[test]
fn origin_of_the_string_names_the_ownership_events() {
    let Some(bin) = hopper_built_worked_example() else {
        return;
    };
    if live_binary(&bin).is_none() {
        return;
    }
    let rec = Arc::new(
        StepRecording::capture(
            &bin,
            WindowStart::FunctionEntry("worked_example::main"),
            18,
            2_000_000,
        )
        .expect("capture"),
    );
    let d = session(&rec);
    let end = d.run(Direction::Forward, Until::EndOfTrace).unwrap();
    let n = d.eval(end.at, top(&end), "*n").unwrap();
    let chain = d.origin(&n.handle, Budget::default()).unwrap();
    assert_eq!(chain.end, OriginEnd::Literal, "{chain:?}");
    let labels: Vec<Label> = hopper_engine::source_hops(&chain)
        .iter()
        .map(|h| {
            let o = h.ownership.as_ref().unwrap();
            (
                o.kind,
                o.src.clone(),
                o.dst.clone(),
                o.span.as_ref().map(|s| s.line),
            )
        })
        .collect();
    let find = |kind: OwnershipKind, src: &str, dst: &str, line: u32| {
        labels
            .iter()
            .position(|l| {
                l.0 == kind
                    && l.1.as_deref().is_some_and(|s| s.contains(src))
                    && l.2.as_deref() == Some(dst)
                    && l.3 == Some(line)
            })
            .unwrap_or_else(|| panic!("no {kind:?} {src} -> {dst} at {line} in {labels:?}"))
    };
    let into_push = find(OwnershipKind::Move, "c", "_8", 16);
    let clone = find(OwnershipKind::Clone, "b", "c", 14);
    let moved = find(OwnershipKind::Move, "a", "b", 13);
    let from = find(OwnershipKind::CallReturn, "String as From", "name", 7);
    assert!(
        into_push < clone && clone < moved && moved < from,
        "{labels:?}"
    );
    // The move a -> b is a whole-place copy at -O0: an exact label.
    let exact = hopper_engine::source_hops(&chain)
        .into_iter()
        .find(|h| {
            h.ownership
                .as_ref()
                .is_some_and(|o| o.kind == OwnershipKind::Move && o.dst.as_deref() == Some("b"))
        })
        .unwrap();
    assert_eq!(
        exact.ownership.unwrap().confidence,
        hopper_core::api::Confidence::Exact
    );
}

/// M4's second half on the step recording: an agent reproduces the chain through MCP from
/// moments alone (run to line 18, then ask for the origin at the moment it returned).
#[test]
fn an_agent_reproduces_the_origin_through_mcp() {
    let Some(rec) = recording(true) else { return };
    let d: Arc<dyn Debugger> = Arc::new(session(&rec));
    let mcp = |lines: &[serde_json::Value]| -> Vec<serde_json::Value> {
        let input: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        let mut out = Vec::new();
        hopper_mcp::serve(d.clone(), std::io::Cursor::new(input.join("\n")), &mut out).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    };
    let call = |id: u64, name: &str, args: serde_json::Value| serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":args}});
    let replies = mcp(&[call(1, "run_until", serde_json::json!({"until":"end"}))]);
    let stop = &replies[0]["result"]["structuredContent"];
    let moment = stop["moment"]
        .as_str()
        .expect("the stop's moment")
        .to_string();
    assert!(moment.starts_with("hopper://"));
    let replies = mcp(&[call(
        2,
        "origin",
        serde_json::json!({"moment": moment, "expr": "(*n).vec[0]"}),
    )]);
    let chain = &replies[0]["result"]["structuredContent"];
    assert_eq!(replies[0]["result"]["isError"], false, "{}", replies[0]);
    assert_eq!(chain["hops"].as_array().map(Vec::len), Some(2), "{chain}");
    assert_eq!(chain["end"], "Literal", "{chain}");
    assert_eq!(chain["hops"][0]["ownership"]["kind"], "Clone", "{chain}");
    assert!(
        chain["hops"][0]["moment"]
            .as_str()
            .is_some_and(|m| m.starts_with("hopper://")),
        "{chain}"
    );
}
