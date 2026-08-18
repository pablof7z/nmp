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

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::ReadRouting;
    use nmp_nip73::Nip73;
    use nostr::{EventId, PublicKey};

    #[test]
    fn comment_thread_demand_scopes_kind_1111_by_uppercase_i_tag() {
        let root = CommentRoot::External(Nip73::podcast_episode("guid-1").unwrap());
        let demand = comment_thread_demand(&root);
        assert_eq!(demand.selection.kinds, Some(BTreeSet::from([1111u16])));
        let i = IndexedTagName::new('I').unwrap();
        assert_eq!(
            demand.selection.tags.get(&i),
            Some(&Binding::Literal(BTreeSet::from([
                "podcast:item:guid:guid-1".to_string()
            ])))
        );
        // Never a parent-only lowercase `#i` shortcut -- confirm no `i`
        // binding exists at all.
        let lower_i = IndexedTagName::new('i').unwrap();
        assert_eq!(demand.selection.tags.get(&lower_i), None);
    }

    /// Different roots must never alias the same demand -- their
    /// selections (the root tag binding) must differ.
    #[test]
    fn distinct_roots_yield_distinct_demands() {
        let a = comment_thread_demand(&CommentRoot::External(
            Nip73::podcast_episode("guid-a").unwrap(),
        ));
        let b = comment_thread_demand(&CommentRoot::External(
            Nip73::podcast_episode("guid-b").unwrap(),
        ));
        assert_ne!(a.selection, b.selection);
    }

    /// The demand must ask for the tag the composer actually WRITES, for
    /// every root shape (#1876). Asserting against `root_rows` rather than
    /// against a hardcoded letter is what makes this non-vacuous: if either
    /// side changes its spelling, the pair stops agreeing and this fails.
    ///
    /// Before the fix, `Event` and `Address` roots asked `#I` while their
    /// comments carried `E` and `A`, so a thread rooted at anything but an
    /// external target returned zero rows forever.
    #[test]
    fn the_demanded_tag_is_the_tag_the_composer_writes_for_every_root_shape() {
        use nmp_grammar::{RootScope, TagOptions};

        let author = PublicKey::from_slice(&[2u8; 32]).unwrap();
        let roots = [
            CommentRoot::External(Nip73::podcast_episode("guid-1").unwrap()),
            CommentRoot::Event {
                event_id: EventId::from_slice(&[7u8; 32]).unwrap(),
                kind: 1,
                author: Some(author),
            },
            CommentRoot::Address {
                author,
                kind: 30023,
                identifier: "slug".to_string(),
                event_id: None,
            },
        ];

        for root in &roots {
            let demand = comment_thread_demand(root);
            let (name, binding) = demand
                .selection
                .tags
                .iter()
                .next()
                .expect("the demand binds exactly one root tag");
            assert_eq!(demand.selection.tags.len(), 1, "one root tag only");

            let wanted = match binding {
                Binding::Literal(values) => values.iter().next().unwrap().clone(),
                other => panic!("root tag must be a literal, got {other:?}"),
            };

            // The composer's own uppercase root rows for this same root.
            let rows = root.root_rows(&TagOptions::default());
            let emitted: Vec<Vec<String>> = rows.iter().map(|row| row.clone().to_vec()).collect();

            let matched = emitted.iter().any(|row| {
                row.first().map(|n| n.as_str()) == Some(name.to_string().as_str())
                    && row.get(1) == Some(&wanted)
            });
            assert!(
                matched,
                "demand asks #{name}={wanted} but {root:?} composes {emitted:?}"
            );
        }
    }

    /// This filter names no `authors` binding and no routing, so it rides
    /// `Auto` like any other selection -- `Auto`'s outbox lane simply has no
    /// author to solve for, and the remaining lanes carry the whole route.
    #[test]
    fn demand_names_no_routing() {
        let root = CommentRoot::External(Nip73::podcast_episode("guid-1").unwrap());
        let demand = comment_thread_demand(&root);
        assert_eq!(demand.routing, ReadRouting::Auto);
    }
}
