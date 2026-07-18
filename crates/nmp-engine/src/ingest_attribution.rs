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
    pub relay_core_reduce_ns: u64,
    pub relay_effect_dispatch_ns: u64,
    pub relay_ingest_prelude_ns: u64,
    pub relay_ingest_post_store_ns: u64,
    pub relay_ingest_apply_committed_ns: u64,
    pub relay_ingest_effect_build_ns: u64,
    pub committed_observation_effect_ns: u64,
    pub diagnostics_effect_ns: u64,
    pub committed_projection_total_ns: u64,
    pub committed_projection_prelude_ns: u64,
    pub committed_projection_recompile_ns: u64,
    pub committed_live_projection_ns: u64,
    pub committed_history_projection_ns: u64,
    pub history_projection_setup_ns: u64,
    pub history_projection_apply_ns: u64,
    pub history_projection_delta_ns: u64,
    pub history_projection_batch_ns: u64,
    pub history_sink_delivery_ns: u64,
    pub history_channel_send_ns: u64,
    pub history_receiver_reconcile_ns: u64,
    pub history_batches: u64,
    pub history_deltas: u64,
    pub history_rows: u64,
    pub row_sink_delivery_ns: u64,
    pub row_sink_batches: u64,
    pub row_sink_deltas: u64,
    pub row_channel_send_ns: u64,
    pub row_channel_batches: u64,
    pub row_channel_deltas: u64,
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
    RELAY_CORE_REDUCE_NS,
    RELAY_EFFECT_DISPATCH_NS,
    RELAY_INGEST_PRELUDE_NS,
    RELAY_INGEST_POST_STORE_NS,
    RELAY_INGEST_APPLY_COMMITTED_NS,
    RELAY_INGEST_EFFECT_BUILD_NS,
    COMMITTED_OBSERVATION_EFFECT_NS,
    DIAGNOSTICS_EFFECT_NS,
    COMMITTED_PROJECTION_TOTAL_NS,
    COMMITTED_PROJECTION_PRELUDE_NS,
    COMMITTED_PROJECTION_RECOMPILE_NS,
    COMMITTED_LIVE_PROJECTION_NS,
    COMMITTED_HISTORY_PROJECTION_NS,
    HISTORY_PROJECTION_SETUP_NS,
    HISTORY_PROJECTION_APPLY_NS,
    HISTORY_PROJECTION_DELTA_NS,
    HISTORY_PROJECTION_BATCH_NS,
    HISTORY_SINK_DELIVERY_NS,
    HISTORY_CHANNEL_SEND_NS,
    HISTORY_RECEIVER_RECONCILE_NS,
    HISTORY_BATCHES,
    HISTORY_DELTAS,
    HISTORY_ROWS,
    ROW_SINK_DELIVERY_NS,
    ROW_SINK_BATCHES,
    ROW_SINK_DELTAS,
    ROW_CHANNEL_SEND_NS,
    ROW_CHANNEL_BATCHES,
    ROW_CHANNEL_DELTAS,
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
        &RELAY_CORE_REDUCE_NS,
        &RELAY_EFFECT_DISPATCH_NS,
        &RELAY_INGEST_PRELUDE_NS,
        &RELAY_INGEST_POST_STORE_NS,
        &RELAY_INGEST_APPLY_COMMITTED_NS,
        &RELAY_INGEST_EFFECT_BUILD_NS,
        &COMMITTED_OBSERVATION_EFFECT_NS,
        &DIAGNOSTICS_EFFECT_NS,
        &COMMITTED_PROJECTION_TOTAL_NS,
        &COMMITTED_PROJECTION_PRELUDE_NS,
        &COMMITTED_PROJECTION_RECOMPILE_NS,
        &COMMITTED_LIVE_PROJECTION_NS,
        &COMMITTED_HISTORY_PROJECTION_NS,
        &HISTORY_PROJECTION_SETUP_NS,
        &HISTORY_PROJECTION_APPLY_NS,
        &HISTORY_PROJECTION_DELTA_NS,
        &HISTORY_PROJECTION_BATCH_NS,
        &HISTORY_SINK_DELIVERY_NS,
        &HISTORY_CHANNEL_SEND_NS,
        &HISTORY_RECEIVER_RECONCILE_NS,
        &HISTORY_BATCHES,
        &HISTORY_DELTAS,
        &HISTORY_ROWS,
        &ROW_SINK_DELIVERY_NS,
        &ROW_SINK_BATCHES,
        &ROW_SINK_DELTAS,
        &ROW_CHANNEL_SEND_NS,
        &ROW_CHANNEL_BATCHES,
        &ROW_CHANNEL_DELTAS,
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
        relay_core_reduce_ns: load(&RELAY_CORE_REDUCE_NS),
        relay_effect_dispatch_ns: load(&RELAY_EFFECT_DISPATCH_NS),
        relay_ingest_prelude_ns: load(&RELAY_INGEST_PRELUDE_NS),
        relay_ingest_post_store_ns: load(&RELAY_INGEST_POST_STORE_NS),
        relay_ingest_apply_committed_ns: load(&RELAY_INGEST_APPLY_COMMITTED_NS),
        relay_ingest_effect_build_ns: load(&RELAY_INGEST_EFFECT_BUILD_NS),
        committed_observation_effect_ns: load(&COMMITTED_OBSERVATION_EFFECT_NS),
        diagnostics_effect_ns: load(&DIAGNOSTICS_EFFECT_NS),
        committed_projection_total_ns: load(&COMMITTED_PROJECTION_TOTAL_NS),
        committed_projection_prelude_ns: load(&COMMITTED_PROJECTION_PRELUDE_NS),
        committed_projection_recompile_ns: load(&COMMITTED_PROJECTION_RECOMPILE_NS),
        committed_live_projection_ns: load(&COMMITTED_LIVE_PROJECTION_NS),
        committed_history_projection_ns: load(&COMMITTED_HISTORY_PROJECTION_NS),
        history_projection_setup_ns: load(&HISTORY_PROJECTION_SETUP_NS),
        history_projection_apply_ns: load(&HISTORY_PROJECTION_APPLY_NS),
        history_projection_delta_ns: load(&HISTORY_PROJECTION_DELTA_NS),
        history_projection_batch_ns: load(&HISTORY_PROJECTION_BATCH_NS),
        history_sink_delivery_ns: load(&HISTORY_SINK_DELIVERY_NS),
        history_channel_send_ns: load(&HISTORY_CHANNEL_SEND_NS),
        history_receiver_reconcile_ns: load(&HISTORY_RECEIVER_RECONCILE_NS),
        history_batches: load(&HISTORY_BATCHES),
        history_deltas: load(&HISTORY_DELTAS),
        history_rows: load(&HISTORY_ROWS),
        row_sink_delivery_ns: load(&ROW_SINK_DELIVERY_NS),
        row_sink_batches: load(&ROW_SINK_BATCHES),
        row_sink_deltas: load(&ROW_SINK_DELTAS),
        row_channel_send_ns: load(&ROW_CHANNEL_SEND_NS),
        row_channel_batches: load(&ROW_CHANNEL_BATCHES),
        row_channel_deltas: load(&ROW_CHANNEL_DELTAS),
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

