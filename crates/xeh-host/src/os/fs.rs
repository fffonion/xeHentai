//! Named file-system primitives the pinned Core does not publish.
//!
//! The Core `io` surface is `open` / `popen` / `read_all` / `read_line` /
//! `write` / `flush` / `close` / `exists`. Session recovery additionally needs
//! a metadata query, the zero-byte cleanup the upstream scan performs, and one
//! atomic whole-file replacement. Those are the only three capabilities added
//! here, next to the two trusted-path accessors that keep the session target
//! out of the dispatch request:
//!
//! * [`install`] binds one [`FsState`] to a single VM. Every path is resolved
//!   against the state's trusted roots; a request can never widen them.
//! * `write_atomic` stages in the target's own directory, writes the complete
//!   UTF-8 payload, syncs it, renames over the target, and then syncs the
//!   parent directory. A failure before the rename leaves the old target
//!   byte-identical; a directory-sync failure is reported separately so a
//!   committed replacement is never described as an untouched old file.
//!
//! The host-private checkpoint used by the crash test is not part of any guest
//! catalog schema: production [`write_atomic`] never pauses.

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostParamSchema, HostStructField, HostStructSchema,
    HostTypeSchema, Value, Vm, VmError, VmResult,
};

use crate::types::HostError;

static STAGING_SEQ: AtomicU64 = AtomicU64::new(1);

/// Per-VM trusted file-system policy.
///
/// `roots` are the only directories `stat` / `remove_zero_file` /
/// `write_atomic` may touch. `session_path` and `legacy_cookie_path` are the
/// trusted targets the RSS session module reads from `session_paths`; they are
/// installed here, never supplied by a guest request.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FsState {
    roots: Vec<PathBuf>,
    session_path: Option<PathBuf>,
    legacy_cookie_path: Option<PathBuf>,
}

impl FsState {
    pub fn new(
        roots: Vec<PathBuf>,
        session_path: Option<PathBuf>,
        legacy_cookie_path: Option<PathBuf>,
    ) -> Self {
        Self {
            roots,
            session_path,
            legacy_cookie_path,
        }
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn session_path(&self) -> Option<&Path> {
        self.session_path.as_deref()
    }

    pub fn legacy_cookie_path(&self) -> Option<&Path> {
        self.legacy_cookie_path.as_deref()
    }
}

/// Binds the trusted file-system state to one VM.
pub fn install(vm: &mut Vm, state: FsState) {
    vm.host_context().set_module_state(state);
}

pub fn structs() -> Vec<HostStructSchema> {
    vec![
        HostStructSchema::new("FsStat", stat_fields()),
        HostStructSchema::new("ZeroFileResult", zero_fields()),
        HostStructSchema::new("AtomicWriteResult", atomic_fields()),
        HostStructSchema::new("SessionPaths", session_paths_fields()),
    ]
}

pub fn functions() -> Vec<HostFunctionSchema> {
    vec![
        HostFunctionSchema::with_return(
            "xeh_fs::stat",
            vec![HostParamSchema::value("path", HostTypeSchema::String)],
            named("FsStat", stat_fields()),
        )
        .with_description("Read the metadata of one path inside the configured roots"),
        HostFunctionSchema::with_return(
            "xeh_fs::remove_zero_file",
            vec![HostParamSchema::value("path", HostTypeSchema::String)],
            named("ZeroFileResult", zero_fields()),
        )
        .with_description("Remove one zero-byte regular file inside the configured roots"),
        HostFunctionSchema::with_return(
            "xeh_fs::write_atomic",
            vec![
                HostParamSchema::value("path", HostTypeSchema::String),
                HostParamSchema::value("contents", HostTypeSchema::String),
            ],
            named("AtomicWriteResult", atomic_fields()),
        )
        .with_description("Replace one UTF-8 file atomically inside the configured roots"),
        HostFunctionSchema::with_return(
            "xeh_fs::rename_atomic",
            vec![
                HostParamSchema::value("source", HostTypeSchema::String),
                HostParamSchema::value("target", HostTypeSchema::String),
            ],
            named("AtomicWriteResult", atomic_fields()),
        )
        .with_description("Rename one staged file over its target inside the configured roots"),
        HostFunctionSchema::with_return(
            "xeh_fs::session_paths",
            Vec::new(),
            named("SessionPaths", session_paths_fields()),
        )
        .with_description("Read the trusted session and legacy-cookie targets for this VM"),
    ]
}

pub fn register(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> Result<(), HostError> {
    register_one(registry, catalog, "xeh_fs::stat", || Box::new(StatFn))?;
    register_one(registry, catalog, "xeh_fs::remove_zero_file", || {
        Box::new(RemoveZeroFn)
    })?;
    register_one(registry, catalog, "xeh_fs::write_atomic", || {
        Box::new(WriteAtomicFn)
    })?;
    register_one(registry, catalog, "xeh_fs::rename_atomic", || {
        Box::new(RenameAtomicFn)
    })?;
    register_one(registry, catalog, "xeh_fs::session_paths", || {
        Box::new(SessionPathsFn)
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
        .ok_or_else(|| HostError::bind(format!("missing fs schema {name}")))?
        .clone();
    register_catalog_function(registry, catalog, name, schema, factory)
        .map_err(|error| HostError::bind(error.to_string()))
}

fn named(name: &str, fields: Vec<HostStructField>) -> HostTypeSchema {
    HostTypeSchema::Named {
        name: name.to_string(),
        fields,
    }
}

fn stat_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("exists", HostTypeSchema::Bool),
        HostStructField::new("size", HostTypeSchema::Int),
        HostStructField::new("is_file", HostTypeSchema::Bool),
        HostStructField::new("is_dir", HostTypeSchema::Bool),
    ]
}

fn zero_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("removed", HostTypeSchema::Bool),
        HostStructField::new("size", HostTypeSchema::Int),
        HostStructField::new(
            "error",
            HostTypeSchema::Optional(Box::new(HostTypeSchema::String)),
        ),
    ]
}

