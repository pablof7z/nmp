//! #1039 through the FFI boundary: the app reads its own publish queue back,
//! and removes one entry from it.
//!
//! Enumeration answers "what have I got outstanding, and what went wrong with
//! it" for an app that has not held a receipt stream open since acceptance.
//! Removal is the companion half and is not optional: a write parked forever
//! on a signer that never attached, and a permanently-failed refused entry,
//! end only by the app's own decision — so removal is a termination path, not
//! housekeeping. A write that still owns open delivery work is refused with
//! `StillActive`; the app cancels that one first and removes the terminal
//! receipt cancellation leaves behind.

use std::time::Duration;

use nmp_ffi::facade::{NmpEngine, NmpEngineConfig};
use nmp_ffi::session::FfiPrivateKey;
use nmp_ffi::types::{
    FfiIdentity, FfiPublishQueueError, FfiRefuseReason, FfiRemoveQueueEntryError, FfiSigningState,
    FfiWriteFact, FfiWriteIntent, FfiWritePayload, FfiWriteRouting,
};
use nmp_store::{EventStore, RefuseReason};

/// Seed a real `RedbStore` with one write the acceptance door REFUSED: a
/// whole-value replacement that lost its compare-and-swap. Publish took
/// custody of it (one row, permanently failed), which is exactly the thing
/// the enumeration door exists to show. There is no FFI payload that can
/// express a CAS-guarded edit, so the custody row is seeded through the store
/// the engine itself uses.
fn seed_refused_entry(
    path: &std::path::Path,
    keys: &nostr::Keys,
    expected: nostr::EventId,
    actual: nostr::EventId,
) -> (u64, nostr::EventId) {
    let mut store = nmp_store::RedbStore::open(path).expect("open store");
    let signed = nostr::EventBuilder::new(nostr::Kind::Custom(30_000), "refused edit")
        .custom_created_at(nostr::Timestamp::from(1_000u64))
        .sign_with_keys(keys)
        .expect("fixture event signs");
    let receipt_id = store
        .accept_refused(
            signed.id,
            keys.public_key(),
            RefuseReason::ReplaceableBaseChanged {
                expected: Some(expected),
                actual: Some(actual),
            },
        )
        .expect("the refusal is taken into custody as a queue entry");
    (receipt_id, signed.id)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_entry_is_enumerable_with_both_ids_and_removal_is_its_only_exit() {
    let fixture = tempfile::tempdir().expect("tempdir");
    let path = fixture.path().join("refused-queue.redb");
    let keys = nostr::Keys::generate();
    let expected = nostr::EventId::from_slice(&[0x11; 32]).unwrap();
    let actual = nostr::EventId::from_slice(&[0x22; 32]).unwrap();
    let (receipt_id, frozen_id) = seed_refused_entry(&path, &keys, expected, actual);

    let engine = NmpEngine::new(
        NmpEngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..NmpEngineConfig::default()
        },
        None,
    )
    .expect("engine opens over the seeded store");

    let entries = engine.publish_queue(None, u8::MAX).expect("engine is open");
    assert_eq!(entries.len(), 1, "exactly the refused entry: {entries:?}");
    let entry = &entries[0];
    assert_eq!(entry.receipt_id, receipt_id);
    assert_eq!(
        entry.event_id,
        frozen_id.to_hex(),
        "the frozen id is the write's identity from acceptance onward"
    );
    assert_eq!(entry.pubkey, keys.public_key().to_hex());
    assert_eq!(
        entry.outcome,
        Some(nmp_ffi::types::FfiWriteOutcome::Refused {
            reason: FfiRefuseReason::ReplaceableBaseChanged {
                // BOTH ids survive whole. Reduced to a string this failure
                // could only tell a user to redo the edit; with the pair an
                // app fetches `actual`, reapplies the change and resubmits.
                expected: Some(expected.to_hex()),
                actual: Some(actual.to_hex()),
            }
        }),
        "a refused entry reports WHY, with both event ids intact"
    );

    engine
        .remove_publish_queue_entry(receipt_id)
        .expect("a permanently-failed entry is removable");
    assert!(
        engine
            .publish_queue(None, u8::MAX)
            .expect("engine is open")
            .is_empty(),
        "removal is a real termination path: the entry is gone"
    );

    assert_eq!(
        engine.remove_publish_queue_entry(receipt_id),
        Err(FfiRemoveQueueEntryError::UnknownReceipt { receipt_id }),
        "a second removal names the receipt it could not find, rather than \
         reporting a silent success"
    );

    engine.shutdown();
}

