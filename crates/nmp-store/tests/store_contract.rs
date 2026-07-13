//! The `EventStore` contract suite — every test here runs against BOTH
//! `MemoryStore` (the oracle) and a fresh `RedbStore` backed by a temp file
//! (M3 step A1: "MemoryStore updated in lockstep as the oracle" + "run
//! against both backends"). Covers: M1's dedup/supersession semantics
//! (preserved unchanged), provenance merge (ledger #5, plan §5 test 8),
//! coverage record/get/merge (the Fable ruling), and claim-based GC with
//! watermark-lowering (plan §5 test 13).
//!
//! `persistence_roundtrip_events_and_coverage_survive_reopen` (plan §5 test
//! 12) is `RedbStore`-only — it specifically exercises closing and
//! reopening the same file, which `MemoryStore` has no equivalent of.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{AccessContext, ConcreteFilter, ContextualAtom, SourceAuthority};
use nmp_store::{
    coverage_key, sentinel_signature, AcceptWrite, ClaimSet, CoverageInterval, EventStore,
    InsertOutcome, IntentSigState, MemoryStore, Provenance, RedbStore, RefuseReason, RelayObserved,
    RetractReason, StoredEvent, WriteDurability,
};
use nostr::nips::nip01::Coordinate;
use nostr::{Event, EventBuilder, Filter, Keys, Kind, RelayUrl, Tag, Timestamp};

fn keys() -> Keys {
    Keys::generate()
}

fn relay(url: &str) -> RelayUrl {
    RelayUrl::parse(url).unwrap()
}

fn observed(url: &str, at: u64) -> RelayObserved {
    RelayObserved::new(relay(url), Timestamp::from(at))
}

/// The `StoredEvent` a single `insert(event, observed(url, at))` produces —
/// used to assert `Superseded { replaced }` hands back the FULL evicted row,
/// not just its id.
fn stored(event: Event, url: &str, at: u64) -> StoredEvent {
    let mut seen = BTreeMap::new();
    seen.insert(relay(url), Timestamp::from(at));
    StoredEvent {
        event,
        provenance: Provenance { seen, local: None },
    }
}

fn kind3_event(keys: &Keys, created_at: u64) -> Event {
    EventBuilder::new(Kind::ContactList, "")
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

fn addressable_event(keys: &Keys, kind: u16, d: &str, created_at: u64) -> Event {
    EventBuilder::new(Kind::from(kind), "")
        .tag(Tag::identifier(d))
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

fn regular_event(keys: &Keys, content: &str) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .sign_with_keys(keys)
        .unwrap()
}

fn regular_event_at(keys: &Keys, content: &str, created_at: u64) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

fn shape(kinds: &[u16], authors: Option<&Keys>) -> ConcreteFilter {
    ConcreteFilter {
        kinds: Some(kinds.iter().copied().collect()),
        authors: authors.map(|k| BTreeSet::from([k.public_key().to_hex()])),
        ids: None,
        tags: Default::default(),
        since: None,
        until: None,
        limit: None,
    }
}

/// Wrap a filter into a fixed-context (`AuthorOutboxes`/`Public`) demand
/// atom (#106): `coverage_key`/`record_coverage` now take a full
/// `ContextualAtom`, not a bare `ConcreteFilter` -- this contract suite
/// exercises the SELECTION/interval-merge axis, so every atom here shares a
/// uniform context (a dedicated context-anti-alias falsifier lives in
/// `nmp-store/src/coverage.rs`, closer to the identity it's proving).
fn atom(filter: &ConcreteFilter) -> ContextualAtom {
    ContextualAtom {
        filter: filter.clone(),
        source: SourceAuthority::AuthorOutboxes,
        access: AccessContext::Public,
    }
}

/// Run `body` against both backends: `MemoryStore` first (the oracle), then
/// a fresh `RedbStore` in its own throwaway temp file. Every shared contract
/// test goes through this so the two backends can never silently diverge.
fn for_each_backend(mut body: impl FnMut(&mut dyn EventStore)) {
    let mut mem = MemoryStore::new();
    body(&mut mem);

    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");
    let mut redb = RedbStore::open(&path).expect("open redb store");
    body(&mut redb);
}

// ---------------------------------------------------------------------
// M1 dedup/supersession contract (preserved, ported to the new signatures)
// ---------------------------------------------------------------------

#[test]
fn insert_batch_preserves_input_order_and_governed_supersession() {
    for_each_backend(|store| {
        let author = keys();
        let old = kind3_event(&author, 100);
        let newer = kind3_event(&author, 200);
        let outcomes = store
            .insert_batch(vec![
                (old.clone(), observed("wss://r1", 101)),
                (newer.clone(), observed("wss://r1", 201)),
                (newer.clone(), observed("wss://r2", 202)),
            ])
            .unwrap();

        assert_eq!(outcomes.len(), 3);
        assert!(matches!(outcomes[0], InsertOutcome::Inserted));
        assert!(matches!(
            &outcomes[1],
            InsertOutcome::Superseded { replaced } if replaced.event.id == old.id
        ));
        assert!(matches!(
            outcomes[2],
            InsertOutcome::Duplicate {
                provenance_grew: true,
                ..
            }
        ));
        let rows = store.query(&Filter::new().id(newer.id)).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].provenance.seen.len(), 2);
        assert!(store.query(&Filter::new().id(old.id)).unwrap().is_empty());
    });
}

#[test]
fn newest_created_at_wins_replaceable() {
    for_each_backend(|store| {
        let k = keys();

        let old = kind3_event(&k, 100);
        assert_eq!(
            store.insert(old.clone(), observed("wss://r1", 1)).unwrap(),
            InsertOutcome::Inserted
        );

        let newer = kind3_event(&k, 200);
        let newer_id = newer.id;
        assert_eq!(
            store.insert(newer, observed("wss://r1", 2)).unwrap(),
            InsertOutcome::Superseded {
                replaced: Box::new(stored(old, "wss://r1", 1))
            }
        );

        let results = store
            .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event.id, newer_id);
    });
}

