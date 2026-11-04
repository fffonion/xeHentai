//! Focused proof (diagnostic candidate): the terminal HTTP worker wakes the
//! waiting VM only *before* its OS thread returns, so the last quiescence wake
//! can be lost.
//!
//! Candidate under test, pinned Core `cdb0b79`
//! (`src/builtins/runtime/http/request.rs`):
//!
//! * `1210-1213`: the worker publishes its result, calls
//!   `mark_worker_finished()` and re-wakes the result waker, then the closure
//!   returns and the thread exits.
//! * `638-644`: `mark_worker_finished` stores `thread_finished` and
//!   `WorkerLifecycle::Finished` and wakes the close/quiescence wakers while
//!   the OS thread is still executing.
//! * `696-723`: `try_join_finished` refuses to join while
//!   `JoinHandle::is_finished()` is false.
//! * `725-734`, `908-923`: `is_quiescent` additionally requires the join
//!   handle to be taken, so `poll_quiescent` can return `Pending` immediately
//!   after the last quiescence wake with every shared "finished" flag set, and
//!   nothing wakes the registered quiescence waker once the thread really
//!   exits.
//! * `vm/operation/registry.rs 311-323, 670-687`: the public SDK only delivers
//!   `Completed` after `driver.poll_quiescent` reports ready.
//!
//! The probe drives the real public `Vm::await_waiting_host_op` future by hand
//! (exactly the entry the production updater `call_waiting` awaits) with a
//! counted, gated test waker. Every wake the pinned HTTP worker delivers is
//! parked inside the test waker (bounded, never forever) so the test can poll
//! the VM between the worker's own notification points instead of sleeping on
//! the race.
//!
//! Verdicts:
//!
//! * RED (`panic!`): after the worker OS thread is observed gone
//!   (`/proc/self/task/<tid>` disappears; tid captured inside the worker's own
//!   wake callback and matched against the `rustscript-http-request` thread
//!   name) the await is still `Pending` and one explicit diagnostic poll
//!   completes the real response. A wake-driven consumer would never be woken
//!   after the last quiescence wake.
//! * GREEN: a wake is delivered after the worker thread exit (or the await is
//!   already ready without an explicit poll), i.e. no lost wake on this
//!   schedule.
//!
//! The fixture never releases the response before the first poll and every
//! gate/wait/thread join is bounded, so the schedule is deterministic rather
//! than timeout-dependent.

#![cfg(target_os = "linux")]

