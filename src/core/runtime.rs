//! M4 task loop, persistent scan/download workers, monitor and cancellation.
//!
//! This child module keeps the reference ownership split: `App` owns the task
//! registry and queues, `Task` owns its files/counters, and each persistent
//! worker owns its own `Session`, `Filters` VM, heartbeat and speed ring. The
//! shared task records are reached through very short `RefCell` borrows; no
//! borrow is held across a network, stream or blocking-pool await.

use std::cell::Cell;
use std::collections::{BTreeMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::{Notify, watch};
use tokio::task::JoinHandle;

use super::{
    App, AppError, CommitOutcome, DownloadPort, DownloadRuntime, ImageDownload, StageDrain,
    is_runnable_state, run_download,
};
use crate::r#const as consts;
use crate::filters::{Filters, ImageOutcome, MetadataOutcome, PageOutcome, PageResponse};
use crate::task::{
    ArchiveSnapshot, CommittedArchive, PreparedArchive, commit_prepared, discard_prepared,
    prepare_archive_snapshot, remove_detached,
};
use crate::task::{SaveStart, StagedSave, Task, TaskError};
use crate::util;
use crate::util::logger::{DEBUG, ERROR, INFO, VERBOSE, WARNING};
use crate::worker::{HttpClock, Session, SpeedChecker};

fn localized(key: &str, args: &[&str]) -> String {
    crate::i18n::render(crate::i18n::Locale::EnUs, key, args).unwrap_or_default()
}

fn error_message(code: i64) -> String {
    crate::i18n::err_msg(crate::i18n::Locale::EnUs, code, &[])
}

/// One structured diagnostic event with the injected clock value. The buffer
/// is bounded; it is used by the deterministic M4 traces and never decides
/// business behaviour.
#[derive(Clone, Debug)]
pub struct TraceEvent {
    pub at: f64,
    pub kind: String,
    pub detail: Value,
}

pub(crate) struct Slot {
    pub guid: String,
    pub shared: Rc<SharedSlot>,
}

/// Shared ownership record for one spawned logical worker. The `JoinHandle`
/// stays in this record until one owner joins its actual retirement; every
/// other waiter observes the same completion instead of taking the handle
/// away. Moving a waiter (task-loop local, public pause/shutdown/delete
/// drain) can therefore never hide work from the other drains, and dropping
/// a waiter only drops its own observation without detaching the task.
pub(crate) struct SharedSlot {
    handle: std::cell::RefCell<Option<JoinHandle<()>>>,
    done: std::cell::Cell<bool>,
    notify: Notify,
}

impl SharedSlot {
    /// Block until the owned worker actually retired. A cancelled waiter
    /// simply drops its observation; the handle stays owned by this record.
    async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            if self.done.get() {
                return;
            }
            notified.await;
        }
    }

    /// Join the owned handle exactly once. Only retired entries are reaped,
    /// so the await resolves immediately and never strands the record.
    async fn join_owned(&self) {
        let handle = self.handle.borrow_mut().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    fn complete(&self) {
        self.done.set(true);
        self.notify.notify_waiters();
    }
}

/// Completes the shared record when the wrapped worker future returns,
/// including through a panic unwind: the completion is a synchronous record
/// update, never a second task, registry or reaper.
struct CompleteOnExit(Rc<SharedSlot>);

impl Drop for CompleteOnExit {
    fn drop(&mut self) {
        self.0.complete();
    }
}

/// Result of the synchronous archive admission boundary.
enum ArchiveAdmission {
    Admitted(ArchiveSnapshot),
    Stale,
    Failed(TaskError),
}

/// Result of the synchronous publication boundary. `Stale` hands the
/// unpublished container back so its owner can remove the private stage.
pub(crate) enum ArchiveCommit {
    Committed(CommittedArchive),
    Stale(PreparedArchive),
}

pub(crate) struct RuntimeInner {
    pub app: std::cell::RefCell<App>,
    resource_root: PathBuf,
    clock: Arc<dyn HttpClock>,
    threads: std::cell::RefCell<Vec<Vec<Slot>>>,
    cancels: std::cell::RefCell<BTreeMap<String, watch::Sender<bool>>>,
    monitors: std::cell::RefCell<BTreeMap<String, Rc<Monitor>>>,
    last_monitor: std::cell::RefCell<Option<Rc<Monitor>>>,
    stage: StageDrain,
    trace: std::cell::RefCell<Vec<TraceEvent>>,
}

impl RuntimeInner {
    fn exit(&self) -> i64 {
        self.app.borrow().exit_state()
    }

    fn state(&self, guid: &str) -> Option<i64> {
        self.app.borrow().task_state(guid)
    }

    fn failcode(&self, guid: &str) -> i64 {
        self.app
            .borrow()
            .task(guid)
            .map(|task| task.failcode)
            .unwrap_or(0)
    }

    fn is_current(&self, guid: &str, token: (u64, u64)) -> bool {
        self.app.borrow().task_is_current(guid, token)
    }

    /// Identity-only ownership check for effects that legitimately happen at
    /// FINISHED (archive admission), where the worker-runnable window no
    /// longer applies.
    fn owns_run(&self, guid: &str, token: (u64, u64)) -> bool {
        self.app.borrow().task_run_matches(guid, token)
    }

    fn finished(&self, guid: &str) -> i64 {
        self.app
            .borrow()
            .task(guid)
            .and_then(|task| task.meta.get("finished").and_then(Value::as_i64))
            .unwrap_or(0)
    }

    fn total(&self, guid: &str) -> i64 {
        self.app
            .borrow()
            .task(guid)
            .and_then(|task| task.meta.get("total").and_then(Value::as_i64))
            .unwrap_or(0)
    }

    fn img_len(&self, guid: &str) -> usize {
        self.app
            .borrow()
            .task(guid)
            .and_then(|task| task.img_q.as_ref().map(VecDeque::len))
            .unwrap_or(0)
    }

    fn unfinished(&self, guid: &str) -> Vec<i64> {
        self.app
            .borrow()
            .task(guid)
            .and_then(|task| task.get_fid_unfinished().ok())
            .unwrap_or_default()
    }

    fn set_state(&self, guid: &str, state: i64) {
        if let Some(task) = self.app.borrow_mut().task_mut(guid) {
            task.state = state;
        }
    }

    fn log(&self, level: i64, message: &str) {
        let mut app = self.app.borrow_mut();
        let logger = app.logger_mut();
        match level {
            ERROR => {
                let _ = logger.error(message);
            }
            WARNING => {
                let _ = logger.warning(message);
            }
            INFO => {
                let _ = logger.info(message);
            }
            DEBUG => {
                let _ = logger.debug(message);
            }
            _ => {
                let _ = logger.verbose(message);
            }
        }
    }

    fn record(&self, kind: &str, detail: Value) {
        let at = self.clock.now();
        let mut trace = self.trace.borrow_mut();
        if trace.len() >= 512 {
            trace.remove(0);
        }
        trace.push(TraceEvent {
            at,
            kind: kind.to_string(),
            detail,
        });
    }

    fn save_session(&self) {
        let _ = self.app.borrow_mut().save_session();
    }

    /// Observe the owned completions for one state without detaching them.
    /// Every waiter (task loop or public drain) sees the same records.
    fn snapshot_slots(&self, state: i64) -> Vec<Rc<SharedSlot>> {
        if !(0..20).contains(&state) {
            return Vec::new();
        }
        self.threads.borrow()[state as usize]
            .iter()
            .map(|slot| Rc::clone(&slot.shared))
            .collect()
    }

    /// Observe every owned completion of one task without detaching them.
    fn snapshot_guid_slots(&self, guid: &str) -> Vec<Rc<SharedSlot>> {
        let mut out = Vec::new();
        for slots in self.threads.borrow().iter() {
            for slot in slots {
                if slot.guid == guid {
                    out.push(Rc::clone(&slot.shared));
                }
            }
        }
        out
    }

    /// Observe every owned completion without detaching any of them.
    fn snapshot_all_slots(&self) -> Vec<Rc<SharedSlot>> {
        let mut out = Vec::new();
        for slots in self.threads.borrow().iter() {
            for slot in slots {
                out.push(Rc::clone(&slot.shared));
            }
        }
        out
    }

    /// Await owned completions, then reap the retired entries. Reaping joins
    /// only handles whose completion already fired, so the joins resolve
    /// immediately; waiters never take a handle, so a dropped waiter keeps
    /// the record visible and joined exactly once here.
    async fn wait_and_reap(&self, shared: Vec<Rc<SharedSlot>>) -> usize {
        let count = shared.len();
        for entry in &shared {
            entry.wait().await;
        }
        let retired = {
            let mut threads = self.threads.borrow_mut();
            let mut retired = Vec::new();
            for slots in threads.iter_mut() {
                let mut index = 0;
                while index < slots.len() {
                    if slots[index].shared.done.get() {
                        retired.push(slots.remove(index).shared);
                    } else {
                        index += 1;
                    }
                }
            }
            retired
        };
        for entry in retired {
            entry.join_owned().await;
        }
        count
    }

    /// Spawn one logical worker and keep its shared ownership record until
    /// its actual retirement is joined. The wrapper completes the record
    /// when the worker future returns (or unwinds); waiters only ever
    /// observe the completion, so no waiter can hide or detach the task.
    fn push_tracked(
        &self,
        state: i64,
        guid: String,
        future: impl std::future::Future<Output = ()> + 'static,
    ) {
        if !(0..20).contains(&state) {
            tokio::task::spawn_local(future);
            return;
        }
        let shared = Rc::new(SharedSlot {
            handle: std::cell::RefCell::new(None),
            done: std::cell::Cell::new(false),
            notify: Notify::new(),
        });
        let guard = CompleteOnExit(Rc::clone(&shared));
        let handle = tokio::task::spawn_local(async move {
            let _guard = guard;
            future.await;
        });
        shared.handle.borrow_mut().replace(handle);
        self.threads.borrow_mut()[state as usize].push(Slot { guid, shared });
    }

