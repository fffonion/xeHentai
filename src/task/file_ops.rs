//! File, naming and streaming work of `xeHentai/task.py`.
//!
//! Everything here is a `Task` method or a helper it calls directly. The
//! authorized download root is fixed when the task is created and all writes
//! stay below it; paths that would leave it (empty titles, `..` names, symlink
//! children) are refused instead of being followed like the reference did.
#![allow(dead_code)]

use std::ffi::{OsStr, OsString};
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use regex::Regex;

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
use cap_primitives::fs as capfs;
#[cfg(unix)]
use cap_primitives::fs::OpenOptionsExt as _;

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
    /// This task's read view of the application private-path policy. App-owned
    /// tasks share the App's live cell; a standalone task owns the fixed
    /// base-derived names.
    private: Arc<PrivatePaths>,
}

impl TaskFiles {
    /// A standalone task whose private-path policy is the fixed, NONEMPTY set
    /// the reference resolves against its configuration base. App-owned tasks
    /// are built with [`TaskFiles::with_private_paths`] and share the App cell.
    pub fn new(root: PathBuf, base: &Path) -> Arc<Self> {
        Self::with_private_paths(root, PrivatePaths::for_base(base))
    }

    /// Build a task file owner that reads through the application's live
    /// private-path cell.
    pub fn with_private_paths(root: PathBuf, private: Arc<PrivatePaths>) -> Arc<Self> {
        Arc::new(Self {
            root,
            f_lock: Arc::new(Mutex::new(())),
            cnt_lock: Mutex::new(()),
            private,
        })
    }

    /// Freeze one gallery read grant.
    ///
    /// `component` is the validated single gallery component and `path` its
    /// spelling under the fixed authorized root. Both directory observations
    /// are taken here, at the snapshot boundary, before any permit or heavy
    /// work.
    pub(super) fn grant(&self, component: &str, path: &Path) -> GalleryGrant {
        GalleryGrant::new(
            self.root.clone(),
            OsString::from(component),
            observe_directory(&self.root),
            observe_directory(path),
            PrivatePolicy::freeze(Arc::clone(&self.private)),
        )
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
        // The installer-owned update area is not business data: a gallery
        // title may not name it, live inside it, or cover it as an ancestor
        // (the `dir="."` install-root case). This is the reserved-root rule,
        // not a display-name filter, and it runs before any write.
        ensure_outside_installer_area(&path, &self.private.install_areas())?;
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
        // A direct file write may not land in or above the installer area even
        // when the caller reached the directory without `task_dir`.
        ensure_outside_installer_area(dir, &self.private.install_areas())?;
        // The gallery directory itself must not be an owned container: this
        // covers cfg.dir descendants that bypass `task_dir` and keeps every
        // direct file write inside the same lifetime-bound boundary.
        ensure_not_inside_detached(dir)?;
        let path = dir.join(name);
        ensure_outside_installer_area(&path, &self.private.install_areas())?;
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

/// Refuse a business path that overlaps the installer-owned area.
///
/// The comparison uses the authorization boundary path (longest existing
/// ancestor canonicalised, missing tail kept as names), so an alias through a
/// symlinked parent is judged by its real target and a dangling component
/// fails closed. Both directions are denied: anything INSIDE the area, and any
/// ancestor whose recursive deletion would cover it.
fn ensure_outside_installer_area(candidate: &Path, areas: &[PathBuf]) -> Result<(), TaskError> {
    if areas.is_empty() {
        return Ok(());
    }
    let candidate = boundary_path(candidate)?;
    for area in areas {
        let area = boundary_path(area)?;
        if candidate.starts_with(&area) || area.starts_with(&candidate) {
            return Err(TaskError::UnsafePath {
                path: candidate,
                reason: "path overlaps the installer-owned update area",
            });
        }
    }
    Ok(())
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

/// An identity snapshot of a directory: the `(device, inode)` pair the path
/// resolved to when the snapshot was taken on Unix, or the `(volume serial
/// number, file index)` pair on Windows. Compared against the actually opened
/// directory handle, so a parent swap between snapshot and open fails closed
/// instead of serving the replacement.
pub type DirIdentity = (u64, u64);

/// A frozen, non-opening observation of one directory path.
///
/// `Present` carries the exact identity the path had when it was observed,
/// `Missing` is a genuine `NotFound`, and `Unavailable` is every other
/// outcome: an existing non-directory (FIFO, socket, regular file, device), a
/// permission or probe error, or a target outside the confined backend
/// (Linux, macOS and Windows).
/// Only `Present` can authorize an open, and that open must still
/// match the recorded identity. A `Missing`/`Unavailable` observation can
/// never be promoted to `Present`, not even after a wait or a later
/// filesystem change inside the same request.
///
/// The observation itself is filesystem I/O. On Unix it is a name-based
/// `stat`, so it never opens the path (a FIFO cannot stall it). On Windows a
/// directory identity is only available from a handle, so one metadata-only
/// directory handle is opened and no entries or bytes are read. Either way it
/// is I/O with no universal latency bound (network filesystems, cold caches,
/// device servers); it is not a pure function and must not be described as
/// one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirectoryObservation {
    Present(DirIdentity),
    Missing,
    Unavailable(std::io::ErrorKind),
}

/// Observe `path` without opening it.
///
/// Only a real `NotFound` is `Missing`; an existing non-directory (including a
/// FIFO, socket or regular file) and every other probe error are
/// `Unavailable`. On targets outside the confined backend this is
/// `Unavailable(Unsupported)` BEFORE any filesystem operation: there is no
/// ambient fallback read.
pub fn observe_directory(path: &Path) -> DirectoryObservation {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                DirectoryObservation::Present((metadata.dev(), metadata.ino()))
            }
            Ok(_) => DirectoryObservation::Unavailable(std::io::ErrorKind::NotADirectory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DirectoryObservation::Missing
            }
            Err(error) => DirectoryObservation::Unavailable(error.kind()),
        }
    }
    #[cfg(windows)]
    {
        // A Windows directory identity is only available from a handle, so the
        // observation opens one metadata-only directory handle (no entries or
        // bytes are read). `fs::metadata` first keeps the existing
        // "existing non-directory" class instead of reporting an open error.
        match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => {
                match capfs::open_ambient_dir(path, cap_primitives::ambient_authority()) {
                    Ok(dir) => match capfs::Metadata::from_file(&dir) {
                        Ok(metadata) => match windows_file_id(&metadata) {
                            Ok(id) => DirectoryObservation::Present(id),
                            Err(kind) => DirectoryObservation::Unavailable(kind),
                        },
                        Err(error) => DirectoryObservation::Unavailable(error.kind()),
                    },
                    // Gone between the two probes: the snapshot grants nothing.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                        DirectoryObservation::Missing
                    }
                    Err(error) => DirectoryObservation::Unavailable(error.kind()),
                }
            }
            Ok(_) => DirectoryObservation::Unavailable(std::io::ErrorKind::NotADirectory),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                DirectoryObservation::Missing
            }
            Err(error) => DirectoryObservation::Unavailable(error.kind()),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = path;
        DirectoryObservation::Unavailable(std::io::ErrorKind::Unsupported)
    }
}

