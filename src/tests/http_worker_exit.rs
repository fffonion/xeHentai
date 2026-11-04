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
//! the race. Each poll is tied to the specific park sequence that triggered
//! it, and the park is released only after that poll.
//!
//! Verdicts ([`Report::diagnosis`]):
//!
//! * `LostWake` (`panic!`): after the owned worker OS thread is observed gone
//!   (`/proc/self/task/<tid>` gone with `NotFound`; tid captured inside the
//!   worker's own wake callback and matched against the `rustscript-http`
//!   thread name) the await is still `Pending`, the wake count is identical at
//!   the last `Pending` poll, at thread exit and after the single diagnostic
//!   poll, no wake was delivered on the polling thread, and every delivered
//!   wake was parked and consumed. One explicit diagnostic poll then completes
//!   the real response, so a wake-driven consumer would never have been woken.
//! * `NoLostWake`: a delivered wake already completed the await.
//! * `Inconclusive` (`panic!`): the schedule did not establish the claim. This
//!   includes *any* wake delivered on the polling thread (an
//!   `AtomicWaker::register` retry is a real re-schedule notification the test
//!   cannot park and must not swallow), any wake count change between the last
//!   `Pending` poll and the exit observation, any delivered wake that was never
//!   parked/consumed, any non-`NotFound` `/proc` error while observing the
//!   owned tid, and a diagnostic poll that is not `Ready(Ok)`. An inconclusive
//!   run is never reported as a lost wake.
//!
//! The fixture never releases the response before the first poll and every
//! gate/wait/thread join is bounded, so the schedule is deterministic rather
//! than timeout-dependent.
//!
//! The three `#[test]` controls at the bottom run without any HTTP traffic:
//! a polling-thread wake cannot produce a lost-wake verdict, the `/proc`
//! task-entry classifier separates `NotFound` from other I/O errors, and
//! `GateShutdown` releases a callback that is already parked.

#![cfg(target_os = "linux")]

use std::io::{self, Read, Write};
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
    let tid = report
        .worker_tid
        .expect("the owned HTTP worker must have delivered at least one wake");
    assert_eq!(
        report.worker_comm.as_deref(),
        Some(WORKER_THREAD_COMM),
        "the parked wake must come from the owned HTTP worker thread\n{detail}"
    );

    // Complete the real response through the public SDK; the status text is
    // part of the evidence for every verdict, including inconclusive.
    let status_text = finish_and_read_status(&mut vm);
    let (verdict, reason) = report.diagnosis();

    match verdict {
        Verdict::Inconclusive => panic!(
            "probe inconclusive: {reason}. No lost-wake conclusion is drawn \
             from this run.\n{detail}"
        ),
        Verdict::LostWake => panic!(
            "LOST TERMINAL WAKE: {reason}. One explicit diagnostic poll then \
             delivered the real response (status {status_text}); a wake-driven \
             consumer such as the production updater `call_waiting` is \
             therefore not woken after the worker's last quiescence wake: \
             `is_quiescent()` also requires `JoinHandle::is_finished()`, and \
             nothing wakes `quiescence_waker` once the thread really returns \
             (request.rs 638-644 / 696-734 / 908-923 / 1210-1213). \
             worker_tid={tid}, comm={comm:?}, parked_wakes={parks}, \
             main_thread_wakes={main}.\n{detail}",
            comm = report.worker_comm,
            parks = report.parked_wakes,
            main = report.main_thread_wakes,
        ),
        Verdict::NoLostWake => {
            // Falsified on this schedule: the real response still completed.
            assert_eq!(
                status_text, "200",
                "the real public HTTP host operation must return the fixture \
                 status\n{detail}"
            );
        }
    }
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Verdict {
    LostWake,
    NoLostWake,
    Inconclusive,
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
    /// Wake count observed at the last poll that returned `Pending`.
    wakes_at_last_pending: usize,
    /// Wake count observed once the owned worker thread was gone.
    wakes_at_exit: usize,
    /// Wake count after the single diagnostic poll; must equal
    /// `wakes_at_exit` for the lost-wake verdict.
    wakes_after_nudge: usize,
    pending_after_last_wake: bool,
    await_ready: bool,
    nudge: Option<String>,
    timeout: bool,
    /// Non-`NotFound` `/proc` failure while observing the owned tid; never
    /// treated as "the thread exited".
    exit_error: Option<String>,
}

