//! #762 falsifiers for Kotlin's UniFFI READY-before-complete cancellation seam.
//!
//! `NmpRowStream::begin_next` creates a private native pull ticket before any
//! cancellable await. A delta returned by `NmpRowPull::receive` is not
//! committed until the foreign wrapper synchronously calls `commit`; dropping
//! or aborting the ticket restores that exact transition for the next ticket.

use std::collections::BTreeSet;
use std::future::{poll_fn, Future};
use std::pin::Pin;
use std::sync::{Arc, Barrier};
use std::task::Poll;
use std::time::Duration;

use nmp_ffi::convert::FfiRowPullError;
use nmp_ffi::facade::{NmpEngine, NmpEngineConfig, NmpRowPull, NmpRowStream};
use nmp_ffi::session::FfiPrivateKey;
use nmp_ffi::types::{
    FfiEventBuilder, FfiFilter, FfiFrame, FfiIdentity, FfiRowDelta, FfiWindow, FfiWriteIntent,
    FfiWritePayload, FfiWriteRouting,
};

const TEST_SECRET_KEY_HEX: &str =
    "0000000000000000000000000000000000000000000000000000000000000001";

fn test_engine() -> Arc<NmpEngine> {
    let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("in-memory engine opens");
    engine
        .add_private_key_account(
            FfiPrivateKey::from_bytes(
                nostr::Keys::parse(TEST_SECRET_KEY_HEX)
                    .unwrap()
                    .secret_key()
                    .to_secret_bytes()
                    .to_vec(),
            )
            .unwrap(),
            true,
        )
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
    // `publish` returning `Ok` IS acceptance: the write is durably recorded
    // before this call returns, so the producer mutation is committed before
    // the next cancellation cycle without reading anything off the stream.
    engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: FfiEventBuilder {
                    kind: 1,
                    tags: Vec::new(),
                    content: format!("ticketed-delta-{sequence}"),
                    created_at: Some(sequence),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec!["wss://write.example".to_string()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("local acceptance succeeds");
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

type ReceiveFuture =
    Pin<Box<dyn Future<Output = Result<Option<FfiFrame>, FfiRowPullError>> + Send>>;

/// Return a ticket whose `receive()` is *observably* parked on an empty
/// mailbox, plus that in-flight future.
///
/// An observation delivers its initial current-state frame on open, and engine
/// startup can produce further frames after it. Asserting "the first poll is
/// Pending" therefore encodes a scheduling assumption rather than a property
/// (that is the shape #1166 tracks). This helper instead *establishes* the
/// parked state by observation: it commits whatever is already available and
/// retries, so it is the arrival of a frame, never the loaded runner, that
/// decides when the loop stops.
async fn parked_ticket(stream: &NmpRowStream) -> (Arc<NmpRowPull>, ReceiveFuture) {
    for _ in 0..64 {
        let pull = stream.begin_next().expect("stream ticket is available");
        let owned = pull.clone();
        let mut receiving: ReceiveFuture = Box::pin(async move { owned.receive().await });
        match poll_fn(|cx| Poll::Ready(receiving.as_mut().poll(cx))).await {
            Poll::Pending => return (pull, receiving),
            Poll::Ready(delivered) => {
                delivered.expect("an already-available frame is a valid delivery");
                drop(receiving);
                pull.commit().expect("the already-available frame commits");
            }
        }
    }
    panic!("the observation never reached an empty mailbox");
}

/// Consume every already-available frame, then release the claim through the
/// exact wrapper order (generated free of the Rust future, then `abort`).
async fn drain(stream: &NmpRowStream) {
    let (pull, receiving) = parked_ticket(stream).await;
    drop(receiving);
    pull.abort();
}

/// Pull until a ticket holds a frame that carries a row transition, committing
/// every frame that carries none. The returned ticket is left UNSETTLED, so the
/// caller still owns the commit/abort/drop decision under test.
///
/// A frame with no deltas is an ordinary delivery, not an anomaly. The row
/// mailbox carries three things -- the exact transition, this observation's
/// acquisition evidence, and its ordered execution facts -- and the last two
/// change with no row change at all. `QueryState::refresh_observation_evidence`
/// emits `Effect::EmitRows(id, Vec::new(), evidence)` outright, and
/// `Effect::EmitObservationEvidence` reaches the same mailbox through
/// `RowsSender::send_evidence`, which composes execution facts onto an
/// otherwise empty pending transition. Opening an observation already produces
/// two such sends before any row exists: one execution-fact send for the
/// branch's concrete filter, then the initial current-state frame.
///
/// Nor does `publish` order itself ahead of them. `Ok` is local acceptance, and
/// the engine thread replies to `Cmd::PublishTracked` BEFORE it dispatches that
/// write's effects, so acceptance never proves the write's `EmitRows` is
/// already the pending transition. "The frame right after a publish carries the
/// row" is therefore a scheduling assumption, not a property -- the same shape
/// #1166 tracks, and what #1231 recorded when a loaded runner let this
/// observation consume the two empty opening frames one at a time instead of
/// conflated. Settle on the fact the assertion actually depends on: this
/// observation delivered the row.
async fn ticket_holding_a_transition(stream: &NmpRowStream) -> (Arc<NmpRowPull>, FfiFrame) {
    for _ in 0..64 {
        let ticket = stream.begin_next().expect("stream ticket is available");
        let frame = receive(&ticket)
            .await
            .expect("the observation is still delivering");
        if !frame.deltas.is_empty() {
            return (ticket, frame);
        }
        ticket
            .commit()
            .expect("a frame carrying no transition commits like any other");
    }
    panic!("the observation never delivered a row transition");
}

/// Wait until the engine has composed `expected` matching rows, observed
/// through an *independent* observation.
///
/// A write receipt reports local acceptance; it does not report that some other
/// observation's reducer has already folded that row into its mailbox. A test
/// that publishes and then immediately asserts a composed-successor frame is
/// therefore racing the producer, not testing composition -- which is how
/// `repeated_ready_cancellation_keeps_one_claim_and_one_composed_successor`
/// could report 128 rows instead of 129 on a loaded runner. This is the same
/// family as #1166: establish the precondition by observation.
async fn await_rows_composed(engine: &NmpEngine, expected: usize) {
    let witness = engine
        .observe(note_query(), None)
        .expect("witness observation opens");
    let mut ids = BTreeSet::new();
    for _ in 0..256 {
        let Some(frame) = next_committed(&witness).await else {
            break;
        };
        apply_ids(&mut ids, &frame);
        if ids.len() >= expected {
            witness.cancel();
            return;
        }
    }
    witness.cancel();
    panic!(
        "the engine never composed {expected} rows (an independent observation saw {})",
        ids.len()
    );
}

/// A windowed frame carries its whole view in `window`, not as deltas.
fn window_row_ids(frame: &FfiFrame) -> BTreeSet<String> {
    frame
        .window
        .as_ref()
        .expect("a windowed frame carries its view")
        .rows
        .iter()
        .map(|row| row.id.clone())
        .collect()
}

fn apply_ids(ids: &mut BTreeSet<String>, frame: &FfiFrame) {
    for delta in &frame.deltas {
        match delta {
            FfiRowDelta::Added { row } => {
                ids.insert(row.id.clone());
            }
            FfiRowDelta::Updated { row } => {
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
    let (first_ticket, first) = ticket_holding_a_transition(&stream).await;

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

    // Which successor frame carries the second write is not a property either,
    // so hold the rule against EVERY frame after the commit and settle on the
    // row set converging rather than on one frame's contents.
    let mut ids = BTreeSet::new();
    apply_ids(&mut ids, &first);
    let mut converged = false;
    for _ in 0..64 {
        let successor = next_committed(&stream)
            .await
            .expect("updates composed while the claim was retained");
        assert_ne!(
            successor, first,
            "a committed frame is never replayed by the successor pull"
        );
        apply_ids(&mut ids, &successor);
        if ids.len() == 2 {
            converged = true;
            break;
        }
    }
    assert!(
        converged,
        "the replayed baseline plus composed successors converge exactly on both rows (saw {})",
        ids.len()
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
    // Settle the producer through an independent witness, so the assertion
    // below is about composition being bounded rather than about whether the
    // last write had reached this observation yet.
    await_rows_composed(&engine, 129).await;

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

/// Start-once is a state invariant, not a scheduling one (#1166).
///
/// The refusal is the same in every phase after the first `receive` starts, so
/// the property is "one ticket admits exactly one receive, whatever the first
/// one is doing" -- true under every interleaving. The earlier spelling asserted
/// instead that the first receive was *parked* at the moment the second began,
/// which is not a property of the ticket at all: it required the mailbox to be
/// empty, which a single warm-up pull does not establish, so engine startup
/// work on a loaded runner could make it fail on branches touching no row-pull
/// code. Both halves below establish their starting state by observation.
#[tokio::test]
async fn one_ticket_admits_exactly_one_receive_whatever_the_first_is_doing() {
    // Half one: the first receive has started and has not resolved.
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    let (pending, first_receive) = parked_ticket(&stream).await;
    assert_eq!(
        pending.receive().await.unwrap_err(),
        FfiRowPullError::ReceiveAlreadyStarted,
        "one ticket cannot start a second Rust future alongside the first"
    );
    stream.cancel();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), first_receive)
            .await
            .expect("cancellation wakes the accepted receiver within five seconds"),
        Ok(None),
        "stream cancellation wakes the one accepted receiver"
    );

    // Half two: the first receive has already resolved with a frame.
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    drain(&stream).await;
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

/// The same restatement as the start-once falsifier above (#1166): assert what
/// the next delivery *is*, not which scheduling phase it was in.
///
/// A windowed frame carries the whole current view, so aborting one discards it
/// rather than retaining it. That is observable without any timing assumption:
/// abort a view, publish another row, and require the next ticket to return the
/// grown view. A wrongly retained snapshot would hand back the smaller one.
#[tokio::test]
async fn abort_does_not_replay_a_self_contained_window_snapshot() {
    let engine = test_engine();
    let stream = engine
        .observe(
            note_query(),
            Some(FfiWindow::Expandable { initial: 2, max: 2 }),
        )
        .expect("windowed observation opens");
    publish_note(&engine, 1).await;

    // Establish the aborted view by observation: pull until the engine's view
    // actually holds the first row, then abort exactly that snapshot.
    let mut aborted_view = BTreeSet::new();
    for _ in 0..64 {
        let ticket = stream.begin_next().expect("snapshot ticket begins");
        let frame = receive(&ticket).await.expect("a window snapshot arrives");
        let view = window_row_ids(&frame);
        if view.len() == 1 {
            aborted_view = view;
            ticket.abort();
            break;
        }
        ticket.commit().expect("an earlier view commits");
    }
    assert_eq!(
        aborted_view.len(),
        1,
        "the aborted snapshot is the view holding exactly the first row"
    );

    publish_note(&engine, 2).await;

    let retry = stream.begin_next().expect("abort releases the ticket");
    let fresh = receive(&retry).await.expect("a current view arrives");
    retry.commit().expect("the current view commits");
    let fresh_view = window_row_ids(&fresh);
    assert!(
        fresh_view.is_superset(&aborted_view) && fresh_view.len() == 2,
        "the next ticket returns the engine's current view, never the aborted snapshot replayed"
    );

    stream.cancel();
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

/// An app killed mid-pull releases the ticket object without a word: no
/// `commit`, no `abort`, no chance to run cleanup. `Drop` must roll the
/// transition back exactly as an explicit rollback does, or that app loses a
/// row forever.
#[tokio::test]
async fn dropping_a_ticket_without_settling_rolls_the_delta_back_like_abort() {
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    drain(&stream).await;
    publish_note(&engine, 1).await;

    let first = {
        let (ticket, frame) = ticket_holding_a_transition(&stream).await;
        // Neither committed nor aborted: the ticket is simply released.
        drop(ticket);
        frame
    };

    let retry = stream
        .begin_next()
        .expect("dropping the ticket released the claim");
    assert_eq!(
        receive(&retry).await,
        Some(first),
        "Drop restores the exact undelivered transition"
    );
    retry.commit().expect("the replayed delta commits");

    engine.shutdown();
}

/// Cancelling a pull that had nothing to deliver is the common case (a screen
/// closes while the query is idle). It must leave the observation able to
/// deliver a row produced afterwards, and must not hold the claim.
#[tokio::test]
async fn abandoning_a_parked_ticket_loses_no_row_produced_afterwards() {
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");

    let (parked, receiving) = parked_ticket(&stream).await;
    drop(receiving); // generated free of the Rust future
    parked.abort(); // wrapper `finally`
    drop(parked);

    publish_note(&engine, 1).await;

    // The claim was released, so tickets are available again, and the row
    // produced after the abandonment still reaches this observation -- that
    // fact, not which frame carries it, is what "loses no later row" means.
    let (retry, _) = ticket_holding_a_transition(&stream).await;
    retry.commit().expect("the delta commits");

    engine.shutdown();
}

/// Rollback can win while the Rust receive is still resolving. The transition
/// that arrives in that window belongs to the next pull, and the cancelled
/// caller learns the outcome through a typed refusal rather than a frame it
/// must decide whether to trust.
#[tokio::test]
async fn aborting_a_waiting_ticket_retains_the_delta_that_arrives_meanwhile() {
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");

    let (parked, receiving) = parked_ticket(&stream).await;
    parked.abort();
    publish_note(&engine, 1).await;

    let outcome = tokio::time::timeout(Duration::from_secs(5), receiving)
        .await
        .expect("the waiting receive resolves within five seconds");
    assert_eq!(
        outcome,
        Err(FfiRowPullError::Aborted),
        "a rollback that wins mid-flight is a typed refusal, never a delivery"
    );

    // The claim was released, and the row produced during the rollback window
    // still reaches this observation rather than dying with the refused ticket.
    let (retry, _) = ticket_holding_a_transition(&stream).await;
    retry.commit().expect("the retained delta commits");

    engine.shutdown();
}

/// Engine shutdown and observation withdrawal are different facts. Withdrawal
/// discards a retained candidate on purpose; shutdown must not, or a transition
/// the engine already produced disappears between the app's last two pulls.
#[tokio::test]
async fn a_retained_delta_survives_engine_shutdown_and_precedes_the_terminal_result() {
    let engine = test_engine();
    let stream = engine
        .observe(note_query(), None)
        .expect("observation opens");
    drain(&stream).await;
    publish_note(&engine, 1).await;

    let (ticket, retained) = ticket_holding_a_transition(&stream).await;
    ticket.abort();

    engine.shutdown();

    let replay = stream
        .begin_next()
        .expect("shutdown is not observation withdrawal");
    assert_eq!(
        receive(&replay).await,
        Some(retained.clone()),
        "a transition the engine already produced is still delivered after shutdown"
    );
    replay.commit().expect("the retained delta commits");

    // End of stream FOLLOWS the retained transition; it is not required to be
    // the immediately next frame, because frames already pending when the
    // engine stopped are still the observation's to deliver.
    let mut terminated = false;
    for _ in 0..64 {
        let terminal = stream.begin_next().expect("terminal ticket begins");
        let frame = receive(&terminal).await;
        terminal.commit().expect("the terminal result commits");
        assert_ne!(
            frame.as_ref(),
            Some(&retained),
            "a committed transition is never replayed after shutdown"
        );
        if frame.is_none() {
            terminated = true;
            break;
        }
    }
    assert!(
        terminated,
        "end of stream follows the retained transition, never precedes it"
    );
}