fn atomic_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("committed", HostTypeSchema::Bool),
        HostStructField::new("directory_synced", HostTypeSchema::Bool),
        HostStructField::new(
            "error",
            HostTypeSchema::Optional(Box::new(HostTypeSchema::String)),
        ),
    ]
}

fn session_paths_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("session_path", HostTypeSchema::String),
        HostStructField::new("legacy_cookie_path", HostTypeSchema::String),
    ]
}

fn state_of(vm: &mut Vm) -> Result<FsState, VmError> {
    vm.host_context()
        .module_state::<FsState>()
        .cloned()
        .ok_or_else(|| VmError::HostError("xeh_fs state is not installed for this VM".to_string()))
}

struct StatFn;

impl HostFunction for StatFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let path = string_arg(args.first(), "path")?;
        let resolved = resolve(&state, &path)?;
        let metadata = resolved.metadata();
        let value = match metadata {
            Some(metadata) => Value::map(vec![
                (Value::string("exists"), Value::Bool(true)),
                (
                    Value::string("size"),
                    Value::Int(i64::try_from(metadata.len()).unwrap_or(i64::MAX)),
                ),
                (Value::string("is_file"), Value::Bool(metadata.is_file())),
                (Value::string("is_dir"), Value::Bool(metadata.is_dir())),
            ]),
            None => Value::map(vec![
                (Value::string("exists"), Value::Bool(false)),
                (Value::string("size"), Value::Int(0)),
                (Value::string("is_file"), Value::Bool(false)),
                (Value::string("is_dir"), Value::Bool(false)),
            ]),
        };
        Ok(CallOutcome::Return(CallReturn::one(value)))
    }
}

struct RemoveZeroFn;

