//! One real Unix end-to-end migration chain.
//!
//! The fixed Python reference (`43fc55c`) writes a genuine half-finished
//! session through its own `App.save_session()`; the ordinary native host then
//! restores it, downloads only the two missing images with both bodies
//! provably in flight at the same time, renames and archives them, serves the
//! ORIGINAL WebUI from a product-managed persisted cache, accepts a
//! memory-only settings change, saves the session on the first SIGTERM, and the
//! SAME executable starts a second time, applies signed release B and reads the
//! saved task and archived images through the real RPC server.
//!
//! Everything is synthetic and owned: the gallery/update source is loopback,
//! the browser only reaches that listener, and no generic digest is computed.
//! Required interpreter/browser/oracle inputs are explicit; when they are
//! missing this ignored case FAILS instead of skipping.

use std::io::{Read, Seek, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;

const CASE: &str = "tests::native_rss_update::migration_chain::native_legacy_session_to_archived_ui_and_rss_b_same_host";
const PYTHON_REVISION: &str = "43fc55c0c662f195d048becf6276a26ce06b09d9";
const TITLE: &str = "M8 Migration Chain";
const GUID: &str = "m8chain1";
const SECRET: &str = "s3cret";
const GALLERY: &str = "http://e-hentai.org/g/1/aaaa";

/// The closed set of UI probe checks the chain report carries. The probe writes
/// them; this list only pins the contract.
const PROBE_CHECKS: [&str; 13] = [
    "connect.dialog",
    "connect.port",
    "connect.secret",
    "tasks.row",
    "gallery.opened",
    "gallery.images.decode",
    "gallery.images.identity",
    "settings.tab",
    "settings.threads.input",
    "settings.getconfig.value",
    "settings.updateconfig.sent",
    "network.owned",
    "network.no_external",
];

const REPORT_CHECKS: [&str; 14] = [
    "oracle.python_session_fixture",
    "config.python_import_first_launch",
    "phaseA.pending_while_running",
    "phaseA.concurrent_missing_bodies",
    "phaseA.rename_archive",
    "phaseA.ui_reader_and_setting",
    "phaseA.sigterm_graceful_exit",
    "phaseB.same_exe_ordinary_start",
    "phaseB.applied_v2_four_scripts",
    "phaseB.b_gh_plan_wire",
    "phaseB.rpc_saved_task_and_zip",
    "phaseB.preservation_window",
    "negative.single_worker_barrier",
    "cleanup.owned_roots",
];

// MARK: - explicit inputs

struct Inputs {
    root: PathBuf,
    report: PathBuf,
    python: PathBuf,
    chromium: String,
    upstream: PathBuf,
    probe: PathBuf,
}

fn require_env(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => panic!(
            "{CASE}: missing required input {name}; this explicit case fails instead of skipping"
        ),
    }
}

fn inputs() -> Inputs {
    let root = PathBuf::from(require_env("XEH_MIGRATION_RUN_ROOT"));
    assert!(
        root.is_absolute() && root.is_dir(),
        "XEH_MIGRATION_RUN_ROOT must be an existing absolute owned directory"
    );
    let report = PathBuf::from(require_env("XEH_MIGRATION_REPORT"));
    assert!(
        report.parent() == Some(root.as_path()),
        "the chain report must live directly in the fresh run root"
    );
    assert!(
        !report.exists(),
        "refusing a stale chain report; CI allocates a fresh run root"
    );
    let entries = std::fs::read_dir(&root).expect("run root entries");
    assert_eq!(
        entries.count(),
        0,
        "the chain run root must start empty so its report is bound to this run"
    );
    let python = PathBuf::from(require_env("XEH_PYTHON"));
    assert!(python.is_file(), "XEH_PYTHON is not a file");
    let chromium = require_env("XEH_CHROMIUM");
    assert!(Path::new(&chromium).is_file(), "XEH_CHROMIUM is not a file");
    let upstream = PathBuf::from(require_env("XEH_UPSTREAM"));
    assert!(
        upstream.join("xeHentai/config.py").is_file(),
        "XEH_UPSTREAM is not the fixed reference tree"
    );
    let probe = Path::new(MANIFEST).join("tools/browser/migration_probe.py");
    assert!(probe.is_file(), "missing the migration UI probe");
    Inputs {
        root,
        report,
        python,
        chromium,
        upstream,
        probe,
    }
}

/// Byte-compare the whole reference package against the fixed revision through
/// Git. This is a direct content comparison, not a digest.
fn verify_reference_bytes(upstream: &Path) -> usize {
    fn collect(dir: &Path, base: &Path, out: &mut Vec<String>) {
        for entry in std::fs::read_dir(dir).expect("reference tree") {
            let path = entry.expect("reference entry").path();
            if path.is_dir() {
                if path.file_name().and_then(|name| name.to_str()) == Some("__pycache__") {
                    continue;
                }
                collect(&path, base, out);
            } else if path.extension().and_then(|value| value.to_str()) == Some("py") {
                out.push(
                    path.strip_prefix(base)
                        .expect("relative reference file")
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let package = upstream.join("xeHentai");
    let mut files = Vec::new();
    collect(&package, &package, &mut files);
    files.sort();
    assert!(!files.is_empty(), "empty reference tree");
    for relative in &files {
        let shown = std::process::Command::new("git")
            .arg("-C")
            .arg(MANIFEST)
            .args(["show", &format!("{PYTHON_REVISION}:xeHentai/{relative}")])
            .output()
            .expect("git show reference bytes");
        assert!(
            shown.status.success(),
            "cannot resolve {PYTHON_REVISION}:xeHentai/{relative}"
        );
        let local = std::fs::read(package.join(relative)).expect("reference file");
        assert!(
            shown.stdout == local,
            "reference file {relative} differs from {PYTHON_REVISION}"
        );
    }
    files.len()
}

// MARK: - commands with a bounded kill of our own child

fn run_command(mut command: std::process::Command, timeout: Duration, log: &Path) -> Value {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("spawn owned tool");
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("owned tool state is unknown: {error}");
            }
        }
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "owned tool exceeded its bounded deadline: {}",
                log.display()
            );
        }
        std::thread::sleep(Duration::from_millis(40));
    }
    let output = child.wait_with_output().expect("owned tool output");
    let mut sink = Vec::new();
    sink.extend_from_slice(&output.stdout);
    sink.extend_from_slice(b"\n--- stderr ---\n");
    sink.extend_from_slice(&output.stderr);
    std::fs::write(log, &sink).expect("tool log");
    json!({
        "code": output.status.code(),
        "stdout_bytes": output.stdout.len(),
        "stderr_bytes": output.stderr.len(),
        "log": log.to_string_lossy(),
    })
}

