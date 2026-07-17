//! Opt-in resolver ingest attribution for evidence binaries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub batches: u64,
    pub events: u64,
    pub max_batch_events: u64,
    pub total_ns: u64,
    pub prepare_ns: u64,
    pub store_ns: u64,
    pub classify_ns: u64,
    pub react_and_affected_ns: u64,
}

macro_rules! counters { ($($name:ident),+ $(,)?) => { $(static $name: AtomicU64 = AtomicU64::new(0);)+ }; }
counters!(
    BATCHES,
    EVENTS,
    MAX_BATCH_EVENTS,
    TOTAL_NS,
    PREPARE_NS,
    STORE_NS,
    CLASSIFY_NS,
    REACT_NS
);

fn add(counter: &AtomicU64, duration: Duration) {
    counter.fetch_add(
        duration.as_nanos().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
}
pub fn reset() {
    for counter in [
        &BATCHES,
        &EVENTS,
        &MAX_BATCH_EVENTS,
        &TOTAL_NS,
        &PREPARE_NS,
        &STORE_NS,
        &CLASSIFY_NS,
        &REACT_NS,
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
        total_ns: load(&TOTAL_NS),
        prepare_ns: load(&PREPARE_NS),
        store_ns: load(&STORE_NS),
        classify_ns: load(&CLASSIFY_NS),
        react_and_affected_ns: load(&REACT_NS),
    }
}
pub(crate) fn batch(events: usize) {
    BATCHES.fetch_add(1, Ordering::Relaxed);
    EVENTS.fetch_add(events as u64, Ordering::Relaxed);
    MAX_BATCH_EVENTS.fetch_max(events as u64, Ordering::Relaxed);
}
pub(crate) fn total(duration: Duration) {
    add(&TOTAL_NS, duration);
}
pub(crate) fn prepare(duration: Duration) {
    add(&PREPARE_NS, duration);
}
pub(crate) fn store(duration: Duration) {
    add(&STORE_NS, duration);
}
pub(crate) fn classify(duration: Duration) {
    add(&CLASSIFY_NS, duration);
}
pub(crate) fn react(duration: Duration) {
    add(&REACT_NS, duration);
}
