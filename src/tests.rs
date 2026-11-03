//! Cross-module and process-level tests.
#![allow(clippy::field_reassign_with_default)]

use std::fs;
use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
fn update_config_stores_rpc_start_values_without_unavailable() {
    let root = tempfile::tempdir().expect("root");
    let logger = crate::util::logger::Logger::with_output(Box::new(Vec::new()), false);
    let mut app =
        crate::core::App::new(Config::default(), logger, root.path(), root.path()).expect("app");
    let mut patch = serde_json::Map::new();
    patch.insert("rpc_interface".into(), serde_json::json!("127.0.0.1"));
    patch.insert("rpc_port".into(), serde_json::json!(8123));
    patch.insert("rpc_secret".into(), serde_json::json!("s3cret"));
    app.update_config(patch)
        .expect("rpc start values must be accepted");
    assert_eq!(app.cfg.rpc_interface, "127.0.0.1");
    assert_eq!(app.cfg.rpc_port, Some(8123));
    assert_eq!(app.cfg.rpc_secret.as_deref(), Some("s3cret"));
    let mut clear = serde_json::Map::new();
    clear.insert("rpc_port".into(), serde_json::Value::Null);
    app.update_config(clear).expect("null clears the port");
    assert_eq!(app.cfg.rpc_port, None);
    let mut invalid = serde_json::Map::new();
    invalid.insert("rpc_port".into(), serde_json::json!("not a port"));
    assert!(app.update_config(invalid).is_err());
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
    // An unusable URL still exercises the real startup: configuration, log,
    // session restore, a submitted task and the URL-mode exit.
    let argv = [
        "xeHentai",
        "-d",
        "dl",
        "-l",
        "eh.log",
        "--auto-update",
        "off",
        "not-a-url",
    ];
    assert_eq!(crate::run_with_args(root.path(), argv), 0);
    assert!(root.path().join(CONFIG_FILE).is_file());
    let log = fs::read_to_string(root.path().join("eh.log")).expect("log file");
    assert!(log.contains("xeHentai 2.023-rss started."), "{log}");
    assert!(log.contains("application task loop finished"), "{log}");
}

#[test]
fn entry_point_imports_legacy_python_once() {
    let root = tempfile::tempdir().expect("root");
    fs::write(
        root.path().join(crate::config::LEGACY_CONFIG_FILE),
        "dir = \"from-python\"\nlog_verbose = 2\n",
    )
    .unwrap();
    let argv = [
        "xeHentai",
        "-d",
        "dl",
        "-l",
        "eh.log",
        "--auto-update",
        "off",
        "not-a-url",
    ];
    assert_eq!(crate::run_with_args(root.path(), argv), 0);
    let rss = fs::read_to_string(root.path().join(CONFIG_FILE)).unwrap();
    assert!(rss.contains("from-python"));
    // The log file is opened by the config update, after the first import.
    let log = fs::read_to_string(root.path().join("eh.log")).unwrap();
    assert!(log.contains("started."), "{log}");
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
        "monitor" => compat_monitor(&request["inputs"]),
        "download" => compat_download(&request["inputs"]),
        "rpc" => compat_rpc(&request["inputs"]),
        "gallery" => compat_gallery(&request["inputs"]),
        "webui" => compat_webui(&request["inputs"]),
        "cli" => compat_cli(&request["inputs"]),
        "update" => compat_update_probe(&request["inputs"]),
        other => panic!("unknown probe suite {other}"),
    };
    let output = std::env::var("XEH_COMPAT_OUTPUT").expect("probe output");
    fs::write(
        output,
        serde_json::to_string_pretty(&result).expect("probe JSON"),
    )
    .expect("write probe output");
}

/// Update probe entry: the module is Unix-only (real child processes and POSIX
/// signals). Off Unix it fails closed instead of pretending to observe.
#[cfg(unix)]
fn compat_update_probe(inputs: &serde_json::Value) -> serde_json::Value {
    m7p::compat_update(inputs)
}

#[cfg(not(unix))]
fn compat_update_probe(_inputs: &serde_json::Value) -> serde_json::Value {
    panic!("the update compatibility probe is not supported on this platform")
}

/// M6 CLI projection: the reference `argparse.Namespace` shape, produced by the
/// production parser (`crate::cli::parse_from`), not by a second parser.
fn cli_projection(opts: &crate::cli::Opts) -> serde_json::Value {
    serde_json::json!({
        "username": opts.username,
        "key": opts.key,
        "cookie": opts.cookie,
        "interactive": opts.interactive,
        "daemon": opts.daemon,
        "urls": opts.urls,
        "dir": opts.dir,
        "download_ori": opts.download_ori,
        "jpn_title": opts.jpn_title,
        "rename_ori": opts.rename_ori,
        "proxy": opts.proxy,
        "proxy_image": opts.proxy_image,
        "proxy_image_only": opts.proxy_image_only,
        "rpc_interface": opts.rpc_interface,
        "rpc_port": opts.rpc_port,
        "rpc_secret": opts.rpc_secret,
        "rpc_open_browser": opts.rpc_open_browser,
        "delete_task_files": opts.delete_task_files,
        "make_archive": opts.make_archive,
        "download_range": opts.download_range,
        "download_thread_cnt": opts.download_thread_cnt,
        "download_timeout": opts.download_timeout,
        "low_speed_threshold": opts.low_speed_threshold,
        "ignored_errors": opts.ignored_errors,
        "auto_update": opts.auto_update,
        "update_beta_channel": opts.update_beta_channel,
        "log_path": opts.log_path,
        "verbose": opts.verbose,
    })
}

fn compat_cli(inputs: &serde_json::Value) -> serde_json::Value {
    let root = std::path::PathBuf::from(inputs["root"].as_str().unwrap());
    let loaded = crate::config::Config::load(&root).expect("cli probe config");
    // The reference computes its `-d`/`-l` defaults with `os.path.abspath`,
    // which is relative to the process working directory. This probe runs as
    // the only test in its process (`--exact`), so it reproduces that by
    // working in the case root, exactly like the reference subprocess does.
    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&root).expect("probe cwd");
    let mut cases = Vec::new();
    for case in inputs["cases"].as_array().expect("cases") {
        let key = case["key"].as_str().expect("key").to_string();
        let argv: Vec<String> = case["argv"]
            .as_array()
            .expect("argv")
            .iter()
            .map(|value| value.as_str().expect("argv string").to_string())
            .collect();
        let mut full = vec!["xeH".to_string()];
        full.extend(argv);
        let entry = match crate::cli::parse_from(full, &loaded.config, crate::i18n::Locale::EnUs) {
            Ok(crate::cli::Parsed::Opts(opts)) => {
                serde_json::json!({"key": key, "exit": 0, "namespace": cli_projection(&opts)})
            }
            Ok(crate::cli::Parsed::Help(text)) | Ok(crate::cli::Parsed::Version(text)) => {
                serde_json::json!({"key": key, "exit": 0, "text": text})
            }
            Err(error) => serde_json::json!({
                "key": key,
                "exit": 2,
                "usage_error": true,
                "stdout": "",
                "stderr": error.message,
            }),
        };
        cases.push(entry);
    }
    let mut range = Vec::new();
    for token in inputs["range_tokens"].as_array().expect("range tokens") {
        let token = token.as_str().expect("token");
        match crate::cli::parse_range(token, crate::i18n::Locale::EnUs) {
            Ok(value) => range.push(serde_json::json!({
                "token": token,
                "ok": true,
                "value": value,
            })),
            Err(message) => range.push(serde_json::json!({
                "token": token,
                "ok": false,
                "error_class": "ArgumentTypeError",
                "message": message,
            })),
        }
    }
    std::env::set_current_dir(&previous).expect("restore cwd");
    serde_json::json!({
        "root": root.to_string_lossy(),
        "cases": cases,
        "range": range,
    })
}

fn compat_monitor_app(scenario: &serde_json::Value) -> crate::core::App {
    use crate::config::Config;
    use crate::task::{Task, TaskConfig};

    let root = tempfile::tempdir().expect("monitor root").keep();
    let mut config = Config::default();
    config.dir = root.join("downloads").to_string_lossy().into_owned();
    config.save_tasks = false;
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    let mut app = crate::core::App::with_guid_source(
        config,
        crate::util::logger::Logger::with_output(Box::new(Vec::new()), false),
        &root,
        &root,
        Box::new(crate::core::FixedGuids::new(vec!["guidmon1".into()])),
    )
    .unwrap();
    let task_config = TaskConfig::from_app(&app.cfg, &serde_json::Map::new()).unwrap();
    let task = Task::new(
        "http://e-hentai.org/g/123/abcdef0123/",
        task_config,
        "guidmon1".into(),
        &root,
    )
    .unwrap();
    let guid = app.insert_task_for_test(task);
    let task = app.task_mut(&guid).unwrap();
    if let Some(ignored) = scenario["ignored"].as_array() {
        task.config.ignored_errors = ignored
            .iter()
            .filter_map(serde_json::Value::as_i64)
            .collect();
    }
    task.state = crate::r#const::TASK_STATE_DOWNLOAD;
    task.meta
        .insert("title".into(), serde_json::json!("compat"));
    task.meta.insert(
        "total".into(),
        serde_json::json!(scenario["total"].as_i64().unwrap_or(0)),
    );
    task.meta.insert(
        "finished".into(),
        serde_json::json!(scenario["finished"].as_i64().unwrap_or(0)),
    );
    task.img_q = Some(std::collections::VecDeque::new());
    if let Some(pending) = scenario["pending"].as_u64() {
        let queue = task.img_q.as_mut().unwrap();
        for _ in 0..pending {
            queue.push_back("x".into());
        }
    }
    app
}

