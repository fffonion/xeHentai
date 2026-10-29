#[allow(dead_code)]
#[path = "support.rs"]
mod support;

use std::collections::HashSet;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vm::HttpConfig;

use xeh_host::{JobKey, PoolConfig, TransferRequest, WorkerPool};

use support::TestRoot;

#[test]
fn two_rss_workers_enter_the_server_barrier_on_distinct_owner_threads() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let address = listener.local_addr().expect("fixture address");
    let barrier = Arc::new(Barrier::new());
    let server = spawn_barrier(listener, Arc::clone(&barrier), 2);
    let _guard = ServerGuard {
        barrier: Arc::clone(&barrier),
        server: Some(server),
    };

    let job_capacity = 2;
    let event_capacity = 4;
    let workers = 2;
    let config = PoolConfig {
        workers,
        job_capacity,
        event_capacity,
        http: local_http(address.port()),
        root: root.path().to_path_buf(),
        chunk_bytes: 1024,
        stream_limit: HttpConfig::default().max_stream_total_bytes,
        writer_fault: None,
        commit_gate: None,
        link_gate: None,
        commit_hold: None,
        hooks: xeh_host::TransportHooks::default(),
        fail_worker_once: false,
        submit_hold: None,
    };
    let pool = WorkerPool::start(config).expect("worker pool");
    pool.wait_ready(Duration::from_secs(5))
        .expect("workers publish owner threads");

    let owners = pool.owner_threads();
    let builds = pool.build_threads();
    assert_eq!(pool.configured_workers(), workers);
    assert_eq!(owners.len(), workers, "owner threads: {owners:?}");
    assert_eq!(
        builds.len(),
        workers,
        "each VM is built on its owner thread: build={builds:?} owner={owners:?}"
    );
    assert_ne!(owners[0], owners[1], "workers must be distinct OS threads");
    assert!(
        !owners.contains(&std::thread::current().id()),
        "owner threads must not be the test thread"
    );
    assert_eq!(
        owners.iter().collect::<HashSet<_>>(),
        builds.iter().collect::<HashSet<_>>(),
        "VM construction thread must be the worker owner"
    );
    assert_eq!(pool.job_capacity(), job_capacity);
    assert_eq!(pool.event_capacity(), event_capacity);
    assert!(pool.job_capacity() > 0);
    assert!(pool.event_capacity() > 0);
    pool.register(1, 1).expect("register");

    pool.submit(request(1, 1, 1, address, "a.bin"))
        .expect("first job");
    pool.submit(request(1, 1, 2, address, "b.bin"))
        .expect("second job");
    barrier
        .wait_entered(2, Duration::from_secs(5))
        .expect("both requests must be inside the server before either response");

    let executing = pool.execution_threads();
    assert_eq!(
        executing.len(),
        workers,
        "both RSS transfers must be executing: {executing:?}"
    );
    assert_ne!(executing[0], executing[1]);
    assert_eq!(
        executing.iter().collect::<HashSet<_>>(),
        owners.iter().collect::<HashSet<_>>(),
        "RSS transfer must run on the VM owner thread"
    );
    assert_eq!(
        pool.peak_in_flight(),
        workers,
        "peak concurrency is the observed in-flight count, not a duration comparison"
    );
    assert_eq!(pool.in_flight(), workers);
    assert_eq!(pool.connect_targets().len(), workers);
    for target in pool.connect_targets() {
        assert_eq!(target.ip().to_string(), "127.0.0.1");
        assert_eq!(target.port(), address.port());
    }

    barrier.release();
    let events = pool
        .wait_terminals(2, Duration::from_secs(5))
        .expect("terminal events");
    assert_eq!(events.len(), 2);
    let mut statuses = events
        .iter()
        .map(|event| event.terminal_status.clone())
        .collect::<Vec<_>>();
    statuses.sort();
    assert_eq!(
        statuses,
        vec![Some("completed".to_string()), Some("completed".to_string())]
    );
    for event in &events {
        assert!(event.terminal_status.is_some());
        assert_eq!(event.bytes, 2);
        assert!(!event.kind.contains("http"));
    }
    assert_eq!(pool.completion_count(1, 1), 2);
    compare_file(&root.path().join("a.bin"), b"ok");
    compare_file(&root.path().join("b.bin"), b"ok");

    let report = pool.shutdown().expect("shutdown");
    assert_eq!(report.pending_jobs, 0);
    assert_eq!(report.in_flight, 0);
    assert_eq!(report.permits, 0);
    assert_eq!(report.tracked_jobs, 0);
    assert!(report.joined);
}

