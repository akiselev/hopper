//! The `rr` executable: version detection and the commands hopper runs to completion
//! (`record`, `ps`, `traceinfo`, `pack`).
//!
//! Output formats were read from rr's source at ca20dcb: `rr --version` prints
//! `rr version <X.Y.Z> <extra>` (main.cc), `rr ps` prints a tab-separated table headed
//! `PID PPID EXIT CMD` (PsCommand.cc), `rr traceinfo` prints a JSON object whose `uuid` is an
//! array of 16 byte values (TraceInfoCommand.cc).

use std::ffi::{OsStr, OsString};
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Output, Stdio};

use hopper_core::{HopperError, RecordingId, Result};
use serde::Deserialize;

/// An rr release.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RrVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl RrVersion {
    /// The releases whose server replies and trace formats hopper was written against.
    pub const SUPPORTED: [(u32, u32); 2] = [(5, 9), (5, 10)];

    /// Parse `rr --version` output.
    pub fn parse(output: &str) -> Result<RrVersion> {
        let bad = || HopperError::Invalid(format!("unrecognised rr version output {output:?}"));
        let rest = output.trim().strip_prefix("rr version ").ok_or_else(bad)?;
        let number = rest.split_whitespace().next().ok_or_else(bad)?;
        let mut parts = number.split('.').map(|p| p.parse::<u32>());
        let mut next = || parts.next().ok_or_else(bad)?.map_err(|_| bad());
        let major = next()?;
        let minor = next()?;
        let patch = next().unwrap_or(0);
        Ok(RrVersion { major, minor, patch })
    }

    pub fn is_supported(&self) -> bool {
        Self::SUPPORTED.contains(&(self.major, self.minor))
    }
}

impl fmt::Display for RrVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A located, version-checked rr.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rr {
    pub binary: PathBuf,
    pub version: RrVersion,
}

fn run(cmd: &mut Command) -> Result<Output> {
    let desc = format!("{cmd:?}");
    cmd.stdin(Stdio::null())
        .output()
        .map_err(|e| HopperError::Io(format!("cannot run {desc}: {e}")))
}

fn failed(what: &str, out: &Output) -> HopperError {
    HopperError::TargetRefused {
        request: what.to_string(),
        reply: format!(
            "{}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ),
    }
}

/// How `rr pack` stores the files a trace maps.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PackMode {
    /// Copy them into the trace (the default).
    Copy,
    /// Symlink them (`--symlink`).
    Symlink,
}

/// Options for `rr record`.
#[derive(Clone, Debug, Default)]
pub struct RecordOptions {
    /// The trace directory to create (`-o`). It must not exist. When `None`, hopper picks a
    /// fresh one under rr's save directory.
    pub output_dir: Option<PathBuf>,
    /// Extra environment for the recorded program.
    pub env: Vec<(OsString, OsString)>,
    /// Extra `rr record` options, placed before the program (for example `--chaos`).
    pub rr_args: Vec<OsString>,
}

/// A finished recording.
#[derive(Debug)]
pub struct Recorded {
    pub trace_dir: PathBuf,
    /// rr's own exit status, which is the recorded program's.
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

/// How a process in a recording ended (`rr ps` EXIT column).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProcessExit {
    Code(i32),
    /// Killed by this signal (rr prints it negated).
    Signal(i32),
    /// It detached from rr.
    Detached,
    /// The recording ends before it does.
    Running,
}

/// One row of `rr ps`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PsRow {
    pub pid: i32,
    /// The pid inside the process's own pid namespace, when rr shows one.
    pub namespace_pid: Option<i32>,
    /// `None` for the initial process (`--`).
    pub ppid: Option<i32>,
    pub exit: ProcessExit,
    /// The exec'd command line, or rr's note (`(forked without exec)`).
    pub command: String,
}

