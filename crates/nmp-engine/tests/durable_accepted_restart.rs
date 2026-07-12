//! Genuine redb close/reopen falsifiers for issues #2/#3 U4. No sleeps,
//! retry timers, or polling: restart is represented by dropping the whole
//! reducer/store and opening the database again.

use std::sync::{Arc, Mutex};

use nmp_engine::core::{Effect, EngineCore, EngineMsg, ReceiptId};
use nmp_engine::outbox::{
    Durability, ReceiptSink, WriteIntent, WritePayload, WriteRouting, WriteStatus,
};
use nmp_router::FixtureDirectory;
use nmp_store::{
    sentinel_signature, AcceptWrite, AttemptOutcome, EventStore, IntentSigState, RedbStore,
    SigState, WriteDurability,
};
use nostr::{EventBuilder, Keys, Kind, PublicKey, RelayUrl, Timestamp, UnsignedEvent};
use redb::{Database, ReadableTable, TableDefinition};

#[derive(Clone, Default)]
struct Sink(Arc<Mutex<Vec<WriteStatus>>>);

impl ReceiptSink for Sink {
    fn on_status(&self, status: WriteStatus) {
        self.0.lock().unwrap().push(status);
    }
}

fn receipt_id(effects: &[Effect]) -> ReceiptId {
    effects
        .iter()
        .find_map(|effect| match effect {
            Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
            _ => None,
        })
        .expect("accepted receipt")
}

fn signed(keys: &Keys, content: &str, created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

fn directory(pk: PublicKey, relay: RelayUrl) -> FixtureDirectory {
    FixtureDirectory::new().with_write(pk.to_hex(), [relay])
}

#[test]
fn durable_started_attempt_replays_exact_bytes_and_same_receipt_without_accepting_again() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("durable.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://durable.example").unwrap();
    let appended = RelayUrl::parse("wss://appended-after-restart.example").unwrap();
    let event = signed(&keys, "exact", 100);

    let id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(Sink::default()),
        ));
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e) if r == &relay && e == &event
        )));
        receipt_id(&effects)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(FixtureDirectory::new().with_write(
            keys.public_key().to_hex(),
            [relay.clone(), appended.clone()],
        )),
        10,
    );
    let recovery = core.recover_on_boot();
    assert!(recovery.iter().any(|effect| matches!(effect,
        Effect::PublishEvent(r, e) if r == &relay && e == &event
    )));
    assert!(
        recovery.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e) if r == &appended && e == &event
        )),
        "a newly resolved relay appends a first lane without overwriting the recovered one"
    );
    assert!(
        !recovery
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(_, WriteStatus::Accepted))),
        "boot recovery must not accept the write a second time"
    );

    let first = Sink::default();
    let second = Sink::default();
    assert!(core.reattach_receipt(id, Box::new(first.clone())));
    assert!(core.reattach_receipt(id, Box::new(second.clone())));
    assert_eq!(
        first.0.lock().unwrap().len(),
        second.0.lock().unwrap().len()
    );
    assert!(first
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|s| matches!(s, WriteStatus::Sent(r) if r == &relay)));
}

#[test]
fn at_most_once_started_attempt_becomes_outcome_unknown_and_is_never_resent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("amo.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://amo.example").unwrap();
    let event = signed(&keys, "once", 101);
    let intent_id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event),
                durability: Durability::AtMostOnce,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(Sink::default()),
        ));
        let id = receipt_id(&effects);
        // Resolve the stable receipt to its intent after the reducer drops.
        drop(core);
        RedbStore::open(&path)
            .unwrap()
            .reattach_receipt(id.0)
            .unwrap()
            .intent_id
            .unwrap()
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(directory(keys.public_key(), relay.clone())),
        10,
    );
    let recovery = core.recover_on_boot();
    assert!(!recovery
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    drop(core);

    let store = RedbStore::open(&path).unwrap();
    let attempts = store.recover_attempts(intent_id).unwrap();
    assert_eq!(attempts.len(), 1);
    assert_eq!(attempts[0].outcome, AttemptOutcome::OutcomeUnknown);
}

#[test]
fn pending_row_and_frozen_signer_resume_after_reopen_then_cancel_compensates() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("signer.redb");
    let keys = Keys::generate();
    let wrong = Keys::generate();
    let relay = RelayUrl::parse("wss://signer.example").unwrap();
    let unsigned = UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(102u64),
        Kind::TextNote,
        vec![],
        "resume",
    );
    let frozen_id = nostr::EventId::new(
        &unsigned.pubkey,
        &unsigned.created_at,
        &unsigned.kind,
        &unsigned.tags,
        &unsigned.content,
    );
    let id = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(Sink::default()),
        ));
        receipt_id(&effects)
    };

    let store = RedbStore::open(&path).unwrap();
    let rows = store.query(&nostr::Filter::new().id(frozen_id));
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].provenance.local.as_ref().unwrap().sig_state,
        SigState::Pending
    );
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    assert!(core.recover_on_boot().is_empty());
    let reattached = Sink::default();
    assert!(core.reattach_receipt(id, Box::new(reattached.clone())));
    assert_eq!(
        *reattached.0.lock().unwrap(),
        vec![WriteStatus::Accepted, WriteStatus::AwaitingCapability]
    );
    assert!(!core
        .handle(EngineMsg::SignerAttached(wrong.public_key()))
        .iter()
        .any(|e| matches!(e, Effect::RequestSign(..))));
    assert!(core
        .handle(EngineMsg::SignerAttached(keys.public_key()))
        .iter()
        .any(|e| matches!(e, Effect::RequestSign(request_id, _, u)
            if *request_id == id && u.pubkey == keys.public_key())));
    core.handle(EngineMsg::CancelWrite(id));
    drop(core);
    let store = RedbStore::open(&path).unwrap();
    assert!(store.query(&nostr::Filter::new().id(frozen_id)).is_empty());
}

