//! The typed NIP-22 root/parent relationship (#572). Modeling GENERAL roots
//! now (maintainer-decided, not podcast-only) avoids a near-term breaking
//! reshape when event/address-rooted comments arrive: `CommentRoot::Event
//! | Address | External` mirrors NIP-22's own uppercase `E`/`A`/`I` root
//! vocabulary exactly, one variant per root shape the spec defines --
//! never one variant per NIP-73 *namespace* (that restraint lives in
//! [`Nip73`] instead).

use nostr::{EventId, PublicKey};

use nmp_nip73::Nip73;

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
    External(Nip73),
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

/// A decoded root is a legal tagging target, so `reply_to`/`compose_comment`
/// take one directly (#1243).
///
/// A `CommentRoot` describes an entity by its PARTS rather than holding the
/// event, which is what a decoder produces and what an app holds for an
/// external content id. It is always the root of its own thread — there are
/// no rows on it to read a position out of — so the four-case thread reading
/// applies to the event case and is untouched here.
impl nmp_grammar::RootScope for CommentRoot {
    fn root_rows(&self, options: &nmp_grammar::TagOptions) -> Vec<nostr::Tag> {
        self.rows(true, options)
    }

    fn parent_rows(&self, options: &nmp_grammar::TagOptions) -> Vec<nostr::Tag> {
        self.rows(false, options)
    }

    fn entity_kind(&self) -> Option<nostr::Kind> {
        match self {
            Self::Event { kind, .. } | Self::Address { kind, .. } => Some(nostr::Kind::from(*kind)),
            // Not a Nostr event, so no kind. A reply to it is a comment.
            Self::External(_) => None,
        }
    }
}

impl CommentRoot {
    /// One shape, rendered in either case. NIP-22 states importance with case
    /// and never with a marker, so the two differ in exactly the letters.
    fn rows(&self, uppercase: bool, options: &nmp_grammar::TagOptions) -> Vec<nostr::Tag> {
        let (e, a, i, k, p) = if uppercase {
            ("E", "A", "I", "K", "P")
        } else {
            ("e", "a", "i", "k", "p")
        };
        let relay = options
            .relay_hint()
            .map(|relay| relay.to_string())
            .unwrap_or_default();
        let mut rows = Vec::new();
        let mut author_row = None;
        match self {
            Self::Event {
                event_id,
                kind,
                author,
            } => {
                let author_hex = author.map(|author| author.to_hex()).unwrap_or_default();
                rows.push(row(&[e, &event_id.to_hex(), &relay, &author_hex]));
                rows.push(row(&[k, &kind.to_string()]));
                author_row = *author;
            }
            Self::Address {
                author,
                kind,
                identifier,
                event_id,
            } => {
                let coordinate = Self::address_coordinate(*kind, author, identifier);
                rows.push(row(&[a, &coordinate, &relay, &author.to_hex()]));
                rows.push(row(&[k, &kind.to_string()]));
                if let Some(event_id) = event_id {
                    // NIP-22: "when the parent event is replaceable or
                    // addressable, also include an `e` tag referencing its
                    // id" -- a coordinate alone does not pin a revision.
                    rows.push(row(&[e, &event_id.to_hex(), &relay, &author.to_hex()]));
                }
                author_row = Some(*author);
            }
            Self::External(id) => {
                rows.push(row(&[i, &id.i_value()]));
                rows.push(row(&[k, id.k_value()]));
            }
        }
        if let Some(author) = author_row.filter(|author| options.keeps_pubkey(author)) {
            if !options.suppresses_author() {
                rows.push(row(&[p, &author.to_hex(), &relay]));
            }
        }
        rows
    }
}

/// Build one row, dropping the trailing empty cells it does not need. An
/// empty cell in the MIDDLE stays, because everything after it is positional.
fn row(cells: &[&str]) -> nostr::Tag {
    let mut cells = cells.to_vec();
    while cells.len() > 2 && cells.last().is_some_and(|last| last.is_empty()) {
        cells.pop();
    }
    nostr::Tag::parse(cells).expect("a NIP-22 row always has a non-empty first cell")
}
