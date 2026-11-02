//! Phase A against targets that name moments and threads the way rr does, built around a live
//! gdbserver: breakpoint stops named by region visits (resolved only at each flush), and
//! thread starts and exits seen by comparing thread lists.
//!
//! Every test skips, with a message, when gdbserver or the fixtures are unavailable.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use hopper_core::api::{CallFilter, PageReq, TimelineKind};
use hopper_core::target::{
    Breakpoint, Direction, Resume, StopEvent, StopReason, Target, TargetInfo, ThreadChange,
    ThreadEvent, ThreadEventSource, ThreadEventTime,
};
use hopper_core::{
    CheckpointId, Moment, MomentRange, RegionVisit, RegisterSet, Result, SourceLoc, StopMoment, Tid,
};
use hopper_index::{Index, IndexConfig, IndexControl, PhaseA, RecordingMeta};
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

#[derive(Debug, PartialEq, Eq)]
enum Mode {
    /// Breakpoint stops are named by region visits (the visit number is a private token).
    Visits,
    /// Thread starts and exits come from thread-list comparisons, as on rr.
    ThreadListDiff,
}

/// A gdbserver target that behaves like an rr replica in one respect (`mode`).
#[derive(Debug)]
struct RrLike {
    inner: GdbserverTarget,
    mode: Mode,
    at_breakpoint: bool,
    issued: Vec<(Moment, u64)>,
    resolve_calls: usize,
    resolved: usize,
    known: Option<Vec<Tid>>,
    events: Vec<ThreadEvent>,
}

impl RrLike {
    fn new(inner: GdbserverTarget, mode: Mode) -> RrLike {
        RrLike {
            inner,
            mode,
            at_breakpoint: false,
            issued: Vec::new(),
            resolve_calls: 0,
            resolved: 0,
            known: None,
            events: Vec::new(),
        }
    }
}

impl Target for RrLike {
    fn info(&self) -> &TargetInfo {
        self.inner.info()
    }
    fn threads(&mut self) -> Result<Vec<Tid>> {
        self.inner.threads()
    }
    fn current_thread(&mut self) -> Result<Tid> {
        self.inner.current_thread()
    }
    fn registers(&mut self, tid: Tid) -> Result<RegisterSet> {
        self.inner.registers(tid)
    }
    fn read_memory(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        self.inner.read_memory(addr, buf)
    }
    fn insert(&mut self, bp: Breakpoint) -> Result<()> {
        self.inner.insert(bp)
    }
    fn remove(&mut self, bp: Breakpoint) -> Result<()> {
        self.inner.remove(bp)
    }
    fn resume(&mut self, how: Resume, dir: Direction) -> Result<StopEvent> {
        let ev = self.inner.resume(how, dir)?;
        self.at_breakpoint = matches!(ev.reason, StopReason::Breakpoint { .. });
        let alive = !matches!(
            ev.reason,
            StopReason::Exited { .. } | StopReason::Killed { .. }
        );
        if let (Some(before), true) = (self.known.clone(), alive) {
            let now = self.inner.threads()?;
            let event = |tid, change| ThreadEvent {
                tid,
                change,
                time: ThreadEventTime::ByCurrentStop,
            };
            for t in before.iter().filter(|t| !now.contains(t)) {
                self.events.push(event(*t, ThreadChange::Exited));
            }
            for t in now.iter().filter(|t| !before.contains(t)) {
                self.events.push(event(*t, ThreadChange::Started));
            }
            self.known = Some(now);
        }
        Ok(ev)
    }
    fn now(&mut self) -> Result<Moment> {
        self.inner.now()
    }
    fn seek(&mut self, to: &Moment) -> Result<StopEvent> {
        self.inner.seek(to)
    }
    fn checkpoint(&mut self) -> Result<CheckpointId> {
        self.inner.checkpoint()
    }
    fn restore(&mut self, id: CheckpointId) -> Result<StopEvent> {
        self.inner.restore(id)
    }
    fn auxv(&mut self) -> Result<Vec<u8>> {
        self.inner.auxv()
    }
    fn now_at_stop(&mut self) -> Result<StopMoment> {
        let m = self.inner.now()?;
        if self.mode != Mode::Visits || !self.at_breakpoint {
            return Ok(StopMoment::Exact(m));
        }
        let pc = self.inner.registers(m.tid)?.pc().unwrap();
        self.issued.push((m, pc));
        Ok(StopMoment::Visit(RegionVisit {
            region: Moment { sub: 0, ..m },
            pc,
            visit: (self.issued.len() - 1) as u32,
        }))
    }
    fn resolve_visits(&mut self, visits: &[RegionVisit]) -> Result<Vec<Moment>> {
        self.resolve_calls += 1;
        self.resolved += visits.len();
        Ok(visits
            .iter()
            .map(|v| {
                let (m, pc) = self.issued[v.visit as usize];
                assert_eq!(pc, v.pc);
                assert!(v.same_region(&m));
                m
            })
            .collect())
    }
    fn enable_thread_events(&mut self) -> Result<ThreadEventSource> {
        match self.mode {
            Mode::Visits => self.inner.enable_thread_events(),
            Mode::ThreadListDiff => {
                self.known = Some(self.inner.threads()?);
                Ok(ThreadEventSource::ThreadListDiff)
            }
        }
    }
    fn take_thread_events(&mut self) -> Result<Vec<ThreadEvent>> {
        match self.mode {
            Mode::Visits => self.inner.take_thread_events(),
            Mode::ThreadListDiff => Ok(std::mem::take(&mut self.events)),
        }
    }
}

