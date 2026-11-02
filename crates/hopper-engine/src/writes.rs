//! Phase B on demand: when `origin`, `last_write` or `value_history` asks about a moment the
//! write log does not cover, the engine logs a bounded window ending at that moment on a
//! background replica, so the next question about it answers from the index.
//!
//! The window starts at the entry of the innermost call enclosing the moment on its thread (from
//! the index's call tree when phase A has it, else found by reverse execution), and is bounded by
//! [`WriteLogPolicy::OnDemand::window_steps`] instructions. On a replay it is single-stepped
//! (`hopper_index::writes::log_writes_by_stepping`); on an rr recording it is one
//! `rr rerun --singlestep` shard over the window's events (`hopper_index::writes::rerun`).
//! The query that asked answers now by reverse execution, as without a log; the window runs
//! in the background and [`Debugger::index_status`](hopper_core::api::Debugger::index_status)
//! reports it as the phase `writes (on demand)`.

use std::sync::{Arc, Mutex};

use hopper_core::api::{CallFilter, Completeness, PageReq};
use hopper_core::target::Target;
use hopper_core::{HopperError, Moment, MomentRange, Place, Result};
use hopper_index::writes::rerun::{RerunJob, Shard, ShardStart, run_rerun_shards};
use hopper_index::writes::{StepWindow, WindowEnd, log_writes_by_stepping};

use crate::indexed::Indexed;
use crate::queries::{FrameKey, Spend, reverse_to_entry, to_start};
use crate::view::MomentView;
use crate::{Mode, Shared, lock};

/// The phase name on-demand windows report under in [`hopper_core::api::IndexStatus`].
pub const PHASE: &str = "writes (on demand)";

/// Whether queries start phase-B windows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WriteLogPolicy {
    /// Log a window ending at a queried moment the log does not cover, while fewer than
    /// `max_windows` have been started in this session.
    OnDemand {
        /// The most instructions one window steps (a single-stepped window costs about a
        /// thousand times their run time).
        window_steps: u64,
        max_windows: usize,
    },
    /// Never start phase B; queries reverse-execute.
    Off,
}

impl Default for WriteLogPolicy {
    fn default() -> Self {
        WriteLogPolicy::OnDemand {
            window_steps: 200_000,
            max_windows: 16,
        }
    }
}

/// One on-demand window.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WriteWindow {
    /// The queried moment the window ends at.
    pub end: Moment,
    /// Where it started, once known.
    pub start: Option<Moment>,
    pub state: WindowState,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WindowState {
    Running,
    /// Logged: `writes` writes over `instructions` instructions.
    Logged {
        instructions: u64,
        writes: u64,
    },
    Failed {
        reason: String,
    },
}

/// The session's on-demand windows.
#[derive(Debug, Default)]
pub struct WriteWindows {
    windows: Mutex<Vec<WriteWindow>>,
}

impl WriteWindows {
    pub fn windows(&self) -> Vec<WriteWindow> {
        lock(&self.windows).clone()
    }

    /// The `writes (on demand)` phase: the fraction of started windows that finished, and the
    /// last failure. `None` before any window started.
    pub fn status(&self) -> Option<((String, f32), bool, Option<String>)> {
        let ws = lock(&self.windows);
        if ws.is_empty() {
            return None;
        }
        let finished = ws
            .iter()
            .filter(|w| w.state != WindowState::Running)
            .count();
        let error = ws.iter().rev().find_map(|w| match &w.state {
            WindowState::Failed { reason } => Some(reason.clone()),
            _ => None,
        });
        Some((
            (PHASE.to_string(), finished as f32 / ws.len() as f32),
            finished == ws.len(),
            error,
        ))
    }

    /// Claim a window ending at `end`, unless one already covers it or the policy's budget is
    /// spent. A window that ended at `end` or contains it covers it.
    fn claim(&self, end: Moment, max_windows: usize) -> bool {
        let mut ws = lock(&self.windows);
        let covered = ws.iter().any(|w| {
            w.end == end
                || (w.state != WindowState::Running
                    && w.start
                        .is_some_and(|s| MomentRange::between(s, w.end).contains(&end)))
        });
        if covered || ws.len() >= max_windows {
            return false;
        }
        ws.push(WriteWindow {
            end,
            start: None,
            state: WindowState::Running,
        });
        true
    }

    fn update(&self, end: Moment, f: impl FnOnce(&mut WriteWindow)) {
        if let Some(w) = lock(&self.windows).iter_mut().find(|w| w.end == end) {
            f(w);
        }
    }
}

/// Whether the write log answers for `place` up to `before`.
fn covered(ix: &Indexed, place: Place, before: Moment) -> bool {
    ix.index
        .last_write(place, before)
        .is_ok_and(|a| a.completeness == Completeness::Complete)
}

