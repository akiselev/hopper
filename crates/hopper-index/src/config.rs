//! What phase A indexes.

use serde::{Deserialize, Serialize};

/// A function-name pattern, matched against demangled names without hashes
/// (`worked_example::load`, `<alloc::vec::Vec<u8>>::push`).
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NamePattern {
    /// The whole name.
    Exact(String),
    /// A `::`-separated path suffix: `load` matches `worked_example::load`, not `reload`.
    PathSuffix(String),
    /// Any substring.
    Contains(String),
}

impl NamePattern {
    pub fn matches(&self, name: &str) -> bool {
        match self {
            NamePattern::Exact(p) => name == p,
            NamePattern::PathSuffix(p) => hopper_symbols::names::path_suffix_match(name, p),
            NamePattern::Contains(p) => name.contains(p.as_str()),
        }
    }
}

/// Which source files count as "the program's own code".
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum SourceSelection {
    /// Files of the binary's own crates: paths under a Rust compile unit's compilation directory
    /// that is not a toolchain or registry directory (`/rustc/<hash>`, `~/.rustup`,
    /// `~/.cargo/registry`, `~/.cargo/git`). Excludes std, core, alloc and dependencies from
    /// crates.io.
    #[default]
    CrateFiles,
    /// Every file with line-table rows.
    All,
    /// Files whose path ends with one of these (on a component boundary, as `main.rs:12` does).
    Files(Vec<String>),
    /// No files.
    Nothing,
}

/// Which functions get entry and return breakpoints for the call tree.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum FunctionSelection {
    /// Functions whose entry address maps to a [`SourceSelection::CrateFiles`] file.
    #[default]
    CrateFiles,
    /// Every function in the main executable's symbol table.
    All,
    /// Functions (of every module) matching one of the patterns.
    Matching(Vec<NamePattern>),
    /// No call tree.
    Nothing,
}

/// Which allocator entry points phase A breaks on.
#[derive(Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AllocatorTracking {
    /// Rust's allocator shims (`__rust_*`, `__rdl_*`, `__rg_*`) and libc's malloc family,
    /// whichever are present (see [`crate::allocator_function`]).
    #[default]
    RustAndLibc,
    /// No allocator lineage.
    Off,
}

/// Where the target is when phase A starts.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum StartPoint {
    /// At the first instruction of the recording (a fresh replica or a freshly launched
    /// gdbserver): coverage starts with the recording.
    #[default]
    RecordingStart,
    /// Somewhere later: coverage starts at the target's current moment.
    Here,
}

/// Phase A's configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexConfig {
    /// Files whose statement addresses are recorded as line hits.
    pub lines: SourceSelection,
    /// Functions recorded in the call tree.
    pub functions: FunctionSelection,
    pub allocator: AllocatorTracking,
    /// Functions whose entries are timeline anchors. cadabra3's decision-trace emitters are the
    /// motivating case: `PathSuffix("trace_log::push_cmp")`, `PathSuffix("trace_log::record_decision")`.
    pub anchors: Vec<NamePattern>,
    /// Functions whose entry starts a thread and whose return ends it. The default is Rust std's
    /// `<std::sys::thread::unix::Thread>::new::thread_start` (any path ending in
    /// `Thread>::new::thread_start` or `::thread_start` inside `std::sys`).
    pub thread_entries: Vec<NamePattern>,
    /// Frames skipped when naming the function that requested an allocation: the allocator's
    /// own layers (`alloc::`, `core::`, `__rustc::`, ...). A name starts with one of these to be
    /// skipped (a leading `<` is ignored, so `<alloc::vec::Vec<T>>::push` counts as `alloc::`).
    pub allocator_frames: Vec<String>,
    /// After this many hits on one breakpoint address the breakpoint is removed and the kind's
    /// coverage for that address becomes partial from that moment.
    pub max_hits_per_address: u64,
    /// Records buffered before a chunk is written and committed.
    pub flush_every: usize,
    /// How many physical frames to unwind when naming an allocation's requester.
    pub requester_unwind_depth: usize,
    pub start: StartPoint,
}

impl Default for IndexConfig {
    fn default() -> Self {
        IndexConfig {
            lines: SourceSelection::CrateFiles,
            functions: FunctionSelection::CrateFiles,
            allocator: AllocatorTracking::RustAndLibc,
            anchors: Vec::new(),
            thread_entries: vec![NamePattern::PathSuffix("thread_start".into())],
            allocator_frames: [
                "alloc::",
                "core::",
                "std::alloc",
                "std::sys::alloc",
                "__rustc::",
                "__rust_",
                "__rdl_",
                "__rg_",
                "hashbrown::raw",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            max_hits_per_address: 100_000,
            flush_every: 4096,
            requester_unwind_depth: 32,
            start: StartPoint::RecordingStart,
        }
    }
}

impl IndexConfig {
    /// Whether `name` is one of the allocator's own frames (see [`IndexConfig::allocator_frames`]).
    pub fn is_allocator_frame(&self, name: &str) -> bool {
        let n = name.strip_prefix('<').unwrap_or(name);
        self.allocator_frames
            .iter()
            .any(|p| n.starts_with(p.as_str()))
    }
}

/// Toolchain and registry directories: never the program's own code.
pub(crate) fn is_toolchain_path(path: &str) -> bool {
    path.starts_with("/rustc/")
        || path.contains("/.rustup/")
        || path.contains("/.cargo/registry/")
        || path.contains("/.cargo/git/")
        || path.contains("/rustlib/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns_match_as_documented() {
        assert!(NamePattern::PathSuffix("load".into()).matches("worked_example::load"));
        assert!(!NamePattern::PathSuffix("load".into()).matches("worked_example::reload"));
        assert!(NamePattern::Exact("a::b".into()).matches("a::b"));
        assert!(NamePattern::Contains("push".into()).matches("<alloc::vec::Vec<u8>>::push"));
        assert!(
            NamePattern::PathSuffix("thread_start".into())
                .matches("<std::sys::thread::unix::Thread>::new::thread_start")
        );
    }

    #[test]
    fn allocator_frames_skip_std_layers() {
        let c = IndexConfig::default();
        assert!(c.is_allocator_frame("<alloc::vec::Vec<u8>>::push"));
        assert!(c.is_allocator_frame("alloc::raw_vec::finish_grow"));
        assert!(c.is_allocator_frame("__rustc::__rust_alloc"));
        assert!(!c.is_allocator_frame("worked_example::load"));
        assert!(!c.is_allocator_frame("<worked_example::Config as core::clone::Clone>::clone"));
    }

    #[test]
    fn toolchain_paths() {
        assert!(is_toolchain_path("/rustc/abc/library/std/src/rt.rs"));
        assert!(is_toolchain_path("/home/u/.cargo/registry/src/x/lib.rs"));
        assert!(!is_toolchain_path("/home/u/proj/src/main.rs"));
    }
}
