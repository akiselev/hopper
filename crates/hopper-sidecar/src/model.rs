//! The records a `.hsc` file holds, as plain Rust values.
//!
//! These are what `hopper-build` produces and what the join reads. The byte layout is in
//! `docs/formats/hsc.md` and in [`crate::codec`].

use std::fmt;

use hopper_core::SourceLoc;
use hopper_core::api::OwnershipKind;
use serde::{Deserialize, Serialize};

/// A GNU build-id (the `NT_GNU_BUILD_ID` note's descriptor bytes).
#[derive(Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BuildId(pub Vec<u8>);

impl BuildId {
    pub fn to_hex(&self) -> String {
        self.0.iter().map(|b| format!("{b:02x}")).collect()
    }
}

impl fmt::Display for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for BuildId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "BuildId({})", self.to_hex())
    }
}

/// What a sidecar describes: a linked binary (identified by its build-id) or one crate's
/// compilation before linking (a partial that `hopper-build` merges later).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Binding {
    Binary { build_id: BuildId },
    Partial(CrateUnit),
}

/// One crate compilation, as the driver saw it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrateUnit {
    pub name: String,
    /// rustc's `StableCrateId`: unique per crate name, `-C metadata` and target.
    pub stable_crate_id: u64,
    pub output: UnitOutput,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnitOutput {
    /// A library (rlib and the like); merged into the executables that link it.
    Library,
    /// An executable or shared object: the path rustc wrote, and the stable crate ids of every
    /// crate it links, so the merge can pick that crate's partials.
    Linked { path: String, upstream: Vec<u64> },
}

/// The sidecar's header.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Header {
    pub binding: Binding,
    /// `rustc -vV`'s first line, for example `rustc 1.97.0-nightly (f53b654a8 2026-04-30)`.
    pub rustc_version: String,
    /// The crates whose functions the sidecar holds.
    pub crates: Vec<String>,
    /// The tool that wrote the file and its version, for example `hopper-build 0.1.0`.
    pub tool: String,
}

/// A line and column, both 1-based; the column counts bytes from the start of the line, as the
/// DWARF columns rustc emits do.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct LineCol {
    pub line: u32,
    pub col: u32,
}

/// A source range: `lo` inclusive, `hi` exclusive (rustc's convention).
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Span {
    pub file: String,
    pub lo: LineCol,
    pub hi: LineCol,
}

impl Span {
    /// The start of the span as a [`SourceLoc`], which is what a DWARF line row names.
    pub fn start(&self) -> SourceLoc {
        SourceLoc {
            file: self.file.clone(),
            line: self.lo.line,
            column: Some(self.lo.col),
        }
    }

    /// Whether `loc` (a line, and a column when it has one) falls inside this span.
    pub fn contains(&self, loc: &SourceLoc) -> bool {
        if !files_match(&self.file, &loc.file) {
            return false;
        }
        let at = LineCol {
            line: loc.line,
            col: loc.column.unwrap_or(0),
        };
        match loc.column {
            Some(_) => {
                self.lo <= at
                    && at
                        < self.hi.max(LineCol {
                            line: self.lo.line,
                            col: self.lo.col + 1,
                        })
            }
            None => self.lo.line <= loc.line && loc.line <= self.hi.line,
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}:{}:{}-{}:{}",
            self.file, self.lo.line, self.lo.col, self.hi.line, self.hi.col
        )
    }
}

/// Two file names name the same file when one is a path-component suffix of the other: the
/// sidecar keeps the path rustc saw, DWARF joins it with the compilation directory, and users
/// type `main.rs`.
/// Both are compared after lexical normalisation (`a/./b` and `a/x/../b` are `a/b`).
pub fn files_match(a: &str, b: &str) -> bool {
    use hopper_core::source::file_suffix_match;
    if file_suffix_match(a, b) || file_suffix_match(b, a) {
        return true;
    }
    let (a, b) = (normalize_path(a), normalize_path(b));
    file_suffix_match(&a, &b) || file_suffix_match(&b, &a)
}

/// Remove `.` components and resolve `..` against the preceding component, without touching the
/// file system. A leading `..` that cannot be resolved is kept.
pub fn normalize_path(path: &str) -> String {
    let absolute = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." if parts.last().is_some_and(|p| *p != "..") => {
                parts.pop();
            }
            ".." if absolute => {}
            other => parts.push(other),
        }
    }
    let joined = parts.join("/");
    if absolute {
        format!("/{joined}")
    } else {
        joined
    }
}

/// One step of an inline chain: `callee`'s body was inlined at `call_site`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct InlineFrame {
    /// The callee's def path with generic arguments, for example `std::mem::replace::<u32>`.
    pub callee: String,
    pub call_site: Span,
}

/// Where in a MIR body an event happens.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct MirLocation {
    pub block: u32,
    pub position: MirPosition,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum MirPosition {
    /// The statement with this index in its block.
    Statement(u32),
    /// The block's terminator (a call, a drop, a return).
    Terminator,
}

