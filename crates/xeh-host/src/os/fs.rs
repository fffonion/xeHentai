//! Named file-system primitives the pinned Core does not publish.
//!
//! The Core `io` surface is `open` / `popen` / `read_all` / `read_line` /
//! `write` / `flush` / `close` / `exists`. Session recovery additionally needs
//! a metadata query, the zero-byte cleanup the upstream scan performs, and one
//! atomic whole-file replacement. Those are the only three capabilities added
//! here, next to the two trusted-path accessors that keep the session target
//! out of the dispatch request:
//!
//! * [`install`] binds one [`FsState`] to a single VM. Download mutations are
//!   resolved only against the state's explicit download roots, and the exact
//!   session/legacy files stay reserved even inside one of those roots; a
//!   request can never widen either capability.
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
    HostTypeSchema, IoHostExt, Value, Vm, VmError, VmResult,
};

use crate::types::HostError;

/// The single byte budget for one session document: used by the Core read
/// bound and by the native atomic write.
pub const SESSION_READ_LIMIT_BYTES: usize = 16 * 1024 * 1024;

/// Stable diagnostic tag for a refused `xeh_fs` path authority check.
pub const ROOT_DENIED_TAG: &str = "xeh_fs:root_denied";

static STAGING_SEQ: AtomicU64 = AtomicU64::new(1);

/// Per-VM trusted file-system policy.
///
/// `roots` are the only directories `stat` / `remove_zero_file` /
/// `rename_atomic` may touch. `session_path` is the only path `write_atomic`
/// accepts; `legacy_cookie_path` is read-only. Both exact files are installed
/// by the embedding, never supplied by a guest request, and remain reserved
/// from the download-root mutation surface even when a root contains them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FsState {
    roots: Vec<PathBuf>,
    session_path: Option<PathBuf>,
    legacy_cookie_path: Option<PathBuf>,
    scan_budget: u64,
    max_session_bytes: usize,
}

impl Default for FsState {
    fn default() -> Self {
        Self {
            roots: Vec::new(),
            session_path: None,
            legacy_cookie_path: None,
            scan_budget: 100_000,
            max_session_bytes: SESSION_READ_LIMIT_BYTES,
        }
    }
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
            ..Self::default()
        }
    }

    /// Trusted test/embedding configuration for the aggregate scan budget and
    /// the session byte budget. Both defaults stay in force in production.
    pub fn with_limits(mut self, scan_budget: u64, max_session_bytes: usize) -> Self {
        self.scan_budget = scan_budget;
        self.max_session_bytes = max_session_bytes;
        self
    }

    pub fn roots(&self) -> &[PathBuf] {
        &self.roots
    }

    pub fn scan_budget(&self) -> u64 {
        self.scan_budget
    }

    pub fn session_byte_limit(&self) -> usize {
        self.max_session_bytes
    }

    pub fn session_path(&self) -> Option<&Path> {
        self.session_path.as_deref()
    }

    pub fn legacy_cookie_path(&self) -> Option<&Path> {
        self.legacy_cookie_path.as_deref()
    }

    fn existing_read_roots(&self) -> Vec<String> {
        [
            self.session_path.as_deref(),
            self.legacy_cookie_path.as_deref(),
        ]
        .into_iter()
        .flatten()
        .filter(|path| path.exists())
        .filter_map(|path| path.to_str().map(str::to_string))
        .collect()
    }

    /// Keeps the Core read surface at exactly the files that exist now.
    ///
    /// The Core policy resolves every configured root on each call, so an
    /// absent target cannot be listed as a root and a parent directory is not
    /// used as a substitute read authority. `session_paths` is the first call
    /// of every session operation, which makes the refresh cover a session
    /// file created by a previous save in the same VM.
    fn refresh_core_policy(&self, vm: &mut Vm) {
        vm.configure_io(vm::IoPolicy {
            allowed_roots: self.existing_read_roots(),
            allow_write: false,
            allow_process: false,
            max_read_bytes: self.max_session_bytes,
            max_write_bytes: self.max_session_bytes,
        });
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
        .with_description("Remove one zero-byte regular file inside the configured download roots"),
        HostFunctionSchema::with_return(
            "xeh_fs::write_atomic",
            vec![
                HostParamSchema::value("path", HostTypeSchema::String),
                HostParamSchema::value("contents", HostTypeSchema::String),
            ],
            named("AtomicWriteResult", atomic_fields()),
        )
        .with_description("Replace the trusted session file atomically within its byte budget"),
        HostFunctionSchema::with_return(
            "xeh_fs::rename_atomic",
            vec![
                HostParamSchema::value("source", HostTypeSchema::String),
                HostParamSchema::value("target", HostTypeSchema::String),
            ],
            named("AtomicWriteResult", atomic_fields()),
        )
        .with_description(
            "Rename one staged file over its target inside the configured download roots",
        ),
        HostFunctionSchema::with_return(
            "xeh_fs::session_paths",
            Vec::new(),
            named("SessionPaths", session_paths_fields()),
        )
        .with_description(
            "Read the trusted session/legacy targets, their existence and the scan budget",
        ),
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
        HostStructField::new("session_exists", HostTypeSchema::Bool),
        HostStructField::new("legacy_exists", HostTypeSchema::Bool),
        HostStructField::new("session_size", HostTypeSchema::Int),
        HostStructField::new("max_session_bytes", HostTypeSchema::Int),
        HostStructField::new("scan_budget", HostTypeSchema::Int),
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
        let outcome = remove_zero_file(&state, Path::new(&path))?;
        Ok(CallOutcome::Return(CallReturn::one(outcome.to_value())))
    }
}

