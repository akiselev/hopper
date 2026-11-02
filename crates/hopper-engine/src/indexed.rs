//! The engine's use of hopper-index: phase A runs on a background replica from the moment the
//! session opens, and a query uses the index's answer when the index covers the asked range
//! ([`Completeness::Complete`]); otherwise it replays on demand.

use std::path::PathBuf;
use std::sync::Arc;

use hopper_core::api::{CallFilter, Completeness, PageReq};
use hopper_core::target::Target;
use hopper_core::{Moment, MomentRange, Result, SourceLoc};
use hopper_index::{Index, IndexConfig, IndexControl, PhaseA, RecordingMeta};
use hopper_symbols::SymbolSet;

use crate::Shared;
use crate::ownership::{LineHit, TraceMark};
use crate::queries::FrameKey;

/// Whether a session keeps an index, and where.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum IndexPolicy {
    /// An rr recording's index lives beside its trace, in `<trace>.hopper-index/`; other
    /// sessions keep none.
    #[default]
    BesideTrace,
    /// This directory (any session that can seek: rr, or a replay).
    Dir(PathBuf),
    /// No index: every omniscient query replays on demand.
    Off,
}

/// Why a session has no index.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NoIndex {
    /// The policy or the session kind keeps none (normal; queries replay on demand).
    NotKept(String),
    /// One should have been opened, and opening it failed.
    Failed(String),
}

impl std::fmt::Display for NoIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoIndex::NotKept(why) | NoIndex::Failed(why) => f.write_str(why),
        }
    }
}

/// An open index and its phase-A job's control.
#[derive(Debug, Clone)]
pub struct Indexed {
    pub index: Arc<Index>,
    pub control: IndexControl,
}

impl Indexed {
    pub fn open(dir: &std::path::Path, meta: RecordingMeta) -> Result<Indexed> {
        Ok(Indexed {
            index: Arc::new(Index::open(dir, meta)?),
            control: IndexControl::new(),
        })
    }
}

/// Start phase A on a background replica, in its own thread. It holds one background replica
/// until it finishes or the session cancels it.
pub fn start_phase_a(shared: &Arc<Shared>, ix: &Indexed, main: SymbolSet) -> Result<()> {
    let s = shared.clone();
    let index = ix.index.clone();
    let control = ix.control.clone();
    std::thread::Builder::new()
        .name("hopper-index-phase-a".into())
        .spawn(move || {
            let c = control.clone();
            let run = || -> Result<()> {
                let lease = s.pool.lease()?;
                let start = s.start.at;
                lease.run(move |t: &mut dyn Target| -> Result<()> {
                    if t.now().ok() != Some(start) {
                        t.seek(&start)?;
                    }
                    PhaseA::new(main, IndexConfig::default()).run(t, &index, &c)?;
                    Ok(())
                })?
            };
            if let Err(e) = run() {
                control.finish(Some(e.to_string()));
            }
        })?;
    Ok(())
}

/// A page from the index when it is complete for what was asked.
pub fn complete<T>(page: Result<hopper_core::api::Page<T>>) -> Option<hopper_core::api::Page<T>> {
    page.ok()
        .filter(|p| p.completeness == Completeness::Complete)
}

/// Every page of an index answer, from the start.
fn all<T>(
    mut get: impl FnMut(PageReq) -> Result<hopper_core::api::Page<T>>,
) -> Option<(Vec<T>, Completeness)> {
    let mut out = Vec::new();
    let mut offset = 0u64;
    loop {
        let page = get(PageReq {
            offset,
            limit: 10_000,
        })
        .ok()?;
        let n = page.items.len() as u64;
        let completeness = page.completeness.clone();
        out.extend(page.items);
        if n < 10_000 {
            return Some((out, completeness));
        }
        offset += n;
    }
}

