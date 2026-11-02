//! The M1 exit test, in live mode: at 20 scripted stops (a breakpoint at `main.rs:N` in three
//! fixtures), the source line and every local and argument match rust-gdb's
//! `gdb -batch -ex 'break main.rs:N' -ex run -ex 'info locals' -ex 'info args'`.
//!
//! The stops are deterministic ones: in `threads`, a stop while the spawned threads run sees
//! racing reference counts, and one after `handles.into_iter()` sees `handles`' stale, freed
//! bytes (moved out), which differ from run to run.
//!
//! Skips when gdb, gdbserver or the fixtures are missing.

mod support;

use support::gdb::{GdbRun, M1_STOPS, compare_stop, gdb_stop, same_value};
use support::*;

#[test]
fn the_comparison_itself() {
    // Elisions are compared as text: hopper writes `{...}` exactly where gdb does.
    assert!(same_value("Foo {a: {...}}", "Foo {a: {...}}"));
    assert!(!same_value("Foo {a: {...}}", "Foo {a: Bar (2)}"));
    assert!(same_value(
        "HashMap(size=2) = {[\"a\"] = 1, [\"b\"] = 2}",
        "HashMap(size=2) = {[\"b\"] = 2, [\"a\"] = 1}"
    ));
    assert!(!same_value(
        "HashMap(size=2) = {[\"a\"] = 1, [\"b\"] = 3}",
        "HashMap(size=2) = {[\"b\"] = 2, [\"a\"] = 1}"
    ));
}

#[test]
fn locals_match_rust_gdb_at_twenty_stops() {
    if !gdb_available() {
        eprintln!("skipping: gdb is not installed");
        return;
    }
    let mut checked = 0;
    let mut failures = Vec::new();
    for (fixture, line) in M1_STOPS {
        let Some(binary) = fixture_binary(fixture) else {
            return;
        };
        let gdb = gdb_stop(GdbRun::Live(&binary), line);
        let Some(d) = live_binary(&binary) else {
            return;
        };
        let stop = run_to_line(&d, "main.rs", line);
        let found = compare_stop(&d, &stop, &gdb);
        if found.is_empty() {
            checked += 1;
        }
        failures.extend(
            found
                .into_iter()
                .map(|f| format!("{fixture} main.rs:{line}: {f}")),
        );
    }
    assert!(
        failures.is_empty(),
        "{} of 20 stops differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(checked, 20);
}
