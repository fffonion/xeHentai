//! Archive work of `xeHentai/task.py` (`make_archive`).
//!
//! The reference writes a STORE-only `ZIP_STORED` archive next to the gallery
//! directory with a UTF-8 comment and then removes the directory. This
//! implementation keeps that container contract and splits it into two owned
//! steps so a stale run can never publish late:
//!
//! 1. [`prepare_archive_snapshot`] only builds the container in a private
//!    staging file owned by this attempt. It never touches the public archive
//!    path and never removes the gallery, so it is safe to run on the blocking
//!    pool even after the run it was admitted for has been invalidated.
//! 2. [`commit_prepared`] performs the bounded namespace effects (move the
//!    gallery into a freshly reserved exclusively owned container, one
//!    rename over the archive path) and is
//!    only called by the runtime while it still owns the exact task run. The
//!    detached gallery is then removed from the blocking pool; cleanup can
//!    only ever touch that owned container, never a reused public gallery.
//!
//! The writer itself is a free function over an immutable [`ArchiveSnapshot`]
//! so the runtime can run it on Tokio's blocking pool without holding a
//! `Task`/`App` borrow; the task file lock is taken inside that thread.
#![allow(dead_code)]

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;

use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipWriter};

use super::file_ops;
use super::{Task, TaskError};

/// Everything the archive writer needs, captured while the task is borrowed.
#[derive(Clone)]
pub struct ArchiveSnapshot {
    pub fpath: PathBuf,
    pub arc: PathBuf,
    pub comment: String,
    pub lock: Arc<Mutex<()>>,
}

/// A finished container built in this attempt's own staging file. The stage
/// is consumed explicitly (renamed by [`commit_prepared`] or removed by
/// [`discard_prepared`]); the armed private cleanup guard stays inside this
/// value so an unobserved/abandoned result still removes exactly its own
/// unpublished file on drop.
pub struct PreparedStage {
    stage: PathBuf,
    arc: PathBuf,
    gallery: PathBuf,
    lock: Arc<Mutex<()>>,
    remove: bool,
    cleanup: Option<StageCleanup>,
}

/// What [`prepare_archive_snapshot`] produced. `Existing` mirrors the
/// reference early return for an archive that is already present.
pub enum PreparedArchive {
    Existing(PathBuf),
    Staged(PreparedStage),
}

/// Result of the run-authorized publication step.
pub struct CommittedArchive {
    pub path: PathBuf,
    /// The gallery directory, moved into an exclusively reserved private
    /// container. The caller removes it on the blocking pool; only this
    /// owned value addresses that container.
    pub detached: Option<DetachedGallery>,
}

/// An exclusively owned detached namespace: a freshly reserved private
/// container in the same filesystem plus the fixed child path holding the
/// moved gallery. The container is allocated with an atomic exclusive
/// `create_dir`, so it can never coincide with a public gallery, an
/// existing file or another reservation; the fixed child sits one level
/// below the download root, unreachable through ordinary single-component
/// public gallery admission. Only this value (created by reservation)
/// addresses the container through cleanup or rollback.
pub struct DetachedGallery {
    container: PathBuf,
    child: PathBuf,
    public: PathBuf,
}

impl DetachedGallery {
    /// Move the owned payload back to its public gallery path. Success
    /// removes the now-empty container; failure retains the recoverable
    /// images at the owned child and reports that location instead of
    /// claiming the public path survived.
    pub(crate) fn restore(self) -> Result<(), TaskError> {
        match fs::rename(&self.child, &self.public) {
            Ok(()) => {
                let _ = fs::remove_dir(&self.container);
                file_ops::untrack_detached_container(&self.container);
                Ok(())
            }
            Err(source) => Err(TaskError::RollbackFailed {
                owned: self.child.clone(),
                target: self.public.clone(),
                message: source.to_string(),
            }),
        }
    }
}

