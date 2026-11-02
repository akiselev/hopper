//! Modules (a binary plus its load bias) and the symbol set that looks things up by runtime
//! address. Every address in this module's public API is a *runtime* address.

use std::collections::BTreeMap;
use std::ops::Range;
use std::sync::Arc;

use hopper_core::{HopperError, Result, SourceLoc};

use crate::binary::{Binary, BinaryKind, SymbolKind};
use crate::dwarf::{DieRef, DwarfIndex, FunctionRef};
use crate::lines::LineRow;
use crate::names;

/// A binary loaded at `bias`: runtime address = file address + bias.
#[derive(Clone, Debug)]
pub struct Module {
    binary: Arc<Binary>,
    bias: u64,
}

/// A module's position in a [`SymbolSet`]. Modules are only ever appended, so ids stay valid.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ModuleId(pub usize);

impl Module {
    pub fn new(binary: Arc<Binary>, bias: u64) -> Module {
        Module { binary, bias }
    }

    /// A module loaded at its own file addresses (an `ET_EXEC`, or a file inspected statically).
    pub fn unrelocated(binary: Arc<Binary>) -> Module {
        Module { binary, bias: 0 }
    }

    /// The main executable, placed by the auxiliary vector (`AT_ENTRY` minus `e_entry`).
    pub fn main_from_auxv(binary: Arc<Binary>, auxv: &[u8]) -> Result<Module> {
        let entry = crate::auxv::Auxv::parse(auxv)
            .entry()
            .ok_or_else(|| HopperError::NotFound("AT_ENTRY in the auxiliary vector".into()))?;
        let bias = match binary.kind() {
            BinaryKind::FixedExecutable => 0,
            _ => entry.wrapping_sub(binary.entry()),
        };
        Ok(Module { binary, bias })
    }

    /// A module placed by one of its mappings: the file offset `file_offset` is mapped at
    /// `map_start` (as `/proc/<pid>/maps` or a core file's `NT_FILE` note say).
    pub fn from_mapping(binary: Arc<Binary>, map_start: u64, file_offset: u64) -> Module {
        let bias = match binary.kind() {
            BinaryKind::FixedExecutable => 0,
            _ => {
                let seg = binary
                    .segments()
                    .iter()
                    .find(|s| s.file_offset & !0xfff == file_offset & !0xfff)
                    .or_else(|| binary.segments().first());
                match seg {
                    Some(s) => map_start.wrapping_sub(s.vaddr & !0xfff),
                    None => map_start,
                }
            }
        };
        Module { binary, bias }
    }

    pub fn binary(&self) -> &Arc<Binary> {
        &self.binary
    }

    pub fn bias(&self) -> u64 {
        self.bias
    }

    pub fn to_file(&self, runtime: u64) -> u64 {
        runtime.wrapping_sub(self.bias)
    }

    pub fn to_runtime(&self, file: u64) -> u64 {
        file.wrapping_add(self.bias)
    }

    /// Whether the runtime address falls inside one of the module's loaded segments.
    pub fn contains(&self, runtime: u64) -> bool {
        self.binary.contains(self.to_file(runtime))
    }

    pub fn runtime_ranges(&self) -> Vec<Range<u64>> {
        self.binary
            .segments()
            .iter()
            .map(|s| self.to_runtime(s.vaddr)..self.to_runtime(s.vaddr + s.memsz))
            .collect()
    }
}

/// A function: a concrete subprogram, an inlined instance of one, or a symbol-table entry when
/// there is no debug information.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionInfo {
    pub module: ModuleId,
    /// Qualified name (`values::stop_here`, `std::rt::lang_start<()>`).
    pub name: String,
    pub linkage_name: Option<String>,
    /// The entry address.
    pub entry: u64,
    /// Every address range of the function (one inlined instance can have several).
    pub ranges: Vec<Range<u64>>,
    pub decl: Option<SourceLoc>,
    pub origin: FunctionOrigin,
}

/// Where a [`FunctionInfo`] came from.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum FunctionOrigin {
    /// A `DW_TAG_subprogram` with code.
    Subprogram { die: DieRef },
    /// A `DW_TAG_inlined_subroutine`: one inlined copy.
    Inlined { die: DieRef, origin: Option<DieRef> },
    /// Only a symbol-table entry.
    Symbol,
}

impl FunctionInfo {
    pub fn contains(&self, pc: u64) -> bool {
        self.ranges.iter().any(|r| r.contains(&pc))
    }

