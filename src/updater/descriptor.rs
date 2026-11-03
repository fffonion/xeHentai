//! The v1 signed package descriptor and its mandatory native checks.
//!
//! The descriptor is a small JSON document whose EXACT bytes are signed with a
//! detached Ed25519 signature. Verification therefore runs on the raw bytes
//! before any parse or trust decision: nothing is re-serialized, canonicalized
//! or repaired. `serde_json` is only a decoder at this wire boundary; every
//! field is then validated against the explicit contract numbers compiled into
//! this binary.
//!
//! Native safety that the descriptor controls:
//!   * `update_id` must be the identifier the trusted RSS decision selected;
//!   * `target` must equal the compiled target triple, `host_abi`/`script_abi`
//!     the compiled contract revisions, `app_version` this build's version;
//!   * `archive_sha256` is the SHA-256 of the whole payload ZIP;
//!   * `entries` is the complete list of install-relative files with their
//!     exact lengths, and no entry may name user state or an ambiguous path;
//!   * a native executable entry is only meaningful together with the
//!     compiled build identifier it must report after the update is applied.

use std::path::Component;

use ed25519_dalek::pkcs8::DecodePublicKey;
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::error::UpdateError;

pub const DESCRIPTOR_VERSION: i64 = 1;
/// Bound on the signed descriptor document itself.
pub const MAX_DESCRIPTOR_BYTES: usize = 256 * 1024;
/// A detached Ed25519 signature is exactly 64 bytes.
pub const SIGNATURE_BYTES: usize = 64;
pub const MAX_ENTRIES: usize = 4096;
/// Bounds applied to declared and actually copied bytes, not just headers.
pub const MAX_ENTRY_BYTES: u64 = 256 * 1024 * 1024;
pub const MAX_TOTAL_BYTES: u64 = 384 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Descriptor {
    pub v: i64,
    pub update_id: String,
    pub app_version: String,
    pub target: String,
    pub host_abi: i64,
    pub script_abi: i64,
    pub archive_sha256: String,
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub native_executable: Option<String>,
    #[serde(default)]
    pub native_build_id: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub path: String,
    pub len: u64,
}

/// Verify the detached signature over the EXACT descriptor bytes, then parse
/// and validate the document. A missing verification key is a rejection: a
/// build whose source binding carries no key cannot report a verified package
/// (production stays unbound until the release stage binds it).
pub fn verify(
    descriptor_bytes: &[u8],
    signature: &[u8],
    verification_key_pem: Option<&str>,
    expected_update_id: &str,
) -> Result<Descriptor, UpdateError> {
    let descriptor = authenticate(descriptor_bytes, signature, verification_key_pem)?;
    validate(&descriptor, expected_update_id)?;
    Ok(descriptor)
}

/// Signature and schema only: the bytes are authenticated, so the update id
/// they carry is trusted. Used by the startup controller, which validates the
/// already-selected pending package against its own signed id.
pub fn authenticate(
    descriptor_bytes: &[u8],
    signature: &[u8],
    verification_key_pem: Option<&str>,
) -> Result<Descriptor, UpdateError> {
    if descriptor_bytes.is_empty() || descriptor_bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(UpdateError::package(format!(
            "descriptor is {} bytes; the limit is {MAX_DESCRIPTOR_BYTES}",
            descriptor_bytes.len()
        )));
    }
    let Some(pem) = verification_key_pem else {
        return Err(UpdateError::package(
            "the source binding carries no verification key; signed packages cannot be trusted",
        ));
    };
    let key = VerifyingKey::from_public_key_pem(pem)
        .map_err(|error| UpdateError::package(format!("invalid verification key: {error}")))?;
    if signature.len() != SIGNATURE_BYTES {
        return Err(UpdateError::package(format!(
            "detached signature is {} bytes, expected {SIGNATURE_BYTES}",
            signature.len()
        )));
    }
    let signature = Signature::from_slice(signature)
        .map_err(|error| UpdateError::package(format!("invalid signature bytes: {error}")))?;
    key.verify_strict(descriptor_bytes, &signature)
        .map_err(|_| UpdateError::package("detached signature does not match the descriptor"))?;
    parse(descriptor_bytes)
}

