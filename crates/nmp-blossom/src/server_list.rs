//! BUD-03 kind:10063 user server-list schema and ordinary live-query demand
//! (#731). This module owns no observation lifecycle and no cache: callers
//! observe [`active_account_server_list_demand`] through the normal engine,
//! then decode the canonical replacement winner delivered as an ordinary row.

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter, IdentityField};
use nostr::Event;

use crate::{BlossomServerUrl, ServerUrlError};

/// BUD-03's replaceable user server-list kind.
pub const USER_SERVER_LIST_KIND: u16 = 10063;

/// Why one `server` tag could not become a typed [`BlossomServerUrl`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MalformedServerEntryReason {
    /// The tag was exactly `["server"]` (or otherwise lacked its URL value).
    MissingUrl,
    /// The URL violated the same syntax gate every Blossom HTTP operation
    /// applies. DNS/SSRF admission is intentionally later and belongs to
    /// [`crate::BlossomClient::qualify_server_candidates`].
    InvalidUrl(ServerUrlError),
}

/// Evidence for one malformed BUD-03 `server` tag. `tag_index` is its exact
/// position in the signed event's tag array, so a bad entry never vanishes or
/// gets detached from the user-authored order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MalformedServerEntry {
    pub tag_index: usize,
    pub raw_url: Option<String>,
    pub reason: MalformedServerEntryReason,
}

/// Closed decode of one BUD-03 kind:10063 replacement winner.
///
/// `servers` preserves the exact relative order of every well-formed
/// `["server", URL]` row. Invalid rows are retained separately with their
/// original positions. BUD-03 requires at least one server and does not use
/// content; `is_spec_compliant` states those two facts without discarding the
/// usable well-formed prefix.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserServerList {
    servers: Vec<BlossomServerUrl>,
    malformed_entries: Vec<MalformedServerEntry>,
    server_tag_count: usize,
    unexpected_content: bool,
}

impl UserServerList {
    pub fn servers(&self) -> &[BlossomServerUrl] {
        &self.servers
    }

    pub fn malformed_entries(&self) -> &[MalformedServerEntry] {
        &self.malformed_entries
    }

    pub fn server_tag_count(&self) -> usize {
        self.server_tag_count
    }

    pub fn has_unexpected_content(&self) -> bool {
        self.unexpected_content
    }

    /// Whether the event obeys NMP's closed BUD-03 schema: at least one
    /// well-formed server, no malformed server rows, and empty content.
    pub fn is_spec_compliant(&self) -> bool {
        !self.servers.is_empty() && self.malformed_entries.is_empty() && !self.unexpected_content
    }
}

/// The active account's BUD-03 replacement-list demand:
/// `kinds:[10063], authors:Reactive(ActivePubkey), AuthorOutboxes + Public`.
/// Signed-out state resolves to zero atoms through the same ordinary reactive
/// binding path as kind:10009; account changes reroot only this demand.
pub fn active_account_server_list_demand() -> Demand {
    Demand::from_filter(Filter {
        kinds: Some(BTreeSet::from([USER_SERVER_LIST_KIND])),
        authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
        ..Filter::default()
    })
}

/// Decode an ordinary signed event selected by the canonical store path.
pub fn decode_server_list(event: &Event) -> UserServerList {
    decode_server_list_from_raw_tags(event.tags.iter().map(|tag| tag.as_slice()), &event.content)
}

