use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::dto::{lock, CommitGate, JobKey, PoolError};

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
    pub generation: u64,
}

struct TaskLease {
    epoch: i64,
    generation: u64,
}

struct EpochState {
    completions: HashMap<(i64, i64), u64>,
    inflight: HashMap<i64, usize>,
    order: VecDeque<i64>,
}

struct State {
    jobs: HashMap<JobKey, JobSlot>,
    tasks: HashMap<i64, TaskLease>,
    next_generation: u64,
    epochs: EpochState,
}

/// One admission lock for registration, queue records, cancel, epoch advance,
/// and the final hard link. Network waits must not hold this lock.
pub struct Admission {
    inner: Mutex<State>,
}

impl Admission {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(State {
                jobs: HashMap::new(),
                tasks: HashMap::new(),
                next_generation: 1,
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
    /// registration does not replace the lease, bump generation, or cancel jobs.
    pub fn register(&self, task_id: i64, epoch: i64) -> Result<(), PoolError> {
        let mut state = lock(&self.inner);
        if state.tasks.contains_key(&task_id) {
            return Err(PoolError::new("task_exists"));
        }
        state.make_room()?;
        let generation = state.next_generation;
        state.next_generation = state.next_generation.saturating_add(1);
        state.tasks.insert(task_id, TaskLease { epoch, generation });
        state.epochs.touch(task_id);
        Ok(())
    }

    pub fn insert_queued(&self, key: JobKey, token: CancellationToken) -> Result<(), PoolError> {
        let mut state = lock(&self.inner);
        if state.jobs.contains_key(&key) {
            return Err(PoolError::new("duplicate"));
        }
        let Some(task) = state.tasks.get(&key.task_id) else {
            return Err(PoolError::new("unregistered"));
        };
        if task.epoch != key.epoch {
            return Err(PoolError::new("stale_epoch"));
        }
        let generation = task.generation;
        state.jobs.insert(
            key,
            JobSlot {
                token,
                cancelled: false,
                phase: Phase::Queued,
                outcome: Outcome::Open,
                generation,
            },
        );
        Ok(())
    }

    pub fn rollback(&self, key: &JobKey) {
        let mut state = lock(&self.inner);
        if let Some(slot) = state.jobs.get(key) {
            if slot.phase == Phase::Queued {
                state.jobs.remove(key);
            }
        }
    }

    /// Queued and Running confirm a cancel. Committing is too late. A linked
    /// terminal is `already_complete`; a failed terminal is `already_failed`.
    /// A cancel that already won stays `Ok`.
    pub fn cancel(&self, key: &JobKey) -> Result<(), PoolError> {
        let mut state = lock(&self.inner);
        let Some(slot) = state.jobs.get_mut(key) else {
            return Err(PoolError::new("unknown"));
        };
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

    pub fn advance(&self, task_id: i64, epoch: i64) -> Result<(), PoolError> {
        let mut state = lock(&self.inner);
        let current = {
            let Some(task) = state.tasks.get(&task_id) else {
                return Err(PoolError::new("unregistered"));
            };
            task.epoch
        };
        if epoch < current {
            return Err(PoolError::new("regression"));
        }
        if committing(&state, task_id) {
            return Err(PoolError::new("commit_pending"));
        }
        if epoch == current {
            return Ok(());
        }
        if let Some(stored) = state.tasks.get_mut(&task_id) {
            stored.epoch = epoch;
        }
        state.epochs.touch(task_id);
        state.bound_completions(task_id);
        for (key, slot) in &mut state.jobs {
            if key.task_id == task_id
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

    /// Take the queued token. A cancel that already won returns `None` and drops the slot.
    pub fn take_running(&self, key: &JobKey) -> Option<CancellationToken> {
        let mut state = lock(&self.inner);
        let (generation, cancelled, token) = {
            let slot = state.jobs.get(key)?;
            (
                slot.generation,
                slot.cancelled || slot.token.is_cancelled(),
                slot.token.clone(),
            )
        };
        if cancelled {
            state.jobs.remove(key);
            return None;
        }
        let current = {
            let Some(task) = state.tasks.get(&key.task_id) else {
                state.jobs.remove(key);
                return None;
            };
            (task.generation, task.epoch)
        };
        if current.0 != generation || current.1 != key.epoch {
            state.jobs.remove(key);
            return None;
        }
        if let Some(slot) = state.jobs.get_mut(key) {
            slot.phase = Phase::Running;
        }
        Some(token)
    }

    pub fn retire(&self, key: &JobKey) {
        lock(&self.inner).jobs.remove(key);
    }

    /// Keep a lightweight terminal record so cancel can tell linked from failed
    /// while the event is still unpublished. Does not replace a committed link.
    pub fn seal_terminal(&self, key: &JobKey, committed: bool) {
        let mut state = lock(&self.inner);
        let Some(slot) = state.jobs.get_mut(key) else {
            return;
        };
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

    /// Link only if this lease still owns the publish. The check and the link
    /// share this lock. `hold` runs after the phase is committing and after the
    /// lock is released, so cancel observes `committing` instead of winning.
    #[allow(clippy::too_many_arguments)]
    pub fn publish(
        &self,
        task_id: i64,
        epoch: i64,
        key: &JobKey,
        staging: &Path,
        final_path: &Path,
        hold: Option<Arc<CommitGate>>,
        deadline: std::time::Instant,
    ) -> Result<(), &'static str> {
        {
            let mut state = lock(&self.inner);
            if std::time::Instant::now() >= deadline {
                return Err("deadline");
            }
            let admitted = match (state.jobs.get(key), state.tasks.get(&task_id)) {
                (Some(slot), _) if slot.cancelled || slot.token.is_cancelled() => {
                    return Err("cancelled");
                }
                (Some(slot), Some(task))
                    if slot.phase == Phase::Running
                        && slot.generation == task.generation
                        && task.epoch == epoch =>
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
        let still = state
            .jobs
            .get(key)
            .is_some_and(|slot| slot.phase == Phase::Committing);
        if !still {
            return Err("cancelled");
        }
        if std::time::Instant::now() >= deadline {
            if let Some(slot) = state.jobs.get_mut(key) {
                if slot.phase == Phase::Committing {
                    slot.phase = Phase::Running;
                }
            }
            return Err("deadline");
        }
        match std::fs::hard_link(staging, final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Some(slot) = state.jobs.get_mut(key) {
                    slot.phase = Phase::TerminalPending;
                    slot.outcome = Outcome::Failed;
                }
                return Err("target_exists");
            }
            Err(_) => {
                if let Some(slot) = state.jobs.get_mut(key) {
                    slot.phase = Phase::TerminalPending;
                    slot.outcome = Outcome::Failed;
                }
                return Err("io_error");
            }
        }
        *state
            .epochs
            .completions
            .entry((task_id, epoch))
            .or_insert(0) += 1;
        state.bound_completions(task_id);
        if let Some(slot) = state.jobs.get_mut(key) {
            slot.phase = Phase::TerminalPending;
            slot.outcome = Outcome::Committed;
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
