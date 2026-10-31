//! Generic proxy exchange and a native-owned response body.
//!
//! Binary bytes stay in a spool file under the scenario root. The guest receives
//! a typed body resource plus a bounded text view, never an image-sized int
//! array. The spool, cancel token and driver owner belong to the VM session:
//! reset closes the lease, cancels the token and reaps drivers before reuse.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use http_body_util::BodyExt;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use vm::resource::{CloseProgress, HostResource, ResourceCloseReason};
use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostFutureOutput, HostParamPassing, HostParamSchema,
    HostResourceType, HostState, HostStructField, HostStructSchema, HostTypeSchema, ResourceHandle,
    ResourceTypeKey, ResourceTypeSchema, Value, Vm, VmResult,
};

use crate::dto::HeaderEntry;
use crate::net::{open_get_with_options, DriverOwner, NativeProxyRoute, OpenOptions, ProxyKind};
use crate::observe::Metrics;
use crate::proxy_host::ProxyHost;
use crate::types::HostError;

pub const BODY_KEY: &str = "xeh.proxy.body";
const LEASE_KEY: &str = "xeh.proxy.lease";
const INLINE_LIMIT: usize = 64 * 1024;
const CHUNK: usize = 8 * 1024;
const SPOOL_DIR: &str = ".xeh-spool";

static OWNER_SEQ: AtomicU64 = AtomicU64::new(1);

/// Per-VM live cancel token and driver owner.
///
/// The token and drivers are replaced when the previous generation is cancelled
/// or the Core closes the lease. They are not a process-wide once-flag.
pub struct LiveGeneration {
    pub owner: u64,
    generation: AtomicU64,
    cancel: Mutex<CancellationToken>,
    drivers: Mutex<Arc<DriverOwner>>,
}

impl LiveGeneration {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            owner: OWNER_SEQ.fetch_add(1, Ordering::AcqRel),
            generation: AtomicU64::new(1),
            cancel: Mutex::new(CancellationToken::new()),
            drivers: Mutex::new(Arc::new(DriverOwner::new())),
        })
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    pub fn cancel_current(&self) {
        self.cancel.lock().expect("proxy generation").cancel();
    }

    fn current(&self) -> (CancellationToken, Arc<DriverOwner>, u64) {
        let cancel = self.cancel.lock().expect("proxy generation").clone();
        let drivers = Arc::clone(&self.drivers.lock().expect("proxy generation"));
        (cancel, drivers, self.generation())
    }

    /// Replaces a cancelled generation. A still-live token is left alone so a
    /// second call does not retire in-flight drivers.
    fn refresh_if_cancelled(&self) -> bool {
        let cancelled = self.cancel.lock().expect("proxy generation").is_cancelled();
        if !cancelled {
            return false;
        }
        *self.cancel.lock().expect("proxy generation") = CancellationToken::new();
        *self.drivers.lock().expect("proxy generation") = Arc::new(DriverOwner::new());
        self.generation.fetch_add(1, Ordering::AcqRel);
        true
    }
}

pub struct ProxySession {
    pub root: PathBuf,
    spool_seq: AtomicU64,
}

impl ProxySession {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            root,
            spool_seq: AtomicU64::new(1),
        })
    }

    fn next_spool(&self) -> PathBuf {
        let seq = self.spool_seq.fetch_add(1, Ordering::AcqRel);
        self.root.join(SPOOL_DIR).join(format!("{seq}.spool"))
    }
}

/// Deletes the partial spool this operation created, unless the body was
/// handed to a resource. A preexisting file is never armed, so drop cannot
/// truncate or remove it.
struct SpoolGuard {
    path: PathBuf,
    armed: bool,
}

impl SpoolGuard {
    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for SpoolGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

pub struct BodyHandle {
    path: PathBuf,
    generation: u64,
    length: u64,
    owner: u64,
    live: Arc<LiveGeneration>,
    cancel: CancellationToken,
    closed: bool,
}

impl BodyHandle {
    fn live(&self) -> Result<(), &'static str> {
        if self.closed {
            return Err("closed");
        }
        if self.owner != self.live.owner {
            return Err("wrong_owner");
        }
        if self.generation != self.live.generation() {
            return Err("stale");
        }
        Ok(())
    }
}

