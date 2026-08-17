use std::collections::BTreeSet;
use std::thread;
use std::time::{Duration, Instant};

use nmp_grammar::{Binding, Filter};
use nmp_store::{RedbStore, RelayObserved};
use nostr::{Keys, Kind};

use super::*;
use nmp_engine::core::{ShortfallFact, WindowLoad};

fn row(keys: &Keys, created_at: u64, content: &str) -> Row {
    Row::from_relay_event(
        UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(created_at),
            Kind::TextNote,
            Vec::new(),
            content,
        )
        .sign_with_keys(keys)
        .unwrap(),
        BTreeSet::new(),
    )
}

fn canonical(mut rows: Vec<Row>) -> Vec<Row> {
    rows.sort_by(|a, b| {
        b.created_at()
            .cmp(&a.created_at())
            .then_with(|| a.id().cmp(&b.id()))
    });
    rows
}

fn batch(rows: Vec<Row>) -> HistoryBatch {
    HistoryBatch {
        rows,
        deltas: Vec::new(),
        evidence: Vec::new(),
        load: WindowLoad::Idle,
    }
}

fn apply(rows: &mut BTreeMap<EventId, Row>, deltas: &[RowDelta]) {
    for delta in deltas {
        match delta {
            RowDelta::Added(row) => {
                rows.insert(row.id(), row.clone());
            }
            RowDelta::Updated(row) => {
                rows.insert(row.id(), row.clone());
            }
            RowDelta::SourcesGrew { id, sources } => {
                rows.get_mut(id).unwrap().sources = sources.clone();
            }
            RowDelta::Removed(id) => {
                rows.remove(id);
            }
        }
    }
}

#[test]
fn non_consuming_history_receiver_gets_one_latest_exact_bounded_state() {
    const MAX_ROWS: usize = 5;
    let keys = Keys::generate();
    let candidates: Vec<_> = (0..7)
        .map(|index| row(&keys, 100 + index, &format!("row-{index}")))
        .collect();
    let (tx, rx) = latest_channel();
    let rx = HistoryReceiver::new(rx);

    let first = canonical(vec![candidates[0].clone(), candidates[1].clone()]);
    tx.send(batch(first.clone()));
    let first_batch = rx.recv().unwrap();
    assert_eq!(first_batch.rows, first);
    let mut delivered = BTreeMap::new();
    apply(&mut delivered, &first_batch.deltas);

    let mut expected = Vec::new();
    for update in 0..10_000 {
        let omitted = update % candidates.len();
        expected = canonical(
            candidates
                .iter()
                .enumerate()
                .filter(|(index, _)| *index != omitted)
                .take(MAX_ROWS)
                .map(|(_, row)| row.clone())
                .collect(),
        );
        tx.send(batch(expected.clone()));
    }

    let latest = rx.recv().unwrap();
    assert_eq!(latest.rows, expected);
    assert!(latest.rows.len() <= MAX_ROWS);
    apply(&mut delivered, &latest.deltas);
    assert_eq!(
        delivered,
        expected
            .iter()
            .cloned()
            .map(|row| (row.id(), row))
            .collect()
    );
    assert_eq!(rx.delivered.borrow().len(), expected.len());
    assert!(
        matches!(
            rx.recv_timeout(Duration::from_millis(1)),
            Err(RecvTimeoutError::Timeout)
        ),
        "the 9,999 overwritten frames must not remain queued"
    );
}

#[test]
fn conflation_keeps_authoritative_rows_and_latest_metadata_with_exact_rebased_deltas() {
    fn assert_send<T: Send>() {}
    assert_send::<HistoryReceiver>();

    let keys = Keys::generate();
    let removed = row(&keys, 101, "removed");
    let mut provenance_grew = row(&keys, 100, "provenance");
    let added = row(&keys, 99, "added");
    let overwritten = row(&keys, 98, "overwritten");
    let relay = RelayUrl::parse("wss://history-latest.example").unwrap();
    let (tx, rx) = latest_channel();
    let rx = HistoryReceiver::new(rx);

    let initial_rows = canonical(vec![removed.clone(), provenance_grew.clone()]);
    tx.send(HistoryBatch {
        rows: initial_rows,
        deltas: Vec::new(),
        evidence: Vec::new(),
        load: WindowLoad::Idle,
    });
    let initial = rx.recv().unwrap();
    let mut delivered = BTreeMap::new();
    apply(&mut delivered, &initial.deltas);

    tx.send(HistoryBatch {
        rows: canonical(vec![provenance_grew.clone(), overwritten]),
        deltas: Vec::new(),
        evidence: Vec::new(),
        load: WindowLoad::Requesting,
    });

    provenance_grew.sources.insert(relay);
    let latest_rows = canonical(vec![provenance_grew.clone(), added.clone()]);
    let latest_evidence = vec![AcquisitionEvidence {
        sources: Vec::new(),
        shortfall: vec![ShortfallFact::NoResolvedDemand],
    }];
    tx.send(HistoryBatch {
        rows: latest_rows.clone(),
        deltas: Vec::new(),
        evidence: latest_evidence.clone(),
        load: WindowLoad::Returned { added: 1 },
    });

    let latest = rx.recv().unwrap();
    assert_eq!(latest.rows, latest_rows);
    assert_eq!(latest.evidence, latest_evidence);
    assert_eq!(latest.load, WindowLoad::Returned { added: 1 });
    assert!(latest
        .deltas
        .iter()
        .any(|delta| matches!(delta, RowDelta::Added(row) if row.id() == added.id())));
    assert!(latest.deltas.iter().any(|delta| matches!(
        delta,
        RowDelta::SourcesGrew { id, sources }
            if *id == provenance_grew.id() && *sources == provenance_grew.sources
    )));
    assert!(latest
        .deltas
        .iter()
        .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == removed.id())));
    assert_eq!(latest.deltas.len(), 3);
    apply(&mut delivered, &latest.deltas);
    assert_eq!(
        delivered,
        latest_rows.into_iter().map(|row| (row.id(), row)).collect()
    );
    assert!(matches!(
        rx.recv_timeout(Duration::from_millis(1)),
        Err(RecvTimeoutError::Timeout)
    ));
}