/// The tracepoint hits of frame instance `key` before `at`, from the index's call tree and line
/// hits: the innermost call of the function on that thread that encloses `at`, and the hits of
/// the sidecar's lines in it (outside nested calls of the same function). `None` unless both
/// are complete for that range.
pub fn frame_hits(
    shared: &Shared,
    ix: &Indexed,
    at: Moment,
    key: &FrameKey,
    function: usize,
) -> Option<Vec<LineHit>> {
    let sidecar = shared.sidecar.as_ref()?;
    let record = sidecar.function(function);
    let filter = CallFilter {
        function: Some(record.demangled.clone()),
        tid: Some(key.tid),
        ..CallFilter::default()
    };
    let (calls, c) = all(|p| ix.index.calls(&filter, p))?;
    if c != Completeness::Complete {
        return None;
    }
    let calls: Vec<_> = calls
        .into_iter()
        .filter(|n| n.function == record.demangled)
        .collect();
    let enclosing = calls
        .iter()
        .filter(|n| n.entry.is_before(&at) && n.exit.is_none_or(|x| at.is_before(&x)))
        .max_by(|a, b| a.entry.order_key().cmp(&b.entry.order_key()))?;
    let nested: Vec<(Moment, Option<Moment>)> = calls
        .iter()
        .filter(|n| enclosing.entry.is_before(&n.entry) && n.entry.is_before(&at))
        .map(|n| (n.entry, n.exit))
        .collect();
    let inside_nested = |m: &Moment| {
        nested
            .iter()
            .any(|(e, x)| !m.is_before(e) && x.is_none_or(|x| m.is_before(&x)))
    };
    let mut lines: Vec<u32> = shared
        .sidecar_lines
        .values()
        .filter(|p| p.function == function)
        .filter_map(|p| match p.mark {
            TraceMark::Line(l) => Some(l),
            TraceMark::Entry => None,
        })
        .collect();
    lines.sort_unstable();
    lines.dedup();
    let range = MomentRange::between(enclosing.entry, at);
    let mut hits = vec![LineHit {
        at: enclosing.entry,
        tid: key.tid,
        cfa: key.cfa,
        function,
        mark: TraceMark::Entry,
    }];
    for line in lines {
        let loc = SourceLoc::new(record.def_span.file.clone(), line);
        let (moments, c) = all(|p| ix.index.line_hits(&loc, range, p))?;
        if c != Completeness::Complete {
            return None;
        }
        hits.extend(
            moments
                .into_iter()
                .filter(|m| m.tid == key.tid && !inside_nested(m))
                .map(|m| LineHit {
                    at: m,
                    tid: key.tid,
                    cfa: key.cfa,
                    function,
                    mark: TraceMark::Line(line),
                }),
        );
    }
    hits.sort_by_key(|h| h.at.order_key());
    Some(hits)
}

/// Whether a live session indexes a second run of its program.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum LiveIndexPolicy {
    /// Run the program a second time under its own gdbserver, with the same arguments, and
    /// run phase A on that run in the background. The program's side effects happen twice.
    #[default]
    ReferenceRun,
    /// No reference run: calls and line hits are what the cursor has seen.
    Off,
}

/// The index of a live session's reference run: a second run of the same program, whose call
/// tree, line hits and allocations answer for the live process (which cannot be replayed).
///
/// Its moments are the reference run's own: they carry its recording id
/// ([`hopper_core::api::ReferenceRun::id`]), not the live session's, so a front end can show
/// them as moments of the other run and never seek to them. A deterministic program executes
/// the same calls and lines in both runs, and gdbserver disables address randomisation, so its
/// heap addresses match too; a program that reads input or races threads can differ.
#[derive(Debug)]
pub struct ReferenceIndex {
    pub indexed: Indexed,
    pub run: hopper_core::api::ReferenceRun,
    /// The directory the session made for the index, removed when the session ends.
    temporary: Option<PathBuf>,
}

impl Drop for ReferenceIndex {
    fn drop(&mut self) {
        self.indexed.control.cancel();
        if let Some(d) = &self.temporary {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// Launch the reference run of `program` and start phase A on it in its own thread.
pub fn start_reference_run(
    policy: &IndexPolicy,
    config: &hopper_rr::GdbserverConfig,
    program: &std::path::Path,
    args: &[std::ffi::OsString],
) -> std::result::Result<ReferenceIndex, NoIndex> {
    let failed = |what: &str, e: hopper_core::HopperError| NoIndex::Failed(format!("{what}: {e}"));
    let mut t = hopper_rr::GdbserverTarget::launch(config, program, args)
        .map_err(|e| failed("the reference run did not start", e))?;
    let recording = t.info().recording;
    let binary = hopper_symbols::Binary::open(program).map_err(|e| failed("its binary", e))?;
    let build_ids = binary.build_id().map(|b| b.to_hex()).into_iter().collect();
    let auxv = t.auxv().map_err(|e| failed("its auxv", e))?;
    let module = hopper_symbols::Module::main_from_auxv(binary, &auxv)
        .map_err(|e| failed("its main module", e))?;
    let mut main = SymbolSet::new();
    main.add(module);
    let (dir, temporary) = match policy {
        IndexPolicy::Dir(d) => (d.clone(), None),
        IndexPolicy::BesideTrace | IndexPolicy::Off => {
            let d = std::env::temp_dir().join(format!("hopper-reference-run-{recording}"));
            (d.clone(), Some(d))
        }
    };
    std::fs::create_dir_all(&dir)
        .map_err(|e| NoIndex::Failed(format!("index directory {}: {e}", dir.display())))?;
    let indexed = Indexed::open(
        &dir,
        RecordingMeta {
            recording,
            build_ids,
        },
    )
    .map_err(|e| failed("its index", e))?;
    let (index, control) = (indexed.index.clone(), indexed.control.clone());
    std::thread::Builder::new()
        .name("hopper-index-reference-run".into())
        .spawn(move || {
            if let Err(e) = PhaseA::new(main, IndexConfig::default()).run(&mut t, &index, &control)
            {
                control.finish(Some(e.to_string()));
            }
            // Dropping the target ends the reference run's gdbserver.
            drop(t);
        })
        .map_err(|e| NoIndex::Failed(format!("cannot start the reference run's thread: {e}")))?;
    Ok(ReferenceIndex {
        indexed,
        run: hopper_core::api::ReferenceRun {
            id: recording,
            description: format!(
                "a second gdbserver run of {} with the same arguments",
                program.display()
            ),
        },
        temporary,
    })
}
