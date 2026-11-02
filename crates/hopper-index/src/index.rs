//! The query side: an open index directory, shared by the job that builds it and the engine
//! that asks it.
//!
//! **Memory.** Line hits are not held in memory: the catalogue names the line ids in each
//! chunk, so a line query decodes only the chunks that hold that line inside the asked range
//! (through a small cache of recently decoded chunks). The call tree, the allocations, the
//! timeline and the write log are folded into memory caches as chunks arrive; allocation
//! queries go through an interval index over those rows ([`AllocIntervals`]).

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

use hopper_core::api::{
    AllocationId, AllocationInfo, CallFilter, CallId, CallNode, Completeness, Page, PageReq,
    TimelineEvent, TimelineKind,
};
use hopper_core::{
    HopperError, Moment, MomentRange, Place, ProcessId, RecordingId, Result, SourceLoc,
};
use serde::{Deserialize, Serialize};

use crate::store::{Batch, CapKey, ChunkEntry, Coverage, Record, Stamp, Store};
use crate::writes::{LoggedWrite, WriteValue};

/// The kinds of index data; each has its own chunk stream and coverage row.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum IndexKind {
    Calls,
    Lines,
    Allocations,
    Timeline,
    Writes,
}

impl IndexKind {
    pub const ALL: [IndexKind; 5] = [
        IndexKind::Calls,
        IndexKind::Lines,
        IndexKind::Allocations,
        IndexKind::Timeline,
        IndexKind::Writes,
    ];

    /// The kinds phase A builds.
    pub const PHASE_A: [IndexKind; 4] = [
        IndexKind::Calls,
        IndexKind::Lines,
        IndexKind::Allocations,
        IndexKind::Timeline,
    ];

    /// The byte stored in chunk headers and catalogue keys.
    pub fn code(self) -> u8 {
        match self {
            IndexKind::Calls => 1,
            IndexKind::Lines => 2,
            IndexKind::Allocations => 3,
            IndexKind::Timeline => 4,
            IndexKind::Writes => 5,
        }
    }

    pub fn from_code(code: u8) -> Option<IndexKind> {
        IndexKind::ALL.into_iter().find(|k| k.code() == code)
    }

    /// The name used in file names and progress phases.
    pub fn name(self) -> &'static str {
        match self {
            IndexKind::Calls => "calls",
            IndexKind::Lines => "lines",
            IndexKind::Allocations => "allocations",
            IndexKind::Timeline => "timeline",
            IndexKind::Writes => "writes",
        }
    }
}

/// What an index is for: one recording and the build ids of its binaries (lowercase hex, in
/// the order the engine lists them). Opening an index for different metadata fails.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordingMeta {
    pub recording: RecordingId,
    pub build_ids: Vec<String>,
}

/// A single answer and how complete it is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer<T> {
    pub value: T,
    pub completeness: Completeness,
}

#[derive(Clone, Debug)]
struct CallRow {
    id: u64,
    fid: u32,
    parent: Option<u64>,
    process: ProcessId,
    entry: Stamp,
    exit: Option<Stamp>,
    depth: u32,
}

#[derive(Debug, Default)]
struct CallCache {
    functions: HashMap<u32, (String, Option<SourceLoc>)>,
    rows: Vec<CallRow>,
    by_id: HashMap<u64, usize>,
    children: HashMap<u64, Vec<u64>>,
    roots: Vec<u64>,
}

#[derive(Debug, Default)]
struct LineCache {
    /// The line dictionary, as of `dictionary_chunks` line chunks.
    lines: HashMap<u32, SourceLoc>,
    dictionary_chunks: Option<usize>,
}

/// A decoded chunk, by (kind, sequence number).
type DecodedChunk = ((IndexKind, u64), Arc<Vec<Record>>);

/// Recently decoded chunks, the most recently used last.
#[derive(Debug)]
struct ChunkCache {
    entries: VecDeque<DecodedChunk>,
    capacity: usize,
}

impl Default for ChunkCache {
    fn default() -> Self {
        ChunkCache {
            entries: VecDeque::new(),
            capacity: 32,
        }
    }
}

impl ChunkCache {
    fn get(&mut self, store: &Store, entry: &ChunkEntry) -> Result<Arc<Vec<Record>>> {
        let key = (entry.kind, entry.seq);
        if let Some(i) = self.entries.iter().position(|(k, _)| *k == key) {
            let hit = self.entries.remove(i).expect("found");
            let records = hit.1.clone();
            self.entries.push_back(hit);
            return Ok(records);
        }
        let records = Arc::new(store.read_chunk(entry)?);
        if self.entries.len() >= self.capacity {
            self.entries.pop_front();
        }
        self.entries.push_back((key, records.clone()));
        Ok(records)
    }
}

/// A max segment tree over a fixed sequence, for "every index in a range whose value is above
/// x" in O((k + 1) log n).
#[derive(Debug, Clone)]
struct MaxTree<T> {
    len: usize,
    size: usize,
    nodes: Vec<T>,
}

impl<T: Copy + Ord> MaxTree<T> {
    fn new(values: &[T], least: T) -> MaxTree<T> {
        let size = values.len().next_power_of_two().max(1);
        let mut nodes = vec![least; 2 * size];
        nodes[size..size + values.len()].copy_from_slice(values);
        for i in (1..size).rev() {
            nodes[i] = nodes[2 * i].max(nodes[2 * i + 1]);
        }
        MaxTree {
            len: values.len(),
            size,
            nodes,
        }
    }

    /// Call `f` with every index in `lo..hi` whose value is greater than `x`, highest index
    /// first, until `f` returns `false`. Returns `false` when `f` stopped the walk.
    fn above_desc(&self, lo: usize, hi: usize, x: T, f: &mut impl FnMut(usize) -> bool) -> bool {
        let hi = hi.min(self.len);
        if lo >= hi {
            return true;
        }
        self.walk(1, 0, self.size, lo, hi, x, f)
    }

