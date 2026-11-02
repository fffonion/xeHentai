//! Configuration: one-time regex import of the legacy `config.py`, and
//! `config.rss` loading/saving through the RustScript SDK.
//!
//! `config.rss` is the only configuration file the application reads once it
//! exists. The legacy Python file is never parsed as Python and never written.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

use regex::Regex;
use vm::{
    CompileSourceFileOptions, HostApiCatalog, HostFunctionRegistry, SourceFlavor, Value, Vm,
    VmStatus, compile_source_with_flavor_and_options,
};

pub const CONFIG_FILE: &str = "config.rss";
pub const LEGACY_CONFIG_FILE: &str = "config.py";
pub const MAX_IMPORT_BYTES: usize = 1024 * 1024;

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Config {
    pub daemon: bool,
    pub dir: String,
    pub download_ori: bool,
    pub jpn_title: bool,
    pub rename_ori: bool,
    pub proxy: Vec<String>,
    pub proxy_image: bool,
    pub proxy_image_only: bool,
    pub rpc_interface: String,
    pub rpc_port: Option<i64>,
    pub rpc_secret: Option<String>,
    pub rpc_open_browser: bool,
    pub make_archive: bool,
    pub download_range: Option<Vec<[i64; 2]>>,
    pub scan_thread_cnt: i64,
    pub download_thread_cnt: i64,
    pub download_timeout: i64,
    pub ignored_errors: Vec<i64>,
    pub log_path: String,
    pub log_verbose: i64,
    pub save_tasks: bool,
    pub delete_task_files: bool,
    pub low_speed_threshold: i64,
    pub auto_update: String,
    pub update_beta_channel: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            daemon: false,
            dir: ".".into(),
            download_ori: false,
            jpn_title: true,
            rename_ori: false,
            proxy: Vec::new(),
            proxy_image: true,
            proxy_image_only: false,
            rpc_interface: "localhost".into(),
            rpc_port: None,
            rpc_secret: None,
            rpc_open_browser: true,
            make_archive: false,
            download_range: None,
            scan_thread_cnt: 1,
            download_thread_cnt: 5,
            download_timeout: 10,
            ignored_errors: Vec::new(),
            log_path: "eh.log".into(),
            log_verbose: 2,
            save_tasks: false,
            delete_task_files: false,
            low_speed_threshold: 10,
            auto_update: "download".into(),
            update_beta_channel: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    /// A complete `config.rss` was loaded.
    Rss,
    /// `config.py` was imported and a new `config.rss` was written.
    ImportedPython,
    /// No configuration existed; defaults were written to `config.rss`.
    Defaults,
}

#[derive(Clone, Debug)]
pub struct Loaded {
    pub config: Config,
    // Provenance of the loaded configuration. Kept for callers/tests that
    // inspect where the values came from; the startup no longer prints it.
    #[allow(dead_code)]
    pub origin: Origin,
    #[allow(dead_code)]
    pub path: PathBuf,
}

#[derive(Debug)]
pub enum ConfigError {
    Io {
        path: PathBuf,
        message: String,
    },
    TooLarge {
        path: PathBuf,
        limit: usize,
    },
    RootUnavailable {
        path: PathBuf,
    },
    UnknownKey {
        line: usize,
        key: String,
    },
    InvalidValue {
        line: usize,
        key: String,
        detail: String,
    },
    Unterminated {
        line: usize,
        what: &'static str,
    },
    Range {
        key: &'static str,
        value: String,
    },
    RssCompile(String),
    RssRuntime(String),
    RssDenied(String),
    RssShape(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::TooLarge { path, limit } => {
                write!(f, "{}: input exceeds {limit} bytes", path.display())
            }
            Self::RootUnavailable { path } => {
                write!(
                    f,
                    "{}: configuration root is not a writable directory",
                    path.display()
                )
            }
            Self::UnknownKey { line, key } => write!(f, "line {line}: unknown config key '{key}'"),
            Self::InvalidValue { line, key, detail } => {
                write!(f, "line {line}: invalid value for '{key}': {detail}")
            }
            Self::Unterminated { line, what } => {
                write!(f, "line {line}: unterminated {what}")
            }
            Self::Range { key, value } => write!(f, "'{key}' is out of range: {value}"),
            Self::RssCompile(message) => write!(f, "config.rss compile error: {message}"),
            Self::RssRuntime(message) => write!(f, "config.rss runtime error: {message}"),
            Self::RssDenied(message) => {
                write!(
                    f,
                    "config.rss capability denied before execution: {message}"
                )
            }
            Self::RssShape(message) => write!(f, "config.rss shape error: {message}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl ConfigError {
    fn io(path: &Path, error: std::io::Error) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
        }
    }
}

/// Test-visible read boundary. Production always uses [`DiskSource`].
pub trait ConfigSource {
    fn is_file(&self, path: &Path) -> bool;
    /// `limit` applies to the legacy import only; `config.rss` is not capped.
    fn read(&self, path: &Path, limit: Option<usize>) -> Result<String, ConfigError>;
}

pub struct DiskSource;

impl ConfigSource for DiskSource {
    fn is_file(&self, path: &Path) -> bool {
        path.is_file()
    }

    fn read(&self, path: &Path, limit: Option<usize>) -> Result<String, ConfigError> {
        read_text(path, limit)
    }
}

fn read_text(path: &Path, limit: Option<usize>) -> Result<String, ConfigError> {
    let mut file = File::open(path).map_err(|error| ConfigError::io(path, error))?;
    let mut bytes = Vec::new();
    match limit {
        Some(limit) => {
            let metadata = fs::metadata(path).map_err(|error| ConfigError::io(path, error))?;
            if metadata.len() > limit as u64 {
                return Err(ConfigError::TooLarge {
                    path: path.to_path_buf(),
                    limit,
                });
            }
            file.take(limit as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|error| ConfigError::io(path, error))?;
            if bytes.len() > limit {
                return Err(ConfigError::TooLarge {
                    path: path.to_path_buf(),
                    limit,
                });
            }
        }
        None => {
            file.read_to_end(&mut bytes)
                .map_err(|error| ConfigError::io(path, error))?;
        }
    }
    String::from_utf8(bytes).map_err(|error| ConfigError::Io {
        path: path.to_path_buf(),
        message: format!("input is not valid UTF-8: {error}"),
    })
}

/// The only host catalog configuration modules are compiled against.
///
/// A configuration module is a pure data export: it imports no host module and
/// calls no host function, so the minimal catalog advertises nothing.
fn config_catalog() -> Arc<HostApiCatalog> {
    static CATALOG: OnceLock<Arc<HostApiCatalog>> = OnceLock::new();
    Arc::clone(CATALOG.get_or_init(|| Arc::new(HostApiCatalog::default())))
}

/// The only host registry configuration modules are bound to.
///
/// [`HostFunctionRegistry::restricted`] keeps the standard surface registered
/// but grants neither builtin nor host-import capabilities; binding through
/// [`HostFunctionRegistry::bind_vm_cached`] also disables the VM's default host
/// fallback and installs that empty grant list. A configuration module can
/// therefore neither spawn a process, touch the filesystem, open a network
/// surface nor run any other host builtin.
fn config_registry() -> &'static HostFunctionRegistry {
    static REGISTRY: OnceLock<HostFunctionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(HostFunctionRegistry::restricted)
}

impl Config {
    /// Load configuration from `root`, applying the reference priority:
    /// existing `config.rss` (only), then `config.py`, then defaults.
    pub fn load(root: &Path) -> Result<Loaded, ConfigError> {
        Self::load_with_source(root, &DiskSource)
    }

