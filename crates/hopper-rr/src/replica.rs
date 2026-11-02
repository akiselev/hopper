//! One rr replay server (`rr replay -s`) as a full [`Target`]: forward and reverse execution,
//! seeks, checkpoints and rr's own clock.
//!
//! **Time.** A [`Moment`]'s `frame_time` is rr's `when` (the last completely replayed event),
//! `ticks` is `when-ticks` for the running thread and `tid` is the thread the last stop named.
//! `sub`, the number of instructions since the tick count last changed, is hopper's own:
//! - a forward single step that leaves (event, thread, ticks) unchanged adds one; one that
//!   changes only the ticks lands on the first instruction of a new tick count, `sub` 0;
//! - a reverse single step that leaves them unchanged subtracts one;
//! - after any other resume `sub` is unmeasured, and [`Target::now`] measures it: it seeks to
//!   (event, ticks), the position `sub` 0 names, and steps forward until the general registers
//!   equal the ones it left, counting the steps. A continue that stops at a breakpoint can be
//!   several instructions past the last conditional branch, so resetting `sub` to 0 there would
//!   name a different instruction.
//!
//! **Seeking** to a moment is `vRun` to its event, `vRun t<ticks>` when the ticks differ, then
//! `sub` forward steps. rr removes every breakpoint and watchpoint on a `vRun`
//! (`GdbServer::restart_session`), so the replica inserts its own again afterwards. rr also
//! answers the first resume after a `vRun` with a stop at the same place (`interrupt_pending`
//! in `GdbServer::debug_one_step`); the replica absorbs that stop and resumes again.
//!
//! **The end of the recording.** rr reports the exit of the debuggee's last thread as a
//! synthetic SIGKILL stop when running forward, and the next forward resume as `W00` (its exit
//! code is always 0, "TODO return real exit code"). Both map to [`StopReason::HistoryEnd`];
//! after `W00` only seeks and checkpoint restores are accepted, because rr aborts on a resume
//! after the end of the trace.

use std::collections::HashMap;
use std::net::TcpStream;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use hopper_core::target::{
    Breakpoint, Direction, Resume, StopEvent, StopReason, Target, TargetInfo,
};
use hopper_core::{
    CheckpointId, HopperError, Moment, ProcessId, RecordingId, Reg, RegisterSet, Result, Tid,
};
use hopper_rsp::{Connection, RrSeek, RspClient, SignalStop, StopCause, StopReply};

use crate::remote::Remote;
use crate::rr::{Rr, RrVersion};
use crate::server::{ServerProcess, rr_listen_addr};

/// The gdb number of SIGKILL.
const GDB_SIGKILL: u8 = 9;

/// How to start `rr replay`.
#[derive(Clone, Debug)]
pub struct ReplayOptions {
    /// Debug this process (`-p <pid>`): the server starts once it has exec'd.
    pub process: Option<i32>,
    /// Start the server at this event (`-g`).
    pub goto_event: Option<u64>,
    /// How long rr may take to replay to its starting point and print its port.
    pub startup_timeout: Duration,
    /// Single steps [`Target::now`] may take to measure `sub` before giving up.
    pub sub_measure_budget: u32,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        ReplayOptions {
            process: None,
            goto_event: None,
            startup_timeout: Duration::from_secs(600),
            sub_measure_budget: 1_000_000,
        }
    }
}

/// Where the replica is relative to `sub`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum SubPosition {
    Known(u32),
    Unmeasured,
}

/// rr's clock at the current stop: (event, thread, ticks).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Clock {
    frame_time: u64,
    tid: Tid,
    ticks: u64,
}

/// One rr replay server.
#[derive(Debug)]
pub struct RrReplica<C: Connection = TcpStream> {
    remote: Remote<C>,
    info: TargetInfo,
    sub: SubPosition,
    clock: Option<Clock>,
    /// rr will answer the next resume with a stop in place (after a `vRun`).
    absorb_next_resume: bool,
    /// rr has reported `W00`: only seeks and restores may follow.
    past_end: bool,
    checkpoints: HashMap<CheckpointId, Moment>,
    sub_measure_budget: u32,
    _server: Option<ServerProcess>,
}

