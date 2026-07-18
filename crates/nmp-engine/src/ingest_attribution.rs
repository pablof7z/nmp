//! Opt-in engine ingest attribution for evidence binaries.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default)]
pub struct Snapshot {
    pub bridge_batches: u64,
    pub bridge_frames: u64,
    pub max_bridge_batch: u64,
    pub bridge_event_bytes: u64,
    pub max_bridge_batch_bytes: u64,
    pub bridge_send_ns: u64,
    pub bridge_applied_wait_ns: u64,
    pub engine_batch_process_ns: u64,
    pub projection_event_clones: u64,
}

macro_rules! counters { ($($name:ident),+ $(,)?) => { $(static $name: AtomicU64 = AtomicU64::new(0);)+ }; }
counters!(
    BRIDGE_BATCHES,
    BRIDGE_FRAMES,
    MAX_BRIDGE_BATCH,
    BRIDGE_EVENT_BYTES,
    MAX_BRIDGE_BATCH_BYTES,
    BRIDGE_SEND_NS,
    BRIDGE_APPLIED_WAIT_NS,
    ENGINE_BATCH_PROCESS_NS,
    PROJECTION_EVENT_CLONES
);
fn add(counter: &AtomicU64, duration: Duration) {
    counter.fetch_add(
        duration.as_nanos().min(u64::MAX as u128) as u64,
        Ordering::Relaxed,
    );
}
pub fn reset() {
    for counter in [
        &BRIDGE_BATCHES,
        &BRIDGE_FRAMES,
        &MAX_BRIDGE_BATCH,
        &BRIDGE_EVENT_BYTES,
        &MAX_BRIDGE_BATCH_BYTES,
        &BRIDGE_SEND_NS,
        &BRIDGE_APPLIED_WAIT_NS,
        &ENGINE_BATCH_PROCESS_NS,
        &PROJECTION_EVENT_CLONES,
    ] {
        counter.store(0, Ordering::Relaxed);
    }
    nmp_transport::ingest_attribution::reset();
    nmp_resolver::ingest_attribution::reset();
    nmp_store::ingest_attribution::reset();
}
pub fn snapshot() -> Snapshot {
    let load = |counter: &AtomicU64| counter.load(Ordering::Relaxed);
    Snapshot {
        bridge_batches: load(&BRIDGE_BATCHES),
        bridge_frames: load(&BRIDGE_FRAMES),
        max_bridge_batch: load(&MAX_BRIDGE_BATCH),
        bridge_event_bytes: load(&BRIDGE_EVENT_BYTES),
        max_bridge_batch_bytes: load(&MAX_BRIDGE_BATCH_BYTES),
        bridge_send_ns: load(&BRIDGE_SEND_NS),
        bridge_applied_wait_ns: load(&BRIDGE_APPLIED_WAIT_NS),
        engine_batch_process_ns: load(&ENGINE_BATCH_PROCESS_NS),
        projection_event_clones: load(&PROJECTION_EVENT_CLONES),
    }
}
pub(crate) fn bridge_batch(frames: usize, event_bytes: usize) {
    BRIDGE_BATCHES.fetch_add(1, Ordering::Relaxed);
    BRIDGE_FRAMES.fetch_add(frames as u64, Ordering::Relaxed);
    MAX_BRIDGE_BATCH.fetch_max(frames as u64, Ordering::Relaxed);
    BRIDGE_EVENT_BYTES.fetch_add(event_bytes as u64, Ordering::Relaxed);
    MAX_BRIDGE_BATCH_BYTES.fetch_max(event_bytes as u64, Ordering::Relaxed);
}
pub(crate) fn bridge_send(duration: Duration) {
    add(&BRIDGE_SEND_NS, duration);
}
pub(crate) fn bridge_applied_wait(duration: Duration) {
    add(&BRIDGE_APPLIED_WAIT_NS, duration);
}
pub(crate) fn engine_batch_process(duration: Duration) {
    add(&ENGINE_BATCH_PROCESS_NS, duration);
}

pub(crate) fn projection_event_clone() {
    PROJECTION_EVENT_CLONES.fetch_add(1, Ordering::Relaxed);
}
