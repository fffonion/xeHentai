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
//! The owned background task, cancellation and logger sink belong to the
//! existing `main`/`run_supervised` owner (see `src/main.rs`). This scope is
//! the CHECK path only: no download, pending file, package or apply decision
//! exists here, so a `download` run is reported as an explicit unsupported
//! stage rather than a completed update.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use vm::{
    BuiltinFunction, CapabilityProfile, CompileSourceFileOptions, HostFunctionRegistry, HttpConfig,
    HttpHostExt, SourceFlavor, Value, Vm, VmStatus, compile_source_with_flavor_and_options,
};

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

#[derive(Debug)]
pub enum UpdateError {
    Io {
        path: PathBuf,
        message: String,
    },
    Compile {
        script: &'static str,
        message: String,
    },
    Bind {
        script: &'static str,
        message: String,
    },
    Runtime {
        script: &'static str,
        message: String,
    },
    Shape {
        what: &'static str,
        message: String,
    },
    Binding(String),
}

impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Compile { script, message } => write!(f, "{script} compile error: {message}"),
            Self::Bind { script, message } => write!(f, "{script} host binding error: {message}"),
            Self::Runtime { script, message } => write!(f, "{script} runtime error: {message}"),
            Self::Shape { what, message } => write!(f, "{what} shape error: {message}"),
            Self::Binding(message) => write!(f, "update source binding error: {message}"),
        }
    }
}

impl std::error::Error for UpdateError {}

/// Named update metadata (`xeHentai/updater/__init__.py` `UpdateInfo`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateInfo {
    pub update_id: String,
    pub download_link: String,
    pub ts: String,
    pub message: String,
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
/// also the trusted HTTP allowlist: one exact scheme/host/port.
#[derive(Clone, Debug)]
pub struct SourceBinding {
    pub api_origin: String,
    pub repo: String,
    pub stable_channel: String,
    pub beta_channel: Option<String>,
    pub package_download_origin: String,
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
    updater: Vm,
    updater_fetch: Value,
    updater_mode: Value,
    updater_decide: Value,
    updater_failure: Value,
    binding: SourceBinding,
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
fn http_origin(origin: &str, what: &'static str) -> Result<(HttpConfig, String, u16), UpdateError> {
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
    Ok((
        HttpConfig {
            allowed_schemes: vec![scheme],
            allowed_hosts: vec![host.clone()],
            allowed_ports: vec![port],
            allow_private_ips: loopback,
            ..HttpConfig::default()
        },
        host,
        port,
    ))
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
        let binding = decode_binding(
            &github
                .invoke_callable(github_binding.clone(), &[])
                .map_err(|error| UpdateError::Runtime {
                    script: GITHUB_RSS,
                    message: error.to_string(),
                })?,
        )?;

        let (http, _, _) = http_origin(&binding.api_origin, "api")?;
        let _ = http_origin(&binding.package_download_origin, "package download")?;
        if binding.beta_channel.as_deref().is_some_and(str::is_empty) {
            return Err(UpdateError::Binding(
                "beta channel must be absent or non-empty".into(),
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
        let updater_decide = callable(&updater, UPDATER_RSS, "upd_decide")?;
        let updater_failure = callable(&updater, UPDATER_RSS, "upd_failure")?;
        let updater_mode = callable(&updater, UPDATER_RSS, "upd_mode")?;

        Ok(Self {
            github,
            github_plan,
            github_parse,
            updater,
            updater_fetch,
            updater_mode,
            updater_decide,
            updater_failure,
            binding,
        })
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
        Ok(CheckReport {
            outcome: decision.outcome,
            update: Some(info),
            events: decision.events,
        })
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