pub(crate) fn relay_core_reduce(duration: Duration) {
    add(&RELAY_CORE_REDUCE_NS, duration);
}

pub(crate) fn relay_effect_dispatch(duration: Duration) {
    add(&RELAY_EFFECT_DISPATCH_NS, duration);
}

pub(crate) fn relay_ingest_prelude(duration: Duration) {
    add(&RELAY_INGEST_PRELUDE_NS, duration);
}

pub(crate) fn relay_ingest_post_store(duration: Duration) {
    add(&RELAY_INGEST_POST_STORE_NS, duration);
}

pub(crate) fn relay_ingest_apply_committed(duration: Duration) {
    add(&RELAY_INGEST_APPLY_COMMITTED_NS, duration);
}

pub(crate) fn relay_ingest_effect_build(duration: Duration) {
    add(&RELAY_INGEST_EFFECT_BUILD_NS, duration);
}

pub(crate) fn committed_observation_effect(duration: Duration) {
    add(&COMMITTED_OBSERVATION_EFFECT_NS, duration);
}

pub(crate) fn diagnostics_effect(duration: Duration) {
    add(&DIAGNOSTICS_EFFECT_NS, duration);
}

pub(crate) fn committed_projection_total(duration: Duration) {
    add(&COMMITTED_PROJECTION_TOTAL_NS, duration);
}

pub(crate) fn committed_projection_prelude(duration: Duration) {
    add(&COMMITTED_PROJECTION_PRELUDE_NS, duration);
}

pub(crate) fn committed_projection_recompile(duration: Duration) {
    add(&COMMITTED_PROJECTION_RECOMPILE_NS, duration);
}

pub(crate) fn committed_live_projection(duration: Duration) {
    add(&COMMITTED_LIVE_PROJECTION_NS, duration);
}

pub(crate) fn committed_history_projection(duration: Duration) {
    add(&COMMITTED_HISTORY_PROJECTION_NS, duration);
}

pub(crate) fn history_projection_setup(duration: Duration) {
    add(&HISTORY_PROJECTION_SETUP_NS, duration);
}

pub(crate) fn history_projection_apply(duration: Duration) {
    add(&HISTORY_PROJECTION_APPLY_NS, duration);
}

pub(crate) fn history_projection_delta(duration: Duration) {
    add(&HISTORY_PROJECTION_DELTA_NS, duration);
}

pub(crate) fn history_projection_batch(duration: Duration, deltas: usize, rows: usize) {
    add(&HISTORY_PROJECTION_BATCH_NS, duration);
    HISTORY_BATCHES.fetch_add(1, Ordering::Relaxed);
    HISTORY_DELTAS.fetch_add(deltas as u64, Ordering::Relaxed);
    HISTORY_ROWS.fetch_add(rows as u64, Ordering::Relaxed);
}

pub(crate) fn history_sink_delivery(duration: Duration) {
    add(&HISTORY_SINK_DELIVERY_NS, duration);
}

pub(crate) fn history_channel_send(duration: Duration) {
    add(&HISTORY_CHANNEL_SEND_NS, duration);
}

pub(crate) fn history_receiver_reconcile(duration: Duration) {
    add(&HISTORY_RECEIVER_RECONCILE_NS, duration);
}

pub(crate) fn row_sink_delivery(duration: Duration, deltas: usize) {
    add(&ROW_SINK_DELIVERY_NS, duration);
    ROW_SINK_BATCHES.fetch_add(1, Ordering::Relaxed);
    ROW_SINK_DELTAS.fetch_add(deltas as u64, Ordering::Relaxed);
}

pub(crate) fn row_channel_send(duration: Duration, deltas: usize) {
    add(&ROW_CHANNEL_SEND_NS, duration);
    ROW_CHANNEL_BATCHES.fetch_add(1, Ordering::Relaxed);
    ROW_CHANNEL_DELTAS.fetch_add(deltas as u64, Ordering::Relaxed);
}

pub(crate) fn projection_event_clone() {
    PROJECTION_EVENT_CLONES.fetch_add(1, Ordering::Relaxed);
}
