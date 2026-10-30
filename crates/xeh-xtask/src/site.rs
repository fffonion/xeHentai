//! Focused RSS site/rule execution gates and fixed-source differential checks.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::paths::{
    authoritative_git, lease_tmp, python_executable, scripts_dir, workspace_root, UPSTREAM_APP_REV,
};

pub(crate) struct FixedSource {
    pub(crate) root: PathBuf,
    pub(crate) files: usize,
}

struct ComparedCase {
    id: String,
    expected: Value,
    actual: Value,
}

fn stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

fn cases() -> Result<Vec<Value>, String> {
    let path = workspace_root().join("compat/fixtures/site/cases.json");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read site cases {}: {error}", path.display()))?;
    let value: Value =
        serde_json::from_str(&text).map_err(|error| format!("site cases are not JSON: {error}"))?;
    let cases = value
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "site cases are missing the cases array".to_string())?;
    validate_cases(&cases)?;
    Ok(cases)
}

fn validate_cases(cases: &[Value]) -> Result<(), String> {
    let mut ids = BTreeSet::new();
    for case in cases {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| "site case is missing a nonempty id".to_string())?;
        if !ids.insert(id.to_string()) {
            return Err(format!("site case id is duplicated: {id}"));
        }
        if !matches!(
            case.get("suite").and_then(Value::as_str),
            Some("site" | "rules")
        ) {
            return Err(format!("{id}: suite must be site or rules"));
        }
        if case.get("request").and_then(Value::as_object).is_none() {
            return Err(format!("{id}: request must be an object"));
        }
    }
    if cases.is_empty() {
        return Err("site fixtures selected zero cases".to_string());
    }
    Ok(())
}

fn new_output_dir(label: &str) -> Result<PathBuf, String> {
    let path = lease_tmp()?.join(format!("task7-{label}-{}-{}", std::process::id(), stamp()));
    fs::create_dir(&path)
        .map_err(|error| format!("cannot create {label} output {}: {error}", path.display()))?;
    Ok(path)
}

fn assert_case(case: &Value, report: &Value, failures: &mut Vec<String>) {
    let id = case
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<missing-id>");
    let assertions = match case.get("assertions").and_then(Value::as_object) {
        Some(assertions) if !assertions.is_empty() => assertions,
        _ => {
            failures.push(format!("{id}: assertions are missing"));
            return;
        }
    };
    for (pointer, expected) in assertions {
        let actual = report.pointer(pointer).cloned().unwrap_or(Value::Null);
        if actual != *expected {
            failures.push(format!("{id} {pointer}: expected {expected}, got {actual}"));
        }
    }
}

fn selected(cases: &[Value], suite: &str) -> Vec<Value> {
    cases
        .iter()
        .filter(|case| case.get("suite").and_then(Value::as_str) == Some(suite))
        .cloned()
        .collect()
}

fn run_assertion_suite(suite: &str) -> Result<(), String> {
    let all_cases = cases()?;
    let selected = selected(&all_cases, suite);
    if selected.is_empty() {
        return Err(format!("{suite} suite selected zero cases"));
    }
    let output = new_output_dir(suite)?;
    let mut failures = Vec::new();
    let mut passed = 0usize;
    for (index, case) in selected.iter().enumerate() {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<missing-id>");
        let request = case
            .get("request")
            .ok_or_else(|| format!("{id}: request is missing"))?;
        let case_root = output.join(format!("case-{index:03}"));
        fs::create_dir(&case_root)
            .map_err(|error| format!("cannot create site case root: {error}"))?;
        let request_text = serde_json::to_string(request)
            .map_err(|error| format!("{id}: cannot encode RSS request: {error}"))?;
        fs::write(case_root.join("request.json"), &request_text)
            .map_err(|error| format!("{id}: cannot save RSS request: {error}"))?;
        let raw = xeh_host::run_site_scenario(&case_root, &request_text)
            .map_err(|error| format!("{id}: RSS execution failed: {error}"))?;
        let report: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("{id}: RSS report is not JSON: {error}\n{raw}"))?;
        let failures_before = failures.len();
        assert_case(case, &report, &mut failures);
        fs::write(case_root.join("rss-result.json"), &raw)
            .map_err(|error| format!("{id}: cannot save RSS result: {error}"))?;
        if failures.len() == failures_before {
            passed += 1;
        }
    }

    println!("{suite}-suite cases={} passed={passed}", selected.len());
    println!("output={}", output.display());
    if failures.is_empty() {
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("{suite}-suite failure: {failure}");
        }
        Err(format!(
            "{suite} suite failed: {} mismatches across {} cases",
            failures.len(),
            selected.len()
        ))
    }
}

