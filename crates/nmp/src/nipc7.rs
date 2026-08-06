//! NIP-C7 chat (kind:9) projected through the canonical facade (#1239).
//!
//! `nmp-nipc7` owns the kind:9 schema and its reply row -- and specifically
//! owns the fact that a chat reply is an `e` row and not NIP-18's `q`, and
//! that a kind:9 must not become a NIP-22 comment because NIP-29 clients only
//! fetch kind 9. Those are precisely the rules a caller composing kind:9 by
//! hand gets wrong, which is why the door existing only for Swift was the
//! problem #1239 records rather than a cosmetic asymmetry.
//!
//! Both composers return a [`crate::EventBuilder`] and state no content: what
//! a message says belongs to `EventBuilder::content`, which is also where the
//! rows an inline mention or quote needs come from.

pub use nmp_nipc7::{chat, chat_reply, CHAT_KIND};
