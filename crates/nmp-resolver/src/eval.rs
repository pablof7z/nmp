//! Pure evaluation functions: merging a resolved [`Element`] into a
//! [`ConcreteFilter`], projecting queried events through a [`Selector`], and
//! folding a [`SetAlgebra`] over operand [`ResolvedSet`]s, plus resolving
//! the reactive identity root. None of these touch the store or the graph —
//! they are the leaf computations the graph's recompute machinery
//! (`engine.rs`) calls.
//!
//! **Kill-guard note (M1 plan §3.3 step 2 / test 10):** nothing in this
//! module (or anywhere else in `src/`) branches on an event's `kind` value.
//! [`project_events`] and [`merge_element_into`] dispatch only on the
//! grammar's own closed vocabulary (`Selector`, `FieldSlot`, `Element`) —
//! structural dispatch over a type, never a literal kind comparison.

use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, IdentityField, Selector, SetAlgebra, TagName};

use crate::types::{Element, FieldSlot, ResolvedSet};

/// Merge one resolved element into `cf`, per its shape:
///
/// - `Element::Coord` co-pins `kinds`/`authors`/`tags['d']` together (M1
///   plan §3.5) — regardless of which grammar field slot nominally carries
///   the binding, since an address coordinate is never a single-field
///   value.
/// - `Element::Scalar` is written into exactly the one `slot` this binding
///   is attached to.
pub(crate) fn merge_element_into(cf: &mut ConcreteFilter, slot: &FieldSlot, el: &Element) {
    match el {
        Element::Coord { kind, author, d } => {
            cf.kinds.get_or_insert_with(BTreeSet::new).insert(*kind);
            cf.authors
                .get_or_insert_with(BTreeSet::new)
                .insert(author.clone());
            let d_tag = TagName::new('d').expect("'d' is in M1's valid TagName set");
            cf.tags.entry(d_tag).or_default().insert(d.clone());
        }
        Element::Scalar(s) => match slot {
            FieldSlot::Authors => {
                cf.authors
                    .get_or_insert_with(BTreeSet::new)
                    .insert(s.clone());
            }
            FieldSlot::Ids => {
                cf.ids.get_or_insert_with(BTreeSet::new).insert(s.clone());
            }
            FieldSlot::Tag(t) => {
                cf.tags.entry(*t).or_default().insert(s.clone());
            }
        },
    }
}

/// The single legit kind-value read in this crate: `Selector::AddressCoord`
/// projects an event's kind INTO the `(kind, author, d)` coordinate it
/// contributes to a `ResolvedSet` -- a data projection, not a routing
/// branch (M1 verification review nit 1 / M2 plan §8.1). Scoped narrowly to
/// this one-line helper (rather than the whole `project_events` function,
/// and taking the whole `&Event` so the field read itself stays inside the
/// annotated/marked line too) so the `#[allow]` cannot silently cover an
/// unrelated kind-branch added later; `nmp-resolver/tests/no_kind_branches.rs`
/// additionally asserts this is the ONLY `KIND-VALUE-READ`-marked site in
/// the crate (or its `nmp-router` sibling).
#[allow(clippy::disallowed_methods)]
fn kind_value_for_coord_projection(event: &nostr::Event) -> u16 {
    event.kind.as_u16() // KIND-VALUE-READ: projection into Element::Coord, not a routing branch
}

/// Project a batch of queried events through `project`, per the closed
/// [`Selector`] vocabulary. A single event may contribute zero, one, or
/// several elements (e.g. an event with multiple `p` tags contributes one
/// `Element::Scalar` per tag value).
pub(crate) fn project_events(events: &[nostr::Event], project: &Selector) -> ResolvedSet {
    let mut out = ResolvedSet::new();
    for event in events {
        match project {
            Selector::Authors => {
                out.insert(Element::Scalar(event.pubkey.to_hex()));
            }
            Selector::Ids => {
                out.insert(Element::Scalar(event.id.to_hex()));
            }
            Selector::Tag(tag) => {
                let single = nostr::SingleLetterTag::from_char(tag.as_char())
                    .expect("TagName is pre-validated against M1's closed single-letter set");
                for t in event.tags.iter() {
                    if t.single_letter_tag() == Some(single) {
                        if let Some(value) = t.content() {
                            out.insert(Element::Scalar(value.to_string()));
                        }
                    }
                }
            }
            Selector::AddressCoord => {
                out.insert(Element::Coord {
                    kind: kind_value_for_coord_projection(event),
                    author: event.pubkey.to_hex(),
                    d: event.tags.identifier().unwrap_or("").to_string(),
                });
            }
        }
    }
    out
}

/// Resolve `Binding::Reactive` from the identity register. `None` (identity
/// unset) resolves to the empty set — never a wildcard (M1 plan §3.4
/// invariant: empty set != wildcard).
pub(crate) fn resolve_reactive(
    field: IdentityField,
    identity: Option<nostr::PublicKey>,
) -> ResolvedSet {
    match field {
        IdentityField::ActivePubkey => match identity {
            Some(pk) => ResolvedSet::from([Element::Scalar(pk.to_hex())]),
            None => ResolvedSet::new(),
        },
    }
}

/// Fold a `SetAlgebra` over resolved operand sets. `Diff` is the first
/// operand minus the union of the rest (bug-class ledger #11: "follows
/// MINUS mutes").
pub(crate) fn resolve_setop(op: SetAlgebra, operands: &[&ResolvedSet]) -> ResolvedSet {
    match op {
        SetAlgebra::Union => operands.iter().fold(ResolvedSet::new(), |mut acc, s| {
            acc.extend(s.iter().cloned());
            acc
        }),
        SetAlgebra::Intersect => {
            let mut iter = operands.iter();
            match iter.next() {
                None => ResolvedSet::new(),
                Some(first) => iter.fold((*first).clone(), |acc, s| {
                    acc.intersection(s).cloned().collect()
                }),
            }
        }
        SetAlgebra::Diff => {
            let mut iter = operands.iter();
            match iter.next() {
                None => ResolvedSet::new(),
                Some(first) => {
                    let rest_union = iter.fold(ResolvedSet::new(), |mut acc, s| {
                        acc.extend(s.iter().cloned());
                        acc
                    });
                    (*first).difference(&rest_union).cloned().collect()
                }
            }
        }
    }
}