pub fn run_site_suite() -> Result<(), String> {
    run_assertion_suite("site")
}

pub fn run_rules_suite() -> Result<(), String> {
    run_assertion_suite("rules")
}

fn git_output(bare: &Path, args: &[&str]) -> Result<Vec<u8>, String> {
    let bare = bare
        .to_str()
        .ok_or_else(|| "upstream Git path is not valid UTF-8".to_string())?;
    let output = Command::new("git")
        .arg(format!("--git-dir={bare}"))
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .map_err(|error| format!("cannot execute pinned-source Git check: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "pinned-source Git check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn relative_name(path: &Path) -> Result<String, String> {
    let mut parts = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(name) => parts.push(
                name.to_str()
                    .ok_or_else(|| "upstream package contains a non-UTF-8 name".to_string())?,
            ),
            _ => return Err("upstream package contains an unsafe path".to_string()),
        }
    }
    Ok(parts.join("/"))
}

fn collect_tree(root: &Path, current: &Path, output: &mut BTreeSet<String>) -> Result<(), String> {
    for entry in fs::read_dir(current).map_err(|error| {
        format!(
            "cannot enumerate upstream package {}: {error}",
            current.display()
        )
    })? {
        let entry =
            entry.map_err(|error| format!("cannot read upstream package entry: {error}"))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|error| format!("cannot inspect upstream package entry: {error}"))?;
        if file_type.is_symlink() {
            return Err(format!(
                "upstream package contains a symlink: {}",
                path.display()
            ));
        }
        if file_type.is_dir() {
            collect_tree(root, &path, output)?;
        } else if file_type.is_file() {
            let relative = path
                .strip_prefix(root)
                .map_err(|error| format!("cannot make upstream path relative: {error}"))?;
            output.insert(relative_name(relative)?);
        } else {
            return Err(format!(
                "upstream package contains a special file: {}",
                path.display()
            ));
        }
    }
    Ok(())
}

pub(crate) fn verify_fixed_source() -> Result<FixedSource, String> {
    let root = workspace_root()
        .join("../../reviews/task5-5af6f1e/upstream")
        .canonicalize()
        .map_err(|error| format!("fixed upstream source directory is unavailable: {error}"))?;
    let package = root.join("xeHentai");
    let bare = authoritative_git("xeHentai")?;
    let tree_output = git_output(
        &bare,
        &["ls-tree", "-r", "--name-only", UPSTREAM_APP_REV, "xeHentai"],
    )?;
    let tree_text = String::from_utf8(tree_output)
        .map_err(|error| format!("pinned package file list is not UTF-8: {error}"))?;
    let mut expected = BTreeSet::new();
    for line in tree_text.lines() {
        let relative = line
            .strip_prefix("xeHentai/")
            .ok_or_else(|| format!("unexpected pinned package path: {line}"))?;
        let path = Path::new(relative);
        let _ = relative_name(path)?;
        expected.insert(format!("xeHentai/{relative}"));
    }
    let mut actual = BTreeSet::new();
    collect_tree(&root, &package, &mut actual)?;
    if expected != actual {
        let missing: Vec<_> = expected.difference(&actual).cloned().collect();
        let extra: Vec<_> = actual.difference(&expected).cloned().collect();
        return Err(format!(
            "fixed upstream package tree differs from {UPSTREAM_APP_REV}: missing={missing:?}, extra={extra:?}"
        ));
    }
    for path in &expected {
        let object = format!("{UPSTREAM_APP_REV}:{path}");
        let pinned = git_output(&bare, &["cat-file", "blob", &object])?;
        let candidate = fs::read(root.join(path)).map_err(|error| {
            format!(
                "cannot read fixed upstream source {}: {error}",
                root.join(path).display()
            )
        })?;
        if pinned != candidate {
            return Err(format!(
                "fixed upstream file differs from {UPSTREAM_APP_REV}: {path}"
            ));
        }
    }
    Ok(FixedSource {
        root,
        files: expected.len(),
    })
}