/// Typed denial for a path that was observed but cannot carry directory
/// authority for this request. Shared by the confined Linux/macOS backend and
/// the `Unsupported` surface of every other target so both report the same
/// denial class instead of a missing leaf.
fn unavailable_denial(path: &Path, kind: std::io::ErrorKind) -> TaskError {
    TaskError::UnsafePath {
        path: path.to_path_buf(),
        reason: unavailable_reason(kind),
    }
}

fn unavailable_reason(kind: std::io::ErrorKind) -> &'static str {
    use std::io::ErrorKind as Kind;
    match kind {
        Kind::NotADirectory => "observed path is not a directory",
        Kind::PermissionDenied => "observed path is not readable",
        Kind::Unsupported => "confined gallery opens are unsupported on this platform",
        _ => "observed path is not an available directory for this request",
    }
}

// ---------------------------------------------------------------------------
// Task-root read authority (M5 FD product integration).
//
// One coherent boundary for every production read consumer: loose gallery
// image, sibling-archive fallback, final archive stream, descriptor-relative
// archive enumeration and every archive entry. It replaces the ambient
// check-then-reopen and the canonicalized-payload-path alias proof with:
//
// 1. a mandatory App-owned live private-path policy plus the private names and
//    identities frozen into one read snapshot (see [`PrivatePaths`]);
// 2. the owning `TaskFiles` fixed download root and the validated single
//    gallery component as the only root provenance;
// 3. the frozen pre-permit directory observations as the only binding;
// 4. the accepted mature `cap-primitives` 4.0.3 resolver for the loose leaf
//    (including the reviewed `open_with_absolute_prefix` literal rebase on
//    Linux and macOS) and for descriptor-relative enumeration;
// 5. a single-component `openat` for archive entries, with the same-descriptor
//    type and private-identity verdict before any byte is copied.
// ---------------------------------------------------------------------------

/// `(device, inode)` identity. Always taken from a descriptor or a metadata
/// probe; never used as a containment proof on its own.
pub type FileId = (u64, u64);

fn dedupe_paths(paths: &mut Vec<PathBuf>) {
    let mut unique: Vec<PathBuf> = Vec::with_capacity(paths.len());
    for path in paths.drain(..) {
        if !unique.contains(&path) {
            unique.push(path);
        }
    }
    *paths = unique;
}

fn push_unique_id(ids: &mut Vec<FileId>, id: FileId) {
    if !ids.contains(&id) {
        ids.push(id);
    }
}

/// The identities one private spelling protects.
///
/// `target` is the object the private consumer actually opens:
/// `config::load` and `Logger::set_logfile` follow a legal private symlink, so
/// the target — not the link node — is the private object. `link` keeps the
/// link node's own identity as an extra (a descriptor can never be a symlink,
/// so it is never a substitute for the followed target). A dangling symlink
/// has no target (`None`): the link inode is retained as an extra but no
/// target identity is invented, so a known-absent CONSUMED referent denies
/// nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PrivateObject {
    target: Option<FileId>,
    link: Option<FileId>,
}

impl PrivateObject {
    fn covers(&self, id: FileId) -> bool {
        self.target == Some(id) || self.link == Some(id)
    }

    fn ids(&self) -> impl Iterator<Item = FileId> {
        [self.target, self.link].into_iter().flatten()
    }
}

/// Resolve one private spelling to the objects it protects.
///
/// Only genuine absence (the spelling itself does not exist) is `Ok(None)`.
/// A dangling symlink (the spelling exists, its referent does not) is
/// `Ok` with no target identity — only the link inode as an extra — so an
/// unused known-absent referent contributes nothing and never globally
/// disables the App. A link loop, a permission failure or any other probe
/// error is an error, so the policy fails closed instead of silently
/// forgetting a private object. No target identity is ever invented.
#[cfg(unix)]
fn observe_identity(path: &Path) -> Result<Option<PrivateObject>, std::io::ErrorKind> {
    use std::os::unix::fs::MetadataExt;
    let link = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.kind()),
    };
    let link_id = (link.dev(), link.ino());
    if !link.file_type().is_symlink() {
        return Ok(Some(PrivateObject {
            target: Some(link_id),
            link: None,
        }));
    }
    match fs::metadata(path) {
        Ok(metadata) => Ok(Some(PrivateObject {
            target: Some((metadata.dev(), metadata.ino())),
            link: Some(link_id),
        })),
        // The spelling exists but its referent is genuinely absent: no target
        // to protect, only the link inode as an extra. Anything else (loop,
        // permission, I/O) still fails closed via `Err`.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(PrivateObject {
            target: None,
            link: Some(link_id),
        })),
        Err(error) => Err(error.kind()),
    }
}

/// Resolve a private spelling through the captured parent directory on
/// Windows, where a metadata identity is only available from a handle. The
/// mature resolver's `stat` opens a metadata-only handle (`access_mode(0)`),
/// so no payload byte is read and the resolver still starts from the parent
/// descriptor rather than re-authorizing a pathname.
///
/// Name-based links on Windows are expanded by the resolver; the resulting
/// target identity is the same confinement, not the Linux same-descriptor
/// literal property.
#[cfg(windows)]
fn observe_identity(path: &Path) -> Result<Option<PrivateObject>, std::io::ErrorKind> {
    let link = match windows_stat_identity(path, capfs::FollowSymlinks::No) {
        Ok(metadata) => metadata,
        Err(std::io::ErrorKind::NotFound) => return Ok(None),
        Err(kind) => return Err(kind),
    };
    if !link.file_type().is_symlink() {
        return Ok(Some(PrivateObject {
            target: Some(windows_file_id(&link)?),
            link: None,
        }));
    }
    let link_id = windows_file_id(&link)?;
    match windows_stat_identity(path, capfs::FollowSymlinks::Yes) {
        Ok(target) => Ok(Some(PrivateObject {
            target: Some(windows_file_id(&target)?),
            link: Some(link_id),
        })),
        // The spelling exists but its referent is genuinely absent: no target
        // to protect, only the link node as an extra.
        Err(std::io::ErrorKind::NotFound) => Ok(Some(PrivateObject {
            target: None,
            link: Some(link_id),
        })),
        Err(kind) => Err(kind),
    }
}

