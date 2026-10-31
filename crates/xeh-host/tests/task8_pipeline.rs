//! One TaskControl drives pool selection, Glype session retention, and native
//! body staging. The staged file is compared with the bytes this test supplied
//! to the fixture server, not with the spool the host wrote.
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use vm::HttpConfig;

use xeh_host::{ControlOptions, ProxyServices, TaskControl};

fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|index| (index as u8).wrapping_add(0xA5))
        .collect()
}

fn http_bytes(status: u16, extra: &str, body: &[u8]) -> Vec<u8> {
    let mut out = format!(
        "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n",
        body.len()
    )
    .into_bytes();
    out.extend_from_slice(body);
    out
}

fn serve(agree: Arc<AtomicUsize>, saw_session: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let small = Arc::new(pattern(100 * 1024));
    let large = Arc::new(pattern(4 * 1024 * 1024 + 1024));
    std::thread::spawn(move || loop {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
        let mut buf = [0u8; 8192];
        let n = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        let has_session = request.contains("s=retained-s");
        if has_session {
            saw_session.fetch_add(1, Ordering::SeqCst);
        }
        let response = if request.contains("action=sslagree") {
            agree.fetch_add(1, Ordering::SeqCst);
            http_bytes(200, "Set-Cookie: s=retained-s\r\n", b"ok")
        } else if !has_session {
            http_bytes(
                200,
                "Content-Type: text/html\r\n",
                b"<input type=\"hidden\" name=\"action\" value=\"sslagree\">",
            )
        } else if request.contains("large.bin") {
            http_bytes(200, "Content-Type: image/jpeg\r\n", &large)
        } else {
            http_bytes(200, "Content-Type: image/jpeg\r\n", &small)
        };
        let _ = stream.write_all(&response);
    });
    port
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

fn invoke(control: &mut TaskControl, request: Value) -> Value {
    let raw = control
        .invoke(&serde_json::to_string(&request).expect("request"))
        .expect("invoke");
    serde_json::from_str(&raw).expect("report")
}

fn value_of(report: &Value) -> Value {
    serde_json::from_str(report["value"].as_str().expect("value")).expect("value json")
}

fn assert_exact(path: &std::path::Path, expected: &[u8]) {
    let got = std::fs::read(path).expect("staged bytes");
    assert_eq!(got.len(), expected.len());
    assert_eq!(got, expected);
}

fn spool_count(root: &std::path::Path) -> usize {
    std::fs::read_dir(root)
        .expect("root")
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".xeh-body-")
        })
        .count()
}

