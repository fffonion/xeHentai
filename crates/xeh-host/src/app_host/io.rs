//! `xeh_io::try_write_text`: a resource-free whole-file UTF-8 writer on the
//! application namespace.
//!
//! The writer is asynchronous and uses the public generic host bridge: the
//! synchronous host call clones the per-VM [`vm::IoPolicy`] and submits one
//! future, which the VM's existing bridge polls. There is no second runtime,
//! thread pool, blocking owner-loop write, or copied authority store.
//!
//! Authorization runs before the file is opened or truncated:
//! `allow_write`, the UTF-8 byte budget and the allowed-root check all happen
//! first, and a missing target is resolved through its existing parent. The
//! final component is checked for a symlink so a link cannot redirect the
//! write outside the policy roots, including a dangling link whose target does
//! not exist yet. Absent policy denies.
//!
//! Ordinary outcomes become [`IoWriteTextResult`] data (`policy_denied`,
//! `write_limit`, `io_error`) and never a VM error; the value never carries the
//! path or the text. Writes are not atomic with respect to the file system: an
//! OS failure after opening may leave an empty or partial file. Success means
//! every byte was written and flushed and the handle was closed. Only argument
//! type errors or engine faults propagate as `VmError`.

use std::path::{Path, PathBuf};

use tokio::fs::OpenOptions;
use tokio::io::AsyncWriteExt;
use vm::{
    CallOutcome, CallReturn, HostFunction, HostFunctionSchema, HostFutureOutput, HostParamSchema,
    HostStructField, HostStructSchema, HostTypeSchema, Value, Vm, VmError, VmResult,
};

/// Guest name of the writer.
pub const IO_WRITE_TEXT_NAME: &str = "xeh_io::try_write_text";

/// Named result struct the writer returns.
pub const IO_WRITE_TEXT_RESULT: &str = "IoWriteTextResult";

const POLICY_DENIED: &str = "policy_denied";
const WRITE_LIMIT: &str = "write_limit";
const IO_ERROR: &str = "io_error";

/// Typed result returned to the guest.
///
/// `error_code` is empty on success and one of the stable codes otherwise; it
/// never contains the path, the text, or an OS message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IoWriteTextResult {
    pub ok: bool,
    pub error_code: String,
}

impl IoWriteTextResult {
    fn success() -> Self {
        Self {
            ok: true,
            error_code: String::new(),
        }
    }

    fn failure(code: &str) -> Self {
        Self {
            ok: false,
            error_code: code.to_string(),
        }
    }

    fn into_value(self) -> Value {
        Value::map(vec![
            (Value::string("ok"), Value::Bool(self.ok)),
            (Value::string("error_code"), Value::string(self.error_code)),
        ])
    }
}

fn result_fields() -> Vec<HostStructField> {
    vec![
        HostStructField::new("ok", HostTypeSchema::Bool),
        HostStructField::new("error_code", HostTypeSchema::String),
    ]
}

/// The named struct body the guest schema references.
pub fn structs() -> Vec<HostStructSchema> {
    vec![
        HostStructSchema::new(IO_WRITE_TEXT_RESULT, result_fields()).with_description(
            "Typed result of a whole-file UTF-8 text write; error_code is empty on success.",
        ),
    ]
}

/// Guest contract for the writer.
pub fn try_write_text_schema() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        IO_WRITE_TEXT_NAME,
        vec![
            HostParamSchema::value("path", HostTypeSchema::String),
            HostParamSchema::value("text", HostTypeSchema::String),
        ],
        HostTypeSchema::named_struct(IO_WRITE_TEXT_RESULT, result_fields()),
    )
    .with_description(
        "Create or truncate one UTF-8 text file inside the configured IO roots. Ordinary \
         policy, size and OS failures return IoWriteTextResult; a later OS error may leave \
         partial bytes.",
    )
}

pub(crate) struct TryWriteTextFn;

impl HostFunction for TryWriteTextFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let path = string_arg(args.first(), "path")?;
        let text = string_arg(args.get(1), "text")?;
        let policy = vm.host_context().module_state::<vm::IoPolicy>().cloned();
        let future = async move {
            let result = write_with_policy(policy.as_ref(), &path, &text).await;
            Ok(HostFutureOutput::returning(CallReturn::one(
                result.into_value(),
            )))
        };
        vm.submit_host_future(Box::pin(future))
    }
}

fn string_arg(value: Option<&Value>, name: &str) -> VmResult<String> {
    match value {
        Some(Value::String(text)) => Ok(text.as_str().to_string()),
        _ => Err(VmError::HostError(format!(
            "{IO_WRITE_TEXT_NAME} expects a string '{name}'"
        ))),
    }
}

async fn write_with_policy(
    policy: Option<&vm::IoPolicy>,
    requested: &str,
    text: &str,
) -> IoWriteTextResult {
    let Some(policy) = policy else {
        return IoWriteTextResult::failure(POLICY_DENIED);
    };
    if !policy.allow_write {
        return IoWriteTextResult::failure(POLICY_DENIED);
    }
    let target = match authorize(policy, requested).await {
        Ok(target) => target,
        Err(code) => return IoWriteTextResult::failure(code),
    };
    if text.len() > policy.max_write_bytes {
        return IoWriteTextResult::failure(WRITE_LIMIT);
    }
    match write_all(&target, text).await {
        Ok(()) => IoWriteTextResult::success(),
        Err(_) => IoWriteTextResult::failure(IO_ERROR),
    }
}

async fn authorize(policy: &vm::IoPolicy, requested: &str) -> Result<PathBuf, &'static str> {
    let requested = PathBuf::from(requested);
    let absolute = if requested.is_absolute() {
        requested
    } else {
        std::env::current_dir()
            .map_err(|_| IO_ERROR)?
            .join(requested)
    };
    let canonical = canonicalize_target(&absolute).await?;
    for root in &policy.allowed_roots {
        let root = tokio::fs::canonicalize(root).await.map_err(|_| IO_ERROR)?;
        if canonical.starts_with(&root) {
            return Ok(canonical);
        }
    }
    Err(POLICY_DENIED)
}

/// Resolves the write target without letting a final symlink escape the roots.
///
/// An existing target is canonicalized (following a final link, whose resolved
/// path must still pass the root check). A missing target is canonicalized
/// through its parent and keeps the unopened leaf name. A final component that
/// is a symlink is always resolved here: a dangling or otherwise unresolvable
/// link is refused as a policy denial instead of being opened, which would let
/// the OS follow it later.
async fn canonicalize_target(path: &Path) -> Result<PathBuf, &'static str> {
    match tokio::fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_symlink() => tokio::fs::canonicalize(path)
            .await
            .map_err(|_| POLICY_DENIED),
        Ok(_) => tokio::fs::canonicalize(path).await.map_err(|_| IO_ERROR),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or(IO_ERROR)?;
            let name = path.file_name().ok_or(IO_ERROR)?;
            let parent = tokio::fs::canonicalize(parent)
                .await
                .map_err(|_| IO_ERROR)?;
            Ok(parent.join(name))
        }
        Err(_) => Err(IO_ERROR),
    }
}

async fn write_all(path: &Path, text: &str) -> std::io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await?;
    file.write_all(text.as_bytes()).await?;
    file.flush().await?;
    file.shutdown().await?;
    drop(file);
    Ok(())
}
