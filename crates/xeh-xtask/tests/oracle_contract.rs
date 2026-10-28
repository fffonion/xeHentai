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
