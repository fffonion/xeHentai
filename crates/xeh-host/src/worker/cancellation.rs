use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::dto::{
    lock, CommitGate, JobKey, NativeSubmission, PoolError, PoolToken, TaskRegistration,
};

const EPOCH_CAP: usize = 64;
const COMPLETIONS_PER_TASK: usize = 4;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Queued,
    Running,
    Committing,
    TerminalPending,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    Open,
    Cancelled,
    Committed,
    Failed,
}

pub(crate) struct JobSlot {
    pub token: CancellationToken,
    pub cancelled: bool,
    pub phase: Phase,
    pub outcome: Outcome,
    pub identity: Arc<crate::dto::RegistrationIdentity>,
}

struct TaskLease {
    epoch: i64,
    identity: Arc<crate::dto::RegistrationIdentity>,
}

struct EpochState {
    completions: HashMap<(i64, i64), u64>,
    inflight: HashMap<i64, usize>,
    order: VecDeque<i64>,
}

struct State {
    jobs: HashMap<JobKey, JobSlot>,
    tasks: HashMap<i64, TaskLease>,
    epochs: EpochState,
}

/// One admission lock for registration, queue records, cancel, epoch advance,
/// and the final hard link. Network waits must not hold this lock.
pub struct Admission {
    pool: Arc<PoolToken>,
    inner: Mutex<State>,
}

impl Admission {
    pub fn new() -> Self {
        Self {
            pool: Arc::new(PoolToken),
            inner: Mutex::new(State {
                jobs: HashMap::new(),
                tasks: HashMap::new(),
                epochs: EpochState {
                    completions: HashMap::new(),
                    inflight: HashMap::new(),
                    order: VecDeque::new(),
                },
            }),
        }
    }

    pub fn tracked(&self) -> usize {
        lock(&self.inner).jobs.len()
    }

    pub fn state_entries(&self) -> usize {
        let state = lock(&self.inner);
        state.jobs.len()
            + state.tasks.len()
            + state.epochs.completions.len()
            + state.epochs.inflight.len()
            + state.epochs.order.len()
    }

    /// Initial lease only. A later epoch uses [`Self::advance`]. Duplicate
    /// registration does not replace the lease or cancel jobs.
    pub fn register(&self, task_id: i64, epoch: i64) -> Result<TaskRegistration, PoolError> {
        let mut state = lock(&self.inner);
        if state.tasks.contains_key(&task_id) {
            return Err(PoolError::new("task_exists"));
        }
        state.make_room()?;
        let registration = TaskRegistration::mint(Arc::clone(&self.pool), task_id, epoch);
        state.tasks.insert(
            task_id,
            TaskLease {
                epoch,
                identity: Arc::clone(registration.identity()),
            },
        );
        state.epochs.touch(task_id);
        Ok(registration)
    }

    /// Admit the envelope captured at bind time. Does not read the current lease
    /// to authorize a request that arrived without that identity.
    pub fn insert_queued(
        &self,
        submission: &NativeSubmission,
        parent: &CancellationToken,
    ) -> Result<(), PoolError> {
        let mut state = lock(&self.inner);
        self.authorize(&state, submission)?;
        if state.jobs.contains_key(submission.key()) {
            return Err(PoolError::new("duplicate"));
        }
        let token = parent.child_token();
        let key = submission.key().clone();
        state.jobs.insert(
            key,
            JobSlot {
                token,
                cancelled: false,
                phase: Phase::Queued,
                outcome: Outcome::Open,
                identity: Arc::clone(submission.identity()),
            },
        );
        Ok(())
    }

    pub fn rollback(&self, submission: &NativeSubmission) {
        let mut state = lock(&self.inner);
        let matches = state.jobs.get(submission.key()).is_some_and(|slot| {
            slot.phase == Phase::Queued && Arc::ptr_eq(&slot.identity, submission.identity())
        });
        if matches {
            state.jobs.remove(submission.key());
        }
    }