impl RrReplica<TcpStream> {
    /// Run `rr replay -s 0 -q [-p <pid>] [-g <event>] <trace>` and connect to it.
    pub fn start(
        rr: &Rr,
        trace_dir: &Path,
        options: &ReplayOptions,
        recording: RecordingId,
    ) -> Result<Self> {
        let mut cmd = rr.command();
        cmd.arg("replay").arg("-s").arg("0").arg("-q");
        if let Some(pid) = options.process {
            cmd.arg("-p").arg(pid.to_string());
        }
        if let Some(ev) = options.goto_event {
            cmd.arg("-g").arg(ev.to_string());
        }
        cmd.arg(trace_dir).stdout(Stdio::null());
        let (mut server, addr) =
            ServerProcess::spawn(cmd, rr_listen_addr, options.startup_timeout)?;
        let stream = server.connect(&addr)?;
        let mut r = RrReplica::attach(stream, recording, Some(rr.version))?;
        r.sub_measure_budget = options.sub_measure_budget;
        r._server = Some(server);
        Ok(r)
    }
}

/// The registers that identify an instruction boundary inside one tick count: the general
/// registers and rip. (eflags is left out: its resume flag differs between a breakpoint stop
/// and a single step to the same instruction.)
fn position_key(regs: &RegisterSet) -> Vec<(Reg, Vec<u8>)> {
    regs.values
        .iter()
        .filter(|(r, _)| r.0 <= Reg::RIP.0)
        .cloned()
        .collect()
}

