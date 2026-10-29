//! Embedded task-control owner.
//!
//! One `TaskControl` owns one VM, one catalog, and one control root value. Each
//! command is a real invocation of `app/domain/entry.rss::dispatch` with the
//! previous root, and the returned root is threaded into the next command. The
//! store therefore lives in the RSS object graph across calls; Rust never
//! encodes it as text, rebuilds an empty store, or decides a state transition.
//!
//! The only system primitive this file adds is `xeh_system::uuid_v4`, which
//! returns canonical UUID text. The pinned Core has no UUID or random source,
//! so the GUID is produced by an OS entropy read and formatted here; the
//! 8-character task GUID is still chosen in RSS.
use std::future::poll_fn;
use std::path::PathBuf;
use std::sync::Arc;
use std::task::Poll;

use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostTypeSchema, InvocationItem, InvocationPoll,
    Value, Vm, VmError, VmResult,
};

use crate::catalog::{open_registry, standard_catalog};
use crate::runtime::{bind_vm, compile_file_with_catalog, drive_to_halt};
use crate::types::HostError;

pub fn task_entry_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/domain/state.rss")
}

/// Inputs for a control owner. `production` plans no GUIDs and uses the system
/// UUID source; `fixture` injects the deterministic prefixes the oracle also
/// receives. Seeded states and metadata stay disabled unless the fixture asks
/// for them, so the production surface has no arbitrary state injection.
#[derive(Debug, Clone, Default)]
pub struct ControlOptions {
    pub guids: Vec<String>,
    pub epoch_start: i64,
    pub allow_seed: bool,
}

impl ControlOptions {
    pub fn production() -> Self {
        Self::default()
    }

    pub fn fixture(guids: &[&str], epoch_start: i64) -> Self {
        Self {
            guids: guids.iter().map(|guid| (*guid).to_string()).collect(),
            epoch_start,
            allow_seed: true,
        }
    }
}

pub struct TaskControl {
    runtime: tokio::runtime::Runtime,
    vm: Vm,
    control: Value,
    dispatch: Value,
    report: Value,
}

impl TaskControl {
    pub fn open(options: &ControlOptions) -> Result<Self, HostError> {
        for guid in &options.guids {
            if guid.len() != 8 {
                return Err(HostError::bind(format!(
                    "fixture guid '{guid}' must be 8 characters"
                )));
            }
        }
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| HostError::runtime(error.to_string()))?;
        let catalog = task_catalog();
        let entry = task_entry_path();
        let program = compile_file_with_catalog(&entry, Arc::clone(&catalog)).map_err(|error| {
            HostError::compile(format!("task entry {}: {error}", entry.display()))
        })?;
        let mut vm = Vm::try_new(program).map_err(HostError::from_vm)?;
        // The pinned Core enables the trace JIT by default on this platform
        // even without the native backend; the configuration loops must stay
        // interpreter-only, as in the accepted config entry.
        vm.set_jit_config(vm::JitConfig {
            enabled: false,
            ..Default::default()
        });
        let registry = registry_with_task_hosts(&catalog)?;
        bind_vm(&mut vm, &registry)?;
        runtime.block_on(drive_to_halt(&mut vm))?;

        let create = vm
            .resolve_exported_callable("create")
            .map_err(HostError::from_vm)?;
        let planned = options
            .guids
            .iter()
            .map(|guid| Value::string(guid.clone()))
            .collect::<Vec<_>>();
        let control = runtime.block_on(invoke_value(
            &mut vm,
            create,
            vec![
                Value::Array(planned.into()),
                Value::Int(options.epoch_start),
                Value::Bool(options.allow_seed),
            ],
        ))?;
        let dispatch = vm
            .resolve_exported_callable("dispatch")
            .map_err(HostError::from_vm)?;
        let report = vm
            .resolve_exported_callable("report")
            .map_err(HostError::from_vm)?;
        Ok(Self {
            runtime,
            vm,
            control,
            dispatch,
            report,
        })
    }

    pub fn open_production() -> Result<Self, HostError> {
        Self::open(&ControlOptions::production())
    }

    /// Runs one command on the same owner VM and returns its JSON report.
    pub fn invoke(&mut self, request: &str) -> Result<String, HostError> {
        let control = self.runtime.block_on(invoke_value(
            &mut self.vm,
            self.dispatch.clone(),
            vec![self.control.clone(), Value::string(request.to_string())],
        ))?;
        let report = self.runtime.block_on(invoke_value(
            &mut self.vm,
            self.report.clone(),
            vec![control.clone()],
        ))?;
        self.control = control;
        match report {
            Value::String(text) => Ok(text.as_str().to_string()),
            other => Err(HostError::runtime(format!(
                "task report is {other:?}, not a string"
            ))),
        }
    }
}

async fn invoke_value(vm: &mut Vm, callable: Value, args: Vec<Value>) -> Result<Value, HostError> {
    let mut invocation = vm
        .start_invocation(callable, args)
        .map_err(HostError::from_vm)?;
    poll_fn(|context| loop {
        match invocation.poll_next_with_context(context) {
            Ok(InvocationPoll::Pending) => return Poll::Pending,
            Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Event(_))))) => continue,
            Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value))))) => {
                return Poll::Ready(Ok(value));
            }
            Ok(InvocationPoll::Ready(Some(Err(error)))) => {
                return Poll::Ready(Err(HostError::runtime(format!(
                    "task invocation failed: {error:?}"
                ))));
            }
            Ok(InvocationPoll::Ready(None)) => {
                return Poll::Ready(Err(HostError::runtime(
                    "task invocation ended without a result",
                )));
            }
            Err(error) => return Poll::Ready(Err(HostError::from_vm(error))),
        }
    })
    .await
}

/// Standard catalog plus the one executed Core gap: UUID v4 text.
pub fn task_catalog() -> Arc<HostApiCatalog> {
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
    builder.function(HostFunctionSchema::with_return(
        "xeh_system::uuid_v4",
        Vec::new(),
        HostTypeSchema::String,
    ));
    Arc::new(builder.build().expect("task control catalog"))
}

fn registry_with_task_hosts(catalog: &HostApiCatalog) -> Result<HostFunctionRegistry, HostError> {
    let mut registry = open_registry(catalog)?;
    let schema = catalog
        .function("xeh_system::uuid_v4")
        .ok_or_else(|| HostError::bind("missing xeh_system::uuid_v4 schema"))?
        .clone();
    register_catalog_function(
        &mut registry,
        catalog,
        "xeh_system::uuid_v4",
        schema,
        || Box::new(UuidFn),
    )
    .map_err(|error| HostError::bind(error.to_string()))?;
    Ok(registry)
}

struct UuidFn;

impl HostFunction for UuidFn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Ok(CallOutcome::Return(CallReturn::one(Value::string(
            uuid_v4_text()?,
        ))))
    }
}

fn uuid_v4_text() -> VmResult<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes)
        .map_err(|error| VmError::HostError(format!("uuid entropy unavailable: {error}")))?;
    // RFC 4122 version 4 layout.
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut text = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if index == 4 || index == 6 || index == 8 || index == 10 {
            text.push('-');
        }
        text.push_str(&format!("{byte:02x}"));
    }
    Ok(text)
}
