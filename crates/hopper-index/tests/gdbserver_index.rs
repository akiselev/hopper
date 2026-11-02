//! Phase A and phase B for real, on the fixtures under a live gdbserver.
//!
//! Every test skips, with a message, when gdbserver or the fixtures are unavailable. Index
//! directories are created under `target/index-tests/`.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use hopper_core::api::{CallFilter, Completeness, PageReq, TimelineKind};
use hopper_core::target::{Breakpoint, Direction, Resume, StopReason, Target, TargetMemory};
use hopper_core::{MomentRange, Place, SourceLoc, Tid};
use hopper_eval::{EvalContext, Evaluator, FrameRef};
use hopper_index::writes::{StepWindow, WindowEnd, log_writes_by_stepping};
use hopper_index::{
    AllocatorFunction, Index, IndexConfig, IndexControl, IndexKind, PhaseA, RecordingMeta,
    WriteValue,
};
use hopper_rr::{GdbserverConfig, GdbserverTarget};
use hopper_symbols::fixtures::{fixture_binary, repo_root};
use hopper_symbols::{Binary, Module, SymbolSet};

fn gdbserver_available() -> bool {
    let ok = std::process::Command::new("gdbserver")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    if !ok {
        eprintln!("skipping: gdbserver is not available");
    }
    ok
}

fn launch(exe: &Path) -> (GdbserverTarget, SymbolSet, RecordingMeta) {
    let mut t =
        GdbserverTarget::launch(&GdbserverConfig::default(), exe, &[]).expect("launch gdbserver");
    let binary = Binary::open(exe).expect("open fixture");
    let build_ids = binary.build_id().map(|b| b.to_hex()).into_iter().collect();
    let module = Module::main_from_auxv(binary, &t.auxv().unwrap()).expect("place main module");
    let mut symbols = SymbolSet::new();
    symbols.add(module);
    let meta = RecordingMeta {
        recording: t.info().recording,
        build_ids,
    };
    (t, symbols, meta)
}

/// A fresh index directory under `target/index-tests/`, removed when the guard drops.
fn index_dir(name: &str) -> tempfile::TempDir {
    let root = repo_root().join("target/index-tests");
    std::fs::create_dir_all(&root).unwrap();
    tempfile::Builder::new()
        .prefix(name)
        .tempdir_in(&root)
        .unwrap()
}

fn all() -> PageReq {
    PageReq::first(u32::MAX)
}