/// Start a window ending at `before` when the write log does not cover `place` there and the
/// policy allows one. Returns at once; the window runs on its own thread.
pub(crate) fn request(shared: &Arc<Shared>, place: Place, before: Moment) {
    let WriteLogPolicy::OnDemand {
        window_steps,
        max_windows,
    } = shared.options.write_log
    else {
        return;
    };
    let Ok(ix) = &shared.index else { return };
    if shared.is_live() || covered(ix, place, before) {
        return;
    }
    if !shared.write_windows.claim(before, max_windows) {
        return;
    }
    let s = shared.clone();
    let ix = ix.clone();
    let spawned = std::thread::Builder::new()
        .name("hopper-index-phase-b".into())
        .spawn(move || {
            let result = run_window(&s, &ix, before, window_steps);
            s.write_windows.update(before, |w| {
                w.state = match result {
                    Ok((start, instructions, writes)) => {
                        w.start = Some(start);
                        WindowState::Logged {
                            instructions,
                            writes,
                        }
                    }
                    Err(e) => WindowState::Failed {
                        reason: e.to_string(),
                    },
                };
            });
        });
    if let Err(e) = spawned {
        shared.write_windows.update(before, |w| {
            w.state = WindowState::Failed {
                reason: format!("cannot start a phase-B thread: {e}"),
            }
        });
    }
}

/// The entry of the innermost call enclosing `at` on its thread, from the index's call tree
/// when phase A finished it.
fn enclosing_call_entry(ix: &Indexed, at: Moment) -> Option<Moment> {
    let filter = CallFilter {
        tid: Some(at.tid),
        function: Some(String::new()),
        ..CallFilter::default()
    };
    let page = ix
        .index
        .calls(
            &filter,
            PageReq {
                offset: 0,
                limit: 100_000,
            },
        )
        .ok()?;
    if page.completeness != Completeness::Complete {
        return None;
    }
    page.items
        .into_iter()
        .filter(|c| c.entry.is_before(&at) && c.exit.is_none_or(|x| at.is_before(&x)))
        .max_by_key(|c| c.entry.order_key())
        .map(|c| c.entry)
}

/// Log the window ending at `end`: (its start, instructions, writes).
fn run_window(
    shared: &Arc<Shared>,
    ix: &Indexed,
    end: Moment,
    window_steps: u64,
) -> Result<(Moment, u64, u64)> {
    let from_tree = enclosing_call_entry(ix, end);
    match &shared.mode {
        Mode::Recording { trace_dir, rr } => {
            // rerun shards are event ranges: from the call's event (or the window's own) to
            // the event after the queried moment.
            let start = from_tree.unwrap_or(end);
            let main = shared.main_symbols.clone().ok_or_else(|| {
                HopperError::Unsupported("phase B needs the main executable's symbols".into())
            })?;
            let job = RerunJob {
                rr: rr.clone(),
                trace_dir: trace_dir.clone(),
                recording: shared.recording,
                process: shared.process,
                shards: vec![Shard {
                    start_event: start.frame_time,
                    end_event: end.frame_time + 1,
                }],
                start: ShardStart::Replay,
                flush_every: 4096,
            };
            let summary = run_rerun_shards(&job, &main, &ix.index, &ix.control)?;
            Ok((start, summary.instructions, summary.writes))
        }
        Mode::Replay { .. } => {
            let lease = shared.pool.lease()?;
            let s = shared.clone();
            let index = ix.index.clone();
            let control = ix.control.clone();
            lease.run(move |t: &mut dyn Target| -> Result<(Moment, u64, u64)> {
                let start = match from_tree {
                    Some(m) => m,
                    None => frame_entry(&s, t, end)?,
                };
                let window = StepWindow {
                    tid: Some(end.tid),
                    start: Some(start),
                    max_steps: window_steps,
                    ..StepWindow::new(WindowEnd::Moment(end))
                };
                let summary = log_writes_by_stepping(t, &window, &index, &control)?;
                Ok((start, summary.instructions, summary.writes))
            })?
        }
        Mode::Live { .. } => Err(HopperError::Unsupported(
            "a live process cannot be stepped again".into(),
        )),
    }
}

/// The entry of the innermost frame at `at` by reverse execution, or the start of history.
fn frame_entry(shared: &Arc<Shared>, t: &mut dyn Target, at: Moment) -> Result<Moment> {
    if t.now().ok() != Some(at) {
        t.seek(&at)?;
    }
    let key = {
        let mut v = MomentView::on(shared, at, &mut *t);
        let stack = v.stack(at.tid)?;
        let syms = v.symbols()?;
        stack.first().and_then(|f| FrameKey::of(&syms.set, f))
    };
    let mut spend = Spend::new(shared.options.query_budget);
    match key {
        Some(k) if reverse_to_entry(t, &k, &mut spend)? => t.now(),
        _ => {
            to_start(shared, t)?;
            t.now()
        }
    }
}
