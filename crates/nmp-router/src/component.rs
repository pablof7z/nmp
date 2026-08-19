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
//!   compiled byte-changing filter names as its predecessor: the one it
//!   differs from in exactly one component, zero-diff ranked first. Exact
//!   zero-diff keeps its token; changed bytes receive a fresh token.
//!
//! **They share this module deliberately, and the sharing is load-bearing
//! rather than incidental.** The shared component model lets coalescing and
//! predecessor selection agree on which single logical axis changed. A
//! byte-changing successor still receives a fresh token and is offered before
//! its predecessor closes; the component match identifies that transition,
//! never an in-place overwrite. One definition makes the agreement structural.
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

