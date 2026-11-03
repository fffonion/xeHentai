//! Update check host (`updater/github.py:17-35`, `updater/updater.py:21-63`).
//!
//! The business decisions live in `rss/updater/github.rss` (source binding,
//! channel query, first commit, named fields, CR/LF rewrite, native bundle
//! link) and `rss/updater/updater.rss` (mode, same-update-id, check/download
//! branch, log selection). This module only:
//!
//! * loads the trusted install-resource source binding and derives the exact
//!   `HttpConfig` allowlist (one scheme/host/port, no gallery cookies);
//! * compiles/binds both scripts against the existing Core SDK surface and
//!   drives the buffered `http::client::request`; and
//! * converts the named script results into typed values.
//!
//! The download/apply stages are the native installation primitives in
//! [`package`] (the pending envelope) and [`install`] (complete candidate
//! preparation, live replacement and re-execution); [`descriptor`] holds the
//! signed package contract. The RSS modules still pick the channel, the update
//! id and whether to download, apply or drop a pending candidate, and the
//! native side enforces the signature, digest, ABI, path, platform and
//! preparation safety whatever a script asks for.
//!
//! The owned background task, cancellation and logger sink belong to the
//! existing `main`/`run_supervised` owner (see `src/main.rs`).

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use vm::{
    BuiltinFunction, CapabilityProfile, CompileSourceFileOptions, HostFunctionRegistry, HttpConfig,
    HttpHostExt, SourceFlavor, Value, Vm, VmStatus, compile_source_with_flavor_and_options,
};

pub mod descriptor;
pub mod error;
pub mod install;
pub mod package;

pub use error::UpdateError;

pub const GITHUB_RSS: &str = "rss/updater/github.rss";
pub const UPDATER_RSS: &str = "rss/updater/updater.rss";

/// The exact HTTP host imports the updater scripts may call. Nothing else is
/// granted, so the gallery/session/proxy surface is unreachable from a check.
const HTTP_HOST_IMPORTS: &[&str] = &[
    "http::request::new",
    "http::request::set_header",
    "http::client::request",
    "http::response::status",
    "http::response::url",
    "http::response::body",
];

const BUILTIN_CALLS: &[&str] = &["json::decode", "bytes::to_utf8", "string_replace_literal"];

/// Named update metadata (`xeHentai/updater/__init__.py` `UpdateInfo`).
///
/// The four reference fields keep their names; the three transport fields are
/// the native package sidecars the RSS business module derives from the same
/// selected commit (one fixed convention, never a provider API).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub update_id: String,
    pub download_link: String,
    pub ts: String,
    pub message: String,
    pub descriptor_link: String,
    pub signature_link: String,
    pub archive_prefix: String,
}

/// One typed log action selected by `rss/updater/updater.rss`; the App logger
/// renders `key` with the existing locale catalog.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEvent {
    pub level: String,
    pub key: String,
    pub args: Vec<String>,
}

/// Typed result of one check. `outcome`/`update` are part of the named
/// result contract and are asserted by the check tests; the logging owner only
/// needs `events`.
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct CheckReport {
    pub outcome: String,
    pub update: Option<UpdateInfo>,
    pub events: Vec<LogEvent>,
}

/// Typed view of `SourceBinding` in `rss/updater/github.rss`. The binding is
/// also the trusted HTTP allowlist: the exact API and package origins, nothing
/// else. `verification_key` is the detached-signature public key (PEM); it is
/// `None` in the production binding until the release stage binds it, and a
/// missing key always rejects a package.
#[derive(Clone, Debug)]
pub struct SourceBinding {
    pub api_origin: String,
    pub repo: String,
    pub stable_channel: String,
    pub beta_channel: Option<String>,
    pub package_download_origin: String,
    pub verification_key: Option<String>,
}

struct Plan {
    outcome: String,
    url: String,
    headers: Vec<(String, String)>,
    exception: Option<String>,
}

struct Fetched {
    status: i64,
    body: String,
}

struct ParsedCommit {
    outcome: String,
    info: Option<UpdateInfo>,
    exception: Option<String>,
}

struct Decision {
    outcome: String,
    events: Vec<LogEvent>,
}

pub struct Updater {
    github: Vm,
    github_plan: Value,
    github_parse: Value,
    github_prefix: Value,
    updater: Vm,
    updater_fetch: Value,
    updater_fetch_bytes: Value,
    updater_mode: Value,
    updater_decide: Value,
    updater_failure: Value,
    updater_pending: Value,
    updater_startup: Value,
    binding: SourceBinding,
    install: install::Installation,
    /// The one owned blocking publication job of this updater instance.
    publisher: install::Publisher,
}

fn registry() -> &'static HostFunctionRegistry {
    static REGISTRY: OnceLock<HostFunctionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = HostFunctionRegistry::restricted();
        let mut profile = CapabilityProfile::builder();
        for name in BUILTIN_CALLS {
            let builtin = BuiltinFunction::from_namespaced_name(name)
                .or_else(|| BuiltinFunction::from_source_name(name))
                .unwrap_or_else(|| panic!("unknown updater builtin {name}"));
            profile = profile.allow_builtin(builtin);
        }
        for name in HTTP_HOST_IMPORTS {
            profile = profile.allow_host_import(*name);
        }
        registry.set_capability_profile(profile.build());
        registry
    })
}

