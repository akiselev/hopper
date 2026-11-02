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
    let reference = info.reference_run.clone().expect("a reference run");
    assert_ne!(reference.id, info.id, "the reference run is another run");

    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        let st = d.index_status();
        assert!(st.error.is_none(), "{st:?}");
        assert!(
            st.phases.iter().all(|(p, _)| p.ends_with("(reference run)")),
            "{st:?}"
        );
        if st.done {
            break;
        }
        assert!(Instant::now() < deadline, "the reference run did not finish: {st:?}");
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
