//! Cross-module and process-level tests.
#![allow(clippy::field_reassign_with_default)]

use std::fs;
use std::path::Path;

use vm::{SourceFlavor, Value, Vm, VmStatus, compile_source_with_flavor};

use crate::config::{CONFIG_FILE, Config, ConfigError, Origin};

const SHAPE_PROBE: &str = r#"
struct Probe {
    flag: bool,
    text: string,
    port: int?,
    items: [string],
    range: [[int]]?,
}

fn probe() -> Probe {
    {
        flag: false,
        text: "a\nb",
        port: null,
        items: [],
        range: [[1, 5], [8, 8]],
    }
}

probe();
"#;

#[test]
fn sdk_exports_named_structs_with_optional_and_array_fields() {
    let compiled = compile_source_with_flavor(SHAPE_PROBE, SourceFlavor::RustScript)
        .expect("probe module must compile");
    let mut vm = Vm::new(compiled.program);
    assert_eq!(vm.run().expect("probe must run"), VmStatus::Halted);
    let value = vm.stack().last().cloned().expect("probe result");
    let Value::Map(map) = value else {
        panic!("expected a named struct map, got {value:?}");
    };
    assert_eq!(map.get(&Value::string("flag")), Some(&Value::Bool(false)));
    assert_eq!(
        map.get(&Value::string("text")),
        Some(&Value::string("a\nb"))
    );
    // Guest named-struct values omit optional fields set to null.
    assert!(map.get(&Value::string("port")).is_none());
    assert_eq!(
        map.get(&Value::string("range")),
        Some(&Value::array(vec![
            Value::array(vec![Value::Int(1), Value::Int(5)]),
            Value::array(vec![Value::Int(8), Value::Int(8)]),
        ]))
    );
}

#[test]
fn migration_then_save_then_reload_survives_python_changes() {
    let root = tempfile::tempdir().expect("root");
    let python = root.path().join(crate::config::LEGACY_CONFIG_FILE);
    fs::write(
        &python,
        "dir = \"downloads\"\ndownload_ori = True\nproxy = [\"socks5://127.0.0.1:1080\"]\n",
    )
    .unwrap();

    let first = Config::load(root.path()).expect("import");
    assert_eq!(first.origin, Origin::ImportedPython);
    assert_eq!(first.config.dir, "downloads");
    assert!(first.config.download_ori);
    assert_eq!(first.config.proxy, ["socks5://127.0.0.1:1080"]);

    let mut changed = first.config.clone();
    changed.dir = "changed".into();
    changed.rpc_port = Some(7777);
    changed.save(root.path()).expect("save");

    let second = Config::load(root.path()).expect("reload");
    assert_eq!(second.origin, Origin::Rss);
    assert_eq!(second.config, changed);

    fs::write(&python, "this is not valid python at all").unwrap();
    let third = Config::load(root.path()).expect("rss only");
    assert_eq!(third.config, changed);

    fs::remove_file(&python).unwrap();
    let fourth = Config::load(root.path()).expect("rss only after delete");
    assert_eq!(fourth.config, changed);
}

#[test]
fn broken_rss_never_falls_back_to_python() {
    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join(CONFIG_FILE), "fn broken( {").unwrap();
    fs::write(
        root.path().join(crate::config::LEGACY_CONFIG_FILE),
        "dir = \"python\"\n",
    )
    .unwrap();
    let error = Config::load(root.path()).expect_err("must fail");
    assert!(matches!(error, ConfigError::RssCompile(_)), "{error}");
}

#[test]
fn entry_point_initializes_configuration_and_log() {
    let root = tempfile::tempdir().expect("root");
    assert_eq!(crate::run(root.path()), 2);
    assert!(root.path().join(CONFIG_FILE).is_file());
    let log = fs::read_to_string(root.path().join("eh.log")).expect("log file");
    assert!(
        log.contains("configuration loaded from built-in defaults"),
        "{log}"
    );
    assert!(log.contains("file "), "{log}");
}

#[test]
fn entry_point_imports_legacy_python_once() {
    let root = tempfile::tempdir().expect("root");
    fs::write(
        root.path().join(crate::config::LEGACY_CONFIG_FILE),
        "dir = \"from-python\"\nlog_verbose = 2\n",
    )
    .unwrap();
    assert_eq!(crate::run(root.path()), 2);
    let rss = fs::read_to_string(root.path().join(CONFIG_FILE)).unwrap();
    assert!(rss.contains("from-python"));
    let log = fs::read_to_string(root.path().join("eh.log")).unwrap();
    assert!(log.contains("config.py (imported once)"), "{log}");
}

#[test]
fn rust_and_rss_sources_stay_in_separate_directories() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for entry in fs::read_dir(root.join("src")).unwrap() {
        let path = entry.unwrap().path();
        assert_ne!(
            path.extension().and_then(|value| value.to_str()),
            Some("rss"),
            "found RSS source under src/: {}",
            path.display()
        );
    }
    let rss_dir = root.join("rss");
    if rss_dir.is_dir() {
        let mut stack = vec![rss_dir];
        while let Some(dir) = stack.pop() {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    assert_ne!(
                        path.extension().and_then(|value| value.to_str()),
                        Some("rs"),
                        "found Rust source under rss/: {}",
                        path.display()
                    );
                }
            }
        }
    }
    assert!(root.join("config.example.rss").is_file());
    assert!(!root.join("src").join("config.example.rss").exists());
}

#[test]
fn locale_resources_are_embedded_and_available_offline() {
    assert_eq!(
        crate::i18n::available_locales(),
        ["en_us", "zh_hans", "zh_hant"]
    );
}

fn config_json(config: &Config) -> serde_json::Value {
    serde_json::json!({
        "daemon": config.daemon,
        "dir": config.dir,
        "download_ori": config.download_ori,
        "jpn_title": config.jpn_title,
        "rename_ori": config.rename_ori,
        "proxy": config.proxy.clone(),
        "proxy_image": config.proxy_image,
        "proxy_image_only": config.proxy_image_only,
        "rpc_interface": config.rpc_interface,
        "rpc_port": config.rpc_port,
        "rpc_secret": config.rpc_secret,
        "rpc_open_browser": config.rpc_open_browser,
        "make_archive": config.make_archive,
        "download_range": config.download_range.clone().map(|rows| {
            rows.into_iter()
                .map(|row| vec![row[0], row[1]])
                .collect::<Vec<_>>()
        }),
        "scan_thread_cnt": config.scan_thread_cnt,
        "download_thread_cnt": config.download_thread_cnt,
        "download_timeout": config.download_timeout,
        "ignored_errors": config.ignored_errors.clone(),
        "log_path": config.log_path,
        "log_verbose": config.log_verbose,
        "save_tasks": config.save_tasks,
        "delete_task_files": config.delete_task_files,
        "low_speed_threshold": config.low_speed_threshold,
        "auto_update": config.auto_update,
        "update_beta_channel": config.update_beta_channel,
    })
}

fn compat_config(request: &serde_json::Value) -> serde_json::Value {
    let source = request["inputs"]["python"]
        .as_str()
        .expect("config probe input");
    let imported = crate::config::import_python(source).expect("config import");
    serde_json::json!({
        "defaults": config_json(&Config::default()),
        "imported": config_json(&imported),
    })
}

fn compat_util(request: &serde_json::Value) -> serde_json::Value {
    use crate::util;
    let inputs = &request["inputs"];
    let cookie_texts: Vec<&str> = inputs["cookie_texts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    let human_times: Vec<&str> = inputs["human_times"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    let html: Vec<&str> = inputs["html"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    let paths: Vec<&str> = inputs["paths"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap())
        .collect();
    let sizes: Vec<i64> = inputs["human_sizes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_i64().unwrap())
        .collect();
    serde_json::json!({
        "cookies": cookie_texts
            .iter()
            .map(|text| util::parse_cookie(text))
            .collect::<Vec<_>>(),
        "cookie_texts": cookie_texts
            .iter()
            .map(|text| util::make_cookie(&util::parse_cookie(text)))
            .collect::<Vec<_>>(),
        "human_time": human_times
            .iter()
            .map(|text| util::parse_human_time(text))
            .collect::<Vec<_>>(),
        "html": html
            .iter()
            .map(|text| match util::htmlescape(text) {
                Ok(value) => serde_json::json!({"ok": true, "value": value}),
                Err(error) => serde_json::json!({"ok": false, "error": error.to_string()}),
            })
            .collect::<Vec<_>>(),
        "legalpath_posix": paths
            .iter()
            .map(|text| util::legalpath_with(text, util::PathStyle::Posix))
            .collect::<Vec<_>>(),
        "legalpath_windows": paths
            .iter()
            .map(|text| util::legalpath_with(text, util::PathStyle::Windows))
            .collect::<Vec<_>>(),
        "human_size": sizes.iter().map(|size| util::human_size(*size)).collect::<Vec<_>>(),
        "proxy": ({
            [
                util::get_proxy_policy(true, true),
                util::get_proxy_policy(true, false),
                util::get_proxy_policy(false, true),
                util::get_proxy_policy(false, false),
            ]
            .iter()
            .map(|policy| format!("{policy:?}"))
            .collect::<Vec<_>>()
        }),
        "ua": [util::make_ua()],
    })
}

fn compat_i18n(request: &serde_json::Value) -> serde_json::Value {
    use crate::i18n;
    let inputs = &request["inputs"];
    let messages = inputs["messages"].as_array().unwrap();
    let codes = inputs["codes"].as_array().unwrap();
    let mut locales = serde_json::Map::new();
    for locale in [
        i18n::Locale::EnUs,
        i18n::Locale::ZhHans,
        i18n::Locale::ZhHant,
    ] {
        let mut rendered = serde_json::Map::new();
        for message in messages {
            let name = message["key"].as_str().unwrap();
            let args: Vec<&str> = message["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            let value = i18n::render(locale, name, &args).unwrap_or_default();
            rendered.insert(name.to_string(), serde_json::Value::String(value));
        }
        let mut errors = serde_json::Map::new();
        for entry in codes {
            let code = entry["code"].as_i64().unwrap();
            let args: Vec<&str> = entry["args"]
                .as_array()
                .unwrap()
                .iter()
                .map(|value| value.as_str().unwrap())
                .collect();
            errors.insert(
                code.to_string(),
                serde_json::Value::String(i18n::err_msg(locale, code, &args)),
            );
        }
        locales.insert(
            locale.tag().to_string(),
            serde_json::json!({"messages": rendered, "errors": errors}),
        );
    }
    serde_json::json!({
        "available": i18n::available_locales(),
        "reference_keys": i18n::REFERENCE_KEYS.to_vec(),
        "error_keys": i18n::ERROR_KEYS.to_vec(),
        "locales": locales,
    })
}

/// Environment-driven probe used by `tools/compat.py`. It is inert in normal
/// test runs.
#[test]
fn compat_probe() {
    let Ok(request_path) = std::env::var("XEH_COMPAT_REQUEST") else {
        return;
    };
    let request: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&request_path).expect("probe request"))
            .expect("probe request JSON");
    let suite = request["suite"].as_str().expect("probe suite");
    let result = match suite {
        "config" => compat_config(&request),
        "util" => compat_util(&request),
        "i18n" => compat_i18n(&request),
        "task-api" => compat_task_api(&request["inputs"]),
        "session" => compat_session(&request["inputs"]),
        "files" => compat_files(&request["inputs"]),
        "site" => compat_site(&request["inputs"]),
        "proxy" => compat_proxy(&request["inputs"]),
        "transport" => compat_transport(&request["inputs"]),
        other => panic!("unknown probe suite {other}"),
    };
    let output = std::env::var("XEH_COMPAT_OUTPUT").expect("probe output");
    fs::write(
        output,
        serde_json::to_string_pretty(&result).expect("probe JSON"),
    )
    .expect("write probe output");
}

fn compat_proxy(inputs: &serde_json::Value) -> serde_json::Value {
    use crate::proxy::Pool;

    let clock = std::sync::Arc::new(std::sync::Mutex::new(
        inputs["clock"].as_f64().unwrap_or(100.0),
    ));
    let now = std::sync::Arc::clone(&clock);
    let mut pool = Pool::with_policy_and_clock(
        Box::new(|_success, fails| fails >= crate::proxy::MAX_FAIL),
        Box::new(move || *now.lock().unwrap()),
    );
    let mut out = Vec::new();
    for step in inputs["steps"].as_array().expect("proxy steps") {
        let kind = step["op"].as_str().expect("op");
        let mut record = serde_json::Map::new();
        record.insert("op".into(), serde_json::Value::String(kind.into()));
        let mut exception: Option<String> = None;
        match kind {
            "add" => match pool.add_proxy(step["addr"].as_str().unwrap()) {
                Ok(()) => {
                    record.insert("result".into(), serde_json::Value::Null);
                }
                Err(error) => {
                    exception = Some(match error {
                        crate::proxy::ProxyError::Address(_) => "ValueError".into(),
                        _ => "ProxyError".into(),
                    });
                }
            },
            "select" => match pool.select() {
                Ok(spec) => {
                    record.insert(
                        "selected".into(),
                        serde_json::Value::String(spec.addr.clone()),
                    );
                    // The oracle calls the selected handler, which is wrapped
                    // by trace_proxy; a successful call is one success.
                    pool.trace(&spec.addr, Some(false), None);
                    record.insert("result".into(), serde_json::Value::Null);
                }
                Err(_) => exception = Some("PoolException".into()),
            },
            "not_good" => {
                pool.not_good(
                    step["addr"].as_str().unwrap(),
                    step["weight"].as_i64().unwrap_or(crate::proxy::MAX_FAIL),
                    step["expire"].as_f64().unwrap_or(0.0),
                );
                record.insert("result".into(), serde_json::Value::Null);
            }
            "trace" => {
                let addr = step["addr"].as_str().unwrap();
                match step["result"].as_str().unwrap_or("ok") {
                    "connect_error" => {
                        pool.trace(addr, Some(true), None);
                        exception = Some("ConnectionError".into());
                    }
                    "bad_status_line" => {
                        pool.trace(addr, None, None);
                        exception = Some("ConnectionError".into());
                    }
                    "check_false" => {
                        pool.trace(addr, Some(false), Some(false));
                        record.insert("result".into(), serde_json::Value::Null);
                    }
                    _ => {
                        pool.trace(addr, Some(false), None);
                        record.insert("result".into(), serde_json::Value::Null);
                    }
                }
            }
            "has_available" => {
                record.insert(
                    "result".into(),
                    serde_json::Value::Bool(pool.has_available_proxies()),
                );
            }
            "time" => {
                *clock.lock().unwrap() = step["now"].as_f64().unwrap_or(0.0);
                record.insert("result".into(), serde_json::Value::Null);
            }
            "snapshot" => {
                record.insert("result".into(), serde_json::Value::Null);
            }
            other => panic!("unknown proxy op {other}"),
        }
        if let Some(error) = exception {
            record.insert("exception".into(), serde_json::Value::String(error));
        }
        let entries: Vec<serde_json::Value> = pool
            .proxies
            .iter()
            .map(|(addr, entry)| serde_json::json!([addr, entry.success, entry.fail]))
            .collect();
        record.insert("entries".into(), serde_json::Value::Array(entries));
        let disabled: Vec<serde_json::Value> = pool
            .disabled
            .iter()
            .map(|(addr, expire)| serde_json::json!([addr, expire]))
            .collect();
        record.insert("disabled".into(), serde_json::Value::Array(disabled));
        record.insert(
            "has_available".into(),
            serde_json::Value::Bool(pool.has_available_proxies()),
        );
        out.push(serde_json::Value::Object(record));
    }
    let glype = compat_glype(inputs);
    serde_json::json!({"steps": out, "glype": glype})
}

fn compat_glype(_inputs: &serde_json::Value) -> serde_json::Value {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::proxy::{MapResolver, Pool, TlsClient};
    use crate::util::ProxyPolicy;
    use crate::worker::{FallbackMap, Session, SessionConfig, TestClock};

    async fn run_scenario(
        responses: Vec<Vec<u8>>,
        method: &str,
        data: Option<Vec<(String, String)>>,
    ) -> serde_json::Value {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&recorded);
        let server = tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    let read = socket.read(&mut byte).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&request).into_owned();
                let length = head
                    .lines()
                    .find_map(|line| {
                        let lower = line.to_ascii_lowercase();
                        lower
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                if length > 0 {
                    socket.read_exact(&mut body).await.unwrap();
                }
                seen.lock()
                    .unwrap()
                    .push((head, String::from_utf8_lossy(&body).into_owned()));
                socket.write_all(&response).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        let addr = format!("http://127.0.0.1:{port}/glype/browse.php?u=x&b=4");
        let mut pool = Pool::new();
        pool.add_proxy(&addr).unwrap();
        let mut client = Session::new(
            vec![
                ("User-Agent".into(), Some("xeHentai-test".into())),
                ("Cookie".into(), Some("a=1".into())),
            ],
            Some(Arc::new(Mutex::new(pool))),
            ProxyPolicy::All,
            SessionConfig {
                retry: 1,
                timeout: Duration::from_secs(3),
                tls: TlsClient::production(),
                resolver: Arc::new(MapResolver::default()),
                fallback: FallbackMap::empty(),
                clock: Arc::new(TestClock::new(0.0)),
                ..SessionConfig::default()
            },
        );
        let outcome = client
            .request(method, "https://e-hentai.org/g/1/", data, false)
            .await;
        let observation = match outcome {
            Ok(outcome) => {
                let mut calls = Vec::new();
                for (head, body) in recorded.lock().unwrap().iter() {
                    let line = head.lines().next().unwrap_or("");
                    let mut parts = line.split_whitespace();
                    let request_method = parts.next().unwrap_or("").to_string();
                    let path = parts.next().unwrap_or("").to_string();
                    let lower = head.to_ascii_lowercase();
                    let referer = lower
                        .lines()
                        .find_map(|header| header.strip_prefix("referer: "))
                        .map(|value| {
                            url::Url::parse(value.trim())
                                .map(|url| match url.query() {
                                    Some(query) => format!("{}?{query}", url.path()),
                                    None => url.path().to_string(),
                                })
                                .unwrap_or_else(|_| value.trim().to_string())
                        });
                    let cookie = lower
                        .lines()
                        .find_map(|header| header.strip_prefix("cookie: "))
                        .unwrap_or("")
                        .to_string();
                    calls.push(serde_json::json!({
                        "method": request_method,
                        "path": path,
                        "referer": referer,
                        "cookie": cookie,
                        "body": body,
                    }));
                }
                serde_json::json!({
                    "exception": serde_json::Value::Null,
                    "status": outcome.response.status,
                    "text": outcome.response.text,
                    "set_cookie": outcome.response.headers.joined("set-cookie"),
                    "calls": calls,
                })
            }
            Err(error) => serde_json::json!({
                "exception": "HttpError",
                "calls": [],
                "error": error.to_string(),
            }),
        };
        server.await.unwrap();
        observation
    }

    let normal_body =
        b"<a href=\"/glype/browse.php?u=https%3A%2F%2Fe-hentai.org%2Fg%2F1&b=4&f=norefer\">x</a>";
    let mut normal_response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nSet-Cookie: c[proxy][/][foo]=bar\r\nContent-Length: {}\r\n\r\n",
        normal_body.len()
    )
    .into_bytes();
    normal_response.extend_from_slice(normal_body);
    let ssl_agree = b"<input type=\"hidden\" name=\"action\" value=\"sslagree\">";
    let mut ssl_page = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
        ssl_agree.len()
    )
    .into_bytes();
    ssl_page.extend_from_slice(ssl_agree);
    let process_response = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: s=abc\r\nContent-Length: 2\r\n\r\nok".to_vec();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .unwrap();
    runtime.block_on(async move {
        let normal = run_scenario(vec![normal_response.clone()], "GET", None).await;
        let fixed = run_scenario(
            vec![
                ssl_page.clone(),
                process_response.clone(),
                normal_response.clone(),
            ],
            "GET",
            None,
        )
        .await;
        let caller = run_scenario(
            vec![
                ssl_page.clone(),
                process_response.clone(),
                normal_response.clone(),
            ],
            "GET",
            None,
        )
        .await;
        let post = run_scenario(
            vec![
                ssl_page.clone(),
                process_response.clone(),
                normal_response.clone(),
            ],
            "POST",
            Some(vec![
                ("username".to_string(), "synthetic-user".to_string()),
                ("password".to_string(), "synthetic-pass".to_string()),
            ]),
        )
        .await;
        serde_json::json!({
            "normal": normal,
            "sslagree_fixed": fixed,
            "sslagree_caller": caller,
            "post_fixed": post,
        })
    })
}

fn compat_transport(inputs: &serde_json::Value) -> serde_json::Value {
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use crate::proxy::{MapResolver, TlsClient};
    use crate::util::ProxyPolicy;
    use crate::worker::{FallbackMap, Session, SessionConfig, TestClock};

    async fn serve(
        responses: Vec<Vec<u8>>,
    ) -> (u16, Arc<Mutex<Vec<String>>>, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&recorded);
        let handle = tokio::spawn(async move {
            for response in responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut request = Vec::new();
                let mut byte = [0u8; 1];
                loop {
                    let read = socket.read(&mut byte).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                let head = String::from_utf8_lossy(&request).to_ascii_lowercase();
                let length = head
                    .lines()
                    .find_map(|line| {
                        line.strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
                let mut body = vec![0u8; length];
                if length > 0 {
                    socket.read_exact(&mut body).await.unwrap();
                }
                seen.lock().unwrap().push(format!(
                    "{}\n\n{}",
                    String::from_utf8_lossy(&request).trim_end_matches(['\r', '\n']),
                    String::from_utf8_lossy(&body)
                ));
                socket.write_all(&response).await.unwrap();
                socket.shutdown().await.unwrap();
            }
        });
        (port, recorded, handle)
    }

    fn session(
        retry: usize,
        headers: Vec<(String, Option<String>)>,
        clock: Arc<TestClock>,
    ) -> Session {
        Session::new(
            headers,
            None,
            ProxyPolicy::All,
            SessionConfig {
                retry,
                timeout: Duration::from_secs(3),
                tls: TlsClient::production(),
                resolver: Arc::new(MapResolver::default()),
                fallback: FallbackMap::empty(),
                clock,
                ..SessionConfig::default()
            },
        )
    }

    fn norm(text: &str, ports: &[u16]) -> String {
        let mut out = text.to_string();
        for port in ports {
            out = out.replace(&port.to_string(), "<PORT>");
        }
        out
    }

    let root = inputs["root"].as_str().unwrap_or(".");
    let _ = root;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(2)
        .build()
        .unwrap();
    runtime.block_on(async move {
        let mut out = serde_json::Map::new();

        // Redirect chain.
        let final_server = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\nlast".to_vec()
        ])
        .await;
        let final_port = final_server.0;
        let start_response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{final_port}/final\r\nContent-Length: 0\r\n\r\n"
        );
        let start_server = serve(vec![start_response.into_bytes()]).await;
        let start_port = start_server.0;
        let clock = Arc::new(TestClock::new(0.25));
        let mut client = session(1, Vec::new(), clock);
        let outcome = client
            .request(
                "GET",
                &format!("http://127.0.0.1:{start_port}/start"),
                None,
                false,
            )
            .await
            .unwrap();
        out.insert(
            "redirect".into(),
            serde_json::json!({
                "status": outcome.response.status,
                "url": norm(&outcome.response.url, &[start_port, final_port]),
                "real_url": norm(&outcome.response.real_url, &[start_port, final_port]),
                "content_length": outcome.response.content_length,
                "text": outcome.response.text,
                "hits": [1, 1],
            }),
        );
        start_server.2.await.unwrap();
        final_server.2.await.unwrap();

        // Retry loop against a closed port.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let closed_port = listener.local_addr().unwrap().port();
        drop(listener);
        let clock = Arc::new(TestClock::new(0.25));
        let mut client = session(2, Vec::new(), Arc::clone(&clock));
        let outcome = client
            .request(
                "GET",
                &format!("http://127.0.0.1:{closed_port}/x"),
                None,
                false,
            )
            .await
            .unwrap();
        out.insert(
            "retry".into(),
            serde_json::json!({
                "status": outcome.response.status,
                "url": norm(&outcome.response.url, &[closed_port]),
                "real_url": norm(&outcome.response.real_url, &[closed_port]),
                "content_length": if outcome.response.status == 600 { serde_json::Value::Null } else { serde_json::json!(outcome.response.content_length) },
                "text": outcome.response.text,
                "sleeps": clock.sleeps(),
                "attempts": outcome.attempts.len(),
            }),
        );

        // Session cookie jar and explicit header suppression.
        let jar_server = serve(vec![
            b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1; Path=/\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
        ])
        .await;
        let jar_port = jar_server.0;
        let clock = Arc::new(TestClock::new(0.0));
        let mut client = session(1, Vec::new(), clock);
        client
            .request("GET", &format!("http://127.0.0.1:{jar_port}/one"), None, false)
            .await
            .unwrap();
        client
            .request("GET", &format!("http://127.0.0.1:{jar_port}/two"), None, false)
            .await
            .unwrap();
        jar_server.2.await.unwrap();
        let jar_requests = jar_server.1.lock().unwrap().clone();
        let second = jar_requests[1].to_ascii_lowercase();
        let explicit_server = serve(vec![
            b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1; Path=/\r\nContent-Length: 2\r\n\r\nok".to_vec(),
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec(),
        ])
        .await;
        let explicit_port = explicit_server.0;
        let clock = Arc::new(TestClock::new(0.0));
        let mut client = session(
            1,
            vec![("Cookie".into(), Some("explicit=1".into()))],
            clock,
        );
        client
            .request(
                "GET",
                &format!("http://127.0.0.1:{explicit_port}/one"),
                None,
                false,
            )
            .await
            .unwrap();
        client
            .request(
                "GET",
                &format!("http://127.0.0.1:{explicit_port}/two"),
                None,
                false,
            )
            .await
            .unwrap();
        explicit_server.2.await.unwrap();
        let explicit = explicit_server.1.lock().unwrap()[1].to_ascii_lowercase();
        out.insert(
            "cookies".into(),
            serde_json::json!({
                "jar_cookie": if second.contains("cookie: a=1") { "cookie: a=1" } else { "" },
                "explicit_cookie": if explicit.contains("cookie: explicit=1") { "cookie: explicit=1" } else { "" },
                "jar_in_explicit": explicit.contains("a=1"),
            }),
        );

        // Duplicate and ISO-8859-1 response headers.
        let mut header_response =
            b"HTTP/1.1 200 OK\r\nX-Dup: one\r\nX-Dup: two\r\nX-Latin: caf\xe9\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                .to_vec();
        header_response.extend_from_slice(b"");
        let header_server = serve(vec![header_response]).await;
        let header_port = header_server.0;
        let clock = Arc::new(TestClock::new(0.0));
        let mut client = session(1, Vec::new(), clock);
        let outcome = client
            .request(
                "GET",
                &format!("http://127.0.0.1:{header_port}/h"),
                None,
                false,
            )
            .await
            .unwrap();
        let mut order: Vec<String> = Vec::new();
        let mut joined = serde_json::Map::new();
        for (name, value) in outcome.response.business_headers() {
            let key = name.to_ascii_lowercase();
            if let Some(existing) = joined.get_mut(&key) {
                let current = existing.as_str().unwrap_or("").to_string();
                *existing = serde_json::Value::String(format!("{current}, {value}"));
            } else {
                order.push(key.clone());
                joined.insert(key, serde_json::Value::String(value));
            }
        }
        out.insert(
            "headers".into(),
            serde_json::json!({
                "order": order,
                "joined": joined,
                "latin": outcome.response.headers.joined("x-latin"),
            }),
        );
        header_server.2.await.unwrap();

        // POST form body.
        let post_server = serve(vec![
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok".to_vec()
        ])
        .await;
        let post_port = post_server.0;
        let clock = Arc::new(TestClock::new(0.0));
        let mut client = session(1, Vec::new(), clock);
        let outcome = client
            .request(
                "POST",
                &format!("http://127.0.0.1:{post_port}/post"),
                Some(vec![
                    ("a".into(), "1".into()),
                    ("b".into(), "two words".into()),
                ]),
                false,
            )
            .await
            .unwrap();
        post_server.2.await.unwrap();
        let request = post_server.1.lock().unwrap()[0].clone();
        let (head, _, body) = request
            .split_once("\n\n")
            .map(|(head, body)| (head.to_string(), "", body.to_string()))
            .unwrap_or((request.clone(), "", String::new()));
        let method_line = head.lines().next().unwrap_or("").to_string();
        let content_type = head.lines().find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-type:")
                .map(|value| value.trim().to_string())
        });
        out.insert(
            "post".into(),
            serde_json::json!({
                "status": outcome.response.status,
                "method_line": method_line,
                "body": body,
                "content_type": content_type,
            }),
        );

        serde_json::Value::Object(out)
    })
}