impl HostResource for BodyHandle {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new(BODY_KEY).expect("body key"))
    }

    fn begin_close(
        &mut self,
        _reason: ResourceCloseReason,
    ) -> vm::resource::ResourceResult<CloseProgress> {
        self.closed = true;
        let _ = std::fs::remove_file(&self.path);
        Ok(CloseProgress::Ready)
    }
}

impl HostResourceType for BodyHandle {
    const KEY: &'static str = BODY_KEY;
    const DESCRIPTION: &'static str = "Native-owned HTTP response body spool";
}

struct ProxyLease {
    live: Arc<LiveGeneration>,
    reap: Option<Pin<Box<dyn std::future::Future<Output = bool> + Send>>>,
}

/// Remembers the lease handle for this VM. Reset closes the resource; the slot
/// survives, so the next exchange detects a dead handle and installs a fresh one.
struct LeaseSlot {
    handle: u64,
}

impl HostState for LeaseSlot {
    const KEY: &'static str = "xeh.proxy.lease-slot";

    fn initialize() -> Result<Self, String> {
        Ok(Self { handle: 0 })
    }
}

impl HostResource for ProxyLease {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new(LEASE_KEY).expect("lease key"))
    }

    fn begin_close(
        &mut self,
        _reason: ResourceCloseReason,
    ) -> vm::resource::ResourceResult<CloseProgress> {
        self.live.cancel_current();
        self.live.generation.fetch_add(1, Ordering::AcqRel);
        let drivers = Arc::clone(&self.live.drivers.lock().expect("proxy generation"));
        let deadline = Instant::now() + Duration::from_secs(5);
        self.reap = Some(Box::pin(async move { drivers.reap(deadline).await }));
        Ok(CloseProgress::Pending)
    }

    fn poll_close(&mut self, cx: &mut Context<'_>) -> Poll<vm::resource::ResourceResult<()>> {
        let Some(reap) = self.reap.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        match reap.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(done) => {
                self.reap = None;
                if done {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Ready(Err(vm::resource::ResourceError::new(
                        vm::resource::ResourceErrorCode::ResourceCleanupFailed,
                        "proxy_lease",
                        "driver reap did not finish before the deadline",
                    )))
                }
            }
        }
    }
}

impl HostResourceType for ProxyLease {
    const KEY: &'static str = LEASE_KEY;
    const DESCRIPTION: &'static str = "Per-VM proxy cancel and driver owner";
}

pub fn ensure_lease(vm: &mut Vm, live: &Arc<LiveGeneration>) -> VmResult<()> {
    let existing = vm
        .host_context()
        .host_state::<LeaseSlot>()
        .map(|slot| slot.handle)
        .unwrap_or(0);
    let alive = ResourceHandle::from_raw(existing)
        .ok()
        .and_then(|handle| {
            vm.host_context()
                .borrow_resource::<ProxyLease>(handle)
                .ok()
                .map(|_| ())
        })
        .is_some();
    let refreshed = live.refresh_if_cancelled();
    if alive {
        let _ = refreshed;
        return Ok(());
    }
    let handle = vm
        .host_context()
        .push_resource(ProxyLease {
            live: Arc::clone(live),
            reap: None,
        })
        .map_err(|error| vm::VmError::HostError(error.to_string()))?;
    vm.host_context()
        .set_host_state(LeaseSlot {
            handle: handle.handle().raw(),
        })
        .map_err(|error| vm::VmError::HostError(error.to_string()))?;
    Ok(())
}

struct ExchangeDone {
    status: i64,
    url: String,
    headers: Vec<(String, String)>,
    error_kind: String,
    error_detail: String,
    path: PathBuf,
    byte_length: u64,
    text_valid: bool,
    body_text: String,
    text_error: String,
    generation: u64,
    guard: SpoolGuard,
}

struct ParsedExchange {
    url: String,
    method: String,
    headers: Vec<HeaderEntry>,
    body: Vec<u8>,
    route: Option<NativeProxyRoute>,
    follow_redirects: bool,
    raw_status: bool,
    text_budget: usize,
}

pub fn exchange_structs() -> Vec<HostStructSchema> {
    vec![
        HostStructSchema::new("ProxyExchange", exchange_fields()),
        HostStructSchema::new("ProxyTextView", text_fields()),
        HostStructSchema::new("ProxyInline", inline_fields()),
        HostStructSchema::new("ProxyStage", stage_fields()),
    ]
}

