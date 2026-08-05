//! #1269 — a write parked on a signer nobody has must be REMOVABLE.
//!
//! #1039 made removal a TERMINATION path rather than housekeeping:
//!
//! > a write parked on a missing signer, and a permanently-failed entry, end
//! > **ONLY** by the app removing them
//!
//! and `SigningState::AwaitingSigner` says the same in its own docstring —
//! no clock ends the park, so the app removing the entry is its only other
//! exit. Both sentences were false for exactly the case they name: the door
//! refused every receipt still in the reducer's `pending` map, and a
//! signer-parked write is in that map forever by design (the obligation is
//! still owned, and a signer may still arrive).
//!
//! The guard was protecting something real, which is why this is not a
//! relaxation. Two writes, one real engine:
//!
//! - **PARKED** names a key no signer answers for. Nothing is in motion: no
//!   signer holds a request, no signature exists, so no lane exists and no
//!   relay can ever answer. Only the app can end it.
//! - **IN FLIGHT** has an attached signer holding the request open. An
//!   answer is already on its way, and removing it would destroy a write
//!   that is about to succeed. The door must still refuse.
//!
//! #1270 is what makes the two distinguishable in reducer state; this is
//! what makes the first one removable.

use std::collections::BTreeSet;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nmp::mechanism::core::{RelayAdmissionPolicy, RowDelta};
use nmp::mechanism::publish_queue::{
    NotSentReason, PublishQueueEntry, RemoveQueueEntryError, SigningState, WriteFact, WriteOutcome,
};
use nmp::mechanism::runtime::{EngineThread, FifoReceiver, FifoRecvTimeoutError, RowsReceiver};
use nmp_grammar::{
    Binding, Demand, Filter, Identity, LiveQuery, WriteIntent, WritePayload, WriteRouting,
};
use nmp_router::FixtureRoutingFacts;
use nmp_signer::{
    PendingSignerSender, SignerOp, SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent,
    SigningCapability,
};
use nmp_store::MemoryStore;
use nostr::{EventId, Keys, Kind, PublicKey, Timestamp, UnsignedEvent};

/// A signer that answers for its key and then holds the request open, so
/// "a signature is in flight" is observable for a deterministic window
/// rather than a race against a real signer's round trip.
struct HeldSigner {
    pubkey: PublicKey,
    started: mpsc::Sender<()>,
    held: Arc<Mutex<Vec<PendingSignerSender<SignerSignedEvent>>>>,
}

impl SigningCapability for HeldSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(SignerPublicKey::new(self.pubkey.to_bytes()))
    }

    fn sign(&self, _unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        let (sender, operation) = SignerOp::pending_channel();
        self.held.lock().expect("held senders").push(sender);
        let _ = self.started.send(());
        operation
    }
}

fn note(author: PublicKey, content: &str) -> nmp_grammar::EventBuilder {
    let unsigned = UnsignedEvent::new(
        author,
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        content.to_string(),
    );
    nmp_grammar::EventBuilder {
        kind: unsigned.kind,
        tags: unsigned.tags.iter().cloned().collect(),
        content: unsigned.content.clone(),
        created_at: Some(unsigned.created_at),
    }
}

fn wait_for_status(
    rx: &FifoReceiver<WriteFact>,
    timeout: Duration,
    pred: impl Fn(&WriteFact) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        match rx.recv_timeout(remaining) {
            Ok(status) if pred(&status) => return true,
            Ok(_) => {}
            Err(FifoRecvTimeoutError::Timeout | FifoRecvTimeoutError::Closed) => return false,
            Err(FifoRecvTimeoutError::Lagged) => panic!("fixture receipt stream must not lag"),
        }
    }
}

fn wait_for_row(rows: &RowsReceiver, timeout: Duration, pred: impl Fn(&RowDelta) -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let Ok((deltas, _, _)) = rows.recv_timeout(remaining) else {
            return false;
        };
        if deltas.iter().any(&pred) {
            return true;
        }
    }
}

fn entry_for(entries: &[PublishQueueEntry], pubkey: PublicKey) -> &PublishQueueEntry {
    entries
        .iter()
        .find(|entry| entry.pubkey == pubkey)
        .unwrap_or_else(|| panic!("the queue must hold the write accepted for {pubkey}"))
}

fn notes_by(author: PublicKey) -> LiveQuery {
    LiveQuery::single(Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([1u16])),
        authors: Some(Binding::Literal(BTreeSet::from([author.to_hex()]))),
        ..Filter::default()
    }))
}