    /// The lowest and highest address of the function.
    pub fn bounds(&self) -> Option<Range<u64>> {
        let lo = self.ranges.iter().map(|r| r.start).min()?;
        let hi = self.ranges.iter().map(|r| r.end).max()?;
        Some(lo..hi)
    }

    pub fn die(&self) -> Option<DieRef> {
        match self.origin {
            FunctionOrigin::Subprogram { die } | FunctionOrigin::Inlined { die, .. } => Some(die),
            FunctionOrigin::Symbol => None,
        }
    }
}

/// One source-level frame at a PC: the innermost inlined call first.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlineFrame {
    pub module: ModuleId,
    pub function: Option<String>,
    /// For the innermost frame, the line of the PC; for outer frames, the call site.
    pub location: Option<SourceLoc>,
    /// The `DW_TAG_subprogram` or `DW_TAG_inlined_subroutine` DIE.
    pub die: Option<DieRef>,
    /// This frame is an inlined call inside the next (outer) one.
    pub inlined: bool,
}

/// A symbol-table hit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymbolHit {
    pub module: ModuleId,
    pub name: String,
    /// The symbol's runtime address.
    pub address: u64,
    pub offset: u64,
}

impl SymbolHit {
    /// `name` or `name+0x1f`.
    pub fn label(&self) -> String {
        if self.offset == 0 {
            self.name.clone()
        } else {
            format!("{}+{:#x}", self.name, self.offset)
        }
    }
}

/// An address a source line resolved to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineAddress {
    pub module: ModuleId,
    pub address: u64,
    pub location: SourceLoc,
    /// The function (or inlined function) the address is in.
    pub function: Option<String>,
}

/// The run of line-table rows around a PC that belong to one source line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LineRange {
    pub range: Range<u64>,
    pub location: SourceLoc,
}

/// Several modules, looked up by runtime address.
#[derive(Clone, Debug, Default)]
pub struct SymbolSet {
    modules: Vec<Module>,
}

impl SymbolSet {
    pub fn new() -> SymbolSet {
        SymbolSet::default()
    }

    pub fn add(&mut self, module: Module) -> ModuleId {
        self.modules.push(module);
        ModuleId(self.modules.len() - 1)
    }

    pub fn modules(&self) -> impl Iterator<Item = (ModuleId, &Module)> {
        self.modules
            .iter()
            .enumerate()
            .map(|(i, m)| (ModuleId(i), m))
    }

    pub fn module(&self, id: ModuleId) -> Option<&Module> {
        self.modules.get(id.0)
    }

    /// The module whose loaded segments contain `pc`.
    pub fn module_at(&self, pc: u64) -> Option<(ModuleId, &Module)> {
        self.modules().find(|(_, m)| m.contains(pc))
    }

    /// The module holding a binary with this build id.
    pub fn module_by_build_id(&self, id: &crate::BuildId) -> Option<(ModuleId, &Module)> {
        self.modules()
            .find(|(_, m)| m.binary().build_id() == Some(id))
    }

    /// The symbol-table entry containing `pc`.
    pub fn symbol_at(&self, pc: u64) -> Option<SymbolHit> {
        let (id, m) = self.module_at(pc)?;
        let sym = m.binary().symbol_at(m.to_file(pc))?;
        let address = m.to_runtime(sym.address);
        Some(SymbolHit {
            module: id,
            name: sym.name.clone(),
            address,
            offset: pc - address,
        })
    }

    /// The source location of `pc` (the innermost line-table row).
    pub fn location(&self, pc: u64) -> Option<SourceLoc> {
        let (_, m) = self.module_at(pc)?;
        let file_pc = m.to_file(pc);
        let from_a2l = m.binary().with_addr2line(|ctx| {
            let loc = ctx.find_location(file_pc).ok().flatten()?;
            Some(SourceLoc {
                file: loc.file?.to_string(),
                line: loc.line.filter(|l| *l != 0)?,
                column: loc.column.filter(|c| *c != 0),
            })
        });
        if let Some(Some(loc)) = from_a2l {
            return Some(loc);
        }
        let dwarf = m.binary().dwarf().ok()?;
        dwarf
            .units_at(file_pc)
            .find_map(|u| dwarf.line_table(u).ok()?.location(file_pc))
    }

