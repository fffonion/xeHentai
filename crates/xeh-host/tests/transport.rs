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
    pool.register(1, 1).expect("register");
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
        peak > capacity,
        "peak {peak} did not include the response bytes alongside capacity {capacity}"
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
    pool.register(7, 1).expect("register");
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
    pool.register(8, 3).expect("register");
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
    pool.register(1, 1).expect("register");
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
    pool.register(1, 1).expect("register");
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
    pool.register(1, 1).expect("register");
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
    pool.register(1, 1).expect("register");
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
    pool.register(1, 1).expect("register");
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
    pool.register(1, 1).expect("register");
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
        let mut admitted = false;
        loop {
            if !admitted {
                match pool.register(id as i64, 1) {
                    Ok(()) => admitted = true,
                    Err(error) if error.code() == "epoch_budget" => {
                        finished += pool
                            .wait_terminals(1, Duration::from_secs(5))
                            .expect("drain register")
                            .len();
                        continue;
                    }
                    Err(error) => panic!("register {id}: {error:?}"),
                }
            }
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

#[test]
fn cancel_confirmed_before_link_does_not_publish() {
    let root = TestRoot::create();
    let port = serve_ok_in_background(1);
    let link = CommitGate::new();
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.link_gate = Some(Arc::clone(&link));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(9, 1).expect("register");
    let key = JobKey {
        task_id: 9,
        epoch: 1,
        job_id: 1,
    };
    pool.submit(request(9, 1, 1, port, "race.bin"))
        .expect("submit");
    link.wait_arrived(1, Duration::from_secs(5))
        .expect("before link");
    assert!(!root.path().join("race.bin").exists());
    pool.cancel(&key).expect("cancel wins admission");
    link.release();
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("cancelled"));
    assert!(!root.path().join("race.bin").exists());
    assert_eq!(pool.completion_count(9, 1), 0);
    assert_no_staging(root.path());
    pool.shutdown().expect("shutdown");
}

#[test]
fn cancel_during_commit_reports_committing_and_keeps_the_file() {
    let root = TestRoot::create();
    let port = serve_ok_in_background(1);
    let hold = CommitGate::new();
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.commit_hold = Some(Arc::clone(&hold));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(11, 1).expect("register");
    let key = JobKey {
        task_id: 11,
        epoch: 1,
        job_id: 1,
    };
    pool.submit(request(11, 1, 1, port, "held.bin"))
        .expect("submit");
    hold.wait_arrived(1, Duration::from_secs(5))
        .expect("inside commit");
    let error = pool.cancel(&key).expect_err("too late");
    assert_eq!(error.code(), "committing");
    hold.release();
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("completed"));
    assert_files_equal(&root.path().join("held.bin"), b"ok");
    assert_eq!(pool.completion_count(11, 1), 1);
    pool.shutdown().expect("shutdown");
}

#[test]
fn unregistered_epoch_is_not_self_authorized() {
    let root = TestRoot::create();
    let port = serve_ok_in_background(1);
    let pool = WorkerPool::start(pool_config(&root, port, 1, 1, 4, 64)).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    let error = pool
        .submit(request(21, 1, 1, port, "ghost.bin"))
        .expect_err("unregistered");
    assert_eq!(error.code(), "unregistered");
    assert!(!root.path().join("ghost.bin").exists());
    pool.shutdown().expect("shutdown");
}

