//! hopper's shared vocabulary.
//!
//! Every other crate names these types and no second type exists for the same concept:
//! a point in a recording is a [`Moment`], a range of memory or a register is a [`Place`], a
//! source position is a [`SourceLoc`], and every failure is a [`HopperError`].
//!
//! Two traits are the seams between crates:
//! - [`target::Target`] is what a replay (an rr replica, or a live gdbserver for tests) offers the
//!   engine and the indexer: registers, memory, breakpoints, resume in either direction, seek.
//! - [`api::Debugger`] is the one engine API. The GUI, the CLI, the MCP server and the DAP adapter
//!   all talk to a `dyn Debugger`; `hopper-engine` implements it and `hopper-fake` fakes it.
//!
//! This crate does no I/O.

pub mod api;
pub mod error;
pub mod ids;
pub mod moment;
pub mod place;
pub mod regs;
pub mod source;
pub mod target;
pub mod wire;

pub use error::{HopperError, Result};
pub use ids::{CheckpointId, FrameId, ProcessId, RecordingId, Tid};
pub use moment::{Moment, MomentRange, PositionUrl};
pub use place::Place;
pub use regs::{Reg, RegisterSet};
pub use source::SourceLoc;
