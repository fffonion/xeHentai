// SPDX-License-Identifier: GPL-3.0-or-later
//! Focused execution coverage for the pure-RSS site and rules entry.

use std::future::poll_fn;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::task::Poll;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use vm::{InvocationItem, InvocationPoll, IoHostExt, IoPolicy, JitConfig, Value as VmValue, Vm};
use xeh_host::{
    attach_timer, bind_vm, compile_file_with_catalog, drive_to_halt, open_registry,
    standard_catalog, EmbeddingTimer,
};

struct TestRoot(PathBuf);

impl TestRoot {
    fn create() -> Self {
        let base = PathBuf::from(std::env::var_os("XEH_LEASE_TMP").expect("XEH_LEASE_TMP"));
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let path = base.join(format!("task7-host-test-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&path).unwrap_or_else(|error| {
            panic!("create task-owned test root {}: {error}", path.display())
        });
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TestRoot {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn site_entry_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/site/entry.rss")
}

fn run_site(request: &Value) -> Result<Value, String> {
    let root = TestRoot::create();
    let debug_path = root.path().join("debug.htm");
    run_site_with_paths(request, &debug_path, root.path())
}

fn run_site_with_paths(
    request: &Value,
    debug_path: &Path,
    allowed_root: &Path,
) -> Result<Value, String> {
    let debug_path = debug_path
        .to_str()
        .ok_or_else(|| "debug path is not valid UTF-8".to_string())?
        .to_string();
    let allowed_root = allowed_root
        .to_str()
        .ok_or_else(|| "debug capability root is not valid UTF-8".to_string())?
        .to_string();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    runtime.block_on(async {
        let catalog = standard_catalog();
        let program = compile_file_with_catalog(&site_entry_path(), Arc::clone(&catalog))
            .map_err(|error| error.to_string())?;
        let mut vm = Vm::try_new(program).map_err(|error| format!("{error:?}"))?;
        vm.set_jit_config(JitConfig {
            enabled: false,
            ..Default::default()
        });
        let registry = open_registry(&catalog).map_err(|error| error.to_string())?;
        bind_vm(&mut vm, &registry).map_err(|error| error.to_string())?;
        attach_timer(&mut vm, Arc::new(EmbeddingTimer::new()));
        vm.configure_io(IoPolicy {
            allowed_roots: vec![allowed_root],
            allow_write: true,
            allow_process: false,
            max_read_bytes: 8 * 1024 * 1024,
            max_write_bytes: 8 * 1024 * 1024,
        });
        drive_to_halt(&mut vm)
            .await
            .map_err(|error| error.to_string())?;
        let callable = vm
            .resolve_exported_callable("run")
            .map_err(|error| format!("{error:?}"))?;
        let mut invocation = vm
            .start_invocation(
                callable,
                vec![
                    VmValue::string(request.to_string()),
                    VmValue::string(debug_path),
                ],
            )
            .map_err(|error| format!("{error:?}"))?;
        let value = poll_fn(|context| loop {
            match invocation.poll_next_with_context(context) {
                Ok(InvocationPoll::Pending) => return Poll::Pending,
                Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Event(_))))) => continue,
                Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value))))) => {
                    return Poll::Ready(Ok(value));
                }
                Ok(InvocationPoll::Ready(Some(Err(error)))) => {
                    return Poll::Ready(Err(format!("invocation: {error:?}")));
                }
                Ok(InvocationPoll::Ready(None)) => {
                    return Poll::Ready(Err("invocation ended without a result".to_string()));
                }
                Err(error) => return Poll::Ready(Err(format!("invocation: {error:?}"))),
            }
        })
        .await?;
        let VmValue::String(text) = value else {
            return Err(format!("site entry returned {value:?}, not a string"));
        };
        serde_json::from_str(text.as_str())
            .map_err(|error| format!("site report is not JSON: {error}\n{text}"))
    })
}

#[test]
fn support_module_compiles_independently() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/site/support.rss");
    compile_file_with_catalog(&path, standard_catalog())
        .unwrap_or_else(|error| panic!("support module compile: {error}"));
}

#[test]
fn download_rule_module_compiles() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/rules/download.rss");
    compile_file_with_catalog(&path, standard_catalog())
        .unwrap_or_else(|error| panic!("download rule compile: {error}"));
}

#[test]
fn rules_engine_compiles_the_ordered_site_filters() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/rules/engine.rss");
    compile_file_with_catalog(&path, standard_catalog())
        .unwrap_or_else(|error| panic!("rules engine compile: {error}"));
}

#[test]
fn site_support_time_adds_one_minute_when_minutes_are_absent() {
    let report = run_site(&json!({
        "operation": "support.time",
        "status_code": 200,
        "requested_url": "https://e-hentai.org/g/1/aaaaaaaaaa/",
        "real_url": "https://e-hentai.org/g/1/aaaaaaaaaa/",
        "content_length": null,
        "header_names": [],
        "header_values": [],
        "text": "2 days 3 hours",
        "original_image": false,
        "stream_callback_present": false,
        "stream_events": [],
        "suc_returns": [],
        "fail_returns": []
    }))
    .expect("support time result");

    assert_eq!(report["return"], json!(183_660));
    assert_eq!(report["exception"], Value::Null);
}

