//! The installer-owned update area, activation and rollback.
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
//!   lock                               per-install cross-process transaction lock
//!   pending/src.zip                    one atomically published envelope
//!   staging/<random>/                  complete candidate tree, private
//!   preflight/                         private state for the candidate run
//!   backup/                            the single previous usable release
//!   state.json                         confirmed active + previous records (atomic)
//!   activation.json                    ONE provisional activation/recovery record
//! ```
//!
//! Activation order: materialize the signed entries into a fresh staging tree,
//! preflight the real candidate with the same host's minimal private update
//! mode, persist ONE provisional activation record (complete previous snapshot)
//! BEFORE any live mutation, move the previous files aside, install the new
//! ones, and (for a native package) replace the executable and re-execute it.
//! The candidate stays provisional until the real Config/App/session and the
//! required startup modules have initialized in the new image; only then is the
//! active record published. Any failure restores the previous files and record.
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
pub const ACTIVATION_FILE: &str = "activation.json";
pub const LOCK_FILE: &str = "lock";
pub const PENDING_DIR: &str = "pending";
pub const STAGING_DIR: &str = "staging";
pub const BACKUP_DIR: &str = "backup";
pub const PREFLIGHT_DIR: &str = "preflight";
pub const STATE_VERSION: i64 = 1;
pub const ACTIVATION_VERSION: i64 = 1;
/// Environment spelling used ONLY by the private native-installer exec path to
/// hand the owned lock descriptor to the replacement image.
pub const LOCK_HANDOFF_ENV: &str = "XEHENTAI_UPDATE_LOCK_FD";
/// The candidate preflight is a real process; this bound only stops a wedged
/// child, it is not a policy timeout.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(180);

/// One applied release identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Record {
    pub update_id: String,
    pub build_id: String,
    pub native: bool,
    pub applied_at: String,
}

/// The single active record plus at most one previous usable release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Records {
    pub v: i64,
    pub active: Option<Record>,
    pub previous: Option<Record>,
}

impl Default for Records {
    fn default() -> Self {
        Self {
            v: STATE_VERSION,
            active: None,
            previous: None,
        }
    }
}

/// The ONE provisional activation record.
///
/// It is written atomically BEFORE the first live resource/native mutation and
/// carries everything recovery needs: the candidate identity, the complete
/// previous record, and every install-relative file this activation may have
/// moved aside. `state.json` is not touched until the real new image has
/// initialized, so a crash at any point leaves a recoverable provisional
/// record rather than a half-confirmed release.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Activation {
    pub v: i64,
    pub candidate: Record,
    pub previous: Option<Record>,
    /// Install-relative files moved into `backup/` by this activation.
    pub paths: Vec<String>,
    /// Install-relative candidate resources that did NOT exist on the live
    /// tree before this activation. A rollback removes them again; every
    /// other path is restored from `backup/`. An older record without this
    /// field simply has no candidate-new files.
    #[serde(default)]
    pub created: Vec<String>,
    /// Install-relative path of the executable this activation replaced, so a
    /// recovery writes the previous image back to the KNOWN installation path
    /// instead of guessing from the current process.
    #[serde(default)]
    pub executable: Option<String>,
    pub native: bool,
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

    fn activation_path(&self) -> PathBuf {
        self.area().join(ACTIVATION_FILE)
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

    fn backup_root(&self) -> PathBuf {
        self.area().join(BACKUP_DIR)
    }

    /// True while an activation has not been confirmed by the real new image.
    ///
    /// Fallible: only a genuine absence (`NotFound`) means "no activation".
    /// Any other metadata error, or a marker that is not a real
    /// installer-owned regular file, is untrusted recovery state and must
    /// stop the startup before the App, never read as absent.
    pub fn activation_present(&self) -> Result<bool, UpdateError> {
        let path = self.activation_path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(UpdateError::io(&path, &error)),
        };
        verify_activation_leaf(&path, &metadata)?;
        Ok(true)
    }

    /// The provisional record, if one is present.
    pub fn activation(&self) -> Result<Option<Activation>, UpdateError> {
        let path = self.activation_path();
        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(UpdateError::io(&path, &error)),
        };
        // The marker is inspected before it is read: a symlink could point
        // anywhere outside the private area, so it is rejected here and the
        // read itself refuses to follow a final symlink.
        verify_activation_leaf(&path, &metadata)?;
        let bytes = read_private_leaf(&path)?;
        let activation: Activation = serde_json::from_slice(&bytes)
            .map_err(|error| UpdateError::install(format!("activation record: {error}")))?;
        if activation.v != ACTIVATION_VERSION {
            return Err(UpdateError::install(format!(
                "activation record version {} is not {ACTIVATION_VERSION}",
                activation.v
            )));
        }
        Ok(Some(activation))
    }

    fn write_activation(&self, activation: &Activation) -> Result<(), UpdateError> {
        let area = self.area();
        ensure_private_dir(&area)?;
        let bytes = serde_json::to_vec(activation)
            .map_err(|error| UpdateError::install(format!("activation record: {error}")))?;
        write_atomic(&area, &self.activation_path(), &bytes)
    }

    fn clear_activation(&self) -> Result<(), UpdateError> {
        let path = self.activation_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(UpdateError::io(&path, &error)),
        }
    }

    /// Verify an EXISTING update area before it is trusted for a read,
    /// recovery or write. A missing area is not an error here.
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

    /// The update id the running build was activated from, if any. This is the
    /// only source of the same-id decision; it is never inferred from files.
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