    /// Capture the archive inputs and enter MAKE_ARCHIVE only while `guid`
    /// still holds the exact task object/run that was admitted. One borrow
    /// covers the identity check, the snapshot and the state change, so no
    /// await can interleave a resume/reuse between them. FINISHED (20) is a
    /// valid archive admission even though it is outside the worker-runnable
    /// window.
    fn begin_archive(&self, guid: &str, token: (u64, u64)) -> ArchiveAdmission {
        let mut app = self.app.borrow_mut();
        if app.exit_state() >= consts::XEH_STATE_FULL_EXIT {
            return ArchiveAdmission::Stale;
        }
        let Some(task) = app.task_mut(guid) else {
            return ArchiveAdmission::Stale;
        };
        if task.token() != token || task.state < consts::TASK_STATE_FINISHED {
            return ArchiveAdmission::Stale;
        }
        match task.archive_snapshot() {
            Ok(snapshot) => {
                task.state = consts::TASK_STATE_MAKE_ARCHIVE;
                ArchiveAdmission::Admitted(snapshot)
            }
            Err(error) => {
                task.state = consts::TASK_STATE_FAILED;
                ArchiveAdmission::Failed(error)
            }
        }
    }

    /// Publish a prepared container for the exact run that was admitted.
    /// Identity validation and the bounded namespace effects share one
    /// borrow, so an invalidated run can never publish in between. The
    /// minimum state is optional: the M4 worker publishes at MAKE_ARCHIVE,
    /// while the frozen RPC reader (`rpc.py:477-486`) consults only the
    /// captured task identity and its progress within that run does not
    /// invalidate publication.
    fn commit_archive(
        &self,
        guid: &str,
        token: (u64, u64),
        prepared: PreparedArchive,
        min_state: Option<i64>,
    ) -> Result<ArchiveCommit, TaskError> {
        // An immutable RefCell borrow still excludes every mutation for the
        // duration of the bounded rename/removal, so a validated run cannot
        // be replaced between validation and publication.
        let app = self.app.borrow();
        let current = app.exit_state() < consts::XEH_STATE_FULL_EXIT
            && app.task(guid).is_some_and(|task| {
                task.token() == token && min_state.is_none_or(|min| task.state >= min)
            });
        if !current {
            return Ok(ArchiveCommit::Stale(prepared));
        }
        match commit_prepared(prepared) {
            Ok(committed) => Ok(ArchiveCommit::Committed(committed)),
            Err(error) => Err(error),
        }
    }

    /// Complete the archive only while this run still owns the task.
    fn finish_archive(&self, guid: &str, token: (u64, u64)) -> bool {
        let mut app = self.app.borrow_mut();
        if app.exit_state() >= consts::XEH_STATE_FULL_EXIT {
            return false;
        }
        let Some(task) = app.task_mut(guid) else {
            return false;
        };
        if task.token() != token || task.state < consts::TASK_STATE_MAKE_ARCHIVE {
            return false;
        }
        task.state = consts::TASK_STATE_FINISHED;
        true
    }

    /// Publish a failure only while this run still owns the task; an old
    /// writer's error must never fail a replacement run.
    fn fail_archive(&self, guid: &str, token: (u64, u64)) -> bool {
        let mut app = self.app.borrow_mut();
        if app.exit_state() >= consts::XEH_STATE_FULL_EXIT {
            return false;
        }
        let Some(task) = app.task_mut(guid) else {
            return false;
        };
        if task.token() != token || task.state < consts::TASK_STATE_MAKE_ARCHIVE {
            return false;
        }
        task.state = consts::TASK_STATE_FAILED;
        true
    }

    fn cancel_channel(&self, guid: &str) -> watch::Receiver<bool> {
        let mut cancels = self.cancels.borrow_mut();
        cancels
            .entry(guid.to_string())
            .or_insert_with(|| watch::channel(false).0)
            .subscribe()
    }

    fn start_run_channel(&self, guid: &str) -> watch::Receiver<bool> {
        let (sender, receiver) = watch::channel(false);
        self.cancels.borrow_mut().insert(guid.to_string(), sender);
        receiver
    }

    fn signal_cancel(&self, guid: &str) {
        if let Some(sender) = self.cancels.borrow().get(guid) {
            let _ = sender.send(true);
        }
    }

    fn monitor_exit(&self, guid: &str) {
        if let Some(monitor) = self.monitors.borrow().get(guid) {
            monitor.request_exit();
        }
    }

    fn worker_session(
        &self,
        policy: util::ProxyPolicy,
        timeout: Duration,
        retry: usize,
    ) -> Session {
        let app = self.app.borrow();
        let mut config = app.session_config.clone();
        config.timeout = timeout;
        config.retry = retry;
        let headers = app
            .headers()
            .iter()
            .map(|header| (header.name.clone(), header.value.clone()))
            .collect();
        let mut session = Session::new(headers, app.proxy_pool(), policy, config);
        session.set_logger(app.logger.clone());
        session
    }
}

/// Public lifecycle owner: `Runtime::run` consumes it and `AppHandle` stays
/// usable for pause/resume/delete/config/cookie/session observation.
pub struct Runtime {
    inner: Rc<RuntimeInner>,
}

impl Runtime {
    /// Build the runtime over an existing App. It does not spawn anything by
    /// itself and does not require a hidden `LocalSet` until `run`/`spawn`.
    pub fn new(app: App, resource_root: PathBuf) -> Self {
        let clock = app.session_config.clock.clone();
        let inner = RuntimeInner {
            app: std::cell::RefCell::new(app),
            resource_root,
            clock,
            threads: std::cell::RefCell::new((0..20).map(|_| Vec::new()).collect()),
            cancels: std::cell::RefCell::new(BTreeMap::new()),
            monitors: std::cell::RefCell::new(BTreeMap::new()),
            last_monitor: std::cell::RefCell::new(None),
            stage: StageDrain::default(),
            trace: std::cell::RefCell::new(Vec::new()),
        };
        Self {
            inner: Rc::new(inner),
        }
    }

    pub fn handle(&self) -> AppHandle {
        AppHandle {
            inner: Rc::clone(&self.inner),
        }
    }

    /// Spawn the task loop on the caller's `LocalSet` and return its join
    /// handle plus a live App handle.
    pub fn spawn(self) -> (JoinHandle<()>, AppHandle) {
        let handle = self.handle();
        let task = tokio::task::spawn_local(self.run());
        (task, handle)
    }

    /// The real `_task_loop`: pop task guids, skip non-runnable states, keep
    /// `last_task_guid`, autosave every ten empty polls, and drive `_do_task`.
    pub async fn run(self) {
        let inner = Rc::clone(&self.inner);
        let mut current: Option<String> = None;
        let mut cnt = 0usize;
        while inner.exit() == consts::XEH_STATE_RUNNING {
            if cnt == 10 {
                inner.save_session();
                cnt = 0;
            }
            let guid = inner.app.borrow_mut().pop_queued_task(&mut current);
            let Some(guid) = guid else {
                inner.clock.sleep(1.0).await;
                cnt += 1;
                continue;
            };
            let state = inner.state(&guid).unwrap_or(consts::TASK_STATE_FINISHED);
            if is_runnable_state(state) {
                let message = localized("TASK_START", &[&guid]);
                inner.log(INFO, &message);
                inner.save_session();
                cnt = 0;
                self.do_task(&guid).await;
            }
        }
        let message = localized("XEH_LOOP_FINISHED", &[]);
        inner.log(INFO, &message);
        self.cleanup().await;
    }

    async fn cleanup(&self) {
        let inner = &self.inner;
        if inner.exit() == consts::XEH_STATE_RUNNING {
            inner.app.borrow_mut().set_exit(consts::XEH_STATE_SOFT_EXIT);
        }
        inner.save_session();
        // Shared observations: retirement is joined through the same records
        // every drain uses, so entries disappear only after actual completion.
        inner.wait_and_reap(inner.snapshot_all_slots()).await;
        inner.stage.wait_zero().await;
        inner.stage.wait_native().await;
        inner.save_session();
        inner.app.borrow_mut().set_exit(consts::XEH_STATE_CLEAN);
    }

