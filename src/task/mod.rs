//! The download task from the reference `xeHentai/task.py`.
//!
//! One `Task` keeps exactly the reference public attributes: `url`, optional
//! `gid`/`sethash`, `failcode`, `state`, `guid`, the create-time `config`
//! snapshot, the open `meta` mapping, the reload/duplicate/rename maps and the
//! two queues. File work lives in [`file_ops`] and [`archive`], the two child
//! modules of the same reference module.
#![allow(dead_code)]

mod archive;
pub(crate) mod file_ops;

#[cfg(test)]
pub(crate) use archive::{
    ArchiveFault, archive_cleanup_gate, archive_waiting_gate, archive_writer_log,
    archive_writer_stats, arm_archive_cleanup_gate, arm_archive_fault, arm_archive_gate,
    arm_archive_waiting, arm_detach_candidate, detach_gallery, reset_archive_writer_stats,
};
pub use archive::{ArchiveSnapshot, CommittedArchive, PreparedArchive};
pub(crate) use archive::{
    commit_prepared, discard_prepared, make_archive_snapshot, prepare_archive_snapshot,
    remove_detached,
};

use std::collections::{BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use indexmap::IndexMap;
use serde::ser::{SerializeMap, Serializer};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::{Map as JsonMap, Value};
use tokio::sync::Notify;

use crate::config::Config;
use crate::r#const as consts;
use crate::util;

pub use file_ops::SaveStart;
pub use file_ops::StagedSave;
pub use file_ops::TaskFiles;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RenameError {
    pub from: String,
    pub to: String,
    pub message: String,
}
pub use file_ops::resolve_dir;
pub use file_ops::{
    ActivePrivateLog, GalleryGrant, OpenedEntry, PrivatePaths, boundary_path, normalize,
    path_within,
};
#[cfg(test)]
pub use file_ops::{DirectoryObservation, PrivatePolicy, observe_directory};
#[cfg(test)]
pub(crate) use file_ops::{
    ObservationSwap, PreopenSwap, arm_observation_swap, arm_preopen_swap, run_observation_swap,
};

/// The fourteen configuration keys captured when a task is created, in the
/// order the reference `config.py` defines them.
pub const BASE_CONFIG_KEYS: [&str; 14] = [
    "dir",
    "download_ori",
    "jpn_title",
    "rename_ori",
    "proxy_image",
    "proxy_image_only",
    "make_archive",
    "download_range",
    "scan_thread_cnt",
    "download_thread_cnt",
    "download_timeout",
    "ignored_errors",
    "delete_task_files",
    "low_speed_threshold",
];

#[derive(Clone, Debug, PartialEq)]
pub struct TaskConfig {
    pub dir: String,
    pub download_ori: bool,
    pub jpn_title: bool,
    pub rename_ori: bool,
    pub proxy_image: bool,
    pub proxy_image_only: bool,
    pub make_archive: bool,
    pub download_range: Option<Vec<[i64; 2]>>,
    pub scan_thread_cnt: i64,
    pub download_thread_cnt: i64,
    pub download_timeout: i64,
    pub ignored_errors: Vec<i64>,
    pub delete_task_files: bool,
    pub low_speed_threshold: i64,
    /// Task kwargs that are not one of the fixed keys. The reference stores
    /// them in the task config dict and persists them unchanged.
    pub extra: JsonMap<String, Value>,
}

impl TaskConfig {
    /// Snapshot the reference's fourteen global keys, then overlay task kwargs.
    pub fn from_app(cfg: &Config, kwargs: &JsonMap<String, Value>) -> Result<Self, TaskError> {
        let mut config = Self {
            dir: cfg.dir.clone(),
            download_ori: cfg.download_ori,
            jpn_title: cfg.jpn_title,
            rename_ori: cfg.rename_ori,
            proxy_image: cfg.proxy_image,
            proxy_image_only: cfg.proxy_image_only,
            make_archive: cfg.make_archive,
            download_range: cfg.download_range.clone(),
            scan_thread_cnt: cfg.scan_thread_cnt,
            download_thread_cnt: cfg.download_thread_cnt,
            download_timeout: cfg.download_timeout,
            ignored_errors: cfg.ignored_errors.clone(),
            delete_task_files: cfg.delete_task_files,
            low_speed_threshold: cfg.low_speed_threshold,
            extra: JsonMap::new(),
        };
        for (key, value) in kwargs {
            match key.as_str() {
                "dir" => config.dir = type_string(key, value)?,
                "download_ori" => config.download_ori = type_bool(key, value)?,
                "jpn_title" => config.jpn_title = type_bool(key, value)?,
                "rename_ori" => config.rename_ori = type_bool(key, value)?,
                "proxy_image" => config.proxy_image = type_bool(key, value)?,
                "proxy_image_only" => config.proxy_image_only = type_bool(key, value)?,
                "make_archive" => config.make_archive = type_bool(key, value)?,
                "download_range" => {
                    config.download_range = match value {
                        Value::Null => None,
                        _ => Some(type_range(key, value)?),
                    };
                }
                "scan_thread_cnt" => config.scan_thread_cnt = type_int(key, value)?,
                "download_thread_cnt" => config.download_thread_cnt = type_int(key, value)?,
                "download_timeout" => config.download_timeout = type_int(key, value)?,
                "ignored_errors" => config.ignored_errors = type_int_array(key, value)?,
                "delete_task_files" => config.delete_task_files = type_bool(key, value)?,
                "low_speed_threshold" => config.low_speed_threshold = type_int(key, value)?,
                _ => {
                    config.extra.insert(key.clone(), value.clone());
                }
            }
        }
        Ok(config)
    }
}

fn type_bool(key: &str, value: &Value) -> Result<bool, TaskError> {
    value
        .as_bool()
        .ok_or_else(|| TaskError::InvalidConfigValue {
            key: key.to_string(),
            expected: "bool",
        })
}

fn type_int(key: &str, value: &Value) -> Result<i64, TaskError> {
    value.as_i64().ok_or_else(|| TaskError::InvalidConfigValue {
        key: key.to_string(),
        expected: "int",
    })
}

fn type_string(key: &str, value: &Value) -> Result<String, TaskError> {
    value
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| TaskError::InvalidConfigValue {
            key: key.to_string(),
            expected: "string",
        })
}

