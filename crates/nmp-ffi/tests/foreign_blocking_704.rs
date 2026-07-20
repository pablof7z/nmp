//! #704 review falsifier — "category 3 is empty": moving every executor user
//! onto the shared engine runtime did NOT relocate synchronous FOREIGN blocking
//! onto a runtime worker.
//!
//! # The claim under review
//!
//! #704 eliminated the internal task/thread admission-capacity concept and made
//! every logical operation an async task on ONE shared, fixed-width tokio
//! runtime — `ADAPTER_RUNTIME_WORKERS == 2` workers (see
//! `nmp-engine/src/runtime/mod.rs`). The review then asks a sharp question:
//! when the old dedicated per-operation threads went away, did any place where a
//! FOREIGN callback can run arbitrary, genuinely-blocking code get moved *onto*
//! one of those two workers? If it did, a blocking foreign callback would hold a
//! worker, and blocking `ADAPTER_RUNTIME_WORKERS`-many of them would starve every
//! unrelated adapter task on that runtime.
//!
//! The review taxonomy of "where synchronous foreign blocking could now land":
//!   1. the reducer thread — structurally impossible (pure `EngineCore`).
//!   2. a fresh, dedicated per-operation OS thread — allowed; holds no worker.
//!   3. an `ADAPTER_RUNTIME_WORKERS` runtime worker — MUST BE EMPTY.
//!
//! This test falsifies "category 3 is non-empty" for the one foreign seam that
//! runs arbitrary caller code on completion: the sign-event completion closure.
//! #704's `spawn_sign_event_completion` runs that closure on a FRESH per-op OS
//! thread (category 2), never a worker, "precisely so a blocking completion holds
//! no worker" (its own doc comment). We prove it: we park several completion
//! closures for a long, bounded time and show that unrelated adapter work — row
//! observations, a NIP-11 fetch, a follow snapshot, and an unrelated LOCAL sign
//! — all keep making bounded progress while those closures are blocked, and that
//! the whole-engine OS-thread count grows by only a small CONSTANT (at most one
//! per blocked callback), never onto the two shared workers.
//!
//! # The exact FFI seam used (and why the facade seam alone is insufficient)
//!
//! The blocking foreign completion is submitted through
//! `nmp::Engine::sign_event_with_completion(request, completion)` — the
//! doc-hidden `pub` seam whose `completion: impl FnOnce(Result<Event,
//! SignEventError>) + Send + 'static` is exactly the "foreign callback that may
//! run arbitrary (blocking) code on completion", and exactly the seam the FFI
//! facade's `NmpEngine::sign_event` calls. The facade wraps it with a
//! NON-blocking forwarding completion (`move |r| sender.send(r)`), and does not
//! expose its inner `nmp::Engine` (`pub(crate)`), so a blocking foreign
//! completion cannot be injected through the public facade method. Because the
//! falsifier requires the blocking completion and the unrelated work to share ONE
//! engine/runtime, the whole test drives `nmp::Engine` directly — the object the
//! FFI `NmpEngine` wraps 1:1; each unrelated operation calls the precise engine
//! method the corresponding facade method delegates to (`observe_async`,
//! `relay_information`, `nmp_nip02::observe_following_async`, `sign_event_with_
//! completion`, `shutdown`). This is the harness of `mixed_load_704.rs` /
//! `thread_scaling.rs` one thin layer below the FFI wrapper types.
//!
//! # A note on `shutdown()` ordering (a deliberate, orthogonal property)
//!
//! `Engine::shutdown()` intentionally DRAINS every outstanding sign-event
//! completion: the reducer will not exit while a completion op is still
//! registered, and that registration clears only when the completion CLOSURE
//! RETURNS (`Cmd::SignEventFinished`, posted by a panic-safe drop guard). This is
//! a deliberate correctness guarantee with its own `*_shutdown_drain` runtime
//! tests — a foreign completion is never abandoned uncalled. It follows that
//! shutdown called while a completion is blocked returns only AFTER that
//! completion is released; that is BY DESIGN and is orthogonal to the
//! worker-starvation property under review. Asserting "shutdown returns while the
//! callback is blocked" would assert a property the engine deliberately does not
//! provide. So we demonstrate the truthful, stronger fact instead: while the
//! completions are blocked a backgrounded `shutdown()` is (correctly) still
//! draining them — deterministically NOT returned, proving the completions are
//! real, drained, per-op work — and the instant they are released `shutdown()`
//! returns within a tight bound, proving no worker was leaked or stalled.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nmp::{
    Engine, EngineConfig, Event, Filter, Kind, LiveQuery, RelayInformationCachePolicy,
    SignEventError, SignEventRequest, Timestamp,
};

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000007";

