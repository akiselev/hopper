//! What gdbserver and rr targets share: one RSP client plus the bookkeeping hopper needs around
//! it (the selected thread, the inserted breakpoints, the last stop) and the mapping from stop
//! replies to hopper-core's [`StopEvent`].

use hopper_core::target::{
    BreakKind, Breakpoint, Direction, Resume, StopEvent, StopReason, ThreadChange, ThreadEvent,
    ThreadEventSource, ThreadEventTime,
};
use hopper_core::{HopperError, Reg, RegisterSet, Result, Tid};
use hopper_rsp::{
    Connection, ResumeRequest, RspClient, StopCause, StopReply, ThreadId, ThreadOp, ThreadSelector,
    gdb_signal_to_linux,
};

/// The gdb signal number of SIGTRAP (the same as Linux's).
pub(crate) const GDB_SIGTRAP: u8 = 5;

/// How the connection observes thread starts and exits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ThreadWatch {
    Off,
    /// The server reports them (`QThreadEvents:1`); [`Remote::resume_raw`] absorbs the reports
    /// and collects them here until the target names their time.
    Reported {
        seen: Vec<(Tid, ThreadChange)>,
    },
    /// The thread list is compared at every forward stop with the one at the previous stop.
    /// `None`: re-read it at the next stop without reporting anything (after a seek).
    Diff {
        known: Option<Vec<Tid>>,
    },
}

/// Whether the server holds the breakpoints in [`Remote::breakpoints`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ServerBreakpoints {
    /// Every tracked breakpoint is inserted on the server.
    Held,
    /// The server dropped them all (rr does on every `vRun`); none is inserted until
    /// [`Remote::reinsert_all`].
    Dropped,
}

#[derive(Debug)]
pub(crate) struct Remote<C: Connection> {
    pub client: RspClient<C>,
    /// The process id, once a multiprocess thread id has named it.
    pub pid: Option<i32>,
    /// The thread the last stop reported.
    pub current: Option<ThreadId>,
    /// What `Hg` currently selects on the server, when known.
    selected: Option<ThreadId>,
    /// Breakpoints and watchpoints hopper has inserted, in insertion order.
    pub breakpoints: Vec<Breakpoint>,
    /// Whether the server holds `breakpoints` right now. Sending `z` for a breakpoint the
    /// server does not hold aborts rr (`ReplayTimeline::remove_breakpoint` asserts), and
    /// inserting one it already holds raises rr's reference count on it.
    server_breakpoints: ServerBreakpoints,
    /// Set once the process exited or was killed: the stop that ended it.
    pub ended: Option<StopEvent>,
    pub thread_watch: ThreadWatch,
    /// Thread events with their times, for [`hopper_core::target::Target::take_thread_events`].
    pub thread_events: Vec<ThreadEvent>,
}

impl<C: Connection> Remote<C> {
    pub fn new(client: RspClient<C>) -> Self {
        Remote {
            client,
            pid: None,
            current: None,
            selected: None,
            breakpoints: Vec::new(),
            server_breakpoints: ServerBreakpoints::Held,
            ended: None,
            thread_watch: ThreadWatch::Off,
            thread_events: Vec::new(),
        }
    }

    /// Ask the server for thread events; fall back to comparing thread lists when it does not
    /// know `QThreadEvents` (rr answers it with an empty reply).
    pub fn enable_thread_events(&mut self) -> Result<ThreadEventSource> {
        match &self.thread_watch {
            ThreadWatch::Reported { .. } => return Ok(ThreadEventSource::ServerReports),
            ThreadWatch::Diff { .. } => return Ok(ThreadEventSource::ThreadListDiff),
            ThreadWatch::Off => {}
        }
        self.check_alive()?;
        match self.client.enable_thread_events() {
            Ok(()) => {
                self.thread_watch = ThreadWatch::Reported { seen: Vec::new() };
                Ok(ThreadEventSource::ServerReports)
            }
            Err(HopperError::Unsupported(_)) => {
                let known = self.threads()?;
                self.thread_watch = ThreadWatch::Diff { known: Some(known) };
                Ok(ThreadEventSource::ThreadListDiff)
            }
            Err(e) => Err(e),
        }
    }

