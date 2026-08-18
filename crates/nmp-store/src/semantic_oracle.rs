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

use nmp_grammar::RelaySessionKey;
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
    ("insert", "8fe95e770b60bb6194bedaf20bbb4a3a5f1302b743cfdedb7f4756637f4a22d7"),
    ("duplicate provenance", "05f979fddbdfed4bc3b15571ae87aa60149f3bb0ce9549050115b99abf9b6605"),
    ("replaceable first winner", "60b6f2c9d17aa781c66fd6ff310ab0ec2057cba3c3dd47db0dc1631325c0c33b"),
    ("replaceable conflict", "f0edf738fc7133e7b61434e00a0a3502ff7e08a60cdf73409c119c204693ba39"),
    ("addressable first winner", "94d64f5f71fa7286a395352d23f9f8f169d800d16727e588467a6616ba574553"),
    ("addressable conflict", "9942951fe3d86bb8a2b0695839b01936cf63492474d528995623d1ac34334adc"),
    ("deletion target", "b56f93d14749f5bccf9f018064f50b6b9d020925f16ed9a66aea003a9bd596aa"),
    ("deletion", "123ba6144f4edba6aa2544437d61002bb3e4f01af562d2a496e0c4ac072eeb90"),
    ("deletion before target", "eba439b21190a60e9f2822ed79849d9c43afa91e6c65a4395daa5da73ce57190"),
    ("tombstoned target refused", "eba439b21190a60e9f2822ed79849d9c43afa91e6c65a4395daa5da73ce57190"),
    ("expiry indexed", "c911549251ee12ab980566ccd4e4020f91c0a9f345366afeef86058dccb21ec1"),
    ("expiry applied", "eba439b21190a60e9f2822ed79849d9c43afa91e6c65a4395daa5da73ce57190"),
    ("coverage fact persisted", "40c268a6118a60ce67b7eb07e0c6c1db7b126069470a6e68935e1d73cdcf8737"),
    ("coverage recorded after facts", "4958a2e7b69b383384c451191d77fc0dd7d5944636fd9bae56e4f7bd29be5813"),
    ("coverage-safe gc", "a2ea68b85f0baa837d8a590f94f8db969c3a91c40780d4245344cd2f8bb5102d"),
    ("maximum coverage fact and claim persisted", "e4bbfb78ba450793678bdadf4f7c9ebb7e8a9548c0c24e507f36864df3a04507"),
    ("maximum coverage removed with fact", "a2ea68b85f0baa837d8a590f94f8db969c3a91c40780d4245344cd2f8bb5102d"),
    ("pending write accepted", "381fbfa5a9038fbeb97e4824dd4ad6a994dd9c87ee18a659c14b2c2886ebbfa2"),
    ("pending write signed", "861ddd321293e0810d78318f9b68085bb7747bd6b13c81d0910fbddf1d6713f6"),
    ("cancellable write accepted", "2c8b11895defe4166ba2c051564a98a5c200c802171f7810c6a5d8109d386e24"),
    ("write cancelled", "9167708d00dbdfa6177181a83e42dc67b2bddf1f3972fb65ea9222accbeb79a3"),
    ("replaceable delivery accepted", "35cdb477826289aebd57fe11d4b463abbab59811be021afacf61a2fd452383bc"),
    ("replaceable delivery superseded", "ee2b61751a9366da7dacc58b6621ad69ce62ff8cdae50854023ae01678c023d7"),
    ("superseding delivery compensated", "234a899c07ac2e96554acbe187c3bdc69e4ca52c19f2485ef1440bcd4ac0d3e4"),
    ("multi-relay delivery accepted", "89a54e29ceda3a77b86d2cb9a8a741a573756483c3a1b6cab91ba13623fdd33f"),
    ("multi-relay lanes durable", "54b00c6bdb29affed80879448b2da07251aeba28991fb9a83f62282e2aab2d20"),
    ("ambiguous handoff became outcome unknown", "f5fa34bef08e3cabdd4fc2c9fd4519289e2fac798965d46074a402dac4528039"),
    ("delivery attempt gave up", "381224dba9cdc222a82636cdc81e3e579d987bdf5c53352f29f56404e03c1e14"),
    ("in-flight delivery interrupted", "646a9ad9bbb773a51e9c6852e2e0bade37bb1ae3e4b4780f72ba987ba7cd303f"),
    ("retry ended in relay rejection", "045516cfca3a730f4269e9ececa7659f75aa89035433d69e202bd3855562b746"),
    ("multi-relay terminal obligation closed", "045516cfca3a730f4269e9ececa7659f75aa89035433d69e202bd3855562b746"),
    ("publication route durable", "23e6ae7db71c900b4284cc8387deef7335b9d070f26815ad20ca36117367d10a"),
    ("publication lanes bootstrapped", "2b697e6c39c121f62e67a3dfbd14b2d425d7f0ecbda972aa1946b042e60a2a72"),
    ("publication lane eligible", "dfa65796a16c139a2b7a1336ae07052f56dec02796f28c29db0407c602aecdb7"),
    ("publication attempt started", "277921ec9624bab892a32b92b8a507be87d259967130d671ee228d8e6c80b045"),
    ("publication retry scheduled", "f84cebe521948f75a0f1d8b2c228d0cc3178e35e39f0361f978492b49685d72a"),
    ("publication retry eligible", "046e6019218ab1ebabf4f2882017e8b874dd3421dee8d8f4887927e2d22aef93"),
    ("publication retry started", "8a3231cefcdcf4911e2ba7c61a505babd56b3f04939baac161a86377f57c2adf"),
    ("publication handed off", "18367d9e30f68778af9e84fbb2946412b6958e2a3adbc02c50072eedd36982ee"),
    ("publication receipt acked", "ff416389fd48daea76ebeca9d459bad0af2a15dcf05d6f4ecb7b8537db8a9a84"),
    ("publication obligation closed", "ff416389fd48daea76ebeca9d459bad0af2a15dcf05d6f4ecb7b8537db8a9a84"),
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
        authenticate_as: None,
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
                .get_coverage(
                    coverage_key(atom),
                    &RelaySessionKey::unauthenticated(relay.clone()),
                )
                .expect("oracle coverage read");
            json!({
                "key": RedbStore::coverage_row_key(
                    &coverage_key(atom),
                    &RelaySessionKey::unauthenticated(relay.clone()),
                ),
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
                RelaySessionKey::unauthenticated(primary.clone()),
                CoverageInterval::new(Timestamp::from(0), Timestamp::from(300)),
            ),
            (
                atom.clone(),
                RelaySessionKey::unauthenticated(secondary.clone()),
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
            RelaySessionKey::unauthenticated(primary.clone()),
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