pub fn exchange_resources() -> Vec<ResourceTypeSchema> {
    vec![
        ResourceTypeSchema::new(
            ResourceTypeKey::new(BODY_KEY).expect("body key"),
            BodyHandle::DESCRIPTION,
        ),
        ResourceTypeSchema::new(
            ResourceTypeKey::new(LEASE_KEY).expect("lease key"),
            ProxyLease::DESCRIPTION,
        ),
    ]
}

fn body_type() -> HostTypeSchema {
    HostTypeSchema::Resource(ResourceTypeKey::new(BODY_KEY).expect("body key"))
}

fn exchange_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("status", HostTypeSchema::Int),
        HostStructField::new("url", HostTypeSchema::String),
        HostStructField::new(
            "header_names",
            HostTypeSchema::Array(Box::new(HostTypeSchema::String)),
        ),
        HostStructField::new(
            "header_values",
            HostTypeSchema::Array(Box::new(HostTypeSchema::String)),
        ),
        HostStructField::new("error_kind", HostTypeSchema::String),
        HostStructField::new("error_detail", HostTypeSchema::String),
        HostStructField::new("byte_length", HostTypeSchema::Int),
        HostStructField::new("text_valid", HostTypeSchema::Bool),
        HostStructField::new("body_text", HostTypeSchema::String),
        HostStructField::new("text_error", HostTypeSchema::String),
        HostStructField::new("body", body_type()),
    ]
}

fn text_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("valid", HostTypeSchema::Bool),
        HostStructField::new("text", HostTypeSchema::String),
        HostStructField::new("error", HostTypeSchema::String),
    ]
}

fn inline_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("ok", HostTypeSchema::Bool),
        HostStructField::new("error", HostTypeSchema::String),
        HostStructField::new(
            "bytes",
            HostTypeSchema::Array(Box::new(HostTypeSchema::Int)),
        ),
    ]
}

fn stage_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("ok", HostTypeSchema::Bool),
        HostStructField::new("error", HostTypeSchema::String),
        HostStructField::new("bytes", HostTypeSchema::Int),
    ]
}

fn named(name: &str, fields: Vec<HostStructField>) -> HostTypeSchema {
    HostTypeSchema::Named {
        name: name.to_string(),
        fields,
    }
}

pub fn exchange_functions() -> Vec<HostFunctionSchema> {
    let body = body_type();
    vec![
        HostFunctionSchema::with_return(
            "xeh_proxy::exchange",
            vec![
                HostParamSchema::value("url", HostTypeSchema::String),
                HostParamSchema::value("method", HostTypeSchema::String),
                HostParamSchema::value(
                    "header_names",
                    HostTypeSchema::Array(Box::new(HostTypeSchema::String)),
                ),
                HostParamSchema::value(
                    "header_values",
                    HostTypeSchema::Array(Box::new(HostTypeSchema::String)),
                ),
                HostParamSchema::value("data", HostTypeSchema::String),
                HostParamSchema::value("route_kind", HostTypeSchema::String),
                HostParamSchema::value("route_host", HostTypeSchema::String),
                HostParamSchema::value("route_port", HostTypeSchema::Int),
                HostParamSchema::value("trusted_remote_dns", HostTypeSchema::Bool),
                HostParamSchema::value("follow_redirects", HostTypeSchema::Bool),
                HostParamSchema::value("raw_status", HostTypeSchema::Bool),
                HostParamSchema::value("text_budget", HostTypeSchema::Int),
            ],
            named("ProxyExchange", exchange_fields()),
        )
        .with_description("One GET or POST with raw status and a native body resource"),
        HostFunctionSchema::with_return(
            "xeh_proxy::body_scan",
            vec![
                HostParamSchema::with_passing("body", body.clone(), HostParamPassing::Borrow),
                HostParamSchema::value("needle", HostTypeSchema::String),
            ],
            HostTypeSchema::Bool,
        )
        .with_description("Chunked literal scan of a native body; the needle may cross a chunk"),
        HostFunctionSchema::with_return(
            "xeh_proxy::body_text",
            vec![
                HostParamSchema::with_passing("body", body.clone(), HostParamPassing::Borrow),
                HostParamSchema::value("budget", HostTypeSchema::Int),
            ],
            named("ProxyTextView", text_fields()),
        )
        .with_description("Bounded UTF-8 view. Over-budget bodies are not copied into the guest"),
        HostFunctionSchema::with_return(
            "xeh_proxy::body_inline",
            vec![
                HostParamSchema::with_passing("body", body.clone(), HostParamPassing::Borrow),
                HostParamSchema::value("limit", HostTypeSchema::Int),
            ],
            named("ProxyInline", inline_fields()),
        )
        .with_description("Small-body inline bytes. Refuses anything above the caller limit"),
        HostFunctionSchema::with_return(
            "xeh_proxy::body_stage",
            vec![
                HostParamSchema::with_passing("body", body.clone(), HostParamPassing::Borrow),
                HostParamSchema::value("relative", HostTypeSchema::String),
            ],
            named("ProxyStage", stage_fields()),
        )
        .with_description("Copy a native body into the scenario root without entering the guest"),
        HostFunctionSchema::with_return(
            "xeh_proxy::body_close",
            vec![HostParamSchema::with_passing(
                "body",
                body,
                HostParamPassing::TakeOwned,
            )],
            HostTypeSchema::Int,
        )
        .with_description("Take and delete a native body spool"),
    ]
}

