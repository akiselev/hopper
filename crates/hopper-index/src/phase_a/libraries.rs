//! Shared libraries from the dynamic linker's own list.
//!
//! glibc's `ld.so` publishes `struct r_debug` through the main executable's `DT_DEBUG` dynamic
//! entry once it has mapped the program's libraries:
//!
//! ```text
//! struct r_debug   { int r_version; struct link_map *r_map; ElfW(Addr) r_brk; int r_state; ... }
//!                    offsets 0, 8, 16, 24
//! struct link_map  { ElfW(Addr) l_addr; char *l_name; ElfW(Dyn) *l_ld; link_map *l_next, *l_prev; }
//!                    offsets 0, 8, 16, 24, 32
//! ```
//!
//! `l_addr` is the library's load bias. `r_brk` is a function ld.so calls before and after every
//! change to the list (`dlopen`, `dlclose`); a breakpoint there catches later loads. Reading the
//! list needs only memory reads, so it works the same on rr and gdbserver.

use hopper_core::Result;
use hopper_core::target::{Memory, Target, TargetMemory};
use hopper_symbols::Module;
use object::{Object, ObjectSection};

const DT_NULL: u64 = 0;
const DT_DEBUG: u64 = 21;

/// `r_debug.r_state` when the list is consistent.
pub(crate) const RT_CONSISTENT: u64 = 0;

/// One `link_map` entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LoadedLibrary {
    pub bias: u64,
    pub path: String,
}

/// The dynamic linker's list as read now.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LinkMap {
    pub libraries: Vec<LoadedLibrary>,
    /// `r_debug.r_brk`.
    pub r_brk: u64,
    pub r_debug: u64,
}

/// The runtime address of the main module's `.dynamic` section.
fn dynamic_address(main: &Module) -> Option<u64> {
    let file = object::File::parse(main.binary().bytes()).ok()?;
    let s = file.section_by_name(".dynamic")?;
    Some(main.to_runtime(s.address()))
}

fn read_c_string(mem: &mut dyn Memory, addr: u64) -> Option<String> {
    let mut out = Vec::new();
    let mut at = addr;
    while out.len() < 4096 {
        // Read up to the end of the page, so a string that ends before an unmapped page reads.
        let to_page_end = 0x1000 - (at & 0xfff) as usize;
        let mut buf = vec![0u8; to_page_end.min(256)];
        mem.read(at, &mut buf).ok()?;
        if let Some(n) = buf.iter().position(|b| *b == 0) {
            out.extend_from_slice(&buf[..n]);
            return Some(String::from_utf8_lossy(&out).into_owned());
        }
        out.extend_from_slice(&buf);
        at += buf.len() as u64;
    }
    None
}

/// The `r_debug` address from `DT_DEBUG`, once ld.so has filled it in.
pub(crate) fn r_debug_address(target: &mut dyn Target, main: &Module) -> Result<Option<u64>> {
    let Some(dynamic) = dynamic_address(main) else {
        return Ok(None);
    };
    let mut mem = TargetMemory(target);
    for i in 0..512u64 {
        let tag = mem.read_u64(dynamic + i * 16)?;
        if tag == DT_NULL {
            break;
        }
        if tag == DT_DEBUG {
            let v = mem.read_u64(dynamic + i * 16 + 8)?;
            return Ok((v != 0).then_some(v));
        }
    }
    Ok(None)
}

/// `r_debug.r_state`.
pub(crate) fn r_state(target: &mut dyn Target, r_debug: u64) -> Result<u64> {
    let mut b = [0u8; 4];
    target.read_memory(r_debug + 24, &mut b)?;
    Ok(u64::from(u32::from_le_bytes(b)))
}

/// Read the list. `None` before ld.so has published it (a static executable never does).
pub(crate) fn read_link_map(target: &mut dyn Target, main: &Module) -> Result<Option<LinkMap>> {
    let Some(r_debug) = r_debug_address(target, main)? else {
        return Ok(None);
    };
    let mut mem = TargetMemory(target);
    let mut entry = mem.read_u64(r_debug + 8)?;
    let r_brk = mem.read_u64(r_debug + 16)?;
    let mut libraries = Vec::new();
    let mut guard = 0;
    while entry != 0 && guard < 4096 {
        guard += 1;
        let bias = mem.read_u64(entry)?;
        let name_ptr = mem.read_u64(entry + 8)?;
        let path = if name_ptr == 0 {
            None
        } else {
            read_c_string(&mut mem, name_ptr)
        };
        if let Some(path) = path.filter(|p| !p.is_empty()) {
            libraries.push(LoadedLibrary { bias, path });
        }
        entry = mem.read_u64(entry + 24)?;
    }
    Ok(Some(LinkMap {
        libraries,
        r_brk,
        r_debug,
    }))
}