    /// `_do_task` for one guid. Every await is outside an App borrow; each
    /// state step re-reads the live task and stops on pause/exit/failure.
    async fn do_task(&self, guid: &str) {
        let inner = Rc::clone(&self.inner);
        let run = {
            let mut app = inner.app.borrow_mut();
            match app.begin_run(guid) {
                Ok(run) => run,
                Err(error) => {
                    drop(app);
                    inner.log(WARNING, &format!("task #{guid} cannot start: {error}"));
                    return;
                }
            }
        };
        let token = run.token;
        let policy = run.policy;
        let mut cancel = inner.start_run_channel(guid);
        let mut producer_session = inner.worker_session(policy, Duration::from_secs(20), 10);
        let mut producer = match producer_filters(&inner) {
            Ok(filters) => filters,
            Err(error) => {
                inner.log(ERROR, &format!("task #{guid} error: {error}"));
                inner.set_state(guid, consts::TASK_STATE_FAILED);
                return;
            }
        };
        let mut monitor_started = false;
        let mut monitor: Option<Rc<Monitor>> = None;

        while inner.exit() < consts::XEH_STATE_FULL_EXIT {
            let Some(state) = inner.state(guid) else {
                break;
            };
            if !is_runnable_state(state) {
                break;
            }
            let waiting = inner.snapshot_slots(state);
            if !waiting.is_empty() {
                let message = format!("wait {} threads in state {state}", waiting.len());
                inner.log(VERBOSE, &message);
                // Observe the shared completions without detaching them: a
                // public drain waits on the same records while this loop
                // holds its own observation.
                inner.wait_and_reap(waiting).await;
                continue;
            }
            if state >= consts::TASK_STATE_SCAN_IMG && !monitor_started {
                let mut names: Vec<String> = (1..=run.download_threads.max(0))
                    .map(|i| format!("down-{i}"))
                    .collect();
                // The reference seeds scan names for every state >= SCAN_IMG,
                // including a resumed DOWNLOAD where no scan worker exists;
                // those phantom names never heartbeat and delay completion by
                // a zombie timeout. Register only workers this iteration
                // actually spawns (registered M4 correction).
                if state == consts::TASK_STATE_SCAN_IMG {
                    names.extend((1..=run.scan_threads.max(0)).map(|i| format!("scan-{i}")));
                }
                let started = self.start_monitor(guid, &names);
                monitor = Some(started);
                monitor_started = true;
            }
            match state {
                consts::TASK_STATE_GET_META => {
                    let request = producer_session.request("GET", &run.url, None, false);
                    let outcome = tokio::select! {
                        biased;
                        _ = cancel.changed() => break,
                        result = request => result,
                    };
                    let outcome = match outcome {
                        Ok(outcome) => outcome,
                        Err(error) => {
                            inner.log(ERROR, &format!("task #{guid} error: {error}"));
                            inner.set_state(guid, consts::TASK_STATE_FAILED);
                            break;
                        }
                    };
                    if !inner.is_current(guid, token) {
                        break;
                    }
                    let view = App::page_view(&outcome.response, false, false);
                    let parsed = match producer
                        .metadata_outcome(&view)
                        .map_err(|error| AppError::Filter(error.to_string()))
                    {
                        Ok(parsed) => parsed,
                        Err(error) => {
                            inner.log(ERROR, &format!("task #{guid} error: {error}"));
                            inner.set_state(guid, consts::TASK_STATE_FAILED);
                            break;
                        }
                    };
                    drop(view);
                    let failcode = {
                        let mut app = inner.app.borrow_mut();
                        match app.apply_metadata(guid, parsed) {
                            Ok(code) => code,
                            Err(error) => {
                                drop(app);
                                inner.log(ERROR, &format!("task #{guid} error: {error}"));
                                inner.set_state(guid, consts::TASK_STATE_FAILED);
                                break;
                            }
                        }
                    };
                    if matches!(
                        failcode,
                        Some(consts::ERR_ONLY_VISIBLE_EXH) | Some(consts::ERR_GALLERY_REMOVED)
                    ) {
                        let migrated = {
                            let mut app = inner.app.borrow_mut();
                            let logged_in = app.has_login();
                            if logged_in {
                                app.task_mut(guid)
                                    .map(Task::migrate_exhentai)
                                    .unwrap_or(false)
                            } else {
                                false
                            }
                        };
                        if migrated {
                            let message = localized("TASK_MIGRATE_EXH", &[guid]);
                            inner.log(INFO, &message);
                            inner.app.borrow_mut().push_task_back(guid);
                            break;
                        }
                    }
                    let failcode = inner.failcode(guid);
                    if failcode != 0 {
                        let message = format!("task #{guid} error: {}", error_message(failcode));
                        inner.log(ERROR, &message);
                        break;
                    }
                }
                consts::TASK_STATE_SCAN_PAGE => {
                    let start = {
                        let mut app = inner.app.borrow_mut();
                        app.begin_page_scan(guid)
                    };
                    let start = match start {
                        Ok(start) => start,
                        Err(error) => {
                            inner.log(ERROR, &format!("task #{guid} error: {error}"));
                            inner.set_state(guid, consts::TASK_STATE_FAILED);
                            break;
                        }
                    };
                    let PageScanStart::Plan(plan) = start else {
                        continue;
                    };
                    let mut failed = false;
                    if !plan.use_mpv {
                        for page_index in 0..plan.pages {
                            if !inner.is_current(guid, token) {
                                failed = true;
                                break;
                            }
                            let page_url = format!("{}/?p={}", plan.url, page_index);
                            let request = producer_session.request("GET", &page_url, None, false);
                            let outcome = tokio::select! {
                                biased;
                                _ = cancel.changed() => { failed = true; break; }
                                result = request => result,
                            };
                            let outcome = match outcome {
                                Ok(outcome) => outcome,
                                Err(error) => {
                                    inner.log(ERROR, &format!("task #{guid} error: {error}"));
                                    failed = true;
                                    break;
                                }
                            };
                            if !inner.is_current(guid, token) {
                                failed = true;
                                break;
                            }
                            let view = App::page_view(&outcome.response, false, false);
                            let parsed = match producer
                                .pageurl_outcome(&view)
                                .map_err(|error| AppError::Filter(error.to_string()))
                            {
                                Ok(parsed) => parsed,
                                Err(error) => {
                                    inner.log(ERROR, &format!("task #{guid} error: {error}"));
                                    failed = true;
                                    break;
                                }
                            };
                            let stop = {
                                let mut app = inner.app.borrow_mut();
                                match app.apply_page(guid, &parsed) {
                                    Ok(stop) => stop,
                                    Err(error) => {
                                        inner.log(ERROR, &format!("task #{guid} error: {error}"));
                                        true
                                    }
                                }
                            };
                            if stop || parsed.exception.is_some() {
                                failed = true;
                                break;
                            }
                        }
                    } else if plan.finished < plan.total {
                        let mpv = plan.mpv_url.clone();
                        let request = producer_session.request("GET", &mpv, None, false);
                        let outcome = tokio::select! {
                            biased;
                            _ = cancel.changed() => break,
                            result = request => result,
                        };
                        let outcome = match outcome {
                            Ok(outcome) => outcome,
                            Err(error) => {
                                inner.log(ERROR, &format!("task #{guid} error: {error}"));
                                break;
                            }
                        };
                        if !inner.is_current(guid, token) {
                            failed = true;
                        } else {
                            let view = App::page_view(&outcome.response, false, false);
                            match producer
                                .pageurl_mpv_outcome(&view)
                                .map_err(|error| AppError::Filter(error.to_string()))
                            {
                                Ok(parsed) => {
                                    let stop = {
                                        let mut app = inner.app.borrow_mut();
                                        app.apply_page(guid, &parsed).unwrap_or(true)
                                    };
                                    failed = stop || parsed.exception.is_some();
                                }
                                Err(error) => {
                                    inner.log(ERROR, &format!("task #{guid} error: {error}"));
                                    failed = true;
                                }
                            }
                        }
                    }
                    if failed {
                        let state = inner.state(guid).unwrap_or(consts::TASK_STATE_FAILED);
                        if state != consts::TASK_STATE_PAUSED && state != consts::TASK_STATE_FAILED
                        {
                            inner.set_state(guid, consts::TASK_STATE_FAILED);
                        }
                        break;
                    }
                }
                consts::TASK_STATE_SCAN_IMG => {
                    let title = {
                        inner
                            .app
                            .borrow()
                            .task(guid)
                            .and_then(|task| {
                                task.meta
                                    .get("title")
                                    .and_then(Value::as_str)
                                    .map(str::to_string)
                            })
                            .unwrap_or_default()
                    };
                    let message = localized("TASK_TITLE", &[guid, &title]);
                    inner.log(INFO, &message);
                    let total = inner.total(guid);
                    let finished = inner.finished(guid);
                    let message = localized(
                        "TASK_WILL_DOWNLOAD_CNT",
                        &[guid, &(total - finished).to_string(), &total.to_string()],
                    );
                    inner.log(INFO, &message);
                    let monitor = monitor.as_ref().expect("monitor started");
                    if let Err(error) = self.spawn_scan_workers(
                        guid,
                        monitor,
                        run.scan_threads.max(0) as usize,
                        policy,
                    ) {
                        inner.log(ERROR, &format!("task #{guid} error: {error}"));
                        inner.set_state(guid, consts::TASK_STATE_FAILED);
                        break;
                    }
                    inner.set_state(guid, consts::TASK_STATE_DOWNLOAD - 1);
                    continue;
                }
                consts::TASK_STATE_SCAN_ARCHIVE => {
                    inner.set_state(guid, consts::TASK_STATE_DOWNLOAD - 1);
                    continue;
                }
                consts::TASK_STATE_DOWNLOAD => {
                    let monitor = monitor.as_ref().expect("monitor started");
                    if let Err(error) = self.spawn_download_workers(
                        guid,
                        monitor,
                        run.download_threads.max(0) as usize,
                        policy,
                    ) {
                        inner.log(ERROR, &format!("task #{guid} error: {error}"));
                        inner.set_state(guid, consts::TASK_STATE_FAILED);
                        break;
                    }
                    if run.make_archive {
                        // The reference starts the new download workers
                        // first, then joins the previous archiver, and only
                        // afterwards admits the next one. The await happens
                        // outside every App/Task borrow.
                        let previous = inner.snapshot_slots(consts::TASK_STATE_MAKE_ARCHIVE);
                        if !previous.is_empty() {
                            let message = format!(
                                "wait {} threads in state {}",
                                previous.len(),
                                consts::TASK_STATE_MAKE_ARCHIVE
                            );
                            inner.log(VERBOSE, &message);
                            // Shared observation, not a take: public
                            // pause/shutdown drains await the same archive
                            // completion while this loop holds its own.
                            inner.wait_and_reap(previous).await;
                            // A blocked admission can outlive the run it was
                            // waiting for; revalidate the exact task/run (the
                            // download may legitimately have reached FINISHED
                            // while the previous archive was still running).
                            if *cancel.borrow()
                                || inner.exit() >= consts::XEH_STATE_FULL_EXIT
                                || !inner.owns_run(guid, token)
                            {
                                break;
                            }
                        }
                        if let Err(error) = self.spawn_archive_worker(guid, token) {
                            inner.log(ERROR, &format!("task #{guid} error: {error}"));
                            inner.set_state(guid, consts::TASK_STATE_FAILED);
                        }
                    }
                    break;
                }
                _ => {}
            }
            if inner.exit() >= consts::XEH_STATE_FULL_EXIT || !inner.is_current(guid, token) {
                break;
            }
            let failcode = inner.failcode(guid);
            if failcode != 0 {
                let message = format!("task #{guid} error: {}", error_message(failcode));
                inner.log(ERROR, &message);
                break;
            }
            inner.set_state(guid, state + 1);
        }
    }

    fn start_monitor(&self, guid: &str, names: &[String]) -> Rc<Monitor> {
        let monitor = Rc::new(Monitor::new(
            Rc::clone(&self.inner),
            guid.to_string(),
            Arc::clone(&self.inner.clock),
        ));
        monitor.set_vote_ns(names);
        let cancel = self.inner.cancel_channel(guid);
        self.inner.push_tracked(
            consts::TASK_STATE_SCAN_IMG,
            guid.to_string(),
            Rc::clone(&monitor).run(cancel),
        );
        self.inner
            .monitors
            .borrow_mut()
            .insert(guid.to_string(), Rc::clone(&monitor));
        self.inner.last_monitor.replace(Some(Rc::clone(&monitor)));
        monitor
    }

    /// Test-only entry to run the production monitor over a live task without
    /// starting the whole pipeline. Returns the monitor and its join handle.
    #[cfg(test)]
    pub fn start_monitor_for_test(
        &self,
        guid: &str,
        names: &[String],
    ) -> (Rc<Monitor>, JoinHandle<()>) {
        let monitor = Rc::new(Monitor::new(
            Rc::clone(&self.inner),
            guid.to_string(),
            Arc::clone(&self.inner.clock),
        ));
        monitor.set_vote_ns(names);
        let cancel = self.inner.cancel_channel(guid);
        let handle = tokio::task::spawn_local(Rc::clone(&monitor).run(cancel));
        self.inner
            .monitors
            .borrow_mut()
            .insert(guid.to_string(), Rc::clone(&monitor));
        self.inner.last_monitor.replace(Some(Rc::clone(&monitor)));
        (monitor, handle)
    }