    /// The thread events the server reported during the last resume, without times.
    pub fn take_reported(&mut self) -> Vec<(Tid, ThreadChange)> {
        match &mut self.thread_watch {
            ThreadWatch::Reported { seen } => std::mem::take(seen),
            _ => Vec::new(),
        }
    }

    /// In diff mode, compare the thread list with the previous stop's and record the
    /// difference as events of the current stop.
    pub fn diff_threads(&mut self) -> Result<()> {
        if !matches!(self.thread_watch, ThreadWatch::Diff { .. }) || self.ended.is_some() {
            return Ok(());
        }
        let now = self.threads()?;
        if let ThreadWatch::Diff { known } = &mut self.thread_watch {
            if let Some(before) = known.as_ref() {
                let event = |tid: Tid, change| ThreadEvent {
                    tid,
                    change,
                    time: ThreadEventTime::ByCurrentStop,
                };
                for t in before.iter().filter(|t| !now.contains(t)) {
                    self.thread_events.push(event(*t, ThreadChange::Exited));
                }
                for t in now.iter().filter(|t| !before.contains(t)) {
                    self.thread_events.push(event(*t, ThreadChange::Started));
                }
            }
            *known = Some(now);
        }
        Ok(())
    }

    /// In diff mode, forget the previous thread list: the target moved without executing
    /// forward (a seek, a restore, reverse execution), so a difference is not an event.
    pub fn rebase_threads(&mut self) {
        if let ThreadWatch::Diff { known } = &mut self.thread_watch {
            *known = None;
        }
    }

    /// Forget what the server selects: it resets its threads on every resume and seek.
    pub fn forget_selection(&mut self) {
        self.selected = None;
    }

    pub fn check_alive(&self) -> Result<()> {
        match &self.ended {
            Some(e) => Err(HopperError::Disconnected(format!(
                "the process has ended: {:?}",
                e.reason
            ))),
            None => Ok(()),
        }
    }

    fn note_thread(&mut self, t: ThreadId) {
        if let Some(p) = t.pid {
            self.pid = Some(p);
        }
        self.current = Some(t);
    }

    /// The wire id for `tid`, with the pid when the server uses multiprocess ids.
    pub fn thread_id(&self, tid: Tid) -> ThreadId {
        ThreadId::new(self.pid, tid)
    }

    pub fn current_tid(&self) -> Result<Tid> {
        self.current.map(|t| t.tid).ok_or_else(|| {
            HopperError::Protocol("the server has not named a current thread".into())
        })
    }

    /// `?`, recording the thread it names. Returns the raw reply.
    pub fn initial_stop(&mut self) -> Result<StopReply> {
        let reply = self.client.stop_reason()?;
        match &reply {
            StopReply::Stopped(s) => {
                if let Some(t) = s.thread {
                    self.note_thread(t);
                } else {
                    let t = self.client.current_thread()?;
                    self.note_thread(t);
                }
            }
            StopReply::Exited { .. } | StopReply::Killed { .. } => {
                return Err(HopperError::Disconnected(
                    "the process is not running".into(),
                ));
            }
            StopReply::ThreadExited { .. } => {
                return Err(HopperError::Protocol(
                    "the server answered `?` with a thread exit".into(),
                ));
            }
        }
        Ok(reply)
    }

    pub fn threads(&mut self) -> Result<Vec<Tid>> {
        self.check_alive()?;
        let ids = if self.client.features().supports("qXfer:threads:read") {
            self.client
                .thread_info()?
                .into_iter()
                .map(|t| t.id)
                .collect()
        } else {
            self.client.thread_list()?
        };
        Ok(ids.into_iter().map(|t| t.tid).collect())
    }

