#[path = "support.rs"]
mod support;

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

use vm::{
    HostFunctionRegistry, HttpConfig, HttpHostExt, InvocationError, InvocationItem, InvocationPoll,
    IoHostExt, IoPolicy, TimerBackend, TimerConfig, TimerHostExt, Value, VmStatus,
};

use xeh_host::{
    compile_source_with_catalog, drive_to_halt, drive_until_waiting, open_registry,
    release_for_reuse, resource_count, stack_int, standard_catalog, EmbeddingTimer,
};

use support::{
    probe_catalog, register_arm_http, register_fail, register_hold, register_record,
    register_reenter, SyncCallbackGate, TestRoot,
};

#[test]
fn fixture_sources_do_not_embed_a_lease_path() {
    let source = include_str!("support.rs");
    assert!(
        !source.contains("/mnt/") && !source.contains("/home/"),
        "fixture helper must follow TMPDIR instead of a machine or lease path"
    );
    let first = TestRoot::create();
    let second = TestRoot::create();
    assert!(first.path().starts_with(std::env::temp_dir()));
    assert!(second.path().starts_with(std::env::temp_dir()));
    assert_ne!(first.path(), second.path());
    assert!(first.path().is_dir());
}

fn lease_file(root: &TestRoot, name: &str) -> PathBuf {
    let path = root.path().join(name);
    assert!(
        !path.display().to_string().contains(['"', '\\']),
        "fixture path must be a plain RSS string"
    );
    path
}

fn rss_path(path: &std::path::Path) -> String {
    path.display().to_string()
}