    /// Queued and Running confirm a cancel. Committing is too late. A linked
    /// terminal is `already_complete`; a failed terminal is `already_failed`.
    /// A cancel that already won stays `Ok`. The lease must own the slot.
    pub fn cancel(&self, registration: &TaskRegistration, key: &JobKey) -> Result<(), PoolError> {
        if !Arc::ptr_eq(registration.pool(), &self.pool) {
            return Err(PoolError::new("wrong_pool"));
        }
        if key.task_id != registration.task_id() {
            return Err(PoolError::new("stale_registration"));
        }
        let mut state = lock(&self.inner);
        let Some(slot) = state.jobs.get_mut(key) else {
            return Err(PoolError::new("unknown"));
        };
        if !Arc::ptr_eq(&slot.identity, registration.identity()) {
            return Err(PoolError::new("stale_registration"));
        }
        match slot.phase {
            Phase::Committing => Err(PoolError::new("committing")),
            Phase::TerminalPending => match slot.outcome {
                Outcome::Committed => Err(PoolError::new("already_complete")),
                Outcome::Cancelled => Ok(()),
                Outcome::Failed | Outcome::Open => Err(PoolError::new("already_failed")),
            },
            Phase::Queued | Phase::Running => {
                if slot.cancelled || slot.outcome == Outcome::Cancelled {
                    return Ok(());
                }
                slot.cancelled = true;
                slot.outcome = Outcome::Cancelled;
                slot.token.cancel();
                Ok(())
            }
        }
    }

    pub fn advance(&self, registration: &TaskRegistration, epoch: i64) -> Result<(), PoolError> {
        if !Arc::ptr_eq(registration.pool(), &self.pool) {
            return Err(PoolError::new("wrong_pool"));
        }
        let mut state = lock(&self.inner);
        let current = {
            let Some(task) = state.tasks.get(&registration.task_id()) else {
                return Err(PoolError::new("unregistered"));
            };
            if !Arc::ptr_eq(&task.identity, registration.identity()) {
                return Err(PoolError::new("stale_registration"));
            }
            task.epoch
        };
        if epoch < current {
            return Err(PoolError::new("regression"));
        }
        if committing(&state, registration.task_id()) {
            return Err(PoolError::new("commit_pending"));
        }
        if epoch == current {
            return Ok(());
        }
        if let Some(stored) = state.tasks.get_mut(&registration.task_id()) {
            if Arc::ptr_eq(&stored.identity, registration.identity()) {
                stored.epoch = epoch;
            }
        }
        registration.set_epoch(epoch);
        state.epochs.touch(registration.task_id());
        state.bound_completions(registration.task_id());
        for (key, slot) in &mut state.jobs {
            if key.task_id == registration.task_id()
                && Arc::ptr_eq(&slot.identity, registration.identity())
                && key.epoch != epoch
                && slot.phase != Phase::Committing
                && slot.phase != Phase::TerminalPending
            {
                slot.cancelled = true;
                slot.outcome = Outcome::Cancelled;
                slot.token.cancel();
            }
        }
        Ok(())
    }

    pub fn begin(&self, task_id: i64) {
        let mut state = lock(&self.inner);
        *state.epochs.inflight.entry(task_id).or_insert(0) += 1;
        state.epochs.touch(task_id);
    }