#[test]
fn lexically_smallest_id_wins_on_created_at_tie() {
    for_each_backend(|store| {
        let k = keys();

        let mut candidates: Vec<Event> = (0..6)
            .map(|i| {
                EventBuilder::new(Kind::ContactList, "")
                    .tag(Tag::hashtag(format!("salt{i}")))
                    .custom_created_at(Timestamp::from(100u64))
                    .sign_with_keys(&k)
                    .unwrap()
            })
            .collect();
        candidates.sort_by_key(|e| e.id);

        let smallest = candidates[0].clone();
        let larger = candidates[1].clone();

        assert_eq!(
            store
                .insert(larger.clone(), observed("wss://r1", 1))
                .unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            store
                .insert(smallest.clone(), observed("wss://r1", 2))
                .unwrap(),
            InsertOutcome::Superseded {
                replaced: Box::new(stored(larger.clone(), "wss://r1", 1))
            }
        );

        let third = candidates[2].clone();
        assert_eq!(
            store.insert(third, observed("wss://r1", 3)).unwrap(),
            InsertOutcome::Stale
        );

        let results = store
            .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event.id, smallest.id);
    });
}

#[test]
fn stale_older_event_rejected() {
    for_each_backend(|store| {
        let k = keys();

        let newer = kind3_event(&k, 200);
        assert_eq!(
            store
                .insert(newer.clone(), observed("wss://r1", 1))
                .unwrap(),
            InsertOutcome::Inserted
        );

        let older = kind3_event(&k, 100);
        assert_eq!(
            store.insert(older, observed("wss://r1", 2)).unwrap(),
            InsertOutcome::Stale
        );

        let results = store
            .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event.id, newer.id);
    });
}

#[test]
fn replaceable_keyed_by_pubkey_kind_not_by_id_alone() {
    for_each_backend(|store| {
        let alice = keys();
        let bob = keys();

        assert_eq!(
            store
                .insert(kind3_event(&alice, 100), observed("wss://r1", 1))
                .unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            store
                .insert(kind3_event(&bob, 100), observed("wss://r1", 1))
                .unwrap(),
            InsertOutcome::Inserted
        );

        let results = store.query(&Filter::new().kind(Kind::ContactList)).unwrap();
        assert_eq!(results.len(), 2);
    });
}

#[test]
fn addressable_keyed_by_pubkey_kind_d_distinct_from_replaceable() {
    for_each_backend(|store| {
        let k = keys();

        let g1_old = addressable_event(&k, 30_003, "g1", 100);
        assert_eq!(
            store
                .insert(g1_old.clone(), observed("wss://r1", 1))
                .unwrap(),
            InsertOutcome::Inserted
        );

        let g2 = addressable_event(&k, 30_003, "g2", 100);
        assert_eq!(
            store.insert(g2.clone(), observed("wss://r1", 1)).unwrap(),
            InsertOutcome::Inserted
        );

        let g1_new = addressable_event(&k, 30_003, "g1", 200);
        let g1_new_id = g1_new.id;
        assert_eq!(
            store.insert(g1_new, observed("wss://r1", 2)).unwrap(),
            InsertOutcome::Superseded {
                replaced: Box::new(stored(g1_old, "wss://r1", 1))
            }
        );

        let mut results = store
            .query(
                &Filter::new()
                    .kind(Kind::from(30_003u16))
                    .author(k.public_key()),
            )
            .unwrap();
        results.sort_by_key(|se| se.event.id);
        let mut expected = vec![g1_new_id, g2.id];
        expected.sort();
        assert_eq!(
            results.iter().map(|se| se.event.id).collect::<Vec<_>>(),
            expected
        );
    });
}

#[test]
fn query_returns_only_current_winners_never_superseded() {
    for_each_backend(|store| {
        let k = keys();

        let old = kind3_event(&k, 100);
        let old_id = old.id;
        store.insert(old, observed("wss://r1", 1)).unwrap();
        let newer = kind3_event(&k, 200);
        store.insert(newer, observed("wss://r1", 2)).unwrap();

        let results = store.query(&Filter::new()).unwrap();
        assert!(!results.iter().any(|se| se.event.id == old_id));
    });
}

/// #124: `EventStore::query`'s own doc states the current contract
/// precisely -- `filter.limit` is NOT consulted locally, deliberately (see
/// that doc for why: honoring it requires an ordering decision reserved
/// for #9's Collection Tier-A gate). This pins that CURRENT contract
/// across both backends so it can never silently regress un-noticed, and
/// gives whoever resolves #9 an obvious test to flip once ordered/
/// truncated local reads are actually implemented.
#[test]
fn query_ignores_limit_and_returns_every_matching_row_on_both_backends() {
    for_each_backend(|store| {
        let k = keys();
        let matching_count = 5;
        for i in 0..matching_count {
            store
                .insert(
                    regular_event_at(&k, &format!("note {i}"), 100 + i),
                    observed("wss://r1", 1),
                )
                .unwrap();
        }

        let limited = Filter::new()
            .kind(Kind::TextNote)
            .author(k.public_key())
            .limit(2);
        let results = store.query(&limited).unwrap();
        assert_eq!(
            results.len(),
            matching_count as usize,
            "query must return every currently-matching row regardless of \
             filter.limit -- this is the CURRENT (deliberately undocumented-\
             no-longer, #124) contract, not a bug to silently fix here"
        );
    });
}

#[test]
fn query_newest_is_an_explicit_bounded_door_on_both_backends() {
    for_each_backend(|store| {
        let k = keys();
        for created_at in [100, 500, 300, 200, 400] {
            store
                .insert(
                    regular_event_at(&k, &format!("note {created_at}"), created_at),
                    observed("wss://r1", created_at),
                )
                .unwrap();
        }

        let filter = Filter::new().kind(Kind::TextNote).author(k.public_key());
        let rows = store.query_newest(&filter, 3).unwrap();
        assert_eq!(rows.len(), 3);
        assert_eq!(
            rows.iter()
                .map(|row| row.event.created_at.as_secs())
                .collect::<Vec<_>>(),
            vec![500, 400, 300]
        );
    });
}