/// Strict decode of an already-authenticated descriptor document.
pub fn parse(descriptor_bytes: &[u8]) -> Result<Descriptor, UpdateError> {
    serde_json::from_slice(descriptor_bytes)
        .map_err(|error| UpdateError::package(format!("descriptor schema: {error}")))
}

/// Contract checks every native consumer enforces, whatever the RSS decision
/// asked for.
pub fn validate(descriptor: &Descriptor, expected_update_id: &str) -> Result<(), UpdateError> {
    if descriptor.v != DESCRIPTOR_VERSION {
        return Err(UpdateError::package(format!(
            "descriptor version {} is not {DESCRIPTOR_VERSION}",
            descriptor.v
        )));
    }
    validate_identifier(&descriptor.update_id, "update_id")?;
    if descriptor.update_id != expected_update_id {
        return Err(UpdateError::package(format!(
            "descriptor update_id {:?} is not the selected {:?}",
            descriptor.update_id, expected_update_id
        )));
    }
    if descriptor.app_version != crate::r#const::VERSION_DISPLAY {
        return Err(UpdateError::package(format!(
            "package app_version {:?} does not match this build {:?}",
            descriptor.app_version,
            crate::r#const::VERSION_DISPLAY
        )));
    }
    if descriptor.target != crate::r#const::TARGET {
        return Err(UpdateError::package(format!(
            "package target {:?} is not this build target {:?}",
            descriptor.target,
            crate::r#const::TARGET
        )));
    }
    if descriptor.host_abi != crate::r#const::HOST_ABI
        || descriptor.script_abi != crate::r#const::SCRIPT_ABI
    {
        return Err(UpdateError::package(format!(
            "package ABI host={} script={} is not this build host={} script={}",
            descriptor.host_abi,
            descriptor.script_abi,
            crate::r#const::HOST_ABI,
            crate::r#const::SCRIPT_ABI
        )));
    }
    validate_sha256(&descriptor.archive_sha256, "archive_sha256")?;
    if descriptor.entries.is_empty() || descriptor.entries.len() > MAX_ENTRIES {
        return Err(UpdateError::package(format!(
            "descriptor lists {} entries; the bounds are 1..={MAX_ENTRIES}",
            descriptor.entries.len()
        )));
    }
    let mut total = 0u64;
    for entry in &descriptor.entries {
        validate_install_path(&entry.path)?;
        if entry.len > MAX_ENTRY_BYTES {
            return Err(UpdateError::package(format!(
                "entry {:?} declares {} bytes, above the {MAX_ENTRY_BYTES} limit",
                entry.path, entry.len
            )));
        }
        total = total.saturating_add(entry.len);
    }
    if total > MAX_TOTAL_BYTES {
        return Err(UpdateError::package(format!(
            "declared entry total {total} is above the {MAX_TOTAL_BYTES} limit"
        )));
    }
    let mut seen: Vec<&str> = Vec::with_capacity(descriptor.entries.len());
    for entry in &descriptor.entries {
        if seen.contains(&entry.path.as_str()) {
            return Err(UpdateError::package(format!(
                "duplicate entry {:?}",
                entry.path
            )));
        }
        seen.push(&entry.path);
    }
    match (&descriptor.native_executable, &descriptor.native_build_id) {
        (None, None) => {}
        (Some(executable), Some(build_id)) => {
            validate_install_path(executable)?;
            if !descriptor
                .entries
                .iter()
                .any(|entry| &entry.path == executable)
            {
                return Err(UpdateError::package(format!(
                    "native executable {executable:?} is not one of the declared entries"
                )));
            }
            validate_identifier(build_id, "native_build_id")?;
        }
        (Some(_), None) => {
            return Err(UpdateError::package(
                "a native executable requires a native_build_id",
            ));
        }
        (None, Some(_)) => {
            return Err(UpdateError::package(
                "a native_build_id requires a native executable entry",
            ));
        }
    }
    Ok(())
}

/// SHA-256 of the whole downloaded payload ZIP, lowercase hex.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    hex::encode(digest)
}

pub fn validate_sha256(value: &str, what: &str) -> Result<(), UpdateError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(UpdateError::package(format!(
            "{what} {value:?} is not 64 lowercase hex digits"
        )));
    }
    Ok(())
}

