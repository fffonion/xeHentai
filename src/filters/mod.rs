//! RSS page filters (`filters.py:15-268`) and their typed host boundary.
//!
//! The business decisions live in `rss/filters/match.rss`. This module only
//! compiles that module once per worker/session instance, binds the narrow
//! regex/JSON host surface it needs, converts the named request DTO into a
//! script value, and converts the named result back. It never parses a page
//! or decides a filter outcome itself.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use vm::{
    CallOutcome, CompileSourceFileOptions, HostApiBuilder, HostApiCatalog, HostFunctionRegistry,
    HostFunctionSchema, HostParamSchema, HostTypeSchema, SourceFlavor, Value, Vm, VmError,
    VmStatus, compile_source_with_flavor_and_options, register_catalog_static_function, return_one,
};

pub const FILTER_RSS: &str = "rss/filters/match.rss";

#[derive(Debug)]
pub enum FilterError {
    Io { path: PathBuf, message: String },
    Compile(String),
    Bind(String),
    Runtime(String),
    Shape(String),
}

impl std::fmt::Display for FilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Compile(message) => write!(f, "filter script compile error: {message}"),
            Self::Bind(message) => write!(f, "filter host binding error: {message}"),
            Self::Runtime(message) => write!(f, "filter script runtime error: {message}"),
            Self::Shape(message) => write!(f, "filter result shape error: {message}"),
        }
    }
}

impl std::error::Error for FilterError {}

fn array_of(schema: HostTypeSchema) -> HostTypeSchema {
    HostTypeSchema::Array(Box::new(schema))
}

/// Catalog of the extra pure primitives this module adds.
///
/// The SDK already provides `re::match`, `re::find`, `re::replace`,
/// `re::split`, `re::captures` and `json::decode`. It has no `findall`
/// (multiple matches with capture groups), no Python `re.match` prefix
/// predicate, and no header view preserving duplicate values; those three are
/// the only gap and are declared here as typed host functions. `html_escape`
/// reuses the already accepted `util::htmlescape` implementation so the
/// script does not carry a second entity table.
fn filter_catalog() -> Arc<HostApiCatalog> {
    static CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();
    Arc::clone(CATALOG.get_or_init(|| {
        let string = || HostTypeSchema::String;
        let mut builder = HostApiBuilder::new();
        builder.function(HostFunctionSchema::with_return(
            "flt::find_all",
            vec![
                HostParamSchema::value("pattern", string()),
                HostParamSchema::value("text", string()),
            ],
            array_of(array_of(string())),
        ));
        builder.function(HostFunctionSchema::with_return(
            "flt::match_prefix",
            vec![
                HostParamSchema::value("pattern", string()),
                HostParamSchema::value("text", string()),
            ],
            HostTypeSchema::Bool,
        ));
        builder.function(HostFunctionSchema::with_return(
            "flt::html_escape",
            vec![HostParamSchema::value("text", string())],
            string(),
        ));
        builder.function(HostFunctionSchema::with_return(
            "flt::header_get",
            vec![
                HostParamSchema::value("headers", array_of(array_of(string()))),
                HostParamSchema::value("name", string()),
            ],
            HostTypeSchema::Optional(Box::new(string())),
        ));
        builder.function(HostFunctionSchema::with_return(
            "flt::json_parse",
            vec![HostParamSchema::value("text", string())],
            HostTypeSchema::Optional(Box::new(array_of(HostTypeSchema::Map(Box::new(string()))))),
        ));
        Arc::new(builder.build().expect("filter catalog must build"))
    }))
}

fn string_arg(args: &[Value], index: usize) -> Result<String, VmError> {
    match args.get(index) {
        Some(Value::String(value)) => Ok((**value).clone()),
        _ => Err(VmError::TypeMismatch("string")),
    }
}