/// Raw-tag entry point for `nmp-ffi`'s already-delivered row shape. This
/// intentionally does not reconstruct or revalidate an event: signature,
/// replacement, deletion, expiry, and absence are owned by the canonical
/// ingest/store/observation path that produced the row.
pub fn decode_server_list_from_raw_tags<'a>(
    tags: impl IntoIterator<Item = &'a [String]>,
    content: &str,
) -> UserServerList {
    let mut servers = Vec::new();
    let mut malformed_entries = Vec::new();
    let mut server_tag_count = 0usize;

    for (tag_index, tag) in tags.into_iter().enumerate() {
        if tag.first().map(String::as_str) != Some("server") {
            continue;
        }
        server_tag_count += 1;
        let Some(raw_url) = tag.get(1) else {
            malformed_entries.push(MalformedServerEntry {
                tag_index,
                raw_url: None,
                reason: MalformedServerEntryReason::MissingUrl,
            });
            continue;
        };
        match BlossomServerUrl::parse(raw_url) {
            Ok(server) => servers.push(server),
            Err(error) => malformed_entries.push(MalformedServerEntry {
                tag_index,
                raw_url: Some(raw_url.clone()),
                reason: MalformedServerEntryReason::InvalidUrl(error),
            }),
        }
    }

    UserServerList {
        servers,
        malformed_entries,
        server_tag_count,
        unexpected_content: !content.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{AccessContext, SourceAuthority};
    use nostr::{Keys, Kind, Tag, Timestamp, UnsignedEvent};

    fn event(tags: Vec<Vec<&str>>, content: &str) -> Event {
        let keys = Keys::generate();
        let tags: Vec<Tag> = tags
            .into_iter()
            .map(|tag| {
                Tag::parse(tag.into_iter().map(str::to_string).collect::<Vec<_>>())
                    .expect("test tag")
            })
            .collect();
        UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(10u64),
            Kind::from(USER_SERVER_LIST_KIND),
            tags,
            content,
        )
        .sign_with_keys(&keys)
        .expect("sign test event")
    }

    #[test]
    fn active_account_demand_is_reactive_author_outbox_public() {
        let demand = active_account_server_list_demand();
        assert_eq!(
            demand.selection.kinds,
            Some(BTreeSet::from([USER_SERVER_LIST_KIND]))
        );
        assert_eq!(
            demand.selection.authors,
            Some(Binding::Reactive(IdentityField::ActivePubkey))
        );
        assert_eq!(demand.source, SourceAuthority::AuthorOutboxes);
        assert_eq!(demand.access, AccessContext::Public);
    }

    #[test]
    fn ordered_server_tags_decode_through_the_existing_url_gate() {
        let list = decode_server_list(&event(
            vec![
                vec!["server", "https://first.example"],
                vec!["ignored", "value"],
                vec!["server", "https://second.example/"],
            ],
            "",
        ));
        assert_eq!(
            list.servers()
                .iter()
                .map(BlossomServerUrl::as_str)
                .collect::<Vec<_>>(),
            vec!["https://first.example/", "https://second.example/"]
        );
        assert_eq!(list.server_tag_count(), 2);
        assert!(list.malformed_entries().is_empty());
        assert!(list.is_spec_compliant());
    }

    #[test]
    fn malformed_missing_and_invalid_urls_remain_visible_by_tag_position() {
        let list = decode_server_list(&event(
            vec![
                vec!["server"],
                vec!["server", "http://127.0.0.1:3000"],
                vec!["server", "ftp://invalid.example"],
            ],
            "not-used",
        ));
        // Literal-local syntax is valid here; DNS/SSRF admission is the
        // client's later gate and signature never bypasses it.
        assert_eq!(list.servers().len(), 1);
        assert_eq!(list.malformed_entries().len(), 2);
        assert_eq!(list.malformed_entries()[0].tag_index, 0);
        assert!(matches!(
            list.malformed_entries()[0].reason,
            MalformedServerEntryReason::MissingUrl
        ));
        assert_eq!(list.malformed_entries()[1].tag_index, 2);
        assert!(matches!(
            list.malformed_entries()[1].reason,
            MalformedServerEntryReason::InvalidUrl(ServerUrlError::UnsupportedScheme { .. })
        ));
        assert!(list.has_unexpected_content());
        assert!(!list.is_spec_compliant());
    }

    #[test]
    fn no_server_tag_is_explicitly_not_spec_compliant() {
        let list = decode_server_list(&event(vec![vec!["p", "11"]], ""));
        assert_eq!(list.server_tag_count(), 0);
        assert!(list.servers().is_empty());
        assert!(!list.is_spec_compliant());
    }
}