impl Task {
    /// Capture the immutable archive inputs. The title and URL are copied, so
    /// the writer never touches task state from the blocking pool.
    pub fn archive_snapshot(&self) -> Result<ArchiveSnapshot, TaskError> {
        let dpath = self.get_fpath()?;
        let arc = file_ops::zip_path(&dpath)?;
        let title = self.meta_str("title")?.to_string();
        let comment = format!(
            "xeHentai Archiver v{}\nTitle:{}\nOriginal URL:{}",
            crate::r#const::VERSION,
            title,
            self.url
        );
        Ok(ArchiveSnapshot {
            fpath: dpath,
            arc,
            comment,
            lock: self.files().lock_handle(),
        })
    }

    /// Reference `make_archive`. Delegates to the blocking-pool-safe writer.
    pub fn make_archive(&mut self, remove: bool) -> Result<PathBuf, TaskError> {
        let snapshot = self.archive_snapshot()?;
        make_archive_snapshot(&snapshot, remove)
    }
}

/// Synchronous entry point for the direct `Task::make_archive` caller: prepare
/// and commit in one call, exactly like the reference.
pub fn make_archive_snapshot(
    snapshot: &ArchiveSnapshot,
    remove: bool,
) -> Result<PathBuf, TaskError> {
    let committed = commit_prepared(prepare_archive_snapshot(snapshot, remove)?)?;
    if let Some(detached) = committed.detached {
        remove_detached(&detached)?;
    }
    Ok(committed.path)
}

/// Remove the exclusively owned detached container. Only a container
/// produced by the reservation below is ever addressed here: the blocking
/// pool walks exactly this private subtree, never a rebound public name.
/// Retirement unregisters the lifetime-bound namespace so the exact
/// ordinary name is usable again; a failed cleanup keeps the registration.
pub fn remove_detached(detached: &DetachedGallery) -> Result<(), TaskError> {
    let result = file_ops::remove_tree(&detached.container);
    if result.is_ok() {
        file_ops::untrack_detached_container(&detached.container);
    }
    result
}

/// Build `<gallery>.zip` from `snapshot.fpath` in a private staging file.
/// Nothing public is created or removed here: an invalidated run can only
/// discard its own stage.
pub fn prepare_archive_snapshot(
    snapshot: &ArchiveSnapshot,
    remove: bool,
) -> Result<PreparedArchive, TaskError> {
    #[cfg(test)]
    let marker = snapshot.fpath.to_string_lossy().into_owned();
    #[cfg(test)]
    let _writer = WriterProbe::enter(&marker);
    let _guard = snapshot
        .lock
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let dpath = &snapshot.fpath;
    let arc = &snapshot.arc;
    if file_ops::path_exists(arc)? {
        // Reference returns without releasing `_f_lock`; the guard is dropped
        // here so the lock stays usable (registered delta).
        return Ok(PreparedArchive::Existing(arc.clone()));
    }
    let parent = dpath.parent().ok_or_else(|| TaskError::UnsafePath {
        path: dpath.clone(),
        reason: "gallery directory has no parent",
    })?;
    let stage = file_ops::create_stage_with_prefix(parent, ".xehentai-archive-")?;
    // Owns the private stage until the container is complete. Any error or
    // panic in this thread removes exactly this attempt's file.
    let cleanup = StageCleanup {
        path: stage.clone(),
        armed: true,
    };

    let file = File::create(&stage).map_err(|source| TaskError::Io {
        path: stage.clone(),
        source,
    })?;
    let mut writer = ZipWriter::new(file);
    writer.set_comment(snapshot.comment.as_str());
    let mut entries: Vec<std::ffi::OsString> = fs::read_dir(dpath)
        .map_err(|source| TaskError::Io {
            path: dpath.clone(),
            source,
        })?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name())
                .map_err(|source| TaskError::Io {
                    path: dpath.clone(),
                    source,
                })
        })
        .collect::<Result<_, _>>()?;
    entries.sort_by(|left, right| left.as_encoded_bytes().cmp(right.as_encoded_bytes()));
    let options = SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    for name in entries {
        let full = dpath.join(&name);
        let metadata = fs::symlink_metadata(&full).map_err(|source| TaskError::Io {
            path: full.clone(),
            source,
        })?;
        if metadata.file_type().is_symlink() {
            return Err(TaskError::UnsafePath {
                path: full,
                reason: "refusing to archive a symlink entry",
            });
        }
        let name = name.to_str().ok_or_else(|| TaskError::UnsafePath {
            path: full.clone(),
            reason: "archive entry name is not UTF-8",
        })?;
        if metadata.is_dir() {
            writer
                .add_directory(format!("{name}/"), options)
                .map_err(|error| TaskError::Zip(error.to_string()))?;
        } else {
            writer
                .start_file(name, options)
                .map_err(|error| TaskError::Zip(error.to_string()))?;
            let mut source = File::open(&full).map_err(|source| TaskError::Io {
                path: full.clone(),
                source,
            })?;
            std::io::copy(&mut source, &mut writer)
                .map_err(|error| TaskError::Zip(error.to_string()))?;
        }
    }
    let file = writer
        .finish()
        .map_err(|error| TaskError::Zip(error.to_string()))?;
    file.sync_all().map_err(|source| TaskError::Io {
        path: stage.clone(),
        source,
    })?;
    #[cfg(test)]
    {
        seam_gate("writer", &marker);
        match seam_fault(&marker) {
            Some(ArchiveFault::Prepare) => {
                return Err(TaskError::Zip("injected archive prepare failure".into()));
            }
            Some(ArchiveFault::Panic) => panic!("injected archive writer panic"),
            None => {}
        }
    }
    // Transfer the armed guard into the value: commit/discard disarm it at
    // the completed ownership boundary; an unobserved drop still unlinks
    // exactly this attempt's private stage.
    Ok(PreparedArchive::Staged(PreparedStage {
        stage,
        arc: arc.clone(),
        gallery: dpath.clone(),
        lock: Arc::clone(&snapshot.lock),
        remove,
        cleanup: Some(cleanup),
    }))
}

