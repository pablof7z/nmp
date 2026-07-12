//! [`Demand`] — the full live-query identity (#106,
//! `docs/design/query-demand-and-evidence.md`): `selection + source +
//! access`, not filter-only. Two queries with the same [`Filter`] but
//! different intended authority must never collapse to the same atom/
//! refcount/coverage/attribution identity — that collapse (bug-class ledger
//! #18) is exactly what conflating "what rows match" with "where reads are
//! authorized to come from" caused.
//!
//! [`SourceAuthority`]/[`AccessContext`] are CLOSED vocabularies (VISION
//! P4-style): extend the enum, never admit a free-form config string.

use crate::binding::Filter;

/// Where reads are authorized to come from — the SOURCE axis of a
/// [`Demand`]. Closed vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourceAuthority {
    /// Content is fetched from each author's own outbox (NIP-65 write
    /// relays), discovered live — today's only real routing path for an
    /// author-bearing filter, now an explicit, named authority rather than
    /// an implicit consequence of "the filter happens to have an authors
    /// binding."
    AuthorOutboxes,
    /// Routed via operator-configured lanes (indexer/app/fallback) or
    /// protocol-fact pinned lookups (NIP-29 group host, DM inbox kind:10050)
    /// — today's authorless-filter heuristic, now an explicit authority
    /// rather than an emergent side effect of "no authors."
    Public,
}

/// The access/AUTH context a [`Demand`] carries — a reserved slot for #8
/// (NIP-42 AUTH). Only `Public` is populated today; this exists so the axis
/// is part of `Demand` identity from day one, not retrofitted once AUTH
/// lands (which would silently re-alias every existing coverage/evidence
/// row keyed before the axis existed).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccessContext {
    Public,
}

/// The cache-provenance mode a [`Demand`] carries -- meaningful ONLY under
/// `SourceAuthority::Pinned` once #107 adds that variant (today's closed
/// `SourceAuthority` has no `Pinned` case yet, so this field is currently
/// always `Agnostic`'s no-op equivalent in practice, but is threaded through
/// now so #107 lands as a clean read of an already-present field, never a
/// later widening of `Demand` itself). Deliberately NOT part of
/// `ContextualAtom`'s hashed identity (`Demand::hash`-equivalent) — it
/// governs the LOCAL row-projection read, never wire/coverage identity
/// (atlas's #106/#107 seam ruling: the two axes are orthogonal).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum CacheMode {
    /// Serve every matching cached row regardless of provenance.
    #[default]
    Agnostic,
    /// Serve only cached rows whose unioned provenance set intersects a
    /// pinned relay set (meaningless/no-op under any `SourceAuthority`
    /// other than `Pinned` — #107).
    Strict,
}

/// The full live-query identity: `selection + source + access` (#106).
/// `selection` is pure `Filter` — no context field is ever added to `Filter`
/// itself, keeping the grammar's own encoding/hashing untouched; `source`/
/// `access` fold into identity one level up, at [`crate::ContextualAtom`].
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Demand {
    pub selection: Filter,
    pub source: SourceAuthority,
    pub access: AccessContext,
    /// Orthogonal to `source`/`access` (see [`CacheMode`]'s doc) — a
    /// sibling field, deliberately excluded from `ContextualAtom`'s hashed
    /// identity.
    pub cache: CacheMode,
}

impl Demand {
    /// The default-preservation constructor (#106 acceptance criterion): a
    /// bare `Filter` lowers to `AuthorOutboxes` iff its `selection`
    /// STATICALLY names an `authors` binding — Literal, Reactive, Derived,
    /// or SetOp, ALL of them, not literal-authors-only. This is a shape
    /// check on the `Filter`, never a runtime resolution: a `$myFollows`-
    /// shaped `Derived` authors binding that happens to resolve empty on a
    /// given tick still declared an authors binding, so it still lowers to
    /// `AuthorOutboxes` — total and stable, and byte-identical to today's
    /// `route::classify` behavior (which keys on the LOWERED, post-
    /// resolution atom's authors presence, unaffected by this static
    /// pre-classification).
    pub fn from_filter(selection: Filter) -> Self {
        let source = if selection.authors.is_some() {
            SourceAuthority::AuthorOutboxes
        } else {
            SourceAuthority::Public
        };
        Self {
            selection,
            source,
            access: AccessContext::Public,
            cache: CacheMode::Agnostic,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::binding::{Binding, Derived};
    use crate::selector::{IdentityField, Selector};
    use std::collections::BTreeSet;

    #[test]
    fn a_filter_with_no_authors_binding_defaults_to_public() {
        let demand = Demand::from_filter(Filter {
            kinds: Some(BTreeSet::from([1u16])),
            ..Filter::default()
        });
        assert_eq!(demand.source, SourceAuthority::Public);
    }

    #[test]
    fn a_literal_authors_binding_defaults_to_author_outboxes() {
        let demand = Demand::from_filter(Filter {
            authors: Some(Binding::Literal(BTreeSet::from(["a".repeat(64)]))),
            ..Filter::default()
        });
        assert_eq!(demand.source, SourceAuthority::AuthorOutboxes);
    }

    /// The hard guardrail: a $myFollows-shaped DERIVED authors binding must
    /// ALSO default to AuthorOutboxes, never Public -- regressing this would
    /// silently misroute every reactive-follow-feed query in the workspace.
    #[test]
    fn a_derived_authors_binding_also_defaults_to_author_outboxes() {
        let my_follows = Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: Demand::from_filter(Filter {
                    kinds: Some(BTreeSet::from([3u16])),
                    authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                    ..Filter::default()
                }),
                project: Selector::Tag("p".to_string()),
            }))),
            ..Filter::default()
        };
        assert_eq!(
            Demand::from_filter(my_follows).source,
            SourceAuthority::AuthorOutboxes
        );
    }
}