    fn spawn_scan_workers(
        &self,
        guid: &str,
        monitor: &Rc<Monitor>,
        count: usize,
        policy: util::ProxyPolicy,
    ) -> Result<(), AppError> {
        for index in 1..=count {
            let name = format!("scan-{index}");
            let filters = task_filters(&self.inner)?;
            let session = self
                .inner
                .worker_session(policy, Duration::from_secs(10), 3);
            let alive = Arc::new(AtomicBool::new(true));
            let reference = WorkerRef {
                name: name.clone(),
                alive: Arc::clone(&alive),
                speed: None,
                zombie_threshold: 10.0 * 4.0,
            };
            monitor.register(reference.clone());
            let cancel = self.inner.cancel_channel(guid);
            let inner = Rc::clone(&self.inner);
            let worker_guid = guid.to_string();
            let slot_guid = worker_guid.clone();
            let monitor = Rc::clone(monitor);
            self.inner
                .push_tracked(consts::TASK_STATE_SCAN_IMG, slot_guid, async move {
                    scan_worker(
                        inner,
                        worker_guid,
                        name,
                        cancel,
                        monitor,
                        session,
                        filters,
                        reference,
                    )
                    .await
                });
        }
        Ok(())
    }

    fn spawn_download_workers(
        &self,
        guid: &str,
        monitor: &Rc<Monitor>,
        count: usize,
        policy: util::ProxyPolicy,
    ) -> Result<(), AppError> {
        for index in 1..=count {
            let name = format!("down-{index}");
            let filters = task_filters(&self.inner)?;
            let (timeout, threshold) = {
                let app = self.inner.app.borrow();
                match app.task(guid) {
                    Some(task) => (
                        Duration::from_secs(task.config.download_timeout.max(1) as u64),
                        task.config.low_speed_threshold * 1024,
                    ),
                    None => (Duration::from_secs(10), 0),
                }
            };
            let session = self.inner.worker_session(policy, timeout, 3);
            let speed = Arc::new(Mutex::new(SpeedChecker::new(
                5,
                Arc::clone(&self.inner.clock),
            )));
            let alive = Arc::new(AtomicBool::new(true));
            let reference = WorkerRef {
                name: name.clone(),
                alive: Arc::clone(&alive),
                speed: Some(Arc::clone(&speed)),
                zombie_threshold: timeout.as_secs_f64() * 4.0,
            };
            monitor.register(reference.clone());
            let cancel = self.inner.cancel_channel(guid);
            let inner = Rc::clone(&self.inner);
            let worker_guid = guid.to_string();
            let slot_guid = worker_guid.clone();
            let monitor = Rc::clone(monitor);
            self.inner
                .push_tracked(consts::TASK_STATE_DOWNLOAD, slot_guid, async move {
                    download_worker(
                        inner,
                        worker_guid,
                        name,
                        cancel,
                        monitor,
                        session,
                        filters,
                        speed,
                        threshold,
                        reference,
                    )
                    .await
                });
        }
        Ok(())
    }

    fn spawn_archive_worker(&self, guid: &str, token: (u64, u64)) -> Result<(), AppError> {
        let cancel = self.inner.cancel_channel(guid);
        let inner = Rc::clone(&self.inner);
        let worker_guid = guid.to_string();
        let slot_guid = worker_guid.clone();
        self.inner
            .push_tracked(consts::TASK_STATE_MAKE_ARCHIVE, slot_guid, async move {
                archive_worker(inner, worker_guid, cancel, token).await
            });
        Ok(())
    }
}

/// Live operations over the shared App. Each method borrows the App only for
/// the synchronous mutation and never holds it over an await; the async
/// variants additionally await the owned worker retirement.
#[derive(Clone)]
pub struct AppHandle {
    inner: Rc<RuntimeInner>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DrainReport {
    pub joined: usize,
    pub blocking_pending: usize,
    pub native_pending: usize,
}

impl AppHandle {
    pub fn borrow_app<R>(&self, f: impl FnOnce(&App) -> R) -> R {
        f(&self.inner.app.borrow())
    }

    pub fn borrow_app_mut<R>(&self, f: impl FnOnce(&mut App) -> R) -> R {
        f(&mut self.inner.app.borrow_mut())
    }

    pub fn task_state(&self, guid: &str) -> Option<i64> {
        self.inner.state(guid)
    }

    pub fn pause_task(&self, guid: &str) -> Result<(i64, Option<String>), AppError> {
        let result = self.inner.app.borrow_mut().pause_task(guid)?;
        if result.0 == consts::ERR_NO_ERROR {
            self.inner.signal_cancel(guid);
            self.inner.monitor_exit(guid);
        }
        Ok(result)
    }

    /// Mark paused, cancel in-flight network/stream work, then await every
    /// owned worker of this task plus any queued blocking stage open.
    pub async fn pause_task_and_wait(&self, guid: &str) -> Result<DrainReport, AppError> {
        let result = self.pause_task(guid)?;
        if result.0 != consts::ERR_NO_ERROR {
            return Ok(DrainReport {
                joined: 0,
                blocking_pending: self.inner.stage.pending(),
                native_pending: self.inner.stage.native_pending(),
            });
        }
        let mut joined = 0usize;
        // Shared observations: the task loop may hold the same completions
        // in its own local while this drain waits on them.
        joined += self
            .inner
            .wait_and_reap(self.inner.snapshot_guid_slots(guid))
            .await;
        self.inner.stage.wait_zero().await;
        Ok(DrainReport {
            joined,
            blocking_pending: self.inner.stage.pending(),
            native_pending: self.inner.stage.native_pending(),
        })
    }

    pub fn resume_task(&self, guid: &str) -> Result<(i64, Option<String>), AppError> {
        self.inner.app.borrow_mut().resume_task(guid)
    }

    /// Delete a task only after its workers are gone. A running task keeps the
    /// reference `ERR_DELETE_RUNNING_TASK` answer.
    pub async fn delete_task_and_wait(
        &self,
        guid: &str,
    ) -> Result<(i64, Option<String>), AppError> {
        let state = self.inner.state(guid);
        if state.is_none() {
            return Ok((consts::ERR_TASK_NOT_FOUND, None));
        }
        if let Some(state) = state
            && consts::TASK_STATE_PAUSED < state
            && state < consts::TASK_STATE_FINISHED
        {
            return Ok((consts::ERR_DELETE_RUNNING_TASK, None));
        }
        self.inner.signal_cancel(guid);
        self.inner.monitor_exit(guid);
        self.inner
            .wait_and_reap(self.inner.snapshot_guid_slots(guid))
            .await;
        self.inner.stage.wait_zero().await;
        let result = self.inner.app.borrow_mut().del_task(guid)?;
        if result.0 == consts::ERR_NO_ERROR {
            self.inner.cancels.borrow_mut().remove(guid);
            self.inner.monitors.borrow_mut().remove(guid);
            if self
                .inner
                .last_monitor
                .borrow()
                .as_ref()
                .is_some_and(|monitor| monitor.guid == guid)
            {
                self.inner.last_monitor.replace(None);
            }
        }
        Ok(result)
    }

    pub fn update_config(&self, patch: serde_json::Map<String, Value>) -> Result<(), AppError> {
        self.inner.app.borrow_mut().update_config(patch)
    }

    pub fn set_cookie(&self, cookie: &str) -> (i64, Option<String>) {
        self.inner.app.borrow_mut().set_cookie(cookie)
    }

    pub fn save_session(&self) -> (i64, Option<String>) {
        self.inner.app.borrow_mut().save_session()
    }

    /// Full App operations that do not need an await (config/cookie/session
    /// observation, login state, queue order).
    pub fn cookies(&self) -> Vec<(String, String)> {
        self.inner.app.borrow().cookies().to_vec()
    }

    pub fn queue_len(&self) -> usize {
        self.inner.app.borrow().queue_len()
    }

    pub fn last_task_guid(&self) -> Option<String> {
        self.inner.app.borrow().last_task_guid().map(str::to_string)
    }

    /// Zero `getInfo` view without a monitor (`rpc.py:415-424` defaults).
    /// The RPC layer calls this when no runtime monitor is mounted.
    pub fn fallback_info() -> Value {
        json!({
            "version": consts::VERSION_DISPLAY,
            "threads_zombie": 0,
            "threads_running": 0,
            "queue_pending": 0,
            "queue_finished": 0,
            "download_speed": 0,
        })
    }

    pub fn get_info(&self) -> Value {
        let mut info = Self::fallback_info();
        let monitor = self.inner.last_monitor.borrow().clone();
        if let Some(monitor) = monitor {
            let running = monitor.running_count();
            let zombies = monitor.zombie_count();
            info["threads_running"] = json!(running);
            info["threads_zombie"] = json!(zombies);
            let state = self.inner.state(&monitor.guid).unwrap_or(0);
            let pending = self.inner.img_len(&monitor.guid);
            if state > consts::TASK_STATE_PAUSED && pending > 0 {
                info["queue_pending"] = json!(pending);
                info["queue_finished"] = json!(self.inner.finished(&monitor.guid));
                info["download_speed"] = json!(monitor.download_speed());
            }
        }
        info
    }

    pub fn trace(&self) -> Vec<TraceEvent> {
        self.inner.trace.borrow().clone()
    }

    /// Owned M5 RPC start: bind the configured interface/port and serve the
    /// real dispatcher on this `LocalSet` until `shutdown` fires. Bind errors
    /// are real (an occupied or invalid port errors here); the server is
    /// never marked started without a listener. Startup publishes the actual
    /// bound address once, after `SharedRpc` is initialized, so an owner (the
    /// WebUI opener) can act on this process's readiness instead of probing
    /// the port. A missing `rpc_port` is a configuration error, not a silent
    /// no-op.
    pub async fn serve_rpc(
        &self,
        shutdown: watch::Receiver<bool>,
        static_origin: String,
        ready: tokio::sync::oneshot::Sender<std::net::SocketAddr>,
    ) -> Result<(), crate::rpc::ServerError> {
        let (interface, port, secret, base_dir, resource_root, clock) = self
            .inner
            .app
            .borrow()
            .rpc_bind_parts(&self.inner.resource_root, Arc::clone(&self.inner.clock));
        let port = port.ok_or_else(|| {
            crate::rpc::ServerError::Shape("rpc_port is not configured".to_string())
        })?;
        if !(1..=65535).contains(&port) {
            return Err(crate::rpc::ServerError::Shape(format!(
                "rpc_port {port} is out of range"
            )));
        }
        crate::rpc::server::serve(
            &interface,
            port as u16,
            &base_dir,
            &resource_root,
            clock,
            secret,
            static_origin,
            shutdown,
            self.clone(),
            ready,
        )
        .await
    }