    #[allow(clippy::too_many_arguments, reason = "the recursion's state")]
    fn walk(
        &self,
        node: usize,
        from: usize,
        to: usize,
        lo: usize,
        hi: usize,
        x: T,
        f: &mut impl FnMut(usize) -> bool,
    ) -> bool {
        if to <= lo || hi <= from || self.nodes[node] <= x {
            return true;
        }
        if to - from == 1 {
            return f(from);
        }
        let mid = (from + to) / 2;
        self.walk(2 * node + 1, mid, to, lo, hi, x, f)
            && self.walk(2 * node, from, mid, lo, hi, x, f)
    }
}

type Key = (u64, u64, u32);
/// Later than every moment: the free time of a block that is still live.
const NEVER: Key = (u64::MAX, u64::MAX, u32::MAX);

/// The blocks of one process that start at one address, in allocation order.
#[derive(Debug, Clone)]
struct BlockGroup {
    process: ProcessId,
    start: u64,
    /// Rows (indices into `AllocCache::rows`) sorted by allocation moment.
    rows: Vec<usize>,
    allocated: Vec<Key>,
    /// The free moments of `rows`, for "which of them is live at t".
    freed: MaxTree<Key>,
}

/// An interval index over the allocation rows: blocks grouped by (process, start address),
/// with a max tree over each group's furthest end (which groups can contain an address) and,
/// in each group, one over the free moments (which of its blocks is live at a moment).
#[derive(Debug, Clone)]
struct AllocIntervals {
    groups: Vec<BlockGroup>,
    ends: MaxTree<u64>,
}

impl AllocIntervals {
    fn build(rows: &[AllocRow]) -> AllocIntervals {
        let mut order: Vec<usize> = (0..rows.len()).collect();
        order.sort_by_key(|&i| (rows[i].process, rows[i].addr, rows[i].allocated.key(), i));
        let mut groups: Vec<BlockGroup> = Vec::new();
        let mut ends: Vec<u64> = Vec::new();
        let mut i = 0;
        while i < order.len() {
            let first = &rows[order[i]];
            let j = order[i..]
                .iter()
                .position(|&k| rows[k].process != first.process || rows[k].addr != first.addr)
                .map_or(order.len(), |n| i + n);
            let members = order[i..j].to_vec();
            let allocated: Vec<Key> = members.iter().map(|&k| rows[k].allocated.key()).collect();
            let freed: Vec<Key> = members
                .iter()
                .map(|&k| rows[k].freed.map_or(NEVER, |f| f.key()))
                .collect();
            ends.push(
                members
                    .iter()
                    .map(|&k| rows[k].addr.saturating_add(rows[k].size.max(1)))
                    .max()
                    .unwrap_or(0),
            );
            groups.push(BlockGroup {
                process: first.process,
                start: first.addr,
                rows: members,
                allocated,
                freed: MaxTree::new(&freed, (0, 0, 0)),
            });
            i = j;
        }
        AllocIntervals {
            ends: MaxTree::new(&ends, 0),
            groups,
        }
    }

    /// The latest-allocated row containing `addr` in `process` that is allocated at or before
    /// `at` and not freed by then.
    fn live_at(&self, rows: &[AllocRow], process: ProcessId, addr: u64, at: Key) -> Option<usize> {
        let lo = self.groups.partition_point(|g| g.process < process);
        let hi = self
            .groups
            .partition_point(|g| (g.process, g.start) <= (process, addr));
        let mut best: Option<usize> = None;
        self.ends.above_desc(lo, hi, addr, &mut |gi| {
            let g = &self.groups[gi];
            // Blocks allocated at or before `at`, and of those the ones not freed by `at`.
            let n = g.allocated.partition_point(|k| *k <= at);
            g.freed.above_desc(0, n, at, &mut |m| {
                let r = g.rows[m];
                if addr < rows[r].addr.saturating_add(rows[r].size.max(1)) {
                    if best.is_none_or(|b| r > b) {
                        best = Some(r);
                    }
                    return false;
                }
                true
            });
            true
        });
        best
    }
}

#[derive(Clone, Debug)]
struct AllocRow {
    info_id: u64,
    process: ProcessId,
    addr: u64,
    size: u64,
    allocated: Stamp,
    freed: Option<Stamp>,
    predecessor: Option<u64>,
    requested_by: Option<String>,
}

#[derive(Debug, Default)]
struct AllocCache {
    rows: Vec<AllocRow>,
    by_id: HashMap<u64, usize>,
    /// Built on the first query after the rows changed.
    intervals: Option<AllocIntervals>,
}

#[derive(Debug, Default)]
struct Loaded {
    /// Chunks of each kind already folded into the caches.
    chunks: HashMap<IndexKind, u64>,
    calls: CallCache,
    lines: LineCache,
    chunk_cache: ChunkCache,
    allocs: AllocCache,
    timeline: Vec<(Stamp, TimelineKind)>,
    writes: Vec<(Stamp, u64, Place, WriteValue)>,
}

#[derive(Debug)]
struct Inner {
    store: Store,
    loaded: Loaded,
}

/// An open index. `Send + Sync`: phase A, phase B and any number of query threads share one
/// `Index` (in an `Arc`). Queries see what has been committed so far.
#[derive(Debug)]
pub struct Index {
    inner: Mutex<Inner>,
    recording: RecordingId,
}