#[test]
fn one_owner_keeps_glype_session_and_stages_exact_bytes() {
    let agree = Arc::new(AtomicUsize::new(0));
    let saw_session = Arc::new(AtomicUsize::new(0));
    let port = serve(agree.clone(), saw_session.clone());
    let root = lease_root("task8-r2-glype");
    let mut owner = TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root: root.clone(),
            http: http_config(port),
            grants: Vec::new(),
        },
    )
    .expect("owner");
    let address = format!("http://127.0.0.1:{port}/browse.php?u=x&b=4");
    let added = invoke(&mut owner, json!({"op": "proxy_add", "address": address}));
    assert_eq!(added["ok"], json!(true), "{added}");

    let small_url = format!("http://127.0.0.1:{port}/small.bin");
    let first = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": small_url,
            "method": "GET",
            "stage": "first.bin",
            "header_names": ["Cookie", "User-Agent"],
            "header_values": ["nw=1", "xeh-fixture/1"],
            "proxy_image": true,
            "explicit_allow_redirects": false,
            "now": 10.0
        }),
    );
    // The first call is the warning plus agreement. The retry in that same
    // call already carries the published cookie, so the staged file is the
    // image and the agreement happened once.
    let first_value = value_of(&first);
    assert_eq!(first["ok"], json!(true), "{first}");
    assert_eq!(first_value["glyph_s"], json!("retained-s"), "{first_value}");
    assert_eq!(agree.load(Ordering::SeqCst), 1);
    assert_exact(&root.join("first.bin"), &pattern(100 * 1024));

    let before = owner.resource_count();
    let second = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": small_url,
            "method": "GET",
            "stage": "second.bin",
            "header_names": ["Cookie", "User-Agent"],
            "header_values": ["nw=1", "xeh-fixture/1"],
            "proxy_image": true,
            "explicit_allow_redirects": false,
            "now": 20.0
        }),
    );
    let second_value = value_of(&second);
    assert_eq!(second["ok"], json!(true), "{second}");
    assert!(
        second_value["sent_cookie"]
            .as_str()
            .unwrap()
            .contains("s=retained-s"),
        "{second_value}"
    );
    assert_eq!(agree.load(Ordering::SeqCst), 1, "second call must reuse s");
    assert!(saw_session.load(Ordering::SeqCst) >= 2);
    assert_exact(&root.join("second.bin"), &pattern(100 * 1024));

    let large_url = format!("http://127.0.0.1:{port}/large.bin");
    let third = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": large_url,
            "method": "GET",
            "stage": "large.bin",
            "header_names": ["Cookie", "User-Agent"],
            "header_values": ["nw=1", "xeh-fixture/1"],
            "proxy_image": true,
            "explicit_allow_redirects": false,
            "now": 30.0
        }),
    );
    assert_eq!(third["ok"], json!(true), "{third}");
    assert_eq!(
        value_of(&third)["byte_length"],
        json!(4 * 1024 * 1024 + 1024)
    );
    assert_exact(&root.join("large.bin"), &pattern(4 * 1024 * 1024 + 1024));
    assert_eq!(owner.resource_count(), before);
    assert_eq!(spool_count(&root), 0);

    let other_root = lease_root("task8-r2-other");
    let mut other = TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root: other_root,
            http: http_config(port),
            grants: Vec::new(),
        },
    )
    .expect("second owner");
    let empty = invoke(&mut other, json!({"op": "proxy_select", "now": 1.0}));
    assert_eq!(empty["ok"], json!(false));
    assert_eq!(empty["error"], json!("PoolException"));

    let readded = invoke(&mut owner, json!({"op": "proxy_add", "address": address}));
    assert_eq!(readded["ok"], json!(true));
    let read = invoke(
        &mut owner,
        json!({"op": "proxy_glyph_read", "address": address}),
    );
    let read_value = value_of(&read);
    assert_eq!(read_value["s"], json!(""));
    assert_eq!(read_value["epoch"], json!(2));
    let stale = invoke(
        &mut owner,
        json!({
            "op": "proxy_glyph_publish",
            "address": address,
            "epoch": 1,
            "value": "stale"
        }),
    );
    assert_eq!(value_of(&stale)["updated"], json!(false));
    let after = invoke(
        &mut owner,
        json!({"op": "proxy_glyph_read", "address": address}),
    );
    assert_eq!(value_of(&after)["s"], json!(""));
}

fn text_server() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    std::thread::spawn(move || {
        let big = "a".repeat(270_000);
        loop {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let (kind, body) = if request.contains("bad-utf8") {
                ("text/html", vec![0xff, 0xfe])
            } else if request.contains(" /loop ") || request.contains(" /hop ") {
                // The next hop is a different path, so a limit result that
                // records the previous hop cannot accidentally equal the
                // original URL.
                let location = format!("Location: http://127.0.0.1:{port}/hop\r\n");
                let response = http_bytes(302, &location, b"");
                let _ = stream.write_all(&response);
                continue;
            } else {
                ("text/html", big.as_bytes().to_vec())
            };
            let extra = format!("Content-Type: {kind}\r\n");
            let _ = stream.write_all(&http_bytes(200, &extra, &body));
        }
    });
    port
}

