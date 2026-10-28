use std::fs;
use std::os::unix::fs::PermissionsExt;
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
    PathBuf::from(
        std::env::var("XEH_LEASE_TMP").expect("XEH_LEASE_TMP is required for oracle tests"),
    )
}

fn python_bin() -> PathBuf {
    PathBuf::from(std::env::var("XEH_PYTHON").expect("XEH_PYTHON is required for oracle tests"))
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
        .env("XEH_LEASE_TMP", lease_tmp())
        .env("XEH_PYTHON", python_bin())
        .env("CARGO_TARGET_DIR", target_dir())
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
        .env("XEH_LEASE_TMP", lease_tmp())
        .env("XEH_PYTHON", python_bin())
        .env("CARGO_TARGET_DIR", target_dir())
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
        .env("CARGO_TARGET_DIR", target_dir())
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
        .env("XEH_LEASE_TMP", lease_tmp())
        .env("XEH_PYTHON", python_bin())
        .env("CARGO_TARGET_DIR", target_dir())
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

    let manifest_text =
        fs::read_to_string(workspace_root().join("compat/manifest.json")).expect("manifest");
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
            assert!(
                item.get(field).is_some(),
                "missing {field} on {}",
                item["id"]
            );
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
        .env("CARGO_TARGET_DIR", target_dir())
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
        .env("CARGO_TARGET_DIR", target_dir())
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

fn target_dir() -> String {
    std::env::var("CARGO_TARGET_DIR").expect("CARGO_TARGET_DIR is required for oracle tests")
}

fn command(bin: &std::path::Path) -> Command {
    let mut cmd = Command::new(bin);
    cmd.current_dir(workspace_root())
        .env("XEH_LEASE_TMP", lease_tmp())
        .env("XEH_PYTHON", python_bin())
        .env("CARGO_TARGET_DIR", target_dir())
        .env("RUSTUP_HOME", "/home/wow/.rustup");
    cmd
}

fn combined(out: &std::process::Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn copy_tree(from: &std::path::Path, to: &std::path::Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let dest = to.join(entry.file_name());
        let kind = entry.file_type().unwrap();
        if kind.is_dir() {
            copy_tree(&entry.path(), &dest);
        } else if kind.is_file() {
            fs::copy(entry.path(), dest).unwrap();
        }
    }
}

#[test]
fn capture_requires_explicit_lease_tmp() {
    let bin = xtask_bin();
    let out = Command::new(&bin)
        .current_dir(workspace_root())
        .env_remove("XEH_LEASE_TMP")
        .env("CARGO_TARGET_DIR", target_dir())
        .env("RUSTUP_HOME", "/home/wow/.rustup")
        .args(["oracle", "capture", "--suite", "isolation"])
        .output()
        .expect("spawn");
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "missing XEH_LEASE_TMP must fail closed\n{text}"
    );
    assert!(
        text.contains("XEH_LEASE_TMP"),
        "error must name the missing configuration, got:\n{text}"
    );
    assert!(
        !text.contains("xehentai-20261029-main-13b130ba"),
        "must not fall back to a hardcoded lease path\n{text}"
    );
}

#[test]
fn missing_python_dependency_is_reported() {
    let _guard = test_lock();
    let root = lease_tmp().join("py-missing-dep");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let fake = root.join("python");
    fs::write(&fake, "#!/bin/sh\necho boom >&2\nexit 1\n").unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let out = command(&xtask_bin())
        .env("XEH_PYTHON", &fake)
        .env("HOME", "/home/wow")
        .args(["oracle", "capture", "--suite", "isolation"])
        .output()
        .expect("spawn");
    let text = combined(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("requests[socks]"),
        "missing dependency must name requests[socks], got:\n{text}"
    );
    assert!(
        !text.contains("isolation-ok"),
        "a broken interpreter must not be treated as a successful oracle\n{text}"
    );
}

