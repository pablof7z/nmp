//! Independent wire and restart witnesses for semantic successor delivery.
//!
//! These tests observe physical `EVENT` frames at relays and durable facts
//! through the supported `Engine` facade. They do not inspect delivery tables
//! or insert the result they are meant to prove.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use nmp::{
    AccessContext, Demand, Engine, EngineConfig, Filter, Identity, LiveQuery, ReceiptReattachment,
    RelayState, ReplaceableMaterializer, ReplaceableMaterializerOperation,
    ReplaceableMaterializerRefusal, ReplaceableMaterializerSpec, Row, RowDelta, RowSignature,
    SignerOp, SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent, SigningCapability,
    SigningState, SourceAuthority, WriteFact, WriteIntent, WriteOutcome, WriteRouting,
};
use nmp_signer::PendingSignerSender;
use nmp_store::{InsertOutcome, RedbStore, RelayObserved};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::nips::nip01::Coordinate;
use nostr::{EventBuilder, EventId, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

const SETTLE: Duration = Duration::from_secs(30);

struct AddPeople;

struct CountingDefaultPeople {
    default_calls: Arc<AtomicUsize>,
}

impl ReplaceableMaterializer for AddPeople {
    fn materialize(
        &self,
        _source: &UnsignedEvent,
        current: &UnsignedEvent,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp::EventBuilder, ReplaceableMaterializerRefusal> {
        let mut tags = current.tags.clone().to_vec();
        for operation in operations {
            let key = PublicKey::from_slice(operation.bytes()).map_err(|error| {
                ReplaceableMaterializerRefusal {
                    reason: error.to_string(),
                }
            })?;
            if !tags
                .iter()
                .any(|tag| tag.as_slice() == ["p", &key.to_hex()])
            {
                tags.push(Tag::public_key(key));
            }
        }
        Ok(nmp::EventBuilder {
            kind: current.kind,
            tags,
            content: current.content.clone(),
            created_at: None,
        })
    }

    fn materialize_default(
        &self,
        coordinate: &Coordinate,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp::EventBuilder, ReplaceableMaterializerRefusal> {
        let mut tags = if coordinate.kind.is_addressable() {
            vec![Tag::identifier(coordinate.identifier.clone())]
        } else {
            Vec::new()
        };
        for operation in operations {
            let key = PublicKey::from_slice(operation.bytes()).map_err(|error| {
                ReplaceableMaterializerRefusal {
                    reason: error.to_string(),
                }
            })?;
            if !tags
                .iter()
                .any(|tag| tag.as_slice() == ["p", &key.to_hex()])
            {
                tags.push(Tag::public_key(key));
            }
        }
        Ok(nmp::EventBuilder {
            kind: coordinate.kind,
            tags,
            content: String::new(),
            created_at: None,
        })
    }
}

impl ReplaceableMaterializer for CountingDefaultPeople {
    fn materialize(
        &self,
        source: &UnsignedEvent,
        current: &UnsignedEvent,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp::EventBuilder, ReplaceableMaterializerRefusal> {
        AddPeople.materialize(source, current, operations)
    }

    fn materialize_default(
        &self,
        coordinate: &Coordinate,
        operations: &[ReplaceableMaterializerOperation<'_>],
    ) -> Result<nmp::EventBuilder, ReplaceableMaterializerRefusal> {
        self.default_calls.fetch_add(1, Ordering::SeqCst);
        AddPeople.materialize_default(coordinate, operations)
    }
}

struct HeldSigner {
    pubkey: PublicKey,
    started: mpsc::Sender<(SignerUnsignedEvent, PendingSignerSender<SignerSignedEvent>)>,
}

impl SigningCapability for HeldSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(SignerPublicKey::new(self.pubkey.to_bytes()))
    }

    fn sign(&self, unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        let (sender, operation) = SignerOp::pending_channel();
        self.started
            .send((unsigned, sender))
            .expect("the test owns the signer request receiver");
        operation
    }
}

fn to_nostr_unsigned(unsigned: SignerUnsignedEvent) -> UnsignedEvent {
    let (public_key, created_at, kind, tags, content) = unsigned.into_parts();
    UnsignedEvent::new(
        PublicKey::from_slice(public_key.as_bytes()).expect("engine supplied a public key"),
        Timestamp::from(created_at),
        Kind::from(kind),
        tags.into_iter()
            .map(Tag::parse)
            .collect::<Result<Vec<_>, _>>()
            .expect("engine supplied valid tags"),
        content,
    )
}

fn to_signer_event(event: nostr::Event) -> SignerSignedEvent {
    SignerSignedEvent::new(
        event.id.to_bytes(),
        SignerPublicKey::new(event.pubkey.to_bytes()),
        event.created_at.as_secs(),
        event.kind.as_u16(),
        event.tags.into_iter().map(|tag| tag.to_vec()).collect(),
        event.content,
        event.sig.serialize(),
    )
}

fn contact_list(keys: &Keys, at: u64, content: &str, people: &[PublicKey]) -> nostr::Event {
    EventBuilder::new(Kind::ContactList, content)
        .tags(people.iter().copied().map(Tag::public_key))
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(keys)
        .expect("fixture contact list signs")
}

fn pinned_contact_lists(
    relays: impl IntoIterator<Item = nostr::RelayUrl>,
    author: PublicKey,
) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                authors: Some(nmp::Binding::Literal(BTreeSet::from([author.to_hex()]))),
                ..Filter::default()
            },
            SourceAuthority::Pinned(relays.into_iter().collect()),
            AccessContext::Public,
        )
        .expect("one pinned source is valid"),
    )
}

