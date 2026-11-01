//! `cargo hopper-build [cargo build args]`.
//!
//! Runs `cargo build` on the pinned nightly with `hopper-build-driver` as the workspace rustc
//! wrapper (so every workspace crate, and no dependency, is analysed), collects the executables
//! cargo reports, and links each one's sidecar from the partials the driver wrote beside its
//! profile's `deps/` (`<target>/<profile>/hopper-partials/`).
//!
//! rustc_plugin's own `cli_main` runs `cargo check`, which produces no binaries, so this CLI
//! drives cargo itself and reuses only rustc_plugin's driver side and its argument passing.
//!
//! Freshness: the driver records [`DRIVER_IDENTITY_VAR`] as an env dependency, so cargo rebuilds
//! a crate whenever a different driver build runs. A crate that cargo still considers fresh but
//! has no partial (it was last compiled by a plain `cargo build` on the same nightly, or its
//! partial was deleted) is cleaned for the selected profile and rebuilt once.

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use hopper_sidecar::link::PartialSet;

use crate::plugin::{DRIVER_IDENTITY_VAR, DRIVER_NAME, DriverArgs, PARTIALS_DIR_NAME, driver_identity};

/// What a build produced.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct BuildOutcome {
    /// The executables cargo built, each with the sidecar written beside it.
    pub sidecars: Vec<(PathBuf, PathBuf)>,
    /// Executables no partial described even after a rebuild.
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

/// One executable cargo reported.
struct Executable {
    path: PathBuf,
    package_id: String,
}

/// Build with sidecars. `cargo_args` are `cargo build`'s arguments.
pub fn run(cargo_args: &[String]) -> Result<BuildOutcome, String> {
    if cargo_args.iter().any(|a| a.starts_with("--message-format")) {
        return Err("--message-format is set by hopper-build".into());
    }
    let driver = std::env::current_exe()
        .map_err(|e| format!("cannot find own executable: {e}"))?
        .with_file_name(DRIVER_NAME);
    let identity = driver_identity(&driver).map_err(|e| format!("driver {}: {e}", driver.display()))?;
    let sysroot = sysroot()?;

    let tool = crate::tool_version();
    let mut outcome = BuildOutcome::default();
    let mut missing = link_all(&build(cargo_args, &driver, &identity, &sysroot)?, &tool, &mut outcome)?;
    if !missing.is_empty() {
        let packages: BTreeSet<&str> = missing.iter().map(|e| e.package_id.as_str()).collect();
        clean(cargo_args, &packages)?;
        missing = link_all(&build(cargo_args, &driver, &identity, &sysroot)?, &tool, &mut outcome)?;
    }
    outcome.without_sidecar = missing.into_iter().map(|e| e.path).collect();
    Ok(outcome)
}

/// Link the sidecar of every executable that has a partial; return those that have none.
fn link_all(executables: &[Executable], tool: &str, outcome: &mut BuildOutcome) -> Result<Vec<Executable>, String> {
    let mut missing = Vec::new();
    for exe in executables {
        let linked = match partials_dir_for(&exe.path) {
            Some(dir) => {
                let set = PartialSet::load_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
                set.link_binary(&exe.path, tool).map_err(|e| format!("{}: {e}", exe.path.display()))?
            }
            None => None,
        };
        match linked {
            Some(sc) => outcome.sidecars.push((exe.path.clone(), sc)),
            None => missing.push(Executable { path: exe.path.clone(), package_id: exe.package_id.clone() }),
        }
    }
    Ok(missing)
}

/// The partials directory of an executable's profile: beside the executable
/// (`target/debug/app`) or one level up (`target/debug/examples/app`, `target/debug/deps/test-…`).
fn partials_dir_for(exe: &Path) -> Option<PathBuf> {
    exe.ancestors().skip(1).take(2).map(|d| d.join(PARTIALS_DIR_NAME)).find(|d| d.is_dir())
}