#[test]
fn tampered_upstream_rejected_before_import() {
    let _guard = test_lock();
    let root = lease_tmp().join("tampered-upstream");
    let _ = fs::remove_dir_all(&root);
    let candidate = root.join("xeHentai");
    copy_tree(
        &workspace_root().join("compat/upstream/xeHentai"),
        &candidate,
    );
    let const_py = candidate.join("xeHentai/const.py");
    let original =
        fs::read(workspace_root().join("compat/upstream/xeHentai/xeHentai/const.py")).unwrap();
    let mut tampered = b"raise SystemExit('TAMPERED_CONST_IMPORTED')\n".to_vec();
    tampered.extend_from_slice(&original);
    fs::write(&const_py, &tampered).unwrap();
    let emit = root.join("emit");
    fs::create_dir_all(&emit).unwrap();
    let out = command(&xtask_bin())
        .env("HOME", "/home/wow")
        .env("XEH_UPSTREAM_CANDIDATE", &candidate)
        .env("XEH_EMIT_ROOT", &emit)
        .args(["oracle", "capture", "--suite", "isolation"])
        .output()
        .expect("spawn");
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "tampered upstream must be rejected before import\n{text}"
    );
    assert!(
        text.contains("byte mismatch") || text.contains("before import"),
        "rejection must be a source-byte failure, got:\n{text}"
    );
    assert!(
        !text.contains("TAMPERED_CONST_IMPORTED"),
        "tampered module must not be imported\n{text}"
    );
    assert!(
        !emit.join("compat/manifest.json").exists(),
        "rejected capture must not write a success baseline"
    );
    let frozen =
        fs::read(workspace_root().join("compat/upstream/xeHentai/xeHentai/const.py")).unwrap();
    assert_eq!(
        frozen, original,
        "frozen upstream const.py must stay unchanged"
    );
}

#[test]
fn decoy_child_forbidden_open_rejects_capture() {
    let _guard = test_lock();
    let root = lease_tmp().join("decoy-child");
    let decoy = root.join("decoy-home");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&decoy).unwrap();
    fs::write(decoy.join("SECRET_SENTINEL"), b"SENTINEL-DECOY-HOME").unwrap();
    let emit = root.join("emit");
    fs::create_dir_all(&emit).unwrap();
    let out = command(&xtask_bin())
        .env("HOME", &decoy)
        .env("USERPROFILE", &decoy)
        .env("XEH_DECOY_CHILD", "1")
        .env("XEH_EMIT_ROOT", &emit)
        .args(["oracle", "capture", "--suite", "isolation"])
        .output()
        .expect("spawn");
    let text = combined(&out);
    assert!(
        !out.status.success(),
        "child decoy open must fail the top-level capture\n{text}"
    );
    assert!(
        text.contains("forbidden"),
        "failure must name the forbidden-path audit, got:\n{text}"
    );
    assert!(!text.contains("SENTINEL-DECOY-HOME"), "{text}");
    assert!(!emit.join("compat/manifest.json").exists());
}

#[test]
fn missing_audit_fails_closed() {
    let _guard = test_lock();
    let emit = lease_tmp().join("missing-audit/emit");
    let _ = fs::remove_dir_all(lease_tmp().join("missing-audit"));
    fs::create_dir_all(&emit).unwrap();
    let out = command(&xtask_bin())
        .env("HOME", "/home/wow")
        .env("XEH_SUPPRESS_AUDIT", "1")
        .env("XEH_EMIT_ROOT", &emit)
        .args(["oracle", "capture", "--suite", "isolation"])
        .output()
        .expect("spawn");
    let text = combined(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("audit is missing"),
        "missing audit must fail closed, got:\n{text}"
    );
    assert!(!emit.join("compat/manifest.json").exists());
}

#[test]
fn malformed_audit_fails_closed() {
    let _guard = test_lock();
    let emit = lease_tmp().join("malformed-audit/emit");
    let _ = fs::remove_dir_all(lease_tmp().join("malformed-audit"));
    fs::create_dir_all(&emit).unwrap();
    let out = command(&xtask_bin())
        .env("HOME", "/home/wow")
        .env("XEH_MALFORM_AUDIT", "1")
        .env("XEH_EMIT_ROOT", &emit)
        .args(["oracle", "capture", "--suite", "isolation"])
        .output()
        .expect("spawn");
    let text = combined(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("audit is malformed"),
        "malformed audit must fail closed, got:\n{text}"
    );
    assert!(!emit.join("compat/manifest.json").exists());
}

