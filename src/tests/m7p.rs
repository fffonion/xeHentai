//! M7 remaining scope: the signed native package, the atomic pending envelope
//! and next-start application (resource-only and native A/B).
//!
//! Every fixture here is a real loopback HTTP source and a real signed package:
//! the descriptor is signed with a synthetic test Ed25519 key that the copied
//! `rss/updater/github.rss` binding explicitly trusts. No production key, no
//! external network and no user configuration is involved.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use ed25519_dalek::{Signer, SigningKey};

use super::m6;
use super::m7::{COMMITS, Request, UPDATE_ID};

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");
const AREA: &str = ".xehentai-update";

// MARK: - fixture source

#[derive(Clone)]
struct Route {
    status: u16,
    body: Arc<Vec<u8>>,
    /// Send this many body bytes, then hold the rest until `release()`.
    hold_after: Option<usize>,
}

impl Route {
    fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            body: Arc::new(body),
            hold_after: None,
        }
    }

    fn status(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            body: Arc::new(body),
            hold_after: None,
        }
    }

    fn holding(body: Vec<u8>, after: usize) -> Self {
        Self {
            status: 200,
            body: Arc::new(body),
            hold_after: Some(after),
        }
    }
}

/// A real loopback HTTP source with per-target routes. It records every request
/// head, and for a holding route it records how many body bytes really went out
/// and whether the client closed first.
struct Server {
    port: u16,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    release: Arc<(Mutex<bool>, Condvar)>,
    partial: Arc<Mutex<HashMap<String, usize>>>,
    eof: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start(routes: Vec<(&str, Route)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes: HashMap<String, Route> = routes
            .into_iter()
            .map(|(path, route)| (path.to_string(), route))
            .collect();
        let routes = Arc::new(Mutex::new(routes));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let partial = Arc::new(Mutex::new(HashMap::new()));
        let eof = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&requests);
        let table = Arc::clone(&routes);
        let gate = Arc::clone(&release);
        let flushed = Arc::clone(&partial);
        let closed = Arc::clone(&eof);
        let stopping = Arc::clone(&stop);
        let handle = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
                        // One thread per connection: a stalled peer can never
                        // wedge the accept loop of the next case.
                        socket.set_nonblocking(false).unwrap();
                        socket
                            .set_read_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        socket
                            .set_write_timeout(Some(Duration::from_secs(10)))
                            .unwrap();
                        let table = Arc::clone(&table);
                        let seen = Arc::clone(&seen);
                        let gate = Arc::clone(&gate);
                        let flushed = Arc::clone(&flushed);
                        let closed = Arc::clone(&closed);
                        let stopping = Arc::clone(&stopping);
                        std::thread::spawn(move || {
                            serve(socket, &table, &seen, &gate, &flushed, &closed, &stopping);
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
            routes,
            requests,
            release,
            partial,
            eof,
            stop,
            handle: Some(handle),
        }
    }

    fn origin(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    fn set(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    fn partial(&self, path: &str) -> usize {
        *self.partial.lock().unwrap().get(path).unwrap_or(&0)
    }

    fn peer_eof(&self, path: &str) -> bool {
        self.eof.lock().unwrap().iter().any(|seen| seen == path)
    }

    fn release(&self) {
        let (lock, cond) = &*self.release;
        *lock.lock().unwrap() = true;
        cond.notify_all();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.release();
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    mut socket: TcpStream,
    routes: &Arc<Mutex<HashMap<String, Route>>>,
    seen: &Arc<Mutex<Vec<Request>>>,
    gate: &Arc<(Mutex<bool>, Condvar)>,
    flushed: &Arc<Mutex<HashMap<String, usize>>>,
    closed: &Arc<Mutex<Vec<String>>>,
    stopping: &Arc<AtomicBool>,
) {
    let Some((method, target, headers)) = read_head(&mut socket) else {
        return;
    };
    seen.lock().unwrap().push(Request {
        method,
        target: target.clone(),
        headers,
    });
    let route = routes.lock().unwrap().get(&target).cloned();
    let Some(route) = route else {
        let body = b"not found".to_vec();
        write_head(&mut socket, 404, &body);
        let _ = socket.write_all(&body);
        let _ = socket.shutdown(std::net::Shutdown::Write);
        return;
    };
    write_head(&mut socket, route.status, &route.body);
    match route.hold_after {
        None => {
            let _ = socket.write_all(&route.body);
            let _ = socket.flush();
            let _ = socket.shutdown(std::net::Shutdown::Write);
            let mut sink = [0u8; 128];
            let _ = socket.read(&mut sink);
        }
        Some(prefix) => {
            let prefix = prefix.min(route.body.len());
            if socket.write_all(&route.body[..prefix]).is_err() || socket.flush().is_err() {
                return;
            }
            flushed.lock().unwrap().insert(target.clone(), prefix);
            socket
                .set_read_timeout(Some(Duration::from_millis(20)))
                .unwrap();
            let (lock, cond) = &**gate;
            loop {
                if stopping.load(Ordering::SeqCst) {
                    return;
                }
                if *lock.lock().unwrap() {
                    let _ = socket.write_all(&route.body[prefix..]);
                    let _ = socket.flush();
                    let _ = socket.shutdown(std::net::Shutdown::Write);
                    return;
                }
                let mut chunk = [0u8; 256];
                match socket.read(&mut chunk) {
                    Ok(0) => {
                        closed.lock().unwrap().push(target.clone());
                        return;
                    }
                    Ok(_) => {}
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        ) => {}
                    Err(_) => return,
                }
                let (guard, _) = cond
                    .wait_timeout(lock.lock().unwrap(), Duration::from_millis(10))
                    .unwrap();
                drop(guard);
            }
        }
    }
}

fn read_head(socket: &mut TcpStream) -> Option<(String, String, Vec<(String, String)>)> {
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
    let head = String::from_utf8_lossy(&buffer).into_owned();
    let mut lines = head.lines();
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }
    Some((method, target, headers))
}

fn write_head(socket: &mut TcpStream, status: u16, body: &[u8]) {
    let reason = match status {
        200 => "OK",
        404 => "Not Found",
        500 => "Internal Server Error",
        503 => "Service Unavailable",
        _ => "Status",
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = socket.write_all(head.as_bytes());
    let _ = socket.flush();
}

// MARK: - package fixtures

/// Fixed synthetic test key (the matching public PEM is test data next to the
/// updater sources). No production or user key is ever involved.
const TEST_SEED_HEX: &str = "4efca9f65359d358c489465852634f818ee7b34ed5922e015e5b24b80fb78b3b";
const OTHER_SEED_HEX: &str = "1f8c8695f06c270ce3a1f0b2d3f4d22c2ac145d6850523e7e38862a186c31759";

fn key_from(seed_hex: &str) -> SigningKey {
    let mut seed = [0u8; 32];
    let bytes = seed_hex.as_bytes();
    for index in 0..32 {
        let pair = std::str::from_utf8(&bytes[index * 2..index * 2 + 2]).expect("hex");
        seed[index] = u8::from_str_radix(pair, 16).expect("hex");
    }
    SigningKey::from_bytes(&seed)
}

fn signing_key() -> SigningKey {
    key_from(TEST_SEED_HEX)
}

fn other_key() -> SigningKey {
    key_from(OTHER_SEED_HEX)
}

fn public_pem(_key: &SigningKey) -> String {
    include_str!("../updater/testdata/m7p-test-public.pem").to_string()
}

fn other_public_pem() -> String {
    include_str!("../updater/testdata/m7p-other-public.pem").to_string()
}

fn download_link(origin: &str, update_id: &str) -> String {
    format!("{origin}/fffonion/xeHentai/releases/download/rss-{update_id}/xeHentai-rss.zip")
}

fn target_of(link: &str) -> String {
    let without_scheme = link.split_once("://").map(|(_, rest)| rest).unwrap_or(link);
    match without_scheme.find('/') {
        Some(index) => without_scheme[index..].to_string(),
        None => "/".to_string(),
    }
}

/// The three package transport targets for one update id.
fn package_targets(origin: &str, update_id: &str) -> (String, String, String) {
    let payload = download_link(origin, update_id);
    (
        target_of(&payload),
        target_of(&format!("{payload}.manifest.json")),
        target_of(&format!("{payload}.sig")),
    )
}

const COMMITS_TARGET: &str = "/repos/fffonion/xeHentai/commits?sha=rss";
const COMMITS_TARGET_B: &str = "/repos/fffonion/xeHentai/commits?sha=rss-b";

#[derive(Clone)]
struct Spec {
    update_id: String,
    app_version: String,
    target: String,
    host_abi: i64,
    script_abi: i64,
    entries: Vec<(String, u64)>,
    members: Vec<(String, Vec<u8>)>,
    native: Option<(String, String)>,
    symlinks: Vec<String>,
}

impl Spec {
    fn new(update_id: &str) -> Self {
        Self {
            update_id: update_id.to_string(),
            app_version: crate::r#const::VERSION_DISPLAY.to_string(),
            target: crate::r#const::TARGET.to_string(),
            host_abi: crate::r#const::HOST_ABI,
            script_abi: crate::r#const::SCRIPT_ABI,
            entries: Vec::new(),
            members: Vec::new(),
            native: None,
            symlinks: Vec::new(),
        }
    }

    /// Add install-relative files under the RSS-selected archive prefix.
    fn with_tree(mut self, prefix: &str, files: &[(String, Vec<u8>)]) -> Self {
        for (path, body) in files {
            self.entries.push((path.clone(), body.len() as u64));
            self.members.push((format!("{prefix}{path}"), body.clone()));
        }
        self
    }

    fn native(mut self, path: &str, build_id: &str, body: Vec<u8>) -> Self {
        self.entries.push((path.to_string(), body.len() as u64));
        self.members
            .push((format!("xeHentai-{}/{}", self.update_id, path), body));
        self.native = Some((path.to_string(), build_id.to_string()));
        self
    }
}

struct Pkg {
    info: Vec<u8>,
    sig: Vec<u8>,
    payload: Vec<u8>,
}

fn sign(key: &SigningKey, spec: &Spec) -> Pkg {
    let payload = crate::updater::package::store_zip(&spec.members).expect("build payload archive");
    let payload = mark_symlinks(payload, &spec.symlinks);
    let digest = crate::updater::descriptor::sha256_hex(&payload);
    let entries: Vec<serde_json::Value> = spec
        .entries
        .iter()
        .map(|(path, len)| serde_json::json!({"path": path, "len": len}))
        .collect();
    let info = serde_json::to_vec(&serde_json::json!({
        "v": 1,
        "update_id": spec.update_id,
        "app_version": spec.app_version,
        "target": spec.target,
        "host_abi": spec.host_abi,
        "script_abi": spec.script_abi,
        "archive_sha256": digest,
        "entries": entries,
        "native_executable": spec.native.as_ref().map(|(path, _)| path.clone()),
        "native_build_id": spec.native.as_ref().map(|(_, id)| id.clone()),
    }))
    .expect("descriptor json");
    let sig = key.sign(&info).to_bytes().to_vec();
    Pkg { info, sig, payload }
}

/// Rewrite one central-directory entry's external attributes so the member is
/// read back as a Unix symlink. The ZIP writer masks permission bits, so a
/// hostile archive has to be produced this way; it is test data only.
fn mark_symlinks(mut zip: Vec<u8>, members: &[String]) -> Vec<u8> {
    for member in members {
        let mut index = 0usize;
        while let Some(found) = find(&zip[index..], b"PK\x01\x02") {
            let start = index + found;
            let name_len = u16::from_le_bytes([zip[start + 28], zip[start + 29]]) as usize;
            let name = &zip[start + 46..start + 46 + name_len];
            if name == member.as_bytes() {
                let attributes: u32 = 0o120777 << 16;
                zip[start + 38..start + 42].copy_from_slice(&attributes.to_le_bytes());
                break;
            }
            index = start + 4;
        }
    }
    zip
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn collect_files(dir: &Path, relative: &Path, out: &mut Vec<(String, Vec<u8>)>) {
    let mut paths: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("tree dir")
        .map(|entry| entry.expect("entry").path())
        .collect();
    paths.sort();
    for path in paths {
        let name = relative.join(path.file_name().expect("name"));
        if path.is_dir() {
            collect_files(&path, &name, out);
        } else {
            let body = std::fs::read(&path).expect("tree file");
            out.push((name.to_string_lossy().replace('\\', "/"), body));
        }
    }
}

fn tree(root: &Path, sub: &str) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    collect_files(&root.join(sub), Path::new(sub), &mut out);
    out
}

// MARK: - installation fixtures

fn patch_binding(path: &Path, origin: &str, pem: Option<&str>, channel: &str) {
    let source = std::fs::read_to_string(path).expect("github.rss");
    let mut source = source
        .replace(
            "api_origin: \"https://api.github.com\",",
            &format!("api_origin: \"{origin}\","),
        )
        .replace(
            "package_download_origin: \"https://github.com\",",
            &format!("package_download_origin: \"{origin}\","),
        )
        .replace(
            "stable_channel: \"rss\",",
            &format!("stable_channel: \"{channel}\","),
        );
    if let Some(pem) = pem {
        let pem_literal = pem.replace('\n', "\\n");
        source = source.replace(
            "verification_key: null,",
            &format!("verification_key: \"{pem_literal}\","),
        );
    }
    std::fs::write(path, source).expect("write binding");
}

/// Publish the production resource tree, then bind ONLY the trusted source
/// constants to the owned loopback fixture and its synthetic test key.
fn publish_rss(root: &Path, origin: &str, pem: Option<&str>, channel: &str) {
    let rss = root.join("rss");
    if !rss.is_dir() {
        m6::copy_tree(&Path::new(MANIFEST).join("rss"), &rss);
    }
    patch_binding(&rss.join("updater/github.rss"), origin, pem, channel);
}

/// A complete candidate resource tree: all four RSS modules, bound to the
/// fixture source with the given channel.
fn candidate_rss(origin: &str, pem: Option<&str>, channel: &str) -> Vec<(String, Vec<u8>)> {
    let mut files = tree(&Path::new(MANIFEST), "rss");
    let scratch = tempfile::tempdir().expect("scratch");
    let rss = scratch.path().join("rss");
    m6::copy_tree(&Path::new(MANIFEST).join("rss"), &rss);
    patch_binding(&rss.join("updater/github.rss"), origin, pem, channel);
    let binding = std::fs::read(rss.join("updater/github.rss")).expect("patched binding");
    for (path, body) in files.iter_mut() {
        if path == "rss/updater/github.rss" {
            *body = binding.clone();
        }
    }
    files
}

fn pending_path(install: &Path) -> PathBuf {
    install.join(AREA).join("pending").join("src.zip")
}

fn seed_pending(install: &Path, info: Vec<u8>, sig: Vec<u8>, payload: Vec<u8>) -> Vec<u8> {
    let dir = pending_path(install)
        .parent()
        .map(Path::to_path_buf)
        .expect("pending dir");
    let envelope =
        crate::updater::package::PendingEnvelope::new(info, sig, payload).expect("envelope");
    crate::updater::package::publish(&dir, &envelope).expect("seed pending");
    std::fs::read(pending_path(install)).expect("pending bytes")
}

struct Member {
    name: String,
    stored: bool,
    body: Vec<u8>,
}

fn pending_members(install: &Path) -> Vec<Member> {
    let bytes = std::fs::read(pending_path(install)).expect("pending");
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(bytes)).expect("pending zip");
    let mut out = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).expect("member");
        let name = file.name().to_string();
        let stored = file.compression() == zip::CompressionMethod::Stored;
        let mut body = Vec::new();
        file.read_to_end(&mut body).expect("member body");
        out.push(Member { name, stored, body });
    }
    out
}