fn type_int_array(key: &str, value: &Value) -> Result<Vec<i64>, TaskError> {
    let array = value
        .as_array()
        .ok_or_else(|| TaskError::InvalidConfigValue {
            key: key.to_string(),
            expected: "array",
        })?;
    array
        .iter()
        .map(|item| {
            item.as_i64().ok_or_else(|| TaskError::InvalidConfigValue {
                key: key.to_string(),
                expected: "int array",
            })
        })
        .collect()
}

fn type_range(key: &str, value: &Value) -> Result<Vec<[i64; 2]>, TaskError> {
    let rows = value
        .as_array()
        .ok_or_else(|| TaskError::InvalidConfigValue {
            key: key.to_string(),
            expected: "range",
        })?;
    rows.iter()
        .map(|row| {
            let row = row
                .as_array()
                .ok_or_else(|| TaskError::InvalidConfigValue {
                    key: key.to_string(),
                    expected: "range",
                })?;
            if row.len() != 2 {
                return Err(TaskError::InvalidConfigValue {
                    key: key.to_string(),
                    expected: "range pair",
                });
            }
            let start = row[0]
                .as_i64()
                .ok_or_else(|| TaskError::InvalidConfigValue {
                    key: key.to_string(),
                    expected: "range int",
                })?;
            let end = row[1]
                .as_i64()
                .ok_or_else(|| TaskError::InvalidConfigValue {
                    key: key.to_string(),
                    expected: "range int",
                })?;
            Ok([start, end])
        })
        .collect()
}

impl Serialize for TaskConfig {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(BASE_CONFIG_KEYS.len() + self.extra.len()))?;
        map.serialize_entry("dir", &self.dir)?;
        map.serialize_entry("download_ori", &self.download_ori)?;
        map.serialize_entry("jpn_title", &self.jpn_title)?;
        map.serialize_entry("rename_ori", &self.rename_ori)?;
        map.serialize_entry("proxy_image", &self.proxy_image)?;
        map.serialize_entry("proxy_image_only", &self.proxy_image_only)?;
        map.serialize_entry("make_archive", &self.make_archive)?;
        map.serialize_entry("download_range", &self.download_range)?;
        map.serialize_entry("scan_thread_cnt", &self.scan_thread_cnt)?;
        map.serialize_entry("download_thread_cnt", &self.download_thread_cnt)?;
        map.serialize_entry("download_timeout", &self.download_timeout)?;
        map.serialize_entry("ignored_errors", &self.ignored_errors)?;
        map.serialize_entry("delete_task_files", &self.delete_task_files)?;
        map.serialize_entry("low_speed_threshold", &self.low_speed_threshold)?;
        for (key, value) in &self.extra {
            map.serialize_entry(key, value)?;
        }
        map.end()
    }
}