#[test]
fn missing_child_audit_fails_closed() {
    let _guard = test_lock();
    let root = lease_tmp().join("missing-child-audit");
    let decoy = root.join("decoy-home");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&decoy).unwrap();
    fs::write(decoy.join("SECRET_SENTINEL"), b"SENTINEL-DECOY-HOME").unwrap();
    let emit = root.join("emit");
    fs::create_dir_all(&emit).unwrap();
    let out = command(&xtask_bin())
        .env("HOME", &decoy)
        .env("USERPROFILE", &decoy)
        .env("XEH_DECOY_CHILD", "1")
        .env("XEH_SUPPRESS_CHILD_AUDIT", "1")
        .env("XEH_EMIT_ROOT", &emit)
        .args(["oracle", "capture", "--suite", "isolation"])
        .output()
        .expect("spawn");
    let text = combined(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("child audit is missing"),
        "missing child audit must fail closed, got:\n{text}"
    );
    assert!(!text.contains("SENTINEL-DECOY-HOME"), "{text}");
    assert!(!emit.join("compat/manifest.json").exists());
}

#[test]
fn output_write_failure_fails_closed() {
    let _guard = test_lock();
    let root = lease_tmp().join("output-write-fail");
    let _ = fs::remove_dir_all(&root);
    let capture_out = root.join("capture-out");
    fs::create_dir_all(&capture_out).unwrap();
    fs::create_dir_all(capture_out.join("stdout.txt")).unwrap();
    let emit = root.join("emit");
    fs::create_dir_all(&emit).unwrap();
    let out = command(&xtask_bin())
        .env("HOME", "/home/wow")
        .env("XEH_CAPTURE_OUT", &capture_out)
        .env("XEH_EMIT_ROOT", &emit)
        .args(["oracle", "capture", "--suite", "isolation"])
        .output()
        .expect("spawn");
    let text = combined(&out);
    assert!(!out.status.success(), "{text}");
    assert!(
        text.contains("cannot write oracle stdout"),
        "stdout write failure must fail closed, got:\n{text}"
    );
    assert!(!emit.join("compat/manifest.json").exists());
}

fn feature<'a>(manifest: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    manifest["features"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["id"].as_str() == Some(id))
        .unwrap_or_else(|| panic!("missing feature {id}"))
}