/// An activation marker must be a real installer-owned regular file, for the
/// same reason as a pending leaf: a symlink could point anywhere, and a
/// foreign-owned or non-regular marker is untrusted recovery state.
fn verify_activation_leaf(path: &Path, metadata: &std::fs::Metadata) -> Result<(), UpdateError> {
    verify_owned_regular_leaf(path, metadata, "the activation record")
}

/// The previous-executable backup a native rollback restores must be a real
/// installer-owned regular file. Resource-only activations never consult it.
fn verify_backup_executable(path: &Path, metadata: &std::fs::Metadata) -> Result<(), UpdateError> {
    verify_owned_regular_leaf(path, metadata, "the previous executable backup")
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

/// One held cross-process transaction lock for this installation.
///
/// The lock lives on an ordinary file in the private update area and covers
/// the startup read/apply/discard/recovery path through real App confirmation,
/// the final pending publication, and any rollback. It is a real filesystem
/// lock, never an in-process mutex: two xeHentai processes in one installation
/// serialize here.
pub struct InstallLock {
    file: std::fs::File,
}

impl InstallLock {
    /// Take the transaction lock without waiting. A concurrent owner yields a
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
        // open file DESCRIPTION so it survives the real exec and is shared by
        // the daemon child after fork. Closing the parent's descriptor must not
        // release the child's lock (a POSIX F_SETLK lock would be dropped by
        // any close in the owning process, and is not inherited across fork).
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

    /// Adopt the owned descriptor the private native installer handed across
    /// the real exec. The descriptor is validated against the real lock file
    /// (same device/inode, regular file), then close-on-exec is restored and
    /// the private environment spelling is cleared so ordinary children never
    /// inherit it.
    #[cfg(unix)]
    pub fn adopt_handoff(installation: &Installation) -> Result<Option<Self>, UpdateError> {
        let Some(text) = std::env::var_os(LOCK_HANDOFF_ENV) else {
            return Ok(None);
        };
        // Clear the handoff spelling before anything else can spawn.
        unsafe { std::env::remove_var(LOCK_HANDOFF_ENV) };
        let fd: i32 = text
            .to_string_lossy()
            .parse()
            .map_err(|_| UpdateError::install("invalid inherited update lock descriptor"))?;
        use std::os::fd::{BorrowedFd, FromRawFd, RawFd};
        let borrowed: BorrowedFd<'_> = unsafe { BorrowedFd::borrow_raw(fd as RawFd) };
        let stat = nix::sys::stat::fstat(borrowed)
            .map_err(|error| UpdateError::install(format!("inherited lock descriptor: {error}")))?;
        let path = installation.lock_path();
        let metadata = std::fs::metadata(&path).map_err(|error| UpdateError::io(&path, &error))?;
        use std::os::unix::fs::MetadataExt;
        if !metadata.is_file()
            || metadata.dev() != stat.st_dev as u64
            || metadata.ino() != stat.st_ino as u64
        {
            return Err(UpdateError::install(
                "inherited update lock descriptor does not match this installation's lock file",
            ));
        }
        set_cloexec(borrowed, true)?;
        let file = unsafe { std::fs::File::from_raw_fd(fd as RawFd) };
        Ok(Some(Self { file }))
    }

    #[cfg(not(unix))]
    pub fn adopt_handoff(_installation: &Installation) -> Result<Option<Self>, UpdateError> {
        Ok(None)
    }

    /// Make this exact owned descriptor survive the private exec and publish
    /// its number for the replacement image.
    #[cfg(unix)]
    fn prepare_exec(&self) -> Result<(), UpdateError> {
        use std::os::fd::AsFd;
        set_cloexec(self.file.as_fd(), false)?;
        let fd = {
            use std::os::fd::AsRawFd;
            self.file.as_raw_fd()
        };
        unsafe { std::env::set_var(LOCK_HANDOFF_ENV, fd.to_string()) };
        Ok(())
    }

    #[cfg(not(unix))]
    fn prepare_exec(&self) -> Result<(), UpdateError> {
        Ok(())
    }

    /// Test-only: the owned descriptor number for the real fork handoff proof.
    /// The caller is Unix-only, so the helper must not reference Unix-only
    /// descriptor APIs on a Windows test build.
    #[cfg(all(test, unix))]
    pub(crate) fn raw_fd_for_test(&self) -> i32 {
        use std::os::fd::AsRawFd;
        self.file.as_raw_fd()
    }
}

#[cfg(unix)]
fn set_cloexec<Fd: std::os::fd::AsFd>(fd: Fd, close: bool) -> Result<(), UpdateError> {
    use nix::fcntl::{FcntlArg, FdFlag, fcntl};
    let flags = if close {
        FdFlag::FD_CLOEXEC
    } else {
        FdFlag::empty()
    };
    fcntl(fd, FcntlArg::F_SETFD(flags))
        .map(|_| ())
        .map_err(|error| UpdateError::install(format!("update lock descriptor flags: {error}")))
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
/// hard rejection, because a shared-writable area could replace the previous
/// executable or rollback files.
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

/// Test-only hooks for the deterministic pre-commit barrier.
#[cfg(test)]
pub mod test_hooks {
    #![allow(dead_code)]
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    pub type Hook = Box<dyn Fn(&Path) + Send + Sync>;

    static HOOK: OnceLock<Mutex<Option<Hook>>> = OnceLock::new();
    static ROLLBACK_HOOK: OnceLock<Mutex<Option<Hook>>> = OnceLock::new();
    pub type JoinHook = Box<dyn Fn() + Send + Sync>;
    static JOIN_HOOK: OnceLock<Mutex<Option<JoinHook>>> = OnceLock::new();
    static EXEC_FAULT: AtomicBool = AtomicBool::new(false);

    fn cell() -> &'static Mutex<Option<Hook>> {
        HOOK.get_or_init(|| Mutex::new(None))
    }

    fn rollback_cell() -> &'static Mutex<Option<Hook>> {
        ROLLBACK_HOOK.get_or_init(|| Mutex::new(None))
    }

    pub fn set(hook: Option<Hook>) {
        *cell().lock().unwrap_or_else(|error| error.into_inner()) = hook;
    }

    pub fn set_rollback(hook: Option<Hook>) {
        *rollback_cell()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = hook;
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

    /// Test-only: make the next real exec fail with `EACCES` by removing the
    /// execute bits from the installed image immediately before `execv`.
    pub fn set_exec_permission_fault(fault: bool) {
        EXEC_FAULT.store(fault, Ordering::SeqCst);
    }

    pub fn exec_permission_fault() -> bool {
        EXEC_FAULT.load(Ordering::SeqCst)
    }

    pub fn publish_boundary(path: &Path) {
        let guard = cell().lock().unwrap_or_else(|error| error.into_inner());
        if let Some(hook) = guard.as_ref() {
            hook(path);
        }
    }

    /// Test-only barrier at the START of a real rollback, after the transaction
    /// lock was taken and before any file is restored.
    pub fn rollback_boundary(path: &Path) {
        let guard = rollback_cell()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
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

/// One installed but not yet confirmed release.
pub struct Committed {
    pub record: Record,
    /// True when the caller must re-execute the installed image.
    pub reexec: bool,
}

/// Why a candidate did not become the live release. `rolled_back` means live
/// files had already moved and were restored, so the caller logs a rollback
/// rather than a plain rejection.
pub struct ActivationFailure {
    pub error: UpdateError,
    pub rolled_back: bool,
    /// The previous release could NOT be completely restored (or its recovery
    /// record could not be retired). The caller must keep the recovery state
    /// and stop instead of running the business App on a mixed tree.
    pub recovery_failed: bool,
}

impl ActivationFailure {
    fn before_install(error: UpdateError) -> Self {
        Self {
            error,
            rolled_back: false,
            recovery_failed: false,
        }
    }
}

/// Install one validated candidate provisionally.
///
/// The single provisional record is persisted before the first live mutation;
/// `state.json` stays on the previous release until [`confirm_activation`].
pub fn install_candidate(
    installation: &Installation,
    candidate: &Candidate,
    applied_update_id: &str,
) -> Result<Committed, ActivationFailure> {
    let area = installation.area();
    ensure_private_dir(&area).map_err(ActivationFailure::before_install)?;
    ensure_private_dir(&installation.staging_root()).map_err(ActivationFailure::before_install)?;
    ensure_private_dir(&installation.preflight_root())
        .map_err(ActivationFailure::before_install)?;

    let staging = tempfile::Builder::new()
        .prefix("candidate-")
        .tempdir_in(installation.staging_root())
        .map_err(|error| UpdateError::io(&installation.staging_root(), &error))
        .map_err(ActivationFailure::before_install)?;
    let selected = package::materialize(
        &candidate.payload,
        &candidate.descriptor,
        &candidate.prefix,
        staging.path(),
    )
    .map_err(ActivationFailure::before_install)?;

    let exe_path = std::env::current_exe()
        .map_err(|error| {
            UpdateError::install(format!(
                "the running executable path is unavailable: {error}"
            ))
        })
        .map_err(ActivationFailure::before_install)?;
    let native = candidate.descriptor.native_executable.clone();
    let preflight_exe = match &native {
        Some(entry) => {
            let staged = staging.path().join(entry);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
                    .map_err(|error| UpdateError::io(&staged, &error))
                    .map_err(ActivationFailure::before_install)?;
            }
            staged
        }
        None => exe_path.clone(),
    };

    #[cfg(not(unix))]
    if native.is_some() {
        // Honest platform boundary: native self replacement plus re-execution
        // is only verified on Unix. A resource-only candidate stays available.
        return Err(ActivationFailure::before_install(UpdateError::install(
            "native executable replacement is not verified on this platform; the candidate was rejected",
        )));
    }

    preflight(
        &preflight_exe,
        staging.path(),
        &installation.preflight_root(),
    )
    .map_err(ActivationFailure::before_install)?;

    // Commit: previous files aside, candidates in, optional executable replace.
    let backup = installation.backup_root();
    let previous_record = installation
        .records()
        .map_err(ActivationFailure::before_install)?
        .active
        .clone();

    // A single previous usable release is kept; the older backup is dropped
    // only once the new candidate is ready to be committed.
    if backup.exists() {
        ensure_private_dir(&backup).map_err(ActivationFailure::before_install)?;
        std::fs::remove_dir_all(&backup)
            .map_err(|error| UpdateError::io(&backup, &error))
            .map_err(ActivationFailure::before_install)?;
    }
    ensure_private_dir(&backup).map_err(ActivationFailure::before_install)?;
    if native.is_some() {
        backup_executable(installation).map_err(ActivationFailure::before_install)?;
    }

    let resources: Vec<&SelectedEntry> = selected
        .iter()
        .filter(|entry| Some(&entry.path) != native.as_ref())
        .collect();

    let build_id = candidate
        .descriptor
        .native_build_id
        .clone()
        .unwrap_or_else(|| crate::r#const::BUILD_ID.to_string());
    let record = Record {
        update_id: applied_update_id.to_string(),
        build_id,
        native: native.is_some(),
        applied_at: crate::util::logger::now_gmt8().to_rfc3339(),
    };

    // ONE provisional activation/recovery record BEFORE the first live change.
    // It records, for every candidate resource path, whether the entry already
    // existed on the live tree: a rollback restores the old ones from `backup/`
    // and REMOVES the ones this activation created.
    let mut created: Vec<String> = Vec::new();
    for entry in &resources {
        let live = installation.root().join(&entry.path);
        match std::fs::symlink_metadata(&live) {
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                created.push(entry.path.clone());
            }
            Err(error) => {
                return Err(ActivationFailure::before_install(UpdateError::io(
                    &live, &error,
                )));
            }
        }
    }
    let activation = Activation {
        v: ACTIVATION_VERSION,
        candidate: record.clone(),
        previous: previous_record.clone(),
        paths: resources.iter().map(|entry| entry.path.clone()).collect(),
        created: created.clone(),
        executable: native.clone(),
        native: native.is_some(),
    };
    installation
        .write_activation(&activation)
        .map_err(ActivationFailure::before_install)?;

    let mut moved: Vec<String> = Vec::new();

    let commit = (|| -> Result<(), UpdateError> {
        for entry in &resources {
            let live = installation.root().join(&entry.path);
            let aside = backup.join(&entry.path);
            if let Some(parent) = aside.parent() {
                // Backup subdirectories are installer state too: create them
                // privately (0700) rather than inheriting the umask.
                ensure_private_dir(parent)?;
            }
            match std::fs::rename(&live, &aside) {
                Ok(()) => moved.push(entry.path.clone()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(UpdateError::io(&live, &error)),
            }
        }
        for entry in &resources {
            let staged = staging.path().join(&entry.path);
            let live = installation.root().join(&entry.path);
            if let Some(parent) = live.parent() {
                std::fs::create_dir_all(parent).map_err(|error| UpdateError::io(parent, &error))?;
            }
            std::fs::rename(&staged, &live).map_err(|error| UpdateError::io(&live, &error))?;
        }
        if let Some(entry) = &native {
            // The running image keeps its inode; the path now refers to the
            // candidate bytes. Re-execution below loads the real new image.
            let staged = staging.path().join(entry);
            self_replace::self_replace(&staged)
                .map_err(|error| UpdateError::install(format!("self replacement: {error}")))?;
        }
        Ok(())
    })();

    if let Err(error) = commit {
        let mut recovery_failed = false;
        // The staged native image is prepared BEFORE the resource undo, on
        // this path exactly as on the startup path: an unreadable backup
        // leaves the partly switched tree and the recovery record for the
        // next start instead of moving more files.
        let error = match prepare_native_image(
            installation,
            &backup,
            activation.executable.as_deref(),
            activation.native,
        )
        .and_then(|prepared| {
            undo_activation(
                installation,
                &backup,
                &activation,
                &activation.paths,
                prepared,
            )
        }) {
            Ok(()) => {
                // Only a COMPLETE restore retires the one recovery record; a
                // failure here left the live tree partly switched, so the
                // record must survive for the next start to retry.
                match installation.clear_activation() {
                    Ok(()) => error,
                    Err(clear) => {
                        recovery_failed = true;
                        UpdateError::install(format!(
                            "{error}; the previous release was restored but the recovery record could not be retired: {clear}"
                        ))
                    }
                }
            }
            Err(restore) => {
                recovery_failed = true;
                UpdateError::install(format!(
                    "{error}; restoring the previous release failed: {restore}; the recovery record and backup were kept for the next start"
                ))
            }
        };
        return Err(ActivationFailure {
            error,
            rolled_back: !moved.is_empty(),
            recovery_failed,
        });
    }

    Ok(Committed {
        record,
        reexec: native.is_some(),
    })
}

/// Publish the confirmed identity. Called by the real startup owner ONLY after
/// the new image's Config/App/session and required modules initialized.
pub fn confirm_activation(installation: &Installation) -> Result<Option<Record>, UpdateError> {
    installation.verify_private_area()?;
    let Some(activation) = installation.activation()? else {
        return Ok(None);
    };
    installation.write_records(&Records {
        v: STATE_VERSION,
        active: Some(activation.candidate.clone()),
        previous: activation.previous,
    })?;
    installation.clear_activation()?;
    Ok(Some(activation.candidate))
}

/// Undo one provisional activation from its ONE recovery record.
///
/// Restores every previous file from `backup/`, REMOVES every candidate file
/// this activation created, and commits the already staged previous
/// executable image at the KNOWN installation path. Every step reports its
/// real failure; a partly restored tree is never reported as success.
fn undo_activation(
    installation: &Installation,
    backup: &Path,
    activation: &Activation,
    paths: &[String],
    prepared_native: Option<PreparedNativeImage>,
) -> Result<(), UpdateError> {
    restore_moved(installation, backup, paths, &activation.created)?;
    for path in &activation.created {
        let live = installation.root().join(path);
        match std::fs::symlink_metadata(&live) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(UpdateError::install(format!(
                    "cannot remove the candidate-created {}: it is a directory",
                    live.display()
                )));
            }
            Ok(_) => std::fs::remove_file(&live).map_err(|error| UpdateError::io(&live, &error))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(UpdateError::io(&live, &error)),
        }
    }
    match (activation.native, prepared_native) {
        (true, Some(prepared)) => commit_prepared_native_image(prepared)?,
        (true, None) => {
            return Err(UpdateError::install(
                "a native rollback reached the tree undo without a staged previous executable; the recovery record was kept",
            ));
        }
        (false, _) => {}
    }
    Ok(())
}

/// Put the previous files back. A path that had no previous version is a
/// candidate-created file and has no backup to restore; a path that HAD one is
/// an unrecoverable gap if neither the live file nor its backup exists.
fn restore_moved(
    installation: &Installation,
    backup: &Path,
    moved: &[String],
    created: &[String],
) -> Result<(), UpdateError> {
    let mut failure = None;
    for path in moved {
        let live = installation.root().join(path);
        let aside = backup.join(path);
        match std::fs::symlink_metadata(&aside) {
            Ok(metadata) => {
                if metadata.is_dir() {
                    failure.get_or_insert_with(|| {
                        UpdateError::install(format!(
                            "{} inside the backup is a directory, not a previous file",
                            aside.display()
                        ))
                    });
                    continue;
                }
                if let Some(parent) = live.parent()
                    && let Err(error) = std::fs::create_dir_all(parent)
                {
                    failure.get_or_insert_with(|| UpdateError::io(parent, &error));
                    continue;
                }
                if let Err(error) = std::fs::rename(&aside, &live) {
                    failure.get_or_insert_with(|| UpdateError::io(&live, &error));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // No backup: either the candidate created this path (and the
                // removal pass retires it), or the previous file is simply
                // still live. Anything else is a real, reported gap.
                if !created.iter().any(|created| created == path)
                    && std::fs::symlink_metadata(&live).is_err()
                {
                    failure.get_or_insert_with(|| {
                        UpdateError::install(format!(
                            "{} has neither a live file nor a backup copy",
                            live.display()
                        ))
                    });
                }
            }
            Err(error) => {
                failure.get_or_insert_with(|| UpdateError::io(&aside, &error));
            }
        }
    }
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// A fully staged previous-executable image, held in a synced temporary file
/// in the KNOWN destination directory. Staged BEFORE any live resource is
/// touched, committed (atomically persisted) only after the resource undo
/// completed. Dropping it cleans the incomplete stage and leaves the original
/// backup untouched for a retry.
struct PreparedNativeImage {
    temp: tempfile::NamedTempFile,
    destination: PathBuf,
}

/// Stage the trusted previous-executable copy BEFORE the recovery tree is
/// mutated: open the backup no-follow, re-validate the OPEN handle, stream
/// the ENTIRE image into an owned destination-directory temporary file with
/// the required permissions, and sync it.
///
/// Any failure returns before live resources, records or backups change; the
/// dropped stage cleans itself. Resource-only activations need no native
/// image and return `Ok(None)` without consulting the backup.
fn prepare_native_image(
    installation: &Installation,
    backup: &Path,
    executable: Option<&str>,
    native: bool,
) -> Result<Option<PreparedNativeImage>, UpdateError> {
    if !native {
        return Ok(None);
    }
    use std::io::Write;
    let backup_exe = backup.join(BACKUP_EXE);
    let metadata = match std::fs::symlink_metadata(&backup_exe) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(UpdateError::install(format!(
                "{} is missing; the previous executable cannot be restored",
                backup_exe.display()
            )));
        }
        Err(error) => return Err(UpdateError::io(&backup_exe, &error)),
    };
    verify_backup_executable(&backup_exe, &metadata)?;
    let destination = match executable {
        Some(relative) => installation.root().join(relative),
        None => std::env::current_exe().map_err(|error| {
            UpdateError::install(format!(
                "the installation executable path is unavailable: {error}"
            ))
        })?,
    };
    let parent = destination.parent().ok_or_else(|| {
        UpdateError::install(format!("{} has no parent directory", destination.display()))
    })?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let mut source = options
        .open(&backup_exe)
        .map_err(|error| UpdateError::io(&backup_exe, &error))?;
    // Re-validate the OPEN handle, not the path: a swap between the metadata
    // probe and the open cannot smuggle in an untrusted file.
    let open_metadata = source
        .metadata()
        .map_err(|error| UpdateError::io(&backup_exe, &error))?;
    verify_backup_executable(&backup_exe, &open_metadata)?;
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).map_err(|error| UpdateError::io(parent, &error))?;
    std::io::copy(&mut source, &mut temp).map_err(|error| UpdateError::io(temp.path(), &error))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| UpdateError::io(temp.path(), &error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o755))
            .map_err(|error| UpdateError::io(temp.path(), &error))?;
    }
    let _ = temp.flush();
    Ok(Some(PreparedNativeImage { temp, destination }))
}

