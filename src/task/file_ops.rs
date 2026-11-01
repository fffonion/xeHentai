//! File, naming and streaming work of `xeHentai/task.py`.
//!
//! Everything here is a `Task` method or a helper it calls directly. The
//! authorized download root is fixed when the task is created and all writes
//! stay below it; paths that would leave it (empty titles, `..` names, symlink
//! children) are refused instead of being followed like the reference did.
#![allow(dead_code)]

use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use regex::Regex;

use super::{FidKey, RenameError, Task, TaskError};

static STAGE_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Lifetime-bound registry of exclusively owned detached containers.
///
/// Each entry is the canonicalized container path from reservation until
/// explicit retirement (`remove_detached` success or `restore` success).
/// While an entry is present, no new public gallery, writable cfg.dir,
/// title or canonical alias may resolve to that container or any of its
/// descendants: admission is rejected before any directory/stage/file
/// write. After retirement the exact ordinary name is usable again, so
/// there is no blanket prefix ban.
static ACTIVE_DETACHED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());

fn detached_key(container: &Path) -> PathBuf {
    boundary_path(container).unwrap_or_else(|_| normalize(container))
}

/// Record a freshly reserved owned container. Called once the gallery has
/// been moved into the container; only this path addresses the payload.
pub(crate) fn track_detached_container(container: &Path) {
    let key = detached_key(container);
    let mut active = ACTIVE_DETACHED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !active.iter().any(|entry| entry == &key) {
        active.push(key);
    }
}

/// Forget a retired container. Called only when the owned payload is gone
/// (successful tree removal or successful restore that removed the empty
/// container). A failed cleanup keeps the entry so the payload stays
/// protected.
pub(crate) fn untrack_detached_container(container: &Path) {
    let key = detached_key(container);
    let lexical = normalize(container);
    let mut active = ACTIVE_DETACHED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    active.retain(|entry| entry != &key && entry != &lexical);
}

/// Reject a candidate public path that would enter an owned container.
/// The check is component-wise on canonicalized paths, so `..`, symlink
/// and `legalpath` aliases are judged by their real target. Safe to call
/// before any write.
pub(crate) fn ensure_not_inside_detached(candidate: &Path) -> Result<(), TaskError> {
    let canon = boundary_path(candidate).unwrap_or_else(|_| normalize(candidate));
    let active = ACTIVE_DETACHED
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for owner in active.iter() {
        if path_within(owner, &canon) {
            return Err(TaskError::UnsafePath {
                path: candidate.to_path_buf(),
                reason: "detached gallery container is still owned",
            });
        }
    }
    Ok(())
}

/// The task-owned download directory and the two short locks the reference
/// keeps on `Task` (`_f_lock` for the filesystem and `_cnt_lock` for the
/// finished counter). They are never held across a stream.
pub struct TaskFiles {
    root: PathBuf,
    f_lock: Arc<Mutex<()>>,
    cnt_lock: Mutex<()>,
}

