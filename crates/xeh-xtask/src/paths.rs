use std::path::{Path, PathBuf};

pub const UPSTREAM_APP_REPO: &str = "fffonion/xeHentai";
pub const UPSTREAM_APP_REV: &str = "43fc55c0c662f195d048becf6276a26ce06b09d9";
pub const UPSTREAM_WEBUI_REPO: &str = "fffonion/xeHentai-webui";
pub const UPSTREAM_WEBUI_REV: &str = "4e3738ef463fef344e96df613de9a81b4cda4b4e";
pub const BASE_COMMIT: &str = "7686fa200bc82eafae79bd078a14867a066243ee";

pub fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

pub fn lease_tmp() -> PathBuf {
    if let Some(value) = std::env::var_os("XEH_LEASE_TMP") {
        return PathBuf::from(value);
    }
    PathBuf::from("/mnt/TEMP/workspace/xehentai-rss/tmp/xehentai-20261029-main-13b130ba")
}

pub fn evidence_dir() -> PathBuf {
    lease_tmp().join("evidence")
}

pub fn python_executable() -> PathBuf {
    if let Some(value) = std::env::var_os("XEH_PYTHON") {
        return PathBuf::from(value);
    }
    let venv = lease_tmp().join("py/venv/bin/python");
    if venv.is_file() {
        return venv;
    }
    PathBuf::from("python3")
}

pub fn scripts_dir() -> PathBuf {
    workspace_root().join("compat/oracle")
}

pub fn upstream_checkout(root: &Path) -> PathBuf {
    root.join("compat/upstream/xeHentai")
}

pub fn extracted_app_tree() -> PathBuf {
    lease_tmp().join("upstream/src/fffonion-xeHentai-43fc55c")
}

pub fn extracted_webui_tree() -> PathBuf {
    lease_tmp().join("upstream/src/fffonion-xeHentai-webui-4e3738e")
}
