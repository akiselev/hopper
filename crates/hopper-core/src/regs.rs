//! Registers, named by their x86-64 DWARF numbers (System V psABI, table 3.36), which is what
//! DWARF location expressions and CFI use.

use serde::{Deserialize, Serialize};

/// A register by DWARF number.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Reg(pub u16);

impl Reg {
    pub const RAX: Reg = Reg(0);
    pub const RDX: Reg = Reg(1);
    pub const RCX: Reg = Reg(2);
    pub const RBX: Reg = Reg(3);
    pub const RSI: Reg = Reg(4);
    pub const RDI: Reg = Reg(5);
    pub const RBP: Reg = Reg(6);
    pub const RSP: Reg = Reg(7);
    pub const R8: Reg = Reg(8);
    pub const R15: Reg = Reg(15);
    /// The return-address column; holds `rip` in a register set.
    pub const RIP: Reg = Reg(16);
    pub const XMM0: Reg = Reg(17);
    pub const XMM15: Reg = Reg(32);
    pub const RFLAGS: Reg = Reg(49);
    pub const ES: Reg = Reg(50);
    pub const CS: Reg = Reg(51);
    pub const SS: Reg = Reg(52);
    pub const DS: Reg = Reg(53);
    pub const FS: Reg = Reg(54);
    pub const GS: Reg = Reg(55);
    pub const FS_BASE: Reg = Reg(58);
    pub const GS_BASE: Reg = Reg(59);

    /// The name gdb's target descriptions use.
    pub fn name(self) -> &'static str {
        const GPR: [&str; 17] = [
            "rax", "rdx", "rcx", "rbx", "rsi", "rdi", "rbp", "rsp", "r8", "r9", "r10", "r11",
            "r12", "r13", "r14", "r15", "rip",
        ];
        const XMM: [&str; 16] = [
            "xmm0", "xmm1", "xmm2", "xmm3", "xmm4", "xmm5", "xmm6", "xmm7", "xmm8", "xmm9",
            "xmm10", "xmm11", "xmm12", "xmm13", "xmm14", "xmm15",
        ];
        match self.0 {
            n @ 0..=16 => GPR[n as usize],
            n @ 17..=32 => XMM[(n - 17) as usize],
            49 => "eflags",
            50 => "es",
            51 => "cs",
            52 => "ss",
            53 => "ds",
            54 => "fs",
            55 => "gs",
            58 => "fs_base",
            59 => "gs_base",
            _ => "?",
        }
    }

    /// The DWARF number for a gdb register name, if hopper models it.
    pub fn from_name(name: &str) -> Option<Reg> {
        let all = (0u16..=32).chain([49, 50, 51, 52, 53, 54, 55, 58, 59]);
        for n in all {
            if Reg(n).name() == name {
                return Some(Reg(n));
            }
        }
        None
    }

    /// Size in bytes.
    pub fn size(self) -> usize {
        match self.0 {
            17..=32 => 16,
            50..=55 => 4,
            49 => 4,
            _ => 8,
        }
    }
}

/// The registers of one thread at one moment.
///
/// `values` holds every register hopper models, keyed by DWARF number, little-endian bytes.
/// `raw` keeps the target's own `g` packet bytes, so two register sets can be compared
/// byte-for-byte (the M1 exit test).
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RegisterSet {
    pub values: Vec<(Reg, Vec<u8>)>,
    pub raw: Vec<u8>,
}

impl RegisterSet {
    pub fn get_bytes(&self, reg: Reg) -> Option<&[u8]> {
        self.values.iter().find(|(r, _)| *r == reg).map(|(_, v)| v.as_slice())
    }

    /// The register as an integer (the low 8 bytes, zero-extended).
    pub fn get(&self, reg: Reg) -> Option<u64> {
        let b = self.get_bytes(reg)?;
        let mut buf = [0u8; 8];
        let n = b.len().min(8);
        buf[..n].copy_from_slice(&b[..n]);
        Some(u64::from_le_bytes(buf))
    }

    pub fn set(&mut self, reg: Reg, value: u64) {
        let bytes = value.to_le_bytes()[..reg.size().min(8)].to_vec();
        self.set_bytes(reg, bytes);
    }

    pub fn set_bytes(&mut self, reg: Reg, bytes: Vec<u8>) {
        if let Some(slot) = self.values.iter_mut().find(|(r, _)| *r == reg) {
            slot.1 = bytes;
        } else {
            self.values.push((reg, bytes));
            self.values.sort_by_key(|(r, _)| *r);
        }
    }

    pub fn pc(&self) -> Option<u64> {
        self.get(Reg::RIP)
    }

    pub fn sp(&self) -> Option<u64> {
        self.get(Reg::RSP)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_round_trip() {
        for r in [Reg::RAX, Reg::RSP, Reg::RIP, Reg::XMM0, Reg::RFLAGS, Reg::FS_BASE] {
            assert_eq!(Reg::from_name(r.name()), Some(r));
        }
        assert_eq!(Reg::from_name("st0"), None);
    }

    #[test]
    fn set_and_get() {
        let mut rs = RegisterSet::default();
        rs.set(Reg::RIP, 0x401000);
        rs.set(Reg::RAX, u64::MAX);
        assert_eq!(rs.pc(), Some(0x401000));
        assert_eq!(rs.get(Reg::RAX), Some(u64::MAX));
        rs.set(Reg::RIP, 5);
        assert_eq!(rs.pc(), Some(5));
        assert_eq!(rs.values.len(), 2);
    }
}
