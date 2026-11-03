//! The pending update envelope and the payload archive reader.
//!
//! Pending is ONE atomically published STORE ZIP named `src.zip` in the
//! installer-owned update area. It contains exactly three members:
//!
//! ```text
//! info.json    the exact signed descriptor bytes (never re-serialized)
//! info.sig     the detached Ed25519 signature over those bytes
//! payload.zip  the original downloaded ZIP bytes, unchanged
//! ```
//!
//! The envelope preserves the signed archive bytes without a digest
//! self-reference or a multi-file publication transaction, and a STORE member
//! keeps every byte readable without a codec. Publishing writes a private
//! temporary file in the same directory and renames it over `src.zip`, so a
//! failed or cancelled download leaves the previous valid pending untouched.
//!
//! The payload archive itself is only ever read through the `zip` crate's own
//! `enclosed_name`/reader, and every selected entry is copied through the real
//! reader while its byte count is compared with the signed length. Header
//! sizes alone are never trusted. Foreign entries (outside the RSS-selected
//! `xeHentai-<update_id>/` prefix) and directory entries are not materialized.

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use zip::ZipArchive;
use zip::write::SimpleFileOptions;

use super::descriptor::{Descriptor, MAX_ENTRY_BYTES, MAX_TOTAL_BYTES};
use super::error::UpdateError;

/// Fixed envelope member names, in publication order.
pub const INFO_NAME: &str = "info.json";
pub const SIGNATURE_NAME: &str = "info.sig";
pub const PAYLOAD_NAME: &str = "payload.zip";
/// Pending file name inside the update area (reference `const.SRC_UPDATE_FILE`).
pub const PENDING_FILE: &str = crate::r#const::SRC_UPDATE_FILE;

pub const MAX_INFO_BYTES: usize = 256 * 1024;
pub const MAX_SIGNATURE_BYTES: usize = 4 * 1024;
pub const MAX_PAYLOAD_BYTES: usize = 384 * 1024 * 1024;
/// Bound on ALL payload members (selected, foreign and directory entries),
/// enforced immediately after the standard ZIP parser and before the walk.
/// It is the signed entry limit plus a small allowance for the prefix
/// directory entries a normal archive carries.
pub const MAX_ARCHIVE_MEMBERS: usize = crate::updater::descriptor::MAX_ENTRIES + 256;
/// The three fixed members plus their STORE framing only; the envelope cannot
/// be larger than its parts.
const MAX_ENVELOPE_BYTES: u64 =
    (MAX_INFO_BYTES + MAX_SIGNATURE_BYTES + MAX_PAYLOAD_BYTES + 4096) as u64;

pub struct PendingEnvelope {
    pub info: Vec<u8>,
    pub sig: Vec<u8>,
    pub payload: Vec<u8>,
}

impl PendingEnvelope {
    pub fn new(info: Vec<u8>, sig: Vec<u8>, payload: Vec<u8>) -> Result<Self, UpdateError> {
        if info.len() > MAX_INFO_BYTES {
            return Err(UpdateError::package(
                "descriptor is too large for the envelope",
            ));
        }
        if sig.len() > MAX_SIGNATURE_BYTES {
            return Err(UpdateError::package(
                "signature is too large for the envelope",
            ));
        }
        if payload.len() > MAX_PAYLOAD_BYTES {
            return Err(UpdateError::package("payload archive is too large"));
        }
        Ok(Self { info, sig, payload })
    }
}

/// Publish the envelope atomically as `<pending_dir>/src.zip`.
#[allow(dead_code)]
pub fn publish(pending_dir: &Path, envelope: &PendingEnvelope) -> Result<PathBuf, UpdateError> {
    let staged = stage(pending_dir, envelope)?;
    staged.commit()?;
    Ok(pending_dir.join(PENDING_FILE))
}

