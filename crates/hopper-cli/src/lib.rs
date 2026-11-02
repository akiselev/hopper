//! The `hopper` command.
//!
//! Every subcommand opens its debugger through [`open`]: `--fake` gives the scripted
//! [`FakeDebugger`], `--live -- <prog> [args]` runs a program under gdbserver ([`open_live`],
//! forward only), and a trace directory goes through [`open_debugger`], the one place that opens
//! an rr recording. The command logic lives in functions that take a reader and a writer
//! ([`show`], [`repl`], [`info`]), so tests drive them against the fake or a live process.

use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use hopper_core::api::{
    BreakLocation, CallFilter, CallId, Debugger, OriginHop, OwnershipState, PageReq, StepKind,
    Stop, StopCause, Until, ValueView,
};
use hopper_core::target::Direction;
use hopper_core::wire::{parse_break_location, parse_source_loc, parse_u64_str};
use hopper_core::{FrameId, HopperError, Moment, MomentRange, PositionUrl, Result};
use hopper_dap::LaunchTarget;
use hopper_engine::{Session, SessionOptions};
use hopper_fake::FakeDebugger;
use hopper_rr::{RecordOptions, Rr};

/// Open an rr recording. Every subcommand that takes a trace comes through here.
pub fn open_debugger(path: &Path) -> Result<Arc<dyn Debugger>> {
    if !path.join("version").is_file() {
        return Err(HopperError::NotFound(format!(
            "{} is not an rr trace directory (it has no `version` file)",
            path.display()
        )));
    }
    Ok(Arc::new(Session::open_rr(path, SessionOptions::default())?))
}

/// Run `program` under gdbserver: a forward-only live session.
pub fn open_live(program: &[OsString]) -> Result<Arc<dyn Debugger>> {
    let (prog, args) = program
        .split_first()
        .ok_or_else(|| HopperError::Invalid("give the program to run after --".into()))?;
    Ok(Arc::new(Session::live(
        Path::new(prog),
        args,
        SessionOptions::default(),
    )?))
}

/// The fake when `fake` is set, otherwise the recording at `trace`.
pub fn open(trace: Option<&Path>, fake: bool) -> Result<Arc<dyn Debugger>> {
    match (fake, trace) {
        (true, _) => Ok(Arc::new(FakeDebugger::new())),
        (false, Some(path)) => open_debugger(path),
        (false, None) => Err(HopperError::Invalid(
            "give a trace directory, --live -- <program>, or --fake".into(),
        )),
    }
}

// ---------------------------------------------------------------- command line

#[derive(Debug, Parser)]
#[command(
    name = "hopper",
    version,
    about = "A time-travel debugger for Rust, over rr recordings."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Which recording to open.
#[derive(Debug, Args)]
pub struct Source {
    /// The rr trace directory.
    #[arg(required_unless_present_any = ["fake", "live"])]
    pub trace: Option<PathBuf>,
    /// Use the scripted fake recording of fixtures/worked-example instead of a trace.
    #[arg(long, conflicts_with = "live")]
    pub fake: bool,
    /// Run the program given after `--` under gdbserver instead (forward only).
    #[arg(long, requires = "program")]
    pub live: bool,
    /// With --live: the program and its arguments.
    #[arg(last = true)]
    pub program: Vec<OsString>,
}

