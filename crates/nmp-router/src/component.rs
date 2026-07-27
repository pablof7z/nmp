//! The STRUCTURAL COMPONENT MODEL of a [`ConcreteFilter`] — the one
//! vocabulary in which both halves of the subscription design express
//! themselves.
//!
//! A filter is read as a bag of independent components:
//!
//! ```text
//! since | until | kinds | authors | ids | one component PER TAG NAME | limit
//! ```
//!
//! Two consumers ask the same question of it, for different reasons:
//!
//! - [`crate::coalesce`] merges two filters only when they differ in exactly
//!   one ARRAY component (`kinds`/`authors`/`ids`/one tag name). Unioning two
//!   components at once over-widens into cartesian corners:
//!   `{k:[1],a:[A]} + {k:[2],a:[B]}` would also fetch k2-from-A and
//!   k1-from-B, events neither operand asked for, and the waste is unbounded
//!   on sparse inputs.
//! - [`crate::wire_id`] decides which previously-allocated wire token a newly
//!   compiled filter CONTINUES: the one it differs from in exactly one
//!   component, zero-diff ranked first.
//!
//! **They share this module deliberately, and the sharing is load-bearing
//! rather than incidental.** The design's whole wire story is that growing a
//! value set costs ONE overwriting REQ. That only holds if what the merge
//! produces when a value arrives is, by the identity matcher's own
//! definition, a one-component difference from what the merge produced last
//! compile. Two separate notions of "component" could drift apart, and the
//! symptom would be silent: merges that mint fresh tokens and churn the wire
//! instead of widening in place. One definition makes the agreement
//! structural.
//!
//! What the two halves do NOT share is policy. Merging decides HOW MANY
//! subscriptions exist and carries a widening proof obligation; identity
//! decides WHAT THEY ARE CALLED and carries none. Only the coordinate system
//! is common (`docs/internals/subscriptions/identity-grouping-and-limits.md`
//! §7.3).
//!
//! TAG POLARITY IS INVERTED and lives with the consumers, not here: this
//! module reports THAT a tag name differs, never whether either side's shape
//! is admissible. On `authors`/`kinds`/`ids` both `None` and `Some(∅)` are
//! unconstrained; on tags an ABSENT name is unconstrained while a present
//! name with an empty value set matches nothing (§3.5).

// Only [`differing`] (test-only) and the tests themselves collect a set here;
// `sole_difference` walks the two tag maps directly and allocates nothing.
#[cfg(test)]
use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, IndexedTagName};

/// One component of a filter's structural signature. `Tag` is per tag NAME.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Component {
    Since,
    Until,
    Kinds,
    Authors,
    Ids,
    Limit,
    Tag(IndexedTagName),
}

/// Every signature component in which `a` and `b` disagree.
///
/// THE EXHAUSTIVE FORM, and deliberately test-only: both production callers
/// ask only "is there exactly one difference, and which", which
/// [`sole_difference`] answers without building a list. This one survives
/// because it is the readable statement of the model and the thing the tests
/// assert against ("these two filters differ in TWO components"), with
/// `sole_difference_agrees_with_differing` pinning the fast path to it.
///
/// A field that is `None` in one filter and `Some` in the other DOES differ —
/// including `authors: None` vs `authors: Some(∅)`, which are distinct
/// selections (absent dimension vs a dimension constrained to nothing), and a
/// tag name present in one filter and absent from the other.
///
/// THE DESTRUCTURING BELOW IS A GUARD, not style. This function's contract is
/// that it enumerates EVERY field of `ConcreteFilter`; a field it forgets is
/// reported as always-equal, and both consumers then treat two filters that
/// genuinely disagree on it as a one-component (or zero-component) move. For
/// `wire_id` that misnames a subscription; for `coalesce` it is a real
/// NARROWING — the merge keeps `a`'s value for the forgotten field and drops
/// `b`'s constraint entirely, violating the widen-only contract. Binding
/// every field by name makes adding an eighth field a compile error here
/// rather than a silent defect in two places at once.
#[cfg(test)]
pub(crate) fn differing(a: &ConcreteFilter, b: &ConcreteFilter) -> Vec<Component> {
    let ConcreteFilter {
        kinds: a_kinds,
        authors: a_authors,
        ids: a_ids,
        tags: a_tags,
        since: a_since,
        until: a_until,
        limit: a_limit,
    } = a;
    let ConcreteFilter {
        kinds: b_kinds,
        authors: b_authors,
        ids: b_ids,
        tags: b_tags,
        since: b_since,
        until: b_until,
        limit: b_limit,
    } = b;

    let mut out = Vec::new();
    if a_since != b_since {
        out.push(Component::Since);
    }
    if a_until != b_until {
        out.push(Component::Until);
    }
    if a_kinds != b_kinds {
        out.push(Component::Kinds);
    }
    if a_authors != b_authors {
        out.push(Component::Authors);
    }
    if a_ids != b_ids {
        out.push(Component::Ids);
    }
    if a_limit != b_limit {
        out.push(Component::Limit);
    }
    // The UNION of both filters' tag names: a name only one side carries is
    // itself a differing component.
    let names: BTreeSet<&IndexedTagName> = a_tags.keys().chain(b_tags.keys()).collect();
    for name in names {
        if a_tags.get(name) != b_tags.get(name) {
            out.push(Component::Tag(*name));
        }
    }
    out
}