/// Atomically persist an already staged native image at its known
/// destination. The backup is never reopened after the resource undo, so a
/// retry still has the only recovery copy. A copy through a synced temporary
/// file in the destination directory, unlike `self_replace`, does NOT consume
/// `backup/previous-exe`: a retry after a failed rollback still has the only
/// recovery copy. The destination is the path recorded in the activation
/// record (never an arbitrary current process image).
fn commit_prepared_native_image(prepared: PreparedNativeImage) -> Result<(), UpdateError> {
    let destination = prepared.destination.clone();
    match prepared.temp.persist(&destination) {
        Ok(_) => Ok(()),
        Err(error) => Err(UpdateError::io(&destination, &error.error)),
    }
}

/// Name of the previous executable copy inside the backup directory.
const BACKUP_EXE: &str = "previous-exe";

/// Undo a provisional activation: put every backed up file back over the live
/// tree, restore the previous executable and record, and retire the
/// provisional record. Only installer-owned files are touched.
pub fn rollback_activation(installation: &Installation) -> Result<Option<Record>, UpdateError> {
    installation.verify_private_area()?;
    let Some(activation) = installation.activation()? else {
        return Ok(None);
    };
    #[cfg(test)]
    test_hooks::rollback_boundary(installation.root());
    let backup = installation.backup_root();
    // Every path the record lists, plus any previous file a crash actually
    // moved into the backup, is restored from the same complete snapshot.
    let mut paths: Vec<String> = activation.paths.clone();
    if backup.exists() {
        ensure_private_dir(&backup)?;
        let mut files: Vec<PathBuf> = Vec::new();
        collect_files(&backup, &backup, &mut files)?;
        for relative in files {
            if relative.file_name().is_some_and(|name| name == BACKUP_EXE) {
                continue;
            }
            let spelled = relative.to_string_lossy().replace('\\', "/");
            if !paths.contains(&spelled) {
                paths.push(spelled);
            }
        }
    }
    // A native rollback restores the previous executable from the backup, so
    // the ENTIRE trusted image is staged BEFORE the recovery tree is mutated:
    // a missing or unreadable backup keeps the journal, the backup and the
    // live tree untouched for a later retry. Resource-only rollbacks stage
    // nothing and never consult the backup.
    let prepared = prepare_native_image(
        installation,
        &backup,
        activation.executable.as_deref(),
        activation.native,
    )?;
    undo_activation(installation, &backup, &activation, &paths, prepared)?;
    installation.write_records(&Records {
        v: STATE_VERSION,
        active: activation.previous.clone(),
        previous: None,
    })?;
    // The recovery record is retired only after EVERY resource, native image
    // and state record was actually restored.
    installation.clear_activation()?;
    installation.discard_pending()?;
    Ok(activation.previous)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), UpdateError> {
    let entries = std::fs::read_dir(dir).map_err(|error| UpdateError::io(dir, &error))?;
    for entry in entries {
        let entry = entry.map_err(|error| UpdateError::io(dir, &error))?;
        let path = entry.path();
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|error| UpdateError::io(&path, &error))?;
        if metadata.file_type().is_symlink() {
            return Err(UpdateError::install(format!(
                "{} inside the backup is a symlink",
                path.display()
            )));
        }
        if metadata.is_dir() {
            ensure_private_dir(&path)?;
            collect_files(root, &path, out)?;
        } else {
            let relative = path
                .strip_prefix(root)
                .map_err(|_| UpdateError::install("backup path outside the backup root"))?
                .to_path_buf();
            out.push(relative);
        }
    }
    Ok(())
}