fn compat_monitor(inputs: &serde_json::Value) -> serde_json::Value {
    use crate::core::{Runtime, WorkerRef};
    use crate::worker::{SpeedChecker, StepClock, TickClock};

    let speed = &inputs["speed"];
    let start = speed["start"].as_f64().unwrap_or(100.0);
    let clock = std::sync::Arc::new(StepClock::new(start, 0.0));
    let mut checker = SpeedChecker::new(speed["cnt"].as_u64().unwrap_or(5) as usize, clock.clone());
    let mut speed_trace = Vec::new();
    let mut now = start;
    let resets: Vec<usize> = speed["resets"]
        .as_array()
        .map(|values| {
            values
                .iter()
                .filter_map(|value| value.as_u64().map(|v| v as usize))
                .collect()
        })
        .unwrap_or_default();
    for (index, step) in speed["timeline"].as_array().unwrap().iter().enumerate() {
        let size = step[0].as_u64().unwrap_or(0) as usize;
        let delta = step[1].as_f64().unwrap_or(0.0);
        now += delta;
        clock.set_now(now);
        checker.check(size);
        speed_trace.push(serde_json::json!([
            round6(checker.calc(false)),
            round6(checker.calc(true)),
            checker.sample_count(),
        ]));
        if resets.contains(&index) {
            checker.reset();
            speed_trace.push(serde_json::json!([
                "reset",
                round6(checker.calc(false)),
                checker.sample_count(),
            ]));
        }
    }

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("compat runtime");
    let monitors = runtime.block_on(async {
        tokio::task::LocalSet::new()
            .run_until(async {
                let mut output = serde_json::Map::new();
                for (name, scenario) in inputs["monitor"].as_object().unwrap() {
                    let mut app = crate::tests::compat_monitor_app(scenario);
                    let guid = app
                        .all_task_guids()
                        .into_iter()
                        .next()
                        .expect("monitor task");
                    app.set_session_config(crate::worker::SessionConfig {
                        clock: std::sync::Arc::new(TickClock::new(1000.0)),
                        ..crate::worker::SessionConfig::default()
                    });
                    let runtime =
                        Runtime::new(app, std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
                    let handle = runtime.handle();
                    let names: Vec<String> = scenario["names"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|value| value.as_str().unwrap().to_string())
                        .collect();
                    let (monitor, join) = runtime.start_monitor_for_test(&guid, &names);
                    let sample = scenario["speed_bytes"].as_u64().unwrap_or(0) as usize;
                    let mut refs = Vec::new();
                    for name in &names {
                        let speed = if sample == 0 {
                            None
                        } else {
                            let clock = std::sync::Arc::new(StepClock::new(0.0, 1.1));
                            let mut checker = SpeedChecker::new(5, clock);
                            for _ in 0..5 {
                                checker.check(sample);
                            }
                            Some(std::sync::Arc::new(std::sync::Mutex::new(checker)))
                        };
                        let reference = WorkerRef {
                            name: name.clone(),
                            alive: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
                            speed,
                            zombie_threshold: scenario["thresholds"]
                                .get(name)
                                .and_then(serde_json::Value::as_f64)
                                .unwrap_or(30.0),
                        };
                        monitor.register(reference.clone());
                        monitor.keepalive(&reference, false);
                        refs.push(reference);
                    }
                    let mut events = Vec::new();
                    for vote in scenario["votes"].as_array().into_iter().flatten() {
                        let code = vote[1].as_i64().unwrap_or(0);
                        monitor.vote(vote[0].as_str().unwrap(), code);
                        events.push(serde_json::json!(format!("vote:{code}")));
                    }
                    let finished_updates: Vec<(usize, i64)> = scenario["finished_updates"]
                        .as_array()
                        .map(|values| {
                            values
                                .iter()
                                .map(|row| {
                                    (
                                        row[0].as_u64().unwrap_or(0) as usize,
                                        row[1].as_i64().unwrap_or(0),
                                    )
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    let finish_when_complete =
                        scenario["finish_when_complete"].as_bool().unwrap_or(false);
                    let zero_after = scenario["zero_speed_after_stats"].as_u64();

                    let max_ticks = scenario["max_ticks"].as_u64().unwrap_or(1000);
                    monitor.set_tick_limit(max_ticks);
                    let mut stats = 0usize;
                    let mut applied = 0usize;
                    let mut guard = 0u64;
                    let mut guard_hit = false;
                    loop {
                        let trace = handle.trace();
                        let current_stats = trace
                            .iter()
                            .filter(|event| event.kind == "monitor.stats")
                            .count();
                        if current_stats != stats {
                            stats = current_stats;
                            while applied < finished_updates.len()
                                && finished_updates[applied].0 <= stats
                            {
                                let finished = finished_updates[applied].1;
                                handle.borrow_app_mut(|app| {
                                    if let Some(task) = app.task_mut(&guid) {
                                        task.meta
                                            .insert("finished".into(), serde_json::json!(finished));
                                    }
                                });
                                applied += 1;
                            }
                            if zero_after.is_some_and(|after| stats as u64 >= after) {
                                for reference in &refs {
                                    if let Some(speed) = &reference.speed {
                                        speed.lock().unwrap().reset();
                                    }
                                }
                            }
                            let finished = handle.borrow_app(|app| {
                                app.task(&guid)
                                    .and_then(|task| {
                                        task.meta
                                            .get("finished")
                                            .and_then(serde_json::Value::as_i64)
                                    })
                                    .unwrap_or(0)
                            });
                            let total = scenario["total"].as_i64().unwrap_or(0);
                            if finish_when_complete && finished == total {
                                for reference in &refs {
                                    monitor.keepalive(reference, true);
                                }
                            }
                        }
                        if join.is_finished() {
                            break;
                        }
                        guard += 1;
                        if guard > 200_000 {
                            guard_hit = true;
                            monitor.request_exit();
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                    }
                    let _ = join.await;
                    let finished_now = handle.borrow_app(|app| {
                        app.task(&guid)
                            .and_then(|task| {
                                task.meta
                                    .get("finished")
                                    .and_then(serde_json::Value::as_i64)
                            })
                            .unwrap_or(0)
                    });
                    let total = scenario["total"].as_i64().unwrap_or(0);
                    let stopped = (monitor.limit_hit() && finished_now != total) || guard_hit;
                    let trace = handle.trace();
                    for event in &trace {
                        match event.kind.as_str() {
                            "monitor.stats" => events.push(serde_json::json!("stats")),
                            "monitor.slow" => events.push(serde_json::json!("slow")),
                            "monitor.stuck" => events.push(serde_json::json!("stuck")),
                            "monitor.quota" => events.push(serde_json::json!("quota")),
                            "monitor.zombie" => events.push(serde_json::json!("zombie")),
                            "monitor.finished" => events.push(serde_json::json!("finished")),
                            _ => {}
                        }
                    }
                    let state = handle.borrow_app(|app| app.task(&guid).unwrap().state);
                    let finished = handle.borrow_app(|app| {
                        app.task(&guid)
                            .and_then(|task| {
                                task.meta
                                    .get("finished")
                                    .and_then(serde_json::Value::as_i64)
                            })
                            .unwrap_or(0)
                    });
                    let download_speed = round6(monitor.download_speed());
                    output.insert(
                        name.clone(),
                        serde_json::json!({
                            "events": events,
                            "state": state,
                            "finished": finished,
                            "download_speed": download_speed,
                            "stopped": stopped,
                            "stats": stats,
                        }),
                    );
                }
                serde_json::Value::Object(output)
            })
            .await
    });
    serde_json::json!({"speed": speed_trace, "monitor": monitors})
}

fn round6(value: f64) -> f64 {
    (value * 1_000_000.0).round() / 1_000_000.0
}

fn hex_bytes(text: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index + 1 < bytes.len() {
        let high = (bytes[index] as char).to_digit(16).expect("hex high");
        let low = (bytes[index + 1] as char).to_digit(16).expect("hex low");
        out.push(((high << 4) | low) as u8);
        index += 2;
    }
    out
}

async fn compat_read_head<S: tokio::io::AsyncRead + Unpin>(socket: &mut S) -> String {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let read = socket.read(&mut byte).await.unwrap_or(0);
        if read == 0 {
            break;
        }
        head.push(byte[0]);
        if head.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

fn compat_path_of(head: &str) -> String {
    head.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string()
}

async fn compat_respond<S: tokio::io::AsyncWrite + Unpin>(
    socket: &mut S,
    status: &str,
    content_type: &str,
    body: &[u8],
) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body).await;
    let _ = socket.shutdown().await;
}

fn compat_download(inputs: &serde_json::Value) -> serde_json::Value {
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    use crate::config::Config;
    use crate::core::{Runtime, TraceEvent};
    use crate::proxy::FixedResolver;
    use crate::task::{Task, TaskConfig};
    use crate::worker::{FallbackMap, SessionConfig};

    let scenario = &inputs["scenario"];
    let url = scenario["url"].as_str().unwrap();
    let metadata = scenario["metadata"].as_str().unwrap().as_bytes().to_vec();
    let page = scenario["page"].as_str().unwrap().as_bytes().to_vec();
    let img_pages: Vec<Vec<u8>> = scenario["img_pages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().as_bytes().to_vec())
        .collect();
    let images: Vec<Vec<u8>> = scenario["images"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| hex_bytes(value.as_str().unwrap()))
        .collect();
    let image_hashes: Vec<String> = scenario["image_hashes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|value| value.as_str().unwrap().to_string())
        .collect();
    let root = std::path::PathBuf::from(inputs["root"].as_str().unwrap());
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("download runtime");
    runtime.block_on(async {
        tokio::task::LocalSet::new()
            .run_until(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let address: SocketAddr = listener.local_addr().unwrap();
                let requests = Arc::new(Mutex::new(Vec::new()));
                let seen = Arc::clone(&requests);
                let server = tokio::spawn(async move {
                    loop {
                        let Ok((mut socket, _)) = listener.accept().await else {
                            return;
                        };
                        let metadata = metadata.clone();
                        let page = page.clone();
                        let img_pages = img_pages.clone();
                        let images = images.clone();
                        let image_hashes = image_hashes.clone();
                        let seen = Arc::clone(&seen);
                        tokio::spawn(async move {
                            let head = compat_read_head(&mut socket).await;
                            let path = compat_path_of(&head);
                            seen.lock().unwrap().push(format!("GET {path}"));
                            if path.contains("?p=") {
                                compat_respond(&mut socket, "200 OK", "text/html; charset=utf-8", &page).await;
                            } else if path.contains("/img/") {
                                let index = if path.contains(&image_hashes[0]) {
                                    0
                                } else {
                                    1
                                };
                                compat_respond(&mut socket, "200 OK", "image/jpeg", &images[index]).await;
                            } else if path.contains("/s/") {
                                let fid = path
                                    .rsplit('-')
                                    .next()
                                    .and_then(|value| value.split('?').next())
                                    .and_then(|value| value.parse::<usize>().ok())
                                    .unwrap_or(1);
                                compat_respond(
                                    &mut socket,
                                    "200 OK",
                                    "text/html; charset=utf-8",
                                    &img_pages[fid.saturating_sub(1)],
                                )
                                .await;
                            } else {
                                compat_respond(&mut socket, "200 OK", "text/html; charset=utf-8", &metadata).await;
                            }
                        });
                    }
                });

                let mut config = Config::default();
                config.dir = root.join("downloads").to_string_lossy().into_owned();
                config.save_tasks = false;
                config.scan_thread_cnt = scenario["scan_threads"].as_i64().unwrap_or(1);
                config.download_thread_cnt = scenario["download_threads"].as_i64().unwrap_or(1);
                config.download_timeout = 5;
                config.low_speed_threshold = 0;
                config.make_archive = false;
                config.delete_task_files = false;
                let mut app = crate::core::App::with_guid_source(
                    config.clone(),
                    crate::util::logger::Logger::with_output(Box::new(Vec::new()), false),
                    &root,
                    &root,
                    Box::new(crate::core::FixedGuids::new(vec!["00000001".into()])),
                )
                .unwrap();
                let task_config = TaskConfig::from_app(&app.cfg, &serde_json::Map::new()).unwrap();
                let task = Task::new(url, task_config, "00000001".into(), &root).unwrap();
                let guid = app.insert_task_for_test(task);
                // Force the same 14-key snapshot values the Python task uses.
                let mut resolver = FixedResolver::new(address);
                resolver.map.insert("img.test".into(), address);
                app.set_session_config(SessionConfig {
                    retry: 10,
                    timeout: std::time::Duration::from_secs(10),
                    resolver: Arc::new(resolver),
                    fallback: FallbackMap::empty(),
                    clock: Arc::new(crate::worker::RealClock),
                    ..SessionConfig::default()
                });
                let runtime = Runtime::new(app, std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")));
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
                loop {
                    if handle.task_state(&guid) == Some(crate::r#const::TASK_STATE_FINISHED)
                        || handle.task_state(&guid) == Some(crate::r#const::TASK_STATE_FAILED)
                    {
                        break;
                    }
                    if std::time::Instant::now() > deadline {
                        panic!("compat download did not finish: {:?}", handle.trace());
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
                }
                handle.shutdown().await;
                run.await.unwrap();
                server.abort();
                let task = handle.borrow_app(|app| {
                    let task = app.task(&guid).unwrap();
                    let mut config = serde_json::to_value(&task.config).unwrap();
                    normalize_value(&mut config, &root);
                    let reload: Vec<serde_json::Value> = task
                        .reload_map
                        .iter()
                        .map(|(key, (page, name))| serde_json::json!([key, [page, name]]))
                        .collect();
                    let duplicate: Vec<serde_json::Value> = task
                        .duplicate_pairs()
                        .into_iter()
                        .map(|(fid, copies)| serde_json::json!([fid, copies]))
                        .collect();
                    let mut renamed: Vec<serde_json::Value> = task
                        .renamed_pairs()
                        .into_iter()
                        .map(|(key, name)| serde_json::json!([key, name]))
                        .collect();
                    renamed.sort_by_key(|value| value[0].to_string());
                    let mut reload = reload;
                    reload.sort_by_key(|value| value[0].to_string());
                    let mut duplicate = duplicate;
                    duplicate.sort_by_key(|value| value[0].to_string());
                    serde_json::json!({
                        "url": task.url,
                        "failcode": task.failcode,
                        "state": task.state,
                        "guid": "GUID",
                        "config": config,
                        "meta": task.meta,
                        "reload": reload,
                        "duplicate": duplicate,
                        "renamed": renamed,
                        "img_q": task.img_q.as_ref().map(|queue| queue.iter().cloned().collect::<Vec<_>>()),
                        "page_q": task.page_q.as_ref().map(|queue| queue.iter().cloned().collect::<Vec<_>>()),
                        "flist": task.flist_done().iter().copied().collect::<Vec<_>>(),
                        "gid": task.gid,
                        "sethash": task.sethash,
                    })
                });
                let tree = compat_tree(&root.join("downloads"), &root);
                let mut requests = requests.lock().unwrap().clone();
                requests.sort();
                let trace: Vec<TraceEvent> = handle.trace();
                let mut votes = std::collections::BTreeMap::<i64, i64>::new();
                for event in &trace {
                    if event.kind == "monitor.vote" {
                        let code = event.detail["code"].as_i64().unwrap_or(0);
                        *votes.entry(code).or_insert(0) += 1;
                    }
                }
                let finished = task["meta"]["finished"].clone();
                serde_json::json!({
                    "task": task,
                    "tree": tree,
                    "requests": requests,
                    "monitor": {
                        "state": task["state"].clone(),
                        "finished": finished,
                        "votes": votes.into_iter().map(|(code, count)| serde_json::json!([code.to_string(), count])).collect::<Vec<_>>(),
                    },
                })
            })
            .await
    })
}

fn normalize_value(value: &mut serde_json::Value, root: &std::path::Path) {
    let root = root.to_string_lossy().into_owned();
    match value {
        serde_json::Value::String(text) => {
            if let Some(rest) = text.strip_prefix(&root) {
                *text = format!("<ROOT>{rest}");
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                normalize_value(item, std::path::Path::new(&root));
            }
        }
        serde_json::Value::Object(map) => {
            for item in map.values_mut() {
                normalize_value(item, std::path::Path::new(&root));
            }
        }
        _ => {}
    }
}

fn compat_tree(dir: &std::path::Path, _root: &std::path::Path) -> Vec<serde_json::Value> {
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<serde_json::Value>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name == "h.json" || name == "eh.log" {
                continue;
            }
            let relative = path.strip_prefix(root).unwrap_or(&path);
            if path.is_dir() {
                out.push(serde_json::json!({
                    "path": relative.to_string_lossy().replace('\\', "/"),
                    "type": "dir",
                }));
                walk(&path, root, out);
            } else {
                let data = std::fs::read(&path).unwrap_or_default();
                let text = if data.len() <= 4096 {
                    String::from_utf8(data.clone()).ok()
                } else {
                    None
                };
                out.push(serde_json::json!({
                    "path": relative.to_string_lossy().replace('\\', "/"),
                    "type": "file",
                    "size": data.len(),
                    "text": text,
                }));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort_by_key(|value| value["path"].to_string());
    out
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
    let permissions = fs::metadata(root.path()).unwrap().permissions();
    let original = permissions.clone();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut permissions = permissions;
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
    let variants: Vec<(&str, String)> = {
        let variants = vec![
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
        let variants = {
            let mut variants = variants;
            std::os::unix::fs::symlink(&outside, base.join("link")).unwrap();
            variants.insert(2, ("link0001", "link".to_string()));
            variants
        };
        variants
    };
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
    #[cfg(unix)]
    let rejected = vec!["prefix01", "dotdot01", "abs00001", "link0001"];
    #[cfg(not(unix))]
    let rejected = vec!["prefix01", "dotdot01", "abs00001"];
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
        crate::run_with_args(
            root.path(),
            [
                "xeHentai",
                "-d",
                "downloads",
                "-l",
                "eh.log",
                "--auto-update",
                "off",
                "not-a-url",
            ],
        ),
        0,
        "a rejected task must not fail the run"
    );
    // The unsafe task is rejected before any I/O: it is never re-published,
    // and the run exits normally rather than failing on the other task.
    let session: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(root.path().join("h.json")).unwrap()).unwrap();
    assert!(
        !session.to_string().contains("unsafe01"),
        "the rejected task survived: {session}"
    );
    let log = fs::read_to_string(root.path().join("eh.log")).expect("log file");
    assert!(log.contains("started."), "{log}");
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
    let task = prepared_task(root.path(), "guid0001", "badarchive", 1, |_| {});
    let gallery = task.get_fpath().unwrap();
    fs::create_dir_all(&gallery).unwrap();
    fs::write(gallery.join("1.jpg"), b"data").unwrap();
    #[cfg(unix)]
    {
        let mut task = task;
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
    let downloads = root.path().join("downloads");
    fs::create_dir_all(&downloads).unwrap();
    #[cfg(unix)]
    {
        // The gallery symlink refusal is proven on Unix, where `symlink(2)`
        // needs no extra privilege; the task only exists for that probe.
        let task = prepared_task(root.path(), "guid0002", "safe", 1, |_| {});
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
        TaskError::RollbackFailed { .. } => "OSError",
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

    struct DownloadFixture {
        app: crate::core::App,
        guid: String,
        image_url: String,
        reload_url: String,
        task_dir: std::path::PathBuf,
        sentinel: std::path::PathBuf,
    }

    fn prepare_download_fixture(root: &std::path::Path, port: u16) -> DownloadFixture {
        const HASH: &str = "0123456789abcdef0123456789abcdef01234567";
        let image_url = format!("http://127.0.0.1:{port}/{HASH}-4096-200-0-jpg/keystamp=abc");
        let mut app = super::test_app(root, &["guid0001"]);
        app.set_session_config(SessionConfig {
            retry: 1,
            timeout: Duration::from_secs(5),
            resolver: Arc::new(MapResolver::default()),
            fallback: FallbackMap::empty(),
            clock: Arc::new(TestClock::new(0.0)),
            ..SessionConfig::default()
        });
        let (code, guid) = app
            .add_task(
                "https://e-hentai.org/g/123/abcdef0123/",
                serde_json::Map::new(),
            )
            .unwrap();
        assert_eq!(code, 0);
        let guid = guid.unwrap();
        app.start_task(&guid).unwrap();
        let reload = format!("https://e-hentai.org/s/abcdef0123/{HASH}-1");
        let task = app.task_mut(&guid).unwrap();
        task.meta.insert(
            "title".into(),
            serde_json::Value::String("Test Gallery".into()),
        );
        task.meta
            .insert("total".into(), serde_json::Value::Number(1.into()));
        task.put_img_queue(&image_url, &reload, "pic.jpg").unwrap();
        task.img_q = Some(std::collections::VecDeque::from([image_url.clone()]));
        let task_dir = app.task(&guid).unwrap().get_fpath().unwrap();
        std::fs::create_dir_all(&task_dir).unwrap();
        let sentinel = task_dir.join("sentinel.txt");
        std::fs::write(&sentinel, b"old-state").unwrap();
        DownloadFixture {
            app,
            guid,
            image_url,
            reload_url: reload,
            task_dir,
            sentinel,
        }
    }

    fn stage_names(dir: &std::path::Path) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("xehentai-"))
            .collect();
        names.sort();
        names
    }

    fn assert_clean_stage(fixture: &DownloadFixture) {
        assert!(
            stage_names(&fixture.task_dir).is_empty(),
            "leftover stages: {:?}",
            stage_names(&fixture.task_dir)
        );
        assert!(
            !fixture.task_dir.join("1.jpg").exists(),
            "no destination may be published"
        );
        let task = fixture.app.task(&fixture.guid).unwrap();
        assert!(
            task.meta.get("finished").is_none(),
            "finished must not move before publish"
        );
        assert_eq!(
            std::fs::read(&fixture.sentinel).unwrap(),
            b"old-state",
            "a pre-existing file in the task directory is untouched"
        );
    }

    fn refill_image_queue(fixture: &mut DownloadFixture) {
        let task = fixture.app.task_mut(&fixture.guid).unwrap();
        task.img_q = Some(std::collections::VecDeque::from([fixture
            .image_url
            .clone()]));
    }

    fn image_body() -> Vec<u8> {
        (0..4096u32).map(|value| (value % 251) as u8).collect()
    }

    async fn serve_full_image(socket: &mut (impl AsyncRead + AsyncWrite + Unpin), bytes: &[u8]) {
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
            bytes.len()
        )
        .into_bytes();
        response.extend_from_slice(bytes);
        socket.write_all(&response).await.unwrap();
        socket.shutdown().await.unwrap();
    }

    async fn read_until_eof(socket: &mut (impl AsyncRead + Unpin), millis: u64) -> bool {
        let mut buffer = [0u8; 16];
        matches!(
            tokio::time::timeout(Duration::from_millis(millis), socket.read(&mut buffer)).await,
            Ok(Ok(0))
        )
    }

    #[tokio::test]
    async fn download_open_error_aborts_the_owned_stage() {
        use crate::core::{ImageDownload, IoFault, set_io_fault};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bytes = image_body();
        let expected = bytes.clone();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            serve_full_image(&mut first, &bytes).await;
            let closed = read_until_eof(&mut first, 1000).await;
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            serve_full_image(&mut second, &bytes).await;
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut fixture = prepare_download_fixture(root.path(), port);
        let mut filters =
            crate::filters::Filters::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        set_io_fault(Some(IoFault::Open));
        let error = fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .expect_err("injected open failure");
        set_io_fault(None);
        assert!(
            matches!(error, crate::core::AppError::Download(_)),
            "{error:?}"
        );
        assert_clean_stage(&fixture);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), closed_rx)
                .await
                .expect("peer closure observation timed out")
                .expect("closure notify"),
            "the stream must close when the stage cannot be opened"
        );
        refill_image_queue(&mut fixture);
        match fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .unwrap()
        {
            ImageDownload::Saved(path) => {
                assert_eq!(std::fs::read(path).unwrap(), expected);
            }
            other => panic!("expected saved image, got {other:?}"),
        }
        assert_eq!(
            fixture
                .app
                .task(&fixture.guid)
                .unwrap()
                .meta_i64("finished")
                .unwrap(),
            1
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn download_write_error_after_partial_bytes_aborts_the_owned_stage() {
        use crate::core::{ImageDownload, IoFault, set_io_fault};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bytes = image_body();
        let expected = bytes.clone();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            let mut head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                bytes.len()
            )
            .into_bytes();
            head.extend_from_slice(&bytes[..1024]);
            first.write_all(&head).await.unwrap();
            tokio::time::sleep(Duration::from_millis(300)).await;
            first.write_all(&bytes[1024..]).await.unwrap();
            first.flush().await.unwrap();
            let closed = read_until_eof(&mut first, 1000).await;
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            serve_full_image(&mut second, &bytes).await;
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut fixture = prepare_download_fixture(root.path(), port);
        let mut filters =
            crate::filters::Filters::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        set_io_fault(Some(IoFault::Write));
        let error = fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .expect_err("injected write failure");
        set_io_fault(None);
        assert!(
            matches!(error, crate::core::AppError::Download(_)),
            "{error:?}"
        );
        assert_clean_stage(&fixture);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), closed_rx)
                .await
                .expect("peer closure observation timed out")
                .expect("closure notify"),
            "the stream must close when a partial write fails"
        );
        refill_image_queue(&mut fixture);
        match fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .unwrap()
        {
            ImageDownload::Saved(path) => {
                assert_eq!(std::fs::read(path).unwrap(), expected);
            }
            other => panic!("expected saved image, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn download_flush_error_aborts_the_owned_stage() {
        use crate::core::{ImageDownload, IoFault, set_io_fault};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bytes = image_body();
        let expected = bytes.clone();
        let server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                read_headers(&mut socket).await;
                serve_full_image(&mut socket, &bytes).await;
            }
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut fixture = prepare_download_fixture(root.path(), port);
        let mut filters =
            crate::filters::Filters::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        set_io_fault(Some(IoFault::Flush));
        let error = fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .expect_err("injected flush failure");
        set_io_fault(None);
        assert!(
            matches!(error, crate::core::AppError::Download(_)),
            "{error:?}"
        );
        assert_clean_stage(&fixture);
        refill_image_queue(&mut fixture);
        match fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .unwrap()
        {
            ImageDownload::Saved(path) => {
                assert_eq!(std::fs::read(path).unwrap(), expected);
            }
            other => panic!("expected saved image, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn download_read_error_aborts_the_owned_stage() {
        use crate::core::ImageDownload;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bytes = image_body();
        let expected = bytes.clone();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            let mut head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                bytes.len()
            )
            .into_bytes();
            head.extend_from_slice(&bytes[..1024]);
            first.write_all(&head).await.unwrap();
            first.shutdown().await.unwrap();
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            serve_full_image(&mut second, &bytes).await;
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut fixture = prepare_download_fixture(root.path(), port);
        let mut filters =
            crate::filters::Filters::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        match fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .unwrap()
        {
            ImageDownload::Failed { fail_code, .. } => {
                assert_eq!(fail_code, crate::r#const::ERR_IMAGE_BROKEN);
            }
            other => panic!("expected broken image, got {other:?}"),
        }
        assert_clean_stage(&fixture);
        let task = fixture.app.task(&fixture.guid).unwrap();
        assert!(
            task.meta.get("finished").is_none() || task.meta_i64("finished").unwrap() == 0,
            "read failure must not mark a file finished"
        );
        {
            let task = fixture.app.task_mut(&fixture.guid).unwrap();
            task.put_img_queue(&fixture.image_url, &fixture.reload_url, "pic.jpg")
                .unwrap();
        }
        refill_image_queue(&mut fixture);
        match fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .unwrap()
        {
            ImageDownload::Saved(path) => {
                assert_eq!(std::fs::read(path).unwrap(), expected);
            }
            other => panic!("expected saved image, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_download_aborts_the_owned_stage_and_closes_the_stream() {
        use crate::core::ImageDownload;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bytes = image_body();
        let expected = bytes.clone();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            let mut head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                bytes.len()
            )
            .into_bytes();
            head.extend_from_slice(&bytes[..1024]);
            first.write_all(&head).await.unwrap();
            let closed = read_until_eof(&mut first, 1500).await;
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            serve_full_image(&mut second, &bytes).await;
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut fixture = prepare_download_fixture(root.path(), port);
        let mut filters =
            crate::filters::Filters::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        let mut download = Box::pin(fixture.app.download_one_image(&fixture.guid, &mut filters));
        let mut created = false;
        for _ in 0..10_000 {
            let written = std::fs::read_dir(&fixture.task_dir)
                .unwrap()
                .filter_map(|entry| entry.ok())
                .any(|entry| {
                    entry.file_name().to_string_lossy().starts_with("xehentai-")
                        && entry.metadata().map(|meta| meta.len() > 0).unwrap_or(false)
                });
            if written {
                created = true;
                break;
            }
            tokio::select! {
                biased;
                _ = &mut download => panic!("download completed before the staging barrier"),
                _ = tokio::task::yield_now() => {}
            }
        }
        assert!(created, "download never wrote the first staged bytes");
        drop(download);
        assert_clean_stage(&fixture);
        assert!(
            tokio::time::timeout(Duration::from_secs(2), closed_rx)
                .await
                .expect("peer closure observation timed out")
                .expect("closure notify"),
            "dropping the download must close the image stream"
        );
        refill_image_queue(&mut fixture);
        match fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .unwrap()
        {
            ImageDownload::Saved(path) => {
                assert_eq!(std::fs::read(path).unwrap(), expected);
            }
            other => panic!("expected saved image, got {other:?}"),
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn cancelled_queued_stage_open_cannot_recreate_the_stage() {
        use crate::core::ImageDownload;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let bytes = image_body();
        let expected = bytes.clone();
        let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<bool>();
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_headers(&mut first).await;
            serve_full_image(&mut first, &bytes).await;
            let closed = read_until_eof(&mut first, 1500).await;
            let _ = closed_tx.send(closed);
            let (mut second, _) = listener.accept().await.unwrap();
            read_headers(&mut second).await;
            serve_full_image(&mut second, &bytes).await;
        });
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("downloads")).unwrap();
        let mut fixture = prepare_download_fixture(root.path(), port);
        let mut filters =
            crate::filters::Filters::load(std::path::Path::new(env!("CARGO_MANIFEST_DIR")))
                .unwrap();
        let crate::core::SlowOpenHandle {
            started,
            finished,
            release,
        } = crate::core::arm_slow_open();
        crate::core::set_io_fault(Some(crate::core::IoFault::SlowOpen));
        let mut download = Box::pin(fixture.app.download_one_image(&fixture.guid, &mut filters));
        tokio::select! {
            biased;
            _ = &mut download => panic!("the slow stage open must hold the download"),
            result = tokio::task::spawn_blocking(move || {
                started.recv_timeout(Duration::from_secs(3))
            }) => {
                result.expect("started receiver").expect("stage open started");
            }
        }
        drop(download);
        crate::core::set_io_fault(None);
        release.send(()).expect("release the queued open");
        tokio::task::spawn_blocking(move || finished.recv_timeout(Duration::from_secs(3)))
            .await
            .expect("finished receiver")
            .expect("queued open completed");
        assert_eq!(
            stage_names(&fixture.task_dir),
            Vec::<String>::new(),
            "the late blocking open must not leave a recreated stage"
        );
        assert!(!fixture.task_dir.join("1.jpg").exists());
        let task = fixture.app.task(&fixture.guid).unwrap();
        assert!(task.meta.get("finished").is_none());
        assert_eq!(std::fs::read(&fixture.sentinel).unwrap(), b"old-state");
        assert!(
            tokio::time::timeout(Duration::from_secs(2), closed_rx)
                .await
                .expect("peer closure observation timed out")
                .expect("closure notify"),
            "cancelling a queued stage open must close the stream"
        );
        refill_image_queue(&mut fixture);
        match fixture
            .app
            .download_one_image(&fixture.guid, &mut filters)
            .await
            .unwrap()
        {
            ImageDownload::Saved(path) => {
                assert_eq!(std::fs::read(path).unwrap(), expected);
            }
            other => panic!("expected saved image, got {other:?}"),
        }
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

/// M4: persistent scan/download workers, Task pipeline, monitor and
/// cancellation. These tests use real local sockets, real RSS filters and the
/// production Runtime; only the clock/DNS fixture is injected.
mod m4 {
    use std::collections::VecDeque;
    use std::net::SocketAddr;
    use std::path::Path;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::task::LocalSet;

    use crate::config::Config;
    use crate::r#const as consts;
    use crate::core::{
        App, AppHandle, CommitOutcome, DownloadPort, Runtime, TraceEvent, WorkerRef,
    };
    use crate::proxy::FixedResolver;
    use crate::task::detach_gallery;
    use crate::task::{
        ArchiveFault, SaveStart, StagedSave, Task, TaskConfig, archive_writer_log,
        archive_writer_stats, arm_archive_cleanup_gate, arm_archive_fault, arm_archive_gate,
        arm_archive_waiting, arm_detach_candidate, commit_prepared, prepare_archive_snapshot,
        remove_detached, reset_archive_writer_stats,
    };
    use crate::tests::prepared_task;
    use crate::worker::{SessionConfig, SpeedChecker, StepClock, TestClock, TickClock};

    const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

    fn m4_logger() -> crate::util::logger::Logger {
        crate::util::logger::Logger::with_output(Box::new(std::io::stderr()), false)
    }

    fn m4_config(root: &Path, scan: i64, download: i64) -> Config {
        let mut config = Config::default();
        config.dir = root.join("downloads").to_string_lossy().into_owned();
        config.save_tasks = true;
        config.scan_thread_cnt = scan;
        config.download_thread_cnt = download;
        config.download_timeout = 5;
        config.low_speed_threshold = 0;
        config
    }

    fn m4_app(root: &Path, config: Config) -> App {
        m4_app_guids(root, config, &["guid0001"])
    }

    fn m4_app_guids(root: &Path, config: Config, guids: &[&str]) -> App {
        std::fs::create_dir_all(root.join("downloads")).unwrap();
        App::with_guid_source(
            config,
            m4_logger(),
            root,
            root,
            Box::new(crate::core::FixedGuids::new(
                guids.iter().map(|guid| guid.to_string()),
            )),
        )
        .unwrap()
    }

    fn m4_task(app: &mut App, root: &Path, url: &str, kwargs: serde_json::Value) -> String {
        let config = TaskConfig::from_app(&app.cfg, kwargs.as_object().unwrap()).unwrap();
        let task = Task::new(url, config, "guid0001".into(), root).unwrap();
        app.insert_task_for_test(task)
    }

    fn m4_session(
        app: &mut App,
        clock: Arc<dyn crate::worker::HttpClock>,
        resolver: Arc<dyn crate::proxy::Resolver>,
    ) {
        app.set_session_config(SessionConfig {
            retry: 1,
            timeout: Duration::from_secs(5),
            resolver,
            fallback: crate::worker::FallbackMap::empty(),
            clock,
            ..SessionConfig::default()
        });
    }

    async fn read_head<S: tokio::io::AsyncRead + Unpin>(socket: &mut S) -> String {
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let read = socket.read(&mut byte).await.unwrap();
            if read == 0 {
                break;
            }
            head.push(byte[0]);
            if head.ends_with(b"\r\n\r\n") {
                break;
            }
        }
        String::from_utf8_lossy(&head).into_owned()
    }

    async fn respond<S: tokio::io::AsyncWrite + Unpin>(
        socket: &mut S,
        status: &str,
        content_type: &str,
        body: &[u8],
    ) {
        let head = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(head.as_bytes()).await;
        let _ = socket.write_all(body).await;
        let _ = socket.shutdown().await;
    }

    fn path_of(head: &str) -> String {
        head.lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string()
    }

    fn metadata_body(total: i64) -> Vec<u8> {
        format!(
            "=\"gj\">日本語タイトル</h1>=\"gn\">English title</h1>\
             Length:</td><td class=\"gdt2\">{total} pages</td>\
             Showing 1 - 20 of {total} images"
        )
        .into_bytes()
    }

    fn page_body(hash: &str, count: i64) -> Vec<u8> {
        let mut body = String::new();
        for fid in 1..=count {
            body.push_str(&format!(
                "<a href=\"http://e-hentai.org/s/{hash}/123456-{fid}\"><div title=\"Page {fid}\""
            ));
        }
        body.into_bytes()
    }

    fn image_page_body(
        port: u16,
        hash40: &str,
        hash10: &str,
        fid: i64,
        name: &str,
        size: usize,
    ) -> Vec<u8> {
        format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=abc\" style=\"max-width:100%\">\
             </a></div><div>{name}.jpg :: 100x200 :: 2 KB</di>\
             return nl('{hash10}-{fid}')"
        )
        .into_bytes()
    }

    fn image_body(seed: u8, size: usize) -> Vec<u8> {
        (0..size)
            .map(|index| seed.wrapping_add(index as u8))
            .collect()
    }

    async fn wait_state(handle: &AppHandle, guid: &str, state: i64) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            if handle.task_state(guid) == Some(state) {
                return;
            }
            if std::time::Instant::now() > deadline {
                let trace = handle.trace();
                panic!(
                    "task {guid} did not reach {state}: {:?}\ntrace {:?}",
                    handle.task_state(guid),
                    trace
                );
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pipeline_scans_pages_downloads_real_files_and_finishes() {
        let image_hash_a = "0123456789abcdef0123456789abcdef01234567";
        let image_hash_b = "89abcdef0123456789abcdef0123456789abcdef";
        let image_hash10_a = "0123456789";
        let image_hash10_b = "89abcdef01";
        let image_a = image_body(1, 1000);
        let image_b = image_body(2, 1200);
        let image_a_server = image_a.clone();
        let image_b_server = image_b.clone();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let metadata = metadata_body(2);
        let page = page_body("0123456789", 2);
        let page2 = page_body("0123456789", 2);
        let img_a = image_page_body(port, image_hash_a, image_hash10_a, 1, "one", image_a.len());
        let img_b = image_page_body(port, image_hash_b, image_hash10_b, 2, "two", image_b.len());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                let metadata = metadata.clone();
                let page = page.clone();
                let page2 = page2.clone();
                let img_a = img_a.clone();
                let img_b = img_b.clone();
                let image_a = image_a_server.clone();
                let image_b = image_b_server.clone();
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    seen.lock().unwrap().push(path.clone());
                    if path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.starts_with("/g/123/abcdef0123/") {
                        let _ = page2;
                        respond(&mut socket, "200 OK", "text/html", &metadata).await;
                    } else if path.contains("/s/0123456789/123456-1") {
                        respond(&mut socket, "200 OK", "text/html", &img_a).await;
                    } else if path.contains("/s/0123456789/123456-2") {
                        respond(&mut socket, "200 OK", "text/html", &img_b).await;
                    } else if path.contains("123456-1") {
                        respond(&mut socket, "200 OK", "text/html", &img_a).await;
                    } else if path.contains("123456-2") {
                        respond(&mut socket, "200 OK", "text/html", &img_b).await;
                    } else if path.contains(image_hash_a) {
                        respond(&mut socket, "200 OK", "image/jpeg", &image_a).await;
                    } else if path.contains(image_hash_b) {
                        respond(&mut socket, "200 OK", "image/jpeg", &image_b).await;
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 2, 2);
        config.download_timeout = 5;
        let mut app = m4_app(root.path(), config);
        let guid = {
            let mut kwargs = serde_json::Map::new();
            kwargs.insert("scan_thread_cnt".into(), serde_json::json!(2));
            kwargs.insert("download_thread_cnt".into(), serde_json::json!(2));
            m4_task(
                &mut app,
                root.path(),
                "http://e-hentai.org/g/123/abcdef0123/",
                serde_json::Value::Object(kwargs),
            )
        };
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = wait_state(&handle, &guid, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let report = handle.shutdown().await;
                run.await.unwrap();
                assert!(report.joined >= 1, "{report:?}");
                let app = handle.borrow_app(|app| {
                    let task = app.task(&guid).unwrap();
                    assert_eq!(task.meta_i64("finished").unwrap(), 2);
                    let dir = task.get_fpath().unwrap();
                    assert_eq!(std::fs::read(dir.join("1.jpg")).unwrap(), image_a);
                    assert_eq!(std::fs::read(dir.join("2.jpg")).unwrap(), image_b);
                    assert!(task.renamed_map.len() >= 2);
                    true
                });
                assert!(app);
                let trace = handle.trace();
                assert!(
                    trace.iter().any(|event| event.kind == "worker.scan.ok"),
                    "{trace:?}"
                );
                assert!(
                    trace
                        .iter()
                        .any(|event| event.kind == "worker.download.saved"),
                    "{trace:?}"
                );
                assert!(
                    trace.iter().any(|event| event.kind == "monitor.finished"),
                    "{trace:?}"
                );
            })
            .await;
        server.abort();
        let requests = requests.lock().unwrap();
        assert!(
            requests.iter().any(|path| path.contains("/img/")),
            "{requests:?}"
        );
        assert!(
            requests
                .iter()
                .filter(|path| path.starts_with("/s/"))
                .count()
                == 2,
            "{requests:?}"
        );
    }

    async fn wait_trace_count(handle: &AppHandle, kind: &str, count: usize) -> Vec<TraceEvent> {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        loop {
            let trace = handle.trace();
            if trace.iter().filter(|event| event.kind == kind).count() >= count {
                return trace;
            }
            if std::time::Instant::now() > deadline {
                panic!("trace {kind} x{count} not reached: {trace:?}");
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn monitor_fixture(root: &Path, total: i64, ignored: Vec<i64>) -> (Runtime, String) {
        let mut config = m4_config(root, 0, 2);
        config.ignored_errors = ignored;
        let mut app = m4_app(root, config);
        let guid = m4_task(
            &mut app,
            root,
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::json!({}),
        );
        {
            let task = app.task_mut(&guid).unwrap();
            task.state = consts::TASK_STATE_DOWNLOAD;
            task.meta
                .insert("title".into(), serde_json::json!("monitor"));
            task.meta.insert("total".into(), serde_json::json!(total));
            task.meta.insert("finished".into(), serde_json::json!(0));
            task.meta
                .insert("thumbnail_cnt".into(), serde_json::json!(20));
            task.img_q = Some(std::collections::VecDeque::new());
            task.page_q = Some(std::collections::VecDeque::new());
        }
        app.set_session_config(SessionConfig {
            clock: Arc::new(TickClock::new(100.0)),
            ..SessionConfig::default()
        });
        (Runtime::new(app, Path::new(MANIFEST).to_path_buf()), guid)
    }

    fn speed_ref(bytes: usize) -> Arc<Mutex<SpeedChecker>> {
        let mut checker = SpeedChecker::new(5, Arc::new(StepClock::new(0.0, 1.1)));
        for _ in 0..5 {
            checker.check(bytes);
        }
        Arc::new(Mutex::new(checker))
    }

    #[tokio::test]
    async fn monitor_reports_two_slow_windows_before_a_zero_speed_stuck_break() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, guid) = monitor_fixture(root.path(), 10, Vec::new());
        let names: Vec<String> = vec!["down-1".into(), "down-2".into()];
        let local = LocalSet::new();
        local
            .run_until(async {
                let (monitor, join) = runtime.start_monitor_for_test(&guid, &names);
                let refs: Vec<WorkerRef> = names
                    .iter()
                    .map(|name| WorkerRef {
                        name: name.clone(),
                        alive: Arc::new(AtomicBool::new(true)),
                        speed: Some(speed_ref(20_000)),
                        zombie_threshold: 1.0e9,
                    })
                    .collect();
                for reference in &refs {
                    monitor.register(reference.clone());
                }
                let handle = runtime.handle();
                wait_trace_count(&handle, "monitor.stats", 1).await;
                let info = handle.get_info();
                assert_eq!(info["threads_running"], serde_json::json!(2));
                assert_eq!(info["threads_zombie"], serde_json::json!(0));
                let trace = wait_trace_count(&handle, "monitor.slow", 2).await;
                let slow: Vec<usize> = trace
                    .iter()
                    .enumerate()
                    .filter(|(_, event)| event.kind == "monitor.slow")
                    .map(|(index, _)| index)
                    .collect();
                assert!(slow.len() >= 2, "{trace:?}");
                assert!(
                    !trace.iter().any(|event| event.kind == "monitor.stuck"),
                    "no stuck before the zero-speed window: {trace:?}"
                );
                for reference in &refs {
                    reference.speed.as_ref().unwrap().lock().unwrap().reset();
                }
                let trace = wait_trace_count(&handle, "monitor.stuck", 1).await;
                let stuck = trace
                    .iter()
                    .position(|event| event.kind == "monitor.stuck")
                    .unwrap();
                assert!(slow[1] < stuck, "two slow windows first: {trace:?}");
                monitor.request_exit();
                let _ = join.await;
                assert_eq!(
                    handle.task_state(&guid),
                    Some(consts::TASK_STATE_DOWNLOAD),
                    "stuck leaves the task unfinished"
                );
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn configured_scan_and_download_workers_reach_the_server_barrier_limits() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let scan_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let down_barrier = Arc::new(tokio::sync::Barrier::new(2));
        let scan_active = Arc::new(AtomicUsize::new(0));
        let scan_peak = Arc::new(AtomicUsize::new(0));
        let down_active = Arc::new(AtomicUsize::new(0));
        let down_peak = Arc::new(AtomicUsize::new(0));
        let metadata = metadata_body(4);
        let mut pages = String::new();
        let mut image_pages = Vec::new();
        let mut images = Vec::new();
        for index in 0..4u32 {
            let hash10 = format!("{:010x}", 0xaaaa0000u32 + index);
            let hash40 = format!("{:040x}", index + 1);
            let size = 1000 + index as usize;
            pages.push_str(&format!(
                "<a href=\"http://e-hentai.org/s/{hash10}/123456-{index}\"><div title=\"Page {index}\""
            ));
            image_pages.push(
                format!(
                    "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=x\" style=\"max-width:100%\">\
                     </a></div><div>n{index}.jpg :: 100x200 :: 2 KB</di>\
                     return nl('{hash10}-{index}')"
                )
                .into_bytes(),
            );
            images.push(image_body(index as u8 + 1, size));
        }
        let pages = pages.into_bytes();
        let image_pages = Arc::new(image_pages);
        let images = Arc::new(images);
        let server = tokio::spawn({
            let scan_barrier = Arc::clone(&scan_barrier);
            let down_barrier = Arc::clone(&down_barrier);
            let scan_active = Arc::clone(&scan_active);
            let scan_peak = Arc::clone(&scan_peak);
            let down_active = Arc::clone(&down_active);
            let down_peak = Arc::clone(&down_peak);
            async move {
                loop {
                    let Ok((mut socket, _)) = listener.accept().await else {
                        return;
                    };
                    let metadata = metadata.clone();
                    let pages = pages.clone();
                    let image_pages = Arc::clone(&image_pages);
                    let images = Arc::clone(&images);
                    let scan_barrier = Arc::clone(&scan_barrier);
                    let down_barrier = Arc::clone(&down_barrier);
                    let scan_active = Arc::clone(&scan_active);
                    let scan_peak = Arc::clone(&scan_peak);
                    let down_active = Arc::clone(&down_active);
                    let down_peak = Arc::clone(&down_peak);
                    tokio::spawn(async move {
                        let head = read_head(&mut socket).await;
                        let path = path_of(&head);
                        if path.contains("?p=") {
                            respond(&mut socket, "200 OK", "text/html", &pages).await;
                        } else if path.starts_with("/g/123/abcdef0123/") {
                            respond(&mut socket, "200 OK", "text/html", &metadata).await;
                        } else if path.contains("/s/") {
                            let fid = path
                                .rsplit('-')
                                .next()
                                .and_then(|value| value.parse::<usize>().ok())
                                .unwrap_or(0);
                            let now = scan_active.fetch_add(1, Ordering::SeqCst) + 1;
                            scan_peak.fetch_max(now, Ordering::SeqCst);
                            scan_barrier.wait().await;
                            respond(&mut socket, "200 OK", "text/html", &image_pages[fid]).await;
                            scan_active.fetch_sub(1, Ordering::SeqCst);
                        } else if path.contains("/img/") {
                            let index = (0..4)
                                .find(|index| path.contains(&format!("{:040x}", index + 1)))
                                .unwrap_or(0);
                            let now = down_active.fetch_add(1, Ordering::SeqCst) + 1;
                            down_peak.fetch_max(now, Ordering::SeqCst);
                            down_barrier.wait().await;
                            respond(&mut socket, "200 OK", "image/jpeg", &images[index]).await;
                            down_active.fetch_sub(1, Ordering::SeqCst);
                        } else {
                            respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                        }
                    });
                }
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 2, 2);
        config.download_timeout = 5;
        let mut app = m4_app(root.path(), config);
        let mut kwargs = serde_json::Map::new();
        kwargs.insert("scan_thread_cnt".into(), serde_json::json!(2));
        kwargs.insert("download_thread_cnt".into(), serde_json::json!(2));
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::Value::Object(kwargs),
        );
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = wait_state(&handle, &guid, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                handle.shutdown().await;
                run.await.unwrap();
                let finished =
                    handle.borrow_app(|app| app.task(&guid).unwrap().meta_i64("finished").unwrap());
                assert_eq!(finished, 4);
            })
            .await;
        server.abort();
        assert_eq!(scan_peak.load(Ordering::SeqCst), 2);
        assert_eq!(down_peak.load(Ordering::SeqCst), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn one_persistent_download_worker_keeps_cookies_across_urls() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40_a = "0123456789abcdef0123456789abcdef01234567";
        let hash40_b = "89abcdef0123456789abcdef0123456789abcdef";
        let size_a = 900usize;
        let size_b = 700usize;
        let image_a = image_body(3, size_a);
        let image_b = image_body(4, size_b);
        let image_a_server = image_a.clone();
        let image_b_server = image_b.clone();
        let metadata = metadata_body(2);
        let page = page_body("0123456789", 2);
        let img_a = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40_a}-{size_a}-200-0-jpg/keystamp=a\" style=\"x\">\
             </a></div><div>a.jpg :: 100x200 :: 2 KB</di>return nl('0123456789-1')"
        )
        .into_bytes();
        let img_b = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40_b}-{size_b}-200-0-jpg/keystamp=b\" style=\"x\">\
             </a></div><div>b.jpg :: 100x200 :: 2 KB</di>return nl('89abcdef01-2')"
        )
        .into_bytes();
        let recorded = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&recorded);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                let metadata = metadata.clone();
                let page = page.clone();
                let img_a = img_a.clone();
                let img_b = img_b.clone();
                let image_a = image_a_server.clone();
                let image_b = image_b_server.clone();
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    if path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.starts_with("/g/123/abcdef0123/") {
                        respond(&mut socket, "200 OK", "text/html", &metadata).await;
                    } else if path.contains("123456-1") {
                        respond(&mut socket, "200 OK", "text/html", &img_a).await;
                    } else if path.contains("123456-2") {
                        respond(&mut socket, "200 OK", "text/html", &img_b).await;
                    } else if path.contains("/img/") {
                        let body = if path.contains(hash40_a) {
                            &image_a
                        } else {
                            &image_b
                        };
                        seen.lock().unwrap().push(head.to_ascii_lowercase());
                        if body.len() == size_a {
                            let response = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nSet-Cookie: img=1; Path=/\r\nContent-Length: {}\r\n\r\n",
                                body.len()
                            );
                            socket.write_all(response.as_bytes()).await.unwrap();
                            socket.write_all(body).await.unwrap();
                            let _ = socket.shutdown().await;
                        } else {
                            respond(&mut socket, "200 OK", "image/jpeg", body).await;
                        }
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.download_timeout = 5;
        let mut app = m4_app(root.path(), config);
        let mut kwargs = serde_json::Map::new();
        kwargs.insert("scan_thread_cnt".into(), serde_json::json!(1));
        kwargs.insert("download_thread_cnt".into(), serde_json::json!(1));
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::Value::Object(kwargs),
        );
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = wait_state(&handle, &guid, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                handle.shutdown().await;
                run.await.unwrap();
                let dir = handle.borrow_app(|app| app.task(&guid).unwrap().get_fpath().unwrap());
                assert_eq!(std::fs::read(dir.join("1.jpg")).unwrap(), image_a);
                assert_eq!(std::fs::read(dir.join("2.jpg")).unwrap(), image_b);
            })
            .await;
        server.abort();
        let recorded = recorded.lock().unwrap();
        assert_eq!(recorded.len(), 2, "{recorded:?}");
        assert!(recorded[0].starts_with("get /img/"), "{recorded:?}");
        assert!(
            recorded[1].contains("cookie: img=1"),
            "second URL on the same worker carries the Set-Cookie jar: {recorded:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn low_speed_stream_feeds_fake_response_through_the_same_filter_and_rescans() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40_a = "0123456789abcdef0123456789abcdef01234567";
        let hash40_b = "89abcdef0123456789abcdef0123456789abcdef";
        let metadata = metadata_body(1);
        let page = page_body("aaaa000001", 1);
        let img_a = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40_a}-5000-200-0-jpg/keystamp=a\" style=\"x\">\
             </a></div><div>a.jpg :: 100x200 :: 2 KB</di>return nl('aaaa000001-1')"
        )
        .into_bytes();
        let img_b = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40_b}-25000-200-0-jpg/keystamp=b\" style=\"x\">\
             </a></div><div>b.jpg :: 100x200 :: 2 KB</di>return nl('aaaa000001-1')"
        )
        .into_bytes();
        let image_a = image_body(5, 5000);
        let image_b = image_body(6, 25_000);
        let image_b_server = image_b.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let page_served = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                let metadata = metadata.clone();
                let page = page.clone();
                let img_a = img_a.clone();
                let img_b = img_b.clone();
                let image_a = image_a.clone();
                let image_b = image_b_server.clone();
                let page_served = Arc::clone(&page_served);
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    seen.lock().unwrap().push(path.clone());
                    if path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.starts_with("/g/123/abcdef0123/") {
                        respond(&mut socket, "200 OK", "text/html", &metadata).await;
                    } else if path.contains("/s/") {
                        let attempt = page_served.fetch_add(1, Ordering::SeqCst);
                        if attempt > 0 {
                            tokio::time::sleep(Duration::from_millis(1500)).await;
                            respond(&mut socket, "200 OK", "text/html", &img_b).await;
                        } else {
                            respond(&mut socket, "200 OK", "text/html", &img_a).await;
                        }
                    } else if path.contains(hash40_a) {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                            image_a.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        for chunk in image_a.chunks(1000) {
                            if socket.write_all(chunk).await.is_err() {
                                return;
                            }
                            tokio::time::sleep(Duration::from_millis(1100)).await;
                        }
                        let _ = socket.shutdown().await;
                    } else if path.contains(hash40_b) {
                        respond(&mut socket, "200 OK", "image/jpeg", &image_b).await;
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.download_timeout = 8;
        config.low_speed_threshold = 1;
        let mut app = m4_app(root.path(), config);
        let mut kwargs = serde_json::Map::new();
        kwargs.insert("scan_thread_cnt".into(), serde_json::json!(1));
        kwargs.insert("download_thread_cnt".into(), serde_json::json!(1));
        kwargs.insert("low_speed_threshold".into(), serde_json::json!(1));
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::Value::Object(kwargs),
        );
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = wait_state(&handle, &guid, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let dir = handle.borrow_app(|app| app.task(&guid).unwrap().get_fpath().unwrap());
                assert_eq!(std::fs::read(dir.join("1.jpg")).unwrap(), image_b);
                let trace = handle.trace();
                assert!(
                    trace
                        .iter()
                        .any(|event| event.kind == "worker.download.fail"),
                    "{trace:?}"
                );
                assert!(
                    trace
                        .iter()
                        .filter(|event| event.kind == "worker.scan.ok")
                        .count()
                        >= 2,
                    "{trace:?}"
                );
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
        server.abort();
        let requests = requests.lock().unwrap();
        assert_eq!(
            requests.iter().filter(|path| path.contains("/s/")).count(),
            2,
            "{requests:?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|path| path.contains(hash40_a))
                .count(),
            1,
            "{requests:?}"
        );
        assert_eq!(
            requests
                .iter()
                .filter(|path| path.contains(hash40_b))
                .count(),
            1,
            "{requests:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_mid_body_aborts_the_owned_stage_and_resume_completes() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40 = "0123456789abcdef0123456789abcdef01234567";
        let size = 4000usize;
        let image = image_body(9, size);
        let image_server = image.clone();
        let metadata = metadata_body(1);
        let page = page_body("aaaa000001", 1);
        let img_page = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=x\" style=\"x\">\
             </a></div><div>one.jpg :: 100x200 :: 2 KB</di>return nl('aaaa000001-1')"
        )
        .into_bytes();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let release_rx = Arc::new(tokio::sync::Mutex::new(release_rx));
        let attempts = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let metadata = metadata.clone();
                let page = page.clone();
                let img_page = img_page.clone();
                let image = image_server.clone();
                let started_tx = Arc::clone(&started_tx);
                let release_rx = Arc::clone(&release_rx);
                let attempts = Arc::clone(&attempts);
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    if path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.starts_with("/g/123/abcdef0123/") {
                        respond(&mut socket, "200 OK", "text/html", &metadata).await;
                    } else if path.contains("/s/") {
                        respond(&mut socket, "200 OK", "text/html", &img_page).await;
                    } else if path.contains("/img/") {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                            image.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        if attempt == 0 {
                            let _ = socket.write_all(&image[..500]).await;
                            if let Some(tx) = started_tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            let mut release = release_rx.lock().await;
                            let _ = (&mut *release).await;
                            let _ = socket.write_all(&image[500..]).await;
                            let _ = socket.shutdown().await;
                        } else {
                            let _ = socket.write_all(&image).await;
                            let _ = socket.shutdown().await;
                        }
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.download_timeout = 8;
        let mut app = m4_app(root.path(), config);
        let mut kwargs = serde_json::Map::new();
        kwargs.insert("scan_thread_cnt".into(), serde_json::json!(1));
        kwargs.insert("download_thread_cnt".into(), serde_json::json!(1));
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::Value::Object(kwargs),
        );
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = started_rx => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let report = handle.pause_task_and_wait(&guid).await.unwrap();
                assert_eq!(report.blocking_pending, 0, "{report:?}");
                assert_eq!(handle.task_state(&guid), Some(consts::TASK_STATE_PAUSED));
                let (dir, pending) = handle.borrow_app(|app| {
                    let task = app.task(&guid).unwrap();
                    (
                        task.get_fpath().unwrap(),
                        task.img_q.as_ref().map(VecDeque::len).unwrap_or(0),
                    )
                });
                assert_eq!(pending, 1, "the interrupted image is requeued for resume");
                assert!(!dir.join("1.jpg").exists());
                let mut leftovers = Vec::new();
                if let Ok(entries) = std::fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        if entry.file_name().to_string_lossy().starts_with("xehentai-") {
                            leftovers.push(entry.path());
                        }
                    }
                }
                assert!(leftovers.is_empty(), "{leftovers:?}");
                let _ = release_tx.send(());
                handle.resume_task(&guid).unwrap();
                tokio::select! {
                    _ = wait_state(&handle, &guid, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let path = handle.borrow_app(|app| app.task(&guid).unwrap().get_fpath().unwrap());
                assert_eq!(std::fs::read(path.join("1.jpg")).unwrap(), image);
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_mid_page_drops_the_late_scan_result_and_resume_scans_again() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40 = "0123456789abcdef0123456789abcdef01234567";
        let size = 1200usize;
        let image = image_body(11, size);
        let image_server = image.clone();
        let metadata = metadata_body(1);
        let page = page_body("aaaa000001", 1);
        let img_page = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=x\" style=\"x\">\
             </a></div><div>one.jpg :: 100x200 :: 2 KB</di>return nl('aaaa000001-1')"
        )
        .into_bytes();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let release_rx = Arc::new(tokio::sync::Mutex::new(release_rx));
        let attempts = Arc::new(AtomicUsize::new(0));
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let metadata = metadata.clone();
                let page = page.clone();
                let img_page = img_page.clone();
                let image = image_server.clone();
                let started_tx = Arc::clone(&started_tx);
                let release_rx = Arc::clone(&release_rx);
                let attempts = Arc::clone(&attempts);
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    if path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.starts_with("/g/123/abcdef0123/") {
                        respond(&mut socket, "200 OK", "text/html", &metadata).await;
                    } else if path.contains("/s/") {
                        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
                        if attempt == 0 {
                            if let Some(tx) = started_tx.lock().unwrap().take() {
                                let _ = tx.send(());
                            }
                            let mut release = release_rx.lock().await;
                            let _ = (&mut *release).await;
                            respond(&mut socket, "200 OK", "text/html", &img_page).await;
                        } else {
                            respond(&mut socket, "200 OK", "text/html", &img_page).await;
                        }
                    } else if path.contains("/img/") {
                        respond(&mut socket, "200 OK", "image/jpeg", &image).await;
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.download_timeout = 8;
        let mut app = m4_app(root.path(), config);
        let mut kwargs = serde_json::Map::new();
        kwargs.insert("scan_thread_cnt".into(), serde_json::json!(1));
        kwargs.insert("download_thread_cnt".into(), serde_json::json!(1));
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::Value::Object(kwargs),
        );
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = started_rx => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                handle.pause_task_and_wait(&guid).await.unwrap();
                let (img_pending, page_pending) = handle.borrow_app(|app| {
                    let task = app.task(&guid).unwrap();
                    (
                        task.img_q.as_ref().map(VecDeque::len).unwrap_or(0),
                        task.page_q.as_ref().map(VecDeque::len).unwrap_or(0),
                    )
                });
                assert_eq!(img_pending, 0, "the dropped response is not published");
                assert!(page_pending >= 1, "the in-flight page is requeued");
                let _ = release_tx.send(());
                handle.resume_task(&guid).unwrap();
                tokio::select! {
                    _ = wait_state(&handle, &guid, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let path = handle.borrow_app(|app| app.task(&guid).unwrap().get_fpath().unwrap());
                assert_eq!(std::fs::read(path.join("1.jpg")).unwrap(), image);
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn partial_session_resume_downloads_only_missing_images_and_restart_is_quiet() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40_b = "89abcdef0123456789abcdef0123456789abcdef";
        let hash40_c = "abcdef0123456789abcdef0123456789abcdef01";
        let hash10_b = "89abcdef01";
        let hash10_c = "abcdef0123";
        let image_b = image_body(12, 1100);
        let image_c = image_body(13, 1300);
        let image_b_server = image_b.clone();
        let image_c_server = image_c.clone();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                let image_b = image_b_server.clone();
                let image_c = image_c_server.clone();
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    seen.lock().unwrap().push(path.clone());
                    if path.contains(hash40_b) {
                        respond(&mut socket, "200 OK", "image/jpeg", &image_b).await;
                    } else if path.contains(hash40_c) {
                        respond(&mut socket, "200 OK", "image/jpeg", &image_c).await;
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let config = m4_config(root.path(), 1, 1);
        let mut app = m4_app(root.path(), config.clone());
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1}),
        );
        let gallery = root.path().join("downloads").join("resume");
        std::fs::create_dir_all(&gallery).unwrap();
        std::fs::write(gallery.join("1.jpg"), b"already-downloaded").unwrap();
        let image_b_url =
            format!("http://127.0.0.1:{port}/img/{hash40_b}-1100-200-0-jpg/keystamp=b");
        let image_c_url =
            format!("http://127.0.0.1:{port}/img/{hash40_c}-1300-200-0-jpg/keystamp=c");
        {
            let task = app.task_mut(&guid).unwrap();
            task.state = consts::TASK_STATE_DOWNLOAD;
            task.meta
                .insert("title".into(), serde_json::json!("resume"));
            task.meta.insert("total".into(), serde_json::json!(3));
            task.meta.insert("finished".into(), serde_json::json!(1));
            task.meta
                .insert("thumbnail_cnt".into(), serde_json::json!(20));
            task.img_q = Some(VecDeque::from([image_b_url.clone(), image_c_url.clone()]));
            task.reload_map.insert(
                hash10_b.into(),
                (
                    format!("http://e-hentai.org/s/{hash10_b}/123456-2"),
                    "two.jpg".into(),
                ),
            );
            task.reload_map.insert(
                hash10_c.into(),
                (
                    format!("http://e-hentai.org/s/{hash10_c}/123456-3"),
                    "three.jpg".into(),
                ),
            );
        }
        assert_eq!(app.save_session().0, consts::ERR_NO_ERROR);
        drop(app);

        let mut resumed = m4_app(root.path(), config.clone());
        assert_eq!(resumed.queue_len(), 1);
        m4_session(
            &mut resumed,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let guid_first = guid.clone();
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(resumed, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = wait_state(&handle, &guid_first, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let finished = handle
                    .borrow_app(|app| app.task(&guid_first).unwrap().meta_i64("finished").unwrap());
                assert_eq!(finished, 3);
                assert_eq!(std::fs::read(gallery.join("2.jpg")).unwrap(), image_b);
                assert_eq!(std::fs::read(gallery.join("3.jpg")).unwrap(), image_c);
                handle.save_session();
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
        let requests_after_resume = requests.lock().unwrap().len();
        assert_eq!(requests_after_resume, 2, "{requests:?}");
        assert!(
            !requests
                .lock()
                .unwrap()
                .iter()
                .any(|path| path.contains("already")),
        );

        // Restart from the saved finished session: the task is skipped and no
        // image byte is requested again.
        let mut restarted = m4_app(root.path(), config);
        m4_session(
            &mut restarted,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(restarted, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                tokio::time::sleep(Duration::from_millis(1200)).await;
                assert_eq!(
                    handle.borrow_app(|app| app.task(&guid).unwrap().state),
                    consts::TASK_STATE_FINISHED
                );
                handle.shutdown().await;
                let _ = run.await;
            })
            .await;
        assert_eq!(requests.lock().unwrap().len(), requests_after_resume);
        server.abort();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archive_runs_after_downloads_rename_and_then_removes_the_directory() {
        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app(root.path(), config);
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true}),
        );
        let gallery = root.path().join("downloads").join("archived");
        std::fs::create_dir_all(&gallery).unwrap();
        std::fs::write(gallery.join("1.jpg"), b"image-bytes").unwrap();
        {
            let task = app.task_mut(&guid).unwrap();
            task.state = consts::TASK_STATE_DOWNLOAD;
            task.meta
                .insert("title".into(), serde_json::json!("archived"));
            task.meta.insert("total".into(), serde_json::json!(1));
            task.meta.insert("finished".into(), serde_json::json!(1));
            task.meta
                .insert("thumbnail_cnt".into(), serde_json::json!(20));
            task.img_q = Some(VecDeque::new());
            task.page_q = Some(VecDeque::new());
        }
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                "127.0.0.1:9".parse::<SocketAddr>().unwrap(),
            )),
        );
        let zip_path = root.path().join("downloads").join("archived.zip");
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                loop {
                    if zip_path.exists() {
                        break;
                    }
                    if std::time::Instant::now() > deadline {
                        panic!("archive not created: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                assert!(!gallery.exists(), "directory removed after archive");
                assert_eq!(
                    handle.borrow_app(|app| app.task(&guid).unwrap().state),
                    consts::TASK_STATE_FINISHED
                );
                let trace = handle.trace();
                assert!(
                    trace.iter().any(|event| event.kind == "archive.done"),
                    "{trace:?}"
                );
                let file = std::fs::File::open(&zip_path).unwrap();
                let mut zip = zip::ZipArchive::new(file).unwrap();
                assert!(zip.by_name("1.jpg").is_ok());
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
    }

    /// M5 FD coverage (real M4 producer): a private persistence file hardlinked
    /// as an ordinary gallery entry must make the REAL archive worker deny with
    /// no public ZIP and no archive stage left behind; its worker record
    /// retires and the same App then archives a legitimate task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archive_worker_denies_a_private_hardlink_and_then_the_same_app_reuses() {
        fn archive_kwargs() -> serde_json::Map<String, serde_json::Value> {
            let mut kwargs = serde_json::Map::new();
            kwargs.insert("make_archive".to_string(), serde_json::json!(true));
            kwargs
        }
        let root = tempfile::tempdir().unwrap();
        let downloads = root.path().join("downloads");
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app_guids(root.path(), config, &["guid0001", "guid0002"]);
        app.cfg.dir = downloads.to_string_lossy().into_owned();
        std::fs::create_dir_all(&downloads).unwrap();

        // The task is added through the REAL App entry, so it shares the live
        // private-path cell. The private file is hardlinked as a gallery entry.
        let (code, denied_guid) = app
            .add_task("http://e-hentai.org/g/123/abcdef0123/", archive_kwargs())
            .expect("the real add_task entry");
        assert_eq!(code, consts::ERR_NO_ERROR);
        let denied_guid = denied_guid.expect("a real task guid");
        let private = root.path().join(crate::config::CONFIG_FILE);
        std::fs::write(&private, b"PRIVATE-CONFIG-BYTES").unwrap();
        let denied_gallery = {
            let task = app.task_mut(&denied_guid).unwrap();
            task.state = consts::TASK_STATE_DOWNLOAD;
            task.meta
                .insert("title".into(), serde_json::json!("private-entry"));
            task.meta.insert("total".into(), serde_json::json!(1));
            task.meta.insert("finished".into(), serde_json::json!(1));
            task.img_q = Some(VecDeque::new());
            task.page_q = Some(VecDeque::new());
            let gallery = task.get_fpath().unwrap();
            std::fs::create_dir_all(&gallery).unwrap();
            gallery
        };
        std::fs::hard_link(&private, denied_gallery.join("1.jpg")).unwrap();
        let denied_zip = denied_gallery.with_extension("zip");

        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                loop {
                    let failed = handle.borrow_app(|app| {
                        app.task(&denied_guid)
                            .is_some_and(|task| task.state == consts::TASK_STATE_FAILED)
                    });
                    if failed {
                        break;
                    }
                    if std::time::Instant::now() > deadline {
                        panic!("the denied archive never failed: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert!(
                    !denied_zip.exists(),
                    "a private hardlink must never be published as a public archive"
                );
                let stages: Vec<String> = std::fs::read_dir(&downloads)
                    .unwrap()
                    .filter_map(|entry| entry.ok())
                    .map(|entry| entry.file_name().to_string_lossy().into_owned())
                    .filter(|name| name.starts_with(".xehentai-archive-"))
                    .collect();
                assert!(
                    stages.is_empty(),
                    "no stage may survive a denial: {stages:?}"
                );
                assert!(
                    handle.trace().iter().any(|event| {
                        event.kind == "archive.failed"
                            && event.detail.to_string().contains("reserved persistence")
                    }),
                    "the denial must come from the private-object gate: {:?}",
                    handle.trace()
                );

                // Same App, later legitimate archive through the same worker.
                let (code, legit_guid) = handle.borrow_app_mut(|app| {
                    let (code, guid) = app
                        .add_task("http://e-hentai.org/g/456/abcdef0123/", archive_kwargs())
                        .expect("add a legitimate task");
                    let guid = guid.expect("a real task guid");
                    let task = app.task_mut(&guid).unwrap();
                    task.state = consts::TASK_STATE_DOWNLOAD;
                    task.meta
                        .insert("title".into(), serde_json::json!("legit-entry"));
                    task.meta.insert("total".into(), serde_json::json!(1));
                    task.meta.insert("finished".into(), serde_json::json!(1));
                    task.img_q = Some(VecDeque::new());
                    task.page_q = Some(VecDeque::new());
                    let gallery = task.get_fpath().unwrap();
                    std::fs::create_dir_all(&gallery).unwrap();
                    std::fs::write(gallery.join("1.jpg"), b"LEGIT-IMAGE-BYTES").unwrap();
                    (code, guid)
                });
                assert_eq!(code, consts::ERR_NO_ERROR);
                let legit_zip = downloads.join("legit-entry.zip");
                let deadline = std::time::Instant::now() + Duration::from_secs(15);
                loop {
                    if legit_zip.exists() {
                        break;
                    }
                    if std::time::Instant::now() > deadline {
                        panic!(
                            "the legitimate archive never appeared: {:?}",
                            handle.trace()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                assert_eq!(
                    handle.borrow_app(|app| app.task(&legit_guid).unwrap().state),
                    consts::TASK_STATE_FINISHED
                );
                let file = std::fs::File::open(&legit_zip).unwrap();
                let mut zip = zip::ZipArchive::new(file).unwrap();
                let mut entry = zip.by_name("1.jpg").unwrap();
                let mut bytes = Vec::new();
                std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
                assert_eq!(bytes, b"LEGIT-IMAGE-BYTES");
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
    }

    #[tokio::test]
    async fn same_run_and_same_guid_reuse_reject_old_scan_and_file_publication() {
        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.delete_task_files = false;
        let mut app = m4_app(root.path(), config);
        let image_hash = "0123456789abcdef0123456789abcdef01234567";
        let image_url = format!("http://127.0.0.1:9/img/{image_hash}-10-20-0-jpg/keystamp=x");
        let page_url = "http://e-hentai.org/s/0123456789/123456-1";
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::json!({}),
        );
        {
            let task = app.task_mut(&guid).unwrap();
            task.state = consts::TASK_STATE_DOWNLOAD;
            task.meta.insert("title".into(), serde_json::json!("stale"));
            task.meta.insert("total".into(), serde_json::json!(1));
            task.meta.insert("finished".into(), serde_json::json!(0));
            task.reload_map.insert(
                "0123456789".into(),
                (page_url.to_string(), "one.jpg".into()),
            );
            task.page_q = Some(VecDeque::new());
            task.img_q = Some(VecDeque::new());
        }
        let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
        let handle = runtime.handle();
        let old_token = handle.borrow_app(|app| app.task_token(&guid).unwrap());
        let staged: StagedSave = handle.borrow_app_mut(|app| {
            let task = app.task_mut(&guid).unwrap();
            match task.begin_staged_save(&image_url, &image_url).unwrap() {
                SaveStart::Staged(staged) => staged,
                SaveStart::AlreadyDone(_) => panic!("fresh task must stage"),
            }
        });
        let stage_path = staged.stage.clone();
        handle.pause_task(&guid).unwrap();
        let mut old_port = handle.shared_port(&guid, old_token);
        assert!(matches!(
            old_port.commit(staged).unwrap(),
            CommitOutcome::Rejected
        ));
        assert!(!stage_path.exists(), "stale commit removes its stage");
        handle.borrow_app_mut(|app| {
            app.put_scanned_image(&guid, old_token, &image_url, page_url, "one.jpg")
                .unwrap();
        });
        assert_eq!(
            handle.borrow_app(|app| app.task(&guid).unwrap().img_q.as_ref().unwrap().len()),
            0
        );

        // Delete and recreate the same guid: the new Task has a new owner, so
        // the old port cannot publish even into a fresh run marker.
        handle
            .borrow_app_mut(|app| app.task_mut(&guid).unwrap().state = consts::TASK_STATE_FINISHED);
        assert_eq!(
            handle.delete_task_and_wait(&guid).await.unwrap().0,
            consts::ERR_NO_ERROR
        );
        let new_guid = handle.borrow_app_mut(|app| {
            let config = TaskConfig::from_app(&app.cfg, &serde_json::Map::new()).unwrap();
            let task = Task::new(
                "http://e-hentai.org/g/123/abcdef0123/",
                config,
                "guid0001".into(),
                root.path(),
            )
            .unwrap();
            let guid = app.insert_task_for_test(task);
            app.ensure_task_queues(&guid).unwrap();
            let task = app.task_mut(&guid).unwrap();
            task.state = consts::TASK_STATE_DOWNLOAD;
            task.meta.insert("title".into(), serde_json::json!("stale"));
            task.meta.insert("total".into(), serde_json::json!(1));
            task.reload_map.insert(
                "0123456789".into(),
                (page_url.to_string(), "one.jpg".into()),
            );
            guid
        });
        assert_eq!(new_guid, guid);
        let new_token = handle.borrow_app(|app| app.task_token(&guid).unwrap());
        assert_ne!(old_token.0, new_token.0, "same guid, new owner");
        let staged: StagedSave = handle.borrow_app_mut(|app| {
            let task = app.task_mut(&guid).unwrap();
            match task.begin_staged_save(&image_url, &image_url).unwrap() {
                SaveStart::Staged(staged) => staged,
                SaveStart::AlreadyDone(_) => panic!("new task must stage"),
            }
        });
        let stage_path = staged.stage.clone();
        assert!(matches!(
            old_port.commit(staged).unwrap(),
            CommitOutcome::Rejected
        ));
        assert!(!stage_path.exists());
        handle.borrow_app_mut(|app| {
            app.put_scanned_image(&guid, old_token, &image_url, page_url, "one.jpg")
                .unwrap();
        });
        assert_eq!(
            handle.borrow_app(|app| app.task(&guid).unwrap().img_q.as_ref().unwrap().len()),
            0
        );
        handle.borrow_app_mut(|app| {
            app.put_scanned_image(&guid, new_token, &image_url, page_url, "one.jpg")
                .unwrap();
        });
        assert_eq!(
            handle.borrow_app(|app| app.task(&guid).unwrap().img_q.as_ref().unwrap().len()),
            1
        );
        handle.shutdown().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn task_loop_records_last_task_guid_and_prefetches_the_next_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40 = "0123456789abcdef0123456789abcdef01234567";
        let size = 1800usize;
        let image = image_body(51, size);
        let metadata_one =
            b"=\"gj\">one-jp</h1>=\"gn\">one</h1>Length:</td><td class=\"gdt2\">1 pages</td>\
              Showing 1 - 20 of 1 images"
                .to_vec();
        let metadata_two =
            b"=\"gj\">two-jp</h1>=\"gn\">two</h1>Length:</td><td class=\"gdt2\">0 pages</td>\
              Showing 1 - 20 of 0 images"
                .to_vec();
        let page = page_body("aaaaaaaaaa", 1);
        let img_page = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=x\" style=\"x\">\
             </a></div><div>one.jpg :: 100x200 :: 2 KB</di>return nl('aaaaaaaaaa-1')"
        )
        .into_bytes();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let started_tx = Arc::new(Mutex::new(Some(started_tx)));
        let release_rx = Arc::new(tokio::sync::Mutex::new(release_rx));
        let second_metadata = Arc::new(AtomicBool::new(false));
        let seen_second = Arc::clone(&second_metadata);
        let seen_second_server = Arc::clone(&second_metadata);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let metadata_one = metadata_one.clone();
                let metadata_two = metadata_two.clone();
                let page = page.clone();
                let img_page = img_page.clone();
                let image = image.clone();
                let started_tx = Arc::clone(&started_tx);
                let release_rx = Arc::clone(&release_rx);
                let seen_second = Arc::clone(&seen_second_server);
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    if path.contains("/g/111/aaaaaaaaaa/") && path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.contains("/g/111/aaaaaaaaaa/") {
                        respond(&mut socket, "200 OK", "text/html", &metadata_one).await;
                    } else if path.contains("/g/222/bbbbbbbbbb/") {
                        seen_second.store(true, Ordering::SeqCst);
                        respond(&mut socket, "200 OK", "text/html", &metadata_two).await;
                    } else if path.contains("/s/aaaaaaaaaa/") {
                        respond(&mut socket, "200 OK", "text/html", &img_page).await;
                    } else if path.contains("/img/") {
                        let head = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                            image.len()
                        );
                        let _ = socket.write_all(head.as_bytes()).await;
                        if let Some(tx) = started_tx.lock().unwrap().take() {
                            let _ = tx.send(());
                        }
                        let mut release = release_rx.lock().await;
                        let _ = (&mut *release).await;
                        let _ = socket.write_all(&image).await;
                        let _ = socket.shutdown().await;
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let config = m4_config(root.path(), 1, 1);
        std::fs::create_dir_all(root.path().join("downloads").join("two-jp")).unwrap();
        let mut app = m4_app_guids(root.path(), config, &["guid0001", "guid0002"]);
        let guid_one = app
            .add_task(
                "http://e-hentai.org/g/111/aaaaaaaaaa/",
                serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap()
            .1
            .unwrap();
        let guid_two = app
            .add_task(
                "http://e-hentai.org/g/222/bbbbbbbbbb/",
                serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1})
                    .as_object()
                    .unwrap()
                    .clone(),
            )
            .unwrap()
            .1
            .unwrap();
        assert_ne!(guid_one, guid_two);
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = started_rx => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while !seen_second.load(Ordering::SeqCst) {
                    if std::time::Instant::now() > deadline {
                        panic!("second metadata was not prefetched: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(
                    handle.last_task_guid().as_deref(),
                    Some(guid_one.as_str()),
                    "the popped second task records the first as last_task_guid"
                );
                let _ = release_tx.send(());
                wait_state(&handle, &guid_one, consts::TASK_STATE_FINISHED).await;
                wait_state(&handle, &guid_two, consts::TASK_STATE_FINISHED).await;
                assert!(root.path().join("h.json").exists());
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
        server.abort();
    }

    struct StallResolver;

    impl crate::proxy::Resolver for StallResolver {
        fn resolve(&self, host: &str, _port: u16) -> Result<Vec<SocketAddr>, String> {
            panic!("synchronous DNS must not run for the system resolver path: {host}");
        }

        fn lookup<'a>(
            &'a self,
            _host: &'a str,
            _port: u16,
        ) -> Option<crate::proxy::LookupFuture<'a>> {
            Some(Box::pin(std::future::pending()))
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pending_dns_lookup_does_not_freeze_other_local_work_or_start_late_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let _ = read_head(&mut socket).await;
            respond(&mut socket, "200 OK", "text/html", b"local").await;
        });
        let local = LocalSet::new();
        local
            .run_until(async {
                let mut stalled = crate::worker::Session::new(
                    vec![],
                    None,
                    crate::util::ProxyPolicy::All,
                    SessionConfig {
                        retry: 1,
                        timeout: Duration::from_secs(5),
                        resolver: Arc::new(StallResolver),
                        fallback: crate::worker::FallbackMap::empty(),
                        clock: Arc::new(TestClock::new(0.0)),
                        ..SessionConfig::default()
                    },
                );
                let stalled_handle = tokio::task::spawn_local(async move {
                    let _ = stalled
                        .request("GET", "http://stall.test/page", None, false)
                        .await;
                });
                let mut local_session = crate::worker::Session::new(
                    vec![],
                    None,
                    crate::util::ProxyPolicy::All,
                    SessionConfig {
                        retry: 1,
                        timeout: Duration::from_secs(3),
                        resolver: Arc::new(FixedResolver::new(
                            format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
                        )),
                        fallback: crate::worker::FallbackMap::empty(),
                        clock: Arc::new(TestClock::new(0.0)),
                        ..SessionConfig::default()
                    },
                );
                let ticks = Arc::new(AtomicUsize::new(0));
                let tick_counter = Arc::clone(&ticks);
                let ticker = tokio::task::spawn_local(async move {
                    for _ in 0..20 {
                        tokio::time::sleep(Duration::from_millis(5)).await;
                        tick_counter.fetch_add(1, Ordering::SeqCst);
                    }
                });
                tokio::time::sleep(Duration::from_millis(30)).await;
                assert!(
                    ticks.load(Ordering::SeqCst) >= 1,
                    "other local work kept running while DNS is pending"
                );
                let outcome = tokio::time::timeout(
                    Duration::from_secs(2),
                    local_session.request(
                        "GET",
                        &format!("http://127.0.0.1:{port}/ok"),
                        None,
                        false,
                    ),
                )
                .await
                .expect("unrelated request must not wait on DNS")
                .expect("local request");
                assert_eq!(outcome.response.text, "local");
                assert!(
                    ticks.load(Ordering::SeqCst) >= 1,
                    "other local work kept running"
                );
                assert!(
                    !stalled_handle.is_finished(),
                    "stalled lookup still pending"
                );
                stalled_handle.abort();
                let _ = stalled_handle.await;
                ticker.abort();
            })
            .await;
        server.await.unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn same_task_duplicate_images_download_once_and_copy_for_the_second_fid() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40 = "0123456789abcdef0123456789abcdef01234567";
        let size = 1500usize;
        let image = image_body(21, size);
        let image_server = image.clone();
        let metadata = metadata_body(2);
        let page = b"<a href=\"http://e-hentai.org/s/0123456789/123456-1\"><div title=\"Page 1\"\
                     <a href=\"http://e-hentai.org/s/0123456789/123456-2\"><div title=\"Page 2\""
            .to_vec();
        let image_pages = Arc::new(vec![
            format!(
                "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=x\" style=\"x\">\
                 </a></div><div>same.jpg :: 100x200 :: 2 KB</di>return nl('123456-1')"
            )
            .into_bytes(),
            format!(
                "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=x\" style=\"x\">\
                 </a></div><div>same.jpg :: 100x200 :: 2 KB</di>return nl('123456-2')"
            )
            .into_bytes(),
        ]);
        let image_requests = Arc::new(AtomicUsize::new(0));
        let seen_images = Arc::clone(&image_requests);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let metadata = metadata.clone();
                let page = page.clone();
                let image_pages = Arc::clone(&image_pages);
                let image = image_server.clone();
                let seen_images = Arc::clone(&seen_images);
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    if path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.starts_with("/g/123/abcdef0123/") {
                        respond(&mut socket, "200 OK", "text/html", &metadata).await;
                    } else if path.contains("123456-1") {
                        respond(&mut socket, "200 OK", "text/html", &image_pages[0]).await;
                    } else if path.contains("123456-2") {
                        respond(&mut socket, "200 OK", "text/html", &image_pages[1]).await;
                    } else if path.contains("/img/") {
                        seen_images.fetch_add(1, Ordering::SeqCst);
                        respond(&mut socket, "200 OK", "image/jpeg", &image).await;
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let config = m4_config(root.path(), 2, 2);
        let mut app = m4_app(root.path(), config);
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::json!({"scan_thread_cnt": 2, "download_thread_cnt": 2}),
        );
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = wait_state(&handle, &guid, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let (a, b) = handle.borrow_app(|app| {
                    let task = app.task(&guid).unwrap();
                    assert_eq!(task.meta_i64("finished").unwrap(), 2);
                    assert!(task.duplicate_pairs().is_empty());
                    let dir = task.get_fpath().unwrap();
                    (
                        std::fs::read(dir.join("1.jpg")).unwrap(),
                        std::fs::read(dir.join("2.jpg")).unwrap(),
                    )
                });
                assert_eq!(a, b);
                assert_eq!(a, image);
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
        server.abort();
        assert_eq!(image_requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn download_worker_retries_exactly_three_times_before_the_fake_rescan() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40 = "0123456789abcdef0123456789abcdef01234567";
        let size = 1600usize;
        let image = image_body(31, size);
        let image_server = image.clone();
        let metadata = metadata_body(1);
        let page = page_body("aaaa000001", 1);
        let img_page = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=x\" style=\"x\">\
             </a></div><div>one.jpg :: 100x200 :: 2 KB</di>return nl('aaaa000001-1')"
        )
        .into_bytes();
        let attempts = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&attempts);
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let metadata = metadata.clone();
                let page = page.clone();
                let img_page = img_page.clone();
                let image = image_server.clone();
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    if path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.starts_with("/g/123/abcdef0123/") {
                        respond(&mut socket, "200 OK", "text/html", &metadata).await;
                    } else if path.contains("/s/") {
                        respond(&mut socket, "200 OK", "text/html", &img_page).await;
                    } else if path.contains("/img/") {
                        let attempt = seen.fetch_add(1, Ordering::SeqCst);
                        if attempt < 3 {
                            // Close without a response: a retryable protocol
                            // error inside the worker's HttpReq (retry 3).
                            let _ = socket.shutdown().await;
                        } else {
                            respond(&mut socket, "200 OK", "image/jpeg", &image).await;
                        }
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.download_timeout = 5;
        let mut app = m4_app(root.path(), config);
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1}),
        );
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (mut run, handle) = runtime.spawn();
                tokio::select! {
                    _ = wait_state(&handle, &guid, consts::TASK_STATE_FINISHED) => {}
                    result = &mut run => panic!("runtime stopped early: {result:?}"),
                }
                let dir = handle.borrow_app(|app| app.task(&guid).unwrap().get_fpath().unwrap());
                assert_eq!(std::fs::read(dir.join("1.jpg")).unwrap(), image);
                handle.shutdown().await;
                run.await.unwrap();
            })
            .await;
        server.abort();
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            4,
            "three failures then rescan success"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pause_waits_for_a_queued_blocking_stage_open_before_reporting_quiescent() {
        use crate::core::{IoFault, arm_slow_open, set_io_fault};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let hash40 = "0123456789abcdef0123456789abcdef01234567";
        let size = 900usize;
        let image = image_body(41, size);
        let metadata = metadata_body(1);
        let page = page_body("aaaa000001", 1);
        let img_page = format!(
            "<img src=\"http://127.0.0.1:{port}/img/{hash40}-{size}-200-0-jpg/keystamp=x\" style=\"x\">\
             </a></div><div>one.jpg :: 100x200 :: 2 KB</di>return nl('aaaa000001-1')"
        )
        .into_bytes();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let metadata = metadata.clone();
                let page = page.clone();
                let img_page = img_page.clone();
                let image = image.clone();
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    if path.contains("?p=") {
                        respond(&mut socket, "200 OK", "text/html", &page).await;
                    } else if path.starts_with("/g/123/abcdef0123/") {
                        respond(&mut socket, "200 OK", "text/html", &metadata).await;
                    } else if path.contains("/s/") {
                        respond(&mut socket, "200 OK", "text/html", &img_page).await;
                    } else if path.contains("/img/") {
                        respond(&mut socket, "200 OK", "image/jpeg", &image).await;
                    } else {
                        respond(&mut socket, "404 Not Found", "text/html", b"missing").await;
                    }
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let config = m4_config(root.path(), 1, 1);
        let mut app = m4_app(root.path(), config);
        let guid = m4_task(
            &mut app,
            root.path(),
            "http://e-hentai.org/g/123/abcdef0123/",
            serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1}),
        );
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let slow = arm_slow_open();
                set_io_fault(Some(IoFault::SlowOpen));
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(10);
                while slow.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!("blocking stage open did not start: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let mut pause = Box::pin(handle.pause_task_and_wait(&guid));
                let early = tokio::time::timeout(Duration::from_millis(150), &mut pause).await;
                assert!(
                    early.is_err(),
                    "pause cannot be quiescent while the OS open is owned"
                );
                let _ = slow.release.send(());
                let report = pause.await.unwrap();
                assert_eq!(report.blocking_pending, 0);
                let _ = slow.finished.try_recv();
                set_io_fault(None);
                assert_eq!(handle.task_state(&guid), Some(consts::TASK_STATE_PAUSED));
                handle.shutdown().await;
                let _ = run.await;
            })
            .await;
        server.abort();
        let _ = std::fs::read_dir(root.path().join("downloads")).map(|entries| {
            for entry in entries.flatten() {
                assert!(!entry.file_name().to_string_lossy().starts_with("xehentai-"));
            }
        });
        let _ = image;
    }

    #[tokio::test]
    async fn monitor_quota_ignored_errors_and_zombie_reentry_follow_the_reference() {
        let root = tempfile::tempdir().unwrap();
        let (runtime, guid) = monitor_fixture(root.path(), 10, Vec::new());
        let names: Vec<String> = (1..=5).map(|i| format!("down-{i}")).collect();
        let local = LocalSet::new();
        local
            .run_until(async {
                let (monitor, join) = runtime.start_monitor_for_test(&guid, &names);
                let refs: Vec<WorkerRef> = names
                    .iter()
                    .map(|name| WorkerRef {
                        name: name.clone(),
                        alive: Arc::new(AtomicBool::new(true)),
                        speed: None,
                        zombie_threshold: 1.0e9,
                    })
                    .collect();
                for reference in &refs {
                    monitor.register(reference.clone());
                }
                for _ in 0..names.len() {
                    monitor.vote("down-1", consts::ERR_QUOTA_EXCEEDED);
                }
                let handle = runtime.handle();
                wait_trace_count(&handle, "monitor.quota", 1).await;
                assert_eq!(handle.task_state(&guid), Some(consts::TASK_STATE_FAILED));
                monitor.request_exit();
                let _ = join.await;

                // A zombie threshold passed without a heartbeat yields a
                // zombie; a late heartbeat removes the flag and re-enters.
                let (runtime2, guid2) = monitor_fixture(root.path(), 10, Vec::new());
                let names2 = vec!["down-1".to_string()];
                let (monitor2, join2) = runtime2.start_monitor_for_test(&guid2, &names2);
                let reference = WorkerRef {
                    name: "down-1".into(),
                    alive: Arc::new(AtomicBool::new(true)),
                    speed: None,
                    zombie_threshold: 0.5,
                };
                monitor2.register(reference.clone());
                let handle2 = runtime2.handle();
                wait_trace_count(&handle2, "monitor.zombie", 1).await;
                assert_eq!(monitor2.zombie_count(), 1);
                assert!(!monitor2.keepalive(&reference, false));
                assert_eq!(monitor2.zombie_count(), 0);
                assert_eq!(monitor2.running_count(), 1);
                monitor2.keepalive(&reference, true);
                assert_eq!(monitor2.running_count(), 0, "worker exit removes last_seen");
                monitor2.request_exit();
                let _ = join2.await;

                // Ignored quota votes never fail the task.
                let (runtime3, guid3) =
                    monitor_fixture(root.path(), 10, vec![consts::ERR_QUOTA_EXCEEDED]);
                let names3 = vec!["down-1".to_string()];
                let (monitor3, join3) = runtime3.start_monitor_for_test(&guid3, &names3);
                let reference = WorkerRef {
                    name: "down-1".into(),
                    alive: Arc::new(AtomicBool::new(true)),
                    speed: None,
                    zombie_threshold: 1.0e9,
                };
                monitor3.register(reference);
                monitor3.vote("down-1", consts::ERR_QUOTA_EXCEEDED);
                let handle3 = runtime3.handle();
                wait_trace_count(&handle3, "monitor.stats", 1).await;
                assert_ne!(handle3.task_state(&guid3), Some(consts::TASK_STATE_FAILED));
                assert!(
                    !handle3
                        .trace()
                        .iter()
                        .any(|event| event.kind == "monitor.quota")
                );
                monitor3.request_exit();
                let _ = join3.await;
            })
            .await;
    }

    // --- M4 R1: archive run identity and previous-archive retirement -------

    fn m4_task_named(
        app: &mut App,
        root: &Path,
        guid: &str,
        url: &str,
        kwargs: serde_json::Value,
    ) -> String {
        let config = TaskConfig::from_app(&app.cfg, kwargs.as_object().unwrap()).unwrap();
        let task = Task::new(url, config, guid.into(), root).unwrap();
        app.insert_task_for_test(task)
    }

    /// Put a real task directly at DOWNLOAD with a real gallery directory.
    fn ready_archive_task(
        app: &mut App,
        guid: &str,
        title: &str,
        gallery: &Path,
        total: i64,
        finished: i64,
    ) {
        std::fs::create_dir_all(gallery).unwrap();
        let task = app.task_mut(guid).unwrap();
        task.state = consts::TASK_STATE_DOWNLOAD;
        task.meta.insert("title".into(), serde_json::json!(title));
        task.meta.insert("total".into(), serde_json::json!(total));
        task.meta
            .insert("finished".into(), serde_json::json!(finished));
        task.meta
            .insert("thumbnail_cnt".into(), serde_json::json!(20));
        task.img_q = Some(VecDeque::new());
        task.page_q = Some(VecDeque::new());
    }

    fn leftover_private_entries(dir: &Path) -> Vec<String> {
        let mut names = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name.starts_with(".xehentai-archive-")
                    || name.starts_with(".xehentai-remove-")
                    || name.starts_with(".xehentai-detached-")
                {
                    names.push(name);
                }
            }
        }
        names
    }

    async fn wait_until(mut predicate: impl FnMut() -> bool, what: &str) {
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !predicate() {
            if std::time::Instant::now() > deadline {
                panic!("timed out waiting for {what}");
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Bounded observation window after a gated writer is released: stop as
    /// soon as the worker records an archive effect, changes the task state or
    /// creates the ZIP. It never proves success by elapsed time; the
    /// assertions decide.
    async fn wait_for_archive_effect(handle: &AppHandle, guid: &str, state: i64, zip: &Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let recorded = handle
                .trace()
                .iter()
                .any(|event| event.kind.starts_with("archive."));
            if handle.task_state(guid) != Some(state) || zip.exists() || recorded {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The archive worker observed FINISHED and is held at the wakeup seam.
    /// A same-guid `add_task` then advances the run; the old worker may not
    /// follow the replacement into admission, state 19, ZIP or gallery
    /// removal.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn archive_waiting_worker_cannot_follow_a_same_guid_replacement_run() {
        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app(root.path(), config);
        let url = "http://e-hentai.org/g/123/abcdef0123/";
        let guid = m4_task(
            &mut app,
            root.path(),
            url,
            serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true}),
        );
        let gallery = root.path().join("downloads").join("archived");
        ready_archive_task(&mut app, &guid, "archived", &gallery, 1, 1);
        std::fs::write(gallery.join("1.jpg"), b"old-run-bytes").unwrap();
        let zip = root.path().join("downloads").join("archived.zip");
        let waiting = arm_archive_waiting(&gallery.to_string_lossy());
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while waiting.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!(
                            "archive worker never reached the wakeup seam: {:?}",
                            handle.trace()
                        );
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(handle.task_state(&guid), Some(consts::TASK_STATE_FINISHED));
                // Real App mutation: a finished task's same-guid add_task
                // resets it to WAITING and advances the run marker.
                let (code, returned) =
                    handle.borrow_app_mut(|app| app.add_task(url, serde_json::Map::new()).unwrap());
                assert_eq!(code, consts::ERR_NO_ERROR);
                assert_eq!(returned.as_deref(), Some(guid.as_str()));
                assert_eq!(handle.task_state(&guid), Some(consts::TASK_STATE_WAITING));
                // The replacement run reaches FINISHED before the old worker
                // wakes: the state window alone cannot tell them apart, only
                // the captured owner/run can.
                handle.borrow_app_mut(|app| {
                    app.task_mut(&guid).unwrap().state = consts::TASK_STATE_FINISHED;
                });
                assert_eq!(handle.task_state(&guid), Some(consts::TASK_STATE_FINISHED));
                let _ = waiting.release.send(());
                wait_for_archive_effect(&handle, &guid, consts::TASK_STATE_FINISHED, &zip).await;
                let report = handle.shutdown().await;
                assert!(report.joined >= 1, "{report:?}");
                run.await.unwrap();
                assert_eq!(
                    handle.task_state(&guid),
                    Some(consts::TASK_STATE_FINISHED),
                    "an old wakeup must not admit the replacement run"
                );
                assert_eq!(
                    handle.borrow_app(|app| app.task(&guid).unwrap().failcode),
                    0
                );
                assert!(!zip.exists(), "no old ZIP may be published");
                assert_eq!(
                    std::fs::read(gallery.join("1.jpg")).unwrap(),
                    b"old-run-bytes"
                );
                assert!(leftover_private_entries(&root.path().join("downloads")).is_empty());
                assert_eq!(handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE), 0);
            })
            .await;
    }

    /// One stale-writer scenario: pause -> delete -> recreate the same guid,
    /// then release the blocking writer. `fault` selects the success path or
    /// one of the two failure branches (preparation error / join error).
    async fn stale_writer_case(fault: Option<ArchiveFault>) {
        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app(root.path(), config);
        let url = "http://e-hentai.org/g/123/abcdef0123/";
        let guid = m4_task(
            &mut app,
            root.path(),
            url,
            serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true}),
        );
        let gallery = root.path().join("downloads").join("archived");
        ready_archive_task(&mut app, &guid, "archived", &gallery, 1, 1);
        std::fs::write(gallery.join("1.jpg"), b"old-run-bytes").unwrap();
        let zip = root.path().join("downloads").join("archived.zip");
        let downloads = root.path().join("downloads");
        let marker = gallery.to_string_lossy().into_owned();
        reset_archive_writer_stats(&downloads);
        let gate = arm_archive_gate(&marker);
        if let Some(fault) = fault {
            arm_archive_fault(&marker, fault);
        }
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while gate.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!("blocking writer never started: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(
                    handle.task_state(&guid),
                    Some(consts::TASK_STATE_MAKE_ARCHIVE)
                );
                handle.borrow_app_mut(|app| {
                    assert_eq!(app.pause_task(&guid).unwrap().0, consts::ERR_NO_ERROR);
                    assert_eq!(app.del_task(&guid).unwrap().0, consts::ERR_NO_ERROR);
                    let (code, returned) = app.add_task(url, serde_json::Map::new()).unwrap();
                    assert_eq!(code, consts::ERR_NO_ERROR);
                    assert_eq!(returned.as_deref(), Some(guid.as_str()));
                    assert_eq!(app.task_state(&guid), Some(consts::TASK_STATE_WAITING));
                    // The replacement run owns the same public gallery name.
                    // Drive it to a real FINISHED while the old writer is
                    // still blocked; it must not start an archive itself.
                    let task = app.task_mut(&guid).unwrap();
                    task.config.make_archive = false;
                    task.state = consts::TASK_STATE_DOWNLOAD;
                    task.meta
                        .insert("title".into(), serde_json::json!("archived"));
                    task.meta.insert("total".into(), serde_json::json!(1));
                    task.meta.insert("finished".into(), serde_json::json!(1));
                    task.meta
                        .insert("thumbnail_cnt".into(), serde_json::json!(20));
                    task.img_q = Some(VecDeque::new());
                    task.page_q = Some(VecDeque::new());
                });
                std::fs::create_dir_all(&gallery).unwrap();
                std::fs::write(gallery.join("1.jpg"), b"new-run-bytes").unwrap();
                std::fs::write(gallery.join("new-only.txt"), b"only-new-run").unwrap();
                wait_until(
                    || handle.task_state(&guid) == Some(consts::TASK_STATE_FINISHED),
                    "replacement run completion",
                )
                .await;
                let _ = gate.release.send(());
                wait_for_archive_effect(&handle, &guid, consts::TASK_STATE_FINISHED, &zip).await;
                let _ = handle.shutdown().await;
                run.await.unwrap();
                assert_eq!(
                    handle.task_state(&guid),
                    Some(consts::TASK_STATE_FINISHED),
                    "a stale writer must not change the replacement task state"
                );
                let (failcode, meta_len, queues) = handle.borrow_app(|app| {
                    let task = app.task(&guid).unwrap();
                    (
                        task.failcode,
                        task.meta.len(),
                        (task.img_q.is_none(), task.page_q.is_none()),
                    )
                });
                assert_eq!(failcode, 0);
                assert_eq!(meta_len, 4, "the replacement metadata is untouched");
                assert_eq!(queues, (true, true));
                assert!(!zip.exists(), "a stale archive must not publish a ZIP");
                assert_eq!(
                    std::fs::read(gallery.join("1.jpg")).unwrap(),
                    b"new-run-bytes",
                    "a stale archive must not remove the replacement gallery"
                );
                assert_eq!(
                    std::fs::read(gallery.join("new-only.txt")).unwrap(),
                    b"only-new-run"
                );
                assert!(
                    leftover_private_entries(&root.path().join("downloads")).is_empty(),
                    "the discarded private stage must be removed"
                );
                let (active, peak) = archive_writer_stats(&downloads);
                assert_eq!(active, 0);
                assert!(peak <= 1);
                assert_eq!(handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE), 0);
            })
            .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_archive_success_cannot_publish_or_remove_a_new_gallery() {
        stale_writer_case(None).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_archive_prepare_error_cannot_fail_a_new_owner() {
        stale_writer_case(Some(ArchiveFault::Prepare)).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn stale_archive_join_error_cannot_fail_a_new_owner() {
        stale_writer_case(Some(ArchiveFault::Panic)).await;
    }

    /// Three real archive tasks with distinct galleries. The first writer is
    /// held; the second task's downloads must already be running, but its
    /// archive may not start until the first worker is actually joined. The
    /// third task cannot pass that stage boundary either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn previous_archive_is_joined_before_the_next_archive_admission() {
        let hash_a = "0123456789abcdef0123456789abcdef01234567";
        let hash_b = "89abcdef0123456789abcdef0123456789abcdef";
        let hash_c = "fedcba9876543210fedcba9876543210fedcba98";
        let image_a = image_body(3, 640);
        let image_b = image_body(4, 700);
        let image_c = image_body(5, 760);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        let server_bodies = [
            (hash_a.to_string(), image_a.clone()),
            (hash_b.to_string(), image_b.clone()),
            (hash_c.to_string(), image_c.clone()),
        ];
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                let bodies = server_bodies.clone();
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    seen.lock().unwrap().push(path.clone());
                    let body = bodies
                        .iter()
                        .find(|(hash, _)| path.contains(hash))
                        .map(|(_, body)| body.clone())
                        .unwrap_or_default();
                    respond(&mut socket, "200 OK", "image/jpeg", &body).await;
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app_guids(root.path(), config, &["aaaa0001", "bbbb0002", "cccc0003"]);
        let url = "http://e-hentai.org/g/123/abcdef0123/";
        let mut guids = Vec::new();
        for (guid, title) in [("aaaa0001", "aa"), ("bbbb0002", "bb"), ("cccc0003", "cc")] {
            guids.push(m4_task_named(
                &mut app,
                root.path(),
                guid,
                url,
                serde_json::json!({
                    "scan_thread_cnt": 1,
                    "download_thread_cnt": 1,
                    "make_archive": true,
                }),
            ));
            let gallery = root.path().join("downloads").join(title);
            ready_archive_task(&mut app, guid, title, &gallery, 1, 0);
        }
        let setups = [
            (guids[0].as_str(), "aa", hash_a, "0123456789", &image_a),
            (guids[1].as_str(), "bb", hash_b, "89abcdef01", &image_b),
            (guids[2].as_str(), "cc", hash_c, "fedcba9876", &image_c),
        ];
        for (guid, _title, hash40, hash10, body) in setups {
            let task = app.task_mut(guid).unwrap();
            let image_url = format!(
                "http://127.0.0.1:{port}/img/{hash40}-{}-200-0-jpg/keystamp=x",
                body.len()
            );
            task.img_q = Some(VecDeque::from([image_url]));
            task.reload_map.insert(
                hash10.into(),
                (
                    format!("http://e-hentai.org/s/{hash10}/123456-1"),
                    "one.jpg".into(),
                ),
            );
        }
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let gallery_a = root.path().join("downloads").join("aa");
        let marker_a = gallery_a.to_string_lossy().into_owned();
        let downloads = root.path().join("downloads");
        let zip_b = downloads.join("bb.zip");
        reset_archive_writer_stats(&downloads);
        let gate = arm_archive_gate(&marker_a);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while gate.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!("first archive writer never started: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(
                    handle.task_state(&guids[0]),
                    Some(consts::TASK_STATE_MAKE_ARCHIVE)
                );
                wait_until(
                    {
                        let requests = Arc::clone(&requests);
                        move || {
                            requests
                                .lock()
                                .unwrap()
                                .iter()
                                .any(|path| path.contains(hash_b))
                        }
                    },
                    "second task's image download",
                )
                .await;
                wait_until(
                    || handle.task_state(&guids[1]) == Some(consts::TASK_STATE_FINISHED),
                    "second task's download completion",
                )
                .await;
                // Give the old implementation its synchronous chance to admit
                // the second archive while the first writer is still held.
                let chance = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < chance
                    && handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE) <= 1
                    && !zip_b.exists()
                    && archive_writer_log(&downloads).len() == 1
                {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert!(
                    handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE) <= 1,
                    "no second archive handle may accumulate in state 19"
                );
                assert!(
                    !zip_b.exists(),
                    "second archive started before the first was joined"
                );
                assert_eq!(
                    archive_writer_log(&downloads),
                    vec![marker_a.clone()],
                    "no second archive writer may start before the first is joined"
                );
                assert_ne!(
                    handle.task_state(&guids[1]),
                    Some(consts::TASK_STATE_MAKE_ARCHIVE)
                );
                assert_eq!(
                    requests
                        .lock()
                        .unwrap()
                        .iter()
                        .filter(|path| path.contains(hash_c))
                        .count(),
                    0,
                    "the third task must not pass the archive stage boundary"
                );
                let _ = gate.release.send(());
                let archive_names = ["aa", "bb", "cc"];
                wait_until(
                    || {
                        archive_names.iter().all(|title| {
                            root.path()
                                .join("downloads")
                                .join(format!("{title}.zip"))
                                .exists()
                        }) && guids.iter().all(|guid| {
                            handle.task_state(guid) == Some(consts::TASK_STATE_FINISHED)
                        })
                    },
                    "all three archives",
                )
                .await;
                let (active, peak) = archive_writer_stats(&downloads);
                assert_eq!(active, 0);
                assert_eq!(peak, 1, "archive writers must never overlap");
                let expected: Vec<String> = ["aa", "bb", "cc"]
                    .iter()
                    .map(|title| {
                        root.path()
                            .join("downloads")
                            .join(title)
                            .to_string_lossy()
                            .into_owned()
                    })
                    .collect();
                assert_eq!(archive_writer_log(&downloads), expected);
                for (title, body) in [("aa", &image_a), ("bb", &image_b), ("cc", &image_c)] {
                    let gallery = root.path().join("downloads").join(title);
                    let zip = root.path().join("downloads").join(format!("{title}.zip"));
                    assert!(!gallery.exists(), "{title} gallery removed after archive");
                    let mut archive =
                        zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
                    assert_eq!(archive.len(), 1);
                    let mut entry = archive.by_index(0).unwrap();
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
                    assert_eq!(&bytes, body, "{title} zip bytes");
                }
                assert!(leftover_private_entries(&root.path().join("downloads")).is_empty());
                let _ = handle.shutdown().await;
                let _ = run.await;
                assert_eq!(handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE), 0);
            })
            .await;
        server.abort();
    }

    /// Shutdown while the real ZIP writer is still blocked must wait for the
    /// thread's completion acknowledgement and must not publish a late ZIP or
    /// state change afterwards.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_waits_for_a_pending_archive_writer_before_retirement() {
        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app(root.path(), config);
        let url = "http://e-hentai.org/g/123/abcdef0123/";
        let guid = m4_task(
            &mut app,
            root.path(),
            url,
            serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true}),
        );
        let gallery = root.path().join("downloads").join("pending");
        ready_archive_task(&mut app, &guid, "pending", &gallery, 1, 1);
        std::fs::write(gallery.join("1.jpg"), b"pending-bytes").unwrap();
        let zip = root.path().join("downloads").join("pending.zip");
        let downloads = root.path().join("downloads");
        let marker = gallery.to_string_lossy().into_owned();
        reset_archive_writer_stats(&downloads);
        let gate = arm_archive_gate(&marker);
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while gate.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!("blocking writer never started: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(
                    handle.task_state(&guid),
                    Some(consts::TASK_STATE_MAKE_ARCHIVE)
                );
                let mut shutdown = Box::pin(handle.shutdown());
                let early = tokio::time::timeout(Duration::from_millis(200), &mut shutdown).await;
                assert!(
                    early.is_err(),
                    "shutdown reported retirement while the ZIP writer was still pending"
                );
                assert_eq!(
                    handle.task_state(&guid),
                    Some(consts::TASK_STATE_MAKE_ARCHIVE)
                );
                assert!(gallery.join("1.jpg").exists());
                assert!(!zip.exists());
                let _ = gate.release.send(());
                let report = shutdown.await;
                assert!(report.joined >= 1, "{report:?}");
                let _ = run.await;
                assert_eq!(
                    archive_writer_stats(&downloads).0,
                    0,
                    "the blocking writer really retired before shutdown returned"
                );
                assert_eq!(
                    handle.task_state(&guid),
                    Some(consts::TASK_STATE_MAKE_ARCHIVE),
                    "shutdown must not claim a finished archive"
                );
                assert!(!zip.exists(), "no late ZIP after shutdown");
                assert_eq!(
                    std::fs::read(gallery.join("1.jpg")).unwrap(),
                    b"pending-bytes"
                );
                assert!(leftover_private_entries(&root.path().join("downloads")).is_empty());
                assert_eq!(handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE), 0);
            })
            .await;
    }

    // --- M4 R2 D1: public drains must see joins owned by another task loop --
    //
    // Task B's DOWNLOAD iteration takes task A's archive handle into its own
    // `do_task` local (`take_slots(19)` at runtime.rs:763) and blocks joining
    // it while B's downloads run. A public drain that only takes the
    // currently shared slots cannot report retired while that join is still
    // active. These fixtures pin that window deterministically: B reaching
    // FINISHED proves B's downloads ran, and the held writer gate proves A is
    // still being joined, so B's `do_task` must already hold A's handle.

    /// Public `shutdown` must stay pending while the archive join is owned by
    /// another task loop, then retire only after the real writer completion.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn public_shutdown_sees_an_archive_join_held_by_another_task_loop() {
        let hash_b = "89abcdef0123456789abcdef0123456789abcdef";
        let image_b = image_body(4, 700);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = image_b.clone();
                let hash = hash_b.to_string();
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    let content = if path.contains(&hash) {
                        body
                    } else {
                        Vec::new()
                    };
                    respond(&mut socket, "200 OK", "image/jpeg", &content).await;
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app_guids(root.path(), config, &["aaaa0001", "bbbb0002"]);
        let url = "http://e-hentai.org/g/123/abcdef0123/";
        let guid_a = m4_task_named(
            &mut app,
            root.path(),
            "aaaa0001",
            url,
            serde_json::json!({
                "scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true,
            }),
        );
        let guid_b = m4_task_named(
            &mut app,
            root.path(),
            "bbbb0002",
            url,
            serde_json::json!({
                "scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true,
            }),
        );
        let gallery_a = root.path().join("downloads").join("aa");
        ready_archive_task(&mut app, &guid_a, "aa", &gallery_a, 1, 1);
        std::fs::write(gallery_a.join("1.jpg"), b"a-bytes").unwrap();
        let gallery_b = root.path().join("downloads").join("bb");
        ready_archive_task(&mut app, &guid_b, "bb", &gallery_b, 1, 0);
        {
            let task = app.task_mut(&guid_b).unwrap();
            let image_url = format!(
                "http://127.0.0.1:{port}/img/{hash_b}-{}-200-0-jpg/keystamp=x",
                700
            );
            task.img_q = Some(VecDeque::from([image_url]));
            task.reload_map.insert(
                "89abcdef01".into(),
                (
                    "http://e-hentai.org/s/89abcdef01/123456-1".into(),
                    "one.jpg".into(),
                ),
            );
        }
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let downloads = root.path().join("downloads");
        let marker_a = gallery_a.to_string_lossy().into_owned();
        reset_archive_writer_stats(&downloads);
        let gate = arm_archive_gate(&marker_a);
        let zip_a = downloads.join("aa.zip");
        let zip_b = downloads.join("bb.zip");
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while gate.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!("A writer never started: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                assert_eq!(
                    handle.task_state(&guid_a),
                    Some(consts::TASK_STATE_MAKE_ARCHIVE)
                );
                // B's downloads ran to completion while A is still held: B's
                // `do_task` must already hold A's archive handle locally.
                wait_state(&handle, &guid_b, consts::TASK_STATE_FINISHED).await;
                let mut shutdown = Box::pin(handle.shutdown());
                let early = tokio::time::timeout(Duration::from_millis(200), &mut shutdown).await;
                assert!(
                    early.is_err(),
                    "public shutdown retired while the archive join was still \
                     owned by another task loop"
                );
                assert!(!zip_a.exists(), "no ZIP while the writer is held");
                let _ = gate.release.send(());
                let report = tokio::time::timeout(Duration::from_secs(20), shutdown)
                    .await
                    .expect("shutdown must retire after the real writer completion");
                assert!(report.joined >= 1, "{report:?}");
                assert_eq!(report.blocking_pending, 0);
                let _ = tokio::time::timeout(Duration::from_secs(20), run)
                    .await
                    .expect("runtime must stop after shutdown");
                assert!(!zip_a.exists(), "no late ZIP after shutdown");
                assert!(!zip_b.exists(), "B never reached archive admission");
                assert_eq!(
                    std::fs::read(gallery_a.join("1.jpg")).unwrap(),
                    b"a-bytes",
                    "the held gallery is untouched"
                );
                assert!(gallery_b.join("1.jpg").exists());
                assert!(leftover_private_entries(&root.path().join("downloads")).is_empty());
                assert_eq!(
                    archive_writer_stats(&root.path().join("downloads")).0,
                    0,
                    "the blocking writer really retired before shutdown returned"
                );
                assert_eq!(handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE), 0);
            })
            .await;
        server.abort();
    }

    /// Public `pause_task_and_wait` on the held task must stay pending until
    /// the owned archive join actually retires, not report quiescent early.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn public_pause_waits_for_an_archive_join_held_by_another_task_loop() {
        let hash_b = "89abcdef0123456789abcdef0123456789abcdef";
        let image_b = image_body(4, 700);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = image_b.clone();
                let hash = hash_b.to_string();
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    let content = if path.contains(&hash) {
                        body
                    } else {
                        Vec::new()
                    };
                    respond(&mut socket, "200 OK", "image/jpeg", &content).await;
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app_guids(root.path(), config, &["aaaa0001", "bbbb0002"]);
        let url = "http://e-hentai.org/g/123/abcdef0123/";
        let guid_a = m4_task_named(
            &mut app,
            root.path(),
            "aaaa0001",
            url,
            serde_json::json!({
                "scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true,
            }),
        );
        let guid_b = m4_task_named(
            &mut app,
            root.path(),
            "bbbb0002",
            url,
            serde_json::json!({
                "scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true,
            }),
        );
        let gallery_a = root.path().join("downloads").join("aa");
        ready_archive_task(&mut app, &guid_a, "aa", &gallery_a, 1, 1);
        std::fs::write(gallery_a.join("1.jpg"), b"a-bytes").unwrap();
        let gallery_b = root.path().join("downloads").join("bb");
        ready_archive_task(&mut app, &guid_b, "bb", &gallery_b, 1, 0);
        {
            let task = app.task_mut(&guid_b).unwrap();
            let image_url = format!(
                "http://127.0.0.1:{port}/img/{hash_b}-{}-200-0-jpg/keystamp=x",
                700
            );
            task.img_q = Some(VecDeque::from([image_url]));
            task.reload_map.insert(
                "89abcdef01".into(),
                (
                    "http://e-hentai.org/s/89abcdef01/123456-1".into(),
                    "one.jpg".into(),
                ),
            );
        }
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let downloads = root.path().join("downloads");
        let marker_a = gallery_a.to_string_lossy().into_owned();
        reset_archive_writer_stats(&downloads);
        let gate = arm_archive_gate(&marker_a);
        let zip_a = downloads.join("aa.zip");
        let zip_b = downloads.join("bb.zip");
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while gate.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!("A writer never started: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                wait_state(&handle, &guid_b, consts::TASK_STATE_FINISHED).await;
                let mut pause = Box::pin(handle.pause_task_and_wait(&guid_a));
                let early = tokio::time::timeout(Duration::from_millis(200), &mut pause).await;
                assert!(
                    early.is_err(),
                    "pause reported quiescent while the owned archive join was \
                     still held by another task loop"
                );
                let _ = gate.release.send(());
                let report = tokio::time::timeout(Duration::from_secs(20), pause)
                    .await
                    .expect("pause must retire after the real writer completion")
                    .unwrap();
                assert!(report.joined >= 1, "{report:?}");
                assert_eq!(report.blocking_pending, 0);
                assert_eq!(handle.task_state(&guid_a), Some(consts::TASK_STATE_PAUSED));
                assert!(
                    !zip_a.exists(),
                    "a stale writer after pause must not publish"
                );
                assert_eq!(std::fs::read(gallery_a.join("1.jpg")).unwrap(), b"a-bytes");
                // B was never paused: its loop still owns the shared
                // completion and must archive normally once A retires.
                wait_state(&handle, &guid_b, consts::TASK_STATE_FINISHED).await;
                wait_until(|| zip_b.exists(), "B archive after A retirement").await;
                let _ = handle.shutdown().await;
                let _ = tokio::time::timeout(Duration::from_secs(20), run)
                    .await
                    .expect("runtime must stop after shutdown");
                assert!(leftover_private_entries(&root.path().join("downloads")).is_empty());
                assert_eq!(handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE), 0);
            })
            .await;
        server.abort();
    }

    /// Two overlapping public drains plus a dropped waiter: cancelling one
    /// waiter must neither detach the owned join nor release the other drain.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_dropped_drain_waiter_never_detaches_the_owned_archive_join() {
        let hash_b = "89abcdef0123456789abcdef0123456789abcdef";
        let image_b = image_body(4, 700);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let body = image_b.clone();
                let hash = hash_b.to_string();
                tokio::spawn(async move {
                    let head = read_head(&mut socket).await;
                    let path = path_of(&head);
                    let content = if path.contains(&hash) {
                        body
                    } else {
                        Vec::new()
                    };
                    respond(&mut socket, "200 OK", "image/jpeg", &content).await;
                });
            }
        });

        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app_guids(root.path(), config, &["aaaa0001", "bbbb0002"]);
        let url = "http://e-hentai.org/g/123/abcdef0123/";
        let guid_a = m4_task_named(
            &mut app,
            root.path(),
            "aaaa0001",
            url,
            serde_json::json!({
                "scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true,
            }),
        );
        let guid_b = m4_task_named(
            &mut app,
            root.path(),
            "bbbb0002",
            url,
            serde_json::json!({
                "scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true,
            }),
        );
        let gallery_a = root.path().join("downloads").join("aa");
        ready_archive_task(&mut app, &guid_a, "aa", &gallery_a, 1, 1);
        std::fs::write(gallery_a.join("1.jpg"), b"a-bytes").unwrap();
        let gallery_b = root.path().join("downloads").join("bb");
        ready_archive_task(&mut app, &guid_b, "bb", &gallery_b, 1, 0);
        {
            let task = app.task_mut(&guid_b).unwrap();
            let image_url = format!(
                "http://127.0.0.1:{port}/img/{hash_b}-{}-200-0-jpg/keystamp=x",
                700
            );
            task.img_q = Some(VecDeque::from([image_url]));
            task.reload_map.insert(
                "89abcdef01".into(),
                (
                    "http://e-hentai.org/s/89abcdef01/123456-1".into(),
                    "one.jpg".into(),
                ),
            );
        }
        m4_session(
            &mut app,
            Arc::new(crate::worker::RealClock),
            Arc::new(FixedResolver::new(
                format!("127.0.0.1:{port}").parse::<SocketAddr>().unwrap(),
            )),
        );
        let downloads = root.path().join("downloads");
        let marker_a = gallery_a.to_string_lossy().into_owned();
        reset_archive_writer_stats(&downloads);
        let gate = arm_archive_gate(&marker_a);
        let zip_a = downloads.join("aa.zip");
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while gate.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!("A writer never started: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                wait_state(&handle, &guid_b, consts::TASK_STATE_FINISHED).await;
                let mut waiter = Box::pin(handle.pause_task_and_wait(&guid_a));
                let mut shutdown = Box::pin(handle.shutdown());
                assert!(
                    tokio::time::timeout(Duration::from_millis(200), &mut waiter)
                        .await
                        .is_err(),
                    "first waiter must observe the owned completion"
                );
                assert!(
                    tokio::time::timeout(Duration::from_millis(200), &mut shutdown)
                        .await
                        .is_err(),
                    "second waiter must observe the same owned completion"
                );
                drop(waiter);
                assert!(
                    tokio::time::timeout(Duration::from_millis(200), &mut shutdown)
                        .await
                        .is_err(),
                    "dropping one waiter must not detach the owned join or \
                     release the other drain"
                );
                let _ = gate.release.send(());
                let report = tokio::time::timeout(Duration::from_secs(20), shutdown)
                    .await
                    .expect("surviving drain must retire after the real completion");
                assert!(report.joined >= 1, "{report:?}");
                let _ = tokio::time::timeout(Duration::from_secs(20), run)
                    .await
                    .expect("runtime must stop after shutdown");
                assert!(!zip_a.exists(), "no late ZIP after shutdown");
                assert_eq!(std::fs::read(gallery_a.join("1.jpg")).unwrap(), b"a-bytes");
                assert_eq!(
                    archive_writer_log(&root.path().join("downloads")),
                    vec![marker_a],
                    "the writer ran exactly once: no double join"
                );
                assert!(leftover_private_entries(&root.path().join("downloads")).is_empty());
                assert_eq!(handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE), 0);
            })
            .await;
        server.abort();
    }

    // --- M4 R2 N1: exclusively owned detached namespaces -------------------
    //
    // `detach_gallery` (task/archive.rs) builds its private name with a
    // counter and a plain rename. A legal gallery whose title already equals
    // the generated name renames onto itself (success no-op), and an
    // existing empty gallery at the generated name is replaced by POSIX
    // rename. The detached path then still names the public gallery, so the
    // post-publication cleanup can remove a new owner's files.

    /// A gallery whose legal title is exactly an old generated detach
    /// candidate: after commit/detach but before cleanup, pause, delete and
    /// recreate the same guid with a sentinel; the released cleanup must not
    /// touch the new gallery.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn detached_collision_cannot_rebind_or_remove_a_new_public_gallery() {
        let root = tempfile::tempdir().unwrap();
        let mut config = m4_config(root.path(), 1, 1);
        config.make_archive = true;
        let mut app = m4_app(root.path(), config);
        let url = "http://e-hentai.org/g/123/abcdef0123/";
        let guid = m4_task(
            &mut app,
            root.path(),
            url,
            serde_json::json!({"scan_thread_cnt": 1, "download_thread_cnt": 1, "make_archive": true}),
        );
        // A legal single-component title in the exact shape the old counter
        // scheme generates; the armed candidate forces that proposal.
        let title = format!(".xehentai-remove-{}-7", std::process::id());
        let gallery = root.path().join("downloads").join(&title);
        ready_archive_task(&mut app, &guid, &title, &gallery, 1, 1);
        std::fs::write(gallery.join("1.jpg"), b"old-bytes").unwrap();
        let zip = root.path().join("downloads").join(format!("{title}.zip"));
        arm_detach_candidate(&root.path().join("downloads"), &title);
        let cleanup = arm_archive_cleanup_gate(&zip.to_string_lossy());
        let local = LocalSet::new();
        local
            .run_until(async move {
                let runtime = Runtime::new(app, Path::new(MANIFEST).to_path_buf());
                let (run, handle) = runtime.spawn();
                let deadline = std::time::Instant::now() + Duration::from_secs(20);
                while cleanup.started.try_recv().is_err() {
                    if std::time::Instant::now() > deadline {
                        panic!("archive never published: {:?}", handle.trace());
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                // The already-published ZIP is consistent before anything else.
                {
                    let mut archive =
                        zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
                    assert_eq!(archive.len(), 1);
                    let mut entry = archive.by_index(0).unwrap();
                    assert_eq!(entry.name(), "1.jpg");
                    let mut bytes = Vec::new();
                    std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
                    assert_eq!(bytes, b"old-bytes");
                }
                // Real pause/delete/add_task boundary while cleanup is held.
                handle.borrow_app_mut(|app| {
                    assert_eq!(app.pause_task(&guid).unwrap().0, consts::ERR_NO_ERROR);
                    assert_eq!(app.del_task(&guid).unwrap().0, consts::ERR_NO_ERROR);
                    let (code, returned) = app.add_task(url, serde_json::Map::new()).unwrap();
                    assert_eq!(code, consts::ERR_NO_ERROR);
                    assert_eq!(returned.as_deref(), Some(guid.as_str()));
                    // The replacement run owns the same public gallery name
                    // but never archives; it must finish on its own.
                    let task = app.task_mut(&guid).unwrap();
                    task.config.make_archive = false;
                    task.state = consts::TASK_STATE_DOWNLOAD;
                    task.meta.insert("title".into(), serde_json::json!(title));
                    task.meta.insert("total".into(), serde_json::json!(1));
                    task.meta.insert("finished".into(), serde_json::json!(1));
                    task.meta
                        .insert("thumbnail_cnt".into(), serde_json::json!(20));
                    task.img_q = Some(VecDeque::new());
                    task.page_q = Some(VecDeque::new());
                });
                std::fs::create_dir_all(&gallery).unwrap();
                std::fs::write(gallery.join("1.jpg"), b"new-bytes").unwrap();
                std::fs::write(gallery.join("new-only.txt"), b"only-new-run").unwrap();
                let _ = cleanup.release.send(());
                wait_state(&handle, &guid, consts::TASK_STATE_FINISHED).await;
                let _ = handle.shutdown().await;
                run.await.unwrap();
                assert_eq!(
                    std::fs::read(gallery.join("1.jpg")).unwrap(),
                    b"new-bytes",
                    "cleanup removed the new owner's gallery through the rebound name"
                );
                assert_eq!(
                    std::fs::read(gallery.join("new-only.txt")).unwrap(),
                    b"only-new-run"
                );
                assert_eq!(handle.task_state(&guid), Some(consts::TASK_STATE_FINISHED));
                // The already-published ZIP stays consistent.
                {
                    let archive = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
                    assert_eq!(archive.len(), 1);
                }
                // The recreated public gallery and its already-published ZIP
                // keep their (old-pattern) names by design; everything else
                // private must be gone.
                let mut leftovers = leftover_private_entries(&root.path().join("downloads"));
                let zip_name = format!("{title}.zip");
                leftovers.retain(|name| name != &title && name != &zip_name);
                assert!(leftovers.is_empty(), "{leftovers:?}");
                assert_eq!(handle.slot_count(consts::TASK_STATE_MAKE_ARCHIVE), 0);
            })
            .await;
    }

    /// An existing other empty gallery at the proposed detach target must
    /// survive: rename may not replace it, and the archive still completes.
    #[test]
    fn detached_reservation_never_replaces_an_existing_empty_gallery() {
        let root = tempfile::tempdir().expect("root");
        let task = prepared_task(root.path(), "guid0001", "victim", 1, |_| {});
        let gallery = task.get_fpath().unwrap();
        std::fs::create_dir_all(&gallery).unwrap();
        std::fs::write(gallery.join("1.jpg"), b"victim-bytes").unwrap();
        let downloads = root.path().join("downloads");
        // Another ordinary empty gallery sits exactly where detach proposes
        // to move the victim. It stays empty: a sentinel inside would hide a
        // POSIX empty-directory replacement.
        let other = downloads.join(format!(".xehentai-detached-{}-9", std::process::id()));
        std::fs::create_dir_all(&other).unwrap();
        arm_detach_candidate(&downloads, &other.file_name().unwrap().to_string_lossy());
        let snapshot = task.archive_snapshot().unwrap();
        let zip = snapshot.arc.clone();
        let committed =
            commit_prepared(prepare_archive_snapshot(&snapshot, true).unwrap()).unwrap();
        assert!(zip.exists(), "the archive is published");
        assert_eq!(
            std::fs::read_dir(&other).unwrap().count(),
            0,
            "the existing empty gallery was replaced by the detach rename"
        );
        assert!(other.is_dir());
        if let Some(detached) = committed.detached {
            remove_detached(&detached).unwrap();
        }
        assert_eq!(
            std::fs::read_dir(&other).unwrap().count(),
            0,
            "cleanup touched the unrelated gallery"
        );
        let other_name = other.file_name().unwrap().to_string_lossy().into_owned();
        let mut archive = zip::ZipArchive::new(std::fs::File::open(&zip).unwrap()).unwrap();
        assert_eq!(archive.len(), 1);
        let mut entry = archive.by_index(0).unwrap();
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut entry, &mut bytes).unwrap();
        assert_eq!(bytes, b"victim-bytes");
        // The pre-existing ordinary gallery keeps its name by design; every
        // other private entry must be gone, so a leaked container would fail
        // here.
        let mut leftovers = leftover_private_entries(&downloads);
        leftovers.retain(|name| name != &other_name);
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    /// A new task admitted while a detached payload is owned cannot rebind
    /// it: while the container is owned, any public gallery, writable
    /// cfg.dir, title or canonical alias resolving to the container or its
    /// descendants is rejected before any directory/stage/file write. After
    /// retirement the exact legal name admits normally.
    #[test]
    fn active_detached_payload_cannot_be_rebound_by_task_admission() {
        let root = tempfile::tempdir().expect("root");
        let task = prepared_task(root.path(), "guid0001", "victim", 1, |_| {});
        let gallery = task.get_fpath().unwrap();
        std::fs::create_dir_all(&gallery).unwrap();
        std::fs::write(gallery.join("1.jpg"), b"victim-bytes").unwrap();
        let downloads = root.path().join("downloads");
        let snapshot = task.archive_snapshot().unwrap();
        let committed =
            commit_prepared(prepare_archive_snapshot(&snapshot, true).unwrap()).unwrap();
        assert!(committed.detached.is_some(), "gallery detached");
        assert!(!gallery.exists(), "the public gallery path is moved aside");
        // The owned payload lives in exactly one freshly reserved exclusive
        // container; the child holding the images sits at depth two.
        let mut owned = Vec::new();
        for entry in std::fs::read_dir(&downloads).unwrap().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with(".xehentai-detached-") {
                owned.push(downloads.join(name));
            }
        }
        assert_eq!(owned.len(), 1, "exactly one exclusively owned container");
        let container = owned.pop().unwrap();
        let child = container.join("gallery");
        assert!(child.strip_prefix(&downloads).unwrap().components().count() >= 2);
        assert_eq!(std::fs::read(child.join("1.jpg")).unwrap(), b"victim-bytes");
        let container_title = container
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let probe = "http://127.0.0.1:9/img/0123456789abcdef0123456789abcdef01234567-10-20-0-jpg/keystamp=x";
        // While the container is owned, a rival gallery resolving to the
        // container root must be rejected before ANY write. A genuinely
        // committed rival file would otherwise be deleted by the
        // whole-container cleanup.
        let mut rival = prepared_task(root.path(), "guid0002", &container_title, 1, |_| {});
        rival.reload_map.insert(
            "0123456789".into(),
            (
                "http://e-hentai.org/s/0123456789/123456-1".into(),
                "one.jpg".into(),
            ),
        );
        match rival.get_fpath() {
            Err(crate::task::TaskError::UnsafePath { reason, .. }) => {
                assert!(
                    reason.contains("still owned"),
                    "unexpected rejection reason: {reason}"
                );
            }
            Ok(path) => {
                assert_eq!(
                    path, container,
                    "colliding title resolves to container root"
                );
                match rival.begin_staged_save(probe, probe) {
                    Err(crate::task::TaskError::UnsafePath { reason, .. }) => {
                        assert!(
                            reason.contains("still owned"),
                            "unexpected rejection reason: {reason}"
                        );
                    }
                    Ok(_) => panic!(
                        "rival admission at the owned container root must be rejected \
                         before any write; a committed file would be deleted by \
                         whole-container cleanup"
                    ),
                    Err(error) => panic!("unexpected rival stage error: {error}"),
                }
            }
            Err(error) => panic!("unexpected admission error: {error}"),
        }
        // No rival write may have entered the owned container.
        assert_eq!(
            std::fs::read(child.join("1.jpg")).unwrap(),
            b"victim-bytes",
            "the owned payload was modified through admission"
        );
        let mut names: Vec<String> = std::fs::read_dir(&container)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec!["gallery".to_string()],
            "rival admission wrote into the owned container: {names:?}"
        );
        // A writable cfg.dir pointing directly at the owned container (hence
        // at the owned child `gallery`) is the same namespace admission and
        // must also be rejected before any write.
        {
            let mut cfg = m4_config(root.path(), 1, 1);
            cfg.dir = container.to_string_lossy().into_owned();
            let task_config = TaskConfig::from_app(&cfg, &serde_json::Map::new()).unwrap();
            match Task::new(
                "http://e-hentai.org/g/123/abcdef0123/",
                task_config,
                "guid-cfg".into(),
                root.path(),
            ) {
                Err(crate::task::TaskError::UnsafePath { reason, .. }) => {
                    assert!(
                        reason.contains("still owned"),
                        "unexpected cfg.dir rejection reason: {reason}"
                    );
                }
                Ok(other) => panic!(
                    "Task with cfg.dir inside the owned container must be rejected, got {}",
                    other.files().root().display()
                ),
                Err(error) => panic!("unexpected cfg.dir error: {error}"),
            }
        }
        // A canonical alias that collapses to the same container name via
        // legalpath must be rejected identically.
        {
            let alias = format!("{}/{}", &container_title[..1], &container_title[1..]);
            assert_eq!(
                crate::util::legalpath(&alias),
                container_title,
                "alias must collapse to the container name"
            );
            let mut alias_rival = prepared_task(root.path(), "guid-alias", &alias, 1, |_| {});
            alias_rival.reload_map.insert(
                "0123456789".into(),
                (
                    "http://e-hentai.org/s/0123456789/123456-1".into(),
                    "one.jpg".into(),
                ),
            );
            match alias_rival.get_fpath() {
                Err(crate::task::TaskError::UnsafePath { .. }) => {}
                Ok(path) => {
                    assert_eq!(path, container);
                    match alias_rival.begin_staged_save(probe, probe) {
                        Err(crate::task::TaskError::UnsafePath { .. }) => {}
                        Ok(_) => panic!("canonical alias admission must be rejected"),
                        Err(error) => panic!("unexpected alias stage error: {error}"),
                    }
                }
                Err(error) => panic!("unexpected alias admission error: {error}"),
            }
        }
        // Explicit retirement: the old cleanup consumes only the owned payload.
        if let Some(detached) = committed.detached {
            remove_detached(&detached).unwrap();
        }
        assert!(
            leftover_private_entries(&downloads).is_empty(),
            "owned container must be gone after retirement"
        );
        // After retirement the exact legal name admits normally with a real
        // committed file, proving there is no blanket prefix ban.
        {
            let mut plain = prepared_task(root.path(), "guid0003", &container_title, 1, |_| {});
            plain.reload_map.insert(
                "0123456789".into(),
                (
                    "http://e-hentai.org/s/0123456789/123456-1".into(),
                    "one.jpg".into(),
                ),
            );
            let fpath = plain
                .get_fpath()
                .expect("exact legal name must admit after retirement");
            assert_eq!(fpath, downloads.join(&container_title));
            match plain.begin_staged_save(probe, probe) {
                Ok(SaveStart::Staged(staged)) => {
                    std::fs::write(&staged.stage, b"post-bytes").unwrap();
                    let dest = plain.commit_staged_save(staged).unwrap();
                    assert_eq!(std::fs::read(&dest).unwrap(), b"post-bytes");
                }
                Ok(SaveStart::AlreadyDone(_)) => panic!("fresh post-retirement task must stage"),
                Err(error) => panic!("post-retirement admission must succeed, got {error}"),
            }
            std::fs::remove_dir_all(&fpath).unwrap();
            assert!(
                leftover_private_entries(&downloads).is_empty(),
                "post-retirement gallery cleaned"
            );
        }
        // Ordinary titles still admit normally.
        let mut plain = prepared_task(root.path(), "guid0004", "ordinary", 1, |_| {});
        plain.reload_map.insert(
            "0123456789".into(),
            (
                "http://e-hentai.org/s/0123456789/123456-1".into(),
                "one.jpg".into(),
            ),
        );
        let fpath = plain.get_fpath().unwrap();
        std::fs::create_dir_all(&fpath).unwrap();
        match plain.begin_staged_save(probe, probe) {
            Ok(SaveStart::Staged(staged)) => crate::task::Task::abort_staged_save(staged),
            Ok(SaveStart::AlreadyDone(_)) => panic!("fresh task must stage"),
            Err(error) => panic!("unexpected admission error: {error}"),
        }
    }

    /// Publication failure restores the original gallery: images are intact,
    /// only the attempt's staging file is removed, and nothing detached is
    /// left behind.
    #[test]
    fn publish_failure_restores_the_original_gallery() {
        let root = tempfile::tempdir().expect("root");
        let task = prepared_task(root.path(), "guid0001", "victim", 1, |_| {});
        let gallery = task.get_fpath().unwrap();
        std::fs::create_dir_all(&gallery).unwrap();
        std::fs::write(gallery.join("1.jpg"), b"victim-bytes").unwrap();
        let snapshot = task.archive_snapshot().unwrap();
        let prepared = prepare_archive_snapshot(&snapshot, true).unwrap();
        // Force the bounded publish rename to fail with a real directory at
        // the archive path, after the container was built.
        std::fs::create_dir_all(&snapshot.arc).unwrap();
        let error = match commit_prepared(prepared) {
            Ok(_) => panic!("publish must fail"),
            Err(error) => error,
        };
        assert!(
            matches!(error, crate::task::TaskError::PublishFailed { .. }),
            "unexpected error: {error}"
        );
        assert_eq!(
            std::fs::read(gallery.join("1.jpg")).unwrap(),
            b"victim-bytes",
            "the gallery was not restored after publish failure"
        );
        assert!(!snapshot.arc.is_file());
        std::fs::remove_dir_all(&snapshot.arc).unwrap();
        assert!(leftover_private_entries(&root.path().join("downloads")).is_empty());
    }

    /// Rollback failure retains the recoverable images at the owned path and
    /// reports that location: when the public path cannot take the gallery
    /// back, the owned container is kept, never dropped with its only copy.
    #[test]
    fn rollback_failure_retains_images_at_the_owned_path() {
        let root = tempfile::tempdir().expect("root");
        let task = prepared_task(root.path(), "guid0001", "victim", 1, |_| {});
        let gallery = task.get_fpath().unwrap();
        std::fs::create_dir_all(&gallery).unwrap();
        std::fs::write(gallery.join("1.jpg"), b"victim-bytes").unwrap();
        let detached = detach_gallery(&gallery)
            .expect("detach")
            .expect("gallery detached");
        // The public path is rebound before the restore: a real file there
        // makes the rename-back fail deterministically.
        std::fs::write(&gallery, b"squatter").unwrap();
        let error = match detached.restore() {
            Ok(()) => panic!("restore must fail"),
            Err(error) => error,
        };
        let owned = match &error {
            crate::task::TaskError::RollbackFailed { owned, .. } => owned.clone(),
            _ => panic!("unexpected error: {error}"),
        };
        assert_eq!(
            std::fs::read(owned.join("1.jpg")).unwrap(),
            b"victim-bytes",
            "the only image copy was dropped with the rollback"
        );
        assert!(
            format!("{error}").contains(&owned.to_string_lossy().into_owned()),
            "the error must report the actual retained location: {error}"
        );
        assert_eq!(std::fs::read(&gallery).unwrap(), b"squatter");
    }
}
fn compat_rpc_app(root: &std::path::Path, inputs: &serde_json::Value) -> crate::core::App {
    use crate::config::Config;
    use crate::task::{FidKey, Task, TaskConfig};
    let mut config = Config::default();
    config.dir = root.join("downloads").to_string_lossy().into_owned();
    std::fs::create_dir_all(root.join("downloads")).unwrap();
    let logger = crate::util::logger::Logger::with_output(Box::new(Vec::new()), false);
    let mut app = crate::core::App::new(config, logger, root, root).expect("rpc probe app");
    for task_value in inputs["tasks"].as_array().cloned().unwrap_or_default() {
        let guid = task_value["guid"]
            .as_str()
            .unwrap_or("probe-guid")
            .to_string();
        let task_config =
            TaskConfig::from_app(&app.cfg, &serde_json::Map::new()).expect("task config");
        let mut task = Task::new(
            task_value["url"]
                .as_str()
                .unwrap_or("http://e-hentai.org/g/123/abcdef0123/"),
            task_config,
            guid.clone(),
            root,
        )
        .expect("probe task");
        if let Some(meta) = task_value.get("meta").and_then(|meta| meta.as_object()) {
            for (key, value) in meta {
                task.meta.insert(key.clone(), value.clone());
            }
        }
        let meta_title = task_value
            .get("meta")
            .and_then(|meta| meta.get("title"))
            .and_then(|title| title.as_str())
            .unwrap_or("probe-gallery");
        task.meta
            .insert("title".to_string(), serde_json::json!(meta_title));
        let dir = task.get_fpath().expect("probe fpath");
        std::fs::create_dir_all(&dir).unwrap();
        if let Some(files) = task_value.get("files").and_then(|files| files.as_object()) {
            for (name, content) in files {
                // Fixtures transmit raw image bytes as latin-1 code points;
                // restore bytes instead of writing UTF-8 text.
                let text = content.as_str().unwrap_or("");
                let raw: Vec<u8> = text.chars().map(|ch| u32::from(ch) as u8).collect();
                std::fs::write(dir.join(name), raw).unwrap();
            }
        }
        // Differential alias fixtures: real subdirectories, legal symlinks and
        // a pre-built sibling ZIP with exact bytes. `{root}` is this side's own
        // work root, so the absolute aliases are legal everywhere.
        if let Some(subdirs) = task_value
            .get("subdirs")
            .and_then(|value| value.as_object())
        {
            for (name, files) in subdirs {
                let full = dir.join(name);
                std::fs::create_dir_all(&full).unwrap();
                if let Some(files) = files.as_object() {
                    for (file, content) in files {
                        let text = content.as_str().unwrap_or("");
                        let raw: Vec<u8> = text.chars().map(|ch| u32::from(ch) as u8).collect();
                        std::fs::write(full.join(file), raw).unwrap();
                    }
                }
            }
        }
        #[cfg(unix)]
        if let Some(links) = task_value
            .get("symlinks")
            .and_then(|value| value.as_array())
        {
            let side_root = root.to_string_lossy();
            for link in links {
                let target = link["target"]
                    .as_str()
                    .unwrap_or("")
                    .replace("{root}", &side_root);
                let name = link["name"].as_str().unwrap_or("");
                std::os::unix::fs::symlink(target, dir.join(name)).unwrap();
            }
        }
        if let Some(entries) = task_value
            .get("zip_entries")
            .and_then(|value| value.as_object())
        {
            use std::io::Write as _;
            let mut zip_name = dir.file_name().unwrap().to_os_string();
            zip_name.push(".zip");
            let file = std::fs::File::create(dir.with_file_name(zip_name)).unwrap();
            let mut writer = zip::ZipWriter::new(file);
            let options = zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, content) in entries {
                let text = content.as_str().unwrap_or("");
                let raw: Vec<u8> = text.chars().map(|ch| u32::from(ch) as u8).collect();
                writer.start_file(name, options).unwrap();
                writer.write_all(&raw).unwrap();
            }
            writer.finish().unwrap();
        }
        if let Some(renamed) = task_value
            .get("renamed")
            .and_then(|value| value.as_object())
        {
            for (fid, name) in renamed {
                // Match the reference/Python oracle: numeric fids are Int
                // keys (JSON numbers), not Text. Preserve Text only for
                // genuinely non-numeric fids.
                let key = fid
                    .parse::<i64>()
                    .map(FidKey::Int)
                    .unwrap_or_else(|_| FidKey::Text(fid.clone()));
                task.renamed_map
                    .insert(key, name.as_str().unwrap_or("").to_string());
            }
        }
        if let Some(state) = task_value.get("state").and_then(|value| value.as_i64()) {
            task.state = state;
        }
        app.insert_task_for_test(task);
        // The oracle registers fixtures without queueing them; drain the
        // test-insert queue so both sides start unqueued. last_task_guid is
        // not observable on any RPC method.
        let mut _drain_current = None;
        while app.pop_queued_task(&mut _drain_current).is_some() {}
        if task_value
            .get("zip_build")
            .and_then(|value| value.as_bool())
            .unwrap_or(false)
        {
            let built = crate::rpc::read_archive(&app, &guid).expect("probe zip build");
            assert!(built.starts_with(b"PK"));
            if let Some(remove) = task_value
                .get("zip_remove")
                .and_then(|value| value.as_array())
            {
                let dir = app.task(&guid).expect("task").get_fpath().expect("fpath");
                for name in remove {
                    if let Some(name) = name.as_str() {
                        let _ = std::fs::remove_file(dir.join(name));
                    }
                }
            }
        }
    }
    app
}

