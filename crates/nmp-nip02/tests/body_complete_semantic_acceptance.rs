//! #1432 capability-owned public capstone: NIP-02 composes body-complete
//! operations through the ordinary `WriteIntent` / `publish` / receipt /
//! `LiveQuery` path -- this proves the door end-to-end against a real
//! engine. Moved back here from `nmp` by #1707 alongside the follow door
//! itself: `nmp` must not carry a NIP-02-specific proof any more than it
//! carries NIP-02-specific code.

use std::collections::BTreeSet;
use std::time::{Duration, Instant};

use nmp::{
    Binding, Demand, Engine, EngineConfig, Filter, Identity, LiveQuery, ReceiptReattachment,
    ReplaceableMaterializer, ReplaceableMaterializerOperation, ReplaceableMaterializerRefusal, Row,
    RowDelta, RowSignature, SigningState, WriteIntent, WriteRouting,
};
use nmp_nip02::{
    follow_capability, follow_writes, set_following, FollowActionFailure, FollowChange,
};
use nmp_store::{RedbStore, RelayObserved};
use nostr::{EventBuilder, EventId, Keys, Kind, RelayUrl, Timestamp};

fn contact(keys: &Keys, created_at: u64) -> nostr::Event {
    EventBuilder::new(Kind::ContactList, "opaque-encrypted-content")
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .expect("fixture signs")
}

fn row(event: &nostr::Event) -> Row {
    Row::from_parts(
        event.id,
        event.pubkey,
        event.created_at,
        event.kind,
        event.tags.clone(),
        event.content.clone(),
        RowSignature::Signed(event.sig),
        BTreeSet::new(),
    )
}

fn wait_for_current(
    subscription: &nmp::Subscription,
    expected_id: EventId,
    expected_people: &[nostr::PublicKey],
) -> Row {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "latest semantic row never appeared");
        let frame = subscription
            .recv_timeout(remaining)
            .expect("query stays open through semantic acceptance");
        if let Some(row) = frame.deltas.into_iter().find_map(|delta| {
            let row = match delta {
                RowDelta::Added(row) | RowDelta::Updated(row) => row,
                _ => return None,
            };
            let people = row
                .tags()
                .iter()
                .filter_map(|tag| {
                    let cells = tag.as_slice();
                    (cells.first().is_some_and(|cell| cell == "p"))
                        .then(|| cells.get(1))
                        .flatten()
                })
                .collect::<BTreeSet<_>>();
            let expected = expected_people
                .iter()
                .map(nostr::PublicKey::to_hex)
                .collect::<BTreeSet<_>>();
            (row.id() == expected_id
                && row.signature() == RowSignature::Pending
                && people == expected.iter().collect())
            .then_some(row)
        }) {
            return row;
        }
    }
}

#[test]
fn alice_then_bob_keep_two_receipts_and_one_complete_pending_event() {
    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    let bob = Keys::generate().public_key();
    let base = contact(&author, 1_800_000_000);
    let temp = tempfile::tempdir().expect("temporary store directory");
    let path = temp.path().join("body-complete-nip02.redb");
    {
        let mut store = RedbStore::open(&path).expect("seed store opens");
        store
            .insert(
                base.clone(),
                RelayObserved::new(
                    RelayUrl::parse("wss://source.example").expect("fixture relay"),
                    Timestamp::from(1_800_000_001),
                ),
            )
            .expect("the signed source is canonical before editing");
    }

    let engine = Engine::new_with_capabilities(
        EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        },
        vec![follow_capability()],
    )
    .expect("engine starts over the seeded store");
    engine
        .add_public_key_account(author.public_key(), true)
        .expect("author is current without installing a signer");
    let writes = follow_writes();
    let subscription = engine
        .observe(
            LiveQuery::single(
                Demand::author_outboxes(Filter {
                    kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                    authors: Some(Binding::Literal(BTreeSet::from([author
                        .public_key()
                        .to_hex()]))),
                    ..Filter::default()
                })
                .expect("the selection binds `authors`"),
            ),
            None,
        )
        .expect("query opens");

    let first = set_following(&engine, &writes, alice, FollowChange::Follow)
        .expect("Alice enters ordinary custody");
    let _alice_row = wait_for_current(&subscription, first.event_id, &[alice]);

    let second = set_following(&engine, &writes, bob, FollowChange::Follow)
        .expect("Bob gets a second ordinary receipt");
    let bob_row = wait_for_current(&subscription, second.event_id, &[alice, bob]);

    assert_ne!(first.id, second.id, "each operation keeps receipt identity");
    assert_ne!(first.event_id, second.event_id);
    assert_eq!(bob_row.content(), base.content, "unowned bytes survive");

    for receipt_id in [first.id, second.id] {
        assert!(matches!(
            engine.reattach_receipt(receipt_id).expect("reattach runs"),
            ReceiptReattachment::Attached { id, .. } if id == receipt_id
        ));
    }

    let queue = engine.publish_queue(None, 10).expect("queue is readable");
    assert_eq!(queue.len(), 2);
    assert_eq!(
        queue
            .iter()
            .map(|entry| entry.receipt_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([first.id, second.id])
    );
    assert!(queue.iter().all(|entry| entry.event_id == second.event_id));
    assert!(queue.iter().all(|entry| matches!(
        entry.signing,
        SigningState::AwaitingSigner { pubkey } if pubkey == author.public_key()
    )));
    assert!(queue.iter().all(|entry| entry.relays.is_empty()));
    assert!(queue.iter().all(|entry| entry.relay_states.is_empty()));

    let receipt_ids = [first.id, second.id];
    let latest_event_id = second.event_id;
    drop(subscription);
    engine.shutdown();
    drop(engine);

    let reopened = Engine::new_with_capabilities(
        EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        },
        vec![follow_capability()],
    )
    .expect("engine reopens the durable semantic state");
    reopened
        .add_public_key_account(author.public_key(), true)
        .expect("the same public identity is restored for this fixture");
    let _writes = follow_writes();
    let recovered_query = reopened
        .observe(
            LiveQuery::single(
                Demand::author_outboxes(Filter {
                    kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                    authors: Some(Binding::Literal(BTreeSet::from([author
                        .public_key()
                        .to_hex()]))),
                    ..Filter::default()
                })
                .expect("the selection binds `authors`"),
            ),
            None,
        )
        .expect("recovered query opens");
    let recovered = wait_for_current(&recovered_query, latest_event_id, &[alice, bob]);
    assert_eq!(recovered.content(), base.content);

    for receipt_id in receipt_ids {
        assert!(matches!(
            reopened
                .reattach_receipt(receipt_id)
                .expect("recovered reattach runs"),
            ReceiptReattachment::Attached { id, .. } if id == receipt_id
        ));
    }
    let recovered_queue = reopened
        .publish_queue(None, 10)
        .expect("recovered queue is readable");
    assert_eq!(recovered_queue.len(), 2);
    assert!(recovered_queue
        .iter()
        .all(|entry| entry.event_id == latest_event_id));
    assert!(recovered_queue.iter().all(|entry| matches!(
        entry.signing,
        SigningState::AwaitingSigner { pubkey } if pubkey == author.public_key()
    )));
    assert!(recovered_queue.iter().all(|entry| entry.relays.is_empty()));
    assert!(recovered_queue
        .iter()
        .all(|entry| entry.relay_states.is_empty()));

    reopened.shutdown();
}

