//! Backend-independent semantic qualification for event and publishing state.
//!
//! The oracle deliberately observes only `EventStore` semantics. It never
//! reads physical tables, keys, row counts, or backend file bytes. A durable
//! harness additionally closes and reopens Redb after every successful
//! operation, proving that each checkpoint's complete normalized state and
//! digest survive recovery. Future backend candidates plug into the same
//! trace before they can be considered for production.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, SourceAuthority};
use nostr::{Event, EventBuilder, Filter, JsonUtil, Keys, Kind, RelayUrl, Tag, Timestamp};
use serde_json::{json, Value};

use crate::{
    coverage_key, sentinel_signature, AcceptOutcome, AcceptWrite, AttemptHandoffDetail,
    AttemptOutcome, ClaimSet, CoverageInterval, EventStore, HandoffEvidence, InsertOutcome,
    IntentId, IntentSigState, LaneKey, MemoryStore, PostHandoffState, RedbStore, RefuseReason,
    RelayObserved, StoredEvent, TransientCause, WriteDurability,
};

const ALICE_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const BOB_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000002";
const COVERAGE_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000003";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Checkpoint {
    operation: &'static str,
    digest: String,
    normalized: String,
}

#[derive(Default)]
struct OracleContext {
    receipt_ids: Vec<u64>,
    intent_ids: Vec<IntentId>,
    coverage: Vec<(ContextualAtom, RelayUrl)>,
}

#[derive(Clone)]
struct TraceFixture {
    duplicate: Event,
    old_replaceable: Event,
    new_replaceable: Event,
    old_addressable: Event,
    new_addressable: Event,
    delete_target: Event,
    delete_existing: Event,
    future_target: Event,
    delete_future: Event,
    expiring: Event,
    covered: Event,
    publish_signed: Event,
    publish_frozen: Event,
    cancel_frozen: Event,
}

impl TraceFixture {
    fn new(alice: &Keys, bob: &Keys, coverage_author: &Keys) -> Self {
        let delete_target = regular(alice, "delete me", 150);
        let future_target = regular(alice, "arrives after deletion", 170);
        let (publish_signed, publish_frozen) = signed_and_frozen(bob, "publish with retry", 220);
        let (_, cancel_frozen) = signed_and_frozen(bob, "cancel before signing", 230);
        Self {
            duplicate: regular(alice, "duplicate provenance", 100),
            old_replaceable: replaceable(alice, "old metadata", 110),
            new_replaceable: replaceable(alice, "new metadata", 120),
            old_addressable: addressable(alice, "oracle", "old address", 130),
            new_addressable: addressable(alice, "oracle", "new address", 140),
            delete_existing: deletion(alice, delete_target.id, 160),
            delete_target,
            delete_future: deletion(alice, future_target.id, 180),
            future_target,
            expiring: expiring(alice, "short lived", 190, 200),
            covered: regular(coverage_author, "covered then evicted", 210),
            publish_signed,
            publish_frozen,
            cancel_frozen,
        }
    }
}

enum Harness {
    Memory(Box<MemoryStore>),
    Redb {
        path: PathBuf,
        store: Option<RedbStore>,
    },
}

impl Harness {
    fn memory() -> Self {
        Self::Memory(Box::new(MemoryStore::new()))
    }

    fn redb(path: PathBuf) -> Self {
        let store = RedbStore::open(&path).expect("open oracle Redb store");
        Self::Redb {
            path,
            store: Some(store),
        }
    }

    fn store(&mut self) -> &mut dyn EventStore {
        match self {
            Self::Memory(store) => store.as_mut(),
            Self::Redb { store, .. } => store.as_mut().expect("Redb harness store is open"),
        }
    }