/// Parse `rr ps` output.
pub fn parse_ps(text: &str) -> Result<Vec<PsRow>> {
    let mut lines = text.lines();
    let header = lines.next().unwrap_or_default();
    if header.trim() != "PID\tPPID\tEXIT\tCMD" {
        return Err(HopperError::Protocol(format!("unexpected rr ps header {header:?}")));
    }
    let mut rows = Vec::new();
    for line in lines.filter(|l| !l.is_empty()) {
        let bad = || HopperError::Protocol(format!("bad rr ps row {line:?}"));
        let mut cols = line.splitn(4, '\t');
        let pid_col = cols.next().ok_or_else(bad)?;
        let ppid_col = cols.next().ok_or_else(bad)?;
        let exit_col = cols.next().ok_or_else(bad)?;
        let command = cols.next().ok_or_else(bad)?.to_string();
        let (pid, namespace_pid) = match pid_col.split_once(" (") {
            Some((p, ns)) => (
                p.parse().map_err(|_| bad())?,
                Some(ns.trim_end_matches(')').parse().map_err(|_| bad())?),
            ),
            None => (pid_col.parse().map_err(|_| bad())?, None),
        };
        let ppid = match ppid_col {
            "--" => None,
            p => Some(p.parse().map_err(|_| bad())?),
        };
        let exit = match exit_col {
            "detach" => ProcessExit::Detached,
            "none" => ProcessExit::Running,
            n => {
                let v: i32 = n.parse().map_err(|_| bad())?;
                if v < 0 { ProcessExit::Signal(-v) } else { ProcessExit::Code(v) }
            }
        };
        rows.push(PsRow { pid, namespace_pid, ppid, exit, command });
    }
    Ok(rows)
}

/// The fields of `rr traceinfo` hopper reads; the whole object is kept in `json`.
#[derive(Clone, Debug, PartialEq)]
pub struct TraceInfo {
    pub uuid: RecordingId,
    /// `rcb` (retired conditional branches) or `branches`.
    pub ticks_semantics: String,
    pub required_forward_compatibility_version: Option<i64>,
    pub program: Option<String>,
    pub json: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TraceInfoFields {
    uuid: Vec<u8>,
    ticks_semantics: String,
    required_forward_compatibility_version: Option<i64>,
    program: Option<String>,
}

/// Parse `rr traceinfo` output.
pub fn parse_traceinfo(text: &str) -> Result<TraceInfo> {
    let json: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| HopperError::Protocol(format!("rr traceinfo is not JSON: {e}")))?;
    let f: TraceInfoFields = serde_json::from_value(json.clone())
        .map_err(|e| HopperError::Protocol(format!("rr traceinfo fields: {e}")))?;
    let uuid: [u8; 16] = f.uuid.try_into().map_err(|v: Vec<u8>| {
        HopperError::Protocol(format!("rr traceinfo uuid has {} bytes, not 16", v.len()))
    })?;
    Ok(TraceInfo {
        uuid: RecordingId(uuid),
        ticks_semantics: f.ticks_semantics,
        required_forward_compatibility_version: f.required_forward_compatibility_version,
        program: f.program,
        json,
    })
}

/// rr's default trace directory (`trace_save_dir` in rr): `$_RR_TRACE_DIR`, else
/// `$XDG_DATA_HOME/rr`, else `$HOME/.local/share/rr`.
pub fn default_trace_root() -> Result<PathBuf> {
    if let Some(d) = std::env::var_os("_RR_TRACE_DIR").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d));
    }
    if let Some(d) = std::env::var_os("XDG_DATA_HOME").filter(|d| !d.is_empty()) {
        return Ok(PathBuf::from(d).join("rr"));
    }
    let home = std::env::var_os("HOME")
        .ok_or_else(|| HopperError::Invalid("neither _RR_TRACE_DIR nor HOME is set".into()))?;
    Ok(PathBuf::from(home).join(".local/share/rr"))
}

impl Rr {
    /// Find `rr` on `PATH` and check its version.
    pub fn detect() -> Result<Rr> {
        Rr::at("rr")
    }

    /// Use the rr at `binary` and check its version.
    pub fn at(binary: impl Into<PathBuf>) -> Result<Rr> {
        let binary = binary.into();
        let out = run(Command::new(&binary).arg("--version"))?;
        if !out.status.success() {
            return Err(failed("rr --version", &out));
        }
        let version = RrVersion::parse(&String::from_utf8_lossy(&out.stdout))?;
        if !version.is_supported() {
            return Err(HopperError::Unsupported(format!(
                "rr {version}: hopper supports rr 5.9 and 5.10"
            )));
        }
        Ok(Rr { binary, version })
    }

    pub fn command(&self) -> Command {
        Command::new(&self.binary)
    }

    /// `rr record -o <dir> <program> <args>`; returns once the program finishes.
    pub fn record(
        &self,
        program: &Path,
        args: &[OsString],
        options: &RecordOptions,
    ) -> Result<Recorded> {
        let trace_dir = match &options.output_dir {
            Some(d) => d.clone(),
            None => {
                let root = default_trace_root()?;
                std::fs::create_dir_all(&root)?;
                let name = program.file_name().unwrap_or(OsStr::new("program")).to_string_lossy();
                root.join(format!("{name}-hopper-{}", &uuid::Uuid::new_v4().simple().to_string()[..12]))
            }
        };
        let mut cmd = self.command();
        cmd.arg("record").arg("-o").arg(&trace_dir).args(&options.rr_args).arg(program).args(args);
        for (k, v) in &options.env {
            cmd.env(k, v);
        }
        let out = run(&mut cmd)?;
        if !trace_dir.join("version").is_file() {
            return Err(failed("rr record", &out));
        }
        Ok(Recorded { trace_dir, status: out.status, stdout: out.stdout, stderr: out.stderr })
    }

