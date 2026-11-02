use hopper_core::PositionUrl;
use hopper_core::api::{MachineKind, OwnershipKind, OwnershipState};

use super::*;

fn main_frame() -> FrameId {
    FrameId { tid: TID, depth: 0 }
}

fn at_line(d: &FakeDebugger, function: &str, line: u32) -> Moment {
    d.first_moment_at(function, line)
        .unwrap_or_else(|| panic!("{function}:{line}"))
}

fn line_of(stop: &Stop) -> u32 {
    stop.location.as_ref().unwrap().line
}

#[test]
fn starts_at_main_line_12() {
    let d = FakeDebugger::new();
    let c = d.cursor().unwrap();
    assert_eq!(line_of(&c), 12);
    assert_eq!(c.function.as_deref(), Some(script::MAIN));
    assert_eq!(c.cause, StopCause::HistoryStart);
    let info = d.recording().unwrap();
    assert_eq!(info.start, Some(c.at));
    assert_eq!(info.processes[0].threads, vec![TID]);
}

#[test]
fn step_over_walks_main_lines() {
    let d = FakeDebugger::new();
    let mut lines = vec![line_of(&d.cursor().unwrap())];
    for _ in 0..8 {
        let s = d.step(StepKind::Over, Direction::Forward).unwrap();
        assert_eq!(s.function.as_deref(), Some(script::MAIN));
        lines.push(line_of(&s));
    }
    assert_eq!(lines, vec![12, 13, 14, 15, 16, 17, 18, 19, 19]);
    let end = d.step(StepKind::Over, Direction::Forward).unwrap();
    assert_eq!(end.cause, StopCause::HistoryEnd);
}

#[test]
fn step_into_enters_calls_and_out_returns() {
    let d = FakeDebugger::new();
    let s = d.step(StepKind::Into, Direction::Forward).unwrap();
    assert_eq!(s.function.as_deref(), Some(script::LOAD));
    assert_eq!(line_of(&s), 7);
    let s = d.step(StepKind::Into, Direction::Forward).unwrap();
    assert_eq!(s.function.as_deref(), Some(script::STRING_FROM));
    let s = d.step(StepKind::Out, Direction::Forward).unwrap();
    assert_eq!(s.function.as_deref(), Some(script::LOAD));
    assert_eq!(line_of(&s), 7);
    let s = d.step(StepKind::Out, Direction::Forward).unwrap();
    assert_eq!(s.function.as_deref(), Some(script::MAIN));
    assert_eq!(line_of(&s), 12);
}

#[test]
fn reverse_steps_mirror_forward() {
    let d = FakeDebugger::new();
    d.seek(at_line(&d, "main", 16)).unwrap();
    let s = d.step(StepKind::Over, Direction::Reverse).unwrap();
    assert_eq!(
        (s.function.as_deref(), line_of(&s)),
        (Some(script::MAIN), 15)
    );
    let s = d.step(StepKind::Over, Direction::Reverse).unwrap();
    assert_eq!(line_of(&s), 14);
    assert_eq!(
        s.at,
        at_line(&d, "main", 14),
        "lands on the start of line 14"
    );
    // Reverse into from the start of line 15 goes into the end of the clone call.
    d.seek(at_line(&d, "main", 15)).unwrap();
    let s = d.step(StepKind::Into, Direction::Reverse).unwrap();
    assert_eq!(line_of(&s), 14);
    // Reverse out of push lands on the call in main.
    d.seek(at_line(&d, "push", 2405)).unwrap();
    let s = d.step(StepKind::Out, Direction::Reverse).unwrap();
    assert_eq!(
        (s.function.as_deref(), line_of(&s)),
        (Some(script::MAIN), 16)
    );
    // Instruction steps are exact inverses.
    let here = d.cursor().unwrap().at;
    d.step(StepKind::Instruction, Direction::Forward).unwrap();
    let back = d.step(StepKind::Instruction, Direction::Reverse).unwrap();
    assert_eq!(back.at, here);
    assert_eq!(
        d.registers(here, TID).unwrap(),
        d.registers(back.at, TID).unwrap()
    );
}

