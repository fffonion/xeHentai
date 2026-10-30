//! Cross-version remaining-image end-to-end over one real upstream session.
//!
//! The `h.json` input is written by the pinned upstream `save_session` through
//! the shared oracle runner; this test never rewrites or normalizes it. The
//! source task carries the already-downloaded fid 1 plus the missing fids 11
//! and 12 in `img_q`/`page_q`/`reload_map`, so the real RSS done set and
//! `remaining_plan` must filter the completed queue member before the thin
//! driver submits anything. The local HTTP fixture answers all three routes;
//! only the two missing fids may be requested.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use xeh_host::{
    ControlOptions, HeaderEntry, JobKey, PoolConfig, SessionOptions, TaskControl, TransferRequest,
    TransportHooks, WorkerPool,
};
use xeh_xtask::{load_produced_session, produce_remaining_session};

const GALLERY: &str = "https://e-hentai.org/g/999999/ffffffffff/";
const PAGE_HASH_ONE: &str = "0123456789";
const PAGE_HASH_ELEVEN: &str = "1122334455";
const PAGE_HASH_TWELVE: &str = "aabbccddee";
const FULL_HASH_ONE: &str = "0123456789abcdef0123456789abcdef01234567";
const FULL_HASH_ELEVEN: &str = "1122334455abcdef0123456789abcdef01234567";
const FULL_HASH_TWELVE: &str = "aabbccddeeabcdef0123456789abcdef01234567";

struct CaseRoot {
    path: PathBuf,
}

impl CaseRoot {
    fn create() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0);
        let path = xeh_xtask::lease_tmp()
            .expect("XEH_LEASE_TMP is required for the session E2E")
            .join(format!("session-e2e-{}-{nanos}", std::process::id()));
        fs::create_dir(&path).expect("create case root");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for CaseRoot {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn evidence_dir() -> PathBuf {
    match std::env::var_os("XEH_EVIDENCE_DIR") {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => xeh_xtask::lease_tmp()
            .expect("XEH_LEASE_TMP is required")
            .join("evidence-task6-r1"),
    }
}

fn write_evidence(run: &Path, name: &str, value: &Value) {
    let _ = fs::create_dir_all(run);
    let text = serde_json::to_string_pretty(value).unwrap();
    let _ = fs::write(run.join(name), text + "\n");
}

fn parse(text: &str, what: &str) -> Value {
    serde_json::from_str(text).unwrap_or_else(|error| panic!("{what} is not JSON: {error}\n{text}"))
}

fn invoke(control: &mut TaskControl, request: Value) -> Value {
    let text = control
        .invoke(&request.to_string())
        .unwrap_or_else(|error| panic!("command {request} failed: {error}"));
    parse(&text, "command report")
}

fn load(control: &mut TaskControl) -> Value {
    let text = control.load_session().expect("load session");
    parse(&text, "load report")
}

fn cookie_pairs(report: &Value) -> Value {
    Value::Array(
        report["cookies"]
            .as_array()
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|cookie| json!([cookie["name"].clone(), cookie["value"].clone()]))
            .collect(),
    )
}

fn disk_entries(downloads: &Path) -> Value {
    let mut entries: Vec<(String, u64)> = Vec::new();
    let mut stack = vec![downloads.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(read) = fs::read_dir(&directory) else {
            continue;
        };
        for entry in read.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if let Ok(metadata) = fs::metadata(&path) {
                entries.push((
                    path.strip_prefix(downloads)
                        .unwrap_or(&path)
                        .to_string_lossy()
                        .to_string(),
                    metadata.len(),
                ));
            }
        }
    }
    entries.sort();
    Value::Array(
        entries
            .into_iter()
            .map(|(path, size)| json!([path, size]))
            .collect(),
    )
}

struct FixtureServer {
    port: u16,
    seen: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: JoinHandle<()>,
}

