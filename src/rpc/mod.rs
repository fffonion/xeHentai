//! Original RPC module `xeHentai/rpc.py`: JSON-RPC dispatch and gallery views.
//!
//! The business decisions live in `rss/rpc/handlers.rss`: method lookup, the
//! camelCase mapping, the 13-method allowlist, arity/defaults, the
//! updateConfig key filter and proxy no-op rule, the listTasks level
//! selector, the getImage range quirk and the public field projections. This
//! module only compiles and binds that script, converts the named typed
//! requests into script values, converts the named results back, performs
//! the resulting `App` operations through short borrows, and serializes the
//! RSS-selected values. It never decides a method, a default, a filter or a
//! projection itself.
//!
//! Wire framing (HTTP/1.0 close semantics, the trailing newline, CORS and
//! the `/img` `/zip` `/ui` `/static` routes) lives in [`server`].
#![allow(dead_code)]

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, OnceLock};

use serde_json::{Map as JsonMap, Value};
use vm::{
    CompileSourceFileOptions, HostApiBuilder, HostApiCatalog, HostFunctionRegistry, SourceFlavor,
    Value as VmValue, Vm, VmStatus, compile_source_with_flavor_and_options,
};

pub mod server;

pub const RPC_RSS: &str = "rss/rpc/handlers.rss";

#[derive(Debug)]
pub enum RpcError {
    Io { path: PathBuf, message: String },
    Compile(String),
    Bind(String),
    Runtime(String),
    Shape(String),
}

impl std::fmt::Display for RpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Compile(message) => write!(f, "rpc script compile error: {message}"),
            Self::Bind(message) => write!(f, "rpc host binding error: {message}"),
            Self::Runtime(message) => write!(f, "rpc script runtime error: {message}"),
            Self::Shape(message) => write!(f, "rpc result shape error: {message}"),
        }
    }
}

impl std::error::Error for RpcError {}

fn rpc_catalog() -> Arc<HostApiCatalog> {
    static CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();
    Arc::clone(CATALOG.get_or_init(|| {
        let builder = HostApiBuilder::new();
        Arc::new(builder.build().expect("rpc catalog must build"))
    }))
}

fn rpc_registry() -> &'static HostFunctionRegistry {
    static REGISTRY: OnceLock<HostFunctionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = HostFunctionRegistry::restricted();
        let profile = vm::CapabilityProfile::builder().build();
        registry.set_capability_profile(profile);
        registry
    })
}

fn compile_module(source: &str) -> Result<Vm, RpcError> {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(rpc_catalog());
    let compiled =
        compile_source_with_flavor_and_options(source, SourceFlavor::RustScript, options)
            .map_err(|error| RpcError::Compile(error.to_string()))?;
    let mut vm = Vm::new(compiled.program);
    rpc_registry()
        .bind_vm_cached(&mut vm)
        .map_err(|error| RpcError::Bind(error.to_string()))?;
    let status = vm
        .run()
        .map_err(|error| RpcError::Runtime(error.to_string()))?;
    if status != VmStatus::Halted {
        return Err(RpcError::Runtime(format!(
            "rpc module ended with {status:?}"
        )));
    }
    Ok(vm)
}

/// Typed view of `MethodSelect` in `rss/rpc/handlers.rss`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MethodSelect {
    pub outcome: String,
    pub method: String,
}

/// Typed view of `Arity` in `rss/rpc/handlers.rss`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arity {
    pub outcome: String,
}

/// Typed view of `UpdateFilter` in `rss/rpc/handlers.rss`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateFilter {
    pub kept: Vec<String>,
    pub invoke: bool,
}

/// Typed view of `LevelSelect` in `rss/rpc/handlers.rss`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LevelSelect {
    pub outcome: String,
    pub canonical: String,
}

/// Typed view of `RangeSelect` in `rss/rpc/handlers.rss`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RangeSelect {
    pub outcome: String,
    pub start: i64,
    pub end: i64,
}

fn object<'a>(value: &'a VmValue, what: &str) -> Result<&'a vm::VmMap, RpcError> {
    match value {
        VmValue::Map(map) => Ok(map),
        other => Err(RpcError::Shape(format!("{what} is not a map: {other:?}"))),
    }
}

fn field<'a>(map: &'a vm::VmMap, name: &str) -> Option<&'a VmValue> {
    map.get(&VmValue::string(name))
}

fn text(map: &vm::VmMap, name: &str) -> Result<Option<String>, RpcError> {
    match field(map, name) {
        None | Some(VmValue::Null) => Ok(None),
        Some(VmValue::String(value)) => Ok(Some((**value).clone())),
        Some(other) => Err(RpcError::Shape(format!(
            "field {name} is not a string: {other:?}"
        ))),
    }
}

fn required_text(map: &vm::VmMap, name: &str) -> Result<String, RpcError> {
    text(map, name)?.ok_or_else(|| RpcError::Shape(format!("field {name} is missing")))
}

fn flag(map: &vm::VmMap, name: &str) -> Result<bool, RpcError> {
    match field(map, name) {
        None | Some(VmValue::Null) => Ok(false),
        Some(VmValue::Bool(value)) => Ok(*value),
        Some(other) => Err(RpcError::Shape(format!(
            "field {name} is not a bool: {other:?}"
        ))),
    }
}

fn integer(map: &vm::VmMap, name: &str) -> Result<i64, RpcError> {
    match field(map, name) {
        Some(VmValue::Int(value)) => Ok(*value),
        other => Err(RpcError::Shape(format!(
            "field {name} is not an int: {other:?}"
        ))),
    }
}

fn strings(map: &vm::VmMap, name: &str) -> Result<Vec<String>, RpcError> {
    match field(map, name) {
        Some(VmValue::Array(items)) => items
            .iter()
            .map(|item| match item {
                VmValue::String(value) => Ok((**value).clone()),
                other => Err(RpcError::Shape(format!(
                    "field {name} contains a non-string: {other:?}"
                ))),
            })
            .collect(),
        None | Some(VmValue::Null) => Ok(Vec::new()),
        Some(other) => Err(RpcError::Shape(format!(
            "field {name} is not an array: {other:?}"
        ))),
    }
}

/// The compiled `rss/rpc/handlers.rss` business rules with one VM per owner.
///
/// Like [`crate::filters::Filters`], the VM is owned by the requesting
/// context (the RPC server task) and every call is a real compile/bind/call
/// through the same catalog; nothing is precomputed in Rust.
pub struct RpcDispatcher {
    vm: Vm,
    method_select: VmValue,
    add_task_arity: VmValue,
    guid_arity: VmValue,
    noarg_arity: VmValue,
    login_arity: VmValue,
    update_config_filter: VmValue,
    level_select: VmValue,
    image_range: VmValue,
    config_keep: VmValue,
    task_keep: VmValue,
    config_fields: VmValue,
    task_fields: VmValue,
}

impl RpcDispatcher {
    pub fn load(resource_root: &Path) -> Result<Self, RpcError> {
        let path = resource_root.join(RPC_RSS);
        let source = std::fs::read_to_string(&path).map_err(|error| RpcError::Io {
            path,
            message: error.to_string(),
        })?;
        Self::from_source(&source)
    }

    pub fn from_source(source: &str) -> Result<Self, RpcError> {
        let vm = compile_module(source)?;
        let resolve = |vm: &Vm, name: &str| {
            vm.resolve_exported_callable(name)
                .map_err(|error| RpcError::Runtime(format!("{name}: {error}")))
        };
        let method_select = resolve(&vm, "rpc_method_select")?;
        let add_task_arity = resolve(&vm, "rpc_add_task_arity")?;
        let guid_arity = resolve(&vm, "rpc_guid_arity")?;
        let noarg_arity = resolve(&vm, "rpc_noarg_arity")?;
        let login_arity = resolve(&vm, "rpc_login_arity")?;
        let update_config_filter = resolve(&vm, "rpc_update_config_filter")?;
        let level_select = resolve(&vm, "rpc_level_select")?;
        let image_range = resolve(&vm, "rpc_image_range")?;
        let config_keep = resolve(&vm, "rpc_config_keep")?;
        let task_keep = resolve(&vm, "rpc_task_keep")?;
        let config_fields = resolve(&vm, "rpc_config_fields")?;
        let task_fields = resolve(&vm, "rpc_task_fields")?;
        Ok(Self {
            vm,
            method_select,
            add_task_arity,
            guid_arity,
            noarg_arity,
            login_arity,
            update_config_filter,
            level_select,
            image_range,
            config_keep,
            task_keep,
            config_fields,
            task_fields,
        })
    }

    fn invoke(&mut self, callable: &VmValue, args: &[VmValue]) -> Result<VmValue, RpcError> {
        self.vm
            .invoke_callable(callable.clone(), args)
            .map_err(|error| RpcError::Runtime(error.to_string()))
    }

    fn invoke0(&mut self, name: &str) -> Result<VmValue, RpcError> {
        let callable = match name {
            "config_fields" => self.config_fields.clone(),
            "task_fields" => self.task_fields.clone(),
            _ => return Err(RpcError::Shape(format!("unknown callable {name}"))),
        };
        self.invoke(&callable, &[])
    }

    pub fn method_select(&mut self, raw: &str) -> Result<MethodSelect, RpcError> {
        let callable = self.method_select.clone();
        let value = self.invoke(&callable, &[VmValue::string(raw.to_string())])?;
        let map = object(&value, "method select")?;
        Ok(MethodSelect {
            outcome: required_text(map, "outcome")?,
            method: required_text(map, "method")?,
        })
    }

    pub fn add_task_arity(&mut self, argc: i64, kw_names: &[String]) -> Result<Arity, RpcError> {
        let callable = self.add_task_arity.clone();
        let names = kw_names
            .iter()
            .map(|name| VmValue::string(name.clone()))
            .collect();
        let value = self.invoke(&callable, &[VmValue::Int(argc), VmValue::array(names)])?;
        Ok(Arity {
            outcome: required_text(object(&value, "arity")?, "outcome")?,
        })
    }

    pub fn guid_arity(&mut self, argc: i64, kw_count: i64) -> Result<Arity, RpcError> {
        let callable = self.guid_arity.clone();
        let value = self.invoke(&callable, &[VmValue::Int(argc), VmValue::Int(kw_count)])?;
        Ok(Arity {
            outcome: required_text(object(&value, "arity")?, "outcome")?,
        })
    }

    pub fn noarg_arity(&mut self, argc: i64, kw_count: i64) -> Result<Arity, RpcError> {
        let callable = self.noarg_arity.clone();
        let value = self.invoke(&callable, &[VmValue::Int(argc), VmValue::Int(kw_count)])?;
        Ok(Arity {
            outcome: required_text(object(&value, "arity")?, "outcome")?,
        })
    }

    pub fn login_arity(&mut self, argc: i64, kw_count: i64) -> Result<Arity, RpcError> {
        let callable = self.login_arity.clone();
        let value = self.invoke(&callable, &[VmValue::Int(argc), VmValue::Int(kw_count)])?;
        Ok(Arity {
            outcome: required_text(object(&value, "arity")?, "outcome")?,
        })
    }

    pub fn update_config_filter(&mut self, names: &[String]) -> Result<UpdateFilter, RpcError> {
        let callable = self.update_config_filter.clone();
        let input = names
            .iter()
            .map(|name| VmValue::string(name.clone()))
            .collect();
        let value = self.invoke(&callable, &[VmValue::array(input)])?;
        let map = object(&value, "update filter")?;
        Ok(UpdateFilter {
            kept: strings(map, "kept")?,
            invoke: flag(map, "invoke")?,
        })
    }