    pub fn load_with_source<S: ConfigSource>(
        root: &Path,
        source: &S,
    ) -> Result<Loaded, ConfigError> {
        if !root.is_dir() {
            return Err(ConfigError::RootUnavailable {
                path: root.to_path_buf(),
            });
        }
        let rss = root.join(CONFIG_FILE);
        if source.is_file(&rss) {
            let text = source.read(&rss, None)?;
            let config = Self::decode_rss(&text)?;
            return Ok(Loaded {
                config,
                origin: Origin::Rss,
                path: rss,
            });
        }
        let legacy = root.join(LEGACY_CONFIG_FILE);
        if source.is_file(&legacy) {
            let text = source.read(&legacy, Some(MAX_IMPORT_BYTES))?;
            let imported = import_python(&text)?;
            let (config, created) = imported.create_with(root)?;
            return Ok(Loaded {
                config,
                origin: if created {
                    Origin::ImportedPython
                } else {
                    Origin::Rss
                },
                path: rss,
            });
        }
        let (config, created) = Self::default().create_with(root)?;
        Ok(Loaded {
            config,
            origin: if created {
                Origin::Defaults
            } else {
                Origin::Rss
            },
            path: rss,
        })
    }

    /// Write `config.rss` atomically. Never touches `config.py`.
    #[allow(dead_code)] // explicit save lands with the RPC/CLI config actions
    pub fn save(&self, root: &Path) -> Result<(), ConfigError> {
        if !root.is_dir() {
            return Err(ConfigError::RootUnavailable {
                path: root.to_path_buf(),
            });
        }
        let source = self.encode_rss()?;
        // Refuse to publish a module the pinned SDK cannot decode.
        Self::decode_rss(&source)?;
        let target = root.join(CONFIG_FILE);
        let stage = write_stage(root, &source)?;
        if let Err(error) = fs::rename(&stage, &target) {
            let _ = fs::remove_file(&stage);
            return Err(ConfigError::io(&target, error));
        }
        Ok(())
    }

    /// Atomically create `config.rss` if it does not exist.
    ///
    /// If another complete, valid `config.rss` appeared concurrently, that
    /// file wins and is returned unchanged.
    #[allow(dead_code)] // first-migration path uses `create_with` directly
    pub fn create_if_absent(&self, root: &Path) -> Result<Config, ConfigError> {
        self.create_with(root).map(|(config, _)| config)
    }

    fn create_with(&self, root: &Path) -> Result<(Config, bool), ConfigError> {
        if !root.is_dir() {
            return Err(ConfigError::RootUnavailable {
                path: root.to_path_buf(),
            });
        }
        let source = self.encode_rss()?;
        Self::decode_rss(&source)?;
        let target = root.join(CONFIG_FILE);
        let stage = write_stage(root, &source)?;
        match fs::hard_link(&stage, &target) {
            Ok(()) => {
                let _ = fs::remove_file(&stage);
                Ok((self.clone(), true))
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&stage);
                let text = read_text(&target, None)?;
                let config = Self::decode_rss(&text)?;
                Ok((config, false))
            }
            Err(error) => {
                let _ = fs::remove_file(&stage);
                Err(ConfigError::io(&target, error))
            }
        }
    }

    /// Encode this configuration as a RustScript module exporting `Config`.
    pub fn encode_rss(&self) -> Result<String, ConfigError> {
        validate(self)?;
        let mut out = String::new();
        out.push_str(
            "// Generated by xeHentai. config.rss is the configuration source of truth.\n",
        );
        out.push_str(
            "// The legacy config.py is imported only once; edit this file afterwards.\n\n",
        );
        out.push_str("struct Config {\n");
        let fields: [(&str, &str); 25] = [
            ("daemon", "bool"),
            ("dir", "string"),
            ("download_ori", "bool"),
            ("jpn_title", "bool"),
            ("rename_ori", "bool"),
            ("proxy", "[string]"),
            ("proxy_image", "bool"),
            ("proxy_image_only", "bool"),
            ("rpc_interface", "string"),
            ("rpc_port", "int?"),
            ("rpc_secret", "string?"),
            ("rpc_open_browser", "bool"),
            ("make_archive", "bool"),
            ("download_range", "[[int]]?"),
            ("scan_thread_cnt", "int"),
            ("download_thread_cnt", "int"),
            ("download_timeout", "int"),
            ("ignored_errors", "[int]"),
            ("log_path", "string"),
            ("log_verbose", "int"),
            ("save_tasks", "bool"),
            ("delete_task_files", "bool"),
            ("low_speed_threshold", "int"),
            ("auto_update", "string"),
            ("update_beta_channel", "bool"),
        ];
        for (name, ty) in fields {
            out.push_str(&format!("    {name}: {ty},\n"));
        }
        out.push_str("}\n\nfn config() -> Config {\n    {\n");
        out.push_str(&format!("        daemon: {},\n", self.daemon));
        out.push_str(&format!("        dir: {},\n", rss_string(&self.dir)?));
        out.push_str(&format!("        download_ori: {},\n", self.download_ori));
        out.push_str(&format!("        jpn_title: {},\n", self.jpn_title));
        out.push_str(&format!("        rename_ori: {},\n", self.rename_ori));
        out.push_str(&format!(
            "        proxy: {},\n",
            rss_string_array(&self.proxy)?
        ));
        out.push_str(&format!("        proxy_image: {},\n", self.proxy_image));
        out.push_str(&format!(
            "        proxy_image_only: {},\n",
            self.proxy_image_only
        ));
        out.push_str(&format!(
            "        rpc_interface: {},\n",
            rss_string(&self.rpc_interface)?
        ));
        out.push_str(&format!(
            "        rpc_port: {},\n",
            self.rpc_port
                .map_or_else(|| "null".into(), |value| value.to_string())
        ));
        out.push_str(&format!(
            "        rpc_secret: {},\n",
            match &self.rpc_secret {
                Some(value) => rss_string(value)?,
                None => "null".into(),
            }
        ));
        out.push_str(&format!(
            "        rpc_open_browser: {},\n",
            self.rpc_open_browser
        ));
        out.push_str(&format!("        make_archive: {},\n", self.make_archive));
        out.push_str(&format!(
            "        download_range: {},\n",
            match &self.download_range {
                Some(rows) => format!(
                    "[{}]",
                    rows.iter()
                        .map(|row| format!("[{}, {}]", row[0], row[1]))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                None => "null".into(),
            }
        ));
        out.push_str(&format!(
            "        scan_thread_cnt: {},\n",
            self.scan_thread_cnt
        ));
        out.push_str(&format!(
            "        download_thread_cnt: {},\n",
            self.download_thread_cnt
        ));
        out.push_str(&format!(
            "        download_timeout: {},\n",
            self.download_timeout
        ));
        out.push_str(&format!(
            "        ignored_errors: [{}],\n",
            self.ignored_errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
        out.push_str(&format!(
            "        log_path: {},\n",
            rss_string(&self.log_path)?
        ));
        out.push_str(&format!("        log_verbose: {},\n", self.log_verbose));
        out.push_str(&format!("        save_tasks: {},\n", self.save_tasks));
        out.push_str(&format!(
            "        delete_task_files: {},\n",
            self.delete_task_files
        ));
        out.push_str(&format!(
            "        low_speed_threshold: {},\n",
            self.low_speed_threshold
        ));
        out.push_str(&format!(
            "        auto_update: {},\n",
            rss_string(&self.auto_update)?
        ));
        out.push_str(&format!(
            "        update_beta_channel: {},\n",
            self.update_beta_channel
        ));
        out.push_str("    }\n}\n\nconfig();\n");
        Ok(out)
    }

    /// Compile and execute a configuration module through the pinned SDK.
    ///
    /// Compilation and binding use one minimal, explicitly restricted host
    /// surface. Binding happens before execution, so an ungranted capability is
    /// denied without any process, filesystem or network side effect, and the
    /// default host fallback is never installed.
    pub fn decode_rss(source: &str) -> Result<Config, ConfigError> {
        let options = CompileSourceFileOptions::new().with_host_api_catalog(config_catalog());
        let compiled =
            compile_source_with_flavor_and_options(source, SourceFlavor::RustScript, options)
                .map_err(|error| ConfigError::RssCompile(error.to_string()))?;
        let mut vm = Vm::new(compiled.program);
        config_registry()
            .bind_vm_cached(&mut vm)
            .map_err(|error| ConfigError::RssDenied(error.to_string()))?;
        let status = vm
            .run()
            .map_err(|error| ConfigError::RssRuntime(error.to_string()))?;
        if status != VmStatus::Halted {
            return Err(ConfigError::RssRuntime(format!(
                "configuration module ended with {status:?}"
            )));
        }
        let value = vm
            .stack()
            .last()
            .cloned()
            .ok_or_else(|| ConfigError::RssShape("module produced no value".into()))?;
        extract_config(&value)
    }
}

fn validate(config: &Config) -> Result<(), ConfigError> {
    let range = |key: &'static str, value: String| ConfigError::Range { key, value };
    if let Some(port) = config.rpc_port
        && !(0..=65535).contains(&port)
    {
        return Err(range("rpc_port", port.to_string()));
    }
    if config.scan_thread_cnt < 1 {
        return Err(range("scan_thread_cnt", config.scan_thread_cnt.to_string()));
    }
    if config.download_thread_cnt < 1 {
        return Err(range(
            "download_thread_cnt",
            config.download_thread_cnt.to_string(),
        ));
    }
    if config.download_timeout < 1 {
        return Err(range(
            "download_timeout",
            config.download_timeout.to_string(),
        ));
    }
    if config.low_speed_threshold < 0 {
        return Err(range(
            "low_speed_threshold",
            config.low_speed_threshold.to_string(),
        ));
    }
    Ok(())
}

fn rss_string(value: &str) -> Result<String, ConfigError> {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            ch if ch.is_control() => {
                return Err(ConfigError::InvalidValue {
                    line: 0,
                    key: "string".into(),
                    detail: format!("unsupported control character U+{:04X}", ch as u32),
                });
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    Ok(out)
}

fn rss_string_array(values: &[String]) -> Result<String, ConfigError> {
    let mut items = Vec::with_capacity(values.len());
    for value in values {
        items.push(rss_string(value)?);
    }
    Ok(format!("[{}]", items.join(", ")))
}

fn write_stage(root: &Path, source: &str) -> Result<PathBuf, ConfigError> {
    let counter = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
    let stage = root.join(format!(
        ".{CONFIG_FILE}.stage-{}-{counter}",
        std::process::id()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&stage)
        .map_err(|error| ConfigError::io(&stage, error))?;
    let result = file
        .write_all(source.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(error) = result {
        let _ = fs::remove_file(&stage);
        return Err(ConfigError::io(&stage, error));
    }
    Ok(stage)
}

fn field<'a>(map: &'a vm::VmMap, name: &str) -> Option<&'a Value> {
    map.get(&Value::string(name))
}

fn required<'a>(map: &'a vm::VmMap, name: &str) -> Result<&'a Value, ConfigError> {
    field(map, name).ok_or_else(|| ConfigError::RssShape(format!("missing field '{name}'")))
}

fn shape(name: &str, expected: &str, actual: &Value) -> ConfigError {
    ConfigError::RssShape(format!(
        "field '{name}' must be {expected}, found {actual:?}"
    ))
}

fn get_bool(map: &vm::VmMap, name: &str) -> Result<bool, ConfigError> {
    match required(map, name)? {
        Value::Bool(value) => Ok(*value),
        other => Err(shape(name, "bool", other)),
    }
}

fn get_int(map: &vm::VmMap, name: &str) -> Result<i64, ConfigError> {
    match required(map, name)? {
        Value::Int(value) => Ok(*value),
        other => Err(shape(name, "int", other)),
    }
}

fn get_string(map: &vm::VmMap, name: &str) -> Result<String, ConfigError> {
    match required(map, name)? {
        Value::String(value) => Ok((**value).clone()),
        other => Err(shape(name, "string", other)),
    }
}

fn get_optional_int(map: &vm::VmMap, name: &str) -> Result<Option<i64>, ConfigError> {
    match field(map, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Int(value)) => Ok(Some(*value)),
        Some(other) => Err(shape(name, "int?", other)),
    }
}