fn page_of<T>(items: Vec<T>, page: PageReq, completeness: Completeness) -> Page<T> {
    let total = items.len() as u64;
    let items = items
        .into_iter()
        .skip(page.offset.min(usize::MAX as u64) as usize)
        .take(page.limit as usize)
        .collect();
    Page {
        items,
        total: Some(total),
        completeness,
    }
}

fn completeness_of(pending: Vec<MomentRange>) -> Completeness {
    if pending.is_empty() {
        Completeness::Complete
    } else {
        Completeness::Partial { pending }
    }
}

impl Index {
    /// Open (or create) the index in `dir`. See [`Store::open`] for what is refused.
    pub fn open(dir: &Path, meta: RecordingMeta) -> Result<Index> {
        let recording = meta.recording;
        let store = Store::open(dir, &meta)?;
        Ok(Index {
            inner: Mutex::new(Inner {
                store,
                loaded: Loaded::default(),
            }),
            recording,
        })
    }

    pub fn recording(&self) -> RecordingId {
        self.recording
    }

    pub fn dir(&self) -> PathBuf {
        self.lock().store.dir().to_path_buf()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// What `kind` has covered so far.
    pub fn coverage(&self, kind: IndexKind) -> Result<Coverage> {
        self.lock().store.coverage(kind)
    }

    /// Whether `kind` covers the whole recording with nothing capped.
    pub fn is_complete(&self, kind: IndexKind) -> Result<bool> {
        let c = self.coverage(kind)?;
        Ok(c.capped.is_empty() && c.pending(&MomentRange::all(), self.recording).is_empty())
    }

    pub(crate) fn commit(&self, batch: Batch) -> Result<()> {
        self.lock().store.commit(batch).map(|_| ())
    }

    pub(crate) fn resume_state(&self) -> Result<Option<Vec<u8>>> {
        self.lock().store.resume_state()
    }

    pub(crate) fn clear(&self, kinds: &[IndexKind]) -> Result<()> {
        let mut g = self.lock();
        g.store.clear(kinds)?;
        g.loaded = Loaded::default();
        Ok(())
    }

    fn check(&self, m: &Moment) -> Result<()> {
        if m.recording != self.recording {
            return Err(HopperError::BadMoment(format!(
                "moment {m} belongs to recording {}, the index is for {}",
                m.recording, self.recording
            )));
        }
        Ok(())
    }

    fn check_range(&self, r: &MomentRange) -> Result<()> {
        if let Some(s) = &r.start {
            self.check(s)?;
        }
        if let Some(e) = &r.end {
            self.check(e)?;
        }
        Ok(())
    }

    fn moment(&self, s: Stamp) -> Moment {
        s.moment(self.recording)
    }

    /// Fold chunks committed since the last query into the caches.
    fn refresh(inner: &mut Inner, kind: IndexKind) -> Result<()> {
        let have = inner.loaded.chunks.get(&kind).copied().unwrap_or(0);
        let entries = inner.store.chunks(kind)?;
        for e in entries.iter().filter(|e| e.seq >= have) {
            let records = inner.store.read_chunk(e)?;
            for r in records {
                fold(&mut inner.loaded, r);
            }
        }
        if let Some(last) = entries.last() {
            inner.loaded.chunks.insert(kind, last.seq + 1);
        }
        Ok(())
    }

    fn pending_with_caps(
        &self,
        cov: &Coverage,
        range: &MomentRange,
        cap_matches: impl Fn(&CapKey) -> bool,
    ) -> Completeness {
        let mut pending = cov.pending(range, self.recording);
        for c in cov.capped.iter().filter(|c| cap_matches(&c.key)) {
            let from = self.moment(c.from);
            if range.end.is_none_or(|e| from.is_before(&e)) {
                let start = match range.start {
                    Some(s) if from.is_before(&s) => Some(s),
                    _ => Some(from),
                };
                pending.push(MomentRange {
                    start,
                    end: range.end,
                });
            }
        }
        completeness_of(pending)
    }

    fn call_node(&self, cache: &CallCache, row: &CallRow) -> CallNode {
        let (function, location) = cache
            .functions
            .get(&row.fid)
            .cloned()
            .unwrap_or_else(|| (format!("<function {}>", row.fid), None));
        CallNode {
            id: CallId(row.id),
            parent: row.parent.map(CallId),
            tid: hopper_core::Tid(row.entry.1),
            function,
            entry: self.moment(row.entry),
            exit: row.exit.map(|s| self.moment(s)),
            depth: row.depth,
            child_count: cache.children.get(&row.id).map_or(0, |c| c.len() as u64),
            location,
        }
    }

    /// The call tree, as [`hopper_core::api::Debugger::calls`] defines it: the children of
    /// `filter.parent`; with no parent, the roots, unless `filter.function` is set, which then
    /// searches every call. Calls are in entry order.
    pub fn calls(&self, filter: &CallFilter, page: PageReq) -> Result<Page<CallNode>> {
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Calls)?;
        let cov = g.store.coverage(IndexKind::Calls)?;
        let cache = &g.loaded.calls;
        let ids: Vec<u64> = match (filter.parent, &filter.function) {
            (Some(p), _) => cache.children.get(&p.0).cloned().unwrap_or_default(),
            (None, Some(_)) => cache.rows.iter().map(|r| r.id).collect(),
            (None, None) => cache.roots.clone(),
        };
        let wanted = |r: &CallRow| {
            filter.tid.is_none_or(|t| t.0 == r.entry.1)
                && filter.process.is_none_or(|p| p == r.process)
                && filter.function.as_ref().is_none_or(|f| {
                    cache
                        .functions
                        .get(&r.fid)
                        .is_some_and(|(n, _)| n.contains(f.as_str()))
                })
        };
        let rows: Vec<&CallRow> = ids
            .iter()
            .filter_map(|id| cache.by_id.get(id).map(|&i| &cache.rows[i]))
            .filter(|r| wanted(r))
            .collect();
        let range = match filter.parent.and_then(|p| cache.by_id.get(&p.0)) {
            Some(&i) => {
                let p = &cache.rows[i];
                MomentRange {
                    start: Some(self.moment(p.entry)),
                    end: p.exit.map(|s| self.moment(s)),
                }
            }
            None => MomentRange::all(),
        };
        let completeness = self.pending_with_caps(&cov, &range, |k| match (k, &filter.function) {
            (CapKey::Function(name), Some(f)) => name.contains(f.as_str()),
            (CapKey::Function(_), None) => true,
            _ => false,
        });
        let nodes: Vec<CallNode> = rows.into_iter().map(|r| self.call_node(cache, r)).collect();
        Ok(page_of(nodes, page, completeness))
    }

