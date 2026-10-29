//! JSON configuration loading, validation, overlays, views, and task
//! snapshots executed through the real RustScript entry.
//!
//! Each test owns a fresh fixture root. The suite creates every input it needs,
//! so it never depends on another test or on files left by a previous run.

#[allow(dead_code)]
#[path = "support.rs"]
mod support;

use std::path::{Path, PathBuf};

use serde_json::{json, Value};
use support::TestRoot;

use xeh_host::run_config_scenario;

const CONFIG_KEYS: usize = 25;
const PUBLIC_KEYS: usize = 21;
const TASK_KEYS: usize = 14;

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn fixture_text(name: &str) -> String {
    let path = workspace().join("compat/fixtures/config").join(name);
    std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("read fixture {}: {error}", path.display()))
}

fn write_file(root: &Path, name: &str, text: &str) -> PathBuf {
    let path = root.join(name);
    std::fs::write(&path, text).unwrap_or_else(|error| panic!("write {}: {error}", path.display()));
    path
}

fn scenario(root: &Path, request: Value) -> Value {
    let text = run_config_scenario(root, &request.to_string())
        .unwrap_or_else(|error| panic!("scenario failed for {request}: {error}"));
    serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("scenario report is not JSON: {error}\n{text}"))
}

fn load_request(root: &Path, path: Value) -> Value {
    json!({
        "op": "load",
        "base": root.to_str().expect("utf-8 root"),
        "path": path,
    })
}

fn load_default(root: &Path) -> Value {
    scenario(root, load_request(root, Value::Null))
}

fn load_explicit(root: &Path, path: &Path) -> Value {
    scenario(
        root,
        load_request(
            root,
            Value::String(path.to_str().expect("utf-8 path").into()),
        ),
    )
}

fn golden_defaults() -> Value {
    let text = std::fs::read_to_string(workspace().join("compat/golden/config.json"))
        .expect("accepted config golden");
    let golden: Value = serde_json::from_str(&text).expect("golden is JSON");
    assert_eq!(
        golden.get("count").and_then(Value::as_u64),
        Some(CONFIG_KEYS as u64)
    );
    golden.get("keys").cloned().expect("golden keys")
}

fn object_keys(value: &Value) -> Vec<String> {
    let mut keys: Vec<String> = value.as_object().expect("object").keys().cloned().collect();
    keys.sort();
    keys
}

fn assert_rejected(report: &Value, expected_error: &str) {
    assert_eq!(report.get("ok"), Some(&Value::Bool(false)), "{report}");
    assert_eq!(
        report.get("error").and_then(Value::as_str),
        Some(expected_error),
        "{report}"
    );
    assert!(
        report.get("config").is_some_and(Value::is_null),
        "a rejected document must not publish a config: {report}"
    );
    assert!(report.get("public").is_some_and(Value::is_null), "{report}");
    assert!(report.get("task").is_some_and(Value::is_null), "{report}");
}

#[test]
fn defaults_match_the_accepted_golden() {
    let root = TestRoot::create();
    let report = load_default(root.path());

    assert_eq!(report.get("ok"), Some(&Value::Bool(true)), "{report}");
    assert_eq!(
        report.get("source").and_then(Value::as_str),
        Some("defaults"),
        "{report}"
    );
    assert_eq!(report.get("config"), Some(&golden_defaults()));
    assert_eq!(
        object_keys(report.get("config").unwrap()).len(),
        CONFIG_KEYS
    );
    assert_eq!(
        object_keys(report.get("public").unwrap()).len(),
        PUBLIC_KEYS
    );
    assert_eq!(object_keys(report.get("task").unwrap()).len(), TASK_KEYS);

    assert_eq!(report["task"]["dir"], json!("."));
    assert_eq!(report["task"]["ignored_errors"], json!([]));
    assert_eq!(report["task"]["download_range"], Value::Null);
    assert_eq!(report["task"]["download_timeout"], json!(10));
    assert_eq!(report["task"]["download_thread_cnt"], json!(5));
}