/// Blocking foreign completions to park at once. `ADAPTER_RUNTIME_WORKERS == 2`;
/// parking `2 * 2 = 4` genuinely-blocking completions would saturate AND backlog
/// every runtime worker under the hypothetical "category 3 is non-empty"
/// regression, so unrelated adapter work (the NIP-11 fetch and the unrelated
/// local sign in particular) would deterministically hang and this test's
/// bounded timeouts would fire. In the real design each completion holds its own
/// fresh per-op OS thread, so the two workers stay free.
const BLOCKING_COMPLETIONS: usize = 4;

/// Concurrent live row observations opened while the completions are blocked.
/// Chosen large so the whole-engine thread-growth bound below (a small constant)
/// is visibly independent of it: a per-operation thread admission would grow
/// proportional to this number.
const CONCURRENT_OBSERVATIONS: usize = 64;

/// Per-operation bound for every unrelated op driven while the callbacks are
/// blocked. Generous for CI, yet far below the completions' safety park ceiling,
/// so a category-3 regression (a completion holding a worker) deterministically
/// trips it.
const OP_BOUND: Duration = Duration::from_secs(10);

/// Bound for `shutdown()` to return AFTER the blocked completions are released.
const SHUTDOWN_BOUND: Duration = Duration::from_secs(15);

/// Absolute ceiling a blocked completion will park, as a safety net so a test
/// bug can never wedge CI. Vastly larger than every `OP_BOUND` above, so it
/// never masks a real stall: the test always releases the callbacks explicitly
/// long before this.
const PARK_CEILING: Duration = Duration::from_secs(60);

fn note_query() -> LiveQuery {
    LiveQuery::from_filter(Filter {
        kinds: Some(std::collections::BTreeSet::from([1u16])),
        ..Filter::default()
    })
}

fn text_note_request(content: &str) -> SignEventRequest {
    SignEventRequest {
        created_at: Timestamp::from(1_700_000_704u64),
        kind: Kind::from(1u16),
        tags: Vec::new(),
        content: content.to_string(),
    }
}

/// Coordinates the parked foreign completions with the test thread. Each blocked
/// completion increments `started` when it enters (so the test knows the per-op
/// thread is occupied), parks on `cv` until `released`, then increments `ran`
/// exactly once on the way out.
struct Block {
    released: Mutex<bool>,
    cv: Condvar,
    started: AtomicUsize,
    ran: AtomicUsize,
}

impl Block {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            released: Mutex::new(false),
            cv: Condvar::new(),
            started: AtomicUsize::new(0),
            ran: AtomicUsize::new(0),
        })
    }

    /// The genuinely-blocking foreign completion body. It runs on the fresh
    /// per-op OS thread `spawn_sign_event_completion` allocates — NOT a runtime
    /// worker — and parks there until the test releases it.
    fn park(&self) {
        self.started.fetch_add(1, Ordering::SeqCst);
        let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
        let deadline = Instant::now() + PARK_CEILING;
        while !*released {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break; // safety net only; the test always releases first.
            }
            let (guard, _timeout) = self
                .cv
                .wait_timeout(released, remaining)
                .unwrap_or_else(|p| p.into_inner());
            released = guard;
        }
        drop(released);
        self.ran.fetch_add(1, Ordering::SeqCst);
    }

    fn release(&self) {
        let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
        *released = true;
        drop(released);
        self.cv.notify_all();
    }
}

/// A minimal NIP-11 HTTP server (verbatim harness from `mixed_load_704.rs`):
/// answers every connection with a valid `application/nostr+json` document so
/// the engine's `relay_information` fetch reaches a real terminal SUCCESS. Its
/// accept/serve threads are raw test threads and do not touch the NMP counter.
fn spawn_nip11_ok_server() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut stream) = conn else { continue };
            thread::spawn(move || {
                let mut buf = [0u8; 1024];
                let _ = stream.read(&mut buf);
                let body = r#"{"name":"mock-704-relay","supported_nips":[1,11]}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/nostr+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.flush();
            });
        }
    });
    format!("ws://{addr}")
}

