//! The application object and session handling from `xeHentai/core.py`.
//!
//! `App` owns the real task registry, the ready queue, cookies and headers.
//! It keeps the reference dict/queue order and the reference return codes; the
//! network login, proxy pool and RPC server arrive in later milestones and are
//! not pretended here.
#![allow(dead_code)]

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use serde_json::{Map as JsonMap, Value};

use crate::config::Config;
use crate::r#const as consts;
use crate::task::{Task, TaskConfig, TaskError, TaskLoadError};
use crate::task::{boundary_path, normalize, path_within, resolve_dir};
use crate::util;
use crate::util::logger::Logger;

pub const SESSION_FILE: &str = "h.json";
pub const LEGACY_COOKIE_FILE: &str = ".ehentai.cookie";
/// Explicit read budget for the session file. Larger inputs are refused with
/// an error and the current state stays untouched; nothing is truncated.
pub const MAX_SESSION_BYTES: u64 = 8 * 1024 * 1024;

static SESSION_STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

pub trait GuidSource: Send {
    fn next_guid(&mut self) -> String;
}

/// Production identifier: the first eight characters of a UUID4, as in
/// `str(uuid.uuid4())[:8]`.
pub struct UuidGuids;

impl GuidSource for UuidGuids {
    fn next_guid(&mut self) -> String {
        uuid::Uuid::new_v4().to_string().chars().take(8).collect()
    }
}

/// Deterministic identifiers for tests and the compatibility probe.
pub struct FixedGuids {
    values: VecDeque<String>,
}

impl FixedGuids {
    pub fn new<I: IntoIterator<Item = String>>(values: I) -> Self {
        Self {
            values: values.into_iter().collect(),
        }
    }
}

