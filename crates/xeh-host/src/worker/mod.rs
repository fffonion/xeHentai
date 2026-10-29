use std::collections::HashMap;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle, ThreadId};
use std::time::Duration;

use tokio::runtime::Builder;
use tokio_util::sync::CancellationToken;
use vm::Vm;

use crate::catalog::open_registry;
use crate::dto::{
    lock, JobKey, PoolConfig, PoolError, ShutdownReport, TransferEvent, TransferReceipt,
    TransferRequest,
};
use crate::observe::Metrics;
use crate::runtime::{
    attach_timer, bind_vm, compile_source_with_catalog, drive_to_halt, release_for_reuse,
};
use crate::timer::EmbeddingTimer;
use crate::transfer::{register_transfer, transfer_catalog, TransferHost};

pub(crate) mod cancellation;
pub(crate) mod events;

use cancellation::EpochTable;
use events::EventBus;

struct ReadyState {
    owners: usize,
    failed: bool,
    detail: String,
}

struct Shared {
    config: PoolConfig,
    source: String,
    shut: Arc<AtomicBool>,
    fail_once: AtomicBool,
    metrics: Arc<Metrics>,
    events: Arc<EventBus>,
    epochs: Arc<EpochTable>,
    jobs: Mutex<HashMap<JobKey, CancellationToken>>,
    ready: Mutex<ReadyState>,
    ready_cv: Condvar,
    cancel_root: CancellationToken,
    queue: Mutex<Receiver<TransferRequest>>,
}

/// Fixed OS-thread workers. Each thread owns one current-thread runtime, VM,
/// registry, bridge, and timer. Cross-thread traffic is the job DTO, epoch
/// table, and this pool's synchronization primitives.
pub struct WorkerPool {
    shared: Arc<Shared>,
    sender: Mutex<Option<SyncSender<TransferRequest>>>,
    handles: Mutex<Vec<JoinHandle<()>>>,
    joined: AtomicBool,
}

impl WorkerPool {
    pub fn start(config: PoolConfig) -> Result<Self, PoolError> {
        validate_config(&config)?;
        let source = std::fs::read_to_string(transfer_source_path()).map_err(|_| {
            eprintln!("worker source is missing");
            PoolError::new("worker_failed")
        })?;
        let (sender, receiver) = sync_channel(config.job_capacity);
        let shared = Arc::new(Shared {
            source,
            shut: Arc::new(AtomicBool::new(false)),
            fail_once: AtomicBool::new(config.fail_worker_once),
            metrics: Arc::new(Metrics::default()),
            events: Arc::new(EventBus::new(config.event_capacity)),
            epochs: Arc::new(EpochTable::new()),
            jobs: Mutex::new(HashMap::new()),
            ready: Mutex::new(ReadyState {
                owners: 0,
                failed: false,
                detail: String::new(),
            }),
            ready_cv: Condvar::new(),
            cancel_root: CancellationToken::new(),
            queue: Mutex::new(receiver),
            config,
        });
        let mut handles = Vec::new();
        for index in 0..shared.config.workers {
            let shared = Arc::clone(&shared);
            let handle = thread::Builder::new()
                .name(format!("xeh-worker-{index}"))
                .spawn(move || worker_main(shared))
                .map_err(|_| PoolError::new("worker_failed"))?;
            handles.push(handle);
        }
        Ok(Self {
            shared,
            sender: Mutex::new(Some(sender)),
            handles: Mutex::new(handles),
            joined: AtomicBool::new(false),
        })
    }

    pub fn wait_ready(&self, timeout: Duration) -> Result<(), PoolError> {
        let deadline = std::time::Instant::now() + timeout;
        let mut ready = lock(&self.shared.ready);
        while ready.owners < self.shared.config.workers && !ready.failed {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(PoolError::new("timeout"));
            }
            let (next, wait) = self
                .shared
                .ready_cv
                .wait_timeout(ready, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            ready = next;
            if wait.timed_out() && ready.owners < self.shared.config.workers && !ready.failed {
                return Err(PoolError::new("timeout"));
            }
        }
        if ready.failed {
            eprintln!("worker setup failed: {}", ready.detail);
            return Err(PoolError::new("worker_failed"));
        }
        Ok(())
    }

    pub fn submit(&self, request: TransferRequest) -> Result<(), PoolError> {
        if self.shared.shut.load(Ordering::Acquire) {
            return Err(PoolError::new("stopped"));
        }
        let sender = lock(&self.sender);
        let Some(sender) = sender.as_ref() else {
            return Err(PoolError::new("stopped"));
        };
        match sender.try_send(request) {
            Ok(()) => {
                self.shared.metrics.pending_add();
                Ok(())
            }
            Err(TrySendError::Full(_)) => Err(PoolError::new("queue_full")),
            Err(TrySendError::Disconnected(_)) => Err(PoolError::new("stopped")),
        }
    }