#[test]
fn capture_splits_aliases_coverage_and_normalizes_receipts() {
    let _guard = test_lock();
    let root = lease_tmp().join("capture-claims");
    let decoy = root.join("decoy-home");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&decoy).unwrap();
    fs::write(decoy.join("SECRET_SENTINEL"), b"SENTINEL-DECOY-HOME").unwrap();
    let emit = root.join("emit");
    fs::create_dir_all(&emit).unwrap();
    let out = command(&xtask_bin())
        .env("HOME", &decoy)
        .env("USERPROFILE", &decoy)
        .env("XEH_SECRET_SENTINEL", "sentinel-value-not-for-receipt")
        .env("XEH_CANDIDATE_OVERRIDE", "not-a-real-commit")
        .env("XEH_EMIT_ROOT", &emit)
        .args(["oracle", "capture", "--suite", "all"])
        .output()
        .expect("spawn capture");
    let text = combined(&out);
    assert!(out.status.success(), "capture must succeed\n{text}");
    assert!(!text.contains("SENTINEL-DECOY-HOME"), "{text}");
    let manifest_line = text
        .lines()
        .find(|line| line.starts_with("manifest="))
        .expect("capture must print manifest path");
    let manifest_path = PathBuf::from(manifest_line.trim_start_matches("manifest="));
    let manifest: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
    let username = feature(&manifest, "cli.option.--username");
    let short = feature(&manifest, "cli.option.-u");
    assert_eq!(username["status"], "oracle_characterized", "{username}");
    assert_eq!(short["status"], "oracle_characterized");
    assert_eq!(username["expected"]["observed"]["executed"], true);
    assert_eq!(
        username["expected"]["observed"]["parsed_username"],
        short["expected"]["observed"]["parsed_username"]
    );
    let spellings = Command::new(python_bin())
        .arg("-c")
        .arg("import ast,json,sys\ntree=ast.parse(open(sys.argv[1],'rb').read())\nfound=[]\nfor node in ast.walk(tree):\n    func=getattr(node,'func',None)\n    if isinstance(node, ast.Call) and isinstance(func, ast.Attribute) and func.attr=='add_argument':\n        for arg in node.args:\n            if isinstance(arg, ast.Constant) and isinstance(arg.value, str) and arg.value.startswith('-'):\n                found.append(arg.value)\nprint(json.dumps(found))\n")
        .arg(workspace_root().join("compat/upstream/xeHentai/xeHentai/cli.py"))
        .output()
        .expect("spellings");
    assert!(
        spellings.status.success(),
        "{}",
        String::from_utf8_lossy(&spellings.stderr)
    );
    let required: Vec<String> = serde_json::from_slice(&spellings.stdout).unwrap();
    assert!(required.iter().any(|item| item == "--username"));
    let ids: Vec<String> = manifest["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["id"].as_str().unwrap().to_string())
        .collect();
    for spelling in &required {
        let id = format!("cli.option.{spelling}");
        assert!(ids.iter().any(|item| item == &id), "missing {id}");
        let item = feature(&manifest, &id);
        let executed = item["expected"]["observed"]["executed"].as_bool() == Some(true);
        if executed {
            assert_eq!(item["status"], "oracle_characterized", "{id}");
        } else {
            assert_eq!(item["status"], "unverified", "{id}");
            assert_ne!(item["expected"]["observed"]["present"], true);
        }
    }
    let rpc = feature(&manifest, "behavior.sec4.rpc-methods");
    let characterized = ids
        .iter()
        .filter(|id| id.starts_with("rpc.method."))
        .filter(|id| feature(&manifest, id)["status"] == "oracle_characterized")
        .count();
    let failed = ids
        .iter()
        .filter(|id| id.starts_with("rpc.method."))
        .filter(|id| feature(&manifest, id)["status"] == "oracle_failed")
        .count();
    assert_eq!(
        feature(&manifest, "rpc.method.xeH.loginExhentai")["status"],
        "oracle_failed"
    );
    assert_eq!(
        rpc["expected"]["observed"]["methods_characterized"].as_u64(),
        Some(characterized as u64)
    );
    assert_eq!(
        rpc["expected"]["observed"]["methods_failed"].as_u64(),
        Some(failed as u64)
    );
    assert_ne!(rpc["status"], "oracle_characterized");
    assert_ne!(
        rpc["expected"]["observed"]["methods_executed"].as_u64(),
        Some(13)
    );
    assert_eq!(
        feature(&manifest, "behavior.sec4.proxy-schemes.https")["status"],
        "unverified"
    );
    assert_eq!(
        feature(&manifest, "behavior.sec4.proxy-schemes.socks")["status"],
        "unverified"
    );
    assert_ne!(
        feature(&manifest, "behavior.sec4.proxy-schemes")["status"],
        "oracle_characterized"
    );
    let receipt_line = text
        .lines()
        .find(|line| line.starts_with("receipt="))
        .unwrap();
    let receipt: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(receipt_line.trim_start_matches("receipt=")).unwrap(),
    )
    .unwrap();
    let head = Command::new("git")
        .args([
            "-C",
            workspace_root().to_str().unwrap(),
            "rev-parse",
            "HEAD",
        ])
        .output()
        .unwrap();
    let head = String::from_utf8_lossy(&head.stdout).trim().to_string();
    assert_eq!(receipt["candidate"], head);
    assert_ne!(receipt["candidate"], "not-a-real-commit");
    let fixture = receipt["cwd"].as_str().unwrap().to_string();
    assert_eq!(receipt["env"]["USERPROFILE"], fixture);
    assert_eq!(receipt["env"]["PWD"], fixture);
    assert!(receipt["env"].get("XEH_SECRET_SENTINEL").is_none());
    assert!(receipt["processes"].as_array().unwrap().len() > 1);
    assert_eq!(
        receipt["upstream"]["app"]["verification"]["result"],
        "match"
    );
    let raw = fs::read_to_string(receipt["result"].as_str().unwrap()).unwrap();
    assert!(raw.contains(&fixture));
    let golden_dir = manifest_path.parent().unwrap().join("golden");
    let golden = fs::read_to_string(golden_dir.join("runtime_config.json")).unwrap();
    assert!(!golden.contains(&fixture));
    assert!(golden.contains("<fixture-root>"));
    let rpc_golden = fs::read_to_string(golden_dir.join("rpc.json")).unwrap();
    assert!(rpc_golden.contains("<http-date>"));
    assert!(!rpc_golden.contains(" GMT"));
    let changed = root.join("changed-golden");
    copy_tree(&golden_dir, &changed);
    let config_path = changed.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&config_path).unwrap()).unwrap();
    config["keys"]["daemon"] = serde_json::json!(true);
    fs::write(config_path, serde_json::to_string_pretty(&config).unwrap()).unwrap();
    let compared = command(&xtask_bin())
        .args([
            "oracle",
            "compare",
            "--left",
            golden_dir.to_str().unwrap(),
            "--right",
            changed.to_str().unwrap(),
        ])
        .output()
        .expect("compare");
    let compared_text = combined(&compared);
    assert!(!compared.status.success(), "{compared_text}");
    assert!(compared_text.contains("daemon"), "{compared_text}");
    let emit_b = root.join("emit-b");
    fs::create_dir_all(&emit_b).unwrap();
    let second = command(&xtask_bin())
        .env("HOME", &decoy)
        .env("USERPROFILE", &decoy)
        .env("XEH_SECRET_SENTINEL", "sentinel-value-not-for-receipt")
        .env("XEH_CANDIDATE_OVERRIDE", "not-a-real-commit")
        .env("XEH_EMIT_ROOT", &emit_b)
        .args(["oracle", "capture", "--suite", "all"])
        .output()
        .expect("second capture");
    let second_text = combined(&second);
    assert!(
        second.status.success(),
        "second capture must succeed\n{second_text}"
    );
    let equal = command(&xtask_bin())
        .args([
            "oracle",
            "compare",
            "--left",
            emit.join("compat/golden").to_str().unwrap(),
            "--right",
            emit_b.join("compat/golden").to_str().unwrap(),
        ])
        .output()
        .expect("compare captures");
    let equal_text = combined(&equal);
    assert!(
        equal.status.success(),
        "two captures must normalize to the same golden\n{equal_text}"
    );
    assert!(equal_text.contains("normalized-equal"), "{equal_text}");
}