fn compile(script: &'static str, source: &str) -> Result<Vm, UpdateError> {
    // No explicit catalog: the locked Core installs its HTTP subcatalog, which
    // is exactly the surface these scripts import.
    let options = CompileSourceFileOptions::new();
    let compiled =
        compile_source_with_flavor_and_options(source, SourceFlavor::RustScript, options).map_err(
            |error| UpdateError::Compile {
                script,
                message: error.to_string(),
            },
        )?;
    let mut vm = Vm::new(compiled.program);
    registry()
        .bind_vm_cached(&mut vm)
        .map_err(|error| UpdateError::Bind {
            script,
            message: error.to_string(),
        })?;
    match vm.run() {
        Ok(VmStatus::Halted) => Ok(vm),
        Ok(status) => Err(UpdateError::Runtime {
            script,
            message: format!("module ended with {status:?}"),
        }),
        Err(error) => Err(UpdateError::Runtime {
            script,
            message: error.to_string(),
        }),
    }
}

fn callable(vm: &Vm, script: &'static str, name: &str) -> Result<Value, UpdateError> {
    vm.resolve_exported_callable(name)
        .map_err(|error| UpdateError::Runtime {
            script,
            message: format!("{name}: {error}"),
        })
}

fn map_of<'a>(value: &'a Value, what: &'static str) -> Result<&'a vm::VmMap, UpdateError> {
    match value {
        Value::Map(map) => Ok(map),
        other => Err(UpdateError::Shape {
            what,
            message: format!("not a map: {other:?}"),
        }),
    }
}

fn field<'a>(map: &'a vm::VmMap, name: &str) -> Option<&'a Value> {
    map.get(&Value::string(name))
}

fn text_of(map: &vm::VmMap, name: &str, what: &'static str) -> Result<String, UpdateError> {
    match field(map, name) {
        Some(Value::String(text)) => Ok((**text).clone()),
        Some(other) => Err(UpdateError::Shape {
            what,
            message: format!("field {name} is not a string: {other:?}"),
        }),
        None => Err(UpdateError::Shape {
            what,
            message: format!("missing field {name}"),
        }),
    }
}

fn optional_text(
    map: &vm::VmMap,
    name: &str,
    what: &'static str,
) -> Result<Option<String>, UpdateError> {
    match field(map, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some((**text).clone())),
        Some(other) => Err(UpdateError::Shape {
            what,
            message: format!("field {name} is not a string: {other:?}"),
        }),
    }
}

fn int_of(map: &vm::VmMap, name: &str, what: &'static str) -> Result<i64, UpdateError> {
    match field(map, name) {
        Some(Value::Int(value)) => Ok(*value),
        Some(other) => Err(UpdateError::Shape {
            what,
            message: format!("field {name} is not an int: {other:?}"),
        }),
        None => Err(UpdateError::Shape {
            what,
            message: format!("missing field {name}"),
        }),
    }
}

fn strings_of(map: &vm::VmMap, name: &str, what: &'static str) -> Result<Vec<String>, UpdateError> {
    match field(map, name) {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(text) => Ok((**text).clone()),
                other => Err(UpdateError::Shape {
                    what,
                    message: format!("field {name} contains a non-string: {other:?}"),
                }),
            })
            .collect(),
        Some(other) => Err(UpdateError::Shape {
            what,
            message: format!("field {name} is not an array: {other:?}"),
        }),
        None => Err(UpdateError::Shape {
            what,
            message: format!("missing field {name}"),
        }),
    }
}

fn binding_value(binding: &SourceBinding) -> Value {
    let mut entries = vec![
        (
            Value::string("api_origin"),
            Value::string(binding.api_origin.clone()),
        ),
        (Value::string("repo"), Value::string(binding.repo.clone())),
        (
            Value::string("stable_channel"),
            Value::string(binding.stable_channel.clone()),
        ),
        (
            Value::string("package_download_origin"),
            Value::string(binding.package_download_origin.clone()),
        ),
    ];
    entries.push((
        Value::string("beta_channel"),
        match &binding.beta_channel {
            Some(channel) => Value::string(channel.clone()),
            None => Value::Null,
        },
    ));
    entries.push((
        Value::string("verification_key"),
        match &binding.verification_key {
            Some(key) => Value::string(key.clone()),
            None => Value::Null,
        },
    ));
    Value::map(entries)
}

fn decode_binding(value: &Value) -> Result<SourceBinding, UpdateError> {
    let what = "source binding";
    let map = map_of(value, what)?;
    Ok(SourceBinding {
        api_origin: text_of(map, "api_origin", what)?,
        repo: text_of(map, "repo", what)?,
        stable_channel: text_of(map, "stable_channel", what)?,
        beta_channel: optional_text(map, "beta_channel", what)?,
        package_download_origin: text_of(map, "package_download_origin", what)?,
        verification_key: optional_text(map, "verification_key", what)?,
    })
}