pub fn register_exchange(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    host: Arc<ProxyHost>,
) -> Result<(), HostError> {
    for name in [
        "xeh_proxy::exchange",
        "xeh_proxy::body_scan",
        "xeh_proxy::body_text",
        "xeh_proxy::body_inline",
        "xeh_proxy::body_stage",
        "xeh_proxy::body_close",
    ] {
        let schema = catalog
            .function(name)
            .ok_or_else(|| HostError::bind(format!("missing application schema {name}")))?
            .clone();
        let host = Arc::clone(&host);
        register_catalog_function(registry, catalog, name, schema, move || match name {
            "xeh_proxy::exchange" => Box::new(ExchangeFn {
                host: Arc::clone(&host),
            }),
            "xeh_proxy::body_scan" => Box::new(ScanFn),
            "xeh_proxy::body_text" => Box::new(TextFn),
            "xeh_proxy::body_inline" => Box::new(InlineFn),
            "xeh_proxy::body_stage" => Box::new(StageFn {
                root: host.session.root.clone(),
            }),
            _ => Box::new(CloseFn),
        })
        .map_err(|error| HostError::bind(error.to_string()))?;
    }
    Ok(())
}

struct ExchangeFn {
    host: Arc<ProxyHost>,
}

impl HostFunction for ExchangeFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        ensure_lease(vm, &self.host.live)?;
        let parsed =
            parse_exchange(args).map_err(|error| vm::VmError::HostError(error.to_string()))?;
        let host = Arc::clone(&self.host);
        let (cancel, drivers, generation) = host.live.current();
        let future = async move {
            let mut done = run_exchange(&host, parsed, cancel, drivers, generation).await;
            Ok(HostFutureOutput::complete(move |vm| {
                let handed = done.error_kind.is_empty();
                let token = vm
                    .host_context()
                    .push_resource(BodyHandle {
                        path: done.path.clone(),
                        generation: done.generation,
                        length: done.byte_length,
                        owner: host.live.owner,
                        live: Arc::clone(&host.live),
                        cancel: host.live.current().0,
                        closed: false,
                    })
                    .map_err(|error| vm::VmError::HostError(error.to_string()))?;
                if handed {
                    done.guard.disarm();
                }
                Ok(CallReturn::one(exchange_value(&done, token.handle())))
            }))
        };
        vm.submit_host_future(Box::pin(future))
    }
}

fn exchange_value(done: &ExchangeDone, handle: ResourceHandle) -> Value {
    let names = done
        .headers
        .iter()
        .map(|(name, _)| Value::string(name.clone()))
        .collect();
    let values = done
        .headers
        .iter()
        .map(|(_, value)| Value::string(value.clone()))
        .collect();
    Value::map(vec![
        (Value::string("status"), Value::Int(done.status)),
        (Value::string("url"), Value::string(done.url.clone())),
        (Value::string("header_names"), Value::array(names)),
        (Value::string("header_values"), Value::array(values)),
        (
            Value::string("error_kind"),
            Value::string(done.error_kind.clone()),
        ),
        (
            Value::string("error_detail"),
            Value::string(done.error_detail.clone()),
        ),
        (
            Value::string("byte_length"),
            Value::Int(done.byte_length as i64),
        ),
        (Value::string("text_valid"), Value::Bool(done.text_valid)),
        (
            Value::string("body_text"),
            Value::string(done.body_text.clone()),
        ),
        (
            Value::string("text_error"),
            Value::string(done.text_error.clone()),
        ),
        (Value::string("body"), Value::Int(handle.raw() as i64)),
    ])
}