#[test]
fn threads_call_tree_counts_recursion_per_thread() {
    if !gdbserver_available() {
        return;
    }
    let Some(exe) = fixture_binary("threads") else {
        return;
    };
    let (mut t, symbols, meta) = launch(&exe);
    let _dir = index_dir("threads");
    let index = Index::open(_dir.path(), meta).unwrap();
    let job = PhaseA::new(symbols, IndexConfig::default());
    let plan = job.plan();
    assert!(
        plan.functions.iter().any(|f| f == "threads::fib"),
        "{:?}",
        plan.functions
    );
    let control = IndexControl::new();
    let s = job.run(&mut t, &index, &control).unwrap();
    assert!(s.reached_end);
    assert!(control.status().done);
    eprintln!("threads: {s:?}");

    let fib = index
        .calls(
            &CallFilter {
                function: Some("threads::fib".into()),
                ..Default::default()
            },
            all(),
        )
        .unwrap();
    assert_eq!(fib.completeness, Completeness::Complete);
    assert_eq!(fib.total, Some(25 + 41));
    let mut per_tid: BTreeMap<Tid, usize> = BTreeMap::new();
    for c in &fib.items {
        *per_tid.entry(c.tid).or_default() += 1;
        assert!(c.exit.is_some(), "every fib call returned: {c:?}");
        assert!(c.entry.is_before(c.exit.as_ref().unwrap()));
    }
    let mut counts: Vec<usize> = per_tid.values().copied().collect();
    counts.sort();
    assert_eq!(counts, vec![25, 41], "{per_tid:?}");
    // The outermost fib of each thread has two fib children; its children nest under it.
    let outer: Vec<_> = fib
        .items
        .iter()
        .filter(|c| {
            c.parent.is_none_or(|p| {
                index
                    .call(p)
                    .unwrap()
                    .is_none_or(|p| p.function != "threads::fib")
            })
        })
        .collect();
    assert_eq!(outer.len(), 2, "{outer:?}");
    for o in &outer {
        let kids = index
            .calls(
                &CallFilter {
                    parent: Some(o.id),
                    ..Default::default()
                },
                all(),
            )
            .unwrap();
        assert_eq!(kids.items.len(), 2);
        assert!(
            kids.items
                .iter()
                .all(|k| k.function == "threads::fib" && k.depth == o.depth + 1)
        );
        assert_eq!(o.child_count, 2);
    }

    let bump = index
        .calls(
            &CallFilter {
                function: Some("threads::bump".into()),
                ..Default::default()
            },
            all(),
        )
        .unwrap();
    assert_eq!(bump.total, Some(6));
    let bump_tids: HashSet<Tid> = bump.items.iter().map(|c| c.tid).collect();
    let fib_tids: HashSet<Tid> = per_tid.keys().copied().collect();
    assert_eq!(bump_tids, fib_tids);
    assert_eq!(bump_tids.len(), 2, "two worker threads");

    // Roots: main's call tree (through the std runtime, only crate functions are nodes).
    let roots = index.calls(&CallFilter::default(), all()).unwrap();
    assert!(
        roots.items.iter().any(|c| c.function == "threads::main"),
        "{:?}",
        roots.items
    );

    let tl = index.timeline(MomentRange::all(), all()).unwrap();
    let starts: Vec<Tid> = tl
        .items
        .iter()
        .filter(|e| e.kind == TimelineKind::ThreadStart)
        .map(|e| e.at.tid)
        .collect();
    let exits: HashSet<Tid> = tl
        .items
        .iter()
        .filter(|e| e.kind == TimelineKind::ThreadExit)
        .map(|e| e.at.tid)
        .collect();
    assert_eq!(starts.len(), 3, "main and two workers: {:?}", tl.items);
    for w in &fib_tids {
        assert!(
            starts.contains(w) && exits.contains(w),
            "worker {w} start and exit: {:?}",
            tl.items
        );
    }
    assert!(matches!(
        tl.items.last().map(|e| &e.kind),
        Some(TimelineKind::ProcessExit { code: Some(0) })
    ));
}