#[test]
fn wrong_but_valid_audit_json_fails_closed() {
    let _guard = test_lock();
    let cases = [
        (
            "root-empty",
            Some("empty-object"),
            None,
            "audit schema is invalid",
        ),
        ("root-array", Some("array"), None, "audit schema is invalid"),
        (
            "root-missing-opens",
            Some("missing-opens"),
            None,
            "audit schema is invalid",
        ),
        (
            "child-empty",
            None,
            Some("child-empty-object"),
            "audit schema is invalid",
        ),
        (
            "child-opens-string",
            None,
            Some("child-opens-string"),
            "audit schema is invalid",
        ),
        (
            "child-opens-mixed",
            None,
            Some("child-opens-mixed"),
            "audit schema is invalid",
        ),
        (
            "processes-missing",
            None,
            Some("empty-object"),
            "child list schema is invalid",
        ),
        (
            "processes-null",
            None,
            Some("processes-null"),
            "child list schema is invalid",
        ),
        (
            "bad-item",
            None,
            Some("bad-item"),
            "child list schema is invalid",
        ),
        (
            "identity-mismatch",
            None,
            Some("identity-mismatch"),
            "child identity does not match audit",
        ),
        (
            "parent-mismatch",
            None,
            Some("parent-mismatch"),
            "child identity does not match audit",
        ),
        (
            "foreign-audit",
            None,
            Some("foreign-audit"),
            "outside this capture output",
        ),
    ];
    let mut failures = Vec::new();
    for (name, audit_shape, child_shape, needle) in cases {
        let emit = lease_tmp().join(format!("audit-shape-{name}/emit"));
        let _ = fs::remove_dir_all(lease_tmp().join(format!("audit-shape-{name}")));
        fs::create_dir_all(&emit).unwrap();
        let mut cmd = command(&xtask_bin());
        cmd.env("HOME", "/home/wow")
            .env("XEH_EMIT_ROOT", &emit)
            .args(["oracle", "capture", "--suite", "isolation"]);
        if let Some(shape) = audit_shape {
            cmd.env("XEH_AUDIT_SHAPE", shape);
        }
        if let Some(shape) = child_shape {
            cmd.env("XEH_CHILD_LIST_SHAPE", shape);
        }
        let out = cmd.output().expect("spawn shaped capture");
        let text = combined(&out);
        let published =
            emit.join("compat/manifest.json").exists() || emit.join("compat/golden").exists();
        if out.status.success() || published || !text.contains(needle) {
            failures.push(format!(
                "{name}: success={} published={published} missing `{needle}`\n{text}",
                out.status.success()
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "wrong-but-valid audit JSON must fail closed before publish:\n{}",
        failures.join("\n---\n")
    );
}

#[test]
fn normalize_keeps_unregistered_business_fields() {
    let _guard = test_lock();
    let root = lease_tmp().join("normalize-r5");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(&root).unwrap();
    let script = root.join("check.py");
    fs::write(&script, NORMALIZE_R5_CHECK).unwrap();
    let oracle = workspace_root().join("compat/oracle");
    let ran = Command::new(python_bin())
        .arg(&script)
        .arg(oracle.as_os_str())
        .arg(root.as_os_str())
        .env("PYTHONPATH", &oracle)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("normalize check");
    let text = combined(&ran);
    assert!(ran.status.success(), "normalize check failed\n{text}");
    let report = report_lines(&text);
    let mut failures = Vec::new();
    if report.get("REGISTERED_DATE").map(String::as_str) != Some("<http-date>") {
        failures.push(format!("registered date was not normalized: {report:?}"));
    }
    if report.get("BUSINESS_DATE").map(String::as_str) != Some("Thu, 01 Jan 2026 00:00:00 GMT") {
        failures.push(format!("business date was rewritten: {report:?}"));
    }
    if report.get("SIBLING_DATE").map(String::as_str) != Some("Thu, 01 Jan 2026 00:00:00 GMT") {
        failures.push(format!(
            "unregistered sibling date was rewritten: {report:?}"
        ));
    }
    let url = report.get("BUSINESS_URL").map(String::as_str).unwrap_or("");
    if !url.contains("/tmp/fixture-a") || !url.contains("127.0.0.1:18011") {
        failures.push(format!("business URL was rewritten: {url}"));
    }
    if report.get("FILEPATH").map(String::as_str) != Some("<fixture-root>") {
        failures.push(format!(
            "registered filepath was not normalized: {report:?}"
        ));
    }
    if report.get("REFERER").map(String::as_str)
        != Some("http://127.0.0.1:<ephemeral-port>/browse.php")
    {
        failures.push(format!("registered referer was not normalized: {report:?}"));
    }
    if report.get("CASE_BUSINESS_DATE").map(String::as_str) != Some("Thu, 01 Jan 2026 00:00:00 GMT")
    {
        failures.push(format!("case business date was rewritten: {report:?}"));
    }
    let applied = report.get("APPLIED").map(String::as_str).unwrap_or("");
    if !applied.contains("/expected/observed/headers/date")
        || !applied.contains("rpc.method.xeH.getInfo")
    {
        failures.push(format!("applied rules missing registered path: {applied}"));
    }
    if applied.contains("business_url")
        || applied.contains("/json/result/url")
        || applied.contains("/body")
    {
        failures.push(format!("applied rules include a business path: {applied}"));
    }
    let pair1 = command(&xtask_bin())
        .args([
            "oracle",
            "compare",
            "--left",
            root.join("pair1-left").to_str().unwrap(),
            "--right",
            root.join("pair1-right").to_str().unwrap(),
        ])
        .output()
        .expect("compare business mutation");
    let pair1_text = combined(&pair1);
    if pair1.status.success() {
        failures.push(format!("business mutation compared equal\n{pair1_text}"));
    }
    let pair2 = command(&xtask_bin())
        .args([
            "oracle",
            "compare",
            "--left",
            root.join("pair2-left").to_str().unwrap(),
            "--right",
            root.join("pair2-right").to_str().unwrap(),
        ])
        .output()
        .expect("compare registered volatile fields");
    let pair2_text = combined(&pair2);
    if !pair2.status.success() || !pair2_text.contains("normalized-equal") {
        failures.push(format!(
            "registered volatile fields did not compare equal\n{pair2_text}"
        ));
    }
    assert!(
        failures.is_empty(),
        "explicit-path normalize failed:\n{}\n{text}",
        failures.join("\n---\n")
    );
}

fn report_lines(text: &str) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=') {
            if key.chars().all(|ch| ch.is_ascii_uppercase() || ch == '_') {
                out.insert(key.to_string(), value.to_string());
            }
        }
    }
    out
}

