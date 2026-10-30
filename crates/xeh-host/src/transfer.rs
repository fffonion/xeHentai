use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;
use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostFutureOutput, HostParamSchema, HostStructField,
    HostStructSchema, HostTypeSchema, Value, Vm, VmResult,
};

use crate::catalog::standard_catalog;
use crate::dto::{lock, HeaderEntry, JobKey, NativeSubmission, TransferReceipt, TransferRequest};
use crate::net::{write_body, BodyWrite, DriverOwner};
use crate::observe::Metrics;
use crate::types::HostError;
use crate::worker::events::EventBus;

/// Per-worker host state. The current job DTO is installed by the owner thread
/// before `drive_to_halt` and is never sent to another thread as a `Value`.
pub struct TransferHost {
    pub installed: Mutex<Option<NativeSubmission>>,
    pub receipt: Mutex<Option<TransferReceipt>>,
    pub http: vm::HttpConfig,
    pub root: PathBuf,
    pub chunk_bytes: usize,
    pub stream_limit: usize,
    pub writer_fault: Option<crate::dto::WriterFault>,
    pub commit_gate: Option<Arc<crate::dto::CommitGate>>,
    pub link_gate: Option<Arc<crate::dto::CommitGate>>,
    pub commit_hold: Option<Arc<crate::dto::CommitGate>>,
    pub hooks: crate::dto::TransportHooks,
    pub admission: Arc<crate::worker::cancellation::Admission>,
    pub metrics: Arc<Metrics>,
    pub events: Arc<EventBus>,
    pub shut: Arc<AtomicBool>,
    pub active_cancel: Mutex<Option<CancellationToken>>,
    pub drivers: Arc<DriverOwner>,
}

pub fn transfer_catalog() -> Arc<HostApiCatalog> {
    let standard = standard_catalog();
    let mut builder = HostApiCatalog::builder();
    for function in standard.functions() {
        builder.function(function.clone());
    }
    for resource in standard.resources() {
        builder.resource(resource.clone());
    }
    for schema in standard.structs() {
        builder.named_struct(schema.clone());
    }
    for schema in transfer_structs() {
        builder.named_struct(schema);
    }
    for function in transfer_functions() {
        builder.function(function);
    }
    Arc::new(builder.build().expect("transfer catalog"))
}

pub fn register_transfer(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    host: Arc<TransferHost>,
) -> Result<(), HostError> {
    register_one(registry, catalog, "xeh_transfer::job", {
        let host = Arc::clone(&host);
        move || {
            Box::new(JobFn {
                host: Arc::clone(&host),
            })
        }
    })?;
    register_one(registry, catalog, "xeh_transfer::run", move || {
        Box::new(RunFn {
            host: Arc::clone(&host),
        })
    })?;
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
        .ok_or_else(|| HostError::bind(format!("missing transfer schema {name}")))?
        .clone();
    register_catalog_function(registry, catalog, name, schema, factory)
        .map_err(|error| HostError::bind(error.to_string()))
}

struct JobFn {
    host: Arc<TransferHost>,
}

impl HostFunction for JobFn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let request = lock(&self.host.installed)
            .as_ref()
            .map(|submission| submission.request().clone())
            .ok_or_else(|| vm::VmError::HostError("transfer job is not installed".to_string()))?;
        let value = request_to_value(&request);
        self.host.metrics.set_guest_payload(payload_len(&value));
        Ok(CallOutcome::Return(CallReturn::one(value)))
    }
}

struct RunFn {
    host: Arc<TransferHost>,
}

impl HostFunction for RunFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let guest = parse_request(args.first())
            .map_err(|error| vm::VmError::HostError(error.kind().to_string()))?;
        let installed = lock(&self.host.installed)
            .clone()
            .ok_or_else(|| vm::VmError::HostError("transfer job is not installed".to_string()))?;
        if guest.key != *installed.key() {
            return Err(vm::VmError::HostError("stale_epoch".to_string()));
        }
        let cancel = lock(&self.host.active_cancel).clone().unwrap_or_default();
        let host = Arc::clone(&self.host);
        let future = async move {
            let receipt = execute(&host, &installed, cancel).await;
            *lock(&host.receipt) = Some(receipt.clone());
            let metrics = Arc::clone(&host.metrics);
            Ok(HostFutureOutput::complete(move |_vm| {
                let value = receipt_to_value(&receipt);
                metrics.set_guest_payload(payload_len(&value));
                Ok(CallReturn::one(value))
            }))
        };
        vm.submit_host_future(Box::pin(future))
    }
}

async fn execute(
    host: &TransferHost,
    submission: &NativeSubmission,
    cancel: CancellationToken,
) -> TransferReceipt {
    host.metrics.begin_flight();
    let flight = FlightGuard {
        metrics: &host.metrics,
    };
    let request = submission.request();
    let result = execute_inner(host, submission, &cancel).await;
    drop(flight);
    match result {
        Ok(bytes) => TransferReceipt {
            key: request.key.clone(),
            status: "completed".to_string(),
            bytes,
            path: request.relative_path.clone(),
            error_code: None,
        },
        Err(code) => {
            host.metrics.record_failure(code);
            TransferReceipt {
                key: request.key.clone(),
                status: code.to_string(),
                bytes: 0,
                path: String::new(),
                error_code: Some(code.to_string()),
            }
        }
    }
}