impl Source {
    fn open(&self) -> Result<Arc<dyn Debugger>> {
        if self.live {
            return open_live(&self.program);
        }
        open(self.trace.as_deref(), self.fake)
    }
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Record a program with rr: `hopper record -- <prog> [args]`, or build a cargo project with
    /// ownership sidecars first: `hopper record --build <dir> [--bin <name>] -- [args]`.
    Record {
        /// Do not record; report the fake recording instead.
        #[arg(long)]
        fake: bool,
        /// Build this cargo project with `cargo hopper-build` and record its executable.
        #[arg(long, value_name = "DIR")]
        build: Option<PathBuf>,
        /// With --build: which executable to record, when the project builds several.
        #[arg(long, requires = "build")]
        bin: Option<String>,
        /// The trace directory to create (rr picks one when absent).
        #[arg(long, short)]
        output: Option<PathBuf>,
        /// The program and its arguments (with --build: only the arguments).
        #[arg(last = true, required_unless_present_any = ["fake", "build"])]
        program: Vec<OsString>,
    },
    /// Print the source line, the stack and every local at one position.
    Show {
        #[command(flatten)]
        source: Source,
        /// A position URL (hopper://...), an rr event number, `start`, `end`, or a breakpoint
        /// location (`file:line` or a function) to run to.
        #[arg(long)]
        at: String,
    },
    /// Debug a program live under gdbserver, forward only: `hopper live -- <prog> [args]`.
    Live {
        /// The program and its arguments.
        #[arg(last = true, required = true)]
        program: Vec<OsString>,
    },
    /// An interactive line-based debugger (type `help`).
    Repl {
        #[command(flatten)]
        source: Source,
    },
    /// Serve MCP (for agents) on stdin/stdout.
    Mcp {
        #[command(flatten)]
        source: Source,
    },
    /// Serve the Debug Adapter Protocol on stdin/stdout; launch arguments name the recording.
    Dap {
        /// Open the fake recording whatever the launch arguments say.
        #[arg(long)]
        fake: bool,
    },
    /// Describe a recording.
    Info {
        #[command(flatten)]
        source: Source,
    },
}

/// Run a parsed command line with the given standard input and output.
pub fn run(cli: Cli, input: impl BufRead, mut out: impl Write) -> Result<()> {
    match cli.command {
        Command::Record { fake: true, .. } => {
            let d = open(None, true)?;
            writeln!(out, "fake: no recording made; the fake recording is:")?;
            info(d.as_ref(), &mut out)
        }
        Command::Record {
            fake: false,
            build,
            bin,
            output,
            program,
        } => {
            let what = match build {
                Some(dir) => RecordWhat::CargoProject {
                    dir,
                    bin,
                    args: program,
                },
                None => RecordWhat::Program(program),
            };
            let trace = record(&what, output.as_deref(), &mut out)?;
            writeln!(out, "{}", trace.display())?;
            Ok(())
        }
        Command::Live { program } => repl(open_live(&program)?.as_ref(), input, &mut out),
        Command::Show { source, at } => show(source.open()?.as_ref(), &at, &mut out),
        Command::Repl { source } => repl(source.open()?.as_ref(), input, &mut out),
        Command::Info { source } => info(source.open()?.as_ref(), &mut out),
        Command::Mcp { source } => Ok(hopper_mcp::serve(source.open()?, input, out)?),
        Command::Dap { fake } => {
            let factory = move |target: &LaunchTarget| match (fake, target) {
                (true, _) | (false, LaunchTarget::Fake) => open(None, true),
                (false, LaunchTarget::Recording(path)) => open_debugger(path),
                (false, LaunchTarget::Live { program, args }) => {
                    let mut argv = vec![program.clone().into_os_string()];
                    argv.extend(args.iter().cloned());
                    open_live(&argv)
                }
            };
            Ok(hopper_dap::serve(factory, input, out)?)
        }
    }
}

/// What `hopper record` records.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RecordWhat {
    /// A program and its arguments.
    Program(Vec<OsString>),
    /// A cargo project, built with `cargo hopper-build` first (so the recording has ownership
    /// sidecars), then its executable with these arguments.
    CargoProject {
        dir: PathBuf,
        bin: Option<String>,
        args: Vec<OsString>,
    },
}

/// `hopper record`: returns the trace directory.
pub fn record(what: &RecordWhat, output: Option<&Path>, out: &mut impl Write) -> Result<PathBuf> {
    let (program, args) = match what {
        RecordWhat::Program(argv) => {
            let (p, a) = argv
                .split_first()
                .ok_or_else(|| HopperError::Invalid("give the program to record".into()))?;
            (PathBuf::from(p), a.to_vec())
        }
        RecordWhat::CargoProject { dir, bin, args } => {
            let exe = hopper_build(dir, bin.as_deref())?;
            writeln!(out, "built {} (with its ownership sidecar)", exe.display())?;
            (exe, args.clone())
        }
    };
    let rr = Rr::detect()?;
    let options = RecordOptions {
        output_dir: output.map(Path::to_path_buf),
        ..RecordOptions::default()
    };
    let rec = rr.record(&program, &args, &options)?;
    io::stderr().write_all(&rec.stderr).ok();
    Ok(rec.trace_dir)
}

