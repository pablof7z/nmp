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

use crate::convert::{
    event_builder_from_ffi, parse_pubkey, parse_relay_url, signed_event_from_ffi, FfiError,
};
use crate::types::{FfiContentPart, FfiEventBuilder, FfiReaction, FfiRow};
use nmp::{At, InterpolatedContent, Mention};

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

/// Compose a top-level NIP-C7 kind:9 chat.
///
/// The other half of what #1243 opened and #964 named: `chat_reply` closed
/// the reply case, so an app that replies no longer states a kind -- but an
/// app sending an ordinary message still stated `kind: 9` itself, because the
/// composer for THAT never crossed this boundary.
///
/// It composes SCHEMA ONLY, exactly as `chat_reply` does: no `h` row, no
/// notification policy, no routing, and no content. What the message says
/// comes from [`with_content`], which is also what emits the rows an inline
/// mention or quote needs.
#[uniffi::export]
pub fn chat() -> FfiEventBuilder {
    builder_to_ffi(nmp_nipc7::chat())
}

/// State what a composed draft SAYS, and emit the rows its inline references
/// need, from one call.
///
/// This is the second thing #964 named still living in Swift. An app that
/// lets somebody @-mention a person wrote `["p", hex]` by hand and hoped it
/// matched the `nostr:npub…` token it had separately put in the content;
/// nothing could catch a disagreement, because from the app's side nothing is
/// missing. Here the token and the row come out of the same
/// [`FfiContentPart`], so they cannot be written apart.
///
/// The rows are APPENDED after whatever the composer already stated for its
/// own reasons, never reordered and never deduplicated against them -- a
/// chat reply's `e` and `p` rows survive intact and the mention rows follow.
#[uniffi::export]
pub fn with_content(
    draft: FfiEventBuilder,
    content: Vec<FfiContentPart>,
) -> Result<FfiEventBuilder, FfiError> {
    let builder = event_builder_from_ffi(draft)?;
    let mut interpolated = InterpolatedContent::default();
    for part in content {
        match part {
            FfiContentPart::Text { text } => interpolated.text.push_str(&text),
            FfiContentPart::Person { pubkey, relay } => {
                let pubkey = parse_pubkey(&pubkey)?;
                match relay {
                    Some(relay) => {
                        interpolate(&At(pubkey, parse_relay_url(&relay)?), &mut interpolated)
                    }
                    None => interpolate(&pubkey, &mut interpolated),
                }
            }
            FfiContentPart::Quote { target } => {
                let row = row_from_ffi(target)?;
                match row.sources.iter().next() {
                    Some(relay) => interpolate(&At(&row.event, relay.clone()), &mut interpolated),
                    None => interpolate(&row.event, &mut interpolated),
                }
            }
        }
    }
    Ok(builder_to_ffi(builder.content(interpolated)))
}

