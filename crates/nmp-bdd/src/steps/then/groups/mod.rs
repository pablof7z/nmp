//! Assertions about the NIP-29 `Group` door: what it put on which host's
//! socket, what the delivered event literally was, and what it refused.
//!
//! Its own family because the domain is its own. [`super::writes`] asks where
//! a publish was routed and what its receipt said; these steps ask a narrower
//! and stricter question -- WHICH HOST, and NO OTHER, and WITH WHICH TAG --
//! and answer it from the relay's own record of the bytes it was handed
//! rather than only from the receipt. Two witnesses again, for the same
//! reason [`super::routing`] uses two: a `must-never` claim about a group
//! write must not rest on the engine's self-report alone.
//!
//! A handful of claims here are about the SHAPE of the door rather than about
//! a run ("no group write operation accepts a relay"). Absence of a parameter
//! is not observable from any execution -- a scenario that never passed a
//! relay proves nothing about whether it could have -- so those read the
//! door's own declaration, which is the same evidence
//! `scripts/check-nip29-ownership.sh` uses.
//!
//! The families below split by the DOMAIN each claim is about, the same seam
//! [`super`] itself uses: what reached which host, how the route was chosen and
//! what the receipt said, the identity's own promises, kind blindness and the
//! gate that enforces it, the reads, the refusals, the signing boundary, the
//! door's declared shape, and the rows NIP-29's own operations carry.

mod blindness;
mod identity;
mod operations;
mod publish_queue;
mod reads;
mod refusals;
mod route;
mod signing;
mod surface;

use nmp::mechanism::publish_queue::{RelayState, SigningState, WriteFact, WriteOutcome};

use crate::world::NmpWorld;

// ---- shared helpers ------------------------------------------------------

/// Let the publication under test reach a terminal beat, then let every
/// relay's client wire go quiet.
///
/// EVERY negative claim in this file goes through here first. "Relay X
/// received no event" read mid-flight is a statement about when the step ran,
/// not about where the write went, and it is green for the wrong reason
/// exactly when it matters most.
async fn settled(w: &mut NmpWorld) {
    if w.receipt_count() > 0 {
        // Exactly one `Outcome` ends every receipt stream, so the terminal
        // beat is now a single fact rather than a list of per-relay ones
        // that had to be kept in step with the vocabulary by hand.
        w.receipt_eventually(|seen| seen.iter().any(|s| matches!(s, WriteFact::Outcome(_))));
    }
    w.wire_settled().await;
}

/// The event the group publication actually put on its host's socket.
async fn delivered(w: &mut NmpWorld) -> nostr::Event {
    let host = w.group_host_name(None);
    w.delivered_event_at(&host).await.unwrap_or_else(|| {
        panic!(
            "nmp-bdd: nothing reached the group's host {host:?}; receipt showed {:?}",
            w.receipt_statuses()
        )
    })
}

/// Every `["<name>", ...]` row of an event, as plain strings.
fn rows(event: &nostr::Event) -> Vec<Vec<String>> {
    event
        .tags
        .iter()
        .map(|tag| tag.as_slice().to_vec())
        .collect()
}

fn values_of(event: &nostr::Event, name: &str) -> Vec<String> {
    rows(event)
        .into_iter()
        .filter(|row| row.first().map(String::as_str) == Some(name))
        .map(|row| row.get(1).cloned().unwrap_or_default())
        .collect()
}

/// The NIP-01 id of `event`'s frozen fields with `tags` substituted -- the one
/// arithmetic these scenarios do for themselves, because "the h row is inside
/// the signed bytes" is only checkable by hashing with and without it.
fn event_id_over(event: &nostr::Event, tags: Vec<nostr::Tag>) -> nostr::EventId {
    nostr::EventId::new(
        &event.pubkey,
        &event.created_at,
        &event.kind,
        &nostr::Tags::from_list(tags),
        &event.content,
    )
}

/// No method the trait declares may name any of `forbidden`.
fn assert_no_parameter(surface: &crate::world::GroupSurface, forbidden: &[&str], what: &str) {
    assert!(
        !surface.write_signatures.is_empty(),
        "NOTHING TO OBSERVE -- no group write operation was found to inspect"
    );
    for signature in &surface.write_signatures {
        for needle in forbidden {
            assert!(
                !signature.contains(needle),
                "no group write operation accepts {what}, but {signature:?} names {needle:?}"
            );
        }
    }
}

/// Neither the pure door nor its engine binding may grow a read verb.
fn assert_no_read_door(surface: &crate::world::GroupSurface) {
    for source in [&surface.door, &surface.binding] {
        for forbidden in ["fn observe", "fn subscribe", "fn stream"] {
            assert!(
                !source.contains(forbidden),
                "the one read door is Engine::observe; a group must not declare {forbidden:?}"
            );
        }
    }
}