impl<'de> Deserialize<'de> for TaskConfig {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error as _;
        let raw = JsonMap::<String, Value>::deserialize(deserializer)?;
        let mut config = Self {
            dir: String::new(),
            download_ori: false,
            jpn_title: false,
            rename_ori: false,
            proxy_image: false,
            proxy_image_only: false,
            make_archive: false,
            download_range: None,
            scan_thread_cnt: 0,
            download_thread_cnt: 0,
            download_timeout: 0,
            ignored_errors: Vec::new(),
            delete_task_files: false,
            low_speed_threshold: 0,
            extra: JsonMap::new(),
        };
        let mut present = 0usize;
        for (key, value) in raw {
            let result = (|| -> Result<bool, TaskError> {
                match key.as_str() {
                    "dir" => config.dir = type_string(&key, &value)?,
                    "download_ori" => config.download_ori = type_bool(&key, &value)?,
                    "jpn_title" => config.jpn_title = type_bool(&key, &value)?,
                    "rename_ori" => config.rename_ori = type_bool(&key, &value)?,
                    "proxy_image" => config.proxy_image = type_bool(&key, &value)?,
                    "proxy_image_only" => config.proxy_image_only = type_bool(&key, &value)?,
                    "make_archive" => config.make_archive = type_bool(&key, &value)?,
                    "download_range" => {
                        config.download_range = match value {
                            Value::Null => None,
                            _ => Some(type_range(&key, &value)?),
                        }
                    }
                    "scan_thread_cnt" => config.scan_thread_cnt = type_int(&key, &value)?,
                    "download_thread_cnt" => config.download_thread_cnt = type_int(&key, &value)?,
                    "download_timeout" => config.download_timeout = type_int(&key, &value)?,
                    "ignored_errors" => config.ignored_errors = type_int_array(&key, &value)?,
                    "delete_task_files" => config.delete_task_files = type_bool(&key, &value)?,
                    "low_speed_threshold" => config.low_speed_threshold = type_int(&key, &value)?,
                    _ => {
                        config.extra.insert(key.clone(), value);
                        return Ok(false);
                    }
                }
                Ok(true)
            })();
            match result {
                Ok(true) => present += 1,
                Ok(false) => {}
                Err(TaskError::InvalidConfigValue { .. }) => {
                    return Err(D::Error::custom(format!(
                        "invalid task config value for {key}"
                    )));
                }
                Err(_) => {
                    return Err(D::Error::custom(format!(
                        "invalid task config value for {key}"
                    )));
                }
            }
        }
        if present != BASE_CONFIG_KEYS.len() {
            return Err(D::Error::custom(
                "task config is missing one of the fourteen required keys",
            ));
        }
        Ok(config)
    }
}

/// Key of `Task.renamed_map`: the reference starts with integer ids and later
/// writes string ids, so both can coexist in one dict.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FidKey {
    Int(i64),
    Text(String),
}

impl FidKey {
    pub fn wire_key(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            Self::Text(value) => value.clone(),
        }
    }

    pub fn as_fid(&self) -> Result<i64, TaskError> {
        match self {
            Self::Int(value) => Ok(*value),
            Self::Text(value) => value
                .parse::<i64>()
                .map_err(|_| TaskError::ParseInt(value.clone())),
        }
    }
}

impl Serialize for FidKey {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.wire_key())
    }
}

struct RenamedMap<'a>(&'a IndexMap<FidKey, String>);

impl Serialize for RenamedMap<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (key, value) in self.0 {
            // Deliberately keeps duplicate textual keys: the reference dict can
            // hold both `5` and `"5"`, and json.dumps emits both.
            map.serialize_entry(&key.wire_key(), value)?;
        }
        map.end()
    }
}

struct DuplicateMap<'a>(&'a IndexMap<i64, BTreeSet<i64>>);

impl Serialize for DuplicateMap<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if !self.0.is_empty() {
            // The reference `to_dict` keeps real `set` values, which
            // json.dumps refuses to serialize. Session writing therefore fails.
            return Err(serde::ser::Error::custom(
                "duplicate_map contains a pending copy set and cannot be serialized",
            ));
        }
        serializer.serialize_map(Some(0))?.end()
    }
}

#[derive(Debug)]
pub enum TaskError {
    InvalidConfigValue {
        key: String,
        expected: &'static str,
    },
    MissingMeta(&'static str),
    MetaType {
        key: &'static str,
        expected: &'static str,
    },
    ParseInt(String),
    InvalidUrl,
    QueueNotStarted(&'static str),
    UnknownImage(String),
    SimulatedSizeExceeded,
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    UnsafePath {
        path: PathBuf,
        reason: &'static str,
    },
    NameCollisionBug {
        name: String,
    },
    PublishFailed {
        from: PathBuf,
        to: PathBuf,
        message: String,
    },
    /// The post-publish restore of a detached gallery failed: the images
    /// stay retained at `owned` and the error reports that location instead
    /// of claiming the public `target` survived.
    RollbackFailed {
        owned: PathBuf,
        target: PathBuf,
        message: String,
    },
    Zip(String),
}

impl std::fmt::Display for TaskError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidConfigValue { key, expected } => {
                write!(
                    f,
                    "invalid task config value for {key}: expected {expected}"
                )
            }
            Self::MissingMeta(key) => write!(f, "task meta has no {key}"),
            Self::MetaType { key, expected } => {
                write!(f, "task meta {key} is not {expected}")
            }
            Self::ParseInt(value) => write!(f, "invalid literal for int(): {value}"),
            Self::InvalidUrl => write!(f, "task url has no gallery identifier"),
            Self::QueueNotStarted(queue) => write!(f, "task {queue} queue is not started"),
            Self::UnknownImage(hash) => write!(f, "reload map has no {hash}"),
            Self::SimulatedSizeExceeded => write!(f, "simulated size exceeded"),
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::UnsafePath { path, reason } => {
                write!(f, "unsafe path {}: {reason}", path.display())
            }
            Self::NameCollisionBug { name } => write!(
                f,
                "expected string or bytes-like object, got 'function' while renaming {name}"
            ),
            Self::PublishFailed { from, to, message } => write!(
                f,
                "cannot publish {} to {}: {message}",
                from.display(),
                to.display()
            ),
            Self::RollbackFailed {
                owned,
                target,
                message,
            } => write!(
                f,
                "cannot restore {} from owned {}: {message}; images retained at {}",
                target.display(),
                owned.display(),
                owned.display()
            ),
            Self::Zip(message) => write!(f, "zip error: {message}"),
        }
    }
}