/// Build the cargo project in `dir` with `cargo hopper-build` (the tool at `$HOPPER_BUILD`, else
/// `cargo hopper-build` from `PATH`) and return the executable it built.
pub fn hopper_build(dir: &Path, bin: Option<&str>) -> Result<PathBuf> {
    let manifest = dir.join("Cargo.toml");
    if !manifest.is_file() {
        return Err(HopperError::NotFound(format!(
            "{} is not a cargo project",
            dir.display()
        )));
    }
    let mut cmd = match std::env::var_os("HOPPER_BUILD") {
        Some(tool) => std::process::Command::new(tool),
        None => std::process::Command::new("cargo"),
    };
    cmd.arg("hopper-build").arg("--manifest-path").arg(&manifest);
    if let Some(b) = bin {
        cmd.arg("--bin").arg(b);
    }
    let output = cmd.output()?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(HopperError::Invalid(format!(
            "cargo hopper-build failed:\n{stderr}"
        )));
    }
    let built = built_executables(&stderr);
    match (built.as_slice(), bin) {
        ([one], _) => Ok(one.clone()),
        (many, Some(b)) => many
            .iter()
            .find(|p| p.file_name().is_some_and(|n| n == b))
            .cloned()
            .ok_or_else(|| HopperError::NotFound(format!("executable {b} in the build"))),
        ([], None) => Err(HopperError::NotFound(format!(
            "cargo hopper-build built no executable with a sidecar:\n{stderr}"
        ))),
        (_, None) => Err(HopperError::Invalid(
            "the project builds several executables; choose one with --bin".into(),
        )),
    }
}

/// The executables in `cargo hopper-build`'s report (`hopper-build: <bin> -> <bin>.hsc`).
pub fn built_executables(report: &str) -> Vec<PathBuf> {
    report
        .lines()
        .filter_map(|l| l.strip_prefix("hopper-build: "))
        .filter_map(|l| l.split_once(" -> ").map(|(bin, _)| PathBuf::from(bin)))
        .collect()
}


// ---------------------------------------------------------------- shared printing

fn io_err(e: io::Error) -> HopperError {
    HopperError::Io(e.to_string())
}

fn ownership_note(o: &OwnershipState) -> String {
    match o {
        OwnershipState::MovedOut {
            to: Some(to), at, ..
        } => match at {
            Some(at) => format!("  [moved out to {to} at {}]", short_loc(&at.to_string())),
            None => format!("  [moved out to {to}]"),
        },
        OwnershipState::MovedOut { to: None, .. } => "  [moved out]".into(),
        OwnershipState::Dropped { at: Some(at), .. } => {
            format!("  [dropped at {}]", short_loc(&at.to_string()))
        }
        OwnershipState::Dropped { at: None, .. } => "  [dropped]".into(),
        OwnershipState::Live | OwnershipState::Unknown => String::new(),
    }
}

/// `main.rs:12` rather than the full path.
fn short_loc(loc: &str) -> String {
    loc.rsplit('/').next().unwrap_or(loc).to_string()
}

fn cause_note(c: &StopCause) -> String {
    match c {
        StopCause::Breakpoint { id } => format!("  [breakpoint {}]", id.0),
        StopCause::Watchpoint { .. } => "  [watchpoint]".into(),
        StopCause::Signal { signo } => format!("  [signal {signo}]"),
        StopCause::Exited { code } => format!("  [exited {code}]"),
        StopCause::HistoryStart => "  [start of recording]".into(),
        StopCause::HistoryEnd => "  [end of recording]".into(),
        StopCause::Step | StopCause::Jump => String::new(),
    }
}

/// The source text of a line, if the source is available.
fn source_line(d: &dyn Debugger, file: &str, line: u32) -> Option<String> {
    let text = d.source_text(file).ok()?;
    text.lines()
        .nth(line.checked_sub(1)? as usize)
        .map(str::to_string)
}

