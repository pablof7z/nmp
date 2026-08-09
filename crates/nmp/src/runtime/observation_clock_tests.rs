use std::collections::BTreeSet;
use std::sync::mpsc;
use std::time::Duration;

use nmp_grammar::{Binding, Demand, Filter, Freshness, LiveQuery};
use nmp_store::{EventStore, MemoryStore, RelayObserved};
use nostr::{Keys, Kind, RelayUrl, Timestamp};

use super::*;
use crate::core::HistoryQuery;
use crate::lane_fault_store::{FaultyLaneStore, LaneFaults};

fn query(author: &Keys, freshness: Freshness) -> LiveQuery {
    let mut demand = Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([Kind::TextNote.as_u16()])),
        authors: Some(Binding::Literal(BTreeSet::from([author
            .public_key()
            .to_hex()]))),
        ..Filter::default()
    });
    demand.freshness = freshness;
    LiveQuery::single(demand)
}

fn runtime(store: MemoryStore, faults: LaneFaults) -> (EngineThread, Handle) {
    EngineThread::spawn(
        FaultyLaneStore::new(store, faults),
        4,
        PoolConfig::default(),
        RelayAdmissionPolicy::default(),
    )
    .expect("engine construction")
}

/// #1344: opening an observation is not a maintenance event. This exact
/// cardinality reproduces the Mosaico-shaped workload that exposed one Redb
/// expiry/retry sweep per open. The two history opens cover the parallel
/// runtime command. Live and CacheOnly differ in wire ownership, but neither
/// has a reason to call `expire_due` when no deadline is due.
#[test]
fn many_live_and_cache_only_opens_run_zero_maintenance_sweeps() {
    let faults = LaneFaults::default();
    let (thread, handle) = runtime(MemoryStore::new(), faults.clone());
    let author = Keys::generate();
    let mut observations = Vec::new();

    for index in 0..207 {
        let freshness = if index % 2 == 0 {
            Freshness::Live
        } else {
            Freshness::CacheOnly
        };
        observations.push(
            handle
                .subscribe(query(&author, freshness))
                .expect("observation opens"),
        );
    }
    let history_observations = [Freshness::Live, Freshness::CacheOnly].map(|freshness| {
        handle
            .subscribe_history(HistoryQuery::new(query(&author, freshness), 1, 2))
            .expect("history observation opens")
    });

    assert_eq!(
        faults.maintenance_sweeps(),
        0,
        "opening observations with no due deadline must never call store.expire_due"
    );

    drop(observations);
    drop(history_observations);
    handle.shutdown();
    thread.join();
}

/// #1344: if a command and a core deadline become ready together, deadline
/// maintenance runs before the command. The probe blocks the engine inside
/// that command turn, so an implementation that defers Tick until the next
/// loop iteration cannot race its way to a false pass.
#[test]
fn due_deadline_runs_before_a_simultaneously_ready_command() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://deadline-race.example").unwrap();
    let base = Timestamp::from(Timestamp::now().as_secs().saturating_add(3_600));
    let expiry = Timestamp::from(base.as_secs().saturating_add(1));
    let event = nmp_resolver::testkit::expiring_kind1(
        &author,
        "deadline must win the command race",
        base.as_secs(),
        expiry.as_secs(),
    );
    let event_id = event.id;
    let mut store = MemoryStore::new();
    store
        .insert(event, RelayObserved::new(relay, base))
        .expect("seed expiring event");

    let faults = LaneFaults::default();
    let (thread, handle) = runtime(store, faults.clone());
    let clock = thread.clock();
    clock.set(base);
    let (_observation, rows) = handle
        .subscribe(query(&author, Freshness::CacheOnly))
        .expect("cache-only observation opens");
    let (initial, _, _) = rows
        .recv_timeout(Duration::from_secs(2))
        .expect("opening row frame");
    assert!(initial
        .iter()
        .any(|delta| { matches!(delta, RowDelta::Added(row) if row.event.id == event_id) }));

    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    handle
        .inbox
        .send(Cmd::DeadlineRaceProbe {
            at: expiry,
            entered: entered_tx,
            release: release_rx,
        })
        .expect("queue race probe");
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("engine entered the command turn");

    let (retracted, _, _) = rows
        .recv_timeout(Duration::from_millis(100))
        .expect("due expiration ran before the blocked command");
    assert!(retracted
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == event_id)));
    assert_eq!(
        faults.maintenance_sweeps(),
        2,
        "base Tick plus exact expiry Tick"
    );

    let _ = release_tx.send(());
    handle.shutdown();
    thread.join();
}

/// A harness-stated clock change already arrives as `EngineMsg::Tick`. The
/// command/deadline race guard must recognize that command as the maintenance
/// owner instead of running a second pre-command sweep at the same instant.
#[test]
fn explicit_tick_at_a_due_deadline_runs_maintenance_once() {
    let author = Keys::generate();
    let relay = RelayUrl::parse("wss://explicit-tick.example").unwrap();
    let base = Timestamp::from(Timestamp::now().as_secs().saturating_add(3_600));
    let expiry = Timestamp::from(base.as_secs().saturating_add(60));
    let event = nmp_resolver::testkit::expiring_kind1(
        &author,
        "one maintenance owner",
        base.as_secs(),
        expiry.as_secs(),
    );
    let event_id = event.id;
    let mut store = MemoryStore::new();
    store
        .insert(event, RelayObserved::new(relay, base))
        .expect("seed expiring event");

    let faults = LaneFaults::default();
    let (thread, handle) = runtime(store, faults.clone());
    let clock = thread.clock();
    clock.set(base);
    let (_observation, rows) = handle
        .subscribe(query(&author, Freshness::CacheOnly))
        .expect("cache-only observation opens");
    let _ = rows
        .recv_timeout(Duration::from_secs(2))
        .expect("opening row frame");

    clock.set(expiry);
    let (retracted, _, _) = rows
        .recv_timeout(Duration::from_secs(2))
        .expect("explicit due Tick retracts the row");
    assert!(retracted
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == event_id)));
    assert_eq!(
        faults.maintenance_sweeps(),
        2,
        "one initial clock Tick plus one exactly-due maintenance Tick"
    );

    handle.shutdown();
    thread.join();
}