#[test]
fn worked_example_lines_and_allocator_lineage() {
    if !gdbserver_available() {
        return;
    }
    let Some(exe) = fixture_binary("worked-example") else {
        return;
    };
    let (mut t, symbols, meta) = launch(&exe);
    let dir_guard = index_dir("worked");
    let dir = dir_guard.path().to_path_buf();
    let index = Index::open(&dir, meta.clone()).unwrap();
    let job = PhaseA::new(symbols.clone(), IndexConfig::default());
    let s = job.run(&mut t, &index, &IndexControl::new()).unwrap();
    eprintln!("worked-example: {s:?}");
    assert!(s.reached_end);
    assert!(
        s.allocator_symbols
            .iter()
            .any(|a| a.ends_with("__rust_alloc")),
        "{:?}",
        s.allocator_symbols
    );
    assert!(
        s.allocator_symbols.iter().any(|a| a.ends_with(":malloc")),
        "{:?}",
        s.allocator_symbols
    );
    assert!(
        s.libraries.iter().any(|l| l.contains("libc.so")),
        "{:?}",
        s.libraries
    );

    let mut order = Vec::new();
    for line in 12..=18 {
        let hits = index
            .line_hits(&SourceLoc::new("main.rs", line), MomentRange::all(), all())
            .unwrap();
        assert_eq!(hits.completeness, Completeness::Complete);
        assert_eq!(
            hits.items.len(),
            1,
            "line {line} runs once: {:?}",
            hits.items
        );
        order.push(hits.items[0]);
    }
    assert!(
        order.windows(2).all(|w| w[0].is_before(&w[1])),
        "lines run in order: {order:?}"
    );
    let load_lines = index
        .line_hits(&SourceLoc::new("src/main.rs", 7), MomentRange::all(), all())
        .unwrap();
    assert_eq!(load_lines.items.len(), 1);
    assert!(
        index
            .line_hits(&SourceLoc::new("main.rs", 99), MomentRange::all(), all())
            .unwrap()
            .items
            .is_empty()
    );

    let allocs = index.allocations(MomentRange::all(), all()).unwrap();
    for a in &allocs.items {
        eprintln!(
            "alloc {:?} {:#x} size {} by {:?}",
            a.id, a.addr, a.size, a.requested_by
        );
    }
    let by = |needle: &str| {
        allocs
            .items
            .iter()
            .filter(|a| {
                a.requested_by
                    .as_deref()
                    .is_some_and(|r| r.contains(needle))
            })
            .collect::<Vec<_>>()
    };
    let alpha = by("worked_example::load");
    assert_eq!(alpha.len(), 1, "the \"alpha\" buffer");
    assert_eq!(alpha[0].size, 5);
    // The derived `<Config as Clone>::clone`, which DWARF names `worked_example::{impl#0}::clone`.
    let clone = by("worked_example::{impl#0}::clone");
    assert_eq!(clone.len(), 1, "the clone's buffer");
    assert_eq!(clone[0].size, 5);
    let vec_buf: Vec<_> = by("worked_example::main")
        .into_iter()
        .filter(|a| a.size % 32 == 0 && a.size > 0)
        .collect();
    assert_eq!(vec_buf.len(), 1, "the Vec buffer");
    // Line order: alpha before clone before the Vec's buffer.
    assert!(alpha[0].allocated.is_before(&clone[0].allocated));
    assert!(clone[0].allocated.is_before(&vec_buf[0].allocated));
    // The alpha buffer is live at line 18 and its bytes are the literal.
    let at18 = order[6];
    let found = index
        .allocation_at(alpha[0].process, alpha[0].addr + 2, at18)
        .unwrap();
    assert_eq!(found.value.as_ref().map(|a| a.id), Some(alpha[0].id));
    assert_eq!(found.completeness, Completeness::Complete);
    // Every block of these three is freed by the end of main.
    assert!(alpha[0].freed.is_some() && clone[0].freed.is_some() && vec_buf[0].freed.is_some());
    let after = index
        .allocation_at(alpha[0].process, alpha[0].addr, alpha[0].freed.unwrap())
        .unwrap();
    assert_ne!(after.value.map(|a| a.id), Some(alpha[0].id));
    assert!(allocs.items.iter().any(|a| a.requested_by.is_some()));
    let _ = AllocatorFunction::RustAlloc;

    // Reopening reuses the finished index.
    drop(index);
    let reopened = Index::open(&dir, meta.clone()).unwrap();
    assert!(reopened.is_complete(IndexKind::Lines).unwrap());
    assert_eq!(
        reopened
            .line_hits(&SourceLoc::new("main.rs", 14), MomentRange::all(), all())
            .unwrap()
            .items,
        vec![order[2]]
    );
    drop(reopened);
    // A live target has a fresh recording id each launch: the index refuses it.
    let (mut t2, symbols2, meta2) = launch(&exe);
    assert!(Index::open(&dir, meta2).is_err());
    let index = Index::open(&dir, meta).unwrap();
    let again = PhaseA::new(symbols2, IndexConfig::default())
        .run(&mut t2, &index, &IndexControl::new())
        .unwrap();
    assert!(again.reused);
    assert_eq!(again.stops, 0);
}

