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

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use hopper_core::api::{Debugger, ValueTarget, ValueView};
use support::*;

/// The 20 stops: (fixture, line).
const STOPS: [(&str, u32); 20] = [
    ("worked-example", 7),
    ("worked-example", 8),
    ("worked-example", 12),
    ("worked-example", 13),
    ("worked-example", 14),
    ("worked-example", 15),
    ("worked-example", 16),
    ("worked-example", 17),
    ("worked-example", 18),
    ("values", 25),
    ("values", 30),
    ("values", 35),
    ("values", 40),
    ("values", 45),
    ("values", 52),
    ("values", 55),
    ("values", 58),
    ("threads", 18),
    ("threads", 21),
    ("threads", 20),
];

/// gdb's line and locals at the first hit of `main.rs:line`, with Rust's pretty printers.
struct GdbStop {
    line: u32,
    /// Innermost binding of each name.
    locals: BTreeMap<String, String>,
}

fn rust_gdb_scripts() -> Option<String> {
    let out = Command::new("rustc")
        .args(["--print", "sysroot"])
        .output()
        .ok()?;
    let root = String::from_utf8(out.stdout).ok()?;
    let dir = Path::new(root.trim()).join("lib/rustlib/etc");
    dir.join("gdb_lookup.py")
        .exists()
        .then(|| dir.display().to_string())
}

fn gdb_stop(binary: &Path, line: u32) -> GdbStop {
    let mut cmd = Command::new("gdb");
    cmd.args(["-batch", "-nx", "-iex", "set debuginfod enabled off"]);
    cmd.args(["-iex", "set auto-load safe-path /"]);
    if let Some(d) = rust_gdb_scripts() {
        cmd.args(["-iex", &format!("directory {d}")]);
        cmd.args(["-iex", &format!("python import sys; sys.path.insert(0, '{d}')")]);
    }
    cmd.args(["-ex", "set pagination off", "-ex", "set width 0"]);
    cmd.args(["-ex", "set print elements 1000"]);
    cmd.args(["-ex", &format!("break main.rs:{line}"), "-ex", "run"]);
    cmd.args([
        "-ex",
        "python print('@@LINE', gdb.selected_frame().find_sal().line)",
    ]);
    cmd.args(["-ex", "echo @@LOCALS\\n", "-ex", "info locals", "-ex", "info args"]);
    cmd.args(["-ex", "echo @@END\\n"]);
    cmd.args(["-ex", "kill"]);
    cmd.arg(binary);
    let out = cmd.output().expect("run gdb");
    let text = String::from_utf8_lossy(&out.stdout);
    let gline = text
        .lines()
        .find_map(|l| l.strip_prefix("@@LINE "))
        .and_then(|l| l.trim().parse().ok())
        .unwrap_or_else(|| panic!("gdb did not stop at main.rs:{line}:\n{text}"));
    let body = text
        .split("@@LOCALS\n")
        .nth(1)
        .and_then(|b| b.split("@@END").next())
        .unwrap_or("");
    let mut locals = BTreeMap::new();
    for l in body.lines() {
        if let Some((name, value)) = l.split_once(" = ")
            && !name.contains(' ')
        {
            // gdb lists the innermost block first, then the arguments: keep the first binding
            // of a name.
            locals
                .entry(name.to_string())
                .or_insert_with(|| value.to_string());
        }
    }
    GdbStop {
        line: gline,
        locals,
    }
}

/// gdb prefixes a top-level pointer with its type; hopper keeps the type apart.
fn without_pointer_cast(text: &str) -> &str {
    if (text.starts_with("(*") || text.starts_with("(&"))
        && let Some(i) = text.find(") 0x")
    {
        return &text[i + 2..];
    }
    text
}

/// The items of a `{a, b, c}` list at the top level of `s` (after its first `{`).
fn top_level_items(s: &str) -> Option<Vec<String>> {
    let open = s.find('{')?;
    let body = s[open + 1..].strip_suffix('}')?;
    let mut items = Vec::new();
    let mut depth = 0i32;
    let mut cur = String::new();
    let mut in_str = false;
    for c in body.chars() {
        match c {
            '"' => in_str = !in_str,
            '{' | '(' | '[' if !in_str => depth += 1,
            '}' | ')' | ']' if !in_str => depth -= 1,
            ',' if depth == 0 && !in_str => {
                items.push(cur.trim().to_string());
                cur.clear();
                continue;
            }
            _ => {}
        }
        cur.push(c);
    }
    items.push(cur.trim().to_string());
    Some(items)
}

