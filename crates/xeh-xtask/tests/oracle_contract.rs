use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;

use xeh_xtask::reject_unmapped;

static TEST_LOCK: Mutex<()> = Mutex::new(());

fn test_lock() -> std::sync::MutexGuard<'static, ()> {
    TEST_LOCK.lock().unwrap_or_else(|err| err.into_inner())
}

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn xtask_bin() -> PathBuf {
    let mut path = PathBuf::from(env!("CARGO_BIN_EXE_xeh-xtask"));
    if !path.is_file() {
        path = workspace_root().join("target/debug/xeh-xtask");
    }
    path
}

fn lease_tmp() -> PathBuf {
    PathBuf::from("/mnt/TEMP/workspace/xehentai-rss/tmp/xehentai-20261029-main-13b130ba")
}

#[test]
fn manifest_rejects_unmapped_feature() {
    let manifest = r#"{"features":[{"id":"config.key.dir","source":"x","input":{},"expected":{},"target":"t","tests":[],"status":"unverified"}]}"#;
    let err = reject_unmapped(manifest, &["config.key.daemon", "config.key.dir"])
        .expect_err("a manifest missing a required feature must be rejected");
    assert!(
        err.contains("config.key.daemon"),
        "rejection must name the missing feature, got {err}"
    );
}

#[test]
fn oracle_never_reads_user_home() {
    let _guard = test_lock();
    let root = lease_tmp().join("oracle-isolation-test");
    let decoy = root.join("decoy-home");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&decoy).unwrap();
    fs::write(decoy.join("SECRET_SENTINEL"), b"SENTINEL-DECOY-HOME").unwrap();
    fs::write(
        decoy.join("config.py"),
        b"raise SystemExit('DECOY_CONFIG_IMPORTED')\n",
    )
    .unwrap();
    fs::write(decoy.join("h.json"), b"{\"marker\":\"decoy-session\"}\n").unwrap();
    fs::write(decoy.join(".ehentai.cookie"), b"decoy-id,decoy-hash\n").unwrap();

    let bin = xtask_bin();
    let mut cmd = Command::new(&bin);
    cmd.current_dir(workspace_root())
        .env("HOME", &decoy)
        .env("USERPROFILE", &decoy)
        .env("XEH_SECRET_SENTINEL", "sentinel-value-not-for-receipt")
        .env("CARGO_TARGET_DIR", "/mnt/TEMP/workspace/xehentai-rss/targets/slot-1/target")
        .env("RUSTUP_HOME", "/home/wow/.rustup")
        .args(["oracle", "capture", "--suite", "isolation"]);
    let out = cmd.output().expect("spawn xtask");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "isolation capture must succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let combined = format!("{stdout}{stderr}");
    assert!(
        !combined.contains("SENTINEL-DECOY-HOME"),
        "oracle output leaked the decoy sentinel\n{combined}"
    );
    assert!(
        !combined.contains("sentinel-value-not-for-receipt"),
        "receipt serialized an unlisted environment value\n{combined}"
    );
    assert!(
        !decoy.join("ISOLATION_BREACH").exists(),
        "decoy home was written by the upstream process"
    );

    let mut broken = Command::new(&bin);
    broken
        .current_dir(workspace_root())
        .env("HOME", &decoy)
        .env("USERPROFILE", &decoy)
        .env("XEH_ISOLATION_BREAK", "1")
        .env("CARGO_TARGET_DIR", "/mnt/TEMP/workspace/xehentai-rss/targets/slot-1/target")
        .env("RUSTUP_HOME", "/home/wow/.rustup")
        .args(["oracle", "capture", "--suite", "isolation"]);
    let broken_out = broken.output().expect("spawn broken isolation");
    assert!(
        !broken_out.status.success(),
        "audit must fail closed when a forbidden root is read\n{}",
        String::from_utf8_lossy(&broken_out.stderr)
    );
}

#[test]
fn verify_deny_unverified_fails_closed() {
    let _guard = test_lock();
    let bin = xtask_bin();
    let out = Command::new(&bin)
        .current_dir(workspace_root())
        .env("HOME", "/home/wow")
        .env("RUSTUP_HOME", "/home/wow/.rustup")
        .env("CARGO_TARGET_DIR", "/mnt/TEMP/workspace/xehentai-rss/targets/slot-1/target")
        .args(["verify", "--all", "--deny-unverified"])
        .output()
        .expect("spawn verify");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        !out.status.success(),
        "verify --deny-unverified must fail while application behavior is pending\n{combined}"
    );
    assert!(
        combined.contains("fail-closed"),
        "verify must report fail-closed, got:\n{combined}"
    );
    assert!(
        combined.contains("implementation_accepted=0"),
        "verify must show that no application behavior is accepted, got:\n{combined}"
    );
}