/// Copy the current executable into the backup directory so a rollback can put
/// it back. Called only for native candidates, and only on a copy of the
/// installation (never Cargo's shared artifact).
pub fn backup_executable(installation: &Installation) -> Result<(), UpdateError> {
    let backup = installation.backup_root();
    ensure_private_dir(&backup)?;
    let exe = std::env::current_exe()
        .map_err(|error| UpdateError::install(format!("executable path: {error}")))?;
    let target = backup.join(BACKUP_EXE);
    std::fs::copy(&exe, &target).map_err(|error| UpdateError::io(&target, &error))?;
    Ok(())
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
        // `env_clear` also removes the native lock handoff spelling: the
        // private preflight never reacquires or inherits the parent's lock.
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

/// Re-execute the installed image in place, carrying the owned installation
/// lock across the exec when one is held. Returns only if the platform has no
/// verified path, which the caller reports as a failed activation.
pub fn reexec(exe: &Path, lock: Option<&InstallLock>) -> UpdateError {
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
        if let Some(lock) = lock
            && let Err(error) = lock.prepare_exec()
        {
            return error;
        }
        // Test-only fault, compiled out of production: the REAL exec then
        // fails with EACCES instead of entering the new image.
        #[cfg(test)]
        if test_hooks::exec_permission_fault() {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(exe, std::fs::Permissions::from_mode(0o000));
        }
        let error = nix::unistd::execv(&args[0], &args).unwrap_err();
        UpdateError::install(format!("re-executing the installed update: {error}"))
    }
    #[cfg(not(unix))]
    {
        let _ = (exe, lock);
        UpdateError::install("re-execution is not verified on this platform")
    }
}