#[test]
fn public_view_filters_rpc_keys_and_urls() {
    let root = TestRoot::create();
    let report = load_default(root.path());
    let public = report.get("public").expect("public view");
    let keys = object_keys(public);
    assert!(
        keys.iter().all(|key| !key.starts_with("rpc_")),
        "rpc_* leaked into the public view: {keys:?}"
    );
    assert!(!keys.contains(&"urls".to_string()));
    assert_eq!(
        keys,
        vec![
            "auto_update",
            "daemon",
            "delete_task_files",
            "dir",
            "download_ori",
            "download_range",
            "download_thread_cnt",
            "download_timeout",
            "ignored_errors",
            "jpn_title",
            "log_path",
            "log_verbose",
            "low_speed_threshold",
            "make_archive",
            "proxy",
            "proxy_image",
            "proxy_image_only",
            "rename_ori",
            "save_tasks",
            "scan_thread_cnt",
            "update_beta_channel",
        ]
    );
}

#[test]
fn explicit_file_overrides_defaults_without_truthiness() {
    let root = TestRoot::create();
    let path = write_file(root.path(), "config.json", &fixture_text("user_full.json"));
    let report = load_explicit(root.path(), &path);

    assert_eq!(report.get("ok"), Some(&Value::Bool(true)), "{report}");
    assert_eq!(
        report.get("source").and_then(Value::as_str),
        Some("file"),
        "{report}"
    );
    let config = report.get("config").expect("config");
    assert_eq!(config["daemon"], json!(true));
    assert_eq!(config["jpn_title"], json!(false));
    assert_eq!(config["rename_ori"], json!(true));
    assert_eq!(config["proxy"], json!(["socks5h://127.0.0.1:1080"]));
    assert_eq!(config["proxy_image"], json!(false));
    assert_eq!(config["proxy_image_only"], json!(true));
    assert_eq!(config["rpc_port"], json!(0));
    assert!(config["rpc_secret"].is_null());
    assert_eq!(config["download_range"], json!([[2, 4], [9, 9]]));
    assert_eq!(config["download_thread_cnt"], json!(0));
    assert_eq!(config["download_timeout"], json!(2.5));
    assert_eq!(config["ignored_errors"], json!([1006, -5, 1006]));
    assert_eq!(config["log_path"], json!(""));
    assert_eq!(config["log_verbose"], json!(0));
    assert_eq!(config["save_tasks"], json!(true));
    assert_eq!(config["low_speed_threshold"], json!(0.5));

    let task = report.get("task").expect("task snapshot");
    assert_eq!(task["dir"], json!("downloads"));
    assert_eq!(task["download_ori"], json!(true));
    assert_eq!(task["download_thread_cnt"], json!(0));
    assert_eq!(task["scan_thread_cnt"], json!(3));
    assert_eq!(task["proxy_image"], json!(false));
    assert_eq!(task["proxy_image_only"], json!(true));
    assert_eq!(task["ignored_errors"], json!([1006, -5, 1006]));
    assert_eq!(task["low_speed_threshold"], json!(0.5));
    assert_eq!(task["download_range"], json!([[2, 4], [9, 9]]));
    assert_eq!(task["download_timeout"], json!(2.5));
}

#[test]
fn partial_file_keeps_missing_defaults_and_accepts_explicit_empty_values() {
    let root = TestRoot::create();
    let path = write_file(
        root.path(),
        "config.json",
        &fixture_text("user_partial.json"),
    );
    let report = load_explicit(root.path(), &path);
    assert_eq!(report.get("ok"), Some(&Value::Bool(true)), "{report}");
    let config = report.get("config").expect("config");

    assert_eq!(config["jpn_title"], json!(false));
    assert_eq!(config["download_thread_cnt"], json!(0));
    assert_eq!(config["log_path"], json!(""));
    assert_eq!(config["proxy"], json!([]));
    assert_eq!(config["ignored_errors"], json!([]));
    assert!(config["download_range"].is_null());
    assert_eq!(config["daemon"], json!(false));
    assert_eq!(config["scan_thread_cnt"], json!(1));
    assert_eq!(config["log_verbose"], json!(2));
}

#[test]
fn default_file_missing_uses_defaults_but_explicit_missing_is_an_error() {
    let root_a = TestRoot::create();
    let report = load_default(root_a.path());
    assert_eq!(report.get("ok"), Some(&Value::Bool(true)), "{report}");
    assert_eq!(
        report.get("source").and_then(Value::as_str),
        Some("defaults")
    );

    let root_b = TestRoot::create();
    let missing = root_b.path().join("absent.json");
    let report = load_explicit(root_b.path(), &missing);
    assert_rejected(&report, "missing_file");
}

