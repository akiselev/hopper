//! Source positions.

use std::fmt;

use serde::{Deserialize, Serialize};

/// A line (and optionally a column) in a source file. `file` is the path as the debug
/// information names it, after joining the compilation directory.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct SourceLoc {
    pub file: String,
    pub line: u32,
    pub column: Option<u32>,
}

impl SourceLoc {
    pub fn new(file: impl Into<String>, line: u32) -> Self {
        SourceLoc {
            file: file.into(),
            line,
            column: None,
        }
    }

    /// Whether `self` names the same file as `query`, matching by path suffix so that a user can
    /// say `main.rs:12` for `/home/me/proj/src/main.rs`.
    pub fn file_matches(&self, query: &str) -> bool {
        file_suffix_match(&self.file, query)
    }

    /// Whether the file is the program's own code: not in a toolchain or registry directory.
    pub fn is_own_code(&self) -> bool {
        !is_toolchain_path(&self.file)
    }
}

/// Toolchain and registry directories: never the program's own code.
pub fn is_toolchain_path(path: &str) -> bool {
    path.starts_with("/rustc/")
        || path.contains("/.rustup/")
        || path.contains("cargo/registry/")
        || path.contains("cargo/git/")
        || path.contains("/rustlib/")
}

/// `path` ends with `query` on a path-component boundary.
pub fn file_suffix_match(path: &str, query: &str) -> bool {
    if path == query {
        return true;
    }
    path.ends_with(query) && path[..path.len() - query.len()].ends_with('/')
}

impl fmt::Display for SourceLoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.column {
            Some(c) => write!(f, "{}:{}:{}", self.file, self.line, c),
            None => write!(f, "{}:{}", self.file, self.line),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_match_respects_components() {
        assert!(file_suffix_match("/a/src/main.rs", "main.rs"));
        assert!(file_suffix_match("/a/src/main.rs", "src/main.rs"));
        assert!(!file_suffix_match("/a/src/xmain.rs", "main.rs"));
        assert!(file_suffix_match("main.rs", "main.rs"));
    }

    #[test]
    fn toolchain_paths() {
        assert!(is_toolchain_path("/rustc/abc/library/std/src/rt.rs"));
        assert!(is_toolchain_path("/home/u/.cargo/registry/src/x/lib.rs"));
        assert!(is_toolchain_path("/usr/local/cargo/registry/src/x/lib.rs"));
        assert!(is_toolchain_path("/home/u/.cargo/git/checkouts/x/lib.rs"));
        assert!(!is_toolchain_path("/home/u/proj/src/main.rs"));
        assert!(SourceLoc::new("/home/u/proj/src/main.rs", 1).is_own_code());
        assert!(!SourceLoc::new("/rustc/abc/library/std/src/rt.rs", 1).is_own_code());
    }
}
