//! Issue #459: the production runtime retains verified durable history by
//! default. Bounded query projection is a resident-memory concern, never an
//! implicit durable-store eviction policy.

use std::collections::{BTreeSet, HashSet};
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use nmp_engine::core::{RelayAdmissionPolicy, RowDelta};
use nmp_engine::runtime::{EngineThread, RowsMsg};
use nmp_grammar::{
    AccessContext, Binding, ConcreteFilter, ContextualAtom, Filter as QueryFilter, SourceAuthority,
};
use nmp_resolver::LiveQuery;
use nmp_router::FixtureDirectory;
use nmp_store::{coverage_key, CoverageInterval, EventStore, RedbStore, RelayObserved};
use nmp_transport::PoolConfig;
use nostr::{EventBuilder, EventId, Filter, Keys, Kind, RelayUrl, Timestamp};

const HISTORY_LEN: usize = 128;
const VISIBLE_LIMIT: usize = 8;

fn limited_query(author_hex: String) -> LiveQuery {
    LiveQuery::from_filter(QueryFilter {
        kinds: Some(BTreeSet::from([1])),
        authors: Some(Binding::Literal(BTreeSet::from([author_hex]))),
        limit: Some(VISIBLE_LIMIT),
        ..QueryFilter::default()
    })
}

fn receive_current_ids(rx: &Receiver<RowsMsg>) -> BTreeSet<EventId> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut current = BTreeSet::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        assert!(!remaining.is_zero(), "timed out waiting for cached rows");
        let (deltas, _) = rx
            .recv_timeout(remaining)
            .expect("bounded cached query must emit");
        for delta in deltas {
            match delta {
                RowDelta::Added(row) => {
                    current.insert(row.event.id);
                }
                RowDelta::Removed(id) => {
                    current.remove(&id);
                }
                RowDelta::SourcesGrew { .. } => {}
            }
        }
        if current.len() == VISIBLE_LIMIT {
            return current;
        }
    }
}

#[test]
fn bounded_runtime_working_sets_do_not_delete_default_durable_history() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("durable-history.redb");
    let keys = Keys::generate();
    let author_hex = keys.public_key().to_hex();
    let relay = RelayUrl::parse("wss://history.example").expect("relay URL");
    let mut all_ids = Vec::with_capacity(HISTORY_LEN);

    let atom = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            authors: Some(BTreeSet::from([author_hex.clone()])),
            ids: None,
            tags: Default::default(),
            since: None,
            until: None,
            limit: None,
        },
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
        routing_evidence: BTreeSet::new(),
    };
    let coverage = CoverageInterval::new(Timestamp::from(100u64), Timestamp::from(227u64));

    {
        let mut store = RedbStore::open(&path).expect("create production store");
        for offset in 0..HISTORY_LEN {
            let event = EventBuilder::new(Kind::TextNote, format!("history #{offset}"))
                .custom_created_at(Timestamp::from(100 + offset as u64))
                .sign_with_keys(&keys)
                .expect("sign fixture event");
            all_ids.push(event.id);
            store
                .insert(
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(300 + offset as u64)),
                )
                .expect("persist verified history");
        }
        store
            .record_coverage(&atom, &relay, coverage)
            .expect("persist acquisition evidence");
    }

    let expected_visible: BTreeSet<EventId> =
        all_ids.iter().rev().take(VISIBLE_LIMIT).copied().collect();

    // Exercise the real engine lifecycle and repeatedly allocate/release a
    // bounded result working set. These limits are intentionally much smaller
    // than the durable history; no runtime path may reinterpret them as a
    // store-retention request.
    {
        let store = RedbStore::open(&path).expect("ordinary engine startup");
        let (engine_thread, handle) = EngineThread::spawn(
            store,
            FixtureDirectory::new(),
            10,
            PoolConfig::default(),
            RelayAdmissionPolicy::default(),
        )
        .expect("spawn production runtime");

        for _ in 0..24 {
            let (query_handle, rows) = handle
                .subscribe(limited_query(author_hex.clone()))
                .expect("open bounded query");
            assert_eq!(receive_current_ids(&rows), expected_visible);
            handle.unsubscribe(query_handle);
        }

        handle.shutdown();
        engine_thread.join();
    }

    // A fresh store process is the durable truth check. Startup, bounded
    // projection churn, ordinary idle maintenance, shutdown, and reopen must
    // leave every row and its coverage evidence intact unless explicit GC ran.
    let reopened = RedbStore::open(&path).expect("reopen durable store");
    let rows = reopened
        .query(&Filter::new().kind(Kind::TextNote).author(keys.public_key()))
        .expect("read full retained history");
    let retained: HashSet<EventId> = rows.into_iter().map(|row| row.event.id).collect();
    assert_eq!(retained.len(), HISTORY_LEN);
    assert_eq!(retained, all_ids.into_iter().collect());
    assert_eq!(
        reopened.get_coverage(coverage_key(&atom), &relay),
        Some(coverage),
        "ordinary runtime pressure must not lower durable acquisition evidence"
    );
}