/// Whether `theirs` starting at byte `i` begins with one balanced expression; its end.
fn balanced_end(theirs: &str, i: usize, stop: Option<char>) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_str = false;
    for (k, c) in theirs[i..].char_indices() {
        if depth == 0 && !in_str && Some(c) == stop && k > 0 {
            return Some(i + k);
        }
        match c {
            '"' => in_str = !in_str,
            '{' | '(' | '[' if !in_str => depth += 1,
            '}' | ')' | ']' if !in_str => {
                depth -= 1;
                if depth < 0 {
                    return Some(i + k);
                }
            }
            _ => {}
        }
    }
    (depth == 0).then_some(theirs.len())
}

/// hopper's text equals gdb's, where each `{...}` hopper wrote (it stops nesting at a smaller
/// depth than gdb) stands for one balanced value in gdb's.
fn matches_with_elisions(mine: &str, theirs: &str) -> bool {
    let pieces: Vec<&str> = mine.split("{...}").collect();
    let mut at = 0usize;
    for (k, piece) in pieces.iter().enumerate() {
        if !theirs[at..].starts_with(piece) {
            return false;
        }
        at += piece.len();
        if k + 1 < pieces.len() {
            let stop = pieces[k + 1].chars().next();
            match balanced_end(theirs, at, stop) {
                Some(e) if e > at => at = e,
                _ => return false,
            }
        }
    }
    at == theirs.len()
}

/// The same value: equal text, up to hopper's depth elisions, with a HashMap's entries compared
/// as a set (each process seeds its hasher at random, so iteration order differs between runs).
fn same_value(mine: &str, theirs: &str) -> bool {
    if matches_with_elisions(mine, theirs) {
        return true;
    }
    if mine.starts_with("HashMap(") && theirs.starts_with("HashMap(") {
        let head = |s: &str| s.split(" = {").next().map(str::to_string);
        if let (Some(mut a), Some(mut b)) = (top_level_items(mine), top_level_items(theirs)) {
            a.sort();
            b.sort();
            return head(mine) == head(theirs) && a == b;
        }
    }
    false
}

#[test]
fn the_comparison_itself() {
    assert!(same_value("Foo {a: {...}}", "Foo {a: Bar (2)}"));
    assert!(same_value("X ({...}), Y", "X (Z {v: 1}), Y"));
    assert!(!same_value("Foo {a: {...}}", "Foo {b: 1}"));
    assert!(same_value(
        "HashMap(size=2) = {[\"a\"] = 1, [\"b\"] = 2}",
        "HashMap(size=2) = {[\"b\"] = 2, [\"a\"] = 1}"
    ));
    assert!(!same_value(
        "HashMap(size=2) = {[\"a\"] = 1, [\"b\"] = 3}",
        "HashMap(size=2) = {[\"b\"] = 2, [\"a\"] = 1}"
    ));
}

/// hopper's innermost binding of each name (shadowed bindings are named by place).
fn hopper_locals(views: &[ValueView]) -> BTreeMap<String, String> {
    views
        .iter()
        .filter(|v| matches!(v.handle.target, ValueTarget::Expr(_)))
        .map(|v| (v.name.clone(), v.summary.clone()))
        .collect()
}

#[test]
fn locals_match_rust_gdb_at_twenty_stops() {
    if !gdb_available() {
        eprintln!("skipping: gdb is not installed");
        return;
    }
    let mut checked = 0;
    let mut failures = Vec::new();
    for (fixture, line) in STOPS {
        let Some(binary) = fixture_binary(fixture) else {
            return;
        };
        let gdb = gdb_stop(&binary, line);
        let Some(d) = live_binary(&binary) else { return };
        let stop = run_to_line(&d, "main.rs", line);
        let ours_line = line_of(&stop).unwrap_or(0);
        if ours_line != gdb.line {
            failures.push(format!(
                "{fixture} main.rs:{line}: hopper stopped at line {ours_line}, gdb at {}",
                gdb.line
            ));
            continue;
        }
        let ours = hopper_locals(&d.locals(stop.at, top(&stop)).unwrap());
        let ours_names: Vec<&String> = ours.keys().collect();
        let gdb_names: Vec<&String> = gdb.locals.keys().collect();
        if ours_names != gdb_names {
            failures.push(format!(
                "{fixture} main.rs:{line}: locals {ours_names:?}, gdb {gdb_names:?}"
            ));
            continue;
        }
        for (name, theirs) in &gdb.locals {
            let mine = &ours[name];
            if !same_value(mine, without_pointer_cast(theirs)) {
                failures.push(format!(
                    "{fixture} main.rs:{line}: {name} = {mine}, gdb {theirs}"
                ));
            }
        }
        checked += 1;
    }
    assert!(
        failures.is_empty(),
        "{} of 20 stops differ:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert_eq!(checked, 20);
}