// ---------------------------------------------------------------------
// Provenance merge (ledger #5, plan §5 test 8)
// ---------------------------------------------------------------------

#[test]
fn provenance_merges_across_relays() {
    for_each_backend(|store| {
        let k = keys();
        let e = regular_event(&k, "hello");

        assert_eq!(
            store.insert(e.clone(), observed("wss://a", 10)).unwrap(),
            InsertOutcome::Inserted
        );
        assert_eq!(
            store.insert(e, observed("wss://b", 20)).unwrap(),
            InsertOutcome::Duplicate {
                provenance_grew: true,
                satisfied_intents: Vec::new(),
            }
        );

        let results = store
            .query(&Filter::new().kind(Kind::TextNote).author(k.public_key()))
            .unwrap();
        assert_eq!(results.len(), 1);
        let seen = &results[0].provenance.seen;
        assert_eq!(seen.get(&relay("wss://a")), Some(&Timestamp::from(10u64)));
        assert_eq!(seen.get(&relay("wss://b")), Some(&Timestamp::from(20u64)));
    });
}

#[test]
fn provenance_does_not_grow_on_earlier_or_equal_redelivery_from_same_relay() {
    for_each_backend(|store| {
        let k = keys();
        let e = regular_event(&k, "hello");

        store.insert(e.clone(), observed("wss://a", 10)).unwrap();
        assert_eq!(
            store.insert(e.clone(), observed("wss://a", 10)).unwrap(),
            InsertOutcome::Duplicate {
                provenance_grew: false,
                satisfied_intents: Vec::new(),
            }
        );
        assert_eq!(
            store.insert(e.clone(), observed("wss://a", 5)).unwrap(),
            InsertOutcome::Duplicate {
                provenance_grew: false,
                satisfied_intents: Vec::new(),
            }
        );
        assert_eq!(
            store.insert(e, observed("wss://a", 15)).unwrap(),
            InsertOutcome::Duplicate {
                provenance_grew: true,
                satisfied_intents: Vec::new(),
            }
        );

        let results = store
            .query(&Filter::new().kind(Kind::TextNote).author(k.public_key()))
            .unwrap();
        assert_eq!(
            results[0].provenance.seen.get(&relay("wss://a")),
            Some(&Timestamp::from(15u64))
        );
    });
}

// ---------------------------------------------------------------------
// Coverage: record -> get, interval merge, refuse-the-floor, limit-poisons
// ---------------------------------------------------------------------

#[test]
fn record_coverage_then_get_coverage_roundtrip() {
    for_each_backend(|store| {
        let s = shape(&[1], None);
        let r = relay("wss://r1");
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
            )
            .unwrap();

        let key = coverage_key(&atom(&s));
        let interval = store.get_coverage(key, &r).expect("row should exist");
        assert_eq!(interval.from, Timestamp::from(0u64));
        assert_eq!(interval.through, Timestamp::from(100u64));
    });
}

#[test]
fn get_coverage_returns_none_when_no_row_recorded() {
    for_each_backend(|store| {
        let s = shape(&[1], None);
        let key = coverage_key(&atom(&s));
        assert!(store.get_coverage(key, &relay("wss://r1")).is_none());
    });
}

#[test]
fn coverage_key_is_window_erased_a_floored_refetch_finds_the_same_row() {
    for_each_backend(|store| {
        let unfloored = shape(&[1], None);
        let r = relay("wss://r1");
        store
            .record_coverage(
                &atom(&unfloored),
                &r,
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
            )
            .unwrap();

        // Same shape, `since` set (a floored refetch's atom) — must hash to
        // the SAME `CoverageKey` (ruling §1) and therefore find the same row.
        let floored = ConcreteFilter {
            since: Some(101),
            limit: Some(50),
            ..unfloored.clone()
        };
        let key = coverage_key(&atom(&floored));
        assert_eq!(key, coverage_key(&atom(&unfloored)));
        let interval = store
            .get_coverage(key, &r)
            .expect("same row, found via the floored atom's key");
        assert_eq!(interval.through, Timestamp::from(100u64));
    });
}

#[test]
fn limited_fetch_that_never_calls_record_coverage_leaves_get_coverage_none() {
    // The `limit` POISONS coverage unconditionally (ruling §3): a limited
    // REQ's EOSE proves only that the relay stopped, not that the window
    // was exhausted, so the caller (the engine reducer, out of this crate's
    // scope) must simply never call `record_coverage` for it. This pins
    // down the store's half of that contract: no call in => no row out.
    for_each_backend(|store| {
        let limited_shape = shape(&[1], None);
        let limited_shape = ConcreteFilter {
            limit: Some(500),
            ..limited_shape
        };
        // The engine never calls `record_coverage` here — that's the whole
        // point. We only assert the store's side: nothing was ever recorded.
        let key = coverage_key(&atom(&limited_shape));
        assert!(store.get_coverage(key, &relay("wss://r1")).is_none());
    });
}

#[test]
fn coverage_merge_extends_across_two_record_coverage_calls() {
    for_each_backend(|store| {
        let s = shape(&[1], None);
        let r = relay("wss://r1");
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
            )
            .unwrap();
        // Planner floors the next REQ at covered_through + 1 — the common
        // contiguous-extension path.
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(101u64), Timestamp::from(200u64)),
            )
            .unwrap();

        let key = coverage_key(&atom(&s));
        let interval = store.get_coverage(key, &r).unwrap();
        assert_eq!(interval.from, Timestamp::from(0u64));
        assert_eq!(interval.through, Timestamp::from(200u64));
    });
}

