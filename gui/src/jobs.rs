//! Background jobs: every `Debugger` call runs off the UI thread, and its result comes back to
//! the UI once per frame (cadatomic's job pattern, `cadatomic-core/src/jobs.rs`, cut down).
//!
//! Two lanes, each one std thread fed by a channel:
//! - [`Lane::Replay`] is serial, like the replay it drives: navigation, state at a moment and
//!   omniscient queries run there in submission order.
//! - [`Lane::Status`] answers cheap status questions (`index_status`) without waiting behind a
//!   long navigation.
//!
//! A job is a closure over the context plus a completion that runs on the UI thread with the
//! result ([`Jobs::drain`]). [`Jobs::is_idle`] is the real idle barrier tests wait on: no job is
//! queued, running or finished-but-undrained. The waker asks the platform for a frame when a
//! result lands.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

type Work<C> = Box<dyn FnOnce(&C) -> Box<dyn std::any::Any + Send> + Send>;
type Completion<S> = Box<dyn FnOnce(&mut S, Box<dyn std::any::Any + Send>)>;
type Waker = Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>>;

/// Which worker runs a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Lane {
    Replay,
    Status,
}

struct Job<C: ?Sized> {
    id: u64,
    /// Wait this long before running (a status poll's interval).
    delay: Option<Duration>,
    work: Work<C>,
}

/// The job runner over a shared context `C` (the `dyn Debugger`), completing into UI state `S`.
pub struct Jobs<C: ?Sized + Send + Sync + 'static, S> {
    lanes: HashMap<Lane, Sender<Job<C>>>,
    done: Receiver<(u64, Box<dyn std::any::Any + Send>)>,
    completions: HashMap<u64, Completion<S>>,
    next: u64,
    /// Jobs submitted and not yet drained.
    outstanding: Arc<AtomicUsize>,
    /// Results sent by a worker and not yet drained.
    landed: Arc<AtomicUsize>,
    waker: Waker,
    threads: Vec<JoinHandle<()>>,
}

impl<C: ?Sized + Send + Sync + 'static, S> std::fmt::Debug for Jobs<C, S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Jobs").field("outstanding", &self.outstanding()).finish_non_exhaustive()
    }
}

