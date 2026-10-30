//! Session suite and upstream differential command.
//!
//! `rss-test --suite session` drives the real RSS session owner through save,
//! load, cookie, legacy and scan cases and checks the declared expectations.
//! `diff --suite session` feeds one identical case script to the pinned
//! upstream implementation (test oracle only) and to the RSS owner, then
//! compares return outcomes, task tables, ready queues, cookies/header/login,
//! and the resulting disk layout. Known safety differences are recorded as
//! explicit deltas instead of being normalized away.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use xeh_host::{ControlOptions, SessionOptions, TaskControl, ROOT_DENIED_TAG};

use crate::paths::{lease_tmp, python_executable, scripts_dir, UPSTREAM_APP_REV};

fn stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or(0)
}

const URL_A: &str = "https://e-hentai.org/g/123456/0123456789/";
const URL_B: &str = "https://e-hentai.org/g/654321/abcdef0123/";

#[allow(clippy::too_many_arguments)]
fn record(
    url: &str,
    guid: &str,
    state: i64,
    config: Value,
    meta: Value,
    reload_map: Value,
    duplicate_map: Value,
    renamed_map: Value,
    img_q: Value,
    page_q: Value,
) -> Value {
    json!({
        "url": url,
        "failcode": 0,
        "state": state,
        "guid": guid,
        "config": config,
        "meta": meta,
        "reload_map": reload_map,
        "duplicate_map": duplicate_map,
        "renamed_map": renamed_map,
        "img_q": img_q,
        "page_q": page_q
    })
}