#[test]
fn epoch_regression_is_rejected() {
    let root = TestRoot::create();
    let port = serve_ok_in_background(1);
    let pool = WorkerPool::start(pool_config(&root, port, 1, 1, 4, 64)).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(21, 3).expect("register");
    pool.advance_epoch(21, 3).expect("same epoch");
    let error = pool.advance_epoch(21, 2).expect_err("regression");
    assert_eq!(error.code(), "regression");
    pool.shutdown().expect("shutdown");
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
        link_gate: None,
        commit_hold: None,
        hooks: xeh_host::TransportHooks::default(),
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

#[test]
fn queued_cancel_before_dequeue_makes_no_request_and_no_file() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let entered = Arc::new(Note::new());
    let release = Arc::new(Note::new());
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let accepts = Arc::clone(&accepts);
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            serve_until_stop(listener, accepts, entered, release, stop);
        })
    };
    let config = pool_config(&root, port, 1, 2, 4, 64);
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(3, 1).expect("register");
    let queued = JobKey {
        task_id: 3,
        epoch: 1,
        job_id: 2,
    };
    pool.submit(request(3, 1, 1, port, "running.bin"))
        .expect("running");
    entered
        .wait_count(1, Duration::from_secs(5))
        .expect("first request accepted");
    pool.submit(request(3, 1, 2, port, "queued.bin"))
        .expect("queued");
    pool.cancel(&queued).expect("cancel queued");
    release.signal();
    let events = pool
        .wait_terminals(2, Duration::from_secs(5))
        .expect("terminals");
    let status = |job: i64| {
        events
            .iter()
            .find(|event| event.key.job_id == job)
            .and_then(|event| event.terminal_status.clone())
            .unwrap_or_default()
    };
    assert_eq!(status(1), "completed");
    assert_eq!(status(2), "cancelled");
    assert_eq!(
        accepts.load(Ordering::Acquire),
        1,
        "queued cancel must not open a socket"
    );
    assert!(!root.path().join("queued.bin").exists());
    assert_files_equal(&root.path().join("running.bin"), b"ok");
    assert_eq!(pool.completion_count(3, 1), 1);
    assert_no_staging(root.path());
    let report = pool.shutdown().expect("shutdown");
    assert_eq!(report.pending_jobs, 0);
    assert_eq!(report.permits, 0);
    assert_eq!(report.tracked_jobs, 0);
    stop.store(true, Ordering::Release);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    server.join().expect("server");
}

#[test]
fn duplicate_live_key_is_rejected_without_a_second_entry() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let entered = Arc::new(Note::new());
    let release = Arc::new(Note::new());
    let stop = Arc::new(AtomicBool::new(false));
    let server = {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let stop = Arc::clone(&stop);
        std::thread::spawn(move || {
            serve_until_stop(
                listener,
                Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                entered,
                release,
                stop,
            );
        })
    };
    let config = pool_config(&root, port, 1, 1, 4, 64);
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(4, 1).expect("register");
    pool.submit(request(4, 1, 1, port, "live.bin"))
        .expect("live");
    entered
        .wait_count(1, Duration::from_secs(5))
        .expect("accepted");
    let duplicate = pool
        .submit(request(4, 1, 1, port, "live.bin"))
        .expect_err("duplicate key");
    assert_eq!(duplicate.code(), "duplicate");
    assert_eq!(pool.tracked_jobs(), 1);
    pool.submit(request(4, 1, 2, port, "queued.bin"))
        .expect("queued");
    let before = pool.tracked_jobs();
    let full = pool
        .submit(request(4, 1, 3, port, "overflow.bin"))
        .expect_err("queue full");
    assert_eq!(full.code(), "queue_full");
    assert_eq!(pool.tracked_jobs(), before);
    release.signal();
    let events = pool
        .wait_terminals(2, Duration::from_secs(5))
        .expect("terminals");
    assert_eq!(events.len(), 2);
    stop.store(true, Ordering::Release);
    let _ = std::net::TcpStream::connect(("127.0.0.1", port));
    server.join().expect("server");
    pool.shutdown().expect("shutdown");
}

fn serve_until_stop(
    listener: TcpListener,
    accepts: Arc<std::sync::atomic::AtomicUsize>,
    entered: Arc<Note>,
    release: Arc<Note>,
    stop: Arc<AtomicBool>,
) {
    let _ = listener.set_nonblocking(false);
    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let Ok((mut stream, _)) = listener.accept() else {
            return;
        };
        if stop.load(Ordering::Acquire) {
            return;
        }
        let seen = accepts.fetch_add(1, Ordering::AcqRel) + 1;
        let _ = read_headers(&mut stream);
        entered.signal();
        if seen == 1 {
            let _ = release.wait_count(1, Duration::from_secs(10));
        }
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    }
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