impl TaskFiles {
    pub fn new(root: PathBuf) -> Arc<Self> {
        Arc::new(Self {
            root,
            f_lock: Arc::new(Mutex::new(())),
            cnt_lock: Mutex::new(()),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Cloneable handle for blocking work that must take the same file lock
    /// from its own thread (the archive writer). The lock is still taken for
    /// the duration of that work, exactly like `task.py:401`.
    pub fn lock_handle(&self) -> Arc<Mutex<()>> {
        Arc::clone(&self.f_lock)
    }

    pub(super) fn lock(&self) -> MutexGuard<'_, ()> {
        self.f_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub(super) fn counter_lock(&self) -> MutexGuard<'_, ()> {
        self.cnt_lock
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// True while a short file critical section is held. Used by the
    /// concurrency checks to prove a stream does not hold the task file lock.
    pub fn is_locked(&self) -> bool {
        self.f_lock.try_lock().is_err()
    }

    /// `root/<legalpath(title)>`, with the same single-component rule the
    /// reference gets from `legalpath` plus explicit traversal and symlink
    /// refusal.
    pub(super) fn task_dir(&self, component: &str) -> Result<PathBuf, TaskError> {
        check_component(component)?;
        let path = self.root.join(component);
        // Lifetime-bound owned namespace: while a detached container owns
        // this namespace, no new public gallery may resolve here. Rejected
        // before any directory/stage/file write and before symlink checks.
        ensure_not_inside_detached(&path)?;
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() {
                    return Err(TaskError::UnsafePath {
                        path,
                        reason: "gallery directory is a symlink",
                    });
                }
                if !metadata.is_dir() {
                    return Err(TaskError::UnsafePath {
                        path,
                        reason: "gallery path is not a directory",
                    });
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => return Err(TaskError::Io { path, source }),
        }
        Ok(path)
    }

    /// A file directly inside `dir`; refuses separators, dot components and
    /// symlink targets.
    pub(super) fn child_file(&self, dir: &Path, name: &str) -> Result<PathBuf, TaskError> {
        check_component(name)?;
        // The gallery directory itself must not be an owned container: this
        // covers cfg.dir descendants that bypass `task_dir` and keeps every
        // direct file write inside the same lifetime-bound boundary.
        ensure_not_inside_detached(dir)?;
        let path = dir.join(name);
        // The file itself must not land inside an owned container either:
        // a new gallery named exactly like a container resolves here.
        ensure_not_inside_detached(&path)?;
        if path.parent() != Some(dir) {
            return Err(TaskError::UnsafePath {
                path,
                reason: "file is not a direct child of the gallery directory",
            });
        }
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => Err(TaskError::UnsafePath {
                path,
                reason: "target is a symlink",
            }),
            Ok(_) => Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(path),
            Err(source) => Err(TaskError::Io { path, source }),
        }
    }
}

fn check_component(component: &str) -> Result<(), TaskError> {
    if component.is_empty() {
        return Err(TaskError::UnsafePath {
            path: PathBuf::from(component),
            reason: "empty path component would name the download root itself",
        });
    }
    if component == "." || component == ".." {
        return Err(TaskError::UnsafePath {
            path: PathBuf::from(component),
            reason: "dot path component",
        });
    }
    if component.contains('\0') {
        return Err(TaskError::UnsafePath {
            path: PathBuf::from(component),
            reason: "NUL byte in path component",
        });
    }
    if component.contains('/') || (cfg!(windows) && component.contains('\\')) {
        return Err(TaskError::UnsafePath {
            path: PathBuf::from(component),
            reason: "path separator in a single component",
        });
    }
    Ok(())
}

/// Lexically resolve a configured directory against the application base.
pub fn resolve_dir(base: &Path, dir: &str) -> PathBuf {
    let path = Path::new(dir);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    normalize(&joined)
}

pub fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => out.push(prefix.as_os_str()),
            Component::RootDir => out.push(Path::new(std::path::MAIN_SEPARATOR_STR)),
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            Component::Normal(part) => out.push(part),
        }
    }
    out
}

/// Resolve a directory for the authorization boundary.
///
/// The longest existing ancestor is canonicalized so symlinked components are
/// judged by their real target, missing tail components are kept as ordinary
/// names, and a dangling symlink or an unreadable component refuses the whole
/// path instead of silently falling back to a lexical prefix.
pub fn boundary_path(path: &Path) -> Result<PathBuf, TaskError> {
    let normalized = normalize(path);
    let mut tail: Vec<OsString> = Vec::new();
    let mut current = normalized.as_path();
    loop {
        match fs::canonicalize(current) {
            Ok(mut resolved) => {
                for part in tail.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                match fs::symlink_metadata(current) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        return Err(TaskError::UnsafePath {
                            path: current.to_path_buf(),
                            reason: "path component is a dangling symlink",
                        });
                    }
                    Ok(_) => {}
                    Err(meta_error) if meta_error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(source) => {
                        return Err(TaskError::Io {
                            path: current.to_path_buf(),
                            source,
                        });
                    }
                }
                let Some(parent) = current.parent() else {
                    return Ok(normalized);
                };
                let Some(name) = current.file_name() else {
                    return Ok(normalized);
                };
                tail.push(name.to_os_string());
                current = parent;
            }
            Err(source) => {
                return Err(TaskError::Io {
                    path: current.to_path_buf(),
                    source,
                });
            }
        }
    }
}

/// Component-wise containment; never a raw string prefix.
pub fn path_within(root: &Path, candidate: &Path) -> bool {
    candidate.starts_with(root)
}

/// An FD-identity snapshot of a directory: the device and inode the path
/// resolved to when the snapshot was taken. Compared against the actually
/// opened directory FD, so a parent swap between snapshot and open fails
/// closed instead of serving the replacement.
pub type DirIdentity = (u64, u64);