#[test]
fn exact_duplicate_coowners_recover_distinct_receipts_and_lossless_routes() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("coowners.redb");
    let keys = Keys::generate();
    let r1 = RelayUrl::parse("wss://one.example").unwrap();
    let r2 = RelayUrl::parse("wss://two.example").unwrap();
    let event = signed(&keys, "shared", 103);
    let (a, b) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(
                FixtureDirectory::new()
                    .with_write(keys.public_key().to_hex(), [r1.clone(), r2.clone()]),
            ),
            10,
        );
        let publish = |core: &mut EngineCore<RedbStore>| {
            core.handle(EngineMsg::Publish(
                WriteIntent {
                    payload: WritePayload::Signed(event.clone()),
                    durability: Durability::Durable,
                    routing: WriteRouting::AuthorOutbox,
                },
                Box::new(Sink::default()),
            ))
        };
        let a = receipt_id(&publish(&mut core));
        let b = receipt_id(&publish(&mut core));
        assert_ne!(a, b);
        (a, b)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(
        store,
        Box::new(
            FixtureDirectory::new()
                .with_write(keys.public_key().to_hex(), [r1.clone(), r2.clone()]),
        ),
        10,
    );
    let effects = core.recover_on_boot();
    let replays = effects
        .iter()
        .filter(|effect| matches!(effect, Effect::PublishEvent(_, replayed) if replayed == &event))
        .count();
    assert_eq!(
        replays, 4,
        "two coowners retain both append-only relay lanes"
    );
    assert!(core.reattach_receipt(a, Box::new(Sink::default())));
    assert!(core.reattach_receipt(b, Box::new(Sink::default())));
}

#[test]
fn malformed_persisted_routing_fails_closed_without_dropping_the_obligation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("malformed-route.redb");
    let keys = Keys::generate();
    let event = signed(&keys, "malformed", 104);
    let frozen = nostr::Event::new(
        event.id,
        event.pubkey,
        event.created_at,
        event.kind,
        event.tags.clone(),
        event.content.clone(),
        sentinel_signature(),
    );
    let (intent_id, receipt_id) = {
        let mut store = RedbStore::open(&path).unwrap();
        let outcome = store
            .accept_write(AcceptWrite {
                frozen,
                expected_pubkey: keys.public_key(),
                signing_identity_ref: keys.public_key().to_hex(),
                durability: WriteDurability::Durable,
                routing: "future-routing-version-with-no-decoder".into(),
                sig_state: IntentSigState::Pending,
                accepted_at: Timestamp::from(104u64),
            })
            .unwrap();
        let intent_id = outcome.journaled_intent_id().unwrap();
        let receipt_id = ReceiptId(outcome.journaled_receipt_id().unwrap());
        store.promote_signed(intent_id, event.sig).unwrap();
        (intent_id, receipt_id)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    let effects = core.recover_on_boot();
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(core.reattach_receipt(receipt_id, Box::new(Sink::default())));
    drop(core);
    let store = RedbStore::open(&path).unwrap();
    assert!(store
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
    assert!(store.recover_attempts(intent_id).unwrap().is_empty());
}

#[test]
fn corrupt_attempt_evidence_keeps_parent_obligation_and_boot_fails_closed() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("corrupt-boot.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://corrupt-boot.example").unwrap();
    let event = signed(&keys, "corrupt boot", 108);
    let (intent_id, receipt_id) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(
            store,
            Box::new(directory(keys.public_key(), relay.clone())),
            10,
        );
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(Sink::default()),
        ));
        let receipt_id = receipt_id(&effects);
        drop(core);
        let store = RedbStore::open(&path).unwrap();
        (
            store
                .reattach_receipt(receipt_id.0)
                .unwrap()
                .intent_id
                .unwrap(),
            receipt_id,
        )
    };
    const ATTEMPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempts");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(ATTEMPTS).unwrap();
        let key = format!(
            "{:020}:{:020}:{}:{:020}",
            intent_id.0,
            relay.as_str().len(),
            relay.as_str(),
            1
        );
        let json = table
            .get(key.as_str())
            .unwrap()
            .unwrap()
            .value()
            .to_string();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["version"] = serde_json::json!(200);
        let encoded = serde_json::to_string(&value).unwrap();
        table.insert(key.as_str(), encoded.as_str()).unwrap();
    }
    tx.commit().unwrap();
    drop(db);

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    assert!(core.recover_on_boot().is_empty());
    assert!(!core.reattach_receipt(receipt_id, Box::new(Sink::default())));
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}
