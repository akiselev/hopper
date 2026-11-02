//! The DWARF of one binary: units (parsed eagerly, they are cheap), and per-unit indexes of
//! functions, statics, scopes and line tables (built lazily, on first use of that unit).
//!
//! The lazy per-unit parts follow BugStalker's `UnitLazyPart` idea (MIT, see
//! `THIRD_PARTY_NOTICES.md`); the code here is hopper's own.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::sync::{Arc, OnceLock};

use gimli::{AttributeValue, DwTag, Reader as _, RunTimeEndian, UnitOffset};
use hopper_core::{HopperError, Result};

use crate::binary::Binary;
use crate::lines::LineTable;
use crate::names;

/// The gimli reader every hopper crate uses: a shared slice of the binary's bytes.
pub type Reader = gimli::EndianArcSlice<RunTimeEndian>;

/// A DIE of one binary, named by its `.debug_info` offset.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct DieRef(pub u64);

/// A rustc version, parsed from `DW_AT_producer` (`clang LLVM (rustc version 1.97.0 (...))`).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RustcVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl RustcVersion {
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        RustcVersion {
            major,
            minor,
            patch,
        }
    }

    /// Find `rustc version X.Y.Z` in a producer string.
    pub fn from_producer(producer: &str) -> Option<RustcVersion> {
        let rest = &producer[producer.find("rustc version ")? + "rustc version ".len()..];
        let ver: String = rest
            .chars()
            .take_while(|c| c.is_ascii_digit() || *c == '.')
            .collect();
        let mut it = ver.split('.').map(|p| p.parse::<u32>().ok());
        Some(RustcVersion {
            major: it.next()??,
            minor: it.next()??,
            patch: it.next().flatten().unwrap_or(0),
        })
    }
}

impl fmt::Display for RustcVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// One compilation unit.
pub struct UnitInfo {
    offset: u64,
    end: u64,
    pub unit: gimli::Unit<Reader>,
    pub name: Option<String>,
    pub comp_dir: Option<String>,
    pub language: Option<gimli::DwLang>,
    pub producer: Option<String>,
    pub rustc_version: Option<RustcVersion>,
    index: OnceLock<std::result::Result<UnitIndex, HopperError>>,
    lines: OnceLock<std::result::Result<LineTable, HopperError>>,
}

impl fmt::Debug for UnitInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UnitInfo")
            .field("offset", &self.offset)
            .field("name", &self.name)
            .field("language", &self.language)
            .field("producer", &self.producer)
            .finish()
    }
}

impl UnitInfo {
    /// The unit's `.debug_info` offset.
    pub fn offset(&self) -> u64 {
        self.offset
    }

    pub fn is_rust(&self) -> bool {
        self.language == Some(gimli::DW_LANG_Rust)
    }

    /// The DIE at `die` as an offset inside this unit.
    pub fn unit_offset(&self, die: DieRef) -> UnitOffset<usize> {
        UnitOffset((die.0 - self.offset) as usize)
    }

    pub fn die_ref(&self, offset: UnitOffset<usize>) -> DieRef {
        DieRef(self.offset + offset.0 as u64)
    }
}

/// A subprogram DIE (concrete or abstract).
#[derive(Clone, Debug)]
pub(crate) struct FunctionDie {
    pub offset: DieRef,
    pub name: Option<String>,
    /// File-address ranges (empty for declarations and abstract instances).
    pub ranges: Vec<Range<u64>>,
    /// `DW_AT_abstract_origin` or `DW_AT_specification`.
    pub origin: Option<DieRef>,
    pub decl_file: Option<u64>,
    pub decl_line: Option<u32>,
    pub main_subprogram: bool,
}

/// An inlined call (`DW_TAG_inlined_subroutine`).
#[derive(Clone, Debug)]
pub(crate) struct InlinedDie {
    pub offset: DieRef,
    pub origin: Option<DieRef>,
    pub ranges: Vec<Range<u64>>,
    /// `DW_AT_entry_pc`, else the lowest range start.
    pub entry_pc: Option<u64>,
}

/// A namespace-level variable (a `static`).
#[derive(Clone, Debug)]
pub(crate) struct StaticDie {
    pub offset: DieRef,
    pub name: String,
    pub linkage_name: Option<String>,
    /// The variable's own DIE has a location (a declaration-only DIE does not).
    pub has_location: bool,
    pub specification: Option<DieRef>,
}