fn compat_site(inputs: &serde_json::Value) -> serde_json::Value {
    use crate::filters::{Filters, PageResponse};
    let root = std::path::Path::new(inputs["root"].as_str().expect("site root"));
    let mut filters = Filters::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
        .expect("site filter module");
    let mut out = serde_json::Map::new();
    for scenario in inputs["scenarios"].as_array().expect("scenarios") {
        let id = scenario["id"].as_str().expect("id").to_string();
        let fn_name = scenario["fn"].as_str().expect("fn");
        let headers: Vec<(String, String)> = scenario["headers"]
            .as_array()
            .map(|rows| {
                rows.iter()
                    .map(|row| {
                        (
                            row[0].as_str().unwrap().to_string(),
                            row[1].as_str().unwrap().to_string(),
                        )
                    })
                    .collect()
            })
            .unwrap_or_default();
        let page = PageResponse {
            status: scenario["status"].as_i64().unwrap_or(200),
            url: scenario["url"].as_str().unwrap_or("").to_string(),
            real_url: scenario["real_url"].as_str().unwrap_or("").to_string(),
            content_length: scenario["content_length"].as_i64().unwrap_or(0),
            text: scenario["text"].as_str().unwrap_or("").to_string(),
            headers,
            ori: scenario["ori"].as_bool().unwrap_or(false),
            has_stream: scenario["has_stream"].as_bool().unwrap_or(false),
        };
        let mut callbacks: Vec<serde_json::Value> = Vec::new();
        let mut ret = serde_json::Value::Null;
        let mut exception: Option<String> = None;
        let mut debug: Option<String> = None;
        let mut stream_callbacks: Vec<serde_json::Value> = Vec::new();
        let mut stream_exception: Option<String> = None;
        match fn_name {
            "login" => match filters.login_outcome(&page) {
                Ok(result) => {
                    if let Some(cookies) = result.cookies {
                        let map: serde_json::Map<String, serde_json::Value> = cookies
                            .into_iter()
                            .map(|(name, value)| (name, serde_json::Value::String(value)))
                            .collect();
                        callbacks.push(serde_json::json!(["suc", [map]]));
                        ret = serde_json::json!(0);
                    } else if let Some(message) = result.fail_message {
                        callbacks.push(serde_json::json!(["fail", [message]]));
                        ret = serde_json::json!(1);
                    } else {
                        exception = result.exception;
                    }
                }
                Err(error) => exception = Some(error.to_string()),
            },
            "metadata" => match filters.metadata_outcome(&page) {
                Ok(result) => {
                    if let Some(code) = result.fail_code {
                        callbacks.push(serde_json::json!(["fail", [code]]));
                        if let Some(ban) = result.ban {
                            ret = serde_json::Value::String(ban);
                        }
                        exception = result.exception;
                    } else if let Some(meta) = result.meta {
                        callbacks.push(serde_json::json!([
                            "suc",
                            [{
                                "gjname": meta.gjname,
                                "gnname": meta.gnname,
                                "total": meta.total,
                                "finished": meta.finished,
                                "tags": meta.tags,
                                "thumbnail_cnt": meta.thumbnail_cnt,
                                "use_multipage_viewer": meta.use_multipage_viewer,
                            }]
                        ]));
                    } else {
                        exception = result.exception;
                    }
                }
                Err(error) => exception = Some(error.to_string()),
            },
            "pageurl" | "pageurl_mpv" => {
                let parsed = if fn_name == "pageurl" {
                    filters.pageurl_outcome(&page)
                } else {
                    filters.pageurl_mpv_outcome(&page)
                };
                match parsed {
                    Ok(result) => {
                        if let Some(code) = result.fail_code {
                            callbacks.push(serde_json::json!(["fail", [code]]));
                        }
                        for url in &result.urls {
                            callbacks.push(serde_json::json!(["suc", [url]]));
                        }
                        if let Some(body) = result.debug_text {
                            let path = root.join("debug.htm");
                            std::fs::write(&path, &body).expect("debug.htm");
                            debug = Some(std::fs::read_to_string(path).unwrap());
                        }
                        exception = result.exception;
                    }
                    Err(error) => exception = Some(error.to_string()),
                }
            }
            "imgurl" => match filters.imgurl_outcome(&page) {
                Ok(result) => {
                    if let (Some(image), Some(reload), Some(filename)) =
                        (result.image_url, result.reload_url, result.filename)
                    {
                        callbacks.push(serde_json::json!(["suc", [[image, reload, filename]]]));
                    } else if let Some(code) = result.fail_code {
                        callbacks.push(serde_json::json!([
                            "fail",
                            [[
                                code,
                                result.real_url.unwrap_or_default(),
                                result.url.unwrap_or_default()
                            ]]
                        ]));
                    } else {
                        exception = result.exception;
                    }
                }
                Err(error) => exception = Some(error.to_string()),
            },
            "download" => match filters.download_outcome(&page) {
                Ok(result) => {
                    if result.stream {
                        callbacks.push(serde_json::json!([
                            "suc",
                            [["STREAM", page.real_url, page.url]]
                        ]));
                    } else if let Some(code) = result.fail_code {
                        callbacks.push(serde_json::json!([
                            "fail",
                            [[
                                code,
                                result.real_url.unwrap_or_default(),
                                result.url.unwrap_or_default()
                            ]]
                        ]));
                    } else {
                        exception = result.exception;
                    }
                }
                Err(error) => exception = Some(error.to_string()),
            },
            "download_stream" => {
                let chunks = scenario["chunks"].as_array().cloned().unwrap_or_default();
                let mut actual = 0i64;
                for chunk in &chunks {
                    let len = chunk.as_str().unwrap_or("").len() as i64;
                    actual += len;
                    stream_callbacks.push(serde_json::json!(["chunk", len]));
                }
                let read_error = scenario["stream_error"].as_str();
                match filters.stream_outcome(&page, actual, read_error) {
                    Ok(result) => {
                        if result.broken {
                            stream_callbacks.push(serde_json::json!([
                                "fail",
                                [[
                                    result.code.unwrap_or(0),
                                    result.real_url.unwrap_or_default(),
                                    result.url.unwrap_or_default()
                                ]]
                            ]));
                            stream_exception = Some("DownloadAbortedException".into());
                        }
                    }
                    Err(error) => stream_exception = Some(error.to_string()),
                }
            }
            "reset_quota" => {
                let _ = filters.reset_quota(&page);
            }
            other => panic!("unknown site fn {other}"),
        }
        let mut observation = serde_json::Map::new();
        observation.insert("callbacks".into(), serde_json::Value::Array(callbacks));
        observation.insert("ret".into(), ret);
        observation.insert(
            "exception".into(),
            exception
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        observation.insert(
            "debug".into(),
            debug
                .map(serde_json::Value::String)
                .unwrap_or(serde_json::Value::Null),
        );
        if fn_name == "download_stream" {
            observation.insert(
                "stream_callbacks".into(),
                serde_json::Value::Array(stream_callbacks),
            );
            observation.insert(
                "stream_exception".into(),
                stream_exception
                    .map(serde_json::Value::String)
                    .unwrap_or(serde_json::Value::Null),
            );
        }
        out.insert(id, serde_json::Value::Object(observation));
    }
    serde_json::Value::Object(out)
}

#[test]
fn example_configuration_is_a_valid_default_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(root.join("config.example.rss")).unwrap();
    let config = Config::decode_rss(&source).expect("example must load through the SDK");
    assert_eq!(config, Config::default());
}

// --- M2: real Task snapshots and the public task API -----------------------

const IMAGE_HASH: &str = "0123456789abcdef0123456789abcdef01234567";
const GALLERY_URL: &str = "https://e-hentai.org/g/123456/0123456789/";
const EXH_URL: &str = "https://exhentai.org/g/123456/0123456789/";

fn test_logger() -> crate::util::logger::Logger {
    crate::util::logger::Logger::with_output(Box::new(Vec::new()), false)
}

fn test_app(root: &Path, guids: &[&str]) -> crate::core::App {
    let mut config = Config::default();
    config.dir = root.join("downloads").to_string_lossy().into_owned();
    config.save_tasks = true;
    crate::core::App::with_guid_source(
        config,
        test_logger(),
        root,
        root,
        Box::new(crate::core::FixedGuids::new(
            guids.iter().map(|g| g.to_string()),
        )),
    )
    .expect("app")
}

#[test]
fn task_configuration_is_snapshotted_at_creation() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["guid0001", "guid0002"]);
    let task_dir = root.path().join("task-specific");
    let kwargs = serde_json::json!({
        "download_thread_cnt": 9,
        "dir": task_dir.to_string_lossy(),
        "extra_flag": "kept",
        "extra_number": 7,
    });
    let (code, guid) = app
        .add_task(GALLERY_URL, kwargs.as_object().unwrap().clone())
        .expect("add task");
    assert_eq!(code, 0);
    let guid = guid.expect("guid");
    let task = app.task(&guid).expect("task");
    assert_eq!(task.config.download_thread_cnt, 9);
    assert_eq!(task.config.dir, task_dir.to_string_lossy());
    assert!(task.config.jpn_title);
    assert_eq!(task.config.scan_thread_cnt, 1);
    assert_eq!(
        task.config.extra.get("extra_flag"),
        Some(&serde_json::json!("kept"))
    );
    assert!(!task.config.extra.contains_key("rpc_port"));
    assert!(!task.config.extra.contains_key("daemon"));

    let mut patch = serde_json::Map::new();
    patch.insert("download_thread_cnt".into(), serde_json::json!(2));
    patch.insert("jpn_title".into(), serde_json::json!(false));
    patch.insert("scan_thread_cnt".into(), serde_json::json!(3));
    app.update_config(patch).expect("global config update");

    let old = app.task(&guid).expect("old task");
    assert_eq!(
        old.config.download_thread_cnt, 9,
        "snapshot must not follow app config"
    );
    assert!(old.config.jpn_title);
    assert_eq!(old.config.scan_thread_cnt, 1);

    let (code, second) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("second task");
    assert_eq!(code, 0);
    let second = second.expect("guid");
    let second = app.task(&second).expect("second task");
    assert_eq!(second.config.download_thread_cnt, 2);
    assert!(!second.config.jpn_title);
    assert_eq!(second.config.scan_thread_cnt, 3);
    assert!(second.config.extra.is_empty());
}

#[test]
fn task_api_codes_and_queue_order_match_the_reference() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["guid0001", "guid0002", "guid0003"]);
    assert_eq!(
        app.add_task("https://example.com/nope", serde_json::Map::new())
            .expect("bad url"),
        (1000, None)
    );
    assert_eq!(
        app.add_task(EXH_URL, serde_json::Map::new())
            .expect("exhentai"),
        (1001, None)
    );
    let (code, guid) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("valid");
    assert_eq!(code, 0);
    let guid = guid.unwrap();
    assert_eq!(app.queue_len(), 1);
    assert_eq!(app.task(&guid).unwrap().state, 1);

    assert_eq!(
        app.pause_task(&guid).expect("pause"),
        (0, Some(String::new()))
    );
    assert_eq!(app.task(&guid).unwrap().state, 0);
    assert_eq!(app.pause_task(&guid).expect("pause again"), (1106, None));
    assert_eq!(
        app.resume_task(&guid).expect("resume"),
        (0, Some(String::new()))
    );
    assert_eq!(app.task(&guid).unwrap().state, 1);
    assert_eq!(app.queue_len(), 2);
    assert_eq!(
        app.resume_task(&guid).expect("resume waiting"),
        (1107, None)
    );
    assert_eq!(app.del_task(&guid).expect("delete running"), (1105, None));
    app.pause_task(&guid).expect("pause");
    assert_eq!(
        app.del_task(&guid).expect("delete paused"),
        (0, Some(String::new()))
    );
    assert!(app.task(&guid).is_none());
    assert_eq!(app.del_task(&guid).expect("delete missing"), (1101, None));
    assert_eq!(
        app.pause_task("missing").expect("pause missing"),
        (1101, None)
    );
    assert_eq!(
        app.resume_task("missing").expect("resume missing"),
        (1101, None)
    );
}

#[test]
fn finished_resume_keeps_state_twenty_and_failed_resume_waits() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["guid0001", "guid0002"]);
    let (_, finished) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("task");
    let finished = finished.unwrap();
    app.task_mut(&finished).unwrap().state = 20;
    assert_eq!(app.queue_len(), 1);
    assert_eq!(
        app.resume_task(&finished).expect("resume finished"),
        (0, Some(String::new()))
    );
    assert_eq!(app.task(&finished).unwrap().state, 20);
    assert_eq!(app.queue_len(), 2);

    let (_, failed) = app
        .add_task("https://example.com/nope", serde_json::Map::new())
        .expect("bad");
    assert!(failed.is_none());
    let failed = app.all_task_guids()[1].clone();
    assert_eq!(app.task(&failed).unwrap().state, -1);
    assert_eq!(
        app.resume_task(&failed).expect("resume failed"),
        (0, Some(String::new()))
    );
    assert_eq!(app.task(&failed).unwrap().state, 1);
}

#[test]
fn guid_collisions_keep_the_existing_task() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["dupe0001", "dupe0001", "dupe0001"]);
    let (code, first) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("first");
    assert_eq!((code, first.as_deref()), (0, Some("dupe0001")));
    let (code, second) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("collision");
    assert_eq!((code, second.as_deref()), (0, Some("dupe0001")));
    assert_eq!(app.all_tasks_len(), 1);
    assert_eq!(app.queue_len(), 1);
    app.task_mut("dupe0001").unwrap().state = 20;
    let (code, third) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("finished collision");
    assert_eq!((code, third.as_deref()), (0, Some("dupe0001")));
    assert_eq!(app.all_tasks_len(), 1);
    assert_eq!(app.task("dupe0001").unwrap().state, 1);
    assert_eq!(app.queue_len(), 1);
}

#[test]
fn task_identifiers_and_metadata_follow_the_reference() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["guid0001"]);
    let (_, guid) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("task");
    let guid = guid.unwrap();
    let task = app.task_mut(&guid).unwrap();
    assert_eq!(task.base_url().unwrap(), "https://e-hentai.org");
    assert_eq!(
        task.mpv_url(),
        "https://e-hentai.org/mpv/123456/0123456789/"
    );
    assert_eq!(task.gid.as_deref(), Some("123456"));
    assert_eq!(task.sethash.as_deref(), Some("0123456789"));

    let mut meta = serde_json::Map::new();
    meta.insert("gnname".into(), serde_json::json!("English title"));
    meta.insert("gjname".into(), serde_json::json!("日本語タイトル"));
    meta.insert("total".into(), serde_json::json!(1000));
    task.update_meta(meta).unwrap();
    assert_eq!(task.meta["title"], serde_json::json!("日本語タイトル"));
    task.config.jpn_title = false;
    let mut meta = serde_json::Map::new();
    meta.insert("gnname".into(), serde_json::json!("English title"));
    meta.insert("gjname".into(), serde_json::json!("日本語タイトル"));
    task.update_meta(meta).unwrap();
    assert_eq!(task.meta["title"], serde_json::json!("English title"));

    assert_eq!(task.get_fidpad(7, "jpg").unwrap(), "0007.jpg");
    assert_eq!(task.get_fidpad(1000, "png").unwrap(), "1000.png");
    task.meta.insert("total".into(), serde_json::json!(1155));
    assert_eq!(task.get_fidpad(7, "jpg").unwrap(), "0007.jpg");
    assert_eq!(
        task.get_fid_unfinished().unwrap(),
        (1..1155).collect::<Vec<_>>()
    );
    assert_eq!(
        task.get_fpath().unwrap(),
        root.path().join("downloads").join("English title")
    );
    let hash = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(
        crate::task::Task::get_imghash(&format!(
            "https://img.example/{hash}-123-1-1-jpg/0123456789/sample.jpg"
        ))
        .unwrap(),
        "0123456789"
    );
    assert_eq!(
        task.get_imgfid("https://e-hentai.org/fullimg/1/456?x=1")
            .unwrap(),
        456
    );
}

#[test]
fn migrate_and_meta_and_fail_behaviour_match() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["guid0001"]);
    let (_, guid) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("task");
    let guid = guid.unwrap();
    let task = app.task_mut(&guid).unwrap();
    task.set_fail(1004);
    assert_eq!((task.state, task.failcode), (-1, 1004));
    assert!(task.meta.is_empty());
    assert!(task.migrate_exhentai());
    assert_eq!(task.url, "https://exhentai.org/g/123456/0123456789/");
    assert_eq!((task.state, task.failcode), (1, 0));
    task.url = GALLERY_URL.into();
    task.state = 10;
    task.failcode = 1004;
    assert!(task.migrate_exhentai());
    assert_eq!(task.state, 10);
    assert_eq!(task.failcode, 0);
    task.url = "https://example.com/g/1/x/".into();
    assert!(!task.migrate_exhentai());
}

#[test]
fn task_wire_round_trip_keeps_public_fields_and_omits_internal_ones() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["guid0001"]);
    let (_, guid) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("task");
    let guid = guid.unwrap();
    let task = app.task_mut(&guid).unwrap();
    let mut meta = serde_json::Map::new();
    meta.insert("gnname".into(), serde_json::json!("T"));
    meta.insert("gjname".into(), serde_json::Value::Null);
    meta.insert("total".into(), serde_json::json!(3));
    meta.insert("filelist".into(), serde_json::json!({"h": ["1"]}));
    task.update_meta(meta).unwrap();
    let wire = task.to_value().expect("wire");
    for key in [
        "url",
        "gid",
        "sethash",
        "failcode",
        "state",
        "guid",
        "config",
        "meta",
        "reload_map",
        "duplicate_map",
        "renamed_map",
    ] {
        assert!(wire.get(key).is_some(), "missing {key}");
    }
    assert!(wire.get("img_q").is_none());
    assert!(wire.get("page_q").is_none());
    assert!(wire.get("_flist_done").is_none());
    assert!(wire.get("logger").is_none());
    assert_eq!(wire["url"], serde_json::json!(GALLERY_URL));
    assert_eq!(wire["guid"], serde_json::json!("guid0001"));

    let loaded = crate::task::Task::from_wire(&wire, root.path()).expect("load");
    assert_eq!(loaded.url, GALLERY_URL);
    assert_eq!(loaded.guid, "guid0001");
    assert_eq!(loaded.state, 1);
    assert_eq!(loaded.config, task.config);
    assert_eq!(loaded.meta, task.meta);
    assert_eq!(loaded.gid.as_deref(), Some("123456"));
    assert_eq!(loaded.get_fpath().unwrap(), task.get_fpath().unwrap());
}

#[test]
fn empty_task_queues_serialize_as_empty_arrays_and_reload_as_none() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "queues", 2, |_| {});
    task.img_q = Some(std::collections::VecDeque::new());
    task.page_q = Some(std::collections::VecDeque::new());
    let wire = task.to_value().unwrap();
    assert_eq!(wire["img_q"], serde_json::json!([]));
    assert_eq!(wire["page_q"], serde_json::json!([]));
    let loaded = crate::task::Task::from_wire(&wire, root.path()).unwrap();
    assert!(loaded.img_q.is_none());
    assert!(loaded.page_q.is_none());
}

#[test]
fn legacy_cookie_file_is_loaded_only_without_a_login() {
    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join(".ehentai.cookie"), "42,hashvalue\n").unwrap();
    let mut config = Config::default();
    config.dir = root.path().join("downloads").to_string_lossy().into_owned();
    config.save_tasks = true;
    let app = crate::core::App::with_guid_source(
        config,
        test_logger(),
        root.path(),
        root.path(),
        Box::new(crate::core::FixedGuids::new(Vec::new())),
    )
    .expect("app");
    assert!(app.has_login());
    assert_eq!(app.cookie("ipb_member_id"), Some("42"));
    assert_eq!(app.cookie("ipb_pass_hash"), Some("hashvalue"));
    assert_eq!(
        app.cookie_header(),
        Some("nw=1;ipb_member_id=42;ipb_pass_hash=hashvalue")
    );
}

#[test]
fn session_save_and_load_round_trip_the_public_task_state() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["guid0001"]);
    let (_, guid) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("task");
    let guid = guid.unwrap();
    {
        let task = app.task_mut(&guid).unwrap();
        let mut meta = serde_json::Map::new();
        meta.insert("gnname".into(), serde_json::json!("T"));
        meta.insert("gjname".into(), serde_json::Value::Null);
        meta.insert("total".into(), serde_json::json!(3));
        meta.insert(
            "filelist".into(),
            serde_json::json!({"abcdef0123": ["1", "x"]}),
        );
        task.update_meta(meta).unwrap();
        task.config.make_archive = true;
        task.reload_map.insert(
            "abcdef0123".into(),
            ("https://e/1".into(), "001.jpg".into()),
        );
        task.renamed_map
            .insert(crate::task::FidKey::Int(1), "001.jpg".into());
        task.set_fid_finished(1);
    }
    let (code, message) = app.save_session();
    assert_eq!((code, message.as_deref()), (0, None));
    let raw = fs::read_to_string(root.path().join("h.json")).expect("h.json");
    let doc: serde_json::Value = serde_json::from_str(&raw).unwrap();
    assert!(doc.get("tasks").is_some());
    assert!(doc.get("cookies").is_some());
    assert_eq!(doc["cookies"]["nw"], serde_json::json!("1"));
    assert_eq!(
        doc["tasks"][&guid]["config"]["make_archive"],
        serde_json::json!(true)
    );

    let reloaded = test_app(root.path(), &[]);
    assert_eq!(reloaded.all_tasks_len(), 1);
    assert_eq!(reloaded.queue_len(), 1);
    let task = reloaded.task(&guid).expect("task");
    assert!(task.config.make_archive);
    assert_eq!(task.meta["title"], serde_json::json!("T"));
    assert_eq!(task.reload_map["abcdef0123"].1, "001.jpg");
    assert_eq!(
        task.renamed_map.get(&crate::task::FidKey::Text("1".into())),
        Some(&"001.jpg".to_string())
    );
}

#[test]
fn session_scan_downloaded_runs_on_load_and_state_ten_falls_back_to_scan_img() {
    let root = tempfile::tempdir().expect("root");
    let download_dir = root.path().join("downloads");
    let gallery = download_dir.join("T");
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"x").unwrap();
    fs::write(gallery.join("2.jpg"), b"").unwrap();
    let mut app = test_app(root.path(), &["guid0001"]);
    let (_, guid) = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .expect("task");
    let guid = guid.unwrap();
    {
        let task = app.task_mut(&guid).unwrap();
        let mut meta = serde_json::Map::new();
        meta.insert("gnname".into(), serde_json::json!("T"));
        meta.insert("gjname".into(), serde_json::Value::Null);
        meta.insert("total".into(), serde_json::json!(3));
        meta.insert(
            "filelist".into(),
            serde_json::json!({"abcdef0123": ["1", "x"]}),
        );
        task.update_meta(meta).unwrap();
        task.state = 10;
        task.page_q = Some(std::collections::VecDeque::from([GALLERY_URL.to_string()]));
        task.page_q
            .as_mut()
            .unwrap()
            .push_back(GALLERY_URL.to_string());
    }
    assert_eq!(app.save_session().0, 0);
    let reloaded = test_app(root.path(), &[]);
    let task = reloaded.task(&guid).unwrap();
    assert!(task.flist_done().contains(&1));
    assert!(!task.flist_done().contains(&2));
    assert!(!gallery.join("2.jpg").exists(), "zero-byte file removed");
    assert_eq!(task.state, 4);
    assert_eq!(task.meta["finished"], serde_json::json!(1));
}

// --- M2: session recovery, cookies and atomic writes ----------------------