/// Publish a prepared container. The caller must have re-validated the exact
/// task owner/run synchronously before entering this function; every step is
/// a bounded namespace operation.
pub fn commit_prepared(prepared: PreparedArchive) -> Result<CommittedArchive, TaskError> {
    let mut stage = match prepared {
        PreparedArchive::Existing(path) => {
            return Ok(CommittedArchive {
                path,
                detached: None,
            });
        }
        PreparedArchive::Staged(stage) => stage,
    };
    let _guard = stage.lock.lock().unwrap_or_else(|error| error.into_inner());
    let mut cleanup = stage.cleanup.take();
    let detached = if stage.remove {
        match detach_gallery(&stage.gallery) {
            Ok(detached) => detached,
            Err(error) => {
                // Nothing was published: this attempt removes its own stage.
                remove_stage_file(&stage.stage, &mut cleanup);
                return Err(error);
            }
        }
    } else {
        None
    };
    match file_ops::publish(&stage.stage, &stage.arc) {
        Ok(()) => {
            // Publication completed: the stage file no longer exists under
            // this name, so its cleanup guard is disarmed, never fired.
            if let Some(mut guard) = cleanup.take() {
                guard.disarm();
            }
            Ok(CommittedArchive {
                path: stage.arc,
                detached,
            })
        }
        Err(error) => {
            // The bound run keeps its images: put the detached gallery back
            // and remove only this attempt's staging file. If the restore
            // itself fails, the images stay retained at the owned path and
            // the error reports that location; nothing is dropped.
            if let Some(owned) = detached {
                match owned.restore() {
                    Ok(()) => {
                        remove_stage_file(&stage.stage, &mut cleanup);
                        Err(error)
                    }
                    Err(restore) => {
                        remove_stage_file(&stage.stage, &mut cleanup);
                        Err(restore)
                    }
                }
            } else {
                remove_stage_file(&stage.stage, &mut cleanup);
                Err(error)
            }
        }
    }
}