/// The lazily built index of one unit.
#[derive(Debug, Default)]
pub(crate) struct UnitIndex {
    pub functions: Vec<FunctionDie>,
    pub by_offset: HashMap<DieRef, usize>,
    pub inlined: Vec<InlinedDie>,
    pub statics: Vec<StaticDie>,
    /// Parent of each scope-like DIE (namespaces, types, subprograms, variables), when that
    /// parent is itself a scope that qualifies names.
    pub parents: HashMap<DieRef, DieRef>,
    /// Names of the DIEs that qualify their children: namespaces, structs, unions, enums.
    pub scope_names: HashMap<DieRef, String>,
}

/// Where a function is, after resolving abstract origins.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct FunctionLoc {
    pub unit: usize,
    pub index: usize,
}

/// Every key the name index files a function under.
#[derive(Debug, Default)]
struct NameIndex {
    /// Full names (DWARF-qualified, generics stripped, demangled linkage) to functions.
    exact: HashMap<String, Vec<FunctionRef>>,
    /// The last path segment (generics stripped) to the full names that end with it.
    by_last: HashMap<String, Vec<String>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum FunctionRef {
    Concrete { unit: usize, index: usize },
    Inlined { unit: usize, index: usize },
}

/// The DWARF of one binary.
pub struct DwarfIndex {
    dwarf: Arc<gimli::Dwarf<Reader>>,
    units: Vec<UnitInfo>,
    /// `(begin, end, unit)` sorted by `begin`.
    ranges: Vec<(u64, u64, usize)>,
    names: OnceLock<NameIndex>,
}

impl fmt::Debug for DwarfIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DwarfIndex")
            .field("units", &self.units.len())
            .finish()
    }
}

impl DwarfIndex {
    pub(crate) fn load(binary: &Binary) -> Result<DwarfIndex> {
        let empty = Reader::new(Arc::from(Vec::new()), binary.endian());
        let sections = gimli::DwarfSections::load(|id| -> std::result::Result<Reader, ()> {
            Ok(binary
                .section(id.name())
                .map(|s| s.data.clone())
                .unwrap_or_else(|| empty.clone()))
        })
        .map_err(|()| HopperError::Symbols("loading DWARF sections".into()))?;
        let mut dwarf = sections.borrow(|r| r.clone());
        dwarf.populate_abbreviations_cache(gimli::AbbreviationsCacheStrategy::Duplicates);
        let dwarf = Arc::new(dwarf);

        let mut units = Vec::new();
        let mut headers = dwarf.units();
        while let Some(header) = headers.next().map_err(sym_err)? {
            let Some(offset) = header.debug_info_offset() else {
                continue;
            };
            let end = offset.0 as u64 + header.length_including_self() as u64;
            let unit = match dwarf.unit(header) {
                Ok(u) => u,
                Err(_) => continue,
            };
            let name = unit.name.as_ref().map(reader_string);
            let comp_dir = unit.comp_dir.as_ref().map(reader_string);
            let (language, producer) = {
                let root = unit.entry(UnitOffset(unit.header.header_size())).ok();
                let lang = root
                    .as_ref()
                    .and_then(|e| match e.attr_value(gimli::DW_AT_language) {
                        Some(AttributeValue::Language(l)) => Some(l),
                        _ => None,
                    });
                let producer = root
                    .as_ref()
                    .and_then(|e| e.attr_value(gimli::DW_AT_producer))
                    .and_then(|v| dwarf.attr_string(&unit, v).ok())
                    .map(|r| reader_string(&r));
                (lang, producer)
            };
            let rustc_version = producer.as_deref().and_then(RustcVersion::from_producer);
            units.push(UnitInfo {
                offset: offset.0 as u64,
                end,
                unit,
                name,
                comp_dir,
                language,
                producer,
                rustc_version,
                index: OnceLock::new(),
                lines: OnceLock::new(),
            });
        }
        units.sort_by_key(|u| u.offset);

        let mut ranges = Vec::new();
        for (i, u) in units.iter().enumerate() {
            if let Ok(mut iter) = dwarf.unit_ranges(&u.unit) {
                while let Ok(Some(r)) = iter.next() {
                    if r.begin < r.end {
                        ranges.push((r.begin, r.end, i));
                    }
                }
            }
        }
        ranges.sort();
        Ok(DwarfIndex {
            dwarf,
            units,
            ranges,
            names: OnceLock::new(),
        })
    }

