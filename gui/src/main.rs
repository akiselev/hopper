//! `hopper-gui <trace>` opens an rr trace (through `hopper_cli::open_debugger`, as the CLI, MCP
//! and DAP do); `hopper-gui --live -- <prog> [args]` debugs a program live under gdbserver,
//! forward only; `hopper-gui --fake` opens the scripted worked example.
//! `--shot <out.png> [--size <w>x<h>] [--keys F10,Alt+Left] [--select <local>]` renders the window
//! headlessly.

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use hopper_core::api::Debugger;

const USAGE: &str = "usage: hopper-gui (--fake | <trace-dir> | --live) [--shot <out.png> [--size <w>x<h>] [--keys <chord>,...] [--select <local>]] [-- <prog> [args] (with --live)]";

#[derive(Debug, Default)]
struct Args {
    fake: bool,
    trace: Option<PathBuf>,
    /// `--live`: the program and its arguments, after `--`.
    live: Option<Vec<OsString>>,
    shot: Option<PathBuf>,
    size: Option<(f32, f32)>,
    keys: Vec<String>,
    /// `--select`: a local to select before the shot.
    select: Option<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    let mut live = false;
    while let Some(x) = it.next() {
        match x.as_str() {
            "--fake" => a.fake = true,
            "--live" => live = true,
            "--" => {
                let program: Vec<OsString> = it.by_ref().map(OsString::from).collect();
                if !live {
                    return Err("a program after -- needs --live".into());
                }
                a.live = Some(program);
            }
            "--shot" => a.shot = Some(it.next().ok_or("--shot needs a path")?.into()),
            "--size" => {
                let v = it.next().ok_or("--size needs <w>x<h>")?;
                let (w, h) = v.split_once('x').ok_or("--size needs <w>x<h>")?;
                a.size = Some((w.parse().map_err(|_| "bad --size")?, h.parse().map_err(|_| "bad --size")?));
            }
            "--select" => a.select = Some(it.next().ok_or("--select needs a local's name")?.clone()),
            "--keys" => a.keys = it.next().ok_or("--keys needs a list")?.split(',').map(str::to_owned).collect(),
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => a.trace = Some(s.into()),
        }
    }
    match (&a.live, live) {
        (Some(p), true) if !p.is_empty() => {}
        (_, true) => return Err("--live needs the program after --".into()),
        _ => {}
    }
    Ok(a)
}

fn main() -> ExitCode {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    let args = match parse(&argv) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("hopper-gui: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let opened = match (&args.trace, args.fake, &args.live) {
        (None, true, None) => Ok(hopper_fake::FakeDebugger::shared()),
        (Some(t), false, None) => hopper_cli::open_debugger(t),
        (None, false, Some(program)) => hopper_cli::open_live(program),
        (None, false, None) => {
            eprintln!("hopper-gui: nothing to open\n{USAGE}");
            return ExitCode::from(2);
        }
        _ => {
            eprintln!("hopper-gui: give one of --fake, a trace or --live\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let dbg: Arc<dyn Debugger> = match opened {
        Ok(d) => d,
        Err(e) => {
            eprintln!("hopper-gui: {e}");
            return ExitCode::from(1);
        }
    };
    let result = match &args.shot {
        Some(out) => hopper_gui::run::shot(dbg, args.size.unwrap_or((1440.0, 900.0)), &args.keys, args.select.as_deref(), out),
        None => hopper_gui::run::run(dbg),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hopper-gui: {e}");
            ExitCode::from(1)
        }
    }
}
