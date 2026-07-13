use std::collections::BTreeSet;

use nmp_resolver::Engine;
use nmp_store::{MemoryStore, RelayObserved};
use nostr::{Event, EventBuilder, Keys, Kind, RelayUrl, Tag, Timestamp};

fn relay(name: &str) -> RelayUrl {
    RelayUrl::parse(&format!("wss://{name}.committed-delta.example")).unwrap()
}

fn observed(relay: RelayUrl, at: u64) -> RelayObserved {
    RelayObserved::new(relay, Timestamp::from(at))
}

fn event(keys: &Keys, kind: Kind, content: &str, created_at: u64) -> Event {
    EventBuilder::new(kind, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

#[test]
fn inserted_row_carries_every_same_batch_source_and_later_growth_carries_only_new_sources() {
    let keys = Keys::generate();
    let event = event(&keys, Kind::TextNote, "one", 10);
    let first = relay("first");
    let second = relay("second");
    let third = relay("third");
    let fourth = relay("fourth");
    let mut engine = Engine::new(MemoryStore::new());

    let inserted = engine
        .ingest_observed_detailed(vec![
            (event.clone(), observed(first.clone(), 11)),
            (event.clone(), observed(second.clone(), 12)),
        ])
        .unwrap()
        .row_changes;
    assert_eq!(inserted.inserted.len(), 1);
    assert_eq!(inserted.inserted[0].event.id, event.id);
    assert_eq!(
        inserted.inserted[0].observed_relays,
        BTreeSet::from([first, second])
    );
    assert!(inserted.removed.is_empty());
    assert!(inserted.provenance_grew.is_empty());

    let grew = engine
        .ingest_observed_detailed(vec![
            (event.clone(), observed(third.clone(), 13)),
            (event.clone(), observed(fourth.clone(), 14)),
        ])
        .unwrap()
        .row_changes;
    assert!(grew.inserted.is_empty());
    assert!(grew.removed.is_empty());
    assert_eq!(grew.provenance_grew.len(), 1);
    assert_eq!(grew.provenance_grew[0].event.id, event.id);
    assert_eq!(
        grew.provenance_grew[0].observed_relays,
        BTreeSet::from([third, fourth])
    );
}

#[test]
fn same_batch_insert_then_delete_reports_only_the_durable_deletion_row() {
    let keys = Keys::generate();
    let target = event(&keys, Kind::TextNote, "transient", 10);
    let deletion = EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::event(target.id))
        .custom_created_at(Timestamp::from(20u64))
        .sign_with_keys(&keys)
        .unwrap();
    let source = relay("delete");
    let mut engine = Engine::new(MemoryStore::new());

    let changes = engine
        .ingest_observed_detailed(vec![
            (target, observed(source.clone(), 11)),
            (deletion.clone(), observed(source.clone(), 21)),
        ])
        .unwrap()
        .row_changes;

    assert_eq!(changes.inserted.len(), 1);
    assert_eq!(changes.inserted[0].event.id, deletion.id);
    assert_eq!(
        changes.inserted[0].observed_relays,
        BTreeSet::from([source])
    );
    assert!(changes.removed.is_empty());
    assert!(changes.provenance_grew.is_empty());
}

#[test]
fn same_batch_supersession_chain_collapses_to_old_removed_and_final_winner_inserted() {
    let keys = Keys::generate();
    let old = event(&keys, Kind::from(10_000u16), "old", 10);
    let middle = event(&keys, Kind::from(10_000u16), "middle", 20);
    let winner = event(&keys, Kind::from(10_000u16), "winner", 30);
    let source = relay("replaceable");
    let mut engine = Engine::new(MemoryStore::new());
    engine
        .ingest_observed_detailed(vec![(old.clone(), observed(source.clone(), 11))])
        .unwrap();

    let changes = engine
        .ingest_observed_detailed(vec![
            (middle, observed(source.clone(), 21)),
            (winner.clone(), observed(source.clone(), 31)),
        ])
        .unwrap()
        .row_changes;

    assert_eq!(changes.inserted.len(), 1);
    assert_eq!(changes.inserted[0].event.id, winner.id);
    assert_eq!(changes.removed, vec![old]);
    assert!(changes.provenance_grew.is_empty());
}