// MARK: - the installed host, the reference session and the UI cache

fn migration_install(root: &Path, api: &str) -> PathBuf {
    let install = root.join("install");
    std::fs::create_dir_all(&install).expect("install dir");
    std::fs::copy(m6::binary(), install.join(exe_name())).expect("copy ordinary host");
    m6::copy_tree(&Path::new(MANIFEST).join("rss"), &install.join("rss"));
    m6::copy_tree(
        &Path::new(MANIFEST).join("locales"),
        &install.join("locales"),
    );
    m6::copy_tree(&Path::new(MANIFEST).join("ui"), &install.join("ui"));
    let github = install.join("rss/updater/github.rss");
    let source = std::fs::read_to_string(&github).expect("github.rss");
    std::fs::write(&github, bind(source, api, api)).expect("write binding A");
    install
}

fn config_literal(proxy: &str, threads: i64) -> String {
    format!(
        "# M8 migration chain: minimal supported literal configuration.\n\
         dir = \"downloads\"\n\
         save_tasks = True\n\
         rename_ori = True\n\
         make_archive = True\n\
         proxy = [\"{proxy}\"]\n\
         proxy_image = True\n\
         proxy_image_only = False\n\
         download_thread_cnt = {threads}\n\
         scan_thread_cnt = 1\n\
         download_timeout = 10\n\
         jpn_title = False\n\
         auto_update = \"download\"\n"
    )
}

fn write_reference_session(
    inputs: &Inputs,
    install: &Path,
    proxy: &str,
    threads: i64,
    log: &Path,
) -> Value {
    let request = json!({
        "variant": "parallel-resume",
        "root": install.to_string_lossy(),
        "proxy": proxy,
        "download_thread_cnt": threads,
        "scan_thread_cnt": 1,
        "download_timeout": 10,
        "title": TITLE,
        "url": GALLERY,
        "guid": GUID,
    });
    let request_path = inputs.root.join("oracle-request.json");
    std::fs::write(
        &request_path,
        serde_json::to_vec_pretty(&request).expect("oracle request"),
    )
    .expect("oracle request file");
    let output_path = inputs.root.join("oracle-result.json");
    let mut command = std::process::Command::new(&inputs.python);
    command
        .current_dir(&inputs.root)
        .arg(Path::new(MANIFEST).join("tools/compat.py"))
        .args(["--oracle", "session-fixture", "--input"])
        .arg(&request_path)
        .arg("--output")
        .arg(&output_path)
        .arg("--upstream")
        .arg(&inputs.upstream)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("HOME", install.join("home"))
        .env("TMPDIR", install.join("tmp"))
        .env("XDG_CACHE_HOME", install.join("cache"));
    let result = run_command(command, Duration::from_secs(120), log);
    assert_eq!(result["code"], 0, "the fixed reference fixture failed");
    let parsed: Value =
        serde_json::from_slice(&std::fs::read(&output_path).expect("oracle output"))
            .expect("oracle json");
    assert_eq!(parsed["ok"], true, "reference fixture is not ok");
    assert_eq!(parsed["guid"], GUID);
    parsed
}

/// Walk the copied original UI and return the exact FrozenPack asset names.
fn ui_asset_names(install: &Path) -> Vec<String> {
    fn walk(dir: &Path, relative: &Path, out: &mut Vec<String>) {
        let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
            .expect("ui dir")
            .map(|entry| entry.expect("ui entry").path())
            .collect();
        paths.sort();
        for path in paths {
            let name = relative.join(path.file_name().expect("ui name"));
            if path.is_dir() {
                walk(&path, &name, out);
            } else {
                out.push(format!("/{}", name.to_string_lossy().replace('\\', "/")));
            }
        }
    }
    let mut names = vec!["/".to_string()];
    walk(&install.join("ui/static"), Path::new("static"), &mut names);
    names
}

/// Prime the product's persisted static cache through its own API and reload
/// it, so an ignored store failure or eviction is a real failure here.
fn prime_ui_cache(install: &Path) -> Vec<String> {
    let names = ui_asset_names(install);
    assert_eq!(
        names.len(),
        7,
        "the frozen pack must carry its seven assets"
    );
    let pack = crate::rpc::FrozenPack::load(install).expect("frozen UI pack");
    let clock = Arc::new(crate::worker::RealClock);
    let path = install.join(crate::rpc::STATIC_CACHE_NAME);
    let mut cache = crate::rpc::StaticCache::load(&path, clock.clone());
    let now = <crate::worker::RealClock as crate::worker::HttpClock>::now(clock.as_ref());
    for name in &names {
        let (bytes, mime) = pack.get(name).expect("pack asset");
        cache.store(
            name,
            crate::rpc::StaticEntry {
                body: bytes.to_vec(),
                mime: mime.to_string(),
                fetched: now,
                last_modified: "Wed, 01 Jan 2025 00:00:00 GMT".to_string(),
            },
            name == "/",
        );
    }
    let mut reloaded = crate::rpc::StaticCache::load(&path, clock);
    for name in &names {
        let (bytes, mime) = pack.get(name).expect("pack asset");
        let entry = reloaded
            .fresh(name)
            .unwrap_or_else(|| panic!("the product cache lost {name}"));
        assert_eq!(entry.body, bytes, "cache bytes for {name}");
        assert_eq!(entry.mime, mime, "cache MIME for {name}");
    }
    names
}

// MARK: - the scenario-local resume gallery

#[derive(Clone)]
struct Identity {
    fid: i64,
    hash: String,
    name: String,
    bytes: Arc<Vec<u8>>,
}

struct BarrierState {
    arrived: Vec<i64>,
    released: bool,
    timed_out: bool,
    release_distinct: usize,
    body_requests: usize,
    old_body_requests: usize,
}