    /// `rr ps <trace>`.
    pub fn ps(&self, trace_dir: &Path) -> Result<Vec<PsRow>> {
        let out = run(self.command().arg("ps").arg(trace_dir))?;
        if !out.status.success() {
            return Err(failed("rr ps", &out));
        }
        parse_ps(&String::from_utf8_lossy(&out.stdout))
    }

    /// `rr traceinfo <trace>`.
    pub fn traceinfo(&self, trace_dir: &Path) -> Result<TraceInfo> {
        let out = run(self.command().arg("traceinfo").arg(trace_dir))?;
        if !out.status.success() {
            return Err(failed("rr traceinfo", &out));
        }
        parse_traceinfo(&String::from_utf8_lossy(&out.stdout))
    }

    /// The recording id of a trace: its uuid.
    pub fn recording_id(&self, trace_dir: &Path) -> Result<RecordingId> {
        Ok(self.traceinfo(trace_dir)?.uuid)
    }

    /// `rr pack [--symlink] <trace>`.
    pub fn pack(&self, trace_dir: &Path, mode: PackMode) -> Result<()> {
        let mut cmd = self.command();
        cmd.arg("pack");
        if mode == PackMode::Symlink {
            cmd.arg("--symlink");
        }
        let out = run(cmd.arg(trace_dir))?;
        if !out.status.success() {
            return Err(failed("rr pack", &out));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions() {
        let v = RrVersion::parse("rr version 5.10.0 \n").unwrap();
        assert_eq!(v, RrVersion { major: 5, minor: 10, patch: 0 });
        assert!(v.is_supported());
        assert!(RrVersion::parse("rr version 5.9.0 extra").unwrap().is_supported());
        assert!(!RrVersion::parse("rr version 5.8.0").unwrap().is_supported());
        assert!(!RrVersion::parse("rr version 6.0").unwrap().is_supported());
        assert!(RrVersion::parse("gdb 15").is_err());
        assert!(RrVersion::parse("rr version x.y").is_err());
    }

    #[test]
    fn parses_ps_rows() {
        let text = "PID\tPPID\tEXIT\tCMD\n\
                    4242\t--\t0\t/tmp/prog a b\n\
                    4243 (2)\t4242\t-9\t/bin/sh -c x\n\
                    4244\t4242\tnone\t(forked without exec)\n\
                    4245\t4243\tdetach\t/bin/true\n";
        let rows = parse_ps(text).unwrap();
        assert_eq!(rows.len(), 4);
        assert_eq!(rows[0].pid, 4242);
        assert_eq!(rows[0].ppid, None);
        assert_eq!(rows[0].exit, ProcessExit::Code(0));
        assert_eq!(rows[0].command, "/tmp/prog a b");
        assert_eq!(rows[1].namespace_pid, Some(2));
        assert_eq!(rows[1].exit, ProcessExit::Signal(9));
        assert_eq!(rows[2].exit, ProcessExit::Running);
        assert_eq!(rows[3].exit, ProcessExit::Detached);
        assert!(parse_ps("PID PPID\n").is_err());
        assert!(parse_ps("PID\tPPID\tEXIT\tCMD\nx\t--\t0\ty\n").is_err());
    }

    #[test]
    fn parses_traceinfo() {
        let text = r#"{
  "uuid":[1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16],
  "xcr0":231,
  "bindToCpu":3,
  "cpuidFaulting":false,
  "requiredForwardCompatibilityVersion":0,
  "ticksSemantics":"rcb",
  "cpuidRecords":[
    [0,0,22,1970169159,1818588270,1231384169]
  ],
  "environ":[
    "HOME=/root"
  ],
  "program": "/tmp/prog"
}"#;
        let info = parse_traceinfo(text).unwrap();
        assert_eq!(info.uuid.to_hex(), "0102030405060708090a0b0c0d0e0f10");
        assert_eq!(info.ticks_semantics, "rcb");
        assert_eq!(info.program.as_deref(), Some("/tmp/prog"));
        assert_eq!(info.json["bindToCpu"], 3);
        assert!(parse_traceinfo(r#"{"uuid":[1],"ticksSemantics":"rcb"}"#).is_err());
        assert!(parse_traceinfo("Replay finished").is_err());
    }
}