/// Spin (async) until `cond` holds or `bound` elapses; `what` names the wait for
/// the panic message. Deterministic waits are gated on this rather than a fixed
/// sleep so the test stays non-flaky.
async fn await_until(what: &str, bound: Duration, cond: impl Fn() -> bool) {
    let deadline = Instant::now() + bound;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn blocking_foreign_completion_never_stalls_unrelated_engine_work() {
    let relay_url = spawn_nip11_ok_server();
    let engine = Arc::new(
        Engine::new(EngineConfig {
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..EngineConfig::default()
        })
        .expect("in-memory engine must build"),
    );

    let account = engine
        .add_account(TEST_SECRET_KEY_HEX)
        .expect("test key parses");
    let author = account.public_key();
    engine
        .set_active_account(Some(author))
        .expect("account activates");

    // Baseline AFTER construction: the engine's fixed runtime/transport threads
    // are already counted, so any growth below is attributable to the operations.
    let baseline = nmp::nmp_threads_spawned();

    // ---- (1) Park BLOCKING_COMPLETIONS genuinely-blocking foreign completions.
    // A local signer produces a ready result inline, so each completion is
    // dispatched immediately onto its own fresh per-op OS thread, where it parks.
    let block = Block::new();
    // `SignEventCancel` has no Drop side effect; retained only to be explicit
    // that these operations stay outstanding (not cancelled) while parked.
    let mut cancels = Vec::new();
    for i in 0..BLOCKING_COMPLETIONS {
        let block = Arc::clone(&block);
        let cancel = engine
            .sign_event_with_completion(text_note_request(&format!("parked-{i}")), move |_result| {
                block.park();
            })
            .expect("sign submission is never refused for a capacity reason");
        cancels.push(cancel);
    }

    // Every parked completion has entered its blocking body: its per-op thread is
    // now occupied. If these ran on a runtime worker, all workers would be held.
    await_until("all foreign completions to block", OP_BOUND, || {
        block.started.load(Ordering::SeqCst) == BLOCKING_COMPLETIONS
    })
    .await;

    // The blocked callbacks consumed at most their own per-op thread each — not a
    // worker, and nothing proportional. (Each `spawn_sign_event_completion`
    // thread is counted by `nmp_threads_spawned`, so worker-hosted blocking would
    // instead show ZERO growth here while stalling the ops below.)
    let after_block = nmp::nmp_threads_spawned();
    let block_growth = after_block.saturating_sub(baseline);
    eprintln!(
        "#704 foreign-blocking: {BLOCKING_COMPLETIONS} parked completions -> \
         thread growth {block_growth} (baseline={baseline}, after={after_block})"
    );
    assert!(
        block_growth <= (BLOCKING_COMPLETIONS as u64) + 6,
        "{BLOCKING_COMPLETIONS} blocked completions grew NMP threads by {block_growth}; each must \
         hold at most its own per-op OS thread (bound={})",
        BLOCKING_COMPLETIONS + 6
    );

    // ---- (2) WHILE the completions are blocked, every unrelated operation makes
    // bounded progress. Under a category-3 regression the two workers would be
    // saturated and these would hang past OP_BOUND.

    // (a) Many concurrent live row observations, opened then driven to their
    // initial current-state frame.
    let mut streams = Vec::new();
    for _ in 0..CONCURRENT_OBSERVATIONS {
        streams.push(
            engine
                .observe_async(note_query(), None)
                .expect("no observation is ever refused for a capacity reason"),
        );
    }
    for stream in streams.iter().take(8) {
        let frame = tokio::time::timeout(OP_BOUND, stream.next())
            .await
            .expect("a row observation delivers its initial frame within bound")
            .expect("row next() is not a concurrent misuse");
        assert!(
            frame.is_some(),
            "each row observation yields an initial frame"
        );
    }

    // (b) A NIP-11 fetch against the local fixture reaches a real document.
    let info = tokio::time::timeout(
        OP_BOUND,
        engine.relay_information(&relay_url, RelayInformationCachePolicy::Refresh),
    )
    .await
    .expect("relay_information resolves within bound while completions are blocked")
    .expect("relay_information reaches a real terminal document");
    assert_eq!(info.document.name.as_deref(), Some("mock-704-relay"));

    // (c) A follow observation delivers its initial relationship snapshot.
    let follow = nmp_nip02::observe_following_async(Arc::clone(&engine), author)
        .expect("follow observation opens without a capacity refusal");
    let snapshot = tokio::time::timeout(OP_BOUND, follow.next())
        .await
        .expect("follow observation delivers within bound while completions are blocked")
        .expect("follow next() is not a concurrent misuse");
    assert!(
        snapshot.is_some(),
        "the follow observation yields an initial snapshot"
    );

    // (d) An UNRELATED local sign runs its own (fast) completion on its own per-op
    // thread and returns the verified signed event — the sharpest probe: were
    // completions worker-hosted, this fast completion could not run behind the
    // four blocked ones and its result would never arrive.
    let (tx, rx) = nmp::fifo_channel::<Result<Event, SignEventError>>();
    let _unrelated_cancel = engine
        .sign_event_with_completion(text_note_request("unrelated-local-sign"), move |result| {
            tx.send(result);
        })
        .expect("unrelated sign submission is never refused");
    let unrelated = rx.into_async();
    let outcome = tokio::time::timeout(OP_BOUND, unrelated.next())
        .await
        .expect("unrelated local sign resolves within bound while completions are blocked")
        .expect("unrelated sign result is not a concurrent misuse")
        .expect("unrelated sign delivers a result (not a dropped None)");
    let signed = outcome.expect("unrelated local sign succeeds under blocked foreign completions");
    assert_eq!(
        signed.pubkey, author,
        "the unrelated local sign is attributed to the active account"
    );

    // The blocked completions are still parked (none has been released yet): they
    // never fired their own result path, so `ran` is still zero.
    assert_eq!(
        block.ran.load(Ordering::SeqCst),
        0,
        "no blocked foreign completion has been released yet"
    );

    // Thread growth after ALL the concurrent work remains a small constant,
    // independent of CONCURRENT_OBSERVATIONS — the whole point.
    let after_work = nmp::nmp_threads_spawned();
    let work_growth = after_work.saturating_sub(baseline);
    eprintln!(
        "#704 foreign-blocking: after {CONCURRENT_OBSERVATIONS} observations + NIP-11 + follow + \
         unrelated sign, thread growth {work_growth} (baseline={baseline}, after={after_work})"
    );
    assert!(
        work_growth <= (BLOCKING_COMPLETIONS as u64) + 20,
        "mixed work over {CONCURRENT_OBSERVATIONS} observations grew NMP threads by {work_growth}; \
         growth must stay a small constant, NOT proportional to the observation count \
         (bound={})",
        BLOCKING_COMPLETIONS + 20
    );

    // ---- (3) shutdown() while the completions are blocked is (correctly) still
    // draining them; releasing them lets it return within a tight bound.
    let shutdown_engine = Arc::clone(&engine);
    let shutdown_returned = Arc::new(AtomicBool::new(false));
    let shutdown_flag = Arc::clone(&shutdown_returned);
    let shutdown_thread = thread::spawn(move || {
        shutdown_engine.shutdown();
        shutdown_flag.store(true, Ordering::SeqCst);
    });

    // Deterministic: `shutdown()` cannot return before the completions it drains
    // are released (the drain clears an op only on `SignEventFinished`, posted
    // when the completion closure returns), so the flag is provably false here.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !shutdown_returned.load(Ordering::SeqCst),
        "shutdown must still be draining the blocked foreign completions, proving each is a real, \
         drained per-op completion rather than lost work"
    );

    // Release every parked callback; each completes exactly once.
    block.release();
    await_until(
        "every foreign completion to complete once",
        OP_BOUND,
        || block.ran.load(Ordering::SeqCst) == BLOCKING_COMPLETIONS,
    )
    .await;
    assert_eq!(
        block.ran.load(Ordering::SeqCst),
        BLOCKING_COMPLETIONS,
        "each blocked foreign completion runs exactly once"
    );

    // With the completions drained, shutdown returns within a tight bound: no
    // worker was leaked or permanently stalled by the blocking callbacks.
    await_until("shutdown to return after release", SHUTDOWN_BOUND, || {
        shutdown_returned.load(Ordering::SeqCst)
    })
    .await;
    shutdown_thread
        .join()
        .expect("shutdown thread joins cleanly");

    // Keep the outstanding sign cancel handles alive to the end so the parked
    // operations were unambiguously live (not cancelled) throughout.
    drop(cancels);
    drop(streams);
}
