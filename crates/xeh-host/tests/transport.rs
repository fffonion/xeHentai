#[allow(dead_code)]
#[path = "support.rs"]
mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use vm::{HttpConfig, HttpHostExt};

use xeh_host::{
    drive_to_halt, stack_int, standard_catalog, CommitGate, HeaderEntry, JobKey, PoolConfig,
    TransferRequest, WorkerPool, WriterFault,
};

use image::ImageEncoder;
use support::TestRoot;

#[test]
fn worker_streams_png_larger_than_core_body_cap() {
    let image = encode_large_png();
    let cap = HttpConfig::default().max_response_body_bytes;
    assert!(
        image.len() > cap,
        "encoder output {} is not above the Core body cap {cap}",
        image.len()
    );
    assert!(image.starts_with(b"\x89PNG\r\n\x1a\n"));

    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let port = listener.local_addr().expect("port").port();
    let body = Arc::new(image.clone());
    let server = std::thread::spawn(move || serve_fixed(listener, body, 2));

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("core runtime");
    let core_error = runtime.block_on(core_body_limit_error(port));
    assert!(
        core_error.contains("HTTP response body exceeds limit"),
        "core rejection: {core_error}"
    );

    let chunk_bytes = 4096;
    let config = pool_config(&root, port, 1, 1, 4, chunk_bytes);
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.submit(request(1, 1, 1, port, "image.png"))
        .expect("submit");
    let events = pool
        .wait_terminals(1, Duration::from_secs(30))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("completed"));
    let saved = root.path().join("image.png");
    assert_files_equal(&saved, &image);
    let peak = pool.peak_application_bytes();
    let capacity = pool.peak_application_capacity();
    let guest = pool.guest_payload_bytes();
    assert!(peak > 0, "application buffer peak was not observed");
    assert!(
        peak <= chunk_bytes,
        "peak {peak} exceeds chunk {chunk_bytes}"
    );
    assert!(capacity > 0, "buffer capacity was not observed");
    assert!(
        capacity < image.len(),
        "capacity {capacity} held the full image {}",
        image.len()
    );
    assert!(guest > 0, "guest payload was not observed");
    assert!(guest < image.len(), "guest payload {guest} held the image");
    assert_no_staging(root.path());
    let report = pool.shutdown().expect("shutdown");
    assert_eq!(report.pending_jobs, 0);
    assert_eq!(report.permits, 0);
    server.join().expect("server");
}

#[test]
fn advance_before_release_does_not_publish() {
    let root = TestRoot::create();
    let port = serve_ok_in_background(1);
    let gate = CommitGate::new();
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.commit_gate = Some(Arc::clone(&gate));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.submit(request(7, 1, 1, port, "late.bin"))
        .expect("submit");
    gate.wait_arrived(1, Duration::from_secs(5))
        .expect("file closed");
    let saved = root.path().join("late.bin");
    assert!(!saved.exists(), "publish happened before release");
    pool.advance_epoch(7, 2).expect("advance");
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_ne!(events[0].terminal_status.as_deref(), Some("completed"));
    assert!(!saved.exists());
    assert_eq!(pool.completion_count(7, 1), 0);
    assert_eq!(pool.completion_count(7, 2), 0);
    assert_no_staging(root.path());
    pool.shutdown().expect("shutdown");
}

#[test]
fn release_before_advance_publishes_the_old_epoch_once() {
    let root = TestRoot::create();
    let port = serve_ok_in_background(1);
    let gate = CommitGate::new();
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.commit_gate = Some(Arc::clone(&gate));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.submit(request(8, 3, 1, port, "kept.bin"))
        .expect("submit");
    gate.wait_arrived(1, Duration::from_secs(5))
        .expect("file closed");
    gate.release();
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("completed"));
    let saved = root.path().join("kept.bin");
    assert_files_equal(&saved, b"ok");
    assert_eq!(pool.completion_count(8, 3), 1);
    pool.advance_epoch(8, 4).expect("advance");
    assert_eq!(pool.completion_count(8, 3), 1);
    assert_eq!(pool.completion_count(8, 4), 0);
    assert_files_equal(&saved, b"ok");
    pool.shutdown().expect("shutdown");
}