#[test]
fn a_write_parked_on_a_missing_signer_is_removable_and_one_being_signed_is_not() {
    let held_keys = Keys::generate();
    let parked_keys = Keys::generate();
    let held_pubkey = held_keys.public_key();
    let parked_pubkey = parked_keys.public_key();

    let (started_tx, started_rx) = mpsc::channel();
    let held: Arc<Mutex<Vec<PendingSignerSender<SignerSignedEvent>>>> =
        Arc::new(Mutex::new(Vec::new()));

    let (engine_thread, handle) = EngineThread::spawn_with_fixture_routing_facts(
        MemoryStore::new(),
        FixtureRoutingFacts::new(),
        10,
        Default::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("test engine thread construction");

    handle
        .add_signer(HeldSigner {
            pubkey: held_pubkey,
            started: started_tx,
            held: held.clone(),
        })
        .expect("HeldSigner always reports a public key");

    // The app's own live query over the parked author. A locally accepted
    // write is visible here immediately -- that optimistic row is the
    // obligation's local promise, and releasing the obligation has to take
    // it back.
    let (_query, rows) = handle
        .subscribe(notes_by(parked_pubkey))
        .expect("subscribing to the parked author's notes");

    // ---- PARKED: nobody answers for this key at all ----------------------
    let parked_receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(note(parked_pubkey, "no signer has this key")),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(parked_pubkey),
            correlation: None,
        })
        .expect("receipt id allocation")
        .statuses;
    assert!(
        wait_for_status(&parked_receipt, Duration::from_secs(5), |fact| matches!(
            fact,
            WriteFact::Signing(SigningState::AwaitingSigner { .. })
        )),
        "a key with no registered signer must park as AwaitingSigner"
    );

    // ---- IN FLIGHT: a real signer holds the request open ------------------
    let held_receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(note(held_pubkey, "signature in flight")),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(held_pubkey),
            correlation: None,
        })
        .expect("receipt id allocation");
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the engine must hand the attached signer the request");

    let entries = handle
        .publish_queue_entries()
        .expect("enumerating the publish queue");
    assert_eq!(entries.len(), 2, "both writes are in custody");
    let parked = entry_for(&entries, parked_pubkey).clone();
    let in_flight = entry_for(&entries, held_pubkey).clone();
    assert_eq!(
        parked.signing,
        SigningState::AwaitingSigner {
            pubkey: parked_pubkey
        },
        "the parked entry names the key nobody answers for"
    );
    assert_eq!(
        in_flight.signing,
        SigningState::InFlight {
            pubkey: held_pubkey
        },
        "the other entry has a signer holding its request"
    );

    let parked_event: EventId = parked.event_id;
    assert!(
        wait_for_row(&rows, Duration::from_secs(5), |delta| matches!(
            delta,
            RowDelta::Added(row) if row.event.id == parked_event
        )),
        "the accepted write is visible through the app's own live query"
    );

    // ---- the defect ------------------------------------------------------
    assert_eq!(
        handle.remove_publish_queue_entry(parked.receipt_id),
        Ok(()),
        "#1269: a write parked on a signer nobody has is the exact entry \
         whose ONLY termination path is the app removing it, and the door \
         refused it -- so nothing ends it at all: no clock (by design), no \
         signer (that is the premise), and not removal"
    );

    assert!(
        wait_for_status(&parked_receipt, Duration::from_secs(5), |fact| fact
            == &WriteFact::Outcome(WriteOutcome::NotSent(NotSentReason::Removed))),
        "the receipt stream ends on a fact naming what the app did, not in \
         silence a dropped subscription is indistinguishable from"
    );
    assert!(
        wait_for_row(&rows, Duration::from_secs(5), |delta| matches!(
            delta,
            RowDelta::Removed(id) if *id == parked_event
        )),
        "removing the entry releases the obligation: the optimistic row it \
         promised goes with it, rather than outliving it as a ghost"
    );

    let entries = handle
        .publish_queue_entries()
        .expect("enumerating the publish queue");
    assert!(
        entries
            .iter()
            .all(|entry| entry.receipt_id != parked.receipt_id),
        "removal is a real termination path: the entry is gone -- {entries:?}"
    );
    assert_eq!(
        handle.remove_publish_queue_entry(parked.receipt_id),
        Err(RemoveQueueEntryError::UnknownReceipt {
            receipt_id: parked.receipt_id
        }),
        "a second removal names the receipt it could not find"
    );

    // ---- and the guard the door exists for still holds --------------------
    assert_eq!(
        handle.remove_publish_queue_entry(in_flight.receipt_id),
        Err(RemoveQueueEntryError::StillActive {
            receipt_id: in_flight.receipt_id
        }),
        "a signer HAS this request and its answer is already on the way; \
         removing it would destroy a write that is about to succeed"
    );
    assert!(
        handle
            .publish_queue_entries()
            .expect("enumerating the publish queue")
            .iter()
            .any(|entry| entry.receipt_id == in_flight.receipt_id),
        "the refused removal changed nothing"
    );

    drop(held);
    drop(parked_receipt);
    drop(held_receipt);
    handle.shutdown();
    engine_thread.join();
}
