//! #762 falsifiers for Kotlin's UniFFI READY-before-complete cancellation seam.
//!
//! `NmpRowStream::begin_next` creates a private native pull ticket before any
//! cancellable await. A delta returned by `NmpRowPull::receive` is not
//! committed until the foreign wrapper synchronously calls `commit`; dropping
//! or aborting the ticket restores that exact transition for the next ticket.

use std::collections::BTreeSet;
use std::future::{poll_fn, Future};
use std::sync::{Arc, Barrier};
use std::task::Poll;
use std::time::Duration;

use nmp_ffi::convert::FfiRowPullError;
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpRowPull, NmpRowStream};
use nmp_ffi::types::{
    FfiDurability, FfiEventBuilder, FfiFilter, FfiFrame, FfiIdentity, FfiRowDelta, FfiWindow,
    FfiWriteIntent, FfiWritePayload, FfiWriteRouting,
};

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";

fn test_engine() -> Arc<NmpEngine> {
    let engine = NmpEngine::new(NmpEngineConfig::default()).expect("in-memory engine opens");
    let account = engine
        .add_account(TEST_SECRET_KEY_HEX.to_string())
        .expect("test key parses");
    engine
        .set_active_account(Some(account.public_key()))
        .expect("test account becomes active");
    engine
}

fn note_query() -> FfiFilter {
    FfiFilter {
        kinds: Some(vec![1]),
        ..FfiFilter::default()
    }
}

async fn publish_note(engine: &NmpEngine, sequence: u64) {
    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: FfiEventBuilder {
                    kind: 1,
                    tags: Vec::new(),
                    content: format!("ticketed-delta-{sequence}"),
                    created_at: Some(sequence),
                },
            },
            durability: FfiDurability::Durable,
            routing: FfiWriteRouting::Auto,
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("local acceptance succeeds");
    let accepted = tokio::time::timeout(Duration::from_secs(5), receipt.next())
        .await
        .expect("acceptance fact arrives")
        .expect("receipt pull is valid")
        .expect("receipt has an acceptance fact");
    assert!(
        matches!(accepted, nmp_ffi::types::FfiWriteStatus::Accepted),
        "the producer mutation is accepted before the next cancellation cycle"
    );
}

async fn receive(pull: &NmpRowPull) -> Option<FfiFrame> {
    tokio::time::timeout(Duration::from_secs(5), pull.receive())
        .await
        .expect("pull resolves within five seconds")
        .expect("pull lifecycle is valid")
}

async fn next_committed(stream: &NmpRowStream) -> Option<FfiFrame> {
    let pull = stream.begin_next().expect("stream ticket is available");
    let frame = receive(&pull).await;
    pull.commit()
        .expect("foreign completion commits the ticket");
    frame
}

fn apply_ids(ids: &mut BTreeSet<String>, frame: &FfiFrame) {
    for delta in &frame.deltas {
        match delta {
            FfiRowDelta::Added { row } => {
                ids.insert(row.id.clone());
            }
            FfiRowDelta::SourcesGrew { .. } => {}
            FfiRowDelta::Removed { id } => {
                ids.remove(id);
            }
        }
    }
}

#[tokio::test]
async fn ready_frame_is_retained_until_foreign_commit_and_replayed_after_abort() {
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    let initial = next_committed(&stream)
        .await
        .expect("initial current-state frame");
    assert!(initial.deltas.is_empty());

    publish_note(&engine, 1).await;
    let first_ticket = stream.begin_next().expect("first ticket begins");
    let first = receive(&first_ticket)
        .await
        .expect("first delta reaches Rust READY");
    assert!(!first.deltas.is_empty());

    publish_note(&engine, 2).await;
    assert_eq!(
        stream.begin_next().unwrap_err(),
        FfiRowPullError::ConcurrentNext,
        "the READY ticket keeps exclusive ownership before foreign completion"
    );

    // This is the exact semantic outcome of Kotlin cancellation after UniFFI
    // reported READY but before its separate complete call retrieved the value.
    first_ticket.abort();

    let retry_ticket = stream.begin_next().expect("abort releases the claim");
    let replay = receive(&retry_ticket)
        .await
        .expect("the undelivered frame is replayed");
    assert_eq!(
        replay, first,
        "retry returns byte-for-byte the retained frame"
    );
    retry_ticket
        .commit()
        .expect("foreign completion commits the replay");

    let successor = next_committed(&stream)
        .await
        .expect("updates composed while the claim was retained");
    assert_ne!(
        successor, first,
        "a committed frame is never replayed by the successor pull"
    );

    let mut ids = BTreeSet::new();
    apply_ids(&mut ids, &first);
    apply_ids(&mut ids, &successor);
    assert_eq!(
        ids.len(),
        2,
        "the replayed baseline plus composed successor converges exactly"
    );

    engine.shutdown();
}

#[tokio::test]
async fn repeated_ready_cancellation_keeps_one_claim_and_one_composed_successor() {
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    let _ = next_committed(&stream).await;

    publish_note(&engine, 1).await;
    let mut retained: Option<FfiFrame> = None;
    for sequence in 2..=129 {
        let ticket = stream.begin_next().expect("one ticket at a time");
        let replay = receive(&ticket).await.expect("retained delta resolves");
        if let Some(expected) = &retained {
            assert_eq!(
                &replay, expected,
                "cancellation count never creates another retained frame"
            );
        } else {
            retained = Some(replay.clone());
        }
        publish_note(&engine, sequence).await;
        ticket.abort();
    }

    let retained = retained.expect("one baseline frame was retained");
    let final_ticket = stream.begin_next().expect("final retry begins");
    assert_eq!(receive(&final_ticket).await, Some(retained.clone()));
    final_ticket
        .commit()
        .expect("baseline commits exactly once");

    let successor = next_committed(&stream)
        .await
        .expect("one composed successor remains");
    let mut ids = BTreeSet::new();
    apply_ids(&mut ids, &retained);
    apply_ids(&mut ids, &successor);
    assert_eq!(
        ids.len(),
        129,
        "128 cancel/retry cycles converge without an item-per-cancel queue"
    );

    engine.shutdown();
}