/// Directory FD identity via `fstat`, or `None` when the path cannot be
/// stated (missing root: the caller treats this as a missing leaf, and the
/// binding check is skipped).
pub fn dir_identity(path: &Path) -> Option<DirIdentity> {
    let file = std::fs::File::open(path).ok()?;
    let metadata = file.metadata().ok()?;
    if !metadata.is_dir() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some((metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        None
    }
}

// ---------------------------------------------------------------------------
// Race-safe confined open (M5 R2 B3.2).
// ---------------------------------------------------------------------------

/// Open `root/name` as a regular file confined to the owned gallery root,
/// validating the OPENED file descriptor (never just the prechecked path),
/// so a swap between check and open cannot smuggle bytes in.
///
/// The leaf is opened relative to a pinned root directory FD. A symlink
/// leaf is denied on the fast path; an inside-root alias is served only
/// when the opened FD's identity matches the resolved target and the
/// target stays inside the freshly canonicalized root and off the reserved
/// persistence set. Magic links, outside targets and reserved targets are
/// denied before any read, as are non-regular files (directories, pipes,
/// devices, sockets). The opened root FD must also still be the freshly
/// canonicalized root AND the snapshot-time identity, so parent/root swaps
/// fail closed. `O_NONBLOCK` avoids hangs on FIFOs during the open; it is
/// cleared before the handle is returned.
///
/// Returns `Ok(None)` only when the leaf (or the root itself) is genuinely
/// missing, so the caller can apply the reference ZIP fallback; every
/// confinement failure is `Err` (denied: no fallback, no bytes).
/// No production debug hook exists; the `#[cfg(test)]` seam below is the
/// only deterministic window and is compiled out of production builds.
pub fn open_confined_regular(
    root: &Path,
    snapshot_id: Option<DirIdentity>,
    name: &str,
    reserved: &[PathBuf],
) -> Result<Option<std::fs::File>, TaskError> {
    use rustix::fs::{Mode, OFlags};
    check_component(name)?;
    let root_fd = match rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => {
            return Err(TaskError::Io {
                path: root.to_path_buf(),
                source: std::io::Error::from_raw_os_error(error.raw_os_error()),
            });
        }
    };
    let root_stat = rustix::fs::fstat(&root_fd).map_err(|error| TaskError::Io {
        path: root.to_path_buf(),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    if rustix::fs::FileType::from_raw_mode(root_stat.st_mode) != rustix::fs::FileType::Directory {
        return Err(TaskError::UnsafePath {
            path: root.to_path_buf(),
            reason: "confined root is not a directory",
        });
    }
    #[cfg(test)]
    run_preopen_seam(root, name);
    let leaf = match rustix::fs::openat(
        &root_fd,
        name,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) if error == rustix::io::Errno::LOOP => {
            return open_confined_alias(&root_fd, root, snapshot_id, name, reserved);
        }
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => {
            return Err(TaskError::Io {
                path: root.join(name),
                source: std::io::Error::from_raw_os_error(error.raw_os_error()),
            });
        }
    };
    finish_confined_leaf(root_fd, leaf, root, snapshot_id, name, reserved)
}

/// Symlink-leaf path: the leaf is a link. Follow it, then serve the opened
/// FD only when its identity matches the contained, non-reserved resolved
/// target. Any mismatch (including a link swap racing the resolve) denies.
fn open_confined_alias(
    root_fd: &rustix::fd::OwnedFd,
    root: &Path,
    snapshot_id: Option<DirIdentity>,
    name: &str,
    reserved: &[PathBuf],
) -> Result<Option<std::fs::File>, TaskError> {
    use rustix::fs::{Mode, OFlags};
    let leaf = match rustix::fs::openat(
        root_fd,
        name,
        OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(error) if error == rustix::io::Errno::NOENT => return Ok(None),
        Err(error) => {
            return Err(TaskError::Io {
                path: root.join(name),
                source: std::io::Error::from_raw_os_error(error.raw_os_error()),
            });
        }
    };
    let leaf_stat = rustix::fs::fstat(&leaf).map_err(|error| TaskError::Io {
        path: root.join(name),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    if rustix::fs::FileType::from_raw_mode(leaf_stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(TaskError::UnsafePath {
            path: root.join(name),
            reason: "confined alias target is not a regular file",
        });
    }
    // Resolve the link through the pinned root FD and require containment;
    // the opened FD must then BE that resolved target (dev, ino).
    let target =
        rustix::fs::readlinkat(root_fd, name, Vec::new()).map_err(|error| TaskError::Io {
            path: root.join(name),
            source: std::io::Error::from_raw_os_error(error.raw_os_error()),
        })?;
    let target = target.to_string_lossy();
    let fresh_real = canonical_root(root)?;
    let resolved = if Path::new(target.as_ref()).is_absolute() {
        normalize(Path::new(target.as_ref()))
    } else {
        normalize(&fresh_real.join(Path::new(target.as_ref())))
    };
    if !path_within(&fresh_real, &resolved) {
        return Err(TaskError::UnsafePath {
            path: root.join(name),
            reason: "link target escapes the gallery root",
        });
    }
    let resolved_stat = std::fs::metadata(&resolved).map_err(|_| TaskError::UnsafePath {
        path: root.join(name),
        reason: "link target cannot be stated",
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (resolved_stat.dev(), resolved_stat.ino()) != (leaf_stat.st_dev, leaf_stat.st_ino) {
            return Err(TaskError::UnsafePath {
                path: root.join(name),
                reason: "link target changed under the open",
            });
        }
    }
    deny_reserved(&leaf_stat, root.join(name), reserved)?;
    bind_root(root_fd, &root_stat_of(root_fd)?, root, snapshot_id)?;
    Ok(Some(clear_nonblock(leaf, root.join(name))?))
}

/// Validate the fast-path (non-link) leaf: regular file, off the reserved
/// set, root still bound; then clear `O_NONBLOCK` and hand over the file.
fn finish_confined_leaf(
    root_fd: rustix::fd::OwnedFd,
    leaf: rustix::fd::OwnedFd,
    root: &Path,
    snapshot_id: Option<DirIdentity>,
    name: &str,
    reserved: &[PathBuf],
) -> Result<Option<std::fs::File>, TaskError> {
    let leaf_stat = rustix::fs::fstat(&leaf).map_err(|error| TaskError::Io {
        path: root.join(name),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    if rustix::fs::FileType::from_raw_mode(leaf_stat.st_mode) != rustix::fs::FileType::RegularFile {
        return Err(TaskError::UnsafePath {
            path: root.join(name),
            reason: "confined target is not a regular file",
        });
    }
    deny_reserved(&leaf_stat, root.join(name), reserved)?;
    let root_stat = rustix::fs::fstat(&root_fd).map_err(|error| TaskError::Io {
        path: root.to_path_buf(),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    bind_root(&root_fd, &root_stat_of_fd(&root_stat), root, snapshot_id)?;
    Ok(Some(clear_nonblock(leaf, root.join(name))?))
}

/// Deny when the opened leaf IS a reserved persistence file (dev, ino).
fn deny_reserved(
    leaf_stat: &rustix::fs::Stat,
    path: PathBuf,
    reserved: &[PathBuf],
) -> Result<(), TaskError> {
    for entry in reserved {
        let Ok(metadata) = std::fs::metadata(entry) else {
            continue;
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if (metadata.dev(), metadata.ino()) == (leaf_stat.st_dev, leaf_stat.st_ino) {
                return Err(TaskError::UnsafePath {
                    path,
                    reason: "target is reserved persistence",
                });
            }
        }
    }
    Ok(())
}

/// Bind the pinned root FD to the freshly canonicalized root and, when the
/// caller has one, to the snapshot-time identity. Any replacement of the
/// root (or of a parent component) between snapshot and open fails closed.
fn bind_root(
    _root_fd: &rustix::fd::OwnedFd,
    root_stat: &DirIdentity,
    root: &Path,
    snapshot_id: Option<DirIdentity>,
) -> Result<(), TaskError> {
    if let Some(snapshot) = snapshot_id
        && *root_stat != snapshot
    {
        return Err(TaskError::UnsafePath {
            path: root.to_path_buf(),
            reason: "gallery root changed since the snapshot",
        });
    }
    let fresh_real = canonical_root(root)?;
    let fresh_stat = std::fs::metadata(&fresh_real).map_err(|_| TaskError::UnsafePath {
        path: root.to_path_buf(),
        reason: "gallery root cannot be stated",
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if (fresh_stat.dev(), fresh_stat.ino()) != *root_stat {
            return Err(TaskError::UnsafePath {
                path: root.to_path_buf(),
                reason: "gallery root changed under the open",
            });
        }
    }
    Ok(())
}

/// Canonicalize the root for the containment/binding checks. Fail closed:
/// any canonicalization failure denies instead of serving.
fn canonical_root(root: &Path) -> Result<PathBuf, TaskError> {
    boundary_path(root).map_err(|_| TaskError::UnsafePath {
        path: root.to_path_buf(),
        reason: "gallery root cannot be canonicalized",
    })
}

fn root_stat_of(root_fd: &rustix::fd::OwnedFd) -> Result<DirIdentity, TaskError> {
    let stat = rustix::fs::fstat(root_fd).map_err(|error| TaskError::Io {
        path: PathBuf::from("<gallery root>"),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    Ok(root_stat_of_fd(&stat))
}

fn root_stat_of_fd(stat: &rustix::fs::Stat) -> DirIdentity {
    (stat.st_dev, stat.st_ino)
}

/// Clear `O_NONBLOCK` (set only to avoid hangs while opening) so the
/// returned file has ordinary blocking semantics for the streaming reader.
fn clear_nonblock(leaf: rustix::fd::OwnedFd, path: PathBuf) -> Result<std::fs::File, TaskError> {
    use rustix::fs::OFlags;
    rustix::fs::fcntl_setfl(&leaf, OFlags::empty()).map_err(|error| TaskError::Io {
        path: path.clone(),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    Ok(std::fs::File::from(leaf))
}

// ---------------------------------------------------------------------------
// TEST-ONLY deterministic barrier between precheck and open.
// ---------------------------------------------------------------------------

/// TEST-ONLY swap performed on the calling thread between the link precheck
/// and the open, so a regular inside-gallery file becomes an outside or
/// reserved symlink (or a swapped parent) exactly in the race window. Armed
/// once per `(root, name)` open attempt; a no-op unless armed. Production
/// builds contain no hook at all.
#[cfg(test)]
#[derive(Debug)]
pub(crate) enum PreopenSwap {
    /// Replace `root/name` with a symlink to `target`.
    LeafLink(PathBuf),
    /// Rename `root` aside and replace it with a symlink to `target`.
    RootLink(PathBuf),
}

#[cfg(test)]
static PREOPEN_SEAMS: Mutex<Vec<(String, String, PreopenSwap)>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn arm_preopen_swap(root: &Path, name: &str, swap: PreopenSwap) {
    PREOPEN_SEAMS.lock().unwrap().push((
        root.to_string_lossy().into_owned(),
        name.to_string(),
        swap,
    ));
}

#[cfg(test)]
fn take_preopen_swap(root: &Path, name: &str) -> Option<PreopenSwap> {
    let key = root.to_string_lossy().into_owned();
    let mut seams = PREOPEN_SEAMS.lock().unwrap();
    seams
        .iter()
        .position(|(scope, leaf, _)| *scope == key && *leaf == name)
        .map(|index| seams.remove(index).2)
}

/// Run the armed swap for this open attempt, if any. Called by the gallery
/// and sibling-archive open paths between their precheck and their open, on
/// the same thread, so the swap lands deterministically inside the window.
#[cfg(test)]
pub(crate) fn run_preopen_seam(root: &Path, name: &str) {
    match take_preopen_swap(root, name) {
        None => {}
        Some(PreopenSwap::LeafLink(target)) => {
            let path = root.join(name);
            let _ = fs::remove_file(&path);
            #[cfg(unix)]
            let _ = std::os::unix::fs::symlink(&target, &path);
        }
        Some(PreopenSwap::RootLink(target)) => {
            let backup = root.with_extension("seambak");
            let _ = fs::remove_dir_all(&backup);
            if fs::rename(root, &backup).is_ok() {
                #[cfg(unix)]
                let _ = std::os::unix::fs::symlink(&target, root);
            }
        }
    }
}

pub(super) fn gallery_identifier(url: &str) -> (Option<String>, Option<String>) {
    static INDEX: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r".+/(\d+)/([^/]+)/*").expect("index regex"));
    INDEX
        .captures(url)
        .map(|captures| (Some(captures[1].to_string()), Some(captures[2].to_string())))
        .unwrap_or((None, None))
}

pub(super) fn gallery_hash(url: &str) -> Option<String> {
    static GALLERY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"/([a-f0-9]{10})/[^-]+-(\d+)").expect("gallery regex")
    });
    GALLERY
        .captures(url)
        .map(|captures| captures[1].to_string())
}

pub(super) fn gallery_fid(url: &str) -> Option<i64> {
    static GALLERY: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"/([a-f0-9]{10})/[^-]+-(\d+)").expect("gallery regex")
    });
    GALLERY
        .captures(url)
        .and_then(|captures| captures[2].parse::<i64>().ok())
}

pub(super) fn image_hash_captures(url: &str) -> Option<String> {
    static IMAGE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"/([a-f0-9]{40})-(\d+)-(\d+)-(\d+)-([a-z]{0,4})").expect("image regex")
    });
    IMAGE.captures(url).map(|captures| captures[1].to_string())
}

pub(super) fn has_image_hash(url: &str) -> bool {
    image_hash_captures(url).is_some()
}

pub(super) fn fullimg_fid(url: &str) -> Option<i64> {
    static FULL: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"fullimg/\d+/(\d+)").expect("fullimg regex"));
    FULL.captures(url)
        .and_then(|captures| captures[1].parse::<i64>().ok())
}