fn run_local<F, T>(future: F) -> T
where
    F: std::future::Future<Output = T>,
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime");
    let local = tokio::task::LocalSet::new();
    runtime.block_on(local.run_until(future))
}

fn check_download(install: &Path, mode: &str) -> crate::updater::CheckReport {
    let mode = mode.to_string();
    let install = install.to_path_buf();
    run_local(async move {
        let mut updater = crate::updater::Updater::load(&install).expect("load updater");
        updater
            .check(&mode, false, None, "xeHentai/test")
            .await
            .expect("download check")
    })
}

fn event_keys(report: &crate::updater::CheckReport) -> Vec<String> {
    report
        .events
        .iter()
        .map(|event| event.key.clone())
        .collect()
}

fn spawn_in(install: &Path, exe: &Path, args: &[&str]) -> Child {
    std::fs::create_dir_all(install.join("home")).unwrap();
    std::fs::create_dir_all(install.join("tmp")).unwrap();
    std::fs::create_dir_all(install.join("cache")).unwrap();
    let mut command = std::process::Command::new(exe);
    command
        .current_dir(install)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", install.join("home"))
        .env("TMPDIR", install.join("tmp"))
        .env("XDG_CACHE_HOME", install.join("cache"))
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command.spawn().expect("spawn the production binary")
}

/// One owned production child. Dropping it kills and reaps the process, so a
/// panicking test never leaves an installation running behind it.
struct ChildRun {
    child: Option<Child>,
}

impl ChildRun {
    fn spawn(install: &Path, exe: &Path, args: &[&str]) -> Self {
        Self {
            child: Some(spawn_in(install, exe, args)),
        }
    }

    /// True while the production process is still running (a normal,
    /// resident App rather than an exited/rejected start).
    fn running(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    /// First SIGINT: the production graceful stop, which must retire the whole
    /// run (including an already re-executed image) and return 0.
    fn stop(mut self) -> i32 {
        let mut child = self.child.take().expect("child");
        let code = stop_child(&mut child);
        let _ = child.wait();
        code
    }
}

impl Drop for ChildRun {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn stop_child(child: &mut Child) -> i32 {
    // Activation and logging happen before the supervised loop installs its
    // signal handlers; let the child reach that point before signalling.
    std::thread::sleep(Duration::from_millis(300));
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(child.id() as i32),
        nix::sys::signal::Signal::SIGINT,
    );
    m6::wait_exit(child, Duration::from_secs(30))
        .expect("the update owner must retire on the first signal")
        .code()
        .unwrap_or(-1)
}

fn log_of(install: &Path) -> String {
    std::fs::read_to_string(install.join("eh.log")).unwrap_or_default()
}

fn wait_log(install: &Path, needle: &str) -> bool {
    m6::wait_for(Duration::from_secs(30), || log_of(install).contains(needle))
}

fn serve_package(server: &Server, origin: &str, pkg: &Pkg) {
    let (payload, descriptor, signature) = package_targets(origin, UPDATE_ID);
    server.set(COMMITS_TARGET, Route::ok(COMMITS.as_bytes().to_vec()));
    server.set(&descriptor, Route::ok(pkg.info.clone()));
    server.set(&signature, Route::ok(pkg.sig.clone()));
    server.set(&payload, Route::ok(pkg.payload.clone()));
}

// MARK: - download / pending

#[test]
fn m7p_download_publishes_one_signed_pending_envelope_and_keeps_the_live_tree() {
    let key = signing_key();
    let pem = public_pem(&key);
    let install = tempfile::tempdir().expect("install");
    let files = vec![(
        "rss/updater/updater.rss".to_string(),
        b"// candidate resource\n".to_vec(),
    )];
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(&format!("xeHentai-{UPDATE_ID}/"), &files),
    );

    let server = Server::start(vec![]);
    let origin = server.origin();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let live_before =
        std::fs::read(install.path().join("rss/updater/updater.rss")).expect("live before");
    serve_package(&server, &origin, &pkg);

    let report = check_download(install.path(), "download");
    assert_eq!(
        report.outcome,
        "pending",
        "events: {:?}",
        event_keys(&report)
    );
    assert_eq!(
        event_keys(&report),
        vec!["UPDATE_AVAILABLE", "UPDATE_COMPLETE", "UPDATE_PENDING_ID"]
    );

    let members = pending_members(install.path());
    let names: Vec<&str> = members.iter().map(|member| member.name.as_str()).collect();
    assert_eq!(names, vec!["info.json", "info.sig", "payload.zip"]);
    assert!(
        members.iter().all(|member| member.stored),
        "an envelope without a codec"
    );
    assert_eq!(
        members[0].body, pkg.info,
        "the signed descriptor bytes must be preserved exactly"
    );
    assert_eq!(members[1].body, pkg.sig, "the detached signature");
    assert_eq!(
        members[2].body, pkg.payload,
        "the original payload archive bytes"
    );

    // The live installation is untouched: this build keeps running A.
    assert_eq!(
        std::fs::read(install.path().join("rss/updater/updater.rss")).expect("live after"),
        live_before
    );
    assert!(
        !install.path().join(AREA).join("state.json").exists(),
        "a download must not publish an applied identity"
    );

    let requests = server.requests();
    let (payload_target, descriptor_target, signature_target) =
        package_targets(&server.origin(), UPDATE_ID);
    let targets: Vec<&str> = requests
        .iter()
        .map(|request| request.target.as_str())
        .collect();
    assert_eq!(
        targets,
        vec![
            COMMITS_TARGET,
            descriptor_target.as_str(),
            signature_target.as_str(),
            payload_target.as_str()
        ]
    );
    for request in &requests {
        assert_eq!(request.method, "GET");
        assert_eq!(request.header("cookie"), None);
        assert_eq!(request.header("authorization"), None);
    }
}

#[test]
fn m7p_download_same_update_id_skips_the_package() {
    let key = signing_key();
    let pem = public_pem(&key);
    let install = tempfile::tempdir().expect("install");
    let server = Server::start(vec![]);
    let origin = server.origin();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &[(
                "rss/updater/updater.rss".to_string(),
                b"// candidate\n".to_vec(),
            )],
        ),
    );
    serve_package(&server, &origin, &pkg);

    let install_path = install.path().to_path_buf();
    run_local(async move {
        let mut updater = crate::updater::Updater::load(&install_path).expect("load");
        let report = updater
            .check("download", false, Some(UPDATE_ID), "xeHentai/test")
            .await
            .expect("check");
        assert_eq!(report.outcome, "up_to_date");
        assert_eq!(event_keys(&report), vec!["UPDATE_NO_UPDATE"]);
    });
    assert_eq!(
        server.requests().len(),
        1,
        "the same update id must skip the package"
    );
    assert!(!pending_path(install.path()).exists());
}

#[test]
fn m7p_package_rejections_keep_the_previous_pending_intact() {
    let key = signing_key();
    let pem = public_pem(&key);
    let other_pem = other_public_pem();

    // The previously valid pending that every rejection must leave untouched.
    let seeded = sign(
        &key,
        &Spec::new("1111111111111111111111111111111111111111").with_tree(
            "xeHentai-1111111111111111111111111111111111111111/",
            &[(
                "rss/updater/updater.rss".to_string(),
                b"// previous pending\n".to_vec(),
            )],
        ),
    );
    let good = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &[(
                "rss/updater/updater.rss".to_string(),
                b"// next candidate\n".to_vec(),
            )],
        ),
    );
    let prefix = format!("xeHentai-{UPDATE_ID}/");

    let mut cases: Vec<(
        &str,
        Option<String>,
        Box<dyn Fn(&Spec, &Pkg, &Server, &str) -> Spec>,
    )> = Vec::new();

    // Wrong verification key: the package is valid but this binding trusts a
    // different public key.
    cases.push((
        "wrong binding key",
        Some(other_pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| spec.clone()),
    ));
    // Signed by a key the binding does not trust.
    cases.push((
        "signed by another key",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| spec.clone()),
    ));
    // No verification key at all: production stays unbound and must reject.
    cases.push((
        "unbound key",
        None,
        Box::new(|spec, _pkg, _server, _origin| spec.clone()),
    ));
    // Target / ABI / version contract mismatches.
    cases.push((
        "wrong target",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.target = "x86_64-pc-windows-gnu".into();
            spec
        }),
    ));
    cases.push((
        "wrong host abi",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.host_abi = 2;
            spec
        }),
    ));
    cases.push((
        "wrong script abi",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.script_abi = 7;
            spec
        }),
    ));
    cases.push((
        "wrong app version",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.app_version = "9.999-rss".into();
            spec
        }),
    ));
    // Entry list / path attacks.
    cases.push((
        "missing selected entry",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.entries.push(("rss/missing.rss".into(), 4));
            spec
        }),
    ));
    cases.push((
        "traversal entry",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.entries.push(("../evil.sh".into(), 3));
            spec
        }),
    ));
    cases.push((
        "absolute entry",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.entries.push(("/etc/passwd".into(), 3));
            spec
        }),
    ));
    cases.push((
        "backslash entry",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.entries.push(("rss\\\\evil.rss".into(), 3));
            spec
        }),
    ));
    cases.push((
        "duplicate entry",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            let first = spec.entries[0].clone();
            spec.entries.push(first);
            spec
        }),
    ));
    cases.push((
        "reserved user state entry",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.entries.push(("h.json".into(), 3));
            spec.entries.push(("config.rss".into(), 3));
            spec
        }),
    ));
    cases.push((
        "declared length longer than the member",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.entries[0].1 += 32;
            spec
        }),
    ));
    // Symlink member inside the selected prefix.
    cases.push((
        "symlink member",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| {
            let mut spec = spec.clone();
            spec.symlinks = vec![format!("{prefix}rss/updater/updater.rss")];
            spec
        }),
    ));
    // Same JSON content, different bytes: the signature must reject it.
    cases.push((
        "re-serialized descriptor",
        Some(pem.clone()),
        Box::new(|spec, _pkg, _server, _origin| spec.clone()),
    ));

    for (name, case_pem, shape) in cases {
        let install = tempfile::tempdir().expect("install");
        let server = Server::start(vec![]);
        let origin = server.origin();
        publish_rss(install.path(), &origin, case_pem.as_deref(), "rss");
        let before = seed_pending(
            install.path(),
            seeded.info.clone(),
            seeded.sig.clone(),
            seeded.payload.clone(),
        );

        let spec = shape(
            &Spec::new(UPDATE_ID).with_tree(
                &prefix,
                &[(
                    "rss/updater/updater.rss".to_string(),
                    b"// next candidate\n".to_vec(),
                )],
            ),
            &good,
            &server,
            &origin,
        );
        let signing = if name == "signed by another key" {
            &other_key()
        } else {
            &key
        };
        let pkg = sign(signing, &spec);
        serve_package(&server, &origin, &pkg);
        if name == "re-serialized descriptor" {
            let (_, descriptor, _) = package_targets(&origin, UPDATE_ID);
            let value: serde_json::Value =
                serde_json::from_slice(&pkg.info).expect("descriptor json");
            let pretty = serde_json::to_vec_pretty(&value).expect("pretty");
            server.set(&descriptor, Route::ok(pretty));
        }

        let report = check_download(install.path(), "download");
        assert_eq!(
            report.outcome,
            "failed",
            "case {name}: {:?}",
            event_keys(&report)
        );
        assert_eq!(
            event_keys(&report),
            vec!["UPDATE_AVAILABLE", "UPDATE_FAILED"],
            "case {name} must log a failure, not a completion"
        );
        assert_eq!(
            std::fs::read(pending_path(install.path())).expect("pending"),
            before,
            "case {name} must leave the previous pending byte-identical"
        );
        assert!(
            !install.path().join(AREA).join("state.json").exists(),
            "case {name} must not publish an applied identity"
        );
    }
}

#[test]
fn m7p_transport_failures_and_damaged_archives_are_rejected() {
    let key = signing_key();
    let pem = public_pem(&key);
    let prefix = format!("xeHentai-{UPDATE_ID}/");
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &prefix,
            &[(
                "rss/updater/updater.rss".to_string(),
                b"// next candidate\n".to_vec(),
            )],
        ),
    );
    let mut damaged = pkg.payload.clone();
    damaged.truncate(damaged.len() / 2);

    let cases: Vec<(&str, Box<dyn Fn(&Server, &str)>)> = vec![
        (
            "descriptor 500",
            Box::new(|server, origin| {
                let (_, descriptor, _) = package_targets(origin, UPDATE_ID);
                server.set(&descriptor, Route::status(500, b"boom".to_vec()));
            }),
        ),
        (
            "signature 503",
            Box::new(|server, origin| {
                let (_, _, signature) = package_targets(origin, UPDATE_ID);
                server.set(&signature, Route::status(503, b"busy".to_vec()));
            }),
        ),
        (
            "payload 500",
            Box::new(|server, origin| {
                let (payload, _, _) = package_targets(origin, UPDATE_ID);
                server.set(&payload, Route::status(500, b"boom".to_vec()));
            }),
        ),
        (
            "payload is not a zip",
            Box::new(|server, origin| {
                let (payload, _, _) = package_targets(origin, UPDATE_ID);
                server.set(&payload, Route::ok(b"this is not a zip".to_vec()));
            }),
        ),
        (
            "truncated payload",
            Box::new(|server, origin| {
                let (payload, _, _) = package_targets(origin, UPDATE_ID);
                server.set(&payload, Route::ok(damaged.clone()));
            }),
        ),
        (
            "payload differs from the signed digest",
            Box::new(|server, origin| {
                let (payload, _, _) = package_targets(origin, UPDATE_ID);
                let mut other = pkg.payload.clone();
                other.extend_from_slice(b"tampered");
                server.set(&payload, Route::ok(other));
            }),
        ),
    ];

    for (name, tamper) in cases {
        let install = tempfile::tempdir().expect("install");
        let server = Server::start(vec![]);
        let origin = server.origin();
        publish_rss(install.path(), &origin, Some(&pem), "rss");
        let before = seed_pending(
            install.path(),
            pkg.info.clone(),
            pkg.sig.clone(),
            pkg.payload.clone(),
        );
        serve_package(&server, &origin, &pkg);
        tamper(&server, &origin);

        let report = check_download(install.path(), "download");
        assert_eq!(report.outcome, "failed", "case {name}");
        assert_eq!(
            event_keys(&report),
            vec!["UPDATE_AVAILABLE", "UPDATE_FAILED"],
            "case {name}"
        );
        assert_eq!(
            std::fs::read(pending_path(install.path())).expect("pending"),
            before,
            "case {name} must keep the previous pending"
        );
    }
}

