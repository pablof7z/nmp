use std::collections::BTreeSet;

use nmp_resolver::testkit::accept_write_of;
use nmp_resolver::Engine;
use nmp_store::{AcceptOutcome, CompensateOutcome, EventStore, MemoryStore, RelayObserved};
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

#[test]
fn local_supersession_and_compensation_carry_exact_inverse_row_changes() {
    let keys = Keys::generate();
    let predecessor = event(&keys, Kind::from(10_000u16), "old", 10);
    let winner = event(&keys, Kind::from(10_000u16), "new", 20);
    let mut engine = Engine::new(MemoryStore::new());
    engine
        .accept_local_detailed(accept_write_of(predecessor.clone(), 11))
        .unwrap();

    let accepted = engine
        .accept_local_detailed(accept_write_of(winner, 21))
        .unwrap();
    let (intent_id, pending) = match &accepted.outcome {
        AcceptOutcome::Superseded { intent_id, row, .. } => (*intent_id, row.event.clone()),
        other => panic!("expected local supersession, got {other:?}"),
    };
    assert_eq!(accepted.committed.row_changes.inserted.len(), 1);
    assert_eq!(
        accepted.committed.row_changes.inserted[0].event.id,
        pending.id
    );
    assert!(accepted.committed.row_changes.inserted[0]
        .observed_relays
        .is_empty());
    assert_eq!(accepted.committed.row_changes.removed.len(), 1);
    assert_eq!(accepted.committed.row_changes.removed[0].id, predecessor.id);

    let outcome = engine.store_mut().compensate_write(intent_id).unwrap();
    assert!(matches!(outcome, CompensateOutcome::Compensated { .. }));
    let compensated = engine
        .react_to_compensation_detailed(pending.clone(), &outcome)
        .unwrap();
    assert_eq!(compensated.row_changes.inserted.len(), 1);
    assert_eq!(compensated.row_changes.inserted[0].event.id, predecessor.id);
    assert!(compensated.row_changes.inserted[0]
        .observed_relays
        .is_empty());
    assert_eq!(compensated.row_changes.removed, vec![pending]);
}

#[test]
fn local_kind5_compensation_carries_exact_revealed_target() {
    let keys = Keys::generate();
    let target = event(&keys, Kind::TextNote, "target", 10);
    let deletion = EventBuilder::new(Kind::EventDeletion, "")
        .tag(Tag::event(target.id))
        .custom_created_at(Timestamp::from(20u64))
        .sign_with_keys(&keys)
        .unwrap();
    let mut engine = Engine::new(MemoryStore::new());
    engine
        .accept_local_detailed(accept_write_of(target.clone(), 11))
        .unwrap();

    let accepted = engine
        .accept_local_detailed(accept_write_of(deletion, 21))
        .unwrap();
    let (intent_id, pending) = match &accepted.outcome {
        AcceptOutcome::Kind5Processed { intent_id, row, .. } => (*intent_id, row.event.clone()),
        other => panic!("expected local kind5, got {other:?}"),
    };
    assert_eq!(accepted.committed.row_changes.inserted.len(), 1);
    assert_eq!(
        accepted.committed.row_changes.inserted[0].event.id,
        pending.id
    );
    assert_eq!(accepted.committed.row_changes.removed.len(), 1);
    assert_eq!(accepted.committed.row_changes.removed[0].id, target.id);

    let outcome = engine.store_mut().compensate_write(intent_id).unwrap();
    let compensated = engine
        .react_to_compensation_detailed(pending.clone(), &outcome)
        .unwrap();
    assert_eq!(compensated.row_changes.inserted.len(), 1);
    assert_eq!(compensated.row_changes.inserted[0].event.id, target.id);
    assert_eq!(compensated.row_changes.removed, vec![pending]);
}

#[test]
fn expiry_retraction_carries_the_exact_removed_row() {
    let keys = Keys::generate();
    let expiring = EventBuilder::new(Kind::TextNote, "expires")
        .tag(Tag::expiration(Timestamp::from(100u64)))
        .custom_created_at(Timestamp::from(50u64))
        .sign_with_keys(&keys)
        .unwrap();
    let source = relay("expiry");
    let mut engine = Engine::new(MemoryStore::new());
    engine
        .ingest_observed_detailed(vec![(expiring.clone(), observed(source, 51))])
        .unwrap();

    let expired = engine
        .store_mut()
        .expire_due(Timestamp::from(100u64))
        .unwrap();
    let removed: Vec<_> = expired.into_iter().map(|row| row.event).collect();
    let retracted = engine.retract_detailed(removed).unwrap();

    assert!(retracted.row_changes.inserted.is_empty());
    assert_eq!(retracted.row_changes.removed, vec![expiring]);
    assert!(retracted.row_changes.provenance_grew.is_empty());
}

#[test]
fn local_duplicate_stale_and_refused_outcomes_carry_no_phantom_row_changes() {
    let keys = Keys::generate();
    let mut engine = Engine::new(MemoryStore::new());

    let ordinary = event(&keys, Kind::TextNote, "ordinary", 10);
    engine
        .accept_local_detailed(accept_write_of(ordinary.clone(), 11))
        .unwrap();
    let duplicate = engine
        .accept_local_detailed(accept_write_of(ordinary, 12))
        .unwrap();
    assert!(matches!(duplicate.outcome, AcceptOutcome::Duplicate { .. }));
    assert!(duplicate.committed.delta.is_empty());
    assert!(duplicate.committed.affected_handles.is_empty());
    assert!(duplicate.committed.row_changes.inserted.is_empty());
    assert!(duplicate.committed.row_changes.removed.is_empty());
    assert!(duplicate.committed.row_changes.provenance_grew.is_empty());

    let winner = event(&keys, Kind::from(10_000u16), "winner", 30);
    let loser = event(&keys, Kind::from(10_000u16), "loser", 20);
    engine
        .accept_local_detailed(accept_write_of(winner, 31))
        .unwrap();
    let stale = engine
        .accept_local_detailed(accept_write_of(loser, 32))
        .unwrap();
    assert!(matches!(stale.outcome, AcceptOutcome::Stale { .. }));
    assert!(stale.committed.delta.is_empty());
    assert!(stale.committed.affected_handles.is_empty());
    assert!(stale.committed.row_changes.inserted.is_empty());
    assert!(stale.committed.row_changes.removed.is_empty());
    assert!(stale.committed.row_changes.provenance_grew.is_empty());

    let expired = EventBuilder::new(Kind::TextNote, "expired")
        .tag(Tag::expiration(Timestamp::from(40u64)))
        .custom_created_at(Timestamp::from(39u64))
        .sign_with_keys(&keys)
        .unwrap();
    let refused = engine
        .accept_local_detailed(accept_write_of(expired, 41))
        .unwrap();
    assert!(matches!(refused.outcome, AcceptOutcome::Refused(_)));
    assert!(refused.committed.delta.is_empty());
    assert!(refused.committed.affected_handles.is_empty());
    assert!(refused.committed.row_changes.inserted.is_empty());
    assert!(refused.committed.row_changes.removed.is_empty());
    assert!(refused.committed.row_changes.provenance_grew.is_empty());
}
