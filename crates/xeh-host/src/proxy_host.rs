//! Application-owned generic HTTP operation with an immutable route.
//!
//! The pinned Core HTTP host always follows redirects and has no proxy route,
//! which cannot express the Glype flow (a sslagree response must be observed
//! with redirects disabled) or a pool-selected connector. This host is the
//! minimal application primitive that closes those two execution gaps: one
//! bounded GET, explicit no-follow, raw response headers/body, and a typed
//! proxy route that reuses the same native connector as the transfer worker.
//! Arguments and the response are plain named scalars/arrays; no raw map or
//! unknown value crosses the host schema, and all URL/cookie/business
//! decisions remain in RSS.

use std::sync::Arc;
use std::time::Instant;

use http_body_util::BodyExt;
use vm::{
    register_catalog_function, CallOutcome, CallReturn, HostApiCatalog, HostFunction,
    HostFunctionRegistry, HostFunctionSchema, HostFutureOutput, HostParamSchema, HostStructField,
    HostStructSchema, HostTypeSchema, Value, Vm, VmResult,
};

use crate::dto::{HeaderEntry, TransportHooks};
use crate::net::{open_get_with_options, NativeProxyRoute, OpenOptions, ProxyKind};
use crate::observe::Metrics;
use crate::types::HostError;

const MAX_BODY_BYTES: usize = 4 * 1024 * 1024;

/// Host state for one proxy scenario VM. The policy is fixed before the guest
/// starts, so the guest cannot widen its own authorization between requests.
pub struct ProxyHost {
    pub http: vm::HttpConfig,
    pub hooks: TransportHooks,
    pub session: std::sync::Arc<crate::proxy_exchange::ProxySession>,
    pub live: std::sync::Arc<crate::proxy_exchange::LiveGeneration>,
    pub grants: Vec<crate::task_control::ProxyGrant>,
}

impl ProxyHost {
    pub fn new(
        root: std::path::PathBuf,
        http: vm::HttpConfig,
        hooks: TransportHooks,
        grants: Vec<crate::task_control::ProxyGrant>,
    ) -> Arc<Self> {
        Arc::new(Self {
            http,
            hooks,
            session: crate::proxy_exchange::ProxySession::new(root),
            live: crate::proxy_exchange::LiveGeneration::new(),
            grants,
        })
    }
}

/// Standard catalog plus task and proxy declarations. `task_catalog` and
/// `proxy_catalog` both use this composition; neither calls the other.
pub fn compose_application_catalog() -> Arc<HostApiCatalog> {
    let standard = crate::catalog::standard_catalog();
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
    for schema in proxy_structs() {
        builder.named_struct(schema);
    }
    for schema in crate::proxy_exchange::exchange_structs() {
        builder.named_struct(schema);
    }
    for resource in crate::proxy_exchange::exchange_resources() {
        builder.resource(resource);
    }
    for function in proxy_functions() {
        builder.function(function);
    }
    for function in crate::proxy_exchange::exchange_functions() {
        builder.function(function);
    }
    builder.function(admit_function());
    for function in crate::worker::permits::functions() {
        builder.function(function);
    }
    Arc::new(builder.build().expect("application host catalog"))
}

pub fn proxy_catalog() -> Arc<HostApiCatalog> {
    compose_application_catalog()
}

pub fn register_proxy(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    host: Arc<ProxyHost>,
) -> Result<(), HostError> {
    let schema = catalog
        .function("xeh_proxy::request")
        .ok_or_else(|| HostError::bind("missing application schema xeh_proxy::request"))?
        .clone();
    let request_host = Arc::clone(&host);
    register_catalog_function(registry, catalog, "xeh_proxy::request", schema, move || {
        Box::new(ProxyRequestFn {
            host: Arc::clone(&request_host),
        })
    })
    .map_err(|error| HostError::bind(error.to_string()))?;
    crate::proxy_exchange::register_exchange(registry, catalog, Arc::clone(&host))?;
    register_admit(registry, catalog, host)
}