/// A real HTTP proxy for the restored task. Page responses are immediate; the
/// two missing image bodies are held until two DISTINCT fids are simultaneously
/// live, or until a bounded deadline strictly below the client timeout. It
/// never changes the parent `Gallery` default hold-all behaviour.
struct ResumeGallery {
    port: u16,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<String>>>,
    barrier: Arc<(Mutex<BarrierState>, Condvar)>,
    cookie_sent: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl ResumeGallery {
    fn start(identities: Vec<Identity>, target: usize, timeout: Duration) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind resume gallery");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("gallery addr").port();
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let barrier = Arc::new((
            Mutex::new(BarrierState {
                arrived: Vec::new(),
                released: false,
                timed_out: false,
                release_distinct: 0,
                body_requests: 0,
                old_body_requests: 0,
            }),
            Condvar::new(),
        ));
        let table = Arc::new(identities);
        let stopping = Arc::clone(&stop);
        let recorded = Arc::clone(&seen);
        let gate = Arc::clone(&barrier);
        let cookie_sent = Arc::new(AtomicBool::new(false));
        let cookie_flag = Arc::clone(&cookie_sent);
        let handle = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let table = Arc::clone(&table);
                        let recorded = Arc::clone(&recorded);
                        let gate = Arc::clone(&gate);
                        let stopping = Arc::clone(&stopping);
                        let cookie_flag = Arc::clone(&cookie_flag);
                        std::thread::spawn(move || {
                            serve_resume(
                                socket,
                                &table,
                                &recorded,
                                &gate,
                                &stopping,
                                &cookie_flag,
                                target,
                                timeout,
                            );
                        });
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            port,
            stop,
            seen,
            barrier,
            cookie_sent,
            handle: Some(handle),
        }
    }

    fn proxy(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn stats(&self) -> (usize, usize, usize, bool) {
        let state = self.barrier.0.lock().unwrap();
        (
            state.release_distinct,
            state.body_requests,
            state.old_body_requests,
            state.timed_out,
        )
    }

    fn seen(&self) -> usize {
        self.seen.lock().unwrap().len()
    }

    fn cookie_sent(&self) -> bool {
        self.cookie_sent.load(Ordering::SeqCst)
    }
}

impl Drop for ResumeGallery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let state = &self.barrier.0;
        if let Ok(mut state) = state.lock() {
            state.released = true;
            self.barrier.1.notify_all();
        }
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn proxy_path(target: &str) -> String {
    match target.find("://") {
        Some(index) => match target[index + 3..].find('/') {
            Some(slash) => target[index + 3 + slash..].to_string(),
            None => "/".to_string(),
        },
        None => target.to_string(),
    }
}

#[allow(clippy::too_many_arguments)]
fn serve_resume(
    mut socket: TcpStream,
    identities: &[Identity],
    seen: &Arc<Mutex<Vec<String>>>,
    barrier: &Arc<(Mutex<BarrierState>, Condvar)>,
    stop: &Arc<AtomicBool>,
    cookie_sent: &Arc<AtomicBool>,
    target: usize,
    timeout: Duration,
) {
    let Some(request) = read_head(&mut socket) else {
        return;
    };
    // The only cookie observation this chain needs: whether the restored
    // session's synthetic credential pair really reached the owned source.
    // Nothing is logged and the header value is never retained.
    if let Some(cookie) = request.header("cookie")
        && cookie.contains("ipb_member_id=42")
        && cookie.contains("ipb_pass_hash=cafebabe")
    {
        cookie_sent.store(true, Ordering::SeqCst);
    }
    let target_text = request.target;
    let path = proxy_path(&target_text);
    seen.lock().unwrap().push(target_text.clone());
    let identity = identities
        .iter()
        .find(|identity| path.contains(&identity.hash))
        .or_else(|| {
            identities
                .iter()
                .find(|identity| path.contains(&identity.hash[..10]))
        });
    if path.contains("/s/") {
        let Some(identity) = identity else {
            respond(&mut socket, "404 Not Found", "text/plain", b"unknown page");
            return;
        };
        let body = format!(
            "<img src=\"http://img.test/{}-{}-200-0-jpg/keystamp=abc\" style=\"max-width:100%\">\
             </a></div><div>{} :: 64x48 :: 2 KB</di>\
             return nl('700000-{}')",
            identity.hash,
            identity.bytes.len(),
            identity.name,
            identity.fid
        );
        respond(
            &mut socket,
            "200 OK",
            "text/html; charset=utf-8",
            body.as_bytes(),
        );
        return;
    }
    if path.contains("/g/") {
        let mut body = format!("=\\\"gj\\\">移行ギャラリー</h1>=\\\"gn\\\">{TITLE}</h1>");
        body.push_str("Length:</td><td class=\"gdt2\">1 pages</td>");
        body.push_str("Showing 1 - 4 of 4 images");
        respond(
            &mut socket,
            "200 OK",
            "text/html; charset=utf-8",
            body.as_bytes(),
        );
        return;
    }
    // The proxy receives the absolute image URL, so the owned host name is not
    // part of `path`; the full 40-hex identity is.
    let Some(identity) = identities
        .iter()
        .find(|identity| path.contains(&identity.hash))
    else {
        respond(&mut socket, "404 Not Found", "text/plain", b"missing");
        return;
    };
    {
        let mut state = barrier.0.lock().unwrap();
        state.body_requests += 1;
        if identity.fid == 1 || identity.fid == 4 {
            state.old_body_requests += 1;
        }
        if !state.released && !state.arrived.contains(&identity.fid) {
            state.arrived.push(identity.fid);
        }
        let deadline = Instant::now() + timeout;
        loop {
            if state.released || stop.load(Ordering::SeqCst) {
                break;
            }
            if state.arrived.len() >= target {
                state.released = true;
                state.timed_out = false;
                state.release_distinct = state.arrived.len();
                barrier.1.notify_all();
                break;
            }
            let now = Instant::now();
            if now >= deadline {
                state.released = true;
                state.timed_out = true;
                state.release_distinct = state.arrived.len();
                barrier.1.notify_all();
                break;
            }
            let wait = (deadline - now).min(Duration::from_millis(20));
            let (next, _) = barrier.1.wait_timeout(state, wait).unwrap();
            state = next;
        }
    }
    respond(&mut socket, "200 OK", "image/jpeg", &identity.bytes);
}

// MARK: - RPC and binary wire reads

fn read_response(stream: &mut TcpStream) -> Result<(u16, Vec<u8>), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(20)))
        .map_err(|error| error.to_string())?;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(index) = raw.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed before the response head".to_string());
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > 64 * 1024 {
            return Err("oversized response head".to_string());
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end]).into_owned();
    let status: u16 = head
        .split_whitespace()
        .nth(1)
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| "missing response status".to_string())?;
    let mut length: Option<usize> = None;
    for line in head.lines() {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok();
        }
    }
    let mut body = raw[head_end..].to_vec();
    let expected = length.ok_or_else(|| "response without content-length".to_string())?;
    while body.len() < expected {
        let read = stream.read(&mut chunk).map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed inside the response body".to_string());
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(expected);
    Ok((status, body))
}

