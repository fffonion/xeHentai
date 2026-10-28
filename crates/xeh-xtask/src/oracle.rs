use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::manifest::{check_coverage_claims, check_manifest, count_statuses};
use crate::paths::{
    authoritative_git, evidence_dir, lease_tmp, python_executable, scripts_dir, upstream_checkout,
    workspace_root, BASE_COMMIT, UPSTREAM_APP_REPO, UPSTREAM_APP_REV, UPSTREAM_WEBUI_REPO,
    UPSTREAM_WEBUI_REV,
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

struct VerifyReport {
    files_compared: u64,
    source_path: String,
    method: String,
    result: String,
}

pub fn capture(suite: &str) -> Result<(), String> {
    let tmp = lease_tmp()?;
    let python = python_executable();
    ensure_python(&python)?;
    let root = workspace_root();
    let (candidate, _report) = prepare_verified_upstream(&root)?;
    let stamp = unix_stamp();
    let fixture = tmp.join(format!("fixtures/{suite}-{stamp}"));
    fs::create_dir_all(&fixture).map_err(|err| format!("cannot create fixture: {err}"))?;
    fs::create_dir_all(evidence_dir()?)
        .map_err(|err| format!("cannot create evidence dir: {err}"))?;

    let forbidden = forbidden_roots(&fixture);
    let out_dir = capture_out_dir(suite, stamp)?;
    fs::create_dir_all(&out_dir).map_err(|err| format!("cannot create capture dir: {err}"))?;
    let runtime = fixture.join("pkg");
    copy_tree(&candidate, &runtime)?;
    let import_report = verify_tree(&runtime, &authoritative_git("xeHentai")?, UPSTREAM_APP_REV)?;
    let scripts = scripts_dir();
    for name in ["loader.py", "harness.py", "contract.py", "process_log.py"] {
        let src = scripts.join(name);
        if !src.is_file() {
            return Err(format!("oracle script is missing: {}", src.display()));
        }
        fs::copy(&src, fixture.join(name)).map_err(|err| format!("cannot copy {name}: {err}"))?;
    }

    let audit_path = out_dir.join("processes/root/audit.json");
    if let Some(parent) = audit_path.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    let mut cmd = Command::new(&python);
    cmd.arg(fixture.join("loader.py"))
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
        .env("XEH_UPSTREAM", &runtime)
        .env("XEH_SUITE", suite)
        .env("XEH_LEASE_TMP", &tmp)
        .env("XEH_OUT", &out_dir)
        .env("XEH_PROCESS_ID", "root")
        .env("XEH_AUDIT_PATH", &audit_path)
        .env("XEH_FORBIDDEN_ROOTS", forbidden.join(":"))
        .env("XEH_WORKSPACE", &root)
        .env("COLUMNS", "4096")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some(value) = std::env::var_os("CARGO_TARGET_DIR") {
        cmd.env("CARGO_TARGET_DIR", value);
    }
    for key in [
        "XEH_ISOLATION_BREAK",
        "XEH_SUPPRESS_AUDIT",
        "XEH_MALFORM_AUDIT",
        "XEH_DECOY_CHILD",
        "XEH_SUPPRESS_CHILD_AUDIT",
    ] {
        if let Some(value) = std::env::var_os(key) {
            cmd.env(key, value);
        }
    }

    let mut child = cmd
        .spawn()
        .map_err(|err| format!("cannot start oracle: {err}"))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    child
        .stdout
        .take()
        .ok_or("oracle stdout pipe is missing")?
        .read_to_end(&mut stdout)
        .map_err(|err| format!("cannot read oracle stdout: {err}"))?;
    child
        .stderr
        .take()
        .ok_or("oracle stderr pipe is missing")?
        .read_to_end(&mut stderr)
        .map_err(|err| format!("cannot read oracle stderr: {err}"))?;
    let status = child
        .wait()
        .map_err(|err| format!("cannot wait for oracle: {err}"))?;
    let code = status.code().unwrap_or(1);
    write_bytes(&out_dir.join("stdout.txt"), &stdout, "oracle stdout")?;
    write_bytes(&out_dir.join("stderr.txt"), &stderr, "oracle stderr")?;

    let root_audit = load_audit(&audit_path)?;
    let processes = load_children(&out_dir)?;
    let mut audits = vec![("root".to_string(), root_audit.clone())];
    for process in &processes {
        let audit = load_child_audit(process)?;
        if let Some(exit) = process.get("exit").and_then(Value::as_i64) {
            if let Some(path) = forbidden_hit(&audit, &forbidden) {
                return Err(format!(
                    "oracle isolation failed: forbidden path was opened: {path}"
                ));
            }
            if exit == 3 {
                return Err(format!(
                    "subprocess isolation failed: {} exit={exit}",
                    process.get("id").and_then(Value::as_str).unwrap_or("child")
                ));
            }
        }
        audits.push((
            process
                .get("id")
                .and_then(Value::as_str)
                .unwrap_or("child")
                .to_string(),
            audit,
        ));
    }
    if let Some(path) = audits
        .iter()
        .find_map(|(_, audit)| forbidden_hit(audit, &forbidden))
    {
        return Err(format!(
            "oracle isolation failed: forbidden path was opened: {path}"
        ));
    }
    if std::env::var_os("XEH_ISOLATION_BREAK").is_some() && code == 0 {
        return Err("oracle isolation break was not detected".to_string());
    }
    if std::env::var_os("XEH_ISOLATION_BREAK").is_some() {
        return Err(format!("oracle isolation break detected; exit={code}"));
    }
    if code != 0 {
        return Err(format!(
            "oracle capture failed: exit={code}\n{}",
            String::from_utf8_lossy(&stderr)
        ));
    }

    if suite == "isolation" {
        let receipt = write_receipt(ReceiptInput {
            root: &root,
            suite,
            exit: code,
            fixture: &fixture,
            out_dir: &out_dir,
            forbidden: &forbidden,
            root_audit: &root_audit,
            processes: &processes,
            report: &import_report,
            counts: json!({}),
        })?;
        println!("suite=isolation");
        println!("exit=0");
        println!("receipt={}", receipt.display());
        return Ok(());
    }

    let normalized_path = out_dir.join("normalized-result.json");
    let normalized_text = fs::read_to_string(&normalized_path).map_err(|_| {
        "normalized result is missing; refusing to publish raw capture as a golden".to_string()
    })?;
    let result: Value = serde_json::from_str(&normalized_text)
        .map_err(|err| format!("normalized result is not JSON: {err}"))?;
    let emit = emit_root(&root);
    write_outputs(&emit, &result)?;
    let counts = count_statuses(result.get("manifest").unwrap_or(&json!({})));
    let receipt = write_receipt(ReceiptInput {
        root: &root,
        suite,
        exit: code,
        fixture: &fixture,
        out_dir: &out_dir,
        forbidden: &forbidden,
        root_audit: &root_audit,
        processes: &processes,
        report: &import_report,
        counts: Value::Object(counts),
    })?;
    println!("suite={suite}");
    println!("exit=0");
    println!("receipt={}", receipt.display());
    println!("manifest={}", emit.join("compat/manifest.json").display());
    Ok(())
}

pub fn compare_trees(left: &Path, right: &Path) -> Result<(), String> {
    let mut diffs = Vec::new();
    compare_dir(left, right, &mut diffs)?;
    if diffs.is_empty() {
        println!("normalized-equal");
        Ok(())
    } else {
        Err(diffs.join("\n"))
    }
}

fn emit_root(workspace: &Path) -> PathBuf {
    match std::env::var_os("XEH_EMIT_ROOT") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => workspace.to_path_buf(),
    }
}

