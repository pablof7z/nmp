use std::collections::{BTreeMap, BTreeSet};

use nmp_engine::core::{
    Effect, EngineCore, EngineMsg, HistoryBatch, HistoryLoadError, HistoryLoadFact, HistoryQuery,
    HistorySessionId, HistorySink, RowDelta,
};
use nmp_grammar::{Binding, Filter};
use nmp_resolver::LiveQuery;
use nmp_router::{FixtureDirectory, WireOp};
use nmp_store::{EventStore, MemoryStore, RelayObserved};
use nostr::{Event, Keys, Kind, RelayUrl, Timestamp, UnsignedEvent};

#[derive(Default)]
struct NullHistorySink;

impl HistorySink for NullHistorySink {
    fn on_history(&self, _batch: HistoryBatch) {}
}

fn signed(keys: &Keys, created_at: u64, content: &str) -> Event {
    UnsignedEvent::new(
        keys.public_key(),
        Timestamp::from(created_at),
        Kind::TextNote,
        Vec::new(),
        content,
    )
    .sign_with_keys(keys)
    .unwrap()
}

fn query(keys: &Keys, page_size: usize, max_rows: usize) -> HistoryQuery {
    HistoryQuery::new(
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(Binding::Literal(BTreeSet::from([keys
                .public_key()
                .to_hex()]))),
            ..Filter::default()
        }),
        page_size,
        max_rows,
    )
    .unwrap()
}

fn seeded(count: usize) -> (EngineCore<MemoryStore>, Keys, RelayUrl, Vec<Event>) {
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://history.example").unwrap();
    let mut events: Vec<_> = (0..count)
        .map(|index| signed(&keys, 100, &format!("same-second-{index}")))
        .collect();
    events.sort_by_key(|event| event.id);
    let mut store = MemoryStore::new();
    for event in &events {
        store
            .insert(
                event.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(200)),
            )
            .unwrap();
    }
    let directory = FixtureDirectory::new().with_write(keys.public_key().to_hex(), [relay.clone()]);
    (
        EngineCore::new(store, Box::new(directory), 10),
        keys,
        relay,
        events,
    )
}

fn returned(effects: &[Effect]) -> (HistorySessionId, HistoryBatch) {
    effects
        .iter()
        .rev()
        .find_map(|effect| match effect {
            Effect::EmitHistory(id, batch)
                if matches!(
                    batch.load,
                    HistoryLoadFact::Idle | HistoryLoadFact::Returned { .. }
                ) =>
            {
                Some((*id, batch.clone()))
            }
            _ => None,
        })
        .expect("history operation emits a current batch")
}

fn apply(rows: &mut BTreeMap<nostr::EventId, Event>, batch: &HistoryBatch) {
    for delta in &batch.deltas {
        match delta {
            RowDelta::Added(row) => {
                assert!(rows.insert(row.event.id, row.event.clone()).is_none());
            }
            RowDelta::Removed(id) => {
                assert!(rows.remove(id).is_some());
            }
            RowDelta::SourcesGrew { .. } => {}
        }
    }
}

#[test]
fn coordinated_session_walks_three_same_second_pages_without_gap_or_duplicate() {
    let (mut core, keys, relay, events) = seeded(13);
    let open = core.handle(EngineMsg::SubscribeHistory(
        query(&keys, 5, 13),
        Box::new(NullHistorySink),
    ));
    let (id, first) = returned(&open);
    assert_eq!(first.deltas.len(), 5);
    let mut rows = BTreeMap::new();
    apply(&mut rows, &first);
    let first_continuation = first.continuation.clone().unwrap();

    let second_effects = core.handle(EngineMsg::LoadOlder(id, first_continuation.clone()));
    let (_, second) = returned(&second_effects);
    assert_eq!(second.load, HistoryLoadFact::Returned { added: 5 });
    apply(&mut rows, &second);
    assert_eq!(rows.len(), 10);
    core.handle(EngineMsg::CommitHistoryLoad(id));

    let reqs: Vec<_> = second_effects
        .iter()
        .filter_map(|effect| match effect {
            Effect::Wire(delta) => Some(delta),
            _ => None,
        })
        .flat_map(|delta| &delta.ops)
        .filter(|(url, _)| url == &relay)
        .flat_map(|(_, ops)| ops)
        .filter_map(|op| match op {
            WireOp::Req(_, filter) => Some(filter),
            WireOp::Close(_) => None,
        })
        .collect();
    assert!(reqs.iter().any(|filter| {
        filter.since == Some(100) && filter.until == Some(100) && filter.limit.is_none()
    }));
    assert!(reqs
        .iter()
        .any(|filter| filter.until == Some(99) && filter.limit == Some(5)));

    let stale = core.handle(EngineMsg::LoadOlder(id, first_continuation));
    assert!(stale.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Err(HistoryLoadError::StaleGeneration))
            if *session == id
    )));

    let third_effects = core.handle(EngineMsg::LoadOlder(
        id,
        second.continuation.clone().unwrap(),
    ));
    let (_, third) = returned(&third_effects);
    assert_eq!(third.load, HistoryLoadFact::Returned { added: 3 });
    apply(&mut rows, &third);
    assert_eq!(rows.len(), 13);
    assert_eq!(
        rows.keys().copied().collect::<BTreeSet<_>>(),
        events.iter().map(|e| e.id).collect()
    );
    core.handle(EngineMsg::CommitHistoryLoad(id));

    let at_bound = core.handle(EngineMsg::LoadOlder(
        id,
        third.continuation.clone().unwrap(),
    ));
    assert!(at_bound.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(session, Err(HistoryLoadError::AtBound { max_rows: 13 }))
            if *session == id
    )));
}

#[test]
fn continuation_is_engine_and_session_bound_and_cancel_releases_session() {
    let (mut first_core, keys, _, _) = seeded(3);
    let first_open = first_core.handle(EngineMsg::SubscribeHistory(
        query(&keys, 1, 3),
        Box::new(NullHistorySink),
    ));
    let (first_id, first_batch) = returned(&first_open);
    let token = first_batch.continuation.unwrap();

    let (mut other_core, other_keys, _, _) = seeded(2);
    let other_open = other_core.handle(EngineMsg::SubscribeHistory(
        query(&other_keys, 1, 2),
        Box::new(NullHistorySink),
    ));
    let (other_id, _) = returned(&other_open);
    let wrong_engine = other_core.handle(EngineMsg::LoadOlder(other_id, token.clone()));
    assert!(wrong_engine.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(_, Err(HistoryLoadError::WrongEngine))
    )));

    first_core.handle(EngineMsg::UnsubscribeHistory(first_id));
    let after_cancel = first_core.handle(EngineMsg::LoadOlder(first_id, token));
    assert!(after_cancel.iter().any(|effect| matches!(
        effect,
        Effect::HistoryLoadResult(_, Err(HistoryLoadError::WrongSession))
    )));
}