fn app_with_config(
    root: &Path,
    config: Config,
    guids: &[&str],
) -> Result<crate::core::App, crate::core::AppError> {
    crate::core::App::with_guid_source(
        config,
        test_logger(),
        root,
        root,
        Box::new(crate::core::FixedGuids::new(
            guids.iter().map(|g| g.to_string()),
        )),
    )
}

fn session_config(root: &Path, save_tasks: bool) -> Config {
    let mut config = Config::default();
    config.dir = root.join("downloads").to_string_lossy().into_owned();
    config.save_tasks = save_tasks;
    config
}

#[test]
fn session_save_without_save_tasks_still_writes_cookies() {
    let root = tempfile::tempdir().expect("root");
    let mut app = app_with_config(
        root.path(),
        session_config(root.path(), false),
        &["guid0001"],
    )
    .expect("app");
    app.add_task(GALLERY_URL, serde_json::Map::new())
        .expect("task");
    app.set_cookie("a=1; b=x=y; Path=/; ipb_member_id=7; ipb_pass_hash=h");
    assert_eq!(
        app.cookie_header(),
        Some("nw=1;a=1;b=x=y;ipb_member_id=7;ipb_pass_hash=h")
    );
    assert_eq!(app.save_session().0, 0);
    let doc: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.path().join("h.json")).unwrap()).unwrap();
    assert_eq!(doc["tasks"], serde_json::json!({}));
    assert_eq!(doc["cookies"]["nw"], serde_json::json!("1"));
    assert_eq!(doc["cookies"]["a"], serde_json::json!("1"));

    let reloaded =
        app_with_config(root.path(), session_config(root.path(), false), &[]).expect("reload");
    assert_eq!(reloaded.all_tasks_len(), 0);
    assert_eq!(
        reloaded.cookie_header(),
        Some("nw=1;a=1;b=x=y;ipb_member_id=7;ipb_pass_hash=h")
    );
    assert!(reloaded.has_login());
}

#[test]
fn session_load_rejects_missing_top_level_keys() {
    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join("h.json"), "{\"tasks\":{}}").unwrap();
    let error = app_with_config(root.path(), session_config(root.path(), true), &[])
        .expect_err("missing cookies");
    assert!(
        matches!(
            error,
            crate::core::AppError::Session(crate::core::SessionError::MissingKey("cookies"))
        ),
        "{error}"
    );
    fs::write(root.path().join("h.json"), "{\"cookies\":{}}").unwrap();
    let error = app_with_config(root.path(), session_config(root.path(), true), &[])
        .expect_err("missing tasks");
    assert!(
        matches!(
            error,
            crate::core::AppError::Session(crate::core::SessionError::MissingKey("tasks"))
        ),
        "{error}"
    );
    fs::write(root.path().join("h.json"), "[]").unwrap();
    let error = app_with_config(root.path(), session_config(root.path(), true), &[])
        .expect_err("array session");
    assert!(
        matches!(
            error,
            crate::core::AppError::Session(crate::core::SessionError::MissingKey("tasks"))
        ),
        "{error}"
    );
}

#[test]
fn session_bad_json_is_a_warning_and_keeps_running() {
    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join("h.json"), "not json at all").unwrap();
    let mut app = app_with_config(root.path(), session_config(root.path(), true), &[])
        .expect("bad JSON is recoverable");
    assert_eq!(app.all_tasks_len(), 0);
    assert_eq!(app.cookie_header(), None);
    let error = app.load_session().expect_err("direct load reports");
    assert!(
        matches!(error, crate::core::SessionError::Json(_)),
        "{error}"
    );
    assert_eq!(
        fs::read_to_string(root.path().join("h.json")).unwrap(),
        "not json at all"
    );
}

#[test]
fn session_duplicate_load_enqueues_every_task_again() {
    let root = tempfile::tempdir().expect("root");
    let mut app = app_with_config(
        root.path(),
        session_config(root.path(), true),
        &["guid0001"],
    )
    .expect("app");
    let (_, guid) = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    let guid = guid.unwrap();
    app.task_mut(&guid).unwrap().state = 0;
    assert_eq!(app.save_session().0, 0);
    let mut reloaded =
        app_with_config(root.path(), session_config(root.path(), true), &[]).expect("first load");
    assert_eq!(reloaded.queue_len(), 1);
    reloaded.load_session().expect("second load");
    assert_eq!(reloaded.all_tasks_len(), 1);
    assert_eq!(reloaded.queue_len(), 2);
}

#[test]
fn session_save_preserves_the_previous_file_when_serialization_fails() {
    let root = tempfile::tempdir().expect("root");
    let mut app = app_with_config(
        root.path(),
        session_config(root.path(), true),
        &["guid0001"],
    )
    .expect("app");
    let (_, guid) = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    let guid = guid.unwrap();
    {
        let task = app.task_mut(&guid).unwrap();
        task.meta.insert("title".into(), serde_json::json!("T"));
        task.reload_map
            .insert("abcdef0123".into(), ("https://e/1".into(), "1.jpg".into()));
        task.duplicate_map.entry(1).or_default().insert(2);
    }
    let previous = "{\"tasks\": {}, \"cookies\": {\"nw\": \"1\"}}";
    fs::write(root.path().join("h.json"), previous).unwrap();
    let (code, message) = app.save_session();
    assert_eq!(code, 1103);
    assert!(message.unwrap().contains("pending copy set"));
    assert_eq!(
        fs::read_to_string(root.path().join("h.json")).unwrap(),
        previous,
        "the reference truncates here; the candidate keeps the old session"
    );
    let stages: Vec<_> = fs::read_dir(root.path())
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with(".h.json-"))
        .collect();
    assert!(stages.is_empty(), "no staging file may remain");
}

#[test]
fn session_save_failure_keeps_the_previous_session() {
    let root = tempfile::tempdir().expect("root");
    let mut app = app_with_config(
        root.path(),
        session_config(root.path(), true),
        &["guid0001"],
    )
    .expect("app");
    app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    let previous = "{\"tasks\": {}, \"cookies\": {\"nw\": \"1\"}}";
    fs::write(root.path().join("h.json"), previous).unwrap();
    let mut permissions = fs::metadata(root.path()).unwrap().permissions();
    let original = permissions.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o500);
        fs::set_permissions(root.path(), permissions).unwrap();
        let (code, _) = app.save_session();
        assert_eq!(code, 1103);
        assert_eq!(
            fs::read_to_string(root.path().join("h.json")).unwrap(),
            previous
        );
        fs::set_permissions(root.path(), original).unwrap();
    }
    #[cfg(not(unix))]
    {
        let _ = original;
    }
}

#[derive(Clone, Default)]
struct CapturedOutput(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl CapturedOutput {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}

impl std::io::Write for CapturedOutput {
    fn write(&mut self, data: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn session_app(
    root: &Path,
    dir: &str,
    guids: &[&str],
) -> Result<crate::core::App, crate::core::AppError> {
    session_app_with_logger(root, dir, guids, test_logger())
}

fn session_app_with_logger(
    root: &Path,
    dir: &str,
    guids: &[&str],
    logger: crate::util::logger::Logger,
) -> Result<crate::core::App, crate::core::AppError> {
    let mut config = Config::default();
    config.dir = dir.to_string();
    config.save_tasks = true;
    crate::core::App::with_guid_source(
        config,
        logger,
        root,
        root,
        Box::new(crate::core::FixedGuids::new(
            guids.iter().map(|guid| guid.to_string()),
        )),
    )
}

fn titled_task(
    app: &mut crate::core::App,
    kwargs: serde_json::Map<String, serde_json::Value>,
    title: &str,
) -> String {
    let (code, guid) = app.add_task(GALLERY_URL, kwargs).expect("add titled task");
    assert_eq!(code, 0);
    let guid = guid.expect("guid");
    let task = app.task_mut(&guid).expect("task");
    task.meta.insert("title".into(), serde_json::json!(title));
    guid
}

fn inject_task(document: &mut serde_json::Value, template: &str, guid: &str, dir: &str) {
    let tasks = document["tasks"].as_object_mut().expect("tasks object");
    let mut task = tasks
        .get(template)
        .expect("template task")
        .as_object()
        .expect("task object")
        .clone();
    task.insert("guid".into(), serde_json::json!(guid));
    task["config"]
        .as_object_mut()
        .expect("task config")
        .insert("dir".into(), serde_json::json!(dir));
    tasks.insert(guid.to_string(), serde_json::Value::Object(task));
}

fn move_task_to(document: &mut serde_json::Value, guid: &str, position: usize) {
    let tasks = document["tasks"].as_object_mut().expect("tasks object");
    let moved = tasks.shift_remove(guid).expect("moved task");
    let rest: Vec<(String, serde_json::Value)> = tasks
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    tasks.clear();
    for (index, (key, value)) in rest.into_iter().enumerate() {
        if index == position {
            tasks.insert(guid.to_string(), moved.clone());
        }
        tasks.insert(key, value);
    }
    if position >= tasks.len() {
        tasks.insert(guid.to_string(), moved);
    }
}

#[test]
fn session_restores_a_task_saved_with_an_older_relative_directory() {
    let root = tempfile::tempdir().expect("root");
    let mut app = session_app(root.path(), "elsewhere", &["guid0001"]).expect("app");
    let guid = titled_task(&mut app, serde_json::Map::new(), "T");
    app.set_cookie("a=1; ipb_member_id=7; ipb_pass_hash=h");
    assert_eq!(app.save_session().0, 0);
    let document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.path().join("h.json")).unwrap()).unwrap();
    assert_eq!(
        document["tasks"][&guid]["config"]["dir"],
        serde_json::json!("elsewhere")
    );

    let loaded = session_app(root.path(), "downloads", &[]).expect("historical session");
    let task = loaded.task(&guid).expect("historical task restored");
    assert_eq!(task.config.dir, "elsewhere");
    assert_eq!(
        task.get_fpath().unwrap(),
        root.path().join("elsewhere").join("T")
    );
    assert_eq!(*loaded.queue(), std::collections::VecDeque::from([guid]));
    assert_eq!(
        loaded.cookie_header(),
        Some("nw=1;a=1;ipb_member_id=7;ipb_pass_hash=h")
    );
}

#[test]
fn session_restores_tasks_after_update_config_changed_the_directory() {
    let root = tempfile::tempdir().expect("root");
    let mut app = session_app(root.path(), "downloads", &["guid0001", "guid0002"]).expect("app");
    let first = titled_task(&mut app, serde_json::Map::new(), "First");
    let mut patch = serde_json::Map::new();
    patch.insert(
        "dir".into(),
        serde_json::json!(root.path().join("alt").to_string_lossy()),
    );
    app.update_config(patch).expect("update config");
    let second = titled_task(&mut app, serde_json::Map::new(), "Second");
    assert_eq!(app.save_session().0, 0);

    let loaded = session_app(root.path(), "downloads", &[]).expect("updated session");
    assert_eq!(loaded.all_task_guids(), vec![first.clone(), second.clone()]);
    assert_eq!(loaded.task(&first).unwrap().config.dir, "downloads");
    assert_eq!(
        loaded.task(&second).unwrap().config.dir,
        root.path().join("alt").to_string_lossy()
    );
    assert_eq!(
        loaded.task(&second).unwrap().get_fpath().unwrap(),
        root.path().join("alt").join("Second")
    );
    assert_eq!(
        *loaded.queue(),
        std::collections::VecDeque::from([first, second])
    );
}

#[test]
fn session_restores_multiple_task_directories_and_subdirectories() {
    let root = tempfile::tempdir().expect("root");
    let mut app = session_app(
        root.path(),
        "downloads",
        &["guid0001", "guid0002", "guid0003"],
    )
    .expect("app");
    let first = titled_task(&mut app, serde_json::Map::new(), "First");
    let mut elsewhere = serde_json::Map::new();
    elsewhere.insert("dir".into(), serde_json::json!("elsewhere"));
    let second = titled_task(&mut app, elsewhere, "Second");
    let mut special = serde_json::Map::new();
    special.insert("dir".into(), serde_json::json!("downloads/special"));
    let third = titled_task(&mut app, special, "Third");
    assert_eq!(app.save_session().0, 0);

    let loaded = session_app(root.path(), "downloads", &[]).expect("multiple dirs");
    assert_eq!(
        loaded.all_task_guids(),
        vec![first.clone(), second.clone(), third.clone()]
    );
    assert_eq!(loaded.task(&first).unwrap().config.dir, "downloads");
    assert_eq!(loaded.task(&second).unwrap().config.dir, "elsewhere");
    assert_eq!(loaded.task(&third).unwrap().config.dir, "downloads/special");
    assert_eq!(
        loaded.task(&third).unwrap().get_fpath().unwrap(),
        root.path().join("downloads").join("special").join("Third")
    );
    assert_eq!(
        *loaded.queue(),
        std::collections::VecDeque::from([first, second, third])
    );
}

#[test]
fn session_accepts_equivalent_and_sibling_absolute_directories() {
    let root = tempfile::tempdir().expect("root");
    let mut app = session_app(root.path(), "downloads", &["guid0001", "guid0002"]).expect("app");
    let mut equivalent = serde_json::Map::new();
    equivalent.insert(
        "dir".into(),
        serde_json::json!(root.path().join("downloads").to_string_lossy()),
    );
    let same = titled_task(&mut app, equivalent, "Same");
    let mut sibling = serde_json::Map::new();
    sibling.insert(
        "dir".into(),
        serde_json::json!(root.path().join("sibling").to_string_lossy()),
    );
    let other = titled_task(&mut app, sibling, "Other");
    assert_eq!(app.save_session().0, 0);

    let loaded = session_app(root.path(), "downloads", &[]).expect("absolute roots");
    assert_eq!(loaded.all_task_guids(), vec![same.clone(), other.clone()]);
    let task = loaded
        .task(&same)
        .expect("absolute form of the configured relative root");
    assert_eq!(
        task.config.dir,
        root.path().join("downloads").to_string_lossy()
    );
    assert_eq!(
        task.get_fpath().unwrap(),
        root.path().join("downloads").join("Same")
    );
    assert_eq!(
        loaded.task(&other).unwrap().get_fpath().unwrap(),
        root.path().join("sibling").join("Other")
    );
    assert_eq!(
        *loaded.queue(),
        std::collections::VecDeque::from([same, other])
    );
}

#[test]
fn session_skips_an_unauthorized_task_and_keeps_the_valid_state() {
    let root = tempfile::tempdir().expect("root");
    let output = CapturedOutput::default();
    let mut app = session_app_with_logger(root.path(), "downloads", &["guid0001"], {
        crate::util::logger::Logger::with_output(Box::new(output.clone()), false)
    })
    .expect("app");
    let guid = titled_task(&mut app, serde_json::Map::new(), "T");
    app.set_cookie("a=1; ipb_member_id=7; ipb_pass_hash=h");
    assert_eq!(app.save_session().0, 0);
    let path = root.path().join("h.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    inject_task(
        &mut document,
        &guid,
        "hostile1",
        "/definitely/not/authorized",
    );
    fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();
    let before = fs::read(&path).unwrap();

    let loaded = session_app_with_logger(root.path(), "downloads", &[], {
        crate::util::logger::Logger::with_output(Box::new(output.clone()), false)
    })
    .expect("a single unsafe task must not fail startup");
    assert_eq!(loaded.all_task_guids(), vec![guid.clone()]);
    assert!(loaded.task("hostile1").is_none());
    assert_eq!(*loaded.queue(), std::collections::VecDeque::from([guid]));
    assert_eq!(
        loaded.cookie_header(),
        Some("nw=1;a=1;ipb_member_id=7;ipb_pass_hash=h")
    );
    assert!(loaded.has_login());
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "load must not rewrite h.json"
    );
    let log = output.text();
    assert!(
        log.contains("skipping task hostile1") && log.contains("/definitely/not/authorized"),
        "{log}"
    );
}

#[test]
fn session_boundary_checks_use_components_and_canonical_paths() {
    let root = tempfile::tempdir().expect("root");
    let base = root.path().join("work");
    fs::create_dir_all(&base).unwrap();
    let outside = root.path().join("outside");
    fs::create_dir_all(outside.join("T")).unwrap();
    fs::write(outside.join("T").join("1.jpg"), b"").unwrap();
    let external = root.path().join("ext").join("downloads");
    fs::create_dir_all(root.path().join("ext")).unwrap();
    fs::create_dir_all(root.path().join("ext").join("downloads-evil").join("T")).unwrap();
    fs::write(
        root.path()
            .join("ext")
            .join("downloads-evil")
            .join("T")
            .join("1.jpg"),
        b"",
    )
    .unwrap();
    let mut variants: Vec<(&str, String)> = vec![
        (
            "prefix01",
            root.path()
                .join("ext")
                .join("downloads-evil")
                .to_string_lossy()
                .into_owned(),
        ),
        ("dotdot01", "../outside".to_string()),
        ("abs00001", "/definitely/not/authorized".to_string()),
        (
            "legal001",
            root.path()
                .join("ext")
                .join("downloads")
                .join("sub")
                .to_string_lossy()
                .into_owned(),
        ),
    ];
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();
        variants.insert(2, ("link0001", "link".to_string()));
    }
    #[cfg(not(unix))]
    let _ = &outside;

    let output = CapturedOutput::default();
    let mut app = session_app_with_logger(
        &base,
        &external.to_string_lossy(),
        &["base0001"],
        crate::util::logger::Logger::with_output(Box::new(output.clone()), false),
    )
    .expect("app");
    let base_guid = titled_task(&mut app, serde_json::Map::new(), "T");
    app.set_cookie("a=1; ipb_member_id=7; ipb_pass_hash=h");
    assert_eq!(app.save_session().0, 0);
    let path = base.join("h.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    {
        let tasks = document["tasks"].as_object_mut().unwrap();
        let template = tasks[&base_guid].as_object().unwrap().clone();
        for (guid, dir) in variants {
            let mut task = template.clone();
            task.insert("guid".into(), serde_json::json!(guid));
            task["config"]
                .as_object_mut()
                .unwrap()
                .insert("dir".into(), serde_json::json!(dir));
            tasks.insert(guid.to_string(), serde_json::Value::Object(task));
        }
    }
    fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();

    let loaded = session_app_with_logger(
        &base,
        &external.to_string_lossy(),
        &[],
        crate::util::logger::Logger::with_output(Box::new(output.clone()), false),
    )
    .expect("boundary session");
    assert_eq!(
        loaded.all_task_guids(),
        vec![base_guid.clone(), "legal001".into()]
    );
    assert_eq!(
        *loaded.queue(),
        std::collections::VecDeque::from([base_guid.clone(), "legal001".into()])
    );
    let mut rejected = vec!["prefix01", "dotdot01", "abs00001"];
    #[cfg(unix)]
    rejected.push("link0001");
    for rejected in &rejected {
        assert!(
            loaded.task(rejected).is_none(),
            "{rejected} must be rejected"
        );
    }
    assert_eq!(
        loaded.task("legal001").unwrap().get_fpath().unwrap(),
        external.join("sub").join("T")
    );
    assert!(
        outside.join("T").join("1.jpg").is_file(),
        "rejected tasks must not be scanned"
    );
    assert!(
        root.path()
            .join("ext")
            .join("downloads-evil")
            .join("T")
            .join("1.jpg")
            .is_file(),
        "component-prefix escape must not be scanned"
    );
    assert_eq!(
        loaded.cookie_header(),
        Some("nw=1;a=1;ipb_member_id=7;ipb_pass_hash=h")
    );
    let log = output.text();
    for rejected in &rejected {
        assert!(log.contains(&format!("skipping task {rejected}")), "{log}");
    }
}

