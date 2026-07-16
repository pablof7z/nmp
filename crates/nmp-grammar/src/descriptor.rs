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

use std::collections::BTreeSet;

use crate::binding::Filter;

/// Where reads are authorized to come from — the SOURCE axis of a
/// [`Demand`]. Closed vocabulary.
///
/// No longer `Copy` (#107): `Pinned`'s relay set makes that impossible.
/// Every call site that used to rely on an implicit copy now clones
/// explicitly -- a one-time, mechanical cost of carrying a real relay set
/// in the type, not a design smell.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
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
    /// Explicit pinned wire authority (#107): ask ONLY these relays, on the
    /// wire, full stop — never expand to outbox/directory/app/fallback/
    /// indexer routing, regardless of whether the selection is author-
    /// bearing. Validated nonempty at construction (`Demand::new`);
    /// `BTreeSet<RelayUrl>` already gives canonical sort + dedup for free
    /// once each `RelayUrl` came through `RelayUrl::parse` (the #107
    /// Contract's "URL-canonicalized, sorted, and deduplicated" clause).
    /// Cache-read behavior over this pinned set (Agnostic vs Strict) is a
    /// SIBLING axis (`Demand::cache`), never nested here — see
    /// [`CacheMode`]'s doc.
    Pinned(BTreeSet<nostr::RelayUrl>),
}

/// The connection-scoped access/AUTH context a [`Demand`] carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum AccessContext {
    /// An unauthenticated relay connection. It never shares a physical
    /// session or acquisition evidence with an authenticated context.
    Public,
    /// Authenticate the exact relay session with this explicit identity.
    /// The value is fixed in the demand; changing the engine's active account
    /// cannot redirect it.
    Nip42(nostr::PublicKey),
}

/// The complete identity of one physical relay session.
///
/// NIP-42 visibility is connection-scoped, so a URL without its frozen
/// access context is never a sufficient key for planning, transport,
/// attribution, replay, or coverage.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RelaySessionKey {
    pub relay: nostr::RelayUrl,
    pub access: AccessContext,
}

impl RelaySessionKey {
    #[must_use]
    pub const fn new(relay: nostr::RelayUrl, access: AccessContext) -> Self {
        Self { relay, access }
    }

    #[must_use]
    pub const fn public(relay: nostr::RelayUrl) -> Self {
        Self::new(relay, AccessContext::Public)
    }
}

/// The cache-provenance mode a [`Demand`] carries -- meaningful ONLY under
/// `SourceAuthority::Pinned` (#107's Contract: "pinned cache policy is part
/// of source identity"); a no-op over any other source, since there is no
/// pinned relay set to intersect against. Deliberately NOT part of
/// `ContextualAtom`'s hashed identity (`Demand::hash`-equivalent) — it
/// governs the LOCAL row-projection read (`nmp-engine`'s
/// `rows_and_evidence_for`), never wire/coverage identity (atlas's
/// #106/#107 seam ruling: the two axes are orthogonal). Consumed per-handle,
/// off `QueryHandle::cache()` -- never per-graph-node, since two handles
/// may share the identical (cache-free-deduped) `AcquisitionKey` while
/// disagreeing on `cache` (the #107 Done-when: "Same-filter Agnostic and
/// Strict handles remain distinct even when wire work coalesces").
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

/// How one query handle uses existing coverage when deciding whether it
/// contributes remote acquisition work. This is a third orthogonal axis on
/// the existing live-query noun, beside [`SourceAuthority`] and
/// [`CacheMode`]; it is not part of [`crate::ContextualAtom`] identity.
/// Equal handles may therefore share their graph, rows, wire subscription,
/// and coverage history while making independent freshness decisions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Freshness {
    /// Cache-then-live: contribute ordinary wire work for the handle's
    /// lifetime. This is the pre-existing behavior and the default.
    #[default]
    Live,
    /// Suppress wire work when every currently planned source has coverage
    /// through at least `seconds` before the handle's opening-time engine
    /// clock. If not satisfied at opening, degrade once to [`Self::Live`].
    /// Whole seconds are exact: Nostr timestamps and coverage watermarks do
    /// not carry subsecond precision.
    MaxAge { seconds: u64 },
    /// Serve the canonical cache projection without ever contributing wire
    /// work, regardless of coverage or row presence.
    CacheOnly,
}

/// The full live-query declaration. Its semantic identity is
/// `selection + source + access` (#106); `cache` and `freshness` remain
/// per-handle policy axes.
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
    /// Per-handle acquisition freshness. Deliberately excluded from atom,
    /// wire, and coverage identity; see [`Freshness`].
    pub freshness: Freshness,
}

/// The unconstructible `Demand` combinations (#106/#107, Fable's ratified
/// shape + the #107 Contract): `Demand::new` refuses these at construction
/// rather than silently producing a `Demand` whose routing path resolves
/// nothing forever.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemandError {
    /// `SourceAuthority::AuthorOutboxes` declared over a selection whose
    /// `authors` field is not bound at all -- there is no author whose
    /// outbox could possibly be chased.
    AuthorOutboxesRequiresBoundAuthors,
    /// `SourceAuthority::Pinned` declared with an empty relay set (#107
    /// Contract: "the pinned relay set must be nonempty") -- there is
    /// nothing for the wire to ask.
    PinnedRequiresNonemptyRelaySet,
}

