//! Builds `fixtures/worked-example` with `cargo hopper-build` and checks the sidecar against the
//! plan's worked example: the events each line implies, the six-hop relabelling through the join,
//! the moved-out state of `a`, and the build-id check.
//!
//! The fixture's line numbers are two higher than the plan's table (its comments give them).

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use hopper_core::api::{Confidence, OwnershipKind as K, OwnershipState};
use hopper_core::{HopperError, SourceLoc};
use hopper_sidecar::dump::event_line;
use hopper_sidecar::{
    Access, ControlPath, FrameInfo, FunctionOrigin, MachineEffect, OwnershipEvent, PcInfo, Sidecar,
    ownership_state, sidecar_path,
};

fn fixture_manifest() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/worked-example/Cargo.toml")
}

fn target_dir() -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("worked-example")
}

/// Build the fixture once per profile and return the binary.
fn build(release: bool) -> PathBuf {
    static DEBUG: OnceLock<PathBuf> = OnceLock::new();
    static RELEASE: OnceLock<PathBuf> = OnceLock::new();
    let cell = if release { &RELEASE } else { &DEBUG };
    cell.get_or_init(|| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_cargo-hopper-build"));
        cmd.arg("hopper-build")
            .arg("--manifest-path")
            .arg(fixture_manifest())
            .arg("--target-dir")
            .arg(target_dir());
        if release {
            cmd.arg("--release");
        }
        let out = cmd.output().expect("run cargo-hopper-build");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(out.status.success(), "cargo hopper-build failed:\n{stderr}");
        let bin = target_dir().join(if release { "release" } else { "debug" }).join("worked-example");
        let sc = sidecar_path(&bin);
        assert!(stderr.contains(&format!("{} -> {}", bin.display(), sc.display())), "{stderr}");
        bin
    })
    .clone()
}

fn sidecar(release: bool) -> Sidecar {
    let bin = build(release);
    Sidecar::open_for_binary(&sidecar_path(&bin), &bin).expect("sidecar matches its binary")
}

fn function<'a>(sc: &'a Sidecar, def_path: &str) -> &'a hopper_sidecar::FunctionRecord {
    sc.functions()
        .iter()
        .find(|f| f.def_path == def_path)
        .unwrap_or_else(|| panic!("no function {def_path}"))
}

fn golden(name: &str) -> Vec<String> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden").join(format!("{name}.txt"));
    std::fs::read_to_string(&path).unwrap().lines().map(str::to_owned).collect()
}

fn lines(events: &[OwnershipEvent]) -> Vec<String> {
    events.iter().map(event_line).collect()
}

/// The events of `f` on `line` with this kind, as (src, dst).
fn on_line(f: &hopper_sidecar::FunctionRecord, line: u32, kind: K) -> Vec<(Option<&str>, Option<&str>)> {
    f.events
        .iter()
        .filter(|e| e.span.lo.line == line && e.kind == kind && e.path == ControlPath::Normal)
        .map(|e| (e.src.as_deref(), e.dst.as_deref()))
        .collect()
}

#[test]
fn header_names_the_build() {
    let sc = sidecar(false);
    let h = sc.header();
    assert_eq!(h.crates, ["worked_example"]);
    assert!(h.rustc_version.starts_with("rustc 1.97.0-nightly"), "{}", h.rustc_version);
    assert!(h.tool.starts_with("hopper-build "), "{}", h.tool);
}

