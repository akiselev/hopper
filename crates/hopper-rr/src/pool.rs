//! A pool of replicas: one cursor replica that follows the user, plus background replicas the
//! indexer leases. Each replica is owned by its own worker thread and fed jobs
//! (`FnOnce(&mut dyn Target) -> R`) through a channel, because a replay connection is serial.
//! Replicas start lazily, on their first job, through a [`ReplicaFactory`]. A live gdbserver
//! gives a pool with no background replicas.

use std::ffi::OsString;
use std::fmt;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;

use hopper_core::target::Target;
use hopper_core::{HopperError, RecordingId, Result};

use crate::gdbserver::{GdbserverConfig, GdbserverTarget};
use crate::replica::{ReplayOptions, RrReplica};
use crate::rr::Rr;

/// Which replica a factory is asked for.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum ReplicaRole {
    Cursor,
    Background(usize),
}

/// Starts replicas for a pool.
pub trait ReplicaFactory: Send + Sync + 'static {
    fn start(&self, role: ReplicaRole) -> Result<Box<dyn Target>>;
}

/// Starts `rr replay` servers on one trace.
#[derive(Clone, Debug)]
pub struct RrReplicaFactory {
    pub rr: Rr,
    pub trace_dir: PathBuf,
    pub options: ReplayOptions,
    pub recording: RecordingId,
}

impl ReplicaFactory for RrReplicaFactory {
    fn start(&self, _role: ReplicaRole) -> Result<Box<dyn Target>> {
        Ok(Box::new(RrReplica::start(
            &self.rr,
            &self.trace_dir,
            &self.options,
            self.recording,
        )?))
    }
}

/// Launches a program under gdbserver. Only one live process exists, so use it with a pool of
/// no background replicas.
#[derive(Clone, Debug)]
pub struct GdbserverFactory {
    pub config: GdbserverConfig,
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl ReplicaFactory for GdbserverFactory {
    fn start(&self, role: ReplicaRole) -> Result<Box<dyn Target>> {
        if role != ReplicaRole::Cursor {
            return Err(HopperError::Unsupported(
                "a live process has only a cursor replica".into(),
            ));
        }
        Ok(Box::new(GdbserverTarget::launch(
            &self.config,
            &self.program,
            &self.args,
        )?))
    }
}

struct Slot<F: ReplicaFactory> {
    factory: Arc<F>,
    role: ReplicaRole,
    target: Option<Box<dyn Target>>,
}

impl<F: ReplicaFactory> Slot<F> {
    fn target(&mut self) -> Result<&mut dyn Target> {
        if self.target.is_none() {
            self.target = Some(self.factory.start(self.role)?);
        }
        Ok(self.target.as_deref_mut().expect("just started"))
    }
}

type Job<F> = Box<dyn FnOnce(&mut Slot<F>) + Send>;

struct Worker<F: ReplicaFactory> {
    tx: Option<mpsc::Sender<Job<F>>>,
    handle: Option<JoinHandle<()>>,
}

impl<F: ReplicaFactory> Worker<F> {
    fn spawn(factory: Arc<F>, role: ReplicaRole) -> Result<Worker<F>> {
        let (tx, rx) = mpsc::channel::<Job<F>>();
        let name = match role {
            ReplicaRole::Cursor => "hopper-replica-cursor".to_string(),
            ReplicaRole::Background(i) => format!("hopper-replica-{i}"),
        };
        let handle = std::thread::Builder::new()
            .name(name)
            .spawn(move || {
                let mut slot = Slot {
                    factory,
                    role,
                    target: None,
                };
                while let Ok(job) = rx.recv() {
                    job(&mut slot);
                }
            })
            .map_err(|e| HopperError::Io(format!("cannot start a replica worker: {e}")))?;
        Ok(Worker {
            tx: Some(tx),
            handle: Some(handle),
        })
    }