fn run_python_oracle(
    cases: &[Value],
    output_root: &Path,
    source: &FixedSource,
) -> Result<Vec<Value>, String> {
    let input_path = output_root.join("cases.json");
    let result_path = output_root.join("oracle-results.json");
    let oracle_root = output_root.join("oracle");
    fs::create_dir(&oracle_root)
        .map_err(|error| format!("cannot create isolated oracle root: {error}"))?;
    let fixture = json!({"cases": cases});
    let fixture_text = serde_json::to_string_pretty(&fixture)
        .map_err(|error| format!("cannot encode oracle fixture input: {error}"))?;
    fs::write(&input_path, fixture_text)
        .map_err(|error| format!("cannot write oracle fixture input: {error}"))?;
    let python = python_executable();
    let script = scripts_dir().join("site_runner.py");
    let output = Command::new(&python)
        .arg("-W")
        .arg("ignore::SyntaxWarning")
        .arg(&script)
        .current_dir(&oracle_root)
        .env_clear()
        .env("HOME", &oracle_root)
        .env("LANG", "C.UTF-8")
        .env("LC_ALL", "C.UTF-8")
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("PYTHONIOENCODING", "utf-8")
        .env("PYTHONPATH", &source.root)
        .env("XEH_FIXTURE_ROOT", &oracle_root)
        .env("XEH_UPSTREAM", &source.root)
        .env("XEH_CASES", &input_path)
        .env("XEH_RESULT", &result_path)
        .output()
        .map_err(|error| format!("cannot launch isolated fixed-source oracle: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        println!("oracle: {}", stdout.trim());
    }
    if !stderr.trim().is_empty() {
        eprintln!("oracle stderr: {}", stderr.trim());
    }
    if !output.status.success() {
        return Err(format!("fixed-source oracle exited with {}", output.status));
    }
    let text = fs::read_to_string(&result_path)
        .map_err(|error| format!("oracle result is unavailable: {error}"))?;
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| format!("oracle result is not JSON: {error}"))?;
    parsed
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "oracle result is missing its cases array".to_string())
}

fn rss_observable(report: &Value, case_root: &Path) -> Result<Value, String> {
    let mut observable = Map::new();
    for key in ["callbacks", "stream"] {
        let value = report
            .get(key)
            .ok_or_else(|| format!("RSS result is missing {key}"))?;
        observable.insert(key.to_string(), value.clone());
    }
    for key in ["return", "exception"] {
        observable.insert(
            key.to_string(),
            report.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    let debug_path = case_root.join("debug.htm");
    let debug = if debug_path.is_file() {
        let bytes = fs::read(&debug_path)
            .map_err(|error| format!("cannot read RSS debug effect: {error}"))?;
        Value::Array(bytes.into_iter().map(|byte| json!(byte)).collect())
    } else {
        Value::Null
    };
    observable.insert("debug".to_string(), debug);
    Ok(Value::Object(observable))
}

fn compare_json(path: &str, expected: &Value, actual: &Value, failures: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            let keys: BTreeSet<&String> = expected.keys().chain(actual.keys()).collect();
            for key in keys {
                let child = format!("{path}/{key}");
                match (expected.get(key), actual.get(key)) {
                    (Some(expected), Some(actual)) => {
                        compare_json(&child, expected, actual, failures)
                    }
                    (Some(_), None) => failures.push(format!("{child}: RSS field is missing")),
                    (None, Some(_)) => failures.push(format!("{child}: RSS field is unexpected")),
                    (None, None) => unreachable!(),
                }
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                failures.push(format!(
                    "{path}: expected {} entries, RSS returned {}",
                    expected.len(),
                    actual.len()
                ));
            }
            for index in 0..expected.len().min(actual.len()) {
                compare_json(
                    &format!("{path}/{index}"),
                    &expected[index],
                    &actual[index],
                    failures,
                );
            }
        }
        _ if expected != actual => {
            failures.push(format!("{path}: expected {expected}, got {actual}"))
        }
        _ => {}
    }
}

fn compare_observables(expected: &Value, actual: &Value) -> Vec<String> {
    let mut failures = Vec::new();
    compare_json("", expected, actual, &mut failures);
    failures
}