impl HostFunction for RemoveZeroFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let path = string_arg(args.first(), "path")?;
        let resolved = resolve(&state, &path)?;
        let Some(metadata) = resolved.metadata() else {
            return Ok(CallOutcome::Return(CallReturn::one(zero_result(
                false, 0, None,
            )?)));
        };
        if metadata.is_dir() {
            return Err(VmError::HostError(
                "xeh_fs remove_zero_file target is a directory".to_string(),
            ));
        }
        let size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
        if metadata.len() != 0 {
            return Ok(CallOutcome::Return(CallReturn::one(zero_result(
                false, size, None,
            )?)));
        }
        match fs::remove_file(resolved.path()) {
            Ok(()) => Ok(CallOutcome::Return(CallReturn::one(zero_result(
                true, 0, None,
            )?))),
            Err(error) => Ok(CallOutcome::Return(CallReturn::one(zero_result(
                false,
                0,
                Some(format!("remove_zero_file failed: {error}")),
            )?))),
        }
    }
}

fn zero_result(removed: bool, size: i64, error: Option<String>) -> VmResult<Value> {
    let mut entries = vec![
        (Value::string("removed"), Value::Bool(removed)),
        (Value::string("size"), Value::Int(size)),
    ];
    match error {
        Some(message) => entries.push((Value::string("error"), Value::string(message))),
        None => entries.push((Value::string("error"), Value::Null)),
    }
    Ok(Value::map(entries))
}

struct WriteAtomicFn;

impl HostFunction for WriteAtomicFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let path = string_arg(args.first(), "path")?;
        let contents = string_arg(args.get(1), "contents")?;
        let outcome = write_atomic(&state, Path::new(&path), &contents)?;
        Ok(CallOutcome::Return(CallReturn::one(outcome.to_value())))
    }
}

struct RenameAtomicFn;

impl HostFunction for RenameAtomicFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let source = string_arg(args.first(), "source")?;
        let target = string_arg(args.get(1), "target")?;
        let outcome = rename_atomic(&state, Path::new(&source), Path::new(&target))?;
        Ok(CallOutcome::Return(CallReturn::one(outcome.to_value())))
    }
}

struct SessionPathsFn;

impl HostFunction for SessionPathsFn {
    fn call(&mut self, vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        let state = state_of(vm)?;
        let session = state.session_path().ok_or_else(|| {
            VmError::HostError("session paths are not configured for this VM".to_string())
        })?;
        let session_text = session
            .to_str()
            .ok_or_else(|| VmError::HostError("session path is not valid UTF-8".to_string()))?;
        let legacy_text = match state.legacy_cookie_path() {
            Some(path) => path
                .to_str()
                .ok_or_else(|| {
                    VmError::HostError("legacy cookie path is not valid UTF-8".to_string())
                })?
                .to_string(),
            None => String::new(),
        };
        Ok(CallOutcome::Return(CallReturn::one(Value::map(vec![
            (
                Value::string("session_path"),
                Value::string(session_text.to_string()),
            ),
            (
                Value::string("legacy_cookie_path"),
                Value::string(legacy_text),
            ),
        ]))))
    }
}

fn string_arg(value: Option<&Value>, name: &str) -> Result<String, VmError> {
    match value {
        Some(Value::String(text)) => Ok(text.as_str().to_string()),
        _ => Err(VmError::HostError(format!(
            "xeh_fs::{name} expects a string"
        ))),
    }
}

// ------------------------------------------------------------- path authority

struct Resolved {
    path: PathBuf,
    metadata: Option<fs::Metadata>,
}

impl Resolved {
    fn path(&self) -> &Path {
        &self.path
    }

    fn metadata(&self) -> Option<&fs::Metadata> {
        self.metadata.as_ref()
    }
}

