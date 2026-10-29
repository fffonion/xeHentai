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

fn default_task_projection() -> Value {
    json!({
        "dir": ".",
        "download_ori": false,
        "download_thread_cnt": 5,
        "scan_thread_cnt": 1,
        "proxy_image": true,
        "proxy_image_only": false,
        "ignored_errors": [],
        "low_speed_threshold": 10,
        "rename_ori": false,
        "make_archive": false,
        "delete_task_files": false,
        "jpn_title": true,
        "download_range": null,
        "download_timeout": 10,
    })
}

fn user_full_task_projection() -> Value {
    json!({
        "dir": "downloads",
        "download_ori": true,
        "download_thread_cnt": 0,
        "scan_thread_cnt": 3,
        "proxy_image": false,
        "proxy_image_only": true,
        "ignored_errors": [1006, -5, 1006],
        "low_speed_threshold": 0.5,
        "rename_ori": true,
        "make_archive": true,
        "delete_task_files": true,
        "jpn_title": false,
        "download_range": [[2, 4], [9, 9]],
        "download_timeout": 2.5,
    })
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
    assert!(
        report.get("task_projection").is_some_and(Value::is_null),
        "{report}"
    );
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
    let projection = report.get("task_projection").expect("task projection");
    assert_eq!(object_keys(projection).len(), TASK_KEYS);
    assert_eq!(
        projection,
        &default_task_projection(),
        "the projection must be the independent 14-key expectation, not a second call of the same function"
    );
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

    let projection = report.get("task_projection").expect("task projection");
    assert_eq!(object_keys(projection).len(), TASK_KEYS);
    assert_eq!(projection, &user_full_task_projection());
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

/// Twice the Core default script call depth (1024), so per-element recursion
/// exhausts the 1024-frame limit.
///
/// The generated documents stay far below the 4 MiB configuration input
/// limit, and these tests never perform network or directory operations.
const LARGE_ARRAY_ITEMS: usize = 2048;

fn large_proxy_document() -> Value {
    let values: Vec<Value> = (0..LARGE_ARRAY_ITEMS)
        .map(|index| json!(format!("socks5h://127.0.0.1:{}", 1080 + index)))
        .collect();
    json!({ "proxy": values })
}

fn large_ignored_errors_document() -> Value {
    let values: Vec<Value> = (0..LARGE_ARRAY_ITEMS)
        .map(|index| json!(index as i64 - 1024))
        .collect();
    json!({ "ignored_errors": values })
}

fn large_download_range_document() -> Value {
    let values: Vec<Value> = (0..LARGE_ARRAY_ITEMS)
        .map(|index| json!([index, index + 1]))
        .collect();
    json!({ "download_range": values })
}

#[test]
fn large_proxy_array_validates_every_element_without_call_depth() {
    let root = TestRoot::create();
    let path = write_file(
        root.path(),
        "large_proxy.json",
        &large_proxy_document().to_string(),
    );
    let report = load_explicit(root.path(), &path);
    assert_eq!(
        report.get("ok"),
        Some(&Value::Bool(true)),
        "a 2048-entry proxy array must load: {report}"
    );
    let proxy = report["config"]["proxy"]
        .as_array()
        .expect("proxy must stay an array");
    assert_eq!(proxy.len(), LARGE_ARRAY_ITEMS);
    assert_eq!(proxy[0], json!("socks5h://127.0.0.1:1080"));
    assert_eq!(
        proxy[LARGE_ARRAY_ITEMS - 1],
        json!(format!(
            "socks5h://127.0.0.1:{}",
            1080 + LARGE_ARRAY_ITEMS - 1
        ))
    );

    let mut invalid = large_proxy_document();
    invalid["proxy"].as_array_mut().expect("proxy array")[LARGE_ARRAY_ITEMS - 1] = json!(2048);
    let path = write_file(
        root.path(),
        "large_proxy_invalid.json",
        &invalid.to_string(),
    );
    let report = load_explicit(root.path(), &path);
    assert_rejected(&report, "type:proxy");
}

#[test]
fn large_ignored_errors_array_validates_every_element_without_call_depth() {
    let root = TestRoot::create();
    let path = write_file(
        root.path(),
        "large_ignored_errors.json",
        &large_ignored_errors_document().to_string(),
    );
    let report = load_explicit(root.path(), &path);
    assert_eq!(
        report.get("ok"),
        Some(&Value::Bool(true)),
        "a 2048-entry ignored_errors array must load: {report}"
    );
    let errors = report["config"]["ignored_errors"]
        .as_array()
        .expect("ignored_errors must stay an array");
    assert_eq!(errors.len(), LARGE_ARRAY_ITEMS);
    assert_eq!(errors[0], json!(-1024));
    assert_eq!(
        errors[LARGE_ARRAY_ITEMS - 1],
        json!((LARGE_ARRAY_ITEMS - 1) as i64 - 1024)
    );

    let mut invalid = large_ignored_errors_document();
    invalid["ignored_errors"]
        .as_array_mut()
        .expect("ignored_errors array")[LARGE_ARRAY_ITEMS - 1] = json!("1006");
    let path = write_file(
        root.path(),
        "large_ignored_errors_invalid.json",
        &invalid.to_string(),
    );
    let report = load_explicit(root.path(), &path);
    assert_rejected(&report, "type:ignored_errors");
}

#[test]
fn large_download_range_array_validates_every_element_without_call_depth() {
    let root = TestRoot::create();
    let path = write_file(
        root.path(),
        "large_download_range.json",
        &large_download_range_document().to_string(),
    );
    let report = load_explicit(root.path(), &path);
    assert_eq!(
        report.get("ok"),
        Some(&Value::Bool(true)),
        "a 2048-entry download_range array must load: {report}"
    );
    let ranges = report["config"]["download_range"]
        .as_array()
        .expect("download_range must stay an array");
    assert_eq!(ranges.len(), LARGE_ARRAY_ITEMS);
    assert_eq!(ranges[0], json!([0, 1]));
    assert_eq!(
        ranges[LARGE_ARRAY_ITEMS - 1],
        json!([LARGE_ARRAY_ITEMS - 1, LARGE_ARRAY_ITEMS])
    );

    let mut invalid = large_download_range_document();
    invalid["download_range"]
        .as_array_mut()
        .expect("download_range array")[LARGE_ARRAY_ITEMS - 1] = json!([2048, "end"]);
    let path = write_file(
        root.path(),
        "large_download_range_invalid.json",
        &invalid.to_string(),
    );
    let report = load_explicit(root.path(), &path);
    assert_rejected(&report, "type:download_range");
}

#[test]
fn envelope_errors_use_the_task_projection_field() {
    let root = TestRoot::create();

    let report = scenario(
        root.path(),
        json!({ "op": "unknown", "base": root.path().to_str().expect("utf-8 root") }),
    );
    assert_eq!(report.get("ok"), Some(&Value::Bool(false)), "{report}");
    assert_eq!(
        report.get("error").and_then(Value::as_str),
        Some("unknown_op:unknown"),
        "{report}"
    );
    assert!(report.get("task").is_none(), "{report}");
    assert!(
        report.get("task_projection").is_some_and(Value::is_null),
        "{report}"
    );

    let report = scenario(root.path(), json!({ "op": "load" }));
    assert_eq!(
        report.get("error").and_then(Value::as_str),
        Some("type:base"),
        "{report}"
    );
    assert!(report.get("task").is_none(), "{report}");
    assert!(
        report.get("task_projection").is_some_and(Value::is_null),
        "{report}"
    );
}

#[test]
fn runtime_update_reports_effective_config_and_pre_update_task_projection() {
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
    for legacy in ["task", "task_before", "task_after"] {
        assert!(
            report.get(legacy).is_none(),
            "the self-referential {legacy} field must not be reported: {report}"
        );
    }
    let projection = report.get("task_projection").expect("pure task projection");
    assert_eq!(object_keys(projection).len(), TASK_KEYS);
    assert_eq!(
        projection,
        &user_full_task_projection(),
        "the update report projects the pre-update document; this is a field projection only, not a task lifecycle assertion"
    );

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