fn print_position(
    d: &dyn Debugger,
    at: &Moment,
    function: Option<&str>,
    loc: Option<&hopper_core::SourceLoc>,
    note: &str,
    out: &mut impl Write,
) -> io::Result<()> {
    writeln!(out, "{}", at.url())?;
    let where_ = loc.map(|l| l.to_string()).unwrap_or_else(|| "??".into());
    writeln!(out, "  {} at {where_}{note}", function.unwrap_or("??"))?;
    if let Some(l) = loc
        && let Some(text) = source_line(d, &l.file, l.line)
    {
        writeln!(out, "  {:>4} | {}", l.line, text)?;
    }
    Ok(())
}

fn print_stop(d: &dyn Debugger, s: &Stop, out: &mut impl Write) -> io::Result<()> {
    print_position(
        d,
        &s.at,
        s.function.as_deref(),
        s.location.as_ref(),
        &cause_note(&s.cause),
        out,
    )
}

fn print_value(v: &ValueView, indent: usize, out: &mut impl Write) -> io::Result<()> {
    writeln!(
        out,
        "{:indent$}{}: {} = {}{}",
        "",
        v.name,
        v.type_name,
        v.summary,
        ownership_note(&v.ownership)
    )
}

fn print_frames(d: &dyn Debugger, at: Moment, out: &mut impl Write) -> Result<()> {
    for f in d.frames(at, at.tid)? {
        let loc = f
            .location
            .map(|l| l.to_string())
            .unwrap_or_else(|| "??".into());
        writeln!(
            out,
            "  #{} {} at {loc} (pc {:#x})",
            f.id.depth,
            f.function.as_deref().unwrap_or("??"),
            f.pc
        )?;
    }
    Ok(())
}

fn hop_line(i: usize, h: &OriginHop) -> String {
    let mut s = format!("  #{} {}", i + 1, h.at.url());
    match &h.ownership {
        Some(o) => {
            let _ = write!(
                s,
                "\n     {:?} {} -> {}",
                o.kind,
                o.src.as_deref().unwrap_or("?"),
                o.dst.as_deref().unwrap_or("?")
            );
            if let Some(span) = &o.span {
                let _ = write!(s, " at {}", short_loc(&span.to_string()));
            }
            let _ = write!(s, " [{:?}]", o.confidence);
        }
        None => s.push_str("\n     (no ownership event)"),
    }
    let _ = write!(
        s,
        "\n     machine: {:?} in {} at {} (pc {:#x})",
        h.machine,
        h.function.as_deref().unwrap_or("??"),
        h.location
            .as_ref()
            .map(|l| short_loc(&l.to_string()))
            .unwrap_or_else(|| "??".into()),
        h.pc
    );
    if let Some(a) = h.allocation {
        let _ = write!(s, ", heap allocation #{}", a.0);
    }
    s
}

/// A position given on the command line: a position URL, an rr event number, `start` or `end`.
/// An event number moves the cursor to the first moment of that event.
pub fn resolve_position(d: &dyn Debugger, text: &str) -> Result<Moment> {
    let t = text.trim();
    let info = d.recording()?;
    let start = info
        .start
        .ok_or_else(|| HopperError::NotFound("recording start".into()))?;
    match t {
        "start" => return Ok(start),
        "end" => {
            return info
                .end
                .ok_or_else(|| HopperError::NotFound("recording end".into()));
        }
        _ => {}
    }
    if t.starts_with("hopper://") {
        return Ok(t.parse::<PositionUrl>()?.0);
    }
    let Ok(event) = t.parse::<u64>() else {
        return run_to_location(d, t);
    };
    if event <= start.frame_time {
        return Ok(start);
    }
    let probe = Moment {
        frame_time: event,
        ticks: 0,
        sub: 0,
        ..start
    };
    let stop = d.seek(probe)?;
    if stop.at.frame_time < event {
        let next = d.step(StepKind::Instruction, Direction::Forward)?;
        if next.at.frame_time >= event {
            return Ok(next.at);
        }
        return Err(HopperError::NotFound(format!(
            "event {event} is past the end of the recording"
        )));
    }
    Ok(stop.at)
}

