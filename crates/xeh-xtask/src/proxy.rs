//! Proxy pool/config/policy/Glype differential and RSS assertion suites.

use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

use crate::paths::{lease_tmp, python_executable, scripts_dir, workspace_root, UPSTREAM_APP_REV};
use crate::site::verify_fixed_source;

const AUTH_EMPTY: &str = "{\"allowed_schemes\":[],\"allowed_hosts\":[],\"allowed_ports\":[],\"allow_private_ips\":false}";

fn stamp() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or(0)
}

fn new_output_dir(label: &str) -> Result<PathBuf, String> {
    let path = lease_tmp()?.join(format!("task8-{label}-{}-{}", std::process::id(), stamp()));
    fs::create_dir(&path)
        .map_err(|error| format!("cannot create {label} output {}: {error}", path.display()))?;
    Ok(path)
}

fn cases() -> Result<Vec<Value>, String> {
    let path = workspace_root().join("compat/fixtures/proxy/cases.json");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read proxy cases {}: {error}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("proxy cases are not JSON: {error}"))?;
    let cases = value
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "proxy cases are missing the cases array".to_string())?;
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
            .ok_or_else(|| "proxy case is missing a nonempty id".to_string())?;
        if !ids.insert(id.to_string()) {
            return Err(format!("proxy case id is duplicated: {id}"));
        }
        if case.get("suite").and_then(Value::as_str) != Some("proxy") {
            return Err(format!("{id}: suite must be proxy"));
        }
        match case.get("kind").and_then(Value::as_str) {
            Some("pool" | "config" | "policy" | "glype") => {}
            _ => return Err(format!("{id}: unknown kind")),
        }
        if case.get("request").and_then(Value::as_object).is_none() {
            return Err(format!("{id}: request must be an object"));
        }
        if case.get("kind").and_then(Value::as_str) == Some("glype")
            && case.get("glype").and_then(Value::as_object).is_none()
        {
            return Err(format!("{id}: glype fixture is missing"));
        }
    }
    if cases.is_empty() {
        return Err("proxy fixtures selected zero cases".to_string());
    }
    Ok(())
}

// ---- scripted Glype/HTTP server -------------------------------------------

