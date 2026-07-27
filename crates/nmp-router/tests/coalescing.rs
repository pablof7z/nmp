//! M2 contract tests 10, 11, 13 (`docs/plans/M2-compiler-router-plan.md`
//! §4.2, §4.3, §5) — the widen-only property test per `MergeRule`, the
//! local-refilter exactness property, and the non-widening-rule drop
//! mechanism.
//!
//! `default_widen_only()` now holds ONE rule (`StructuralUnion`) spanning all
//! four array axes, so the per-rule structure these tests once had has become
//! PER-AXIS: the vacuity guard that matters is no longer "did the rule fire"
//! but "did it fire on each axis it claims to cover".

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};

use nostr::filter::MatchEventOptions;
use nostr::{EventBuilder, Keys, Kind, Tag};
use proptest::prelude::*;

use nmp_grammar::{ConcreteFilter, IndexedTagName};
use nmp_router::{deliver, DiscardSecondOperand, MergeRule, RuleRegistry, StructuralUnion};

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
//     side and absent on the other. These were the same class of trap on an
//     axis that had no union rule at all; `StructuralUnion` now covers that
//     axis, so the shapes these generators emit are exactly the ones its
//     inverted polarity has to get right.
//
// The tag axis's polarity is INVERTED relative to kinds/authors/ids, which
// is why it needs its own generator rather than another `component_shape`
// (measured, not assumed): an ABSENT tag name is the unconstrained shape
// (matches every event), while a PRESENT name with an empty value set
// (`{t: ∅}`) matches NOTHING -- it lowers to `generic_tags: {t: {}}`, which
// `match_event` fails for tagged and untagged events alike. So a future
// tag-union rule's trap is folding an ABSENT name into a present one, and
// `{t: ∅}` is the harmless end; on `authors`/`kinds`/`ids` BOTH `None` and
// `Some(∅)` are the trap.
//
// These helpers are deliberately RULE-AGNOSTIC: they describe the shape
// space of a `ConcreteFilter`, not the domain of any one rule -- which is
// why they carried over unchanged when the AuthorUnion/KindUnion/IdUnion
// trio was replaced by the single structural "exactly one array component
// differs" rule.
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

/// The four array axes `StructuralUnion` spans, as a fire-counter key. One
/// counter for the rule as a whole would be VACUITY BY ANOTHER NAME: the rule
/// firing sixty times on `authors` proves nothing about `tags`, which is the
/// axis that had no rule at all until now.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Axis {
    Kinds,
    Authors,
    Ids,
    Tags,
}

const AXES: [Axis; 4] = [Axis::Kinds, Axis::Authors, Axis::Ids, Axis::Tags];

/// One hand-built mergeable pair per axis: identical in every component
/// except `axis`, where the two operands hold different single values. Both
/// operands stay constrained on every axis, since an unconstrained operand is
/// refused outright (the #900 guard) and would prove nothing about firing.
fn constructed_pair_differing_in(u: &Universe, axis: Axis) -> (ConcreteFilter, ConcreteFilter) {
    let kinds = |i: usize| Some(BTreeSet::from([i]));
    let one = |i: usize| Some(BTreeSet::from([i]));
    let tag = |i: usize| BTreeMap::from([('t', BTreeSet::from([i]))]);

    let (ka, kb) = match axis {
        Axis::Kinds => (kinds(0), kinds(1)),
        _ => (kinds(0), kinds(0)),
    };
    let (aa, ab) = match axis {
        Axis::Authors => (one(0), one(1)),
        _ => (one(0), one(0)),
    };
    let (ia, ib) = match axis {
        Axis::Ids => (one(0), one(1)),
        _ => (one(0), one(0)),
    };
    let (ta, tb) = match axis {
        Axis::Tags => (tag(0), tag(1)),
        _ => (tag(0), tag(0)),
    };

    (
        build_filter(u, &ka, &aa, &ia, &ta),
        build_filter(u, &kb, &ab, &ib, &tb),
    )
}

/// Which array axis a successful merge actually widened, read off the OUTPUT
/// rather than assumed from the generator: the merged filter differs from `a`
/// on exactly the component that was unioned.
fn widened_axis(a: &ConcreteFilter, merged: &ConcreteFilter) -> Option<Axis> {
    if a.kinds != merged.kinds {
        return Some(Axis::Kinds);
    }
    if a.authors != merged.authors {
        return Some(Axis::Authors);
    }
    if a.ids != merged.ids {
        return Some(Axis::Ids);
    }
    if a.tags != merged.tags {
        return Some(Axis::Tags);
    }
    None
}