/// The first moment, from the start, at which a breakpoint location is hit.
fn run_to_location(d: &dyn Debugger, text: &str) -> Result<Moment> {
    let loc = parse_break_location(text).map_err(|_| {
        HopperError::Invalid(format!(
            "expected a position url, an event number, start, end or a breakpoint location: \
             {text:?}"
        ))
    })?;
    let bp = d.add_breakpoint(loc)?;
    let ran = (|| {
        let start = d.recording()?.start;
        if let Some(start) = start
            && d.cursor()?.at != start
        {
            d.seek(start)?;
        }
        d.run(Direction::Forward, Until::Breakpoints)
    })();
    let removed = d.remove_breakpoint(bp.id);
    let stop = ran?;
    removed?;
    match stop.cause {
        StopCause::Breakpoint { id } if id == bp.id => Ok(stop.at),
        other => Err(HopperError::NotFound(format!(
            "{text} was never reached ({other:?})"
        ))),
    }
}

// ---------------------------------------------------------------- info and show

/// `hopper info`.
pub fn info(d: &dyn Debugger, out: &mut impl Write) -> Result<()> {
    let info = d.recording()?;
    writeln!(out, "recording {}", info.id)?;
    writeln!(out, "  path: {}", info.path)?;
    if let Some(v) = &info.rr_version {
        writeln!(out, "  rr: {v}")?;
    }
    if let Some(s) = &info.start {
        writeln!(out, "  start: {}", s.url())?;
    }
    if let Some(e) = &info.end {
        writeln!(out, "  end:   {}", e.url())?;
    }
    for p in &info.processes {
        let threads: Vec<String> = p.threads.iter().map(|t| t.to_string()).collect();
        writeln!(
            out,
            "  process {} (exec {}): {}  threads {}",
            p.id.pid,
            p.id.exec_generation,
            p.executable,
            threads.join(", ")
        )?;
    }
    let status = d.index_status();
    let phases: Vec<String> = status
        .phases
        .iter()
        .map(|(n, p)| format!("{n} {:.0}%", p * 100.0))
        .collect();
    writeln!(
        out,
        "  index: {}{}",
        if status.done { "done" } else { "building" },
        if phases.is_empty() {
            String::new()
        } else {
            format!(" ({})", phases.join(", "))
        }
    )?;
    if let Some(e) = status.error {
        writeln!(out, "  index error: {e}")?;
    }
    Ok(())
}

