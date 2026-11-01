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
//! 2. [`commit_prepared`] performs the bounded namespace effects (detach the
//!    gallery to a private name, one rename over the archive path) and is
//!    only called by the runtime while it still owns the exact task run. The
//!    detached gallery is then removed from the blocking pool; cleanup can
//!    only ever touch that private path, never a reused public gallery.
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
/// is always consumed explicitly (renamed by [`commit_prepared`] or removed by
/// [`discard_prepared`]/the prepare guard), never by a hidden destructor.
pub struct PreparedStage {
    stage: PathBuf,
    arc: PathBuf,
    gallery: PathBuf,
    lock: Arc<Mutex<()>>,
    remove: bool,
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
    /// The gallery directory, already detached to this private name. The
    /// caller removes it on the blocking pool; nothing else may touch it.
    pub detached: Option<PathBuf>,
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

/// Remove a private staging file or a detached gallery tree. The runtime calls
/// the latter on the blocking pool so a large tree is never walked under a
/// `Task`/`App` borrow.
pub fn remove_detached(path: &Path) -> Result<(), TaskError> {
    file_ops::remove_tree(path)
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
    let mut cleanup = StageCleanup {
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
    cleanup.armed = false;
    Ok(PreparedArchive::Staged(PreparedStage {
        stage,
        arc: arc.clone(),
        gallery: dpath.clone(),
        lock: Arc::clone(&snapshot.lock),
        remove,
    }))
}

/// Publish a prepared container. The caller must have re-validated the exact
/// task owner/run synchronously before entering this function; every step is
/// a bounded namespace operation.
pub fn commit_prepared(prepared: PreparedArchive) -> Result<CommittedArchive, TaskError> {
    let stage = match prepared {
        PreparedArchive::Existing(path) => {
            return Ok(CommittedArchive {
                path,
                detached: None,
            });
        }
        PreparedArchive::Staged(stage) => stage,
    };
    let _guard = stage.lock.lock().unwrap_or_else(|error| error.into_inner());
    let detached = if stage.remove {
        match detach_gallery(&stage.gallery) {
            Ok(detached) => detached,
            Err(error) => {
                // Nothing was published: this attempt removes its own stage.
                let _ = fs::remove_file(&stage.stage);
                return Err(error);
            }
        }
    } else {
        None
    };
    match file_ops::publish(&stage.stage, &stage.arc) {
        Ok(()) => Ok(CommittedArchive {
            path: stage.arc,
            detached,
        }),
        Err(error) => {
            // The bound run keeps its images: put the detached gallery back
            // and remove only this attempt's staging file.
            if let Some(trash) = &detached {
                let _ = fs::rename(trash, &stage.gallery);
            }
            let _ = fs::remove_file(&stage.stage);
            Err(error)
        }
    }
}

/// Remove the private staging file of a container that was never published.
/// A single unlink is bounded; the ZIP bytes are never copied here.
pub fn discard_prepared(prepared: PreparedArchive) {
    if let PreparedArchive::Staged(stage) = prepared {
        let _ = fs::remove_file(&stage.stage);
    }
}

/// Move the gallery directory to a private name in the same parent. Renaming
/// is the only removal step allowed under the task borrow; walking the tree
/// happens later on the blocking pool and only ever sees this detached path.
fn detach_gallery(gallery: &Path) -> Result<Option<PathBuf>, TaskError> {
    match fs::symlink_metadata(gallery) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(TaskError::UnsafePath {
                path: gallery.to_path_buf(),
                reason: "refusing to delete a symlinked directory",
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
    for _ in 0..1024 {
        let counter = TRASH_COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let trash = parent.join(format!(".xehentai-remove-{}-{counter}", std::process::id()));
        match fs::rename(gallery, &trash) {
            Ok(()) => return Ok(Some(trash)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(source) => {
                return Err(TaskError::Io {
                    path: gallery.to_path_buf(),
                    source,
                });
            }
        }
    }
    Err(TaskError::Io {
        path: gallery.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no free detached gallery name",
        ),
    })
}

static TRASH_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

struct StageCleanup {
    path: PathBuf,
    armed: bool,
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