#[test]
fn history_start_and_end() {
    let d = FakeDebugger::new();
    let s = d.step(StepKind::Instruction, Direction::Reverse).unwrap();
    assert_eq!(s.cause, StopCause::HistoryStart);
    let s = d.run(Direction::Forward, Until::EndOfTrace).unwrap();
    assert_eq!(s.cause, StopCause::HistoryEnd);
    assert_eq!(Some(s.at), d.recording().unwrap().end);
}

#[test]
fn breakpoints_stop_run_both_ways() {
    let d = FakeDebugger::new();
    let line16 = d
        .add_breakpoint(BreakLocation::Source(SourceLoc::new("main.rs", 16)))
        .unwrap();
    let push = d
        .add_breakpoint(BreakLocation::Function("push".into()))
        .unwrap();
    assert_eq!(d.breakpoints().unwrap().len(), 2);
    let s = d.run(Direction::Forward, Until::Breakpoints).unwrap();
    assert_eq!(s.cause, StopCause::Breakpoint { id: line16.id });
    assert_eq!(line_of(&s), 16);
    let s = d.run(Direction::Forward, Until::Breakpoints).unwrap();
    assert_eq!(s.cause, StopCause::Breakpoint { id: push.id });
    assert_eq!(s.function.as_deref(), Some(script::VEC_PUSH));
    let s = d.run(Direction::Forward, Until::Breakpoints).unwrap();
    assert_eq!(s.cause, StopCause::HistoryEnd);
    let s = d.run(Direction::Reverse, Until::Breakpoints).unwrap();
    assert_eq!(s.cause, StopCause::Breakpoint { id: push.id });
    d.remove_breakpoint(push.id).unwrap();
    assert!(d.remove_breakpoint(push.id).is_err());
    let s = d.run(Direction::Reverse, Until::Breakpoints).unwrap();
    assert_eq!(s.cause, StopCause::Breakpoint { id: line16.id });
    assert!(
        d.add_breakpoint(BreakLocation::Source(SourceLoc::new("main.rs", 1)))
            .is_err()
    );
}

#[test]
fn watch_stops_at_writes() {
    let d = FakeDebugger::new();
    let b = d.eval(at_line(&d, "main", 18), main_frame(), "b").unwrap();
    let place = b.place.unwrap();
    let s = d.run(Direction::Forward, Until::Watch(place)).unwrap();
    assert_eq!(s.cause, StopCause::Watchpoint { place });
    assert_eq!(line_of(&s), 13);
    let w = d.last_write(place, s.at).unwrap().unwrap();
    assert_eq!(w.at, s.at);
    assert_eq!(w.location.unwrap().line, 13);
}

#[test]
fn navigation_history_round_trips() {
    let d = FakeDebugger::new();
    let start = d.cursor().unwrap().at;
    let m14 = d.seek(at_line(&d, "main", 14)).unwrap().at;
    let m17 = d.seek(at_line(&d, "main", 17)).unwrap().at;
    assert_eq!(d.back().unwrap().unwrap().at, m14);
    assert_eq!(d.back().unwrap().unwrap().at, start);
    assert_eq!(d.back().unwrap(), None);
    assert_eq!(d.forward().unwrap().unwrap().at, m14);
    assert_eq!(d.forward().unwrap().unwrap().at, m17);
    assert_eq!(d.forward().unwrap(), None);
    assert_eq!(d.cursor().unwrap().at, m17);
}

#[test]
fn position_urls_round_trip_through_seek() {
    let d = FakeDebugger::new();
    for m in d.moments() {
        let url = m.url().to_string();
        let parsed: PositionUrl = url.parse().unwrap();
        assert_eq!(d.seek(parsed.0).unwrap().at, m);
    }
    let mut other = d.moments()[3];
    other.recording = hopper_core::RecordingId([0; 16]);
    assert!(matches!(d.seek(other), Err(HopperError::BadMoment(_))));
}