impl std::error::Error for TaskError {}

/// A task as the application and the session file see it.
pub struct Task {
    pub url: String,
    pub gid: Option<String>,
    pub sethash: Option<String>,
    pub failcode: i64,
    pub state: i64,
    pub guid: String,
    pub config: TaskConfig,
    pub meta: JsonMap<String, Value>,
    pub reload_map: IndexMap<String, (String, String)>,
    pub duplicate_map: IndexMap<i64, BTreeSet<i64>>,
    pub renamed_map: IndexMap<FidKey, String>,
    pub img_q: Option<VecDeque<String>>,
    pub page_q: Option<VecDeque<String>>,
    flist_done: BTreeSet<i64>,
    files: Arc<TaskFiles>,
    /// Identity of this concrete `Task` object. A deleted task recreated under
    /// the same guid gets a new owner, so a late worker result can never land
    /// on the replacement.
    owner: u64,
    /// Local run marker bumped whenever a run is started, paused, resumed or
    /// invalidated by delete/reuse. A worker captures it with `token` and must
    /// match it before publishing anything.
    run_marker: u64,
    page_notify: Notify,
    img_notify: Notify,
}

static TASK_OWNER_COUNTER: AtomicU64 = AtomicU64::new(1);

impl Task {
    pub fn new(
        url: &str,
        config: TaskConfig,
        guid: String,
        base: &Path,
    ) -> Result<Self, TaskError> {
        Self::new_with_private_paths(url, config, guid, base, PrivatePaths::for_base(base))
    }

    /// Build a task that reads through `private`, the application's live
    /// private-path cell. `App::add_task` and session restore use this entry, so
    /// the policy is in place BEFORE any scan, native read or publication.
    pub fn new_with_private_paths(
        url: &str,
        config: TaskConfig,
        guid: String,
        base: &Path,
        private: Arc<PrivatePaths>,
    ) -> Result<Self, TaskError> {
        let root = file_ops::resolve_dir(base, &config.dir);
        // A writable cfg.dir may not point into an owned detached container:
        // reject the task before any gallery/file admission.
        file_ops::ensure_not_inside_detached(&root)?;
        let mut task = Self {
            url: url.to_string(),
            gid: None,
            sethash: None,
            failcode: 0,
            state: consts::TASK_STATE_WAITING,
            guid,
            config,
            meta: JsonMap::new(),
            reload_map: IndexMap::new(),
            duplicate_map: IndexMap::new(),
            renamed_map: IndexMap::new(),
            img_q: None,
            page_q: None,
            flist_done: BTreeSet::new(),
            files: TaskFiles::with_private_paths(root, private),
            owner: TASK_OWNER_COUNTER.fetch_add(1, Ordering::Relaxed),
            run_marker: 0,
            page_notify: Notify::new(),
            img_notify: Notify::new(),
        };
        task.refresh_gallery_identifier();
        Ok(task)
    }

    /// `(owner, run_marker)` captured by a worker when it takes one queue item.
    pub fn token(&self) -> (u64, u64) {
        (self.owner, self.run_marker)
    }

    /// Invalidate every in-flight callback for this task object and return the
    /// new token. Called on start, pause, resume, delete and same-guid reuse.
    pub fn bump_run(&mut self) -> u64 {
        self.run_marker = self.run_marker.wrapping_add(1);
        self.run_marker
    }

    pub fn page_waiter(&self) -> &Notify {
        &self.page_notify
    }

    pub fn img_waiter(&self) -> &Notify {
        &self.img_notify
    }

    fn refresh_gallery_identifier(&mut self) {
        let (gid, sethash) = file_ops::gallery_identifier(&self.url);
        self.gid = gid;
        self.sethash = sethash;
    }

    /// Reference `from_dict`: only attributes the fresh `Task("")` has are
    /// copied, then `gid`/`sethash` are recomputed from the URL.
    pub fn from_wire(value: &Value, base: &Path) -> Result<Self, TaskLoadError> {
        Self::from_wire_with_private_paths(value, base, PrivatePaths::for_base(base))
    }

