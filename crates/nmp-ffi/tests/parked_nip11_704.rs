//! #704 falsifier — many parked NIP-11 waits hold NO OS thread, and cancel
//! immediately.
//!
//! Start N concurrent `relay_information` fetches against N distinct
//! never-answering local relays. Each fetch establishes its TCP connection
//! (the kernel completes the handshake into the listen backlog even though the
//! server never `accept()`s) and then PARKS awaiting an HTTP response that
//! never comes, until the engine's internal fetch deadline. #704 makes every
//! such wait an async future on the shared engine runtime — there is no per-op
//! worker or permit — so N genuinely-pending fetches add ZERO OS threads.
//! Under the removed admission design each blocked adapter call held a pooled
//! OS thread (and past a bound refused with the deleted `ThreadUnavailable`).
//!
//! Then we CANCEL (abort) every pending fetch and confirm each resolves
//! immediately (well under the ~3s fetch deadline) — a parked future cancels at
//! once because it is not sitting behind any thread/permit. This is the
//! "cancellable admission — no permit exists" property in its strongest form.
//!
//! Thread count via `nmp::nmp_threads_spawned()`. One thread-counting test per
//! binary keeps the global counter isolated. The complementary parked-`next()`
//! observation property (a pending observation reserves no thread, to 1,000
//! handles) is proven by `thread_scaling.rs`.

use std::net::TcpListener;
use std::time::{Duration, Instant};

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nmp_ffi::types::FfiRelayInformationCachePolicy;

/// Concurrent pending NIP-11 fetches. Far above any worker count, so a
/// per-op-thread design would spawn ~this many OS threads.
const PENDING_FETCHES: usize = 64;

/// Thread-growth tolerance for N genuinely-pending fetches: a fixed constant
/// (host-runtime lazy-thread slack), independent of `PENDING_FETCHES`.
/// Expected actual growth is 0 — a NIP-11 fetch runs on the engine's existing
/// runtime and opens no transport pool.
const THREAD_GROWTH_BOUND: u64 = 8;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn parked_nip11_waits_hold_no_os_thread_and_cancel_immediately() {
    // N distinct never-answering relays. Distinct URLs guarantee N distinct
    // engine flights (a shared URL would coalesce into one). We keep the
    // listeners bound (in scope) but NEVER accept, so each connection parks
    // after the kernel completes its handshake.
    let mut listeners: Vec<TcpListener> = Vec::new();
    let mut urls: Vec<String> = Vec::new();
    for _ in 0..PENDING_FETCHES {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        urls.push(format!("ws://{}", listener.local_addr().unwrap()));
        listeners.push(listener);
    }

    let engine = NmpEngine::new(NmpEngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("in-memory engine must build");

    let baseline = nmp::nmp_threads_spawned();

    // Fire all N fetches concurrently; each parks awaiting a response.
    let mut fetches: Vec<tokio::task::JoinHandle<()>> = Vec::new();
    for url in &urls {
        let engine = engine.clone();
        let url = url.clone();
        fetches.push(tokio::spawn(async move {
            // Resolves (Err, deadline) if not cancelled first; we cancel below.
            let _ = engine
                .relay_information(url, FfiRelayInformationCachePolicy::Refresh)
                .await;
        }));
    }

    // Let every fetch reach its parked await (well under the ~3s deadline).
    tokio::time::sleep(Duration::from_millis(800)).await;

    // They must genuinely still be pending at measurement time.
    let still_pending = fetches.iter().filter(|f| !f.is_finished()).count();
    assert_eq!(
        still_pending, PENDING_FETCHES,
        "all {PENDING_FETCHES} fetches must still be parked when we sample threads; \
         {still_pending} were pending"
    );

    let parked = nmp::nmp_threads_spawned();
    let growth = parked.saturating_sub(baseline);
    eprintln!(
        "\n#704 parked-NIP-11 thread growth: baseline={baseline} with_{PENDING_FETCHES}_pending={parked} \
         growth={growth} (bound={THREAD_GROWTH_BOUND}, old per-op-thread design would add ~{PENDING_FETCHES})\n"
    );
    assert!(
        growth <= THREAD_GROWTH_BOUND,
        "{PENDING_FETCHES} pending NIP-11 fetches added {growth} NMP OS threads \
         (baseline={baseline}, parked={parked}); a growth proportional to the pending count \
         would mean each parked wait holds a thread/permit. Bound={THREAD_GROWTH_BOUND}."
    );

    // Cancel every parked fetch and confirm each resolves IMMEDIATELY — a
    // parked future is not behind any thread/permit, so aborting wakes it at
    // once, far under the ~3s fetch deadline.
    let cancel_start = Instant::now();
    for fetch in &fetches {
        fetch.abort();
    }
    for fetch in fetches {
        let joined = tokio::time::timeout(Duration::from_secs(2), fetch).await;
        assert!(
            joined.is_ok(),
            "a cancelled parked fetch must resolve within 2s (well under the 3s fetch deadline)"
        );
    }
    let cancel_elapsed = cancel_start.elapsed();
    eprintln!(
        "#704 parked-NIP-11 cancellation latency for {PENDING_FETCHES} fetches: {cancel_elapsed:?}\n"
    );
    assert!(
        cancel_elapsed < Duration::from_secs(2),
        "cancelling {PENDING_FETCHES} parked fetches took {cancel_elapsed:?}; parked futures must \
         cancel promptly, not wait out the fetch deadline"
    );

    drop(listeners);
    engine.shutdown();
}