impl GuidSource for FixedGuids {
    fn next_guid(&mut self) -> String {
        self.values
            .pop_front()
            .expect("scripted guid sequence exhausted")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Header {
    pub name: String,
    pub value: Option<String>,
}

#[derive(Debug)]
pub enum AppError {
    Task(TaskError),
    UnknownConfig(String),
    Unavailable(&'static str),
    Session(SessionError),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Task(error) => write!(f, "{error}"),
            Self::UnknownConfig(key) => write!(f, "unknown global configuration key {key}"),
            Self::Unavailable(what) => write!(f, "not implemented in this milestone: {what}"),
            Self::Session(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for AppError {}

impl From<TaskError> for AppError {
    fn from(error: TaskError) -> Self {
        Self::Task(error)
    }
}

impl From<SessionError> for AppError {
    fn from(error: SessionError) -> Self {
        Self::Session(error)
    }
}

#[derive(Debug)]
pub enum SessionError {
    Io { path: PathBuf, message: String },
    Json(String),
    TooLarge { path: PathBuf, limit: u64 },
    MissingKey(&'static str),
    Shape(String),
    Task(TaskLoadError),
    Scan(TaskError),
    Unsafe { path: PathBuf, reason: &'static str },
}

impl SessionError {
    /// A malformed session body is the one error the reference catches and
    /// reports as a warning while continuing to start.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, Self::Json(_))
    }
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, message } => write!(f, "{}: {message}", path.display()),
            Self::Json(message) => write!(f, "invalid session JSON: {message}"),
            Self::TooLarge { path, limit } => {
                write!(
                    f,
                    "{} exceeds the {limit} byte session budget",
                    path.display()
                )
            }
            Self::MissingKey(key) => write!(f, "session is missing {key}"),
            Self::Shape(message) => write!(f, "invalid session: {message}"),
            Self::Task(error) => write!(f, "{error}"),
            Self::Scan(error) => write!(f, "{error}"),
            Self::Unsafe { path, reason } => {
                write!(f, "unsafe session path {}: {reason}", path.display())
            }
        }
    }
}

impl std::error::Error for SessionError {}

pub struct App {
    pub cfg: Config,
    pub logger: Logger,
    tasks: VecDeque<String>,
    last_task_guid: Option<String>,
    all_tasks: IndexMap<String, Task>,
    cookies: Vec<(String, String)>,
    headers: Vec<Header>,
    has_login: bool,
    exit: i64,
    base_dir: PathBuf,
    filepath: PathBuf,
    authorized_dir: PathBuf,
    /// Canonical roots a restored task directory may live under: the user's
    /// base/config root plus roots explicitly chosen through config or task
    /// arguments. A value read from h.json never extends this set.
    trusted_roots: Vec<PathBuf>,
    guid_source: Box<dyn GuidSource>,
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("App")
            .field("tasks", &self.tasks)
            .field("all_tasks", &self.all_tasks.keys().collect::<Vec<_>>())
            .field("cookies", &self.cookies)
            .field("headers", &self.headers)
            .field("has_login", &self.has_login)
            .field("exit", &self.exit)
            .field("base_dir", &self.base_dir)
            .field("authorized_dir", &self.authorized_dir)
            .field("trusted_roots", &self.trusted_roots)
            .finish()
    }
}

impl App {
    pub fn new(
        cfg: Config,
        logger: Logger,
        base_dir: &Path,
        filepath: &Path,
    ) -> Result<Self, AppError> {
        Self::with_guid_source(cfg, logger, base_dir, filepath, Box::new(UuidGuids))
    }

    pub fn with_guid_source(
        cfg: Config,
        mut logger: Logger,
        base_dir: &Path,
        filepath: &Path,
        guid_source: Box<dyn GuidSource>,
    ) -> Result<Self, AppError> {
        logger.set_verbosity(cfg.log_verbose);
        let authorized_dir = resolve_dir(base_dir, &cfg.dir);
        let base_root = boundary_path(base_dir).unwrap_or_else(|_| normalize(base_dir));
        let mut app = Self {
            cfg,
            logger,
            tasks: VecDeque::new(),
            last_task_guid: None,
            all_tasks: IndexMap::new(),
            cookies: vec![("nw".to_string(), "1".to_string())],
            headers: vec![
                Header {
                    name: "User-Agent".into(),
                    value: util::make_ua(),
                },
                Header {
                    name: "Accept-Charset".into(),
                    value: Some("utf-8;q=0.7,*;q=0.7".into()),
                },
                Header {
                    name: "Accept".into(),
                    value: Some(
                        "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8".into(),
                    ),
                },
                Header {
                    name: "Connection".into(),
                    value: Some("keep-alive".into()),
                },
            ],
            has_login: false,
            exit: consts::XEH_STATE_RUNNING,
            base_dir: base_dir.to_path_buf(),
            filepath: filepath.to_path_buf(),
            authorized_dir,
            trusted_roots: vec![base_root],
            guid_source,
        };
        // The configured download root is an explicit user choice, so it may
        // be an external absolute directory. A directory read from h.json is
        // not granted this way.
        let configured_root = app.authorized_dir.clone();
        app.trust_dir(&configured_root);
        match app.load_session() {
            Ok(()) => Ok(app),
            Err(error) if error.is_recoverable() => {
                let message = crate::i18n::render(
                    crate::i18n::Locale::EnUs,
                    "SESSION_LOAD_EXCEPTION",
                    &[&error.to_string()],
                )
                .unwrap_or_else(|| error.to_string());
                let _ = app.logger.warning(&message);
                Ok(app)
            }
            Err(error) => Err(AppError::Session(error)),
        }
    }

    pub fn task(&self, guid: &str) -> Option<&Task> {
        self.all_tasks.get(guid)
    }

    pub fn task_mut(&mut self, guid: &str) -> Option<&mut Task> {
        self.all_tasks.get_mut(guid)
    }

    pub fn all_task_guids(&self) -> Vec<String> {
        self.all_tasks.keys().cloned().collect()
    }

    pub fn all_tasks_len(&self) -> usize {
        self.all_tasks.len()
    }

    pub fn queue_len(&self) -> usize {
        self.tasks.len()
    }

    pub fn queue(&self) -> &VecDeque<String> {
        &self.tasks
    }

    pub fn last_task_guid(&self) -> Option<&str> {
        self.last_task_guid.as_deref()
    }

    pub fn exit_state(&self) -> i64 {
        self.exit
    }

    pub fn cookies(&self) -> &[(String, String)] {
        &self.cookies
    }

    pub fn cookie(&self, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }

    pub fn headers(&self) -> &[Header] {
        &self.headers
    }

    pub fn cookie_header(&self) -> Option<&str> {
        self.headers
            .iter()
            .find(|header| header.name == "Cookie")
            .and_then(|header| header.value.as_deref())
    }

    pub fn has_login(&self) -> bool {
        self.has_login
    }

    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    pub fn filepath(&self) -> &Path {
        &self.filepath
    }

    pub fn authorized_dir(&self) -> &Path {
        &self.authorized_dir
    }

    /// Grant a download root that an explicit user entry point selected
    /// (global config or task arguments). Never called with session values.
    fn trust_dir(&mut self, dir: &Path) {
        let Ok(root) = boundary_path(dir) else {
            return;
        };
        if self
            .trusted_roots
            .iter()
            .any(|trusted| path_within(trusted, &root))
        {
            return;
        }
        self.trusted_roots.push(root);
    }

    /// True when a task directory, after resolving symlinks on its existing
    /// ancestors, stays below one of the trusted roots. This is a component
    /// check, not equality with the current global download dir.
    fn task_dir_is_authorized(&self, dir: &Path) -> Result<bool, TaskError> {
        let candidate = boundary_path(dir)?;
        Ok(self
            .trusted_roots
            .iter()
            .any(|root| path_within(root, &candidate)))
    }

    pub fn logger_mut(&mut self) -> &mut Logger {
        &mut self.logger
    }

    fn set_cookie_header(&mut self) {
        let value = util::make_cookie(&self.cookies);
        self.upsert_header("Cookie", Some(value));
    }

    fn upsert_header(&mut self, name: &str, value: Option<String>) {
        if let Some(header) = self.headers.iter_mut().find(|header| header.name == name) {
            header.value = value;
        } else {
            self.headers.push(Header {
                name: name.to_string(),
                value,
            });
        }
    }

    fn update_login_flag(&mut self) {
        if self.cookie("ipb_member_id").is_some() && self.cookie("ipb_pass_hash").is_some() {
            self.has_login = true;
        }
    }

    pub fn set_cookie(&mut self, cookie: &str) -> (i64, Option<String>) {
        for (key, value) in util::parse_cookie(cookie) {
            if let Some(existing) = self.cookies.iter_mut().find(|(name, _)| *name == key) {
                existing.1 = value;
            } else {
                self.cookies.push((key, value));
            }
        }
        self.set_cookie_header();
        self.update_login_flag();
        (consts::ERR_NO_ERROR, None)
    }

    /// Internal configuration update used by task snapshot tests. It is not
    /// the public RPC `updateConfig` (that handler arrives with the RPC
    /// milestone), and it refuses proxy/RPC side effects instead of faking
    /// them.
    pub fn update_config(&mut self, patch: JsonMap<String, Value>) -> Result<(), AppError> {
        if let Some(Value::Array(proxy)) = patch.get("proxy")
            && !proxy.is_empty()
        {
            return Err(AppError::Unavailable("proxy pool"));
        }
        if let Some(value) = patch.get("rpc_port")
            && !value.is_null()
            && crate::task::value_truthy(value)
        {
            return Err(AppError::Unavailable("RPC server"));
        }
        let mut new_ignored: Option<Vec<i64>> = None;
        for (key, value) in &patch {
            match key.as_str() {
                "daemon" => self.cfg.daemon = as_bool(key, value)?,
                "dir" => self.cfg.dir = as_string(key, value)?,
                "download_ori" => self.cfg.download_ori = as_bool(key, value)?,
                "jpn_title" => self.cfg.jpn_title = as_bool(key, value)?,
                "rename_ori" => self.cfg.rename_ori = as_bool(key, value)?,
                "proxy" => {
                    let items = value.as_array().ok_or_else(|| {
                        AppError::UnknownConfig(format!("{key} must be an array"))
                    })?;
                    if !items.is_empty() {
                        return Err(AppError::Unavailable("proxy pool"));
                    }
                    self.cfg.proxy = Vec::new();
                }
                "proxy_image" => self.cfg.proxy_image = as_bool(key, value)?,
                "proxy_image_only" => self.cfg.proxy_image_only = as_bool(key, value)?,
                "rpc_interface" => self.cfg.rpc_interface = as_string(key, value)?,
                "rpc_port" => {
                    self.cfg.rpc_port = match value {
                        Value::Null => None,
                        _ => Some(as_int(key, value)?),
                    }
                }
                "rpc_secret" => {
                    self.cfg.rpc_secret = match value {
                        Value::Null => None,
                        _ => Some(as_string(key, value)?),
                    }
                }
                "rpc_open_browser" => self.cfg.rpc_open_browser = as_bool(key, value)?,
                "make_archive" => self.cfg.make_archive = as_bool(key, value)?,
                "download_range" => {
                    self.cfg.download_range = match value {
                        Value::Null => None,
                        _ => Some(as_range(key, value)?),
                    }
                }
                "scan_thread_cnt" => self.cfg.scan_thread_cnt = as_int(key, value)?,
                "download_thread_cnt" => self.cfg.download_thread_cnt = as_int(key, value)?,
                "download_timeout" => self.cfg.download_timeout = as_int(key, value)?,
                "ignored_errors" => {
                    let items = as_int_array(key, value)?;
                    if !items.is_empty() {
                        let mut merged = self.cfg.ignored_errors.clone();
                        for item in items {
                            if !merged.contains(&item) {
                                merged.push(item);
                            }
                        }
                        new_ignored = Some(merged);
                    }
                }
                "log_path" => self.cfg.log_path = as_string(key, value)?,
                "log_verbose" => self.cfg.log_verbose = as_int(key, value)?,
                "save_tasks" => self.cfg.save_tasks = as_bool(key, value)?,
                "delete_task_files" => self.cfg.delete_task_files = as_bool(key, value)?,
                "low_speed_threshold" => self.cfg.low_speed_threshold = as_int(key, value)?,
                "auto_update" => self.cfg.auto_update = as_string(key, value)?,
                "update_beta_channel" => self.cfg.update_beta_channel = as_bool(key, value)?,
                other => return Err(AppError::UnknownConfig(other.to_string())),
            }
        }
        if let Some(merged) = new_ignored {
            self.cfg.ignored_errors = merged;
        }
        self.logger.set_verbosity(self.cfg.log_verbose);
        if let Some(Value::String(dir)) = patch.get("dir") {
            let authorized = resolve_dir(&self.base_dir, dir);
            self.authorized_dir = authorized.clone();
            self.trust_dir(&authorized);
        }
        if let Some(dir) = patch.get("dir").and_then(Value::as_str)
            && !dir.is_empty()
            && !self.authorized_dir.exists()
            && let Err(error) = fs::create_dir_all(&self.authorized_dir)
        {
            let display = self.authorized_dir.to_string_lossy().into_owned();
            let message = crate::i18n::render(
                crate::i18n::Locale::EnUs,
                "ERR_CANNOT_CREATE_DIR",
                &[&display],
            )
            .unwrap_or_else(|| error.to_string());
            let _ = self.logger.error(&message);
        }
        let log_path = Path::new(&self.cfg.log_path);
        let log_path = if log_path.is_absolute() {
            log_path.to_path_buf()
        } else {
            self.base_dir.join(log_path)
        };
        if let Err(error) = self.logger.set_logfile(&log_path) {
            let _ = self.logger.warning(&format!(
                "cannot open log file {}: {error}",
                log_path.display()
            ));
        }
        Ok(())
    }

    pub fn add_task(
        &mut self,
        url: &str,
        kwargs: JsonMap<String, Value>,
    ) -> Result<(i64, Option<String>), AppError> {
        let url = url.trim();
        let config = TaskConfig::from_app(&self.cfg, &kwargs)?;
        if config.download_ori && !self.has_login {
            let message = crate::i18n::render(
                crate::i18n::Locale::EnUs,
                "XEH_DOWNLOAD_ORI_NEED_LOGIN",
                &[],
            )
            .unwrap_or_default();
            let _ = self.logger.warning(&message);
        }
        let guid = self.guid_source.next_guid();
        let task_root = resolve_dir(&self.base_dir, &config.dir);
        let task = Task::new(url, config, guid.clone(), &self.base_dir)?;
        // A task argument is an explicit user choice and keeps its directory
        // usable for the lifetime of this App, even if the global dir changes.
        self.trust_dir(&task_root);
        if let Some(existing) = self.all_tasks.get_mut(&guid) {
            if existing.state == consts::TASK_STATE_FINISHED
                || existing.state == consts::TASK_STATE_FAILED
            {
                existing.state = consts::TASK_STATE_WAITING;
                existing.cleanup(false)?;
            }
            return Ok((consts::ERR_NO_ERROR, Some(guid)));
        }
        self.all_tasks.insert(guid.clone(), task);
        if !valid_task_url(url) {
            let task = self.all_tasks.get_mut(&guid).expect("just inserted");
            task.set_fail(consts::ERR_URL_NOT_RECOGNIZED);
            self.log_task_error(&guid, consts::ERR_URL_NOT_RECOGNIZED);
            return Ok((consts::ERR_URL_NOT_RECOGNIZED, None));
        }
        if !self.has_login && exhentai_url(url) {
            let task = self.all_tasks.get_mut(&guid).expect("just inserted");
            task.set_fail(consts::ERR_CANT_DOWNLOAD_EXH);
            self.log_task_error(&guid, consts::ERR_CANT_DOWNLOAD_EXH);
            return Ok((consts::ERR_CANT_DOWNLOAD_EXH, None));
        }
        self.tasks.push_back(guid.clone());
        Ok((consts::ERR_NO_ERROR, Some(guid)))
    }

    fn log_task_error(&mut self, guid: &str, code: i64) {
        let message = crate::i18n::err_msg(crate::i18n::Locale::EnUs, code, &[]);
        let text = crate::i18n::render(crate::i18n::Locale::EnUs, "TASK_ERROR", &[guid, &message])
            .unwrap_or_else(|| format!("task #{guid} error: {message}"));
        let _ = self.logger.error(&text);
    }

    pub fn del_task(&mut self, guid: &str) -> Result<(i64, Option<String>), AppError> {
        if !self.all_tasks.contains_key(guid) {
            return Ok((consts::ERR_TASK_NOT_FOUND, None));
        }
        let state = self.all_tasks.get(guid).expect("checked").state;
        if consts::TASK_STATE_PAUSED < state && state < consts::TASK_STATE_FINISHED {
            return Ok((consts::ERR_DELETE_RUNNING_TASK, None));
        }
        self.all_tasks
            .get_mut(guid)
            .expect("checked")
            .cleanup(true)?;
        self.all_tasks.shift_remove(guid);
        Ok((consts::ERR_NO_ERROR, Some(String::new())))
    }

    pub fn pause_task(&mut self, guid: &str) -> Result<(i64, Option<String>), AppError> {
        let Some(task) = self.all_tasks.get_mut(guid) else {
            return Ok((consts::ERR_TASK_NOT_FOUND, None));
        };
        if matches!(
            task.state,
            consts::TASK_STATE_PAUSED | consts::TASK_STATE_FINISHED | consts::TASK_STATE_FAILED
        ) {
            return Ok((consts::ERR_TASK_CANNOT_PAUSE, None));
        }
        task.state = consts::TASK_STATE_PAUSED;
        Ok((consts::ERR_NO_ERROR, Some(String::new())))
    }

    pub fn resume_task(&mut self, guid: &str) -> Result<(i64, Option<String>), AppError> {
        let Some(task) = self.all_tasks.get_mut(guid) else {
            return Ok((consts::ERR_TASK_NOT_FOUND, None));
        };
        if consts::TASK_STATE_PAUSED < task.state && task.state < consts::TASK_STATE_FINISHED {
            return Ok((consts::ERR_TASK_CANNOT_RESUME, None));
        }
        task.state = task.state.max(consts::TASK_STATE_WAITING);
        self.tasks.push_back(guid.to_string());
        Ok((consts::ERR_NO_ERROR, Some(String::new())))
    }

    pub fn save_session(&mut self) -> (i64, Option<String>) {
        let document = SessionDocument {
            save_tasks: self.cfg.save_tasks,
            tasks: &self.all_tasks,
            cookies: &self.cookies,
        };
        let mut buffer = Vec::new();
        if let Err(error) = serde_json::to_writer(&mut buffer, &document) {
            let message = crate::i18n::render(
                crate::i18n::Locale::EnUs,
                "SESSION_WRITE_EXCEPTION",
                &[&error.to_string()],
            )
            .unwrap_or_else(|| error.to_string());
            let _ = self.logger.warning(&message);
            return (consts::ERR_SAVE_SESSION_FAILED, Some(error.to_string()));
        }
        match self.write_session(&buffer) {
            Ok(()) => (consts::ERR_NO_ERROR, None),
            Err(error) => {
                let message = crate::i18n::render(
                    crate::i18n::Locale::EnUs,
                    "SESSION_WRITE_EXCEPTION",
                    &[&error.to_string()],
                )
                .unwrap_or_else(|| error.to_string());
                let _ = self.logger.warning(&message);
                (consts::ERR_SAVE_SESSION_FAILED, Some(error.to_string()))
            }
        }
    }

    fn write_session(&mut self, buffer: &[u8]) -> Result<(), SessionError> {
        let path = self.base_dir.join(SESSION_FILE);
        let stage = loop {
            let counter = SESSION_STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
            let name = format!(".h.json-{}-{counter}", std::process::id());
            let candidate = self.base_dir.join(name);
            match fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    file.write_all(buffer).map_err(|error| SessionError::Io {
                        path: candidate.clone(),
                        message: error.to_string(),
                    })?;
                    file.sync_all().map_err(|error| SessionError::Io {
                        path: candidate.clone(),
                        message: error.to_string(),
                    })?;
                    break candidate;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(error) => {
                    return Err(SessionError::Io {
                        path: candidate,
                        message: error.to_string(),
                    });
                }
            }
        };
        if let Err(error) = fs::rename(&stage, &path) {
            let _ = fs::remove_file(&stage);
            return Err(SessionError::Io {
                path,
                message: error.to_string(),
            });
        }
        Ok(())
    }

    pub fn load_session(&mut self) -> Result<(), SessionError> {
        let path = self.base_dir.join(SESSION_FILE);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(SessionError::Unsafe {
                    path,
                    reason: "session file is a symlink",
                });
            }
            Ok(metadata) if metadata.len() > MAX_SESSION_BYTES => {
                return Err(SessionError::TooLarge {
                    path,
                    limit: MAX_SESSION_BYTES,
                });
            }
            Ok(_) => {
                let text = fs::read_to_string(&path).map_err(|error| {
                    if error.kind() == std::io::ErrorKind::InvalidData {
                        SessionError::Json(error.to_string())
                    } else {
                        SessionError::Io {
                            path: path.clone(),
                            message: error.to_string(),
                        }
                    }
                })?;
                let document: Value = serde_json::from_str(&text)
                    .map_err(|error| SessionError::Json(error.to_string()))?;
                let tasks = document
                    .get("tasks")
                    .ok_or(SessionError::MissingKey("tasks"))?;
                let tasks = tasks
                    .as_object()
                    .ok_or_else(|| SessionError::Shape("tasks is not an object".into()))?;
                let mut pending: Vec<Task> = Vec::new();
                for value in tasks.values() {
                    let mut task =
                        Task::from_wire(value, &self.base_dir).map_err(SessionError::Task)?;
                    let authorized = match self.task_dir_is_authorized(task.files().root()) {
                        Ok(authorized) => authorized,
                        Err(error) => {
                            let message = format!(
                                "skipping task {}: cannot verify download directory {}: {error}",
                                task.guid,
                                task.files().root().display()
                            );
                            let _ = self.logger.warning(&message);
                            continue;
                        }
                    };
                    if !authorized {
                        let message = format!(
                            "skipping task {}: download directory {} is outside the trusted session roots",
                            task.guid,
                            task.files().root().display()
                        );
                        let _ = self.logger.warning(&message);
                        continue;
                    }
                    if task.meta.contains_key("filelist") {
                        task.scan_downloaded().map_err(SessionError::Scan)?;
                    }
                    if task.state == consts::TASK_STATE_DOWNLOAD && task.page_q.is_some() {
                        task.state = consts::TASK_STATE_SCAN_IMG;
                    }
                    pending.push(task);
                }
                let cookies = document
                    .get("cookies")
                    .ok_or(SessionError::MissingKey("cookies"))?;
                let cookies = cookies
                    .as_object()
                    .ok_or_else(|| SessionError::Shape("cookies is not an object".into()))?;
                let mut new_cookies: Vec<(String, String)> = Vec::new();
                for (key, value) in cookies {
                    let value = value.as_str().ok_or_else(|| {
                        SessionError::Shape(format!("cookie {key} is not a string"))
                    })?;
                    new_cookies.push((key.clone(), value.to_string()));
                }
                for task in pending {
                    let guid = task.guid.clone();
                    self.all_tasks.insert(guid.clone(), task);
                    self.tasks.push_back(guid);
                }
                if !self.all_tasks.is_empty() {
                    let count = self.all_tasks.len().to_string();
                    if let Some(message) = crate::i18n::render(
                        crate::i18n::Locale::EnUs,
                        "XEH_LOAD_TASKS_CNT",
                        &[&count],
                    ) {
                        let _ = self.logger.info(&message);
                    }
                }
                for (key, value) in new_cookies {
                    if let Some(existing) = self.cookies.iter_mut().find(|(name, _)| *name == key) {
                        existing.1 = value;
                    } else {
                        self.cookies.push((key, value));
                    }
                }
                if !self.cookies.is_empty() {
                    self.set_cookie_header();
                    self.update_login_flag();
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(SessionError::Io {
                    path,
                    message: error.to_string(),
                });
            }
        }
        if !self.has_login {
            self.load_legacy_cookie();
        }
        Ok(())
    }

