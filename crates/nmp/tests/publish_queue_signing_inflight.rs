//! #1261 — the publish queue must not report a signature in flight as a
//! write parked on a signer nobody has.
//!
//! Two writes, one real engine, one enumeration:
//!
//! - **A** has an attached signer that has taken the request and not
//!   answered yet. This is the ordinary state of every healthy write between
//!   acceptance and signature promotion. It is transient and normal, and it
//!   ends when the signer answers.
//! - **B** names a key no signer answers for. Nothing is in flight and
//!   nothing ever will be until a signer for THAT key attaches. No clock
//!   ends it; the app cancelling it and then removing its entry is the only
//!   other exit, which is the termination path #1039's removal door exists
//!   to serve.
//!
//! Collapsing the two makes every healthy write read as stuck (mosaico's
//! first `mosaico doctor` draft did exactly that) and leaves the genuinely
//! parked write — the one whose only termination path is the app's own
//! decision — invisible.

use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nmp::mechanism::core::RelayAdmissionPolicy;
use nmp::mechanism::publish_queue::{PublishQueueEntry, SigningState, WriteFact};
use nmp::mechanism::runtime::{
    EngineThread, FifoReceiver, FifoRecvTimeoutError, ReceiptReattachment,
};
use nmp_grammar::{Identity, WriteIntent, WritePayload, WriteRouting};
use nmp_router::FixtureRoutingFacts;
use nmp_signer::{
    PendingSignerSender, SignerOp, SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent,
    SigningCapability,
};
use nmp_store::MemoryStore;
use nostr::{Keys, Kind, PublicKey, Timestamp, UnsignedEvent};

/// A signer that answers for its key and then holds the request open. It is
/// the ONLY thing that makes "a signature is in flight" observable for a
/// deterministic length of time: a real remote signer's round trip is a
/// window this widens rather than a state it invents.
struct HeldSigner {
    pubkey: PublicKey,
    /// Fires once the engine has actually handed this signer the request.
    started: mpsc::Sender<()>,
    /// Kept alive so the pending op is never resolved or cancelled.
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

fn body_of(unsigned: &UnsignedEvent) -> nmp_grammar::EventBuilder {
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

fn note(author: PublicKey, content: &str) -> nmp_grammar::EventBuilder {
    body_of(&UnsignedEvent::new(
        author,
        Timestamp::now(),
        Kind::TextNote,
        vec![],
        content,
    ))
}

fn entry_for(entries: &[PublishQueueEntry], pubkey: PublicKey) -> &PublishQueueEntry {
    entries
        .iter()
        .find(|entry| entry.pubkey == pubkey)
        .unwrap_or_else(|| panic!("the queue must hold the write accepted for {pubkey}"))
}

#[test]
fn a_signature_in_flight_is_not_reported_as_a_write_parked_on_a_missing_signer() {
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

    // ---- A: a real signer holds the request open -------------------------
    let held_receipt = handle
        .publish(WriteIntent {
            payload: WritePayload::Event(note(held_pubkey, "signature in flight")),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(held_pubkey),
            correlation: None,
        })
        .expect("receipt id allocation")
        .statuses;
    started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("the engine must hand the attached signer the request");

    // ---- B: nobody answers for this key at all ---------------------------
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
    // A's signer has still not answered, so A is still in flight.
    assert!(
        !wait_for_status(&held_receipt, Duration::from_millis(200), |fact| matches!(
            fact,
            WriteFact::Signing(_)
        )),
        "the held signer must not have answered yet"
    );

    let entries = handle
        .publish_queue_entries(None, u8::MAX)
        .expect("enumerating the publish queue");
    assert_eq!(entries.len(), 2, "both writes are in custody");
    let in_flight = entry_for(&entries, held_pubkey);
    let parked = entry_for(&entries, parked_pubkey);

    assert_eq!(
        parked.signing,
        SigningState::AwaitingSigner {
            pubkey: parked_pubkey
        },
        "the genuinely parked write must name the key nobody answers for -- this is the \
         entry an app surfaces to a person, because no clock ends it"
    );
    assert!(
        !matches!(in_flight.signing, SigningState::AwaitingSigner { .. }),
        "#1261: a healthy write whose attached signer is mid-round-trip was reported as \
         {:?} -- indistinguishable from the parked entry above, so every healthy write \
         reads as stuck and the genuinely parked one cannot be found",
        in_flight.signing
    );
    assert_eq!(
        in_flight.signing,
        SigningState::InFlight {
            pubkey: held_pubkey
        },
        "a signer holding the request is reported as such, naming the key it is signing for"
    );

    // The same distinction on the reattach door: an app that comes back to a
    // persisted receipt id must be told which of the two it is, not the
    // pessimistic one.
    let replayed = match handle.reattach_receipt(in_flight.receipt_id) {
        ReceiptReattachment::Attached { statuses, .. } => statuses,
        ReceiptReattachment::NotFound => panic!("the in-flight receipt is retained"),
        ReceiptReattachment::RetainedButUnreadable => {
            panic!("the in-flight receipt's evidence is readable")
        }
    };
    assert!(
        wait_for_status(&replayed, Duration::from_secs(5), |fact| fact
            == &WriteFact::Signing(SigningState::InFlight {
                pubkey: held_pubkey
            })),
        "the reattach replay must report the in-flight signature, not a missing signer"
    );

    drop(held);
    drop(parked_receipt);
    handle.shutdown();
    engine_thread.join();
}
