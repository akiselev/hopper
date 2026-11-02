//! The symbols of one process at one moment.
//!
//! The main executable is placed by the auxiliary vector ([`Module::main_from_auxv`]). Shared
//! libraries come from the dynamic linker's list, read from the process's memory: the main
//! executable's `DT_DEBUG` entry points at `struct r_debug`, whose `r_map` heads the
//! `struct link_map` list (`l_addr`, `l_name`, `l_ld`, `l_next`, ...; glibc's `<link.h>`). Before
//! the dynamic linker has run, `DT_DEBUG` is 0; then the interpreter alone is placed by
//! `AT_BASE`.
//!
//! A [`SymbolSet`] is built once per distinct list of loaded objects and shared; binaries are
//! opened once per path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use hopper_core::target::Memory;
use hopper_core::{HopperError, Result};
use hopper_symbols::{Binary, Module, SymbolSet};
use object::{Object, ObjectSection, ObjectSegment};

const DT_NULL: u64 = 0;
const DT_DEBUG: u64 = 21;
const AT_BASE: u64 = 7;
/// The longest link map hopper follows (a corrupt list must not loop forever).
const MAX_LINK_MAP: usize = 4096;

/// Facts about an ELF file the symbol crate does not expose: where its dynamic section and
/// interpreter name are, and which address ranges are read-only.
#[derive(Clone, Debug, Default)]
pub struct ElfFacts {
    /// File address of `.dynamic`.
    pub dynamic: Option<u64>,
    /// The `PT_INTERP` path.
    pub interpreter: Option<String>,
    /// File-address ranges of loaded segments that are not writable.
    pub read_only: Vec<std::ops::Range<u64>>,
}

impl ElfFacts {
    pub fn of(bytes: &[u8]) -> ElfFacts {
        let Ok(file) = object::File::parse(bytes) else {
            return ElfFacts::default();
        };
        let dynamic = file.section_by_name(".dynamic").map(|s| s.address());
        let interpreter = file
            .section_by_name(".interp")
            .and_then(|s| s.data().ok())
            .map(|d| String::from_utf8_lossy(d.split(|b| *b == 0).next().unwrap_or(d)).into_owned())
            .filter(|s| !s.is_empty());
        let mut read_only = Vec::new();
        for seg in file.segments() {
            let writable = match seg.flags() {
                object::SegmentFlags::Elf { p_flags, .. } => p_flags.contains(object::elf::PF_W),
                _ => true,
            };
            if !writable {
                read_only.push(seg.address()..seg.address() + seg.size());
            }
        }
        ElfFacts {
            dynamic,
            interpreter,
            read_only,
        }
    }
}

/// One object on the dynamic linker's list.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LoadedObject {
    pub path: PathBuf,
    /// `l_addr`: the difference between runtime and file addresses.
    pub bias: u64,
}

/// A binary opened once, with its facts; `None` when it could not be read.
type Opened = Option<(Arc<Binary>, Arc<ElfFacts>)>;

/// Every process symbol set the session has built.
#[derive(Debug, Default)]
pub struct SymbolCache {
    binaries: Mutex<HashMap<PathBuf, Opened>>,
    sets: Mutex<HashMap<Vec<LoadedObject>, Arc<ProcessSymbols>>>,
}

/// The symbols of a process plus what the engine needs beside them.
#[derive(Debug)]
pub struct ProcessSymbols {
    pub set: SymbolSet,
    /// Per module (same order as the set), its ELF facts.
    pub facts: Vec<Arc<ElfFacts>>,
}

impl ProcessSymbols {
    /// Whether `addr` is in a read-only segment of a loaded module (code, `.rodata`).
    pub fn is_read_only(&self, addr: u64) -> bool {
        self.set.modules().any(|(id, m)| {
            let f = m.to_file(addr);
            self.facts
                .get(id.0)
                .is_some_and(|facts| facts.read_only.iter().any(|r| r.contains(&f)))
        })
    }
}

impl SymbolCache {
    fn binary(&self, path: &Path) -> Opened {
        let mut map = self.binaries.lock().unwrap_or_else(|p| p.into_inner());
        map.entry(path.to_path_buf())
            .or_insert_with(|| {
                let b = Binary::open(path).ok()?;
                let facts = Arc::new(ElfFacts::of(b.bytes()));
                Some((b, facts))
            })
            .clone()
    }

    /// The symbols of the process whose main executable is `exe`, at the state `memory`
    /// describes.
    pub fn process(
        &self,
        exe: &Path,
        auxv: &[u8],
        memory: &mut dyn Memory,
    ) -> Result<Arc<ProcessSymbols>> {
        let (main_bin, main_facts) = self
            .binary(exe)
            .ok_or_else(|| HopperError::Symbols(format!("cannot read {}", exe.display())))?;
        let main = Module::main_from_auxv(main_bin, auxv)?;
        let mut objects = vec![LoadedObject {
            path: exe.to_path_buf(),
            bias: main.bias(),
        }];
        let listed = main_facts
            .dynamic
            .map(|d| link_map(memory, main.to_runtime(d)))
            .unwrap_or_default();
        if listed.is_empty() {
            // The dynamic linker has not run: place the interpreter by AT_BASE.
            if let (Some(interp), Some(base)) = (&main_facts.interpreter, auxv_get(auxv, AT_BASE))
                && base != 0
            {
                objects.push(LoadedObject {
                    path: PathBuf::from(interp),
                    bias: base,
                });
            }
        } else {
            for o in listed {
                // The main program's own entry has an empty name; vdso has no file.
                if o.path.as_os_str().is_empty() || !o.path.is_file() {
                    continue;
                }
                if !objects.iter().any(|x| x.path == o.path) {
                    objects.push(o);
                }
            }
        }
        let mut sets = self.sets.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(s) = sets.get(&objects) {
            return Ok(s.clone());
        }
        let mut set = SymbolSet::new();
        let mut facts = Vec::new();
        set.add(main);
        facts.push(main_facts);
        for o in objects.iter().skip(1) {
            if let Some((b, f)) = self.binary(&o.path) {
                set.add(Module::new(b, o.bias));
                facts.push(f);
            }
        }
        let ps = Arc::new(ProcessSymbols { set, facts });
        sets.insert(objects, ps.clone());
        Ok(ps)
    }
}

