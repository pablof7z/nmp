//! The tagging door at the native boundary (#1243/#1258).
//!
//! #1243's original report was that a NIP-29 chat app could reach
//! `NMPGroup.publish` for the `h` row but had to hand-build the one thing
//! NIP-C7 owns, because the C7 composer never crossed the FFI. Its comment
//! block in `29er-next` said so:
//!
//! ```text
//! /// NOT NMP-owned yet, and it should be: NMP already owns this schema in Rust
//! tags.append(["q", reply.eventID, relay, reply.author.pubkey])
//! ```
//!
//! That row was wrong twice over — a `q` is NIP-18's QUOTE marker, whose whole
//! purpose is keeping the referenced event OUT of the thread — and nothing
//! caught it, which is exactly why schema ownership sits in NMP. These
//! functions are what an app calls instead. They return an
//! [`FfiEventBuilder`], the same value `FfiGroup::publish` already takes, so
//! a chat reply composes here and publishes through the one write lifecycle.
//!
//! Every one of them takes an [`FfiRow`] — the event the app is already
//! holding, sources included — and never a relationship, a marker, a relay
//! hint or an author. Those are what the door fills.

use crate::convert::{signed_event_from_ffi, FfiError};
use crate::types::{FfiEventBuilder, FfiRow};

/// Rebuild the canonical row an app read from NMP. `sources` is the verified
/// provenance the hint slot is filled from, and it survives the round trip
/// rather than being dropped at the boundary — a hint the native side cannot
/// carry is a hint nothing can emit.
pub(crate) fn row_from_ffi(row: FfiRow) -> Result<nmp::Row, FfiError> {
    let event = signed_event_from_ffi(
        row.id,
        row.pubkey,
        row.created_at,
        row.kind,
        row.tags,
        row.content,
        row.sig,
    )?;
    let sources = row
        .sources
        .iter()
        .map(|url| crate::convert::parse_relay_url(url))
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    Ok(nmp::Row { event, sources })
}

fn builder_to_ffi(builder: nmp::EventBuilder) -> FfiEventBuilder {
    crate::convert::event_builder_to_ffi(builder)
}

/// Compose the ordinary reply to `target`.
///
/// Two-way and no more: a text note threads through NIP-10, and everything
/// else becomes a NIP-22 comment. The split reads the TARGET's kind, and the
/// root/parent determination underneath reads neither the target's kind nor
/// the composing one — it reads the target's own rows, so a reply composed by
/// an app that believes it is replying to a root and one composed by an app
/// that knows better produce the same bytes.
#[uniffi::export]
pub fn reply_to(target: FfiRow) -> Result<FfiEventBuilder, FfiError> {
    Ok(builder_to_ffi(nmp::reply_to(&row_from_ffi(target)?)))
}

/// Compose a NIP-C7 kind:9 chat reply to `target`.
///
/// C7 offers its own verb rather than an arm in the general dispatcher
/// because kind:9 must NOT become a NIP-22 comment: NIP-29 clients MUST only
/// fetch kind 9, so a 1111 reply in a group would be invisible to every one
/// of them. The reply row is `e`, not `q`.
///
/// It composes SCHEMA ONLY — no `h` row, no notification policy, no routing.
/// A group's `h` row and its relay set come from `FfiGroup::publish`, which
/// takes exactly this value.
#[uniffi::export]
pub fn chat_reply(target: FfiRow) -> Result<FfiEventBuilder, FfiError> {
    Ok(builder_to_ffi(nmp_nipc7::chat_reply(&row_from_ffi(
        target,
    )?)))
}

