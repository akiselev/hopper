//! Where a value lives.

use serde::{Deserialize, Serialize};

use crate::ids::{FrameId, ProcessId};
use crate::regs::Reg;

/// A storage location: a byte range in one process's memory, or a register of one frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Place {
    Memory { process: ProcessId, addr: u64, len: u64 },
    Register { frame: FrameId, reg: Reg },
}

impl Place {
    pub fn memory(process: ProcessId, addr: u64, len: u64) -> Self {
        Place::Memory { process, addr, len }
    }

    /// Whether two memory places share at least one byte.
    pub fn overlaps(&self, other: &Place) -> bool {
        match (self, other) {
            (
                Place::Memory { process: p1, addr: a1, len: l1 },
                Place::Memory { process: p2, addr: a2, len: l2 },
            ) => p1 == p2 && *a1 < a2.saturating_add(*l2) && *a2 < a1.saturating_add(*l1),
            (Place::Register { frame: f1, reg: r1 }, Place::Register { frame: f2, reg: r2 }) => {
                f1 == f2 && r1 == r2
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_overlap() {
        let p = ProcessId::new(1);
        let a = Place::memory(p, 100, 8);
        assert!(a.overlaps(&Place::memory(p, 107, 1)));
        assert!(!a.overlaps(&Place::memory(p, 108, 4)));
        assert!(!a.overlaps(&Place::memory(ProcessId::new(2), 100, 8)));
    }
}
