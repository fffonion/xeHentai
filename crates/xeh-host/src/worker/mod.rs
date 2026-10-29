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

use cancellation::Admission;
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
    admission: Arc<Admission>,
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
    joiners: Mutex<Vec<std::sync::mpsc::Receiver<()>>>,
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
            admission: Arc::new(Admission::new()),
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
            joiners: Mutex::new(Vec::new()),
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
        let token = self.shared.cancel_root.child_token();
        let key = request.key.clone();
        self.shared.admission.insert_queued(key.clone(), token)?;
        match sender.try_send(request) {
            Ok(()) => {
                self.shared.metrics.pending_add();
                Ok(())
            }
            Err(error) => {
                self.shared.admission.rollback(&key);
                match error {
                    TrySendError::Full(_) => Err(PoolError::new("queue_full")),
                    TrySendError::Disconnected(_) => Err(PoolError::new("stopped")),
                }
            }
        }
    }

    pub fn cancel(&self, key: &JobKey) -> Result<(), PoolError> {
        self.shared.admission.cancel(key)?;
        self.kick_cancellable();
        Ok(())
    }

    pub fn advance_epoch(&self, task_id: i64, epoch: i64) -> Result<(), PoolError> {
        self.shared.admission.advance(task_id, epoch)?;
        if let Some(gate) = &self.shared.config.commit_gate {
            gate.kick();
        }
        if let Some(gate) = &self.shared.config.link_gate {
            gate.kick();
        }
        if let Some(gate) = &self.shared.config.commit_hold {
            gate.kick();
        }
        Ok(())
    }

    pub fn register(&self, task_id: i64, epoch: i64) -> Result<(), PoolError> {
        self.shared.admission.register(task_id, epoch)?;
        if let Some(gate) = &self.shared.config.commit_gate {
            gate.kick();
        }
        if let Some(gate) = &self.shared.config.link_gate {
            gate.kick();
        }
        Ok(())
    }

    pub fn state_entries(&self) -> usize {
        self.shared.admission.state_entries()
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
        self.shared.admission.completion_count(task_id, epoch)
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
        self.shared.admission.entries()
    }

    pub fn epoch_cap(&self) -> usize {
        self.shared.admission.cap()
    }

    pub fn dropped_progress(&self) -> u64 {
        self.shared.events.dropped_progress()
    }

    pub fn dropped_terminal(&self) -> u64 {
        self.shared.events.dropped_terminal()
    }

    pub fn detach_events(&self) {
        self.shared.events.detach_consumer();
    }

    pub fn dns_candidates(&self) -> Vec<std::net::SocketAddr> {
        self.shared.metrics.dns_candidates()
    }

    pub fn failure_codes(&self) -> usize {
        self.shared.metrics.failure_codes()
    }

    pub fn permits(&self) -> usize {
        self.shared.metrics.permits()
    }

    pub fn pending_jobs(&self) -> usize {
        self.shared.metrics.pending()
    }

    pub fn tracked_jobs(&self) -> usize {
        self.shared.admission.tracked()
    }

    pub fn shutdown(self) -> Result<ShutdownReport, PoolError> {
        self.shutdown_inner()
    }

    fn shutdown_inner(&self) -> Result<ShutdownReport, PoolError> {
        self.shutdown_within(Duration::from_secs(5))
    }

    pub fn shutdown_within(&self, budget: Duration) -> Result<ShutdownReport, PoolError> {
        self.shared.shut.store(true, Ordering::Release);
        self.shared.cancel_root.cancel();
        self.kick_cancellable();
        self.shared.events.close();
        drop(lock(&self.sender).take());
        self.spawn_joiners();
        let finished = self.wait_joiners(budget);
        if finished {
            self.joined.store(true, Ordering::Release);
        }
        Ok(self.report(finished))
    }

    fn kick_cancellable(&self) {
        for gate in [
            self.shared.config.commit_gate.as_ref(),
            self.shared.config.link_gate.as_ref(),
            self.shared.config.commit_hold.as_ref(),
            self.shared.config.hooks.dns_hold.as_ref(),
            self.shared.config.hooks.write_hold.as_ref(),
            self.shared.config.hooks.buffer_hold.as_ref(),
        ]
        .into_iter()
        .flatten()
        {
            gate.kick();
        }
    }

    fn spawn_joiners(&self) {
        let mut handles = lock(&self.handles);
        let mut joiners = lock(&self.joiners);
        for handle in handles.drain(..) {
            let (tx, rx) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let _ = handle.join();
                let _ = tx.send(());
            });
            joiners.push(rx);
        }
    }

    fn wait_joiners(&self, budget: Duration) -> bool {
        let deadline = std::time::Instant::now() + budget;
        let mut joiners = lock(&self.joiners);
        let mut pending = Vec::new();
        let mut finished = true;
        for rx in joiners.drain(..) {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                pending.push(rx);
                finished = false;
                continue;
            }
            match rx.recv_timeout(remaining) {
                Ok(()) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    pending.push(rx);
                    finished = false;
                }
            }
        }
        *joiners = pending;
        finished
    }

    fn report(&self, joined: bool) -> ShutdownReport {
        ShutdownReport {
            pending_jobs: self.shared.metrics.pending(),
            in_flight: self.shared.metrics.in_flight(),
            permits: self.shared.metrics.permits(),
            tracked_jobs: self.shared.admission.tracked(),
            draining: self.shared.shut.load(Ordering::Acquire) && !joined,
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
            shared.admission.retire(&job.key);
            publish_terminal_status(&shared, &job, "cancelled", 0);
            continue;
        }
        let Some(token) = shared.admission.take_running(&job.key) else {
            publish_terminal_status(&shared, &job, "cancelled", 0);
            continue;
        };
        let parked = token.clone();
        *lock(&host.current) = Some(job.clone());
        *lock(&host.active_cancel) = Some(token);
        *lock(&host.receipt) = None;
        shared.metrics.begin_permit();
        shared.admission.begin(job.key.task_id);
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
            runtime.block_on(async {
                tokio::task::yield_now().await;
            });
        }
        let receipt = lock(&host.receipt).clone();
        publish_terminal(&shared, &job, outcome.is_ok(), receipt);
        *lock(&host.current) = None;
        *lock(&host.active_cancel) = None;
        shared.admission.retire(&job.key);
        shared.admission.end(job.key.task_id);
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

fn publish_terminal_status(shared: &Shared, job: &TransferRequest, status: &str, bytes: i64) {
    let _ = shared.events.publish_terminal(TransferEvent {
        key: job.key.clone(),
        kind: "terminal".to_string(),
        bytes,
        terminal_status: Some(status.to_string()),
    });
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
    publish_terminal_status(shared, job, &status, bytes);
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
        link_gate: shared.config.link_gate.clone(),
        commit_hold: shared.config.commit_hold.clone(),
        hooks: shared.config.hooks.clone(),
        admission: Arc::clone(&shared.admission),
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