#[test]
fn m7p_cancelled_download_keeps_the_old_pending_and_a_fresh_run_succeeds() {
    let key = signing_key();
    let pem = public_pem(&key);
    let install = tempfile::tempdir().expect("install");
    let server = Server::start(vec![]);
    let origin = server.origin();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let prefix = format!("xeHentai-{UPDATE_ID}/");
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &prefix,
            &[(
                "rss/updater/updater.rss".to_string(),
                b"// next candidate\n".to_vec(),
            )],
        ),
    );
    let seeded = sign(
        &key,
        &Spec::new("3333333333333333333333333333333333333333").with_tree(
            "xeHentai-3333333333333333333333333333333333333333/",
            &[(
                "rss/updater/updater.rss".to_string(),
                b"// previous pending\n".to_vec(),
            )],
        ),
    );
    let before = seed_pending(
        install.path(),
        seeded.info.clone(),
        seeded.sig.clone(),
        seeded.payload.clone(),
    );
    serve_package(&server, &origin, &pkg);
    let (payload_target, _, _) = package_targets(&origin, UPDATE_ID);
    // Real cancellation point: the response body is only half delivered.
    server.set(&payload_target, Route::holding(pkg.payload.clone(), 64));
    let payload_len = pkg.payload.len();
    let install_path = install.path().to_path_buf();

    run_local(async {
        let mut updater = crate::updater::Updater::load(&install_path).expect("load");
        let mut future = Box::pin(updater.check("download", false, None, "xeHentai/test"));
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            tokio::select! {
                result = &mut future => panic!(
                    "the download finished instead of holding: {:?}",
                    result.map(|report| report.outcome)
                ),
                _ = tokio::time::sleep(Duration::from_millis(20)) => {
                    if server.partial(&payload_target) > 0 {
                        break;
                    }
                    assert!(std::time::Instant::now() < deadline, "the payload body never started");
                }
            }
        }
        assert!(
            server.partial(&payload_target) < payload_len,
            "the body was not still incomplete at the cancellation point"
        );
        // Dropping the in-flight future is the real cancellation.
        drop(future);
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    assert!(
        m6::wait_for(Duration::from_secs(10), || server.peer_eof(&payload_target)),
        "the cancelled request kept its connection open"
    );
    assert_eq!(
        std::fs::read(pending_path(install.path())).expect("pending"),
        before,
        "cancellation must not publish a partial pending"
    );
    assert!(!install.path().join(AREA).join("state.json").exists());

    // A later fresh run against the complete body publishes the candidate.
    server.set(&payload_target, Route::ok(pkg.payload.clone()));
    let report = check_download(install.path(), "download");
    assert_eq!(report.outcome, "pending", "a fresh run must succeed");
    let members = pending_members(install.path());
    assert_eq!(members[2].body, pkg.payload);
    assert_ne!(
        std::fs::read(pending_path(install.path())).expect("pending"),
        before
    );
}

#[test]
fn m7p_publish_failure_keeps_the_old_pending() {
    let key = signing_key();
    let pem = public_pem(&key);
    let install = tempfile::tempdir().expect("install");
    let server = Server::start(vec![]);
    let origin = server.origin();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &[(
                "rss/updater/updater.rss".to_string(),
                b"// next candidate\n".to_vec(),
            )],
        ),
    );
    let before = seed_pending(
        install.path(),
        pkg.info.clone(),
        pkg.sig.clone(),
        pkg.payload.clone(),
    );
    serve_package(&server, &origin, &pkg);

    // A directory that cannot be written is a real publication failure.
    let pending_dir = pending_path(install.path()).parent().unwrap().to_path_buf();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&pending_dir, std::fs::Permissions::from_mode(0o500)).unwrap();
    }
    let report = check_download(install.path(), "download");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&pending_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert_eq!(report.outcome, "failed");
    assert_eq!(
        event_keys(&report),
        vec!["UPDATE_AVAILABLE", "UPDATE_FAILED"]
    );
    assert_eq!(
        std::fs::read(pending_path(install.path())).expect("pending"),
        before
    );

    // Recovery: once the area is writable again the same package publishes.
    let report = check_download(install.path(), "download");
    assert_eq!(report.outcome, "pending");
}

// MARK: - next start: resource-only upgrade

fn install_dir() -> tempfile::TempDir {
    tempfile::tempdir().expect("install")
}

fn target_dir() -> String {
    std::env::var("CARGO_TARGET_DIR")
        .expect("run the update tests with the assigned CARGO_TARGET_DIR")
}

fn cargo_build(build_id: Option<&str>) {
    let mut command = std::process::Command::new(env!("CARGO"));
    command
        .args(["build", "--jobs", "2", "--target-dir"])
        .arg(target_dir())
        .current_dir(MANIFEST)
        .env("CARGO_TARGET_DIR", target_dir());
    match build_id {
        Some(id) => {
            command.env("XEH_BUILD_ID", id);
        }
        None => {
            command.env_remove("XEH_BUILD_ID");
        }
    }
    let output = command.output().expect("nested cargo build");
    assert!(
        output.status.success(),
        "nested build failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Each installation owns a copy of the ordinary (unidentified) build, so a
/// resource-only case never depends on whatever a previous native build left
/// in Cargo's output directory.
fn owned_exe(install: &Path) -> PathBuf {
    cargo_build(None);
    let exe = install.join("xeHentai");
    std::fs::copy(
        Path::new(&target_dir()).join("debug").join("xeHentai"),
        &exe,
    )
    .expect("copy the default build");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    exe
}

/// Restore the ordinary build when a test that built identified artifacts
/// ends, however it ends.
struct RestoreDefaultBuild;

impl Drop for RestoreDefaultBuild {
    fn drop(&mut self) {
        cargo_build(None);
    }
}

#[test]
fn m7p_next_start_applies_the_resource_candidate_and_runs_the_new_rss() {
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let candidate = candidate_rss(&origin, Some(&pem), "rss-b");
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(&format!("xeHentai-{UPDATE_ID}/"), &candidate),
    );
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    serve_package(&server, &origin, &pkg);
    let exe = owned_exe(install.path());

    // Run 1: the real download path publishes pending while A keeps running.
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "download"],
    );
    assert!(
        wait_log(install.path(), "Update is complete"),
        "no completion log: {}",
        log_of(install.path())
    );
    let log = log_of(install.path());
    assert!(log.contains("xeHentai 2.023-rss started."), "{log}");
    assert!(
        !log.contains("files installed"),
        "a download must not apply: {log}"
    );
    assert_eq!(child.stop(), 0);
    assert!(
        pending_path(install.path()).exists(),
        "pending was published"
    );
    let session = std::fs::read(install.path().join("h.json")).expect("session h.json");
    let live_before =
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live binding");
    assert_eq!(
        server.requests().len(),
        4,
        "one commits + descriptor + signature + payload"
    );

    // Run 2: the next start applies the candidate through the OLD RSS
    // controller, then the NEW business module drives the real request.
    server.set(COMMITS_TARGET_B, Route::ok(COMMITS.as_bytes().to_vec()));
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "check"],
    );
    assert!(
        wait_log(install.path(), "files installed"),
        "the next start never applied the candidate: {}",
        log_of(install.path())
    );
    assert!(
        m6::wait_for(Duration::from_secs(30), || {
            server
                .requests()
                .iter()
                .any(|request| request.target == COMMITS_TARGET_B)
        }),
        "the new RSS never ran: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);
    let requests = server.requests();
    let targets: Vec<&str> = requests
        .iter()
        .map(|request| request.target.as_str())
        .collect();
    let (payload_target, descriptor_target, signature_target) =
        package_targets(&server.origin(), UPDATE_ID);
    assert_eq!(
        targets,
        vec![
            COMMITS_TARGET,
            descriptor_target.as_str(),
            signature_target.as_str(),
            payload_target.as_str(),
            COMMITS_TARGET_B
        ],
        "unexpected request sequence"
    );

    // The applied tree is the candidate tree and the user state is untouched.
    let live_after =
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live binding");
    assert_ne!(live_after, live_before);
    let expected = candidate
        .iter()
        .find(|(path, _)| path == "rss/updater/github.rss")
        .map(|(_, body)| body.clone())
        .expect("candidate binding");
    assert_eq!(live_after, expected, "the real RSS module was replaced");
    assert_eq!(
        std::fs::read(install.path().join("h.json")).expect("session h.json"),
        session,
        "the previous session must be readable and unchanged"
    );
    assert!(!pending_path(install.path()).exists());
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(install.path().join(AREA).join("state.json")).expect("state"),
    )
    .expect("state json");
    assert_eq!(state["active"]["update_id"], UPDATE_ID);
    assert_eq!(state["active"]["native"], false);
    assert!(log_of(install.path()).contains("2.023-rss started."));
}

/// A successful next-start upgrade changes the live tree and its applied id
/// and leaves NO rollback machinery behind: no previous-release backup, no
/// activation/journal record and no previous-identity field. The old design
/// wrote those artifacts for every applied candidate, so this fails on it for
/// a real reachable behavior, not a missing symbol.
#[test]
fn m7p_successful_upgrade_leaves_no_rollback_artifacts() {
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &candidate_rss(&origin, Some(&pem), "rss-b"),
        ),
    );
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    serve_package(&server, &origin, &pkg);
    let exe = owned_exe(install.path());
    let before = std::fs::read(install.path().join("rss/updater/github.rss")).expect("binding");

    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "download"],
    );
    assert!(wait_log(install.path(), "Update is complete"));
    assert_eq!(child.stop(), 0);
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(install.path(), "files installed"),
        "the candidate was not applied: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);

    let after = std::fs::read(install.path().join("rss/updater/github.rss")).expect("binding");
    assert_ne!(before, after, "the live resource tree really changed");
    assert!(
        !pending_path(install.path()).exists(),
        "the pending candidate was consumed"
    );
    let area = area_path(install.path());
    assert!(
        !area.join("backup").exists(),
        "a previous-release backup was created by the upgrade"
    );
    assert!(
        !area.join("activation.json").exists(),
        "an activation/journal record was created by the upgrade"
    );
    let state: serde_json::Value =
        serde_json::from_slice(&std::fs::read(area.join("state.json")).expect("state"))
            .expect("state json");
    assert_eq!(state["active"]["update_id"], UPDATE_ID);
    assert!(
        state.get("previous").is_none(),
        "the applied state kept previous-release history: {state}"
    );
}

#[test]
fn m7p_next_start_rejects_an_uncompilable_candidate_and_keeps_the_live_tree() {
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let mut broken = candidate_rss(&origin, Some(&pem), "rss-b");
    for (path, body) in broken.iter_mut() {
        if path == "rss/updater/github.rss" {
            *body = b"fn broken( {".to_vec();
        }
    }
    let bad = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(&format!("xeHentai-{UPDATE_ID}/"), &broken),
    );
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let exe = owned_exe(install.path());
    let live_before =
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live binding");
    // A real session file, written by a normal start of this installation.
    std::fs::write(install.path().join("eh.log"), "").expect("reset log");
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(install.path(), "2.023-rss started."),
        "the plain start did not happen: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);
    let session = std::fs::read(install.path().join("h.json")).expect("session h.json");

    serve_package(&server, &origin, &bad);
    let report = check_download(install.path(), "download");
    assert_eq!(report.outcome, "pending");

    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(install.path(), "Failure when updating program"),
        "the bad candidate was not reported: {}",
        log_of(install.path())
    );
    assert!(
        wait_log(install.path(), "2.023-rss started."),
        "the app did not keep starting after a rejected candidate: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);
    let log = log_of(install.path());
    assert!(!log.contains("files installed"), "{log}");
    assert!(!log.contains("Update is complete"), "{log}");
    assert_eq!(
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live"),
        live_before,
        "the live tree must be untouched"
    );
    assert_eq!(
        std::fs::read(install.path().join("h.json")).expect("session"),
        session
    );
    assert!(
        !pending_path(install.path()).exists(),
        "the bad candidate is retired"
    );
    assert!(!install.path().join(AREA).join("state.json").exists());

    // A later valid candidate still applies on a following start.
    let good = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &candidate_rss(&origin, Some(&pem), "rss-b"),
        ),
    );
    serve_package(&server, &origin, &good);
    assert_eq!(
        check_download(install.path(), "download").outcome,
        "pending"
    );
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "check"],
    );
    assert!(
        wait_log(install.path(), "files installed"),
        "the fresh candidate did not apply: {}",
        log_of(install.path())
    );
    assert!(m6::wait_for(Duration::from_secs(30), || {
        server
            .requests()
            .iter()
            .any(|request| request.target == COMMITS_TARGET_B)
    }));
    assert_eq!(child.stop(), 0);
    assert_eq!(
        std::fs::read(install.path().join("h.json")).expect("session"),
        session
    );
}

