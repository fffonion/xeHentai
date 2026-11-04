//! M8: the updater's own native HTTP transport.
//!
//! Every fixture here is a real loopback listener: no mock transport, no
//! external network. The trusted source binding is patched to the fixture's
//! API and package origins, exactly as the production binding is the two
//! GitHub origins.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::m6;
use super::m7::{COMMITS, Request, UPDATE_ID};

const MANIFEST: &str = env!("CARGO_MANIFEST_DIR");

// MARK: - one real loopback source

#[derive(Clone)]
pub(super) struct Route {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Arc<Vec<u8>>,
    /// Send this many body bytes, then hold the rest until `release()`.
    pub hold_after: Option<usize>,
}

impl Route {
    pub fn ok(body: Vec<u8>) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: Arc::new(body),
            hold_after: None,
        }
    }

    pub fn json(body: &str) -> Self {
        Self::ok(body.as_bytes().to_vec())
    }

    pub fn status(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![("Content-Type".into(), "application/octet-stream".into())],
            body: Arc::new(body),
            hold_after: None,
        }
    }

    pub fn redirect(location: &str) -> Self {
        Self {
            status: 302,
            headers: vec![("Location".into(), location.to_string())],
            body: Arc::new(Vec::new()),
            hold_after: None,
        }
    }

    pub fn holding(body: Vec<u8>, after: usize) -> Self {
        Self {
            status: 200,
            headers: vec![("Content-Type".into(), "application/json".into())],
            body: Arc::new(body),
            hold_after: Some(after),
        }
    }
}