#[test]
fn coverage_merge_keeps_greater_through_on_disjoint_recording() {
    for_each_backend(|store| {
        let s = shape(&[1], None);
        let r = relay("wss://r1");
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(300u64), Timestamp::from(400u64)),
            )
            .unwrap();
        // A disjoint, strictly-older interval must never overwrite it.
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(50u64)),
            )
            .unwrap();

        let key = coverage_key(&atom(&s));
        let interval = store.get_coverage(key, &r).unwrap();
        assert_eq!(interval.from, Timestamp::from(300u64));
        assert_eq!(interval.through, Timestamp::from(400u64));
    });
}

// ---------------------------------------------------------------------
// Claim-based bounded GC + watermark-lowering (plan §5 test 13)
// ---------------------------------------------------------------------

#[test]
fn gc_evicts_unclaimed_regular_event_and_shrinks_covering_watermark() {
    for_each_backend(|store| {
        let k = keys();
        let e = regular_event_at(&k, "hello", 150);
        let e_id = e.id;
        store.insert(e, observed("wss://r1", 1)).unwrap();

        let s = shape(&[1], Some(&k));
        let r = relay("wss://r1");
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(300u64)),
            )
            .unwrap();

        let claims = ClaimSet::new(vec![]); // nothing is live-claimed
        let report = store.gc(&claims).unwrap();
        assert_eq!(report.events_evicted, 1);
        assert_eq!(report.coverage_rows_shrunk, 1);
        assert_eq!(report.coverage_rows_deleted, 0);

        let key = coverage_key(&atom(&s));
        let interval = store
            .get_coverage(key, &r)
            .expect("row should survive, shrunk");
        assert_eq!(interval.from, Timestamp::from(151u64));
        assert_eq!(interval.through, Timestamp::from(300u64));

        let results = store
            .query(&Filter::new().kind(Kind::TextNote).author(k.public_key()))
            .unwrap();
        assert!(!results.iter().any(|se| se.event.id == e_id));
    });
}

#[test]
fn gc_deletes_watermark_row_when_shrink_empties_it() {
    for_each_backend(|store| {
        let k = keys();
        let e = regular_event_at(&k, "hello", 100);
        store.insert(e, observed("wss://r1", 1)).unwrap();

        let s = shape(&[1], Some(&k));
        let r = relay("wss://r1");
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(100u64), Timestamp::from(100u64)),
            )
            .unwrap();

        let claims = ClaimSet::new(vec![]);
        let report = store.gc(&claims).unwrap();
        assert_eq!(report.events_evicted, 1);
        assert_eq!(report.coverage_rows_deleted, 1);
        assert_eq!(report.coverage_rows_shrunk, 0);

        let key = coverage_key(&atom(&s));
        assert!(store.get_coverage(key, &r).is_none());
    });
}

#[test]
fn gc_retains_claimed_event_and_replaceable_current_winner() {
    for_each_backend(|store| {
        let k = keys();

        // A regular event that IS claimed by a live query -> must survive.
        let claimed = regular_event_at(&k, "hello", 50);
        let claimed_id = claimed.id;
        store.insert(claimed, observed("wss://r1", 1)).unwrap();

        // A replaceable current winner -> must survive regardless of claims
        // (never a GC candidate at all).
        let winner = kind3_event(&k, 10);
        let winner_id = winner.id;
        store.insert(winner, observed("wss://r1", 1)).unwrap();

        let claims = ClaimSet::new(vec![shape(&[1], Some(&k))]);

        let report = store.gc(&claims).unwrap();
        assert_eq!(report.events_evicted, 0);

        let results = store.query(&Filter::new()).unwrap();
        assert!(results.iter().any(|se| se.event.id == claimed_id));
        assert!(results.iter().any(|se| se.event.id == winner_id));
    });
}

#[test]
fn gc_evicts_unclaimed_event_even_when_unrelated_claims_exist() {
    for_each_backend(|store| {
        let k = keys();
        let other = keys();
        let e = regular_event_at(&k, "hello", 50);
        let e_id = e.id;
        store.insert(e, observed("wss://r1", 1)).unwrap();

        // A claim for an unrelated author's kind:1 shape does not protect e.
        let claims = ClaimSet::new(vec![shape(&[1], Some(&other))]);

        let report = store.gc(&claims).unwrap();
        assert_eq!(report.events_evicted, 1);

        let results = store.query(&Filter::new()).unwrap();
        assert!(!results.iter().any(|se| se.event.id == e_id));
    });
}

// ---------------------------------------------------------------------
// Retraction: the store door goes symmetric (issue #25 / #23 §1.1) —
// `Superseded` hands back the full row, `remove` clears both indexes, and an
// already-expired event is `Refused` before it ever touches storage.
// ---------------------------------------------------------------------

#[test]
fn superseded_returns_the_full_evicted_row() {
    for_each_backend(|store| {
        let k = keys();

        let old = kind3_event(&k, 100);
        store.insert(old.clone(), observed("wss://r1", 1)).unwrap();
        // A second relay observes the same old event before it is
        // superseded -- its provenance must merge into the returned row too,
        // not just the event.
        store.insert(old.clone(), observed("wss://r2", 2)).unwrap();

        let newer = kind3_event(&k, 200);
        match store.insert(newer, observed("wss://r1", 3)).unwrap() {
            InsertOutcome::Superseded { replaced } => {
                assert_eq!(replaced.event, old);
                assert_eq!(
                    replaced.provenance.seen.get(&relay("wss://r1")),
                    Some(&Timestamp::from(1u64))
                );
                assert_eq!(
                    replaced.provenance.seen.get(&relay("wss://r2")),
                    Some(&Timestamp::from(2u64))
                );
            }
            other => panic!("expected Superseded, got {other:?}"),
        }
    });
}

