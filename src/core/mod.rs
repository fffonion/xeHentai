//! The application object and session handling from `xeHentai/core.py`.
//!
//! `App` owns the real task registry, the ready queue, cookies and headers.
//! It keeps the reference dict/queue order and the reference return codes; the
//! network login, proxy pool and RPC server arrive in later milestones and are
//! not pretended here.
#![allow(dead_code)]

pub(crate) mod runtime;

#[allow(unused_imports)]
pub use runtime::{AppHandle, DrainReport, Monitor, Runtime, TraceEvent, WorkerRef};

use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use indexmap::IndexMap;
use regex::Regex;
use serde::Serialize;
use serde::ser::{SerializeMap, Serializer};
use serde_json::{Map as JsonMap, Value};
use tokio::io::AsyncWriteExt;
use tokio::sync::watch;

use crate::config::Config;
use crate::r#const as consts;
use crate::filters::{self, Filters, PageResponse};
use crate::proxy::Pool;
use crate::task::{SaveStart, StagedSave, Task, TaskConfig, TaskError, TaskLoadError};
use crate::task::{boundary_path, normalize, path_within, resolve_dir};
use crate::util;
use crate::util::logger::Logger;
use crate::worker::{
    FetchedResponse, Session, SessionConfig, SpeedChecker, StreamBody, is_low_speed,
};

pub const SESSION_FILE: &str = "h.json";
pub const LEGACY_COOKIE_FILE: &str = ".ehentai.cookie";
/// Explicit read budget for the session file. Larger inputs are refused with
/// an error and the current state stays untouched; nothing is truncated.
pub const MAX_SESSION_BYTES: u64 = 8 * 1024 * 1024;

static SESSION_STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Test-only deterministic failures at the existing staging writer. The
/// production path is unchanged; tests use these to exercise cleanup on open,
/// write and flush errors without replacing the real request/filter/save code.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IoFault {
    Open,
    Write,
    Flush,
    /// Test-only: hold the blocking stage open until the test releases it.
    SlowOpen,
}

#[cfg(test)]
thread_local! {
    static IO_FAULT: std::cell::RefCell<Option<IoFault>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub(crate) fn set_io_fault(fault: Option<IoFault>) {
    IO_FAULT.with(|slot| *slot.borrow_mut() = fault);
}

#[cfg(test)]
fn io_fault() -> Option<IoFault> {
    IO_FAULT.with(|slot| *slot.borrow())
}

/// Test-only gate that holds the blocking stage open mid-flight so a
/// cancellation can be delivered while the queued filesystem open is still
/// pending. Production never arms this.
#[cfg(test)]
pub(crate) struct SlowOpenHandle {
    pub started: std::sync::mpsc::Receiver<()>,
    pub finished: std::sync::mpsc::Receiver<()>,
    pub release: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
struct SlowOpenChannels {
    started: std::sync::mpsc::Sender<()>,
    release: std::sync::mpsc::Receiver<()>,
    finished: std::sync::mpsc::Sender<()>,
}

#[cfg(test)]
static SLOW_OPEN: Mutex<Option<SlowOpenChannels>> = Mutex::new(None);

#[cfg(test)]
pub(crate) fn arm_slow_open() -> SlowOpenHandle {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (finished_tx, finished_rx) = std::sync::mpsc::channel();
    *SLOW_OPEN.lock().unwrap() = Some(SlowOpenChannels {
        started: started_tx,
        release: release_rx,
        finished: finished_tx,
    });
    SlowOpenHandle {
        started: started_rx,
        finished: finished_rx,
        release: release_tx,
    }
}

#[cfg(test)]
fn take_slow_open() -> Option<SlowOpenChannels> {
    SLOW_OPEN.lock().unwrap().take()
}

/// Owns one staged download from `begin_staged_save` through the final RSS
/// decision and publish. Any `?`/drop/cancellation before `commit` removes only
/// this stage, leaves the target and task bookkeeping untouched and closes the
/// native response stream.
struct DownloadStage {
    staged: Option<StagedSave>,
    stream: Option<StreamBody>,
    canceled: Arc<AtomicBool>,
}

impl DownloadStage {
    fn new(staged: StagedSave, stream: StreamBody) -> Self {
        Self {
            staged: Some(staged),
            stream: Some(stream),
            canceled: Arc::new(AtomicBool::new(false)),
        }
    }

    fn staged_ref(&self) -> &StagedSave {
        self.staged.as_ref().expect("staged save is armed")
    }

    fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.canceled)
    }

    fn stream_mut(&mut self) -> &mut StreamBody {
        self.stream.as_mut().expect("response stream is armed")
    }

    /// Take the staged save for a port-owned publish. The stream stays owned
    /// by the stage and is closed when the stage drops right after commit.
    fn staged_take(&mut self) -> StagedSave {
        self.staged.take().expect("staged save is armed")
    }

    /// Publish exactly once. Taking the stage disarms the drop cleanup only
    /// for the successful publish itself; `commit_staged_save` removes the
    /// stage when the publish fails.
    fn commit(mut self, task: &mut Task) -> Result<PathBuf, TaskError> {
        let staged = self.staged.take().expect("staged save is armed");
        task.commit_staged_save(staged)
    }
}

impl Drop for DownloadStage {
    fn drop(&mut self) {
        self.canceled.store(true, Ordering::SeqCst);
        if let Some(staged) = self.staged.take() {
            let _ = fs::remove_file(&staged.stage);
        }
        if let Some(stream) = self.stream.take() {
            drop(stream);
        }
    }
}

/// Tracks filesystem opens that are already queued on the blocking pool.
/// A pause can wait for these owners to retire instead of claiming quiescence
/// while an OS open is still in flight (an OS open itself cannot be aborted).
///
/// It also owns the RPC media layer's native workers. The collection only
/// ever holds records: each record's `JoinHandle` is awaited IN PLACE under
/// its own async mutex, so a cancelled observation drops only its guard and
/// never hides or detaches the worker from later drains.
#[derive(Clone, Default)]
pub struct StageDrain {
    count: Arc<AtomicUsize>,
    notify: Arc<tokio::sync::Notify>,
    native: Arc<Mutex<Vec<Arc<NativeSlot>>>>,
}

/// One owned native RPC worker. `joined` becomes true only after the actual
/// `JoinHandle` resolved to Ready; a worker return or channel send is not
/// join evidence. Records are removed only after that actual join.
pub(crate) struct NativeSlot {
    handle: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
    joined: AtomicBool,
}

impl NativeSlot {
    /// Await this record's own handle without taking it out. Dropping the
    /// guard (cancelled observer) leaves the handle visible for the next
    /// observer; awaiting a second time is safe because the handle never
    /// moves.
    async fn wait_joined(&self) {
        if self.joined.load(Ordering::SeqCst) {
            return;
        }
        let mut guard = self.handle.lock().await;
        if self.joined.load(Ordering::SeqCst) {
            return;
        }
        if let Some(handle) = guard.as_mut() {
            let _ = handle.await;
        }
        self.joined.store(true, Ordering::SeqCst);
    }

    /// Reap a record whose worker already finished, without waiting for an
    /// active worker. Returns true when this observation actually joined it.
    async fn join_if_finished(&self) -> bool {
        if self.joined.load(Ordering::SeqCst) {
            return true;
        }
        let Ok(mut guard) = self.handle.try_lock() else {
            return false;
        };
        if self.joined.load(Ordering::SeqCst) {
            return true;
        }
        match guard.as_mut() {
            Some(handle) if handle.is_finished() => {
                let _ = handle.await;
                self.joined.store(true, Ordering::SeqCst);
                true
            }
            _ => false,
        }
    }
}