    pub fn dwarf(&self) -> &gimli::Dwarf<Reader> {
        &self.dwarf
    }

    pub(crate) fn dwarf_arc(&self) -> Arc<gimli::Dwarf<Reader>> {
        self.dwarf.clone()
    }

    pub fn units(&self) -> &[UnitInfo] {
        &self.units
    }

    /// The unit that contains `die`.
    pub fn unit_of(&self, die: DieRef) -> Result<&UnitInfo> {
        let idx = self.units.partition_point(|u| u.offset <= die.0);
        idx.checked_sub(1)
            .map(|i| &self.units[i])
            .filter(|u| die.0 < u.end)
            .ok_or_else(|| HopperError::Symbols(format!("no unit contains DIE {:#x}", die.0)))
    }

    pub(crate) fn unit_index_of(&self, die: DieRef) -> Result<usize> {
        let u = self.unit_of(die)?;
        Ok(self.units.partition_point(|x| x.offset < u.offset))
    }

    /// Read the DIE at `die`.
    pub fn entry(
        &self,
        die: DieRef,
    ) -> Result<(&UnitInfo, gimli::DebuggingInformationEntry<Reader>)> {
        let unit = self.unit_of(die)?;
        let entry = unit.unit.entry(unit.unit_offset(die)).map_err(sym_err)?;
        Ok((unit, entry))
    }

