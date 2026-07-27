//! Kind:10009 -- NIP-51's Simple groups list (#63/#108). A thin, TOLERANT,
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
//! Write-side replacement encoding is deliberately NOT here -- #63's
//! `rememberGroup`/`forgetGroup` mutations stay gated on #50's source-scoped
//! base-version contract; this file is read/parse-only. When such a
//! destructive mutation does arrive it must bind its exact observed base
//! privately, inside that semantic operation, while constructing an ordinary
//! opaque `WriteIntent` -- never by promoting a value from this module into a
//! reusable observed-authority noun.

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
/// that; it is enforced mechanically by
/// `scripts/check-nip51-no-derived-authority.sh`.
pub fn parse_simple_groups_list_tolerant(event: &Event) -> SimpleGroupsList {
    parse_simple_groups_list_from_raw_tags_tolerant(
        event.tags.iter().map(|tag| tag.as_slice()),
        &event.content,
    )
}

/// The same tolerant parse, over raw `[tag_name, values...]` string arrays
/// rather than a `nostr::Event` -- the shape `nmp-ffi`'s `FfiRow.tags`
/// already carries (a delivered row's raw tokens, ledger #12). Sharing this
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

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{Keys, Kind, Tag, Timestamp, UnsignedEvent};

    fn signed_event(tags: Vec<Tag>, content: &str) -> Event {
        let keys = Keys::generate();
        UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(1u64),
            Kind::Custom(10009),
            tags,
            content,
        )
        .sign_with_keys(&keys)
        .expect("sign fixture event")
    }

    fn group_tag(id: &str, relay: &str, name: Option<&str>) -> Tag {
        let mut values = vec!["group".to_string(), id.to_string(), relay.to_string()];
        if let Some(n) = name {
            values.push(n.to_string());
        }
        Tag::parse(values).expect("well-formed group tag")
    }

    fn r_tag(relay: &str) -> Tag {
        Tag::parse(vec!["r".to_string(), relay.to_string()]).expect("well-formed r tag")
    }

    #[test]
    fn decodes_group_items_preserving_order_and_optional_name() {
        let event = signed_event(
            vec![
                group_tag("group-a", "wss://relay-a.example.com", Some("Group A")),
                group_tag("group-b", "wss://relay-b.example.com", None),
            ],
            "",
        );
        let list = parse_simple_groups_list_tolerant(&event);
        assert_eq!(list.malformed_item_count, 0);
        assert_eq!(list.items.len(), 2);
        assert_eq!(list.items[0].group_id, "group-a");
        assert_eq!(
            list.items[0].host_relay,
            RelayUrl::parse("wss://relay-a.example.com").unwrap()
        );
        assert_eq!(list.items[0].name.as_deref(), Some("Group A"));
        assert_eq!(list.items[1].group_id, "group-b");
        assert_eq!(list.items[1].name, None);
        assert!(!list.has_private_content);
    }

    #[test]
    fn decodes_r_tags_as_relays_in_use_distinct_from_group_items() {
        let event = signed_event(
            vec![
                group_tag("group-a", "wss://relay-a.example.com", None),
                r_tag("wss://relay-c.example.com"),
            ],
            "",
        );
        let list = parse_simple_groups_list_tolerant(&event);
        assert_eq!(list.items.len(), 1);
        assert_eq!(
            list.relays_in_use,
            vec![RelayUrl::parse("wss://relay-c.example.com").unwrap()]
        );
    }

    #[test]
    fn malformed_group_tag_is_skipped_and_counted_not_fatal() {
        let event = signed_event(
            vec![
                Tag::parse(vec!["group".to_string(), "only-id".to_string()])
                    .expect("well-formed-enough tag shape"),
                group_tag("group-b", "wss://relay-b.example.com", None),
            ],
            "",
        );
        let list = parse_simple_groups_list_tolerant(&event);
        assert_eq!(list.malformed_item_count, 1);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].group_id, "group-b");
    }

    #[test]
    fn unparseable_relay_url_is_skipped_and_counted() {
        let event = signed_event(vec![group_tag("group-a", "not-a-url", None)], "");
        let list = parse_simple_groups_list_tolerant(&event);
        assert_eq!(list.malformed_item_count, 1);
        assert!(list.items.is_empty());
    }

    #[test]
    fn nonempty_content_reports_private_items_present() {
        let event = signed_event(vec![], "encrypted-blob-placeholder");
        let list = parse_simple_groups_list_tolerant(&event);
        assert!(list.has_private_content);
        assert!(list.items.is_empty());
    }

    /// This equivalence is the seam `nmp-ffi` relies on: an app only ever
    /// hands the FFI boundary raw tag arrays (`FfiRow.tags`), never a
    /// reconstructed `nostr::Event` -- so
    /// `parse_simple_groups_list_from_raw_tags_tolerant` must produce EXACTLY
    /// what `parse_simple_groups_list_tolerant` would over the same event's
    /// tags/content.
    #[test]
    fn raw_tags_entry_point_agrees_with_the_event_entry_point() {
        let tags = vec![
            group_tag("group-a", "wss://relay-a.example.com", Some("Group A")),
            r_tag("wss://relay-c.example.com"),
        ];
        let event = signed_event(tags.clone(), "some-content");
        let via_event = parse_simple_groups_list_tolerant(&event);
        let raw_tags: Vec<Vec<String>> = tags.iter().map(|t| t.as_slice().to_vec()).collect();
        let via_raw = parse_simple_groups_list_from_raw_tags_tolerant(
            raw_tags.iter().map(|t| t.as_slice()),
            "some-content",
        );
        assert_eq!(via_event, via_raw);
    }

    /// #863's core falsifier at the schema layer: a wholly fabricated,
    /// never-signed, wrong-kind input parses to the SAME observational value
    /// a delivered kind:10009 event would. Nothing in the result can tell the
    /// two apart -- which is exactly the point: the value carries no
    /// authority, so no consumer can be tricked into reading one out of it.
    #[test]
    fn tolerant_parse_of_fabricated_input_yields_plain_evidence_not_authority() {
        let fabricated: Vec<Vec<String>> = vec![
            vec![
                "group".to_string(),
                "group-a".to_string(),
                "wss://relay-a.example.com".to_string(),
                "Group A".to_string(),
            ],
            vec!["group".to_string(), "missing-relay".to_string()],
            vec![
                "r".to_string(),
                "wss://relay-in-use.example.com".to_string(),
            ],
        ];
        let from_fabricated = parse_simple_groups_list_from_raw_tags_tolerant(
            fabricated.iter().map(|t| t.as_slice()),
            "encrypted-private-items",
        );
        assert_eq!(from_fabricated.items.len(), 1);
        assert_eq!(from_fabricated.malformed_item_count, 1);
        assert!(from_fabricated.has_private_content);
        assert_eq!(
            from_fabricated.relays_in_use,
            vec![RelayUrl::parse("wss://relay-in-use.example.com").unwrap()]
        );

        // A real signed kind:10009 event with the same tags/content is
        // indistinguishable in the parsed value -- the parser grants the
        // signed one no extra standing, and the fabricated one no less.
        let signed = signed_event(
            vec![
                group_tag("group-a", "wss://relay-a.example.com", Some("Group A")),
                Tag::parse(vec!["group".to_string(), "missing-relay".to_string()])
                    .expect("well-formed-enough tag shape"),
                r_tag("wss://relay-in-use.example.com"),
            ],
            "encrypted-private-items",
        );
        assert_eq!(
            parse_simple_groups_list_tolerant(&signed),
            from_fabricated,
            "a parsed Simple-groups list is data; it never encodes provenance"
        );
    }
}