use std::io::{Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use vm::{
    CapabilityProfile, CompileSourceFileOptions, HostFunctionRegistry, HttpConfig, HttpHostExt,
    SourceFlavor, Value, Vm, VmResult, VmStatus, compile_source_with_flavor_and_options,
};

/// The owned worker thread name set by Core `spawn_worker`; `/proc/<pid>/task/
/// <tid>/comm` truncates it to 15 bytes.
const WORKER_THREAD_COMM: &str = "rustscript-http";
const WAKE_PARK_LIMIT: Duration = Duration::from_secs(10);
const PHASE_LIMIT: Duration = Duration::from_secs(10);
const EXIT_LIMIT: Duration = Duration::from_secs(5);

const PROBE_SCRIPT: &str = r#"
use http;

pub fn exit_probe(url: string) -> int {
    let request = http::request::new("GET", url);
    let response = http::client::request(request);
    http::response::status(&response)
}
"#;

/// Drives the real buffered `http::client::request` host operation through the
/// public SDK with a deterministic schedule.
#[test]
fn terminal_http_worker_exit_must_wake_the_waiting_vm() {
    let mut fixture = Fixture::start();
    let port = fixture.addr.port();
    let mut vm = build_vm(port);
    let callable = vm
        .resolve_exported_callable("exit_probe")
        .expect("probe callable must be exported");
    let url = format!("http://127.0.0.1:{port}/probe");

    let gate = Arc::new(Gate::new());
    let _shutdown = GateShutdown(Arc::clone(&gate));
    gate.arm();

    let status = vm
        .start_callable(callable, &[Value::string(url)])
        .expect("probe callable must start");
    assert!(
        matches!(status, VmStatus::Waiting(_)),
        "expected the script to wait on the HTTP host operation, got {status:?}"
    );

    // The worker is inside its real network read: the fixture has received the
    // request and withholds the response until this test says so. That makes
    // the first poll register the waker before the first publish wake.
    fixture.wait_requested();
    let reported_status = format!("{status:?}");
    let report = drive_await(&mut vm, &gate, &fixture, reported_status);
    gate.disarm();

    let detail = report.describe();
    assert!(
        !report.timeout,
        "probe inconclusive: neither a gated worker wake nor the worker thread \
         exit was observed within {PHASE_LIMIT:?}\n{detail}"
    );
    let tid = report
        .worker_tid
        .expect("the owned HTTP worker must have delivered at least one wake");
    assert_eq!(
        report.worker_comm.as_deref(),
        Some(WORKER_THREAD_COMM),
        "the parked wake must come from the owned HTTP worker thread\n{detail}"
    );

    if !report.await_ready {
        panic!(
            "probe inconclusive: the public await neither completed nor stayed pending\n{detail}"
        );
    }

    // Complete the real response through the public SDK; the status text is
    // part of the evidence either way.
    let status_text = finish_and_read_status(&mut vm);

    let lost_wake = report.worker_exited
        && report.pending_after_last_wake
        && report.nudge.as_deref() == Some("Ready(Ok)");

    assert!(
        !lost_wake,
        "LOST TERMINAL WAKE: the pinned HTTP worker announced completion and \
         woke the waiting VM {wakes} time(s), yet after the worker OS thread \
         (tid {tid}, comm {comm:?}) had fully exited the public \
         `Vm::await_waiting_host_op` was still Pending with no further wake \
         (wakes unchanged at {wakes_at_exit}); one explicit diagnostic poll \
         then delivered the real response (status {status_text}). A \
         wake-driven consumer such as the production updater `call_waiting` is \
         therefore not woken after the worker's last quiescence wake: \
         `is_quiescent()` also requires `JoinHandle::is_finished()`, and \
         nothing wakes `quiescence_waker` once the thread really returns \
         (request.rs 638-644 / 696-734 / 908-923 / 1210-1213).\n{detail}",
        wakes = report.parked_wakes,
        comm = report.worker_comm,
        wakes_at_exit = report.wakes_at_exit,
    );

    // Falsified on this schedule: the real response still completed normally.
    assert_eq!(
        status_text, "200",
        "the real public HTTP host operation must return the fixture status\n{detail}"
    );
}

fn build_vm(port: u16) -> Vm {
    let compiled = compile_source_with_flavor_and_options(
        PROBE_SCRIPT,
        SourceFlavor::RustScript,
        CompileSourceFileOptions::new(),
    )
    .expect("probe script must compile");
    let mut vm = Vm::new(compiled.program);
    probe_registry()
        .bind_vm_cached(&mut vm)
        .expect("probe host functions must bind");
    assert_eq!(
        vm.run().expect("probe module must run"),
        VmStatus::Halted,
        "probe module must only declare its callable"
    );
    vm.configure_http(HttpConfig {
        allowed_schemes: vec!["http".into()],
        allowed_hosts: vec!["127.0.0.1".into()],
        allowed_ports: vec![port],
        allow_private_ips: true,
        connect_timeout: Duration::from_secs(5),
        request_timeout: Duration::from_secs(5),
        ..HttpConfig::default()
    })
    .expect("probe HTTP policy must configure");
    vm
}

/// The same restricted surface the updater grants for its one HTTP call: only
/// the three host imports this script names, nothing else.
fn probe_registry() -> &'static HostFunctionRegistry {
    static REGISTRY: OnceLock<HostFunctionRegistry> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut registry = HostFunctionRegistry::restricted();
        let mut profile = CapabilityProfile::builder();
        for name in [
            "http::request::new",
            "http::client::request",
            "http::response::status",
        ] {
            profile = profile.allow_host_import(name);
        }
        registry.set_capability_profile(profile.build());
        registry
    })
}

#[derive(Debug, Default)]
struct Report {
    phases: Vec<String>,
    parked_wakes: usize,
    main_thread_wakes: usize,
    polls: usize,
    worker_tid: Option<u64>,
    worker_comm: Option<String>,
    worker_exited: bool,
    wakes_at_exit: usize,
    pending_after_last_wake: bool,
    await_ready: bool,
    nudge: Option<String>,
    timeout: bool,
}

