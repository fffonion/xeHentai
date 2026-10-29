use std::collections::VecDeque;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use crate::dto::{lock, PoolError, TransferEvent};

struct EventState {
    items: VecDeque<TransferEvent>,
    closed: bool,
    consumers: usize,
    dropped_progress: u64,
    dropped_terminal: u64,
}

/// Bounded event bus. Progress for one job is replaced in place. A terminal
/// event is not discarded while a consumer is still attached, unless the bus
/// is closed by shutdown or the consumer has detached.
pub struct EventBus {
    capacity: usize,
    state: Mutex<EventState>,
    cv: Condvar,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(EventState {
                items: VecDeque::new(),
                closed: false,
                consumers: 1,
                dropped_progress: 0,
                dropped_terminal: 0,
            }),
            cv: Condvar::new(),
        }
    }

    pub fn close(&self) {
        let mut state = lock(&self.state);
        state.closed = true;
        state.consumers = 0;
        self.cv.notify_all();
    }

    pub fn dropped_progress(&self) -> u64 {
        lock(&self.state).dropped_progress
    }

    pub fn dropped_terminal(&self) -> u64 {
        lock(&self.state).dropped_terminal
    }

    pub fn publish_progress(&self, event: TransferEvent) {
        let mut state = lock(&self.state);
        if state.closed || state.consumers == 0 {
            state.dropped_progress += 1;
            return;
        }
        if let Some(existing) = state
            .items
            .iter_mut()
            .find(|item| item.terminal_status.is_none() && item.key == event.key)
        {
            existing.bytes = event.bytes;
            return;
        }
        if state.items.len() >= self.capacity {
            state.dropped_progress += 1;
            return;
        }
        state.items.push_back(event);
        self.cv.notify_all();
    }

    pub fn publish_terminal(&self, event: TransferEvent) -> Result<(), PoolError> {
        let mut state = lock(&self.state);
        state
            .items
            .retain(|item| !(item.terminal_status.is_none() && item.key == event.key));
        if state
            .items
            .iter()
            .any(|item| item.terminal_status.is_some() && item.key == event.key)
        {
            return Ok(());
        }
        loop {
            if state.closed || state.consumers == 0 {
                state.dropped_terminal += 1;
                return Ok(());
            }
            if state.items.len() < self.capacity {
                state.items.push_back(event);
                self.cv.notify_all();
                return Ok(());
            }
            if let Some(index) = state
                .items
                .iter()
                .position(|item| item.terminal_status.is_none())
            {
                state.items.remove(index);
                state.dropped_progress += 1;
                continue;
            }
            let (next, _) = self
                .cv
                .wait_timeout(state, Duration::from_secs(30))
                .unwrap_or_else(|poison| poison.into_inner());
            state = next;
        }
    }

    pub fn wait_terminals(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<TransferEvent>, PoolError> {
        let deadline = Instant::now() + timeout;
        let mut got = Vec::new();
        let mut state = lock(&self.state);
        loop {
            while got.len() < count {
                let Some(index) = state
                    .items
                    .iter()
                    .position(|event| event.terminal_status.is_some())
                else {
                    break;
                };
                if let Some(event) = state.items.remove(index) {
                    got.push(event);
                }
            }
            if got.len() >= count {
                return Ok(got);
            }
            if state.closed {
                return Err(PoolError::new("stopped"));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(PoolError::new("timeout"));
            }
            let (next, wait) = self
                .cv
                .wait_timeout(state, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            state = next;
            if wait.timed_out() && got.len() < count {
                return Err(PoolError::new("timeout"));
            }
        }
    }

    pub fn drain(&self, limit: usize) -> Vec<TransferEvent> {
        let mut state = lock(&self.state);
        let mut out = Vec::new();
        while out.len() < limit {
            let Some(event) = state.items.pop_front() else {
                break;
            };
            out.push(event);
        }
        if !state.items.is_empty() || !out.is_empty() {
            self.cv.notify_all();
        }
        out
    }
}
