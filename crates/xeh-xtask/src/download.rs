//! Download scheduling suite. Decisions stay in RSS; this crate only runs them.

use std::collections::BTreeSet;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::JoinHandle;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::paths::{lease_tmp, python_executable, scripts_dir, workspace_root, UPSTREAM_APP_REV};

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
    let path = workspace_root().join("compat/fixtures/download/cases.json");
    let text = fs::read_to_string(&path)
        .map_err(|error| format!("cannot read download cases {}: {error}", path.display()))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("download cases are not JSON: {error}"))?;
    let cases = value
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "download cases are missing the cases array".to_string())?;
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
            .ok_or_else(|| "download case is missing a nonempty id".to_string())?;
        if !ids.insert(id.to_string()) {
            return Err(format!("download case id is duplicated: {id}"));
        }
        if case.get("suite").and_then(Value::as_str) != Some("download") {
            return Err(format!("{id}: suite must be download"));
        }
        if case.get("request").and_then(Value::as_object).is_none() {
            return Err(format!("{id}: request must be an object"));
        }
    }
    if cases.is_empty() {
        return Err("download fixtures selected zero cases".to_string());
    }
    Ok(())
}

fn new_output_dir(label: &str) -> Result<PathBuf, String> {
    let path = lease_tmp()?.join(format!(
        "task9-{label}-{}-{}",
        std::process::id(),
        stamp()
    ));
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

struct HopFixture {
    url: String,
    proxy_address: String,
    _servers: Vec<JoinHandle<()>>,
}

fn spawn_ok_server() -> Result<(u16, JoinHandle<()>), String> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("cannot bind download hop fixture: {error}"))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("download hop fixture has no address: {error}"))?
        .port();
    let server = std::thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 4096];
            let mut got = Vec::new();
            while !got.windows(4).any(|window| window == b"\r\n\r\n") && got.len() < 8192 {
                match stream.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => got.extend_from_slice(&buf[..n]),
                }
            }
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
            );
        }
    });
    Ok((port, server))
}

fn hop_fixture(request: &Value) -> Result<Option<HopFixture>, String> {
    if request.get("operation").and_then(Value::as_str) != Some("hop") {
        return Ok(None);
    }
    let (target_port, target) = spawn_ok_server()?;
    let kind = request.get("kind").and_then(Value::as_str).unwrap_or("direct");
    if kind == "proxied" {
        let (proxy_port, proxy) = spawn_ok_server()?;
        Ok(Some(HopFixture {
            url: format!("http://target.invalid:{target_port}/image.bin"),
            proxy_address: format!("http://127.0.0.1:{proxy_port}"),
            _servers: vec![target, proxy],
        }))
    } else {
        Ok(Some(HopFixture {
            url: format!("http://127.0.0.1:{target_port}/image.bin"),
            proxy_address: String::new(),
            _servers: vec![target],
        }))
    }
}

fn prepare_request(request: &Value, hop: Option<&HopFixture>) -> Value {
    let mut prepared = request.clone();
    if let Some(hop) = hop {
        if let Some(object) = prepared.as_object_mut() {
            object.insert("url".to_string(), json!(hop.url));
            if !hop.proxy_address.is_empty() {
                object.insert("proxy_address".to_string(), json!(hop.proxy_address));
            }
        }
    }
    prepared
}

fn host_port(item: &str) -> Option<(String, Option<u16>)> {
    let without = item.strip_prefix("http://")?;
    let authority = without.split('/').next().unwrap_or("");
    if let Some((host, port)) = authority.rsplit_once(':') {
        Some((host.to_string(), port.parse().ok()))
    } else if authority.is_empty() {
        None
    } else {
        Some((authority.to_string(), None))
    }
}

fn authorization_for(request: &Value) -> String {
    let url = request.get("url").and_then(Value::as_str).unwrap_or("");
    let proxy = request
        .get("proxy_address")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut hosts = vec!["127.0.0.1".to_string()];
    let mut ports = vec![80u16, 9];
    for item in [url, proxy] {
        if let Some((host, port)) = host_port(item) {
            if !hosts.iter().any(|existing| existing == &host) {
                hosts.push(host);
            }
            if let Some(port) = port {
                if !ports.contains(&port) {
                    ports.push(port);
                }
            }
        }
    }
    json!({
        "allowed_schemes": ["http"],
        "allowed_hosts": hosts,
        "allowed_ports": ports,
        "allow_private_ips": true,
        "grants": if proxy.is_empty() {
            Vec::<Value>::new()
        } else {
            vec![json!({"address": proxy, "remote_dns": true})]
        },
    })
    .to_string()
}

fn run_one(root: &Path, request: &Value) -> Result<String, String> {
    let hop = hop_fixture(request)?;
    let prepared = prepare_request(request, hop.as_ref());
    let request_text = serde_json::to_string(&prepared)
        .map_err(|error| format!("cannot encode download request: {error}"))?;
    let authorization = authorization_for(&prepared);
    xeh_host::run_download_scenario(root, &request_text, &authorization)
        .map_err(|error| error.to_string())
}

pub fn run_download_suite() -> Result<(), String> {
    let selected = cases()?;
    let output = new_output_dir("download")?;
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
            .map_err(|error| format!("cannot create download case root: {error}"))?;
        let raw = run_one(&case_root, request)
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
    println!("download-suite cases={} passed={passed}", selected.len());
    println!("output={}", output.display());
    if failures.is_empty() {
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("download-suite failure: {failure}");
        }
        Err(format!(
            "download suite failed: {} mismatches across {} cases",
            failures.len(),
            selected.len()
        ))
    }
}

