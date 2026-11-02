//! The CLI's commands against the fake.

use std::io::Cursor;

use clap::Parser;
use hopper_cli::{Cli, open, open_debugger, repl, resolve_position, run, show};
use hopper_core::HopperError;
use hopper_core::api::Debugger;
use hopper_fake::FakeDebugger;

fn repl_output(script: &str) -> String {
    let d = FakeDebugger::new();
    let mut out = Vec::new();
    repl(&d, Cursor::new(script.to_string()), &mut out).unwrap();
    String::from_utf8(out).unwrap()
}

/// The output of each command, split at the prompts (index 0 is the banner).
fn replies(script: &str) -> Vec<String> {
    repl_output(script)
        .split("(hopper) ")
        .map(str::to_string)
        .collect()
}

fn run_args(args: &[&str], input: &str) -> Result<String, HopperError> {
    let cli = Cli::try_parse_from(args).map_err(|e| HopperError::Invalid(e.to_string()))?;
    let mut out = Vec::new();
    run(cli, Cursor::new(input.to_string()), &mut out)?;
    Ok(String::from_utf8(out).unwrap())
}

#[test]
fn opening_a_trace_goes_through_open_debugger() {
    let err = open_debugger("/some/trace".as_ref()).err().unwrap();
    assert!(matches!(err, HopperError::NotFound(_)), "{err}");
    assert!(err.to_string().contains("not an rr trace directory"));
    assert!(open(Some("/some/trace".as_ref()), false).is_err());
    assert!(open(None, false).is_err());
    assert!(open(Some("/ignored".as_ref()), true).is_ok());
    assert!(
        run_args(&["hopper", "info", "/some/trace"], "")
            .unwrap_err()
            .to_string()
            .contains("not an rr trace directory")
    );
    assert!(
        run_args(&["hopper", "live"], "").is_err(),
        "live needs a program"
    );
    assert!(
        run_args(&["hopper", "show", "--live", "--at", "start"], "").is_err(),
        "--live needs a program after --"
    );
}

#[test]
fn cargo_hopper_build_reports_its_executables() {
    let report = "   Compiling x\nhopper-build: /t/debug/a -> /t/debug/a.hsc\nhopper-build: /t/debug/b -> /t/debug/b.hsc\n";
    assert_eq!(
        hopper_cli::built_executables(report),
        vec![std::path::PathBuf::from("/t/debug/a"), "/t/debug/b".into()]
    );
    let e = hopper_cli::hopper_build("/nonexistent".as_ref(), None).unwrap_err();
    assert!(e.to_string().contains("not a cargo project"));
}

#[test]
fn every_subcommand_takes_fake() {
    assert!(
        run_args(&["hopper", "info"], "").is_err(),
        "a trace or --fake is required"
    );
    let info = run_args(&["hopper", "info", "--fake"], "").unwrap();
    assert!(info.contains("recording 5eed0fa4"));
    assert!(info.contains("index: done"));
    let rec = run_args(&["hopper", "record", "--fake"], "").unwrap();
    assert!(rec.contains("fake:worked-example"));
    let repl = run_args(&["hopper", "repl", "--fake"], "where\nq\n").unwrap();
    assert!(repl.contains("hopper://"));
    let show = run_args(&["hopper", "show", "--fake", "--at", "start"], "").unwrap();
    assert!(show.contains("main.rs:12"));
}

#[test]
fn show_prints_line_stack_and_every_local() {
    let d = FakeDebugger::new();
    let at = d.first_moment_at("Clone", 3).unwrap();
    let mut out = Vec::new();
    show(&d, &at.url().to_string(), &mut out).unwrap();
    let out = String::from_utf8(out).unwrap();
    assert!(out.starts_with(&at.url().to_string()));
    assert!(out.contains("   3 | #[derive(Clone)]"), "{out}");
    assert!(out.contains("#0 <worked_example::Config as core::clone::Clone>::clone"));
    assert!(
        out.contains("#1 worked_example::main at /hopper/fixtures/worked-example/src/main.rs:14")
    );
    assert!(
        out.contains("self: &worked_example::Config = &Config { name: \"alpha\", retries: 3 }")
    );
    assert!(out.contains("a: worked_example::Config = Config { name: \"alpha\", retries: 3 }  [moved out to b at main.rs:13]"));
    assert!(out.contains("c: worked_example::Config = <uninitialised>"));
    // show reads at a moment; it does not move the cursor for a URL.
    assert_eq!(d.cursor().unwrap().at, d.moments()[0]);
    let mut out = Vec::new();
    assert!(show(&d, "hopper://garbage", &mut out).is_err());
}

#[test]
fn positions_resolve_from_urls_events_and_names() {
    let d = FakeDebugger::new();
    let info = d.recording().unwrap();
    assert_eq!(resolve_position(&d, "start").unwrap(), info.start.unwrap());
    assert_eq!(resolve_position(&d, "end").unwrap(), info.end.unwrap());
    let m = d.moments()[20];
    assert_eq!(resolve_position(&d, &m.url().to_string()).unwrap(), m);
    // Event 4 starts after the write syscall inside _print.
    let e4 = resolve_position(&d, "4").unwrap();
    assert_eq!(e4.frame_time, 4);
    let before = d
        .moments()
        .into_iter()
        .rev()
        .find(|x| x.frame_time < 4)
        .unwrap();
    assert!(before < e4);
    assert!(resolve_position(&d, "999").is_err());
    assert!(resolve_position(&d, "soon").is_err());
}