#[test]
fn m7p_next_start_discards_a_tampered_pending_and_keeps_the_running_release() {
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &candidate_rss(&origin, Some(&pem), "rss-b"),
        ),
    );
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let exe = owned_exe(install.path());
    let live_before =
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live binding");
    // The signed descriptor and signature are valid; only the payload bytes
    // differ, so the startup digest check is the rejecting step.
    let mut tampered = pkg.payload.clone();
    let last = tampered.len() - 1;
    tampered[last] ^= 0xff;
    seed_pending(install.path(), pkg.info.clone(), pkg.sig.clone(), tampered);

    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(install.path(), "Failure when updating program"),
        "the tampered candidate was not reported: {}",
        log_of(install.path())
    );
    assert!(
        wait_log(install.path(), "2.023-rss started."),
        "the app did not keep starting after a rejected candidate: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);
    let log = log_of(install.path());
    assert!(!log.contains("files installed"), "{log}");
    assert_eq!(
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live"),
        live_before,
        "the tampered candidate changed the live tree"
    );
    assert!(
        !pending_path(install.path()).exists(),
        "the refused candidate was not retired"
    );
    assert!(
        !area_path(install.path()).join("state.json").exists(),
        "a refused candidate published an applied identity"
    );
}

#[test]
fn m7p_next_start_drops_a_pending_candidate_that_is_already_applied() {
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &candidate_rss(&origin, Some(&pem), "rss-b"),
        ),
    );
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    serve_package(&server, &origin, &pkg);
    let exe = owned_exe(install.path());

    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "download"],
    );
    assert!(wait_log(install.path(), "Update is complete"));
    assert_eq!(child.stop(), 0);
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(wait_log(install.path(), "files installed"));
    assert_eq!(child.stop(), 0);
    let applied = std::fs::read(install.path().join("rss/updater/github.rss")).expect("binding");

    // The same envelope reappears (a second download of the same id).
    seed_pending(
        install.path(),
        pkg.info.clone(),
        pkg.sig.clone(),
        pkg.payload.clone(),
    );
    let after_apply = server.requests().len();
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(install.path(), "2.023-rss started."),
        "the app did not keep starting after a rejected candidate: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);
    assert_eq!(
        server.requests().len(),
        after_apply,
        "off must not request anything"
    );
    assert!(
        !pending_path(install.path()).exists(),
        "an already applied candidate is dropped"
    );
    assert_eq!(
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("binding"),
        applied
    );
}
// MARK: - next start: native A/B

/// Build the real application with a distinct compiled identifier and copy it
/// out of Cargo's output. The Cargo artifact itself is only ever copied from.
fn build_identity(id: &str) -> Vec<u8> {
    cargo_build(Some(id));
    let binary = Path::new(&target_dir()).join("debug").join("xeHentai");
    // Package the copy the way a release would: the debug build carries
    // hundreds of megabytes of debug info, the installed image does not.
    let unpacked = tempfile::tempdir().expect("unpacked");
    let copy = unpacked.path().join("xeHentai");
    std::fs::copy(&binary, &copy).expect("copy built binary");
    let strip = Path::new("/usr/bin/strip");
    if strip.exists() {
        let output = std::process::Command::new(strip)
            .arg("--strip-unneeded")
            .arg(&copy)
            .output()
            .expect("strip copy");
        assert!(output.status.success(), "strip failed: {output:?}");
    }
    std::fs::read(&copy).expect("built binary bytes")
}

fn release_tree(install: &Path) {
    m6::copy_tree(&Path::new(MANIFEST).join("rss"), &install.join("rss"));
    m6::copy_tree(
        &Path::new(MANIFEST).join("locales"),
        &install.join("locales"),
    );
    m6::copy_tree(&Path::new(MANIFEST).join("ui"), &install.join("ui"));
}

#[test]
fn m7p_native_ab_reexec_runs_the_new_image_and_rejects_a_bad_candidate() {
    let _restore = RestoreDefaultBuild;
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let build_b = format!("b-{}", &UPDATE_ID[..8]);

    // Two REAL builds, sequentially, each copied out before any replacement.
    let binary_a = build_identity("a-native");
    let binary_b = build_identity(&build_b);

    let install = install_dir();
    release_tree(install.path());
    std::fs::write(install.path().join("xeHentai"), &binary_a).expect("install A");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            install.path().join("xeHentai"),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let exe = install.path().join("xeHentai");

    // Candidate: B's executable plus B's RSS tree (channel rss-b).
    let mut spec = Spec::new(UPDATE_ID).with_tree(
        &format!("xeHentai-{UPDATE_ID}/"),
        &candidate_rss(&origin, Some(&pem), "rss-b"),
    );
    spec = spec.native("xeHentai", &build_b, binary_b.clone());
    let pkg = sign(&key, &spec);
    serve_package(&server, &origin, &pkg);

    // A downloads B and keeps running A.
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "download"],
    );
    assert!(
        wait_log(install.path(), "Update is complete"),
        "no pending: {}",
        log_of(install.path())
    );
    let log = log_of(install.path());
    assert!(
        log.contains("xeHentai 2.023-rss+a-native started."),
        "{log}"
    );
    assert_eq!(child.stop(), 0);
    assert!(pending_path(install.path()).exists());
    assert_eq!(
        std::fs::read(&exe).expect("live exe"),
        binary_a,
        "A is still installed"
    );
    let session = std::fs::read(install.path().join("h.json")).expect("session");

    // The next start really re-executes: the running image reports B's
    // compiled identifier and B's RSS module drives the request.
    server.set(COMMITS_TARGET_B, Route::ok(COMMITS.as_bytes().to_vec()));
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "check"],
    );
    assert!(
        wait_log(
            install.path(),
            &format!("xeHentai 2.023-rss+{build_b} started.")
        ),
        "the replacement never ran B: {}",
        log_of(install.path())
    );
    assert!(
        m6::wait_for(Duration::from_secs(30), || {
            server
                .requests()
                .iter()
                .any(|request| request.target == COMMITS_TARGET_B)
        }),
        "B's RSS never ran: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);
    let log = log_of(install.path());
    assert!(log.contains("files installed"), "{log}");
    assert_eq!(
        std::fs::read(&exe).expect("live exe"),
        binary_b,
        "B is installed"
    );
    assert_eq!(
        std::fs::read(install.path().join("h.json")).expect("session"),
        session,
        "the previous session is read by B"
    );
    assert!(!pending_path(install.path()).exists());

    // Negative fixture: a deliberately broken executable (never an application
    // version) must be rejected by the preflight and leave B running. It
    // carries its own update id, so it is a real candidate rather than a stale
    // copy of the release that is already applied.
    const BAD_ID: &str = "2222222222222222222222222222222222222222";
    let mut bad_spec = Spec::new(BAD_ID).with_tree(
        &format!("xeHentai-{BAD_ID}/"),
        &candidate_rss(&origin, Some(&pem), "rss-b"),
    );
    bad_spec = bad_spec.native(
        "xeHentai",
        "b-broken-fixture",
        b"#!/bin/sh\nexit 7\n".to_vec(),
    );
    let bad_pkg = sign(&key, &bad_spec);

    let offset = log_of(install.path()).len();
    seed_pending(
        install.path(),
        bad_pkg.info.clone(),
        bad_pkg.sig.clone(),
        bad_pkg.payload.clone(),
    );
    let child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(install.path(), "Failure when updating program"),
        "the broken executable was not rejected: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);
    let log = log_of(install.path())[offset..].to_string();
    assert!(!log.contains("files installed"), "{log}");
    assert!(!log.contains("is running"), "{log}");
    assert!(
        log.contains(&format!("xeHentai 2.023-rss+{build_b} started.")),
        "B must still be the running image: {log}"
    );
    assert_eq!(
        std::fs::read(&exe).expect("live exe"),
        binary_b,
        "B stays installed"
    );
    assert_eq!(
        std::fs::read(install.path().join("h.json")).expect("session"),
        session
    );
}

// MARK: - address r1: reserved installer area, applied state, lock, bounds

fn area_path(install: &Path) -> PathBuf {
    install.join(AREA)
}

/// Byte snapshot of an installer tree (read compare; no hashing).
fn snapshot_tree(root: &Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    collect_files(root, Path::new(""), &mut out);
    out.sort();
    out
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).expect("chmod");
}

#[cfg(unix)]
fn mode_of(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).expect("mode").permissions().mode() & 0o7777
}

fn seed_installer_tree(install: &Path) -> Vec<(String, Vec<u8>)> {
    let area = area_path(install);
    crate::updater::install::ensure_private_dir(&area).expect("area");
    let pending = area.join("pending");
    crate::updater::install::ensure_private_dir(&pending).expect("pending dir");
    std::fs::write(pending.join("src.zip"), b"OLD-PENDING-BYTES").expect("pending bytes");
    std::fs::write(
        area.join("state.json"),
        br#"{"v":1,"active":{"update_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","build_id":"b-old","native":false,"applied_at":"2026-11-01T00:00:00+08:00"}}"#,
    )
    .expect("state");
    snapshot_tree(&area_path(install))
}

fn app_with_install_root(install: &Path, dir: &str) -> crate::core::App {
    let mut loaded = crate::config::Config::load(install).expect("config");
    loaded.config.dir = dir.to_string();
    crate::core::App::new(
        loaded.config,
        crate::util::logger::Logger::new(),
        install,
        install,
    )
    .expect("app")
}

fn titled_task(app: &crate::core::App, title: &str, dir: &str, base: &Path) -> crate::task::Task {
    let mut config =
        crate::task::TaskConfig::from_app(&app.cfg, &serde_json::Map::new()).expect("task config");
    config.dir = dir.to_string();
    config.delete_task_files = true;
    let policy = crate::task::PrivatePaths::for_app(base, base, &app.cfg.log_path);
    let mut task = crate::task::Task::new_with_private_paths(
        "https://exhentai.org/g/1/0123456789abcdef/",
        config,
        format!("guid-{title}"),
        base,
        policy,
    )
    .expect("task");
    task.meta
        .insert("title".to_string(), serde_json::json!(title));
    task
}

#[test]
fn m7p_installer_area_is_never_business_data() {
    let install = install_dir();
    let before = seed_installer_tree(install.path());
    let app = app_with_install_root(install.path(), ".");

    // The exact installer area title under dir="." resolves to the area.
    let mut task = titled_task(&app, AREA, ".", install.path());
    assert!(
        task.get_fpath().is_err(),
        "a gallery may not name the installer area"
    );
    assert!(
        task.read_grant().is_err(),
        "the area grants no gallery read"
    );
    assert!(
        task.cleanup(true).is_err(),
        "task cleanup may not delete the installer area"
    );
    task.state = crate::r#const::TASK_STATE_FINISHED;
    let mut app = app;
    let guid = app.insert_task_for_test(task);
    assert!(
        app.del_task(&guid).is_err(),
        "App task deletion may not delete the installer area"
    );
    assert_eq!(
        snapshot_tree(&area_path(install.path())),
        before,
        "the installer tree bytes must be unchanged"
    );

    // A canonical alias through a symlinked download root is the same area.
    #[cfg(unix)]
    {
        let alias = install.path().parent().unwrap().join(format!(
            "alias-{}",
            install.path().file_name().unwrap().to_string_lossy()
        ));
        let _ = std::fs::remove_file(&alias);
        std::os::unix::fs::symlink(install.path(), &alias).expect("alias");
        let app = app_with_install_root(install.path(), alias.to_str().unwrap());
        let task = titled_task(&app, AREA, alias.to_str().unwrap(), install.path());
        assert!(task.get_fpath().is_err(), "an aliased area is refused");
        assert!(task.read_grant().is_err(), "an aliased area grants no read");
        std::fs::remove_file(&alias).expect("unlink alias");
    }

    // An ANCESTOR of the area (the installation root itself) is refused too.
    let parent = install.path().parent().expect("parent").to_path_buf();
    let name = install
        .path()
        .file_name()
        .expect("name")
        .to_string_lossy()
        .to_string();
    let app = app_with_install_root(install.path(), parent.to_str().unwrap());
    let mut ancestor = titled_task(&app, &name, parent.to_str().unwrap(), install.path());
    assert!(
        ancestor.get_fpath().is_err(),
        "deleting an ancestor of the installer area is refused"
    );
    assert!(ancestor.cleanup(true).is_err());
    assert_eq!(
        snapshot_tree(&area_path(install.path())),
        before,
        "no ancestor attempt changed the installer tree"
    );

    // A legal sibling gallery keeps the normal delete semantics.
    let app = app_with_install_root(install.path(), ".");
    let mut legal = titled_task(&app, "normal-gallery", ".", install.path());
    let gallery = install.path().join("normal-gallery");
    std::fs::create_dir_all(&gallery).expect("gallery");
    std::fs::write(gallery.join("001.jpg"), b"img").expect("img");
    legal
        .cleanup(true)
        .expect("a legal gallery is still removable");
    assert!(!gallery.exists(), "the legal gallery was removed");
    assert_eq!(
        snapshot_tree(&area_path(install.path())),
        before,
        "the area survived"
    );
}

#[test]
fn m7p_private_installer_area_rejects_shared_metadata() {
    use crate::updater::install::{Installation, ensure_private_dir, private_dir_verdict};
    assert!(private_dir_verdict(1000, 0o700, 1000).is_ok());
    assert!(private_dir_verdict(1000, 0o500, 1000).is_ok());
    assert!(private_dir_verdict(1001, 0o700, 1000).is_err());
    assert!(private_dir_verdict(1000, 0o750, 1000).is_err());
    assert!(private_dir_verdict(1000, 0o707, 1000).is_err());

    let install = install_dir();
    let area = area_path(install.path());
    std::fs::create_dir_all(&area).expect("area");
    std::fs::write(area.join("sentinel"), b"KEEP").expect("sentinel");
    #[cfg(unix)]
    {
        set_mode(&area, 0o777);
        let error = ensure_private_dir(&area).expect_err("a world-writable area is refused");
        assert!(
            error.to_string().contains("accessible to group/others"),
            "{error}"
        );
        assert_eq!(
            mode_of(&area),
            0o777,
            "an unsafe existing dir is never chmodded"
        );
        assert_eq!(std::fs::read(area.join("sentinel")).unwrap(), b"KEEP");

        set_mode(&area, 0o750);
        assert!(
            ensure_private_dir(&area).is_err(),
            "a group-readable area is refused"
        );

        // An owner-only read-only directory is accepted but a write fails
        // naturally; the mode is not widened.
        set_mode(&area, 0o500);
        let installation = Installation::new(install.path());
        ensure_private_dir(&area).expect("0500 is owner-only");
        assert_eq!(mode_of(&area), 0o500);
        assert!(installation.discard_pending().is_err() || !area.join("pending").exists());
        set_mode(&area, 0o700);
    }
}