#[test]
fn locals_change_over_time_and_a_is_moved_out() {
    let d = FakeDebugger::new();
    let at12 = at_line(&d, "main", 12);
    let locals = d.locals(at12, main_frame()).unwrap();
    assert_eq!(locals.len(), 1);
    assert_eq!(locals[0].summary, "<uninitialised>");

    let at13 = at_line(&d, "main", 13);
    let a = d.eval(at13, main_frame(), "a").unwrap();
    assert_eq!(a.summary, "Config { name: \"alpha\", retries: 3 }");
    assert_eq!(a.ownership, OwnershipState::Live);

    let at14 = at_line(&d, "main", 14);
    let a = d.eval(at14, main_frame(), "a").unwrap();
    assert!(matches!(&a.ownership, OwnershipState::MovedOut { to: Some(t), .. } if t == "b"));
    let b = d.eval(at14, main_frame(), "b").unwrap();
    assert_eq!(b.ownership, OwnershipState::Live);

    let at18 = at_line(&d, "main", 18);
    let names: Vec<String> = d
        .locals(at18, main_frame())
        .unwrap()
        .into_iter()
        .map(|v| v.name)
        .collect();
    assert_eq!(names, ["a", "b", "c", "v", "n"]);
    let n = d.eval(at18, main_frame(), "n").unwrap();
    assert_eq!(n.type_name, "&alloc::string::String");
    assert_eq!(n.summary, "&\"alpha\"");
    let kids = d.children(&n.handle, PageReq::first(10)).unwrap();
    assert_eq!(kids.len(), 1);
    assert_eq!(kids[0].summary, "\"alpha\"");
    assert_eq!(kids[0].handle.path, vec![PathElem::Deref]);
}

#[test]
fn nested_children_and_paths() {
    let d = FakeDebugger::new();
    let at = at_line(&d, "main", 17);
    let v = d.eval(at, main_frame(), "v").unwrap();
    assert_eq!(v.type_name, "alloc::vec::Vec<worked_example::Config>");
    assert_eq!(v.summary, "Vec(len=1)");
    let elems = d.children(&v.handle, PageReq::first(10)).unwrap();
    assert_eq!(elems[0].name, "[0]");
    let fields = d.children(&elems[0].handle, PageReq::first(10)).unwrap();
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    assert_eq!(names, ["name", "retries"]);
    assert_eq!(fields[1].summary, "3");
    // The handle path and the expression agree.
    let direct = d.eval(at, main_frame(), "v[0].name").unwrap();
    assert_eq!(d.view(&fields[0].handle).unwrap().summary, direct.summary);
    assert_eq!(fields[0].place, direct.place);
    // The bytes in memory are the string's text.
    let Place::Memory { addr, .. } = direct.place.unwrap() else {
        panic!()
    };
    let header = d.memory(at, ProcessId::new(PID), addr, 24).unwrap();
    let ptr = u64::from_le_bytes(header[8..16].try_into().unwrap());
    let len = u64::from_le_bytes(header[16..24].try_into().unwrap());
    assert_eq!(
        d.memory(at, ProcessId::new(PID), ptr, len).unwrap(),
        b"alpha"
    );
    // Before the push, v is empty.
    let early = d.eval(at_line(&d, "main", 16), main_frame(), "v").unwrap();
    assert_eq!(early.summary, "Vec(len=0)");
    assert!(d.eval(at, main_frame(), "nope").is_err());
    assert!(d.eval(at, main_frame(), "b.missing").is_err());
}

#[test]
fn queries_do_not_move_the_cursor() {
    let d = FakeDebugger::new();
    let before = d.cursor().unwrap();
    let late = at_line(&d, "main", 18);
    d.locals(late, main_frame()).unwrap();
    d.frames(late, TID).unwrap();
    let n = d.eval(late, main_frame(), "n").unwrap();
    d.origin(&n.handle, Budget::default()).unwrap();
    d.value_history(&n.handle, MomentRange::all(), PageReq::first(10))
        .unwrap();
    d.calls(CallFilter::default(), PageReq::first(10)).unwrap();
    assert_eq!(d.cursor().unwrap(), before);
}