    fn checkpoint(
        &mut self,
        operation: &'static str,
        context: &OracleContext,
        alice: &Keys,
        primary_relay: &RelayUrl,
    ) -> Checkpoint {
        let before = normalized_state(self.store(), context, alice, primary_relay);

        if let Self::Redb { path, store } = self {
            let recovery_before = normalized_recovery_state(
                store.as_ref().expect("Redb harness store is open"),
                context,
            );
            drop(store.take());
            *store = Some(RedbStore::open(path).expect("reopen oracle Redb store"));
            let reopened = store.as_ref().expect("reopened Redb harness store");
            let after = normalized_state(reopened, context, alice, primary_relay);
            assert_eq!(
                after, before,
                "semantic state changed across reopen after {operation}"
            );
            assert_eq!(
                normalized_recovery_state(reopened, context),
                recovery_before,
                "recovery state changed across reopen after {operation}"
            );
        }

        Checkpoint {
            operation,
            digest: blake3::hash(before.as_bytes()).to_hex().to_string(),
            normalized: before,
        }
    }
}

fn keys(secret: &str) -> Keys {
    Keys::parse(secret).expect("fixed oracle key")
}

fn relay(url: &str) -> RelayUrl {
    RelayUrl::parse(url).expect("fixed oracle relay")
}

fn observed(relay: &RelayUrl, at: u64) -> RelayObserved {
    RelayObserved::new(relay.clone(), Timestamp::from(at))
}

fn regular(keys: &Keys, content: &str, created_at: u64) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign oracle event")
}

fn replaceable(keys: &Keys, content: &str, created_at: u64) -> Event {
    EventBuilder::new(Kind::Metadata, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign oracle replaceable event")
}

fn addressable(keys: &Keys, identifier: &str, content: &str, created_at: u64) -> Event {
    EventBuilder::new(Kind::from(30_003u16), content)
        .tag(Tag::identifier(identifier))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign oracle addressable event")
}

fn deletion(keys: &Keys, target: nostr::EventId, created_at: u64) -> Event {
    EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::event(target))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign oracle deletion")
}

fn expiring(keys: &Keys, content: &str, created_at: u64, expiration: u64) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .tag(Tag::expiration(Timestamp::from(expiration)))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("sign oracle expiring event")
}

fn signed_and_frozen(keys: &Keys, content: &str, created_at: u64) -> (Event, Event) {
    let signed = regular(keys, content, created_at);
    let frozen = Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    );
    (signed, frozen)
}

fn accept(frozen: Event, keys: &Keys, accepted_at: u64) -> AcceptWrite {
    AcceptWrite {
        frozen,
        replaceable_base: None,
        expected_pubkey: keys.public_key(),
        signing_identity_ref: "semantic-oracle-key".into(),
        durability: WriteDurability::Durable,
        routing: "semantic-oracle-route".into(),
        sig_state: IntentSigState::Pending,
        accepted_at: Timestamp::from(accepted_at),
        correlation: None,
    }
}

fn coverage_atom(keys: &Keys) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
            ids: None,
            tags: BTreeMap::new(),
            since: None,
            until: None,
            limit: None,
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    }
}

fn protect_author(keys: &Keys) -> ConcreteFilter {
    ConcreteFilter {
        kinds: None,
        authors: Some(BTreeSet::from([keys.public_key().to_hex()])),
        ids: None,
        tags: BTreeMap::new(),
        since: None,
        until: None,
        limit: None,
    }
}

fn canonical_row(row: &StoredEvent) -> Value {
    let seen = row
        .provenance
        .seen
        .iter()
        .map(|(relay, at)| json!([relay.as_str(), at.as_secs()]))
        .collect::<Vec<_>>();
    let local = row.provenance.local.as_ref().map(|local| {
        json!({
            "owners": local.owners.iter().map(|id| id.0).collect::<Vec<_>>(),
            "sig_state": format!("{:?}", local.sig_state),
        })
    });
    json!({
        "event_json": row.event.as_json(),
        "seen": seen,
        "local": local,
    })
}

fn canonical_rows(mut rows: Vec<StoredEvent>) -> Vec<Value> {
    rows.sort_by(|a, b| {
        b.event
            .created_at
            .cmp(&a.event.created_at)
            .then_with(|| a.event.id.cmp(&b.event.id))
    });
    rows.iter().map(canonical_row).collect()
}

