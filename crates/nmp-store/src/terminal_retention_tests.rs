use std::path::Path;

use nmp_grammar::CorrelationToken;
use nostr::{Event, EventBuilder, Keys, Kind, Timestamp};

use crate::terminal_retention::TerminalRetentionLimits;
use crate::{
    sentinel_signature, AcceptOutcome, AcceptWrite, AcceptWritePayload, IntentSigState,
    PublishQueueReceipt, PublishQueueReceiptPayload, ReceiptState, RedbStore, RefuseReason,
    RemoveQueueEntryOutcome,
};

fn maintain_at(
    store: &mut RedbStore,
    now: Timestamp,
    limits: TerminalRetentionLimits,
) -> Result<Vec<u64>, crate::PersistenceError> {
    crate::redb_store::publish_queue_ops::maintain_terminal_receipts_at(store, now, limits)
}

fn frozen(keys: &Keys, content: &str, created_at: u64) -> Event {
    let signed = EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap();
    Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags,
        signed.content,
        sentinel_signature(),
    )
}

fn accept(
    store: &mut RedbStore,
    keys: &Keys,
    content: &str,
    created_at: u64,
    correlation: Option<&str>,
) -> AcceptOutcome {
    store
        .accept_write(AcceptWrite {
            payload: AcceptWritePayload::Event {
                frozen: Box::new(frozen(keys, content, created_at)),
                replaceable_base: None,
                monotonic_stamp: false,
                routing: "auto".to_owned(),
                sig_state: IntentSigState::Pending,
            },
            expected_pubkey: keys.public_key(),
            signing_identity_ref: "terminal-retention-test".to_owned(),
            accepted_at: Timestamp::from(created_at),
            correlation: correlation.map(|value| CorrelationToken::try_from(value).unwrap()),
        })
        .unwrap()
}

fn assert_event_receipt_state(receipt: PublishQueueReceipt, expected: ReceiptState) {
    match receipt.payload {
        PublishQueueReceiptPayload::Event { event_id, state } => {
            assert_eq!(state, expected, "unexpected state for event {event_id}");
        }
        PublishQueueReceiptPayload::ReplaceableOperation {
            coordinate, state, ..
        } => {
            panic!(
                "expected an event receipt, got replaceable operation {coordinate:?} in state {state:?}"
            );
        }
    }
}

fn exercise_global_fifo(store: &mut RedbStore) {
    let keys = Keys::generate();
    let cancelled = accept(store, &keys, "cancelled", 1, None);
    let cancelled_id = cancelled.journaled_receipt_id().unwrap();

    let refused = store
        .accept_refused(
            nostr::EventId::all_zeros(),
            keys.public_key(),
            RefuseReason::Tombstoned,
        )
        .unwrap();
    store
        .cancel_write(cancelled.journaled_intent_id().unwrap())
        .unwrap();

    let unroutable = accept(store, &keys, "unroutable", 3, None);
    let unroutable_id = unroutable.journaled_receipt_id().unwrap();
    store
        .close_unroutable_intent(unroutable.journaled_intent_id().unwrap())
        .unwrap();

    let open = accept(store, &keys, "still open", 4, None);
    let open_id = open.journaled_receipt_id().unwrap();

    let removed = maintain_at(
        store,
        Timestamp::from(u64::MAX / 4),
        TerminalRetentionLimits {
            max_age_secs: u64::MAX,
            max_count: 2,
            max_bytes: u64::MAX,
        },
    )
    .unwrap();
    assert_eq!(removed, vec![refused]);
    assert!(store.reattach_receipt(refused).unwrap().is_none());
    assert_event_receipt_state(
        store.reattach_receipt(cancelled_id).unwrap().unwrap(),
        ReceiptState::Cancelled,
    );
    assert_event_receipt_state(
        store.reattach_receipt(unroutable_id).unwrap().unwrap(),
        ReceiptState::NoDestination,
    );
    assert!(store.reattach_receipt(open_id).unwrap().is_some());
    assert_eq!(
        store.remove_publish_queue_entry(open_id).unwrap(),
        RemoveQueueEntryOutcome::StillOpen
    );
}

#[test]
fn all_terminal_receipt_kinds_share_one_fifo() {
    exercise_global_fifo(&mut RedbStore::temporary().expect("temporary Redb store"));
}

#[test]
fn terminal_receipt_fifo_survives_redb_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("reopen-fifo.redb");
    let keys = Keys::generate();
    let receipt_id = {
        let mut store = RedbStore::open(&path).unwrap();
        let accepted = accept(
            &mut store,
            &keys,
            "cancel after reopen",
            10,
            Some("reopen-token"),
        );
        let receipt_id = accepted.journaled_receipt_id().unwrap();
        store
            .cancel_write(accepted.journaled_intent_id().unwrap())
            .unwrap();
        receipt_id
    };

    let mut reopened = RedbStore::open(Path::new(&path)).unwrap();
    assert_eq!(
        reopened.lookup_correlation("reopen-token").unwrap(),
        Some(receipt_id)
    );
    assert_eq!(
        maintain_at(
            &mut reopened,
            Timestamp::from(u64::MAX / 4),
            TerminalRetentionLimits {
                max_age_secs: 0,
                max_count: u64::MAX,
                max_bytes: u64::MAX,
            },
        )
        .unwrap(),
        vec![receipt_id]
    );
    assert!(reopened.reattach_receipt(receipt_id).unwrap().is_none());
    assert_eq!(reopened.lookup_correlation("reopen-token").unwrap(), None);
}

#[test]
fn terminal_age_count_and_bytes_each_force_whole_eviction() {
    for trigger in ["age", "count", "bytes"] {
        exercise_limit(
            trigger,
            &mut RedbStore::temporary().expect("temporary Redb store"),
        );
    }
}

fn exercise_limit(trigger: &str, store: &mut RedbStore) {
    let keys = Keys::generate();
    let receipt_id = store
        .accept_refused(
            nostr::EventId::all_zeros(),
            keys.public_key(),
            RefuseReason::Tombstoned,
        )
        .unwrap();
    let limits = match trigger {
        "age" => TerminalRetentionLimits {
            max_age_secs: 0,
            max_count: u64::MAX,
            max_bytes: u64::MAX,
        },
        "count" => TerminalRetentionLimits {
            max_age_secs: u64::MAX,
            max_count: 0,
            max_bytes: u64::MAX,
        },
        _ => TerminalRetentionLimits {
            max_age_secs: u64::MAX,
            max_count: u64::MAX,
            max_bytes: 0,
        },
    };
    assert_eq!(
        maintain_at(store, Timestamp::from(u64::MAX / 4), limits).unwrap(),
        vec![receipt_id],
        "{trigger} must independently force eviction"
    );
    assert!(store.reattach_receipt(receipt_id).unwrap().is_none());
}