#[test]
fn m7p_archive_member_cap_rejects_signed_payload_without_pending_change() {
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let old = seed_pending(
        install.path(),
        b"old-info".to_vec(),
        b"old-sig".to_vec(),
        b"old-payload".to_vec(),
    );
    let mut spec = Spec::new(UPDATE_ID).with_tree(
        &format!("xeHentai-{UPDATE_ID}/"),
        &[("rss/updater/github.rss".to_string(), b"ok".to_vec())],
    );
    // A VALID signed payload whose foreign member count exceeds the archive
    // member bound: the digest and signature are correct by construction.
    for index in 0..(crate::updater::package::MAX_ARCHIVE_MEMBERS + 16) {
        spec.members
            .push((format!("foreign/{index}"), b"x".to_vec()));
    }
    let pkg = sign(&key, &spec);
    serve_package(&server, &origin, &pkg);

    let report = check_download(install.path(), "download");
    assert_eq!(report.outcome, "failed", "{:?}", report.events);
    assert!(
        event_keys(&report).iter().any(|key| key == "UPDATE_FAILED"),
        "the member bound is a real rejection"
    );
    assert_eq!(
        std::fs::read(pending_path(install.path())).expect("old pending"),
        old,
        "the previous pending is byte-identical"
    );
}

#[test]
fn m7p_cancelled_publication_before_commit_keeps_the_old_pending() {
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let old = seed_pending(
        install.path(),
        b"old-info".to_vec(),
        b"old-sig".to_vec(),
        b"old-payload".to_vec(),
    );
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &candidate_rss(&origin, Some(&pem), "rss-b"),
        ),
    );
    serve_package(&server, &origin, &pkg);
    let watched = pending_path(install.path())
        .parent()
        .expect("pending dir")
        .to_path_buf();

    let gate = Arc::new((Mutex::new(false), Condvar::new()));
    let hit = Arc::new((Mutex::new(false), Condvar::new()));
    {
        let gate = Arc::clone(&gate);
        let hit = Arc::clone(&hit);
        let watched = watched.clone();
        crate::updater::install::test_hooks::set(Some(Box::new(move |path: &Path| {
            if path != watched.as_path() {
                return;
            }
            *hit.0.lock().unwrap() = true;
            hit.1.notify_all();
            let mut released = gate.0.lock().unwrap();
            while !*released {
                released = gate.1.wait(released).unwrap();
            }
        })));
    }

    let install_path = install.path().to_path_buf();
    let publisher = crate::updater::install::Publisher::new();
    let publisher_for_task = publisher.clone();
    run_local(async move {
        let mut updater = crate::updater::Updater::load(&install_path).expect("updater");
        updater.set_publisher(publisher_for_task);
        let task = tokio::task::spawn_local(async move {
            let _ = updater
                .check("download", false, None, "xeHentai/test")
                .await;
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(30);
        while !*hit.0.lock().unwrap() {
            assert!(
                std::time::Instant::now() < deadline,
                "the pre-commit staging barrier was never reached"
            );
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        // The staging phase is complete; the commit has NOT happened.
        task.abort();
        let _ = task.await;
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        // Joins the real blocking job.
        publisher.retire().await;
    });
    crate::updater::install::test_hooks::set(None);

    assert_eq!(
        std::fs::read(pending_path(install.path())).expect("old pending"),
        old,
        "a cancellation before the commit preserves the previous pending bytes"
    );
    // The next independent operation still publishes normally.
    let report = check_download(install.path(), "download");
    assert_eq!(report.outcome, "pending");
    assert_ne!(
        std::fs::read(pending_path(install.path())).expect("new pending"),
        old
    );
}

#[cfg(unix)]
#[test]
fn m7p_install_lock_serializes_real_processes() {
    use crate::updater::install::{InstallLock, Installation};
    let install = install_dir();
    let installation = Installation::new(install.path());
    let lock = InstallLock::acquire(&installation).expect("lock");

    // A real second process with its OWN descriptor must be refused.
    let (rx, tx) = nix::unistd::pipe().expect("pipe");
    match unsafe { nix::unistd::fork() }.expect("fork") {
        nix::unistd::ForkResult::Child => {
            drop(rx);
            let message = match InstallLock::acquire(&installation) {
                Ok(_) => "acquired".to_string(),
                Err(error) => format!("busy {error}"),
            };
            let _ = nix::unistd::write(&tx, message.as_bytes());
            unsafe { nix::libc::_exit(0) };
        }
        nix::unistd::ForkResult::Parent { child } => {
            drop(tx);
            let mut buffer = [0u8; 1024];
            let read = nix::unistd::read(&rx, &mut buffer).expect("read");
            nix::sys::wait::waitpid(child, None).expect("wait");
            let text = String::from_utf8_lossy(&buffer[..read]).to_string();
            assert!(
                text.starts_with("busy"),
                "a concurrent process must be refused, not admitted: {text}"
            );
            assert!(
                !area_path(install.path()).join("state.json").exists(),
                "the refused process wrote nothing"
            );
        }
    }
    // Releasing this owner's descriptor makes the same lock available again.
    drop(lock);
    InstallLock::acquire(&installation).expect("a released lock is acquirable");
}
// MARK: - address r2: every stop joins the owned publication job

/// Child side of the non-signal stop proof. It is a REAL separate process
/// running the real `run_with_args` supervisor; the test-only publish barrier
/// (compiled out of production) holds the owned blocking job AFTER staging and
/// BEFORE the commit. The gallery completes naturally, so the supervisor stops
/// with `AutoExit` while that job is still held.
#[test]
fn m7p_stage_job_retention_child() {
    let Some(install) = std::env::var_os("XEH_M7P_CHILD_INSTALL") else {
        return;
    };
    let install = PathBuf::from(install);
    let hit = install.join(".stage-hit");
    let release = install.join(".stage-release");
    let watched = install.join(AREA).join("pending");
    {
        let hit = hit.clone();
        let release = release.clone();
        crate::updater::install::test_hooks::set(Some(Box::new(move |path: &Path| {
            if path != watched.as_path() {
                return;
            }
            std::fs::write(&hit, b"hit").expect("barrier hit");
            let deadline = std::time::Instant::now() + Duration::from_secs(120);
            while !release.exists() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "the stage barrier was never released"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
        })));
    }
    {
        let join_hit = install.join(".join-hit");
        crate::updater::install::test_hooks::set_join(Some(Box::new(move || {
            std::fs::write(&join_hit, b"join").expect("join marker");
        })));
    }
    let gallery = m6::Gallery::start(8);
    let code = crate::run_with_args(
        &install,
        [
            "xeHentai",
            "-l",
            "eh.log",
            "-vv",
            "-p",
            &gallery.proxy(),
            "--auto-update",
            "download",
            "http://e-hentai.org/g/1/aaaa",
        ],
    );
    assert_eq!(code, 0, "the natural gallery completion must exit zero");
}

#[test]
fn m7p_non_signal_stop_joins_the_held_stage_job() {
    let Some(child_exe) = std::env::current_exe().ok() else {
        return;
    };
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    // No pending candidate exists before this run: startup consumes none, so
    // the only pending bytes are the ones the held job would publish.
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &candidate_rss(&origin, Some(&pem), "rss-b"),
        ),
    );
    serve_package(&server, &origin, &pkg);
    std::fs::create_dir_all(install.path().join("home")).unwrap();
    std::fs::create_dir_all(install.path().join("tmp")).unwrap();
    std::fs::create_dir_all(install.path().join("cache")).unwrap();
    let mut child = std::process::Command::new(child_exe)
        .current_dir(install.path())
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", install.path().join("home"))
        .env("TMPDIR", install.path().join("tmp"))
        .env("XDG_CACHE_HOME", install.path().join("cache"))
        .env("LANG", "C")
        .env("LC_ALL", "C")
        .env("XEH_M7P_CHILD_INSTALL", install.path())
        .args([
            "--exact",
            "tests::m7p::m7p_stage_job_retention_child",
            "--nocapture",
            "--test-threads=1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn the real supervisor process");

    assert!(
        m6::wait_for(Duration::from_secs(60), || install
            .path()
            .join(".stage-hit")
            .exists()),
        "the stage barrier was never reached"
    );
    // The blocking job is parked after staging, before the persist gate: no
    // pending envelope may exist while the gallery finishes.
    assert!(
        !pending_path(install.path()).exists(),
        "a staged job published before its commit gate"
    );
    // The supervisor reached its publication join (the stop already happened
    // and the local updater task was aborted) while the job is still held.
    assert!(
        m6::wait_for(Duration::from_secs(60), || install
            .path()
            .join(".join-hit")
            .exists()),
        "the supervisor never reached the publication join"
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        child.try_wait().expect("try_wait").is_none(),
        "AutoExit returned while the owned publication job was still held"
    );
    assert!(
        !pending_path(install.path()).exists(),
        "the held job must not publish while parked"
    );
    std::fs::write(install.path().join(".stage-release"), b"go").expect("release");
    let status = m6::wait_exit(&mut child, Duration::from_secs(60)).expect("child exit");
    assert_eq!(status.code(), Some(0), "the supervisor must finish cleanly");
    assert!(
        !pending_path(install.path()).exists(),
        "a cancellation after staging must not publish a late candidate"
    );
    let leftovers: Vec<String> =
        std::fs::read_dir(pending_path(install.path()).parent().expect("pending dir"))
            .expect("pending dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name != "src.zip")
            .collect();
    assert!(
        leftovers.is_empty(),
        "the retired job must not leave staged temp files: {leftovers:?}"
    );
    // A fresh run owns a clean publisher and still publishes normally.
    let report = check_download(install.path(), "download");
    assert_eq!(report.outcome, "pending");
    assert!(pending_path(install.path()).exists());
}

// MARK: - address r3: untrusted recovery state stops before the App

// MARK: - installer area and pending path validation

fn private_dir(path: &Path) {
    std::fs::create_dir_all(path).expect("private dir");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Every installer-created level must be private, exactly as the real
        // create path leaves it. Only levels at or below the installer's own
        // `.xehentai-update` are touched.
        let mut chain: Vec<&Path> = vec![path];
        let mut current = path.parent();
        while let Some(dir) = current {
            chain.push(dir);
            if dir
                .file_name()
                .is_some_and(|name| name == ".xehentai-update")
            {
                break;
            }
            current = dir.parent();
        }
        if !chain
            .last()
            .and_then(|dir| dir.file_name())
            .is_some_and(|name| name == ".xehentai-update")
        {
            chain.truncate(1);
        }
        for dir in chain {
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                .expect("chmod 0700");
        }
    }
}

// MARK: - startup write lock
#[test]
fn m7p_busy_startup_stops_before_the_business_app() {
    use crate::updater::install::{InstallLock, Installation};
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &candidate_rss(&origin, Some(&pem), "rss-b"),
        ),
    );
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    serve_package(&server, &origin, &pkg);
    let old_binding =
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("old binding");
    seed_pending(
        install.path(),
        pkg.info.clone(),
        pkg.sig.clone(),
        pkg.payload.clone(),
    );
    let exe = owned_exe(install.path());
    let installation = Installation::new(install.path());
    // This process holds the real transaction lock, exactly as a concurrent
    // updater mid-rename would.
    let held = InstallLock::acquire(&installation).expect("held lock");
    let mut child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    let status = m6::wait_exit(
        &mut child.child.as_mut().expect("child"),
        Duration::from_secs(30),
    )
    .expect("a busy startup must stop instead of running the App")
    .code();
    child.child.take();
    drop(held);
    assert_eq!(
        status,
        Some(75),
        "a busy startup is a retryable explicit exit"
    );
    let log = log_of(install.path());
    assert!(
        !log.contains("started."),
        "the business App started on a tree another process is switching: {log}"
    );
    assert_eq!(
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live"),
        old_binding,
        "the busy startup modified the live tree"
    );
    assert!(
        pending_path(install.path()).exists(),
        "the busy startup consumed the pending candidate"
    );
    assert!(
        !install.path().join("h.json").exists(),
        "the busy startup wrote a session"
    );
}

// MARK: - address r2: pending subdirectory and leaf validation

#[test]
fn m7p_pending_inspection_does_not_create_or_follow_unsafe_paths() {
    use crate::updater::install::{Installation, ensure_private_dir};
    let install = install_dir();
    let installation = Installation::new(install.path());
    // A missing area/subdirectory is "no candidate", never an empty one.
    assert!(!installation.pending_present().expect("absent probe"));
    assert!(
        !install.path().join(AREA).exists(),
        "the probe created the area"
    );
    ensure_private_dir(&install.path().join(AREA)).expect("area");
    assert!(!installation.pending_present().expect("absent pending"));
    assert!(
        !install.path().join(AREA).join("pending").exists(),
        "the probe created the pending directory"
    );

    // A normal private pending envelope is readable and discardable.
    let bytes = seed_pending(install.path(), b"i".to_vec(), b"s".to_vec(), b"p".to_vec());
    assert!(installation.pending_present().expect("present"));
    let envelope = installation
        .read_pending()
        .expect("read normal pending")
        .expect("envelope");
    assert_eq!(envelope.payload, b"p".to_vec());
    assert_eq!(
        std::fs::read(pending_path(install.path())).expect("bytes"),
        bytes
    );
    installation.discard_pending().expect("discard normal");
    assert!(!pending_path(install.path()).exists());

    // A read-only private directory keeps its REAL write failure instead of
    // being repaired to make a test pass.
    let second = install_dir();
    let second_installation = Installation::new(second.path());
    seed_pending(second.path(), b"i".to_vec(), b"s".to_vec(), b"p".to_vec());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            second.path().join(AREA).join("pending"),
            std::fs::Permissions::from_mode(0o500),
        )
        .expect("read-only pending");
    }
    assert!(
        second_installation
            .read_pending()
            .expect("read only")
            .is_some()
    );
    assert!(
        second_installation.discard_pending().is_err(),
        "a read-only pending directory must fail the unlink for real"
    );
}