/// One submitted native job. The typed result travels through a oneshot; the
/// owner awaits the result and then joins the SAME owner-held handle. Both
/// steps are cancellation-safe: the record stays until an actual join.
pub(crate) struct NativeHandle<T> {
    drain: StageDrain,
    slot: Arc<NativeSlot>,
    result: tokio::sync::oneshot::Receiver<T>,
}

impl<T> NativeHandle<T> {
    /// Receive the typed result, then join the actual worker. A channel
    /// failure (panic before send) still performs the real join.
    pub(crate) async fn join(self) -> Result<T, NativeJoinError> {
        let result = self.result.await;
        self.slot.wait_joined().await;
        self.drain.reap_native();
        result.map_err(|_| NativeJoinError)
    }
}

/// The native worker ended without delivering a typed result (panic or
/// dropped sender); the actual join still happened before this is returned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NativeJoinError;

pub struct StageGuard {
    drain: StageDrain,
}

impl StageDrain {
    pub fn enter(&self) -> StageGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        StageGuard {
            drain: self.clone(),
        }
    }

    pub fn pending(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    pub async fn wait_zero(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.count.load(Ordering::SeqCst) == 0 {
                return;
            }
            notified.await;
        }
    }

    /// Spawn one owned native job and keep its handle in this drain. Must be
    /// called synchronously by the admission section (no await between the
    /// caller's check and this registration). `undelivered` receives the
    /// typed result when the owner's receiver is already gone, so an owned
    /// result (for example a private archive stage) can still be discarded
    /// explicitly.
    pub(crate) fn spawn_native<T, F, D>(&self, job: F, undelivered: D) -> NativeHandle<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(T) + Send + 'static,
    {
        let (sender, result) = tokio::sync::oneshot::channel();
        let slot = Arc::new(NativeSlot {
            handle: tokio::sync::Mutex::new(None),
            joined: AtomicBool::new(false),
        });
        let handle = tokio::task::spawn_blocking(move || {
            let outcome = job();
            if let Err(outcome) = sender.send(outcome) {
                undelivered(outcome);
            }
        });
        // The slot is not published yet, so this try_lock is uncontended and
        // cannot block the LocalSet thread.
        {
            let mut owned = slot.handle.try_lock().expect("fresh native slot");
            *owned = Some(handle);
        }
        self.native
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Arc::clone(&slot));
        NativeHandle {
            drain: self.clone(),
            slot,
            result,
        }
    }

    fn native_snapshot(&self) -> Vec<Arc<NativeSlot>> {
        self.native
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Number of native records whose actual join has not been observed yet.
    pub(crate) fn native_pending(&self) -> usize {
        self.native
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }

    /// Remove only records already joined by an actual `JoinHandle` Ready.
    fn reap_native(&self) -> usize {
        let mut native = self
            .native
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let before = native.len();
        native.retain(|slot| !slot.joined.load(Ordering::SeqCst));
        before - native.len()
    }

    /// Join records whose workers already retired, then reap them. This keeps
    /// the record vector bounded during ordinary traffic, including requests
    /// abandoned by their observer.
    pub(crate) async fn reap_native_finished(&self) -> usize {
        let mut joined = 0;
        for slot in self.native_snapshot() {
            if slot.join_if_finished().await {
                joined += 1;
            }
        }
        self.reap_native();
        joined
    }

    /// Await every owned native worker's actual retirement. Records admitted
    /// while this drain is running (a live server pause) are observed by the
    /// next round; cancelled observers only drop their own guard.
    pub(crate) async fn wait_native(&self) -> usize {
        let mut joined = 0;
        loop {
            let snapshot = self.native_snapshot();
            if snapshot.is_empty() {
                return joined;
            }
            for slot in &snapshot {
                if !slot.joined.load(Ordering::SeqCst) {
                    joined += 1;
                }
                slot.wait_joined().await;
            }
            self.reap_native();
        }
    }
}

impl Drop for StageGuard {
    fn drop(&mut self) {
        self.drain.count.fetch_sub(1, Ordering::SeqCst);
        self.drain.notify.notify_waiters();
    }
}

/// Open the staging file on the blocking pool. If the surrounding download is
/// cancelled while the open is still queued, the cancellation flag makes the
/// blocking side remove the just-created path, so cleanup cannot be followed by
/// a recreated stage. An optional drain guard keeps pause truthful while the
/// uninterruptible OS open is still running.
async fn create_stage_file(
    staged: &StagedSave,
    canceled: Arc<AtomicBool>,
    drain: Option<StageDrain>,
) -> std::io::Result<tokio::fs::File> {
    #[cfg(test)]
    if io_fault() == Some(IoFault::Open) {
        return Err(std::io::Error::other("injected open failure"));
    }
    let path = staged.stage.clone();
    #[cfg(test)]
    let slow = if io_fault() == Some(IoFault::SlowOpen) {
        take_slow_open()
    } else {
        None
    };
    let guard = drain.as_ref().map(StageDrain::enter);
    let std_file = tokio::task::spawn_blocking(move || -> std::io::Result<std::fs::File> {
        let _guard = guard;
        #[cfg(test)]
        if let Some(channels) = &slow {
            let _ = channels.started.send(());
            // Long enough for the supervisor regression to observe a pending
            // drain; every existing test releases the open explicitly.
            let _ = channels
                .release
                .recv_timeout(std::time::Duration::from_secs(60));
        }
        let outcome = (|| -> std::io::Result<std::fs::File> {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)?;
            if canceled.load(Ordering::SeqCst) {
                drop(file);
                let _ = std::fs::remove_file(&path);
                return Err(std::io::Error::new(
                    std::io::ErrorKind::Interrupted,
                    "download cancelled",
                ));
            }
            Ok(file)
        })();
        #[cfg(test)]
        if let Some(channels) = &slow {
            let _ = channels.finished.send(());
        }
        outcome
    })
    .await
    .map_err(std::io::Error::other)??;
    Ok(tokio::fs::File::from_std(std_file))
}

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
    Filter(String),
    Http(String),
    Download(String),
    State(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Task(error) => write!(f, "{error}"),
            Self::UnknownConfig(key) => write!(f, "unknown global configuration key {key}"),
            Self::Unavailable(what) => write!(f, "not implemented in this milestone: {what}"),
            Self::Session(error) => write!(f, "{error}"),
            Self::Filter(message) => write!(f, "filter error: {message}"),
            Self::Http(message) => write!(f, "http error: {message}"),
            Self::Download(message) => write!(f, "download error: {message}"),
            Self::State(message) => write!(f, "task state error: {message}"),
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
    /// Shared proxy pool, created on the first non-empty proxy update.
    proxy: Option<Arc<Mutex<Pool>>>,
    /// The native session reused by task steps; a header/login change drops it
    /// so the next request is prepared from the updated App headers.
    session: Option<Session>,
    /// Transport settings used when a session is created. Production keeps the
    /// default; local protocol tests inject their test CA/DNS mapping here.
    session_config: SessionConfig,
    /// The live private persistence spellings every task read is checked
    /// against: the session file, `config.rss`, the legacy `config.py`, the
    /// resolved `log_path` and the legacy cookie file. `set_config` replaces it
    /// BEFORE the logger is reopened, so a pathname that becomes private at
    /// runtime is protected from that moment on while older snapshots keep
    /// their own captured names.
    private_paths: Arc<crate::task::PrivatePaths>,
}