fn http_get(port: u16, target: &str) -> Result<(u16, Vec<u8>), String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    write!(
        stream,
        "GET {target} HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n"
    )
    .map_err(|error| error.to_string())?;
    read_response(&mut stream)
}

fn rpc_call(port: u16, method: &str, params: Value) -> Result<Value, String> {
    let body = json!({ "method": method, "id": 1, "params": params }).to_string();
    let mut stream = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    write!(
        stream,
        "POST /jsonrpc HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json-rpc\r\n\
         Content-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .map_err(|error| error.to_string())?;
    let (status, raw) = read_response(&mut stream)?;
    if status != 200 {
        return Err(format!("jsonrpc answered {status}"));
    }
    let value: Value = serde_json::from_slice(&raw).map_err(|error| error.to_string())?;
    if !value["error"].is_null() {
        return Err("jsonrpc error".to_string());
    }
    Ok(value.get("result").cloned().unwrap_or(Value::Null))
}

fn rpc_params(args: Value) -> Value {
    json!([args, {}])
}

fn list_finished(port: u16) -> Result<Value, String> {
    rpc_call(
        port,
        "xeH.listTasks",
        rpc_params(json!([format!("token:{SECRET}"), "finished"])),
    )
}

fn get_config(port: u16) -> Result<Value, String> {
    rpc_call(
        port,
        "xeH.getConfig",
        rpc_params(json!([format!("token:{SECRET}")])),
    )
}

fn get_images(port: u16) -> Result<Value, String> {
    rpc_call(
        port,
        "xeH.getImage",
        rpc_params(json!([format!("token:{SECRET}"), GUID])),
    )
}

// MARK: - report construction

struct Report {
    checks: Vec<Value>,
    fields: serde_json::Map<String, Value>,
}

impl Report {
    fn new(root: &Path) -> Self {
        let head = std::process::Command::new("git")
            .arg("-C")
            .arg(MANIFEST)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("git head");
        assert!(head.status.success(), "cannot read the source head");
        let head = String::from_utf8(head.stdout)
            .expect("head utf8")
            .trim()
            .to_string();
        assert!(
            head.len() == 40 && head.chars().all(|c| c.is_ascii_hexdigit()),
            "source head is not a full revision"
        );
        let status = std::process::Command::new("git")
            .arg("-C")
            .arg(MANIFEST)
            .args(["status", "--porcelain"])
            .output()
            .expect("git status");
        assert!(status.status.success(), "cannot read the source status");
        let dirty = !status.stdout.is_empty();
        let mut fields = serde_json::Map::new();
        fields.insert("schema".into(), json!("m8-migration-chain-report-v1"));
        fields.insert("case".into(), json!(CASE));
        fields.insert("run_root".into(), json!(root.to_string_lossy()));
        fields.insert(
            "source".into(),
            json!({
                "head": head,
                "dirty": dirty,
                "worktree": MANIFEST,
            }),
        );
        Self {
            checks: Vec::new(),
            fields,
        }
    }

    fn record(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        assert!(ok, "{name}: {detail}");
        self.checks.push(json!({
            "name": name,
            "ok": true,
            "detail": detail.to_string(),
        }));
    }

    fn field(&mut self, name: &str, value: Value) {
        self.fields.insert(name.to_string(), value);
    }

    fn write(self, path: &Path) {
        assert!(!path.exists(), "refusing to overwrite a chain report");
        let names: Vec<&str> = self
            .checks
            .iter()
            .filter_map(|check| check["name"].as_str())
            .collect();
        assert_eq!(
            names,
            REPORT_CHECKS.to_vec(),
            "the chain report must carry the exact required check set in order"
        );
        let mut root = self.fields;
        root.insert("checks".into(), Value::Array(self.checks));
        let text = serde_json::to_string_pretty(&Value::Object(root)).expect("report json");
        let temporary = path.with_extension("tmp");
        std::fs::write(&temporary, format!("{text}\n")).expect("write report");
        std::fs::rename(&temporary, path).expect("publish report");
    }
}

// MARK: - small file helpers

fn dir_removed(path: &Path) -> bool {
    !path.exists()
}

fn log_contains(path: &Path, needle: &str) -> bool {
    std::fs::read(path)
        .map(|bytes| contains(&bytes, needle.as_bytes()))
        .unwrap_or(false)
}

fn zip_snapshot<R: Read + Seek>(reader: R) -> Vec<(String, bool, Vec<u8>)> {
    let mut archive = zip::ZipArchive::new(reader).expect("zip archive");
    let mut out = Vec::new();
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).expect("zip entry");
        let name = entry.name().to_string();
        let stored = entry.compression() == zip::CompressionMethod::Stored;
        let mut body = Vec::new();
        entry.read_to_end(&mut body).expect("zip body");
        out.push((name, stored, body));
    }
    out
}

fn assert_archive(entries: &[(String, bool, Vec<u8>)], images: &[Vec<u8>]) -> Vec<String> {
    let mut names = Vec::new();
    for (name, stored, _) in entries {
        assert!(stored, "archive entry {name} is not STORE");
        names.push(name.clone());
    }
    let sorted = {
        let mut clone = names.clone();
        clone.sort();
        clone
    };
    assert_eq!(names, sorted, "archive entries must be bytewise sorted");
    assert_eq!(
        names,
        ARCHIVE_ENTRIES.map(str::to_string).to_vec(),
        "archive entry set must be exactly the completion marker and the four renamed images"
    );
    for (index, name) in names.iter().enumerate() {
        let Some(fid) = name
            .strip_prefix("unique-")
            .and_then(|rest| rest.strip_suffix(".jpg"))
            .and_then(|fid| fid.parse::<usize>().ok())
        else {
            continue;
        };
        assert!(
            (1..=images.len()).contains(&fid),
            "archive entry {name} is not one of the downloaded images"
        );
        assert_eq!(
            entries[index].2,
            images[fid - 1],
            "archive entry {name} does not carry the downloaded bytes"
        );
    }
    names
}

/// The exact archive the real rename/archive step produces: the completion
/// marker plus the four renamed images, bytewise sorted.
const ARCHIVE_ENTRIES: [&str; 5] = [
    ".xehdone",
    "unique-1.jpg",
    "unique-2.jpg",
    "unique-3.jpg",
    "unique-4.jpg",
];

