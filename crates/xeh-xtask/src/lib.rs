//! xeHentai compatibility manifest and oracle command library.
//!
//! Application behavior is not implemented here. This crate only checks the
//! acceptance manifest and runs the original Python program as an oracle.

mod manifest;
mod oracle;
mod paths;
mod verify;

pub use manifest::{check_coverage_claims, feature_ids, reject_unmapped, ManifestError};
pub use oracle::{capture, compare_trees};
pub use paths::{evidence_dir, lease_tmp, workspace_root};
pub use verify::{verify_deny_unverified, VerifyReport};