#[test]
fn metadata_module_compiles_with_support() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/site/metadata.rss");
    compile_file_with_catalog(&path, standard_catalog())
        .unwrap_or_else(|error| panic!("metadata module compile: {error}"));
}

#[test]
fn pages_write_raw_debug_body_only_inside_trusted_root_and_swallow_denial() {
    let root = TestRoot::create();
    let debug_path = root.path().join("debug.htm");
    let body = "<html>unmatched page body &amp; Ω\nraw bytes\n";
    let request = json!({
        "operation": "pages",
        "status_code": 200,
        "requested_url": "https://e-hentai.org/g/1/aaaaaaaaaa/",
        "real_url": "https://e-hentai.org/g/1/aaaaaaaaaa/",
        "content_length": body.len(),
        "header_names": [],
        "header_values": [],
        "text": body,
        "original_image": false,
        "stream_callback_present": false,
        "stream_events": [],
        "suc_returns": [],
        "fail_returns": ["no-pages"]
    });

    let report = run_site_with_paths(&request, &debug_path, root.path())
        .expect("page debug write RSS execution");
    assert_eq!(report["callbacks"].as_array().unwrap().len(), 1);
    assert_eq!(report["callbacks"][0]["kind"], json!("fail"));
    assert_eq!(report["callbacks"][0]["value"], json!(1008));
    assert_eq!(std::fs::read(&debug_path).unwrap(), body.as_bytes());

    let outside = root.path().parent().unwrap().join(format!(
        "{}-outside-debug.htm",
        root.path().file_name().unwrap().to_string_lossy()
    ));
    let denied_report = run_site_with_paths(&request, &outside, root.path())
        .expect("denied debug write remains best-effort");
    assert_eq!(denied_report["callbacks"][0]["value"], json!(1008));
    let outside_existed = outside.exists();
    if outside_existed {
        std::fs::remove_file(&outside).unwrap();
    }
    assert!(!outside_existed, "debug write escaped its trusted root");
}

#[test]
fn metadata_emits_the_ordered_success_with_single_pass_entities() {
    let _root = TestRoot::create();
    let request = json!({
        "operation": "metadata",
        "status_code": 200,
        "requested_url": "https://e-hentai.org/g/1/aaaaaaaaaa/",
        "real_url": "https://e-hentai.org/g/1/aaaaaaaaaa/",
        "content_length": 128,
        "header_names": [],
        "header_values": [],
        "text": "<h1 id=\"gj\">A &amp;lt; Ω</h1><h1 id=\"gn\">Synthetic</h1>Length:</td><td class=\"gdt2\">2 pages</td>Showing 1 - 20 of 20 images toggle_tagmenu('artist:one' toggle_tagmenu('artist:one'",
        "original_image": false,
        "stream_callback_present": true,
        "stream_events": [],
        "suc_returns": ["success-A"],
        "fail_returns": ["failure-A"]
    });

    let report = run_site(&request).expect("metadata RSS execution");
    assert_eq!(report["callbacks"][0]["kind"], json!("suc"));
    assert_eq!(report["callbacks"][0]["value"]["gjname"], json!("A &lt; Ω"));
    assert_eq!(
        report["callbacks"][0]["value"]["tags"],
        json!(["artist:one", "artist:one"])
    );
    assert_eq!(report["callbacks"][0]["returned"], json!("success-A"));
    assert_eq!(report["return"], Value::Null);
    assert_eq!(report["exception"], Value::Null);
}

#[test]
fn pages_preserve_duplicate_links_order_and_each_callback_return() {
    let request = json!({
        "operation": "pages",
        "status_code": 200,
        "requested_url": "https://e-hentai.org/g/1/aaaaaaaaaa/",
        "real_url": "https://e-hentai.org/g/1/aaaaaaaaaa/",
        "content_length": 128,
        "header_names": [],
        "header_values": [],
        "text": "<a href=\"https://e-hentai.org/s/abcdef0123/1-1\"><div><div title=\"Page 1\"></a><a href=\"https://exhentai.org/s/0123456789/2-2\"><div><div title=\"Page 2\"></a><a href=\"https://e-hentai.org/s/abcdef0123/1-1\"><div><div title=\"Page 1 again\"></a>",
        "original_image": false,
        "stream_callback_present": true,
        "stream_events": [],
        "suc_returns": ["page-A", "page-B", "page-C"],
        "fail_returns": ["failure-A"]
    });

    let report = run_site(&request).expect("pages RSS execution");
    assert_eq!(report["callbacks"].as_array().unwrap().len(), 3);
    assert_eq!(
        report["callbacks"][0]["value"],
        json!("https://e-hentai.org/s/abcdef0123/1-1")
    );
    assert_eq!(
        report["callbacks"][1]["value"],
        json!("https://exhentai.org/s/0123456789/2-2")
    );
    assert_eq!(
        report["callbacks"][2]["value"],
        json!("https://e-hentai.org/s/abcdef0123/1-1")
    );
    assert_eq!(report["callbacks"][0]["returned"], json!("page-A"));
    assert_eq!(report["callbacks"][1]["returned"], json!("page-B"));
    assert_eq!(report["callbacks"][2]["returned"], json!("page-C"));
    assert_eq!(report["return"], Value::Null);
}

