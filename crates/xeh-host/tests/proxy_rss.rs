use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::Duration;

use vm::HttpConfig;
use xeh_host::{ControlOptions, ProxyServices, TaskControl};

const AUTH: &str = "{\"allowed_schemes\":[\"http\"],\"allowed_hosts\":[\"127.0.0.1\"],\"allowed_ports\":[9],\"allow_private_ips\":true}";

fn run(request: Value) -> Value {
    let text = serde_json::to_string(&request).expect("request");
    let raw = xeh_host::run_proxy_scenario(std::path::Path::new("."), &text, AUTH)
        .expect("proxy scenario");
    serde_json::from_str(&raw).expect("report")
}

#[test]
fn pool_sequence_runs_on_the_retained_store_owner() {
    let report = run(json!({
        "operation": "pool",
        "steps": [
            {"op": "proxy_add", "address": "http://a.invalid:1"},
            {"op": "proxy_add", "address": "http://b.invalid:2"},
            {"op": "proxy_select", "now": 100.0},
            {"op": "proxy_not_good", "address": "http://a.invalid:1", "weight": 5, "expire": 0.0, "now": 100.0},
            {"op": "proxy_select", "now": 200.0},
            {"op": "proxy_has"}
        ]
    }));
    println!("{}", serde_json::to_string_pretty(&report).expect("pretty"));
    let steps = report["steps"].as_array().expect("steps");
    assert_eq!(steps.len(), 6);
    assert_eq!(steps[0]["proxy"]["order"], json!(["http://a.invalid:1"]));
    let selected: Value =
        serde_json::from_str(steps[2]["value"].as_str().expect("value")).expect("selected JSON");
    assert_eq!(selected["address"], json!("http://a.invalid:1"));
    assert_eq!(selected["route"]["kind"], json!("http"));
    assert_eq!(selected["route"]["host"], json!("a.invalid"));
    assert_eq!(selected["route"]["port"], json!(1));
    assert_eq!(selected["route"]["trusted_remote_dns"], json!(true));
    assert_eq!(
        steps[3]["proxy"]["disabled"]["http://a.invalid:1"],
        json!(100.0)
    );
    assert_eq!(steps[4]["error"], json!("RuntimeError"));
    assert_eq!(steps[4]["proxy"]["disabled"], json!({}));
    assert_eq!(
        steps[5]["proxy"]["order"],
        json!(["http://a.invalid:1", "http://b.invalid:2"])
    );
}

#[test]
fn policy_matches_source_predicates() {
    let report = run(json!({
        "operation": "policy",
        "config": {"proxy_image": false, "proxy_image_only": true},
        "urls": ["https://e-hentai.org/g/1/x/", "https://example.invalid/x", ""]
    }));
    println!("{}", serde_json::to_string_pretty(&report).expect("pretty"));
    let rows = report["urls"].as_array().expect("rows");
    assert_eq!(rows[0]["image_only"], json!(false));
    assert_eq!(rows[1]["image_only"], json!(true));
    assert_eq!(rows[2]["image_only"], json!(true));
}

fn lease_root(name: &str) -> PathBuf {
    let base = std::env::var("XEH_LEASE_TMP").expect("XEH_LEASE_TMP");
    let root = PathBuf::from(base).join(name);
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("fixture root");
    root
}

fn http_config(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(60),
        stream_idle_timeout: Duration::from_secs(30),
        max_stream_duration: Duration::from_secs(60),
        ..HttpConfig::default()
    }
}

fn invoke_owner(control: &mut TaskControl, request: Value) -> Value {
    let raw = control
        .invoke(&serde_json::to_string(&request).expect("request"))
        .expect("invoke");
    serde_json::from_str(&raw).expect("report")
}

fn value_of(report: &Value) -> Value {
    serde_json::from_str(report["value"].as_str().expect("value")).expect("value json")
}

fn retained_owner(name: &str, port: u16) -> TaskControl {
    let root = lease_root(name);
    TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root,
            http: http_config(port),
            grants: Vec::new(),
        },
    )
    .expect("owner")
}

#[test]
fn glyph_mkurl_and_cookie_flow_compiles_and_runs() {
    let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let record = seen.clone();
    let server = serve(move |request| {
        *record.lock().expect("seen") = request;
        http(200, "Content-Type: text/html\r\n", "ok")
    });
    let mut owner = retained_owner("proxy-rss-glype-mkurl", server.port);
    let address = format!("http://127.0.0.1:{}/browse.php?u=x&b=4", server.port);
    let target = format!("http://127.0.0.1:{}/page", server.port);
    assert_eq!(
        invoke_owner(&mut owner, json!({"op": "proxy_add", "address": address}))["ok"],
        json!(true)
    );
    let report = invoke_owner(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": target,
            "method": "GET",
            "header_names": ["Cookie", "User-Agent"],
            "header_values": ["nw=1", "xeh-fixture/1"],
            "proxy_image": true,
            "explicit_allow_redirects": false,
            "now": 10.0
        }),
    );
    let value = value_of(&report);
    assert_eq!(report["ok"], json!(true), "{report}");
    assert_eq!(value["status"], json!(200), "{value}");
    assert_eq!(value["error"], json!(""), "{value}");
    let wire = seen.lock().expect("seen").clone();
    assert!(wire.contains("browse.php"), "{wire}");
    assert!(
        wire.contains(&format!("c[127.0.0.1:{}][/][nw]=1", server.port)),
        "{wire}"
    );
}