pub fn register_proxy_denied(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
) -> Result<(), HostError> {
    for name in [
        "xeh_proxy::request",
        "xeh_proxy::exchange",
        "xeh_proxy::body_scan",
        "xeh_proxy::body_text",
        "xeh_proxy::body_inline",
        "xeh_proxy::body_stage",
        "xeh_proxy::body_close",
        "xeh_proxy::admit",
    ] {
        let schema = catalog
            .function(name)
            .ok_or_else(|| HostError::bind(format!("missing application schema {name}")))?
            .clone();
        register_catalog_function(registry, catalog, name, schema, || Box::new(DenyFn))
            .map_err(|error| HostError::bind(error.to_string()))?;
    }
    Ok(())
}

fn register_admit(
    registry: &mut HostFunctionRegistry,
    catalog: &HostApiCatalog,
    host: Arc<ProxyHost>,
) -> Result<(), HostError> {
    let schema = catalog
        .function("xeh_proxy::admit")
        .ok_or_else(|| HostError::bind("missing application schema xeh_proxy::admit"))?
        .clone();
    register_catalog_function(registry, catalog, "xeh_proxy::admit", schema, move || {
        Box::new(AdmitFn {
            host: Arc::clone(&host),
        })
    })
    .map_err(|error| HostError::bind(error.to_string()))
}

fn admit_function() -> HostFunctionSchema {
    HostFunctionSchema::with_return(
        "xeh_proxy::admit",
        vec![HostParamSchema::value("url", HostTypeSchema::String)],
        HostTypeSchema::String,
    )
    .with_description("Empty string when the fixed HTTP policy admits the URL")
}

struct AdmitFn {
    host: Arc<ProxyHost>,
}

impl HostFunction for AdmitFn {
    fn call(&mut self, _vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let url = match args.first() {
            Some(Value::String(text)) => text.as_str(),
            _ => "",
        };
        let result = match crate::proxy_exchange::admit_url(&self.host.http, url) {
            Ok(()) => "",
            Err(error) => error,
        };
        Ok(CallOutcome::Return(CallReturn::one(Value::string(
            result.to_string(),
        ))))
    }
}

struct DenyFn;

impl HostFunction for DenyFn {
    fn call(&mut self, _vm: &mut Vm, _args: &[Value]) -> VmResult<CallOutcome> {
        Err(vm::VmError::HostError("proxy_denied".to_string()))
    }
}

fn proxy_structs() -> Vec<HostStructSchema> {
    vec![HostStructSchema::new("ProxyResponse", response_fields())]
}

fn proxy_functions() -> Vec<HostFunctionSchema> {
    vec![HostFunctionSchema::with_return(
        "xeh_proxy::request",
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
            HostParamSchema::value("route_kind", HostTypeSchema::String),
            HostParamSchema::value("route_host", HostTypeSchema::String),
            HostParamSchema::value("route_port", HostTypeSchema::Int),
            HostParamSchema::value("trusted_remote_dns", HostTypeSchema::Bool),
            HostParamSchema::value("follow_redirects", HostTypeSchema::Bool),
        ],
        response_type(),
    )
    .with_description("One bounded HTTP GET with an explicit route and redirect policy")]
}

fn response_fields() -> Vec<HostStructField> {
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
        HostStructField::new("body", HostTypeSchema::Array(Box::new(HostTypeSchema::Int))),
        HostStructField::new("body_text", HostTypeSchema::String),
        HostStructField::new("text_valid", HostTypeSchema::Bool),
        HostStructField::new("error_kind", HostTypeSchema::String),
        HostStructField::new("error_detail", HostTypeSchema::String),
    ]
}

fn response_type() -> HostTypeSchema {
    HostTypeSchema::Named {
        name: "ProxyResponse".to_string(),
        fields: response_fields(),
    }
}

struct ParsedRequest {
    url: String,
    headers: Vec<HeaderEntry>,
    route: Option<NativeProxyRoute>,
    follow_redirects: bool,
}