fn local_http(port: u16) -> HttpConfig {
    HttpConfig {
        allowed_schemes: vec!["http".to_string()],
        allowed_hosts: vec!["127.0.0.1".to_string()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(20),
        ..HttpConfig::default()
    }
}

#[tokio::test(flavor = "current_thread")]
async fn standard_modules_install_at_the_pinned_revision() {
    open_registry(&standard_catalog()).expect("io, timer, and http catalogs install");
}

#[tokio::test(flavor = "current_thread")]
async fn missing_http_import_fails_before_a_result() {
    let catalog = standard_catalog();
    let program = compile_source_with_catalog(
        "use http; let req = http::request::new(\"GET\", \"http://127.0.0.1/\"); http::response::status(&http::client::request(req));",
        Arc::clone(&catalog),
    )
    .expect("script compiles against the standard catalog");
    let mut vm = vm::Vm::try_new(program).expect("vm");
    vm.set_default_host_fallback_enabled(false);
    let message = match HostFunctionRegistry::empty().bind_vm_cached(&mut vm) {
        Err(error) => error.to_string(),
        Ok(()) => drive_to_halt(&mut vm)
            .await
            .expect_err("unbound http must fail")
            .to_string(),
    };
    assert!(
        message.contains("http"),
        "schema failure must name the missing import: {message}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn json_decode_error_reaches_the_caller() {
    let catalog = standard_catalog();
    let program =
        compile_source_with_catalog("use json; json::decode(\"{\");", Arc::clone(&catalog))
            .expect("invalid json still compiles");
    let mut vm = vm::Vm::try_new(program).expect("vm");
    let registry = open_registry(&catalog).expect("registry");
    xeh_host::bind_vm(&mut vm, &registry).expect("bind");
    let error = drive_to_halt(&mut vm)
        .await
        .expect_err("invalid json must fail");
    assert!(
        error.to_string().contains("json_decode failed"),
        "json builtin error must cross the call boundary: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn io_catalog_open_rejects_the_unstamped_handle() {
    let root = TestRoot::create();
    let path = lease_file(&root, "round-trip.txt");
    let _ = std::fs::remove_file(&path);
    let opened = format!(
        r#"
        use io;
        io::open("{}", "w");
        "#,
        rss_path(&path),
    );
    let (mut vm, _) = support::prepare_source(&opened, standard_catalog()).await;
    configure_io(&mut vm, root.path());
    let error = drive_to_halt(&mut vm)
        .await
        .expect_err("catalog open must reject the unstamped handle");
    let message = error.to_string();
    assert!(
        message.contains("io.file") && message.contains("<none>"),
        "missing resource_type_key must be visible: {message}"
    );
    drop(vm);
}

#[tokio::test(flavor = "current_thread")]
async fn io_policy_authorizes_only_the_test_root() {
    let allowed = TestRoot::create();
    let foreign = TestRoot::create();
    let outside = lease_file(&foreign, "outside.txt");
    std::fs::write(&outside, b"no").expect("foreign fixture");
    let opened = format!(
        r#"
        use io;
        io::open("{}", "r");
        "#,
        rss_path(&outside),
    );
    let (mut vm, _) = support::prepare_source(&opened, standard_catalog()).await;
    configure_io(&mut vm, allowed.path());
    let error = drive_to_halt(&mut vm)
        .await
        .expect_err("a sibling temp root must stay unauthorized");
    let message = error.to_string();
    assert!(
        message.contains("outside the allowed roots"),
        "IO policy must name the foreign path rejection: {message}"
    );
    drop(vm);
}

fn configure_io(vm: &mut vm::Vm, root: &std::path::Path) {
    vm.configure_io(IoPolicy {
        allowed_roots: vec![root.display().to_string()],
        allow_write: true,
        allow_process: false,
        max_read_bytes: 1024,
        max_write_bytes: 1024,
    });
}

#[tokio::test(flavor = "current_thread")]
async fn timer_callback_runs_and_shutdown_cancels_pending() {
    let catalog = probe_catalog();
    let fired = Arc::new(Mutex::new(Vec::new()));
    let timer = register_and_run(
        &catalog,
        r#"
        use timer;
        use xeh_test;
        timer::at(0, |premature| if premature => { xeh_test::record("premature"); null } else => { xeh_test::record("fired"); null });
        timer::pending_count();
        "#,
        Arc::clone(&fired),
    )
    .await;
    assert_eq!(timer.pending_count(), 1);
    assert_eq!(timer.drive_due().expect("drive due"), 1);
    assert_eq!(fired.lock().expect("log").as_slice(), ["fired"]);
    assert_eq!(timer.pending_count(), 0);

    let premature = Arc::new(Mutex::new(Vec::new()));
    let timer = register_and_run(
        &catalog,
        r#"
        use timer;
        use xeh_test;
        timer::at(60000, |premature| if premature => { xeh_test::record("premature"); null } else => { xeh_test::record("fired"); null });
        timer::pending_count();
        "#,
        Arc::clone(&premature),
    )
    .await;
    assert_eq!(timer.pending_count(), 1);
    timer.shutdown().expect("shutdown");
    assert_eq!(timer.pending_count(), 0);
    assert_eq!(premature.lock().expect("log").as_slice(), ["premature"]);
    timer.shutdown().expect("shutdown is idempotent");
    assert!(
        timer.errors().is_empty(),
        "clean cancel must not report an error"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn timer_rejects_drive_and_shutdown_from_another_thread() {
    let timer = Arc::new(EmbeddingTimer::new());
    let probed = Arc::clone(&timer);
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let drive = probed
            .drive_due()
            .expect_err("another thread must not drive the owner VM");
        let shutdown = probed
            .shutdown()
            .expect_err("another thread must not shut down the owner VM");
        tx.send((drive.to_string(), shutdown.to_string()))
            .expect("wrong-thread result");
    });
    let (drive, shutdown) = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("wrong-thread probe");
    assert!(
        drive.contains("owner thread"),
        "drive rejection must name the owner thread: {drive}"
    );
    assert!(
        shutdown.contains("owner thread"),
        "shutdown rejection must name the owner thread: {shutdown}"
    );
    timer.shutdown().expect("owner shutdown");
    assert_eq!(timer.pending_count(), 0);
    assert_eq!(timer.running_count(), 0);
    timer.shutdown().expect("owner shutdown is idempotent");
}

#[test]
fn timer_wrong_owner_drive_and_shutdown_reject_while_sync_callback_runs() {
    let gate = Arc::new(SyncCallbackGate::new());
    let log = Arc::new(Mutex::new(Vec::new()));
    let (started_tx, started_rx) = mpsc::channel();
    let (report_tx, report_rx) = mpsc::channel();
    let owner_gate = Arc::clone(&gate);
    let owner_log = Arc::clone(&log);
    let owner_join = JoinOnDrop(Some(std::thread::spawn(move || {
        let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            run_overlap_owner(owner_gate, owner_log, started_tx)
        }));
        let report = match report {
            Ok(report) => report,
            Err(payload) => OwnerReport::Failed(panic_payload(payload.as_ref())),
        };
        let _ = report_tx.send(report);
    })));
    let mut intruder_join = JoinOnDrop(None);
    let _gate_lease = ReleaseGate(Arc::clone(&gate));

    let started = started_rx
        .recv_timeout(Duration::from_secs(10))
        .expect("owner thread should publish its timer handle");
    let callback_thread = gate
        .wait_entered(Duration::from_secs(20))
        .unwrap_or_else(|error| {
            panic!(
                "{error}; log={:?} pending={} running={}",
                log.lock().expect("log"),
                started.timer.pending_count(),
                started.timer.running_count()
            );
        });
    assert_eq!(
        callback_thread, started.thread_id,
        "the synchronous callback must be executing on the owner thread"
    );
    assert!(
        !gate.is_released(),
        "the overlap probe starts only while the callback is still held"
    );
    let pending_before = started.timer.pending_count();
    let running_before = started.timer.running_count();
    let peak_before = started.timer.peak_running();
    let log_before = log.lock().expect("log").clone();
    assert_eq!(
        pending_before, 2,
        "the subsequent due callback and the late callback stay pending"
    );
    assert_eq!(
        running_before, 1,
        "the gated callback must occupy the running slot before the probe"
    );
    assert_eq!(peak_before, 1, "running must not exceed max_running");
    assert_eq!(
        log_before.as_slice(),
        ["entered"],
        "only the held callback may have started"
    );
    assert!(
        started.timer.errors().is_empty(),
        "callback errors before the probe: {:?}",
        started.timer.errors()
    );

    let (intruder_tx, intruder_rx) = mpsc::channel();
    let probed = Arc::clone(&started.timer);
    intruder_join.0 = Some(std::thread::spawn(move || {
        let thread_id = std::thread::current().id();
        let drive_due = api_text(probed.drive_due());
        let waker = std::task::Waker::noop();
        let mut cx = std::task::Context::from_waker(waker);
        let drive_with = api_text(probed.drive_with(&mut cx));
        let shutdown = api_text(probed.shutdown());
        let _ = intruder_tx.send(IntruderReport {
            thread_id,
            drive_due,
            drive_with,
            shutdown,
        });
    }));
    let intruder = match intruder_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(report) => report,
        Err(error) => panic!(
            "cross-thread drive/shutdown did not return while the callback was inside the gate: {error}; pending={} running={} released={} log={:?}",
            started.timer.pending_count(),
            started.timer.running_count(),
            gate.is_released(),
            log.lock().expect("log")
        ),
    };
    intruder_join.join();
    assert_ne!(
        intruder.thread_id, started.thread_id,
        "the rejection probe must run on a different thread from the owner"
    );
    assert_owner_rejection("drive_due", &intruder.drive_due);
    assert_owner_rejection("drive_with", &intruder.drive_with);
    assert_owner_rejection("shutdown", &intruder.shutdown);
    assert!(
        !gate.is_released(),
        "wrong-owner results arrived only after the callback had been released"
    );
    assert_eq!(
        started.timer.pending_count(),
        pending_before,
        "a rejected shutdown must not clear pending"
    );
    assert_eq!(
        started.timer.running_count(),
        running_before,
        "a rejected shutdown must not clear the running slot"
    );
    assert_eq!(started.timer.peak_running(), peak_before);
    assert_eq!(
        log.lock().expect("log").as_slice(),
        log_before.as_slice(),
        "premature and subsequent callbacks must not run on the rejecting thread"
    );
    assert!(
        started.timer.errors().is_empty(),
        "a rejected cross-thread call must not record a callback error: {:?}",
        started.timer.errors()
    );

    gate.release();
    let report = report_rx
        .recv_timeout(Duration::from_secs(15))
        .unwrap_or_else(|error| {
            panic!("owner did not finish after the gate release: {error}");
        });
    let finished = match report {
        OwnerReport::Finished(finished) => finished,
        OwnerReport::Failed(error) => panic!("owner failed after release: {error}"),
    };
    assert_eq!(
        finished.drive_completed, 2,
        "owner drive must finish the held callback and the next due callback"
    );
    assert_eq!(finished.post_shutdown_drive, 0);
    assert_eq!(finished.pending, 0);
    assert_eq!(finished.running, 0);
    assert!(
        finished.errors.is_empty(),
        "owner cleanup errors: {:?}",
        finished.errors
    );
    assert_eq!(
        finished.log,
        vec![
            "entered".to_string(),
            "finished".to_string(),
            "next".to_string(),
            "premature-late".to_string(),
        ]
    );
    owner_join.join();
}

