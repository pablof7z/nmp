//! Redb semantic and reconstruction qualification for event and publishing
//! state.
//!
//! The oracle deliberately observes only the typed semantics of a real
//! `RedbStore`. It never
//! reads physical tables, keys, row counts, or backend file bytes. The harness
//! closes and reopens Redb after every successful operation,
//! proving that each checkpoint's complete normalized state and digest
//! survive recovery without relying on another store implementation as an
//! oracle.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use nmp_grammar::{ConcreteFilter, ContextualAtom, ReadRouting};
use nostr::secp256k1::rand::{rngs::StdRng, SeedableRng};
use nostr::secp256k1::SECP256K1;
use nostr::{Event, EventBuilder, Filter, JsonUtil, Keys, Kind, RelayUrl, Tag, Timestamp};
use serde_json::{json, Value};

use crate::{
    coverage_key, sentinel_signature, AcceptOutcome, AcceptWrite, CoverageInterval, GcRetentionSet,
    HandoffEvidence, InsertOutcome, IntentId, IntentSigState, PublishQueueAttemptHandoff,
    PublishQueueAttemptOutcome, PublishQueueLaneKey, PublishQueuePostHandoffState,
    PublishQueueTransientCause, RedbStore, RefuseReason, RelayObserved, StoredEvent,
    VerifiedSignature,
};

/// The verified, intent-bound evidence `promote_signed` takes (#768). Every
/// event promoted below is one this fixture just signed itself, so the
/// verification succeeding is part of the setup, not the property under test.
fn evidence(signed: &Event) -> VerifiedSignature {
    VerifiedSignature::verify(signed).expect("fixture events are validly signed")
}

const ALICE_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000001";
const BOB_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000002";
const COVERAGE_SECRET: &str = "0000000000000000000000000000000000000000000000000000000000000003";
const MAX_COVERAGE_SECRET: &str =
    "0000000000000000000000000000000000000000000000000000000000000004";

#[derive(Debug, Clone, PartialEq, Eq)]
struct Checkpoint {
    operation: &'static str,
    digest: String,
}