impl Report {
    fn describe(&self) -> String {
        format!(
            "parked_wakes={} main_thread_wakes={} polls={} worker_tid={:?} \
             worker_comm={:?} worker_exited={} exit_error={:?} \
             wakes_at_last_pending={} wakes_at_exit={} wakes_after_nudge={} \
             pending_after_last_wake={} await_ready={} nudge={:?}\nphases:\n  {}",
            self.parked_wakes,
            self.main_thread_wakes,
            self.polls,
            self.worker_tid,
            self.worker_comm,
            self.worker_exited,
            self.exit_error,
            self.wakes_at_last_pending,
            self.wakes_at_exit,
            self.wakes_after_nudge,
            self.pending_after_last_wake,
            self.await_ready,
            self.nudge,
            self.phases.join("\n  ")
        )
    }

    /// Classifies the run.
    ///
    /// A lost-wake verdict needs *all* of: no wake on the polling thread, an
    /// unchanged wake count between the last `Pending` poll and the exit
    /// observation, every delivered wake parked and consumed by the poll that
    /// handled its park sequence, no wake during the diagnostic poll, the owned
    /// thread confirmed gone through `NotFound`, the await still `Pending`, and
    /// one diagnostic poll that is `Ready(Ok)`.
    ///
    /// Anything else is [`Verdict::Inconclusive`]: in particular a wake
    /// delivered on the polling thread is a real re-schedule notification (an
    /// `AtomicWaker::register` retry) that cannot be parked, so a silent await
    /// afterwards cannot be attributed to Core.
    fn diagnosis(&self) -> (Verdict, String) {
        if self.timeout {
            return (
                Verdict::Inconclusive,
                format!("no gated worker wake or worker exit was observed within {PHASE_LIMIT:?}"),
            );
        }
        if let Some(error) = &self.exit_error {
            return (
                Verdict::Inconclusive,
                format!(
                    "the thread-exit observation failed with an I/O error instead of \
                     NotFound: {error}"
                ),
            );
        }
        let Some(tid) = self.worker_tid else {
            return (
                Verdict::Inconclusive,
                "no worker wake was ever delivered, so the schedule was not exercised".to_string(),
            );
        };
        if self.main_thread_wakes != 0 {
            return (
                Verdict::Inconclusive,
                format!(
                    "{} wake(s) were delivered on the polling thread; that notification \
                     must re-poll the task and cannot be parked, so a silent await after \
                     exit cannot be attributed to Core",
                    self.main_thread_wakes
                ),
            );
        }
        if self.wakes_at_exit != self.wakes_at_last_pending {
            return (
                Verdict::Inconclusive,
                format!(
                    "the wake count changed from {} at the last Pending poll to {} when \
                     the worker thread was observed gone",
                    self.wakes_at_last_pending, self.wakes_at_exit
                ),
            );
        }
        if self.wakes_at_exit != self.parked_wakes + self.main_thread_wakes {
            return (
                Verdict::Inconclusive,
                format!(
                    "{} wake(s) were delivered but only {} parked and {} polling-thread \
                     wake(s) were handled",
                    self.wakes_at_exit, self.parked_wakes, self.main_thread_wakes
                ),
            );
        }
        if self.wakes_after_nudge != self.wakes_at_exit {
            return (
                Verdict::Inconclusive,
                format!(
                    "the diagnostic poll itself received {} new wake(s); the work was \
                     signalled after all",
                    self.wakes_after_nudge.saturating_sub(self.wakes_at_exit)
                ),
            );
        }
        if !self.pending_after_last_wake {
            return (
                Verdict::NoLostWake,
                format!(
                    "a delivered wake already completed the public await (worker tid \
                     {tid}), so no wake was lost on this schedule"
                ),
            );
        }
        if !self.worker_exited {
            return (
                Verdict::Inconclusive,
                format!("worker tid {tid} was still present after the bounded exit wait"),
            );
        }
        match self.nudge.as_deref() {
            Some("Ready(Ok)") => (
                Verdict::LostWake,
                format!(
                    "the pinned HTTP worker (tid {tid}) woke the waiting VM {} time(s) and \
                     then its OS thread fully exited while the public await was still \
                     Pending (wakes_at_last_pending={}, wakes_at_exit={}) with no \
                     polling-thread wake and no unhandled delivery; nothing notifies the \
                     await after the thread returns",
                    self.parked_wakes, self.wakes_at_last_pending, self.wakes_at_exit
                ),
            ),
            Some(other) => (
                Verdict::Inconclusive,
                format!(
                    "the single diagnostic poll after exit returned {other} instead of \
                     Ready(Ok), so completed work waiting for a wake is not established"
                ),
            ),
            None => (
                Verdict::Inconclusive,
                "the worker exited while the await was not pending, so there was nothing \
                 to nudge"
                    .to_string(),
            ),
        }
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
    if report.pending_after_last_wake {
        report.wakes_at_last_pending = gate.wake_count();
    }
    if first.is_ready() {
        report.await_ready = true;
        report.wakes_at_exit = gate.wake_count();
        report.wakes_after_nudge = report.wakes_at_exit;
        return report;
    }

    // Release the response only after the VM registered its result waker.
    fixture.respond();

    let mut consumed = 0u64;
    loop {
        match gate.wait_park(consumed, report.worker_tid, PHASE_LIMIT) {
            ParkOutcome::Parked { seq, tid, comm } => {
                // This poll is tied to exactly this park sequence, and the
                // release below is the only way that callback can resume.
                consumed = seq;
                if report.worker_tid.is_none() {
                    report.worker_tid = tid;
                    report.worker_comm = comm.clone();
                }
                report.polls += 1;
                let wakes = gate.wake_count();
                let result = fut.as_mut().poll(&mut cx);
                report.pending_after_last_wake = result.is_pending();
                if result.is_pending() {
                    report.wakes_at_last_pending = wakes;
                }
                report.phases.push(format!(
                    "poll{} after worker wake#{wakes} (park {seq} consumed) on tid \
                     {tid:?} ({}) -> {}",
                    report.polls,
                    comm.as_deref().unwrap_or("?"),
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
            ParkOutcome::ThreadError(error) => {
                report.exit_error = Some(error);
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
    if let Some(tid) = report.worker_tid
        && !report.worker_exited
    {
        match wait_thread_gone(tid, EXIT_LIMIT) {
            Ok(true) => report.worker_exited = true,
            Ok(false) => {}
            Err(error) => report.exit_error = Some(format!("{error}")),
        }
    }
    report.wakes_at_exit = gate.wake_count();

    if report.worker_exited && report.pending_after_last_wake && !report.await_ready {
        // The one explicit diagnostic poll the contract allows: it must not be
        // counted as normal wake-driven completion.
        report.polls += 1;
        let nudge = fut.as_mut().poll(&mut cx);
        report.phases.push(format!(
            "diagnostic nudge: poll{} after the worker thread exited with no wake -> {}",
            report.polls,
            poll_text(&nudge)
        ));
        report.await_ready = nudge.is_ready();
        report.nudge = Some(poll_text(&nudge));
    }
    // Final accounting: a delivery during the diagnostic poll would mean the
    // completed work was signalled after all.
    report.wakes_after_nudge = gate.wake_count();
    report.main_thread_wakes = gate.main_thread_wake_count();
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
    /// The strict `/proc` observation failed with a non-`NotFound` I/O error.
    ThreadError(String),
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
/// bounded so a wake callback can never block forever, and a disarmed gate
/// releases a parked callback at once.
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
            // `AtomicWaker::register` retry). It must re-poll the task and
            // cannot be parked: the polling thread is the only one that can
            // release the gate. It is counted so the run becomes inconclusive
            // instead of silently swallowing a valid notification.
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

        // A disarmed gate must release an already-parked callback immediately:
        // `disarm` clears `gating` and notifies, so unwind cleanup never waits
        // for the park timeout.
        let deadline = Instant::now() + WAKE_PARK_LIMIT;
        while state.released < seq && self.gating.load(Ordering::SeqCst) {
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
                match task_entry_state(tid) {
                    Ok(true) => {}
                    Ok(false) => return ParkOutcome::Exited,
                    Err(error) => return ParkOutcome::ThreadError(format!("{error}")),
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
        // Disarms *and* notifies, so a callback already parked in `on_wake`
        // leaves the park immediately instead of waiting for its timeout.
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

/// Strict classification of one `/proc` task entry.
///
/// `Ok(true)`  — the entry is still present (the thread has not returned).
/// `Ok(false)` — `NotFound`, i.e. the thread function has returned.
/// `Err(..)`   — any other I/O failure (`PermissionDenied`, `ENOTDIR`, ...).
///
/// `Path::exists()` collapses all three into a bool and would report a
/// permission or I/O failure as "the thread exited", which must never be the
/// basis of a lost-wake verdict.
fn classify_task_entry(path: &Path) -> io::Result<bool> {
    match std::fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

fn task_entry_state(tid: u64) -> io::Result<bool> {
    classify_task_entry(Path::new(&format!("/proc/self/task/{tid}")))
}

/// A thread that has run to completion disappears from `/proc/self/task`
/// immediately, even while its `JoinHandle` is still unjoined; that is the
/// precise "the worker really returned" signal used here. Only `NotFound`
/// counts as gone; any other I/O error is returned and makes the run
/// inconclusive.
fn wait_thread_gone(tid: u64, limit: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + limit;
    loop {
        if !task_entry_state(tid)? {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// Control (PASS expected): a wake delivered on the polling thread is a real
/// re-schedule notification the probe cannot park or consume, so it must make
/// the run inconclusive and can never produce a lost-wake verdict.
///
/// This is a pure classification control: its counters describe the rules, not
/// the Core schedule, and it drives no HTTP request.
#[test]
fn polling_thread_wake_cannot_be_classified_as_lost_wake() {
    let gate = Gate::new();
    gate.arm();
    gate.on_wake(); // this thread is `Gate::main_tid`
    assert_eq!(gate.wake_count(), 1);
    assert_eq!(gate.main_thread_wake_count(), 1);
    assert_eq!(gate.parked_count(), 0);

    let mut report = Report {
        worker_tid: Some(4242),
        worker_comm: Some(WORKER_THREAD_COMM.into()),
        worker_exited: true,
        parked_wakes: 0,
        main_thread_wakes: gate.main_thread_wake_count(),
        wakes_at_last_pending: 1,
        wakes_at_exit: 1,
        wakes_after_nudge: 1,
        pending_after_last_wake: true,
        await_ready: true,
        nudge: Some("Ready(Ok)".into()),
        ..Report::default()
    };
    let (verdict, reason) = report.diagnosis();
    assert_eq!(
        verdict,
        Verdict::Inconclusive,
        "a polling-thread wake must never produce a lost-wake verdict: {reason}"
    );
    assert!(reason.contains("polling thread"), "{reason}");

    // The same accounting with no polling-thread wake does reach the lost-wake
    // verdict, so the guard is specific rather than blanket weakening.
    report.main_thread_wakes = 0;
    report.parked_wakes = 1;
    assert_eq!(report.diagnosis().0, Verdict::LostWake);

    // One extra delivery between the last Pending poll and the exit
    // observation is inconclusive as well.
    report.wakes_at_exit = 2;
    let (verdict, reason) = report.diagnosis();
    assert_eq!(verdict, Verdict::Inconclusive, "{reason}");
    assert!(reason.contains("wake count changed"), "{reason}");
}

/// Control (PASS expected): only `NotFound` means the owned thread returned;
/// other I/O errors must be surfaced instead of being swallowed as "gone".
#[test]
fn task_entry_classification_never_treats_io_errors_as_exit() {
    let own_tid = current_native_tid().expect("this test thread has a native tid");
    assert!(
        task_entry_state(own_tid).expect("this thread's own task entry must be readable"),
        "a live task entry must classify as present"
    );
    assert!(
        !classify_task_entry(Path::new("m8-http-worker-exit-absent-task-entry"))
            .expect("a missing path is NotFound, not an I/O error"),
        "a missing task entry must classify as gone"
    );
    // `/proc/self/status` is a regular file, so `/proc/self/status/x` fails
    // with ENOTDIR: a non-NotFound error that `Path::exists()` reports as
    // "gone".
    let error = classify_task_entry(Path::new("/proc/self/status/x"))
        .expect_err("a non-NotFound I/O error must not be reported as thread exit");
    assert_ne!(error.kind(), io::ErrorKind::NotFound, "{error}");
}

/// Control (PASS expected): dropping the RAII guard releases a callback that is
/// already parked inside its wake, instead of leaving it for the park timeout
/// (the unwind path the review flagged).
#[test]
fn shutdown_releases_an_already_parked_wake_callback() {
    let gate = Arc::new(Gate::new());
    gate.arm();
    let parked_gate = Arc::clone(&gate);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let callback = std::thread::Builder::new()
        .name("m8-parked-callback".into())
        .spawn(move || {
            parked_gate.on_wake();
            let _ = done_tx.send(());
        })
        .expect("spawn the parked callback thread");

    match gate.wait_park(0, None, PHASE_LIMIT) {
        ParkOutcome::Parked { seq, .. } => assert_eq!(seq, 1, "the first delivery must park"),
        other => panic!("the callback never parked: {other:?}"),
    }
    assert_eq!(gate.parked_count(), 1);

    drop(GateShutdown(Arc::clone(&gate)));
    done_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("a disarmed gate must release an already-parked callback at once");
    callback
        .join()
        .expect("the parked callback thread must exit cleanly");
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
