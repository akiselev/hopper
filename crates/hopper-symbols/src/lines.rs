//! Line tables: one unit's rows, decoded once, with the queries stepping and breakpoints need.

use std::ops::Range;

use gimli::AttributeValue;
use hopper_core::{HopperError, SourceLoc};

use crate::dwarf::{Reader, UnitInfo, sym_err};

/// One row of a line table (file address).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LineRow {
    pub address: u64,
    /// Index into [`LineTable::files`].
    pub file: u32,
    /// 0 means "no source line" (compiler-generated code).
    pub line: u32,
    pub column: u32,
    pub is_stmt: bool,
    pub prologue_end: bool,
}

/// One contiguous sequence of rows; `end` is the address of its end-sequence row.
#[derive(Clone, Debug)]
pub(crate) struct Sequence {
    pub rows: Vec<LineRow>,
    pub end: u64,
}

impl Sequence {
    pub fn range(&self) -> Range<u64> {
        self.rows.first().map_or(self.end, |r| r.address)..self.end
    }

    /// The index of the last row at or below `addr`, when `addr` is inside the sequence.
    pub fn row_index(&self, addr: u64) -> Option<usize> {
        if !self.range().contains(&addr) {
            return None;
        }
        let i = self.rows.partition_point(|r| r.address <= addr);
        i.checked_sub(1)
    }

    /// The end address of row `i` (the next row with a higher address, or the sequence end).
    pub fn row_end(&self, i: usize) -> u64 {
        let a = self.rows[i].address;
        self.rows[i + 1..]
            .iter()
            .find(|r| r.address > a)
            .map_or(self.end, |r| r.address)
    }
}

/// A unit's decoded line table.
#[derive(Clone, Debug, Default)]
pub struct LineTable {
    /// File paths, joined with the compilation directory, indexed by [`LineRow::file`].
    pub files: Vec<String>,
    pub(crate) sequences: Vec<Sequence>,
}

impl LineTable {
    pub(crate) fn load(
        dwarf: &gimli::Dwarf<Reader>,
        info: &UnitInfo,
    ) -> Result<LineTable, HopperError> {
        let unit = &info.unit;
        let Some(program) = unit.line_program.clone() else {
            return Ok(LineTable::default());
        };
        let header = program.header().clone();
        // Map raw file indices to rendered paths. DWARF 5 indexes from 0, earlier versions from 1.
        let mut files = Vec::new();
        let mut raw_to_file = std::collections::HashMap::new();
        let first = if header.version() >= 5 { 0 } else { 1 };
        for (i, entry) in header.file_names().iter().enumerate() {
            let raw = first + i as u64;
            let path = render_file(dwarf, info, &header, entry).unwrap_or_default();
            raw_to_file.insert(raw, files.len() as u32);
            files.push(path);
        }
        let mut sequences = Vec::new();
        let mut rows = program.rows();
        let mut current = Vec::new();
        while let Some((_, row)) = rows.next_row().map_err(sym_err)? {
            if row.end_sequence() {
                if !current.is_empty() {
                    sequences.push(Sequence {
                        rows: std::mem::take(&mut current),
                        end: row.address(),
                    });
                }
                continue;
            }
            let file = raw_to_file
                .get(&row.file_index())
                .copied()
                .unwrap_or(u32::MAX);
            current.push(LineRow {
                address: row.address(),
                file,
                line: row.line().map_or(0, |l| l.get() as u32),
                column: match row.column() {
                    gimli::ColumnType::LeftEdge => 0,
                    gimli::ColumnType::Column(c) => c.get() as u32,
                },
                is_stmt: row.is_stmt(),
                prologue_end: row.prologue_end(),
            });
        }
        // Sequences that start at address 0 in an executable are discarded functions (dead code
        // the linker removed); they would shadow real code at low addresses.
        sequences.retain(|s| s.rows.first().is_some_and(|r| r.address != 0));
        sequences.sort_by_key(|s| s.range().start);
        Ok(LineTable { files, sequences })
    }

    pub fn file_name(&self, file: u32) -> Option<&str> {
        self.files.get(file as usize).map(String::as_str)
    }

