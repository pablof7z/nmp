//! Root-thread observation (#572): a comment thread's root uses the
//! UPPERCASE root tag its own shape is carried on -- `E` for an event root,
//! `A` for an addressable one, `I` for an external target. One filter covers
//! the WHOLE thread, since every reply retains the root tag regardless of
//! nesting depth. There is deliberately no parent-only lowercase shortcut:
//! that would only ever surface top-level comments, silently losing every
//! reply.

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter, IndexedTagName};

use crate::root::{CommentRoot, COMMENT_KIND};

/// The uppercase root tag a [`CommentRoot`] is actually carried on, matching
/// what [`CommentRoot::rows`] emits: `E` for an event root, `A` for an
/// addressable one, `I` for an external target. Querying the wrong letter
/// asks for a tag no comment in the thread has (#1876).
fn root_tag_name(root: &CommentRoot) -> char {
    match root {
        CommentRoot::Event { .. } => 'E',
        CommentRoot::Address { .. } => 'A',
        CommentRoot::External(_) => 'I',
    }
}

/// The tag value a [`CommentRoot`] is queried by, paired with
/// [`root_tag_name`]: `E`/`A`'s own reference string, or an external
/// target's `I` value.
fn root_identifier(root: &CommentRoot) -> String {
    match root {
        CommentRoot::Event { event_id, .. } => event_id.to_hex(),
        CommentRoot::Address {
            author,
            kind,
            identifier,
            ..
        } => CommentRoot::address_coordinate(*kind, author, identifier),
        CommentRoot::External(target) => target.i_value().to_string(),
    }
}

/// The demand for an entire NIP-22 comment thread rooted at `root`:
/// `kinds:[1111]`, scoped by the uppercase root reference on the tag that
/// root shape is carried on (#1876). One
/// filter covers the whole thread -- top-level comments AND every reply,
/// regardless of nesting depth, since NIP-22 requires every reply to
/// retain the identical root tag.
pub fn comment_thread_demand(root: &CommentRoot) -> Demand {
    let name = root_tag_name(root);
    let tag = IndexedTagName::new(name).expect("root tag name is an ASCII letter");
    let filter = Filter {
        kinds: Some(BTreeSet::from([COMMENT_KIND])),
        tags: std::collections::BTreeMap::from([(
            tag,
            Binding::Literal(BTreeSet::from([root_identifier(root)])),
        )]),
        ..Filter::default()
    };
    Demand {
        selection: filter,
        ..Demand::default()
    }
}