/// Resolves `requested` inside one of the state's canonical roots.
///
/// A missing target is resolved through its nearest existing ancestor so a
/// not-yet-created scan target still answers `exists = false` without widening
/// the root check. Existing paths are canonicalized again, so a symlink that
/// points outside the roots is rejected.
fn resolve(state: &FsState, requested: &str) -> Result<Resolved, VmError> {
    if requested.is_empty() {
        return Err(VmError::HostError("xeh_fs path is empty".to_string()));
    }
    let requested = PathBuf::from(requested);
    let absolute = if requested.is_absolute() {
        requested
    } else {
        std::env::current_dir()
            .map_err(|error| VmError::HostError(format!("xeh_fs cwd failed: {error}")))?
            .join(requested)
    };
    let mut existing = absolute.clone();
    let mut suffix: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name() else {
            return Err(VmError::HostError(
                "xeh_fs path has no existing ancestor".to_string(),
            ));
        };
        suffix.push(name.to_os_string());
        let Some(parent) = existing.parent() else {
            return Err(VmError::HostError(
                "xeh_fs path has no existing ancestor".to_string(),
            ));
        };
        existing = parent.to_path_buf();
    }
    let mut path = fs::canonicalize(&existing)
        .map_err(|error| VmError::HostError(format!("xeh_fs path resolution failed: {error}")))?;
    for name in suffix.iter().rev() {
        path.push(name);
    }
    check_root(state, &path)?;
    if path.exists() {
        path = fs::canonicalize(&path).map_err(|error| {
            VmError::HostError(format!("xeh_fs path resolution failed: {error}"))
        })?;
        check_root(state, &path)?;
        let metadata = fs::metadata(&path)
            .map_err(|error| VmError::HostError(format!("xeh_fs stat failed: {error}")))?;
        return Ok(Resolved {
            path,
            metadata: Some(metadata),
        });
    }
    Ok(Resolved {
        path,
        metadata: None,
    })
}

fn check_root(state: &FsState, path: &Path) -> Result<(), VmError> {
    for root in &state.roots {
        let canonical = fs::canonicalize(root).map_err(|error| {
            VmError::HostError(format!(
                "xeh_fs allowed root '{}' cannot be resolved: {error}",
                root.display()
            ))
        })?;
        if path.starts_with(&canonical) {
            return Ok(());
        }
    }
    Err(VmError::HostError(format!(
        "xeh_fs path '{}' is outside the allowed roots",
        path.display()
    )))
}

// ---------------------------------------------------------------- atomic file

pub struct AtomicOutcome {
    committed: bool,
    directory_synced: bool,
    error: Option<String>,
}

impl AtomicOutcome {
    pub fn committed(&self) -> bool {
        self.committed
    }