fn decode_plan(value: &Value) -> Result<Plan, UpdateError> {
    let what = "request plan";
    let map = map_of(value, what)?;
    let headers = match field(map, "headers") {
        Some(Value::Array(pairs)) => {
            let mut out = Vec::with_capacity(pairs.len());
            for pair in pairs.iter() {
                let Value::Array(items) = pair else {
                    return Err(UpdateError::Shape {
                        what,
                        message: "header pair is not an array".into(),
                    });
                };
                let (Some(Value::String(name)), Some(Value::String(value))) =
                    (items.first(), items.get(1))
                else {
                    return Err(UpdateError::Shape {
                        what,
                        message: "header pair is not two strings".into(),
                    });
                };
                out.push(((**name).clone(), (**value).clone()));
            }
            out
        }
        _ => {
            return Err(UpdateError::Shape {
                what,
                message: "missing header array".into(),
            });
        }
    };
    Ok(Plan {
        outcome: text_of(map, "outcome", what)?,
        url: text_of(map, "url", what)?,
        headers,
        exception: optional_text(map, "exception", what)?,
    })
}

fn decode_fetched(value: &Value) -> Result<Fetched, UpdateError> {
    let what = "fetch result";
    let map = map_of(value, what)?;
    Ok(Fetched {
        status: int_of(map, "status", what)?,
        body: text_of(map, "body", what)?,
    })
}

fn decode_commit(value: &Value) -> Result<ParsedCommit, UpdateError> {
    let what = "commit result";
    let map = map_of(value, what)?;
    let outcome = text_of(map, "outcome", what)?;
    let exception = optional_text(map, "exception", what)?;
    let info = if outcome == "ok" {
        Some(UpdateInfo {
            update_id: text_of(map, "update_id", what)?,
            download_link: text_of(map, "download_link", what)?,
            ts: text_of(map, "ts", what)?,
            message: text_of(map, "message", what)?,
            descriptor_link: text_of(map, "descriptor_link", what)?,
            signature_link: text_of(map, "signature_link", what)?,
            archive_prefix: text_of(map, "archive_prefix", what)?,
        })
    } else {
        None
    };
    Ok(ParsedCommit {
        outcome,
        info,
        exception,
    })
}

fn decode_decision(value: &Value) -> Result<Decision, UpdateError> {
    let what = "check decision";
    let map = map_of(value, what)?;
    let outcome = text_of(map, "outcome", what)?;
    let events = match field(map, "events") {
        Some(Value::Array(items)) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items.iter() {
                let item = map_of(item, "log event")?;
                out.push(LogEvent {
                    level: text_of(item, "level", "log event")?,
                    key: text_of(item, "key", "log event")?,
                    args: strings_of(item, "args", "log event")?,
                });
            }
            out
        }
        _ => {
            return Err(UpdateError::Shape {
                what,
                message: "missing events array".into(),
            });
        }
    };
    Ok(Decision { outcome, events })
}

/// Exact scheme/host/port from one binding origin. A private/loopback address
/// is admitted only when the origin itself is a loopback IP, i.e. an explicitly
/// configured owned fixture binding; production ships public HTTPS origins.
struct Origin {
    scheme: String,
    host: String,
    port: u16,
    loopback: bool,
}

fn http_origin(origin: &str, what: &'static str) -> Result<Origin, UpdateError> {
    let url = url::Url::parse(origin).map_err(|error| {
        UpdateError::Binding(format!("{what} origin {origin:?} is not a URL: {error}"))
    })?;
    if !url.username().is_empty() || url.password().is_some() {
        return Err(UpdateError::Binding(format!(
            "{what} origin must not carry credentials"
        )));
    }
    if url.query().is_some() || url.fragment().is_some() || url.path() != "/" {
        return Err(UpdateError::Binding(format!(
            "{what} origin must be scheme://host[:port] without a path"
        )));
    }
    let scheme = url.scheme().to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return Err(UpdateError::Binding(format!(
            "{what} origin scheme {scheme:?} is not http(s)"
        )));
    }
    let host = url
        .host_str()
        .ok_or_else(|| UpdateError::Binding(format!("{what} origin has no host")))?
        .to_ascii_lowercase();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| UpdateError::Binding(format!("{what} origin has no known port")))?;
    let loopback = host
        .parse::<std::net::IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or(false);
    Ok(Origin {
        scheme,
        host,
        port,
        loopback,
    })
}