struct OwnerStarted {
    timer: Arc<EmbeddingTimer>,
    thread_id: std::thread::ThreadId,
}

struct OwnerFinished {
    drive_completed: usize,
    post_shutdown_drive: usize,
    pending: usize,
    running: usize,
    errors: Vec<String>,
    log: Vec<String>,
}

enum OwnerReport {
    Failed(String),
    Finished(OwnerFinished),
}

struct IntruderReport {
    thread_id: std::thread::ThreadId,
    drive_due: String,
    drive_with: String,
    shutdown: String,
}

struct JoinOnDrop(Option<std::thread::JoinHandle<()>>);

impl JoinOnDrop {
    fn join(mut self) {
        if let Some(handle) = self.0.take() {
            handle.join().expect("worker thread");
        }
    }
}

impl Drop for JoinOnDrop {
    fn drop(&mut self) {
        if let Some(handle) = self.0.take() {
            let _ = handle.join();
        }
    }
}

struct ReleaseGate(Arc<SyncCallbackGate>);

impl Drop for ReleaseGate {
    fn drop(&mut self) {
        self.0.release();
    }
}

fn api_text<T: std::fmt::Debug, E: std::fmt::Display>(result: Result<T, E>) -> String {
    match result {
        Ok(value) => format!("ok:{value:?}"),
        Err(error) => error.to_string(),
    }
}

