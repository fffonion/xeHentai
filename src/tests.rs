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

#[test]
fn session_directory_must_be_authorized() {
    let root = tempfile::tempdir().expect("root");
    let mut app = app_with_config(
        root.path(),
        session_config(root.path(), true),
        &["guid0001"],
    )
    .expect("app");
    let (_, guid) = app.add_task(GALLERY_URL, serde_json::Map::new()).unwrap();
    let guid = guid.unwrap();
    app.task_mut(&guid)
        .unwrap()
        .meta
        .insert("title".into(), serde_json::json!("T"));
    assert_eq!(app.save_session().0, 0);
    let raw = fs::read_to_string(root.path().join("h.json")).unwrap();
    let hostile = raw.replace(
        &root.path().join("downloads").to_string_lossy().into_owned(),
        "/definitely/not/authorized",
    );
    fs::write(root.path().join("h.json"), hostile).unwrap();
    let error = app_with_config(root.path(), session_config(root.path(), true), &[])
        .expect_err("hostile directory");
    assert!(
        matches!(
            error,
            crate::core::AppError::Session(crate::core::SessionError::DirNotAuthorized { .. })
        ),
        "{error}"
    );
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
        AppError::Session(SessionError::DirNotAuthorized { .. }) => "UnsafePathDelta",
        AppError::Session(SessionError::Unsafe { .. }) => "UnsafePathDelta",
        AppError::Session(SessionError::Json(_)) => "JSONDecodeError",
        AppError::Session(SessionError::Io { .. }) => "OSError",
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

    let auth_dir = m2_scenario(root, "dir_authorization");
    let mut auth_config = m2_config(&auth_dir);
    auth_config.dir = "elsewhere".into();
    let mut auth_app = crate::core::App::with_guid_source(
        auth_config,
        m2_logger(),
        &auth_dir,
        &auth_dir,
        Box::new(crate::core::FixedGuids::new(["00000001".to_string()])),
    )
    .unwrap();
    let auth_guid = auth_app
        .add_task(GALLERY_URL, serde_json::Map::new())
        .unwrap()
        .1
        .unwrap();
    {
        let task = auth_app.task_mut(&auth_guid).unwrap();
        let mut meta = serde_json::Map::new();
        meta.insert("gnname".into(), serde_json::json!("T"));
        meta.insert("gjname".into(), serde_json::Value::Null);
        meta.insert("total".into(), serde_json::json!(1));
        task.update_meta(meta).unwrap();
    }
    auth_app.save_session();
    let auth_result = crate::core::App::with_guid_source(
        m2_config(&auth_dir),
        m2_logger(),
        &auth_dir,
        &auth_dir,
        Box::new(crate::core::FixedGuids::new(Vec::new())),
    );
    let loader = match auth_result {
        Ok(app) => serde_json::json!({
            "ok": true,
            "value": app.task(&auth_guid).unwrap().get_fpath().unwrap()
                .strip_prefix(&auth_dir).unwrap().to_string_lossy().replace('\\', "/"),
        }),
        Err(error) => serde_json::json!({"ok": false, "error": m2_app_error_name(&error)}),
    };
    result.insert(
        "dir_authorization".into(),
        serde_json::json!({"loader": loader}),
    );

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