/// The transport policy: exactly the two explicitly bound origins (API and
/// package download), no gallery cookies. The response bound is the one
/// setting that has to grow for a real native package: a payload archive with
/// the executable plus resources. Nothing else is relaxed.
fn http_policy(binding: &SourceBinding) -> Result<HttpConfig, UpdateError> {
    let api = http_origin(&binding.api_origin, "api")?;
    let package = http_origin(&binding.package_download_origin, "package download")?;
    let mut config = HttpConfig {
        allowed_schemes: Vec::new(),
        allowed_hosts: Vec::new(),
        allowed_ports: Vec::new(),
        allow_private_ips: api.loopback || package.loopback,
        max_response_body_bytes: package::MAX_PAYLOAD_BYTES + 1024 * 1024,
        ..HttpConfig::default()
    };
    for origin in [api, package] {
        if !config.allowed_schemes.contains(&origin.scheme) {
            config.allowed_schemes.push(origin.scheme);
        }
        if !config.allowed_hosts.contains(&origin.host) {
            config.allowed_hosts.push(origin.host);
        }
        if !config.allowed_ports.contains(&origin.port) {
            config.allowed_ports.push(origin.port);
        }
    }
    Ok(config)
}

impl Updater {
    /// Load both trusted business modules and their binding. Nothing is
    /// requested here; the loaded `Updater` owns only its two VMs.
    pub fn load(resource_root: &Path) -> Result<Self, UpdateError> {
        let github_source = read_script(resource_root, GITHUB_RSS)?;
        let updater_source = read_script(resource_root, UPDATER_RSS)?;
        let mut github = compile(GITHUB_RSS, &github_source)?;
        let github_binding = callable(&github, GITHUB_RSS, "gh_binding")?;
        let github_plan = callable(&github, GITHUB_RSS, "gh_plan")?;
        let github_parse = callable(&github, GITHUB_RSS, "gh_parse")?;
        let github_prefix = callable(&github, GITHUB_RSS, "gh_prefix")?;
        let binding = decode_binding(
            &github
                .invoke_callable(github_binding.clone(), &[])
                .map_err(|error| UpdateError::Runtime {
                    script: GITHUB_RSS,
                    message: error.to_string(),
                })?,
        )?;

        let http = http_policy(&binding)?;
        if binding.beta_channel.as_deref().is_some_and(str::is_empty) {
            return Err(UpdateError::Binding(
                "beta channel must be absent or non-empty".into(),
            ));
        }
        if let Some(key) = &binding.verification_key
            && key.trim().is_empty()
        {
            return Err(UpdateError::Binding(
                "verification key must be absent or a PEM public key".into(),
            ));
        }

        let mut updater = compile(UPDATER_RSS, &updater_source)?;
        updater
            .configure_http(http)
            .map_err(|error| UpdateError::Bind {
                script: UPDATER_RSS,
                message: format!("HTTP policy: {error}"),
            })?;
        let updater_fetch = callable(&updater, UPDATER_RSS, "upd_fetch")?;
        let updater_fetch_bytes = callable(&updater, UPDATER_RSS, "upd_fetch_bytes")?;
        let updater_decide = callable(&updater, UPDATER_RSS, "upd_decide")?;
        let updater_failure = callable(&updater, UPDATER_RSS, "upd_failure")?;
        let updater_mode = callable(&updater, UPDATER_RSS, "upd_mode")?;
        let updater_pending = callable(&updater, UPDATER_RSS, "upd_pending_written")?;
        let updater_startup = callable(&updater, UPDATER_RSS, "upd_startup")?;

        Ok(Self {
            github,
            github_plan,
            github_parse,
            github_prefix,
            updater,
            updater_fetch,
            updater_fetch_bytes,
            updater_mode,
            updater_decide,
            updater_failure,
            updater_pending,
            updater_startup,
            binding,
            install: install::Installation::new(resource_root),
            publisher: install::Publisher::new(),
        })
    }

    /// Attach the publisher owner the supervisor keeps across an abort, so the
    /// shutdown drain can join the real blocking job.
    pub fn set_publisher(&mut self, publisher: install::Publisher) {
        self.publisher = publisher;
    }

    #[allow(dead_code)]
    pub fn publisher(&self) -> install::Publisher {
        self.publisher.clone()
    }

    fn pure(
        &mut self,
        script: &'static str,
        callable: &Value,
        args: &[Value],
    ) -> Result<Value, UpdateError> {
        // Split the borrow so the callable clone does not alias the mutable VM.
        let callable = callable.clone();
        self.vm_mut(script)
            .invoke_callable(callable, args)
            .map_err(|error| UpdateError::Runtime {
                script,
                message: error.to_string(),
            })
    }

    fn vm_mut(&mut self, script: &'static str) -> &mut Vm {
        match script {
            GITHUB_RSS => &mut self.github,
            _ => &mut self.updater,
        }
    }

    fn failure(&mut self, detail: &str) -> Result<CheckReport, UpdateError> {
        let callable = self.updater_failure.clone();
        let value = self.pure(UPDATER_RSS, &callable, &[Value::string(detail)])?;
        let decision = decode_decision(&value)?;
        Ok(CheckReport {
            outcome: decision.outcome,
            update: None,
            events: decision.events,
        })
    }

