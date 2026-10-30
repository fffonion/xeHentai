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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;

use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostTypeSchema, InvocationItem, InvocationPoll,
    IoHostExt, Value, Vm, VmError, VmResult,
};

use crate::catalog::{open_registry, standard_catalog};
use crate::os::fs::SESSION_READ_LIMIT_BYTES;
use crate::runtime::{bind_vm, compile_file_with_catalog, drive_to_halt};
use crate::types::HostError;

pub fn task_entry_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../app/domain/state.rss")
}

/// Trusted session storage for one control owner.
///
/// These paths come from the embedding, never from a dispatch request. The
/// download roots are the only directories `xeh_fs` will stat or clean; an
/// imported task's `config.dir` cannot widen them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionOptions {
    session_path: PathBuf,
    legacy_cookie_path: PathBuf,
    download_roots: Vec<PathBuf>,
    scan_budget: u64,
    session_byte_limit: usize,
}

impl SessionOptions {
    pub fn new(
        session_path: PathBuf,
        legacy_cookie_path: PathBuf,
        download_roots: Vec<PathBuf>,
    ) -> Self {
        Self {
            session_path,
            legacy_cookie_path,
            download_roots,
            scan_budget: 100_000,
            session_byte_limit: SESSION_READ_LIMIT_BYTES,
        }
    }

    /// Trusted embedding/test limits: at most `scan_budget` fids are scanned
    /// during one restore, and one session document is at most
    /// `session_byte_limit` UTF-8 bytes for both the Core read and the native
    /// atomic write. Neither limit can be raised by imported JSON.
    pub fn with_limits(mut self, scan_budget: u64, session_byte_limit: usize) -> Self {
        self.scan_budget = scan_budget;
        self.session_byte_limit = session_byte_limit;
        self
    }

    /// Production defaults: `h.json` in the process working directory and the
    /// 1.x `.ehentai.cookie` next to the application executable, exactly like
    /// the upstream `FILEPATH`. The working directory is also the trusted
    /// download root for this scope; a configured download directory must be
    /// added by the config owner before scanning it.
    pub fn production() -> Result<Self, HostError> {
        let cwd = std::env::current_dir().map_err(|error| {
            HostError::runtime(format!("cannot read the working directory: {error}"))
        })?;
        let executable = std::env::current_exe().map_err(|error| {
            HostError::runtime(format!("cannot locate the executable: {error}"))
        })?;
        let app_dir = executable.parent().ok_or_else(|| {
            HostError::runtime("the executable has no parent directory".to_string())
        })?;
        Ok(Self::new(
            cwd.join("h.json"),
            app_dir.join(".ehentai.cookie"),
            vec![cwd],
        ))
    }

    pub fn session_path(&self) -> &Path {
        &self.session_path
    }

    pub fn legacy_cookie_path(&self) -> &Path {
        &self.legacy_cookie_path
    }

    pub fn download_roots(&self) -> &[PathBuf] {
        &self.download_roots
    }

    pub fn scan_budget(&self) -> u64 {
        self.scan_budget
    }

    pub fn session_byte_limit(&self) -> usize {
        self.session_byte_limit
    }
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
    session: Option<SessionOptions>,
}

impl TaskControl {
    pub fn open(options: &ControlOptions) -> Result<Self, HostError> {
        Self::open_with_session(options, None)
    }