    /// Restore a task that reads through the application's live private-path
    /// cell. `App::load_session` uses this entry, so a restored historical root
    /// reads under the same current policy as a freshly added task.
    pub fn from_wire_with_private_paths(
        value: &Value,
        base: &Path,
        private: Arc<PrivatePaths>,
    ) -> Result<Self, TaskLoadError> {
        let object = value
            .as_object()
            .ok_or_else(|| TaskLoadError::Shape("task is not an object".into()))?;
        let url = match object.get("url") {
            Some(Value::String(url)) => url.clone(),
            None => String::new(),
            Some(_) => return Err(TaskLoadError::Shape("task url is not a string".into())),
        };
        let guid = match object.get("guid") {
            Some(Value::String(guid)) => guid.clone(),
            Some(_) => return Err(TaskLoadError::Shape("task guid is not a string".into())),
            None => return Err(TaskLoadError::Missing("guid")),
        };
        let config_value = object
            .get("config")
            .ok_or(TaskLoadError::Missing("config"))?;
        let config: TaskConfig = serde_json::from_value(config_value.clone())
            .map_err(|error| TaskLoadError::Shape(format!("task config: {error}")))?;
        let mut task = Self::new_with_private_paths(&url, config, guid, base, private)
            .map_err(|error| TaskLoadError::Shape(error.to_string()))?;
        if let Some(value) = object.get("failcode") {
            task.failcode = value
                .as_i64()
                .ok_or_else(|| TaskLoadError::Shape("task failcode is not an int".into()))?;
        }
        if let Some(value) = object.get("state") {
            task.state = value
                .as_i64()
                .ok_or_else(|| TaskLoadError::Shape("task state is not an int".into()))?;
        }
        if let Some(value) = object.get("meta") {
            let meta = value
                .as_object()
                .ok_or_else(|| TaskLoadError::Shape("task meta is not an object".into()))?;
            task.meta = meta.clone();
        }
        if let Some(value) = object.get("reload_map") {
            let map = value
                .as_object()
                .ok_or_else(|| TaskLoadError::Shape("reload_map is not an object".into()))?;
            for (key, entry) in map {
                let pair = entry.as_array().ok_or_else(|| {
                    TaskLoadError::Shape(format!("reload_map {key} is not an array"))
                })?;
                if pair.len() != 2 {
                    return Err(TaskLoadError::Shape(format!(
                        "reload_map {key} is not a pair"
                    )));
                }
                let url = pair[0]
                    .as_str()
                    .ok_or_else(|| TaskLoadError::Shape("reload url is not a string".into()))?;
                let name = pair[1]
                    .as_str()
                    .ok_or_else(|| TaskLoadError::Shape("reload name is not a string".into()))?;
                task.reload_map
                    .insert(key.clone(), (url.to_string(), name.to_string()));
            }
        }
        if let Some(value) = object.get("duplicate_map") {
            let map = value
                .as_object()
                .ok_or_else(|| TaskLoadError::Shape("duplicate_map is not an object".into()))?;
            for (key, entry) in map {
                let fid = key
                    .parse::<i64>()
                    .map_err(|_| TaskLoadError::Shape(format!("duplicate_map key {key}")))?;
                let copies = entry.as_array().ok_or_else(|| {
                    TaskLoadError::Shape(format!("duplicate_map {key} is not an array"))
                })?;
                let mut set = BTreeSet::new();
                for copy in copies {
                    let copy = copy.as_i64().ok_or_else(|| {
                        TaskLoadError::Shape("duplicate_map entry is not an int".into())
                    })?;
                    set.insert(copy);
                }
                task.duplicate_map.insert(fid, set);
            }
        }
        if let Some(value) = object.get("renamed_map") {
            let map = value
                .as_object()
                .ok_or_else(|| TaskLoadError::Shape("renamed_map is not an object".into()))?;
            for (key, entry) in map {
                let name = entry
                    .as_str()
                    .ok_or_else(|| TaskLoadError::Shape("renamed name is not a string".into()))?;
                task.renamed_map
                    .insert(FidKey::Text(key.clone()), name.to_string());
            }
        }
        for key in ["img_q", "page_q"] {
            let Some(value) = object.get(key) else {
                continue;
            };
            if value.is_null() {
                continue;
            }
            let items = value
                .as_array()
                .ok_or_else(|| TaskLoadError::Shape(format!("{key} is not an array")))?;
            if items.is_empty() {
                continue;
            }
            let mut queue = VecDeque::new();
            for item in items {
                let item = item
                    .as_str()
                    .ok_or_else(|| TaskLoadError::Shape(format!("{key} item is not a string")))?;
                queue.push_back(item.to_string());
            }
            if key == "img_q" {
                task.img_q = Some(queue);
            } else {
                task.page_q = Some(queue);
            }
        }
        Ok(task)
    }

    pub fn flist_done(&self) -> &BTreeSet<i64> {
        &self.flist_done
    }

    pub fn files(&self) -> &Arc<TaskFiles> {
        &self.files
    }

