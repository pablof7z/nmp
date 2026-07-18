//! Opt-in resolver ingest attribution for evidence binaries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub batches: u64,
    pub events: u64,
    pub max_batch_events: u64,
    pub total_ns: u64,
    pub total_cpu_ns: u64,
    pub prepare_ns: u64,
    pub prepare_cpu_ns: u64,
    pub store_ns: u64,
    pub store_cpu_ns: u64,
    pub classify_ns: u64,
    pub classify_cpu_ns: u64,
    pub react_and_affected_ns: u64,
    pub react_and_affected_cpu_ns: u64,
    pub event_clones: u64,
}

macro_rules! counters { ($($name:ident),+ $(,)?) => { $(static $name: AtomicU64 = AtomicU64::new(0);)+ }; }
counters!(
    BATCHES,
    EVENTS,
    MAX_BATCH_EVENTS,
    TOTAL_NS,
    TOTAL_CPU_NS,
    PREPARE_NS,
    PREPARE_CPU_NS,
    STORE_NS,
    STORE_CPU_NS,
    CLASSIFY_NS,
    CLASSIFY_CPU_NS,
    REACT_NS,
    REACT_CPU_NS,
    EVENT_CLONES
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
        &TOTAL_CPU_NS,
        &PREPARE_NS,
        &PREPARE_CPU_NS,
        &STORE_NS,
        &STORE_CPU_NS,
        &CLASSIFY_NS,
        &CLASSIFY_CPU_NS,
        &REACT_NS,
        &REACT_CPU_NS,
        &EVENT_CLONES,
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
        total_cpu_ns: load(&TOTAL_CPU_NS),
        prepare_ns: load(&PREPARE_NS),
        prepare_cpu_ns: load(&PREPARE_CPU_NS),
        store_ns: load(&STORE_NS),
        store_cpu_ns: load(&STORE_CPU_NS),
        classify_ns: load(&CLASSIFY_NS),
        classify_cpu_ns: load(&CLASSIFY_CPU_NS),
        react_and_affected_ns: load(&REACT_NS),
        react_and_affected_cpu_ns: load(&REACT_CPU_NS),
        event_clones: load(&EVENT_CLONES),
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
pub(crate) fn total_cpu(nanos: u64) {
    TOTAL_CPU_NS.fetch_add(nanos, Ordering::Relaxed);
}
pub(crate) fn prepare(duration: Duration) {
    add(&PREPARE_NS, duration);
}
pub(crate) fn prepare_cpu(nanos: u64) {
    PREPARE_CPU_NS.fetch_add(nanos, Ordering::Relaxed);
}
pub(crate) fn store(duration: Duration) {
    add(&STORE_NS, duration);
}
pub(crate) fn store_cpu(nanos: u64) {
    STORE_CPU_NS.fetch_add(nanos, Ordering::Relaxed);
}
pub(crate) fn classify(duration: Duration) {
    add(&CLASSIFY_NS, duration);
}
pub(crate) fn classify_cpu(nanos: u64) {
    CLASSIFY_CPU_NS.fetch_add(nanos, Ordering::Relaxed);
}
pub(crate) fn react(duration: Duration) {
    add(&REACT_NS, duration);
}
pub(crate) fn react_cpu(nanos: u64) {
    REACT_CPU_NS.fetch_add(nanos, Ordering::Relaxed);
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) fn thread_cpu_time_ns() -> u64 {
    let mut value = std::mem::MaybeUninit::<libc::timespec>::uninit();
    // SAFETY: `clock_gettime` initializes the owned `timespec` on success.
    let result = unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, value.as_mut_ptr()) };
    if result != 0 {
        return 0;
    }
    // SAFETY: the successful call above initialized `value`.
    let value = unsafe { value.assume_init() };
    u64::try_from(value.tv_sec)
        .unwrap_or(u64::MAX)
        .saturating_mul(1_000_000_000)
        .saturating_add(u64::try_from(value.tv_nsec).unwrap_or(0))
}

#[cfg(target_arch = "wasm32")]
pub(crate) fn thread_cpu_time_ns() -> u64 {
    0
}

pub(crate) fn event_clone() {
    EVENT_CLONES.fetch_add(1, Ordering::Relaxed);
}