#[test]
fn default_file_present_at_the_entry_base_is_loaded() {
    let root_a = TestRoot::create();
    write_file(
        root_a.path(),
        "config.json",
        "{\n  \"jpn_title\": false,\n  \"scan_thread_cnt\": 4\n}\n",
    );
    let report = load_default(root_a.path());
    assert_eq!(report.get("ok"), Some(&Value::Bool(true)), "{report}");
    assert_eq!(
        report.get("source").and_then(Value::as_str),
        Some("file"),
        "{report}"
    );
    assert_eq!(report["config"]["jpn_title"], json!(false));
    assert_eq!(report["config"]["scan_thread_cnt"], json!(4));

    let root_b = TestRoot::create();
    write_file(root_b.path(), "other.json", "{\"scan_thread_cnt\": 9}\n");
    let report = load_default(root_b.path());
    assert_eq!(
        report.get("source").and_then(Value::as_str),
        Some("defaults"),
        "the other root must not leak into this one: {report}"
    );
    assert_eq!(report["config"]["scan_thread_cnt"], json!(1));
}

#[test]
fn bad_json_is_rejected_by_the_core_decoder() {
    let root = TestRoot::create();
    let path = write_file(root.path(), "config.json", &fixture_text("bad_json.json"));
    let error = run_config_scenario(
        root.path(),
        &load_request(root.path(), json!(path)).to_string(),
    )
    .expect_err("bad JSON must not be silently ignored");
    let text = error.to_string();
    assert!(
        text.contains("json_decode failed"),
        "the Core decoder must report the syntax error: {text}"
    );
}

#[test]
fn wrong_root_type_and_unknown_keys_are_rejected() {
    let root = TestRoot::create();
    let path = write_file(root.path(), "config.json", &fixture_text("root_array.json"));
    let error = run_config_scenario(
        root.path(),
        &load_request(root.path(), json!(path)).to_string(),
    )
    .expect_err("a non-object root must not be accepted");
    let text = error.to_string();
    assert!(
        text.contains("TypeMismatch") || text.contains("map"),
        "the Core map decode must report the root type: {text}"
    );

    let path = write_file(
        root.path(),
        "config.json",
        &fixture_text("unknown_key.json"),
    );
    let report = load_default_with_path(root.path(), &path);
    assert_rejected(&report, "unknown_key:urls");
}

#[test]
fn field_type_errors_are_rejected() {
    let root = TestRoot::create();
    let cases = [
        ("{\"daemon\": \"yes\"}", "type:daemon"),
        ("{\"daemon\": null}", "type:daemon"),
        ("{\"download_thread_cnt\": 1.5}", "type:download_thread_cnt"),
        ("{\"proxy\": [1]}", "type:proxy"),
        ("{\"download_range\": [[1]]}", "type:download_range"),
        ("{\"download_range\": [[1, 2, 3]]}", "type:download_range"),
        ("{\"rpc_port\": \"8080\"}", "type:rpc_port"),
        ("{\"ignored_errors\": [\"1006\"]}", "type:ignored_errors"),
        ("{\"auto_update\": 1}", "type:auto_update"),
        (
            "{\"low_speed_threshold\": true}",
            "type:low_speed_threshold",
        ),
    ];
    for (document, expected) in cases {
        let path = write_file(root.path(), "case.json", document);
        let report = load_default_with_path(root.path(), &path);
        assert_rejected(&report, expected);
    }
}

fn load_default_with_path(root: &Path, path: &Path) -> Value {
    load_explicit(root, path)
}

