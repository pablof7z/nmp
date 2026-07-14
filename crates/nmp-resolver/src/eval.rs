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
///   is attached to.
pub(crate) fn merge_element_into(cf: &mut ConcreteFilter, slot: &FieldSlot, el: &Element) {
    match el {
        Element::Coord { kind, author, d } => {
            cf.kinds.get_or_insert_with(BTreeSet::new).insert(*kind);
            cf.authors
                .get_or_insert_with(BTreeSet::new)
                .insert(author.clone());
            let d_tag = IndexedTagName::new('d').expect("'d' is an ASCII letter");
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
/// operand minus the union of the rest (bug-class ledger #11: "follows
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

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    /// An arbitrary caller-owned kind, not `Kind::TextNote`/any other
    /// NIP-01 core schema -- `Selector::Tag`'s local projection is
    /// protocol-neutral grammar mechanics, and a `kind:1`-flavored fixture
    /// would reintroduce exactly the kind bias the v2 docs reject.
    const ARBITRARY_CALLER_KIND: u16 = 9999;

    fn note_with_tags(tags: Vec<Tag>) -> StoredEvent {
        let keys = Keys::generate();
        StoredEvent {
            event: EventBuilder::new(Kind::Custom(ARBITRARY_CALLER_KIND), "hi")
                .tags(tags)
                .sign_with_keys(&keys)
                .expect("test fixture must sign cleanly"),
            provenance: nmp_store::Provenance::default(),
        }
    }

    fn observed(mut stored: StoredEvent, relays: &[&str]) -> StoredEvent {
        for (index, relay) in relays.iter().enumerate() {
            stored.provenance.seen.insert(
                nostr::RelayUrl::parse(relay).unwrap(),
                nostr::Timestamp::from(index as u64),
            );
        }
        stored
    }

    /// `Selector::Tag` is a purely local projection over already-acquired
    /// events (#64) — it must project multi-character/punctuation event-tag
    /// names exactly as it would a single-letter one, never rejecting them
    /// as "unknown".
    #[test]
    fn tag_selector_projects_arbitrary_multi_character_event_tag_names() {
        let event = note_with_tags(vec![
            Tag::parse(["poop", "value1"]).unwrap(),
            Tag::parse(["-", "value2"]).unwrap(),
            Tag::parse(["alt", "value3"]).unwrap(),
        ]);
        for (name, expected) in [("poop", "value1"), ("-", "value2"), ("alt", "value3")] {
            let set = project_events(
                std::slice::from_ref(&event),
                &Selector::Tag(name.to_string()),
            );
            assert_eq!(
                set,
                ResolvedSet::from([Element::Scalar(expected.to_string())])
            );
        }
    }

    /// Every ASCII letter is a valid `Selector::Tag` key, not just the old
    /// hard-coded M1 set -- `x`/`Z` are the structural (not whitelist)
    /// witnesses (#64 acceptance evidence).
    #[test]
    fn tag_selector_matches_previously_unlisted_letters() {
        for c in ['x', 'Z'] {
            let event = note_with_tags(vec![Tag::parse([c.to_string(), "v".to_string()]).unwrap()]);
            let set = project_events(&[event], &Selector::Tag(c.to_string()));
            assert_eq!(set, ResolvedSet::from([Element::Scalar("v".to_string())]));
        }
    }

    /// Lowercase and uppercase tag names are distinct keys -- `e` and `E`
    /// must not be folded together by the projection.
    #[test]
    fn tag_selector_is_case_and_spelling_exact() {
        let event = note_with_tags(vec![
            Tag::parse(["e", "lower"]).unwrap(),
            Tag::parse(["E", "upper"]).unwrap(),
        ]);
        let lower = project_events(
            std::slice::from_ref(&event),
            &Selector::Tag("e".to_string()),
        );
        let upper = project_events(&[event], &Selector::Tag("E".to_string()));
        assert_eq!(
            lower,
            ResolvedSet::from([Element::Scalar("lower".to_string())])
        );
        assert_eq!(
            upper,
            ResolvedSet::from([Element::Scalar("upper".to_string())])
        );
    }

    #[test]
    fn reference_tags_carry_explicit_hint_or_source_provenance_fallback() {
        let hinted = "wss://hint.example";
        let source = "wss://source.example";
        let values = [
            ("e", "11".repeat(32)),
            ("p", "22".repeat(32)),
            ("a", format!("30023:{}:slug", "33".repeat(32))),
        ];
        for (name, value) in values {
            let explicit = observed(
                note_with_tags(vec![Tag::parse([name, value.as_str(), hinted]).unwrap()]),
                &[source],
            );
            let set = project_events(&[explicit], &Selector::Tag(name.to_string()));
            let (_, evidence) = set.iter().next().unwrap();
            assert_eq!(
                evidence,
                &BTreeSet::from([RoutingEvidence {
                    relay: nostr::RelayUrl::parse(hinted).unwrap(),
                    origin: RoutingEvidenceKind::Hint,
                }]),
                "an explicit {name}-tag hint suppresses provenance fallback"
            );

            let fallback = observed(
                note_with_tags(vec![Tag::parse([name, value.as_str()]).unwrap()]),
                &[source],
            );
            let set = project_events(&[fallback], &Selector::Tag(name.to_string()));
            let (_, evidence) = set.iter().next().unwrap();
            assert_eq!(
                evidence,
                &BTreeSet::from([RoutingEvidence {
                    relay: nostr::RelayUrl::parse(source).unwrap(),
                    origin: RoutingEvidenceKind::SourceProvenance,
                }])
            );
        }
    }

    #[test]
    fn duplicate_projected_values_union_evidence_and_setops_preserve_it() {
        let value = "44".repeat(32);
        let one = observed(
            note_with_tags(vec![Tag::parse(["e", value.as_str()]).unwrap()]),
            &["wss://one.example"],
        );
        let two = observed(
            note_with_tags(vec![Tag::parse(["e", value.as_str()]).unwrap()]),
            &["wss://two.example"],
        );
        let projected = project_events(&[one, two], &Selector::Tag("e".to_string()));
        assert_eq!(projected.iter().next().unwrap().1.len(), 2);

        let intersected = resolve_setop(SetAlgebra::Intersect, &[&projected, &projected]);
        assert_eq!(intersected, projected);
        let removed = resolve_setop(SetAlgebra::Diff, &[&projected, &projected]);
        assert!(removed.is_empty());
    }

    #[test]
    fn address_coord_uses_source_provenance() {
        let event = observed(note_with_tags(Vec::new()), &["wss://coord.example"]);
        let projected = project_events(&[event], &Selector::AddressCoord);
        let (_, evidence) = projected.iter().next().unwrap();
        assert_eq!(
            evidence.iter().next().unwrap().origin,
            RoutingEvidenceKind::SourceProvenance
        );
    }
}