pub(super) fn redirect_target(url: &str) -> Option<String> {
    static REDIRECT: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"redirect=(.+)").expect("redirect regex"));
    REDIRECT
        .captures(url)
        .map(|captures| captures[1].to_string())
}

/// First path segment of a redirect URL (`/([^/?]+)(?:\?|$)`).
pub(super) fn redirect_fname(url: &str) -> Option<String> {
    static NAME: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"/([^/?]+)(?:\?|$)").expect("name regex"));
    NAME.captures(url).map(|captures| captures[1].to_string())
}

pub(super) fn base_url(url: &str) -> Option<String> {
    static SITE: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"https*://(?:[g.]*e-|ex)hentai\.org").expect("site regex")
    });
    SITE.find(url).map(|found| found.as_str().to_string())
}

pub(super) fn exhentai_path(url: &str) -> Option<String> {
    static EXH: std::sync::LazyLock<Regex> = std::sync::LazyLock::new(|| {
        Regex::new(r"(?:https*://[g.]*e-hentai\.org)(.+)").expect("exhentai regex")
    });
    EXH.captures(url).map(|captures| captures[1].to_string())
}

/// `re.sub` with a literal replacement. `gid`/`sethash` come from the URL and
/// are inserted literally, matching the reference for normal identifiers.
pub(super) fn regex_replace_all(pattern: &str, replacement: &str, text: &str) -> String {
    match Regex::new(pattern) {
        Ok(regex) => regex
            .replace_all(text, regex::NoExpand(replacement))
            .into_owned(),
        Err(_) => text.to_string(),
    }
}