#[derive(Debug)]
pub enum ImageScan {
    Empty,
    Enqueued {
        image_url: String,
        filename: String,
    },
    Failed {
        fail_code: i64,
        real_url: String,
        url: String,
    },
    Exception {
        kind: String,
    },
}

#[derive(Debug)]
pub enum ImageDownload {
    Empty,
    AlreadyDone(PathBuf),
    Saved(PathBuf),
    Failed {
        fail_code: i64,
        real_url: String,
        url: String,
    },
    Aborted,
}

/// Immutable inputs captured when a download worker takes one queue item.
pub(crate) struct DownloadItem {
    pub image_url: String,
    pub policy: util::ProxyPolicy,
    pub token: (u64, u64),
    pub timeout: i64,
    pub low_speed_threshold: i64,
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
            .field("proxy", &self.proxy)
            .field("session", &self.session.is_some())
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
        // Initialized BEFORE any task/session admission, so a task restored or
        // added below already reads under the current policy.
        let private_paths = crate::task::PrivatePaths::for_app(base_dir, filepath, &cfg.log_path);
        // The logger this App was handed may already hold a real log file
        // (the CLI opens `log_path` before constructing the App). Publish its
        // actual descriptor identity before any task can read; the getter
        // failure is recorded as Unobservable so the policy fails closed.
        private_paths.publish_active_log(Self::active_private_log(&logger));
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
            proxy: None,
            session: None,
            session_config: SessionConfig::default(),
            private_paths,
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