async fn execute_inner(
    host: &TransferHost,
    submission: &NativeSubmission,
    cancel: &CancellationToken,
) -> Result<i64, &'static str> {
    let request = submission.request();
    if cancel.is_cancelled() || host.shut.load(Ordering::Acquire) {
        return Err("cancelled");
    }
    let started = std::time::Instant::now();
    let opened = crate::net::open_get_with_options(
        &host.http,
        &request.url,
        &request.headers,
        cancel,
        &host.metrics,
        &host.hooks,
        &host.drivers,
        started,
        crate::net::OpenOptions {
            route: submission.route(),
            follow_redirects: submission.follow_redirects(),
            allow_redirect_response: false,
        },
    )
    .await?;
    let deadline = started + host.http.request_timeout.min(host.http.max_stream_duration);
    let stream_limit = host.stream_limit.min(host.http.max_stream_total_bytes);
    let staged = write_body(
        opened,
        &host.root,
        &request.relative_path,
        request.key.job_id,
        BodyWrite {
            chunk_bytes: host.chunk_bytes,
            stream_limit,
            writer_fault: host.writer_fault.as_ref(),
            write_hold: host.hooks.write_hold.as_deref(),
            buffer_hold: host.hooks.buffer_hold.as_deref(),
            uncancellable_hold: host.hooks.uncancellable_hold.as_deref(),
            cancel,
            deadline,
            idle: host.http.stream_idle_timeout,
            metrics: &host.metrics,
        },
    )
    .await?;
    host.events.publish_progress(crate::dto::TransferEvent {
        key: request.key.clone(),
        kind: "progress".to_string(),
        bytes: i64::try_from(staged.bytes).unwrap_or(i64::MAX),
        terminal_status: None,
        identity: std::sync::Arc::clone(submission.identity()),
    });
    if let Some(gate) = &host.commit_gate {
        gate.arrive();
        if let Err(code) = gate.wait_release(cancel, &host.shut) {
            let _ = std::fs::remove_file(&staged.staging);
            return Err(code);
        }
    }
    if cancel.is_cancelled() || host.shut.load(Ordering::Acquire) {
        let _ = std::fs::remove_file(&staged.staging);
        return Err("cancelled");
    }
    if let Some(gate) = &host.link_gate {
        gate.arrive();
        if let Err(code) = gate.wait_release_only() {
            let _ = std::fs::remove_file(&staged.staging);
            return Err(code);
        }
    }
    if let Err(code) = host.admission.publish(
        submission,
        &staged.staging,
        &staged.final_path,
        host.commit_hold.clone(),
        deadline,
    ) {
        let _ = std::fs::remove_file(&staged.staging);
        return Err(code);
    }
    let _ = std::fs::remove_file(&staged.staging);
    i64::try_from(staged.bytes).map_err(|_| "io_error")
}

struct FlightGuard<'a> {
    metrics: &'a Metrics,
}

impl Drop for FlightGuard<'_> {
    fn drop(&mut self) {
        self.metrics.end_flight();
    }
}

fn transfer_structs() -> Vec<HostStructSchema> {
    vec![
        HostStructSchema::new("JobKey", job_key_fields()),
        HostStructSchema::new("HeaderEntry", header_fields()),
        HostStructSchema::new("TransferRequest", request_fields()),
        HostStructSchema::new("TransferReceipt", receipt_fields()),
    ]
}

fn transfer_functions() -> Vec<HostFunctionSchema> {
    vec![
        HostFunctionSchema::with_return("xeh_transfer::job", Vec::new(), request_type())
            .with_description("Read the installed transfer job"),
        HostFunctionSchema::with_return(
            "xeh_transfer::run",
            vec![HostParamSchema::value("request", request_type())],
            receipt_type(),
        )
        .with_description("Stream one transfer into the pool root"),
    ]
}

fn job_key_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("task_id", HostTypeSchema::Int),
        HostStructField::new("epoch", HostTypeSchema::Int),
        HostStructField::new("job_id", HostTypeSchema::Int),
    ]
}

fn header_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("name", HostTypeSchema::Bytes),
        HostStructField::new("value", HostTypeSchema::Bytes),
    ]
}

fn request_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("key", job_key_type()),
        HostStructField::new("url", HostTypeSchema::String),
        HostStructField::new("headers", HostTypeSchema::Array(Box::new(header_type()))),
        HostStructField::new("relative_path", HostTypeSchema::String),
    ]
}

fn receipt_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("key", job_key_type()),
        HostStructField::new("status", HostTypeSchema::String),
        HostStructField::new("bytes", HostTypeSchema::Int),
        HostStructField::new("path", HostTypeSchema::String),
        HostStructField::new(
            "error_code",
            HostTypeSchema::Optional(Box::new(HostTypeSchema::String)),
        ),
    ]
}