#[derive(Clone)]
struct ScriptResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct ScriptedServer {
    address: String,
    seen: Arc<Mutex<Vec<Value>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedServer {
    fn stop(mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(mut stream) = TcpStream::connect(self.address.trim_start_matches("http://")) {
            let _ = stream.write_all(b"GET /__stop HTTP/1.0\r\n\r\n");
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }

    fn seen(&self) -> Vec<Value> {
        self.seen.lock().expect("seen").clone()
    }
}

fn parse_responses(case: &Value) -> Result<Vec<ScriptResponse>, String> {
    let responses = case
        .get("glype")
        .and_then(|value| value.get("responses"))
        .and_then(Value::as_array)
        .ok_or_else(|| "glype responses are missing".to_string())?;
    let mut output = Vec::new();
    for response in responses {
        let status = response
            .get("status")
            .and_then(Value::as_u64)
            .unwrap_or(200) as u16;
        let headers = response
            .get("headers")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|item| {
                        let pair = item.as_array()?;
                        Some((
                            pair.first()?.as_str()?.to_string(),
                            pair.get(1)?.as_str()?.to_string(),
                        ))
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let body = if let Some(repeat) = response.get("body_repeat").and_then(Value::as_object) {
            let byte = repeat
                .get("byte")
                .and_then(Value::as_u64)
                .ok_or_else(|| "body_repeat.byte must be an integer".to_string())?
                as u8;
            let length = usize::try_from(
                repeat
                    .get("length")
                    .and_then(Value::as_u64)
                    .ok_or_else(|| "body_repeat.length must be an integer".to_string())?,
            )
            .map_err(|_| "body_repeat.length does not fit".to_string())?;
            vec![byte; length]
        } else if let Some(text) = response.get("body_text").and_then(Value::as_str) {
            text.as_bytes().to_vec()
        } else {
            let hex = response
                .get("body_hex")
                .and_then(Value::as_str)
                .ok_or_else(|| "glype response body is missing".to_string())?;
            let mut bytes = Vec::new();
            let mut index = 0usize;
            while index + 1 < hex.len() {
                let byte = u8::from_str_radix(&hex[index..index + 2], 16)
                    .map_err(|_| "glype response hex is invalid".to_string())?;
                bytes.push(byte);
                index += 2;
            }
            bytes
        };
        output.push(ScriptResponse {
            status,
            headers,
            body,
        });
    }
    Ok(output)
}

const INLINE_LIMIT: usize = 64 * 1024;

fn large_scripted_bodies(case: &Value) -> Result<Vec<Vec<u8>>, String> {
    if case.get("kind").and_then(Value::as_str) != Some("glype") {
        return Ok(Vec::new());
    }
    Ok(parse_responses(case)?
        .into_iter()
        .filter(|response| response.body.len() > INLINE_LIMIT)
        .map(|response| response.body)
        .collect())
}

/// The staged file must equal the scripted Glype body. A missing file means
/// the differential path still used the scenario entry instead of proxy_request.
fn compare_large_stage(
    case: &Value,
    root: &Path,
    failures: &mut Vec<String>,
) -> Result<(), String> {
    let id = case
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<missing id>");
    for (index, expected) in large_scripted_bodies(case)?.into_iter().enumerate() {
        let path = root.join(format!("glype-staged-{index}.bin"));
        match fs::read(&path) {
            Err(error) => failures.push(format!(
                "{id}: glype response larger than 64KiB was not staged at {}: {error}",
                path.display()
            )),
            Ok(got) if got != expected => failures.push(format!(
                "{id}: staged glype bytes differ from the scripted response {index}: staged {} bytes, scripted {} bytes",
                got.len(),
                expected.len()
            )),
            Ok(_) => {}
        }
    }
    Ok(())
}

fn read_header_block(stream: &mut TcpStream) -> Vec<u8> {
    let mut output = Vec::new();
    let mut byte = [0u8; 1];
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    loop {
        match stream.read(&mut byte) {
            Ok(0) | Err(_) => return output,
            Ok(_) => output.push(byte[0]),
        }
        if output.ends_with(b"\r\n\r\n") || output.len() > 64 * 1024 {
            return output;
        }
    }
}

fn parse_wire(headers: &[u8]) -> Value {
    let text = String::from_utf8_lossy(headers);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("").to_string();
    let path = parts.next().unwrap_or("").to_string();
    let mut map = Map::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            let key = name.trim().to_ascii_lowercase();
            if key == "connection" {
                continue;
            }
            map.insert(key, Value::String(value.trim().to_string()));
        }
    }
    json!({"method": method, "path": path, "headers": Value::Object(map), "body": ""})
}

fn start_server(responses: Vec<ScriptResponse>) -> Result<ScriptedServer, String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot bind scripted server: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("scripted server address: {error}"))?
        .port();
    let address = format!("http://127.0.0.1:{port}");
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let handle = {
        let seen = Arc::clone(&seen);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                if stop.load(Ordering::Acquire) {
                    return;
                }
                let Ok(mut stream) = stream else {
                    return;
                };
                let headers = read_header_block(&mut stream);
                if headers.is_empty() {
                    continue;
                }
                let entry = parse_wire(&headers);
                let index = {
                    let mut seen = seen.lock().expect("seen");
                    seen.push(entry);
                    seen.len() - 1
                };
                let response = responses.get(index).cloned().unwrap_or(ScriptResponse {
                    status: 500,
                    headers: Vec::new(),
                    body: Vec::new(),
                });
                let mut head = format!("HTTP/1.1 {} X\r\n", response.status);
                for (name, value) in &response.headers {
                    head.push_str(&format!("{name}: {value}\r\n"));
                }
                head.push_str(&format!(
                    "Content-Length: {}\r\nConnection: close\r\n\r\n",
                    response.body.len()
                ));
                let _ = stream.write_all(head.as_bytes());
                let _ = stream.write_all(&response.body);
            }
        })
    };
    Ok(ScriptedServer {
        address,
        seen,
        stop,
        handle: Some(handle),
    })
}

fn normalize_seen(entries: &[Value], base: &str) -> Value {
    let authority = base.trim_start_matches("http://");
    let mut output = Vec::new();
    for entry in entries {
        let mut item = entry.clone();
        if let Some(headers) = item.get_mut("headers").and_then(Value::as_object_mut) {
            for key in ["referer", "host"] {
                if let Some(Value::String(value)) = headers.get_mut(key) {
                    *value = value.replace(base, "<glype>").replace(authority, "<glype>");
                }
            }
        }
        output.push(item);
    }
    Value::Array(output)
}

