//! ELF core files (x86-64 Linux): memory from the `PT_LOAD` segments, threads and registers from
//! `NT_PRSTATUS` and `NT_FPREGSET`, mapped files from `NT_FILE` and the auxiliary vector from
//! `NT_AUXV`. Good for post-mortem debugging, and the main test harness for symbols and values.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use hopper_core::target::Memory;
use hopper_core::{HopperError, Reg, RegisterSet, Result, Tid};
use object::read::elf::{ElfFile64, FileHeader, ProgramHeader};
use object::{Endianness, elf};

use crate::binary::Binary;
use crate::module::{Module, SymbolSet};

/// A `PT_LOAD` segment of the core.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct CoreSegment {
    pub vaddr: u64,
    pub memsz: u64,
    pub file_offset: u64,
    pub filesz: u64,
}

/// A thread at the moment the core was written.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CoreThread {
    pub tid: Tid,
    /// The signal the thread was stopped by (`pr_cursig`), 0 for none.
    pub signal: u16,
    pub regs: RegisterSet,
}

/// A file mapping from `NT_FILE`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MappedFile {
    pub start: u64,
    pub end: u64,
    /// Offset in the file, in bytes.
    pub file_offset: u64,
    pub path: String,
}

/// A parsed core file.
#[derive(Debug)]
pub struct CoreFile {
    path: PathBuf,
    data: Arc<[u8]>,
    segments: Vec<CoreSegment>,
    threads: Vec<CoreThread>,
    mapped: Vec<MappedFile>,
    auxv: Vec<u8>,
    /// `pr_fname` from `NT_PRPSINFO`: the first 15 bytes of the command name.
    command: Option<String>,
}

const NT_PRSTATUS: u32 = 1;
const NT_FPREGSET: u32 = 2;
const NT_PRPSINFO: u32 = 3;
const NT_AUXV: u32 = 6;
const NT_FILE: u32 = 0x4649_4c45;

/// Offsets inside `struct elf_prstatus` on x86-64.
const PR_CURSIG: usize = 12;
const PR_PID: usize = 32;
const PR_REG: usize = 112;
/// `struct user_regs_struct`, in order, as DWARF register numbers (`None` for `orig_rax`).
const USER_REGS: [Option<Reg>; 27] = [
    Some(Reg(15)),
    Some(Reg(14)),
    Some(Reg(13)),
    Some(Reg(12)),
    Some(Reg::RBP),
    Some(Reg::RBX),
    Some(Reg(11)),
    Some(Reg(10)),
    Some(Reg(9)),
    Some(Reg::R8),
    Some(Reg::RAX),
    Some(Reg::RCX),
    Some(Reg::RDX),
    Some(Reg::RSI),
    Some(Reg::RDI),
    None,
    Some(Reg::RIP),
    Some(Reg::CS),
    Some(Reg::RFLAGS),
    Some(Reg::RSP),
    Some(Reg::SS),
    Some(Reg::FS_BASE),
    Some(Reg::GS_BASE),
    Some(Reg::DS),
    Some(Reg::ES),
    Some(Reg::FS),
    Some(Reg::GS),
];
/// Offset of `xmm_space` in `struct user_fpregs_struct`.
const FP_XMM: usize = 160;

impl CoreFile {
    pub fn open(path: impl AsRef<Path>) -> Result<CoreFile> {
        let path = path.as_ref();
        let data = std::fs::read(path)
            .map_err(|e| HopperError::Io(format!("reading {}: {e}", path.display())))?;
        CoreFile::parse(path.to_path_buf(), data)
    }