/// Test 10: `merge_rule_widens`, over the FULL component-shape space --
/// `None`, `Some(∅)` and `Some(non-empty)` on `kinds`/`authors`/`ids`, and
/// present/absent/disjoint tag NAMES with possibly-empty value sets.
///
/// #900 lived because the generators could not express an UNCONSTRAINED
/// operand, so no generator could ever pair one against a constrained sibling
/// -- the single pairing that makes a union rule narrow. It is written
/// against `dyn MergeRule` so it keeps guarding the same class if the
/// registry ever holds more than one rule again.
///
/// THE PER-AXIS FIRE COUNTERS ARE LOAD-BEARING. A widening property over
/// pairs no rule accepts is vacuously green -- that is the second, subtler
/// reason #900 survived. Collapsing three rules into one makes a single
/// whole-rule counter weaker than what it replaced, because the rule can fire
/// prolifically on `authors` while never once touching `tags`. Each of the
/// four axes must be measured firing in its own right.
#[test]
fn the_merge_rule_widens_across_the_full_component_shape_space() {
    let u = universe();
    let events = u.events.len();
    let rules: Vec<Box<dyn MergeRule>> = vec![Box::new(StructuralUnion)];
    let fired: Vec<AtomicUsize> = AXES.iter().map(|_| AtomicUsize::new(0)).collect();

    proptest!(|(
        kinds_a in component_shape(KIND_POOL.len(), 2),
        kinds_b in component_shape(KIND_POOL.len(), 2),
        authors_a in component_shape(3, 2),
        authors_b in component_shape(3, 2),
        ids_a in component_shape(events, 2),
        ids_b in component_shape(events, 2),
        tags_a in tag_shape(),
        tags_b in tag_shape(),
        // Mostly vary exactly ONE component (the shape the rule's domain
        // requires, so merges actually fire), sometimes vary an arbitrary
        // subset (so multi-axis pairs are covered too and the two-component
        // refusal is exercised).
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

        for rule in &rules {
            let Some(merged) = rule.try_merge(&a, &b) else {
                continue;
            };
            if let Some(axis) = widened_axis(&a, &merged) {
                let idx = AXES.iter().position(|x| *x == axis).expect("known axis");
                fired[idx].fetch_add(1, Ordering::Relaxed);
            }
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

    // Deliberately NOT asserted here: see
    // `the_merge_rule_fires_on_every_axis` for why the vacuity guard is a
    // separate, constructed test rather than a claim about what random
    // sampling happened to reach.
    let _ = &fired;
}

/// The vacuity guard, stated over CONSTRUCTED pairs rather than sampled ones.
///
/// A widening property over pairs no rule accepts is vacuously green -- the
/// second, subtler reason #900 survived -- so something must prove the rule
/// actually fires on each axis. That guarantee used to be asserted inside the
/// proptest above, from per-axis fire counters accumulated over 256 random
/// cases. It was measured failing about one run in twelve on a green tree:
/// with four axes competing for a fixed sample budget, an axis occasionally
/// never came up, and the suite reported a defect where there was none.
///
/// Sampling is the wrong instrument for this claim. "Can the rule fire on the
/// tags axis" is a fact about the rule, not about a draw, so it is asserted
/// here directly: one hand-built pair per axis, differing in exactly that
/// component and nothing else. That is both deterministic and strictly
/// stronger, because it fails the moment an axis stops being mergeable
/// instead of only when the generator happens to notice.
#[test]
fn the_merge_rule_fires_on_every_axis() {
    let u = universe();
    let rule = StructuralUnion;

    for axis in AXES {
        let (a, b) = constructed_pair_differing_in(&u, axis);
        let merged = rule.try_merge(&a, &b).unwrap_or_else(|| {
            panic!(
                "StructuralUnion refuses a pair differing ONLY in {axis:?}, so the \
                 widening property is vacuous on that axis\n  a = {a:?}\n  b = {b:?}"
            )
        });
        let widened = widened_axis(&a, &merged).unwrap_or_else(|| {
            panic!("merging a {axis:?}-only pair widened no axis at all\n  merged = {merged:?}")
        });
        assert_eq!(
            widened, axis,
            "merging a pair differing only in {axis:?} widened {widened:?} instead"
        );
    }
}

/// The #900 falsifier stated structurally, so it does not depend on the
/// sampled event universe happening to contain a discriminating event: the
/// rule must REFUSE outright whenever either operand leaves the axis it
/// unions unconstrained (`None` or `Some(∅)` on `kinds`/`authors`/`ids`). An
/// unconstrained operand is already a superset of any constrained sibling
/// with the same skeleton, so there is nothing to gain from the merge and the
/// widening contract to lose.
#[test]
fn the_rule_refuses_an_operand_that_leaves_the_merged_axis_unconstrained() {
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

        // authors axis -- everything else identical, so `authors` is the
        // single differing component and nothing else can be blamed.
        let a = build_filter(&u, &kind_shape, &shape_a, &none, &no_tags);
        let b = build_filter(&u, &kind_shape, &shape_b, &none, &no_tags);
        prop_assert!(StructuralUnion.try_merge(&a, &b).is_none());

        // kinds axis.
        let a = build_filter(&u, &shape_a, &none, &none, &no_tags);
        let b = build_filter(&u, &shape_b, &none, &none, &no_tags);
        prop_assert!(StructuralUnion.try_merge(&a, &b).is_none());

        // ids axis.
        let a = build_filter(&u, &none, &none, &shape_a, &no_tags);
        let b = build_filter(&u, &none, &none, &shape_b, &no_tags);
        prop_assert!(StructuralUnion.try_merge(&a, &b).is_none());
    });
}

/// THE TAG AXIS'S OWN POLARITY TEST, and the reason the check above cannot
/// simply be extended to tags. On `authors`/`kinds`/`ids` the unconstrained
/// shapes are `None` and `Some(∅)`. On tags the polarity INVERTS:
///
/// - an ABSENT tag name is UNCONSTRAINED -- the filter matches every event on
///   that axis, so folding it into a present name NARROWS, and must be
///   refused;
/// - a PRESENT name with an EMPTY value set matches NOTHING, so unioning it
///   in is a widening and must be ACCEPTED.
///
/// Both halves are asserted, because a rule that got the polarity backwards
/// would still pass a one-sided "refuses something" test.
#[test]
fn the_rule_refuses_an_absent_tag_name_but_accepts_an_empty_value_set() {
    let u = universe();
    let name = IndexedTagName::new(TAG_NAMES[0]).expect("TAG_NAMES are ASCII letters");

    proptest!(|(
        values in prop::collection::btree_set(0usize..TAG_VALUES.len(), 0..=TAG_VALUES.len()),
        kind_shape in component_shape(KIND_POOL.len(), 2),
    )| {
        let none = None;
        let present = BTreeMap::from([(TAG_NAMES[0], values.clone())]);
        let absent = BTreeMap::new();

        let bearing = build_filter(&u, &kind_shape, &none, &none, &present);
        let unconstrained = build_filter(&u, &kind_shape, &none, &none, &absent);

        // The DANGEROUS end. An absent name matches everything; the merge
        // would constrain it to a value list.
        prop_assert!(StructuralUnion.try_merge(&unconstrained, &bearing).is_none());
        prop_assert!(StructuralUnion.try_merge(&bearing, &unconstrained).is_none());

        // The HARMLESS end. `{t:∅}` matches nothing, so the union of it with
        // any sibling under the same name is a widening -- and must actually
        // be taken, or the rule is over-refusing on a shape it could have
        // collapsed.
        let empty = build_filter(
            &u,
            &kind_shape,
            &none,
            &none,
            &BTreeMap::from([(TAG_NAMES[0], BTreeSet::new())]),
        );
        if empty != bearing {
            let merged = StructuralUnion
                .try_merge(&empty, &bearing)
                .expect("{t:∅} matches nothing, so unioning it in only widens");
            prop_assert_eq!(
                merged.tags.get(&name),
                bearing.tags.get(&name),
                "the union with the empty set is the non-empty set itself"
            );
            for event in &u.events {
                if matches(&empty, event) || matches(&bearing, event) {
                    prop_assert!(matches(&merged, event));
                }
            }
        }
    });
}

/// TAGS ARE CONJUNCTIVE ACROSS NAMES, and this is the most dangerous mistake
/// available on the new axis: had the rule treated `tags` as ONE component,
/// `{#t:X}` unioned with `{#d:Y}` would have produced a filter demanding BOTH
/// -- matching NEITHER operand. That is a narrowing, and no amount of value
/// overlap makes it safe.
///
/// Asserted structurally (the rule must refuse) AND semantically (no event
/// matching either operand may be lost), so it holds whether or not the
/// sampled universe contains a discriminating event.
#[test]
fn the_rule_never_merges_across_two_tag_names() {
    let u = universe();

    proptest!(|(
        values_a in prop::collection::btree_set(0usize..TAG_VALUES.len(), 1..=TAG_VALUES.len()),
        values_b in prop::collection::btree_set(0usize..TAG_VALUES.len(), 1..=TAG_VALUES.len()),
        kind_shape in component_shape(KIND_POOL.len(), 2),
    )| {
        let none = None;
        let a = build_filter(
            &u,
            &kind_shape,
            &none,
            &none,
            &BTreeMap::from([(TAG_NAMES[0], values_a)]),
        );
        let b = build_filter(
            &u,
            &kind_shape,
            &none,
            &none,
            &BTreeMap::from([(TAG_NAMES[1], values_b)]),
        );
        prop_assert!(
            StructuralUnion.try_merge(&a, &b).is_none(),
            "#{} and #{} are TWO components -- merging them demands both at once",
            TAG_NAMES[0],
            TAG_NAMES[1]
        );
        prop_assert!(StructuralUnion.try_merge(&b, &a).is_none());
    });
}

/// The relay-truncation falsifier the per-event widening property can never
/// catch: `match_event` is a PREDICATE, so it cannot express "a relay only
/// returns the first `limit` rows" -- a merged filter can satisfy the
/// per-event widening property and STILL under-fetch once a real relay
/// truncates the result count. The actual fix is structural (exclude any
/// limited filter from merging), so this checks the structural invariant
/// directly.
///
/// THE GENERATOR IS CONSTRAINED TO PAIRS THAT WOULD OTHERWISE MERGE. Varying
/// two axes at once would make the pair a two-component refusal regardless of
/// `limit`, and the property would prove nothing about limits at all -- the
/// same vacuity trap as an unfired rule. Here exactly one array axis differs,
/// so the ONLY thing that can refuse the pair is the limit.
#[test]
fn the_rule_never_merges_a_filter_that_carries_a_limit() {
    let pool: Vec<Keys> = (0..4).map(|_| Keys::generate()).collect();
    let pool_hex: Vec<String> = pool.iter().map(|k| k.public_key().to_hex()).collect();
    let n = pool.len();
    let merged_without_limits = AtomicUsize::new(0);

    proptest!(|(
        kind in small_kind(),
        authors_a in prop::collection::btree_set(0..n, 1..=2),
        authors_b in prop::collection::btree_set(0..n, 1..=2),
        limit_a in prop::option::of(1usize..500),
        limit_b in prop::option::of(1usize..500),
    )| {
        prop_assume!(limit_a.is_some() || limit_b.is_some());
        prop_assume!(authors_a != authors_b);
        let build = |authors: &BTreeSet<usize>, limit| ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            authors: Some(authors.iter().map(|&i| pool_hex[i].clone()).collect()),
            limit,
            ..ConcreteFilter::default()
        };
        let a = build(&authors_a, limit_a);
        let b = build(&authors_b, limit_b);
        prop_assert!(StructuralUnion.try_merge(&a, &b).is_none());

        // ANTI-VACUITY: the SAME pair without limits must merge. Without
        // this, a generator drifting into pairs that refuse for some other
        // reason (two components differing, an unconstrained operand) would
        // keep the property green while proving nothing about `limit`.
        let unlimited_a = build(&authors_a, None);
        let unlimited_b = build(&authors_b, None);
        prop_assert!(
            StructuralUnion.try_merge(&unlimited_a, &unlimited_b).is_some(),
            "the fixture pair must be mergeable BUT FOR the limit, or this \
             property is vacuous"
        );
        merged_without_limits.fetch_add(1, Ordering::Relaxed);
    });

    assert!(
        merged_without_limits.load(Ordering::Relaxed) > 0,
        "no generated pair was ever mergeable-but-for-the-limit"
    );
}