struct ProxyRequestFn {
    host: Arc<ProxyHost>,
}

impl HostFunction for ProxyRequestFn {
    fn call(&mut self, vm: &mut Vm, args: &[Value]) -> VmResult<CallOutcome> {
        let parsed =
            parse_request(args).map_err(|error| vm::VmError::HostError(error.to_string()))?;
        crate::proxy_exchange::ensure_lease(vm, &self.host.live)?;
        let host = Arc::clone(&self.host);
        let future = async move {
            let response = execute(&host, parsed).await;
            Ok(HostFutureOutput::complete(move |_vm| {
                Ok(CallReturn::one(response_to_value(&response)))
            }))
        };
        vm.submit_host_future(Box::pin(future))
    }
}

struct ProxyResponse {
    status: i64,
    url: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
    error_kind: String,
    error_detail: String,
}

async fn execute(host: &ProxyHost, request: ParsedRequest) -> ProxyResponse {
    let metrics = Metrics::default();
    let owner = crate::proxy_exchange::session_owner(host);
    let cancel = crate::proxy_exchange::child_cancel(host);
    let started = Instant::now();
    let mut opened = match open_get_with_options(
        &host.http,
        &request.url,
        &request.headers,
        &cancel,
        &metrics,
        &host.hooks,
        &owner,
        started,
        OpenOptions {
            route: request.route.as_ref(),
            follow_redirects: request.follow_redirects,
            allow_redirect_response: !request.follow_redirects,
            return_raw_status: false,
            method: "GET",
            body: b"",
        },
    )
    .await
    {
        Ok(opened) => opened,
        Err(code) => {
            return error_response(&request.url, classify_error(code), code);
        }
    };
    let mut body = Vec::new();
    let deadline = started + host.http.request_timeout.min(host.http.max_stream_duration);
    loop {
        if Instant::now() >= deadline || cancel.is_cancelled() {
            let _ = opened
                .connection
                .shutdown(crate::net::cleanup_deadline(deadline))
                .await;
            return error_response(&request.url, "ConnectionError", "deadline");
        }
        let frame = tokio::select! {
            biased;
            _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => {
                let _ = opened.connection.shutdown(crate::net::cleanup_deadline(deadline)).await;
                return error_response(&request.url, "ConnectionError", "deadline");
            }
            frame = opened.body.frame() => frame,
        };
        match frame {
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    if body.len().saturating_add(data.len()) > MAX_BODY_BYTES {
                        let _ = opened
                            .connection
                            .shutdown(crate::net::cleanup_deadline(deadline))
                            .await;
                        return error_response(&request.url, "ResponseTooLarge", "body_limit");
                    }
                    body.extend_from_slice(&data);
                }
            }
            None => break,
            Some(Err(_)) => {
                let _ = opened
                    .connection
                    .shutdown(crate::net::cleanup_deadline(deadline))
                    .await;
                return error_response(&request.url, "ConnectionError", "transport_failed");
            }
        }
    }
    let status = i64::from(opened.status);
    let url = opened.url.clone();
    let headers = opened.headers.clone();
    let _ = opened
        .connection
        .shutdown(crate::net::cleanup_deadline(deadline))
        .await;
    ProxyResponse {
        status,
        url,
        headers,
        body,
        error_kind: String::new(),
        error_detail: String::new(),
    }
}

