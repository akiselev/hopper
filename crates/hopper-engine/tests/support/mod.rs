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
    // Tests of a live session's own behaviour skip the reference run (tests/live_reference.rs
    // covers it), which would run each fixture a second time.
    let options = SessionOptions {
        live_index: hopper_engine::LiveIndexPolicy::Off,
        ..SessionOptions::default()
    };
    Some(Session::live(binary, &[], options).expect("start a live session"))
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

/// The plan's six source hops of the worked example's click on `*n` at line 18 (the plan's table,
/// at the fixture's lines, which are the plan's plus two), as hopper-engine's source view must
/// produce them: (kind, text the source contains, destination, line, confidence), and the events
/// underneath each as (kind, text the source contains, destination, line).
pub fn assert_six_source_hops(chain: &hopper_core::api::OriginChain) {
    use hopper_core::api::{Confidence::Exact, OwnershipKind as K};
    type Inner = &'static [(K, &'static str, &'static str, u32)];
    let plan: [(K, &str, &str, u32, Inner); 6] = [
        (K::SharedBorrow, "v[0].name", "n", 17, &[]),
        (K::Move, "c", "v[0]", 16, &[]),
        (K::Clone, "b", "c", 14, &[]),
        (K::Move, "a", "b", 13, &[]),
        (
            K::CallReturn,
            "load",
            "a",
            12,
            &[
                (K::Aggregate, "Config { name, .. }", "return", 8),
                (K::Move, "name", "return.name", 8),
            ],
        ),
        (
            K::CallReturn,
            "String as From",
            "name",
            7,
            &[(K::Literal, "\"alpha\"", "(arg 0)", 7)],
        ),
    ];
    let hops = hopper_engine::source_hops(chain);
    let shown: Vec<String> = hops.iter().map(source_hop_text).collect();
    assert_eq!(hops.len(), 6, "{shown:#?}");
    for (h, (kind, src, dst, line, inner)) in hops.iter().zip(plan) {
        let o = h.ownership.as_ref().expect("source hops are labelled");
        assert_eq!(o.kind, kind, "{shown:#?}");
        assert!(
            o.src.as_deref().is_some_and(|s| s.contains(src)),
            "{src}: {shown:#?}"
        );
        assert_eq!(o.dst.as_deref(), Some(dst), "{shown:#?}");
        assert_eq!(o.span.as_ref().map(|l| l.line), Some(line), "{shown:#?}");
        assert_eq!(o.confidence, Exact, "{shown:#?}");
        assert_eq!(h.inner.len(), inner.len(), "{shown:#?}");
        for (i, (kind, src, dst, line)) in h.inner.iter().zip(inner) {
            assert_eq!(i.kind, *kind, "{shown:#?}");
            assert!(
                i.src.as_deref().is_some_and(|s| s.contains(src)),
                "{shown:#?}"
            );
            assert!(
                i.dst.as_deref().is_some_and(|d| d.ends_with(dst)),
                "{shown:#?}"
            );
            assert_eq!(i.span.as_ref().map(|l| l.line), Some(*line), "{shown:#?}");
        }
    }
}

/// One source hop as text: `17 SharedBorrow v[0].name -> n (Exact)`, with the events underneath.
pub fn source_hop_text(h: &hopper_core::api::OriginHop) -> String {
    let label = |o: &hopper_core::api::OwnershipLabel| {
        format!(
            "{} {:?} {} -> {} ({:?})",
            o.span.as_ref().map_or(0, |l| l.line),
            o.kind,
            o.src.as_deref().unwrap_or("-"),
            o.dst.as_deref().unwrap_or("-"),
            o.confidence
        )
    };
    let mut s = h.ownership.as_ref().map(label).unwrap_or_default();
    for i in &h.inner {
        s.push_str(&format!("\n    {}", label(i)));
    }
    s
}

pub mod gdb;
pub mod step_recording;