/// Two components moving at once is the CARTESIAN-CORNER refusal, and it is a
/// ratified hard rule rather than a conservative default:
/// `{k:[1],a:[A]} + {k:[2],a:[B]} → {k:[1,2],a:[A,B]}` also fetches kind 2
/// from A and kind 1 from B, events neither operand asked for, with unbounded
/// waste on sparse inputs. Guarded as a property so no future "improvement"
/// can quietly relax it.
#[test]
fn the_rule_never_merges_two_components_at_once() {
    let u = universe();

    proptest!(|(
        kinds_a in prop::collection::btree_set(0..KIND_POOL.len(), 1..=2),
        kinds_b in prop::collection::btree_set(0..KIND_POOL.len(), 1..=2),
        authors_a in prop::collection::btree_set(0usize..3, 1..=2),
        authors_b in prop::collection::btree_set(0usize..3, 1..=2),
    )| {
        prop_assume!(kinds_a != kinds_b);
        prop_assume!(authors_a != authors_b);
        let no_tags = BTreeMap::new();
        let a = build_filter(&u, &Some(kinds_a), &Some(authors_a), &None, &no_tags);
        let b = build_filter(&u, &Some(kinds_b), &Some(authors_b), &None, &no_tags);
        prop_assert!(StructuralUnion.try_merge(&a, &b).is_none());
    });
}

