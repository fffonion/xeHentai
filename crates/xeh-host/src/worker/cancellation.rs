use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::sync::Mutex;

use crate::dto::lock;

const EPOCH_CAP: usize = 64;

struct EpochState {
    current: HashMap<i64, i64>,
    completions: HashMap<(i64, i64), u64>,
    inflight: HashMap<i64, usize>,
    order: VecDeque<i64>,
}

/// Epoch admission for one pool. The lock is held only around the publish
/// check and the same-directory hard link, never across a network wait.
pub struct EpochTable {
    inner: Mutex<EpochState>,
}

impl EpochTable {
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(EpochState {
                current: HashMap::new(),
                completions: HashMap::new(),
                inflight: HashMap::new(),
                order: VecDeque::new(),
            }),
        }
    }

    pub fn advance(&self, task_id: i64, epoch: i64) {
        let mut state = lock(&self.inner);
        state.current.insert(task_id, epoch);
        state.touch(task_id);
        state.evict();
    }

    pub fn begin(&self, task_id: i64) {
        let mut state = lock(&self.inner);
        *state.inflight.entry(task_id).or_insert(0) += 1;
        state.touch(task_id);
    }

    pub fn end(&self, task_id: i64) {
        let mut state = lock(&self.inner);
        if let Some(count) = state.inflight.get_mut(&task_id) {
            *count = count.saturating_sub(1);
        }
        state.evict();
    }

    pub fn completion_count(&self, task_id: i64, epoch: i64) -> u64 {
        lock(&self.inner)
            .completions
            .get(&(task_id, epoch))
            .copied()
            .unwrap_or(0)
    }

    pub fn entries(&self) -> usize {
        lock(&self.inner).current.len()
    }

    pub fn cap(&self) -> usize {
        EPOCH_CAP
    }

    /// Link `staging` onto `final_path` only if this epoch is still admitted.
    /// An existing final path is left untouched.
    pub fn publish(
        &self,
        task_id: i64,
        epoch: i64,
        staging: &Path,
        final_path: &Path,
    ) -> Result<(), &'static str> {
        let mut state = lock(&self.inner);
        if let Some(current) = state.current.get(&task_id) {
            if *current != epoch {
                return Err("stale_epoch");
            }
        } else {
            state.current.insert(task_id, epoch);
            state.touch(task_id);
        }
        if final_path.exists() {
            return Err("target_exists");
        }
        match std::fs::hard_link(staging, final_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err("target_exists");
            }
            Err(_) => return Err("io_error"),
        }
        *state.completions.entry((task_id, epoch)).or_insert(0) += 1;
        state.evict();
        Ok(())
    }
}

impl EpochState {
    fn touch(&mut self, task_id: i64) {
        self.order.retain(|item| *item != task_id);
        self.order.push_back(task_id);
    }

    fn evict(&mut self) {
        while self.current.len() > EPOCH_CAP {
            let idle = self
                .order
                .iter()
                .copied()
                .find(|task| self.inflight.get(task).copied().unwrap_or(0) == 0);
            let Some(task) = idle else {
                break;
            };
            self.current.remove(&task);
            self.inflight.remove(&task);
            self.completions.retain(|(stored, _), _| *stored != task);
            self.order.retain(|item| *item != task);
        }
    }
}