impl<C: Connection> RrReplica<C> {
    /// Use an already-connected rr server.
    pub fn attach(conn: C, recording: RecordingId, rr_version: Option<RrVersion>) -> Result<Self> {
        let client = RspClient::connect(conn)?;
        let can_reverse = client.features().supports("ReverseContinue")
            && client.features().supports("ReverseStep");
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
            can_reverse,
            can_seek: true,
            rr_version: rr_version.map(|v| v.to_string()),
        };
        Ok(RrReplica {
            remote,
            info,
            // Where rr stops on connection is an arbitrary instruction.
            sub: SubPosition::Unmeasured,
            clock: None,
            absorb_next_resume: false,
            past_end: false,
            checkpoints: HashMap::new(),
            sub_measure_budget: ReplayOptions::default().sub_measure_budget,
            _server: None,
        })
    }

    pub fn client(&mut self) -> &mut RspClient<C> {
        &mut self.remote.client
    }

    /// The rr server's stderr so far.
    pub fn server_stderr(&self) -> Option<String> {
        self._server.as_ref().map(|s| s.stderr_text())
    }

    /// rr's `delete checkpoint`.
    pub fn delete_checkpoint(&mut self, id: CheckpointId) -> Result<()> {
        self.remote.client.rr_delete_checkpoint(id.0)?;
        self.checkpoints.remove(&id);
        Ok(())
    }

    /// Seek to the start of the region after `event` completes (`vRun` to an event number). The
    /// replica's breakpoints are inserted again; `sub` is measured when next asked.
    pub fn seek_event(&mut self, event: u64) -> Result<StopEvent> {
        let reply = self.vrun(RrSeek::Event(event))?;
        self.remote.reinsert_all()?;
        self.sub = SubPosition::Unmeasured;
        let tid = match &reply {
            StopReply::Stopped(SignalStop {
                thread: Some(t), ..
            }) => t.tid,
            _ => self.remote.current_tid()?,
        };
        Ok(StopEvent {
            tid,
            reason: StopReason::Arrived,
        })
    }

    /// The last event of the recording.
    pub fn last_event(&mut self) -> Result<u64> {
        self.remote.client.rr_when_end()
    }

    fn clock(&mut self) -> Result<Clock> {
        if let Some(c) = self.clock {
            return Ok(c);
        }
        let tid = self.remote.current_tid()?;
        let frame_time = self.remote.client.rr_when()?;
        let ticks = self.remote.client.rr_when_ticks(Some(tid))?;
        let c = Clock {
            frame_time,
            tid,
            ticks,
        };
        self.clock = Some(c);
        Ok(c)
    }

    fn after_vrun(&mut self, reply: &StopReply) {
        self.remote.forget_selection();
        self.clock = None;
        self.absorb_next_resume = true;
        self.past_end = false;
        if let StopReply::Stopped(SignalStop {
            thread: Some(t), ..
        }) = reply
        {
            self.remote.current = Some(*t);
        }
    }

    fn vrun(&mut self, to: RrSeek) -> Result<StopReply> {
        let reply = self.remote.client.rr_seek(to)?;
        self.after_vrun(&reply);
        Ok(reply)
    }

    /// Resume, absorbing the in-place stop rr answers the first resume after a `vRun` with.
    fn resume_absorbing(&mut self, req: hopper_rsp::ResumeRequest) -> Result<StopReply> {
        self.clock = None;
        if self.absorb_next_resume {
            self.absorb_next_resume = false;
            let tid = self.remote.current_tid()?;
            let pc_before = self.remote.registers(tid)?.pc();
            let reply = self.remote.resume_raw(req)?;
            let in_place = match &reply {
                StopReply::Stopped(s) => {
                    s.signal == 0
                        && s.cause == StopCause::Unspecified
                        && self.remote.registers(s.thread.map_or(tid, |t| t.tid))?.pc() == pc_before
                }
                _ => false,
            };
            if !in_place {
                return Ok(reply);
            }
        }
        self.remote.resume(req)
    }

    /// Seek to (event, thread, ticks) with `sub` 0, without inserting breakpoints again.
    fn seek_clock(&mut self, target: Clock) -> Result<()> {
        self.vrun(RrSeek::Event(target.frame_time))?;
        if self.clock()? != target {
            self.vrun(RrSeek::Ticks(target.ticks))?;
        }
        let got = self.clock()?;
        if got != target {
            return Err(HopperError::BadMoment(format!(
                "rr reached event {} thread {} ticks {}, not event {} thread {} ticks {}",
                got.frame_time, got.tid, got.ticks, target.frame_time, target.tid, target.ticks
            )));
        }
        Ok(())
    }

    /// Single-step `tid` forward `n` times, checking that the tick count stays put.
    fn step_within_ticks(&mut self, target: Clock, n: u32) -> Result<()> {
        for _ in 0..n {
            self.step_once_checked(target)?;
        }
        Ok(())
    }

    fn step_once_checked(&mut self, target: Clock) -> Result<()> {
        let req = self
            .remote
            .request_for(Resume::Step { tid: target.tid }, Direction::Forward);
        let reply = self.resume_absorbing(req)?;
        if !matches!(reply, StopReply::Stopped(_)) {
            return Err(HopperError::BadMoment(
                "the recording ended inside a seek".into(),
            ));
        }
        let now = self.clock()?;
        if now != target {
            return Err(HopperError::BadMoment(format!(
                "a step left event {} ticks {} (now event {} ticks {})",
                target.frame_time, target.ticks, now.frame_time, now.ticks
            )));
        }
        Ok(())
    }

    /// Measure `sub` at the current stop (see the module docs).
    fn measure_sub(&mut self, at: Clock) -> Result<u32> {
        let want = position_key(&self.remote.registers(at.tid)?);
        let measured = self.count_steps_to(at, &want);
        // The vRun removed every breakpoint: put them back whatever happened.
        let reinserted = self.remote.reinsert_all();
        let k = measured?;
        reinserted?;
        Ok(k)
    }

    fn count_steps_to(&mut self, at: Clock, want: &[(Reg, Vec<u8>)]) -> Result<u32> {
        self.seek_clock(at)?;
        let mut k = 0u32;
        loop {
            if position_key(&self.remote.registers(at.tid)?) == want {
                return Ok(k);
            }
            if k >= self.sub_measure_budget {
                return Err(HopperError::BudgetExhausted {
                    steps: u64::from(k),
                });
            }
            self.step_once_checked(at)?;
            k += 1;
        }
    }

    /// Map rr's stop, folding its end-of-recording stops into `HistoryEnd`.
    fn map_stop(&mut self, reply: &StopReply, how: Resume, dir: Direction) -> Result<StopEvent> {
        let tid = self
            .remote
            .current
            .map(|t| t.tid)
            .unwrap_or(Tid(self.info.process.pid));
        match reply {
            StopReply::Exited { .. } | StopReply::Killed { .. } => {
                self.past_end = true;
                Ok(StopEvent {
                    tid,
                    reason: StopReason::HistoryEnd,
                })
            }
            StopReply::Stopped(s)
                if dir == Direction::Forward
                    && s.signal == GDB_SIGKILL
                    && s.cause == StopCause::Unspecified =>
            {
                Ok(StopEvent {
                    tid: s.thread.map_or(tid, |t| t.tid),
                    reason: StopReason::HistoryEnd,
                })
            }
            _ => self
                .remote
                .stop_event(reply, matches!(how, Resume::Step { .. })),
        }
    }
}

