//! Kind:10009 -- NIP-51's Simple groups list (#63/#108). A thin decode-only
//! codec over `nostr::Event`'s own `Tag`/`Tags` accessors (rust-nostr has no
//! kind:10009 helper of its own -- unlike NIP-19's bech32 module, there is
//! no existing implementation to adapt here; memory rule "use rust-nostr,
//! not scratch crypto" is still honored: no hand-rolled tag/relay-url
//! parsing, `RelayUrl::parse` and `Tag::kind()`/`Tag::as_slice()` do the
//! actual work).
//!
//! Write-side replacement encoding is deliberately NOT here -- #63's
//! `rememberGroup`/`forgetGroup` mutations stay gated on #50's source-scoped
//! base-version contract; this file is read/decode-only.

use nostr::{Event, RelayUrl};

/// One decoded `["group", <id>, <relay>, <name>?]` item from a kind:10009
/// event -- exactly the three fields #63 names: group id, host relay, and
/// an optional name. `host_relay` is canonicalized via `RelayUrl::parse`
/// (#108 Done-when: "decoded kind-10009 rows produce canonical remembered
/// relay hosts").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SimpleGroupEntry {
    pub group_id: String,
    pub host_relay: RelayUrl,
    pub name: Option<String>,
}

/// A decoded kind:10009 event. `items`/`relays_in_use` preserve the tag
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

/// Decode a kind:10009 event's PUBLIC (tag-carried) items. Never fails --
/// a malformed individual tag is skipped and counted (see
/// [`SimpleGroupsList::malformed_item_count`]), never treated as a reason
/// to discard the whole list; this event's `kind` is the caller's own
/// concern (the query that acquired it already constrained `kinds:
/// [10009]`), not re-validated here.
pub fn decode_simple_groups_list(event: &Event) -> SimpleGroupsList {
    let mut items = Vec::new();
    let mut relays_in_use = Vec::new();
    let mut malformed_item_count = 0usize;

    for tag in event.tags.iter() {
        match tag.kind().as_str() {
            "group" => {
                let slice = tag.as_slice();
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
            "r" => {
                let slice = tag.as_slice();
                match slice.get(1).map(|s| RelayUrl::parse(s)) {
                    Some(Ok(relay)) => relays_in_use.push(relay),
                    Some(Err(_)) => malformed_item_count += 1,
                    None => malformed_item_count += 1,
                }
            }
            _ => {}
        }
    }

    SimpleGroupsList {
        items,
        relays_in_use,
        malformed_item_count,
        has_private_content: !event.content.is_empty(),
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
        let list = decode_simple_groups_list(&event);
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
        let list = decode_simple_groups_list(&event);
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
        let list = decode_simple_groups_list(&event);
        assert_eq!(list.malformed_item_count, 1);
        assert_eq!(list.items.len(), 1);
        assert_eq!(list.items[0].group_id, "group-b");
    }

    #[test]
    fn unparseable_relay_url_is_skipped_and_counted() {
        let event = signed_event(vec![group_tag("group-a", "not-a-url", None)], "");
        let list = decode_simple_groups_list(&event);
        assert_eq!(list.malformed_item_count, 1);
        assert!(list.items.is_empty());
    }

    #[test]
    fn nonempty_content_reports_private_items_present() {
        let event = signed_event(vec![], "encrypted-blob-placeholder");
        let list = decode_simple_groups_list(&event);
        assert!(list.has_private_content);
        assert!(list.items.is_empty());
    }
}