impl Report {
    fn describe(&self) -> String {
        format!(
            "parked_wakes={} main_thread_wakes={} polls={} worker_tid={:?} \
             worker_comm={:?} \
             worker_exited={} wakes_at_exit={} pending_after_last_wake={} \
             await_ready={} nudge={:?}\nphases:\n  {}",
            self.parked_wakes,
            self.main_thread_wakes,
            self.polls,
            self.worker_tid,
            self.worker_comm,
            self.worker_exited,
            self.wakes_at_exit,
            self.pending_after_last_wake,
            self.await_ready,
            self.nudge,
            self.phases.join("\n  ")
        )
    }
}

/// Polls the public awaiting-host-operation future only in response to a real
/// worker wake (plus the initial poll and the single documented diagnostic
/// nudge after the worker thread is gone).
fn drive_await(vm: &mut Vm, gate: &Arc<Gate>, fixture: &Fixture, waiting: String) -> Report {
    let mut report = Report::default();
    report
        .phases
        .push(format!("operation admitted: VmStatus::{waiting}"));
    let waker = Waker::from(Arc::new(GatedWaker {
        gate: Arc::clone(gate),
    }));
    let mut cx = Context::from_waker(&waker);
    let mut fut = Box::pin(vm.await_waiting_host_op());

    report.polls += 1;
    let first = fut.as_mut().poll(&mut cx);
    report.phases.push(format!(
        "poll{} (fixture holding the response) -> {}",
        report.polls,
        poll_text(&first)
    ));
    report.pending_after_last_wake = first.is_pending();
    if first.is_ready() {
        report.await_ready = true;
        return report;
    }

    // Release the response only after the VM registered its result waker.
    fixture.respond();

    let mut consumed = 0u64;
    loop {
        match gate.wait_park(consumed, report.worker_tid, PHASE_LIMIT) {
            ParkOutcome::Parked { seq, tid, comm } => {
                consumed = seq;
                if report.worker_tid.is_none() {
                    report.worker_tid = tid;
                    report.worker_comm = comm.clone();
                }
                report.polls += 1;
                let wakes = gate.wake_count();
                let result = fut.as_mut().poll(&mut cx);
                report.pending_after_last_wake = result.is_pending();
                report.phases.push(format!(
                    "worker wake#{wakes} (park {seq}) on tid {tid:?} ({}) -> poll{} {}",
                    comm.as_deref().unwrap_or("?"),
                    report.polls,
                    poll_text(&result)
                ));
                gate.release(seq);
                if result.is_ready() {
                    report.await_ready = true;
                    break;
                }
            }
            ParkOutcome::Exited => {
                report.worker_exited = true;
                break;
            }
            ParkOutcome::Timeout => {
                report.timeout = true;
                break;
            }
        }
    }

    report.parked_wakes = gate.parked_count();
    report.main_thread_wakes = gate.main_thread_wake_count();
    if let Some(tid) = report.worker_tid {
        if !report.worker_exited {
            report.worker_exited = wait_thread_gone(tid, EXIT_LIMIT);
        }
    }
    report.wakes_at_exit = gate.wake_count();

    if report.worker_exited && report.pending_after_last_wake && !report.await_ready {
        // The one explicit diagnostic poll the contract allows: it must not be
        // counted as normal wake-driven completion.
        report.polls += 1;
        let nudge = fut.as_mut().poll(&mut cx);
        report.phases.push(format!(
            "diagnostic nudge: poll{} after the worker thread exited with no \
             wake -> {}",
            report.polls,
            poll_text(&nudge)
        ));
        report.await_ready = nudge.is_ready();
        report.nudge = Some(poll_text(&nudge));
    }
    report
}

fn finish_and_read_status(vm: &mut Vm) -> String {
    let mut status = "<none>".to_string();
    for _ in 0..8 {
        match vm.resume() {
            Ok(VmStatus::Halted) => {
                status = vm
                    .take_callable_result()
                    .map(|value| value_text(&value))
                    .unwrap_or_else(|| "<none>".into());
                break;
            }
            Ok(VmStatus::Yielded) => continue,
            Ok(VmStatus::Waiting(_)) => {
                status = "<unexpected second host wait>".into();
                break;
            }
            Err(error) => {
                status = format!("<resume error: {error}>");
                break;
            }
        }
    }
    status
}

fn value_text(value: &Value) -> String {
    match value {
        Value::Int(number) => number.to_string(),
        other => format!("{other:?}"),
    }
}

fn poll_text(result: &Poll<VmResult<()>>) -> String {
    match result {
        Poll::Pending => "Pending".to_string(),
        Poll::Ready(Ok(())) => "Ready(Ok)".to_string(),
        Poll::Ready(Err(error)) => format!("Ready(Err({error}))"),
    }
}