#[test]
fn remove_returns_the_removed_row_and_clears_indexes() {
    for_each_backend(|store| {
        let k = keys();

        let e = kind3_event(&k, 100);
        let e_id = e.id;
        store.insert(e.clone(), observed("wss://r1", 1)).unwrap();

        let removed = store
            .remove(e_id, RetractReason::Deleted)
            .unwrap()
            .expect("the row was present");
        assert_eq!(removed.event, e);

        // Id index misses.
        assert!(store.query(&Filter::new().id(e_id)).unwrap().is_empty());

        // Address index misses too: if `remove` had left `addr_index`
        // pointing at the now-gone `e_id`, inserting a fresh event at the
        // SAME address (even an older `created_at`) would either panic
        // (memory store: `addr_index must always point at a stored event`)
        // or wrongly lose to a ghost winner. It must simply win as the
        // first event at a now-empty address.
        let fresh = kind3_event(&k, 50);
        let fresh_id = fresh.id;
        assert_eq!(
            store.insert(fresh, observed("wss://r1", 2)).unwrap(),
            InsertOutcome::Inserted
        );
        let results = store
            .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event.id, fresh_id);

        // Removing an id no longer held is a no-op `None`.
        assert!(store
            .remove(e_id, RetractReason::Deleted)
            .unwrap()
            .is_none());
    });
}

#[test]
fn refused_event_is_never_stored() {
    for_each_backend(|store| {
        let k = keys();

        let expired = EventBuilder::new(Kind::TextNote, "bye")
            .tag(Tag::expiration(Timestamp::from(100u64)))
            .sign_with_keys(&k)
            .unwrap();
        let expired_id = expired.id;

        // Observed well after its expiration deadline.
        assert_eq!(
            store.insert(expired, observed("wss://r1", 200)).unwrap(),
            InsertOutcome::Refused(RefuseReason::AlreadyExpired)
        );

        assert!(!store
            .query(&Filter::new())
            .unwrap()
            .iter()
            .any(|se| se.event.id == expired_id));
        assert!(store
            .remove(expired_id, RetractReason::Expired)
            .unwrap()
            .is_none());
    });
}

// ---------------------------------------------------------------------
// Persistence roundtrip (plan §5 test 12) — `RedbStore`-only: exercises
// closing and reopening the SAME file, which `MemoryStore` has no
// equivalent of.
// ---------------------------------------------------------------------

#[test]
fn persistence_roundtrip_events_and_coverage_survive_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");

    let k = keys();
    let old = kind3_event(&k, 100);
    let newer = kind3_event(&k, 200);
    let newer_id = newer.id;
    let regular = regular_event(&k, "hello");
    let regular_id = regular.id;

    let s = shape(&[1], Some(&k));
    let r = relay("wss://r1");
    let key = coverage_key(&atom(&s));

    {
        let mut store = RedbStore::open(&path).expect("open");
        store.insert(old, observed("wss://r1", 1)).unwrap();
        store.insert(newer, observed("wss://r1", 2)).unwrap();
        store.insert(regular, observed("wss://r1", 3)).unwrap();
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(150u64)),
            )
            .unwrap();
        // `store` dropped here, closing the database file.
    }

    let store = RedbStore::open(&path).expect("reopen");

    let contact_results = store
        .query(&Filter::new().kind(Kind::ContactList).author(k.public_key()))
        .unwrap();
    assert_eq!(contact_results.len(), 1, "current winner only survives");
    assert_eq!(contact_results[0].event.id, newer_id);

    let text_results = store
        .query(&Filter::new().kind(Kind::TextNote).author(k.public_key()))
        .unwrap();
    assert_eq!(text_results.len(), 1);
    assert_eq!(text_results[0].event.id, regular_id);

    let interval = store
        .get_coverage(key, &r)
        .expect("coverage survives reopen");
    assert_eq!(interval.from, Timestamp::from(0u64));
    assert_eq!(interval.through, Timestamp::from(150u64));
}

// ---------------------------------------------------------------------
// Retraction store-internals (issue #28,
// retraction-and-negative-deltas.md §2/§3.1/§7): kind:5 deletion +
// PERMANENT tombstones, and the persistent NIP-40 expiration index.
// ---------------------------------------------------------------------

fn deletion_event(keys: &Keys, targets: Vec<Tag>, created_at: u64) -> Event {
    EventBuilder::new(Kind::EventDeletion, "")
        .tags(targets)
        .custom_created_at(Timestamp::from(created_at))
        .sign_with_keys(keys)
        .unwrap()
}

fn expiring_event(keys: &Keys, content: &str, created_at: u64, expiration: u64) -> Event {
    EventBuilder::new(Kind::TextNote, content)
        .custom_created_at(Timestamp::from(created_at))
        .tag(Tag::expiration(Timestamp::from(expiration)))
        .sign_with_keys(keys)
        .unwrap()
}

#[test]
fn kind5_from_author_drops_held_target_and_returns_it() {
    for_each_backend(|store| {
        let k = keys();
        let target = regular_event_at(&k, "delete me", 100);
        let target_id = target.id;
        store
            .insert(target.clone(), observed("wss://r1", 1))
            .unwrap();

        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 200);
        let deletion_id = deletion.id;
        match store.insert(deletion, observed("wss://r1", 2)).unwrap() {
            InsertOutcome::Kind5Processed { deleted } => {
                assert_eq!(deleted.len(), 1);
                assert_eq!(deleted[0].event, target);
            }
            other => panic!("expected Kind5Processed, got {other:?}"),
        }

        assert!(store
            .query(&Filter::new().id(target_id))
            .unwrap()
            .is_empty());
        // The kind:5 event itself is stored normally, re-servable.
        assert_eq!(
            store.query(&Filter::new().id(deletion_id)).unwrap().len(),
            1
        );
    });
}