impl fmt::Display for MirLocation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.position {
            MirPosition::Statement(i) => write!(f, "bb{}[{}]", self.block, i),
            MirPosition::Terminator => write!(f, "bb{}[term]", self.block),
        }
    }
}

/// A type as the event names it.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TypeInfo {
    /// The type as rustc prints it with trimmed paths (`String`, `Vec<Config>`, `&u32`).
    pub name: String,
    /// Its size in bytes, when its layout is known.
    pub size: Option<u64>,
}

/// Why ownership reasoning must not prune on an event: its labels still describe what MIR did,
/// but borrowck's guarantees do not cover it.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Caveat {
    /// The statement is inside an `unsafe` block or an `unsafe fn`.
    Unsafe,
    /// A place the event reads or writes has interior mutability (contains an `UnsafeCell`).
    InteriorMutability,
}

/// When an event's code runs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ControlPath {
    /// On the normal path.
    Normal,
    /// On the normal path, but only when a drop flag says the value is still owned (a drop of a
    /// local that may have been moved out).
    DropFlagGuarded,
    /// Only while unwinding from a panic (a MIR cleanup block).
    Unwind,
}

/// One ownership event of one function.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct OwnershipEvent {
    pub kind: OwnershipKind,
    /// The span of the MIR statement or terminator (the span codegen gives its instructions).
    pub span: Span,
    /// The MIR inlining chain, outermost first; empty when the statement is the function's own.
    pub inline_chain: Vec<InlineFrame>,
    pub location: MirLocation,
    pub path: ControlPath,
    /// Source-level place the value comes from (`a`, `b.name`, `v[0].name`, `*self`), or the
    /// literal's text for [`OwnershipKind::Literal`], or the call for
    /// [`OwnershipKind::CallReturn`] (`load()`).
    pub src: Option<String>,
    /// Source-level place the value goes to.
    pub dst: Option<String>,
    /// The type of the value that moves, is copied, borrowed, built or dropped.
    pub ty: Option<TypeInfo>,
    /// For calls and drops: the callee's def path with generic arguments.
    pub callee: Option<String>,
    /// Sorted, without duplicates.
    pub caveats: Vec<Caveat>,
}

impl OwnershipEvent {
    /// Whether the engine may prune a backward search on this event.
    pub fn prunable(&self) -> bool {
        self.caveats.is_empty()
    }

    pub fn callee_role(&self) -> Option<CalleeRole> {
        self.callee.as_deref().and_then(CalleeRole::of)
    }
}

/// Callees the join treats specially.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CalleeRole {
    /// `<T as Clone>::clone`.
    Clone,
    /// `ToOwned::to_owned`, `From::from`, `Into::into` and `ToString::to_string`: calls that
    /// build an owned value from another.
    Conversion,
    /// The global allocator or `Box::new`'s allocation shim.
    Allocator,
    /// `core::ptr::drop_in_place` and `Drop::drop`.
    Drop,
}

impl CalleeRole {
    /// Recognise a callee from its def path (as the driver writes it: no trimmed paths).
    pub fn of(path: &str) -> Option<CalleeRole> {
        let base = strip_generic_args(path);
        if base.ends_with("as std::clone::Clone>::clone")
            || base.ends_with("as core::clone::Clone>::clone")
            || base == "std::clone::Clone::clone"
            || base == "core::clone::Clone::clone"
        {
            return Some(CalleeRole::Clone);
        }
        const CONVERSIONS: [&str; 4] = [
            "borrow::ToOwned>::to_owned",
            "convert::From<",
            "convert::Into<",
            "string::ToString>::to_string",
        ];
        if CONVERSIONS.iter().any(|c| path.contains(c))
            || base.ends_with("::from") && path.contains(" as ")
        {
            return Some(CalleeRole::Conversion);
        }
        const ALLOCATORS: [&str; 9] = [
            "alloc::alloc::alloc",
            "alloc::alloc::alloc_zeroed",
            "alloc::alloc::realloc",
            "alloc::alloc::dealloc",
            "alloc::alloc::exchange_malloc",
            "alloc::alloc::Global",
            "std::alloc::alloc",
            "std::alloc::realloc",
            "std::alloc::dealloc",
        ];
        if ALLOCATORS
            .iter()
            .any(|a| base.starts_with(a) || base.contains(&format!("<{a}")))
            || base.starts_with("__rust_alloc")
            || base.starts_with("__rust_realloc")
            || base.starts_with("__rust_dealloc")
            || base.contains("as std::alloc::GlobalAlloc>")
            || base.contains("as core::alloc::GlobalAlloc>")
            || base.contains("as std::alloc::Allocator>")
            || base.contains("as core::alloc::Allocator>")
        {
            return Some(CalleeRole::Allocator);
        }
        if base.ends_with("ptr::drop_in_place") || base.ends_with("ops::Drop>::drop") {
            return Some(CalleeRole::Drop);
        }
        None
    }
}

