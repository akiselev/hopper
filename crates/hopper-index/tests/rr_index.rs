//! The index over rr. rr needs a hardware performance counter, so every test here is
//! `#[ignore]`d; run them on a PMU host with `cargo test -p hopper-index -- --ignored rr_`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hopper_core::api::{CallFilter, Completeness, PageReq};
use hopper_core::target::Target;
use hopper_core::{HopperError, MomentRange, Tid};
use hopper_index::writes::rerun::{RerunJob, ShardStart, plan_shards, run_rerun_shards};
use hopper_index::{Index, IndexConfig, IndexControl, IndexKind, PhaseA, RecordingMeta};
use hopper_rr::{RecordOptions, ReplayOptions, Rr, RrReplica};
use hopper_symbols::fixtures::{fixture_binary, repo_root};
use hopper_symbols::{Binary, Module, SymbolSet};

fn rr() -> Rr {
    Rr::detect().expect("rr 5.9 or 5.10 on PATH")
}

fn record(rr: &Rr, name: &str) -> (PathBuf, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join(format!("{name}-trace"));
    let exe = fixture_binary(name).expect("fixture");
    let rec = rr
        .record(
            &exe,
            &[],
            &RecordOptions {
                output_dir: Some(trace.clone()),
                ..Default::default()
            },
        )
        .expect("rr record");
    assert!(
        rec.status.success(),
        "{}",
        String::from_utf8_lossy(&rec.stderr)
    );
    (trace, dir)
}

fn replica(rr: &Rr, trace: &Path) -> RrReplica {
    let id = rr.recording_id(trace).unwrap();
    RrReplica::start(rr, trace, &ReplayOptions::default(), id).expect("rr replay")
}

