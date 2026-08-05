//! `nmp-nip18` -- the NIP-18 repost schema owner (#1243).
//!
//! NIP-18 defines TWO kinds and owns both, so the two-way split lives here
//! and there is no cross-crate dispatch to arrange: a reposted text note is a
//! kind:6, and anything else is a kind:16 that also states what it reposted
//! with a `k` row. A caller says "repost this" once and never picks a kind.
//!
//! Quoting is deliberately NOT here. NIP-18's quote repost is a `q` row on an
//! ordinary event with no kind dispatch at all, and it is written by naming
//! the event inside the content (`nmp_grammar::text!`), which is what makes
//! the row and the rendered reference impossible to disagree.

use nmp_grammar::{entity_rows, EventBuilder, TagOptions};
use nostr::{Event, Kind, RelayUrl};

/// NIP-18's kind for reposting a text note.
pub const REPOST_KIND: u16 = 6;

/// NIP-18's kind for reposting anything else. It carries a `k` row naming
/// what was reposted, because the kind is no longer implied by the repost's
/// own kind.
pub const GENERIC_REPOST_KIND: u16 = 16;

/// Compose a repost of `target`, observed at `sources`.
///
/// The rows come from the one tagging door's entity form, so the relay hint,
/// the author slot and the companion `p` row are filled exactly as they are
/// for every other pointer in NMP. It is the ENTITY form and not the
/// threading one on purpose: a text note that is itself a reply threads as two
/// `e` rows, and a NIP-18 reader takes the first `e` as the reposted event —
/// so threading a repost would repost the conversation's root instead of the
/// note the user chose.
///
/// The `k` row naming what was reposted comes from that same door, so it is
/// present on BOTH kinds rather than special-cased onto kind:16. NIP-18
/// requires it on a generic repost and does not forbid it on a kind:6; one
/// rule with no exception is worth more than suppressing a true statement.
pub fn repost(target: &Event, sources: Option<RelayUrl>) -> EventBuilder {
    let kind = if target.kind == Kind::from(nmp_grammar::TEXT_NOTE_KIND) {
        Kind::from(REPOST_KIND)
    } else {
        Kind::from(GENERIC_REPOST_KIND)
    };
    let mut builder = EventBuilder::new(kind);
    for row in entity_rows(target, sources, &TagOptions::default()) {
        builder = builder.tag(row);
    }
    builder
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder as NostrBuilder, EventId, Keys, Tag, Timestamp};

    fn signed(kind: u16, tags: Vec<Tag>) -> Event {
        NostrBuilder::new(Kind::from(kind), "content")
            .tags(tags)
            .custom_created_at(Timestamp::from(1_700_000_000))
            .sign_with_keys(&Keys::generate())
            .expect("test event signs")
    }

    fn rows(builder: &EventBuilder) -> Vec<Vec<String>> {
        builder
            .tags
            .iter()
            .map(|tag| tag.as_slice().to_vec())
            .collect()
    }

    /// The two-way split, decided inside NIP-18 because NIP-18 owns both
    /// kinds. A caller never names either.
    #[test]
    fn a_text_note_reposts_as_kind_6_and_anything_else_as_kind_16_plus_k() {
        let note = signed(1, vec![]);
        let reposted_note = repost(&note, None);
        assert_eq!(reposted_note.kind, Kind::from(REPOST_KIND));

        let picture = signed(20, vec![]);
        let reposted_picture = repost(&picture, None);
        assert_eq!(reposted_picture.kind, Kind::from(GENERIC_REPOST_KIND));
        assert!(
            rows(&reposted_picture)
                .iter()
                .any(|row| row[0] == "k" && row[1] == "20"),
            "a generic repost states what it reposted"
        );
    }

    /// A repost names the entity, never its thread position. Threading it
    /// would emit the ROOT's `e` row first, and a NIP-18 reader takes the
    /// first `e` as the reposted event -- so the user would have reposted a
    /// different note than the one they chose.
    #[test]
    fn reposting_a_reply_names_the_reply_and_never_its_root() {
        let root = EventId::from_slice(&[1; 32]).unwrap();
        let reply = signed(
            1,
            vec![Tag::parse(["e", &root.to_hex(), "", "root"]).unwrap()],
        );
        let reposted = repost(&reply, None);
        let emitted = rows(&reposted);
        let e_rows: Vec<&Vec<String>> = emitted.iter().filter(|row| row[0] == "e").collect();
        assert_eq!(e_rows.len(), 1, "exactly one e row: {e_rows:?}");
        assert_eq!(e_rows[0][1], reply.id.to_hex());
        for row in &emitted {
            assert!(!row.contains(&"root".to_string()));
        }
    }

    /// The hint, the author slot and the companion `p` row come from the one
    /// door, so a repost carries them exactly as a reply does.
    #[test]
    fn a_repost_carries_the_hint_the_author_slot_and_the_p_row() {
        let relay = RelayUrl::parse("wss://relay.example").unwrap();
        let note = signed(1, vec![]);
        let emitted = rows(&repost(&note, Some(relay.clone())));
        let e_row = emitted.iter().find(|row| row[0] == "e").expect("an e row");
        assert_eq!(e_row[2], relay.to_string());
        assert_eq!(e_row[3], note.pubkey.to_hex());
        assert!(emitted
            .iter()
            .any(|row| row[0] == "p" && row[1] == note.pubkey.to_hex()));
    }
}
