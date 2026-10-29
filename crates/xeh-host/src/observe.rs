use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::thread::ThreadId;

use crate::dto::lock;

/// Observations taken from the running transfer, not from guessed constants.
#[derive(Debug, Default)]
pub struct Metrics {
    in_flight: AtomicUsize,
    peak_in_flight: AtomicUsize,
    permits: AtomicUsize,
    pending: AtomicUsize,
    application_bytes: AtomicUsize,
    application_capacity: AtomicUsize,
    peak_application_bytes: AtomicUsize,
    peak_application_capacity: AtomicUsize,
    guest_payload_bytes: AtomicUsize,
    threads: Mutex<ThreadObs>,
    connect_targets: Mutex<Vec<SocketAddr>>,
    dns_candidates: Mutex<Vec<SocketAddr>>,
    failure_ring: Mutex<Vec<String>>,
}

#[derive(Debug, Default)]
struct ThreadObs {
    owners: Vec<ThreadId>,
    builds: Vec<ThreadId>,
    executing: Vec<ThreadId>,
}

impl Metrics {
    pub fn record_owner(&self, id: ThreadId) {
        lock(&self.threads).owners.push(id);
    }

    pub fn record_build(&self, id: ThreadId) {
        lock(&self.threads).builds.push(id);
    }

    pub fn record_execution(&self, id: ThreadId) {
        lock(&self.threads).executing.push(id);
    }

    pub fn record_connect(&self, address: SocketAddr) {
        let mut targets = lock(&self.connect_targets);
        if targets.len() == 32 {
            targets.remove(0);
        }
        targets.push(address);
    }

    pub fn record_dns_candidates(&self, candidates: &[SocketAddr]) {
        *lock(&self.dns_candidates) = candidates.to_vec();
    }

    pub fn record_failure(&self, code: &str) {
        let mut ring = lock(&self.failure_ring);
        if ring.len() == 32 {
            ring.remove(0);
        }
        ring.push(code.to_string());
    }

    pub fn begin_flight(&self) {
        let now = self.in_flight.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak_in_flight.fetch_max(now, Ordering::AcqRel);
    }

    pub fn end_flight(&self) {
        self.in_flight.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn begin_permit(&self) {
        self.permits.fetch_add(1, Ordering::AcqRel);
    }

    pub fn end_permit(&self) {
        self.permits.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn pending_add(&self) {
        self.pending.fetch_add(1, Ordering::AcqRel);
    }

    pub fn pending_sub(&self) {
        self.pending.fetch_sub(1, Ordering::AcqRel);
    }

    pub fn acquire_hold(&self, len: usize, capacity: usize) {
        let held = self.application_bytes.fetch_add(len, Ordering::AcqRel) + len;
        let caps = self
            .application_capacity
            .fetch_add(capacity, Ordering::AcqRel)
            + capacity;
        self.peak_application_bytes
            .fetch_max(held, Ordering::AcqRel);
        self.peak_application_capacity
            .fetch_max(caps, Ordering::AcqRel);
    }

    pub fn release_hold(&self, len: usize, capacity: usize) {
        self.application_bytes.fetch_sub(len, Ordering::AcqRel);
        self.application_capacity
            .fetch_sub(capacity, Ordering::AcqRel);
    }

    pub fn set_guest_payload(&self, len: usize) {
        self.guest_payload_bytes.fetch_max(len, Ordering::AcqRel);
    }

    pub fn in_flight(&self) -> usize {
        self.in_flight.load(Ordering::Acquire)
    }

    pub fn peak_in_flight(&self) -> usize {
        self.peak_in_flight.load(Ordering::Acquire)
    }

    pub fn permits(&self) -> usize {
        self.permits.load(Ordering::Acquire)
    }

    pub fn pending(&self) -> usize {
        self.pending.load(Ordering::Acquire)
    }

    pub fn application_bytes(&self) -> usize {
        self.application_bytes.load(Ordering::Acquire)
    }

    pub fn peak_application_bytes(&self) -> usize {
        self.peak_application_bytes.load(Ordering::Acquire)
    }

    pub fn peak_application_capacity(&self) -> usize {
        self.peak_application_capacity.load(Ordering::Acquire)
    }

    pub fn guest_payload_bytes(&self) -> usize {
        self.guest_payload_bytes.load(Ordering::Acquire)
    }

    pub fn owners(&self) -> Vec<ThreadId> {
        lock(&self.threads).owners.clone()
    }

    pub fn builds(&self) -> Vec<ThreadId> {
        lock(&self.threads).builds.clone()
    }

    pub fn executing(&self) -> Vec<ThreadId> {
        lock(&self.threads).executing.clone()
    }

    pub fn connect_targets(&self) -> Vec<SocketAddr> {
        lock(&self.connect_targets).clone()
    }

    pub fn dns_candidates(&self) -> Vec<SocketAddr> {
        lock(&self.dns_candidates).clone()
    }

    pub fn failure_codes(&self) -> usize {
        lock(&self.failure_ring).len()
    }
}