fn ordered_ids(rows: Vec<StoredEvent>) -> Vec<String> {
    rows.into_iter().map(|row| row.event.id.to_hex()).collect()
}

fn normalized_state(
    store: &dyn EventStore,
    context: &OracleContext,
    alice: &Keys,
    primary_relay: &RelayUrl,
) -> String {
    let all_filter = Filter::new();
    let author_filter = Filter::new().author(alice.public_key());
    let text_filter = Filter::new()
        .kind(Kind::TextNote)
        .author(alice.public_key());
    let addressable_filter = Filter::new()
        .kind(Kind::from(30_003u16))
        .author(alice.public_key());

    let global = store
        .query_newest(&all_filter, 1_000)
        .expect("global query");
    let cursor_tail = global.first().map_or_else(Vec::new, |first| {
        store
            .query_newest_before(
                &all_filter,
                crate::EventCursor::from_event(&first.event),
                1_000,
            )
            .expect("cursor query")
    });
    let strict = store
        .query_newest_observed_by(&all_filter, &BTreeSet::from([primary_relay.clone()]), 1_000)
        .expect("strict provenance query");

    let coverage = context
        .coverage
        .iter()
        .map(|(atom, relay)| {
            let interval = store.get_coverage(coverage_key(atom), relay);
            json!({
                "key": blake3::hash(coverage_key(atom).as_bytes()).to_hex().to_string(),
                "relay": relay.as_str(),
                "interval": interval.map(|value| [value.from.as_secs(), value.through.as_secs()]),
            })
        })
        .collect::<Vec<_>>();

    let receipts = context
        .receipt_ids
        .iter()
        .map(|receipt_id| {
            let receipt = store.reattach_receipt(*receipt_id).expect("receipt lookup");
            json!({"receipt_id": receipt_id, "record": format!("{receipt:?}")})
        })
        .collect::<Vec<_>>();

    let delivery = context
        .intent_ids
        .iter()
        .map(|intent_id| {
            json!({
                "intent_id": intent_id.0,
                "routes": format!("{:?}", store.recover_route_revisions(*intent_id).expect("routes")),
                "attempts": format!("{:?}", store.recover_attempts(*intent_id).expect("attempts")),
                "details": format!("{:?}", store.recover_attempt_details(*intent_id).expect("attempt details")),
                "lanes": format!("{:?}", store.recover_outbox_lanes(*intent_id).expect("lanes")),
            })
        })
        .collect::<Vec<_>>();

    let state = json!({
        "events": canonical_rows(store.query(&all_filter).expect("all rows")),
        "ordered_queries": {
            "global": ordered_ids(global),
            "author": ordered_ids(store.query_newest(&author_filter, 1_000).expect("author query")),
            "text": ordered_ids(store.query_newest(&text_filter, 1_000).expect("text query")),
            "addressable": ordered_ids(store.query_newest(&addressable_filter, 1_000).expect("addressable query")),
            "strict_primary_relay": ordered_ids(strict),
            "cursor_tail": ordered_ids(cursor_tail),
        },
        "coverage": coverage,
        "receipts": receipts,
        "delivery": delivery,
        "deadlines": format!("{:?}", store.due_outbox_deadlines(Timestamp::from(u64::MAX), 1_000).expect("deadlines")),
        "next_expiration": store.next_expiration().map(|value| value.as_secs()),
    });
    serde_json::to_string(&state).expect("serialize normalized oracle state")
}

fn normalized_recovery_state(store: &dyn EventStore, context: &OracleContext) -> String {
    let intents = store
        .recover_outbox()
        .into_iter()
        .map(|intent| {
            json!({
                "intent_id": intent.intent_id.0,
                "receipt_id": intent.receipt_id,
                "frozen_json": intent.frozen.as_json(),
                "expected_pubkey": intent.expected_pubkey.to_hex(),
                "signing_identity_ref": intent.signing_identity_ref,
                "durability": format!("{:?}", intent.durability),
                "routing": intent.routing,
                "sig_state": format!("{:?}", intent.sig_state),
                "displaced": intent.displaced.as_ref().map(canonical_row),
                "accepted_at": intent.accepted_at.as_secs(),
            })
        })
        .collect::<Vec<_>>();
    let receipts = context
        .receipt_ids
        .iter()
        .map(|receipt_id| {
            format!(
                "{:?}",
                store
                    .reattach_receipt(*receipt_id)
                    .expect("recovery receipt lookup")
            )
        })
        .collect::<Vec<_>>();
    serde_json::to_string(&json!({"open_intents": intents, "receipts": receipts}))
        .expect("serialize recovery state")
}