#[test]
fn kind5_from_non_author_does_not_delete() {
    for_each_backend(|store| {
        let author = keys();
        let attacker = keys();
        let target = regular_event_at(&author, "keep me", 100);
        let target_id = target.id;
        store
            .insert(target.clone(), observed("wss://r1", 1))
            .unwrap();

        let deletion = deletion_event(&attacker, vec![Tag::event(target_id)], 200);
        match store.insert(deletion, observed("wss://r1", 2)).unwrap() {
            InsertOutcome::Kind5Processed { deleted } => assert!(deleted.is_empty()),
            other => panic!("expected Kind5Processed, got {other:?}"),
        }

        let results = store.query(&Filter::new().id(target_id)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event, target);
    });
}

#[test]
fn tombstoned_event_is_refused_on_redelivery() {
    for_each_backend(|store| {
        let k = keys();
        let target = regular_event_at(&k, "delete me", 100);
        let target_id = target.id;
        store
            .insert(target.clone(), observed("wss://r1", 1))
            .unwrap();

        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 200);
        store.insert(deletion, observed("wss://r1", 2)).unwrap();

        // A relay replays the deleted event later -- never resurrected.
        assert_eq!(
            store.insert(target, observed("wss://r2", 3)).unwrap(),
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        );
        assert!(store
            .query(&Filter::new().id(target_id))
            .unwrap()
            .is_empty());
    });
}

#[test]
fn kind5_before_target_arrives_still_tombstones_then_refuses() {
    for_each_backend(|store| {
        let k = keys();
        let target = regular_event_at(&k, "delete me", 100);
        let target_id = target.id;

        // The deletion arrives BEFORE its target ever does -- arrival-order
        // independence.
        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 200);
        match store.insert(deletion, observed("wss://r1", 1)).unwrap() {
            InsertOutcome::Kind5Processed { deleted } => assert!(deleted.is_empty()),
            other => panic!("expected Kind5Processed, got {other:?}"),
        }

        assert_eq!(
            store.insert(target, observed("wss://r2", 2)).unwrap(),
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        );
        assert!(store
            .query(&Filter::new().id(target_id))
            .unwrap()
            .is_empty());
    });
}

#[test]
fn unauthorized_kind5_cannot_resurrect_authorized_deletion() {
    // The smoking-gun falsifier: id-tombstones must be keyed per claiming
    // author, never collapsed to one overwritable slot per id -- else an
    // unauthorized third party naming an already-deleted id can silently
    // undo the real author's permanent, authorized deletion.
    for_each_backend(|store| {
        let author = keys();
        let attacker = keys();
        let target = regular_event_at(&author, "delete me", 100);
        let target_id = target.id;
        store
            .insert(target.clone(), observed("wss://r1", 1))
            .unwrap();

        // The real author deletes it -- authorized, permanent.
        let real_deletion = deletion_event(&author, vec![Tag::event(target_id)], 200);
        match store
            .insert(real_deletion, observed("wss://r1", 2))
            .unwrap()
        {
            InsertOutcome::Kind5Processed { deleted } => assert_eq!(deleted.len(), 1),
            other => panic!("expected Kind5Processed, got {other:?}"),
        }
        assert!(store
            .query(&Filter::new().id(target_id))
            .unwrap()
            .is_empty());

        // An unrelated, unauthorized third party ALSO names the same id in
        // its own kind:5 -- structurally powerless (author-only), and must
        // not be able to overwrite or shadow the real author's claim.
        let attacker_deletion = deletion_event(&attacker, vec![Tag::event(target_id)], 300);
        match store
            .insert(attacker_deletion, observed("wss://r1", 3))
            .unwrap()
        {
            InsertOutcome::Kind5Processed { deleted } => assert!(deleted.is_empty()),
            other => panic!("expected Kind5Processed, got {other:?}"),
        }

        // The real author's authorized, permanent deletion must still
        // hold -- the attacker's claim must never resurrect it.
        assert_eq!(
            store.insert(target, observed("wss://r2", 4)).unwrap(),
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        );
        assert!(store
            .query(&Filter::new().id(target_id))
            .unwrap()
            .is_empty());
    });
}

#[test]
fn kind5_id_claims_are_independent_per_author() {
    // Positive companion to the falsifier above: distinct (id, author)
    // claims never interfere with each other in either direction.
    for_each_backend(|store| {
        let author = keys();
        let bystander = keys();

        // `bystander` deletes an id it actually authored -- authorized.
        let bystanders_own = regular_event_at(&bystander, "bystander's own", 50);
        let bystanders_own_id = bystanders_own.id;
        store
            .insert(bystanders_own.clone(), observed("wss://r1", 1))
            .unwrap();
        let bystanders_deletion =
            deletion_event(&bystander, vec![Tag::event(bystanders_own_id)], 60);
        store
            .insert(bystanders_deletion, observed("wss://r1", 1))
            .unwrap();

        // `bystander` ALSO (unauthorized) names `author`'s target id in a
        // separate kind:5 -- structurally powerless.
        let authors_target = regular_event_at(&author, "author's own", 100);
        let authors_target_id = authors_target.id;
        store
            .insert(authors_target.clone(), observed("wss://r1", 2))
            .unwrap();
        let unauthorized = deletion_event(&bystander, vec![Tag::event(authors_target_id)], 200);
        match store.insert(unauthorized, observed("wss://r1", 3)).unwrap() {
            InsertOutcome::Kind5Processed { deleted } => assert!(deleted.is_empty()),
            other => panic!("expected Kind5Processed, got {other:?}"),
        }

        // `author`'s own event, which `author` never deleted, is
        // unaffected by `bystander`'s unrelated, unauthorized claim on the
        // same id.
        let results = store.query(&Filter::new().id(authors_target_id)).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event, authors_target);

        // `bystander`'s own legitimate deletion is still correctly
        // tombstoned -- the fix didn't break the authorized case.
        assert!(store
            .query(&Filter::new().id(bystanders_own_id))
            .unwrap()
            .is_empty());
        assert_eq!(
            store
                .insert(bystanders_own, observed("wss://r2", 4))
                .unwrap(),
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        );
    });
}