    /// The source-level frames at `pc`, innermost inlined call first. Without debug information
    /// it is one frame named by the symbol table.
    pub fn frames_at(&self, pc: u64) -> Vec<InlineFrame> {
        let Some((id, m)) = self.module_at(pc) else {
            return Vec::new();
        };
        let file_pc = m.to_file(pc);
        let dwarf = m.binary().dwarf().ok();
        let raw = m.binary().with_addr2line(|ctx| {
            let unit_off = ctx
                .find_dwarf_and_unit(file_pc)
                .skip_all_loads()
                .and_then(|u| u.unit.header.debug_info_offset())
                .map(|o| o.0 as u64);
            let mut out = Vec::new();
            if let Ok(mut iter) = ctx.find_frames(file_pc).skip_all_loads() {
                while let Ok(Some(f)) = iter.next() {
                    let die = match (unit_off, f.dw_die_offset) {
                        (Some(base), Some(off)) => Some(DieRef(base + off.0 as u64)),
                        _ => None,
                    };
                    let name = f
                        .function
                        .as_ref()
                        .and_then(|n| n.raw_name().ok())
                        .map(|n| n.into_owned());
                    let location = f.location.and_then(|l| {
                        Some(SourceLoc {
                            file: l.file?.to_string(),
                            line: l.line.filter(|x| *x != 0)?,
                            column: l.column.filter(|c| *c != 0),
                        })
                    });
                    out.push((die, name, location));
                }
            }
            out
        });
        let raw = raw.unwrap_or_default();
        let has_function = raw.iter().any(|(die, _, _)| die.is_some());
        if !has_function {
            let function = self.symbol_at(pc).map(|s| s.name);
            let location = raw.into_iter().find_map(|(_, _, l)| l);
            if function.is_none() && location.is_none() {
                return Vec::new();
            }
            return vec![InlineFrame {
                module: id,
                function,
                location,
                die: None,
                inlined: false,
            }];
        }
        let n = raw.len();
        raw.into_iter()
            .enumerate()
            .map(|(i, (die, raw_name, location))| {
                let function = die
                    .and_then(|d| dwarf.and_then(|dw| dw.function_name(d)))
                    .or_else(|| raw_name.map(|r| names::demangle(&r)));
                InlineFrame {
                    module: id,
                    function,
                    location,
                    die,
                    inlined: i + 1 < n,
                }
            })
            .collect()
    }

    /// The concrete function containing `pc`, or the symbol-table entry when there is no DWARF.
    pub fn function_at(&self, pc: u64) -> Option<FunctionInfo> {
        let (id, m) = self.module_at(pc)?;
        let file_pc = m.to_file(pc);
        if let Ok(dwarf) = m.binary().dwarf()
            && let Some(loc) = dwarf.functions_at(file_pc).into_iter().next()
        {
            let f = dwarf.function(&loc)?;
            return Some(self.subprogram_info(id, m, dwarf, loc.unit, f));
        }
        let sym = m.binary().symbol_at(file_pc)?;
        (sym.kind == SymbolKind::Function).then(|| FunctionInfo {
            module: id,
            name: sym.name.clone(),
            linkage_name: Some(sym.raw_name.clone()),
            entry: m.to_runtime(sym.address),
            ranges: std::iter::once(
                m.to_runtime(sym.address)..m.to_runtime(sym.address + sym.size.max(1)),
            )
            .collect(),
            decl: None,
            origin: FunctionOrigin::Symbol,
        })
    }

    /// The address ranges of the function containing `pc`.
    pub fn function_bounds(&self, pc: u64) -> Option<Vec<Range<u64>>> {
        self.function_at(pc).map(|f| f.ranges)
    }

    fn subprogram_info(
        &self,
        id: ModuleId,
        m: &Module,
        dwarf: &DwarfIndex,
        unit: usize,
        f: &crate::dwarf::FunctionDie,
    ) -> FunctionInfo {
        let ranges: Vec<Range<u64>> = f
            .ranges
            .iter()
            .map(|r| m.to_runtime(r.start)..m.to_runtime(r.end))
            .collect();
        let entry = ranges.iter().map(|r| r.start).min().unwrap_or(0);
        FunctionInfo {
            module: id,
            name: dwarf
                .function_name(f.offset)
                .unwrap_or_else(|| f.name.clone().unwrap_or_else(|| "?".into())),
            linkage_name: dwarf.linkage_name(f.offset),
            entry,
            ranges,
            decl: self.decl_location(dwarf, unit, f),
            origin: FunctionOrigin::Subprogram { die: f.offset },
        }
    }

