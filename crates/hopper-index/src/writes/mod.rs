//! Phase B: the last-writer log for chosen moment windows.
//!
//! Two producers fill the same log:
//! - [`log_writes_by_stepping`] single-steps any [`Target`] (an rr replica or a live gdbserver)
//!   through a window. Before each step it decodes the instruction at the PC with iced-x86 and
//!   computes the effective address of every memory operand the instruction writes from the
//!   registers; after the step it reads the bytes there.
//! - [`rerun`] runs `rr rerun --singlestep=...` shards in parallel and decodes their register
//!   dumps the same way. rerun dumps registers only, so the written bytes are known only when
//!   they come from a register or an immediate ([`WriteValue::Unrecorded`] otherwise).
//!
//! Writes the kernel performs (syscall results) are not instruction writes and are not logged.

pub mod decode;
pub mod rerun;
mod step;

use hopper_core::api::WriteEvent;
use hopper_core::{Moment, Place};
use hopper_symbols::SymbolSet;
use serde::{Deserialize, Serialize};

pub use step::{StepWindow, WindowEnd, log_writes_by_stepping};

/// The bytes a write left behind, when the producer saw them.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum WriteValue {
    Bytes(Vec<u8>),
    /// The producer knows where the instruction wrote but not what (an rr rerun step whose
    /// source was memory or a computation). Read the place at the moment after the write.
    Unrecorded,
}

/// One logged write: the instruction at `pc`, about to execute at moment `at`, writes `place`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoggedWrite {
    pub at: Moment,
    pub pc: u64,
    pub place: Place,
    pub value: WriteValue,
}

impl LoggedWrite {
    /// As the engine API's [`WriteEvent`], with the function and source line of the PC. `None`
    /// when the bytes were not recorded (the caller reads them at the next moment and uses
    /// [`LoggedWrite::event_with_value`]).
    pub fn to_event(&self, symbols: &SymbolSet) -> Option<WriteEvent> {
        match &self.value {
            WriteValue::Bytes(b) => Some(self.event_with_value(symbols, b.clone())),
            WriteValue::Unrecorded => None,
        }
    }

    /// As a [`WriteEvent`] holding `value`.
    pub fn event_with_value(&self, symbols: &SymbolSet, value: Vec<u8>) -> WriteEvent {
        let function = symbols
            .frames_at(self.pc)
            .into_iter()
            .next()
            .and_then(|f| f.function)
            .or_else(|| symbols.symbol_at(self.pc).map(|s| s.name));
        WriteEvent {
            at: self.at,
            pc: self.pc,
            place: self.place,
            value,
            function,
            location: symbols.location(self.pc),
        }
    }
}

/// What a phase-B run logged.
#[derive(Clone, Debug, PartialEq)]
pub struct WriteLogSummary {
    /// Instructions executed (single steps taken, or rerun step records decoded).
    pub instructions: u64,
    pub writes: u64,
    /// Steps whose instruction could not be decoded or whose write could not be read.
    pub undecoded: u64,
    pub first: Option<Moment>,
    pub last: Option<Moment>,
    pub elapsed: std::time::Duration,
}

impl WriteLogSummary {
    /// Instructions per second of wall time.
    pub fn instructions_per_second(&self) -> f64 {
        let s = self.elapsed.as_secs_f64();
        if s > 0.0 {
            self.instructions as f64 / s
        } else {
            0.0
        }
    }
}