/// The case script shared by the RSS suite and the upstream oracle.
///
/// Paths inside task config are `{downloads}` placeholders substituted with
/// the case-owned download root, so neither side reads a real user directory.
fn script() -> Value {
    json!({
        "cases": [
            {
                "name": "save_false",
                "mode": "save",
                "save_tasks": false,
                "tasks": [
                    record(URL_A, "save0001", 10,
                        json!({"dir": "{downloads}", "download_range": null, "extra_zero": 0, "extra_null": null}),
                        json!({"total": 3, "title": "saved", "filelist": {}}),
                        json!({}), json!({}), json!({"1": "renamed.jpg"}), json!(null), json!(null))
                ],
                "cookies": [["nw", "1"], ["zzz", "9"]],
                "expect": {"success": true, "code": 0, "saved_tasks_empty": true,
                    "cookie_names": ["nw", "zzz"], "header": "nw=1;zzz=9", "login": false}
            },
            {
                "name": "save_true",
                "mode": "save",
                "save_tasks": true,
                "tasks": [
                    record(URL_B, "save0002", 10,
                        json!({"dir": "{downloads}", "download_range": [[2, 3]], "custom": true}),
                        json!({"total": 4, "title": "saved2", "filelist": {}}),
                        json!({"1": ["https://e-hentai.org/s/aaaaaaaaaa/1-1", "renamed.jpg"]}),
                        json!({}), json!({"1": "renamed.jpg"}),
                        json!(["https://e-hentai.org/i/1"]),
                        json!(["https://e-hentai.org/s/aaaaaaaaaa/1-1"]))
                ],
                "cookies": [["nw", "1"], ["ipb_member_id", "42"], ["ipb_pass_hash", "hash"]],
                "expect": {"success": true, "code": 0, "saved_tasks_empty": false,
                    "queue": ["save0002"], "header": "nw=1;ipb_member_id=42;ipb_pass_hash=hash", "login": true}
            },
            {
                "name": "save_reverse_order",
                "mode": "save",
                "save_tasks": true,
                "tasks": [
                    record(URL_B, "zzzzzzz9", 1, json!({}), json!({}), json!({}), json!({}), json!({}), json!(null), json!(null)),
                    record(URL_A, "0000000a", 1, json!({}), json!({}), json!({}), json!({}), json!({}), json!(null), json!(null))
                ],
                "cookies": [["nw", "1"]],
                "expect": {"success": true, "code": 0, "saved_tasks_empty": false,
                    "queue": ["zzzzzzz9", "0000000a"]}
            },
            {
                "name": "load_order",
                "mode": "load",
                "document": "{\"tasks\":{\"zzz-outer\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"order001\",\"state\":1,\"failcode\":0,\"config\":{},\"meta\":{},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}},\"aaa-outer\":{\"url\":\"https://e-hentai.org/g/654321/abcdef0123/\",\"guid\":\"order002\",\"state\":1,\"failcode\":0,\"config\":{},\"meta\":{},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}}},\"cookies\":{\"zzz\":\"9\",\"nw\":\"7\",\"aaa\":\"1\"}}",
                "expect": {"success": true, "code": 0,
                    "queue": ["order001", "order002"],
                    "header": "nw=7;zzz=9;aaa=1", "login": false}
            },
            {
                "name": "load_double",
                "mode": "load",
                "repeat": 2,
                "document": "{\"tasks\":{\"k\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"double01\",\"state\":1,\"failcode\":0,\"config\":{},\"meta\":{},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}}},\"cookies\":{\"nw\":\"1\"}}",
                "expect": {"success": true, "code": 0, "queue": ["double01", "double01"]}
            },
            {
                "name": "load_scan",
                "mode": "load",
                "document": "{\"tasks\":{\"scan\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"scan0001\",\"state\":10,\"failcode\":0,\"config\":{\"dir\":\"{downloads}\",\"download_range\":null},\"meta\":{\"total\":3,\"title\":\"gallery\",\"filelist\":{},\"finished\":0},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{},\"page_q\":[\"https://e-hentai.org/s/aaaaaaaaaa/1-1\"]},\"marked\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"mark0001\",\"state\":10,\"failcode\":0,\"config\":{\"dir\":\"{downloads}\",\"download_range\":null},\"meta\":{\"total\":2,\"title\":\"marked\",\"filelist\":{},\"finished\":0},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}},\"zipped\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"zip00001\",\"state\":10,\"failcode\":0,\"config\":{\"dir\":\"{downloads}\",\"download_range\":null},\"meta\":{\"total\":1,\"title\":\"zipped\",\"filelist\":{},\"finished\":0},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}},\"ranged\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"range001\",\"state\":10,\"failcode\":0,\"config\":{\"dir\":\"{downloads}\",\"download_range\":[[2,3]]},\"meta\":{\"total\":4,\"title\":\"ranged\",\"filelist\":{},\"finished\":0},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}},\"complete\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"full0001\",\"state\":10,\"failcode\":0,\"config\":{\"dir\":\"{downloads}\",\"download_range\":null},\"meta\":{\"total\":2,\"title\":\"complete\",\"filelist\":{},\"finished\":0},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}}},\"cookies\":{\"nw\":\"1\"}}",
                "files": [
                    {"path": "gallery/1.jpg", "text": "payload"},
                    {"path": "ranged/2.jpg", "text": "two"},
                    {"path": "ranged/3.jpg", "text": "three"},
                    {"path": "complete/1.jpg", "text": "x"},
                    {"path": "complete/2.jpg", "text": "y"}
                ],
                "zero_files": ["gallery/2.jpg"],
                "markers": ["marked/.xehdone"],
                "zips": ["zipped.zip"],
                "expect": {"success": true, "code": 0,
                    "finished": {"scan0001": 1, "mark0001": 2, "zip00001": 1, "range001": 4, "full0001": 2},
                    "state": {"scan0001": 4, "mark0001": 10, "full0001": 10},
                    "disk": ["complete/1.jpg", "complete/2.jpg", "gallery/1.jpg", "marked/.xehdone", "ranged/2.jpg", "ranged/3.jpg", "zipped.zip"]}
            },
            {
                "name": "legacy_valid",
                "mode": "load",
                "document": "{\"tasks\":{},\"cookies\":{\"zzz\":\"9\",\"nw\":\"7\",\"aaa\":\"1\"}}",
                "legacy": " 11 ,synthetic-hash \n",
                "expect": {"success": true, "code": 0,
                    "header": "nw=7;zzz=9;aaa=1;ipb_member_id=11 ;ipb_pass_hash=synthetic-hash", "login": true}
            },
            {
                "name": "legacy_malformed",
                "mode": "load",
                "document": null,
                "legacy": "a,b,c",
                "expect": {"success": true, "code": 0, "header": null, "login": false}
            },
            {
                "name": "legacy_existing_login",
                "mode": "load",
                "document": "{\"tasks\":{},\"cookies\":{\"nw\":\"7\",\"ipb_member_id\":\"5\",\"ipb_pass_hash\":\"H\"}}",
                "legacy": "other,values\n",
                "expect": {"success": true, "code": 0,
                    "header": "nw=7;ipb_member_id=5;ipb_pass_hash=H", "login": true}
            },
            {
                "name": "queues_null_empty",
                "mode": "load",
                "document": "{\"tasks\":{\"a\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"queue001\",\"state\":1,\"failcode\":0,\"config\":{},\"meta\":{},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{},\"img_q\":null,\"page_q\":[]}},\"cookies\":{\"nw\":\"1\"}}",
                "expect": {"success": true, "code": 0}
            },
            {
                "name": "load_duplicate_guid",
                "mode": "load",
                "document": "{\"tasks\":{\"a\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"dup00001\",\"state\":1,\"failcode\":0,\"config\":{},\"meta\":{},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}},\"b\":{\"url\":\"https://e-hentai.org/g/654321/abcdef0123/\",\"guid\":\"dup00001\",\"state\":3,\"failcode\":0,\"config\":{},\"meta\":{},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}}},\"cookies\":{\"nw\":\"1\"}}",
                "expect": {"success": true, "code": 0, "queue": ["dup00001", "dup00001"],
                    "state": {"dup00001": 3}}
            },
            {
                "name": "unreadable_session",
                "mode": "load",
                "document": "{\"tasks\":{},\"cookies\":{\"nw\":\"1\"}}",
                "unreadable": true,
                "expect": {"success": false, "code": null, "error_prefix": "session:host_failure"}
            },
            {
                "name": "bad_json",
                "mode": "load",
                "document": "{\"tasks\": {}, \"cookies\": {}",
                "expect": {"success": false, "code": 1103, "error_prefix": "session:parse", "file_unchanged": true}
            },
            {
                "name": "missing_tasks",
                "mode": "load",
                "document": "{\"cookies\": {}}",
                "expect": {"success": false, "code": null, "error_prefix": "session:missing_tasks"}
            },
            {
                "name": "tasks_type",
                "mode": "load",
                "document": "{\"tasks\": [], \"cookies\": {}}",
                "expect": {"success": false, "code": null, "error_prefix": "session:structure"}
            },
            {
                "name": "missing_guid",
                "mode": "load",
                "document": "{\"tasks\": {\"a\": {\"url\": \"https://e-hentai.org/g/1/aaaaaaaaaa/\"}}, \"cookies\": {}}",
                "expect": {"success": false, "code": null, "error_prefix": "session:missing_guid"}
            },
            {
                "name": "outside_root",
                "mode": "load",
                "document": "{\"tasks\":{\"a\":{\"url\":\"https://e-hentai.org/g/123456/0123456789/\",\"guid\":\"outside1\",\"state\":10,\"failcode\":0,\"config\":{\"dir\":\"/definitely-not-a-trusted-root\",\"download_range\":null},\"meta\":{\"total\":1,\"title\":\"x\",\"filelist\":{}},\"reload_map\":{},\"duplicate_map\":{},\"renamed_map\":{}}},\"cookies\":{\"nw\":\"1\"}}",
                "expect": {"success": false, "code": null, "error_prefix": "session:host_failure"}
            }
        ]
    })
}