fn assert_owner_rejection(label: &str, text: &str) {
    assert!(
        text.contains("owner thread") && !text.contains("re-enter") && !text.starts_with("ok:"),
        "{label} must be a typed wrong-owner rejection, got {text}"
    );
}

fn panic_payload(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(message) = payload.downcast_ref::<&str>() {
        return (*message).to_string();
    }
    if let Some(message) = payload.downcast_ref::<String>() {
        return message.clone();
    }
    "owner thread panicked".to_string()
}

fn run_overlap_owner(
    gate: Arc<SyncCallbackGate>,
    log: Arc<Mutex<Vec<String>>>,
    started_tx: mpsc::Sender<OwnerStarted>,
) -> OwnerReport {
    let timer = Arc::new(EmbeddingTimer::new());
    let thread_id = std::thread::current().id();
    if started_tx
        .send(OwnerStarted {
            timer: Arc::clone(&timer),
            thread_id,
        })
        .is_err()
    {
        return OwnerReport::Failed("coordinator dropped the timer handle".to_string());
    }
    let catalog = probe_catalog();
    let source = r#"
        use timer;
        use xeh_test;
        timer::at(0, |premature| if premature => { xeh_test::record("premature-active"); null } else => { xeh_test::hold(); xeh_test::record("finished"); null });
        timer::at(0, |premature| if premature => { xeh_test::record("premature-next"); null } else => { xeh_test::record("next"); null });
        timer::at(60000, |premature| if premature => { xeh_test::record("premature-late"); null } else => { xeh_test::record("late"); null });
        timer::pending_count();
    "#;
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => return OwnerReport::Failed(error.to_string()),
    };
    let registered = runtime.block_on(async {
        let program = compile_source_with_catalog(source, Arc::clone(&catalog))
            .map_err(|error| error.to_string())?;
        let mut registry = open_registry(&catalog).map_err(|error| error.to_string())?;
        register_record(&mut registry, &catalog, Arc::clone(&log));
        register_hold(&mut registry, &catalog, Arc::clone(&gate), Arc::clone(&log));
        let mut vm = vm::Vm::try_new(program).map_err(|error| error.to_string())?;
        xeh_host::bind_vm(&mut vm, &registry).map_err(|error| error.to_string())?;
        let backend = Arc::clone(&timer) as Arc<dyn TimerBackend>;
        vm.install_timer_runtime(
            backend,
            TimerConfig {
                max_pending: 8,
                max_running: 1,
            },
        );
        drive_to_halt(&mut vm)
            .await
            .map_err(|error| error.to_string())?;
        let pending = stack_int(&vm).map_err(|error| error.to_string())?;
        if pending != 3 {
            return Err(format!("expected 3 pending registrations, got {pending}"));
        }
        Ok(())
    });
    if let Err(error) = registered {
        return OwnerReport::Failed(error);
    }
    let drive_completed = match timer.drive_due() {
        Ok(count) => count,
        Err(error) => return OwnerReport::Failed(format!("owner drive_due: {error}")),
    };
    if let Err(error) = timer.shutdown() {
        return OwnerReport::Failed(format!("owner shutdown: {error}"));
    }
    let post_shutdown_drive = match timer.drive_due() {
        Ok(count) => count,
        Err(error) => return OwnerReport::Failed(format!("drive after shutdown: {error}")),
    };
    if let Err(error) = timer.shutdown() {
        return OwnerReport::Failed(format!("idempotent shutdown: {error}"));
    }
    OwnerReport::Finished(OwnerFinished {
        drive_completed,
        post_shutdown_drive,
        pending: timer.pending_count(),
        running: timer.running_count(),
        errors: timer.errors(),
        log: log.lock().expect("log").clone(),
    })
}

#[tokio::test(flavor = "current_thread")]
async fn timer_shutdown_cleans_remaining_callbacks_after_an_error() {
    let catalog = probe_catalog();
    let log = Arc::new(Mutex::new(Vec::new()));
    let timer = register_with(
        &catalog,
        r#"
        use timer;
        use xeh_test;
        timer::at(60000, |premature| if premature => { xeh_test::fail(); null } else => { null });
        timer::at(60000, |premature| if premature => { xeh_test::record("second"); null } else => { null });
        timer::pending_count();
        "#,
        Arc::clone(&log),
        true,
    )
    .await;
    assert_eq!(timer.pending_count(), 2);
    timer
        .shutdown()
        .expect("one failing callback must not abort cleanup");
    assert_eq!(timer.pending_count(), 0);
    assert_eq!(timer.running_count(), 0);
    assert_eq!(log.lock().expect("log").as_slice(), ["second"]);
    assert!(
        !timer.errors().is_empty(),
        "the failing callback must be reported"
    );
    timer.shutdown().expect("shutdown is idempotent");
    assert_eq!(log.lock().expect("log").as_slice(), ["second"]);
}