#[test]
fn text_budget_is_not_a_decode_error_and_invalid_utf8_stays_separate() {
    let port = text_server();
    let root = lease_root("task8-r2-text");
    let mut owner = TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root: root.clone(),
            http: http_config(port),
            grants: Vec::new(),
        },
    )
    .expect("owner");
    let url = format!("http://127.0.0.1:{port}/big.txt");
    let ok = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": url,
            "method": "GET",
            "stage": "big.txt",
            "text_budget": 300_000,
            "proxy_image": false,
            "now": 1.0
        }),
    );
    assert_eq!(ok["ok"], json!(true), "{ok}");
    assert_eq!(value_of(&ok)["byte_length"], json!(270_000));
    assert_exact(&root.join("big.txt"), "a".repeat(270_000).as_bytes());

    let over = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": url,
            "method": "GET",
            "text_budget": 1024,
            "proxy_image": false,
            "now": 2.0
        }),
    );
    assert_eq!(over["ok"], json!(false), "{over}");
    assert_eq!(value_of(&over)["error"], json!("text_budget"));

    let bad = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{port}/bad-utf8"),
            "method": "GET",
            "text_budget": 300_000,
            "proxy_image": false,
            "now": 3.0
        }),
    );
    assert_eq!(bad["ok"], json!(false), "{bad}");
    assert_eq!(value_of(&bad)["error"], json!("invalid_utf8"));
    assert_eq!(spool_count(&root), 0);
}

#[test]
fn explicit_allow_redirects_raises_before_agreement() {
    let agree = Arc::new(AtomicUsize::new(0));
    let saw = Arc::new(AtomicUsize::new(0));
    let port = serve(agree.clone(), saw);
    let root = lease_root("task8-r2-typeerror");
    let mut owner = TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root,
            http: http_config(port),
            grants: Vec::new(),
        },
    )
    .expect("owner");
    let address = format!("http://127.0.0.1:{port}/browse.php?u=x&b=4");
    assert_eq!(
        invoke(&mut owner, json!({"op": "proxy_add", "address": address}))["ok"],
        json!(true)
    );
    let report = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{port}/small.bin"),
            "method": "GET",
            "header_names": ["Cookie"],
            "header_values": ["nw=1"],
            "proxy_image": true,
            "explicit_allow_redirects": true,
            "now": 4.0
        }),
    );
    assert_eq!(report["ok"], json!(false), "{report}");
    assert_eq!(value_of(&report)["error"], json!("TypeError"));
    assert_eq!(agree.load(Ordering::SeqCst), 0);
}

#[test]
fn more_than_thirty_redirects_return_status_600() {
    let port = text_server();
    let root = lease_root("task8-r2-redirect");
    let mut owner = TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root,
            http: http_config(port),
            grants: Vec::new(),
        },
    )
    .expect("owner");
    let url = format!("http://127.0.0.1:{port}/loop");
    let report = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": url,
            "method": "GET",
            "proxy_image": false,
            "now": 5.0
        }),
    );
    let value = value_of(&report);
    assert_eq!(report["ok"], json!(true), "{report}");
    assert_eq!(value["status"], json!(600), "{value}");
    assert_eq!(value["error"], json!(""));
    assert_eq!(value["logical_url"], json!(url), "{value}");
    assert_eq!(value["final_url"], json!(url), "{value}");
    assert_eq!(value["real_url"], json!(url), "{value}");
}

