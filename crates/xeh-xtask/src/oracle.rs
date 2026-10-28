use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::manifest::{check_manifest, count_statuses};
use crate::paths::{
    evidence_dir, extracted_app_tree, extracted_webui_tree, lease_tmp, python_executable,
    scripts_dir, upstream_checkout, workspace_root, BASE_COMMIT, UPSTREAM_APP_REPO,
    UPSTREAM_APP_REV, UPSTREAM_WEBUI_REPO, UPSTREAM_WEBUI_REV,
};

const RECEIPT_ENV: &[&str] = &[
    "HOME",
    "USERPROFILE",
    "LANG",
    "LC_ALL",
    "PWD",
    "CARGO_TARGET_DIR",
    "PYTHONDONTWRITEBYTECODE",
    "PYTHONNOUSERSITE",
    "XEH_FIXTURE_ROOT",
    "XEH_UPSTREAM",
    "XEH_SUITE",
    "XEH_LEASE_TMP",
];

pub fn capture(suite: &str) -> Result<(), String> {
    let root = workspace_root();
    let upstream = ensure_upstream(&root)?;
    let stamp = unix_stamp();
    let fixture = lease_tmp().join(format!("fixtures/{suite}-{stamp}"));
    fs::create_dir_all(&fixture).map_err(|err| format!("cannot create fixture: {err}"))?;
    fs::create_dir_all(evidence_dir()).map_err(|err| format!("cannot create evidence dir: {err}"))?;

    let forbidden = forbidden_roots(&fixture);
    let out_dir = evidence_dir().join(format!("capture-{suite}-{stamp}"));
    fs::create_dir_all(&out_dir).map_err(|err| format!("cannot create capture dir: {err}"))?;
    let runtime = fixture.join("pkg");
    copy_tree(&upstream, &runtime)?;
    let scripts = scripts_dir();
    for name in ["loader.py", "harness.py", "contract.py"] {
        let src = scripts.join(name);
        if !src.is_file() {
            return Err(format!("oracle script is missing: {}", src.display()));
        }
        fs::copy(&src, fixture.join(name)).map_err(|err| format!("cannot copy {name}: {err}"))?;
    }
    let loader = fixture.join("loader.py");
    let python = python_executable();
    if !python.is_file() && python != Path::new("python3") {
        return Err(format!("python interpreter is missing: {}", python.display()));
    }

    let mut cmd = Command::new(&python);
    cmd.arg(&loader)
        .arg("--mode")
        .arg("suite")
        .arg("--suite")
        .arg(suite)
        .current_dir(&fixture)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", &fixture)
        .env("USERPROFILE", &fixture)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("PWD", &fixture)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONPATH", &runtime)
        .env("XEH_FIXTURE_ROOT", &fixture)
        .env("XEH_UPSTREAM", &upstream)
        .env("XEH_SUITE", suite)
        .env("XEH_LEASE_TMP", lease_tmp())
        .env("XEH_OUT", &out_dir)
        .env("XEH_FORBIDDEN_ROOTS", forbidden.join(":"))
        .env("XEH_WORKSPACE", &root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(value) = std::env::var_os("XEH_ISOLATION_BREAK") {
        cmd.env("XEH_ISOLATION_BREAK", value);
    }
    if let Some(value) = std::env::var_os("CARGO_TARGET_DIR") {
        cmd.env("CARGO_TARGET_DIR", value);
    }

    let mut child = cmd.spawn().map_err(|err| format!("cannot start oracle: {err}"))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .unwrap()
        .read_to_end(&mut stdout)
        .map_err(|err| format!("cannot read oracle stdout: {err}"))?;
    child
        .stderr
        .take()
        .unwrap()
        .read_to_end(&mut stderr)
        .map_err(|err| format!("cannot read oracle stderr: {err}"))?;
    let status = child.wait().map_err(|err| format!("cannot wait for oracle: {err}"))?;
    let code = status.code().unwrap_or(1);
    fs::write(out_dir.join("stdout.txt"), &stdout).ok();
    fs::write(out_dir.join("stderr.txt"), &stderr).ok();

    let audit_text = fs::read_to_string(out_dir.join("audit.json")).unwrap_or_default();
    let leaked = forbidden_hit(&audit_text, &forbidden);
    if let Some(path) = leaked {
        write_receipt(&root, suite, code, &fixture, &out_dir, &forbidden, json!({"leaked": path}))?;
        return Err(format!(
            "oracle isolation failed: forbidden path was opened: {path}"
        ));
    }
    if std::env::var_os("XEH_ISOLATION_BREAK").is_some() {
        write_receipt(&root, suite, code, &fixture, &out_dir, &forbidden, json!({"break": true}))?;
        if code == 0 {
            return Err("oracle isolation break was not detected".to_string());
        }
        return Err(format!(
            "oracle isolation break detected; exit={code}"
        ));
    }
    if code != 0 {
        write_receipt(&root, suite, code, &fixture, &out_dir, &forbidden, json!({}))?;
        return Err(format!(
            "oracle capture failed: exit={code}\n{}",
            String::from_utf8_lossy(&stderr)
        ));
    }

    if suite == "isolation" {
        let receipt = write_receipt(&root, suite, code, &fixture, &out_dir, &forbidden, json!({}))?;
        println!("suite=isolation");
        println!("exit=0");
        println!("receipt={}", receipt.display());
        return Ok(());
    }

    let result_path = out_dir.join("result.json");
    let result_text = fs::read_to_string(&result_path)
        .map_err(|err| format!("oracle did not write result.json: {err}"))?;
    let result: Value = serde_json::from_str(&result_text)
        .map_err(|err| format!("oracle result is not JSON: {err}"))?;
    write_outputs(&root, &result)?;
    let counts = count_statuses(result.get("manifest").unwrap_or(&json!({})));
    let receipt = write_receipt(
        &root,
        suite,
        code,
        &fixture,
        &out_dir,
        &forbidden,
        Value::Object(counts),
    )?;
    println!("suite={suite}");
    println!("exit=0");
    println!("receipt={}", receipt.display());
    println!("manifest={}", root.join("compat/manifest.json").display());
    Ok(())
}

fn write_outputs(root: &Path, result: &Value) -> Result<(), String> {
    let manifest = result.get("manifest").ok_or("oracle result has no manifest")?;
    let text = serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
    if text.contains("\"status\": \"implemented\"") {
        return Err("oracle result claims implemented status; refusing to write it".to_string());
    }
    let catalog = result.get("catalog").cloned().unwrap_or(json!({"ids": []}));
    let behaviors = result.get("behaviors").cloned().unwrap_or(json!({"ids": []}));
    let catalog_ids = string_ids(&catalog);
    let behavior_ids = string_ids(&behaviors);
    let mut required = catalog_ids;
    required.extend(behavior_ids);
    let required_refs: Vec<&str> = required.iter().map(String::as_str).collect();
    check_manifest(&text, &required_refs).map_err(|err| err.message)?;
    fs::create_dir_all(root.join("compat")).map_err(|err| err.to_string())?;
    fs::create_dir_all(root.join("compat/golden")).map_err(|err| err.to_string())?;
    fs::write(root.join("compat/manifest.json"), text + "\n").map_err(|err| err.to_string())?;
    fs::write(
        root.join("compat/catalog.json"),
        serde_json::to_string_pretty(&catalog).unwrap() + "\n",
    )
    .map_err(|err| err.to_string())?;
    fs::write(
        root.join("compat/required-behaviors.json"),
        serde_json::to_string_pretty(&behaviors).unwrap() + "\n",
    )
    .map_err(|err| err.to_string())?;
    if let Some(golden) = result.get("golden").and_then(Value::as_object) {
        for (name, value) in golden {
            let path = root.join("compat/golden").join(format!("{name}.json"));
            fs::write(path, serde_json::to_string_pretty(value).unwrap() + "\n")
                .map_err(|err| err.to_string())?;
        }
    }
    let deltas = result
        .get("deltas_markdown")
        .and_then(Value::as_str)
        .unwrap_or("# 兼容差异\n\n尚未记录已执行的原版差异。\n");
    fs::create_dir_all(root.join("docs")).map_err(|err| err.to_string())?;
    fs::write(root.join("docs/compatibility-deltas.md"), deltas).map_err(|err| err.to_string())?;
    Ok(())
}

fn string_ids(value: &Value) -> Vec<String> {
    value
        .get("ids")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

fn write_receipt(
    root: &Path,
    suite: &str,
    exit: i32,
    fixture: &Path,
    out_dir: &Path,
    forbidden: &[String],
    counts: Value,
) -> Result<PathBuf, String> {
    let candidate = git_head(root);
    let mut env = serde_json::Map::new();
    for key in RECEIPT_ENV {
        if let Ok(value) = std::env::var(key) {
            env.insert((*key).to_string(), Value::String(value));
        }
    }
    env.insert(
        "HOME".to_string(),
        Value::String(fixture.display().to_string()),
    );
    env.insert(
        "XEH_FIXTURE_ROOT".to_string(),
        Value::String(fixture.display().to_string()),
    );
    env.insert("XEH_SUITE".to_string(), Value::String(suite.to_string()));
    let receipt = json!({
        "command": format!("xeh-xtask oracle capture --suite {suite}"),
        "cwd": fixture,
        "base": BASE_COMMIT,
        "candidate": candidate,
        "exit": exit,
        "counts": counts,
        "scope": "task1-compat-oracle",
        "upstream": {
            "app": {"repo": UPSTREAM_APP_REPO, "revision": UPSTREAM_APP_REV},
            "webui": {"repo": UPSTREAM_WEBUI_REPO, "revision": UPSTREAM_WEBUI_REV}
        },
        "forbidden_roots": forbidden,
        "env": env,
        "stdout": out_dir.join("stdout.txt"),
        "stderr": out_dir.join("stderr.txt"),
        "audit": out_dir.join("audit.json"),
        "result": out_dir.join("result.json")
    });
    let path = evidence_dir().join(format!("receipt-{suite}-{}.json", unix_stamp()));
    fs::write(&path, serde_json::to_string_pretty(&receipt).unwrap() + "\n")
        .map_err(|err| format!("cannot write receipt: {err}"))?;
    Ok(path)
}

fn git_head(root: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}

fn forbidden_roots(fixture: &Path) -> Vec<String> {
    let mut roots = Vec::new();
    for key in ["HOME", "USERPROFILE"] {
        if let Ok(value) = std::env::var(key) {
            if !value.is_empty() && Path::new(&value) != fixture {
                roots.push(value);
            }
        }
    }
    roots.push("/home/wow".to_string());
    roots.sort();
    roots.dedup();
    roots
}

fn forbidden_hit(audit: &str, forbidden: &[String]) -> Option<String> {
    let Ok(value) = serde_json::from_str::<Value>(audit) else {
        return None;
    };
    let Some(opens) = value.get("opens").and_then(Value::as_array) else {
        return None;
    };
    for item in opens {
        let Some(path) = item.as_str() else {
            continue;
        };
        for root in forbidden {
            if root.is_empty() {
                continue;
            }
            if path == root || path.starts_with(&format!("{root}/")) {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn unix_stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn ensure_upstream(root: &Path) -> Result<PathBuf, String> {
    let dest = upstream_checkout(root);
    let marker = dest.join("xeHentai/const.py");
    if !marker.is_file() {
        let extracted = extracted_app_tree();
        if extracted.join("xeHentai/const.py").is_file() {
            copy_tree(&extracted, &dest)?;
        } else {
            download_and_extract(root)?;
        }
    }
    if !marker.is_file() && !dest.join("xeHentai/const.py").is_file() {
        return Err("pinned xeHentai sources are not available".to_string());
    }
    let webui = root.join("compat/upstream/webui");
    if !webui.join("package.json").is_file() {
        let extracted = extracted_webui_tree();
        if extracted.join("package.json").is_file() {
            copy_tree(&extracted, &webui)?;
        }
    }
    let provenance = root.join("compat/upstream/PROVENANCE.txt");
    if let Some(parent) = provenance.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(
        provenance,
        format!(
            "xeHentai repository={UPSTREAM_APP_REPO} revision={UPSTREAM_APP_REV} retrieval=gh-api-tarball\n\
             xeHentai-webui repository={UPSTREAM_WEBUI_REPO} revision={UPSTREAM_WEBUI_REV} retrieval=gh-api-tarball\n"
        ),
    )
    .map_err(|err| err.to_string())?;
    Ok(dest)
}

fn download_and_extract(root: &Path) -> Result<(), String> {
    let tar_dir = lease_tmp().join("upstream");
    fs::create_dir_all(&tar_dir).map_err(|err| err.to_string())?;
    let tar = tar_dir.join("xeHentai-43fc55c.tar.gz");
    if !tar.is_file() {
        let mut file = File::create(&tar).map_err(|err| err.to_string())?;
        let output = Command::new("gh")
            .args([
                "api",
                &format!("repos/{UPSTREAM_APP_REPO}/tarball/{UPSTREAM_APP_REV}"),
            ])
            .output()
            .map_err(|err| format!("gh api failed: {err}"))?;
        if !output.status.success() {
            return Err(format!(
                "gh api failed: {}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        file.write_all(&output.stdout).map_err(|err| err.to_string())?;
    }
    let src = tar_dir.join("src");
    fs::create_dir_all(&src).map_err(|err| err.to_string())?;
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&tar)
        .arg("-C")
        .arg(&src)
        .status()
        .map_err(|err| format!("tar failed: {err}"))?;
    if !status.success() {
        return Err("tar extract failed".to_string());
    }
    let extracted = extracted_app_tree();
    if extracted.join("xeHentai/const.py").is_file() {
        copy_tree(&extracted, &upstream_checkout(root))?;
    }
    Ok(())
}

fn copy_tree(from: &Path, to: &Path) -> Result<(), String> {
    fs::create_dir_all(to).map_err(|err| err.to_string())?;
    for entry in fs::read_dir(from).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let file_type = entry.file_type().map_err(|err| err.to_string())?;
        if file_type.is_symlink() {
            continue;
        }
        let dest = to.join(entry.file_name());
        if file_type.is_dir() {
            copy_tree(&entry.path(), &dest)?;
        } else if file_type.is_file() {
            fs::copy(entry.path(), dest).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}