#[test]
fn closing_history_mailbox_wakes_blocked_receiver() {
    let (tx, rx) = latest_channel();
    let rx = HistoryReceiver::new(rx);
    let waiter = thread::spawn(move || rx.recv());
    thread::sleep(Duration::from_millis(20));
    drop(tx);
    assert!(waiter.join().unwrap().is_err());
}

#[test]
fn runtime_reply_drop_rolls_back_and_idle_cancel_and_shutdown_wake_receivers() {
    let _serial = RUNTIME_LIFECYCLE_TEST_LOCK.lock().unwrap();
    let keys = Keys::generate();
    let relay = RelayUrl::parse("wss://history-runtime.example").unwrap();
    let events: Vec<_> = (0..3)
        .map(|index| row(&keys, 100 + index, &format!("runtime-{index}")))
        .map(|row| row.signed_event().expect("fixture rows are signed"))
        .collect();
    let mut store = RedbStore::temporary().expect("temporary Redb store");
    for event in &events {
        store
            .insert(
                event.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(500)),
            )
            .unwrap();
    }
    let query = HistoryQuery::new(
        LiveQuery::single(
            nmp_grammar::Demand::author_outboxes(Filter {
                authors: Some(Binding::Literal(BTreeSet::from([keys
                    .public_key()
                    .to_hex()]))),
                kinds: Some(BTreeSet::from([1])),
                ..Filter::default()
            })
            .expect("the selection binds `authors`"),
        ),
        1,
        3,
    );
    let (engine_thread, handle) = EngineThread::spawn(store, 4, PoolConfig::default()).unwrap();

    let (history_handle, receiver) = handle.subscribe_history(query.clone()).unwrap();
    receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    // A request whose reply receiver is already dropped stages, fails to
    // reply, and rolls back — leaving the window exactly as before.
    let (reply, dropped_reply) = mpsc::channel();
    drop(dropped_reply);
    handle
        .inbox
        .send(Cmd::RequestRows {
            id: history_handle.0,
            at_least: 2,
            reply,
        })
        .unwrap();
    handle
        .request_rows(history_handle, 2)
        .expect("engine thread alive")
        .expect("the same request must retry after reply-drop rollback");
    let deadline = Instant::now() + Duration::from_secs(1);
    let loaded = loop {
        let batch = receiver
            .recv_timeout(deadline.saturating_duration_since(Instant::now()))
            .unwrap();
        if matches!(batch.load, WindowLoad::Returned { .. }) {
            break batch;
        }
    };
    assert_eq!(loaded.rows.len(), 2);
    assert_eq!(loaded.load, WindowLoad::Returned { added: 1 });

    let (idle_ready, idle_started) = mpsc::channel();
    let (idle_result, idle_done) = mpsc::channel();
    let idle_waiter = thread::spawn(move || {
        idle_ready.send(()).unwrap();
        idle_result.send(receiver.recv().is_err()).unwrap();
    });
    idle_started.recv().unwrap();
    handle.unsubscribe_history(history_handle);
    assert!(idle_done.recv_timeout(Duration::from_secs(1)).unwrap());
    idle_waiter.join().unwrap();

    let (_shutdown_handle, shutdown_receiver) = handle.subscribe_history(query).unwrap();
    shutdown_receiver
        .recv_timeout(Duration::from_secs(1))
        .unwrap();
    let (shutdown_ready, shutdown_started) = mpsc::channel();
    let (shutdown_result, shutdown_done) = mpsc::channel();
    let shutdown_waiter = thread::spawn(move || {
        shutdown_ready.send(()).unwrap();
        shutdown_result
            .send(shutdown_receiver.recv().is_err())
            .unwrap();
    });
    shutdown_started.recv().unwrap();
    handle.shutdown();
    engine_thread.join();
    assert!(shutdown_done.recv_timeout(Duration::from_secs(1)).unwrap());
    shutdown_waiter.join().unwrap();
}