#[cfg(unix)]
#[test]
fn m7p_pending_dir_symlink_and_leaf_symlink_are_refused_before_read_or_delete() {
    use crate::updater::install::{Installation, ensure_private_dir};
    let install = install_dir();
    let installation = Installation::new(install.path());
    let area = install.path().join(AREA);
    private_dir(&area);
    let outside = tempfile::tempdir().expect("outside");
    let outside_pending = outside.path().join("pending");
    private_dir(&outside_pending);
    let envelope = crate::updater::package::PendingEnvelope::new(
        b"outside-info".to_vec(),
        b"outside-sig".to_vec(),
        b"outside-payload".to_vec(),
    )
    .expect("envelope");
    crate::updater::package::publish(&outside_pending, &envelope).expect("outside publish");
    let sentinel = std::fs::read(outside_pending.join("src.zip")).expect("sentinel");
    // A symlinked pending DIRECTORY must not be probed, read or deleted.
    std::os::unix::fs::symlink(&outside_pending, area.join("pending")).expect("dir symlink");
    assert!(
        installation.pending_present().is_err(),
        "a symlinked pending directory was trusted"
    );
    assert!(
        installation.read_pending().is_err(),
        "a symlinked pending directory was read"
    );
    assert!(
        installation.discard_pending().is_err(),
        "a symlinked pending directory was deleted through"
    );
    assert_eq!(
        std::fs::read(outside_pending.join("src.zip")).expect("sentinel"),
        sentinel,
        "an outside envelope was read or deleted through the symlinked directory"
    );
    std::fs::remove_file(area.join("pending")).expect("remove dir symlink");

    // A symlinked LEAF inside a real private directory is refused the same way.
    ensure_private_dir(&area.join("pending")).expect("pending dir");
    let target = outside.path().join("leaf.zip");
    std::fs::write(&target, &sentinel).expect("outside leaf");
    std::os::unix::fs::symlink(&target, pending_path(install.path())).expect("leaf symlink");
    assert!(
        installation.read_pending().is_err(),
        "a symlinked pending leaf was read"
    );
    assert!(
        installation.discard_pending().is_err(),
        "a symlinked pending leaf was deleted"
    );
    assert_eq!(
        std::fs::read(&target).expect("outside leaf"),
        sentinel,
        "the outside leaf was modified"
    );
    assert!(
        pending_path(install.path()).is_symlink(),
        "the leaf symlink was removed instead of refused"
    );
}

#[cfg(unix)]
#[test]
fn m7p_startup_refuses_a_symlinked_pending_leaf_before_reading_it() {
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(
            &format!("xeHentai-{UPDATE_ID}/"),
            &candidate_rss(&origin, Some(&pem), "rss-b"),
        ),
    );
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    serve_package(&server, &origin, &pkg);
    let old_binding =
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("old binding");
    let outside = tempfile::tempdir().expect("outside");
    let outer_pending = outside.path().join("pending");
    private_dir(&outer_pending);
    let envelope = crate::updater::package::PendingEnvelope::new(
        pkg.info.clone(),
        pkg.sig.clone(),
        pkg.payload.clone(),
    )
    .expect("envelope");
    crate::updater::package::publish(&outer_pending, &envelope).expect("outside publish");
    let outside_leaf = outer_pending.join("src.zip");
    let outside_bytes = std::fs::read(&outside_leaf).expect("outside envelope");
    private_dir(&install.path().join(AREA).join("pending"));
    std::os::unix::fs::symlink(&outside_leaf, pending_path(install.path())).expect("leaf symlink");
    let exe = owned_exe(install.path());

    let mut child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    let code = m6::wait_exit(
        &mut child.child.as_mut().expect("child"),
        Duration::from_secs(30),
    )
    .expect("an unsafe pending leaf must stop startup")
    .code();
    child.child.take();
    assert_eq!(code, Some(1));
    let log = log_of(install.path());
    assert!(
        !log.contains("started."),
        "the App started with an unsafe pending leaf: {log}"
    );
    assert_eq!(
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live"),
        old_binding,
        "the candidate outside the area was applied"
    );
    assert_eq!(
        std::fs::read(&outside_leaf).expect("outside leaf"),
        outside_bytes,
        "the outside envelope was read, consumed or modified"
    );
    assert!(
        pending_path(install.path()).is_symlink(),
        "the refused leaf symlink was removed"
    );
    assert!(!area_path(install.path()).join("activation.json").exists());
    assert!(!area_path(install.path()).join("state.json").exists());
}

// MARK: - address r1: apply-phase failure and truthful installed-files event

/// One live target of a signed entry is an incompatible type (a directory), so
/// a LATER `rename(2)` in the real apply loop fails with EISDIR after earlier
/// files were really replaced. On the old source that ordinary error is treated
/// like a preparation rejection: pending is discarded and the start continues
/// into the business App on the mixed tree. The corrected behavior stops THIS
/// start with a nonzero status before App/session/tasks, keeps the pending
/// candidate and writes no applied identity and no completion event.
#[test]
fn m7p_r1_late_apply_failure_stops_the_start_before_the_business_app() {
    use std::os::unix::fs::MetadataExt as _;
    let key = signing_key();
    let pem = public_pem(&key);
    let server = Server::start(vec![]);
    let origin = server.origin();
    let mut files = candidate_rss(&origin, Some(&pem), "rss-b");
    // A signed resource the business modules never load: refusing exactly this
    // path can only come from the apply loop, not from module loading.
    let probe = "rss/update-probe/apply-target.bin";
    files.push((probe.to_string(), b"probe\n".to_vec()));
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(&format!("xeHentai-{UPDATE_ID}/"), &files),
    );
    let install = install_dir();
    publish_rss(install.path(), &origin, Some(&pem), "rss");
    let exe = owned_exe(install.path());
    let first_live = install.path().join("rss/filters/match.rss");
    let first_inode = std::fs::metadata(&first_live).expect("first live").ino();
    let last_live = install.path().join("rss/updater/updater.rss");
    // The live target of the LAST signed entry is a directory.
    let sabotage = install.path().join(probe);
    std::fs::create_dir_all(sabotage.parent().expect("probe parent")).expect("probe dir");
    std::fs::create_dir(&sabotage).expect("probe leaf");
    let seeded = seed_pending(
        install.path(),
        pkg.info.clone(),
        pkg.sig.clone(),
        pkg.payload.clone(),
    );

    let mut child = ChildRun::spawn(
        install.path(),
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    let exit = m6::wait_exit(
        child.child.as_mut().expect("child"),
        Duration::from_secs(30),
    );
    drop(child);
    let log = log_of(install.path());
    let code = exit.and_then(|status| status.code());
    assert_eq!(
        code,
        Some(1),
        "a failed live apply must stop this start with a nonzero status before the business App \
         (None means it kept running); log: {log}"
    );
    assert!(
        !log.contains("started."),
        "the business App started on a mixed tree: {log}"
    );
    assert!(
        log.contains("Failure when updating program") && log.contains("update application failed"),
        "the real apply error was not reported: {log}"
    );
    assert!(
        log.contains("Is a directory"),
        "the report does not carry the real replacement error: {log}"
    );
    // The earlier replacements really happened, so this is a mixed tree.
    let first_after = std::fs::metadata(&first_live)
        .expect("first live after")
        .ino();
    assert_ne!(
        first_inode, first_after,
        "the first live replacement never happened"
    );
    let expected_last = files
        .iter()
        .find(|(path, _)| path == "rss/updater/updater.rss")
        .map(|(_, body)| body.clone())
        .expect("candidate updater module");
    assert_eq!(
        std::fs::read(&last_live).expect("last live after"),
        expected_last,
        "the last replacement before the failure never happened"
    );
    assert!(
        std::fs::read(pending_path(install.path())).expect("pending kept") == seeded,
        "the pending candidate was consumed on an apply failure"
    );
    assert!(
        !area_path(install.path()).join("state.json").exists(),
        "an applied identity was published on an apply failure"
    );
    assert!(
        !install.path().join("h.json").exists(),
        "the session was written on an apply failure"
    );
    assert!(
        !log.contains("Update is complete") && !log.contains("files installed"),
        "the failed apply published a completion event: {log}"
    );
}

/// The installed-files event states exactly what happened before the native
/// image is started. It must not claim that this start already runs the new
/// version; the real new process's own version log is the only proof of that.
#[test]
fn m7p_r1_installed_files_event_is_neutral_in_every_locale() {
    use crate::i18n::{self, Locale};
    let cases = [
        (Locale::EnUs, "Update files installed: rss-b6"),
        (Locale::ZhHans, "更新文件已安装: rss-b6"),
        (Locale::ZhHant, "更新檔案已安裝: rss-b6"),
    ];
    for (locale, expected) in cases {
        let text = i18n::render(locale, "UPDATE_APPLIED", &["rss-b6"]).expect("UPDATE_APPLIED");
        assert_eq!(text, expected, "{locale:?}");
        assert!(
            !text.contains("runs the new version")
                && !text.contains("运行新版本")
                && !text.contains("執行新版本"),
            "{locale:?} still claims the new program runs: {text}"
        );
    }
}

/// The real main branch that starts an installed native image. Here the copied
/// TEMP image is revoked AFTER file application, so the real `execv` reports
/// the real OS error. The start must report `UPDATE_FAILED`, exit nonzero and
/// never claim the new version is running. Only an independent copied TEMP
/// executable is involved: the Cargo test binary is never self-replaced or
/// executed, and no production fault flag exists.
#[test]
fn m7p_r1_reexec_failure_reports_failed_without_claiming_a_running_version() {
    use crate::i18n::Locale;
    let install = install_dir();
    let exe = install.path().join("installed-candidate");
    std::fs::copy(m6::binary(), &exe).expect("copy the installed TEMP image");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    }
    // Revoke the installed image after the files are installed: the real exec
    // now fails for a real OS reason.
    std::fs::remove_file(&exe).expect("revoke the installed image");
    let mut logger = crate::util::logger::Logger::new();
    logger
        .set_logfile(&install.path().join("eh.log"))
        .expect("open the log file");
    let code = crate::reexec_installed(Locale::EnUs, &mut logger, &exe);
    assert_eq!(code, 1, "a failed exec must exit nonzero");
    let log = log_of(install.path());
    assert!(
        log.contains("Failure when updating program") && log.contains("No such file or directory"),
        "the real exec error was not reported: {log}"
    );
    assert!(
        !log.contains("runs the new version") && !log.contains("files installed"),
        "the failed exec start claimed an installed/running update: {log}"
    );
}

// MARK: - B6 update compatibility probe
//
// `tests::compat_probe` dispatches the `update` suite here (Unix only: the
// probe spawns real child processes and uses POSIX signals, exactly like the
// M7p tests above). Every observation below comes from the REAL production
// path: `Updater::load` + `check` over the Core HTTP host against an owned
// loopback source, the real signed package/pending primitives, and the real
// compiled binary for startup apply/reject and the CLI `off` gate. Nothing is
// mocked and no second business implementation exists.

fn b6_fresh(root: &Path, name: &str) -> PathBuf {
    let dir = root.join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("b6 clean install");
    }
    std::fs::create_dir_all(&dir).expect("b6 install dir");
    dir
}

fn b6_feed(requests: &[Request]) -> serde_json::Value {
    serde_json::json!({
        "request_count": requests.len(),
        "methods": requests.iter().map(|r| r.method.clone()).collect::<Vec<_>>(),
        "targets": requests.iter().map(|r| r.target.clone())
            .collect::<Vec<_>>(),
        "statuses": requests.iter().map(|_| 200).collect::<Vec<_>>(),
    })
}

fn b6_events(report: &crate::updater::CheckReport) -> serde_json::Value {
    serde_json::Value::Array(
        report
            .events
            .iter()
            .map(|event| {
                serde_json::json!({
                    "level": event.level,
                    "key": event.key,
                    "text": crate::updater::render_event(
                        crate::i18n::Locale::EnUs, event),
                })
            })
            .collect(),
    )
}

fn b6_commit(info: &crate::updater::UpdateInfo) -> serde_json::Value {
    serde_json::json!({
        "type": "UpdateInfo",
        "update_id": info.update_id,
        "update_id_type": "str",
        "ts": info.ts,
        "message": info.message,
    })
}

fn b6_absent_commit() -> serde_json::Value {
    serde_json::json!({
        "type": serde_json::Value::Null,
        "update_id": serde_json::Value::Null,
        "update_id_type": serde_json::Value::Null,
        "ts": serde_json::Value::Null,
        "message": serde_json::Value::Null,
    })
}

fn b6_channel_of(target: &str) -> String {
    target
        .split_once("sha=")
        .map(|(_, rest)| rest.to_string())
        .unwrap_or_default()
}

fn b6_source(info: Option<&crate::updater::UpdateInfo>, commits_url: &str) -> serde_json::Value {
    match info {
        Some(info) => serde_json::json!({
            "channel": b6_channel_of(commits_url),
            "channel_url": commits_url,
            "download_link": info.download_link,
            "descriptor_link": info.descriptor_link,
            "signature_link": info.signature_link,
            "archive_prefix": info.archive_prefix,
        }),
        None => serde_json::json!({
            "channel": "",
            "channel_url": "",
            "download_link": "",
            "descriptor_link": "",
            "signature_link": "",
            "archive_prefix": "",
        }),
    }
}

fn b6_pending_bytes(install: &Path) -> Option<Vec<u8>> {
    std::fs::read(pending_path(install)).ok()
}

/// Private installer area with a pre-existing (previous-run) pending file.
fn b6_seed_sentinel(install: &Path) -> PathBuf {
    let pending = install.join(AREA).join("pending");
    private_dir(&pending);
    let leaf = pending.join("src.zip");
    std::fs::write(
        &leaf,
        b"b6 pre-existing pending sentinel, deliberately not a zip\n",
    )
    .expect("sentinel pending");
    leaf
}

fn b6_decision(
    mode: &str,
    applied: Option<&str>,
    outcome: &str,
    report: Option<&crate::updater::CheckReport>,
    before: Option<&[u8]>,
    after: Option<&[u8]>,
) -> serde_json::Value {
    serde_json::json!({
        "mode": mode,
        "applied": applied,
        "outcome": outcome,
        "events": report.map(b6_events).unwrap_or(serde_json::Value::Array(vec![])),
        "pending_before": before.is_some(),
        "pending_after": after.is_some(),
        "pending_equal": before == after,
    })
}

fn b6_zip_members(bytes: &[u8]) -> Vec<(String, u16, Vec<u8>)> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes.to_vec())).expect("b6 zip archive");
    let mut out = Vec::new();
    for index in 0..archive.len() {
        let mut file = archive.by_index(index).expect("b6 zip member");
        let name = file.name().to_string();
        let method: u16 = match file.compression() {
            zip::CompressionMethod::Stored => 0,
            _ => 8,
        };
        let mut body = Vec::new();
        file.read_to_end(&mut body).expect("b6 zip body");
        out.push((name, method, body));
    }
    out
}

