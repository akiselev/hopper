//! rr on the fixtures. rr needs a hardware performance counter, so every test here is
//! `#[ignore]`d; run them on a PMU host with `cargo test -p hopper-rr -- --ignored rr_`.

mod support;

use std::path::{Path, PathBuf};

use hopper_core::target::{BreakKind, Breakpoint, Direction, Resume, StopReason, Target};
use hopper_core::{HopperError, Reg, Tid};
use hopper_rr::{Auxv, PackMode, ProcessExit, RecordOptions, ReplayOptions, Rr, RrReplica};
use support::fixtures::{Elf, fixture};

const FIXTURES: [&str; 3] = ["worked-example", "threads", "values"];

fn rr() -> Rr {
    Rr::detect().expect("rr 5.9 or 5.10 on PATH")
}

fn record(rr: &Rr, name: &str) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join(format!("{name}-trace"));
    let options = RecordOptions {
        output_dir: Some(trace.clone()),
        ..Default::default()
    };
    let rec = rr.record(&fixture(name), &[], &options).expect("rr record");
    assert!(
        rec.status.success(),
        "{name} failed under rr: {}",
        String::from_utf8_lossy(&rec.stderr)
    );
    assert_eq!(rec.trace_dir, trace);
    (trace, dir)
}

fn replay(rr: &Rr, trace: &Path) -> RrReplica {
    let id = rr.recording_id(trace).unwrap();
    RrReplica::start(rr, trace, &ReplayOptions::default(), id).expect("rr replay")
}

fn runtime_addr(t: &mut RrReplica, elf: &Elf, symbol: &str) -> u64 {
    let auxv = Auxv::parse(&t.auxv().unwrap()).unwrap();
    auxv.load_bias_from_entry(elf.entry).unwrap() + elf.symbol(symbol)
}

/// Replay `name` and stop at the first call of `symbol`.
fn replay_to(rr: &Rr, trace: &Path, name: &str, symbol: &str) -> (RrReplica, u64) {
    let elf = Elf::open(&fixture(name));
    let mut t = replay(rr, trace);
    let addr = runtime_addr(&mut t, &elf, symbol);
    t.insert(Breakpoint::code(addr)).unwrap();
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr });
    (t, addr)
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_records_each_fixture() {
    let rr = rr();
    assert!(rr.version.is_supported());
    for name in FIXTURES {
        let (trace, _dir) = record(&rr, name);
        let info = rr.traceinfo(&trace).unwrap();
        assert_ne!(info.uuid.0, [0; 16]);
        assert_eq!(info.ticks_semantics, "rcb");
        let ps = rr.ps(&trace).unwrap();
        assert_eq!(ps.len(), 1, "{name}: {ps:?}");
        assert_eq!(ps[0].exit, ProcessExit::Code(0));
        assert!(ps[0].command.contains(name));
        rr.pack(&trace, PackMode::Copy).unwrap();
        assert_eq!(
            rr.recording_id(&trace).unwrap(),
            info.uuid,
            "packing keeps the uuid"
        );
    }
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_replays_each_fixture() {
    let rr = rr();
    for name in FIXTURES {
        let (trace, _dir) = record(&rr, name);
        let mut t = replay(&rr, &trace);
        assert!(t.info().can_reverse && t.info().can_seek);
        assert_eq!(t.info().recording, rr.recording_id(&trace).unwrap());
        assert_eq!(
            t.info().rr_version.as_deref(),
            Some(rr.version.to_string().as_str())
        );
        assert!(t.info().executable.ends_with(name));
        let start = t.now().unwrap();
        let tid = t.current_thread().unwrap();
        assert!(t.registers(tid).unwrap().pc().is_some());
        let mut last = start;
        loop {
            let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
            if ev.reason == StopReason::HistoryEnd {
                break;
            }
            let now = t.now().unwrap();
            assert!(last.is_before(&now));
            last = now;
        }
        assert!(start.is_before(&t.now().unwrap()));
    }
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_reverse_step_then_step_is_byte_identical() {
    let rr = rr();
    let (trace, _dir) = record(&rr, "worked-example");
    let (mut t, main) = replay_to(&rr, &trace, "worked-example", "worked_example::main");
    t.remove(Breakpoint::code(main)).unwrap();
    let tid = t.current_thread().unwrap();
    for _ in 0..20 {
        t.resume(Resume::Step { tid }, Direction::Forward).unwrap();
    }
    for _ in 0..10 {
        let here = t.registers(tid).unwrap();
        let at = t.now().unwrap();
        assert_eq!(
            t.resume(Resume::Step { tid }, Direction::Reverse)
                .unwrap()
                .reason,
            StopReason::Step
        );
        assert!(t.now().unwrap().is_before(&at));
        assert_eq!(
            t.resume(Resume::Step { tid }, Direction::Forward)
                .unwrap()
                .reason,
            StopReason::Step
        );
        assert_eq!(
            t.registers(tid).unwrap().raw,
            here.raw,
            "registers after back and forth"
        );
        assert_eq!(t.now().unwrap(), at);
        t.resume(Resume::Step { tid }, Direction::Reverse).unwrap();
    }
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_seeks_to_captured_moments() {
    let rr = rr();
    let (trace, _dir) = record(&rr, "threads");
    let (mut t, bump) = replay_to(&rr, &trace, "threads", "threads::bump");
    let mut captured = Vec::new();
    for _ in 0..4 {
        let tid = t.current_thread().unwrap();
        captured.push((t.now().unwrap(), tid, t.registers(tid).unwrap()));
        // Some steps inside the function, so the moments have sub > 0 as well.
        for _ in 0..3 {
            t.resume(Resume::Step { tid }, Direction::Forward).unwrap();
        }
        captured.push((t.now().unwrap(), tid, t.registers(tid).unwrap()));
        let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
        assert_eq!(ev.reason, StopReason::Breakpoint { addr: bump });
    }
    for (m, tid, regs) in captured.iter().rev() {
        t.seek(m).unwrap();
        assert_eq!(&t.now().unwrap(), m);
        assert_eq!(t.registers(*tid).unwrap().raw, regs.raw, "registers at {m}");
    }
    // Breakpoints survive the seeks.
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: bump });
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_checkpoints_and_restores() {
    let rr = rr();
    let (trace, _dir) = record(&rr, "threads");
    let (mut t, bump) = replay_to(&rr, &trace, "threads", "threads::bump");
    let tid = t.current_thread().unwrap();
    let m = t.now().unwrap();
    let regs = t.registers(tid).unwrap();
    let id = t.checkpoint().unwrap();
    for _ in 0..3 {
        t.resume(Resume::Continue, Direction::Forward).unwrap();
    }
    assert!(m.is_before(&t.now().unwrap()));
    t.restore(id).unwrap();
    assert_eq!(t.now().unwrap(), m);
    assert_eq!(t.registers(tid).unwrap().raw, regs.raw);
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: bump });
    t.delete_checkpoint(id).unwrap();
    assert!(t.restore(id).is_err());
}

