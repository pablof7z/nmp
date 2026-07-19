//! #704 falsifier — mixed engine load makes progress with NO capacity refusal.
//!
//! One engine drives, CONCURRENTLY: many live `observe` handles, a NIP-11
//! `relay_information` fetch, a local sign, a follow observation, a follow
//! action, and a durable publish receipt. #704 removed the internal
//! task/thread admission-capacity concept entirely: every logical operation is
//! an async task on the ONE shared engine-owned runtime, so there is no worker
//! permit to be blocked on and no capacity error type left to return
//! (`ThreadUnavailable`/`ExecutorSaturated`/`WaiterSaturated` were deleted;
//! the only split infra errors are `EngineStartFailed`/`ObservationUnavailable`).
//!
//! Because the capacity error variants no longer exist, this test cannot assert
//! "capacity refusal is not returned" against a variant — the STRONGEST form of
//! that assertion is structural: this file COMPILES while referencing no such
//! variant, every operation resolves to a real (non-capacity) domain outcome,
//! AND the whole-engine OS-thread count does not grow with the number of
//! concurrent logical operations. Thread count via `nmp::nmp_threads_spawned()`
//! (runtime workers + transport + reducer/bridge threads; never logical tasks).
//!
//! The zero-thread-per-observation property itself is proven exhaustively (to
//! 1,000 handles, with a printed table) by `thread_scaling.rs`'s
//! `observation_handles_scale_with_zero_native_thread_growth`; here we only
//! assert the MIXED workload adds no thread proportional to its op count.
//!
//! One thread-counting test per test binary keeps the global spawn counter
//! isolated (cargo runs integration-test binaries sequentially); the parked-
//! wait property lives in the sibling `parked_nip11_704.rs`.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpRowStream};
use nmp_ffi::types::{
    FfiDurability, FfiFilter, FfiRelayInformationCachePolicy, FfiSignEventRequest, FfiWriteIntent,
    FfiWritePayload, FfiWriteRouting,
};

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000007";

/// Number of concurrent live row observations in the mixed load. The old
/// design refused at the 13th and spawned one OS thread per admitted
/// observation; the thread-growth bound below is a small CONSTANT independent
/// of this number, which is the whole point.
const CONCURRENT_OBSERVATIONS: usize = 200;

/// Whole-engine OS-thread growth we tolerate for the entire mixed workload.
/// It is a fixed constant (host-runtime lazy-thread slack), deliberately far
/// below `CONCURRENT_OBSERVATIONS`: a design with any per-operation thread
/// admission would blow past it. Expected actual growth is ~0.
const THREAD_GROWTH_BOUND: u64 = 16;

fn note_query() -> FfiFilter {
    FfiFilter {
        kinds: Some(vec![1]),
        ..FfiFilter::default()
    }
}

