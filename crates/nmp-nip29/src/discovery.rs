//! The NIP-29 discovery vocabulary (#1033): the three kinds NIP-29 itself
//! defines for describing a group, and the relationships between them.
//!
//! What is worth encapsulating here is not the number 39000. It is that a
//! group's metadata lives in kind:39000, its admin-list evidence in
//! kind:39001, its member-list evidence in kind:39002, and that the `d` tag
//! is the join key between them. Those relationships are what let an app ask
//! a relational question -- "groups whose member list includes me" -- through
//! the ordinary derived-query grammar instead of a canned single-kind demand.
//!
//! # Evidence, never exact state
//!
//! kind:39002 is an OPTIONAL member-list snapshot that may be absent,
//! access-restricted, or partial; kind:39001 is likewise an optional
//! informative admin list. Inclusion in an observed list is evidence.
//! ABSENCE FROM ONE IS NOT EVIDENCE of non-membership or of not being an
//! admin. Every predicate here is therefore spelled `..._list_includes`: it
//! matches on observed inclusion and claims nothing about completeness.
//! Reconstructing exact current membership from the canonical
//! kind:9000/kind:9001 sequence is a separate problem and is deliberately not
//! smuggled in here.
//!
//! # One host per branch
//!
//! NIP-29 authority is PER-RELAY, not per-group: 39000/39001/39002 are
//! relay-signed, so two relays hosting the same `h` id are two independent
//! groups with the same name. Membership evidence observed at relay A must
//! constrain the listing at relay A and never at relay B. Every constructor
//! in this module therefore takes exactly ONE host and stamps
//! `SourceAuthority::Pinned({host})` explicitly on the demand it builds AND
//! on every inner demand it nests -- at depth 1, 2, or deeper. Nothing here
//! ever inherits an outer demand's source, and nothing here ever rewrites the
//! authority of a binding the CALLER supplied (a `$myFollows`-shaped kind:3
//! lookup keeps its own `AuthorOutboxes`).
//!
//! Assembling one branch per host into a single live query is the facade's
//! job (`nmp::nip29`), not this crate's: this crate is engine-free and mints
//! atomic values only.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    AccessContext, Binding, Demand, Derived, Filter, IndexedTagName, Selector, SourceAuthority,
};
use nostr::RelayUrl;

/// kind:39000 -- the relay-signed group metadata NIP-29 defines.
pub const GROUP_METADATA_KIND: u16 = 39000;
/// kind:39001 -- the relay-signed, optional and informative admin list.
pub const GROUP_ADMINS_KIND: u16 = 39001;
/// kind:39002 -- the relay-signed, optional and possibly partial member list.
pub const GROUP_MEMBERS_KIND: u16 = 39002;

/// The tag NIP-29's relay-signed group records key themselves by.
const JOIN_KEY_TAG: char = 'd';
/// The tag a member/admin list names its subjects with.
const SUBJECT_TAG: char = 'p';

/// One host's complete branch of a group listing: the three relay-signed
/// group kinds at `host`, keyed by whatever `predicate` resolves `d` to.
///
/// The predicate is embedded VERBATIM. If the caller built it with
/// [`member_list_includes_at`] it is already pinned to `host`; if the caller
/// built it themselves it keeps its own authority, because rewriting it would
/// be exactly the silent repin `nmp_grammar::Derived` forbids.
#[must_use]
pub fn groups_where_at(host: &RelayUrl, predicate: Binding) -> Demand {
    pinned_public_at(
        host,
        Filter {
            kinds: Some(BTreeSet::from([
                GROUP_METADATA_KIND,
                GROUP_ADMINS_KIND,
                GROUP_MEMBERS_KIND,
            ])),
            tags: BTreeMap::from([(join_key(), predicate)]),
            ..Filter::default()
        },
    )
}

/// Groups whose kind:39002 member-list evidence AT `host` names `subjects`.
///
/// Evidence-scoped: a group matches when an observed member list includes a
/// subject. A group NOT matching proves nothing -- the list may be absent,
/// restricted, or partial.
///
/// Returns an ordinary [`Binding`], so `Binding::SetOp` composes it with any
/// other binding for free.
#[must_use]
pub fn member_list_includes_at(host: &RelayUrl, subjects: Binding) -> Binding {
    list_evidence_at(host, GROUP_MEMBERS_KIND, subjects)
}

/// Groups whose kind:39001 admin-list evidence AT `host` names `subjects`.
///
/// Evidence-scoped exactly like [`member_list_includes_at`]: absence from an
/// observed admin list is not evidence that a subject is not an admin.
#[must_use]
pub fn admin_list_includes_at(host: &RelayUrl, subjects: Binding) -> Binding {
    list_evidence_at(host, GROUP_ADMINS_KIND, subjects)
}

