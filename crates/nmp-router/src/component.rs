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
}
