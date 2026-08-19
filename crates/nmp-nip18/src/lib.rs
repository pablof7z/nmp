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