#[cfg(windows)]
fn windows_stat_identity(
    path: &Path,
    follow: capfs::FollowSymlinks,
) -> Result<capfs::Metadata, std::io::ErrorKind> {
    let parent = path.parent().ok_or(std::io::ErrorKind::InvalidInput)?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let name = path.file_name().ok_or(std::io::ErrorKind::InvalidInput)?;
    let dir = capfs::open_ambient_dir(parent, cap_primitives::ambient_authority())
        .map_err(|error| error.kind())?;
    capfs::stat(&dir, Path::new(name), follow).map_err(|error| error.kind())
}

#[cfg(not(any(unix, windows)))]
fn observe_identity(_path: &Path) -> Result<Option<PrivateObject>, std::io::ErrorKind> {
    Err(std::io::ErrorKind::Unsupported)
}

/// The file the application's logger actually holds open, published into the
/// bounded App private-policy cell.
///
/// The held descriptor keeps receiving log records even after its pathname
/// moves, so the inode it points at is live private state that no current
/// spelling has to name. One bounded current value only: no history, no
/// descriptor registry and no logger lifecycle change.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActivePrivateLog {
    /// No log file is held (nothing to protect).
    None,
    /// The identity of the descriptor the logger holds right now.
    Held(FileId),
    /// The held file's metadata could not be read. The policy fails closed
    /// instead of inventing or forgetting an identity.
    Unobservable,
}

/// This application's current private persistence spellings, shared by every
/// task that lives in the same App.
///
/// The set is small and bounded by the fixed persistence names: the session
/// file, `config.rss`, the legacy `config.py`, the resolved `log_path` and the
/// legacy cookie file. `set_config` replaces it BEFORE the logger is reopened,
/// so a name that changes at runtime is protected from the moment it becomes
/// private, while an older snapshot keeps its own captured names. The single
/// [`ActivePrivateLog`] value covers the log file the logger holds open across
/// such a rename.
#[derive(Debug)]
pub struct PrivatePaths {
    spellings: RwLock<Vec<PathBuf>>,
    /// The installer-owned area(s) of the ACTUAL installation this App runs
    /// from. Business paths may never overlap them, in either direction.
    install_areas: RwLock<Vec<PathBuf>>,
    active_log: RwLock<ActivePrivateLog>,
}

impl PrivatePaths {
    /// Build the cell from a fixed, nonempty spelling set. Only the fixed
    /// App/base constructors below reach this: there is no generic empty-list
    /// production policy.
    fn from_fixed(mut spellings: Vec<PathBuf>, mut areas: Vec<PathBuf>) -> Arc<Self> {
        dedupe_paths(&mut spellings);
        dedupe_paths(&mut areas);
        Arc::new(Self {
            spellings: RwLock::new(spellings),
            install_areas: RwLock::new(areas),
            active_log: RwLock::new(ActivePrivateLog::None),
        })
    }

    /// The standalone (non-App) base policy: the fixed persistence names the
    /// reference resolves against its configuration base.
    pub fn for_base(base: &Path) -> Arc<Self> {
        Self::from_fixed(base_spellings(base), base_install_areas(base))
    }

    /// The App policy: the same fixed names plus the resolved `log_path` and
    /// the App's real cookie-file location. Built from the actual configuration
    /// inputs, never from an arbitrary caller-supplied list.
    pub fn for_app(base: &Path, filepath: &Path, log_path: &str) -> Arc<Self> {
        Self::from_fixed(
            app_spellings(base, filepath, log_path),
            app_install_areas(base, filepath),
        )
    }

    /// Replace the live set from the same fixed App inputs. Called before a
    /// logger reopen or any other private-file creation.
    pub fn replace_for_app(&self, base: &Path, filepath: &Path, log_path: &str) {
        let mut spellings = app_spellings(base, filepath, log_path);
        dedupe_paths(&mut spellings);
        let mut areas = app_install_areas(base, filepath);
        dedupe_paths(&mut areas);
        let mut current = self
            .spellings
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *current = spellings;
        let mut current_areas = self
            .install_areas
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *current_areas = areas;
    }