/// Negative qualifiers for the archive contract. A synthetic ZIP is only
/// constructed here to prove the checker refuses a wrong entry set; the real
/// product archive is the one the chain itself builds and inspects.
type ZipBytes = zip::ZipWriter<std::io::Cursor<Vec<u8>>>;

fn stored_entry(writer: &mut ZipBytes, name: &str, body: &[u8]) {
    writer
        .start_file(
            name,
            zip::write::SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Stored),
        )
        .expect("synthetic zip entry");
    writer.write_all(body).expect("synthetic zip body");
}

fn finish_zip(writer: ZipBytes) -> Vec<u8> {
    writer.finish().expect("synthetic zip finish").into_inner()
}

fn image_index(name: &str) -> Option<usize> {
    name.strip_prefix("unique-")
        .and_then(|rest| rest.strip_suffix(".jpg"))
        .and_then(|fid| fid.parse().ok())
}

fn synthetic_archive(names: &[&str], images: &[Vec<u8>]) -> Vec<(String, bool, Vec<u8>)> {
    let mut writer = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for name in names {
        let body: &[u8] = match image_index(name) {
            Some(fid) => &images[fid - 1],
            None => b"",
        };
        stored_entry(&mut writer, name, body);
    }
    zip_snapshot(std::io::Cursor::new(finish_zip(writer)))
}

fn chain_images() -> Vec<Vec<u8>> {
    (1..=4usize).map(|fid| vec![fid as u8; 32 + fid]).collect()
}

#[test]
fn archive_entry_set_is_exactly_the_marker_and_four_images() {
    let images = chain_images();
    let entries = synthetic_archive(&ARCHIVE_ENTRIES, &images);
    assert_eq!(
        assert_archive(&entries, &images),
        ARCHIVE_ENTRIES.map(str::to_string).to_vec()
    );
}

#[test]
#[should_panic(expected = "archive entry set")]
fn archive_without_the_completion_marker_is_rejected() {
    let images = chain_images();
    let entries = synthetic_archive(&ARCHIVE_ENTRIES[1..], &images);
    assert_archive(&entries, &images);
}

#[test]
#[should_panic(expected = "archive entry set")]
fn archive_with_an_extra_entry_is_rejected() {
    let images = chain_images();
    let entries = synthetic_archive(
        &[
            ".xehdone",
            "notes.txt",
            "unique-1.jpg",
            "unique-2.jpg",
            "unique-3.jpg",
            "unique-4.jpg",
        ],
        &images,
    );
    assert_archive(&entries, &images);
}

#[test]
#[should_panic(expected = "archive entry set")]
fn archive_with_a_duplicate_entry_is_rejected() {
    let images = chain_images();
    let mut entries = synthetic_archive(&ARCHIVE_ENTRIES, &images);
    // The pinned zip reader keys entries by name, so a repeated
    // central-directory name is collapsed before this checker sees it. The
    // control therefore feeds the entry list a per-record reader would hand
    // over, which is exactly what the ordered-vector contract must refuse.
    let repeated = entries[2].clone();
    entries.insert(3, repeated);
    assert_archive(&entries, &images);
}

fn task_from_session(bytes: &[u8]) -> Value {
    let document: Value = serde_json::from_slice(bytes).expect("saved session json");
    document
        .get("tasks")
        .and_then(|tasks| tasks.get(GUID))
        .cloned()
        .expect("saved task record")
}

// MARK: - the one real chain