#[test]
fn mpv_uses_real_url_gid_and_preserves_imagelist_order() {
    let request = json!({
        "operation": "mpv",
        "status_code": 200,
        "requested_url": "https://exhentai.org/mpv/12345/abcdef/",
        "real_url": "https://exhentai.org/mpv/12345/abcdef/",
        "content_length": 128,
        "header_names": [],
        "header_values": [],
        "text": "imagelist = [{\"k\":\"key-a\"},{\"k\":\"key-b\"}];",
        "original_image": false,
        "stream_callback_present": true,
        "stream_events": [],
        "suc_returns": ["mpv-A", "mpv-B"],
        "fail_returns": ["failure-A"]
    });

    let report = run_site(&request).expect("MPV RSS execution");
    assert_eq!(report["callbacks"].as_array().unwrap().len(), 2, "{report}");
    assert_eq!(
        report["callbacks"][0]["value"],
        json!("https://exhentai.org/s/key-a/12345-1")
    );
    assert_eq!(
        report["callbacks"][1]["value"],
        json!("https://exhentai.org/s/key-b/12345-2")
    );
    assert_eq!(report["callbacks"][0]["returned"], json!("mpv-A"));
    assert_eq!(report["callbacks"][1]["returned"], json!("mpv-B"));
    assert_eq!(report["return"], Value::Null);
}

#[test]
fn login_extracts_both_cookie_values_and_returns_upstream_status() {
    let request = json!({
        "operation": "login",
        "status_code": 200,
        "requested_url": "https://exhentai.org/",
        "real_url": "https://exhentai.org/",
        "content_length": 40,
        "header_names": ["Set-Cookie"],
        "header_values": ["ipb_member_id=member-7; ipb_pass_hash=pass-9; path=/"],
        "text": "",
        "original_image": false,
        "stream_callback_present": false,
        "stream_events": [],
        "suc_returns": ["login-suc"],
        "fail_returns": ["login-fail"]
    });

    let report = run_site(&request).expect("login RSS execution");
    assert_eq!(report["callbacks"].as_array().unwrap().len(), 1);
    assert_eq!(report["callbacks"][0]["kind"], json!("suc"));
    assert_eq!(
        report["callbacks"][0]["value"]["ipb_member_id"],
        json!("member-7")
    );
    assert_eq!(
        report["callbacks"][0]["value"]["ipb_pass_hash"],
        json!("pass-9")
    );
    assert_eq!(report["callbacks"][0]["returned"], json!("login-suc"));
    assert_eq!(report["return"], json!(0));
}

#[test]
fn image_filter_uses_keystamp_url_real_url_and_filename_in_order() {
    let request = json!({
        "operation": "image",
        "status_code": 200,
        "requested_url": "https://e-hentai.org/s/abcdef/123-1",
        "real_url": "https://e-hentai.org/s/abcdef/123-1",
        "content_length": 1024,
        "header_names": ["Content-Type"],
        "header_values": ["image/jpeg"],
        "text": "<img src=\"https://img.test/path/file.jpg?keystamp=stamp\" style=\"x\"></a></div><div>file.jpg :: x :: y</di class=\"mr\" x <a href=\"https://img.test/original.jpg\" >Download original return nl('nonce-123')",
        "original_image": false,
        "stream_callback_present": true,
        "stream_events": [],
        "suc_returns": ["image-suc"],
        "fail_returns": ["image-fail"]
    });

    let report = run_site(&request).expect("image RSS execution");
    assert_eq!(report["callbacks"].as_array().unwrap().len(), 1);
    assert_eq!(report["callbacks"][0]["kind"], json!("suc"));
    assert_eq!(
        report["callbacks"][0]["value"],
        json!([
            "https://img.test/path/file.jpg?keystamp=stamp",
            "https://e-hentai.org/s/abcdef/123-1?nl=nonce-123",
            "file.jpg"
        ])
    );
    assert_eq!(report["callbacks"][0]["returned"], json!("image-suc"));
    assert_eq!(report["return"], Value::Null);
}

const IMAGE_PAGE: &str = "<img src=\"https://img.test/path/file.jpg?keystamp=stamp\" style=\"x\"></a></div><div>file.jpg :: x :: y</di class=\"mr\" x <a href=\"https://img.test/original.jpg\" >Download original return nl('nonce-123')";

fn image_request(
    status_code: i64,
    requested_url: &str,
    real_url: &str,
    content_length: Value,
    headers: &[(&str, &str)],
    text: &str,
) -> Value {
    let names = headers.iter().map(|(name, _)| *name).collect::<Vec<_>>();
    let values = headers.iter().map(|(_, value)| *value).collect::<Vec<_>>();
    json!({
        "operation": "image",
        "status_code": status_code,
        "requested_url": requested_url,
        "real_url": real_url,
        "content_length": content_length,
        "header_names": names,
        "header_values": values,
        "text": text,
        "original_image": false,
        "stream_callback_present": true,
        "stream_events": [],
        "suc_returns": ["image-suc"],
        "fail_returns": ["image-fail"]
    })
}

#[test]
fn image_quota_checks_connection_before_key_and_other_limits() {
    let requested =
        "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-12-100-100-jpg/509.gif";
    let real = "https://e-hentai.org/s/key/123-1";
    let request = image_request(
        600,
        requested,
        real,
        json!(925),
        &[("Content-Type", "text/html")],
        "exceeded your image viewing limits",
    );

    let report = run_site(&request).expect("quota RSS execution");
    assert_eq!(report["callbacks"].as_array().unwrap().len(), 1);
    assert_eq!(report["callbacks"][0]["kind"], json!("fail"));
    assert_eq!(
        report["callbacks"][0]["value"],
        json!([1009, real, requested])
    );
    assert_eq!(report["callbacks"][0]["returned"], json!("image-fail"));
    assert_eq!(report["return"], Value::Null);
}