const EXPECTED_SEMANTIC_TRACE: &[(&str, &str)] = &[
    (
        "insert",
        "438cd8b97ec0e2be28042aa5ace9bd7fc75bd8a583e3f0c9787b893132608fe2",
    ),
    (
        "duplicate provenance",
        "3e7ec1339084ef345d59b35677fb3fe242525892540fd209e14523b1d533a13c",
    ),
    (
        "replaceable first winner",
        "a68348d2f5d27925b89105f0151b5145cb4af95405b09af3e8fc2ac634b0a4c0",
    ),
    (
        "replaceable conflict",
        "1df9d7308e5241259493eb3256c27555af8f108bab4baf841872cd9b4da73e82",
    ),
    (
        "addressable first winner",
        "d85cf593a1c4e8489b78ced897c9c33a08daf26495d620c8f43a5ec6cee52aa2",
    ),
    (
        "addressable conflict",
        "eca8a0d61721f735e4d4e9e7045c212c3a966abc4fe4b4e633801caf882d725f",
    ),
    (
        "deletion target",
        "8e9a22dedef5dcf718ddbe4d0bc5c80fe43ef26f566992e9836852f05caa2d7d",
    ),
    (
        "deletion",
        "3bae52aa6e311be89264014232bc6b823ac8fa830f41d45bbf5a48b0bde35e37",
    ),
    (
        "deletion before target",
        "f802b018266aa1490f9e365b5dfad5a3675a7b361ca7aa421444d70c8fc290fc",
    ),
    (
        "tombstoned target refused",
        "f802b018266aa1490f9e365b5dfad5a3675a7b361ca7aa421444d70c8fc290fc",
    ),
    (
        "expiry indexed",
        "7168d16aa531fcb41e3233915c78fdf9edd4b8510542fdccaffd4e4956424b48",
    ),
    (
        "expiry applied",
        "f802b018266aa1490f9e365b5dfad5a3675a7b361ca7aa421444d70c8fc290fc",
    ),
    (
        "coverage fact persisted",
        "582aad6704ef47e5604c53e00f75080c3512faa51f8497bacd22285c3857f7e5",
    ),
    (
        "coverage recorded after facts",
        "67c3444fc94484ab6e1a88ed79c4f21032fd8dbe4a93a1f636549dd2b4107952",
    ),
    (
        "coverage-safe gc",
        "2a253ec599c7a2159d0477855a75974ed29fbe040c4f4d3be540220c5051b8b2",
    ),
    (
        "maximum coverage fact and claim persisted",
        "d2303426800c988d08fe682b24ca948766d9723be54dd294089d0b890877d25b",
    ),
    (
        "maximum coverage removed with fact",
        "2a253ec599c7a2159d0477855a75974ed29fbe040c4f4d3be540220c5051b8b2",
    ),
    (
        "pending write accepted",
        "89d7ad04daf5d3371b54da60839cec7bdf4d295f48451acd9de2839a20bd2965",
    ),
    (
        "pending write signed",
        "7796710791a29258fec0f7c85fdc08eaa12e3dbada5724a37767a40a8a1001e0",
    ),
    (
        "cancellable write accepted",
        "0d94ee7c4827c3f1d5641032294e82ee5cf4279caa647efe1879a8d801d2c200",
    ),
    (
        "write cancelled",
        "2d36c0fdced730f1c5bc8c6600ec74ecd39368aed48135d3881905b8e0e1f3e0",
    ),
    (
        "replaceable delivery accepted",
        "193c10569dd7bb80a2aa58289fb80cb18bebfad50893219a8d80ae559fba5210",
    ),
    (
        "replaceable delivery superseded",
        "d058ec28f482f0bf276ff2cea24b459901535a2bddd8a6a173e80fa4fd02f776",
    ),
    (
        "superseding delivery compensated",
        "a9dd593aed8abfcb3d4682046732ae4bc548604b1d36ae24ab729fa7d8146447",
    ),
    (
        "multi-relay delivery accepted",
        "e4f65711422f464d12a58cc1ad31c366658ef75ee64849366528b6bf4d5d11be",
    ),
    (
        "multi-relay lanes durable",
        "5c2e495734ac08203cd09e06254b8b3e7d6ee46acb650e4c9b1ab5d7c66fcea8",
    ),
    (
        "ambiguous handoff became outcome unknown",
        "8a9c9dbc07d719764f053a6e8ab63de430d8e5311989c9b80625fe799adfbe67",
    ),
    (
        "delivery attempt gave up",
        "1587bc3a213e7b313afb99ea8f65c67d88f2f5784a7e55fd042d761a7b7f767e",
    ),
    (
        "in-flight delivery interrupted",
        "9cbeb8678476e4bfdf3d2b738848c7ed422682ccb42260c9590b282d52416f1d",
    ),
    (
        "retry ended in relay rejection",
        "05e93aae6eac089b08e742a9f8d93687f7635f741ed20dd8a74bfd8084753523",
    ),
    (
        "multi-relay terminal obligation closed",
        "05e93aae6eac089b08e742a9f8d93687f7635f741ed20dd8a74bfd8084753523",
    ),
    (
        "publication route durable",
        "4a417b275884f33462148d18b0b60888a225b71fd3c31ce418896bd198eecadb",
    ),
    (
        "publication lanes bootstrapped",
        "b2fc2d23ab28f651fc3fdcfd1c23bbf78956d8146769c43b64fce956845f9d7b",
    ),
    (
        "publication lane eligible",
        "3ab760f8ec669a0d6c62287e909151040da97f3dc2dfac5e8913292079ba2272",
    ),
    (
        "publication attempt started",
        "83533bff9f96cc04625678dd0b01c566be1b0a2787dea289d8791c7d838143d5",
    ),
    (
        "publication retry scheduled",
        "1d49dae0a31b25373882d9a7e0478a2e3ca0db41229174f009da1d67c6d78a0c",
    ),
    (
        "publication retry eligible",
        "73d21ec206155ac4e17dd821ebe8a62807536aa77ee0d25fa205130d3616f534",
    ),
    (
        "publication retry started",
        "df903b8d6211415b286f6f33d3bdca27f67a0550e9fccfa62be777cf9020c007",
    ),
    (
        "publication handed off",
        "484053ad90512913ba8b75d1a27e6af926bf1502d67f030f7f851a206d0cd998",
    ),
    (
        "publication receipt acked",
        "e581d3ba1ae85d141e9e52fc154d5a4f8ef05b1fa2d5b512d79aa4975572bf39",
    ),
    (
        "publication obligation closed",
        "e581d3ba1ae85d141e9e52fc154d5a4f8ef05b1fa2d5b512d79aa4975572bf39",
    ),
];

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
    max_covered: Event,
    publish_signed: Event,
    publish_frozen: Event,
    cancel_frozen: Event,
    edge_signed: Event,
    edge_frozen: Event,
    superseded_old_frozen: Event,
    superseding_new_frozen: Event,
}

