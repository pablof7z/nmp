//! #680 falsifiers — real composition, cancellation, and async delivery over
//! the pull-based observation handles. Driven by a real Tokio executor
//! (`#[tokio::test]`, dev-only; production stays runtime-free).

use std::sync::Arc;
use std::time::Duration;

use nmp_ffi::convert::FfiRowPullError;
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpRowStream};
#[cfg(feature = "nip02")]
use nmp_ffi::session::FfiPrivateKey;
#[cfg(feature = "nip02")]
use nmp_ffi::types::FfiFrame;
use nmp_ffi::types::{
    FfiCacheMode, FfiDemand, FfiFilter, FfiFreshness, FfiLiveQuery,
    FfiReadRouting,
};

/// Consume every immediately-available frame (an observation delivers its
/// initial current-state frame on open) so a subsequent ticket genuinely parks
/// on an empty mailbox. The timed-out `receive()` future is dropped mid-poll,
/// then the pre-existing ticket is aborted — the exact Kotlin wrapper order.
async fn quiesce(stream: &NmpRowStream) {
    loop {
        let pull = stream.begin_next().expect("stream is open");
        match tokio::time::timeout(Duration::from_millis(150), pull.receive()).await {
            Ok(Ok(Some(_))) => pull.commit().expect("delivered frame commits"),
            Ok(Ok(None)) => {
                pull.commit().expect("terminal result commits");
                break;
            }
            Ok(Err(error)) => panic!("unexpected row-pull error: {error}"),
            Err(_) => {
                pull.abort();
                break;
            }
        }
    }
}

#[cfg(feature = "nip02")]
async fn next_committed(stream: &NmpRowStream) -> Option<FfiFrame> {
    let pull = stream.begin_next().expect("stream is open");
    let frame = pull.receive().await.expect("ticket lifecycle is valid");
    pull.commit()
        .expect("foreign completion commits the ticket");
    frame
}

#[cfg(feature = "nip02")]
const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";

fn engine() -> Arc<NmpEngine> {
    NmpEngine::new(NmpEngineConfig::default(), None).expect("temporary Redb engine must build")
}

fn note_query() -> FfiLiveQuery {
    FfiLiveQuery {
        branches: vec![FfiDemand {
            selection: FfiFilter {
                kinds: Some(vec![1]),
                ..FfiFilter::default()
            },
            routing: FfiReadRouting::Auto,
            authenticate_as: None,
            cache: FfiCacheMode::Agnostic,
            freshness: FfiFreshness::Live,
        }],
        aggregate_result_limit: None,
    }
}

/// Falsifier 2 — real composition. One engine holds 64 row observations,
/// diagnostics, a follow observation, and an active receipt stream at once. No
/// operation is refused for a global native-task-capacity reason (there is no
/// such concept any more), and the current-state stream (diagnostics) delivers.
#[tokio::test]
#[cfg(feature = "nip02")]
async fn dense_composition_never_refuses_and_delivers_current_state() {
    let engine = engine();
    let keys = nostr::Keys::parse(TEST_SECRET_KEY_HEX).unwrap();
    let author = keys.public_key().to_hex();
    // #1237: `Identity::Active` with no current account is now an instruction
    // that cannot resolve, so `publish` refuses the call outright. This
    // falsifier is about composition, not about that refusal — activate the
    // account so the receipt stream this test holds actually exists.
    engine
        .add_private_key_account(
            FfiPrivateKey::from_bytes(keys.secret_key().to_secret_bytes().to_vec()).unwrap(),
            true,
        )
        .expect("current account activates");

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
            payload: nmp_ffi::types::FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: 1,
                    tags: Vec::new(),
                    content: "composition".to_string(),
                    created_at: Some(0),
                },
            },
            routing: nmp_ffi::types::FfiWriteRouting::Explicit {
                relays: vec!["wss://write.example".to_string()],
            },
            identity: nmp_ffi::types::FfiIdentity::Active,
        })
        .expect("publish opens a receipt stream");

    assert_eq!(rows.len(), 64);
    let _ = (&follow, &receipt);

    // Acceptance #2: every observation receives its initial/current state over
    // the async pull path.
    let initial = tokio::time::timeout(Duration::from_secs(5), next_committed(&rows[0]))
        .await
        .expect("a row observation delivers its initial frame within 5s");
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

    let pull = stream.begin_next().expect("parked ticket begins");
    let waiter = tokio::spawn(async move { pull.receive().await });
    // Let the reader park on the now-empty mailbox.
    tokio::time::sleep(Duration::from_millis(50)).await;

    stream.cancel();
    stream.cancel(); // idempotent

    let ended = tokio::time::timeout(Duration::from_secs(5), waiter)
        .await
        .expect("cancel wakes the parked next() within 5s")
        .expect("reader task did not panic")
        .expect("receive() is not a misuse");
    assert!(ended.is_none(), "a cancelled handle yields None, no frame");

    // A post-cancel ticket cannot resurrect a retained frame.
    assert_eq!(
        stream.begin_next().unwrap_err(),
        FfiRowPullError::Closed,
        "no ticket or frame exists after cancel"
    );

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
            let pull = s.begin_next().expect("shutdown waiter ticket begins");
            tokio::spawn(async move {
                let ended = pull.receive().await;
                if ended.is_ok() {
                    pull.commit().expect("terminal shutdown pull commits");
                }
                ended
            })
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

    let first_pull = stream.begin_next().expect("first ticket begins");
    let first = tokio::spawn(async move { first_pull.receive().await });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second overlapping next() must return promptly with the misuse error,
    // not hang behind the parked first one.
    assert_eq!(
        stream.begin_next().unwrap_err(),
        FfiRowPullError::ConcurrentNext,
        "a concurrent ticket is rejected before any second async pull begins"
    );

    stream.cancel();
    let _ = first.await;
    engine.shutdown();
}