fn pinned_kind(
    relays: impl IntoIterator<Item = nostr::RelayUrl>,
    author: PublicKey,
    kind: Kind,
) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([kind.as_u16()])),
                authors: Some(nmp::Binding::Literal(BTreeSet::from([author.to_hex()]))),
                ..Filter::default()
            },
            SourceAuthority::Pinned(relays.into_iter().collect()),
            AccessContext::Public,
        )
        .expect("one pinned source is valid"),
    )
}

fn row_contains_person(row: &Row, person: PublicKey) -> bool {
    let expected = person.to_hex();
    row.tags()
        .iter()
        .any(|tag| tag.as_slice() == ["p", expected.as_str()])
}

fn apply_rows(rows: &mut BTreeMap<EventId, Row>, deltas: Vec<RowDelta>) {
    for delta in deltas {
        match delta {
            RowDelta::Added(row) | RowDelta::Updated(row) => {
                rows.insert(row.id(), row);
            }
            RowDelta::SourcesGrew { id, sources } => {
                if let Some(row) = rows.get_mut(&id) {
                    row.sources = sources;
                }
            }
            RowDelta::Removed(id) => {
                rows.remove(&id);
            }
        }
    }
}

fn wait_for_row(
    subscription: &nmp::Subscription,
    rows: &mut BTreeMap<EventId, Row>,
    predicate: impl Fn(&Row) -> bool,
) -> Row {
    let deadline = Instant::now() + SETTLE;
    loop {
        if let Some(row) = rows.values().find(|row| predicate(row)).cloned() {
            return row;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "row did not arrive; current={rows:?}");
        let frame = subscription
            .recv_timeout(remaining)
            .expect("live query remains open");
        apply_rows(rows, frame.deltas);
    }
}

fn wait_for_generation_relay_facts(
    statuses: &nmp::mechanism::runtime::FifoReceiver<WriteFact>,
    event_id: EventId,
    relays: &BTreeSet<nostr::RelayUrl>,
) -> Vec<WriteFact> {
    let deadline = Instant::now() + SETTLE;
    let mut facts = Vec::new();
    let mut published = BTreeSet::new();
    while &published != relays {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(
            !remaining.is_zero(),
            "generation facts did not arrive for {event_id}; saw {facts:?}"
        );
        let fact = statuses.recv_timeout(remaining).unwrap_or_else(|error| {
            panic!(
                "receipt remains attached while waiting for {event_id}; \
                     error={error:?}; saw={facts:?}; published={published:?}"
            )
        });
        match &fact {
            WriteFact::Relay {
                event_id: relay_event,
                relay,
                state: RelayState::Published,
            } if *relay_event == event_id => {
                published.insert(relay.clone());
            }
            _ => {}
        }
        // Settlement is terminal and implies every routed lane already
        // reached a terminal state, so it ends the wait rather than
        // extending it past the end of the stream.
        let settled = matches!(fact, WriteFact::Outcome(WriteOutcome::Settled));
        facts.push(fact);
        if settled {
            break;
        }
    }
    facts
}

/// Assert one receipt is still open because a destination it is routed to
/// has not answered.
///
/// This is a NAMED reason, not the old blanket "a semantic receipt never
/// settles": #1631 ends active semantic work exactly when routing is closed
/// and every lane of the current generation is terminal. A routed relay that
/// is not running has no terminal lane, so the obligation is genuinely
/// outstanding.
fn assert_open_pending_unreachable_destination(facts: &[WriteFact]) {
    assert!(
        facts
            .iter()
            .all(|fact| !matches!(fact, WriteFact::Outcome(WriteOutcome::Settled))),
        "a receipt with an unreachable routed destination settled: {facts:?}"
    );
}

/// One routed destination that will never answer: a relay started only long
/// enough to claim a port, then stopped.
///
/// #1631 ends active semantic work when routing is closed and every lane of
/// the current generation is terminal. A test about SUCCESSORS or about a
/// generation shared by several receipts needs its cohort to stay open while
/// it makes its point, and an unreachable destination is the honest way to
/// keep it open -- it is exactly the epic's own scenario, where r1 publishes
/// and r2 is offline.
async fn unreachable_destination() -> nostr::RelayUrl {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let url = relay.url.clone();
    relay.shutdown();
    url
}

fn attached_statuses(
    reattachment: ReceiptReattachment,
) -> nmp::mechanism::runtime::FifoReceiver<WriteFact> {
    match reattachment {
        ReceiptReattachment::Attached { statuses, .. } => statuses,
        ReceiptReattachment::NotFound => panic!("retained receipt disappeared"),
        ReceiptReattachment::RetainedButUnreadable => panic!("retained receipt became unreadable"),
    }
}