#[test]
fn image_quota_key_failure_precedes_quota_and_size_detection() {
    let requested =
        "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-12-100-100-jpg/509.gif";
    let real = "https://e-hentai.org/s/key/123-1";
    let request = image_request(403, requested, real, json!(925), &[], IMAGE_PAGE);

    let report = run_site(&request).expect("quota RSS execution");
    assert_eq!(
        report["callbacks"][0]["value"],
        json!([1007, real, requested])
    );
    assert_eq!(report["callbacks"][0]["returned"], json!("image-fail"));
    assert_eq!(report["return"], Value::Null);
}

#[test]
fn image_quota_allows_matching_encoded_size_and_rejects_a_mismatch() {
    let real = "https://e-hentai.org/s/key/123-1";
    let matching_url = "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-925-100-100-jpg";
    let allowed = image_request(
        200,
        matching_url,
        real,
        json!(925),
        &[("content-type", "image/jpeg")],
        IMAGE_PAGE,
    );
    let allowed_report = run_site(&allowed).expect("matching size RSS execution");
    assert_eq!(allowed_report["callbacks"][0]["kind"], json!("suc"));

    let mismatched_url = "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-12-100-100-jpg";
    let rejected = image_request(
        200,
        mismatched_url,
        real,
        json!(925),
        &[("Content-Type", "image/jpeg")],
        IMAGE_PAGE,
    );
    let rejected_report = run_site(&rejected).expect("mismatched size RSS execution");
    assert_eq!(
        rejected_report["callbacks"][0]["value"],
        json!([1006, real, mismatched_url])
    );
    assert_eq!(rejected_report["return"], Value::Null);
}

#[test]
fn image_quota_uses_strict_small_text_threshold_and_case_sensitive_body_match() {
    let requested = "https://img.test/unhashed";
    let real = "https://e-hentai.org/s/key/123-1";
    let blocked_text = image_request(
        200,
        requested,
        real,
        json!(199),
        &[("Content-Type", "text/html")],
        "exceeded your image viewing limits",
    );
    let blocked = run_site(&blocked_text).expect("small text quota execution");
    assert_eq!(
        blocked["callbacks"][0]["value"],
        json!([1006, real, requested])
    );

    let boundary = image_request(
        200,
        requested,
        real,
        json!(200),
        &[("Content-Type", "text/html")],
        "exceeded your image viewing limits",
    );
    let boundary_report = run_site(&boundary).expect("threshold RSS execution");
    assert_eq!(
        boundary_report["callbacks"][0]["value"],
        json!([1013, real, requested])
    );

    let case_mismatch = image_request(
        200,
        requested,
        real,
        json!(199),
        &[("Content-Type", "Text/html")],
        "Exceeded your image viewing limits",
    );
    let case_report = run_site(&case_mismatch).expect("case-sensitive RSS execution");
    assert_eq!(
        case_report["callbacks"][0]["value"],
        json!([1013, real, requested])
    );
}

#[test]
fn image_original_mode_uses_full_url_and_appends_redirect_to_both_queries() {
    let requested = "https://e-hentai.org/s/abcdef/123-1?source=task7";
    let real = "https://e-hentai.org/s/abcdef/123-1?server=1";
    let request = json!({
        "operation": "image",
        "status_code": 200,
        "requested_url": requested,
        "real_url": real,
        "content_length": 1024,
        "header_names": [],
        "header_values": [],
        "text": "<img src=\"https://img.test/path/file.jpg?keystamp=stamp\" style=\"x\"></a></div><div>file.jpg :: x :: y</di class=\"mr\" x <a href=\"https://img.test/original.jpg?token=1\" >Download original return nl('nonce-123')",
        "original_image": true,
        "stream_callback_present": false,
        "stream_events": [],
        "suc_returns": ["original-suc"],
        "fail_returns": ["original-fail"]
    });

    let report = run_site(&request).expect("original image RSS execution");
    assert_eq!(report["callbacks"][0]["value"], json!([
        "https://img.test/original.jpg?token=1&redirect=https://e-hentai.org/s/abcdef/123-1?source=task7",
        "https://e-hentai.org/s/abcdef/123-1?server=1&nl=nonce-123&redirect=https://e-hentai.org/s/abcdef/123-1?source=task7",
        "file.jpg"
    ]));
    assert_eq!(report["return"], Value::Null);
}

#[test]
fn image_style_fallback_unescapes_entities_and_recovers_image_php_name() {
    let requested = "https://e-hentai.org/s/abcdef/123-1";
    let real = "https://e-hentai.org/s/abcdef/123-1";
    let request = json!({
        "operation": "image",
        "status_code": 200,
        "requested_url": requested,
        "real_url": real,
        "content_length": 1024,
        "header_names": [],
        "header_values": [],
        "text": "<img src=\"https://img.test/image.php?n=pretty&amp;name.jpg\" style=\"x\"></a></div><div>image.php :: x :: y</di return nl('nonce-123')",
        "original_image": false,
        "stream_callback_present": false,
        "stream_events": [],
        "suc_returns": ["fallback-suc"],
        "fail_returns": ["fallback-fail"]
    });

    let report = run_site(&request).expect("style fallback RSS execution");
    assert_eq!(
        report["callbacks"][0]["value"],
        json!([
            "https://img.test/image.php?n=pretty&name.jpg",
            "https://e-hentai.org/s/abcdef/123-1?nl=nonce-123",
            "pretty&name.jpg"
        ])
    );
    assert!(report["decisions"]
        .as_array()
        .unwrap()
        .iter()
        .any(|value| value == "image:filename-from-n-query"));
}