    pub fn end(&self, task_id: i64) {
        let mut state = lock(&self.inner);
        if let Some(count) = state.epochs.inflight.get_mut(&task_id) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                state.epochs.inflight.remove(&task_id);
            }
        }
    }

    pub fn completion_count(&self, task_id: i64, epoch: i64) -> u64 {
        lock(&self.inner)
            .epochs
            .completions
            .get(&(task_id, epoch))
            .copied()
            .unwrap_or(0)
    }

    pub fn entries(&self) -> usize {
        lock(&self.inner).tasks.len()
    }

    pub fn cap(&self) -> usize {
        EPOCH_CAP
    }

    /// Take the queued token for this envelope. A cancel that already won returns
    /// `None` and drops only the matching slot.
    pub fn take_running(&self, submission: &NativeSubmission) -> Option<CancellationToken> {
        let mut state = lock(&self.inner);
        let key = submission.key().clone();
        let (cancelled, token, owned) = {
            let slot = state.jobs.get(&key)?;
            (
                slot.cancelled || slot.token.is_cancelled(),
                slot.token.clone(),
                Arc::ptr_eq(&slot.identity, submission.identity()),
            )
        };
        if !owned {
            return None;
        }
        if cancelled {
            state.jobs.remove(&key);
            return None;
        }
        let current_matches = state.tasks.get(&submission.task_id()).is_some_and(|task| {
            Arc::ptr_eq(&task.identity, submission.identity()) && task.epoch == submission.epoch()
        });
        if !current_matches {
            state.jobs.remove(&key);
            return None;
        }
        if let Some(slot) = state.jobs.get_mut(&key) {
            slot.phase = Phase::Running;
        }
        Some(token)
    }

    pub fn retire(&self, submission: &NativeSubmission) {
        let mut state = lock(&self.inner);
        let owned = state
            .jobs
            .get(submission.key())
            .is_some_and(|slot| Arc::ptr_eq(&slot.identity, submission.identity()));
        if owned {
            state.jobs.remove(submission.key());
        }
    }

    /// Keep a lightweight terminal record so cancel can tell linked from failed
    /// while the event is still unpublished. Does not replace a committed link
    /// or a slot that belongs to a different registration.
    pub fn seal_terminal(&self, submission: &NativeSubmission, committed: bool) {
        let mut state = lock(&self.inner);
        let Some(slot) = state.jobs.get_mut(submission.key()) else {
            return;
        };
        if !Arc::ptr_eq(&slot.identity, submission.identity()) {
            return;
        }
        if slot.outcome == Outcome::Committed {
            slot.phase = Phase::TerminalPending;
            return;
        }
        if slot.cancelled || slot.outcome == Outcome::Cancelled || slot.token.is_cancelled() {
            slot.cancelled = true;
            slot.outcome = Outcome::Cancelled;
            slot.phase = Phase::TerminalPending;
            return;
        }
        if slot.outcome != Outcome::Failed {
            slot.outcome = if committed {
                Outcome::Committed
            } else {
                Outcome::Failed
            };
        }
        slot.phase = Phase::TerminalPending;
    }

    /// Link only if this envelope still owns the publish. The check and the link
    /// share this lock. `hold` runs after the phase is committing and after the
    /// lock is released, so cancel observes `committing` instead of winning.
    pub fn publish(
        &self,
        submission: &NativeSubmission,
        staging: &Path,
        final_path: &Path,
        hold: Option<Arc<CommitGate>>,
        deadline: std::time::Instant,
    ) -> Result<(), &'static str> {
        let key = submission.key();
        {
            let mut state = lock(&self.inner);
            if std::time::Instant::now() >= deadline {
                return Err("deadline");
            }
            if !Arc::ptr_eq(submission.pool(), &self.pool) {
                return Err("stale_epoch");
            }
            let admitted = match (state.jobs.get(key), state.tasks.get(&submission.task_id())) {
                (Some(slot), _) if slot.cancelled || slot.token.is_cancelled() => {
                    return Err("cancelled");
                }
                (Some(slot), Some(task))
                    if slot.phase == Phase::Running
                        && Arc::ptr_eq(&slot.identity, submission.identity())
                        && Arc::ptr_eq(&task.identity, submission.identity())
                        && task.epoch == submission.epoch()
                        && submission.epoch() == key.epoch =>
                {
                    true
                }
                (None, _) => return Err("cancelled"),
                _ => false,
            };
            if !admitted {
                return Err("stale_epoch");
            }
            if final_path.exists() {
                return Err("target_exists");
            }
            if let Some(slot) = state.jobs.get_mut(key) {
                slot.phase = Phase::Committing;
            }
        }
        if let Some(hold) = hold {
            hold.arrive();
            let _ = hold.wait_release_only();
        }
        let mut state = lock(&self.inner);
        let still = state.jobs.get(key).is_some_and(|slot| {
            slot.phase == Phase::Committing && Arc::ptr_eq(&slot.identity, submission.identity())
        });
        if !still {
            return Err("cancelled");
        }
        if std::time::Instant::now() >= deadline {
            if let Some(slot) = state.jobs.get_mut(key) {
                if slot.phase == Phase::Committing
                    && Arc::ptr_eq(&slot.identity, submission.identity())
                {
                    slot.phase = Phase::Running;
                }
            }
            return Err("deadline");
        }
        match std::fs::hard_link(staging, final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(slot) = state.jobs.get_mut(key) {
                    if Arc::ptr_eq(&slot.identity, submission.identity()) {
                        slot.phase = Phase::TerminalPending;
                        slot.outcome = Outcome::Failed;
                    }
                }
                return Err("target_exists");
            }
            Err(_) => {
                if let Some(slot) = state.jobs.get_mut(key) {
                    if Arc::ptr_eq(&slot.identity, submission.identity()) {
                        slot.phase = Phase::TerminalPending;
                        slot.outcome = Outcome::Failed;
                    }
                }
                return Err("io_error");
            }
        }
        *state
            .epochs
            .completions
            .entry((submission.task_id(), submission.epoch()))
            .or_insert(0) += 1;
        state.bound_completions(submission.task_id());
        if let Some(slot) = state.jobs.get_mut(key) {
            if Arc::ptr_eq(&slot.identity, submission.identity()) {
                slot.phase = Phase::TerminalPending;
                slot.outcome = Outcome::Committed;
            }
        }
        Ok(())
    }

    fn authorize(&self, state: &State, submission: &NativeSubmission) -> Result<(), PoolError> {
        if !Arc::ptr_eq(submission.pool(), &self.pool) {
            return Err(PoolError::new("wrong_pool"));
        }
        if submission.task_id() != submission.key().task_id
            || submission.epoch() != submission.key().epoch
        {
            return Err(PoolError::new("stale_epoch"));
        }
        let Some(task) = state.tasks.get(&submission.task_id()) else {
            return Err(PoolError::new("unregistered"));
        };
        if task.epoch != submission.epoch() {
            return Err(PoolError::new("stale_epoch"));
        }
        if !Arc::ptr_eq(&task.identity, submission.identity()) {
            return Err(PoolError::new("stale_registration"));
        }
        Ok(())
    }
}

