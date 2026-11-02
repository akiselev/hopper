//! `RrReplica::now_at_stop` and `resolve_visits` against the in-memory rr stand-in: breakpoint
//! stops are named by region visits without seeking, and resolving them gives exactly the
//! moments `now` measures.

mod support;

use hopper_core::target::{Breakpoint, Direction, Resume, StopReason, Target, ThreadEventSource};
use hopper_core::{Moment, RecordingId, RegionVisit, StopMoment, Tid};
use hopper_rr::RrReplica;
use support::fake_rr::{FakeRr, LEN, Layout, TID};

const REC: RecordingId = RecordingId([0x5b; 16]);

/// rip alternates between 0x1000 (even positions) and 0x1004, ticks rise every 7 instructions:
/// 0x1000 comes round up to four times inside one tick region.
const LOOP: Layout = Layout {
    pc_period: 2,
    tick_every: 7,
    event_every: 13,
};

fn moment(layout: Layout, p: usize) -> Moment {
    Moment {
        recording: REC,
        frame_time: layout.frame_time(p),
        tid: Tid(TID),
        ticks: layout.ticks(p),
        sub: layout.sub(p),
    }
}

/// The visit naming position `p`, counted from the fake program's definition.
fn visit(layout: Layout, p: usize) -> RegionVisit {
    let start = layout.region_start(p);
    RegionVisit {
        region: moment(layout, start),
        pc: layout.rip(p),
        visit: (start..p)
            .filter(|q| layout.rip(*q) == layout.rip(p))
            .count() as u32,
    }
}

fn replica(start: usize, layout: Layout) -> (RrReplica<FakeRr>, FakeRr) {
    let fake = FakeRr::with_layout(start, layout);
    let r = RrReplica::attach(fake.clone(), REC, None).unwrap();
    (r, fake)
}

/// Continue to breakpoints until `stop` says so or the recording ends; returns (position,
/// moment) per breakpoint stop and asserts that no stop after the first seeks.
fn collect(
    r: &mut RrReplica<FakeRr>,
    fake: &FakeRr,
    stop: impl Fn(usize) -> bool,
) -> Vec<(usize, StopMoment)> {
    let mut out = Vec::new();
    loop {
        let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
        if !matches!(ev.reason, StopReason::Breakpoint { .. }) {
            return out;
        }
        let p = fake.pos();
        let vruns = fake.vruns();
        let m = r.now_at_stop().unwrap();
        if !out.is_empty() {
            assert_eq!(fake.vruns(), vruns, "the stop at {p} sought");
        }
        assert_eq!(fake.pos(), p, "naming the stop moved the replay");
        out.push((p, m));
        if stop(p) {
            return out;
        }
    }
}

#[test]
fn breakpoint_stops_are_named_by_visits_without_seeking() {
    let (mut r, fake) = replica(0, LOOP);
    r.insert(Breakpoint::code(0x1000)).unwrap();
    let got = collect(&mut r, &fake, |_| false);
    assert_eq!(got.len(), (LEN - 2) / 2, "every even position after 0");
    // The first stop is in the region the replica attached in, which nothing counted: it is
    // measured (and the measuring walk counts the region).
    assert_eq!(got[0], (2, StopMoment::Exact(moment(LOOP, 2))));
    for (p, m) in &got[1..] {
        assert_eq!(*m, StopMoment::Visit(visit(LOOP, *p)), "position {p}");
    }
    // Some visits are not the first of their PC in the region.
    assert!(
        got.iter()
            .any(|(_, m)| matches!(m, StopMoment::Visit(v) if v.visit >= 2))
    );
}

