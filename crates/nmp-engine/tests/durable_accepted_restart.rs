//! Genuine redb close/reopen falsifiers for issues #2/#3 U4. No sleeps,
//! retry timers, or polling: restart is represented by dropping the whole
//! reducer/store and opening the database again.

use std::sync::{Arc, Mutex};

use nmp_engine::core::{Effect, EngineCore, EngineMsg, ReattachOutcome, ReceiptId};
use nmp_engine::outbox::{ReceiptSink, WriteStatus};
use nmp_grammar::{Durability, HostAuthority, WriteIntent, WritePayload, WriteRouting};
use nmp_router::FixtureDirectory;
use nmp_store::{
    sentinel_signature, AcceptWrite, AttemptOutcome, EventStore, IntentSigState, RedbStore,
    SigState, WriteDurability,
};
use nmp_transport::{RelayFrame, RelayHandle};
use nostr::{
    EventBuilder, JsonUtil, Keys, Kind, PublicKey, RelayMessage, RelayUrl, Timestamp, UnsignedEvent,
};
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

fn strip_additive_lane_rows(path: &std::path::Path, intent: nmp_store::IntentId, relay: &RelayUrl) {
    let db = Database::open(path).unwrap();
    let write = db.begin_write().unwrap();
    {
        let details: TableDefinition<&str, &str> = TableDefinition::new("outbox_attempt_details");
        let lanes: TableDefinition<&str, &str> = TableDefinition::new("outbox_lanes");
        let mut details = write.open_table(details).unwrap();
        let mut lanes = write.open_table(lanes).unwrap();
        let attempt_prefix = format!(
            "{:020}:{:020}:{}",
            intent.0,
            relay.as_str().len(),
            relay.as_str()
        );
        let canonical: &nostr::Url = relay.into();
        let canonical = canonical.as_str();
        let lane_key = format!("{:020}:{:020}:{canonical}", intent.0, canonical.len());
        assert!(details
            .remove(format!("{attempt_prefix}:{:020}", 1).as_str())
            .unwrap()
            .is_some());
        assert!(lanes.remove(lane_key.as_str()).unwrap().is_some());
    }
    write.commit().unwrap();
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
            Effect::PublishEvent(r, e, _) if r == &relay && e == &event
        )));
        receipt_id(&effects)
    };
    let intent = RedbStore::open(&path)
        .unwrap()
        .reattach_receipt(id.0)
        .unwrap()
        .unwrap()
        .intent_id
        .unwrap();
    strip_additive_lane_rows(&path, intent, &relay);

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
        Effect::PublishEvent(r, e, _) if r == &relay && e == &event
    )));
    assert!(
        recovery.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e, _) if r == &appended && e == &event
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
    assert!(core
        .reattach_receipt(id, Box::new(first.clone()))
        .is_attached());
    assert!(core
        .reattach_receipt(id, Box::new(second.clone()))
        .is_attached());
    assert_eq!(
        first.0.lock().unwrap().len(),
        second.0.lock().unwrap().len()
    );
    assert!(
        !first
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|s| matches!(s, WriteStatus::Sent(r) if r == &relay)),
        "a recovered Started attempt predates transport Written and cannot replay as Sent"
    );
    core.handle(EngineMsg::RelayConnected(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        relay.clone(),
    ));
    let acked = core.handle(EngineMsg::RelayFrame(
        RelayHandle {
            slot: 0,
            generation: 1,
        },
        RelayFrame::Text(RelayMessage::ok(event.id, true, "").as_json()),
    ));
    assert!(acked.iter().any(|effect| matches!(
        effect,
        Effect::EmitReceipt(receipt, WriteStatus::Acked(acked_relay))
            if *receipt == id && acked_relay == &relay
    )));
    drop(core);
    let store = RedbStore::open(&path).unwrap();
    let original_attempt = store
        .recover_attempts(intent)
        .unwrap()
        .into_iter()
        .find(|attempt| attempt.relay == relay)
        .unwrap();
    assert_eq!(original_attempt.outcome, AttemptOutcome::Acked);
    let original_lane = store
        .recover_outbox_lanes(intent)
        .unwrap()
        .into_iter()
        .find(|lane| lane.key.relay == relay)
        .unwrap();
    assert_eq!(
        original_lane.state,
        nmp_store::LaneState::Terminal {
            ordinal: 1,
            outcome: AttemptOutcome::Acked,
        }
    );
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
            .unwrap()
            .intent_id
            .unwrap()
    };
    strip_additive_lane_rows(&path, intent_id, &relay);

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
    let rows = store.query(&nostr::Filter::new().id(frozen_id)).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].provenance.local.as_ref().unwrap().sig_state,
        SigState::Pending
    );
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    assert!(core.recover_on_boot().is_empty());
    let reattached = Sink::default();
    assert!(core
        .reattach_receipt(id, Box::new(reattached.clone()))
        .is_attached());
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
    assert!(store
        .query(&nostr::Filter::new().id(frozen_id))
        .unwrap()
        .is_empty());
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
        .filter(
            |effect| matches!(effect, Effect::PublishEvent(_, replayed, _) if replayed == &event),
        )
        .count();
    assert_eq!(
        replays, 4,
        "two coowners retain both append-only relay lanes"
    );
    assert!(core
        .reattach_receipt(a, Box::new(Sink::default()))
        .is_attached());
    assert!(core
        .reattach_receipt(b, Box::new(Sink::default()))
        .is_attached());
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
        (intent_id, receipt_id)
    };

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    let effects = core.recover_on_boot();
    assert!(!effects
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    let unreadable = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(unreadable.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(
        unreadable.0.lock().unwrap().is_empty(),
        "unreadable routing must replay no receipt prefix"
    );

    let sign_request = core.handle(EngineMsg::SignerAttached(keys.public_key()));
    let generation = sign_request
        .iter()
        .find_map(|effect| match effect {
            Effect::RequestSign(id, generation, unsigned) if *id == receipt_id => {
                assert_eq!(unsigned.pubkey, keys.public_key());
                Some(*generation)
            }
            _ => None,
        })
        .expect("the retained unsigned obligation must remain signer-owned");
    let completed = core.handle(EngineMsg::SignerCompleted(
        receipt_id,
        generation,
        Ok(event),
    ));
    assert!(!completed
        .iter()
        .any(|effect| matches!(effect, Effect::PublishEvent(..))));
    assert!(
        unreadable.0.lock().unwrap().is_empty(),
        "an unreadable reattach must not register for later signer facts"
    );
    let second = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(second.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(second.0.lock().unwrap().is_empty());
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
    let unreadable_sink = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(unreadable_sink.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(unreadable_sink.0.lock().unwrap().is_empty());
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}

#[test]
fn retained_terminal_receipt_is_attached_and_replays_terminal_fact() {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://terminal.example").unwrap();
    let store = nmp_store::MemoryStore::new();
    let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
    core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
    let effects = core.handle(EngineMsg::Publish(
        WriteIntent {
            payload: WritePayload::Unsigned(UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(500),
                Kind::TextNote,
                vec![],
                "terminal retained",
            )),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
        },
        Box::new(Sink::default()),
    ));
    let receipt = receipt_id(&effects);
    core.handle(EngineMsg::CancelWrite(receipt));

    let replay = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt, Box::new(replay.clone())),
        ReattachOutcome::Attached
    );
    assert_eq!(
        *replay.0.lock().unwrap(),
        vec![WriteStatus::Failed("write compensated".to_string())]
    );
}

