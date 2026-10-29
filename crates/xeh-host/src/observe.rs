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
    peak_application_bytes: AtomicUsize,
    peak_application_capacity: AtomicUsize,
    guest_payload_bytes: AtomicUsize,
    threads: Mutex<ThreadObs>,
    connect_targets: Mutex<Vec<SocketAddr>>,
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
        lock(&self.connect_targets).push(address);
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

    pub fn note_application(&self, len: usize, capacity: usize) {
        self.application_bytes.store(len, Ordering::Release);
        self.peak_application_bytes.fetch_max(len, Ordering::AcqRel);
        self.peak_application_capacity
            .fetch_max(capacity, Ordering::AcqRel);
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
}