    pub fn meta_i64(&self, key: &'static str) -> Result<i64, TaskError> {
        match self.meta.get(key) {
            Some(Value::Number(number)) => number.as_i64().ok_or(TaskError::MetaType {
                key,
                expected: "an int",
            }),
            Some(_) => Err(TaskError::MetaType {
                key,
                expected: "an int",
            }),
            None => Err(TaskError::MissingMeta(key)),
        }
    }

    pub fn meta_str(&self, key: &'static str) -> Result<&str, TaskError> {
        match self.meta.get(key) {
            Some(Value::String(text)) => Ok(text),
            Some(_) => Err(TaskError::MetaType {
                key,
                expected: "a string",
            }),
            None => Err(TaskError::MissingMeta(key)),
        }
    }

    pub fn cleanup(&mut self, before_delete: bool) -> Result<(), TaskError> {
        if before_delete {
            if self.config.delete_task_files && self.meta.contains_key("title") {
                let fpath = self.get_fpath()?;
                if file_ops::path_exists(&fpath)? {
                    file_ops::remove_tree(&fpath)?;
                }
                let zippath = file_ops::zip_path(&fpath)?;
                if file_ops::path_exists(&zippath)? {
                    file_ops::remove_file(&zippath)?;
                }
            }
        } else if self.state == consts::TASK_STATE_FINISHED
            || self.state == consts::TASK_STATE_FAILED
        {
            self.img_q = None;
            self.page_q = None;
            self.reload_map.clear();
        }
        Ok(())
    }

    pub fn set_fail(&mut self, code: i64) {
        self.state = consts::TASK_STATE_FAILED;
        self.failcode = code;
        self.meta = JsonMap::new();
    }

    pub fn migrate_exhentai(&mut self) -> bool {
        let Some(captures) = file_ops::exhentai_path(&self.url) else {
            return false;
        };
        self.url = format!("https://exhentai.org{}", captures);
        if self.state == consts::TASK_STATE_FAILED {
            self.state = consts::TASK_STATE_WAITING;
        }
        self.failcode = 0;
        true
    }

    pub fn mpv_url(&self) -> String {
        let gid = self.gid.as_deref().unwrap_or("None");
        let sethash = self.sethash.as_deref().unwrap_or("None");
        file_ops::regex_replace_all(
            &format!("/./{gid}/{sethash}"),
            &format!("/mpv/{gid}/{sethash}"),
            &self.url,
        )
    }

    pub fn update_meta(&mut self, meta: JsonMap<String, Value>) -> Result<(), TaskError> {
        for (key, value) in meta {
            self.meta.insert(key, value);
        }
        let jpn_name = self
            .meta
            .get("gjname")
            .ok_or(TaskError::MissingMeta("gjname"))?
            .clone();
        let title = if self.config.jpn_title && value_truthy(&jpn_name) {
            jpn_name
        } else {
            self.meta
                .get("gnname")
                .ok_or(TaskError::MissingMeta("gnname"))?
                .clone()
        };
        self.meta.insert("title".to_string(), title);
        Ok(())
    }

    pub fn base_url(&self) -> Result<String, TaskError> {
        file_ops::base_url(&self.url).ok_or(TaskError::InvalidUrl)
    }

    pub fn put_img_queue(
        &mut self,
        imgurl: &str,
        reload_url: &str,
        fname: &str,
    ) -> Result<(), TaskError> {
        let img_hash = if self.config.download_ori {
            file_ops::gallery_hash(reload_url).ok_or(TaskError::InvalidUrl)?
        } else {
            Self::get_imghash(imgurl)?
        };
        let this_fid = file_ops::gallery_fid(reload_url).ok_or(TaskError::InvalidUrl)?;
        self.renamed_map
            .insert(FidKey::Int(this_fid), fname.to_string());
        if self.reload_map.contains_key(&img_hash) {
            let fpath = self.get_fpath()?;
            let old_fid = self.get_fname(&img_hash)?.0;
            let old_f = fpath.join(self.get_fidpad(old_fid, "jpg")?);
            let this_f = fpath.join(self.get_fidpad(this_fid, "jpg")?);
            // Short file critical section, exactly like the reference; the
            // self-duplicate branch releases the lock instead of leaking it.
            let guard = self.files.lock();
            if this_fid == old_fid {
                drop(guard);
            } else {
                if file_ops::path_exists(&old_f)? {
                    let result = file_ops::copy_file(&old_f, &this_f);
                    drop(guard);
                    result?;
                    self.set_fid_finished(this_fid);
                } else {
                    self.duplicate_map
                        .entry(old_fid)
                        .or_default()
                        .insert(this_fid);
                    drop(guard);
                }
                return Ok(());
            }
        }
        self.reload_map
            .insert(img_hash, (reload_url.to_string(), fname.to_string()));
        self.img_q
            .as_mut()
            .ok_or(TaskError::QueueNotStarted("img"))?
            .push_back(imgurl.to_string());
        self.img_notify.notify_one();
        Ok(())
    }