#[test]
fn session_isolates_an_unsafe_task_and_preserves_every_legal_field() {
    let root = tempfile::tempdir().expect("root");
    let base = root.path().join("work");
    fs::create_dir_all(&base).unwrap();
    let outside = root.path().join("outside");
    fs::create_dir_all(outside.join("T-first")).unwrap();
    fs::write(outside.join("T-first").join("1.jpg"), b"").unwrap();

    let output = CapturedOutput::default();
    let mut app = session_app_with_logger(
        &base,
        "downloads",
        &["guid0001", "guid0002", "guid0003"],
        crate::util::logger::Logger::with_output(Box::new(output.clone()), false),
    )
    .expect("app");
    let first = titled_task(&mut app, serde_json::Map::new(), "T-first");
    {
        let task = app.task_mut(&first).unwrap();
        task.state = 1;
        task.img_q = Some(std::collections::VecDeque::from([
            "https://img.example/a.jpg".to_string(),
            "https://img.example/b.jpg".to_string(),
        ]));
        task.page_q = Some(std::collections::VecDeque::from([format!(
            "{GALLERY_URL}?p=0"
        )]));
        task.reload_map.insert(
            "abcdef0123".into(),
            (
                "https://e-hentai.org/s/abcdef0123/111-1".into(),
                "1.jpg".into(),
            ),
        );
        task.renamed_map
            .insert(crate::task::FidKey::Int(1), "1.jpg".into());
        task.meta.insert("total".into(), serde_json::json!(2));
        task.meta.insert(
            "filelist".into(),
            serde_json::json!({"abcdef0123": ["1", "x"]}),
        );
    }
    let mut elsewhere = serde_json::Map::new();
    elsewhere.insert("dir".into(), serde_json::json!("elsewhere"));
    let second = titled_task(&mut app, elsewhere, "T-second");
    {
        let task = app.task_mut(&second).unwrap();
        task.state = 10;
        task.page_q = Some(std::collections::VecDeque::from([format!(
            "{GALLERY_URL}?p=1"
        )]));
    }
    let mut special = serde_json::Map::new();
    special.insert("dir".into(), serde_json::json!("downloads/special"));
    let third = titled_task(&mut app, special, "T-third");
    app.task_mut(&third).unwrap().state = 3;
    app.set_cookie("a=1; b=x=y; ipb_member_id=7; ipb_pass_hash=h");
    assert_eq!(app.save_session().0, 0);

    let legal_first = base.join("downloads").join("T-first");
    fs::create_dir_all(&legal_first).unwrap();
    fs::write(legal_first.join("1.jpg"), b"one").unwrap();
    fs::write(legal_first.join("2.jpg"), b"").unwrap();

    let path = base.join("h.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    inject_task(&mut document, &first, "unsafe01", "../outside");
    move_task_to(&mut document, "unsafe01", 1);
    fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();
    let before = fs::read(&path).unwrap();

    let loaded = session_app_with_logger(
        &base,
        "downloads",
        &[],
        crate::util::logger::Logger::with_output(Box::new(output.clone()), false),
    )
    .expect("mixed session");
    assert_eq!(
        loaded.all_task_guids(),
        vec![first.clone(), second.clone(), third.clone()]
    );
    assert_eq!(
        *loaded.queue(),
        std::collections::VecDeque::from([first.clone(), second.clone(), third.clone()])
    );
    assert!(loaded.task("unsafe01").is_none());
    assert_eq!(loaded.task(&first).unwrap().config.dir, "downloads");
    assert_eq!(loaded.task(&second).unwrap().config.dir, "elsewhere");
    assert_eq!(loaded.task(&third).unwrap().config.dir, "downloads/special");
    assert_eq!(loaded.task(&first).unwrap().state, 1);
    assert_eq!(loaded.task(&second).unwrap().state, 4);
    assert_eq!(loaded.task(&third).unwrap().state, 3);
    assert_eq!(
        loaded.task(&first).unwrap().img_q,
        Some(std::collections::VecDeque::from([
            "https://img.example/a.jpg".to_string(),
            "https://img.example/b.jpg".to_string(),
        ]))
    );
    assert_eq!(
        loaded.task(&second).unwrap().page_q,
        Some(std::collections::VecDeque::from([format!(
            "{GALLERY_URL}?p=1"
        )]))
    );
    assert_eq!(loaded.task(&first).unwrap().flist_done().len(), 1);
    assert!(!legal_first.join("2.jpg").exists(), "legal scan still runs");
    assert_eq!(
        loaded.cookies(),
        &[
            ("nw".to_string(), "1".to_string()),
            ("a".to_string(), "1".to_string()),
            ("b".to_string(), "x=y".to_string()),
            ("ipb_member_id".to_string(), "7".to_string()),
            ("ipb_pass_hash".to_string(), "h".to_string()),
        ]
    );
    assert_eq!(
        loaded.cookie_header(),
        Some("nw=1;a=1;b=x=y;ipb_member_id=7;ipb_pass_hash=h")
    );
    let names: Vec<&str> = loaded
        .headers()
        .iter()
        .map(|header| header.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "User-Agent",
            "Accept-Charset",
            "Accept",
            "Connection",
            "Cookie"
        ]
    );
    assert!(loaded.has_login());
    assert_eq!(
        fs::read(&path).unwrap(),
        before,
        "load must keep the session file"
    );
    assert!(
        outside.join("T-first").join("1.jpg").is_file(),
        "the unsafe task must not be scanned"
    );
    assert!(
        !outside.join("T-first").join(".xehdone").exists(),
        "the unsafe task must not be touched"
    );
    let log = output.text();
    assert!(log.contains("skipping task unsafe01"), "{log}");

    let mut loaded = loaded;
    assert_eq!(loaded.save_session().0, 0);
    let saved: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(
        saved["tasks"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect::<Vec<_>>(),
        vec![first.clone(), second.clone(), third.clone()]
    );
    let restarted = session_app(&base, "downloads", &[]).expect("restart");
    assert_eq!(
        restarted.all_task_guids(),
        vec![first.clone(), second.clone(), third.clone()]
    );
    assert_eq!(restarted.task(&first).unwrap().config.dir, "downloads");
    assert_eq!(restarted.task(&second).unwrap().config.dir, "elsewhere");
    assert_eq!(
        restarted.task(&third).unwrap().config.dir,
        "downloads/special"
    );
}

#[test]
fn session_legacy_cookie_and_legal_tasks_survive_an_unsafe_task() {
    let root = tempfile::tempdir().expect("root");
    let base = root.path().join("work");
    fs::create_dir_all(&base).unwrap();
    let mut app = session_app(&base, "downloads", &["guid0001"]).expect("app");
    let guid = titled_task(&mut app, serde_json::Map::new(), "T");
    assert_eq!(app.save_session().0, 0);
    let path = base.join("h.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    inject_task(&mut document, &guid, "unsafe01", "../outside");
    fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();
    fs::write(base.join(".ehentai.cookie"), "42,legacyhash\n").unwrap();

    let loaded = session_app(&base, "downloads", &[]).expect("legacy mixed session");
    assert_eq!(loaded.all_task_guids(), vec![guid]);
    assert!(loaded.task("unsafe01").is_none());
    assert!(loaded.has_login());
    assert_eq!(
        loaded.cookie_header(),
        Some("nw=1;ipb_member_id=42;ipb_pass_hash=legacyhash")
    );
}

#[test]
fn entry_point_skips_an_unsafe_session_task() {
    let root = tempfile::tempdir().expect("root");
    let mut app = session_app(root.path(), "downloads", &["guid0001"]).expect("app");
    let guid = titled_task(&mut app, serde_json::Map::new(), "T");
    assert_eq!(app.save_session().0, 0);
    let path = root.path().join("h.json");
    let mut document: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    inject_task(&mut document, &guid, "unsafe01", "../outside");
    fs::write(&path, serde_json::to_string(&document).unwrap()).unwrap();
    assert_eq!(
        crate::run(root.path()),
        2,
        "a rejected task must not exit 1"
    );
    let log = fs::read_to_string(root.path().join("eh.log")).expect("log file");
    assert!(log.contains("skipping task unsafe01"), "{log}");
    assert!(log.contains("load 1 tasks from saved session"), "{log}");
}

#[test]
fn legacy_cookie_does_not_override_a_logged_in_session() {
    let root = tempfile::tempdir().expect("root");
    fs::write(root.path().join(".ehentai.cookie"), "1,oldhash\n").unwrap();
    fs::write(
        root.path().join("h.json"),
        "{\"tasks\":{},\"cookies\":{\"nw\":\"1\",\"ipb_member_id\":\"9\",\"ipb_pass_hash\":\"newhash\"}}",
    )
    .unwrap();
    let app = app_with_config(root.path(), session_config(root.path(), true), &[]).expect("app");
    assert_eq!(app.cookie("ipb_member_id"), Some("9"));
    assert_eq!(app.cookie("ipb_pass_hash"), Some("newhash"));
    assert_eq!(
        app.cookie_header(),
        Some("nw=1;ipb_member_id=9;ipb_pass_hash=newhash")
    );
}

#[test]
fn session_cookie_order_follows_the_reference_update_rules() {
    let root = tempfile::tempdir().expect("root");
    fs::write(
        root.path().join("h.json"),
        "{\"tasks\":{},\"cookies\":{\"z\":\"1\",\"nw\":\"9\",\"a\":\"2\"}}",
    )
    .unwrap();
    let app = app_with_config(root.path(), session_config(root.path(), true), &[]).expect("app");
    assert_eq!(
        app.cookies(),
        &[
            ("nw".to_string(), "9".to_string()),
            ("z".to_string(), "1".to_string()),
            ("a".to_string(), "2".to_string())
        ]
    );
    assert_eq!(app.cookie_header(), Some("nw=9;z=1;a=2"));
}

#[test]
fn session_rejects_oversized_input_without_touching_it() {
    let root = tempfile::tempdir().expect("root");
    let path = root.path().join("h.json");
    let file = fs::File::create(&path).unwrap();
    file.set_len(crate::core::MAX_SESSION_BYTES + 1).unwrap();
    let error =
        app_with_config(root.path(), session_config(root.path(), true), &[]).expect_err("budget");
    assert!(
        matches!(
            error,
            crate::core::AppError::Session(crate::core::SessionError::TooLarge { .. })
        ),
        "{error}"
    );
    assert_eq!(
        fs::metadata(&path).unwrap().len(),
        crate::core::MAX_SESSION_BYTES + 1
    );
}

#[test]
fn mixed_rename_indices_are_serialized_twice_and_collapse_on_reload() {
    let root = tempfile::tempdir().expect("root");
    let mut app = app_with_config(
        root.path(),
        session_config(root.path(), true),
        &["guid0001"],
    )
    .expect("app");
    let (_, guid) = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    let guid = guid.unwrap();
    {
        let task = app.task_mut(&guid).unwrap();
        task.meta.insert("title".into(), serde_json::json!("T"));
        task.renamed_map
            .insert(crate::task::FidKey::Int(5), "a.png".into());
        task.renamed_map
            .insert(crate::task::FidKey::Text("5".into()), "b.png".into());
    }
    assert_eq!(app.save_session().0, 0);
    let raw = fs::read_to_string(root.path().join("h.json")).unwrap();
    assert_eq!(
        raw.matches("\"5\":").count(),
        2,
        "json.dumps emits both dict keys: {raw}"
    );
    let reloaded =
        app_with_config(root.path(), session_config(root.path(), true), &[]).expect("reload");
    let task = reloaded.task(&guid).unwrap();
    assert_eq!(task.renamed_map.len(), 1);
    assert_eq!(
        task.renamed_map.get(&crate::task::FidKey::Text("5".into())),
        Some(&"b.png".to_string())
    );
}

// --- M2: files, duplicates, naming, streaming and archives -----------------

fn prepared_task(
    root: &Path,
    guid: &str,
    title: &str,
    total: i64,
    config_patch: impl FnOnce(&mut Config),
) -> crate::task::Task {
    let mut config = session_config(root, true);
    config_patch(&mut config);
    let task_config = crate::task::TaskConfig::from_app(&config, &serde_json::Map::new()).unwrap();
    let mut task = crate::task::Task::new(GALLERY_URL, task_config, guid.into(), root).unwrap();
    fs::create_dir_all(root.join("downloads")).unwrap();
    let mut meta = serde_json::Map::new();
    meta.insert("gnname".into(), serde_json::json!(title));
    meta.insert("gjname".into(), serde_json::Value::Null);
    meta.insert("total".into(), serde_json::json!(total));
    task.update_meta(meta).unwrap();
    task
}

#[test]
fn scan_downloaded_handles_ranges_zero_bytes_markers_and_zips() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "scan", 4, |config| {
        config.download_range = Some(vec![[2, 3]]);
    });
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"one").unwrap();
    fs::write(gallery.join("2.jpg"), b"two").unwrap();
    fs::write(gallery.join("3.jpg"), b"").unwrap();
    fs::write(gallery.join("4.jpg"), b"four").unwrap();
    task.scan_downloaded().unwrap();
    assert_eq!(
        task.flist_done().iter().copied().collect::<Vec<_>>(),
        vec![1, 2, 4]
    );
    assert_eq!(task.meta["finished"], serde_json::json!(3));
    assert!(!gallery.join("3.jpg").exists());
    assert_eq!(task.state, 1, "task.py:206 compares, it does not assign");

    for marker in [".xehdone", "scan.zip"] {
        let mut task = prepared_task(root.path(), &format!("guid-{marker}"), "scan", 2, |_| {});
        let gallery = task.get_fpath().unwrap();
        fs::create_dir_all(&gallery).unwrap();
        fs::write(gallery.join("1.jpg"), b"").unwrap();
        if marker == ".xehdone" {
            fs::write(gallery.join(marker), b"").unwrap();
        } else {
            fs::write(root.path().join("downloads").join("scan.zip"), b"").unwrap();
        }
        task.scan_downloaded().unwrap();
        assert_eq!(task.flist_done().len(), 2, "{marker}");
        assert_eq!(task.meta["finished"], serde_json::json!(2));
        assert_eq!(task.state, 1);
        assert!(
            gallery.join("1.jpg").exists(),
            "{marker} keeps zero-byte files"
        );
        fs::remove_file(root.path().join("downloads").join("scan.zip")).ok();
    }
}

#[test]
fn duplicate_images_copy_when_done_and_register_when_pending() {
    let root = tempfile::tempdir().expect("root");
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let first_img = format!("https://img.example/{hash}-1-1-1-jpg/full.jpg");
    let second_img = format!("https://img.example/{hash}-1-1-1-jpg/full.jpg");
    let first_page = "https://e-hentai.org/s/abcdef0123/111-1";
    let second_page = "https://e-hentai.org/s/abcdef0123/111-2";
    let first_redirect = format!("https://img.example/{hash}-1-1-1-jpg/1.png");

    let mut task = prepared_task(root.path(), "guid0001", "dups", 3, |_| {});
    task.img_q = Some(std::collections::VecDeque::new());
    task.put_img_queue(&first_img, first_page, "1.png").unwrap();
    assert_eq!(task.img_q.as_ref().unwrap().len(), 1);
    assert_eq!(task.reload_map["0123456789"].1, "1.png");

    // The first file is not on disk yet: register a pending copy.
    task.put_img_queue(&second_img, second_page, "2.png")
        .unwrap();
    assert_eq!(task.img_q.as_ref().unwrap().len(), 1);
    assert_eq!(
        task.duplicate_pairs(),
        vec![(1, vec![2])],
        "pending duplicate is stored under the finished id"
    );
    assert!(task.meta.get("finished").is_none());

    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    let chunks = vec![b"image-bytes".to_vec()];
    let mut chunks = chunks.into_iter();
    task.save_file(&first_img, &first_redirect, move || chunks.next(), || false)
        .unwrap();
    assert_eq!(fs::read(gallery.join("1.jpg")).unwrap(), b"image-bytes");
    assert_eq!(
        fs::read(gallery.join("2.jpg")).unwrap(),
        b"image-bytes",
        "pending duplicate copied when the source finishes"
    );
    assert!(task.duplicate_pairs().is_empty());
    assert_eq!(task.meta["finished"], serde_json::json!(2));

    // A third copy arrives after the file exists: copied immediately.
    let third_page = "https://e-hentai.org/s/abcdef0123/111-3";
    task.put_img_queue(&first_img, third_page, "3.png").unwrap();
    assert_eq!(fs::read(gallery.join("3.jpg")).unwrap(), b"image-bytes");
    assert_eq!(task.meta["finished"], serde_json::json!(3));
    assert_eq!(task.img_q.as_ref().unwrap().len(), 1);

    // Same fid again: the reference downloads as usual and overrides the
    // reload entry; the file lock must stay usable here.
    let files = task.files().clone();
    task.put_img_queue(&first_img, first_page, "1.png").unwrap();
    assert!(
        !files.is_locked(),
        "self duplicate must not leak the file lock"
    );
    assert_eq!(task.img_q.as_ref().unwrap().len(), 2);
    assert_eq!(task.renamed_map.len(), 3);
}

#[test]
fn page_queue_honours_finished_ids_and_retry_uses_the_reload_map() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "pages", 3, |_| {});
    task.page_q = Some(std::collections::VecDeque::new());
    task.set_fid_finished(1);
    let page_done = "https://e-hentai.org/s/abcdef0123/111-1";
    let page_new = "https://e-hentai.org/s/abcdef0123/111-2";
    task.put_page_queue(page_done).unwrap();
    assert!(task.page_q.as_ref().unwrap().is_empty());
    task.put_page_queue(page_new).unwrap();
    assert_eq!(
        task.page_q.as_ref().unwrap().front().map(String::as_str),
        Some(page_new)
    );

    task.reload_map
        .insert("0123456789".into(), (page_new.to_string(), "2.png".into()));
    task.put_page_queue_retry(
        "https://img.example/redirect?redirect=https://e-hentai.org/s/0123456789/111-2",
    )
    .unwrap();
    assert!(!task.reload_map.contains_key("0123456789"));
    assert_eq!(task.page_q.as_ref().unwrap().back().unwrap(), page_new);
}

#[test]
fn streaming_save_uses_a_local_stage_and_updates_the_redirect_name() {
    let root = tempfile::tempdir().expect("root");
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let img = format!("https://img.example/{hash}-1-1-1-jpg/full.jpg");
    let page = "https://e-hentai.org/s/abcdef0123/111-1";
    let mut task = prepared_task(root.path(), "guid0001", "stream", 2, |_| {});
    task.img_q = Some(std::collections::VecDeque::new());
    task.put_img_queue(&img, page, "1.png").unwrap();
    let files = task.files().clone();
    let gallery = task.get_fpath().unwrap();
    let mut chunks = vec![b"abc".to_vec(), b"def".to_vec()].into_iter();
    let mut saw_unlocked = false;
    let redirect = format!("https://img.example/full/{hash}-1-1-1-jpg/real-name.jpg?token=1");
    let result = task
        .save_file(
            &img,
            &redirect,
            || {
                saw_unlocked |= !files.is_locked();
                chunks.next()
            },
            || false,
        )
        .unwrap()
        .expect("published");
    assert_eq!(fs::read(&result).unwrap(), b"abcdef");
    assert!(
        saw_unlocked,
        "the stream must run without the task file lock"
    );
    assert_eq!(task.reload_map["0123456789"].1, "real-name.jpg");
    assert_eq!(task.meta["finished"], serde_json::json!(1));
    let leftovers: Vec<_> = fs::read_dir(&gallery)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("xehentai-"))
        .collect();
    assert!(leftovers.is_empty());

    // Existing non-empty file wins without consuming the stream.
    let mut consumed = false;
    let existing = task
        .save_file(
            &img,
            &redirect,
            || {
                consumed = true;
                Some(b"new".to_vec())
            },
            || false,
        )
        .unwrap()
        .unwrap();
    assert_eq!(fs::read(existing).unwrap(), b"abcdef");
    assert!(!consumed);
    assert_eq!(task.meta["finished"], serde_json::json!(1));

    // A zero-byte file is replaced.
    fs::write(gallery.join("1.jpg"), b"").unwrap();
    let mut chunks = vec![b"replaced".to_vec()].into_iter();
    let replaced = task
        .save_file(&img, &img, move || chunks.next(), || false)
        .unwrap()
        .unwrap();
    assert_eq!(fs::read(replaced).unwrap(), b"replaced");
}

#[test]
fn streaming_cancellation_removes_the_stage_and_changes_nothing() {
    let root = tempfile::tempdir().expect("root");
    let hash = "0123456789abcdef0123456789abcdef01234567";
    let img = format!("https://img.example/{hash}-1-1-1-jpg/full.jpg");
    let page = "https://e-hentai.org/s/abcdef0123/111-1";
    let mut task = prepared_task(root.path(), "guid0001", "cancel", 2, |_| {});
    task.img_q = Some(std::collections::VecDeque::new());
    task.put_img_queue(&img, page, "1.png").unwrap();
    let gallery = task.get_fpath().unwrap();
    let calls = std::cell::Cell::new(0usize);
    let mut chunks = vec![b"a".to_vec(), b"b".to_vec()].into_iter();
    let result = task
        .save_file(
            &img,
            &img,
            || {
                calls.set(calls.get() + 1);
                chunks.next()
            },
            || calls.get() >= 1,
        )
        .unwrap();
    assert!(result.is_none());
    assert!(!gallery.join("1.jpg").exists());
    assert!(task.meta.get("finished").is_none());
    let leftovers: Vec<_> = fs::read_dir(&gallery)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("xehentai-"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn rename_fname_matches_reference_extension_and_conflict_rules() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "rename", 4, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    for fid in 1..=4 {
        fs::write(gallery.join(format!("{fid}.jpg")), b"x").unwrap();
    }
    task.renamed_map
        .insert(crate::task::FidKey::Int(1), "1.jpg".into());
    task.renamed_map
        .insert(crate::task::FidKey::Int(2), "2.png".into());
    task.renamed_map
        .insert(crate::task::FidKey::Int(3), "3.png".into());
    task.renamed_map
        .insert(crate::task::FidKey::Int(4), "4.jpg".into());
    // fid 2 target exists, so it gets a (1) suffix; fid 3 collides with
    // 3.png twice? No: only 3.jpg exists, so it simply renames.
    fs::write(gallery.join("2.png"), b"old").unwrap();
    let errors = task.rename_fname().unwrap();
    assert!(errors.is_empty());
    assert!(gallery.join("2(1).png").exists());
    assert!(!gallery.join("2.jpg").exists(), "png entry is renamed away");
    assert!(gallery.join("3.png").exists());
    assert!(gallery.join("1.jpg").exists(), "jpg entry skipped");
    assert!(gallery.join("4.jpg").exists());
    assert_eq!(
        task.renamed_map.get(&crate::task::FidKey::Text("2".into())),
        Some(&"2(1).png".to_string())
    );
    assert!(!gallery.join(".xehdone").exists());
}

#[test]
fn rename_fname_keeps_the_reference_collision_crash() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "crash", 2, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"x").unwrap();
    fs::write(gallery.join("1.png"), b"x").unwrap();
    fs::write(gallery.join("1(1).png"), b"x").unwrap();
    task.renamed_map
        .insert(crate::task::FidKey::Int(1), "1.png".into());
    let error = task.rename_fname().expect_err("reference TypeError path");
    assert!(
        matches!(error, crate::task::TaskError::NameCollisionBug { .. }),
        "{error}"
    );
    assert!(gallery.join("1.jpg").exists(), "no file lost");
    assert!(gallery.join("1.png").exists());
}

#[test]
fn rename_ori_moves_conflicts_to_the_temporary_directory() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "ori", 2, |config| {
        config.rename_ori = true;
    });
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"auto").unwrap();
    fs::write(gallery.join("cover.jpg"), b"original-name").unwrap();
    task.renamed_map
        .insert(crate::task::FidKey::Int(1), "cover.jpg".into());
    let errors = task.rename_fname().unwrap();
    assert!(errors.is_empty());
    assert_eq!(fs::read(gallery.join("cover.jpg")).unwrap(), b"auto");
    assert_eq!(
        fs::read(gallery.join("-xeh-conflict-").join("cover.jpg")).unwrap(),
        b"original-name"
    );
    assert_eq!(
        task.renamed_map.get(&crate::task::FidKey::Text("1".into())),
        Some(&"cover.jpg".to_string())
    );
    assert!(!gallery.join(".xehdone").exists());
}

#[test]
fn rename_pads_thousands_writes_the_done_marker_and_reads_string_keys() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "pad", 1000, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    assert_eq!(task.get_fidpad(7, "jpg").unwrap(), "0007.jpg");
    fs::write(gallery.join("0007.jpg"), b"x").unwrap();
    task.renamed_map
        .insert(crate::task::FidKey::Text("7".into()), "page-7.png".into());
    let errors = task.rename_fname().unwrap();
    assert!(errors.is_empty());
    assert!(gallery.join("0007.png").exists());
    assert!(!gallery.join(".xehdone").exists(), "count is not the total");

    let mut done = prepared_task(root.path(), "guid0002", "done", 2, |_| {});
    let done_gallery = done.get_fpath().unwrap();
    fs::create_dir_all(&done_gallery).unwrap();
    fs::write(done_gallery.join("1.jpg"), b"x").unwrap();
    fs::write(done_gallery.join("2.jpg"), b"x").unwrap();
    done.renamed_map
        .insert(crate::task::FidKey::Text("1".into()), "1.png".into());
    done.renamed_map
        .insert(crate::task::FidKey::Text("2".into()), "2.png".into());
    done.rename_fname().unwrap();
    assert!(done_gallery.join(".xehdone").exists(), "count equals total");

    let mut bad = prepared_task(root.path(), "guid0003", "badkey", 2, |_| {});
    bad.renamed_map
        .insert(crate::task::FidKey::Text("abc".into()), "x.png".into());
    assert!(matches!(
        bad.rename_fname(),
        Err(crate::task::TaskError::ParseInt(_))
    ));
}

#[test]
fn archive_uses_store_sorted_entries_and_the_reference_comment() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "archive", 2, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("b.jpg"), b"bbb").unwrap();
    fs::write(gallery.join("a.jpg"), b"aaa").unwrap();
    fs::write(gallery.join(".xehdone"), b"").unwrap();
    let arc = task.make_archive(false).unwrap();
    assert!(arc.ends_with("archive.zip"));
    assert!(gallery.exists());

    let mut archive = zip::ZipArchive::new(fs::File::open(&arc).unwrap()).unwrap();
    assert_eq!(
        archive.comment(),
        b"xeHentai Archiver v2.023\nTitle:archive\nOriginal URL:https://e-hentai.org/g/123456/0123456789/"
    );
    assert_eq!(archive.len(), 3);
    let names: Vec<String> = (0..archive.len())
        .map(|index| archive.by_index(index).unwrap().name().to_string())
        .collect();
    assert_eq!(names, vec![".xehdone", "a.jpg", "b.jpg"]);
    for index in 0..archive.len() {
        let entry = archive.by_index(index).unwrap();
        assert_eq!(entry.compression(), zip::CompressionMethod::Stored);
    }
    let contents: Vec<Vec<u8>> = (0..archive.len())
        .map(|index| {
            use std::io::Read;
            let mut entry = archive.by_index(index).unwrap();
            let mut buffer = Vec::new();
            entry.read_to_end(&mut buffer).unwrap();
            buffer
        })
        .collect();
    assert_eq!(
        contents,
        vec![b"".to_vec(), b"aaa".to_vec(), b"bbb".to_vec()]
    );

    // Existing archive returns early and the lock stays usable.
    let files = task.files().clone();
    let again = task.make_archive(true).unwrap();
    assert_eq!(again, arc);
    assert!(!files.is_locked());
    assert!(gallery.exists(), "early return does not delete the images");

    fs::remove_file(&arc).unwrap();
    let first = task.make_archive(true).unwrap();
    assert_eq!(first, arc);
    assert!(
        !gallery.exists(),
        "remove=true deletes the gallery directory"
    );
}

#[test]
fn archive_failure_keeps_images_and_releases_the_lock() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "badarchive", 1, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"data").unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc/hostname", gallery.join("link.jpg")).unwrap();
        let error = task.make_archive(true).expect_err("symlink refused");
        assert!(
            matches!(error, crate::task::TaskError::UnsafePath { .. }),
            "{error}"
        );
        assert_eq!(fs::read(gallery.join("1.jpg")).unwrap(), b"data");
        assert!(!gallery.join("1.jpg.zip").exists());
        assert!(
            !root
                .path()
                .join("downloads")
                .join("badarchive.zip")
                .exists()
        );
        assert!(!task.files().is_locked());
        fs::remove_file(gallery.join("link.jpg")).unwrap();
    }
}

#[test]
fn archive_permission_failure_keeps_images_and_releases_the_lock() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().expect("root");
        let mut task = prepared_task(root.path(), "guid0001", "permarchive", 1, |_| {});
        let gallery = task.get_fpath().unwrap();
        fs::create_dir_all(&gallery).unwrap();
        let image = gallery.join("1.jpg");
        fs::write(&image, b"data").unwrap();
        fs::set_permissions(&image, fs::Permissions::from_mode(0o000)).unwrap();
        let error = task.make_archive(true).expect_err("unreadable entry");
        assert!(
            matches!(
                error,
                crate::task::TaskError::Io { .. } | crate::task::TaskError::Zip(_)
            ),
            "{error}"
        );
        assert!(gallery.exists());
        assert!(
            !root
                .path()
                .join("downloads")
                .join("permarchive.zip")
                .exists()
        );
        assert!(!task.files().is_locked());
        fs::set_permissions(&image, fs::Permissions::from_mode(0o644)).unwrap();
    }
}

#[test]
fn path_traversal_symlinks_and_empty_titles_are_refused() {
    let root = tempfile::tempdir().expect("root");
    let task = prepared_task(root.path(), "guid0001", "///", 1, |_| {});
    assert!(matches!(
        task.get_fpath(),
        Err(crate::task::TaskError::UnsafePath { .. })
    ));
    let task = prepared_task(root.path(), "guid0002", "safe", 1, |_| {});
    let downloads = root.path().join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("/etc", downloads.join("safe")).unwrap();
        assert!(matches!(
            task.get_fpath(),
            Err(crate::task::TaskError::UnsafePath { .. })
        ));
        fs::remove_file(downloads.join("safe")).unwrap();
    }
    fs::create_dir_all(downloads.join("safe")).unwrap();
    let mut traversal = prepared_task(root.path(), "guid0003", "safe", 2, |config| {
        config.rename_ori = true;
    });
    traversal
        .renamed_map
        .insert(crate::task::FidKey::Int(1), "..".into());
    assert!(matches!(
        traversal.rename_fname(),
        Err(crate::task::TaskError::UnsafePath { .. })
    ));
}

#[test]
fn deleting_a_task_honours_delete_task_files_safely() {
    let root = tempfile::tempdir().expect("root");
    let mut config = session_config(root.path(), true);
    config.delete_task_files = true;
    let mut app = app_with_config(root.path(), config, &["guid0001"]).expect("app");
    let (_, guid) = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    let guid = guid.unwrap();
    {
        let task = app.task_mut(&guid).unwrap();
        task.meta.insert("title".into(), serde_json::json!("gone"));
    }
    let gallery = root.path().join("downloads").join("gone");
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"x").unwrap();
    fs::write(root.path().join("downloads").join("gone.zip"), b"x").unwrap();
    app.pause_task(&guid).unwrap();
    assert_eq!(app.del_task(&guid).unwrap(), (0, Some(String::new())));
    assert!(!gallery.exists());
    assert!(!root.path().join("downloads").join("gone.zip").exists());
}

