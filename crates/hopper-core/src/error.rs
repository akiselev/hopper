//! The one error type. Variants say what failed, not where.

use thiserror::Error;

pub type Result<T, E = HopperError> = std::result::Result<T, E>;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum HopperError {
    /// The remote end sent something the RSP client could not parse, or broke the protocol.
    #[error("protocol: {0}")]
    Protocol(String),
    /// The target (rr, gdbserver) answered with an error reply (`E nn`) or refused a request.
    #[error("target refused {request}: {reply}")]
    TargetRefused { request: String, reply: String },
    /// The connection or child process went away.
    #[error("target disconnected: {0}")]
    Disconnected(String),
    /// The target or engine does not offer this operation (for example reverse execution on a
    /// live gdbserver).
    #[error("unsupported: {0}")]
    Unsupported(String),
    /// Memory at this address could not be read at this moment.
    #[error("unreadable memory at {addr:#x} ({len} bytes)")]
    UnreadableMemory { addr: u64, len: u64 },
    /// No debug information covers what was asked (an address, a file and line, a symbol).
    #[error("not found: {0}")]
    NotFound(String),
    /// ELF/DWARF could not be read or is malformed.
    #[error("symbols: {0}")]
    Symbols(String),
    /// An expression or a DWARF location could not be evaluated.
    #[error("eval: {0}")]
    Eval(String),
    /// The value was optimised out at this point.
    #[error("optimised out")]
    OptimisedOut,
    /// A sidecar's build id does not match the binary.
    #[error("stale sidecar: {0}")]
    StaleSidecar(String),
    /// Index storage failed.
    #[error("index: {0}")]
    Index(String),
    /// A moment named a different recording, or one the recording does not contain.
    #[error("bad moment: {0}")]
    BadMoment(String),
    /// The work was cancelled.
    #[error("cancelled")]
    Cancelled,
    /// A bounded search ran out of budget before finishing.
    #[error("budget exhausted after {steps} steps")]
    BudgetExhausted { steps: u64 },
    /// Operating-system I/O failed.
    #[error("io: {0}")]
    Io(String),
    /// Invalid input from a caller (a malformed URL, an unknown option).
    #[error("invalid: {0}")]
    Invalid(String),
}

impl From<std::io::Error> for HopperError {
    fn from(e: std::io::Error) -> Self {
        HopperError::Io(e.to_string())
    }
}