#[test]
fn existing_stage_target_is_preserved_and_failed_exchange_leaves_no_spool() {
    let port = text_server();
    let root = lease_root("task8-r2-stage");
    std::fs::write(root.join("keep.bin"), b"preexisting").expect("seed");
    let mut owner = TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root: root.clone(),
            http: http_config(port),
            grants: Vec::new(),
        },
    )
    .expect("owner");
    let kept = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{port}/big.txt"),
            "method": "GET",
            "stage": "keep.bin",
            "proxy_image": false,
            "now": 1.0
        }),
    );
    assert_eq!(kept["ok"], json!(false), "{kept}");
    assert_eq!(value_of(&kept)["staged_error"], json!("target_exists"));
    assert_eq!(
        std::fs::read(root.join("keep.bin")).expect("kept"),
        b"preexisting"
    );
    assert_eq!(spool_count(&root), 0);

    let closed = TcpListener::bind("127.0.0.1:0").expect("closed listener");
    let closed_port = closed.local_addr().expect("port").port();
    std::thread::spawn(move || {
        for index in 0..2 {
            let Ok((mut stream, _)) = closed.accept() else {
                return;
            };
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buf = [0u8; 1024];
            let _ = stream.read(&mut buf);
            if index == 0 {
                drop(stream);
                continue;
            }
            let _ = stream.write_all(&http_bytes(200, "Content-Type: text/plain\r\n", b"ok"));
        }
    });
    let mut failed_owner = TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root: root.clone(),
            http: http_config(closed_port),
            grants: Vec::new(),
        },
    )
    .expect("failed owner");
    let broken = invoke(
        &mut failed_owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{closed_port}/gone"),
            "method": "GET",
            "proxy_image": false,
            "now": 2.0
        }),
    );
    assert_eq!(broken["ok"], json!(false), "{broken}");
    assert_eq!(spool_count(&root), 0);
    let recovered = invoke(
        &mut failed_owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{closed_port}/back"),
            "method": "GET",
            "stage": "recovered.txt",
            "proxy_image": false,
            "now": 3.0
        }),
    );
    assert_eq!(recovered["ok"], json!(true), "{recovered}");
    assert_exact(&root.join("recovered.txt"), b"ok");
    assert_eq!(spool_count(&root), 0);
}

fn http_config_ports(ports: &[u16]) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: ports.to_vec(),
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(2),
        request_timeout: Duration::from_secs(5),
        stream_idle_timeout: Duration::from_secs(2),
        max_stream_duration: Duration::from_secs(5),
        ..HttpConfig::default()
    }
}

fn owner_for(root: PathBuf, http: HttpConfig) -> TaskControl {
    TaskControl::open_with_proxy(
        &ControlOptions::fixture(&[], 0),
        None,
        ProxyServices {
            root,
            http,
            grants: Vec::new(),
        },
    )
    .expect("owner")
}

fn one_shot(
    status: u16,
    extra: &str,
    body: &[u8],
    seen: Arc<std::sync::Mutex<Vec<String>>>,
) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let extra = extra.to_string();
    let body = body.to_vec();
    std::thread::spawn(move || loop {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0u8; 4096];
        let n = stream.read(&mut buf).unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]).to_string();
        seen.lock().expect("seen").push(request);
        let _ = stream.write_all(&http_bytes(status, &extra, &body));
    });
    port
}

const ERROR_DIV: &str = "<div id=\"error\">upstream</div>";

#[test]
fn glype_500_error_div_is_pool_exception_not_staged_success() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = one_shot(
        500,
        "Content-Type: text/html\r\n",
        ERROR_DIV.as_bytes(),
        seen,
    );
    let root = lease_root("task8-r3-error-div");
    let mut owner = owner_for(root.clone(), http_config(port));
    let address = format!("http://127.0.0.1:{port}/browse.php?u=x&b=4");
    assert_eq!(
        invoke(&mut owner, json!({"op": "proxy_add", "address": address}))["ok"],
        json!(true)
    );
    let report = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{port}/err500"),
            "method": "GET",
            "stage": "err.bin",
            "header_names": ["User-Agent"],
            "header_values": ["xeh-fixture/1"],
            "proxy_image": true,
            "now": 1.0
        }),
    );
    let value = value_of(&report);
    assert_eq!(report["ok"], json!(false), "{report}");
    assert_eq!(value["error"], json!("PoolException"), "{value}");
    assert_eq!(
        value["detail"],
        json!(format!("glype returns: {ERROR_DIV}")),
        "{value}"
    );
    assert!(
        !root.join("err.bin").exists(),
        "error div must not be staged"
    );
}

