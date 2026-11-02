//! Index storage beside the trace: append-only chunk files plus a redb catalogue.
//!
//! A directory holds `catalogue.redb` and one `chunks-<kind>.bin` per [`IndexKind`]. A chunk
//! ([`chunk`]) is a 64-byte header carrying its first moment's order key and one zstd frame of
//! records. The catalogue has three tables:
//! - `chunks`: `(kind, seq)` → offset, length, record count and moment range of every chunk;
//! - `coverage`: kind → the moment intervals that kind covers and the addresses that were capped;
//! - `meta`: the recording id, the binaries' build ids, the index format version and the resume
//!   state of an unfinished phase A.
//!
//! **Crash safety.** [`Store::commit`] appends the chunk bytes and syncs the file, then commits
//! the catalogue rows in one redb transaction. A chunk is part of the index only once its row is
//! committed; [`Store::open`] truncates every chunk file to its last catalogued chunk, so a torn
//! write after a crash is dropped and the finished chunks are kept.

pub mod chunk;
pub mod records;

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use hopper_core::{HopperError, Moment, MomentRange, RecordingId, Result, SourceLoc, Tid};
use redb::{Database, ReadableDatabase, ReadableTable, TableDefinition};
use serde::{Deserialize, Serialize};

use crate::index::{IndexKind, RecordingMeta};
pub use records::{Record, Stamp};

/// The index format version stored in the catalogue. A catalogue with another version is
/// refused, not migrated.
pub const FORMAT_VERSION: u32 = 1;

const CHUNKS: TableDefinition<(u8, u64), &[u8]> = TableDefinition::new("chunks");
const COVERAGE: TableDefinition<u8, &[u8]> = TableDefinition::new("coverage");
const META: TableDefinition<&str, &[u8]> = TableDefinition::new("meta");

fn db_err(e: impl std::fmt::Display) -> HopperError {
    HopperError::Index(format!("catalogue: {e}"))
}

/// One row of the chunk table.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    pub kind: IndexKind,
    pub seq: u64,
    /// Byte offset of the chunk's header in its kind's chunk file.
    pub offset: u64,
    /// Header plus payload.
    pub len: u64,
    pub count: u32,
    /// The earliest and latest stamps of the chunk's records.
    pub first: Option<Stamp>,
    pub last: Option<Stamp>,
}

/// Which threads a covered interval speaks for.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoverageScope {
    /// Everything that happened in the interval (phase A).
    AllThreads,
    /// Only what this thread did (a phase-B window stepped one thread).
    Thread(Tid),
}

/// A moment interval an index kind has fully processed.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoveredInterval {
    /// `None`: from the start of the recording.
    pub from: Option<Stamp>,
    /// `None`: through the end of the recording; otherwise up to and including this moment.
    pub through: Option<Stamp>,
    pub scope: CoverageScope,
}

/// What a capped breakpoint address was recording.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum CapKey {
    Function(String),
    Line(SourceLoc),
    Allocator(String),
    Anchor(String),
}

/// A breakpoint address that hit its cap: results for it are missing from `from` on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Capped {
    pub key: CapKey,
    pub from: Stamp,
}

/// What one index kind has covered.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Coverage {
    pub intervals: Vec<CoveredInterval>,
    pub capped: Vec<Capped>,
}

fn after_or_eq(a: Option<Stamp>, b: Stamp) -> bool {
    a.is_none_or(|a| a.key() >= b.key())
}

impl Coverage {
    /// Whether `at` lies in an interval that speaks for `tid` (`None`: for every thread).
    pub fn covers(&self, at: Stamp, tid: Option<Tid>) -> bool {
        self.intervals.iter().any(|iv| {
            let scope_ok = match (iv.scope, tid) {
                (CoverageScope::AllThreads, _) => true,
                (CoverageScope::Thread(t), Some(q)) => t == q,
                (CoverageScope::Thread(_), None) => false,
            };
            scope_ok && iv.from.is_none_or(|f| f.key() <= at.key()) && after_or_eq(iv.through, at)
        })
    }

