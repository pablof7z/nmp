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