#[test]
fn runtime_update_merges_ignored_errors_and_keeps_the_task_snapshot() {
    let root = TestRoot::create();
    let config = write_file(root.path(), "config.json", &fixture_text("user_full.json"));
    let overlay = write_file(root.path(), "overlay.json", &fixture_text("overlay.json"));
    let report = scenario(
        root.path(),
        json!({
            "op": "update",
            "base": root.path().to_str().expect("utf-8 root"),
            "path": config.to_str().expect("utf-8 config"),
            "overlay": overlay.to_str().expect("utf-8 overlay"),
        }),
    );

    assert_eq!(report.get("ok"), Some(&Value::Bool(true)), "{report}");
    let before = report.get("task_before").expect("task_before");
    let after = report.get("task_after").expect("task_after");
    assert_eq!(before, after, "a created task keeps its snapshot");

    assert_eq!(
        report["config"]["proxy"],
        json!(["socks5h://127.0.0.1:1080", "http://127.0.0.1:8080"])
    );
    assert_eq!(
        report["config"]["ignored_errors"],
        json!([1006, -5, 1009, -3])
    );
    assert_eq!(report["config"]["download_thread_cnt"], json!(2));
    assert_eq!(report["config"]["make_archive"], json!(false));
    assert_eq!(report["config"]["log_path"], json!("updated.log"));
    assert_eq!(report["config"]["download_timeout"], json!(7));
    assert_eq!(report["config"]["dir"], json!("updated"));
    assert_eq!(
        report["presence"],
        json!({"proxy": true, "dir": true, "ignored_errors": true})
    );

    let public = report.get("public").expect("public view after update");
    assert_eq!(public["ignored_errors"], json!([1006, -5, 1009, -3]));
    assert!(public.get("rpc_port").is_none());
}

#[test]
fn runtime_update_with_empty_ignored_errors_keeps_the_existing_set() {
    let root = TestRoot::create();
    let config = write_file(root.path(), "config.json", &fixture_text("user_full.json"));
    let overlay = write_file(
        root.path(),
        "overlay.json",
        &fixture_text("overlay_empty_errors.json"),
    );
    let report = scenario(
        root.path(),
        json!({
            "op": "update",
            "base": root.path().to_str().expect("utf-8 root"),
            "path": config.to_str().expect("utf-8 config"),
            "overlay": overlay.to_str().expect("utf-8 overlay"),
        }),
    );
    assert_eq!(report.get("ok"), Some(&Value::Bool(true)), "{report}");
    assert_eq!(
        report["config"]["ignored_errors"],
        json!([1006, -5, 1006]),
        "the original update only merges a non-empty ignored_errors list"
    );
    assert_eq!(report["config"]["log_verbose"], json!(4));
    assert_eq!(
        report["presence"],
        json!({"proxy": false, "dir": false, "ignored_errors": true})
    );
}

#[test]
fn runtime_update_empty_proxy_list_is_applied_to_the_config_view() {
    let root = TestRoot::create();
    let config = write_file(root.path(), "config.json", &fixture_text("user_full.json"));
    let overlay = write_file(
        root.path(),
        "overlay.json",
        "{\n  \"proxy\": [],\n  \"jpn_title\": true\n}\n",
    );
    let report = scenario(
        root.path(),
        json!({
            "op": "update",
            "base": root.path().to_str().expect("utf-8 root"),
            "path": config.to_str().expect("utf-8 config"),
            "overlay": overlay.to_str().expect("utf-8 overlay"),
        }),
    );
    assert_eq!(report.get("ok"), Some(&Value::Bool(true)), "{report}");
    assert_eq!(report["config"]["proxy"], json!([]));
    assert_eq!(report["config"]["jpn_title"], json!(true));
    assert_eq!(
        report["presence"],
        json!({"proxy": true, "dir": false, "ignored_errors": false})
    );
}

#[test]
fn two_fresh_roots_run_the_same_suite_independently() {
    let expectations = [
        ("a", Some("{\n  \"jpn_title\": false\n}\n"), false, "file"),
        ("b", None, true, "defaults"),
    ];
    for (name, document, jpn_title, source) in expectations {
        let root = TestRoot::create();
        if let Some(document) = document {
            write_file(root.path(), "config.json", document);
        }
        let report = load_default(root.path());
        assert_eq!(
            report.get("ok"),
            Some(&Value::Bool(true)),
            "{name}: {report}"
        );
        assert_eq!(
            report.get("source").and_then(Value::as_str),
            Some(source),
            "{name}: {report}"
        );
        assert_eq!(report["config"]["jpn_title"], json!(jpn_title), "{name}");
    }
}
