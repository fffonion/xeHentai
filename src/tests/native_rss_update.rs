//! M8: one REAL signed RSS update applied to the ordinary native executable.
//!
//! The case installs the production resource tree next to a copied host, binds
//! the trusted source to an owned loopback fixture and a synthetic Ed25519 test
//! key (memory only), then drives the actual executable twice:
//!
//! 1. `--auto-update download` publishes the signed pending candidate while the
//!    host is still running and the live RSS scripts are still release A;
//! 2. an ordinary second start applies the candidate through the OLD release's
//!    startup controller and then the NEW business module really runs: its own
//!    commit request carries the B marker, so the success is the executable's
//!    observable behavior, not a displayed version or a fixture simulation.
//!
//! Everything here is cross-platform: loopback std sockets, a copied executable,
//! real child processes and the existing interactive prompt/URL-completion exit
//! path. No POSIX signal, no Unix permission call and no external network.
//!
//! On stdin EOF the production prompt returns `PromptError::Io(UnexpectedEof)`,
//! which `main` reports with exit code 1; a graceful `0` therefore comes from the
//! existing URL-completion auto-exit (`src/main.rs` Stop::AutoExit). The fixture
//! gallery holds its response until the required event has been observed, so the
//! host provably stays alive across that event without a fixed sleep and without
//! killing it.

use std::collections::{BTreeMap, HashMap};
use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use ed25519_dalek::{Signer, SigningKey};

use super::m6;

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");
/// The private installer area the startup controller owns.
const AREA: &str = ".xehentai-update";
/// The four scripts an automatic update may replace, exactly.
const RSS_FILES: [&str; 4] = [
    "rss/filters/match.rss",
    "rss/rpc/handlers.rss",
    "rss/updater/github.rss",
    "rss/updater/updater.rss",
];

const UPDATE_ID: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";
/// The same two-entry commits document the accepted M7/M8 transport cases use.
const COMMITS: &str = "[{\"sha\": \"a1b2c3d4e5f60718293a4b5c6d7e8f9012345678\", \
    \"commit\": {\"author\": {\"date\": \"2026-10-31T12:34:56Z\"}, \
    \"message\": \"first entry\\r\\nwith CRLF\\r\\nthird line\"}}, \
    {\"sha\": \"0f1e2d3c4b5a69788796a5b4c3d2e1f00f1e2d3c\", \
    \"commit\": {\"author\": {\"date\": \"2026-10-30T01:02:03Z\"}, \
    \"message\": \"second entry\"}}]";
const COMMITS_TARGET: &str = "/repos/fffonion/xeHentai/commits?sha=rss";
/// Release B rewrites the real request construction: the commits query carries
/// this marker. Nothing here is production data; the live scripts never get it.
const COMMITS_TARGET_B: &str = "/repos/fffonion/xeHentai/commits?sha=rss&m8=b";
/// A valid synthetic session/cookie pair: the interactive login question is
/// skipped and no credential request is made.
const COOKIE: &str = "ipb_member_id=42; ipb_pass_hash=cafebabe";
/// The fixed synthetic test seed whose public PEM ships with the updater tests.
const TEST_SEED_HEX: &str = "4efca9f65359d358c489465852634f818ee7b34ed5922e015e5b24b80fb78b3b";
const TEST_PUBLIC_PEM: &str = include_str!("../updater/testdata/m7p-test-public.pem");
const GALLERY_URL: &str = "http://e-hentai.org/g/1/aaaa";

// MARK: - one real loopback source

#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }
}