fn substitute(value: &Value, downloads: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace("{downloads}", downloads)),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| substitute(item, downloads))
                .collect(),
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| (key.clone(), substitute(item, downloads)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn case_dir(root: &Path, name: &str) -> PathBuf {
    root.join("cases").join(name)
}

fn write_case_files(case_root: &Path, case: &Value, downloads: &Path) -> Result<(), String> {
    for dir in ["session", "legacy", "downloads", "boot"] {
        fs::create_dir_all(case_root.join(dir)).map_err(|error| error.to_string())?;
    }
    let _ = downloads;
    if let Some(document) = case.get("document").and_then(Value::as_str) {
        let session_file = case_root.join("session/h.json");
        fs::write(&session_file, document).map_err(|error| error.to_string())?;
        if case["unreadable"].as_bool().unwrap_or(false) {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&session_file, fs::Permissions::from_mode(0o000))
                    .map_err(|error| error.to_string())?;
            }
        }
    }
    if let Some(legacy) = case.get("legacy").and_then(Value::as_str) {
        fs::write(case_root.join("legacy/.ehentai.cookie"), legacy)
            .map_err(|error| error.to_string())?;
    }
    for relative in case
        .get("zero_files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = case_root
            .join("downloads")
            .join(relative.as_str().unwrap_or(""));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, b"").map_err(|error| error.to_string())?;
    }
    for item in case
        .get("files")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = case_root
            .join("downloads")
            .join(item["path"].as_str().unwrap_or(""));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, item["text"].as_str().unwrap_or("").as_bytes())
            .map_err(|error| error.to_string())?;
    }
    for relative in case
        .get("markers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = case_root
            .join("downloads")
            .join(relative.as_str().unwrap_or(""));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, b"synthetic\n").map_err(|error| error.to_string())?;
    }
    for relative in case
        .get("zips")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let path = case_root
            .join("downloads")
            .join(relative.as_str().unwrap_or(""));
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        }
        fs::write(path, b"").map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn build_document(case: &Value, downloads: &str) -> String {
    let tasks = case["tasks"].as_array().cloned().unwrap_or_default();
    let entries: Vec<String> = tasks
        .iter()
        .map(|task| {
            let guid = task["guid"].as_str().unwrap_or("");
            let record = substitute(task, downloads);
            format!("\"{guid}\":{}", serde_json::to_string(&record).unwrap())
        })
        .collect();
    let cookies = case["cookies"].as_array().cloned().unwrap_or_default();
    let cookie_entries: Vec<String> = cookies
        .iter()
        .map(|pair| {
            let name = pair[0].as_str().unwrap_or("");
            format!("\"{name}\":{}", serde_json::to_string(&pair[1]).unwrap())
        })
        .collect();
    format!(
        "{{\"tasks\":{{{}}},\"cookies\":{{{}}}}}",
        entries.join(","),
        cookie_entries.join(",")
    )
}

fn disk_snapshot(case_root: &Path) -> Value {
    let downloads = case_root.join("downloads");
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut stack = vec![downloads.clone()];
    while let Some(directory) = stack.pop() {
        let Ok(read) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(metadata) = fs::metadata(&path) {
                let relative = path
                    .strip_prefix(&downloads)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .to_string();
                entries.push((relative, metadata.len()));
            }
        }
    }
    entries.sort();
    Value::Array(
        entries
            .into_iter()
            .map(|(path, size)| json!([path, size]))
            .collect(),
    )
}

fn cookie_pairs(report: &Value) -> Value {
    let mut pairs = Vec::new();
    for cookie in report["cookies"].as_array().cloned().unwrap_or_default() {
        pairs.push(json!([cookie["name"].clone(), cookie["value"].clone()]));
    }
    Value::Array(pairs)
}

fn session_snapshot(
    control: &TaskControl,
    report: &Value,
    case_root: &Path,
) -> Result<Value, String> {
    let _ = control;
    Ok(json!({
        "tasks": report["tasks"].clone(),
        "queue": report["queue"].clone(),
        "cookies": cookie_pairs(report),
        "header": report["cookie_header"].clone(),
        "login": report["has_login"].clone(),
        "disk": disk_snapshot(case_root),
    }))
}

fn open_case(case_root: &Path) -> Result<TaskControl, String> {
    let options = SessionOptions::new(
        case_root.join("session/h.json"),
        case_root.join("legacy/.ehentai.cookie"),
        vec![case_root.join("downloads")],
    );
    TaskControl::open_with_session(&ControlOptions::fixture(&[], 0), Some(&options))
        .map_err(|error| error.to_string())
}

fn parse(text: &str, what: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|error| format!("{what} is not JSON: {error}\n{text}"))
}