fn glype_request(case: &Value, base: &str) -> Result<(Value, String), String> {
    let path = case
        .get("glype")
        .and_then(|value| value.get("path"))
        .and_then(Value::as_str)
        .ok_or_else(|| "glype path is missing".to_string())?;
    let mut request = case
        .get("request")
        .cloned()
        .ok_or_else(|| "request is missing".to_string())?;
    let address = format!("{base}{path}");
    request["address"] = Value::String(address);
    Ok((request, path.to_string()))
}

// ---- oracle ---------------------------------------------------------------

fn run_oracle(case: &Value, output: &Path) -> Result<Value, String> {
    let case_path = output.join("oracle-case.json");
    let result_path = output.join("oracle-result.json");
    let mut oracle_case = case.clone();
    if case.get("kind").and_then(Value::as_str) == Some("glype") {
        // The Python runner starts its own scripted server and substitutes the
        // address; the path and responses travel verbatim.
        oracle_case["glype"]["address"] = Value::Null;
    }
    fs::write(
        &case_path,
        serde_json::to_string_pretty(&oracle_case)
            .map_err(|error| format!("cannot encode oracle case: {error}"))?,
    )
    .map_err(|error| format!("cannot write oracle case: {error}"))?;
    let python = python_executable();
    let script = scripts_dir().join("proxy_runner.py");
    let oracle_root = output.join("oracle-root");
    fs::create_dir_all(&oracle_root)
        .map_err(|error| format!("cannot create oracle root: {error}"))?;
    let source = workspace_root().join("../../reviews/task5-5af6f1e/upstream");
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
        .env("PYTHONPATH", &source)
        .env("XEH_FIXTURE_ROOT", &oracle_root)
        .env("XEH_UPSTREAM", &source)
        .env("XEH_CASE", &case_path)
        .env("XEH_RESULT", &result_path)
        .output()
        .map_err(|error| format!("cannot launch proxy oracle: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !stdout.trim().is_empty() {
        println!("oracle: {}", stdout.trim());
    }
    if !stderr.trim().is_empty() {
        eprintln!("oracle stderr: {}", stderr.trim());
    }
    if !output.status.success() {
        return Err(format!("proxy oracle exited with {}", output.status));
    }
    let text = fs::read_to_string(&result_path)
        .map_err(|error| format!("proxy oracle result is unavailable: {error}"))?;
    serde_json::from_str(&text).map_err(|error| format!("proxy oracle result is not JSON: {error}"))
}

// ---- comparisons ----------------------------------------------------------

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

fn reject_mutant<F>(label: &str, expected: &Value, actual: &Value, mutate: F) -> Result<(), String>
where
    F: FnOnce(&mut Value) -> Result<(), String>,
{
    let mut mutated = actual.clone();
    mutate(&mut mutated)?;
    if compare_observables(expected, &mutated).is_empty() {
        return Err(format!("production comparator accepted {label} mutant"));
    }
    println!("negative={label}:rejected");
    Ok(())
}

fn proxy_state(step: &Value) -> Value {
    let Some(proxy) = step.get("proxy").cloned() else {
        return Value::Null;
    };
    if proxy.is_null() {
        return Value::Null;
    }
    json!({
        "order": proxy.get("order").cloned().unwrap_or(Value::Null),
        "counts": proxy.get("counts").cloned().unwrap_or(Value::Null),
        "disabled": proxy.get("disabled").cloned().unwrap_or(Value::Null),
        "glyph": proxy.get("glyph").cloned().unwrap_or(json!({})),
    })
}

fn pool_steps(report: &Value) -> Result<Value, String> {
    let steps = report
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| "RSS pool report is missing steps".to_string())?;
    let mut output = Vec::new();
    for step in steps {
        let op = step.get("op").and_then(Value::as_str).unwrap_or("");
        let mut address = Value::Null;
        if op == "proxy_select" && step.get("ok") == Some(&Value::Bool(true)) {
            if let Some(text) = step.get("value").and_then(Value::as_str) {
                if let Ok(value) = serde_json::from_str::<Value>(text) {
                    address = value.get("address").cloned().unwrap_or(Value::Null);
                }
            }
        }
        let mut row = json!({
            "op": op,
            "ok": step.get("ok").cloned().unwrap_or(Value::Null),
            "error": step.get("error").cloned().unwrap_or(Value::Null),
            "address": address,
            "proxy": proxy_state(step),
        });
        if op == "proxy_has" {
            let available = step
                .get("value")
                .and_then(Value::as_str)
                .and_then(|text| serde_json::from_str::<Value>(text).ok())
                .and_then(|value| value.get("value").cloned())
                .unwrap_or(Value::Null);
            row["available"] = available;
        }
        output.push(row);
    }
    Ok(Value::Array(output))
}

