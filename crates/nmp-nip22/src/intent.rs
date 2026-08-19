//! Compose a NIP-22 comment and its publishable `WriteIntent` (#572, folded
//! onto the one tagging door in #1243).
//!
//! The tag SHAPE is no longer here. `nmp-grammar` owns kind:1111 and the
//! uppercase-root/lowercase-parent row shape, because that shape is what
//! `EventBuilder::tag` produces for every non-text-note target and having a
//! second copy of it was the whole defect: the hand-built composer emitted
//! `["E", <id>]` -- two cells -- where NIP-22 defines
//! `["E", <id>, <relay>, <pubkey>]`, six times in one file.
//!
//! What stays here is NIP-22's own verb. [`comment_intent`] always composes a
//! kind:1111 comment, including on a text note, where the general
//! `nmp_grammar::reply_to` would compose a NIP-10 reply instead. An app that
//! wants "the ordinary reply for whatever this is" calls that; an app that
//! wants "a NIP-22 comment on this specifically" calls this.

use nmp_grammar::{
    EventBuilder, Identity, Modifiers, RootScope, WriteIntent, WritePayload, WriteRouting,
    COMMENT_KIND,
};
use nostr::Kind;

/// Compose an unsigned kind:1111 comment on `target`.
///
/// Two calls through the one door: the uppercase root scope naming the
/// thread's root, then the lowercase rows naming the target itself. Neither
/// call states a relationship -- the root is read from the target's own rows
/// (`nmp_grammar::ThreadPosition`), so commenting on a root and commenting on
/// a deep reply are the same call and cannot be got backwards.
pub fn compose_comment(target: &impl RootScope, content: String) -> EventBuilder {
    EventBuilder::new(Kind::from(COMMENT_KIND))
        .tag(target.uppercase())
        .tag(target)
        .content(content)
}

/// Compose a `WriteIntent` for a NIP-22 comment on `target`. This crate adds
/// no routing or identity policy beyond the ordinary defaults every write has.
pub fn comment_intent(target: &impl RootScope, content: String) -> WriteIntent {
    WriteIntent {
        payload: WritePayload::Event(compose_comment(target, content)),
        routing: WriteRouting::Auto,
        identity: Identity::Active,
    }
}