#[test]
fn frames_inside_clone() {
    let d = FakeDebugger::new();
    let at = at_line(&d, "Clone", 3);
    let frames = d.frames(at, TID).unwrap();
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].function.as_deref(), Some(script::CONFIG_CLONE));
    assert_eq!(frames[1].function.as_deref(), Some(script::MAIN));
    assert_eq!(frames[1].location.as_ref().unwrap().line, 14);
    let s = d.eval(at, frames[0].id, "self.name").unwrap();
    assert_eq!(s.summary, "\"alpha\"");
    let outer = d.eval(at, frames[1].id, "b.retries").unwrap();
    assert_eq!(outer.summary, "3");
}

#[test]
fn call_tree() {
    let d = FakeDebugger::new();
    let roots = d.calls(CallFilter::default(), PageReq::first(10)).unwrap();
    assert_eq!(roots.items.len(), 1);
    let main = &roots.items[0];
    assert_eq!(main.function, script::MAIN);
    assert_eq!(main.child_count, 4);
    let kids = d
        .calls(
            CallFilter {
                parent: Some(main.id),
                ..Default::default()
            },
            PageReq::first(10),
        )
        .unwrap();
    let names: Vec<&str> = kids.items.iter().map(|c| c.function.as_str()).collect();
    assert_eq!(
        names,
        [
            script::LOAD,
            script::CONFIG_CLONE,
            script::VEC_PUSH,
            script::PRINT
        ]
    );
    assert!(kids.items.iter().all(|c| c.exit.is_some() && c.depth == 1));
    let from = d
        .calls(
            CallFilter {
                function: Some("From".into()),
                ..Default::default()
            },
            PageReq::first(10),
        )
        .unwrap();
    assert_eq!(from.items.len(), 1);
    assert_eq!(from.items[0].parent, Some(kids.items[0].id));
    assert_eq!(from.completeness, Completeness::Complete);
}

#[test]
fn line_hits_and_paging() {
    let d = FakeDebugger::new();
    let hits = d.line_hits(
        SourceLoc::new("main.rs", 3),
        MomentRange::all(),
        PageReq::first(10),
    );
    assert_eq!(hits.unwrap().items, vec![at_line(&d, "Clone", 3)]);
    let hits = d
        .line_hits(
            SourceLoc::new("main.rs", 19),
            MomentRange::all(),
            PageReq::first(10),
        )
        .unwrap();
    assert_eq!(hits.items.len(), 1);
    let hits = d
        .line_hits(
            SourceLoc::new("main.rs", 12),
            MomentRange::all(),
            PageReq::first(10),
        )
        .unwrap();
    assert_eq!(
        hits.items.len(),
        1,
        "a line resumed after a call is one hit"
    );
    let page = d
        .timeline(
            MomentRange::all(),
            PageReq {
                offset: 1,
                limit: 2,
            },
        )
        .unwrap();
    assert_eq!(page.total, Some(4));
    assert_eq!(page.items.len(), 2);
}

#[test]
fn value_history_follows_the_frame() {
    let d = FakeDebugger::new();
    let a = d.eval(at_line(&d, "main", 18), main_frame(), "a").unwrap();
    let h = d
        .value_history(&a.handle, MomentRange::all(), PageReq::first(20))
        .unwrap();
    let summaries: Vec<(&str, bool)> = h
        .items
        .iter()
        .map(|(_, v)| {
            (
                v.summary.as_str(),
                matches!(v.ownership, OwnershipState::MovedOut { .. }),
            )
        })
        .collect();
    assert_eq!(
        summaries,
        [
            ("<uninitialised>", false),
            ("Config { name: \"alpha\", retries: 3 }", false),
            ("Config { name: \"alpha\", retries: 3 }", true),
        ]
    );
    // The middle entry is observed while load's frame is innermost: a is found by its frame.
    let (m, v) = &h.items[1];
    assert_eq!(v.handle.frame.unwrap().depth, 1);
    assert_eq!(
        d.frames(*m, TID).unwrap()[0].function.as_deref(),
        Some(script::LOAD)
    );
}