#[test]
fn full_job_queue_rejects_without_starting_another_transfer() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let entered = Arc::new(Note::new());
    let release = Arc::new(Note::new());
    let server = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        std::thread::spawn(move || serve_barrier(listener, entered, release, 2))
    };
    let config = pool_config(&root, port, 1, 1, 4, 64);
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.submit(request(1, 1, 1, port, "a.bin"))
        .expect("running");
    entered
        .wait_count(1, Duration::from_secs(5))
        .expect("entered");
    pool.submit(request(1, 1, 2, port, "b.bin"))
        .expect("queued");
    let rejected = pool
        .submit(request(1, 1, 3, port, "c.bin"))
        .expect_err("queue must reject");
    assert_eq!(rejected.code(), "queue_full");
    release.signal();
    let events = pool
        .wait_terminals(2, Duration::from_secs(5))
        .expect("two terminals");
    assert!(events
        .iter()
        .all(|event| event.terminal_status.as_deref() == Some("completed")));
    let report = pool.shutdown().expect("shutdown");
    assert_eq!(report.pending_jobs, 0);
    assert_eq!(report.permits, 0);
    server.join().expect("server");
}

#[test]
fn cancel_during_stalled_body_closes_the_socket_and_the_pool_accepts_another_job() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let stalled = Arc::new(Note::new());
    let closed = Arc::new(Note::new());
    let server = {
        let stalled = Arc::clone(&stalled);
        let closed = Arc::clone(&closed);
        std::thread::spawn(move || serve_stall_then_ok(listener, stalled, closed))
    };
    let config = pool_config(&root, port, 1, 2, 4, 64);
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    let stalled_key = JobKey {
        task_id: 1,
        epoch: 1,
        job_id: 1,
    };
    pool.submit(request(1, 1, 1, port, "stall.bin"))
        .expect("stall job");
    stalled
        .wait_count(1, Duration::from_secs(5))
        .expect("stalled");
    pool.cancel(&stalled_key).expect("cancel");
    closed
        .wait_count(1, Duration::from_secs(5))
        .expect("socket closed");
    let cancelled = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("cancel terminal");
    assert_eq!(cancelled[0].terminal_status.as_deref(), Some("cancelled"));
    assert!(!root.path().join("stall.bin").exists());
    assert_no_staging(root.path());
    pool.submit(request(1, 1, 2, port, "next.bin"))
        .expect("reuse");
    let done = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("reuse terminal");
    assert_eq!(done[0].terminal_status.as_deref(), Some("completed"));
    assert_files_equal(&root.path().join("next.bin"), b"ok");
    let report = pool.shutdown().expect("shutdown");
    assert_eq!(report.pending_jobs, 0);
    assert_eq!(report.permits, 0);
    assert_eq!(report.tracked_jobs, 0);
    server.join().expect("server");
}

#[test]
fn writer_fault_removes_staging_and_the_same_pool_completes_the_next_job() {
    let root = TestRoot::create();
    let port = serve_ok_in_background(2);
    let mut config = pool_config(&root, port, 1, 2, 4, 64);
    config.writer_fault = Some(WriterFault::once(0));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.submit(request(1, 1, 1, port, "fault.bin"))
        .expect("fault job");
    let faulted = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("fault terminal");
    assert_eq!(faulted[0].terminal_status.as_deref(), Some("io_error"));
    assert!(!root.path().join("fault.bin").exists());
    assert_no_staging(root.path());
    pool.submit(request(1, 1, 2, port, "after.bin"))
        .expect("reuse");
    let done = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("reuse terminal");
    assert_eq!(done[0].terminal_status.as_deref(), Some("completed"));
    assert_files_equal(&root.path().join("after.bin"), b"ok");
    let report = pool.shutdown().expect("shutdown");
    assert_eq!(report.pending_jobs, 0);
    assert_eq!(report.permits, 0);
}