    pub fn directory_synced(&self) -> bool {
        self.directory_synced
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn failed(error: String) -> Self {
        Self {
            committed: false,
            directory_synced: false,
            error: Some(error),
        }
    }

    fn to_value(&self) -> Value {
        let mut entries = vec![
            (Value::string("committed"), Value::Bool(self.committed)),
            (
                Value::string("directory_synced"),
                Value::Bool(self.directory_synced),
            ),
        ];
        match &self.error {
            Some(message) => entries.push((Value::string("error"), Value::string(message.clone()))),
            None => entries.push((Value::string("error"), Value::Null)),
        }
        Value::map(entries)
    }
}

/// Replaces `path` with `contents`, returning a structured pre/post-commit
/// result. Path authority failures are VM errors; ordinary I/O failures are
/// reported in the value so the session layer can turn them into code 1103.
pub fn write_atomic(state: &FsState, path: &Path, contents: &str) -> VmResult<AtomicOutcome> {
    write_atomic_with_hook(state, path, contents, None, || {})
}

/// Host-private crash checkpoint used by the interrupt test only.
///
/// After `pause_after` bytes of the complete payload have been written to the
/// staging file, `hook` runs before the remainder. No guest schema exposes
/// this entry point and production [`write_atomic`] passes `None`.
#[doc(hidden)]
pub fn write_atomic_with_hook(
    state: &FsState,
    path: &Path,
    contents: &str,
    pause_after: Option<usize>,
    hook: impl FnOnce(),
) -> VmResult<AtomicOutcome> {
    let target = resolve(state, &path.to_string_lossy())?;
    let parent = target
        .path()
        .parent()
        .ok_or_else(|| VmError::HostError("xeh_fs target has no parent".to_string()))?
        .to_path_buf();
    if !parent.is_dir() {
        return Ok(AtomicOutcome::failed(
            "xeh_fs parent directory does not exist".to_string(),
        ));
    }
    let name = target
        .path()
        .file_name()
        .ok_or_else(|| VmError::HostError("xeh_fs target has no file name".to_string()))?;
    let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
    let staging = parent.join(format!(
        ".{}.xeh-{}-{}.tmp",
        name.to_string_lossy(),
        std::process::id(),
        seq
    ));
    let staged = stage_file(&parent, &staging);
    let mut file = match staged {
        Ok(file) => file,
        Err(error) => return Ok(AtomicOutcome::failed(error)),
    };
    if let Err(error) = match pause_after {
        Some(limit) if limit < contents.len() => {
            if let Err(error) = file.write_all(&contents.as_bytes()[..limit]) {
                Err(format!("xeh_fs staging write failed: {error}"))
            } else if let Err(error) = file.flush() {
                Err(format!("xeh_fs staging flush failed: {error}"))
            } else {
                hook();
                file.write_all(&contents.as_bytes()[limit..])
                    .and_then(|()| file.flush())
                    .map_err(|error| format!("xeh_fs staging write failed: {error}"))
            }
        }
        _ => file
            .write_all(contents.as_bytes())
            .and_then(|()| file.flush())
            .map_err(|error| format!("xeh_fs staging write failed: {error}")),
    } {
        drop(file);
        let _ = fs::remove_file(&staging);
        return Ok(AtomicOutcome::failed(error));
    }
    if let Err(error) = file.sync_all() {
        drop(file);
        let _ = fs::remove_file(&staging);
        return Ok(AtomicOutcome::failed(format!(
            "xeh_fs staging sync failed: {error}"
        )));
    }
    drop(file);
    if let Err(error) = fs::rename(&staging, target.path()) {
        let _ = fs::remove_file(&staging);
        return Ok(AtomicOutcome::failed(format!(
            "xeh_fs rename failed: {error}"
        )));
    }
    match File::open(&parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(AtomicOutcome {
            committed: true,
            directory_synced: true,
            error: None,
        }),
        Err(error) => Ok(AtomicOutcome {
            committed: true,
            directory_synced: false,
            error: Some(format!("xeh_fs directory sync failed: {error}")),
        }),
    }
}

/// Renames `source` over `target` after both pass the trusted-root check, then
/// syncs the target's parent directory. An ordinary failure is reported in the
/// value; a path-authority failure is a VM error.
pub fn rename_atomic(state: &FsState, source: &Path, target: &Path) -> VmResult<AtomicOutcome> {
    let resolved_source = resolve(state, &source.to_string_lossy())?;
    let resolved_target = resolve(state, &target.to_string_lossy())?;
    let Some(metadata) = resolved_source.metadata() else {
        return Ok(AtomicOutcome::failed(
            "xeh_fs rename source does not exist".to_string(),
        ));
    };
    if !metadata.is_file() {
        return Ok(AtomicOutcome::failed(
            "xeh_fs rename source is not a regular file".to_string(),
        ));
    }
    let parent = match resolved_target.path().parent() {
        Some(parent) => parent.to_path_buf(),
        None => {
            return Ok(AtomicOutcome::failed(
                "xeh_fs rename target has no parent".to_string(),
            ))
        }
    };
    if let Err(error) = fs::rename(resolved_source.path(), resolved_target.path()) {
        return Ok(AtomicOutcome::failed(format!(
            "xeh_fs rename failed: {error}"
        )));
    }
    match File::open(&parent).and_then(|directory| directory.sync_all()) {
        Ok(()) => Ok(AtomicOutcome {
            committed: true,
            directory_synced: true,
            error: None,
        }),
        Err(error) => Ok(AtomicOutcome {
            committed: true,
            directory_synced: false,
            error: Some(format!("xeh_fs directory sync failed: {error}")),
        }),
    }
}

fn stage_file(parent: &Path, staging: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(staging).map_err(|error| {
        format!(
            "xeh_fs staging create failed in '{}': {error}",
            parent.display()
        )
    })
}
