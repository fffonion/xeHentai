//! xeHentai compatibility manifest and oracle command library.
//!
//! Application behavior is not implemented here. This crate only checks the
//! acceptance manifest and runs the original Python program as an oracle.

mod config;
mod manifest;
mod oracle;
mod paths;
mod task_api;
mod verify;

pub use config::{run_diff, run_suite};
pub use manifest::{check_coverage_claims, feature_ids, reject_unmapped, ManifestError};
pub use oracle::{capture, compare_trees};
pub use paths::{evidence_dir, lease_tmp, workspace_root};
pub use task_api::{run_state_suite, run_task_diff};
pub use verify::{verify_deny_unverified, VerifyReport};

pub fn rss_test() -> Result<(), String> {
    let value = xeh_host::run_app().map_err(|error| error.to_string())?;
    println!("{value}");
    Ok(())
}