impl TraceFixture {
    fn new(alice: &Keys, bob: &Keys, coverage_author: &Keys, max_coverage_author: &Keys) -> Self {
        let delete_target = regular(alice, "delete me", 150);
        let future_target = regular(alice, "arrives after deletion", 170);
        let (publish_signed, publish_frozen) = signed_and_frozen(bob, "publish with retry", 220);
        let (_, cancel_frozen) = signed_and_frozen(bob, "cancel before signing", 230);
        let (edge_signed, edge_frozen) =
            signed_and_frozen(bob, "terminal delivery edge cases", 270);
        let superseded_old_frozen =
            frozen_from(replaceable(bob, "superseded pending metadata", 280));
        let superseding_new_frozen =
            frozen_from(replaceable(bob, "superseding pending metadata", 281));
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
            max_covered: regular(max_coverage_author, "maximum timestamp boundary", u64::MAX),
            publish_signed,
            publish_frozen,
            cancel_frozen,
            edge_signed,
            edge_frozen,
            superseded_old_frozen,
            superseding_new_frozen,
        }
    }
}

struct Harness {
    path: PathBuf,
    store: Option<RedbStore>,
}

impl Harness {
    fn redb(path: PathBuf) -> Self {
        let store = RedbStore::open(&path).expect("open oracle Redb store");
        Self {
            path,
            store: Some(store),
        }
    }

    fn store(&mut self) -> &mut RedbStore {
        self.store.as_mut().expect("Redb harness store is open")
    }