    pub fn cancel(&self, key: &JobKey) -> Result<(), PoolError> {
        if let Some(token) = lock(&self.shared.jobs).get(key).cloned() {
            token.cancel();
        }
        if let Some(gate) = &self.shared.config.commit_gate {
            gate.kick();
        }
        Ok(())
    }

    pub fn advance_epoch(&self, task_id: i64, epoch: i64) -> Result<(), PoolError> {
        self.shared.epochs.advance(task_id, epoch);
        let stale: Vec<CancellationToken> = lock(&self.shared.jobs)
            .iter()
            .filter(|(key, _)| key.task_id == task_id && key.epoch != epoch)
            .map(|(_, token)| token.clone())
            .collect();
        for token in stale {
            token.cancel();
        }
        if let Some(gate) = &self.shared.config.commit_gate {
            gate.kick();
        }
        Ok(())
    }

    pub fn wait_terminals(
        &self,
        count: usize,
        timeout: Duration,
    ) -> Result<Vec<TransferEvent>, PoolError> {
        self.shared.events.wait_terminals(count, timeout)
    }

    pub fn drain(&self, limit: usize) -> Vec<TransferEvent> {
        self.shared.events.drain(limit)
    }

    pub fn completion_count(&self, task_id: i64, epoch: i64) -> u64 {
        self.shared.epochs.completion_count(task_id, epoch)
    }

    pub fn owner_threads(&self) -> Vec<ThreadId> {
        self.shared.metrics.owners()
    }

    pub fn build_threads(&self) -> Vec<ThreadId> {
        self.shared.metrics.builds()
    }

    pub fn execution_threads(&self) -> Vec<ThreadId> {
        self.shared.metrics.executing()
    }

    pub fn configured_workers(&self) -> usize {
        self.shared.config.workers
    }

    pub fn job_capacity(&self) -> usize {
        self.shared.config.job_capacity
    }

    pub fn event_capacity(&self) -> usize {
        self.shared.config.event_capacity
    }

    pub fn in_flight(&self) -> usize {
        self.shared.metrics.in_flight()
    }

    pub fn peak_in_flight(&self) -> usize {
        self.shared.metrics.peak_in_flight()
    }

    pub fn connect_targets(&self) -> Vec<std::net::SocketAddr> {
        self.shared.metrics.connect_targets()
    }

    pub fn peak_application_bytes(&self) -> usize {
        self.shared.metrics.peak_application_bytes()
    }

    pub fn peak_application_capacity(&self) -> usize {
        self.shared.metrics.peak_application_capacity()
    }

    pub fn application_bytes(&self) -> usize {
        self.shared.metrics.application_bytes()
    }

    pub fn guest_payload_bytes(&self) -> usize {
        self.shared.metrics.guest_payload_bytes()
    }

    pub fn epoch_entries(&self) -> usize {
        self.shared.epochs.entries()
    }

    pub fn epoch_cap(&self) -> usize {
        self.shared.epochs.cap()
    }

    pub fn dropped_progress(&self) -> u64 {
        self.shared.events.dropped_progress()
    }

    pub fn dropped_terminal(&self) -> u64 {
        self.shared.events.dropped_terminal()
    }

    pub fn permits(&self) -> usize {
        self.shared.metrics.permits()
    }

    pub fn pending_jobs(&self) -> usize {
        self.shared.metrics.pending()
    }

    pub fn tracked_jobs(&self) -> usize {
        lock(&self.shared.jobs).len()
    }

    pub fn shutdown(self) -> Result<ShutdownReport, PoolError> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&self) -> Result<ShutdownReport, PoolError> {
        if self.joined.swap(true, Ordering::AcqRel) {
            return Ok(self.report(true));
        }
        self.shared.shut.store(true, Ordering::Release);
        self.shared.cancel_root.cancel();
        if let Some(gate) = &self.shared.config.commit_gate {
            gate.kick();
        }
        self.shared.events.close();
        drop(lock(&self.sender).take());
        let handles = std::mem::take(&mut *lock(&self.handles));
        let mut joined = true;
        for handle in handles {
            if handle.join().is_err() {
                joined = false;
            }
        }
        Ok(self.report(joined))
    }

    fn report(&self, joined: bool) -> ShutdownReport {
        ShutdownReport {
            pending_jobs: self.shared.metrics.pending(),
            in_flight: self.shared.metrics.in_flight(),
            permits: self.shared.metrics.permits(),
            tracked_jobs: lock(&self.shared.jobs).len(),
            joined,
        }
    }
}

impl Drop for WorkerPool {
    fn drop(&mut self) {
        let _ = self.shutdown_inner();
    }
}

fn validate_config(config: &PoolConfig) -> Result<(), PoolError> {
    if config.workers == 0
        || config.job_capacity == 0
        || config.event_capacity == 0
        || config.chunk_bytes == 0
        || config.stream_limit == 0
    {
        return Err(PoolError::new("invalid_config"));
    }
    if !config.root.is_dir() {
        return Err(PoolError::new("invalid_config"));
    }
    config
        .http
        .validate()
        .map_err(|_| PoolError::new("policy_denied"))?;
    Ok(())
}