/// Stable semantic digest used by every process-death failpoint. Individual
/// crash tests still assert the operation-specific allowed pre/post state;
/// this adds a backend-table-independent proof that the recovered state,
/// ordered query projection, and durable publishing journal survive a second
/// reopen byte-for-byte.
pub(crate) fn recovered_semantic_digest(store: &dyn EventStore) -> String {
    let rows = canonical_rows(store.query(&Filter::new()).expect("crash-oracle query"));
    let ordered = ordered_ids(
        store
            .query_newest(&Filter::new(), 10_000)
            .expect("crash-oracle ordered query"),
    );
    let intents = store
        .recover_outbox()
        .into_iter()
        .map(|intent| {
            json!({
                "intent_id": intent.intent_id.0,
                "receipt_id": intent.receipt_id,
                "frozen_json": intent.frozen.as_json(),
                "sig_state": format!("{:?}", intent.sig_state),
                "displaced": intent.displaced.as_ref().map(canonical_row),
                "receipt": format!("{:?}", store.reattach_receipt(intent.receipt_id).expect("crash-oracle receipt")),
                "routes": format!("{:?}", store.recover_route_revisions(intent.intent_id).expect("crash-oracle routes")),
                "attempts": format!("{:?}", store.recover_attempts(intent.intent_id).expect("crash-oracle attempts")),
                "details": format!("{:?}", store.recover_attempt_details(intent.intent_id).expect("crash-oracle details")),
                "lanes": format!("{:?}", store.recover_outbox_lanes(intent.intent_id).expect("crash-oracle lanes")),
            })
        })
        .collect::<Vec<_>>();
    let normalized = serde_json::to_string(&json!({
        "events": rows,
        "ordered": ordered,
        "open_intents": intents,
        "deadlines": format!("{:?}", store.due_outbox_deadlines(Timestamp::from(u64::MAX), 1_024).expect("crash-oracle deadlines")),
        "next_expiration": store.next_expiration().map(|value| value.as_secs()),
    }))
    .expect("serialize crash-oracle state");
    blake3::hash(normalized.as_bytes()).to_hex().to_string()
}

fn record(
    harness: &mut Harness,
    context: &OracleContext,
    checkpoints: &mut Vec<Checkpoint>,
    operation: &'static str,
    alice: &Keys,
    primary_relay: &RelayUrl,
) {
    checkpoints.push(harness.checkpoint(operation, context, alice, primary_relay));
}

