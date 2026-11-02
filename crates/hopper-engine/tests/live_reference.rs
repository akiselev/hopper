//! A live session's index: phase A runs on a second gdbserver run of the same program (the
//! reference run), so calls, line hits and allocations answer for a process that cannot be
//! replayed. Their moments are the reference run's, and say so.

mod support;

use std::time::{Duration, Instant};

use hopper_core::api::{CallFilter, Completeness, Debugger, PageReq};
use hopper_core::{MomentRange, Place, SourceLoc};
use hopper_engine::{Session, SessionOptions};
use support::*;

#[test]
fn calls_line_hits_and_allocations_come_from_the_reference_run() {
    if std::process::Command::new("gdbserver")
        .arg("--version")
        .output()
        .is_err()
    {
        eprintln!("skipping: gdbserver is not installed");
        return;
    }
    let Some(bin) = fixture_binary("worked-example") else {
        return;
    };
    let d = Session::live(&bin, &[], SessionOptions::default()).expect("live session");
    let info = d.recording().unwrap();
    assert_eq!(info.reference_runs.len(), 1, "{info:?}");
    let reference = info.reference_runs[0].clone();
    assert_ne!(reference.id, info.id, "the reference run is another run");

    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let st = d.index_status();
        assert!(st.error.is_none(), "{st:?}");
        assert!(
            st.phases
                .iter()
                .all(|(p, _)| p.ends_with("(reference run)")),
            "{st:?}"
        );
        if st.done {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the reference run did not finish: {st:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // The cursor has not moved: the live process is still at its first instruction.
    let start = d.cursor().unwrap();
    let calls = d
        .calls(
            CallFilter {
                function: Some("worked_example::load".into()),
                ..CallFilter::default()
            },
            PageReq::first(10),
        )
        .unwrap();
    assert_eq!(calls.completeness, Completeness::Complete, "{calls:?}");
    assert_eq!(calls.items.len(), 1, "{calls:?}");
    let load = &calls.items[0];
    assert!(info.is_reference_moment(&load.entry));
    assert!(!info.is_reference_moment(&start.at));
    // Its moments belong to the other run: this session cannot seek to them.
    assert!(d.seek(load.entry).is_err());

    let hits = d
        .line_hits(
            SourceLoc::new("main.rs", 13),
            MomentRange::all(),
            PageReq::first(10),
        )
        .unwrap();
    assert_eq!(hits.completeness, Completeness::Complete);
    assert_eq!(hits.items.len(), 1);
    assert!(info.is_reference_moment(&hits.items[0]));
    // A range in the live run's moments has no counterpart in the reference run: the hits are
    // the stops this session saw.
    let seen = d
        .line_hits(
            SourceLoc::new("main.rs", 13),
            MomentRange {
                start: Some(start.at),
                end: None,
            },
            PageReq::first(10),
        )
        .unwrap();
    assert!(seen.items.is_empty());
    assert!(matches!(seen.completeness, Completeness::Partial { .. }));

    // The live process, at line 18: `v`'s buffer is an allocation the reference run saw (no
    // address randomisation under gdbserver), requested by the push.
    let s18 = run_to_line(&d, "main.rs", 18);
    let elem = d.eval(s18.at, top(&s18), "v[0]").unwrap();
    let Some(Place::Memory { addr, process, .. }) = elem.place else {
        panic!("v[0] is in memory: {elem:?}");
    };
    let alloc = d
        .allocation(process, addr, s18.at)
        .unwrap()
        .expect("the reference run allocated it");
    assert!(addr >= alloc.addr && addr < alloc.addr + alloc.size);
    assert!(info.is_reference_moment(&alloc.allocated));
}

/// `origin` in a live session: a stepped window of another run, from `main`'s entry to the live
/// process's state, answers it with the same six source hops as a recording; its moments are the
/// window's run's, listed as a reference run.
#[test]
fn live_origin_comes_from_a_stepped_window_of_another_run() {
    let Some(bin) = hopper_built_worked_example() else {
        return;
    };
    let Some(d) = live_binary(&bin) else { return };
    let s = run_to_line(&d, "main.rs", 18);
    let n = d.eval(s.at, top(&s), "*n").unwrap();
    let chain = d
        .origin(&n.handle, hopper_core::api::Budget::default())
        .expect("a live origin");
    assert_eq!(chain.end, hopper_core::api::OriginEnd::Literal, "{chain:?}");
    assert_six_source_hops(&chain);
    let info = d.recording().unwrap();
    assert!(
        chain
            .hops
            .iter()
            .chain(&chain.references)
            .all(|h| info.is_reference_moment(&h.at)),
        "{info:?}"
    );
    // the cursor did not move, and the same window answers `last_write`
    assert_eq!(d.cursor().unwrap().at, s.at);
    let b = d.eval(s.at, top(&s), "b.retries").unwrap();
    let w = d.last_write(b.place.unwrap(), s.at).unwrap().unwrap();
    assert_eq!(w.location.map(|l| l.line), Some(13));
    assert_eq!(w.value, 3u32.to_le_bytes());
    // and `value_history`: b changed once in the window, at the move
    let hist = d
        .value_history(
            &b.handle,
            hopper_core::MomentRange::all(),
            PageReq::first(10),
        )
        .unwrap();
    assert!(hist.items.iter().any(|(_, v)| v.summary == "3"), "{hist:?}");
    assert!(hist.items.iter().all(|(m, _)| info.is_reference_moment(m)) || hist.items.is_empty());
    assert_eq!(
        d.recording().unwrap().reference_runs.len(),
        1,
        "one window per moment, reused (the session's reference run is off)"
    );
}