async fn run_exchange(
    host: &ProxyHost,
    mut request: ParsedExchange,
    cancel: CancellationToken,
    drivers: Arc<DriverOwner>,
    generation: u64,
) -> ExchangeDone {
    let path = host.session.next_spool();
    let fail = |kind: &str, detail: &str, armed: bool| ExchangeDone {
        status: 0,
        url: request.url.clone(),
        headers: Vec::new(),
        error_kind: kind.to_string(),
        error_detail: detail.to_string(),
        path: path.clone(),
        byte_length: 0,
        text_valid: false,
        body_text: String::new(),
        text_error: String::new(),
        generation,
        guard: SpoolGuard {
            path: path.clone(),
            armed,
        },
    };
    if let Some(route) = request.route.as_mut() {
        match grant_remote_dns(&host.grants, route) {
            Some(remote_dns) => route.trusted_remote_dns = remote_dns,
            None => {
                return fail(
                    "proxy_not_granted",
                    "proxy endpoint is not in the trusted grant",
                    false,
                );
            }
        }
    }
    let started = Instant::now();
    let mut opened = match open_get_with_options(
        &host.http,
        &request.url,
        &request.headers,
        &cancel,
        &Metrics::default(),
        &host.hooks,
        &drivers,
        started,
        OpenOptions {
            route: request.route.as_ref(),
            follow_redirects: request.follow_redirects,
            allow_redirect_response: !request.follow_redirects,
            return_raw_status: request.raw_status,
            method: &request.method,
            body: &request.body,
        },
    )
    .await
    {
        Ok(opened) => opened,
        Err(code) => return fail(classify(code), code, false),
    };
    if let Err(code) = create_reserved_spool(&path) {
        return fail("ConnectionError", code, false);
    }
    let guard = SpoolGuard {
        path: path.clone(),
        armed: true,
    };
    let deadline = started + host.http.request_timeout.min(host.http.max_stream_duration);
    let cap = host.http.max_stream_total_bytes;
    let spooled = spool_body(&mut opened, &path, &cancel, deadline, cap).await;
    let _ = opened
        .connection
        .shutdown(crate::net::cleanup_deadline(deadline))
        .await;
    let byte_length = match spooled {
        Ok(length) => length,
        Err(code) => return fail("ConnectionError", code, false),
    };
    let (text_valid, body_text, text_error) = if request.text_budget == 0 {
        (false, String::new(), String::new())
    } else {
        match read_text(&path, request.text_budget, &cancel) {
            Ok((valid, text)) => (valid, text, String::new()),
            Err("text_budget") => (false, String::new(), "text_budget".to_string()),
            Err("invalid_utf8") => (false, String::new(), "invalid_utf8".to_string()),
            Err(other) => (false, String::new(), other.to_string()),
        }
    };
    ExchangeDone {
        status: i64::from(opened.status),
        url: opened.url,
        headers: opened.headers,
        error_kind: String::new(),
        error_detail: String::new(),
        path: path.clone(),
        byte_length,
        text_valid,
        body_text,
        text_error,
        generation,
        guard,
    }
}

fn grant_remote_dns(
    grants: &[crate::task_control::ProxyGrant],
    route: &NativeProxyRoute,
) -> Option<bool> {
    for grant in grants {
        let Some((kind, host, port)) = parse_grant(&grant.address) else {
            continue;
        };
        if kind == route.kind && host == route.host && port == route.port {
            return Some(grant.remote_dns);
        }
    }
    None
}

fn parse_grant(address: &str) -> Option<(ProxyKind, String, u16)> {
    let (scheme, rest) = address.split_once("://")?;
    let kind = ProxyKind::from_code(scheme)?;
    let (host, port_text) = rest.rsplit_once(':')?;
    let port = port_text.parse().ok()?;
    if host.is_empty() {
        return None;
    }
    Some((kind, host.to_string(), port))
}

fn create_reserved_spool(path: &Path) -> Result<(), &'static str> {
    let Some(parent) = path.parent() else {
        return Err("io_error");
    };
    if parent.exists() && !parent.is_dir() {
        return Err("io_error");
    }
    std::fs::create_dir_all(parent).map_err(|_| "io_error")?;
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "io_error")?;
    Ok(())
}

