//! Cancelling a running index job and watching its progress.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use hopper_core::api::IndexStatus;

/// Shared between the thread running an index job and whoever watches or cancels it. Cheap to
/// clone; every clone controls the same job.
#[derive(Clone, Debug, Default)]
pub struct IndexControl {
    inner: Arc<ControlInner>,
}

#[derive(Debug, Default)]
struct ControlInner {
    cancelled: AtomicBool,
    status: Mutex<JobStatus>,
}

#[derive(Debug, Default, Clone)]
struct JobStatus {
    phases: Vec<(String, f32)>,
    done: bool,
    error: Option<String>,
}

impl IndexControl {
    pub fn new() -> Self {
        IndexControl::default()
    }

    /// Ask the job to stop. It stops at its next stop or step, persists what it finished and
    /// returns [`hopper_core::HopperError::Cancelled`].
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// Set one phase's progress (0.0 to 1.0), adding the phase if it is new.
    pub fn set_progress(&self, phase: &str, fraction: f32) {
        let mut s = self.lock();
        let fraction = fraction.clamp(0.0, 1.0);
        match s.phases.iter_mut().find(|(p, _)| p == phase) {
            Some(slot) => slot.1 = fraction,
            None => s.phases.push((phase.to_string(), fraction)),
        }
    }

    /// Mark the job finished, with the error that ended it if any.
    pub fn finish(&self, error: Option<String>) {
        let mut s = self.lock();
        s.done = true;
        s.error = error;
    }

    /// The job's status in the engine API's vocabulary.
    pub fn status(&self) -> IndexStatus {
        let s = self.lock().clone();
        IndexStatus {
            phases: s.phases,
            done: s.done,
            error: s.error,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, JobStatus> {
        self.inner.status.lock().unwrap_or_else(|p| p.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_state() {
        let a = IndexControl::new();
        let b = a.clone();
        b.set_progress("calls", 0.5);
        b.set_progress("calls", 2.0);
        a.cancel();
        assert!(b.is_cancelled());
        let s = a.status();
        assert_eq!(s.phases, vec![("calls".to_string(), 1.0)]);
        assert!(!s.done);
        b.finish(Some("boom".into()));
        assert_eq!(a.status().error.as_deref(), Some("boom"));
    }
}