/// The pinned toolchain's sysroot. The driver needs it to find that toolchain's std; rustc_plugin
/// would otherwise derive it from `RUSTUP_HOME` and `RUSTUP_TOOLCHAIN`, which names the channel
/// without its host triple and so no directory.
fn sysroot() -> Result<PathBuf, String> {
    let out = Command::new("rustc")
        .env("RUSTUP_TOOLCHAIN", rustc_plugin::CHANNEL)
        .args(["--print", "sysroot"])
        .stderr(Stdio::inherit())
        .output()
        .map_err(|e| format!("cannot run rustc --print sysroot: {e}"))?;
    if !out.status.success() {
        return Err(format!("rustc --print sysroot failed ({}); is {} installed?", out.status, rustc_plugin::CHANNEL));
    }
    Ok(PathBuf::from(String::from_utf8_lossy(&out.stdout).trim()))
}

fn build(cargo_args: &[String], driver: &Path, identity: &str, sysroot: &Path) -> Result<Vec<Executable>, String> {
    let driver_args = serde_json::to_string(&DriverArgs::default()).map_err(|e| e.to_string())?;
    let mut cmd = cargo();
    cmd.arg("build")
        .args(cargo_args)
        .arg("--message-format=json-render-diagnostics")
        .env("RUSTC_WORKSPACE_WRAPPER", driver)
        .env("PLUGIN_ARGS", driver_args)
        .env(DRIVER_IDENTITY_VAR, identity)
        .env("SYSROOT", sysroot)
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
                    executables.push(Executable {
                        path: PathBuf::from(exe),
                        package_id: msg["package_id"].as_str().unwrap_or_default().to_owned(),
                    });
                }
            }
            // Not a cargo message: a build script printed it.
            Err(_) => println!("{line}"),
        }
    }
    let status = child.wait().map_err(|e| format!("waiting for cargo: {e}"))?;
    if !status.success() {
        return Err(format!("cargo build failed ({status})"));
    }
    Ok(executables)
}

/// `cargo clean -p <package>...` for the profile, target and target directory `args` select.
fn clean(args: &[String], packages: &BTreeSet<&str>) -> Result<(), String> {
    let mut cmd = cargo();
    cmd.args(["clean", "-q"]);
    for flag in ["--manifest-path", "--target-dir", "--target", "--profile"] {
        if let Some(v) = flag_value(args, flag) {
            cmd.args([flag, v]);
        }
    }
    if args.iter().take_while(|a| *a != "--").any(|a| a == "--release" || a == "-r") {
        cmd.arg("--release");
    }
    for p in packages {
        cmd.args(["-p", p]);
    }
    let status = cmd.status().map_err(|e| format!("cannot run cargo clean: {e}"))?;
    if !status.success() {
        return Err(format!("cargo clean failed ({status})"));
    }
    Ok(())
}

/// `cargo` on the pinned nightly: the driver links that nightly's rustc, so the standard library
/// and every crate must come from it too.
fn cargo() -> Command {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo"));
    let mut cmd = Command::new(cargo);
    cmd.env("RUSTUP_TOOLCHAIN", rustc_plugin::CHANNEL);
    cmd
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

    #[test]
    fn partials_dir_is_found_beside_or_above() {
        let dir = tempfile::tempdir().unwrap();
        let profile = dir.path().join("debug");
        std::fs::create_dir_all(profile.join(PARTIALS_DIR_NAME)).unwrap();
        std::fs::create_dir_all(profile.join("examples")).unwrap();
        assert_eq!(partials_dir_for(&profile.join("app")), Some(profile.join(PARTIALS_DIR_NAME)));
        assert_eq!(partials_dir_for(&profile.join("examples/ex")), Some(profile.join(PARTIALS_DIR_NAME)));
        assert_eq!(partials_dir_for(&dir.path().join("elsewhere/x/y")), None);
    }
}