    /// Units whose address ranges contain `addr` (a file address).
    pub(crate) fn units_at(&self, addr: u64) -> impl Iterator<Item = usize> + '_ {
        let end = self.ranges.partition_point(|r| r.0 <= addr);
        let mut seen = Vec::new();
        self.ranges[..end]
            .iter()
            .rev()
            .filter_map(move |&(b, e, u)| {
                if b <= addr && addr < e && !seen.contains(&u) {
                    seen.push(u);
                    Some(u)
                } else {
                    None
                }
            })
    }

    pub(crate) fn unit_index(&self, unit: usize) -> Result<&UnitIndex> {
        let u = &self.units[unit];
        u.index
            .get_or_init(|| build_unit_index(&self.dwarf, u))
            .as_ref()
            .map_err(Clone::clone)
    }

    pub(crate) fn line_table(&self, unit: usize) -> Result<&LineTable> {
        let u = &self.units[unit];
        u.lines
            .get_or_init(|| LineTable::load(&self.dwarf, u))
            .as_ref()
            .map_err(Clone::clone)
    }

    /// Resolve a reference-class attribute to a `DieRef`.
    pub fn attr_ref(&self, unit: &UnitInfo, value: AttributeValue<Reader>) -> Option<DieRef> {
        match value {
            AttributeValue::UnitRef(off) => Some(unit.die_ref(off)),
            AttributeValue::DebugInfoRef(off) => Some(DieRef(off.0 as u64)),
            _ => None,
        }
    }

    /// A string-class attribute of `entry`.
    pub fn attr_str(
        &self,
        unit: &UnitInfo,
        entry: &gimli::DebuggingInformationEntry<Reader>,
        at: gimli::DwAt,
    ) -> Option<String> {
        let v = entry.attr_value(at)?;
        self.dwarf
            .attr_string(&unit.unit, v)
            .ok()
            .map(|r| reader_string(&r))
    }

    /// The fully qualified name of a DIE: its enclosing namespaces and types joined with `::`
    /// (`core::option::Option<u32>`, `values::stop_here`). Follows `DW_AT_specification` and
    /// `DW_AT_abstract_origin` for DIEs without their own name.
    pub fn qualified_name(&self, die: DieRef) -> Option<String> {
        let mut die = die;
        for _ in 0..8 {
            let (unit, entry) = self.entry(die).ok()?;
            if let Some(name) = self.attr_str(unit, &entry, gimli::DW_AT_name) {
                let prefix = self.scope_prefix(die);
                return Some(match prefix {
                    Some(p) => format!("{p}::{name}"),
                    None => name,
                });
            }
            let next = entry
                .attr_value(gimli::DW_AT_specification)
                .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin))
                .and_then(|v| self.attr_ref(unit, v))?;
            die = next;
        }
        None
    }

    /// The `a::b` prefix that qualifies the name of `die`, from its enclosing scopes.
    pub fn scope_prefix(&self, die: DieRef) -> Option<String> {
        let unit = self.unit_index_of(die).ok()?;
        let index = self.unit_index(unit).ok()?;
        let mut parts = Vec::new();
        let mut cur = die;
        while let Some(&parent) = index.parents.get(&cur) {
            match index.scope_names.get(&parent) {
                Some(name) => parts.push(name.as_str()),
                None => break,
            }
            cur = parent;
        }
        if parts.is_empty() {
            return None;
        }
        parts.reverse();
        Some(parts.join("::"))
    }

    /// The display name of a subprogram or inlined instance: its qualified DWARF name, else its
    /// demangled linkage name.
    pub(crate) fn function_name(&self, die: DieRef) -> Option<String> {
        if let Some(n) = self.qualified_name(die) {
            return Some(n);
        }
        self.linkage_name(die).map(|l| names::demangle(&l))
    }

    /// `DW_AT_linkage_name`, following origins.
    pub fn linkage_name(&self, die: DieRef) -> Option<String> {
        let mut die = die;
        for _ in 0..8 {
            let (unit, entry) = self.entry(die).ok()?;
            if let Some(l) = self
                .attr_str(unit, &entry, gimli::DW_AT_linkage_name)
                .or_else(|| self.attr_str(unit, &entry, gimli::DW_AT_MIPS_linkage_name))
            {
                return Some(l);
            }
            die = entry
                .attr_value(gimli::DW_AT_specification)
                .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin))
                .and_then(|v| self.attr_ref(unit, v))?;
        }
        None
    }

    /// Concrete subprograms whose ranges contain `addr`, innermost (smallest) first.
    pub(crate) fn functions_at(&self, addr: u64) -> Vec<FunctionLoc> {
        let mut out = Vec::new();
        for unit in self.units_at(addr) {
            let Ok(index) = self.unit_index(unit) else {
                continue;
            };
            for (i, f) in index.functions.iter().enumerate() {
                if f.ranges.iter().any(|r| r.contains(&addr)) {
                    out.push(FunctionLoc { unit, index: i });
                }
            }
            if !out.is_empty() {
                break;
            }
        }
        out.sort_by_key(|l| {
            let f = &self.unit_index(l.unit).expect("indexed above").functions[l.index];
            f.ranges.iter().map(|r| r.end - r.start).sum::<u64>()
        });
        out
    }

    pub(crate) fn function(&self, loc: &FunctionLoc) -> Option<&FunctionDie> {
        self.unit_index(loc.unit).ok()?.functions.get(loc.index)
    }

    /// Functions (concrete or inlined instances) filed under `query`: exact names first; when
    /// nothing matches exactly, names that end with `query` on a `::` boundary.
    pub(crate) fn find_functions(&self, query: &str) -> Vec<FunctionRef> {
        let names = self.names.get_or_init(|| self.build_name_index());
        let query_stripped = names::strip_generics(query);
        let mut found: Vec<FunctionRef> = Vec::new();
        fn add(found: &mut Vec<FunctionRef>, refs: &[FunctionRef]) {
            for r in refs {
                if !found.contains(r) {
                    found.push(*r);
                }
            }
        }
        if let Some(refs) = names.exact.get(query) {
            add(&mut found, refs);
        }
        if let Some(refs) = names.exact.get(&query_stripped) {
            add(&mut found, refs);
        }
        if !found.is_empty() {
            return found;
        }
        let last = last_segment(&query_stripped);
        if let Some(full) = names.by_last.get(last) {
            for name in full {
                if names::path_suffix_match(name, query)
                    || names::path_suffix_match(name, &query_stripped)
                {
                    add(&mut found, &names.exact[name]);
                }
            }
        }
        found
    }

    pub(crate) fn main_subprograms(&self) -> Vec<FunctionRef> {
        let names = self.names.get_or_init(|| self.build_name_index());
        let mut out = Vec::new();
        for refs in names.exact.values() {
            for r in refs {
                if let FunctionRef::Concrete { unit, index } = *r
                    && self
                        .unit_index(unit)
                        .is_ok_and(|ix| ix.functions[index].main_subprogram)
                    && !out.contains(r)
                {
                    out.push(*r);
                }
            }
        }
        out
    }

    fn build_name_index(&self) -> NameIndex {
        let mut ix = NameIndex::default();
        let mut file = |name: String, r: FunctionRef| {
            let stripped = names::strip_generics(&name);
            for key in [name, stripped] {
                let entry = ix.exact.entry(key.clone()).or_default();
                if !entry.contains(&r) {
                    entry.push(r);
                }
                let last = last_segment(&names::strip_generics(&key)).to_string();
                let list = ix.by_last.entry(last).or_default();
                if !list.contains(&key) {
                    list.push(key);
                }
            }
        };
        for unit in 0..self.units.len() {
            let Ok(index) = self.unit_index(unit) else {
                continue;
            };
            for (i, f) in index.functions.iter().enumerate() {
                if f.ranges.is_empty() {
                    continue;
                }
                let r = FunctionRef::Concrete { unit, index: i };
                if let Some(n) = self.qualified_name(f.offset) {
                    file(n, r);
                }
                if let Some(l) = self.linkage_name(f.offset) {
                    file(names::demangle(&l), r);
                }
            }
            for (i, inl) in index.inlined.iter().enumerate() {
                let Some(origin) = inl.origin else { continue };
                let r = FunctionRef::Inlined { unit, index: i };
                if let Some(n) = self.qualified_name(origin) {
                    file(n, r);
                }
            }
        }
        ix
    }

    /// Statics whose qualified name is `query` or ends with it on a `::` boundary. Returns the
    /// DIE that carries the location.
    pub fn find_statics(&self, query: &str) -> Vec<DieRef> {
        let mut exact = Vec::new();
        let mut suffix = Vec::new();
        for unit in 0..self.units.len() {
            let Ok(index) = self.unit_index(unit) else {
                continue;
            };
            for s in &index.statics {
                if !s.has_location {
                    continue;
                }
                if s.name != last_segment(query) && s.linkage_name.is_none() {
                    continue;
                }
                let named = s.specification.unwrap_or(s.offset);
                let full = self.qualified_name(named).unwrap_or_else(|| s.name.clone());
                if full == query {
                    exact.push(s.offset);
                } else if names::path_suffix_match(&full, query) {
                    suffix.push(s.offset);
                } else if let Some(l) = &s.linkage_name
                    && names::demangle(l) == query
                {
                    exact.push(s.offset);
                }
            }
        }
        if exact.is_empty() { suffix } else { exact }
    }
}

