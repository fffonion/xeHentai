//! The installer-owned update area and candidate application.
//!
//! Everything here is a native installation primitive: the RSS business
//! modules decide *whether* a pending candidate should be applied, while this
//! module enforces the safety that must hold even if a script asked for
//! something unsafe.
//!
//! Layout under the installation root (the directory the release ships its
//! executable and `rss/` in):
//!
//! ```text
//! .xehentai-update/                    private (0700), symlink-free
//!   lock                               per-install cross-process write lock
//!   pending/src.zip                    one atomically published envelope
//!   staging/<random>/                  complete candidate tree, private
//!   preflight/                         private state for the candidate run
//!   state.json                         the single applied identity (atomic)
//! ```
//!
//! There is no rollback state. Download only publishes the pending envelope;
//! the next start materializes the COMPLETE candidate into staging, runs the
//! real private preflight and only then replaces the selected live files. A
//! candidate that fails any preparation step is discarded and the running
//! release keeps running unchanged. The applied identity is published only
//! after the replacement really succeeded.
//! `h.json`, `config.rss`, `eh.log` and the legacy cookie file are never
//! package entries and never touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::descriptor::Descriptor;
use super::error::UpdateError;
use super::package::{self, SelectedEntry};

pub const AREA_DIR: &str = ".xehentai-update";
pub const STATE_FILE: &str = "state.json";
pub const LOCK_FILE: &str = "lock";
pub const PENDING_DIR: &str = "pending";
pub const STAGING_DIR: &str = "staging";
pub const PREFLIGHT_DIR: &str = "preflight";
pub const STATE_VERSION: i64 = 1;
/// The candidate preflight is a real process; this bound only stops a wedged
/// child, it is not a policy timeout.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(180);

/// The one applied release identity. It is also the only source of the
/// same-update-id decision; it is never inferred from files.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub update_id: String,
    pub build_id: String,
    pub native: bool,
    pub applied_at: String,
}

/// The installer state: exactly one applied identity, no history.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    pub v: i64,
    pub active: Option<Record>,
}

impl Default for Records {
    fn default() -> Self {
        Self {
            v: STATE_VERSION,
            active: None,
        }
    }
}

/// The installation the running executable and its resources live in.
#[derive(Clone, Debug)]
pub struct Installation {
    root: PathBuf,
}