#[test]
fn removing_a_receipt_that_never_existed_is_an_unknown_receipt() {
    let engine = NmpEngine::new(NmpEngineConfig::default(), None).expect("engine builds");
    assert!(engine
        .publish_queue_for_event("00".repeat(32), None, u8::MAX)
        .expect("an exact absent event is an empty page")
        .is_empty());
    assert!(matches!(
        engine.publish_queue_for_event("not-an-event-id".to_owned(), None, u8::MAX),
        Err(FfiPublishQueueError::InvalidEventId { .. })
    ));
    assert_eq!(
        engine.remove_publish_queue_entry(u64::MAX),
        Err(FfiRemoveQueueEntryError::UnknownReceipt {
            receipt_id: u64::MAX
        })
    );
    assert!(engine
        .publish_queue(None, u8::MAX)
        .expect("engine is open")
        .is_empty());
    engine.shutdown();
}

/// Removal is for entries nothing is going to move. A write that is signed
/// and has a live relay lane still owns open delivery work, so the door
/// refuses and names the repair: cancel it first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn removing_an_entry_with_open_delivery_work_is_still_active() {
    let keys = nostr::Keys::generate();
    let engine = NmpEngine::new(
        NmpEngineConfig {
            // A destination that exists as an instruction and never as a
            // connection: the lane is owned, live and nonterminal for the whole
            // test, which is exactly the state removal must refuse.
            app_relays: vec!["ws://127.0.0.1:1/".to_string()],
            allowed_local_relay_hosts: vec!["127.0.0.1".to_string()],
            ..NmpEngineConfig::default()
        },
        None,
    )
    .expect("engine builds");
    engine
        .add_private_key_account(
            FfiPrivateKey::from_bytes(keys.secret_key().to_secret_bytes().to_vec()).unwrap(),
            true,
        )
        .expect("account activates");

    let receipt = engine
        .publish(FfiWriteIntent {
            payload: FfiWritePayload::Event {
                builder: nmp_ffi::types::FfiEventBuilder {
                    kind: 9999,
                    tags: vec![],
                    content: "open delivery work".to_string(),
                    created_at: Some(1_000),
                },
            },
            routing: FfiWriteRouting::Explicit {
                relays: vec!["ws://127.0.0.1:1/".to_string()],
            },
            identity: FfiIdentity::Active,
            correlation: None,
        })
        .expect("publish takes custody");
    let receipt_id = receipt.id();

    // Drive the write to the state under test: signed, with a relay lane
    // opened for it. Reading the facts is what makes "open delivery work" a
    // fact rather than a sleep.
    let mut signed = false;
    let mut lane_open = false;
    while !(signed && lane_open) {
        let fact = tokio::time::timeout(Duration::from_secs(5), receipt.next())
            .await
            .expect("a fact arrives within the lifecycle bound")
            .expect("receipt next() is not a misuse")
            .expect("the write does not end before it is signed");
        match fact {
            FfiWriteFact::Signing {
                state: FfiSigningState::Signed { .. },
            } => signed = true,
            FfiWriteFact::Relay { .. } => lane_open = true,
            _ => {}
        }
    }

    assert_eq!(
        engine.remove_publish_queue_entry(receipt_id),
        Err(FfiRemoveQueueEntryError::StillActive { receipt_id }),
        "removal is for entries nothing is going to move; this one is live"
    );
    assert!(
        engine
            .publish_queue(None, u8::MAX)
            .expect("engine is open")
            .iter()
            .any(|entry| entry.receipt_id == receipt_id),
        "the refused removal changed nothing -- the entry is still enumerable"
    );

    engine.shutdown();
}
