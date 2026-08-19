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

use nmp_grammar::{
    ConcreteFilter, IdentityField, IndexedTagName, RoutingEvidence, RoutingEvidenceKind, Selector,
    SetAlgebra,
};
use nmp_store::StoredEvent;

use crate::types::{Element, FieldSlot, ResolvedSet};

/// Merge one resolved element into `cf`, per its shape:
///
/// - `Element::Coord` co-pins `kinds`/`authors`/`tags['d']` together (M1
///   plan §3.5) — regardless of which grammar field slot nominally carries
///   the binding, since an address coordinate is never a single-field
///   value.
/// - `Element::Scalar` is written into exactly the one `slot` this binding
///   is attached to. Author/id destinations validate and canonicalize their
///   protocol types here, where the destination semantics are known; an
///   invalid scalar contributes no value.
///
/// Returns `true` iff the element contributed a value. Callers must treat a
/// bound slot with zero contributing elements as matching nothing, never as
/// an unconstrained filter.
pub(crate) fn merge_element_into(cf: &mut ConcreteFilter, slot: &FieldSlot, el: &Element) -> bool {
    match el {
        Element::Coord { kind, author, d } => {
            cf.kinds.get_or_insert_with(BTreeSet::new).insert(*kind);
            cf.authors
                .get_or_insert_with(BTreeSet::new)
                .insert(author.clone());
            let d_tag = IndexedTagName::new('d').expect("'d' is an ASCII letter");
            cf.tags.entry(d_tag).or_default().insert(d.clone());
            true
        }
        Element::Scalar(s) => match slot {
            FieldSlot::Authors => match nostr::PublicKey::from_hex(s) {
                Ok(author) => {
                    cf.authors
                        .get_or_insert_with(BTreeSet::new)
                        .insert(author.to_hex());
                    true
                }
                Err(_) => false,
            },
            FieldSlot::Ids => match nostr::EventId::from_hex(s) {
                Ok(id) => {
                    cf.ids.get_or_insert_with(BTreeSet::new).insert(id.to_hex());
                    true
                }
                Err(_) => false,
            },
            FieldSlot::Tag(t) => {
                cf.tags.entry(*t).or_default().insert(s.clone());
                true
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
pub(crate) fn project_events(events: &[StoredEvent], project: &Selector) -> ResolvedSet {
    let mut out = ResolvedSet::new();
    for stored in events {
        let event = &stored.event;
        match project {
            Selector::Authors => {
                out.insert(Element::Scalar(event.pubkey.to_hex()));
            }
            Selector::Ids => {
                out.insert(Element::Scalar(event.id.to_hex()));
            }
            Selector::Tag(name) => {
                // `name` is an arbitrary event-tag key (#64) -- NOT
                // restricted to the single-letter wire-filter alphabet
                // (`nostr::SingleLetterTag`). This is a purely local
                // projection over already-acquired events, so it matches the
                // tag array's raw name slot (index 0, same as `Tag::kind()`
                // reads internally) directly -- case- and spelling-exact for
                // both single-letter and multi-character/custom tag names --
                // rather than going through `single_letter_tag()`.
                for t in event.tags.iter() {
                    if t.as_slice().first().map(String::as_str) == Some(name.as_str()) {
                        if let Some(value) = t.content() {
                            let explicit_hint = matches!(name.as_str(), "e" | "a" | "p")
                                .then(|| t.as_slice().get(2))
                                .flatten()
                                .and_then(|raw| nostr::RelayUrl::parse(raw).ok())
                                .map(|relay| RoutingEvidence {
                                    relay,
                                    origin: RoutingEvidenceKind::Hint,
                                });
                            let evidence: Vec<RoutingEvidence> = match explicit_hint {
                                Some(hint) => vec![hint],
                                None if matches!(name.as_str(), "e" | "a" | "p") => stored
                                    .provenance
                                    .seen
                                    .keys()
                                    .cloned()
                                    .map(|relay| RoutingEvidence {
                                        relay,
                                        origin: RoutingEvidenceKind::SourceProvenance,
                                    })
                                    .collect(),
                                None => Vec::new(),
                            };
                            out.insert_with(Element::Scalar(value.to_string()), evidence);
                        }
                    }
                }
            }
            Selector::AddressCoord => {
                out.insert_with(
                    Element::Coord {
                        kind: kind_value_for_coord_projection(event),
                        author: event.pubkey.to_hex(),
                        d: event.tags.identifier().unwrap_or("").to_string(),
                    },
                    stored
                        .provenance
                        .seen
                        .keys()
                        .cloned()
                        .map(|relay| RoutingEvidence {
                            relay,
                            origin: RoutingEvidenceKind::SourceProvenance,
                        }),
                );
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
/// operand minus the union of the rest (guarantee #11: "follows
/// MINUS mutes").
pub(crate) fn resolve_setop(op: SetAlgebra, operands: &[&ResolvedSet]) -> ResolvedSet {
    match op {
        SetAlgebra::Union => operands.iter().fold(ResolvedSet::new(), |mut acc, s| {
            acc.merge_from(s);
            acc
        }),
        SetAlgebra::Intersect => {
            let mut iter = operands.iter();
            match iter.next() {
                None => ResolvedSet::new(),
                Some(first) => iter.fold((*first).clone(), |mut acc, s| {
                    let missing: Vec<Element> = acc
                        .iter()
                        .filter_map(|(element, _)| {
                            (!s.contains(element)).then_some(element.clone())
                        })
                        .collect();
                    for element in missing {
                        acc.remove(&element);
                    }
                    for (element, evidence) in s.iter() {
                        if acc.contains(element) {
                            acc.insert_with(element.clone(), evidence.iter().cloned());
                        }
                    }
                    acc
                }),
            }
        }
        SetAlgebra::Diff => {
            let mut iter = operands.iter();
            match iter.next() {
                None => ResolvedSet::new(),
                Some(first) => {
                    let mut out = (*first).clone();
                    for other in iter {
                        for (element, _) in other.iter() {
                            out.remove(element);
                        }
                    }
                    out
                }
            }
        }
    }
}