/// Remove this attempt's private stage once. A guard (when present) is
/// disarmed after the attempt so drop never repeats the unlink.
fn remove_stage_file(path: &Path, cleanup: &mut Option<StageCleanup>) {
    match cleanup {
        Some(guard) => {
            let _ = guard.remove();
        }
        None => {
            let _ = fs::remove_file(path);
        }
    }
}

/// Remove the private staging file of a container that was never published.
/// A single unlink is bounded; the ZIP bytes are never copied here. The real
/// filesystem outcome is returned so a caller never claims removal happened
/// while the private file remains.
pub fn discard_prepared(prepared: PreparedArchive) -> std::io::Result<()> {
    match prepared {
        PreparedArchive::Existing(_) => Ok(()),
        PreparedArchive::Staged(mut stage) => match stage.cleanup.take() {
            Some(mut guard) => guard.remove(),
            None => fs::remove_file(&stage.stage),
        },
    }
}

/// Atomically reserve a fresh private container in the same filesystem as
/// the gallery. The exclusive `create_dir` is the allocation: an existing
/// gallery, file, symlink or previously reserved container at the candidate
/// fails reservation and retries with the next name, never overwrites and
/// never accepts a self-rename. A test-armed candidate goes through the
/// same exclusive step, so collisions reproduce exactly.
fn reserve_detach_container(parent: &Path) -> Result<PathBuf, TaskError> {
    #[cfg(test)]
    if let Some(name) = take_detach_candidate(parent) {
        let candidate = parent.join(name);
        if fs::create_dir(&candidate).is_ok() {
            return Ok(candidate);
        }
    }
    for _ in 0..1024 {
        let counter = DETACH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".xehentai-detached-{}-{counter}",
            std::process::id()
        ));
        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(TaskError::Io {
                    path: candidate,
                    source,
                });
            }
        }
    }
    Err(TaskError::Io {
        path: parent.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no free detached container name",
        ),
    })
}

/// Move the gallery directory into a freshly reserved owned container as its
/// fixed child. Renaming is the only removal step allowed under the task
/// borrow; walking the tree happens later on the blocking pool and only ever
/// sees this owned container, never a public gallery path.
pub(crate) fn detach_gallery(gallery: &Path) -> Result<Option<DetachedGallery>, TaskError> {
    match fs::symlink_metadata(gallery) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(TaskError::UnsafePath {
                path: gallery.to_path_buf(),
                reason: "refusing to delete a symlinked directory",
            });
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(TaskError::UnsafePath {
                path: gallery.to_path_buf(),
                reason: "refusing to detach a non-directory gallery",
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(TaskError::Io {
                path: gallery.to_path_buf(),
                source,
            });
        }
    }
    let parent = gallery.parent().ok_or_else(|| TaskError::UnsafePath {
        path: gallery.to_path_buf(),
        reason: "gallery directory has no parent",
    })?;
    // Reservation and the move are one bounded synchronous section: the
    // container is exclusively ours, so no public gallery can interleave
    // between the two steps on this thread.
    let container = reserve_detach_container(parent)?;
    let child = container.join("gallery");
    match fs::rename(gallery, &child) {
        Ok(()) => {
            // The namespace is now owned: no new public gallery, writable
            // cfg.dir, title or alias may resolve here until retirement.
            file_ops::track_detached_container(&container);
            Ok(Some(DetachedGallery {
                container,
                child,
                public: gallery.to_path_buf(),
            }))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let _ = fs::remove_dir(&container);
            Ok(None)
        }
        Err(source) => {
            let _ = fs::remove_dir(&container);
            Err(TaskError::Io {
                path: gallery.to_path_buf(),
                source,
            })
        }
    }
}

static DETACH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Test-only deterministic detach candidates, scoped by the gallery parent
/// directory so parallel tests in different roots never interact. Each armed
/// name is consumed once by the next detach in its scope and still goes
/// through the production rename/reservation step.
#[cfg(test)]
static DETACH_SEAMS: Mutex<Vec<(String, String)>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn arm_detach_candidate(scope: &Path, name: &str) {
    DETACH_SEAMS
        .lock()
        .unwrap()
        .push((scope.to_string_lossy().into_owned(), name.to_string()));
}