#[test]
fn corrupt_retained_receipt_is_not_misreported_absent_and_keeps_obligation() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("corrupt-receipt.redb");
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://corrupt-receipt.example").unwrap();
    let (intent_id, receipt_id) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, Box::new(directory(keys.public_key(), relay)), 10);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(501),
                    Kind::TextNote,
                    vec![],
                    "corrupt receipt",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
            },
            Box::new(Sink::default()),
        ));
        let receipt_id = receipt_id(&effects);
        drop(core);
        let store = RedbStore::open(&path).unwrap();
        let intent_id = store
            .reattach_receipt(receipt_id.0)
            .unwrap()
            .unwrap()
            .intent_id
            .unwrap();
        (intent_id, receipt_id)
    };

    const RECEIPTS: TableDefinition<&str, &str> = TableDefinition::new("outbox_receipts");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(RECEIPTS).unwrap();
        table
            .insert(format!("{:020}", receipt_id.0).as_str(), "{")
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    assert!(core.recover_on_boot().is_empty());
    let replay = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(replay.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(replay.0.lock().unwrap().is_empty());
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}

/// #115, cedar's flagged gap: the ruling text only said "resolve_routes
/// gains ONE arm," but a `PinnedHost`-routed pending write also has to
/// survive the reattach/restart path -- `routing_snapshot` (encode) is
/// wildcard-free so a missing arm there is a compile error, but
/// `parse_routing_snapshot` (decode) falls through to `None` on an
/// unrecognized prefix, which would silently mis-resolve a pinned-host
/// write on reboot without ever touching `resolve_routes` or a live relay
/// (invisible to `pinned_host_write.rs`'s falsifiers, which never cross a
/// restart boundary). This proves both snapshot arms actually exist and
/// round-trip: a `PinnedHost` write, restarted, still resolves to its
/// EXACT host, never any other relay, and never re-accepts.
#[test]
fn pinned_host_routing_round_trips_across_a_restart() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("pinned-host.redb");
    let keys = Keys::generate();
    let host = RelayUrl::parse("wss://pinned-host.example").unwrap();
    let event = signed(&keys, "pinned", 600);

    let id = {
        let store = RedbStore::open(&path).unwrap();
        // Deliberately empty: `PinnedHost` routing must never consult the
        // directory (#115) -- if it ever did, this publish would have no
        // route to fall back on and this test would never see
        // `Effect::PublishEvent` at all.
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Signed(event.clone()),
                durability: Durability::Durable,
                routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(host.clone())),
            },
            Box::new(Sink::default()),
        ));
        assert!(effects.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e, _) if r == &host && e == &event
        )));
        receipt_id(&effects)
    };

    // Restart: drop the whole reducer/store, reopen the SAME file.
    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    let recovery = core.recover_on_boot();
    assert!(
        recovery.iter().any(|effect| matches!(effect,
            Effect::PublishEvent(r, e, _) if r == &host && e == &event
        )),
        "a PinnedHost-routed pending write must still resolve to its exact host after a \
         restart -- parse_routing_snapshot must decode the persisted `pinned-host-hex:` \
         snapshot back into WriteRouting::PinnedHost, not just have routing_snapshot encode it"
    );
    assert!(
        !recovery
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(_, WriteStatus::Accepted))),
        "boot recovery must not accept the write a second time"
    );

    let replay = Sink::default();
    assert!(core
        .reattach_receipt(id, Box::new(replay.clone()))
        .is_attached());
}

