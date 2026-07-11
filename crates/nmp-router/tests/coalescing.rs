//! M2 contract tests 10, 11, 13 (`docs/plans/M2-compiler-router-plan.md`
//! §4.2, §4.3, §5) — the widen-only property test per `MergeRule`, the
//! local-refilter exactness property, and the non-widening-rule drop
//! mechanism.

use std::collections::BTreeSet;

use nostr::filter::MatchEventOptions;
use nostr::{EventBuilder, Keys, Kind};
use proptest::prelude::*;

use nmp_grammar::ConcreteFilter;
use nmp_router::{deliver, AuthorUnion, DiscardSecondOperand, KindUnion, MergeRule, RuleRegistry};

fn matches(cf: &ConcreteFilter, e: &nostr::Event) -> bool {
    cf.to_nostr().match_event(e, MatchEventOptions::new())
}

fn small_kind() -> impl Strategy<Value = u16> {
    prop_oneof![Just(1u16), Just(2u16), Just(3u16)]
}

/// Test 10: `merge_rule_widens` for `AuthorUnion` -- the load-bearing rule.
/// Generator: same kind for `a`/`b` (so `try_merge` fires whenever the
/// author subsets differ), authors + events drawn from a small pool so
/// collisions are frequent.
#[test]
fn merge_rule_widens_author_union() {
    let pool: Vec<Keys> = (0..4).map(|_| Keys::generate()).collect();
    let pool_hex: Vec<String> = pool.iter().map(|k| k.public_key().to_hex()).collect();
    let n = pool.len();

    proptest!(|(
        kind in small_kind(),
        authors_a in prop::collection::btree_set(0..n, 1..=2),
        authors_b in prop::collection::btree_set(0..n, 1..=2),
        events in prop::collection::vec((small_kind(), 0..n), 0..6)
    )| {
        let a = ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(authors_a.iter().map(|&i| pool_hex[i].clone()).collect()),
            ..ConcreteFilter::default()
        };
        let b = ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(authors_b.iter().map(|&i| pool_hex[i].clone()).collect()),
            ..ConcreteFilter::default()
        };
        let evs: Vec<nostr::Event> = events
            .into_iter()
            .map(|(k, author_idx)| {
                EventBuilder::new(Kind::from(k), "")
                    .sign_with_keys(&pool[author_idx])
                    .expect("test fixture event must sign cleanly")
            })
            .collect();

        if let Some(m) = AuthorUnion.try_merge(&a, &b) {
            for e in &evs {
                if matches(&a, e) || matches(&b, e) {
                    prop_assert!(matches(&m, e));
                }
            }
        }
    });
}

/// Test 10: `merge_rule_widens` for `KindUnion` -- the optional rule. Same
/// structure, roles swapped: authors fixed (so `try_merge` fires whenever
/// the kind sets differ).
#[test]
fn merge_rule_widens_kind_union() {
    let pool: Vec<Keys> = (0..4).map(|_| Keys::generate()).collect();
    let pool_hex: Vec<String> = pool.iter().map(|k| k.public_key().to_hex()).collect();
    let n = pool.len();

    proptest!(|(
        author_idx in 0..n,
        kind_a in small_kind(),
        kind_b in small_kind(),
        events in prop::collection::vec((small_kind(), 0..n), 0..6)
    )| {
        let author = pool_hex[author_idx].clone();
        let a = ConcreteFilter {
            kinds: Some(BTreeSet::from([kind_a])),
            authors: Some(BTreeSet::from([author.clone()])),
            ..ConcreteFilter::default()
        };
        let b = ConcreteFilter {
            kinds: Some(BTreeSet::from([kind_b])),
            authors: Some(BTreeSet::from([author])),
            ..ConcreteFilter::default()
        };
        let evs: Vec<nostr::Event> = events
            .into_iter()
            .map(|(k, ai)| {
                EventBuilder::new(Kind::from(k), "")
                    .sign_with_keys(&pool[ai])
                    .expect("test fixture event must sign cleanly")
            })
            .collect();

        if let Some(m) = KindUnion.try_merge(&a, &b) {
            for e in &evs {
                if matches(&a, e) || matches(&b, e) {
                    prop_assert!(matches(&m, e));
                }
            }
        }
    });
}

