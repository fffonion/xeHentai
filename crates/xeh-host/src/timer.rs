use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use vm::{
    TimerBackend, TimerCallbackError, TimerCallbackState, TimerRegistration, VmError, VmResult,
};

struct StoredTimer {
    due: Instant,
    registration: TimerRegistration,
}

/// Manual timer backend owned by one embedding session.
///
/// Registrations stay on this backend until `drive_due` or `shutdown`. There
/// is no process-global scheduler and no VM pool. `shutdown` is idempotent:
/// each pending callback runs once with `premature=true` and is then cancelled.
pub struct EmbeddingTimer {
    pending: Mutex<Vec<StoredTimer>>,
    running: AtomicUsize,
    shut: AtomicBool,
    errors: Mutex<Vec<String>>,
}

impl EmbeddingTimer {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            running: AtomicUsize::new(0),
            shut: AtomicBool::new(false),
            errors: Mutex::new(Vec::new()),
        }
    }

    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().expect("timer errors").clone()
    }

    /// Runs every registration whose delay has elapsed.
    ///
    /// A callback that does not finish synchronously is an error: this driver
    /// does not leave callback VMs parked across threads.
    pub fn drive_due(&self) -> VmResult<usize> {
        if self.shut.load(Ordering::SeqCst) {
            return Ok(0);
        }
        let mut driven = 0;
        loop {
            let mut pending = self.pending.lock().expect("timer pending");
            if self.shut.load(Ordering::SeqCst) {
                return Ok(driven);
            }
            let now = Instant::now();
            let Some(index) = pending.iter().position(|item| item.due <= now) else {
                return Ok(driven);
            };
            if self.running.load(Ordering::SeqCst) >= pending[index].registration.max_running {
                return Ok(driven);
            }
            let mut stored = pending.remove(index);
            drop(pending);
            self.running.fetch_add(1, Ordering::SeqCst);
            let state = stored.registration.callback.start(false);
            self.running.fetch_sub(1, Ordering::SeqCst);
            let state = state?;
            if !matches!(
                state,
                TimerCallbackState::Complete | TimerCallbackState::Cancelled
            ) {
                return Err(VmError::HostError(format!(
                    "timer callback did not finish synchronously: {state:?}"
                )));
            }
            driven += 1;
            if let Some(interval) = stored.registration.interval {
                let mut pending = self.pending.lock().expect("timer pending");
                pending.push(StoredTimer {
                    due: Instant::now() + interval,
                    registration: stored.registration,
                });
            }
        }
    }
}

impl Default for EmbeddingTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerBackend for EmbeddingTimer {
    fn register(&self, registration: TimerRegistration) -> VmResult<()> {
        if self.shut.load(Ordering::SeqCst) {
            return Err(VmError::HostError("timer runtime is shut down".to_string()));
        }
        let mut pending = self.pending.lock().expect("timer pending");
        if self.shut.load(Ordering::SeqCst) {
            return Err(VmError::HostError("timer runtime is shut down".to_string()));
        }
        if pending.len() >= registration.max_pending {
            return Err(VmError::HostError(format!(
                "timer pending limit {} reached",
                registration.max_pending
            )));
        }
        let due = Instant::now() + registration.delay;
        pending.push(StoredTimer { due, registration });
        Ok(())
    }

    fn pending_count(&self) -> usize {
        self.pending.lock().expect("timer pending").len()
    }

    fn running_count(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    fn report_callback_error(&self, error: TimerCallbackError) {
        self.errors
            .lock()
            .expect("timer errors")
            .push(format!("{error:?}"));
    }

    fn shutdown(&self) -> VmResult<()> {
        if self.shut.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let pending = std::mem::take(&mut *self.pending.lock().expect("timer pending"));
        for mut stored in pending {
            stored.registration.callback.start(true)?;
            stored.registration.callback.cancel()?;
        }
        Ok(())
    }
}