    /// Run one real check decision. `applied_update_id` is the id of the
    /// currently loaded build (`None` for this not-yet-updated build) and is
    /// the only input to the same-id decision. A `download` mode returns the
    /// explicit unsupported-stage outcome after the check; it never fetches a
    /// package or reports a completed update.
    ///
    /// `off` is NOT short-circuited here: only the CLI caller
    /// (`main::spawn_update_check`) suppresses the module for `off`, while a
    /// direct legacy `check_update(off)` still performs the one commits request
    /// and selects the manual-download hint (oracle `u2_off_stable_diff`). The
    /// RSS decision therefore treats `off` like `check`.
    pub async fn check(
        &mut self,
        mode: &str,
        beta: bool,
        applied_update_id: Option<&str>,
        user_agent: &str,
    ) -> Result<CheckReport, UpdateError> {
        let mode_value = {
            let callable = self.updater_mode.clone();
            self.updater
                .invoke_callable(callable, &[Value::string(mode)])
                .map_err(|error| UpdateError::Runtime {
                    script: UPDATER_RSS,
                    message: error.to_string(),
                })?
        };
        let mode_map = map_of(&mode_value, "mode decision")?;
        let mode_outcome = text_of(mode_map, "outcome", "mode decision")?;
        if mode_outcome == "invalid" {
            let detail = optional_text(mode_map, "exception", "mode decision")?
                .unwrap_or_else(|| format!("unknown auto update mode: {mode}"));
            return self.failure(&detail);
        }
        let binding = binding_value(&self.binding);
        let plan_value = {
            let callable = self.github_plan.clone();
            self.github
                .invoke_callable(
                    callable,
                    &[
                        binding.clone(),
                        Value::Bool(beta),
                        Value::string(user_agent),
                    ],
                )
                .map_err(|error| UpdateError::Runtime {
                    script: GITHUB_RSS,
                    message: error.to_string(),
                })?
        };
        let plan = decode_plan(&plan_value)?;
        if plan.outcome != "ok" {
            let detail = plan.exception.unwrap_or(plan.outcome);
            return self.failure(&detail);
        }

        let fetch_value = {
            let callable = self.updater_fetch.clone();
            let args = [
                Value::string(plan.url.clone()),
                Value::array(
                    plan.headers
                        .iter()
                        .map(|(name, value)| {
                            Value::array(vec![
                                Value::string(name.clone()),
                                Value::string(value.clone()),
                            ])
                        })
                        .collect(),
                ),
            ];
            self.call_waiting(UPDATER_RSS, &callable, &args).await?
        };
        let fetched = decode_fetched(&fetch_value)?;

        let parse_value = {
            let callable = self.github_parse.clone();
            self.github
                .invoke_callable(
                    callable,
                    &[
                        binding,
                        Value::Int(fetched.status),
                        Value::string(fetched.body),
                    ],
                )
                .map_err(|error| UpdateError::Runtime {
                    script: GITHUB_RSS,
                    message: error.to_string(),
                })?
        };
        let parsed = decode_commit(&parse_value)?;
        let Some(info) = parsed.info else {
            let detail = parsed.exception.unwrap_or(parsed.outcome);
            return self.failure(&detail);
        };

        let decide_value = {
            let callable = self.updater_decide.clone();
            let args = [
                Value::string(mode),
                Value::string(applied_update_id.unwrap_or("")),
                Value::string(info.update_id.clone()),
                Value::string(info.ts.clone()),
                Value::string(info.message.clone()),
            ];
            self.updater
                .invoke_callable(callable, &args)
                .map_err(|error| UpdateError::Runtime {
                    script: UPDATER_RSS,
                    message: error.to_string(),
                })?
        };
        let decision = decode_decision(&decide_value)?;
        if decision.outcome == "download" {
            // The RSS decision selected a real package download; the current
            // process keeps running this build while the fetch happens. The
            // availability event precedes the publication event.
            let mut report = self.download(&info, &plan.headers).await?;
            let mut events = decision.events;
            events.extend(report.events);
            report.events = events;
            return Ok(report);
        }
        Ok(CheckReport {
            outcome: decision.outcome,
            update: Some(info),
            events: decision.events,
        })
    }