#[test]
fn image_invalid_page_check_is_prefix_anchored_and_uses_scan_error_otherwise() {
    let real = "https://e-hentai.org/s/abcdef/123-1";
    let exact = image_request(
        200,
        "https://img.test/page",
        real,
        json!(1024),
        &[],
        "Invalid page 1",
    );
    let exact_report = run_site(&exact).expect("invalid page RSS execution");
    assert_eq!(
        exact_report["callbacks"][0]["value"],
        json!([1005, real, "https://img.test/page"])
    );

    let prefixed = image_request(
        200,
        "https://img.test/page",
        real,
        json!(1024),
        &[],
        "wrapper says Invalid page",
    );
    let prefixed_report = run_site(&prefixed).expect("anchored prefix RSS execution");
    assert_eq!(
        prefixed_report["callbacks"][0]["value"],
        json!([1013, real, "https://img.test/page"])
    );
}

#[test]
fn download_stream_calls_success_then_chunk_callback_before_each_yield() {
    let requested = "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-4-10-10-jpg";
    let real = "https://e-hentai.org/s/key/123-1";
    let request = json!({
        "operation": "download",
        "status_code": 200,
        "requested_url": requested,
        "real_url": real,
        "content_length": 4,
        "header_names": [],
        "header_values": [],
        "text": "",
        "original_image": false,
        "stream_callback_present": true,
        "stream_events": [
            {"kind": "chunk", "bytes": [65, 66], "callback_return": "chunk-A"},
            {"kind": "chunk", "bytes": [], "callback_return": "chunk-empty"},
            {"kind": "chunk", "bytes": [67, 68], "callback_return": "chunk-B"}
        ],
        "suc_returns": ["download-suc"],
        "fail_returns": ["download-fail"]
    });

    let report = run_site(&request).expect("download RSS execution");
    assert_eq!(report["callbacks"].as_array().unwrap().len(), 1);
    assert_eq!(report["callbacks"][0]["kind"], json!("suc"));
    assert_eq!(
        report["callbacks"][0]["value"],
        json!(["stream", real, requested])
    );
    assert_eq!(report["callbacks"][0]["returned"], json!("download-suc"));
    assert_eq!(report["stream"][0]["kind"], json!("callback"));
    assert_eq!(report["stream"][1]["kind"], json!("yield"));
    assert_eq!(report["stream"][2]["bytes"], json!([]));
    assert_eq!(report["stream"][5]["kind"], json!("yield"));
    assert_eq!(report["stream"][6]["kind"], json!("complete"));
    assert_eq!(report["callbacks"][0]["order"], json!(0));
    assert_eq!(report["stream"][0]["order"], json!(1));
    assert_eq!(report["stream"][1]["order"], json!(2));
    assert_eq!(report["stream"][6]["bytes_read"], json!(4));
    assert_eq!(report["return"], Value::Null);
}

fn download_request(
    status_code: i64,
    requested_url: &str,
    real_url: &str,
    content_length: Value,
    callback_present: bool,
    stream_events: Value,
) -> Value {
    json!({
        "operation": "download",
        "status_code": status_code,
        "requested_url": requested_url,
        "real_url": real_url,
        "content_length": content_length,
        "header_names": [],
        "header_values": [],
        "text": "",
        "original_image": false,
        "stream_callback_present": callback_present,
        "stream_events": stream_events,
        "suc_returns": ["download-suc"],
        "fail_returns": ["download-fail"]
    })
}

#[test]
fn download_validation_keeps_404_broken_body_and_missing_callback_order() {
    let requested = "https://img.test/one";
    let real = "https://e-hentai.org/s/key/123-1";

    let not_found = download_request(404, requested, real, json!(0), false, json!([]));
    let not_found_report = run_site(&not_found).expect("404 download execution");
    assert_eq!(
        not_found_report["callbacks"][0]["value"],
        json!([1011, real, requested])
    );

    let broken = download_request(200, requested, real, json!(0), false, json!([]));
    let broken_report = run_site(&broken).expect("zero-length download execution");
    assert_eq!(
        broken_report["callbacks"][0]["value"],
        json!([1012, real, requested])
    );

    let missing_callback = download_request(200, requested, real, json!(1), false, json!([]));
    let callback_report = run_site(&missing_callback).expect("missing callback execution");
    assert_eq!(
        callback_report["callbacks"][0]["value"],
        json!([1014, real, requested])
    );
    assert_eq!(callback_report["return"], Value::Null);
}

