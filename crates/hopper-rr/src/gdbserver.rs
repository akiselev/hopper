//! A live process under gdbserver: the forward-only [`Target`] hopper tests against on machines
//! without a PMU.
//!
//! Time on a live target is synthetic: `frame_time` counts stops (every resume that is not a
//! single step adds one), `ticks` is 0, and `sub` counts the single steps since the last such
//! resume. Reverse execution, seeking and checkpoints are [`HopperError::Unsupported`].

use std::ffi::OsString;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use hopper_core::target::{Breakpoint, Direction, Resume, StopEvent, Target, TargetInfo};
use hopper_core::{
    CheckpointId, HopperError, Moment, ProcessId, RecordingId, RegisterSet, Result, Tid,
};
use hopper_rsp::{Connection, RspClient};

use crate::remote::Remote;
use crate::server::{ServerProcess, gdbserver_listen_addr};

/// How to start gdbserver.
#[derive(Clone, Debug)]
pub struct GdbserverConfig {
    pub gdbserver: PathBuf,
    /// How long to wait for gdbserver to report its port.
    pub startup_timeout: Duration,
    /// Where the debuggee's stdout goes.
    pub debuggee_stdout: OutputPolicy,
}

/// What to do with a child's standard output.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OutputPolicy {
    Discard,
    Inherit,
}

impl Default for GdbserverConfig {
    fn default() -> Self {
        GdbserverConfig {
            gdbserver: PathBuf::from("gdbserver"),
            startup_timeout: Duration::from_secs(30),
            debuggee_stdout: OutputPolicy::Discard,
        }
    }
}

/// A process started (or attached to) through gdbserver.
#[derive(Debug)]
pub struct GdbserverTarget<C: Connection = TcpStream> {
    remote: Remote<C>,
    info: TargetInfo,
    stops: u64,
    steps: u32,
    // Dropped last: the connection closes before the server is killed.
    _server: Option<ServerProcess>,
}

impl GdbserverTarget<TcpStream> {
    /// Run `gdbserver --once --no-startup-with-shell 127.0.0.1:0 <program> <args>`, connect to
    /// the port it prints and stop at the program's first instruction.
    ///
    /// gdbserver without a shell cannot pass an argument containing whitespace; such arguments
    /// are refused here rather than by gdbserver's exit.
    pub fn launch(config: &GdbserverConfig, program: &Path, args: &[OsString]) -> Result<Self> {
        if let Some(a) = args.iter().find(|a| a.to_string_lossy().contains(char::is_whitespace)) {
            return Err(HopperError::Invalid(format!(
                "gdbserver cannot pass the argument {a:?}: it contains whitespace"
            )));
        }
        let mut cmd = Command::new(&config.gdbserver);
        cmd.arg("--once")
            .arg("--no-startup-with-shell")
            .arg("127.0.0.1:0")
            .arg(program)
            .args(args);
        cmd.stdout(match config.debuggee_stdout {
            OutputPolicy::Discard => Stdio::null(),
            OutputPolicy::Inherit => Stdio::inherit(),
        });
        let (mut server, addr) =
            ServerProcess::spawn(cmd, gdbserver_listen_addr, config.startup_timeout)?;
        let stream = server.connect(&addr)?;
        let mut t = GdbserverTarget::attach(stream, RecordingId(*uuid::Uuid::new_v4().as_bytes()))?;
        t._server = Some(server);
        Ok(t)
    }
}

impl<C: Connection> GdbserverTarget<C> {
    /// Use an already-connected gdbserver. A live target has no stable identity, so the caller
    /// names the recording (normally a fresh random id).
    pub fn attach(conn: C, recording: RecordingId) -> Result<Self> {
        let client = RspClient::connect(conn)?;
        let mut remote = Remote::new(client);
        remote.initial_stop()?;
        let pid = remote.pid.unwrap_or(remote.current_tid()?.0);
        let executable = if remote.client.features().supports("qXfer:exec-file:read") {
            PathBuf::from(remote.client.exec_file(Some(pid))?)
        } else {
            PathBuf::new()
        };
        let info = TargetInfo {
            recording,
            process: ProcessId::new(pid),
            executable,
            can_reverse: false,
            can_seek: false,
            rr_version: None,
        };
        Ok(GdbserverTarget { remote, info, stops: 0, steps: 0, _server: None })
    }