pub struct ZeroOutcome {
    removed: bool,
    size: i64,
    error: Option<String>,
}

impl ZeroOutcome {
    pub fn removed(&self) -> bool {
        self.removed
    }

    pub fn size(&self) -> i64 {
        self.size
    }

    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn to_value(&self) -> Value {
        match &self.error {
            Some(message) => Value::map(vec![
                (Value::string("removed"), Value::Bool(self.removed)),
                (Value::string("size"), Value::Int(self.size)),
                (Value::string("error"), Value::string(message.clone())),
            ]),
            None => Value::map(vec![
                (Value::string("removed"), Value::Bool(self.removed)),
                (Value::string("size"), Value::Int(self.size)),
                (Value::string("error"), Value::Null),
            ]),
        }
    }
}

/// Removes one zero-byte regular file after the trusted-root check.
pub fn remove_zero_file(state: &FsState, path: &Path) -> VmResult<ZeroOutcome> {
    let resolved = resolve(state, &path.to_string_lossy())?;
    reserved(state, resolved.path())?;
    let Some(metadata) = resolved.metadata() else {
        return Ok(ZeroOutcome {
            removed: false,
            size: 0,
            error: None,
        });
    };
    if metadata.is_dir() {
        return Err(VmError::HostError(
            "xeh_fs remove_zero_file target is a directory".to_string(),
        ));
    }
    let size = i64::try_from(metadata.len()).unwrap_or(i64::MAX);
    if metadata.len() != 0 {
        return Ok(ZeroOutcome {
            removed: false,
            size,
            error: None,
        });
    }
    match fs::remove_file(resolved.path()) {
        Ok(()) => Ok(ZeroOutcome {
            removed: true,
            size: 0,
            error: None,
        }),
        Err(error) => Ok(ZeroOutcome {
            removed: false,
            size: 0,
            error: Some(format!("remove_zero_file failed: {error}")),
        }),
    }
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
        state.refresh_core_policy(vm);
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
        let session_exists = state
            .session_path()
            .map(|path| path.exists())
            .unwrap_or(false);
        let session_size = state
            .session_path()
            .and_then(|path| fs::metadata(path).ok())
            .map(|metadata| i64::try_from(metadata.len()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        let legacy_exists = state
            .legacy_cookie_path()
            .map(|path| path.exists())
            .unwrap_or(false);
        Ok(CallOutcome::Return(CallReturn::one(Value::map(vec![
            (
                Value::string("session_path"),
                Value::string(session_text.to_string()),
            ),
            (
                Value::string("legacy_cookie_path"),
                Value::string(legacy_text),
            ),
            (Value::string("session_exists"), Value::Bool(session_exists)),
            (Value::string("session_size"), Value::Int(session_size)),
            (
                Value::string("max_session_bytes"),
                Value::Int(i64::try_from(state.session_byte_limit()).unwrap_or(i64::MAX)),
            ),
            (Value::string("legacy_exists"), Value::Bool(legacy_exists)),
            (
                Value::string("scan_budget"),
                Value::Int(i64::try_from(state.scan_budget()).unwrap_or(i64::MAX)),
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

/// Resolves `requested` against the trusted download roots without replacing
/// the final component of the operation path.
///
/// A missing target is resolved through its nearest existing ancestor, so a
/// not-yet-created scan target still answers `exists = false` without widening
/// the root check. The requested path remains the unlink/rename path. If its
/// final component is a symlink, its canonical target must also be inside a
/// trusted root before metadata follows it. A dangling, cyclic, or otherwise
/// unresolved final symlink is conservatively refused as root-denied.
fn resolve(state: &FsState, requested: &str) -> Result<Resolved, VmError> {
    let path = resolve_without_final_follow(requested)?;
    check_roots(&state.roots, &path)?;
    match fs::symlink_metadata(&path) {
        Ok(entry_metadata) if entry_metadata.file_type().is_symlink() => {
            let target = fs::canonicalize(&path).map_err(|_| root_denied(&path))?;
            check_roots(&state.roots, &target)?;
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(VmError::HostError(format!("xeh_fs stat failed: {error}")));
        }
    }
    let metadata = match fs::metadata(&path) {
        Ok(metadata) => Some(metadata),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(VmError::HostError(format!("xeh_fs stat failed: {error}"))),
    };
    Ok(Resolved { path, metadata })
}

fn resolve_without_final_follow(requested: &str) -> Result<PathBuf, VmError> {
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
    let Some(name) = absolute.file_name() else {
        return Err(VmError::HostError(
            "xeh_fs path has no file name".to_string(),
        ));
    };
    let mut suffix: Vec<OsString> = vec![name.to_os_string()];
    let mut existing = absolute
        .parent()
        .ok_or_else(|| VmError::HostError("xeh_fs path has no parent".to_string()))?
        .to_path_buf();
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
    Ok(path)
}

fn check_roots(roots: &[PathBuf], path: &Path) -> Result<(), VmError> {
    for root in roots {
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
    Err(root_denied(path))
}

fn root_denied(path: &Path) -> VmError {
    VmError::HostError(format!(
        "{ROOT_DENIED_TAG}: xeh_fs path '{}' is outside the allowed roots",
        path.display()
    ))
}

/// Refuses an exact session/legacy target from the download-root mutation
/// surface, even when a download root happens to be the same directory.
fn reserved(state: &FsState, path: &Path) -> Result<(), VmError> {
    for target in [state.session_path(), state.legacy_cookie_path()]
        .into_iter()
        .flatten()
    {
        if let Ok(normalized) = resolve_without_final_follow(&target.to_string_lossy()) {
            if normalized == path {
                return Err(root_denied(path));
            }
        }
    }
    Ok(())
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
    let requested = resolve_without_final_follow(&path.to_string_lossy())?;
    let session = state.session_path().ok_or_else(|| {
        VmError::HostError(format!(
            "{ROOT_DENIED_TAG}: no trusted session target is configured for this VM"
        ))
    })?;
    let session_target = resolve_without_final_follow(&session.to_string_lossy())?;
    if requested != session_target {
        return Err(root_denied(&requested));
    }
    if let Some(legacy) = state.legacy_cookie_path() {
        if let Ok(normalized) = resolve_without_final_follow(&legacy.to_string_lossy()) {
            if normalized == requested {
                return Err(root_denied(&requested));
            }
        }
    }
    if let Ok(metadata) = fs::symlink_metadata(&requested) {
        if metadata.file_type().is_symlink() {
            return Ok(AtomicOutcome::failed(
                "xeh_fs:session_target_symlink: refusing to replace a symlink".to_string(),
            ));
        }
    }
    if contents.len() > state.session_byte_limit() {
        return Ok(AtomicOutcome::failed(format!(
            "xeh_fs:session_payload_limit: {} bytes exceeds the {} byte budget",
            contents.len(),
            state.session_byte_limit()
        )));
    }
    let parent = requested
        .parent()
        .ok_or_else(|| VmError::HostError("xeh_fs target has no parent".to_string()))?
        .to_path_buf();
    if !parent.is_dir() {
        return Ok(AtomicOutcome::failed(
            "xeh_fs parent directory does not exist".to_string(),
        ));
    }
    let name = requested
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
    if let Err(error) = fs::rename(&staging, &requested) {
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
    reserved(state, resolved_source.path())?;
    reserved(state, resolved_target.path())?;
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