    pub fn level_select(
        &mut self,
        has_pos: bool,
        has_kw: bool,
        level: &str,
    ) -> Result<LevelSelect, RpcError> {
        let callable = self.level_select.clone();
        let value = self.invoke(
            &callable,
            &[
                VmValue::Bool(has_pos),
                VmValue::Bool(has_kw),
                VmValue::string(level.to_string()),
            ],
        )?;
        let map = object(&value, "level select")?;
        Ok(LevelSelect {
            outcome: required_text(map, "outcome")?,
            canonical: text(map, "canonical")?.unwrap_or_default(),
        })
    }

    pub fn image_range(&mut self, has_range: bool, text: &str) -> Result<RangeSelect, RpcError> {
        let callable = self.image_range.clone();
        let value = self.invoke(
            &callable,
            &[VmValue::Bool(has_range), VmValue::string(text.to_string())],
        )?;
        let map = object(&value, "range select")?;
        Ok(RangeSelect {
            outcome: required_text(map, "outcome")?,
            start: integer(map, "start")?,
            end: integer(map, "end")?,
        })
    }

    pub fn config_keep(&mut self, names: &[String]) -> Result<Vec<String>, RpcError> {
        let callable = self.config_keep.clone();
        let input = names
            .iter()
            .map(|name| VmValue::string(name.clone()))
            .collect();
        let value = self.invoke(&callable, &[VmValue::array(input)])?;
        match &value {
            VmValue::Array(items) => items
                .iter()
                .map(|item| match item {
                    VmValue::String(name) => Ok((**name).clone()),
                    other => Err(RpcError::Shape(format!(
                        "config keep list holds a non-string: {other:?}"
                    ))),
                })
                .collect(),
            other => Err(RpcError::Shape(format!(
                "config keep result is not an array: {other:?}"
            ))),
        }
    }

    pub fn task_keep(&mut self, names: &[String]) -> Result<Vec<String>, RpcError> {
        let callable = self.task_keep.clone();
        let input = names
            .iter()
            .map(|name| VmValue::string(name.clone()))
            .collect();
        let value = self.invoke(&callable, &[VmValue::array(input)])?;
        match &value {
            VmValue::Array(items) => items
                .iter()
                .map(|item| match item {
                    VmValue::String(name) => Ok((**name).clone()),
                    other => Err(RpcError::Shape(format!(
                        "task keep list holds a non-string: {other:?}"
                    ))),
                })
                .collect(),
            other => Err(RpcError::Shape(format!(
                "task keep result is not an array: {other:?}"
            ))),
        }
    }

    pub fn config_fields(&mut self) -> Result<Vec<String>, RpcError> {
        let value = self.invoke0("config_fields")?;
        match &value {
            VmValue::Array(items) => items
                .iter()
                .map(|item| match item {
                    VmValue::String(name) => Ok((**name).clone()),
                    other => Err(RpcError::Shape(format!(
                        "config field list holds a non-string: {other:?}"
                    ))),
                })
                .collect(),
            other => Err(RpcError::Shape(format!(
                "config field list is not an array: {other:?}"
            ))),
        }
    }

    pub fn task_fields(&mut self) -> Result<Vec<String>, RpcError> {
        let value = self.invoke0("task_fields")?;
        match &value {
            VmValue::Array(items) => items
                .iter()
                .map(|item| match item {
                    VmValue::String(name) => Ok((**name).clone()),
                    other => Err(RpcError::Shape(format!(
                        "task field list holds a non-string: {other:?}"
                    ))),
                })
                .collect(),
            other => Err(RpcError::Shape(format!(
                "task field list is not an array: {other:?}"
            ))),
        }
    }
}

/// `rpc.py:79-83`: `md5(secret-or-empty + "-xehentai-" + uri)` first 8 hex.
///
/// Product link signing/validation for the M5 gallery paths (`getImage`,
/// `/img`, `/zip`). Executes real MD5 per the fixed reference algorithm;
/// the M5 product-signing authorization covers exactly this function plus
/// its synthetic oracle/regression/gallery/browser executions.
pub fn hash_link(secret: Option<&str>, uri: &str) -> String {
    let text = format!("{}-xehentai-{uri}", secret.unwrap_or(""));
    let digest = md5::compute(text.as_bytes());
    format!("{digest:08x}").chars().take(8).collect()
}

/// Exact-literal token check replacing the reference substring regex
/// (`rpc.py:363` `re.findall("token:%s" % secret, secret)`).
///
/// The reference accepted any first argument merely *containing*
/// `token:<secret>`; the candidate requires the whole argument to equal
/// `token:<secret>` and never places the secret in a regex, a log line or a
/// script view. Recorded security delta, not a silent comparator ignore.
pub fn check_auth(secret: Option<&str>, first: Option<&str>) -> bool {
    match secret {
        None => true,
        Some(secret) => first.is_some_and(|candidate| {
            let mut expected = String::with_capacity(secret.len() + 6);
            expected.push_str("token:");
            expected.push_str(secret);
            constant_eq(candidate.as_bytes(), expected.as_bytes())
        }),
    }
}

fn constant_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut acc = 0u8;
    for (l, r) in left.iter().zip(right.iter()) {
        acc |= l ^ r;
    }
    acc == 0
}

/// The reference pops a stray `token:` first argument even on an open server
/// (`rpc.py:373-375`); the substring test for the *pop* is preserved while
/// authorization itself is exact.
pub fn pop_stray_token(args: &mut Vec<String>) {
    if args.first().is_some_and(|first| first.contains("token:")) {
        args.remove(0);
    }
}

/// `rpc.py:107-119`: falsy ids (`0`, `""`, `null`, `false`) encode as `None`.
/// A missing or non-scalar id is rejected before this point.
pub fn response_id(id: &Value) -> Value {
    let falsy = match id {
        Value::Null => true,
        Value::Bool(value) => !value,
        Value::Number(number) => {
            number.as_i64().is_some_and(|n| n == 0)
                || number.as_u64().is_some_and(|n| n == 0)
                || number.as_f64().is_some_and(|n| n == 0.0)
        }
        Value::String(text) => text.is_empty(),
        Value::Array(items) => items.is_empty(),
        Value::Object(fields) => fields.is_empty(),
    };
    if falsy { Value::Null } else { id.clone() }
}

/// Build a JSON-RPC response envelope for one method outcome.
pub fn jsonrpc_envelope(
    id: &Value,
    code: i64,
    result: Option<Value>,
    message: Option<String>,
) -> Value {
    let mut envelope = JsonMap::new();
    envelope.insert("id".to_string(), response_id(id));
    envelope.insert("jsonrpc".to_string(), Value::String("2.0".to_string()));
    if code != 0 {
        let text =
            message.unwrap_or_else(|| crate::i18n::err_msg(crate::i18n::Locale::EnUs, code, &[]));
        let mut error = JsonMap::new();
        error.insert("code".to_string(), Value::Number(code.into()));
        error.insert("message".to_string(), Value::String(text));
        envelope.insert("error".to_string(), Value::Object(error));
    } else {
        envelope.insert("result".to_string(), result.unwrap_or(Value::Null));
    }
    Value::Object(envelope)
}

/// `rpc.py:452-460` MIME map, including the reference `jepg` key.
pub fn image_mime(ext: &str) -> &'static str {
    match ext.to_lowercase().as_str() {
        "jpg" => "image/jpeg",
        "jepg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        _ => "application/octet-stream",
    }
}

#[derive(Debug)]
pub enum ServerError {
    Io(String),
    Rpc(RpcError),
    Shape(String),
}

impl std::fmt::Display for ServerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(message) => write!(f, "rpc server I/O: {message}"),
            Self::Rpc(error) => write!(f, "{error}"),
            Self::Shape(message) => write!(f, "rpc server shape: {message}"),
        }
    }
}

impl std::error::Error for ServerError {}

impl From<RpcError> for ServerError {
    fn from(error: RpcError) -> Self {
        Self::Rpc(error)
    }
}

impl From<std::io::Error> for ServerError {
    fn from(error: std::io::Error) -> Self {
        Self::Io(error.to_string())
    }
}

/// One decoded JSON-RPC call. `args`/`kwargs` keep the arbitrary wire JSON;
/// method-specific DTO mapping happens in [`execute`].
#[derive(Debug)]
pub struct RpcRequest {
    pub method_raw: String,
    pub id: Value,
    pub args: Vec<Value>,
    pub kwargs: JsonMap<String, Value>,
}

/// Outcome of one method execution: the HTTP status plus the envelope parts.
#[derive(Debug)]
pub struct ExecOutcome {
    pub status: u16,
    pub code: i64,
    pub result: Option<Value>,
    pub message: Option<String>,
}

impl ExecOutcome {
    fn ok(result: Value) -> Self {
        Self {
            status: 200,
            code: 0,
            result: Some(result),
            message: None,
        }
    }

    fn method_error(code: i64, message: Option<String>) -> Self {
        Self {
            status: 200,
            code,
            result: None,
            message,
        }
    }

    fn http(status: u16, code: i64, message: Option<String>) -> Self {
        Self {
            status,
            code,
            result: None,
            message,
        }
    }