/// `hopper show --at <position>`: the source line, the stack and every local of every frame.
pub fn show(d: &dyn Debugger, at: &str, out: &mut impl Write) -> Result<()> {
    let at = resolve_position(d, at)?;
    let frames = d.frames(at, at.tid)?;
    let top = frames.first();
    print_position(
        d,
        &at,
        top.and_then(|f| f.function.as_deref()),
        top.and_then(|f| f.location.as_ref()),
        "",
        out,
    )?;
    writeln!(out, "stack:")?;
    print_frames(d, at, out)?;
    for f in &frames {
        writeln!(
            out,
            "locals of #{} {}:",
            f.id.depth,
            f.function.as_deref().unwrap_or("??")
        )?;
        let locals = match d.locals(at, f.id) {
            Ok(l) => l,
            // Frames of code built without debug information (libc, the dynamic linker).
            Err(HopperError::NotFound(_)) => {
                writeln!(out, "  (no debug information)")?;
                continue;
            }
            Err(e) => return Err(e),
        };
        if locals.is_empty() {
            writeln!(out, "  (none)")?;
        }
        for v in locals {
            print_value(&v, 2, out)?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- the REPL

const HELP: &str = "\
navigation (forward / reverse):
  s / rs              step into / back into (one source line)
  n / rn              step over / back over
  finish / rfinish    run until the function returns / was called
  si / rsi            one instruction forward / back
  c / rc              continue / reverse-continue to a breakpoint
  seek <url>          jump to a position URL
  back / fwd          undo / redo the last cursor move
  where               print the position URL of the cursor
state at the cursor:
  bt                  the stack
  locals              the innermost frame's locals
  p <expr>            evaluate (a, v[0].name, *n, &b)
  b <file:line|fn>    add a breakpoint
queries (do not move the cursor):
  origin <expr>       where the value came from, with ownership labels
  history <expr>      every change of the value
  calls [fn]          the call tree, or every call of fn
  hits <file:line>    every execution of a line
  q                   quit";

/// The interactive debugger: reads commands from `input` until `q` or end of input.
pub fn repl(d: &dyn Debugger, input: impl BufRead, out: &mut impl Write) -> Result<()> {
    let start = d.cursor()?;
    print_stop(d, &start, out)?;
    let mut lines = input.lines();
    loop {
        write!(out, "(hopper) ")?;
        out.flush()?;
        let Some(line) = lines.next() else {
            writeln!(out)?;
            return Ok(());
        };
        let line = line.map_err(io_err)?;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (cmd, arg) = match line.split_once(char::is_whitespace) {
            Some((c, a)) => (c, a.trim()),
            None => (line, ""),
        };
        if matches!(cmd, "q" | "quit" | "exit") {
            return Ok(());
        }
        if let Err(e) = repl_command(d, cmd, arg, out) {
            writeln!(out, "error: {e}")?;
        }
    }
}

fn need<'a>(arg: &'a str, what: &str) -> Result<&'a str> {
    if arg.is_empty() {
        Err(HopperError::Invalid(format!("this command needs {what}")))
    } else {
        Ok(arg)
    }
}

fn repl_command(d: &dyn Debugger, cmd: &str, arg: &str, out: &mut impl Write) -> Result<()> {
    use Direction::{Forward, Reverse};
    let step = |kind, dir| d.step(kind, dir);
    let stop = match cmd {
        "s" | "step" => Some(step(StepKind::Into, Forward)?),
        "n" | "next" => Some(step(StepKind::Over, Forward)?),
        "finish" => Some(step(StepKind::Out, Forward)?),
        "si" | "stepi" => Some(step(StepKind::Instruction, Forward)?),
        "rs" => Some(step(StepKind::Into, Reverse)?),
        "rn" => Some(step(StepKind::Over, Reverse)?),
        "rfinish" => Some(step(StepKind::Out, Reverse)?),
        "rsi" => Some(step(StepKind::Instruction, Reverse)?),
        "c" | "continue" => Some(d.run(Forward, Until::Breakpoints)?),
        "rc" => Some(d.run(Reverse, Until::Breakpoints)?),
        "seek" => {
            let url: PositionUrl = need(arg, "a position url")?.parse()?;
            Some(d.seek(url.0)?)
        }
        "back" => match d.back()? {
            Some(s) => Some(s),
            None => {
                writeln!(out, "no earlier position")?;
                None
            }
        },
        "fwd" | "forward" => match d.forward()? {
            Some(s) => Some(s),
            None => {
                writeln!(out, "no later position")?;
                None
            }
        },
        _ => None,
    };
    if let Some(s) = stop {
        print_stop(d, &s, out)?;
        return Ok(());
    }
    let cursor = || d.cursor();
    let top = |c: &Stop| FrameId {
        tid: c.tid,
        depth: 0,
    };
    match cmd {
        "s" | "step" | "n" | "next" | "finish" | "si" | "stepi" | "rs" | "rn" | "rfinish"
        | "rsi" | "c" | "continue" | "rc" | "seek" | "back" | "fwd" | "forward" => {}
        "where" => writeln!(out, "{}", cursor()?.at.url())?,
        "bt" | "backtrace" => print_frames(d, cursor()?.at, out)?,
        "locals" => {
            let c = cursor()?;
            let locals = d.locals(c.at, top(&c))?;
            if locals.is_empty() {
                writeln!(out, "(no locals)")?;
            }
            for v in locals {
                print_value(&v, 0, out)?;
            }
        }
        "p" | "print" => {
            let c = cursor()?;
            let v = d.eval(c.at, top(&c), need(arg, "an expression")?)?;
            print_value(&v, 0, out)?;
            if v.child_count.unwrap_or(0) > 0 {
                for k in d.children(&v.handle, PageReq::first(50))? {
                    print_value(&k, 2, out)?;
                }
            }
        }
        "b" | "break" => {
            let loc: BreakLocation = parse_break_location(need(arg, "a file:line or function")?)?;
            let bp = d.add_breakpoint(loc)?;
            let addrs: Vec<String> = bp.addresses.iter().map(|a| format!("{a:#x}")).collect();
            writeln!(
                out,
                "breakpoint {} at {arg} ({})",
                bp.id.0,
                addrs.join(", ")
            )?;
        }
        "origin" => {
            let c = cursor()?;
            let v = d.eval(c.at, top(&c), need(arg, "an expression")?)?;
            let chain = d.origin(&v.handle, Default::default())?;
            writeln!(
                out,
                "origin of {} = {} at {}",
                v.name,
                v.summary,
                c.at.url()
            )?;
            for (i, h) in chain.hops.iter().enumerate() {
                writeln!(out, "{}", hop_line(i, h))?;
            }
            writeln!(out, "  end: {:?}", chain.end)?;
        }
        "history" => {
            let c = cursor()?;
            let v = d.eval(c.at, top(&c), need(arg, "an expression")?)?;
            let page = d.value_history(&v.handle, MomentRange::all(), PageReq::first(1000))?;
            for (m, view) in &page.items {
                writeln!(
                    out,
                    "  {}  {}{}",
                    m.url(),
                    view.summary,
                    ownership_note(&view.ownership)
                )?;
            }
            partial_note(&page.completeness, out)?;
        }
        "calls" => {
            if arg.is_empty() {
                print_call_tree(d, None, out)?;
            } else {
                let filter = CallFilter {
                    function: Some(arg.to_string()),
                    ..Default::default()
                };
                let page = d.calls(filter, PageReq::first(1000))?;
                if page.items.is_empty() {
                    writeln!(out, "no calls of {arg}")?;
                }
                for c in &page.items {
                    writeln!(out, "  {} #{}  {}", c.function, c.id.0, call_span(c))?;
                }
                partial_note(&page.completeness, out)?;
            }
        }
        "hits" => {
            let loc = parse_source_loc(need(arg, "a file:line")?)?;
            let page = d.line_hits(loc, MomentRange::all(), PageReq::first(1000))?;
            if page.items.is_empty() {
                writeln!(out, "{arg} never ran")?;
            }
            for m in &page.items {
                writeln!(out, "  {}", m.url())?;
            }
            partial_note(&page.completeness, out)?;
        }
        "x" => {
            // x <addr> [len]: memory at the cursor (an extra beyond the listed commands).
            let mut parts = need(arg, "an address")?.split_whitespace();
            let addr = parse_u64_str(parts.next().unwrap_or_default())?;
            let len = parts.next().map(parse_u64_str).transpose()?.unwrap_or(16);
            let c = cursor()?;
            let p = d.recording()?.processes.first().map(|p| p.id);
            let p = p.ok_or_else(|| HopperError::NotFound("a process".into()))?;
            let bytes = d.memory(c.at, p, addr, len)?;
            writeln!(out, "  {addr:#x}: {}", hopper_core::wire::hex(&bytes))?;
        }
        "help" | "h" | "?" => writeln!(out, "{HELP}")?,
        other => writeln!(out, "unknown command `{other}`; type `help`")?,
    }
    Ok(())
}

fn partial_note(c: &hopper_core::api::Completeness, out: &mut impl Write) -> io::Result<()> {
    if let hopper_core::api::Completeness::Partial { .. } = c {
        writeln!(
            out,
            "  (the index is still building; results may be missing)"
        )?;
    }
    Ok(())
}

fn call_span(c: &hopper_core::api::CallNode) -> String {
    match &c.exit {
        Some(e) => format!("{} .. {}", c.entry.url(), e.url()),
        None => format!("{} .. (never returned)", c.entry.url()),
    }
}

fn print_call_tree(d: &dyn Debugger, parent: Option<CallId>, out: &mut impl Write) -> Result<()> {
    let filter = CallFilter {
        parent,
        ..Default::default()
    };
    let page = d.calls(filter, PageReq::first(1000))?;
    for c in &page.items {
        writeln!(
            out,
            "{:indent$}{} #{}  {}",
            "",
            c.function,
            c.id.0,
            call_span(c),
            indent = 2 + 2 * c.depth as usize
        )?;
        if c.child_count > 0 {
            print_call_tree(d, Some(c.id), out)?;
        }
    }
    if parent.is_none() {
        partial_note(&page.completeness, out)?;
    }
    Ok(())
}