fn classify(code: &str) -> &'static str {
    match code {
        "deadline" => "ConnectTimeout",
        "proxy_unsupported" => "ProxySchemeUnknown",
        "policy_denied" => "policy_denied",
        "header_rejected" => "header_rejected",
        "route_invalid" => "route_invalid",
        "method_rejected" => "method_rejected",
        "proxy_not_granted" => "proxy_not_granted",
        "proxy_status" | "proxy_handshake_failed" | "proxy_auth_required" => "ProxyError",
        "redirect_limit" => "TooManyRedirects",
        _ => "ConnectionError",
    }
}

async fn spool_body(
    opened: &mut crate::net::OpenedResponse,
    path: &Path,
    cancel: &CancellationToken,
    deadline: Instant,
    cap: usize,
) -> Result<u64, &'static str> {
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .open(path)
        .await
        .map_err(|_| "io_error")?;
    let mut total = 0usize;
    loop {
        if cancel.is_cancelled() {
            return Err("cancelled");
        }
        if Instant::now() >= deadline {
            return Err("deadline");
        }
        let frame = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err("cancelled"),
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                return Err("deadline");
            }
            frame = opened.body.frame() => frame,
        };
        match frame {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    total = total.saturating_add(data.len());
                    if total > cap {
                        return Err("body_limit");
                    }
                    file.write_all(&data).await.map_err(|_| "io_error")?;
                }
            }
            None => break,
            Some(Err(_)) => return Err("transport_failed"),
        }
    }
    file.flush().await.map_err(|_| "io_error")?;
    Ok(total as u64)
}

fn read_text(
    path: &Path,
    budget: usize,
    cancel: &CancellationToken,
) -> Result<(bool, String), &'static str> {
    if cancel.is_cancelled() {
        return Err("cancelled");
    }
    let len = std::fs::metadata(path).map_err(|_| "io_error")?.len() as usize;
    if len > budget {
        return Err("text_budget");
    }
    if cancel.is_cancelled() {
        return Err("cancelled");
    }
    let bytes = std::fs::read(path).map_err(|_| "io_error")?;
    match std::str::from_utf8(&bytes) {
        Ok(text) => Ok((true, text.to_string())),
        Err(_) => Err("invalid_utf8"),
    }
}

pub fn scan_file(
    path: &Path,
    needle: &str,
    cancel: &CancellationToken,
) -> Result<bool, &'static str> {
    if needle.is_empty() {
        return Ok(false);
    }
    let needle = needle.as_bytes();
    let mut file = std::fs::File::open(path).map_err(|_| "io_error")?;
    let mut carry = Vec::new();
    let mut buf = vec![0u8; CHUNK];
    loop {
        if cancel.is_cancelled() {
            return Err("cancelled");
        }
        let read = std::io::Read::read(&mut file, &mut buf).map_err(|_| "io_error")?;
        if read == 0 {
            return Ok(false);
        }
        carry.extend_from_slice(&buf[..read]);
        if needle.len() <= carry.len() && carry.windows(needle.len()).any(|window| window == needle)
        {
            return Ok(true);
        }
        let keep = needle.len().saturating_sub(1);
        if carry.len() > keep {
            carry.drain(..carry.len() - keep);
        }
    }
}

fn decode_handle(value: Option<&Value>) -> VmResult<ResourceHandle> {
    let Some(Value::Int(raw)) = value else {
        return Err(vm::VmError::HostError(
            "body argument is not a handle".to_string(),
        ));
    };
    ResourceHandle::from_raw(*raw as u64).map_err(|error| vm::VmError::HostError(error.to_string()))
}

struct BodySnap {
    path: PathBuf,
    length: u64,
    cancel: CancellationToken,
}

fn snap_body(vm: &mut Vm, args: &[Value]) -> VmResult<BodySnap> {
    let handle = decode_handle(args.first())?;
    let ctx = vm.host_context();
    let body = ctx
        .borrow_resource::<BodyHandle>(handle)
        .map_err(|error| vm::VmError::HostError(error.to_string()))?;
    let inner = body.get();
    inner
        .live()
        .map_err(|error| vm::VmError::HostError(error.to_string()))?;
    Ok(BodySnap {
        path: inner.path.clone(),
        length: inner.length,
        cancel: inner.cancel.clone(),
    })
}

struct ScanFn;