fn last_segment(name: &str) -> &str {
    name.rsplit("::").next().unwrap_or(name)
}

/// A gimli string as UTF-8, replacing invalid bytes.
pub fn reader_string(r: &Reader) -> String {
    r.to_string_lossy()
        .map(|c| c.into_owned())
        .unwrap_or_default()
}

pub(crate) fn sym_err(e: gimli::Error) -> HopperError {
    HopperError::Symbols(e.to_string())
}

/// Tags whose name qualifies the names of their children.
fn qualifies_children(tag: DwTag) -> bool {
    matches!(
        tag,
        gimli::DW_TAG_namespace
            | gimli::DW_TAG_structure_type
            | gimli::DW_TAG_union_type
            | gimli::DW_TAG_enumeration_type
            | gimli::DW_TAG_class_type
    )
}

/// Tags whose parent is worth recording for name qualification.
fn records_parent(tag: DwTag) -> bool {
    qualifies_children(tag)
        || matches!(
            tag,
            gimli::DW_TAG_subprogram
                | gimli::DW_TAG_variable
                | gimli::DW_TAG_typedef
                | gimli::DW_TAG_base_type
                | gimli::DW_TAG_pointer_type
                | gimli::DW_TAG_array_type
                | gimli::DW_TAG_subroutine_type
        )
}

pub(crate) fn die_ranges(
    dwarf: &gimli::Dwarf<Reader>,
    unit: &gimli::Unit<Reader>,
    entry: &gimli::DebuggingInformationEntry<Reader>,
) -> Vec<Range<u64>> {
    let mut out = Vec::new();
    if let Ok(mut iter) = dwarf.die_ranges(unit, entry) {
        while let Ok(Some(r)) = iter.next() {
            if r.begin < r.end {
                out.push(r.begin..r.end);
            }
        }
    }
    out
}