fn compare_case(case: &Value, expected: &Value, actual: &Value) -> Result<Vec<String>, String> {
    let Some(delta) = case.get("known_delta").and_then(Value::as_str) else {
        return Ok(compare_observables(expected, actual));
    };
    let id = case
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<missing-id>");
    let mut adjusted = expected.clone();
    match delta {
        "python-json-constant-nan" => {
            // Python's decoder accepts the non-standard `NaN` constant; the
            // pinned Core strict decoder rejects it, so the RSS filter keeps
            // its own failure callback while the oracle raises TypeError with
            // no callback. Every other observable stays compared.
            if expected.pointer("/exception") != Some(&json!("TypeError"))
                || expected.pointer("/callbacks") != Some(&json!([]))
                || actual.pointer("/exception") != Some(&json!("TypeError"))
                || actual
                    .pointer("/callbacks")
                    .and_then(Value::as_array)
                    .map(Vec::len)
                    != Some(1)
                || actual.pointer("/callbacks/0/value") != Some(&json!(1008))
                || actual.pointer("/callbacks/0/returned") != Some(&json!("strict-json-invalid"))
            {
                return Err(format!(
                    "{id}: observed behavior does not match its declared NaN delta"
                ));
            }
            adjusted["callbacks"] = actual["callbacks"].clone();
        }
        "python-unicode-surrogate-entity" => {
            // Python builds a lone-surrogate string; a Rust string cannot
            // represent it, so the RSS escape filter raises ValueError. The
            // comparison stays field-exact instead of ignoring the case.
            if expected.pointer("/return/$python_string_codepoints") != Some(&json!([55_296]))
                || expected.pointer("/exception") != Some(&Value::Null)
                || expected.pointer("/callbacks") != Some(&json!([]))
                || actual.pointer("/return") != Some(&Value::Null)
                || actual.pointer("/exception") != Some(&json!("ValueError"))
            {
                return Err(format!(
                    "{id}: observed behavior does not match its declared surrogate delta"
                ));
            }
            adjusted["return"] = actual["return"].clone();
            adjusted["exception"] = actual["exception"].clone();
        }
        "python-int-underscore-html-entity" => {
            // Python `int()` accepts underscores between digits, so `&#1_0;`
            // becomes chr(10); the RSS entity guard accepts decimal digits
            // only and raises ValueError. This exact pair is the declared
            // limit, not a general allowance.
            if expected.pointer("/return") != Some(&json!("\n"))
                || expected.pointer("/exception") != Some(&Value::Null)
                || expected.pointer("/callbacks") != Some(&json!([]))
                || actual.pointer("/return") != Some(&Value::Null)
                || actual.pointer("/exception") != Some(&json!("ValueError"))
            {
                return Err(format!(
                    "{id}: observed behavior does not match its declared underscore delta"
                ));
            }
            adjusted["return"] = actual["return"].clone();
            adjusted["exception"] = actual["exception"].clone();
        }
        other => {
            return Err(format!(
                "{id}: unknown declared compatibility delta {other}"
            ))
        }
    }
    println!("known-delta={id}:{delta}");
    Ok(compare_observables(&adjusted, actual))
}

fn reject_mutant<F>(
    compared: &[ComparedCase],
    label: &str,
    case_id: &str,
    mutate: F,
) -> Result<(), String>
where
    F: FnOnce(&mut Value) -> Result<(), String>,
{
    let case = compared
        .iter()
        .find(|case| case.id == case_id)
        .ok_or_else(|| format!("mutation gate {label} needs fixture {case_id}"))?;
    let mut mutated = case.actual.clone();
    mutate(&mut mutated)?;
    if compare_observables(&case.expected, &mutated).is_empty() {
        return Err(format!("production comparator accepted {label} mutant"));
    }
    println!("negative={label}:rejected");
    Ok(())
}

fn callback_value_mutant<'a>(
    actual: &'a mut Value,
    index: usize,
    field: &str,
) -> Result<&'a mut Value, String> {
    actual
        .get_mut("callbacks")
        .and_then(Value::as_array_mut)
        .and_then(|callbacks| callbacks.get_mut(index))
        .and_then(Value::as_object_mut)
        .and_then(|callback| callback.get_mut(field))
        .ok_or_else(|| format!("mutation cannot find callback {index} field {field}"))
}

