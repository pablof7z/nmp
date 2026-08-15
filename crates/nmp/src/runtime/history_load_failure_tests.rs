//! Store-load failures surfacing through the RUNTIME as typed refusals.
//!
//! These two falsifiers spawn a real `EngineThread` and assert what an app
//! sees: a typed `ObservationUnavailable`, no leaked ownership, and no panic
//! when shutdown races the refusal. They lived under `core/` and reached back
//! up through `crate::runtime::*` to do it, which was the only
//! `core -> runtime` edge in the crate (#1142 boundary cleanup).
//!
//! Their core-level siblings — the ones that drive `EngineCore` and the
//! resolver directly and need core-private methods such as
//! `flush_wire_admission` — correctly stayed in
//! `core/history_load_failure_tests.rs`. Moving those here would have forced
//! core internals to `pub(crate)` purely to serve tests.

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Derived, Filter, LiveQuery, Selector};
use nmp_store::{testing, RedbStore, RelayObserved};
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Timestamp};

use crate::core::HistoryQuery;
use crate::runtime::{EngineThread, EngineThreadError, ObservationOwnershipCensus};

fn canonical_corruption(kind: u16, filename: &str) -> (tempfile::TempDir, std::path::PathBuf) {
    let directory = tempfile::tempdir().expect("canonical corruption directory");
    let path = directory.path().join(filename);
    let corrupt_id = {
        let keys = Keys::generate();
        let corrupt = event(&keys, kind, 1_000);
        let corrupt_id = corrupt.id;
        let mut store = RedbStore::open(&path).expect("create persistent Redb fixture");
        store
            .insert(
                corrupt,
                RelayObserved::new(
                    RelayUrl::parse("wss://canonical-corruption.example").unwrap(),
                    Timestamp::from(1_001u64),
                ),
            )
            .expect("seed canonical event");
        corrupt_id
    };
    testing::corrupt_canonical_event(&path, corrupt_id)
        .expect("store-owned canonical-event corruption");
    (directory, path)
}

fn event(keys: &Keys, kind: u16, created_at: u64) -> Event {
    EventBuilder::new(Kind::from(kind), format!("row-{kind}-{created_at}"))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

#[test]
fn observation_open_failures_are_typed_leak_free_and_leave_runtime_usable() {
    let (_directory, path) = canonical_corruption(1, "observation-open-corruption.redb");
    let store = RedbStore::open(&path).expect("reopen corrupted Redb fixture");
    let (engine, handle) = EngineThread::spawn(store, 4, nmp_transport::PoolConfig::default())
        .expect("runtime starts over targeted canonical corruption");
    assert_eq!(
        handle.observation_ownership_census(),
        ObservationOwnershipCensus::default()
    );

    let ordinary = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([1])),
        ..Filter::default()
    });
    assert!(matches!(
        handle.subscribe(ordinary),
        Err(EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("decode canonical event view")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        ObservationOwnershipCensus::default(),
        "post-handle ordinary projection refusal must roll back every owner"
    );
    let healthy = LiveQuery::from_filter(Filter {
        kinds: Some(BTreeSet::from([2])),
        ..Filter::default()
    });
    let (ordinary_handle, ordinary_rows) = handle.subscribe(healthy.clone()).expect(
        "a disjoint healthy ordinary filter proves corruption is targeted and runtime survived",
    );
    ordinary_rows
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("healthy empty ordinary query still receives its initial frame");
    handle.unsubscribe(ordinary_handle);
    assert_eq!(
        handle.observation_ownership_census(),
        ObservationOwnershipCensus::default()
    );

    let history = HistoryQuery::new(
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1])),
            ..Filter::default()
        }),
        1,
        2,
    );
    assert!(matches!(
        handle.subscribe_history(history),
        Err(EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("decode canonical event view")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        ObservationOwnershipCensus::default(),
        "post-handle history projection refusal must roll back every owner"
    );
    let (history_handle, history_rows) = handle
        .subscribe_history(HistoryQuery::new(healthy, 1, 2))
        .expect("the same disjoint filter remains usable through history");
    history_rows
        .recv_timeout(std::time::Duration::from_secs(1))
        .expect("healthy empty history query still receives its initial frame");
    handle.unsubscribe_history(history_handle);
    assert_eq!(
        handle.observation_ownership_census(),
        ObservationOwnershipCensus::default()
    );

    let derived = LiveQuery::from_filter(Filter {
        authors: Some(Binding::Derived(Box::new(Derived {
            inner: nmp_grammar::Demand::from_filter(Filter {
                kinds: Some(BTreeSet::from([1])),
                ..Filter::default()
            }),
            project: Selector::Tag("p".to_owned()),
        }))),
        kinds: Some(BTreeSet::from([4])),
        ..Filter::default()
    });
    assert!(matches!(
        handle.subscribe(derived.clone()),
        Err(EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("decode canonical event view")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        ObservationOwnershipCensus::default(),
        "pre-handle ordinary refusal must discard partial resolver nodes"
    );

    assert!(matches!(
        handle.subscribe_history(HistoryQuery::new(derived, 1, 2)),
        Err(EngineThreadError::ObservationUnavailable { reason })
            if reason.contains("decode canonical event view")
    ));
    assert_eq!(
        handle.observation_ownership_census(),
        ObservationOwnershipCensus::default(),
        "pre-handle history refusal must discard partial resolver nodes"
    );

    handle.shutdown();
    engine.join();
}

#[test]
fn shutdown_queued_during_each_refusal_keeps_the_typed_reply_and_never_panics() {
    {
        let (_directory, path) = canonical_corruption(1, "ordinary-shutdown-race-corruption.redb");
        let (store, blocked) = RedbStore::open_with_ordered_event_read_pause(&path)
            .expect("reopen corrupted Redb fixture with one ordered-read pause");
        let (engine, handle) =
            EngineThread::spawn(store, 4, nmp_transport::PoolConfig::default()).unwrap();
        let caller_handle = handle.clone();
        let caller = std::thread::spawn(move || {
            caller_handle.subscribe(LiveQuery::from_filter(Filter {
                kinds: Some(BTreeSet::from([1])),
                ..Filter::default()
            }))
        });
        blocked.wait_until_entered();
        handle.shutdown();
        blocked.release();
        assert!(matches!(
            caller.join().expect("ordinary caller must not panic"),
            Err(EngineThreadError::ObservationUnavailable { reason })
                if reason.contains("decode canonical event view")
        ));
        engine.join();
    }

    {
        let (_directory, path) = canonical_corruption(2, "history-shutdown-race-corruption.redb");
        let (store, blocked) = RedbStore::open_with_ordered_event_read_pause(&path)
            .expect("reopen corrupted Redb fixture with one ordered-read pause");
        let (engine, handle) =
            EngineThread::spawn(store, 4, nmp_transport::PoolConfig::default()).unwrap();
        let caller_handle = handle.clone();
        let caller = std::thread::spawn(move || {
            caller_handle.subscribe_history(HistoryQuery::new(
                LiveQuery::from_filter(Filter {
                    kinds: Some(BTreeSet::from([2])),
                    ..Filter::default()
                }),
                1,
                2,
            ))
        });
        blocked.wait_until_entered();
        handle.shutdown();
        blocked.release();
        assert!(matches!(
            caller.join().expect("history caller must not panic"),
            Err(EngineThreadError::ObservationUnavailable { reason })
                if reason.contains("decode canonical event view")
        ));
        engine.join();
    }
}