#[test]
fn unauthorized_logical_target_is_refused_when_glype_endpoint_is_granted() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = one_shot(200, "Content-Type: text/plain\r\n", b"secret", seen.clone());
    let root = lease_root("task8-r3-logical");
    let mut owner = owner_for(root, http_config(port));
    let address = format!("http://127.0.0.1:{port}/browse.php?u=x&b=4");
    assert_eq!(
        invoke(&mut owner, json!({"op": "proxy_add", "address": address}))["ok"],
        json!(true)
    );
    let report = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": "http://evil.example/page",
            "method": "GET",
            "proxy_image": true,
            "now": 1.0
        }),
    );
    let value = value_of(&report);
    assert_eq!(report["ok"], json!(false), "{report}");
    assert_eq!(value["error"], json!("policy_denied"), "{value}");
    assert!(
        seen.lock().expect("seen").is_empty(),
        "rewriting the logical URL must not reach the granted Glype endpoint"
    );
}

#[test]
fn proxy_endpoint_outside_trusted_grant_fails_before_connect() {
    let accepts = Arc::new(AtomicUsize::new(0));
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let proxy_port = listener.local_addr().expect("port").port();
    let seen = accepts.clone();
    std::thread::spawn(move || loop {
        let Ok((stream, _)) = listener.accept() else {
            return;
        };
        seen.fetch_add(1, Ordering::SeqCst);
        drop(stream);
    });
    let http_port = one_shot(
        200,
        "Content-Type: text/plain\r\n",
        b"direct",
        Arc::new(std::sync::Mutex::new(Vec::new())),
    );
    let root = lease_root("task8-r3-grant");
    let mut owner = owner_for(root, http_config_ports(&[http_port]));
    let address = format!("socks5://127.0.0.1:{proxy_port}");
    assert_eq!(
        invoke(&mut owner, json!({"op": "proxy_add", "address": address}))["ok"],
        json!(true)
    );
    let report = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{http_port}/a.jpg"),
            "method": "GET",
            "proxy_image": true,
            "now": 1.0
        }),
    );
    let value = value_of(&report);
    assert_eq!(report["ok"], json!(false), "{report}");
    assert_eq!(value["error"], json!("proxy_not_granted"), "{value}");
    assert_ne!(value["error"], json!("InvalidSchema"));
    assert_eq!(accepts.load(Ordering::SeqCst), 0);
}

#[test]
fn cross_origin_redirect_does_not_forward_cookie_or_authorization() {
    let second_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let second = one_shot(
        200,
        "Content-Type: text/plain\r\n",
        b"landed",
        second_seen.clone(),
    );
    let first_seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let location = format!("Location: http://127.0.0.1:{second}/landed\r\n");
    let first = one_shot(302, &location, b"", first_seen);
    let root = lease_root("task8-r3-redirect-headers");
    let mut owner = owner_for(root, http_config_ports(&[first, second]));
    let report = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{first}/start"),
            "method": "GET",
            "header_names": ["Cookie", "Authorization", "User-Agent"],
            "header_values": ["session=keep", "Bearer secret", "xeh-fixture/1"],
            "proxy_image": false,
            "now": 1.0
        }),
    );
    assert_eq!(report["ok"], json!(true), "{report}");
    let landed = second_seen.lock().expect("seen");
    assert_eq!(landed.len(), 1, "{landed:?}");
    let request = landed[0].to_ascii_lowercase();
    assert!(!request.contains("cookie:"), "{request}");
    assert!(!request.contains("authorization:"), "{request}");
    assert!(request.contains("user-agent:"), "{request}");
}