#[test]
fn events_the_plan_implies() {
    let sc = sidecar(false);
    let load = function(&sc, "load");
    let main = function(&sc, "main");
    assert_eq!(load.origin, FunctionOrigin::Source);
    assert!(load.def_span.file.ends_with("fixtures/worked-example/src/main.rs"), "{}", load.def_span.file);

    // Line 7: the literal and `String::from`'s return into `name`.
    assert_eq!(on_line(load, 7, K::Literal), [(Some("\"alpha\""), Some("<String as From<&str>>::from(arg 0)"))]);
    assert_eq!(on_line(load, 7, K::CallReturn), [(Some("<String as From<&str>>::from(..)"), Some("name"))]);
    let from = load.events.iter().find(|e| e.kind == K::CallReturn).unwrap();
    assert_eq!(from.callee.as_deref(), Some("<std::string::String as std::convert::From<&str>>::from"));
    assert_eq!(from.callee_role(), Some(hopper_sidecar::CalleeRole::Conversion));

    // Line 8: the aggregate, with `name` moved in.
    assert_eq!(on_line(load, 8, K::Aggregate), [(None, Some("return"))]);
    assert_eq!(on_line(load, 8, K::Move), [(Some("name"), Some("return.name"))]);
    assert_eq!(on_line(load, 8, K::Literal), [(Some("3"), Some("return.retries"))]);

    // Line 12: `load()`'s return into `a`.
    assert_eq!(on_line(main, 12, K::CallReturn), [(Some("load(..)"), Some("a"))]);
    // Line 13: the move of `a` into `b`, a 32-byte `Config`.
    assert_eq!(on_line(main, 13, K::Move), [(Some("a"), Some("b"))]);
    let mv = main.events.iter().find(|e| e.span.lo.line == 13).unwrap();
    assert_eq!(mv.ty.as_ref().map(|t| (t.name.as_str(), t.size)), Some(("Config", Some(32))));
    // Line 14: the clone from `b` into `c`, recognised by its callee.
    assert_eq!(on_line(main, 14, K::Clone), [(Some("b"), Some("c"))]);
    let clone = main.events.iter().find(|e| e.kind == K::Clone).unwrap();
    assert_eq!(clone.callee_role(), Some(hopper_sidecar::CalleeRole::Clone));
    // Line 16: `c` moved into `push`'s argument.
    assert!(on_line(main, 16, K::Move).contains(&(Some("c"), Some("Vec::<Config>::push(arg 1)"))));
    // Line 17: the borrow of the projection `v[0].name`, through `Index::index`.
    assert_eq!(on_line(main, 17, K::Projection), [(Some("v[0]"), Some("v[0].name"))]);
    assert_eq!(on_line(main, 17, K::SharedBorrow), [(Some("v"), Some("&v")), (Some("v[0].name"), Some("n"))]);
    // End of `main`: `v` and `b` are dropped on the normal path; `a` and `c` were moved, so
    // they have no normal-path drop (`c`'s drop is on the unwind path only).
    assert_eq!(on_line(main, 19, K::Drop), [(Some("v"), None), (Some("b"), None)]);
    assert!(main.events.iter().any(|e| e.kind == K::Drop && e.src.as_deref() == Some("c") && e.path == ControlPath::Unwind));

    // Safe user code carries no caveats; std code is never pruned on.
    assert!(main.events.iter().chain(&load.events).all(|e| e.caveats.is_empty()));
    let push = function(&sc, "std::vec::Vec::<Config>::push");
    assert_eq!(push.origin, FunctionOrigin::Dependency);
    assert!(push.events.iter().all(|e| !e.prunable()));
    let derive = function(&sc, "<Config as std::clone::Clone>::clone");
    assert_eq!(derive.origin, FunctionOrigin::MacroExpansion);
}

#[test]
fn user_functions_match_the_golden_dump() {
    let sc = sidecar(false);
    assert_eq!(lines(&function(&sc, "load").events), golden("load"));
    assert_eq!(lines(&function(&sc, "main").events), golden("main"));
    assert_eq!(lines(&function(&sc, "<Config as std::clone::Clone>::clone").events), golden("config_clone"));
}

fn at(line: u32, col: u32) -> Option<SourceLoc> {
    Some(SourceLoc {
        // As DWARF names it: the compilation directory joined with `src/main.rs`.
        file: fixture_manifest().parent().unwrap().canonicalize().unwrap().join("src/main.rs").to_string_lossy().into_owned(),
        line,
        column: Some(col),
    })
}