#[derive(Clone, Debug)]
enum ParkOutcome {
    Parked {
        seq: u64,
        tid: Option<u64>,
        comm: Option<String>,
    },
    Exited,
    Timeout,
}

struct GatedWaker {
    gate: Arc<Gate>,
}

impl Wake for GatedWaker {
    fn wake(self: Arc<Self>) {
        Self::wake_by_ref(&self);
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.gate.on_wake();
    }
}

/// A counted, gated waker: it records every delivery, captures the delivering
/// thread's native id and comm, and parks the (non-main) caller inside the
/// wake callback until the test has polled and released it. Every park is
/// bounded so a wake callback can never block forever.
struct Gate {
    main_tid: u64,
    gating: AtomicBool,
    wakes: AtomicUsize,
    parked: AtomicUsize,
    main_thread_wakes: AtomicUsize,
    first_worker_tid: AtomicU64,
    state: Mutex<ParkState>,
    changed: Condvar,
}

#[derive(Default)]
struct ParkState {
    /// Monotone park counter: every delivery of this waker that parked the
    /// caller gets its own sequence number.
    seq: u64,
    /// Highest sequence released by the test.
    released: u64,
    parked: bool,
    park_tid: Option<u64>,
    park_comm: Option<String>,
}

impl Gate {
    fn new() -> Self {
        Self {
            main_tid: current_native_tid().unwrap_or(0),
            gating: AtomicBool::new(false),
            wakes: AtomicUsize::new(0),
            parked: AtomicUsize::new(0),
            main_thread_wakes: AtomicUsize::new(0),
            first_worker_tid: AtomicU64::new(0),
            state: Mutex::new(ParkState::default()),
            changed: Condvar::new(),
        }
    }

    fn arm(&self) {
        self.gating.store(true, Ordering::SeqCst);
    }

    fn disarm(&self) {
        self.gating.store(false, Ordering::SeqCst);
        self.changed.notify_all();
    }

    fn wake_count(&self) -> usize {
        self.wakes.load(Ordering::SeqCst)
    }

    fn parked_count(&self) -> usize {
        self.parked.load(Ordering::SeqCst)
    }

    fn main_thread_wake_count(&self) -> usize {
        self.main_thread_wakes.load(Ordering::SeqCst)
    }