impl State {
    fn make_room(&mut self) -> Result<(), PoolError> {
        while self.tasks.len() >= EPOCH_CAP {
            if !self.evict_idle() {
                return Err(PoolError::new("epoch_budget"));
            }
        }
        Ok(())
    }

    fn evict_idle(&mut self) -> bool {
        let idle = self.epochs.order.iter().copied().find(|task| {
            self.epochs.inflight.get(task).copied().unwrap_or(0) == 0
                && !self.jobs.keys().any(|key| key.task_id == *task)
        });
        let Some(task) = idle else {
            return false;
        };
        self.tasks.remove(&task);
        self.epochs.inflight.remove(&task);
        self.epochs
            .completions
            .retain(|(stored, _), _| *stored != task);
        self.epochs.order.retain(|item| *item != task);
        true
    }

    fn bound_completions(&mut self, task_id: i64) {
        let mut epochs: Vec<i64> = self
            .epochs
            .completions
            .keys()
            .filter(|(stored, _)| *stored == task_id)
            .map(|(_, epoch)| *epoch)
            .collect();
        epochs.sort_unstable();
        while epochs.len() > COMPLETIONS_PER_TASK {
            let oldest = epochs.remove(0);
            self.epochs.completions.remove(&(task_id, oldest));
        }
    }
}

impl EpochState {
    fn touch(&mut self, task_id: i64) {
        self.order.retain(|item| *item != task_id);
        self.order.push_back(task_id);
    }
}

fn committing(state: &State, task_id: i64) -> bool {
    state
        .jobs
        .iter()
        .any(|(key, slot)| key.task_id == task_id && slot.phase == Phase::Committing)
}