#[test]
fn missing_location_stays_original_3xx() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = one_shot(302, "Content-Type: text/plain\r\n", b"", seen);
    let root = lease_root("task8-r3-missing-location");
    let mut owner = owner_for(root, http_config(port));
    let url = format!("http://127.0.0.1:{port}/stay");
    let report = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": url,
            "method": "GET",
            "proxy_image": false,
            "now": 1.0
        }),
    );
    let value = value_of(&report);
    assert_eq!(report["ok"], json!(true), "{report}");
    assert_eq!(value["status"], json!(302), "{value}");
    assert_eq!(value["final_url"], json!(url), "{value}");
    assert_eq!(value["real_url"], json!(url), "{value}");
}

#[test]
fn header_rejection_does_not_change_proxy_score_or_report_invalid_schema() {
    let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
    let port = one_shot(200, "Content-Type: text/plain\r\n", b"ok", seen.clone());
    let root = lease_root("task8-r3-header");
    let mut owner = owner_for(root, http_config(port));
    let address = format!("http://127.0.0.1:{port}/browse.php?u=x&b=4");
    assert_eq!(
        invoke(&mut owner, json!({"op": "proxy_add", "address": address}))["ok"],
        json!(true)
    );
    let report = invoke(
        &mut owner,
        json!({
            "op": "proxy_request",
            "url": format!("http://127.0.0.1:{port}/page"),
            "method": "GET",
            "header_names": ["Connection"],
            "header_values": ["keep-alive"],
            "proxy_image": true,
            "now": 1.0
        }),
    );
    let value = value_of(&report);
    assert_eq!(report["ok"], json!(false), "{report}");
    assert_eq!(value["error"], json!("header_rejected"), "{value}");
    assert_ne!(value["error"], json!("InvalidSchema"));
    assert_eq!(
        report["proxy"]["counts"][&address],
        json!([0, 0]),
        "{report}"
    );
    assert!(
        seen.lock().expect("seen").is_empty(),
        "header rejection connects"
    );
}

#[test]
fn dropped_exchange_removes_only_its_partial_spool() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    std::thread::spawn(move || loop {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut buf = [0u8; 1024];
        let _ = stream.read(&mut buf);
        let header = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nContent-Type: application/octet-stream\r\n\r\n";
        let _ = stream.write_all(header);
        let _ = stream.write_all(b"x");
        std::thread::sleep(Duration::from_secs(30));
    });
    let root = lease_root("task8-r3-cancel");
    std::fs::write(root.join("artifact.bin"), b"keep-me").expect("artifact");
    std::fs::write(root.join(".xeh-body-1.spool"), b"preexisting").expect("old spool name");
    let mut owner = owner_for(root.clone(), http_config(port));
    let cancel = owner.proxy_cancel().expect("cancel handle");
    let report = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            invoke(
                &mut owner,
                json!({
                    "op": "proxy_request",
                    "url": format!("http://127.0.0.1:{port}/slow.bin"),
                    "method": "GET",
                    "stage": "slow.bin",
                    "proxy_image": false,
                    "now": 1.0
                }),
            )
        });
        let spool_dir = root.join(".xeh-spool");
        let started = std::time::Instant::now();
        while started.elapsed() < Duration::from_secs(5) && !spool_dir.exists() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            spool_dir.exists(),
            "exchange did not create its reserved spool"
        );
        cancel.cancel();
        handle.join().expect("invoke")
    });
    assert_eq!(report["ok"], json!(false), "{report}");
    assert_eq!(
        std::fs::read(root.join("artifact.bin")).expect("artifact"),
        b"keep-me"
    );
    assert_eq!(
        std::fs::read(root.join(".xeh-body-1.spool")).expect("preexisting"),
        b"preexisting"
    );
    assert_eq!(spool_count(&root), 1, "preexisting spool name changed");
    let reserved = root.join(".xeh-spool");
    if reserved.exists() {
        let left = std::fs::read_dir(&reserved).expect("reserved").count();
        assert_eq!(left, 0, "cancelled exchange left a partial spool");
    }
    assert!(!root.join("slow.bin").exists());
}