fn latin1(bytes: &[u8]) -> String {
    // The oracle probe decodes response bytes as latin-1; mirror that so
    // arbitrary gallery/archive bytes survive the JSON transcript exactly.
    bytes.iter().map(|&byte| byte as char).collect()
}

/// Real HTTP fixture backend for the browser evidence run. Inert in normal
/// test runs; started explicitly with `--ignored` and XEH_BROWSER_REQUEST.
/// Serves the compiled RSS dispatcher, the real App and the frozen UI on a
/// real TCP listener. Tasks are seeded through the same compat fixture seam
/// the differential suites use, so no network fetch is attempted: this
/// process intentionally runs no task loop.
#[test]
#[ignore = "browser evidence harness, started explicitly"]
fn browser_fixture_server() {
    use crate::core::Runtime;
    use crate::rpc::server::{SharedRpc, bind, serve_until_closed};
    use std::sync::Arc;

    let request_path = std::env::var("XEH_BROWSER_REQUEST").expect("XEH_BROWSER_REQUEST");
    let request: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&request_path).expect("request")).expect("json");
    let root = std::path::PathBuf::from(request["root"].as_str().expect("root"));
    let resource_root =
        std::path::PathBuf::from(request["resource_root"].as_str().expect("resource_root"));
    let ready = std::path::PathBuf::from(request["ready"].as_str().expect("ready"));
    let shutdown = std::path::PathBuf::from(request["shutdown"].as_str().expect("shutdown"));
    let port = request["port"].as_u64().unwrap_or(8010) as u16;
    let secret = request["secret"].as_str().map(str::to_string);
    let app = compat_rpc_app(&root, &request);
    let runtime = Runtime::new(app, resource_root.clone());
    let handle = runtime.handle();
    let local = tokio::task::LocalSet::new();
    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    tokio_runtime.block_on(local.run_until(async move {
        let listener = bind("127.0.0.1", port).await.expect("fixture bind");
        let actual = listener.local_addr().expect("addr").port();
        fs::write(
            &ready,
            serde_json::json!({"port": actual, "pid": std::process::id()}).to_string(),
        )
        .expect("ready");
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let origin = "http://127.0.0.1:9".to_string();
        let shared = std::rc::Rc::new(std::cell::RefCell::new(
            SharedRpc::new(
                &resource_root,
                &root,
                Arc::new(crate::worker::RealClock),
                secret,
                origin.clone(),
            )
            .expect("shared rpc"),
        ));
        let server = serve_until_closed(listener, shutdown_rx, handle, shared, origin);
        tokio::pin!(server);
        loop {
            tokio::select! {
                result = &mut server => {
                    result.expect("fixture server");
                    break;
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(200)) => {
                    if shutdown.exists() {
                        let _ = shutdown_tx.send(true);
                    }
                }
            }
        }
    }));
}