fn list_evidence_at(host: &RelayUrl, kind: u16, subjects: Binding) -> Binding {
    Binding::Derived(Box::new(Derived {
        inner: pinned_public_at(
            host,
            Filter {
                kinds: Some(BTreeSet::from([kind])),
                tags: BTreeMap::from([(subject(), subjects)]),
                ..Filter::default()
            },
        ),
        project: Selector::Tag(JOIN_KEY_TAG.to_string()),
    }))
}

/// The one place a NIP-29-owned demand acquires its authority: pinned to
/// EXACTLY one host, public access, and the grammar's own cache/freshness
/// defaults. Called at every nesting level rather than once at the top.
///
/// Infallible for the same reason the deleted single-host door was, and for
/// that reason ONLY: a one-element pinned set cannot be empty, and the source
/// is never `AuthorOutboxes`. The caller-suppliable relay SET is validated
/// once, where it enters -- `nmp::nip29::on` -- and the nonempty scope proves
/// every host handed down here.
pub(crate) fn pinned_public_at(host: &RelayUrl, selection: Filter) -> Demand {
    Demand::new(
        selection,
        SourceAuthority::Pinned(BTreeSet::from([host.clone()])),
        AccessContext::Public,
    )
    .expect("a singleton pinned relay set with a non-outbox source is always constructible")
}

fn join_key() -> IndexedTagName {
    IndexedTagName::new(JOIN_KEY_TAG).expect("'d' is a single ASCII letter")
}

fn subject() -> IndexedTagName {
    IndexedTagName::new(SUBJECT_TAG).expect("'p' is a single ASCII letter")
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{CacheMode, Freshness, IdentityField};

    fn host(n: u16) -> RelayUrl {
        RelayUrl::parse(&format!("wss://host-{n}.example.com")).expect("a well-formed host")
    }

    fn pinned(relays: [RelayUrl; 1]) -> SourceAuthority {
        SourceAuthority::Pinned(BTreeSet::from(relays))
    }

    fn derived(binding: &Binding) -> &Derived {
        match binding {
            Binding::Derived(inner) => inner,
            other => panic!("expected a Derived binding, got {other:?}"),
        }
    }

    #[test]
    fn a_listing_branch_selects_exactly_the_three_nip29_group_kinds() {
        let demand = groups_where_at(&host(1), Binding::Literal(BTreeSet::from(["x".to_string()])));
        assert_eq!(
            demand.selection.kinds,
            Some(BTreeSet::from([39000u16, 39001, 39002]))
        );
        assert_eq!(demand.source, pinned([host(1)]));
        assert_eq!(demand.access, AccessContext::Public);
        assert!(demand.selection.tags.contains_key(&join_key()));
    }

    #[test]
    fn member_evidence_is_kind_39002_over_p_projected_through_d() {
        let binding = member_list_includes_at(
            &host(1),
            Binding::Reactive(IdentityField::ActivePubkey),
        );
        let derived = derived(&binding);
        assert_eq!(derived.project, Selector::Tag("d".to_string()));
        assert_eq!(
            derived.inner.selection.kinds,
            Some(BTreeSet::from([39002u16]))
        );
        assert_eq!(
            derived.inner.selection.tags.get(&subject()),
            Some(&Binding::Reactive(IdentityField::ActivePubkey)),
            "identity stays REACTIVE -- never flattened to a literal"
        );
        assert_eq!(derived.inner.source, pinned([host(1)]));
    }

    #[test]
    fn admin_evidence_is_kind_39001_with_the_same_authority_shape() {
        let binding =
            admin_list_includes_at(&host(2), Binding::Reactive(IdentityField::ActivePubkey));
        let derived = derived(&binding);
        assert_eq!(
            derived.inner.selection.kinds,
            Some(BTreeSet::from([39001u16]))
        );
        assert_eq!(derived.inner.source, pinned([host(2)]));
        assert_eq!(derived.inner.access, AccessContext::Public);
        assert_eq!(derived.inner.cache, CacheMode::Agnostic);
        assert_eq!(derived.inner.freshness, Freshness::Live);
    }

    /// Every level a NIP-29 constructor OWNS is stamped with the exact host;
    /// nothing relies on inheritance. See the facade's own depth-2 falsifier
    /// for the full-graph version of this property.
    #[test]
    fn every_nip29_owned_level_is_pinned_to_the_exact_host() {
        let demand = groups_where_at(
            &host(3),
            member_list_includes_at(&host(3), Binding::Reactive(IdentityField::ActivePubkey)),
        );
        assert_eq!(demand.source, pinned([host(3)]));
        let inner = &derived(demand.selection.tags.get(&join_key()).expect("d is bound")).inner;
        assert_eq!(inner.source, pinned([host(3)]));
    }
}