fn find_all_host(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
    let pattern = string_arg(args, 0)?;
    let text = string_arg(args, 1)?;
    let regex = regex::Regex::new(&pattern)
        .map_err(|error| VmError::HostError(format!("flt::find_all invalid pattern: {error}")))?;
    let matches = regex
        .captures_iter(&text)
        .map(|captures| {
            Value::array(
                (0..captures.len())
                    .map(|index| {
                        Value::string(
                            captures
                                .get(index)
                                .map(|group| group.as_str())
                                .unwrap_or_default(),
                        )
                    })
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    Ok(CallOutcome::Return(return_one(Value::array(matches))))
}

/// Python `re.match` semantics: a match must begin at byte (character) zero.
fn match_prefix_host(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
    let pattern = string_arg(args, 0)?;
    let text = string_arg(args, 1)?;
    let regex = regex::Regex::new(&pattern).map_err(|error| {
        VmError::HostError(format!("flt::match_prefix invalid pattern: {error}"))
    })?;
    let matched = regex
        .captures(&text)
        .is_some_and(|captures| captures.get(0).is_some_and(|whole| whole.start() == 0));
    Ok(CallOutcome::Return(return_one(Value::Bool(matched))))
}

fn html_escape_host(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
    let text = string_arg(args, 0)?;
    let escaped = crate::util::htmlescape(&text)
        .map_err(|error| VmError::HostError(format!("flt::html_escape: {error}")))?;
    Ok(CallOutcome::Return(return_one(Value::string(escaped))))
}

fn json_parse_host(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
    let text = string_arg(args, 0)?;
    let parsed: serde_json::Value = match serde_json::from_str(&text) {
        Ok(value) => value,
        Err(_) => return Ok(CallOutcome::Return(return_one(Value::Null))),
    };
    let serde_json::Value::Array(items) = parsed else {
        return Ok(CallOutcome::Return(return_one(Value::Null)));
    };
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let serde_json::Value::Object(fields) = item else {
            return Ok(CallOutcome::Return(return_one(Value::Null)));
        };
        let mut row = Vec::with_capacity(fields.len());
        for (key, value) in fields {
            let value = match value {
                serde_json::Value::String(value) => value,
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            row.push((Value::string(key), Value::string(value)));
        }
        rows.push(Value::map(row));
    }
    Ok(CallOutcome::Return(return_one(Value::array(rows))))
}

fn header_get_host(_vm: &mut Vm, args: &[Value]) -> Result<CallOutcome, VmError> {
    let Some(Value::Array(headers)) = args.first() else {
        return Err(VmError::TypeMismatch("[[string]]"));
    };
    let name = string_arg(args, 1)?;
    let mut joined: Option<String> = None;
    for header in headers.iter() {
        let Value::Array(pair) = header else {
            return Err(VmError::TypeMismatch("[[string]]"));
        };
        if pair.len() != 2 {
            return Err(VmError::TypeMismatch("[[string]]"));
        }
        let (Value::String(header_name), Value::String(header_value)) = (&pair[0], &pair[1]) else {
            return Err(VmError::TypeMismatch("[[string]]"));
        };
        if header_name.eq_ignore_ascii_case(&name) {
            match &mut joined {
                Some(current) => {
                    current.push_str(", ");
                    current.push_str(header_value);
                }
                None => joined = Some((**header_value).clone()),
            }
        }
    }
    Ok(CallOutcome::Return(return_one(match joined {
        Some(value) => Value::string(value),
        None => Value::Null,
    })))
}

fn filter_registry() -> &'static HostFunctionRegistry {
    static REGISTRY: OnceLock<HostFunctionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = HostFunctionRegistry::restricted();
        let catalog = filter_catalog();
        for (name, function) in [
            ("flt::find_all", find_all_host as vm::StaticHostFunction),
            ("flt::match_prefix", match_prefix_host),
            ("flt::html_escape", html_escape_host),
            ("flt::header_get", header_get_host),
            ("flt::json_parse", json_parse_host),
        ] {
            let schema = catalog
                .function(name)
                .unwrap_or_else(|| panic!("missing {name} schema"))
                .clone();
            register_catalog_static_function(&mut registry, &catalog, name, schema, function)
                .unwrap_or_else(|error| panic!("register {name}: {error}"));
        }
        let mut profile = vm::CapabilityProfile::builder();
        for name in [
            "re::match",
            "re::find",
            "re::replace",
            "re::split",
            "re::captures",
            "json::decode",
            "json::encode",
        ] {
            let builtin = vm::BuiltinFunction::from_namespaced_name(name)
                .unwrap_or_else(|| panic!("unknown filter builtin {name}"));
            profile = profile.allow_builtin(builtin);
        }
        for name in [
            "flt::find_all",
            "flt::match_prefix",
            "flt::html_escape",
            "flt::header_get",
            "flt::json_parse",
        ] {
            profile = profile.allow_host_import(name);
        }
        registry.set_capability_profile(profile.build());
        registry
    })
}

/// Values passed from the transport layer; they mirror the `requests`
/// response attributes the reference filters read, not the wire body.
#[derive(Clone, Debug)]
pub struct PageResponse {
    pub status: i64,
    pub url: String,
    pub real_url: String,
    pub content_length: i64,
    pub text: String,
    pub headers: Vec<(String, String)>,
    pub ori: bool,
    pub has_stream: bool,
}

#[derive(Clone, Debug)]
pub struct DownloadInput {
    pub page: PageResponse,
    pub actual_length: Option<i64>,
    pub read_error: Option<String>,
}

// Typed views of the RSS results. The guest named structs are converted here
// once; Task/App code never parses a raw map.

