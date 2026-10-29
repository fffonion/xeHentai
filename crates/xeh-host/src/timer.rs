use std::sync::{Mutex, MutexGuard};
use std::task::{Context, Poll, Waker};
use std::thread::ThreadId;
use std::time::Instant;

use vm::{
    TimerBackend, TimerCallbackError, TimerCallbackState, TimerRegistration, VmError, VmResult,
};

use crate::bridge::EmbeddingBridge;

struct StoredTimer {
    due: Instant,
    registration: TimerRegistration,
}

struct Scheduler {
    pending: Vec<StoredTimer>,
    active: Vec<TimerRegistration>,
    /// Registrations temporarily removed so a callback can run without this lock.
    outside: usize,
    shut: bool,
    driving: bool,
    peak_running: usize,
}

impl Scheduler {
    fn running(&self) -> usize {
        self.active.len() + self.outside
    }

    fn note_peak(&mut self) {
        let running = self.running();
        if running > self.peak_running {
            self.peak_running = running;
        }
    }
}

/// Manual timer backend owned by the thread that created it.
///
/// `drive_due`, `drive_with`, and `shutdown` run only on that owner thread.
/// Another thread must hand the command to the owner; this API does not drive
/// or preempt callback bytecode from a second thread. A callback may register
/// another timer, but it cannot re-enter drive or shutdown.
///
/// Admission reserves a running slot in the same state transition that removes
/// a due registration. A `Waiting` or `Yielded` callback keeps that slot.
/// There is no process-global scheduler and no VM pool.
pub struct EmbeddingTimer {
    owner: ThreadId,
    state: Mutex<Scheduler>,
    errors: Mutex<Vec<String>>,
}

impl EmbeddingTimer {
    pub fn new() -> Self {
        Self {
            owner: std::thread::current().id(),
            state: Mutex::new(Scheduler {
                pending: Vec::new(),
                active: Vec::new(),
                outside: 0,
                shut: false,
                driving: false,
                peak_running: 0,
            }),
            errors: Mutex::new(Vec::new()),
        }
    }

    pub fn errors(&self) -> Vec<String> {
        self.errors.lock().expect("timer errors").clone()
    }

    pub fn peak_running(&self) -> usize {
        self.lock().peak_running
    }

    /// Drives due and waiting callbacks once, using a noop waker.
    ///
    /// Async callbacks need [`Self::drive_with`] so the caller's task waker can
    /// be registered with the callback bridge.
    pub fn drive_due(&self) -> VmResult<usize> {
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);
        self.drive_with(&mut cx)
    }

    /// Polls active callbacks with `cx`, then admits due callbacks up to the cap.
    ///
    /// Returns how many callbacks reached a terminal state. A callback that is
    /// still `Waiting` or `Yielded` stays running and is not an error.
    pub fn drive_with(&self, cx: &mut Context<'_>) -> VmResult<usize> {
        self.ensure_owner()?;
        {
            let mut state = self.lock();
            if state.driving {
                return Err(VmError::HostError(
                    "timer driver cannot re-enter on its owner thread".to_string(),
                ));
            }
            if state.shut {
                return Ok(0);
            }
            state.driving = true;
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.drive_body(cx)));
        self.lock().driving = false;
        match result {
            Ok(completed) => completed,
            Err(payload) => std::panic::resume_unwind(payload),
        }
    }

    fn drive_body(&self, cx: &mut Context<'_>) -> VmResult<usize> {
        let mut completed = 0;
        loop {
            let finished = self.poll_active(cx);
            completed += finished;
            let (admitted, just_finished) = self.admit_one()?;
            completed += just_finished;
            if !admitted && finished == 0 {
                break;
            }
        }
        Ok(completed)
    }

    fn poll_active(&self, cx: &mut Context<'_>) -> usize {
        let active = {
            let mut state = self.lock();
            if state.shut || state.active.is_empty() {
                return 0;
            }
            let active = std::mem::take(&mut state.active);
            state.outside += active.len();
            state.note_peak();
            active
        };
        let taken = active.len();
        let mut still = Vec::new();
        let mut finished = Vec::new();
        for mut registration in active {
            match poll_until_pending(self, &mut registration, cx) {
                Poll::Pending => still.push(registration),
                Poll::Ready(()) => finished.push(registration),
            }
        }
        let finished_count = finished.len();
        {
            let mut state = self.lock();
            state.outside = state.outside.saturating_sub(taken);
            if !state.shut {
                for registration in finished {
                    reschedule(&mut state, registration);
                }
            }
            state.active.extend(still);
            state.note_peak();
        }
        finished_count
    }

    fn admit_one(&self) -> VmResult<(bool, usize)> {
        let now = Instant::now();
        let mut registration = {
            let mut state = self.lock();
            if state.shut {
                return Ok((false, 0));
            }
            let Some(index) = state.pending.iter().position(|item| item.due <= now) else {
                return Ok((false, 0));
            };
            let cap = state.pending[index].registration.max_running;
            if state.running() >= cap {
                return Ok((false, 0));
            }
            let stored = state.pending.remove(index);
            state.outside += 1;
            state.note_peak();
            stored.registration
        };
        let live = self.start_round(&mut registration, false);
        let finished = usize::from(
            !live && !matches!(registration.callback.state(), TimerCallbackState::Cancelled),
        );
        {
            let mut state = self.lock();
            state.outside = state.outside.saturating_sub(1);
            if live {
                state.active.push(registration);
            } else if !state.shut {
                reschedule(&mut state, registration);
            }
            state.note_peak();
        }
        Ok((true, finished))
    }

    fn start_round(&self, registration: &mut TimerRegistration, premature: bool) -> bool {
        if let Err(error) = registration
            .callback
            .set_async_bridge(Box::new(EmbeddingBridge::default()))
        {
            self.report_callback_error(error.into());
            let _ = registration.callback.cancel();
            return false;
        }
        let _state = registration.callback.start_reporting(premature, self);
        registration.callback.is_live()
    }

    fn ensure_owner(&self) -> VmResult<()> {
        if std::thread::current().id() == self.owner {
            Ok(())
        } else {
            Err(VmError::HostError(
                "timer driver must run on its owner thread; hand drive and shutdown to that thread"
                    .to_string(),
            ))
        }
    }

    fn lock(&self) -> MutexGuard<'_, Scheduler> {
        self.state.lock().expect("timer scheduler")
    }
}