#[test]
fn zip64_is_written_for_more_than_65535_entries() {
    let root = tempfile::tempdir().expect("root");
    let mut task = prepared_task(root.path(), "guid0001", "big", 0, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    for index in 0..65_537 {
        fs::write(gallery.join(format!("{index}.jpg")), b"").unwrap();
    }
    let arc = task.make_archive(false).expect("archive");
    let mut archive = zip::ZipArchive::new(fs::File::open(&arc).unwrap()).unwrap();
    assert_eq!(archive.len(), 65_537);
    assert_eq!(
        archive.by_index(0).unwrap().compression(),
        zip::CompressionMethod::Stored
    );
    let bytes = fs::read(&arc).unwrap();
    assert!(
        bytes.windows(4).any(|window| window == b"PK\x06\x06"),
        "ZIP64 EOCD record"
    );
    assert!(
        bytes.windows(4).any(|window| window == b"PK\x06\x07"),
        "ZIP64 locator"
    );
    drop(archive);
    fs::remove_dir_all(&gallery).unwrap();
    fs::remove_file(&arc).unwrap();
}

// --- M2 differential scenarios (tools/compat.py drives these) --------------

fn m2_logger() -> crate::util::logger::Logger {
    crate::util::logger::Logger::with_output(Box::new(Vec::new()), false)
}

fn m2_config(dir: &Path) -> Config {
    let mut config = Config::default();
    config.dir = dir.join("downloads").to_string_lossy().into_owned();
    config.save_tasks = true;
    config.log_path = dir.join("eh.log").to_string_lossy().into_owned();
    config.log_verbose = 2;
    config
}

fn m2_app(dir: &Path, guids: &[&str]) -> crate::core::App {
    crate::core::App::with_guid_source(
        m2_config(dir),
        m2_logger(),
        dir,
        dir,
        Box::new(crate::core::FixedGuids::new(
            guids.iter().map(|guid| guid.to_string()),
        )),
    )
    .expect("m2 app")
}

fn m2_scenario(root: &Path, name: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn m2_norm(value: &str, root: &Path) -> String {
    let prefix = root.to_string_lossy();
    match value.strip_prefix(prefix.as_ref()) {
        Some(rest) => format!("<ROOT>{rest}"),
        None => value.to_string(),
    }
}

fn m2_cfg_obs(config: &crate::task::TaskConfig, root: &Path) -> serde_json::Value {
    let mut value = serde_json::to_value(config).unwrap();
    if let Some(dir) = value.get_mut("dir")
        && let Some(text) = dir.as_str()
    {
        *dir = serde_json::json!(m2_norm(text, root));
    }
    value
}

fn m2_task_obs(task: &crate::task::Task, root: &Path) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert("url".into(), serde_json::json!(task.url));
    if let (Some(gid), Some(sethash)) = (&task.gid, &task.sethash) {
        out.insert("gid".into(), serde_json::json!(gid));
        out.insert("sethash".into(), serde_json::json!(sethash));
    }
    out.insert("failcode".into(), serde_json::json!(task.failcode));
    out.insert("state".into(), serde_json::json!(task.state));
    out.insert("guid".into(), serde_json::json!(task.guid));
    out.insert("config".into(), m2_cfg_obs(&task.config, root));
    out.insert("meta".into(), serde_json::Value::Object(task.meta.clone()));
    out.insert(
        "reload".into(),
        serde_json::json!(
            task.reload_map
                .iter()
                .map(|(key, (url, name))| serde_json::json!([key, [url, name]]))
                .collect::<Vec<_>>()
        ),
    );
    out.insert(
        "duplicate".into(),
        serde_json::json!(
            task.duplicate_pairs()
                .into_iter()
                .map(|(fid, copies)| serde_json::json!([fid, copies]))
                .collect::<Vec<_>>()
        ),
    );
    out.insert(
        "renamed".into(),
        serde_json::json!(
            task.renamed_pairs()
                .into_iter()
                .map(|(key, name)| serde_json::json!([key, name]))
                .collect::<Vec<_>>()
        ),
    );
    out.insert(
        "img_q".into(),
        match &task.img_q {
            Some(queue) => serde_json::json!(queue.iter().collect::<Vec<_>>()),
            None => serde_json::Value::Null,
        },
    );
    out.insert(
        "page_q".into(),
        match &task.page_q {
            Some(queue) => serde_json::json!(queue.iter().collect::<Vec<_>>()),
            None => serde_json::Value::Null,
        },
    );
    out.insert(
        "flist".into(),
        serde_json::json!(task.flist_done().iter().collect::<Vec<_>>()),
    );
    serde_json::Value::Object(out)
}

fn m2_app_obs(app: &crate::core::App, root: &Path) -> serde_json::Value {
    serde_json::json!({
        "tasks": app
            .all_task_guids()
            .iter()
            .map(|guid| serde_json::json!([
                guid,
                m2_task_obs(app.task(guid).expect("task"), root)
            ]))
            .collect::<Vec<_>>(),
        "queue": app.queue().iter().collect::<Vec<_>>(),
        "cookies": app.cookies().iter().map(|(k, v)| serde_json::json!([k, v])).collect::<Vec<_>>(),
        "headers": app.headers().iter().map(|h| serde_json::json!([h.name, h.value])).collect::<Vec<_>>(),
        "has_login": app.has_login(),
    })
}

fn m2_task_error_name(error: &crate::task::TaskError) -> &'static str {
    use crate::task::TaskError;
    match error {
        TaskError::MissingMeta(_) | TaskError::UnknownImage(_) => "KeyError",
        TaskError::MetaType { .. } | TaskError::InvalidConfigValue { .. } => "TypeError",
        TaskError::ParseInt(_) => "ValueError",
        TaskError::InvalidUrl => "IndexError",
        TaskError::QueueNotStarted(_) => "AttributeError",
        TaskError::NameCollisionBug { .. } => "TypeError",
        TaskError::UnsafePath { .. } => "UnsafePathDelta",
        TaskError::Io { .. } => "OSError",
        TaskError::PublishFailed { .. } => "NameError",
        TaskError::Zip(_) => "BadZipFile",
        TaskError::SimulatedSizeExceeded => "RuntimeError",
    }
}

fn m2_app_error_name(error: &crate::core::AppError) -> &'static str {
    use crate::core::{AppError, SessionError};
    match error {
        AppError::Session(SessionError::MissingKey(_)) => "KeyError",
        AppError::Session(SessionError::Shape(_)) => "AttributeError",
        AppError::Session(SessionError::Task(crate::task::TaskLoadError::Missing(_))) => "KeyError",
        AppError::Session(SessionError::Task(_)) => "TypeError",
        AppError::Session(SessionError::TooLarge { .. }) => "SessionTooLarge",
        AppError::Session(SessionError::Unsafe { .. }) => "UnsafePathDelta",
        AppError::Session(SessionError::Json(_)) => "JSONDecodeError",
        AppError::Session(SessionError::Io { .. }) => "OSError",
        AppError::Filter(_) => "RuntimeError",
        AppError::Http(_) => "RequestException",
        AppError::Download(_) => "OSError",
        AppError::State(_) => "RuntimeError",
        AppError::Session(SessionError::Scan(_)) => "KeyError",
        AppError::Task(error) => m2_task_error_name(error),
        AppError::UnknownConfig(_) => "TypeError",
        AppError::Unavailable(_) => "RuntimeError",
    }
}

fn m2_capture<T: serde::Serialize>(result: Result<T, crate::task::TaskError>) -> serde_json::Value {
    match result {
        Ok(value) => serde_json::json!({"ok": true, "value": value}),
        Err(error) => serde_json::json!({"ok": false, "error": m2_task_error_name(&error)}),
    }
}

fn m2_capture_app(
    result: Result<crate::core::App, crate::core::AppError>,
    root: &Path,
) -> serde_json::Value {
    match result {
        Ok(app) => serde_json::json!({"ok": true, "value": m2_app_obs(&app, root)}),
        Err(error) => serde_json::json!({"ok": false, "error": m2_app_error_name(&error)}),
    }
}

fn m2_rename_errors(errors: Vec<crate::task::RenameError>) -> serde_json::Value {
    serde_json::json!(
        errors
            .into_iter()
            .map(|error| serde_json::json!([error.from, error.to, error.message]))
            .collect::<Vec<_>>()
    )
}

fn m2_tree(root: &Path) -> serde_json::Value {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<serde_json::Value>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let name_text = name.to_string_lossy();
            if name_text == "h.json" || name_text == "eh.log" {
                continue;
            }
            let path = entry.path();
            let rel = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            let metadata = fs::symlink_metadata(&path).unwrap();
            if metadata.file_type().is_symlink() {
                out.push(serde_json::json!({
                    "path": rel,
                    "type": "link",
                    "target": fs::read_link(&path).unwrap().to_string_lossy(),
                }));
            } else if metadata.is_dir() {
                out.push(serde_json::json!({"path": rel, "type": "dir"}));
                walk(root, &path, out);
            } else {
                let data = fs::read(&path).unwrap();
                let text = if data.len() <= 4096 {
                    String::from_utf8(data.clone()).ok()
                } else {
                    None
                };
                out.push(serde_json::json!({
                    "path": rel,
                    "type": "file",
                    "size": data.len(),
                    "text": text,
                }));
            }
        }
    }
    let mut entries = Vec::new();
    walk(root, root, &mut entries);
    entries.sort_by(|left, right| {
        left["path"]
            .as_str()
            .unwrap()
            .cmp(right["path"].as_str().unwrap())
    });
    serde_json::Value::Array(entries)
}

fn m2_zip_obs(path: &Path) -> serde_json::Value {
    use std::io::Read;
    let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    let comment = String::from_utf8_lossy(archive.comment()).into_owned();
    let mut entries = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).unwrap();
        let name = entry.name().to_string();
        let compression = match entry.compression() {
            zip::CompressionMethod::Stored => 0,
            _ => -1,
        };
        let size = entry.size();
        let mut data = Vec::new();
        entry.read_to_end(&mut data).unwrap();
        entries.push(serde_json::json!({
            "name": name,
            "compress": compression,
            "size": size,
            "text": String::from_utf8_lossy(&data),
        }));
    }
    serde_json::json!({"comment": comment, "entries": entries})
}

fn m2_zip64_obs(path: &Path) -> serde_json::Value {
    let bytes = fs::read(path).unwrap();
    let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
    let count = archive.len();
    let first = archive.by_index(0).unwrap().name().to_string();
    let mut stored = true;
    for index in 0..count {
        if archive.by_index(index).unwrap().compression() != zip::CompressionMethod::Stored {
            stored = false;
            break;
        }
    }
    serde_json::json!({
        "zip64_eocd": bytes.windows(4).any(|window| window == b"PK\x06\x06"),
        "zip64_locator": bytes.windows(4).any(|window| window == b"PK\x06\x07"),
        "entry_count": count,
        "first_entry": first,
        "stored": stored,
        "comment": String::from_utf8_lossy(archive.comment()),
    })
}

fn m2_prepared_task(
    root: &Path,
    guid: &str,
    title: &str,
    total: i64,
    patch: impl FnOnce(&mut Config),
) -> crate::task::Task {
    let mut config = m2_config(root);
    patch(&mut config);
    let task_config = crate::task::TaskConfig::from_app(&config, &serde_json::Map::new()).unwrap();
    let mut task = crate::task::Task::new(GALLERY_URL, task_config, guid.into(), root).unwrap();
    fs::create_dir_all(root.join("downloads")).unwrap();
    let mut meta = serde_json::Map::new();
    meta.insert("gnname".into(), serde_json::json!(title));
    meta.insert("gjname".into(), serde_json::Value::Null);
    meta.insert("total".into(), serde_json::json!(total));
    task.update_meta(meta).unwrap();
    task
}

fn compat_task_api(inputs: &serde_json::Value) -> serde_json::Value {
    let root = Path::new(inputs["root"].as_str().expect("root"));
    let mut result = serde_json::Map::new();

    let snapshot = m2_scenario(root, "snapshot");
    let mut app = m2_app(&snapshot, &["00000001", "00000002"]);
    let mut kwargs = serde_json::Map::new();
    kwargs.insert("download_thread_cnt".into(), serde_json::json!(9));
    kwargs.insert(
        "dir".into(),
        serde_json::json!(snapshot.join("task-dir").to_string_lossy()),
    );
    kwargs.insert("extra_flag".into(), serde_json::json!("kept"));
    kwargs.insert("extra_number".into(), serde_json::json!(7));
    let (_, first) = app.add_task(GALLERY_URL, kwargs).unwrap();
    let first = first.unwrap();
    let mut patch = serde_json::Map::new();
    patch.insert(
        "dir".into(),
        serde_json::json!(snapshot.join("downloads").to_string_lossy()),
    );
    patch.insert("download_thread_cnt".into(), serde_json::json!(2));
    patch.insert("jpn_title".into(), serde_json::json!(false));
    patch.insert("scan_thread_cnt".into(), serde_json::json!(3));
    patch.insert("proxy".into(), serde_json::json!([]));
    app.update_config(patch).unwrap();
    let (_, second) = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    let second = second.unwrap();
    let first_task = app.task(&first).unwrap();
    let config_value = serde_json::to_value(&first_task.config).unwrap();
    let base_keys: Vec<String> = config_value
        .as_object()
        .unwrap()
        .keys()
        .take(14)
        .cloned()
        .collect();
    let rpc_leaked: Vec<String> = config_value
        .as_object()
        .unwrap()
        .keys()
        .filter(|key| key.starts_with("rpc_") || *key == "daemon" || *key == "save_tasks")
        .cloned()
        .collect();
    result.insert(
        "snapshot".into(),
        serde_json::json!({
            "first": m2_task_obs(first_task, &snapshot),
            "second": m2_task_obs(app.task(&second).unwrap(), &snapshot),
            "queue": app.queue().iter().collect::<Vec<_>>(),
            "base_keys": base_keys,
            "rpc_leaked": rpc_leaked,
        }),
    );

    let states = m2_scenario(root, "states");
    let mut app = m2_app(&states, &["00000001", "00000002", "00000003"]);
    let mut steps: Vec<serde_json::Value> = Vec::new();
    let mut step = |label: &str, outcome: (i64, Option<String>)| {
        steps.push(serde_json::json!([label, outcome.0, outcome.1]));
    };
    step(
        "bad-url",
        app.add_task("https://example.com/nope", serde_json::Map::new())
            .unwrap(),
    );
    step(
        "exhentai",
        app.add_task(EXH_URL, serde_json::Map::new()).unwrap(),
    );
    let added = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    step("valid", added.clone());
    let guid = added.1.clone().unwrap();
    step("pause", app.pause_task(&guid).unwrap());
    step("pause-again", app.pause_task(&guid).unwrap());
    step("resume", app.resume_task(&guid).unwrap());
    step("resume-waiting", app.resume_task(&guid).unwrap());
    step("delete-running", app.del_task(&guid).unwrap());
    step("pause-before-delete", app.pause_task(&guid).unwrap());
    step("delete", app.del_task(&guid).unwrap());
    step("delete-missing", app.del_task(&guid).unwrap());
    step("pause-missing", app.pause_task("missing").unwrap());
    step("resume-missing", app.resume_task("missing").unwrap());
    result.insert(
        "states".into(),
        serde_json::json!({"steps": steps, "final": m2_app_obs(&app, &states)}),
    );

    let resumes = m2_scenario(root, "resumes");
    let mut app = m2_app(&resumes, &["00000001", "00000002"]);
    let finished = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .unwrap()
        .1
        .unwrap();
    app.task_mut(&finished).unwrap().state = 20;
    let finished_resume = app.resume_task(&finished).unwrap();
    let failed = app
        .add_task("https://example.com/nope", serde_json::Map::new())
        .unwrap();
    let failed_guid = app.all_task_guids()[1].clone();
    let failed_resume = app.resume_task(&failed_guid).unwrap();
    result.insert(
        "resumes".into(),
        serde_json::json!({
            "finished": [finished_resume.0, finished_resume.1],
            "finished_state": app.task(&finished).unwrap().state,
            "failed_guid": failed_guid,
            "failed": [failed_resume.0, failed_resume.1],
            "failed_state": app.task(&app.all_task_guids()[1]).unwrap().state,
            "queue": app.queue().iter().collect::<Vec<_>>(),
            "tasks": app.all_task_guids().iter().map(|guid| serde_json::json!([
                guid, m2_task_obs(app.task(guid).unwrap(), &resumes)
            ])).collect::<Vec<_>>(),
            "bad_result": [failed.0, failed.1],
        }),
    );

    let collision = m2_scenario(root, "collision");
    let mut app = m2_app(&collision, &["dupe0001", "dupe0001", "dupe0001"]);
    let first = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    let second = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    app.task_mut("dupe0001").unwrap().state = 20;
    let third = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    result.insert(
        "collision".into(),
        serde_json::json!({
            "first": [first.0, first.1],
            "second": [second.0, second.1],
            "third": [third.0, third.1],
            "tasks": app.all_task_guids().iter().map(|guid| serde_json::json!([
                guid, m2_task_obs(app.task(guid).unwrap(), &collision)
            ])).collect::<Vec<_>>(),
            "queue": app.queue().iter().collect::<Vec<_>>(),
        }),
    );

    let methods = m2_scenario(root, "methods");
    let mut app = m2_app(&methods, &["00000001"]);
    let guid = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .unwrap()
        .1
        .unwrap();
    let task = app.task_mut(&guid).unwrap();
    let mut values = serde_json::Map::new();
    values.insert("base_url".into(), m2_capture(task.base_url()));
    values.insert("mpv_url".into(), m2_capture(Ok(task.mpv_url())));
    values.insert("gid".into(), serde_json::json!(task.gid));
    values.insert("sethash".into(), serde_json::json!(task.sethash));
    values.insert("fidpad".into(), m2_capture(task.get_fidpad(7, "jpg")));
    values.insert(
        "fidpad_png".into(),
        m2_capture(task.get_fidpad(1000, "png")),
    );
    task.meta.insert("total".into(), serde_json::json!(1155));
    values.insert("unfinished".into(), m2_capture(task.get_fid_unfinished()));
    values.insert(
        "imghash".into(),
        m2_capture(crate::task::Task::get_imghash(&format!(
            "https://img.example/{IMAGE_HASH}-123-1-1-jpg/0123456789/sample.jpg"
        ))),
    );
    values.insert(
        "imgfid".into(),
        m2_capture(task.get_imgfid("https://e-hentai.org/fullimg/1/456?x=1")),
    );
    values.insert(
        "imgfid_missing".into(),
        m2_capture(task.get_imgfid("https://example.com/x")),
    );
    task.meta
        .insert("gnname".into(), serde_json::json!("English title"));
    task.meta
        .insert("gjname".into(), serde_json::json!("日本語タイトル"));
    task.update_meta(serde_json::Map::new()).unwrap();
    values.insert("title_jpn".into(), task.meta["title"].clone());
    task.config.jpn_title = false;
    task.meta
        .insert("gjname".into(), serde_json::json!("日本語タイトル"));
    task.meta
        .insert("gnname".into(), serde_json::json!("English title"));
    task.update_meta(serde_json::Map::new()).unwrap();
    values.insert("title_eng".into(), task.meta["title"].clone());
    task.reload_map.insert(
        "0123456789".into(),
        (
            "https://e-hentai.org/s/abcdef0123/111-2".into(),
            "2.png".into(),
        ),
    );
    values.insert("fname".into(), m2_capture(task.get_fname("0123456789")));
    values.insert(
        "fname_missing".into(),
        m2_capture(task.get_fname("ffffffffff")),
    );
    values.insert(
        "set_fail".into(),
        m2_capture({
            task.set_fail(1004);
            Ok(())
        }),
    );
    values.insert(
        "after_fail".into(),
        serde_json::json!([
            task.state,
            task.failcode,
            serde_json::Value::Object(task.meta.clone())
        ]),
    );
    values.insert("migrate".into(), m2_capture(Ok(task.migrate_exhentai())));
    values.insert(
        "after_migrate".into(),
        serde_json::json!([task.url, task.state, task.failcode]),
    );
    values.insert(
        "migrate_again".into(),
        m2_capture(Ok(task.migrate_exhentai())),
    );
    result.insert("methods".into(), serde_json::Value::Object(values));
    let task_config = crate::task::TaskConfig::from_app(&app.cfg, &serde_json::Map::new()).unwrap();
    let bare = crate::task::Task::new("", task_config, "bare0001".into(), &methods).unwrap();
    result.insert(
        "methods_exceptions".into(),
        serde_json::json!({"fpath_missing_title": m2_capture(bare.get_fpath())}),
    );
    serde_json::Value::Object(result)
}

fn m2_wire_tasks(raw: &str, root: &Path) -> serde_json::Value {
    let document: serde_json::Value = serde_json::from_str(raw).unwrap();
    let mut tasks = document["tasks"].as_object().unwrap().clone();
    for task in tasks.values_mut() {
        if let Some(config) = task
            .get_mut("config")
            .and_then(|value| value.as_object_mut())
            && let Some(dir) = config.get("dir").and_then(|value| value.as_str())
        {
            let normalized = m2_norm(dir, root);
            config.insert("dir".into(), serde_json::json!(normalized));
        }
    }
    serde_json::Value::Object(tasks)
}

fn m2_rename_key_count(raw: &str) -> usize {
    let Some(start) = raw.find("\"renamed_map\"") else {
        return 0;
    };
    let rest = &raw[start..];
    let Some(end) = rest.find('}') else {
        return 0;
    };
    rest[..end].matches("\"1\":").count()
}

fn m2_load_directory(directory: &Path) -> serde_json::Value {
    let app = crate::core::App::with_guid_source(
        m2_config(directory),
        m2_logger(),
        directory,
        directory,
        Box::new(crate::core::FixedGuids::new(Vec::new())),
    )
    .expect("migrate app");
    let before = m2_app_obs(&app, directory);
    let mut app = app;
    let save = app.save_session();
    let restarted = crate::core::App::with_guid_source(
        m2_config(directory),
        m2_logger(),
        directory,
        directory,
        Box::new(crate::core::FixedGuids::new(Vec::new())),
    )
    .expect("restart app");
    serde_json::json!({
        "before": before,
        "save": [save.0, save.1],
        "restarted": m2_app_obs(&restarted, directory),
        "tree": m2_tree(directory),
    })
}

fn m2_fixture_load(root: &Path, name: &str, dir: Option<&Path>, twice: bool) -> serde_json::Value {
    let directory = root.join(name);
    let mut config = m2_config(&directory);
    if let Some(dir) = dir {
        config.dir = dir.to_string_lossy().into_owned();
    }
    let build = || {
        crate::core::App::with_guid_source(
            config.clone(),
            m2_logger(),
            &directory,
            &directory,
            Box::new(crate::core::FixedGuids::new(Vec::new())),
        )
        .expect("fixture app")
    };
    let mut app = build();
    let mut result = serde_json::json!({"loaded": m2_app_obs(&app, &directory)});
    if twice {
        app.load_session().expect("second fixture load");
        result["twice"] = m2_app_obs(&app, &directory);
    }
    let save = app.save_session();
    result["save"] = serde_json::json!([save.0, save.1]);
    let restarted = build();
    result["restarted"] = m2_app_obs(&restarted, &directory);
    result["tree"] = m2_tree(&directory);
    result
}

fn compat_session(inputs: &serde_json::Value) -> serde_json::Value {
    let root = Path::new(inputs["root"].as_str().expect("root"));
    let mut result = serde_json::Map::new();

    let roundtrip = m2_scenario(root, "roundtrip");
    let mut app = m2_app(&roundtrip, &["00000001"]);
    let guid = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .unwrap()
        .1
        .unwrap();
    {
        let task = app.task_mut(&guid).unwrap();
        let mut meta = serde_json::Map::new();
        meta.insert("gnname".into(), serde_json::json!("T"));
        meta.insert("gjname".into(), serde_json::Value::Null);
        meta.insert("total".into(), serde_json::json!(3));
        meta.insert(
            "filelist".into(),
            serde_json::json!({"abcdef0123": ["1", "x"]}),
        );
        task.update_meta(meta).unwrap();
        task.config.make_archive = true;
        task.reload_map.insert(
            "abcdef0123".into(),
            (
                "https://e-hentai.org/s/abcdef0123/111-1".into(),
                "1.jpg".into(),
            ),
        );
        task.renamed_map
            .insert(crate::task::FidKey::Int(1), "1.jpg".into());
        task.renamed_map
            .insert(crate::task::FidKey::Text("1".into()), "renamed.jpg".into());
        task.img_q = Some(std::collections::VecDeque::from([
            "https://img.example/a.jpg".to_string(),
        ]));
        task.page_q = Some(std::collections::VecDeque::from([format!(
            "{GALLERY_URL}?p=0"
        )]));
    }
    app.set_cookie("a=1; b=x=y; Path=/; ipb_member_id=7; ipb_pass_hash=h");
    let save = app.save_session();
    let raw = fs::read_to_string(roundtrip.join("h.json")).unwrap();
    let reloaded = m2_app(&roundtrip, &[]);
    let mut roundtrip_obs = serde_json::json!({
        "save": [save.0, save.1],
        "wire_tasks": m2_wire_tasks(&raw, &roundtrip),
        "rename_key_count": m2_rename_key_count(&raw),
        "loaded": m2_app_obs(&reloaded, &roundtrip),
    });
    let mut reloaded = reloaded;
    reloaded.save_session();
    let restarted = m2_app(&roundtrip, &[]);
    roundtrip_obs["restarted"] = m2_app_obs(&restarted, &roundtrip);
    result.insert("roundtrip".into(), roundtrip_obs);

    let save_false = m2_scenario(root, "save_false");
    let mut config = m2_config(&save_false);
    config.save_tasks = false;
    let mut app = crate::core::App::with_guid_source(
        config,
        m2_logger(),
        &save_false,
        &save_false,
        Box::new(crate::core::FixedGuids::new(["00000001".to_string()])),
    )
    .unwrap();
    app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    app.set_cookie("a=1; b=x=y; path=/; ipb_member_id=7; ipb_pass_hash=h");
    let save = app.save_session();
    let raw = fs::read_to_string(save_false.join("h.json")).unwrap();
    let mut reload_config = m2_config(&save_false);
    reload_config.save_tasks = false;
    let loaded = crate::core::App::with_guid_source(
        reload_config,
        m2_logger(),
        &save_false,
        &save_false,
        Box::new(crate::core::FixedGuids::new(Vec::new())),
    )
    .unwrap();
    result.insert(
        "save_false".into(),
        serde_json::json!({
            "save": [save.0, save.1],
            "wire_tasks": m2_wire_tasks(&raw, &save_false),
            "loaded": m2_app_obs(&loaded, &save_false),
        }),
    );

    let cookies = m2_scenario(root, "cookies");
    fs::write(
        cookies.join("h.json"),
        "{\"tasks\":{},\"cookies\":{\"z\":\"1\",\"nw\":\"9\",\"a\":\"2\"}}",
    )
    .unwrap();
    fs::write(cookies.join(".ehentai.cookie"), "1,oldhash\n").unwrap();
    result.insert(
        "cookies".into(),
        m2_app_obs(&m2_app(&cookies, &[]), &cookies),
    );

    let logged_in = m2_scenario(root, "logged_in");
    fs::write(
        logged_in.join("h.json"),
        "{\"tasks\":{},\"cookies\":{\"nw\":\"1\",\"ipb_member_id\":\"9\",\"ipb_pass_hash\":\"newhash\"}}",
    )
    .unwrap();
    fs::write(logged_in.join(".ehentai.cookie"), "1,oldhash\n").unwrap();
    result.insert(
        "logged_in".into(),
        m2_app_obs(&m2_app(&logged_in, &[]), &logged_in),
    );

    let bad = m2_scenario(root, "bad_json");
    fs::write(bad.join("h.json"), "not json at all").unwrap();
    result.insert("bad_json".into(), m2_app_obs(&m2_app(&bad, &[]), &bad));

    for (name, payload) in [
        ("missing_tasks", "{\"cookies\":{}}"),
        ("missing_cookies", "{\"tasks\":{}}"),
    ] {
        let directory = m2_scenario(root, name);
        fs::write(directory.join("h.json"), payload).unwrap();
        let outcome = m2_capture_app(
            crate::core::App::with_guid_source(
                m2_config(&directory),
                m2_logger(),
                &directory,
                &directory,
                Box::new(crate::core::FixedGuids::new(Vec::new())),
            ),
            &directory,
        );
        result.insert(name.into(), outcome);
    }

    let duplicate = m2_scenario(root, "duplicate_load");
    let mut app = m2_app(&duplicate, &["00000001"]);
    let guid = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .unwrap()
        .1
        .unwrap();
    app.task_mut(&guid).unwrap().state = 0;
    app.save_session();
    let mut app = m2_app(&duplicate, &[]);
    let first_queue: Vec<String> = app.queue().iter().cloned().collect();
    app.load_session().unwrap();
    result.insert(
        "duplicate_load".into(),
        serde_json::json!({
            "first_queue": first_queue,
            "queue": app.queue().iter().collect::<Vec<_>>(),
            "tasks": app.all_task_guids(),
        }),
    );

    let state10 = m2_scenario(root, "state10");
    let mut app = m2_app(&state10, &["00000001"]);
    let guid = app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .unwrap()
        .1
        .unwrap();
    {
        let task = app.task_mut(&guid).unwrap();
        let mut meta = serde_json::Map::new();
        meta.insert("gnname".into(), serde_json::json!("T"));
        meta.insert("gjname".into(), serde_json::Value::Null);
        meta.insert("total".into(), serde_json::json!(3));
        meta.insert(
            "filelist".into(),
            serde_json::json!({"abcdef0123": ["1", "x"]}),
        );
        task.update_meta(meta).unwrap();
        task.state = 10;
        task.page_q = Some(std::collections::VecDeque::from([format!(
            "{GALLERY_URL}?p=0"
        )]));
    }
    let gallery = state10.join("downloads").join("T");
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"x").unwrap();
    fs::write(gallery.join("2.jpg"), b"").unwrap();
    app.save_session();
    let reloaded = m2_app(&state10, &[]);
    result.insert(
        "state10".into(),
        serde_json::json!({
            "loaded": m2_app_obs(&reloaded, &state10),
            "tree": m2_tree(&state10),
        }),
    );

    for name in ["history", "updated", "multi", "subdir"] {
        result.insert(name.into(), m2_fixture_load(root, name, None, false));
    }
    let external = root.join("external");
    result.insert(
        "external".into(),
        m2_fixture_load(root, "external", Some(&external), false),
    );
    for name in ["isolation", "isolation_legacy"] {
        result.insert(name.into(), m2_fixture_load(root, name, None, true));
    }

    let migrate_dir = Path::new(inputs["migrate_dir"].as_str().expect("migrate dir"));
    result.insert("migrate".into(), m2_load_directory(migrate_dir));
    serde_json::Value::Object(result)
}

