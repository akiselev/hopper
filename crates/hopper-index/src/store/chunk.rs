//! The chunk format: a fixed 64-byte header, then one zstd frame of newline-separated JSON
//! records.
//!
//! ```text
//! offset size field
//!      0    4 magic "HIXC"
//!      4    2 format version (1), little-endian
//!      6    1 index kind (IndexKind::code)
//!      7    1 reserved (0)
//!      8    4 record count
//!     12    4 uncompressed payload length
//!     16    4 compressed payload length
//!     20    4 reserved (0)
//!     24    8 first moment: frame_time      \
//!     32    8 first moment: ticks            | the order key of the earliest record
//!     40    4 first moment: sub              |
//!     44    4 first moment: tid             /
//!     48    8 last moment: frame_time
//!     56    4 last moment: sub   (ticks of the last moment are in the catalogue only)
//!     60    4 reserved (0)
//! ```
//!
//! Every chunk is self-contained: it can be read from its offset without reading the chunks
//! before it, and the catalogue's chunk table gives offsets and moment ranges for seeking.

use hopper_core::{HopperError, Result};

use super::records::{Record, Stamp};
use crate::index::IndexKind;

pub const MAGIC: &[u8; 4] = b"HIXC";
pub const VERSION: u16 = 1;
pub const HEADER_LEN: usize = 64;
const ZSTD_LEVEL: i32 = 3;

/// A decoded chunk header.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ChunkHeader {
    pub kind: IndexKind,
    pub count: u32,
    pub uncompressed_len: u32,
    pub compressed_len: u32,
    /// The earliest stamp among the records (`None` when no record has one).
    pub first: Option<Stamp>,
}

/// The earliest and latest stamps of `records`, by order key.
pub fn stamp_bounds(records: &[Record]) -> Option<(Stamp, Stamp)> {
    let mut it = records.iter().filter_map(Record::stamp);
    let first = it.next()?;
    let (mut lo, mut hi) = (first, first);
    for s in it {
        if s.key() < lo.key() {
            lo = s;
        }
        if s.key() > hi.key() {
            hi = s;
        }
    }
    Some((lo, hi))
}

/// Encode `records` as one chunk.
pub fn encode(kind: IndexKind, records: &[Record]) -> Result<Vec<u8>> {
    let mut payload = Vec::new();
    for r in records {
        serde_json::to_writer(&mut payload, r)
            .map_err(|e| HopperError::Index(format!("encode record: {e}")))?;
        payload.push(b'\n');
    }
    let compressed = zstd::bulk::compress(&payload, ZSTD_LEVEL)
        .map_err(|e| HopperError::Index(format!("compress chunk: {e}")))?;
    let len32 = |n: usize, what: &str| {
        u32::try_from(n).map_err(|_| HopperError::Index(format!("chunk {what} too large: {n}")))
    };
    let mut out = Vec::with_capacity(HEADER_LEN + compressed.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.push(kind.code());
    out.push(0);
    out.extend_from_slice(&len32(records.len(), "record count")?.to_le_bytes());
    out.extend_from_slice(&len32(payload.len(), "payload")?.to_le_bytes());
    out.extend_from_slice(&len32(compressed.len(), "compressed payload")?.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    let bounds = stamp_bounds(records);
    let (first, last) = match bounds {
        Some((f, l)) => (f, l),
        None => (Stamp(0, 0, 0, 0), Stamp(0, 0, 0, 0)),
    };
    out.extend_from_slice(&first.0.to_le_bytes());
    out.extend_from_slice(&first.2.to_le_bytes());
    out.extend_from_slice(&first.3.to_le_bytes());
    // The tid slot doubles as the "has a first moment" marker: -1 means none.
    let tid = if bounds.is_some() { first.1 } else { -1 };
    out.extend_from_slice(&tid.to_le_bytes());
    out.extend_from_slice(&last.0.to_le_bytes());
    out.extend_from_slice(&last.3.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes());
    debug_assert_eq!(out.len(), HEADER_LEN);
    out.extend_from_slice(&compressed);
    Ok(out)
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(b[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(b[at..at + 8].try_into().expect("8 bytes"))
}

/// Decode a chunk header.
pub fn decode_header(bytes: &[u8]) -> Result<ChunkHeader> {
    if bytes.len() < HEADER_LEN || &bytes[..4] != MAGIC {
        return Err(HopperError::Index("not a hopper index chunk".into()));
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != VERSION {
        return Err(HopperError::Index(format!(
            "chunk format version {version}, expected {VERSION}"
        )));
    }
    let kind = IndexKind::from_code(bytes[6])
        .ok_or_else(|| HopperError::Index(format!("unknown index kind {}", bytes[6])))?;
    let tid = i32::from_le_bytes(bytes[44..48].try_into().expect("4 bytes"));
    let first =
        (tid != -1).then(|| Stamp(u64_at(bytes, 24), tid, u64_at(bytes, 32), u32_at(bytes, 40)));
    Ok(ChunkHeader {
        kind,
        count: u32_at(bytes, 8),
        uncompressed_len: u32_at(bytes, 12),
        compressed_len: u32_at(bytes, 16),
        first,
    })
}

/// Decode a whole chunk (header plus payload).
pub fn decode(bytes: &[u8]) -> Result<(ChunkHeader, Vec<Record>)> {
    let header = decode_header(bytes)?;
    let end = HEADER_LEN + header.compressed_len as usize;
    if bytes.len() < end {
        return Err(HopperError::Index("truncated chunk".into()));
    }
    let payload = zstd::bulk::decompress(&bytes[HEADER_LEN..end], header.uncompressed_len as usize)
        .map_err(|e| HopperError::Index(format!("decompress chunk: {e}")))?;
    let mut records = Vec::with_capacity(header.count as usize);
    for line in payload.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
        records.push(
            serde_json::from_slice(line)
                .map_err(|e| HopperError::Index(format!("decode record: {e}")))?,
        );
    }
    if records.len() != header.count as usize {
        return Err(HopperError::Index(format!(
            "chunk holds {} records, header says {}",
            records.len(),
            header.count
        )));
    }
    Ok((header, records))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_with_first_moment_in_header() {
        let recs = vec![
            Record::Line {
                lid: 0,
                location: hopper_core::SourceLoc::new("main.rs", 12),
            },
            Record::LineHit {
                lid: 0,
                at: Stamp(5, 7, 0, 2),
            },
            Record::LineHit {
                lid: 0,
                at: Stamp(3, 7, 9, 1),
            },
        ];
        let bytes = encode(IndexKind::Lines, &recs).unwrap();
        let h = decode_header(&bytes).unwrap();
        assert_eq!(h.kind, IndexKind::Lines);
        assert_eq!(h.count, 3);
        assert_eq!(h.first, Some(Stamp(3, 7, 9, 1)));
        let (_, back) = decode(&bytes).unwrap();
        assert_eq!(back, recs);
    }

    #[test]
    fn empty_and_corrupt() {
        let bytes = encode(IndexKind::Calls, &[]).unwrap();
        assert_eq!(decode_header(&bytes).unwrap().first, None);
        assert!(decode(&bytes[..10]).is_err());
        let mut bad = bytes.clone();
        bad[0] = b'X';
        assert!(decode(&bad).is_err());
        let full = encode(
            IndexKind::Calls,
            &[Record::CallExit {
                id: 1,
                at: Stamp(1, 1, 1, 1),
            }],
        )
        .unwrap();
        assert!(decode(&full[..full.len() - 1]).is_err());
    }
}
