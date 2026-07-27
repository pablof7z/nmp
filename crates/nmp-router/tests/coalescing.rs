//! M2 contract tests 10, 11, 13 (`docs/plans/M2-compiler-router-plan.md`
//! §4.2, §4.3, §5) — the widen-only property test per `MergeRule`, the
//! local-refilter exactness property, and the non-widening-rule drop
//! mechanism.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use nostr::filter::MatchEventOptions;
use nostr::{EventBuilder, Keys, Kind, Tag};
use proptest::prelude::*;

use nmp_grammar::{ConcreteFilter, IndexedTagName};
use nmp_router::{
    deliver, AuthorUnion, DiscardSecondOperand, IdUnion, KindUnion, MergeRule, RuleRegistry,
};

fn matches(cf: &ConcreteFilter, e: &nostr::Event) -> bool {
    cf.to_nostr().match_event(e, MatchEventOptions::new())
}

fn small_kind() -> impl Strategy<Value = u16> {
    prop_oneof![Just(1u16), Just(2u16), Just(3u16)]
}

// ---------------------------------------------------------------------------
// Component-shape generators (#900).
//
// The bug #900 reports survived a green `merge_rule_widens_*` suite for one
// reason: every pre-existing generator built BOTH operands with
// `authors: Some(non-empty)` / `kinds: Some(non-empty)` / `ids:
// Some(non-empty)` and identical (always EMPTY) `tags`. The shapes it could
// never produce are exactly the ones that break the widening contract:
//
//   * `None` on a set-valued component -- UNCONSTRAINED, i.e. matches EVERY
//     value on that axis. A union rule that folds `None` into a `Some` set
//     NARROWS (that is #900).
//   * `Some(∅)` -- which `nostr`'s `match_event` also treats as
//     unconstrained (measured, not assumed: `Filter { authors: Some({}) }`
//     matches every event), so folding it into a `Some` set narrows the
//     same way.
//   * tag components: disjoint tag-NAME sets, and one name present on one
//     side and absent on the other. No rule unions tags TODAY (all three
//     require `a.tags == b.tags`), so these pairs are currently refused --
//     but the generator must produce them anyway, because they are the same
//     class of trap on an axis whose union rule does not exist yet.
//
// These helpers are deliberately RULE-AGNOSTIC: they describe the shape
// space of a `ConcreteFilter`, not the domain of any one rule, so they keep
// working when the AuthorUnion/KindUnion/IdUnion trio is replaced by a
// single structural "exactly one array component differs" rule.
// ---------------------------------------------------------------------------

/// Kind values shared by the generated filters and the event universe.
const KIND_POOL: [u16; 3] = [1, 2, 3];
/// Indexed tag NAMES the generators draw from. Two distinct names is the
/// minimum that expresses both "present on one side, absent on the other"
/// and "disjoint name sets".
const TAG_NAMES: [char; 2] = ['t', 'd'];
/// Tag VALUES, shared by the generated filters and the event universe.
const TAG_VALUES: [&str; 2] = ["alpha", "beta"];

/// Every shape a set-valued `ConcreteFilter` component can take, as pool
/// indices: `None` (absent => unconstrained), `Some(∅)` (present but empty),
/// and `Some(non-empty)` (the only genuinely constraining shape). The first
/// two are what the pre-#900 generators could not emit.
fn component_shape(pool: usize, max: usize) -> impl Strategy<Value = Option<BTreeSet<usize>>> {
    prop_oneof![
        2 => Just(None),
        1 => Just(Some(BTreeSet::new())),
        5 => prop::collection::btree_set(0..pool, 1..=max).prop_map(Some),
    ]
}

/// The tag component's shape space: which NAMES are constrained at all (none,
/// one, or both), and what value set each carries (possibly empty).
fn tag_shape() -> impl Strategy<Value = BTreeMap<char, BTreeSet<usize>>> {
    prop::collection::btree_map(
        prop_oneof![Just(TAG_NAMES[0]), Just(TAG_NAMES[1])],
        prop::collection::btree_set(0usize..TAG_VALUES.len(), 0..=TAG_VALUES.len()),
        0..=TAG_NAMES.len(),
    )
}

/// True iff this shape leaves its axis unconstrained -- `None` OR `Some(∅)`.
/// Both match every value on the axis, so neither may be folded into a
/// narrower `Some(non-empty)` set by a widen-only rule.
fn is_unconstrained(shape: &Option<BTreeSet<usize>>) -> bool {
    !matches!(shape, Some(values) if !values.is_empty())
}