const NORMALIZE_R5_CHECK: &str = r#"
import json, os, sys
sys.path.insert(0, sys.argv[1])
import harness

root_a = "/tmp/fixture-a"
root_b = "/tmp/fixture-b"
port_a = 18011
port_b = 18012
date_a = "Tue, 29 Oct 2026 01:02:03 GMT"
date_b = "Wed, 30 Oct 2026 04:05:06 GMT"
business_date = "Thu, 01 Jan 2026 00:00:00 GMT"
other_business_date = "Fri, 02 Jan 2026 00:00:00 GMT"

def payload(root, port, http_date, posted, business_date, business_url):
    observed = {
        "headers": {"date": http_date, "server": "xeHentai/2.023"},
        "date": business_date,
        "business_headers": {"date": business_date},
        "json": {"result": {"url": business_url, "posted": posted}},
        "body": business_url,
    }
    cases = {
        "rpc.method.xeH.getInfo": {
            "id": "rpc.method.xeH.getInfo",
            "expected": {"observed": observed},
        },
        "behavior.sec4.config-defaults": {
            "id": "behavior.sec4.config-defaults",
            "expected": {"observed": {"filepath": root, "business_url": business_url}},
        },
        "behavior.sec4.proxy-pool": {
            "id": "behavior.sec4.proxy-pool",
            "expected": {"observed": {"seen": [{"referer": "http://127.0.0.1:%d/browse.php" % port}]}},
        },
    }
    golden = {
        "rpc": {"getInfo": observed},
        "runtime_config": {"filepath": root, "business_url": business_url},
        "glype": {"seen": [{"referer": "http://127.0.0.1:%d/browse.php" % port}]},
    }
    return cases, golden