fn config_steps(report: &Value) -> Result<Value, String> {
    let steps = report
        .get("steps")
        .and_then(Value::as_array)
        .ok_or_else(|| "RSS config report is missing steps".to_string())?;
    let mut output = Vec::new();
    for step in steps {
        output.push(json!({
            "op": step.get("op").cloned().unwrap_or(Value::Null),
            "ok": step.get("ok").cloned().unwrap_or(Value::Null),
            "error": step.get("error").cloned().unwrap_or(Value::Null),
            "proxy": proxy_state(step),
        }));
    }
    Ok(Value::Array(output))
}

fn rss_observable(case: &Value, report: &Value, seen: Value) -> Result<Value, String> {
    match case.get("kind").and_then(Value::as_str) {
        Some("pool") => Ok(json!({
            "steps": pool_steps(report)?,
            "seen": seen,
        })),
        Some("config") => Ok(json!({
            "steps": config_steps(report)?,
            "seen": seen,
        })),
        Some("policy") => Ok(json!({
            "urls": report.get("urls").cloned().unwrap_or(Value::Null),
            "seen": seen,
        })),
        Some("glype") => {
            let mut observable = report.clone();
            if let Some(object) = observable.as_object_mut() {
                object.remove("operation");
            }
            observable["seen"] = seen;
            Ok(observable)
        }
        _ => Err("unknown proxy case kind".to_string()),
    }
}

// ---- RSS drivers ----------------------------------------------------------

fn session_headers(case: &Value) -> Result<(Vec<String>, Vec<String>), String> {
    let headers = case
        .get("glype")
        .and_then(|value| value.get("session_headers"))
        .and_then(Value::as_array)
        .ok_or_else(|| "glype session headers are missing".to_string())?;
    let mut names = Vec::new();
    let mut values = Vec::new();
    for item in headers {
        names.push(
            item.get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
        values.push(
            item.get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        );
    }
    Ok((names, values))
}

fn logical_hosts(targets: &[String]) -> Vec<String> {
    let mut hosts = Vec::new();
    for target in targets {
        let rest = target
            .strip_prefix("http://")
            .or_else(|| target.strip_prefix("https://"))
            .unwrap_or(target);
        let host = rest.split(['/', '?', ':']).next().unwrap_or("");
        if !host.is_empty()
            && !hosts
                .iter()
                .any(|item: &String| item.eq_ignore_ascii_case(host))
        {
            hosts.push(host.to_string());
        }
    }
    hosts
}

fn glype_targets(case: &Value) -> Result<Vec<String>, String> {
    let targets = case
        .get("glype")
        .and_then(|value| value.get("targets"))
        .and_then(Value::as_array)
        .ok_or_else(|| "glype targets are missing".to_string())?;
    targets
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_string)
                .ok_or_else(|| "glype target must be a string".to_string())
        })
        .collect()
}

fn invoke_owner(control: &mut xeh_host::TaskControl, request: &Value) -> Result<Value, String> {
    let text = serde_json::to_string(request)
        .map_err(|error| format!("cannot encode retained proxy request: {error}"))?;
    let raw = control
        .invoke(&text)
        .map_err(|error| format!("retained proxy invoke failed: {error}"))?;
    serde_json::from_str(&raw)
        .map_err(|error| format!("retained proxy report is not JSON: {error}\n{raw}"))
}

fn retained_value(report: &Value) -> Result<Value, String> {
    match report.get("value").and_then(Value::as_str) {
        Some(text) if !text.is_empty() => serde_json::from_str(text)
            .map_err(|error| format!("retained proxy value is not JSON: {error}")),
        _ => Ok(Value::Null),
    }
}

fn bytes_json(bytes: &[u8]) -> Value {
    Value::Array(bytes.iter().copied().map(|byte| json!(byte)).collect())
}