#[test]
fn injected_worker_failure_rebuilds_and_the_same_pool_completes_a_job() {
    let root = TestRoot::create();
    let port = serve_ok_in_background(1);
    let mut config = pool_config(&root, port, 1, 2, 4, 64);
    config.fail_worker_once = true;
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.submit(request(1, 1, 1, port, "panic.bin"))
        .expect("panic job");
    let failed = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("failure terminal");
    assert_eq!(failed[0].terminal_status.as_deref(), Some("worker_failed"));
    assert_eq!(pool.permits(), 0);
    pool.submit(request(1, 1, 2, port, "rebuilt.bin"))
        .expect("reuse");
    let done = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("reuse terminal");
    assert_eq!(done[0].terminal_status.as_deref(), Some("completed"));
    assert_files_equal(&root.path().join("rebuilt.bin"), b"ok");
    pool.shutdown().expect("shutdown");
}

#[test]
fn path_escape_and_existing_target_do_not_overwrite() {
    let root = TestRoot::create();
    let outside = TestRoot::create();
    std::os::unix::fs::symlink(outside.path(), root.path().join("escape")).expect("symlink");
    std::fs::write(root.path().join("kept.bin"), b"keep").expect("existing");
    let port = serve_ok_in_background(4);
    let pool = WorkerPool::start(pool_config(&root, port, 1, 4, 8, 64)).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.submit(request(1, 1, 1, port, "../outside.bin"))
        .expect("parent");
    pool.submit(request(1, 1, 2, port, "escape/file.bin"))
        .expect("symlink");
    pool.submit(request(1, 1, 3, port, "/tmp/absolute.bin"))
        .expect("absolute");
    pool.submit(request(1, 1, 4, port, "kept.bin"))
        .expect("existing");
    let events = pool
        .wait_terminals(4, Duration::from_secs(5))
        .expect("terminals");
    let status = |job: i64| {
        events
            .iter()
            .find(|event| event.key.job_id == job)
            .and_then(|event| event.terminal_status.clone())
            .unwrap_or_default()
    };
    assert_eq!(status(1), "path_rejected");
    assert_eq!(status(2), "path_rejected");
    assert_eq!(status(3), "path_rejected");
    assert_eq!(status(4), "target_exists");
    assert!(!outside.path().join("file.bin").exists());
    assert!(!outside.path().join("outside.bin").exists());
    assert_eq!(
        std::fs::read(root.path().join("kept.bin")).expect("kept"),
        b"keep"
    );
    assert_no_staging(root.path());
    pool.shutdown().expect("shutdown");
}

#[test]
fn policy_redirect_and_hop_header_fail_before_a_file_exists() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let accepted = Arc::new(AtomicBool::new(false));
    let server = {
        let accepted = Arc::clone(&accepted);
        std::thread::spawn(move || serve_redirect_once(listener, accepted))
    };
    let pool = WorkerPool::start(pool_config(&root, port, 1, 3, 4, 64)).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    let mut denied = request(1, 1, 1, port, "denied.bin");
    denied.url = "http://denied.invalid/".to_string();
    pool.submit(denied).expect("denied");
    let mut hop = request(1, 1, 2, port, "hop.bin");
    hop.headers.push(HeaderEntry {
        name: b"connection".to_vec(),
        value: b"close".to_vec(),
    });
    pool.submit(hop).expect("hop");
    pool.submit(request(1, 1, 3, port, "redirect.bin"))
        .expect("redirect");
    let events = pool
        .wait_terminals(3, Duration::from_secs(5))
        .expect("terminals");
    let status = |job: i64| {
        events
            .iter()
            .find(|event| event.key.job_id == job)
            .and_then(|event| event.terminal_status.clone())
            .unwrap_or_default()
    };
    assert_eq!(status(1), "policy_denied");
    assert_eq!(status(2), "header_rejected");
    assert_eq!(status(3), "redirect_unsupported");
    assert!(!root.path().join("denied.bin").exists());
    assert!(!root.path().join("hop.bin").exists());
    assert!(!root.path().join("redirect.bin").exists());
    pool.shutdown().expect("shutdown");
    server.join().expect("server");
}