    /// Fetch the trusted signed descriptor, its detached signature and the
    /// payload archive, verify all three, and publish ONE pending envelope.
    ///
    /// Nothing here changes the live installation: the running App, its task
    /// and its reported identity stay on the version that started. A rejection
    /// leaves any previous valid pending byte-identical.
    async fn download(
        &mut self,
        info: &UpdateInfo,
        headers: &[(String, String)],
    ) -> Result<CheckReport, UpdateError> {
        let descriptor_fetch = self.fetch_text(&info.descriptor_link, headers).await?;
        if descriptor_fetch.status != 200 {
            return self.failure(&format!(
                "descriptor request returned HTTP {}",
                descriptor_fetch.status
            ));
        }
        let descriptor_bytes = descriptor_fetch.body.into_bytes();
        let (signature_status, signature) = self.fetch_bytes(&info.signature_link, headers).await?;
        if signature_status != 200 {
            return self.failure(&format!(
                "signature request returned HTTP {signature_status}"
            ));
        }
        let descriptor = match descriptor::verify(
            &descriptor_bytes,
            &signature,
            self.binding.verification_key.as_deref(),
            &info.update_id,
        ) {
            Ok(descriptor) => descriptor,
            Err(error) => return self.failure(&error.to_string()),
        };

        let (payload_status, payload) = self.fetch_bytes(&info.download_link, headers).await?;
        if payload_status != 200 {
            return self.failure(&format!("payload request returned HTTP {payload_status}"));
        }
        let digest = descriptor::sha256_hex(&payload);
        if digest != descriptor.archive_sha256 {
            return self.failure(&format!(
                "payload digest {digest} does not match the signed {}",
                descriptor.archive_sha256
            ));
        }

        // Entry selection reads the real archive and the envelope is written to
        // disk: both are blocking work, so they run on the blocking pool
        // instead of holding the LocalSet that the download workers share.
        //
        // The job is OWNED by the publisher, not by this local future. If the
        // local updater task is aborted (shutdown), the cancel guard marks the
        // job cancelled; the job still finishes its staging, sees the
        // cancellation at the commit boundary and drops its staged temp file,
        // so the previous pending stays byte-identical. A commit that already
        // took the gate before the cancellation is a completed operation.
        let prefix = info.archive_prefix.clone();
        let install = self.install.clone();
        install
            .verify_private_area()
            .map_err(|error| UpdateError::install(error.to_string()))?;
        let pending_dir = install
            .pending_path()
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| UpdateError::install("the update area has no pending directory"))?;
        // One per-install cross-process transaction lock around the final
        // pending publication. A concurrent owner returns a bounded failure
        // without touching the shared candidate.
        let lock = match install::InstallLock::acquire(&install) {
            Ok(lock) => lock,
            Err(error) => return self.failure(&error.to_string()),
        };
        let publisher = self.publisher.clone();
        let job_dir = pending_dir.clone();
        let published = publisher
            .publish(job_dir, move || {
                package::select(&payload, &descriptor, &prefix)?;
                let envelope = package::PendingEnvelope::new(descriptor_bytes, signature, payload)?;
                package::stage(&pending_dir, &envelope)
            })
            .await;
        drop(lock);
        if let Err(error) = published {
            return self.failure(&error.to_string());
        }

        let callable = self.updater_pending.clone();
        let value = self.pure(
            UPDATER_RSS,
            &callable,
            &[Value::string(info.update_id.clone())],
        )?;
        let decision = decode_decision(&value)?;
        Ok(CheckReport {
            outcome: decision.outcome,
            update: Some(info.clone()),
            events: decision.events,
        })
    }

    async fn fetch_text(
        &mut self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Fetched, UpdateError> {
        let callable = self.updater_fetch.clone();
        let value = self
            .call_waiting(UPDATER_RSS, &callable, &request_args(url, headers))
            .await?;
        decode_fetched(&value)
    }

    async fn fetch_bytes(
        &mut self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<(i64, Vec<u8>), UpdateError> {
        let callable = self.updater_fetch_bytes.clone();
        let value = self
            .call_waiting(UPDATER_RSS, &callable, &request_args(url, headers))
            .await?;
        let what = "raw fetch";
        let map = map_of(&value, what)?;
        let status = int_of(map, "status", what)?;
        match field(map, "body") {
            Some(Value::Bytes(body)) => Ok((status, (**body).clone())),
            Some(other) => Err(UpdateError::Shape {
                what,
                message: format!("body is not bytes: {other:?}"),
            }),
            None => Err(UpdateError::Shape {
                what,
                message: "missing field body".into(),
            }),
        }
    }

    fn startup_decision(
        &mut self,
        pending_present: bool,
        applied_update_id: &str,
        candidate_update_id: &str,
    ) -> Result<Decision, UpdateError> {
        let callable = self.updater_startup.clone();
        let value = self.pure(
            UPDATER_RSS,
            &callable,
            &[
                Value::Bool(pending_present),
                Value::string(applied_update_id),
                Value::string(candidate_update_id),
            ],
        )?;
        decode_decision(&value)
    }

    /// The payload archive prefix for one update id, derived by the trusted
    /// business module (`gh_prefix`), never by the native installer.
    fn prefix_for(&mut self, update_id: &str) -> Result<String, UpdateError> {
        let callable = self.github_prefix.clone();
        let value = self.pure(GITHUB_RSS, &callable, &[Value::string(update_id)])?;
        match value {
            Value::String(text) => Ok((*text).clone()),
            other => Err(UpdateError::Shape {
                what: "archive prefix",
                message: format!("not a string: {other:?}"),
            }),
        }
    }

    /// Drive one script call that may wait on the buffered HTTP host operation.
    /// Only the updater VM performs network work; the await releases the
    /// LocalSet so gallery tasks keep running.
    async fn call_waiting(
        &mut self,
        script: &'static str,
        callable: &Value,
        args: &[Value],
    ) -> Result<Value, UpdateError> {
        let callable = callable.clone();
        let vm = self.vm_mut(script);
        let mut status =
            vm.start_callable(callable, args)
                .map_err(|error| UpdateError::Runtime {
                    script,
                    message: error.to_string(),
                })?;
        loop {
            match status {
                VmStatus::Halted => break,
                VmStatus::Yielded => {
                    status = vm.resume().map_err(|error| UpdateError::Runtime {
                        script,
                        message: error.to_string(),
                    })?;
                }
                VmStatus::Waiting(_) => {
                    vm.await_waiting_host_op()
                        .await
                        .map_err(|error| UpdateError::Runtime {
                            script,
                            message: error.to_string(),
                        })?;
                    status = vm.resume().map_err(|error| UpdateError::Runtime {
                        script,
                        message: error.to_string(),
                    })?;
                }
            }
        }
        vm.take_callable_result()
            .ok_or_else(|| UpdateError::Runtime {
                script,
                message: "host invocation completed without a result".into(),
            })
    }
}

fn read_script(root: &Path, relative: &'static str) -> Result<String, UpdateError> {
    let path = root.join(relative);
    std::fs::read_to_string(&path).map_err(|error| UpdateError::Io {
        path,
        message: error.to_string(),
    })
}

fn request_args(url: &str, headers: &[(String, String)]) -> [Value; 2] {
    [
        Value::string(url),
        Value::array(
            headers
                .iter()
                .map(|(name, value)| {
                    Value::array(vec![
                        Value::string(name.clone()),
                        Value::string(value.clone()),
                    ])
                })
                .collect(),
        ),
    ]
}

/// What the pre-business-App startup controller did, in terms `main` can log
/// and act on. `reexec` is present only for a native candidate whose real new
/// image was installed and must now run.
pub struct StartupReport {
    pub events: Vec<LogEvent>,
    pub reexec: Option<PathBuf>,
    /// Set when the startup could not safely continue: the caller must NOT
    /// construct the business App or touch the session.
    pub stop: Option<StartupStop>,
}

/// A fail-closed startup outcome. `Busy` is retryable (another process is
/// publishing or applying an update to this installation); `Untrusted` means
/// the installer area itself is not the private state this process needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupStop {
    Busy,
    Untrusted,
}