#[test]
fn dns_hold_hits_connect_deadline_without_a_file() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let hold = CommitGate::new();
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.http.connect_timeout = Duration::from_millis(200);
    config.http.request_timeout = Duration::from_secs(5);
    config.hooks.dns_hold = Some(Arc::clone(&hold));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(20, 1).expect("register");
    pool.submit(request(20, 1, 1, port, "dns.bin"))
        .expect("submit");
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("deadline"));
    assert!(!root.path().join("dns.bin").exists());
    assert_no_staging(root.path());
    let report = pool.shutdown().expect("shutdown");
    assert!(report.joined);
    assert_eq!(report.pending_jobs, 0);
    drop(listener);
}

#[test]
fn write_hold_cancel_removes_staging() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_headers(&mut stream);
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    });
    let hold = CommitGate::new();
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.hooks.write_hold = Some(Arc::clone(&hold));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(21, 1).expect("register");
    let key = JobKey {
        task_id: 21,
        epoch: 1,
        job_id: 1,
    };
    pool.submit(request(21, 1, 1, port, "write.bin"))
        .expect("submit");
    hold.wait_arrived(1, Duration::from_secs(5))
        .expect("writing");
    pool.cancel(&key).expect("cancel");
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("cancelled"));
    assert!(!root.path().join("write.bin").exists());
    assert_no_staging(root.path());
    pool.shutdown().expect("shutdown");
    server.join().expect("server");
}

#[test]
fn shutdown_during_uncancellable_hold_stays_pending_until_retry() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_headers(&mut stream);
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    });
    let hold = CommitGate::new();
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.hooks.uncancellable_hold = Some(Arc::clone(&hold));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(22, 1).expect("register");
    pool.submit(request(22, 1, 1, port, "stuck.bin"))
        .expect("submit");
    hold.wait_arrived(1, Duration::from_secs(5)).expect("held");
    let pending = pool
        .shutdown_within(Duration::from_millis(200))
        .expect("first drain");
    assert!(pending.draining);
    assert!(!pending.joined);
    assert!(pending.tracked_jobs > 0 || pending.permits > 0);
    hold.release();
    let done = pool.shutdown_within(Duration::from_secs(5)).expect("retry");
    assert!(done.joined);
    assert_eq!(done.pending_jobs, 0);
    assert_eq!(done.permits, 0);
    assert_eq!(done.tracked_jobs, 0);
    server.join().expect("server");
}

#[test]
fn many_unique_failures_keep_observation_bounded() {
    let root = TestRoot::create();
    let mut config = pool_config(&root, 9, 1, 8, 64, 64);
    config.http.allowed_hosts = vec!["127.0.0.1".to_string()];
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    for id in 1..=80 {
        pool.register(id, 1).expect("register");
        let mut job = request(id, 1, 1, 9, "no.bin");
        job.url = format!("http://denied.example/{id}");
        pool.submit(job).expect("submit");
        if id % 8 == 0 {
            pool.wait_terminals(8, Duration::from_secs(5))
                .expect("batch");
        }
    }
    let idle = std::time::Instant::now() + Duration::from_secs(5);
    while pool.tracked_jobs() != 0 {
        if std::time::Instant::now() >= idle {
            panic!("tracked jobs {}", pool.tracked_jobs());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(pool.failure_codes() <= 32);
    assert!(pool.epoch_entries() <= pool.epoch_cap());
    assert!(pool.state_entries() <= pool.epoch_cap() * 3);
    assert_eq!(pool.tracked_jobs(), 0);
    let report = pool.shutdown().expect("shutdown");
    assert!(report.joined);
    assert_eq!(report.permits, 0);
}

#[test]
fn dropped_receiver_releases_the_permit() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_headers(&mut stream);
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    });
    let config = pool_config(&root, port, 1, 1, 1, 64);
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.detach_events();
    pool.register(23, 1).expect("register");
    pool.submit(request(23, 1, 1, port, "drop.bin"))
        .expect("submit");
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while pool.permits() != 0 || pool.dropped_terminal() == 0 {
        if std::time::Instant::now() >= deadline {
            panic!(
                "permit {} dropped {}",
                pool.permits(),
                pool.dropped_terminal()
            );
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let report = pool.shutdown().expect("shutdown");
    assert!(report.joined);
    assert_eq!(report.permits, 0);
    server.join().expect("server");
}

#[test]
fn two_workers_hold_bytes_and_vecs_at_once() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = read_headers(&mut stream);
            let _ = stream.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\nabcd",
            );
        }
    });
    let hold = CommitGate::new();
    let mut config = pool_config(&root, port, 2, 2, 4, 64);
    config.hooks.buffer_hold = Some(Arc::clone(&hold));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(24, 1).expect("register");
    pool.submit(request(24, 1, 1, port, "a.bin")).expect("a");
    pool.submit(request(24, 1, 2, port, "b.bin")).expect("b");
    hold.wait_arrived(2, Duration::from_secs(5))
        .expect("both held");
    let current = pool.application_bytes();
    assert!(
        current >= 8,
        "concurrent hold {current} did not include both copies"
    );
    hold.release();
    let events = pool
        .wait_terminals(2, Duration::from_secs(5))
        .expect("done");
    assert_eq!(events.len(), 2);
    assert_eq!(pool.application_bytes(), 0);
    pool.shutdown().expect("shutdown");
    server.join().expect("server");
}

