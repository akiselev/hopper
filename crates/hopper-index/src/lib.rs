//! hopper's background index.
//!
//! The index answers the omniscient queries of [`hopper_core::api::Debugger`] without moving the
//! cursor. It is built by replaying the recording on a background [`Target`] and stored beside the
//! trace (never inside it) in a directory the engine chooses.
//!
//! - **Phase A** ([`PhaseA`]) is breakpoint-only: function entries and return addresses give the
//!   call tree, line-table statement addresses give line hits, allocator entry points give heap
//!   lineage, and thread entry points, process exit and configured anchor functions give the
//!   timeline.
//! - **Phase B** is the last-writer log for chosen windows: [`writes::log_writes_by_stepping`]
//!   single-steps any target and decodes each instruction with iced-x86;
//!   [`writes::rerun`] drives `rr rerun --singlestep` shards and decodes their register dumps.
//! - **Storage** ([`store`]) is seekable zstd chunks plus a redb catalogue (chunk table,
//!   coverage per index kind, metadata). It persists incrementally.
//! - **Queries** live on [`Index`]: [`Index::calls`], [`Index::line_hits`],
//!   [`Index::allocation_at`], [`Index::timeline`], [`Index::last_write`] and
//!   [`Index::writes_in`]. Every answer says how complete it is.
//! - [`divergence`] aligns two event streams (cadabra3 decision traces) by site and per-site
//!   ordinal and names the first difference.
//!
//! [`Target`]: hopper_core::target::Target

pub mod config;
pub mod control;
pub mod divergence;
mod index;
mod phase_a;
pub mod store;
pub mod writes;

pub use config::{
    AllocatorTracking, FunctionSelection, IndexConfig, NamePattern, SourceSelection, StartPoint,
};
pub use control::IndexControl;
pub use index::{Answer, Index, IndexKind, RecordingMeta};
pub use phase_a::{AllocatorFunction, PhaseA, PhaseAPlan, PhaseASummary, allocator_function};
pub use writes::{LoggedWrite, WriteLogSummary, WriteValue};
