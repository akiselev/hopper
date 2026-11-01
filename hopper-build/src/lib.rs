//! hopper-build: `cargo hopper-build` builds a Rust workspace with the pinned nightly and writes an
//! `.hsc` ownership sidecar beside every binary it produces.
//!
//! Two binaries share this library:
//! - `cargo-hopper-build` ([`cli`]) runs `cargo build` with `hopper-build-driver` as the
//!   workspace rustc wrapper, then links the per-crate partial sidecars into `<binary>.hsc`;
//! - `hopper-build-driver` ([`plugin`]) is rustc plus one callback: after analysis it walks the
//!   optimised MIR of every instance the crate codegens ([`extract`]) and writes the crate's
//!   partial sidecar. Compilation then continues unchanged, so the sidecar and the binary come
//!   from the same rustc invocation.

#![feature(rustc_private)]

extern crate rustc_driver;
extern crate rustc_hir;
extern crate rustc_interface;
extern crate rustc_middle;
extern crate rustc_session;
extern crate rustc_span;

pub mod cli;
pub mod extract;
pub mod plugin;

/// The tool name and version written into every sidecar header.
pub fn tool_version() -> String {
    format!("hopper-build {}", env!("CARGO_PKG_VERSION"))
}
