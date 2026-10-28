use std::path::{Path, PathBuf};

pub const UPSTREAM_APP_REPO: &str = "fffonion/xeHentai";
pub const UPSTREAM_APP_REV: &str = "43fc55c0c662f195d048becf6276a26ce06b09d9";
pub const UPSTREAM_WEBUI_REPO: &str = "fffonion/xeHentai-webui";
pub const UPSTREAM_WEBUI_REV: &str = "4e3738ef463fef344e96df613de9a81b4cda4b4e";
pub const BASE_COMMIT: &str = "7686fa200bc82eafae79bd078a14867a066243ee";

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn lease_tmp() -> Result<PathBuf, String> {
    match std::env::var_os("XEH_LEASE_TMP") {
        Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
        _ => Err(
            "XEH_LEASE_TMP is required; set it to the execution lease temporary directory"
                .to_string(),
        ),
    }
}

pub fn evidence_dir() -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("XEH_EVIDENCE_DIR") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    Ok(lease_tmp()?.join("evidence-address-r1"))
}

pub fn python_executable() -> PathBuf {
    if let Some(value) = std::env::var_os("XEH_PYTHON") {
        if !value.is_empty() {
            return PathBuf::from(value);
        }
    }
    PathBuf::from("python3")
}

pub fn scripts_dir() -> PathBuf {
    workspace_root().join("compat/oracle")
}

pub fn upstream_checkout(root: &Path) -> PathBuf {
    root.join("compat/upstream/xeHentai")
}

pub fn authoritative_git(name: &str) -> Result<PathBuf, String> {
    Ok(lease_tmp()?.join("upstream").join(format!("{name}.git")))
}