#[test]
fn hostname_peer_is_not_the_dns_decoy() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let _ = read_headers(&mut stream);
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
    });
    let decoy = std::net::SocketAddr::from(([127, 0, 0, 1], 1));
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.http.allowed_hosts = vec!["localhost".to_string()];
    config.hooks.dns_decoy = Some(decoy);
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(25, 1).expect("register");
    let mut job = request(25, 1, 1, port, "host.bin");
    job.url = format!("http://localhost:{port}/host.bin");
    pool.submit(job).expect("submit");
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("completed"));
    assert!(pool.dns_candidates().contains(&decoy));
    let peers = pool.connect_targets();
    assert!(peers.iter().any(|peer| peer.port() == port));
    assert!(peers.iter().all(|peer| *peer != decoy));
    pool.shutdown().expect("shutdown");
    server.join().expect("server");
}

#[test]
fn retired_incarnation_cannot_submit_the_old_epoch() {
    let root = TestRoot::create();
    let pool = WorkerPool::start(pool_config(&root, 9, 1, 1, 4, 64)).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(1, 1).expect("first");
    for id in 2..=65 {
        pool.register(id, 1).expect("fill");
    }
    let evicted = pool
        .submit(request(1, 1, 1, 9, "old.bin"))
        .expect_err("evicted");
    assert_eq!(evicted.code(), "unregistered");
    pool.register(1, 2).expect("reregister");
    let stale = pool
        .submit(request(1, 1, 1, 9, "old.bin"))
        .expect_err("old epoch");
    assert_eq!(stale.code(), "stale_epoch");
    assert!(!root.path().join("old.bin").exists());
    assert_eq!(pool.tracked_jobs(), 0);
    pool.shutdown().expect("shutdown");
}

#[test]
fn live_receiver_keeps_a_full_terminal_until_drain() {
    let root = TestRoot::create();
    let listener = TcpListener::bind("127.0.0.1:0").expect("listener");
    let port = listener.local_addr().expect("port").port();
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let _ = read_headers(&mut stream);
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        }
    });
    let hold = CommitGate::new();
    let mut config = pool_config(&root, port, 2, 2, 1, 64);
    config.hooks.buffer_hold = Some(Arc::clone(&hold));
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(28, 1).expect("register");
    pool.submit(request(28, 1, 1, port, "one.bin"))
        .expect("one");
    pool.submit(request(28, 1, 2, port, "two.bin"))
        .expect("two");
    hold.wait_arrived(2, Duration::from_secs(5))
        .expect("both held");
    hold.release();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        pool.dropped_terminal(),
        0,
        "live receiver dropped a terminal"
    );
    let events = pool
        .wait_terminals(2, Duration::from_secs(5))
        .expect("both terminals");
    assert_eq!(events.len(), 2);
    assert_eq!(pool.dropped_terminal(), 0);
    assert!(events
        .iter()
        .all(|event| event.terminal_status.as_deref() == Some("completed")));
    let report = pool.shutdown().expect("shutdown");
    assert!(report.joined);
    assert_eq!(report.permits, 0);
    server.join().expect("server");
}