#[test]
fn kind5_a_tag_deletes_addressable_target_and_ceiling_blocks_older_redelivery() {
    for_each_backend(|store| {
        let k = keys();
        let g1 = addressable_event(&k, 30_003, "g1", 100);
        let g1_id = g1.id;
        store.insert(g1.clone(), observed("wss://r1", 1)).unwrap();

        let coord = Coordinate::new(Kind::from(30_003u16), k.public_key()).identifier("g1");
        let deletion = deletion_event(&k, vec![Tag::coordinate(coord, None)], 200);
        match store.insert(deletion, observed("wss://r1", 2)).unwrap() {
            InsertOutcome::Kind5Processed { deleted } => {
                assert_eq!(deleted.len(), 1);
                assert_eq!(deleted[0].event, g1);
            }
            other => panic!("expected Kind5Processed, got {other:?}"),
        }
        assert!(store.query(&Filter::new().id(g1_id)).unwrap().is_empty());

        // Older-than-the-deletion-ceiling event at the same address: still
        // blocked -- the ceiling, not just the specific id, is tombstoned.
        let older_replay = addressable_event(&k, 30_003, "g1", 150);
        assert_eq!(
            store.insert(older_replay, observed("wss://r2", 3)).unwrap(),
            InsertOutcome::Refused(RefuseReason::Tombstoned)
        );

        // A genuinely NEW post-deletion event at the same address wins
        // normally -- NIP-09 does not permanently kill the address itself.
        let fresh = addressable_event(&k, 30_003, "g1", 250);
        let fresh_id = fresh.id;
        assert_eq!(
            store.insert(fresh, observed("wss://r2", 4)).unwrap(),
            InsertOutcome::Inserted
        );
        let results = store
            .query(
                &Filter::new()
                    .kind(Kind::from(30_003u16))
                    .author(k.public_key()),
            )
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].event.id, fresh_id);
    });
}

#[test]
fn tombstones_survive_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");

    let k = keys();
    let target = regular_event_at(&k, "delete me", 100);
    let target_id = target.id;

    {
        let mut store = RedbStore::open(&path).expect("open");
        store
            .insert(target.clone(), observed("wss://r1", 1))
            .unwrap();
        let deletion = deletion_event(&k, vec![Tag::event(target_id)], 200);
        store.insert(deletion, observed("wss://r1", 2)).unwrap();
        // `store` dropped here, closing the database file.
    }

    let mut store = RedbStore::open(&path).expect("reopen");
    assert_eq!(
        store.insert(target, observed("wss://r2", 3)).unwrap(),
        InsertOutcome::Refused(RefuseReason::Tombstoned)
    );
}

#[test]
fn expiration_index_drains_due_and_reports_next() {
    for_each_backend(|store| {
        let k = keys();
        let soon = expiring_event(&k, "soon", 1, 150);
        let soon_id = soon.id;
        let later = expiring_event(&k, "later", 1, 300);
        let later_id = later.id;

        store.insert(soon, observed("wss://r1", 1)).unwrap();
        store.insert(later, observed("wss://r1", 1)).unwrap();

        assert_eq!(store.next_expiration(), Some(Timestamp::from(150u64)));

        let due = store.expire_due(Timestamp::from(200u64)).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].event.id, soon_id);
        assert!(store.query(&Filter::new().id(soon_id)).unwrap().is_empty());

        assert_eq!(store.next_expiration(), Some(Timestamp::from(300u64)));
        let due2 = store.expire_due(Timestamp::from(300u64)).unwrap();
        assert_eq!(due2.len(), 1);
        assert_eq!(due2[0].event.id, later_id);
        assert_eq!(store.next_expiration(), None);
    });
}

#[test]
fn expired_events_retract_at_reopen() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("store.redb");

    let k = keys();
    let expiring = expiring_event(&k, "bye", 1, 150);
    let expiring_id = expiring.id;

    {
        let mut store = RedbStore::open(&path).expect("open");
        // Observed well BEFORE its own deadline -- inserted normally.
        assert_eq!(
            store.insert(expiring, observed("wss://r1", 1)).unwrap(),
            InsertOutcome::Inserted
        );
        // `store` dropped here; the deadline passes while "offline".
    }

    let mut store = RedbStore::open(&path).expect("reopen");
    assert_eq!(store.next_expiration(), Some(Timestamp::from(150u64)));
    let due = store.expire_due(Timestamp::from(200u64)).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].event.id, expiring_id);
    assert!(store
        .query(&Filter::new().id(expiring_id))
        .unwrap()
        .is_empty());
}