/// One real HTTP source for the update API and the package sidecars. It records
/// every request head and answers with the exact registered bytes.
struct Server {
    ip: String,
    port: u16,
    routes: Arc<Mutex<HashMap<String, Vec<u8>>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Server {
    fn start(routes: Vec<(&str, Vec<u8>)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind update fixture");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("local addr").port();
        let routes: HashMap<String, Vec<u8>> = routes
            .into_iter()
            .map(|(path, body)| (path.to_string(), body))
            .collect();
        let routes = Arc::new(Mutex::new(routes));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let table = Arc::clone(&routes);
        let seen = Arc::clone(&requests);
        let stopping = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let table = Arc::clone(&table);
                        let seen = Arc::clone(&seen);
                        std::thread::spawn(move || serve_http(socket, &table, &seen));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self {
            ip: "127.0.0.1".to_string(),
            port,
            routes,
            requests,
            stop,
            handle: Some(handle),
        }
    }

    fn origin(&self) -> String {
        format!("http://{}:{}", self.ip, self.port)
    }

    fn set(&self, path: &str, body: Vec<u8>) {
        self.routes.lock().unwrap().insert(path.to_string(), body);
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect((self.ip.as_str(), self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_http(
    mut socket: TcpStream,
    routes: &Arc<Mutex<HashMap<String, Vec<u8>>>>,
    seen: &Arc<Mutex<Vec<Request>>>,
) {
    let Some(request) = read_head(&mut socket) else {
        return;
    };
    let body = routes.lock().unwrap().get(&request.target).cloned();
    seen.lock().unwrap().push(request);
    let Some(body) = body else {
        let head = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let _ = socket.write_all(head);
        let _ = socket.flush();
        let _ = socket.shutdown(Shutdown::Both);
        return;
    };
    let head = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes());
    let _ = socket.write_all(&body);
    let _ = socket.flush();
    let _ = socket.shutdown(Shutdown::Both);
}

fn read_head(socket: &mut TcpStream) -> Option<Request> {
    socket
        .set_read_timeout(Some(Duration::from_secs(20)))
        .expect("read timeout");
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    while !buffer.windows(4).any(|window| window == b"\r\n\r\n") {
        match socket.read(&mut chunk) {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
        if buffer.len() > 64 * 1024 {
            return None;
        }
    }
    let head = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
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
    Some(Request {
        method,
        target,
        headers,
    })
}

// MARK: - the gated gallery the real download task talks to

#[derive(Debug)]
struct Image {
    key: String,
    bytes: Vec<u8>,
}

/// A real HTTP proxy endpoint for `http://e-hentai.org/...`. It answers only
/// after `release()`: the successful download is therefore held at its first
/// response while the update check publishes the pending candidate, so the
/// timeout/event ordering is explicit instead of a race.
struct Gallery {
    port: u16,
    paths: Arc<Mutex<Vec<String>>>,
    gate: Arc<(Mutex<bool>, Condvar)>,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl Gallery {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind gallery fixture");
        listener.set_nonblocking(true).expect("nonblocking");
        let port = listener.local_addr().expect("local addr").port();
        let paths = Arc::new(Mutex::new(Vec::new()));
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&paths);
        let held = Arc::clone(&gate);
        let stopping = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let seen = Arc::clone(&seen);
                        let held = Arc::clone(&held);
                        let stopping = Arc::clone(&stopping);
                        std::thread::spawn(move || serve_gallery(socket, &seen, &held, &stopping));
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
            paths,
            gate,
            stop,
            handle: Some(handle),
        }
    }

    fn proxy(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn paths(&self) -> Vec<String> {
        self.paths.lock().unwrap().clone()
    }

    fn released(&self) -> bool {
        *self.gate.0.lock().unwrap()
    }

    fn release(&self) {
        let (lock, cond) = &*self.gate;
        *lock.lock().unwrap() = true;
        cond.notify_all();
    }
}

impl Drop for Gallery {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.release();
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve_gallery(
    mut socket: TcpStream,
    seen: &Arc<Mutex<Vec<String>>>,
    gate: &Arc<(Mutex<bool>, Condvar)>,
    stop: &Arc<AtomicBool>,
) {
    let Some(request) = read_head(&mut socket) else {
        return;
    };
    // A proxy receives the absolute form; keep the path part for dispatch.
    let target = request.target;
    let path = match target.find("://") {
        Some(index) => match target[index + 3..].find('/') {
            Some(slash) => target[index + 3 + slash..].to_string(),
            None => "/".to_string(),
        },
        None => target.clone(),
    };
    seen.lock().unwrap().push(target);
    // Hold every response until the test has seen the real required event.
    let (lock, cond) = &**gate;
    let mut released = lock.lock().unwrap();
    while !*released {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let (next, _) = cond
            .wait_timeout(released, Duration::from_millis(20))
            .unwrap();
        released = next;
    }
    drop(released);
    let image = image();
    if path.contains("/g/") && path.contains("?p=") {
        respond(
            &mut socket,
            "200 OK",
            "text/html; charset=utf-8",
            page_body().as_bytes(),
        );
    } else if path.contains("/g/") {
        let mut body = metadata_body();
        body.push_str(&page_body());
        respond(
            &mut socket,
            "200 OK",
            "text/html; charset=utf-8",
            body.as_bytes(),
        );
    } else if path.contains("/s/") {
        respond(
            &mut socket,
            "200 OK",
            "text/html; charset=utf-8",
            image_page_body(&image).as_bytes(),
        );
    } else if path.contains(image.key.as_str()) {
        // Binary content type: the production consumer only opens a streaming
        // body for a non-text response, exactly as a real image gallery does.
        respond(&mut socket, "200 OK", "image/jpeg", &image.bytes);
    } else {
        respond(&mut socket, "404 Not Found", "text/plain", b"missing");
    }
}

fn respond(socket: &mut TcpStream, status: &str, mime: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes());
    let _ = socket.write_all(body);
    let _ = socket.flush();
    let _ = socket.shutdown(Shutdown::Both);
}

/// One deterministic gallery page and its one image: the same wire shapes the
/// accepted M6 gallery fixture serves, reduced to the minimum this case needs.
fn image() -> Image {
    let key = format!("{:010x}{:030x}", 0xabc000, 0);
    let bytes: Vec<u8> = (0..2048u32).map(|value| (value % 251) as u8).collect();
    Image { key, bytes }
}

fn metadata_body() -> String {
    "=\"gj\">日本語タイトル</h1>=\"gn\">English title</h1>\
     Length:</td><td class=\"gdt2\">1 pages</td>\
     Showing 1 - 20 of 1 images"
        .to_string()
}

fn page_body() -> String {
    "<a href=\"http://e-hentai.org/s/abc0000000/700000-1\"><div title=\"Page 1\"".to_string()
}

fn image_page_body(image: &Image) -> String {
    format!(
        "<img src=\"http://img.test/{}-{}-200-0-jpg/keystamp=abc\" style=\"max-width:100%\">\
         </a></div><div>unique-0.jpg :: 100x200 :: 2 KB</di>\
         return nl('700000-0')",
        image.key,
        image.bytes.len()
    )
}

// MARK: - signed release B

fn signing_key() -> SigningKey {
    let mut seed = [0u8; 32];
    let bytes = TEST_SEED_HEX.as_bytes();
    for index in 0..32 {
        let pair = std::str::from_utf8(&bytes[index * 2..index * 2 + 2]).expect("hex pair");
        seed[index] = u8::from_str_radix(pair, 16).expect("hex byte");
    }
    SigningKey::from_bytes(&seed)
}

/// Bind the one trusted source constant block in the copied `github.rss`. Only
/// the fixture origins and the synthetic test key change; the business body is
/// the production module.
fn bind(mut source: String, api: &str, package: &str) -> String {
    source = source.replace(
        "api_origin: \"https://api.github.com\",",
        &format!("api_origin: \"{api}\","),
    );
    source = source.replace(
        "package_download_origin: \"https://github.com\",",
        &format!("package_download_origin: \"{package}\","),
    );
    let pem_literal = TEST_PUBLIC_PEM.replace('\n', "\\n");
    source.replace(
        "verification_key: null,",
        &format!("verification_key: \"{pem_literal}\","),
    )
}

/// Release B: the same four scripts, each carrying a test comment so the whole
/// four-file install is byte-observable, and the REAL request construction
/// changed so the commits query carries the B marker.
fn candidate(origin: &str) -> Vec<(String, Vec<u8>)> {
    let mut files = Vec::new();
    for relative in RSS_FILES {
        let mut body = std::fs::read(Path::new(MANIFEST).join(relative)).expect("production RSS");
        if relative == "rss/updater/github.rss" {
            let source = String::from_utf8(body).expect("UTF-8 RSS");
            let source = bind(source, origin, origin).replace(
                "\"/commits?sha=\" + channel;",
                "\"/commits?sha=\" + channel + \"&m8=b\";",
            );
            body = source.into_bytes();
        }
        let mut marked = format!("// m8 candidate B: {relative}\n").into_bytes();
        marked.extend_from_slice(&body);
        files.push((relative.to_string(), marked));
    }
    files
}

/// The v2 descriptor and detached signature over the EXACT bytes, the same
/// envelope the accepted M7 fixture produces with the current ABI constants.
fn signed_package(files: &[(String, Vec<u8>)]) -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let prefix = format!("xeHentai-{UPDATE_ID}/");
    let members: Vec<(String, Vec<u8>)> = files
        .iter()
        .map(|(path, body)| (format!("{prefix}{path}"), body.clone()))
        .collect();
    let payload = crate::updater::package::store_zip(&members).expect("payload archive");
    let digest = crate::updater::descriptor::sha256_hex(&payload);
    let entries: Vec<serde_json::Value> = files
        .iter()
        .map(|(path, body)| serde_json::json!({ "path": path, "len": body.len() }))
        .collect();
    let info = serde_json::to_vec(&serde_json::json!({
        "v": 2,
        "update_id": UPDATE_ID,
        "host_abi": crate::r#const::HOST_ABI,
        "script_abi": crate::r#const::SCRIPT_ABI,
        "archive_sha256": digest,
        "entries": entries,
    }))
    .expect("descriptor json");
    let sig = signing_key().sign(&info).to_bytes().to_vec();
    (info, sig, payload)
}

fn package_target(origin: &str, suffix: &str) -> String {
    let link = format!(
        "{origin}/fffonion/xeHentai/releases/download/rss-{UPDATE_ID}/xeHentai-rss.zip{suffix}"
    );
    match link.find("://") {
        Some(index) => match link[index + 3..].find('/') {
            Some(slash) => link[index + 3 + slash..].to_string(),
            None => "/".to_string(),
        },
        None => link,
    }
}

// MARK: - the installed ordinary host

fn exe_name() -> String {
    format!("xeHentai{}", std::env::consts::EXE_SUFFIX)
}

fn pending_path(install: &Path) -> PathBuf {
    install.join(AREA).join("pending").join("src.zip")
}

fn installed_host(root: &Path, api: &str) -> (PathBuf, Vec<u8>) {
    let install = root.join("install");
    std::fs::create_dir_all(&install).expect("install dir");
    let exe = install.join(exe_name());
    std::fs::copy(m6::binary(), &exe).expect("copy the ordinary host");
    m6::copy_tree(&Path::new(MANIFEST).join("rss"), &install.join("rss"));
    m6::copy_tree(
        &Path::new(MANIFEST).join("locales"),
        &install.join("locales"),
    );
    m6::copy_tree(&Path::new(MANIFEST).join("ui"), &install.join("ui"));
    std::fs::copy(
        Path::new(MANIFEST).join("config.example.rss"),
        install.join("config.rss"),
    )
    .expect("config fixture");
    std::fs::write(
        install.join("h.json"),
        serde_json::json!({
            "tasks": {},
            "cookies": { "ipb_member_id": "42", "ipb_pass_hash": "cafebabe" }
        })
        .to_string(),
    )
    .expect("session fixture");
    // Release A keeps the production business body; only the trusted binding is
    // the owned fixture one.
    let github = install.join("rss/updater/github.rss");
    let source = std::fs::read_to_string(&github).expect("github.rss");
    let bound = bind(source, api, api).into_bytes();
    std::fs::write(&github, &bound).expect("write binding A");
    (exe, bound)
}

// MARK: - early-exit facts (test-only diagnostics)

/// The only child-output facts this case reports. Each entry is a fixed message
/// that `src/main.rs`, the startup controller, the prompt, the Rust runtime or
/// the producer's pool failure path emit verbatim; nothing else is classified
/// and the captured bytes are dropped after the scan.
///
/// `proxy_pool_exhausted` is the production `ProxyError::NoProxy` Display text
/// (`src/proxy.rs`), surfaced unchanged by `HttpError::Pool` (`src/worker.rs`)
/// and logged by the producer as `task #<guid> error: <error>`
/// (`src/core/runtime.rs`). The `avaliable` spelling is the production text and
/// must not be corrected here.
const OUTPUT_LABELS: [(&str, &str); 7] = [
    ("loop_finished", "application task loop finished"),
    (
        "proxy_pool_exhausted",
        "try to use proxy but no proxies avaliable",
    ),
    ("update_failed", "Failure when updating program:"),
    ("prompt_io", "xeHentai: cannot read input:"),
    ("startup_stopped", "xeHentai: update startup stopped"),
    ("session_error", "xeHentai: session error:"),
    ("child_panic", "panicked at"),
];

/// Fixed closed categories for the existing updater error text. Only these
/// five producer strings are recognised; everything else stays `unknown`, so a
/// diagnostic never carries raw error text, a path or a URL.
const UPDATE_FAILURE_LABELS: [(&str, &str); 5] = [
    (
        "transport_deadline",
        "transport: update transport deadline exceeded",
    ),
    ("transport_request", "transport: request failed:"),
    ("package_rejected", "update package rejected:"),
    ("installation_failed", "update installation failed:"),
    ("source_binding_error", "update source binding error:"),
];

/// The first fixed updater category present in the captured stream, else
/// `unknown`. Returns only the closed label.
fn scan_update_failure(bytes: &[u8]) -> &'static str {
    UPDATE_FAILURE_LABELS
        .iter()
        .find(|(_, needle)| contains(bytes, needle.as_bytes()))
        .map(|(name, _)| *name)
        .unwrap_or("unknown")
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Recognise the fixed messages present in one captured stream. It returns the
/// label names only, never a slice of the output.
fn scan_labels(bytes: &[u8]) -> Vec<&'static str> {
    OUTPUT_LABELS
        .iter()
        .filter(|(_, needle)| contains(bytes, needle.as_bytes()))
        .map(|(name, _)| *name)
        .collect()
}

/// Render the observed exit facts: the code, both stream lengths and the fixed
/// labels. Pure data in, fixed text out.
fn exit_facts_text(
    exited: bool,
    code: Option<i32>,
    out_len: usize,
    err_len: usize,
    labels: &[&str],
) -> String {
    let code = match code {
        Some(code) => code.to_string(),
        None => "none".to_string(),
    };
    format!(
        "exited={exited} code={code} stdout_bytes={out_len} stderr_bytes={err_len} labels={labels:?}"
    )
}

/// `yes`/`no` from a metadata answer; an unreadable path is `unknown` instead
/// of being folded into either answer.
fn path_state(path: &Path) -> &'static str {
    match path.try_exists() {
        Ok(true) => "yes",
        Ok(false) => "no",
        Err(_) => "unknown",
    }
}

/// Whether the four live scripts are already release B. A definitive
/// difference is `no`; only an unreadable script with no differing one is
/// `unknown`.
fn release_b_state(install: &Path, files: &[(String, Vec<u8>)]) -> &'static str {
    let mut differs = false;
    let mut unreadable = false;
    for (path, body) in files {
        match std::fs::read(install.join(path)) {
            Ok(live) => differs |= &live != body,
            Err(_) => unreadable = true,
        }
    }
    if differs {
        "no"
    } else if unreadable {
        "unknown"
    } else {
        "yes"
    }
}