/// A real HTTP source bound to one explicit loopback address. It records every
/// request head, signals arrival, and for a holding route the bytes really
/// written plus whether the client closed the connection.
pub(super) struct Source {
    pub ip: IpAddr,
    pub port: u16,
    pub label: &'static str,
    routes: Arc<Mutex<HashMap<String, Route>>>,
    requests: Arc<Mutex<Vec<Request>>>,
    arrived: Arc<(Mutex<usize>, Condvar)>,
    release: Arc<(Mutex<bool>, Condvar)>,
    partial: Arc<Mutex<HashMap<String, usize>>>,
    eof: Arc<Mutex<Vec<String>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Source {
    pub fn start(label: &'static str, ip: &str, routes: Vec<(&str, Route)>) -> Self {
        let addr: IpAddr = ip.parse().expect("fixture ip");
        let listener = TcpListener::bind(SocketAddr::new(addr, 0)).expect("bind fixture");
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        let routes: HashMap<String, Route> = routes
            .into_iter()
            .map(|(path, route)| (path.to_string(), route))
            .collect();
        let routes = Arc::new(Mutex::new(routes));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let arrived = Arc::new((Mutex::new(0usize), Condvar::new()));
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let partial = Arc::new(Mutex::new(HashMap::new()));
        let eof = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let seen = Arc::clone(&requests);
        let table = Arc::clone(&routes);
        let gate = Arc::clone(&release);
        let flushed_table = Arc::clone(&partial);
        let closed = Arc::clone(&eof);
        let stopping = Arc::clone(&stop);
        let arrival = Arc::clone(&arrived);
        let handle = std::thread::spawn(move || {
            while !stopping.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((socket, _)) => {
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
                        let flushed = Arc::clone(&flushed_table);
                        let closed = Arc::clone(&closed);
                        let stopping = Arc::clone(&stopping);
                        let arrival = Arc::clone(&arrival);
                        std::thread::spawn(move || {
                            serve(
                                socket, &table, &seen, &arrival, &gate, &flushed, &closed,
                                &stopping,
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
            ip: addr,
            port,
            label,
            routes,
            requests,
            arrived,
            release,
            partial,
            eof,
            stop,
            handle: Some(handle),
        }
    }

    pub fn origin(&self) -> String {
        match self.ip {
            IpAddr::V4(ip) => format!("http://{ip}:{}", self.port),
            IpAddr::V6(ip) => format!("http://[{ip}]:{}", self.port),
        }
    }

    pub fn set(&self, path: &str, route: Route) {
        self.routes.lock().unwrap().insert(path.to_string(), route);
    }

    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }

    pub fn partial(&self, path: &str) -> usize {
        *self.partial.lock().unwrap().get(path).unwrap_or(&0)
    }

    pub fn peer_eof(&self, path: &str) -> bool {
        self.eof.lock().unwrap().iter().any(|seen| seen == path)
    }

    /// Block until at least `count` requests have been fully read. This is the
    /// explicit test barrier: no sleep, no polling of the consumer.
    pub fn wait_arrived(&self, count: usize) {
        let (lock, cond) = &*self.arrived;
        let mut guard = lock.lock().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while *guard < count {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                panic!("{}: request barrier timed out", self.label);
            }
            let (next, _) = cond.wait_timeout(guard, left).unwrap();
            guard = next;
        }
    }

    pub fn release(&self) {
        let (lock, cond) = &*self.release;
        *lock.lock().unwrap() = true;
        cond.notify_all();
    }
}

impl Drop for Source {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        self.release();
        let _ = TcpStream::connect(SocketAddr::new(self.ip, self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn serve(
    mut socket: TcpStream,
    routes: &Arc<Mutex<HashMap<String, Route>>>,
    seen: &Arc<Mutex<Vec<Request>>>,
    arrived: &Arc<(Mutex<usize>, Condvar)>,
    gate: &Arc<(Mutex<bool>, Condvar)>,
    partial: &Arc<Mutex<HashMap<String, usize>>>,
    eof: &Arc<Mutex<Vec<String>>>,
    stop: &Arc<AtomicBool>,
) {
    let Some(target) = read_head(&mut socket, seen) else {
        return;
    };
    let route = routes.lock().unwrap().get(&target).cloned();
    let Some(route) = route else {
        let _ = socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        drain_until_eof(&mut socket, &target, eof, stop);
        return;
    };
    let mut head = format!(
        "HTTP/1.1 {} Status\r\nContent-Length: {}\r\nConnection: close\r\n",
        route.status,
        route.body.len()
    );
    for (name, value) in &route.headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    let split = route.hold_after.map(|after| after.min(route.body.len()));
    if let Some(split) = split {
        partial.lock().unwrap().insert(target.clone(), split);
    }
    let _ = socket.write_all(head.as_bytes());
    let _ = socket.write_all(&route.body[..split.unwrap_or(route.body.len())]);
    let _ = socket.flush();
    // Announce arrival only after the head (and any held prefix) is on the
    // wire, so a barrier on `arrived` also guarantees the hold is in place.
    let (lock, cond) = &**arrived;
    *lock.lock().unwrap() += 1;
    cond.notify_all();
    if let Some(split) = split {
        let (lock, cond) = &**gate;
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            if *lock.lock().unwrap() {
                let _ = socket.write_all(&route.body[split..]);
                let _ = socket.flush();
                break;
            }
            let mut chunk = [0u8; 256];
            match socket.read(&mut chunk) {
                Ok(0) => {
                    eof.lock().unwrap().push(target.clone());
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
    drain_until_eof(&mut socket, &target, eof, stop);
}

/// Observe how the client ends the connection after a complete response. A
/// clean FIN is recorded; a deadline or a fixture stop is not.
fn drain_until_eof(
    socket: &mut TcpStream,
    target: &str,
    eof: &Arc<Mutex<Vec<String>>>,
    stop: &Arc<AtomicBool>,
) {
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let mut chunk = [0u8; 256];
        match socket.read(&mut chunk) {
            Ok(0) => {
                eof.lock().unwrap().push(target.to_string());
                return;
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                return;
            }
            Err(_) => return,
        }
    }
}

fn read_head(socket: &mut TcpStream, seen: &Arc<Mutex<Vec<Request>>>) -> Option<String> {
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
    seen.lock().unwrap().push(Request {
        method,
        target: target.clone(),
        headers,
    });
    Some(target)
}

// MARK: - trusted binding fixture

/// Publish the production resource tree and bind ONLY the two trusted origin
/// constants (API and package) to the fixture origins.
pub(super) fn publish_binding(root: &Path, api: &str, package: &str) {
    publish_binding_with(root, api, package, None);
}

pub(super) fn publish_binding_with(root: &Path, api: &str, package: &str, pem: Option<&str>) {
    let rss = root.join("rss");
    if !rss.is_dir() {
        m6::copy_tree(&Path::new(MANIFEST).join("rss"), &rss);
    }
    let path = rss.join("updater/github.rss");
    let source = std::fs::read_to_string(&path).expect("github.rss");
    let mut source = source
        .replace(
            "api_origin: \"https://api.github.com\",",
            &format!("api_origin: \"{api}\","),
        )
        .replace(
            "package_download_origin: \"https://github.com\",",
            &format!("package_download_origin: \"{package}\","),
        );
    if let Some(pem) = pem {
        let pem_literal = pem.replace('\n', "\\n");
        source = source.replace(
            "verification_key: null,",
            &format!("verification_key: \"{pem_literal}\","),
        );
    }
    std::fs::write(&path, source).expect("write binding");
}

pub(super) const COMMITS_TARGET: &str = "/repos/fffonion/xeHentai/commits?sha=rss";

// MARK: - runtime helpers

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
}

fn run_local<F: std::future::Future>(future: F) -> F::Output {
    let runtime = runtime();
    let local = tokio::task::LocalSet::new();
    runtime.block_on(local.run_until(future))
}

/// Drive the real public consumer: load the trusted modules and run one check
/// on the current-thread runtime with the updater owner's LocalSet.
pub(super) fn run_check(
    root: &Path,
    mode: &str,
) -> Result<crate::updater::CheckReport, crate::updater::UpdateError> {
    let mode = mode.to_string();
    let root = root.to_path_buf();
    run_local(async move {
        let mut updater = crate::updater::Updater::load(&root).expect("load updater");
        updater.check(&mode, false, None, "xeHentai/test").await
    })
}

fn seam_session(
    resolver: Arc<dyn crate::proxy::Resolver>,
    control_limit: usize,
) -> crate::worker::Session {
    use crate::worker::{FallbackMap, Session, SessionConfig, TestClock};
    Session::new(
        vec![("User-Agent".into(), Some("xeHentai/test".into()))],
        None,
        crate::util::ProxyPolicy::All,
        SessionConfig {
            retry: 1,
            timeout: Duration::from_secs(5),
            control_limit,
            fallback: FallbackMap::empty(),
            resolver,
            clock: Arc::new(TestClock::new(0.0)),
            ..SessionConfig::default()
        },
    )
}

fn map_resolver(entries: Vec<(&str, Vec<&str>)>) -> Arc<crate::proxy::MapResolver> {
    Arc::new(crate::proxy::MapResolver::new(
        entries
            .into_iter()
            .map(|(host, addresses)| {
                (
                    host.to_string(),
                    addresses
                        .into_iter()
                        .map(|address| SocketAddr::new(address.parse().unwrap(), 0))
                        .collect(),
                )
            })
            .collect(),
    ))
}

// MARK: - policy RED: the source is two complete origin tuples

/// The trusted source is TWO complete origin tuples. A redirect that recombines
/// the API's host with the package's port is in neither tuple, so it must be
/// refused BEFORE the forbidden listener is contacted. The old split
/// scheme/host/port allowlist admitted exactly that combination.
#[test]
fn m8_cross_origin_tuple_redirect_is_refused_before_connect() {
    let root = tempfile::tempdir().expect("root");
    // The forbidden cross tuple: the API host with a package port. It answers
    // with a completely valid commits document, so a request that reaches it
    // would let the check succeed and never mask the policy failure.
    let forbidden = Source::start(
        "forbidden",
        "127.0.0.1",
        vec![(COMMITS_TARGET, Route::json(COMMITS))],
    );
    // The package origin owns the same port on the OTHER loopback address, so
    // its host and port are each individually allowlisted but the pair is not.
    let package_origin = format!("http://127.0.0.2:{}", forbidden.port);
    let api = Source::start(
        "api",
        "127.0.0.1",
        vec![(
            COMMITS_TARGET,
            Route::redirect(&format!(
                "http://127.0.0.1:{}{COMMITS_TARGET}",
                forbidden.port
            )),
        )],
    );
    publish_binding(root.path(), &api.origin(), &package_origin);

    let report = run_check(root.path(), "check");

    let reached = forbidden.requests();
    assert!(
        reached.is_empty(),
        "the updater contacted an address that is in neither trusted origin \
         tuple: {reached:?}"
    );
    assert!(
        report.is_err(),
        "a redirect outside both trusted origin tuples must be refused, got {:?}",
        report.map(|report| report.outcome)
    );
    assert_eq!(api.requests().len(), 1, "exactly one API request");
}

// MARK: - the real public consumer wakes and closes

/// A real `Updater::load` + public `check("check")` request whose response is
/// withheld behind an explicit barrier. Only `release()` may complete it, and
/// the completion is the runtime's own wake: the test never polls the VM and
/// never sleeps on the schedule.
#[test]
fn m8_check_barrier_completes_on_the_runtime_wake_and_closes() {
    let root = tempfile::tempdir().expect("root");
    let body = COMMITS.as_bytes().to_vec();
    let split = body.len() / 2;
    let source = Arc::new(Source::start(
        "api",
        "127.0.0.1",
        vec![(COMMITS_TARGET, Route::holding(body, split))],
    ));
    publish_binding(root.path(), &source.origin(), &source.origin());
    let root_path = root.path().to_path_buf();
    let barrier = Arc::clone(&source);

    let report = run_local(async move {
        let check = tokio::task::spawn_local(async move {
            let mut updater =
                crate::updater::Updater::load(&root_path).expect("load the trusted modules");
            updater
                .check("check", false, None, "xeHentai/test")
                .await
                .expect("the check must complete")
        });
        // Explicit barrier: block until the fixture has the whole request.
        let waiter = Arc::clone(&barrier);
        tokio::task::spawn_blocking(move || waiter.wait_arrived(1))
            .await
            .expect("barrier");
        assert!(
            barrier.partial(COMMITS_TARGET) > 0,
            "the response body must still be incomplete at the barrier"
        );
        barrier.release();
        check.await.expect("the check task must finish")
    });

    assert_eq!(report.outcome, "available");
    assert_eq!(
        report.update.as_ref().map(|info| info.update_id.as_str()),
        Some(UPDATE_ID)
    );
    let requests = source.requests();
    assert_eq!(requests.len(), 1, "exactly one request: {requests:?}");
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].target, COMMITS_TARGET);
    assert_eq!(requests[0].header("cookie"), None);
    assert_eq!(requests[0].header("authorization"), None);
    assert!(
        m6::wait_for(Duration::from_secs(10), || source.peer_eof(COMMITS_TARGET)),
        "the peer must see the client close the connection after the response"
    );
}

// MARK: - the worker seam is one plain hop

#[test]
fn m8_seam_single_hop_preserves_status_and_binary_body() {
    let bytes = vec![0xff, 0x00, 0x80, b'{', b'\r', b'\n'];
    let source = Source::start(
        "api",
        "127.0.0.1",
        vec![("/probe", Route::status(503, bytes.clone()))],
    );
    let url = format!("{}/probe", source.origin());
    let mut session = seam_session(map_resolver(vec![]), crate::worker::CONTROL_BODY_LIMIT);
    let response = match run_local(async move { session.get_once(&url).await }) {
        Ok(response) => response,
        Err(error) => panic!("one hop failed: {error:?}"),
    };
    assert_eq!(response.status, 503);
    assert_eq!(response.body, bytes, "the raw body bytes are preserved");
    assert_eq!(source.requests().len(), 1);
}

#[test]
fn m8_seam_single_hop_does_not_follow_redirect_or_carry_cookies() {
    let target = Source::start("api", "127.0.0.1", vec![("/after", Route::json("{}"))]);
    let after = format!("{}/after", target.origin());
    let source = Source::start(
        "api",
        "127.0.0.1",
        vec![("/probe", Route::redirect(&after))],
    );
    let url = format!("{}/probe", source.origin());
    let mut session = seam_session(map_resolver(vec![]), crate::worker::CONTROL_BODY_LIMIT);
    let response = match run_local(async move { session.get_once(&url).await }) {
        Ok(response) => response,
        Err(error) => panic!("one hop failed: {error:?}"),
    };
    assert_eq!(response.status, 302, "the redirect is returned as-is");
    assert!(response.body.is_empty());
    assert!(
        target.requests().is_empty(),
        "the seam must never visit the redirect target"
    );
    let requests = source.requests();
    assert_eq!(requests.len(), 1, "no duplicate request: {requests:?}");
    assert_eq!(requests[0].header("cookie"), None);
    assert_eq!(requests[0].header("authorization"), None);
}

#[test]
fn m8_seam_body_limit_reports_body_too_large() {
    let body = vec![b'x'; 64];
    let source = Source::start(
        "api",
        "127.0.0.1",
        vec![("/probe", Route::status(200, body))],
    );
    let url = format!("{}/probe", source.origin());
    let mut session = seam_session(map_resolver(vec![]), 8);
    let error = match run_local(async move { session.get_once(&url).await }) {
        Ok(_) => panic!("the body exceeds the configured bound"),
        Err(error) => error,
    };
    assert!(
        matches!(error, crate::worker::HttpError::BodyTooLarge { limit: 8 }),
        "expected BodyTooLarge, got {error:?}"
    );
}

// MARK: - transport policy

fn transport(
    api: &str,
    package: &str,
) -> Result<crate::updater::http::Transport, crate::updater::UpdateError> {
    crate::updater::http::Transport::for_test(
        api,
        package,
        crate::updater::http::body_limit(),
        map_resolver(vec![]),
    )
}

#[test]
fn m8_transport_reaches_both_trusted_tuples_and_refuses_recombined_ones() {
    let package = Source::start(
        "package",
        "127.0.0.2",
        vec![("/pkg", Route::status(200, b"pkg".to_vec()))],
    );
    // A listener that owns the API host with the package port, i.e. a
    // combination that is in neither origin tuple.
    let forbidden = Source::start(
        "forbidden",
        "127.0.0.1",
        vec![("/cross", Route::json("{}"))],
    );
    let wrong_host = Source::start(
        "wrong-host",
        "127.0.0.3",
        vec![("/elsewhere", Route::json("{}"))],
    );
    let api = Source::start(
        "api",
        "127.0.0.1",
        vec![
            ("/api", Route::status(200, b"api".to_vec())),
            (
                "/to-cross",
                Route::redirect(&format!("http://127.0.0.1:{}/cross", forbidden.port)),
            ),
            (
                "/to-host",
                Route::redirect(&format!("{}/elsewhere", wrong_host.origin())),
            ),
        ],
    );
    // A matching host and port with the wrong scheme is still not a tuple.
    api.set(
        "/to-scheme",
        Route::redirect(&format!("https://127.0.0.1:{}/api", api.port)),
    );
    let transport = match transport(&api.origin(), &format!("http://127.0.0.2:{}", package.port)) {
        Ok(transport) => transport,
        Err(error) => panic!("transport: {error}"),
    };

    let api_url = format!("{}/api", api.origin());
    let package_url = format!("http://127.0.0.2:{}/pkg", package.port);
    let (api_response, package_response) = run_local(async {
        let first = transport.get(&api_url, &[]).await.expect("api origin");
        let second = transport
            .get(&package_url, &[])
            .await
            .expect("package origin");
        (first, second)
    });
    assert_eq!(api_response.status, 200);
    assert_eq!(api_response.body, b"api");
    assert_eq!(package_response.body, b"pkg");

    for path in ["/to-cross", "/to-host", "/to-scheme"] {
        let url = format!("{}{path}", api.origin());
        let error = run_local(async { transport.get(&url, &[]).await })
            .expect_err("a redirect outside the two tuples must be refused");
        assert!(
            format!("{error}").contains("trusted"),
            "the refusal must name the trusted origin policy: {error}"
        );
    }
    for source in [&forbidden, &wrong_host, &package] {
        let requests = source.requests();
        let offending: Vec<_> = requests
            .iter()
            .filter(|request| request.target != "/pkg")
            .collect();
        assert!(
            offending.is_empty(),
            "{} was contacted through a recombined origin: {requests:?}",
            source.label
        );
    }
}

/// A loopback IP binding may be an IPv6 literal: the origin tuple is compared
/// through `url::Host`, never through a hand-written URL string, and the same
/// single-hop request must still go out.
#[test]
fn m8_transport_reaches_an_ipv6_loopback_tuple() {
    let source = Source::start("api", "::1", vec![(COMMITS_TARGET, Route::json(COMMITS))]);
    let transport = transport(&source.origin(), &source.origin()).expect("transport");
    let url = format!("{}{COMMITS_TARGET}", source.origin());
    let response = run_local(async { transport.get(&url, &[]).await }).expect("ipv6 origin");
    assert_eq!(response.status, 200);
    let requests = source.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].target, COMMITS_TARGET);
    assert_eq!(
        requests[0].header("host"),
        Some(format!("[::1]:{}", source.port).as_str())
    );
}

#[test]
fn m8_transport_follows_five_redirects_and_refuses_the_sixth() {
    let source = Source::start(
        "api",
        "127.0.0.1",
        vec![
            ("/r0", Route::redirect("/r1")),
            ("/r1", Route::redirect("/r2")),
            ("/r2", Route::redirect("/r3")),
            ("/r3", Route::redirect("/r4")),
            ("/r4", Route::redirect("/r5")),
            ("/r5", Route::status(200, b"done".to_vec())),
            ("/s0", Route::redirect("/s1")),
            ("/s1", Route::redirect("/s2")),
            ("/s2", Route::redirect("/s3")),
            ("/s3", Route::redirect("/s4")),
            ("/s4", Route::redirect("/s5")),
            ("/s5", Route::redirect("/s6")),
            ("/s6", Route::status(200, b"never".to_vec())),
        ],
    );
    let transport = transport(&source.origin(), &source.origin()).expect("transport");
    let five = format!("{}/r0", source.origin());
    let response = run_local(async { transport.get(&five, &[]).await }).expect("five redirects");
    assert_eq!(response.status, 200);
    assert_eq!(response.body, b"done");

    let six = format!("{}/s0", source.origin());
    let error = run_local(async { transport.get(&six, &[]).await })
        .expect_err("the sixth redirect in a row must be refused");
    assert!(
        format!("{error}").contains("redirect"),
        "the refusal must name the redirect limit: {error}"
    );
    let targets: Vec<String> = source
        .requests()
        .into_iter()
        .map(|request| request.target)
        .collect();
    assert!(
        !targets.iter().any(|target| target == "/s6"),
        "the address after the sixth redirect must never be requested: {targets:?}"
    );
}

/// The one classifier's real values: public unicast addresses pass, every
/// special-purpose class the updater must never reach fails, and an IPv4-mapped
/// IPv6 address is judged as its IPv4 address.
#[test]
fn m8_address_classifier_is_public_unicast_only() {
    use crate::updater::http::is_public_address;
    for public in [
        "8.8.8.8",
        "1.1.1.1",
        "140.82.112.3",
        "2001:4860:4860::8888",
        "::ffff:8.8.8.8",
    ] {
        let address = public.parse().expect("address");
        assert!(
            is_public_address(address),
            "{public} must be public unicast"
        );
    }
    for rejected in [
        "127.0.0.1",
        "127.0.0.2",
        "10.0.0.1",
        "172.16.0.1",
        "192.168.0.1",
        "169.254.1.1",
        "0.0.0.0",
        "224.0.0.1",
        "255.255.255.255",
        "192.0.2.1",
        "198.51.100.1",
        "203.0.113.1",
        "100.64.0.1",
        "198.18.0.1",
        "::1",
        "::",
        "fe80::1",
        "fc00::1",
        "ff02::1",
        "2001:db8::1",
        "::ffff:10.0.0.1",
        "::ffff:127.0.0.1",
        "::ffff:192.168.1.1",
    ] {
        let address = rejected.parse().expect("address");
        assert!(
            !is_public_address(address),
            "{rejected} must never be a trusted public unicast address"
        );
    }
}

#[test]
fn m8_transport_refuses_private_and_mapped_private_dns_answers() {
    // A name binding is NOT a loopback literal: whatever it resolves to must be
    // public unicast, so a private or IPv4-mapped-private answer is refused
    // before any connect.
    let resolver = map_resolver(vec![
        ("update.example", vec!["127.0.0.1"]),
        ("package.example", vec!["::ffff:10.1.2.3"]),
    ]);
    let transport = crate::updater::http::Transport::for_test(
        "http://update.example:80",
        "http://package.example:80",
        crate::updater::http::body_limit(),
        resolver,
    )
    .expect("transport");
    for url in ["http://update.example/x", "http://package.example/y"] {
        let error = run_local(async { transport.get(url, &[]).await })
            .expect_err("a non-public address must be refused");
        assert!(
            format!("{error}").contains("public") || format!("{error}").contains("address"),
            "the refusal must name the address policy: {error}"
        );
    }
}

#[test]
fn m8_transport_rejects_invalid_utf8_before_the_business_module() {
    let root = tempfile::tempdir().expect("root");
    let source = Source::start(
        "api",
        "127.0.0.1",
        vec![(COMMITS_TARGET, Route::status(200, vec![0xff, 0xfe, 0x00]))],
    );
    publish_binding(root.path(), &source.origin(), &source.origin());
    let error = run_check(root.path(), "check")
        .expect_err("a non-UTF-8 body must not reach the strict typed decode");
    assert!(
        format!("{error}").contains("UTF-8"),
        "the refusal must name UTF-8: {error}"
    );
}

#[test]
fn m8_transport_cancel_of_a_half_response_closes_and_the_owner_stays_usable() {
    let body = COMMITS.as_bytes().to_vec();
    let source = Arc::new(Source::start(
        "api",
        "127.0.0.1",
        vec![(COMMITS_TARGET, Route::holding(body.clone(), body.len() / 2))],
    ));
    let transport = transport(&source.origin(), &source.origin()).expect("transport");
    let url = format!("{}{COMMITS_TARGET}", source.origin());

    // Cancel by dropping the whole future at a deadline: no fixture release.
    let cancelled = run_local(async {
        tokio::time::timeout(Duration::from_millis(300), transport.get(&url, &[])).await
    });
    assert!(cancelled.is_err(), "the half response must be cancelled");
    assert!(
        m6::wait_for(Duration::from_secs(10), || source.peer_eof(COMMITS_TARGET)),
        "a cancelled request must close its own connection"
    );

    // The same owner is immediately usable again.
    source.set(COMMITS_TARGET, Route::json(COMMITS));
    source.release();
    let response = run_local(async { transport.get(&url, &[]).await }).expect("next request");
    assert_eq!(response.status, 200);
    assert_eq!(response.text().expect("text"), COMMITS);
}

#[test]
fn m8_download_requests_the_signature_and_rejects_after_its_body_is_released() {
    let root = tempfile::tempdir().expect("root");
    let base = format!("/fffonion/xeHentai/releases/download/rss-{UPDATE_ID}/xeHentai-rss.zip");
    let descriptor = format!("{base}.manifest.json");
    let signature = format!("{base}.sig");
    let body = b"not a signature".to_vec();
    let source = Arc::new(Source::start(
        "api",
        "127.0.0.1",
        vec![
            (COMMITS_TARGET, Route::json(COMMITS)),
            (&descriptor, Route::json("{}")),
            (&signature, Route::holding(body, 4)),
        ],
    ));
    publish_binding(root.path(), &source.origin(), &source.origin());
    let root_path = root.path().to_path_buf();
    let barrier = Arc::clone(&source);
    let signature_path = signature.clone();

    let report = run_local(async move {
        let check = tokio::task::spawn_local(async move {
            let mut updater =
                crate::updater::Updater::load(&root_path).expect("load the trusted modules");
            updater
                .check("download", false, None, "xeHentai/test")
                .await
                .expect("the check must complete")
        });
        let waiter = Arc::clone(&barrier);
        tokio::task::spawn_blocking(move || waiter.wait_arrived(3))
            .await
            .expect("barrier");
        assert!(
            barrier.partial(&signature_path) > 0,
            "the signature body must still be incomplete at the barrier"
        );
        barrier.release();
        check.await.expect("the check task must finish")
    });

    assert_eq!(report.outcome, "failed");
    // The download decision's availability event precedes the native rejection.
    let keys: Vec<&str> = report
        .events
        .iter()
        .map(|event| event.key.as_str())
        .collect();
    assert_eq!(keys, vec!["UPDATE_AVAILABLE", "UPDATE_FAILED"]);
    let requests = source.requests();
    let targets: Vec<&str> = requests.iter().map(|r| r.target.as_str()).collect();
    assert_eq!(
        targets,
        vec![COMMITS_TARGET, descriptor.as_str(), signature.as_str()],
        "the download fetches the commits, the descriptor and the signature only"
    );
    assert!(
        m6::wait_for(Duration::from_secs(10), || source.peer_eof(&signature)),
        "the released signature connection must be closed by the client"
    );
}
