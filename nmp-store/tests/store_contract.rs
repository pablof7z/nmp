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

use std::collections::BTreeSet;

use nmp_grammar::ConcreteFilter;
use nmp_store::{
    coverage_key, ClaimSet, CoverageInterval, EventStore, InsertOutcome, MemoryStore, RedbStore,
    RelayObserved,
};
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
fn newest_created_at_wins_replaceable() {
    for_each_backend(|store| {
        let k = keys();

        let old = kind3_event(&k, 100);
        let old_id = old.id;
        assert_eq!(
            store.insert(old, observed("wss://r1", 1)),
            InsertOutcome::Inserted
        );

        let newer = kind3_event(&k, 200);
        let newer_id = newer.id;
        assert_eq!(
            store.insert(newer, observed("wss://r1", 2)),
            InsertOutcome::Superseded { replaced: old_id }
        );

        let results = store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
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
            store.insert(larger.clone(), observed("wss://r1", 1)),
            InsertOutcome::Inserted
        );
        assert_eq!(
            store.insert(smallest.clone(), observed("wss://r1", 2)),
            InsertOutcome::Superseded {
                replaced: larger.id
            }
        );

        let third = candidates[2].clone();
        assert_eq!(
            store.insert(third, observed("wss://r1", 3)),
            InsertOutcome::Stale
        );

        let results = store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
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
            store.insert(newer.clone(), observed("wss://r1", 1)),
            InsertOutcome::Inserted
        );

        let older = kind3_event(&k, 100);
        assert_eq!(
            store.insert(older, observed("wss://r1", 2)),
            InsertOutcome::Stale
        );

        let results = store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
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
            store.insert(kind3_event(&alice, 100), observed("wss://r1", 1)),
            InsertOutcome::Inserted
        );
        assert_eq!(
            store.insert(kind3_event(&bob, 100), observed("wss://r1", 1)),
            InsertOutcome::Inserted
        );

        let results = store.query(&Filter::new().kind(Kind::ContactList));
        assert_eq!(results.len(), 2);
    });
}

#[test]
fn addressable_keyed_by_pubkey_kind_d_distinct_from_replaceable() {
    for_each_backend(|store| {
        let k = keys();

        let g1_old = addressable_event(&k, 30_003, "g1", 100);
        let g1_old_id = g1_old.id;
        assert_eq!(
            store.insert(g1_old, observed("wss://r1", 1)),
            InsertOutcome::Inserted
        );

        let g2 = addressable_event(&k, 30_003, "g2", 100);
        assert_eq!(
            store.insert(g2.clone(), observed("wss://r1", 1)),
            InsertOutcome::Inserted
        );

        let g1_new = addressable_event(&k, 30_003, "g1", 200);
        let g1_new_id = g1_new.id;
        assert_eq!(
            store.insert(g1_new, observed("wss://r1", 2)),
            InsertOutcome::Superseded {
                replaced: g1_old_id
            }
        );

        let mut results = store.query(
            &Filter::new()
                .kind(Kind::from(30_003u16))
                .author(k.public_key()),
        );
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
        store.insert(old, observed("wss://r1", 1));
        let newer = kind3_event(&k, 200);
        store.insert(newer, observed("wss://r1", 2));

        let results = store.query(&Filter::new());
        assert!(!results.iter().any(|se| se.event.id == old_id));
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
            store.insert(e.clone(), observed("wss://a", 10)),
            InsertOutcome::Inserted
        );
        assert_eq!(
            store.insert(e, observed("wss://b", 20)),
            InsertOutcome::Duplicate {
                provenance_grew: true
            }
        );

        let results = store.query(&Filter::new().kind(Kind::TextNote).author(k.public_key()));
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

        store.insert(e.clone(), observed("wss://a", 10));
        assert_eq!(
            store.insert(e.clone(), observed("wss://a", 10)),
            InsertOutcome::Duplicate {
                provenance_grew: false
            }
        );
        assert_eq!(
            store.insert(e.clone(), observed("wss://a", 5)),
            InsertOutcome::Duplicate {
                provenance_grew: false
            }
        );
        assert_eq!(
            store.insert(e, observed("wss://a", 15)),
            InsertOutcome::Duplicate {
                provenance_grew: true
            }
        );

        let results = store.query(&Filter::new().kind(Kind::TextNote).author(k.public_key()));
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
        store.record_coverage(
            &s,
            &r,
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
        );

        let key = coverage_key(&s);
        let interval = store.get_coverage(key, &r).expect("row should exist");
        assert_eq!(interval.from, Timestamp::from(0u64));
        assert_eq!(interval.through, Timestamp::from(100u64));
    });
}

#[test]
fn get_coverage_returns_none_when_no_row_recorded() {
    for_each_backend(|store| {
        let s = shape(&[1], None);
        let key = coverage_key(&s);
        assert!(store.get_coverage(key, &relay("wss://r1")).is_none());
    });
}