    fn submit<R, J>(&self, job: J) -> Result<Pending<R>>
    where
        R: Send + 'static,
        J: FnOnce(&mut dyn Target) -> R + Send + 'static,
    {
        let (rtx, rrx) = mpsc::sync_channel(1);
        let boxed: Job<F> = Box::new(move |slot: &mut Slot<F>| {
            let result = match slot.target() {
                Err(e) => Err(e),
                Ok(t) => match catch_unwind(AssertUnwindSafe(|| job(t))) {
                    Ok(r) => Ok(r),
                    Err(_) => {
                        // The replica's state is unknown after a panic: start a new one next.
                        slot.target = None;
                        Err(HopperError::Disconnected("a replica job panicked".into()))
                    }
                },
            };
            let _ = rtx.send(result);
        });
        self.tx
            .as_ref()
            .expect("sender lives until drop")
            .send(boxed)
            .map_err(|_| HopperError::Disconnected("the replica worker has stopped".into()))?;
        Ok(Pending { rx: rrx })
    }
}

impl<F: ReplicaFactory> Drop for Worker<F> {
    fn drop(&mut self) {
        self.tx = None;
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

/// A submitted job's result, to wait for later.
#[derive(Debug)]
pub struct Pending<R> {
    rx: mpsc::Receiver<Result<R>>,
}

impl<R> Pending<R> {
    pub fn wait(self) -> Result<R> {
        self.rx
            .recv()
            .map_err(|_| HopperError::Disconnected("the replica worker dropped the job".into()))?
    }

    /// The result if the job has finished, or the handle back.
    pub fn try_take(self) -> std::result::Result<Result<R>, Pending<R>> {
        match self.rx.try_recv() {
            Ok(r) => Ok(r),
            Err(mpsc::TryRecvError::Empty) => Err(self),
            Err(mpsc::TryRecvError::Disconnected) => Ok(Err(HopperError::Disconnected(
                "the replica worker dropped the job".into(),
            ))),
        }
    }
}

/// One cursor replica plus `N` background replicas.
pub struct ReplicaPool<F: ReplicaFactory> {
    cursor: Worker<F>,
    background: Vec<Worker<F>>,
    free: Arc<(Mutex<Vec<usize>>, Condvar)>,
}

impl<F: ReplicaFactory> fmt::Debug for ReplicaPool<F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReplicaPool")
            .field("background", &self.background.len())
            .finish()
    }
}

impl<F: ReplicaFactory> ReplicaPool<F> {
    /// Start the worker threads. No replica starts until its first job.
    pub fn new(factory: F, background: usize) -> Result<Self> {
        let factory = Arc::new(factory);
        let cursor = Worker::spawn(factory.clone(), ReplicaRole::Cursor)?;
        let background = (0..background)
            .map(|i| Worker::spawn(factory.clone(), ReplicaRole::Background(i)))
            .collect::<Result<Vec<_>>>()?;
        let free = (0..background.len()).rev().collect();
        Ok(ReplicaPool {
            cursor,
            background,
            free: Arc::new((Mutex::new(free), Condvar::new())),
        })
    }

    pub fn background_count(&self) -> usize {
        self.background.len()
    }

    /// Run `job` on the cursor replica and wait for it.
    pub fn cursor<R, J>(&self, job: J) -> Result<R>
    where
        R: Send + 'static,
        J: FnOnce(&mut dyn Target) -> R + Send + 'static,
    {
        self.cursor.submit(job)?.wait()
    }

    /// Queue `job` on the cursor replica without waiting.
    pub fn cursor_submit<R, J>(&self, job: J) -> Result<Pending<R>>
    where
        R: Send + 'static,
        J: FnOnce(&mut dyn Target) -> R + Send + 'static,
    {
        self.cursor.submit(job)
    }

    /// Lease a background replica, waiting until one is free. A pool with none refuses.
    pub fn lease(&self) -> Result<Lease<'_, F>> {
        if self.background.is_empty() {
            return Err(HopperError::Unsupported(
                "this pool has no background replicas".into(),
            ));
        }
        let (lock, cvar) = &*self.free;
        let mut free = lock
            .lock()
            .map_err(|_| HopperError::Disconnected("pool lock poisoned".into()))?;
        loop {
            if let Some(index) = free.pop() {
                return Ok(Lease { pool: self, index });
            }
            free = cvar
                .wait(free)
                .map_err(|_| HopperError::Disconnected("pool lock poisoned".into()))?;
        }
    }