impl StartupReport {
    fn new(outcome: &str, events: Vec<LogEvent>) -> Self {
        let _ = outcome;
        Self {
            events,
            reexec: None,
            stop: None,
        }
    }
    fn failed(detail: impl Into<String>) -> Self {
        Self::new(
            "failed",
            vec![LogEvent {
                level: "warning".into(),
                key: "UPDATE_FAILED".into(),
                args: vec![detail.into()],
            }],
        )
    }
    fn stopped(detail: impl Into<String>, stop: StartupStop) -> Self {
        let mut report = Self::failed(detail);
        report.stop = Some(stop);
        report
    }
}

/// The startup controller, called by `main` BEFORE the business App exists.
///
/// With nothing pending there is no business decision to make, so the trusted
/// modules and the per-install lock are not loaded at all. When a pending
/// envelope exists, the OLD installation's `rss/updater/updater.rss` decides
/// apply/stale, while this function enforces the native safety: signature,
/// digest, ABI/target, entry list, complete staging and the private preflight
/// all happen BEFORE any live file is replaced. A rejected candidate is
/// discarded and the current release keeps running exactly as it was.
pub fn startup(install_root: &Path) -> StartupReport {
    let installation = install::Installation::new(install_root);
    // Never trust an existing area with a foreign owner or a shared-writable
    // mode, before reading or any write.
    if let Err(error) = installation.verify_private_area() {
        return StartupReport::stopped(error.to_string(), StartupStop::Untrusted);
    }
    // The pending SUBDIRECTORY and its leaf are validated before they are
    // probed, read or deleted. A missing subdirectory is simply "no pending"
    // and is never created here.
    let pending_present = match installation.pending_present() {
        Ok(present) => present,
        Err(error) => {
            return StartupReport::stopped(error.to_string(), StartupStop::Untrusted);
        }
    };
    if !pending_present {
        return StartupReport::new("none", Vec::new());
    }

    // The per-install write lock covers the pending read, the candidate
    // preparation and the real live replacement, so two xeHentai processes
    // never apply or publish into the same installation concurrently.
    let _lock = match install::InstallLock::acquire(&installation) {
        Ok(lock) => lock,
        Err(error @ UpdateError::Busy(_)) => {
            return StartupReport::stopped(error.to_string(), StartupStop::Busy);
        }
        Err(error) => {
            return StartupReport::stopped(error.to_string(), StartupStop::Untrusted);
        }
    };

    let mut updater = match Updater::load(install_root) {
        Ok(updater) => updater,
        Err(error) => return StartupReport::failed(error.to_string()),
    };
    let records = match installation.records() {
        Ok(records) => records,
        Err(error) => return StartupReport::failed(error.to_string()),
    };

    let envelope = match installation.read_pending() {
        Ok(Some(envelope)) => envelope,
        Ok(None) => return StartupReport::new("none", Vec::new()),
        Err(error) => {
            // A malformed candidate is retired through the same validated
            // parent/leaf path. If even that is refused (an unsafe leaf), the
            // state is left untouched and startup stops.
            if let Err(discard) = installation.discard_pending() {
                return StartupReport::stopped(
                    format!("{error}; retiring the pending candidate failed: {discard}"),
                    StartupStop::Untrusted,
                );
            }
            return StartupReport::failed(error.to_string());
        }
    };
    let descriptor = match descriptor::authenticate(
        &envelope.info,
        &envelope.sig,
        updater.binding.verification_key.as_deref(),
    ) {
        Ok(descriptor) => descriptor,
        Err(error) => {
            let _ = installation.discard_pending();
            return StartupReport::failed(error.to_string());
        }
    };
    if let Err(error) = descriptor::validate(&descriptor, &descriptor.update_id) {
        let _ = installation.discard_pending();
        return StartupReport::failed(error.to_string());
    }
    let digest = descriptor::sha256_hex(&envelope.payload);
    if digest != descriptor.archive_sha256 {
        let _ = installation.discard_pending();
        return StartupReport::failed(format!(
            "pending payload digest {digest} does not match the signed {}",
            descriptor.archive_sha256
        ));
    }

    let applied = records
        .active
        .as_ref()
        .map(|record| record.update_id.clone())
        .unwrap_or_default();
    let decision = match updater.startup_decision(true, &applied, &descriptor.update_id) {
        Ok(decision) => decision,
        Err(error) => return StartupReport::failed(error.to_string()),
    };
    match decision.outcome.as_str() {
        "stale" => {
            // Already applied; a re-downloaded identical candidate is dropped.
            let _ = installation.discard_pending();
            StartupReport::new("stale", decision.events)
        }
        "apply" => {
            let prefix = match updater.prefix_for(&descriptor.update_id) {
                Ok(prefix) => prefix,
                Err(error) => return StartupReport::failed(error.to_string()),
            };
            let candidate = install::Candidate {
                descriptor,
                payload: envelope.payload,
                prefix,
            };
            // Capture the running path BEFORE any self-replacement: after the
            // executable inode is replaced, `/proc/self/exe` reports the old
            // unlinked image (Linux appends " (deleted)") and would not be
            // executable again.
            let exe = std::env::current_exe().ok();
            match install::install_candidate(&installation, &candidate) {
                Ok(committed) => {
                    let _ = installation.discard_pending();
                    let mut report = StartupReport::new("applied", decision.events);
                    if committed.reexec {
                        report.reexec = exe;
                    }
                    report
                }
                Err(error) => {
                    // A rejection happened before any live change; a real
                    // replacement I/O error is reported as the error it is.
                    // Either way the candidate is retired and this start
                    // continues with the session and files it already has.
                    let _ = installation.discard_pending();
                    StartupReport::new(
                        "failed",
                        vec![LogEvent {
                            level: "error".into(),
                            key: "UPDATE_FAILED".into(),
                            args: vec![error.to_string()],
                        }],
                    )
                }
            }
        }
        _ => StartupReport::new("none", decision.events),
    }
}