    pub fn put_page_queue_retry(&mut self, redirect_url: &str) -> Result<(), TaskError> {
        if redirect_url.is_empty() {
            return Ok(());
        }
        let img_hash = if redirect_url.contains("redirect=") {
            let page_url = file_ops::redirect_target(redirect_url).ok_or(TaskError::InvalidUrl)?;
            file_ops::gallery_hash(&page_url).ok_or(TaskError::InvalidUrl)?
        } else {
            Self::get_imghash(redirect_url)?
        };
        let (reload_url, _) = self
            .reload_map
            .shift_remove(&img_hash)
            .ok_or_else(|| TaskError::UnknownImage(img_hash.clone()))?;
        self.page_q
            .as_mut()
            .ok_or(TaskError::QueueNotStarted("page"))?
            .push_back(reload_url);
        self.page_notify.notify_one();
        Ok(())
    }

    pub fn scan_downloaded(&mut self) -> Result<(), TaskError> {
        let fpath = self.get_fpath()?;
        let donefile = file_ops::path_exists(&fpath.join(".xehdone"))? || {
            let zippath = file_ops::zip_path(&fpath)?;
            file_ops::path_exists(&zippath)?
        };
        let total = self.meta_i64("total")?;
        let mut range_index = 0usize;
        let mut fid = 1;
        while fid <= total {
            // Reference truthiness: an empty list (the interactive "download
            // everything" answer) is no range at all, not a range that matches
            // nothing.
            if let Some(ranges) = &self.config.download_range
                && !ranges.is_empty()
            {
                let mut found = false;
                for [start, end] in ranges.iter().skip(range_index) {
                    if fid > *end {
                        range_index += 1;
                    } else if *start <= fid && fid <= *end {
                        found = true;
                        break;
                    } else if fid < *start {
                        break;
                    }
                }
                if !found {
                    self.flist_done.insert(fid);
                    fid += 1;
                    continue;
                }
            }
            let fname = fpath.join(self.get_fidpad(fid, "jpg")?);
            if donefile {
                self.flist_done.insert(fid);
            } else if file_ops::path_exists(&fname)? {
                if file_ops::file_size(&fname)? == 0 {
                    file_ops::remove_file(&fname)?;
                } else {
                    self.flist_done.insert(fid);
                }
            }
            fid += 1;
        }
        let finished = self.flist_done.len() as i64;
        self.meta
            .insert("finished".to_string(), Value::Number(finished.into()));
        // The reference compares instead of assigning here (task.py:206); the
        // no-op is preserved.
        let _ = self.meta_i64("finished")? == total;
        Ok(())
    }

    pub fn put_page_queue(&mut self, url: &str) -> Result<(), TaskError> {
        let fid = file_ops::gallery_fid(url).ok_or(TaskError::InvalidUrl)?;
        if !self.flist_done.contains(&fid) {
            self.page_q
                .as_mut()
                .ok_or(TaskError::QueueNotStarted("page"))?
                .push_back(url.to_string());
            self.page_notify.notify_one();
        }
        Ok(())
    }

    /// Pre-request duplicate/resume check. The reference always transfers the
    /// body and only notices an existing destination inside `save_file`; the
    /// M4 pipeline checks the destination first so a resumed session transfers
    /// only the missing images. Returns the fid and path when a non-empty file
    /// already exists.
    pub fn existing_image(&self, imgurl: &str) -> Result<Option<(i64, PathBuf)>, TaskError> {
        let fid = if file_ops::has_image_hash(imgurl) {
            match self.reload_map.get(&Self::get_imghash(imgurl)?) {
                Some((pageurl, _)) => {
                    file_ops::gallery_fid(pageurl).ok_or(TaskError::InvalidUrl)?
                }
                None => return Ok(None),
            }
        } else {
            match file_ops::fullimg_fid(imgurl) {
                Some(fid) => fid,
                None => return Ok(None),
            }
        };
        let fpath = self.get_fpath()?;
        let dest = self
            .files
            .child_file(&fpath, &self.get_fidpad(fid, "jpg")?)?;
        if file_ops::path_exists(&dest)? && file_ops::file_size(&dest)? > 0 {
            Ok(Some((fid, dest)))
        } else {
            Ok(None)
        }
    }

    pub fn set_fid_finished(&mut self, fid: i64) {
        let _guard = self.files.counter_lock();
        self.flist_done.insert(fid);
        let finished = self.flist_done.len() as i64;
        self.meta
            .insert("finished".to_string(), Value::Number(finished.into()));
    }

    pub fn get_fid_unfinished(&self) -> Result<Vec<i64>, TaskError> {
        let total = self.meta_i64("total")?;
        Ok((1..total)
            .filter(|fid| !self.flist_done.contains(fid))
            .collect())
    }