    fn load_legacy_cookie(&mut self) {
        let path = self.filepath.join(LEGACY_COOKIE_FILE);
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            return;
        };
        if metadata.file_type().is_symlink() {
            return;
        }
        let Ok(bytes) = fs::read(&path) else {
            return;
        };
        let Ok(text) = String::from_utf8(bytes) else {
            return;
        };
        let text = text.trim();
        let parts: Vec<&str> = text.split(',').collect();
        if parts.len() != 2 {
            return;
        }
        for (key, value) in [("ipb_member_id", parts[0]), ("ipb_pass_hash", parts[1])] {
            if let Some(existing) = self.cookies.iter_mut().find(|(name, _)| name == key) {
                existing.1 = value.to_string();
            } else {
                self.cookies.push((key.to_string(), value.to_string()));
            }
        }
        self.set_cookie_header();
        self.has_login = true;
        if let Some(message) =
            crate::i18n::render(crate::i18n::Locale::EnUs, "XEH_LOAD_OLD_COOKIE", &[])
        {
            let _ = self.logger.info(&message);
        }
    }
}

fn as_bool(key: &str, value: &Value) -> Result<bool, AppError> {
    value
        .as_bool()
        .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be a bool")))
}

fn as_int(key: &str, value: &Value) -> Result<i64, AppError> {
    value
        .as_i64()
        .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be an int")))
}