/// Test 11: `local_refilter_is_exact` -- ties widen-only + the local
/// re-filter together end to end. `AuthorUnion`-merges atom X (author A)
/// and atom Y (author B) into wire filter M; a relay serving M would return
/// every event in the universe matching M (a strict superset of X's own
/// matches, since M widens). `deliver(wire_events, X)` must recover EXACTLY
/// the events X's own filter matches out of the full universe -- no
/// over-delivery (B's-only events excluded) and no under-delivery (every
/// A event present).
#[test]
fn local_refilter_is_exact() {
    let pool: Vec<Keys> = (0..4).map(|_| Keys::generate()).collect();
    let pool_hex: Vec<String> = pool.iter().map(|k| k.public_key().to_hex()).collect();

    proptest!(|(
        kind in small_kind(),
        author_x in 0..pool.len(),
        author_y in 0..pool.len(),
        events in prop::collection::vec((small_kind(), 0..pool.len()), 0..8)
    )| {
        prop_assume!(author_x != author_y);
        let x = ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(BTreeSet::from([pool_hex[author_x].clone()])),
            ..ConcreteFilter::default()
        };
        let y = ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(BTreeSet::from([pool_hex[author_y].clone()])),
            ..ConcreteFilter::default()
        };
        let merged = AuthorUnion
            .try_merge(&x, &y)
            .expect("same kind, different single author -- must merge");

        let universe: Vec<nostr::Event> = events
            .into_iter()
            .map(|(k, ai)| {
                EventBuilder::new(Kind::from(k), "")
                    .sign_with_keys(&pool[ai])
                    .expect("test fixture event must sign cleanly")
            })
            .collect();

        // "What a relay returns for the wire filter M."
        let wire_events: Vec<nostr::Event> = universe
            .iter()
            .filter(|e| matches(&merged, e))
            .cloned()
            .collect();

        let delivered_to_x: BTreeSet<nostr::EventId> =
            deliver(&wire_events, &x).into_iter().map(|e| e.id).collect();
        let expected_x: BTreeSet<nostr::EventId> = universe
            .iter()
            .filter(|e| matches(&x, e))
            .map(|e| e.id)
            .collect();
        prop_assert_eq!(delivered_to_x, expected_x);
    });
}

/// The relay-truncation falsifier the original widen-only property test
/// could never catch (ledger's own admitted gap): `matches()`/`match_event`
/// is a per-event PREDICATE, so it cannot express "a relay only returns the
/// first `limit` rows" -- a merged filter can satisfy the per-event
/// widening property and STILL under-fetch once a real relay truncates the
/// result count. The actual fix is structural (exclude any limited filter
/// from the union rules), so this property test checks the structural
/// invariant directly rather than trying to model truncation: for ANY pair
/// where at least one side carries a `limit`, `AuthorUnion`/`KindUnion` must
/// refuse to merge, full stop -- regardless of kind/author overlap.
#[test]
fn union_rules_never_merge_a_filter_that_carries_a_limit() {
    let pool: Vec<Keys> = (0..4).map(|_| Keys::generate()).collect();
    let pool_hex: Vec<String> = pool.iter().map(|k| k.public_key().to_hex()).collect();
    let n = pool.len();

    proptest!(|(
        kind_a in small_kind(),
        kind_b in small_kind(),
        authors_a in prop::collection::btree_set(0..n, 1..=2),
        authors_b in prop::collection::btree_set(0..n, 1..=2),
        limit_a in prop::option::of(1usize..500),
        limit_b in prop::option::of(1usize..500),
    )| {
        prop_assume!(limit_a.is_some() || limit_b.is_some());
        let a = ConcreteFilter {
            kinds: Some(BTreeSet::from([kind_a])),
            authors: Some(authors_a.iter().map(|&i| pool_hex[i].clone()).collect()),
            limit: limit_a,
            ..ConcreteFilter::default()
        };
        let b = ConcreteFilter {
            kinds: Some(BTreeSet::from([kind_b])),
            authors: Some(authors_b.iter().map(|&i| pool_hex[i].clone()).collect()),
            limit: limit_b,
            ..ConcreteFilter::default()
        };
        prop_assert!(AuthorUnion.try_merge(&a, &b).is_none());
        prop_assert!(KindUnion.try_merge(&a, &b).is_none());
    });
}

/// Test 13: `non_widening_rule_is_dropped_and_ships_separately`.
#[test]
fn non_widening_rule_is_dropped_and_ships_separately() {
    let registry =
        RuleRegistry::default_widen_only().register(Box::new(DiscardSecondOperand), false);
    assert_eq!(registry.dropped_rules(), &["DiscardSecondOperand"]);

    // Same `kinds`, different `since` -- outside AuthorUnion/KindUnion's
    // domain, but squarely inside DiscardSecondOperand's unsound
    // applicability predicate.
    let a = ConcreteFilter {
        kinds: Some(BTreeSet::from([1u16])),
        since: Some(100),
        ..ConcreteFilter::default()
    };
    let b = ConcreteFilter {
        kinds: Some(BTreeSet::from([1u16])),
        since: Some(200),
        ..ConcreteFilter::default()
    };

    // Sanity: the dropped rule really would have applied here, had it been
    // active.
    assert!(DiscardSecondOperand.try_merge(&a, &b).is_some());

    let out = registry.coalesce(BTreeSet::from([a, b]));
    assert_eq!(
        out.len(),
        2,
        "dropped rule must not fire -- both ship separately"
    );
}