fn std_loc(file: &str, line: u32) -> Option<SourceLoc> {
    Some(SourceLoc { file: format!("/rustc/library/{file}"), line, column: None })
}

fn frame(sc: &Sidecar, def_path: &str, location: Option<SourceLoc>) -> FrameInfo {
    FrameInfo::new(function(sc, def_path).symbol.clone(), location)
}

fn written(place: Option<&str>, size: u64) -> Option<MachineEffect> {
    Some(MachineEffect { read: None, written: Some(Access { size, place: place.map(str::to_owned) }) })
}

fn label(sc: &Sidecar, pc: &PcInfo) -> (K, String, String, u32, Confidence) {
    let j = sc.join(pc);
    let l = j.label().unwrap_or_else(|| panic!("no label for {pc:?}: {j:?}"));
    (l.kind, l.src.clone().unwrap_or_default(), l.dst.clone().unwrap_or_default(), l.span.as_ref().unwrap().line, l.confidence)
}

/// The plan's table, hop by hop, with PC information written by hand the way the engine will
/// pass it (the PC's frame from DWARF, its callers, the machine effect).
#[test]
fn six_hop_relabelling() {
    let sc = sidecar(false);
    let s = |x: &str| x.to_owned();

    // Hop 1 (plan line 15): the store of `n`'s pointer.
    let hop1 = PcInfo { frame: frame(&sc, "main", at(17, 13)), callers: vec![], effect: written(Some("n"), 8) };
    assert_eq!(label(&sc, &hop1), (K::SharedBorrow, s("v[0].name"), s("n"), 17, Confidence::Exact));

    // Hop 2 (plan line 14): `Vec::push`'s memcpy into the heap, reached from line 16.
    let hop2 = PcInfo {
        frame: frame(&sc, "std::vec::Vec::<Config>::push_mut", std_loc("core/src/ptr/mod.rs", 1920)),
        callers: vec![
            frame(&sc, "std::vec::Vec::<Config>::push", std_loc("alloc/src/vec/mod.rs", 1004)),
            frame(&sc, "main", at(16, 7)),
        ],
        effect: written(None, 32),
    };
    assert_eq!(label(&sc, &hop2), (K::Move, s("c"), s("Vec::<Config>::push(arg 1)"), 16, Confidence::Inferred));

    // Hop 3 (plan line 12): the 5 bytes `<[u8]>::to_vec` copies inside `String::clone`, inside
    // the derived `Config::clone`, called at line 14.
    let to_vec = sc
        .functions()
        .iter()
        .find(|f| f.def_path.contains("ConvertVec>::to_vec"))
        .expect("to_vec instance")
        .symbol
        .clone();
    let hop3 = PcInfo {
        frame: FrameInfo::new(to_vec.clone(), std_loc("alloc/src/slice.rs", 454)),
        callers: vec![
            FrameInfo::new("<alloc::string::String as core::clone::Clone>::clone", std_loc("alloc/src/string.rs", 2120)),
            frame(&sc, "<Config as std::clone::Clone>::clone", at(4, 17)),
            frame(&sc, "main", at(14, 15)),
        ],
        effect: written(None, 5),
    };
    assert_eq!(label(&sc, &hop3), (K::Clone, s("b"), s("c"), 14, Confidence::Inferred));

    // Hop 4 (plan line 11): the stack copy of `a` into `b`.
    let hop4 = PcInfo {
        frame: frame(&sc, "main", at(13, 13)),
        callers: vec![],
        effect: Some(MachineEffect { read: Some(Access::of("a", 32)), written: Some(Access::of("b", 32)) }),
    };
    assert_eq!(label(&sc, &hop4), (K::Move, s("a"), s("b"), 13, Confidence::Exact));

    // Hop 5 (plan lines 10 and 6): `a` is `load()`'s return value, built by the aggregate with
    // `name` moved in.
    let hop5 = PcInfo { frame: frame(&sc, "main", at(12, 13)), callers: vec![], effect: written(Some("a"), 32) };
    assert_eq!(label(&sc, &hop5), (K::CallReturn, s("load(..)"), s("a"), 12, Confidence::Exact));
    let hop5_inside = PcInfo {
        frame: frame(&sc, "load", at(8, 5)),
        callers: vec![frame(&sc, "main", at(12, 13))],
        effect: Some(MachineEffect { read: Some(Access::of("name", 24)), written: Some(Access::sized(24)) }),
    };
    assert_eq!(label(&sc, &hop5_inside), (K::Move, s("name"), s("return.name"), 8, Confidence::Inferred));

    // Hop 6 (plan line 5): the memcpy from `.rodata` inside `String::from`, called at line 7.
    let hop6 = PcInfo {
        frame: FrameInfo::new(to_vec, std_loc("alloc/src/slice.rs", 454)),
        callers: vec![
            frame(&sc, "<std::string::String as std::convert::From<&str>>::from", std_loc("alloc/src/string.rs", 2950)),
            frame(&sc, "load", at(7, 16)),
        ],
        effect: written(None, 5),
    };
    assert_eq!(
        label(&sc, &hop6),
        (K::CallReturn, s("<String as From<&str>>::from(..)"), s("name"), 7, Confidence::Inferred)
    );
}