    /// The server's stderr so far (gdbserver's messages and the debuggee's stderr).
    pub fn server_stderr(&self) -> Option<String> {
        self._server.as_ref().map(|s| s.stderr_text())
    }

    pub fn client(&mut self) -> &mut RspClient<C> {
        &mut self.remote.client
    }
}

impl<C: Connection> Target for GdbserverTarget<C> {
    fn info(&self) -> &TargetInfo {
        &self.info
    }

    fn threads(&mut self) -> Result<Vec<Tid>> {
        self.remote.threads()
    }

    fn current_thread(&mut self) -> Result<Tid> {
        self.remote.check_alive()?;
        self.remote.current_tid()
    }

    fn registers(&mut self, tid: Tid) -> Result<RegisterSet> {
        self.remote.registers(tid)
    }

    fn read_memory(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        self.remote.read_memory(addr, buf)
    }

    fn insert(&mut self, bp: Breakpoint) -> Result<()> {
        self.remote.insert(bp)
    }

    fn remove(&mut self, bp: Breakpoint) -> Result<()> {
        self.remote.remove(bp)
    }

    fn resume(&mut self, how: Resume, dir: Direction) -> Result<StopEvent> {
        if dir == Direction::Reverse {
            return Err(HopperError::Unsupported("reverse execution on a live gdbserver".into()));
        }
        let req = self.remote.request_for(how, dir);
        let reply = self.remote.resume_raw(req)?;
        let stepped = matches!(how, Resume::Step { .. });
        if stepped {
            self.steps += 1;
        } else {
            self.stops += 1;
            self.steps = 0;
        }
        self.remote.stop_event(&reply, stepped)
    }

    fn now(&mut self) -> Result<Moment> {
        let tid = match &self.remote.ended {
            Some(e) => e.tid,
            None => self.remote.current_tid()?,
        };
        Ok(Moment {
            recording: self.info.recording,
            frame_time: self.stops,
            tid,
            ticks: 0,
            sub: self.steps,
        })
    }

    fn seek(&mut self, _to: &Moment) -> Result<StopEvent> {
        Err(HopperError::Unsupported("seeking on a live gdbserver".into()))
    }

    fn checkpoint(&mut self) -> Result<CheckpointId> {
        Err(HopperError::Unsupported("checkpoints on a live gdbserver".into()))
    }

    fn restore(&mut self, _id: CheckpointId) -> Result<StopEvent> {
        Err(HopperError::Unsupported("checkpoints on a live gdbserver".into()))
    }