#[test]
fn epoch_table_stays_within_its_cap_after_many_tasks() {
    let root = TestRoot::create();
    let tasks = 70usize;
    let port = serve_ok_in_background(tasks);
    let pool = WorkerPool::start(pool_config(&root, port, 2, 4, 8, 64)).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    let mut finished = 0usize;
    for id in 1..=tasks {
        loop {
            match pool.submit(request(id as i64, 1, 1, port, &format!("t{id}.bin"))) {
                Ok(()) => break,
                Err(error) if error.code() == "queue_full" => {
                    finished += pool
                        .wait_terminals(1, Duration::from_secs(5))
                        .expect("drain")
                        .len();
                }
                Err(error) => panic!("submit {id}: {error:?}"),
            }
        }
    }
    while finished < tasks {
        finished += pool
            .wait_terminals(1, Duration::from_secs(5))
            .expect("remaining")
            .len();
    }
    assert!(pool.epoch_entries() <= pool.epoch_cap());
    assert_eq!(pool.epoch_cap(), 64);
    assert_eq!(pool.permits(), 0);
    pool.shutdown().expect("shutdown");
}

#[tokio::test(flavor = "current_thread")]
async fn foreign_resource_handle_stays_on_the_owner_thread() {
    let catalog = support::probe_catalog();
    let (mut owner, _) = support::prepare_source(
        r#"
        use xeh_test;
        let handle = xeh_test::open("owned");
        xeh_test::raw(handle);
        "#,
        Arc::clone(&catalog),
    )
    .await;
    drive_to_halt(&mut owner).await.expect("open probe");
    let raw = stack_int(&owner).expect("raw token");
    let (mut foreign, _) = support::prepare_source(
        &format!("use xeh_test; xeh_test::close_raw({raw});"),
        catalog,
    )
    .await;
    let error = drive_to_halt(&mut foreign)
        .await
        .expect_err("foreign token must be rejected");
    let dto = error.to_string();
    assert!(
        dto.contains("different resource table") || dto.contains("wrong"),
        "foreign owner rejection: {dto}"
    );
}

fn pool_config(
    root: &TestRoot,
    port: u16,
    workers: usize,
    job_capacity: usize,
    event_capacity: usize,
    chunk_bytes: usize,
) -> PoolConfig {
    PoolConfig {
        workers,
        job_capacity,
        event_capacity,
        http: local_http(port),
        root: root.path().to_path_buf(),
        chunk_bytes,
        stream_limit: HttpConfig::default().max_stream_total_bytes,
        writer_fault: None,
        commit_gate: None,
        fail_worker_once: false,
    }
}

