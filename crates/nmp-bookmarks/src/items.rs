//! NIP-51's kind:10003 bookmarks list: what an item on it means, and the
//! reactive query that reads the current account's own list.
//!
//! Public bookmarks only. NIP-51 also permits an encrypted (NIP-44,
//! to-self) `content` payload of the same four tag shapes for PRIVATE
//! bookmarks; this crate does not read or write it. That needs a signer
//! round-trip at both compose and read time, which is a different, larger
//! capability than the one this proof exists to demonstrate -- and NIP-51
//! itself treats the two halves as independent (a client may support only
//! public bookmarks). Left out on purpose, not by oversight.

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter, IdentityField};
use nostr::nips::nip01::Coordinate;
use nostr::{Event, EventId, RelayUrl};

/// The kind NIP-51 defines for the bookmarks list.
pub const BOOKMARKS_KIND: u16 = 10_003;

/// One bookmarked thing, exactly as NIP-51 defines the public tag shapes.
///
/// `Ord` is derived so a [`BookmarksList`] can de-duplicate and compare by
/// value rather than by tag position -- two lists naming the same items in
/// a different order are the SAME list.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BookmarkedItem {
    /// An `e` row: a bookmarked event, with the relay hint NIP-51 permits
    /// as the row's third cell, if the event carried one.
    Event {
        id: EventId,
        relay_hint: Option<RelayUrl>,
    },
    /// An `a` row: a bookmarked parameterized-replaceable coordinate.
    Address(Coordinate),
    /// A `t` row: a bookmarked hashtag, lowercased exactly as NIP-12/NIP-51
    /// already require for the `t` indexed tag.
    Hashtag(String),
    /// An `r` row: a bookmarked URL, carried verbatim -- NIP-51 defines no
    /// normalization for it, and inventing one here would silently change
    /// what the user bookmarked.
    Url(String),
}

/// The current account's public bookmarks, tolerantly read off one event.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BookmarksList {
    pub items: Vec<BookmarkedItem>,
}

/// Read every recognized public bookmark row off `event`, in tag order.
///
/// Tolerant, not validating: a malformed `e`/`a` row (bad hex, bad
/// coordinate) is skipped rather than refusing the whole list -- one bad
/// row must not hide every other bookmark a relay actually served. Rows
/// this crate does not recognize (any tag name other than `e`/`a`/`t`/`r`)
/// are silently not bookmarks, which is the correct reading: NIP-51 defines
/// exactly these four public shapes for kind:10003, so nothing else on the
/// event is part of the list this crate owns.
#[must_use]
pub fn parse_bookmarks_tolerant(event: &Event) -> BookmarksList {
    let mut items = Vec::new();
    for tag in event.tags.iter() {
        let row = tag.as_slice();
        let Some(name) = row.first() else { continue };
        match name.as_str() {
            "e" => {
                let Some(id) = row.get(1).and_then(|hex| EventId::from_hex(hex).ok()) else {
                    continue;
                };
                let relay_hint = row
                    .get(2)
                    .filter(|hint| !hint.is_empty())
                    .and_then(|hint| RelayUrl::parse(hint).ok());
                items.push(BookmarkedItem::Event { id, relay_hint });
            }
            "a" => {
                let Some(coordinate) = row.get(1).and_then(|raw| Coordinate::parse(raw).ok())
                else {
                    continue;
                };
                items.push(BookmarkedItem::Address(coordinate));
            }
            "t" => {
                let Some(hashtag) = row.get(1) else { continue };
                items.push(BookmarkedItem::Hashtag(hashtag.clone()));
            }
            "r" => {
                let Some(url) = row.get(1) else { continue };
                items.push(BookmarkedItem::Url(url.clone()));
            }
            _ => {}
        }
    }
    BookmarksList { items }
}

/// The current account's kind:10003 bookmarks list through the ordinary
/// reactive live-query path. Logged out resolves to zero atoms; account
/// changes reroot this same demand without a component-managed
/// subscription graph -- the identical shape `nmp_nip02::current_account_
/// demand` and `nmp_nip29::current_account_group_list_demand` already use
/// for their own single-owned-list kinds.
#[must_use]
pub fn current_account_bookmarks_demand() -> Demand {
    Demand {
        selection: Filter {
            kinds: Some(BTreeSet::from([BOOKMARKS_KIND])),
            authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
            ..Filter::default()
        },
        ..Demand::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::ReadRouting;
    use nostr::{EventBuilder, Keys, Kind, Tag, Timestamp};

    #[test]
    fn bookmarks_demand_uses_current_account_author_outboxes() {
        let demand = current_account_bookmarks_demand();
        assert_eq!(demand.selection.kinds, Some(BTreeSet::from([10_003])));
        assert_eq!(
            demand.selection.authors,
            Some(Binding::Reactive(IdentityField::ActivePubkey))
        );
        assert_eq!(demand.routing, ReadRouting::Auto);
    }

    fn signed(tags: Vec<Tag>) -> Event {
        EventBuilder::new(Kind::Custom(BOOKMARKS_KIND), "")
            .tags(tags)
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&Keys::generate())
            .expect("fixture signs")
    }

    #[test]
    fn every_recognized_public_row_is_read_in_order() {
        let bookmarked = Keys::generate().public_key();
        let event_id = EventId::from_slice(&[7u8; 32]).unwrap();
        let coordinate = Coordinate::new(Kind::Custom(30_023), bookmarked);
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let event = signed(vec![
            Tag::parse(["e", &event_id.to_hex(), relay.as_str()]).unwrap(),
            Tag::parse(["a", &coordinate.to_string()]).unwrap(),
            Tag::parse(["t", "nostr"]).unwrap(),
            Tag::parse(["r", "https://example.com/article"]).unwrap(),
        ]);
        let parsed = parse_bookmarks_tolerant(&event);
        assert_eq!(
            parsed.items,
            vec![
                BookmarkedItem::Event {
                    id: event_id,
                    relay_hint: Some(relay),
                },
                BookmarkedItem::Address(coordinate),
                BookmarkedItem::Hashtag("nostr".to_string()),
                BookmarkedItem::Url("https://example.com/article".to_string()),
            ]
        );
    }

    #[test]
    fn a_malformed_row_is_skipped_not_refused() {
        let good = EventId::from_slice(&[9u8; 32]).unwrap();
        let event = signed(vec![
            Tag::parse(["e", "not-valid-hex"]).unwrap(),
            Tag::parse(["e", &good.to_hex()]).unwrap(),
            Tag::parse(["a", "not:a:coordinate:at:all:extra"]).unwrap(),
        ]);
        let parsed = parse_bookmarks_tolerant(&event);
        assert_eq!(
            parsed.items,
            vec![BookmarkedItem::Event {
                id: good,
                relay_hint: None,
            }]
        );
    }

    #[test]
    fn an_unrecognized_tag_name_is_not_a_bookmark() {
        let event = signed(vec![Tag::parse(["client", "some-app"]).unwrap()]);
        assert_eq!(parse_bookmarks_tolerant(&event).items, Vec::new());
    }
}
