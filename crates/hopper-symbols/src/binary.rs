//! One ELF file: its sections, symbol table, build id and DWARF, read once and shared.
//!
//! Addresses in this module are *file* addresses (the virtual addresses the ELF file names, before
//! the loader adds a bias). [`crate::Module`] adds the load bias.
//!
//! The file is read into memory rather than mapped: `memmap2::Mmap::map` is an `unsafe fn`, and
//! the workspace forbids unsafe code. Every section is a sub-range of that one buffer
//! (`EndianArcSlice`), so nothing is copied twice; compressed sections are decompressed into
//! their own buffers.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use gimli::RunTimeEndian;
use hopper_core::{HopperError, Result};
use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol};

use crate::cfi::CallFrameInfo;
use crate::dwarf::{DwarfIndex, Reader};
use crate::names;

/// The GNU build id of a binary (`NT_GNU_BUILD_ID`).
#[derive(Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BuildId(pub Vec<u8>);

impl BuildId {
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Display for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BuildId({})", self.to_hex())
    }
}

/// How the binary is loaded.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum BinaryKind {
    /// `ET_EXEC`: loaded at its file addresses (bias 0).
    FixedExecutable,
    /// `ET_DYN`: a position-independent executable or a shared library, loaded at a bias.
    Relocatable,
    /// `ET_CORE`.
    Core,
    Other,
}

/// A `PT_LOAD` segment, in file addresses.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct LoadSegment {
    pub vaddr: u64,
    pub memsz: u64,
    pub file_offset: u64,
    pub filesz: u64,
    pub executable: bool,
}

impl LoadSegment {
    pub fn range(&self) -> Range<u64> {
        self.vaddr..self.vaddr + self.memsz
    }
}

/// What a symbol-table entry names.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum SymbolKind {
    Function,
    Data,
}

/// A symbol-table entry (from `.symtab`, or `.dynsym` when there is no `.symtab`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ElfSymbol {
    /// The demangled name without the Rust hash (`values::main`), or the raw name when it is not
    /// mangled.
    pub name: String,
    /// The name as the symbol table spells it.
    pub raw_name: String,
    pub address: u64,
    pub size: u64,
    pub kind: SymbolKind,
}

impl ElfSymbol {
    pub fn range(&self) -> Range<u64> {
        self.address..self.address + self.size.max(1)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct SectionData {
    pub address: u64,
    pub data: Reader,
}

/// One opened ELF file. Cheap to share: modules hold it in an `Arc`.
pub struct Binary {
    path: PathBuf,
    data: Arc<[u8]>,
    endian: RunTimeEndian,
    kind: BinaryKind,
    entry: u64,
    build_id: Option<BuildId>,
    segments: Vec<LoadSegment>,
    sections: HashMap<String, SectionData>,
    /// Function and data symbols, sorted by address.
    symbols: Vec<ElfSymbol>,
    dwarf: OnceLock<std::result::Result<Arc<DwarfIndex>, HopperError>>,
    cfi: OnceLock<CallFrameInfo>,
    /// addr2line's context is not `Sync` (it caches with `OnceCell`), so it sits behind a lock.
    addr2line: OnceLock<Option<Mutex<addr2line::Context<Reader>>>>,
}

impl fmt::Debug for Binary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Binary")
            .field("path", &self.path)
            .field("kind", &self.kind)
            .field("build_id", &self.build_id)
            .field("size", &self.data.len())
            .finish()
    }
}