    /// Test-only view of the per-state handle collection. Used to prove the
    /// archive slot is joined/reaped rather than accumulated.
    #[cfg(test)]
    pub fn slot_count(&self, state: i64) -> usize {
        if !(0..20).contains(&state) {
            return 0;
        }
        self.inner.threads.borrow()[state as usize].len()
    }

    /// Test-only handle to the production download port for stale-result
    /// checks (same run marker and same-guid new owner).
    #[cfg(test)]
    pub fn shared_port(&self, guid: &str, token: (u64, u64)) -> super::runtime::SharedPort {
        super::runtime::SharedPort {
            inner: Rc::clone(&self.inner),
            guid: guid.to_string(),
            token,
        }
    }

    /// Stop admission, cancel every in-flight task, await all owned logical
    /// workers (including a running blocking archive), then save the session
    /// twice like `_cleanup`.
    pub async fn shutdown(&self) -> DrainReport {
        self.inner
            .app
            .borrow_mut()
            .request_exit(consts::XEH_STATE_FULL_EXIT);
        for sender in self.inner.cancels.borrow().values() {
            let _ = sender.send(true);
        }
        for monitor in self.inner.monitors.borrow().values() {
            monitor.request_exit();
        }
        let waiting = self.inner.snapshot_all_slots();
        // Shared observations: a task loop may hold the same completions in
        // its own local while shutdown waits on them. Nothing is detached,
        // so this future stays pending until every owned worker retires.
        let joined = self.inner.wait_and_reap(waiting).await;
        self.inner.stage.wait_zero().await;
        self.inner.stage.wait_native().await;
        self.inner.save_session();
        self.inner.save_session();
        DrainReport {
            joined,
            blocking_pending: self.inner.stage.pending(),
            native_pending: self.inner.stage.native_pending(),
        }
    }

    /// Join every owned native media worker's actual retirement. Explicit
    /// public drain used by the server's own stop path and by tests; a
    /// cancelled observer leaves the records visible for the next drain.
    pub async fn wait_native(&self) -> usize {
        self.inner.stage.wait_native().await
    }

    /// Reap native records whose workers already finished, without waiting
    /// for active ones. Called before a new submission and by ordinary
    /// traffic so abandoned requests cannot grow the record vector.
    pub(crate) async fn reap_native_finished(&self) {
        self.inner.stage.reap_native_finished().await;
    }

    /// Admit one RPC native job. The caller has already captured the exact
    /// task owner/run in a short borrow and acquired its heavy permit; this
    /// section checks the live App is still RUNNING and that the exact run
    /// still owns the guid, then spawns and registers with no await gap.
    pub(crate) fn spawn_native_job<T, F, D>(
        &self,
        guid: &str,
        token: (u64, u64),
        job: F,
        undelivered: D,
    ) -> Option<super::NativeHandle<T>>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
        D: FnOnce(T) + Send + 'static,
    {
        let app = self.inner.app.borrow();
        if app.exit_state() != consts::XEH_STATE_RUNNING || !app.task_run_matches(guid, token) {
            return None;
        }
        let handle = self.inner.stage.spawn_native(job, undelivered);
        drop(app);
        Some(handle)
    }

    /// Publish an RPC-prepared archive only for the exact captured run. The
    /// reader has no minimum task state (`rpc.py:477-486`), so `None` gates
    /// on identity alone; the M4 worker keeps `Some(MAKE_ARCHIVE)`. One
    /// borrow covers identity, the optional state floor and the bounded
    /// namespace effects.
    pub(crate) fn commit_archive_exact(
        &self,
        guid: &str,
        token: (u64, u64),
        prepared: crate::task::PreparedArchive,
        min_state: Option<i64>,
    ) -> Result<ArchiveCommit, crate::task::TaskError> {
        self.inner.commit_archive(guid, token, prepared, min_state)
    }

    /// Test-only native record count.
    #[cfg(test)]
    pub fn native_pending(&self) -> usize {
        self.inner.stage.native_pending()
    }
}

/// Immutable plan for one `_do_task` entry.
pub(crate) struct RunInputs {
    pub url: String,
    pub token: (u64, u64),
    pub policy: util::ProxyPolicy,
    pub scan_threads: i64,
    pub download_threads: i64,
    pub make_archive: bool,
    pub ori: bool,
}

/// Snapshot produced by the SCAN_PAGE step; the network loop is driven by
/// `_do_task`, not by this value.
pub(crate) struct PagePlan {
    pub url: String,
    pub use_mpv: bool,
    pub total: i64,
    pub thumbnail_cnt: i64,
    pub finished: i64,
    pub pages: usize,
    pub mpv_url: String,
}

pub(crate) enum PageScanStart {
    Finished,
    Plan(PagePlan),
}

impl App {
    /// Start one task run: WAITING -> GET_META, queues, a fresh local run
    /// token and the resume repair that rebuilds `flist_done` from the real
    /// files before any network request (registered M4 correction: the
    /// reference only scans when the removed `filelist` key is present).
    pub(crate) fn begin_run(&mut self, guid: &str) -> Result<RunInputs, AppError> {
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        if task.state == consts::TASK_STATE_WAITING {
            task.state = consts::TASK_STATE_GET_META;
        }
        if task.page_q.is_none() {
            task.page_q = Some(VecDeque::new());
        }
        if task.img_q.is_none() {
            task.img_q = Some(VecDeque::new());
        }
        let resumed = task.state >= consts::TASK_STATE_SCAN_IMG;
        let token = task.bump_run();
        if resumed
            && task.flist_done().is_empty()
            && task.meta.contains_key("total")
            && task.meta.contains_key("title")
        {
            task.scan_downloaded()?;
        }
        let config = task.config.clone();
        let policy = util::get_proxy_policy(config.proxy_image_only, config.proxy_image);
        Ok(RunInputs {
            url: task.url.clone(),
            token: (task.token().0, token),
            policy,
            scan_threads: config.scan_thread_cnt,
            download_threads: config.download_thread_cnt,
            make_archive: config.make_archive,
            ori: config.download_ori && self.has_login,
        })
    }

    pub(crate) fn apply_metadata(
        &mut self,
        guid: &str,
        parsed: MetadataOutcome,
    ) -> Result<Option<i64>, AppError> {
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        if let Some(code) = parsed.fail_code {
            task.set_fail(code);
            if let Some(ban) = &parsed.ban {
                let message = format!("ip banned for {ban}; task {guid}");
                let _ = self.logger.warning(&message);
            }
            return Ok(Some(code));
        }
        if let Some(kind) = &parsed.exception {
            return Err(AppError::Filter(format!("metadata raised {kind}")));
        }
        if let Some(meta) = parsed.meta {
            let mut map = serde_json::Map::new();
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
        Ok(None)
    }

    pub(crate) fn begin_page_scan(&mut self, guid: &str) -> Result<PageScanStart, AppError> {
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        task.scan_downloaded()?;
        if task.state == consts::TASK_STATE_FINISHED {
            return Ok(PageScanStart::Finished);
        }
        let use_mpv = task
            .meta
            .get("use_multipage_viewer")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let total = task.meta_i64("total")?;
        let thumbnail_cnt = task.meta_i64("thumbnail_cnt")?;
        let finished = task.meta_i64("finished")?;
        if !use_mpv && thumbnail_cnt <= 0 {
            return Err(AppError::State("thumbnail_cnt must be positive".into()));
        }
        let pages = if use_mpv {
            0
        } else {
            ((total + thumbnail_cnt - 1) / thumbnail_cnt).max(0) as usize
        };
        Ok(PageScanStart::Plan(PagePlan {
            url: task.url.clone(),
            use_mpv,
            total,
            thumbnail_cnt,
            finished,
            pages,
            mpv_url: task.mpv_url(),
        }))
    }

    pub(crate) fn apply_page(
        &mut self,
        guid: &str,
        parsed: &PageOutcome,
    ) -> Result<bool, AppError> {
        if let Some(body) = &parsed.debug_text {
            let _ = std::fs::write(self.base_dir.join("debug.htm"), body);
        }
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        for page in &parsed.urls {
            task.put_page_queue(page)?;
        }
        if let Some(code) = parsed.fail_code {
            task.set_fail(code);
            return Ok(true);
        }
        Ok(parsed.exception.is_some())
    }

    pub(crate) fn begin_scan_item(&mut self, guid: &str) -> Option<ScanItem> {
        let task = self.all_tasks.get_mut(guid)?;
        let page_url = task.page_q.as_mut()?.pop_front()?;
        Some(ScanItem {
            page_url,
            policy: util::get_proxy_policy(task.config.proxy_image_only, task.config.proxy_image),
            token: task.token(),
            ori: task.config.download_ori && self.has_login,
        })
    }

    pub(crate) fn put_scanned_image(
        &mut self,
        guid: &str,
        token: (u64, u64),
        image_url: &str,
        reload_url: &str,
        filename: &str,
    ) -> Result<(), AppError> {
        if !self.task_is_current(guid, token) {
            return Ok(());
        }
        let task = self
            .all_tasks
            .get_mut(guid)
            .ok_or_else(|| AppError::State(format!("unknown task {guid}")))?;
        task.put_img_queue(image_url, reload_url, filename)
            .map_err(AppError::Task)
    }

    fn requeue_page_if_interrupted(&mut self, guid: &str, _token: (u64, u64), url: &str) {
        let Some(task) = self.all_tasks.get_mut(guid) else {
            return;
        };
        if task.state == consts::TASK_STATE_PAUSED {
            if task.page_q.is_none() {
                task.page_q = Some(VecDeque::new());
            }
            if let Some(queue) = task.page_q.as_mut() {
                queue.push_front(url.to_string());
            }
            task.page_waiter().notify_one();
        }
    }

    fn requeue_image_if_interrupted(&mut self, guid: &str, _token: (u64, u64), url: &str) {
        let Some(task) = self.all_tasks.get_mut(guid) else {
            return;
        };
        if task.state == consts::TASK_STATE_PAUSED {
            if task.img_q.is_none() {
                task.img_q = Some(VecDeque::new());
            }
            if let Some(queue) = task.img_q.as_mut() {
                queue.push_front(url.to_string());
            }
            task.img_waiter().notify_one();
        }
    }

    pub(crate) fn existing_image(
        &mut self,
        guid: &str,
        token: (u64, u64),
        image_url: &str,
    ) -> Option<(i64, PathBuf)> {
        if !self.task_is_current(guid, token) {
            return None;
        }
        let task = self.all_tasks.get(guid)?;
        task.existing_image(image_url).ok().flatten()
    }
}

/// One filters VM for a worker of this run: the existing per-worker live load.
fn task_filters(inner: &RuntimeInner) -> Result<Filters, AppError> {
    Filters::load(&inner.resource_root).map_err(|error| AppError::Filter(error.to_string()))
}

fn producer_filters(inner: &RuntimeInner) -> Result<Filters, AppError> {
    task_filters(inner)
}

pub(crate) struct ScanItem {
    pub page_url: String,
    pub policy: util::ProxyPolicy,
    pub token: (u64, u64),
    pub ori: bool,
}

/// One download worker's live Task port. Every method takes a fresh short
/// borrow, checks the captured token and then mutates exactly like the
/// reference callback would.
pub(crate) struct SharedPort {
    pub inner: Rc<RuntimeInner>,
    pub guid: String,
    pub token: (u64, u64),
}

impl SharedPort {
    fn with_task<R>(&self, f: impl FnOnce(&mut Task) -> R) -> Option<R> {
        if !self.inner.is_current(&self.guid, self.token) {
            return None;
        }
        self.inner.app.borrow_mut().task_mut(&self.guid).map(f)
    }
}

impl DownloadPort for SharedPort {
    fn validate(&self) -> bool {
        self.inner.is_current(&self.guid, self.token)
    }