#[tokio::test(flavor = "current_thread")]
async fn timer_callback_cannot_reenter_the_driver() {
    let catalog = probe_catalog();
    let log = Arc::new(Mutex::new(Vec::new()));
    let timer = Arc::new(EmbeddingTimer::new());
    let source = r#"
        use timer;
        use xeh_test;
        timer::at(0, |premature| if premature => { null } else => { xeh_test::reenter(); xeh_test::record("after"); null });
        timer::pending_count();
    "#;
    let program = compile_source_with_catalog(source, Arc::clone(&catalog)).expect("timer script");
    let mut registry = open_registry(&catalog).expect("registry");
    register_record(&mut registry, &catalog, Arc::clone(&log));
    register_reenter(
        &mut registry,
        &catalog,
        Arc::clone(&timer),
        Arc::clone(&log),
    );
    let mut vm = vm::Vm::try_new(program).expect("vm");
    xeh_host::bind_vm(&mut vm, &registry).expect("bind");
    xeh_host::attach_timer(&mut vm, Arc::clone(&timer));
    drive_to_halt(&mut vm).await.expect("register");
    assert_eq!(timer.drive_due().expect("drive outer callback"), 1);
    let entries = log.lock().expect("log").clone();
    assert!(
        entries.iter().any(|entry| entry.contains("re-enter")),
        "nested drive must be a typed rejection, got {entries:?}"
    );
    assert!(
        entries.iter().any(|entry| entry == "after"),
        "the callback must continue after the rejection: {entries:?}"
    );
    assert_eq!(timer.running_count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn timer_callback_can_register_another_timer() {
    let catalog = probe_catalog();
    let log = Arc::new(Mutex::new(Vec::new()));
    let timer = register_and_run(
        &catalog,
        r#"
        use timer;
        use xeh_test;
        timer::at(0, |premature| if true => { timer::at(60000, |inner| if true => { xeh_test::record("inner"); null } else => { null }); xeh_test::record("outer"); null } else => { null });
        timer::pending_count();
        "#,
        Arc::clone(&log),
    )
    .await;
    assert_eq!(timer.drive_due().expect("drive outer"), 1);
    assert_eq!(log.lock().expect("log").as_slice(), ["outer"]);
    assert_eq!(timer.pending_count(), 1);
    assert_eq!(timer.running_count(), 0);
}

#[tokio::test(flavor = "current_thread")]
async fn timer_shutdown_does_not_reschedule_a_periodic_callback() {
    let catalog = probe_catalog();
    let log = Arc::new(Mutex::new(Vec::new()));
    let timer = register_and_run(
        &catalog,
        r#"
        use timer;
        use xeh_test;
        timer::every(60000, |premature| if premature => { xeh_test::record("every"); null } else => { null });
        timer::pending_count();
        "#,
        Arc::clone(&log),
    )
    .await;
    timer.shutdown().expect("shutdown");
    assert_eq!(log.lock().expect("log").as_slice(), ["every"]);
    assert_eq!(timer.pending_count(), 0);
    assert_eq!(timer.running_count(), 0);
    assert_eq!(timer.drive_due().expect("drive after shutdown"), 0);
    timer.shutdown().expect("idempotent shutdown");
    assert_eq!(log.lock().expect("log").as_slice(), ["every"]);
}

#[tokio::test(flavor = "current_thread")]
async fn timer_max_running_keeps_a_due_callback_pending_while_http_waits() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("barrier listener");
    let port = listener.local_addr().expect("port").port();
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || hold_then_serve(listener, accepted_tx, release_rx));
    let log = Arc::new(Mutex::new(Vec::new()));
    let source = format!(
        r#"
        use timer;
        use http;
        use xeh_test;
        timer::at(0, |premature| if premature => {{ xeh_test::record("premature-a"); null }} else => {{ xeh_test::arm_http(); let req = http::request::new("GET", "http://127.0.0.1:{port}/a"); let response = http::client::request(req); xeh_test::record("done-a"); null }});
        timer::at(0, |premature| if premature => {{ xeh_test::record("premature-b"); null }} else => {{ xeh_test::arm_http(); let req = http::request::new("GET", "http://127.0.0.1:{port}/b"); let response = http::client::request(req); xeh_test::record("done-b"); null }});
        timer::pending_count();
        "#
    );
    let timer = register_http_timers(&source, Arc::clone(&log), port, 1).await;
    assert_eq!(timer.pending_count(), 2);
    drive_until(&timer, &accepted_rx, "accepted", "first callback request").await;
    assert_eq!(
        timer.running_count(),
        1,
        "a Waiting callback occupies the only slot"
    );
    assert_eq!(
        timer.pending_count(),
        1,
        "the second due callback stays pending"
    );
    assert!(
        timer.peak_running() <= 1,
        "running peaked at {}",
        timer.peak_running()
    );
    assert!(
        !log.lock()
            .expect("log")
            .iter()
            .any(|entry| entry.starts_with("done-")),
        "neither callback has finished: {:?}",
        log.lock().expect("log")
    );
    release_tx.send(()).expect("release");
    drive_until_log(&timer, &log, &["done-a", "done-b"]).await;
    assert_eq!(timer.pending_count(), 0);
    assert_eq!(timer.running_count(), 0);
    server.join().expect("barrier server");
}

