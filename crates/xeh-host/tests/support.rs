use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use vm::resource::HostResource;
use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostParamPassing, HostParamSchema, HostResourceType,
    HostTypeSchema, HttpConfig, HttpHostExt, ResourceHandle, ResourceTypeKey, ResourceTypeSchema,
    Value, Vm, VmError, VmResult,
};

use xeh_host::{
    attach_timer, bind_vm, compile_source_with_catalog, open_registry, standard_catalog,
    EmbeddingTimer,
};

static TEST_ROOT_SEQ: AtomicU64 = AtomicU64::new(1);

/// One test-owned directory under the process temp dir (`TMPDIR`).
///
/// The path is unique for this process and is removed when the guard drops.
/// Callers must close VM handles before dropping the guard.
pub struct TestRoot {
    path: PathBuf,
}

impl TestRoot {
    pub fn create() -> Self {
        let seq = TEST_ROOT_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        let path =
            std::env::temp_dir().join(format!("xeh-host-{}-{}-{}", std::process::id(), nanos, seq));
        std::fs::create_dir(&path)
            .unwrap_or_else(|error| panic!("create test root {}: {error}", path.display()));
        Self { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

const PROBE_KEY: &str = "xeh.test.probe";

pub struct ProbeHandle {
    pub label: String,
}

impl HostResource for ProbeHandle {
    fn resource_type_key() -> Option<ResourceTypeKey> {
        Some(ResourceTypeKey::new(PROBE_KEY).expect("probe key"))
    }
}

impl HostResourceType for ProbeHandle {
    const KEY: &'static str = PROBE_KEY;
    const DESCRIPTION: &'static str = "Test-only probe handle";
}

struct OpenProbe;

impl HostFunction for OpenProbe {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let Some(Value::String(label)) = args.first() else {
            return Err(VmError::HostError("open expects a string".to_string()));
        };
        let token = vm
            .host_context()
            .push_resource(ProbeHandle {
                label: (**label).clone(),
            })
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(
            token.handle().raw() as i64,
        ))))
    }
}

struct ReadLabel;

impl HostFunction for ReadLabel {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let handle = decode_handle(args.first())?;
        let label = {
            let context = vm.host_context();
            let borrowed = context
                .borrow_resource::<ProbeHandle>(handle)
                .map_err(|error| VmError::HostError(error.to_string()))?;
            borrowed.label.clone()
        };
        Ok(CallOutcome::Return(CallReturn::one(Value::String(
            Arc::new(label),
        ))))
    }
}

struct RawProbe;

impl HostFunction for RawProbe {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let handle = decode_handle(args.first())?;
        {
            let context = vm.host_context();
            context
                .borrow_resource::<ProbeHandle>(handle)
                .map_err(|error| VmError::HostError(error.to_string()))?;
        }
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(
            handle.raw() as i64,
        ))))
    }
}

struct CloseProbe;

impl HostFunction for CloseProbe {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let handle = decode_handle(args.first())?;
        vm.host_context()
            .take_resource::<ProbeHandle>(handle)
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

struct FailFn;

impl HostFunction for FailFn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Err(VmError::HostError(
            "intentional timer callback failure".to_string(),
        ))
    }
}

struct ArmHttp {
    config: HttpConfig,
}

impl HostFunction for ArmHttp {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        vm.configure_http(self.config.clone())
            .map_err(|error| VmError::HostError(error.to_string()))?;
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

struct ReenterFn {
    timer: Arc<EmbeddingTimer>,
    log: Arc<Mutex<Vec<String>>>,
}

impl HostFunction for ReenterFn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let message = match self.timer.drive_due() {
            Ok(driven) => format!("reenter-ok:{driven}"),
            Err(error) => error.to_string(),
        };
        self.log.lock().expect("reenter log").push(message);
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

struct RecordFn {
    log: Arc<Mutex<Vec<String>>>,
}

impl HostFunction for RecordFn {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let Some(Value::String(text)) = args.first() else {
            return Err(VmError::HostError("record expects a string".to_string()));
        };
        self.log.lock().expect("record log").push((**text).clone());
        Ok(CallOutcome::Return(CallReturn::one(Value::Int(1))))
    }
}

fn decode_handle(value: Option<&Value>) -> VmResult<ResourceHandle> {
    let Some(Value::Int(raw)) = value else {
        return Err(VmError::HostError(
            "resource argument is not a handle token".to_string(),
        ));
    };
    ResourceHandle::from_raw(*raw as u64).map_err(|error| VmError::HostError(error.to_string()))
}