impl Binary {
    /// Read and parse the ELF file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Binary>> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|e| HopperError::Io(format!("reading {}: {e}", path.display())))?;
        Binary::from_bytes(path.to_path_buf(), data)
    }

    /// Parse an ELF image already in memory. `path` is only recorded.
    pub fn from_bytes(path: PathBuf, data: Vec<u8>) -> Result<Arc<Binary>> {
        let data: Arc<[u8]> = data.into();
        let file = object::File::parse(&*data)
            .map_err(|e| HopperError::Symbols(format!("{}: {e}", path.display())))?;
        if file.format() != object::BinaryFormat::Elf {
            return Err(HopperError::Symbols(format!(
                "{}: not an ELF file",
                path.display()
            )));
        }
        let endian = if file.is_little_endian() {
            RunTimeEndian::Little
        } else {
            RunTimeEndian::Big
        };
        let kind = match file.kind() {
            object::ObjectKind::Executable => BinaryKind::FixedExecutable,
            object::ObjectKind::Dynamic => BinaryKind::Relocatable,
            object::ObjectKind::Core => BinaryKind::Core,
            _ => BinaryKind::Other,
        };
        let build_id = file.build_id().ok().flatten().map(|b| BuildId(b.to_vec()));
        let whole = Reader::new(data.clone(), endian);
        let base = data.as_ptr() as usize;

        let mut sections = HashMap::new();
        for section in file.sections() {
            let Ok(name) = section.name() else { continue };
            if name.is_empty() {
                continue;
            }
            let reader = match section.compressed_data() {
                Ok(compressed) if compressed.format != object::CompressionFormat::None => {
                    match compressed.decompress() {
                        Ok(bytes) => Reader::new(Arc::from(bytes.into_owned()), endian),
                        Err(_) => continue,
                    }
                }
                _ => match section.data() {
                    Ok(bytes) if !bytes.is_empty() => {
                        let start = bytes.as_ptr() as usize - base;
                        whole.range(start..start + bytes.len())
                    }
                    _ => continue,
                },
            };
            sections.insert(
                name.to_string(),
                SectionData {
                    address: section.address(),
                    data: reader,
                },
            );
        }

        let segments = file
            .segments()
            .map(|seg| {
                let (file_offset, filesz) = seg.file_range();
                // Only PT_LOAD segments are reported by `segments()` for ELF.
                LoadSegment {
                    vaddr: seg.address(),
                    memsz: seg.size(),
                    file_offset,
                    filesz,
                    executable: is_executable_segment(&file, seg.address()),
                }
            })
            .collect();

        let mut symbols = Vec::new();
        let mut push = |sym: object::Symbol<'_, '_>| {
            let kind = match sym.kind() {
                object::SymbolKind::Text => SymbolKind::Function,
                object::SymbolKind::Data => SymbolKind::Data,
                _ => return,
            };
            if sym.is_undefined() || sym.address() == 0 {
                return;
            }
            let Ok(raw) = sym.name() else { return };
            if raw.is_empty() {
                return;
            }
            symbols.push(ElfSymbol {
                name: names::demangle(raw),
                raw_name: raw.to_string(),
                address: sym.address(),
                size: sym.size(),
                kind,
            });
        };
        let mut any = false;
        for sym in file.symbols() {
            any = true;
            push(sym);
        }
        if !any {
            for sym in file.dynamic_symbols() {
                push(sym);
            }
        }
        symbols.sort_by(|a, b| a.address.cmp(&b.address).then(b.size.cmp(&a.size)));
        symbols.dedup_by(|b, a| a.address == b.address && a.raw_name == b.raw_name);

        Ok(Arc::new(Binary {
            path,
            entry: file.entry(),
            data: data.clone(),
            endian,
            kind,
            build_id,
            segments,
            sections,
            symbols,
            dwarf: OnceLock::new(),
            cfi: OnceLock::new(),
            addr2line: OnceLock::new(),
        }))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn build_id(&self) -> Option<&BuildId> {
        self.build_id.as_ref()
    }

    pub fn kind(&self) -> BinaryKind {
        self.kind
    }

    /// The entry point (`e_entry`), a file address.
    pub fn entry(&self) -> u64 {
        self.entry
    }

    pub fn segments(&self) -> &[LoadSegment] {
        &self.segments
    }

    /// The lowest `PT_LOAD` address, rounded down to a page. The loader maps the first segment of
    /// the file there plus the bias.
    pub fn load_base(&self) -> u64 {
        self.segments
            .iter()
            .map(|s| s.vaddr & !0xfff)
            .min()
            .unwrap_or(0)
    }

    /// Whether `addr` (a file address) falls inside a loaded segment.
    pub fn contains(&self, addr: u64) -> bool {
        self.segments.iter().any(|s| s.range().contains(&addr))
    }

    /// Whether `addr` (a file address) falls inside an executable segment.
    pub fn contains_code(&self, addr: u64) -> bool {
        self.segments
            .iter()
            .any(|s| s.executable && s.range().contains(&addr))
    }

    pub fn endian(&self) -> RunTimeEndian {
        self.endian
    }

    pub(crate) fn section(&self, name: &str) -> Option<&SectionData> {
        self.sections.get(name)
    }

    /// The raw bytes of the file.
    pub fn bytes(&self) -> &[u8] {
        &self.data
    }

    /// Read `len` bytes at file address `addr` from the file's own segment contents (for
    /// read-only data and code when a core file left them out). Bytes past a segment's file size
    /// read as zero.
    pub fn read_static(&self, addr: u64, buf: &mut [u8]) -> Result<()> {
        let len = buf.len() as u64;
        for seg in &self.segments {
            if addr >= seg.vaddr && addr + len <= seg.vaddr + seg.memsz {
                let off = addr - seg.vaddr;
                for (i, b) in buf.iter_mut().enumerate() {
                    let o = off + i as u64;
                    *b = if o < seg.filesz {
                        self.data
                            .get((seg.file_offset + o) as usize)
                            .copied()
                            .unwrap_or(0)
                    } else {
                        0
                    };
                }
                return Ok(());
            }
        }
        Err(HopperError::UnreadableMemory { addr, len })
    }

    /// Function and data symbols, sorted by address.
    pub fn symbols(&self) -> &[ElfSymbol] {
        &self.symbols
    }

    /// The symbol whose range contains `addr` (file address); for sizeless symbols, the nearest
    /// one at or below `addr` within the same segment.
    pub fn symbol_at(&self, addr: u64) -> Option<&ElfSymbol> {
        let idx = self.symbols.partition_point(|s| s.address <= addr);
        let candidates = self.symbols[..idx].iter().rev();
        let mut fallback = None;
        for sym in candidates.take(64) {
            if sym.size > 0 && addr < sym.address + sym.size {
                return Some(sym);
            }
            if sym.size == 0 && fallback.is_none() && sym.kind == SymbolKind::Function {
                fallback = Some(sym);
            }
            if sym.size > 0 {
                // Sized symbols below `addr` that don't contain it: stop at the first one, since
                // the sorted order means nothing further down can.
                break;
            }
        }
        fallback.filter(|s| self.same_segment(s.address, addr))
    }

    fn same_segment(&self, a: u64, b: u64) -> bool {
        self.segments
            .iter()
            .any(|s| s.range().contains(&a) && s.range().contains(&b))
    }

    /// The DWARF index, built on first use. Fails when the debug information is malformed;
    /// binaries without DWARF get an empty index.
    pub fn dwarf(&self) -> Result<&Arc<DwarfIndex>> {
        self.dwarf
            .get_or_init(|| DwarfIndex::load(self).map(Arc::new))
            .as_ref()
            .map_err(Clone::clone)
    }

    pub(crate) fn cfi(&self) -> &CallFrameInfo {
        self.cfi.get_or_init(|| CallFrameInfo::load(self))
    }

    /// Run `f` with addr2line's context for this binary, when it has DWARF.
    pub(crate) fn with_addr2line<T>(
        &self,
        f: impl FnOnce(&addr2line::Context<Reader>) -> T,
    ) -> Option<T> {
        let slot = self.addr2line.get_or_init(|| {
            let index = self.dwarf().ok()?;
            if index.units().is_empty() {
                return None;
            }
            addr2line::Context::from_arc_dwarf(index.dwarf_arc())
                .ok()
                .map(Mutex::new)
        });
        let ctx = slot.as_ref()?;
        let guard = ctx.lock().unwrap_or_else(|p| p.into_inner());
        Some(f(&guard))
    }
}

fn is_executable_segment(file: &object::File<'_>, vaddr: u64) -> bool {
    use object::read::elf::ProgramHeader;
    match file {
        object::File::Elf64(elf) => {
            let endian = elf.endian();
            elf.elf_program_headers().iter().any(|ph| {
                ph.p_type(endian) == object::elf::PT_LOAD
                    && ph.p_vaddr(endian) == vaddr
                    && ph.p_flags(endian) & object::elf::PF_X != object::elf::ProgramFlags(0)
            })
        }
        object::File::Elf32(elf) => {
            let endian = elf.endian();
            elf.elf_program_headers().iter().any(|ph| {
                ph.p_type(endian) == object::elf::PT_LOAD
                    && u64::from(ph.p_vaddr(endian)) == vaddr
                    && ph.p_flags(endian) & object::elf::PF_X != object::elf::ProgramFlags(0)
            })
        }
        _ => false,
    }
}
