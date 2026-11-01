//! `cargo hopper-build [cargo build args]`.
//!
//! Runs `cargo build` on the pinned nightly with `hopper-build-driver` as the workspace rustc
//! wrapper (so every workspace crate, and no dependency, is analysed), collects the executables
//! cargo reports, and links each one's sidecar from the partials the driver wrote.
//!
//! rustc_plugin's own `cli_main` runs `cargo check`, which produces no binaries, so this CLI
//! drives cargo itself and reuses only rustc_plugin's driver side and its argument passing.

use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use hopper_sidecar::link::PartialSet;

use crate::plugin::{DRIVER_NAME, DriverArgs};

/// What a build produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildOutcome {
    /// The executables cargo built, each with the sidecar written beside it.
    pub sidecars: Vec<(PathBuf, PathBuf)>,
    /// Executables no partial described (built outside hopper-build, or not workspace code).
    pub without_sidecar: Vec<PathBuf>,
}

const USAGE: &str = "\
cargo hopper-build [cargo build args]

Builds the workspace with the pinned nightly (the toolchain hopper-build was built with) and
writes <binary>.hsc, an ownership sidecar, beside every executable it builds. All arguments go
to `cargo build`, except --message-format, which hopper-build sets itself.";

pub fn main() -> ExitCode {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    // Invoked as `cargo hopper-build ...`, cargo passes the subcommand name first.
    if args.first().map(String::as_str) == Some("hopper-build") {
        args.remove(0);
    }
    if args.iter().any(|a| a == "-V" || a == "--version") {
        println!("{} (rustc_plugin channel {})", crate::tool_version(), rustc_plugin::CHANNEL);
        return ExitCode::SUCCESS;
    }
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("{USAGE}");
        return ExitCode::SUCCESS;
    }
    match run(&args) {
        Ok(outcome) => {
            for (bin, sc) in &outcome.sidecars {
                eprintln!("hopper-build: {} -> {}", bin.display(), sc.display());
            }
            for bin in &outcome.without_sidecar {
                eprintln!("hopper-build: no sidecar for {} (no partial produced it)", bin.display());
            }
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("hopper-build: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Build with sidecars. `cargo_args` are `cargo build`'s arguments.
pub fn run(cargo_args: &[String]) -> Result<BuildOutcome, String> {
    if cargo_args.iter().any(|a| a.starts_with("--message-format")) {
        return Err("--message-format is set by hopper-build".into());
    }
    let driver = std::env::current_exe()
        .map_err(|e| format!("cannot find own executable: {e}"))?
        .with_file_name(DRIVER_NAME);
    if !driver.exists() {
        return Err(format!("driver not found at {}", driver.display()));
    }
    let meta = Metadata::read(cargo_args)?;
    let target_dir = match flag_value(cargo_args, "--target-dir") {
        Some(dir) => std::path::absolute(dir).map_err(|e| e.to_string())?,
        None => meta.target_directory.clone(),
    };
    let partials_dir = target_dir.join("hopper").join("partials");
    refresh_if_driver_changed(&driver, &target_dir, &partials_dir, &meta, cargo_args)?;
    let driver_args = serde_json::to_string(&DriverArgs { partials_dir: partials_dir.clone() })
        .map_err(|e| e.to_string())?;

    let mut cmd = cargo();
    cmd.arg("build")
        .args(cargo_args)
        .arg("--message-format=json-render-diagnostics")
        .env("RUSTC_WORKSPACE_WRAPPER", &driver)
        .env("PLUGIN_ARGS", driver_args)
        // Every crate the wrapper sees is a workspace member; analyse them all, not only the
        // packages selected on the command line.
        .env("RUSTC_PLUGIN_ALL_TARGETS", "")
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().map_err(|e| format!("cannot run cargo: {e}"))?;
    let stdout = child.stdout.take().ok_or("cargo stdout not captured")?;
    let mut executables = Vec::new();
    for line in BufReader::new(stdout).lines() {
        let line = line.map_err(|e| format!("reading cargo output: {e}"))?;
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(msg) => {
                if msg["reason"] == "compiler-artifact"
                    && let Some(exe) = msg["executable"].as_str()
                {
                    executables.push(PathBuf::from(exe));
                }
            }
            // Not a cargo message: a build script or the program printed it.
            Err(_) => println!("{line}"),
        }
    }
    let status = child.wait().map_err(|e| format!("waiting for cargo: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed ({status})"));
    }

    let partials = PartialSet::load_dir(&partials_dir).map_err(|e| e.to_string())?;
    let mut outcome = BuildOutcome::default();
    let tool = crate::tool_version();
    for exe in executables {
        match partials.link_binary(&exe, &tool).map_err(|e| format!("{}: {e}", exe.display()))? {
            Some(sc) => outcome.sidecars.push((exe, sc)),
            None => outcome.without_sidecar.push(exe),
        }
    }
    Ok(outcome)
}

/// `cargo` on the pinned nightly: the driver links that nightly's rustc, so the standard library
/// and every crate must come from it too.
fn cargo() -> Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut cmd = Command::new(cargo);
    cmd.env("RUSTUP_TOOLCHAIN", rustc_plugin::CHANNEL);
    cmd
}

