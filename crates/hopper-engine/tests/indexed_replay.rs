//! The engine with hopper-index: phase A runs on a background replica of a step recording of the
//! worked example (from the entry of the std function that calls `main`, so the index sees
//! `main` called, to line 18), and queries answer from it.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use hopper_core::api::{CallFilter, Completeness, Debugger, OwnershipState, PageReq, Until};
use hopper_core::target::Direction;
use hopper_core::{MomentRange, SourceLoc};
use hopper_engine::{IndexPolicy, Session, SessionOptions};
use support::step_recording::{StepFactory, StepRecording, WindowStart};
use support::*;

/// Wait for phase A to finish (it runs on its own thread).
fn wait_for_index(d: &Session) {
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let st = d.index_status();
        assert!(st.error.is_none(), "phase A failed: {st:?}");
        if st.done {
            return;
        }
        assert!(Instant::now() < deadline, "phase A did not finish: {st:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn queries_answer_from_the_index() {
    let Some(bin) = hopper_built_worked_example() else {
        return;
    };
    if live_binary(&bin).is_none() {
        return;
    }
    let rec = Arc::new(
        StepRecording::capture(
            &bin,
            WindowStart::FunctionEntry("__rust_begin_short_backtrace"),
            18,
            2_000_000,
        )
        .expect("capture"),
    );
    let dir = tempfile::tempdir().unwrap();
    let options = SessionOptions {
        index: IndexPolicy::Dir(dir.path().join("index")),
        ..SessionOptions::default()
    };
    let d = Session::replay(
        StepFactory(rec.clone()),
        2,
        "indexed step recording",
        options,
    )
    .unwrap();
    wait_for_index(&d);

    let hits = d
        .line_hits(
            SourceLoc::new("main.rs", 13),
            MomentRange::all(),
            PageReq::first(10),
        )
        .unwrap();
    assert_eq!(hits.completeness, Completeness::Complete);
    assert_eq!(hits.items.len(), 1);

    let calls = d
        .calls(
            CallFilter {
                function: Some("load".into()),
                ..CallFilter::default()
            },
            PageReq::first(10),
        )
        .unwrap();
    assert_eq!(calls.completeness, Completeness::Complete, "{calls:?}");
    assert_eq!(calls.items.len(), 1, "{calls:?}");
    let load = &calls.items[0];
    assert!(load.function.ends_with("load"));
    assert!(load.exit.is_some_and(|x| load.entry.is_before(&x)));
    // Line 13 starts where load returns to (its return address is line 13's first instruction).
    assert!(!hits.items[0].is_before(&load.exit.unwrap()));

    // Ownership from the index's lines and calls, at line 14.
    let s14 = run_to_line(&d, "main.rs", 14);
    let locals = d.locals(s14.at, top(&s14)).unwrap();
    assert!(
        matches!(&by_name(&locals, "a").ownership, OwnershipState::MovedOut { to: Some(b), .. } if b == "b"),
        "{:?}",
        by_name(&locals, "a").ownership
    );

    // The clone's heap buffer is an allocation requested by the clone.
    let end = d.run(Direction::Forward, Until::EndOfTrace).unwrap();
    let c = d.eval(end.at, top(&end), "(*n).vec").unwrap();
    let byte = d.children(&c.handle, PageReq::first(1)).unwrap().remove(0);
    let Some(hopper_core::Place::Memory { addr, .. }) = byte.place else {
        panic!("no place for {byte:?}");
    };
    let process = d.recording().unwrap().processes[0].id;
    let alloc = d
        .allocation(process, addr, end.at)
        .unwrap()
        .expect("a heap allocation");
    assert!(addr >= alloc.addr && addr < alloc.addr + alloc.size);
    assert!(alloc.allocated.is_before(&end.at));
    // The origin chain names it.
    let chain = d.origin(&byte.handle, Default::default()).unwrap();
    assert_eq!(chain.hops[0].allocation, Some(alloc.id), "{chain:?}");
}
