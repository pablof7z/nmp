//! #680 falsifiers — real composition, cancellation, and async delivery over
//! the pull-based observation handles. Driven by a real Tokio executor
//! (`#[tokio::test]`, dev-only; production stays runtime-free).

use std::sync::Arc;
use std::time::Duration;

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpRowStream};
use nmp_ffi::types::FfiFilter;

/// Consume every immediately-available frame (an observation delivers its
/// initial current-state frame on open) so a subsequent `next()` genuinely
/// parks on an empty mailbox. The timed-out `next()` future is dropped
/// mid-poll, which releases the single-reader guard (RAII) — proving that path
/// too.
async fn quiesce(stream: &NmpRowStream) {
    while tokio::time::timeout(Duration::from_millis(150), stream.next())
        .await
        .is_ok()
    {}
}

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";

fn engine() -> Arc<NmpEngine> {
    NmpEngine::new(NmpEngineConfig::default()).expect("in-memory engine must build")
}

fn note_query() -> FfiFilter {
    FfiFilter {
        kinds: Some(vec![1]),
        ..FfiFilter::default()
    }
}

/// Falsifier 2 — real composition. One engine holds 64 row observations,
/// diagnostics, a follow observation, and an active receipt stream at once. No
/// operation is refused for a global native-task-capacity reason (there is no
/// such concept any more), and the current-state stream (diagnostics) delivers.
#[tokio::test]
async fn dense_composition_never_refuses_and_delivers_current_state() {
    let engine = engine();
    let account = engine
        .add_account(TEST_SECRET_KEY_HEX.to_string())
        .expect("test key parses");
    let author = account.public_key();

    // 64 simultaneous live row observations — the old design refused at 13.
    let mut rows = Vec::new();
    for _ in 0..64 {
        rows.push(
            engine
                .observe(note_query(), None)
                .expect("no capacity refusal exists"),
        );
    }

    let diagnostics = engine
        .observe_diagnostics()
        .expect("diagnostics observation opens");
    let follow = engine
        .observe_following(author.clone())
        .expect("follow observation opens");
    let receipt = engine
        .publish(nmp_ffi::types::FfiWriteIntent {
            payload: nmp_ffi::types::FfiWritePayload::Unsigned {
                pubkey: author.clone(),
                created_at: 0,
                kind: 1,
                tags: Vec::new(),
                content: "composition".to_string(),
            },
            durability: nmp_ffi::types::FfiDurability::Durable,
            routing: nmp_ffi::types::FfiWriteRouting::AuthorOutbox,
            identity_override: None,
        })
        .expect("publish opens a receipt stream");

    assert_eq!(rows.len(), 64);
    let _ = (&follow, &receipt);

    // Acceptance #2: every observation receives its initial/current state over
    // the async pull path.
    let initial = tokio::time::timeout(Duration::from_secs(5), rows[0].next())
        .await
        .expect("a row observation delivers its initial frame within 5s")
        .expect("row next() is not a misuse");
    assert!(
        initial.is_some(),
        "a row observation yields its initial current-state frame"
    );

    // The current-state diagnostics stream delivers its current snapshot
    // immediately over the async pull path — proof the waker delivery works.
    let snapshot = tokio::time::timeout(Duration::from_secs(5), diagnostics.next())
        .await
        .expect("diagnostics delivers within 5s")
        .expect("diagnostics next() is not a misuse");
    assert!(snapshot.is_some(), "diagnostics yields a current snapshot");

    engine.shutdown();
}

/// Falsifier 4 — cancellation. `cancel()` wakes a parked `next()` to `None`
/// immediately, is idempotent, and yields no post-cancel frame.
#[tokio::test]
async fn cancel_wakes_a_parked_next_to_none_and_is_idempotent() {
    let engine = engine();
    let stream = engine.observe(note_query(), None).expect("opens");
    quiesce(&stream).await;

    let reader = stream.clone();
    let waiter = tokio::spawn(async move { reader.next().await });
    // Let the reader park on the now-empty mailbox.
    tokio::time::sleep(Duration::from_millis(50)).await;

    stream.cancel();
    stream.cancel(); // idempotent

    let ended = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("cancel wakes the parked next() within 5s")
        .expect("reader task did not panic")
        .expect("next() is not a misuse");
    assert!(ended.is_none(), "a cancelled handle yields None, no frame");

    // A post-cancel next() stays None.
    let again = stream.next().await.expect("not a misuse");
    assert!(again.is_none(), "no frame after cancel");

    engine.shutdown();
}

/// Falsifier 6 — shutdown wakes a pending `next()` deterministically.
#[tokio::test]
async fn shutdown_wakes_all_pending_next_to_none() {
    let engine = engine();
    let mut streams = Vec::new();
    for _ in 0..16 {
        let stream = engine.observe(note_query(), None).expect("opens");
        quiesce(&stream).await;
        streams.push(stream);
    }
    let waiters: Vec<_> = streams
        .iter()
        .map(|s| {
            let s = s.clone();
            tokio::spawn(async move { s.next().await })
        })
        .collect();
    tokio::time::sleep(Duration::from_millis(50)).await;

    engine.shutdown();

    for waiter in waiters {
        let ended = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("shutdown wakes every pending next() within 5s")
            .expect("no panic")
            .expect("not a misuse");
        assert!(ended.is_none(), "shutdown ends every stream with None");
    }
}

/// Falsifier — concurrent `next()` on one handle is a typed misuse, never a
/// silent lost wakeup or hang.
#[tokio::test]
async fn concurrent_next_on_one_handle_is_a_typed_error() {
    let engine = engine();
    let stream = engine.observe(note_query(), None).expect("opens");
    quiesce(&stream).await;

    let a = stream.clone();
    let first = tokio::spawn(async move { a.next().await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second overlapping next() must return promptly with the misuse error,
    // not hang behind the parked first one.
    let second = tokio::time::timeout(Duration::from_secs(5), stream.next())
        .await
        .expect("the concurrent next() returns promptly");
    assert!(
        second.is_err(),
        "a concurrent next() on one handle is rejected as misuse"
    );

    stream.cancel();
    let _ = first.await;
    engine.shutdown();
}