fn serve_routes(routes: Vec<(String, Vec<u8>)>) -> FixtureServer {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");
    let port = listener.local_addr().unwrap().port();
    let map: BTreeMap<String, Vec<u8>> = routes.into_iter().collect();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let log = Arc::clone(&seen);
    let stop_flag = Arc::clone(&stop);
    let handle = thread::spawn(move || {
        while !stop_flag.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buffer = [0u8; 4096];
                    let read = stream.read(&mut buffer).unwrap_or(0);
                    let request = String::from_utf8_lossy(&buffer[..read]).to_string();
                    let path = request
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .unwrap_or("")
                        .to_string();
                    log.lock().unwrap().push(path.clone());
                    match map.get(&path) {
                        Some(body) => {
                            let head = format!(
                                "HTTP/1.1 200 OK\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                body.len()
                            );
                            let _ = stream.write_all(head.as_bytes());
                            let _ = stream.write_all(body);
                        }
                        None => {
                            let _ = stream.write_all(
                                b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                            );
                        }
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => panic!("fixture accept failed: {error}"),
            }
        }
    });
    FixtureServer {
        port,
        seen,
        stop,
        handle,
    }
}
#[test]
fn remaining_downloads_use_the_pinned_upstream_session_and_filter_completed_fid() {
    assert_eq!(
        xeh_host::SESSION_READ_LIMIT_BYTES,
        16 * 1024 * 1024,
        "the session budget is the reviewed 16 MiB constant"
    );
    let root = CaseRoot::create();
    let root_path = root.path();
    let ev = evidence_dir().join(format!(
        "session-e2e-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or(0)
    ));
    fs::create_dir_all(&ev).expect("evidence dir");

    let session_dir = root_path.join("session");
    let legacy_dir = root_path.join("legacy");
    for dir in [&session_dir, &legacy_dir] {
        fs::create_dir(dir).expect("case dir");
    }

    let page_one = format!("https://e-hentai.org/s/{PAGE_HASH_ONE}/999999-1");
    let page_eleven = format!("https://e-hentai.org/s/{PAGE_HASH_ELEVEN}/999999-11");
    let page_twelve = format!("https://e-hentai.org/s/{PAGE_HASH_TWELVE}/999999-12");
    let payload_one = b"known-eleven-payload".to_vec();
    let payload_two = b"known-twelve-payload".to_vec();
    let route_one = format!("/{FULL_HASH_ELEVEN}-11-1-1-jpg");
    let route_two = format!("/{FULL_HASH_TWELVE}-12-1-1-jpg");
    let server = serve_routes(vec![
        (
            format!("/{FULL_HASH_ONE}-1-1-1-jpg"),
            b"fid-one-must-not-be-requested".to_vec(),
        ),
        (route_one.clone(), payload_one.clone()),
        (route_two.clone(), payload_two.clone()),
    ]);
    let port = server.port;
    let seen = Arc::clone(&server.seen);
    let image_one = format!("http://127.0.0.1:{port}/{FULL_HASH_ONE}-1-1-1-jpg");
    let image_eleven = format!("http://127.0.0.1:{port}/{FULL_HASH_ELEVEN}-11-1-1-jpg");
    let image_twelve = format!("http://127.0.0.1:{port}/{FULL_HASH_TWELVE}-12-1-1-jpg");
    let reload_map = json!({
        PAGE_HASH_ONE: [page_one.clone(), "renamed-one.jpg"],
        PAGE_HASH_ELEVEN: [page_eleven.clone(), "renamed-eleven.jpg"],
        PAGE_HASH_TWELVE: [page_twelve.clone(), "renamed-twelve.jpg"],
    });
    let record = json!({
        "url": GALLERY,
        "failcode": 0,
        "state": 10,
        "guid": "partial1",
        "config": {"dir": "{downloads}", "save_tasks": true, "download_range": null},
        "meta": {"total": 12, "title": "partial", "finished": 0, "filelist": {}},
        "reload_map": reload_map,
        "duplicate_map": {},
        "renamed_map": {},
        "img_q": [image_one, image_eleven, image_twelve],
        "page_q": [page_one, page_eleven, page_twelve]
    });

    let produced = produce_remaining_session(
        root_path,
        record,
        json!([{"path": "partial/01.jpg", "text": "known-fid-one-payload"}]),
    )
    .unwrap_or_else(|error| panic!("upstream producer: {error}"));
    let downloads = produced.case_root.join("downloads");
    let partial = downloads.join("partial");
    assert_eq!(
        fs::read(partial.join("01.jpg")).unwrap(),
        b"known-fid-one-payload",
        "the producer's own case disk carries the downloaded fid"
    );
    let produced_bytes = fs::read(&produced.session_path).expect("produced h.json");
    write_evidence(&ev, "producer.json", &produced.record);
    fs::write(ev.join("produced-h.json"), &produced_bytes).unwrap();
    let raw = String::from_utf8(produced_bytes.clone()).expect("produced UTF-8");
    assert!(
        raw.contains(FULL_HASH_ONE) && raw.contains("-11-") && raw.contains("-12-"),
        "the produced file must carry the completed and missing fids: {raw}"
    );

    let options = SessionOptions::new(
        produced.session_path.clone(),
        legacy_dir.join(".ehentai.cookie"),
        vec![downloads.clone()],
    );
    let mut control =
        TaskControl::open_with_session(&ControlOptions::fixture(&[], 0), Some(&options))
            .expect("open session control");
    let loaded = load(&mut control);
    write_evidence(&ev, "rss-load.json", &loaded);
    assert_eq!(loaded["session"]["code"], json!(0), "load: {loaded}");
    assert_eq!(loaded["tasks"].as_array().map(Vec::len), Some(1));
    let task = &loaded["tasks"][0];
    assert_eq!(task["guid"], json!("partial1"));
    assert_eq!(task["state"], json!(4), "page_q moves download to scan_img");
    assert_eq!(
        task["meta"]["finished"],
        json!(1),
        "fid 1 is already on disk"
    );
    assert_eq!(loaded["queue"], json!(["partial1"]));
    assert_eq!(loaded["cookie_header"], json!("nw=1"));

    let plan_report = invoke(
        &mut control,
        json!({"op": "remaining_plan", "guid": "partial1"}),
    );
    assert_eq!(plan_report["ok"], json!(true), "plan: {plan_report}");
    let plan = parse(plan_report["value"].as_str().expect("plan text"), "plan");
    write_evidence(&ev, "rss-plan.json", &plan);
    let items = plan["items"].as_array().expect("plan items");
    assert_eq!(items.len(), 2, "the completed fid must be filtered: {plan}");
    assert_eq!(items[0]["fid"], json!(11));
    assert_eq!(items[1]["fid"], json!(12));
    assert_eq!(items[0]["url"], json!(image_eleven));
    assert_eq!(items[1]["url"], json!(image_twelve));
    assert_eq!(
        items[0]["file"],
        json!(partial.join("11.jpg").to_string_lossy())
    );
    assert_eq!(
        items[0]["staging"],
        json!(partial.join("11.jpg.xehpart").to_string_lossy())
    );

    let http = vm::HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(20),
        stream_idle_timeout: Duration::from_secs(5),
        max_stream_duration: Duration::from_secs(20),
        ..vm::HttpConfig::default()
    };
    let pool = WorkerPool::start(PoolConfig {
        workers: 2,
        job_capacity: 4,
        event_capacity: 8,
        http,
        root: downloads.clone(),
        chunk_bytes: 4096,
        stream_limit: vm::HttpConfig::default().max_stream_total_bytes,
        writer_fault: None,
        commit_gate: None,
        link_gate: None,
        commit_hold: None,
        hooks: TransportHooks::default(),
        fail_worker_once: false,
        submit_hold: None,
    })
    .expect("worker pool");
    pool.wait_ready(Duration::from_secs(5)).expect("pool ready");
    let lease = pool.register(1, 1).expect("register");
    for (index, item) in items.iter().enumerate() {
        let staging = PathBuf::from(item["staging"].as_str().expect("staging"));
        let relative = staging
            .strip_prefix(&downloads)
            .expect("staging under downloads")
            .to_string_lossy()
            .to_string();
        pool.submit(lease.submission(TransferRequest {
            key: JobKey {
                task_id: 1,
                epoch: 1,
                job_id: index as i64 + 1,
            },
            url: item["url"].as_str().expect("url").to_string(),
            headers: Vec::<HeaderEntry>::new(),
            relative_path: relative,
        }))
        .expect("submit download");
    }
    let terminals = pool
        .wait_terminals(2, Duration::from_secs(30))
        .expect("terminals");
    for terminal in &terminals {
        assert_eq!(
            terminal.terminal_status.as_deref(),
            Some("completed"),
            "{terminal:?}"
        );
    }
    let fs_state = xeh_host::os::fs::FsState::new(vec![downloads.clone()], None, None);
    for item in items {
        let staging = PathBuf::from(item["staging"].as_str().unwrap());
        let file = PathBuf::from(item["file"].as_str().unwrap());
        let renamed = xeh_host::os::fs::rename_atomic(&fs_state, &staging, &file).expect("rename");
        assert!(renamed.committed(), "rename failed: {:?}", renamed.error());
        assert!(renamed.directory_synced());
    }
    pool.shutdown().expect("shutdown");
    server.stop.store(true, Ordering::Release);
    server.handle.join().expect("server");

    let requested = seen.lock().unwrap().clone();
    write_evidence(&ev, "requests.json", &json!(requested));
    assert_eq!(
        requested.len(),
        2,
        "no request for the finished fid: {requested:?}"
    );
    assert!(
        requested.iter().any(|path| path == &route_one)
            && requested.iter().any(|path| path == &route_two),
        "the two missing fids were actually requested: {requested:?}"
    );
    assert_eq!(
        fs::read(partial.join("01.jpg")).unwrap(),
        b"known-fid-one-payload",
        "the completed fid's bytes are untouched"
    );
    assert_eq!(fs::read(partial.join("11.jpg")).unwrap(), payload_one);
    assert_eq!(fs::read(partial.join("12.jpg")).unwrap(), payload_two);
    assert!(!partial.join("11.jpg.xehpart").exists());
    assert!(!partial.join("12.jpg.xehpart").exists());
    let disk = disk_entries(&downloads);
    write_evidence(&ev, "disk.json", &disk);

    // A fresh owner rescans the same layout: all three fids finished and no
    // remaining plan.
    let mut rescanned =
        TaskControl::open_with_session(&ControlOptions::fixture(&[], 0), Some(&options))
            .expect("rescan control");
    let rescan = load(&mut rescanned);
    write_evidence(&ev, "rss-rescan.json", &rescan);
    assert_eq!(rescan["session"]["code"], json!(0), "rescan: {rescan}");
    assert_eq!(rescan["tasks"][0]["guid"], json!("partial1"));
    assert_eq!(rescan["tasks"][0]["meta"]["finished"], json!(3));
    assert_eq!(rescan["tasks"][0]["state"], json!(4));
    let empty_plan = invoke(
        &mut rescanned,
        json!({"op": "remaining_plan", "guid": "partial1"}),
    );
    assert_eq!(empty_plan["ok"], json!(true), "empty plan: {empty_plan}");
    let empty = parse(empty_plan["value"].as_str().unwrap(), "empty plan");
    assert_eq!(empty["items"], json!([]));

    // The pinned upstream implementation loads the same produced document on
    // the same on-disk layout before the RSS save.
    let upstream_rescan = load_produced_session(root_path).expect("upstream rescan");
    write_evidence(&ev, "upstream-rescan.json", &upstream_rescan);
    assert_eq!(
        upstream_rescan["tasks"][0]["guid"],
        rescan["tasks"][0]["guid"]
    );
    assert_eq!(
        upstream_rescan["tasks"][0]["state"],
        rescan["tasks"][0]["state"]
    );
    assert_eq!(
        upstream_rescan["tasks"][0]["meta"]["finished"],
        rescan["tasks"][0]["meta"]["finished"]
    );
    assert_eq!(upstream_rescan["queue"], rescan["queue"]);
    assert_eq!(upstream_rescan["cookies"], cookie_pairs(&rescan));
    assert_eq!(upstream_rescan["header"], rescan["cookie_header"]);
    assert_eq!(upstream_rescan["login"], rescan["has_login"]);
    assert_eq!(upstream_rescan["disk"], disk);

    let updated = invoke(
        &mut rescanned,
        json!({"op": "update", "overlay": {"save_tasks": true}}),
    );
    assert_eq!(updated["code"], json!(0), "save_tasks update: {updated}");
    let saved_text = rescanned.save_session().expect("save session");
    let saved = parse(&saved_text, "save report");
    write_evidence(&ev, "rss-save.json", &saved);
    assert_eq!(saved["session"]["code"], json!(0), "save: {saved}");
    assert_eq!(saved["session"]["committed"], json!(true));

    let mut third =
        TaskControl::open_with_session(&ControlOptions::fixture(&[], 0), Some(&options))
            .expect("third control");
    let third_report = load(&mut third);
    write_evidence(&ev, "rss-third.json", &third_report);
    assert_eq!(
        third_report["session"]["code"],
        json!(0),
        "third: {third_report}"
    );
    assert_eq!(third_report["tasks"][0]["guid"], json!("partial1"));
    assert_eq!(third_report["tasks"][0]["meta"]["finished"], json!(3));
    assert_eq!(third_report["queue"], json!(["partial1"]));
    assert_eq!(third_report["cookie_header"], json!("nw=1"));

    // The same on-disk layout is loaded by the pinned upstream implementation.
    let upstream = load_produced_session(root_path).expect("upstream load on same layout");
    write_evidence(&ev, "upstream-load.json", &upstream);
    assert_eq!(
        upstream["tasks"][0]["guid"],
        third_report["tasks"][0]["guid"]
    );
    assert_eq!(
        upstream["tasks"][0]["state"],
        third_report["tasks"][0]["state"]
    );
    assert_eq!(
        upstream["tasks"][0]["meta"]["finished"],
        third_report["tasks"][0]["meta"]["finished"]
    );
    assert_eq!(upstream["queue"], third_report["queue"]);
    assert_eq!(upstream["cookies"], cookie_pairs(&third_report));
    assert_eq!(upstream["header"], third_report["cookie_header"]);
    assert_eq!(upstream["login"], third_report["has_login"]);
    assert_eq!(upstream["disk"], disk);
    fs::write(
        ev.join("summary.txt"),
        format!(
            "produced={}\nproducer_ret={}\nrequests={:?}\nfinished=3\n",
            produced.session_path.display(),
            produced.record["ret"],
            requested
        ),
    )
    .unwrap();
}
