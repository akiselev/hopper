//! `RrReplica` against the in-memory rr stand-in: time, sub tracking, seeks, checkpoints,
//! reverse execution and the ends of the recording.

mod support;

use hopper_core::target::{BreakKind, Breakpoint, Direction, Resume, StopReason, Target};
use hopper_core::{HopperError, Moment, RecordingId, Reg, Tid};
use hopper_rr::{Auxv, RrReplica};
use support::fake_rr::{self, FakeRr, TID, WATCHED};

const REC: RecordingId = RecordingId([0x5a; 16]);

fn replica(start: usize) -> (RrReplica<FakeRr>, FakeRr) {
    let fake = FakeRr::new(start);
    let r = RrReplica::attach(fake.clone(), REC, None).unwrap();
    (r, fake)
}

fn expected(p: usize) -> Moment {
    Moment {
        recording: REC,
        frame_time: fake_rr::frame_time(p),
        tid: Tid(TID),
        ticks: fake_rr::ticks(p),
        sub: fake_rr::sub(p),
    }
}

fn step(r: &mut RrReplica<FakeRr>, dir: Direction) -> StopReason {
    r.resume(Resume::Step { tid: Tid(TID) }, dir).unwrap().reason
}

#[test]
fn attaches_and_describes_the_replay() {
    let (mut r, _fake) = replica(0);
    let info = r.info().clone();
    assert!(info.can_reverse && info.can_seek);
    assert_eq!(info.process.pid, TID);
    assert_eq!(info.executable.to_str(), Some("/tmp/fake-prog"));
    assert_eq!(r.threads().unwrap(), [Tid(TID)]);
    assert_eq!(r.last_event().unwrap(), fake_rr::frame_time(fake_rr::LEN - 1));
    let auxv = Auxv::parse(&r.auxv().unwrap()).unwrap();
    assert_eq!(auxv.entry(), Some(0x1000));
}

#[test]
fn measures_sub_where_rr_stopped_and_leaves_the_state_alone() {
    // Position 8: ticks 2 started at position 6, so sub is 2.
    let (mut r, fake) = replica(8);
    let before = r.registers(Tid(TID)).unwrap();
    assert_eq!(r.now().unwrap(), expected(8));
    assert_eq!(fake.pos(), 8);
    assert_eq!(r.registers(Tid(TID)).unwrap(), before);
    // Measured once: asking again sends nothing that moves the replay.
    let vruns = fake.vruns();
    assert_eq!(r.now().unwrap(), expected(8));
    assert_eq!(fake.vruns(), vruns);
}

#[test]
fn counts_steps_both_ways_without_seeking() {
    let (mut r, fake) = replica(6);
    assert_eq!(r.now().unwrap(), expected(6));
    let vruns = fake.vruns();
    for p in 7..=14 {
        assert_eq!(step(&mut r, Direction::Forward), StopReason::Step);
        assert_eq!(fake.pos(), p);
        assert_eq!(r.now().unwrap(), expected(p), "after a forward step to {p}");
    }
    // 14 -> 13 stays in ticks 4 (sub 1 -> 0); 13 -> 12 changes the event, so sub must be
    // measured; that costs a seek.
    assert_eq!(step(&mut r, Direction::Reverse), StopReason::Step);
    assert_eq!(r.now().unwrap(), expected(13));
    assert_eq!(fake.vruns(), vruns, "steps that keep the tick count need no seek");
    assert_eq!(step(&mut r, Direction::Reverse), StopReason::Step);
    assert_eq!(r.now().unwrap(), expected(12));
    assert_eq!(fake.pos(), 12);
}

#[test]
fn reverse_step_then_step_gives_identical_registers() {
    let (mut r, _fake) = replica(20);
    let at = r.registers(Tid(TID)).unwrap();
    let m = r.now().unwrap();
    step(&mut r, Direction::Reverse);
    assert_ne!(r.registers(Tid(TID)).unwrap().raw, at.raw);
    step(&mut r, Direction::Forward);
    assert_eq!(r.registers(Tid(TID)).unwrap().raw, at.raw);
    assert_eq!(r.now().unwrap(), m);
}

#[test]
fn continue_to_a_breakpoint_measures_sub_and_keeps_breakpoints() {
    let (mut r, fake) = replica(0);
    // rip 0x1008 is at positions 2, 12, 22, ...
    let bp = Breakpoint::code(0x1008);
    r.insert(bp).unwrap();
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x1008 });
    assert_eq!(fake.pos(), 2);
    // Position 2 is ticks 0's third instruction: sub 2, not 0.
    assert_eq!(r.now().unwrap(), expected(2));
    assert_eq!(fake.pos(), 2);
    assert_eq!(fake.inserted_breakpoints(), [0x1008], "measuring re-inserts breakpoints");
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x1008 });
    assert_eq!(fake.pos(), 12);
    assert_eq!(r.now().unwrap(), expected(12));
    let ev = r.resume(Resume::Continue, Direction::Reverse).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x1008 });
    assert_eq!(fake.pos(), 2);
    assert_eq!(r.now().unwrap(), expected(2));
}

