//! Test support (feature `test-fixtures`): build the fixture programs, make core files with gdb,
//! and ask gdb (with Rust's pretty printers) what it prints for an expression, so tests can
//! compare hopper's answers with gdb's on the same core.
//!
//! Every function returns `None` (and prints why on stderr) when a tool is missing, so tests can
//! skip with a clear message instead of failing.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Mutex, OnceLock};

/// The repository root.
pub fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repository root")
}

/// Whether gdb can be run.
pub fn gdb_available() -> bool {
    static AVAILABLE: OnceLock<bool> = OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        Command::new("gdb").arg("--version").output().is_ok_and(|o| o.status.success())
    })
}

/// Skip message helper: prints and returns `None`.
fn skip<T>(why: &str) -> Option<T> {
    eprintln!("skipping: {why}");
    None
}

/// The fixture binary `name` (`values`, `worked-example`, `threads`), building the fixtures with
/// `scripts/build-fixtures.sh` the first time one is missing.
pub fn fixture_binary(name: &str) -> Option<PathBuf> {
    static BUILD: Mutex<Option<bool>> = Mutex::new(None);
    let root = repo_root();
    let path = root.join("target/fixtures").join(name).join("debug").join(name);
    let mut built = BUILD.lock().unwrap_or_else(|p| p.into_inner());
    if built.is_none() {
        let status = Command::new("bash").arg(root.join("scripts/build-fixtures.sh")).status();
        *built = Some(status.is_ok_and(|s| s.success()));
    }
    if path.exists() {
        Some(path)
    } else {
        skip(&format!("fixture {name} could not be built ({})", path.display()))
    }
}

fn cores_dir() -> PathBuf {
    let d = repo_root().join("target/test-cores");
    std::fs::create_dir_all(&d).expect("create target/test-cores");
    d
}

/// A core file of fixture `name` stopped at the gdb breakpoint `location` (`values::stop_here`,
/// `main.rs:14`). Made once per binary build and cached under `target/test-cores/`.
pub fn core_at(name: &str, location: &str) -> Option<(PathBuf, PathBuf)> {
    static LOCK: Mutex<()> = Mutex::new(());
    if !gdb_available() {
        return skip("gdb is not installed");
    }
    let binary = fixture_binary(name)?;
    let _guard = LOCK.lock().unwrap_or_else(|p| p.into_inner());
    let tag: String =
        location.chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    let core = cores_dir().join(format!("{name}-{tag}.core"));
    let fresh = match (std::fs::metadata(&core), std::fs::metadata(&binary)) {
        (Ok(c), Ok(b)) => c.modified().ok() >= b.modified().ok(),
        _ => false,
    };
    if !fresh {
        let tmp = core.with_extension("tmp");
        let out = Command::new("gdb")
            .args(["-batch", "-nx", "-iex", "set debuginfod enabled off"])
            .args(["-ex", "set pagination off"])
            .args(["-ex", &format!("break {location}")])
            .args(["-ex", "run"])
            .args(["-ex", &format!("gcore {}", tmp.display())])
            .args(["-ex", "kill"])
            .arg(&binary)
            .output()
            .ok()?;
        if !tmp.exists() {
            return skip(&format!(
                "gdb did not write a core at {location}: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        std::fs::rename(&tmp, &core).ok()?;
    }
    Some((binary, core))
}

/// The directory with Rust's gdb pretty printers (`<sysroot>/lib/rustlib/etc`), which the
/// binaries' `.debug_gdb_scripts` section names.
fn rust_gdb_scripts() -> Option<PathBuf> {
    static DIR: OnceLock<Option<PathBuf>> = OnceLock::new();
    DIR.get_or_init(|| {
        let out = Command::new("rustc").args(["--print", "sysroot"]).output().ok()?;
        let root = String::from_utf8(out.stdout).ok()?;
        let dir = Path::new(root.trim()).join("lib/rustlib/etc");
        dir.join("gdb_lookup.py").exists().then_some(dir)
    })
    .clone()
}

/// gdb arguments that load Rust's pretty printers, as `rust-gdb` does.
fn pretty_printer_args(cmd: &mut Command) {
    cmd.args(["-iex", "set auto-load safe-path /"]);
    if let Some(dir) = rust_gdb_scripts() {
        let d = dir.display();
        cmd.args(["-iex", &format!("directory {d}")]);
        cmd.args(["-iex", &format!("python import sys; sys.path.insert(0, '{d}')")]);
    }
}

/// How gdb should print values.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum GdbPrinting {
    /// With the Rust pretty printers the binary's `.debug_gdb_scripts` names (rust-gdb's view).
    PrettyPrinters,
    /// gdb's own Rust printing of the raw structures.
    Raw,
}

/// Run gdb on `binary` and `core`, select frame `frame`, and print each expression. Each result is
/// gdb's text after `$N = `, or `None` when gdb reported an error for it.
pub fn gdb_print(
    binary: &Path,
    core: &Path,
    frame: u32,
    exprs: &[&str],
    printing: GdbPrinting,
) -> Vec<Option<String>> {
    let mut cmd = Command::new("gdb");
    cmd.args(["-batch", "-nx", "-iex", "set debuginfod enabled off"]);
    match printing {
        GdbPrinting::PrettyPrinters => pretty_printer_args(&mut cmd),
        GdbPrinting::Raw => {
            cmd.args(["-iex", "set auto-load python-scripts off"]);
        }
    }
    cmd.args(["-ex", "set pagination off", "-ex", "set print elements 1000", "-ex", "set width 0"]);
    cmd.args(["-ex", &format!("frame {frame}")]);
    for (i, e) in exprs.iter().enumerate() {
        cmd.args(["-ex", &format!("echo @@{i}@@\\n"), "-ex", &format!("print {e}")]);
    }
    cmd.args(["-ex", &format!("echo @@{}@@\\n", exprs.len())]);
    cmd.arg(binary).arg(core);
    let Ok(out) = cmd.output() else { return vec![None; exprs.len()] };
    let text = String::from_utf8_lossy(&out.stdout);
    (0..exprs.len())
        .map(|i| {
            let start = text.find(&format!("@@{i}@@\n"))? + format!("@@{i}@@\n").len();
            let end = text[start..].find(&format!("@@{}@@\n", i + 1))? + start;
            let body = text[start..end].trim();
            let (_, value) = body.split_once(" = ")?;
            Some(value.to_string())
        })
        .collect()
}

/// gdb's raw output for `commands` on `binary` (and `core`, when given).
pub fn gdb_output(binary: &Path, core: Option<&Path>, commands: &[&str]) -> String {
    let mut cmd = Command::new("gdb");
    cmd.args(["-batch", "-nx", "-iex", "set debuginfod enabled off"]);
    pretty_printer_args(&mut cmd);
    cmd.args(["-ex", "set pagination off", "-ex", "set width 0"]);
    for c in commands {
        cmd.args(["-ex", c]);
    }
    cmd.arg(binary);
    if let Some(c) = core {
        cmd.arg(c);
    }
    cmd.output().map(|o| String::from_utf8_lossy(&o.stdout).into_owned()).unwrap_or_default()
}