pub fn probe_catalog() -> Arc<HostApiCatalog> {
    let standard = standard_catalog();
    let key = ResourceTypeKey::new(PROBE_KEY).expect("probe key");
    let resource = HostTypeSchema::Resource(key.clone());
    let mut builder = HostApiCatalog::builder();
    for function in standard.functions() {
        builder.function(function.clone());
    }
    for resource_schema in standard.resources() {
        builder.resource(resource_schema.clone());
    }
    for schema in standard.structs() {
        builder.named_struct(schema.clone());
    }
    builder.resource(ResourceTypeSchema::new(key, ProbeHandle::DESCRIPTION));
    builder.function(host_fn(
        "xeh_test::open",
        vec![HostParamSchema::value("label", HostTypeSchema::String)],
        resource.clone(),
        "Open a test probe handle",
    ));
    builder.function(host_fn(
        "xeh_test::label",
        vec![HostParamSchema::with_passing(
            "handle",
            resource.clone(),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::String,
        "Read a test probe label",
    ));
    builder.function(host_fn(
        "xeh_test::raw",
        vec![HostParamSchema::with_passing(
            "handle",
            resource.clone(),
            HostParamPassing::Borrow,
        )],
        HostTypeSchema::Int,
        "Expose a probe handle token for a foreign-owner check",
    ));
    builder.function(host_fn(
        "xeh_test::close",
        vec![HostParamSchema::with_passing(
            "handle",
            resource.clone(),
            HostParamPassing::TakeOwned,
        )],
        HostTypeSchema::Int,
        "Close a test probe handle",
    ));
    builder.function(host_fn(
        "xeh_test::close_raw",
        vec![HostParamSchema::value("token", HostTypeSchema::Int)],
        HostTypeSchema::Int,
        "Close a probe using a raw token from another VM",
    ));
    builder.function(host_fn(
        "xeh_test::record",
        vec![HostParamSchema::value("text", HostTypeSchema::String)],
        HostTypeSchema::Int,
        "Record a timer callback observation",
    ));
    builder.function(host_fn(
        "xeh_test::fail",
        vec![],
        HostTypeSchema::Int,
        "Fail the current timer callback",
    ));
    builder.function(host_fn(
        "xeh_test::arm_http",
        vec![],
        HostTypeSchema::Int,
        "Install the fixture HTTP policy on this callback VM",
    ));
    builder.function(host_fn(
        "xeh_test::reenter",
        vec![],
        HostTypeSchema::Int,
        "Attempt to drive the owner timer from inside a callback",
    ));
    Arc::new(
        builder
            .build()
            .expect("composed test catalog should validate"),
    )
}

fn host_fn(
    name: &str,
    params: Vec<HostParamSchema>,
    return_type: HostTypeSchema,
    description: &str,
) -> HostFunctionSchema {
    HostFunctionSchema::with_return(name, params, return_type).with_description(description)
}

pub fn register_probe(registry: &mut HostFunctionRegistry, catalog: &HostApiCatalog) {
    register_one(registry, catalog, "xeh_test::open", || Box::new(OpenProbe));
    register_one(registry, catalog, "xeh_test::label", || Box::new(ReadLabel));
    register_one(registry, catalog, "xeh_test::raw", || Box::new(RawProbe));
    register_one(registry, catalog, "xeh_test::close", || {
        Box::new(CloseProbe)
    });
    register_one(registry, catalog, "xeh_test::close_raw", || {
        Box::new(CloseProbe)
    });
}

pub fn register_record(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    log: Arc<Mutex<Vec<String>>>,
) {
    register_catalog_function(
        registry,
        catalog,
        "xeh_test::record",
        catalog
            .function("xeh_test::record")
            .expect("record schema")
            .clone(),
        move || {
            Box::new(RecordFn {
                log: Arc::clone(&log),
            })
        },
    )
    .expect("record host should register");
}

pub fn register_fail(registry: &mut HostFunctionRegistry, catalog: &HostApiCatalog) {
    register_one(registry, catalog, "xeh_test::fail", || Box::new(FailFn));
}

pub fn register_arm_http(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    config: HttpConfig,
) {
    register_catalog_function(
        registry,
        catalog,
        "xeh_test::arm_http",
        catalog
            .function("xeh_test::arm_http")
            .expect("arm_http schema")
            .clone(),
        move || {
            Box::new(ArmHttp {
                config: config.clone(),
            })
        },
    )
    .expect("arm_http host should register");
}

pub fn register_reenter(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    timer: Arc<EmbeddingTimer>,
    log: Arc<Mutex<Vec<String>>>,
) {
    register_catalog_function(
        registry,
        catalog,
        "xeh_test::reenter",
        catalog
            .function("xeh_test::reenter")
            .expect("reenter schema")
            .clone(),
        move || {
            Box::new(ReenterFn {
                timer: Arc::clone(&timer),
                log: Arc::clone(&log),
            })
        },
    )
    .expect("reenter host should register");
}

fn register_one<F>(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    name: &str,
    factory: F,
) where
    F: Fn() -> Box<dyn HostFunction> + Send + Sync + 'static,
{
    let schema = catalog.function(name).expect(name).clone();
    register_catalog_function(registry, catalog, name, schema, factory)
        .unwrap_or_else(|error| panic!("register {name}: {error}"));
}

pub async fn prepare_source(
    source: &str,
    catalog: Arc<HostApiCatalog>,
) -> (Vm, Arc<EmbeddingTimer>) {
    let program = compile_source_with_catalog(source, Arc::clone(&catalog)).expect("compile");
    let mut registry = open_registry(&catalog).expect("registry");
    if catalog.function("xeh_test::open").is_some() {
        register_probe(&mut registry, &catalog);
    }
    let mut vm = Vm::try_new(program).expect("vm");
    bind_vm(&mut vm, &registry).expect("bind");
    let timer = Arc::new(EmbeddingTimer::new());
    attach_timer(&mut vm, Arc::clone(&timer));
    (vm, timer)
}