/// The event universe the widening oracle is evaluated over: every
/// (kind, author) pair, tagged, plus untagged events of the same shape so
/// that "tag name constrained at all" is itself discriminating.
struct Universe {
    authors: Vec<String>,
    events: Vec<nostr::Event>,
}

fn universe() -> Universe {
    let keys: Vec<Keys> = (0..3).map(|_| Keys::generate()).collect();
    let authors: Vec<String> = keys.iter().map(|k| k.public_key().to_hex()).collect();
    let mut events = Vec::new();
    for (i, kind) in KIND_POOL.iter().enumerate() {
        for (j, key) in keys.iter().enumerate() {
            let value = TAG_VALUES[(i + j) % TAG_VALUES.len()];
            events.push(
                EventBuilder::new(Kind::from(*kind), "")
                    .tags([Tag::hashtag(value), Tag::identifier(value)])
                    .sign_with_keys(key)
                    .expect("test fixture event must sign cleanly"),
            );
            events.push(
                EventBuilder::new(Kind::from(*kind), "untagged")
                    .sign_with_keys(key)
                    .expect("test fixture event must sign cleanly"),
            );
        }
    }
    Universe { authors, events }
}

fn build_filter(
    u: &Universe,
    kinds: &Option<BTreeSet<usize>>,
    authors: &Option<BTreeSet<usize>>,
    ids: &Option<BTreeSet<usize>>,
    tags: &BTreeMap<char, BTreeSet<usize>>,
) -> ConcreteFilter {
    ConcreteFilter {
        kinds: kinds
            .as_ref()
            .map(|s| s.iter().map(|&i| KIND_POOL[i]).collect()),
        authors: authors
            .as_ref()
            .map(|s| s.iter().map(|&i| u.authors[i].clone()).collect()),
        ids: ids
            .as_ref()
            .map(|s| s.iter().map(|&i| u.events[i].id.to_hex()).collect()),
        tags: tags
            .iter()
            .map(|(&name, values)| {
                (
                    IndexedTagName::new(name).expect("TAG_NAMES are ASCII letters"),
                    values
                        .iter()
                        .map(|&i| TAG_VALUES[i].to_string())
                        .collect::<BTreeSet<String>>(),
                )
            })
            .collect(),
        ..ConcreteFilter::default()
    }
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

#[test]
fn merge_rule_widens_id_union() {
    let keys = Keys::generate();
    let events: Vec<nostr::Event> = (0..8)
        .map(|i| {
            EventBuilder::new(Kind::TextNote, format!("event-{i}"))
                .sign_with_keys(&keys)
                .unwrap()
        })
        .collect();

    proptest!(|(
        ids_a in prop::collection::btree_set(0usize..events.len(), 1..=4),
        ids_b in prop::collection::btree_set(0usize..events.len(), 1..=4)
    )| {
        let a = ConcreteFilter {
            ids: Some(ids_a.iter().map(|&i| events[i].id.to_hex()).collect()),
            ..ConcreteFilter::default()
        };
        let b = ConcreteFilter {
            ids: Some(ids_b.iter().map(|&i| events[i].id.to_hex()).collect()),
            ..ConcreteFilter::default()
        };
        if let Some(merged) = IdUnion.try_merge(&a, &b) {
            for event in &events {
                if matches(&a, event) || matches(&b, event) {
                    prop_assert!(matches(&merged, event));
                }
            }
        }
    });
}

/// #900's durable guard, and the one test in this file that is not written
/// against a specific rule: `matches(try_merge(a,b)) ⊇ matches(a) ∪
/// matches(b)` for EVERY registered rule, over operand pairs drawn from the
/// FULL component-shape space -- `None`, `Some(∅)` and `Some(non-empty)` on
/// `kinds`/`authors`/`ids`, and present/absent/disjoint tag NAMES.
///
/// The three `merge_rule_widens_*` tests above are the per-rule M2 contract
/// tests; each pins one rule's own axis. This one exists because #900 was
/// not a defect in any single rule's logic so much as a hole in what the
/// generators could express: no generator in this file could produce an
/// operand that left a component UNCONSTRAINED, so no generator could ever
/// pair one against a constrained sibling -- the single pairing that makes a
/// union rule narrow. It is deliberately written against `dyn MergeRule` so
/// it keeps guarding the same class when these three rules are replaced.
///
/// The fire counters are load-bearing: a widening property over pairs that
/// no rule ever accepts is vacuously green, which is the failure mode that
/// let #900 through in the first place.
#[test]
fn every_merge_rule_widens_across_the_full_component_shape_space() {
    let u = universe();
    let events = u.events.len();
    let rules: Vec<Box<dyn MergeRule>> = vec![
        Box::new(AuthorUnion),
        Box::new(KindUnion),
        Box::new(IdUnion),
    ];
    let fired: Vec<AtomicUsize> = rules.iter().map(|_| AtomicUsize::new(0)).collect();

    proptest!(|(
        kinds_a in component_shape(KIND_POOL.len(), 2),
        kinds_b in component_shape(KIND_POOL.len(), 2),
        authors_a in component_shape(3, 2),
        authors_b in component_shape(3, 2),
        ids_a in component_shape(events, 2),
        ids_b in component_shape(events, 2),
        tags_a in tag_shape(),
        tags_b in tag_shape(),
        // Mostly vary exactly ONE component (the shape every union rule's
        // domain requires, so merges actually fire), sometimes vary an
        // arbitrary subset (so multi-axis pairs are covered too).
        vary in prop_oneof![
            6 => (0usize..4).prop_map(|i| { let mut v = [false; 4]; v[i] = true; v }),
            1 => any::<[bool; 4]>(),
        ],
    )| {
        let a = build_filter(&u, &kinds_a, &authors_a, &ids_a, &tags_a);
        let b = build_filter(
            &u,
            if vary[0] { &kinds_b } else { &kinds_a },
            if vary[1] { &authors_b } else { &authors_a },
            if vary[2] { &ids_b } else { &ids_a },
            if vary[3] { &tags_b } else { &tags_a },
        );

        for (idx, rule) in rules.iter().enumerate() {
            let Some(merged) = rule.try_merge(&a, &b) else {
                continue;
            };
            fired[idx].fetch_add(1, Ordering::Relaxed);
            for event in &u.events {
                if matches(&a, event) || matches(&b, event) {
                    prop_assert!(
                        matches(&merged, event),
                        "{} NARROWED: an event matching an operand does not match the merge\n  \
                         a       = {a:?}\n  b       = {b:?}\n  merged  = {merged:?}",
                        rule.name(),
                    );
                }
            }
        }
    });

    for (idx, rule) in rules.iter().enumerate() {
        assert!(
            fired[idx].load(Ordering::Relaxed) > 0,
            "the generator never produced a pair {} accepts -- the widening \
             property is VACUOUS for it, which is exactly how #900 survived",
            rule.name()
        );
    }
}

/// The #900 falsifier stated structurally, so it does not depend on the
/// sampled event universe happening to contain a discriminating event: a
/// union rule must REFUSE outright whenever either operand leaves the axis
/// it unions unconstrained (`None` or `Some(∅)`). An unconstrained operand
/// is already a superset of any constrained sibling with the same skeleton,
/// so there is nothing to gain from the merge and the widening contract to
/// lose.
#[test]
fn union_rules_refuse_an_operand_that_leaves_the_merged_axis_unconstrained() {
    let u = universe();

    // Index pool 3 is valid on every axis at once: 3 kinds in `KIND_POOL`,
    // 3 authors, and far more than 3 events in the universe.
    proptest!(|(
        shape_a in component_shape(3, 2),
        shape_b in component_shape(3, 2),
        kind_shape in component_shape(KIND_POOL.len(), 2),
    )| {
        prop_assume!(shape_a != shape_b);
        prop_assume!(is_unconstrained(&shape_a) || is_unconstrained(&shape_b));
        let none = None;
        let no_tags = BTreeMap::new();

        // authors axis -- everything else identical, so only AuthorUnion's
        // domain is in play.
        let a = build_filter(&u, &kind_shape, &shape_a, &none, &no_tags);
        let b = build_filter(&u, &kind_shape, &shape_b, &none, &no_tags);
        prop_assert!(AuthorUnion.try_merge(&a, &b).is_none());

        // kinds axis.
        let a = build_filter(&u, &shape_a, &none, &none, &no_tags);
        let b = build_filter(&u, &shape_b, &none, &none, &no_tags);
        prop_assert!(KindUnion.try_merge(&a, &b).is_none());

        // ids axis.
        let a = build_filter(&u, &none, &none, &shape_a, &no_tags);
        let b = build_filter(&u, &none, &none, &shape_b, &no_tags);
        prop_assert!(IdUnion.try_merge(&a, &b).is_none());
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
