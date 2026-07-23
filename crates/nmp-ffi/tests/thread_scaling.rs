//! #680 falsifier 1 — thread scaling, with a printed 1/12/64/1000 table and
//! genuinely-pending `next()` calls on real exported observation handles.
//!
//! Under the OLD architecture every FFI observation reserved a native-task slot
//! and spawned one dedicated OS thread that blocked on `recv()` for the
//! observation's whole life, so `NmpEngine::observe` here would have spawned
//! ~1,000 threads (and refused at the 13th under the default-12 cap). Under the
//! pull-based async design an observation is a lightweight `Arc` + waker.
//!
//! Instrumentation: `nmp::nmp_threads_spawned()` counts EVERY real NMP-owned OS
//! thread — the transient blocking-adapter pool (`nmp-executor`), the engine
//! runtime + its two bridges (`nmp-engine`), and every transport thread
//! (`nmp-transport`'s single `SystemThreadSpawner`). There is no uninstrumented
//! NMP thread source, so a "0 growth" result cannot hide a thread behind an
//! unmeasured spawn site.
//!
//! Each opened handle has a live consumer task that drains its initial frame
//! and then PARKS on a pending `next()` — proving a parked `next()` future
//! reserves no thread. The consumer tasks are async tokio tasks (a handful of
//! shared OS workers), which `nmp_threads_spawned()` does not count.
//!
//! One test per process so the global spawn counter is not perturbed by a
//! sibling test spawning its own engine.

use std::sync::Arc;
use std::time::Duration;

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpRowStream};
use nmp_ffi::types::FfiFilter;

fn text_note_query() -> FfiFilter {
    FfiFilter {
        kinds: Some(vec![1]),
        ..FfiFilter::default()
    }
}

/// A live consumer for one handle: drain the initial current-state frame(s),
/// then park on a pending `next()` for the rest of the test. Returned join
/// handle keeps the task (and its parked `next()`) alive until cancelled.
fn spawn_parked_consumer(stream: Arc<NmpRowStream>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // The first `next()` resolves with the observation's initial frame; the
        // second parks on the empty mailbox (no relays, no further changes) with
        // its waker registered — a genuinely pending `next()`.
        while let Ok(Some(_frame)) = stream.next().await {
            // keep pulling: after the initial frame(s) the next `next()` parks
        }
    })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn observation_handles_scale_with_zero_native_thread_growth() {
    let engine = NmpEngine::new(NmpEngineConfig::default()).expect("in-memory engine must build");

    let mut streams: Vec<Arc<NmpRowStream>> = Vec::new();
    let mut consumers: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    let checkpoints = [1usize, 12, 64, 1000];
    let mut table: Vec<(usize, u64)> = Vec::new();

    for &target in &checkpoints {
        while streams.len() < target {
            let stream = engine
                .observe(text_note_query(), None)
                .expect("no observation is ever refused for a capacity reason");
            consumers.push(spawn_parked_consumer(stream.clone()));
            streams.push(stream);
        }
        // Let the newly-opened handles deliver their initial frame and park.
        tokio::time::sleep(Duration::from_millis(300)).await;
        table.push((target, nmp::nmp_threads_spawned()));
    }

    let baseline = table[0].1;
    eprintln!("\n#680 thread-scaling table (all NMP-owned OS threads):");
    eprintln!(
        "  {:>12} | {:>20} | {:>14}",
        "observations", "nmp_threads_spawned", "delta_vs_first"
    );
    for (n, threads) in &table {
        eprintln!(
            "  {:>12} | {:>20} | {:>+14}",
            n,
            threads,
            *threads as i64 - baseline as i64
        );
    }
    eprintln!(
        "  (old design would read ~{} at 1000: 7 + 2*max_relays + N drain threads)\n",
        baseline + 988
    );

    assert_eq!(
        table.last().unwrap().1,
        baseline,
        "opening 1,000 observations (each with a pending next()) must create 0 \
         additional NMP OS threads; table={table:?}"
    );

    // Teardown: cancel every handle (wakes each parked next() to None) and join.
    for stream in &streams {
        stream.cancel();
    }
    for consumer in consumers {
        let _ = tokio::time::timeout(Duration::from_secs(5), consumer).await;
    }
    engine.shutdown();
}