/// Identifiers are used exactly as given. Anything that is not a bounded
/// `[A-Za-z0-9._-]` string is rejected rather than repaired.
pub fn validate_identifier(value: &str, what: &str) -> Result<(), UpdateError> {
    let ok = !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'));
    if ok {
        Ok(())
    } else {
        Err(UpdateError::package(format!(
            "{what} {value:?} is not a plain identifier"
        )))
    }
}

/// Install-relative path that a package may write. Absolute paths, `..`,
/// empty components, backslashes, drive/stream separators, trailing slashes
/// and the application's own private persistence names are all rejected.
pub fn validate_install_path(path: &str) -> Result<(), UpdateError> {
    if path.is_empty() || path.len() > 4096 {
        return Err(UpdateError::package(format!(
            "install path {path:?} is empty or too long"
        )));
    }
    if path.contains('\\') {
        return Err(UpdateError::package(format!(
            "install path {path:?} contains a backslash"
        )));
    }
    if path.ends_with('/') {
        return Err(UpdateError::package(format!(
            "install path {path:?} ends with a separator"
        )));
    }
    let mut components = 0usize;
    for component in std::path::Path::new(path).components() {
        match component {
            Component::Normal(name) => {
                let name = name.to_str().ok_or_else(|| {
                    UpdateError::package(format!("install path {path:?} is not UTF-8"))
                })?;
                if name.is_empty() || name.contains(':') {
                    return Err(UpdateError::package(format!(
                        "install path {path:?} has an ambiguous component {name:?}"
                    )));
                }
                if components == 0 && is_private_name(name) {
                    return Err(UpdateError::package(format!(
                        "install path {path:?} targets application state"
                    )));
                }
                components += 1;
            }
            Component::CurDir => {
                return Err(UpdateError::package(format!(
                    "install path {path:?} contains a current-directory component"
                )));
            }
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(UpdateError::package(format!(
                    "install path {path:?} is not a relative install path"
                )));
            }
        }
    }
    if components == 0 {
        return Err(UpdateError::package(format!(
            "install path {path:?} names nothing"
        )));
    }
    if is_private_name(path) {
        return Err(UpdateError::package(format!(
            "install path {path:?} targets application state"
        )));
    }
    Ok(())
}

/// The names this application keeps its own state in. A package never owns
/// them, so it can never overwrite, restore or copy over them.
fn is_private_name(name: &str) -> bool {
    matches!(
        name,
        crate::core::SESSION_FILE
            | crate::config::CONFIG_FILE
            | crate::config::LEGACY_CONFIG_FILE
            | crate::core::LEGACY_COOKIE_FILE
            | "eh.log"
    ) || name == super::install::AREA_DIR
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Interoperability with the approved independent OpenSSL/Ed25519 vectors:
    /// the retained manifest bytes verify under the retained public key, and
    /// tampered bytes / wrong key / altered signature do not. This is primitive
    /// interop only — it is not the production package schema.
    #[test]
    fn openssl_ed25519_vectors_verify_exactly() {
        let manifest = include_bytes!("testdata/manifest.json");
        let signature = include_bytes!("testdata/manifest.sig");
        let key = include_str!("testdata/test-public-key.pem");
        let wrong = include_str!("testdata/wrong-test-public-key.pem");
        let key = VerifyingKey::from_public_key_pem(key).expect("vector key");
        let signature = Signature::from_slice(signature).expect("vector signature");
        key.verify_strict(manifest, &signature)
            .expect("vector verify");
        let wrong = VerifyingKey::from_public_key_pem(wrong).expect("wrong key");
        assert!(wrong.verify_strict(manifest, &signature).is_err());
        for altered in [
            include_bytes!("testdata/manifest-tampered.json").as_slice(),
            include_bytes!("testdata/manifest-whitespace.json").as_slice(),
        ] {
            assert!(
                key.verify_strict(altered, &signature).is_err(),
                "equivalent-but-different bytes must not verify"
            );
        }
        for bad in [
            include_bytes!("testdata/manifest-bad.sig").as_slice(),
            include_bytes!("testdata/manifest-short.sig").as_slice(),
        ] {
            if let Ok(signature) = Signature::from_slice(bad) {
                assert!(key.verify_strict(manifest, &signature).is_err());
            }
        }
    }
}
