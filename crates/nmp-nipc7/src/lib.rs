//! `nmp-nipc7` -- the pure NIP-C7 kind:9 chat schema owner (#838).
//!
//! C7 owns the event kind and its reply row. It does not own NIP-29 group
//! context, NIP-27 inline mention materialization, or client notification
//! policy, and it owns no write policy either -- so its composers own SCHEMA
//! ONLY and return an [`EventBuilder`], leaving routing and identity to
//! whoever does. They name no author and read no clock: the engine resolves
//! the identity and stamps the time at acceptance.
//!
//! ## Why C7 offers its own reply verb
//!
//! Kind:9 must NOT become a NIP-22 comment. NIP-29 clients MUST only fetch
//! kind 9, so a 1111 reply inside a group would be invisible to every one of
//! them. That is why a schema with its own reply convention offers **its own
//! verb** ([`chat_reply`]) rather than an arm in a general dispatcher
//! (#1243): the app is already holding this crate, and the general
//! `nmp_grammar::reply_to` stays a two-way split with nothing to grow.
//!
//! The rows themselves still come from the one tagging door, so a chat reply
//! carries the relay hint, the author slot and the companion `p` row that
//! every other pointer in NMP carries.

use nmp_grammar::{EventBuilder, RootScope};
use nostr::Kind;

pub const CHAT_KIND: u16 = 9;

/// Compose a top-level kind:9 chat with no policy-added tags.
///
/// It states no content, exactly as [`chat_reply`] states none. What a
/// message SAYS belongs to `EventBuilder::content`, which is also where the
/// rows an inline mention or quote needs come from; taking a `String` here
/// meant a caller with either in the body passed an empty one and then
/// restated the content anyway, which is what the composer's own quote test
/// was doing (#964).
pub fn chat() -> EventBuilder {
    EventBuilder::new(Kind::from(CHAT_KIND))
}

/// Compose a kind:9 reply to `target`.
///
/// The reply row is `e`, not `q`. The composer this replaces emitted
/// `["q", <id>, <relay>, <author>]` (#1243), and a `q` row is NIP-18's QUOTE
/// marker whose entire stated purpose is that *"quote reposts are not pulled
/// and included as replies in threads"* -- so a C7 client reading it correctly
/// would place the message outside the thread it is replying to. Worse, that
/// composer emitted the `q` with nothing in the content quoting it, so the
/// half-formed quote could not render either. `nmp_grammar::text!` is how a
/// chat message actually quotes something now, and it makes the row and the
/// inline reference come from one statement so they cannot diverge again.
pub fn chat_reply(target: &impl RootScope) -> EventBuilder {
    EventBuilder::new(Kind::from(CHAT_KIND)).tag(target)
}