#[test]
fn origin_is_the_six_hop_chain() {
    let d = FakeDebugger::new();
    let at = at_line(&d, "main", 18);
    let n = d.eval(at, main_frame(), "*n").unwrap();
    let chain = d.origin(&n.handle, Budget::default()).unwrap();
    // The borrow wrote `n`, the reference `*n` is reached through: a reference hop, first.
    assert_eq!(chain.references.len(), 1);
    let source: Vec<&OriginHop> = chain.references.iter().chain(&chain.hops).collect();
    let kinds: Vec<OwnershipKind> = source
        .iter()
        .map(|h| h.ownership.as_ref().unwrap().kind)
        .collect();
    assert_eq!(
        kinds,
        [
            OwnershipKind::SharedBorrow,
            OwnershipKind::Move,
            OwnershipKind::Clone,
            OwnershipKind::Move,
            OwnershipKind::CallReturn,
            OwnershipKind::Literal,
        ]
    );
    let spans: Vec<u32> = source
        .iter()
        .map(|h| h.ownership.as_ref().unwrap().span.as_ref().unwrap().line)
        .collect();
    assert_eq!(spans, [17, 16, 14, 13, 12, 7]);
    assert_eq!(chain.end, OriginEnd::Literal);
    for w in source.windows(2) {
        assert!(w[1].at < w[0].at, "hops go back in time");
    }
    let allocs: Vec<Option<u64>> = source
        .iter()
        .map(|h| h.allocation.map(|a| a.0))
        .collect();
    assert_eq!(allocs, [None, Some(3), Some(2), None, None, Some(1)]);
    // The last hop read the literal from .rodata.
    let last = chain.hops.last().unwrap();
    let Some(Place::Memory { addr, len, .. }) = last.read else {
        panic!()
    };
    assert_eq!(
        d.memory(at, ProcessId::new(PID), addr, len).unwrap(),
        b"alpha"
    );
    // The borrow is an arithmetic node (an address computation) with its operand; `a`'s hop has
    // load's aggregate underneath it.
    assert_eq!(chain.references[0].machine, MachineKind::Arithmetic);
    assert_eq!(chain.references[0].operands.len(), 1);
    let inner: Vec<OwnershipKind> = chain.hops[3].inner.iter().map(|l| l.kind).collect();
    assert_eq!(inner, [OwnershipKind::Aggregate, OwnershipKind::Move]);
    // The origin of `n` itself is the borrow alone.
    let n_ref = d.eval(at, main_frame(), "n").unwrap();
    let own = d.origin(&n_ref.handle, Budget::default()).unwrap();
    assert!(own.references.is_empty());
    assert_eq!(own.hops, chain.references);
    // Every hop's written place really was written at that moment.
    for hop in &source {
        let w = d.last_write(hop.written, hop.at).unwrap().unwrap();
        assert_eq!(w.at, hop.at);
        assert_eq!(w.pc, hop.pc);
    }
    let short = d
        .origin(
            &n.handle,
            Budget {
                max_hops: 2,
                max_steps: 10,
            },
        )
        .unwrap();
    assert_eq!(short.hops.len(), 2);
    assert_eq!(short.end, OriginEnd::Budget);
    let retries = d.eval(at, main_frame(), "b.retries").unwrap();
    let chain = d.origin(&retries.handle, Budget::default()).unwrap();
    assert_eq!(
        chain
            .hops
            .last()
            .unwrap()
            .ownership
            .as_ref()
            .unwrap()
            .src
            .as_deref(),
        Some("3")
    );
    let early = d.eval(at_line(&d, "main", 12), main_frame(), "a").unwrap();
    let chain = d.origin(&early.handle, Budget::default()).unwrap();
    assert!(chain.hops.is_empty());
    assert!(matches!(chain.end, OriginEnd::Unknown { .. }));
}