#[test]
fn download_connection_error_aborts_after_callback_and_yield_order() {
    let requested = "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-4-10-10-jpg";
    let real = "https://e-hentai.org/s/key/123-1";
    let request = download_request(
        200,
        requested,
        real,
        json!(4),
        true,
        json!([
            {"kind": "chunk", "bytes": [65, 66], "callback_return": "chunk-A"},
            {"kind": "connection-error", "bytes": [], "callback_return": "unused"}
        ]),
    );

    let report = run_site(&request).expect("connection error RSS execution");
    assert_eq!(report["callbacks"].as_array().unwrap().len(), 2);
    assert_eq!(report["callbacks"][0]["kind"], json!("suc"));
    assert_eq!(report["callbacks"][1]["kind"], json!("fail"));
    assert_eq!(
        report["callbacks"][1]["value"],
        json!([1012, real, requested])
    );
    assert_eq!(report["callbacks"][1]["returned"], json!("download-fail"));
    assert_eq!(report["callbacks"][1]["order"], json!(1));
    assert_eq!(report["exception"], json!("DownloadAbortedException"));
    assert_eq!(report["stream"][0]["kind"], json!("callback"));
    assert_eq!(report["stream"][1]["kind"], json!("yield"));
    assert_eq!(report["stream"][2]["kind"], json!("connection-error"));
}

#[test]
fn download_callback_connection_error_aborts_before_yield() {
    let requested = "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-4-10-10-jpg";
    let real = "https://e-hentai.org/s/key/123-1";
    let request = download_request(
        200,
        requested,
        real,
        json!(4),
        true,
        json!([
            {"kind": "callback-connection-error", "bytes": [65, 66], "callback_return": "callback-A"}
        ]),
    );

    let report = run_site(&request).expect("callback connection error RSS execution");
    assert_eq!(
        report["callbacks"][1]["value"],
        json!([1012, real, requested])
    );
    assert_eq!(report["callbacks"][1]["returned"], json!("download-fail"));
    assert_eq!(report["callbacks"][1]["order"], json!(1));
    assert_eq!(report["exception"], json!("DownloadAbortedException"));
    assert_eq!(report["stream"].as_array().unwrap().len(), 2);
    assert_eq!(report["stream"][0]["kind"], json!("callback"));
    assert_eq!(report["stream"][0]["bytes"], json!([65, 66]));
    assert_eq!(report["stream"][1]["kind"], json!("connection-error"));
    assert_eq!(report["stream"][1]["bytes"], json!([]));
}

#[test]
fn download_short_and_long_streams_abort_after_preserving_all_chunk_events() {
    let real = "https://e-hentai.org/s/key/123-1";
    let url4 = "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-4-10-10-jpg";
    let short = download_request(
        200,
        url4,
        real,
        json!(4),
        true,
        json!([
            {"kind": "chunk", "bytes": [1, 2], "callback_return": "short"}
        ]),
    );
    let short_report = run_site(&short).expect("short stream RSS execution");
    assert_eq!(short_report["callbacks"].as_array().unwrap().len(), 2);
    assert_eq!(
        short_report["callbacks"][1]["value"],
        json!([1012, real, url4])
    );
    assert_eq!(short_report["exception"], json!("DownloadAbortedException"));
    assert_eq!(short_report["stream"][1]["kind"], json!("yield"));
    assert_eq!(short_report["stream"][0]["bytes_read"], json!(2));
    assert_eq!(short_report["stream"].as_array().unwrap().len(), 2);

    let url2 = "https://img.test/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-2-10-10-jpg";
    let long = download_request(
        200,
        url2,
        real,
        json!(2),
        true,
        json!([
            {"kind": "chunk", "bytes": [1, 2, 3], "callback_return": "long"}
        ]),
    );
    let long_report = run_site(&long).expect("long stream RSS execution");
    assert_eq!(
        long_report["callbacks"][1]["value"],
        json!([1012, real, url2])
    );
    assert_eq!(long_report["stream"][1]["bytes"], json!([1, 2, 3]));
    assert_eq!(long_report["exception"], json!("DownloadAbortedException"));
}

fn download_rule_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../app/rules/download.rss")
}

/// Drives `app/rules/download.rss` transitions as separate owner calls, the
/// way a downloader keeps one named per-stream state between bounded chunks.
struct StreamVm {
    runtime: tokio::runtime::Runtime,
    vm: Vm,
    begin_fn: VmValue,
    chunk_fn: VmValue,
    end_fn: VmValue,
    error_fn: VmValue,
    render_fn: VmValue,
    state_fn: VmValue,
}