/// A completely written, synced envelope temp file that has not yet replaced
/// the published pending candidate.
///
/// Splitting staging from the final rename is what lets one owned blocking job
/// do all the heavy work first and then linearize its single commit against
/// cancellation: a cancelled job simply drops this value, which removes its
/// temp file, so the previous pending stays byte-identical.
pub struct Staged {
    temp: tempfile::NamedTempFile,
    target: PathBuf,
}

impl Staged {
    /// Atomically replace the published pending envelope with this staged one.
    pub fn commit(self) -> Result<(), UpdateError> {
        match self.temp.persist(&self.target) {
            Ok(_) => Ok(()),
            Err(error) => Err(UpdateError::io(&self.target, &error.error)),
        }
    }

    /// Path of the published envelope this staged file targets.
    #[allow(dead_code)]
    pub fn target(&self) -> &Path {
        &self.target
    }
}

/// Write and sync the envelope into `pending_dir` without publishing it.
pub fn stage(pending_dir: &Path, envelope: &PendingEnvelope) -> Result<Staged, UpdateError> {
    super::install::ensure_private_dir(pending_dir)?;
    let mut bytes = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut bytes));
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o644);
        for (name, body) in [
            (INFO_NAME, envelope.info.as_slice()),
            (SIGNATURE_NAME, envelope.sig.as_slice()),
            (PAYLOAD_NAME, envelope.payload.as_slice()),
        ] {
            writer.start_file(name, options).map_err(|error| {
                UpdateError::package(format!("envelope member {name}: {error}"))
            })?;
            writer.write_all(body).map_err(|error| {
                UpdateError::package(format!("envelope member {name}: {error}"))
            })?;
        }
        writer
            .finish()
            .map_err(|error| UpdateError::package(format!("envelope finish: {error}")))?;
    }
    let target = pending_dir.join(PENDING_FILE);
    let mut temp = tempfile::NamedTempFile::new_in(pending_dir)
        .map_err(|error| UpdateError::io(pending_dir, &error))?;
    temp.write_all(&bytes)
        .map_err(|error| UpdateError::io(temp.path(), &error))?;
    temp.as_file()
        .sync_all()
        .map_err(|error| UpdateError::io(temp.path(), &error))?;
    Ok(Staged { temp, target })
}

/// Read and structurally validate a published envelope. The signature and
/// digest are checked by the caller against the trusted binding; this function
/// only guarantees the three fixed members, their uniqueness and their bounds.
/// The caller must have validated the directory and leaf it opened.
/// Decode an envelope whose bytes the caller obtained through its own trusted
/// open (the installer validates the pending directory and leaf first).
pub fn read_bytes(bytes: &[u8]) -> Result<PendingEnvelope, UpdateError> {
    if bytes.len() as u64 > MAX_ENVELOPE_BYTES {
        return Err(UpdateError::package(
            "pending envelope is larger than its parts allow",
        ));
    }
    let mut archive = ZipArchive::new(Cursor::new(bytes.to_vec()))
        .map_err(|error| UpdateError::package(format!("pending envelope is not a ZIP: {error}")))?;
    let mut names: Vec<String> = Vec::new();
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .map_err(|error| UpdateError::package(format!("pending member {index}: {error}")))?;
        if file.is_dir() {
            return Err(UpdateError::package(
                "pending envelope contains a directory",
            ));
        }
        let name = file.name().to_string();
        if !matches!(name.as_str(), INFO_NAME | SIGNATURE_NAME | PAYLOAD_NAME) {
            return Err(UpdateError::package(format!(
                "pending envelope contains unexpected member {name:?}"
            )));
        }
        if names.contains(&name) {
            return Err(UpdateError::package(format!(
                "pending envelope repeats member {name:?}"
            )));
        }
        names.push(name);
    }
    if names.len() != 3 {
        return Err(UpdateError::package(format!(
            "pending envelope has {} members, expected 3",
            names.len()
        )));
    }
    let info = member(&mut archive, INFO_NAME, MAX_INFO_BYTES)?;
    let sig = member(&mut archive, SIGNATURE_NAME, MAX_SIGNATURE_BYTES)?;
    let payload = member(&mut archive, PAYLOAD_NAME, MAX_PAYLOAD_BYTES)?;
    if payload.len() as u64 > MAX_PAYLOAD_BYTES as u64 {
        return Err(UpdateError::package("payload archive is too large"));
    }
    PendingEnvelope::new(info, sig, payload)
}