#[test]
fn a_is_moved_out_after_line_13() {
    let sc = sidecar(false);
    let main = sc.function_named("main").expect("main by def path");
    let a = sc.local_events(main, "a");
    // Line 12 executed: live.
    assert_eq!(ownership_state(a.iter().filter(|e| e.span.lo.line <= 12).map(|e| (e, None))), OwnershipState::Live);
    // Line 13 executed: moved out into `b`, although DWARF still shows `a` as a valid Config.
    match ownership_state(a.iter().filter(|e| e.span.lo.line <= 13).map(|e| (e, None))) {
        OwnershipState::MovedOut { to, at, .. } => {
            assert_eq!(to.as_deref(), Some("b"));
            assert_eq!(at.map(|l| (l.line, l.column)), Some((13, Some(13))));
        }
        other => panic!("{other:?}"),
    }
    // At the end of main: `a` stays moved out, `c` too (moved into the Vec), `b` and `v` are
    // dropped, `n` is live.
    let state = |name: &str| ownership_state(sc.local_events(main, name).iter().map(|e| (e, None)));
    assert!(matches!(state("a"), OwnershipState::MovedOut { .. }));
    assert!(matches!(state("c"), OwnershipState::MovedOut { .. }));
    assert!(matches!(state("b"), OwnershipState::Dropped { .. }));
    assert!(matches!(state("v"), OwnershipState::Dropped { .. }));
    assert_eq!(state("n"), OwnershipState::Live);
}

#[test]
fn a_sidecar_is_rejected_for_another_binary() {
    let debug = build(false);
    let release = build(true);
    match Sidecar::open_for_binary(&sidecar_path(&debug), &release) {
        Err(HopperError::StaleSidecar(_)) => {}
        other => panic!("expected StaleSidecar, got {other:?}"),
    }
}

#[test]
fn release_build_records_mir_inlining_and_degrades_to_renames() {
    let sc = sidecar(true);
    let main = function(&sc, "main");
    // The MIR inliner put the derived clone into `main`: its events carry the inline chain,
    // with the call site on line 14.
    let inlined: Vec<&OwnershipEvent> = main
        .events
        .iter()
        .filter(|e| e.inline_chain.first().is_some_and(|f| f.callee.ends_with("Clone>::clone")))
        .collect();
    assert!(!inlined.is_empty());
    assert!(inlined.iter().all(|e| e.inline_chain[0].call_site.lo.line == 14));
    // With no machine copy observed, the move at line 13 is a heuristic rename.
    let rename = PcInfo { frame: frame(&sc, "main", at(13, 13)), callers: vec![], effect: None };
    assert_eq!(label(&sc, &rename), (K::Move, "a".into(), "b".into(), 13, Confidence::Heuristic));
}