impl StreamVm {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("stream runtime");
        let catalog = standard_catalog();
        let program = compile_file_with_catalog(&download_rule_path(), Arc::clone(&catalog))
            .expect("download rule module compile");
        let mut vm = Vm::try_new(program).expect("stream vm");
        vm.set_jit_config(JitConfig {
            enabled: false,
            ..Default::default()
        });
        let registry = open_registry(&catalog).expect("stream registry");
        bind_vm(&mut vm, &registry).expect("stream bind");
        runtime
            .block_on(drive_to_halt(&mut vm))
            .expect("stream halt");
        let begin_fn = vm
            .resolve_exported_callable("stream_begin")
            .expect("stream_begin");
        let chunk_fn = vm
            .resolve_exported_callable("stream_chunk")
            .expect("stream_chunk");
        let end_fn = vm
            .resolve_exported_callable("stream_end")
            .expect("stream_end");
        let error_fn = vm
            .resolve_exported_callable("stream_connection_error")
            .expect("stream_connection_error");
        let render_fn = vm
            .resolve_exported_callable("stream_step_json")
            .expect("stream_step_json");
        let state_fn = vm
            .resolve_exported_callable("stream_step_state")
            .expect("stream_step_state");
        Self {
            runtime,
            vm,
            begin_fn,
            chunk_fn,
            end_fn,
            error_fn,
            render_fn,
            state_fn,
        }
    }

    fn call(&mut self, callable: &VmValue, args: Vec<VmValue>) -> VmValue {
        let Self { runtime, vm, .. } = self;
        let mut invocation = vm
            .start_invocation(callable.clone(), args)
            .expect("stream invocation start");
        runtime.block_on(poll_fn(|context| loop {
            match invocation.poll_next_with_context(context) {
                Ok(InvocationPoll::Pending) => return Poll::Pending,
                Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Event(_))))) => continue,
                Ok(InvocationPoll::Ready(Some(Ok(InvocationItem::Complete(value))))) => {
                    return Poll::Ready(value);
                }
                Ok(InvocationPoll::Ready(Some(Err(error)))) => {
                    panic!("stream invocation: {error:?}")
                }
                Ok(InvocationPoll::Ready(None)) => {
                    panic!("stream invocation ended without a result")
                }
                Err(error) => panic!("stream invocation: {error:?}"),
            }
        }))
    }

    fn bytes(values: &[u8]) -> VmValue {
        VmValue::Array(
            values
                .iter()
                .map(|byte| VmValue::Int(i64::from(*byte)))
                .collect::<Vec<_>>()
                .into(),
        )
    }

    fn begin(&mut self, expected_length: i64) -> VmValue {
        let callable = self.begin_fn.clone();
        self.call(&callable, vec![VmValue::Int(expected_length)])
    }

    fn step(&mut self, step: VmValue) -> (VmValue, Value) {
        let render = self.render_fn.clone();
        let rendered = self.call(&render, vec![step.clone()]);
        let VmValue::String(text) = rendered else {
            panic!("stream_step_json did not return a string");
        };
        let report: Value =
            serde_json::from_str(text.as_str()).expect("stream step report is JSON");
        let state_callable = self.state_fn.clone();
        let state = self.call(&state_callable, vec![step]);
        (state, report)
    }

    fn chunk(&mut self, state: &VmValue, bytes: &[u8], returned: &str) -> (VmValue, Value) {
        let callable = self.chunk_fn.clone();
        let step = self.call(
            &callable,
            vec![
                state.clone(),
                Self::bytes(bytes),
                VmValue::string(returned.to_string()),
            ],
        );
        self.step(step)
    }

    fn end(&mut self, state: &VmValue) -> (VmValue, Value) {
        let callable = self.end_fn.clone();
        let step = self.call(&callable, vec![state.clone()]);
        self.step(step)
    }

    fn error(
        &mut self,
        state: &VmValue,
        kind: &str,
        bytes: &[u8],
        returned: &str,
    ) -> (VmValue, Value) {
        let callable = self.error_fn.clone();
        let step = self.call(
            &callable,
            vec![
                state.clone(),
                VmValue::string(kind.to_string()),
                Self::bytes(bytes),
                VmValue::string(returned.to_string()),
            ],
        );
        self.step(step)
    }
}

#[test]
fn download_stream_transitions_advance_one_bounded_chunk_per_call() {
    let mut streams = StreamVm::new();
    let started = streams.begin(4);

    let (after_first, first) = streams.chunk(&started, &[65, 66], "chunk-A");
    assert_eq!(
        first["decisions"],
        json!(["download:chunk-callback-before-yield"])
    );
    assert_eq!(
        first["events"],
        json!([
            {"kind": "callback", "bytes": [65, 66], "callback_return": "chunk-A", "bytes_read": 2, "order": 1},
            {"kind": "yield", "bytes": [65, 66], "callback_return": "", "bytes_read": 2, "order": 2}
        ])
    );
    assert_eq!(first["state"]["bytes_read"], json!(2));
    assert_eq!(first["state"]["next_order"], json!(3));
    assert_eq!(first["state"]["terminal"], json!(false));
    let state_fields = first["state"].as_object().expect("state object");
    assert_eq!(
        state_fields.len(),
        8,
        "stream state must stay scalar-only: {state_fields:?}"
    );
    assert!(state_fields.get("events").is_none());
    assert!(state_fields.get("bytes").is_none());

    let (after_empty, empty) = streams.chunk(&after_first, &[], "chunk-empty");
    assert_eq!(empty["events"][0]["bytes"], json!([]));
    assert_eq!(empty["events"][1]["bytes"], json!([]));
    assert_eq!(empty["events"][0]["order"], json!(3));
    assert_eq!(empty["events"][1]["order"], json!(4));
    assert_eq!(empty["events"][1]["bytes_read"], json!(2));
    assert_eq!(empty["state"]["bytes_read"], json!(2));
    assert_eq!(empty["state"]["next_order"], json!(5));

    let (after_second, second) = streams.chunk(&after_empty, &[67, 68], "chunk-B");
    assert_eq!(second["events"][0]["order"], json!(5));
    assert_eq!(second["events"][1]["order"], json!(6));
    assert_eq!(second["events"][1]["bytes"], json!([67, 68]));
    assert_eq!(second["state"]["bytes_read"], json!(4));
    assert_eq!(second["state"]["next_order"], json!(7));

    let (finished, completed) = streams.end(&after_second);
    assert_eq!(
        completed["events"],
        json!([{"kind": "complete", "bytes_read": 4, "order": 7}])
    );
    assert_eq!(completed["decisions"], json!(["download:stream-complete"]));
    assert_eq!(completed["state"]["finished"], json!(true));
    assert_eq!(completed["state"]["terminal"], json!(true));

    let (_, late) = streams.chunk(&finished, &[9], "late");
    assert_eq!(late["events"], json!([]));
    assert_eq!(late["decisions"], json!(["download:chunk-after-terminal"]));
    assert_eq!(late["state"]["bytes_read"], json!(4));
}