    fn on_wake(&self) {
        let tid = current_native_tid().unwrap_or(0);
        self.wakes.fetch_add(1, Ordering::SeqCst);
        if tid != 0 && tid == self.main_tid {
            // A wake raised on the polling thread itself (the legal
            // `AtomicWaker::register` retry). Park nothing: the polling thread
            // is the only one that can release the gate.
            self.main_thread_wakes.fetch_add(1, Ordering::SeqCst);
            return;
        }
        if tid != 0 {
            let _ =
                self.first_worker_tid
                    .compare_exchange(0, tid, Ordering::SeqCst, Ordering::SeqCst);
        }
        if !self.gating.load(Ordering::SeqCst) {
            return;
        }
        let comm = thread_comm(tid);
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if !self.gating.load(Ordering::SeqCst) {
            return;
        }
        state.seq = state.seq.wrapping_add(1);
        let seq = state.seq;
        state.parked = true;
        state.park_tid = Some(tid);
        state.park_comm = comm;
        self.parked.fetch_add(1, Ordering::SeqCst);
        self.changed.notify_all();

        let deadline = Instant::now() + WAKE_PARK_LIMIT;
        while state.released < seq {
            let now = Instant::now();
            if now >= deadline {
                // Bounded: never block a real wake callback forever.
                break;
            }
            let (guard, _) = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poison| poison.into_inner());
            state = guard;
        }
        state.parked = false;
        self.changed.notify_all();
    }

    /// Waits until a park newer than `consumed` exists (or, once the owned
    /// worker thread id is known, until that thread has fully exited).
    fn wait_park(&self, consumed: u64, tid: Option<u64>, limit: Duration) -> ParkOutcome {
        let deadline = Instant::now() + limit;
        loop {
            {
                let state = self
                    .state
                    .lock()
                    .unwrap_or_else(|poison| poison.into_inner());
                if state.parked && state.seq > consumed {
                    return ParkOutcome::Parked {
                        seq: state.seq,
                        tid: state.park_tid,
                        comm: state.park_comm.clone(),
                    };
                }
            }
            if let Some(tid) = tid {
                if !task_entry_exists(tid) {
                    return ParkOutcome::Exited;
                }
            }
            if Instant::now() >= deadline {
                return ParkOutcome::Timeout;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// Releases park `seq` and waits until the parked caller has actually left
    /// it, so the next `wait_park` can never misread a stale park as a new
    /// wake.
    fn release(&self, seq: u64) {
        let deadline = Instant::now() + PHASE_LIMIT;
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        if state.released < seq {
            state.released = seq;
        }
        self.changed.notify_all();
        while state.parked && state.seq == seq {
            let now = Instant::now();
            if now >= deadline {
                break;
            }
            let (guard, _) = self
                .changed
                .wait_timeout(state, deadline - now)
                .unwrap_or_else(|poison| poison.into_inner());
            state = guard;
        }
    }
}

struct GateShutdown(Arc<Gate>);

impl Drop for GateShutdown {
    fn drop(&mut self) {
        self.0.disarm();
    }
}

fn current_native_tid() -> Option<u64> {
    let link = std::fs::read_link("/proc/thread-self").ok()?;
    let text = link.to_string_lossy().into_owned();
    let tail = text
        .rsplit("/task/")
        .next()?
        .trim_end_matches('/')
        .to_string();
    tail.parse().ok()
}

fn thread_comm(tid: u64) -> Option<String> {
    std::fs::read_to_string(format!("/proc/self/task/{tid}/comm"))
        .ok()
        .map(|name| name.trim().to_string())
}

fn task_entry_exists(tid: u64) -> bool {
    Path::new(&format!("/proc/self/task/{tid}")).exists()
}

/// A thread that has run to completion disappears from `/proc/self/task`
/// immediately, even while its `JoinHandle` is still unjoined; that is the
/// precise "the worker really returned" signal used here.
fn wait_thread_gone(tid: u64, limit: Duration) -> bool {
    let deadline = Instant::now() + limit;
    loop {
        if !task_entry_exists(tid) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// One-shot loopback fixture with a tiny complete response. It withholds the
/// response until the test has polled the VM once, so the worker is guaranteed
/// to be blocked in its real network read at that point.
struct Fixture {
    addr: SocketAddr,
    requested: Receiver<String>,
    respond: Sender<()>,
    handle: Option<JoinHandle<()>>,
}

impl Fixture {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback fixture");
        let addr = listener
            .local_addr()
            .expect("loopback fixture must have an address");
        listener
            .set_nonblocking(true)
            .expect("loopback fixture must be non-blocking");
        let (requested_tx, requested_rx) = mpsc::channel();
        let (respond_tx, respond_rx) = mpsc::channel::<()>();
        let handle = std::thread::Builder::new()
            .name("m8-http-fixture".into())
            .spawn(move || {
                let accept_deadline = Instant::now() + PHASE_LIMIT;
                let mut accepted: Option<TcpStream> = None;
                while Instant::now() < accept_deadline {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            accepted = Some(stream);
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(1));
                        }
                        Err(_) => return,
                    }
                }
                let Some(mut stream) = accepted else {
                    return;
                };
                let _ = stream.set_read_timeout(Some(EXIT_LIMIT));
                let _ = stream.set_write_timeout(Some(EXIT_LIMIT));
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while head.len() < 8192 {
                    match stream.read(&mut byte) {
                        Ok(0) => break,
                        Ok(_) => {
                            head.push(byte[0]);
                            if head.ends_with(b"\r\n\r\n") {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
                let _ = requested_tx.send(String::from_utf8_lossy(&head).into_owned());
                if respond_rx.recv_timeout(PHASE_LIMIT).is_err() {
                    return;
                }
                let response = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\
                                 Content-Length: 2\r\nConnection: close\r\n\r\nok";
                let _ = stream.write_all(response);
                let _ = stream.flush();
                let _ = stream.shutdown(Shutdown::Both);
            })
            .expect("spawn loopback fixture");
        Self {
            addr,
            requested: requested_rx,
            respond: respond_tx,
            handle: Some(handle),
        }
    }

    fn wait_requested(&mut self) {
        let head = self
            .requested
            .recv_timeout(PHASE_LIMIT)
            .expect("the HTTP worker must send its request to the fixture");
        assert!(
            head.starts_with("GET /probe "),
            "unexpected fixture request head: {head:?}"
        );
    }

    fn respond(&self) {
        let _ = self.respond.send(());
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.respond.send(());
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}
