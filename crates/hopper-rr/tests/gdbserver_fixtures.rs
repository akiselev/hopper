//! `GdbserverTarget` on the real fixtures under a real gdbserver.

mod support;

use std::ffi::OsString;
use std::path::Path;

use hopper_core::target::{
    Breakpoint, Direction, Resume, StopReason, Target, ThreadChange, ThreadEventSource,
    ThreadEventTime,
};
use hopper_core::{HopperError, Reg};
use hopper_rr::{Auxv, GdbserverConfig, GdbserverFactory, GdbserverTarget, ReplicaPool};
use support::fixtures::{Elf, fixture};

fn launch(program: &Path, args: &[&str]) -> GdbserverTarget {
    let args: Vec<OsString> = args.iter().map(OsString::from).collect();
    GdbserverTarget::launch(&GdbserverConfig::default(), program, &args).expect("launch gdbserver")
}

/// The runtime address of `symbol` in the target's main executable.
fn runtime_addr(t: &mut GdbserverTarget, elf: &Elf, symbol: &str) -> u64 {
    let auxv = Auxv::parse(&t.auxv().unwrap()).unwrap();
    auxv.load_bias_from_entry(elf.entry).unwrap() + elf.symbol(symbol)
}

#[test]
fn breaks_on_main_reads_state_and_steps() {
    let exe = fixture("worked-example");
    let elf = Elf::open(&exe);
    let mut t = launch(&exe, &[]);
    assert_eq!(t.info().executable, exe);
    assert!(!t.info().can_reverse);
    let start = t.now().unwrap();

    let main_link = elf.symbol("worked_example::main");
    let main = runtime_addr(&mut t, &elf, "worked_example::main");
    t.insert(Breakpoint::code(main)).unwrap();
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: main });
    let tid = t.current_thread().unwrap();
    assert_eq!(ev.tid, tid);

    let regs = t.registers(tid).unwrap();
    assert_eq!(regs.pc(), Some(main));
    let sp = regs.sp().unwrap();
    assert!(sp > 0x7f00_0000_0000, "stack pointer {sp:#x}");
    assert_eq!(sp % 8, 0);
    assert!(regs.get(Reg::RFLAGS).is_some());
    assert!(
        regs.get(Reg::FS_BASE).is_some(),
        "fs_base from the target description"
    );
    assert!(regs.raw.len() >= 17 * 8);

    // gdbserver shadows its breakpoint: memory reads show the program's own bytes.
    let mut code = [0u8; 16];
    t.read_memory(main, &mut code).unwrap();
    assert_eq!(code.to_vec(), elf.bytes_at(main_link, 16));
    // The return address on the stack is readable too.
    let mut ret = [0u8; 8];
    t.read_memory(sp, &mut ret).unwrap();
    assert_ne!(u64::from_le_bytes(ret), 0);
    // A page that is never mapped.
    let mut b = [0u8; 4];
    assert!(matches!(
        t.read_memory(8, &mut b),
        Err(HopperError::UnreadableMemory { .. })
    ));

    let after_break = t.now().unwrap();
    assert!(start.is_before(&after_break));
    t.remove(Breakpoint::code(main)).unwrap();
    let ev = t.resume(Resume::Step { tid }, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Step);
    let pc = t.registers(tid).unwrap().pc().unwrap();
    assert!(
        pc > main && pc < main + 16,
        "rip advanced from {main:#x} to {pc:#x}"
    );
    let stepped = t.now().unwrap();
    assert_eq!(stepped.frame_time, after_break.frame_time);
    assert_eq!(stepped.sub, 1);

    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Exited { code: 0 });
    assert!(matches!(
        t.registers(tid),
        Err(HopperError::Disconnected(_))
    ));
}

#[test]
fn lists_the_threads_fixture_threads() {
    let exe = fixture("threads");
    let elf = Elf::open(&exe);
    let mut t = launch(&exe, &[]);
    assert_eq!(t.threads().unwrap().len(), 1);
    let bump = runtime_addr(&mut t, &elf, "threads::bump");
    t.insert(Breakpoint::code(bump)).unwrap();
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: bump });
    let threads = t.threads().unwrap();
    assert!(threads.len() >= 2, "{threads:?}");
    assert!(threads.contains(&ev.tid));
    assert!(
        threads.contains(&hopper_core::Tid(t.info().process.pid)),
        "the main thread"
    );
    // Every thread's registers are readable.
    for tid in &threads {
        assert!(t.registers(*tid).unwrap().pc().is_some());
    }
    // Six calls to bump in all (two threads, three each).
    let mut hits = 1;
    loop {
        let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
        match ev.reason {
            StopReason::Breakpoint { addr } => {
                assert_eq!(addr, bump);
                hits += 1;
            }
            StopReason::Exited { code } => {
                assert_eq!(code, 0);
                break;
            }
            other => panic!("unexpected stop {other:?}"),
        }
    }
    assert_eq!(hits, 6);
}

