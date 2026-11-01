//! ELF and DWARF for hopper.
//!
//! - [`Binary`]: one ELF file (sections, symbol table, build id, DWARF), read once and shared.
//! - [`Module`]: a binary plus its load bias; [`SymbolSet`]: several modules, looked up by
//!   runtime PC.
//! - PC to source: [`SymbolSet::location`], [`SymbolSet::frames_at`] (the inline chain),
//!   [`SymbolSet::function_at`].
//! - Lookups the other way: [`SymbolSet::find_functions`] (full path, path suffix, symbol table),
//!   [`SymbolSet::addresses_for_line`] (file and line, as gdb's `break file:line`),
//!   [`SymbolSet::breakpoint_address`] (past the prologue).
//! - Stepping helpers: [`SymbolSet::line_range_at`], [`SymbolSet::function_bounds`].
//! - Unwinding: [`SymbolSet::unwind`] (physical frames from CFI, falling back to the `rbp`
//!   chain) and [`SymbolSet::stack`] (source frames, with inlined calls expanded).
//! - Scopes: [`SymbolSet::scope_at`] lists the variables of one frame for the evaluator.
//! - [`disasm`] (iced-x86) and [`corefile`] (ELF core files: memory, threads, mapped files).
//!
//! All `SymbolSet` addresses are runtime addresses; `Binary` and [`DwarfIndex`] work in file
//! addresses.

pub mod auxv;
mod binary;
mod cfi;
pub mod corefile;
pub mod disasm;
mod dwarf;
#[cfg(feature = "test-fixtures")]
pub mod fixtures;
mod lines;
mod module;
pub mod names;
mod scope;
mod unwind;

pub use binary::{Binary, BinaryKind, BuildId, ElfSymbol, LoadSegment, SymbolKind};
pub use dwarf::{DieRef, DwarfIndex, Reader, RustcVersion, UnitInfo};
pub use lines::{LineRow, LineTable};
pub use module::{
    FunctionInfo, FunctionOrigin, InlineFrame, LineAddress, LineRange, Module, ModuleId, SymbolHit,
    SymbolSet,
};
pub use scope::{FrameScope, Variable, VariableKind};
pub use unwind::{MAX_FRAMES, PhysicalFrame, StackFrame, UnwindMethod};

/// Re-exported so dependants name exactly the gimli this crate was built with.
pub use gimli;