    /// First ten characters of the image hash already present in the URL.
    /// No digest is computed here.
    pub fn get_imghash(imgurl: &str) -> Result<String, TaskError> {
        let captures = file_ops::image_hash_captures(imgurl).ok_or(TaskError::InvalidUrl)?;
        Ok(captures.chars().take(10).collect())
    }

    pub fn get_imgfid(&self, imgurl: &str) -> Result<i64, TaskError> {
        if file_ops::has_image_hash(imgurl) {
            return Ok(self.get_fname(&Self::get_imghash(imgurl)?)?.0);
        }
        file_ops::fullimg_fid(imgurl).ok_or(TaskError::InvalidUrl)
    }

    pub fn get_fname(&self, img_hash: &str) -> Result<(i64, String), TaskError> {
        let (pageurl, fname) = self
            .reload_map
            .get(img_hash)
            .ok_or_else(|| TaskError::UnknownImage(img_hash.to_string()))?;
        let fid = file_ops::gallery_fid(pageurl).ok_or(TaskError::InvalidUrl)?;
        Ok((fid, fname.clone()))
    }

    pub fn get_fpath(&self) -> Result<PathBuf, TaskError> {
        let title = self.meta_str("title")?;
        self.files.task_dir(&util::legalpath(title))
    }

    /// Freeze one gallery read grant: the fixed authorized download root, the
    /// validated single gallery component, the mandatory pre-permit directory
    /// observations and this task's private-path policy.
    ///
    /// The component still passes the existing `task_dir` admission checks
    /// (traversal, symlink gallery, owned detached container) before the
    /// observation boundary, exactly like the earlier snapshot scope.
    pub fn read_grant(&self) -> Result<GalleryGrant, TaskError> {
        let title = self.meta_str("title")?;
        let component = util::legalpath(title);
        let path = self.files.task_dir(&component)?;
        #[cfg(test)]
        crate::task::run_observation_swap(&path);
        Ok(self.files.grant(&component, &path))
    }

    pub fn get_fidpad(&self, fid: i64, ext: &str) -> Result<String, TaskError> {
        let total = self.meta_i64("total")?;
        let width = total.to_string().len();
        Ok(format!("{fid:0width$}.{ext}"))
    }

    pub fn to_value(&self) -> Result<Value, TaskWireError> {
        serde_json::to_value(self).map_err(|error| TaskWireError(error.to_string()))
    }

    pub fn duplicate_pairs(&self) -> Vec<(i64, Vec<i64>)> {
        self.duplicate_map
            .iter()
            .map(|(fid, copies)| (*fid, copies.iter().copied().collect()))
            .collect()
    }

    pub fn renamed_pairs(&self) -> Vec<(Value, String)> {
        self.renamed_map
            .iter()
            .map(|(key, name)| {
                let key = match key {
                    FidKey::Int(value) => Value::Number((*value).into()),
                    FidKey::Text(value) => Value::String(value.clone()),
                };
                (key, name.clone())
            })
            .collect()
    }
}

impl Serialize for Task {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(None)?;
        map.serialize_entry("url", &self.url)?;
        if let Some(gid) = &self.gid {
            map.serialize_entry("gid", gid)?;
        }
        if let Some(sethash) = &self.sethash {
            map.serialize_entry("sethash", sethash)?;
        }
        map.serialize_entry("failcode", &self.failcode)?;
        map.serialize_entry("state", &self.state)?;
        map.serialize_entry("guid", &self.guid)?;
        map.serialize_entry("config", &self.config)?;
        map.serialize_entry("meta", &Value::Object(self.meta.clone()))?;
        map.serialize_entry(
            "reload_map",
            &self
                .reload_map
                .iter()
                .map(|(key, (url, name))| {
                    (
                        key.clone(),
                        Value::Array(vec![
                            Value::String(url.clone()),
                            Value::String(name.clone()),
                        ]),
                    )
                })
                .collect::<JsonMap<String, Value>>(),
        )?;
        map.serialize_entry("duplicate_map", &DuplicateMap(&self.duplicate_map))?;
        map.serialize_entry("renamed_map", &RenamedMap(&self.renamed_map))?;
        if let Some(queue) = &self.img_q {
            map.serialize_entry("img_q", &queue.iter().collect::<Vec<_>>())?;
        }
        if let Some(queue) = &self.page_q {
            map.serialize_entry("page_q", &queue.iter().collect::<Vec<_>>())?;
        }
        map.end()
    }
}

#[derive(Debug)]
pub struct TaskWireError(pub String);

impl std::fmt::Display for TaskWireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for TaskWireError {}

#[derive(Debug)]
pub enum TaskLoadError {
    Missing(&'static str),
    Shape(String),
}

impl std::fmt::Display for TaskLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing(key) => write!(f, "task is missing {key}"),
            Self::Shape(message) => write!(f, "invalid task wire: {message}"),
        }
    }
}

impl std::error::Error for TaskLoadError {}

pub(crate) fn value_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(number) => number.as_f64().is_some_and(|value| value != 0.0),
        Value::String(text) => !text.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}