impl HostFunction for ScanFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let needle = match args.get(1) {
            Some(Value::String(text)) => (**text).clone(),
            _ => {
                return Err(vm::VmError::HostError(
                    "needle must be a string".to_string(),
                ));
            }
        };
        let snap = snap_body(vm, args)?;
        let found = scan_file(&snap.path, &needle, &snap.cancel)
            .map_err(|error| vm::VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(Value::Bool(found))))
    }
}

struct TextFn;

impl HostFunction for TextFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let budget = match args.get(1) {
            Some(Value::Int(value)) if *value >= 0 => *value as usize,
            _ => return Err(vm::VmError::HostError("budget must be an int".to_string())),
        };
        let snap = snap_body(vm, args)?;
        let (valid, text, error) = match read_text(&snap.path, budget, &snap.cancel) {
            Ok((valid, text)) => (valid, text, String::new()),
            Err("text_budget") => (false, String::new(), "text_budget".to_string()),
            Err("invalid_utf8") => (false, String::new(), "invalid_utf8".to_string()),
            Err(code) => (false, String::new(), code.to_string()),
        };
        Ok(CallOutcome::Return(CallReturn::one(Value::map(vec![
            (Value::string("valid"), Value::Bool(valid)),
            (Value::string("text"), Value::string(text)),
            (Value::string("error"), Value::string(error)),
        ]))))
    }
}

struct InlineFn;

impl HostFunction for InlineFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let limit = match args.get(1) {
            Some(Value::Int(value)) if *value >= 0 => (*value as usize).min(INLINE_LIMIT),
            _ => return Err(vm::VmError::HostError("limit must be an int".to_string())),
        };
        let snap = snap_body(vm, args)?;
        if snap.cancel.is_cancelled() {
            return Ok(CallOutcome::Return(CallReturn::one(Value::map(vec![
                (Value::string("ok"), Value::Bool(false)),
                (Value::string("error"), Value::string("cancelled")),
                (Value::string("bytes"), Value::array(Vec::new())),
            ]))));
        }
        let outcome = if snap.length as usize > limit {
            Err("inline_refused")
        } else {
            std::fs::read(&snap.path).map_err(|_| "io_error")
        };
        let (ok, error, bytes) = match outcome {
            Ok(bytes) => (true, String::new(), bytes),
            Err(error) => (false, error.to_string(), Vec::new()),
        };
        let values = bytes
            .into_iter()
            .map(|byte| Value::Int(i64::from(byte)))
            .collect();
        Ok(CallOutcome::Return(CallReturn::one(Value::map(vec![
            (Value::string("ok"), Value::Bool(ok)),
            (Value::string("error"), Value::string(error)),
            (Value::string("bytes"), Value::array(values)),
        ]))))
    }
}

struct StageFn {
    root: PathBuf,
}

impl HostFunction for StageFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let relative = match args.get(1) {
            Some(Value::String(text)) => (**text).clone(),
            _ => {
                return Err(vm::VmError::HostError(
                    "relative path must be a string".to_string(),
                ));
            }
        };
        let snap = snap_body(vm, args)?;
        let outcome = stage_copy(&snap.path, &self.root, &relative, &snap.cancel);
        let (ok, error, bytes) = match outcome {
            Ok(bytes) => (true, String::new(), bytes as i64),
            Err(error) => (false, error, 0),
        };
        Ok(CallOutcome::Return(CallReturn::one(Value::map(vec![
            (Value::string("ok"), Value::Bool(ok)),
            (Value::string("error"), Value::string(error)),
            (Value::string("bytes"), Value::Int(bytes)),
        ]))))
    }
}