/// The private installer booleans and, when the candidate is known, whether the
/// four live scripts are release B. These distinguish an exit before the apply
/// from one after it.
fn install_facts(install: &Path, expected: Option<&[(String, Vec<u8>)]>) -> String {
    let pending = path_state(&pending_path(install));
    let state = path_state(&install.join(AREA).join("state.json"));
    match expected {
        None => format!("pending_zip={pending} state_json={state}"),
        Some(files) => format!(
            "files_b={} pending_zip={pending} state_json={state}",
            release_b_state(install, files)
        ),
    }
}

/// The true successful-apply boundary: the private v2 record naming this
/// candidate. `updater/install.rs` replaces the four live scripts first and
/// writes this record last, inside the same apply closure, so a partial apply
/// leaves the four files at release B without ever satisfying this condition.
/// An unreadable path or unparseable document is `false`, never a success.
fn applied_record_matches(install: &Path) -> bool {
    let Ok(bytes) = std::fs::read(install.join(AREA).join("state.json")) else {
        return false;
    };
    let Ok(state) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return false;
    };
    state["v"] == 2 && state["active"]["update_id"] == UPDATE_ID
}

/// What the fixture already observed: counts per known update path (never the
/// URL or headers) and the held gallery's request count and release state.
fn fixture_facts(origin: &Server, since: usize, gallery: &Gallery) -> String {
    let requests = origin.requests();
    let since = since.min(requests.len());
    // commits_a, commits_b, info, sig, payload, other
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
    format!(
        "requests commits_a={} commits_b={} info={} sig={} payload={} other={} \
         gallery_seen={} gallery_released={}",
        counts[0],
        counts[1],
        counts[2],
        counts[3],
        counts[4],
        counts[5],
        gallery.paths().len(),
        gallery.released(),
    )
}