fn capture_out_dir(suite: &str, stamp: u64) -> Result<PathBuf, String> {
    if let Some(value) = std::env::var_os("XEH_CAPTURE_OUT") {
        if !value.is_empty() {
            return Ok(PathBuf::from(value));
        }
    }
    Ok(evidence_dir()?.join(format!("capture-{suite}-{stamp}")))
}

fn ensure_python(python: &Path) -> Result<(), String> {
    let output = Command::new(python)
        .args(["-c", "import requests, socks"])
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .map_err(|err| {
            format!(
                "cannot start python interpreter {}: {err}",
                python.display()
            )
        })?;
    if output.status.success() {
        return Ok(());
    }
    Err(format!(
        "python interpreter {} is missing requests[socks]; install requests[socks] in an isolated environment and set XEH_PYTHON. import check failed: {}",
        python.display(),
        String::from_utf8_lossy(&output.stderr)
    ))
}

fn prepare_verified_upstream(root: &Path) -> Result<(PathBuf, VerifyReport), String> {
    let bare = authoritative_git("xeHentai")?;
    ensure_pin(
        &bare,
        &format!("https://github.com/{UPSTREAM_APP_REPO}.git"),
        UPSTREAM_APP_REV,
    )?;
    let candidate = if let Some(value) = std::env::var_os("XEH_UPSTREAM_CANDIDATE") {
        if value.is_empty() {
            default_candidate(root, &bare)?
        } else {
            PathBuf::from(value)
        }
    } else {
        default_candidate(root, &bare)?
    };
    let report = verify_tree(&candidate, &bare, UPSTREAM_APP_REV)?;
    if std::env::var_os("XEH_UPSTREAM_CANDIDATE").is_none() {
        write_provenance(root, &report)?;
    }
    Ok((candidate, report))
}