    fn decl_location(
        &self,
        dwarf: &DwarfIndex,
        unit: usize,
        f: &crate::dwarf::FunctionDie,
    ) -> Option<SourceLoc> {
        // Declarations live on the subprogram or on its origin.
        let (file, line, unit) = match (f.decl_file, f.decl_line) {
            (Some(file), Some(line)) => (file, line, unit),
            _ => {
                let origin = f.origin?;
                let ou = dwarf.unit_index_of(origin).ok()?;
                let ix = dwarf.unit_index(ou).ok()?;
                let of = &ix.functions[*ix.by_offset.get(&origin)?];
                (of.decl_file?, of.decl_line?, ou)
            }
        };
        let table = dwarf.line_table(unit).ok()?;
        let version5 = dwarf.units()[unit]
            .unit
            .line_program
            .as_ref()
            .is_some_and(|p| p.header().version() >= 5);
        let idx = if version5 { file } else { file.checked_sub(1)? };
        Some(SourceLoc::new(table.file_name(idx as u32)?, line))
    }

    /// Functions named `name`: a full path (`worked_example::load`), a suffix of one (`load`), or
    /// a symbol-table name. Exact names win over suffixes; `main` means the program's main
    /// subprogram (`DW_AT_main_subprogram`) when there is one, as in gdb. Inlined copies are
    /// included.
    pub fn find_functions(&self, name: &str) -> Vec<FunctionInfo> {
        let mut out = Vec::new();
        for (id, m) in self.modules() {
            let Ok(dwarf) = m.binary().dwarf() else {
                continue;
            };
            let refs = if name == "main" {
                let mains = dwarf.main_subprograms();
                if mains.is_empty() {
                    dwarf.find_functions(name)
                } else {
                    mains
                }
            } else {
                dwarf.find_functions(name)
            };
            for r in refs {
                if let Some(info) = self.function_ref_info(id, m, dwarf, r) {
                    out.push(info);
                }
            }
        }
        // Symbol-table entries with no DWARF function at the same address.
        for (id, m) in self.modules() {
            for sym in m.binary().symbols() {
                if sym.kind != SymbolKind::Function {
                    continue;
                }
                let matches = sym.name == name
                    || sym.raw_name == name
                    || names::path_suffix_match(&sym.name, name);
                if !matches {
                    continue;
                }
                let entry = m.to_runtime(sym.address);
                if out.iter().any(|f: &FunctionInfo| f.entry == entry) {
                    continue;
                }
                if name == "main" && !out.is_empty() {
                    continue;
                }
                out.push(FunctionInfo {
                    module: id,
                    name: sym.name.clone(),
                    linkage_name: Some(sym.raw_name.clone()),
                    entry,
                    ranges: std::iter::once(entry..entry + sym.size.max(1)).collect(),
                    decl: None,
                    origin: FunctionOrigin::Symbol,
                });
            }
        }
        // Exact names before suffix matches.
        let exact: Vec<FunctionInfo> = out
            .iter()
            .filter(|f| f.name == name || names::strip_generics(&f.name) == name)
            .cloned()
            .collect();
        let mut result = if exact.is_empty() || name == "main" {
            out
        } else {
            exact
        };
        result.sort_by_key(|f| (f.module, f.entry));
        result.dedup_by(|a, b| a.entry == b.entry && a.module == b.module);
        result
    }

    fn function_ref_info(
        &self,
        id: ModuleId,
        m: &Module,
        dwarf: &DwarfIndex,
        r: FunctionRef,
    ) -> Option<FunctionInfo> {
        match r {
            FunctionRef::Concrete { unit, index } => {
                let f = &dwarf.unit_index(unit).ok()?.functions[index];
                if !f.ranges.iter().any(|r| m.binary().contains_code(r.start)) {
                    return None;
                }
                Some(self.subprogram_info(id, m, dwarf, unit, f))
            }
            FunctionRef::Inlined { unit, index } => {
                let inl = &dwarf.unit_index(unit).ok()?.inlined[index];
                if !m.binary().contains_code(inl.entry_pc?) {
                    return None;
                }
                let ranges: Vec<Range<u64>> = inl
                    .ranges
                    .iter()
                    .map(|r| m.to_runtime(r.start)..m.to_runtime(r.end))
                    .collect();
                let entry = m.to_runtime(inl.entry_pc?);
                let origin = inl.origin;
                Some(FunctionInfo {
                    module: id,
                    name: origin
                        .and_then(|o| dwarf.function_name(o))
                        .unwrap_or_else(|| "?".into()),
                    linkage_name: origin.and_then(|o| dwarf.linkage_name(o)),
                    entry,
                    ranges,
                    decl: None,
                    origin: FunctionOrigin::Inlined {
                        die: inl.offset,
                        origin,
                    },
                })
            }
        }
    }