/// One TaskControl drives every Glype target. Large bodies are the files
/// `proxy_request` staged, compared later with the scripted response.
fn run_glype_retained(
    case: &Value,
    output: &Path,
) -> Result<(Value, Value, Option<PathBuf>), String> {
    let responses = parse_responses(case)?;
    let server = start_server(responses)?;
    let (request, _path) = glype_request(case, &server.address)?;
    let address = request
        .get("address")
        .and_then(Value::as_str)
        .ok_or_else(|| "glype address is missing".to_string())?
        .to_string();
    let port: u16 = server
        .address
        .rsplit(':')
        .next()
        .and_then(|value| value.parse().ok())
        .ok_or_else(|| "scripted server address has no port".to_string())?;
    let (names, values) = session_headers(case)?;
    let targets = glype_targets(case)?;
    let logical_hosts = logical_hosts(&targets);
    let mut control = xeh_host::TaskControl::open_with_proxy(
        &xeh_host::ControlOptions::fixture(&[], 0),
        None,
        xeh_host::ProxyServices::loopback(output, port, &logical_hosts),
    )
    .map_err(|error| format!("cannot open retained proxy owner: {error}"))?;
    let added = invoke_owner(
        &mut control,
        &json!({"op": "proxy_add", "address": address}),
    )?;
    if added.get("ok") != Some(&Value::Bool(true)) {
        server.stop();
        return Err(format!("proxy_add failed: {added}"));
    }
    let mut flows = Vec::new();
    let mut glyph_s = String::new();
    let mut large_index = 0usize;
    let mut stopped = false;
    for (index, target) in targets.iter().enumerate() {
        if stopped {
            break;
        }
        let stage = format!("flow-{index}.bin");
        let report = invoke_owner(
            &mut control,
            &json!({
                "op": "proxy_request",
                "url": target,
                "method": "GET",
                "stage": stage,
                "header_names": names,
                "header_values": values,
                "proxy_image": true,
                "explicit_allow_redirects": false,
                "now": 10.0 + (index as f64) * 10.0
            }),
        )?;
        let value = retained_value(&report)?;
        if let Some(text) = value.get("glyph_s").and_then(Value::as_str) {
            if !text.is_empty() {
                glyph_s = text.to_string();
            }
        }
        if value.get("ok") == Some(&Value::Bool(true)) {
            let content_type = value
                .get("content_type")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let staged_path = output.join(&stage);
            let staged = fs::read(&staged_path).map_err(|error| {
                format!(
                    "proxy_request did not stage {target} at {}: {error}",
                    staged_path.display()
                )
            })?;
            if staged.len() > INLINE_LIMIT {
                let dest = output.join(format!("glype-staged-{large_index}.bin"));
                fs::write(&dest, &staged).map_err(|error| {
                    format!(
                        "cannot retain staged glype body {}: {error}",
                        dest.display()
                    )
                })?;
                large_index += 1;
            }
            let content_bytes = if content_type.starts_with("text") {
                json!([])
            } else {
                bytes_json(&staged)
            };
            flows.push(json!({
                "ok": true,
                "exception": "",
                "detail": "",
                "url": target,
                "status": value.get("status").cloned().unwrap_or(Value::Null),
                "content_type": content_type,
                "content": value.get("content").cloned().unwrap_or(json!("")),
                "content_bytes": content_bytes,
                "set_cookie": value.get("set_cookie").cloned().unwrap_or(json!("")),
            }));
        } else {
            flows.push(json!({
                "ok": false,
                "exception": value.get("error").and_then(Value::as_str).unwrap_or(""),
                "detail": value.get("detail").and_then(Value::as_str).unwrap_or(""),
            }));
            stopped = true;
        }
    }
    let seen = normalize_seen(&server.seen(), &server.address);
    server.stop();
    let failed = flows
        .iter()
        .rev()
        .find(|flow| flow.get("ok") != Some(&Value::Bool(true)));
    let report = if let Some(flow) = failed {
        json!({
            "ok": false,
            "exception": flow.get("exception").cloned().unwrap_or(json!("")),
            "detail": flow.get("detail").cloned().unwrap_or(json!("")),
            "flows": flows,
            "glyph_s": glyph_s,
        })
    } else {
        json!({
            "ok": true,
            "exception": "",
            "detail": "",
            "flows": flows,
            "glyph_s": glyph_s,
        })
    };
    let mut observed = report;
    observed["seen"] = seen.clone();
    Ok((observed, seen, None))
}