    fn auxv(&mut self) -> Result<Vec<u8>> {
        self.remote.check_alive()?;
        self.remote.client.auxv()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::target::{BreakKind, StopReason};
    use hopper_rsp::script::{Script, ScriptedConnection};

    const QSUP: &str =
        "qSupported:multiprocess+;swbreak+;hwbreak+;vContSupported+;xmlRegisters=i386";
    const GDBSERVER_SUPPORTED: &str = "PacketSize=2001f;QPassSignals+;qXfer:auxv:read+;\
qXfer:features:read+;QStartNoAckMode+;multiprocess+;qXfer:threads:read+;swbreak+;hwbreak+;\
qXfer:exec-file:read+;vContSupported+";

    fn attach(script: Script) -> (GdbserverTarget<ScriptedConnection>, hopper_rsp::script::ScriptLog) {
        let s = Script::new()
            .exchange(QSUP, GDBSERVER_SUPPORTED)
            .exchange("QStartNoAckMode", "OK")
            .exchange("?", "T0506:0000000000000000;thread:p2a.2a;core:1;")
            .exchange("qXfer:exec-file:read:2a:0,ffff", "l/tmp/prog")
            .then(script);
        let (conn, log) = ScriptedConnection::new(s);
        let t = GdbserverTarget::attach(conn, RecordingId([7; 16])).unwrap();
        (t, log)
    }

    #[test]
    fn attaches_and_describes_the_process() {
        let (mut t, log) = attach(Script::new());
        assert_eq!(t.info().process, ProcessId::new(0x2a));
        assert_eq!(t.info().executable, PathBuf::from("/tmp/prog"));
        assert!(!t.info().can_reverse);
        assert_eq!(t.current_thread().unwrap(), Tid(0x2a));
        let m = t.now().unwrap();
        assert_eq!((m.frame_time, m.ticks, m.sub, m.tid), (0, 0, 0, Tid(0x2a)));
        log.assert_finished();
    }

    #[test]
    fn counts_stops_and_steps_in_synthetic_time() {
        let tdesc = r#"<target><architecture>i386:x86-64</architecture>
<reg name="rax" bitsize="64" regnum="0"/><reg name="rip" bitsize="64" regnum="16"/></target>"#;
        let script = Script::new()
            .exchange("Z0,401000,1", "OK")
            .exchange("vCont?", "vCont;c;C;t;s;S;r")
            .exchange("vCont;c", "T05swbreak:;10:0010400000000000;thread:p2a.2a;core:1;")
            .exchange("qXfer:features:read:target.xml:0,ffff", format!("l{tdesc}"))
            .exchange("vCont;s:p2a.2a", "T0510:0110400000000000;thread:p2a.2a;core:1;")
            .exchange("vCont;s:p2a.2a", "T0510:0410400000000000;thread:p2a.2a;core:1;")
            .exchange("vCont;c", "W00;process:2a");
        let (mut t, log) = attach(script);
        t.insert(Breakpoint::code(0x401000)).unwrap();
        let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
        assert_eq!(ev.reason, StopReason::Breakpoint { addr: 0x401000 });
        assert_eq!(t.now().unwrap().order_key(), (1, 0, 0));
        let tid = Tid(0x2a);
        assert_eq!(t.resume(Resume::Step { tid }, Direction::Forward).unwrap().reason, StopReason::Step);
        assert_eq!(t.resume(Resume::Step { tid }, Direction::Forward).unwrap().reason, StopReason::Step);
        assert_eq!(t.now().unwrap().order_key(), (1, 0, 2));
        let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
        assert_eq!(ev.reason, StopReason::Exited { code: 0 });
        assert_eq!(t.now().unwrap().order_key(), (2, 0, 0));
        assert!(matches!(t.registers(tid), Err(HopperError::Disconnected(_))));
        log.assert_finished();
    }

    #[test]
    fn refuses_time_travel() {
        let (mut t, log) = attach(Script::new());
        let m = t.now().unwrap();
        assert!(matches!(t.resume(Resume::Continue, Direction::Reverse), Err(HopperError::Unsupported(_))));
        assert!(matches!(t.seek(&m), Err(HopperError::Unsupported(_))));
        assert!(matches!(t.checkpoint(), Err(HopperError::Unsupported(_))));
        assert!(matches!(t.restore(CheckpointId(1)), Err(HopperError::Unsupported(_))));
        log.assert_finished();
    }

    #[test]
    fn tracks_watchpoints_and_removal() {
        let script = Script::new()
            .exchange("Z2,7ffd0000,8", "OK")
            .exchange("vCont?", "vCont;c;s")
            .exchange("vCont;c", "T05watch:7ffd0004;thread:p2a.2b;")
            .exchange("z2,7ffd0000,8", "OK");
        let (mut t, log) = attach(script);
        let w = Breakpoint::write_watch(0x7ffd0000, 8);
        t.insert(w).unwrap();
        t.insert(w).unwrap(); // already inserted: nothing sent
        let ev = t.resume(Resume::Continue, Direction::Forward).unwrap();
        assert_eq!(ev.tid, Tid(0x2b));
        assert_eq!(ev.reason, StopReason::Watchpoint { kind: BreakKind::WriteWatch, addr: 0x7ffd0004 });
        t.remove(w).unwrap();
        assert!(matches!(t.remove(w), Err(HopperError::NotFound(_))));
        log.assert_finished();
    }
}