fn auxv_get(auxv: &[u8], ty: u64) -> Option<u64> {
    auxv.chunks_exact(16).find_map(|p| {
        let t = u64::from_le_bytes(p[..8].try_into().ok()?);
        let v = u64::from_le_bytes(p[8..].try_into().ok()?);
        (t == ty).then_some(v)
    })
}

/// The objects on the link map reached from the dynamic section at runtime address `dynamic`.
/// Empty when the dynamic linker has not filled in `DT_DEBUG` yet or memory is unreadable.
fn link_map(memory: &mut dyn Memory, dynamic: u64) -> Vec<LoadedObject> {
    let Some(r_debug) = dt_debug(memory, dynamic) else {
        return Vec::new();
    };
    let Ok(mut entry) = memory.read_u64(r_debug + 8) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    while entry != 0 && out.len() < MAX_LINK_MAP {
        let (Ok(l_addr), Ok(l_name), Ok(next)) = (
            memory.read_u64(entry),
            memory.read_u64(entry + 8),
            memory.read_u64(entry + 24),
        ) else {
            break;
        };
        let name = read_c_string(memory, l_name).unwrap_or_default();
        out.push(LoadedObject {
            path: PathBuf::from(name),
            bias: l_addr,
        });
        entry = next;
    }
    out
}

fn dt_debug(memory: &mut dyn Memory, dynamic: u64) -> Option<u64> {
    for i in 0..512u64 {
        let tag = memory.read_u64(dynamic + 16 * i).ok()?;
        if tag == DT_NULL {
            return None;
        }
        if tag == DT_DEBUG {
            let v = memory.read_u64(dynamic + 16 * i + 8).ok()?;
            return (v != 0).then_some(v);
        }
    }
    None
}

fn read_c_string(memory: &mut dyn Memory, addr: u64) -> Option<String> {
    if addr == 0 {
        return None;
    }
    let mut out = Vec::new();
    for _ in 0..16 {
        // Read in small pieces so a string near the end of a mapping still reads.
        let mut b = [0u8; 64];
        let at = addr + out.len() as u64;
        if memory.read(at, &mut b).is_err() {
            for k in 0..64 {
                let mut one = [0u8; 1];
                memory.read(at + k, &mut one).ok()?;
                if one[0] == 0 {
                    return Some(String::from_utf8_lossy(&out).into_owned());
                }
                out.push(one[0]);
            }
            continue;
        }
        if let Some(n) = b.iter().position(|c| *c == 0) {
            out.extend_from_slice(&b[..n]);
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        out.extend_from_slice(&b);
    }
    Some(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::target::ImageMemory;

    #[test]
    fn follows_dt_debug_to_the_link_map() {
        let dynamic = 0x1000u64;
        let mut dynsec = Vec::new();
        for (t, v) in [(1u64, 5u64), (DT_DEBUG, 0x2000), (DT_NULL, 0)] {
            dynsec.extend_from_slice(&t.to_le_bytes());
            dynsec.extend_from_slice(&v.to_le_bytes());
        }
        // r_debug { r_version, r_map }
        let mut rdebug = 1u64.to_le_bytes().to_vec();
        rdebug.extend_from_slice(&0x3000u64.to_le_bytes());
        let lm = |addr: u64, name: u64, next: u64| {
            let mut v = Vec::new();
            for x in [addr, name, 0, next, 0] {
                v.extend_from_slice(&x.to_le_bytes());
            }
            v
        };
        let mut mem = ImageMemory::default()
            .with(dynamic, dynsec)
            .with(0x2000, rdebug)
            .with(0x3000, lm(0, 0x4000, 0x3100))
            .with(0x3100, lm(0x7f00_0000_0000, 0x4100, 0))
            .with(0x4000, vec![0; 64])
            .with(
                0x4100,
                b"/lib/libc.so.6\0".iter().copied().chain([0; 49]).collect(),
            );
        let objs = link_map(&mut mem, dynamic);
        assert_eq!(objs.len(), 2);
        assert_eq!(objs[0].path, PathBuf::new());
        assert_eq!(
            objs[1],
            LoadedObject {
                path: PathBuf::from("/lib/libc.so.6"),
                bias: 0x7f00_0000_0000
            }
        );
    }

    #[test]
    fn no_dt_debug_value_means_no_list() {
        let mut dynsec = Vec::new();
        for (t, v) in [(DT_DEBUG, 0u64), (DT_NULL, 0)] {
            dynsec.extend_from_slice(&t.to_le_bytes());
            dynsec.extend_from_slice(&v.to_le_bytes());
        }
        let mut mem = ImageMemory::default().with(0x1000, dynsec);
        assert!(link_map(&mut mem, 0x1000).is_empty());
    }
}