    /// The installer-owned areas of this installation. A business path that
    /// overlaps any of them is refused before any write or recursive delete.
    pub fn install_areas(&self) -> Vec<PathBuf> {
        self.install_areas
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    /// Publish the log file the logger actually holds. Called at App
    /// initialization and after every successful reopen, before the first
    /// private log write; the previous value stays published while the name
    /// moves and the reopen is still in flight.
    pub fn publish_active_log(&self, active: ActivePrivateLog) {
        let mut current = self
            .active_log
            .write()
            .unwrap_or_else(|error| error.into_inner());
        *current = active;
    }

    /// The single bounded current held-log state.
    pub fn active_log(&self) -> ActivePrivateLog {
        *self
            .active_log
            .read()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// A bounded clone of the current spellings. The lock is released before
    /// the caller performs any filesystem work.
    pub fn spellings(&self) -> Vec<PathBuf> {
        self.spellings
            .read()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

/// The five fixed base persistence spellings the reference resolves against
/// its configuration base.
fn base_spellings(base: &Path) -> Vec<PathBuf> {
    vec![
        base.join(crate::core::SESSION_FILE),
        base.join(crate::config::CONFIG_FILE),
        base.join(crate::config::LEGACY_CONFIG_FILE),
        // `Config::default().log_path` (`eh.log`), resolved against base.
        base.join("eh.log"),
        base.join(crate::core::LEGACY_COOKIE_FILE),
        // The installer's own area lives under the installation root; when the
        // configuration base IS that root (the ordinary `dir="."` case) it must
        // be private persistence, not gallery data.
        base.join(crate::updater::install::AREA_DIR),
    ]
}

fn base_install_areas(base: &Path) -> Vec<PathBuf> {
    vec![base.join(crate::updater::install::AREA_DIR)]
}

fn app_install_areas(base: &Path, filepath: &Path) -> Vec<PathBuf> {
    vec![
        base.join(crate::updater::install::AREA_DIR),
        filepath.join(crate::updater::install::AREA_DIR),
    ]
}

/// The same fixed names with the actual configured log path and the App's
/// cookie-file location.
fn app_spellings(base: &Path, filepath: &Path, log_path: &str) -> Vec<PathBuf> {
    let log_path = Path::new(log_path);
    let log_path = if log_path.is_absolute() {
        log_path.to_path_buf()
    } else {
        base.join(log_path)
    };
    vec![
        base.join(crate::core::SESSION_FILE),
        base.join(crate::config::CONFIG_FILE),
        base.join(crate::config::LEGACY_CONFIG_FILE),
        log_path,
        filepath.join(crate::core::LEGACY_COOKIE_FILE),
        filepath.join(crate::updater::install::AREA_DIR),
    ]
}

/// The private-path policy frozen into one read snapshot.
///
/// It keeps four things: the names captured at the snapshot boundary, the
/// private identities those names had at that moment, the log file the logger
/// held open at that moment, and a handle to the live current names plus the
/// currently held log file. A gate therefore denies an OLD private inode that
/// was hardlinked into the gallery after its private pathname was atomically
/// replaced (captured identity), the NEW inode that a captured or current
/// spelling now names (fresh resolution at the returned descriptor), and the
/// inode the live logger keeps writing even when no spelling names it any more.
///
/// Metadata capture is ordinary filesystem I/O, exactly like the `6a`
/// directory observations: it is not a pure or latency-free operation. Only a
/// genuine `NotFound` may be absent from the policy — a missing spelling
/// contributes nothing and a dangling spelling contributes only its link
/// inode, never a fabricated target; every other probe failure (loop,
/// permission, I/O) freezes a denial instead of weakening the set.
#[derive(Clone, Debug)]
pub struct PrivatePolicy {
    captured: Vec<PathBuf>,
    captured_ids: Vec<FileId>,
    captured_active_log: ActivePrivateLog,
    current: Arc<PrivatePaths>,
    frozen: Option<std::io::ErrorKind>,
}

impl PrivatePolicy {
    /// Freeze the current spellings, the identities they have right now and
    /// the log file the logger actually holds.
    pub fn freeze(current: Arc<PrivatePaths>) -> Self {
        let captured = current.spellings();
        let captured_active_log = current.active_log();
        let mut captured_ids = Vec::new();
        let mut frozen = None;
        for path in &captured {
            match observe_identity(path) {
                Ok(Some(object)) => {
                    for id in object.ids() {
                        push_unique_id(&mut captured_ids, id);
                    }
                }
                Ok(None) => {}
                Err(kind) => {
                    if frozen.is_none() {
                        frozen = Some(kind);
                    }
                }
            }
        }
        Self {
            captured,
            captured_ids,
            captured_active_log,
            current,
            frozen,
        }
    }

    /// Deny a descriptor identity that is private persistence.
    ///
    /// Runs at every returned-FD gate, after the descriptor is open and before
    /// any payload byte is read. The current-name/held-file clone happens under
    /// a short lock that is released BEFORE the fresh metadata probes.
    pub fn deny(&self, id: FileId, path: &Path) -> Result<(), TaskError> {
        if self.frozen.is_some() {
            return Err(TaskError::UnsafePath {
                path: path.to_path_buf(),
                reason: "private persistence identities could not be observed",
            });
        }
        match self.captured_active_log {
            ActivePrivateLog::Unobservable => {
                return Err(TaskError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "the held log file could not be identified",
                });
            }
            ActivePrivateLog::Held(held) if held == id => {
                return Err(TaskError::UnsafePath {
                    path: path.to_path_buf(),
                    reason: "target is the live log file",
                });
            }
            _ => {}
        }
        if self.captured_ids.contains(&id) {
            return Err(TaskError::UnsafePath {
                path: path.to_path_buf(),
                reason: "target is reserved persistence",
            });
        }
        let mut names = self.captured.clone();
        names.extend(self.current.spellings());
        dedupe_paths(&mut names);
        for name in &names {
            match observe_identity(name) {
                Ok(Some(found)) if found.covers(id) => {
                    return Err(TaskError::UnsafePath {
                        path: path.to_path_buf(),
                        reason: "target is reserved persistence",
                    });
                }
                Ok(_) => {}
                Err(_) => {
                    return Err(TaskError::UnsafePath {
                        path: path.to_path_buf(),
                        reason: "private persistence identities could not be observed",
                    });
                }
            }
        }
        match self.current.active_log() {
            ActivePrivateLog::Held(held) if held == id => Err(TaskError::UnsafePath {
                path: path.to_path_buf(),
                reason: "target is the live log file",
            }),
            ActivePrivateLog::Unobservable => Err(TaskError::UnsafePath {
                path: path.to_path_buf(),
                reason: "the held log file could not be identified",
            }),
            ActivePrivateLog::None | ActivePrivateLog::Held(_) => Ok(()),
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
#[derive(Clone, Debug)]
/// Frozen read authority for exactly one gallery.
///
/// Minted from the owning [`TaskFiles`] fixed authorized download root plus the
/// validated single gallery component, together with the mandatory pre-permit
/// directory observations. A `Missing` gallery grants no gallery descriptor and
/// no enumeration; an `Unavailable` one denies outright. Neither can be
/// promoted by a later filesystem change inside the same request.
pub struct GalleryGrant {
    root: PathBuf,
    gallery: OsString,
    root_obs: DirectoryObservation,
    gallery_obs: DirectoryObservation,
    private: PrivatePolicy,
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl GalleryGrant {
    /// Sealed to the owning task module: a grant is only minted by
    /// [`TaskFiles::grant`] from the fixed authorized root, the validated
    /// single gallery component and the frozen observations.
    pub(super) fn new(
        root: PathBuf,
        gallery: OsString,
        root_obs: DirectoryObservation,
        gallery_obs: DirectoryObservation,
        private: PrivatePolicy,
    ) -> Self {
        Self {
            root,
            gallery,
            root_obs,
            gallery_obs,
            private,
        }
    }

    /// The fixed authorized download root (the gallery's parent).
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The gallery directory spelled from the task root and component.
    pub fn gallery_path(&self) -> PathBuf {
        self.root.join(&self.gallery)
    }

    /// The task-minted sibling archive name, `<gallery>.zip`.
    pub fn sibling_name(&self) -> OsString {
        let mut name = self.gallery.clone();
        name.push(".zip");
        name
    }

    /// Open one validated leaf inside the gallery.
    ///
    /// `Ok(None)` is a genuine missing payload: the reference ZIP fallback may
    /// apply. A confinement denial is `Err` and never falls back.
    pub fn open_leaf(&self, leaf: &OsStr) -> Result<Option<File>, TaskError> {
        check_leaf(leaf)?;
        // Test-only deterministic window: the fixture change lands between the
        // snapshot observation and the descriptor capture, exactly where the
        // real race would be. Unix-only, a no-op unless armed, and compiled
        // out of production builds.
        #[cfg(all(test, unix))]
        run_preopen_seam(&self.gallery_path(), &leaf.to_string_lossy());
        let Some(rooted) = self.capture()? else {
            return Ok(None);
        };
        let Some(gallery) = rooted.gallery.as_ref() else {
            return Ok(None);
        };
        match capfs::open_with_absolute_prefix(
            gallery,
            Path::new(leaf),
            &leaf_options(),
            &rooted.gallery_spelling,
        ) {
            Ok(file) => Ok(Some(self.finish(file, &self.gallery_path().join(leaf))?)),
            // Genuine absence inside the gallery: an internal dangling alias or
            // a file that was never there. The reference ZIP fallback applies.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            // Everything else is a confinement/policy denial: outside literals,
            // magic links, non-regular descriptors. Never a fallback.
            Err(error) => Err(TaskError::UnsafePath {
                path: self.gallery_path().join(leaf),
                reason: leaf_denial_reason(&error),
            }),
        }
    }

    /// Open the task-minted sibling archive, `<gallery>.zip`, from the
    /// separately authorized gallery parent.
    ///
    /// Only the exact sibling name is ever opened, and only a `Present` parent
    /// observation grants it. A `Missing`/`Unavailable` gallery still allows the
    /// sibling (the archive is a sibling of the directory), but a parent that
    /// was missing or unavailable at snapshot time grants nothing.
    pub fn open_sibling_archive(&self) -> Result<Option<File>, TaskError> {
        let name = self.sibling_name();
        #[cfg(all(test, unix))]
        run_preopen_seam(&self.root, &name.to_string_lossy());
        let Some(rooted) = self.capture()? else {
            return Ok(None);
        };
        let path = self.root.join(&name);
        match capfs::open(&rooted.parent, Path::new(&name), &leaf_options()) {
            Ok(file) => Ok(Some(self.finish(file, &path)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(TaskError::UnsafePath {
                path,
                reason: leaf_denial_reason(&error),
            }),
        }
    }

    /// Capture the gallery directory for descriptor-relative enumeration.
    ///
    /// `Ok(None)` only when the gallery was genuinely absent at the snapshot
    /// boundary (or disappeared before the descriptor was taken); an
    /// `Unavailable` observation is a denial, never an empty directory.
    pub fn capture_gallery_dir(&self) -> Result<Option<CapturedGalleryDir>, TaskError> {
        let Some(rooted) = self.capture()? else {
            return Ok(None);
        };
        let Some(dir) = rooted.gallery else {
            return Ok(None);
        };
        Ok(Some(CapturedGalleryDir {
            dir,
            spelling: rooted.gallery_spelling,
            private: self.private.clone(),
        }))
    }

    /// Capture the trusted root descriptor and, when the gallery was `Present`,
    /// its child descriptor — both compared against the frozen observations.
    fn capture(&self) -> Result<Option<Rooted>, TaskError> {
        // The gallery component is validated again at the descriptor boundary:
        // even a grant constructed directly can never reach a
        // descriptor-relative open with `..`, a separator or a NUL byte.
        check_leaf(&self.gallery)?;
        let expected_root = match self.root_obs {
            DirectoryObservation::Present(identity) => identity,
            DirectoryObservation::Missing => return Ok(None),
            DirectoryObservation::Unavailable(kind) => {
                return Err(unavailable_denial(&self.root, kind));
            }
        };
        // Trusted-root canonical spelling: a binding/prefix LABEL only. No
        // payload target is ever canonicalized, and no containment verdict is
        // ever taken from this spelling.
        let spelling = match boundary_path(&self.root) {
            Ok(path) => path,
            Err(_) => {
                return Err(TaskError::UnsafePath {
                    path: self.root.clone(),
                    reason: "trusted download root cannot be resolved",
                });
            }
        };
        let parent = match capfs::open_ambient_dir(&spelling, cap_primitives::ambient_authority()) {
            Ok(dir) => dir,
            // The trusted root disappeared between snapshot and open: this is
            // the ordinary "gallery archived away" demotion, which grants
            // nothing beyond what a Missing observation grants.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(TaskError::UnsafePath {
                    path: self.root.clone(),
                    reason: root_denial_reason(&error),
                });
            }
        };
        let parent_id = descriptor_identity(&parent, &self.root)?;
        if parent_id != expected_root {
            return Err(TaskError::UnsafePath {
                path: self.root.clone(),
                reason: "trusted download root changed since the snapshot",
            });
        }
        let gallery_spelling = spelling.join(&self.gallery);
        let gallery = match self.gallery_obs {
            DirectoryObservation::Missing => None,
            DirectoryObservation::Unavailable(kind) => {
                return Err(unavailable_denial(&gallery_spelling, kind));
            }
            DirectoryObservation::Present(expected) => {
                match capfs::open_dir_nofollow(&parent, Path::new(&self.gallery)) {
                    Ok(dir) => {
                        if descriptor_identity(&dir, &gallery_spelling)? != expected {
                            return Err(TaskError::UnsafePath {
                                path: gallery_spelling,
                                reason: "gallery root changed since the snapshot",
                            });
                        }
                        Some(dir)
                    }
                    // Genuinely gone since the observation: the snapshot grants
                    // no gallery, but the sibling-archive parent grant stands.
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                    Err(error) => {
                        return Err(TaskError::UnsafePath {
                            path: gallery_spelling,
                            reason: capture_denial_reason(&error),
                        });
                    }
                }
            }
        };
        Ok(Some(Rooted {
            parent,
            gallery,
            gallery_spelling,
        }))
    }

    /// The returned-descriptor gate: type, then current private identity, then
    /// ordinary blocking semantics. Never a second path lookup.
    fn finish(&self, file: File, path: &Path) -> Result<File, TaskError> {
        let (id, is_regular) = descriptor_verdict(&file, path)?;
        if !is_regular {
            return Err(TaskError::UnsafePath {
                path: path.to_path_buf(),
                reason: "target is not a regular file",
            });
        }
        self.private.deny(id, path)?;
        clear_nonblock(file, path)
    }
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
/// The captured descriptors behind one [`GalleryGrant`].
struct Rooted {
    parent: File,
    gallery: Option<File>,
    gallery_spelling: PathBuf,
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
/// A captured gallery directory for descriptor-relative enumeration.
pub struct CapturedGalleryDir {
    dir: File,
    spelling: PathBuf,
    private: PrivatePolicy,
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
/// What one archive entry descriptor turned out to be.
pub enum OpenedEntry {
    Directory,
    File(File),
}

#[cfg(any(target_os = "linux", target_os = "macos", windows))]
impl CapturedGalleryDir {
    /// Entry names as raw `OsString` bytes, sorted bytewise. `.`/`..` are
    /// filtered by the resolver.
    pub fn names(&self) -> Result<Vec<std::ffi::OsString>, TaskError> {
        let entries =
            capfs::read_dir(&self.dir, Path::new(".")).map_err(|source| TaskError::Io {
                path: self.spelling.clone(),
                source,
            })?;
        let mut names = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|source| TaskError::Io {
                path: self.spelling.clone(),
                source,
            })?;
            names.push(entry.file_name());
        }
        names.sort_by(|left, right| left.as_encoded_bytes().cmp(right.as_encoded_bytes()));
        Ok(names)
    }
}

#[cfg(unix)]
impl CapturedGalleryDir {
    /// Open exactly one enumerated leaf relative to the captured descriptor.
    ///
    /// A symlink is an existing `UnsafePath` error (never skipped or followed),
    /// a FIFO/socket/device is denied from the returned descriptor without a
    /// payload read, and a directory is reported as such and never read.
    pub fn open_entry(&self, name: &OsStr) -> Result<OpenedEntry, TaskError> {
        check_leaf(name)?;
        let full = self.spelling.join(name);
        let fd = match rustix::fs::openat(
            &self.dir,
            name,
            rustix::fs::OFlags::RDONLY
                | rustix::fs::OFlags::NOFOLLOW
                | rustix::fs::OFlags::NONBLOCK
                | rustix::fs::OFlags::CLOEXEC,
            rustix::fs::Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(error) if error == rustix::io::Errno::LOOP => {
                return Err(TaskError::UnsafePath {
                    path: full,
                    reason: "refusing to archive a symlink entry",
                });
            }
            Err(error) => {
                return Err(TaskError::Io {
                    path: full,
                    source: std::io::Error::from_raw_os_error(error.raw_os_error()),
                });
            }
        };
        let stat = rustix::fs::fstat(&fd).map_err(|error| TaskError::Io {
            path: full.clone(),
            source: std::io::Error::from_raw_os_error(error.raw_os_error()),
        })?;
        // `st_dev` is 32-bit on macOS and 64-bit on Linux; the product
        // identity is the normalized 64-bit `(dev, ino)` pair on both.
        #[cfg(target_os = "linux")]
        let id = (stat.st_dev, stat.st_ino);
        #[cfg(target_os = "macos")]
        let id = (stat.st_dev as u64, stat.st_ino);
        match rustix::fs::FileType::from_raw_mode(stat.st_mode) {
            rustix::fs::FileType::Directory => Ok(OpenedEntry::Directory),
            rustix::fs::FileType::RegularFile => {
                self.private.deny(id, &full)?;
                rustix::fs::fcntl_setfl(&fd, rustix::fs::OFlags::empty()).map_err(|error| {
                    TaskError::Io {
                        path: full.clone(),
                        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
                    }
                })?;
                Ok(OpenedEntry::File(File::from(fd)))
            }
            _ => Err(TaskError::UnsafePath {
                path: full,
                reason: "archive entry is not a regular file",
            }),
        }
    }
}

#[cfg(windows)]
impl CapturedGalleryDir {
    /// Open exactly one enumerated leaf relative to the captured descriptor.
    ///
    /// Windows has no `openat`/`O_NOFOLLOW`: the same mature resolver's
    /// relative `open` with `FollowSymlinks::No` is used instead. It refuses
    /// reserved device names, and a symlink entry is denied from its own
    /// descriptor without ever being followed or read. The descriptor's type
    /// and private identity are re-checked from that handle before any byte is
    /// returned.
    pub fn open_entry(&self, name: &OsStr) -> Result<OpenedEntry, TaskError> {
        check_leaf(name)?;
        let full = self.spelling.join(name);
        let mut options = capfs::OpenOptions::new();
        options
            .read(true)
            ._cap_fs_ext_follow(capfs::FollowSymlinks::No);
        let file = match capfs::open(&self.dir, Path::new(name), &options) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(TaskError::Io {
                    path: full,
                    source: error,
                });
            }
            Err(file_error) => {
                // A directory cannot be opened with a file-only access mode on
                // Windows. Probe it with the same mature resolver's directory
                // open, still relative to the captured handle and still
                // no-follow; anything that is not a directory keeps the
                // original error.
                return match capfs::open_dir_nofollow(&self.dir, Path::new(name)) {
                    Ok(dir) => {
                        let metadata = windows_descriptor_metadata(&dir, &full)?;
                        if metadata.file_type().is_symlink() {
                            return Err(TaskError::UnsafePath {
                                path: full,
                                reason: "refusing to archive a symlink entry",
                            });
                        }
                        Ok(OpenedEntry::Directory)
                    }
                    Err(_) => Err(TaskError::Io {
                        path: full,
                        source: file_error,
                    }),
                };
            }
        };
        let metadata = windows_descriptor_metadata(&file, &full)?;
        let file_type = metadata.file_type();
        if file_type.is_symlink() {
            return Err(TaskError::UnsafePath {
                path: full,
                reason: "refusing to archive a symlink entry",
            });
        }
        if file_type.is_dir() {
            // A directory reached through a leaf open has no payload bytes;
            // the caller reports it as a directory and never reads it.
            return Ok(OpenedEntry::Directory);
        }
        if !file_type.is_file() {
            return Err(TaskError::UnsafePath {
                path: full,
                reason: "archive entry is not a regular file",
            });
        }
        self.private
            .deny(windows_identity_or_io(&metadata, &full)?, &full)?;
        Ok(OpenedEntry::File(file))
    }
}

/// A validated single path component as an `OsStr`, taken from the task record
/// or from a descriptor-relative enumeration. Never a multi-component path.
fn check_leaf(leaf: &OsStr) -> Result<(), TaskError> {
    let bytes = leaf.as_encoded_bytes();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
        || bytes.contains(&0)
        || (cfg!(windows) && bytes.contains(&b'\\'))
    {
        return Err(TaskError::UnsafePath {
            path: PathBuf::from(leaf),
            reason: "not a single normal path component",
        });
    }
    Ok(())
}

/// Read-only open options for every payload descriptor.
///
/// On Unix `O_NONBLOCK` only exists to keep a FIFO from stalling the open; it
/// is cleared from the returned descriptor after the regular-file and
/// private-identity verdicts. Windows has no FIFO-at-path semantics and its
/// `OpenOptionsExt` has no non-blocking flag, so the leaf is simply read-only
/// and there is nothing to clear.
#[cfg(unix)]
fn leaf_options() -> capfs::OpenOptions {
    let mut options = capfs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    options
}

#[cfg(windows)]
fn leaf_options() -> capfs::OpenOptions {
    let mut options = capfs::OpenOptions::new();
    options.read(true);
    options
}

/// Identity and regularity of an opened descriptor, taken from that descriptor.
#[cfg(unix)]
fn descriptor_verdict(file: &File, path: &Path) -> Result<(FileId, bool), TaskError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|source| TaskError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let file_type = metadata.file_type();
    Ok((
        (metadata.dev(), metadata.ino()),
        !file_type.is_symlink() && file_type.is_file(),
    ))
}

#[cfg(windows)]
fn descriptor_verdict(file: &File, path: &Path) -> Result<(FileId, bool), TaskError> {
    let metadata = windows_descriptor_metadata(file, path)?;
    let file_type = metadata.file_type();
    Ok((
        windows_identity_or_io(&metadata, path)?,
        !file_type.is_symlink() && file_type.is_file(),
    ))
}

#[cfg(unix)]
fn descriptor_identity(file: &File, path: &Path) -> Result<FileId, TaskError> {
    use std::os::unix::fs::MetadataExt;
    let metadata = file.metadata().map_err(|source| TaskError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    Ok((metadata.dev(), metadata.ino()))
}

#[cfg(windows)]
fn descriptor_identity(file: &File, path: &Path) -> Result<FileId, TaskError> {
    let metadata = windows_descriptor_metadata(file, path)?;
    windows_identity_or_io(&metadata, path)
}

#[cfg(windows)]
fn windows_descriptor_metadata(file: &File, path: &Path) -> Result<capfs::Metadata, TaskError> {
    capfs::Metadata::from_file(file).map_err(|source| TaskError::Io {
        path: path.to_path_buf(),
        source,
    })
}

/// The product `FileId` on Windows: `(volume serial number, file index)`.
///
/// The pair comes from the existing pinned `cap_primitives` handle metadata
/// (`Metadata::from_file` plus the doc-hidden `_WindowsByHandle` accessors),
/// which fills both halves on stable via the mature `winx` file information.
/// A missing half is an unobservable identity, never an invented zero.
#[cfg(windows)]
fn windows_file_id(metadata: &capfs::Metadata) -> Result<FileId, std::io::ErrorKind> {
    use cap_primitives::fs::_WindowsByHandle as _;
    match (metadata.volume_serial_number(), metadata.file_index()) {
        (Some(volume), Some(index)) => Ok((u64::from(volume), index)),
        _ => Err(std::io::ErrorKind::Unsupported),
    }
}

#[cfg(windows)]
fn windows_identity_or_io(metadata: &capfs::Metadata, path: &Path) -> Result<FileId, TaskError> {
    windows_file_id(metadata).map_err(|kind| TaskError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::new(kind, "the Windows file identity is unavailable"),
    })
}

/// Clear `O_NONBLOCK` (set only to avoid hangs while opening) so the
/// returned file has ordinary blocking semantics for the streaming reader.
#[cfg(unix)]
fn clear_nonblock(file: File, path: &Path) -> Result<File, TaskError> {
    rustix::fs::fcntl_setfl(&file, rustix::fs::OFlags::empty()).map_err(|error| TaskError::Io {
        path: path.to_path_buf(),
        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
    })?;
    Ok(file)
}

/// Windows has no `O_NONBLOCK` to clear: the leaf was opened for ordinary
/// blocking reads and keeps exactly those semantics.
#[cfg(windows)]
fn clear_nonblock(file: File, _path: &Path) -> Result<File, TaskError> {
    Ok(file)
}

#[cfg(unix)]
/// Diagnosis for a denied gallery leaf. The variant is a label; the contract is
/// "denied, no bytes, no fallback".
fn leaf_denial_reason(error: &std::io::Error) -> &'static str {
    match error.raw_os_error() {
        Some(code) if code == rustix_errno(rustix::io::Errno::LOOP) => {
            "target is a symlink loop or a magic link"
        }
        Some(code) if code == rustix_errno(rustix::io::Errno::ISDIR) => {
            "gallery target is not a regular file"
        }
        Some(code) if code == rustix_errno(rustix::io::Errno::XDEV) => {
            "link target leaves the gallery root"
        }
        Some(code) if code == rustix_errno(rustix::io::Errno::PERM) => {
            "link target leaves the gallery root"
        }
        _ => "gallery target is not available to this request",
    }
}

#[cfg(windows)]
/// Windows diagnoses by `ErrorKind`: the Windows error codes have no stable
/// `rustix` mapping. The label is still only a diagnosis; every arm denies.
fn leaf_denial_reason(error: &std::io::Error) -> &'static str {
    match error.kind() {
        std::io::ErrorKind::IsADirectory | std::io::ErrorKind::NotADirectory => {
            "gallery target is not a regular file"
        }
        _ => "gallery target is not available to this request",
    }
}

#[cfg(unix)]
fn root_denial_reason(error: &std::io::Error) -> &'static str {
    match error.raw_os_error() {
        Some(code) if code == rustix_errno(rustix::io::Errno::LOOP) => {
            "trusted download root is a symlink loop or a magic link"
        }
        _ => "trusted download root is not an available directory",
    }
}

#[cfg(windows)]
fn root_denial_reason(_error: &std::io::Error) -> &'static str {
    "trusted download root is not an available directory"
}

