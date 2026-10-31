//! Worker permits are the pool's in-flight count, not a guest counter.
//!
//! `WorkerPool` takes a permit at `begin_permit` when a worker starts a job and
//! returns it at `end_permit` when that worker is reaped. Pause and exit cancel
//! the job and wait for that return. A start that never reaches a worker reports
//! zero. A second start while a permit is held does not submit again.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostParamSchema, HostTypeSchema, HttpConfig, Value,
    Vm, VmError, VmResult,
};

use crate::dto::{lock, CommitGate, JobKey, PoolConfig, TaskRegistration, TransferRequest, TransportHooks};
use crate::types::HostError;
use crate::worker::WorkerPool;

#[derive(Clone)]
pub struct PermitState {
    inner: Arc<Mutex<PermitInner>>,
}

struct PermitInner {
    root: PathBuf,
    lease: Option<Lease>,
}

struct Lease {
    pool: Arc<WorkerPool>,
    hold: Arc<CommitGate>,
    port: u16,
    registration: Option<TaskRegistration>,
    keys: Vec<JobKey>,
    launched: bool,
    stop: Arc<AtomicBool>,
    accept: Option<JoinHandle<()>>,
}

impl PermitState {
    fn new(root: PathBuf) -> Self {
        Self {
            inner: Arc::new(Mutex::new(PermitInner { root, lease: None })),
        }
    }

    fn shutdown(&self) {
        let lease = lock(&self.inner).lease.take();
        if let Some(mut lease) = lease {
            lease.stop.store(true, Ordering::Release);
            let _ = lease.pool.shutdown_within(Duration::from_secs(15));
            if let Some(handle) = lease.accept.take() {
                let _ = handle.join();
            }
        }
    }
}

pub fn install(vm: &mut Vm, root: PathBuf) {
    vm.host_context().set_module_state(PermitState::new(root));
}

pub fn shutdown(vm: &mut Vm) {
    if let Ok(state) = state_of(vm) {
        state.shutdown();
    }
}

pub fn functions() -> Vec<HostFunctionSchema> {
    vec![
        HostFunctionSchema::with_return(
            "xeh_worker::start",
            vec![HostParamSchema::value("count", HostTypeSchema::Int)],
            HostTypeSchema::Int,
        )
        .with_description("Submit pool jobs and return permits actually taken by workers"),
        HostFunctionSchema::with_return("xeh_worker::reap", Vec::new(), HostTypeSchema::Int)
            .with_description("Cancel running jobs and return permits after workers are reaped"),
        HostFunctionSchema::with_return("xeh_worker::permits", Vec::new(), HostTypeSchema::Int)
            .with_description("Read WorkerPool permits taken at begin_permit"),
    ]
}

pub fn register(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> Result<(), HostError> {
    register_one(registry, catalog, "xeh_worker::start", || Box::new(StartFn))?;
    register_one(registry, catalog, "xeh_worker::reap", || Box::new(ReapFn))?;
    register_one(registry, catalog, "xeh_worker::permits", || Box::new(PermitsFn))?;
    Ok(())
}

fn register_one<F>(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    factory: F,
) -> Result<(), HostError>
where
    F: Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
{
    let schema = catalog
        .function(name)
        .ok_or_else(|| HostError::bind(format!("missing worker permit schema {name}")))?
        .clone();
    register_catalog_function(registry, catalog, name, schema, factory)
        .map_err(|error| HostError::bind(error.to_string()))
}

fn state_of(vm: &mut Vm) -> Result<PermitState, VmError> {
    vm.host_context()
        .module_state::<PermitState>()
        .cloned()
        .ok_or_else(|| VmError::HostError("xeh_worker permits are not installed for this VM".to_string()))
}

fn count_arg(value: Option<&Value>) -> Result<usize, VmError> {
    match value {
        Some(Value::Int(count)) if *count >= 0 => Ok(*count as usize),
        _ => Err(VmError::HostError(
            "xeh_worker::start count must be a non-negative int".to_string(),
        )),
    }
}

fn held_permits(inner: &PermitInner) -> i64 {
    inner
        .lease
        .as_ref()
        .map(|lease| lease.pool.permits() as i64)
        .unwrap_or(0)
}

struct StartFn;

impl HostFunction for StartFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let count = count_arg(args.first())?;
        let held = launch(&state, count)?;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(held))))
    }
}

struct ReapFn;

impl HostFunction for ReapFn {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let held = reap(&state)?;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(held))))
    }
}

struct PermitsFn;

impl HostFunction for PermitsFn {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let held = held_permits(&lock(&state.inner));
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(held))))
    }
}