#[test]
fn corrupt_route_lane_evidence_is_unreadable_not_absent() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("corrupt-route.redb");
    let keys = Keys::generate();
    let event = signed(&keys, "corrupt route", 502);
    let (intent_id, receipt_id) = {
        let store = RedbStore::open(&path).unwrap();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
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
        let intent_id = store
            .reattach_receipt(receipt_id.0)
            .unwrap()
            .unwrap()
            .intent_id
            .unwrap();
        (intent_id, receipt_id)
    };

    const ROUTES: TableDefinition<&str, &str> = TableDefinition::new("outbox_route_revisions");
    let db = Database::open(&path).unwrap();
    let tx = db.begin_write().unwrap();
    {
        let mut table = tx.open_table(ROUTES).unwrap();
        table
            .insert(format!("{:020}:{:020}", intent_id.0, 1).as_str(), "{}")
            .unwrap();
    }
    tx.commit().unwrap();
    drop(db);

    let store = RedbStore::open(&path).unwrap();
    let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
    assert!(core.recover_on_boot().is_empty());
    let replay = Sink::default();
    assert_eq!(
        core.reattach_receipt(receipt_id, Box::new(replay.clone())),
        ReattachOutcome::RetainedButUnreadable
    );
    assert!(replay.0.lock().unwrap().is_empty());
    drop(core);
    assert!(RedbStore::open(&path)
        .unwrap()
        .recover_outbox()
        .iter()
        .any(|intent| intent.intent_id == intent_id));
}