/// The aligned word in `a` and `b` (reads of the same 24 bytes at two moments) that differs.
fn changed_word(a: &[u8], b: &[u8]) -> usize {
    (0..a.len() / 8)
        .find(|i| a[i * 8..i * 8 + 8] != b[i * 8..i * 8 + 8])
        .map(|i| i * 8)
        .expect("the counter changed between two calls of bump")
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_reverse_continues_to_a_write_watchpoint() {
    let rr = rr();
    let (trace, _dir) = record(&rr, "threads");
    let (mut t, bump) = replay_to(&rr, &trace, "threads", "threads::bump");
    // bump(counter: &Mutex<u64>, by): find the counter word inside the mutex by comparing two
    // calls (std's Mutex layout is not part of any contract).
    let read = |t: &mut RrReplica| {
        let tid = t.current_thread().unwrap();
        let mutex = t.registers(tid).unwrap().get(Reg::RDI).unwrap();
        let mut b = [0u8; 24];
        t.read_memory(mutex, &mut b).unwrap();
        (mutex, b)
    };
    let (mutex, first) = read(&mut t);
    t.resume(Resume::Continue, Direction::Forward).unwrap();
    t.resume(Resume::Continue, Direction::Forward).unwrap();
    let (mutex2, third) = read(&mut t);
    assert_eq!(mutex, mutex2);
    let word = mutex + changed_word(&first, &third) as u64;
    let value_now = |t: &mut RrReplica| {
        let mut b = [0u8; 8];
        t.read_memory(word, &mut b).unwrap();
        u64::from_le_bytes(b)
    };
    let current = value_now(&mut t);

    t.remove(Breakpoint::code(bump)).unwrap();
    let watch = Breakpoint::write_watch(word, 8);
    t.insert(watch).unwrap();
    let ev = t.resume(Resume::Continue, Direction::Reverse).unwrap();
    assert_eq!(
        ev.reason,
        StopReason::Watchpoint {
            kind: BreakKind::WriteWatch,
            addr: word
        }
    );
    // Stopped just after the write that produced the current value.
    assert_eq!(value_now(&mut t), current);
    let tid: Tid = ev.tid;
    t.remove(watch).unwrap();
    t.resume(Resume::Step { tid }, Direction::Reverse).unwrap();
    assert_ne!(
        value_now(&mut t),
        current,
        "one instruction back, before the write"
    );
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_refuses_moments_of_other_recordings() {
    let rr = rr();
    let (trace, _dir) = record(&rr, "worked-example");
    let mut t = replay(&rr, &trace);
    let m = t.now().unwrap();
    let other = hopper_core::Moment {
        recording: hopper_core::RecordingId([9; 16]),
        ..m
    };
    assert!(matches!(t.seek(&other), Err(HopperError::BadMoment(_))));
}