fn transfer_source_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/worker/transfer.rss")
}

fn worker_main(shared: Arc<Shared>) {
    let id = thread::current().id();
    shared.metrics.record_owner(id);
    let host = Arc::new(make_host(&shared));
    let built = build_vm(&shared, &host);
    let (mut runtime, mut vm) = match built {
        Ok(pair) => pair,
        Err(detail) => {
            fail_ready(&shared, detail);
            return;
        }
    };
    {
        let mut ready = lock(&shared.ready);
        ready.owners += 1;
        shared.ready_cv.notify_all();
    }
    loop {
        let job = {
            let receiver = lock(&shared.queue);
            receiver.recv()
        };
        let Ok(job) = job else {
            break;
        };
        shared.metrics.pending_sub();
        if shared.shut.load(Ordering::Acquire) {
            continue;
        }
        let token = shared.cancel_root.child_token();
        let parked = token.clone();
        lock(&shared.jobs).insert(job.key.clone(), token.clone());
        *lock(&host.current) = Some(job.clone());
        *lock(&host.active_cancel) = Some(token);
        *lock(&host.receipt) = None;
        shared.metrics.begin_permit();
        shared.epochs.begin(job.key.task_id);
        let panicked = shared.fail_once.swap(false, Ordering::AcqRel);
        let outcome = catch_unwind(AssertUnwindSafe(|| {
            if panicked {
                panic!("injected worker failure");
            }
            runtime.block_on(async {
                let drive = drive_to_halt(&mut vm).await;
                let reset = release_for_reuse(&mut vm).await;
                drive.and(reset)
            })
        }));
        if parked.is_cancelled() {
            runtime.block_on(flush_cancelled_connection());
        }
        let receipt = lock(&host.receipt).clone();
        publish_terminal(&shared, &job, outcome.is_ok(), receipt);
        *lock(&host.current) = None;
        *lock(&host.active_cancel) = None;
        lock(&shared.jobs).remove(&job.key);
        shared.epochs.end(job.key.task_id);
        shared.metrics.end_permit();
        if outcome.is_err() {
            match build_vm(&shared, &host) {
                Ok(rebuilt) => {
                    runtime = rebuilt.0;
                    vm = rebuilt.1;
                }
                Err(detail) => {
                    eprintln!("worker rebuild failed: {detail}");
                    break;
                }
            }
        }
    }
}

async fn flush_cancelled_connection() {
    for _ in 0..4 {
        tokio::task::yield_now().await;
    }
    tokio::time::sleep(Duration::from_millis(20)).await;
    tokio::task::yield_now().await;
}

fn publish_terminal(
    shared: &Shared,
    job: &TransferRequest,
    drive_ok: bool,
    receipt: Option<TransferReceipt>,
) {
    let (status, bytes) = match receipt {
        Some(receipt) => (receipt.status, receipt.bytes),
        None if drive_ok => ("worker_failed".to_string(), 0),
        None => ("worker_failed".to_string(), 0),
    };
    let _ = shared.events.publish_terminal(TransferEvent {
        key: job.key.clone(),
        kind: "terminal".to_string(),
        bytes,
        terminal_status: Some(status),
    });
}

fn make_host(shared: &Shared) -> TransferHost {
    TransferHost {
        current: Mutex::new(None),
        receipt: Mutex::new(None),
        http: shared.config.http.clone(),
        root: shared.config.root.clone(),
        chunk_bytes: shared.config.chunk_bytes,
        stream_limit: shared.config.stream_limit,
        writer_fault: shared.config.writer_fault.clone(),
        commit_gate: shared.config.commit_gate.clone(),
        epochs: Arc::clone(&shared.epochs),
        metrics: Arc::clone(&shared.metrics),
        events: Arc::clone(&shared.events),
        shut: Arc::clone(&shared.shut),
        active_cancel: Mutex::new(None),
    }
}

fn build_vm(
    shared: &Shared,
    host: &Arc<TransferHost>,
) -> Result<(tokio::runtime::Runtime, Vm), String> {
    let runtime = Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let catalog = transfer_catalog();
    let program = compile_source_with_catalog(&shared.source, Arc::clone(&catalog))
        .map_err(|error| error.to_string())?;
    let mut vm = Vm::try_new(program).map_err(|error| error.to_string())?;
    shared.metrics.record_build(thread::current().id());
    let mut registry = open_registry(&catalog).map_err(|error| error.to_string())?;
    register_transfer(&mut registry, &catalog, Arc::clone(host))
        .map_err(|error| error.to_string())?;
    bind_vm(&mut vm, &registry).map_err(|error| error.to_string())?;
    attach_timer(&mut vm, Arc::new(EmbeddingTimer::new()));
    Ok((runtime, vm))
}

fn fail_ready(shared: &Shared, detail: String) {
    eprintln!("worker setup failed: {detail}");
    let mut ready = lock(&shared.ready);
    ready.failed = true;
    ready.detail = detail;
    shared.ready_cv.notify_all();
}