    /// The parts of `range` no all-thread interval covers, as moment ranges of `recording`,
    /// sorted. A pending range starts at the last covered moment (inclusive), since moments have
    /// no successor function.
    pub fn pending(&self, range: &MomentRange, recording: RecordingId) -> Vec<MomentRange> {
        type Key = (u64, u64, u32);
        let mut ivs: Vec<&CoveredInterval> = self
            .intervals
            .iter()
            .filter(|iv| iv.scope == CoverageScope::AllThreads)
            .collect();
        // Open starts first.
        ivs.sort_by_key(|iv| iv.from.map(|s| (1u8, s.key())).unwrap_or((0, (0, 0, 0))));
        let q_end: Option<Key> = range.end.as_ref().map(Moment::order_key);
        // Everything before `cursor` is accounted for; `None` is the start of the recording.
        let mut cursor: Option<Key> = range.start.as_ref().map(Moment::order_key);
        let mut cursor_m: Option<Moment> = range.start;
        let mut out = Vec::new();
        let nonempty = |from: Option<Key>, to: Option<Key>| match (from, to) {
            (Some(a), Some(b)) => a < b,
            _ => true,
        };
        for iv in ivs {
            if let Some(f) = iv.from {
                let fk = f.key();
                if cursor.is_none_or(|c| c < fk) {
                    let clipped_by_end = q_end.is_some_and(|e| e <= fk);
                    let (gap_end, gap_end_m) = if clipped_by_end {
                        (q_end, range.end)
                    } else {
                        (Some(fk), Some(f.moment(recording)))
                    };
                    if nonempty(cursor, gap_end) {
                        out.push(MomentRange {
                            start: cursor_m,
                            end: gap_end_m,
                        });
                    }
                    if clipped_by_end {
                        return out;
                    }
                }
            }
            match iv.through {
                None => return out,
                Some(t) => {
                    let tk = t.key();
                    if cursor.is_none_or(|c| tk >= c) {
                        cursor = Some(tk);
                        cursor_m = Some(t.moment(recording));
                    }
                    if q_end.is_some_and(|e| tk >= e) {
                        return out;
                    }
                }
            }
        }
        if nonempty(cursor, q_end) {
            out.push(MomentRange {
                start: cursor_m,
                end: range.end,
            });
        }
        out
    }
}

/// Everything one [`Store::commit`] writes, atomically.
#[derive(Clone, Debug, Default)]
pub struct Batch {
    pub chunks: Vec<(IndexKind, Vec<Record>)>,
    pub coverage: Vec<(IndexKind, Coverage)>,
    /// The resume state to store (`Some(None)` clears it).
    pub resume: Option<Option<Vec<u8>>>,
}

/// An open index directory.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    db: Database,
    meta: RecordingMeta,
}

/// Whether `dir` (or one of its ancestors) is an rr trace directory.
fn inside_rr_trace(dir: &Path) -> bool {
    dir.ancestors()
        .any(|d| d.join("events").is_file() && d.join("mmaps").is_file())
}