/// What `cargo metadata` says about the workspace.
struct Metadata {
    /// Honours `CARGO_TARGET_DIR` and cargo config (not `--target-dir`, which `run` applies).
    target_directory: PathBuf,
    /// Package id specs of the workspace members.
    members: Vec<String>,
}

impl Metadata {
    fn read(args: &[String]) -> Result<Metadata, String> {
        let mut cmd = cargo();
        cmd.args(["metadata", "--format-version", "1", "--no-deps"]);
        if let Some(m) = flag_value(args, "--manifest-path") {
            cmd.args(["--manifest-path", m]);
        }
        let out =
            cmd.stderr(Stdio::inherit()).output().map_err(|e| format!("cannot run cargo metadata: {e}"))?;
        if !out.status.success() {
            return Err(format!("cargo metadata failed ({})", out.status));
        }
        let v: serde_json::Value =
            serde_json::from_slice(&out.stdout).map_err(|e| format!("cargo metadata output: {e}"))?;
        let target_directory = v["target_directory"]
            .as_str()
            .map(PathBuf::from)
            .ok_or("cargo metadata gave no target_directory")?;
        let members = v["workspace_members"]
            .as_array()
            .map(|a| a.iter().filter_map(|m| m.as_str().map(str::to_owned)).collect())
            .unwrap_or_default();
        Ok(Metadata { target_directory, members })
    }
}

/// Cargo does not fingerprint the wrapper binary's contents, so after the driver changes it would
/// reuse crates the old driver analysed (or that a plain `cargo build` compiled) and their
/// partials would be stale or missing. The driver's identity is stamped in the target directory;
/// when it differs, the workspace members are cleaned and their partials removed.
fn refresh_if_driver_changed(
    driver: &Path,
    target_dir: &Path,
    partials_dir: &Path,
    meta: &Metadata,
    args: &[String],
) -> Result<(), String> {
    let md = std::fs::metadata(driver).map_err(|e| format!("{}: {e}", driver.display()))?;
    let mtime = md
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_nanos());
    let stamp = format!(
        "{}\n{}\n{}\n{mtime}\n{}\n",
        crate::tool_version(),
        rustc_plugin::CHANNEL,
        driver.display(),
        md.len()
    );
    let stamp_path = target_dir.join("hopper").join("driver-stamp");
    if std::fs::read_to_string(&stamp_path).ok().as_deref() == Some(stamp.as_str()) {
        return Ok(());
    }
    if !meta.members.is_empty() {
        let mut cmd = cargo();
        cmd.arg("clean").arg("--target-dir").arg(target_dir).arg("-q");
        if let Some(m) = flag_value(args, "--manifest-path") {
            cmd.args(["--manifest-path", m]);
        }
        for spec in &meta.members {
            cmd.args(["-p", spec]);
        }
        let status = cmd.status().map_err(|e| format!("cannot run cargo clean: {e}"))?;
        if !status.success() {
            return Err(format!("cargo clean of the workspace members failed ({status})"));
        }
    }
    match std::fs::remove_dir_all(partials_dir) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("{}: {e}", partials_dir.display())),
    }
    std::fs::create_dir_all(stamp_path.parent().unwrap_or(target_dir)).map_err(|e| e.to_string())?;
    std::fs::write(&stamp_path, stamp).map_err(|e| format!("{}: {e}", stamp_path.display()))
}

/// The value of `--flag value` or `--flag=value` among cargo's arguments.
fn flag_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--" {
            return None;
        }
        if a == flag {
            return it.next().map(String::as_str);
        }
        if let Some(v) = a.strip_prefix(flag).and_then(|r| r.strip_prefix('=')) {
            return Some(v);
        }
    }
    None
}

/// The sidecar `cargo hopper-build` writes for `binary`.
pub fn sidecar_for(binary: &Path) -> PathBuf {
    hopper_sidecar::sidecar_path(binary)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &[&str]) -> Vec<String> {
        v.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn flag_values() {
        assert_eq!(flag_value(&s(&["--target-dir", "/t"]), "--target-dir"), Some("/t"));
        assert_eq!(flag_value(&s(&["--target-dir=/t"]), "--target-dir"), Some("/t"));
        assert_eq!(flag_value(&s(&["--release"]), "--target-dir"), None);
        assert_eq!(flag_value(&s(&["--", "--target-dir", "/t"]), "--target-dir"), None);
    }
}