impl Installation {
    pub fn new(root: &Path) -> Self {
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn area(&self) -> PathBuf {
        self.root.join(AREA_DIR)
    }

    pub fn pending_path(&self) -> PathBuf {
        self.area().join(PENDING_DIR).join(package::PENDING_FILE)
    }

    fn state_path(&self) -> PathBuf {
        self.area().join(STATE_FILE)
    }

    fn lock_path(&self) -> PathBuf {
        self.area().join(LOCK_FILE)
    }

    fn staging_root(&self) -> PathBuf {
        self.area().join(STAGING_DIR)
    }

    fn preflight_root(&self) -> PathBuf {
        self.area().join(PREFLIGHT_DIR)
    }

    /// Verify an EXISTING update area before it is trusted for a read or a
    /// write. A missing area is not an error here.
    pub fn verify_private_area(&self) -> Result<(), UpdateError> {
        match std::fs::symlink_metadata(self.area()) {
            Ok(metadata) => verify_private_dir_metadata(self.area().as_path(), &metadata),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(UpdateError::io(self.area(), &error)),
        }
    }

    fn pending_dir(&self) -> PathBuf {
        self.area().join(PENDING_DIR)
    }

    /// Do not trust an EXISTING `pending/` subdirectory for a probe, read or
    /// delete before checking that it is a real installer-owned private
    /// directory. `Ok(false)` means the subdirectory does not exist, which is
    /// simply "no candidate": this NEVER creates it.
    pub fn verify_pending_dir(&self) -> Result<bool, UpdateError> {
        self.verify_private_area()?;
        let path = self.pending_dir();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(UpdateError::install(format!(
                        "{} is a symlink; the pending update directory must be a real installer-owned directory",
                        path.display()
                    )));
                }
                if !metadata.is_dir() {
                    return Err(UpdateError::install(format!(
                        "{} exists and is not a directory",
                        path.display()
                    )));
                }
                private_dir_verdict_metadata(&path, &metadata)?;
                Ok(true)
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(UpdateError::io(&path, &error)),
        }
    }

    /// Probe the pending envelope WITHOUT reading it, after the parent
    /// directory has been validated. A missing leaf is "no candidate".
    pub fn pending_present(&self) -> Result<bool, UpdateError> {
        if !self.verify_pending_dir()? {
            return Ok(false);
        }
        let path = self.pending_path();
        match std::fs::symlink_metadata(&path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(UpdateError::io(&path, &error)),
        }
    }

    /// Read the pending envelope through the validated private directory and a
    /// leaf that is a real, installer-owned regular file opened without
    /// following a symlink. Nothing outside the installer area can be read.
    pub fn read_pending(&self) -> Result<Option<package::PendingEnvelope>, UpdateError> {
        if !self.verify_pending_dir()? {
            return Ok(None);
        }
        let path = self.pending_path();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => verify_pending_leaf(&path, &metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(UpdateError::io(&path, &error)),
        }
        let bytes = read_private_leaf(&path)?;
        Ok(Some(package::read_bytes(&bytes)?))
    }

    pub fn records(&self) -> Result<Records, UpdateError> {
        self.verify_private_area()?;
        let path = self.state_path();
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Records::default());
            }
            Err(error) => return Err(UpdateError::io(&path, &error)),
        };
        let records: Records = serde_json::from_slice(&bytes)
            .map_err(|error| UpdateError::install(format!("state record: {error}")))?;
        if records.v != STATE_VERSION {
            return Err(UpdateError::install(format!(
                "state record version {} is not {STATE_VERSION}",
                records.v
            )));
        }
        Ok(records)
    }

    /// The update id the running build was applied from, if any.
    pub fn applied_update_id(&self) -> Option<String> {
        self.records()
            .ok()
            .and_then(|records| records.active)
            .map(|record| record.update_id)
    }

    fn write_records(&self, records: &Records) -> Result<(), UpdateError> {
        let area = self.area();
        ensure_private_dir(&area)?;
        let bytes = serde_json::to_vec(records)
            .map_err(|error| UpdateError::install(format!("state record: {error}")))?;
        write_atomic(&area, &self.state_path(), &bytes)
    }

    /// The pending envelope has been consumed (applied or rejected); removing
    /// it can never resurrect a partially written candidate.
    ///
    /// The parent directory is validated first (a symlinked `pending/` would
    /// redirect the unlink outside the installer area) and a symlink or
    /// non-regular leaf is rejected instead of deleted.
    pub fn discard_pending(&self) -> Result<(), UpdateError> {
        if !self.verify_pending_dir()? {
            return Ok(());
        }
        let path = self.pending_path();
        match std::fs::symlink_metadata(&path) {
            Ok(metadata) => verify_pending_leaf(&path, &metadata)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(UpdateError::io(&path, &error)),
        }
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(UpdateError::io(&path, &error)),
        }
    }
}

/// One existing private installer directory. Used both by the create path and
/// by read/delete probes, so a probe can never create or `chmod` anything.
fn verify_private_dir_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), UpdateError> {
    if metadata.file_type().is_symlink() {
        return Err(UpdateError::install(format!(
            "{} is a symlink; the update area must be a real directory",
            path.display()
        )));
    }
    if !metadata.is_dir() {
        return Err(UpdateError::install(format!(
            "{} exists and is not a directory",
            path.display()
        )));
    }
    private_dir_verdict_metadata(path, metadata)
}

fn private_dir_verdict_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
) -> Result<(), UpdateError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let euid = nix::unistd::geteuid().as_raw();
        private_dir_verdict(metadata.uid(), metadata.mode() & 0o7777, euid)
            .map_err(|problem| UpdateError::install(format!("{}: {problem}", path.display())))?;
    }
    let _ = (path, metadata);
    Ok(())
}

/// A pending leaf must be a real installer-owned regular file. A symlink could
/// point anywhere outside the private area, so it is rejected BEFORE the read
/// or delete rather than resolved.
fn verify_pending_leaf(path: &Path, metadata: &std::fs::Metadata) -> Result<(), UpdateError> {
    verify_owned_regular_leaf(path, metadata, "the pending update envelope")
}

