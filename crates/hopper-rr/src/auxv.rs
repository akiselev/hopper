//! The auxiliary vector, and the main executable's load bias computed from it.
//!
//! On x86-64 Linux the auxv is a list of `(type: u64, value: u64)` pairs ending with `AT_NULL`.
//! The kernel puts the main executable's program headers at `AT_PHDR` and its entry point at
//! `AT_ENTRY`, both already relocated; `AT_BASE` is the interpreter's (ld.so) load address.
//! A position-independent executable is loaded at `bias + p_vaddr`, so
//! `bias = AT_ENTRY - e_entry = AT_PHDR - phdr_vaddr`.

use hopper_core::{HopperError, Result};

pub const AT_NULL: u64 = 0;
pub const AT_PHDR: u64 = 3;
pub const AT_PHENT: u64 = 4;
pub const AT_PHNUM: u64 = 5;
pub const AT_BASE: u64 = 7;
pub const AT_ENTRY: u64 = 9;

/// The entries hopper uses.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct Auxv {
    /// Every `(type, value)` pair before `AT_NULL`, in order.
    pub entries: Vec<(u64, u64)>,
}

impl Auxv {
    /// Parse a little-endian 64-bit auxv as `qXfer:auxv:read` returns it.
    pub fn parse(bytes: &[u8]) -> Result<Auxv> {
        if !bytes.len().is_multiple_of(16) {
            return Err(HopperError::Protocol(format!(
                "auxv length {} is not a multiple of 16",
                bytes.len()
            )));
        }
        let mut entries = Vec::new();
        for pair in bytes.chunks_exact(16) {
            let ty = u64::from_le_bytes(pair[..8].try_into().expect("8 bytes"));
            let val = u64::from_le_bytes(pair[8..].try_into().expect("8 bytes"));
            if ty == AT_NULL {
                return Ok(Auxv { entries });
            }
            entries.push((ty, val));
        }
        Err(HopperError::Protocol(
            "auxv has no AT_NULL terminator".into(),
        ))
    }

    pub fn get(&self, ty: u64) -> Option<u64> {
        self.entries.iter().find(|(t, _)| *t == ty).map(|(_, v)| *v)
    }

    pub fn phdr(&self) -> Option<u64> {
        self.get(AT_PHDR)
    }

    pub fn entry(&self) -> Option<u64> {
        self.get(AT_ENTRY)
    }

    /// The interpreter's load address; `None` (or 0 in the vector) for a static executable.
    pub fn interpreter_base(&self) -> Option<u64> {
        self.get(AT_BASE).filter(|b| *b != 0)
    }

    /// The main executable's load bias from its ELF entry point (`e_entry`).
    pub fn load_bias_from_entry(&self, elf_entry: u64) -> Result<u64> {
        let entry = self
            .entry()
            .ok_or_else(|| HopperError::NotFound("AT_ENTRY in the auxv".into()))?;
        entry.checked_sub(elf_entry).ok_or_else(|| {
            HopperError::Invalid(format!(
                "AT_ENTRY {entry:#x} is below the ELF entry point {elf_entry:#x}"
            ))
        })
    }

    /// The main executable's load bias from the `p_vaddr` of its program headers (`PT_PHDR`, or
    /// the first `PT_LOAD`'s `p_vaddr + e_phoff` when there is no `PT_PHDR`).
    pub fn load_bias_from_phdr(&self, phdr_vaddr: u64) -> Result<u64> {
        let phdr = self
            .phdr()
            .ok_or_else(|| HopperError::NotFound("AT_PHDR in the auxv".into()))?;
        phdr.checked_sub(phdr_vaddr).ok_or_else(|| {
            HopperError::Invalid(format!(
                "AT_PHDR {phdr:#x} is below the program header address {phdr_vaddr:#x}"
            ))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(entries: &[(u64, u64)]) -> Vec<u8> {
        entries
            .iter()
            .flat_map(|(t, v)| t.to_le_bytes().into_iter().chain(v.to_le_bytes()))
            .collect()
    }

    #[test]
    fn computes_bias_both_ways() {
        let raw = bytes(&[
            (33, 0x7ffff7fc1000),
            (AT_PHDR, 0x555555554040),
            (AT_PHENT, 56),
            (AT_PHNUM, 13),
            (AT_BASE, 0x7ffff7fc3000),
            (AT_ENTRY, 0x555555559e60),
            (AT_NULL, 0),
            (99, 99),
        ]);
        let a = Auxv::parse(&raw).unwrap();
        assert_eq!(a.entries.len(), 6);
        assert_eq!(a.load_bias_from_entry(0x5e60).unwrap(), 0x555555554000);
        assert_eq!(a.load_bias_from_phdr(0x40).unwrap(), 0x555555554000);
        assert_eq!(a.interpreter_base(), Some(0x7ffff7fc3000));
        assert!(a.load_bias_from_entry(0x7fff_ffff_ffff).is_err());
    }

    #[test]
    fn rejects_malformed() {
        assert!(Auxv::parse(&[0; 15]).is_err());
        assert!(Auxv::parse(&bytes(&[(AT_ENTRY, 1)])).is_err());
        let a = Auxv::parse(&bytes(&[(AT_BASE, 0), (AT_NULL, 0)])).unwrap();
        assert_eq!(a.interpreter_base(), None);
        assert!(a.load_bias_from_entry(0).is_err());
        assert!(a.load_bias_from_phdr(0).is_err());
    }
}