fn compat_rpc(inputs: &serde_json::Value) -> serde_json::Value {
    use crate::rpc::server::{SharedRpc, serve_one};
    let root = std::path::PathBuf::from(inputs["root"].as_str().expect("root"));
    let secret = inputs["secret"].as_str().map(str::to_string);
    let app = compat_rpc_app(&root, inputs);
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let shared = std::rc::Rc::new(std::cell::RefCell::new(
        SharedRpc::new(
            manifest,
            &root,
            std::sync::Arc::new(crate::worker::RealClock),
            secret,
            "http://127.0.0.1:9".to_string(),
        )
        .expect("shared"),
    ));
    let runtime = crate::core::Runtime::new(app, manifest.to_path_buf());
    let handle = runtime.handle();
    let local = tokio::task::LocalSet::new();
    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("probe runtime");
    fn snapshot(handle: &crate::core::runtime::AppHandle) -> serde_json::Value {
        handle.borrow_app(thin_snapshot)
    }
    /// Full isolated-update snapshot: the real public-config view plus pool
    /// members, task queue, cookies/login, cfg dir and the directory tree.
    /// Used only for the independent proxy-nodir repairs, never for the
    /// shared sequential transcript.
    fn snapshot_full(
        handle: &crate::core::runtime::AppHandle,
        shared: &std::rc::Rc<std::cell::RefCell<SharedRpc>>,
        root: &std::path::Path,
    ) -> serde_json::Value {
        handle.borrow_app_mut(|app| {
            let mut thin = thin_snapshot(app);
            let cfg_view = {
                let mut shared_ref = shared.borrow_mut();
                let req = crate::rpc::RpcRequest {
                    method_raw: "xeH.getConfig".to_string(),
                    id: serde_json::Value::Null,
                    args: Vec::new(),
                    kwargs: serde_json::Map::new(),
                };
                // The real public view, not a second mapping.
                match crate::rpc::execute(app, &mut shared_ref.rpc, &req, None, None) {
                    outcome if outcome.code == 0 => {
                        outcome.result.unwrap_or(serde_json::Value::Null)
                    }
                    outcome => serde_json::json!({"error": outcome.code}),
                }
            };
            let proxy_members: Vec<String> = app
                .proxy_pool()
                .map(|pool| pool.lock().expect("pool").proxies.keys().cloned().collect())
                .unwrap_or_default();
            let mut dirs = Vec::new();
            let mut stack = vec![root.to_path_buf()];
            while let Some(dir) = stack.pop() {
                let entries =
                    std::fs::read_dir(&dir).unwrap_or_else(|_| panic!("list {}", dir.display()));
                let mut names: Vec<std::path::PathBuf> = entries
                    .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                    .collect();
                names.sort();
                for path in names {
                    let rel = path
                        .strip_prefix(root)
                        .expect("rel")
                        .to_string_lossy()
                        .replace('\\', "/");
                    // Harness isolation dirs (oracle subprocess cwd); the
                    // candidate probe never creates them.
                    if !rel.contains('/') && ["home", "cache", "tmp"].contains(&rel.as_str()) {
                        continue;
                    }
                    if path.is_dir() && !path.is_symlink() {
                        dirs.push(format!("{rel}/"));
                        stack.push(path);
                    } else if path.is_symlink() {
                        let target = std::fs::read_link(&path)
                            .map(|target| target.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        dirs.push(format!("{rel} -> {target}"));
                    } else {
                        let name = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("");
                        if ["h.json", "eh.log", "webui.gz", "webui-cache.json"].contains(&name) {
                            continue;
                        }
                        dirs.push(rel);
                    }
                }
            }
            dirs.sort();
            let mut cookie_pairs: Vec<serde_json::Value> = thin
                .get("cookies")
                .and_then(|cookies| cookies.as_array())
                .cloned()
                .unwrap_or_default();
            cookie_pairs.sort_by_key(|left| left.to_string());
            let object = thin.as_object_mut().expect("snapshot object");
            object.insert(
                "cookies".to_string(),
                serde_json::Value::Array(cookie_pairs),
            );
            object.insert("cfg".to_string(), cfg_view);
            object.insert(
                "proxy_members".to_string(),
                serde_json::Value::Array(
                    proxy_members
                        .into_iter()
                        .map(serde_json::Value::String)
                        .collect(),
                ),
            );
            object.insert("dirs".to_string(), serde_json::json!(dirs));
            thin
        })
    }
    fn thin_snapshot(app: &crate::core::App) -> serde_json::Value {
        {
            let mut states = serde_json::Map::new();
            for guid in app.all_task_guids() {
                if let Some(task) = app.task(&guid) {
                    states.insert(guid, serde_json::json!(task.state));
                }
            }
            serde_json::json!({
                "cfg_dir": app.cfg.dir,
                "proxy_count": app.proxy_count(),
                "states": states,
                "queue_len": app.queue_len(),
                "has_login": app.has_login(),
                "cookies": app.cookies().to_vec(),
            })
        }
    }
    let full = inputs["snapshot"]
        .as_str()
        .is_some_and(|mode| mode == "full");
    let mut transcripts = Vec::new();
    for case in inputs["cases"].as_array().cloned().unwrap_or_default() {
        let id = case["id"].as_str().unwrap_or("?").to_string();
        let head = if let Some(raw) = case["raw_head"].as_str() {
            raw.to_string()
        } else {
            let target = case["target"].as_str().unwrap_or("/jsonrpc");
            let body = case["body"].as_str().unwrap_or("");
            format!(
                "{} {} HTTP/1.0\r\nHost: x\r\nContent-Length: {}\r\n\r\n{}",
                case["method"].as_str().unwrap_or("POST"),
                target,
                body.len(),
                body
            )
        };
        let transcript = tokio_runtime.block_on(local.run_until(async {
            let (mut client, mut server) = tokio::io::duplex(256 * 1024);
            {
                use tokio::io::AsyncWriteExt;
                client.write_all(head.as_bytes()).await.expect("write");
            }
            let before = if full {
                snapshot_full(&handle, &shared, &root)
            } else {
                snapshot(&handle)
            };
            let result = serve_one(
                &mut server,
                handle.clone(),
                std::rc::Rc::clone(&shared),
                "http://127.0.0.1:9".to_string(),
            )
            .await;
            let after = if full {
                snapshot_full(&handle, &shared, &root)
            } else {
                snapshot(&handle)
            };
            drop(server);
            let mut raw = Vec::new();
            {
                use tokio::io::AsyncReadExt;
                client.read_to_end(&mut raw).await.expect("read");
            }
            match result {
                Err(error) => serde_json::json!({"id": id, "error": error.to_string()}),
                Ok(()) => match raw.windows(4).position(|window| window == b"\r\n\r\n") {
                    None => serde_json::json!({
                        "id": id, "dropped": true,
                        "before": before, "after": after,
                    }),
                    Some(end) => {
                        let head_text = String::from_utf8_lossy(&raw[..end]).into_owned();
                        let mut lines = head_text.split("\r\n");
                        let status = lines.next().unwrap_or("").to_string();
                        let mut headers = Vec::new();
                        for line in lines {
                            if let Some((name, value)) = line.split_once(':') {
                                headers.push(vec![
                                    name.trim().to_lowercase(),
                                    value.trim().to_string(),
                                ]);
                            }
                        }
                        let declared = headers
                            .iter()
                            .find(|entry| entry[0] == "content-length")
                            .and_then(|entry| entry[1].parse::<usize>().ok())
                            .unwrap_or(0);
                        let rest = &raw[end + 4..];
                        let body_text = latin1(&rest[..declared.min(rest.len())]);
                        let trailing_newline =
                            rest.get(declared).is_some_and(|byte| *byte == b'\n');
                        serde_json::json!({
                            "id": id,
                            "status": status,
                            "headers": headers,
                            "body": body_text,
                            "trailing_newline": trailing_newline,
                            "before": before,
                            "after": after,
                        })
                    }
                },
            }
        }));
        transcripts.push(transcript);
    }
    serde_json::json!({
        "root": root.to_string_lossy(),
        "cases": transcripts,
    })
}

/// Test-only production error collection for readdir items: the SAME code
/// walk uses for genuine kernel errors. Injected iterator errors (not actual
/// kernel EIO) exercise it without a flaky filesystem.
#[cfg(test)]
fn record_readdir_item_results(
    parent_rel: &str,
    iter: impl Iterator<Item = Result<std::path::PathBuf, std::io::Error>>,
    out: &mut Vec<serde_json::Value>,
) -> Vec<std::path::PathBuf> {
    let mut indexed: Vec<(usize, Result<std::path::PathBuf, std::io::Error>)> = Vec::new();
    for (index, entry) in iter.enumerate() {
        match entry {
            Ok(path) => indexed.push((index, Ok(path))),
            Err(error) => {
                out.push(serde_json::json!({"path": format!("{parent_rel}/#{}", index), "type": "error", "error": error.kind().to_string(), "stage": "readdir"}));
            }
        }
    }
    let mut paths: Vec<std::path::PathBuf> = indexed
        .into_iter()
        .filter_map(|(_, result)| result.ok())
        .collect();
    paths.sort();
    paths
}

fn compat_gallery_dirs(root: &std::path::Path) -> Vec<serde_json::Value> {
    const TOP_FILE_EXCLUDED: [&str; 4] = ["h.json", "eh.log", "webui.gz", "webui-cache.json"];
    const TOP_DIR_EXCLUDED: [&str; 3] = ["home", "cache", "tmp"];
    /// Build a transport-relative path with `/` separators, preserving
    /// literal backslashes inside Unix components. Non-Unicode components
    /// are an explicit failed observation, never lossy conversion.
    fn rel_string(root: &std::path::Path, path: &std::path::Path) -> Result<String, String> {
        let rest = path
            .strip_prefix(root)
            .map_err(|_| "not-under-root".to_string())?;
        let mut parts = Vec::new();
        for component in rest.components() {
            match component {
                std::path::Component::Normal(os) => {
                    parts.push(
                        os.to_str()
                            .ok_or_else(|| "non-utf8-component".to_string())?
                            .to_string(),
                    );
                }
                std::path::Component::CurDir => parts.push(".".to_string()),
                std::path::Component::ParentDir => parts.push("..".to_string()),
                _ => return Err("unexpected-root-in-rel".to_string()),
            }
        }
        Ok(parts.join("/"))
    }
    fn file_name_string(path: &std::path::Path) -> Result<String, String> {
        path.file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.to_string())
            .ok_or_else(|| "non-utf8-filename".to_string())
    }
    fn map_target(root: &std::path::Path, target: &std::path::Path) -> Result<String, String> {
        // Preserve literal backslashes, `.`, `..`, relative/external tails.
        // Only a leading absolute FixtureROOT with a true component boundary
        // maps to `<ROOT>`; non-boundary substrings stay literal.
        let root_str = root.to_str().ok_or_else(|| "non-utf8-root".to_string())?;
        let target_str = target
            .to_str()
            .ok_or_else(|| "non-utf8-target".to_string())?;
        if target_str == root_str {
            return Ok("<ROOT>".to_string());
        }
        let prefix = format!("{root_str}/");
        if target_str.starts_with(&prefix) {
            return Ok(format!("<ROOT>{}", &target_str[root_str.len()..]));
        }
        Ok(target_str.to_string())
    }
    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut Vec<serde_json::Value>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) => {
                let rel = rel_string(root, dir).unwrap_or_else(|_| "<non-utf8-dir>".to_string());
                let path = if rel.is_empty() {
                    "./".to_string()
                } else {
                    format!("{rel}/")
                };
                out.push(serde_json::json!({"path": path, "type": "error", "error": error.kind().to_string()}));
                return;
            }
        };
        // Record per-item readdir errors with parent rel path and index;
        // never silently drop via filter_map. No leaf identity is fabricated.
        let parent_rel = rel_string(root, dir).unwrap_or_else(|_| "<non-utf8-dir>".to_string());
        let paths = record_readdir_item_results(
            parent_rel.as_str(),
            entries.map(|entry| entry.map(|entry| entry.path())),
            out,
        );
        for path in paths {
            let name = match file_name_string(&path) {
                Ok(name) => name,
                Err(error) => {
                    let rel =
                        rel_string(root, &path).unwrap_or_else(|_| "<non-utf8-path>".to_string());
                    out.push(serde_json::json!({"path": rel, "type": "error", "error": error}));
                    continue;
                }
            };
            let rel = match rel_string(root, &path) {
                Ok(rel) => rel,
                Err(error) => {
                    out.push(serde_json::json!({"path": "<non-utf8-path>", "type": "error", "error": error}));
                    continue;
                }
            };
            // Exact top-level transport artifacts only; nested same basenames observed.
            if !rel.contains('/') && TOP_DIR_EXCLUDED.contains(&rel.as_str()) {
                continue;
            }
            if !rel.contains('/') && TOP_FILE_EXCLUDED.contains(&name.as_str()) {
                continue;
            }
            let metadata = match std::fs::symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(error) => {
                    out.push(serde_json::json!({"path": rel, "type": "error", "error": error.kind().to_string()}));
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                // Raw link target via read_link (never a Path constructor that
                // collapses literal `.` text); component-boundary mapping only.
                // read_link/metadata/read failures are explicit failed
                // observations, never disappearance. An observer error fails
                // comparison even when both sides report the same error.
                let raw_target = match std::fs::read_link(&path) {
                    Ok(target) => target,
                    Err(error) => {
                        out.push(serde_json::json!({"path": rel, "type": "error", "error": error.kind().to_string()}));
                        continue;
                    }
                };
                let target = match map_target(root, &raw_target) {
                    Ok(mapped) => mapped,
                    Err(error) => {
                        out.push(serde_json::json!({"path": rel, "type": "error", "error": error}));
                        continue;
                    }
                };
                out.push(serde_json::json!({"path": rel, "type": "link", "target": target}));
            } else if metadata.is_dir() {
                out.push(serde_json::json!({"path": format!("{rel}/"), "type": "dir"}));
                walk(root, &path, out);
            } else if path.extension().and_then(|ext| ext.to_str()) == Some("zip") {
                match (|| -> Result<(Vec<String>, std::collections::BTreeMap<String, String>), String> {
                    let file = std::fs::File::open(&path).map_err(|error| error.kind().to_string())?;
                    let mut archive = zip::ZipArchive::new(file).map_err(|_| "bad_zip".to_string())?;
                    let mut names = Vec::new();
                    let mut contents = std::collections::BTreeMap::new();
                    for index in 0..archive.len() {
                        let mut entry = archive.by_index(index).map_err(|_| "bad_zip".to_string())?;
                        let name = entry.name().to_string();
                        let mut bytes = Vec::new();
                        use std::io::Read;
                        entry.read_to_end(&mut bytes).map_err(|error| error.kind().to_string())?;
                        contents.insert(name.clone(), bytes.iter().map(|&byte| byte as char).collect::<String>());
                        names.push(name);
                    }
                    names.sort();
                    Ok((names, contents))
                })() {
                    Ok((names, contents)) => {
                        out.push(serde_json::json!({"path": rel, "type": "zip", "names": names, "contents": contents}));
                    }
                    Err(error) => {
                        out.push(serde_json::json!({"path": rel, "type": "error", "error": error}));
                    }
                }
            } else {
                match std::fs::read(&path) {
                    Ok(bytes) => {
                        let text: String = bytes.iter().map(|&byte| byte as char).collect();
                        out.push(serde_json::json!({"path": rel, "type": "file", "bytes": text}));
                    }
                    Err(error) => {
                        out.push(serde_json::json!({"path": rel, "type": "error", "error": error.kind().to_string()}));
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort_by(|left, right| left["path"].as_str().cmp(&right["path"].as_str()));
    out
}

/// M5 FD R2 P3 disk: nested persistence basenames, same-name content,
/// ROOT-substring boundary and probe errors must be observable. A nested
/// `h.json` is business data (not a top-level transport artifact) and must
/// appear; same names with different bytes must differ; only a leading
/// absolute FixtureROOT with a component boundary may map to `<ROOT>`; a
/// probe failure must not silently vanish.
#[test]
fn gallery_disk_observes_nested_content_and_errors() {
    // Nested h.json must be observed (global basename filter hides it).
    let root = tempfile::tempdir().expect("root");
    let nested = root.path().join("downloads").join("nested");
    std::fs::create_dir_all(&nested).expect("nested");
    std::fs::write(nested.join("h.json"), b"NESTED-SENTINEL").expect("sentinel");
    let dirs = compat_gallery_dirs(root.path());
    assert!(
        dirs.iter().any(|entry| {
            entry["path"].as_str().unwrap_or("") == "downloads/nested/h.json"
                && entry["type"].as_str().unwrap_or("") == "file"
                && entry["bytes"].as_str().unwrap_or("") == "NESTED-SENTINEL"
        }),
        "nested h.json must be observed with bytes, got: {dirs:?}"
    );
    // Same name, different bytes must differ (names alone hide corruption).
    let root_a = tempfile::tempdir().expect("root-a");
    let root_b = tempfile::tempdir().expect("root-b");
    for (root, bytes) in [
        (root_a.path(), b"BYTES-A".as_slice()),
        (root_b.path(), b"BYTES-B".as_slice()),
    ] {
        let dir = root.join("downloads");
        std::fs::create_dir_all(&dir).expect("downloads");
        std::fs::write(dir.join("same.jpg"), bytes).expect("same name");
    }
    let dirs_a = compat_gallery_dirs(root_a.path());
    let dirs_b = compat_gallery_dirs(root_b.path());
    // Strip the mapped ROOT prefix for comparison: both have
    // downloads/same.jpg, but contents differ and must be observed.
    // Before GREEN (names only) they are equal; after GREEN (content) they differ.
    assert_ne!(
        dirs_a, dirs_b,
        "same filename with different bytes must be distinguished"
    );
    // Only leading FixtureROOT with boundary maps: a sibling prefix without
    // the trailing component boundary must stay literal.
    let root_c = tempfile::tempdir().expect("root-c");
    let gallery = root_c.path().join("downloads");
    std::fs::create_dir_all(&gallery).expect("gallery");
    let prefix = root_c.path().to_string_lossy().into_owned();
    // Target shares the prefix text but not a component boundary
    // (prefix + "-evil" instead of prefix + "/").
    let evil_target = format!("{prefix}-evil/file.jpg");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&evil_target, gallery.join("link.jpg")).expect("evil link");
    #[cfg(not(unix))]
    std::fs::write(gallery.join("link.jpg"), b"placeholder").expect("placeholder");
    let dirs_c = compat_gallery_dirs(root_c.path());
    let link_entry = dirs_c
        .iter()
        .find(|entry| entry["path"].as_str().unwrap_or("") == "downloads/link.jpg")
        .expect("link entry");
    #[cfg(unix)]
    {
        assert_eq!(
            link_entry["type"].as_str().unwrap_or(""),
            "link",
            "link must be typed, got: {link_entry:?}"
        );
        assert_eq!(
            link_entry["target"].as_str().unwrap_or(""),
            evil_target,
            "non-boundary ROOT substring must stay literal, got: {link_entry:?}"
        );
        assert!(
            !link_entry["target"]
                .as_str()
                .unwrap_or("")
                .contains("<ROOT>-evil"),
            "must not map a non-boundary prefix, got: {link_entry:?}"
        );
    }
    #[cfg(not(unix))]
    {
        let _ = &evil_target;
        assert_eq!(
            link_entry["type"].as_str().unwrap_or(""),
            "file",
            "non-Unix placeholder must be a regular file, got: {link_entry:?}"
        );
        assert_eq!(
            link_entry["bytes"].as_str().unwrap_or(""),
            "placeholder",
            "non-Unix placeholder bytes must be observed, got: {link_entry:?}"
        );
    }
    // Probe errors must be observable, not an empty inventory. A real
    // read_dir/metadata/read failure is a REQUIRED error observation;
    // `error OR inner exists` would hide a silently skipped failure.
    let root_d = tempfile::tempdir().expect("root-d");
    let locked = root_d.path().join("locked");
    std::fs::create_dir_all(&locked).expect("locked");
    std::fs::write(locked.join("inner.jpg"), b"inner").expect("inner");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("drop perms");
        // Privileged runners (root) can still list 000 dirs; perms do not
        // apply there. In that case skip the strict denial and rely on the
        // portable bad-zip genuine error below.
        let privileged = std::fs::read_dir(&locked).is_ok();
        let dirs_d = compat_gallery_dirs(root_d.path());
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("restore");
        if !privileged {
            assert!(
                dirs_d.iter().any(|entry| {
                    entry["path"].as_str().unwrap_or("").contains("locked")
                        && entry["type"].as_str().unwrap_or("") == "error"
                }),
                "a probe failure must be an explicit error observation, got: {dirs_d:?}"
            );
        }
    }
    // Genuine portable filesystem error: a corrupt sibling ZIP is an
    // explicit error observation, never disappearance. Works as root too.
    {
        let bad_zip = root_d.path().join("downloads-bad.zip");
        std::fs::create_dir_all(root_d.path()).expect("root");
        std::fs::write(&bad_zip, b"not a zip").expect("bad zip");
        let dirs_bad = compat_gallery_dirs(root_d.path());
        assert!(
            dirs_bad.iter().any(|entry| {
                entry["path"].as_str().unwrap_or("") == "downloads-bad.zip"
                    && entry["type"].as_str().unwrap_or("") == "error"
            }),
            "a corrupt ZIP must be an explicit error observation, got: {dirs_bad:?}"
        );
        let _ = std::fs::remove_file(&bad_zip);
    }
}

/// Lossless filesystem: literal backslashes stay distinct from nesting,
/// raw link targets preserve backslash/prefix-sibling/non-leading
/// substrings, and injected readdir item errors use the same production
/// collection as genuine kernel errors.
#[test]
fn gallery_disk_preserves_backslash_and_reports_injected_errors() {
    // Backslash filename vs genuine nested path remain distinct.
    let root = tempfile::tempdir().expect("root");
    let downloads = root.path().join("downloads");
    std::fs::create_dir_all(&downloads).expect("downloads");
    std::fs::write(downloads.join("a\\b.jpg"), b"BACKSLASH").expect("backslash file");
    std::fs::create_dir_all(downloads.join("a")).expect("nested dir");
    std::fs::write(downloads.join("a").join("b.jpg"), b"NESTED").expect("nested file");
    let dirs = compat_gallery_dirs(root.path());
    let backslash = dirs
        .iter()
        .find(|entry| entry["path"].as_str().unwrap_or("") == "downloads/a\\b.jpg");
    let nested = dirs
        .iter()
        .find(|entry| entry["path"].as_str().unwrap_or("") == "downloads/a/b.jpg");
    assert!(
        backslash.is_some() && nested.is_some(),
        "backslash file and nested path must both be observed distinctly, got: {dirs:?}"
    );
    assert_eq!(
        backslash.unwrap()["bytes"].as_str().unwrap_or(""),
        "BACKSLASH"
    );
    assert_eq!(nested.unwrap()["bytes"].as_str().unwrap_or(""), "NESTED");
    assert_ne!(
        backslash.unwrap(),
        nested.unwrap(),
        "backslash must not collapse to nesting"
    );
    // Raw link target backslash preserved (no global separator rewrite).
    let root_b = tempfile::tempdir().expect("root-b");
    let gallery_b = root_b.path().join("downloads");
    std::fs::create_dir_all(&gallery_b).expect("gallery");
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink("a\\b-target.jpg", gallery_b.join("link.jpg"))
            .expect("backslash link");
        let dirs_b = compat_gallery_dirs(root_b.path());
        let link = dirs_b
            .iter()
            .find(|entry| entry["path"].as_str().unwrap_or("") == "downloads/link.jpg")
            .expect("link entry");
        assert_eq!(link["type"].as_str().unwrap_or(""), "link");
        assert_eq!(
            link["target"].as_str().unwrap_or(""),
            "a\\b-target.jpg",
            "raw link target backslash must be preserved, got: {link:?}"
        );
    }
}

