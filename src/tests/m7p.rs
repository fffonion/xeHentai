//! M7 remaining scope: the signed native package, the atomic pending envelope,
//! next-start activation (resource-only and native A/B) and rollback.
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

// MARK: - next start: resource-only activation

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
        .args(["build", "--jobs", "2"])
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
        !log.contains("was applied"),
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
        wait_log(install.path(), "was applied"),
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
    assert!(!log.contains("was applied"), "{log}");
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
        wait_log(install.path(), "was applied"),
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
    assert!(wait_log(install.path(), "was applied"));
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

#[test]
fn m7p_interrupted_activation_rolls_back_to_the_previous_release() {
    let pem = public_pem(&signing_key());
    let install = install_dir();
    // The live tree is the candidate release, the backup tree the previous one.
    publish_rss(install.path(), "http://127.0.0.1:9", Some(&pem), "rss-c");
    let previous_binding = {
        let scratch = tempfile::tempdir().expect("scratch");
        publish_rss(scratch.path(), "http://127.0.0.1:9", Some(&pem), "rss");
        std::fs::read(scratch.path().join("rss/updater/github.rss")).expect("previous binding")
    };
    let backup = install.path().join(AREA).join("backup");
    std::fs::create_dir_all(backup.join("rss/updater")).expect("backup dir");
    std::fs::write(backup.join("rss/updater/github.rss"), &previous_binding).expect("backup");
    let binary = owned_exe(install.path());
    std::fs::copy(&binary, backup.join("previous-exe")).expect("previous executable");
    std::fs::create_dir_all(install.path().join("home")).unwrap();
    std::fs::create_dir_all(install.path().join("tmp")).unwrap();
    std::fs::create_dir_all(install.path().join("cache")).unwrap();
    // The commit point of an activation whose replaced image never started.
    std::fs::write(
        install.path().join(AREA).join("state.json"),
        serde_json::to_vec(&serde_json::json!({
            "v": 1,
            "active": {
                "update_id": "cccccccccccccccccccccccccccccccccccccccc",
                "build_id": "c-native",
                "native": true,
                "applied_at": "2026-11-03T00:00:00+08:00"
            },
            "previous": {
                "update_id": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                "build_id": crate::r#const::BUILD_ID,
                "native": true,
                "applied_at": "2026-11-02T00:00:00+08:00"
            }
        }))
        .expect("state"),
    )
    .expect("state file");
    std::fs::write(install.path().join(AREA).join("confirm"), b"pending\n").expect("confirm");

    let child = ChildRun::spawn(
        install.path(),
        &install.path().join("xeHentai"),
        &["-l", "eh.log", "-vv", "--auto-update", "off"],
    );
    assert!(
        wait_log(
            install.path(),
            "rolled back to bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
        ),
        "the interrupted activation was not rolled back: {}",
        log_of(install.path())
    );
    assert!(
        wait_log(install.path(), "2.023-rss started."),
        "the previous release did not start: {}",
        log_of(install.path())
    );
    assert_eq!(child.stop(), 0);

    assert_eq!(
        std::fs::read(install.path().join("rss/updater/github.rss")).expect("live binding"),
        previous_binding,
        "the previous release files are restored"
    );
    assert!(
        !install.path().join(AREA).join("confirm").exists(),
        "the activation marker is retired"
    );
    let state: serde_json::Value = serde_json::from_slice(
        &std::fs::read(install.path().join(AREA).join("state.json")).expect("state"),
    )
    .expect("state json");
    assert_eq!(
        state["active"]["update_id"],
        "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    );
    assert!(state["previous"].is_null());
    let log = log_of(install.path());
    assert!(!log.contains("was applied"), "{log}");
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
    assert!(log.contains("was applied"), "{log}");
    assert!(log.contains("is running"), "the new image confirms: {log}");
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
    assert!(!log.contains("was applied"), "{log}");
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