#[test]
fn repl_navigates_forward_and_back() {
    let r = replies("n\nn\ns\nfinish\nrn\nrs\nrfinish\nsi\nrsi\nq\n");
    assert!(r[0].contains("main.rs:12"));
    assert!(r[1].contains("main.rs:13"));
    assert!(r[2].contains("main.rs:14"));
    assert!(r[3].contains("Clone>::clone at /hopper/fixtures/worked-example/src/main.rs:3"));
    assert!(r[4].contains("worked_example::main at") && r[4].contains("main.rs:14"));
    assert!(
        r[5].contains("main.rs:14"),
        "reverse-next goes to the start of line 14: {}",
        r[5]
    );
    assert!(r[6].contains("main.rs:13"));
    assert!(
        r[7].contains("[start of recording]"),
        "rfinish from main: {}",
        r[7]
    );
    let after_si = r[8].lines().next().unwrap().to_string();
    let after_rsi = r[9].lines().next().unwrap().to_string();
    assert_ne!(after_si, after_rsi);
    assert_eq!(r.len(), 11, "q ends the session");
}

#[test]
fn repl_breakpoints_continue_and_reverse_continue() {
    let r = replies("b main.rs:16\nb load\nb nowhere_fn\nc\nc\nc\nrc\nbt\nq\n");
    assert!(r[1].starts_with("breakpoint 1 at main.rs:16 (0x"));
    assert!(r[2].starts_with("breakpoint 2 at load"));
    assert!(r[3].starts_with("error: not found"));
    assert!(r[4].contains("worked_example::load") && r[4].contains("[breakpoint 2]"));
    assert!(r[5].contains("main.rs:16") && r[5].contains("[breakpoint 1]"));
    assert!(r[6].contains("[end of recording]"));
    assert!(r[7].contains("[breakpoint 1]"));
    assert!(r[8].contains("#0 worked_example::main"));
}

#[test]
fn repl_values_and_queries() {
    let r = replies(
        "b main.rs:18\nc\nlocals\np v\np b.retries\norigin *n\nhistory a\ncalls push\ncalls\nhits main.rs:16\np\nq\n",
    );
    let locals = &r[3];
    for name in ["a:", "b:", "c:", "v:", "n:"] {
        assert!(locals.contains(name), "{locals}");
    }
    assert!(locals.contains("c: worked_example::Config = Config { name: \"alpha\", retries: 3 }  [moved out to v[0] at main.rs:16]"));
    assert!(r[4].contains("v: alloc::vec::Vec<worked_example::Config> = Vec(len=1)"));
    assert!(r[4].contains("  [0]: worked_example::Config"));
    assert_eq!(r[5].trim(), "b.retries: u32 = 3");
    let origin = &r[6];
    let kinds: Vec<&str> = [
        "SharedBorrow v[0].name -> n",
        "Move c -> v[0]",
        "Clone b -> c",
        "Move a -> b",
        "CallReturn load() -> a",
        "Literal \"alpha\" -> name",
    ]
    .into_iter()
    .filter(|k| origin.contains(k))
    .collect();
    assert_eq!(kinds.len(), 6, "{origin}");
    assert!(origin.contains("heap allocation #3"));
    assert!(origin.contains("end: Literal"));
    assert_eq!(r[7].lines().count(), 3, "{}", r[7]);
    assert!(r[7].lines().last().unwrap().contains("moved out to b"));
    assert!(r[8].contains("alloc::vec::Vec<T,A>::push #4"));
    assert!(r[9].contains("  <alloc::string::String as core::convert::From<&str>>::from #2"));
    assert_eq!(r[10].lines().count(), 1);
    assert!(r[11].starts_with("error: invalid: this command needs an expression"));
}

#[test]
fn repl_navigation_history_and_urls() {
    let d = FakeDebugger::new();
    let m = d.first_moment_at("main", 17).unwrap();
    let script = format!(
        "where\nseek {}\nwhere\nback\nwhere\nfwd\nfwd\nback\nback\nseek nope\nhelp\nwat\n",
        m.url()
    );
    let r = replies(&script);
    let start = r[1].trim().to_string();
    assert!(start.starts_with("hopper://"));
    assert!(r[2].contains("main.rs:17"));
    assert_eq!(r[3].trim(), m.url().to_string());
    assert!(r[4].starts_with(&start));
    assert_eq!(r[5].trim(), start);
    assert!(r[6].contains("main.rs:17"));
    assert_eq!(r[7].trim(), "no later position");
    assert!(r[8].starts_with(&start));
    assert_eq!(r[9].trim(), "no earlier position");
    assert!(r[10].starts_with("error: invalid: not a hopper position url"));
    assert!(r[11].contains("origin <expr>"));
    assert!(r[12].contains("unknown command `wat`"));
    // End of input ends the REPL without `q`.
    assert!(repl_output("where\n").ends_with("(hopper) \n"));
}

#[test]
fn mcp_and_dap_subcommands_serve_stdio() {
    let init = r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#;
    let call = r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"recording_info","arguments":{}}}"#;
    let out = run_args(&["hopper", "mcp", "--fake"], &format!("{init}\n{call}\n")).unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2);
    assert!(lines[1].contains("\"isError\":false"));

    let frame = |s: &str| format!("Content-Length: {}\r\n\r\n{s}", s.len());
    let input = [
        frame(r#"{"seq":1,"type":"request","command":"initialize","arguments":{}}"#),
        frame(r#"{"seq":2,"type":"request","command":"launch","arguments":{"recording":"/x"}}"#),
        frame(r#"{"seq":3,"type":"request","command":"disconnect","arguments":{}}"#),
    ]
    .concat();
    let out = run_args(&["hopper", "dap"], &input).unwrap();
    assert!(
        out.contains("not an rr trace directory"),
        "a recording goes through open_debugger"
    );
    let out = run_args(&["hopper", "dap", "--fake"], &input).unwrap();
    assert!(
        out.contains("\"event\":\"initialized\""),
        "--fake opens the fake whatever launch says"
    );
}