/// Render one mention and collect the rows it requires, in the one place
/// both halves are produced -- which is the whole reason this door exists.
fn interpolate(mention: &dyn Mention, into: &mut InterpolatedContent) {
    into.text.push_str(&mention.render());
    into.rows.extend(mention.rows());
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

/// Compose a NIP-25 reaction to `target`.
///
/// #155's own report: NMP had no reaction door at all, so both consuming apps
/// hand-wrote `kind: 7` with their own `["e", …]` and `["p", …]` rows. What
/// that spelling loses is not the kind — it is everything the one tagging
/// door fills: the relay hint, the author slot, the `k` row, and the fact that
/// a reaction to a REPLY must name the reply rather than its thread root.
///
/// It composes SCHEMA ONLY. `reaction` is what the reaction says and never a
/// raw content string, because NIP-25 assigns `+`, `-` and the empty string
/// fixed meanings and a caller writing content by hand can mean one of them by
/// accident.
#[uniffi::export]
pub fn react_to(target: FfiRow, reaction: FfiReaction) -> Result<FfiEventBuilder, FfiError> {
    let reaction = match reaction {
        FfiReaction::Like => nmp_nip25::Reaction::Like,
        FfiReaction::Dislike => nmp_nip25::Reaction::Dislike,
        FfiReaction::Emoji { emoji } => {
            nmp_nip25::Reaction::emoji(emoji).map_err(|error| FfiError::InvalidReaction {
                reason: error.to_string(),
            })?
        }
    };
    let row = row_from_ffi(target)?;
    let hint = row.sources.iter().next().cloned();
    Ok(builder_to_ffi(nmp_nip25::react(&row.event, hint, reaction)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::row_to_ffi_row;
    use crate::types::FfiContentPart;

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
            built
                .tags
                .iter()
                .any(|row| row[0] == "p" && row[1] == parent_author),
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
            vec![vec![
                "e".into(),
                root_id.clone(),
                String::new(),
                "root".into(),
            ]],
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
            vec![vec![
                "e".into(),
                root_id.clone(),
                String::new(),
                "root".into(),
            ]],
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

    /// #964's remaining half: a message that is not a reply. Before this the
    /// C7 composer for a TOP-LEVEL chat never crossed the boundary, so an app
    /// stated `kind: 9` itself for every ordinary message it sent.
    #[test]
    fn a_native_top_level_chat_is_kind_9_and_carries_no_rows() {
        let built = chat();
        assert_eq!(built.kind, 9);
        assert!(built.tags.is_empty(), "a chat states no policy rows");
        assert_eq!(built.content, "");
        assert_eq!(
            built.created_at, None,
            "a schema-only composer invents no timestamp"
        );
    }

    /// The whole point of the door: the `nostr:npub…` a reader sees and the
    /// `p` row that notifies the person come out of ONE call, so an app can no
    /// longer write `["p", hex]` by hand and hope it matches the token it put
    /// in the content.
    #[test]
    fn naming_a_person_writes_the_token_and_the_p_row_together() {
        let alice = nostr::Keys::generate().public_key();
        let built = with_content(
            chat(),
            vec![
                FfiContentPart::Text {
                    text: "hey ".into(),
                },
                FfiContentPart::Person {
                    pubkey: alice.to_hex(),
                    relay: None,
                },
                FfiContentPart::Text {
                    text: ", look".into(),
                },
            ],
        )
        .expect("a named person composes");

        assert!(
            built.content.starts_with("hey nostr:npub1"),
            "bech32 is rendered at the user boundary: {}",
            built.content
        );
        assert!(built.content.ends_with(", look"));
        assert_eq!(built.tags, vec![vec!["p".to_string(), alice.to_hex()]]);
    }

    /// A stated relay reaches BOTH halves, because both come from the same
    /// part: the rendered pointer becomes an `nprofile` carrying the relay and
    /// the `p` row's hint cell is filled with the same value.
    #[test]
    fn a_stated_relay_reaches_the_rendered_pointer_and_the_row_together() {
        let alice = nostr::Keys::generate().public_key();
        let built = with_content(
            chat(),
            vec![FfiContentPart::Person {
                pubkey: alice.to_hex(),
                relay: Some("wss://relay.example".into()),
            }],
        )
        .expect("a named person composes");

        assert!(built.content.starts_with("nostr:nprofile1"));
        assert_eq!(
            built.tags,
            vec![vec![
                "p".to_string(),
                alice.to_hex(),
                "wss://relay.example".to_string()
            ]]
        );
    }

    /// An event named inline is a QUOTE, never a thread reply, and its hint
    /// comes from where NMP actually saw it -- the row's own verified sources,
    /// exactly as `chat_reply` fills the same cell.
    #[test]
    fn quoting_an_event_renders_it_and_emits_its_q_row_from_the_same_part() {
        let quoted = row(9, vec![], &["wss://chat.example.com"]);
        let quoted_id = quoted.id.clone();
        let quoted_author = quoted.pubkey.clone();
        let built = with_content(
            chat(),
            vec![
                FfiContentPart::Text {
                    text: "look: ".into(),
                },
                FfiContentPart::Quote { target: quoted },
            ],
        )
        .expect("a quote composes");

        assert!(
            built.content.starts_with("look: nostr:nevent1"),
            "{}",
            built.content
        );
        assert_eq!(
            built.tags,
            vec![vec![
                "q".to_string(),
                quoted_id,
                "wss://chat.example.com".to_string(),
                quoted_author
            ]],
            "an event named inline is a QUOTE, never a thread reply"
        );
    }

    /// Interpolated rows land AFTER whatever the composer stated for its own
    /// reasons and never disturb them -- the same guarantee the Rust door
    /// makes, held across the boundary.
    #[test]
    fn interpolated_rows_never_disturb_the_rows_a_composer_stated() {
        let parent = row(9, vec![], &["wss://chat.example.com"]);
        let alice = nostr::Keys::generate().public_key();
        let draft = chat_reply(parent).expect("a chat reply composes");
        let stated = draft.tags.clone();
        let built = with_content(
            draft,
            vec![FfiContentPart::Person {
                pubkey: alice.to_hex(),
                relay: None,
            }],
        )
        .expect("a named person composes");

        assert_eq!(built.tags[..stated.len()], stated[..]);
        assert_eq!(
            built.tags.last().unwrap(),
            &vec!["p".to_string(), alice.to_hex()]
        );
    }

    /// A malformed key is a typed synchronous refusal, and nothing partial
    /// escapes: no content, no rows.
    #[test]
    fn a_malformed_named_key_refuses_rather_than_composing_half_a_message() {
        let err = with_content(
            chat(),
            vec![FfiContentPart::Person {
                pubkey: "not-a-key".into(),
                relay: None,
            }],
        )
        .expect_err("a malformed key refuses");
        assert!(matches!(err, FfiError::InvalidPublicKey { .. }), "{err:?}");
    }

    /// #155's own report, closed at the boundary it named: a native app
    /// composes a reaction through NMP instead of hand-writing `kind: 7` with
    /// its own `e` and `p` rows, and the door fills the hint, the author slot
    /// and the `k` row an app-written pair never carried.
    #[test]
    fn a_native_reaction_is_kind_7_and_carries_what_the_one_door_fills() {
        let target = row(1, vec![], &["wss://relay.example"]);
        let target_id = target.id.clone();
        let target_author = target.pubkey.clone();
        let built = react_to(target, FfiReaction::Like).expect("a reaction composes");

        assert_eq!(built.kind, 7);
        assert_eq!(built.content, "+");
        let e_row = built
            .tags
            .iter()
            .find(|row| row[0] == "e")
            .expect("a reaction points with e");
        assert_eq!(e_row[1], target_id);
        assert_eq!(e_row[2], "wss://relay.example");
        assert_eq!(e_row[3], target_author);
        assert!(built
            .tags
            .iter()
            .any(|row| row[0] == "p" && row[1] == target_author));
        assert!(built.tags.iter().any(|row| row[0] == "k" && row[1] == "1"));
    }

    /// The three readings NIP-25 defines, across the boundary. A caller never
    /// writes the content bytes, so it cannot spell "like" by accident.
    #[test]
    fn the_native_reaction_vocabulary_is_nip25s_three_readings() {
        let content = |reaction| {
            react_to(row(1, vec![], &[]), reaction)
                .expect("a reaction composes")
                .content
        };
        assert_eq!(content(FfiReaction::Like), "+");
        assert_eq!(content(FfiReaction::Dislike), "-");
        assert_eq!(
            content(FfiReaction::Emoji {
                emoji: "🔥".into()
            }),
            "🔥"
        );
    }

    /// NIP-25: *"There MUST be always an `e` tag set to the `id` of the event
    /// that is being reacted to."* Reacting to a reply names the REPLY, so a
    /// client tallying by the first `e` cannot credit the thread root with a
    /// reaction nobody gave it.
    #[test]
    fn a_native_reaction_to_a_reply_names_the_reply_and_never_its_root() {
        let root_id = nostr::EventId::from_slice(&[6; 32]).unwrap().to_hex();
        let reply = row(
            1,
            vec![vec![
                "e".into(),
                root_id.clone(),
                String::new(),
                "root".into(),
            ]],
            &[],
        );
        let reply_id = reply.id.clone();
        let built = react_to(reply, FfiReaction::Like).expect("a reaction composes");
        let e_rows: Vec<&Vec<String>> = built.tags.iter().filter(|row| row[0] == "e").collect();
        assert_eq!(e_rows.len(), 1, "exactly one e row: {e_rows:?}");
        assert_eq!(e_rows[0][1], reply_id);
    }

    /// Both refusals reach the boundary as typed synchronous errors: an empty
    /// emoji is NIP-25's spelling of a LIKE, and a NIP-30 `:shortcode:` needs
    /// a companion `emoji` row this door does not write.
    #[test]
    fn an_emoji_that_would_say_something_else_refuses_before_a_builder_exists() {
        for emoji in ["", ":soapbox:"] {
            let err = react_to(
                row(1, vec![], &[]),
                FfiReaction::Emoji {
                    emoji: emoji.into(),
                },
            )
            .expect_err("a reaction that would say something else refuses");
            assert!(matches!(err, FfiError::InvalidReaction { .. }), "{err:?}");
        }
    }
}
