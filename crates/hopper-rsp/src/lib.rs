//! A blocking GDB Remote Serial Protocol client, with rr's extensions.
//!
//! [`RspClient`] runs over any [`Connection`] (a `TcpStream` in production, a
//! [`script::ScriptedConnection`] in tests). It handles framing, checksums, `}` escapes,
//! run-length decoding, ack and no-ack mode and `qSupported` negotiation; reads target
//! descriptions (with `xi:include`) and maps their registers to hopper-core [`hopper_core::Reg`]s
//! by name; reads registers, memory and `qXfer` objects; sets breakpoints and watchpoints;
//! resumes forward and backward; parses stop replies; and speaks rr's `vRun` seeks and `qRRCmd`
//! commands. Every packet format here was read from rr's server (`GdbServerConnection.cc`,
//! `GdbServer.cc`, `DebuggerExtensionCommandHandler.cc` at rr ca20dcb) or from a live gdbserver
//! session.

pub mod client;
pub mod conn;
pub mod packet;
pub mod script;
pub mod stop;
pub mod tdesc;
pub mod thread;
pub mod xml;

pub use client::{
    FeatureValue, InterruptHandle, ResumeRequest, RrSeek, RspClient, ServerFeatures, ThreadInfo,
    ThreadOp, VContActions,
};
pub use conn::Connection;
pub use stop::{SignalStop, StopCause, StopReply, gdb_signal_to_linux};
pub use tdesc::{RegisterDesc, TargetDescription};
pub use thread::{ThreadId, ThreadSelector};