fn get_optional_string(map: &vm::VmMap, name: &str) -> Result<Option<String>, ConfigError> {
    match field(map, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some((**value).clone())),
        Some(other) => Err(shape(name, "string?", other)),
    }
}

fn get_string_array(map: &vm::VmMap, name: &str) -> Result<Vec<String>, ConfigError> {
    match required(map, name)? {
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, item)| match item {
                Value::String(value) => Ok((**value).clone()),
                other => Err(ConfigError::RssShape(format!(
                    "field '{name}[{index}]' must be string, found {other:?}"
                ))),
            })
            .collect(),
        other => Err(shape(name, "[string]", other)),
    }
}

fn get_int_array(map: &vm::VmMap, name: &str) -> Result<Vec<i64>, ConfigError> {
    match required(map, name)? {
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, item)| match item {
                Value::Int(value) => Ok(*value),
                other => Err(ConfigError::RssShape(format!(
                    "field '{name}[{index}]' must be int, found {other:?}"
                ))),
            })
            .collect(),
        other => Err(shape(name, "[int]", other)),
    }
}

fn get_download_range(map: &vm::VmMap, name: &str) -> Result<Option<Vec<[i64; 2]>>, ConfigError> {
    match field(map, name) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(rows)) => {
            let mut out = Vec::with_capacity(rows.len());
            for (index, row) in rows.iter().enumerate() {
                let Value::Array(pair) = row else {
                    return Err(ConfigError::RssShape(format!(
                        "field '{name}[{index}]' must be a pair, found {row:?}"
                    )));
                };
                if pair.len() != 2 {
                    return Err(ConfigError::RssShape(format!(
                        "field '{name}[{index}]' must have exactly two elements"
                    )));
                }
                let (Value::Int(start), Value::Int(end)) = (&pair[0], &pair[1]) else {
                    return Err(ConfigError::RssShape(format!(
                        "field '{name}[{index}]' must contain two ints"
                    )));
                };
                out.push([*start, *end]);
            }
            Ok(Some(out))
        }
        Some(other) => Err(shape(name, "[[int]]?", other)),
    }
}

const FIELD_NAMES: [&str; 25] = [
    "daemon",
    "dir",
    "download_ori",
    "jpn_title",
    "rename_ori",
    "proxy",
    "proxy_image",
    "proxy_image_only",
    "rpc_interface",
    "rpc_port",
    "rpc_secret",
    "rpc_open_browser",
    "make_archive",
    "download_range",
    "scan_thread_cnt",
    "download_thread_cnt",
    "download_timeout",
    "ignored_errors",
    "log_path",
    "log_verbose",
    "save_tasks",
    "delete_task_files",
    "low_speed_threshold",
    "auto_update",
    "update_beta_channel",
];

