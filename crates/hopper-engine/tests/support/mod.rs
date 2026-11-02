//! Shared test support: fixtures, live sessions, and gdb's answers.

#![allow(dead_code, reason = "each test binary uses a different subset")]

use std::path::{Path, PathBuf};
use std::process::Command;

use hopper_core::api::{BreakLocation, Debugger, Stop, Until, ValueView};
use hopper_core::target::Direction;
use hopper_core::{FrameId, SourceLoc};
use hopper_engine::{Session, SessionOptions};

#[allow(unused_imports, reason = "each test binary uses a different subset")]
pub use hopper_symbols::fixtures::{fixture_binary, gdb_available, repo_root};

/// A live session on fixture `name`, or `None` (with a message) when gdbserver or the fixture is
/// missing.
pub fn live(name: &str) -> Option<Session> {
    live_binary(&fixture_binary(name)?)
}

pub fn live_binary(binary: &Path) -> Option<Session> {
    if Command::new("gdbserver").arg("--version").output().is_err() {
        eprintln!("skipping: gdbserver is not installed");
        return None;
    }
    Some(Session::live(binary, &[], SessionOptions::default()).expect("start a live session"))
}

/// Break at `file:line` and run to it.
pub fn run_to_line(d: &dyn Debugger, file: &str, line: u32) -> Stop {
    let bp = d
        .add_breakpoint(BreakLocation::Source(SourceLoc::new(file, line)))
        .expect("breakpoint resolves");
    let stop = d.run(Direction::Forward, Until::Breakpoints).expect("run");
    d.remove_breakpoint(bp.id).unwrap();
    stop
}

pub fn line_of(s: &Stop) -> Option<u32> {
    s.location.as_ref().map(|l| l.line)
}

pub fn top(s: &Stop) -> FrameId {
    FrameId {
        tid: s.tid,
        depth: 0,
    }
}

pub fn by_name<'a>(views: &'a [ValueView], name: &str) -> &'a ValueView {
    views
        .iter()
        .find(|v| v.name == name)
        .unwrap_or_else(|| panic!("no local {name} in {:?}", names(views)))
}

pub fn names(views: &[ValueView]) -> Vec<&str> {
    views.iter().map(|v| v.name.as_str()).collect()
}

/// The worked example built by `cargo hopper-build` (with its `.hsc`), building it when the
/// hopper-build tool is built. `None` with a message otherwise.
pub fn hopper_built_worked_example() -> Option<PathBuf> {
    let root = repo_root();
    let target = root.join("target/lane-d-build/fixtures/worked-example");
    let bin = target.join("debug/worked-example");
    let hsc = PathBuf::from(format!("{}.hsc", bin.display()));
    if bin.is_file() && hsc.is_file() {
        return Some(bin);
    }
    let tool = root.join("target/lane-d-build/debug/cargo-hopper-build");
    if !tool.is_file() {
        eprintln!("skipping: {} is not built", tool.display());
        return None;
    }
    let out = Command::new(&tool)
        .arg("hopper-build")
        .arg("--manifest-path")
        .arg(root.join("fixtures/worked-example/Cargo.toml"))
        .arg("--target-dir")
        .arg(&target)
        .current_dir(root.join("hopper-build"))
        .output()
        .ok()?;
    if bin.is_file() && hsc.is_file() {
        Some(bin)
    } else {
        eprintln!(
            "skipping: cargo hopper-build failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        None
    }
}

pub mod gdb;
pub mod step_recording;
