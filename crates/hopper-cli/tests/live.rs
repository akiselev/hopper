//! `hopper live` and `hopper show --live` on a real process under gdbserver.
//!
//! Skips when gdbserver or the fixtures are missing.

use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

use clap::Parser;
use hopper_cli::{Cli, run};

fn worked_example() -> Option<PathBuf> {
    if Command::new("gdbserver").arg("--version").output().is_err() {
        eprintln!("skipping: gdbserver is not installed");
        return None;
    }
    hopper_symbols::fixtures::fixture_binary("worked-example")
}

fn run_args(args: &[&str], input: &str) -> String {
    let cli = Cli::try_parse_from(args).expect("parses");
    let mut out = Vec::new();
    run(cli, Cursor::new(input.to_string()), &mut out).expect("runs");
    String::from_utf8(out).unwrap()
}

#[test]
fn show_live_runs_to_a_line_and_prints_every_local() {
    let Some(bin) = worked_example() else { return };
    let bin = bin.display().to_string();
    let out = run_args(&["hopper", "show", "--live", "--at", "main.rs:14", "--", &bin], "");
    assert!(out.starts_with("hopper://"), "{out}");
    assert!(out.contains("worked_example::main at"), "{out}");
    assert!(out.contains("main.rs:14"), "{out}");
    assert!(
        out.contains("  14 |     let c = b.clone();"),
        "the source line: {out}"
    );
    assert!(out.contains("stack:"));
    assert!(out.contains("locals of #0 worked_example::main:"));
    assert!(
        out.contains("b: worked_example::Config = worked_example::Config {name: \"alpha\", retries: 3}"),
        "{out}"
    );
    assert!(out.contains("a: worked_example::Config = "), "{out}");
}

#[test]
fn live_repl_breaks_steps_and_prints() {
    let Some(bin) = worked_example() else { return };
    let bin = bin.display().to_string();
    let script = "b main.rs:16\nc\nlocals\nn\nwhere\np v[0].name\nbt\nrn\nc\nq\n";
    let out = run_args(&["hopper", "live", "--", &bin], script);
    let replies: Vec<&str> = out.split("(hopper) ").collect();
    assert!(replies[1].starts_with("breakpoint 1 at main.rs:16 (0x"), "{}", replies[1]);
    assert!(replies[2].contains("main.rs:16") && replies[2].contains("[breakpoint 1]"), "{}", replies[2]);
    assert!(replies[3].contains("c: worked_example::Config"), "{}", replies[3]);
    assert!(replies[4].contains("main.rs:17"), "{}", replies[4]);
    assert!(replies[5].starts_with("hopper://"), "{}", replies[5]);
    assert!(replies[6].contains("\"alpha\""), "{}", replies[6]);
    assert!(replies[7].contains("#0 worked_example::main at"), "{}", replies[7]);
    assert!(replies[8].starts_with("error: unsupported"), "{}", replies[8]);
    assert!(replies[9].contains("[exited 0]"), "{}", replies[9]);
}