    fn begin_save(&mut self, imgurl: &str, redirect_url: &str) -> Result<SaveStart, TaskError> {
        self.with_task(|task| task.begin_staged_save(imgurl, redirect_url))
            .unwrap_or(Err(TaskError::QueueNotStarted("stale download")))
    }

    fn retry_page(&mut self, url: &str) -> Result<bool, TaskError> {
        match self.with_task(|task| task.put_page_queue_retry(url)) {
            None => Ok(false),
            Some(result) => {
                result?;
                Ok(true)
            }
        }
    }

    fn commit(&mut self, staged: StagedSave) -> Result<CommitOutcome, TaskError> {
        if !self.inner.is_current(&self.guid, self.token) {
            Task::abort_staged_save(staged);
            return Ok(CommitOutcome::Rejected);
        }
        let mut app = self.inner.app.borrow_mut();
        if !app.task_is_current(&self.guid, self.token) {
            drop(app);
            Task::abort_staged_save(staged);
            return Ok(CommitOutcome::Rejected);
        }
        match app.task_mut(&self.guid) {
            Some(task) => Ok(CommitOutcome::Published(task.commit_staged_save(staged)?)),
            None => {
                drop(app);
                Task::abort_staged_save(staged);
                Ok(CommitOutcome::Rejected)
            }
        }
    }
}

/// One worker record shared with the monitor: identity, liveness, optional
/// stream speed ring and the reference zombie threshold.
#[derive(Clone)]
pub struct WorkerRef {
    pub name: String,
    pub alive: Arc<AtomicBool>,
    pub speed: Option<Arc<Mutex<SpeedChecker>>>,
    pub zombie_threshold: f64,
}

/// `Monitor` from `worker.py:279-438`. The same algorithm runs against an
/// injected clock; production passes `RealClock`, tests pass `TickClock`.
pub struct Monitor {
    inner: Rc<RuntimeInner>,
    pub guid: String,
    clock: Arc<dyn HttpClock>,
    votes: std::cell::RefCell<BTreeMap<i64, usize>>,
    cleared: HashSet<i64>,
    last_seen: std::cell::RefCell<BTreeMap<String, f64>>,
    refs: std::cell::RefCell<BTreeMap<String, WorkerRef>>,
    zombies: std::cell::RefCell<HashSet<String>>,
    download_speed: Cell<f64>,
    exiting: Cell<bool>,
    #[cfg(test)]
    tick_limit: Cell<Option<u64>>,
    #[cfg(test)]
    ticks: Cell<u64>,
    #[cfg(test)]
    limit_hit: Cell<bool>,
}

impl Monitor {
    pub(crate) fn new(inner: Rc<RuntimeInner>, guid: String, clock: Arc<dyn HttpClock>) -> Self {
        let ignored = inner
            .app
            .borrow()
            .task(&guid)
            .map(|task| task.config.ignored_errors.iter().copied().collect())
            .unwrap_or_default();
        Self {
            inner,
            guid,
            clock,
            votes: std::cell::RefCell::new(BTreeMap::new()),
            cleared: ignored,
            last_seen: std::cell::RefCell::new(BTreeMap::new()),
            refs: std::cell::RefCell::new(BTreeMap::new()),
            zombies: std::cell::RefCell::new(HashSet::new()),
            download_speed: Cell::new(0.0),
            exiting: Cell::new(false),
            #[cfg(test)]
            tick_limit: Cell::new(None),
            #[cfg(test)]
            ticks: Cell::new(0),
            #[cfg(test)]
            limit_hit: Cell::new(false),
        }
    }

    #[cfg(test)]
    pub fn set_tick_limit(&self, limit: u64) {
        self.tick_limit.set(Some(limit));
    }

    #[cfg(test)]
    pub fn limit_hit(&self) -> bool {
        self.limit_hit.get()
    }

    pub fn request_exit(&self) {
        self.exiting.set(true);
    }

    pub fn set_vote_ns(&self, names: &[String]) {
        let now = self.clock.now();
        let mut last_seen = self.last_seen.borrow_mut();
        for name in names {
            last_seen.insert(name.clone(), now);
        }
    }

    pub fn register(&self, reference: WorkerRef) {
        let now = self.clock.now();
        self.last_seen
            .borrow_mut()
            .entry(reference.name.clone())
            .or_insert(now);
        self.refs
            .borrow_mut()
            .insert(reference.name.clone(), reference);
    }

    pub fn vote(&self, tname: &str, code: i64) {
        let mut votes = self.votes.borrow_mut();
        let count = votes.entry(code).or_insert(0);
        *count += 1;
        let count = *count;
        drop(votes);
        if code != consts::ERR_NO_ERROR {
            self.inner.log(VERBOSE, &format!("t-{tname} vote:{code}"));
        }
        self.inner.record(
            "monitor.vote",
            json!({"thread": tname, "code": code, "count": count}),
        );
    }

    /// `wrk_keepalive`: returns true when this worker must stop. A heartbeat
    /// re-enters life and clears its zombie marker (the reference passes the
    /// live thread object, so a late heartbeat can re-register it).
    pub fn keepalive(&self, reference: &WorkerRef, exit: bool) -> bool {
        let name = reference.name.as_str();
        self.zombies.borrow_mut().remove(name);
        let finished = self.inner.finished(&self.guid);
        let total = self.inner.total(&self.guid);
        let state = self
            .inner
            .state(&self.guid)
            .unwrap_or(consts::TASK_STATE_FAILED);
        let alive = reference.alive.load(Ordering::SeqCst);
        let stop = finished == total
            || state == consts::TASK_STATE_FINISHED
            || state == consts::TASK_STATE_FAILED
            || self.exiting.get()
            || exit
            || !alive;
        if stop {
            self.last_seen.borrow_mut().remove(name);
            self.refs.borrow_mut().remove(name);
        } else {
            self.last_seen
                .borrow_mut()
                .insert(name.to_string(), self.clock.now());
            self.refs
                .borrow_mut()
                .entry(name.to_string())
                .or_insert_with(|| reference.clone());
        }
        stop
    }

    fn check_vote(&self) {
        let votes = self.votes.borrow();
        let Some(&quota) = votes.get(&consts::ERR_QUOTA_EXCEEDED) else {
            return;
        };
        if self.cleared.contains(&consts::ERR_QUOTA_EXCEEDED) {
            return;
        }
        let live = self.last_seen.borrow().len();
        if quota >= live {
            let message = localized("TASK_STOP_QUOTA_EXCEEDED", &[&self.guid]);
            drop(votes);
            self.inner.log(ERROR, &message);
            self.inner.set_state(&self.guid, consts::TASK_STATE_FAILED);
            self.inner
                .record("monitor.quota", json!({"votes": quota, "live": live}));
        }
    }

    fn rename_and_finish(&self) {
        let errors = {
            let mut app = self.inner.app.borrow_mut();
            match app.task_mut(&self.guid) {
                Some(task) => task.rename_fname(),
                None => return,
            }
        };
        match errors {
            Ok(errors) => {
                if !errors.is_empty() {
                    let rows: Vec<String> = errors
                        .iter()
                        .map(|error| format!("{} => {} : {}", error.from, error.to, error.message))
                        .collect();
                    let message = localized("XEH_RENAME_HAS_ERRORS", &[&rows.join("\n")]);
                    self.inner.log(WARNING, &message);
                }
                let message = localized("TASK_FINISHED", &[&self.guid]);
                self.inner.log(INFO, &message);
                self.inner
                    .set_state(&self.guid, consts::TASK_STATE_FINISHED);
                self.inner
                    .record("monitor.finished", json!({"task": self.guid}));
            }
            Err(error) => {
                self.inner.log(
                    WARNING,
                    &format!("task #{} rename failed: {error}", self.guid),
                );
            }
        }
        let mut app = self.inner.app.borrow_mut();
        if let Some(task) = app.task_mut(&self.guid) {
            let _ = task.cleanup(false);
        }
    }