struct Run {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: Option<JoinHandle<Vec<u8>>>,
    stderr: Option<JoinHandle<Vec<u8>>>,
    /// Whether `try_wait`/`wait_exit` observed a finish, and the code it
    /// reported. A finished child without a code (a Unix signal) stays `None`.
    exited: bool,
    exit_code: Option<i32>,
    /// Captured-output facts only: the byte lengths and the fixed labels this
    /// case can recognise. The bytes themselves are dropped after the scan.
    out_len: usize,
    err_len: usize,
    labels: Vec<&'static str>,
    /// The first fixed updater failure category seen in the captured streams,
    /// else `unknown`. Closed labels only; no raw text is retained.
    update_failure: &'static str,
}

fn spawn(install: &Path, args: &[&str]) -> Run {
    let home = install.join("home");
    let tmp = install.join("tmp");
    let cache = install.join("cache");
    for dir in [&home, &tmp, &cache] {
        std::fs::create_dir_all(dir).expect("child dir");
    }
    let mut command = Command::new(install.join(exe_name()));
    command
        .current_dir(install)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // Inherited capture/emit and test-only switches must not reach the host.
    for name in [
        "XEH_EMIT_ROOT",
        "XEH_EVIDENCE_DIR",
        "XEH_CAPTURE_OUT",
        "RUST_TEST_THREADS",
        "XEH_BUILD_ID",
    ] {
        command.env_remove(name);
    }
    // The fixed-label scan needs the C-locale message text. `LC_MESSAGES`
    // overrides `LANG`/`LC_ALL=C` for gettext-style message catalogues, so a
    // caller's locale must not reach the host; the product locale logic is
    // unchanged.
    command.env_remove("LC_MESSAGES");
    // Keep the platform runtime variables (SystemRoot/PATH/...); only the
    // per-case user/temp roots are redirected into this case's own install.
    command
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .env("TMPDIR", &tmp)
        .env("TEMP", &tmp)
        .env("TMP", &tmp)
        .env("XDG_CACHE_HOME", &cache)
        .env("LANG", "C")
        .env("LC_ALL", "C");
    let mut child = command.spawn().expect("spawn the installed host");
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().map(|pipe| {
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let mut pipe = pipe;
            let _ = pipe.read_to_end(&mut sink);
            sink
        })
    });
    let stderr = child.stderr.take().map(|pipe| {
        std::thread::spawn(move || {
            let mut sink = Vec::new();
            let mut pipe = pipe;
            let _ = pipe.read_to_end(&mut sink);
            sink
        })
    });
    Run {
        child,
        stdin,
        stdout,
        stderr,
        exited: false,
        exit_code: None,
        out_len: 0,
        err_len: 0,
        labels: Vec::new(),
        update_failure: "unknown",
    }
}

impl Run {
    fn answer(&mut self, text: &str) {
        if let Some(stdin) = self.stdin.as_mut() {
            stdin.write_all(text.as_bytes()).expect("write prompts");
            stdin.flush().expect("flush prompts");
        }
    }

    /// Record the finish this guard observed. The code is part of the
    /// early-exit diagnostic; the status itself is never discarded silently.
    fn note_exit(&mut self, status: ExitStatus) {
        self.exited = true;
        self.exit_code = status.code();
    }

    /// `try_wait` is part of the guard: an error is never ignored and never
    /// leaves a live child behind.
    fn running(&mut self) -> bool {
        match self.child.try_wait() {
            Ok(None) => true,
            Ok(Some(status)) => {
                self.note_exit(status);
                false
            }
            Err(error) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
                panic!("child state is unknown: {error}");
            }
        }
    }

    fn wait_exit(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.child.try_wait() {
                Ok(Some(status)) => {
                    self.note_exit(status);
                    return Some(status);
                }
                Ok(None) => {}
                Err(error) => {
                    let _ = self.child.kill();
                    let _ = self.child.wait();
                    panic!("child state is unknown: {error}");
                }
            }
            if Instant::now() > deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Bounded exit observation followed by the reader joins. Both captured
    /// readers block in `read_to_end` until the pipe reaches EOF, and only the
    /// child's exit closes it: a child that did not finish within `timeout`
    /// must therefore be killed and reaped BEFORE `collect`, or the join would
    /// outlast the deadline and the promised failure would never be reached.
    /// A killed child is never a natural exit — `None` is returned unchanged
    /// and `note_exit` is not called, so `exited` stays false.
    fn wait_exit_then_collect(&mut self, timeout: Duration) -> Option<ExitStatus> {
        let status = self.wait_exit(timeout);
        if status.is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        self.collect();
        status
    }

    /// Join both readers, keep only the lengths and the fixed labels, and drop
    /// the captured bytes so no raw output can reach a report.
    fn collect(&mut self) {
        if let Some(handle) = self.stdout.take()
            && let Ok(bytes) = handle.join()
        {
            self.out_len = bytes.len();
            self.note_labels(&bytes);
        }
        if let Some(handle) = self.stderr.take()
            && let Ok(bytes) = handle.join()
        {
            self.err_len = bytes.len();
            self.note_labels(&bytes);
        }
    }

    fn note_labels(&mut self, bytes: &[u8]) {
        for label in scan_labels(bytes) {
            if !self.labels.contains(&label) {
                self.labels.push(label);
            }
        }
        let failure = scan_update_failure(bytes);
        if failure != "unknown" && self.update_failure == "unknown" {
            self.update_failure = failure;
        }
    }

    fn exit_facts(&self) -> String {
        format!(
            "{} failure_category={}",
            exit_facts_text(
                self.exited,
                self.exit_code,
                self.out_len,
                self.err_len,
                &self.labels,
            ),
            self.update_failure
        )
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stdin.take();
        match self.child.try_wait() {
            Ok(None) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
            Ok(Some(_)) => {}
            Err(_) => {
                let _ = self.child.kill();
                let _ = self.child.wait();
            }
        }
        self.collect();
    }
}