fn verify_owned_regular_leaf(
    path: &Path,
    metadata: &std::fs::Metadata,
    what: &str,
) -> Result<(), UpdateError> {
    if metadata.file_type().is_symlink() {
        return Err(UpdateError::install(format!(
            "{} is a symlink; {what} must be a regular installer-owned file",
            path.display()
        )));
    }
    if !metadata.is_file() {
        return Err(UpdateError::install(format!(
            "{} is not a regular file",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let euid = nix::unistd::geteuid().as_raw();
        if metadata.uid() != euid {
            return Err(UpdateError::install(format!(
                "{} is owned by uid {}, not the effective uid {euid}",
                path.display(),
                metadata.uid()
            )));
        }
    }
    Ok(())
}

/// Read the validated leaf without following a final symlink. On Unix the open
/// itself refuses a symlink (`O_NOFOLLOW`), so a swap between the metadata
/// check and the read cannot escape the private area.
fn read_private_leaf(path: &Path) -> Result<Vec<u8>, UpdateError> {
    use std::io::Read;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut file = options
        .open(path)
        .map_err(|error| UpdateError::io(path, &error))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| UpdateError::io(path, &error))?;
    Ok(bytes)
}

/// One held cross-process write lock for this installation.
///
/// The lock lives on an ordinary file in the private update area and covers
/// the final pending publication and the startup application. It is a real
/// filesystem lock, never an in-process mutex: two xeHentai processes in one
/// installation serialize here.
pub struct InstallLock {
    file: std::fs::File,
}

impl InstallLock {
    /// Take the write lock without waiting. A concurrent owner yields a
    /// bounded explicit failure; no shared file is touched.
    pub fn acquire(installation: &Installation) -> Result<Self, UpdateError> {
        ensure_private_dir(&installation.area())?;
        let path = installation.lock_path();
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .map_err(|error| UpdateError::io(&path, &error))?;
        // `flock` on Unix, not a POSIX record lock: the lock must belong to the
        // open file DESCRIPTION so it is shared by a daemon child after fork.
        // Closing the parent's descriptor must not release the child's lock (a
        // POSIX F_SETLK lock would be dropped by any close in the owning
        // process, and is not inherited across fork).
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            match nix::fcntl::flock(
                file.as_raw_fd(),
                nix::fcntl::FlockArg::LockExclusiveNonblock,
            ) {
                Ok(()) => Ok(Self { file }),
                Err(nix::errno::Errno::EWOULDBLOCK) => Err(UpdateError::Busy(
                    "another xeHentai process is updating this installation; the pending candidate was left unchanged".to_string(),
                )),
                Err(error) => Err(UpdateError::install(format!(
                    "{}: cannot take the update lock: {error}",
                    path.display()
                ))),
            }
        }
        #[cfg(not(unix))]
        {
            match file.try_lock() {
                Ok(()) => Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock) => Err(UpdateError::Busy(
                    "another xeHentai process is updating this installation; the pending candidate was left unchanged".to_string(),
                )),
                Err(std::fs::TryLockError::Error(error)) => Err(UpdateError::io(&path, &error)),
            }
        }
    }
}

// Closing the only remaining owned descriptor releases the lock. There is
// deliberately no explicit unlock: a daemon parent that returns must not
// release the lock its child still holds through the shared open description.
impl Drop for InstallLock {
    fn drop(&mut self) {
        let _ = &self.file;
    }
}

/// Verdict for an EXISTING private installer directory.
///
/// Pure so the owner/mode predicate can be proven without chown: the effective
/// uid must own the directory and group/other bits must be absent. An
/// owner-only read-only mode is allowed and simply fails later writes.
pub(crate) fn private_dir_verdict(uid: u32, mode: u32, euid: u32) -> Result<(), String> {
    if uid != euid {
        return Err(format!("owned by uid {uid}, not the effective uid {euid}"));
    }
    if mode & 0o077 != 0 {
        return Err(format!(
            "mode {mode:04o} is accessible to group/others; a private installer directory must be 0700 or stricter"
        ));
    }
    if mode & 0o700 == 0 {
        return Err(format!(
            "mode {mode:04o} gives the owner no access to the installer directory"
        ));
    }
    Ok(())
}