/// `std::ptr::drop_in_place::<String>` → `std::ptr::drop_in_place`; generic arguments inside a
/// qualified self type (`<Vec<T> as Clone>::clone`) are kept, only a trailing `::<..>` goes.
fn strip_generic_args(path: &str) -> &str {
    match path.rfind("::<") {
        Some(i) if path.ends_with('>') => &path[..i],
        _ => path,
    }
}

/// Where a function's code comes from.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FunctionOrigin {
    /// Written in the source.
    Source,
    /// Generated by a macro expansion (`#[derive(Clone)]`, for example). The join treats such a
    /// frame as part of its caller when it walks outwards.
    MacroExpansion,
    /// A compiler shim (drop glue, a vtable shim, a closure's `FnOnce` shim).
    Shim,
    /// An instance of a function defined in another crate (a std generic such as
    /// `Vec::<Config>::push`), codegen'd in this one.
    Dependency,
}

/// One codegen'd function instance and its events, in MIR order (block, then statement).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionRecord {
    /// The mangled instance symbol, as the symbol table and `DW_AT_linkage_name` name it.
    pub symbol: String,
    /// The symbol demangled without its hash, for example `worked_example::load`.
    pub demangled: String,
    /// The def path with generic arguments, as rustc prints it.
    pub def_path: String,
    pub krate: String,
    pub def_span: Span,
    pub origin: FunctionOrigin,
    pub events: Vec<OwnershipEvent>,
}

/// A whole sidecar's contents.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SidecarData {
    pub header: Header,
    pub functions: Vec<FunctionRecord>,
}

/// Render an [`OwnershipKind`] the way dumps and the format spec name it.
pub fn kind_name(kind: OwnershipKind) -> &'static str {
    match kind {
        OwnershipKind::Move => "move",
        OwnershipKind::Copy => "copy",
        OwnershipKind::Clone => "clone",
        OwnershipKind::SharedBorrow => "shared-borrow",
        OwnershipKind::MutBorrow => "mut-borrow",
        OwnershipKind::Projection => "projection",
        OwnershipKind::Drop => "drop",
        OwnershipKind::Aggregate => "aggregate",
        OwnershipKind::Literal => "literal",
        OwnershipKind::CallReturn => "call-return",
        OwnershipKind::StorageDead => "storage-dead",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn span(l0: u32, c0: u32, l1: u32, c1: u32) -> Span {
        Span {
            file: "src/main.rs".into(),
            lo: LineCol { line: l0, col: c0 },
            hi: LineCol { line: l1, col: c1 },
        }
    }

    #[test]
    fn span_contains_by_line_and_column() {
        let s = span(13, 13, 13, 14);
        let mut loc = SourceLoc::new("/w/src/main.rs", 13);
        assert!(s.contains(&loc));
        loc.column = Some(13);
        assert!(s.contains(&loc));
        loc.column = Some(5);
        assert!(!s.contains(&loc));
        assert!(!s.contains(&SourceLoc::new("/w/src/lib.rs", 13)));
        // An empty span still contains its start.
        assert!(span(19, 1, 19, 1).contains(&SourceLoc {
            file: "main.rs".into(),
            line: 19,
            column: Some(1)
        }));
    }

    #[test]
    fn paths_normalise_lexically() {
        assert_eq!(normalize_path("/a/b/../c/./main.rs"), "/a/c/main.rs");
        assert_eq!(normalize_path("../x/./y"), "../x/y");
        assert_eq!(normalize_path("/../a"), "/a");
        assert!(files_match(
            "/w/hopper-build/../fixtures/src/main.rs",
            "/w/fixtures/src/main.rs"
        ));
        assert!(files_match("./src/main.rs", "/w/src/main.rs"));
        assert!(!files_match("/w/a/../b/main.rs", "/w/a/main.rs"));
    }

    #[test]
    fn callee_roles() {
        assert_eq!(
            CalleeRole::of("<Config as std::clone::Clone>::clone"),
            Some(CalleeRole::Clone)
        );
        assert_eq!(
            CalleeRole::of("<std::string::String as std::clone::Clone>::clone"),
            Some(CalleeRole::Clone)
        );
        assert_eq!(
            CalleeRole::of("<std::string::String as std::convert::From<&str>>::from"),
            Some(CalleeRole::Conversion)
        );
        assert_eq!(
            CalleeRole::of("alloc::alloc::exchange_malloc"),
            Some(CalleeRole::Allocator)
        );
        assert_eq!(CalleeRole::of("__rust_alloc"), Some(CalleeRole::Allocator));
        assert_eq!(
            CalleeRole::of("std::ptr::drop_in_place::<std::string::String>"),
            Some(CalleeRole::Drop)
        );
        assert_eq!(CalleeRole::of("std::vec::Vec::<Config>::push"), None);
        assert_eq!(CalleeRole::of("load"), None);
    }

    #[test]
    fn strip_generics() {
        assert_eq!(
            strip_generic_args("std::ptr::drop_in_place::<String>"),
            "std::ptr::drop_in_place"
        );
        assert_eq!(
            strip_generic_args("<Vec<T> as Clone>::clone"),
            "<Vec<T> as Clone>::clone"
        );
    }
}