fn run_rss(case: &Value, output: &Path) -> Result<(Value, Value, Option<PathBuf>), String> {
    let kind = case.get("kind").and_then(Value::as_str).unwrap_or("");
    if kind == "glype" {
        run_glype_retained(case, output)
    } else {
        let request = case
            .get("request")
            .cloned()
            .ok_or_else(|| "request is missing".to_string())?;
        let request_text = serde_json::to_string(&request)
            .map_err(|error| format!("cannot encode RSS request: {error}"))?;
        let raw = xeh_host::run_proxy_scenario(output, &request_text, AUTH_EMPTY)
            .map_err(|error| format!("RSS proxy execution failed: {error}"))?;
        let report: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("RSS proxy report is not JSON: {error}\n{raw}"))?;
        Ok((report, Value::Null, None))
    }
}

fn assert_case(case: &Value, observed: &Value, failures: &mut Vec<String>) {
    let id = case
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("<missing-id>");
    let Some(assertions) = case.get("assertions").and_then(Value::as_object) else {
        return;
    };
    for (pointer, expected) in assertions {
        let actual = observed.pointer(pointer).cloned().unwrap_or(Value::Null);
        if actual != *expected {
            failures.push(format!("{id} {pointer}: expected {expected}, got {actual}"));
        }
    }
}

// ---- rss-test --suite proxy ------------------------------------------------

pub fn run_proxy_suite() -> Result<(), String> {
    let all_cases = cases()?;
    let output = new_output_dir("proxy-suite")?;
    let mut failures = Vec::new();
    let mut passed = 0usize;
    for (index, case) in all_cases.iter().enumerate() {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        let case_root = output.join(format!("case-{index:03}"));
        fs::create_dir(&case_root).map_err(|error| format!("cannot create case root: {error}"))?;
        let failures_before = failures.len();
        let (observed, _seen, _) =
            run_rss(case, &case_root).map_err(|error| format!("{id}: {error}"))?;
        assert_case(case, &observed, &mut failures);
        compare_large_stage(case, &case_root, &mut failures)?;
        fs::write(
            case_root.join("rss-observed.json"),
            serde_json::to_vec_pretty(&observed).map_err(|error| error.to_string())?,
        )
        .map_err(|error| format!("cannot save RSS observation: {error}"))?;
        if failures.len() == failures_before {
            passed += 1;
        }
    }
    println!("proxy-suite cases={} passed={passed}", all_cases.len());
    println!("output={}", output.display());
    if failures.is_empty() {
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("proxy-suite failure: {failure}");
        }
        Err(format!(
            "proxy suite failed: {} mismatches across {} cases",
            failures.len(),
            all_cases.len()
        ))
    }
}

// ---- diff --suite proxy ----------------------------------------------------