#[derive(Clone, Debug, Default)]
pub struct LoginOutcome {
    pub cookies: Option<Vec<(String, String)>>,
    pub fail_message: Option<String>,
    pub exception: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Metadata {
    pub gjname: String,
    pub gnname: String,
    pub total: i64,
    pub finished: i64,
    pub tags: Vec<String>,
    pub thumbnail_cnt: i64,
    pub use_multipage_viewer: bool,
}

#[derive(Clone, Debug, Default)]
pub struct MetadataOutcome {
    pub fail_code: Option<i64>,
    pub ban: Option<String>,
    pub meta: Option<Metadata>,
    pub exception: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct PageOutcome {
    pub urls: Vec<String>,
    pub fail_code: Option<i64>,
    pub debug_text: Option<String>,
    pub exception: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ImageOutcome {
    pub image_url: Option<String>,
    pub reload_url: Option<String>,
    pub filename: Option<String>,
    pub fail_code: Option<i64>,
    pub real_url: Option<String>,
    pub url: Option<String>,
    pub exception: Option<String>,
}

impl ImageOutcome {
    pub fn is_success(&self) -> bool {
        self.image_url.is_some()
    }
}

#[derive(Clone, Debug, Default)]
pub struct DownloadOutcome {
    pub stream: bool,
    pub fail_code: Option<i64>,
    pub real_url: Option<String>,
    pub url: Option<String>,
    pub exception: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct StreamOutcome {
    pub broken: bool,
    pub code: Option<i64>,
    pub real_url: Option<String>,
    pub url: Option<String>,
}

fn page_entries(page: &PageResponse) -> Vec<(Value, Value)> {
    let headers = page
        .headers
        .iter()
        .map(|(name, value)| {
            Value::array(vec![
                Value::string(name.clone()),
                Value::string(value.clone()),
            ])
        })
        .collect();
    vec![
        (Value::string("status"), Value::Int(page.status)),
        (Value::string("url"), Value::string(page.url.clone())),
        (
            Value::string("real_url"),
            Value::string(page.real_url.clone()),
        ),
        (
            Value::string("content_length"),
            Value::Int(page.content_length),
        ),
        (Value::string("text"), Value::string(page.text.clone())),
        (Value::string("headers"), Value::array(headers)),
        (Value::string("ori"), Value::Bool(page.ori)),
        (Value::string("has_stream"), Value::Bool(page.has_stream)),
    ]
}

fn page_value(page: &PageResponse) -> Value {
    Value::map(page_entries(page))
}

fn download_value(input: &DownloadInput) -> Value {
    let mut entries = page_entries(&input.page);
    entries.push((
        Value::string("actual_length"),
        match input.actual_length {
            Some(length) => Value::Int(length),
            None => Value::Null,
        },
    ));
    entries.push((
        Value::string("read_error"),
        match &input.read_error {
            Some(error) => Value::string(error.clone()),
            None => Value::Null,
        },
    ));
    Value::map(entries)
}

pub struct Filters {
    vm: Vm,
    login: Value,
    metadata: Value,
    pageurl: Value,
    pageurl_mpv: Value,
    imgurl: Value,
    download: Value,
    stream_result: Value,
    reset_quota: Value,
}

impl Filters {
    pub fn load(resource_root: &Path) -> Result<Self, FilterError> {
        let path = resource_root.join(FILTER_RSS);
        let source = std::fs::read_to_string(&path).map_err(|error| FilterError::Io {
            path,
            message: error.to_string(),
        })?;
        Self::from_source(&source)
    }

    pub fn from_source(source: &str) -> Result<Self, FilterError> {
        let vm = compile_module(source)?;
        let resolve = |vm: &Vm, name: &str| {
            vm.resolve_exported_callable(name)
                .map_err(|error| FilterError::Runtime(format!("{name}: {error}")))
        };
        let login = resolve(&vm, "flt_login")?;
        let metadata = resolve(&vm, "flt_metadata")?;
        let pageurl = resolve(&vm, "flt_pageurl")?;
        let pageurl_mpv = resolve(&vm, "flt_pageurl_mpv")?;
        let imgurl = resolve(&vm, "flt_imgurl")?;
        let download = resolve(&vm, "flt_download")?;
        let stream_result = resolve(&vm, "flt_stream_result")?;
        let reset_quota = resolve(&vm, "flt_reset_quota")?;
        Ok(Self {
            vm,
            login,
            metadata,
            pageurl,
            pageurl_mpv,
            imgurl,
            download,
            stream_result,
            reset_quota,
        })
    }

    fn invoke(&mut self, callable: &Value, input: Value) -> Result<Value, FilterError> {
        self.invoke_args(callable, &[input])
    }

    fn invoke_args(&mut self, callable: &Value, args: &[Value]) -> Result<Value, FilterError> {
        self.vm
            .invoke_callable(callable.clone(), args)
            .map_err(|error| FilterError::Runtime(error.to_string()))
    }

    pub fn login(&mut self, page: &PageResponse) -> Result<Value, FilterError> {
        let callable = self.login.clone();
        self.invoke(&callable, page_value(page))
    }

    pub fn metadata(&mut self, page: &PageResponse) -> Result<Value, FilterError> {
        let callable = self.metadata.clone();
        self.invoke(&callable, page_value(page))
    }

    pub fn pageurl(&mut self, page: &PageResponse) -> Result<Value, FilterError> {
        let callable = self.pageurl.clone();
        self.invoke(&callable, page_value(page))
    }

    pub fn pageurl_mpv(&mut self, page: &PageResponse) -> Result<Value, FilterError> {
        let callable = self.pageurl_mpv.clone();
        self.invoke(&callable, page_value(page))
    }

    pub fn imgurl(&mut self, page: &PageResponse) -> Result<Value, FilterError> {
        let callable = self.imgurl.clone();
        self.invoke(&callable, page_value(page))
    }

    pub fn download(&mut self, page: &PageResponse) -> Result<Value, FilterError> {
        let callable = self.download.clone();
        self.invoke(&callable, page_value(page))
    }

    pub fn stream_result(
        &mut self,
        page: &PageResponse,
        actual_length: i64,
        read_error: Option<&str>,
    ) -> Result<Value, FilterError> {
        let callable = self.stream_result.clone();
        self.invoke_args(
            &callable,
            &[
                page_value(page),
                Value::Int(actual_length),
                Value::string(read_error.unwrap_or("")),
            ],
        )
    }

    pub fn reset_quota(&mut self, page: &PageResponse) -> Result<Value, FilterError> {
        let callable = self.reset_quota.clone();
        self.invoke(&callable, page_value(page))
    }

    pub fn login_outcome(&mut self, page: &PageResponse) -> Result<LoginOutcome, FilterError> {
        let value = self.login(page)?;
        decode_login(&value)
    }

    pub fn metadata_outcome(
        &mut self,
        page: &PageResponse,
    ) -> Result<MetadataOutcome, FilterError> {
        let value = self.metadata(page)?;
        decode_metadata(&value)
    }

    pub fn pageurl_outcome(&mut self, page: &PageResponse) -> Result<PageOutcome, FilterError> {
        let value = self.pageurl(page)?;
        decode_page(&value)
    }

    pub fn pageurl_mpv_outcome(&mut self, page: &PageResponse) -> Result<PageOutcome, FilterError> {
        let value = self.pageurl_mpv(page)?;
        decode_page(&value)
    }

    pub fn imgurl_outcome(&mut self, page: &PageResponse) -> Result<ImageOutcome, FilterError> {
        let value = self.imgurl(page)?;
        decode_image(&value)
    }

    pub fn download_outcome(
        &mut self,
        page: &PageResponse,
    ) -> Result<DownloadOutcome, FilterError> {
        let value = self.download(page)?;
        decode_download(&value)
    }

    pub fn stream_outcome(
        &mut self,
        page: &PageResponse,
        actual_length: i64,
        read_error: Option<&str>,
    ) -> Result<StreamOutcome, FilterError> {
        let value = self.stream_result(page, actual_length, read_error)?;
        decode_stream(&value)
    }
}

fn object<'a>(value: &'a Value, what: &str) -> Result<&'a vm::VmMap, FilterError> {
    match value {
        Value::Map(map) => Ok(map),
        other => Err(FilterError::Shape(format!(
            "{what} is not a map: {other:?}"
        ))),
    }
}

fn field<'a>(map: &'a vm::VmMap, name: &str) -> Option<&'a Value> {
    map.get(&Value::string(name))
}