/// Every moment phase A stored for worked-example, as order keys: line hits of lines 7..=18,
/// call entries and exits, allocations and frees, the timeline.
fn stored_moments(index: &Index) -> Vec<(String, (u64, u64, u32))> {
    let mut out = Vec::new();
    for line in 7..=18 {
        for m in index
            .line_hits(&SourceLoc::new("main.rs", line), MomentRange::all(), all())
            .unwrap()
            .items
        {
            out.push((format!("line {line}"), m.order_key()));
        }
    }
    let calls = index
        .calls(
            &CallFilter {
                function: Some(String::new()),
                ..Default::default()
            },
            all(),
        )
        .unwrap();
    for c in calls.items {
        out.push((format!("enter {}", c.function), c.entry.order_key()));
        if let Some(x) = c.exit {
            out.push((format!("exit {}", c.function), x.order_key()));
        }
    }
    for a in index.allocations(MomentRange::all(), all()).unwrap().items {
        out.push((format!("alloc {}", a.size), a.allocated.order_key()));
        if let Some(f) = a.freed {
            out.push((format!("free {}", a.size), f.order_key()));
        }
    }
    for e in index.timeline(MomentRange::all(), all()).unwrap().items {
        out.push((format!("{:?}", e.kind), e.at.order_key()));
    }
    out
}

#[test]
fn visit_named_stops_are_exact_once_committed() {
    if !gdbserver_available() {
        return;
    }
    let Some(exe) = fixture_binary("worked-example") else {
        return;
    };
    let config = IndexConfig {
        flush_every: 8,
        ..IndexConfig::default()
    };
    // A plain run: every stop measured.
    let (mut t, symbols, meta) = launch(&exe);
    let plain_dir = index_dir("plain");
    let plain = Index::open(plain_dir.path(), meta).unwrap();
    PhaseA::new(symbols.clone(), config.clone())
        .run(&mut t, &plain, &IndexControl::new())
        .unwrap();
    // The same program with breakpoint stops named by visits.
    let (t, symbols, meta) = launch(&exe);
    let mut rr_like = RrLike::new(t, Mode::Visits);
    let cheap_dir = index_dir("cheap");
    let cheap = Index::open(cheap_dir.path(), meta).unwrap();
    let s = PhaseA::new(symbols, config)
        .run(&mut rr_like, &cheap, &IndexControl::new())
        .unwrap();
    assert!(s.reached_end);
    assert!(rr_like.resolve_calls > 2, "resolved at each flush");
    assert_eq!(rr_like.resolved, rr_like.issued.len(), "each visit once");
    assert!(rr_like.issued.len() as u64 >= s.stops / 2);
    let want = stored_moments(&plain);
    assert!(want.len() > 20, "{want:?}");
    assert_eq!(stored_moments(&cheap), want);
}

#[test]
fn thread_list_differences_join_the_thread_entry_heuristic() {
    if !gdbserver_available() {
        return;
    }
    let Some(exe) = fixture_binary("threads") else {
        return;
    };
    let (t, symbols, meta) = launch(&exe);
    let main = Tid(t.info().process.pid);
    let mut rr_like = RrLike::new(t, Mode::ThreadListDiff);
    let dir = index_dir("diff");
    let index = Index::open(dir.path(), meta).unwrap();
    PhaseA::new(symbols, IndexConfig::default())
        .run(&mut rr_like, &index, &IndexControl::new())
        .unwrap();
    let tl = index.timeline(MomentRange::all(), all()).unwrap();
    let mut starts: BTreeMap<Tid, Vec<Moment>> = BTreeMap::new();
    let mut exits: BTreeMap<Tid, Vec<Moment>> = BTreeMap::new();
    for e in &tl.items {
        match e.kind {
            TimelineKind::ThreadStart => starts.entry(e.at.tid).or_default().push(e.at),
            TimelineKind::ThreadExit => exits.entry(e.at.tid).or_default().push(e.at),
            _ => {}
        }
    }
    assert_eq!(starts.len(), 3, "main and two workers: {:?}", tl.items);
    assert!(starts.contains_key(&main));
    for (tid, s) in &starts {
        assert_eq!(s.len(), 1, "{tid} starts once: {:?}", tl.items);
        assert_eq!(exits.get(tid).map(Vec::len), Some(1), "{tid} exits once");
    }
    // A worker starts before its first call and exits after its last.
    let calls = index
        .calls(
            &CallFilter {
                function: Some("threads::".into()),
                ..Default::default()
            },
            all(),
        )
        .unwrap();
    let workers: HashSet<Tid> = starts.keys().copied().filter(|t| *t != main).collect();
    for c in calls.items.iter().filter(|c| workers.contains(&c.tid)) {
        assert!(!c.entry.is_before(&starts[&c.tid][0]), "{c:?}");
        if let Some(x) = c.exit {
            assert!(!exits[&c.tid][0].is_before(&x), "{c:?}");
        }
    }
}