/// Create (and verify) a private installer-owned directory.
///
/// The path is created if missing, then checked with `symlink_metadata`: the
/// object must be a real directory, not a symlink, so a swapped component can
/// never redirect installer writes. A directory this call creates is 0700. An
/// EXISTING directory is never chmodded: it is trusted only when the effective
/// uid owns it and no group/other permission bit is set. Anything else is a
/// hard rejection, because a shared-writable area could redirect a live
/// replacement.
pub fn ensure_private_dir(path: &Path) -> Result<(), UpdateError> {
    // Walk to the nearest existing ancestor, collecting every missing
    // component. Only components THIS call creates are tightened; a
    // pre-existing parent (the ordinary 0755 installation root) keeps its mode.
    let mut missing: Vec<PathBuf> = Vec::new();
    let mut current = path.to_path_buf();
    loop {
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(UpdateError::install(format!(
                    "{} is a symlink; the update area must be a real directory",
                    current.display()
                )));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(UpdateError::install(format!(
                    "{} exists and is not a directory",
                    current.display()
                )));
            }
            Ok(metadata) => {
                if missing.is_empty() {
                    // The target itself exists: it must be private.
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::MetadataExt;
                        let euid = nix::unistd::geteuid().as_raw();
                        private_dir_verdict(metadata.uid(), metadata.mode() & 0o7777, euid)
                            .map_err(|problem| {
                                UpdateError::install(format!("{}: {problem}", path.display()))
                            })?;
                    }
                    let _ = metadata;
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                missing.push(current.clone());
                match current.parent() {
                    Some(parent) if parent != current => current = parent.to_path_buf(),
                    _ => {
                        return Err(UpdateError::install(format!(
                            "{} has no existing ancestor directory",
                            path.display()
                        )));
                    }
                }
            }
            Err(error) => return Err(UpdateError::io(&current, &error)),
        }
    }
    for dir in missing.iter().rev() {
        std::fs::create_dir(dir).map_err(|error| UpdateError::io(dir, &error))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .map_err(|error| UpdateError::io(dir, &error))?;
        }
    }
    Ok(())
}

fn write_atomic(dir: &Path, target: &Path, bytes: &[u8]) -> Result<(), UpdateError> {
    use std::io::Write;
    let mut temp =
        tempfile::NamedTempFile::new_in(dir).map_err(|error| UpdateError::io(dir, &error))?;
    temp.write_all(bytes)
        .map_err(|error| UpdateError::io(temp.path(), &error))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| UpdateError::io(temp.path(), &error))?;
    match temp.persist(target) {
        Ok(_) => Ok(()),
        Err(error) => Err(UpdateError::io(target, &error.error)),
    }
}

// ---------------------------------------------------------------------------
// Owned publication job + cancellation/commit linearization
// ---------------------------------------------------------------------------

#[derive(Default)]
struct GateState {
    cancelled: bool,
    committed: bool,
    finished: bool,
    result: Option<Result<(), String>>,
}

struct PublisherInner {
    gate: Mutex<GateState>,
    job: Mutex<Option<tokio::task::JoinHandle<()>>>,
    done: tokio::sync::Notify,
}

impl Default for PublisherInner {
    fn default() -> Self {
        Self {
            gate: Mutex::new(GateState::default()),
            job: Mutex::new(None),
            done: tokio::sync::Notify::new(),
        }
    }
}

/// The ONE owned blocking stage/publish job of the running updater task.
///
/// The blocking closure is owned by this value, not by the local future: an
/// abort of the awaiting local task drops only the cancel guard, which sets
/// `cancelled`; the job still runs to its commit boundary, sees the
/// cancellation, drops its staged temp file and never replaces the published
/// pending. Conversely a commit that already linearized before the
/// cancellation is a completed operation. `retire()` joins the real job so the
/// shutdown drain never claims retirement while it is still writing.
#[derive(Clone)]
pub struct Publisher {
    inner: Arc<PublisherInner>,
}

impl Default for Publisher {
    fn default() -> Self {
        Self::new()
    }
}