fn run_mutation_gates(compared: &[ComparedCase]) -> Result<usize, String> {
    reject_mutant(compared, "metadata", "site.metadata.success", |actual| {
        let value = callback_value_mutant(actual, 0, "value")?;
        value
            .as_object_mut()
            .ok_or_else(|| "metadata callback value is not an object".to_string())?
            .insert("gjname".to_string(), json!("mutated"));
        Ok(())
    })?;
    reject_mutant(
        compared,
        "callback-order",
        "site.pages.multiple",
        |actual| {
            let callbacks = actual
                .get_mut("callbacks")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "page callbacks are not an array".to_string())?;
            if callbacks.len() < 2 {
                return Err("page callback-order fixture has fewer than two callbacks".to_string());
            }
            callbacks.swap(0, 1);
            Ok(())
        },
    )?;
    reject_mutant(
        compared,
        "callback-count",
        "site.pages.multiple",
        |actual| {
            let callbacks = actual
                .get_mut("callbacks")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "page callbacks are not an array".to_string())?;
            callbacks
                .pop()
                .ok_or_else(|| "page callback-count fixture is empty".to_string())?;
            Ok(())
        },
    )?;
    reject_mutant(compared, "url-source", "site.mpv.urls", |actual| {
        let value = callback_value_mutant(actual, 0, "value")?;
        let url = value
            .as_str()
            .ok_or_else(|| "MPV callback URL is not a string".to_string())?;
        *value = json!(format!("{url}?mutated-source"));
        Ok(())
    })?;
    reject_mutant(
        compared,
        "error-code",
        "rule.image.quota-size-mismatch",
        |actual| {
            let value = callback_value_mutant(actual, 0, "value")?;
            let parts = value
                .as_array_mut()
                .ok_or_else(|| "image quota callback value is not an array".to_string())?;
            let code = parts
                .first()
                .and_then(Value::as_i64)
                .ok_or_else(|| "image quota callback error code is absent".to_string())?;
            parts[0] = json!(code + 1);
            Ok(())
        },
    )?;
    reject_mutant(
        compared,
        "stream-length",
        "rule.download.bounded-stream-order",
        |actual| {
            let bytes_read = actual
                .pointer_mut("/stream/4/bytes_read")
                .ok_or_else(|| "download completion bytes_read is absent".to_string())?;
            *bytes_read = json!(bytes_read.as_i64().unwrap_or_default() + 1);
            Ok(())
        },
    )?;
    reject_mutant(
        compared,
        "stream-order",
        "rule.download.bounded-stream-order",
        |actual| {
            let stream = actual
                .get_mut("stream")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "download stream is not an array".to_string())?;
            if stream.len() < 2 {
                return Err("download stream-order fixture has fewer than two events".to_string());
            }
            stream.swap(0, 1);
            Ok(())
        },
    )?;
    reject_mutant(
        compared,
        "stream-abort",
        "rule.download.iterator-abort",
        |actual| {
            let exception = actual
                .get_mut("exception")
                .ok_or_else(|| "download abort exception is absent".to_string())?;
            *exception = Value::Null;
            Ok(())
        },
    )?;
    reject_mutant(compared, "debug-bytes", "site.pages.no-url", |actual| {
        let bytes = actual
            .get_mut("debug")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| "page debug effect is absent".to_string())?;
        let byte = bytes
            .first()
            .and_then(Value::as_u64)
            .ok_or_else(|| "page debug effect has no first byte".to_string())?;
        bytes[0] = json!(byte ^ 1);
        Ok(())
    })?;
    Ok(9)
}