/// Compose a NIP-18 repost of `target`.
///
/// NIP-18 owns both kinds, so the two-way split happens inside it: a reposted
/// text note is a kind:6 and anything else is a kind:16 that states what it
/// reposted. A caller never picks a kind.
#[uniffi::export]
pub fn repost(target: FfiRow) -> Result<FfiEventBuilder, FfiError> {
    let row = row_from_ffi(target)?;
    let hint = row.sources.iter().next().cloned();
    Ok(builder_to_ffi(nmp_nip18::repost(&row.event, hint)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::row_to_ffi_row;

    fn row(kind: u16, tags: Vec<Vec<String>>, sources: &[&str]) -> FfiRow {
        let keys = nostr::Keys::generate();
        let event = nostr::EventBuilder::new(nostr::Kind::from(kind), "body")
            .tags(
                tags.into_iter()
                    .map(|row| nostr::Tag::parse(row).expect("test tag parses")),
            )
            .sign_with_keys(&keys)
            .expect("test event signs");
        row_to_ffi_row(&nmp::Row {
            event,
            sources: sources
                .iter()
                .map(|url| nostr::RelayUrl::parse(url).expect("test relay parses"))
                .collect(),
        })
    }

    /// #1243's own report, closed at the boundary it named: a native chat app
    /// composes a C7 reply through NMP instead of hand-building a row, it is
    /// kind:9, and it points with `e`.
    #[test]
    fn a_native_chat_reply_is_kind_9_and_points_with_e() {
        let parent = row(9, vec![], &["wss://chat.example.com"]);
        let parent_id = parent.id.clone();
        let parent_author = parent.pubkey.clone();
        let built = chat_reply(parent).expect("a chat reply composes");

        assert_eq!(built.kind, 9);
        let e_row = built
            .tags
            .iter()
            .find(|row| row[0] == "e")
            .expect("a reply points with e");
        assert_eq!(e_row[1], parent_id);
        assert_eq!(
            e_row[2], "wss://chat.example.com",
            "the verified source survives the boundary and fills the hint"
        );
        assert_eq!(e_row[3], parent_author, "the author slot is filled");
        assert!(built.tags.iter().all(|row| row[0] != "q"));
        assert!(
            built.tags.iter().any(|row| row[0] == "p" && row[1] == parent_author),
            "the companion p row crosses too"
        );
    }

    /// The thread position is the wire's, across the boundary as much as in
    /// Rust: replying to a reply names the ROOT as root and the target as
    /// reply, whatever the app believed.
    #[test]
    fn a_native_reply_reads_the_targets_own_thread_position() {
        let root_id = nostr::EventId::from_slice(&[4; 32]).unwrap().to_hex();
        let target = row(
            1,
            vec![vec!["e".into(), root_id.clone(), String::new(), "root".into()]],
            &["wss://relay.example"],
        );
        let target_id = target.id.clone();
        let built = reply_to(target).expect("a reply composes");

        assert_eq!(built.kind, 1);
        assert_eq!(built.tags[0][1], root_id);
        assert_eq!(built.tags[0][3], "root");
        assert_eq!(built.tags[1][1], target_id);
        assert_eq!(built.tags[1][3], "reply");
    }

    /// A repost names the entity, so reposting a reply reposts THAT note and
    /// never the conversation's root -- which is what a NIP-18 reader would
    /// otherwise take from a threaded row pair.
    #[test]
    fn a_native_repost_names_the_entity_and_splits_its_own_kind() {
        let root_id = nostr::EventId::from_slice(&[5; 32]).unwrap().to_hex();
        let reply = row(
            1,
            vec![vec!["e".into(), root_id.clone(), String::new(), "root".into()]],
            &["wss://relay.example"],
        );
        let reply_id = reply.id.clone();
        let built = repost(reply).expect("a repost composes");
        assert_eq!(built.kind, 6);
        let e_rows: Vec<&Vec<String>> = built.tags.iter().filter(|row| row[0] == "e").collect();
        assert_eq!(e_rows.len(), 1);
        assert_eq!(e_rows[0][1], reply_id);

        let picture = row(20, vec![], &[]);
        let built = repost(picture).expect("a repost composes");
        assert_eq!(built.kind, 16);
        assert!(built.tags.iter().any(|row| row[0] == "k" && row[1] == "20"));
    }
}
