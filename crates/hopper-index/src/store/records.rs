//! What the chunks hold: one record per observed event, in the order phase A or phase B saw
//! them.

use hopper_core::api::TimelineKind;
use hopper_core::{Moment, Place, ProcessId, RecordingId, SourceLoc, Tid};
use serde::{Deserialize, Serialize};

use crate::phase_a::AllocatorFunction;

/// A moment without its recording id (every record of one index belongs to one recording),
/// stored as `[frame_time, tid, ticks, sub]`.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Stamp(pub u64, pub i32, pub u64, pub u32);

impl Stamp {
    pub fn of(m: &Moment) -> Stamp {
        Stamp(m.frame_time, m.tid.0, m.ticks, m.sub)
    }

    pub fn moment(&self, recording: RecordingId) -> Moment {
        Moment {
            recording,
            frame_time: self.0,
            tid: Tid(self.1),
            ticks: self.2,
            sub: self.3,
        }
    }

    /// The order key of [`Moment::order_key`].
    pub fn key(&self) -> (u64, u64, u32) {
        (self.0, self.2, self.3)
    }
}

/// One stored record. Dictionary records (`Function`, `Line`) come before the first record that
/// names their id, in the same kind's stream.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Record {
    /// A function of the call tree, named once.
    Function {
        fid: u32,
        name: String,
        location: Option<SourceLoc>,
    },
    /// A call was entered.
    CallEnter {
        id: u64,
        fid: u32,
        parent: Option<u64>,
        process: ProcessId,
        at: Stamp,
        depth: u32,
    },
    /// A call returned (or was unwound past when an outer call returned).
    CallExit { id: u64, at: Stamp },
    /// A source line with breakpoints, named once.
    Line { lid: u32, location: SourceLoc },
    /// Execution reached a statement address of a line.
    LineHit { lid: u32, at: Stamp },
    /// An allocator call returned a block.
    Allocated {
        id: u64,
        process: ProcessId,
        at: Stamp,
        addr: u64,
        size: u64,
        predecessor: Option<u64>,
        requested_by: Option<String>,
        allocator: AllocatorFunction,
    },
    /// A block was freed (or moved away by realloc).
    Freed { id: u64, at: Stamp },
    /// A timeline event.
    Timeline { at: Stamp, kind: TimelineKind },
    /// A memory write seen by phase B.
    Write {
        at: Stamp,
        pc: u64,
        place: Place,
        value: crate::writes::WriteValue,
    },
}

impl Record {
    /// The record's moment, for changing it in place.
    pub fn stamp_mut(&mut self) -> Option<&mut Stamp> {
        match self {
            Record::Function { .. } | Record::Line { .. } => None,
            Record::CallEnter { at, .. }
            | Record::CallExit { at, .. }
            | Record::LineHit { at, .. }
            | Record::Allocated { at, .. }
            | Record::Freed { at, .. }
            | Record::Timeline { at, .. }
            | Record::Write { at, .. } => Some(at),
        }
    }

    /// The record's moment, when it has one.
    pub fn stamp(&self) -> Option<Stamp> {
        match self {
            Record::Function { .. } | Record::Line { .. } => None,
            Record::CallEnter { at, .. }
            | Record::CallExit { at, .. }
            | Record::LineHit { at, .. }
            | Record::Allocated { at, .. }
            | Record::Freed { at, .. }
            | Record::Timeline { at, .. }
            | Record::Write { at, .. } => Some(*at),
        }
    }
}
