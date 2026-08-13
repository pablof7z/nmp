//! Independent wire and restart witnesses for semantic successor delivery.
//!
//! These tests observe physical `EVENT` frames at relays and durable facts
//! through the supported `Engine` facade. They do not inspect delivery tables
//! or insert the result they are meant to prove.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use nmp::{
    AccessContext, Demand, Engine, EngineConfig, Filter, Identity, LiveQuery, ReceiptReattachment,
    RelayState, ReplaceableMaterializer, ReplaceableMaterializerOperation,
    ReplaceableMaterializerRefusal, Row, RowDelta, SourceAuthority, WriteFact, WriteIntent,
    WriteOutcome, WriteRouting,
};
use nmp_test_support::relays::{RelayConfig, ScriptedRelay};
use nostr::{EventBuilder, EventId, Keys, Kind, PublicKey, Tag, Timestamp, UnsignedEvent};

const SETTLE: Duration = Duration::from_secs(30);

struct AddPeople;

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
}

fn contact_list(keys: &Keys, at: u64, content: &str, people: &[PublicKey]) -> nostr::Event {
    EventBuilder::new(Kind::ContactList, content)
        .tags(people.iter().copied().map(Tag::public_key))
        .custom_created_at(Timestamp::from(at))
        .sign_with_keys(keys)
        .expect("fixture contact list signs")
}

fn pinned_contact_lists(relay: &ScriptedRelay, author: PublicKey) -> LiveQuery {
    LiveQuery::single(
        Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([Kind::ContactList.as_u16()])),
                authors: Some(nmp::Binding::Literal(BTreeSet::from([author.to_hex()]))),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay.url.clone()])),
            AccessContext::Public,
        )
        .expect("one pinned source is valid"),
    )
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
                "continuing receipt remains attached while waiting for {event_id}; \
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
        assert!(
            !matches!(fact, WriteFact::Outcome(WriteOutcome::Settled)),
            "a continuing semantic receipt settled after destination completion"
        );
        facts.push(fact);
    }
    facts
}

fn assert_no_settled_fact(facts: &[WriteFact]) {
    assert!(
        facts
            .iter()
            .all(|fact| !matches!(fact, WriteFact::Outcome(WriteOutcome::Settled))),
        "a continuing semantic receipt settled: {facts:?}"
    );
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_second_generation_is_once_per_relay_and_replays_without_settling() {
    let relay_one = ScriptedRelay::start(&RelayConfig::default()).await;
    let relay_two = ScriptedRelay::start(&RelayConfig::default()).await;
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
    let engine = Engine::new(config()).expect("persistent engine opens");
    engine
        .add_private_key_account(&author.secret_key().to_secret_bytes(), true)
        .expect("local author account registers");
    let materializer = engine
        .add_replaceable_materializer([6; 16], [7; 16], AddPeople)
        .expect("semantic capability registers");
    let subscription = engine
        .observe(pinned_contact_lists(&relay_two, author.public_key()), None)
        .expect("contact list observation opens");
    let mut rows = BTreeMap::new();
    let base_row = wait_for_row(&subscription, &mut rows, |row| row.id() == base.id);

    let first = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&base_row, &base_row, alice.to_bytes().to_vec())
                .expect("first operation is complete"),
            routing: WriteRouting::Explicit(vec![relay_one.url.clone()]),
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
    let second = engine
        .publish(WriteIntent {
            payload: materializer
                .operation(&base_row, &first_current, bob.to_bytes().to_vec())
                .expect("second operation composes over current"),
            routing: WriteRouting::Explicit(vec![relay_one.url.clone(), relay_two.url.clone()]),
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
        assert_no_settled_fact(&facts);
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
                && entry
                    .relay_states
                    .iter()
                    .all(|(_, state)| matches!(state, RelayState::Published))
        }),
        "both continuing receipts remain open over terminal E2 relay facts: {before_restart:?}"
    );

    let first_id = first.id;
    let second_id = second.id;
    let session = engine.export_session().expect("session export succeeds");
    drop(subscription);
    engine.shutdown();

    let restarted = Engine::new_with_session(config(), session).expect("engine restarts");
    restarted
        .add_replaceable_materializer([6; 16], [7; 16], AddPeople)
        .expect("semantic capability reattaches");
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
        assert_no_settled_fact(&replay);
    }
    let after_restart = restarted
        .publish_queue(None, u8::MAX)
        .expect("restarted publish queue remains readable");
    assert_eq!(after_restart.len(), 2);
    assert!(
        after_restart.iter().all(|entry| {
            entry.event_id == e2.id()
                && entry.outcome.is_none()
                && entry
                    .relay_states
                    .iter()
                    .all(|(_, state)| matches!(state, RelayState::Published))
        }),
        "restart preserves two open receipts over terminal E2 relay facts: {after_restart:?}"
    );
    restarted.shutdown();
    relay_one.shutdown();
    relay_two.shutdown();
}
