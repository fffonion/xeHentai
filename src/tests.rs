//! Cross-module and process-level tests.

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
        other => panic!("unknown probe suite {other}"),
    };
    let output = std::env::var("XEH_COMPAT_OUTPUT").expect("probe output");
    fs::write(
        output,
        serde_json::to_string_pretty(&result).expect("probe JSON"),
    )
    .expect("write probe output");
}

#[test]
fn example_configuration_is_a_valid_default_module() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let source = fs::read_to_string(root.join("config.example.rss")).unwrap();
    let config = Config::decode_rss(&source).expect("example must load through the SDK");
    assert_eq!(config, Config::default());
}