fn run_port(request: Value, port: u16) -> Value {
    let auth = format!(
        "{{\"allowed_schemes\":[\"http\"],\"allowed_hosts\":[\"127.0.0.1\"],\"allowed_ports\":[{port}],\"allow_private_ips\":true}}"
    );
    let text = serde_json::to_string(&request).expect("request");
    let raw = xeh_host::run_proxy_scenario(std::path::Path::new("."), &text, &auth)
        .expect("proxy scenario");
    serde_json::from_str(&raw).expect("report")
}

struct FixtureServer {
    port: u16,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for FixtureServer {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn serve(handler: impl Fn(String) -> Vec<u8> + Send + 'static) -> FixtureServer {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    listener.set_nonblocking(true).expect("nonblocking");
    let stop = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&stop);
    let thread = std::thread::spawn(move || {
        while !flag.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(2)));
                    let mut buf = [0u8; 2048];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buf[..n]).to_string();
                    let _ = stream.write_all(&handler(request));
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(_) => return,
            }
        }
    });
    FixtureServer {
        port,
        stop,
        thread: Some(thread),
    }
}

fn http(status: u16, extra: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
        body.len()
    )
    .into_bytes()
}

#[test]
fn glype_session_survives_a_second_owner_and_stale_publish_misses() {
    let address = "http://127.0.0.1:9/browse.php?u=x&b=4";
    let kept = run(json!({
        "operation": "pool",
        "steps": [
            {"op": "proxy_add", "address": address},
            {"op": "proxy_glyph_publish", "address": address, "epoch": 1, "value": "kept"},
            {"op": "proxy_glyph_read", "address": address},
            {"op": "proxy_add", "address": address},
            {"op": "proxy_glyph_publish", "address": address, "epoch": 1, "value": "stale"},
            {"op": "proxy_glyph_read", "address": address}
        ]
    }));
    assert_eq!(kept["steps"][1]["value"], json!("{\"updated\":true}"));
    assert_eq!(
        kept["steps"][2]["value"],
        json!("{\"s\":\"kept\",\"epoch\":1}")
    );
    assert_eq!(kept["steps"][4]["value"], json!("{\"updated\":false}"));
    assert_eq!(kept["steps"][5]["value"], json!("{\"s\":\"\",\"epoch\":2}"));
    let other_owner = run(json!({
        "operation": "pool",
        "steps": [
            {"op": "proxy_add", "address": address},
            {"op": "proxy_glyph_read", "address": address}
        ]
    }));
    assert_eq!(
        other_owner["steps"][1]["value"],
        json!("{\"s\":\"\",\"epoch\":1}")
    );
}

#[test]
fn glype_raw_403_is_status_not_connection_error() {
    for status in [403u16, 429, 500] {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let record = seen.clone();
        let server = serve(move |request| {
            *record.lock().expect("seen") = request;
            http(status, "Content-Type: text/html\r\n", "denied")
        });
        let mut owner = retained_owner(&format!("proxy-rss-glype-{status}"), server.port);
        let address = format!("http://127.0.0.1:{}/browse.php?u=x&b=4", server.port);
        let target = format!("http://127.0.0.1:{}/page", server.port);
        assert_eq!(
            invoke_owner(&mut owner, json!({"op": "proxy_add", "address": address}))["ok"],
            json!(true)
        );
        let report = invoke_owner(
            &mut owner,
            json!({
                "op": "proxy_request",
                "url": target,
                "method": "GET",
                "header_names": ["User-Agent"],
                "header_values": ["xeh-fixture/1"],
                "proxy_image": true,
                "explicit_allow_redirects": false,
                "now": 10.0
            }),
        );
        let value = value_of(&report);
        assert_eq!(report["ok"], json!(true), "{report}");
        assert_eq!(value["status"], json!(status), "{value}");
        assert_eq!(value["error"], json!(""), "{value}");
        assert!(
            seen.lock().expect("seen").contains("browse.php"),
            "raw status must still travel through Glype mkurl"
        );
    }
}