fn as_string(key: &str, value: &Value) -> Result<String, AppError> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be a string")))
}

fn as_int_array(key: &str, value: &Value) -> Result<Vec<i64>, AppError> {
    let array = value
        .as_array()
        .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be an array")))?;
    array
        .iter()
        .map(|item| {
            item.as_i64()
                .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be an int array")))
        })
        .collect()
}

pub(crate) fn as_range(key: &str, value: &Value) -> Result<Vec<[i64; 2]>, AppError> {
    let rows = value
        .as_array()
        .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be a range")))?;
    rows.iter()
        .map(|row| {
            let row = row
                .as_array()
                .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be a range")))?;
            if row.len() != 2 {
                return Err(AppError::UnknownConfig(format!("{key} must be a range")));
            }
            let start = row[0]
                .as_i64()
                .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be a range")))?;
            let end = row[1]
                .as_i64()
                .ok_or_else(|| AppError::UnknownConfig(format!("{key} must be a range")))?;
            Ok([start, end])
        })
        .collect()
}

fn valid_task_url(url: &str) -> bool {
    static TASK_URL: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"^https*://(?:[g.]*e-|ex)hentai\.org/[^/]+/\d+/[^/]+/*#*$")
            .expect("task url regex")
    });
    TASK_URL.is_match(url)
}

fn exhentai_url(url: &str) -> bool {
    static EXH: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^https*://exhentai\.org").expect("exh regex"));
    EXH.is_match(url)
}

struct SessionDocument<'a> {
    save_tasks: bool,
    tasks: &'a IndexMap<String, Task>,
    cookies: &'a [(String, String)],
}

impl Serialize for SessionDocument<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2))?;
        map.serialize_entry(
            "tasks",
            &SessionTasks {
                save_tasks: self.save_tasks,
                tasks: self.tasks,
            },
        )?;
        map.serialize_entry("cookies", &Cookies(self.cookies))?;
        map.end()
    }
}

struct SessionTasks<'a> {
    save_tasks: bool,
    tasks: &'a IndexMap<String, Task>,
}

impl Serialize for SessionTasks<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !self.save_tasks {
            return serializer.serialize_map(Some(0))?.end();
        }
        let mut map = serializer.serialize_map(Some(self.tasks.len()))?;
        for (guid, task) in self.tasks {
            map.serialize_entry(guid, task)?;
        }
        map.end()
    }
}

struct Cookies<'a>(&'a [(String, String)]);

impl Serialize for Cookies<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}