fn extract_config(value: &Value) -> Result<Config, ConfigError> {
    let Value::Map(map) = value else {
        return Err(ConfigError::RssShape(format!(
            "configuration module must return a Config struct, found {value:?}"
        )));
    };
    for (key, _) in map.iter() {
        let Value::String(name) = key else {
            return Err(ConfigError::RssShape(format!(
                "configuration struct has a non-string key {key:?}"
            )));
        };
        if !FIELD_NAMES.contains(&name.as_str()) {
            return Err(ConfigError::RssShape(format!(
                "configuration struct has unknown field '{name}'"
            )));
        }
    }
    let config = Config {
        daemon: get_bool(map, "daemon")?,
        dir: get_string(map, "dir")?,
        download_ori: get_bool(map, "download_ori")?,
        jpn_title: get_bool(map, "jpn_title")?,
        rename_ori: get_bool(map, "rename_ori")?,
        proxy: get_string_array(map, "proxy")?,
        proxy_image: get_bool(map, "proxy_image")?,
        proxy_image_only: get_bool(map, "proxy_image_only")?,
        rpc_interface: get_string(map, "rpc_interface")?,
        rpc_port: get_optional_int(map, "rpc_port")?,
        rpc_secret: get_optional_string(map, "rpc_secret")?,
        rpc_open_browser: get_bool(map, "rpc_open_browser")?,
        make_archive: get_bool(map, "make_archive")?,
        download_range: get_download_range(map, "download_range")?,
        scan_thread_cnt: get_int(map, "scan_thread_cnt")?,
        download_thread_cnt: get_int(map, "download_thread_cnt")?,
        download_timeout: get_int(map, "download_timeout")?,
        ignored_errors: get_int_array(map, "ignored_errors")?,
        log_path: get_string(map, "log_path")?,
        log_verbose: get_int(map, "log_verbose")?,
        save_tasks: get_bool(map, "save_tasks")?,
        delete_task_files: get_bool(map, "delete_task_files")?,
        low_speed_threshold: get_int(map, "low_speed_threshold")?,
        auto_update: get_string(map, "auto_update")?,
        update_beta_channel: get_bool(map, "update_beta_channel")?,
    };
    validate(&config)?;
    Ok(config)
}

/// Import the supported subset of a legacy `config.py` over built-in defaults.
pub fn import_python(source: &str) -> Result<Config, ConfigError> {
    if source.len() > MAX_IMPORT_BYTES {
        return Err(ConfigError::TooLarge {
            path: PathBuf::from(LEGACY_CONFIG_FILE),
            limit: MAX_IMPORT_BYTES,
        });
    }
    let mut config = Config::default();
    for statement in logical_statements(source)? {
        apply_statement(&mut config, &statement)?;
    }
    validate(&config)?;
    Ok(config)
}

struct Statement {
    text: String,
    line: usize,
}

fn logical_statements(source: &str) -> Result<Vec<Statement>, ConfigError> {
    let mut statements = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    let mut comment = false;
    let mut depth: i32 = 0;
    let mut line = 1usize;
    let mut start_line = 1usize;
    let mut chars = source.chars().peekable();

    while let Some(ch) = chars.next() {
        if comment {
            if ch == '\n' {
                comment = false;
                line += 1;
                if depth == 0 {
                    finish_statement(&mut statements, &mut current, start_line);
                } else {
                    current.push('\n');
                }
            }
            continue;
        }
        if let Some(current_quote) = quote {
            match ch {
                '\n' => {
                    return Err(ConfigError::Unterminated {
                        line: start_line,
                        what: "string",
                    });
                }
                '\\' => {
                    let Some(escaped) = chars.next() else {
                        return Err(ConfigError::Unterminated {
                            line: start_line,
                            what: "string",
                        });
                    };
                    current.push(ch);
                    if escaped == '\n' {
                        return Err(ConfigError::Unterminated {
                            line: start_line,
                            what: "string",
                        });
                    }
                    current.push(escaped);
                }
                ch if ch == current_quote => {
                    quote = None;
                    current.push(ch);
                }
                ch => current.push(ch),
            }
            continue;
        }
        match ch {
            '\r' => {}
            '\n' => {
                line += 1;
                if depth > 0 {
                    current.push('\n');
                } else {
                    finish_statement(&mut statements, &mut current, start_line);
                }
            }
            '#' => comment = true,
            '"' | '\'' => {
                if current.trim().is_empty() {
                    start_line = line;
                }
                quote = Some(ch);
                current.push(ch);
            }
            '[' => {
                if current.trim().is_empty() {
                    start_line = line;
                }
                depth += 1;
                current.push(ch);
            }
            ']' => {
                depth -= 1;
                if depth < 0 {
                    return Err(ConfigError::InvalidValue {
                        line,
                        key: "<statement>".into(),
                        detail: "unexpected ']'".into(),
                    });
                }
                current.push(ch);
            }
            ch => {
                if current.trim().is_empty() && !ch.is_whitespace() {
                    start_line = line;
                }
                current.push(ch);
            }
        }
    }
    if quote.is_some() {
        return Err(ConfigError::Unterminated {
            line: start_line,
            what: "string",
        });
    }
    if depth != 0 {
        return Err(ConfigError::Unterminated {
            line: start_line,
            what: "array",
        });
    }
    finish_statement(&mut statements, &mut current, start_line);
    Ok(statements)
}

fn finish_statement(statements: &mut Vec<Statement>, current: &mut String, start_line: usize) {
    let text = current.trim().to_string();
    current.clear();
    if !text.is_empty() {
        statements.push(Statement {
            text,
            line: start_line,
        });
    }
}

fn apply_statement(config: &mut Config, statement: &Statement) -> Result<(), ConfigError> {
    static ASSIGN: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*([\s\S]+)$").expect("assignment regex")
    });
    let Some(captures) = ASSIGN.captures(&statement.text) else {
        return Err(ConfigError::InvalidValue {
            line: statement.line,
            key: "<statement>".into(),
            detail: "expected a simple 'key = value' assignment".into(),
        });
    };
    let key = captures[1].to_string();
    let raw = captures[2].trim_end();
    let line = statement.line;

    match key.as_str() {
        "daemon" => config.daemon = parse_bool(raw, line, &key)?,
        "dir" => config.dir = parse_string(raw, line, &key)?,
        "download_ori" => config.download_ori = parse_bool(raw, line, &key)?,
        "jpn_title" => config.jpn_title = parse_bool(raw, line, &key)?,
        "rename_ori" => config.rename_ori = parse_bool(raw, line, &key)?,
        "proxy" => config.proxy = parse_string_array(raw, line, &key)?,
        "proxy_image" => config.proxy_image = parse_bool(raw, line, &key)?,
        "proxy_image_only" => config.proxy_image_only = parse_bool(raw, line, &key)?,
        "rpc_interface" => config.rpc_interface = parse_string(raw, line, &key)?,
        "rpc_port" => config.rpc_port = Some(parse_int(raw, line, &key)?),
        "rpc_secret" => config.rpc_secret = Some(parse_string(raw, line, &key)?),
        "rpc_open_browser" => config.rpc_open_browser = parse_bool(raw, line, &key)?,
        "make_archive" => config.make_archive = parse_bool(raw, line, &key)?,
        "download_range" => config.download_range = Some(parse_range(raw, line, &key)?),
        "scan_thread_cnt" => config.scan_thread_cnt = parse_int(raw, line, &key)?,
        "download_thread_cnt" => config.download_thread_cnt = parse_int(raw, line, &key)?,
        "download_timeout" => config.download_timeout = parse_int(raw, line, &key)?,
        "ignored_errors" => config.ignored_errors = parse_int_array(raw, line, &key)?,
        "log_path" => config.log_path = parse_string(raw, line, &key)?,
        "log_verbose" => config.log_verbose = parse_int(raw, line, &key)?,
        "save_tasks" => config.save_tasks = parse_bool(raw, line, &key)?,
        "delete_task_files" => config.delete_task_files = parse_bool(raw, line, &key)?,
        "low_speed_threshold" => config.low_speed_threshold = parse_int(raw, line, &key)?,
        "auto_update" => config.auto_update = parse_string(raw, line, &key)?,
        "update_beta_channel" => config.update_beta_channel = parse_bool(raw, line, &key)?,
        _ => {
            return Err(ConfigError::UnknownKey { line, key });
        }
    }
    Ok(())
}

