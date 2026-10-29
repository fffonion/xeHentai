//! Task-control suite and upstream differential commands.
//!
//! `rss-test --suite state` runs the RSS control owner through the real
//! RustScript entry. `diff --suite task-api` feeds one identical step script
//! to the pinned upstream Python implementation (test oracle only) and to the
//! RSS owner, then compares the raw return values, task tables, config, queue,
//! and update-presence flags step by step. Nothing is sorted, dropped, or
//! rewritten except the known non-deterministic `ignored_errors` order, which
//! is reported as a delta.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use xeh_host::{ControlOptions, TaskControl};

use crate::paths::{lease_tmp, python_executable, scripts_dir, workspace_root};

const GALLERY_A: &str = "https://e-hentai.org/g/123456/0123456789/";
const GALLERY_B: &str = "https://e-hentai.org/g/654321/abcdef0123/";
const GALLERY_C: &str = "https://e-hentai.org/g/777777/8888888888/";
const EXHENTAI: &str = "https://exhentai.org/g/111111/2222222222/";

fn stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or(0)
}

fn write_json(dir: &Path, name: &str, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    fs::write(dir.join(name), text + "\n").map_err(|error| format!("cannot write {name}: {error}"))
}

// ---- RSS driver ------------------------------------------------------------

fn command(control: &mut TaskControl, request: &Value) -> Result<Value, String> {
    let text = control
        .invoke(&request.to_string())
        .map_err(|error| format!("task command {request} failed: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("report is not JSON: {error}\n{text}"))
}

fn open_fixture(guids: &[&str]) -> Result<TaskControl, String> {
    TaskControl::open(&ControlOptions::fixture(guids, 0)).map_err(|error| error.to_string())
}

fn records(report: &Value) -> Vec<Value> {
    report["tasks"].as_array().cloned().unwrap_or_default()
}

fn state_of(report: &Value, guid: &str) -> i64 {
    records(report)
        .iter()
        .find(|task| task["guid"] == json!(guid))
        .and_then(|task| task["state"].as_i64())
        .unwrap_or(-999)
}

fn expect(value: &Value, pointer: &str, expected: Value, failures: &mut Vec<String>) {
    let actual = value.pointer(pointer).cloned().unwrap_or(Value::Null);
    if actual != expected {
        failures.push(format!("{pointer}: expected {expected}, got {actual}"));
    }
}

// ---- rss-test --suite state ------------------------------------------------

fn suite() -> Result<(), String> {
    let mut failures = Vec::new();
    let mut cases = 0usize;
    let mut control = open_fixture(&["0000000a", "0000000b", "0000000c"])?;

    // Lifecycle: create, update the same owner, read the original task.
    let created = command(
        &mut control,
        &json!({"op": "add", "url": GALLERY_A, "overrides": {"download_timeout": 2.5, "extra_flag": true}}),
    )?;
    cases += 1;
    if created["code"] != json!(0) || created["value"] != json!("0000000a") {
        failures.push(format!("add: {created}"));
    }
    let before = command(&mut control, &json!({"op": "snapshot"}))?;
    let task = &records(&before)[0];
    if task["config"]["download_timeout"] != json!(2.5)
        || task["config"]["extra_flag"] != json!(true)
        || task["config"].as_object().map(Map::len) != Some(15)
    {
        failures.push(format!("task configuration: {task}"));
    }
    let updated = command(
        &mut control,
        &json!({"op": "update", "overlay": {"dir": "after", "download_thread_cnt": 2, "proxy": [], "ignored_errors": [1009]}}),
    )?;
    cases += 1;
    expect(&updated, "/code", json!(0), &mut failures);
    expect(&updated, "/config/dir", json!("after"), &mut failures);
    let after = command(&mut control, &json!({"op": "snapshot"}))?;
    if records(&after)[0]["config"] != records(&before)[0]["config"] {
        failures.push("the stored task configuration followed the global update".to_string());
    }

    // State matrix.
    let guid = "0000000a".to_string();
    for state in [2, 3, 4, 5, 10, 19] {
        command(
            &mut control,
            &json!({"op": "seed_state", "guid": guid, "state": state}),
        )?;
        let deleted = command(&mut control, &json!({"op": "del", "guid": guid}))?;
        cases += 1;
        if deleted["code"] != json!(1105) {
            failures.push(format!("delete state {state}: {deleted}"));
        }
        let resumed = command(&mut control, &json!({"op": "resume", "guid": guid}))?;
        cases += 1;
        if resumed["code"] != json!(1107) {
            failures.push(format!("resume state {state}: {resumed}"));
        }
        let paused = command(&mut control, &json!({"op": "pause", "guid": guid}))?;
        cases += 1;
        if paused["code"] != json!(0) || state_of(&paused, &guid) != 0 {
            failures.push(format!("pause state {state}: {paused}"));
        }
        command(
            &mut control,
            &json!({"op": "seed_state", "guid": guid, "state": 1}),
        )?;
    }
    command(
        &mut control,
        &json!({"op": "seed_state", "guid": guid, "state": 20}),
    )?;
    let finished_resume = command(&mut control, &json!({"op": "resume", "guid": guid}))?;
    cases += 1;
    if finished_resume["code"] != json!(0) || state_of(&finished_resume, &guid) != 20 {
        failures.push(format!("finished resume: {finished_resume}"));
    }

    // Unknown guid keeps the original code and a null value.
    let unknown = command(&mut control, &json!({"op": "del", "guid": "ffffffff"}))?;
    cases += 1;
    if unknown["code"] != json!(1101) || unknown["value"] != Value::Null {
        failures.push(format!("unknown del: {unknown}"));
    }

    // Logged-out exhentai fails with 1001 and stays in the table.
    let created_b = command(&mut control, &json!({"op": "add", "url": GALLERY_B}))?;
    cases += 1;
    if created_b["code"] != json!(0) {
        failures.push(format!("second add: {created_b}"));
    }
    let exhentai = command(&mut control, &json!({"op": "add", "url": EXHENTAI}))?;
    cases += 1;
    if exhentai["code"] != json!(1001) || exhentai["value"] != Value::Null {
        failures.push(format!("exhentai guard: {exhentai}"));
    }

    // Pause invalidates the ticket that was prepared before it.
    let ticket = ticket_of(&command(
        &mut control,
        &json!({"op": "prepare", "guid": "0000000b"}),
    )?)?;
    let paused = command(&mut control, &json!({"op": "pause", "guid": "0000000b"}))?;
    cases += 1;
    if paused["code"] != json!(0) || state_of(&paused, "0000000b") != 0 {
        failures.push(format!("pause before stale event: {paused}"));
    }
    let stale = command(
        &mut control,
        &json!({"op": "event", "ticket": ticket, "kind": "finished"}),
    )?;
    cases += 1;
    if stale["event"]["reason"] != json!("stale_epoch") || state_of(&stale, "0000000b") != 0 {
        failures.push(format!("stale ticket: {stale}"));
    }

    if failures.is_empty() {
        println!("state-suite cases={cases} passed={cases}");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("state-suite failure: {failure}");
        }
        Err(format!(
            "state suite failed: {} of {cases} cases",
            failures.len()
        ))
    }
}

fn ticket_of(report: &Value) -> Result<Value, String> {
    let text = report["value"]
        .as_str()
        .ok_or_else(|| format!("ticket is absent: {report}"))?;
    serde_json::from_str(text).map_err(|error| format!("ticket is not JSON: {error}"))
}

// ---- diff --suite task-api -------------------------------------------------

fn script() -> Value {
    json!({
        "guids": [
            "0000000a", "0000000b", "0000000c", "0000000d", "0000000e",
            "0000000f", "00000010", "00000011", "0000000a", "0000000a",
            "00000012", "00000013"
        ],
        "steps": [
            {"op": "add", "url": ""},
            {"op": "add", "url": "https://example.com/g/1/x/"},
            {"op": "add", "url": GALLERY_A},
            {"op": "add", "url": GALLERY_A},
            {"op": "add", "url": EXHENTAI},
            {"op": "set_login", "value": true},
            {"op": "add", "url": EXHENTAI},
            {"op": "add", "url": GALLERY_A, "overrides": {
                "dir": "task-dir",
                "download_timeout": 2.5,
                "ignored_errors": [1006],
                "custom_flag": true,
                "custom_empty": "",
                "custom_zero": 0,
                "custom_null": null,
                "custom_range": [[1, 2]]
            }},
            {"op": "update", "overlay": {
                "dir": "after-update",
                "download_thread_cnt": 2,
                "proxy": [],
                "ignored_errors": [1009]
            }},
            {"op": "add", "url": GALLERY_B},
            {"op": "seed_state", "guid": "0000000c", "state": 3},
            {"op": "pause", "guid": "0000000c"},
            {"op": "pause", "guid": "0000000c"},
            {"op": "seed_state", "guid": "0000000d", "state": 20},
            {"op": "resume", "guid": "0000000d"},
            {"op": "seed_state", "guid": "00000010", "state": -1},
            {"op": "resume", "guid": "00000010"},
            {"op": "seed_state", "guid": "00000011", "state": 10},
            {"op": "resume", "guid": "00000011"},
            {"op": "seed_state", "guid": "00000011", "state": 19},
            {"op": "del", "guid": "00000011"},
            {"op": "del", "guid": "ffffffff"},
            {"op": "seed_state", "guid": "0000000d", "state": 20},
            {"op": "del", "guid": "0000000d"},
            {"op": "del", "guid": "0000000d"},
            {"op": "seed_state", "guid": "0000000f", "state": 0},
            {"op": "del", "guid": "0000000f"},
            {"op": "pause", "guid": "ffffffff"},
            {"op": "resume", "guid": "ffffffff"},
            {"op": "seed_state", "guid": "0000000c", "state": 20},
            {"op": "add", "url": GALLERY_C},
            {"op": "add", "url": GALLERY_C},
            {"op": "add", "url": format!(" \t{GALLERY_A}\r\n")},
            {"op": "add", "url": format!("\u{3000}{GALLERY_A}\u{3000}")}
        ]
    })
}

fn run_python(root: &Path, script_path: &Path) -> Result<Value, String> {
    let script = scripts_dir().join("task_runner.py");
    fs::copy(&script, root.join("task_runner.py"))
        .map_err(|error| format!("cannot copy task runner: {error}"))?;
    let result_path = root.join("oracle-result.json");
    let upstream = workspace_root().join("compat/upstream/xeHentai");
    let output = Command::new(python_executable())
        .arg(root.join("task_runner.py"))
        .arg("--script")
        .arg(script_path)
        .arg("--result")
        .arg(&result_path)
        .current_dir(root)
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONPATH", &upstream)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .map_err(|error| format!("cannot run upstream task oracle: {error}"))?;
    let _ = fs::write(root.join("oracle-stdout.txt"), &output.stdout);
    let _ = fs::write(root.join("oracle-stderr.txt"), &output.stderr);
    if !output.status.success() {
        return Err(format!(
            "upstream task oracle failed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = fs::read_to_string(&result_path)
        .map_err(|error| format!("cannot read upstream task result: {error}"))?;
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "upstream task result is not JSON: {error}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn run_rss(script: &Value) -> Result<Value, String> {
    let guids: Vec<String> = script["guids"]
        .as_array()
        .ok_or("script is missing guids")?
        .iter()
        .map(|guid| guid.as_str().unwrap_or_default().to_string())
        .collect();
    let borrowed: Vec<&str> = guids.iter().map(String::as_str).collect();
    let mut control = TaskControl::open(&ControlOptions::fixture(&borrowed, 0))
        .map_err(|error| error.to_string())?;
    let mut records = Vec::new();
    for (index, step) in script["steps"]
        .as_array()
        .ok_or("script is missing steps")?
        .iter()
        .enumerate()
    {
        let report = command(&mut control, step)?;
        records.push(json!({
            "index": index,
            "op": step["op"],
            "setup": matches!(
                step["op"].as_str(),
                Some("set_login") | Some("seed_state") | Some("seed_meta")
            ),
            "ret": [report["code"].clone(), report["value"].clone()],
            "presence": report["presence"].clone(),
            "tasks": report["tasks"].clone(),
            "queue": report["queue"].clone(),
            "config": report["config"].clone(),
        }));
    }
    Ok(json!({"records": records}))
}

/// Compares two JSON values, treating `ignored_errors` as an order-insensitive
/// set while recording an order delta when the multisets agree.
fn compare_value(
    original: &Value,
    candidate: &Value,
    path: &str,
    failures: &mut Vec<String>,
    deltas: &mut Vec<String>,
) {
    match (original, candidate) {
        (Value::Object(left), Value::Object(right)) => {
            for (key, value) in left {
                let child = format!("{path}.{key}");
                match right.get(key) {
                    Some(other) => compare_value(value, other, &child, failures, deltas),
                    None => failures.push(format!("{child}: missing in the candidate")),
                }
            }
            for key in right.keys() {
                if !left.contains_key(key) {
                    failures.push(format!("{path}.{key}: not produced upstream"));
                }
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            if path.ends_with("ignored_errors") {
                let mut left_sorted: Vec<i64> = left.iter().filter_map(Value::as_i64).collect();
                let mut right_sorted: Vec<i64> = right.iter().filter_map(Value::as_i64).collect();
                left_sorted.sort_unstable();
                right_sorted.sort_unstable();
                if left_sorted != right_sorted {
                    failures.push(format!("{path}: {original} vs {candidate}"));
                } else if left != right {
                    deltas.push(format!("{path}: ignored_errors order differs"));
                }
                return;
            }
            if left.len() != right.len() {
                failures.push(format!("{path}: length {} vs {}", left.len(), right.len()));
                return;
            }
            for (index, (a, b)) in left.iter().zip(right.iter()).enumerate() {
                compare_value(a, b, &format!("{path}[{index}]"), failures, deltas);
            }
        }
        _ => {
            if original != candidate {
                failures.push(format!("{path}: expected {original}, got {candidate}"));
            }
        }
    }
}

fn compare_records(
    original: &Value,
    candidate: &Value,
    failures: &mut Vec<String>,
    deltas: &mut Vec<String>,
) -> usize {
    let Some(original_records) = original["records"].as_array() else {
        failures.push("oracle records are absent".to_string());
        return 0;
    };
    let Some(candidate_records) = candidate["records"].as_array() else {
        failures.push("rss records are absent".to_string());
        return 0;
    };
    if original_records.len() != candidate_records.len() {
        failures.push(format!(
            "step count {} vs {}",
            original_records.len(),
            candidate_records.len()
        ));
        return 0;
    }
    let mut compared = 0usize;
    for (index, (expected, actual)) in original_records
        .iter()
        .zip(candidate_records.iter())
        .enumerate()
    {
        let label = format!("step {index} ({})", expected["op"]);
        if expected["setup"].as_bool() != actual["setup"].as_bool() {
            failures.push(format!("{label}: setup marker differs"));
            continue;
        }
        if expected["setup"].as_bool() == Some(true) {
            continue;
        }
        compared += 1;
        compare_value(
            &expected["ret"],
            &actual["ret"],
            &format!("{label}.ret"),
            failures,
            deltas,
        );
        compare_value(
            &expected["tasks"],
            &actual["tasks"],
            &format!("{label}.tasks"),
            failures,
            deltas,
        );
        compare_value(
            &expected["queue"],
            &actual["queue"],
            &format!("{label}.queue"),
            failures,
            deltas,
        );
        compare_value(
            &expected["config"],
            &actual["config"],
            &format!("{label}.config"),
            failures,
            deltas,
        );
        compare_value(
            &expected["presence"],
            &actual["presence"],
            &format!("{label}.presence"),
            failures,
            deltas,
        );
    }
    compared
}

fn diff() -> Result<(), String> {
    let root = match std::env::var_os("XEH_TASK_DIFF_OUT") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => lease_tmp()?.join(format!("task-diff-{}-{}", std::process::id(), stamp())),
    };
    if root.exists() {
        return Err(format!(
            "refusing to reuse task diff output: {}",
            root.display()
        ));
    }
    fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;

    let script = script();
    let script_path = root.join("script.json");
    write_json(&root, "script.json", &script)?;
    let original = run_python(&root, &script_path)?;
    let candidate = run_rss(&script)?;

    let mut deltas = vec![
        "task-control: epochs, prepared tickets, completion counts, and stale-ticket rejection are RSS-owned control data with no upstream equivalent; only returns, the task table, config, queue, and update presence are compared".to_string(),
        "task-warnings: the upstream `download_ori` logged-out notice is a localized log line (Task13); the RSS report exposes `download_ori_requires_login` in `warnings`, which is not compared".to_string(),
        "task-guid-source: both sides consume the same planned 8-character prefixes; production generates a UUID v4 through the new `xeh_system::uuid_v4` seed and slices it in RSS like upstream `str(uuid4())[:8]`".to_string(),
    ];
    let mut failures = Vec::new();
    let compared = compare_records(&original, &candidate, &mut failures, &mut deltas);

    // Negative gates: a changed return value and a changed task state must be
    // rejected by the same comparator.
    let mut ret_mutant = candidate.clone();
    if let Some(record) = ret_mutant["records"]
        .as_array_mut()
        .and_then(|records| records.get_mut(2))
    {
        record["ret"][0] = json!(1);
    }
    let mut ret_failures = Vec::new();
    compare_records(&original, &ret_mutant, &mut ret_failures, &mut deltas);
    let ret_rejected = ret_failures.iter().any(|failure| failure.contains("ret"));

    let mut state_mutant = candidate.clone();
    if let Some(state) = state_mutant["records"]
        .as_array_mut()
        .and_then(|records| records.last_mut())
        .and_then(|record| record["tasks"].as_array_mut())
        .and_then(|tasks| tasks.first_mut())
        .and_then(|task| task.get_mut("state"))
    {
        *state = json!(99);
    }
    let mut state_failures = Vec::new();
    compare_records(&original, &state_mutant, &mut state_failures, &mut deltas);
    let state_rejected = state_failures
        .iter()
        .any(|failure| failure.contains("state"));

    if !ret_rejected {
        failures.push("negative gate: a changed return value was not rejected".to_string());
    }
    if !state_rejected {
        failures.push("negative gate: a changed task state was not rejected".to_string());
    }

    write_json(&root, "oracle.json", &original)?;
    write_json(&root, "rss.json", &candidate)?;
    write_json(
        &root,
        "compare.json",
        &json!({
            "ok": failures.is_empty(),
            "compared_steps": compared,
            "failures": failures,
            "deltas": deltas,
            "negative_ret_rejected": ret_rejected,
            "negative_state_rejected": state_rejected,
        }),
    )?;

    println!("diff=task-api");
    println!("output={}", root.display());
    println!("compared_steps={compared}");
    println!(
        "negative=ret:{},state:{}",
        if ret_rejected { "rejected" } else { "accepted" },
        if state_rejected {
            "rejected"
        } else {
            "accepted"
        }
    );
    for delta in &deltas {
        println!("delta={delta}");
    }
    if failures.is_empty() {
        println!("task-api-diff=pass");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("task-api-diff failure: {failure}");
        }
        Err(format!(
            "task-api differential failed with {} mismatches",
            failures.len()
        ))
    }
}

pub fn run_state_suite() -> Result<(), String> {
    suite()
}

pub fn run_task_diff() -> Result<(), String> {
    diff()
}
