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
//!   pending/src.zip                    one atomically published envelope
//!   staging/<random>/                  complete candidate tree, private
//!   preflight/                         private state for the candidate run
//!   backup/                            the single previous usable release
//!   state.json                         active + previous records (atomic)
//!   confirm                            present between replace and re-exec
//! ```
//!
//! Activation order: materialize the signed entries into a fresh staging tree,
//! preflight the real candidate with the same host's minimal private update
//! mode, then move the previous files aside, install the new ones, and (for a
//! native package) replace the executable and re-execute it. Any failure
//! restores the previous files and record. `h.json`, `config.rss`, `eh.log`
//! and the legacy cookie file are never package entries and never touched.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::descriptor::Descriptor;
use super::error::UpdateError;
use super::package::{self, SelectedEntry};

pub const AREA_DIR: &str = ".xehentai-update";
pub const STATE_FILE: &str = "state.json";
pub const CONFIRM_FILE: &str = "confirm";
pub const PENDING_DIR: &str = "pending";
pub const STAGING_DIR: &str = "staging";
pub const BACKUP_DIR: &str = "backup";
pub const PREFLIGHT_DIR: &str = "preflight";
pub const STATE_VERSION: i64 = 1;
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

    fn confirm_path(&self) -> PathBuf {
        self.area().join(CONFIRM_FILE)
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

    /// True while an executable replacement has not been confirmed by the
    /// replaced image actually starting.
    pub fn has_confirm(&self) -> bool {
        self.confirm_path().exists()
    }

    pub fn clear_confirm(&self) -> Result<(), UpdateError> {
        let path = self.confirm_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(UpdateError::io(&path, &error)),
        }
    }

    pub fn records(&self) -> Result<Records, UpdateError> {
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
    pub fn discard_pending(&self) -> Result<(), UpdateError> {
        let path = self.pending_path();
        match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(UpdateError::io(&path, &error)),
        }
    }
}

/// Create (and verify) a private installer-owned directory.
///
/// The path is created if missing, then checked with `symlink_metadata`: the
/// object must be a real directory, not a symlink, so a swapped component can
/// never redirect installer writes. Unix permissions are tightened to 0700.
pub fn ensure_private_dir(path: &Path) -> Result<(), UpdateError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(UpdateError::install(format!(
                "{} is a symlink; the update area must be a real directory",
                path.display()
            )));
        }
        Ok(metadata) if !metadata.is_dir() => {
            return Err(UpdateError::install(format!(
                "{} exists and is not a directory",
                path.display()
            )));
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(UpdateError::io(path, &error)),
    }
    std::fs::create_dir_all(path).map_err(|error| UpdateError::io(path, &error))?;
    let metadata =
        std::fs::symlink_metadata(path).map_err(|error| UpdateError::io(path, &error))?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(UpdateError::install(format!(
            "{} could not be created as a real directory",
            path.display()
        )));
    }
    // Only a directory this installer just created is tightened; an existing
    // directory's mode belongs to whoever administers the installation, and a
    // non-writable one must keep failing writes instead of being widened.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(|error| UpdateError::io(path, &error))?;
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

/// Facts a validated pending candidate provides to the startup decision.
pub struct Candidate {
    pub descriptor: Descriptor,
    pub payload: Vec<u8>,
    pub prefix: String,
}

/// Install one validated candidate. `native` candidates replace the executable
/// and re-execute it; resource candidates only swap the listed files.
///
/// On any failure after the first live file moved, the previous files and
/// record are restored, so the caller can log a rollback over an unchanged
/// installation.
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
}