#[test]
fn caps_mark_results_partial() {
    if !gdbserver_available() {
        return;
    }
    let Some(exe) = fixture_binary("threads") else {
        return;
    };
    let (mut t, symbols, meta) = launch(&exe);
    let _dir = index_dir("capped");
    let index = Index::open(_dir.path(), meta).unwrap();
    let config = IndexConfig {
        max_hits_per_address: 10,
        ..IndexConfig::default()
    };
    let s = PhaseA::new(symbols, config)
        .run(&mut t, &index, &IndexControl::new())
        .unwrap();
    assert!(!s.capped.is_empty());
    let fib = index
        .calls(
            &CallFilter {
                function: Some("threads::fib".into()),
                ..Default::default()
            },
            all(),
        )
        .unwrap();
    assert_eq!(fib.total, Some(10));
    assert!(
        matches!(fib.completeness, Completeness::Partial { .. }),
        "{:?}",
        fib.completeness
    );
    // A function that never reached the cap is still complete.
    let bump = index
        .calls(
            &CallFilter {
                function: Some("threads::bump".into()),
                ..Default::default()
            },
            all(),
        )
        .unwrap();
    assert_eq!(bump.total, Some(6));
    assert_eq!(bump.completeness, Completeness::Complete);
}

#[test]
fn cancelling_keeps_finished_work() {
    if !gdbserver_available() {
        return;
    }
    let Some(exe) = fixture_binary("threads") else {
        return;
    };
    let (mut t, symbols, meta) = launch(&exe);
    let _dir = index_dir("cancel");
    let index = Arc::new(Index::open(_dir.path(), meta).unwrap());
    let control = IndexControl::new();
    // Commit after every few records and cancel from another thread once some are in.
    let config = IndexConfig {
        flush_every: 8,
        ..IndexConfig::default()
    };
    let watcher = {
        let index = index.clone();
        let control = control.clone();
        std::thread::spawn(move || {
            for _ in 0..2000 {
                let n = index
                    .calls(
                        &CallFilter {
                            function: Some("threads::".into()),
                            ..Default::default()
                        },
                        PageReq::first(1),
                    )
                    .unwrap()
                    .total
                    .unwrap_or(0);
                if n >= 8 {
                    control.cancel();
                    return n;
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            0
        })
    };
    let r = PhaseA::new(symbols, config).run(&mut t, &index, &control);
    let seen = watcher.join().unwrap();
    if r.is_ok() {
        eprintln!("the run finished before the cancel arrived ({seen} calls seen)");
        return;
    }
    assert!(matches!(r, Err(hopper_core::HopperError::Cancelled)));
    let calls = index
        .calls(
            &CallFilter {
                function: Some("threads::".into()),
                ..Default::default()
            },
            all(),
        )
        .unwrap();
    assert!(calls.total.unwrap() >= seen as u64);
    assert!(matches!(calls.completeness, Completeness::Partial { .. }));
    assert!(control.status().error.is_some());
}

/// The address and length of local `name` in the innermost frame of the stopped thread.
fn local_place(t: &mut GdbserverTarget, symbols: &SymbolSet, name: &str) -> Place {
    let tid = t.current_thread().unwrap();
    let regs = t.registers(tid).unwrap();
    let process = t.info().process;
    let at = t.now().unwrap();
    let stack = symbols.stack(&mut TargetMemory(t), &regs, tid).unwrap();
    let mut mem = TargetMemory(t);
    let mut cx = EvalContext {
        symbols,
        memory: &mut mem,
        frame: FrameRef::from_stack(&stack[0]),
        at,
        process,
    };
    Evaluator::new()
        .eval(&mut cx, name)
        .unwrap()
        .place
        .expect("in memory")
}

#[test]
fn stepping_logs_the_move_of_b() {
    if !gdbserver_available() {
        return;
    }
    let Some(exe) = fixture_binary("worked-example") else {
        return;
    };
    let (mut t, symbols, meta) = launch(&exe);
    let line = |n: u32| symbols.addresses_for_line("main.rs", n)[0].address;
    let (l13, l15) = (line(13), line(15));
    t.insert(Breakpoint::code(l13)).unwrap();
    let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
    assert_eq!(ev.reason, StopReason::Breakpoint { addr: l13 });
    t.remove(Breakpoint::code(l13)).unwrap();
    // `b`'s scope starts after line 13; `a`'s place is known here.
    let a = local_place(&mut t, &symbols, "a");

    let _dir = index_dir("writes");
    let index = Index::open(_dir.path(), meta).unwrap();
    let mut w = StepWindow::new(WindowEnd::Pc(l15));
    w.max_steps = 200_000;
    let control = IndexControl::new();
    let s = log_writes_by_stepping(&mut t, &w, &index, &control).unwrap();
    eprintln!(
        "phase B over lines 13-14: {} instructions, {} writes, {} undecoded, {:.0} instructions/s (gdbserver)",
        s.instructions,
        s.writes,
        s.undecoded,
        s.instructions_per_second()
    );
    assert_eq!(s.undecoded, 0);
    let end = s.last.unwrap();
    let b = local_place(&mut t, &symbols, "b");
    let Place::Memory {
        addr: b_addr,
        len: b_len,
        ..
    } = b
    else {
        unreachable!()
    };
    assert_eq!(b_len, 32);
    let writes = index
        .writes_in(
            b,
            MomentRange {
                start: None,
                end: Some(end),
            },
            all(),
        )
        .unwrap();
    // The move writes every byte of b.
    let mut covered = vec![false; b_len as usize];
    for w in &writes.items {
        let Place::Memory { addr, len, .. } = w.place else {
            continue;
        };
        for i in addr.max(b_addr)..(addr + len).min(b_addr + b_len) {
            covered[(i - b_addr) as usize] = true;
        }
        assert!(matches!(w.value, WriteValue::Bytes(_)));
    }
    assert!(
        covered.iter().all(|c| *c),
        "writes to b: {:?}",
        writes.items
    );
    // After the move b holds a's bytes; the last writes reproduce them.
    let mut b_now = vec![0u8; 32];
    t.read_memory(b_addr, &mut b_now).unwrap();
    let Place::Memory { addr: a_addr, .. } = a else {
        unreachable!()
    };
    let mut a_now = vec![0u8; 32];
    t.read_memory(a_addr, &mut a_now).unwrap();
    assert_eq!(a_now, b_now, "b is a bitwise move of a");
    let last = index
        .last_write(Place::memory(t.info().process, b_addr, 1), end)
        .unwrap();
    let lw = last.value.expect("a write to b's first byte");
    assert!(
        symbols
            .line_range_at(lw.pc)
            .is_some_and(|r| r.location.line == 13 || r.location.line == 14)
    );
    let ev = lw.to_event(&symbols).unwrap();
    assert!(ev.function.is_some());
    // Single-threaded: the window covers every thread from where it started.
    assert_eq!(last.completeness, Completeness::Complete);
    // Before the window started nothing is known.
    let before = index.last_write(b, s.first.unwrap()).unwrap();
    assert!(before.value.is_none());
    assert!(matches!(before.completeness, Completeness::Partial { .. }));
}

#[test]
fn stepping_throughput_over_a_longer_window() {
    if !gdbserver_available() {
        return;
    }
    let Some(exe) = fixture_binary("threads") else {
        return;
    };
    let (mut t, symbols, meta) = launch(&exe);
    let main = symbols.find_functions("threads::fib")[0].entry;
    t.insert(Breakpoint::code(main)).unwrap();
    t.resume(Resume::Continue, Direction::Forward).unwrap();
    t.remove(Breakpoint::code(main)).unwrap();
    let _dir = index_dir("throughput");
    let index = Index::open(_dir.path(), meta).unwrap();
    let mut w = StepWindow::new(WindowEnd::Steps(500));
    w.flush_every = 1024;
    let s = log_writes_by_stepping(&mut t, &w, &index, &IndexControl::new()).unwrap();
    eprintln!(
        "phase B throughput (gdbserver, cloud container, not a gate): {} instructions, {} writes in {:?}: {:.0} instructions/s",
        s.instructions,
        s.writes,
        s.elapsed,
        s.instructions_per_second()
    );
    assert_eq!(s.instructions, 500);
    assert!(s.writes > 0);
    // Another thread exists by now: the window speaks only for the stepped one.
    let all_threads = index
        .writes_in(
            Place::memory(t.info().process, 0, u64::MAX),
            MomentRange::all(),
            PageReq::first(1),
        )
        .unwrap();
    assert!(matches!(
        all_threads.completeness,
        Completeness::Partial { .. }
    ));
}