#[test]
fn coverage_key_is_window_erased_a_floored_refetch_finds_the_same_row() {
    for_each_backend(|store| {
        let unfloored = shape(&[1], None);
        let r = relay("wss://r1");
        store.record_coverage(
            &unfloored,
            &r,
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
        );

        // Same shape, `since` set (a floored refetch's atom) — must hash to
        // the SAME `CoverageKey` (ruling §1) and therefore find the same row.
        let floored = ConcreteFilter {
            since: Some(101),
            limit: Some(50),
            ..unfloored.clone()
        };
        let key = coverage_key(&floored);
        assert_eq!(key, coverage_key(&unfloored));
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
        let key = coverage_key(&limited_shape);
        assert!(store.get_coverage(key, &relay("wss://r1")).is_none());
    });
}

#[test]
fn coverage_merge_extends_across_two_record_coverage_calls() {
    for_each_backend(|store| {
        let s = shape(&[1], None);
        let r = relay("wss://r1");
        store.record_coverage(
            &s,
            &r,
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(100u64)),
        );
        // Planner floors the next REQ at covered_through + 1 — the common
        // contiguous-extension path.
        store.record_coverage(
            &s,
            &r,
            CoverageInterval::new(Timestamp::from(101u64), Timestamp::from(200u64)),
        );

        let key = coverage_key(&s);
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
        store.record_coverage(
            &s,
            &r,
            CoverageInterval::new(Timestamp::from(300u64), Timestamp::from(400u64)),
        );
        // A disjoint, strictly-older interval must never overwrite it.
        store.record_coverage(
            &s,
            &r,
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(50u64)),
        );

        let key = coverage_key(&s);
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
        store.insert(e, observed("wss://r1", 1));

        let s = shape(&[1], Some(&k));
        let r = relay("wss://r1");
        store.record_coverage(
            &s,
            &r,
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(300u64)),
        );

        let claims = ClaimSet::new(vec![]); // nothing is live-claimed
        let report = store.gc(&claims);
        assert_eq!(report.events_evicted, 1);
        assert_eq!(report.coverage_rows_shrunk, 1);
        assert_eq!(report.coverage_rows_deleted, 0);

        let key = coverage_key(&s);
        let interval = store
            .get_coverage(key, &r)
            .expect("row should survive, shrunk");
        assert_eq!(interval.from, Timestamp::from(151u64));
        assert_eq!(interval.through, Timestamp::from(300u64));

        let results = store.query(&Filter::new().kind(Kind::TextNote).author(k.public_key()));
        assert!(!results.iter().any(|se| se.event.id == e_id));
    });
}

#[test]
fn gc_deletes_watermark_row_when_shrink_empties_it() {
    for_each_backend(|store| {
        let k = keys();
        let e = regular_event_at(&k, "hello", 100);
        store.insert(e, observed("wss://r1", 1));

        let s = shape(&[1], Some(&k));
        let r = relay("wss://r1");
        store.record_coverage(
            &s,
            &r,
            CoverageInterval::new(Timestamp::from(100u64), Timestamp::from(100u64)),
        );

        let claims = ClaimSet::new(vec![]);
        let report = store.gc(&claims);
        assert_eq!(report.events_evicted, 1);
        assert_eq!(report.coverage_rows_deleted, 1);
        assert_eq!(report.coverage_rows_shrunk, 0);

        let key = coverage_key(&s);
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
        store.insert(claimed, observed("wss://r1", 1));

        // A replaceable current winner -> must survive regardless of claims
        // (never a GC candidate at all).
        let winner = kind3_event(&k, 10);
        let winner_id = winner.id;
        store.insert(winner, observed("wss://r1", 1));

        let claims = ClaimSet::new(vec![shape(&[1], Some(&k))]);

        let report = store.gc(&claims);
        assert_eq!(report.events_evicted, 0);

        let results = store.query(&Filter::new());
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
        store.insert(e, observed("wss://r1", 1));

        // A claim for an unrelated author's kind:1 shape does not protect e.
        let claims = ClaimSet::new(vec![shape(&[1], Some(&other))]);

        let report = store.gc(&claims);
        assert_eq!(report.events_evicted, 1);

        let results = store.query(&Filter::new());
        assert!(!results.iter().any(|se| se.event.id == e_id));
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
    let key = coverage_key(&s);

    {
        let mut store = RedbStore::open(&path).expect("open");
        store.insert(old, observed("wss://r1", 1));
        store.insert(newer, observed("wss://r1", 2));
        store.insert(regular, observed("wss://r1", 3));
        store.record_coverage(
            &s,
            &r,
            CoverageInterval::new(Timestamp::from(0u64), Timestamp::from(150u64)),
        );
        // `store` dropped here, closing the database file.
    }

    let store = RedbStore::open(&path).expect("reopen");

    let contact_results =
        store.query(&Filter::new().kind(Kind::ContactList).author(k.public_key()));
    assert_eq!(contact_results.len(), 1, "current winner only survives");
    assert_eq!(contact_results[0].event.id, newer_id);

    let text_results = store.query(&Filter::new().kind(Kind::TextNote).author(k.public_key()));
    assert_eq!(text_results.len(), 1);
    assert_eq!(text_results[0].event.id, regular_id);

    let interval = store
        .get_coverage(key, &r)
        .expect("coverage survives reopen");
    assert_eq!(interval.from, Timestamp::from(0u64));
    assert_eq!(interval.through, Timestamp::from(150u64));
}