impl Publisher {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(PublisherInner::default()),
        }
    }

    /// Run one owned blocking publication job.
    ///
    /// `stage` performs every heavy step and returns a written, synced envelope
    /// temp file. Only its final commit is linearized against cancellation.
    pub async fn publish<F>(
        &self,
        #[cfg_attr(not(test), allow(unused_variables))] pending_dir: PathBuf,
        stage: F,
    ) -> Result<(), UpdateError>
    where
        F: FnOnce() -> Result<package::Staged, UpdateError> + Send + 'static,
    {
        {
            let mut job = self
                .inner
                .job
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if job.as_ref().is_some_and(|handle| !handle.is_finished()) {
                return Err(UpdateError::install(
                    "another update publication is already in flight",
                ));
            }
            job.take();
        }
        {
            let mut gate = self
                .inner
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *gate = GateState::default();
        }
        let inner = Arc::clone(&self.inner);
        let handle = tokio::task::spawn_blocking(move || {
            let staged = stage();
            // Deterministic test barrier: AFTER staging, BEFORE the commit
            // gate. Production never installs a hook.
            #[cfg(test)]
            test_hooks::publish_boundary(&pending_dir);
            let result = match staged {
                Err(error) => Err(error),
                Ok(staged) => {
                    let mut gate = inner.gate.lock().unwrap_or_else(|error| error.into_inner());
                    if gate.cancelled {
                        Err(UpdateError::install(
                            "the update publication was cancelled; the previous pending is unchanged",
                        ))
                    } else {
                        match staged.commit() {
                            Ok(()) => {
                                gate.committed = true;
                                Ok(())
                            }
                            Err(error) => Err(error),
                        }
                    }
                }
            };
            let outcome = result.map_err(|error| error.to_string());
            {
                let mut gate = inner.gate.lock().unwrap_or_else(|error| error.into_inner());
                gate.finished = true;
                gate.result = Some(outcome);
            }
            inner.done.notify_waiters();
        });
        {
            let mut job = self
                .inner
                .job
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *job = Some(handle);
        }
        let guard = CancelOnDrop {
            inner: Arc::clone(&self.inner),
            armed: true,
        };
        loop {
            let notified = self.inner.done.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self
                .inner
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .finished
            {
                break;
            }
            notified.await;
        }
        guard.disarm();
        let outcome = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .result
            .take()
            .ok_or_else(|| UpdateError::install("the update publication job produced no result"))?;
        outcome.map_err(UpdateError::install)
    }

    /// Cancel (if not already committed) and JOIN the owned blocking job.
    pub async fn retire(&self) {
        {
            let mut gate = self
                .inner
                .gate
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if !gate.finished {
                gate.cancelled = true;
            }
        }
        let handle = self
            .inner
            .job
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }

    #[allow(dead_code)]
    fn cancel(&self) {
        let mut gate = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !gate.finished {
            gate.cancelled = true;
        }
    }
}

struct CancelOnDrop {
    inner: Arc<PublisherInner>,
    armed: bool,
}

impl CancelOnDrop {
    fn disarm(mut self) {
        self.armed = false;
    }

    fn inner_cancel(&self) {
        let mut gate = self
            .inner
            .gate
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !gate.finished {
            gate.cancelled = true;
        }
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.inner_cancel();
        }
    }
}

/// Test-only hooks for the deterministic pre-commit and join barriers.
#[cfg(test)]
pub mod test_hooks {
    #![allow(dead_code)]
    use std::path::Path;
    use std::sync::{Mutex, OnceLock};

    pub type Hook = Box<dyn Fn(&Path) + Send + Sync>;

    static HOOK: OnceLock<Mutex<Option<Hook>>> = OnceLock::new();
    pub type JoinHook = Box<dyn Fn() + Send + Sync>;
    static JOIN_HOOK: OnceLock<Mutex<Option<JoinHook>>> = OnceLock::new();

    fn cell() -> &'static Mutex<Option<Hook>> {
        HOOK.get_or_init(|| Mutex::new(None))
    }

    pub fn set(hook: Option<Hook>) {
        *cell().lock().unwrap_or_else(|error| error.into_inner()) = hook;
    }

    fn join_cell() -> &'static Mutex<Option<JoinHook>> {
        JOIN_HOOK.get_or_init(|| Mutex::new(None))
    }

    pub fn set_join(hook: Option<JoinHook>) {
        *join_cell()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = hook;
    }

    /// Test-only barrier at the START of the supervisor's publication join,
    /// i.e. after the local updater task was aborted and before the owned
    /// blocking job is released.
    pub fn join_boundary() {
        let guard = join_cell()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(hook) = guard.as_ref() {
            hook();
        }
    }

    pub fn publish_boundary(path: &Path) {
        let guard = cell().lock().unwrap_or_else(|error| error.into_inner());
        if let Some(hook) = guard.as_ref() {
            hook(path);
        }
    }
}