fn default_candidate(root: &Path, bare: &Path) -> Result<PathBuf, String> {
    let dest = upstream_checkout(root);
    if !dest.join("xeHentai/const.py").is_file() {
        export_tree(bare, UPSTREAM_APP_REV, &dest)?;
    }
    Ok(dest)
}

fn ensure_pin(bare: &Path, url: &str, revision: &str) -> Result<(), String> {
    if !bare.exists() {
        if let Some(parent) = bare.parent() {
            fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let status = git(&["init".into(), "--bare".into(), bare.display().to_string()])?;
        if !status.status.success() {
            return Err(format!(
                "git init failed: {}",
                String::from_utf8_lossy(&status.stderr)
            ));
        }
    }
    let has = git(&[
        "--git-dir".into(),
        bare.display().to_string(),
        "cat-file".into(),
        "-t".into(),
        revision.into(),
    ])?;
    if !has.status.success() {
        let fetch = git(&[
            "--git-dir".into(),
            bare.display().to_string(),
            "fetch".into(),
            "--depth".into(),
            "1".into(),
            url.into(),
            revision.into(),
        ])?;
        if !fetch.status.success() {
            return Err(format!(
                "cannot fetch pinned revision {revision}: {}",
                String::from_utf8_lossy(&fetch.stderr)
            ));
        }
    }
    let parsed = git(&[
        "--git-dir".into(),
        bare.display().to_string(),
        "rev-parse".into(),
        revision.into(),
    ])?;
    if !parsed.status.success() {
        return Err(format!("pinned revision {revision} is not a git object"));
    }
    let got = String::from_utf8_lossy(&parsed.stdout).trim().to_string();
    if got != revision {
        return Err(format!(
            "git object {got} does not match pinned revision {revision}"
        ));
    }
    Ok(())
}

fn export_tree(bare: &Path, revision: &str, dest: &Path) -> Result<(), String> {
    fs::create_dir_all(dest).map_err(|err| err.to_string())?;
    let archive = git(&[
        "--git-dir".into(),
        bare.display().to_string(),
        "archive".into(),
        revision.into(),
    ])?;
    if !archive.status.success() {
        return Err(format!(
            "git archive failed: {}",
            String::from_utf8_lossy(&archive.stderr)
        ));
    }
    let mut tar = Command::new("tar")
        .args(["-x", "-C"])
        .arg(dest)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|err| format!("tar failed: {err}"))?;
    {
        let stdin = tar.stdin.take().ok_or("tar stdin is missing")?;
        std::io::Write::write_all(&mut std::io::BufWriter::new(stdin), &archive.stdout)
            .map_err(|err| format!("cannot write archive to tar: {err}"))?;
    }
    let status = tar.wait().map_err(|err| format!("tar failed: {err}"))?;
    if !status.success() {
        return Err("tar extract failed".to_string());
    }
    Ok(())
}