#[cfg(test)]
fn take_detach_candidate(parent: &Path) -> Option<String> {
    let key = parent.to_string_lossy().into_owned();
    let mut seams = DETACH_SEAMS.lock().unwrap();
    seams
        .iter()
        .position(|(scope, _)| *scope == key)
        .map(|index| seams.remove(index).1)
}

struct StageCleanup {
    path: PathBuf,
    armed: bool,
}

impl StageCleanup {
    fn disarm(&mut self) {
        self.armed = false;
    }

    /// Remove the owned private file once and report the real outcome.
    fn remove(&mut self) -> std::io::Result<()> {
        self.armed = false;
        fs::remove_file(&self.path)
    }
}

impl Drop for StageCleanup {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

// ---------------------------------------------------------------------------
// Narrow test seams. They observe or pause the production writer; they never
// change identity validation, ZIP contents, publication or cleanup ordering.
// ---------------------------------------------------------------------------

/// Test-only fault injected into the production blocking writer.
#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ArchiveFault {
    /// Fail after the container bytes are complete, before publication.
    Prepare,
    /// Panic the blocking thread so the join itself fails.
    Panic,
}

/// Test-only handle that holds the blocking writer until the test releases it.
#[cfg(test)]
pub(crate) struct ArchiveGate {
    pub started: std::sync::mpsc::Receiver<()>,
    pub release: std::sync::mpsc::Sender<()>,
}

/// Test-only async handle that holds the archive worker between observing
/// FINISHED and re-validating the run at admission. Awaiting (not blocking)
/// keeps the LocalSet driving other production futures while it is held.
#[cfg(test)]
pub(crate) struct ArchiveWaitingGate {
    pub started: std::sync::mpsc::Receiver<()>,
    pub release: tokio::sync::oneshot::Sender<()>,
}

#[cfg(test)]
struct ArchiveSeam {
    marker: String,
    gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    fault: Option<ArchiveFault>,
}

#[cfg(test)]
type WaitingSeam = (
    String,
    std::sync::mpsc::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
);

#[cfg(test)]
static ARCHIVE_SEAMS: Mutex<Vec<ArchiveSeam>> = Mutex::new(Vec::new());
#[cfg(test)]
static WAITING_SEAMS: Mutex<Vec<WaitingSeam>> = Mutex::new(Vec::new());
/// Barrier between publication and detached-tree cleanup, keyed by the
/// committed ZIP path. The worker signals `started` after the ZIP is
/// published and waits for `release` before removing the detached payload.
#[cfg(test)]
static CLEANUP_SEAMS: Mutex<Vec<WaitingSeam>> = Mutex::new(Vec::new());
#[cfg(test)]
static WRITER_LOG: Mutex<Vec<String>> = Mutex::new(Vec::new());
#[cfg(test)]
static WRITER_STATS: Mutex<std::collections::BTreeMap<String, WriterStats>> =
    Mutex::new(std::collections::BTreeMap::new());

#[cfg(test)]
#[derive(Default)]
struct WriterStats {
    active: usize,
    peak: usize,
}

/// Writers are counted per public parent directory, so tests in different
/// temporary roots never share a peak and no test-level serialization is
/// needed.
#[cfg(test)]
fn scope_of(marker: &str) -> String {
    std::path::Path::new(marker)
        .parent()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_default()
}

#[cfg(test)]
fn arm_seam(
    marker: &str,
    gate: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
    fault: Option<ArchiveFault>,
) {
    let mut seams = ARCHIVE_SEAMS.lock().unwrap();
    if let Some(seam) = seams.iter_mut().find(|seam| seam.marker == marker) {
        if gate.is_some() {
            seam.gate = gate;
        }
        if fault.is_some() {
            seam.fault = fault;
        }
    } else {
        seams.push(ArchiveSeam {
            marker: marker.to_string(),
            gate,
            fault,
        });
    }
}

