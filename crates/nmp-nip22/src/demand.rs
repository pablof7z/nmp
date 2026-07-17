//! Root-thread observation (#572): a comment thread's root uses the
//! UPPERCASE `#I` indexed tag -- one filter covers the WHOLE thread, since
//! every reply retains the root tag regardless of nesting depth. There is
//! deliberately no parent-only `#i` shortcut: that would only ever surface
//! top-level comments, silently losing every reply.

use std::collections::BTreeSet;

use nmp_grammar::{Binding, Demand, Filter, IndexedTagName};

use crate::root::{CommentRoot, COMMENT_KIND};

/// The single `#I`-scoped tag value a [`CommentRoot`] is queried by:
/// `E`/`A`'s own reference string, or an external target's `I` value.
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
/// `kinds:[1111]`, scoped by the uppercase root reference on `#I`. One
/// filter covers the whole thread -- top-level comments AND every reply,
/// regardless of nesting depth, since NIP-22 requires every reply to
/// retain the identical root tag.
pub fn comment_thread_demand(root: &CommentRoot) -> Demand {
    let i = IndexedTagName::new('I').expect("'I' is an ASCII letter");
    let filter = Filter {
        kinds: Some(BTreeSet::from([COMMENT_KIND])),
        tags: std::collections::BTreeMap::from([(
            i,
            Binding::Literal(BTreeSet::from([root_identifier(root)])),
        )]),
        ..Filter::default()
    };
    Demand::from_filter(filter)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Nip73Target;
    use nmp_grammar::SourceAuthority;

    #[test]
    fn comment_thread_demand_scopes_kind_1111_by_uppercase_i_tag() {
        let root = CommentRoot::External(Nip73Target::podcast_episode_guid("guid-1").unwrap());
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
    /// selections (the `#I` tag binding) must differ.
    #[test]
    fn distinct_roots_yield_distinct_demands() {
        let a = comment_thread_demand(&CommentRoot::External(
            Nip73Target::podcast_episode_guid("guid-a").unwrap(),
        ));
        let b = comment_thread_demand(&CommentRoot::External(
            Nip73Target::podcast_episode_guid("guid-b").unwrap(),
        ));
        assert_ne!(a.selection, b.selection);
    }

    /// Reusable public demand: defaults to `AuthorOutboxes`-free public
    /// selection since this filter names no `authors` binding.
    #[test]
    fn demand_defaults_to_public_source() {
        let root = CommentRoot::External(Nip73Target::podcast_episode_guid("guid-1").unwrap());
        let demand = comment_thread_demand(&root);
        assert_eq!(demand.source, SourceAuthority::Public);
    }
}