#[test]
fn reports_thread_starts_and_exits_with_their_own_moments() {
    let exe = fixture("threads");
    let elf = Elf::open(&exe);
    let mut t = launch(&exe, &[]);
    assert_eq!(
        t.enable_thread_events().unwrap(),
        ThreadEventSource::ServerReports
    );
    let main = hopper_core::Tid(t.info().process.pid);
    let bump = runtime_addr(&mut t, &elf, "threads::bump");
    t.insert(Breakpoint::code(bump)).unwrap();
    let mut events = Vec::new();
    let mut bump_threads = std::collections::BTreeSet::new();
    let mut last_stop = t.now().unwrap();
    loop {
        let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
        let now = t.now().unwrap();
        for e in t.take_thread_events().unwrap() {
            // Each event is its own stop, after the previous stop and before this one.
            let ThreadEventTime::At(at) = e.time else {
                panic!("a continue times its events: {e:?}")
            };
            assert!(last_stop.is_before(&at) && at.is_before(&now), "{e:?}");
            assert_eq!(at.tid, e.tid);
            events.push(e);
        }
        last_stop = now;
        match ev.reason {
            StopReason::Breakpoint { .. } => {
                bump_threads.insert(ev.tid);
            }
            StopReason::Exited { code } => {
                assert_eq!(code, 0);
                break;
            }
            other => panic!("unexpected stop {other:?}"),
        }
    }
    // Two spawned threads, each started before it first bumps and exited before the end.
    let started: Vec<_> = events
        .iter()
        .filter(|e| e.change == ThreadChange::Started)
        .map(|e| e.tid)
        .collect();
    let exited: Vec<_> = events
        .iter()
        .filter(|e| e.change == ThreadChange::Exited)
        .map(|e| e.tid)
        .collect();
    assert_eq!(started.len(), 2, "{events:?}");
    assert!(!started.contains(&main));
    let mut s = started.clone();
    s.sort();
    let mut x = exited.clone();
    x.sort();
    assert_eq!(s, x, "every spawned thread exits: {events:?}");
    assert_eq!(bump_threads, started.iter().copied().collect());
}

/// Run a shell script under gdbserver (gdbserver passes no argument containing whitespace).
fn launch_script(dir: &tempfile::TempDir, name: &str, body: &str) -> GdbserverTarget {
    let path = dir.path().join(name);
    std::fs::write(&path, body).unwrap();
    launch(Path::new("/bin/sh"), &[path.to_str().unwrap()])
}

#[test]
fn reports_exit_codes_and_fatal_signals() {
    let dir = tempfile::tempdir().unwrap();
    let mut t = launch_script(&dir, "exit3.sh", "exit 3\n");
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Exited { code: 3 });

    let mut t = launch_script(&dir, "kill9.sh", "kill -9 $$\n");
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Killed { signo: 9 });

    // A signal the process does not handle stops it first; continuing delivers it.
    let mut t = launch_script(&dir, "segv.sh", "kill -SEGV $$\n");
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Signal { signo: 11 });

    let args = [OsString::from("-c"), OsString::from("exit 3")];
    assert!(matches!(
        GdbserverTarget::launch(&GdbserverConfig::default(), Path::new("/bin/sh"), &args),
        Err(HopperError::Invalid(_))
    ));
}

#[test]
fn a_live_process_is_a_pool_of_one() {
    let exe = fixture("worked-example");
    let pool = ReplicaPool::new(
        GdbserverFactory {
            config: GdbserverConfig::default(),
            program: exe.clone(),
            args: vec![],
        },
        0,
    )
    .unwrap();
    assert_eq!(pool.background_count(), 0);
    assert!(pool.lease().is_err());
    let pc = pool
        .cursor(|t| {
            let tid = t.current_thread()?;
            t.registers(tid).map(|r| r.pc())
        })
        .unwrap()
        .unwrap();
    assert!(pc.is_some());
    let exe_seen = pool.cursor(|t| t.info().executable.clone()).unwrap();
    assert_eq!(exe_seen, exe);
}