    fn select(&mut self, tid: Tid) -> Result<()> {
        let id = self.thread_id(tid);
        if self.selected != Some(id) {
            self.client
                .select_thread(ThreadOp::General, ThreadSelector::Thread(id))?;
            self.selected = Some(id);
        }
        Ok(())
    }

    pub fn registers(&mut self, tid: Tid) -> Result<RegisterSet> {
        self.check_alive()?;
        self.select(tid)?;
        self.client.read_registers()
    }

    pub fn read_memory(&mut self, addr: u64, buf: &mut [u8]) -> Result<()> {
        self.check_alive()?;
        if let Some(t) = self.current {
            self.select(t.tid)?;
        }
        self.client.read_memory(addr, buf)
    }

    pub fn insert(&mut self, bp: Breakpoint) -> Result<()> {
        self.check_alive()?;
        if self.breakpoints.contains(&bp) {
            return Ok(());
        }
        if self.server_breakpoints == ServerBreakpoints::Held {
            self.client.insert_breakpoint(bp.kind, bp.addr, bp.len)?;
        }
        self.breakpoints.push(bp);
        Ok(())
    }

    pub fn remove(&mut self, bp: Breakpoint) -> Result<()> {
        self.check_alive()?;
        let Some(i) = self.breakpoints.iter().position(|b| *b == bp) else {
            return Err(HopperError::NotFound(format!(
                "no {:?} at {:#x}",
                bp.kind, bp.addr
            )));
        };
        if self.server_breakpoints == ServerBreakpoints::Held {
            self.client.remove_breakpoint(bp.kind, bp.addr, bp.len)?;
        }
        self.breakpoints.remove(i);
        Ok(())
    }

    /// Note that the server dropped every breakpoint (rr does on a `vRun`).
    pub fn breakpoints_dropped(&mut self) {
        self.server_breakpoints = ServerBreakpoints::Dropped;
    }

    /// Insert every tracked breakpoint again when the server dropped them; a no-op when it
    /// holds them.
    pub fn reinsert_all(&mut self) -> Result<()> {
        if self.server_breakpoints == ServerBreakpoints::Held {
            return Ok(());
        }
        let all = self.breakpoints.clone();
        for (i, bp) in all.iter().enumerate() {
            if let Err(e) = self.client.insert_breakpoint(bp.kind, bp.addr, bp.len) {
                // Take back the ones already inserted, so the server holds none again and a
                // later call inserts each exactly once.
                for done in &all[..i] {
                    let _ = self
                        .client
                        .remove_breakpoint(done.kind, done.addr, done.len);
                }
                return Err(e);
            }
        }
        self.server_breakpoints = ServerBreakpoints::Held;
        Ok(())
    }

    /// The resume packet for `how` in direction `dir`.
    pub fn request_for(&self, how: Resume, dir: Direction) -> ResumeRequest {
        match (how, dir) {
            (Resume::Continue, Direction::Forward) => ResumeRequest::Continue,
            (Resume::Continue, Direction::Reverse) => ResumeRequest::ReverseContinue,
            (Resume::Step { tid }, Direction::Forward) => ResumeRequest::Step(self.thread_id(tid)),
            (Resume::Step { tid }, Direction::Reverse) => {
                ResumeRequest::ReverseStep(self.thread_id(tid))
            }
        }
    }

