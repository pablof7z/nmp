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
//! in this module therefore takes exactly ONE host and stamps BOTH
//! host-scoping axes -- `ReadRouting::Explicit({host})` for the wire and
//! `CacheMode::Strict` for the local cache -- explicitly on the demand it
//! builds AND on every inner demand it nests, at depth 1, 2, or deeper.
//!
//! Pinning alone is NOT sufficient, and assuming otherwise is the mistake this
//! module was shipped with: `Explicit` scopes only which relays are ASKED, while
//! `CacheMode` governs which locally cached rows may ANSWER, and the grammar's
//! `Agnostic` default ignores provenance entirely. The two axes are
//! independent and NIP-29 needs both.
//!
//! Nothing here ever inherits an outer demand's routing or cache mode, and
//! nothing here ever rewrites the routing of a binding the CALLER supplied
//! (a `$myFollows`-shaped kind:3 lookup keeps its own `Auto` routing and its
//! own cache mode).
//!
//! Assembling one branch per host into a single live query is
//! `crate::RelayScope`'s job, not this module's: this module is engine-free
//! and mints atomic values only.

use std::collections::{BTreeMap, BTreeSet};

use nmp_grammar::{
    Binding, CacheMode, Demand, Derived, Filter, IndexedTagName, ReadRouting,
    Selector,
};
use nostr::RelayUrl;

/// kind:39000 -- the relay-signed group metadata NIP-29 defines.
pub const GROUP_METADATA_KIND: u16 = 39000;
/// kind:39001 -- the relay-signed, optional and informative admin list.
pub const GROUP_ADMINS_KIND: u16 = 39001;
/// kind:39002 -- the relay-signed, optional and possibly partial member list.
pub const GROUP_MEMBERS_KIND: u16 = 39002;

/// The tag NIP-29's relay-signed group records key themselves by.
pub(crate) const JOIN_KEY_TAG: char = 'd';
/// The tag a member/admin list names its subjects with.
const SUBJECT_TAG: char = 'p';

/// The group ids named by the relay-signed records matching `selection` AT
/// `host` -- the ONE host-evaluated id source, of which every named
/// constructor is a shorthand.
///
/// `selection` is an ordinary [`Filter`], so anything a live query can
/// express over a relay-signed group record can key a listing. The projection
/// is always the `d` row: it is the join key NIP-29 defines between the three
/// records, so projecting through anything else would yield values that are
/// not group ids. That is a protocol fact and deliberately not a parameter.
///
/// Which kinds a `selection` may name is [`crate::groups_whose_record_matches`]'s
/// refusal, not this function's: it rejects a kind the group's host is not
/// authoritative for, and every caller here has already passed it.
///
/// Returns an ordinary [`Binding`], so `Binding::SetOp` composes it with any
/// other binding for free.
#[must_use]
pub fn groups_whose_record_matches_at(host: &RelayUrl, selection: Filter) -> Binding {
    Binding::Derived(Box::new(Derived {
        inner: explicit_at(host, selection),
        project: Selector::Tag(JOIN_KEY_TAG.to_string()),
    }))
}

/// Groups whose kind:39002 member-list evidence AT `host` names `subjects`.
///
/// Evidence-scoped: a group matches when an observed member list includes a
/// subject. A group NOT matching proves nothing -- the list may be absent,
/// restricted, or partial.
///
/// Shorthand for [`groups_whose_record_matches_at`] over
/// `{ kinds:[39002], #p: subjects }` and exactly equal to it.
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
    groups_whose_record_matches_at(
        host,
        Filter {
            kinds: Some(BTreeSet::from([kind])),
            tags: BTreeMap::from([(subject(), subjects)]),
            ..Filter::default()
        },
    )
}

/// The one place a NIP-29-owned demand acquires its authority: pinned to
/// EXACTLY one host, public access, and `CacheMode::Strict`. Called at every
/// nesting level rather than once at the top.
///
/// # Both axes, because one is not enough
///
/// `ReadRouting::Explicit` and `CacheMode` are ORTHOGONAL, and NIP-29 needs
/// both pointed at the same host. `Explicit` scopes only the WIRE request:
/// which relays are asked. Which locally CACHED rows may answer is governed
/// separately by `CacheMode`, and the grammar's default `Agnostic` means
/// "serve every matching cached row regardless of provenance".
///
/// Defaulting therefore produced a real cross-host leak, not a theoretical
/// one. Two relays hosting the same group id are two independent groups; host
/// A's and host B's evidence lookups differ ONLY in their pinned set, so the
/// moment host A's kind:39002 row landed in the shared store, host B's
/// structurally-identical lookup resolved against it and reported a member
/// nothing at host B ever supported. The scope was honoured on the wire and
/// silently violated in cache. `Strict` closes it: a cached row answers a
/// branch only when its own provenance names that branch's host.
///
/// The consequence is intended, and is the same statement of per-relay
/// authority: a row that ANOTHER host served does not appear in this branch.
/// For the relay-signed discovery kinds nothing else is even possible -- an
/// app never authors a 39000/39001/39002.
///
/// It says nothing whatsoever about a row this node wrote itself. A locally
/// accepted write is not foreign data to be isolated; it is in the outbound
/// publication queue, it appears immediately in every query it matches
/// reporting zero relays until one carries it, and it is never withdrawn
/// later on the strength of which hosts did. That is general engine
/// behaviour (`nmp_store::Provenance::visible_under_pin`), it is not
/// NIP-29's to decide, and nothing in this crate implements or varies it.
///
/// Infallible for the same reason the deleted single-host door was, and for
/// that reason ONLY: a one-element relay set cannot be empty, which is the
/// only thing `Demand::new` refuses. The caller-suppliable relay SET is
/// validated once, where it enters -- [`crate::on`] -- and the nonempty scope
/// proves every host handed down here.
pub(crate) fn explicit_at(host: &RelayUrl, selection: Filter) -> Demand {
    let mut demand = Demand::new(
        selection,
        ReadRouting::Explicit(vec![host.clone()])
    )
    .expect("a singleton explicit relay set is always constructible");
    demand.cache = CacheMode::Strict;
    demand
}

pub(crate) fn join_key() -> IndexedTagName {
    IndexedTagName::new(JOIN_KEY_TAG).expect("'d' is a single ASCII letter")
}

pub(crate) fn subject() -> IndexedTagName {
    IndexedTagName::new(SUBJECT_TAG).expect("'p' is a single ASCII letter")
}