#[test]
fn seeks_to_captured_moments() {
    let (mut r, fake) = replica(0);
    let mut captured = Vec::new();
    for p in [3usize, 11, 25, 40] {
        while fake.pos() < p {
            step(&mut r, Direction::Forward);
        }
        captured.push((p, r.now().unwrap(), r.registers(Tid(TID)).unwrap()));
    }
    for (p, m, regs) in captured.iter().rev() {
        r.seek(m).unwrap();
        assert_eq!(fake.pos(), *p);
        assert_eq!(&r.registers(Tid(TID)).unwrap(), regs);
        assert_eq!(&r.now().unwrap(), m);
    }
    // Forward inside one tick count: steps only, no vRun.
    let (_, m40, _) = &captured[3];
    r.seek(m40).unwrap();
    let vruns = fake.vruns();
    let later = Moment { sub: m40.sub + 1, ..*m40 };
    assert_eq!(fake_rr::sub(41), later.sub);
    r.seek(&later).unwrap();
    assert_eq!(fake.pos(), 41);
    assert_eq!(fake.vruns(), vruns);
}

#[test]
fn seek_restores_breakpoints_and_refuses_other_recordings() {
    let (mut r, fake) = replica(0);
    r.insert(Breakpoint::code(0x1010)).unwrap();
    r.seek(&expected(30)).unwrap();
    assert_eq!(fake.pos(), 30);
    assert_eq!(fake.inserted_breakpoints(), [0x1010]);
    let other = Moment { recording: RecordingId([1; 16]), ..expected(3) };
    assert!(matches!(r.seek(&other), Err(HopperError::BadMoment(_))));
    // A moment the recording cannot contain: sub past the end of its tick count.
    let bogus = Moment { sub: 7, ..expected(30) };
    assert!(matches!(r.seek(&bogus), Err(HopperError::BadMoment(_))));
    assert_eq!(fake.inserted_breakpoints(), [0x1010]);
}

#[test]
fn checkpoints_restore_position_and_sub() {
    let (mut r, fake) = replica(0);
    for _ in 0..16 {
        step(&mut r, Direction::Forward);
    }
    let m = r.now().unwrap();
    let regs = r.registers(Tid(TID)).unwrap();
    let id = r.checkpoint().unwrap();
    r.seek(&expected(45)).unwrap();
    r.insert(Breakpoint::code(0x1000)).unwrap();
    let ev = r.restore(id).unwrap();
    assert!(matches!(ev.reason, StopReason::Other { .. }));
    assert_eq!(fake.pos(), 16);
    assert_eq!(r.registers(Tid(TID)).unwrap(), regs);
    let vruns = fake.vruns();
    assert_eq!(r.now().unwrap(), m);
    assert_eq!(fake.vruns(), vruns, "the checkpoint remembered sub");
    assert_eq!(fake.inserted_breakpoints(), [0x1000]);
    // The first resume after a restore is rr's in-place stop; the replica absorbs it.
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x1000 });
    assert_eq!(fake.pos(), 20);
    r.delete_checkpoint(id).unwrap();
    assert!(r.restore(id).is_err());
}

#[test]
fn reverse_continues_to_the_last_write() {
    let (mut r, fake) = replica(0);
    r.seek(&expected(40)).unwrap();
    let w = Breakpoint::write_watch(WATCHED, 8);
    r.insert(w).unwrap();
    let ev = r.resume(Resume::Continue, Direction::Reverse).unwrap();
    assert_eq!(ev.reason, StopReason::Watchpoint { kind: BreakKind::WriteWatch, addr: WATCHED });
    assert_eq!(fake.pos(), 29);
    let mut b = [0u8; 8];
    r.read_memory(WATCHED, &mut b).unwrap();
    assert_eq!(u64::from_le_bytes(b), 3);
    assert_eq!(r.now().unwrap(), expected(29));
    // The kind hopper inserted is reported even though rr always says `watch:`.
    r.remove(w).unwrap();
    let a = Breakpoint { kind: BreakKind::AccessWatch, addr: WATCHED, len: 8 };
    r.insert(a).unwrap();
    let ev = r.resume(Resume::Continue, Direction::Reverse).unwrap();
    assert_eq!(ev.reason, StopReason::Watchpoint { kind: BreakKind::AccessWatch, addr: WATCHED });
    assert_eq!(fake.pos(), 17);
}

#[test]
fn reaches_both_ends_of_the_recording() {
    let (mut r, fake) = replica(5);
    let ev = r.resume(Resume::Continue, Direction::Reverse).unwrap();
    assert_eq!(ev.reason, StopReason::HistoryStart);
    assert_eq!(fake.pos(), 0);
    assert_eq!(r.now().unwrap(), expected(0));
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::HistoryEnd, "rr's synthetic SIGKILL at the last exit");
    let ev = r.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::HistoryEnd, "rr's W00");
    assert!(matches!(
        r.resume(Resume::Continue, Direction::Reverse),
        Err(HopperError::BadMoment(_))
    ));
    r.seek(&expected(10)).unwrap();
    assert_eq!(fake.pos(), 10);
    assert_eq!(r.registers(Tid(TID)).unwrap().get(Reg::RAX), Some(10));
}
