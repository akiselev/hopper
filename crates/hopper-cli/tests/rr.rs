//! `hopper record` and `hopper show` on a real rr trace. They need rr and a PMU:
//! `cargo test -p hopper-cli --test rr -- --ignored rr_`.

use std::io::Cursor;

use clap::Parser;
use hopper_cli::{Cli, run};

fn run_args(args: &[&str]) -> String {
    let cli = Cli::try_parse_from(args).expect("parses");
    let mut out = Vec::new();
    run(cli, Cursor::new(String::new()), &mut out).expect("runs");
    String::from_utf8(out).unwrap()
}

#[test]
#[ignore = "needs rr and a PMU"]
fn rr_record_then_show_and_info() {
    let bin = hopper_symbols::fixtures::fixture_binary("worked-example").expect("fixture");
    let dir = tempfile::tempdir().unwrap();
    let trace = dir.path().join("trace");
    let trace_s = trace.display().to_string();
    let out = run_args(&[
        "hopper",
        "record",
        "-o",
        &trace_s,
        "--",
        &bin.display().to_string(),
    ]);
    assert_eq!(out.trim(), trace_s);
    assert!(trace.join("version").is_file());

    let info = run_args(&["hopper", "info", &trace_s]);
    assert!(info.contains("rr: 5."), "{info}");
    assert!(info.contains("start: hopper://"), "{info}");
    assert!(info.contains("end:   hopper://"), "{info}");

    let show = run_args(&["hopper", "show", &trace_s, "--at", "main.rs:14"]);
    assert!(show.contains("  14 |     let c = b.clone();"), "{show}");
    assert!(
        show.contains(
            "b: worked_example::Config = worked_example::Config {name: \"alpha\", retries: 3}"
        ),
        "{show}"
    );
    // The position URL `show` printed opens the same stop.
    let url = show.lines().next().unwrap();
    let again = run_args(&["hopper", "show", &trace_s, "--at", url]);
    assert_eq!(again, show);
}