fn run_trace(mut harness: Harness, fixture: &TraceFixture) -> Vec<Checkpoint> {
    let alice = keys(ALICE_SECRET);
    let bob = keys(BOB_SECRET);
    let coverage_author = keys(COVERAGE_SECRET);
    let primary = relay("wss://oracle-primary.example");
    let secondary = relay("wss://oracle-secondary.example");
    let publish = relay("wss://oracle-publish.example");
    let atom = coverage_atom(&coverage_author);
    let mut context = OracleContext {
        coverage: vec![(atom.clone(), primary.clone())],
        ..OracleContext::default()
    };
    let mut checkpoints = Vec::new();

    let duplicate = fixture.duplicate.clone();
    assert_eq!(
        harness
            .store()
            .insert(duplicate.clone(), observed(&primary, 101))
            .unwrap(),
        InsertOutcome::Inserted
    );
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "insert",
        &alice,
        &primary,
    );
    assert!(matches!(
        harness
            .store()
            .insert(duplicate, observed(&secondary, 102))
            .unwrap(),
        InsertOutcome::Duplicate {
            provenance_grew: true,
            ..
        }
    ));
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "duplicate provenance",
        &alice,
        &primary,
    );

    let old_replaceable = fixture.old_replaceable.clone();
    harness
        .store()
        .insert(old_replaceable, observed(&primary, 111))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "replaceable first winner",
        &alice,
        &primary,
    );
    let new_replaceable = fixture.new_replaceable.clone();
    assert!(matches!(
        harness
            .store()
            .insert(new_replaceable, observed(&primary, 121))
            .unwrap(),
        InsertOutcome::Superseded { .. }
    ));
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "replaceable conflict",
        &alice,
        &primary,
    );

    let old_addressable = fixture.old_addressable.clone();
    harness
        .store()
        .insert(old_addressable, observed(&primary, 131))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "addressable first winner",
        &alice,
        &primary,
    );
    let new_addressable = fixture.new_addressable.clone();
    assert!(matches!(
        harness
            .store()
            .insert(new_addressable, observed(&primary, 141))
            .unwrap(),
        InsertOutcome::Superseded { .. }
    ));
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "addressable conflict",
        &alice,
        &primary,
    );

    let delete_target = fixture.delete_target.clone();
    harness
        .store()
        .insert(delete_target, observed(&primary, 151))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "deletion target",
        &alice,
        &primary,
    );
    assert!(matches!(
        harness
            .store()
            .insert(fixture.delete_existing.clone(), observed(&primary, 161))
            .unwrap(),
        InsertOutcome::Kind5Processed { .. }
    ));
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "deletion",
        &alice,
        &primary,
    );

    let future_target = fixture.future_target.clone();
    harness
        .store()
        .insert(fixture.delete_future.clone(), observed(&primary, 181))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "deletion before target",
        &alice,
        &primary,
    );
    assert_eq!(
        harness
            .store()
            .insert(future_target, observed(&secondary, 182))
            .unwrap(),
        InsertOutcome::Refused(RefuseReason::Tombstoned)
    );
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "tombstoned target refused",
        &alice,
        &primary,
    );

    let expiring_event = fixture.expiring.clone();
    harness
        .store()
        .insert(expiring_event, observed(&primary, 191))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "expiry indexed",
        &alice,
        &primary,
    );
    assert_eq!(
        harness
            .store()
            .expire_due(Timestamp::from(200))
            .unwrap()
            .len(),
        1
    );
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "expiry applied",
        &alice,
        &primary,
    );

    let covered = fixture.covered.clone();
    harness
        .store()
        .insert(covered, observed(&primary, 211))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "coverage fact persisted",
        &alice,
        &primary,
    );
    harness
        .store()
        .record_coverage(
            &atom,
            &primary,
            CoverageInterval::new(Timestamp::from(0), Timestamp::from(300)),
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "coverage recorded after facts",
        &alice,
        &primary,
    );
    let report = harness
        .store()
        .gc(&ClaimSet::new(vec![protect_author(&alice)]))
        .unwrap();
    assert_eq!(report.events_evicted, 1);
    assert_eq!(report.coverage_rows_shrunk, 1);
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "coverage-safe gc",
        &alice,
        &primary,
    );

    let signed = fixture.publish_signed.clone();
    let frozen = fixture.publish_frozen.clone();
    let accepted = harness
        .store()
        .accept_write(accept(frozen, &bob, 221))
        .unwrap();
    let (publish_intent, publish_receipt) = match accepted {
        AcceptOutcome::Inserted {
            intent_id,
            receipt_id,
            ..
        } => (intent_id, receipt_id),
        other => panic!("expected inserted publish intent, got {other:?}"),
    };
    context.intent_ids.push(publish_intent);
    context.receipt_ids.push(publish_receipt);
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "pending write accepted",
        &alice,
        &primary,
    );

    harness
        .store()
        .promote_signed(publish_intent, signed.sig)
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "pending write signed",
        &alice,
        &primary,
    );

    let cancel_frozen = fixture.cancel_frozen.clone();
    let cancelled = harness
        .store()
        .accept_write(accept(cancel_frozen, &bob, 231))
        .unwrap();
    let cancel_intent = cancelled.journaled_intent_id().expect("cancel intent");
    let cancel_receipt = cancelled.journaled_receipt_id().expect("cancel receipt");
    context.intent_ids.push(cancel_intent);
    context.receipt_ids.push(cancel_receipt);
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "cancellable write accepted",
        &alice,
        &primary,
    );
    harness.store().cancel_write(cancel_intent).unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "write cancelled",
        &alice,
        &primary,
    );

    harness
        .store()
        .record_route_revision(publish_intent, BTreeSet::from([publish.clone()]))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication route durable",
        &alice,
        &primary,
    );
    harness
        .store()
        .bootstrap_outbox_lanes(publish_intent)
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication lanes bootstrapped",
        &alice,
        &primary,
    );

    let lane_key = LaneKey {
        intent_id: publish_intent,
        relay: publish,
    };
    let lane = harness
        .store()
        .set_lane_eligible(&lane_key, 1, Timestamp::from(240))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication lane eligible",
        &alice,
        &primary,
    );
    let (attempt, lane) = harness
        .store()
        .start_lane_attempt(
            &lane_key,
            lane.revision,
            signed.clone(),
            Timestamp::from(241),
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication attempt started",
        &alice,
        &primary,
    );
    harness
        .store()
        .record_lane_handoff(
            &lane_key,
            lane.revision,
            attempt.ordinal,
            AttemptHandoffDetail {
                at: Timestamp::from(242),
                result: HandoffEvidence::Ambiguous,
            },
            PostHandoffState::Transient {
                eligible_at: Timestamp::from(250),
                cause: TransientCause::ConnectionLost,
                raw_reason: Some("oracle retry".into()),
            },
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication retry scheduled",
        &alice,
        &primary,
    );

    let retry_lane = harness
        .store()
        .recover_outbox_lanes(publish_intent)
        .unwrap()
        .remove(0);
    let retry_lane = harness
        .store()
        .set_lane_eligible(&lane_key, retry_lane.revision, Timestamp::from(250))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication retry eligible",
        &alice,
        &primary,
    );
    let (retry, retry_lane) = harness
        .store()
        .start_lane_attempt(&lane_key, retry_lane.revision, signed, Timestamp::from(251))
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication retry started",
        &alice,
        &primary,
    );
    let awaiting_ack = harness
        .store()
        .record_lane_handoff(
            &lane_key,
            retry_lane.revision,
            retry.ordinal,
            AttemptHandoffDetail {
                at: Timestamp::from(252),
                result: HandoffEvidence::Written,
            },
            PostHandoffState::AwaitingAck {
                deadline: Timestamp::from(260),
            },
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication handed off",
        &alice,
        &primary,
    );

    harness
        .store()
        .finish_lane_attempt(
            &lane_key,
            awaiting_ack.revision,
            retry.ordinal,
            AttemptOutcome::Acked,
            Timestamp::from(253),
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication receipt acked",
        &alice,
        &primary,
    );
    harness
        .store()
        .close_terminal_intent(publish_intent)
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication obligation closed",
        &alice,
        &primary,
    );

    checkpoints
}

#[test]
fn full_semantic_trace_matches_memory_and_redb_after_every_operation_and_reopen() {
    let fixture = TraceFixture::new(
        &keys(ALICE_SECRET),
        &keys(BOB_SECRET),
        &keys(COVERAGE_SECRET),
    );
    let expected = run_trace(Harness::memory(), &fixture);
    let dir = tempfile::tempdir().expect("oracle tempdir");
    let actual = run_trace(
        Harness::redb(dir.path().join("semantic-oracle.redb")),
        &fixture,
    );

    assert_eq!(actual.len(), expected.len());
    for (expected, actual) in expected.iter().zip(&actual) {
        assert_eq!(actual.operation, expected.operation);
        assert_eq!(
            actual.normalized, expected.normalized,
            "normalized semantic mismatch after {}",
            actual.operation
        );
        assert_eq!(
            actual.digest, expected.digest,
            "digest mismatch after {}",
            actual.operation
        );
    }
}
