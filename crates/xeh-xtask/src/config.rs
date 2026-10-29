//! Configuration suite and upstream differential commands.
//!
//! `rss-test --suite config` executes the fixed configuration entry through
//! the real RustScript host. `diff --suite config` feeds the same synthetic
//! JSON data to the pinned upstream Python implementation (test oracle only)
//! and to the RSS implementation, then compares the public values field by
//! field.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::paths::{lease_tmp, python_executable, scripts_dir, workspace_root};

const CONFIG_KEYS: usize = 25;
const PUBLIC_KEYS: usize = 21;
const TASK_KEYS: usize = 14;

fn stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or(0)
}

fn fresh_dir(label: &str) -> Result<PathBuf, String> {
    let path = lease_tmp()?.join(format!("config-{label}-{}-{}", std::process::id(), stamp()));
    fs::create_dir(&path).map_err(|error| format!("cannot create {}: {error}", path.display()))?;
    Ok(path)
}

fn write_file(root: &Path, name: &str, text: &str) -> Result<PathBuf, String> {
    let path = root.join(name);
    fs::write(&path, text).map_err(|error| format!("cannot write {}: {error}", path.display()))?;
    Ok(path)
}

fn fixture(name: &str) -> Result<String, String> {
    let path = workspace_root().join("compat/fixtures/config").join(name);
    fs::read_to_string(&path).map_err(|error| format!("cannot read {}: {error}", path.display()))
}

