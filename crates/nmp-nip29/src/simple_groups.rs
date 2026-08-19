//! kind:10009 -- NIP-51's Simple groups list, exposed by the `nmp-nip29`
//! product capability
//! (#63/#108/#1551). This module lives in the NIP-29 product capability; that
//! packaging choice does not change which protocol defines the event.
//! read-only codec over `nostr::Event`'s own `Tag`/`Tags` accessors
//! (rust-nostr has no kind:10009 helper of its own -- unlike NIP-19's bech32
//! module, there is no existing implementation to adapt here; memory rule
//! "use rust-nostr, not scratch crypto" is still honored: no hand-rolled
//! tag/relay-url parsing, `RelayUrl::parse` and `Tag::kind()`/
//! `Tag::as_slice()` do the actual work).
//!
//! # This module produces data, never authority (#863)
//!
//! Every function here accepts UNTRUSTED, possibly caller-fabricated tags and
//! content, and returns a plain value. A [`SimpleGroupsList`] carries NO
//! claim of signature validity, canonical-store membership, relay provenance,
//! routing permission, or mutation authority -- the `_tolerant` suffix in
//! each entry point's name says exactly that at every call site. Anything
//! that needs such authority must establish it inside the concrete operation
//! whose precondition requires it; this crate deliberately mints no
//! observation handle, frame proof, witness, or qualified wrapper for a
//! parser result to ride on.
//!
//! Write-side replacement encoding is deliberately NOT here: this file stays
//! a pure, engine-free reader. The app-facing typed add/remove operations
//! live in `crate::group_list_writes`, which compiles private versioned
//! operation bytes into the ordinary durable `WriteIntent` and receipt
//! lifecycle. They never promote this observational value into a reusable
//! authority noun.

use nostr::{Event, RelayUrl};

/// One parsed `["group", <id>, <relay>, <name>?]` item from a
/// Simple-groups-shaped event -- exactly the three fields #63 names: group
/// id, host relay, and an optional name. `host_relay` is canonicalized via
/// `RelayUrl::parse` (#108 Done-when: "decoded kind-10009 rows produce
/// canonical remembered relay hosts") -- canonical SPELLING only. It is an
/// observed string, not a permission to route anywhere (#863).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleGroupEntry {
    pub group_id: String,
    pub host_relay: RelayUrl,
    pub name: Option<String>,
}

/// A tolerantly parsed Simple-groups list -- OBSERVATIONAL DATA, not
/// authority (#863). It may have come from a fully fabricated, unsigned,
/// wrong-kind input; nothing about this type asserts otherwise, and no
/// consumer may read it as proof of provenance, canonical state, or routing
/// permission. `items`/`relays_in_use` preserve the tag
/// array's EXACT order (#63: "preserve exact ordering") -- a `Vec`, never a
/// `Set`/`Map` that would silently re-sort or dedupe a user's own list
/// ordering. `malformed_item_count` and `has_private_content` are evidence
/// fields, never silent drops: a `"group"` tag too short to carry an id+
/// relay, or one whose relay fails to canonicalize, is skipped but COUNTED
/// rather than either aborting the whole decode or vanishing without
/// trace; a non-empty `content` field means the event carries NIP-51
/// PRIVATE (encrypted) items this pure codec has no signer/decrypt
/// capability to reach -- `has_private_content` says so honestly rather
/// than silently reporting a public-only list as complete.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SimpleGroupsList {
    pub items: Vec<SimpleGroupEntry>,
    pub relays_in_use: Vec<RelayUrl>,
    pub malformed_item_count: usize,
    pub has_private_content: bool,
}

/// Tolerantly parse an event's PUBLIC (tag-carried) Simple-groups items.
/// Never fails -- a malformed individual tag is skipped and counted (see
/// [`SimpleGroupsList::malformed_item_count`]), never treated as a reason
/// to discard the whole list. The event's `kind` is NOT re-validated here
/// and the event's signature is NOT consulted: this is a tolerant reader of
/// whatever the caller hands it, and its result is data with no authority
/// attached (#863). The `_tolerant` suffix is the name-level statement of
/// that.
pub fn parse_simple_groups_list_tolerant(event: &Event) -> SimpleGroupsList {
    parse_simple_groups_list_from_raw_tags_tolerant(
        event.tags.iter().map(|tag| tag.as_slice()),
        &event.content,
    )
}

/// The same tolerant parse, over raw `[tag_name, values...]` string arrays
/// rather than a `nostr::Event` -- the shape `nmp-ffi`'s `FfiRow.tags`
/// already carries (a delivered row's raw tokens, guarantee #12). Sharing this
/// core with [`parse_simple_groups_list_tolerant`] (which just adapts a real
/// `Event`'s `Tag`s into the same `&[String]` shape via `Tag::as_slice`)
/// means the FFI boundary never needs to reconstruct a full signed
/// `nostr::Event` just to re-read tags it already received as raw strings.
/// Equally non-authoritative: the caller may have invented every byte.
pub fn parse_simple_groups_list_from_raw_tags_tolerant<'a>(
    tags: impl IntoIterator<Item = &'a [String]>,
    content: &str,
) -> SimpleGroupsList {
    let mut items = Vec::new();
    let mut relays_in_use = Vec::new();
    let mut malformed_item_count = 0usize;

    for slice in tags {
        match slice.first().map(String::as_str) {
            Some("group") => {
                // `["group", id, relay, name?]` -- id + relay are
                // required, name is optional (#63).
                let Some(group_id) = slice.get(1) else {
                    malformed_item_count += 1;
                    continue;
                };
                let Some(relay_str) = slice.get(2) else {
                    malformed_item_count += 1;
                    continue;
                };
                let Ok(host_relay) = RelayUrl::parse(relay_str) else {
                    malformed_item_count += 1;
                    continue;
                };
                items.push(SimpleGroupEntry {
                    group_id: group_id.clone(),
                    host_relay,
                    name: slice.get(3).cloned(),
                });
            }
            Some("r") => match slice.get(1).map(|s| RelayUrl::parse(s)) {
                Some(Ok(relay)) => relays_in_use.push(relay),
                Some(Err(_)) => malformed_item_count += 1,
                None => malformed_item_count += 1,
            },
            _ => {}
        }
    }

    SimpleGroupsList {
        items,
        relays_in_use,
        malformed_item_count,
        has_private_content: !content.is_empty(),
    }
}

