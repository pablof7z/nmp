//! Opt-in ingest attribution counters for evidence binaries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub batches: u64,
    pub events: u64,
    pub max_batch_events: u64,
    pub transaction_total_ns: u64,
    pub begin_write_ns: u64,
    pub open_tables_ns: u64,
    pub apply_events_ns: u64,
    pub flush_ns: u64,
    pub commit_ns: u64,
    pub encode_event_ns: u64,
    pub encoded_event_bytes: u64,
    pub canonical_insert_ns: u64,
    pub index_insert_ns: u64,
}

macro_rules! counters {
    ($($name:ident),+ $(,)?) => { $(static $name: AtomicU64 = AtomicU64::new(0);)+ };
}

counters!(
    BATCHES,
    EVENTS,
    MAX_BATCH_EVENTS,
    TRANSACTION_TOTAL_NS,
    BEGIN_WRITE_NS,
    OPEN_TABLES_NS,
    APPLY_EVENTS_NS,
    FLUSH_NS,
    COMMIT_NS,
    ENCODE_EVENT_NS,
    ENCODED_EVENT_BYTES,
    CANONICAL_INSERT_NS,
    INDEX_INSERT_NS,
);

fn ns(duration: Duration) -> u64 {
    duration.as_nanos().min(u64::MAX as u128) as u64
}

fn add(counter: &AtomicU64, duration: Duration) {
    counter.fetch_add(ns(duration), Ordering::Relaxed);
}

pub fn reset() {
    for counter in [
        &BATCHES,
        &EVENTS,
        &MAX_BATCH_EVENTS,
        &TRANSACTION_TOTAL_NS,
        &BEGIN_WRITE_NS,
        &OPEN_TABLES_NS,
        &APPLY_EVENTS_NS,
        &FLUSH_NS,
        &COMMIT_NS,
        &ENCODE_EVENT_NS,
        &ENCODED_EVENT_BYTES,
        &CANONICAL_INSERT_NS,
        &INDEX_INSERT_NS,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
}

pub fn snapshot() -> Snapshot {
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    Snapshot {
        batches: load(&BATCHES),
        events: load(&EVENTS),
        max_batch_events: load(&MAX_BATCH_EVENTS),
        transaction_total_ns: load(&TRANSACTION_TOTAL_NS),
        begin_write_ns: load(&BEGIN_WRITE_NS),
        open_tables_ns: load(&OPEN_TABLES_NS),
        apply_events_ns: load(&APPLY_EVENTS_NS),
        flush_ns: load(&FLUSH_NS),
        commit_ns: load(&COMMIT_NS),
        encode_event_ns: load(&ENCODE_EVENT_NS),
        encoded_event_bytes: load(&ENCODED_EVENT_BYTES),
        canonical_insert_ns: load(&CANONICAL_INSERT_NS),
        index_insert_ns: load(&INDEX_INSERT_NS),
    }
}

pub(crate) fn record_batch(events: usize) {
    BATCHES.fetch_add(1, Ordering::Relaxed);
    EVENTS.fetch_add(events as u64, Ordering::Relaxed);
    MAX_BATCH_EVENTS.fetch_max(events as u64, Ordering::Relaxed);
}

pub(crate) fn transaction_total(duration: Duration) {
    add(&TRANSACTION_TOTAL_NS, duration);
}
pub(crate) fn begin_write(duration: Duration) {
    add(&BEGIN_WRITE_NS, duration);
}
pub(crate) fn open_tables(duration: Duration) {
    add(&OPEN_TABLES_NS, duration);
}
pub(crate) fn apply_events(duration: Duration) {
    add(&APPLY_EVENTS_NS, duration);
}
pub(crate) fn flush(duration: Duration) {
    add(&FLUSH_NS, duration);
}
pub(crate) fn commit(duration: Duration) {
    add(&COMMIT_NS, duration);
}
pub(crate) fn encode_event(duration: Duration, bytes: usize) {
    add(&ENCODE_EVENT_NS, duration);
    ENCODED_EVENT_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
}
pub(crate) fn canonical_insert(duration: Duration) {
    add(&CANONICAL_INSERT_NS, duration);
}
pub(crate) fn index_insert(duration: Duration) {
    add(&INDEX_INSERT_NS, duration);
}
