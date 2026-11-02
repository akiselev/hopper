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
//! **Cheap moments at breakpoint stops** ([`Target::now_at_stop`]). Measuring `sub` after a
//! continue costs a seek backwards: rr restores its nearest checkpoint before the tick region
//! and replays forward from there, so an indexer that asks for the moment of every breakpoint
//! hit pays a checkpoint restore plus up to a checkpoint interval of replay per hit. A stop can
//! be named exactly without that: inside one tick region the same PC can come round again (a
//! `rep` instruction, a function called twice in straight-line code), but "the `n`-th
//! instruction boundary of region (event, thread, ticks) whose `rip` is `pc`" names one
//! boundary, as `sub` does (a [`RegionVisit`]). The replica counts visits as it goes, in a
//! [`VisitTracker`]:
//! - A software breakpoint traps every time its instruction is about to run, and the replica
//!   steps over a breakpoint it starts a resume on (so a position is never reported twice). So
//!   when a resume lands in a *new* region (its clock differs from the previous stop's), every
//!   position of that region so far was reached during the resume, and every one whose PC holds
//!   a breakpoint inserted when the resume began was reported: the tracker starts with those
//!   breakpoint addresses at zero visits. Each later stop in the same region adds one visit to
//!   its PC when the PC is tracked. A breakpoint inserted in the middle of a region is not
//!   tracked until the next region (its earlier visits were not seen); a removed one stops
//!   being tracked.
//! - A walk from the region start (a seek, or measuring `sub`) sees every position, so it
//!   rebuilds the tracker for every inserted breakpoint address.
//! - Reverse execution, `vRun` seeks without a walk and checkpoint restores forget the tracker.
//!
//! [`Target::now_at_stop`] answers with the visit when `sub` is not known and the stop's PC is
//! tracked, and measures otherwise. [`Target::resolve_visits`] turns visits into moments in
//! bulk: it sorts them by region and walks each region once from its start, seeking only
//! forwards between regions (rr replays forward from where it is when the target is ahead of
//! it, without restoring a checkpoint), then seeks forward again to where it started. A batch
//! of visits thus costs one checkpoint restore (to the first region) and one forward replay,
//! instead of one restore per stop.
//!
//! **Seeking** to a moment is `vRun` to its event, `vRun t<ticks>` when the ticks differ, then
//! `sub` forward steps. rr removes every breakpoint and watchpoint on a `vRun`
//! (`GdbServer::restart_session`), so the replica inserts its own again afterwards. rr also
//! answers the first resume after a `vRun` with a stop at the same place (`interrupt_pending`
//! in `GdbServer::debug_one_step`); the replica absorbs that stop and resumes again.
//!
//! **Thread events.** rr does not implement `QThreadEvents` (`GdbServerConnection::set_var`
//! leaves it unhandled), and its stop replies say nothing about clones or the exit of a thread
//! that is not the last (`GdbServer::maybe_notify_stop` reports only the last thread's exit).
//! [`Target::enable_thread_events`] therefore compares the thread list at every forward stop
//! ([`ThreadEventSource::ThreadListDiff`]).
//!
//! **The thread rr runs next** is `qRRCmd when-tid` ([`Target::scheduled_thread`]): rr can only
//! run the task its trace schedules next and treats a step of any other thread as a continue
//! of that task (`compute_run_command_from_actions`), so stepping `when-tid` each time steps
//! every thread in recorded order.
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
    BreakKind, Breakpoint, Direction, Resume, StopEvent, StopReason, Target, TargetInfo,
    ThreadEvent, ThreadEventSource,
};
use hopper_core::{
    CheckpointId, HopperError, Moment, ProcessId, RecordingId, Reg, RegionVisit, RegisterSet,
    Result, StopMoment, Tid,
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

impl Clock {
    /// The moment of this clock's region start.
    fn region_start(self, recording: RecordingId) -> Moment {
        Moment {
            recording,
            frame_time: self.frame_time,
            tid: self.tid,
            ticks: self.ticks,
            sub: 0,
        }
    }

    fn of_moment(m: &Moment) -> Clock {
        Clock {
            frame_time: m.frame_time,
            tid: m.tid,
            ticks: m.ticks,
        }
    }
}

/// Visits counted in the tick region the replica is in (see the module docs).
#[derive(Clone, Debug, PartialEq, Eq)]
struct VisitTracker {
    clock: Clock,
    /// Code addresses every visit of which in this region so far was counted, with the count
    /// (the current position included).
    counts: HashMap<u64, u32>,
    /// The PC of the current position.
    pc: u64,
}

impl VisitTracker {
    /// A tracker for a region the replica has just entered at `pc`, with every visit of the
    /// `armed` addresses in it seen.
    fn entered(clock: Clock, armed: &[u64], pc: u64) -> VisitTracker {
        let mut t = VisitTracker {
            clock,
            counts: armed.iter().map(|a| (*a, 0)).collect(),
            pc,
        };
        t.arrive(pc);
        t
    }

    fn arrive(&mut self, pc: u64) {
        self.pc = pc;
        if let Some(c) = self.counts.get_mut(&pc) {
            *c += 1;
        }
    }

    /// The visit naming the current position, when its PC is tracked.
    fn current_visit(&self) -> Option<(u64, u32)> {
        let n = *self.counts.get(&self.pc)?;
        n.checked_sub(1).map(|v| (self.pc, v))
    }
}

/// One rr replay server.
#[derive(Debug)]
pub struct RrReplica<C: Connection = TcpStream> {
    remote: Remote<C>,
    info: TargetInfo,
    sub: SubPosition,
    clock: Option<Clock>,
    visits: Option<VisitTracker>,
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
            visits: None,
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

    /// The code breakpoints the replica has inserted (whether or not rr currently holds them).
    fn armed_code(&self) -> Vec<u64> {
        self.remote
            .breakpoints
            .iter()
            .filter(|b| matches!(b.kind, BreakKind::Software | BreakKind::Hardware))
            .map(|b| b.addr)
            .collect()
    }

    /// The clock of the current position as far as it is known without asking rr.
    fn known_clock(&self) -> Option<Clock> {
        self.visits.as_ref().map(|t| t.clock).or(self.clock)
    }

    /// Count the position the replica just moved forward to from a position with clock
    /// `before`: clock `after`, PC `pc`. `armed` are the code breakpoints that stopped the move
    /// at every earlier position with their PC (all of them for a single step or a walk, which
    /// has no earlier positions).
    fn observe(&mut self, before: Option<Clock>, after: Clock, pc: u64, armed: &[u64]) {
        match &mut self.visits {
            // The tracker describes the position the move started from.
            Some(t) if t.clock == after => t.arrive(pc),
            // A new region: every position of it so far was reached during this move.
            _ if before.is_some_and(|b| b != after) => {
                self.visits = Some(VisitTracker::entered(after, armed, pc))
            }
            // The same region, entered before anything was counted.
            _ => self.visits = None,
        }
    }

    fn after_vrun(&mut self, reply: &StopReply) {
        self.remote.forget_selection();
        self.remote.rebase_threads();
        self.visits = None;
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
    /// Returns the registers there.
    fn seek_clock(&mut self, target: Clock) -> Result<RegisterSet> {
        self.vrun(RrSeek::Event(target.frame_time))?;
        if self.clock()? != target {
            self.vrun(RrSeek::Ticks(target.ticks))?;
        }
        self.landed_at_region_start(target)
    }

    /// Like [`Self::seek_clock`], but when the replica is earlier in the same event and thread,
    /// seek by ticks alone, so that rr replays forward from here instead of from a checkpoint
    /// before the event (`ReplayTimeline::seek_to_before_key` keeps the current session when it
    /// is closer than any checkpoint). Both land on the same position: the first with these
    /// ticks after the event's start.
    fn seek_clock_forward(&mut self, target: Clock) -> Result<RegisterSet> {
        let here = if self.past_end {
            None
        } else {
            Some(self.clock()?)
        };
        match here {
            Some(c)
                if c.frame_time == target.frame_time
                    && c.tid == target.tid
                    && c.ticks < target.ticks =>
            {
                self.vrun(RrSeek::Ticks(target.ticks))?;
                self.landed_at_region_start(target)
            }
            _ => self.seek_clock(target),
        }
    }

    /// Check that a seek reached `target` and start counting visits there. Returns the
    /// registers there.
    fn landed_at_region_start(&mut self, target: Clock) -> Result<RegisterSet> {
        let got = self.clock()?;
        if got != target {
            return Err(HopperError::BadMoment(format!(
                "rr reached event {} thread {} ticks {}, not event {} thread {} ticks {}",
                got.frame_time, got.tid, got.ticks, target.frame_time, target.tid, target.ticks
            )));
        }
        let regs = self.remote.registers(target.tid)?;
        let pc = regs
            .pc()
            .ok_or_else(|| HopperError::Protocol("the register set has no rip".into()))?;
        let armed = self.armed_code();
        self.visits = Some(VisitTracker::entered(target, &armed, pc));
        Ok(regs)
    }

    fn pc(&mut self, tid: Tid) -> Result<u64> {
        self.remote
            .registers(tid)?
            .pc()
            .ok_or_else(|| HopperError::Protocol("the register set has no rip".into()))
    }

    /// Single-step `tid` forward `n` times, checking that the tick count stays put.
    fn step_within_ticks(&mut self, target: Clock, n: u32) -> Result<()> {
        for _ in 0..n {
            self.step_once_checked(target)?;
        }
        Ok(())
    }

    /// One forward step of `target.tid` that must stay in `target`'s region; counts the
    /// position it reaches and returns its registers.
    fn step_once_checked(&mut self, target: Clock) -> Result<RegisterSet> {
        let req = self
            .remote
            .request_for(Resume::Step { tid: target.tid }, Direction::Forward);
        let reply = self.resume_absorbing(req)?;
        if !matches!(reply, StopReply::Stopped(_)) {
            self.visits = None;
            return Err(HopperError::BadMoment(
                "the recording ended inside a seek".into(),
            ));
        }
        let now = self.clock()?;
        if now != target {
            self.visits = None;
            return Err(HopperError::BadMoment(format!(
                "a step left event {} ticks {} (now event {} ticks {})",
                target.frame_time, target.ticks, now.frame_time, now.ticks
            )));
        }
        let regs = self.remote.registers(target.tid)?;
        let pc = regs
            .pc()
            .ok_or_else(|| HopperError::Protocol("the register set has no rip".into()))?;
        let armed = self.armed_code();
        self.observe(Some(target), now, pc, &armed);
        Ok(regs)
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
        let mut regs = self.seek_clock(at)?;
        let mut k = 0u32;
        loop {
            if position_key(&regs) == want {
                return Ok(k);
            }
            if k >= self.sub_measure_budget {
                return Err(HopperError::BudgetExhausted {
                    steps: u64::from(k),
                });
            }
            regs = self.step_once_checked(at)?;
            k += 1;
        }
    }

    /// Walk `region` from its start and give each of `wanted` (indices into `visits`, all in
    /// this region) its `sub`. Leaves the replica at the last one, with `sub` known.
    fn resolve_region(
        &mut self,
        region: Clock,
        visits: &[RegionVisit],
        wanted: &[usize],
        out: &mut [Option<u32>],
    ) -> Result<()> {
        let landed = self.seek_clock_forward(region)?;
        let mut waiting: HashMap<(u64, u32), Vec<usize>> = HashMap::new();
        for &i in wanted {
            waiting
                .entry((visits[i].pc, visits[i].visit))
                .or_default()
                .push(i);
        }
        let mut seen: HashMap<u64, u32> = HashMap::new();
        let mut pc = landed
            .pc()
            .ok_or_else(|| HopperError::Protocol("the register set has no rip".into()))?;
        let mut k = 0u32;
        loop {
            let n = seen.entry(pc).or_insert(0);
            if let Some(found) = waiting.remove(&(pc, *n)) {
                for i in found {
                    out[i] = Some(k);
                }
            }
            *n += 1;
            if waiting.is_empty() {
                self.sub = SubPosition::Known(k);
                return Ok(());
            }
            if k >= self.sub_measure_budget {
                return Err(HopperError::BudgetExhausted {
                    steps: u64::from(k),
                });
            }
            let step = self.step_once_checked(region);
            pc = match step {
                Ok(regs) => regs
                    .pc()
                    .ok_or_else(|| HopperError::Protocol("the register set has no rip".into()))?,
                Err(HopperError::BadMoment(why)) => {
                    let (&(pc, visit), _) = waiting.iter().next().expect("not empty");
                    return Err(HopperError::BadMoment(format!(
                        "visit {visit} of {pc:#x} is not in event {} thread {} ticks {}: {why}",
                        region.frame_time, region.tid, region.ticks
                    )));
                }
                Err(e) => return Err(e),
            };
            k += 1;
        }
    }

    fn check_recording(&self, m: &Moment) -> Result<()> {
        if m.recording != self.info.recording {
            return Err(HopperError::BadMoment(format!(
                "moment {m} belongs to recording {}, not {}",
                m.recording, self.info.recording
            )));
        }
        Ok(())
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
                self.visits = None;
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
        self.remote.remove(bp)?;
        if let Some(t) = &mut self.visits
            && !self.remote.breakpoints.iter().any(|b| {
                b.addr == bp.addr && matches!(b.kind, BreakKind::Software | BreakKind::Hardware)
            })
        {
            t.counts.remove(&bp.addr);
        }
        Ok(())
    }

    fn resume(&mut self, how: Resume, dir: Direction) -> Result<StopEvent> {
        if self.past_end {
            return Err(HopperError::BadMoment(
                "the replay is past the end of the recording; seek first".into(),
            ));
        }
        let stepped = matches!(how, Resume::Step { .. });
        let before = if stepped { Some(self.clock()?) } else { None };
        let armed = self.armed_code();
        // Counting visits needs the clock the move starts from, unless a tracker has it.
        let start_clock = match (self.known_clock(), armed.is_empty()) {
            (Some(c), _) => Some(c),
            (None, false) if dir == Direction::Forward => Some(self.clock()?),
            (None, _) => None,
        };
        let req = self.remote.request_for(how, dir);
        let reply = self.resume_absorbing(req)?;
        let ev = self.map_stop(&reply, how, dir)?;
        let moved_forward = dir == Direction::Forward
            && !matches!(
                ev.reason,
                StopReason::HistoryEnd | StopReason::Exited { .. } | StopReason::Killed { .. }
            );
        if !moved_forward {
            self.visits = None;
            self.remote.rebase_threads();
        } else {
            // Visits matter only to breakpoints: without any, counting costs round trips for
            // nothing.
            if !armed.is_empty() || self.visits.is_some() {
                let clock = self.clock()?;
                let pc = match ev.reason {
                    StopReason::Breakpoint { addr } => addr,
                    _ => self.pc(ev.tid)?,
                };
                self.observe(start_clock, clock, pc, &armed);
            }
            self.remote.diff_threads()?;
        }
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
        self.check_recording(to)?;
        let target = Clock::of_moment(to);
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
                    .seek_clock_forward(target)
                    .and_then(|_| self.step_within_ticks(target, to.sub));
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

    fn now_at_stop(&mut self) -> Result<StopMoment> {
        if self.sub == SubPosition::Unmeasured && !self.past_end {
            let clock = self.clock()?;
            if let Some(t) = &self.visits
                && t.clock == clock
                && let Some((pc, visit)) = t.current_visit()
            {
                return Ok(StopMoment::Visit(RegionVisit {
                    region: clock.region_start(self.info.recording),
                    pc,
                    visit,
                }));
            }
        }
        self.now().map(StopMoment::Exact)
    }

    fn resolve_visits(&mut self, visits: &[RegionVisit]) -> Result<Vec<Moment>> {
        if visits.is_empty() {
            return Ok(Vec::new());
        }
        for v in visits {
            self.check_recording(&v.region)?;
        }
        // Where to come back to: the current position, itself perhaps a visit.
        let mut all = visits.to_vec();
        let back = if self.past_end {
            None
        } else {
            match self.now_at_stop()? {
                StopMoment::Exact(m) => Some(Ok(m)),
                StopMoment::Visit(v) => {
                    all.push(v);
                    Some(Err(all.len() - 1))
                }
            }
        };
        // The walks move the replica: its `sub` is known again only where they end.
        self.sub = SubPosition::Unmeasured;
        let mut order: Vec<usize> = (0..all.len()).collect();
        order.sort_by_key(|&i| (all[i].region_key(), all[i].region.tid.0));
        let mut subs: Vec<Option<u32>> = vec![None; all.len()];
        let walked = (|| -> Result<()> {
            let mut i = 0;
            while i < order.len() {
                let region = Clock::of_moment(&all[order[i]].region);
                let j = order[i..]
                    .iter()
                    .position(|&k| Clock::of_moment(&all[k].region) != region)
                    .map_or(order.len(), |n| i + n);
                self.resolve_region(region, &all, &order[i..j], &mut subs)?;
                i = j;
            }
            Ok(())
        })();
        let moment = |i: usize, sub: u32| Moment {
            sub,
            ..all[i].region
        };
        let back = match back {
            Some(Ok(m)) => Some(m),
            Some(Err(i)) => subs[i].map(|s| moment(i, s)),
            None => None,
        };
        // Go back (forward: the replica started at or after every visit), and put the
        // breakpoints back that the walks' seeks removed.
        let returned = match (&walked, back) {
            (Ok(()), Some(m)) => self.seek(&m).map(|_| ()),
            _ => self.remote.reinsert_all(),
        };
        walked?;
        returned?;
        Ok((0..visits.len())
            .map(|i| moment(i, subs[i].expect("every visit walked")))
            .collect())
    }

    fn enable_thread_events(&mut self) -> Result<ThreadEventSource> {
        self.remote.enable_thread_events()
    }

    fn take_thread_events(&mut self) -> Result<Vec<ThreadEvent>> {
        Ok(std::mem::take(&mut self.remote.thread_events))
    }

    fn scheduled_thread(&mut self) -> Result<Option<Tid>> {
        self.remote.client.rr_when_tid().map(Some)
    }
}