impl ActivationFailure {
    fn before_install(error: UpdateError) -> Self {
        Self {
            error,
            rolled_back: false,
        }
    }
}

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
        std::fs::remove_dir_all(&backup)
            .map_err(|error| UpdateError::io(&backup, &error))
            .map_err(ActivationFailure::before_install)?;
    }
    ensure_private_dir(&backup).map_err(ActivationFailure::before_install)?;
    if native.is_some() {
        backup_executable(installation).map_err(ActivationFailure::before_install)?;
    }

    let mut moved: Vec<String> = Vec::new();
    let resources: Vec<&SelectedEntry> = selected
        .iter()
        .filter(|entry| Some(&entry.path) != native.as_ref())
        .collect();

    let commit = (|| -> Result<(), UpdateError> {
        for entry in &resources {
            let live = installation.root().join(&entry.path);
            let aside = backup.join(&entry.path);
            if let Some(parent) = aside.parent() {
                std::fs::create_dir_all(parent).map_err(|error| UpdateError::io(parent, &error))?;
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
        let _ = restore(
            installation,
            &backup,
            &moved,
            previous_record.clone().as_ref(),
        );
        return Err(ActivationFailure {
            error,
            rolled_back: !moved.is_empty(),
        });
    }

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
    let records = Records {
        v: STATE_VERSION,
        active: Some(record.clone()),
        previous: previous_record,
    };
    if let Err(error) = installation.write_records(&records) {
        let _ = restore(
            installation,
            &backup,
            &moved,
            records.previous.clone().as_ref(),
        );
        return Err(ActivationFailure {
            error,
            rolled_back: true,
        });
    }
    if native.is_some() {
        let confirm = installation.confirm_path();
        write_atomic(&area, &confirm, b"pending\n").map_err(ActivationFailure::before_install)?;
    }
    Ok(Committed {
        record,
        reexec: native.is_some(),
    })
}

/// Put the previous files and record back. Best effort: every step that can
/// fail is reported through the returned error, and a partially restored tree
/// is never reported as success.
fn restore(
    installation: &Installation,
    backup: &Path,
    moved: &[String],
    previous: Option<&Record>,
) -> Result<(), UpdateError> {
    let mut failure = None;
    for path in moved {
        let live = installation.root().join(path);
        let aside = backup.join(path);
        if !aside.exists() {
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
    let backup_exe = backup.join(BACKUP_EXE);
    if backup_exe.exists()
        && let Err(error) = self_replace::self_replace(&backup_exe)
    {
        failure.get_or_insert_with(|| {
            UpdateError::install(format!("restoring the previous executable: {error}"))
        });
    }
    let records = Records {
        v: STATE_VERSION,
        active: previous.cloned(),
        previous: None,
    };
    if let Err(error) = installation.write_records(&records) {
        failure.get_or_insert(error);
    }
    let _ = std::fs::remove_file(installation.confirm_path());
    match failure {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Name of the previous executable copy inside the backup directory.
const BACKUP_EXE: &str = "previous-exe";

/// Undo an activation whose replaced image never confirmed: put every backed
/// up file back over the live tree, restore the previous executable, and make
/// the previous record active again. Only installer-owned files are touched.
pub fn rollback_activation(installation: &Installation) -> Result<Option<Record>, UpdateError> {
    let backup = installation.backup_root();
    let previous = installation.records()?.previous;
    let mut files: Vec<PathBuf> = Vec::new();
    if backup.exists() {
        collect_files(&backup, &backup, &mut files)?;
    }
    for relative in &files {
        if relative.file_name().is_some_and(|name| name == BACKUP_EXE) {
            continue;
        }
        let live = installation.root().join(relative);
        let aside = backup.join(relative);
        if let Some(parent) = live.parent() {
            std::fs::create_dir_all(parent).map_err(|error| UpdateError::io(parent, &error))?;
        }
        std::fs::rename(&aside, &live).map_err(|error| UpdateError::io(&live, &error))?;
    }
    let backup_exe = backup.join(BACKUP_EXE);
    if backup_exe.exists() {
        self_replace::self_replace(&backup_exe).map_err(|error| {
            UpdateError::install(format!("restoring the previous executable: {error}"))
        })?;
        let _ = std::fs::remove_file(&backup_exe);
    }
    installation.write_records(&Records {
        v: STATE_VERSION,
        active: previous.clone(),
        previous: None,
    })?;
    installation.clear_confirm()?;
    Ok(previous)
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
/// no verified path, which the caller reports as a failed activation.
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
        let error = nix::unistd::execv(&args[0], &args).unwrap_err();
        UpdateError::install(format!("re-executing the installed update: {error}"))
    }
    #[cfg(not(unix))]
    {
        let _ = exe;
        UpdateError::install("re-execution is not verified on this platform")
    }
}