/// Injected readdir item errors use the SAME production collection as
/// genuine kernel errors (labeled injected, not actual EIO). A genuine
/// filesystem error control is kept separately (corrupt ZIP above).
#[test]
fn gallery_disk_injected_readdir_error_is_recorded() {
    let mut out = Vec::new();
    let good = std::path::PathBuf::from("/tmp/good.jpg");
    let injected: Vec<Result<std::path::PathBuf, std::io::Error>> = vec![
        Ok(good.clone()),
        Err(std::io::Error::other("injected-iterator-error")),
    ];
    // SAME production collection walk uses for genuine kernel errors.
    let paths = record_readdir_item_results("downloads", injected.into_iter(), &mut out);
    assert!(
        out.iter()
            .any(|entry| entry["type"].as_str().unwrap_or("") == "error"
                && entry["path"].as_str().unwrap_or("").contains("#1")),
        "injected readdir item error must be an explicit error observation, got: {out:?}"
    );
    assert_eq!(
        paths,
        vec![good],
        "only the good item survives; the error is recorded, not fabricated"
    );
    assert!(
        out.iter()
            .any(|entry| entry["stage"].as_str().unwrap_or("") == "readdir"),
        "injected error must carry the readdir stage, got: {out:?}"
    );
}

/// Gallery-only boundary normalization for `config.dir`: map ONLY the
/// controlled absolute fixture root with an exact component boundary,
/// preserve literal `..`, `.`, relative/external/root-substring tails.
fn gallery_norm_dir(value: &str, root: &std::path::Path) -> String {
    let root_str = root.to_string_lossy();
    if value == root_str {
        return "<ROOT>".to_string();
    }
    let prefix = format!("{root_str}/");
    if value.starts_with(&prefix) {
        return format!("<ROOT>{}", &value[root_str.len()..]);
    }
    value.to_string()
}