    /// Lease a background replica if one is free now.
    pub fn try_lease(&self) -> Option<Lease<'_, F>> {
        let (lock, _) = &*self.free;
        let index = lock.lock().ok()?.pop()?;
        Some(Lease { pool: self, index })
    }

    fn release(&self, index: usize) {
        let (lock, cvar) = &*self.free;
        if let Ok(mut free) = lock.lock() {
            free.push(index);
            cvar.notify_one();
        }
    }
}

/// Exclusive use of one background replica until dropped.
pub struct Lease<'a, F: ReplicaFactory> {
    pool: &'a ReplicaPool<F>,
    index: usize,
}

impl<F: ReplicaFactory> fmt::Debug for Lease<'_, F> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Lease").field("index", &self.index).finish()
    }
}

impl<F: ReplicaFactory> Lease<'_, F> {
    pub fn index(&self) -> usize {
        self.index
    }

    /// Run `job` on the leased replica and wait for it.
    pub fn run<R, J>(&self, job: J) -> Result<R>
    where
        R: Send + 'static,
        J: FnOnce(&mut dyn Target) -> R + Send + 'static,
    {
        self.pool.background[self.index].submit(job)?.wait()
    }

    /// Queue `job` on the leased replica without waiting.
    pub fn submit<R, J>(&self, job: J) -> Result<Pending<R>>
    where
        R: Send + 'static,
        J: FnOnce(&mut dyn Target) -> R + Send + 'static,
    {
        self.pool.background[self.index].submit(job)
    }
}