#[test]
fn coverage_is_bit_identical_across_all_retractions_and_only_gc_lowers_it() {
    for_each_backend(|store| {
        let k = keys();
        let r = relay("wss://r1");
        let s = shape(&[1, 3], Some(&k));

        let deleted_target = regular_event_at(&k, "delete me", 50);
        let deleted_target_id = deleted_target.id;
        let expiring_target = expiring_event(&k, "expire me", 60, 150);
        let old_replaceable = kind3_event(&k, 70);
        let new_replaceable = kind3_event(&k, 80);

        store
            .insert(deleted_target, observed("wss://r1", 1))
            .unwrap();
        store
            .insert(expiring_target, observed("wss://r1", 1))
            .unwrap();
        store
            .insert(old_replaceable, observed("wss://r1", 1))
            .unwrap();
        store
            .record_coverage(
                &atom(&s),
                &r,
                CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(300u64)),
            )
            .unwrap();

        let key = coverage_key(&atom(&s));
        let before = store.get_coverage(key, &r).expect("row exists");

        assert!(matches!(
            store
                .insert(new_replaceable, observed("wss://r1", 2))
                .unwrap(),
            InsertOutcome::Superseded { .. }
        ));
        assert_eq!(
            store.get_coverage(key, &r),
            Some(before),
            "supersession must not touch coverage"
        );

        let deletion = deletion_event(&k, vec![Tag::event(deleted_target_id)], 200);
        store.insert(deletion, observed("wss://r1", 2)).unwrap();
        let after_delete = store.get_coverage(key, &r).expect("row still exists");
        assert_eq!(before, after_delete, "delete must not touch coverage");

        store.expire_due(Timestamp::from(200u64)).unwrap();
        let after_expiry = store.get_coverage(key, &r).expect("row still exists");
        assert_eq!(before, after_expiry, "expiry must not touch coverage");

        let signed_pending = regular_event_at(&k, "cancel me", 220);
        let frozen_pending = Event::new(
            signed_pending.id,
            signed_pending.pubkey,
            signed_pending.created_at,
            signed_pending.kind,
            signed_pending.tags.clone(),
            signed_pending.content.clone(),
            sentinel_signature(),
        );
        let accepted = store
            .accept_write(AcceptWrite {
                frozen: frozen_pending,
                replaceable_base: None,
                expected_pubkey: k.public_key(),
                signing_identity_ref: "coverage-proof".into(),
                durability: WriteDurability::Durable,
                routing: "coverage-proof".into(),
                sig_state: IntentSigState::Pending,
                accepted_at: Timestamp::from(220u64),
            })
            .expect("accept pending row");
        store
            .compensate_write(accepted.journaled_intent_id().expect("pending intent"))
            .expect("compensate pending row");
        assert_eq!(
            store.get_coverage(key, &r),
            Some(before),
            "pre-signature termination must not touch coverage"
        );

        let gc_target = regular_event_at(&k, "gc me", 250);
        store.insert(gc_target, observed("wss://r1", 3)).unwrap();
        let report = store.gc(&ClaimSet::new(vec![])).unwrap();
        assert!(report.coverage_rows_shrunk + report.coverage_rows_deleted > 0);
        assert_ne!(
            store.get_coverage(key, &r),
            Some(before),
            "GC must remain the only operation in this matrix that lowers coverage"
        );
    });
}

// ---------------------------------------------------------------------
// Query indexing (issue #17): `RedbStore::query` narrows via BY_AUTHOR/
// BY_KIND instead of decoding every row in EVENTS. This is a correctness
// regression guard for that narrowing -- the row-count bound itself is
// `RedbStore`-internal instrumentation and lives in
// `redb_store.rs`'s own `#[cfg(test)] mod tests`
// (`query_by_author_does_not_scan_all_rows`).
// ---------------------------------------------------------------------

#[test]
fn query_returns_same_rows_after_indexing() {
    for_each_backend(|store| {
        let alice = keys();
        let bob = keys();

        let alice_note = regular_event_at(&alice, "hi", 100);
        let alice_note_id = alice_note.id;
        let bob_note = regular_event_at(&bob, "yo", 101);
        let bob_note_id = bob_note.id;
        let alice_profile = EventBuilder::new(Kind::Metadata, "{}")
            .custom_created_at(Timestamp::from(102u64))
            .sign_with_keys(&alice)
            .unwrap();
        let alice_profile_id = alice_profile.id;
        let alice_addressable = addressable_event(&alice, 30078, "app-data", 103);
        let alice_addressable_id = alice_addressable.id;

        store
            .insert(alice_note.clone(), observed("wss://r1", 1))
            .unwrap();
        store
            .insert(bob_note.clone(), observed("wss://r1", 1))
            .unwrap();
        store
            .insert(alice_profile, observed("wss://r1", 1))
            .unwrap();
        store
            .insert(alice_addressable, observed("wss://r1", 1))
            .unwrap();

        // Noise: other authors' kind:3 rows -- present in the table, but
        // must never surface in any assertion below (a stray leak would
        // mean the narrowing widened the candidate set, not just avoided
        // decoding it).
        for i in 0..20u64 {
            let noise = keys();
            store
                .insert(kind3_event(&noise, 200 + i), observed("wss://r1", 200 + i))
                .unwrap();
        }

        // by-id
        let by_id = store.query(&Filter::new().id(alice_note_id)).unwrap();
        assert_eq!(by_id.len(), 1);
        assert_eq!(by_id[0].event.id, alice_note_id);

        // by-author: every one of alice's rows, none of bob's or the noise.
        let mut by_author: Vec<_> = store
            .query(&Filter::new().author(alice.public_key()))
            .unwrap()
            .into_iter()
            .map(|se| se.event.id)
            .collect();
        by_author.sort();
        let mut expected_by_author = vec![alice_note_id, alice_profile_id, alice_addressable_id];
        expected_by_author.sort();
        assert_eq!(by_author, expected_by_author);

        // by-kind: both TextNotes, regardless of author; none of the
        // kind:3 noise or alice's own non-TextNote rows.
        let mut by_kind: Vec<_> = store
            .query(&Filter::new().kind(Kind::TextNote))
            .unwrap()
            .into_iter()
            .map(|se| se.event.id)
            .collect();
        by_kind.sort();
        let mut expected_by_kind = vec![alice_note_id, bob_note_id];
        expected_by_kind.sort();
        assert_eq!(by_kind, expected_by_kind);

        // author + kind intersection: only alice's TextNote.
        let combo = store
            .query(
                &Filter::new()
                    .author(alice.public_key())
                    .kind(Kind::TextNote),
            )
            .unwrap();
        assert_eq!(combo.len(), 1);
        assert_eq!(combo[0].event.id, alice_note_id);

        // by-address shape (kind + author + #d): the one addressable row,
        // matched via the same author/kind narrowing plus `match_event`'s
        // own tag check -- not a separate address-index path.
        let addr = store
            .query(
                &Filter::new()
                    .author(alice.public_key())
                    .kind(Kind::from(30078))
                    .identifier("app-data"),
            )
            .unwrap();
        assert_eq!(addr.len(), 1);
        assert_eq!(addr[0].event.id, alice_addressable_id);
    });
}