pub fn run_site_diff() -> Result<(), String> {
    let all_cases = cases()?;
    let selected_cases: Vec<Value> = all_cases
        .into_iter()
        .filter(|case| {
            matches!(
                case.get("suite").and_then(Value::as_str),
                Some("site" | "rules")
            )
        })
        .collect();
    if selected_cases.is_empty() {
        return Err("site diff selected zero cases".to_string());
    }
    let source = verify_fixed_source()?;
    println!(
        "fixed-source revision={UPSTREAM_APP_REV} package-files={}",
        source.files
    );
    let output = new_output_dir("site-diff")?;
    let oracle_cases = run_python_oracle(&selected_cases, &output, &source)?;
    if oracle_cases.len() != selected_cases.len() {
        return Err(format!(
            "input/oracle case count mismatch: input={}, oracle={}",
            selected_cases.len(),
            oracle_cases.len()
        ));
    }
    let mut compared = Vec::new();
    let mut failures = Vec::new();
    let mut passed_cases = 0usize;
    let mut oracle_ids = BTreeSet::new();
    let mut rss_ids = BTreeSet::new();
    for (index, (case, oracle_case)) in selected_cases.iter().zip(&oracle_cases).enumerate() {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<missing-id>");
        let oracle_id = oracle_case
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<missing-id>");
        if oracle_id != id {
            return Err(format!(
                "oracle case order/id mismatch: input={id}, oracle={oracle_id}"
            ));
        }
        if !oracle_ids.insert(oracle_id.to_string()) || !rss_ids.insert(id.to_string()) {
            return Err(format!("input/oracle/RSS case id is duplicated: {id}"));
        }
        let case_root = output.join("rss").join(format!("case-{index:03}"));
        fs::create_dir_all(&case_root)
            .map_err(|error| format!("cannot create RSS case root: {error}"))?;
        let request = case
            .get("request")
            .ok_or_else(|| format!("{id}: request is missing"))?;
        let request_text = serde_json::to_string(request)
            .map_err(|error| format!("{id}: cannot encode RSS request: {error}"))?;
        fs::write(case_root.join("request.json"), &request_text)
            .map_err(|error| format!("{id}: cannot save RSS request: {error}"))?;
        let raw = xeh_host::run_site_scenario(&case_root, &request_text)
            .map_err(|error| format!("{id}: RSS execution failed: {error}"))?;
        fs::write(case_root.join("rss-result.json"), &raw)
            .map_err(|error| format!("{id}: cannot save RSS result: {error}"))?;
        let report: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("{id}: RSS report is not JSON: {error}\n{raw}"))?;
        let failures_before = failures.len();
        if case.get("assertions").is_some() {
            assert_case(case, &report, &mut failures);
        }
        let actual = rss_observable(&report, &case_root)
            .map_err(|error| format!("{id}: cannot collect RSS effects: {error}"))?;
        let expected = oracle_case
            .get("observable")
            .cloned()
            .ok_or_else(|| format!("{id}: oracle observable is missing"))?;
        let case_failures = compare_case(case, &expected, &actual)?;
        for failure in &case_failures {
            failures.push(format!("{id}{failure}"));
        }
        let comparison = serde_json::to_vec_pretty(&json!({
            "id": id,
            "matches": case_failures.is_empty(),
            "oracle": expected,
            "rss": actual,
        }))
        .map_err(|error| format!("{id}: cannot encode comparison: {error}"))?;
        fs::write(case_root.join("observable-comparison.json"), comparison)
            .map_err(|error| format!("{id}: cannot save comparison: {error}"))?;
        compared.push(ComparedCase {
            id: id.to_string(),
            expected,
            actual,
        });
        if failures.len() == failures_before {
            passed_cases += 1;
        }
    }
    if oracle_ids != rss_ids || oracle_ids.len() != selected_cases.len() {
        return Err(format!(
            "input/oracle/RSS ID sets differ: input={}, oracle={}, RSS={}",
            selected_cases.len(),
            oracle_ids.len(),
            rss_ids.len()
        ));
    }
    println!("output={}", output.display());
    let mutation_count = if failures.is_empty() {
        run_mutation_gates(&compared)?
    } else {
        0
    };
    println!(
        "site-diff compared={} passed={passed_cases}",
        compared.len()
    );
    if failures.is_empty() {
        println!("site-diff mutations-rejected={mutation_count}");
    } else {
        println!("site-diff mutations-rejected=0 baseline-mismatch-present");
    }
    println!("oracle_revision={UPSTREAM_APP_REV}");
    println!("env_whitelist=HOME,LANG,LC_ALL,PYTHONNOUSERSITE,PYTHONDONTWRITEBYTECODE,PYTHONIOENCODING,PYTHONPATH,XEH_FIXTURE_ROOT,XEH_UPSTREAM,XEH_CASES,XEH_RESULT");
    if failures.is_empty() {
        println!("site-diff=pass cases={}", compared.len());
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("site-diff mismatch: {failure}");
        }
        Err(format!("site diff found {} mismatches", failures.len()))
    }
}