fn text(map: &vm::VmMap, name: &str) -> Result<Option<String>, FilterError> {
    match field(map, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some((**value).clone())),
        Some(other) => Err(FilterError::Shape(format!(
            "field {name} is not a string: {other:?}"
        ))),
    }
}

fn int(map: &vm::VmMap, name: &str) -> Result<Option<i64>, FilterError> {
    match field(map, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Int(value)) => Ok(Some(*value)),
        Some(other) => Err(FilterError::Shape(format!(
            "field {name} is not an int: {other:?}"
        ))),
    }
}

fn strings(map: &vm::VmMap, name: &str) -> Result<Vec<String>, FilterError> {
    match field(map, name) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(value) => Ok((**value).clone()),
                other => Err(FilterError::Shape(format!(
                    "field {name} contains a non-string: {other:?}"
                ))),
            })
            .collect(),
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(other) => Err(FilterError::Shape(format!(
            "field {name} is not an array: {other:?}"
        ))),
    }
}

fn support_text(map: &vm::VmMap) -> Result<Option<String>, FilterError> {
    match text(map, "exception")? {
        Some(value) => Ok(Some(value)),
        None => text(map, "message"),
    }
}

fn decode_login(value: &Value) -> Result<LoginOutcome, FilterError> {
    let map = object(value, "login result")?;
    let outcome = text(map, "outcome")?.unwrap_or_default();
    let mut cookies = None;
    if outcome == "ok" {
        let Value::Array(rows) = field(map, "cookies").unwrap_or(&Value::Null) else {
            return Err(FilterError::Shape("login cookies are not an array".into()));
        };
        let mut pairs = Vec::with_capacity(rows.len());
        for row in rows.iter() {
            let Value::Array(pair) = row else {
                return Err(FilterError::Shape("login cookie is not a pair".into()));
            };
            let (Value::String(name), Value::String(value)) = (&pair[0], &pair[1]) else {
                return Err(FilterError::Shape(
                    "login cookie is not a string pair".into(),
                ));
            };
            pairs.push(((**name).clone(), (**value).clone()));
        }
        cookies = Some(pairs);
    }
    Ok(LoginOutcome {
        cookies,
        fail_message: if outcome == "fail" {
            text(map, "message")?
        } else {
            None
        },
        exception: if outcome == "exception" {
            support_text(map)?
        } else {
            None
        },
    })
}