/// The ONE component `a` and `b` differ in — `None` when they agree entirely
/// or disagree in more than one.
///
/// This is what both production callers actually want, and it SHORT-CIRCUITS
/// on the second difference where [`differing`] builds the whole list. That
/// matters: the merge path calls this O(n²) times per compile, the router
/// recompiles the entire plan on every demand mutation (never incrementally),
/// and a catalog resolving one value at a time therefore pays a fresh
/// all-pairs sweep per value. Returning a heap-allocated `Vec` for a question
/// usually settled by the second field was pure overhead — and it is overhead
/// the collapse itself created, since before the union rule spanned four axes
/// most of these pairs were rejected by a cheap field comparison.
///
/// [`differing`] stays as the exhaustive form: it is the readable statement of
/// the model, it is what the tests assert against ("these two filters differ
/// in TWO components"), and `sole_difference_agrees_with_differing` pins the
/// two to each other so the fast path cannot drift from the definition.
pub(crate) fn sole_difference(a: &ConcreteFilter, b: &ConcreteFilter) -> Option<Component> {
    let ConcreteFilter {
        kinds: a_kinds,
        authors: a_authors,
        ids: a_ids,
        tags: a_tags,
        since: a_since,
        until: a_until,
        limit: a_limit,
    } = a;
    let ConcreteFilter {
        kinds: b_kinds,
        authors: b_authors,
        ids: b_ids,
        tags: b_tags,
        since: b_since,
        until: b_until,
        limit: b_limit,
    } = b;

    let mut found: Option<Component> = None;
    // Two differences is already an answer, so every check below bails out
    // rather than continuing to classify.
    let mut record = |component: Component, differs: bool| -> bool {
        if !differs {
            return true;
        }
        if found.is_some() {
            return false;
        }
        found = Some(component);
        true
    };

    if !record(Component::Since, a_since != b_since)
        || !record(Component::Until, a_until != b_until)
        || !record(Component::Kinds, a_kinds != b_kinds)
        || !record(Component::Authors, a_authors != b_authors)
        || !record(Component::Ids, a_ids != b_ids)
        || !record(Component::Limit, a_limit != b_limit)
    {
        return None;
    }
    // Tag names: the UNION of both sides' keys, since a name only one side
    // carries is itself a difference. Walked over the two `BTreeMap`s rather
    // than collected into a set, so this allocates nothing -- `b`'s keys are
    // filtered to those `a` does not carry, or a shared differing name would
    // be counted twice and read as two components.
    for name in a_tags
        .keys()
        .chain(b_tags.keys().filter(|name| !a_tags.contains_key(*name)))
    {
        if !record(Component::Tag(*name), a_tags.get(name) != b_tags.get(name)) {
            return None;
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn cf() -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(BTreeSet::from(["aa".to_string()])),
            ..ConcreteFilter::default()
        }
    }

    fn tag(c: char, values: &[&str]) -> BTreeMap<IndexedTagName, BTreeSet<String>> {
        BTreeMap::from([(
            IndexedTagName::new(c).unwrap(),
            values.iter().map(|s| s.to_string()).collect(),
        )])
    }

    #[test]
    fn identical_filters_differ_in_nothing() {
        assert!(differing(&cf(), &cf()).is_empty());
    }

    #[test]
    fn each_axis_is_its_own_component() {
        let mut b = cf();
        b.authors = Some(BTreeSet::from(["bb".to_string()]));
        assert_eq!(differing(&cf(), &b), vec![Component::Authors]);

        let mut b = cf();
        b.ids = Some(BTreeSet::from(["cc".to_string()]));
        assert_eq!(
            differing(&cf(), &b),
            vec![Component::Ids],
            "ids is a component in its own right -- ConcreteFilter has FOUR array axes"
        );

        let mut b = cf();
        b.limit = Some(10);
        assert_eq!(differing(&cf(), &b), vec![Component::Limit]);

        let mut b = cf();
        b.since = Some(10);
        assert_eq!(differing(&cf(), &b), vec![Component::Since]);

        let mut b = cf();
        b.until = Some(10);
        assert_eq!(differing(&cf(), &b), vec![Component::Until]);
    }

    /// `authors: None` and `authors: Some(∅)` are DIFFERENT selections: an
    /// absent dimension versus a dimension constrained to nothing.
    #[test]
    fn absent_and_empty_author_sets_differ() {
        let mut none = cf();
        none.authors = None;
        let mut empty = cf();
        empty.authors = Some(BTreeSet::new());
        assert_eq!(differing(&none, &empty), vec![Component::Authors]);
    }

    /// The unsoundness one lumped tag component would introduce: tags are
    /// CONJUNCTIVE across names, so moving `#e` AND `#p` together is a
    /// two-component move, not a one-component continuation — and, for the
    /// merge consumer, `{#e:X}` unioned with `{#p:Y}` would demand BOTH tags
    /// at once, which matches neither operand.
    #[test]
    fn tag_values_are_one_component_per_tag_name() {
        let mut a = cf();
        a.tags = tag('e', &["x"]);
        a.tags.extend(tag('p', &["y"]));
        let mut b = cf();
        b.tags = tag('e', &["x2"]);
        b.tags.extend(tag('p', &["y2"]));

        assert_eq!(
            differing(&a, &b).len(),
            2,
            "{{#e:X,#p:Y}} vs {{#e:X',#p:Y'}} must be TWO components, never one"
        );
    }

    #[test]
    fn a_tag_name_present_on_only_one_side_is_a_difference() {
        let mut b = cf();
        b.tags = tag('e', &["x"]);
        assert_eq!(
            differing(&cf(), &b),
            vec![Component::Tag(IndexedTagName::new('e').unwrap())]
        );
    }

    /// Two DIFFERENT tag names, one on each side, are two components — the
    /// shape a merge must refuse (`{#e:X}` and `{#p:Y}`).
    #[test]
    fn disjoint_tag_names_are_two_components() {
        let mut a = cf();
        a.tags = tag('e', &["x"]);
        let mut b = cf();
        b.tags = tag('p', &["y"]);
        assert_eq!(differing(&a, &b).len(), 2);
    }

    /// The fast path and the definition must agree on every pair, or the
    /// short-circuit has silently become its own model of what a component
    /// is. Exhaustive over a small filter space rather than sampled: the
    /// space of interest is SHAPES (absent / empty / present, one tag name /
    /// two / shared), and enumerating it leaves nothing to a generator's
    /// luck.
    #[test]
    fn sole_difference_agrees_with_differing() {
        let sets: [Option<BTreeSet<u16>>; 4] = [
            None,
            Some(BTreeSet::new()),
            Some(BTreeSet::from([1])),
            Some(BTreeSet::from([1, 2])),
        ];
        let scalars = [None, Some(1u64), Some(2u64)];
        let tag_shapes: [BTreeMap<IndexedTagName, BTreeSet<String>>; 5] = [
            BTreeMap::new(),
            tag('e', &[]),
            tag('e', &["x"]),
            tag('p', &["y"]),
            {
                let mut both = tag('e', &["x"]);
                both.extend(tag('p', &["y"]));
                both
            },
        ];

        let mut filters: Vec<ConcreteFilter> = Vec::new();
        for kinds in &sets {
            for since in &scalars {
                for tags in &tag_shapes {
                    filters.push(ConcreteFilter {
                        kinds: kinds.clone(),
                        since: *since,
                        tags: tags.clone(),
                        ..ConcreteFilter::default()
                    });
                }
            }
        }
        assert!(
            filters.len() > 50,
            "the enumerated space must be non-trivial"
        );

        let mut agreed_on_one = 0usize;
        for a in &filters {
            for b in &filters {
                let expected = match differing(a, b).as_slice() {
                    [only] => Some(only.clone()),
                    _ => None,
                };
                assert_eq!(
                    sole_difference(a, b),
                    expected,
                    "the short-circuit disagrees with the full difference set\n                       a = {a:?}\n  b = {b:?}\n  differing = {:?}",
                    differing(a, b)
                );
                if expected.is_some() {
                    agreed_on_one += 1;
                }
            }
        }
        assert!(
            agreed_on_one > 0,
            "no enumerated pair differed in exactly one component -- the \
             agreement is vacuous"
        );
    }

    /// The specific trap the short-circuit invites: a tag name present on
    /// BOTH sides with different values must count ONCE. Walking
    /// `a.tags.keys().chain(b.tags.keys())` without filtering visits it
    /// twice, which reads as two components and refuses a merge that should
    /// have happened -- silently, since over-refusing is never a correctness
    /// failure, only a missed collapse.
    #[test]
    fn a_shared_tag_name_with_different_values_counts_once() {
        let mut a = cf();
        a.tags = tag('d', &["group-1"]);
        let mut b = cf();
        b.tags = tag('d', &["group-2"]);
        assert_eq!(
            sole_difference(&a, &b),
            Some(Component::Tag(IndexedTagName::new('d').unwrap()))
        );
    }
}
