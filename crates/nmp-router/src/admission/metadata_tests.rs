//! Direct proofs for `physical_filter_covers`, the containment predicate that
//! decides whether an already-sent immutable REQ selects every event a later
//! candidate asks for. Its behaviour was pinned only through admission
//! integration tests, where a wrong answer shows up as a REQ count rather than
//! as a statement about the axis that decided it.
//!
//! Containment is per axis and NOT symmetric with coalescing: coalescing
//! unions two filters into a wider one, while this predicate asks whether one
//! side is already wide enough. Every axis therefore has both legs proven —
//! the covering direction and the direction that must refuse.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{ConcreteFilter, IndexedTagName};

use super::metadata::physical_filter_covers;

fn strings<const N: usize>(values: [&str; N]) -> BTreeSet<String> {
    values.into_iter().map(str::to_owned).collect()
}

fn tag(name: char) -> IndexedTagName {
    IndexedTagName::new(name).expect("test tag names are ASCII letters")
}

fn tagged<const N: usize>(
    name: char,
    values: [&str; N],
) -> BTreeMap<IndexedTagName, BTreeSet<String>> {
    BTreeMap::from([(tag(name), strings(values))])
}

#[test]
fn a_wider_kind_set_covers_a_subset_and_refuses_a_non_subset() {
    let physical = ConcreteFilter {
        kinds: Some(BTreeSet::from([0, 1, 7])),
        ..ConcreteFilter::default()
    };
    let covered = ConcreteFilter {
        kinds: Some(BTreeSet::from([1, 7])),
        ..ConcreteFilter::default()
    };
    let uncovered = ConcreteFilter {
        kinds: Some(BTreeSet::from([1, 30023])),
        ..ConcreteFilter::default()
    };
    assert!(physical_filter_covers(&physical, &covered));
    assert!(!physical_filter_covers(&physical, &uncovered));
}

#[test]
fn a_wider_author_set_covers_a_subset_and_refuses_a_non_subset() {
    let physical = ConcreteFilter {
        authors: Some(strings(["alice", "bob"])),
        ..ConcreteFilter::default()
    };
    let covered = ConcreteFilter {
        authors: Some(strings(["alice"])),
        ..ConcreteFilter::default()
    };
    let uncovered = ConcreteFilter {
        authors: Some(strings(["alice", "carol"])),
        ..ConcreteFilter::default()
    };
    assert!(physical_filter_covers(&physical, &covered));
    assert!(!physical_filter_covers(&physical, &uncovered));
}

#[test]
fn a_wider_id_set_covers_a_subset_and_refuses_a_non_subset() {
    let physical = ConcreteFilter {
        ids: Some(strings(["aa", "bb"])),
        ..ConcreteFilter::default()
    };
    let covered = ConcreteFilter {
        ids: Some(strings(["bb"])),
        ..ConcreteFilter::default()
    };
    let uncovered = ConcreteFilter {
        ids: Some(strings(["bb", "cc"])),
        ..ConcreteFilter::default()
    };
    assert!(physical_filter_covers(&physical, &covered));
    assert!(!physical_filter_covers(&physical, &uncovered));
}

/// An UNCONSTRAINED physical axis covers any candidate on that axis, and an
/// unconstrained CANDIDATE axis is never covered by a constrained physical
/// one. This is the asymmetry `option_set_covers` exists for: `None` on the
/// physical side means "every event", `None` on the candidate side means the
/// candidate wants events the physical filter never asked for.
#[test]
fn an_unconstrained_axis_covers_downward_only() {
    let unconstrained = ConcreteFilter::default();
    let constrained = ConcreteFilter {
        authors: Some(strings(["alice"])),
        ..ConcreteFilter::default()
    };
    assert!(physical_filter_covers(&unconstrained, &constrained));
    assert!(!physical_filter_covers(&constrained, &unconstrained));
}

/// Tags are conjunctive across NAMES, so the polarity inverts: every tag name
/// the physical filter constrains must also be constrained by the candidate,
/// at a subset of values. A candidate missing that name asks for events the
/// physical filter excluded.
#[test]
fn a_wider_tag_value_set_covers_a_subset_and_refuses_a_non_subset() {
    let physical = ConcreteFilter {
        tags: tagged('d', ["group-a", "group-b"]),
        ..ConcreteFilter::default()
    };
    let covered = ConcreteFilter {
        tags: tagged('d', ["group-a"]),
        ..ConcreteFilter::default()
    };
    let uncovered = ConcreteFilter {
        tags: tagged('d', ["group-a", "group-c"]),
        ..ConcreteFilter::default()
    };
    assert!(physical_filter_covers(&physical, &covered));
    assert!(!physical_filter_covers(&physical, &uncovered));
}

#[test]
fn a_candidate_missing_a_constrained_tag_name_is_never_covered() {
    let physical = ConcreteFilter {
        tags: tagged('d', ["group-a"]),
        ..ConcreteFilter::default()
    };
    let untagged = ConcreteFilter::default();
    let other_name = ConcreteFilter {
        tags: tagged('e', ["group-a"]),
        ..ConcreteFilter::default()
    };
    assert!(!physical_filter_covers(&physical, &untagged));
    assert!(!physical_filter_covers(&physical, &other_name));
    // The reverse direction is covered: an unconstrained tag axis on the
    // physical side selects tagged and untagged events alike.
    assert!(physical_filter_covers(&untagged, &physical));
}

#[test]
fn a_wider_time_window_covers_a_narrower_one_and_refuses_a_wider_candidate() {
    let physical = ConcreteFilter {
        since: Some(100),
        until: Some(200),
        ..ConcreteFilter::default()
    };
    let covered = ConcreteFilter {
        since: Some(120),
        until: Some(180),
        ..ConcreteFilter::default()
    };
    let earlier_since = ConcreteFilter {
        since: Some(99),
        until: Some(180),
        ..ConcreteFilter::default()
    };
    let later_until = ConcreteFilter {
        since: Some(120),
        until: Some(201),
        ..ConcreteFilter::default()
    };
    let unbounded = ConcreteFilter::default();
    assert!(physical_filter_covers(&physical, &covered));
    assert!(!physical_filter_covers(&physical, &earlier_since));
    assert!(!physical_filter_covers(&physical, &later_until));
    assert!(!physical_filter_covers(&physical, &unbounded));
    assert!(physical_filter_covers(&unbounded, &physical));
}

/// A `limit` caps the RESULT COUNT, not the predicate, so it is not a set axis
/// and containment cannot be reconstructed for a later owner. Both sides
/// refuse — including the case where the two filters are byte-identical, which
/// every set axis would otherwise call covered.
#[test]
fn a_limit_on_either_side_refuses_even_an_identical_filter() {
    let limited = ConcreteFilter {
        authors: Some(strings(["alice"])),
        limit: Some(200),
        ..ConcreteFilter::default()
    };
    let unlimited = ConcreteFilter {
        authors: Some(strings(["alice"])),
        ..ConcreteFilter::default()
    };
    assert!(!physical_filter_covers(&limited, &limited.clone()));
    assert!(!physical_filter_covers(&limited, &unlimited));
    assert!(!physical_filter_covers(&unlimited, &limited));
    assert!(physical_filter_covers(&unlimited, &unlimited.clone()));
}