fn stage_copy(
    spool: &Path,
    root: &Path,
    relative: &str,
    cancel: &CancellationToken,
) -> Result<u64, String> {
    if relative
        .split(['/', '\\'])
        .any(|part| part == SPOOL_DIR || part == "..")
    {
        return Err("stage_path".to_string());
    }
    let dest = crate::net::resolve_inside(root, relative).map_err(|error| error.to_string())?;
    if dest.exists() {
        return Err("target_exists".to_string());
    }
    let mut output = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&dest)
        .map_err(|_| "io_error".to_string())?;
    let copy_result = (|| {
        let mut input = std::fs::File::open(spool).map_err(|_| "io_error".to_string())?;
        let mut copied = 0u64;
        let mut buf = vec![0u8; CHUNK];
        loop {
            if cancel.is_cancelled() {
                return Err("cancelled".to_string());
            }
            let read =
                std::io::Read::read(&mut input, &mut buf).map_err(|_| "io_error".to_string())?;
            if read == 0 {
                break;
            }
            std::io::Write::write_all(&mut output, &buf[..read])
                .map_err(|_| "io_error".to_string())?;
            copied += read as u64;
        }
        output.sync_all().map_err(|_| "io_error".to_string())?;
        let expected = std::fs::metadata(spool)
            .map_err(|_| "io_error".to_string())?
            .len();
        if copied != expected {
            return Err("short_copy".to_string());
        }
        Ok(copied)
    })();
    match copy_result {
        Ok(bytes) => Ok(bytes),
        Err(error) => {
            let _ = std::fs::remove_file(&dest);
            Err(error)
        }
    }
}

struct CloseFn;

impl HostFunction for CloseFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let handle = decode_handle(args.first())?;
        let mut body = vm
            .host_context()
            .take_resource::<BodyHandle>(handle)
            .map_err(|error| vm::VmError::HostError(error.to_string()))?;
        body.closed = true;
        let _ = std::fs::remove_file(&body.path);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

fn string_arg(args: &[Value], index: usize) -> Result<String, &'static str> {
    match args.get(index) {
        Some(Value::String(text)) => Ok((**text).clone()),
        _ => Err("exchange string argument"),
    }
}

fn strings_arg(args: &[Value], index: usize) -> Result<Vec<String>, &'static str> {
    let Some(Value::Array(items)) = args.get(index) else {
        return Err("exchange header array");
    };
    let mut output = Vec::with_capacity(items.len());
    for item in items.iter() {
        let Value::String(text) = item else {
            return Err("exchange header array");
        };
        output.push((**text).clone());
    }
    Ok(output)
}

fn parse_exchange(args: &[Value]) -> Result<ParsedExchange, &'static str> {
    let url = string_arg(args, 0)?;
    let method = string_arg(args, 1)?;
    if method != "GET" && method != "POST" {
        return Err("method_rejected");
    }
    let names = strings_arg(args, 2)?;
    let values = strings_arg(args, 3)?;
    if names.len() != values.len() {
        return Err("exchange header arrays differ in length");
    }
    let headers = names
        .into_iter()
        .zip(values)
        .map(|(name, value)| HeaderEntry {
            name: name.into_bytes(),
            value: value.into_bytes(),
        })
        .collect();
    let data = string_arg(args, 4)?;
    let route_kind = string_arg(args, 5)?;
    let route_host = string_arg(args, 6)?;
    let route_port = match args.get(7) {
        Some(Value::Int(value)) => u16::try_from(*value).map_err(|_| "route_invalid")?,
        _ => return Err("route_invalid"),
    };
    let trusted = matches!(args.get(8), Some(Value::Bool(true)));
    let follow_redirects = matches!(args.get(9), Some(Value::Bool(true)));
    let raw_status = matches!(args.get(10), Some(Value::Bool(true)));
    let text_budget = match args.get(11) {
        Some(Value::Int(value)) if *value >= 0 => *value as usize,
        _ => 0,
    };
    let route = if route_kind == "direct" {
        None
    } else {
        let Some(proxy_kind) = ProxyKind::from_code(&route_kind) else {
            return Err("route_invalid");
        };
        if proxy_kind == ProxyKind::Unsupported {
            return Err("proxy_unsupported");
        }
        Some(NativeProxyRoute::new(
            proxy_kind, route_host, route_port, trusted,
        )?)
    };
    Ok(ParsedExchange {
        url,
        method,
        headers,
        body: data.into_bytes(),
        route,
        follow_redirects,
        raw_status,
        text_budget,
    })
}

pub fn child_cancel(host: &ProxyHost) -> CancellationToken {
    host.live
        .cancel
        .lock()
        .expect("proxy generation")
        .child_token()
}

pub fn session_owner(host: &ProxyHost) -> Arc<DriverOwner> {
    Arc::clone(&host.live.drivers.lock().expect("proxy generation"))
}

pub fn admit_url(http: &vm::HttpConfig, url: &str) -> Result<(), &'static str> {
    let parsed = url::Url::parse(url).map_err(|_| "policy_denied")?;
    crate::net::policy::admit_remote_target(http, &parsed).map(|_| ())
}