/// Run the installer-private candidate preflight: the real executable and real
/// module loading against private state, no user configuration/session, no
/// network. Prints the marker `main`'s caller looks for.
pub fn run_preflight(candidate_root: &Path, state: &Path) -> i32 {
    let loaded = match crate::config::Config::load(state) {
        Ok(loaded) => loaded,
        Err(error) => {
            eprintln!("xeHentai: preflight configuration: {error}");
            return 1;
        }
    };
    let logger = crate::util::logger::Logger::new();
    let app = match crate::core::App::new(loaded.config, logger, state, candidate_root) {
        Ok(app) => app,
        Err(error) => {
            eprintln!("xeHentai: preflight app: {error}");
            return 1;
        }
    };
    if let Err(error) = crate::filters::Filters::load(candidate_root) {
        eprintln!("xeHentai: preflight filters: {error}");
        return 1;
    }
    if let Err(error) = crate::rpc::RpcDispatcher::load(candidate_root) {
        eprintln!("xeHentai: preflight rpc: {error}");
        return 1;
    }
    if let Err(error) = Updater::load(candidate_root) {
        eprintln!("xeHentai: preflight updater: {error}");
        return 1;
    }
    let _ = app.base_dir();
    println!(
        "{} {}",
        install::PREFLIGHT_MARKER,
        crate::r#const::version_label()
    );
    0
}

/// Convenience for the logging owner: render one RSS-selected event through
/// the existing locale catalog.
pub fn render_event(locale: crate::i18n::Locale, event: &LogEvent) -> String {
    let args: Vec<&str> = event.args.iter().map(String::as_str).collect();
    crate::i18n::render(locale, &event.key, &args)
        .unwrap_or_else(|| format!("{} {}", event.key, event.args.join(" ")))
}

/// Convenience for the logging owner: the localized UPDATE_CHANNEL line.
pub fn render_channel(locale: crate::i18n::Locale, beta: bool) -> String {
    let label_key = if beta {
        "UPDATE_DEV_CHANNEL"
    } else {
        "UPDATE_RELEASE_CHANNEL"
    };
    let label = crate::i18n::render(locale, label_key, &[]).unwrap_or_default();
    crate::i18n::render(locale, "UPDATE_CHANNEL", &[label.as_str()]).unwrap_or_default()
}