/// Bounded observation of a real event (a file appearing, a request arriving or
/// a live script changing). The condition is the evidence; the poll is only a
/// transfer mechanism, and an early child exit is reported as its own phase
/// together with the authoritative exit code and the fixture/private facts.
fn wait_event<F, C>(run: &mut Run, timeout: Duration, phase: &str, check: F, facts: C)
where
    F: FnMut() -> bool,
    C: FnMut() -> String,
{
    wait_event_with_fixture(run, timeout, phase, check, facts, None)
}

/// The same bounded waiter with an optional typed fixture tuple
/// `(&Server, seen_before, &Gallery)`. On failure it appends the already
/// existing safe request-stage counts (commits/info/signature/payload and the
/// held gallery's seen/released) computed at that moment. Predicates,
/// deadlines and panic propagation are unchanged.
fn wait_event_with_fixture<F, C>(
    run: &mut Run,
    timeout: Duration,
    phase: &str,
    mut check: F,
    mut facts: C,
    fixture: Option<(&Server, usize, &Gallery)>,
) where
    F: FnMut() -> bool,
    C: FnMut() -> String,
{
    let deadline = Instant::now() + timeout;
    let extra_facts = || match fixture {
        Some((origin, since, gallery)) => fixture_facts(origin, since, gallery),
        None => String::new(),
    };
    loop {
        if check() {
            return;
        }
        if !run.running() {
            run.collect();
            let facts_text = facts();
            let fixture_text = extra_facts();
            panic!(
                "{phase}: the host exited before the required event; {} {facts_text} {fixture_text}",
                run.exit_facts()
            );
        }
        if Instant::now() > deadline {
            let facts_text = facts();
            let fixture_text = extra_facts();
            panic!(
                "{phase}: bounded wait for the required event expired; {} {facts_text} {fixture_text}",
                run.exit_facts()
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// MARK: - byte baselines

/// Snapshot only the user-facing files this case must preserve: the four RSS
/// scripts, the language resources, the UI assets, the configuration source and
/// the session. The executable and ordinary log are handled separately.
fn snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    for relative in ["rss", "locales", "ui", "config.rss", "h.json"] {
        let path = root.join(relative);
        if path.is_file() {
            out.insert(
                relative.to_string(),
                std::fs::read(&path).expect("user file"),
            );
        } else if path.is_dir() {
            walk(&path, Path::new(relative), &mut out);
        }
    }
    out
}

fn private_snapshot(root: &Path) -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let area = root.join(AREA);
    if area.is_dir() {
        walk(&area, Path::new(""), &mut out);
    }
    out
}

fn walk(dir: &Path, relative: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("tree dir")
        .map(|entry| entry.expect("entry").path())
        .collect();
    entries.sort();
    for path in entries {
        let name = relative.join(path.file_name().expect("name"));
        if path.is_dir() {
            walk(&path, &name, out);
        } else {
            let key = name.to_string_lossy().replace('\\', "/");
            out.insert(key, std::fs::read(&path).expect("tree file"));
        }
    }
}

fn diff(
    before: &BTreeMap<String, Vec<u8>>,
    after: &BTreeMap<String, Vec<u8>>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut changed = Vec::new();
    for (path, bytes) in before {
        if let Some(next) = after.get(path)
            && next != bytes
        {
            changed.push(path.clone());
        }
    }
    let added = after
        .keys()
        .filter(|path| !before.contains_key(*path))
        .cloned()
        .collect();
    let removed = before
        .keys()
        .filter(|path| !after.contains_key(*path))
        .cloned()
        .collect();
    (changed, added, removed)
}

/// Compare two files in fixed-size chunks: no hash, no extra full baseline copy.
fn same_bytes(left: &Path, right: &Path) -> bool {
    let (Ok(mut left), Ok(mut right)) = (std::fs::File::open(left), std::fs::File::open(right))
    else {
        return false;
    };
    let (Ok(left_meta), Ok(right_meta)) = (left.metadata(), right.metadata()) else {
        return false;
    };
    if left_meta.len() != right_meta.len() {
        return false;
    }
    let mut left_chunk = [0u8; 64 * 1024];
    let mut right_chunk = [0u8; 64 * 1024];
    loop {
        let left_read = left.read(&mut left_chunk).expect("read installed host");
        let right_read = right.read(&mut right_chunk).expect("read build host");
        if left_read != right_read {
            return false;
        }
        if left_read == 0 {
            return true;
        }
        if left_chunk[..left_read] != right_chunk[..right_read] {
            return false;
        }
    }
}

// MARK: - the one cross-platform consumer

/// The real phase-B start shared by the success case and the held-gallery
/// control: the same install, the same already-published pending candidate, the
/// same argv and prompt answers, and a newly held gallery. Only the behaviour
/// after this point differs between the two cases.
struct PhaseB {
    run: Run,
    gallery: Gallery,
    seen_before: usize,
}

fn start_phase_b(install: &Path, origin: &Server) -> PhaseB {
    origin.set(COMMITS_TARGET_B, COMMITS.as_bytes().to_vec());
    let seen_before = origin.requests().len();
    let gallery = Gallery::start();
    let mut run = spawn(install, &["-i", "-c", COOKIE, "--auto-update", "check"]);
    run.answer(&format!(
        "{GALLERY_URL}\nn\n{}\ndl-b\nn\nn\nn\n\n",
        gallery.proxy()
    ));
    PhaseB {
        run,
        gallery,
        seen_before,
    }
}

/// Wait for the true successful-apply boundary: the private v2 applied record
/// naming this candidate AND the four live scripts equal to B. The production
/// installer writes the record after the four replacements inside the same
/// apply closure, so this is the records boundary, not merely on-disk bytes;
/// the later four-byte assert stays exactly as it was.
fn wait_applied_record(
    run: &mut Run,
    install: &Path,
    origin: &Server,
    seen_before: usize,
    gallery: &Gallery,
    files: &[(String, Vec<u8>)],
) {
    let expected: BTreeMap<String, Vec<u8>> = files.iter().cloned().collect();
    wait_event_with_fixture(
        run,
        Duration::from_secs(120),
        "phase B startup apply",
        || {
            applied_record_matches(install)
                && expected.iter().all(|(path, body)| {
                    std::fs::read(install.join(path))
                        .map(|live| &live == body)
                        .unwrap_or(false)
                })
        },
        || install_facts(install, Some(files)),
        Some((origin, seen_before, gallery)),
    );
}

#[test]
fn native_signed_rss_update_uses_the_same_host_and_preserves_non_rss_files() {
    let root = tempfile::tempdir().expect("case root");
    let origin = Arc::new(Server::start(Vec::new()));
    let files = candidate(&origin.origin());
    let (info, sig, payload) = signed_package(&files);
    origin.set(COMMITS_TARGET, COMMITS.as_bytes().to_vec());
    origin.set(&package_target(&origin.origin(), ""), payload.clone());
    origin.set(&package_target(&origin.origin(), ".manifest.json"), info);
    origin.set(&package_target(&origin.origin(), ".sig"), sig);

    let (exe, binding_a) = installed_host(root.path(), &origin.origin());
    let install = root.path().join("install");
    assert!(
        !pending_path(&install).exists(),
        "phase A: a fresh install must not have a pending candidate"
    );
    let before = snapshot(&install);
    assert!(
        before.get("rss/updater/github.rss") == Some(&binding_a),
        "phase A: the preserved baseline must be the bound release A script"
    );

    // Release A: publish the signed pending candidate while A still runs.
    let gallery_a = Gallery::start();
    let mut run = spawn(&install, &["-i", "-c", COOKIE, "--auto-update", "download"]);
    run.answer(&format!(
        "{GALLERY_URL}\nn\n{}\ndl-a\nn\nn\nn\n\n",
        gallery_a.proxy()
    ));
    wait_event_with_fixture(
        &mut run,
        Duration::from_secs(120),
        "phase A pending publication",
        || pending_path(&install).is_file(),
        || install_facts(&install, None),
        Some((&origin, 0, &gallery_a)),
    );
    assert!(
        run.running(),
        "phase A: the host must still be running when the pending candidate is published"
    );
    // Pending publication must leave every live script exactly at release A,
    // not just the updater script bound to the fixture source.
    for path in RSS_FILES {
        let live = std::fs::read(install.join(path)).expect("phase A: live RSS script");
        assert!(
            before.get(path) == Some(&live),
            "phase A: the live script {path} changed while release A was still running"
        );
    }
    let requests_a = origin.requests();
    let targets_a: Vec<&str> = requests_a
        .iter()
        .map(|request| request.target.as_str())
        .collect();
    assert!(
        targets_a.contains(&COMMITS_TARGET),
        "phase A: the downloaded check must use release A's commits query: {targets_a:?}"
    );
    assert!(
        !targets_a.contains(&COMMITS_TARGET_B),
        "phase A: release B must not have run yet: {targets_a:?}"
    );
    let commits = requests_a
        .iter()
        .find(|request| request.target == COMMITS_TARGET)
        .expect("phase A commits request");
    assert_eq!(commits.method, "GET");
    assert_eq!(commits.header("cookie"), None);
    assert_eq!(commits.header("authorization"), None);

    // Close the prompt input and let the held download finish through the
    // ordinary URL-completion auto-exit.
    run.stdin.take();
    gallery_a.release();
    let status = run
        .wait_exit(Duration::from_secs(120))
        .expect("phase A: the ordinary host must exit after its URL completed");
    run.collect();
    assert_eq!(status.code(), Some(0), "phase A: exit status");
    assert_eq!(
        gallery_a
            .paths()
            .iter()
            .filter(|path| path.contains("img.test"))
            .count(),
        1,
        "phase A: the real download must fetch the gallery image"
    );
    let session_after_a = std::fs::read(install.join("h.json")).expect("session after A");
    let private_after_a = private_snapshot(&install);
    let snapshot_after_a = snapshot(&install);

    // Release B: the next ordinary start applies the candidate through the OLD
    // startup controller, then the NEW business module really drives a request.
    let PhaseB {
        mut run,
        gallery: gallery_b,
        seen_before,
    } = start_phase_b(&install, &origin);
    wait_applied_record(
        &mut run,
        &install,
        &origin,
        seen_before,
        &gallery_b,
        files.as_slice(),
    );
    assert!(
        applied_record_matches(&install),
        "phase B: the successful apply must publish its v2 applied record"
    );
    wait_event_with_fixture(
        &mut run,
        Duration::from_secs(120),
        "phase B release-B request",
        || {
            origin.requests()[seen_before..]
                .iter()
                .any(|request| request.target == COMMITS_TARGET_B)
        },
        || install_facts(&install, Some(files.as_slice())),
        Some((&origin, seen_before, &gallery_b)),
    );
    assert!(
        run.running(),
        "phase B: the host must still be running when release B drives its request"
    );
    let session_read_by_b = std::fs::read(install.join("h.json")).expect("session read by B");
    assert!(
        session_read_by_b == session_after_a,
        "phase B: the previous session must be readable and unchanged"
    );
    run.stdin.take();
    gallery_b.release();
    let status = run
        .wait_exit(Duration::from_secs(120))
        .expect("phase B: the ordinary host must exit after its URL completed");
    run.collect();
    assert_eq!(status.code(), Some(0), "phase B: exit status");

    // The live tree is exactly release B, and only the four RSS scripts moved.
    for (path, body) in &files {
        assert_eq!(
            &std::fs::read(install.join(path)).expect("live script"),
            body,
            "phase B: live {path} is not the candidate script"
        );
    }
    assert!(
        !pending_path(&install).exists(),
        "phase B: the applied candidate must be retired"
    );
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(install.join(AREA).join("state.json")).expect("installer state"),
    )
    .expect("state json");
    assert_eq!(state["v"], 2, "phase B: applied record version");
    assert_eq!(
        state["active"]["update_id"], UPDATE_ID,
        "phase B: applied record identity"
    );
    let session_after_b = std::fs::read(install.join("h.json")).expect("session after B");
    assert!(
        session_after_b == session_after_a,
        "phase B: the user session changed across the RSS-only update"
    );

    let after = snapshot(&install);
    let (changed, added, removed) = diff(&snapshot_after_a, &after);
    assert_eq!(
        changed,
        RSS_FILES.map(str::to_string).to_vec(),
        "the update changed a file outside the four RSS scripts"
    );
    assert!(
        added.is_empty() && removed.is_empty(),
        "the update added {added:?} or removed {removed:?} outside the RSS set"
    );
    let (private_changed, private_added, private_removed) =
        diff(&private_after_a, &private_snapshot(&install));
    assert!(
        private_changed.is_empty(),
        "private state changed: {private_changed:?}"
    );
    assert_eq!(
        private_added,
        vec!["state.json".to_string()],
        "the update published private files other than its applied record"
    );
    assert_eq!(
        private_removed,
        vec!["pending/src.zip".to_string()],
        "the update retired a private file other than its pending envelope"
    );

    // The host executable is the SAME copied file and its bytes equal the
    // independent build output it was copied from.
    assert!(
        same_bytes(&exe, &m6::binary()),
        "the automatic update replaced or rewrote the host executable"
    );
    assert!(
        origin.requests()[seen_before..]
            .iter()
            .all(|request| request.target != COMMITS_TARGET),
        "phase B: release A's script still ran after the apply"
    );
    assert!(
        before.contains_key("config.rss"),
        "the configuration source is part of the preserved baseline"
    );
}

// MARK: - the explicit long-running held-gallery control

/// The negative lifecycle control from the early-exit investigation: one REAL
/// phase-A download publishes the signed pending candidate, the ordinary second
/// host applies it, and from the true applied-record boundary on the gallery is
/// never released. Holding every response consumes the producer's proxies, so
/// the pool itself fails with `ProxyError::NoProxy`, the task fails on its own,
/// and the Runtime's AutoExit ends the host naturally.
///
/// The proof of exhaustion is the closed `proxy_pool_exhausted` label on the
/// captured output together with the natural exit 0 and the finished loop, not
/// a count of HTTP requests: `held_requests` is `Gallery::paths`, i.e. requests
/// whose head the fixture actually read. A connect that fails before reaching
/// the fixture — including the pool-exhaustion error — adds no entry, so this
/// count is not, and must not be read as, the producer's pool-failure count.
///
/// This proves the ordinary pool-failure/AutoExit path and its exit-0 semantics
/// only. It neither reproduces nor excludes any historical early exit, and
/// `commits_b` is observed only, never required to be zero or one. A killed
/// child is never reported as a natural exit.
#[test]
#[ignore = "explicit long-running lifecycle control"]
fn native_held_gallery_times_out_then_auto_exits() {
    let root = tempfile::tempdir().expect("case root");
    let origin = Arc::new(Server::start(Vec::new()));
    let files = candidate(&origin.origin());
    let (info, sig, payload) = signed_package(&files);
    origin.set(COMMITS_TARGET, COMMITS.as_bytes().to_vec());
    origin.set(&package_target(&origin.origin(), ""), payload.clone());
    origin.set(&package_target(&origin.origin(), ".manifest.json"), info);
    origin.set(&package_target(&origin.origin(), ".sig"), sig);
    let (_exe, _binding_a) = installed_host(root.path(), &origin.origin());
    let install = root.path().join("install");

    // Phase A: the same real download publishes the signed pending candidate.
    let gallery_a = Gallery::start();
    let mut run_a = spawn(&install, &["-i", "-c", COOKIE, "--auto-update", "download"]);
    run_a.answer(&format!(
        "{GALLERY_URL}\nn\n{}\ndl-a\nn\nn\nn\n\n",
        gallery_a.proxy()
    ));
    wait_event_with_fixture(
        &mut run_a,
        Duration::from_secs(120),
        "control phase A pending publication",
        || pending_path(&install).is_file(),
        || install_facts(&install, None),
        Some((&origin, 0, &gallery_a)),
    );
    assert!(
        run_a.running(),
        "control phase A: the host must still be running when the pending candidate is published"
    );
    run_a.stdin.take();
    gallery_a.release();
    let status_a = run_a
        .wait_exit(Duration::from_secs(120))
        .expect("control phase A: the ordinary host must exit after its URL completed");
    run_a.collect();
    assert_eq!(status_a.code(), Some(0), "control phase A: exit status");

    // Phase B: the ordinary second start applies the candidate. The gallery is
    // held from here on, `commits_b` is not a wait condition and nothing is
    // released.
    let PhaseB {
        mut run,
        gallery,
        seen_before,
    } = start_phase_b(&install, &origin);
    wait_applied_record(
        &mut run,
        &install,
        &origin,
        seen_before,
        &gallery,
        files.as_slice(),
    );

    let started = Instant::now();
    let status = run.wait_exit_then_collect(Duration::from_secs(150));
    let elapsed = started.elapsed();
    let exit_facts = run.exit_facts();
    let install_state = install_facts(&install, Some(files.as_slice()));
    let fixture = fixture_facts(&origin, seen_before, &gallery);
    // `Gallery::paths` records only requests whose head actually reached the
    // fixture; a failed connect, including the pool-exhaustion error, leaves no
    // entry. This is a counted set of real held requests, not a pool-attempt
    // count, and it is deliberately not compared against MAX_FAIL.
    let held_requests = gallery.paths().len();
    let released = gallery.released();

    let Some(status) = status else {
        panic!(
            "control: no natural exit within 150s, so the AutoExit path is unproven \
             (the child was killed and reaped, not observed to exit); elapsed_ms={} \
             held_requests={held_requests} {install_state} {fixture} {exit_facts}",
            elapsed.as_millis()
        );
    };
    // Fixed safe fields only: no child output, URL, header or credential.
    eprintln!(
        "held-gallery control: elapsed_ms={} held_requests={held_requests} \
         gallery_released={released} {exit_facts} {install_state} {fixture}",
        elapsed.as_millis()
    );
    assert!(
        !released,
        "control: this case must never release the held gallery"
    );
    assert_eq!(
        status.code(),
        Some(0),
        "control: the natural AutoExit must exit 0; {exit_facts}"
    );
    assert!(
        run.labels.contains(&"loop_finished"),
        "control: the natural AutoExit must finish the application task loop; {exit_facts}"
    );
    // The closed production pool error is the causal evidence: the task failed
    // because no proxy was available, not because a request never arrived.
    assert!(
        run.labels.contains(&"proxy_pool_exhausted"),
        "control: the natural exit was not preceded by the production \
         ProxyError::NoProxy failure this control requires; {exit_facts}"
    );
    assert!(
        held_requests >= 1,
        "control: the held gallery read no request head, so no request was \
         actually held; {install_state} {fixture} {exit_facts}"
    );
    assert_eq!(
        release_b_state(&install, files.as_slice()),
        "yes",
        "control: all four live scripts must be release B after the natural exit; \
         {install_state} {exit_facts}"
    );
    assert!(
        applied_record_matches(&install),
        "control: the applied record must still name this candidate after the natural exit"
    );
}

// MARK: - the bounded timeout-cleanup regression

/// The same bounded wait/cleanup helper the held-gallery control ends with,
/// exercised against the same real ordinary host on a deadline that expires
/// first: an interactive run with no URL and no answer keeps the prompt blocked
/// on the still-open stdin, so the child is genuinely alive at the deadline.
/// The regression requires `None` (the original observation is preserved), the
/// child to be reaped, both captured readers to be joined and taken, and no
/// natural exit to be claimed. If the host exits earlier the case fails as it
/// should; nothing here fakes a live child or a natural exit.
#[test]
#[ignore = "explicit bounded timeout-cleanup regression"]
fn native_timeout_cleanup_reaps_the_child_before_collecting() {
    let root = tempfile::tempdir().expect("case root");
    let origin = Arc::new(Server::start(Vec::new()));
    let (_exe, _binding_a) = installed_host(root.path(), &origin.origin());
    let install = root.path().join("install");

    // `--auto-update off` keeps startup network-free; the production prompt
    // prints its question and waits for a line this case never writes.
    let mut run = spawn(&install, &["-i", "-c", COOKIE, "--auto-update", "off"]);
    let started = Instant::now();
    let status = run.wait_exit_then_collect(Duration::from_millis(50));
    let elapsed = started.elapsed();

    assert!(
        status.is_none(),
        "the host left before the deadline, so this regression cannot observe the \
         timeout cleanup: {}",
        run.exit_facts()
    );
    assert!(
        matches!(run.child.try_wait(), Ok(Some(_))),
        "the timed-out child was not killed and reaped"
    );
    assert!(
        run.stdout.is_none() && run.stderr.is_none(),
        "the captured readers were not joined and taken before the case ended"
    );
    assert!(
        !run.exited && run.exit_code.is_none(),
        "a killed child must not be recorded as a natural exit: {}",
        run.exit_facts()
    );
    assert!(
        elapsed < Duration::from_secs(10),
        "the timeout cleanup was not bounded: elapsed_ms={}",
        elapsed.as_millis()
    );
    // Fixed fields only: no child output, URL, header or credential.
    eprintln!(
        "timeout cleanup: status=none reaped=yes readers_joined=yes elapsed_ms={} {}",
        elapsed.as_millis(),
        run.exit_facts()
    );
}

// MARK: - diagnostics controls

// M8: one real Unix end-to-end migration chain. It reuses this module's host,
// signing, source-binding, wait/reap and byte-compare helpers; POSIX signals
// make it Unix-only.
#[cfg(unix)]
mod migration_chain;

/// The apply boundary must treat a missing, unreadable or foreign record as
/// "not applied": folding any of those into success is exactly the
/// misattribution this boundary exists to prevent.
#[cfg(test)]
mod apply_boundary {
    use super::*;

    #[test]
    fn only_a_valid_v2_record_with_this_identity_counts_as_applied() {
        let root = tempfile::tempdir().expect("root");
        assert!(
            !applied_record_matches(root.path()),
            "a missing applied record must not count as a successful apply"
        );

        let area = root.path().join(AREA);
        std::fs::create_dir_all(&area).expect("area");
        let state = area.join("state.json");
        std::fs::write(&state, b"{not json").expect("malformed fixture");
        assert!(
            !applied_record_matches(root.path()),
            "an unparseable applied record must not count as a successful apply"
        );

        std::fs::write(
            &state,
            serde_json::json!({ "v": 1, "active": { "update_id": UPDATE_ID } }).to_string(),
        )
        .expect("v1 fixture");
        assert!(
            !applied_record_matches(root.path()),
            "a pre-v2 record must not count as this apply"
        );

        std::fs::write(
            &state,
            serde_json::json!({ "v": 2, "active": { "update_id": "another-candidate" } })
                .to_string(),
        )
        .expect("foreign fixture");
        assert!(
            !applied_record_matches(root.path()),
            "another candidate's record must not count as this apply"
        );

        std::fs::write(
            &state,
            serde_json::json!({ "v": 2, "active": { "update_id": UPDATE_ID } }).to_string(),
        )
        .expect("matching fixture");
        assert!(
            applied_record_matches(root.path()),
            "the exact v2 identity is the applied boundary"
        );
    }
}

/// Pure-data controls for the early-exit report: it must carry the code and the
/// recognised fixed labels, and must never carry captured bytes. These do not
/// prove the application behaviour; they only pin the diagnostic format.
#[cfg(test)]
mod early_exit_diagnostics {
    use super::*;

    #[test]
    fn report_names_the_code_and_labels_but_not_the_output_bytes() {
        let secret = "ipb_pass_hash=cafebabe";
        let stderr =
            format!("xeHentai: update startup stopped (ApplyFailed): {secret}\n").into_bytes();
        let labels = scan_labels(&stderr);
        assert_eq!(labels, vec!["startup_stopped"], "recognised labels");
        let report = exit_facts_text(true, Some(1), 0, stderr.len(), &labels);
        assert!(report.contains("code=1"), "{report}");
        assert!(report.contains("startup_stopped"), "{report}");
        assert!(
            report.contains(&format!("stderr_bytes={}", stderr.len())),
            "{report}"
        );
        assert!(!report.contains(secret), "{report}");
        assert!(!report.contains("cafebabe"), "{report}");
    }

    #[test]
    fn label_scan_matches_only_the_fixed_messages_this_server_emits() {
        assert!(
            scan_labels(b"INFO - [00:00:00] application task loop finished\n")
                .contains(&"loop_finished")
        );
        assert!(
            scan_labels(b"WARN - [00:00:00] Failure when updating program: boom\n")
                .contains(&"update_failed")
        );
        assert!(scan_labels(b"xeHentai: cannot read input: end of input\n").contains(&"prompt_io"));
        assert!(
            scan_labels(b"thread 'main' panicked at src/main.rs:1:1\n").contains(&"child_panic")
        );
        assert!(scan_labels(b"xeHentai: session error: bad\n").contains(&"session_error"));
        assert!(scan_labels(b"ordinary output\n").is_empty());
    }

    /// The additive setup diagnostic must return only one of five closed
    /// categories. This is a pure formatter control: it never claims that a
    /// platform run reached any updater error.
    #[test]
    fn updater_failure_categories_are_closed_labels_not_raw_text() {
        let secret = "ipb_pass_hash=cafebabe /owned/path?token=hidden";
        for (name, text) in UPDATE_FAILURE_LABELS {
            let captured = format!("WARN - [00:00:00] {text} boom {secret}\n");
            assert_eq!(
                scan_update_failure(captured.as_bytes()),
                name,
                "fixed producer text {text}"
            );
        }
        assert_eq!(
            scan_update_failure(format!("task #t1 error: {secret}").as_bytes()),
            "unknown",
            "an unrecognised failure must stay unknown"
        );
        for (name, _) in UPDATE_FAILURE_LABELS {
            assert!(
                !name
                    .chars()
                    .any(|character| matches!(character, ':' | '/' | ' ')),
                "a category must be a closed identifier: {name}"
            );
        }
    }

    /// The producer logs the pool failure as `task #<guid> error: <error>`, and
    /// `ProxyError::NoProxy`'s production Display text is the only closed label
    /// for it. This pins that scan contract and the report's containment; it is
    /// not evidence that the long control actually reached the pool failure.
    #[test]
    fn proxy_pool_exhaustion_is_recognised_only_as_a_closed_label() {
        let secret = "ipb_pass_hash=cafebabe";
        let message = crate::proxy::ProxyError::NoProxy.to_string();
        let stderr =
            format!("ERROR- [00:00:00] task #t1 error: {message}\n{secret}\n").into_bytes();
        let labels = scan_labels(&stderr);
        assert_eq!(labels, vec!["proxy_pool_exhausted"], "{labels:?}");
        let report = exit_facts_text(true, Some(0), 0, stderr.len(), &labels);
        assert!(report.contains("proxy_pool_exhausted"), "{report}");
        assert!(!report.contains("avaliable"), "raw text leaked: {report}");
        assert!(!report.contains(secret), "{report}");
    }

    #[test]
    fn unreadable_paths_are_reported_as_unknown_not_as_a_boolean() {
        let root = tempfile::tempdir().expect("root");
        let present = root.path().join("present");
        std::fs::write(&present, b"x").expect("file");
        assert_eq!(path_state(&present), "yes");
        assert_eq!(path_state(&root.path().join("missing")), "no");
    }
}