/// Test 11: `local_refilter_is_exact` -- ties widen-only + the local
/// re-filter together end to end. Merges atom X (author A) and atom Y
/// (author B) into wire filter M; a relay serving M would return every event
/// in the universe matching M (a strict superset of X's own matches, since M
/// widens). `deliver(wire_events, X)` must recover EXACTLY the events X's own
/// filter matches out of the full universe -- no over-delivery (B's-only
/// events excluded) and no under-delivery (every A event present).
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
        let merged = StructuralUnion
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

/// The same exactness on the TAG axis, which is what the merge rule newly
/// touches: two `#t` watches merge into one wire filter, and each watch's own
/// re-filter must recover exactly its own value's events out of what the
/// relay returns for the union. This is the mechanism that makes tag merging
/// a bandwidth trade rather than a correctness one.
#[test]
fn local_refilter_is_exact_on_the_tag_axis() {
    let u = universe();
    let name = IndexedTagName::new(TAG_NAMES[0]).expect("TAG_NAMES are ASCII letters");

    let tagged = |value: &str| ConcreteFilter {
        kinds: Some(KIND_POOL.iter().copied().collect()),
        tags: BTreeMap::from([(name, BTreeSet::from([value.to_string()]))]),
        ..ConcreteFilter::default()
    };
    let x = tagged(TAG_VALUES[0]);
    let y = tagged(TAG_VALUES[1]);
    let merged = StructuralUnion
        .try_merge(&x, &y)
        .expect("same everything but one tag name's values -- must merge");

    let wire_events: Vec<nostr::Event> = u
        .events
        .iter()
        .filter(|e| matches(&merged, e))
        .cloned()
        .collect();
    assert!(
        !wire_events.is_empty(),
        "the fixture universe must contain events the merged filter matches"
    );

    for own in [&x, &y] {
        let delivered: BTreeSet<nostr::EventId> = deliver(&wire_events, own)
            .into_iter()
            .map(|e| e.id)
            .collect();
        let expected: BTreeSet<nostr::EventId> = u
            .events
            .iter()
            .filter(|e| matches(own, e))
            .map(|e| e.id)
            .collect();
        assert!(
            !expected.is_empty(),
            "each single-value watch must match something, or this proves nothing"
        );
        assert_eq!(
            delivered, expected,
            "the local re-filter must recover exactly this watch's own events \
             out of the widened wire result"
        );
    }
}

/// Test 13: `non_widening_rule_is_dropped_and_ships_separately`.
#[test]
fn non_widening_rule_is_dropped_and_ships_separately() {
    let registry =
        RuleRegistry::default_widen_only().register(Box::new(DiscardSecondOperand), false);
    assert_eq!(registry.dropped_rules(), &["DiscardSecondOperand"]);

    // Same `kinds`, different `since` -- outside StructuralUnion's domain (a
    // differing scalar is a refusal), but squarely inside
    // DiscardSecondOperand's unsound applicability predicate.
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