    fn checkpoint(
        &mut self,
        operation: &'static str,
        context: &OracleContext,
        alice: &Keys,
        primary_relay: &RelayUrl,
    ) -> Checkpoint {
        let before = normalized_state(self.store(), context, alice, primary_relay);

        let recovery_before = normalized_recovery_state(
            self.store.as_ref().expect("Redb harness store is open"),
            context,
        );
        drop(self.store.take());
        self.store = Some(RedbStore::open(&self.path).expect("reopen oracle Redb store"));
        let reopened = self.store.as_ref().expect("reopened Redb harness store");
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

        Checkpoint {
            operation,
            digest: blake3::hash(before.as_bytes()).to_hex().to_string(),
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

fn sign(builder: EventBuilder, keys: &Keys) -> Event {
    let mut rng = StdRng::seed_from_u64(0x1427);
    builder
        .build(keys.public_key())
        .sign_with_ctx(SECP256K1, &mut rng, keys)
        .expect("sign deterministic oracle event")
}

fn regular(keys: &Keys, content: &str, created_at: u64) -> Event {
    sign(
        EventBuilder::new(Kind::TextNote, content).custom_created_at(Timestamp::from(created_at)),
        keys,
    )
}

fn replaceable(keys: &Keys, content: &str, created_at: u64) -> Event {
    sign(
        EventBuilder::new(Kind::Metadata, content).custom_created_at(Timestamp::from(created_at)),
        keys,
    )
}

fn addressable(keys: &Keys, identifier: &str, content: &str, created_at: u64) -> Event {
    sign(
        EventBuilder::new(Kind::from(30_003u16), content)
            .tag(Tag::identifier(identifier))
            .custom_created_at(Timestamp::from(created_at)),
        keys,
    )
}

fn deletion(keys: &Keys, target: nostr::EventId, created_at: u64) -> Event {
    sign(
        EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target))
            .custom_created_at(Timestamp::from(created_at)),
        keys,
    )
}

fn expiring(keys: &Keys, content: &str, created_at: u64, expiration: u64) -> Event {
    sign(
        EventBuilder::new(Kind::TextNote, content)
            .tag(Tag::expiration(Timestamp::from(expiration)))
            .custom_created_at(Timestamp::from(created_at)),
        keys,
    )
}

fn signed_and_frozen(keys: &Keys, content: &str, created_at: u64) -> (Event, Event) {
    let signed = regular(keys, content, created_at);
    let frozen = frozen_from(signed.clone());
    (signed, frozen)
}

fn frozen_from(signed: Event) -> Event {
    Event::new(
        signed.id,
        signed.pubkey,
        signed.created_at,
        signed.kind,
        signed.tags.clone(),
        signed.content.clone(),
        sentinel_signature(),
    )
}

fn accept(frozen: Event, keys: &Keys, accepted_at: u64) -> AcceptWrite {
    AcceptWrite {
        payload: crate::AcceptWritePayload::Event {
            frozen: Box::new(frozen),
            routing: "semantic-oracle-route".into(),
            sig_state: IntentSigState::Pending,
        },
        expected_pubkey: keys.public_key(),
        signing_identity_ref: "semantic-oracle-key".into(),
        accepted_at: Timestamp::from(accepted_at),
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
        routing: ReadRouting::Auto,
        authenticated_as: None,
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
    store: &RedbStore,
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
        .query_newest_under_pin(&all_filter, &BTreeSet::from([primary_relay.clone()]), 1_000)
        .expect("strict provenance query");

    let coverage = context
        .coverage
        .iter()
        .map(|(atom, relay)| {
            let interval = store
                .get_coverage(coverage_key(atom), relay)
                .expect("oracle coverage read");
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
                "lanes": format!("{:?}", store.recover_publish_queue_lanes(*intent_id).expect("lanes")),
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
        "deadlines": format!("{:?}", store.due_publish_queue_deadlines(Timestamp::from(u64::MAX), 1_000).expect("deadlines")),
        "next_expiration": store
            .next_expiration()
            .expect("oracle expiration peek")
            .map(|value| value.as_secs()),
    });
    serde_json::to_string(&state).expect("serialize normalized oracle state")
}

fn normalized_recovery_state(store: &RedbStore, context: &OracleContext) -> String {
    let intents = store
        .recover_publish_queue()
        .expect("crash-oracle recover_publish_queue")
        .into_iter()
        .map(|intent| {
            json!({
                "intent_id": intent.intent_id.0,
                "receipt_id": intent.receipt_id,
                "work": format!("{:?}", intent.work),
                "expected_pubkey": intent.expected_pubkey.to_hex(),
                "signing_identity_ref": intent.signing_identity_ref,
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
    serde_json::to_string(&json!({
        "open_intents": intents,
        "receipts": receipts,
    }))
    .expect("serialize recovery state")
}

/// Stable semantic digest used by every process-death failpoint. Individual
/// crash tests still assert the operation-specific allowed pre/post state;
/// this adds a backend-table-independent proof that the recovered state,
/// ordered query projection, and durable publishing journal survive a second
/// reopen byte-for-byte.
pub(crate) fn recovered_semantic_digest(store: &RedbStore) -> String {
    let rows = canonical_rows(store.query(&Filter::new()).expect("crash-oracle query"));
    let ordered = ordered_ids(
        store
            .query_newest(&Filter::new(), 10_000)
            .expect("crash-oracle ordered query"),
    );
    let intents = store
        .recover_publish_queue()
        .expect("crash-oracle recover_publish_queue")
        .into_iter()
        .map(|intent| {
            json!({
                "intent_id": intent.intent_id.0,
                "receipt_id": intent.receipt_id,
                "work": format!("{:?}", intent.work),
                "receipt": format!("{:?}", store.reattach_receipt(intent.receipt_id).expect("crash-oracle receipt")),
                "routes": format!("{:?}", store.recover_route_revisions(intent.intent_id).expect("crash-oracle routes")),
                "attempts": format!("{:?}", store.recover_attempts(intent.intent_id).expect("crash-oracle attempts")),
                "details": format!("{:?}", store.recover_attempt_details(intent.intent_id).expect("crash-oracle details")),
                "lanes": format!("{:?}", store.recover_publish_queue_lanes(intent.intent_id).expect("crash-oracle lanes")),
            })
        })
        .collect::<Vec<_>>();
    let normalized = serde_json::to_string(&json!({
        "events": rows,
        "ordered": ordered,
        "open_intents": intents,
        "deadlines": format!("{:?}", store.due_publish_queue_deadlines(Timestamp::from(u64::MAX), 1_024).expect("crash-oracle deadlines")),
        "next_expiration": store
            .next_expiration()
            .expect("crash-oracle expiration peek")
            .map(|value| value.as_secs()),
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
    let max_coverage_author = keys(MAX_COVERAGE_SECRET);
    let primary = relay("wss://oracle-primary.example");
    let secondary = relay("wss://oracle-secondary.example");
    let publish = relay("wss://oracle-publish.example");
    let unknown_relay = relay("wss://oracle-unknown.example");
    let gave_up_relay = relay("wss://oracle-gave-up.example");
    let interrupted_relay = relay("wss://oracle-interrupted.example");
    let atom = coverage_atom(&coverage_author);
    let max_atom = coverage_atom(&max_coverage_author);
    let mut context = OracleContext {
        coverage: vec![
            (atom.clone(), primary.clone()),
            (atom.clone(), secondary.clone()),
            (max_atom.clone(), primary.clone()),
        ],
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
        .record_coverage(&[
            (
                atom.clone(),
                primary.clone(),
                CoverageInterval::new(Timestamp::from(0), Timestamp::from(300)),
            ),
            (
                atom.clone(),
                secondary.clone(),
                CoverageInterval::new(Timestamp::from(0), Timestamp::from(300)),
            ),
        ])
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
        .gc(&GcRetentionSet::new(vec![protect_author(&alice)]))
        .unwrap();
    assert_eq!(report.events_evicted, 1);
    assert_eq!(report.coverage_rows_shrunk, 2);
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "coverage-safe gc",
        &alice,
        &primary,
    );

    harness
        .store()
        .insert(fixture.max_covered.clone(), observed(&primary, u64::MAX))
        .unwrap();
    harness
        .store()
        .record_coverage(&[(
            max_atom.clone(),
            primary.clone(),
            CoverageInterval::new(Timestamp::from(u64::MAX), Timestamp::from(u64::MAX)),
        )])
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "maximum coverage fact and claim persisted",
        &alice,
        &primary,
    );
    let report = harness
        .store()
        .gc(&GcRetentionSet::new(vec![protect_author(&alice)]))
        .unwrap();
    assert_eq!(report.events_evicted, 1);
    assert_eq!(report.coverage_rows_deleted, 1);
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "maximum coverage removed with fact",
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
        .promote_signed(
            crate::PromotionTarget::Event(publish_intent),
            evidence(&signed),
        )
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

    let superseded = harness
        .store()
        .accept_write(accept(fixture.superseded_old_frozen.clone(), &bob, 280))
        .unwrap();
    let superseded_intent = superseded
        .journaled_intent_id()
        .expect("superseded candidate intent");
    context.intent_ids.push(superseded_intent);
    context.receipt_ids.push(
        superseded
            .journaled_receipt_id()
            .expect("superseded receipt"),
    );
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "replaceable delivery accepted",
        &alice,
        &primary,
    );

    let superseding = harness
        .store()
        .accept_write(accept(fixture.superseding_new_frozen.clone(), &bob, 281))
        .unwrap();
    let superseding_intent = superseding
        .journaled_intent_id()
        .expect("superseding intent");
    match &superseding {
        AcceptOutcome::Superseded { retired, .. } => {
            assert!(
                retired
                    .iter()
                    .any(|retired| retired.intent_id == superseded_intent),
                "newer unattempted replacement retires the older delivery obligation"
            );
        }
        other => panic!("expected superseding acceptance, got {other:?}"),
    }
    context.intent_ids.push(superseding_intent);
    context.receipt_ids.push(
        superseding
            .journaled_receipt_id()
            .expect("superseding receipt"),
    );
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "replaceable delivery superseded",
        &alice,
        &primary,
    );
    assert!(matches!(
        harness
            .store()
            .compensate_write(superseding_intent)
            .unwrap(),
        crate::CompensateOutcome::Compensated { .. }
    ));
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "superseding delivery compensated",
        &alice,
        &primary,
    );

    let edge = harness
        .store()
        .accept_write(accept(fixture.edge_frozen.clone(), &bob, 290))
        .unwrap();
    let edge_intent = edge.journaled_intent_id().expect("edge intent");
    let edge_receipt = edge.journaled_receipt_id().expect("edge receipt");
    context.intent_ids.push(edge_intent);
    context.receipt_ids.push(edge_receipt);
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "multi-relay delivery accepted",
        &alice,
        &primary,
    );
    harness
        .store()
        .promote_signed(
            crate::PromotionTarget::Event(edge_intent),
            evidence(&fixture.edge_signed),
        )
        .unwrap();
    harness
        .store()
        .record_route_revision(
            edge_intent,
            BTreeSet::from([
                unknown_relay.clone(),
                gave_up_relay.clone(),
                interrupted_relay.clone(),
            ]),
        )
        .unwrap();
    harness
        .store()
        .bootstrap_publish_queue_lanes(edge_intent)
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "multi-relay lanes durable",
        &alice,
        &primary,
    );

    let edge_lanes = harness
        .store()
        .recover_publish_queue_lanes(edge_intent)
        .unwrap();
    let unknown_key = PublishQueueLaneKey {
        intent_id: edge_intent,
        event_id: fixture.edge_signed.id,
        relay: unknown_relay,
    };
    let unknown_lane = edge_lanes
        .iter()
        .find(|lane| lane.key == unknown_key)
        .expect("unknown lane");
    let unknown_lane = harness
        .store()
        .set_lane_eligible(&unknown_key, unknown_lane.revision, Timestamp::from(291))
        .unwrap();
    let (unknown_attempt, unknown_lane) = harness
        .store()
        .start_lane_attempt(
            &unknown_key,
            unknown_lane.revision,
            fixture.edge_signed.clone(),
            Timestamp::from(292),
        )
        .unwrap();
    harness
        .store()
        .record_lane_handoff(
            &unknown_key,
            unknown_lane.revision,
            unknown_attempt.ordinal,
            PublishQueueAttemptHandoff {
                at: Timestamp::from(293),
                result: HandoffEvidence::Ambiguous,
            },
            PublishQueuePostHandoffState::Terminal {
                outcome: PublishQueueAttemptOutcome::GaveUp,
                finished_at: Timestamp::from(293),
            },
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "ambiguous handoff became outcome unknown",
        &alice,
        &primary,
    );

    let gave_up_key = PublishQueueLaneKey {
        intent_id: edge_intent,
        event_id: fixture.edge_signed.id,
        relay: gave_up_relay,
    };
    let gave_up_lane = edge_lanes
        .iter()
        .find(|lane| lane.key == gave_up_key)
        .expect("gave-up lane");
    let gave_up_lane = harness
        .store()
        .set_lane_eligible(&gave_up_key, gave_up_lane.revision, Timestamp::from(294))
        .unwrap();
    let (gave_up_attempt, gave_up_lane) = harness
        .store()
        .start_lane_attempt(
            &gave_up_key,
            gave_up_lane.revision,
            fixture.edge_signed.clone(),
            Timestamp::from(295),
        )
        .unwrap();
    harness
        .store()
        .finish_lane_attempt(
            &gave_up_key,
            gave_up_lane.revision,
            gave_up_attempt.ordinal,
            PublishQueueAttemptOutcome::GaveUp,
            Timestamp::from(296),
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "delivery attempt gave up",
        &alice,
        &primary,
    );

    let interrupted_key = PublishQueueLaneKey {
        intent_id: edge_intent,
        event_id: fixture.edge_signed.id,
        relay: interrupted_relay,
    };
    let interrupted_lane = edge_lanes
        .iter()
        .find(|lane| lane.key == interrupted_key)
        .expect("interrupted lane");
    let interrupted_lane = harness
        .store()
        .set_lane_eligible(
            &interrupted_key,
            interrupted_lane.revision,
            Timestamp::from(297),
        )
        .unwrap();
    let (interrupted_attempt, interrupted_lane) = harness
        .store()
        .start_lane_attempt(
            &interrupted_key,
            interrupted_lane.revision,
            fixture.edge_signed.clone(),
            Timestamp::from(298),
        )
        .unwrap();
    let interrupted_lane = harness
        .store()
        .suspend_lane_attempt(
            &interrupted_key,
            interrupted_lane.revision,
            interrupted_attempt.ordinal,
            Timestamp::from(299),
            PublishQueueTransientCause::Interrupted,
            Some("oracle process interruption".into()),
            false,
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "in-flight delivery interrupted",
        &alice,
        &primary,
    );
    let interrupted_lane = harness
        .store()
        .set_lane_eligible(
            &interrupted_key,
            interrupted_lane.revision,
            Timestamp::from(300),
        )
        .unwrap();
    let (rejection_attempt, rejection_lane) = harness
        .store()
        .start_lane_attempt(
            &interrupted_key,
            interrupted_lane.revision,
            fixture.edge_signed.clone(),
            Timestamp::from(301),
        )
        .unwrap();
    harness
        .store()
        .finish_lane_attempt(
            &interrupted_key,
            rejection_lane.revision,
            rejection_attempt.ordinal,
            PublishQueueAttemptOutcome::Rejected("oracle relay rejected".into()),
            Timestamp::from(302),
        )
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "retry ended in relay rejection",
        &alice,
        &primary,
    );
    harness.store().close_terminal_intent(edge_intent).unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "multi-relay terminal obligation closed",
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
        .bootstrap_publish_queue_lanes(publish_intent)
        .unwrap();
    record(
        &mut harness,
        &context,
        &mut checkpoints,
        "publication lanes bootstrapped",
        &alice,
        &primary,
    );

    let lane_key = PublishQueueLaneKey {
        intent_id: publish_intent,
        event_id: fixture.publish_signed.id,
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
            PublishQueueAttemptHandoff {
                at: Timestamp::from(242),
                result: HandoffEvidence::Ambiguous,
            },
            PublishQueuePostHandoffState::Transient {
                eligible_at: Timestamp::from(250),
                cause: PublishQueueTransientCause::ConnectionLost,
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
        .recover_publish_queue_lanes(publish_intent)
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
            PublishQueueAttemptHandoff {
                at: Timestamp::from(252),
                result: HandoffEvidence::Written,
            },
            PublishQueuePostHandoffState::AwaitingAck {
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
            PublishQueueAttemptOutcome::Acked,
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
fn full_semantic_trace_survives_redb_reopen_after_every_operation() {
    let fixture = TraceFixture::new(
        &keys(ALICE_SECRET),
        &keys(BOB_SECRET),
        &keys(COVERAGE_SECRET),
        &keys(MAX_COVERAGE_SECRET),
    );
    let dir = tempfile::tempdir().expect("oracle tempdir");
    let checkpoints = run_trace(
        Harness::redb(dir.path().join("semantic-oracle.redb")),
        &fixture,
    );
    let actual = checkpoints
        .iter()
        .map(|checkpoint| (checkpoint.operation, checkpoint.digest.as_str()))
        .collect::<Vec<_>>();
    assert_eq!(actual, EXPECTED_SEMANTIC_TRACE);
}