    /// Opens one control owner with a trusted session storage binding.
    ///
    /// Without `session`, the VM has a deny-all Core `io` policy and no
    /// `xeh_fs` session target: a plain controller cannot read the process
    /// working directory just by existing.
    pub fn open_with_session(
        options: &ControlOptions,
        session: Option<&SessionOptions>,
    ) -> Result<Self, HostError> {
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
        configure_session_boundaries(&mut vm, session)?;
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
            session: session.cloned(),
        })
    }

    pub fn open_production() -> Result<Self, HostError> {
        Self::open(&ControlOptions::production())
    }

    pub fn open_production_session(session: &SessionOptions) -> Result<Self, HostError> {
        Self::open_with_session(&ControlOptions::production(), Some(session))
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

    /// Saves the session through the RSS document builder and the atomic file
    /// primitive. A Core decoder/serializer or host failure is converted into
    /// the upstream-shaped `(code, value)` report instead of escaping as an
    /// opaque host error.
    pub fn save_session(&mut self) -> Result<String, HostError> {
        self.invoke_session("{\"op\":\"session_save\"}")
    }

    /// Loads the trusted `h.json` and legacy cookie file through RSS rules.
    pub fn load_session(&mut self) -> Result<String, HostError> {
        self.invoke_session("{\"op\":\"session_load\"}")
    }

    /// Whether this owner was opened with a trusted session binding.
    pub fn has_session(&self) -> bool {
        self.session.is_some()
    }

    /// Live execution-scope resource count for the owner VM. A completed
    /// session read/write leaves no open file resources behind.
    pub fn resource_count(&mut self) -> usize {
        crate::runtime::resource_count(&mut self.vm)
    }

    fn invoke_session(&mut self, request: &str) -> Result<String, HostError> {
        match self.invoke(request) {
            Ok(report) => Ok(report),
            Err(error) => {
                let message = error.message().to_string();
                let (class, code) = classify_session_failure(&message);
                let fallback = format!(
                    "{{\"op\":\"session_failed\",\"class\":{},\"code\":{},\"message\":{}}}",
                    json_string(class),
                    code,
                    json_string(&message)
                );
                self.invoke(&fallback)
            }
        }
    }
}

/// Maps a host failure to the report class the session module records.
///
/// The Core JSON decoder reports a non-object root separately from malformed
/// JSON, so the two upstream failure families (a caught parse error at 1103 vs
/// an uncaught structural exception) stay distinguishable instead of being
/// flattened into one code.
fn classify_session_failure(message: &str) -> (&'static str, i64) {
    if message.contains("root value must be a JSON object") {
        ("structure", -1)
    } else if message.contains("json_decode_object_entries") || message.contains("json_decode") {
        ("parse", 1103)
    } else {
        ("host_failure", -1)
    }
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for character in text.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            control if (control as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", control as u32));
            }
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

fn configure_session_boundaries(
    vm: &mut Vm,
    session: Option<&SessionOptions>,
) -> Result<(), HostError> {
    match session {
        Some(session) => {
            // Core IO carries only the two trusted read sources. Passing the
            // exact files (not their parents) keeps a session/legacy parent
            // from becoming a general read root; every mutation is either the
            // exact atomic session target or the explicit download roots.
            let read_roots = [session.session_path(), session.legacy_cookie_path()]
                .iter()
                .filter(|path| path.exists())
                .map(|path| {
                    path.to_str().map(str::to_string).ok_or_else(|| {
                        HostError::bind("session read path is not valid UTF-8".to_string())
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            vm.configure_io(vm::IoPolicy {
                allowed_roots: read_roots,
                allow_write: false,
                allow_process: false,
                max_read_bytes: session.session_byte_limit(),
                max_write_bytes: session.session_byte_limit(),
            });
            crate::os::fs::install(
                vm,
                crate::os::fs::FsState::new(
                    session.download_roots().to_vec(),
                    Some(session.session_path().to_path_buf()),
                    Some(session.legacy_cookie_path().to_path_buf()),
                )
                .with_limits(session.scan_budget(), session.session_byte_limit()),
            );
        }
        None => {
            vm.configure_io(vm::IoPolicy {
                allowed_roots: Vec::new(),
                allow_write: false,
                allow_process: false,
                max_read_bytes: SESSION_READ_LIMIT_BYTES,
                max_write_bytes: SESSION_READ_LIMIT_BYTES,
            });
            crate::os::fs::install(vm, crate::os::fs::FsState::default());
        }
    }
    Ok(())
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

/// Standard catalog plus the executed Core gaps: UUID v4 text and the named
/// file-system primitives the session/scan scope needs.
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
    for schema in crate::os::fs_structs() {
        builder.named_struct(schema);
    }
    for function in crate::os::fs_functions() {
        builder.function(function);
    }
    builder.function(HostFunctionSchema::with_return(
        "xeh_system::uuid_v4",
        Vec::new(),
        HostTypeSchema::String,
    ));
    Arc::new(builder.build().expect("task control catalog"))
}

pub(crate) fn registry_with_task_hosts(
    catalog: &HostApiCatalog,
) -> Result<HostFunctionRegistry, HostError> {
    let mut registry = open_registry(catalog)?;
    crate::os::register_fs(&mut registry, catalog)?;
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