#[test]
fn allocations_live_and_freed() {
    let d = FakeDebugger::new();
    let p = ProcessId::new(PID);
    let n = d.eval(at_line(&d, "main", 18), main_frame(), "*n").unwrap();
    let chain = d.origin(&n.handle, Budget::default()).unwrap();
    let vec_hop = &chain.hops[0];
    let Place::Memory { addr, .. } = vec_hop.written else {
        panic!()
    };
    let a = d.allocation(p, addr + 8, vec_hop.at).unwrap().unwrap();
    assert_eq!(a.id, vec_hop.allocation.unwrap());
    assert_eq!(a.requested_by.as_deref(), Some(script::VEC_PUSH));
    let end = d.recording().unwrap().end.unwrap();
    assert_eq!(
        d.allocation(p, addr, end).unwrap(),
        None,
        "freed by the drop of v"
    );
    assert_eq!(
        d.allocation(p, addr, d.moments()[0]).unwrap(),
        None,
        "not yet allocated"
    );
}

#[test]
fn disassembly_matches_memory_and_stops() {
    let d = FakeDebugger::new();
    let c = d.cursor().unwrap();
    let ins = d.disassemble(c.at, ProcessId::new(PID), c.pc, 3).unwrap();
    assert_eq!(ins[0].addr, c.pc);
    assert_eq!(ins[1].addr, ins[0].addr + ins[0].bytes.len() as u64);
    let bytes = d
        .memory(c.at, ProcessId::new(PID), c.pc, ins[0].bytes.len() as u64)
        .unwrap();
    assert_eq!(bytes, ins[0].bytes);
    assert_eq!(d.registers(c.at, TID).unwrap().pc(), Some(c.pc));
    assert!(d.memory(c.at, ProcessId::new(PID), 0x10, 4).is_err());
}

#[test]
fn source_text_by_suffix() {
    let d = FakeDebugger::new();
    let text = d.source_text("src/main.rs").unwrap();
    assert_eq!(
        text.lines().nth(6).unwrap().trim_start(),
        "let name = String::from(\"alpha\");     // line 7"
    );
    assert!(d.source_text("other.rs").is_err());
}

#[test]
fn index_progresses_and_limits_queries() {
    let d = FakeDebugger::with_config(FakeConfig {
        latency: None,
        index: IndexSchedule::AfterPolls(8),
    });
    let first = d.calls(CallFilter::default(), PageReq::first(10)).unwrap();
    assert!(first.items.is_empty());
    assert!(matches!(first.completeness, Completeness::Partial { .. }));
    let mut last = d.index_status();
    assert!(!last.done);
    loop {
        let s = d.index_status();
        for ((_, a), (_, b)) in last.phases.iter().zip(&s.phases) {
            assert!(b >= a);
        }
        last = s;
        if last.done {
            break;
        }
    }
    let after = d.calls(CallFilter::default(), PageReq::first(10)).unwrap();
    assert_eq!(after.items.len(), 1);
    assert_eq!(after.completeness, Completeness::Complete);
}

#[test]
fn latency_is_applied() {
    let d = FakeDebugger::with_config(FakeConfig {
        latency: Some(Duration::from_millis(20)),
        index: IndexSchedule::Complete,
    });
    let t = Instant::now();
    d.cursor().unwrap();
    assert!(t.elapsed() >= Duration::from_millis(20));
}

#[test]
fn is_a_shareable_trait_object() {
    let d: Arc<dyn Debugger> = FakeDebugger::shared();
    let d2 = d.clone();
    let h = std::thread::spawn(move || d2.step(StepKind::Over, Direction::Forward).unwrap());
    let stop = h.join().unwrap();
    assert_eq!(d.cursor().unwrap().at, stop.at);
}
