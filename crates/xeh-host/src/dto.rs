use std::fmt;
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;
use vm::HttpConfig;

use std::sync::atomic::{AtomicBool, Ordering};

/// Cross-thread job identity. Values are copied; no VM handle is included.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct JobKey {
    pub task_id: i64,
    pub epoch: i64,
    pub job_id: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderEntry {
    pub name: Vec<u8>,
    pub value: Vec<u8>,
}

/// Typed transfer request. Image bytes are never a field of this value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferRequest {
    pub key: JobKey,
    pub url: String,
    pub headers: Vec<HeaderEntry>,
    pub relative_path: String,
}

/// Typed result returned to the guest and copied to the worker as a DTO.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferReceipt {
    pub key: JobKey,
    pub status: String,
    pub bytes: i64,
    pub path: String,
    pub error_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferEvent {
    pub key: JobKey,
    pub kind: String,
    pub bytes: i64,
    pub terminal_status: Option<String>,
}

#[derive(Debug, Clone)]
pub struct WriterFault {
    pub fail_after_bytes: u64,
    armed: Arc<std::sync::atomic::AtomicBool>,
}

impl WriterFault {
    pub fn once(fail_after_bytes: u64) -> Self {
        Self {
            fail_after_bytes,
            armed: Arc::new(std::sync::atomic::AtomicBool::new(true)),
        }
    }

    pub fn is_armed(&self) -> bool {
        self.armed.load(std::sync::atomic::Ordering::Acquire)
    }

    pub fn disarm(&self) {
        self.armed
            .store(false, std::sync::atomic::Ordering::Release);
    }
}

/// Test/control gate between file close and epoch admission.
///
/// Arrival and release are explicit notifications. The wait loop does not
/// treat a timeout as success.
#[derive(Debug)]
pub struct CommitGate {
    arrived: Mutex<usize>,
    release: Mutex<bool>,
    cv: Condvar,
}

impl CommitGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            arrived: Mutex::new(0),
            release: Mutex::new(false),
            cv: Condvar::new(),
        })
    }

    pub fn wait_arrived(&self, count: usize, timeout: Duration) -> Result<(), PoolError> {
        let deadline = std::time::Instant::now() + timeout;
        let mut arrived = lock(&self.arrived);
        while *arrived < count {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(PoolError::new("timeout"));
            }
            let (next, wait) = self
                .cv
                .wait_timeout(arrived, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            arrived = next;
            if wait.timed_out() && *arrived < count {
                return Err(PoolError::new("timeout"));
            }
        }
        Ok(())
    }

    pub fn release(&self) {
        *lock(&self.release) = true;
        self.cv.notify_all();
    }

    pub(crate) fn arrive(&self) {
        *lock(&self.arrived) += 1;
        self.cv.notify_all();
    }

    pub(crate) fn kick(&self) {
        self.cv.notify_all();
    }

    pub(crate) fn wait_release(
        &self,
        cancel: &CancellationToken,
        shut: &AtomicBool,
    ) -> Result<(), &'static str> {
        let mut released = lock(&self.release);
        while !*released {
            if cancel.is_cancelled() || shut.load(Ordering::Acquire) {
                return Err("cancelled");
            }
            let (next, _) = self
                .cv
                .wait_timeout(released, Duration::from_secs(30))
                .unwrap_or_else(|poison| poison.into_inner());
            released = next;
        }
        if cancel.is_cancelled() || shut.load(Ordering::Acquire) {
            return Err("cancelled");
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct PoolConfig {
    pub workers: usize,
    pub job_capacity: usize,
    pub event_capacity: usize,
    pub http: HttpConfig,
    pub root: PathBuf,
    pub chunk_bytes: usize,
    pub stream_limit: usize,
    pub writer_fault: Option<WriterFault>,
    pub commit_gate: Option<Arc<CommitGate>>,
    pub fail_worker_once: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownReport {
    pub pending_jobs: usize,
    pub in_flight: usize,
    pub permits: usize,
    pub tracked_jobs: usize,
    pub joined: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolError {
    code: &'static str,
}

impl PoolError {
    pub fn new(code: &'static str) -> Self {
        Self { code }
    }

    pub fn code(&self) -> &'static str {
        self.code
    }
}

impl fmt::Display for PoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code)
    }
}

impl std::error::Error for PoolError {}

pub(crate) fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}