fn classify_error(code: &'static str) -> &'static str {
    match code {
        "deadline" => "ConnectTimeout",
        "proxy_unsupported" => "ProxySchemeUnknown",
        "proxy_tls_failed" | "target_tls_failed" => "ConnectionError",
        "policy_denied" => "policy_denied",
        "header_rejected" => "header_rejected",
        "route_invalid" => "route_invalid",
        "method_rejected" => "method_rejected",
        "proxy_not_granted" => "proxy_not_granted",
        "proxy_status" | "proxy_handshake_failed" | "proxy_auth_required" => "ProxyError",
        "redirect_limit" => "TooManyRedirects",
        "connect_failed"
        | "proxy_connect_failed"
        | "proxy_resolution_failed"
        | "transport_failed"
        | "resolution_failed" => "ConnectionError",
        _ => "ConnectionError",
    }
}

fn error_response(url: &str, kind: &str, detail: &str) -> ProxyResponse {
    ProxyResponse {
        status: 0,
        url: url.to_string(),
        headers: Vec::new(),
        body: Vec::new(),
        error_kind: kind.to_string(),
        error_detail: detail.to_string(),
    }
}

fn string_arg(args: &[Value], index: usize, name: &'static str) -> Result<String, &'static str> {
    match args.get(index) {
        Some(Value::String(text)) => Ok((**text).clone()),
        _ => Err(match name {
            "url" => "proxy request url must be a string",
            "method" => "proxy request method must be a string",
            _ => "proxy request string argument",
        }),
    }
}

fn strings_arg(args: &[Value], index: usize) -> Result<Vec<String>, &'static str> {
    let Some(Value::Array(items)) = args.get(index) else {
        return Err("proxy request header array must be an array of strings");
    };
    let mut output = Vec::with_capacity(items.len());
    for item in items.iter() {
        let Value::String(text) = item else {
            return Err("proxy request header array must be an array of strings");
        };
        output.push((**text).clone());
    }
    Ok(output)
}

fn parse_request(args: &[Value]) -> Result<ParsedRequest, &'static str> {
    let url = string_arg(args, 0, "url")?;
    let method = string_arg(args, 1, "method")?;
    if method != "GET" {
        return Err("method_rejected");
    }
    let names = strings_arg(args, 2)?;
    let values = strings_arg(args, 3)?;
    if names.len() != values.len() {
        return Err("proxy request header arrays differ in length");
    }
    let headers = names
        .into_iter()
        .zip(values)
        .map(|(name, value)| HeaderEntry {
            name: name.into_bytes(),
            value: value.into_bytes(),
        })
        .collect();
    let route_kind = string_arg(args, 4, "route_kind")?;
    let route_host = string_arg(args, 5, "route_host")?;
    let route_port = match args.get(6) {
        Some(Value::Int(value)) => u16::try_from(*value).map_err(|_| "route_invalid")?,
        _ => return Err("proxy route port must be an int"),
    };
    let trusted = match args.get(7) {
        Some(Value::Bool(value)) => *value,
        _ => false,
    };
    let follow_redirects = match args.get(8) {
        Some(Value::Bool(value)) => *value,
        _ => false,
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
    Ok(ParsedRequest {
        url,
        headers,
        route,
        follow_redirects,
    })
}

fn response_to_value(response: &ProxyResponse) -> Value {
    let names = response
        .headers
        .iter()
        .map(|(name, _)| Value::string(name.clone()))
        .collect();
    let values = response
        .headers
        .iter()
        .map(|(_, value)| Value::string(value.clone()))
        .collect();
    let body = response
        .body
        .iter()
        .map(|byte| Value::Int(i64::from(*byte)))
        .collect();
    let mut entries = vec![
        (Value::string("status"), Value::Int(response.status)),
        (Value::string("url"), Value::string(response.url.clone())),
        (Value::string("header_names"), Value::array(names)),
        (Value::string("header_values"), Value::array(values)),
        (Value::string("body"), Value::array(body)),
        (
            Value::string("error_kind"),
            Value::string(response.error_kind.clone()),
        ),
        (
            Value::string("error_detail"),
            Value::string(response.error_detail.clone()),
        ),
        (
            Value::string("text_valid"),
            Value::Bool(std::str::from_utf8(&response.body).is_ok()),
        ),
    ];
    let body_text = std::str::from_utf8(&response.body)
        .unwrap_or("")
        .to_string();
    entries.push((Value::string("body_text"), Value::string(body_text)));
    Value::map(entries)
}