fn b6_package_bodies(inputs: &serde_json::Value) -> Vec<(String, Vec<u8>)> {
    let table = inputs["package_bodies"]
        .as_object()
        .expect("b6 package bodies");
    let mut out: Vec<(String, Vec<u8>)> = table
        .iter()
        .map(|(path, body)| {
            (
                path.clone(),
                body.as_str().expect("b6 body text").as_bytes().to_vec(),
            )
        })
        .collect();
    out.sort();
    out
}

/// Exact controlled content of every SELECTED member, read from the real
/// archive bytes and keyed by its install-relative selected path.
fn b6_selected_member_bodies(
    members: &[(String, u16, Vec<u8>)],
    selected: &[crate::updater::package::SelectedEntry],
) -> serde_json::Map<String, serde_json::Value> {
    let bodies: std::collections::BTreeMap<&str, &[u8]> = members
        .iter()
        .map(|(name, _, body)| (name.as_str(), body.as_slice()))
        .collect();
    let mut out = serde_json::Map::new();
    for item in selected {
        let body = bodies
            .get(item.member.as_str())
            .expect("b6 selected member body");
        out.insert(item.path.clone(), serde_json::Value::String(b6_hex(body)));
    }
    out
}

fn b6_package_probe(
    value_bodies: &[(String, Vec<u8>)],
    foreign: &[&str],
    install: &Path,
) -> serde_json::Value {
    let key = signing_key();
    let pem = public_pem(&key);
    let prefix = format!("xeHentai-{UPDATE_ID}/");
    let mut spec = Spec::new(UPDATE_ID);
    for (path, body) in value_bodies {
        spec.entries.push((path.clone(), body.len() as u64));
        spec.members.push((format!("{prefix}{path}"), body.clone()));
    }
    for name in foreign {
        spec.members.push((name.to_string(), b"foreign\n".to_vec()));
    }
    let pkg = sign(&key, &spec);
    seed_pending(
        install,
        pkg.info.clone(),
        pkg.sig.clone(),
        pkg.payload.clone(),
    );
    let members = pending_members(install);
    let names: Vec<String> = members.iter().map(|m| m.name.clone()).collect();
    let compress: Vec<u16> = members
        .iter()
        .map(|m| if m.stored { 0 } else { 8 })
        .collect();
    let payload_members = b6_zip_members(&pkg.payload);
    let descriptor =
        crate::updater::descriptor::authenticate(&pkg.info, &pkg.sig, Some(pem.as_str()))
            .expect("b6 descriptor authenticate");
    let selected = crate::updater::package::select(&pkg.payload, &descriptor, &prefix)
        .expect("b6 package select");
    let selected_paths: Vec<String> = selected.iter().map(|item| item.path.clone()).collect();
    let selected_members: Vec<String> = selected.iter().map(|item| item.member.clone()).collect();
    let dropped: Vec<String> = payload_members
        .iter()
        .map(|(name, _, _)| name.clone())
        .filter(|name| !selected_members.contains(name))
        .collect();
    let info: serde_json::Value = serde_json::from_slice(&pkg.info).expect("b6 descriptor json");
    let member_bodies = b6_selected_member_bodies(&payload_members, &selected);
    serde_json::json!({
        "member_names": names,
        "compress_types": compress,
        "input_members": payload_members.iter().map(|(n, _, _)| n.clone())
            .collect::<Vec<_>>(),
        "input_compress_types": payload_members.iter().map(|(_, m, _)| *m)
            .collect::<Vec<_>>(),
        "info": info,
        "selected_entries": selected_paths,
        "dropped_entries": dropped,
        "member_bodies": member_bodies,
        "payload_sha256": crate::updater::descriptor::sha256_hex(&pkg.payload),
        "info_archive_sha256": descriptor.archive_sha256,
    })
}

fn b6_pending_observation(install: &Path) -> serde_json::Value {
    let members = pending_members(install);
    let names: Vec<String> = members.iter().map(|m| m.name.clone()).collect();
    let compress: Vec<u16> = members
        .iter()
        .map(|m| if m.stored { 0 } else { 8 })
        .collect();
    let payload = members
        .iter()
        .find(|m| m.name == "payload.zip")
        .map(|m| m.body.clone())
        .expect("b6 pending payload member");
    let info_member = members
        .iter()
        .find(|m| m.name == "info.json")
        .map(|m| m.body.clone())
        .expect("b6 pending info member");
    let payload_members = b6_zip_members(&payload);
    let descriptor = crate::updater::descriptor::authenticate(
        &info_member,
        &members
            .iter()
            .find(|m| m.name == "info.sig")
            .map(|m| m.body.clone())
            .expect("b6 pending signature member"),
        Some(public_pem(&signing_key()).as_str()),
    )
    .expect("b6 pending descriptor");
    let prefix = format!("xeHentai-{UPDATE_ID}/");
    let selected =
        crate::updater::package::select(&payload, &descriptor, &prefix).expect("b6 pending select");
    let selected_paths: Vec<String> = selected.iter().map(|item| item.path.clone()).collect();
    let selected_members: Vec<String> = selected.iter().map(|item| item.member.clone()).collect();
    let dropped: Vec<String> = payload_members
        .iter()
        .map(|(name, _, _)| name.clone())
        .filter(|name| !selected_members.contains(name))
        .collect();
    let info: serde_json::Value = serde_json::from_slice(&info_member).expect("b6 info json");
    let member_bodies = b6_selected_member_bodies(&payload_members, &selected);
    serde_json::json!({
        "member_names": names,
        "compress_types": compress,
        "input_members": payload_members.iter().map(|(n, _, _)| n.clone())
            .collect::<Vec<_>>(),
        "input_compress_types": payload_members.iter().map(|(_, m, _)| *m)
            .collect::<Vec<_>>(),
        "info": info,
        "selected_entries": selected_paths,
        "dropped_entries": dropped,
        "member_bodies": member_bodies,
        "payload_sha256": crate::updater::descriptor::sha256_hex(&payload),
        "info_archive_sha256": descriptor.archive_sha256,
    })
}

fn b6_check(
    install: &Path,
    mode: &str,
    beta: bool,
    applied: Option<&str>,
) -> crate::updater::CheckReport {
    let mode = mode.to_string();
    let applied = applied.map(str::to_string);
    let install = install.to_path_buf();
    run_local(async move {
        let mut updater = crate::updater::Updater::load(&install).expect("b6 load updater");
        updater
            .check(&mode, beta, applied.as_deref(), "xeHentai/b6")
            .await
            .expect("b6 check")
    })
}

fn b6_publish_binding(root: &Path, origin: &str, pem: &str, stable: &str, beta: Option<&str>) {
    publish_rss(root, origin, Some(pem), stable);
    if let Some(beta) = beta {
        let path = root.join("rss/updater/github.rss");
        let source = std::fs::read_to_string(&path).expect("b6 binding");
        let source = source.replace("beta_channel: null,", &format!("beta_channel: \"{beta}\","));
        std::fs::write(&path, source).expect("b6 beta binding");
    }
}

fn b6_started_version(log: &str) -> Option<String> {
    let start = log.find("xeHentai ")? + "xeHentai ".len();
    let rest = &log[start..];
    let end = rest.find(" started.")?;
    Some(rest[..end].to_string())
}

/// Exact lowercase hex of the ACTUAL member bytes (reversible test JSON).
fn b6_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// The exact executed rejection detail rendered by the typed error's own
/// Display (the same string `main` logs), taken from the real run's log.
fn b6_rejection_detail(log: &str) -> Option<String> {
    const PREFIX: &str = "Failure when updating program: ";
    let start = log.rfind(PREFIX)? + PREFIX.len();
    Some(log[start..].lines().next().unwrap_or("").trim().to_string())
}

/// Map the executed typed error's rendered form to its `UpdateError`
/// variant. The class is read from the real rejection message, never from a
/// scenario label or a fixed field.
fn b6_error_class(detail: &str) -> Option<&'static str> {
    const PREFIXES: [(&str, &str); 5] = [
        ("update package rejected: ", "Package"),
        ("update installation failed: ", "Install"),
        ("update application failed: ", "Apply"),
        ("update source binding error: ", "Binding"),
        ("update installation is busy: ", "Busy"),
    ];
    const MARKERS: [(&str, &str); 4] = [
        (" compile error: ", "Compile"),
        (" host binding error: ", "Bind"),
        (" runtime error: ", "Runtime"),
        (" shape error: ", "Shape"),
    ];
    for (prefix, class) in PREFIXES {
        if detail.starts_with(prefix) {
            return Some(class);
        }
    }
    for (marker, class) in MARKERS {
        if detail.contains(marker) {
            return Some(class);
        }
    }
    None
}

fn b6_wait_log_after(install: &Path, offset: usize, needle: &str) -> bool {
    m6::wait_for(Duration::from_secs(30), || {
        let log = log_of(install);
        log.len() >= offset && log[offset..].contains(needle)
    })
}

fn b6_state_active(install: &Path) -> Option<String> {
    let bytes = std::fs::read(install.join(AREA).join("state.json")).ok()?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).ok()?;
    value["active"]["update_id"].as_str().map(str::to_string)
}

/// The real downloaded package for the apply cases: candidate RSS tree (the
/// patched binding asks for `rss-b`) plus the executable-free envelope.
fn b6_candidate_package(origin: &str, pem: &str, broken: bool) -> Pkg {
    let key = signing_key();
    let mut files = candidate_rss(origin, Some(pem), "rss-b");
    if broken {
        for (path, body) in files.iter_mut() {
            if path == "rss/updater/github.rss" {
                *body = b"fn broken( {".to_vec();
            }
        }
    }
    sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(&format!("xeHentai-{UPDATE_ID}/"), &files),
    )
}

/// Copy the already-built plain binary into one installation (the plain
/// `cargo build` is performed once per probe, not per case).
fn b6_owned_exe(install: &Path) -> PathBuf {
    let exe = install.join("xeHentai");
    std::fs::copy(
        Path::new(&target_dir()).join("debug").join("xeHentai"),
        &exe,
    )
    .expect("b6 copy default build");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).expect("b6 chmod");
    }
    exe
}

/// A package whose detached signature is made by an untrusted key: the
/// binding verifies with its own trusted key and must reject it, leaving the
/// previous pending byte-identical and fetching no payload.
fn b6_package_reject(
    root: &Path,
    server: &Server,
    bodies: &[(String, Vec<u8>)],
) -> serde_json::Value {
    let key = signing_key();
    let pem = public_pem(&key);
    let install = b6_fresh(root, "package-reject");
    let origin = server.origin();
    publish_rss(&install, &origin, Some(&pem), "rss");
    b6_seed_sentinel(&install);
    let before = b6_pending_bytes(&install);
    let mut spec = Spec::new(UPDATE_ID);
    for (path, body) in bodies {
        spec.entries.push((path.clone(), body.len() as u64));
        spec.members
            .push((format!("xeHentai-{UPDATE_ID}/{path}"), body.clone()));
    }
    // Signed by a key this binding does not trust.
    let pkg = sign(&other_key(), &spec);
    serve_package(server, &origin, &pkg);
    let before_requests = server.requests().len();
    let report = b6_check(&install, "download", false, None);
    assert_eq!(
        report.outcome, "failed",
        "an untrusted signature must be rejected"
    );
    let requests: Vec<Request> = server.requests()[before_requests..].to_vec();
    let after = b6_pending_bytes(&install);
    let targets: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| serde_json::Value::String(request.target.clone()))
        .collect();
    serde_json::json!({
        "side": "native",
        "sections": {
            "feed": b6_feed(&requests),
            "decision": b6_decision(
                "download", None, &report.outcome, Some(&report),
                before.as_deref(), after.as_deref()),
        },
        "evidence": {"request_targets": targets},
    })
}

fn b6_apply_valid(root: &Path, server: &Server) -> serde_json::Value {
    let key = signing_key();
    let pem = public_pem(&key);
    let install = b6_fresh(root, "apply-valid");
    let origin = server.origin();
    publish_rss(&install, &origin, Some(&pem), "rss");
    let pkg = b6_candidate_package(&origin, &pem, false);
    serve_package(server, &origin, &pkg);
    let exe = b6_owned_exe(&install);
    let live_before = std::fs::read(install.join("rss/updater/github.rss")).expect("live before");

    // Run 1: a real download publishes pending, without applying it.
    let before_run1 = server.requests().len();
    let child = ChildRun::spawn(
        &install,
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "download"],
    );
    assert!(
        wait_log(&install, "Update is complete"),
        "no pending publication: {}",
        log_of(&install)
    );
    assert_eq!(child.stop(), 0);
    let pending_before = pending_path(&install).exists();
    assert!(
        pending_before,
        "the download published no pending candidate"
    );
    let session = std::fs::read(install.join("h.json")).expect("b6 session after run 1");
    let run1_requests = server.requests();
    let run1: Vec<&str> = run1_requests[before_run1..]
        .iter()
        .map(|request| request.target.as_str())
        .collect();
    let (payload_target, descriptor_target, signature_target) = package_targets(&origin, UPDATE_ID);
    assert_eq!(
        run1,
        vec![
            COMMITS_TARGET,
            descriptor_target.as_str(),
            signature_target.as_str(),
            payload_target.as_str()
        ],
        "a download is one commits request plus the signed package triple"
    );

    // Run 2: the next start applies the candidate through the OLD controller,
    // then the NEW RSS module drives a real request on the candidate channel.
    server.set(COMMITS_TARGET_B, Route::ok(COMMITS.as_bytes().to_vec()));
    let before_run2 = server.requests().len();
    let offset = log_of(&install).len();
    let child = ChildRun::spawn(
        &install,
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "check"],
    );
    assert!(
        wait_log(&install, "files installed"),
        "the candidate was not applied: {}",
        log_of(&install)
    );
    assert!(
        m6::wait_for(Duration::from_secs(30), || {
            server
                .requests()
                .iter()
                .any(|request| request.target == COMMITS_TARGET_B)
        }),
        "the new RSS never ran: {}",
        log_of(&install)
    );
    assert_eq!(child.stop(), 0);
    let log_after = log_of(&install)[offset..].to_string();
    let live_after = std::fs::read(install.join("rss/updater/github.rss")).expect("live after");
    let session_after = std::fs::read(install.join("h.json")).expect("session after run 2");
    let requests = server.requests();
    let run2: Vec<&str> = requests[before_run2..]
        .iter()
        .map(|request| request.target.as_str())
        .collect();
    assert_eq!(
        run2,
        vec![COMMITS_TARGET_B],
        "the applied candidate's RSS must drive exactly one new request"
    );
    let wire_after = requests.last().map(|r| r.target.clone());
    serde_json::json!({
        "pending_before": pending_before,
        "pending_after": pending_path(&install).exists(),
        "applied_id": b6_state_active(&install),
        "live_changed": live_before != live_after,
        "session_equal": session == session_after,
        "module_origin_kind": "source_tree",
        "module_version": b6_started_version(&log_after),
        "stop": serde_json::Value::Null,
        "error_class": serde_json::Value::Null,
        "wire_after_apply": wire_after,
    })
}