struct AlwaysRefuse;

impl ReplaceableMaterializer for AlwaysRefuse {
    fn materialize(
        &self,
        _source: &nostr::UnsignedEvent,
        _current: &nostr::UnsignedEvent,
        _operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp::EventBuilder, ReplaceableMaterializerRefusal> {
        Err(ReplaceableMaterializerRefusal {
            reason: "fixture refusal".to_string(),
        })
    }

    fn materialize_default(
        &self,
        _coordinate: &nostr::nips::nip01::Coordinate,
        _operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp::EventBuilder, ReplaceableMaterializerRefusal> {
        Err(ReplaceableMaterializerRefusal {
            reason: "fixture refusal".to_string(),
        })
    }
}

#[test]
fn invalidated_registration_and_materializer_refusal_leave_no_custody() {
    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    let base = contact(&author, 1_800_000_100);
    let temp = tempfile::tempdir().expect("temporary store directory");
    let path = temp.path().join("pre-custody-refusal.redb");
    {
        let mut store = RedbStore::open(&path).expect("seed store opens");
        store
            .insert(
                base.clone(),
                RelayObserved::new(
                    RelayUrl::parse("wss://source.example").expect("fixture relay"),
                    Timestamp::from(1_800_000_101),
                ),
            )
            .expect("source is canonical");
    }
    let missing = Engine::new(EngineConfig {
        store_path: Some(path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    })
    .expect("an empty capability set may open a store with no retained operations");
    missing
        .add_public_key_account(author.public_key(), true)
        .expect("author is current");
    let stale = follow_writes();
    assert!(
        matches!(
            set_following(&missing, &stale, alice, FollowChange::Follow),
            Err(FollowActionFailure::PublishRefused { .. })
        ),
        "an unconfigured NIP-02 capability is refused before custody, \
         with the engine's own real refusal reason -- not a follow-only fiction"
    );
    assert!(missing
        .publish_queue(None, 10)
        .expect("queue reads")
        .is_empty());
    missing.shutdown();

    let engine = Engine::new_with_capabilities(
        EngineConfig {
            store_path: Some(path.to_string_lossy().into_owned()),
            ..EngineConfig::default()
        },
        vec![nmp::ReplaceableMaterializerSpec::new(
            *b"refuse-program00",
            *b"refuse-format-v1",
            AlwaysRefuse,
        )],
    )
    .expect("engine starts with the refusing capability");
    engine
        .add_public_key_account(author.public_key(), true)
        .expect("author is current");
    let original = row(&base);
    let refusing = nmp::ReplaceableMaterializerSpec::new(
        *b"refuse-program00",
        *b"refuse-format-v1",
        AlwaysRefuse,
    )
    .handle();
    let payload = refusing
        .operation(&original, vec![1])
        .expect("registration-bound payload composes");
    let refusal = match engine.publish(WriteIntent {
        payload,
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }) {
        Ok(_) => panic!("materializer refusal must remain pre-custody"),
        Err(error) => error,
    };
    assert!(refusal.to_string().contains("fixture refusal"));
    assert!(engine
        .publish_queue(None, 10)
        .expect("queue reads")
        .is_empty());

    let query = engine
        .observe(
            LiveQuery::single(
                Demand::author_outboxes(Filter {
                    kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                    authors: Some(Binding::Literal(BTreeSet::from([author
                        .public_key()
                        .to_hex()]))),
                    ..Filter::default()
                })
                .expect("the selection binds `authors`"),
            ),
            None,
        )
        .expect("query opens");
    let frame = query
        .recv_timeout(Duration::from_secs(2))
        .expect("seed row remains visible");
    assert!(frame.deltas.into_iter().any(|delta| match delta {
        RowDelta::Added(row) | RowDelta::Updated(row) => {
            row.id() == base.id && row.signature() == RowSignature::Signed(base.sig)
        }
        _ => false,
    }));
    engine.shutdown();
}