#[test]
fn capture_suite_all_records_original_and_stays_pending() {
    let _guard = test_lock();
    let root = lease_tmp().join("oracle-capture-test");
    let decoy = root.join("decoy-home");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&decoy).unwrap();
    fs::write(decoy.join("SECRET_SENTINEL"), b"SENTINEL-DECOY-HOME").unwrap();

    let bin = xtask_bin();
    let out = Command::new(&bin)
        .current_dir(workspace_root())
        .env("HOME", &decoy)
        .env("USERPROFILE", &decoy)
        .env("XEH_SECRET_SENTINEL", "sentinel-value-not-for-receipt")
        .env("CARGO_TARGET_DIR", "/mnt/TEMP/workspace/xehentai-rss/targets/slot-1/target")
        .env("RUSTUP_HOME", "/home/wow/.rustup")
        .args(["oracle", "capture", "--suite", "all"])
        .output()
        .expect("spawn capture");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let combined = format!("{stdout}{stderr}");
    assert!(
        out.status.success(),
        "capture --suite all must succeed\nstdout:\n{stdout}\nstderr:\n{stderr}"
    );
    assert!(!combined.contains("SENTINEL-DECOY-HOME"));
    assert!(!combined.contains("sentinel-value-not-for-receipt"));

    let manifest_text = fs::read_to_string(workspace_root().join("compat/manifest.json"))
        .expect("manifest");
    let manifest: serde_json::Value = serde_json::from_str(&manifest_text).unwrap();
    let features = manifest["features"].as_array().expect("features");
    let ids: Vec<&str> = features
        .iter()
        .map(|item| item["id"].as_str().unwrap())
        .collect();
    let count = |prefix: &str| ids.iter().filter(|id| id.starts_with(prefix)).count();
    assert_eq!(count("config.key."), 25, "config keys: {ids:?}");
    assert_eq!(count("rpc.method."), 13);
    assert_eq!(count("task.state."), 10);
    assert!(count("error.") >= 20, "error constants");
    assert!(count("cli.option.") >= 20, "cli options");
    assert!(count("i18n.en_us.key.") > 0);
    assert!(ids.contains(&"config.key.daemon"));
    assert!(ids.contains(&"delta.scan-downloaded-comparison"));
    assert!(ids.contains(&"behavior.sec9.webui"));
    for item in features {
        let status = item["status"].as_str().unwrap();
        assert_ne!(status, "implemented");
        assert_ne!(status, "accepted");
        for field in ["source", "input", "expected", "target", "tests", "status"] {
            assert!(item.get(field).is_some(), "missing {field} on {}", item["id"]);
        }
    }
    let config: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(workspace_root().join("compat/golden/config.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(config["keys"]["daemon"], false);
    assert_eq!(config["keys"]["proxy"], serde_json::json!([]));
    assert!(config["keys"]["rpc_port"].is_null());
    assert_eq!(config["count"], 25);

    let deltas = fs::read_to_string(workspace_root().join("docs/compatibility-deltas.md")).unwrap();
    assert!(deltas.contains("delta.scan-downloaded-comparison"));
    assert!(deltas.contains("待批准"));
    assert!(!deltas.contains("SENTINEL-DECOY-HOME"));

    let check = Command::new(&bin)
        .current_dir(workspace_root())
        .env("HOME", "/home/wow")
        .env("RUSTUP_HOME", "/home/wow/.rustup")
        .env("CARGO_TARGET_DIR", "/mnt/TEMP/workspace/xehentai-rss/targets/slot-1/target")
        .args(["manifest", "check"])
        .output()
        .expect("manifest check");
    assert!(
        check.status.success(),
        "manifest check failed\n{}",
        String::from_utf8_lossy(&check.stderr)
    );
    let verify = Command::new(&bin)
        .current_dir(workspace_root())
        .env("HOME", "/home/wow")
        .env("RUSTUP_HOME", "/home/wow/.rustup")
        .env("CARGO_TARGET_DIR", "/mnt/TEMP/workspace/xehentai-rss/targets/slot-1/target")
        .args(["verify", "--all", "--deny-unverified"])
        .output()
        .expect("verify");
    let verify_text = format!(
        "{}{}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
    assert!(!verify.status.success(), "{verify_text}");
    assert!(verify_text.contains("fail-closed"));
    assert!(verify_text.contains("implementation_accepted=0"));
}