fn decode_metadata(value: &Value) -> Result<MetadataOutcome, FilterError> {
    let map = object(value, "metadata result")?;
    let outcome = text(map, "outcome")?.unwrap_or_default();
    let mut meta = None;
    if outcome == "ok" {
        let meta_map = match field(map, "meta") {
            Some(Value::Map(meta_map)) => meta_map,
            _ => return Err(FilterError::Shape("metadata result has no meta".into())),
        };
        meta = Some(Metadata {
            gjname: text(meta_map, "gjname")?.unwrap_or_default(),
            gnname: text(meta_map, "gnname")?.unwrap_or_default(),
            total: int(meta_map, "total")?.unwrap_or(0),
            finished: int(meta_map, "finished")?.unwrap_or(0),
            tags: strings(meta_map, "tags")?,
            thumbnail_cnt: int(meta_map, "thumbnail_cnt")?.unwrap_or(0),
            use_multipage_viewer: matches!(
                field(meta_map, "use_multipage_viewer"),
                Some(Value::Bool(true))
            ),
        });
    }
    Ok(MetadataOutcome {
        fail_code: int(map, "fail_code")?,
        ban: text(map, "ban")?,
        meta,
        exception: text(map, "exception")?,
    })
}

fn decode_page(value: &Value) -> Result<PageOutcome, FilterError> {
    let map = object(value, "page result")?;
    Ok(PageOutcome {
        urls: strings(map, "urls")?,
        fail_code: int(map, "fail_code")?,
        debug_text: text(map, "debug_text")?,
        exception: text(map, "exception")?,
    })
}

fn decode_image(value: &Value) -> Result<ImageOutcome, FilterError> {
    let map = object(value, "image result")?;
    Ok(ImageOutcome {
        image_url: text(map, "image_url")?,
        reload_url: text(map, "reload_url")?,
        filename: text(map, "filename")?,
        fail_code: int(map, "code")?,
        real_url: text(map, "real_url")?,
        url: text(map, "url")?,
        exception: text(map, "exception")?,
    })
}

fn decode_download(value: &Value) -> Result<DownloadOutcome, FilterError> {
    let map = object(value, "download result")?;
    let outcome = text(map, "outcome")?.unwrap_or_default();
    Ok(DownloadOutcome {
        stream: outcome == "stream",
        fail_code: int(map, "code")?,
        real_url: text(map, "real_url")?,
        url: text(map, "url")?,
        exception: text(map, "exception")?,
    })
}

fn decode_stream(value: &Value) -> Result<StreamOutcome, FilterError> {
    let map = object(value, "stream result")?;
    let outcome = text(map, "outcome")?.unwrap_or_default();
    Ok(StreamOutcome {
        broken: outcome == "broken",
        code: int(map, "code")?,
        real_url: text(map, "real_url")?,
        url: text(map, "url")?,
    })
}