    /// One call by id.
    pub fn call(&self, id: CallId) -> Result<Option<CallNode>> {
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Calls)?;
        let cache = &g.loaded.calls;
        Ok(cache
            .by_id
            .get(&id.0)
            .map(|&i| self.call_node(cache, &cache.rows[i])))
    }

    /// Every moment execution reached a statement of `loc` (file matched by path suffix, the
    /// line exactly; the column is ignored) inside `range`, in time order.
    pub fn line_hits(
        &self,
        loc: &SourceLoc,
        range: MomentRange,
        page: PageReq,
    ) -> Result<Page<Moment>> {
        self.check_range(&range)?;
        let mut g = self.lock();
        let inner = &mut *g;
        let entries = inner.store.chunks(IndexKind::Lines)?;
        if inner.loaded.lines.dictionary_chunks != Some(entries.len()) {
            inner.loaded.lines.lines = inner.store.line_dictionary()?.into_iter().collect();
            inner.loaded.lines.dictionary_chunks = Some(entries.len());
        }
        let cov = inner.store.coverage(IndexKind::Lines)?;
        let mut lids: Vec<u32> = inner
            .loaded
            .lines
            .lines
            .iter()
            .filter(|(_, l)| l.line == loc.line && l.file_matches(&loc.file))
            .map(|(id, _)| *id)
            .collect();
        lids.sort_unstable();
        let mut stamps: Vec<Stamp> = Vec::new();
        if !lids.is_empty() {
            for e in entries
                .iter()
                .filter(|e| e.overlaps(&range) && e.has_any_key(&lids))
            {
                let records = inner.loaded.chunk_cache.get(&inner.store, e)?;
                stamps.extend(records.iter().filter_map(|r| match r {
                    Record::LineHit { lid, at }
                        if lids.binary_search(lid).is_ok() && range.contains(&self.moment(*at)) =>
                    {
                        Some(*at)
                    }
                    _ => None,
                }));
            }
        }
        stamps.sort_by_key(|s| s.key());
        stamps.dedup();
        let completeness = self.pending_with_caps(
            &cov,
            &range,
            |k| matches!(k, CapKey::Line(l) if l.line == loc.line && l.file_matches(&loc.file)),
        );
        let moments = stamps.into_iter().map(|s| self.moment(s)).collect();
        Ok(page_of(moments, page, completeness))
    }

    fn alloc_info(&self, row: &AllocRow) -> AllocationInfo {
        AllocationInfo {
            id: AllocationId(row.info_id),
            process: row.process,
            addr: row.addr,
            size: row.size,
            allocated: self.moment(row.allocated),
            freed: row.freed.map(|s| self.moment(s)),
            predecessor: row.predecessor.map(AllocationId),
            requested_by: row.requested_by.clone(),
        }
    }

    /// The heap block containing `addr` in `process` at moment `at`: allocated at or before
    /// `at` and not yet freed. A zero-sized block contains only its own address.
    pub fn allocation_at(
        &self,
        process: ProcessId,
        addr: u64,
        at: Moment,
    ) -> Result<Answer<Option<AllocationInfo>>> {
        self.check(&at)?;
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Allocations)?;
        let cov = g.store.coverage(IndexKind::Allocations)?;
        let allocs = &mut g.loaded.allocs;
        let intervals = allocs
            .intervals
            .get_or_insert_with(|| AllocIntervals::build(&allocs.rows));
        let found = intervals.live_at(&allocs.rows, process, addr, at.order_key());
        let value = found.map(|i| self.alloc_info(&g.loaded.allocs.rows[i]));
        let range = MomentRange {
            start: None,
            end: Some(at),
        };
        let completeness = match &value {
            // Only a free between allocation and `at` could change a found answer.
            Some(info) => self.pending_with_caps(
                &cov,
                &MomentRange {
                    start: Some(info.allocated),
                    end: Some(at),
                },
                |k| matches!(k, CapKey::Allocator(_)),
            ),
            None => self.pending_with_caps(&cov, &range, |k| matches!(k, CapKey::Allocator(_))),
        };
        Ok(Answer {
            value,
            completeness,
        })
    }

    /// One allocation by id.
    pub fn allocation(&self, id: AllocationId) -> Result<Option<AllocationInfo>> {
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Allocations)?;
        let a = &g.loaded.allocs;
        Ok(a.by_id.get(&id.0).map(|&i| self.alloc_info(&a.rows[i])))
    }

    /// Allocations made inside `range`, in time order.
    pub fn allocations(&self, range: MomentRange, page: PageReq) -> Result<Page<AllocationInfo>> {
        self.check_range(&range)?;
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Allocations)?;
        let cov = g.store.coverage(IndexKind::Allocations)?;
        let mut rows: Vec<&AllocRow> = g
            .loaded
            .allocs
            .rows
            .iter()
            .filter(|r| range.contains(&self.moment(r.allocated)))
            .collect();
        rows.sort_by_key(|r| r.allocated.key());
        let items = rows.into_iter().map(|r| self.alloc_info(r)).collect();
        let completeness =
            self.pending_with_caps(&cov, &range, |k| matches!(k, CapKey::Allocator(_)));
        Ok(page_of(items, page, completeness))
    }

    /// Timeline events inside `range`, in time order.
    pub fn timeline(&self, range: MomentRange, page: PageReq) -> Result<Page<TimelineEvent>> {
        self.check_range(&range)?;
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Timeline)?;
        let cov = g.store.coverage(IndexKind::Timeline)?;
        let mut evs: Vec<TimelineEvent> = g
            .loaded
            .timeline
            .iter()
            .filter(|(s, _)| range.contains(&self.moment(*s)))
            .map(|(s, k)| TimelineEvent {
                at: self.moment(*s),
                kind: k.clone(),
            })
            .collect();
        evs.sort_by_key(|e| e.at.order_key());
        let completeness = self.pending_with_caps(&cov, &range, |k| matches!(k, CapKey::Anchor(_)));
        Ok(page_of(evs, page, completeness))
    }

    fn logged(&self, w: &(Stamp, u64, Place, WriteValue)) -> LoggedWrite {
        LoggedWrite {
            at: self.moment(w.0),
            pc: w.1,
            place: w.2,
            value: w.3.clone(),
        }
    }

    /// The last logged write to any byte of `place` strictly before `before`. Complete only
    /// when phase-B windows covering every thread span from that write (or the recording start,
    /// when none was found) to `before`.
    pub fn last_write(&self, place: Place, before: Moment) -> Result<Answer<Option<LoggedWrite>>> {
        self.check(&before)?;
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Writes)?;
        let cov = g.store.coverage(IndexKind::Writes)?;
        let key = before.order_key();
        let found = g
            .loaded
            .writes
            .iter()
            .filter(|w| w.0.key() < key && w.2.overlaps(&place))
            .max_by_key(|w| w.0.key())
            .map(|w| self.logged(w));
        let range = MomentRange {
            start: found.as_ref().map(|w| w.at),
            end: Some(before),
        };
        let completeness = completeness_of(cov.pending(&range, self.recording));
        Ok(Answer {
            value: found,
            completeness,
        })
    }

    /// Every logged write to any byte of `place` inside `range`, in time order.
    pub fn writes_in(
        &self,
        place: Place,
        range: MomentRange,
        page: PageReq,
    ) -> Result<Page<LoggedWrite>> {
        self.check_range(&range)?;
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Writes)?;
        let cov = g.store.coverage(IndexKind::Writes)?;
        let mut ws: Vec<LoggedWrite> = g
            .loaded
            .writes
            .iter()
            .filter(|w| w.2.overlaps(&place) && range.contains(&self.moment(w.0)))
            .map(|w| self.logged(w))
            .collect();
        ws.sort_by_key(|w| w.at.order_key());
        let completeness = completeness_of(cov.pending(&range, self.recording));
        Ok(page_of(ws, page, completeness))
    }

    /// The function names the call tree knows, for completion and filters.
    pub fn function_names(&self) -> Result<Vec<String>> {
        let mut g = self.lock();
        Self::refresh(&mut g, IndexKind::Calls)?;
        let mut names: Vec<String> = g
            .loaded
            .calls
            .functions
            .values()
            .map(|(n, _)| n.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        names.sort();
        Ok(names)
    }
}