#[test]
fn worker_redirect_real_url_is_the_previous_hop() {
    let port_slot = std::sync::Arc::new(std::sync::atomic::AtomicU16::new(0));
    let slot = port_slot.clone();
    let server = serve(move |request| {
        let port = slot.load(std::sync::atomic::Ordering::SeqCst);
        if request.contains(" /done ") {
            http(200, "Content-Type: text/plain\r\n", "done")
        } else {
            http(
                302,
                &format!("Location: http://127.0.0.1:{port}/done\r\n"),
                "",
            )
        }
    });
    port_slot.store(server.port, std::sync::atomic::Ordering::SeqCst);
    let start = format!("http://127.0.0.1:{}/start", server.port);
    let report = run_port(
        json!({
            "operation": "worker",
            "url": start,
            "explicit_allow_redirects": false
        }),
        server.port,
    );
    assert_eq!(report["status"], json!(200));
    assert_eq!(report["real_url"], json!(start));
    assert_eq!(
        report["final_url"],
        json!(format!("http://127.0.0.1:{}/done", server.port))
    );
    assert_eq!(report["hops"][1]["proxied"], json!(false));
}

#[test]
fn explicit_allow_redirects_raises_before_the_agreement_request() {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    let server = serve(move |_| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        http(
            200,
            "Content-Type: text/html\r\n",
            "<input type=\"hidden\" name=\"action\" value=\"sslagree\">",
        )
    });
    let report = run_port(
        json!({
            "operation": "worker",
            "url": format!("http://127.0.0.1:{}/warn", server.port),
            "explicit_allow_redirects": true,
            "agreement_url": format!("http://127.0.0.1:{}/agree", server.port)
        }),
        server.port,
    );
    assert_eq!(report["exception"], json!("TypeError"));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

fn run_at(root: &std::path::Path, request: Value, port: u16) -> Value {
    let auth = format!(
        "{{\"allowed_schemes\":[\"http\"],\"allowed_hosts\":[\"127.0.0.1\"],\"allowed_ports\":[{port}],\"allow_private_ips\":true}}"
    );
    let text = serde_json::to_string(&request).expect("request");
    let raw = xeh_host::run_proxy_scenario(root, &text, &auth).expect("proxy scenario");
    serde_json::from_str(&raw).expect("report")
}

#[test]
fn worker_reselects_proxy_on_the_image_hop_without_fetching_it() {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = hits.clone();
    let server = serve(move |_| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        http(302, "Location: http://e-hentai.org/image.jpg\r\n", "")
    });
    let report = run_port(
        json!({
            "operation": "worker",
            "url": format!("http://127.0.0.1:{}/page", server.port),
            "explicit_allow_redirects": false
        }),
        server.port,
    );
    assert_eq!(report["hops"][0]["proxied"], json!(false));
    assert_eq!(report["hops"][1]["proxied"], json!(true));
    assert_eq!(report["hops"][1]["error_kind"], json!("policy_denied"));
    assert_eq!(hits.load(std::sync::atomic::Ordering::SeqCst), 1);
}

#[test]
fn post_and_large_image_stay_native() {
    let hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let posted = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let counted = hits.clone();
    let saw = posted.clone();
    let server = serve(move |request| {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if request.starts_with("POST ") {
            saw.store(request.contains("a=1"), std::sync::atomic::Ordering::SeqCst);
            http(200, "Content-Type: text/plain\r\n", "posted")
        } else {
            let len = 5 * 1024 * 1024;
            let mut response = format!(
                "HTTP/1.1 200 X\r\nContent-Length: {len}\r\nContent-Type: image/jpeg\r\nConnection: close\r\n\r\n"
            )
            .into_bytes();
            response.extend(std::iter::repeat_n(0xABu8, len));
            response
        }
    });
    let root = std::path::Path::new(
        "/mnt/TEMP/workspace/xehentai-rss/tmp/xehentai-20261029-main-13b130ba",
    );
    let post = run_at(
        root,
        json!({
            "operation": "transfer",
            "url": format!("http://127.0.0.1:{}/form", server.port),
            "method": "POST",
            "data": "a=1",
            "stage": "post-body.bin"
        }),
        server.port,
    );
    assert_eq!(post["status"], json!(200));
    assert!(posted.load(std::sync::atomic::Ordering::SeqCst));
    let _ = std::fs::remove_file(root.join("post-body.bin"));
    let staged = root.join("large-image.bin");
    let _ = std::fs::remove_file(&staged);
    let report = run_at(
        root,
        json!({
            "operation": "transfer",
            "url": format!("http://127.0.0.1:{}/image.jpg", server.port),
            "method": "GET",
            "stage": "large-image.bin"
        }),
        server.port,
    );
    assert_eq!(report["status"], json!(200));
    assert_eq!(report["byte_length"], json!(5 * 1024 * 1024));
    assert_eq!(report["body_text"], json!(""));
    assert_eq!(report["staged_bytes"], json!(5 * 1024 * 1024));
    assert_eq!(
        std::fs::metadata(&staged).expect("staged").len(),
        5 * 1024 * 1024
    );
    let _ = std::fs::remove_file(&staged);
}