impl<F: ReplicaFactory> Drop for Lease<'_, F> {
    fn drop(&mut self) {
        self.pool.release(self.index);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hopper_core::target::{Breakpoint, Direction, Resume, StopEvent, TargetInfo};
    use hopper_core::{CheckpointId, Moment, ProcessId, RegisterSet, Tid};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    /// A target that only knows which replica it is.
    #[derive(Debug)]
    struct Fake {
        info: TargetInfo,
        #[allow(dead_code, reason = "read through Debug by the tests")]
        role: ReplicaRole,
        calls: u32,
    }

    impl Target for Fake {
        fn info(&self) -> &TargetInfo {
            &self.info
        }
        fn threads(&mut self) -> Result<Vec<Tid>> {
            self.calls += 1;
            Ok(vec![Tid(1)])
        }
        fn current_thread(&mut self) -> Result<Tid> {
            Ok(Tid(1))
        }
        fn registers(&mut self, _: Tid) -> Result<RegisterSet> {
            Ok(RegisterSet::default())
        }
        fn read_memory(&mut self, addr: u64, _: &mut [u8]) -> Result<()> {
            Err(HopperError::UnreadableMemory { addr, len: 0 })
        }
        fn insert(&mut self, _: Breakpoint) -> Result<()> {
            Ok(())
        }
        fn remove(&mut self, _: Breakpoint) -> Result<()> {
            Ok(())
        }
        fn resume(&mut self, _: Resume, _: Direction) -> Result<StopEvent> {
            Err(HopperError::Unsupported("fake".into()))
        }
        fn now(&mut self) -> Result<Moment> {
            Err(HopperError::Unsupported("fake".into()))
        }
        fn seek(&mut self, _: &Moment) -> Result<StopEvent> {
            Err(HopperError::Unsupported("fake".into()))
        }
        fn checkpoint(&mut self) -> Result<CheckpointId> {
            Err(HopperError::Unsupported("fake".into()))
        }
        fn restore(&mut self, _: CheckpointId) -> Result<StopEvent> {
            Err(HopperError::Unsupported("fake".into()))
        }
        fn auxv(&mut self) -> Result<Vec<u8>> {
            Ok(vec![])
        }
    }

    #[derive(Default)]
    struct CountingFactory {
        starts: AtomicUsize,
        fail_first: bool,
    }

    impl ReplicaFactory for Arc<CountingFactory> {
        fn start(&self, role: ReplicaRole) -> Result<Box<dyn Target>> {
            let n = self.starts.fetch_add(1, Ordering::SeqCst);
            if self.fail_first && n == 0 {
                return Err(HopperError::Io("rr would not start".into()));
            }
            Ok(Box::new(Fake {
                info: TargetInfo {
                    recording: RecordingId([0; 16]),
                    process: ProcessId::new(1),
                    executable: PathBuf::from("/fake"),
                    can_reverse: false,
                    can_seek: false,
                    rr_version: None,
                },
                role,
                calls: 0,
            }))
        }
    }

    fn role_of(t: &mut dyn Target) -> String {
        format!("{t:?}")
    }

    #[test]
    fn starts_replicas_lazily_and_keeps_them() {
        let f = Arc::new(CountingFactory::default());
        let pool = ReplicaPool::new(f.clone(), 2).unwrap();
        assert_eq!(f.starts.load(Ordering::SeqCst), 0);
        let d = pool.cursor(role_of).unwrap();
        assert!(d.contains("Cursor"), "{d}");
        // The same replica serves the next job: its call counter carries over.
        pool.cursor(|t| t.threads().unwrap()).unwrap();
        let d = pool.cursor(role_of).unwrap();
        assert!(d.contains("calls: 1"), "{d}");
        assert_eq!(f.starts.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn leases_background_replicas_exclusively() {
        let f = Arc::new(CountingFactory::default());
        let pool = ReplicaPool::new(f.clone(), 2).unwrap();
        let a = pool.lease().unwrap();
        let b = pool.lease().unwrap();
        assert_ne!(a.index(), b.index());
        assert!(pool.try_lease().is_none());
        assert!(
            a.run(role_of)
                .unwrap()
                .contains(&format!("Background({})", a.index()))
        );
        let ai = a.index();
        drop(a);
        let c = pool.try_lease().unwrap();
        assert_eq!(c.index(), ai);
        drop(b);
        drop(c);
        // A blocked lease wakes when another is released.
        std::thread::scope(|s| {
            let l1 = pool.lease().unwrap();
            let l2 = pool.lease().unwrap();
            let waiter = s.spawn(|| pool.lease().map(|l| l.index()));
            std::thread::sleep(Duration::from_millis(50));
            let freed = l1.index();
            drop(l1);
            assert_eq!(waiter.join().unwrap().unwrap(), freed);
            drop(l2);
        });
    }

    #[test]
    fn a_pool_without_background_refuses_leases() {
        let pool = ReplicaPool::new(Arc::new(CountingFactory::default()), 0).unwrap();
        assert!(matches!(pool.lease(), Err(HopperError::Unsupported(_))));
        assert!(pool.try_lease().is_none());
        assert!(pool.cursor(role_of).is_ok());
    }

    #[test]
    fn reports_start_failures_and_retries() {
        let f = Arc::new(CountingFactory {
            fail_first: true,
            ..Default::default()
        });
        let pool = ReplicaPool::new(f.clone(), 0).unwrap();
        assert!(matches!(pool.cursor(role_of), Err(HopperError::Io(_))));
        assert!(pool.cursor(role_of).is_ok());
        assert_eq!(f.starts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_panicking_job_replaces_the_replica() {
        let f = Arc::new(CountingFactory::default());
        let pool = ReplicaPool::new(f.clone(), 0).unwrap();
        pool.cursor(role_of).unwrap();
        let r: Result<()> = pool.cursor(|_| panic!("boom"));
        assert!(matches!(r, Err(HopperError::Disconnected(_))));
        pool.cursor(role_of).unwrap();
        assert_eq!(f.starts.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn submitted_jobs_run_in_order() {
        let pool = ReplicaPool::new(Arc::new(CountingFactory::default()), 0).unwrap();
        let p1 = pool
            .cursor_submit(|t| t.threads().map(|v| v.len()))
            .unwrap();
        let p2 = pool.cursor_submit(|t| format!("{t:?}")).unwrap();
        assert_eq!(p1.wait().unwrap().unwrap(), 1);
        assert!(p2.wait().unwrap().contains("calls: 1"));
    }
}
