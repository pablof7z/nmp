//! The typed NIP-22 root/parent relationship (#572). Modeling GENERAL roots
//! now (maintainer-decided, not podcast-only) avoids a near-term breaking
//! reshape when event/address-rooted comments arrive: `CommentRoot::Event
//! | Address | External` mirrors NIP-22's own uppercase `E`/`A`/`I` root
//! vocabulary exactly, one variant per root shape the spec defines --
//! never one variant per NIP-73 *namespace* (that restraint lives in
//! [`crate::Nip73Target`] instead).

use nostr::{EventId, PublicKey};

use crate::target::Nip73Target;

/// The root of a NIP-22 comment thread -- what every reply in the thread,
/// regardless of depth, keeps naming via the uppercase `E`/`A`/`K`/`P`/`I`
/// tag family. Every comment in a thread carries an IDENTICAL root value;
/// only [`CommentParent`] varies with nesting depth.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommentRoot {
    /// A root that is itself an ordinary (non-addressable) Nostr event --
    /// `E`/`K`/`P`.
    Event {
        event_id: EventId,
        kind: u16,
        author: Option<PublicKey>,
    },
    /// A root that is an addressable/replaceable Nostr event -- `A`/`K`/`P`,
    /// optionally with an accompanying `E` (NIP-22: "when the parent event
    /// is replaceable or addressable, also include an `e`/`E` tag
    /// referencing its id" -- since a coordinate alone doesn't pin a
    /// specific revision). `identifier` is the address's `d` tag value (may
    /// be empty per NIP-01, but the coordinate string is always well-formed
    /// since `author`/`kind` are structurally typed).
    Address {
        author: PublicKey,
        kind: u16,
        identifier: String,
        /// The addressable event's own id, when the composer/decoded event
        /// pinned one alongside the coordinate. `None` is still a fully
        /// legal root -- the accompanying `E`/`e` is a SHOULD, not a MUST.
        event_id: Option<EventId>,
    },
    /// A root outside Nostr entirely -- `I`/`K` (NIP-73).
    External(Nip73Target),
}

impl CommentRoot {
    /// The address coordinate string (`<kind>:<pubkey-hex>:<identifier>`)
    /// for [`Self::Address`] -- NIP-01's canonical `a`-tag value shape.
    pub fn address_coordinate(kind: u16, author: &PublicKey, identifier: &str) -> String {
        format!("{kind}:{}:{identifier}", author.to_hex())
    }
}

/// The comment's DIRECT parent -- what it is replying to. [`Self::Root`]
/// means this is a TOP-LEVEL comment on the thread (its parent mirrors the
/// root using NIP-22's lowercase tag family: `e`/`a`/`i` + `k` + `p`, the
/// exact same identity as the root, just lowercased). [`Self::Comment`]
/// means this is a reply to another comment: the root tags stay pinned to
/// the thread's root, but the parent becomes the comment event being
/// replied to (`e` + `k=1111` + `p` when the parent's author is known).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentParent {
    /// This comment is top-level: its parent is the thread root itself.
    Root,
    /// This comment replies to another comment event.
    Comment {
        event_id: EventId,
        author: Option<PublicKey>,
    },
}

/// NIP-22's fixed kind for a comment event.
pub const COMMENT_KIND: u16 = 1111;