fn member(
    archive: &mut ZipArchive<Cursor<Vec<u8>>>,
    name: &str,
    limit: usize,
) -> Result<Vec<u8>, UpdateError> {
    let mut file = archive
        .by_name(name)
        .map_err(|error| UpdateError::package(format!("pending member {name}: {error}")))?;
    let mut out = Vec::new();
    let read = file
        .by_ref()
        .take(limit as u64 + 1)
        .read_to_end(&mut out)
        .map_err(|error| UpdateError::package(format!("pending member {name}: {error}")))?;
    if read > limit {
        return Err(UpdateError::package(format!(
            "pending member {name} is larger than {limit} bytes"
        )));
    }
    Ok(out)
}

/// One selected payload member: the install-relative path, the payload member
/// name under the source prefix, and the signed length.
pub struct SelectedEntry {
    pub path: String,
    pub member: String,
    pub len: u64,
}

/// Select, bound and content-verify the payload members listed by the signed
/// descriptor. Every selected member is read through the real ZIP reader and
/// its copied byte count must equal the signed length; dangerous names,
/// symlinks, duplicates and expansion are rejected.
pub fn select(
    payload: &[u8],
    descriptor: &Descriptor,
    prefix: &str,
) -> Result<Vec<SelectedEntry>, UpdateError> {
    let mut archive = open(payload)?;
    // Every member, foreign or selected, is bounded BEFORE the walk. The
    // signed entry limit caps what can be installed, not how much a valid
    // signed archive may make this reader enumerate.
    if archive.len() > MAX_ARCHIVE_MEMBERS {
        return Err(UpdateError::package(format!(
            "payload archive has {} members; the limit is {MAX_ARCHIVE_MEMBERS}",
            archive.len()
        )));
    }
    let mut names: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for index in 0..archive.len() {
        let file = archive
            .by_index(index)
            .map_err(|error| UpdateError::package(format!("payload member {index}: {error}")))?;
        let raw = file.name().to_string();
        if raw.contains('\\') {
            return Err(UpdateError::package(format!(
                "payload member {raw:?} contains a backslash"
            )));
        }
        if let Some(mode) = file.unix_mode()
            && mode & 0o170000 == 0o120000
        {
            return Err(UpdateError::package(format!(
                "payload member {raw:?} is a symlink"
            )));
        }
        // `enclosed_name` is the ZIP crate's own containment check: absolute
        // paths and `..` escapes yield `None` here.
        if file.enclosed_name().is_none() {
            return Err(UpdateError::package(format!(
                "payload member {raw:?} is not an enclosed path"
            )));
        }
        if !names.insert(raw.clone()) {
            return Err(UpdateError::package(format!(
                "payload repeats member {raw:?}"
            )));
        }
    }

    let mut total = 0u64;
    let mut selected = Vec::with_capacity(descriptor.entries.len());
    for entry in &descriptor.entries {
        let member = format!("{prefix}{}", entry.path);
        if !names.contains(&member) {
            return Err(UpdateError::package(format!(
                "payload is missing selected member {member:?}"
            )));
        }
        total = total.saturating_add(entry.len);
        if entry.len > MAX_ENTRY_BYTES || total > MAX_TOTAL_BYTES {
            return Err(UpdateError::package(format!(
                "selected member {member:?} exceeds the archive expansion bounds"
            )));
        }
        selected.push(SelectedEntry {
            path: entry.path.clone(),
            member,
            len: entry.len,
        });
    }

    // Content pass: real reader, real byte counts.
    for item in &selected {
        let mut file = archive.by_name(&item.member).map_err(|error| {
            UpdateError::package(format!("payload member {}: {error}", item.member))
        })?;
        let copied = copy_bounded(&mut file, &mut std::io::sink(), item.len, &item.member)?;
        if copied != item.len {
            return Err(UpdateError::package(format!(
                "payload member {:?} holds {copied} bytes, the descriptor declares {}",
                item.member, item.len
            )));
        }
    }
    Ok(selected)
}