fn build_unit_index(
    dwarf: &gimli::Dwarf<Reader>,
    info: &UnitInfo,
) -> std::result::Result<UnitIndex, HopperError> {
    let unit = &info.unit;
    let mut ix = UnitIndex::default();
    // (depth, die, tag) of the open ancestors.
    let mut stack: Vec<(isize, DieRef, DwTag)> = Vec::new();
    let mut cursor = unit.entries();
    let reference = |v: AttributeValue<Reader>| match v {
        AttributeValue::UnitRef(off) => Some(info.die_ref(off)),
        AttributeValue::DebugInfoRef(off) => Some(DieRef(off.0 as u64)),
        _ => None,
    };
    let string = |v: Option<AttributeValue<Reader>>| {
        v.and_then(|v| dwarf.attr_string(unit, v).ok())
            .map(|r| reader_string(&r))
    };
    while let Some(entry) = cursor.next_dfs().map_err(sym_err)? {
        let depth = entry.depth();
        while stack.last().is_some_and(|(d, _, _)| *d >= depth) {
            stack.pop();
        }
        let die = info.die_ref(entry.offset());
        let tag = entry.tag();
        let parent = stack.last().copied();
        if records_parent(tag)
            && let Some((_, p, ptag)) = parent
            && (qualifies_children(ptag) || ptag == gimli::DW_TAG_subprogram)
        {
            ix.parents.insert(die, p);
        }
        if qualifies_children(tag)
            && let Some(name) = string(entry.attr_value(gimli::DW_AT_name))
        {
            ix.scope_names.insert(die, name);
        }
        let in_function = stack.iter().any(|(_, _, t)| *t == gimli::DW_TAG_subprogram);
        match tag {
            gimli::DW_TAG_subprogram => {
                let origin = entry
                    .attr_value(gimli::DW_AT_specification)
                    .or_else(|| entry.attr_value(gimli::DW_AT_abstract_origin))
                    .and_then(reference);
                let f = FunctionDie {
                    offset: die,
                    name: string(entry.attr_value(gimli::DW_AT_name)),
                    ranges: die_ranges(dwarf, unit, entry),
                    origin,
                    decl_file: entry
                        .attr_value(gimli::DW_AT_decl_file)
                        .and_then(|v| match v {
                            AttributeValue::FileIndex(i) => Some(i),
                            other => other.udata_value(),
                        }),
                    decl_line: entry
                        .attr_value(gimli::DW_AT_decl_line)
                        .and_then(|v| v.udata_value())
                        .map(|l| l as u32),
                    main_subprogram: matches!(
                        entry.attr_value(gimli::DW_AT_main_subprogram),
                        Some(AttributeValue::Flag(true))
                    ),
                };
                ix.by_offset.insert(die, ix.functions.len());
                ix.functions.push(f);
            }
            gimli::DW_TAG_inlined_subroutine => {
                let ranges = die_ranges(dwarf, unit, entry);
                let entry_pc = entry
                    .attr_value(gimli::DW_AT_entry_pc)
                    .and_then(|v| dwarf.attr_address(unit, v).ok().flatten())
                    .or_else(|| ranges.iter().map(|r| r.start).min());
                ix.inlined.push(InlinedDie {
                    offset: die,
                    origin: entry
                        .attr_value(gimli::DW_AT_abstract_origin)
                        .and_then(reference),
                    ranges,
                    entry_pc,
                });
            }
            gimli::DW_TAG_variable if !in_function => {
                let specification = entry
                    .attr_value(gimli::DW_AT_specification)
                    .and_then(reference);
                let name = string(entry.attr_value(gimli::DW_AT_name));
                let has_location = entry.attr(gimli::DW_AT_location).is_some();
                // A definition that points at its declaration has no name of its own.
                if let Some(name) = name.or_else(|| specification.map(|_| String::new())) {
                    ix.statics.push(StaticDie {
                        offset: die,
                        name,
                        linkage_name: string(entry.attr_value(gimli::DW_AT_linkage_name)),
                        has_location,
                        specification,
                    });
                }
            }
            _ => {}
        }
        if entry.has_children() {
            stack.push((depth, die, tag));
        }
    }
    Ok(ix)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rustc_producer() {
        let p = "clang LLVM (rustc version 1.97.0 (2d8144b78 2026-07-07))";
        assert_eq!(
            RustcVersion::from_producer(p),
            Some(RustcVersion::new(1, 97, 0))
        );
        assert_eq!(
            RustcVersion::from_producer(
                "clang LLVM (rustc version 1.96.0-nightly (abc 2026-05-01))"
            ),
            Some(RustcVersion::new(1, 96, 0))
        );
        assert_eq!(RustcVersion::from_producer("GNU C17 13.2.0"), None);
        assert!(RustcVersion::new(1, 95, 0) < RustcVersion::new(1, 97, 0));
    }
}