def write_golden(path, golden):
    os.makedirs(path, exist_ok=True)
    for name, value in golden.items():
        with open(os.path.join(path, name + ".json"), "w") as handle:
            json.dump(value, handle, sort_keys=True)

out = sys.argv[2]
url_a = "http://127.0.0.1:%d/item?path=%s/a" % (port_a, root_a)
url_b = "http://127.0.0.1:%d/item?path=%s/a" % (port_b, root_b)
cases_a, golden_a = payload(root_a, port_a, date_a, business_date, business_date, url_a)
cases_b, golden_b = payload(root_b, port_b, date_b, business_date, other_business_date, url_b)
norm_cases, norm_golden, applied = harness.normalize_capture(cases_a, golden_a, root_a, [port_a])
_, golden_right, _ = harness.normalize_capture(cases_b, golden_b, root_b, [port_b])
write_golden(os.path.join(out, "pair1-left"), norm_golden)
write_golden(os.path.join(out, "pair1-right"), golden_right)

stable = "https://synthetic.invalid/gallery/1"
cases_c, golden_c = payload(root_a, port_a, date_a, business_date, business_date, stable)
cases_d, golden_d = payload(root_b, port_b, date_b, business_date, business_date, stable)
_, golden_c, _ = harness.normalize_capture(cases_c, golden_c, root_a, [port_a])
_, golden_d, _ = harness.normalize_capture(cases_d, golden_d, root_b, [port_b])
write_golden(os.path.join(out, "pair2-left"), golden_c)
write_golden(os.path.join(out, "pair2-right"), golden_d)

info = norm_golden["rpc"]["getInfo"]
print("REGISTERED_DATE=" + info["headers"]["date"])
print("BUSINESS_DATE=" + info["date"])
print("SIBLING_DATE=" + info["business_headers"]["date"])
print("BUSINESS_URL=" + info["json"]["result"]["url"])
print("FILEPATH=" + norm_golden["runtime_config"]["filepath"])
print("REFERER=" + norm_golden["glype"]["seen"][0]["referer"])
print("CASE_BUSINESS_DATE=" + norm_cases["rpc.method.xeH.getInfo"]["expected"]["observed"]["date"])
print("CASE_REGISTERED_DATE=" + norm_cases["rpc.method.xeH.getInfo"]["expected"]["observed"]["headers"]["date"])
print("APPLIED=" + json.dumps(applied, sort_keys=True))
"#;