    fn exec_error(message: String) -> Self {
        Self::http(500, crate::r#const::ERR_RPC_EXEC_ERROR, Some(message))
    }
}

/// Parse one POST body into a call. Returns the HTTP status and error code
/// for wire failures; method-level failures stay HTTP 200 by contract.
pub fn parse_jsonrpc(body: &[u8]) -> Result<RpcRequest, (u16, i64)> {
    let text = std::str::from_utf8(body).map_err(|_| (400, crate::r#const::ERR_RPC_PARSE_ERROR))?;
    let root: Value =
        serde_json::from_str(text).map_err(|_| (400, crate::r#const::ERR_RPC_PARSE_ERROR))?;
    let object = root
        .as_object()
        .ok_or((400, crate::r#const::ERR_RPC_INVALID_REQUEST))?;
    if !object.contains_key("id") {
        return Err((400, crate::r#const::ERR_RPC_INVALID_REQUEST));
    }
    let method = object
        .get("method")
        .ok_or((400, crate::r#const::ERR_RPC_INVALID_REQUEST))?;
    if method.is_null() {
        return Err((400, crate::r#const::ERR_RPC_INVALID_REQUEST));
    }
    let method_raw = method
        .as_str()
        .ok_or((500, crate::r#const::ERR_RPC_EXEC_ERROR))?;
    let id = object.get("id").cloned().unwrap_or(Value::Null);
    let (args, kwargs) = match object.get("params") {
        None => (Vec::new(), JsonMap::new()),
        Some(Value::Array(pair)) if pair.len() == 2 => {
            let args = pair[0]
                .as_array()
                .ok_or((400, crate::r#const::ERR_RPC_INVALID_REQUEST))?
                .clone();
            let kwargs = pair[1]
                .as_object()
                .ok_or((400, crate::r#const::ERR_RPC_INVALID_REQUEST))?
                .clone();
            (args, kwargs)
        }
        Some(_) => return Err((400, crate::r#const::ERR_RPC_INVALID_REQUEST)),
    };
    Ok(RpcRequest {
        method_raw: method_raw.to_string(),
        id,
        args,
        kwargs,
    })
}

fn wire_string(value: &Value) -> Option<String> {
    value.as_str().map(str::to_string)
}

/// Python `str()` of a truthy `request_range`, mirroring `rpc.py:495`.
/// Only reachable shapes are mapped; anything else is an exec error.
fn python_range_text(value: &Value) -> Option<String> {
    match value {
        Value::Null => None,
        Value::Bool(true) => Some("True".to_string()),
        Value::Bool(false) => None,
        Value::Number(number) => Some(number.to_string()),
        Value::String(text) => Some(text.clone()),
        _ => Some(String::new()),
    }
}

fn range_is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(flag) => *flag,
        Value::Number(number) => {
            number.as_i64().is_some_and(|n| n != 0)
                || number.as_u64().is_some_and(|n| n != 0)
                || number.as_f64().is_some_and(|n| n != 0.0)
        }
        Value::String(text) => !text.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::Object(fields) => !fields.is_empty(),
    }
}

/// Execute one decoded call against the single shared `App`. The borrow is
/// short and synchronous; gallery/archive/thumbnail bytes are produced here
/// so the connection task never holds the borrow over an await.
///
/// `secret` is the configured server secret (exact-literal check); `info` is
/// the runtime `get_info` view, falling back to zeros without a monitor.
#[allow(clippy::too_many_lines)]
pub fn execute(
    app: &mut crate::core::App,
    rpc: &mut RpcDispatcher,
    req: &RpcRequest,
    secret: Option<&str>,
    info: Option<Value>,
) -> ExecOutcome {
    let prepared = match prepare_call(rpc, req, secret) {
        Ok(prepared) => prepared,
        Err(outcome) => return outcome,
    };
    execute_prepared(
        app,
        rpc,
        &prepared.method,
        &prepared.args,
        &req.kwargs,
        secret,
        info,
    )
}

/// One authenticated, token-consumed call: the canonical RSS method plus the
/// positional arguments after the single token pop. Both [`execute`] and the
/// server dispatch prepare exactly this, once; downstream code never
/// re-authenticates or re-pops.
#[derive(Debug)]
pub(crate) struct PreparedCall {
    pub method: String,
    pub args: Vec<Value>,
}

/// Normalize through the RSS selector, authenticate the optional first token
/// and consume it, exactly once. Lookup runs before auth (like
/// `rpc.py:343-370`), then a single pop (`rpc.py:373-375`): the whole secret
/// token when a secret is configured, else a stray `token:`-looking first
/// argument on an open server.
pub(crate) fn prepare_call(
    rpc: &mut RpcDispatcher,
    req: &RpcRequest,
    secret: Option<&str>,
) -> Result<PreparedCall, ExecOutcome> {
    use crate::r#const as consts;
    let selected = match rpc.method_select(&req.method_raw) {
        Ok(selected) => selected,
        Err(error) => return Err(ExecOutcome::exec_error(error.to_string())),
    };
    if selected.outcome != "ok" {
        return Err(ExecOutcome::http(
            404,
            consts::ERR_RPC_METHOD_NOT_FOUND,
            None,
        ));
    }
    // Authorization (`rpc.py:355-370`): exact literal `token:<secret>`.
    let first_raw = req.args.first().and_then(wire_string);
    if !check_auth(secret, first_raw.as_deref()) {
        return Err(ExecOutcome::http(403, consts::ERR_RPC_UNAUTHORIZED, None));
    }
    let mut args = req.args.clone();
    if secret.is_some() {
        args.remove(0);
    } else if req
        .args
        .first()
        .and_then(|value| value.as_str())
        .is_some_and(|text| text.contains("token:"))
    {
        // Preserve the open-server stray-token pop (`rpc.py:373-375`).
        args.remove(0);
    }
    Ok(PreparedCall {
        method: selected.method,
        args,
    })
}

/// Run a prepared call: no method lookup, no authentication, no token pop.
/// `secret` here only signs getImage URLs; the token was already consumed.
#[allow(clippy::too_many_lines)]
pub(crate) fn execute_prepared(
    app: &mut crate::core::App,
    rpc: &mut RpcDispatcher,
    method: &str,
    args: &[Value],
    kwargs: &JsonMap<String, Value>,
    secret: Option<&str>,
    info: Option<Value>,
) -> ExecOutcome {
    use crate::r#const as consts;
    let argc = args.len() as i64;
    let kw_count = kwargs.len() as i64;
    match method {
        "add_task" => {
            let kw_names: Vec<String> = kwargs.keys().cloned().collect();
            let arity = match rpc.add_task_arity(argc, &kw_names) {
                Ok(arity) => arity,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if arity.outcome != "ok" {
                return ExecOutcome::exec_error(format!(
                    "add_task() {}",
                    arity_message(&arity.outcome, "'url'")
                ));
            }
            let Some(url) = args.first().and_then(wire_string) else {
                return ExecOutcome::exec_error(
                    "add_task() argument 'url' must be a string".to_string(),
                );
            };
            match app.add_task(&url, kwargs.clone()) {
                Ok((code, guid)) => {
                    if code != 0 {
                        ExecOutcome::method_error(code, None)
                    } else {
                        ExecOutcome::ok(guid.map(Value::String).unwrap_or(Value::Null))
                    }
                }
                Err(error) => ExecOutcome::exec_error(error.to_string()),
            }
        }
        "update_config" => {
            let names: Vec<String> = kwargs.keys().cloned().collect();
            let filter = match rpc.update_config_filter(&names) {
                Ok(filter) => filter,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if !filter.invoke {
                return public_config(app, rpc);
            }
            let mut patch = JsonMap::new();
            for name in &filter.kept {
                if let Some(value) = kwargs.get(name) {
                    patch.insert(name.clone(), value.clone());
                }
            }
            if let Err(error) = app.update_config(patch) {
                return ExecOutcome::exec_error(error.to_string());
            }
            public_config(app, rpc)
        }
        "del_task" => {
            let arity = match rpc.guid_arity(argc, kw_count) {
                Ok(arity) => arity,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if arity.outcome != "ok" {
                return ExecOutcome::exec_error(format!(
                    "del_task() {}",
                    arity_message(&arity.outcome, "'guid'")
                ));
            }
            let Some(guid) = args.first().and_then(wire_string) else {
                return ExecOutcome::exec_error(
                    "del_task() argument 'guid' must be a string".to_string(),
                );
            };
            match app.del_task(&guid) {
                Ok((code, _)) => {
                    if code != 0 {
                        ExecOutcome::method_error(code, None)
                    } else {
                        ExecOutcome::ok(Value::String(String::new()))
                    }
                }
                Err(error) => ExecOutcome::exec_error(error.to_string()),
            }
        }
        "pause_task" | "resume_task" => {
            let arity = match rpc.guid_arity(argc, kw_count) {
                Ok(arity) => arity,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if arity.outcome != "ok" {
                return ExecOutcome::exec_error(format!(
                    "{method}() {}",
                    arity_message(&arity.outcome, "'guid'")
                ));
            }
            let Some(guid) = args.first().and_then(wire_string) else {
                return ExecOutcome::exec_error(format!(
                    "{method}() argument 'guid' must be a string"
                ));
            };
            let result = if method == "pause_task" {
                app.pause_task(&guid)
            } else {
                app.resume_task(&guid)
            };
            match result {
                Ok((code, _)) => {
                    if code != 0 {
                        ExecOutcome::method_error(code, None)
                    } else {
                        ExecOutcome::ok(Value::String(String::new()))
                    }
                }
                Err(error) => ExecOutcome::exec_error(error.to_string()),
            }
        }
        "save_session" => {
            let arity = match rpc.noarg_arity(argc, kw_count) {
                Ok(arity) => arity,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if arity.outcome != "ok" {
                return ExecOutcome::exec_error(format!(
                    "save_session() {}",
                    arity_message(&arity.outcome, "")
                ));
            }
            let (code, message) = app.save_session();
            if code != 0 {
                ExecOutcome::method_error(code, message)
            } else {
                ExecOutcome::ok(Value::Null)
            }
        }
        "load_session" => {
            let arity = match rpc.noarg_arity(argc, kw_count) {
                Ok(arity) => arity,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if arity.outcome != "ok" {
                return ExecOutcome::exec_error(format!(
                    "load_session() {}",
                    arity_message(&arity.outcome, "")
                ));
            }
            match app.load_session() {
                Ok(()) => ExecOutcome::ok(Value::Null),
                Err(error) if error.is_recoverable() => ExecOutcome::ok(Value::Null),
                Err(error) => ExecOutcome::method_error(
                    consts::ERR_SAVE_SESSION_FAILED,
                    Some(error.to_string()),
                ),
            }
        }
        "login_exhentai" => {
            ExecOutcome::exec_error("login_exhentai() requires the async server path".to_string())
        }
        "set_cookie" => {
            let arity = match rpc.guid_arity(argc, kw_count) {
                Ok(arity) => arity,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if arity.outcome != "ok" {
                return ExecOutcome::exec_error(format!(
                    "set_cookie() {}",
                    arity_message(&arity.outcome, "'cookie'")
                ));
            }
            let Some(cookie) = args.first().and_then(wire_string) else {
                return ExecOutcome::exec_error(
                    "set_cookie() argument 'cookie' must be a string".to_string(),
                );
            };
            let (code, _) = app.set_cookie(&cookie);
            if code != 0 {
                ExecOutcome::method_error(code, None)
            } else {
                ExecOutcome::ok(Value::Null)
            }
        }
        "get_info" => {
            let arity = match rpc.noarg_arity(argc, kw_count) {
                Ok(arity) => arity,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if arity.outcome != "ok" {
                return ExecOutcome::exec_error(format!(
                    "get_info() {}",
                    arity_message(&arity.outcome, "")
                ));
            }
            ExecOutcome::ok(info.unwrap_or_else(crate::core::runtime::AppHandle::fallback_info))
        }
        "get_config" => {
            let arity = match rpc.noarg_arity(argc, kw_count) {
                Ok(arity) => arity,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if arity.outcome != "ok" {
                return ExecOutcome::exec_error(format!(
                    "get_config() {}",
                    arity_message(&arity.outcome, "")
                ));
            }
            public_config(app, rpc)
        }
        "list_tasks" => {
            let has_pos = !args.is_empty();
            let has_kw = kwargs.contains_key("level");
            if has_kw && kwargs.len() > 1 {
                return ExecOutcome::exec_error(
                    "list_tasks() got an unexpected keyword argument".to_string(),
                );
            }
            let level_value = if has_pos {
                args.first().cloned()
            } else if has_kw {
                kwargs.get("level").cloned()
            } else {
                None
            };
            // Non-string levels: the reference raises AttributeError on
            // `startswith` outside the ValueError/TypeError catch, so the
            // connection drops. The candidate answers 500 instead.
            if level_value.as_ref().is_some_and(|value| !value.is_string()) {
                return ExecOutcome::exec_error(
                    "list_tasks() argument 'level' must be a string".to_string(),
                );
            }
            // A present null level behaves like the reference `None.startswith`
            // crash; report it as an exec error rather than dropping the wire.
            if level_value.as_ref().is_some_and(Value::is_null) {
                return ExecOutcome::exec_error(
                    "list_tasks() argument 'level' must be a string".to_string(),
                );
            }
            let level_text = level_value
                .as_ref()
                .and_then(|value| value.as_str().map(str::to_string));
            let selected =
                match rpc.level_select(has_pos, has_kw, level_text.as_deref().unwrap_or("")) {
                    Ok(selected) => selected,
                    Err(error) => return ExecOutcome::exec_error(error.to_string()),
                };
            if selected.outcome == "duplicate" {
                return ExecOutcome::exec_error(
                    "list_tasks() got multiple values for argument 'level'".to_string(),
                );
            }
            if selected.outcome == "undef" {
                return ExecOutcome::method_error(consts::ERR_TASK_LEVEL_UNDEF, None);
            }
            list_tasks_view(app, rpc, &selected.canonical)
        }
        "get_image" => {
            // getImage takes (guid, request_range=None): kwargs other than
            // request_range are unexpected.
            if argc < 1 {
                return ExecOutcome::exec_error(
                    "get_image() missing required positional argument: 'guid'".to_string(),
                );
            }
            if argc > 2 {
                return ExecOutcome::exec_error(
                    "get_image() takes at most 2 arguments".to_string(),
                );
            }
            if kwargs.keys().any(|name| name != "request_range") {
                return ExecOutcome::exec_error(
                    "get_image() got an unexpected keyword argument".to_string(),
                );
            }
            if argc == 2 && kwargs.contains_key("request_range") {
                return ExecOutcome::exec_error(
                    "get_image() got multiple values for argument 'request_range'".to_string(),
                );
            }
            let Some(guid) = args.first().and_then(wire_string) else {
                return ExecOutcome::exec_error(
                    "get_image() argument 'guid' must be a string".to_string(),
                );
            };
            if app.task(&guid).is_none() {
                return ExecOutcome::method_error(consts::ERR_TASK_NOT_FOUND, None);
            }
            let range_value = if argc == 2 {
                args.get(1).cloned().unwrap_or(Value::Null)
            } else {
                kwargs.get("request_range").cloned().unwrap_or(Value::Null)
            };
            let truthy = range_is_truthy(&range_value);
            let text = python_range_text(&range_value);
            if truthy && text.is_none() {
                return ExecOutcome::exec_error(
                    "get_image() argument 'request_range' has an unsupported type".to_string(),
                );
            }
            let range = match rpc.image_range(truthy, text.as_deref().unwrap_or("")) {
                Ok(range) => range,
                Err(error) => return ExecOutcome::exec_error(error.to_string()),
            };
            if range.outcome == "bad" {
                return ExecOutcome::exec_error("get_image() invalid request_range".to_string());
            }
            let task = app.task(&guid).expect("checked");
            let total = match task.meta.get("total").and_then(|v| v.as_i64()) {
                Some(total) => total,
                None => {
                    // The reference raises KeyError on `meta['total']` and
                    // drops the connection; the candidate answers 500.
                    return ExecOutcome::exec_error(
                        "get_image() task has no total yet".to_string(),
                    );
                }
            };
            let (start, end) = if range.outcome == "default" {
                (1, total + 1)
            } else {
                (range.start, range.end)
            };
            let mut urls = Vec::new();
            for fid in start..end {
                let name = match image_name(task, fid) {
                    Ok(name) => name,
                    Err(error) => return ExecOutcome::exec_error(error),
                };
                let uri = format!("{guid}/{fid}");
                let sign = hash_link(secret, &uri);
                urls.push(Value::String(format!("/img/{sign}/{uri}/{name}")));
            }
            ExecOutcome::ok(Value::Array(urls))
        }
        _ => ExecOutcome::http(404, consts::ERR_RPC_METHOD_NOT_FOUND, None),
    }
}

fn arity_message(outcome: &str, what: &str) -> String {
    match outcome {
        "missing" => format!("missing required positional argument: {what}"),
        "extra" => "takes no more than the declared arguments".to_string(),
        "duplicate" => format!("got multiple values for argument {what}"),
        _ => "has invalid arguments".to_string(),
    }
}

fn image_name(task: &crate::task::Task, fid: i64) -> Result<String, String> {
    use crate::task::FidKey;
    // The reference checks `fid in renamed_map` with an int key first; later
    // renames store the key as text, so both spellings are honored.
    if let Some(name) = task.renamed_map.get(&FidKey::Int(fid)) {
        return Ok(name.clone());
    }
    if let Some(name) = task.renamed_map.get(&FidKey::Text(fid.to_string())) {
        return Ok(name.clone());
    }
    task.get_fidpad(fid, "jpg")
        .map_err(|error| error.to_string())
}

/// `get_config` / post-`update_config` public view (`rpc.py:427-429`).
fn public_config(app: &crate::core::App, rpc: &mut RpcDispatcher) -> ExecOutcome {
    let fields = match rpc.config_fields() {
        Ok(fields) => fields,
        Err(error) => return ExecOutcome::exec_error(error.to_string()),
    };
    let kept = match rpc.config_keep(&fields) {
        Ok(kept) => kept,
        Err(error) => return ExecOutcome::exec_error(error.to_string()),
    };
    let cfg = &app.cfg;
    let mut out = JsonMap::new();
    for name in kept {
        let value = match name.as_str() {
            "daemon" => Value::Bool(cfg.daemon),
            "dir" => Value::String(cfg.dir.clone()),
            "download_ori" => Value::Bool(cfg.download_ori),
            "jpn_title" => Value::Bool(cfg.jpn_title),
            "rename_ori" => Value::Bool(cfg.rename_ori),
            "proxy" => Value::Array(cfg.proxy.iter().map(|p| Value::String(p.clone())).collect()),
            "proxy_image" => Value::Bool(cfg.proxy_image),
            "proxy_image_only" => Value::Bool(cfg.proxy_image_only),
            "make_archive" => Value::Bool(cfg.make_archive),
            "download_range" => match &cfg.download_range {
                None => Value::Null,
                Some(ranges) => Value::Array(
                    ranges
                        .iter()
                        .map(|pair| Value::Array(vec![Value::from(pair[0]), Value::from(pair[1])]))
                        .collect(),
                ),
            },
            "scan_thread_cnt" => Value::from(cfg.scan_thread_cnt),
            "download_thread_cnt" => Value::from(cfg.download_thread_cnt),
            "download_timeout" => Value::from(cfg.download_timeout),
            "ignored_errors" => Value::Array(
                cfg.ignored_errors
                    .iter()
                    .map(|code| Value::from(*code))
                    .collect(),
            ),
            "log_path" => Value::String(cfg.log_path.clone()),
            "log_verbose" => Value::from(cfg.log_verbose),
            "save_tasks" => Value::Bool(cfg.save_tasks),
            "delete_task_files" => Value::Bool(cfg.delete_task_files),
            "low_speed_threshold" => Value::from(cfg.low_speed_threshold),
            "auto_update" => Value::String(cfg.auto_update.clone()),
            "update_beta_channel" => Value::Bool(cfg.update_beta_channel),
            _ => continue,
        };
        out.insert(name, value);
    }
    ExecOutcome::ok(Value::Object(out))
}

fn state_of(canonical: &str) -> i64 {
    use crate::r#const as consts;
    match canonical {
        "paused" => consts::TASK_STATE_PAUSED,
        "waiting" => consts::TASK_STATE_WAITING,
        "get_meta" => consts::TASK_STATE_GET_META,
        "scan_page" => consts::TASK_STATE_SCAN_PAGE,
        "scan_img" => consts::TASK_STATE_SCAN_IMG,
        "scan_archive" => consts::TASK_STATE_SCAN_ARCHIVE,
        "download" => consts::TASK_STATE_DOWNLOAD,
        "make_archive" => consts::TASK_STATE_MAKE_ARCHIVE,
        "finished" => consts::TASK_STATE_FINISHED,
        "failed" => consts::TASK_STATE_FAILED,
        _ => consts::TASK_STATE_DOWNLOAD,
    }
}

/// `list_tasks` public projection (`rpc.py:437-450`).
fn list_tasks_view(
    app: &crate::core::App,
    rpc: &mut RpcDispatcher,
    canonical: &str,
) -> ExecOutcome {
    let reverse = canonical.starts_with('!');
    let bare = canonical.trim_start_matches('!');
    let level = state_of(bare);
    let fields = match rpc.task_fields() {
        Ok(fields) => fields,
        Err(error) => return ExecOutcome::exec_error(error.to_string()),
    };
    let kept = match rpc.task_keep(&fields) {
        Ok(kept) => kept,
        Err(error) => return ExecOutcome::exec_error(error.to_string()),
    };
    let mut out = Vec::new();
    for guid in app.all_task_guids() {
        let Some(task) = app.task(&guid) else {
            continue;
        };
        let matches = if reverse {
            task.state != level
        } else {
            task.state == level
        };
        if !matches {
            continue;
        }
        let full = match task.to_value() {
            Ok(full) => full,
            Err(error) => return ExecOutcome::exec_error(error.to_string()),
        };
        let object = full.as_object().cloned().unwrap_or_default();
        let mut entry = JsonMap::new();
        for name in &kept {
            if let Some(value) = object.get(name) {
                entry.insert(name.clone(), value.clone());
            }
        }
        out.push(Value::Object(entry));
    }
    ExecOutcome::ok(Value::Array(out))
}

#[derive(Debug, PartialEq, Eq)]
pub enum GalleryFail {
    NotFound,
    BadRequest,
    Server(String),
}

impl GalleryFail {
    pub fn status(&self) -> u16 {
        match self {
            Self::NotFound => 404,
            Self::BadRequest => 400,
            Self::Server(_) => 500,
        }
    }
}

/// Owned gallery snapshot captured under a SHORT App borrow and released
/// before any disk read, ZIP build, decode or network work.
///
/// The snapshot carries the frozen read grant — the task's fixed download root
/// provenance, the validated gallery component, the mandatory pre-permit
/// directory observations and the task's private-path policy — plus the
/// validated mapped leaf name. No App borrow and no task handle is kept.
#[derive(Clone, Debug)]
pub struct GallerySnap {
    /// The gallery directory spelling, kept for logging and test markers. It is
    /// never used as a payload path authority; every open goes through `grant`.
    pub root: PathBuf,
    pub name: String,
    pub grant: crate::task::GalleryGrant,
}

/// What opening a snapshot found: an owned regular file, a genuinely missing
/// file (the reference ZIP fallback may apply), or a denied target (no
/// fallback; the denial stands).
pub enum OpenOutcome {
    File(GalleryFile),
    Missing,
    Denied,
}

/// One validated regular file plus its MIME, owned with no App borrow held.
pub struct GalleryFile {
    pub file: std::fs::File,
    pub len: u64,
    pub mime: String,
}

/// Resolve one gallery file name without touching disk beyond the task
/// record: the fid comes from the integer segment, the file name from the
/// owned task record, confined to the task directory.
pub fn snapshot_gallery_file(
    app: &crate::core::App,
    guid: &str,
    fid_text: &str,
) -> Result<GallerySnap, GalleryFail> {
    let fid: i64 = fid_text.parse().map_err(|_| GalleryFail::BadRequest)?;
    let task = app.task(guid).ok_or(GalleryFail::NotFound)?;
    let mapped = {
        use crate::task::FidKey;
        task.renamed_map
            .get(&FidKey::Int(fid))
            .or_else(|| task.renamed_map.get(&FidKey::Text(fid.to_string())))
            .cloned()
    };
    let name = match mapped {
        Some(name) => name,
        None => task
            .get_fidpad(fid, "jpg")
            .map_err(|_| GalleryFail::NotFound)?,
    };
    refuse_unsafe_name(&name)?;
    // The grant is the snapshot boundary: the task root provenance, the
    // validated gallery component, both directory observations and the
    // private-path policy are frozen here, under the same short borrow, before
    // any permit, read or network work.
    let grant = task.read_grant().map_err(|_| GalleryFail::NotFound)?;
    let root = grant.gallery_path();
    Ok(GallerySnap { root, name, grant })
}

/// Validate the final target against the frozen read grant and open it.
///
/// The open is anchored: the trusted task root is captured as a directory FD,
/// the validated gallery component is captured NOFOLLOW beneath it and both are
/// compared against the snapshot observations, so a root/parent swap between
/// the snapshot and the open fails closed. The leaf then resolves through the
/// accepted mature resolver, and the OPENED descriptor is checked for type and
/// for the current private identity before any byte is read. Outside literals,
/// magic links, pipes, FIFOs, devices, sockets and directories are denied
/// without a payload read. A genuinely missing file (or gallery) reports
/// `Missing` so the reference ZIP fallback still applies; every confinement or
/// policy failure reports `Denied` with no fallback.
pub fn probe_gallery_file(snap: &GallerySnap) -> Result<OpenOutcome, GalleryFail> {
    let opened = match snap.grant.open_leaf(std::ffi::OsStr::new(&snap.name)) {
        Ok(None) => return Ok(OpenOutcome::Missing),
        Ok(Some(file)) => file,
        Err(_) => return Ok(OpenOutcome::Denied),
    };
    // The grant already validated the descriptor (regular file, inside the
    // captured gallery, off the current private policy); the length is taken
    // from the same handle.
    let len = opened.metadata().map_err(|_| GalleryFail::NotFound)?.len();
    let mime = Path::new(&snap.name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_string();
    Ok(OpenOutcome::File(GalleryFile {
        len,
        file: opened,
        mime: image_mime(&mime).to_string(),
    }))
}

/// Resolve one gallery file for `/img` without touching URL path text as a
/// filesystem authority: the fid comes from the integer segment, the file name
/// from the owned task record, and the final target is confined to the captured
/// gallery descriptor with the current private persistence denied. Directory
/// traversal, symlink escape and private persistence access are refused; the
/// reference ZIP fallback is preserved for genuinely missing files.
pub fn read_gallery_file(
    app: &crate::core::App,
    guid: &str,
    fid_text: &str,
) -> Result<(Vec<u8>, String), GalleryFail> {
    let snap = snapshot_gallery_file(app, guid, fid_text)?;
    match probe_gallery_file(&snap).map_err(|_| GalleryFail::NotFound)? {
        OpenOutcome::Denied => Err(GalleryFail::NotFound),
        OpenOutcome::Missing => read_zip_fallback(&snap),
        OpenOutcome::File(mut opened) => {
            let mut bytes = Vec::new();
            use std::io::Read;
            opened
                .file
                .read_to_end(&mut bytes)
                .map_err(|_| GalleryFail::NotFound)?;
            Ok((bytes, opened.mime))
        }
    }
}

pub(crate) fn refuse_unsafe_name(name: &str) -> Result<(), GalleryFail> {
    if name.is_empty() || name == "." || name == ".." {
        return Err(GalleryFail::NotFound);
    }
    for component in Path::new(name).components() {
        match component {
            Component::Normal(_) => {}
            _ => return Err(GalleryFail::NotFound),
        }
    }
    if name.contains('\\') {
        return Err(GalleryFail::NotFound);
    }
    Ok(())
}

/// Sibling-archive fallback for a genuinely missing loose file
/// (`rpc.py:212-226`): the sibling path needs the exact owned archive
/// authority, and the entry faces the dedicated ZIP-image budget. The
/// sibling opens through the same anchored confined open as gallery files
/// (rooted at the gallery parent, bound to its snapshot identity), so a
/// swapped sibling or parent between check and open cannot smuggle bytes
/// in. Shared by the buffered probe path and the streaming server path.
pub fn read_zip_fallback(snap: &GallerySnap) -> Result<(Vec<u8>, String), GalleryFail> {
    // The task-minted exact sibling, opened from the separately authorized
    // gallery parent whose observation was frozen with the snapshot. Nothing
    // here re-observes or re-resolves the parent after a permit wait.
    let file = match snap.grant.open_sibling_archive() {
        Ok(Some(file)) => file,
        Ok(None) | Err(_) => return Err(GalleryFail::NotFound),
    };
    read_zip_entry_bounded(file, &snap.name)
}

/// Bounded ZIP-image entry read for the gallery fallback: the declared
/// uncompressed size and the actual decoded bytes each face the dedicated
/// 512 MiB budget before any unbounded allocation, so oversize or zip-bomb
/// entries fail instead of exhausting memory. The entry is validated (CRC)
/// before any response headers exist, so no 200 ever carries silently
/// incomplete or corrupt content. The archive handle arrives already
/// confined; only the entry budget is enforced here.
fn read_zip_entry_bounded(
    file: std::fs::File,
    name: &str,
) -> Result<(Vec<u8>, String), GalleryFail> {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(file).map_err(|_| GalleryFail::NotFound)?;
    let entry = archive.by_name(name).map_err(|_| GalleryFail::NotFound)?;
    if entry.size() > MAX_ZIP_IMAGE_ENTRY {
        return Err(GalleryFail::NotFound);
    }
    let mut limited = entry.take(MAX_ZIP_IMAGE_ENTRY + 1);
    let mut bytes = Vec::new();
    limited
        .read_to_end(&mut bytes)
        .map_err(|_| GalleryFail::NotFound)?;
    if bytes.len() as u64 > MAX_ZIP_IMAGE_ENTRY {
        return Err(GalleryFail::NotFound);
    }
    let ext = Path::new(name)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("");
    Ok((bytes, image_mime(ext).to_string()))
}

/// Build (on demand, like the reference) and read one task archive for
/// `/zip`. `remove=false` keeps the gallery, exactly like `_get_archive_path`.
/// The committed sibling opens through the anchored confined open, so a
/// swapped sibling or parent cannot smuggle bytes into the read.
pub fn read_archive(app: &crate::core::App, guid: &str) -> Result<Vec<u8>, GalleryFail> {
    use crate::task::TaskError;
    let task = app.task(guid).ok_or(GalleryFail::NotFound)?;
    // One snapshot for both the build and the final read: the writer and the
    // reader share the same root provenance, frozen observations and private
    // policy, so a name that becomes private between build and read is still
    // denied at the returned descriptor.
    let snapshot = task.archive_snapshot().map_err(|_| GalleryFail::NotFound)?;
    crate::task::make_archive_snapshot(&snapshot, false).map_err(|error| match &error {
        TaskError::UnsafePath { .. } => GalleryFail::NotFound,
        _ => GalleryFail::Server(error.to_string()),
    })?;
    let mut file = match snapshot.grant.open_sibling_archive() {
        Ok(Some(file)) => file,
        Ok(None) | Err(_) => return Err(GalleryFail::NotFound),
    };
    let mut bytes = Vec::new();
    use std::io::Read;
    file.read_to_end(&mut bytes)
        .map_err(|_| GalleryFail::NotFound)?;
    Ok(bytes)
}

#[derive(Debug, PartialEq, Eq)]
pub enum ThumbError {
    /// No `w`/`h` given: serve the original bytes (`rpc.py:92-93`).
    Passthrough,
    BadRequest(String),
    Decode(String),
}

/// Native thumbnail (`rpc.py:85-105` PIL branch): fit inside the `w`/`h` box
/// preserving aspect ratio, never upscale, encode in the source format.
/// A single given axis forms a SQUARE box from that axis
/// (`size=(w or h, h or w)`); the box then fits the source preserving
/// aspect with PIL's floor/ceil aspect-best rounding (exact integer math,
/// ties go to floor), never upscaling. Decoding, resizing and encoding use
/// the `image` crate's own APIs; the reader receives the production
/// allocation limit as a plain `image::Limits` setting. Output stays bounded
/// by the never-upscale rule. Pixel content equivalence with the reference
/// resampler is documented as mean-channel proximity, not byte identity.
pub fn thumbnail(
    data: &[u8],
    width: Option<u32>,
    height: Option<u32>,
) -> Result<(Vec<u8>, String), ThumbError> {
    thumbnail_with_limits(data, width, height, MAX_THUMB_DECODED)
}

/// Standard `image::Limits` allocation setting handed to the reader; the
/// library owns how each decoder applies it.
fn thumb_limits(max_alloc: u64) -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(max_alloc);
    limits
}

/// [`thumbnail`] with the `image` allocation limit named at the caller.
/// Every production caller passes [`MAX_THUMB_DECODED`].
pub(crate) fn thumbnail_with_limits(
    data: &[u8],
    width: Option<u32>,
    height: Option<u32>,
    max_alloc: u64,
) -> Result<(Vec<u8>, String), ThumbError> {
    match (width, height) {
        (None, None) => return Err(ThumbError::Passthrough),
        (Some(0), _) | (_, Some(0)) => {
            return Err(ThumbError::BadRequest(
                "thumbnail size must be positive".into(),
            ));
        }
        _ => {}
    }
    if data.len() > MAX_THUMB_ENCODED {
        return Err(ThumbError::Decode(format!(
            "thumbnail source {} bytes exceeds the {MAX_THUMB_ENCODED}-byte encoded bound",
            data.len()
        )));
    }
    let format =
        image::guess_format(data).map_err(|_| ThumbError::Decode("unknown image format".into()))?;
    let mime = match format {
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Gif => "image/gif",
        image::ImageFormat::Bmp => "image/bmp",
        image::ImageFormat::WebP => "image/webp",
        _ => return Err(ThumbError::Decode("unsupported image format".into())),
    };
    // `guess_format` pinned the format above; decode is the library's own
    // path, with the standard reader limits applied.
    let mut reader = image::ImageReader::with_format(std::io::Cursor::new(data), format);
    reader.limits(thumb_limits(max_alloc));
    let image = reader
        .decode()
        .map_err(|error| ThumbError::Decode(error.to_string()))?;
    let (src_w, src_h) = (image.width(), image.height());
    // Single-axis bounds form a square box from the given axis, exactly
    // like the reference `size=(w or h, h or w)`.
    let (box_w, box_h) = match (width, height) {
        (Some(w), Some(h)) => (w, h),
        (Some(w), None) => (w, w),
        (None, Some(h)) => (h, h),
        (None, None) => unreachable!(),
    };
    // PIL `thumbnail` never enlarges: a box covering the source in both
    // axes serves the original bytes.
    if box_w >= src_w && box_h >= src_h {
        return Ok((data.to_vec(), mime.to_string()));
    }
    let (out_w, out_h) = thumbnail_box_fit(src_w, src_h, box_w, box_h);
    let resized = image.resize_exact(out_w, out_h, image::imageops::FilterType::Lanczos3);
    let mut out = Vec::new();
    {
        let mut cursor = std::io::Cursor::new(&mut out);
        resized
            .write_to(&mut cursor, format)
            .map_err(|error| ThumbError::Decode(error.to_string()))?;
    }
    Ok((out, mime.to_string()))
}

/// PIL `thumbnail` box fit with exact integer math. `src_w`/`src_h` are the
/// decoded source dimensions (both nonzero, product within 64 MP by the
/// time this runs); `box_w`/`box_h` are the normalized box (both nonzero,
/// and not both covering the source). Returns the fitted dimensions:
/// the clamped axis keeps the box value, the other is the floor/ceil of
/// the exact ratio that preserves the aspect best (ties go to floor,
/// matching PIL's `min(floor, ceil, key=...)`), at least 1.
fn thumbnail_box_fit(src_w: u32, src_h: u32, box_w: u32, box_h: u32) -> (u32, u32) {
    debug_assert!(src_w > 0 && src_h > 0 && box_w > 0 && box_h > 0);
    let (sw, sh, bw, bh) = (src_w as u128, src_h as u128, box_w as u128, box_h as u128);
    // `box_w / box_h >= src_w / src_h`: the box is relatively wider, so the
    // height binds and the width fits; otherwise the width binds and the
    // height fits.
    if bw * sh >= bh * sw {
        // Fit width: exact `box_h * src_w / src_h`, ties to floor.
        let num = bh * sw;
        let floor = (num / sh) as u32;
        let ceil = num.div_ceil(sh) as u32;
        (pick_width_fit(sw, sh, bh, floor, ceil), box_h)
    } else {
        // Fit height: exact `box_w * src_h / src_w`, ties to floor.
        let num = bw * sh;
        let floor = (num / sw) as u32;
        let ceil = num.div_ceil(sw) as u32;
        (box_w, pick_height_fit(sw, sh, bw, floor, ceil))
    }
}

/// Width-fit choice: the height is clamped at `hc`, candidates fit the
/// width. PIL picks `min(floor, ceil, key=|aspect - n/hc|)`, ties to floor.
/// Compared as `|src_w*hc - n*src_h|` (common denominator), at least 1.
fn pick_width_fit(sw: u128, sh: u128, hc: u128, floor: u32, ceil: u32) -> u32 {
    if floor == ceil {
        return floor.max(1);
    }
    let (floor, ceil) = (floor as u128, ceil as u128);
    let target = sw * hc;
    if target.abs_diff(ceil * sh) < target.abs_diff(floor * sh) {
        ceil as u32
    } else {
        floor as u32
    }
    .max(1)
}

/// Height-fit choice: the width is clamped at `wc`, candidates fit the
/// height. PIL picks `min(floor, ceil, key=|aspect - wc/n|)`, ties to
/// floor. Compared exactly by cross-multiplication
/// (`|src_w*n - wc*src_h| / (src_h*n)`), at least 1.
fn pick_height_fit(sw: u128, sh: u128, wc: u128, floor: u32, ceil: u32) -> u32 {
    if floor == ceil {
        return floor.max(1);
    }
    let (floor, ceil) = (floor as u128, ceil as u128);
    if (sw * ceil).abs_diff(wc * sh) * floor < (sw * floor).abs_diff(wc * sh) * ceil {
        ceil as u32
    } else {
        floor as u32
    }
    .max(1)
}

/// Parse `w`/`h` thumbnail query args with the reference strictness
/// (`rpc.py:200`): entries without `=` or with extra `=` fail instead of
/// being silently repaired.
pub fn parse_thumb_query(query: &str) -> Result<(Option<u32>, Option<u32>), GalleryFail> {
    let mut width = None;
    let mut height = None;
    if query.is_empty() {
        return Ok((None, None));
    }
    for entry in query.split('&') {
        if entry.is_empty() {
            continue;
        }
        let mut parts = entry.split('=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().ok_or(GalleryFail::BadRequest)?;
        if parts.next().is_some() {
            return Err(GalleryFail::BadRequest);
        }
        let parsed: i64 = value.parse().map_err(|_| GalleryFail::BadRequest)?;
        match key {
            "w" => width = Some(parsed),
            "h" => height = Some(parsed),
            _ => {}
        }
    }
    let positive = |value: Option<i64>| match value {
        None => Ok(None),
        Some(n) if n <= 0 => Err(GalleryFail::BadRequest),
        Some(n) => Ok(Some(n as u32)),
    };
    Ok((positive(width)?, positive(height)?))
}

// --- static cache ----------------------------------------------------------

/// One cached static asset: body, MIME, fetch time and origin timestamp.
#[derive(Clone, Debug)]
pub struct StaticEntry {
    pub body: Vec<u8>,
    pub mime: String,
    pub fetched: f64,
    pub last_modified: String,
}

pub struct StaticCache {
    entries: indexmap::IndexMap<String, StaticEntry>,
    total_bytes: u64,
    clock: Arc<dyn crate::worker::HttpClock>,
    path: PathBuf,
}

impl StaticCache {
    pub fn load(path: &Path, clock: Arc<dyn crate::worker::HttpClock>) -> Self {
        let mut cache = Self {
            entries: indexmap::IndexMap::new(),
            total_bytes: 0,
            clock,
            path: path.to_path_buf(),
        };
        // Legacy `webui.gz` pickle payloads are never loaded; any unreadable
        // or version-mismatched cache rebuilds empty and is documented. The
        // persisted file itself is read through a bound: a full cache is
        // ~43 MiB of base64 on disk, anything past the persist bound is
        // refused before allocation. Each entry faces the per-asset budget
        // and the total/count caps on the way in.
        if let Ok(file) = std::fs::File::open(path) {
            let mut limited = file.take(MAX_CACHE_FILE + 1);
            let mut text = Vec::new();
            use std::io::Read;
            if limited.read_to_end(&mut text).is_err() || text.len() as u64 > MAX_CACHE_FILE {
                return cache;
            }
            let text = match String::from_utf8(text) {
                Ok(text) => text,
                Err(_) => return cache,
            };
            if let Ok(root) = serde_json::from_str::<Value>(&text)
                && root.get("v").and_then(|v| v.as_u64()) == Some(STATIC_CACHE_VERSION as u64)
                && let Some(entries) = root.get("entries").and_then(|v| v.as_object())
            {
                for (name, entry) in entries {
                    let body = entry
                        .get("body_b64")
                        .and_then(|v| v.as_str())
                        .and_then(base64_body);
                    let mime = entry.get("mime").and_then(|v| v.as_str());
                    let fetched = entry.get("fetched").and_then(|v| v.as_f64());
                    let last_modified = entry.get("last_modified").and_then(|v| v.as_str());
                    if let (Some(body), Some(mime), Some(fetched), Some(last_modified)) =
                        (body, mime, fetched, last_modified)
                    {
                        cache.insert_capped(
                            name.clone(),
                            StaticEntry {
                                body,
                                mime: mime.to_string(),
                                fetched,
                                last_modified: last_modified.to_string(),
                            },
                        );
                    }
                }
            }
        }
        cache
    }

    pub fn fresh(&mut self, name: &str) -> Option<&StaticEntry> {
        let hit = self
            .entries
            .get(name)
            .filter(|entry| self.clock.now() - STATIC_TTL < entry.fetched)
            .is_some();
        if hit {
            // True LRU: a fresh hit moves to the newest position so
            // frequently served assets survive eviction.
            if let Some(entry) = self.entries.shift_remove(name) {
                self.entries.insert(name.to_string(), entry);
            }
        }
        self.entries.get(name).filter(|entry| {
            let now = self.clock.now();
            now - STATIC_TTL < entry.fetched
        })
    }

    pub fn stale(&self, name: &str) -> Option<&StaticEntry> {
        self.entries.get(name)
    }

    /// Insert with real accounting: oversize single entries are refused (the
    /// caller still serves the fetched bytes inline; they are just never
    /// cached), and oldest-first eviction keeps the total and the count
    /// within their bounds.
    fn insert_capped(&mut self, name: String, entry: StaticEntry) {
        if entry.body.len() > MAX_STATIC_ENTRY {
            return;
        }
        if let Some(old) = self.entries.shift_remove(&name) {
            self.total_bytes = self.total_bytes.saturating_sub(old.body.len() as u64);
        }
        while (self.total_bytes + entry.body.len() as u64 > MAX_STATIC_TOTAL
            || self.entries.len() >= MAX_STATIC_ENTRIES)
            && !self.entries.is_empty()
        {
            if let Some((_, old)) = self.entries.shift_remove_index(0) {
                self.total_bytes = self.total_bytes.saturating_sub(old.body.len() as u64);
            } else {
                break;
            }
        }
        self.total_bytes += entry.body.len() as u64;
        self.entries.insert(name, entry);
    }

    pub fn store(&mut self, name: &str, entry: StaticEntry, clear_others: bool) {
        if clear_others {
            // `rpc.py:281-283`: a refreshed index invalidates asset keys.
            self.entries.clear();
            self.total_bytes = 0;
        }
        self.insert_capped(name.to_string(), entry);
        let _ = self.save();
    }

    pub fn refresh_index(&mut self, name: &str) {
        // `rpc.py:289-293`: a fresh `/` re-stamps every sibling entry.
        if name == "/" {
            let now = self.clock.now();
            for entry in self.entries.values_mut() {
                entry.fetched = now;
            }
            let _ = self.save();
        }
    }

    fn save(&self) -> Result<(), String> {
        let mut entries = serde_json::Map::new();
        for (name, entry) in &self.entries {
            entries.insert(
                name.clone(),
                Value::Object({
                    let mut map = serde_json::Map::new();
                    map.insert(
                        "body_b64".to_string(),
                        Value::String(encode_body(&entry.body)),
                    );
                    map.insert("mime".to_string(), Value::String(entry.mime.clone()));
                    map.insert(
                        "fetched".to_string(),
                        Value::Number(
                            serde_json::Number::from_f64(entry.fetched)
                                .unwrap_or(serde_json::Number::from(0)),
                        ),
                    );
                    map.insert(
                        "last_modified".to_string(),
                        Value::String(entry.last_modified.clone()),
                    );
                    map
                }),
            );
        }
        let mut root = serde_json::Map::new();
        root.insert("v".to_string(), Value::from(STATIC_CACHE_VERSION));
        root.insert("entries".to_string(), Value::Object(entries));
        let text = serde_json::to_string(&Value::Object(root)).map_err(|e| e.to_string())?;
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        // Atomic write: a failed save never leaves a half cache behind.
        let tmp = self.path.with_extension("tmp");
        std::fs::write(&tmp, text).map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        Ok(())
    }
}

fn encode_body(body: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(body.len().div_ceil(3) * 4);
    for chunk in body.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 63) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 63) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 63) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 63) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_body(text: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(text.len().div_ceil(4) * 3);
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        b'=' => Some(64),
        _ => None,
    };
    for chunk in bytes.chunks(4) {
        let v: Vec<u8> = chunk.iter().map(|c| value(*c)).collect::<Option<_>>()?;
        let triple = ((v[0] as u32) << 18)
            | ((v[1] as u32) << 12)
            | (((v[2] & 63) as u32) << 6)
            | ((v[3] & 63) as u32);
        out.push((triple >> 16) as u8);
        if v[2] != 64 {
            out.push((triple >> 8) as u8);
        }
        if v[3] != 64 {
            out.push(triple as u8);
        }
    }
    Some(out)
}

/// Optional local WebUI pack (`ui/` beside the executable): a materialized
/// build of the fixed upstream revision. The repository does not track it;
/// the host proxies `/ui/` and `/static/` from the fixed origin and caches the
/// answers, and this pack is only the offline fallback when the origin is
/// unreachable and no cache entry exists. The reference answers invalid-request
/// JSON in that situation; the candidate serves the frozen offline baseline
/// instead (recorded delta) and keeps the invalid-request body only when the
/// pack itself has no such asset.
#[derive(Clone, Debug)]
pub struct FrozenPack {
    files: Vec<(String, Vec<u8>, String)>,
}

impl FrozenPack {
    pub fn load(resource_root: &Path) -> Option<Self> {
        let root = resource_root.join("ui");
        let index = std::fs::read(root.join("index.html")).ok()?;
        let mut files = vec![("/".to_string(), index, "text/html".to_string())];
        let mut stack = vec![root.join("static")];
        while let Some(dir) = stack.pop() {
            let entries = std::fs::read_dir(&dir).ok()?;
            let mut paths: Vec<PathBuf> = Vec::new();
            for entry in entries {
                paths.push(entry.ok()?.path());
            }
            paths.sort();
            for path in paths {
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    let relative = path.strip_prefix(&root).ok()?;
                    let mut name = String::from("/");
                    name.push_str(&relative.to_string_lossy().replace('\\', "/"));
                    files.push((name, bytes, pack_mime(relative)));
                }
            }
        }
        files.sort_by(|left, right| left.0.cmp(&right.0));
        Some(Self { files })
    }

    pub fn get(&self, name: &str) -> Option<(&[u8], &str)> {
        // Exact asset lookup only; no traversal, no prefix serving.
        if name.is_empty() || !name.starts_with('/') {
            return None;
        }
        for component in Path::new(name).components() {
            match component {
                // The leading `/` and ordinary names are the only allowed
                // shapes; anything else (parent, current dir, prefix,
                // symlink-looking `..`) is refused.
                Component::RootDir | Component::Normal(_) => {}
                _ => return None,
            }
        }
        self.files.iter().find_map(|(key, bytes, mime)| {
            if key == name {
                Some((bytes.as_slice(), mime.as_str()))
            } else {
                None
            }
        })
    }
}

fn pack_mime(relative: &Path) -> String {
    match relative
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "html" => "text/html",
        "js" => "application/javascript",
        "css" => "text/css",
        "woff" => "font/woff",
        "ttf" => "font/ttf",
        "png" => "image/png",
        _ => "application/octet-stream",
    }
    .to_string()
}

// --- HTTP framing ------------------------------------------------------------

/// Production static origin from `rpc.py:272` (`http://xehentai.yooooo.us`).
pub const PRODUCTION_STATIC_ORIGIN: &str = "http://xehentai.yooooo.us";
/// File name of the native static cache. It intentionally differs from the
/// reference `webui.gz`: legacy pickle payloads are never loaded and the
/// safe rebuild is documented in `tools/cases.json`.
pub const STATIC_CACHE_NAME: &str = "webui-cache.json";
pub const STATIC_CACHE_VERSION: u32 = 1;
/// `const.py:43` one-hour static TTL, seconds.
pub const STATIC_TTL: f64 = 3600.0;
/// Explicit RPC bound. The reference reads `Content-Length` unconditionally;
/// the candidate refuses larger bodies instead of buffering them. This bound
/// is an RPC contract, not the config-import 1 MiB budget.
pub const MAX_JSONRPC_BODY: usize = 8 * 1024 * 1024;
pub const MAX_HEAD_BYTES: usize = 32 * 1024;
/// Dedicated thumbnail input budget: encoded sources above 64 MiB are refused
/// before parsing. Separate from the ZIP-image entry budget below and never
/// applied to raw `/zip` or ordinary passthrough file streaming.
pub const MAX_THUMB_ENCODED: usize = 64 * 1024 * 1024;
/// Standard `image::Limits` allocation setting for thumbnail decodes.
pub const MAX_THUMB_DECODED: u64 = 256 * 1024 * 1024;
/// Dedicated ZIP-image entry budget: a gallery ZIP fallback entry whose
/// declared or actual uncompressed size passes 512 MiB is refused before
/// unbounded allocation. Separate from the thumbnail buffer and never a cap
/// on raw `/zip` or ordinary file passthrough streaming.
pub const MAX_ZIP_IMAGE_ENTRY: u64 = 512 * 1024 * 1024;
/// Static remote asset budget: origins answering past 8 MiB are refused
/// before buffering and never cached.
pub const MAX_STATIC_ENTRY: usize = 8 * 1024 * 1024;
/// Static cache capacity: 32 MiB total across at most 64 entries, oldest
/// first eviction with real byte accounting.
pub const MAX_STATIC_TOTAL: u64 = 32 * 1024 * 1024;
/// Maximum retained static entries.
pub const MAX_STATIC_ENTRIES: usize = 64;
/// Bounded read for the persisted static cache file: a full 32 MiB cache is
/// ~43 MiB of base64 on disk, so 64 MiB leaves margin while refusing
/// unbounded files before allocation.
pub const MAX_CACHE_FILE: u64 = 64 * 1024 * 1024;
/// Owned streaming chunk: media files stream in 64 KiB backpressured pieces.
pub const IO_CHUNK: usize = 64 * 1024;
/// Per-I/O idle bounds: a quiet read or a stalled write fails the connection
/// after 30 s. Legit large files stream continuously, so per-operation
/// bounds never cap total transfer time.
pub const READ_IDLE_SECS: u64 = 30;
/// Per-write stall bound for response streaming.
pub const WRITE_IDLE_SECS: u64 = 30;
/// Standard active-connection bound: the accept loop defers (listener
/// backlog) past 64 live connections and reaps finished records.
pub const MAX_ACTIVE_CONN: usize = 64;
/// Heavy decode/archive worker bound: at most 2 concurrent blocking
/// decode/build jobs, queued on one owned semaphore.
pub const MAX_HEAVY_WORKERS: usize = 2;

pub const VERSION_HEADER: &str = "xeHentai/2.023-rss";

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub reason: &'static str,
    pub mime: Option<String>,
    pub body: ResponseBody,
}

/// Response payload: small control bytes inline, or an owned file streamed
/// in bounded chunks. Streaming holds one file handle plus a single 64 KiB
/// buffer per connection; no whole-asset Vec exists on that path.
pub enum ResponseBody {
    Inline(Vec<u8>),
    Stream(FileStream),
}

impl std::fmt::Debug for ResponseBody {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Inline(bytes) => formatter.debug_tuple("Inline").field(&bytes.len()).finish(),
            Self::Stream(stream) => formatter.debug_tuple("Stream").field(&stream.len).finish(),
        }
    }
}

/// One owned media file plus its exact length, streamed in [`IO_CHUNK`]
/// backpressured pieces. The connection task owns the handle; dropping it
/// (disconnect, error, shutdown) closes the file. A spool `cleanup` path is
/// unlinked on drop, so staged entry bytes never outlive their response.
/// The handle sits in an `Option` so the streaming writer can take it out of
/// a `Drop` type without moving the whole value.
pub struct FileStream {
    pub file: Option<std::fs::File>,
    pub len: u64,
    pub cleanup: Option<PathBuf>,
}

impl Drop for FileStream {
    fn drop(&mut self) {
        if let Some(path) = self.cleanup.take() {
            let _ = std::fs::remove_file(&path);
        }
    }
}

impl Response {
    pub fn content_len(&self) -> u64 {
        match &self.body {
            ResponseBody::Inline(bytes) => bytes.len() as u64,
            ResponseBody::Stream(stream) => stream.len,
        }
    }

    pub fn head_bytes(&self, cors: bool) -> Vec<u8> {
        let mut out = format!(
            "HTTP/1.0 {} {}\r\nServer: {}\r\n",
            self.status, self.reason, VERSION_HEADER
        );
        if cors {
            out.push_str("Access-Control-Allow-Origin: *\r\n");
        }
        if let Some(mime) = &self.mime {
            out.push_str(&format!("Content-Type: {mime}\r\n"));
        }
        out.push_str(&format!("Content-Length: {}\r\n", self.content_len()));
        out.push_str("Connection: close\r\n\r\n");
        out.into_bytes()
    }
}

pub fn reason_of(status: u16) -> &'static str {
    match status {
        200 => "OK",
        304 => "Not Modified",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        411 => "Length Required",
        413 => "Content Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        _ => "Error",
    }
}

pub fn cors_404() -> Response {
    // `path_filter` (`rpc.py:121-129`): CORS header only, then `\n`.
    Response {
        status: 404,
        reason: reason_of(404),
        mime: None,
        body: ResponseBody::Inline(Vec::new()),
    }
}

pub fn options_response() -> Response {
    // `do_OPTIONS` (`rpc.py:183-190`): 200 with the exact CORS sets, no
    // body; the framing helper appends the lone `\n`.
    Response {
        status: 200,
        reason: reason_of(200),
        mime: None,
        body: ResponseBody::Inline(Vec::new()),
    }
}

/// Async static fetch against the configured origin (loopback in tests, the
/// fixed trusted static origin in production). Never the public live site
/// from a test; never a login session.
pub async fn fetch_static(
    origin: &str,
    name: &str,
    cached: Option<&StaticEntry>,
) -> Result<Option<(Vec<u8>, String, String)>, String> {
    use http_body_util::BodyExt;
    let url = format!("{origin}{name}?_={}", origin_nonce());
    let parsed: url::Url = url.parse::<url::Url>().map_err(|e| e.to_string())?;
    let host = parsed.host_str().unwrap_or("127.0.0.1").to_string();
    let port = parsed.port_or_known_default().unwrap_or(80);
    let stream = tokio::net::TcpStream::connect((host.as_str(), port))
        .await
        .map_err(|e| e.to_string())?;
    let io = hyper_util::rt::TokioIo::new(stream);
    let (mut sender, connection) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|e| e.to_string())?;
    tokio::spawn(async move {
        let _ = connection.await;
    });
    let path_query = match parsed.query() {
        Some(query) => format!("{}?{query}", parsed.path()),
        None => parsed.path().to_string(),
    };
    let mut builder = hyper::Request::builder()
        .method("GET")
        .uri(path_query)
        .header("User-Agent", VERSION_HEADER)
        .header("Host", host);
    if let Some(entry) = cached {
        builder = builder.header("If-Modified-Since", &entry.last_modified);
    }
    let request = builder
        .body(http_body_util::Full::new(bytes::Bytes::new()))
        .map_err(|e| e.to_string())?;
    let response = sender
        .send_request(request)
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status().as_u16();
    if status == 304 {
        return Ok(None);
    }
    if status != 200 {
        return Err(format!("static origin answered {status}"));
    }
    let mime = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("application/octet-stream")
        .to_string();
    let last_modified = response
        .headers()
        .get("last-modified")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("")
        .to_string();
    // Bounded collection: the origin cannot make the server buffer past
    // the per-asset bound, no matter what Content-Length claims.
    let limited = http_body_util::Limited::new(response.into_body(), MAX_STATIC_ENTRY + 1);
    let body = limited
        .collect()
        .await
        .map_err(|_| format!("static asset {name} past the {MAX_STATIC_ENTRY}-byte bound"))?
        .to_bytes()
        .to_vec();
    if body.len() > MAX_STATIC_ENTRY {
        // Past the per-asset bound: refused before caching, served only
        // through the stale fallback or the offline pack.
        return Err(format!(
            "static asset {name} is {} bytes, past the {MAX_STATIC_ENTRY}-byte bound",
            body.len()
        ));
    }
    Ok(Some((body, mime, last_modified)))
}

fn origin_nonce() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// M5 minimal startup hook: run the task loop and the owned RPC server on
/// RPC-only startup owner kept for the M5 entry-point contract: it runs the
/// same supervised real pipeline as the CLI (`crate::run_supervised`), with no
/// submitted URLs, so it stays resident until the server ends or the process
/// is interrupted.
pub fn run_with_rpc(app: crate::core::App, root: &Path) -> i32 {
    // Release resources (`rss/`, `locales/`) resolve from the program
    // directory when it ships them, else from the working directory. Never
    // from a build-time absolute path, so binaries stay cwd-independent.
    let resource_root = crate::resource_root(root);
    let tokio_runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("xeHentai: cannot start runtime: {error}");
            return 1;
        }
    };
    let local = tokio::task::LocalSet::new();
    let runtime = crate::core::Runtime::new(app, resource_root.clone());
    tokio_runtime.block_on(local.run_until(crate::run_supervised(
        runtime,
        Vec::new(),
        crate::i18n::Locale::EnUs,
        resource_root,
    )))
}

/// Own both children of the M5 startup hook. Whichever future finishes first,
/// the existing shutdown watch signal is sent and BOTH children are awaited;
/// a completed `select!` branch never drops its live sibling. If the RPC side
/// ended first (normal stop or a real bind/serve error), admission is closed
/// and the task loop cancelled through the existing `AppHandle::shutdown`
/// owner path before its cleanup is joined. A completed task loop already
/// closed admission through `Runtime::cleanup`.
#[cfg(test)]
async fn supervise<F>(
    mut task_loop: tokio::task::JoinHandle<()>,
    rpc: F,
    shutdown: tokio::sync::watch::Sender<bool>,
    handle: &crate::core::runtime::AppHandle,
) -> i32
where
    F: std::future::Future<Output = Result<(), ServerError>>,
{
    tokio::pin!(rpc);
    let mut rpc_result = None;
    let mut task_finished = false;
    tokio::select! {
        result = &mut rpc => rpc_result = Some(result),
        result = &mut task_loop => {
            let _ = result;
            task_finished = true;
        }
    }
    let _ = shutdown.send(true);
    if !task_finished {
        let _ = handle.shutdown().await;
    }
    let rpc_result = match rpc_result {
        Some(result) => result,
        None => rpc.await,
    };
    if !task_finished {
        let _ = task_loop.await;
    }
    if task_finished {
        return 0;
    }
    match rpc_result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("xeHentai: RPC server error: {error}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dispatcher() -> RpcDispatcher {
        let source =
            std::fs::read_to_string(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(RPC_RSS))
                .expect("handlers.rss must exist");
        RpcDispatcher::from_source(&source).expect("handlers.rss must compile")
    }

    #[test]
    fn method_select_maps_all_thirteen_wire_names() {
        let mut rpc = dispatcher();
        for (wire, snake) in [
            ("xeH.updateConfig", "update_config"),
            ("xeH.addTask", "add_task"),
            ("xeH.delTask", "del_task"),
            ("xeH.pauseTask", "pause_task"),
            ("xeH.resumeTask", "resume_task"),
            ("xeH.saveSession", "save_session"),
            ("xeH.loadSession", "load_session"),
            ("xeH.loginExhentai", "login_exhentai"),
            ("xeH.setCookie", "set_cookie"),
            ("xeH.getInfo", "get_info"),
            ("xeH.getConfig", "get_config"),
            ("xeH.listTasks", "list_tasks"),
            ("xeH.getImage", "get_image"),
        ] {
            let selected = rpc.method_select(wire).expect("select");
            assert_eq!(
                selected,
                MethodSelect {
                    outcome: "ok".to_string(),
                    method: snake.to_string(),
                },
                "{wire}"
            );
        }
    }

    #[test]
    fn method_select_rejects_unknown_private_and_bare_names() {
        let mut rpc = dispatcher();
        let unknown = rpc.method_select("xeH.deleteAll").expect("select");
        assert_eq!(unknown.outcome, "not_found");
        let private = rpc.method_select("xeH._private").expect("select");
        assert_eq!(private.outcome, "private");
        let dunder = rpc.method_select("xeH.__getattr__").expect("select");
        assert_eq!(dunder.outcome, "private");
        let bare = rpc.method_select("getInfo").expect("select");
        assert_eq!(bare.outcome, "not_found");
        let empty = rpc.method_select("xeH.").expect("select");
        assert_eq!(empty.outcome, "not_found");
        let other = rpc.method_select("other.getInfo").expect("select");
        assert_eq!(other.outcome, "not_found");
    }

    #[test]
    fn arity_rules_match_reference_type_errors() {
        let mut rpc = dispatcher();
        assert_eq!(
            rpc.add_task_arity(0, &[]).expect("arity").outcome,
            "missing"
        );
        assert_eq!(rpc.add_task_arity(2, &[]).expect("arity").outcome, "extra");
        assert_eq!(
            rpc.add_task_arity(1, &["url".to_string()])
                .expect("arity")
                .outcome,
            "duplicate"
        );
        assert_eq!(
            rpc.add_task_arity(1, &["dir".to_string()])
                .expect("arity")
                .outcome,
            "ok"
        );
        assert_eq!(rpc.guid_arity(0, 0).expect("arity").outcome, "missing");
        assert_eq!(rpc.guid_arity(2, 0).expect("arity").outcome, "extra");
        assert_eq!(rpc.guid_arity(1, 1).expect("arity").outcome, "extra");
        assert_eq!(rpc.noarg_arity(1, 0).expect("arity").outcome, "extra");
        assert_eq!(rpc.login_arity(1, 0).expect("arity").outcome, "missing");
        assert_eq!(rpc.login_arity(2, 0).expect("arity").outcome, "ok");
        assert_eq!(rpc.login_arity(3, 0).expect("arity").outcome, "extra");
    }

    #[test]
    fn update_config_filter_drops_rpc_keys_and_detects_proxy() {
        let mut rpc = dispatcher();
        let filtered = rpc
            .update_config_filter(&[
                "dir".to_string(),
                "rpc_port".to_string(),
                "urls".to_string(),
            ])
            .expect("filter");
        assert_eq!(filtered.kept, vec!["dir".to_string()]);
        assert!(!filtered.invoke);
        let with_proxy = rpc
            .update_config_filter(&["proxy".to_string(), "dir".to_string()])
            .expect("filter");
        assert!(with_proxy.invoke);
        assert_eq!(
            with_proxy.kept,
            vec!["proxy".to_string(), "dir".to_string()]
        );
    }

    #[test]
    fn level_select_covers_default_negation_and_invalid() {
        let mut rpc = dispatcher();
        let default = rpc.level_select(false, false, "").expect("level");
        assert_eq!(default.outcome, "ok");
        assert_eq!(default.canonical, "download");
        let upper = rpc.level_select(true, false, "DOWNLOAD").expect("level");
        assert_eq!(upper.canonical, "download");
        let negated = rpc.level_select(false, true, "!download").expect("level");
        assert_eq!(negated.canonical, "!download");
        assert_eq!(
            rpc.level_select(true, true, "download")
                .expect("level")
                .outcome,
            "duplicate"
        );
        for valid in [
            "paused",
            "waiting",
            "get_meta",
            "scan_page",
            "scan_img",
            "scan_archive",
            "download",
            "make_archive",
            "finished",
            "failed",
        ] {
            let selected = rpc.level_select(true, false, valid).expect("level");
            assert_eq!(selected.outcome, "ok", "{valid}");
            assert_eq!(selected.canonical, valid, "{valid}");
        }
        assert_eq!(
            rpc.level_select(true, false, "bogus")
                .expect("level")
                .outcome,
            "undef"
        );
        assert_eq!(
            rpc.level_select(true, false, "").expect("level").outcome,
            "undef"
        );
    }

    #[test]
    fn image_range_preserves_first_item_quirk() {
        let mut rpc = dispatcher();
        let default = rpc.image_range(false, "").expect("range");
        assert_eq!(default.outcome, "default");
        let single = rpc.image_range(true, "5").expect("range");
        assert_eq!(
            (single.outcome.as_str(), single.start, single.end),
            ("single", 5, 6)
        );
        // `rpc.py:497-501`: only the first comma item is ever read.
        let quirk = rpc.image_range(true, "3,10").expect("range");
        assert_eq!(
            (quirk.outcome.as_str(), quirk.start, quirk.end),
            ("single", 3, 4)
        );
        assert_eq!(rpc.image_range(true, "abc").expect("range").outcome, "bad");
        assert_eq!(rpc.image_range(true, "").expect("range").outcome, "bad");
    }

    #[test]
    fn field_projections_hide_exact_reference_keys() {
        let mut rpc = dispatcher();
        let config_fields = rpc.config_fields().expect("fields");
        assert_eq!(config_fields.len(), 25);
        let kept = rpc.config_keep(&config_fields).expect("keep");
        assert_eq!(kept.len(), 21);
        assert!(!kept.iter().any(|name| name.starts_with("rpc_")));
        let task_fields = rpc.task_fields().expect("fields");
        let task_kept = rpc.task_keep(&task_fields).expect("keep");
        for hidden in [
            "reload_map",
            "duplicate_map",
            "renamed_map",
            "logger",
            "img_q",
            "page_q",
        ] {
            assert!(!task_kept.contains(&hidden.to_string()), "{hidden}");
        }
        for visible in ["url", "guid", "state", "config", "meta", "failcode"] {
            assert!(task_kept.contains(&visible.to_string()), "{visible}");
        }
    }

    #[test]
    fn auth_requires_exact_literal_token() {
        assert!(check_auth(None, None));
        assert!(check_auth(None, Some("anything")));
        assert!(check_auth(Some("s3cret"), Some("token:s3cret")));
        assert!(!check_auth(Some("s3cret"), None));
        assert!(!check_auth(Some("s3cret"), Some("wrong")));
        // Recorded security delta: the reference substring regex accepted a
        // token buried in surrounding text; the candidate requires equality.
        assert!(!check_auth(Some("s3cret"), Some("xx token:s3cret yy")));
        assert!(!check_auth(Some("s3cret"), Some("token:s3cret ")));
    }

    #[test]
    fn response_id_coerces_falsy_values_to_null() {
        for falsy in [
            Value::Null,
            Value::Number(0.into()),
            Value::String(String::new()),
            Value::Bool(false),
            Value::Array(vec![]),
        ] {
            assert_eq!(response_id(&falsy), Value::Null);
        }
        assert_eq!(
            response_id(&Value::Number(3.into())),
            Value::Number(3.into())
        );
        assert_eq!(
            response_id(&Value::String("a".to_string())),
            Value::String("a".to_string())
        );
    }

    #[test]
    fn image_mime_keeps_reference_typo_key() {
        assert_eq!(image_mime("jpg"), "image/jpeg");
        assert_eq!(image_mime("jepg"), "image/jpeg");
        assert_eq!(image_mime("PNG"), "image/png");
        assert_eq!(image_mime("xyz"), "application/octet-stream");
    }

    /// M5 R3-A startup owner: a real bind error keeps status 1 AND retires
    /// the task loop through its cleanup before `run_with_rpc` returns. The
    /// observable cleanup is the session file written by `Runtime::cleanup`.
    #[test]
    fn r3a_run_with_rpc_bind_error_retires_the_task_loop() {
        let root = tempfile::tempdir().expect("root");
        let occupied = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("occupy port");
        let port = occupied.local_addr().expect("addr").port();
        let base = root.path().to_path_buf();
        assert!(!base.join(crate::core::SESSION_FILE).exists());
        let (sender, receiver) = std::sync::mpsc::channel();
        let worker_base = base.clone();
        let worker = std::thread::spawn(move || {
            let config = crate::config::Config {
                rpc_interface: "127.0.0.1".to_string(),
                rpc_port: Some(port as i64),
                ..Default::default()
            };
            let logger = crate::util::logger::Logger::with_output(Box::new(Vec::new()), false);
            let app =
                crate::core::App::new(config, logger, &worker_base, &worker_base).expect("app");
            let code = crate::rpc::run_with_rpc(app, &worker_base);
            let _ = sender.send(code);
        });
        let code = receiver
            .recv_timeout(std::time::Duration::from_secs(20))
            .expect("run_with_rpc returns on the bind error");
        assert_eq!(code, 1, "the real bind-error exit status is preserved");
        worker.join().expect("startup thread");
        assert!(
            base.join(crate::core::SESSION_FILE).exists(),
            "the task loop retired through its cleanup before return"
        );
        drop(occupied);
    }

    /// The task-loop-first outcome must not drop the live server future: the
    /// shutdown signal retires the real listener and `supervise` awaits it
    /// before returning the normal status.
    #[test]
    fn r3a_supervise_retires_the_server_after_task_loop_completion() {
        use crate::rpc::server::{SharedRpc, bind, serve_until_closed};
        use std::sync::Arc;
        let root = tempfile::tempdir().expect("root");
        let cache = tempfile::tempdir().expect("cache").keep();
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let config = crate::config::Config::default();
        let logger = crate::util::logger::Logger::with_output(Box::new(Vec::new()), false);
        let app = crate::core::App::new(config, logger, root.path(), root.path()).expect("app");
        let local = tokio::task::LocalSet::new();
        let tokio_runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        tokio_runtime.block_on(local.run_until(async move {
            let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
            let (task_loop, handle) = runtime.spawn();
            let listener = bind("127.0.0.1", 0).await.expect("bind");
            let shared = std::rc::Rc::new(std::cell::RefCell::new(
                SharedRpc::new(
                    manifest,
                    &cache,
                    Arc::new(crate::worker::RealClock),
                    None,
                    "http://127.0.0.1:9".to_string(),
                )
                .expect("shared"),
            ));
            let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
            let done = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let serve_done = Arc::clone(&done);
            let server_handle = handle.clone();
            let serve = async move {
                let result = serve_until_closed(
                    listener,
                    shutdown_rx,
                    server_handle,
                    shared,
                    "http://127.0.0.1:9".to_string(),
                )
                .await;
                serve_done.store(true, std::sync::atomic::Ordering::SeqCst);
                result
            };
            // The task loop exits through the ordinary soft-stop condition
            // before the server; the supervisor must still retire the server.
            handle.borrow_app_mut(|app| app.set_exit(crate::r#const::XEH_STATE_SOFT_EXIT));
            let code = supervise(task_loop, serve, shutdown_tx, &handle).await;
            assert_eq!(code, 0);
            assert!(
                done.load(std::sync::atomic::Ordering::SeqCst),
                "the live server future was awaited, not dropped"
            );
            assert_eq!(
                handle.borrow_app(|app| app.exit_state()),
                crate::r#const::XEH_STATE_CLEAN
            );
        }));
    }
}