fn request(
    task: i64,
    epoch: i64,
    job: i64,
    address: SocketAddr,
    relative: &str,
) -> TransferRequest {
    TransferRequest {
        key: JobKey {
            task_id: task,
            epoch,
            job_id: job,
        },
        url: format!("http://{address}/"),
        headers: Vec::new(),
        relative_path: relative.to_string(),
    }
}

fn local_http(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(20),
        stream_idle_timeout: Duration::from_secs(5),
        max_stream_duration: Duration::from_secs(20),
        ..HttpConfig::default()
    }
}

fn compare_file(path: &std::path::Path, expected: &[u8]) {
    let mut file = std::fs::File::open(path).expect("downloaded file");
    let mut got = Vec::new();
    let mut buf = [0u8; 64];
    loop {
        let n = file.read(&mut buf).expect("read download");
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    assert_eq!(got, expected);
    assert_eq!(got.len(), expected.len());
}

struct Barrier {
    entered: Mutex<usize>,
    release: Mutex<bool>,
    cv: Condvar,
}

impl Barrier {
    fn new() -> Self {
        Self {
            entered: Mutex::new(0),
            release: Mutex::new(false),
            cv: Condvar::new(),
        }
    }

    fn enter(&self) {
        let mut entered = self.entered.lock().expect("entered");
        *entered += 1;
        self.cv.notify_all();
    }

    fn wait_entered(&self, expected: usize, timeout: Duration) -> Result<(), String> {
        let deadline = Instant::now() + timeout;
        let mut entered = self.entered.lock().expect("entered");
        while *entered < expected {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(format!("barrier entered {} of {expected}", *entered));
            }
            let (next, wait) = self
                .cv
                .wait_timeout(entered, remaining)
                .expect("barrier wait");
            entered = next;
            if wait.timed_out() && *entered < expected {
                return Err(format!("barrier entered {} of {expected}", *entered));
            }
        }
        Ok(())
    }

    fn release(&self) {
        let mut release = self.release.lock().expect("release");
        *release = true;
        self.cv.notify_all();
    }

    fn wait_release(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        let mut release = self.release.lock().expect("release");
        while !*release {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            let (next, _) = self
                .cv
                .wait_timeout(release, remaining)
                .unwrap_or_else(|poison| poison.into_inner());
            release = next;
        }
    }
}

fn spawn_barrier(listener: TcpListener, barrier: Arc<Barrier>, expected: usize) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let mut handlers = Vec::new();
        for _ in 0..expected {
            let (stream, _) = listener.accept().expect("accept");
            let barrier = Arc::clone(&barrier);
            handlers.push(std::thread::spawn(move || handle_one(stream, barrier)));
        }
        for handler in handlers {
            handler.join().expect("handler");
        }
    })
}

fn handle_one(mut stream: TcpStream, barrier: Arc<Barrier>) {
    let _ = read_headers(&mut stream);
    barrier.enter();
    barrier.wait_release();
    let _ =
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
}

fn read_headers(stream: &mut TcpStream) -> Vec<u8> {
    let mut buf = [0u8; 1024];
    let mut got = Vec::new();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    while !got.windows(4).any(|window| window == b"\r\n\r\n") && got.len() < 8192 {
        match stream.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => got.extend_from_slice(&buf[..n]),
        }
    }
    got
}

struct ServerGuard {
    barrier: Arc<Barrier>,
    server: Option<JoinHandle<()>>,
}

impl Drop for ServerGuard {
    fn drop(&mut self) {
        self.barrier.release();
        if let Some(server) = self.server.take() {
            let _ = server.join();
        }
    }
}