#[tokio::test(flavor = "current_thread")]
async fn timer_shutdown_closes_an_active_http_callback() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("shutdown listener");
    let port = listener.local_addr().expect("port").port();
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || watch_close(listener, tx));
    let log = Arc::new(Mutex::new(Vec::new()));
    let source = format!(
        r#"
        use timer;
        use http;
        use xeh_test;
        timer::at(0, |premature| if premature => {{ xeh_test::record("premature"); null }} else => {{ xeh_test::arm_http(); let req = http::request::new("GET", "http://127.0.0.1:{port}/"); let response = http::client::request(req); xeh_test::record("done"); null }});
        timer::pending_count();
        "#
    );
    let timer = register_http_timers(&source, Arc::clone(&log), port, 1).await;
    drive_until(&timer, &rx, "accepted", "active callback request").await;
    assert_eq!(timer.running_count(), 1);
    timer.shutdown().expect("owner shutdown while Waiting");
    assert_eq!(timer.pending_count(), 0);
    assert_eq!(timer.running_count(), 0);
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5))
            .expect("socket close"),
        "closed"
    );
    assert!(
        !log.lock().expect("log").iter().any(|entry| entry == "done"),
        "shutdown must not deliver a later business event: {:?}",
        log.lock().expect("log")
    );
    let before = log.lock().expect("log").clone();
    timer.shutdown().expect("idempotent shutdown");
    assert_eq!(timer.drive_due().expect("drive after shutdown"), 0);
    assert_eq!(log.lock().expect("log").as_slice(), before.as_slice());
    server.join().expect("close server");
}

async fn register_and_run(
    catalog: &std::sync::Arc<vm::HostApiCatalog>,
    source: &str,
    log: Arc<Mutex<Vec<String>>>,
) -> Arc<EmbeddingTimer> {
    register_with(catalog, source, log, false).await
}

async fn register_with(
    catalog: &std::sync::Arc<vm::HostApiCatalog>,
    source: &str,
    log: Arc<Mutex<Vec<String>>>,
    fail: bool,
) -> Arc<EmbeddingTimer> {
    let program = compile_source_with_catalog(source, Arc::clone(catalog)).expect("timer script");
    let mut registry = open_registry(catalog).expect("registry");
    register_record(&mut registry, catalog, log);
    if fail {
        register_fail(&mut registry, catalog);
    }
    let mut vm = vm::Vm::try_new(program).expect("vm");
    xeh_host::bind_vm(&mut vm, &registry).expect("bind");
    let timer = Arc::new(EmbeddingTimer::new());
    xeh_host::attach_timer(&mut vm, Arc::clone(&timer));
    drive_to_halt(&mut vm).await.expect("timer registration");
    assert_eq!(
        stack_int(&vm).expect("pending count"),
        if fail { 2 } else { 1 }
    );
    timer
}

async fn register_http_timers(
    source: &str,
    log: Arc<Mutex<Vec<String>>>,
    port: u16,
    max_running: usize,
) -> Arc<EmbeddingTimer> {
    let catalog = probe_catalog();
    let program = compile_source_with_catalog(source, Arc::clone(&catalog)).expect("timer script");
    let mut registry = open_registry(&catalog).expect("registry");
    register_record(&mut registry, &catalog, log);
    register_arm_http(&mut registry, &catalog, local_http(port));
    let mut vm = vm::Vm::try_new(program).expect("vm");
    xeh_host::bind_vm(&mut vm, &registry).expect("bind");
    let timer = Arc::new(EmbeddingTimer::new());
    let backend = Arc::clone(&timer) as Arc<dyn TimerBackend>;
    vm.install_timer_runtime(
        backend,
        TimerConfig {
            max_pending: 8,
            max_running,
        },
    );
    drive_to_halt(&mut vm).await.expect("timer registration");
    timer
}

async fn drive_once(timer: &EmbeddingTimer) {
    std::future::poll_fn(|cx| {
        timer.drive_with(cx).unwrap_or_else(|error| {
            panic!(
                "drive failed: {error}; errors={:?} pending={} running={}",
                timer.errors(),
                timer.pending_count(),
                timer.running_count()
            )
        });
        std::task::Poll::Ready(())
    })
    .await;
}

async fn drive_until(
    timer: &EmbeddingTimer,
    rx: &mpsc::Receiver<&'static str>,
    expected: &'static str,
    what: &str,
) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        drive_once(timer).await;
        if let Ok(message) = rx.try_recv() {
            assert_eq!(message, expected, "{what}");
            return;
        }
        if Instant::now() > deadline {
            panic!(
                "{what} was not observed; pending={} running={} errors={:?}",
                timer.pending_count(),
                timer.running_count(),
                timer.errors()
            );
        }
        tokio::task::yield_now().await;
    }
}