fn launch(state: &PermitState, count: usize) -> Result<i64, VmError> {
    if count == 0 {
        let mut inner = lock(&state.inner);
        if let Some(lease) = inner.lease.as_mut() {
            lease.launched = true;
        }
        return Ok(held_permits(&inner));
    }
    ensure_pool(state, count)?;
    let (pool, hold, port, launched, already) = {
        let inner = lock(&state.inner);
        let lease = inner
            .lease
            .as_ref()
            .ok_or_else(|| VmError::HostError("worker pool is not running".to_string()))?;
        (
            Arc::clone(&lease.pool),
            Arc::clone(&lease.hold),
            lease.port,
            lease.launched,
            lease.pool.permits(),
        )
    };
    if launched || already > 0 {
        return Ok(pool.permits() as i64);
    }
    let registration = pool
        .register(1, 1)
        .map_err(|error| VmError::HostError(format!("worker register failed: {}", error.code())))?;
    let mut keys = Vec::with_capacity(count);
    for job in 1..=count {
        let key = JobKey {
            task_id: 1,
            epoch: 1,
            job_id: job as i64,
        };
        let request = TransferRequest {
            key: key.clone(),
            url: format!("http://127.0.0.1:{port}/lease-{job}"),
            headers: Vec::new(),
            relative_path: format!("lease-{job}.bin"),
        };
        pool.submit(registration.submission(request))
            .map_err(|error| VmError::HostError(format!("worker submit failed: {}", error.code())))?;
        keys.push(key);
    }
    hold.wait_arrived(count, Duration::from_secs(30))
        .map_err(|error| VmError::HostError(format!("workers did not take permits: {}", error.code())))?;
    let held = pool.permits();
    {
        let mut inner = lock(&state.inner);
        if let Some(lease) = inner.lease.as_mut() {
            lease.registration = Some(registration);
            lease.keys = keys;
            lease.launched = held > 0;
        }
    }
    Ok(held as i64)
}

fn reap(state: &PermitState) -> Result<i64, VmError> {
    let (pool, registration, keys) = {
        let inner = lock(&state.inner);
        let Some(lease) = inner.lease.as_ref() else {
            return Ok(0);
        };
        (
            Arc::clone(&lease.pool),
            lease.registration.clone(),
            lease.keys.clone(),
        )
    };
    if let Some(registration) = registration {
        for key in &keys {
            let _ = pool.cancel(&registration, key);
        }
    }
    let deadline = Instant::now() + Duration::from_secs(15);
    while pool.permits() > 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    if !keys.is_empty() {
        let _ = pool.wait_terminals(keys.len(), Duration::from_secs(5));
    }
    Ok(pool.permits() as i64)
}

fn ensure_pool(state: &PermitState, count: usize) -> Result<(), VmError> {
    if lock(&state.inner).lease.is_some() {
        return Ok(());
    }
    let root = lock(&state.inner).root.clone();
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| VmError::HostError(format!("permit listener failed: {error}")))?;
    let port = listener
        .local_addr()
        .map_err(|error| VmError::HostError(format!("permit listener has no port: {error}")))?
        .port();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_thread = Arc::clone(&stop);
    let accept = thread::Builder::new()
        .name("xeh-permit-accept".to_string())
        .spawn(move || accept_loop(listener, stop_thread))
        .map_err(|error| VmError::HostError(format!("permit accept thread failed: {error}")))?;
    let hold = CommitGate::new();
    let http = http_config(port)?;
    let config = PoolConfig {
        workers: count,
        job_capacity: count.max(1),
        event_capacity: count.max(4),
        http,
        root,
        chunk_bytes: 64,
        stream_limit: HttpConfig::default().max_stream_total_bytes,
        writer_fault: None,
        commit_gate: None,
        link_gate: None,
        commit_hold: None,
        hooks: TransportHooks {
            buffer_hold: Some(Arc::clone(&hold)),
            ..TransportHooks::default()
        },
        fail_worker_once: false,
        submit_hold: None,
    };
    let pool = match WorkerPool::start(config) {
        Ok(pool) => Arc::new(pool),
        Err(error) => {
            stop.store(true, Ordering::Release);
            let _ = accept.join();
            return Err(VmError::HostError(format!(
                "worker pool did not start: {}",
                error.code()
            )));
        }
    };
    if let Err(error) = pool.wait_ready(Duration::from_secs(90)) {
        stop.store(true, Ordering::Release);
        let _ = pool.shutdown_within(Duration::from_secs(15));
        let _ = accept.join();
        return Err(VmError::HostError(format!(
            "worker pool was not ready: {}",
            error.code()
        )));
    }
    let mut inner = lock(&state.inner);
    if inner.lease.is_none() {
        inner.lease = Some(Lease {
            pool,
            hold,
            port,
            registration: None,
            keys: Vec::new(),
            launched: false,
            stop,
            accept: Some(accept),
        });
    }
    Ok(())
}

fn http_config(port: u16) -> Result<HttpConfig, VmError> {
    let http = HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(20),
        stream_idle_timeout: Duration::from_secs(5),
        max_stream_duration: Duration::from_secs(20),
        ..HttpConfig::default()
    };
    http.validate()
        .map_err(|error| VmError::HostError(format!("permit http policy rejected: {error}")))?;
    Ok(http)
}

fn accept_loop(listener: TcpListener, stop: Arc<AtomicBool>) {
    let _ = listener.set_nonblocking(true);
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                thread::spawn(move || write_ok(stream));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(20));
            }
            Err(_) => break,
        }
    }
}

fn write_ok(mut stream: std::net::TcpStream) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    let mut buf = [0u8; 1024];
    let _ = stream.read(&mut buf);
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
}