fn mutation_gates(
    cases: &[Value],
    expected: &HashMap<String, Value>,
    actual: &HashMap<String, Value>,
) -> Result<usize, String> {
    let mut count = 0usize;
    let pool_id = "proxy.pool.order-and-readd";
    if let (Some(expected), Some(actual)) = (expected.get(pool_id), actual.get(pool_id)) {
        reject_mutant("pool-order", expected, actual, |value| {
            let order = value
                .pointer_mut("/steps/1/proxy/order")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "pool order is absent".to_string())?;
            if order.len() < 2 {
                return Err("pool order fixture has fewer than two entries".to_string());
            }
            order.swap(0, 1);
            Ok(())
        })?;
        count += 1;
        reject_mutant("pool-counter", expected, actual, |value| {
            let counts = value
                .pointer_mut("/steps/2/proxy/counts")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| "pool counts are absent".to_string())?;
            let (_, row) = counts
                .iter_mut()
                .next()
                .ok_or_else(|| "pool counts are empty".to_string())?;
            let row = row
                .as_array_mut()
                .ok_or_else(|| "pool count row is not an array".to_string())?;
            row[1] = json!(row[1].as_i64().unwrap_or_default() + 1);
            Ok(())
        })?;
        count += 1;
        reject_mutant("pool-disabled-expiry", expected, actual, |value| {
            let disabled = value
                .pointer_mut("/steps/3/proxy/disabled")
                .and_then(Value::as_object_mut)
                .ok_or_else(|| "pool disable is absent".to_string())?;
            let (_, expiry) = disabled
                .iter_mut()
                .next()
                .ok_or_else(|| "pool disable is empty".to_string())?;
            *expiry = json!(expiry.as_f64().unwrap_or_default() + 1.0);
            Ok(())
        })?;
        count += 1;
        reject_mutant("pool-selected-address", expected, actual, |value| {
            let address = value
                .pointer_mut("/steps/4/address")
                .ok_or_else(|| "selected address is absent".to_string())?;
            *address = json!("http://mutated.invalid:9");
            Ok(())
        })?;
        count += 1;
        reject_mutant("pool-exception-class", expected, actual, |value| {
            let error = value
                .pointer_mut("/steps/5/error")
                .ok_or_else(|| "pool exception is absent".to_string())?;
            *error = json!("KeyError");
            Ok(())
        })?;
        count += 1;
        reject_mutant("pool-available", expected, actual, |value| {
            let available = value
                .pointer_mut("/steps/5/available")
                .ok_or_else(|| "pool available is absent".to_string())?;
            *available = json!(true);
            Ok(())
        })?;
        count += 1;
        reject_mutant("pool-glyph", expected, actual, |value| {
            let glyph = value
                .pointer_mut("/steps/1/proxy/glyph")
                .ok_or_else(|| "pool glyph is absent".to_string())?;
            *glyph = json!({"http://a.invalid:1": "stale"});
            Ok(())
        })?;
        count += 1;
    }
    let config_id = "proxy.config.update-pool";
    if let (Some(expected), Some(actual)) = (expected.get(config_id), actual.get(config_id)) {
        reject_mutant("config-added-entry", expected, actual, |value| {
            let order = value
                .pointer_mut("/steps/0/proxy/order")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "config pool order is absent".to_string())?;
            order.pop();
            Ok(())
        })?;
        count += 1;
        reject_mutant("config-retained-entry", expected, actual, |value| {
            let order = value
                .pointer_mut("/steps/1/proxy/order")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "config pool order is absent".to_string())?;
            if order.len() < 2 {
                return Err("config retained fixture has fewer than two entries".to_string());
            }
            order.swap(0, 1);
            Ok(())
        })?;
        count += 1;
    }
    let policy_id = "proxy.policy.matrix";
    if let (Some(expected), Some(actual)) = (expected.get(policy_id), actual.get(policy_id)) {
        reject_mutant("policy-webpage", expected, actual, |value| {
            let webpage = value
                .pointer_mut("/urls/0/webpage")
                .ok_or_else(|| "policy webpage is absent".to_string())?;
            *webpage = json!(!webpage.as_bool().unwrap_or_default());
            Ok(())
        })?;
        count += 1;
    }
    let glype_id = "proxy.glype.flow";
    if let (Some(expected), Some(actual)) = (expected.get(glype_id), actual.get(glype_id)) {
        reject_mutant("glype-seen-cookie", expected, actual, |value| {
            let cookie = value
                .pointer_mut("/seen/0/headers/cookie")
                .ok_or_else(|| "glype seen cookie is absent".to_string())?;
            *cookie = json!("mutated=1");
            Ok(())
        })?;
        count += 1;
        reject_mutant("glype-seen-path", expected, actual, |value| {
            let path = value
                .pointer_mut("/seen/0/path")
                .ok_or_else(|| "glype seen path is absent".to_string())?;
            let text = path.as_str().unwrap_or("").to_string();
            *path = json!(format!("{text}&mutated=1"));
            Ok(())
        })?;
        count += 1;
        reject_mutant("glype-content", expected, actual, |value| {
            let content = value
                .pointer_mut("/flows/0/content")
                .ok_or_else(|| "glype text content is absent".to_string())?;
            let text = content.as_str().unwrap_or("").to_string();
            *content = json!(format!("{text} mutated"));
            Ok(())
        })?;
        count += 1;
        reject_mutant("glype-set-cookie", expected, actual, |value| {
            let cookie = value
                .pointer_mut("/flows/0/set_cookie")
                .ok_or_else(|| "glype set-cookie is absent".to_string())?;
            *cookie = json!("mutated=1");
            Ok(())
        })?;
        count += 1;
    }
    let status_id = "proxy.glype.raw-403";
    if let (Some(expected), Some(actual)) = (expected.get(status_id), actual.get(status_id)) {
        reject_mutant("glype-raw-status", expected, actual, |value| {
            let status = value
                .pointer_mut("/flows/0/status")
                .ok_or_else(|| "glype raw status is absent".to_string())?;
            *status = json!(status.as_i64().unwrap_or_default() + 1);
            Ok(())
        })?;
        count += 1;
    }
    let readd_id = "proxy.pool.readd-resets-scores";
    if let (Some(expected), Some(actual)) = (expected.get(readd_id), actual.get(readd_id)) {
        reject_mutant("pool-readd-count", expected, actual, |value| {
            // RFC 6901 encodes `/` in the source address key as `~1`.
            // The row remains `counts["http://a.invalid:1"]`, not another field.
            let row = value
                .pointer_mut("/steps/4/proxy/counts/http:~1~1a.invalid:1")
                .and_then(Value::as_array_mut)
                .ok_or_else(|| "re-add counts are absent".to_string())?;
            row[0] = json!(row[0].as_i64().unwrap_or_default() + 1);
            Ok(())
        })?;
        count += 1;
    }
    let _ = cases;
    Ok(count)
}