    /// Send a resume as it is and wait for its stop reply. With server thread events on, the
    /// thread-created stops and thread-exit replies on the way are collected and the request is
    /// sent again, except when the thread a step was for exited: that reply is returned.
    pub fn resume_raw(&mut self, req: ResumeRequest) -> Result<StopReply> {
        self.check_alive()?;
        loop {
            self.forget_selection();
            let reply = self.client.resume(req)?;
            if let ThreadWatch::Reported { seen } = &mut self.thread_watch {
                match &reply {
                    StopReply::Stopped(s) if s.cause == StopCause::ThreadCreated => {
                        if let Some(t) = s.thread {
                            if let Some(p) = t.pid {
                                self.pid = Some(p);
                            }
                            seen.push((t.tid, ThreadChange::Started));
                        }
                        continue;
                    }
                    StopReply::ThreadExited { thread, .. } => {
                        seen.push((thread.tid, ThreadChange::Exited));
                        let stepped_it =
                            matches!(req, ResumeRequest::Step(t) if t.tid == thread.tid);
                        if !stepped_it {
                            continue;
                        }
                    }
                    _ => {}
                }
            }
            if let StopReply::Stopped(s) = &reply
                && let Some(t) = s.thread
            {
                self.note_thread(t);
            }
            return Ok(reply);
        }
    }

    /// Resume forward or backward. A forward resume of a thread that sits on one of hopper's
    /// code breakpoints first steps it over that breakpoint (remove, single-step, insert again),
    /// the way gdb drives a remote stub: gdbserver does not step over a client's breakpoint by
    /// itself and would report it again at once (with several threads it spins, re-trapping).
    pub fn resume(&mut self, req: ResumeRequest) -> Result<StopReply> {
        if let Some(reply) = self.step_over_breakpoint(req)? {
            return Ok(reply);
        }
        self.resume_raw(req)
    }

    /// When `req` resumes a thread from a code breakpoint, step it past: returns the step's
    /// reply when that already answers `req` (it was a step, or the step stopped for another
    /// reason), `None` when `req` should now be sent.
    fn step_over_breakpoint(&mut self, req: ResumeRequest) -> Result<Option<StopReply>> {
        let thread = match req {
            ResumeRequest::Continue => match self.current {
                Some(t) => t,
                None => return Ok(None),
            },
            ResumeRequest::Step(t) => t,
            ResumeRequest::ReverseContinue | ResumeRequest::ReverseStep(_) => return Ok(None),
        };
        // Nothing to step over when the server holds no breakpoints (in the middle of a seek).
        if self.server_breakpoints == ServerBreakpoints::Dropped {
            return Ok(None);
        }
        let has_code_breakpoints = self
            .breakpoints
            .iter()
            .any(|b| matches!(b.kind, BreakKind::Software | BreakKind::Hardware));
        if !has_code_breakpoints {
            return Ok(None);
        }
        let here: Vec<Breakpoint> = {
            let pc = self.registers(thread.tid)?.pc();
            self.breakpoints
                .iter()
                .filter(|b| {
                    matches!(b.kind, BreakKind::Software | BreakKind::Hardware)
                        && Some(b.addr) == pc
                })
                .copied()
                .collect()
        };
        if here.is_empty() {
            return Ok(None);
        }
        for bp in &here {
            self.client.remove_breakpoint(bp.kind, bp.addr, bp.len)?;
        }
        let reply = self.resume_raw(ResumeRequest::Step(thread))?;
        if matches!(reply, StopReply::Stopped(_)) {
            for bp in &here {
                self.client.insert_breakpoint(bp.kind, bp.addr, bp.len)?;
            }
        }
        let plain_step = matches!(
            &reply,
            StopReply::Stopped(s) if s.cause == StopCause::Unspecified && s.signal == GDB_SIGTRAP
        );
        Ok(match req {
            ResumeRequest::Continue if plain_step => None,
            _ => Some(reply),
        })
    }

    fn pc_of(&mut self, tid: Tid, stop: &hopper_rsp::SignalStop) -> Result<u64> {
        let tdesc = self.client.target_description()?;
        if let Some(rip) = tdesc.by_reg(Reg::RIP)
            && let Some((_, bytes)) = stop.registers.iter().find(|(n, _)| *n == rip.regnum)
            && bytes.len() == 8
        {
            return Ok(u64::from_le_bytes(
                bytes.as_slice().try_into().expect("8 bytes"),
            ));
        }
        self.registers(tid)?
            .pc()
            .ok_or_else(|| HopperError::Protocol("the register set has no rip".into()))
    }

