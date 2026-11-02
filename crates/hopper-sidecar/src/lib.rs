//! The `.hsc` ownership sidecar: its format, and the join from a PC to an ownership event.
//!
//! `hopper-build` writes a sidecar beside each binary it builds: per codegen'd function instance,
//! the ownership events of its final optimised MIR (moves, copies, clones, borrows, projections,
//! aggregates, literals, call returns, drops, storage ends), each with its span, inline chain,
//! MIR location, source-level places and type. The format is specified in `docs/formats/hsc.md`.
//!
//! The engine uses it three ways:
//! - [`Sidecar::open_for_binary`] loads it and rejects it when the binary's GNU build-id differs
//!   ([`hopper_core::HopperError::StaleSidecar`]);
//! - [`Sidecar::join`] turns a PC (its DWARF frame, callers and observed machine effect) into an
//!   [`hopper_core::api::OwnershipLabel`] with a [`hopper_core::api::Confidence`];
//! - [`Sidecar::user_frame`], [`Sidecar::call_transfer`] and [`Sidecar::returned_value`] lift
//!   the labels of a chain's machine hops to user-level events (`c` → `v[0]` for `v.push(c)`,
//!   `a ← load()` for a returned value);
//! - [`Sidecar::local_events`] and [`ownership_state`] say whether a local is live, moved out or
//!   dropped.
//!
//! This crate builds on stable and on hopper-build's pinned nightly, which uses its writer
//! ([`link::write_partial`], [`link::PartialSet`]).

pub mod codec;
pub mod dump;
pub mod join;
pub mod link;
pub mod local;
pub mod model;
pub mod sidecar;
pub mod source;

pub use join::{Access, Choice, DwarfInline, FrameInfo, Join, MachineEffect, PcInfo};
pub use local::{LocalEvent, LocalTransition, ownership_state};
pub use model::{
    Binding, BuildId, CalleeRole, Caveat, ControlPath, CrateUnit, FunctionOrigin, FunctionRecord,
    Header, InlineFrame, LineCol, MirLocation, MirPosition, OwnershipEvent, SidecarData, Span,
    TypeInfo, UnitOutput,
};
pub use source::{SourceEvent, UserFrame};
pub use sidecar::{EventRef, Sidecar, build_id_of, read_build_id, sidecar_path};