fn scenario(root: &Path, request: &Value) -> Result<Value, String> {
    let text = xeh_host::run_config_scenario(root, &request.to_string())
        .map_err(|error| format!("configuration scenario failed: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("scenario report is not JSON: {error}"))
}

fn request(root: &Path, op: &str, path: Option<&Path>, overlay: Option<&Path>) -> Value {
    let mut value = json!({
        "op": op,
        "base": root.to_str().unwrap_or_default(),
        "path": path.map(|path| path.to_str().unwrap_or_default()),
    });
    if let Some(overlay) = overlay {
        value["overlay"] = json!(overlay.to_str().unwrap_or_default());
    }
    value
}

fn object_len(value: Option<&Value>, expected: usize, label: &str, failures: &mut Vec<String>) {
    let length = value
        .and_then(Value::as_object)
        .map(|object| object.len())
        .unwrap_or(0);
    if length != expected {
        failures.push(format!("{label}: expected {expected} keys, got {length}"));
    }
}

fn expect(value: &Value, pointer: &str, expected: Value, failures: &mut Vec<String>) {
    let actual = value.pointer(pointer).cloned().unwrap_or(Value::Null);
    if actual != expected {
        failures.push(format!("{pointer}: expected {expected}, got {actual}"));
    }
}

fn suite() -> Result<(), String> {
    let mut failures = Vec::new();
    let mut cases = 0usize;
    let golden: Value = {
        let text = fs::read_to_string(workspace_root().join("compat/golden/config.json"))
            .map_err(|error| format!("cannot read config golden: {error}"))?;
        let parsed: Value = serde_json::from_str(&text)
            .map_err(|error| format!("config golden is not JSON: {error}"))?;
        parsed
            .get("keys")
            .cloned()
            .ok_or("config golden has no keys")?
    };

    // Case 1: no file at all uses the built-in defaults.
    let root = fresh_dir("suite-defaults")?;
    let defaults = scenario(&root, &request(&root, "load", None, None))?;
    cases += 1;
    if defaults.get("ok").and_then(Value::as_bool) != Some(true) {
        failures.push(format!("defaults load failed: {defaults}"));
    }
    if defaults.get("source").and_then(Value::as_str) != Some("defaults") {
        failures.push(format!("defaults source: {defaults}"));
    }
    if defaults.get("config") != Some(&golden) {
        failures.push("built-in defaults differ from the accepted golden".to_string());
    }
    object_len(defaults.get("config"), CONFIG_KEYS, "config", &mut failures);
    object_len(defaults.get("public"), PUBLIC_KEYS, "public", &mut failures);
    object_len(defaults.get("task"), TASK_KEYS, "task", &mut failures);

    // Case 2: a full user document overlays every key.
    let root = fresh_dir("suite-full")?;
    let full = write_file(&root, "config.json", &fixture("user_full.json")?)?;
    let report = scenario(&root, &request(&root, "load", Some(&full), None))?;
    cases += 1;
    expect(&report, "/config/daemon", json!(true), &mut failures);
    expect(&report, "/config/jpn_title", json!(false), &mut failures);
    expect(
        &report,
        "/config/download_thread_cnt",
        json!(0),
        &mut failures,
    );
    expect(
        &report,
        "/config/download_timeout",
        json!(2.5),
        &mut failures,
    );
    expect(
        &report,
        "/config/ignored_errors",
        json!([1006, -5, 1006]),
        &mut failures,
    );
    expect(&report, "/task/scan_thread_cnt", json!(3), &mut failures);

    // Case 3: partial document keeps defaults, explicit empty values survive.
    let root = fresh_dir("suite-partial")?;
    let partial = write_file(&root, "config.json", &fixture("user_partial.json")?)?;
    let report = scenario(&root, &request(&root, "load", Some(&partial), None))?;
    cases += 1;
    expect(&report, "/config/jpn_title", json!(false), &mut failures);
    expect(
        &report,
        "/config/download_thread_cnt",
        json!(0),
        &mut failures,
    );
    expect(&report, "/config/log_path", json!(""), &mut failures);
    expect(&report, "/config/scan_thread_cnt", json!(1), &mut failures);

    // Case 4: default file present under the base directory.
    let root = fresh_dir("suite-default-file")?;
    write_file(&root, "config.json", "{\"jpn_title\": false}\n")?;
    let report = scenario(&root, &request(&root, "load", None, None))?;
    cases += 1;
    if report.get("source").and_then(Value::as_str) != Some("file") {
        failures.push(format!("default file was not loaded: {report}"));
    }
    expect(&report, "/config/jpn_title", json!(false), &mut failures);

    // Case 5: explicit missing path is an error; implicit missing is defaults.
    let root = fresh_dir("suite-missing")?;
    let missing = root.join("absent.json");
    let report = scenario(&root, &request(&root, "load", Some(&missing), None))?;
    cases += 1;
    if report.get("ok").and_then(Value::as_bool) != Some(false)
        || report.get("error").and_then(Value::as_str) != Some("missing_file")
    {
        failures.push(format!("explicit missing file was not rejected: {report}"));
    }

    // Case 6: unknown key and type errors.
    let root = fresh_dir("suite-errors")?;
    let unknown = write_file(&root, "config.json", &fixture("unknown_key.json")?)?;
    let report = scenario(&root, &request(&root, "load", Some(&unknown), None))?;
    cases += 1;
    if report.get("error").and_then(Value::as_str) != Some("unknown_key:urls") {
        failures.push(format!("unknown key was not rejected: {report}"));
    }
    let typed = write_file(&root, "config.json", &fixture("type_error.json")?)?;
    let report = scenario(&root, &request(&root, "load", Some(&typed), None))?;
    cases += 1;
    if report.get("error").and_then(Value::as_str) != Some("type:daemon") {
        failures.push(format!("type error was not rejected: {report}"));
    }

    // Case 7: runtime update merges error codes and keeps the task snapshot.
    let root = fresh_dir("suite-update")?;
    let config = write_file(&root, "config.json", &fixture("user_full.json")?)?;
    let overlay = write_file(&root, "overlay.json", &fixture("overlay.json")?)?;
    let report = scenario(
        &root,
        &request(&root, "update", Some(&config), Some(&overlay)),
    )?;
    cases += 1;
    expect(
        &report,
        "/config/ignored_errors",
        json!([1006, -5, 1009, -3]),
        &mut failures,
    );
    expect(
        &report,
        "/config/download_thread_cnt",
        json!(2),
        &mut failures,
    );
    if report.get("task_before") != report.get("task_after") {
        failures.push("created task snapshot changed after the global update".to_string());
    }
    expect(
        &report,
        "/presence",
        json!({"proxy": true, "dir": true, "ignored_errors": true}),
        &mut failures,
    );

    // Case 8: a second independent root is unaffected by the first.
    let root = fresh_dir("suite-second")?;
    let report = scenario(&root, &request(&root, "load", None, None))?;
    cases += 1;
    expect(&report, "/config/scan_thread_cnt", json!(1), &mut failures);

    if failures.is_empty() {
        println!("config-suite cases={cases} passed={cases}");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("config-suite failure: {failure}");
        }
        Err(format!(
            "config suite failed: {} of {cases} cases",
            failures.len()
        ))
    }
}

fn multiset(value: &Value) -> Option<Vec<i64>> {
    let mut values: Vec<i64> = value.as_array()?.iter().filter_map(Value::as_i64).collect();
    values.sort_unstable();
    Some(values)
}

fn compare(
    original: &Value,
    candidate: &Value,
    label: &str,
    deltas: &mut Vec<String>,
) -> Vec<String> {
    let mut failures = Vec::new();
    for key in [
        "config",
        "public",
        "task",
        "task_before",
        "task_after",
        "presence",
    ] {
        let (Some(expected), Some(actual)) = (original.get(key), candidate.get(key)) else {
            if original.get(key).is_some() {
                failures.push(format!("{label}: missing {key} in the candidate"));
            }
            continue;
        };
        let (Some(expected_fields), Some(actual_fields)) =
            (expected.as_object(), actual.as_object())
        else {
            if expected != actual {
                failures.push(format!("{label}: {key} differs"));
            }
            continue;
        };
        for (field, expected_value) in expected_fields {
            let Some(actual_value) = actual_fields.get(field) else {
                failures.push(format!("{label}: {key}.{field} is missing"));
                continue;
            };
            if field == "ignored_errors" {
                if multiset(expected_value) != multiset(actual_value) {
                    failures.push(format!(
                        "{label}: {key}.ignored_errors differs: {expected_value} vs {actual_value}"
                    ));
                } else if expected_value != actual_value {
                    deltas.push(format!(
                        "{label}: {key}.ignored_errors order differs (upstream {expected_value}, rss {actual_value})"
                    ));
                }
            } else if expected_value != actual_value {
                failures.push(format!(
                    "{label}: {key}.{field} expected {expected_value}, got {actual_value}"
                ));
            }
        }
        for field in actual_fields.keys() {
            if !expected_fields.contains_key(field) {
                failures.push(format!("{label}: {key}.{field} is not produced upstream"));
            }
        }
    }
    failures
}

fn prepare_runner(root: &Path) -> Result<(), String> {
    fs::copy(
        scripts_dir().join("config_runner.py"),
        root.join("config_runner.py"),
    )
    .map_err(|error| format!("cannot copy config runner: {error}"))?;
    Ok(())
}

fn run_python(
    root: &Path,
    mode: &str,
    explicit: Option<&Path>,
    overlay: Option<&Path>,
) -> Result<Value, String> {
    let script = root.join("config_runner.py");
    let result_path = root.join(format!("oracle-result-{mode}.json"));
    let upstream = workspace_root().join("compat/upstream/xeHentai");
    let python = python_executable();
    let mut command = Command::new(&python);
    command
        .arg(&script)
        .arg("--mode")
        .arg(mode)
        .arg("--base")
        .arg(root)
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
        .env("XEH_FIXTURE_ROOT", root)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    if let Some(path) = explicit {
        command.arg("--explicit").arg(path);
    }
    if let Some(path) = overlay {
        command.arg("--overlay").arg(path);
    }
    let output = command
        .output()
        .map_err(|error| format!("cannot run upstream config oracle: {error}"))?;
    let _ = fs::write(root.join("oracle-stdout.txt"), &output.stdout);
    let _ = fs::write(root.join("oracle-stderr.txt"), &output.stderr);
    if !output.status.success() {
        return Err(format!(
            "upstream config oracle failed: {}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let text = fs::read_to_string(&result_path)
        .map_err(|error| format!("cannot read upstream oracle result: {error}"))?;
    serde_json::from_str(&text).map_err(|error| {
        format!(
            "upstream oracle output is not JSON: {error}\nresult={text}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn write_json(dir: &Path, name: &str, value: &Value) -> Result<(), String> {
    let text = serde_json::to_string_pretty(value).map_err(|error| error.to_string())?;
    fs::write(dir.join(name), text + "\n").map_err(|error| format!("cannot write {name}: {error}"))
}

fn diff() -> Result<(), String> {
    let root = match std::env::var_os("XEH_CONFIG_DIFF_OUT") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => lease_tmp()?.join(format!("config-diff-{}-{}", std::process::id(), stamp())),
    };
    if root.exists() {
        return Err(format!(
            "refusing to reuse configuration diff output: {}",
            root.display()
        ));
    }
    fs::create_dir_all(&root)
        .map_err(|error| format!("cannot create {}: {error}", root.display()))?;
    fs::copy(
        scripts_dir().join("config_runner.py"),
        root.join("config_runner.py"),
    )
    .map_err(|error| format!("cannot copy config runner: {error}"))?;

    let mut deltas = vec![
        "config-source: JSON document read through Core io/json; the oracle injects the same data as the upstream Python config module".to_string(),
    ];
    let mut failures = Vec::new();

    // Defaults differential.
    let defaults_root = fresh_dir("diff-defaults")?;
    prepare_runner(&defaults_root)?;
    let original_defaults = run_python(&defaults_root, "load", None, None)?;
    let rss_defaults = scenario(&defaults_root, &request(&defaults_root, "load", None, None))?;
    failures.extend(compare(
        &original_defaults,
        &rss_defaults,
        "defaults",
        &mut deltas,
    ));

    // Negative gate: changing one default must be rejected.
    let mut mutated = rss_defaults.clone();
    mutated["config"]["download_thread_cnt"] = json!(6);
    let negative = compare(&original_defaults, &mutated, "defaults", &mut deltas);
    let negative_rejected = negative
        .iter()
        .any(|failure| failure.contains("download_thread_cnt"));
    if !negative_rejected {
        failures.push("negative gate: a changed default was not rejected".to_string());
    }

    // File overlay differential.
    let file_root = fresh_dir("diff-file")?;
    prepare_runner(&file_root)?;
    let config = write_file(&file_root, "config.json", &fixture("user_full.json")?)?;
    let overlay = write_file(&file_root, "overlay.json", &fixture("overlay.json")?)?;
    let original_file = run_python(&file_root, "load", Some(&config), None)?;
    let rss_file = scenario(
        &file_root,
        &request(&file_root, "load", Some(&config), None),
    )?;
    failures.extend(compare(&original_file, &rss_file, "file", &mut deltas));

    // Runtime update differential.
    let original_update = run_python(&file_root, "update", Some(&config), Some(&overlay))?;
    let rss_update = scenario(
        &file_root,
        &request(&file_root, "update", Some(&config), Some(&overlay)),
    )?;
    failures.extend(compare(
        &original_update,
        &rss_update,
        "update",
        &mut deltas,
    ));
    deltas.push(
        "dir-creation: upstream update_config creates a missing download directory; this configuration layer carries the value only, because the pinned Core io surface has no directory-creation call".to_string(),
    );

    write_json(&root, "defaults-original.json", &original_defaults)?;
    write_json(&root, "defaults-rss.json", &rss_defaults)?;
    write_json(&root, "file-original.json", &original_file)?;
    write_json(&root, "file-rss.json", &rss_file)?;
    write_json(&root, "update-original.json", &original_update)?;
    write_json(&root, "update-rss.json", &rss_update)?;
    write_json(
        &root,
        "compare.json",
        &json!({
            "ok": failures.is_empty(),
            "failures": failures,
            "deltas": deltas,
            "negative_rejected": negative_rejected,
        }),
    )?;

    println!("diff=config");
    println!("output={}", root.display());
    println!(
        "negative={}",
        if negative_rejected {
            "rejected"
        } else {
            "accepted"
        }
    );
    for delta in &deltas {
        println!("delta={delta}");
    }
    if failures.is_empty() {
        println!("config-diff=pass");
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("config-diff failure: {failure}");
        }
        Err(format!(
            "configuration differential failed with {} mismatches",
            failures.len()
        ))
    }
}

pub fn run_suite() -> Result<(), String> {
    suite()
}

pub fn run_diff() -> Result<(), String> {
    diff()
}