impl<C: ?Sized + Send + Sync + 'static, S> Jobs<C, S> {
    pub fn new(ctx: Arc<C>) -> Self {
        let (done_tx, done) = channel();
        let waker: Waker = Arc::new(Mutex::new(None));
        let landed = Arc::new(AtomicUsize::new(0));
        let mut lanes = HashMap::new();
        let mut threads = Vec::new();
        for lane in [Lane::Replay, Lane::Status] {
            let (tx, rx) = channel::<Job<C>>();
            let ctx = ctx.clone();
            let done_tx: Sender<(u64, Box<dyn std::any::Any + Send>)> = done_tx.clone();
            let waker = waker.clone();
            let landed = landed.clone();
            let name = format!("hopper-gui-{lane:?}").to_lowercase();
            let t = std::thread::Builder::new()
                .name(name)
                .spawn(move || {
                    for job in rx {
                        if let Some(d) = job.delay {
                            std::thread::sleep(d);
                        }
                        let out = (job.work)(&ctx);
                        landed.fetch_add(1, Ordering::SeqCst);
                        if done_tx.send((job.id, out)).is_err() {
                            break;
                        }
                        let w = waker.lock().ok().and_then(|w| w.clone());
                        if let Some(w) = w {
                            w();
                        }
                    }
                })
                .expect("a job thread starts");
            lanes.insert(lane, tx);
            threads.push(t);
        }
        Jobs { lanes, done, completions: HashMap::new(), next: 0, outstanding: Arc::new(AtomicUsize::new(0)), landed, waker, threads }
    }

    /// Called (from a job thread) whenever a result lands: the platform's waker.
    pub fn set_waker(&self, w: Arc<dyn Fn() + Send + Sync>) {
        if let Ok(mut slot) = self.waker.lock() {
            *slot = Some(w);
        }
    }

    /// Runs `work` on `lane`; `done` gets its result on the UI thread at the next [`Jobs::drain`].
    pub fn submit<T: Send + 'static>(
        &mut self,
        lane: Lane,
        work: impl FnOnce(&C) -> T + Send + 'static,
        done: impl FnOnce(&mut S, T) + 'static,
    ) {
        self.submit_after(lane, None, work, done);
    }

    /// [`Jobs::submit`], run after `delay` on its lane (status polls).
    pub fn submit_after<T: Send + 'static>(
        &mut self,
        lane: Lane,
        delay: Option<Duration>,
        work: impl FnOnce(&C) -> T + Send + 'static,
        done: impl FnOnce(&mut S, T) + 'static,
    ) {
        self.next += 1;
        let id = self.next;
        self.completions.insert(
            id,
            Box::new(move |s, out| {
                if let Ok(t) = out.downcast::<T>() {
                    done(s, *t);
                }
            }),
        );
        self.outstanding.fetch_add(1, Ordering::SeqCst);
        let job = Job { id, delay, work: Box::new(move |c| Box::new(work(c)) as Box<dyn std::any::Any + Send>) };
        if self.lanes[&lane].send(job).is_err() {
            self.completions.remove(&id);
            self.outstanding.fetch_sub(1, Ordering::SeqCst);
        }
    }

    /// The completions of every job that finished, ready to run against the UI state.
    pub fn drain(&mut self) -> Vec<impl FnOnce(&mut S) + use<C, S>> {
        let mut out = Vec::new();
        while let Ok((id, result)) = self.done.try_recv() {
            self.outstanding.fetch_sub(1, Ordering::SeqCst);
            self.landed.fetch_sub(1, Ordering::SeqCst);
            if let Some(c) = self.completions.remove(&id) {
                out.push(move |s: &mut S| c(s, result));
            }
        }
        out
    }

    /// Results are waiting for [`Jobs::drain`].
    pub fn has_results(&self) -> bool {
        self.landed.load(Ordering::SeqCst) > 0
    }

    /// Jobs submitted and not yet drained.
    pub fn outstanding(&self) -> usize {
        self.outstanding.load(Ordering::SeqCst)
    }

    /// Nothing queued, running or waiting to be drained.
    pub fn is_idle(&self) -> bool {
        self.outstanding() == 0
    }

    /// Blocks until a result is waiting for [`Jobs::drain`] or `timeout` passes, without draining
    /// (tests and the headless shot drive frames between results; the UI never waits).
    pub fn wait_landed(&self, timeout: Duration) -> bool {
        let end = std::time::Instant::now() + timeout;
        while !self.has_results() {
            if self.is_idle() || std::time::Instant::now() >= end {
                return self.has_results();
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        true
    }

    /// Blocks until a result lands or `timeout` passes (tests and the headless shot only; the UI
    /// never waits). Returns whether one landed.
    pub fn wait_result(&mut self, timeout: Duration) -> Option<Vec<impl FnOnce(&mut S) + use<C, S>>> {
        let (id, result) = self.done.recv_timeout(timeout).ok()?;
        self.outstanding.fetch_sub(1, Ordering::SeqCst);
        self.landed.fetch_sub(1, Ordering::SeqCst);
        let mut out = Vec::new();
        if let Some(c) = self.completions.remove(&id) {
            out.push(move |s: &mut S| c(s, result));
        }
        Some(out)
    }
}

impl<C: ?Sized + Send + Sync + 'static, S> Drop for Jobs<C, S> {
    fn drop(&mut self) {
        self.lanes.clear();
        // the threads end when their channel closes; a running job finishes first
        for t in self.threads.drain(..) {
            let _ = t.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn results_run_on_the_caller_in_submission_order_per_lane() {
        let mut jobs: Jobs<u32, Vec<u32>> = Jobs::new(Arc::new(7));
        for i in 0..5u32 {
            jobs.submit(Lane::Replay, move |c| c + i, |s, v| s.push(v));
        }
        assert!(!jobs.is_idle());
        let mut seen = Vec::new();
        while seen.len() < 5 {
            for c in jobs.wait_result(Duration::from_secs(5)).expect("a result") {
                c(&mut seen);
            }
        }
        assert_eq!(seen, vec![7, 8, 9, 10, 11]);
        assert!(jobs.is_idle());
    }

    #[test]
    fn the_status_lane_does_not_wait_behind_the_replay_lane() {
        let (tx, rx) = channel::<()>();
        let rx = Mutex::new(rx);
        let mut jobs: Jobs<Mutex<std::sync::mpsc::Receiver<()>>, Vec<&'static str>> = Jobs::new(Arc::new(rx));
        // the replay job blocks until the test releases it
        jobs.submit(Lane::Replay, |r| r.lock().unwrap().recv().unwrap(), |s, ()| s.push("replay"));
        jobs.submit(Lane::Status, |_| (), |s, ()| s.push("status"));
        let mut seen = Vec::new();
        for c in jobs.wait_result(Duration::from_secs(5)).expect("status answers first") {
            c(&mut seen);
        }
        assert_eq!(seen, ["status"]);
        tx.send(()).unwrap();
        for c in jobs.wait_result(Duration::from_secs(5)).expect("then replay") {
            c(&mut seen);
        }
        assert_eq!(seen, ["status", "replay"]);
    }
}