fn fold(l: &mut Loaded, r: Record) {
    match r {
        Record::Function {
            fid,
            name,
            location,
        } => {
            l.calls.functions.insert(fid, (name, location));
        }
        Record::CallEnter {
            id,
            fid,
            parent,
            process,
            at,
            depth,
        } => {
            let c = &mut l.calls;
            c.by_id.insert(id, c.rows.len());
            c.rows.push(CallRow {
                id,
                fid,
                parent,
                process,
                entry: at,
                exit: None,
                depth,
            });
            match parent {
                Some(p) => c.children.entry(p).or_default().push(id),
                None => c.roots.push(id),
            }
        }
        Record::CallExit { id, at } => {
            if let Some(&i) = l.calls.by_id.get(&id) {
                l.calls.rows[i].exit = Some(at);
            }
        }
        // Line records stay in their chunks (see the module docs).
        Record::Line { .. } | Record::LineHit { .. } => {}
        Record::Allocated {
            id,
            process,
            at,
            addr,
            size,
            predecessor,
            requested_by,
            ..
        } => {
            let a = &mut l.allocs;
            a.intervals = None;
            a.by_id.insert(id, a.rows.len());
            a.rows.push(AllocRow {
                info_id: id,
                process,
                addr,
                size,
                allocated: at,
                freed: None,
                predecessor,
                requested_by,
            });
        }
        Record::Freed { id, at } => {
            if let Some(&i) = l.allocs.by_id.get(&id) {
                l.allocs.rows[i].freed = Some(at);
                l.allocs.intervals = None;
            }
        }
        Record::Timeline { at, kind } => l.timeline.push((at, kind)),
        Record::Write {
            at,
            pc,
            place,
            value,
        } => l.writes.push((at, pc, place, value)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::phase_a::AllocatorFunction;
    use crate::store::{Capped, CoverageScope, CoveredInterval};
    use hopper_core::Tid;

    const REC: RecordingId = RecordingId([9; 16]);
    const P: ProcessId = ProcessId {
        pid: 7,
        exec_generation: 0,
    };

    fn s(ft: u64) -> Stamp {
        Stamp(ft, 1, 0, 0)
    }

    fn m(ft: u64) -> Moment {
        s(ft).moment(REC)
    }

    fn open() -> (tempfile::TempDir, Index) {
        let tmp = tempfile::tempdir().unwrap();
        let ix = Index::open(
            tmp.path(),
            RecordingMeta {
                recording: REC,
                build_ids: vec![],
            },
        )
        .unwrap();
        (tmp, ix)
    }

    fn cov(through: Option<u64>, capped: Vec<Capped>) -> Coverage {
        Coverage {
            intervals: vec![CoveredInterval {
                from: None,
                through: through.map(s),
                scope: CoverageScope::AllThreads,
            }],
            capped,
        }
    }

    fn all() -> PageReq {
        PageReq::first(100)
    }

    #[test]
    fn call_tree_queries_follow_the_debugger_semantics() {
        let (_t, ix) = open();
        let enter = |id, fid, parent, at, depth| Record::CallEnter {
            id,
            fid,
            parent,
            process: P,
            at: s(at),
            depth,
        };
        ix.commit(Batch {
            chunks: vec![(
                IndexKind::Calls,
                vec![
                    Record::Function {
                        fid: 0,
                        name: "app::main".into(),
                        location: None,
                    },
                    Record::Function {
                        fid: 1,
                        name: "app::fib".into(),
                        location: Some(SourceLoc::new("main.rs", 3)),
                    },
                    enter(0, 0, None, 1, 0),
                    enter(1, 1, Some(0), 2, 1),
                    enter(2, 1, Some(1), 3, 2),
                    Record::CallExit { id: 2, at: s(4) },
                    Record::CallExit { id: 1, at: s(5) },
                ],
            )],
            coverage: vec![(IndexKind::Calls, cov(Some(5), vec![]))],
            resume: None,
        })
        .unwrap();
        let roots = ix.calls(&CallFilter::default(), all()).unwrap();
        assert_eq!(roots.items.len(), 1);
        assert_eq!(roots.items[0].child_count, 1);
        assert_eq!(roots.items[0].exit, None);
        // Main has not returned and coverage ends at 5: partial.
        assert!(matches!(roots.completeness, Completeness::Partial { .. }));
        let fibs = ix
            .calls(
                &CallFilter {
                    function: Some("fib".into()),
                    ..Default::default()
                },
                all(),
            )
            .unwrap();
        assert_eq!(fibs.total, Some(2));
        assert_eq!(fibs.items[1].depth, 2);
        assert_eq!(fibs.items[1].location, Some(SourceLoc::new("main.rs", 3)));
        let kids = ix
            .calls(
                &CallFilter {
                    parent: Some(CallId(1)),
                    ..Default::default()
                },
                all(),
            )
            .unwrap();
        assert_eq!(
            kids.items.iter().map(|c| c.id).collect::<Vec<_>>(),
            vec![CallId(2)]
        );
        // The children of a call that returned inside the covered interval are complete.
        assert_eq!(kids.completeness, Completeness::Complete);
        let paged = ix
            .calls(
                &CallFilter {
                    function: Some("fib".into()),
                    ..Default::default()
                },
                PageReq {
                    offset: 1,
                    limit: 5,
                },
            )
            .unwrap();
        assert_eq!((paged.items.len(), paged.total), (1, Some(2)));
        let other_tid = ix
            .calls(
                &CallFilter {
                    function: Some("fib".into()),
                    tid: Some(Tid(2)),
                    ..Default::default()
                },
                all(),
            )
            .unwrap();
        assert!(other_tid.items.is_empty());
        assert_eq!(ix.call(CallId(2)).unwrap().unwrap().exit, Some(m(4)));
        assert_eq!(
            ix.function_names().unwrap(),
            vec!["app::fib".to_string(), "app::main".to_string()]
        );
    }

    #[test]
    fn caps_and_coverage_make_answers_partial() {
        let (_t, ix) = open();
        let loc = SourceLoc::new("/p/src/main.rs", 12);
        ix.commit(Batch {
            chunks: vec![(
                IndexKind::Lines,
                vec![
                    Record::Line {
                        lid: 0,
                        location: loc.clone(),
                    },
                    Record::LineHit { lid: 0, at: s(2) },
                    Record::LineHit { lid: 0, at: s(6) },
                ],
            )],
            coverage: vec![(
                IndexKind::Lines,
                cov(
                    None,
                    vec![Capped {
                        key: CapKey::Line(loc.clone()),
                        from: s(6),
                    }],
                ),
            )],
            resume: None,
        })
        .unwrap();
        let q = SourceLoc::new("main.rs", 12);
        let early = ix
            .line_hits(&q, MomentRange::between(m(0), m(5)), all())
            .unwrap();
        assert_eq!(early.items, vec![m(2)]);
        assert_eq!(early.completeness, Completeness::Complete);
        let late = ix.line_hits(&q, MomentRange::all(), all()).unwrap();
        assert_eq!(late.items, vec![m(2), m(6)]);
        assert_eq!(
            late.completeness,
            Completeness::Partial {
                pending: vec![MomentRange {
                    start: Some(m(6)),
                    end: None
                }]
            }
        );
        // Other lines are not affected by the cap.
        let other = ix
            .line_hits(&SourceLoc::new("main.rs", 13), MomentRange::all(), all())
            .unwrap();
        assert_eq!(other.completeness, Completeness::Complete);
        let foreign = s(0).moment(RecordingId([1; 16]));
        assert!(
            ix.line_hits(&q, MomentRange::between(foreign, m(1)), all())
                .is_err()
        );
    }

    #[test]
    fn allocations_and_lineage() {
        let (_t, ix) = open();
        let alloc = |id, at, addr, size, predecessor| Record::Allocated {
            id,
            process: P,
            at: s(at),
            addr,
            size,
            predecessor,
            requested_by: Some("app::main".into()),
            allocator: AllocatorFunction::RustAlloc,
        };
        ix.commit(Batch {
            chunks: vec![(
                IndexKind::Allocations,
                vec![
                    alloc(0, 1, 0x1000, 16, None),
                    Record::Freed { id: 0, at: s(5) },
                    alloc(1, 5, 0x2000, 64, Some(0)),
                    alloc(2, 6, 0x1000, 8, None),
                ],
            )],
            coverage: vec![(IndexKind::Allocations, cov(None, vec![]))],
            resume: None,
        })
        .unwrap();
        let a = ix.allocation_at(P, 0x100f, m(3)).unwrap();
        assert_eq!(a.value.as_ref().map(|a| a.id), Some(AllocationId(0)));
        assert_eq!(a.completeness, Completeness::Complete);
        assert!(ix.allocation_at(P, 0x1010, m(3)).unwrap().value.is_none());
        // Freed at 5: gone at 5, and the same address is a new block from 6.
        assert!(ix.allocation_at(P, 0x1000, m(5)).unwrap().value.is_none());
        assert_eq!(
            ix.allocation_at(P, 0x1000, m(7))
                .unwrap()
                .value
                .map(|a| a.id),
            Some(AllocationId(2))
        );
        let moved = ix.allocation(AllocationId(1)).unwrap().unwrap();
        assert_eq!(moved.predecessor, Some(AllocationId(0)));
        assert_eq!(
            ix.allocation(AllocationId(0)).unwrap().unwrap().freed,
            Some(m(5))
        );
        assert!(
            ix.allocation_at(ProcessId::new(8), 0x1000, m(3))
                .unwrap()
                .value
                .is_none()
        );
        assert_eq!(
            ix.allocations(MomentRange::between(m(2), m(9)), all())
                .unwrap()
                .total,
            Some(2)
        );
    }

    #[test]
    fn a_million_records_answer_line_and_allocation_queries_quickly() {
        use std::time::Instant;
        let (_t, ix) = open();
        let mut rng = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        const BATCH: usize = 32_768;
        const HITS: usize = 750_000;
        const LINES: u32 = 1000;
        const SLOTS: u64 = 5000;
        let started = Instant::now();
        // Line hits: 999 lines everywhere, line 1999 only in the last few thousand.
        let mut batch: Vec<Record> = (0..=LINES)
            .map(|lid| Record::Line {
                lid,
                location: SourceLoc::new("/p/src/main.rs", 1000 + lid),
            })
            .collect();
        let mut rare = Vec::new();
        for i in 0..HITS {
            let lid = if i >= HITS - 3000 && i % 1000 == 7 {
                rare.push(s(i as u64));
                LINES
            } else {
                (next() % u64::from(LINES)) as u32
            };
            batch.push(Record::LineHit {
                lid,
                at: s(i as u64),
            });
            if batch.len() >= BATCH {
                ix.commit(Batch {
                    chunks: vec![(IndexKind::Lines, std::mem::take(&mut batch))],
                    ..Default::default()
                })
                .unwrap();
            }
        }
        ix.commit(Batch {
            chunks: vec![(IndexKind::Lines, std::mem::take(&mut batch))],
            ..Default::default()
        })
        .unwrap();
        // Allocations: 128 000 blocks over 5000 reused slots, plus one huge early block.
        let mut rows: Vec<(u64, u64, u64, Option<u64>)> = Vec::new(); // addr, size, at, freed
        let mut live: HashMap<u64, usize> = HashMap::new();
        let mut t = HITS as u64;
        batch.push(Record::Allocated {
            id: 0,
            process: P,
            at: s(t),
            addr: 0x1000,
            size: 1 << 30,
            predecessor: None,
            requested_by: None,
            allocator: AllocatorFunction::Malloc,
        });
        rows.push((0x1000, 1 << 30, t, Some(t + 5)));
        t += 5;
        batch.push(Record::Freed { id: 0, at: s(t) });
        while rows.len() < 128_000 {
            t += 1;
            let slot = next() % SLOTS;
            let addr = 0x10_0000 + slot * 64;
            match live.remove(&slot) {
                Some(id) => {
                    rows[id].3 = Some(t);
                    batch.push(Record::Freed {
                        id: id as u64,
                        at: s(t),
                    });
                }
                None => {
                    let id = rows.len();
                    let size = 1 + next() % 64;
                    rows.push((addr, size, t, None));
                    live.insert(slot, id);
                    batch.push(Record::Allocated {
                        id: id as u64,
                        process: P,
                        at: s(t),
                        addr,
                        size,
                        predecessor: None,
                        requested_by: None,
                        allocator: AllocatorFunction::RustAlloc,
                    });
                }
            }
            if batch.len() >= BATCH {
                ix.commit(Batch {
                    chunks: vec![(IndexKind::Allocations, std::mem::take(&mut batch))],
                    ..Default::default()
                })
                .unwrap();
            }
        }
        let records = HITS + rows.len() * 2 - live.len() + 1;
        ix.commit(Batch {
            chunks: vec![(IndexKind::Allocations, std::mem::take(&mut batch))],
            coverage: [IndexKind::Lines, IndexKind::Allocations]
                .into_iter()
                .map(|k| (k, cov(None, vec![])))
                .collect(),
            resume: None,
        })
        .unwrap();
        assert!(records >= 1_000_000, "{records} records");
        let built = started.elapsed();

        let queries = Instant::now();
        // A rare line reads only the chunks that hold it.
        let q = ix
            .line_hits(
                &SourceLoc::new("main.rs", 1000 + LINES),
                MomentRange::all(),
                all(),
            )
            .unwrap();
        assert_eq!(
            q.items,
            rare.iter().map(|s| s.moment(REC)).collect::<Vec<_>>()
        );
        assert_eq!(q.completeness, Completeness::Complete);
        let rare_time = queries.elapsed();
        // A hot line inside a range reads the chunks of that range only.
        let range = MomentRange::between(m(100_000), m(120_000));
        let hot = ix
            .line_hits(&SourceLoc::new("main.rs", 1005), range, PageReq::first(10))
            .unwrap();
        let total = hot.total.unwrap();
        assert!(
            (10..=60).contains(&total),
            "{total} hits of a 1-in-1000 line in 20000"
        );
        assert!(hot.items.iter().all(|m| range.contains(m)));
        // Allocation queries against a brute-force answer.
        let expected = |addr: u64, at: u64| {
            rows.iter()
                .enumerate()
                .rev()
                .find(|(_, r)| {
                    addr >= r.0
                        && addr < r.0 + r.1.max(1)
                        && r.2 <= at
                        && r.3.is_none_or(|f| f > at)
                })
                .map(|(i, _)| AllocationId(i as u64))
        };
        let first_alloc = Instant::now();
        ix.allocation_at(P, 0, m(0)).unwrap();
        let fold_time = first_alloc.elapsed();
        let mut checked = Vec::new();
        let mut found = 0;
        for i in 0..2000 {
            let at = HITS as u64 + next() % (t - HITS as u64 + 10);
            let addr = if i % 50 == 0 {
                0x1000 + next() % (1 << 30)
            } else {
                0x10_0000 + (next() % SLOTS) * 64 + next() % 64
            };
            let a = ix.allocation_at(P, addr, m(at)).unwrap();
            let got = a.value.map(|a| a.id);
            if i % 20 == 0 {
                checked.push((addr, at, got));
            }
            found += usize::from(got.is_some());
        }
        let query_time = queries.elapsed();
        for (addr, at, got) in checked {
            assert_eq!(got, expected(addr, at), "{addr:#x} at {at}");
        }
        assert!(found > 100, "{found} of 2000 addresses were live");
        eprintln!(
            "{records} records: built in {built:?}; rare line in {rare_time:?}; loading the \
             allocations in {fold_time:?}; all queries in {query_time:?}"
        );
        assert!(
            query_time.as_secs_f64() < 3.0,
            "queries took {query_time:?}"
        );
        // Only a bounded number of decoded chunks is kept.
        assert!(ix.lock().loaded.chunk_cache.entries.len() <= 32);
    }

    #[test]
    fn max_tree_finds_every_value_above_a_bound() {
        let values = [3u64, 9, 1, 7, 7, 2, 10];
        let t = MaxTree::new(&values, 0);
        let mut seen = Vec::new();
        t.above_desc(1, 6, 5, &mut |i| {
            seen.push(i);
            true
        });
        assert_eq!(seen, vec![4, 3, 1]);
        let mut first = None;
        t.above_desc(0, 7, 6, &mut |i| {
            first = Some(i);
            false
        });
        assert_eq!(first, Some(6));
        let empty = MaxTree::<u64>::new(&[], 0);
        assert!(empty.above_desc(0, 3, 0, &mut |_| panic!()));
    }

    #[test]
    fn last_write_and_window_coverage() {
        let (_t, ix) = open();
        let place = |addr, len| Place::memory(P, addr, len);
        let w = |at, addr, len, b: u8| Record::Write {
            at: s(at),
            pc: 0x400000 + at,
            place: place(addr, len),
            value: WriteValue::Bytes(vec![b; len as usize]),
        };
        ix.commit(Batch {
            chunks: vec![(
                IndexKind::Writes,
                vec![w(11, 0x10, 8, 1), w(13, 0x14, 4, 2), w(15, 0x30, 8, 3)],
            )],
            coverage: vec![(
                IndexKind::Writes,
                Coverage {
                    intervals: vec![
                        CoveredInterval {
                            from: Some(s(10)),
                            through: Some(s(20)),
                            scope: CoverageScope::AllThreads,
                        },
                        CoveredInterval {
                            from: Some(s(30)),
                            through: Some(s(40)),
                            scope: CoverageScope::Thread(Tid(1)),
                        },
                    ],
                    capped: vec![],
                },
            )],
            resume: None,
        })
        .unwrap();
        let lw = ix.last_write(place(0x16, 1), m(18)).unwrap();
        assert_eq!(lw.value.as_ref().map(|w| w.at), Some(m(13)));
        assert_eq!(lw.completeness, Completeness::Complete);
        // Strictly before: the write at 13 is not before 13.
        assert_eq!(
            ix.last_write(place(0x16, 1), m(13))
                .unwrap()
                .value
                .map(|w| w.at),
            Some(m(11))
        );
        // Past the window, a later write may exist: partial.
        let late = ix.last_write(place(0x10, 1), m(25)).unwrap();
        assert_eq!(late.value.as_ref().map(|w| w.at), Some(m(11)));
        assert!(matches!(late.completeness, Completeness::Partial { .. }));
        // Nothing logged and nothing known before the window.
        let none = ix.last_write(place(0x50, 1), m(12)).unwrap();
        assert!(none.value.is_none());
        assert_eq!(
            none.completeness,
            Completeness::Partial {
                pending: vec![MomentRange {
                    start: None,
                    end: Some(m(10))
                }]
            }
        );
        // A one-thread window does not make an answer complete.
        assert!(matches!(
            ix.last_write(place(0x50, 1), m(35)).unwrap().completeness,
            Completeness::Partial { .. }
        ));
        let ws = ix
            .writes_in(place(0x10, 0x30), MomentRange::between(m(10), m(20)), all())
            .unwrap();
        assert_eq!(ws.total, Some(3));
        assert_eq!(ws.completeness, Completeness::Complete);
    }
}