/// Independent full poststate: task/session/queue/cookie/header/login/disk.
///
/// Cookies/headers preserve actual insertion order (no sorting): business
/// order is observable and reorder mutations must be caught. Tasks are the
/// FULL ordered registry `[[guid, record], ...]` reusing the M2 record
/// shape (url/failcode/state/guid/config/meta/reload/duplicate/renamed/
/// img_q/page_q/flist plus gid/sethash), not a state/meta projection.
/// `config` is the actual raw task config with ONLY `dir` mapped through
/// the gallery boundary; every other value is preserved unchanged.
fn compat_gallery_poststate(
    app: &crate::core::App,
    root: &std::path::Path,
    _inputs: &serde_json::Value,
) -> serde_json::Value {
    let mut tasks = Vec::new();
    for guid in app.all_task_guids() {
        if let Some(task) = app.task(&guid) {
            let mut record = m2_task_obs(task, root);
            if let Some(config) = record
                .get_mut("config")
                .and_then(|value| value.as_object_mut())
            {
                let mapped = gallery_norm_dir(&task.config.dir, root);
                config.insert("dir".to_string(), serde_json::json!(mapped));
            }
            tasks.push(serde_json::json!([guid.clone(), record]));
        }
    }
    // Preserve insertion order: no sorting. `cookies` is Vec in registry
    // order, `headers` is Vec<Header> in registry order.
    let headers: Vec<serde_json::Value> = app
        .headers()
        .iter()
        .map(|header| serde_json::json!([header.name, header.value]))
        .collect();
    let cookies: Vec<serde_json::Value> = app
        .cookies()
        .iter()
        .map(|(name, value)| serde_json::json!([name, value]))
        .collect();
    serde_json::json!({
        "tasks": tasks,
        "session": {"has_login": app.has_login()},
        "queue": app.queue().iter().cloned().collect::<Vec<String>>(),
        "cookies": cookies,
        "headers": headers,
        "login": {"has_login": app.has_login(), "cookie_header": app.cookie_header()},
        "disk": compat_gallery_dirs(root),
    })
}

/// M5 FD R2 P3: the gallery poststate must preserve actual insertion order
/// and capture ALL App tasks, not just the input subset. Reverse-order
/// cookies via the real `set_cookie` production path plus an extra task
/// beyond `inputs` prove it: sorting or input-only capture hides them.
#[test]
fn gallery_poststate_preserves_order_and_all_tasks() {
    let root = tempfile::tempdir().expect("root");
    let mut app = test_app(root.path(), &["guid-order"]);
    // Production path, reverse alphabetical to catch sorting: insertion
    // order is nw, zebra, apple, mango; sorted would be apple, mango, nw, zebra.
    app.set_cookie("zebra=3; apple=1; mango=2");
    // Extra task beyond inputs via the real add_task entry.
    let (code, extra_guid) = app
        .add_task(
            "http://e-hentai.org/g/999/abcdef0123/",
            serde_json::Map::new(),
        )
        .expect("extra task");
    assert_eq!(code, 0);
    let extra_guid = extra_guid.expect("extra guid");
    let inputs = serde_json::json!({"tasks": [], "root": root.path().to_string_lossy()});
    let state = compat_gallery_poststate(&app, root.path(), &inputs);
    // Cookies must be insertion order, not sorted.
    let cookies: Vec<Vec<String>> =
        serde_json::from_value(state["cookies"].clone()).expect("cookies");
    let names: Vec<String> = cookies.iter().map(|pair| pair[0].clone()).collect();
    assert_eq!(
        names,
        vec![
            "nw".to_string(),
            "zebra".to_string(),
            "apple".to_string(),
            "mango".to_string()
        ],
        "cookies must preserve insertion order, not sorted"
    );
    // Headers must be insertion order: User-Agent, Accept-Charset, Accept,
    // Connection, then Cookie appended by set_cookie.
    let headers: Vec<Vec<serde_json::Value>> =
        serde_json::from_value(state["headers"].clone()).expect("headers");
    let header_names: Vec<String> = headers
        .iter()
        .map(|pair| pair[0].as_str().unwrap_or("").to_string())
        .collect();
    assert_eq!(
        header_names,
        vec![
            "User-Agent".to_string(),
            "Accept-Charset".to_string(),
            "Accept".to_string(),
            "Connection".to_string(),
            "Cookie".to_string()
        ],
        "headers must preserve insertion order, not sorted"
    );
    // Cookie header must be the ordered serialization.
    let cookie_header = state["login"]["cookie_header"].as_str().unwrap_or("");
    assert_eq!(cookie_header, "nw=1;zebra=3;apple=1;mango=2");
    // ALL tasks, including the extra beyond inputs, must be present as an
    // ordered registry [[guid, record], ...] with the full M2 record shape,
    // not a state/meta projection.
    let tasks = state["tasks"].as_array().expect("tasks ordered list");
    assert!(
        tasks
            .iter()
            .any(|pair| pair[0].as_str().unwrap_or("") == extra_guid),
        "poststate must capture ALL App tasks, not just inputs; missing {extra_guid}"
    );
    // Registry order is preserved: guids appear in all_task_guids order.
    let guids: Vec<String> = tasks
        .iter()
        .map(|pair| pair[0].as_str().unwrap_or("").to_string())
        .collect();
    let expected: Vec<String> = app.all_task_guids();
    assert_eq!(guids, expected, "tasks must preserve registry order");
    // Full record shape: every entry carries the M2 public fields and the
    // raw config with ONLY dir boundary-mapped.
    for pair in tasks {
        let guid = pair[0].as_str().expect("guid");
        let record = pair[1].as_object().expect("record");
        for field in [
            "url",
            "failcode",
            "state",
            "guid",
            "config",
            "meta",
            "reload",
            "duplicate",
            "renamed",
            "img_q",
            "page_q",
            "flist",
        ] {
            assert!(
                record.contains_key(field),
                "task {guid} missing required field {field}"
            );
        }
        assert_eq!(
            record["guid"].as_str().unwrap_or(""),
            guid,
            "guid must match registry key"
        );
    }
    // Queue must preserve order too.
    let queue: Vec<String> = serde_json::from_value(state["queue"].clone()).expect("queue");
    assert!(queue.contains(&extra_guid));
}

fn compat_gallery(inputs: &serde_json::Value) -> serde_json::Value {
    use crate::rpc::{read_archive, read_gallery_file, thumbnail};
    let root = std::path::PathBuf::from(inputs["root"].as_str().expect("root"));
    let mut app = compat_rpc_app(&root, inputs);
    // Same reverse-order cookie fixture as the oracle, via the real
    // production `set_cookie` path, preserving insertion order.
    app.set_cookie("zebra=3; apple=1; mango=2");
    let before = compat_gallery_poststate(&app, &root, inputs);
    let mut reads = Vec::new();
    for query in inputs["reads"].as_array().cloned().unwrap_or_default() {
        let guid = query["guid"].as_str().unwrap_or("");
        let fid = query["fid"].as_str().unwrap_or("1");
        match read_gallery_file(&app, guid, fid) {
            Err(fail) => reads.push(serde_json::json!({
                "guid": guid, "fid": fid, "status": fail.status(),
            })),
            Ok((bytes, mime)) => reads.push(serde_json::json!({
                "guid": guid, "fid": fid, "status": 200u16,
                "mime": mime, "bytes": latin1(&bytes),
            })),
        }
    }
    let mut thumbs = Vec::new();
    for query in inputs["thumbs"].as_array().cloned().unwrap_or_default() {
        let guid = query["guid"].as_str().unwrap_or("");
        let fid = query["fid"].as_str().unwrap_or("1");
        let sizes = match crate::rpc::parse_thumb_query(query["query"].as_str().unwrap_or("")) {
            Err(_) => {
                thumbs.push(serde_json::json!({
                    "guid": guid, "fid": fid, "query": query["query"].as_str().unwrap_or(""), "outcome": "bad_query",
                }));
                continue;
            }
            Ok(sizes) => sizes,
        };
        match read_gallery_file(&app, guid, fid) {
            Err(fail) => thumbs.push(serde_json::json!({
                "guid": guid, "fid": fid, "query": query["query"].as_str().unwrap_or(""), "outcome": "not_found", "status": fail.status(),
            })),
            Ok((bytes, mime)) => match thumbnail(&bytes, sizes.0, sizes.1) {
                Err(crate::rpc::ThumbError::Passthrough) => thumbs.push(serde_json::json!({
                    "guid": guid, "fid": fid, "query": query["query"].as_str().unwrap_or(""), "outcome": "passthrough",
                    "mime": mime, "bytes": latin1(&bytes),
                })),
                Err(crate::rpc::ThumbError::BadRequest(message)) => {
                    thumbs.push(serde_json::json!({
                        "guid": guid, "fid": fid, "query": query["query"].as_str().unwrap_or(""), "outcome": "bad_request", "message": message,
                    }))
                }
                Err(crate::rpc::ThumbError::Decode(message)) => thumbs.push(serde_json::json!({
                    "guid": guid, "fid": fid, "query": query["query"].as_str().unwrap_or(""), "outcome": "decode_error", "message": message,
                })),
                Ok((resized, mime)) => {
                    let (width, height) = match image::load_from_memory(&resized) {
                        Ok(image) => (image.width(), image.height()),
                        Err(_) => (0, 0),
                    };
                    thumbs.push(serde_json::json!({
                        "guid": guid, "fid": fid, "query": query["query"].as_str().unwrap_or(""), "outcome": "resized",
                        "mime": mime, "width": width, "height": height,
                    }))
                }
            },
        }
    }
    let mut archives = Vec::new();
    for query in inputs["archives"].as_array().cloned().unwrap_or_default() {
        let guid = query["guid"].as_str().unwrap_or("");
        match read_archive(&app, guid) {
            Err(fail) => archives.push(serde_json::json!({
                "guid": guid, "status": fail.status(),
            })),
            Ok(bytes) => {
                let cursor = std::io::Cursor::new(&bytes);
                match zip::ZipArchive::new(cursor) {
                    Err(_) => archives.push(serde_json::json!({
                        "guid": guid, "status": "bad_zip",
                    })),
                    Ok(mut archive) => {
                        let mut names = Vec::new();
                        let mut contents = serde_json::Map::new();
                        for index in 0..archive.len() {
                            if let Ok(mut entry) = archive.by_index(index) {
                                let name = entry.name().to_string();
                                let mut bytes = Vec::new();
                                use std::io::Read;
                                if entry.read_to_end(&mut bytes).is_ok() {
                                    contents.insert(
                                        name.clone(),
                                        serde_json::Value::String(latin1(&bytes)),
                                    );
                                }
                                names.push(name);
                            }
                        }
                        names.sort();
                        archives.push(serde_json::json!({
                            "guid": guid, "status": 200u16,
                            "names": names, "contents": contents,
                            "comment": String::from_utf8_lossy(archive.comment()).into_owned(),
                        }))
                    }
                }
            }
        }
    }
    for (entry, query) in reads
        .iter_mut()
        .zip(inputs["reads"].as_array().cloned().unwrap_or_default())
    {
        if let Some(case_id) = query.get("case").and_then(|value| value.as_str()) {
            entry["case"] = serde_json::Value::String(case_id.to_string());
        }
    }
    let after = compat_gallery_poststate(&app, &root, inputs);
    serde_json::json!({
        "reads": reads,
        "thumbs": thumbs,
        "archives": archives,
        "poststate": {"before": before, "after": after},
    })
}

fn compat_webui(_inputs: &serde_json::Value) -> serde_json::Value {
    let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let pack = manifest.join("ui");
    let mut files = Vec::new();
    if pack.is_dir() {
        let mut stack = vec![pack.clone()];
        while let Some(dir) = stack.pop() {
            let mut entries: Vec<_> = std::fs::read_dir(&dir)
                .expect("ui dir")
                .map(|entry| entry.expect("entry").path())
                .collect();
            entries.sort();
            for path in entries {
                if path.is_dir() {
                    stack.push(path);
                } else if let Ok(bytes) = std::fs::read(&path) {
                    files.push(serde_json::json!({
                        "name": path.strip_prefix(&pack).expect("prefix")
                            .to_string_lossy().replace('\\', "/"),
                        "size": bytes.len(),
                    }));
                }
            }
        }
    }
    files.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    let index = pack.join("index.html");
    let index_text = std::fs::read_to_string(&index).unwrap_or_default();
    serde_json::json!({
        "files": files,
        "index_markers": {
            "has_title": index_text.contains("<title"),
            "has_app": index_text.contains("/static/js/app"),
            "has_auth_host": index_text.contains("host="),
        },
    })
}