#[cfg(test)]
pub(crate) fn arm_archive_gate(marker: &str) -> ArchiveGate {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    arm_seam(marker, Some((started_tx, release_rx)), None);
    ArchiveGate {
        started: started_rx,
        release: release_tx,
    }
}

#[cfg(test)]
pub(crate) fn arm_archive_fault(marker: &str, fault: ArchiveFault) {
    arm_seam(marker, None, Some(fault));
}

#[cfg(test)]
pub(crate) fn arm_archive_waiting(marker: &str) -> ArchiveWaitingGate {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut seams = WAITING_SEAMS.lock().unwrap();
    seams.retain(|(armed, _, _)| armed != marker);
    seams.push((marker.to_string(), started_tx, release_rx));
    ArchiveWaitingGate {
        started: started_rx,
        release: release_tx,
    }
}

/// Arm the post-publication, pre-cleanup barrier for one committed ZIP path.
#[cfg(test)]
pub(crate) fn arm_archive_cleanup_gate(zip_marker: &str) -> ArchiveWaitingGate {
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut seams = CLEANUP_SEAMS.lock().unwrap();
    seams.retain(|(armed, _, _)| armed != zip_marker);
    seams.push((zip_marker.to_string(), started_tx, release_rx));
    ArchiveWaitingGate {
        started: started_rx,
        release: release_tx,
    }
}

#[cfg(test)]
pub(crate) async fn archive_cleanup_gate(zip_marker: &str) {
    let channels = {
        let mut seams = CLEANUP_SEAMS.lock().unwrap();
        seams
            .iter()
            .position(|(armed, _, _)| armed == zip_marker)
            .map(|index| seams.remove(index))
    };
    if let Some((_, started, release)) = channels {
        let _ = started.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(20), release).await;
    }
}

#[cfg(test)]
pub(crate) async fn archive_waiting_gate(marker: &str) {
    let channels = {
        let mut seams = WAITING_SEAMS.lock().unwrap();
        seams
            .iter()
            .position(|(armed, _, _)| armed == marker)
            .map(|index| seams.remove(index))
    };
    if let Some((_, started, release)) = channels {
        let _ = started.send(());
        let _ = tokio::time::timeout(std::time::Duration::from_secs(20), release).await;
    }
}

#[cfg(test)]
pub(crate) fn reset_archive_writer_stats(scope: &Path) {
    let key = scope.to_string_lossy().into_owned();
    WRITER_STATS.lock().unwrap().remove(&key);
    WRITER_LOG
        .lock()
        .unwrap()
        .retain(|marker| scope_of(marker) != key);
}

#[cfg(test)]
pub(crate) fn archive_writer_stats(scope: &Path) -> (usize, usize) {
    let key = scope.to_string_lossy().into_owned();
    WRITER_STATS
        .lock()
        .unwrap()
        .get(&key)
        .map(|stats| (stats.active, stats.peak))
        .unwrap_or((0, 0))
}

#[cfg(test)]
pub(crate) fn archive_writer_log(scope: &Path) -> Vec<String> {
    let key = scope.to_string_lossy().into_owned();
    WRITER_LOG
        .lock()
        .unwrap()
        .iter()
        .filter(|marker| scope_of(marker) == key)
        .cloned()
        .collect()
}

#[cfg(test)]
fn seam_fault(marker: &str) -> Option<ArchiveFault> {
    let mut seams = ARCHIVE_SEAMS.lock().unwrap();
    seams
        .iter_mut()
        .find(|seam| seam.marker == marker)
        .and_then(|seam| seam.fault.take())
}

#[cfg(test)]
fn seam_gate(point: &str, marker: &str) {
    if point != "writer" {
        return;
    }
    let channels = {
        let mut seams = ARCHIVE_SEAMS.lock().unwrap();
        seams
            .iter_mut()
            .find(|seam| seam.marker == marker)
            .and_then(|seam| seam.gate.take())
    };
    let Some((started, release)) = channels else {
        return;
    };
    let _ = started.send(());
    let _ = release.recv_timeout(std::time::Duration::from_secs(20));
}