#[test]
#[ignore = "explicit Unix migration chain: needs reference Python, Chromium and the fixed oracle"]
fn native_legacy_session_to_archived_ui_and_rss_b_same_host() {
    let inputs = inputs();
    let origin = Arc::new(Server::start(Vec::new()));
    let files = candidate(&origin.origin());
    let (info, sig, payload) = signed_package(&files);
    origin.set(COMMITS_TARGET, COMMITS.as_bytes().to_vec());
    origin.set(&package_target(&origin.origin(), ""), payload.clone());
    origin.set(&package_target(&origin.origin(), ".manifest.json"), info);
    origin.set(&package_target(&origin.origin(), ".sig"), sig);

    let install = migration_install(&inputs.root, &origin.origin());
    for dir in ["home", "tmp", "cache"] {
        std::fs::create_dir_all(install.join(dir)).expect("owned child dir");
    }
    let reference_files = verify_reference_bytes(&inputs.upstream);
    assert!(reference_files >= 5, "reference package is incomplete");

    // The reference App/Task writes h.json itself; the task's 14-key snapshot
    // deliberately carries the same values the native global config and the
    // explicit CLI flags use. `proxy` is a global configuration key (it is not
    // part of the task snapshot), so the placeholder below only satisfies the
    // helper signature and never reaches the persisted task.
    let oracle = write_reference_session(
        &inputs,
        &install,
        "http://127.0.0.1:9",
        2,
        &inputs.root.join("oracle.log"),
    );
    let identity_values = oracle["identities"].as_array().expect("identity list");
    let mut identities = Vec::new();
    let mut images = Vec::new();
    for identity in identity_values {
        let fid = identity["fid"].as_i64().expect("fid");
        let path = &oracle["images"][(fid - 1) as usize];
        let bytes = std::fs::read(path.as_str().expect("image path")).expect("fixture image");
        images.push(bytes.clone());
        identities.push(Identity {
            fid,
            hash: identity["hash"].as_str().expect("hash").to_string(),
            name: identity["name"].as_str().expect("name").to_string(),
            bytes: Arc::new(bytes),
        });
    }
    assert_eq!(identities.len(), 4, "fixture identities");
    let gallery_dir = PathBuf::from(oracle["gallery"].as_str().expect("gallery path"));
    for fid in [1usize, 4] {
        let present = std::fs::read(gallery_dir.join(format!("{fid}.jpg"))).expect("present image");
        assert_eq!(present, images[fid - 1], "present image bytes");
    }
    for fid in [2usize, 3] {
        assert!(
            !gallery_dir.join(format!("{fid}.jpg")).exists(),
            "fid {fid} must be absent from the half-finished gallery"
        );
    }
    let gallery = ResumeGallery::start(identities.clone(), 2, Duration::from_secs(3));
    let config_py = config_literal(&gallery.proxy(), 2);
    std::fs::write(install.join("config.py"), &config_py).expect("config.py");
    let oracle_session = std::fs::read(install.join("h.json")).expect("reference session");
    let ui_assets = prime_ui_cache(&install);

    let port = free_port();
    let mut run = spawn(
        &install,
        &[
            "--rpc-interface",
            "127.0.0.1",
            "--rpc-port",
            &port.to_string(),
            "--rpc-secret",
            SECRET,
            "--rename-ori",
            "true",
            "--archive",
            "true",
            "--auto-update",
            "download",
        ],
    );
    wait_event(
        &mut run,
        Duration::from_secs(120),
        "migration phase A pending publication",
        || pending_path(&install).is_file(),
        || {
            format!(
                "{} {}",
                install_facts(&install, None),
                chain_fixture_facts(&origin, 0, &gallery)
            )
        },
    );
    assert!(
        run.running(),
        "phase A: the host must still be running when the candidate is pending"
    );
    let pending_while_running = pending_path(&install).is_file();
    assert!(pending_while_running, "phase A pending evidence");
    let imported = crate::config::Config::load(&install).expect("imported config");
    assert!(
        imported.config.save_tasks,
        "the product import must honour save_tasks"
    );
    assert_eq!(
        std::fs::read(install.join("config.py")).expect("config.py bytes"),
        config_py.as_bytes(),
        "config.py must keep its exact bytes"
    );
    assert!(
        install.join("config.rss").is_file(),
        "the first launch must create config.rss through the product importer"
    );

    wait_event(
        &mut run,
        Duration::from_secs(180),
        "migration phase A completion",
        || {
            list_finished(port)
                .ok()
                .and_then(|tasks| {
                    tasks
                        .as_array()
                        .and_then(|tasks| tasks.iter().find(|task| task["guid"] == GUID))
                        .map(|task| task["state"] == 20)
                })
                .unwrap_or(false)
                && dir_removed(&gallery_dir)
                && install
                    .join("downloads")
                    .join(format!("{TITLE}.zip"))
                    .is_file()
        },
        || {
            format!(
                "{} {}",
                install_facts(&install, None),
                chain_fixture_facts(&origin, 0, &gallery)
            )
        },
    );
    let (release_distinct, body_requests, old_body_requests, barrier_timed_out) = gallery.stats();
    assert!(run.running(), "phase A: the host must remain resident");
    assert!(
        !barrier_timed_out,
        "both missing image bodies must arrive inside the bounded barrier"
    );
    assert_eq!(
        release_distinct, 2,
        "two distinct image bodies must be simultaneously live before either is released"
    );
    assert_eq!(
        body_requests, 2,
        "exactly the two missing images are requested"
    );
    assert_eq!(
        old_body_requests, 0,
        "already-downloaded images must not be fetched"
    );
    assert!(
        gallery.cookie_sent(),
        "the restored session's cookie pair must reach the owned source"
    );
    assert_eq!(
        gallery.seen(),
        6,
        "four image pages and two image bodies require exactly six proxy requests"
    );
    let map_entries = zip_snapshot(
        std::fs::File::open(install.join("downloads").join(format!("{TITLE}.zip")))
            .expect("archive file"),
    );
    let archive_names = assert_archive(&map_entries, &images);
    assert!(
        dir_removed(&gallery_dir),
        "the archive step must remove the loose gallery directory"
    );

    // The original WebUI through the persisted product cache, against the real
    // resident host. `config.rss` is memory-only for updateConfig.
    let config_before_probe = std::fs::read(install.join("config.rss")).expect("config.rss");
    let probe_report = inputs.root.join("probe-report.json");
    let mut probe = std::process::Command::new(&inputs.python);
    probe
        .arg(&inputs.probe)
        .args(["--base", &format!("http://127.0.0.1:{port}")])
        .args(["--port", &port.to_string()])
        .args(["--secret", SECRET, "--title", TITLE, "--guid", GUID])
        .args(["--exe", &inputs.chromium])
        .arg("--report")
        .arg(&probe_report)
        .env("HOME", install.join("home"))
        .env("TMPDIR", install.join("tmp"))
        .env("XDG_CACHE_HOME", install.join("cache"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .env("LANG", "C");
    let probe_result = run_command(
        probe,
        Duration::from_secs(300),
        &inputs.root.join("probe.log"),
    );
    assert_eq!(probe_result["code"], 0, "the original-UI probe failed");
    let probe_json: Value =
        serde_json::from_slice(&std::fs::read(&probe_report).expect("probe report"))
            .expect("probe report json");
    let probe_checks = probe_json["checks"]
        .as_array()
        .expect("probe checks")
        .clone();
    assert_eq!(probe_checks.len(), PROBE_CHECKS.len(), "probe check count");
    for (expected, actual) in PROBE_CHECKS.iter().zip(probe_checks.iter()) {
        assert_eq!(actual["name"], *expected, "probe check identity");
        assert_eq!(actual["ok"], true, "probe check {expected} failed");
    }
    let probe_ok = true;
    assert_eq!(
        std::fs::read(install.join("config.rss")).expect("config.rss after probe"),
        config_before_probe,
        "the memory-only UI settings change must not rewrite config.rss"
    );
    let live_config = get_config(port).expect("getConfig after the UI change");
    assert_eq!(
        live_config["download_thread_cnt"], 3,
        "the real RPC must return the new runtime value"
    );

    // First SIGTERM: the real graceful drain, natural exit 0 and a saved
    // session; the task keeps its old snapshot thread value.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(run.child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("first SIGTERM");
    let status = run
        .wait_exit(Duration::from_secs(60))
        .expect("phase A: the first SIGTERM must end the host");
    run.collect();
    assert_eq!(status.code(), Some(0), "phase A: graceful exit status");
    let cleanup_logged = log_contains(&install.join("eh.log"), "cleaning up...");
    assert!(cleanup_logged, "the signal path must log its cleanup");
    let saved_session = std::fs::read(install.join("h.json")).expect("saved session");
    assert!(
        saved_session != oracle_session,
        "the graceful exit must really save the session"
    );
    let task = task_from_session(&saved_session);
    assert_eq!(task["state"], 20, "the saved task must be finished");
    assert_eq!(
        task["config"]["download_thread_cnt"], 2,
        "the task snapshot must stay the value the reference wrote"
    );
    assert_eq!(task["meta"]["finished"], 4, "all four images finished");
    let document: Value = serde_json::from_slice(&saved_session).expect("session json");
    assert_eq!(document["cookies"]["ipb_member_id"], "42");
    assert_eq!(document["cookies"]["ipb_pass_hash"], "cafebabe");
    assert_eq!(
        std::fs::read(install.join("config.py")).expect("config.py after A"),
        config_py.as_bytes(),
        "config.py bytes must survive the whole phase A"
    );
    assert_eq!(
        std::fs::read(install.join("config.rss")).expect("config.rss after A"),
        config_before_probe,
        "the runtime settings change is memory-only"
    );

    // Phase B: the SAME executable path applies the signed candidate and the
    // new business module really runs.
    let before_b = snapshot(&install);
    let exe_before_b = install.join(exe_name());
    assert!(
        same_bytes(&exe_before_b, &m6::binary()),
        "phase A must not replace the host executable"
    );
    origin.set(COMMITS_TARGET_B, COMMITS.as_bytes().to_vec());
    let seen_before = origin.requests().len();
    let gallery_b = Gallery::start();
    let port_b = free_port();
    let mut run_b = spawn(
        &install,
        &[
            "--rpc-interface",
            "127.0.0.1",
            "--rpc-port",
            &port_b.to_string(),
            "--rpc-secret",
            SECRET,
            "--auto-update",
            "check",
        ],
    );
    wait_applied_record(
        &mut run_b,
        &install,
        &origin,
        seen_before,
        &gallery_b,
        files.as_slice(),
    );
    wait_event(
        &mut run_b,
        Duration::from_secs(120),
        "migration phase B business request",
        || {
            origin.requests()[seen_before..]
                .iter()
                .any(|request| request.target == COMMITS_TARGET_B)
        },
        || {
            format!(
                "{} {}",
                install_facts(&install, Some(files.as_slice())),
                fixture_facts(&origin, seen_before, &gallery_b)
            )
        },
    );
    assert!(run_b.running(), "phase B host must still be running");
    let after_apply = snapshot(&install);
    let (changed, added, removed) = diff(&before_b, &after_apply);
    assert_eq!(
        changed,
        RSS_FILES.map(str::to_string).to_vec(),
        "the RSS window changed a file outside the four scripts"
    );
    assert!(
        added.is_empty() && removed.is_empty(),
        "the RSS window added {added:?} or removed {removed:?}"
    );
    assert!(
        applied_record_matches(&install),
        "phase B must publish its v2 applied record"
    );
    assert!(
        !pending_path(&install).exists(),
        "phase B must retire the applied candidate"
    );
    assert!(
        origin.requests()[seen_before..]
            .iter()
            .all(|request| request.target != COMMITS_TARGET),
        "release A's script must not run after the apply"
    );
    let marker_seen = origin.requests()[seen_before..]
        .iter()
        .any(|request| request.target == COMMITS_TARGET_B && request.target.ends_with("&m8=b"));
    assert!(
        marker_seen,
        "the B business query must carry its wire marker"
    );

    // Read the saved task and archived bytes through the real RPC/gallery.
    let tasks = list_finished(port_b).expect("phase B finished task list");
    let saved = tasks
        .as_array()
        .and_then(|tasks| tasks.iter().find(|task| task["guid"] == GUID))
        .expect("the saved task must be visible through RPC");
    assert_eq!(
        saved["state"], 20,
        "RPC must report the saved finished task"
    );
    let links = get_images(port_b).expect("phase B getImage");
    let links = links.as_array().expect("getImage list");
    assert_eq!(links.len(), 4, "getImage must return four archived images");
    for (index, link) in links.iter().enumerate() {
        let link = link.as_str().expect("image link");
        assert!(link.starts_with("/img/"), "image link shape: {link}");
        assert!(
            link.ends_with(&format!("unique-{}.jpg", index + 1)),
            "image link must carry the renamed name: {link}"
        );
        let (status, body) = http_get(port_b, link).expect("archived image read");
        assert_eq!(status, 200, "archived image status");
        assert_eq!(body, images[index], "archived image bytes through RPC");
    }
    let zip_link = format!(
        "/zip/{}/{GUID}/gallery.zip",
        crate::rpc::hash_link(Some(SECRET), GUID)
    );
    let (zip_status, zip_body) = http_get(port_b, &zip_link).expect("archive download");
    assert_eq!(zip_status, 200, "archive download status");
    let remote_entries = zip_snapshot(std::io::Cursor::new(zip_body));
    assert_archive(&remote_entries, &images);

    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(run_b.child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("phase B SIGTERM");
    let status_b = run_b
        .wait_exit(Duration::from_secs(60))
        .expect("phase B: the first SIGTERM must end the host");
    run_b.collect();
    assert_eq!(status_b.code(), Some(0), "phase B: graceful exit status");
    assert!(
        same_bytes(&exe_before_b, &m6::binary()),
        "the host bytes changed"
    );
    for (path, body) in &files {
        assert_eq!(
            &std::fs::read(install.join(path)).expect("live script"),
            body,
            "live {path} is not release B"
        );
    }
    assert!(applied_record_matches(&install));

    drop(run_b);
    drop(gallery_b);

    // The one-worker negative control: the same barrier must NOT report
    // overlap. It is a harness negative, never a product RED.
    let negative_dir = inputs.root.join("negative");
    std::fs::create_dir_all(&negative_dir).expect("negative root");
    let negative = migration_install(&negative_dir, &origin.origin());
    std::fs::create_dir_all(negative.join("home")).expect("negative home");
    std::fs::create_dir_all(negative.join("tmp")).expect("negative tmp");
    std::fs::create_dir_all(negative.join("cache")).expect("negative cache");
    let negative_gallery = ResumeGallery::start(identities.clone(), 2, Duration::from_secs(3));
    let negative_proxy = negative_gallery.proxy();
    std::fs::write(
        negative.join("config.py"),
        config_literal(&negative_proxy, 1),
    )
    .expect("negative config.py");
    let _ = write_reference_session(
        &inputs,
        &negative,
        &negative_proxy,
        1,
        &inputs.root.join("oracle-negative.log"),
    );
    let negative_port = free_port();
    let mut negative_run = spawn(
        &negative,
        &[
            "--rpc-interface",
            "127.0.0.1",
            "--rpc-port",
            &negative_port.to_string(),
            "--rpc-secret",
            SECRET,
            "--rename-ori",
            "true",
            "--archive",
            "true",
            "--auto-update",
            "off",
        ],
    );
    wait_event(
        &mut negative_run,
        Duration::from_secs(180),
        "single-worker negative completion",
        || {
            list_finished(negative_port)
                .ok()
                .and_then(|tasks| {
                    tasks
                        .as_array()
                        .and_then(|tasks| tasks.iter().find(|task| task["guid"] == GUID))
                        .map(|task| task["state"] == 20)
                })
                .unwrap_or(false)
        },
        || {
            format!(
                "{} {}",
                install_facts(&negative, None),
                chain_fixture_facts(&origin, 0, &negative_gallery)
            )
        },
    );
    let (negative_distinct, negative_bodies, negative_old, negative_timeout) =
        negative_gallery.stats();
    assert!(
        negative_timeout,
        "a single download worker must reach the barrier deadline"
    );
    assert_eq!(
        negative_distinct, 1,
        "a single worker can never present two distinct live image bodies"
    );
    assert_eq!(
        negative_bodies, 2,
        "both images eventually download serially"
    );
    assert_eq!(
        negative_old, 0,
        "the negative must not fetch present images"
    );
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(negative_run.child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .expect("negative SIGTERM");
    let negative_status = negative_run
        .wait_exit(Duration::from_secs(60))
        .expect("negative host must exit");
    negative_run.collect();
    assert_eq!(negative_status.code(), Some(0), "negative exit status");
    drop(negative_run);
    drop(negative_gallery);

    // The report exists only after every real assertion above passed.
    let mut report = Report::new(&inputs.root);
    report.field("host", json!({ "exe": "xeHentai", "same_exe_path": true }));
    report.field(
        "rss",
        json!({
            "files": RSS_FILES,
            "b_marker": "&m8=b",
            "applied_update_id": UPDATE_ID,
            "applied_v": 2,
        }),
    );
    report.field(
        "archive",
        json!({
            "zip_name": format!("{TITLE}.zip"),
            "entries": archive_names.clone(),
            "compression": "stored",
            "four_images_byte_equal": true,
            "loose_dir_removed": true,
        }),
    );
    report.field(
        "resources",
        json!({
            "ui_assets": ui_assets.len(),
            "cache_fresh": true,
            "config_rss": "unchanged",
            "session_in_window": "unchanged",
            "exe_unchanged": true,
        }),
    );
    report.field(
        "ui_probe",
        json!({ "checks": probe_checks.clone(), "report": probe_report.to_string_lossy() }),
    );
    report.field(
        "fixture",
        json!({
            "python_source": PYTHON_REVISION,
            "present": [1, 4],
            "missing": [2, 3],
            "image_requests": body_requests,
            "old_image_requests": old_body_requests,
            "barrier_distinct": release_distinct,
            "cookie_sent": true,
        }),
    );
    report.field(
        "negative",
        json!({
            "attempted": true,
            "single_worker": true,
            "barrier_overlap": false,
            "barrier_timed_out": negative_timeout,
            "clean_exit": true,
        }),
    );
    report.record(
        "oracle.python_session_fixture",
        reference_files > 0,
        format!(
            "{reference_files} fixed reference files byte-equal; 4 identities; 2 present, 2 absent"
        ),
    );
    report.record(
        "config.python_import_first_launch",
        imported.config.save_tasks,
        "config.rss imported by the product; config.py bytes unchanged",
    );
    report.record(
        "phaseA.pending_while_running",
        pending_while_running,
        "signed candidate published while A lived",
    );
    report.record(
        "phaseA.concurrent_missing_bodies",
        release_distinct == 2 && body_requests == 2 && old_body_requests == 0,
        "two distinct bodies live at release; exactly two image requests",
    );
    report.record(
        "phaseA.rename_archive",
        archive_names == ARCHIVE_ENTRIES.map(str::to_string).to_vec(),
        format!(
            "STORE archive entries {:?}; loose gallery removed",
            archive_names
        ),
    );
    report.record(
        "phaseA.ui_reader_and_setting",
        probe_ok && probe_checks.len() == PROBE_CHECKS.len(),
        "original UI reader decoded four images; memory-only setting observed through RPC",
    );
    report.record(
        "phaseA.sigterm_graceful_exit",
        cleanup_logged,
        "first SIGTERM exit 0, cleanup log, saved state 20 with cookies",
    );
    report.record(
        "phaseB.same_exe_ordinary_start",
        same_bytes(&exe_before_b, &m6::binary()),
        "ordinary second start of the same executable path",
    );
    report.record(
        "phaseB.applied_v2_four_scripts",
        applied_record_matches(&install) && !pending_path(&install).exists(),
        "v2 applied record and four B scripts; pending retired",
    );
    report.record(
        "phaseB.b_gh_plan_wire",
        marker_seen,
        "commits request carried &m8=b on the wire",
    );
    report.record(
        "phaseB.rpc_saved_task_and_zip",
        saved["state"] == 20,
        "RPC listTasks/getImage and four archived bytes",
    );
    report.record(
        "phaseB.preservation_window",
        changed == RSS_FILES.map(str::to_string).to_vec(),
        "only the four scripts changed inside the RSS installation window",
    );
    report.record(
        "negative.single_worker_barrier",
        negative_timeout && negative_distinct == 1,
        "one worker: barrier expired with one distinct body",
    );

    // Reclaim only the generated install trees; the report and its probe
    // evidence are the retained artifacts.
    std::fs::remove_dir_all(&install).expect("reclaim install");
    std::fs::remove_dir_all(&negative).expect("reclaim negative install");
    report.record(
        "cleanup.owned_roots",
        !install.exists() && !negative.exists(),
        "both generated install roots removed",
    );
    report.field(
        "cleanup",
        json!({
            "install_retained": false,
            "temp_root": inputs.root.to_string_lossy(),
        }),
    );
    report.write(&inputs.report);
}

// MARK: - shared helpers used by the chain

fn free_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("free port");
    listener.local_addr().expect("free addr").port()
}

fn chain_fixture_facts(origin: &Server, since: usize, gallery: &ResumeGallery) -> String {
    let requests = origin.requests();
    let since = since.min(requests.len());
    let mut counts = [0usize; 6];
    for request in &requests[since..] {
        let slot = match request.target.as_str() {
            COMMITS_TARGET => 0,
            COMMITS_TARGET_B => 1,
            target if target.ends_with(".manifest.json") => 2,
            target if target.ends_with(".sig") => 3,
            target if target.ends_with("xeHentai-rss.zip") => 4,
            _ => 5,
        };
        counts[slot] += 1;
    }
    let (distinct, bodies, old, timed_out) = gallery.stats();
    format!(
        "requests commits_a={} commits_b={} info={} sig={} payload={} other={} \
         chain_seen={} gallery_seen={} bodies={bodies} old_bodies={old} \
         barrier_distinct={distinct} barrier_timeout={timed_out}",
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4],
        counts[5],
        gallery.seen(),
        gallery.seen(),
    )
}