async fn drive_until_log(timer: &EmbeddingTimer, log: &Arc<Mutex<Vec<String>>>, expected: &[&str]) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        drive_once(timer).await;
        if log.lock().expect("log").as_slice() == expected
            && timer.pending_count() == 0
            && timer.running_count() == 0
        {
            return;
        }
        if Instant::now() > deadline {
            panic!(
                "callbacks did not finish in order: log={:?} pending={} running={} errors={:?}",
                log.lock().expect("log"),
                timer.pending_count(),
                timer.running_count(),
                timer.errors()
            );
        }
        tokio::task::yield_now().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn http_policy_allow_and_deny() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("fixture listener");
    let port = listener.local_addr().expect("port").port();
    let server = std::thread::spawn(move || immediate_ok(listener));
    let catalog = standard_catalog();
    let allow = format!(
        r#"
        use http;
        let req = http::request::new("GET", "http://127.0.0.1:{port}/");
        let response = http::client::request(req);
        [http::response::status(&response), http::response::body(&response)];
        "#
    );
    let (mut vm, _) = support::prepare_source(&allow, Arc::clone(&catalog)).await;
    vm.configure_http(local_http(port)).expect("http policy");
    drive_to_halt(&mut vm).await.expect("allowed request");
    assert_response(&vm, 200, b"ok");
    server.join().expect("fixture server");

    let deny = r#"
        use http;
        let req = http::request::new("GET", "http://denied.invalid/");
        let response = http::client::request(req);
        http::response::status(&response);
    "#;
    let (mut denied, _) = support::prepare_source(deny, catalog).await;
    denied
        .configure_http(local_http(port))
        .expect("deny policy");
    let error = drive_to_halt(&mut denied)
        .await
        .expect_err("denied host must fail");
    assert!(
        error
            .to_string()
            .contains("HTTP target host is not allowed"),
        "policy rejection must reach the caller: {error}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn cancelled_http_request_closes_and_vm_is_reused() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("stall listener");
    let port = listener.local_addr().expect("port").port();
    let (tx, rx) = mpsc::channel();
    let server = std::thread::spawn(move || stall_then_ok(listener, tx));
    let source = format!(
        r#"
        use http;
        let req = http::request::new("GET", "http://127.0.0.1:{port}/");
        let response = http::client::request(req);
        http::response::status(&response);
        "#
    );
    let (mut vm, _) = support::prepare_source(&source, standard_catalog()).await;
    vm.configure_http(local_http(port)).expect("http policy");
    vm.set_http_max_in_flight(1);
    assert_eq!(
        vm.http_max_in_flight(),
        1,
        "the reused VM has one HTTP permit"
    );
    let status = drive_until_waiting(&mut vm).expect("request should park");
    assert!(
        matches!(status, VmStatus::Waiting(_)),
        "stall must park the VM, got {status:?}"
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(5))
            .expect("server accepted the stalled request"),
        "stalled"
    );
    release_for_reuse(&mut vm)
        .await
        .expect("cancel and quiesce");
    assert_eq!(resource_count(&mut vm), 0);
    assert!(vm.is_reusable());
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(8))
            .expect("cancel should close the socket"),
        "closed"
    );
    drive_to_halt(&mut vm).await.expect("reused request");
    assert_eq!(stack_int(&vm).expect("status"), 200);
    server.join().expect("stall server");
}

#[tokio::test(flavor = "current_thread")]
async fn exported_call_keeps_host_error_on_the_invocation() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("invoke listener");
    let port = listener.local_addr().expect("port").port();
    let server = std::thread::spawn(move || immediate_ok(listener));
    let source = r#"
        use http;
        pub fn status_of(url: string) -> int {
            let req = http::request::new("GET", url);
            let response = http::client::request(req);
            http::response::status(&response)
        }
        0;
    "#;
    let (mut vm, _) = support::prepare_source(source, standard_catalog()).await;
    vm.configure_http(local_http(port)).expect("http policy");
    drive_to_halt(&mut vm).await.expect("export root");
    let denied = invoke(
        &mut vm,
        "status_of",
        vec![Value::String(Arc::new(
            "http://denied.invalid/".to_string(),
        ))],
    )
    .await
    .expect_err("denied invocation");
    assert!(
        denied.contains("HTTP target host is not allowed"),
        "invocation host error: {denied}"
    );
    let allowed = invoke(
        &mut vm,
        "status_of",
        vec![Value::String(Arc::new(format!("http://127.0.0.1:{port}/")))],
    )
    .await
    .expect("allowed invocation after a host error");
    assert_eq!(allowed, Value::Int(200));
    server.join().expect("invoke server");
}