#[test]
fn download_stream_transitions_abort_connection_and_callback_errors() {
    let mut streams = StreamVm::new();

    let started = streams.begin(4);
    let (aborted, failed) = streams.error(&started, "connection-error", &[], "unused");
    assert_eq!(
        failed["events"],
        json!([{"kind": "connection-error", "bytes": [], "callback_return": "unused", "bytes_read": 0, "order": 1}])
    );
    assert_eq!(failed["decisions"], json!(["download:connection-error"]));
    assert_eq!(failed["state"]["aborted"], json!(true));
    assert_eq!(failed["state"]["failure_code"], json!(1012));
    assert_eq!(
        failed["state"]["failure_reason"],
        json!("download:connection-error-aborted")
    );
    let (_, after_end) = streams.end(&aborted);
    assert_eq!(after_end["events"], json!([]));
    assert_eq!(
        after_end["decisions"],
        json!(["download:end-after-terminal"])
    );

    let started = streams.begin(4);
    let (callback_aborted, callback_failed) =
        streams.error(&started, "callback-connection-error", &[1, 2], "callback-A");
    assert_eq!(
        callback_failed["events"],
        json!([
            {"kind": "callback", "bytes": [1, 2], "callback_return": "callback-A", "bytes_read": 2, "order": 1},
            {"kind": "connection-error", "bytes": [], "callback_return": "callback-A", "bytes_read": 2, "order": 2}
        ])
    );
    assert_eq!(callback_failed["state"]["bytes_read"], json!(2));
    assert_eq!(callback_failed["state"]["aborted"], json!(true));
    assert_eq!(callback_failed["state"]["failure_code"], json!(1012));
    assert_eq!(
        callback_failed["state"]["failure_reason"],
        json!("download:callback-connection-error-aborted")
    );
    assert_eq!(
        callback_failed["decisions"],
        json!(["download:callback-connection-error"])
    );

    let (_, after_error) = streams.error(&callback_aborted, "connection-error", &[], "unused");
    assert_eq!(after_error["events"], json!([]));
    assert_eq!(
        after_error["decisions"],
        json!(["download:error-after-terminal"])
    );
}

#[test]
fn download_stream_transitions_abort_short_and_long_completion() {
    let mut streams = StreamVm::new();

    let started = streams.begin(4);
    let (short, _) = streams.chunk(&started, &[1, 2], "short");
    let (_, short_end) = streams.end(&short);
    assert_eq!(short_end["events"], json!([]));
    assert_eq!(
        short_end["decisions"],
        json!(["download:final-length-mismatch"])
    );
    assert_eq!(short_end["state"]["aborted"], json!(true));
    assert_eq!(short_end["state"]["failure_code"], json!(1012));
    assert_eq!(
        short_end["state"]["failure_reason"],
        json!("download:final-length-mismatch-aborted")
    );
    assert_eq!(short_end["state"]["bytes_read"], json!(2));

    let started = streams.begin(2);
    let (long, _) = streams.chunk(&started, &[1, 2, 3], "long");
    let (_, long_end) = streams.end(&long);
    assert_eq!(long_end["events"], json!([]));
    assert_eq!(
        long_end["decisions"],
        json!(["download:final-length-mismatch"])
    );
    assert_eq!(long_end["state"]["aborted"], json!(true));
    assert_eq!(
        long_end["state"]["failure_reason"],
        json!("download:final-length-mismatch-aborted")
    );
    assert_eq!(long_end["state"]["bytes_read"], json!(3));
}

#[test]
fn download_stream_transitions_keep_unrelated_streams_independent() {
    let mut streams = StreamVm::new();
    let first = streams.begin(2);
    let second = streams.begin(2);

    let (first_after, first_chunk) = streams.chunk(&first, &[1, 2], "first");
    let (second_after, second_chunk) = streams.chunk(&second, &[3], "second");
    assert_eq!(first_chunk["state"]["bytes_read"], json!(2));
    assert_eq!(second_chunk["state"]["bytes_read"], json!(1));
    assert_eq!(second_chunk["state"]["expected_length"], json!(2));

    let (_, first_end) = streams.end(&first_after);
    let (_, second_end) = streams.end(&second_after);
    assert_eq!(
        first_end["events"],
        json!([{"kind": "complete", "bytes_read": 2, "order": 3}])
    );
    assert_eq!(first_end["state"]["finished"], json!(true));
    assert_eq!(second_end["events"], json!([]));
    assert_eq!(second_end["state"]["aborted"], json!(true));
    assert_eq!(
        second_end["state"]["failure_reason"],
        json!("download:final-length-mismatch-aborted")
    );
}