impl Store {
    /// Open (or create) the index in `dir` for `meta`'s recording. Refuses a directory inside an
    /// rr trace, a catalogue of another recording, other build ids or another format version.
    pub fn open(dir: &Path, meta: &RecordingMeta) -> Result<Store> {
        std::fs::create_dir_all(dir)?;
        let dir = dir.canonicalize()?;
        if inside_rr_trace(&dir) {
            return Err(HopperError::Invalid(format!(
                "{} is inside an rr trace; the index lives beside the trace, never in it",
                dir.display()
            )));
        }
        let db = Database::create(dir.join("catalogue.redb")).map_err(db_err)?;
        let store = Store {
            dir,
            db,
            meta: meta.clone(),
        };
        store.check_or_write_meta()?;
        store.truncate_torn_chunks()?;
        Ok(store)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn meta(&self) -> &RecordingMeta {
        &self.meta
    }

    fn check_or_write_meta(&self) -> Result<()> {
        let want = serde_json::to_vec(&self.meta).map_err(db_err)?;
        let txn = self.db.begin_write().map_err(db_err)?;
        {
            let mut t = txn.open_table(META).map_err(db_err)?;
            let version = t
                .get("format_version")
                .map_err(db_err)?
                .map(|v| v.value().to_vec());
            match version {
                Some(v) => {
                    let v: u32 = serde_json::from_slice(&v).map_err(db_err)?;
                    if v != FORMAT_VERSION {
                        return Err(HopperError::Index(format!(
                            "index format version {v}, this hopper reads {FORMAT_VERSION}"
                        )));
                    }
                    let have = t
                        .get("recording")
                        .map_err(db_err)?
                        .map(|v| v.value().to_vec());
                    let have: Option<RecordingMeta> = match have {
                        Some(h) => Some(serde_json::from_slice(&h).map_err(db_err)?),
                        None => None,
                    };
                    if have.as_ref() != Some(&self.meta) {
                        return Err(HopperError::Index(format!(
                            "the index in {} was built for {:?}, not {:?}",
                            self.dir.display(),
                            have,
                            self.meta
                        )));
                    }
                }
                None => {
                    let v = serde_json::to_vec(&FORMAT_VERSION).map_err(db_err)?;
                    t.insert("format_version", v.as_slice()).map_err(db_err)?;
                    t.insert("recording", want.as_slice()).map_err(db_err)?;
                }
            }
        }
        txn.open_table(CHUNKS).map_err(db_err)?;
        txn.open_table(COVERAGE).map_err(db_err)?;
        txn.commit().map_err(db_err)?;
        Ok(())
    }

    fn chunk_path(&self, kind: IndexKind) -> PathBuf {
        self.dir.join(format!("chunks-{}.bin", kind.name()))
    }

    fn truncate_torn_chunks(&self) -> Result<()> {
        for kind in IndexKind::ALL {
            let end = self.chunks(kind)?.last().map_or(0, |c| c.offset + c.len);
            let path = self.chunk_path(kind);
            if let Ok(md) = std::fs::metadata(&path)
                && md.len() > end
            {
                let f = OpenOptions::new().write(true).open(&path)?;
                f.set_len(end)?;
                f.sync_all()?;
            }
        }
        Ok(())
    }

    /// The chunk table of one kind, in sequence order.
    pub fn chunks(&self, kind: IndexKind) -> Result<Vec<ChunkEntry>> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(CHUNKS).map_err(db_err)?;
        let mut out = Vec::new();
        for row in t
            .range((kind.code(), 0)..=(kind.code(), u64::MAX))
            .map_err(db_err)?
        {
            let (_, v) = row.map_err(db_err)?;
            out.push(serde_json::from_slice(v.value()).map_err(db_err)?);
        }
        Ok(out)
    }

    /// The chunks of `kind` whose moments may fall inside `range` (chunks without moments are
    /// always included, since they hold dictionary records).
    pub fn chunks_overlapping(
        &self,
        kind: IndexKind,
        range: &MomentRange,
    ) -> Result<Vec<ChunkEntry>> {
        Ok(self
            .chunks(kind)?
            .into_iter()
            .filter(|c| match (c.first, c.last) {
                (Some(f), Some(l)) => {
                    range.end.is_none_or(|e| f.key() < e.order_key())
                        && range.start.is_none_or(|s| l.key() >= s.order_key())
                }
                _ => true,
            })
            .collect())
    }

    /// Read and decode one chunk.
    pub fn read_chunk(&self, entry: &ChunkEntry) -> Result<Vec<Record>> {
        let mut f = File::open(self.chunk_path(entry.kind))?;
        f.seek(SeekFrom::Start(entry.offset))?;
        let mut buf = vec![0u8; entry.len as usize];
        f.read_exact(&mut buf)?;
        let (header, records) = chunk::decode(&buf)?;
        if header.kind != entry.kind {
            return Err(HopperError::Index(format!(
                "chunk {} of {:?} holds {:?} records",
                entry.seq, entry.kind, header.kind
            )));
        }
        Ok(records)
    }

