//! The engine's stepped windows ([`hopper_engine::stepped`]) as the tests capture them: a window
//! of a fixture from a line or a function's entry to the first hit of `main.rs:<line>`.

use std::path::Path;

use hopper_core::Result;
use hopper_core::target::Target;
#[allow(unused_imports, reason = "each test binary uses a different subset")]
pub use hopper_engine::stepped::{StepFactory, StepRecording, StepReplica};
use hopper_engine::stepped::{WindowEnd, WindowStart as Start};
use hopper_rr::{GdbserverConfig, GdbserverTarget};
use hopper_symbols::{Binary, Module, SymbolSet};

/// Where a captured window starts.
#[derive(Copy, Clone, Debug)]
pub enum WindowStart<'a> {
    /// The first hit of `main.rs:<line>`.
    Line(u32),
    /// The first instruction of a function (before its prologue).
    FunctionEntry(&'a str),
}

/// Capture `binary` from `from` to the first hit of `main.rs:to`, at most `max_steps`
/// instructions.
pub fn capture(
    binary: &Path,
    from: WindowStart<'_>,
    to: u32,
    max_steps: usize,
) -> Result<StepRecording> {
    // The fixture's addresses, from a run that is stopped at its first instruction.
    let auxv = GdbserverTarget::launch(&GdbserverConfig::default(), binary, &[])?.auxv()?;
    let mut syms = SymbolSet::new();
    syms.add(Module::main_from_auxv(Binary::open(binary)?, &auxv)?);
    let addr_of = |line: u32| {
        syms.addresses_for_line("main.rs", line)
            .into_iter()
            .find(|a| a.location.line == line)
            .map(|a| a.address)
            .ok_or_else(|| hopper_core::HopperError::NotFound(format!("main.rs:{line}")))
    };
    let start = match from {
        WindowStart::Line(l) => addr_of(l)?,
        WindowStart::FunctionEntry(name) => syms
            .find_functions(name)
            .first()
            .map(|f| f.entry)
            .ok_or_else(|| hopper_core::HopperError::NotFound(name.to_string()))?,
    };
    StepRecording::capture(
        &GdbserverConfig::default(),
        binary,
        &[],
        Start::Address(start),
        &WindowEnd::Address(addr_of(to)?),
        max_steps,
    )
}