fn compat_files(inputs: &serde_json::Value) -> serde_json::Value {
    let root = Path::new(inputs["root"].as_str().expect("root"));
    let mut result = serde_json::Map::new();
    let image_hash = "0123456789abcdef0123456789abcdef01234567";
    let first_img = format!("https://img.example/{image_hash}-1-1-1-jpg/full.jpg");
    let first_page = "https://e-hentai.org/s/abcdef0123/111-1";
    let second_page = "https://e-hentai.org/s/abcdef0123/111-2";
    let third_page = "https://e-hentai.org/s/abcdef0123/111-3";
    let first_redirect = format!("https://img.example/{image_hash}-1-1-1-jpg/1.png");

    let scan_dir = m2_scenario(root, "scan");
    let mut task = m2_prepared_task(&scan_dir, "f9000000", "scan", 4, |config| {
        config.download_range = Some(vec![[2, 3]]);
    });
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"one").unwrap();
    fs::write(gallery.join("2.jpg"), b"two").unwrap();
    fs::write(gallery.join("3.jpg"), b"").unwrap();
    fs::write(gallery.join("4.jpg"), b"four").unwrap();
    task.scan_downloaded().unwrap();
    result.insert(
        "scan".into(),
        serde_json::json!({
            "flist": task.flist_done().iter().collect::<Vec<_>>(),
            "finished": task.meta.get("finished"),
            "state": task.state,
            "tree": m2_tree(&scan_dir),
        }),
    );
    let mut markers = serde_json::Map::new();
    for marker in [".xehdone", "zip"] {
        let dir = m2_scenario(root, &format!("scan-{marker}"));
        let mut marker_task =
            m2_prepared_task(&dir, "f9000000", &format!("scan-{marker}"), 2, |_| {});
        let marker_gallery = marker_task.get_fpath().unwrap();
        fs::create_dir_all(&marker_gallery).unwrap();
        fs::write(marker_gallery.join("1.jpg"), b"").unwrap();
        if marker == ".xehdone" {
            fs::write(marker_gallery.join(marker), b"").unwrap();
        } else {
            fs::write(
                dir.join("downloads").join(format!("scan-{marker}.zip")),
                b"",
            )
            .unwrap();
        }
        marker_task.scan_downloaded().unwrap();
        markers.insert(
            marker.into(),
            serde_json::json!({
                "flist": marker_task.flist_done().iter().collect::<Vec<_>>(),
                "finished": marker_task.meta.get("finished"),
                "state": marker_task.state,
            }),
        );
    }
    result.insert("scan_markers".into(), serde_json::Value::Object(markers));

    let dup_dir = m2_scenario(root, "duplicates");
    let mut task = m2_prepared_task(&dup_dir, "f9000000", "dups", 3, |_| {});
    task.img_q = Some(std::collections::VecDeque::new());
    task.page_q = Some(std::collections::VecDeque::new());
    task.put_img_queue(&first_img, first_page, "1.png").unwrap();
    task.put_img_queue(&first_img, second_page, "2.png")
        .unwrap();
    let pending = serde_json::json!({
        "img_q": task.img_q.as_ref().unwrap().iter().collect::<Vec<_>>(),
        "duplicate": task.duplicate_pairs().into_iter().map(|(fid, copies)| serde_json::json!([fid, copies])).collect::<Vec<_>>(),
        "renamed": task.renamed_pairs().into_iter().map(|(key, name)| serde_json::json!([key, name])).collect::<Vec<_>>(),
        "finished": task.meta.get("finished"),
    });
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    let mut chunks = vec![b"image-bytes".to_vec()].into_iter();
    task.save_file(&first_img, &first_redirect, move || chunks.next(), || false)
        .unwrap();
    let after_copy = serde_json::json!({
        "duplicate": task.duplicate_pairs().into_iter().map(|(fid, copies)| serde_json::json!([fid, copies])).collect::<Vec<_>>(),
        "finished": task.meta.get("finished"),
        "tree": m2_tree(&dup_dir),
    });
    task.put_img_queue(&first_img, third_page, "3.png").unwrap();
    let after_third = serde_json::json!({
        "finished": task.meta.get("finished"),
        "img_q": task.img_q.as_ref().unwrap().iter().collect::<Vec<_>>(),
        "tree": m2_tree(&dup_dir),
    });
    task.put_img_queue(&first_img, first_page, "1.png").unwrap();
    let after_self = serde_json::json!({
        "img_q": task.img_q.as_ref().unwrap().iter().collect::<Vec<_>>(),
        "renamed": task.renamed_pairs().into_iter().map(|(key, name)| serde_json::json!([key, name])).collect::<Vec<_>>(),
        "reload": task.reload_map.iter().map(|(key, (url, name))| serde_json::json!([key, [url, name]])).collect::<Vec<_>>(),
    });
    result.insert(
        "duplicates".into(),
        serde_json::json!({
            "pending": pending,
            "after_copy": after_copy,
            "after_third": after_third,
            "after_self": after_self,
            "state": m2_task_obs(&task, &dup_dir),
        }),
    );

    let pages_dir = m2_scenario(root, "pages");
    let mut task = m2_prepared_task(&pages_dir, "f9000000", "pages", 3, |_| {});
    task.page_q = Some(std::collections::VecDeque::new());
    task.set_fid_finished(1);
    task.put_page_queue("https://e-hentai.org/s/abcdef0123/111-1")
        .unwrap();
    task.put_page_queue("https://e-hentai.org/s/abcdef0123/111-2")
        .unwrap();
    task.reload_map.insert(
        "0123456789".into(),
        (
            "https://e-hentai.org/s/abcdef0123/111-2".into(),
            "2.png".into(),
        ),
    );
    let queued: Vec<String> = task.page_q.as_ref().unwrap().iter().cloned().collect();
    let retry = m2_capture(task.put_page_queue_retry(
        "https://img.example/redirect?redirect=https://e-hentai.org/s/0123456789/111-2",
    ));
    result.insert(
        "pages".into(),
        serde_json::json!({
            "queued": queued,
            "retry": retry,
            "queue": task.page_q.as_ref().unwrap().iter().collect::<Vec<_>>(),
            "reload": task.reload_map.iter().map(|(key, (url, name))| serde_json::json!([key, [url, name]])).collect::<Vec<_>>(),
        }),
    );

    let save_dir = m2_scenario(root, "save");
    let mut task = m2_prepared_task(&save_dir, "f9000000", "stream", 2, |_| {});
    task.img_q = Some(std::collections::VecDeque::new());
    task.page_q = Some(std::collections::VecDeque::new());
    task.put_img_queue(&first_img, first_page, "1.png").unwrap();
    let redirect = format!("https://img.example/full/{image_hash}-1-1-1-jpg/real-name.jpg?token=1");
    let mut chunks = vec![b"abc".to_vec(), b"def".to_vec()].into_iter();
    let saved = task
        .save_file(&first_img, &redirect, move || chunks.next(), || false)
        .unwrap();
    let first_save = serde_json::json!({
        "saved": saved.is_some(),
        "reload": task.reload_map.iter().map(|(key, (url, name))| serde_json::json!([key, [url, name]])).collect::<Vec<_>>(),
        "finished": task.meta.get("finished"),
        "tree": m2_tree(&save_dir),
    });
    let consumed = std::cell::Cell::new(false);
    let existing = task
        .save_file(
            &first_img,
            &redirect,
            || {
                consumed.set(true);
                Some(b"new".to_vec())
            },
            || false,
        )
        .unwrap();
    let second_save = serde_json::json!({
        "existing": existing.is_some(),
        "consumed": consumed.get(),
        "tree": m2_tree(&save_dir),
    });
    let gallery = task.get_fpath().unwrap();
    fs::write(gallery.join("1.jpg"), b"").unwrap();
    let mut chunks = vec![b"replaced".to_vec()].into_iter();
    let replaced = task
        .save_file(&first_img, &redirect, move || chunks.next(), || false)
        .unwrap();
    let third_save = serde_json::json!({
        "replaced": replaced.is_some(),
        "text": fs::read_to_string(gallery.join("1.jpg")).unwrap(),
    });
    let cancel_dir = m2_scenario(root, "cancel");
    let mut cancel_task = m2_prepared_task(&cancel_dir, "f9000000", "cancel", 2, |_| {});
    cancel_task.img_q = Some(std::collections::VecDeque::new());
    cancel_task.page_q = Some(std::collections::VecDeque::new());
    cancel_task
        .put_img_queue(&first_img, first_page, "1.png")
        .unwrap();
    let calls = std::cell::Cell::new(0usize);
    let mut chunks = vec![b"a".to_vec(), b"b".to_vec()].into_iter();
    let cancelled = cancel_task
        .save_file(
            &first_img,
            &first_img,
            || {
                calls.set(calls.get() + 1);
                chunks.next()
            },
            || calls.get() >= 1,
        )
        .unwrap();
    result.insert(
        "save".into(),
        serde_json::json!({
            "first": first_save,
            "second": second_save,
            "third": third_save,
            "cancel": {
                "result": cancelled.is_some(),
                "finished": cancel_task.meta.get("finished"),
                "tree": m2_tree(&cancel_dir),
            },
        }),
    );

    let naming_dir = m2_scenario(root, "naming");
    let mut task = m2_prepared_task(&naming_dir, "f9000000", "rename", 5, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    for fid in 1..=5 {
        fs::write(gallery.join(format!("{fid}.jpg")), b"x").unwrap();
    }
    task.renamed_map
        .insert(crate::task::FidKey::Int(1), "1.jpg".into());
    task.renamed_map
        .insert(crate::task::FidKey::Int(2), "2.png".into());
    task.renamed_map
        .insert(crate::task::FidKey::Int(3), "3.png".into());
    task.renamed_map
        .insert(crate::task::FidKey::Int(4), "4.jpg".into());
    task.renamed_map
        .insert(crate::task::FidKey::Int(5), "noext".into());
    fs::write(gallery.join("2.png"), b"old").unwrap();
    let errors = task.rename_fname().unwrap();
    result.insert(
        "naming".into(),
        serde_json::json!({
            "errors": m2_rename_errors(errors),
            "renamed": task.renamed_pairs().into_iter().map(|(key, name)| serde_json::json!([key, name])).collect::<Vec<_>>(),
            "tree": m2_tree(&naming_dir),
        }),
    );

    let crash_dir = m2_scenario(root, "crash");
    let mut task = m2_prepared_task(&crash_dir, "f9000000", "crash", 2, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"x").unwrap();
    fs::write(gallery.join("1.png"), b"x").unwrap();
    fs::write(gallery.join("1(1).png"), b"x").unwrap();
    task.renamed_map
        .insert(crate::task::FidKey::Int(1), "1.png".into());
    result.insert(
        "rename_crash".into(),
        serde_json::json!({
            "crash": m2_capture(task.rename_fname().map(m2_rename_errors)),
            "tree": m2_tree(&crash_dir),
        }),
    );

    let ori_dir = m2_scenario(root, "ori");
    let mut task = m2_prepared_task(&ori_dir, "f9000000", "ori", 2, |config| {
        config.rename_ori = true;
    });
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"auto").unwrap();
    fs::write(gallery.join("cover.jpg"), b"original-name").unwrap();
    task.renamed_map
        .insert(crate::task::FidKey::Int(1), "cover.jpg".into());
    let errors = task.rename_fname().unwrap();
    result.insert(
        "rename_ori".into(),
        serde_json::json!({
            "errors": m2_rename_errors(errors),
            "renamed": task.renamed_pairs().into_iter().map(|(key, name)| serde_json::json!([key, name])).collect::<Vec<_>>(),
            "tree": m2_tree(&ori_dir),
        }),
    );

    let pad_dir = m2_scenario(root, "pad");
    let mut task = m2_prepared_task(&pad_dir, "f9000000", "pad", 1000, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("0007.jpg"), b"x").unwrap();
    task.renamed_map
        .insert(crate::task::FidKey::Text("7".into()), "page-7.png".into());
    let fidpad = task.get_fidpad(7, "jpg").unwrap();
    let errors = task.rename_fname().unwrap();
    result.insert(
        "padding".into(),
        serde_json::json!({
            "fidpad": fidpad,
            "errors": m2_rename_errors(errors),
            "tree": m2_tree(&pad_dir),
        }),
    );

    let done_dir = m2_scenario(root, "done");
    let mut task = m2_prepared_task(&done_dir, "f9000000", "done", 2, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"x").unwrap();
    fs::write(gallery.join("2.jpg"), b"x").unwrap();
    task.renamed_map
        .insert(crate::task::FidKey::Text("1".into()), "1.png".into());
    task.renamed_map
        .insert(crate::task::FidKey::Text("2".into()), "2.png".into());
    let errors = task.rename_fname().unwrap();
    result.insert(
        "done_marker".into(),
        serde_json::json!({
            "errors": m2_rename_errors(errors),
            "tree": m2_tree(&done_dir),
        }),
    );

    let archive_dir = m2_scenario(root, "archive");
    let mut task = m2_prepared_task(&archive_dir, "f9000000", "archive", 2, |config| {
        config.make_archive = false;
    });
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("b.jpg"), b"bbb").unwrap();
    fs::write(gallery.join("a.jpg"), b"aaa").unwrap();
    fs::write(gallery.join(".xehdone"), b"").unwrap();
    fs::create_dir_all(gallery.join("sub")).unwrap();
    fs::write(gallery.join("sub").join("inner.txt"), b"inner").unwrap();
    let arc = task.make_archive(false).unwrap();
    let again = task.make_archive(true).unwrap();
    let existing = serde_json::json!({
        "arc": again.strip_prefix(&archive_dir).unwrap().to_string_lossy().replace('\\', "/"),
        "again": again.strip_prefix(&archive_dir).unwrap().to_string_lossy().replace('\\', "/"),
        "tree": m2_tree(&archive_dir),
        "archive": m2_zip_obs(&arc),
    });
    fs::remove_file(&arc).unwrap();
    let made = task.make_archive(true).unwrap();
    result.insert(
        "archive".into(),
        serde_json::json!({
            "first": existing,
            "made": made.strip_prefix(&archive_dir).unwrap().to_string_lossy().replace('\\', "/"),
            "tree": m2_tree(&archive_dir),
        }),
    );

    let big_dir = m2_scenario(root, "zip64");
    let mut task = m2_prepared_task(&big_dir, "f9000000", "big", 0, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    for index in 0..65_537 {
        fs::write(gallery.join(format!("{index}.jpg")), b"").unwrap();
    }
    let arc = task.make_archive(false).unwrap();
    result.insert("zip64".into(), m2_zip64_obs(&arc));
    fs::remove_dir_all(&gallery).unwrap();
    fs::remove_file(&arc).unwrap();

    let link_dir = m2_scenario(root, "archive_link");
    let mut task = m2_prepared_task(&link_dir, "f9000000", "archive_link", 1, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(link_dir.join("outside.txt"), b"outside-data").unwrap();
    fs::write(gallery.join("1.jpg"), b"inside").unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(link_dir.join("outside.txt"), gallery.join("link.jpg")).unwrap();
    let archive_link = m2_capture(task.make_archive(false).map(|path| m2_zip_obs(&path)));
    result.insert(
        "archive_symlink".into(),
        serde_json::json!({"oracle": archive_link, "tree": m2_tree(&link_dir)}),
    );

    let path_dir = m2_scenario(root, "path");
    let mut task = m2_prepared_task(&path_dir, "f9000000", "///", 1, |config| {
        config.rename_ori = true;
    });
    fs::write(path_dir.join("downloads").join("1.jpg"), b"x").unwrap();
    task.renamed_map
        .insert(crate::task::FidKey::Int(1), "x.png".into());
    result.insert(
        "path_safety".into(),
        serde_json::json!({
            "oracle": m2_capture(task.rename_fname().map(m2_rename_errors)),
            "tree": m2_tree(&path_dir),
        }),
    );
    serde_json::Value::Object(result)
}

// --- M3: local proxy protocol and transport proofs -------------------------

#[cfg(test)]
mod m3_transport {
    use std::net::SocketAddr;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use rcgen::{BasicConstraints, CertificateParams, IsCa, Issuer, KeyPair};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};
    use tokio_rustls::TlsAcceptor;

    use crate::proxy::{MapResolver, Pool, TlsClient};
    use crate::util::ProxyPolicy;
    use crate::worker::{FallbackMap, HttpError, Session, SessionConfig, TestClock};

    struct TestTls {
        acceptor: TlsAcceptor,
        client: TlsClient,
    }

    fn test_tls() -> TestTls {
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let ca_key = KeyPair::generate().unwrap();
        let ca_cert = ca_params.self_signed(&ca_key).unwrap();
        let issuer = Issuer::from_params(&ca_params, &ca_key);
        let mut leaf_params = CertificateParams::new(vec![
            "gallery.test".into(),
            "proxy.test".into(),
            "e-hentai.org".into(),
            "forums.e-hentai.org".into(),
            "img.test".into(),
            "127.0.0.1".into(),
        ])
        .unwrap();
        leaf_params.is_ca = IsCa::NoCa;
        let leaf_key = KeyPair::generate().unwrap();
        let leaf_cert = leaf_params.signed_by(&leaf_key, &issuer).unwrap();
        let chain = vec![CertificateDer::from(leaf_cert.der().to_vec())];
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(chain, key)
            .unwrap();
        TestTls {
            acceptor: TlsAcceptor::from(Arc::new(config)),
            client: TlsClient::with_test_ca(CertificateDer::from(ca_cert.der().to_vec())),
        }
    }

    async fn read_headers<S: AsyncRead + Unpin>(stream: &mut S) -> Vec<u8> {
        // Read one byte at a time so a proxy never consumes bytes that belong
        // to the following request body.
        let mut request = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let read = stream.read(&mut byte).await.unwrap();
            if read == 0 {
                break;
            }
            request.push(byte[0]);
            if request.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        request
    }

    async fn read_request_with_body<S: AsyncRead + Unpin>(stream: &mut S) -> (String, Vec<u8>) {
        let head = read_headers(stream).await;
        let text = String::from_utf8_lossy(&head).into_owned();
        let length = text
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        let mut body = vec![0u8; length];
        if length > 0 {
            stream.read_exact(&mut body).await.unwrap();
        }
        (text, body)
    }

    async fn respond_http<S: AsyncWrite + Unpin>(stream: &mut S, body: &str, status: &str) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
        stream.shutdown().await.unwrap();
    }

    fn session(
        proxy: Option<Arc<Mutex<Pool>>>,
        resolver: Arc<MapResolver>,
        fallback: FallbackMap,
        tls: TlsClient,
        policy: ProxyPolicy,
    ) -> Session {
        Session::new(
            vec![("User-Agent".into(), Some("xeHentai-test".into()))],
            proxy,
            policy,
            SessionConfig {
                retry: 1,
                timeout: Duration::from_secs(3),
                control_limit: 8 * 1024 * 1024,
                tls,
                resolver,
                fallback,
                clock: Arc::new(TestClock::new(0.1)),
                ..SessionConfig::default()
            },
        )
    }

    async fn http_proxy_server(
        listener: TcpListener,
        body: &'static str,
        recorded: Arc<Mutex<Vec<String>>>,
        target: Option<SocketAddr>,
    ) {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let request = read_headers(&mut socket).await;
            let text = String::from_utf8_lossy(&request).into_owned();
            let first = text.lines().next().unwrap_or("").to_string();
            recorded.lock().unwrap().push(first.clone());
            if first.starts_with("CONNECT ") {
                let authority = first.split_whitespace().nth(1).unwrap();
                let _ = authority;
                let target = target.expect("CONNECT target");
                socket
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                let mut upstream = TcpStream::connect(target).await.unwrap();
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
                return;
            }
            respond_http(&mut socket, body, "200 OK").await;
        }
    }

    #[tokio::test]
    async fn proxy_http_absolute_form_uses_proxy_without_local_dns() {
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&recorded);
        let server = tokio::spawn(async move {
            if let Ok((mut socket, _)) = proxy_listener.accept().await {
                let request = read_headers(&mut socket).await;
                seen.lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&request).into_owned());
                respond_http(&mut socket, "proxied", "200 OK").await;
            }
        });
        let resolver = Arc::new(MapResolver::default());
        let mut pool = Pool::new();
        pool.add_proxy(&format!("http://127.0.0.1:{proxy_port}"))
            .unwrap();
        let pool = Arc::new(Mutex::new(pool));
        let mut session = session(
            Some(pool.clone()),
            Arc::clone(&resolver),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        let outcome = session
            .request("GET", "http://gallery.test/page", None, false)
            .await
            .expect("proxied request");
        assert_eq!(
            outcome.response.text, "proxied",
            "status {} attempts {:?} headers {:?}",
            outcome.response.status, outcome.attempts, outcome.response.headers
        );
        assert_eq!(
            outcome.response.proxy,
            Some(format!("http://127.0.0.1:{proxy_port}"))
        );
        assert!(
            resolver.calls().is_empty(),
            "proxy must resolve DNS: {:?}",
            resolver.calls()
        );
        let request = recorded.lock().unwrap()[0].clone();
        assert!(
            request.starts_with("GET http://gallery.test/page HTTP/1.1"),
            "{request}"
        );
        server.await.unwrap();
        assert_eq!(
            pool.lock()
                .unwrap()
                .counts(&format!("http://127.0.0.1:{proxy_port}")),
            Some((1, 0))
        );
    }

    #[tokio::test]
    async fn proxy_https_connect_tunnels_to_target_tls() {
        let tls = test_tls();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let acceptor = tls.acceptor.clone();
        let target_task = tokio::spawn(async move {
            let (socket, _) = target.accept().await.unwrap();
            let mut socket = acceptor.accept(socket).await.unwrap();
            let request = read_headers(&mut socket).await;
            assert!(String::from_utf8_lossy(&request).starts_with("GET /secure HTTP/1.1"));
            respond_http(&mut socket, "tls-ok", "200 OK").await;
        });
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&recorded);
        let proxy_task = tokio::spawn(http_proxy_server(
            proxy_listener,
            "unused",
            seen,
            Some(target_addr),
        ));
        let mut pool = Pool::new();
        pool.add_proxy(&format!("http://127.0.0.1:{proxy_port}"))
            .unwrap();
        let mut session = session(
            Some(Arc::new(Mutex::new(pool))),
            Arc::new(MapResolver::default()),
            FallbackMap::empty(),
            tls.client,
            ProxyPolicy::All,
        );
        let outcome = session
            .request(
                "GET",
                &format!("https://gallery.test:{}/secure", target_addr.port()),
                None,
                false,
            )
            .await
            .expect("https via CONNECT");
        assert_eq!(outcome.response.text, "tls-ok");
        assert!(recorded.lock().unwrap()[0].starts_with("CONNECT gallery.test:"));
        proxy_task.abort();
        target_task.await.unwrap();
    }

    #[tokio::test]
    async fn proxy_https_outer_tls_then_connect_then_target_tls() {
        let tls = test_tls();
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let target_acceptor = tls.acceptor.clone();
        let target_task = tokio::spawn(async move {
            let (socket, _) = target.accept().await.unwrap();
            let mut socket = target_acceptor.accept(socket).await.unwrap();
            read_headers(&mut socket).await;
            respond_http(&mut socket, "tls-chain", "200 OK").await;
        });
        let acceptor = tls.acceptor.clone();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&recorded);
        let proxy_task = tokio::spawn(async move {
            let (socket, _) = proxy_listener.accept().await.unwrap();
            let mut socket = acceptor.accept(socket).await.unwrap();
            let request = read_headers(&mut socket).await;
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request).into_owned());
            socket.write_all(b"HTTP/1.1 200 OK\r\n\r\n").await.unwrap();
            let mut upstream = TcpStream::connect(target_addr).await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
        });
        let mut pool = Pool::new();
        pool.add_proxy(&format!("https://proxy.test:{proxy_port}"))
            .unwrap();
        let resolver = Arc::new(MapResolver::new(vec![(
            "proxy.test".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), proxy_port)],
        )]));
        let mut session = session(
            Some(Arc::new(Mutex::new(pool))),
            resolver,
            FallbackMap::empty(),
            tls.client,
            ProxyPolicy::All,
        );
        let outcome = session
            .request(
                "GET",
                &format!("https://gallery.test:{}/inside", target_addr.port()),
                None,
                false,
            )
            .await
            .expect("https proxy chain");
        assert_eq!(outcome.response.text, "tls-chain");
        assert!(recorded.lock().unwrap()[0].starts_with("CONNECT gallery.test:"));
        proxy_task.abort();
        target_task.await.unwrap();
    }

    #[tokio::test]
    async fn socks4_and_socks4a_prove_local_vs_proxy_dns() {
        for (scheme, remote) in [("socks4", false), ("socks4a", true)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (tx, rx) = tokio::sync::oneshot::channel::<Vec<u8>>();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut head = [0u8; 9];
                socket.read_exact(&mut head).await.unwrap();
                let mut rest = Vec::new();
                if head[4..8] == [0, 0, 0, 1] {
                    loop {
                        let mut byte = [0u8; 1];
                        socket.read_exact(&mut byte).await.unwrap();
                        rest.push(byte[0]);
                        if byte[0] == 0 {
                            break;
                        }
                    }
                }
                let mut reply = vec![0u8; 8];
                reply[1] = 0x5a;
                socket.write_all(&reply).await.unwrap();
                let request = read_headers(&mut socket).await;
                tx.send([head.to_vec(), rest, request].concat()).unwrap();
                respond_http(&mut socket, "socks-ok", "200 OK").await;
            });
            let resolver = Arc::new(MapResolver::new(vec![(
                "gallery.test".into(),
                vec![SocketAddr::new("127.0.0.1".parse().unwrap(), 80)],
            )]));
            let mut pool = Pool::new();
            pool.add_proxy(&format!("{scheme}://127.0.0.1:{port}"))
                .unwrap();
            let mut session = session(
                Some(Arc::new(Mutex::new(pool))),
                Arc::clone(&resolver),
                FallbackMap::empty(),
                TlsClient::production(),
                ProxyPolicy::All,
            );
            let outcome = session
                .request("GET", "http://gallery.test/socks", None, false)
                .await
                .expect("socks4 request");
            assert_eq!(outcome.response.text, "socks-ok");
            let wire = rx.await.unwrap();
            let has_local_dns = resolver.calls().iter().any(|host| host == "gallery.test");
            assert_eq!(
                has_local_dns,
                !remote,
                "{scheme} resolver calls {:?}",
                resolver.calls()
            );
            if remote {
                assert!(
                    wire.windows("gallery.test".len())
                        .any(|w| w == b"gallery.test")
                );
            } else {
                assert_eq!(&wire[4..8], &[127, 0, 0, 1]);
            }
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn socks5_and_socks5h_prove_local_vs_proxy_dns() {
        for (scheme, remote) in [("socks5", false), ("socks5h", true)] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let (tx, rx) = tokio::sync::oneshot::channel::<u8>();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut greeting = [0u8; 3];
                socket.read_exact(&mut greeting).await.unwrap();
                socket.write_all(&[5, 0]).await.unwrap();
                let mut head = [0u8; 4];
                socket.read_exact(&mut head).await.unwrap();
                let atyp = head[3];
                let mut rest = Vec::new();
                if atyp == 3 {
                    let mut len = [0u8; 1];
                    socket.read_exact(&mut len).await.unwrap();
                    rest.push(len[0]);
                    let mut name = vec![0u8; len[0] as usize + 2];
                    socket.read_exact(&mut name).await.unwrap();
                    rest.extend_from_slice(&name);
                } else if atyp == 1 {
                    let mut addr = [0u8; 6];
                    socket.read_exact(&mut addr).await.unwrap();
                    rest.extend_from_slice(&addr);
                } else {
                    let mut addr = [0u8; 18];
                    socket.read_exact(&mut addr).await.unwrap();
                    rest.extend_from_slice(&addr);
                }
                socket
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0])
                    .await
                    .unwrap();
                let request = read_headers(&mut socket).await;
                tx.send(atyp).unwrap();
                let _ = request;
                respond_http(&mut socket, "socks5-ok", "200 OK").await;
            });
            let resolver = Arc::new(MapResolver::new(vec![(
                "gallery.test".into(),
                vec![SocketAddr::new("127.0.0.1".parse().unwrap(), 80)],
            )]));
            let mut pool = Pool::new();
            pool.add_proxy(&format!("{scheme}://127.0.0.1:{port}"))
                .unwrap();
            let mut session = session(
                Some(Arc::new(Mutex::new(pool))),
                Arc::clone(&resolver),
                FallbackMap::empty(),
                TlsClient::production(),
                ProxyPolicy::All,
            );
            let outcome = session
                .request("GET", "http://gallery.test/socks5", None, false)
                .await
                .expect("socks5 request");
            assert_eq!(outcome.response.text, "socks5-ok");
            assert_eq!(rx.await.unwrap(), if remote { 3 } else { 1 });
            let local = resolver.calls().iter().any(|host| host == "gallery.test");
            assert_eq!(
                local,
                !remote,
                "{scheme} resolver calls {:?}",
                resolver.calls()
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn route_establishment_deadline_bounds_socks_handshakes() {
        for scheme in ["socks4", "socks4a", "socks5", "socks5h"] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut greeting = [0u8; 32];
                let _ = socket.read(&mut greeting).await;
                let mut hold = [0u8; 1];
                let _ = socket.read(&mut hold).await;
            });
            let mut pool = Pool::new();
            pool.add_proxy(&format!("{scheme}://127.0.0.1:{port}"))
                .unwrap();
            let resolver = Arc::new(MapResolver::new(vec![(
                "gallery.test".into(),
                vec![SocketAddr::new("127.0.0.1".parse().unwrap(), 80)],
            )]));
            let mut client = session(
                Some(Arc::new(Mutex::new(pool))),
                resolver,
                FallbackMap::empty(),
                TlsClient::production(),
                ProxyPolicy::All,
            );
            client.config.timeout = Duration::from_millis(200);
            let started = std::time::Instant::now();
            let outcome = tokio::time::timeout(
                Duration::from_secs(3),
                client.request("GET", "http://gallery.test/page", None, false),
            )
            .await;
            let elapsed = started.elapsed();
            // A retryable timeout after the configured retry budget ends in
            // the reference 600 boundary, not an exception; the attempt record
            // must still carry the bounded route error rather than success.
            match outcome {
                Ok(Err(error)) => assert!(
                    error.to_string().contains("route establishment timed out"),
                    "{scheme}: unexpected error {error}"
                ),
                Ok(Ok(outcome)) => {
                    assert_eq!(outcome.response.status, 600, "{scheme}: unexpected success");
                    let error = outcome.attempts[0].error.as_deref().unwrap_or("");
                    assert!(
                        error.contains("route establishment timed out"),
                        "{scheme}: unexpected attempt error {error}"
                    );
                }
                Err(_) => panic!("{scheme}: route establishment was not bounded"),
            }
            assert!(
                elapsed < Duration::from_secs(2),
                "{scheme}: bounded deadline took {elapsed:?}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn route_establishment_deadline_bounds_connect_drip() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let request = read_headers(&mut socket).await;
            assert!(String::from_utf8_lossy(&request).starts_with("CONNECT "));
            for byte in b"HTTP/1.1 200 Connection established\r\n\r\n" {
                if socket.write_all(&[*byte]).await.is_err() {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            let mut hold = [0u8; 1];
            let _ = socket.read(&mut hold).await;
        });
        let mut pool = Pool::new();
        pool.add_proxy(&format!("http://127.0.0.1:{port}")).unwrap();
        let mut client = session(
            Some(Arc::new(Mutex::new(pool))),
            Arc::new(MapResolver::default()),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        client.config.timeout = Duration::from_millis(200);
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            client.request("GET", "https://gallery.test/page", None, false),
        )
        .await;
        let elapsed = started.elapsed();
        match outcome {
            Ok(Err(error)) => assert!(
                error.to_string().contains("route establishment timed out"),
                "unexpected error {error}"
            ),
            Ok(Ok(outcome)) => {
                assert_eq!(outcome.response.status, 600, "unexpected success");
                let error = outcome.attempts[0].error.as_deref().unwrap_or("");
                assert!(
                    error.contains("route establishment timed out"),
                    "unexpected attempt error {error}"
                );
            }
            Err(_) => panic!("CONNECT establishment was not bounded"),
        }
        assert!(
            elapsed < Duration::from_secs(2),
            "CONNECT deadline took {elapsed:?}"
        );
        server.abort();
    }

    #[tokio::test]
    async fn route_establishment_deadline_bounds_tls_handshakes() {
        // A target TLS handshake that never answers, then an outer proxy TLS
        // handshake that never answers. Both must fit one route deadline.
        for proxy_tls in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let port = listener.local_addr().unwrap().port();
            let server = tokio::spawn(async move {
                let (socket, _) = listener.accept().await.unwrap();
                // Hold the accepted TCP connection without ever answering the
                // TLS ClientHello, then let the task end when the test aborts.
                tokio::time::sleep(Duration::from_secs(4)).await;
                drop(socket);
            });
            let resolver = Arc::new(MapResolver::new(vec![(
                "proxy.test".into(),
                vec![SocketAddr::new("127.0.0.1".parse().unwrap(), port)],
            )]));
            let (proxy, target) = if proxy_tls {
                let mut pool = Pool::new();
                pool.add_proxy(&format!("https://proxy.test:{port}"))
                    .unwrap();
                (
                    Some(Arc::new(Mutex::new(pool))),
                    "https://gallery.test/page".to_string(),
                )
            } else {
                (None, format!("https://gallery.test:{port}/page"))
            };
            let mut client = session(
                proxy,
                Arc::clone(&resolver),
                FallbackMap::empty(),
                TlsClient::production(),
                ProxyPolicy::All,
            );
            if !proxy_tls {
                client.config.resolver = Arc::new(MapResolver::new(vec![(
                    "gallery.test".into(),
                    vec![SocketAddr::new("127.0.0.1".parse().unwrap(), port)],
                )]));
            }
            client.config.timeout = Duration::from_millis(200);
            let started = std::time::Instant::now();
            let outcome = tokio::time::timeout(
                Duration::from_secs(3),
                client.request("GET", &target, None, false),
            )
            .await;
            let elapsed = started.elapsed();
            match outcome {
                Ok(Err(error)) => assert!(
                    error.to_string().contains("route establishment timed out"),
                    "proxy_tls={proxy_tls}: unexpected error {error}"
                ),
                Ok(Ok(outcome)) => {
                    assert_eq!(
                        outcome.response.status, 600,
                        "proxy_tls={proxy_tls}: unexpected success"
                    );
                    let error = outcome.attempts[0].error.as_deref().unwrap_or("");
                    assert!(
                        error.contains("route establishment timed out"),
                        "proxy_tls={proxy_tls}: unexpected attempt error {error}"
                    );
                }
                Err(_) => panic!("proxy_tls={proxy_tls}: TLS route establishment was not bounded"),
            }
            assert!(
                elapsed < Duration::from_secs(2),
                "proxy_tls={proxy_tls}: TLS deadline took {elapsed:?}"
            );
            server.abort();
        }
    }

    #[tokio::test]
    async fn route_timeout_is_retryable_and_scores_the_proxy_pool() {
        let stalled = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stalled_port = stalled.local_addr().unwrap().port();
        let stalled_task = tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((socket, _)) = stalled.accept().await {
                held.push(socket);
            }
        });
        let working = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let working_port = working.local_addr().unwrap().port();
        let working_task = tokio::spawn(async move {
            let (mut socket, _) = working.accept().await.unwrap();
            read_headers(&mut socket).await;
            respond_http(&mut socket, "after-timeout", "200 OK").await;
        });
        let mut pool = Pool::new();
        let first = format!("socks5://127.0.0.1:{stalled_port}");
        let second = format!("http://127.0.0.1:{working_port}");
        pool.add_proxy(&first).unwrap();
        pool.add_proxy(&second).unwrap();
        let pool = Arc::new(Mutex::new(pool));
        let resolver = Arc::new(MapResolver::new(vec![(
            "gallery.test".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), 80)],
        )]));
        let mut client = session(
            Some(Arc::clone(&pool)),
            resolver,
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        client.config.timeout = Duration::from_millis(150);
        client.config.retry = 6;
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(4),
            client.request("GET", "http://gallery.test/after", None, false),
        )
        .await
        .expect("retry loop was not bounded")
        .expect("the second proxy must serve after the first times out");
        assert_eq!(outcome.response.text, "after-timeout");
        assert_eq!(outcome.attempts.len(), 6);
        assert!(
            outcome.attempts[..5]
                .iter()
                .all(|attempt| attempt.proxy.as_deref() == Some(first.as_str())),
            "{:?}",
            outcome.attempts
        );
        assert_eq!(pool.lock().unwrap().counts(&first), Some((0, 5)));
        assert_eq!(pool.lock().unwrap().counts(&second), Some((1, 0)));
        assert!(
            started.elapsed() < Duration::from_secs(3),
            "five bounded attempts plus one success took {:?}",
            started.elapsed()
        );
        stalled_task.abort();
        working_task.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_route_attempt_closes_peer_and_next_request_works() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<()>();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            assert!(String::from_utf8_lossy(&read_headers(&mut first).await).starts_with("GET "));
            let _ = seen_tx.send(());
            let mut buffer = [0u8; 16];
            let closed = matches!(
                tokio::time::timeout(Duration::from_secs(1), first.read(&mut buffer)).await,
                Ok(Ok(0))
            );
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            respond_http(&mut second, "fresh", "200 OK").await;
        });
        let mut pool = Pool::new();
        pool.add_proxy(&format!("http://127.0.0.1:{port}")).unwrap();
        let mut client = session(
            Some(Arc::new(Mutex::new(pool))),
            Arc::new(MapResolver::default()),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        client.config.timeout = Duration::from_secs(5);
        let mut request =
            Box::pin(client.request("GET", "http://gallery.test/cancel", None, false));
        tokio::select! {
            _ = &mut request => panic!("the stalled proxy must not answer"),
            _ = seen_rx => {}
        }
        drop(request);
        let closed = tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("peer closure observation timed out")
            .expect("closure notify");
        assert!(closed, "dropping the request must close the proxy peer");
        let outcome = client
            .request("GET", "http://gallery.test/next", None, false)
            .await
            .expect("next request on the same session");
        assert_eq!(outcome.response.text, "fresh");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn dropped_stream_body_owns_and_closes_the_connection() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel::<()>();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            let mut response =
                b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 100000\r\n\r\n"
                    .to_vec();
            response.extend_from_slice(&[7u8; 1024]);
            first.write_all(&response).await.unwrap();
            let _ = ready_tx.send(());
            let mut buffer = [0u8; 16];
            let closed = matches!(
                tokio::time::timeout(Duration::from_secs(1), first.read(&mut buffer)).await,
                Ok(Ok(0))
            );
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            respond_http(&mut second, "fresh", "200 OK").await;
        });
        let resolver = Arc::new(MapResolver::new(vec![(
            "gallery.test".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), port)],
        )]));
        let mut client = session(
            None,
            resolver,
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        client.config.timeout = Duration::from_secs(5);
        let mut outcome = client
            .request(
                "GET",
                &format!("http://gallery.test:{port}/stream"),
                None,
                true,
            )
            .await
            .expect("streaming response");
        let mut body = outcome.response.stream.take().unwrap_or_else(|| {
            panic!(
                "stream body: status {} attempts {:?} body {:?}",
                outcome.response.status,
                outcome.attempts,
                String::from_utf8_lossy(&outcome.response.body)
            )
        });
        let chunk = body.next_chunk().await.expect("chunk").expect("data");
        assert_eq!(chunk.len(), 1024);
        let _ = ready_rx.await;
        drop(body);
        let closed = tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("peer closure observation timed out")
            .expect("closure notify");
        assert!(
            closed,
            "dropping a partially read body must own and close the connection"
        );
        let next = client
            .request(
                "GET",
                &format!("http://gallery.test:{port}/next"),
                None,
                false,
            )
            .await
            .expect("next request");
        assert_eq!(next.response.text, "fresh");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn control_response_closes_keep_alive_connection_promptly() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
                )
                .await
                .unwrap();
            let mut buffer = [0u8; 16];
            let closed = matches!(
                tokio::time::timeout(Duration::from_secs(1), first.read(&mut buffer)).await,
                Ok(Ok(0))
            );
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            respond_http(&mut second, "fresh", "200 OK").await;
        });
        let resolver = Arc::new(MapResolver::new(vec![(
            "gallery.test".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), port)],
        )]));
        let mut client = session(
            None,
            resolver,
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        client.config.timeout = Duration::from_secs(8);
        let started = std::time::Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(3),
            client.request(
                "GET",
                &format!("http://gallery.test:{port}/keepalive"),
                None,
                false,
            ),
        )
        .await
        .expect("a keep-alive peer must not hold the completed request open")
        .expect("control response");
        assert_eq!(outcome.response.text, "hello");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "completed control response took {:?}",
            started.elapsed()
        );
        let closed = tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("peer closure observation timed out")
            .expect("closure notify");
        assert!(
            closed,
            "the response owner must close the idle keep-alive connection"
        );
        let next = client
            .request(
                "GET",
                &format!("http://gallery.test:{port}/next"),
                None,
                false,
            )
            .await
            .expect("next request");
        assert_eq!(next.response.text, "fresh");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_request_before_headers_closes_peer_and_allows_fresh_work() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, seen_rx) = tokio::sync::oneshot::channel::<()>();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            let _ = seen_tx.send(());
            let mut buffer = [0u8; 16];
            let closed = matches!(
                tokio::time::timeout(Duration::from_secs(1), first.read(&mut buffer)).await,
                Ok(Ok(0))
            );
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            respond_http(&mut second, "fresh", "200 OK").await;
        });
        let resolver = Arc::new(MapResolver::new(vec![(
            "gallery.test".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), port)],
        )]));
        let mut client = session(
            None,
            resolver,
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        client.config.timeout = Duration::from_secs(5);
        let stall_url = format!("http://gallery.test:{port}/stall");
        let mut request = Box::pin(client.request("GET", &stall_url, None, false));
        tokio::select! {
            _ = &mut request => panic!("the stalled server must not answer"),
            _ = seen_rx => {}
        }
        drop(request);
        let closed = tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("peer closure observation timed out")
            .expect("closure notify");
        assert!(
            closed,
            "cancelling before response headers must close the peer"
        );
        let next = client
            .request(
                "GET",
                &format!("http://gallery.test:{port}/next"),
                None,
                false,
            )
            .await
            .expect("next request");
        assert_eq!(next.response.text, "fresh");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn control_body_overflow_closes_peer_and_allows_fresh_work() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            let mut response =
                b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 64\r\n\r\n"
                    .to_vec();
            response.extend_from_slice(&[b'x'; 64]);
            first.write_all(&response).await.unwrap();
            let mut buffer = [0u8; 16];
            let closed = matches!(
                tokio::time::timeout(Duration::from_secs(1), first.read(&mut buffer)).await,
                Ok(Ok(0))
            );
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            respond_http(&mut second, "fresh", "200 OK").await;
        });
        let resolver = Arc::new(MapResolver::new(vec![(
            "gallery.test".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), port)],
        )]));
        let mut client = session(
            None,
            resolver,
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        client.config.timeout = Duration::from_secs(5);
        client.config.control_limit = 16;
        let overflow_url = format!("http://gallery.test:{port}/overflow");
        let error = match client.request("GET", &overflow_url, None, false).await {
            Ok(_) => panic!("control body must stay bounded"),
            Err(error) => error,
        };
        assert!(
            matches!(error, HttpError::BodyTooLarge { limit: 16 }),
            "{error}"
        );
        let closed = tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("peer closure observation timed out")
            .expect("closure notify");
        assert!(closed, "an oversized control body must close the peer");
        let next = client
            .request(
                "GET",
                &format!("http://gallery.test:{port}/next"),
                None,
                false,
            )
            .await
            .expect("next request");
        assert_eq!(next.response.text, "fresh");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_request_header_after_handshake_closes_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 16];
            let closed = matches!(
                tokio::time::timeout(Duration::from_secs(1), first.read(&mut buffer)).await,
                Ok(Ok(0))
            );
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            respond_http(&mut second, "fresh", "200 OK").await;
        });
        let resolver = Arc::new(MapResolver::new(vec![(
            "gallery.test".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), port)],
        )]));
        let mut client = Session::new(
            vec![
                ("User-Agent".into(), Some("xeHentai-test".into())),
                ("X-Bad".into(), Some("a\r\nInjected: 1".into())),
            ],
            None,
            ProxyPolicy::All,
            SessionConfig {
                retry: 1,
                timeout: Duration::from_secs(5),
                tls: TlsClient::production(),
                resolver,
                fallback: FallbackMap::empty(),
                clock: Arc::new(TestClock::new(0.0)),
                ..SessionConfig::default()
            },
        );
        let bad_url = format!("http://gallery.test:{port}/bad");
        let error = match client.request("GET", &bad_url, None, false).await {
            Ok(_) => panic!("a CR/LF header value must be refused"),
            Err(error) => error,
        };
        assert!(matches!(error, HttpError::Header(_)), "{error}");
        let closed = tokio::time::timeout(Duration::from_secs(2), closed_rx)
            .await
            .expect("peer closure observation timed out")
            .expect("closure notify");
        assert!(
            closed,
            "a header-build error must close the handshaken peer"
        );
        client.set_header("X-Bad", None);
        let next = client
            .request(
                "GET",
                &format!("http://gallery.test:{port}/next"),
                None,
                false,
            )
            .await
            .expect("next request");
        assert_eq!(next.response.text, "fresh");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn early_eof_is_a_protocol_error_and_fresh_work_succeeds() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            first
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\n\r\nshort",
                )
                .await
                .unwrap();
            first.shutdown().await.unwrap();
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            respond_http(&mut second, "fresh", "200 OK").await;
        });
        let resolver = Arc::new(MapResolver::new(vec![(
            "gallery.test".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), port)],
        )]));
        let mut client = session(
            None,
            resolver,
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        client.config.timeout = Duration::from_secs(5);
        let outcome = client
            .request(
                "GET",
                &format!("http://gallery.test:{port}/eof"),
                None,
                false,
            )
            .await
            .expect("retryable protocol failure ends in the 600 boundary");
        assert_eq!(outcome.response.status, 600);
        let error = outcome.attempts[0].error.as_deref().unwrap_or("");
        assert!(
            error.contains("http protocol error"),
            "early EOF must stay a protocol error: {error}"
        );
        let next = client
            .request(
                "GET",
                &format!("http://gallery.test:{port}/next"),
                None,
                false,
            )
            .await
            .expect("next request");
        assert_eq!(next.response.text, "fresh");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn proxy_ban_disables_the_first_proxy_and_reselects() {
        let first = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first_port = first.local_addr().unwrap().port();
        let second = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let second_port = second.local_addr().unwrap().port();
        let ban = "Your IP address has been temporarily banned. The ban expires in 5 minutes";
        let first_task = tokio::spawn(async move {
            let (mut socket, _) = first.accept().await.unwrap();
            read_headers(&mut socket).await;
            respond_http(&mut socket, ban, "200 OK").await;
        });
        let second_task = tokio::spawn(async move {
            let (mut socket, _) = second.accept().await.unwrap();
            read_headers(&mut socket).await;
            respond_http(&mut socket, "after-ban", "200 OK").await;
        });
        let mut pool = Pool::new();
        let first_addr = format!("http://127.0.0.1:{first_port}");
        let second_addr = format!("http://127.0.0.1:{second_port}");
        pool.add_proxy(&first_addr).unwrap();
        pool.add_proxy(&second_addr).unwrap();
        let pool = Arc::new(Mutex::new(pool));
        let mut session = session(
            Some(pool.clone()),
            Arc::new(MapResolver::default()),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        let outcome = session
            .request("GET", "http://gallery.test/ban", None, false)
            .await
            .expect("ban request");
        assert_eq!(outcome.response.text, "after-ban");
        {
            let pool = pool.lock().unwrap();
            assert_eq!(pool.counts(&first_addr), Some((1, 5)));
            let expire = pool.is_disabled(&first_addr).expect("ban expiry");
            assert!(
                expire > 0.0,
                "ban expiry is a timestamp, not permanent: {expire}"
            );
            assert_eq!(pool.counts(&second_addr), Some((1, 0)));
        }
        first_task.await.unwrap();
        second_task.await.unwrap();
    }

    #[tokio::test]
    async fn image_only_policy_skips_webpages_and_uses_proxy_for_images() {
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let proxy_hits = Arc::new(AtomicUsize::new(0));
        let hits = Arc::clone(&proxy_hits);
        let proxy_task = tokio::spawn(async move {
            let _ = tokio::time::timeout(Duration::from_millis(500), async {
                if let Ok((mut socket, _)) = proxy_listener.accept().await {
                    hits.fetch_add(1, Ordering::SeqCst);
                    read_headers(&mut socket).await;
                    respond_http(&mut socket, "proxy-image", "200 OK").await;
                }
            })
            .await;
        });
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_port = target.local_addr().unwrap().port();
        let target_task = tokio::spawn(async move {
            let (mut socket, _) = target.accept().await.unwrap();
            let request = read_headers(&mut socket).await;
            assert!(String::from_utf8_lossy(&request).starts_with("GET /s/abc/1-2 HTTP/1.1"));
            respond_http(&mut socket, "direct-page", "200 OK").await;
        });
        let resolver = Arc::new(MapResolver::new(vec![(
            "e-hentai.org".into(),
            vec![SocketAddr::new("127.0.0.1".parse().unwrap(), target_port)],
        )]));
        let mut pool = Pool::new();
        pool.add_proxy(&format!("http://127.0.0.1:{proxy_port}"))
            .unwrap();
        let mut session = session(
            Some(Arc::new(Mutex::new(pool))),
            resolver,
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::ImageOnly,
        );
        let outcome = session
            .request(
                "GET",
                &format!("http://e-hentai.org:{target_port}/s/abc/1-2"),
                None,
                false,
            )
            .await
            .expect("webpage direct");
        assert_eq!(outcome.response.text, "direct-page");
        assert_eq!(proxy_hits.load(Ordering::SeqCst), 0);
        target_task.await.unwrap();
        proxy_task.abort();
    }

    #[tokio::test]
    async fn fallback_ip_forces_tls_with_original_host_and_skips_dns() {
        let tls = test_tls();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let acceptor = tls.acceptor.clone();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&seen);
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = acceptor.accept(socket).await.unwrap();
            let request = read_headers(&mut socket).await;
            record
                .lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request).into_owned());
            respond_http(&mut socket, "fallback-ok", "200 OK").await;
        });
        let resolver = Arc::new(MapResolver::default());
        let fallback = FallbackMap::empty().with_host("gallery.test", &["127.0.0.1"]);
        let mut session = session(
            None,
            Arc::clone(&resolver),
            fallback,
            tls.client,
            ProxyPolicy::All,
        );
        let outcome = session
            .request(
                "GET",
                &format!("http://gallery.test:{port}/fb"),
                None,
                false,
            )
            .await
            .expect("fallback request");
        assert_eq!(outcome.response.text, "fallback-ok");
        assert!(
            resolver.calls().is_empty(),
            "fallback must not use DNS: {:?}",
            resolver.calls()
        );
        let request = seen.lock().unwrap()[0].to_ascii_lowercase();
        assert!(request.contains("host: gallery.test\r\n"), "{request}");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn glype_rewrites_sslagree_cookie_and_text_then_streams_binary() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            for step in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_headers(&mut socket).await;
                let text = String::from_utf8_lossy(&request).into_owned();
                record.lock().unwrap().push(text.clone());
                if step == 0 {
                    assert!(text.contains(
                        "browse.php?u=https%3A%2F%2Fe-hentai.org%2Fg%2F1%2F&b=4&f=norefer"
                    ));
                    let body = "<input type=\"hidden\" name=\"action\" value=\"sslagree\">";
                    respond_http(&mut socket, body, "200 OK").await;
                } else if step == 1 {
                    assert!(
                        text.starts_with(
                            "GET /glype/includes/process.php?action=sslagree HTTP/1.1"
                        )
                    );
                    let response = "HTTP/1.1 200 OK\r\nSet-Cookie: s=abc\r\nContent-Type: text/html\r\nContent-Length: 2\r\n\r\nok";
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                } else {
                    assert!(
                        text.to_ascii_lowercase().contains("cookie: s=abc"),
                        "{text}"
                    );
                    let body = "<a href=\"/glype/browse.php?u=https%3A%2F%2Fe-hentai.org%2Fg%2F1&b=4&f=norefer\">done</a>";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nSet-Cookie: c[proxy][/][foo]=bar\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).await.unwrap();
                    socket.shutdown().await.unwrap();
                }
            }
        });
        let mut pool = Pool::new();
        let addr = format!("http://127.0.0.1:{port}/glype/browse.php?u=x&b=4");
        pool.add_proxy(&addr).unwrap();
        let pool = Arc::new(Mutex::new(pool));
        let mut session = session(
            Some(pool.clone()),
            Arc::new(MapResolver::default()),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        let outcome = session
            .request("GET", "https://e-hentai.org/g/1/", None, false)
            .await
            .expect("glype request");
        assert!(
            outcome
                .response
                .text
                .contains("<a href=\"https://e-hentai.org/g/1\">done</a>"),
            "{}",
            outcome.response.text
        );
        let set_cookie = outcome.response.headers.joined("set-cookie").unwrap();
        assert!(set_cookie.contains("foo=bar"), "{set_cookie}");
        {
            let pool = pool.lock().unwrap();
            let entry = &pool.proxies[&addr];
            let state = entry.spec.glype.as_ref().unwrap().lock().unwrap();
            assert_eq!(state.s.as_deref(), Some("abc"));
        }
        server.await.unwrap();
        assert_eq!(requests.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn glype_forwards_post_method_form_body_and_content_headers() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let record = Arc::clone(&recorded);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (head, body) = read_request_with_body(&mut socket).await;
            record.lock().unwrap().push((head, body));
            let payload = b"png-bytes";
            let mut response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
                payload.len()
            )
            .into_bytes();
            response.extend_from_slice(payload);
            socket.write_all(&response).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let mut pool = Pool::new();
        let addr = format!("http://127.0.0.1:{port}/glype/browse.php?u=x&b=4");
        pool.add_proxy(&addr).unwrap();
        let mut session = Session::new(
            vec![
                ("User-Agent".into(), Some("xeHentai-test".into())),
                ("Cookie".into(), Some("a=1".into())),
            ],
            Some(Arc::new(Mutex::new(pool))),
            ProxyPolicy::All,
            SessionConfig {
                retry: 1,
                timeout: Duration::from_secs(3),
                tls: TlsClient::production(),
                resolver: Arc::new(MapResolver::default()),
                fallback: FallbackMap::empty(),
                clock: Arc::new(TestClock::new(0.0)),
                ..SessionConfig::default()
            },
        );
        let data = vec![
            ("username".to_string(), "synthetic-user".to_string()),
            ("password".to_string(), "synthetic-pass".to_string()),
            ("submit".to_string(), "login".to_string()),
        ];
        let outcome = session
            .request("POST", "https://e-hentai.org/g/1/", Some(data), false)
            .await
            .expect("glype post request");
        assert_eq!(outcome.response.status, 200);
        assert_eq!(outcome.response.body, b"png-bytes");
        server.await.unwrap();
        let (head, body) = recorded.lock().unwrap()[0].clone();
        let mut lines = head.lines();
        let request_line = lines.next().unwrap_or("");
        assert!(
            request_line.starts_with(
                "POST /glype/browse.php?u=https%3A%2F%2Fe-hentai.org%2Fg%2F1%2F&b=4&f=norefer HTTP/1.1"
            ),
            "{request_line}"
        );
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("content-type: application/x-www-form-urlencoded"),
            "{head}"
        );
        assert!(
            lower.contains(&format!("content-length: {}", body.len())),
            "{head}"
        );
        assert!(
            lower.contains("referer: http://127.0.0.1:"),
            "glype referer must point at the script: {head}"
        );
        assert!(
            lower.contains("cookie: c[e-hentai.org][/][a]=1"),
            "glype cookie must be re-keyed: {head}"
        );
        assert_eq!(
            body,
            b"username=synthetic-user&password=synthetic-pass&submit=login"
        );
    }

    #[tokio::test]
    async fn glype_sslagree_keeps_post_body_and_replays_it_on_the_same_session() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let record = Arc::clone(&recorded);
        let server = tokio::spawn(async move {
            for step in 0..3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let (head, body) = read_request_with_body(&mut socket).await;
                record.lock().unwrap().push((head.clone(), body.clone()));
                let request_line = head.lines().next().unwrap_or("").to_string();
                match step {
                    0 => {
                        assert!(
                            request_line.starts_with("POST /glype/browse.php?"),
                            "{head}"
                        );
                        assert_eq!(
                            body, b"username=synthetic-user&password=synthetic-pass",
                            "the primary glype hop must keep the caller body"
                        );
                        let marker = b"<input type=\"hidden\" name=\"action\" value=\"sslagree\">";
                        let mut response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n",
                            marker.len()
                        )
                        .into_bytes();
                        response.extend_from_slice(marker);
                        socket.write_all(&response).await.unwrap();
                        socket.shutdown().await.unwrap();
                    }
                    1 => {
                        assert!(
                            request_line.starts_with(
                                "GET /glype/includes/process.php?action=sslagree HTTP/1.1"
                            ),
                            "{head}"
                        );
                        assert_eq!(
                            body, b"username=synthetic-user&password=synthetic-pass",
                            "the reference forwards the caller kwargs to the agreement GET"
                        );
                        let response = "HTTP/1.1 200 OK\r\nSet-Cookie: s=abc\r\nContent-Type: text/html\r\nContent-Length: 2\r\n\r\nok";
                        socket.write_all(response.as_bytes()).await.unwrap();
                        socket.shutdown().await.unwrap();
                    }
                    _ => {
                        assert!(
                            request_line.starts_with("POST /glype/browse.php?"),
                            "{head}"
                        );
                        assert_eq!(
                            body, b"username=synthetic-user&password=synthetic-pass",
                            "the replayed hop must stay POST with its body"
                        );
                        assert!(
                            head.to_ascii_lowercase().contains("cookie: s=abc"),
                            "the agreement cookie must be replayed: {head}"
                        );
                        let payload = b"done";
                        let mut response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
                            payload.len()
                        )
                        .into_bytes();
                        response.extend_from_slice(payload);
                        socket.write_all(&response).await.unwrap();
                        socket.shutdown().await.unwrap();
                    }
                }
            }
        });
        let mut pool = Pool::new();
        let addr = format!("http://127.0.0.1:{port}/glype/browse.php?u=x&b=4");
        pool.add_proxy(&addr).unwrap();
        let mut session = session(
            Some(Arc::new(Mutex::new(pool))),
            Arc::new(MapResolver::default()),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        let data = vec![
            ("username".to_string(), "synthetic-user".to_string()),
            ("password".to_string(), "synthetic-pass".to_string()),
        ];
        let outcome = session
            .request("POST", "https://e-hentai.org/g/1/", Some(data), false)
            .await
            .expect("glype sslagree post");
        assert_eq!(outcome.response.body, b"done");
        assert_eq!(outcome.response.status, 200);
        server.await.unwrap();
        assert_eq!(recorded.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn app_login_post_through_glype_keeps_form_and_session_effects() {
        use crate::filters::Filters;
        use crate::worker::SessionConfig;
        use std::path::Path;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let recorded = Arc::new(Mutex::new(Vec::<(String, Vec<u8>)>::new()));
        let record = Arc::clone(&recorded);
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (head, body) = read_request_with_body(&mut socket).await;
            record.lock().unwrap().push((head, body));
            let response = "HTTP/1.1 200 OK\r\nSet-Cookie: ipb_member_id=42; path=/\r\nSet-Cookie: ipb_pass_hash=deadbeef; path=/\r\nSet-Cookie: c[proxy][/][keep]=1\r\nContent-Type: text/html\r\nContent-Length: 0\r\n\r\n";
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut app = super::test_app(root.path(), &[]);
        app.set_session_config(SessionConfig {
            retry: 1,
            timeout: Duration::from_secs(5),
            resolver: Arc::new(MapResolver::default()),
            fallback: FallbackMap::empty(),
            clock: Arc::new(TestClock::new(0.0)),
            ..SessionConfig::default()
        });
        app.update_config(
            serde_json::json!({"proxy": [format!("http://127.0.0.1:{port}/glype/browse.php?u=x&b=4")]})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        let mut filters = Filters::load(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let outcome = app
            .login_exhentai(&mut filters, "synthetic-user", "synthetic-pass")
            .await
            .unwrap();
        assert!(outcome.cookies.is_some());
        assert!(app.has_login());
        assert_eq!(app.cookie("ipb_member_id"), Some("42"));
        assert_eq!(app.cookie("ipb_pass_hash"), Some("deadbeef"));
        let session_text = std::fs::read_to_string(root.path().join("h.json")).unwrap();
        assert!(session_text.contains("ipb_member_id"), "{session_text}");
        assert!(session_text.contains("ipb_pass_hash"), "{session_text}");
        server.await.unwrap();
        let (head, body) = recorded.lock().unwrap()[0].clone();
        let request_line = head.lines().next().unwrap_or("");
        assert!(
            request_line.starts_with(
                "POST /glype/browse.php?u=https%3A%2F%2Fforums.e-hentai.org%2Findex.php%3Fact%3DLogin%26CODE%3D01&b=4&f=norefer HTTP/1.1"
            ),
            "login must reach glype as POST: {request_line}"
        );
        let lower = head.to_ascii_lowercase();
        assert!(
            lower.contains("content-type: application/x-www-form-urlencoded"),
            "{head}"
        );
        assert!(lower.contains("referer: http://127.0.0.1:"), "{head}");
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("UserName=synthetic-user"), "{text}");
        assert!(text.contains("PassWord=synthetic-pass"), "{text}");
        assert!(text.contains("returntype=8"), "{text}");
    }

    #[tokio::test]
    async fn login_filter_updates_app_cookies_header_and_session_file() {
        use crate::filters::Filters;
        use crate::worker::SessionConfig;
        use std::path::Path;

        let tls = test_tls();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let acceptor = tls.acceptor.clone();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&recorded);
        let server = tokio::spawn(async move {
            let (socket, _) = target.accept().await.unwrap();
            let mut socket = acceptor.accept(socket).await.unwrap();
            let headers = read_headers(&mut socket).await;
            let text = String::from_utf8_lossy(&headers).into_owned();
            let length = text
                .lines()
                .find_map(|line| {
                    let lower = line.to_ascii_lowercase();
                    lower
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            let mut body = vec![0u8; length];
            if length > 0 {
                socket.read_exact(&mut body).await.unwrap();
            }
            seen.lock()
                .unwrap()
                .push(format!("{text}{}", String::from_utf8_lossy(&body)));
            let response = "HTTP/1.1 200 OK\r\nSet-Cookie: ipb_member_id=42; path=/\r\nSet-Cookie: ipb_pass_hash=deadbeef; path=/\r\nContent-Length: 0\r\n\r\n";
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let proxy_task = tokio::spawn(async move {
            let (mut socket, _) = proxy_listener.accept().await.unwrap();
            let request = read_headers(&mut socket).await;
            assert!(
                String::from_utf8_lossy(&request).starts_with("CONNECT forums.e-hentai.org:443")
            );
            socket
                .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                .await
                .unwrap();
            let mut upstream = TcpStream::connect(target_addr).await.unwrap();
            let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
        });

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut app = super::test_app(root.path(), &[]);
        app.set_session_config(SessionConfig {
            retry: 1,
            timeout: Duration::from_secs(5),
            tls: tls.client,
            resolver: Arc::new(MapResolver::default()),
            fallback: FallbackMap::empty(),
            clock: Arc::new(TestClock::new(0.0)),
            ..SessionConfig::default()
        });
        app.update_config(
            serde_json::json!({"proxy": [format!("http://127.0.0.1:{proxy_port}")]})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        let mut filters = Filters::load(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();
        let outcome = app
            .login_exhentai(&mut filters, "synthetic-user", "synthetic-pass")
            .await
            .unwrap();
        assert!(outcome.cookies.is_some());
        assert!(app.has_login());
        assert_eq!(app.cookie("ipb_member_id"), Some("42"));
        assert_eq!(app.cookie("ipb_pass_hash"), Some("deadbeef"));
        assert_eq!(app.cookies().len(), 2);
        assert_eq!(
            app.cookie_header(),
            Some("ipb_member_id=42;ipb_pass_hash=deadbeef")
        );
        let request = recorded.lock().unwrap()[0].to_ascii_lowercase();
        assert!(
            request.contains("post /index.php?act=login&code=01 http/1.1"),
            "{request}"
        );
        assert!(
            request.contains("content-type: application/x-www-form-urlencoded"),
            "{request}"
        );
        assert!(request.contains("username=synthetic-user"), "{request}");
        assert!(request.contains("password=synthetic-pass"), "{request}");
        let session_text = std::fs::read_to_string(root.path().join("h.json")).unwrap();
        assert!(session_text.contains("ipb_member_id"), "{session_text}");
        assert!(session_text.contains("ipb_pass_hash"), "{session_text}");
        server.await.unwrap();
        proxy_task.abort();
    }

    #[tokio::test]
    async fn site_end_to_end_metadata_page_image_queue_and_file_bytes() {
        use crate::core::{ImageDownload, ImageScan};
        use crate::filters::Filters;
        use crate::worker::SessionConfig;
        use std::path::Path;

        let tls = test_tls();
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target_addr = target.local_addr().unwrap();
        let acceptor = tls.acceptor.clone();
        let image_hash = "0123456789abcdef0123456789abcdef01234567";
        let image_bytes: Vec<u8> = (0..4096u32).map(|value| (value % 251) as u8).collect();
        let image_size = image_bytes.len();
        let image_src =
            format!("https://img.test/{image_hash}-{image_size}-200-0-jpg/keystamp=abc");
        let image_page_body = format!(
            "<img src=\"{image_src}\" style=\"max-width:100%\">\
             </a></div><div>unique-name.jpg :: 100x200 :: 2 KB</di>\
             return nl('123456-1')"
        );
        let metadata_body = "=\"gj\">日本語タイトル</h1>=\"gn\">English title</h1>\
            Length:</td><td class=\"gdt2\">1 pages</td>\
            Showing 1 - 20 of 1 images";
        let page_body =
            "<a href=\"https://e-hentai.org/s/abcdef0123/123456-1\"><div title=\"Page 1\"";
        let image_body = image_bytes.clone();
        let server = tokio::spawn(async move {
            for _ in 0..4 {
                let (socket, _) = target.accept().await.unwrap();
                let mut socket = acceptor.accept(socket).await.unwrap();
                let request = read_headers(&mut socket).await;
                let text = String::from_utf8_lossy(&request);
                let path = text
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("");
                if path.starts_with("/s/") {
                    respond_http(&mut socket, &image_page_body, "200 OK").await;
                } else if path.starts_with("/g/") && path.contains("?p=") {
                    respond_http(&mut socket, page_body, "200 OK").await;
                } else if path.starts_with("/g/") {
                    respond_http(&mut socket, metadata_body, "200 OK").await;
                } else {
                    let mut response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                        image_body.len()
                    )
                    .into_bytes();
                    response.extend_from_slice(&image_body);
                    socket.write_all(&response).await.unwrap();
                    socket.shutdown().await.unwrap();
                }
            }
        });
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let proxy_task = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = proxy_listener.accept().await else {
                    return;
                };
                let request = read_headers(&mut socket).await;
                let text = String::from_utf8_lossy(&request);
                if !text.starts_with("CONNECT ") {
                    respond_http(&mut socket, "unexpected", "500 Oops").await;
                    continue;
                }
                socket
                    .write_all(b"HTTP/1.1 200 Connection established\r\n\r\n")
                    .await
                    .unwrap();
                let Ok(mut upstream) = TcpStream::connect(target_addr).await else {
                    return;
                };
                let _ = tokio::io::copy_bidirectional(&mut socket, &mut upstream).await;
            }
        });

        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut app = super::test_app(root.path(), &["guid0001"]);
        let gallery_url = "https://e-hentai.org/g/123/abcdef0123/";
        let (code, guid) = app.add_task(gallery_url, serde_json::Map::new()).unwrap();
        assert_eq!(code, 0);
        let guid = guid.expect("guid");
        app.set_session_config(SessionConfig {
            retry: 1,
            timeout: Duration::from_secs(5),
            tls: tls.client,
            resolver: Arc::new(MapResolver::default()),
            fallback: FallbackMap::empty(),
            clock: Arc::new(TestClock::new(0.0)),
            ..SessionConfig::default()
        });
        app.update_config(
            serde_json::json!({"proxy": [format!("http://127.0.0.1:{proxy_port}")]})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        app.start_task(&guid).unwrap();
        let mut filters = Filters::load(Path::new(env!("CARGO_MANIFEST_DIR"))).unwrap();

        app.fetch_metadata(&guid, &mut filters).await.unwrap();
        {
            let task = app.task(&guid).unwrap();
            assert_eq!(task.state, crate::r#const::TASK_STATE_SCAN_PAGE);
            assert_eq!(task.meta_i64("total").unwrap(), 1);
            assert_eq!(task.meta_i64("thumbnail_cnt").unwrap(), 20);
            assert_eq!(task.meta_str("gnname").unwrap(), "English title");
        }

        app.scan_pages(&guid, &mut filters).await.unwrap();
        {
            let task = app.task(&guid).unwrap();
            assert_eq!(task.state, crate::r#const::TASK_STATE_SCAN_IMG);
            assert_eq!(task.page_q.as_ref().unwrap().len(), 1);
            assert!(task.page_q.as_ref().unwrap()[0].contains("/s/abcdef0123/123456-1"));
        }

        let scanned = app.scan_one_page(&guid, &mut filters).await.unwrap();
        match scanned {
            ImageScan::Enqueued {
                image_url,
                filename,
            } => {
                assert_eq!(image_url, image_src);
                assert_eq!(filename, "unique-name.jpg");
            }
            other => panic!("expected enqueued image, got {other:?}"),
        }
        {
            let task = app.task(&guid).unwrap();
            assert_eq!(task.img_q.as_ref().unwrap().len(), 1);
            assert_eq!(task.img_q.as_ref().unwrap()[0], image_src);
            assert_eq!(
                task.renamed_map
                    .iter()
                    .find(|(key, _)| key.as_fid().ok() == Some(1))
                    .map(|(_, name)| name.as_str()),
                Some("unique-name.jpg")
            );
        }

        let downloaded = app.download_one_image(&guid, &mut filters).await.unwrap();
        let path = match downloaded {
            ImageDownload::Saved(path) => path,
            other => panic!("expected saved file, got {other:?}"),
        };
        assert_eq!(std::fs::read(&path).unwrap(), image_bytes);
        assert_eq!(app.task(&guid).unwrap().meta_i64("finished").unwrap(), 1);
        assert!(app.task(&guid).unwrap().flist_done().contains(&1));
        server.await.unwrap();
        proxy_task.abort();
    }

    #[tokio::test]
    async fn proxy_config_update_adds_to_the_shared_pool() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut app = super::test_app(root.path(), &[]);
        app.update_config(
            serde_json::json!({"proxy": ["http://127.0.0.1:8081"]})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(app.proxy_count(), 1);
        let pool = app.proxy_pool().unwrap();
        let session = app.http_session(ProxyPolicy::All);
        assert!(
            session
                .proxy
                .as_ref()
                .is_some_and(|shared| Arc::ptr_eq(shared, &pool))
        );
        app.update_config(
            serde_json::json!({"proxy": ["socks5h://127.0.0.1:1080"]})
                .as_object()
                .unwrap()
                .clone(),
        )
        .unwrap();
        assert_eq!(app.proxy_count(), 2);
        {
            let pool = pool.lock().unwrap();
            assert_eq!(
                pool.proxies.keys().cloned().collect::<Vec<_>>(),
                vec![
                    "http://127.0.0.1:8081".to_string(),
                    "socks5h://127.0.0.1:1080".to_string()
                ]
            );
        }
    }

    #[tokio::test]
    async fn glype_403_binary_streams_real_bytes_instead_of_empty_success() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let body = vec![0x5au8; 2 * 1024 * 1024];
        let expected = body.clone();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_headers(&mut socket).await;
            let mut response = format!(
                "HTTP/1.1 403 Forbidden\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\n\r\n",
                body.len()
            )
            .into_bytes();
            response.extend_from_slice(&body);
            socket.write_all(&response).await.unwrap();
            socket.shutdown().await.unwrap();
        });
        let mut pool = Pool::new();
        let addr = format!("http://127.0.0.1:{port}/browse.php?u=x&b=4");
        pool.add_proxy(&addr).unwrap();
        let mut session = session(
            Some(Arc::new(Mutex::new(pool))),
            Arc::new(MapResolver::default()),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        let outcome = session
            .request("GET", "https://img.test/big.jpg", None, true)
            .await
            .expect("glype binary");
        assert_eq!(outcome.response.status, 403);
        assert_eq!(outcome.response.content_length, expected.len() as i64);
        assert!(outcome.response.body.is_empty());
        let mut stream = outcome.response.stream.expect("stream");
        let mut read = Vec::new();
        while let Some(chunk) = stream.next_chunk().await.expect("chunk") {
            read.extend_from_slice(&chunk);
        }
        assert_eq!(read.len(), expected.len());
        assert_eq!(read, expected);
        stream.close().await;
        server.await.unwrap();
    }

    #[tokio::test]
    async fn image_only_policy_proxies_non_webpage_urls() {
        let proxy_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let proxy_task = tokio::spawn(async move {
            let (mut socket, _) = proxy_listener.accept().await.unwrap();
            let request = read_headers(&mut socket).await;
            assert!(
                String::from_utf8_lossy(&request)
                    .starts_with("GET http://img.test/pic.jpg HTTP/1.1")
            );
            respond_http(&mut socket, "image-via-proxy", "200 OK").await;
        });
        let mut pool = Pool::new();
        pool.add_proxy(&format!("http://127.0.0.1:{proxy_port}"))
            .unwrap();
        let resolver = Arc::new(MapResolver::default());
        let mut session = session(
            Some(Arc::new(Mutex::new(pool))),
            Arc::clone(&resolver),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::ImageOnly,
        );
        let outcome = session
            .request("GET", "http://img.test/pic.jpg", None, false)
            .await
            .expect("image proxy");
        assert_eq!(outcome.response.text, "image-via-proxy");
        assert!(resolver.calls().is_empty());
        proxy_task.await.unwrap();
    }

    #[tokio::test]
    async fn glype_error_div_is_a_pool_exception_not_a_transport_failure() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            read_headers(&mut socket).await;
            respond_http(
                &mut socket,
                "<div id=\"error\">proxy says no</div>",
                "200 OK",
            )
            .await;
        });
        let mut pool = Pool::new();
        let addr = format!("http://127.0.0.1:{port}/browse.php?u=x&b=4");
        pool.add_proxy(&addr).unwrap();
        let pool = Arc::new(Mutex::new(pool));
        let mut session = session(
            Some(pool.clone()),
            Arc::new(MapResolver::default()),
            FallbackMap::empty(),
            TlsClient::production(),
            ProxyPolicy::All,
        );
        let error = match session
            .request("GET", "https://e-hentai.org/g/1/", None, false)
            .await
        {
            Ok(_) => panic!("expected glype error"),
            Err(error) => error,
        };
        assert!(matches!(error, HttpError::Glype(_)), "{error}");
        assert_eq!(
            pool.lock().unwrap().counts(&addr),
            Some((0, 0)),
            "PoolException is not counted as a proxy failure"
        );
        server.await.unwrap();
    }
}