/// Compile and bind one filter module instance. Kept separate from [`Filters`]
/// so module tests and the compatibility probe can drive small, explicitly
/// exported scripts through the same host surface.
pub fn compile_module(source: &str) -> Result<Vm, FilterError> {
    let options = CompileSourceFileOptions::new().with_host_api_catalog(filter_catalog());
    let compiled =
        compile_source_with_flavor_and_options(source, SourceFlavor::RustScript, options)
            .map_err(|error| FilterError::Compile(error.to_string()))?;
    let mut vm = Vm::new(compiled.program);
    filter_registry()
        .bind_vm_cached(&mut vm)
        .map_err(|error| FilterError::Bind(error.to_string()))?;
    let status = vm
        .run()
        .map_err(|error| FilterError::Runtime(error.to_string()))?;
    if status != VmStatus::Halted {
        return Err(FilterError::Runtime(format!(
            "filter module ended with {status:?}"
        )));
    }
    Ok(vm)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field<'a>(value: &'a Value, name: &str) -> &'a Value {
        let Value::Map(map) = value else {
            panic!("expected map, found {value:?}");
        };
        map.get(&Value::string(name))
            .unwrap_or_else(|| panic!("missing field {name} in {map:?}"))
    }

    fn outcome(value: &Value) -> &str {
        match field(value, "outcome") {
            Value::String(text) => text,
            other => panic!("outcome is not a string: {other:?}"),
        }
    }

    fn int_field(value: &Value, name: &str) -> Option<i64> {
        match field(value, name) {
            Value::Null => None,
            Value::Int(number) => Some(*number),
            other => panic!("{name} is not an int: {other:?}"),
        }
    }

    fn text_field(value: &Value, name: &str) -> Option<String> {
        match field(value, name) {
            Value::Null => None,
            Value::String(text) => Some((**text).clone()),
            other => panic!("{name} is not a string: {other:?}"),
        }
    }

    fn page(status: i64, url: &str, real_url: &str, text: &str) -> PageResponse {
        PageResponse {
            status,
            url: url.into(),
            real_url: real_url.into(),
            content_length: text.len() as i64,
            text: text.into(),
            headers: vec![("Content-Type".into(), "text/html; charset=utf-8".into())],
            ori: false,
            has_stream: false,
        }
    }

    fn loaded() -> Filters {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        Filters::load(root).expect("match.rss must compile and bind")
    }

    const PAGE_URL: &str = "https://e-hentai.org/s/abcdef0123/123456-7";
    const REAL_URL: &str = "https://e-hentai.org/s/abcdef0123/123456-7";

    #[test]
    fn login_parses_cookie_header_and_reports_callback_events() {
        let mut filters = loaded();
        let mut response = page(
            200,
            "https://forums.e-hentai.org/index.php",
            "https://forums.e-hentai.org/index.php",
            "",
        );
        response.headers = vec![(
            "Set-Cookie".into(),
            "ipb_member_id=42; expires=Wed; ipb_pass_hash=deadbeef; path=/".into(),
        )];
        let result = filters.login(&response).expect("login call");
        assert_eq!(outcome(&result), "ok");
        let Value::Array(cookies) = field(&result, "cookies") else {
            panic!("cookies must be an array");
        };
        assert_eq!(cookies.len(), 2);

        let mut empty = response.clone();
        empty.headers = vec![("Set-Cookie".into(), "other=1; path=/".into())];
        let result = filters.login(&empty).expect("login fail call");
        assert_eq!(outcome(&result), "fail");
        assert_eq!(
            text_field(&result, "message").as_deref(),
            Some("ex: list index out of range")
        );

        let mut missing = response.clone();
        missing.headers.clear();
        let result = filters.login(&missing).expect("login missing call");
        assert_eq!(outcome(&result), "exception");
        assert_eq!(
            text_field(&result, "exception").as_deref(),
            Some("TypeError")
        );
    }

    #[test]
    fn metadata_reads_reference_fields_and_status_branches() {
        let mut filters = loaded();
        let body = "=\"gj\">日本語 &amp; title</h1>=\"gn\">English title</h1>\
            Length:</td><td class=\"gdt2\">12 pages</td>\
            toggle_tagmenu('tag one' toggle_tagmenu('tag two'\
            Showing 1 - 20 of 234 images\
            <a href=\"https://e-hentai.org/mpv/123/abcdef0123/#page2\"><div title=\"Page";
        let result = filters.metadata(&page(200, "u", "u", body)).expect("meta");
        assert_eq!(outcome(&result), "ok");
        let Value::Map(meta) = field(&result, "meta") else {
            panic!("meta must be a map");
        };
        assert_eq!(meta.get(&Value::string("total")), Some(&Value::Int(12)));
        assert_eq!(
            meta.get(&Value::string("thumbnail_cnt")),
            Some(&Value::Int(20))
        );
        assert_eq!(
            meta.get(&Value::string("use_multipage_viewer")),
            Some(&Value::Bool(true))
        );
        let Value::Array(tags) = meta.get(&Value::string("tags")).unwrap() else {
            panic!("tags");
        };
        assert_eq!(tags.len(), 2);

        let result = filters.metadata(&page(404, "u", "u", "")).expect("404");
        assert_eq!(int_field(&result, "fail_code"), Some(1004));
        let result = filters.metadata(&page(600, "u", "u", "")).expect("600");
        assert_eq!(int_field(&result, "fail_code"), Some(1009));
        let result = filters
            .metadata(&page(
                200,
                "u",
                "u",
                "Your IP address has been temporarily banned. The ban expires in 5 minutes",
            ))
            .expect("ban");
        assert_eq!(int_field(&result, "fail_code"), Some(1010));
        assert_eq!(text_field(&result, "ban").as_deref(), Some("5 minutes"));
    }

    #[test]
    fn pageurl_returns_every_reference_url_and_debug_body() {
        let mut filters = loaded();
        let body = concat!(
            "<a href=\"https://e-hentai.org/s/abcdef0123/123456-1\">",
            "<div title=\"Page 1\"",
            "<a href=\"https://exhentai.org/s/0123456789/123456-2\">",
            "<img alt=\"2\" title=\"Page 2\""
        );
        let result = filters
            .pageurl(&page(200, "u", "u", body))
            .expect("pageurl");
        assert_eq!(outcome(&result), "ok");
        let Value::Array(urls) = field(&result, "urls") else {
            panic!("urls");
        };
        assert_eq!(urls.len(), 2);
        assert_eq!(
            urls[1],
            Value::string("https://exhentai.org/s/0123456789/123456-2")
        );

        let result = filters
            .pageurl(&page(200, "u", "u", "nothing"))
            .expect("empty");
        assert_eq!(outcome(&result), "fail");
        assert_eq!(int_field(&result, "fail_code"), Some(1008));
        assert_eq!(
            text_field(&result, "debug_text").as_deref(),
            Some("nothing")
        );
    }

    #[test]
    fn mpv_parses_imagelist_keys_in_order() {
        let mut filters = loaded();
        let body = "imagelist = [{\"k\":\"aaaa\"},{\"k\":\"bbbb\"}];";
        let response = page(200, "u", "https://e-hentai.org/mpv/123/abcdef0123", body);
        let result = filters.pageurl_mpv(&response).expect("mpv");
        assert_eq!(outcome(&result), "ok");
        let Value::Array(urls) = field(&result, "urls") else {
            panic!("urls");
        };
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0], Value::string("https://e-hentai.org/s/aaaa/123-1"));
        assert_eq!(urls[1], Value::string("https://e-hentai.org/s/bbbb/123-2"));

        let result = filters
            .pageurl_mpv(&page(
                200,
                "u",
                "https://e-hentai.org/mpv/123/abcdef0123",
                "imagelist = nope;",
            ))
            .expect("invalid json");
        assert_eq!(outcome(&result), "exception");
        assert_eq!(int_field(&result, "fail_code"), Some(1008));
        assert_eq!(
            text_field(&result, "exception").as_deref(),
            Some("TypeError")
        );
    }

    #[test]
    fn imgurl_matches_keystamp_style_and_original_branches() {
        let mut filters = loaded();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let image_url = format!("https://img.example/{hash}-100-200-0-jpg/keystamp=abc");
        let body = format!(
            "<img src=\"{image_url}\" style=\"max-width:100%\">\
             </a></div><div>pretty name.jpg :: 100x200 :: 2 KB</di>\
             <a href=\"https://img.example/original/name.jpg\">Download original 100 x 200 2 KB source</a>\
             return nl('123456-7')"
        );
        let page_url = "https://e-hentai.org/s/abcdef0123/123456-7?keystamp=abc".to_string();
        let mut response = page(200, &page_url, REAL_URL, &body);
        response.content_length = 5000;
        response.headers = vec![("Content-Type".into(), "text/html".into())];
        let result = filters.imgurl(&response).expect("imgurl");
        assert_eq!(outcome(&result), "ok");
        assert_eq!(
            text_field(&result, "filename").as_deref(),
            Some("pretty name.jpg")
        );
        assert!(
            text_field(&result, "image_url")
                .unwrap()
                .contains("keystamp=abc")
        );
        assert!(
            text_field(&result, "reload_url")
                .unwrap()
                .ends_with("nl=123456-7")
        );

        let invalid = page(200, "u", REAL_URL, "Invalid page");
        let result = filters.imgurl(&invalid).expect("invalid");
        assert_eq!(int_field(&result, "code"), Some(1005));
    }

    #[test]
    fn quota_wrapper_keeps_reference_order_and_codes() {
        let mut filters = loaded();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let url = format!("https://img.example/{hash}-100-200-0-jpg/");
        let mut response = page(200, &url, &url, "hello");
        response.content_length = 925;
        let result = filters.imgurl(&response).expect("quota");
        assert_eq!(outcome(&result), "fail");
        assert_eq!(int_field(&result, "code"), Some(1006));

        response.content_length = 42;
        response.headers = vec![("Content-Type".into(), "text/html".into())];
        let mut text_response = response.clone();
        text_response.text = "You have exceeded your image viewing limits".into();
        let result = filters.imgurl(&text_response).expect("text quota");
        assert_eq!(int_field(&result, "code"), Some(1006));

        let mut expired = page(403, "u", "u", "");
        expired.content_length = 12;
        let result = filters.imgurl(&expired).expect("403");
        assert_eq!(int_field(&result, "code"), Some(1007));
    }

    #[test]
    fn download_wrapper_distinguishes_404_length_and_stream() {
        let mut filters = loaded();
        let hash = "0123456789abcdef0123456789abcdef01234567";
        let url = format!("https://img.example/{hash}-1000-200-0-jpg/");
        let mut response = page(200, &url, &url, "");
        response.content_length = 1000;
        response.has_stream = true;
        let result = filters.download(&response).expect("download");
        assert_eq!(outcome(&result), "stream", "{result:?}");

        response.has_stream = false;
        let result = filters.download(&response).expect("download no stream");
        assert_eq!(int_field(&result, "code"), Some(1014));

        let missing = page(404, &url, &url, "");
        let result = filters.download(&missing).expect("download 404");
        assert_eq!(int_field(&result, "code"), Some(1011));

        let result = filters
            .stream_result(&response, 999, None)
            .expect("stream result");
        assert_eq!(outcome(&result), "broken");
        assert_eq!(int_field(&result, "code"), Some(1012));
        let result = filters
            .stream_result(&response, 1000, None)
            .expect("stream result ok");
        assert_eq!(outcome(&result), "ok");
    }

    #[test]
    fn reset_quota_is_a_real_no_op() {
        let mut filters = loaded();
        let result = filters
            .reset_quota(&page(200, "u", "u", ""))
            .expect("reset quota");
        assert_eq!(result, Value::Bool(false));
    }

    const PROBE: &str = r#"