    /// Test/compat fixture entry: insert an already-built Task and queue it,
    /// preserving the normal registry order. Production tasks enter through
    /// `add_task`; this exists so integration tests can point a real task at a
    /// local server URL that `add_task` would reject.
    #[cfg(test)]
    pub fn insert_task_for_test(&mut self, task: Task) -> String {
        let guid = task.guid.clone();
        self.all_tasks.insert(guid.clone(), task);
        self.tasks.push_back(guid.clone());
        guid
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

    /// The identity of the log file the logger actually holds, as understood
    /// by the bounded private-policy cell. A failed metadata probe is
    /// `Unobservable` (fail closed), never a fabricated descriptor identity.
    fn active_private_log(logger: &Logger) -> crate::task::ActivePrivateLog {
        use crate::task::ActivePrivateLog;
        match logger.held_file_identity() {
            Ok(Some(identity)) => ActivePrivateLog::Held(identity),
            Ok(None) => ActivePrivateLog::None,
            Err(_) => ActivePrivateLog::Unobservable,
        }
    }

    pub fn filepath(&self) -> &Path {
        &self.filepath
    }

    pub fn authorized_dir(&self) -> &Path {
        &self.authorized_dir
    }

    /// Parts the owned M5 server start binds from: interface, optional port,
    /// optional secret, cache directory, resource root and clock. Stored
    /// values only; binding (and its real errors) happens in the runtime.
    pub(crate) fn rpc_bind_parts(
        &self,
        resource_root: &Path,
        clock: Arc<dyn crate::worker::HttpClock>,
    ) -> (
        String,
        Option<i64>,
        Option<String>,
        PathBuf,
        PathBuf,
        Arc<dyn crate::worker::HttpClock>,
    ) {
        (
            self.cfg.rpc_interface.clone(),
            self.cfg.rpc_port,
            self.cfg.rpc_secret.clone(),
            self.base_dir.clone(),
            resource_root.to_path_buf(),
            clock,
        )
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
        self.session = None;
        (consts::ERR_NO_ERROR, None)
    }

    /// Internal configuration update. Unlike the M2 placeholder, `rpc_port`
    /// is accepted and stored here: the M5 owned server start binds the
    /// listener through the runtime (`rpc::server::serve_until_closed`), so
    /// bind errors are real and the server is never marked started without
    /// a listener. RPC `updateConfig` itself still filters `rpc_*` keys.
    pub fn update_config(&mut self, patch: JsonMap<String, Value>) -> Result<(), AppError> {
        // `rpc_port` is stored like any other key; binding happens in the
        // owned server start, not in this synchronous patch.
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
                    let mut addresses = Vec::with_capacity(items.len());
                    for item in items {
                        let address = item.as_str().ok_or_else(|| {
                            AppError::UnknownConfig("proxy entries must be strings".into())
                        })?;
                        addresses.push(address.to_string());
                    }
                    self.cfg.proxy = addresses.clone();
                    if !addresses.is_empty() {
                        let pool = self
                            .proxy
                            .get_or_insert_with(|| Arc::new(Mutex::new(Pool::new())));
                        let mut pool = pool.lock().unwrap();
                        for (index, address) in addresses.iter().enumerate() {
                            if let Err(error) = pool.add_proxy(address) {
                                // The configured spelling can hold userinfo or
                                // a legacy Glype rewrite query, and some error
                                // payloads echo the address, so neither is
                                // logged: the position plus a closed class is
                                // enough to act on without leaking a secret.
                                let message = format!(
                                    "proxy #{} rejected: class={}",
                                    index + 1,
                                    error.class()
                                );
                                let _ = self.logger.warning(&message);
                            }
                        }
                    }
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
        let log_path = crate::config::resolve_log_path(&self.base_dir, &self.cfg.log_path);
        // Update the live private-path cell BEFORE the logger is reopened:
        // the new name becomes private state before the first byte can be
        // written to it, while the captured names of an in-flight read keep
        // protecting the previous spelling. A patch that fails earlier returns
        // above without touching either the cell or the old logger.
        self.private_paths
            .replace_for_app(&self.base_dir, &self.filepath, &self.cfg.log_path);
        let reopened = self.logger.set_logfile(&log_path);
        // Publish the descriptor the logger now holds before any other private
        // log write. A failed reopen leaves the logger with no file, so the
        // truthful state is None, never a fabricated identity.
        self.private_paths
            .publish_active_log(Self::active_private_log(&self.logger));
        if let Err(error) = reopened {
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
        let task = Task::new_with_private_paths(
            url,
            config,
            guid.clone(),
            &self.base_dir,
            Arc::clone(&self.private_paths),
        )?;
        // A task argument is an explicit user choice and keeps its directory
        // usable for the lifetime of this App, even if the global dir changes.
        self.trust_dir(&task_root);
        if let Some(existing) = self.all_tasks.get_mut(&guid) {
            if existing.state == consts::TASK_STATE_FINISHED
                || existing.state == consts::TASK_STATE_FAILED
            {
                existing.state = consts::TASK_STATE_WAITING;
                existing.bump_run();
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
        self.all_tasks.get_mut(guid).expect("checked").bump_run();
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
        task.bump_run();
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
        task.bump_run();
        self.tasks.push_back(guid.to_string());
        Ok((consts::ERR_NO_ERROR, Some(String::new())))
    }

    /// Replace the settings used for the next native session. Local protocol
    /// tests use this to inject a test CA/DNS map; production does not call it.
    pub fn set_session_config(&mut self, config: SessionConfig) {
        self.session_config = config;
        self.session = None;
    }

    /// Update the process exit marker (`XEH_STATE_*`). The runtime and later
    /// signal/CLI owners use it; plain `App::new` never spawns anything.
    pub fn set_exit(&mut self, state: i64) {
        self.exit = state;
    }

    /// Request exit and return the previous value.
    pub fn request_exit(&mut self, state: i64) -> i64 {
        let previous = self.exit;
        self.exit = state;
        previous
    }

    /// A fresh native session built from the current App headers, login
    /// cookies, shared proxy pool and injected transport config. Each worker
    /// owns one of these for its whole life, like one `requests.Session`.
    pub fn new_session(&self, policy: util::ProxyPolicy) -> Session {
        let headers = self
            .headers
            .iter()
            .map(|header| (header.name.clone(), header.value.clone()))
            .collect();
        let mut session = Session::new(
            headers,
            self.proxy.clone(),
            policy,
            self.session_config.clone(),
        );
        session.set_logger(self.logger.clone());
        session
    }

    /// Move the App-level session out so a sequential call can borrow `self`
    /// through short callbacks while awaiting network I/O, then put it back.
    pub fn take_http_session(&mut self, policy: util::ProxyPolicy) -> Session {
        let mut session = self
            .session
            .take()
            .unwrap_or_else(|| self.new_session(policy));
        session.proxy_policy = policy;
        session
    }

    pub fn restore_http_session(&mut self, session: Session) {
        self.session = Some(session);
    }

    /// Pop the next queued guid and maintain `last_task_guid` exactly like
    /// `_task_loop`: the previously handled guid is recorded before the new
    /// one becomes current.
    pub fn pop_queued_task(&mut self, current: &mut Option<String>) -> Option<String> {
        let guid = self.tasks.pop_front()?;
        self.last_task_guid = current.take();
        *current = Some(guid.clone());
        Some(guid)
    }

    pub fn push_task_back(&mut self, guid: &str) {
        self.tasks.push_back(guid.to_string());
    }

    pub fn task_token(&self, guid: &str) -> Option<(u64, u64)> {
        self.all_tasks.get(guid).map(Task::token)
    }

    /// True while the exact task object/run a worker captured is still the
    /// live run and can be mutated. Paused, failed, finished, deleted and
    /// same-guid-new-task states all reject the late callback.
    pub fn task_is_current(&self, guid: &str, token: (u64, u64)) -> bool {
        self.task_run_matches(guid, token)
            && self
                .all_tasks
                .get(guid)
                .is_some_and(|task| is_runnable_state(task.state))
    }

    /// Identity-only half of [`Self::task_is_current`]: the same task object
    /// and run marker, without the worker-state window. Archive admission and
    /// its state/publication effects happen at MAKE_ARCHIVE/FINISHED, which
    /// the worker-only runnable predicate deliberately rejects.
    pub fn task_run_matches(&self, guid: &str, token: (u64, u64)) -> bool {
        self.exit < consts::XEH_STATE_FULL_EXIT
            && self
                .all_tasks
                .get(guid)
                .is_some_and(|task| task.token() == token)
    }

    pub fn task_state(&self, guid: &str) -> Option<i64> {
        self.all_tasks.get(guid).map(|task| task.state)
    }

    /// Enter the first real task state (`_do_task`'s WAITING -> GET_META)
    /// without bringing up the M4 scheduler.
    pub fn start_task(&mut self, guid: &str) -> Result<(), AppError> {
        self.ensure_task_queues(guid)?;
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        if task.state == consts::TASK_STATE_WAITING {
            task.state = consts::TASK_STATE_GET_META;
        }
        Ok(())
    }

    /// The shared proxy pool once configuration supplied at least one proxy.
    pub fn proxy_pool(&self) -> Option<Arc<Mutex<Pool>>> {
        self.proxy.clone()
    }

    /// Health/test access: number of pooled proxies and whether any is
    /// available (the reference's inverted `has_available_proxies` lives on
    /// [`Pool`] itself).
    pub fn proxy_count(&self) -> usize {
        self.proxy
            .as_ref()
            .map(|pool| pool.lock().unwrap().proxies.len())
            .unwrap_or(0)
    }

    /// The one native session reused across task steps. Header/login changes
    /// drop it first, which is exactly when the reference would construct a
    /// new `HttpReq`.
    pub fn http_session(&mut self, policy: util::ProxyPolicy) -> &mut Session {
        if self.session.is_none() {
            let headers = self
                .headers
                .iter()
                .map(|header| (header.name.clone(), header.value.clone()))
                .collect();
            let mut session = Session::new(
                headers,
                self.proxy.clone(),
                policy,
                self.session_config.clone(),
            );
            session.set_logger(self.logger.clone());
            self.session = Some(session);
        }
        let session = self.session.as_mut().expect("session");
        session.proxy_policy = policy;
        session
    }

    pub fn ensure_task_queues(&mut self, guid: &str) -> Result<(), AppError> {
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        if task.page_q.is_none() {
            task.page_q = Some(VecDeque::new());
        }
        if task.img_q.is_none() {
            task.img_q = Some(VecDeque::new());
        }
        Ok(())
    }

    fn page_view(response: &FetchedResponse, ori: bool, has_stream: bool) -> PageResponse {
        PageResponse {
            status: response.status,
            url: response.url.clone(),
            real_url: response.real_url.clone(),
            content_length: response.content_length,
            text: response.text.clone(),
            headers: response.business_headers(),
            ori,
            has_stream,
        }
    }

    /// RPC-visible response projection for the login flow. Same view the
    /// workers use, without moving worker ownership into the RPC module.
    pub(crate) fn page_view_for_rpc(
        response: &FetchedResponse,
        ori: bool,
        has_stream: bool,
    ) -> PageResponse {
        Self::page_view(response, ori, has_stream)
    }

    /// Synthetic login against the real login endpoint. The RSS filter parses
    /// the Set-Cookie header; only its typed outcome updates App state.
    pub async fn login_exhentai(
        &mut self,
        filters: &mut Filters,
        username: &str,
        password: &str,
    ) -> Result<filters::LoginOutcome, AppError> {
        if self.has_login {
            return Ok(filters::LoginOutcome {
                cookies: Some(self.cookies.clone()),
                ..Default::default()
            });
        }
        let data = vec![
            ("UserName".to_string(), username.to_string()),
            ("returntype".to_string(), "8".to_string()),
            ("CookieDate".to_string(), "1".to_string()),
            ("b".to_string(), "d".to_string()),
            ("bt".to_string(), "pone".to_string()),
            ("PassWord".to_string(), password.to_string()),
        ];
        let policy = util::get_proxy_policy(self.cfg.proxy_image_only, self.cfg.proxy_image);
        let session = self.http_session(policy);
        let outcome = session
            .request(
                "POST",
                "https://forums.e-hentai.org/index.php?act=Login&CODE=01",
                Some(data),
                false,
            )
            .await
            .map_err(|error| AppError::Http(error.to_string()))?;
        let view = Self::page_view(&outcome.response, false, false);
        let login = filters
            .login_outcome(&view)
            .map_err(|error| AppError::Filter(error.to_string()))?;
        if let Some(cookies) = &login.cookies {
            self.cookies = cookies.clone();
            self.set_cookie_header();
            self.update_login_flag();
            self.session = None;
            let _ = self.save_session();
            let message = format!("login ok: {}", self.has_login);
            let _ = self.logger.info(&message);
        } else if let Some(message) = &login.fail_message {
            let _ = self.logger.warning(message);
        } else if let Some(kind) = &login.exception {
            return Err(AppError::Filter(format!("login raised {kind}")));
        }
        Ok(login)
    }

    /// One `TASK_STATE_GET_META` step: real request -> RSS metadata -> the
    /// existing `Task::update_meta`/`set_fail` effects.
    pub async fn fetch_metadata(
        &mut self,
        guid: &str,
        filters: &mut Filters,
    ) -> Result<bool, AppError> {
        let (url, state, policy) = {
            let task = self
                .all_tasks
                .get(guid)
                .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
            (
                task.url.clone(),
                task.state,
                util::get_proxy_policy(task.config.proxy_image_only, task.config.proxy_image),
            )
        };
        if state != consts::TASK_STATE_GET_META {
            return Err(AppError::State(format!("task {guid} is not in GET_META")));
        }
        let session = self.http_session(policy);
        let outcome = session
            .request("GET", &url, None, false)
            .await
            .map_err(|error| AppError::Http(error.to_string()))?;
        let view = Self::page_view(&outcome.response, false, false);
        let parsed = filters
            .metadata_outcome(&view)
            .map_err(|error| AppError::Filter(error.to_string()))?;
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        if let Some(code) = parsed.fail_code {
            task.set_fail(code);
            if let Some(ban) = parsed.ban {
                let message = format!("ip banned for {ban}; task {guid}");
                let _ = self.logger.warning(&message);
            }
        } else if let Some(kind) = parsed.exception {
            return Err(AppError::Filter(format!("metadata raised {kind}")));
        } else if let Some(meta) = parsed.meta {
            let mut map = JsonMap::new();
            map.insert("gjname".into(), Value::String(meta.gjname));
            map.insert("gnname".into(), Value::String(meta.gnname));
            map.insert("total".into(), Value::Number(meta.total.into()));
            map.insert("finished".into(), Value::Number(meta.finished.into()));
            map.insert(
                "tags".into(),
                Value::Array(meta.tags.into_iter().map(Value::String).collect()),
            );
            map.insert(
                "thumbnail_cnt".into(),
                Value::Number(meta.thumbnail_cnt.into()),
            );
            map.insert(
                "use_multipage_viewer".into(),
                Value::Bool(meta.use_multipage_viewer),
            );
            task.update_meta(map)?;
            task.state = consts::TASK_STATE_SCAN_PAGE;
        }
        Ok(true)
    }

    /// One `TASK_STATE_SCAN_PAGE` step. This performs the reference's page
    /// loop for this state (the parallel worker wrapper remains milestone M4).
    pub async fn scan_pages(&mut self, guid: &str, filters: &mut Filters) -> Result<(), AppError> {
        let (url, state, policy) = {
            let task = self
                .all_tasks
                .get(guid)
                .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
            (
                task.url.clone(),
                task.state,
                util::get_proxy_policy(task.config.proxy_image_only, task.config.proxy_image),
            )
        };
        if state != consts::TASK_STATE_SCAN_PAGE {
            return Err(AppError::State(format!("task {guid} is not in SCAN_PAGE")));
        }
        {
            let task = self
                .all_tasks
                .get_mut(guid)
                .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
            task.scan_downloaded()?;
            if task.state == consts::TASK_STATE_FINISHED {
                return Ok(());
            }
        }
        let (use_mpv, total, thumbnails, finished, mpv_url) = {
            let task = self
                .all_tasks
                .get(guid)
                .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
            (
                task.meta
                    .get("use_multipage_viewer")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
                task.meta_i64("total")?,
                task.meta_i64("thumbnail_cnt")?,
                task.meta_i64("finished")?,
                task.mpv_url(),
            )
        };
        let mut failed = false;
        if !use_mpv {
            if thumbnails <= 0 {
                return Err(AppError::State("thumbnail_cnt must be positive".into()));
            }
            let pages = (total + thumbnails - 1) / thumbnails;
            for page_index in 0..pages {
                let page_url = format!("{url}/?p={page_index}");
                let session = self.http_session(policy);
                let outcome = session
                    .request("GET", &page_url, None, false)
                    .await
                    .map_err(|error| AppError::Http(error.to_string()))?;
                let view = Self::page_view(&outcome.response, false, false);
                let parsed = filters
                    .pageurl_outcome(&view)
                    .map_err(|error| AppError::Filter(error.to_string()))?;
                if let Some(body) = &parsed.debug_text {
                    // The reference writes debug.htm in the process cwd and
                    // swallows write errors. base_dir is the authorized root.
                    let _ = fs::write(self.base_dir.join("debug.htm"), body);
                }
                {
                    let task = self
                        .all_tasks
                        .get_mut(guid)
                        .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
                    for page in &parsed.urls {
                        task.put_page_queue(page)?;
                    }
                    if let Some(code) = parsed.fail_code {
                        task.set_fail(code);
                        failed = true;
                    }
                }
                if parsed.exception.is_some() || failed {
                    break;
                }
            }
        } else if finished < total {
            let session = self.http_session(policy);
            let outcome = session
                .request("GET", &mpv_url, None, false)
                .await
                .map_err(|error| AppError::Http(error.to_string()))?;
            let view = Self::page_view(&outcome.response, false, false);
            let parsed = filters
                .pageurl_mpv_outcome(&view)
                .map_err(|error| AppError::Filter(error.to_string()))?;
            let task = self
                .all_tasks
                .get_mut(guid)
                .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
            for page in &parsed.urls {
                task.put_page_queue(page)?;
            }
            if let Some(code) = parsed.fail_code {
                task.set_fail(code);
                failed = true;
            }
            if parsed.exception.is_some() {
                failed = true;
            }
        }
        if !failed {
            let task = self
                .all_tasks
                .get_mut(guid)
                .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
            if task.state != consts::TASK_STATE_FAILED {
                task.state = consts::TASK_STATE_SCAN_IMG;
            }
        }
        Ok(())
    }

    /// One real page-queue consumer: request -> RSS `imgurl` -> existing
    /// `Task::put_img_queue`.
    pub async fn scan_one_page(
        &mut self,
        guid: &str,
        filters: &mut Filters,
    ) -> Result<ImageScan, AppError> {
        self.ensure_task_queues(guid)?;
        let (page_url, policy, ori) = {
            let task = self
                .all_tasks
                .get_mut(guid)
                .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
            let Some(page_url) = task.page_q.as_mut().and_then(|queue| queue.pop_front()) else {
                return Ok(ImageScan::Empty);
            };
            (
                page_url,
                util::get_proxy_policy(task.config.proxy_image_only, task.config.proxy_image),
                task.config.download_ori && self.has_login,
            )
        };
        let session = self.http_session(policy);
        let outcome = session
            .request("GET", &page_url, None, false)
            .await
            .map_err(|error| AppError::Http(error.to_string()))?;
        let view = Self::page_view(&outcome.response, ori, false);
        let parsed = match filters.imgurl_outcome(&view) {
            Ok(parsed) => parsed,
            Err(error) => {
                // HttpWorker catches a filter exception and re-runs the filter
                // on _FakeResponse(url): the quota wrapper turns that into
                // ERR_CONNECTION_ERROR without killing the worker.
                let fake = PageResponse {
                    status: 600,
                    url: page_url.clone(),
                    real_url: page_url.clone(),
                    content_length: 0,
                    text: String::new(),
                    headers: Vec::new(),
                    ori,
                    has_stream: false,
                };
                let fallback = filters
                    .imgurl_outcome(&fake)
                    .map_err(|_| AppError::Filter(error.to_string()))?;
                let _ = self.logger.warning(&format!("scan filter error: {error}"));
                let code = fallback.fail_code.unwrap_or(consts::ERR_CONNECTION_ERROR);
                return Ok(ImageScan::Failed {
                    fail_code: code,
                    real_url: page_url.clone(),
                    url: page_url,
                });
            }
        };
        if let Some(code) = parsed.fail_code {
            return Ok(ImageScan::Failed {
                fail_code: code,
                real_url: parsed.real_url.unwrap_or_else(|| page_url.clone()),
                url: parsed.url.unwrap_or_else(|| page_url.clone()),
            });
        }
        if let Some(kind) = parsed.exception {
            return Ok(ImageScan::Exception { kind });
        }
        let (Some(image_url), Some(reload_url), Some(filename)) =
            (parsed.image_url, parsed.reload_url, parsed.filename)
        else {
            return Ok(ImageScan::Exception {
                kind: "missing success fields".into(),
            });
        };
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        task.put_img_queue(&image_url, &reload_url, &filename)?;
        Ok(ImageScan::Enqueued {
            image_url,
            filename,
        })
    }
}

#[derive(Debug)]
pub enum CommitOutcome {
    Published(PathBuf),
    Rejected,
}

/// The short, borrow-scoped half of one download. Implemented for the direct
/// `App::download_one_image` call and for a worker sharing the live Task
/// through `Rc<RefCell<App>>`; no variant holds a borrow across an await.
pub(crate) trait DownloadPort {
    fn validate(&self) -> bool;
    fn begin_save(&mut self, imgurl: &str, redirect_url: &str) -> Result<SaveStart, TaskError>;
    fn retry_page(&mut self, url: &str) -> Result<bool, TaskError>;
    fn commit(&mut self, staged: StagedSave) -> Result<CommitOutcome, TaskError>;
    fn warn_download(&mut self, message: &str);
}

pub(crate) struct DirectPort<'a> {
    pub task: &'a mut Task,
    pub logger: &'a mut Logger,
    pub token: (u64, u64),
}

impl DownloadPort for DirectPort<'_> {
    fn validate(&self) -> bool {
        self.task.token() == self.token && is_runnable_state(self.task.state)
    }

    fn begin_save(&mut self, imgurl: &str, redirect_url: &str) -> Result<SaveStart, TaskError> {
        if !self.validate() {
            return Err(TaskError::QueueNotStarted("stale download"));
        }
        self.task.begin_staged_save(imgurl, redirect_url)
    }

    fn retry_page(&mut self, url: &str) -> Result<bool, TaskError> {
        if !self.validate() {
            return Ok(false);
        }
        self.task.put_page_queue_retry(url)?;
        Ok(true)
    }

    fn commit(&mut self, staged: StagedSave) -> Result<CommitOutcome, TaskError> {
        if !self.validate() {
            Task::abort_staged_save(staged);
            return Ok(CommitOutcome::Rejected);
        }
        Ok(CommitOutcome::Published(
            self.task.commit_staged_save(staged)?,
        ))
    }

    fn warn_download(&mut self, message: &str) {
        let _ = self.logger.warning(message);
    }
}

/// Per-item download runtime inputs. `speed`/`low_speed_threshold` are only
/// set for the streaming download worker; the direct API passes none.
pub(crate) struct DownloadRuntime<'a> {
    pub speed: Option<Arc<Mutex<SpeedChecker>>>,
    pub low_speed_threshold: i64,
    pub cancel: Option<&'a mut watch::Receiver<bool>>,
    pub stage_drain: Option<StageDrain>,
}

fn fake_page(url: &str) -> PageResponse {
    PageResponse {
        status: 600,
        url: url.to_string(),
        real_url: url.to_string(),
        content_length: 0,
        text: String::new(),
        headers: Vec::new(),
        ori: false,
        has_stream: false,
    }
}

fn is_html_error_body(prefix: &[u8]) -> bool {
    let text = String::from_utf8_lossy(prefix);
    let start = text
        .trim_start_matches(|character: char| {
            character.is_ascii_whitespace() || character == '\u{feff}'
        })
        .chars()
        .take(96)
        .collect::<String>()
        .to_ascii_lowercase();
    [
        "<!doctype html",
        "<html",
        "<head",
        "<body",
        "<title",
        "<iframe",
        "<meta",
        "<script",
    ]
    .iter()
    .any(|marker| start.starts_with(marker))
}

fn stream_diagnostic(kind: &str, received: i64, expected: Option<i64>) -> String {
    let expected = expected
        .map(|length| length.to_string())
        .unwrap_or_else(|| "unknown".into());
    format!(
        "image stream rejected: kind={kind} received_bytes={received} expected_bytes={expected}"
    )
}

fn download_failed(
    filters: &mut Filters,
    image_url: &str,
    response: &PageResponse,
    ensure_retry: bool,
) -> Result<ImageDownload, AppError> {
    let decision = filters
        .download_outcome(response)
        .map_err(|error| AppError::Filter(error.to_string()))?;
    let code = decision.fail_code.unwrap_or(consts::ERR_CONNECTION_ERROR);
    let failed_url = decision
        .url
        .clone()
        .unwrap_or_else(|| image_url.to_string());
    let _ = ensure_retry;
    Ok(ImageDownload::Failed {
        fail_code: code,
        real_url: decision
            .real_url
            .clone()
            .unwrap_or_else(|| response.real_url.clone()),
        url: failed_url,
    })
}

/// One real image transfer shared by the sequential API and the persistent
/// download worker: request -> RSS decision -> bounded native stream ->
/// staged file -> publish. Every Task mutation goes through `port`, so the
/// caller may keep only short borrows while the socket is in flight.
pub(crate) async fn run_download<P: DownloadPort>(
    port: &mut P,
    session: &mut Session,
    filters: &mut Filters,
    image_url: &str,
    runtime: &mut DownloadRuntime<'_>,
    keepalive: &mut dyn FnMut() -> bool,
) -> Result<ImageDownload, AppError> {
    macro_rules! guarded {
        ($fut:expr) => {
            match runtime.cancel.as_deref_mut() {
                Some(receiver) => {
                    tokio::select! {
                        biased;
                        changed = receiver.changed() => {
                            let _ = changed;
                            return Ok(ImageDownload::Aborted);
                        }
                        value = $fut => value,
                    }
                }
                None => $fut.await,
            }
        };
    }

    if !port.validate() || keepalive() {
        return Ok(ImageDownload::Aborted);
    }
    let mut outcome = match guarded!(session.request("GET", image_url, None, true)) {
        Ok(outcome) => outcome,
        Err(error) => return Err(AppError::Http(error.to_string())),
    };
    if !port.validate() {
        if let Some(stream) = outcome.response.stream.take() {
            stream.close().await;
        }
        return Ok(ImageDownload::Aborted);
    }
    let has_stream = outcome.response.has_stream();
    let mut view = App::page_view(&outcome.response, false, has_stream);
    let mut decision = filters
        .download_outcome(&view)
        .map_err(|error| AppError::Filter(error.to_string()))?;
    if decision.fail_code == Some(consts::ERR_IMAGE_BROKEN) && has_stream {
        let mut body_prefix = Vec::with_capacity(512);
        let mut read = 0i64;
        let mut read_error = None;
        let mut aborted = false;
        if let Some(stream) = outcome.response.stream.as_mut() {
            while body_prefix.len() < 512 {
                if keepalive() {
                    aborted = true;
                    break;
                }
                match guarded!(stream.next_chunk()) {
                    Ok(Some(chunk)) => {
                        read += chunk.len() as i64;
                        let prefix_bytes = (512 - body_prefix.len()).min(chunk.len());
                        body_prefix.extend_from_slice(&chunk[..prefix_bytes]);
                    }
                    Ok(None) => break,
                    Err(error) => {
                        read_error = Some(error);
                        break;
                    }
                }
            }
        }
        if aborted {
            if let Some(stream) = outcome.response.stream.take() {
                stream.close().await;
            }
            return Ok(ImageDownload::Aborted);
        }
        view.text = String::from_utf8_lossy(&body_prefix).into_owned();
        decision = filters
            .download_outcome(&view)
            .map_err(|error| AppError::Filter(error.to_string()))?;
        if decision.fail_code != Some(consts::ERR_QUOTA_EXCEEDED)
            && is_html_error_body(&body_prefix)
        {
            let streamed = filters
                .stream_outcome(&view, read, read_error.as_deref())
                .map_err(|error| AppError::Filter(error.to_string()))?;
            let fail_code = streamed.code.unwrap_or(consts::ERR_IMAGE_BROKEN);
            session.report_stream_result(outcome.response.proxy.as_deref(), false);
            port.warn_download(&stream_diagnostic(
                "html_error_body",
                read,
                streamed.expected_length,
            ));
            if let Some(stream) = outcome.response.stream.take() {
                stream.close().await;
            }
            let failed = ImageDownload::Failed {
                fail_code,
                real_url: view.real_url.clone(),
                url: view.url.clone(),
            };
            if let ImageDownload::Failed { url, .. } = &failed
                && port.validate()
            {
                let _ = port.retry_page(url);
            }
            return Ok(failed);
        }
        if let Some(error) = read_error.as_deref() {
            let streamed = filters
                .stream_outcome(&view, read, Some(error))
                .map_err(|filter| AppError::Filter(filter.to_string()))?;
            port.warn_download(&stream_diagnostic(
                "read_error",
                read,
                streamed.expected_length,
            ));
        }
    }
    if decision.fail_code.is_some() {
        if let Some(stream) = outcome.response.stream.take() {
            stream.close().await;
        }
        if decision.fail_code != Some(consts::ERR_IMAGE_BROKEN) {
            session.report_stream_result(outcome.response.proxy.as_deref(), true);
        }
        let failed = download_failed(filters, image_url, &view, true)?;
        if let ImageDownload::Failed { url, .. } = &failed
            && port.validate()
        {
            let _ = port.retry_page(url);
        }
        return Ok(failed);
    }
    if decision.exception.is_some() || !decision.stream {
        if let Some(stream) = outcome.response.stream.take() {
            stream.close().await;
        }
        return Ok(ImageDownload::Aborted);
    }
    let stream_page = App::page_view(&outcome.response, false, true);
    let Some(mut stream) = outcome.response.stream.take() else {
        return Ok(ImageDownload::Aborted);
    };
    let text_response = stream_page
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        .is_some_and(|(_, value)| {
            value
                .trim_start()
                .get(..4)
                .is_some_and(|prefix| prefix.eq_ignore_ascii_case("text"))
        });
    if text_response {
        let prefix_limit = if stream_page.content_length >= 0 {
            stream_page.content_length.min(512) as usize
        } else {
            512
        };
        let mut body_prefix = Vec::with_capacity(prefix_limit);
        let mut read = 0i64;
        let mut read_error = None;
        while body_prefix.len() < prefix_limit {
            if keepalive() {
                stream.close().await;
                return Ok(ImageDownload::Aborted);
            }
            match guarded!(stream.next_chunk()) {
                Ok(Some(chunk)) => {
                    read += chunk.len() as i64;
                    let prefix_bytes = (prefix_limit - body_prefix.len()).min(chunk.len());
                    body_prefix.extend_from_slice(&chunk[..prefix_bytes]);
                }
                Ok(None) => break,
                Err(error) => {
                    read_error = Some(error);
                    break;
                }
            }
        }
        view.text = String::from_utf8_lossy(&body_prefix).into_owned();
        let quota = filters
            .download_outcome(&view)
            .map_err(|error| AppError::Filter(error.to_string()))?
            .fail_code
            == Some(consts::ERR_QUOTA_EXCEEDED);
        if quota {
            session.report_stream_result(outcome.response.proxy.as_deref(), true);
            stream.close().await;
            if port.validate() {
                let _ = port.retry_page(&view.url);
            }
            return Ok(ImageDownload::Failed {
                fail_code: consts::ERR_QUOTA_EXCEEDED,
                real_url: view.real_url.clone(),
                url: view.url.clone(),
            });
        }
        if is_html_error_body(&body_prefix) {
            let streamed = filters
                .stream_outcome(&view, read, read_error.as_deref())
                .map_err(|error| AppError::Filter(error.to_string()))?;
            let fail_code = streamed.code.unwrap_or(consts::ERR_IMAGE_BROKEN);
            session.report_stream_result(outcome.response.proxy.as_deref(), false);
            port.warn_download(&stream_diagnostic(
                "html_error_body",
                read,
                streamed.expected_length,
            ));
            stream.close().await;
            if port.validate() {
                let _ = port.retry_page(&view.url);
            }
            return Ok(ImageDownload::Failed {
                fail_code,
                real_url: view.real_url.clone(),
                url: view.url.clone(),
            });
        }
        if let Some(error) = read_error.as_deref() {
            let streamed = filters
                .stream_outcome(&view, read, Some(error))
                .map_err(|filter| AppError::Filter(filter.to_string()))?;
            port.warn_download(&stream_diagnostic(
                "read_error",
                read,
                streamed.expected_length,
            ));
            stream.close().await;
            if port.validate() {
                let _ = port.retry_page(&view.url);
            }
            return Ok(ImageDownload::Failed {
                fail_code: streamed.code.unwrap_or(consts::ERR_IMAGE_BROKEN),
                real_url: view.real_url.clone(),
                url: view.url.clone(),
            });
        }
        session.report_stream_result(outcome.response.proxy.as_deref(), true);
        stream.close().await;
        if port.validate() {
            let _ = port.retry_page(&view.url);
        }
        return Ok(ImageDownload::Failed {
            fail_code: 1014,
            real_url: view.real_url.clone(),
            url: view.url.clone(),
        });
    }
    let staged = match port
        .begin_save(image_url, &view.url)
        .map_err(AppError::Task)?
    {
        SaveStart::AlreadyDone(path) => {
            stream.close().await;
            return Ok(ImageDownload::AlreadyDone(path));
        }
        SaveStart::Staged(staged) => staged,
    };
    let mut stage = DownloadStage::new(staged, stream);
    let mut file = match guarded!(create_stage_file(
        stage.staged_ref(),
        stage.cancel_flag(),
        runtime.stage_drain.clone(),
    )) {
        Ok(file) => file,
        Err(error) => {
            return Err(AppError::Download(format!(
                "{}: {error}",
                stage.staged_ref().stage.display()
            )));
        }
    };
    let mut read = 0i64;
    let mut body_prefix = Vec::with_capacity(512);
    loop {
        if keepalive() {
            return Ok(ImageDownload::Aborted);
        }
        let chunk = guarded!(stage.stream_mut().next_chunk());
        match chunk {
            Ok(Some(chunk)) => {
                read += chunk.len() as i64;
                let scan_html_prefix = body_prefix.len() < 512;
                let prefix_bytes = (512 - body_prefix.len()).min(chunk.len());
                body_prefix.extend_from_slice(&chunk[..prefix_bytes]);
                if scan_html_prefix && is_html_error_body(&body_prefix) {
                    drop(file);
                    let broken = filters
                        .stream_outcome(&stream_page, read, Some("HTML error response body"))
                        .map_err(|filter| AppError::Filter(filter.to_string()))?;
                    let code = broken.code.unwrap_or(consts::ERR_IMAGE_BROKEN);
                    session.report_stream_result(outcome.response.proxy.as_deref(), false);
                    port.warn_download(&stream_diagnostic(
                        "html_error_body",
                        read,
                        broken.expected_length,
                    ));
                    drop(stage);
                    if port.validate() {
                        let _ = port.retry_page(&view.url);
                    }
                    return Ok(ImageDownload::Failed {
                        fail_code: code,
                        real_url: view.real_url.clone(),
                        url: view.url.clone(),
                    });
                }
                if let Some(speed) = &runtime.speed {
                    let mut checker = speed.lock().unwrap_or_else(|error| error.into_inner());
                    checker.check(chunk.len());
                    let full = checker.calc(true);
                    drop(checker);
                    if is_low_speed(full, runtime.low_speed_threshold) {
                        let checked = filters
                            .stream_outcome(&stream_page, read, None)
                            .map_err(|filter| AppError::Filter(filter.to_string()))?;
                        port.warn_download(&stream_diagnostic(
                            "low_speed",
                            read,
                            checked.expected_length,
                        ));
                        drop(file);
                        drop(stage);
                        let failed =
                            download_failed(filters, image_url, &fake_page(&view.url), false)?;
                        if let ImageDownload::Failed { url, .. } = &failed
                            && port.validate()
                        {
                            let _ = port.retry_page(url);
                        }
                        return Ok(failed);
                    }
                }
                #[cfg(test)]
                if io_fault() == Some(IoFault::Write) && read > 0 {
                    return Err(AppError::Download("injected write failure".into()));
                }
                file.write_all(&chunk).await.map_err(|error| {
                    AppError::Download(format!("{}: {error}", stage.staged_ref().stage.display()))
                })?;
            }
            Ok(None) => break,
            Err(error) => {
                drop(file);
                let broken = filters
                    .stream_outcome(&stream_page, read, Some(&error))
                    .map_err(|filter| AppError::Filter(filter.to_string()))?;
                let code = broken.code.unwrap_or(consts::ERR_IMAGE_BROKEN);
                port.warn_download(&stream_diagnostic(
                    "read_error",
                    read,
                    broken.expected_length,
                ));
                drop(stage);
                if port.validate() {
                    let _ = port.retry_page(&view.url);
                }
                return Ok(ImageDownload::Failed {
                    fail_code: code,
                    real_url: view.real_url.clone(),
                    url: view.url.clone(),
                });
            }
        }
    }
    #[cfg(test)]
    if io_fault() == Some(IoFault::Flush) {
        return Err(AppError::Download("injected flush failure".into()));
    }
    file.flush().await.map_err(|error| {
        AppError::Download(format!("{}: {error}", stage.staged_ref().stage.display()))
    })?;
    drop(file);
    let streamed = filters
        .stream_outcome(&stream_page, read, None)
        .map_err(|filter| AppError::Filter(filter.to_string()))?;
    if streamed.broken {
        let code = streamed.code.unwrap_or(consts::ERR_IMAGE_BROKEN);
        port.warn_download(&stream_diagnostic(
            "length_mismatch",
            read,
            streamed.expected_length,
        ));
        drop(stage);
        if port.validate() {
            let _ = port.retry_page(&view.url);
        }
        return Ok(ImageDownload::Failed {
            fail_code: code,
            real_url: view.real_url.clone(),
            url: view.url.clone(),
        });
    }
    session.report_stream_result(outcome.response.proxy.as_deref(), true);
    if !port.validate() || keepalive() {
        return Ok(ImageDownload::Aborted);
    }
    match port.commit(stage.staged_take()).map_err(AppError::Task)? {
        CommitOutcome::Published(path) => Ok(ImageDownload::Saved(path)),
        CommitOutcome::Rejected => Ok(ImageDownload::Aborted),
    }
}

impl App {
    /// Pop one image-queue item and capture the immutable inputs a worker
    /// needs. Nothing is requested here.
    pub(crate) fn begin_download_item(
        &mut self,
        guid: &str,
    ) -> Result<Option<DownloadItem>, AppError> {
        self.ensure_task_queues(guid)?;
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        let Some(image_url) = task.img_q.as_mut().and_then(|queue| queue.pop_front()) else {
            return Ok(None);
        };
        Ok(Some(DownloadItem {
            image_url,
            policy: util::get_proxy_policy(task.config.proxy_image_only, task.config.proxy_image),
            token: task.token(),
            timeout: task.config.download_timeout,
            low_speed_threshold: task.config.low_speed_threshold,
        }))
    }