    pub async fn run(self: Rc<Self>, mut cancel: watch::Receiver<bool>) {
        let mut intv = 0u32;
        let mut last_change = self.clock.now();
        let mut last_finished = -1i64;
        while !self.last_seen.borrow().is_empty() {
            if self.exiting.get() || *cancel.borrow() {
                break;
            }
            #[cfg(test)]
            self.inner.record("monitor.tick", json!({}));
            intv += 1;
            self.check_vote();
            let mut total_speed = 0.0f64;
            let snapshot: Vec<(String, f64)> = self
                .last_seen
                .borrow()
                .iter()
                .map(|(name, seen)| (name.clone(), *seen))
                .collect();
            for (name, last_seen) in snapshot {
                let reference = self.refs.borrow().get(&name).cloned();
                let threshold = reference
                    .as_ref()
                    .map(|reference| reference.zombie_threshold)
                    .unwrap_or(30.0);
                if self.clock.now() - last_seen > threshold {
                    let alive = reference
                        .as_ref()
                        .is_some_and(|reference| reference.alive.load(Ordering::SeqCst));
                    if alive {
                        let message = localized("THREAD_MAY_BECOME_ZOMBIE", &[&name]);
                        self.inner.log(WARNING, &message);
                        self.zombies.borrow_mut().insert(name.clone());
                    } else {
                        let message = localized("THREAD_SWEEP_OUT", &[&name]);
                        self.inner.log(WARNING, &message);
                    }
                    self.inner
                        .record("monitor.zombie", json!({"thread": name, "alive": alive}));
                    self.last_seen.borrow_mut().remove(&name);
                } else if let Some(reference) = reference
                    && let Some(speed) = &reference.speed
                {
                    total_speed += speed.lock().unwrap_or_else(|e| e.into_inner()).calc(false);
                }
            }
            self.download_speed.set(total_speed);
            if intv == 10 {
                intv = 0;
                let finished = self.inner.finished(&self.guid);
                let pending = self.inner.img_len(&self.guid);
                let running = self.last_seen.borrow().len();
                let zombies = self.zombies.borrow().len();
                let message = format!(
                    "{} {}R/{}Z, {} {}R/{}D, {}/s",
                    crate::i18n::render(crate::i18n::Locale::EnUs, "THREAD", &[])
                        .unwrap_or_else(|| "thread".into()),
                    running,
                    zombies,
                    crate::i18n::render(crate::i18n::Locale::EnUs, "QUEUE", &[])
                        .unwrap_or_else(|| "queue".into()),
                    pending,
                    finished,
                    util::human_size(total_speed as i64),
                );
                self.inner.log(INFO, &message);
                self.inner.record(
                    "monitor.stats",
                    json!({"running": running, "zombies": zombies, "pending": pending,
                           "finished": finished, "speed": total_speed}),
                );
                if last_finished != finished {
                    last_change = self.clock.now();
                    last_finished = finished;
                } else if self.clock.now() - last_change > 90.0 {
                    let unfinished = self.inner.unfinished(&self.guid);
                    let message =
                        localized("TASK_UNFINISHED", &[&self.guid, &format!("{unfinished:?}")]);
                    self.inner.log(INFO, &message);
                    if total_speed > 0.0 {
                        last_change = self.clock.now();
                        let message = localized("TASK_SLOW", &[&self.guid]);
                        self.inner.log(WARNING, &message);
                        self.inner.record(
                            "monitor.slow",
                            json!({"task": self.guid, "finished": finished,
                                   "speed": total_speed}),
                        );
                    } else {
                        let message = localized("TASK_STUCK", &[&self.guid]);
                        self.inner.log(WARNING, &message);
                        self.inner.record(
                            "monitor.stuck",
                            json!({"task": self.guid, "finished": finished,
                                   "unfinished": unfinished}),
                        );
                        break;
                    }
                }
            }
            tokio::select! {
                biased;
                _ = cancel.changed() => break,
                _ = self.clock.sleep(0.5) => {}
            }
            #[cfg(test)]
            {
                let next = self.ticks.get() + 1;
                self.ticks.set(next);
                if self.tick_limit.get().is_some_and(|limit| next >= limit) {
                    self.limit_hit.set(true);
                    break;
                }
            }
        }
        self.download_speed.set(0.0);
        let paused = self.inner.state(&self.guid) == Some(consts::TASK_STATE_PAUSED);
        if self.inner.finished(&self.guid) == self.inner.total(&self.guid) && !paused {
            self.rename_and_finish();
        } else {
            let mut app = self.inner.app.borrow_mut();
            if let Some(task) = app.task_mut(&self.guid) {
                let _ = task.cleanup(false);
            }
        }
    }

    pub fn running_count(&self) -> usize {
        self.last_seen.borrow().len()
    }

    pub fn zombie_count(&self) -> usize {
        self.zombies.borrow().len()
    }