    pub fn parse(path: PathBuf, data: Vec<u8>) -> Result<CoreFile> {
        let data: Arc<[u8]> = data.into();
        let bad = |e: object::Error| HopperError::Symbols(format!("{}: {e}", path.display()));
        let elf = ElfFile64::<Endianness>::parse(&*data).map_err(bad)?;
        let endian = elf.endian();
        if elf.elf_header().e_type(endian) != elf::ET_CORE {
            return Err(HopperError::Symbols(format!("{}: not a core file", path.display())));
        }
        if elf.elf_header().e_machine(endian) != elf::EM_X86_64 {
            return Err(HopperError::Unsupported("core files of machines other than x86-64".into()));
        }
        let mut segments = Vec::new();
        let mut threads: Vec<CoreThread> = Vec::new();
        let mut mapped = Vec::new();
        let mut auxv = Vec::new();
        let mut command = None;
        for ph in elf.elf_program_headers() {
            match ph.p_type(endian) {
                elf::PT_LOAD => segments.push(CoreSegment {
                    vaddr: ph.p_vaddr(endian),
                    memsz: ph.p_memsz(endian),
                    file_offset: ph.p_offset(endian),
                    filesz: ph.p_filesz(endian),
                }),
                elf::PT_NOTE => {
                    let Ok(Some(mut notes)) = ph.notes(endian, &*data) else { continue };
                    while let Ok(Some(note)) = notes.next() {
                        let desc = note.desc();
                        match note.n_type(endian).0 {
                            NT_PRSTATUS if note.name() == b"CORE" => {
                                if let Some(t) = parse_prstatus(desc) {
                                    threads.push(t);
                                }
                            }
                            NT_FPREGSET if note.name() == b"CORE" => {
                                if let Some(t) = threads.last_mut() {
                                    add_fpregs(&mut t.regs, desc);
                                }
                            }
                            NT_PRPSINFO if desc.len() >= 56 => {
                                let name = &desc[40..56];
                                let end = name.iter().position(|b| *b == 0).unwrap_or(16);
                                command = Some(String::from_utf8_lossy(&name[..end]).into_owned());
                            }
                            NT_AUXV => auxv = desc.to_vec(),
                            NT_FILE => mapped = parse_nt_file(desc),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        segments.sort_by_key(|s| s.vaddr);
        Ok(CoreFile { path, data, segments, threads, mapped, auxv, command })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn segments(&self) -> &[CoreSegment] {
        &self.segments
    }

    /// The threads, in note order (the thread that dumped the core first, as the kernel and
    /// gdb write them).
    pub fn threads(&self) -> &[CoreThread] {
        &self.threads
    }

    pub fn thread(&self, tid: Tid) -> Option<&CoreThread> {
        self.threads.iter().find(|t| t.tid == tid)
    }

    pub fn mapped_files(&self) -> &[MappedFile] {
        &self.mapped
    }

    /// The raw auxiliary vector.
    pub fn auxv(&self) -> &[u8] {
        &self.auxv
    }

    pub fn command(&self) -> Option<&str> {
        self.command.as_deref()
    }

    /// The mapped file that holds the program's entry point: the executable.
    pub fn executable(&self) -> Option<&MappedFile> {
        let entry = crate::auxv::Auxv::parse(&self.auxv).entry()?;
        self.mapped.iter().find(|m| m.start <= entry && entry < m.end)
    }

    /// The core's memory. Ranges the core left out (file-backed read-only mappings) are read from
    /// the mapped files when they still exist on disk.
    pub fn memory(&self) -> CoreMemory<'_> {
        CoreMemory { core: self, files: HashMap::new() }
    }

    /// A symbol set with a module for every mapped ELF file that can be opened: the executable
    /// first, then the shared libraries in address order. Files that are gone are skipped.
    pub fn symbol_set(&self) -> SymbolSet {
        let mut set = SymbolSet::new();
        let mut done: Vec<&str> = Vec::new();
        let exe = self.executable().map(|m| m.path.as_str());
        let mut order: Vec<&MappedFile> = self.mapped.iter().collect();
        order.sort_by_key(|m| (Some(m.path.as_str()) != exe, m.start));
        for m in order {
            if done.contains(&m.path.as_str()) {
                continue;
            }
            done.push(&m.path);
            // The lowest mapping of the file places it.
            let Some(first) = self.mapped.iter().filter(|x| x.path == m.path).min_by_key(|x| x.start)
            else {
                continue;
            };
            let Ok(binary) = Binary::open(&first.path) else { continue };
            set.add(Module::from_mapping(binary, first.start, first.file_offset));
        }
        set
    }

    fn segment_bytes(&self, seg: &CoreSegment) -> &[u8] {
        let start = seg.file_offset as usize;
        let end = (seg.file_offset + seg.filesz) as usize;
        self.data.get(start..end.min(self.data.len())).unwrap_or(&[])
    }
}

/// Memory of a core file.
#[derive(Debug)]
pub struct CoreMemory<'a> {
    core: &'a CoreFile,
    files: HashMap<String, Option<Arc<[u8]>>>,
}

impl CoreMemory<'_> {
    /// Copy the bytes at `addr` into `buf`; returns how many leading bytes were found.
    fn read_some(&mut self, addr: u64, buf: &mut [u8]) -> usize {
        for seg in &self.core.segments {
            if addr < seg.vaddr || addr >= seg.vaddr + seg.memsz {
                continue;
            }
            let off = addr - seg.vaddr;
            let avail = (seg.vaddr + seg.memsz - addr) as usize;
            let n = avail.min(buf.len());
            if off < seg.filesz {
                let bytes = self.core.segment_bytes(seg);
                let from_core = ((seg.filesz - off) as usize).min(n).min(bytes.len() - off as usize);
                buf[..from_core].copy_from_slice(&bytes[off as usize..off as usize + from_core]);
                return from_core;
            }
            // The core left this part out: read it from the mapped file.
            return self.read_mapped(addr, &mut buf[..n]);
        }
        0
    }

    fn read_mapped(&mut self, addr: u64, buf: &mut [u8]) -> usize {
        let Some(m) = self.core.mapped.iter().find(|m| m.start <= addr && addr < m.end) else {
            return 0;
        };
        let data = self
            .files
            .entry(m.path.clone())
            .or_insert_with(|| std::fs::read(&m.path).ok().map(Arc::from))
            .clone();
        let Some(data) = data else { return 0 };
        let off = (m.file_offset + (addr - m.start)) as usize;
        let n = buf.len().min((m.end - addr) as usize);
        let Some(src) = data.get(off..) else { return 0 };
        let n = n.min(src.len());
        buf[..n].copy_from_slice(&src[..n]);
        n
    }
}

impl Memory for CoreMemory<'_> {
    fn read(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        let mut done = 0;
        while done < buf.len() {
            let n = self.read_some(addr + done as u64, &mut buf[done..]);
            if n == 0 {
                return Err(HopperError::UnreadableMemory {
                    addr: addr + done as u64,
                    len: (buf.len() - done) as u64,
                });
            }
            done += n;
        }
        Ok(())
    }
}

fn parse_prstatus(desc: &[u8]) -> Option<CoreThread> {
    if desc.len() < PR_REG + USER_REGS.len() * 8 {
        return None;
    }
    let u64_at = |o: usize| u64::from_le_bytes(desc[o..o + 8].try_into().expect("8 bytes"));
    let signal = u16::from_le_bytes([desc[PR_CURSIG], desc[PR_CURSIG + 1]]);
    let tid = Tid(i32::from_le_bytes(desc[PR_PID..PR_PID + 4].try_into().expect("4 bytes")));
    let mut regs = RegisterSet::default();
    for (i, reg) in USER_REGS.iter().enumerate() {
        if let Some(r) = reg {
            regs.set(*r, u64_at(PR_REG + i * 8));
        }
    }
    regs.raw = desc[PR_REG..PR_REG + USER_REGS.len() * 8].to_vec();
    Some(CoreThread { tid, signal, regs })
}

fn add_fpregs(regs: &mut RegisterSet, desc: &[u8]) {
    for i in 0..16 {
        let o = FP_XMM + i * 16;
        if let Some(b) = desc.get(o..o + 16) {
            regs.set_bytes(Reg(Reg::XMM0.0 + i as u16), b.to_vec());
        }
    }
}

fn parse_nt_file(desc: &[u8]) -> Vec<MappedFile> {
    let u64_at = |o: usize| desc.get(o..o + 8).map(|b| u64::from_le_bytes(b.try_into().expect("8")));
    let (Some(count), Some(page)) = (u64_at(0), u64_at(8)) else { return Vec::new() };
    let count = count as usize;
    let names_start = 16 + count * 24;
    let Some(names) = desc.get(names_start..) else { return Vec::new() };
    let mut names = names.split(|b| *b == 0);
    let mut out = Vec::new();
    for i in 0..count {
        let base = 16 + i * 24;
        let (Some(start), Some(end), Some(ofs)) = (u64_at(base), u64_at(base + 8), u64_at(base + 16))
        else {
            break;
        };
        let Some(name) = names.next() else { break };
        out.push(MappedFile {
            start,
            end,
            file_offset: ofs * page,
            path: String::from_utf8_lossy(name).into_owned(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nt_file() {
        let mut d = Vec::new();
        for v in [2u64, 0x1000, 0x1000, 0x2000, 0, 0x3000, 0x4000, 2] {
            d.extend_from_slice(&v.to_le_bytes());
        }
        d.extend_from_slice(b"/bin/a\0/lib/b.so\0");
        let files = parse_nt_file(&d);
        assert_eq!(files.len(), 2);
        assert_eq!(files[1].path, "/lib/b.so");
        assert_eq!(files[1].file_offset, 0x2000);
    }

    #[test]
    fn parses_prstatus_registers() {
        let mut d = vec![0u8; 336];
        d[PR_CURSIG] = 5;
        d[PR_PID..PR_PID + 4].copy_from_slice(&1234i32.to_le_bytes());
        // rip is index 16, rsp index 19.
        d[PR_REG + 16 * 8..PR_REG + 17 * 8].copy_from_slice(&0x4010u64.to_le_bytes());
        d[PR_REG + 19 * 8..PR_REG + 20 * 8].copy_from_slice(&0x7ff0u64.to_le_bytes());
        let t = parse_prstatus(&d).unwrap();
        assert_eq!(t.tid, Tid(1234));
        assert_eq!(t.signal, 5);
        assert_eq!(t.regs.pc(), Some(0x4010));
        assert_eq!(t.regs.sp(), Some(0x7ff0));
    }
}