fn wait_for_settled(statuses: &nmp::mechanism::runtime::FifoReceiver<WriteFact>) {
    let deadline = Instant::now() + SETTLE;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "receipt did not settle");
        let fact = statuses
            .recv_timeout(remaining)
            .expect("receipt remains attached until settlement");
        if matches!(fact, WriteFact::Outcome(WriteOutcome::Settled)) {
            return;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn capability_default_survives_restart_and_replays_over_later_source() {
    let source_config = RelayConfig {
        query_delay: Some(Duration::from_secs(30)),
        ..RelayConfig::default()
    };
    let mut source = ScriptedRelay::start(&source_config).await;
    let source_port = source.port();
    let source_url = source.url.clone();
    let destination = ScriptedRelay::start(&RelayConfig::default()).await;
    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    let bob = Keys::generate().public_key();
    let carol = Keys::generate().public_key();
    let directory = tempfile::tempdir().expect("persistent fixture directory");
    let store_path = directory.path().join("capability-default.redb");
    let config = || EngineConfig {
        store_path: Some(store_path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    };

    let default_calls = Arc::new(AtomicUsize::new(0));
    let spec = ReplaceableMaterializerSpec::new(
        [66; 16],
        [67; 16],
        CountingDefaultPeople {
            default_calls: Arc::clone(&default_calls),
        },
    );
    let materializer = spec.handle();
    let engine = Arc::new(
        Engine::new_with_capabilities(config(), vec![spec]).expect("persistent engine opens"),
    );
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("local author account registers");
    let subscription = engine
        .observe(
            pinned_contact_lists([source_url.clone()], author.public_key()),
            None,
        )
        .expect("source observation opens before any source exists");
    assert!(
        source
            .wait_query_for_kind(Kind::ContactList.as_u16(), SETTLE)
            .await,
        "the relay is holding the source query open before first-value custody"
    );
    let first_intent = WriteIntent {
        payload: materializer
            .first_value_operation(Kind::ContactList, String::new(), alice.to_bytes().to_vec())
            .expect("capability default operation is complete"),
        routing: WriteRouting::Explicit(vec![destination.url.clone()]),
        identity: Identity::Active,
        correlation: None,
    };
    let (custody_tx, custody_rx) = mpsc::channel();
    let custody_engine = Arc::clone(&engine);
    let custody_worker = std::thread::spawn(move || {
        custody_tx
            .send(custody_engine.publish(first_intent))
            .expect("the bounded custody witness remains alive");
    });
    let receipt = match custody_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(receipt)) => {
            custody_worker
                .join()
                .expect("the bounded first-value publisher exits");
            receipt
        }
        failure => {
            let reason = match failure {
                Ok(Err(error)) => format!("publication failed: {error:?}"),
                Err(error) => format!("bounded wait failed: {error:?}"),
                Ok(Ok(_)) => unreachable!("successful custody matched the success arm"),
            };
            engine.shutdown();
            let _ = custody_rx.recv_timeout(Duration::from_secs(5));
            custody_worker
                .join()
                .expect("the closed first-value publisher exits during cleanup");
            source.shutdown();
            destination.shutdown();
            panic!("first-value custody waited for the delayed relay response or failed: {reason}");
        }
    };
    let mut rows = BTreeMap::new();
    let initial = wait_for_row(&subscription, &mut rows, |row| row.id() == receipt.event_id);
    assert_eq!(default_calls.load(Ordering::SeqCst), 1);
    assert!(row_contains_person(&initial, alice));
    assert!(!row_contains_person(&initial, bob));
    assert_eq!(initial.content(), "");
    assert!(
        destination
            .wait_wire_event_id_count(&initial.id().to_hex(), 1, SETTLE)
            .await,
        "the complete default-based generation reaches its destination"
    );
    let second_receipt = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&initial, carol.to_bytes().to_vec())
                .expect("a later operation composes over the local generation"),
            routing: WriteRouting::Explicit(vec![destination.url.clone()]),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("the second operation enters the same durable program");
    assert_ne!(receipt.id, second_receipt.id);
    let current = wait_for_row(&subscription, &mut rows, |row| {
        row.id() == second_receipt.event_id
    });
    assert!(row_contains_person(&current, alice));
    assert!(row_contains_person(&current, carol));
    assert_eq!(
        default_calls.load(Ordering::SeqCst),
        2,
        "every pre-source generation replays the complete operation program over the capability default"
    );

    engine.shutdown();
    drop(subscription);
    drop(engine);
    source.disconnect().await;

    let relay_source = contact_list(
        &author,
        Timestamp::now().as_secs().saturating_add(5),
        "relay-owned fields survive",
        &[bob],
    );
    let mut closed_store = RedbStore::open(&store_path).expect("closed store reopens for input");
    assert!(matches!(
        closed_store
            .insert(
                relay_source.clone(),
                RelayObserved::new(source_url.clone(), Timestamp::now()),
            )
            .expect("newer source is durably observed while the engine is closed"),
        InsertOutcome::Superseded { .. }
    ));
    drop(closed_store);

    let reopened = Engine::new_with_capabilities(
        config(),
        vec![
            ReplaceableMaterializerSpec::new(
                [66; 16],
                [67; 16],
                CountingDefaultPeople {
                    default_calls: Arc::clone(&default_calls),
                },
            ),
            ReplaceableMaterializerSpec::new([68; 16], [69; 16], AddPeople),
        ],
    )
    .expect("persistent engine reopens");
    reopened
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("author account reattaches");
    let statuses = attached_statuses(
        reopened
            .reattach_receipt(receipt.id)
            .expect("ordinary receipt reattaches with the same id"),
    );
    let second_statuses = attached_statuses(
        reopened
            .reattach_receipt(second_receipt.id)
            .expect("the second ordinary receipt also keeps its id"),
    );
    let reopened_subscription = reopened
        .observe(
            pinned_contact_lists([source_url.clone()], author.public_key()),
            None,
        )
        .expect("reopened source observation attaches");
    let mut reopened_rows = BTreeMap::new();
    let successor = wait_for_row(&reopened_subscription, &mut reopened_rows, |row| {
        row.content() == "relay-owned fields survive"
            && row_contains_person(row, alice)
            && row_contains_person(row, bob)
            && row_contains_person(row, carol)
    });
    assert_eq!(
        default_calls.load(Ordering::SeqCst),
        2,
        "boot uses the durable source without rerunning the default builder"
    );
    assert_ne!(successor.id(), initial.id());
    assert_ne!(successor.id(), current.id());
    source = ScriptedRelay::start_on_port(source_port, &RelayConfig::default()).await;
    assert_eq!(source.url, source_url);
    source.seed_signed_event(&relay_source).await;
    assert!(
        source.wait_contacted(SETTLE).await,
        "the reopened finite source request reaches the rebound relay"
    );
    assert_eq!(default_calls.load(Ordering::SeqCst), 2);
    assert!(
        destination
            .wait_wire_event_id_count(&successor.id().to_hex(), 1, SETTLE)
            .await,
        "the source-based successor reaches the original destination"
    );
    wait_for_settled(&statuses);
    wait_for_settled(&second_statuses);

    let parameterized_kind = Kind::from(30_001u16);
    let parameterized_subscription = reopened
        .observe(
            pinned_kind(
                [source_url.clone()],
                author.public_key(),
                parameterized_kind,
            ),
            None,
        )
        .expect("parameterized observation opens");
    let parameterized = reopened
        .publish(WriteIntent {
            payload: ReplaceableMaterializerSpec::new([68; 16], [69; 16], AddPeople)
                .handle()
                .first_value_operation(
                    parameterized_kind,
                    "bookmarks".to_string(),
                    alice.to_bytes().to_vec(),
                )
                .expect("parameterized default operation is complete"),
            routing: WriteRouting::Explicit(vec![destination.url.clone()]),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("parameterized first value enters the same custody path");
    let mut parameterized_rows = BTreeMap::new();
    let parameterized_row = wait_for_row(
        &parameterized_subscription,
        &mut parameterized_rows,
        |row| row.id() == parameterized.event_id,
    );
    assert!(row_contains_person(&parameterized_row, alice));
    assert!(parameterized_row
        .tags()
        .iter()
        .any(|tag| tag.as_slice() == ["d", "bookmarks"]));

    reopened.shutdown();
    source.shutdown();
    destination.shutdown();
}

/// #1631's headline behavior, driven through the door an APP uses: publish,
/// a real relay, a real `OK`, and the receipt says so.
///
/// This deliberately enters through `Engine::publish` and a live websocket
/// rather than calling the store's cohort-close function, because the bug
/// this replaces lived precisely in the gap between the two -- the store
/// function worked and nothing on earth reached it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_delivered_semantic_write_settles_its_receipt() {
    let relay = ScriptedRelay::start(&RelayConfig::default()).await;
    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    let base = contact_list(
        &author,
        Timestamp::now().as_secs().saturating_sub(10),
        "base",
        &[],
    );
    relay.seed_signed_event(&base).await;

    let spec = ReplaceableMaterializerSpec::new([46; 16], [47; 16], AddPeople);
    let materializer = spec.handle();
    let engine =
        Engine::new_with_capabilities(EngineConfig::default(), vec![spec]).expect("engine opens");
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("local author account registers");
    let subscription = engine
        .observe(
            pinned_contact_lists([relay.url.clone()], author.public_key()),
            None,
        )
        .expect("contact list observation opens");
    let mut rows = BTreeMap::new();
    let base_row = wait_for_row(&subscription, &mut rows, |row| row.id() == base.id);

    let receipt = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&base_row, alice.to_bytes().to_vec())
                .expect("the operation is complete"),
            routing: WriteRouting::Explicit(vec![relay.url.clone()]),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("the operation enters custody");
    let generation = wait_for_row(&subscription, &mut rows, |row| row.id() == receipt.event_id);
    assert!(
        relay
            .wait_wire_event_id_count(&generation.id().to_hex(), 1, SETTLE)
            .await,
        "the generation reaches its one destination"
    );
    wait_for_settled(&receipt.statuses);

    let queue = engine
        .publish_queue(None, u8::MAX)
        .expect("publish queue remains readable");
    assert_eq!(
        queue
            .iter()
            .filter(|entry| entry.receipt_id == receipt.id)
            .map(|entry| entry.outcome.clone())
            .collect::<Vec<_>>(),
        vec![Some(WriteOutcome::Settled)],
        "the settled obligation reports its outcome once: {queue:?}"
    );

    engine.shutdown();
    relay.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_second_generation_is_once_per_relay_and_replays_while_a_destination_is_down() {
    let relay_one = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_two = ScriptedRelay::start(&RelayConfig::default()).await;
    let offline = unreachable_destination().await;
    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    let bob = Keys::generate().public_key();
    let initial_time = Timestamp::now().as_secs().saturating_sub(10);
    let base = contact_list(&author, initial_time, "base", &[]);
    relay_two.seed_signed_event(&base).await;

    let directory = tempfile::tempdir().expect("persistent fixture directory");
    let store_path = directory.path().join("semantic-delivery.redb");
    let config = || EngineConfig {
        store_path: Some(store_path.to_string_lossy().into_owned()),
        ..EngineConfig::default()
    };
    let spec = ReplaceableMaterializerSpec::new([6; 16], [7; 16], AddPeople);
    let materializer = spec.handle();
    let engine =
        Engine::new_with_capabilities(config(), vec![spec]).expect("persistent engine opens");
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("local author account registers");
    let subscription = engine
        .observe(
            pinned_contact_lists([relay_two.url.clone()], author.public_key()),
            None,
        )
        .expect("contact list observation opens");
    let mut rows = BTreeMap::new();
    let base_row = wait_for_row(&subscription, &mut rows, |row| row.id() == base.id);

    let first = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&base_row, alice.to_bytes().to_vec())
                .expect("first operation is complete"),
            routing: WriteRouting::Explicit(vec![relay_one.url.clone(), offline.clone()]),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("first operation enters custody");
    let first_current = wait_for_row(&subscription, &mut rows, |row| row.id() == first.event_id);
    assert!(
        relay_one
            .wait_wire_event_id_count(&first_current.id().to_hex(), 1, SETTLE)
            .await,
        "relay 1 must independently witness the first generation"
    );
    let first_relays = BTreeSet::from([relay_one.url.clone()]);
    let first_facts =
        wait_for_generation_relay_facts(&first.statuses, first_current.id(), &first_relays);
    assert_open_pending_unreachable_destination(&first_facts);
    let second = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&first_current, bob.to_bytes().to_vec())
                .expect("second operation composes over current"),
            routing: WriteRouting::Explicit(vec![
                relay_one.url.clone(),
                relay_two.url.clone(),
                offline.clone(),
            ]),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("second operation enters custody");
    let e2 = wait_for_row(&subscription, &mut rows, |row| row.id() == second.event_id);
    let expected_relays = BTreeSet::from([relay_one.url.clone(), relay_two.url.clone()]);
    for relay in [&relay_one, &relay_two] {
        assert!(
            relay
                .wait_wire_event_id_count(&e2.id().to_hex(), 1, SETTLE)
                .await,
            "{} must independently witness E2; wire={:?}; queue={:?}",
            relay.url,
            relay.wire_record(),
            engine.publish_queue(None, u8::MAX)
        );
        relay
            .wait_wire_quiet(Duration::from_millis(250), SETTLE)
            .await;
        assert_eq!(
            relay
                .wire_record()
                .event_ids
                .iter()
                .filter(|candidate| *candidate == &e2.id().to_hex())
                .count(),
            1,
            "one semantic generation has one physical EVENT per destination"
        );
    }

    for statuses in [&first.statuses, &second.statuses] {
        let facts = wait_for_generation_relay_facts(statuses, e2.id(), &expected_relays);
        assert!(
            facts.iter().any(|fact| matches!(
                fact,
                WriteFact::Signing(SigningState::Signed { event_id }) if *event_id == e2.id()
            )),
            "every contributing live receipt must observe E2 signing: {facts:?}"
        );
        assert_open_pending_unreachable_destination(&facts);
        assert!(facts.iter().all(|fact| {
            !matches!(fact, WriteFact::Relay { event_id, .. } if *event_id != first_current.id() && *event_id != e2.id())
        }));
    }
    let before_restart = engine
        .publish_queue(None, u8::MAX)
        .expect("publish queue remains readable");
    assert_eq!(before_restart.len(), 2);
    assert!(
        before_restart.iter().all(|entry| {
            entry.event_id == e2.id()
                && entry.outcome.is_none()
                && entry.relay_states.iter().all(|(relay, state)| {
                    if relay == &offline {
                        matches!(state, RelayState::Waiting(_))
                    } else {
                        matches!(state, RelayState::Published)
                    }
                })
        }),
        "both receipts stay open on the unreachable destination after E2 reached every \
         reachable one: {before_restart:?}"
    );

    let first_id = first.id;
    let second_id = second.id;
    let session = engine.export_session().expect("session export succeeds");
    drop(subscription);
    engine.shutdown();

    let restarted = Engine::new_with_session_and_capabilities(
        config(),
        session,
        vec![ReplaceableMaterializerSpec::new(
            [6; 16], [7; 16], AddPeople,
        )],
    )
    .expect("engine restarts");
    for (receipt, owns_predecessor) in [(first_id, true), (second_id, false)] {
        let statuses = attached_statuses(
            restarted
                .reattach_receipt(receipt)
                .expect("receipt reattaches"),
        );
        let replay = wait_for_generation_relay_facts(&statuses, e2.id(), &expected_relays);
        if owns_predecessor {
            assert!(
                replay.iter().any(|fact| matches!(
                    fact,
                    WriteFact::Relay { event_id, .. } if *event_id == first_current.id()
                )),
                "predecessor evidence remains historical and event-qualified: {replay:?}"
            );
        }
        assert_open_pending_unreachable_destination(&replay);
    }
    let after_restart = restarted
        .publish_queue(None, u8::MAX)
        .expect("restarted publish queue remains readable");
    assert_eq!(after_restart.len(), 2);
    assert!(
        after_restart.iter().all(|entry| {
            entry.event_id == e2.id()
                && entry.outcome.is_none()
                && entry.relay_states.iter().all(|(relay, state)| {
                    if relay == &offline {
                        matches!(state, RelayState::Waiting(_))
                    } else {
                        matches!(state, RelayState::Published)
                    }
                })
        }),
        "restart preserves two receipts still owed one unreachable destination: \
         {after_restart:?}"
    );
    restarted.shutdown();
    relay_one.shutdown();
    relay_two.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn route_only_addition_preserves_signed_e2_and_sends_only_the_new_destination() {
    let relay_a = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_b = ScriptedRelay::start(&RelayConfig::default()).await;
    let source = ScriptedRelay::start(&RelayConfig::default()).await;
    // Hold the author-route query open long enough for E2 to sign and reach
    // the already-known app relay. The exact raw REQ below proves that Bob's
    // route is genuinely pending before the route-only addition is seeded.
    let indexer = ScriptedRelay::start(&RelayConfig {
        query_delay: Some(Duration::from_secs(10)),
        ..RelayConfig::default()
    })
    .await;
    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    let bob = Keys::generate();
    let initial_time = Timestamp::now().as_secs().saturating_sub(10);
    let base = contact_list(&author, initial_time, "base", &[]);
    source.seed_signed_event(&base).await;

    let spec = ReplaceableMaterializerSpec::new([16; 16], [17; 16], AddPeople);
    let materializer = spec.handle();
    let engine = Engine::new_with_capabilities(
        EngineConfig {
            indexer_relays: vec![indexer.url.to_string()],
            app_relays: vec![relay_a.url.to_string()],
            ..EngineConfig::default()
        },
        vec![spec],
    )
    .expect("engine opens with an app relay and one NIP-65 indexer");
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("local author account registers");
    let subscription = engine
        .observe(
            pinned_contact_lists([source.url.clone()], author.public_key()),
            None,
        )
        .expect("contact list observation opens");
    let mut rows = BTreeMap::new();
    let base_row = wait_for_row(&subscription, &mut rows, |row| row.id() == base.id);

    let first = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&base_row, alice.to_bytes().to_vec())
                .expect("first operation is complete"),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("first operation enters custody");
    let first_current = wait_for_row(&subscription, &mut rows, |row| row.id() == first.event_id);
    let second = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&first_current, bob.public_key().to_bytes().to_vec())
                .expect("second operation composes over current"),
            routing: WriteRouting::Auto,
            identity: Identity::Active,
            correlation: None,
        })
        .expect("second operation enters custody");
    let e2 = wait_for_row(&subscription, &mut rows, |row| {
        row.id() == second.event_id && matches!(row.signature(), RowSignature::Signed(_))
    });
    let RowSignature::Signed(e2_signature) = e2.signature() else {
        unreachable!("the predicate selected only a signed E2 row")
    };

    let bob_hex = bob.public_key().to_hex();
    indexer
        .wait_wire_req(SETTLE, |request| {
            request.kinds().contains(&10_002) && request.authors().contains(&bob_hex)
        })
        .await
        .expect("the live NIP-65 request waits for Bob's exact route");
    assert!(
        relay_a
            .wait_wire_event_id_count(&e2.id().to_hex(), 1, SETTLE)
            .await,
        "the already-known app relay receives E2"
    );
    let facts = wait_for_generation_relay_facts(
        &second.statuses,
        e2.id(),
        &BTreeSet::from([relay_a.url.clone()]),
    );
    assert!(facts.iter().any(|fact| matches!(
        fact,
        WriteFact::Signing(SigningState::Signed { event_id }) if *event_id == e2.id()
    )));

    // Only routing knowledge changes here. Bob's newer relay-list fact says
    // his inbox is relay B; no semantic operation or source event is added.
    indexer
        .seed_relay_list(
            &bob,
            &[],
            &[relay_b.url.to_string()],
            initial_time.saturating_add(1),
        )
        .await;
    assert!(
        relay_b
            .wait_wire_event_id_count(&e2.id().to_hex(), 1, SETTLE)
            .await,
        "the newly learned destination receives the already-signed E2"
    );
    relay_a
        .wait_wire_quiet(Duration::from_millis(250), SETTLE)
        .await;
    relay_b
        .wait_wire_quiet(Duration::from_millis(250), SETTLE)
        .await;

    let count_e2 = |relay: &ScriptedRelay| {
        relay
            .wire_record()
            .event_ids
            .iter()
            .filter(|candidate| candidate.as_str() == e2.id().to_hex())
            .count()
    };
    assert_eq!(count_e2(&relay_a), 1, "terminal relay A is never resent E2");
    assert_eq!(count_e2(&relay_b), 1, "relay B receives exactly one E2");
    let delivered_b = relay_b
        .admitted_events()
        .into_iter()
        .find(|event| event.id == e2.id())
        .expect("relay B admits the exact E2 frame");
    assert_eq!(delivered_b.sig, e2_signature);
    assert_eq!(delivered_b.id, second.event_id);

    engine.shutdown();
    relay_a.shutdown();
    relay_b.shutdown();
    source.shutdown();
    indexer.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_session_replacement_wakes_every_signed_successor_destination() {
    let offline = unreachable_destination().await;
    let relay_one = ScriptedRelay::start(&RelayConfig::default()).await;
    let mut relay_two = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_two_url = relay_two.url.clone();
    let relay_two_port = relay_two.port();

    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    let initial_time = Timestamp::now().as_secs().saturating_sub(20);
    let base = contact_list(&author, initial_time, "base", &[]);
    relay_one.seed_signed_event(&base).await;

    let spec = ReplaceableMaterializerSpec::new([26; 16], [27; 16], AddPeople);
    let materializer = spec.handle();
    let engine = Engine::new_with_capabilities(
        EngineConfig {
            // Six physical sessions live here (#8 splits read and write
            // per relay), and the unreachable destination holds two of
            // them in reconnect backoff for the whole test.
            max_relays: 32,
            ..EngineConfig::default()
        },
        vec![spec],
    )
    .expect("engine opens");
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("local author account registers");
    let source_relays = [relay_one.url.clone(), relay_two_url.clone()];
    let subscription = engine
        .observe(
            pinned_contact_lists(source_relays.clone(), author.public_key()),
            None,
        )
        .expect("the two-relay source observation opens");
    let mut rows = BTreeMap::new();
    let base_row = wait_for_row(&subscription, &mut rows, |row| row.id() == base.id);

    let receipt = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&base_row, alice.to_bytes().to_vec())
                .expect("the body-complete operation composes"),
            routing: WriteRouting::Explicit(
                source_relays
                    .iter()
                    .cloned()
                    .chain([offline.clone()])
                    .collect(),
            ),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("the operation enters custody");
    let e1 = wait_for_row(&subscription, &mut rows, |row| row.id() == receipt.event_id);
    let relays = BTreeSet::from([relay_one.url.clone(), relay_two.url.clone()]);
    for relay in [&relay_one, &relay_two] {
        assert!(
            relay
                .wait_wire_event_id_count(&e1.id().to_hex(), 1, SETTLE)
                .await,
            "the first complete generation reaches {}",
            relay.url
        );
    }
    assert_open_pending_unreachable_destination(&wait_for_generation_relay_facts(
        &receipt.statuses,
        e1.id(),
        &relays,
    ));

    let newer = contact_list(&author, initial_time + 5, "relay-two", &[]);
    relay_two.disconnect().await;
    relay_two = ScriptedRelay::start_on_port(
        relay_two_port,
        &RelayConfig {
            query_delay: Some(Duration::from_millis(250)),
            ..RelayConfig::default()
        },
    )
    .await;
    assert_eq!(relay_two.url, relay_two_url);
    relay_two.seed_signed_event(&newer).await;
    let e2 = wait_for_row(&subscription, &mut rows, |row| {
        row.id() != e1.id()
            && row.content() == "relay-two"
            && matches!(row.signature(), RowSignature::Signed(_))
    });

    for relay in [&relay_one, &relay_two] {
        assert!(
            relay
                .wait_wire_event_id_count(&e2.id().to_hex(), 1, SETTLE)
                .await,
            "the signed E2 successor reaches {}; wire={:?}; queue={:?}",
            relay.url,
            relay.wire_record(),
            engine.publish_queue(None, u8::MAX)
        );
        relay
            .wait_wire_quiet(Duration::from_millis(250), SETTLE)
            .await;
        assert_eq!(
            relay
                .wire_record()
                .event_ids
                .iter()
                .filter(|candidate| *candidate == &e2.id().to_hex())
                .count(),
            1,
            "E2 is published exactly once to {}",
            relay.url
        );
    }

    engine.shutdown();
    relay_one.shutdown();
    relay_two.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn relay_source_successors_resume_current_delivery_and_stay_open_after_restart() {
    let offline = unreachable_destination().await;
    let relay_one = ScriptedRelay::start(&RelayConfig::default()).await;
    let mut relay_two = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_two_url = relay_two.url.clone();
    let relay_two_port = relay_two.port();
    let author = Keys::generate();
    let alice = Keys::generate().public_key();
    let initial_time = Timestamp::now().as_secs().saturating_sub(20);
    let base = contact_list(&author, initial_time, "base", &[]);
    relay_one.seed_signed_event(&base).await;

    let directory = tempfile::tempdir().expect("persistent fixture directory");
    let store_path = directory.path().join("semantic-successor-restart.redb");
    let config = || EngineConfig {
        store_path: Some(store_path.to_string_lossy().into_owned()),
        // Six physical sessions live here (#8 splits read and write per
        // relay), and the unreachable destination holds two of them in
        // reconnect backoff for the whole test.
        max_relays: 32,
        ..EngineConfig::default()
    };
    let (first_signer_tx, first_signer_rx) = mpsc::channel();
    let spec = ReplaceableMaterializerSpec::new([36; 16], [37; 16], AddPeople);
    let materializer = spec.handle();
    let engine =
        Engine::new_with_capabilities(config(), vec![spec]).expect("persistent engine opens");
    engine
        .add_public_key_account(author.public_key(), true)
        .expect("author account registers");
    engine
        .install_test_signing_capability(HeldSigner {
            pubkey: author.public_key(),
            started: first_signer_tx,
        })
        .expect("held signer registers");
    let source_relays = [relay_one.url.clone(), relay_two_url.clone()];
    let subscription = engine
        .observe(
            pinned_contact_lists(source_relays.clone(), author.public_key()),
            None,
        )
        .expect("two-relay source observation opens");
    let mut rows = BTreeMap::new();
    let base_row = wait_for_row(&subscription, &mut rows, |row| row.id() == base.id);
    let receipt = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&base_row, alice.to_bytes().to_vec())
                .expect("body-complete operation composes"),
            routing: WriteRouting::Explicit(
                source_relays
                    .iter()
                    .cloned()
                    .chain([offline.clone()])
                    .collect(),
            ),
            identity: Identity::Active,
            correlation: None,
        })
        .expect("operation enters custody");
    let (e1_unsigned, e1_completion) = first_signer_rx
        .recv_timeout(SETTLE)
        .expect("E1 reaches the held signer");
    let e1 = to_nostr_unsigned(e1_unsigned)
        .sign_with_keys(&author)
        .expect("E1 signs");
    e1_completion
        .resolve(Ok(to_signer_event(e1.clone())))
        .expect("E1 completion remains live");
    assert!(
        relay_one
            .wait_wire_event_id_count(&e1.id.to_hex(), 1, SETTLE)
            .await,
        "relay one receives E1"
    );

    let newer = contact_list(&author, initial_time + 5, "relay-two", &[]);
    relay_two.disconnect().await;
    relay_two = ScriptedRelay::start_on_port(relay_two_port, &RelayConfig::default()).await;
    assert_eq!(relay_two.url, relay_two_url);
    relay_two.seed_signed_event(&newer).await;
    let (e2_unsigned_before, _cancelled_completion) = first_signer_rx
        .recv_timeout(SETTLE)
        .expect("relay source successor E2 reaches the held signer");
    let expected_e2 = to_nostr_unsigned(e2_unsigned_before.clone());
    assert_ne!(expected_e2.id, Some(e1.id));
    let e1_counts = [
        relay_one
            .wire_record()
            .event_ids
            .iter()
            .filter(|id| *id == &e1.id.to_hex())
            .count(),
        relay_two
            .wire_record()
            .event_ids
            .iter()
            .filter(|id| *id == &e1.id.to_hex())
            .count(),
    ];

    let receipt_id = receipt.id;
    let session = engine.export_session().expect("session exports");
    drop(subscription);
    engine.shutdown();

    let (restarted_signer_tx, restarted_signer_rx) = mpsc::channel();
    let restarted = Engine::new_with_session_and_capabilities(
        config(),
        session,
        vec![ReplaceableMaterializerSpec::new(
            [36; 16], [37; 16], AddPeople,
        )],
    )
    .expect("engine restarts");
    restarted
        .install_test_signing_capability(HeldSigner {
            pubkey: author.public_key(),
            started: restarted_signer_tx,
        })
        .expect("signer reattaches");
    let statuses = attached_statuses(
        restarted
            .reattach_receipt(receipt_id)
            .expect("receipt reattaches"),
    );
    let (e2_unsigned_after, e2_completion) = restarted_signer_rx
        .recv_timeout(SETTLE)
        .expect("restart requests the exact current E2");
    let e2_after = to_nostr_unsigned(e2_unsigned_after);
    assert_eq!(e2_after.id, expected_e2.id, "restart resumes exact E2");
    let e2 = e2_after.sign_with_keys(&author).expect("E2 signs");
    e2_completion
        .resolve(Ok(to_signer_event(e2.clone())))
        .expect("E2 completion remains live");
    let expected_relays = BTreeSet::from([relay_one.url.clone(), relay_two.url.clone()]);
    for relay in [&relay_one, &relay_two] {
        assert!(
            relay
                .wait_wire_event_id_count(&e2.id.to_hex(), 1, SETTLE)
                .await,
            "restart publishes exact E2 to {}; wire={:?}; queue={:?}",
            relay.url,
            relay.wire_record(),
            restarted.publish_queue(None, u8::MAX)
        );
        relay
            .wait_wire_quiet(Duration::from_millis(250), SETTLE)
            .await;
        assert_eq!(
            relay
                .wire_record()
                .event_ids
                .iter()
                .filter(|id| *id == &e2.id.to_hex())
                .count(),
            1,
            "E2 publishes exactly once to {}",
            relay.url
        );
    }
    let e2_facts = wait_for_generation_relay_facts(&statuses, e2.id, &expected_relays);
    assert!(e2_facts.iter().any(|fact| matches!(
        fact,
        WriteFact::Relay { event_id, .. } if *event_id == e1.id
    )));
    assert_open_pending_unreachable_destination(&e2_facts);
    for (relay, prior_e1_count) in [(&relay_one, e1_counts[0]), (&relay_two, e1_counts[1])] {
        assert_eq!(
            relay
                .wire_record()
                .event_ids
                .iter()
                .filter(|id| *id == &e1.id.to_hex())
                .count(),
            prior_e1_count,
            "restart never retransmits E1 to {}",
            relay.url
        );
    }

    let queue = restarted
        .publish_queue(None, u8::MAX)
        .expect("continuing receipt remains inspectable");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].event_id, e2.id);
    assert!(queue[0].outcome.is_none());

    let _restarted_subscription = restarted
        .observe(
            pinned_contact_lists(source_relays, author.public_key()),
            None,
        )
        .expect("source observation reopens");
    let latest = contact_list(&author, initial_time + 10, "relay-two-latest", &[]);
    relay_two.disconnect().await;
    relay_two = ScriptedRelay::start_on_port(relay_two_port, &RelayConfig::default()).await;
    relay_two.seed_signed_event(&latest).await;
    let (e3_unsigned, e3_completion) = restarted_signer_rx
        .recv_timeout(SETTLE)
        .expect("later qualified source creates E3");
    let e3_unsigned = to_nostr_unsigned(e3_unsigned);
    assert_eq!(e3_unsigned.content, latest.content);
    let e3 = e3_unsigned.sign_with_keys(&author).expect("E3 signs");
    e3_completion
        .resolve(Ok(to_signer_event(e3.clone())))
        .expect("E3 completion remains live");
    for relay in [&relay_one, &relay_two] {
        assert!(
            relay
                .wait_wire_event_id_count(&e3.id.to_hex(), 1, SETTLE)
                .await,
            "continuing operation publishes E3 to {}; wire={:?}; queue={:?}",
            relay.url,
            relay.wire_record(),
            restarted.publish_queue(None, u8::MAX)
        );
        relay
            .wait_wire_quiet(Duration::from_millis(250), SETTLE)
            .await;
        assert_eq!(
            relay
                .wire_record()
                .event_ids
                .iter()
                .filter(|id| *id == &e3.id.to_hex())
                .count(),
            1,
            "E3 publishes exactly once to {}",
            relay.url
        );
    }
    let e3_facts = wait_for_generation_relay_facts(&statuses, e3.id, &expected_relays);
    assert_open_pending_unreachable_destination(&e3_facts);
    let queue = restarted
        .publish_queue(None, u8::MAX)
        .expect("continuing receipt remains inspectable after E3");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].event_id, e3.id);
    assert!(queue[0].outcome.is_none());

    restarted.shutdown();
    relay_one.shutdown();
    relay_two.shutdown();
}