/// M6: the real production executable, its CLI surface, prompt flow, download
/// auto-exit/residency, signals and the POSIX daemon lifecycle.
///
/// Every test here spawns the built `xeHentai` binary as a child process with
/// an isolated cwd/HOME/TMPDIR; nothing runs the parser or the App in-process.
mod m6 {
    use std::collections::BTreeMap;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::{Path, PathBuf};
    #[cfg(unix)]
    use std::process::Child;
    use std::process::{Command, Stdio};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, OnceLock};
    use std::time::Duration;
    #[cfg(unix)]
    use std::time::Instant;

    const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

    // MARK: - production binary

    /// The real binary next to the test harness, rebuilt when the sources are
    /// newer than it. `cargo test` builds the bin target in *test* mode, so the
    /// plain executable is produced here once per test binary run.
    pub(super) fn binary() -> PathBuf {
        static BINARY: OnceLock<PathBuf> = OnceLock::new();
        BINARY
            .get_or_init(|| {
                let test_exe = std::env::current_exe().expect("test executable");
                let debug = test_exe
                    .parent()
                    .and_then(Path::parent)
                    .expect("target/debug")
                    .to_path_buf();
                let binary = debug.join("xeHentai");
                if !fresh(&binary) {
                    let output = Command::new(env!("CARGO"))
                        .args(["build", "--bin", "xeHentai", "--jobs", "2"])
                        .current_dir(MANIFEST)
                        .output()
                        .expect("run cargo build");
                    assert!(
                        output.status.success(),
                        "cargo build failed: {}\n{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    );
                }
                assert!(binary.is_file(), "missing {}", binary.display());
                binary
            })
            .clone()
    }

    fn fresh(binary: &Path) -> bool {
        let Ok(metadata) = std::fs::metadata(binary) else {
            return false;
        };
        let Ok(built) = metadata.modified() else {
            return false;
        };
        let mut newest = built;
        let mut stack = vec![Path::new(MANIFEST).join("src")];
        stack.push(Path::new(MANIFEST).to_path_buf());
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                let relevant = path.extension().and_then(|value| value.to_str()) == Some("rs")
                    || path.file_name().and_then(|value| value.to_str()) == Some("Cargo.toml");
                if relevant && let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                    newest = newest.max(modified);
                }
            }
        }
        built >= newest
    }

    struct Outcome {
        code: i32,
        stdout: String,
        stderr: String,
    }

    pub(super) fn base_command(root: &Path) -> Command {
        let mut command = Command::new(binary());
        command
            .current_dir(root)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.join("home"))
            .env("TMPDIR", root.join("tmp"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("LANG", "C")
            .env("LC_ALL", "C");
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("tmp")).unwrap();
        std::fs::create_dir_all(root.join("cache")).unwrap();
        // The release ships `rss/` next to the program or in the working
        // directory; the test root is the working directory, so publish the
        // real business modules there instead of reaching into the build tree.
        let rss = root.join("rss");
        if !rss.is_dir() {
            copy_tree(&Path::new(MANIFEST).join("rss"), &rss);
        }
        command
    }

    pub(super) fn copy_tree(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let path = entry.path();
            let target = to.join(entry.file_name());
            if path.is_dir() {
                copy_tree(&path, &target);
            } else {
                std::fs::copy(&path, &target).unwrap();
            }
        }
    }

    fn run(root: &Path, args: &[&str]) -> Outcome {
        let output = base_command(root)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .expect("spawn the production binary");
        Outcome {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    }

    /// Unix-only: every caller drives a POSIX signal/PTY or daemon child. The
    /// Windows lifecycle is not exercised by these helpers.
    #[cfg(unix)]
    pub(super) fn wait_exit(
        child: &mut Child,
        timeout: Duration,
    ) -> Option<std::process::ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(status) = child.try_wait().expect("try_wait") {
                return Some(status);
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[cfg(unix)]
    pub(super) fn wait_for<F: FnMut() -> bool>(timeout: Duration, mut check: F) -> bool {
        let deadline = Instant::now() + timeout;
        loop {
            if check() {
                return true;
            }
            if Instant::now() > deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    pub(super) fn image_files(root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if path
                    .extension()
                    .and_then(|value| value.to_str())
                    .is_some_and(|ext| matches!(ext, "jpg" | "jpeg" | "png"))
                {
                    out.push(path);
                }
            }
        }
        out.sort();
        out
    }

    // MARK: - local gallery fixture

    /// A real TCP gallery/proxy server: it answers the absolute-form requests
    /// an HTTP proxy receives for `http://e-hentai.org/...` and keeps every
    /// requested path, so tests observe actual traffic instead of guessed
    /// state.
    pub(super) struct Gallery {
        port: u16,
        paths: Arc<Mutex<Vec<String>>>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Gallery {
        pub(super) fn start(count: usize) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let paths = Arc::new(Mutex::new(Vec::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let seen = Arc::clone(&paths);
            let stopping = Arc::clone(&stop);
            let bind = listener;
            let images = images(count);
            let handle = std::thread::spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    match bind.accept() {
                        Ok((socket, _)) => {
                            socket.set_nonblocking(false).unwrap();
                            serve(socket, &seen, &images);
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => return,
                    }
                }
            });
            Gallery {
                port,
                paths,
                stop,
                handle: Some(handle),
            }
        }

        pub(super) fn proxy(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        pub(super) fn paths(&self) -> Vec<String> {
            self.paths.lock().unwrap().clone()
        }
    }

    impl Drop for Gallery {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::SeqCst);
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    /// A 40-hex image hash that is distinct in its first ten characters: the
    /// reference keys `reload_map` by that prefix, so hashes differing only at
    /// the end would be treated as duplicate images.
    fn hash(index: usize) -> String {
        format!("{:010x}{:030x}", 0xabc000 + index, 0)
    }

    fn images(count: usize) -> BTreeMap<String, Vec<u8>> {
        let mut images = BTreeMap::new();
        for index in 0..count {
            let bytes: Vec<u8> = (0..2048u32)
                .map(|value| ((value + index as u32) % 251) as u8)
                .collect();
            images.insert(hash(index), bytes);
        }
        images
    }

    fn metadata_body(count: usize) -> String {
        format!(
            "=\"gj\">日本語タイトル</h1>=\"gn\">English title</h1>\
             Length:</td><td class=\"gdt2\">{count} pages</td>\
             Showing 1 - 20 of {count} images"
        )
    }

    fn page_body(count: usize) -> String {
        let mut body = String::new();
        for index in 0..count {
            // Each page carries a distinct 10-hex page hash; the reference
            // de-duplicates pages by that id, so a repeated prefix would turn
            // every later image into a copy of the first one.
            body.push_str(&format!(
                "<a href=\"http://e-hentai.org/s/{:010x}/{}-{}\"><div title=\"Page {}\"",
                0xabc000 + index,
                700000 + index,
                index + 1,
                index + 1
            ));
        }
        body
    }

    fn image_page_body(index: usize, images: &BTreeMap<String, Vec<u8>>) -> String {
        let key = hash(index);
        let size = images.get(&key).map(Vec::len).unwrap_or(0);
        format!(
            "<img src=\"http://img.test/{key}-{size}-200-0-jpg/keystamp=abc\" style=\"max-width:100%\">\
             </a></div><div>unique-{index}.jpg :: 100x200 :: 2 KB</di>\
             return nl('{}-{index}')",
            700000 + index
        )
    }

    fn respond(socket: &mut TcpStream, status: &str, mime: &str, body: &[u8]) {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        let _ = socket.write_all(&response);
        let _ = socket.flush();
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }

    fn serve(
        mut socket: TcpStream,
        seen: &Arc<Mutex<Vec<String>>>,
        images: &BTreeMap<String, Vec<u8>>,
    ) {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            match socket.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                Err(_) => return,
            }
            if buffer.len() > 64 * 1024 {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buffer).into_owned();
        let target = head
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("")
            .to_string();
        seen.lock().unwrap().push(target.clone());
        // An HTTP proxy receives the absolute form; keep only the path part.
        let path = match target.find("://") {
            Some(index) => match target[index + 3..].find('/') {
                Some(slash) => target[index + 3 + slash..].to_string(),
                None => "/".to_string(),
            },
            None => target.clone(),
        };
        let count = images.len();
        if path.contains("/g/") && path.contains("?p=") {
            respond(
                &mut socket,
                "200 OK",
                "text/html; charset=utf-8",
                page_body(count).as_bytes(),
            );
        } else if path.contains("/g/") {
            let mut body = metadata_body(count);
            body.push_str(&page_body(count));
            respond(
                &mut socket,
                "200 OK",
                "text/html; charset=utf-8",
                body.as_bytes(),
            );
        } else if let Some(rest) = path.strip_prefix("/s/") {
            // `/s/<hash10>/<page>-<index>`
            let index = rest
                .rsplit('-')
                .next()
                .and_then(|value| value.trim_end_matches('/').parse::<usize>().ok())
                .unwrap_or(1);
            let body = image_page_body(index.saturating_sub(1), images);
            respond(
                &mut socket,
                "200 OK",
                "text/html; charset=utf-8",
                body.as_bytes(),
            );
        } else if let Some(key) = images
            .keys()
            .find(|key| path.contains(key.as_str()))
            .cloned()
        {
            let bytes = images.get(&key).cloned().unwrap_or_default();
            respond(&mut socket, "200 OK", "image/jpeg", &bytes);
        } else {
            respond(&mut socket, "404 Not Found", "text/plain", b"missing");
        }
    }

    // MARK: - CLI surface

    #[test]
    fn m6_binary_reports_version_help_and_usage_errors() {
        let root = tempfile::tempdir().expect("root");
        let version = run(root.path(), &["--version"]);
        assert_eq!(version.code, 0, "{}", version.stderr);
        assert_eq!(version.stdout.trim(), "xeHentai v2.023-rss");
        assert!(version.stderr.is_empty());

        let help = run(root.path(), &["--help"]);
        assert_eq!(help.code, 0);
        for flag in [
            "-u, --username",
            "-k, --key",
            "-c, --cookie",
            "-i, --interactive",
            "--daemon",
            "-d, --dir",
            "-o, --download-ori",
            "-j, --jpn-title",
            "-r, --rename-ori",
            "-p, --proxy",
            "--proxy-image",
            "--proxy-image-only",
            "--rpc-interface",
            "--rpc-port",
            "--rpc-secret",
            "--rpc-open-browser",
            "--delete-task-files",
            "-a, --archive",
            "--download-range",
            "-t, --thread",
            "--timeout",
            "--low-speed-threshold",
            "-f, --force",
            "--auto-update",
            "--update-beta-channel",
            "-l, --logpath",
            "-v, --verbose",
            "-h, --help",
            "--version",
        ] {
            assert!(help.stdout.contains(flag), "help is missing {flag}");
        }
        assert!(help.stdout.contains("xeHentai Downloader NG"));

        let unknown = run(root.path(), &["--nope"]);
        assert_eq!(unknown.code, 2);
        assert!(unknown.stdout.is_empty());
        assert!(unknown.stderr.contains("--nope"), "{}", unknown.stderr);

        let bare = run(root.path(), &["-j"]);
        assert_eq!(bare.code, 2);
        let mutex = run(root.path(), &["--proxy-image", "--proxy-image-only"]);
        assert_eq!(mutex.code, 2);
        let range = run(root.path(), &["--download-range", "1 - 2"]);
        assert_eq!(range.code, 2);
        assert!(
            range.stderr.contains("'1 - 2' is not a range format"),
            "{}",
            range.stderr
        );
    }

    // MARK: - URL mode: real download, then the reference auto-exit

    #[test]
    fn m6_binary_downloads_urls_then_exits_zero() {
        let root = tempfile::tempdir().expect("root");
        let gallery = Gallery::start(4);
        let outcome = run(
            root.path(),
            &[
                "-p",
                &gallery.proxy(),
                "-d",
                "dl",
                "--auto-update",
                "off",
                "http://e-hentai.org/g/1/aaaa",
            ],
        );
        assert_eq!(
            outcome.code, 0,
            "stdout={}\nstderr={}",
            outcome.stdout, outcome.stderr
        );
        let files = image_files(&root.path().join("dl"));
        assert_eq!(files.len(), 4, "files: {files:?}");
        let paths = gallery.paths();
        assert!(
            paths
                .iter()
                .any(|path| path.contains("e-hentai.org/g/1/aaaa") && !path.contains("?p=")),
            "metadata request missing: {paths:?}"
        );
        assert!(
            paths.iter().any(|path| path.contains("/s/")),
            "image pages not requested: {paths:?}"
        );
        assert_eq!(
            paths
                .iter()
                .filter(|path| path.contains("img.test"))
                .count(),
            4,
            "image requests: {paths:?}"
        );
        let log = std::fs::read_to_string(root.path().join("eh.log")).expect("log file");
        assert!(log.contains("xeHentai 2.023-rss started."), "{log}");
        assert!(log.contains("application task loop finished"), "{log}");
        // `save_tasks` defaults to false, so the reference session holds only
        // the cookies; this is the exact shape the frozen reference writes.
        let session: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.path().join("h.json")).unwrap())
                .expect("session JSON");
        assert_eq!(
            session,
            serde_json::json!({"tasks": {}, "cookies": {"nw": "1"}})
        );
    }

    // MARK: - no URL: resident until interrupted, then the reference cleanup

    #[cfg(unix)]
    #[test]
    fn m6_binary_without_urls_stays_resident_until_sigterm() {
        let root = tempfile::tempdir().expect("root");
        let mut child = base_command(root.path())
            .args(["--auto-update", "off"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let log_path = root.path().join("eh.log");
        // Real barrier: the startup log line proves the app is running.
        let started = wait_for(Duration::from_secs(20), || {
            std::fs::read_to_string(&log_path)
                .map(|log| log.contains("xeHentai 2.023-rss started."))
                .unwrap_or(false)
        });
        assert!(started, "the binary never logged its startup line");
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "without URLs the process must stay resident"
        );
        // The reference `cli.start --auto-update off` run is resident with no
        // network activity; nothing is configured here that could reach one.
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "a resident run must not exit on its own"
        );
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGTERM,
        )
        .expect("SIGTERM");
        let status = wait_exit(&mut child, Duration::from_secs(30)).expect("terminated");
        assert_eq!(status.code(), Some(0));
        let log = std::fs::read_to_string(&log_path).unwrap();
        assert!(log.contains("cleaning up..."), "{log}");
        assert!(log.contains("application task loop finished"), "{log}");
        let session: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.path().join("h.json")).unwrap())
                .expect("session JSON");
        assert_eq!(
            session,
            serde_json::json!({"tasks": {}, "cookies": {"nw": "1"}})
        );
    }

    #[cfg(unix)]
    #[test]
    fn m6_binary_second_interrupt_is_a_bounded_exit() {
        let root = tempfile::tempdir().expect("root");
        let mut child = base_command(root.path())
            .args(["--auto-update", "off"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        let log_path = root.path().join("eh.log");
        assert!(
            wait_for(Duration::from_secs(20), || std::fs::read_to_string(
                &log_path
            )
            .map(|log| log.contains("started."))
            .unwrap_or(false)),
            "startup line never appeared"
        );
        let pid = nix::unistd::Pid::from_raw(child.id() as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).expect("first SIGINT");
        std::thread::sleep(Duration::from_millis(2));
        let _ = nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT);
        let status = wait_exit(&mut child, Duration::from_secs(30))
            .expect("the second interrupt must not hang the process");
        assert_eq!(status.code(), Some(0));
        if child.try_wait().expect("try_wait").is_none() {
            let _ = child.kill();
        }
    }

    /// Test-only child entry point for the supervisor regression below. It runs
    /// the SAME production `run_with_args`/`run_supervised` startup as the real
    /// binary and injects the existing deterministic slow stage-open hook so a
    /// blocking filesystem open stays owned across the first shutdown. Inert in
    /// ordinary test runs (the parent sets the environment).
    #[cfg(unix)]
    #[ignore = "spawned by m6_supervisor_second_interrupt_forces_exit_while_blocking_work_is_held"]
    #[test]
    fn m6_supervisor_held_work_child_process() {
        let Ok(root) = std::env::var("XEH_M6_SUPERVISOR_ROOT") else {
            return;
        };
        let proxy = std::env::var("XEH_M6_SUPERVISOR_PROXY").expect("child proxy");
        let held_path = std::path::PathBuf::from(
            std::env::var("XEH_M6_SUPERVISOR_HELD").expect("child held marker"),
        );
        let finished_path = std::path::PathBuf::from(
            std::env::var("XEH_M6_SUPERVISOR_FINISHED").expect("child finished marker"),
        );
        let crate::core::SlowOpenHandle {
            started,
            finished,
            release,
        } = crate::core::arm_slow_open();
        crate::core::set_io_fault(Some(crate::core::IoFault::SlowOpen));
        std::thread::spawn(move || {
            if started.recv_timeout(Duration::from_secs(90)).is_ok() {
                let _ = std::fs::write(held_path, b"blocking stage open started\n");
            }
        });
        std::thread::spawn(move || {
            if finished.recv_timeout(Duration::from_secs(90)).is_ok() {
                let _ = std::fs::write(finished_path, b"blocking stage open finished\n");
            }
        });
        // The release sender stays owned by this process; the parent never
        // releases the held work, so a bounded exit must kill it in flight.
        let _release = release;
        let argv = [
            "xeHentai",
            "-p",
            proxy.as_str(),
            "-d",
            "sup",
            "--auto-update",
            "off",
            "http://e-hentai.org/g/1/aaaa",
        ];
        let code = crate::run_with_args(Path::new(&root), argv);
        // The fixed supervisor leaves through `process::exit(0)` on the second
        // signal; reaching this line means it drained the held work instead.
        eprintln!("supervisor child returned {code} without the second interrupt");
        std::process::exit(3);
    }

    /// Blocker B regression: the first SIGINT starts a graceful drain that is
    /// held by a real blocking stage open, and the second SIGINT must force a
    /// bounded process exit without releasing that work. The old d8 supervisor
    /// awaited the task loop (and therefore the held open) on the second signal.
    #[cfg(unix)]
    #[test]
    fn m6_supervisor_second_interrupt_forces_exit_while_blocking_work_is_held() {
        let root = tempfile::tempdir().expect("root");
        let gallery = Gallery::start(4);
        copy_tree(&Path::new(MANIFEST).join("rss"), &root.path().join("rss"));
        for name in ["home", "tmp", "cache"] {
            std::fs::create_dir_all(root.path().join(name)).unwrap();
        }
        let held_marker = root.path().join("held.marker");
        let finished_marker = root.path().join("finished.marker");
        let log_path = root.path().join("eh.log");
        let child_output = root.path().join("child.out");
        let sink = std::fs::File::create(&child_output).expect("child output");
        let child = Command::new(std::env::current_exe().expect("test executable"))
            .args([
                "--exact",
                "tests::m6::m6_supervisor_held_work_child_process",
                "--ignored",
                "--nocapture",
                "--test-threads=1",
            ])
            .current_dir(root.path())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", root.path().join("home"))
            .env("TMPDIR", root.path().join("tmp"))
            .env("XDG_CACHE_HOME", root.path().join("cache"))
            .env("LANG", "C")
            .env("LC_ALL", "C")
            .env("XEH_M6_SUPERVISOR_ROOT", root.path())
            .env("XEH_M6_SUPERVISOR_PROXY", gallery.proxy())
            .env("XEH_M6_SUPERVISOR_HELD", &held_marker)
            .env("XEH_M6_SUPERVISOR_FINISHED", &finished_marker)
            .stdin(Stdio::null())
            .stdout(Stdio::from(sink.try_clone().unwrap()))
            .stderr(Stdio::from(sink))
            .spawn()
            .expect("spawn supervisor child");
        // Only this test's own child is ever signalled or reaped.
        struct Reap(Child);
        impl Drop for Reap {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
        let mut child = Reap(child);
        let output = |path: &Path| std::fs::read_to_string(path).unwrap_or_default();
        assert!(
            wait_for(Duration::from_secs(90), || held_marker.is_file()),
            "the child never reached the held blocking stage open; log: {:?}\noutput: {}",
            output(&log_path),
            output(&child_output)
        );
        assert!(
            child.0.try_wait().expect("try_wait").is_none(),
            "the child exited before any signal: {}",
            output(&child_output)
        );
        let pid = nix::unistd::Pid::from_raw(child.0.id() as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).expect("first SIGINT");
        assert!(
            wait_for(Duration::from_secs(20), || {
                std::fs::read_to_string(&log_path)
                    .map(|log| log.contains("cleaning up..."))
                    .unwrap_or(false)
            }),
            "the first signal never started the graceful drain; log: {:?}",
            output(&log_path)
        );
        // The first shutdown is genuinely pending on the owned blocking open.
        std::thread::sleep(Duration::from_millis(1200));
        assert!(
            child.0.try_wait().expect("try_wait").is_none(),
            "the first signal drained although the blocking stage open is still held"
        );
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT).expect("second SIGINT");
        let status = wait_exit(&mut child.0, Duration::from_secs(5)).unwrap_or_else(|| {
            panic!(
                "the second interrupt waited for the held blocking work instead of exiting; \
                 log: {:?}\noutput: {}",
                output(&log_path),
                output(&child_output)
            )
        });
        assert_eq!(status.code(), Some(0), "{}", output(&child_output));
        assert!(
            !finished_marker.exists(),
            "the held blocking open finished before the process exited"
        );
    }

    // MARK: - prompt flow through a real PTY

    #[cfg(unix)]
    struct Pty {
        child: Child,
        master: std::fs::File,
        reader: Option<std::thread::JoinHandle<String>>,
        transcript: Arc<Mutex<String>>,
    }

    #[cfg(unix)]
    fn spawn_pty(root: &Path, args: &[&str]) -> Pty {
        let pty = nix::pty::openpty(None, None).expect("openpty");
        let master = std::fs::File::from(pty.master);
        let slave = std::fs::File::from(pty.slave);
        let stdin = slave.try_clone().expect("slave clone");
        let stdout = slave.try_clone().expect("slave clone");
        let stderr = slave.try_clone().expect("slave clone");
        let child = base_command(root)
            .args(args)
            .stdin(Stdio::from(stdin))
            .stdout(Stdio::from(stdout))
            .stderr(Stdio::from(stderr))
            .spawn()
            .expect("spawn in pty");
        drop(slave);
        let transcript = Arc::new(Mutex::new(String::new()));
        let sink = Arc::clone(&transcript);
        let mut reader_master = master.try_clone().expect("master clone");
        let reader = std::thread::spawn(move || {
            let mut collected = String::new();
            let mut chunk = [0u8; 512];
            loop {
                match reader_master.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(read) => {
                        collected.push_str(&String::from_utf8_lossy(&chunk[..read]));
                        sink.lock()
                            .unwrap()
                            .push_str(&String::from_utf8_lossy(&chunk[..read]));
                    }
                    // EIO once every slave descriptor is closed.
                    Err(_) => break,
                }
            }
            collected
        });
        Pty {
            child,
            master,
            reader: Some(reader),
            transcript,
        }
    }

    #[cfg(unix)]
    impl Pty {
        fn answer(&mut self, text: &str) {
            self.master.write_all(text.as_bytes()).expect("write pty");
            self.master.flush().expect("flush pty");
        }

        fn output(&self) -> String {
            self.transcript.lock().unwrap().clone()
        }

        fn finish(mut self, timeout: Duration) -> (Option<std::process::ExitStatus>, String) {
            let mut status = wait_exit(&mut self.child, timeout);
            if status.is_none() {
                // Bound the test: a stuck prompt is reported, never waited on.
                let _ = nix::sys::signal::kill(
                    nix::unistd::Pid::from_raw(self.child.id() as i32),
                    nix::sys::signal::Signal::SIGKILL,
                );
                status = wait_exit(&mut self.child, Duration::from_secs(10));
            }
            drop(self.master.try_clone());
            let output = self
                .reader
                .take()
                .map(|handle| handle.join().unwrap_or_default())
                .unwrap_or_default();
            (status, output)
        }
    }

    #[cfg(unix)]
    impl Drop for Pty {
        fn drop(&mut self) {
            if self.child.try_wait().ok().flatten().is_none() {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn m6_pty_prompts_in_reference_order_and_answers_drive_the_download() {
        let root = tempfile::tempdir().expect("root");
        let gallery = Gallery::start(4);
        let mut pty = spawn_pty(root.path(), &["-i", "--auto-update", "off"]);
        // login n, URL, original n, proxy = fixture, dir "pti", rename n,
        // archive n, jpn n, range empty.
        pty.answer("n\n");
        pty.answer("http://e-hentai.org/g/1/aaaa\n");
        pty.answer("n\n");
        pty.answer(&format!("{}\n", gallery.proxy()));
        pty.answer("pti\n");
        pty.answer("n\n");
        pty.answer("n\n");
        pty.answer("n\n");
        pty.answer("\n");
        let (status, output) = pty.finish(Duration::from_secs(60));
        let status = status.expect("interactive run must exit");
        assert_eq!(status.code(), Some(0), "{output}");
        let order = [
            "login to exhentai (y/n)? > ",
            "URL (seperate with ,)> ",
            "Download original (y/n, default:n)? > ",
            "Proxy (optional) > ",
            "Download to (default: ",
            "press enter or enter new > ",
            "Rename to original name (y/n, default:n)? > ",
            "Make archive (y/n, default:n)? > ",
            "Use Japanese title (y/n, default:y)? > ",
            "Download range, press enter to download all > ",
        ];
        let mut cursor = 0;
        for prompt in order {
            let found = output[cursor..]
                .find(prompt)
                .unwrap_or_else(|| panic!("prompt {prompt:?} missing from {output:?}"));
            cursor += found + prompt.len();
        }
        // The answers, not the file defaults, decided the directory and the
        // proxy; the English title was chosen by `n`.
        let files = image_files(&root.path().join("pti"));
        assert_eq!(files.len(), 4, "files: {files:?}\noutput: {output}");
        assert!(
            files
                .iter()
                .all(|path| path.to_string_lossy().contains("English title")),
            "{files:?}"
        );
        assert!(!gallery.paths().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn m6_pty_bad_range_retries_then_applies_the_range() {
        let root = tempfile::tempdir().expect("root");
        let gallery = Gallery::start(4);
        let mut pty = spawn_pty(root.path(), &["-i", "--auto-update", "off"]);
        pty.answer("n\n");
        pty.answer("http://e-hentai.org/g/1/aaaa\n");
        pty.answer("n\n");
        pty.answer(&format!("{}\n", gallery.proxy()));
        pty.answer("pti2\n");
        pty.answer("n\n");
        pty.answer("n\n");
        pty.answer("y\n");
        pty.answer("abc\n");
        pty.answer("2-3\n");
        let (status, output) = pty.finish(Duration::from_secs(60));
        let status = status.expect("interactive run must exit");
        assert_eq!(status.code(), Some(0), "{output}");
        assert!(
            output.contains("'abc' is not a range format, expecting '1-2' or '3'"),
            "{output}"
        );
        // The prompt was shown again after the error.
        let range_prompt = "Download range, press enter to download all > ";
        assert_eq!(output.matches(range_prompt).count(), 2, "{output}");
        let files = image_files(&root.path().join("pti2"));
        assert_eq!(
            files.len(),
            2,
            "range 2-3 must limit the download: {files:?}\noutput: {output}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn m6_pty_ctrl_c_at_the_login_prompt_cleans_up_and_exits_zero() {
        let root = tempfile::tempdir().expect("root");
        let pty = spawn_pty(root.path(), &["-i", "--auto-update", "off"]);
        assert!(
            wait_for(Duration::from_secs(20), || pty
                .output()
                .contains("login to exhentai (y/n)? > ")),
            "login prompt missing: {}",
            pty.output()
        );
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(pty.child.id() as i32),
            nix::sys::signal::Signal::SIGINT,
        )
        .expect("SIGINT");
        let (status, output) = pty.finish(Duration::from_secs(30));
        let status = status.expect("cancelled prompt must exit");
        assert_eq!(status.code(), Some(0), "{output}");
        let log = std::fs::read_to_string(root.path().join("eh.log")).unwrap();
        assert!(log.contains("started."), "{log}");
        assert!(log.contains("cleaning up..."), "{log}");
        let session: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.path().join("h.json")).unwrap())
                .expect("session JSON");
        assert_eq!(session["cookies"]["nw"], "1");
    }

    // MARK: - POSIX daemon

    #[cfg(unix)]
    #[test]
    fn m6_daemon_forks_once_and_the_child_runs_the_real_app() {
        let root = tempfile::tempdir().expect("root");
        let gallery = Gallery::start(4);
        let outcome = run(
            root.path(),
            &[
                "--daemon",
                "-p",
                &gallery.proxy(),
                "-d",
                "dmn",
                "--auto-update",
                "off",
                "http://e-hentai.org/g/1/aaaa",
            ],
        );
        assert_eq!(outcome.code, 0, "stderr={}", outcome.stderr);
        let pid_line = outcome
            .stdout
            .lines()
            .find(|line| line.contains("daemon start at PID "))
            .unwrap_or_else(|| panic!("no daemon PID line: {}", outcome.stdout));
        let pid: i32 = pid_line
            .rsplit(' ')
            .next()
            .and_then(|value| value.trim().parse().ok())
            .expect("numeric PID");
        assert!(pid > 0);
        // The child is the one that does the work: wait for it to finish on its
        // own, observing the downloaded tree and its own log lines.
        let finished = wait_for(Duration::from_secs(90), || {
            image_files(&root.path().join("dmn")).len() == 4
        });
        assert!(finished, "the daemon child did not complete the download");
        // URL mode leaves on its own; only the test's own daemon PID is
        // observed, and it is killed only if that exit never happens.
        let exited = wait_for(Duration::from_secs(60), || {
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_err()
        });
        if !exited {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(pid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        assert!(
            exited,
            "the daemon child did not exit after its URLs finished"
        );
        let log = std::fs::read_to_string(root.path().join("eh.log")).unwrap();
        assert!(log.contains("started."), "{log}");
        assert!(log.contains("application task loop finished"), "{log}");
    }

    /// Blocker A regression: a valid login restored from `h.json` or supplied
    /// with `-c` must skip the login question and still reach every download
    /// prompt, because active `-i` needs the filters module in both routes.
    #[cfg(unix)]
    #[test]
    fn m6_pty_logged_in_session_and_cookie_skip_the_login_prompt() {
        for route in ["session", "cookie"] {
            let root = tempfile::tempdir().expect("root");
            let gallery = Gallery::start(4);
            let cookie = "ipb_member_id=42; ipb_pass_hash=cafebabe";
            if route == "session" {
                std::fs::write(
                    root.path().join("h.json"),
                    serde_json::json!({
                        "tasks": {},
                        "cookies": {"ipb_member_id": "42", "ipb_pass_hash": "cafebabe"}
                    })
                    .to_string(),
                )
                .expect("session fixture");
            }
            let mut args = vec!["-i"];
            if route == "cookie" {
                args.extend(["-c", cookie]);
            }
            args.extend(["--auto-update", "off"]);
            let mut pty = spawn_pty(root.path(), &args);
            // No login answer: the valid login must skip that question.
            pty.answer("http://e-hentai.org/g/1/aaaa\n");
            pty.answer("n\n");
            pty.answer(&format!("{}\n", gallery.proxy()));
            pty.answer("logged-in\n");
            pty.answer("n\n");
            pty.answer("n\n");
            pty.answer("n\n");
            pty.answer("\n");
            let (status, output) = pty.finish(Duration::from_secs(60));
            let status = status.unwrap_or_else(|| panic!("{route}: logged-in run must exit"));
            assert_eq!(status.code(), Some(0), "{route}: {output}");
            assert!(
                !output.contains("login to exhentai (y/n)? "),
                "{route}: a valid login must skip the login question: {output}"
            );
            let order = [
                "URL (seperate with ,)> ",
                "Download original (y/n, default:n)? > ",
                "Proxy (optional) > ",
                "Download to (default: ",
                "press enter or enter new > ",
                "Rename to original name (y/n, default:n)? > ",
                "Make archive (y/n, default:n)? > ",
                "Use Japanese title (y/n, default:y)? > ",
                "Download range, press enter to download all > ",
            ];
            let mut cursor = 0;
            for prompt in order {
                let found = output[cursor..].find(prompt).unwrap_or_else(|| {
                    panic!("{route}: prompt {prompt:?} missing from {output:?}")
                });
                cursor += found + prompt.len();
            }
            let files = image_files(&root.path().join("logged-in"));
            assert_eq!(
                files.len(),
                4,
                "{route}: files: {files:?}\noutput: {output}"
            );
            assert!(!gallery.paths().is_empty(), "{route}: no fixture traffic");
            let session: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(root.path().join("h.json")).unwrap())
                    .expect("session JSON");
            assert_eq!(
                session["cookies"]["ipb_member_id"], "42",
                "{route}: the login cookies must survive the run"
            );
        }
    }

    /// The M6 startup now owns the RPC start too: a configured `rpc_port` must
    /// serve the real dispatcher from the same supervised run, and a signal must
    /// retire it through the shared shutdown path.
    #[cfg(unix)]
    #[test]
    fn m6_binary_with_rpc_serves_get_info_and_stops_on_signal() {
        let root = tempfile::tempdir().expect("root");
        let probe = TcpListener::bind(("127.0.0.1", 0)).expect("free port");
        let port = probe.local_addr().unwrap().port();
        drop(probe);
        std::fs::write(
            root.path().join("config.py"),
            format!(
                "rpc_interface = \"127.0.0.1\"\nrpc_port = {port}\nrpc_open_browser = False\nauto_update = \"off\"\n"
            ),
        )
        .unwrap();
        let mut child = base_command(root.path())
            .args(["-d", "dl", "-l", "eh.log"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn");
        // Real readiness barrier: the listener accepts a TCP connection.
        let connected = wait_for(Duration::from_secs(30), || {
            std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()
        });
        assert!(connected, "the RPC server never started listening");
        let body = br#"{"method":"xeH.getInfo","id":"m6","params":[[],{}]}"#;
        let request = format!(
            "POST /jsonrpc HTTP/1.0\r\nHost: x\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let mut stream = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        stream
            .set_read_timeout(Some(Duration::from_secs(20)))
            .unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
        let mut response = Vec::new();
        let _ = stream.read_to_end(&mut response);
        let response = String::from_utf8_lossy(&response).into_owned();
        assert!(
            response.starts_with("HTTP/1.") && response.contains("200"),
            "unexpected RPC response: {response}"
        );
        assert!(response.contains("\"m6\""), "{response}");
        assert!(response.contains("\"version\""), "{response}");
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGINT,
        )
        .expect("SIGINT");
        let status = wait_exit(&mut child, Duration::from_secs(60)).expect("stopped");
        assert_eq!(status.code(), Some(0));
        let log = std::fs::read_to_string(root.path().join("eh.log")).unwrap();
        assert!(log.contains("started."), "{log}");
        assert!(log.contains("cleaning up..."), "{log}");
        // The shared session writer ran under the same supervisor as the server.
        let session: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(root.path().join("h.json")).unwrap())
                .expect("session JSON");
        assert_eq!(session["cookies"]["nw"], "1");
    }
}

/// M7: the real binary update check. The business decision lives in
/// `rss/updater/*`; these tests drive the production entry and observe actual
/// loopback HTTP transcripts, logs and process lifetime.
// POSIX signals and real child processes: the M7 binary cases run on Unix
// only, exactly like the platform they verify.
#[cfg(unix)]
mod m7 {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::Path;
    use std::process::{Child, Stdio};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    use super::m6;

    const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

    /// The commit fixture is the frozen oracle's real response body
    /// (`m7-updater-oracle`, u1_release_master_first_of_two_crlf): two entries,
    /// CR/LF in the first message, and a second entry that must be ignored.
    pub(super) const COMMITS: &str = "[{\"sha\": \"a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\", \
        \"commit\": {\"author\": {\"date\": \"2026-10-31T12:34:56Z\"}, \
        \"message\": \"first entry\\r\\nwith CRLF\\r\\nthird line\"}}, \
        {\"sha\": \"0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c\", \
        \"commit\": {\"author\": {\"date\": \"2026-10-30T01:02:03Z\"}, \
        \"message\": \"second entry\"}}]";

    pub(super) const UPDATE_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

    #[derive(Clone, Debug)]
    pub(super) struct Request {
        pub method: String,
        pub target: String,
        pub headers: Vec<(String, String)>,
    }

    impl Request {
        pub fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }
    }

    /// A real loopback HTTP server. It records every request head and answers
    /// immediately, or sends a real response head plus a PARTIAL body and holds
    /// the remainder until `release`/stop, recording how many body bytes went
    /// out and whether the client closed the connection first.
    pub(super) struct Server {
        pub port: u16,
        requests: Arc<Mutex<Vec<Request>>>,
        partial: Arc<AtomicUsize>,
        eof: Arc<AtomicBool>,
        peer_reset: Arc<AtomicBool>,
        peer_error: Arc<Mutex<Option<String>>>,
        observer_stopped: Arc<AtomicBool>,
        release: Arc<(Mutex<bool>, Condvar)>,
        stop: Arc<AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl Server {
        pub fn start(status: u16, body: &'static str) -> Self {
            Self::build(status, body, false, false)
        }

        pub fn start_holding() -> Self {
            Self::build(200, COMMITS, true, false)
        }

        pub fn start_closing() -> Self {
            Self::build(200, COMMITS, false, true)
        }

        fn build(status: u16, body: &'static str, hold: bool, closing: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
            listener.set_nonblocking(true).unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let partial = Arc::new(AtomicUsize::new(0));
            let eof = Arc::new(AtomicBool::new(false));
            let peer_reset = Arc::new(AtomicBool::new(false));
            let peer_error: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
            let observer_stopped = Arc::new(AtomicBool::new(false));
            let release = Arc::new((Mutex::new(false), Condvar::new()));
            let stop = Arc::new(AtomicBool::new(false));
            let seen = Arc::clone(&requests);
            let flushed = Arc::clone(&partial);
            let peer_eof = Arc::clone(&eof);
            let peer_rst = Arc::clone(&peer_reset);
            let peer_err = Arc::clone(&peer_error);
            let stopped = Arc::clone(&observer_stopped);
            let gate = Arc::clone(&release);
            let stopping = Arc::clone(&stop);
            let handle = std::thread::spawn(move || {
                while !stopping.load(Ordering::SeqCst) {
                    match listener.accept() {
                        Ok((socket, _)) => {
                            socket.set_nonblocking(false).unwrap();
                            if closing {
                                let mut socket = socket;
                                record(&mut socket, &seen);
                                drop(socket);
                            } else if hold {
                                serve_held(
                                    socket, &seen, &flushed, &peer_eof, &peer_rst, &peer_err,
                                    &stopped, &gate, &stopping, body,
                                );
                            } else {
                                serve(socket, &seen, status, body);
                            }
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(_) => return,
                    }
                }
            });
            Server {
                port,
                requests,
                partial,
                eof,
                peer_reset,
                peer_error,
                observer_stopped,
                release,
                stop,
                handle: Some(handle),
            }
        }

        pub fn origin(&self) -> String {
            format!("http://127.0.0.1:{}", self.port)
        }

        pub fn requests(&self) -> Vec<Request> {
            self.requests.lock().unwrap().clone()
        }

        /// Body bytes of the held response that are actually on the wire.
        pub fn partial_flushed(&self) -> usize {
            self.partial.load(Ordering::SeqCst)
        }

        /// The client closed the connection while the body was incomplete.
        pub fn peer_eof(&self) -> bool {
            self.eof.load(Ordering::SeqCst)
        }

        /// The peer sent an explicitly observed TCP reset while the body was
        /// incomplete. Like a clean FIN, a reset proves the client went away;
        /// anything else (deadline, still-open, other I/O error) does not.
        pub fn peer_reset(&self) -> bool {
            self.peer_reset.load(Ordering::SeqCst)
        }

        /// The first non-EOF, non-reset, non-timeout I/O error the held
        /// observer saw, if any. A set value means the closure state is
        /// UNKNOWN: the test must fail closed, never count it as a close.
        pub fn peer_error(&self) -> Option<String> {
            self.peer_error.lock().unwrap().clone()
        }

        /// The observer returned because the fixture stopped, not because the
        /// transport closed. True alongside false EOF/reset means the peer was
        /// likely still open when observation ended.
        pub fn observer_stopped(&self) -> bool {
            self.observer_stopped.load(Ordering::SeqCst)
        }

        /// Only a clean FIN or an explicitly observed reset counts as closed.
        /// Timeouts, still-open peers, stop-timeout exits and other errors are
        /// genuinely open/unknown and must fail the close assertion.
        pub fn peer_closed(&self) -> bool {
            self.peer_eof() || self.peer_reset()
        }

        pub fn release(&self) {
            let (lock, cond) = &*self.release;
            *lock.lock().unwrap() = true;
            cond.notify_all();
        }
    }

    impl Drop for Server {
        fn drop(&mut self) {
            // Stop first: the held loop must return without completing the body.
            self.stop.store(true, Ordering::SeqCst);
            self.release();
            let _ = TcpStream::connect(("127.0.0.1", self.port));
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    fn read_head(socket: &mut TcpStream) -> Option<String> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
            match socket.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                Err(_) => return None,
            }
            if buffer.len() > 64 * 1024 {
                return None;
            }
        }
        Some(String::from_utf8_lossy(&buffer).into_owned())
    }

    fn record(socket: &mut TcpStream, seen: &Arc<Mutex<Vec<Request>>>) -> bool {
        let Some(head) = read_head(socket) else {
            return false;
        };
        let mut lines = head.lines();
        let Some(request_line) = lines.next() else {
            return false;
        };
        let mut parts = request_line.split_whitespace();
        let method = parts.next().unwrap_or("").to_string();
        let target = parts.next().unwrap_or("").to_string();
        let mut headers = Vec::new();
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
        seen.lock().unwrap().push(Request {
            method,
            target,
            headers,
        });
        true
    }

    fn write_response(socket: &mut TcpStream, status: u16, body: &str) {
        let reason = match status {
            200 => "OK",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Status",
        };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes());
        let _ = socket.flush();
        let _ = socket.shutdown(std::net::Shutdown::Both);
    }

    fn serve(mut socket: TcpStream, seen: &Arc<Mutex<Vec<Request>>>, status: u16, body: &str) {
        if record(&mut socket, seen) {
            write_response(&mut socket, status, body);
        }
    }

    /// Send a real `200` head whose Content-Length covers the WHOLE JSON body,
    /// flush only a prefix of that body, then hold the remainder. A client that
    /// cancels must close the connection while the body is still incomplete;
    /// only an explicit `release()` completes it.
    #[allow(clippy::too_many_arguments)]
    fn serve_held(
        mut socket: TcpStream,
        seen: &Arc<Mutex<Vec<Request>>>,
        partial: &Arc<AtomicUsize>,
        eof: &Arc<AtomicBool>,
        peer_reset: &Arc<AtomicBool>,
        peer_error: &Arc<Mutex<Option<String>>>,
        observer_stopped: &Arc<AtomicBool>,
        gate: &Arc<(Mutex<bool>, Condvar)>,
        stop: &Arc<AtomicBool>,
        body: &str,
    ) {
        if !record(&mut socket, seen) {
            return;
        }
        let mut split = body.len() / 2;
        while split > 0 && !body.is_char_boundary(split) {
            split -= 1;
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            &body[..split]
        );
        if let Err(error) = socket
            .write_all(response.as_bytes())
            .and_then(|()| socket.flush())
        {
            classify_held_error(&error, peer_reset, peer_error);
            return;
        }
        partial.store(split, Ordering::SeqCst);
        socket
            .set_read_timeout(Some(Duration::from_millis(20)))
            .unwrap();
        let (lock, cond) = &**gate;
        loop {
            if stop.load(Ordering::SeqCst) {
                observer_stopped.store(true, Ordering::SeqCst);
                return;
            }
            if *lock.lock().unwrap() {
                let _ = socket.write_all(&body.as_bytes()[split..]);
                let _ = socket.flush();
                return;
            }
            let mut chunk = [0u8; 256];
            match socket.read(&mut chunk) {
                Ok(0) => {
                    eof.store(true, Ordering::SeqCst);
                    return;
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => {
                    classify_held_error(&error, peer_reset, peer_error);
                    return;
                }
            }
            let (guard, _) = cond
                .wait_timeout(lock.lock().unwrap(), Duration::from_millis(10))
                .unwrap();
            drop(guard);
        }
    }

    /// Record HOW the held connection ended: a clean FIN sets EOF, an
    /// explicit TCP reset sets the reset flag, anything else (including a
    /// write-side reset) is stored verbatim as an unknown error that must
    /// fail the close assertion. Timeouts/WouldBlock never reach here: the
    /// peer is still open and observation continues.
    fn classify_held_error(
        error: &std::io::Error,
        peer_reset: &Arc<AtomicBool>,
        peer_error: &Arc<Mutex<Option<String>>>,
    ) {
        if error.kind() == std::io::ErrorKind::ConnectionReset {
            peer_reset.store(true, Ordering::SeqCst);
            return;
        }
        let mut slot = peer_error.lock().unwrap();
        if slot.is_none() {
            *slot = Some(format!("{:?}: {error}", error.kind()));
        }
    }

    /// Publish the copied production resources, then replace ONLY the trusted
    /// source binding constants with the owned loopback fixture origin. The
    /// business functions and fixture bodies stay byte-identical.
    pub(super) fn prepare_binding(root: &Path, origin: &str, beta: Option<&str>) {
        let rss = root.join("rss");
        if !rss.is_dir() {
            m6::copy_tree(&Path::new(MANIFEST).join("rss"), &rss);
        }
        let path = rss.join("updater/github.rss");
        let source = std::fs::read_to_string(&path).expect("github.rss");
        let beta_line = match beta {
            Some(channel) => format!("beta_channel: \"{channel}\","),
            None => "beta_channel: null,".to_string(),
        };
        let source = source
            .replace(
                "api_origin: \"https://api.github.com\",",
                &format!("api_origin: \"{origin}\","),
            )
            .replace(
                "package_download_origin: \"https://github.com\",",
                &format!("package_download_origin: \"{origin}\","),
            )
            .replace("beta_channel: null,", &beta_line);
        std::fs::write(&path, source).unwrap();
    }

    fn spawn(root: &Path, args: &[&str]) -> Child {
        m6::base_command(root)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn the production binary")
    }

    fn log(root: &Path) -> String {
        std::fs::read_to_string(root.join("eh.log")).unwrap_or_default()
    }

    #[test]
    fn m7_binary_check_requests_native_stable_channel_and_logs_available() {
        let root = tempfile::tempdir().expect("root");
        let server = Server::start(200, COMMITS);
        prepare_binding(root.path(), &server.origin(), None);
        let mut child = spawn(
            root.path(),
            &["-l", "eh.log", "-vv", "--auto-update", "check"],
        );
        assert!(
            m6::wait_for(Duration::from_secs(20), || !server.requests().is_empty()),
            "the update check never reached the source binding; log:\n{}",
            log(root.path())
        );
        assert!(
            m6::wait_for(Duration::from_secs(20), || log(root.path())
                .contains("Update available")),
            "no availability log; log:\n{}",
            log(root.path())
        );
        nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGINT,
        )
        .expect("SIGINT");
        let status = m6::wait_exit(&mut child, Duration::from_secs(30)).expect("stopped");
        assert_eq!(status.code(), Some(0));
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "requests: {requests:?}");
        assert_eq!(requests[0].method, "GET");
        assert_eq!(
            requests[0].target,
            "/repos/fffonion/xeHentai/commits?sha=rss"
        );
        assert_eq!(requests[0].header("cookie"), None);
        assert_eq!(requests[0].header("authorization"), None);
        let log = log(root.path());
        assert!(log.contains("Update channel is: release"), "{log}");
        assert!(
            log.contains(
                "Update available: 2026-10-31T12:34:56Z \"first entry  with CRLF  third line\" (a1b2c3d4e5f60718293a4b5c6d7e8f9012345678)"
            ),
            "{log}"
        );
        assert!(
            log.contains("You can download update from https://dl.yooooo.us/share/xeHentai/"),
            "{log}"
        );
        assert!(!log.contains("Update is complete"), "{log}");
        assert!(log.contains("cleaning up..."), "{log}");
    }

    fn stop(child: &mut Child) -> i32 {
        let _ = nix::sys::signal::kill(
            nix::unistd::Pid::from_raw(child.id() as i32),
            nix::sys::signal::Signal::SIGINT,
        );
        m6::wait_exit(child, Duration::from_secs(30))
            .expect("the update owner must retire on the first signal")
            .code()
            .unwrap_or(-1)
    }

    fn wait_log(root: &Path, needle: &str) -> bool {
        m6::wait_for(Duration::from_secs(20), || log(root).contains(needle))
    }

    #[test]
    fn m7_binary_off_never_loads_the_update_module() {
        let root = tempfile::tempdir().expect("root");
        let server = Server::start(200, COMMITS);
        prepare_binding(root.path(), &server.origin(), None);
        // A deliberately uncompilable update module: `off` must never load it.
        std::fs::write(root.path().join("rss/updater/github.rss"), "fn broken( {").unwrap();
        let mut child = spawn(
            root.path(),
            &["-l", "eh.log", "-vv", "--auto-update", "off"],
        );
        assert!(
            wait_log(root.path(), "started."),
            "startup never logged: {}",
            log(root.path())
        );
        std::thread::sleep(Duration::from_millis(400));
        assert_eq!(stop(&mut child), 0);
        assert!(server.requests().is_empty(), "off must not request");
        let log = log(root.path());
        for needle in [
            "Update channel",
            "Update available",
            "Failure when updating",
            "update module",
        ] {
            assert!(!log.contains(needle), "off log contains {needle:?}: {log}");
        }
        assert!(log.contains("cleaning up..."), "{log}");
    }

    #[test]
    fn m7_binary_check_loads_the_real_update_module() {
        let root = tempfile::tempdir().expect("root");
        let server = Server::start(200, COMMITS);
        prepare_binding(root.path(), &server.origin(), None);
        std::fs::write(root.path().join("rss/updater/github.rss"), "fn broken( {").unwrap();
        let mut child = spawn(root.path(), &["-l", "eh.log", "--auto-update", "check"]);
        assert!(
            wait_log(root.path(), "github.rss compile error"),
            "the check path never loaded the module: {}",
            log(root.path())
        );
        assert_eq!(stop(&mut child), 0);
        assert!(server.requests().is_empty());
    }

    #[test]
    fn m7_binary_explicit_beta_binding_selects_that_channel() {
        let root = tempfile::tempdir().expect("root");
        let server = Server::start(200, COMMITS);
        prepare_binding(root.path(), &server.origin(), Some("rss-beta"));
        let mut child = spawn(
            root.path(),
            &[
                "-l",
                "eh.log",
                "-vv",
                "--auto-update",
                "check",
                "--update-beta-channel",
                "1",
            ],
        );
        assert!(
            m6::wait_for(Duration::from_secs(20), || log(root.path())
                .contains("Update available")),
            "no beta availability log: {}",
            log(root.path())
        );
        assert_eq!(stop(&mut child), 0);
        let requests = server.requests();
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(
            requests[0].target,
            "/repos/fffonion/xeHentai/commits?sha=rss-beta"
        );
        let log = log(root.path());
        assert!(log.contains("Update channel is: dev"), "{log}");
    }

    #[test]
    fn m7_binary_beta_without_explicit_binding_fails_without_request() {
        let root = tempfile::tempdir().expect("root");
        let server = Server::start(200, COMMITS);
        prepare_binding(root.path(), &server.origin(), None);
        let mut child = spawn(
            root.path(),
            &[
                "-l",
                "eh.log",
                "--auto-update",
                "check",
                "--update-beta-channel",
                "1",
            ],
        );
        assert!(
            wait_log(root.path(), "Failure when updating program"),
            "missing beta binding did not fail: {}",
            log(root.path())
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(stop(&mut child), 0);
        assert!(server.requests().is_empty(), "no fallback channel request");
        let log = log(root.path());
        assert!(!log.contains("Update available"), "{log}");
    }

    #[test]
    fn m7_binary_check_failure_classes_are_warnings() {
        const MISSING_DATE: &str = "[{\"sha\": \"a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\", \"commit\": {\"author\": {}, \"message\": \"m\"}}]";
        for (status, body) in [
            (500u16, COMMITS),
            (200, "[]"),
            (200, "{not valid json"),
            (200, MISSING_DATE),
        ] {
            let root = tempfile::tempdir().expect("root");
            let server = Server::start(status, body);
            prepare_binding(root.path(), &server.origin(), None);
            let mut child = spawn(root.path(), &["-l", "eh.log", "--auto-update", "check"]);
            assert!(
                wait_log(root.path(), "Failure when updating program"),
                "status {status} body {body:?} did not warn: {}",
                log(root.path())
            );
            assert_eq!(stop(&mut child), 0);
            let requests = server.requests();
            assert_eq!(requests.len(), 1, "status {status}: {requests:?}");
            let log = log(root.path());
            assert!(!log.contains("Update available"), "status {status}: {log}");
            assert!(
                !log.contains("Update is complete"),
                "status {status}: {log}"
            );
        }
    }

    #[test]
    fn m7_binary_transport_failure_is_a_warning_and_retires() {
        let root = tempfile::tempdir().expect("root");
        let server = Server::start_closing();
        prepare_binding(root.path(), &server.origin(), None);
        let mut child = spawn(root.path(), &["-l", "eh.log", "--auto-update", "check"]);
        assert!(
            wait_log(root.path(), "Failure when updating program"),
            "a closed transport did not warn: {}",
            log(root.path())
        );
        assert_eq!(stop(&mut child), 0);
        assert_eq!(server.requests().len(), 1);
        assert!(!log(root.path()).contains("Update available"));
    }

    /// `download` is no longer a check-only stub: it asks the source once,
    /// then requests the signed descriptor and its detached signature and
    /// refuses to publish a pending envelope when they are not a valid signed
    /// package. The complete download -> pending -> next-start path is proved
    /// by the `m7p` cases; this case keeps the binary-level refusal.
    #[test]
    fn m7_binary_download_mode_requires_a_signed_package() {
        let root = tempfile::tempdir().expect("root");
        let server = Server::start(200, COMMITS);
        prepare_binding(root.path(), &server.origin(), None);
        let mut child = spawn(
            root.path(),
            &["-l", "eh.log", "-vv", "--auto-update", "download"],
        );
        assert!(
            wait_log(root.path(), "Failure when updating program"),
            "an unsigned package was not refused: {}",
            log(root.path())
        );
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(stop(&mut child), 0);
        let requests = server.requests();
        let targets: Vec<&str> = requests
            .iter()
            .map(|request| request.target.as_str())
            .collect();
        assert_eq!(
            targets,
            vec![
                "/repos/fffonion/xeHentai/commits?sha=rss",
                "/fffonion/xeHentai/releases/download/rss-a1b2c3d4e5f60718293a4b5c6d7e8f9012345678/xeHentai-rss.zip.manifest.json",
                "/fffonion/xeHentai/releases/download/rss-a1b2c3d4e5f60718293a4b5c6d7e8f9012345678/xeHentai-rss.zip.sig"
            ],
            "unexpected requests"
        );
        assert!(
            !root
                .path()
                .join(".xehentai-update")
                .join("pending")
                .join("src.zip")
                .exists(),
            "an unverified package must not become pending"
        );
        let log = log(root.path());
        assert!(log.contains("Update available"), "{log}");
        assert!(!log.contains("Update is complete"), "{log}");
    }

    /// Overlap: the gallery task reaches its real terminal success state and the
    /// process exits naturally at URL completion while the update response body
    /// is still INCOMPLETE and never released; retirement of the owned update
    /// task then closes the request (real peer EOF).
    #[test]
    fn m7_binary_slow_check_does_not_block_gallery_work() {
        let root = tempfile::tempdir().expect("root");
        let update = Server::start_holding();
        prepare_binding(root.path(), &update.origin(), None);
        let gallery = m6::Gallery::start(4);
        let mut child = spawn(
            root.path(),
            &[
                "-p",
                &gallery.proxy(),
                "-d",
                "dl",
                "-l",
                "eh.log",
                "--auto-update",
                "check",
                "http://e-hentai.org/g/1/aaaa",
            ],
        );
        // Real barrier: the fixture already put the head and a PARTIAL body on
        // the wire before we assert anything about the gallery task.
        assert!(
            m6::wait_for(Duration::from_secs(20), || update.partial_flushed() > 0),
            "the held update fixture never flushed a partial response body"
        );
        assert!(
            !update.peer_closed(),
            "the update request was closed before any cancellation: eof={} reset={} err={:?}",
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error()
        );
        // The gallery task's actual production terminal state: the INFO log
        // emitted by `rename_and_finish` when the task is set FINISHED, plus its
        // downloaded artifacts — not merely "files exist".
        assert!(
            m6::wait_for(Duration::from_secs(30), || {
                let log = log(root.path());
                log.contains("download finished")
                    && m6::image_files(&root.path().join("dl")).len() == 4
            }),
            "the gallery task never finished while the check was held; log:\n{}",
            log(root.path())
        );
        let finished_log = log(root.path());
        assert_eq!(
            finished_log.matches(" download finished").count(),
            1,
            "{finished_log}"
        );
        assert!(!finished_log.contains(" error:"), "{finished_log}");
        assert!(
            !finished_log.contains("remaining these files undownloaded"),
            "{finished_log}"
        );
        assert!(
            child.try_wait().expect("try_wait").is_none(),
            "the process must not exit while the update body is still held"
        );
        // Both really overlap: the finished task left the update body gated.
        assert!(
            update.partial_flushed() > 0 && update.partial_flushed() < COMMITS.len(),
            "the update body was not still incomplete when the gallery finished"
        );
        assert!(
            !update.peer_closed(),
            "the update request must stay open while the gallery task ends: eof={} reset={} err={:?}",
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error()
        );
        assert_eq!(update.requests().len(), 1, "one held commits request");
        // Normal URL-completion auto-exit with the body STILL gated: the exit
        // path retires the owned update task, which closes the connection.
        let status =
            m6::wait_exit(&mut child, Duration::from_secs(30)).expect("URL completion exit");
        assert_eq!(status.code(), Some(0));
        assert!(
            update.partial_flushed() < COMMITS.len(),
            "the fixture body must never be released to make the app exit"
        );
        // Reaping the child does not synchronize with the fixture observer's
        // next socket read: the exit closes the socket, while the separate
        // observer thread publishes EOF/reset only on its own poll. Wait on the
        // SAME real closure through the bounded observation helper instead of
        // sampling the atomics on the reaping instant.
        assert!(
            m6::wait_for(Duration::from_secs(3), || update.peer_closed()),
            "retiring the owned update task must close the held request: eof={} reset={} err={:?} stopped={}",
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped()
        );
        assert_eq!(
            update.peer_error(),
            None,
            "the held close is an unknown transport error, not EOF/reset"
        );
        let finished_log = log(root.path());
        assert!(
            finished_log.contains(" download finished"),
            "{finished_log}"
        );
        assert!(
            !finished_log.contains("Failure when updating"),
            "{finished_log}"
        );
        assert!(!finished_log.contains("Update available"), "{finished_log}");
        assert!(gallery.paths().iter().any(|path| path.contains("/s/")));
        println!(
            "V1 overlap: body_bytes={} partial_flushed={} released={} peer_eof={} peer_reset={} peer_error={:?} observer_stopped={} peer_closed={} exit={:?} finished_records={}",
            COMMITS.len(),
            update.partial_flushed(),
            update.partial_flushed() >= COMMITS.len(),
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped(),
            update.peer_closed(),
            status.code(),
            finished_log.matches(" download finished").count()
        );
    }

    /// First signal while the response body is INCOMPLETE: the owned check task
    /// is cancelled, the request closes at the peer, and the incomplete body is
    /// never completed afterwards.
    #[test]
    fn m7_binary_first_signal_cancels_the_held_check_and_closes_the_request() {
        let root = tempfile::tempdir().expect("root");
        let update = Server::start_holding();
        prepare_binding(root.path(), &update.origin(), None);
        let mut child = spawn(root.path(), &["-l", "eh.log", "--auto-update", "check"]);
        assert!(
            m6::wait_for(Duration::from_secs(20), || update.partial_flushed() > 0),
            "the held fixture never flushed a partial response body"
        );
        assert!(
            !update.peer_closed(),
            "the update request was closed before the signal: eof={} reset={} err={:?}",
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error()
        );
        let code = stop(&mut child);
        assert_eq!(code, 0);
        assert!(
            m6::wait_for(Duration::from_secs(10), || update.peer_closed()),
            "the cancelled update request kept its connection open: eof={} reset={} err={:?} stopped={}",
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped()
        );
        assert_eq!(
            update.peer_error(),
            None,
            "the cancelled close is an unknown transport error, not EOF/reset"
        );
        assert!(
            update.partial_flushed() < COMMITS.len(),
            "cancellation completed the body instead of aborting it"
        );
        let log = log(root.path());
        assert!(log.contains("cleaning up..."), "{log}");
        assert!(!log.contains("Update available"), "{log}");
        println!(
            "V1 cancel: body_bytes={} partial_flushed={} released={} peer_eof={} peer_reset={} peer_error={:?} observer_stopped={} peer_closed={} exit={:?}",
            COMMITS.len(),
            update.partial_flushed(),
            update.partial_flushed() >= COMMITS.len(),
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped(),
            update.peer_closed(),
            code
        );
    }

    /// Raw held client: one real request head, then the caller owns the open
    /// socket. The server has already flushed a partial body by the time the
    /// head is readable.
    fn held_raw_client(port: u16) -> TcpStream {
        let mut socket = TcpStream::connect(("127.0.0.1", port)).expect("connect the held fixture");
        socket
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        socket
            .write_all(b"GET /held HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .expect("held request head");
        socket
    }

    /// Read until the real response head is fully on the wire.
    fn read_held_head(socket: &mut TcpStream) -> Vec<u8> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 4096];
        loop {
            match socket.read(&mut chunk) {
                Ok(0) => break,
                Ok(read) => {
                    buffer.extend_from_slice(&chunk[..read]);
                    if buffer.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    break;
                }
                Err(error) => panic!("held fixture read: {error}"),
            }
            assert!(buffer.len() <= 64 * 1024, "held head too large");
        }
        assert!(
            buffer.windows(4).any(|window| window == b"\r\n\r\n"),
            "the held fixture never sent a response head"
        );
        buffer
    }

    /// Deterministic FIN control on the SAME held observer the m7 tests use:
    /// a raw client reads the head, then closes cleanly. Only EOF may be set.
    #[test]
    fn m7_held_observer_records_clean_fin_as_eof_only() {
        let update = Server::start_holding();
        let mut client = held_raw_client(update.port);
        let head = read_held_head(&mut client);
        assert!(
            m6::wait_for(Duration::from_secs(20), || update.partial_flushed() > 0),
            "the held fixture never flushed a partial response body"
        );
        assert!(!head.is_empty());
        assert!(
            !update.peer_closed(),
            "the held request closed before the clean FIN"
        );
        drop(client);
        assert!(
            m6::wait_for(Duration::from_secs(10), || update.peer_eof()),
            "a clean FIN was not observed as EOF: reset={} err={:?} stopped={}",
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped()
        );
        assert!(!update.peer_reset(), "a clean FIN set the reset flag");
        assert_eq!(update.peer_error(), None, "a clean FIN stored an error");
        assert!(
            update.partial_flushed() < COMMITS.len(),
            "the control body was released"
        );
        println!(
            "V1 observer-fin: body_bytes={} partial_flushed={} peer_eof={} peer_reset={} peer_error={:?} observer_stopped={} peer_closed={}",
            COMMITS.len(),
            update.partial_flushed(),
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped(),
            update.peer_closed()
        );
    }

    /// Abort a held socket with a real TCP RST (Unix SO_LINGER 0 on close).
    #[cfg(unix)]
    fn abort_with_reset(socket: TcpStream) {
        use std::os::unix::io::AsRawFd;
        let linger = nix::libc::linger {
            l_onoff: 1,
            l_linger: 0,
        };
        let result = unsafe {
            nix::libc::setsockopt(
                socket.as_raw_fd(),
                nix::libc::SOL_SOCKET,
                nix::libc::SO_LINGER,
                &linger as *const _ as *const nix::libc::c_void,
                std::mem::size_of::<nix::libc::linger>() as nix::libc::socklen_t,
            )
        };
        assert_eq!(result, 0, "SO_LINGER for the reset control");
        drop(socket);
    }

    /// Deterministic reset control on the SAME held observer: a raw client
    /// reads the head, then RSTs. Only the reset flag may be set — never EOF.
    #[cfg(unix)]
    #[test]
    fn m7_held_observer_records_explicit_reset_only() {
        let update = Server::start_holding();
        let mut client = held_raw_client(update.port);
        read_held_head(&mut client);
        assert!(
            m6::wait_for(Duration::from_secs(20), || update.partial_flushed() > 0),
            "the held fixture never flushed a partial response body"
        );
        assert!(
            !update.peer_closed(),
            "the held request closed before the reset"
        );
        abort_with_reset(client);
        assert!(
            m6::wait_for(Duration::from_secs(10), || update.peer_reset()),
            "an explicit RST was not observed as a reset: eof={} err={:?} stopped={}",
            update.peer_eof(),
            update.peer_error(),
            update.observer_stopped()
        );
        assert!(!update.peer_eof(), "an explicit RST set the EOF flag");
        assert_eq!(update.peer_error(), None, "an explicit RST stored an error");
        assert!(
            update.partial_flushed() < COMMITS.len(),
            "the control body was released"
        );
        assert!(
            update.peer_closed(),
            "a reset must count as closed: eof={} reset={} err={:?}",
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error()
        );
        println!(
            "V1 observer-rst: body_bytes={} partial_flushed={} peer_eof={} peer_reset={} peer_error={:?} observer_stopped={} peer_closed={}",
            COMMITS.len(),
            update.partial_flushed(),
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped(),
            update.peer_closed()
        );
    }

    /// Deterministic still-open control on the SAME held observer: a raw
    /// client reads the head and STAYS open. Neither EOF nor reset may be set,
    /// so the request must NOT count as closed — an open peer is genuinely
    /// open, never a close.
    #[test]
    fn m7_held_observer_open_peer_counts_as_open() {
        let update = Server::start_holding();
        let mut client = held_raw_client(update.port);
        read_held_head(&mut client);
        assert!(
            m6::wait_for(Duration::from_secs(20), || update.partial_flushed() > 0),
            "the held fixture never flushed a partial response body"
        );
        // Bounded negative control: while a real client still holds the socket
        // open, the SAME bounded closure observation must stay false — waiting
        // for a close can never manufacture one.
        assert!(
            !m6::wait_for(Duration::from_secs(1), || update.peer_closed()),
            "an open peer counted as closed during a bounded close observation: eof={} reset={} err={:?} stopped={}",
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped()
        );
        assert!(
            !update.peer_closed(),
            "an open peer counted as closed: eof={} reset={} err={:?}",
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error()
        );
        assert_eq!(
            update.peer_error(),
            None,
            "an open peer stored an error: {:?}",
            update.peer_error()
        );
        assert!(
            update.partial_flushed() > 0 && update.partial_flushed() < COMMITS.len(),
            "the control body was not still incomplete"
        );
        println!(
            "V1 observer-open: body_bytes={} partial_flushed={} peer_eof={} peer_reset={} peer_error={:?} observer_stopped={} peer_closed={}",
            COMMITS.len(),
            update.partial_flushed(),
            update.peer_eof(),
            update.peer_reset(),
            update.peer_error(),
            update.observer_stopped(),
            update.peer_closed()
        );
        drop(client);
        assert!(
            m6::wait_for(Duration::from_secs(10), || update.peer_eof()),
            "the open control never closed after the clean FIN"
        );
    }

    #[test]
    fn m7_module_same_id_and_rss_channel_change_change_real_requests() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let local = tokio::task::LocalSet::new();
        runtime.block_on(local.run_until(async {
            let root = tempfile::tempdir().expect("root");
            let server = Server::start(200, COMMITS);
            prepare_binding(root.path(), &server.origin(), None);
            let mut updater = crate::updater::Updater::load(root.path()).expect("load");
            let report = updater
                .check("check", false, Some(UPDATE_ID), "xeHentai/test")
                .await
                .expect("check");
            assert_eq!(report.outcome, "up_to_date");
            assert_eq!(report.events.len(), 1);
            assert_eq!(report.events[0].key, "UPDATE_NO_UPDATE");
            let available = updater
                .check("check", false, Some("different-id"), "xeHentai/test")
                .await
                .expect("check");
            assert_eq!(available.outcome, "available");
            let info = available.update.expect("update info");
            assert_eq!(info.update_id, UPDATE_ID);
            assert_eq!(info.ts, "2026-10-31T12:34:56Z");
            assert_eq!(info.message, "first entry  with CRLF  third line");
            assert!(info.download_link.ends_with("/xeHentai-rss.zip"));
            assert!(!info.download_link.contains("archive"));
            let requests = server.requests();
            assert_eq!(requests.len(), 2, "{requests:?}");

            // Change ONLY the RSS source binding channel: the real VM must ask
            // for the new channel instead of a native-coded version.
            let path = root.path().join("rss/updater/github.rss");
            let source = std::fs::read_to_string(&path).unwrap().replace(
                "stable_channel: \"rss\",",
                "stable_channel: \"rss-choice\",",
            );
            std::fs::write(&path, source).unwrap();
            let mut changed = crate::updater::Updater::load(root.path()).expect("reload");
            let report = changed
                .check("check", false, None, "xeHentai/test")
                .await
                .expect("check");
            assert_eq!(report.outcome, "available");
            let requests = server.requests();
            assert_eq!(requests.len(), 3, "{requests:?}");
            assert_eq!(
                requests[2].target,
                "/repos/fffonion/xeHentai/commits?sha=rss-choice"
            );
        }));
    }

    /// B1: a DIRECT `check("off")` is not the CLI gate. The legacy
    /// `check_update(off)` still asks the source once, compares the common
    /// update id, and prints the manual-download hint; only the CLI caller
    /// suppresses the module. Oracle: u2_off_stable_diff.
    #[test]
    fn m7_module_direct_off_check_requests_and_hints_manually() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let local = tokio::task::LocalSet::new();
        runtime.block_on(local.run_until(async {
            let root = tempfile::tempdir().expect("root");
            let server = Server::start(200, COMMITS);
            prepare_binding(root.path(), &server.origin(), None);
            let mut updater = crate::updater::Updater::load(root.path()).expect("load");
            let report = updater
                .check("off", false, None, "xeHentai/test")
                .await
                .expect("direct off check");
            let requests = server.requests();
            assert_eq!(
                requests.len(),
                1,
                "a direct off check must query the source once: {requests:?}"
            );
            assert_eq!(
                requests[0].target,
                "/repos/fffonion/xeHentai/commits?sha=rss"
            );
            assert_eq!(requests[0].method, "GET");
            assert_eq!(requests[0].header("cookie"), None);
            assert_eq!(requests[0].header("authorization"), None);
            assert!(
                requests
                    .iter()
                    .all(|request| !request.target.contains("archive")
                        && !request.target.ends_with(".zip")),
                "a direct off check must not fetch an archive: {requests:?}"
            );
            assert_eq!(report.outcome, "available");
            let info = report.update.clone().expect("typed update info");
            assert_eq!(info.update_id, UPDATE_ID);
            assert_eq!(info.ts, "2026-10-31T12:34:56Z");
            assert_eq!(info.message, "first entry  with CRLF  third line");
            assert!(info.download_link.ends_with("/xeHentai-rss.zip"));
            let keys: Vec<&str> = report
                .events
                .iter()
                .map(|event| event.key.as_str())
                .collect();
            assert_eq!(keys, vec!["UPDATE_AVAILABLE", "UPDATE_DOWNLOAD_MANUALLY"]);
            assert_eq!(
                report.events[0].args,
                vec![
                    "2026-10-31T12:34:56Z".to_string(),
                    "first entry  with CRLF  third line".to_string(),
                    UPDATE_ID.to_string(),
                ]
            );

            // The common same-id comparison still applies under `off`: no
            // availability or manual-hint log, but the source is queried again.
            let same = updater
                .check("off", false, Some(UPDATE_ID), "xeHentai/test")
                .await
                .expect("same-id off check");
            assert_eq!(same.outcome, "up_to_date");
            let keys: Vec<&str> = same.events.iter().map(|event| event.key.as_str()).collect();
            assert_eq!(keys, vec!["UPDATE_NO_UPDATE"]);
            assert!(same.update.is_some());
            let requests = server.requests();
            assert_eq!(requests.len(), 2, "same-id off still queries: {requests:?}");
            assert_eq!(
                requests[1].target,
                "/repos/fffonion/xeHentai/commits?sha=rss"
            );
        }));
    }
}

// M7 remaining scope: signed package, pending and next-start activation.
// POSIX signals and real child processes, so Unix only.
#[cfg(unix)]
mod m7p;