#[cfg(unix)]
fn capture_denial_reason(error: &std::io::Error) -> &'static str {
    match error.raw_os_error() {
        Some(code) if code == rustix_errno(rustix::io::Errno::NOTDIR) => {
            "gallery root is not a directory"
        }
        Some(code) if code == rustix_errno(rustix::io::Errno::LOOP) => "gallery root is a symlink",
        _ => "gallery root is not an available directory for this request",
    }
}

#[cfg(windows)]
fn capture_denial_reason(_error: &std::io::Error) -> &'static str {
    "gallery root is not an available directory for this request"
}

#[cfg(unix)]
fn rustix_errno(errno: rustix::io::Errno) -> i32 {
    errno.raw_os_error()
}

// ---------------------------------------------------------------------------
// Non-Linux/macOS surface: every payload read is an explicit `Unsupported`
// denial BEFORE any ambient open. There is no portable fallback, no ambient
// re-open and no forged cross-platform PASS. Targets other than
// Linux/macOS/Windows take this surface, because the accepted resolver's
// absolute-prefix entry only covers those three hosts.
// ---------------------------------------------------------------------------

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[allow(dead_code)]
#[derive(Clone, Debug)]
pub struct GalleryGrant {
    root: PathBuf,
    gallery: OsString,
    root_obs: DirectoryObservation,
    gallery_obs: DirectoryObservation,
    private: PrivatePolicy,
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl GalleryGrant {
    pub(super) fn new(
        root: PathBuf,
        gallery: OsString,
        root_obs: DirectoryObservation,
        gallery_obs: DirectoryObservation,
        private: PrivatePolicy,
    ) -> Self {
        Self {
            root,
            gallery,
            root_obs,
            gallery_obs,
            private,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn gallery_path(&self) -> PathBuf {
        self.root.join(&self.gallery)
    }

    pub fn sibling_name(&self) -> OsString {
        let mut name = self.gallery.clone();
        name.push(".zip");
        name
    }

    pub fn open_leaf(&self, leaf: &OsStr) -> Result<Option<File>, TaskError> {
        check_leaf(leaf)?;
        Err(unavailable_denial(
            &self.gallery_path(),
            std::io::ErrorKind::Unsupported,
        ))
    }

    pub fn open_sibling_archive(&self) -> Result<Option<File>, TaskError> {
        Err(unavailable_denial(
            &self.root,
            std::io::ErrorKind::Unsupported,
        ))
    }

    pub fn capture_gallery_dir(&self) -> Result<Option<CapturedGalleryDir>, TaskError> {
        Err(unavailable_denial(
            &self.gallery_path(),
            std::io::ErrorKind::Unsupported,
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[allow(dead_code)]
pub struct CapturedGalleryDir {
    dir: File,
    spelling: PathBuf,
    private: PrivatePolicy,
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
#[allow(dead_code)]
pub enum OpenedEntry {
    Directory,
    File(File),
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
impl CapturedGalleryDir {
    pub fn names(&self) -> Result<Vec<OsString>, TaskError> {
        Err(unavailable_denial(
            &self.spelling,
            std::io::ErrorKind::Unsupported,
        ))
    }

    pub fn open_entry(&self, _name: &OsStr) -> Result<OpenedEntry, TaskError> {
        Err(unavailable_denial(
            &self.spelling,
            std::io::ErrorKind::Unsupported,
        ))
    }
}

// ---------------------------------------------------------------------------
// TEST-ONLY deterministic barrier between precheck and open.
// ---------------------------------------------------------------------------

/// TEST-ONLY swap performed on the calling thread between the link precheck
/// and the open, so a regular inside-gallery file becomes an outside or
/// reserved symlink (or a swapped parent) exactly in the race window. Armed
/// once per `(root, name)` open attempt; a no-op unless armed. Unix-only:
/// every fixture is a real symlink, which this seam must actually create.
/// Production builds contain no hook at all.
#[cfg(all(test, unix))]
#[derive(Debug)]
pub(crate) enum PreopenSwap {
    /// Replace `root/name` with a symlink to `target`.
    LeafLink(PathBuf),
    /// Rename `root` aside and replace it with a symlink to `target`.
    RootLink(PathBuf),
}

#[cfg(all(test, unix))]
static PREOPEN_SEAMS: Mutex<Vec<(String, String, PreopenSwap)>> = Mutex::new(Vec::new());

#[cfg(all(test, unix))]
pub(crate) fn arm_preopen_swap(root: &Path, name: &str, swap: PreopenSwap) {
    PREOPEN_SEAMS.lock().unwrap().push((
        root.to_string_lossy().into_owned(),
        name.to_string(),
        swap,
    ));
}

#[cfg(all(test, unix))]
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
#[cfg(all(test, unix))]
pub(crate) fn run_preopen_seam(root: &Path, name: &str) {
    match take_preopen_swap(root, name) {
        None => {}
        Some(PreopenSwap::LeafLink(target)) => {
            let path = root.join(name);
            let _ = fs::remove_file(&path);
            let _ = std::os::unix::fs::symlink(&target, &path);
        }
        Some(PreopenSwap::RootLink(target)) => {
            let backup = root.with_extension("seambak");
            let _ = fs::remove_dir_all(&backup);
            if fs::rename(root, &backup).is_ok() {
                let _ = std::os::unix::fs::symlink(&target, root);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// TEST-ONLY one-shot fixture change inside the production snapshot window.
// ---------------------------------------------------------------------------

/// TEST-ONLY swap performed on the calling thread between the task directory
/// check (`TaskFiles::task_dir`) and the snapshot's directory observation,
/// i.e. exactly the window in which a checked gallery directory can be
/// replaced by a FIFO. Armed once per root path; a no-op unless armed.
/// Production builds contain no hook.
#[cfg(test)]
#[derive(Debug)]
pub(crate) enum ObservationSwap {
    /// Rename the gallery directory aside and put a FIFO at the same path.
    RootFifo,
}

#[cfg(test)]
static OBSERVATION_SEAMS: Mutex<Vec<(String, ObservationSwap)>> = Mutex::new(Vec::new());

#[cfg(test)]
pub(crate) fn arm_observation_swap(root: &Path, swap: ObservationSwap) {
    OBSERVATION_SEAMS
        .lock()
        .unwrap()
        .push((root.to_string_lossy().into_owned(), swap));
}

/// Apply the armed fixture change (if any) for this exact root path.
#[cfg(test)]
pub(crate) fn run_observation_swap(root: &Path) {
    let key = root.to_string_lossy().into_owned();
    let swap = {
        let mut seams = OBSERVATION_SEAMS.lock().unwrap();
        seams
            .iter()
            .position(|(scope, _)| *scope == key)
            .map(|index| seams.remove(index).1)
    };
    match swap {
        None => {}
        Some(ObservationSwap::RootFifo) => {
            let backup = root.with_extension("obsbak");
            let _ = fs::remove_dir_all(&backup);
            if fs::rename(root, &backup).is_ok() {
                #[cfg(all(unix, not(target_vendor = "apple")))]
                rustix::fs::mkfifoat(
                    rustix::fs::CWD,
                    root,
                    rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
                )
                .expect("observation seam FIFO");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An `Unavailable` observation must deny every entry point instead of
    /// opening, enumerating or granting the sibling archive. The grant can only
    /// be built from this module, because its mint is sealed to the task owner.
    #[test]
    fn unavailable_observations_deny_every_entry_point() {
        let root = PathBuf::from("/definitely/absent-download-root");
        let grant = GalleryGrant::new(
            root.clone(),
            OsString::from("gallery-title"),
            DirectoryObservation::Unavailable(std::io::ErrorKind::Unsupported),
            DirectoryObservation::Unavailable(std::io::ErrorKind::Unsupported),
            PrivatePolicy::freeze(PrivatePaths::for_base(&root)),
        );
        let error = grant
            .open_leaf(OsStr::new("1.jpg"))
            .expect_err("an Unavailable observation must not open anything");
        assert!(
            matches!(error, TaskError::UnsafePath { .. }),
            "an Unavailable observation must be a typed denial, got {error:?}"
        );
        assert!(
            grant.open_sibling_archive().is_err(),
            "an Unavailable root observation must not grant the sibling archive"
        );
        assert!(
            grant.capture_gallery_dir().is_err(),
            "an Unavailable gallery observation must not enumerate anything"
        );
    }

    /// The only production policy constructors are the fixed, NONEMPTY
    /// App/base sets; there is no generic empty-list creation or replacement.
    #[test]
    fn fixed_policies_are_nonempty_and_start_without_a_held_log() {
        let base = PathBuf::from("/policy-base");
        let base_policy = PrivatePaths::for_base(&base);
        // Five user-state names plus the installer-owned area under the base.
        assert_eq!(base_policy.spellings().len(), 6);
        assert_eq!(base_policy.active_log(), ActivePrivateLog::None);
        assert_eq!(
            base_policy.install_areas(),
            vec![base.join(crate::updater::install::AREA_DIR)]
        );

        let app_policy = PrivatePaths::for_app(&base, Path::new("/policy-app"), "logs/live.log");
        assert_eq!(app_policy.spellings().len(), 6);
        assert!(app_policy.spellings().contains(&base.join("logs/live.log")));
        app_policy.replace_for_app(&base, Path::new("/policy-app"), "/absolute/live.log");
        assert_eq!(app_policy.spellings().len(), 6);
        assert!(
            app_policy
                .spellings()
                .contains(&PathBuf::from("/absolute/live.log"))
        );
    }
}