#[tokio::test]
async fn one_ticket_receive_is_start_once_while_pending_and_after_ready() {
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    let _ = next_committed(&stream).await;

    let pending = stream.begin_next().expect("pending ticket begins");
    let mut first_receive = Box::pin(pending.receive());
    let first_poll = poll_fn(|cx| Poll::Ready(first_receive.as_mut().poll(cx))).await;
    assert!(
        first_poll.is_pending(),
        "the first receive is deterministically parked before the second starts"
    );
    assert_eq!(
        pending.receive().await.unwrap_err(),
        FfiRowPullError::ReceiveAlreadyStarted,
        "one ticket cannot start two Rust futures while the first is pending"
    );
    stream.cancel();
    assert_eq!(
        first_receive.await,
        Ok(None),
        "stream cancellation wakes the one accepted receiver"
    );

    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    let _ = next_committed(&stream).await;
    publish_note(&engine, 1).await;
    let ready = stream.begin_next().expect("ready ticket begins");
    assert!(receive(&ready).await.is_some());
    assert_eq!(
        ready.receive().await.unwrap_err(),
        FfiRowPullError::ReceiveAlreadyStarted,
        "Rust READY does not release the ticket before foreign completion"
    );
    ready.abort();
    engine.shutdown();
}

#[tokio::test]
async fn commit_refusals_are_typed_and_leave_the_candidate_unchanged() {
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    let fresh = stream.begin_next().expect("fresh ticket begins");
    assert_eq!(
        fresh.commit().unwrap_err(),
        FfiRowPullError::NotReady,
        "commit cannot consume a ticket before receive returns"
    );
    let initial = receive(&fresh).await.expect("fresh ticket still works");
    fresh.abort();

    assert_eq!(
        fresh.commit().unwrap_err(),
        FfiRowPullError::Finished,
        "an aborted ticket can never commit a later ticket's candidate"
    );
    let replay = stream.begin_next().expect("replay ticket begins");
    assert_eq!(receive(&replay).await, Some(initial));
    replay.commit().expect("replayed initial frame commits");

    publish_note(&engine, 1).await;
    let ready = stream.begin_next().expect("ready ticket begins");
    assert!(receive(&ready).await.is_some());
    stream.cancel();
    assert_eq!(
        ready.commit().unwrap_err(),
        FfiRowPullError::Closed,
        "stream cancellation discards the retained candidate before refusal"
    );
    ready.abort();
    assert_eq!(stream.begin_next().unwrap_err(), FfiRowPullError::Closed);
    engine.shutdown();
}

#[tokio::test]
async fn abort_does_not_replay_a_self_contained_window_snapshot() {
    let engine = test_engine();
    let stream = engine
        .observe(
            note_query(),
            Some(FfiWindow::Expandable { initial: 1, max: 2 }),
        )
        .expect("windowed observation opens");
    let first = stream.begin_next().expect("snapshot ticket begins");
    let snapshot = receive(&first).await.expect("initial snapshot arrives");
    assert!(snapshot.window.is_some());
    assert!(snapshot.deltas.is_empty());
    first.abort();

    let retry = stream.begin_next().expect("abort releases the ticket");
    let mut receive_again = Box::pin(retry.receive());
    let first_poll = poll_fn(|cx| Poll::Ready(receive_again.as_mut().poll(cx))).await;
    assert!(
        first_poll.is_pending(),
        "a self-contained snapshot is not retained or replayed after abort"
    );
    stream.cancel();
    assert_eq!(receive_again.await, Ok(None));
    engine.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commit_abort_cancel_race_never_resurrects_or_duplicates_a_delta() {
    for round in 0..64 {
        let engine = test_engine();
        let stream = engine
            .observe(note_query(), None)
            .expect("observation opens");
        let _ = next_committed(&stream).await;
        publish_note(&engine, round + 1).await;

        let pull = stream.begin_next().expect("race ticket begins");
        assert!(
            receive(&pull).await.is_some(),
            "race starts from Rust READY"
        );

        let barrier = Arc::new(Barrier::new(3));
        let commit = {
            let barrier = barrier.clone();
            let pull = pull.clone();
            std::thread::spawn(move || {
                barrier.wait();
                pull.commit()
            })
        };
        let abort = {
            let barrier = barrier.clone();
            let pull = pull.clone();
            std::thread::spawn(move || {
                barrier.wait();
                pull.abort();
            })
        };
        let cancel = {
            let barrier = barrier.clone();
            let stream = stream.clone();
            std::thread::spawn(move || {
                barrier.wait();
                stream.cancel();
            })
        };

        let commit_result = commit.join().expect("commit racer did not panic");
        abort.join().expect("abort racer did not panic");
        cancel.join().expect("cancel racer did not panic");
        assert!(
            matches!(
                commit_result,
                Ok(()) | Err(FfiRowPullError::Finished) | Err(FfiRowPullError::Closed)
            ),
            "commit reports the exact lifecycle winner: {commit_result:?}"
        );
        assert_eq!(
            stream.begin_next().unwrap_err(),
            FfiRowPullError::Closed,
            "cancel is terminal: neither abort nor a late commit resurrects the frame"
        );
        engine.shutdown();
    }
}
