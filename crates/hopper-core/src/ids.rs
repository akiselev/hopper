//! Identifiers.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::HopperError;

/// One recording. For rr this is the trace's uuid; for a live target it is a fresh random id
/// (a live target has no stable identity across sessions).
#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RecordingId(pub [u8; 16]);

impl RecordingId {
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Display for RecordingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for RecordingId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "RecordingId({})", self.to_hex())
    }
}

impl FromStr for RecordingId {
    type Err = HopperError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s: String = s.chars().filter(|c| *c != '-').collect();
        if s.len() != 32 {
            return Err(HopperError::Invalid(format!(
                "recording id must be 32 hex digits: {s:?}"
            )));
        }
        let mut out = [0u8; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&s[2 * i..2 * i + 2], 16)
                .map_err(|_| HopperError::Invalid(format!("bad hex in recording id: {s:?}")))?;
        }
        Ok(RecordingId(out))
    }
}

/// A Linux thread id as rr and the kernel name it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Tid(pub i32);

impl fmt::Display for Tid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A process in a recording: its thread-group id plus how many `exec`s it has done, because an
/// exec replaces the address space while keeping the pid.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ProcessId {
    pub pid: i32,
    pub exec_generation: u32,
}

impl ProcessId {
    pub fn new(pid: i32) -> Self {
        ProcessId {
            pid,
            exec_generation: 0,
        }
    }
}

/// A frame in a thread's stack at one moment: depth 0 is the innermost frame. Inlined calls get
/// their own depth, as in the source.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct FrameId {
    pub tid: Tid,
    pub depth: u32,
}

/// An rr checkpoint (or another target's saved state).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct CheckpointId(pub u64);