    /// Where a breakpoint on `function` goes: after its prologue. Uses the first line-table row
    /// marked `prologue_end`; without one, gdb's rule of the second line (the first row past the
    /// entry whose line differs from the entry's). Inlined copies have no prologue.
    pub fn breakpoint_address(&self, function: &FunctionInfo) -> u64 {
        if !matches!(function.origin, FunctionOrigin::Subprogram { .. }) {
            return function.entry;
        }
        let Some(m) = self.module(function.module) else {
            return function.entry;
        };
        let Ok(dwarf) = m.binary().dwarf() else {
            return function.entry;
        };
        let entry = m.to_file(function.entry);
        let Some(range) = function
            .ranges
            .iter()
            .find(|r| r.contains(&function.entry))
            .map(|r| m.to_file(r.start)..m.to_file(r.end))
        else {
            return function.entry;
        };
        let Some(unit) = dwarf.units_at(entry).next() else {
            return function.entry;
        };
        let Ok(table) = dwarf.line_table(unit) else {
            return function.entry;
        };
        let rows = table.rows_in(range);
        m.to_runtime(prologue_end(&rows, entry))
    }

    /// The addresses of `file:line`, as `break file:line` resolves them in gdb: rows that are
    /// statements of that line in a file whose path ends with `file`; the lowest address per
    /// function (or per inlined copy), moved past the prologue when it is a function's entry.
    /// When no row has exactly that line, the next line that has code is used.
    pub fn addresses_for_line(&self, file: &str, line: u32) -> Vec<LineAddress> {
        let mut exact = Vec::new();
        let mut best_next: Option<u32> = None;
        let mut candidates_by_line: BTreeMap<u32, Vec<(ModuleId, usize, LineRow, String)>> =
            BTreeMap::new();
        for (id, m) in self.modules() {
            let Ok(dwarf) = m.binary().dwarf() else {
                continue;
            };
            for unit in 0..dwarf.units().len() {
                let Ok(table) = dwarf.line_table(unit) else {
                    continue;
                };
                let files = table.matching_files(file);
                if files.is_empty() {
                    continue;
                }
                for (_, _, row) in table.rows() {
                    if !row.is_stmt || !files.contains(&row.file) || row.line < line {
                        continue;
                    }
                    let path = table.file_name(row.file).unwrap_or_default().to_string();
                    if row.line == line {
                        exact.push((id, unit, *row, path));
                    } else {
                        best_next = Some(best_next.map_or(row.line, |b| b.min(row.line)));
                        candidates_by_line
                            .entry(row.line)
                            .or_default()
                            .push((id, unit, *row, path));
                    }
                }
            }
        }
        let rows = if exact.is_empty() {
            match best_next.and_then(|l| candidates_by_line.remove(&l)) {
                Some(rows) => rows,
                None => return Vec::new(),
            }
        } else {
            exact
        };
        // Keep the lowest address per block (inlined copy, else function).
        let mut per_block: BTreeMap<(ModuleId, Option<DieRef>), (u64, LineRow, String)> =
            BTreeMap::new();
        for (id, _unit, row, path) in rows {
            let Some(m) = self.module(id) else { continue };
            let Ok(dwarf) = m.binary().dwarf() else {
                continue;
            };
            let block = innermost_block(dwarf, row.address);
            let slot = per_block
                .entry((id, block))
                .or_insert((row.address, row, path.clone()));
            if row.address < slot.0 {
                *slot = (row.address, row, path);
            }
        }
        let mut out = Vec::new();
        for ((id, _), (addr, row, path)) in per_block {
            let Some(m) = self.module(id) else { continue };
            let mut runtime = m.to_runtime(addr);
            if let Some(f) = self.function_at(runtime)
                && f.entry == runtime
            {
                runtime = self.breakpoint_address(&f);
            }
            let function = self
                .frames_at(runtime)
                .into_iter()
                .next()
                .and_then(|f| f.function);
            out.push(LineAddress {
                module: id,
                address: runtime,
                location: SourceLoc {
                    file: path,
                    line: row.line,
                    column: (row.column != 0).then_some(row.column),
                },
                function,
            });
        }
        out.sort_by_key(|a| a.address);
        out.dedup_by_key(|a| a.address);
        out
    }