/// Facts a validated pending candidate provides to the startup decision.
pub struct Candidate {
    pub descriptor: Descriptor,
    pub payload: Vec<u8>,
    pub prefix: String,
}

/// One installed release: whether the caller must re-execute the installed
/// image. The applied record was already published by the installer.
pub struct Committed {
    pub reexec: bool,
}

/// Prepare and then apply one validated candidate.
///
/// Everything that can reject the candidate — complete materialization into a
/// private staging tree and the real private preflight — happens BEFORE any
/// live file is touched, so a failure there leaves the running release
/// completely unchanged and is an ordinary `Install` rejection. Only a fully
/// prepared candidate replaces the selected files; the applied record is
/// written only after that replacement succeeded. Any failure from the first
/// live mutation on is tagged as an `Apply` failure (the tree may be mixed),
/// which the startup controller must stop on rather than continue past.
pub fn install_candidate(
    installation: &Installation,
    candidate: &Candidate,
) -> Result<Committed, UpdateError> {
    let area = installation.area();
    ensure_private_dir(&area)?;
    ensure_private_dir(&installation.staging_root())?;
    ensure_private_dir(&installation.preflight_root())?;

    let staging = tempfile::Builder::new()
        .prefix("candidate-")
        .tempdir_in(installation.staging_root())
        .map_err(|error| UpdateError::io(&installation.staging_root(), &error))?;
    let selected = package::materialize(
        &candidate.payload,
        &candidate.descriptor,
        &candidate.prefix,
        staging.path(),
    )?;

    let exe_path = std::env::current_exe().map_err(|error| {
        UpdateError::install(format!(
            "the running executable path is unavailable: {error}"
        ))
    })?;
    let native = candidate.descriptor.native_executable.clone();
    let preflight_exe = match &native {
        Some(entry) => {
            let staged = staging.path().join(entry);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
                    .map_err(|error| UpdateError::io(&staged, &error))?;
            }
            staged
        }
        None => exe_path.clone(),
    };

    #[cfg(not(unix))]
    if native.is_some() {
        // Honest platform boundary: native self replacement plus re-execution
        // is only verified on Unix. A resource-only candidate stays available.
        return Err(UpdateError::install(
            "native executable replacement is not verified on this platform; the candidate was rejected",
        ));
    }

    preflight(
        &preflight_exe,
        staging.path(),
        &installation.preflight_root(),
    )?;

    // The candidate is complete and preflighted; only now are live files
    // replaced. This is the apply phase: from the first replacement on, a
    // failure means the live tree may already be mixed, so it must stop this
    // start instead of being reported as a candidate rejection. There is no
    // undo, journal or recovery record.
    let resources: Vec<&SelectedEntry> = selected
        .iter()
        .filter(|entry| Some(&entry.path) != native.as_ref())
        .collect();
    let apply = || -> Result<(), UpdateError> {
        for entry in &resources {
            let staged = staging.path().join(&entry.path);
            let live = installation.root().join(&entry.path);
            replace_file(&live, &staged)?;
        }
        if let Some(entry) = &native {
            // The running image keeps its inode; the path now refers to the
            // candidate bytes. Re-execution below loads the real new image.
            let staged = staging.path().join(entry);
            self_replace::self_replace(&staged)
                .map_err(|error| UpdateError::install(format!("self replacement: {error}")))?;
        }

        let build_id = candidate
            .descriptor
            .native_build_id
            .clone()
            .unwrap_or_else(|| crate::r#const::BUILD_ID.to_string());
        let record = Record {
            update_id: candidate.descriptor.update_id.clone(),
            build_id,
            native: native.is_some(),
            applied_at: crate::util::logger::now_gmt8().to_rfc3339(),
        };
        installation.write_records(&Records {
            v: STATE_VERSION,
            active: Some(record),
        })?;
        Ok(())
    };
    apply().map_err(UpdateError::during_apply)?;

    Ok(Committed {
        reexec: native.is_some(),
    })
}

/// Replace one live file with the prepared staged copy via the same
/// filesystem. On Unix `rename` replaces the destination atomically; other
/// platforms remove the destination first because their rename refuses an
/// existing target.
fn replace_file(live: &Path, staged: &Path) -> Result<(), UpdateError> {
    if let Some(parent) = live.parent() {
        std::fs::create_dir_all(parent).map_err(|error| UpdateError::io(parent, &error))?;
    }
    #[cfg(unix)]
    {
        std::fs::rename(staged, live).map_err(|error| UpdateError::io(live, &error))
    }
    #[cfg(not(unix))]
    {
        match std::fs::remove_file(live) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(UpdateError::io(live, &error)),
        }
        std::fs::rename(staged, live).map_err(|error| UpdateError::io(live, &error))
    }
}