fn verify_tree(candidate: &Path, bare: &Path, revision: &str) -> Result<VerifyReport, String> {
    let listed = git(&[
        "--git-dir".into(),
        bare.display().to_string(),
        "ls-tree".into(),
        "-r".into(),
        revision.into(),
    ])?;
    if !listed.status.success() {
        return Err(format!(
            "cannot list pinned tree before import: {}",
            String::from_utf8_lossy(&listed.stderr)
        ));
    }
    let mut expected = std::collections::BTreeSet::new();
    let mut compared = 0u64;
    for line in String::from_utf8_lossy(&listed.stdout).lines() {
        let Some((meta, rel)) = line.split_once('\t') else {
            continue;
        };
        if rel.split('/').any(|part| part == ".." || part.is_empty()) {
            return Err(format!("unsafe path in pinned tree: {rel}"));
        }
        let mode = meta.split(' ').next().unwrap_or("");
        if mode == "160000" {
            continue;
        }
        expected.insert(rel.to_string());
        let blob = git(&[
            "--git-dir".into(),
            bare.display().to_string(),
            "cat-file".into(),
            "blob".into(),
            format!("{revision}:{rel}"),
        ])?;
        if !blob.status.success() {
            return Err(format!("pinned blob missing before import: {rel}"));
        }
        let path = candidate.join(rel);
        let actual = if mode == "120000" {
            fs::read_link(&path)
                .map(|target| target.to_string_lossy().into_owned().into_bytes())
                .map_err(|_| format!("upstream byte mismatch before import: missing {rel}"))?
        } else {
            fs::read(&path)
                .map_err(|_| format!("upstream byte mismatch before import: missing {rel}"))?
        };
        if actual != blob.stdout {
            return Err(format!("upstream byte mismatch before import: {rel}"));
        }
        compared += 1;
    }
    if !candidate.is_dir() {
        return Err("upstream checkout is missing before import".to_string());
    }
    for rel in walk_files(candidate)? {
        if !expected.contains(&rel) {
            return Err(format!(
                "upstream byte mismatch before import: unexpected {rel}"
            ));
        }
    }
    Ok(VerifyReport {
        files_compared: compared,
        source_path: candidate.display().to_string(),
        method: "git-object-byte-compare".to_string(),
        result: "match".to_string(),
    })
}

fn walk_files(root: &Path) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    walk_files_inner(root, root, &mut out)?;
    Ok(out)
}

fn walk_files_inner(root: &Path, dir: &Path, out: &mut Vec<String>) -> Result<(), String> {
    for entry in fs::read_dir(dir).map_err(|err| err.to_string())? {
        let entry = entry.map_err(|err| err.to_string())?;
        let path = entry.path();
        let rel = path
            .strip_prefix(root)
            .map_err(|err| err.to_string())?
            .to_string_lossy()
            .replace('\\', "/");
        let file_type = entry.file_type().map_err(|err| err.to_string())?;
        if file_type.is_symlink() || file_type.is_file() {
            out.push(rel);
        } else if file_type.is_dir() {
            walk_files_inner(root, &path, out)?;
        }
    }
    Ok(())
}

fn write_provenance(root: &Path, report: &VerifyReport) -> Result<(), String> {
    let provenance = root.join("compat/upstream/PROVENANCE.txt");
    if let Some(parent) = provenance.parent() {
        fs::create_dir_all(parent).map_err(|err| err.to_string())?;
    }
    fs::write(
        provenance,
        format!(
            "xeHentai repository={UPSTREAM_APP_REPO} revision={UPSTREAM_APP_REV} method={} result={} files_compared={} source_path={}\n\
             xeHentai-webui repository={UPSTREAM_WEBUI_REPO} revision={UPSTREAM_WEBUI_REV} executed=false verification=not-verified\n",
            report.method, report.result, report.files_compared, report.source_path
        ),
    )
    .map_err(|err| err.to_string())
}

fn git(args: &[String]) -> Result<std::process::Output, String> {
    Command::new("git")
        .args(args)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .output()
        .map_err(|err| format!("git failed: {err}"))
}

fn write_bytes(path: &Path, bytes: &[u8], label: &str) -> Result<(), String> {
    fs::write(path, bytes).map_err(|err| format!("cannot write {label} {}: {err}", path.display()))
}

fn load_audit(path: &Path) -> Result<Value, String> {
    let text =
        fs::read_to_string(path).map_err(|_| format!("audit is missing: {}", path.display()))?;
    serde_json::from_str(&text).map_err(|_| format!("audit is malformed: {}", path.display()))
}