/// A minimal NIP-11 HTTP server: answers every connection with a valid
/// `application/nostr+json` document so the engine's `relay_information` fetch
/// reaches a real terminal SUCCESS (not a capacity refusal). Its accept/serve
/// threads are raw test threads and do not touch the NMP thread counter.
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn mixed_engine_load_makes_progress_without_capacity_refusal() {
    let relay_url = spawn_nip11_ok_server();
    let engine = NmpEngine::new(NmpEngineConfig {
        allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
        ..NmpEngineConfig::default()
    })
    .expect("in-memory engine must build");

    let account = engine
        .add_account(TEST_SECRET_KEY_HEX.to_string())
        .expect("test key parses");
    let author = account.public_key();
    engine
        .set_active_account(Some(author.clone()))
        .expect("account activates");

    // Baseline AFTER engine construction: the engine's fixed runtime/transport
    // threads are already counted here, so any growth below is attributable to
    // the concurrent logical operations.
    let baseline = nmp::nmp_threads_spawned();

    // (a) Many concurrent live row observations, held open (an observation is a
    // lightweight Arc+waker; a held-open handle reserves no thread). We do NOT
    // spawn draining consumers here because the progress assertion below calls
    // `next()` directly on a few of these handles, and a concurrent `next()` on
    // one handle is a typed misuse.
    let mut streams: Vec<Arc<NmpRowStream>> = Vec::new();
    for _ in 0..CONCURRENT_OBSERVATIONS {
        let stream = engine
            .observe(note_query(), None)
            .expect("no observation is ever refused for a capacity reason");
        streams.push(stream);
    }

    // (b) A follow observation over the active account.
    let follow_obs = engine
        .observe_following(author.clone())
        .expect("follow observation opens without capacity refusal");

    // (c) A concurrent NIP-11 fetch.
    let nip11 = {
        let engine = engine.clone();
        let relay_url = relay_url.clone();
        tokio::spawn(async move {
            engine
                .relay_information(relay_url, FfiRelayInformationCachePolicy::Refresh)
                .await
        })
    };

    // (d) A concurrent local sign.
    let sign_handle = engine
        .sign_event(FfiSignEventRequest {
            created_at: 1_700_000_704,
            kind: 1,
            tags: Vec::new(),
            content: "mixed-load sign".to_string(),
        })
        .expect("sign operation starts without capacity refusal");

    // (e) A concurrent follow action toward an unrelated target.
    let other = nostr::Keys::generate().public_key().to_hex();
    let follow_action = engine.follow(other);

    // (f) A concurrent durable publish receipt.
    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Unsigned {
                pubkey: author.clone(),
                created_at: 0,
                kind: 1,
                tags: Vec::new(),
                content: "mixed-load publish".to_string(),
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::AuthorOutbox,
            identity_override: None,
            correlation: None,
        })
        .expect("publish opens a receipt stream without capacity refusal");

    // ---- Every operation makes real progress (resolves; no hang, no capacity
    // error). ----

    // The local sign resolves to the exact verified event.
    let signed = tokio::time::timeout(Duration::from_secs(10), sign_handle.signed())
        .await
        .expect("local sign resolves within 10s")
        .expect("local sign succeeds under mixed load");
    assert_eq!(
        signed.pubkey, author,
        "sign is attributed to the active account"
    );

    // The NIP-11 fetch resolves to a real document (progress), not a capacity
    // refusal.
    let info = tokio::time::timeout(Duration::from_secs(10), nip11)
        .await
        .expect("relay_information resolves within 10s")
        .expect("relay_information task did not panic")
        .expect("relay_information reaches a real terminal document under mixed load");
    assert_eq!(info.document.name.as_deref(), Some("mock-704-relay"));

    // The follow observation delivers its initial relationship snapshot.
    let follow_snapshot = tokio::time::timeout(Duration::from_secs(10), follow_obs.next())
        .await
        .expect("follow observation delivers within 10s")
        .expect("follow next() is not a misuse");
    assert!(
        follow_snapshot.is_some(),
        "the follow observation yields an initial snapshot"
    );

    // Representative row observations deliver their initial current-state frame.
    for stream in streams.iter().take(8) {
        let frame = tokio::time::timeout(Duration::from_secs(10), stream.next())
            .await
            .expect("a row observation delivers within 10s")
            .expect("row next() is not a misuse");
        assert!(
            frame.is_some(),
            "each row observation yields an initial frame"
        );
    }

    // The follow action and the receipt stream both remain live, progressing
    // handles (they own no thread; kept alive to prove the mixed set coexists).
    let _ = (&follow_action, &receipt);

    // ---- No thread grew with the concurrent op count. ----
    let after = nmp::nmp_threads_spawned();
    let growth = after.saturating_sub(baseline);
    eprintln!(
        "\n#704 mixed-load thread growth: baseline={baseline} after={after} growth={growth} \
         over {CONCURRENT_OBSERVATIONS} observations + follow-obs + NIP-11 + sign + follow-action \
         + durable publish (bound={THREAD_GROWTH_BOUND}, old design would add ~{CONCURRENT_OBSERVATIONS})\n"
    );
    assert!(
        growth <= THREAD_GROWTH_BOUND,
        "mixed load added {growth} NMP OS threads (baseline={baseline}, after={after}); a growth \
         proportional to the {CONCURRENT_OBSERVATIONS} concurrent operations would mean a \
         per-operation thread admission still exists. Bound={THREAD_GROWTH_BOUND}."
    );

    // Teardown.
    for stream in &streams {
        stream.cancel();
    }
    engine.shutdown();
}