    /// The addresses of the line containing `pc`: the contiguous rows around `pc` with the same
    /// file and line. Stepping over or into a line runs until the PC leaves this range.
    pub fn line_range_at(&self, pc: u64) -> Option<LineRange> {
        let (_, m) = self.module_at(pc)?;
        let file_pc = m.to_file(pc);
        let dwarf = m.binary().dwarf().ok()?;
        for unit in dwarf.units_at(file_pc) {
            let Ok(table) = dwarf.line_table(unit) else {
                continue;
            };
            let Some(seq) = table.sequence_at(file_pc) else {
                continue;
            };
            let i = seq.row_index(file_pc)?;
            let row = seq.rows[i];
            if row.line == 0 {
                return None;
            }
            let same = |r: &LineRow| r.file == row.file && r.line == row.line;
            let mut start = i;
            while start > 0 && same(&seq.rows[start - 1]) {
                start -= 1;
            }
            let mut end = i;
            while end + 1 < seq.rows.len() && same(&seq.rows[end + 1]) {
                end += 1;
            }
            let location = table.row_location(&row)?;
            return Some(LineRange {
                range: m.to_runtime(seq.rows[start].address)..m.to_runtime(seq.row_end(end)),
                location,
            });
        }
        None
    }

    /// Statics named `name` (full path or suffix), with the module that holds each.
    pub fn find_statics(&self, name: &str) -> Vec<(ModuleId, DieRef)> {
        let mut out = Vec::new();
        for (id, m) in self.modules() {
            let Ok(dwarf) = m.binary().dwarf() else {
                continue;
            };
            out.extend(dwarf.find_statics(name).into_iter().map(|d| (id, d)));
        }
        out
    }

    /// The concrete subprogram DIE containing `pc`.
    pub fn subprogram_at(&self, pc: u64) -> Option<(ModuleId, DieRef)> {
        let (id, m) = self.module_at(pc)?;
        let dwarf = m.binary().dwarf().ok()?;
        let loc = dwarf.functions_at(m.to_file(pc)).into_iter().next()?;
        Some((id, dwarf.function(&loc)?.offset))
    }
}

/// The innermost inlined copy containing `addr`, else the innermost concrete function.
fn innermost_block(dwarf: &DwarfIndex, addr: u64) -> Option<DieRef> {
    let mut best: Option<(u64, DieRef)> = None;
    for unit in dwarf.units_at(addr) {
        let Ok(ix) = dwarf.unit_index(unit) else {
            continue;
        };
        for inl in &ix.inlined {
            if inl.ranges.iter().any(|r| r.contains(&addr)) {
                let size: u64 = inl.ranges.iter().map(|r| r.end - r.start).sum();
                if best.is_none_or(|(s, _)| size < s) {
                    best = Some((size, inl.offset));
                }
            }
        }
    }
    if let Some((_, d)) = best {
        return Some(d);
    }
    let loc = dwarf.functions_at(addr).into_iter().next()?;
    dwarf.function(&loc).map(|f| f.offset)
}

/// The post-prologue address among `rows` (one function's rows, sorted), entered at `entry`.
pub(crate) fn prologue_end(rows: &[LineRow], entry: u64) -> u64 {
    if let Some(r) = rows.iter().find(|r| r.prologue_end && r.address >= entry) {
        return r.address;
    }
    let first_line = rows.iter().find(|r| r.address == entry).map(|r| r.line);
    match first_line {
        Some(l) => rows
            .iter()
            .find(|r| r.address > entry && r.line != 0 && r.line != l && r.is_stmt)
            .map_or(entry, |r| r.address),
        None => entry,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(address: u64, line: u32, prologue_end: bool) -> LineRow {
        LineRow {
            address,
            file: 0,
            line,
            column: 0,
            is_stmt: true,
            prologue_end,
        }
    }

    #[test]
    fn prologue_end_wins() {
        let rows = [
            row(0x100, 5, false),
            row(0x108, 6, true),
            row(0x110, 7, false),
        ];
        assert_eq!(prologue_end(&rows, 0x100), 0x108);
    }

    #[test]
    fn second_line_rule_without_marker() {
        let rows = [
            row(0x100, 5, false),
            row(0x104, 5, false),
            row(0x10c, 6, false),
        ];
        assert_eq!(prologue_end(&rows, 0x100), 0x10c);
        assert_eq!(prologue_end(&[row(0x100, 5, false)], 0x100), 0x100);
    }
}