use json;

struct PageResponse {
    status: int,
    url: string,
    real_url: string,
    content_length: int,
    text: string,
    headers: [[string]],
    ori: bool,
    has_stream: bool,
}

pub fn flt_probe(page: PageResponse) -> string {
    let pattern = "Length:</td><td class=\"gdt2\">(\\d+)\\s+page";
    let found = flt::find_all(pattern, page.text);
    let mut joined = "";
    for index in 0..found.length {
        let row = &found[index];
        joined = joined + row[1];
    }
    joined
}

pub fn flt_match_probe(text: string) -> string {
    if flt::match_prefix("This gallery", text) => {
        "yes"
    } else => {
        "no"
    }
}

pub fn flt_header_probe(headers: [[string]]) -> string {
    let value = flt::header_get(headers, "Set-Cookie");
    let rendered = match value {
        null => "",
        _ => value.unwrap_or(""),
    };
    rendered
}

pub fn flt_json_probe(text: string) -> string {
    let list = json::decode::<[map<string>]>(text);
    let mut out = "";
    for index in 0..list.length {
        let row = &list[index];
        out = out + row["k"];
    }
    out
}

pub fn flt_fmt_probe(n: int, text: string) -> string {
    let out: string = json::encode(n);
    out
}

0;
"#;

    #[test]
    fn sdk_compiles_binds_and_calls_exported_filter_functions() {
        let mut vm = compile_module(PROBE).expect("probe module");
        let callable = vm
            .resolve_exported_callable("flt_probe")
            .expect("exported probe");
        let page = PageResponse {
            status: 200,
            url: "https://e-hentai.org/g/1/abc/".into(),
            real_url: "https://e-hentai.org/g/1/abc/".into(),
            content_length: 42,
            text: "Length:</td><td class=\"gdt2\">12 pages</td>".into(),
            headers: vec![("Content-Type".into(), "text/html".into())],
            ori: false,
            has_stream: false,
        };
        let result = vm
            .invoke_callable(callable.clone(), &[page_value(&page)])
            .expect("probe call");
        assert_eq!(result, Value::string("12"));

        let callable = vm
            .resolve_exported_callable("flt_match_probe")
            .expect("match probe");
        let result = vm
            .invoke_callable(callable.clone(), &[Value::string("This gallery is pining")])
            .expect("match call");
        assert_eq!(result, Value::string("yes"));
        let result = vm
            .invoke_callable(callable, &[Value::string("x This gallery")])
            .expect("match call");
        assert_eq!(result, Value::string("no"));

        let callable = vm
            .resolve_exported_callable("flt_header_probe")
            .expect("header probe");
        let result = vm
            .invoke_callable(
                callable,
                &[Value::array(vec![
                    Value::array(vec![
                        Value::string("set-cookie"),
                        Value::string("a=1; Path=/"),
                    ]),
                    Value::array(vec![
                        Value::string("Set-Cookie"),
                        Value::string("b=2; Path=/"),
                    ]),
                ])],
            )
            .expect("header call");
        assert_eq!(result, Value::string("a=1; Path=/, b=2; Path=/"));

        let callable = vm
            .resolve_exported_callable("flt_json_probe")
            .expect("json probe");
        let result = vm
            .invoke_callable(
                callable,
                &[Value::string(
                    r#"[{"k":"abc","w":1280},{"k":"def","w":720}]"#,
                )],
            )
            .expect("json call");
        assert_eq!(result, Value::string("abcdef"));

        let callable = vm
            .resolve_exported_callable("flt_fmt_probe")
            .expect("fmt probe");
        let result = vm
            .invoke_callable(callable, &[Value::Int(7), Value::string("XABCx")])
            .expect("fmt call");
        assert_eq!(result, Value::string("7"));
    }
}