fn symbols_for(t: &mut RrReplica, name: &str) -> (SymbolSet, RecordingMeta) {
    let binary = Binary::open(fixture_binary(name).unwrap()).unwrap();
    let build_ids = binary.build_id().map(|b| b.to_hex()).into_iter().collect();
    let mut s = SymbolSet::new();
    s.add(Module::main_from_auxv(binary, &t.auxv().unwrap()).unwrap());
    (
        s,
        RecordingMeta {
            recording: t.info().recording,
            build_ids,
        },
    )
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

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_phase_a_counts_fib_per_thread() {
    let rr = rr();
    let (trace, _keep) = record(&rr, "threads");
    let mut t = replica(&rr, &trace);
    let (symbols, meta) = symbols_for(&mut t, "threads");
    let _dir = index_dir("rr-threads");
    let index = Index::open(_dir.path(), meta).unwrap();
    let s = PhaseA::new(symbols, IndexConfig::default())
        .run(&mut t, &index, &IndexControl::new())
        .unwrap();
    assert!(s.reached_end);
    let fib = index
        .calls(
            &CallFilter {
                function: Some("threads::fib".into()),
                ..Default::default()
            },
            PageReq::first(1000),
        )
        .unwrap();
    assert_eq!(fib.completeness, Completeness::Complete);
    let mut per: BTreeMap<Tid, u32> = BTreeMap::new();
    for c in &fib.items {
        *per.entry(c.tid).or_default() += 1;
    }
    let mut v: Vec<u32> = per.into_values().collect();
    v.sort();
    assert_eq!(v, vec![25, 41]);
    // Every moment the index stored is a place the replica can seek to, at the function's entry.
    let first = &fib.items[0];
    let mut t2 = replica(&rr, &trace);
    t2.seek(&first.entry).unwrap();
    assert_eq!(t2.now().unwrap(), first.entry);
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_phase_a_resumes_after_cancel() {
    let rr = rr();
    let (trace, _keep) = record(&rr, "threads");
    let mut t = replica(&rr, &trace);
    let (symbols, meta) = symbols_for(&mut t, "threads");
    let dir_guard = index_dir("rr-resume");
    let dir = dir_guard.path().to_path_buf();
    let index = Index::open(&dir, meta.clone()).unwrap();
    let config = IndexConfig {
        flush_every: 4,
        ..IndexConfig::default()
    };
    let control = IndexControl::new();
    // Cancel as soon as the first batch is committed.
    let job = PhaseA::new(symbols.clone(), config.clone());
    let watcher = {
        let control = control.clone();
        let dir = dir.clone();
        std::thread::spawn(move || {
            while std::fs::metadata(dir.join("chunks-calls.bin")).map_or(0, |m| m.len()) == 0 {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            control.cancel();
        })
    };
    let r = job.run(&mut t, &index, &control);
    watcher.join().unwrap();
    assert!(matches!(r, Err(HopperError::Cancelled)), "{r:?}");
    assert!(!index.is_complete(IndexKind::Calls).unwrap());
    drop(index);
    let index = Index::open(&dir, meta).unwrap();
    let mut t2 = replica(&rr, &trace);
    let s = PhaseA::new(symbols, config)
        .run(&mut t2, &index, &IndexControl::new())
        .unwrap();
    assert!(s.reached_end && !s.reused);
    let fib = index
        .calls(
            &CallFilter {
                function: Some("threads::fib".into()),
                ..Default::default()
            },
            PageReq::first(1000),
        )
        .unwrap();
    assert_eq!(fib.total, Some(66));
    assert_eq!(fib.completeness, Completeness::Complete);
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_rerun_moments_match_replica() {
    let rr = rr();
    let (trace, _keep) = record(&rr, "worked-example");
    let mut t = replica(&rr, &trace);
    let (symbols, meta) = symbols_for(&mut t, "worked-example");
    let last = t.last_event().unwrap();
    let _dir = index_dir("rr-rerun");
    let index = Index::open(_dir.path(), meta.clone()).unwrap();
    let job = RerunJob {
        rr: rr.clone(),
        trace_dir: trace.clone(),
        recording: meta.recording,
        process: t.info().process,
        shards: plan_shards(last.saturating_sub(20).max(1), last, 4),
        start: ShardStart::Replay,
        flush_every: 4096,
    };
    let s = run_rerun_shards(&job, &symbols, &index, &IndexControl::new()).unwrap();
    eprintln!(
        "rerun: {} instructions, {} writes, {:.0} instructions/s",
        s.instructions,
        s.writes,
        s.instructions_per_second()
    );
    assert!(s.writes > 0);
    // The first logged write: seeking the replica to its moment lands on its PC.
    let all = index.writes_in(
        hopper_core::Place::memory(t.info().process, 0, u64::MAX),
        MomentRange::all(),
        PageReq::first(1),
    );
    let w = all.unwrap().items.into_iter().next().expect("a write");
    t.seek(&w.at).unwrap();
    let tid = w.at.tid;
    assert_eq!(t.registers(tid).unwrap().pc(), Some(w.pc));
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_phase_a_cheap_moments_seek_to_their_stops() {
    let rr = rr();
    let (trace, _keep) = record(&rr, "threads");
    let mut t = replica(&rr, &trace);
    let (symbols, meta) = symbols_for(&mut t, "threads");
    let dir = index_dir("rr-cheap");
    let index = Index::open(dir.path(), meta).unwrap();
    // Small batches: several flushes, each resolving its batch's visits.
    let config = IndexConfig {
        flush_every: 16,
        ..IndexConfig::default()
    };
    PhaseA::new(symbols.clone(), config)
        .run(&mut t, &index, &IndexControl::new())
        .unwrap();
    let fib = symbols.find_functions("threads::fib")[0].entry;
    let calls = index
        .calls(
            &CallFilter {
                function: Some("threads::fib".into()),
                ..Default::default()
            },
            PageReq::first(u32::MAX),
        )
        .unwrap();
    assert!(calls.items.len() > 20);
    // Every stored moment is exact: seeking there with a fresh replica lands on the stop.
    let mut other = replica(&rr, &trace);
    let mut moments: Vec<_> = calls.items.iter().map(|c| (c.entry, c.tid)).collect();
    moments.sort_by_key(|(m, _)| m.order_key());
    for (m, tid) in moments.iter().step_by(3) {
        other.seek(m).unwrap();
        assert_eq!(other.registers(*tid).unwrap().pc(), Some(fib), "{m}");
        assert_eq!(other.now().unwrap(), *m);
    }
}