fn run_rss_case(root: &Path, case: &Value) -> Result<Value, String> {
    let name = case["name"].as_str().unwrap_or("");
    let case_root = case_dir(root, name);
    let downloads = case_root.join("downloads");
    let downloads_text = downloads.to_string_lossy().to_string();
    let mut prepared = case.clone();
    if case["mode"] == "save" {
        let document = build_document(&substitute(case, &downloads_text), &downloads_text);
        prepared["document"] = Value::String(document);
    } else {
        prepared = substitute(case, &downloads_text);
    }
    write_case_files(&case_root, &prepared, &downloads)?;
    let mut control = open_case(&case_root)?;
    let baseline_text = control
        .invoke(&json!({"op": "snapshot"}).to_string())
        .map_err(|error| error.to_string())?;
    let baseline = session_snapshot(
        &control,
        &parse(&baseline_text, "baseline report")?,
        &case_root,
    )?;
    let mut last = Value::Null;
    if case["mode"] == "save" {
        let loaded = control.load_session().map_err(|error| error.to_string())?;
        let loaded: Value = parse(&loaded, "initial load report")?;
        if loaded["session"]["code"] != json!(0) {
            return Ok(failure_result(name, &loaded, &case_root));
        }
        let save_tasks = case["save_tasks"].as_bool().unwrap_or(false);
        let update = control
            .invoke(&json!({"op": "update", "overlay": {"save_tasks": save_tasks}}).to_string())
            .map_err(|error| error.to_string())?;
        let update: Value = parse(&update, "update report")?;
        if update["code"] != json!(0) {
            return Err(format!("save_tasks update failed: {update}"));
        }
        let saved = control.save_session().map_err(|error| error.to_string())?;
        let saved: Value = parse(&saved, "save report")?;
        let mut result = if saved["session"]["code"] == json!(0) {
            json!({
                "name": name,
                "mode": "save",
                "outcome": "success",
                "ret": [0, Value::Null],
            })
        } else {
            failure_result(name, &saved, &case_root)
        };
        result["saved_size"] = json!(fs::metadata(case_root.join("session/h.json"))
            .map(|m| m.len())
            .unwrap_or(0));
        // Read back through a fresh owner: this exposes the saved document's
        // task order and cookie order without a serde object reordering it.
        let mut fresh = open_case(&case_root)?;
        let loaded = fresh.load_session().map_err(|error| error.to_string())?;
        let loaded: Value = parse(&loaded, "fresh load report")?;
        let snapshot = session_snapshot(&fresh, &loaded, &case_root)?;
        if let (Some(result), Some(snapshot)) = (result.as_object_mut(), snapshot.as_object()) {
            for (key, value) in snapshot {
                result.insert(key.clone(), value.clone());
            }
        }
        result["pre"] = baseline;
        Ok(result)
    } else {
        let repeat = case["repeat"].as_u64().unwrap_or(1);
        for _ in 0..repeat {
            let text = control.load_session().map_err(|error| error.to_string())?;
            last = parse(&text, "load report")?;
            if last["session"]["code"] != json!(0) {
                break;
            }
        }
        let mut result = if last["session"]["code"] == json!(0) {
            json!({
                "name": name,
                "mode": "load",
                "outcome": "success",
                "ret": [0, Value::Null],
            })
        } else {
            failure_result(name, &last, &case_root)
        };
        let snapshot = session_snapshot(&control, &last, &case_root)?;
        if let (Some(result), Some(snapshot)) = (result.as_object_mut(), snapshot.as_object()) {
            for (key, value) in snapshot {
                result.insert(key.clone(), value.clone());
            }
        }
        result["pre"] = baseline;
        restore_case_permissions(&case_root);
        restore_case_permissions(&case_root);
        Ok(result)
    }
}

fn restore_case_permissions(case_root: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(
            case_root.join("session/h.json"),
            fs::Permissions::from_mode(0o600),
        );
    }
    let _ = case_root;
}

fn failure_result(name: &str, report: &Value, case_root: &Path) -> Value {
    let _ = case_root;
    let error = report["session"]["error"]
        .as_str()
        .unwrap_or("session:host_failure");
    let class = error.strip_prefix("session:").unwrap_or(error);
    json!({
        "name": name,
        "outcome": "failure",
        "code": report["session"]["code"].clone(),
        "class": class,
        "value": report["session"]["value"].clone(),
    })
}

fn run_rss(script: &Value, root: &Path) -> Result<Value, String> {
    let mut records = Vec::new();
    for case in script["cases"]
        .as_array()
        .ok_or("script is missing cases")?
    {
        records.push(run_rss_case(root, case)?);
    }
    Ok(json!({"records": records}))
}

// ---- shared upstream producer for the cross-version E2E --------------------

/// One session fixture written by the pinned upstream `save_session`.
pub struct ProducedSession {
    pub session_path: PathBuf,
    pub case_root: PathBuf,
    pub record: Value,
}

/// Runs the pinned upstream producer for the remaining-image case and returns
/// the path of the `h.json` it actually wrote. The caller never rewrites or
/// normalizes that file.
pub fn produce_remaining_session(
    root: &Path,
    record: Value,
    files: Value,
) -> Result<ProducedSession, String> {
    fs::create_dir_all(root)
        .map_err(|error| format!("cannot create producer root {}: {error}", root.display()))?;
    let script = json!({
        "cases": [{
            "name": "remaining",
            "mode": "produce_remaining",
            "record": record,
            "files": files,
        }]
    });
    let response = run_python(root, &script)?;
    let record = response["records"]
        .as_array()
        .and_then(|records| records.first())
        .cloned()
        .ok_or_else(|| "producer returned no record".to_string())?;
    if record["outcome"] != json!("return") || record["ret"][0] != json!(0) {
        return Err(format!("upstream save_session did not succeed: {record}"));
    }
    let case_root = root.join("cases/remaining");
    let session_path = case_root.join("session/h.json");
    if !session_path.is_file() {
        return Err(format!(
            "upstream producer did not write {}",
            session_path.display()
        ));
    }
    Ok(ProducedSession {
        session_path,
        case_root,
        record,
    })
}

/// Loads the already-produced `h.json` in `root` with the pinned upstream
/// implementation and returns its real load/scan snapshot.
pub fn load_produced_session(root: &Path) -> Result<Value, String> {
    let script = json!({
        "cases": [{
            "name": "remaining",
            "mode": "load_existing",
        }]
    });
    let response = run_python(root, &script)?;
    let record = response["records"]
        .as_array()
        .and_then(|records| records.first())
        .cloned()
        .ok_or_else(|| "upstream loader returned no record".to_string())?;
    if record["outcome"] != json!("return") || record["ret"][0] != json!(0) {
        return Err(format!("upstream load_session did not succeed: {record}"));
    }
    Ok(record)
}