    pub fn download_speed(&self) -> f64 {
        self.download_speed.get()
    }
}

fn scan_failed_through_filter(
    filters: &mut Filters,
    page_url: &str,
    ori: bool,
    cause: Option<&str>,
) -> Result<ImageOutcome, crate::filters::FilterError> {
    if let Some(cause) = cause {
        let _ = cause;
    }
    filters.imgurl_outcome(&PageResponse {
        status: 600,
        url: page_url.to_string(),
        real_url: page_url.to_string(),
        content_length: 0,
        text: String::new(),
        headers: Vec::new(),
        ori,
        has_stream: false,
    })
}

#[allow(clippy::too_many_arguments)]
async fn scan_worker(
    inner: Rc<RuntimeInner>,
    guid: String,
    name: String,
    mut cancel: watch::Receiver<bool>,
    monitor: Rc<Monitor>,
    mut session: Session,
    mut filters: Filters,
    reference: WorkerRef,
) {
    loop {
        if *cancel.borrow() || monitor.keepalive(&reference, false) {
            break;
        }
        let item = { inner.app.borrow_mut().begin_scan_item(&guid) };
        let Some(item) = item else {
            tokio::select! {
                biased;
                _ = cancel.changed() => break,
                _ = inner.clock.sleep(1.0) => {}
            }
            continue;
        };
        if *cancel.borrow() || !inner.is_current(&guid, item.token) {
            inner
                .app
                .borrow_mut()
                .requeue_page_if_interrupted(&guid, item.token, &item.page_url);
            break;
        }
        session.proxy_policy = item.policy;
        let request = session.request("GET", &item.page_url, None, false);
        let outcome = tokio::select! {
            biased;
            _ = cancel.changed() => {
                inner.app.borrow_mut().requeue_page_if_interrupted(&guid, item.token, &item.page_url);
                break;
            }
            result = request => result,
        };
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(error) => {
                inner.log(
                    WARNING,
                    &format!("t-{name} {} {}: {error}", "GET", item.page_url),
                );
                let parsed = scan_failed_through_filter(
                    &mut filters,
                    &item.page_url,
                    item.ori,
                    Some(&error.to_string()),
                );
                if let Ok(parsed) = parsed {
                    let code = parsed.fail_code.unwrap_or(consts::ERR_CONNECTION_ERROR);
                    monitor.vote(&name, code);
                    let message = localized(
                        "XEH_SCAN_FAILED",
                        &[&name, &item.page_url, &error_message(code)],
                    );
                    inner.log(WARNING, &message);
                }
                continue;
            }
        };
        if !inner.is_current(&guid, item.token) {
            break;
        }
        let view = App::page_view(&outcome.response, item.ori, false);
        let parsed = match filters.imgurl_outcome(&view) {
            Ok(parsed) => parsed,
            Err(error) => {
                inner.log(WARNING, &format!("t-{name} uncaught exception: {error}"));
                match scan_failed_through_filter(
                    &mut filters,
                    &item.page_url,
                    item.ori,
                    Some(&error.to_string()),
                ) {
                    Ok(parsed) => {
                        let code = parsed.fail_code.unwrap_or(consts::ERR_CONNECTION_ERROR);
                        monitor.vote(&name, code);
                        let message = localized(
                            "XEH_SCAN_FAILED",
                            &[&name, &item.page_url, &error_message(code)],
                        );
                        inner.log(WARNING, &message);
                    }
                    Err(fake) => {
                        inner.log(WARNING, &format!("t-{name} fake filter: {fake}"));
                    }
                }
                continue;
            }
        };
        if let Some(code) = parsed.fail_code {
            monitor.vote(&name, code);
            let real = parsed.real_url.unwrap_or_else(|| item.page_url.clone());
            let url = parsed.url.unwrap_or_else(|| item.page_url.clone());
            let message = localized(
                "XEH_SCAN_FAILED",
                &[&name, &format!("{real} -> {url}"), &error_message(code)],
            );
            inner.log(WARNING, &message);
            continue;
        }
        if let Some(kind) = parsed.exception {
            inner.log(
                WARNING,
                &format!("t-{name} uncaught exception on {}: {kind}", item.page_url),
            );
            continue;
        }
        let (Some(image_url), Some(reload_url), Some(filename)) =
            (parsed.image_url, parsed.reload_url, parsed.filename)
        else {
            inner.log(
                WARNING,
                &format!("t-{name} filter returned no image fields"),
            );
            continue;
        };
        let applied = inner.app.borrow_mut().put_scanned_image(
            &guid,
            item.token,
            &image_url,
            &reload_url,
            &filename,
        );
        match applied {
            Ok(()) => {
                monitor.vote(&name, consts::ERR_NO_ERROR);
                inner.record(
                    "worker.scan.ok",
                    json!({"thread": name, "page": item.page_url, "image": image_url}),
                );
            }
            Err(error) => {
                inner.log(WARNING, &format!("t-{name} uncaught exception: {error}"));
                match scan_failed_through_filter(
                    &mut filters,
                    &item.page_url,
                    item.ori,
                    Some(&error.to_string()),
                ) {
                    Ok(parsed) => {
                        let code = parsed.fail_code.unwrap_or(consts::ERR_CONNECTION_ERROR);
                        monitor.vote(&name, code);
                    }
                    Err(_) => monitor.vote(&name, consts::ERR_CONNECTION_ERROR),
                }
            }
        }
    }
    reference.alive.store(false, Ordering::SeqCst);
    monitor.keepalive(&reference, true);
    inner.log(VERBOSE, &format!("t-{name} exit"));
}

#[allow(clippy::too_many_arguments)]
async fn download_worker(
    inner: Rc<RuntimeInner>,
    guid: String,
    name: String,
    mut cancel: watch::Receiver<bool>,
    monitor: Rc<Monitor>,
    mut session: Session,
    mut filters: Filters,
    speed: Arc<Mutex<SpeedChecker>>,
    low_speed_threshold: i64,
    reference: WorkerRef,
) {
    loop {
        if *cancel.borrow() || monitor.keepalive(&reference, false) {
            break;
        }
        let item = { inner.app.borrow_mut().begin_download_item(&guid) };
        let item = match item {
            Ok(Some(item)) => item,
            Ok(None) => {
                speed.lock().unwrap_or_else(|e| e.into_inner()).reset();
                tokio::select! {
                    biased;
                    _ = cancel.changed() => break,
                    _ = inner.clock.sleep(1.0) => {}
                }
                continue;
            }
            Err(error) => {
                inner.log(WARNING, &format!("t-{name} queue error: {error}"));
                break;
            }
        };
        let item_token = item.token;
        if *cancel.borrow() || !inner.is_current(&guid, item_token) {
            inner
                .app
                .borrow_mut()
                .requeue_image_if_interrupted(&guid, item_token, &item.image_url);
            break;
        }
        let existing = {
            inner
                .app
                .borrow_mut()
                .existing_image(&guid, item_token, &item.image_url)
        };
        if let Some((fid, _path)) = existing {
            let marked = {
                let mut app = inner.app.borrow_mut();
                if !app.task_is_current(&guid, item_token) {
                    false
                } else if let Some(task) = app.task_mut(&guid) {
                    task.set_fid_finished(fid);
                    true
                } else {
                    false
                }
            };
            if marked {
                monitor.vote(&name, consts::ERR_NO_ERROR);
                inner.record(
                    "worker.download.skip",
                    json!({"thread": name, "url": item.image_url, "fid": fid}),
                );
            }
            continue;
        }
        session.proxy_policy = item.policy;
        let mut port = SharedPort {
            inner: Rc::clone(&inner),
            guid: guid.clone(),
            token: item_token,
        };
        let mut runtime = DownloadRuntime {
            speed: Some(Arc::clone(&speed)),
            low_speed_threshold,
            cancel: Some(&mut cancel),
            stage_drain: Some(inner.stage.clone()),
        };
        speed.lock().unwrap_or_else(|e| e.into_inner()).reset();
        let result = run_download(
            &mut port,
            &mut session,
            &mut filters,
            &item.image_url,
            &mut runtime,
            &mut || monitor.keepalive(&reference, false),
        )
        .await;
        match result {
            Ok(ImageDownload::Saved(path)) => {
                monitor.vote(&name, consts::ERR_NO_ERROR);
                inner.log(DEBUG, &format!("t-{name} saved {}", path.display()));
                inner.record(
                    "worker.download.saved",
                    json!({"thread": name, "url": item.image_url}),
                );
            }
            Ok(ImageDownload::AlreadyDone(path)) => {
                monitor.vote(&name, consts::ERR_NO_ERROR);
                inner.log(DEBUG, &format!("t-{name} exists {}", path.display()));
                inner.record(
                    "worker.download.exists",
                    json!({"thread": name, "url": item.image_url}),
                );
            }
            Ok(ImageDownload::Failed {
                fail_code,
                real_url,
                url,
            }) => {
                monitor.vote(&name, fail_code);
                let fid = inner
                    .app
                    .borrow()
                    .task(&guid)
                    .and_then(|task| task.get_imgfid(&item.image_url).ok())
                    .unwrap_or(0);
                let message = localized(
                    "XEH_DOWNLOAD_HAS_ERROR",
                    &[&name, &fid.to_string(), &error_message(fail_code)],
                );
                inner.log(DEBUG, &message);
                inner.record(
                    "worker.download.fail",
                    json!({"thread": name, "url": url, "real": real_url, "code": fail_code}),
                );
            }
            Ok(ImageDownload::Aborted) => {
                inner.app.borrow_mut().requeue_image_if_interrupted(
                    &guid,
                    item_token,
                    &item.image_url,
                );
                inner.record(
                    "worker.download.aborted",
                    json!({"thread": name, "url": item.image_url}),
                );
                if *cancel.borrow() {
                    break;
                }
            }
            Ok(ImageDownload::Empty) => {}
            Err(error) => {
                inner.log(WARNING, &format!("t-{name} uncaught exception: {error}"));
                match drop_to_fake_download(&mut filters, &item.image_url) {
                    Ok((code, url)) => {
                        if inner.is_current(&guid, item_token) {
                            let _ = port.retry_page(&url);
                        }
                        monitor.vote(&name, code);
                    }
                    Err(fake) => {
                        inner.log(WARNING, &format!("t-{name} fake filter: {fake}"));
                        monitor.vote(&name, consts::ERR_CONNECTION_ERROR);
                    }
                }
            }
        }
        if *cancel.borrow() || !inner.is_current(&guid, item_token) {
            break;
        }
    }
    reference.alive.store(false, Ordering::SeqCst);
    monitor.keepalive(&reference, true);
    inner.log(VERBOSE, &format!("t-{name} exit"));
}

fn drop_to_fake_download(
    filters: &mut Filters,
    image_url: &str,
) -> Result<(i64, String), crate::filters::FilterError> {
    let parsed = filters.download_outcome(&PageResponse {
        status: 600,
        url: image_url.to_string(),
        real_url: image_url.to_string(),
        content_length: 0,
        text: String::new(),
        headers: Vec::new(),
        ori: false,
        has_stream: false,
    })?;
    Ok((
        parsed.fail_code.unwrap_or(consts::ERR_CONNECTION_ERROR),
        parsed.url.unwrap_or_else(|| image_url.to_string()),
    ))
}

async fn archive_worker(
    inner: Rc<RuntimeInner>,
    guid: String,
    mut cancel: watch::Receiver<bool>,
    token: (u64, u64),
) {
    loop {
        let state = {
            let app = inner.app.borrow();
            if !app.task_run_matches(&guid, token) {
                // A resumed/deleted/recreated run owns the guid now: this
                // worker must stop waiting for a completion that is not ours.
                return;
            }
            match app.task(&guid) {
                Some(task) => task.state,
                None => return,
            }
        };
        if state >= consts::TASK_STATE_FINISHED {
            break;
        }
        if state == consts::TASK_STATE_PAUSED
            || state == consts::TASK_STATE_FAILED
            || *cancel.borrow()
            || inner.exit() >= consts::XEH_STATE_FULL_EXIT
        {
            return;
        }
        tokio::select! {
            biased;
            _ = cancel.changed() => return,
            _ = inner.clock.sleep(1.0) => {}
        }
    }
    // Narrow test seam: hold the worker after it observed FINISHED but before
    // it re-validates the exact run at admission.
    #[cfg(test)]
    {
        let marker = inner
            .app
            .borrow()
            .task(&guid)
            .and_then(|task| task.get_fpath().ok())
            .map(|path| path.to_string_lossy().into_owned());
        if let Some(marker) = marker {
            crate::task::archive_waiting_gate(&marker).await;
        }
    }
    let snapshot = match inner.begin_archive(&guid, token) {
        ArchiveAdmission::Admitted(snapshot) => snapshot,
        ArchiveAdmission::Stale => {
            inner.record("archive.stale", json!({"task": guid, "stage": "admission"}));
            return;
        }
        ArchiveAdmission::Failed(error) => {
            let message = format!(
                "task #{guid} error: {}",
                localized("ERR_CANNOT_MAKE_ARCHIVE", &[])
            );
            inner.log(ERROR, &format!("{message}: {error}"));
            inner.record(
                "archive.failed",
                json!({"task": guid, "error": error.to_string()}),
            );
            return;
        }
    };
    let message = localized("TASK_START_MAKE_ARCHIVE", &[&guid]);
    inner.log(INFO, &message);
    let started = std::time::Instant::now();
    // The blocking writer only builds a private container. It is never
    // aborted or detached: shutdown waits for this JoinHandle, so
    // cancellation cannot pretend the archive ended early.
    let handle = tokio::task::spawn_blocking(move || prepare_archive_snapshot(&snapshot, true));
    let prepared = match handle.await {
        Ok(Ok(prepared)) => prepared,
        Ok(Err(error)) => {
            archive_failed(&inner, &guid, token, &error.to_string());
            return;
        }
        Err(error) => {
            archive_failed(&inner, &guid, token, &format!("join error: {error}"));
            return;
        }
    };
    match inner.commit_archive(
        &guid,
        token,
        prepared,
        Some(consts::TASK_STATE_MAKE_ARCHIVE),
    ) {
        Ok(ArchiveCommit::Committed(committed)) => {
            // Test-only barrier between publication and detached-tree
            // cleanup; unarmed runs pass straight through.
            #[cfg(test)]
            {
                let marker = committed.path.to_string_lossy().into_owned();
                crate::task::archive_cleanup_gate(&marker).await;
            }
            if let Some(detached) = committed.detached {
                // Heavy tree cleanup owns only the detached private path and
                // is awaited, so shutdown/pause join its real completion.
                let removal = tokio::task::spawn_blocking(move || remove_detached(&detached)).await;
                match removal {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        archive_failed(&inner, &guid, token, &error.to_string());
                        return;
                    }
                    Err(error) => {
                        archive_failed(&inner, &guid, token, &format!("join error: {error}"));
                        return;
                    }
                }
            }
            if inner.finish_archive(&guid, token) {
                let message = localized(
                    "TASK_MAKE_ARCHIVE_FINISHED",
                    &[
                        &guid,
                        &committed.path.display().to_string(),
                        &started.elapsed().as_secs_f64().to_string(),
                    ],
                );
                inner.log(INFO, &message);
                inner.record(
                    "archive.done",
                    json!({"task": guid, "path": committed.path.display().to_string()}),
                );
            } else {
                inner.record(
                    "archive.stale",
                    json!({"task": guid, "stage": "completion"}),
                );
            }
        }
        Ok(ArchiveCommit::Stale(prepared)) => {
            // The run was invalidated after the private container was built:
            // only its owner may remove that stage, and nothing public is
            // touched. A real unlink error is recorded, never claimed as
            // successful removal.
            if let Err(error) = discard_prepared(prepared) {
                inner.record(
                    "archive.discard_failed",
                    json!({"task": guid, "error": error.to_string()}),
                );
            }
            inner.record(
                "archive.stale",
                json!({"task": guid, "stage": "publication"}),
            );
        }
        Err(error) => {
            archive_failed(&inner, &guid, token, &error.to_string());
        }
    }
}

/// Fence one archive failure publication by the captured owner/run. A stale
/// writer error (or join error) never mutates the replacement run.
fn archive_failed(inner: &RuntimeInner, guid: &str, token: (u64, u64), error: &str) {
    if !inner.fail_archive(guid, token) {
        inner.record("archive.stale", json!({"task": guid, "stage": "failure"}));
        return;
    }
    let message = format!(
        "task #{guid} error: {}",
        localized("ERR_CANNOT_MAKE_ARCHIVE", &[])
    );
    inner.log(ERROR, &format!("{message}: {error}"));
    inner.record("archive.failed", json!({"task": guid, "error": error}));
}