    pub fn coverage(&self, kind: IndexKind) -> Result<Coverage> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(COVERAGE).map_err(db_err)?;
        match t.get(kind.code()).map_err(db_err)? {
            Some(v) => serde_json::from_slice(v.value()).map_err(db_err),
            None => Ok(Coverage::default()),
        }
    }

    /// The stored resume state of an unfinished phase A.
    pub fn resume_state(&self) -> Result<Option<Vec<u8>>> {
        let txn = self.db.begin_read().map_err(db_err)?;
        let t = txn.open_table(META).map_err(db_err)?;
        Ok(t.get("resume").map_err(db_err)?.map(|v| v.value().to_vec()))
    }

    /// Append the batch's chunks, sync them, then commit their rows, the coverage and the resume
    /// state in one transaction.
    pub fn commit(&mut self, batch: Batch) -> Result<Vec<ChunkEntry>> {
        let mut entries = Vec::new();
        let mut next_seq: std::collections::HashMap<IndexKind, (u64, u64)> = Default::default();
        for (kind, records) in &batch.chunks {
            if records.is_empty() {
                continue;
            }
            let (seq, offset) = match next_seq.get(kind) {
                Some(s) => *s,
                None => {
                    let last = self.chunks(*kind)?.last().copied();
                    last.map_or((0, 0), |c| (c.seq + 1, c.offset + c.len))
                }
            };
            let bytes = chunk::encode(*kind, records)?;
            let mut f = OpenOptions::new()
                .create(true)
                .append(true)
                .open(self.chunk_path(*kind))?;
            let at = f.seek(SeekFrom::End(0))?;
            if at != offset {
                // Another writer, or a file we did not truncate: refuse rather than misplace.
                return Err(HopperError::Index(format!(
                    "chunk file of {:?} ends at {at}, the catalogue expects {offset}",
                    kind
                )));
            }
            f.write_all(&bytes)?;
            f.sync_data()?;
            let bounds = chunk::stamp_bounds(records);
            entries.push(ChunkEntry {
                kind: *kind,
                seq,
                offset,
                len: bytes.len() as u64,
                count: records.len() as u32,
                first: bounds.map(|b| b.0),
                last: bounds.map(|b| b.1),
            });
            next_seq.insert(*kind, (seq + 1, offset + bytes.len() as u64));
        }
        let txn = self.db.begin_write().map_err(db_err)?;
        {
            let mut t = txn.open_table(CHUNKS).map_err(db_err)?;
            for e in &entries {
                let v = serde_json::to_vec(e).map_err(db_err)?;
                t.insert((e.kind.code(), e.seq), v.as_slice())
                    .map_err(db_err)?;
            }
            let mut c = txn.open_table(COVERAGE).map_err(db_err)?;
            for (kind, cov) in &batch.coverage {
                let v = serde_json::to_vec(cov).map_err(db_err)?;
                c.insert(kind.code(), v.as_slice()).map_err(db_err)?;
            }
            if let Some(resume) = &batch.resume {
                let mut m = txn.open_table(META).map_err(db_err)?;
                match resume {
                    Some(bytes) => {
                        m.insert("resume", bytes.as_slice()).map_err(db_err)?;
                    }
                    None => {
                        m.remove("resume").map_err(db_err)?;
                    }
                }
            }
        }
        txn.commit().map_err(db_err)?;
        Ok(entries)
    }

    /// Forget everything of `kinds` (their chunks, rows and coverage); used before re-indexing
    /// from scratch on a target that cannot resume.
    pub fn clear(&mut self, kinds: &[IndexKind]) -> Result<()> {
        let txn = self.db.begin_write().map_err(db_err)?;
        {
            let mut t = txn.open_table(CHUNKS).map_err(db_err)?;
            let mut c = txn.open_table(COVERAGE).map_err(db_err)?;
            for k in kinds {
                t.retain_in((k.code(), 0)..=(k.code(), u64::MAX), |_, _| false)
                    .map_err(db_err)?;
                c.remove(k.code()).map_err(db_err)?;
            }
            let mut m = txn.open_table(META).map_err(db_err)?;
            m.remove("resume").map_err(db_err)?;
        }
        txn.commit().map_err(db_err)?;
        for k in kinds {
            let p = self.chunk_path(*k);
            if p.exists() {
                let f = OpenOptions::new().write(true).open(&p)?;
                f.set_len(0)?;
                f.sync_all()?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> RecordingId {
        RecordingId([3; 16])
    }

    fn meta() -> RecordingMeta {
        RecordingMeta {
            recording: rec(),
            build_ids: vec!["abcd".into()],
        }
    }

    fn hit(ft: u64) -> Record {
        Record::LineHit {
            lid: 0,
            at: Stamp(ft, 1, 0, 0),
        }
    }

    #[test]
    fn commits_reopens_and_drops_torn_writes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("idx");
        {
            let mut s = Store::open(&dir, &meta()).unwrap();
            s.commit(Batch {
                chunks: vec![(IndexKind::Lines, vec![hit(1), hit(2)])],
                ..Default::default()
            })
            .unwrap();
            s.commit(Batch {
                chunks: vec![(IndexKind::Lines, vec![hit(3)])],
                coverage: vec![(
                    IndexKind::Lines,
                    Coverage {
                        intervals: vec![CoveredInterval {
                            from: None,
                            through: Some(Stamp(3, 1, 0, 0)),
                            scope: CoverageScope::AllThreads,
                        }],
                        capped: vec![],
                    },
                )],
                resume: Some(Some(b"state".to_vec())),
            })
            .unwrap();
        }
        // A torn write after the last commit.
        let path = dir.join("chunks-lines.bin");
        let good_len = std::fs::metadata(&path).unwrap().len();
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"HIXCgarbage").unwrap();
        drop(f);

        let mut s = Store::open(&dir, &meta()).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), good_len);
        let chunks = s.chunks(IndexKind::Lines).unwrap();
        assert_eq!(chunks.len(), 2);
        assert_eq!(chunks[1].first, Some(Stamp(3, 1, 0, 0)));
        let all: Vec<Record> = chunks
            .iter()
            .flat_map(|c| s.read_chunk(c).unwrap())
            .collect();
        assert_eq!(all, vec![hit(1), hit(2), hit(3)]);
        assert_eq!(s.resume_state().unwrap(), Some(b"state".to_vec()));
        assert_eq!(s.coverage(IndexKind::Lines).unwrap().intervals.len(), 1);
        assert_eq!(s.coverage(IndexKind::Calls).unwrap(), Coverage::default());
        // Appending after reopening continues the sequence.
        s.commit(Batch {
            chunks: vec![(IndexKind::Lines, vec![hit(4)])],
            ..Default::default()
        })
        .unwrap();
        assert_eq!(s.chunks(IndexKind::Lines).unwrap()[2].seq, 2);
        let range = MomentRange {
            start: Some(Stamp(3, 1, 0, 0).moment(rec())),
            end: None,
        };
        assert_eq!(
            s.chunks_overlapping(IndexKind::Lines, &range)
                .unwrap()
                .len(),
            2
        );
        s.clear(&[IndexKind::Lines]).unwrap();
        assert!(s.chunks(IndexKind::Lines).unwrap().is_empty());
        assert_eq!(s.resume_state().unwrap(), None);
    }

    #[test]
    fn refuses_other_recordings_and_rr_traces() {
        let tmp = tempfile::tempdir().unwrap();
        Store::open(tmp.path(), &meta()).unwrap();
        let other = RecordingMeta {
            recording: RecordingId([4; 16]),
            build_ids: vec![],
        };
        assert!(matches!(
            Store::open(tmp.path(), &other),
            Err(HopperError::Index(_))
        ));
        let trace = tmp.path().join("trace");
        std::fs::create_dir_all(&trace).unwrap();
        std::fs::write(trace.join("events"), b"").unwrap();
        std::fs::write(trace.join("mmaps"), b"").unwrap();
        assert!(matches!(
            Store::open(&trace.join("index"), &meta()),
            Err(HopperError::Invalid(_))
        ));
    }

    #[test]
    fn pending_is_the_complement_of_coverage() {
        let r = rec();
        let m = |ft| Stamp(ft, 1, 0, 0).moment(r);
        let iv = |from: Option<u64>, through: Option<u64>| CoveredInterval {
            from: from.map(|f| Stamp(f, 1, 0, 0)),
            through: through.map(|t| Stamp(t, 1, 0, 0)),
            scope: CoverageScope::AllThreads,
        };
        let empty = Coverage::default();
        assert_eq!(
            empty.pending(&MomentRange::all(), r),
            vec![MomentRange::all()]
        );
        let done = Coverage {
            intervals: vec![iv(None, None)],
            capped: vec![],
        };
        assert!(done.pending(&MomentRange::all(), r).is_empty());
        let half = Coverage {
            intervals: vec![iv(None, Some(10))],
            capped: vec![],
        };
        assert_eq!(
            half.pending(&MomentRange::all(), r),
            vec![MomentRange {
                start: Some(m(10)),
                end: None
            }]
        );
        assert!(
            half.pending(&MomentRange::between(m(2), m(8)), r)
                .is_empty()
        );
        let gap = Coverage {
            intervals: vec![iv(None, Some(10)), iv(Some(20), None)],
            capped: vec![],
        };
        assert_eq!(
            gap.pending(&MomentRange::all(), r),
            vec![MomentRange::between(m(10), m(20))]
        );
        assert_eq!(
            gap.pending(&MomentRange::between(m(12), m(15)), r),
            vec![MomentRange::between(m(12), m(15))]
        );
        assert!(gap.covers(Stamp(5, 9, 0, 0), None));
        assert!(!gap.covers(Stamp(15, 9, 0, 0), None));
    }
}
