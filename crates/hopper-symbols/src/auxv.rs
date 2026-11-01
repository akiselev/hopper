//! The ELF auxiliary vector (`qXfer:auxv:read`, a core file's `NT_AUXV`).

/// `AT_*` keys hopper reads.
pub const AT_PHDR: u64 = 3;
pub const AT_PHNUM: u64 = 5;
pub const AT_BASE: u64 = 7;
pub const AT_ENTRY: u64 = 9;
pub const AT_SYSINFO_EHDR: u64 = 33;

/// A parsed auxiliary vector of 64-bit `(key, value)` pairs.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Auxv {
    pub entries: Vec<(u64, u64)>,
}

impl Auxv {
    /// Parse little-endian 64-bit pairs up to `AT_NULL` or the end of the bytes.
    pub fn parse(bytes: &[u8]) -> Auxv {
        let mut entries = Vec::new();
        for pair in bytes.chunks_exact(16) {
            let key = u64::from_le_bytes(pair[..8].try_into().expect("8 bytes"));
            let value = u64::from_le_bytes(pair[8..].try_into().expect("8 bytes"));
            if key == 0 {
                break;
            }
            entries.push((key, value));
        }
        Auxv { entries }
    }

    pub fn get(&self, key: u64) -> Option<u64> {
        self.entries.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)
    }

    /// The program's runtime entry point.
    pub fn entry(&self) -> Option<u64> {
        self.get(AT_ENTRY)
    }

    /// The dynamic loader's base address.
    pub fn interpreter_base(&self) -> Option<u64> {
        self.get(AT_BASE).filter(|b| *b != 0)
    }

    /// The vDSO's ELF header address.
    pub fn vdso(&self) -> Option<u64> {
        self.get(AT_SYSINFO_EHDR)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_until_null() {
        let mut b = Vec::new();
        for (k, v) in [(AT_PHDR, 0x40u64), (AT_ENTRY, 0x5555_0000_1000), (0, 0), (AT_BASE, 9)] {
            b.extend_from_slice(&k.to_le_bytes());
            b.extend_from_slice(&v.to_le_bytes());
        }
        let a = Auxv::parse(&b);
        assert_eq!(a.entry(), Some(0x5555_0000_1000));
        assert_eq!(a.get(AT_BASE), None);
        assert_eq!(a.entries.len(), 2);
    }
}
