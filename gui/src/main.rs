//! `hopper-gui --fake` opens the viewer over the scripted worked example; `hopper-gui <trace>`
//! will open an rr trace once `hopper_cli::open_debugger` exists.
//! `--shot <out.png> [--size <w>x<h>] [--keys F10,Alt+Left]` renders the window headlessly.

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use hopper_core::api::Debugger;

const USAGE: &str = "usage: hopper-gui (--fake | <trace-dir>) [--shot <out.png> [--size <w>x<h>] [--keys <chord>,...]]";

#[derive(Debug, Default)]
struct Args {
    fake: bool,
    trace: Option<PathBuf>,
    shot: Option<PathBuf>,
    size: Option<(f32, f32)>,
    keys: Vec<String>,
}

fn parse(args: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = args.iter();
    while let Some(x) = it.next() {
        match x.as_str() {
            "--fake" => a.fake = true,
            "--shot" => a.shot = Some(it.next().ok_or("--shot needs a path")?.into()),
            "--size" => {
                let v = it.next().ok_or("--size needs <w>x<h>")?;
                let (w, h) = v.split_once('x').ok_or("--size needs <w>x<h>")?;
                a.size = Some((w.parse().map_err(|_| "bad --size")?, h.parse().map_err(|_| "bad --size")?));
            }
            "--keys" => a.keys = it.next().ok_or("--keys needs a list")?.split(',').map(str::to_owned).collect(),
            s if s.starts_with('-') => return Err(format!("unknown flag {s}")),
            s => a.trace = Some(s.into()),
        }
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
    let dbg: Arc<dyn Debugger> = match (&args.trace, args.fake) {
        (None, true) => hopper_fake::FakeDebugger::shared(),
        (Some(t), false) => {
            eprintln!(
                "hopper-gui: cannot open {}: opening rr traces is not wired into the GUI yet (it will go through hopper_cli::open_debugger). Use --fake for the scripted worked example.",
                t.display()
            );
            return ExitCode::from(1);
        }
        (Some(_), true) => {
            eprintln!("hopper-gui: give --fake or a trace, not both\n{USAGE}");
            return ExitCode::from(2);
        }
        (None, false) => {
            eprintln!("hopper-gui: nothing to open\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let result = match &args.shot {
        Some(out) => hopper_gui::run::shot(dbg, args.size.unwrap_or((1440.0, 900.0)), &args.keys, out),
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