fn parse_bool(raw: &str, line: usize, key: &str) -> Result<bool, ConfigError> {
    match raw {
        "True" | "true" => Ok(true),
        "False" | "false" => Ok(false),
        _ => Err(ConfigError::InvalidValue {
            line,
            key: key.into(),
            detail: "expected True/False or true/false".into(),
        }),
    }
}

fn parse_int(raw: &str, line: usize, key: &str) -> Result<i64, ConfigError> {
    static INTEGER: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^[+-]?[0-9]+$").expect("integer regex"));
    if !INTEGER.is_match(raw) {
        return Err(ConfigError::InvalidValue {
            line,
            key: key.into(),
            detail: "expected a decimal integer".into(),
        });
    }
    raw.parse::<i64>()
        .map_err(|error| ConfigError::InvalidValue {
            line,
            key: key.into(),
            detail: format!("integer out of range: {error}"),
        })
}

fn parse_string(raw: &str, line: usize, key: &str) -> Result<String, ConfigError> {
    let invalid = |detail: &str| ConfigError::InvalidValue {
        line,
        key: key.into(),
        detail: detail.into(),
    };
    let mut chars = raw.chars();
    let Some(quote @ ('"' | '\'')) = chars.next() else {
        return Err(invalid("expected a quoted string"));
    };
    let mut out = String::new();
    loop {
        let Some(ch) = chars.next() else {
            return Err(invalid("unterminated string"));
        };
        if ch == quote {
            if chars.next().is_some() {
                return Err(invalid("trailing content after string"));
            }
            return Ok(out);
        }
        if ch == '\\' {
            let Some(escaped) = chars.next() else {
                return Err(invalid("unterminated escape"));
            };
            match escaped {
                'n' => out.push('\n'),
                'r' => out.push('\r'),
                't' => out.push('\t'),
                '\\' => out.push('\\'),
                '\'' => out.push('\''),
                '"' => out.push('"'),
                other => {
                    return Err(invalid(&format!("unsupported escape '\\{other}'")));
                }
            }
        } else if ch == '\n' || ch == '\r' {
            return Err(invalid("newline in string literal"));
        } else {
            out.push(ch);
        }
    }
}

fn parse_array<'a>(raw: &'a str, line: usize, key: &str) -> Result<Vec<&'a str>, ConfigError> {
    let invalid = |detail: &str| ConfigError::InvalidValue {
        line,
        key: key.into(),
        detail: detail.into(),
    };
    let trimmed = raw.trim();
    let Some(inner) = trimmed
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
    else {
        return Err(invalid("expected an array"));
    };
    let inner = inner.trim();
    if inner.is_empty() {
        return Ok(Vec::new());
    }
    let mut items = Vec::new();
    let mut start = 0usize;
    let mut depth = 0i32;
    let mut quote: Option<char> = None;
    let mut escaped = false;
    let mut saw_comma = false;
    for (index, ch) in inner.char_indices() {
        if let Some(current) = quote {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == current {
                quote = None;
            }
            continue;
        }
        match ch {
            '"' | '\'' => quote = Some(ch),
            '[' => depth += 1,
            ']' => {
                depth -= 1;
                if depth < 0 {
                    return Err(invalid("unbalanced array"));
                }
            }
            ',' if depth == 0 => {
                items.push(inner[start..index].trim());
                start = index + 1;
                saw_comma = true;
            }
            _ => {}
        }
    }
    if quote.is_some() || depth != 0 {
        return Err(invalid("unbalanced array"));
    }
    let tail = inner[start..].trim();
    if tail.is_empty() {
        if !saw_comma {
            return Err(invalid("empty array element"));
        }
    } else {
        items.push(tail);
    }
    if items.iter().any(|item| item.is_empty()) {
        return Err(invalid("empty array element"));
    }
    Ok(items)
}

fn parse_string_array(raw: &str, line: usize, key: &str) -> Result<Vec<String>, ConfigError> {
    let items = parse_array(raw, line, key)?;
    items
        .into_iter()
        .map(|item| parse_string(item, line, key))
        .collect()
}

fn parse_int_array(raw: &str, line: usize, key: &str) -> Result<Vec<i64>, ConfigError> {
    let items = parse_array(raw, line, key)?;
    items
        .into_iter()
        .map(|item| parse_int(item, line, key))
        .collect()
}