impl std::fmt::Display for DemandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DemandError::AuthorOutboxesRequiresBoundAuthors => write!(
                f,
                "SourceAuthority::AuthorOutboxes requires a selection whose `authors` field is bound"
            ),
            DemandError::PinnedRequiresNonemptyRelaySet => {
                write!(f, "SourceAuthority::Pinned requires a nonempty relay set")
            }
        }
    }
}

impl std::error::Error for DemandError {}

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
            freshness: Freshness::Live,
        }
    }

    /// Explicit constructor (#106, Fable's ratified shape) for a caller who
    /// wants a NON-default `source`/`access` combination -- e.g. `Public`
    /// on an author-bearing selection ("these authors, generic facts only,
    /// no outbox chase"; the one new expressible behavior #106 adds,
    /// Fable's falsifier 1 / landing-review owner nod). Validates the ONE
    /// unconstructible combination (see [`DemandError`]); every other
    /// combination is legal.
    pub fn new(
        selection: Filter,
        source: SourceAuthority,
        access: AccessContext,
    ) -> Result<Self, DemandError> {
        match &source {
            SourceAuthority::AuthorOutboxes if selection.authors.is_none() => {
                return Err(DemandError::AuthorOutboxesRequiresBoundAuthors);
            }
            SourceAuthority::Pinned(relays) if relays.is_empty() => {
                return Err(DemandError::PinnedRequiresNonemptyRelaySet);
            }
            _ => {}
        }
        Ok(Self {
            selection,
            source,
            access,
            cache: CacheMode::Agnostic,
            freshness: Freshness::Live,
        })
    }

    /// The ONE identity projection (#106, Fable's ratified shape): which
    /// fields participate in atom/wire/coverage identity
    /// (`ContextualAtom`) -- `cache` is deliberately excluded (see
    /// [`CacheMode`]'s doc), which is what makes #107's addition of that
    /// field a one-line, identity-neutral change.
    pub fn atom_context(&self) -> (SourceAuthority, AccessContext) {
        (self.source.clone(), self.access)
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

    /// #106's falsifier 7 (constructor validation): `AuthorOutboxes`
    /// declared over an authorless selection is unconstructible.
    #[test]
    fn new_rejects_author_outboxes_over_an_authorless_selection() {
        let err = Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([1u16])),
                ..Filter::default()
            },
            SourceAuthority::AuthorOutboxes,
            AccessContext::Public,
        )
        .unwrap_err();
        assert_eq!(err, DemandError::AuthorOutboxesRequiresBoundAuthors);
    }

    /// The new expressible behavior #106 adds (Fable's owner-flagged
    /// landing-review nod): `Public` on an author-bearing selection is
    /// LEGAL -- "these authors, generic facts only, no outbox chase."
    #[test]
    fn new_allows_public_over_an_author_bearing_selection() {
        let demand = Demand::new(
            Filter {
                authors: Some(Binding::Literal(BTreeSet::from(["a".repeat(64)]))),
                ..Filter::default()
            },
            SourceAuthority::Public,
            AccessContext::Public,
        )
        .expect("Public over an author-bearing selection is legal");
        assert_eq!(demand.source, SourceAuthority::Public);
    }

    /// #107's Contract falsifier (Fable's empty-pinned-fails pattern):
    /// `Pinned` with an empty relay set is unconstructible.
    #[test]
    fn new_rejects_pinned_with_an_empty_relay_set() {
        let err = Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([1u16])),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::new()),
            AccessContext::Public,
        )
        .unwrap_err();
        assert_eq!(err, DemandError::PinnedRequiresNonemptyRelaySet);
    }

    #[test]
    fn new_allows_pinned_with_a_nonempty_relay_set() {
        let relay = nostr::RelayUrl::parse("wss://relay.example").unwrap();
        let demand = Demand::new(
            Filter {
                kinds: Some(BTreeSet::from([1u16])),
                ..Filter::default()
            },
            SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
            AccessContext::Public,
        )
        .expect("a nonempty pinned relay set is legal");
        assert_eq!(
            demand.source,
            SourceAuthority::Pinned(BTreeSet::from([relay]))
        );
    }

    #[test]
    fn atom_context_projects_source_and_access_only() {
        let mut demand = Demand::from_filter(Filter {
            authors: Some(Binding::Literal(BTreeSet::from(["a".repeat(64)]))),
            ..Filter::default()
        });
        assert_eq!(demand.freshness, Freshness::Live);
        let context = demand.atom_context();
        demand.cache = CacheMode::Strict;
        demand.freshness = Freshness::MaxAge { seconds: 14_400 };
        assert_eq!(
            demand.atom_context(),
            (SourceAuthority::AuthorOutboxes, AccessContext::Public)
        );
        assert_eq!(demand.atom_context(), context);
    }
}
