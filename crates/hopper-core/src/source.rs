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
}