#[tokio::test(flavor = "current_thread")]
async fn probe_handle_rejects_foreign_owner_and_second_close() {
    let catalog = probe_catalog();
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
        Arc::clone(&catalog),
    )
    .await;
    let error = drive_to_halt(&mut foreign)
        .await
        .expect_err("foreign token must be rejected");
    let message = error.to_string();
    assert!(
        message.contains("different resource table") || message.contains("wrong"),
        "foreign owner rejection: {message}"
    );

    let (mut once, _) = support::prepare_source(
        r#"
        use xeh_test;
        let handle = xeh_test::open("once");
        let raw = xeh_test::raw(handle);
        xeh_test::close(handle);
        xeh_test::close_raw(raw);
        "#,
        catalog,
    )
    .await;
    let error = drive_to_halt(&mut once)
        .await
        .expect_err("second close must fail");
    let message = error.to_string();
    assert!(
        message.contains("stale") || message.contains("closed") || message.contains("invalid"),
        "close-once rejection: {message}"
    );
}

fn assert_response(vm: &vm::Vm, status: i64, body: &[u8]) {
    let Some(Value::Array(items)) = vm.stack().last() else {
        panic!(
            "response script should return an array, got {:?}",
            vm.stack()
        );
    };
    assert_eq!(items.first(), Some(&Value::Int(status)));
    match items.get(1) {
        Some(Value::Bytes(actual)) => assert_eq!(&actual[..], body),
        Some(Value::String(actual)) => assert_eq!(actual.as_bytes(), body),
        other => panic!("response body has an unexpected type: {other:?}"),
    }
}

async fn invoke(vm: &mut vm::Vm, name: &str, args: Vec<Value>) -> Result<Value, String> {
    let callable = vm
        .resolve_exported_callable(name)
        .map_err(|error| error.to_string())?;
    let mut invocation = vm
        .start_invocation(callable, args)
        .map_err(|error| error.to_string())?;
    loop {
        let item = std::future::poll_fn(|cx| match invocation.poll_next_with_context(cx) {
            Err(error) => std::task::Poll::Ready(Err(error.to_string())),
            Ok(InvocationPoll::Pending) => std::task::Poll::Pending,
            Ok(InvocationPoll::Ready(item)) => std::task::Poll::Ready(Ok(item)),
        })
        .await?;
        match item {
            Some(Ok(InvocationItem::Complete(value))) => return Ok(value),
            Some(Ok(InvocationItem::Event(_))) => continue,
            Some(Err(InvocationError::Host { message })) => return Err(message),
            Some(Err(error)) => return Err(format!("{error:?}")),
            None => return Err("invocation ended without a result".to_string()),
        }
    }
}

fn immediate_ok(listener: TcpListener) {
    let (mut stream, _) = listener.accept().expect("accept");
    let _ = read_headers(&mut stream);
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .expect("response");
}

fn hold_then_serve(
    listener: TcpListener,
    accepted: mpsc::Sender<&'static str>,
    release: mpsc::Receiver<()>,
) {
    let (mut stream, _) = listener.accept().expect("first accept");
    let _ = read_headers(&mut stream);
    accepted.send("accepted").expect("accepted");
    let _ = release.recv_timeout(Duration::from_secs(8));
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .expect("first response");
    let (mut stream, _) = listener.accept().expect("second accept");
    let _ = read_headers(&mut stream);
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .expect("second response");
}

fn watch_close(listener: TcpListener, tx: mpsc::Sender<&'static str>) {
    let (mut stream, _) = listener.accept().expect("accept");
    let _ = read_headers(&mut stream);
    tx.send("accepted").expect("accepted");
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .expect("timeout");
    let mut buf = [0u8; 8];
    let closed = match stream.read(&mut buf) {
        Ok(0) => true,
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => true,
        _ => false,
    };
    tx.send(if closed { "closed" } else { "timeout" })
        .expect("close signal");
}

fn stall_then_ok(listener: TcpListener, tx: mpsc::Sender<&'static str>) {
    let (mut stream, _) = listener.accept().expect("first accept");
    let _ = read_headers(&mut stream);
    tx.send("stalled").expect("stalled signal");
    stream
        .set_read_timeout(Some(Duration::from_secs(8)))
        .expect("timeout");
    let mut buf = [0u8; 8];
    let closed = match stream.read(&mut buf) {
        Ok(0) => true,
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => true,
        _ => false,
    };
    tx.send(if closed { "closed" } else { "timeout" })
        .expect("close signal");
    let (mut stream, _) = listener.accept().expect("second accept");
    let _ = read_headers(&mut stream);
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
        .expect("response");
}

fn read_headers(stream: &mut std::net::TcpStream) -> Vec<u8> {
    let mut buf = [0u8; 1024];
    let mut got = Vec::new();
    while !got.windows(4).any(|window| window == b"\r\n\r\n") && got.len() < 4096 {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        got.extend_from_slice(&buf[..n]);
    }
    got
}