    pub(crate) fn sequence_at(&self, addr: u64) -> Option<&Sequence> {
        let i = self.sequences.partition_point(|s| s.range().start <= addr);
        self.sequences[..i]
            .iter()
            .rev()
            .find(|s| s.range().contains(&addr))
    }

    /// The source location of the row covering `addr`.
    pub fn location(&self, addr: u64) -> Option<SourceLoc> {
        let seq = self.sequence_at(addr)?;
        let row = seq.rows[seq.row_index(addr)?];
        self.row_location(&row)
    }

    pub fn row_location(&self, row: &LineRow) -> Option<SourceLoc> {
        if row.line == 0 {
            return None;
        }
        Some(SourceLoc {
            file: self.file_name(row.file)?.to_string(),
            line: row.line,
            column: (row.column != 0).then_some(row.column),
        })
    }

    /// Every row, with its sequence.
    pub(crate) fn rows(&self) -> impl Iterator<Item = (&Sequence, usize, &LineRow)> {
        self.sequences
            .iter()
            .flat_map(|s| s.rows.iter().enumerate().map(move |(i, r)| (s, i, r)))
    }

    /// The rows inside `range`, in address order.
    pub(crate) fn rows_in(&self, range: Range<u64>) -> Vec<LineRow> {
        let mut out: Vec<LineRow> = self
            .sequences
            .iter()
            .filter(|s| s.range().start < range.end && range.start < s.range().end)
            .flat_map(|s| s.rows.iter().copied())
            .filter(|r| range.contains(&r.address))
            .collect();
        out.sort_by_key(|r| r.address);
        out
    }

    /// Indices of the files whose path ends with `query` on a component boundary.
    pub fn matching_files(&self, query: &str) -> Vec<u32> {
        self.files
            .iter()
            .enumerate()
            .filter(|(_, f)| hopper_core::source::file_suffix_match(f, query))
            .map(|(i, _)| i as u32)
            .collect()
    }
}

/// `comp_dir`, then the file's directory, then its name; an absolute component replaces what came
/// before (the same rule addr2line uses).
fn render_file(
    dwarf: &gimli::Dwarf<Reader>,
    info: &UnitInfo,
    header: &gimli::LineProgramHeader<Reader>,
    file: &gimli::FileEntry<Reader>,
) -> Option<String> {
    let mut path = info.comp_dir.clone().unwrap_or_default();
    let attr_str = |v: AttributeValue<Reader>| {
        dwarf
            .attr_string(&info.unit, v)
            .ok()
            .map(|r| crate::dwarf::reader_string(&r))
    };
    if file.directory_index() != 0
        && let Some(dir) = file.directory(header).and_then(attr_str)
    {
        push_path(&mut path, &dir);
    }
    push_path(&mut path, &attr_str(file.path_name())?);
    Some(path)
}

fn push_path(path: &mut String, p: &str) {
    if p.starts_with('/') || path.is_empty() {
        *path = p.to_string();
    } else {
        if !path.ends_with('/') {
            path.push('/');
        }
        path.push_str(p);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(address: u64, line: u32) -> LineRow {
        LineRow {
            address,
            file: 0,
            line,
            column: 0,
            is_stmt: true,
            prologue_end: false,
        }
    }

    #[test]
    fn sequence_lookup_and_row_end() {
        let s = Sequence {
            rows: vec![row(0x10, 1), row(0x10, 2), row(0x18, 3)],
            end: 0x20,
        };
        assert_eq!(s.row_index(0x10), Some(1));
        assert_eq!(s.row_index(0x17), Some(1));
        assert_eq!(s.row_index(0x1f), Some(2));
        assert_eq!(s.row_index(0x20), None);
        assert_eq!(s.row_end(0), 0x18);
        assert_eq!(s.row_end(2), 0x20);
    }

    #[test]
    fn joins_paths() {
        let mut p = "/home/u/proj".to_string();
        push_path(&mut p, "src/main.rs");
        assert_eq!(p, "/home/u/proj/src/main.rs");
        push_path(&mut p, "/rustc/abc/library/core/src/lib.rs");
        assert_eq!(p, "/rustc/abc/library/core/src/lib.rs");
    }
}