fn run_python(root: &Path, script: &Value) -> Result<Value, String> {
    let runner = scripts_dir().join("session_runner.py");
    if !runner.is_file() {
        return Err(format!("session runner is missing: {}", runner.display()));
    }
    fs::copy(&runner, root.join("session_runner.py"))
        .map_err(|error| format!("cannot copy session runner: {error}"))?;
    let script_path = root.join("cases.json");
    fs::write(&script_path, serde_json::to_string_pretty(script).unwrap())
        .map_err(|error| error.to_string())?;
    let result_path = root.join("oracle-result.json");
    let upstream = crate::oracle::verify_pinned_upstream()?;
    let output = Command::new(python_executable())
        .arg(root.join("session_runner.py"))
        .arg("--script")
        .arg(&script_path)
        .arg("--result")
        .arg(&result_path)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("TMPDIR", root)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONPATH", &upstream)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|error| format!("cannot run session oracle: {error}"))?;
    let _ = fs::write(root.join("oracle-stdout.txt"), &output.stdout);
    let _ = fs::write(root.join("oracle-stderr.txt"), &output.stderr);
    if !output.status.success() {
        return Err(format!(
            "session oracle failed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = fs::read_to_string(&result_path)
        .map_err(|error| format!("cannot read session oracle result: {error}"))?;
    serde_json::from_str(&text)
        .map_err(|error| format!("session oracle result is not JSON: {error}"))
}

fn python_success(record: &Value) -> bool {
    record["outcome"] == json!("return") && record["ret"][0] == json!(0)
}

fn rust_success(record: &Value) -> bool {
    record["outcome"] == json!("success")
}

#[derive(Clone, Copy)]
enum OracleFailure {
    Exception(&'static str),
    Return(i64),
}

struct FailureCase {
    oracle: OracleFailure,
    rss_class: &'static str,
    rss_code: Option<i64>,
}

/// The explicit failure mapping accepted from the raw oracle run. There is no
/// wildcard: an unknown failure name, a different upstream exception/return, a
/// different RSS class/code, or any post-state change is a failure.
fn failure_case(name: &str) -> Option<FailureCase> {
    match name {
        "unreadable_session" => Some(FailureCase {
            oracle: OracleFailure::Exception("PermissionError"),
            rss_class: "host_failure",
            rss_code: None,
        }),
        "bad_json" => Some(FailureCase {
            oracle: OracleFailure::Return(1103),
            rss_class: "parse",
            rss_code: Some(1103),
        }),
        "missing_tasks" => Some(FailureCase {
            oracle: OracleFailure::Exception("KeyError"),
            rss_class: "missing_tasks",
            rss_code: None,
        }),
        "tasks_type" => Some(FailureCase {
            oracle: OracleFailure::Exception("AttributeError"),
            rss_class: "structure",
            rss_code: None,
        }),
        "missing_guid" => Some(FailureCase {
            oracle: OracleFailure::Exception("KeyError"),
            rss_class: "missing_guid",
            rss_code: None,
        }),
        "outside_root" => Some(FailureCase {
            oracle: OracleFailure::Return(0),
            rss_class: "host_failure",
            rss_code: None,
        }),
        _ => None,
    }
}

fn compare_cases(
    oracle: &Value,
    candidate: &Value,
    failures: &mut Vec<String>,
    deltas: &mut Vec<String>,
) -> usize {
    let Some(expected_records) = oracle["records"].as_array() else {
        failures.push("oracle records are absent".to_string());
        return 0;
    };
    let Some(actual_records) = candidate["records"].as_array() else {
        failures.push("rss records are absent".to_string());
        return 0;
    };
    if expected_records.len() != actual_records.len() {
        failures.push(format!(
            "case count {} vs {}",
            expected_records.len(),
            actual_records.len()
        ));
        return 0;
    }
    let mut compared = 0;
    for (expected, actual) in expected_records.iter().zip(actual_records.iter()) {
        let name = expected["name"].as_str().unwrap_or("?");
        compared += 1;
        if expected["name"] != actual["name"] {
            failures.push(format!("case order differs: {name} vs {}", actual["name"]));
            continue;
        }
        let oracle_ok = python_success(expected);
        let rss_ok = rust_success(actual);
        let Some(expectation) = failure_case(name) else {
            if !oracle_ok || !rss_ok {
                failures.push(format!(
                    "{name}: unexpected failure (oracle {}, RSS {})",
                    expected["outcome"], actual["outcome"]
                ));
                continue;
            }
            compare_value(
                &expected["ret"],
                &actual["ret"],
                &format!("{name}.ret"),
                failures,
            );
            for pointer in ["tasks", "queue", "cookies", "header", "login", "disk"] {
                compare_value(
                    &expected[pointer],
                    &actual[pointer],
                    &format!("{name}.{pointer}"),
                    failures,
                );
            }
            if expected["mode"] == json!("save") {
                deltas.push(format!(
                    "{name}: saved JSON whitespace differs between the two encoders; the decoded task/cookie content and order are compared through a fresh load"
                ));
            }
            continue;
        };

        // Upstream side: the recorded exception class or return code must
        // still be exactly what the case table declares.
        match expectation.oracle {
            OracleFailure::Exception(class) => {
                if expected["outcome"] != json!("exception")
                    || expected["exception"] != json!(class)
                {
                    failures.push(format!(
                        "{name}: upstream outcome {:?}/exception {:?} is not the recorded {class}",
                        expected["outcome"], expected["exception"]
                    ));
                }
            }
            OracleFailure::Return(code) => {
                if expected["outcome"] != json!("return") || expected["ret"][0] != json!(code) {
                    failures.push(format!(
                        "{name}: upstream return {:?} is not the recorded code {code}",
                        expected["ret"]
                    ));
                }
            }
        }
        if name == "outside_root" && !oracle_ok {
            failures.push(format!("{name}: upstream must publish instead of failing"));
        }
        if name != "outside_root" && rss_ok {
            failures.push(format!("{name}: RSS must fail"));
        }
        if name == "outside_root" && rss_ok {
            failures.push(format!("{name}: RSS must refuse the untrusted root"));
        }
        if !rss_ok {
            if actual["class"] != json!(expectation.rss_class)
                || actual["code"] != json!(expectation.rss_code)
            {
                failures.push(format!(
                    "{name}: RSS class/code {:?}/{} is not the recorded {}/{:?}",
                    actual["class"], actual["code"], expectation.rss_class, expectation.rss_code
                ));
            }
            if name == "outside_root"
                && !actual["value"]
                    .as_str()
                    .unwrap_or("")
                    .contains(ROOT_DENIED_TAG)
            {
                failures.push(format!(
                    "{name}: the failure is not an explicit root denial: {actual}"
                ));
            }
        }

        // The upstream post-state must equal its own pre-invocation baseline,
        // except outside_root, whose recorded upstream behavior does publish.
        if name == "outside_root" {
            let tasks = expected["tasks"].as_array().cloned().unwrap_or_default();
            if tasks.len() != 1
                || tasks[0]["guid"] != json!("outside1")
                || tasks[0]["meta"]["finished"] != json!(0)
            {
                failures.push(format!(
                    "{name}: upstream no longer publishes the recorded out-of-root task: {expected}"
                ));
            }
        } else {
            for pointer in ["tasks", "queue", "cookies", "header", "login", "disk"] {
                compare_value(
                    &expected["pre"][pointer],
                    &expected[pointer],
                    &format!("{name}.oracle_post.{pointer}"),
                    failures,
                );
            }
        }
        // RSS post-state must equal the independent pre-invocation baseline.
        if !rss_ok {
            for pointer in ["tasks", "queue", "cookies", "header", "login", "disk"] {
                compare_value(
                    &actual["pre"][pointer],
                    &actual[pointer],
                    &format!("{name}.rss_post.{pointer}"),
                    failures,
                );
            }
        }
        let upstream = match expectation.oracle {
            OracleFailure::Exception(class) => format!("raises {class}"),
            OracleFailure::Return(code) => format!("returns {code}"),
        };
        deltas.push(format!(
            "{name}: upstream {upstream}; RSS {} {:?}; both post-states checked against the pre-invocation baseline{}",
            expectation.rss_class,
            expectation.rss_code,
            if name == "outside_root" {
                "; upstream publishes the out-of-root task, RSS refuses before publication"
            } else {
                ""
            }
        ));
    }
    compared
}

fn compare_value(expected: &Value, actual: &Value, path: &str, failures: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in left {
                match right.get(key) {
                    Some(other) => compare_value(value, other, &format!("{path}.{key}"), failures),
                    None => failures.push(format!("{path}.{key}: missing in the candidate")),
                }
            }
            for key in right.keys() {
                if !left.contains_key(key) {
                    failures.push(format!("{path}.{key}: not produced upstream"));
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            if left.len() != right.len() {
                failures.push(format!("{path}: length {} vs {}", left.len(), right.len()));
                return;
            }
            for (index, (a, b)) in left.iter().zip(right.iter()).enumerate() {
                compare_value(a, b, &format!("{path}[{index}]"), failures);
            }
        }
        _ => {
            if expected != actual {
                failures.push(format!("{path}: expected {expected}, got {actual}"));
            }
        }
    }
}

fn mutate_case(records: &Value, name: &str, mutate: impl Fn(&mut Value)) -> Value {
    let mut clone = records.clone();
    if let Some(record) = clone["records"].as_array_mut().and_then(|records| {
        records
            .iter_mut()
            .find(|record| record["name"] == json!(name))
    }) {
        mutate(record);
    }
    clone
}

fn mutation_rejected(
    oracle: &Value,
    candidate: &Value,
    name: &str,
    mutate: impl Fn(&mut Value),
    needle: &str,
) -> bool {
    let mutant = mutate_case(candidate, name, mutate);
    let mut failures = Vec::new();
    let mut ignored = Vec::new();
    compare_cases(oracle, &mutant, &mut failures, &mut ignored);
    failures.iter().any(|failure| failure.contains(needle))
}

/// Mutation gates for the failure cases. The production comparator must reject
/// a wrong class/code and every post-state change, not accept any failure pair.
fn failure_negative_gates(oracle: &Value, candidate: &Value) -> [bool; 8] {
    let class_code = mutation_rejected(
        oracle,
        candidate,
        "bad_json",
        |record| {
            record["class"] = json!("structure");
        },
        "bad_json",
    );
    let code = mutation_rejected(
        oracle,
        candidate,
        "bad_json",
        |record| {
            record["code"] = json!(0);
        },
        "bad_json",
    );
    let task = mutation_rejected(
        oracle,
        candidate,
        "unreadable_session",
        |record| {
            record["tasks"] = json!([{"guid": "injected", "state": 10}]);
        },
        "unreadable_session",
    );
    let cookie = mutation_rejected(
        oracle,
        candidate,
        "missing_tasks",
        |record| {
            record["cookies"] = json!([["nw", "mutated"]]);
        },
        "missing_tasks",
    );
    let queue = mutation_rejected(
        oracle,
        candidate,
        "missing_guid",
        |record| {
            record["queue"] = json!(["injected-queue"]);
        },
        "missing_guid",
    );
    let header = mutation_rejected(
        oracle,
        candidate,
        "tasks_type",
        |record| {
            record["header"] = json!("nw=mutated");
        },
        "tasks_type",
    );
    let login = mutation_rejected(
        oracle,
        candidate,
        "outside_root",
        |record| {
            record["login"] = json!(true);
        },
        "outside_root",
    );
    let disk = mutation_rejected(
        oracle,
        candidate,
        "bad_json",
        |record| {
            record["disk"] = json!([["injected.jpg", 1]]);
        },
        "bad_json",
    );
    [class_code, code, task, cookie, queue, header, login, disk]
}

fn negative_gates(oracle: &Value, candidate: &Value) -> (bool, bool, bool, bool) {
    let mut failures = Vec::new();
    let mut ignored = Vec::new();
    let task_mutant = mutate_case(candidate, "load_order", |record| {
        record["tasks"][0]["guid"] = json!("mutated-guid");
    });
    compare_cases(oracle, &task_mutant, &mut failures, &mut ignored);
    let task_rejected = failures.iter().any(|failure| failure.contains("guid"));

    let mut failures = Vec::new();
    let mut ignored = Vec::new();
    let cookie_mutant = mutate_case(candidate, "legacy_valid", |record| {
        record["cookies"][1][1] = json!("mutated-cookie");
    });
    compare_cases(oracle, &cookie_mutant, &mut failures, &mut ignored);
    let cookie_rejected = failures.iter().any(|failure| failure.contains("cookie"));

    let mut failures = Vec::new();
    let mut ignored = Vec::new();
    let scan_mutant = mutate_case(candidate, "load_scan", |record| {
        if let Some(task) = record["tasks"].as_array_mut().and_then(|tasks| {
            tasks
                .iter_mut()
                .find(|task| task["guid"] == json!("scan0001"))
        }) {
            task["meta"]["finished"] = json!(99);
            record["queue"] = json!(["broken-queue"]);
        }
    });
    compare_cases(oracle, &scan_mutant, &mut failures, &mut ignored);
    let scan_rejected = failures.iter().any(|failure| failure.contains("finished"));
    let queue_rejected = failures.iter().any(|failure| failure.contains("queue"));

    let mut failures = Vec::new();
    let mut ignored = Vec::new();
    let return_mutant = mutate_case(candidate, "load_order", |record| {
        record["ret"][0] = json!(1103);
    });
    compare_cases(oracle, &return_mutant, &mut failures, &mut ignored);
    let return_rejected = failures.iter().any(|failure| {
        failure.contains("code") || failure.contains("outcome") || failure.contains("ret")
    });
    (
        task_rejected,
        cookie_rejected,
        scan_rejected && queue_rejected,
        return_rejected,
    )
}

// ---- rss-test --suite session ---------------------------------------------

fn expect_success(report: &Value, expect: &Value, failures: &mut Vec<String>, name: &str) {
    let expected_success = expect["success"].as_bool().unwrap_or(false);
    let actual_success = report["outcome"] == json!("success");
    if expected_success != actual_success {
        failures.push(format!(
            "{name}: success {expected_success} vs {actual_success}: {report}"
        ));
        return;
    }
    if !expected_success {
        if let Some(code) = expect.get("code") {
            if &report["code"] != code {
                failures.push(format!("{name}: code {} vs {code}", report["code"]));
            }
        }
        if let Some(prefix) = expect["error_prefix"].as_str() {
            let class = report["class"].as_str().unwrap_or("");
            let observed = format!("session:{class}");
            if !observed.starts_with(prefix) {
                failures.push(format!(
                    "{name}: error {observed} does not start with {prefix}"
                ));
            }
        }
        return;
    }
    if let Some(queue) = expect.get("queue") {
        if report["queue"] != *queue {
            failures.push(format!("{name}: queue {} vs {queue}", report["queue"]));
        }
    }
    if expect.get("header").is_some() && report["header"] != expect["header"] {
        failures.push(format!(
            "{name}: header {} vs {}",
            report["header"], expect["header"]
        ));
    }
    if let Some(login) = expect.get("login") {
        if report["login"] != *login {
            failures.push(format!("{name}: login {} vs {login}", report["login"]));
        }
    }
    if let Some(expected_names) = expect["cookie_names"].as_array() {
        let names: Vec<Value> = report["cookies"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|pair| pair[0].clone())
            .collect();
        if Value::Array(names) != Value::Array(expected_names.clone()) {
            failures.push(format!(
                "{name}: cookie names {:?} vs {expected_names:?}",
                report["cookies"]
            ));
        }
    }
    if let Some(finished) = expect["finished"].as_object() {
        for (guid, count) in finished {
            let observed = report["tasks"]
                .as_array()
                .and_then(|tasks| {
                    tasks
                        .iter()
                        .find(|task| task["guid"] == Value::String(guid.clone()))
                })
                .map(|task| task["meta"]["finished"].clone())
                .unwrap_or(Value::Null);
            if observed != *count {
                failures.push(format!("{name}: {guid} finished {observed} vs {count}"));
            }
        }
    }
    if let Some(states) = expect["state"].as_object() {
        for (guid, state) in states {
            let observed = report["tasks"]
                .as_array()
                .and_then(|tasks| {
                    tasks
                        .iter()
                        .find(|task| task["guid"] == Value::String(guid.clone()))
                })
                .map(|task| task["state"].clone())
                .unwrap_or(Value::Null);
            if observed != *state {
                failures.push(format!("{name}: {guid} state {observed} vs {state}"));
            }
        }
    }
    if let Some(expected_disk) = expect["disk"].as_array() {
        let observed: Vec<Value> = report["disk"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .iter()
            .map(|entry| entry[0].clone())
            .collect();
        if Value::Array(observed) != Value::Array(expected_disk.clone()) {
            failures.push(format!(
                "{name}: disk {expected_disk:?} not observed: {report}"
            ));
        }
    }
    if let Some(empty) = expect["saved_tasks_empty"].as_bool() {
        let tasks = report["tasks"].as_array().map(Vec::len).unwrap_or(0);
        if empty && tasks != 0 {
            failures.push(format!("{name}: saved tasks should be empty: {report}"));
        }
    }
}

pub fn run_session_suite() -> Result<(), String> {
    let root = lease_tmp()?.join(format!("session-suite-{}-{}", std::process::id(), stamp()));
    if root.exists() {
        return Err(format!("refusing to reuse suite root {}", root.display()));
    }
    fs::create_dir_all(&root).map_err(|error| error.to_string())?;
    let script = script();
    let mut failures = Vec::new();
    let mut cases = 0usize;
    let records = run_rss(&script, &root)?;
    for (case, record) in script["cases"]
        .as_array()
        .unwrap()
        .iter()
        .zip(records["records"].as_array().unwrap())
    {
        cases += 1;
        expect_success(
            record,
            &case["expect"],
            &mut failures,
            case["name"].as_str().unwrap_or("?"),
        );
    }
    if failures.is_empty() {
        println!("session-suite cases={cases} passed={cases}");
        println!("output={}", root.display());
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("session-suite failure: {failure}");
        }
        Err(format!(
            "session suite failed: {} of {cases} cases (output {})",
            failures.len(),
            root.display()
        ))
    }
}

// ---- diff --suite session --------------------------------------------------

pub fn run_session_diff() -> Result<(), String> {
    let root = match std::env::var_os("XEH_SESSION_DIFF_OUT") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => lease_tmp()?.join(format!("session-diff-{}-{}", std::process::id(), stamp())),
    };
    if root.exists() {
        return Err(format!(
            "refusing to reuse session diff output: {}",
            root.display()
        ));
    }
    fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    let script = script();
    let oracle = run_python(&root, &script)?;
    let candidate = run_rss(&script, &root)?;
    fs::write(
        root.join("oracle.json"),
        serde_json::to_string_pretty(&oracle).unwrap(),
    )
    .map_err(|error| error.to_string())?;
    fs::write(
        root.join("rss.json"),
        serde_json::to_string_pretty(&candidate).unwrap(),
    )
    .map_err(|error| error.to_string())?;

    let mut failures = Vec::new();
    let mut deltas = vec![
        format!(
            "session-oracle: pinned upstream {UPSTREAM_APP_REV}, read-only checkout verified before each run"
        ),
        "session-order: task/cookie member order is carried by the Core ordered object-entry codec; comparing decoded JSON objects alone would hide it".to_string(),
        "session-epochs: controller owner nonces and epoch budgets are RSS-owned and have no upstream counterpart".to_string(),
    ];
    let compared = compare_cases(&oracle, &candidate, &mut failures, &mut deltas);
    let (task_rejected, cookie_rejected, scan_queue_rejected, return_rejected) =
        negative_gates(&oracle, &candidate);
    let failure_gates = failure_negative_gates(&oracle, &candidate);
    let failure_names = [
        "class_code",
        "code",
        "task",
        "cookie",
        "queue",
        "header",
        "login",
        "disk",
    ];
    if !task_rejected {
        failures.push("negative gate: a changed task guid was not rejected".to_string());
    }
    if !cookie_rejected {
        failures.push("negative gate: a changed cookie value was not rejected".to_string());
    }
    if !scan_queue_rejected {
        failures.push("negative gate: a changed scan/queue value was not rejected".to_string());
    }
    if !return_rejected {
        failures.push("negative gate: a changed return value was not rejected".to_string());
    }
    for (name, rejected) in failure_names.iter().zip(failure_gates.iter()) {
        if !rejected {
            failures.push(format!(
                "failure negative gate: a changed {name} on a failure case was not rejected"
            ));
        }
    }
    let report = json!({
        "ok": failures.is_empty(),
        "compared_cases": compared,
        "failures": failures,
        "deltas": deltas,
        "negative": {
            "task": task_rejected,
            "cookie": cookie_rejected,
            "scan_queue": scan_queue_rejected,
            "return": return_rejected,
            "failure": failure_names
                .iter()
                .zip(failure_gates.iter())
                .map(|(name, rejected)| ((*name).to_string(), Value::Bool(*rejected)))
                .collect::<serde_json::Map<String, Value>>()
        }
    });
    fs::write(
        root.join("compare.json"),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .map_err(|error| error.to_string())?;
    println!("diff=session");
    println!("output={}", root.display());
    println!("compared_cases={compared}");
    println!(
        "negative=task:{},cookie:{},scan_queue:{},return:{}",
        if task_rejected {
            "rejected"
        } else {
            "accepted"
        },
        if cookie_rejected {
            "rejected"
        } else {
            "accepted"
        },
        if scan_queue_rejected {
            "rejected"
        } else {
            "accepted"
        },
        if return_rejected {
            "rejected"
        } else {
            "accepted"
        }
    );
    let failure_summary = failure_names
        .iter()
        .zip(failure_gates.iter())
        .map(|(name, rejected)| {
            format!("{name}:{}", if *rejected { "rejected" } else { "accepted" })
        })
        .collect::<Vec<_>>()
        .join(",");
    println!("negative_failure={failure_summary}");
    for delta in &deltas {
        println!("delta={delta}");
    }
    if failures.is_empty() {
        println!("session-diff=pass");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("session-diff failure: {failure}");
        }
        Err(format!(
            "session differential failed with {} mismatches",
            failures.len()
        ))
    }
}