    /// One real image-queue consumer: request -> RSS download decision ->
    /// native bounded stream -> Task staging/publish. The post-stream RSS
    /// decision owns the broken/abort outcome.
    pub async fn download_one_image(
        &mut self,
        guid: &str,
        filters: &mut Filters,
    ) -> Result<ImageDownload, AppError> {
        let Some(item) = self.begin_download_item(guid)? else {
            return Ok(ImageDownload::Empty);
        };
        let mut session = self.take_http_session(item.policy);
        let mut port = DirectPort {
            task: self
                .all_tasks
                .get_mut(guid)
                .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?,
            logger: &mut self.logger,
            token: item.token,
        };
        let mut runtime = DownloadRuntime {
            speed: None,
            low_speed_threshold: 0,
            cancel: None,
            stage_drain: None,
        };
        let exit = self.exit;
        let result = run_download(
            &mut port,
            &mut session,
            filters,
            &item.image_url,
            &mut runtime,
            &mut || exit >= consts::XEH_STATE_FULL_EXIT,
        )
        .await;
        self.restore_http_session(session);
        result
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
                    let mut task = Task::from_wire_with_private_paths(
                        value,
                        &self.base_dir,
                        Arc::clone(&self.private_paths),
                    )
                    .map_err(SessionError::Task)?;
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

fn is_runnable_state(state: i64) -> bool {
    consts::TASK_STATE_PAUSED < state && state < consts::TASK_STATE_FINISHED
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

#[cfg(test)]
mod html_error_body_tests {
    use super::is_html_error_body;

    #[test]
    fn recognizes_html_after_bom_and_leading_whitespace() {
        assert!(is_html_error_body(
            b"\xef\xbb\xbf \r\n<!DOCTYPE html><html><body>blocked</body></html>"
        ));
    }

    #[test]
    fn ignores_binary_image_prefixes() {
        assert!(!is_html_error_body(
            b"\x89PNG\r\n\x1a\n<html-looking payload"
        ));
    }
}