fn request(task: i64, epoch: i64, job: i64, port: u16, relative: &str) -> TransferRequest {
    TransferRequest {
        key: JobKey {
            task_id: task,
            epoch,
            job_id: job,
        },
        url: format!("http://127.0.0.1:{port}/{relative}"),
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

async fn core_body_limit_error(port: u16) -> String {
    let catalog = standard_catalog();
    let source = format!(
        "use http; let req = http::request::new(\"GET\", \"http://127.0.0.1:{port}/image.png\"); http::client::request(req);"
    );
    let (mut vm, _) = support::prepare_source(&source, catalog).await;
    vm.configure_http(local_http(port)).expect("http policy");
    match drive_to_halt(&mut vm).await {
        Ok(_) => panic!("core accepted a body above its cap"),
        Err(error) => error.to_string(),
    }
}

fn encode_large_png() -> Vec<u8> {
    let width = 1800u32;
    let height = 1800u32;
    let mut pixels = vec![0u8; (width * height * 3) as usize];
    let mut state = 0xA5A5_1234u32;
    for chunk in pixels.as_chunks_mut::<3>().0 {
        state = state.wrapping_mul(1664525).wrapping_add(1013904223);
        chunk[0] = (state >> 16) as u8;
        chunk[1] = (state >> 8) as u8;
        chunk[2] = state as u8;
    }
    let mut bytes = Vec::new();
    let encoder = image::codecs::png::PngEncoder::new(&mut bytes);
    encoder
        .write_image(&pixels, width, height, image::ExtendedColorType::Rgb8)
        .expect("png encoder");
    bytes
}

fn assert_files_equal(path: &Path, expected: &[u8]) {
    let mut file = std::fs::File::open(path).expect("open download");
    let mut offset = 0usize;
    let mut buf = [0u8; 4096];
    loop {
        let n = file.read(&mut buf).expect("read download");
        if n == 0 {
            break;
        }
        assert_eq!(&buf[..n], &expected[offset..offset + n]);
        offset += n;
    }
    assert_eq!(offset, expected.len());
}

fn assert_no_staging(root: &Path) {
    for entry in std::fs::read_dir(root).expect("root") {
        let entry = entry.expect("entry");
        let name = entry.file_name();
        let text = name.to_string_lossy();
        assert!(
            !text.starts_with(".xeh-stage-"),
            "staging left behind: {text}"
        );
    }
}

struct Note {
    count: Mutex<usize>,
    cv: Condvar,
}

impl Note {
    fn new() -> Self {
        Self {
            count: Mutex::new(0),
            cv: Condvar::new(),
        }
    }

    fn signal(&self) {
        *self.count.lock().expect("note") += 1;
        self.cv.notify_all();
    }

    fn wait_count(&self, expected: usize, timeout: Duration) -> Result<(), String> {
        let deadline = std::time::Instant::now() + timeout;
        let mut count = self.count.lock().expect("note");
        while *count < expected {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(format!("note {expected}, saw {}", *count));
            }
            let (next, wait) = self.cv.wait_timeout(count, remaining).expect("wait");
            count = next;
            if wait.timed_out() && *count < expected {
                return Err(format!("note {expected}, saw {}", *count));
            }
        }
        Ok(())
    }
}

fn serve_ok_in_background(count: usize) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    std::thread::spawn(move || serve_fixed(listener, Arc::new(b"ok".to_vec()), count));
    port
}

fn serve_fixed(listener: TcpListener, body: Arc<Vec<u8>>, count: usize) {
    for _ in 0..count {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(20)));
        let _ = read_headers(&mut stream);
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = stream.write_all(header.as_bytes());
        let _ = stream.write_all(&body);
    }
}

fn serve_barrier(listener: TcpListener, entered: Arc<Note>, release: Arc<Note>, count: usize) {
    for _ in 0..count {
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        let _ = read_headers(&mut stream);
        entered.signal();
        release
            .wait_count(1, Duration::from_secs(10))
            .expect("release");
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    }
}

fn serve_stall_then_ok(listener: TcpListener, stalled: Arc<Note>, closed: Arc<Note>) {
    let (mut stream, _) = listener.accept().expect("stall accept");
    let _ = read_headers(&mut stream);
    let _ = stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100000\r\nConnection: close\r\n\r\nx");
    let _ = stream.flush();
    stalled.signal();
    let _ = stream.set_read_timeout(Some(Duration::from_secs(15)));
    let mut buf = [0u8; 64];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => {
                closed.signal();
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::TimedOut => continue,
            Err(_) => {
                closed.signal();
                break;
            }
            Ok(_) => {}
        }
    }
    drop(stream);
    let (mut next, _) = listener.accept().expect("reuse accept");
    let _ = read_headers(&mut next);
    let _ = next.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
}

fn serve_redirect_once(listener: TcpListener, accepted: Arc<AtomicBool>) {
    let Ok((mut stream, _)) = listener.accept() else {
        return;
    };
    accepted.store(true, Ordering::Release);
    let _ = read_headers(&mut stream);
    let _ = stream.write_all(
        b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
    );
}

fn read_headers(stream: &mut std::net::TcpStream) -> Vec<u8> {
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