fn job_key_type() -> HostTypeSchema {
    HostTypeSchema::Named {
        name: "JobKey".to_string(),
        fields: job_key_fields(),
    }
}

fn header_type() -> HostTypeSchema {
    HostTypeSchema::Named {
        name: "HeaderEntry".to_string(),
        fields: header_fields(),
    }
}

fn request_type() -> HostTypeSchema {
    HostTypeSchema::Named {
        name: "TransferRequest".to_string(),
        fields: request_fields(),
    }
}

fn receipt_type() -> HostTypeSchema {
    HostTypeSchema::Named {
        name: "TransferReceipt".to_string(),
        fields: receipt_fields(),
    }
}

fn request_to_value(request: &TransferRequest) -> Value {
    Value::map(vec![
        (Value::string("key"), key_to_value(&request.key)),
        (Value::string("url"), Value::string(request.url.clone())),
        (
            Value::string("headers"),
            Value::array(
                request
                    .headers
                    .iter()
                    .map(|header| {
                        Value::map(vec![
                            (Value::string("name"), Value::bytes(header.name.clone())),
                            (Value::string("value"), Value::bytes(header.value.clone())),
                        ])
                    })
                    .collect(),
            ),
        ),
        (
            Value::string("relative_path"),
            Value::string(request.relative_path.clone()),
        ),
    ])
}

fn receipt_to_value(receipt: &TransferReceipt) -> Value {
    let mut entries = vec![
        (Value::string("key"), key_to_value(&receipt.key)),
        (
            Value::string("status"),
            Value::string(receipt.status.clone()),
        ),
        (Value::string("bytes"), Value::Int(receipt.bytes)),
        (Value::string("path"), Value::string(receipt.path.clone())),
    ];
    if let Some(code) = &receipt.error_code {
        entries.push((Value::string("error_code"), Value::string(code.clone())));
    }
    Value::map(entries)
}

fn key_to_value(key: &JobKey) -> Value {
    Value::map(vec![
        (Value::string("task_id"), Value::Int(key.task_id)),
        (Value::string("epoch"), Value::Int(key.epoch)),
        (Value::string("job_id"), Value::Int(key.job_id)),
    ])
}

fn parse_request(value: Option<&Value>) -> Result<TransferRequest, HostError> {
    let Some(Value::Map(map)) = value else {
        return Err(HostError::runtime("transfer request must be a struct"));
    };
    let key = parse_key(map.get(&Value::string("key")))?;
    let url = parse_string(map.get(&Value::string("url")))?;
    let relative_path = parse_string(map.get(&Value::string("relative_path")))?;
    let headers = parse_headers(map.get(&Value::string("headers")))?;
    Ok(TransferRequest {
        key,
        url,
        headers,
        relative_path,
    })
}

fn parse_key(value: Option<&Value>) -> Result<JobKey, HostError> {
    let Some(Value::Map(map)) = value else {
        return Err(HostError::runtime("job key must be a struct"));
    };
    Ok(JobKey {
        task_id: parse_int(map.get(&Value::string("task_id")))?,
        epoch: parse_int(map.get(&Value::string("epoch")))?,
        job_id: parse_int(map.get(&Value::string("job_id")))?,
    })
}

fn parse_headers(value: Option<&Value>) -> Result<Vec<HeaderEntry>, HostError> {
    let Some(Value::Array(items)) = value else {
        return Err(HostError::runtime("headers must be an array"));
    };
    let mut headers = Vec::new();
    for item in items.iter() {
        let Value::Map(map) = item else {
            return Err(HostError::runtime("header must be a struct"));
        };
        headers.push(HeaderEntry {
            name: parse_bytes(map.get(&Value::string("name")))?,
            value: parse_bytes(map.get(&Value::string("value")))?,
        });
    }
    Ok(headers)
}

fn parse_string(value: Option<&Value>) -> Result<String, HostError> {
    match value {
        Some(Value::String(text)) => Ok((**text).clone()),
        _ => Err(HostError::runtime("expected a string field")),
    }
}

fn parse_int(value: Option<&Value>) -> Result<i64, HostError> {
    match value {
        Some(Value::Int(value)) => Ok(*value),
        _ => Err(HostError::runtime("expected an int field")),
    }
}

fn parse_bytes(value: Option<&Value>) -> Result<Vec<u8>, HostError> {
    match value {
        Some(Value::Bytes(bytes)) => Ok((**bytes).clone()),
        _ => Err(HostError::runtime("expected a bytes field")),
    }
}

fn payload_len(value: &Value) -> usize {
    match value {
        Value::String(text) => text.len(),
        Value::Bytes(bytes) => bytes.len(),
        Value::Array(items) => items.iter().map(payload_len).sum(),
        Value::Map(map) => map
            .iter()
            .map(|(key, item)| payload_len(key) + payload_len(item))
            .sum(),
        _ => 0,
    }
}