#[test]
fn resolving_visits_gives_the_measured_moments_and_returns() {
    let (mut r, fake) = replica(0, LOOP);
    r.insert(Breakpoint::code(0x1000)).unwrap();
    let first = collect(&mut r, &fake, |p| p >= 30);
    let here = fake.pos();
    assert_eq!(here, 30);
    let visits: Vec<RegionVisit> = first
        .iter()
        .filter_map(|(_, m)| match m {
            StopMoment::Visit(v) => Some(*v),
            StopMoment::Exact(_) => None,
        })
        .collect();
    // Out of order and with a repeat: answers follow the request.
    let mut asked = visits.clone();
    asked.reverse();
    asked.push(visits[0]);
    let vruns = fake.vruns();
    let resolved = r.resolve_visits(&asked).unwrap();
    let positions: Vec<usize> = first
        .iter()
        .filter(|(_, m)| matches!(m, StopMoment::Visit(_)))
        .map(|(p, _)| *p)
        .collect();
    let mut want: Vec<Moment> = positions.iter().rev().map(|p| moment(LOOP, *p)).collect();
    want.push(moment(LOOP, positions[0]));
    assert_eq!(resolved, want);
    // One walk per region (each starting with a seek, or two when rr needs ticks after the
    // event) plus the way back: far fewer seeks than stops.
    let regions: std::collections::BTreeSet<(u64, u64)> =
        visits.iter().map(|v| v.region_key()).collect();
    assert!(fake.vruns() - vruns <= 2 * (regions.len() + 1));
    // Back where it was, with the breakpoint in place and the stop still cheap to name.
    assert_eq!(fake.pos(), here);
    assert_eq!(fake.inserted_breakpoints(), [0x1000]);
    assert_eq!(r.now().unwrap(), moment(LOOP, here));
    let rest = collect(&mut r, &fake, |_| false);
    assert!(!rest.is_empty());
    for (p, m) in &rest {
        assert_eq!(*m, StopMoment::Visit(visit(LOOP, *p)), "position {p}");
    }
    // At the end of the recording: resolution still works (the replica stays at the last one).
    let last: Vec<RegionVisit> = rest
        .iter()
        .map(|(_, m)| match m {
            StopMoment::Visit(v) => *v,
            StopMoment::Exact(e) => panic!("measured {e}"),
        })
        .collect();
    let resolved = r.resolve_visits(&last).unwrap();
    let want: Vec<Moment> = rest.iter().map(|(p, _)| moment(LOOP, *p)).collect();
    assert_eq!(resolved, want);
    // And every resolved moment seeks to the right place.
    for (m, (p, _)) in resolved.iter().zip(&rest).rev() {
        r.seek(m).unwrap();
        assert_eq!(fake.pos(), *p);
    }
}

#[test]
fn a_breakpoint_inserted_inside_a_region_is_measured() {
    let (mut r, fake) = replica(0, Layout::DEFAULT);
    // 0x1004 is at positions 1, 11, ...; region (event 1, ticks 0) is positions 0..=2.
    r.insert(Breakpoint::code(0x1004)).unwrap();
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x1004 });
    assert_eq!(
        r.now_at_stop().unwrap(),
        StopMoment::Exact(moment(Layout::DEFAULT, 1))
    );
    // 0x1008 (position 2) joins in the middle of the region: its earlier visits were not seen.
    r.insert(Breakpoint::code(0x1008)).unwrap();
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x1008 });
    let vruns = fake.vruns();
    assert_eq!(
        r.now_at_stop().unwrap(),
        StopMoment::Exact(moment(Layout::DEFAULT, 2))
    );
    assert!(fake.vruns() > vruns, "measured");
    // In the next region both are tracked from its start.
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x1004 });
    assert_eq!(fake.pos(), 11);
    assert_eq!(
        r.now_at_stop().unwrap(),
        StopMoment::Visit(visit(Layout::DEFAULT, 11))
    );
    // A removed breakpoint is no longer counted, so its address cannot name a stop there.
    r.remove(Breakpoint::code(0x1004)).unwrap();
    r.insert(Breakpoint::code(0x1004)).unwrap();
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x1008 });
    assert_eq!(fake.pos(), 12);
    assert_eq!(
        r.now_at_stop().unwrap(),
        StopMoment::Visit(visit(Layout::DEFAULT, 12))
    );
}

#[test]
fn single_steps_and_reverse_runs_keep_the_count_honest() {
    let (mut r, fake) = replica(0, LOOP);
    r.insert(Breakpoint::code(0x1000)).unwrap();
    collect(&mut r, &fake, |p| p >= 8);
    // A step onto 0x1004 and one onto 0x1000 again: the second is a visit the next continue
    // steps over rather than reports, so it has to be counted here.
    let tid = Tid(TID);
    assert_eq!(
        r.resume(Resume::Step { tid }, Direction::Forward)
            .unwrap()
            .reason,
        StopReason::Step
    );
    assert_eq!(
        r.resume(Resume::Step { tid }, Direction::Forward)
            .unwrap()
            .reason,
        StopReason::Step
    );
    assert_eq!(fake.pos(), 10);
    let after = collect(&mut r, &fake, |_| true);
    assert_eq!(after[0], (12, StopMoment::Visit(visit(LOOP, 12))));
    // Running backwards forgets the count: the stop is measured.
    let ev = r.resume(Resume::Continue, Direction::Reverse).unwrap();
    assert!(matches!(ev.reason, StopReason::Breakpoint { .. }));
    assert_eq!(fake.pos(), 10);
    assert_eq!(
        r.now_at_stop().unwrap(),
        StopMoment::Exact(moment(LOOP, 10))
    );
}

#[test]
fn rr_reports_threads_by_list_and_names_the_scheduled_thread() {
    let (mut r, _fake) = replica(0, Layout::DEFAULT);
    assert_eq!(
        r.enable_thread_events().unwrap(),
        ThreadEventSource::ThreadListDiff
    );
    r.resume(Resume::Step { tid: Tid(TID) }, Direction::Forward)
        .unwrap();
    assert!(r.take_thread_events().unwrap().is_empty());
    assert_eq!(r.scheduled_thread().unwrap(), Some(Tid(TID)));
}