pub fn run_proxy_diff() -> Result<(), String> {
    let all_cases = cases()?;
    let source = verify_fixed_source()?;
    println!(
        "fixed-source revision={UPSTREAM_APP_REV} package-files={}",
        source.files
    );
    let output = new_output_dir("proxy-diff")?;
    let mut expected = HashMap::new();
    let mut actual = HashMap::new();
    let mut failures = Vec::new();
    let mut passed = 0usize;
    let mut oracle_ids = BTreeSet::new();
    let mut rss_ids = BTreeSet::new();
    for (index, case) in all_cases.iter().enumerate() {
        let id = case
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("<missing>");
        if !oracle_ids.insert(id.to_string()) || !rss_ids.insert(id.to_string()) {
            return Err(format!("proxy case id is duplicated: {id}"));
        }
        let case_root = output.join(format!("case-{index:03}"));
        fs::create_dir(&case_root).map_err(|error| format!("cannot create case root: {error}"))?;
        let oracle_output = case_root.join("oracle");
        fs::create_dir(&oracle_output)
            .map_err(|error| format!("cannot create oracle case root: {error}"))?;
        let oracle = run_oracle(case, &oracle_output).map_err(|error| format!("{id}: {error}"))?;
        let expected_observable = oracle
            .get("observable")
            .cloned()
            .ok_or_else(|| format!("{id}: oracle observable is missing"))?;
        let (report, seen, _) =
            run_rss(case, &case_root).map_err(|error| format!("{id}: {error}"))?;
        let actual_observable =
            rss_observable(case, &report, seen).map_err(|error| format!("{id}: {error}"))?;
        fs::write(
            case_root.join("observable-comparison.json"),
            serde_json::to_vec_pretty(&json!({
                "id": id,
                "matches": compare_observables(&expected_observable, &actual_observable).is_empty(),
                "oracle": expected_observable,
                "rss": actual_observable,
            }))
            .map_err(|error| format!("{id}: cannot encode comparison: {error}"))?,
        )
        .map_err(|error| format!("{id}: cannot save comparison: {error}"))?;
        let case_failures = compare_observables(&expected_observable, &actual_observable);
        for failure in &case_failures {
            failures.push(format!("{id}{failure}"));
        }
        let stage_before = failures.len();
        compare_large_stage(case, &case_root, &mut failures)?;
        if case_failures.is_empty() && failures.len() == stage_before {
            passed += 1;
        }
        expected.insert(id.to_string(), expected_observable);
        actual.insert(id.to_string(), actual_observable);
    }
    if oracle_ids != rss_ids || oracle_ids.len() != all_cases.len() {
        return Err(format!(
            "input/oracle/RSS ID sets differ: input={}, oracle={}, rss={}",
            all_cases.len(),
            oracle_ids.len(),
            rss_ids.len()
        ));
    }
    let mutation_count = if failures.is_empty() {
        mutation_gates(&all_cases, &expected, &actual)?
    } else {
        0
    };
    println!("output={}", output.display());
    println!("proxy-diff compared={} passed={passed}", all_cases.len());
    if failures.is_empty() {
        println!("proxy-diff mutations-rejected={mutation_count}");
        println!("proxy-diff=pass cases={}", all_cases.len());
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("proxy-diff mismatch: {failure}");
        }
        Err(format!("proxy diff found {} mismatches", failures.len()))
    }
}