fn load_children(out_dir: &Path) -> Result<Vec<Value>, String> {
    let path = out_dir.join("expected-children.json");
    let text = fs::read_to_string(&path)
        .map_err(|_| format!("child audit is missing: {}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|_| format!("audit is malformed: {}", path.display()))?;
    Ok(value
        .get("processes")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default())
}

fn load_child_audit(process: &Value) -> Result<Value, String> {
    let path = process
        .get("audit")
        .and_then(Value::as_str)
        .ok_or_else(|| "child audit is missing: expected entry has no audit path".to_string())?;
    load_audit(Path::new(path)).map_err(|err| {
        if err.contains("malformed") {
            err
        } else {
            format!("child audit is missing: {path}")
        }
    })
}

fn write_outputs(root: &Path, result: &Value) -> Result<(), String> {
    let manifest = result
        .get("manifest")
        .ok_or("oracle result has no manifest")?;
    let text = serde_json::to_string_pretty(manifest).map_err(|err| err.to_string())?;
    if text.contains("\"status\": \"implemented\"")
        || text.contains("\"status\": \"accepted\"")
        || text.contains("\"status\": \"passing\"")
    {
        return Err("oracle result claims implemented status; refusing to write it".to_string());
    }
    check_coverage_claims(&text)?;
    let catalog = result.get("catalog").cloned().unwrap_or(json!({"ids": []}));
    let behaviors = result
        .get("behaviors")
        .cloned()
        .unwrap_or(json!({"ids": []}));
    let catalog_ids = string_ids(&catalog);
    let behavior_ids = string_ids(&behaviors);
    let mut required = catalog_ids;
    required.extend(behavior_ids);
    let required_refs: Vec<&str> = required.iter().map(String::as_str).collect();
    check_manifest(&text, &required_refs).map_err(|err| err.message)?;
    fs::create_dir_all(root.join("compat/golden")).map_err(|err| err.to_string())?;
    fs::create_dir_all(root.join("docs")).map_err(|err| err.to_string())?;
    fs::write(root.join("compat/manifest.json"), text + "\n").map_err(|err| err.to_string())?;
    fs::write(
        root.join("compat/catalog.json"),
        serde_json::to_string_pretty(&catalog).map_err(|err| err.to_string())? + "\n",
    )
    .map_err(|err| err.to_string())?;
    fs::write(
        root.join("compat/required-behaviors.json"),
        serde_json::to_string_pretty(&behaviors).map_err(|err| err.to_string())? + "\n",
    )
    .map_err(|err| err.to_string())?;
    if let Some(golden) = result.get("golden").and_then(Value::as_object) {
        for (name, value) in golden {
            let path = root.join("compat/golden").join(format!("{name}.json"));
            fs::write(
                path,
                serde_json::to_string_pretty(value).map_err(|err| err.to_string())? + "\n",
            )
            .map_err(|err| err.to_string())?;
        }
    }
    let deltas = result
        .get("deltas_markdown")
        .and_then(Value::as_str)
        .unwrap_or("# 兼容差异\n\n尚未记录已执行的原版差异。\n");
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

struct ReceiptInput<'a> {
    root: &'a Path,
    suite: &'a str,
    exit: i32,
    fixture: &'a Path,
    out_dir: &'a Path,
    forbidden: &'a [String],
    root_audit: &'a Value,
    processes: &'a [Value],
    report: &'a VerifyReport,
    counts: Value,
}

fn write_receipt(input: ReceiptInput<'_>) -> Result<PathBuf, String> {
    let ReceiptInput {
        root,
        suite,
        exit,
        fixture,
        out_dir,
        forbidden,
        root_audit,
        processes,
        report,
        counts,
    } = input;
    let candidate = git_head(root);
    let env = root_audit.get("env").cloned().unwrap_or_else(|| json!({}));
    let mut recorded = Vec::new();
    recorded.push(json!({
        "id": "root",
        "cwd": root_audit.get("cwd").cloned().unwrap_or(json!(fixture)),
        "env": env,
        "audit": out_dir.join("processes/root/audit.json"),
    }));
    for process in processes {
        let id = process.get("id").and_then(Value::as_str).unwrap_or("child");
        let audit_path = process
            .get("audit")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| out_dir.join("processes").join(id).join("audit.json"));
        let child_audit = fs::read_to_string(&audit_path)
            .ok()
            .and_then(|text| serde_json::from_str::<Value>(&text).ok())
            .unwrap_or(json!({}));
        recorded.push(json!({
            "id": id,
            "parent": process.get("parent").cloned().unwrap_or(json!("root")),
            "cwd": child_audit.get("cwd"),
            "env": child_audit.get("env").cloned().unwrap_or(json!({})),
            "exit": process.get("exit"),
            "audit": audit_path,
            "stdout": process.get("stdout"),
            "stderr": process.get("stderr"),
        }));
    }
    let cwd = root_audit
        .get("cwd")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let receipt = json!({
        "command": format!("xeh-xtask oracle capture --suite {suite}"),
        "cwd": cwd,
        "base": BASE_COMMIT,
        "candidate": candidate,
        "exit": exit,
        "counts": counts,
        "scope": "task1-compat-oracle",
        "upstream": {
            "app": {
                "repo": UPSTREAM_APP_REPO,
                "revision": UPSTREAM_APP_REV,
                "source_path": report.source_path,
                "verification": {
                    "method": report.method,
                    "result": report.result,
                    "files_compared": report.files_compared
                }
            },
            "webui": {
                "repo": UPSTREAM_WEBUI_REPO,
                "revision": UPSTREAM_WEBUI_REV,
                "executed": false,
                "verification": {"method": "not-executed", "result": "not-verified"}
            }
        },
        "forbidden_roots": forbidden,
        "env": root_audit.get("env").cloned().unwrap_or(json!({})),
        "processes": recorded,
        "stdout": out_dir.join("stdout.txt"),
        "stderr": out_dir.join("stderr.txt"),
        "audit": out_dir.join("processes/root/audit.json"),
        "result": out_dir.join("result.json")
    });
    let _ = RECEIPT_ENV;
    let path = evidence_dir()?.join(format!("receipt-{suite}-{}.json", unix_stamp()));
    fs::write(
        &path,
        serde_json::to_string_pretty(&receipt).map_err(|err| err.to_string())? + "\n",
    )
    .map_err(|err| format!("cannot write receipt: {err}"))?;
    Ok(path)
}

fn git_head(root: &Path) -> String {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
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

fn forbidden_hit(audit: &Value, forbidden: &[String]) -> Option<String> {
    let opens = audit.get("opens").and_then(Value::as_array)?;
    for item in opens {
        let path = item.as_str()?;
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

fn compare_dir(left: &Path, right: &Path, diffs: &mut Vec<String>) -> Result<(), String> {
    let mut names = std::collections::BTreeSet::new();
    for dir in [left, right] {
        if !dir.is_dir() {
            return Err(format!("compare path is missing: {}", dir.display()));
        }
        for entry in fs::read_dir(dir).map_err(|err| err.to_string())? {
            let entry = entry.map_err(|err| err.to_string())?;
            names.insert(entry.file_name().to_string_lossy().to_string());
        }
    }
    for name in names {
        let left_path = left.join(&name);
        let right_path = right.join(&name);
        if left_path.is_dir() && right_path.is_dir() {
            compare_dir(&left_path, &right_path, diffs)?;
            continue;
        }
        if !left_path.is_file() || !right_path.is_file() {
            diffs.push(format!("{name}: present on only one side"));
            continue;
        }
        if name.ends_with(".json") {
            let left_value: Value = serde_json::from_str(
                &fs::read_to_string(&left_path).map_err(|err| err.to_string())?,
            )
            .map_err(|err| format!("{name} is not JSON: {err}"))?;
            let right_value: Value = serde_json::from_str(
                &fs::read_to_string(&right_path).map_err(|err| err.to_string())?,
            )
            .map_err(|err| format!("{name} is not JSON: {err}"))?;
            diff_json(&name, &left_value, &right_value, diffs);
        } else {
            let left_bytes = fs::read(&left_path).map_err(|err| err.to_string())?;
            let right_bytes = fs::read(&right_path).map_err(|err| err.to_string())?;
            if left_bytes != right_bytes {
                diffs.push(format!("{name}: bytes differ"));
            }
        }
    }
    Ok(())
}

fn diff_json(path: &str, left: &Value, right: &Value, diffs: &mut Vec<String>) {
    match (left, right) {
        (Value::Object(left_map), Value::Object(right_map)) => {
            for (key, value) in left_map {
                let next = format!("{path}.{key}");
                match right_map.get(key) {
                    Some(other) => diff_json(&next, value, other, diffs),
                    None => diffs.push(format!("{next}: missing on right")),
                }
            }
            for key in right_map.keys() {
                if !left_map.contains_key(key) {
                    diffs.push(format!("{path}.{key}: missing on left"));
                }
            }
        }
        (Value::Array(left_items), Value::Array(right_items)) => {
            let limit = left_items.len().max(right_items.len());
            for index in 0..limit {
                let next = format!("{path}[{index}]");
                match (left_items.get(index), right_items.get(index)) {
                    (Some(left_item), Some(right_item)) => {
                        diff_json(&next, left_item, right_item, diffs)
                    }
                    (None, Some(_)) => diffs.push(format!("{next}: missing on left")),
                    (Some(_), None) => diffs.push(format!("{next}: missing on right")),
                    (None, None) => {}
                }
            }
        }
        _ => {
            if left != right {
                diffs.push(format!("{path}: left={left} right={right}"));
            }
        }
    }
}

fn unix_stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0)
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
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|err| err.to_string())?;
            }
            fs::copy(entry.path(), dest).map_err(|err| err.to_string())?;
        }
    }
    Ok(())
}