impl<C: Connection> Target for RrReplica<C> {
    fn info(&self) -> &TargetInfo {
        &self.info
    }

    fn threads(&mut self) -> Result<Vec<Tid>> {
        self.remote.threads()
    }

    fn current_thread(&mut self) -> Result<Tid> {
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
        if self.past_end {
            return Err(HopperError::BadMoment(
                "the replay is past the end of the recording; seek first".into(),
            ));
        }
        let stepped = matches!(how, Resume::Step { .. });
        let before = if stepped { Some(self.clock()?) } else { None };
        let req = self.remote.request_for(how, dir);
        let reply = self.resume_absorbing(req)?;
        let ev = self.map_stop(&reply, how, dir)?;
        self.sub = match (before, &ev.reason) {
            (Some(before), StopReason::Step) => {
                let after = self.clock()?;
                let same_ticks = after == before;
                let same_event_and_thread =
                    after.frame_time == before.frame_time && after.tid == before.tid;
                match (dir, self.sub) {
                    (Direction::Forward, SubPosition::Known(k)) if same_ticks => {
                        SubPosition::Known(k + 1)
                    }
                    (Direction::Forward, _)
                        if same_event_and_thread && after.ticks != before.ticks =>
                    {
                        SubPosition::Known(0)
                    }
                    (Direction::Reverse, SubPosition::Known(k)) if same_ticks && k > 0 => {
                        SubPosition::Known(k - 1)
                    }
                    _ => SubPosition::Unmeasured,
                }
            }
            _ => SubPosition::Unmeasured,
        };
        Ok(ev)
    }

    fn now(&mut self) -> Result<Moment> {
        let clock = self.clock()?;
        let sub = match self.sub {
            SubPosition::Known(k) => k,
            SubPosition::Unmeasured => {
                let k = self.measure_sub(clock)?;
                self.sub = SubPosition::Known(k);
                k
            }
        };
        Ok(Moment {
            recording: self.info.recording,
            frame_time: clock.frame_time,
            tid: clock.tid,
            ticks: clock.ticks,
            sub,
        })
    }

    fn seek(&mut self, to: &Moment) -> Result<StopEvent> {
        if to.recording != self.info.recording {
            return Err(HopperError::BadMoment(format!(
                "moment {to} belongs to recording {}, not {}",
                to.recording, self.info.recording
            )));
        }
        let target = Clock {
            frame_time: to.frame_time,
            tid: to.tid,
            ticks: to.ticks,
        };
        // Already inside the right tick count, at or before the instruction: step forward.
        let here = if self.past_end {
            None
        } else {
            Some(self.clock()?)
        };
        match (here, self.sub) {
            (Some(c), SubPosition::Known(k)) if c == target && k <= to.sub => {
                self.step_within_ticks(target, to.sub - k)?;
            }
            _ => {
                let arrived = self
                    .seek_clock(target)
                    .and_then(|()| self.step_within_ticks(target, to.sub));
                let reinserted = self.remote.reinsert_all();
                if let Err(e) = arrived {
                    self.sub = SubPosition::Unmeasured;
                    return Err(e);
                }
                reinserted?;
            }
        }
        self.sub = SubPosition::Known(to.sub);
        Ok(StopEvent {
            tid: to.tid,
            reason: StopReason::Arrived,
        })
    }

    fn checkpoint(&mut self) -> Result<CheckpointId> {
        let at = self.now()?;
        let id = CheckpointId(self.remote.client.rr_checkpoint(&at.to_string())?);
        self.checkpoints.insert(id, at);
        Ok(id)
    }

    fn restore(&mut self, id: CheckpointId) -> Result<StopEvent> {
        self.vrun(RrSeek::Checkpoint(id.0))?;
        self.remote.reinsert_all()?;
        self.sub = match self.checkpoints.get(&id) {
            Some(m) => SubPosition::Known(m.sub),
            None => SubPosition::Unmeasured,
        };
        let tid = self.remote.current_tid()?;
        Ok(StopEvent {
            tid,
            reason: StopReason::Arrived,
        })
    }

    fn auxv(&mut self) -> Result<Vec<u8>> {
        self.remote.client.auxv()
    }
}
