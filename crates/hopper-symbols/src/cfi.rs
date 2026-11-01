//! Call-frame information (`.eh_frame` and `.debug_frame`) of one binary, with an FDE index sorted
//! by address so a lookup is a binary search rather than gimli's linear scan.

use std::fmt;

use gimli::{BaseAddresses, CieOrFde, DebugFrame, EhFrame, FrameDescriptionEntry, UnwindSection};

use crate::binary::Binary;
use crate::dwarf::Reader;

/// Which section an FDE came from.
#[derive(Clone, Debug)]
pub(crate) enum FdeSource {
    EhFrame,
    DebugFrame,
}

pub(crate) struct CallFrameInfo {
    pub eh_frame: Option<EhFrame<Reader>>,
    pub debug_frame: Option<DebugFrame<Reader>>,
    pub bases: BaseAddresses,
    /// `(start, end, source, fde)` sorted by start.
    fdes: Vec<(u64, u64, FdeSource, FrameDescriptionEntry<Reader>)>,
}

impl fmt::Debug for CallFrameInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CallFrameInfo").field("fdes", &self.fdes.len()).finish()
    }
}

impl CallFrameInfo {
    pub fn load(binary: &Binary) -> CallFrameInfo {
        let mut bases = BaseAddresses::default();
        if let Some(s) = binary.section(".eh_frame_hdr") {
            bases = bases.set_eh_frame_hdr(s.address);
        }
        if let Some(s) = binary.section(".eh_frame") {
            bases = bases.set_eh_frame(s.address);
        }
        if let Some(s) = binary.section(".text") {
            bases = bases.set_text(s.address);
        }
        if let Some(s) = binary.section(".got") {
            bases = bases.set_got(s.address);
        }
        let eh_frame = binary.section(".eh_frame").map(|s| {
            let mut f = EhFrame::from(s.data.clone());
            f.set_address_size(8);
            f
        });
        let debug_frame = binary.section(".debug_frame").map(|s| {
            let mut f = DebugFrame::from(s.data.clone());
            f.set_address_size(8);
            f
        });
        let mut fdes = Vec::new();
        if let Some(eh) = &eh_frame {
            collect(eh, &bases, FdeSource::EhFrame, &mut fdes);
        }
        if let Some(df) = &debug_frame {
            collect(df, &bases, FdeSource::DebugFrame, &mut fdes);
        }
        fdes.sort_by_key(|f| f.0);
        CallFrameInfo { eh_frame, debug_frame, bases, fdes }
    }

    /// The FDE covering `addr` (file address). `.eh_frame` wins over `.debug_frame` when both
    /// cover it, because it is what the runtime unwinder uses.
    pub fn fde_for(&self, addr: u64) -> Option<(&FdeSource, &FrameDescriptionEntry<Reader>)> {
        let end = self.fdes.partition_point(|f| f.0 <= addr);
        let mut best: Option<(&FdeSource, &FrameDescriptionEntry<Reader>)> = None;
        for (start, stop, source, fde) in self.fdes[..end].iter().rev().take(16) {
            if *start <= addr && addr < *stop {
                match (best, source) {
                    (None, _) | (Some((FdeSource::DebugFrame, _)), FdeSource::EhFrame) => {
                        best = Some((source, fde))
                    }
                    _ => {}
                }
            }
        }
        best
    }
}

fn collect<S: UnwindSection<Reader>>(
    section: &S,
    bases: &BaseAddresses,
    source: FdeSource,
    out: &mut Vec<(u64, u64, FdeSource, FrameDescriptionEntry<Reader>)>,
) where
    S::Offset: gimli::UnwindOffset<usize>,
{
    let mut entries = section.entries(bases);
    loop {
        match entries.next() {
            Ok(Some(CieOrFde::Fde(partial))) => {
                if let Ok(fde) = partial.parse(S::cie_from_offset) {
                    let start = fde.initial_address();
                    let end = start + fde.len();
                    if fde.len() > 0 && start != 0 {
                        out.push((start, end, source.clone(), fde));
                    }
                }
            }
            Ok(Some(CieOrFde::Cie(_))) => {}
            Ok(None) | Err(_) => break,
        }
    }
}