pub(super) fn path_exists(path: &Path) -> Result<bool, TaskError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(TaskError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path is a symlink",
        }),
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(source) => Err(TaskError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn file_size(path: &Path) -> Result<u64, TaskError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(TaskError::UnsafePath {
            path: path.to_path_buf(),
            reason: "path is a symlink",
        }),
        Ok(metadata) => Ok(metadata.len()),
        Err(source) => Err(TaskError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn remove_file(path: &Path) -> Result<(), TaskError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(TaskError::UnsafePath {
            path: path.to_path_buf(),
            reason: "refusing to delete through a symlink",
        }),
        Ok(_) => fs::remove_file(path).map_err(|source| TaskError::Io {
            path: path.to_path_buf(),
            source,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(TaskError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn remove_tree(path: &Path) -> Result<(), TaskError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(TaskError::UnsafePath {
            path: path.to_path_buf(),
            reason: "refusing to delete a symlinked directory",
        }),
        Ok(metadata) if !metadata.is_dir() => Err(TaskError::UnsafePath {
            path: path.to_path_buf(),
            reason: "not a directory",
        }),
        Ok(_) => fs::remove_dir_all(path).map_err(|source| TaskError::Io {
            path: path.to_path_buf(),
            source,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(TaskError::Io {
            path: path.to_path_buf(),
            source,
        }),
    }
}

pub(super) fn make_dir(path: &Path) -> Result<(), TaskError> {
    fs::create_dir(path).map_err(|source| TaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

pub(super) fn zip_path(fpath: &Path) -> Result<PathBuf, TaskError> {
    let name = fpath.file_name().ok_or_else(|| TaskError::UnsafePath {
        path: fpath.to_path_buf(),
        reason: "gallery directory has no name",
    })?;
    let mut zipname = OsString::from(name);
    zipname.push(".zip");
    let parent = fpath.parent().ok_or_else(|| TaskError::UnsafePath {
        path: fpath.to_path_buf(),
        reason: "gallery directory has no parent",
    })?;
    Ok(parent.join(zipname))
}

/// Create the streaming staging file in the destination directory, so the
/// final publish is a same-filesystem rename.
pub(super) fn create_stage(dir: &Path) -> Result<PathBuf, TaskError> {
    create_stage_with_prefix(dir, "xehentai-")
}

pub(super) fn create_stage_with_prefix(dir: &Path, prefix: &str) -> Result<PathBuf, TaskError> {
    for _ in 0..1024 {
        let counter = STAGE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let name = format!("{prefix}{}-{counter}", std::process::id());
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(_) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(source) => return Err(TaskError::Io { path, source }),
        }
    }
    Err(TaskError::Io {
        path: dir.to_path_buf(),
        source: std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "no free staging file name",
        ),
    })
}

/// Publish a finished staging file at `dest` with one rename. On failure the
/// staging file is left for the caller to clean up and the destination is
/// untouched.
pub(super) fn publish(stage: &Path, dest: &Path) -> Result<(), TaskError> {
    if let Ok(metadata) = fs::symlink_metadata(dest)
        && metadata.file_type().is_symlink()
    {
        return Err(TaskError::UnsafePath {
            path: dest.to_path_buf(),
            reason: "target is a symlink",
        });
    }
    fs::rename(stage, dest).map_err(|error| TaskError::PublishFailed {
        from: stage.to_path_buf(),
        to: dest.to_path_buf(),
        message: error.to_string(),
    })
}

/// Copy a duplicate image through a staging file in the same directory.
pub(super) fn copy_file(from: &Path, to: &Path) -> Result<(), TaskError> {
    let dir = to.parent().ok_or_else(|| TaskError::UnsafePath {
        path: to.to_path_buf(),
        reason: "duplicate target has no parent",
    })?;
    let stage = create_stage(dir)?;
    let result = (|| -> Result<(), TaskError> {
        fs::copy(from, &stage).map_err(|source| TaskError::Io {
            path: from.to_path_buf(),
            source,
        })?;
        publish(&stage, to)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&stage);
    }
    result
}

pub(super) fn open_new_file(path: &Path) -> Result<File, TaskError> {
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|source| TaskError::Io {
            path: path.to_path_buf(),
            source,
        })
}

/// A response body staged in the task directory, before publish.
pub struct StagedSave {
    pub stage: PathBuf,
    pub dest: PathBuf,
    fid: i64,
    fpath: PathBuf,
}

/// Result of the pre-stream save checks.
pub enum SaveStart {
    AlreadyDone(PathBuf),
    Staged(StagedSave),
}

impl Task {
    /// Reference `save_file` pre-stream half: resolve the destination and
    /// create a same-filesystem staging file. Shared by the synchronous test
    /// helper and the real async transport consumer.
    pub fn begin_staged_save(
        &mut self,
        imgurl: &str,
        redirect_url: &str,
    ) -> Result<SaveStart, TaskError> {
        let fpath = self.get_fpath()?;
        let img_hash = {
            let _guard = self.files.lock();
            if !path_exists(&fpath)? {
                make_dir(&fpath)?;
            }
            // `imgurl` is only logged by the reference; the hash comes from
            // the redirect URL, exactly as in task.py:230.
            let _ = imgurl;
            Self::get_imghash(redirect_url)?
        };
        let (fid, mut fname) = self.get_fname(&img_hash)?;
        if let Some(redirect_name) = redirect_fname(redirect_url) {
            fname = redirect_name;
            if let Some(entry) = self.reload_map.get_mut(&img_hash) {
                entry.1 = fname;
            }
        }
        let _ = fname;

        let dest = self
            .files
            .child_file(&fpath, &self.get_fidpad(fid, "jpg")?)?;
        if path_exists(&dest)? && file_size(&dest)? > 0 {
            return Ok(SaveStart::AlreadyDone(dest));
        }
        let stage = create_stage(&fpath)?;
        Ok(SaveStart::Staged(StagedSave {
            stage,
            dest,
            fid,
            fpath,
        }))
    }

    /// Publish a fully written staging file and its pending duplicate copies.
    pub fn commit_staged_save(&mut self, staged: StagedSave) -> Result<PathBuf, TaskError> {
        let StagedSave {
            stage,
            dest,
            fid,
            fpath,
        } = staged;
        let published = {
            let files = Arc::clone(&self.files);
            let _guard = files.lock();
            (|| -> Result<(), TaskError> {
                publish(&stage, &dest)?;
                self.set_fid_finished(fid);
                if let Some(copies) = self.duplicate_map.get(&fid).cloned() {
                    for copy in &copies {
                        if *copy == fid {
                            continue;
                        }
                        let copy_path = self
                            .files
                            .child_file(&fpath, &self.get_fidpad(*copy, "jpg")?)?;
                        copy_file(&dest, &copy_path)?;
                        self.set_fid_finished(*copy);
                    }
                    self.duplicate_map.shift_remove(&fid);
                }
                Ok(())
            })()
        };
        if let Err(error) = published {
            let _ = fs::remove_file(&stage);
            return Err(error);
        }
        Ok(dest)
    }

    /// Remove a staging file without publishing.
    pub fn abort_staged_save(staged: StagedSave) {
        let _ = fs::remove_file(&staged.stage);
    }

    /// Reference `save_file`: stream the response to a staging file, then
    /// publish it and its pending duplicate copies. The stream and the
    /// cancellation callback run while the file lock is free.
    pub fn save_file<F, C>(
        &mut self,
        imgurl: &str,
        redirect_url: &str,
        mut chunks: F,
        cancel: C,
    ) -> Result<Option<PathBuf>, TaskError>
    where
        F: FnMut() -> Option<Vec<u8>>,
        C: Fn() -> bool,
    {
        let staged = match self.begin_staged_save(imgurl, redirect_url)? {
            SaveStart::AlreadyDone(dest) => return Ok(Some(dest)),
            SaveStart::Staged(staged) => staged,
        };
        let streamed = (|| -> Result<bool, TaskError> {
            let mut file = File::options()
                .write(true)
                .truncate(true)
                .open(&staged.stage)
                .map_err(|source| TaskError::Io {
                    path: staged.stage.clone(),
                    source,
                })?;
            while let Some(chunk) = chunks() {
                if cancel() {
                    return Ok(false);
                }
                file.write_all(&chunk).map_err(|source| TaskError::Io {
                    path: staged.stage.clone(),
                    source,
                })?;
            }
            file.flush().map_err(|source| TaskError::Io {
                path: staged.stage.clone(),
                source,
            })?;
            Ok(true)
        })();
        match streamed {
            Ok(true) => {}
            Ok(false) => {
                Self::abort_staged_save(staged);
                return Ok(None);
            }
            Err(error) => {
                Self::abort_staged_save(staged);
                return Err(error);
            }
        }
        self.commit_staged_save(staged).map(Some)
    }

    /// Reference `rename_fname`, including the reference collision counter
    /// crash: a second clash against an existing `(n)` name raises instead of
    /// renaming the file.
    pub fn rename_fname(&mut self) -> Result<Vec<RenameError>, TaskError> {
        let fpath = self.get_fpath()?;
        let tmppath = fpath.join(crate::r#const::RENAME_TMPDIR);
        let mut errors = Vec::new();
        let mut done_list: Vec<PathBuf> = Vec::new();
        let mut count = 0i64;
        let keys: Vec<FidKey> = self.renamed_map.keys().cloned().collect();
        for key in keys {
            let fid = key.as_fid()?;
            let name = self.renamed_map.get(&key).cloned().unwrap_or_default();
            let mut original_ext = python_splitext(&name).1;
            if original_ext.is_empty() {
                original_ext = python_splitext(&name).0;
            }
            if !self.config.rename_ori && original_ext.to_lowercase() == ".jpg" {
                continue;
            }
            let mut fname_ori = fpath.join(self.get_fidpad(fid, "jpg")?);
            let mut fname_to = if self.config.rename_ori {
                let tmp_name = self.get_fidpad(fid, "jpg")?;
                if path_exists(&tmppath.join(&tmp_name))? {
                    fname_ori = tmppath.join(tmp_name);
                }
                let legal = crate::util::legalpath(&name);
                self.files.child_file(&fpath, &legal)?
            } else {
                let extension: String = original_ext.chars().skip(1).collect();
                self.files
                    .child_file(&fpath, &self.get_fidpad(fid, &extension)?)?
            };
            // The reference uses a `while` here; every path inside either
            // breaks or returns, so one pass is the real behaviour.
            #[allow(clippy::never_loop)]
            while fname_ori != fname_to {
                if path_exists(&fname_ori)? {
                    while path_exists(&fname_to)? {
                        let (base, ext) = python_splitext_path(&fname_to);
                        let numbered = numbered_suffix(&base);
                        if self.config.rename_ori && !done_list.contains(&fname_to) {
                            if !path_exists(&tmppath)? {
                                make_dir(&tmppath)?;
                            }
                            let target = tmppath.join(fname_to.file_name().ok_or_else(|| {
                                TaskError::UnsafePath {
                                    path: fname_to.clone(),
                                    reason: "conflict target has no name",
                                }
                            })?);
                            fs::rename(&fname_to, &target).map_err(|source| TaskError::Io {
                                path: fname_to.clone(),
                                source,
                            })?;
                            break;
                        }
                        if numbered {
                            // task.py:376 passes the base name as `repl` and
                            // the lambda as `string`; the real call raises.
                            return Err(TaskError::NameCollisionBug {
                                name: base.to_string_lossy().into_owned(),
                            });
                        }
                        let base = format!("{}(1)", base.to_string_lossy());
                        fname_to = PathBuf::from(format!("{base}{}", ext.to_string_lossy()));
                    }
                }
                match fs::rename(&fname_ori, &fname_to) {
                    Ok(()) => {
                        self.renamed_map.insert(
                            FidKey::Text(fid.to_string()),
                            fname_to
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                        );
                    }
                    Err(source) => {
                        errors.push(RenameError {
                            from: fname_ori
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            to: fname_to
                                .file_name()
                                .map(|name| name.to_string_lossy().into_owned())
                                .unwrap_or_default(),
                            message: source.to_string(),
                        });
                        break;
                    }
                }
                if self.config.rename_ori {
                    done_list.push(fname_to.clone());
                }
                break;
            }
            count += 1;
        }
        if count == self.meta_i64("total")? {
            let done = fpath.join(".xehdone");
            open_new_file(&done)?;
        }
        let _ = fs::remove_dir(&tmppath);
        Ok(errors)
    }
}

fn numbered_suffix(base: &Path) -> bool {
    static SUFFIX: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"\((\d+)\)$").expect("suffix regex"));
    let text = base.to_string_lossy();
    SUFFIX.is_match(&text)
}

/// Python `os.path.splitext` for POSIX and Windows separators.
pub fn python_splitext(name: &str) -> (String, String) {
    let sep = name
        .rfind(['/', '\\'])
        .map(|index| index as isize)
        .unwrap_or(-1);
    let filename_start = (sep + 1) as usize;
    let filename = &name[filename_start..];
    if filename.is_empty() {
        return (name.to_string(), String::new());
    }
    let dot = match filename.rfind('.') {
        Some(index) if index > 0 => index,
        _ => return (name.to_string(), String::new()),
    };
    let split = filename_start + dot;
    (name[..split].to_string(), name[split..].to_string())
}

fn python_splitext_path(path: &Path) -> (PathBuf, OsString) {
    let text = path.to_string_lossy();
    let (base, ext) = python_splitext(&text);
    (PathBuf::from(base), OsString::from(ext))
}