fn parse_range(raw: &str, line: usize, key: &str) -> Result<Vec<[i64; 2]>, ConfigError> {
    let rows = parse_array(raw, line, key)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        let pair = parse_array(row, line, key)?;
        if pair.len() != 2 {
            return Err(ConfigError::InvalidValue {
                line,
                key: key.into(),
                detail: "download_range rows must be [start, end] pairs".into(),
            });
        }
        out.push([
            parse_int(pair[0], line, key)?,
            parse_int(pair[1], line, key)?,
        ]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Barrier};

    struct Recording<'a> {
        inner: DiskSource,
        opened: RefCell<Vec<PathBuf>>,
        marker: std::marker::PhantomData<&'a ()>,
    }

    impl<'a> Recording<'a> {
        fn new() -> Self {
            Self {
                inner: DiskSource,
                opened: RefCell::new(Vec::new()),
                marker: std::marker::PhantomData,
            }
        }
    }

    impl ConfigSource for Recording<'_> {
        fn is_file(&self, path: &Path) -> bool {
            self.inner.is_file(path)
        }

        fn read(&self, path: &Path, limit: Option<usize>) -> Result<String, ConfigError> {
            self.opened.borrow_mut().push(path.to_path_buf());
            self.inner.read(path, limit)
        }
    }

    fn root() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp root")
    }

    fn value_map(config: &Config) -> BTreeMap<&'static str, String> {
        let mut map = BTreeMap::new();
        map.insert("daemon", config.daemon.to_string());
        map.insert("dir", config.dir.clone());
        map.insert("download_ori", config.download_ori.to_string());
        map.insert("jpn_title", config.jpn_title.to_string());
        map.insert("rename_ori", config.rename_ori.to_string());
        map.insert("proxy", format!("{:?}", config.proxy));
        map.insert("proxy_image", config.proxy_image.to_string());
        map.insert("proxy_image_only", config.proxy_image_only.to_string());
        map.insert("rpc_interface", config.rpc_interface.clone());
        map.insert("rpc_port", format!("{:?}", config.rpc_port));
        map.insert("rpc_secret", format!("{:?}", config.rpc_secret));
        map.insert("rpc_open_browser", config.rpc_open_browser.to_string());
        map.insert("make_archive", config.make_archive.to_string());
        map.insert("download_range", format!("{:?}", config.download_range));
        map.insert("scan_thread_cnt", config.scan_thread_cnt.to_string());
        map.insert(
            "download_thread_cnt",
            config.download_thread_cnt.to_string(),
        );
        map.insert("download_timeout", config.download_timeout.to_string());
        map.insert("ignored_errors", format!("{:?}", config.ignored_errors));
        map.insert("log_path", config.log_path.clone());
        map.insert("log_verbose", config.log_verbose.to_string());
        map.insert("save_tasks", config.save_tasks.to_string());
        map.insert("delete_task_files", config.delete_task_files.to_string());
        map.insert(
            "low_speed_threshold",
            config.low_speed_threshold.to_string(),
        );
        map.insert("auto_update", config.auto_update.clone());
        map.insert(
            "update_beta_channel",
            config.update_beta_channel.to_string(),
        );
        map
    }

    #[test]
    fn defaults_match_the_reference_configuration() {
        let config = Config::default();
        let expected: BTreeMap<&str, &str> = [
            ("daemon", "false"),
            ("dir", "."),
            ("download_ori", "false"),
            ("jpn_title", "true"),
            ("rename_ori", "false"),
            ("proxy", "[]"),
            ("proxy_image", "true"),
            ("proxy_image_only", "false"),
            ("rpc_interface", "localhost"),
            ("rpc_port", "None"),
            ("rpc_secret", "None"),
            ("rpc_open_browser", "true"),
            ("make_archive", "false"),
            ("download_range", "None"),
            ("scan_thread_cnt", "1"),
            ("download_thread_cnt", "5"),
            ("download_timeout", "10"),
            ("ignored_errors", "[]"),
            ("log_path", "eh.log"),
            ("log_verbose", "2"),
            ("save_tasks", "false"),
            ("delete_task_files", "false"),
            ("low_speed_threshold", "10"),
            ("auto_update", "download"),
            ("update_beta_channel", "false"),
        ]
        .into_iter()
        .collect();
        let actual = value_map(&config);
        assert_eq!(actual.len(), 25);
        let actual: BTreeMap<&str, &str> = actual
            .iter()
            .map(|(key, value)| (*key, value.as_str()))
            .collect();
        assert_eq!(actual, expected);
    }

    #[test]
    fn python_import_overrides_only_named_fields() {
        let source = r#"
# a comment
dir = "downloads"
download_thread_cnt = 5
download_ori = False
proxy = ["socks5://127.0.0.1:1080"]
ignored_errors = [1006, 1004]
download_range = [[1, 5], [8, 8]]
rpc_interface = '127.0.0.1'
rpc_port = 8080
rpc_secret = "se\"cret"
# rpc_port is not set here when commented
log_verbose = 3
"#;
        let config = import_python(source).expect("import");
        assert_eq!(config.dir, "downloads");
        assert_eq!(config.download_thread_cnt, 5);
        assert!(!config.download_ori);
        assert_eq!(config.proxy, vec!["socks5://127.0.0.1:1080"]);
        assert_eq!(config.ignored_errors, vec![1006, 1004]);
        assert_eq!(config.download_range, Some(vec![[1, 5], [8, 8]]));
        assert_eq!(config.rpc_interface, "127.0.0.1");
        assert_eq!(config.rpc_port, Some(8080));
        assert_eq!(config.rpc_secret.as_deref(), Some("se\"cret"));
        assert_eq!(config.log_verbose, 3);
        assert_eq!(config.scan_thread_cnt, 1);
    }

    #[test]
    fn python_import_handles_quotes_comments_crlf_and_multiline_arrays() {
        let source = concat!(
            "# comment with = and [\r\n",
            "dir = \"a#b=c\"  # trailing comment\r\n",
            "log_path = 'C:\\\\temp\\\\eh.log'\r\n",
            "proxy = [\r\n",
            "  \"http://one\",  # first\r\n",
            "  'http://two',\r\n",
            "]\r\n",
            "rpc_secret = \"line1\\nline2\\tend\"\r\n",
            "dir = \"last\"\r\n",
        );
        let config = import_python(source).expect("import");
        assert_eq!(config.dir, "last");
        assert_eq!(config.log_path, "C:\\temp\\eh.log");
        assert_eq!(config.proxy, vec!["http://one", "http://two"]);
        assert_eq!(config.rpc_secret.as_deref(), Some("line1\nline2\tend"));
    }

    #[test]
    fn python_import_allows_empty_values_and_trailing_commas() {
        let source = "proxy = []\nignored_errors = [1, 2,]\nlog_path = \"\"\n";
        let config = import_python(source).expect("import");
        assert!(config.proxy.is_empty());
        assert_eq!(config.ignored_errors, vec![1, 2]);
        assert_eq!(config.log_path, "");
    }

    #[test]
    fn python_import_rejects_everything_outside_the_subset() {
        let cases = [
            ("foo = 1\n", "unknown"),
            ("rpc_port = None\n", "invalid"),
            ("download_range = None\n", "invalid"),
            ("log_verbose = 1.5\n", "invalid"),
            ("proxy = (\"a\", \"b\")\n", "invalid"),
            ("proxy = {\"a\": 1}\n", "invalid"),
            ("dir = os.getcwd()\n", "invalid"),
            ("scan_thread_cnt = 1 + 2\n", "invalid"),
            ("scan_thread_cnt = 999999999999999999999999\n", "invalid"),
            ("scan_thread_cnt = 1\nnot python\n", "line"),
            ("proxy = [\"a\", 1]\n", "invalid"),
            ("proxy = [\"a\",\n", "unterminated"),
            ("dir = \"unterminated\n", "unterminated"),
            ("scan_thread_cnt = 0\n", "range"),
            ("rpc_port = 70000\n", "range"),
        ];
        for (source, needle) in cases {
            let error = import_python(source).expect_err(source);
            let text = error.to_string();
            assert!(
                text.contains(needle),
                "source {source:?} produced {text:?}, wanted {needle:?}"
            );
        }
    }

    #[test]
    fn rss_reads_are_not_limited_by_the_import_budget() {
        let root = root();
        let mut source = String::new();
        while source.len() <= MAX_IMPORT_BYTES + 16 {
            source.push_str("// padding line for the import budget\n");
        }
        source.push_str(&Config::default().encode_rss().unwrap());
        fs::write(root.path().join(CONFIG_FILE), source).unwrap();
        let loaded = Config::load(root.path()).expect("large config.rss");
        assert_eq!(loaded.config, Config::default());
    }

    #[test]
    fn python_import_enforces_the_input_budget() {
        let source = format!("dir = \"{}\"\n", "x".repeat(MAX_IMPORT_BYTES));
        let error = import_python(&source).expect_err("oversized input");
        assert!(matches!(error, ConfigError::TooLarge { .. }), "{error}");
    }

    #[test]
    fn rss_encoding_round_trips_every_field_without_losing_false_zero_or_empty() {
        let config = Config {
            daemon: true,
            dir: "a\"b\\c\nd\r".into(),
            download_ori: true,
            jpn_title: false,
            rename_ori: false,
            proxy: vec!["http://x:1".into(), "".into()],
            proxy_image: false,
            proxy_image_only: true,
            rpc_interface: "0.0.0.0".into(),
            rpc_port: Some(0),
            rpc_secret: Some("sec\tret".into()),
            rpc_open_browser: false,
            make_archive: true,
            download_range: Some(vec![[1, 1], [3, 7]]),
            scan_thread_cnt: 3,
            download_thread_cnt: 9,
            download_timeout: 60,
            ignored_errors: vec![0, -1, 1006],
            log_path: "".into(),
            log_verbose: 0,
            save_tasks: true,
            delete_task_files: true,
            low_speed_threshold: 0,
            auto_update: "off".into(),
            update_beta_channel: true,
        };
        let source = config.encode_rss().expect("encode");
        assert!(source.contains("config();"));
        let decoded = Config::decode_rss(&source).expect("decode");
        assert_eq!(decoded, config);

        let empty = Config::default();
        let decoded = Config::decode_rss(&empty.encode_rss().expect("encode")).expect("decode");
        assert_eq!(decoded, empty);
    }

    #[test]
    fn rss_decoding_reports_compile_runtime_and_shape_errors() {
        let compile_error = Config::decode_rss("fn broken( {").expect_err("compile");
        assert!(
            matches!(compile_error, ConfigError::RssCompile(_)),
            "{compile_error}"
        );

        let runtime_error = Config::decode_rss("1 / 0;").expect_err("runtime");
        assert!(
            matches!(runtime_error, ConfigError::RssRuntime(_)),
            "{runtime_error}"
        );

        let shape_source = "fn main() -> int { 1 }\nmain();\n";
        let shape_error = Config::decode_rss(shape_source).expect_err("shape");
        assert!(
            matches!(shape_error, ConfigError::RssShape(_)),
            "{shape_error}"
        );
    }

    #[test]
    fn load_prefers_rss_and_never_opens_python() {
        let root = root();
        let config = Config {
            dir: "from-rss".into(),
            ..Config::default()
        };
        fs::write(root.path().join(CONFIG_FILE), config.encode_rss().unwrap()).unwrap();
        let legacy = root.path().join(LEGACY_CONFIG_FILE);
        fs::write(&legacy, "dir = \"from-python\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&legacy, fs::Permissions::from_mode(0o000)).unwrap();
        }

        let source = Recording::new();
        let loaded = Config::load_with_source(root.path(), &source).expect("load");
        assert_eq!(loaded.origin, Origin::Rss);
        assert_eq!(loaded.config.dir, "from-rss");
        assert_eq!(
            source.opened.borrow().as_slice(),
            [root.path().join(CONFIG_FILE)]
        );
    }

    #[test]
    fn broken_rss_does_not_fall_back_to_valid_python() {
        let root = root();
        fs::write(root.path().join(CONFIG_FILE), "not a module").unwrap();
        fs::write(root.path().join(LEGACY_CONFIG_FILE), "dir = \"python\"\n").unwrap();
        let source = Recording::new();
        let error = Config::load_with_source(root.path(), &source).expect_err("must fail");
        assert!(matches!(error, ConfigError::RssCompile(_)), "{error}");
        assert_eq!(
            source.opened.borrow().as_slice(),
            [root.path().join(CONFIG_FILE)]
        );
    }

    #[test]
    fn first_import_writes_rss_and_leaves_python_byte_identical() {
        let root = root();
        let python = "dir = \"downloads\"\ndownload_ori = True\nproxy = [\"socks5://x:1\"]\n";
        let legacy = root.path().join(LEGACY_CONFIG_FILE);
        fs::write(&legacy, python).unwrap();

        let loaded = Config::load(root.path()).expect("import");
        assert_eq!(loaded.origin, Origin::ImportedPython);
        assert_eq!(loaded.config.dir, "downloads");
        assert!(loaded.config.download_ori);
        assert_eq!(fs::read(&legacy).unwrap(), python.as_bytes());

        let rss = fs::read_to_string(root.path().join(CONFIG_FILE)).unwrap();
        assert_eq!(Config::decode_rss(&rss).unwrap(), loaded.config);

        fs::write(&legacy, "this is no longer python").unwrap();
        let again = Config::load(root.path()).expect("rss load");
        assert_eq!(again.origin, Origin::Rss);
        assert_eq!(again.config, loaded.config);
    }

    #[test]
    fn missing_configuration_writes_defaults_once() {
        let root = root();
        let first = Config::load(root.path()).expect("defaults");
        assert_eq!(first.origin, Origin::Defaults);
        assert_eq!(first.config, Config::default());
        assert!(root.path().join(CONFIG_FILE).is_file());
        let second = Config::load(root.path()).expect("rss");
        assert_eq!(second.origin, Origin::Rss);
        assert_eq!(second.config, Config::default());
    }

    #[test]
    fn failed_import_leaves_no_partial_rss() {
        let root = root();
        fs::write(root.path().join(LEGACY_CONFIG_FILE), "unknown = 1\n").unwrap();
        let error = Config::load(root.path()).expect_err("import must fail");
        assert!(matches!(error, ConfigError::UnknownKey { .. }), "{error}");
        assert!(!root.path().join(CONFIG_FILE).exists());
    }

    #[test]
    fn save_replaces_only_rss_and_reloads_identically() {
        let root = root();
        let legacy = root.path().join(LEGACY_CONFIG_FILE);
        fs::write(&legacy, "dir = \"python\"\n").unwrap();
        let mut config = Config {
            dir: "saved".into(),
            rpc_port: Some(9999),
            ..Config::default()
        };
        config.save(root.path()).expect("save");
        assert_eq!(fs::read_to_string(&legacy).unwrap(), "dir = \"python\"\n");
        assert_eq!(Config::load(root.path()).expect("load").config, config);

        config.dir = "saved-again".into();
        config.save(root.path()).expect("save again");
        assert_eq!(
            Config::load(root.path()).expect("load").config.dir,
            "saved-again"
        );
        let entries: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(entries.len(), 2, "no stage files remain: {entries:?}");
    }

    #[test]
    fn failed_save_preserves_the_previous_rss() {
        let root = root();
        let config = Config {
            dir: "first".into(),
            ..Config::default()
        };
        config.save(root.path()).expect("save");
        let before = fs::read(root.path().join(CONFIG_FILE)).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o555)).unwrap();
        }
        let mut changed = config.clone();
        changed.dir = "second".into();
        let error = changed.save(root.path()).expect_err("read-only root");
        assert!(matches!(error, ConfigError::Io { .. }), "{error}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(fs::read(root.path().join(CONFIG_FILE)).unwrap(), before);
        assert_eq!(Config::load(root.path()).unwrap().config.dir, "first");
    }

    #[test]
    fn creating_a_new_rss_does_not_clobber_an_existing_valid_one() {
        let root = root();
        let existing = Config {
            dir: "existing".into(),
            ..Config::default()
        };
        existing.save(root.path()).expect("seed");
        let before = fs::read(root.path().join(CONFIG_FILE)).unwrap();

        let candidate = Config {
            dir: "candidate".into(),
            ..Config::default()
        };
        let selected = candidate.create_if_absent(root.path()).expect("create");
        assert_eq!(selected.dir, "existing");
        assert_eq!(fs::read(root.path().join(CONFIG_FILE)).unwrap(), before);
    }

    #[test]
    fn root_must_exist_and_be_a_directory() {
        let missing = tempfile::tempdir().unwrap().path().join("missing");
        let error = Config::load(&missing).expect_err("missing root");
        assert!(
            matches!(error, ConfigError::RootUnavailable { .. }),
            "{error}"
        );
        let file = tempfile::NamedTempFile::new().unwrap();
        let error = Config::load(file.path()).expect_err("file root");
        assert!(
            matches!(error, ConfigError::RootUnavailable { .. }),
            "{error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn unwritable_root_cannot_create_defaults() {
        use std::os::unix::fs::PermissionsExt;
        let root = root();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let error = Config::load(root.path()).expect_err("unwritable root");
        assert!(matches!(error, ConfigError::Io { .. }), "{error}");
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(!root.path().join(CONFIG_FILE).exists());
    }

    #[test]
    fn encoding_rejects_unrepresentable_control_characters() {
        let config = Config {
            log_path: "bad\u{1}path".into(),
            ..Config::default()
        };
        let error = config.encode_rss().expect_err("control character");
        assert!(matches!(error, ConfigError::InvalidValue { .. }), "{error}");
    }

    /// A structurally valid configuration export with one injected statement.
    ///
    /// The export is byte-for-byte what [`Config::save`] publishes, so a
    /// rejection cannot be explained by a malformed configuration shape.
    fn hostile_rss(import: &str, statement: &str) -> String {
        let encoded = Config {
            dir: "hostile".into(),
            ..Config::default()
        }
        .encode_rss()
        .expect("encode hostile baseline");
        let injected = encoded.replace("\nconfig();\n", &format!("\n{statement}\nconfig();\n"));
        assert!(injected.contains(statement), "injection point missing");
        if import.is_empty() {
            injected
        } else {
            format!("{import}\n{injected}")
        }
    }

    /// The refusal must name the capability the module asked for, so a syntax
    /// or shape failure cannot be mistaken for an authority denial.
    fn refused_capability(result: Result<Config, ConfigError>, needle: &str) {
        let error = result.expect_err("hostile configuration must be refused");
        assert!(
            matches!(error, ConfigError::RssDenied(_)),
            "refusal must be an authority denial, got {error}"
        );
        let text = error.to_string();
        assert!(
            text.contains(needle),
            "refusal must name {needle:?}: {text}"
        );
    }

    /// Waits for a worker thread marker; a spawn that happened slightly after
    /// the error still counts as a violated boundary.
    fn marker_shows_up(path: &Path) -> bool {
        for _ in 0..100 {
            if path.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        false
    }

    #[test]
    fn config_module_cannot_spawn_a_process() {
        let root = root();
        let marker = root.path().join("process-marker");
        let source = hostile_rss(
            "use io;",
            &format!(
                "let child = io::popen(\"printf spawned > '{}'\", \"r\");\nchild;",
                marker.display()
            ),
        );
        let result = Config::decode_rss(&source);
        assert!(
            !marker_shows_up(&marker),
            "configuration module spawned a process"
        );
        refused_capability(result, "io_popen");
    }

    #[test]
    fn config_module_cannot_write_files() {
        let root = root();
        let marker = root.path().join("written-by-config");
        let source = hostile_rss(
            "use io;",
            &format!(
                "let handle = io::open(\"{}\", \"w\");\nhandle;",
                marker.display()
            ),
        );
        let result = Config::decode_rss(&source);
        assert!(
            !marker_shows_up(&marker),
            "configuration module wrote a file"
        );
        refused_capability(result, "io_open");
    }

    #[test]
    fn config_module_cannot_call_standard_host_functions() {
        let source = hostile_rss("", "println(\"config said hello\");");
        let result = Config::decode_rss(&source);
        let error = result.expect_err("standard host functions must be refused");
        assert!(
            matches!(error, ConfigError::RssDenied(_)),
            "refusal must be an authority denial: {error}"
        );
        assert!(error.to_string().contains("print"), "{error}");
    }

    #[test]
    fn config_module_cannot_block_on_host_sleep() {
        let source = hostile_rss("use runtime;", "runtime::sleep(1200);");
        let started = std::time::Instant::now();
        let result = Config::decode_rss(&source);
        let elapsed = started.elapsed();
        assert!(
            elapsed < std::time::Duration::from_millis(900),
            "configuration executed a blocking host function for {elapsed:?}"
        );
        refused_capability(result, "runtime::sleep");
    }

    #[test]
    fn config_module_cannot_reach_the_network() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("loopback fixture");
        listener.set_nonblocking(true).expect("nonblocking fixture");
        let port = listener.local_addr().expect("fixture address").port();
        let source = hostile_rss(
            "use http;",
            &format!("http::get(\"http://127.0.0.1:{port}/config\");"),
        );
        let error = Config::decode_rss(&source).expect_err("network configuration was accepted");
        assert!(
            matches!(error, ConfigError::RssDenied(_))
                || matches!(error, ConfigError::RssRuntime(_)),
            "{error}"
        );
        assert!(
            error.to_string().contains("http"),
            "refusal must name the network surface: {error}"
        );
        let mut contacts = 0usize;
        for _ in 0..50 {
            match listener.accept() {
                Ok(_) => contacts += 1,
                Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => break,
            }
        }
        assert_eq!(contacts, 0, "configuration opened a loopback connection");
    }

    #[test]
    fn hostile_rss_never_reads_python_and_keeps_the_previous_configuration() {
        let root = root();
        let baseline = Config {
            dir: "baseline".into(),
            ..Config::default()
        };
        baseline.save(root.path()).expect("baseline save");
        let loaded = Config::load(root.path()).expect("baseline load");
        assert_eq!(loaded.config, baseline);

        let marker = root.path().join("loader-process-marker");
        let hostile = hostile_rss(
            "use io;",
            &format!(
                "let child = io::popen(\"printf spawned > '{}'\", \"r\");\nchild;",
                marker.display()
            ),
        );
        fs::write(root.path().join(CONFIG_FILE), &hostile).expect("hostile rss");
        fs::write(root.path().join(LEGACY_CONFIG_FILE), "dir = \"python\"\n").expect("python");
        let before = fs::read(root.path().join(CONFIG_FILE)).expect("hostile bytes");

        let source = Recording::new();
        let error =
            Config::load_with_source(root.path(), &source).expect_err("hostile load must fail");
        assert!(
            matches!(error, ConfigError::RssDenied(_)),
            "loader must refuse the ungranted capability: {error}"
        );
        assert_eq!(
            source.opened.borrow().as_slice(),
            [root.path().join(CONFIG_FILE)],
            "a refused config.rss must never fall back to reading config.py"
        );
        assert_eq!(
            fs::read(root.path().join(CONFIG_FILE)).unwrap(),
            before,
            "a refused load must not rewrite config.rss"
        );
        assert_eq!(
            loaded.config, baseline,
            "a refused load must not replace the previous configuration"
        );
        assert!(!marker_shows_up(&marker), "loader spawned a process");
    }

    #[test]
    fn first_create_revalidates_an_existing_hostile_rss_without_side_effects() {
        let root = root();
        let marker = root.path().join("create-process-marker");
        let hostile = hostile_rss(
            "use io;",
            &format!(
                "let child = io::popen(\"printf spawned > '{}'\", \"r\");\nchild;",
                marker.display()
            ),
        );
        fs::write(root.path().join(CONFIG_FILE), &hostile).expect("hostile rss");
        let before = fs::read(root.path().join(CONFIG_FILE)).expect("hostile bytes");
        let candidate = Config {
            dir: "candidate".into(),
            ..Config::default()
        };
        let error = candidate
            .create_if_absent(root.path())
            .expect_err("an existing hostile rss must fail revalidation");
        assert!(
            matches!(error, ConfigError::RssDenied(_)),
            "revalidation must refuse the ungranted capability: {error}"
        );
        assert_eq!(
            fs::read(root.path().join(CONFIG_FILE)).unwrap(),
            before,
            "revalidation must leave the existing bytes untouched"
        );
        assert!(!marker_shows_up(&marker), "validation spawned a process");
    }

    #[test]
    fn concurrent_first_import_keeps_one_complete_valid_rss() {
        let root = Arc::new(root());
        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();
        for dir in ["thread-a", "thread-b"] {
            let root = Arc::clone(&root);
            let barrier = Arc::clone(&barrier);
            handles.push(std::thread::spawn(move || {
                let config = Config {
                    dir: dir.into(),
                    ..Config::default()
                };
                barrier.wait();
                config.create_if_absent(root.path()).expect("create")
            }));
        }
        let selected: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect();
        let on_disk = Config::load(root.path()).expect("load").config;
        assert!(
            selected.iter().all(|config| config.dir == on_disk.dir),
            "selected {selected:?} but disk has {}",
            on_disk.dir
        );
        assert!(["thread-a", "thread-b"].contains(&on_disk.dir.as_str()));
        assert_eq!(
            fs::read_dir(root.path()).unwrap().count(),
            1,
            "only config.rss exists"
        );
    }
}