/// Write the selected members into `dest` as a complete candidate tree. Every
/// file is created exclusively (`create_new`), so a staged tree can never be
/// confused with an existing file, and each copied byte count is verified
/// again against the signed length.
pub fn materialize(
    payload: &[u8],
    descriptor: &Descriptor,
    prefix: &str,
    dest: &Path,
) -> Result<Vec<SelectedEntry>, UpdateError> {
    let selected = select(payload, descriptor, prefix)?;
    let mut archive = open(payload)?;
    for item in &selected {
        let target = dest.join(&item.path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|error| UpdateError::io(parent, &error))?;
        }
        let mut file = archive.by_name(&item.member).map_err(|error| {
            UpdateError::package(format!("payload member {}: {error}", item.member))
        })?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)
            .map_err(|error| UpdateError::io(&target, &error))?;
        let copied = copy_bounded(&mut file, &mut out, item.len, &item.member)?;
        if copied != item.len {
            return Err(UpdateError::package(format!(
                "payload member {:?} holds {copied} bytes, the descriptor declares {}",
                item.member, item.len
            )));
        }
        out.sync_all()
            .map_err(|error| UpdateError::io(&target, &error))?;
    }
    Ok(selected)
}

/// Copy at most `limit` bytes, counting what was really read. A member that
/// expands beyond its signed length is a rejection rather than a truncation.
fn copy_bounded<R: Read, W: Write>(
    reader: &mut R,
    writer: &mut W,
    limit: u64,
    what: &str,
) -> Result<u64, UpdateError> {
    let mut buffer = [0u8; 64 * 1024];
    let mut copied = 0u64;
    loop {
        let read = reader
            .read(&mut buffer)
            .map_err(|error| UpdateError::package(format!("payload member {what}: {error}")))?;
        if read == 0 {
            return Ok(copied);
        }
        copied = copied.saturating_add(read as u64);
        if copied > limit {
            return Err(UpdateError::package(format!(
                "payload member {what} expands beyond its declared {limit} bytes"
            )));
        }
        writer
            .write_all(&buffer[..read])
            .map_err(|error| UpdateError::io(Path::new(what), &error))?;
    }
}

fn open(payload: &[u8]) -> Result<ZipArchive<Cursor<&[u8]>>, UpdateError> {
    ZipArchive::new(Cursor::new(payload))
        .map_err(|error| UpdateError::package(format!("payload is not a ZIP: {error}")))
}

/// Zip entry point used by the release/packaging helpers and tests: a STORE
/// archive whose members are exactly `(name, bytes)` in the given order.
pub fn store_zip(members: &[(String, Vec<u8>)]) -> Result<Vec<u8>, UpdateError> {
    let mut bytes = Vec::new();
    {
        let mut writer = zip::ZipWriter::new(Cursor::new(&mut bytes));
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o644);
        for (name, body) in members {
            writer
                .start_file(name, options)
                .map_err(|error| UpdateError::package(format!("archive member {name}: {error}")))?;
            writer
                .write_all(body)
                .map_err(|error| UpdateError::package(format!("archive member {name}: {error}")))?;
        }
        writer
            .finish()
            .map_err(|error| UpdateError::package(format!("archive finish: {error}")))?;
    }
    Ok(bytes)
}
