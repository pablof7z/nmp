//! #680 falsifier 1 — thread scaling.
//!
//! Under the OLD architecture every FFI observation reserved a native-task slot
//! and spawned one dedicated OS thread that blocked on `recv()` for the
//! observation's whole life, so `NmpEngine::observe` would have spawned ~1,000
//! threads here (and refused at the 13th under the default-12 cap). Under the
//! pull-based async design an observation is a lightweight `Arc` + waker; this
//! test asserts opening 1,000 of them spawns ZERO additional NMP-owned OS
//! threads — instrumented directly via `nmp::nmp_threads_spawned()`, not a
//! fragile process-wide count.
//!
//! This file holds exactly one test so the process-global spawn counter is not
//! perturbed by a sibling test spawning its own engine concurrently.

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nmp_ffi::types::FfiFilter;

fn text_note_query() -> FfiFilter {
    FfiFilter {
        kinds: Some(vec![1]),
        ..FfiFilter::default()
    }
}

#[test]
fn opening_one_thousand_observations_spawns_no_native_thread_per_observation() {
    let engine = NmpEngine::new(NmpEngineConfig::default()).expect("in-memory engine must build");

    // Baseline AFTER construction + one observation, so fixed infrastructure
    // (engine runtime, bridges, transport, executor reaper) is already counted.
    let first = engine
        .observe(text_note_query(), None)
        .expect("first observation opens");
    let baseline = nmp::nmp_threads_spawned();

    let mut streams = Vec::with_capacity(1_000);
    for _ in 0..1_000 {
        streams.push(
            engine
                .observe(text_note_query(), None)
                .expect("no observation is ever refused for a capacity reason"),
        );
    }

    let after = nmp::nmp_threads_spawned();
    assert_eq!(
        after, baseline,
        "opening 1,000 observations must create 0 additional NMP OS threads \
         (was O(1000) under the one-thread-per-observer design); \
         baseline={baseline} after={after}"
    );
    assert_eq!(streams.len(), 1_000);

    // Hold everything until here so nothing is withdrawn early.
    drop(first);
    drop(streams);
    engine.shutdown();
}