/// Run the candidate through the same host's minimal private update mode: the
/// real executable, real configuration and module loading, private state, no
/// network and no user configuration or session touched.
fn preflight(exe: &Path, resources: &Path, preflight_root: &Path) -> Result<(), UpdateError> {
    let state = preflight_root.join(format!("state-{}", std::process::id()));
    if state.exists() {
        ensure_private_dir(&state)?;
        std::fs::remove_dir_all(&state).map_err(|error| UpdateError::io(&state, &error))?;
    }
    ensure_private_dir(&state)?;
    for dir in ["home", "tmp", "cache"] {
        ensure_private_dir(&state.join(dir))?;
    }
    let mut child = Command::new(exe)
        .arg("--update-preflight")
        .arg(resources)
        .arg(&state)
        .current_dir(&state)
        // `env_clear` keeps the private preflight independent of the parent:
        // it never inherits the update area or a user configuration spelling.
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", state.join("home"))
        .env("TMPDIR", state.join("tmp"))
        .env("XDG_CACHE_HOME", state.join("cache"))
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            UpdateError::install(format!(
                "cannot run the candidate preflight ({}): {error}",
                exe.display()
            ))
        })?;
    let deadline = Instant::now() + PREFLIGHT_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child
                    .wait_with_output()
                    .map_err(|error| UpdateError::install(format!("preflight output: {error}")))?;
                let stdout = String::from_utf8_lossy(&output.stdout);
                if !status.success() {
                    let stderr = String::from_utf8_lossy(&output.stderr);
                    return Err(UpdateError::install(format!(
                        "candidate preflight exited {:?}: {}{}",
                        status.code(),
                        stdout.trim(),
                        stderr.trim()
                    )));
                }
                if !stdout.contains(PREFLIGHT_MARKER) {
                    return Err(UpdateError::install(format!(
                        "candidate preflight did not report {PREFLIGHT_MARKER}: {}",
                        stdout.trim()
                    )));
                }
                return Ok(());
            }
            Ok(None) => {
                if Instant::now() > deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(UpdateError::install(
                        "candidate preflight did not finish in time",
                    ));
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => {
                return Err(UpdateError::install(format!("preflight wait: {error}")));
            }
        }
    }
}

/// The marker the private preflight mode prints on success.
pub const PREFLIGHT_MARKER: &str = "xeHentai-preflight-ok";

/// Parse `--update-preflight <resources> <state>` from a real process argv.
/// This is an installer primitive, not a public switch: an ordinary command
/// line never reaches it.
pub fn preflight_request(argv: &[std::ffi::OsString]) -> Option<(PathBuf, PathBuf)> {
    if argv.len() != 4 || argv[1] != "--update-preflight" {
        return None;
    }
    Some((PathBuf::from(&argv[2]), PathBuf::from(&argv[3])))
}

/// Re-execute the installed image in place. Returns only if the platform has
/// no verified path, which the caller reports as a failed update.
pub fn reexec(exe: &Path) -> UpdateError {
    #[cfg(unix)]
    {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let mut args: Vec<CString> = Vec::new();
        let mut argv = std::env::args_os();
        argv.next();
        let exe_c = match CString::new(exe.as_os_str().as_bytes()) {
            Ok(value) => value,
            Err(_) => return UpdateError::install("executable path is not a valid argument"),
        };
        args.push(exe_c);
        for arg in argv {
            match CString::new(arg.as_os_str().as_bytes()) {
                Ok(value) => args.push(value),
                Err(_) => return UpdateError::install("argument is not a valid exec argument"),
            }
        }
        // The owned lock descriptor is close-on-exec, so the lock is released
        // when the replacement image really starts; there is no handoff state.
        let error = nix::unistd::execv(&args[0], &args).unwrap_err();
        UpdateError::install(format!("re-executing the installed update: {error}"))
    }
    #[cfg(not(unix))]
    {
        let _ = exe;
        UpdateError::install("re-execution is not verified on this platform")
    }
}