fn poll_until_pending(
    timer: &EmbeddingTimer,
    registration: &mut TimerRegistration,
    cx: &mut Context<'_>,
) -> Poll<()> {
    loop {
        match registration.callback.poll_reporting(cx, timer) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(_) if registration.callback.is_live() => continue,
            Poll::Ready(_) => return Poll::Ready(()),
        }
    }
}

fn reschedule(state: &mut Scheduler, registration: TimerRegistration) {
    if state.shut || matches!(registration.callback.state(), TimerCallbackState::Cancelled) {
        return;
    }
    let Some(interval) = registration.interval else {
        return;
    };
    state.pending.push(StoredTimer {
        due: Instant::now() + interval,
        registration,
    });
}

impl Default for EmbeddingTimer {
    fn default() -> Self {
        Self::new()
    }
}

impl TimerBackend for EmbeddingTimer {
    fn register(&self, registration: TimerRegistration) -> VmResult<()> {
        let mut state = self.lock();
        if state.shut {
            return Err(VmError::HostError("timer runtime is shut down".to_string()));
        }
        if state.pending.len() >= registration.max_pending {
            return Err(VmError::HostError(format!(
                "timer pending limit {} reached",
                registration.max_pending
            )));
        }
        let due = Instant::now() + registration.delay;
        state.pending.push(StoredTimer { due, registration });
        Ok(())
    }

    fn pending_count(&self) -> usize {
        self.lock().pending.len()
    }

    fn running_count(&self) -> usize {
        self.lock().running()
    }

    fn report_callback_error(&self, error: TimerCallbackError) {
        self.errors
            .lock()
            .expect("timer errors")
            .push(format!("{error:?}"));
    }

    fn shutdown(&self) -> VmResult<()> {
        self.ensure_owner()?;
        let (pending, active) = {
            let mut state = self.lock();
            if state.driving {
                return Err(VmError::HostError(
                    "timer driver cannot re-enter on its owner thread".to_string(),
                ));
            }
            if state.shut {
                return Ok(());
            }
            state.shut = true;
            state.driving = true;
            (
                std::mem::take(&mut state.pending),
                std::mem::take(&mut state.active),
            )
        };
        for mut stored in pending {
            let _live = self.start_round(&mut stored.registration, true);
            if let Err(error) = stored.registration.callback.cancel() {
                self.report_callback_error(error.into());
            }
        }
        for mut registration in active {
            if let Err(error) = registration.callback.cancel() {
                self.report_callback_error(error.into());
            }
        }
        let mut state = self.lock();
        state.pending.clear();
        state.active.clear();
        state.outside = 0;
        state.driving = false;
        Ok(())
    }
}