fn local_tls_material() -> (Vec<u8>, Vec<u8>, Vec<u8>) {
    let ca_key = rcgen::KeyPair::generate().expect("ca key");
    let mut ca_params =
        rcgen::CertificateParams::new(vec!["ca.local".to_string()]).expect("ca params");
    ca_params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca_cert = ca_params.self_signed(&ca_key).expect("ca");
    let leaf_key = rcgen::KeyPair::generate().expect("leaf key");
    let leaf_params =
        rcgen::CertificateParams::new(vec!["localhost".to_string()]).expect("leaf params");
    let leaf = leaf_params
        .signed_by(&leaf_key, &ca_cert, &ca_key)
        .expect("leaf");
    (
        ca_cert.der().to_vec(),
        leaf.der().to_vec(),
        leaf_key.serialize_der(),
    )
}

fn spawn_tls_server(leaf_der: Vec<u8>, key_der: Vec<u8>) -> (u16, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("tls listener");
    let port = listener.local_addr().expect("port").port();
    let handle = std::thread::spawn(move || {
        let (mut tcp, _) = listener.accept().expect("tls accept");
        tcp.set_read_timeout(Some(Duration::from_secs(2))).ok();
        let config = rustls::ServerConfig::builder_with_provider(
            rustls::crypto::ring::default_provider().into(),
        )
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from(leaf_der)],
            rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
                key_der,
            )),
        )
        .expect("server cert");
        let mut conn =
            rustls::ServerConnection::new(std::sync::Arc::new(config)).expect("server conn");
        let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
        let mut buf = [0u8; 2048];
        let _ = std::io::Read::read(&mut tls, &mut buf);
        let _ = std::io::Write::write_all(
            &mut tls,
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
    });
    (port, handle)
}

#[test]
fn untrusted_tls_certificate_does_not_complete() {
    let (_ca, leaf, key) = local_tls_material();
    let root = TestRoot::create();
    let (port, server) = spawn_tls_server(leaf, key);
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.http.allowed_hosts = vec!["localhost".to_string()];
    config.http.allowed_schemes = vec!["https".to_string()];
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(26, 1).expect("register");
    let mut job = request(26, 1, 1, port, "bad-tls.bin");
    job.url = format!("https://localhost:{port}/bad-tls.bin");
    pool.submit(job).expect("submit");
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_ne!(events[0].terminal_status.as_deref(), Some("completed"));
    assert!(!root.path().join("bad-tls.bin").exists());
    assert_no_staging(root.path());
    pool.shutdown().expect("shutdown");
    let _ = server.join();
}

#[test]
fn trusted_local_tls_certificate_completes() {
    let (ca, leaf, key) = local_tls_material();
    let root = TestRoot::create();
    let (port, server) = spawn_tls_server(leaf, key);
    let mut config = pool_config(&root, port, 1, 1, 4, 64);
    config.http.allowed_hosts = vec!["localhost".to_string()];
    config.http.allowed_schemes = vec!["https".to_string()];
    config.hooks.extra_trust_der = vec![ca];
    let pool = WorkerPool::start(config).expect("pool");
    pool.wait_ready(Duration::from_secs(5)).expect("ready");
    pool.register(27, 1).expect("register");
    let mut job = request(27, 1, 1, port, "tls.bin");
    job.url = format!("https://localhost:{port}/tls.bin");
    pool.submit(job).expect("submit");
    let events = pool
        .wait_terminals(1, Duration::from_secs(5))
        .expect("terminal");
    assert_eq!(events[0].terminal_status.as_deref(), Some("completed"));
    assert_eq!(
        std::fs::read(root.path().join("tls.bin")).expect("file"),
        b"ok"
    );
    pool.shutdown().expect("shutdown");
    server.join().expect("tls server");
}