fn run_python_oracle(
    cases: &[Value],
    output_root: &Path,
    source_root: &Path,
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
    let script = scripts_dir().join("download_runner.py");
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
        .env("PYTHONPATH", source_root)
        .env("XEH_FIXTURE_ROOT", &oracle_root)
        .env("XEH_UPSTREAM", source_root)
        .env("XEH_CASES", &input_path)
        .env("XEH_RESULT", &result_path)
        .output()
        .map_err(|error| format!("cannot launch isolated fixed-source oracle: {error}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if !output.status.success() {
        return Err(format!(
            "download oracle failed: {}\n{stdout}{stderr}",
            output.status
        ));
    }
    let text = fs::read_to_string(&result_path)
        .map_err(|error| format!("cannot read download oracle results: {error}"))?;
    let value: Value = serde_json::from_str(&text)
        .map_err(|error| format!("download oracle results are not JSON: {error}"))?;
    value
        .get("cases")
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| "download oracle results are missing cases".to_string())
}

fn observable_of(value: &Value) -> Result<Value, String> {
    value
        .get("observable")
        .cloned()
        .ok_or_else(|| "download report is missing observable".to_string())
}

fn reject_mutant(
    compared: &[ComparedCase],
    label: &str,
    id: &str,
    mutate: impl Fn(&mut Value) -> Result<(), String>,
) -> Result<(), String> {
    let case = compared
        .iter()
        .find(|case| case.id == id)
        .ok_or_else(|| format!("mutation gate {label} cannot find {id}"))?;
    let mut actual = case.actual.clone();
    mutate(&mut actual)?;
    if actual == case.expected {
        return Err(format!(
            "download diff mutation gate {label} still matched the oracle"
        ));
    }
    Ok(())
}

pub fn run_download_diff() -> Result<(), String> {
    let selected = cases()?;
    let source = crate::site::verify_fixed_source()?;
    println!(
        "fixed-source revision={UPSTREAM_APP_REV} package-files={}",
        source.files
    );
    let output = new_output_dir("download-diff")?;
    let oracle_cases = run_python_oracle(&selected, &output, &source.root)?;
    if oracle_cases.len() != selected.len() {
        return Err(format!(
            "input/oracle case count mismatch: input={}, oracle={}",
            selected.len(),
            oracle_cases.len()
        ));
    }
    let mut compared = Vec::new();
    let mut failures = Vec::new();
    let mut passed = 0usize;
    for (index, (case, oracle_case)) in selected.iter().zip(&oracle_cases).enumerate() {
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
        let request = case
            .get("request")
            .ok_or_else(|| format!("{id}: request is missing"))?;
        let case_root = output.join(format!("case-{index:03}"));
        fs::create_dir(&case_root)
            .map_err(|error| format!("cannot create download diff root: {error}"))?;
        let raw = run_one(&case_root, request)
            .map_err(|error| format!("{id}: RSS execution failed: {error}"))?;
        let report: Value = serde_json::from_str(&raw)
            .map_err(|error| format!("{id}: RSS report is not JSON: {error}\n{raw}"))?;
        let expected = observable_of(oracle_case)?;
        let actual = observable_of(&report)?;
        if expected == actual {
            passed += 1;
        } else {
            failures.push(format!("{id}: observable mismatch\nexpected {expected}\nactual {actual}"));
        }
        compared.push(ComparedCase {
            id: id.to_string(),
            expected,
            actual,
        });
    }
    let mutations = run_mutation_gates(&compared)?;
    println!(
        "download-diff cases={} passed={passed} mutations={mutations}",
        selected.len()
    );
    println!("output={}", output.display());
    if failures.is_empty() {
        Ok(())
    } else {
        for failure in &failures {
            eprintln!("download-diff failure: {failure}");
        }
        Err(format!(
            "download diff failed: {} mismatches across {} cases",
            failures.len(),
            selected.len()
        ))
    }
}

fn run_mutation_gates(compared: &[ComparedCase]) -> Result<usize, String> {
    reject_mutant(compared, "fail-code", "download.retry.connection-error", |actual| {
        let code = actual
            .pointer_mut("/fail/0")
            .ok_or_else(|| "connection-error fail code is absent".to_string())?;
        *code = json!(code.as_i64().unwrap_or_default() + 1);
        Ok(())
    })?;
    reject_mutant(compared, "requeue", "download.retry.key-expired", |actual| {
        let url = actual
            .get_mut("requeue_url")
            .ok_or_else(|| "key-expired requeue url is absent".to_string())?;
        *url = json!("https://mutated.example/reload");
        Ok(())
    })?;
    reject_mutant(compared, "vote", "download.monitor.quota-vote", |actual| {
        let state = actual
            .get_mut("task_state")
            .ok_or_else(|| "quota vote state is absent".to_string())?;
        *state = json!(10);
        Ok(())
    })?;
    reject_mutant(compared, "finished", "download.duplicates.finish-first", |actual| {
        let finished = actual
            .get_mut("finished")
            .and_then(Value::as_array_mut)
            .ok_or_else(|| "finish-first finished list is absent".to_string())?;
        finished.push(json!(9));
        Ok(())
    })?;
    reject_mutant(compared, "hop-path", "download.hop.direct", |actual| {
        let path = actual
            .get_mut("path")
            .ok_or_else(|| "direct hop path is absent".to_string())?;
        *path = json!("proxied");
        Ok(())
    })?;
    Ok(5)
}