    /// Map a stop reply to hopper's vocabulary. `stepped` says whether the request was a single
    /// step; a step that ends with SIGTRAP (or rr's signal 0) and no other reason is a
    /// [`StopReason::Step`].
    pub fn stop_event(&mut self, reply: &StopReply, stepped: bool) -> Result<StopEvent> {
        let ev = match reply {
            StopReply::Exited { code, .. } => {
                let tid = self
                    .current
                    .map(|t| t.tid)
                    .unwrap_or(Tid(self.pid.unwrap_or(0)));
                StopEvent {
                    tid,
                    reason: StopReason::Exited { code: *code },
                }
            }
            StopReply::Killed { signal, .. } => {
                let tid = self
                    .current
                    .map(|t| t.tid)
                    .unwrap_or(Tid(self.pid.unwrap_or(0)));
                let signo = gdb_signal_to_linux(*signal).unwrap_or(*signal);
                StopEvent {
                    tid,
                    reason: StopReason::Killed { signo },
                }
            }
            // Only a step of the thread that exited returns this (see `resume_raw`).
            StopReply::ThreadExited { status, thread } => StopEvent {
                tid: thread.tid,
                reason: StopReason::Other {
                    packet: format!("w{status:02x};{thread}"),
                },
            },
            StopReply::Stopped(s) => {
                let tid = match s.thread {
                    Some(t) => t.tid,
                    None => self.current_tid()?,
                };
                let reason = match &s.cause {
                    StopCause::Watchpoint { kind, addr } => {
                        // rr reports every data watchpoint as `watch:`; name the kind hopper
                        // inserted at that address.
                        let inserted = self.breakpoints.iter().find(|b| {
                            b.kind != BreakKind::Software
                                && b.kind != BreakKind::Hardware
                                && *addr >= b.addr
                                && *addr < b.addr + b.len.max(1)
                        });
                        StopReason::Watchpoint {
                            kind: inserted.map_or(*kind, |b| b.kind),
                            addr: *addr,
                        }
                    }
                    StopCause::SoftwareBreak | StopCause::HardwareBreak => StopReason::Breakpoint {
                        addr: self.pc_of(tid, s)?,
                    },
                    StopCause::HistoryBegin => StopReason::HistoryStart,
                    StopCause::HistoryEnd => StopReason::HistoryEnd,
                    // Absorbed by `resume_raw` when thread events are on; never sent otherwise.
                    StopCause::ThreadCreated => StopReason::Other {
                        packet: format!("T{:02x}create:", s.signal),
                    },
                    StopCause::Unspecified => {
                        if stepped && (s.signal == GDB_SIGTRAP || s.signal == 0) {
                            StopReason::Step
                        } else if s.signal == GDB_SIGTRAP {
                            let pc = self.pc_of(tid, s)?;
                            let at_breakpoint = self.breakpoints.iter().any(|b| {
                                matches!(b.kind, BreakKind::Software | BreakKind::Hardware)
                                    && b.addr == pc
                            });
                            if at_breakpoint {
                                StopReason::Breakpoint { addr: pc }
                            } else {
                                StopReason::Signal { signo: 5 }
                            }
                        } else {
                            match gdb_signal_to_linux(s.signal) {
                                Some(signo) => StopReason::Signal { signo },
                                None => StopReason::Other {
                                    packet: format!("T{:02x} (unknown gdb signal)", s.signal),
                                },
                            }
                        }
                    }
                };
                StopEvent { tid, reason }
            }
        };
        if matches!(
            ev.reason,
            StopReason::Exited { .. } | StopReason::Killed { .. }
        ) {
            self.ended = Some(ev.clone());
        }
        Ok(ev)
    }
}