fn b6_apply_reject(root: &Path, server: &Server) -> serde_json::Value {
    let key = signing_key();
    let pem = public_pem(&key);
    let install = b6_fresh(root, "apply-reject");
    let origin = server.origin();
    publish_rss(&install, &origin, Some(&pem), "rss");
    let exe = b6_owned_exe(&install);
    let expected_old_version = crate::r#const::version_label();

    // Run 0: an ordinary start creates the real session file this case snapshots.
    let child = ChildRun::spawn(
        &install,
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(&install, "started."),
        "plain start did not run: {}",
        log_of(&install)
    );
    assert_eq!(child.stop(), 0);
    // The baseline old identity comes from the SAME real binary's ordinary
    // start, not from an assumption.
    let baseline_version = b6_started_version(&log_of(&install)).expect("baseline start identity");
    assert_eq!(
        baseline_version, expected_old_version,
        "plain start reported an unexpected identity"
    );
    let session = std::fs::read(install.join("h.json")).expect("b6 session");
    let live_before = std::fs::read(install.join("rss/updater/github.rss")).expect("live before");
    let applied_before = b6_state_active(&install);

    // Seed a real signed candidate whose RSS module cannot compile.
    let pkg = b6_candidate_package(&origin, &pem, true);
    seed_pending(
        &install,
        pkg.info.clone(),
        pkg.sig.clone(),
        pkg.payload.clone(),
    );
    let offset = log_of(&install).len();
    let mut child = ChildRun::spawn(
        &install,
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(&install, "Failure when updating program"),
        "the broken candidate was not rejected: {}",
        log_of(&install)
    );
    // A preparation rejection must NOT stop the ordinary start: the SAME
    // still-resident process must go on to start the OLD business App.
    assert!(
        b6_wait_log_after(&install, offset, "started."),
        "the old release did not continue into the App after the rejection: {}",
        log_of(&install)
    );
    assert!(child.running(), "the rejected start was not resident");
    let rejected_log = log_of(&install)[offset..].to_string();
    assert!(
        !rejected_log.contains("files installed"),
        "a rejected candidate was applied: {rejected_log}"
    );
    // Capture the ACTUAL executed typed rejection: class and detail come from
    // the real process's rendered error, never from the scenario label.
    let detail =
        b6_rejection_detail(&rejected_log).expect("no executed rejection detail in the old log");
    let error_class = b6_error_class(&detail).expect("unknown executed error class");
    assert_eq!(
        error_class, "Install",
        "the rejection was not a preparation (install) rejection: {detail}"
    );
    assert!(
        detail.contains("compile error") && detail.contains("github.rss"),
        "the intended broken candidate error was not observed: {detail}"
    );
    assert!(
        detail.contains("preflight updater"),
        "the rejection did not come from the candidate preflight: {detail}"
    );
    // A harness/argument failure (the test binary refusing to start the
    // preflight) is NOT the intended candidate rejection.
    for harness in [
        "cannot run the candidate preflight",
        "preflight configuration",
        "preflight app",
    ] {
        assert!(
            !detail.contains(harness),
            "a preflight argument/harness failure was accepted as the candidate error: {detail}"
        );
    }
    let old_version = b6_started_version(&rejected_log);
    assert_eq!(
        old_version.as_deref(),
        Some(baseline_version.as_str()),
        "the old release identity changed after the rejection"
    );
    assert_eq!(child.stop(), 0);
    let live_after = std::fs::read(install.join("rss/updater/github.rss")).expect("live after");
    let session_after = std::fs::read(install.join("h.json")).expect("session after");
    let applied_after = b6_state_active(&install);
    assert_eq!(
        applied_after, applied_before,
        "the applied identity changed after a preparation rejection"
    );
    serde_json::json!({
        "pending_before": true,
        "pending_after": pending_path(&install).exists(),
        "applied_id": applied_after,
        "live_changed": live_before != live_after,
        "session_equal": session == session_after,
        "module_origin_kind": "source_tree",
        "module_version": old_version,
        "stop": serde_json::Value::Null,
        "error_class": error_class,
        "wire_after_apply": serde_json::Value::Null,
    })
}

fn b6_cli_off(root: &Path, server: &Server) -> serde_json::Value {
    let key = signing_key();
    let pem = public_pem(&key);
    let install = b6_fresh(root, "cli-off");
    let origin = server.origin();
    publish_rss(&install, &origin, Some(&pem), "rss");
    let exe = b6_owned_exe(&install);
    let before = server.requests().len();
    let child = ChildRun::spawn(
        &install,
        &exe,
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(&install, "started."),
        "the off run never started: {}",
        log_of(&install)
    );
    assert_eq!(child.stop(), 0);
    let requests = server.requests();
    let during: Vec<Request> = requests[before..].to_vec();
    assert!(
        during.is_empty(),
        "the CLI off gate performed HTTP: {during:?}"
    );
    let log = log_of(&install);
    assert!(
        !log.contains("Update channel"),
        "the CLI off gate loaded the update module: {log}"
    );
    serde_json::json!({
        "feed": b6_feed(&during),
        "decision": {
            "mode": "off",
            "applied": serde_json::Value::Null,
            "outcome": "no_vm_http",
            "events": [],
            "pending_before": false,
            "pending_after": false,
            "pending_equal": true,
        },
    })
}

pub(super) fn compat_update(inputs: &serde_json::Value) -> serde_json::Value {
    let root = PathBuf::from(inputs["root"].as_str().expect("b6 probe root"));
    std::fs::create_dir_all(&root).expect("b6 probe root");
    let bodies = b6_package_bodies(inputs);
    // One plain build per probe; every apply case copies this binary.
    cargo_build(None);
    let key = signing_key();
    let pem = public_pem(&key);
    let mut cases = serde_json::Map::new();

    // ---- channel / commit: real checks against one owned source ----
    let stable_install = b6_fresh(&root, "stable");
    let server = Server::start(vec![]);
    let origin = server.origin();
    b6_publish_binding(&stable_install, &origin, &pem, "rss", None);
    server.set(COMMITS_TARGET, Route::ok(COMMITS.as_bytes().to_vec()));
    server.set(COMMITS_TARGET_B, Route::ok(COMMITS.as_bytes().to_vec()));

    eprintln!("b6 probe: channel.stable");
    let stable_report = b6_check(&stable_install, "check", false, None);
    assert_eq!(
        stable_report.outcome,
        "available",
        "stable check: {:?}",
        event_keys(&stable_report)
    );
    let stable_requests = server.requests();
    let stable_info = stable_report.update.clone().expect("stable update info");
    let stable_commits = format!("{origin}{COMMITS_TARGET}");
    cases.insert(
        "channel.stable".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {
                "feed": b6_feed(&stable_requests),
                "source": b6_source(Some(&stable_info), &stable_commits),
                "commit": b6_commit(&stable_info),
            },
        }),
    );
    cases.insert(
        "commit.first".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {
                "feed": b6_feed(&stable_requests),
                "source": b6_source(Some(&stable_info), &stable_commits),
                "commit": b6_commit(&stable_info),
            },
        }),
    );

    // A beta binding selects its own channel; without one the check fails
    // closed BEFORE any request (registered native delta).
    let beta_install = b6_fresh(&root, "beta");
    b6_publish_binding(&beta_install, &origin, &pem, "rss", Some("rss-b"));
    let before_beta = server.requests().len();
    let beta_report = b6_check(&beta_install, "check", true, None);
    assert_eq!(beta_report.outcome, "available");
    let beta_requests: Vec<Request> = server.requests()[before_beta..].to_vec();
    let beta_info = beta_report.update.clone().expect("beta update info");
    let beta_commits = format!("{origin}{COMMITS_TARGET_B}");
    cases.insert(
        "channel.beta".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {
                "feed": b6_feed(&beta_requests),
                "source": b6_source(Some(&beta_info), &beta_commits),
                "commit": b6_commit(&beta_info),
            },
        }),
    );

    let missing_install = b6_fresh(&root, "beta-missing");
    b6_publish_binding(&missing_install, &origin, &pem, "rss", None);
    let before_missing = server.requests().len();
    eprintln!("b6 probe: channel.beta-missing");
    let missing_report = b6_check(&missing_install, "check", true, None);
    assert_eq!(
        missing_report.outcome, "failed",
        "an unbound beta channel must fail closed"
    );
    let missing_requests: Vec<Request> = server.requests()[before_missing..].to_vec();
    assert!(
        missing_requests.is_empty(),
        "an unbound beta channel still made a request: {missing_requests:?}"
    );
    cases.insert(
        "channel.beta-missing".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {
                "feed": b6_feed(&missing_requests),
                "source": b6_source(None, ""),
                "commit": b6_absent_commit(),
            },
        }),
    );

    // ---- decision: off / same / download with a previous pending present ----
    let off_install = b6_fresh(&root, "off");
    b6_publish_binding(&off_install, &origin, &pem, "rss", None);
    b6_seed_sentinel(&off_install);
    let before_off = b6_pending_bytes(&off_install);
    let before_off_requests = server.requests().len();
    eprintln!("b6 probe: decision.off");
    let off_report = b6_check(&off_install, "off", false, None);
    assert_eq!(off_report.outcome, "available", "direct off check");
    let off_requests: Vec<Request> = server.requests()[before_off_requests..].to_vec();
    assert_eq!(off_requests.len(), 1, "a direct off check queries once");
    let after_off = b6_pending_bytes(&off_install);
    assert!(before_off.is_some(), "the previous pending was not present");
    cases.insert(
        "decision.off".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {
                "feed": b6_feed(&off_requests),
                "decision": b6_decision(
                    "off", None, &off_report.outcome, Some(&off_report),
                    before_off.as_deref(), after_off.as_deref()),
            },
        }),
    );

    let same_install = b6_fresh(&root, "same");
    b6_publish_binding(&same_install, &origin, &pem, "rss", None);
    b6_seed_sentinel(&same_install);
    let before_same = b6_pending_bytes(&same_install);
    let before_same_requests = server.requests().len();
    eprintln!("b6 probe: decision.same");
    let same_report = b6_check(&same_install, "check", false, Some(UPDATE_ID));
    assert_eq!(same_report.outcome, "up_to_date");
    let same_requests: Vec<Request> = server.requests()[before_same_requests..].to_vec();
    let after_same = b6_pending_bytes(&same_install);
    cases.insert(
        "decision.same".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {
                "feed": b6_feed(&same_requests),
                "decision": b6_decision(
                    "check", Some(UPDATE_ID), &same_report.outcome,
                    Some(&same_report), before_same.as_deref(),
                    after_same.as_deref()),
            },
        }),
    );

    let download_install = b6_fresh(&root, "download");
    b6_publish_binding(&download_install, &origin, &pem, "rss", None);
    b6_seed_sentinel(&download_install);
    let before_download = b6_pending_bytes(&download_install);
    let pkg = sign(
        &key,
        &Spec::new(UPDATE_ID).with_tree(&format!("xeHentai-{UPDATE_ID}/"), &bodies),
    );
    serve_package(&server, &origin, &pkg);
    let before_download_requests = server.requests().len();
    eprintln!("b6 probe: decision.download");
    let download_report = b6_check(&download_install, "download", false, None);
    assert_eq!(
        download_report.outcome,
        "pending",
        "download: {:?}",
        event_keys(&download_report)
    );
    let download_requests: Vec<Request> = server.requests()[before_download_requests..].to_vec();
    let after_download = b6_pending_bytes(&download_install);
    let download_info = download_report.update.clone().expect("download info");
    cases.insert(
        "decision.download".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {
                "feed": b6_feed(&download_requests),
                "decision": b6_decision(
                    "download", None, &download_report.outcome,
                    Some(&download_report), before_download.as_deref(),
                    after_download.as_deref()),
                "package": b6_pending_observation(&download_install),
                "source": b6_source(
                    Some(&download_info),
                    &format!("{origin}{COMMITS_TARGET}")),
            },
        }),
    );

    // ---- package: the envelope members plus the real selection result ----
    let package_install = b6_fresh(&root, "package");
    let foreign_members = [
        format!("xeHentai-{UPDATE_ID}/foreign.txt"),
        "other/x.txt".to_string(),
    ];
    let foreign_refs: Vec<&str> = foreign_members.iter().map(String::as_str).collect();
    eprintln!("b6 probe: package.envelope");
    cases.insert(
        "package.envelope".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {
                "package": b6_package_probe(&bodies, &foreign_refs, &package_install),
                "source": b6_source(Some(&stable_info), &stable_commits),
            },
        }),
    );

    // ---- apply: real startup apply / real rejection ----
    eprintln!("b6 probe: package.reject; apply.valid");
    cases.insert(
        "package.reject".to_string(),
        b6_package_reject(&root, &server, &bodies),
    );
    cases.insert(
        "apply.valid".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {"apply": b6_apply_valid(&root, &server)},
        }),
    );
    eprintln!("b6 probe: apply.reject");
    cases.insert(
        "apply.reject".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": {"apply": b6_apply_reject(&root, &server)},
        }),
    );

    // ---- CLI off: the gate must not load the module or touch HTTP ----
    eprintln!("b6 probe: cli.off");
    cases.insert(
        "cli.off".to_string(),
        serde_json::json!({
            "side": "native",
            "sections": b6_cli_off(&root, &server),
        }),
    );

    // The comparator reads only side+sections; evidence stays raw side data.
    for value in cases.values_mut() {
        if let Some(object) = value.as_object_mut() {
            object.insert("evidence".to_string(), serde_json::json!({}));
        }
    }

    serde_json::json!({
        "suite": "update",
        "origin": origin,
        "platform": {
            "app_version": crate::r#const::VERSION_DISPLAY,
            "target": crate::r#const::TARGET,
            "host_abi": crate::r#const::HOST_ABI,
            "script_abi": crate::r#const::SCRIPT_ABI,
        },
        "cases": cases,
    })
}