#[cfg(test)]
struct WriterProbe {
    scope: String,
}

#[cfg(test)]
impl WriterProbe {
    fn enter(marker: &str) -> Self {
        WRITER_LOG.lock().unwrap().push(marker.to_string());
        let scope = scope_of(marker);
        let mut stats = WRITER_STATS.lock().unwrap();
        let entry = stats.entry(scope.clone()).or_default();
        entry.active += 1;
        entry.peak = entry.peak.max(entry.active);
        Self { scope }
    }
}

#[cfg(test)]
impl Drop for WriterProbe {
    fn drop(&mut self) {
        if let Some(stats) = WRITER_STATS.lock().unwrap().get_mut(&self.scope) {
            stats.active = stats.active.saturating_sub(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic English fixture: one gallery file in a temp root, no task
    /// object and no public archive.
    fn snapshot(root: &Path) -> ArchiveSnapshot {
        let gallery = root.join("gallery");
        fs::create_dir_all(&gallery).expect("gallery");
        fs::write(gallery.join("1.jpg"), b"synthetic-bytes").expect("image");
        ArchiveSnapshot {
            fpath: gallery.clone(),
            arc: gallery.with_extension("zip"),
            comment: "synthetic archive comment".to_string(),
            lock: Arc::new(Mutex::new(())),
        }
    }

    fn staged(prepared: PreparedArchive) -> PreparedStage {
        match prepared {
            PreparedArchive::Staged(stage) => stage,
            PreparedArchive::Existing(_) => panic!("expected a private stage"),
        }
    }

    /// A prepared-but-unobserved container (for example a result buffered in
    /// a channel whose receiver was dropped) owns its private stage through
    /// the RAII guard: dropping it removes exactly that file and never
    /// touches the gallery or the public archive path.
    #[test]
    fn prepared_stage_guard_removes_only_its_unpublished_container() {
        let root = tempfile::tempdir().expect("root");
        let snapshot = snapshot(root.path());
        let stage = staged(prepare_archive_snapshot(&snapshot, false).expect("prepare"));
        let stage_path = stage.stage.clone();
        assert!(
            stage_path.exists(),
            "the private stage exists before observation"
        );
        assert!(snapshot.fpath.is_dir());
        drop(stage);
        assert!(
            !stage_path.exists(),
            "an unobserved prepared stage must remove its own private file"
        );
        assert!(snapshot.fpath.is_dir(), "the gallery is never touched");
        assert!(!snapshot.arc.exists(), "no public archive is created");
    }

    struct RestoreMode(PathBuf);

    impl Drop for RestoreMode {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }

    /// Explicit discard reports the real unlink outcome instead of claiming
    /// success when the filesystem still holds an error.
    #[test]
    fn discard_prepared_reports_real_removal_outcome() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("root");
        let snapshot = snapshot(root.path());
        let stage = staged(prepare_archive_snapshot(&snapshot, false).expect("prepare"));
        let stage_path = stage.stage.clone();
        assert!(
            discard_prepared(PreparedArchive::Staged(stage)).is_ok(),
            "a writable parent removes the private stage"
        );
        assert!(!stage_path.exists());

        let stage = staged(prepare_archive_snapshot(&snapshot, false).expect("prepare"));
        let stage_path = stage.stage.clone();
        let _restore = RestoreMode(stage_path.parent().expect("parent").to_path_buf());
        fs::set_permissions(
            stage_path.parent().expect("parent"),
            fs::Permissions::from_mode(0o555),
        )
        .expect("read-only parent");
        let result = discard_prepared(PreparedArchive::Staged(stage));
        assert!(
            result.is_err(),
            "a failed unlink must be reported, not claimed as removal"
        );
        assert!(stage_path.exists(), "the file remains when unlink failed");
    }
}
