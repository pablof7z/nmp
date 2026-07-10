//! The local re-filter + the headless delivery model (M2 plan §4.3).
//!
//! Widen-only (`coalesce.rs`) guarantees no UNDER-delivery: the wire filter
//! matches at least every consuming atom's events. This module guarantees
//! no OVER-delivery: each event a relay returns for a (possibly coalesced)
//! wire filter is re-matched against each CONSUMING atom's own original
//! `ConcreteFilter` before delivery to that consumer. State both
//! directions explicitly — they are the two halves that make coalescing
//! non-load-bearing (VISION §6 Q1(b)).

use nostr::filter::MatchEventOptions;

use nmp_grammar::ConcreteFilter;

/// Re-filter `events` (whatever a relay returned for some wire filter) down
/// to exactly the events matching `atom`. Never hand-rolled matching --
/// reuses `nostr::Filter::match_event` (memory rule: use rust-nostr, not
/// scratch logic).
pub fn deliver<'a>(events: &'a [nostr::Event], atom: &ConcreteFilter) -> Vec<&'a nostr::Event> {
    let nf = atom.to_nostr();
    events
        .iter()
        .filter(|e| nf.match_event(e, MatchEventOptions::new()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    use nostr::{EventBuilder, Keys, Kind};

    fn atom_for(kinds: &[u16], author_hex: &str) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(kinds.iter().copied().collect()),
            authors: Some(BTreeSet::from([author_hex.to_string()])),
            ..ConcreteFilter::default()
        }
    }

    #[test]
    fn local_refilter_yields_exactly_the_atom_matches_no_over_no_under() {
        let a_keys = Keys::generate();
        let b_keys = Keys::generate();

        let a_event = EventBuilder::new(Kind::TextNote, "hello a")
            .sign_with_keys(&a_keys)
            .unwrap();
        let b_event = EventBuilder::new(Kind::TextNote, "hello b")
            .sign_with_keys(&b_keys)
            .unwrap();

        // A wire filter widened to cover both A and B (what a coalesced
        // AuthorUnion REQ would look like).
        let wire_events = vec![a_event.clone(), b_event.clone()];

        let atom_a = atom_for(&[1], &a_keys.public_key().to_hex());
        let delivered_to_a = deliver(&wire_events, &atom_a);
        assert_eq!(
            delivered_to_a,
            vec![&a_event],
            "no over-delivery to A's consumer"
        );

        let atom_b = atom_for(&[1], &b_keys.public_key().to_hex());
        let delivered_to_b = deliver(&wire_events, &atom_b);
        assert_eq!(
            delivered_to_b,
            vec![&b_event],
            "no over-delivery to B's consumer"
        );
    }
}
