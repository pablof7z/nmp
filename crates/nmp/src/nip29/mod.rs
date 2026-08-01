//! `nmp::nip29` -- the app-facing NIP-29 door (#1033).
//!
//! Two values, one narrowing:
//!
//! ```text
//! let relays = nip29::on([relay_a, relay_b])?;          // which relays
//! let group  = relays.group("photographers");           // narrowed to one group
//! ```
//!
//! [`RelayScope`] is where an app names relays, ONCE. Nothing downstream of
//! it takes a per-call host, route, group id or raw `h` row: the retained
//! private context is the only source of all four. That is the whole reason
//! the scope exists as a value rather than as a parameter list.
//!
//! # Why the door lives here and not in `nmp-nip29`
//!
//! `nmp-nip29` is engine-free by construction -- `nostr` + `nmp-grammar`, no
//! core, no mechanism (`scripts/check-nip29-ownership.sh`). It owns NIP-29's
//! schema, its tag/predicate semantics, its editors and its signed-event
//! validation, and it returns only validated semantic values. It never
//! imports, constructs, stores or returns a [`WriteIntent`](crate::WriteIntent).
//!
//! The final door needs both halves at once: the retained relay scope AND the
//! opaque write intent the one publish door takes. A lower crate cannot read
//! this module's private retained context without a public accessor, a
//! callback injection, or a reverse dependency -- all three of which are
//! worse than moving the door up. So the door is here, the vocabulary is
//! below, and the dependency still runs `nmp -> nmp-nip29` only.
//!
//! # Per-relay authority is the whole difficulty
//!
//! NIP-29 authority is PER-RELAY, not per-group. The `h` tag is a label; the
//! relay decides. Two relays hosting the same group id are two independent
//! groups with the same name -- membership diverges, and 39000/39001/39002
//! are relay-signed so metadata diverges too. NMP surfaces that divergence
//! rather than collapsing it: the addressable coordinate includes the author
//! pubkey, so two relays' own 39000s never compete.
//!
//! The live hazard is scope threading. Resolving membership evidence from
//! relay A while listing groups on relay B is a confidently WRONG answer, not
//! a slow one. Every read this module mints is therefore one complete
//! singleton-host branch per host, with the host stamped explicitly at every
//! NIP-29-owned nesting level -- never inherited, and never blanket-rewritten
//! onto a binding the caller supplied.

mod group;
mod predicate;
mod read;

use std::collections::BTreeSet;

use nmp_grammar::Demand;
use nostr::RelayUrl;

pub use group::{Group, GroupPublishError, GroupReceipts};
pub use nmp_nip29::GroupContextError;
pub use predicate::{admin_list_includes, member_list_includes, GroupPredicate};
pub use read::GroupReadError;

// The NIP-29-owned composers, re-exported as themselves. An app that wants
// the raw builder for a named operation gets it; the group's own methods are
// the ordinary path.
pub use nmp_nip29::{
    add_user, create_group, create_invite, delete_event, delete_group, edit_metadata, join_request,
    leave_request, remove_user,
};
// The kinds NIP-29 itself defines for describing a group. Named because
// NIP-29 names them -- unlike a group's CONTENT kinds, which are the app's to
// choose and which this crate deliberately refuses to catalogue (#838).
pub use nmp_nip29::{GROUP_ADMINS_KIND, GROUP_MEMBERS_KIND, GROUP_METADATA_KIND};

/// Why a relay scope could not be formed.
///
/// One variant, reachable at exactly one place. `nip29::on` is the only
/// caller-suppliable relay SET in the whole NIP-29 surface, so it is the only
/// place emptiness can enter; every host handed down from a formed
/// [`RelayScope`] is proved nonempty by the type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RelayScopeError {
    /// No relay was named. A group must be hosted somewhere: there is nothing
    /// to read from, nothing to write to, and no honest evidence to report.
    EmptyRelaySet,
}

impl std::fmt::Display for RelayScopeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::EmptyRelaySet => {
                f.write_str("a NIP-29 relay scope must name at least one host relay")
            }
        }
    }
}

impl std::error::Error for RelayScopeError {}

/// The relays a group lives on -- named once, retained privately, and never
/// asked for again.
///
/// Canonical by construction: hosts are collected into a `BTreeSet`, so
/// duplicates collapse and order is the URL order. Two scopes built from
/// permuted or repeated inputs are the SAME value.
///
/// Nonempty by construction: [`on`] is the only way to make one and it
/// refuses an empty set, which is what makes every method below infallible
/// with respect to the relay set.
///
/// A scope owns no observation, engine, signer, store, transport, retry or
/// receipt lifecycle. It mints ordinary values and nothing else.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelayScope {
    hosts: BTreeSet<RelayUrl>,
}

/// Name the relays a NIP-29 group lives on.
///
/// Fallible, and deliberately so. The deleted single-host door was infallible
/// only because a one-element pinned set cannot be empty, and its own doc said
/// that widening it to a caller-supplied SET must restore fallibility. This is
/// that widening.
///
/// Takes decoded [`RelayUrl`]s, never strings an app pasted -- an app parses
/// at its own boundary and hands NMP relays.
pub fn on(hosts: impl IntoIterator<Item = RelayUrl>) -> Result<RelayScope, RelayScopeError> {
    let hosts: BTreeSet<RelayUrl> = hosts.into_iter().collect();
    if hosts.is_empty() {
        return Err(RelayScopeError::EmptyRelaySet);
    }
    Ok(RelayScope { hosts })
}

impl RelayScope {
    /// Narrow to one group id, keeping the same hosts.
    ///
    /// Contacts nothing, subscribes to nothing, and needs no active account:
    /// a kind:9021 join request into a group you cannot read yet is
    /// expressible. The returned [`Group`] retains this scope's hosts and the
    /// id privately; neither is readable back out, so no other layer can
    /// reconstruct the authority and route something elsewhere under it.
    #[must_use]
    pub fn group(&self, group_id: impl Into<String>) -> Group {
        Group::new(self.hosts.clone(), group_id.into())
    }

    /// Groups on these relays matching a composable discovery predicate.
    ///
    /// One complete branch per host: at host `H` the listing selects
    /// 39000/39001/39002 pinned to `H`, keyed on `d` by the predicate lowered
    /// AT `H`. Evidence observed at one relay therefore never constrains a
    /// listing at another.
    ///
    /// ```text
    /// let mine = nip29::member_list_includes(Binding::Reactive(IdentityField::ActivePubkey));
    /// engine.observe(relays.groups_where(&mine)?, None)?;
    /// ```
    pub fn groups_where(&self, predicate: &GroupPredicate) -> Result<LiveQuery, GroupReadError> {
        read::one_live_query(self.listing_branches(predicate))
    }

    /// One complete listing branch per host, in canonical host order. Split
    /// out so the per-branch source-stamping property is assertable on its own
    /// -- it is the property the whole design exists to guarantee, and it must
    /// be provable for a MULTI-host scope without depending on how branches
    /// are later aggregated into one live query.
    pub(crate) fn listing_branches(&self, predicate: &GroupPredicate) -> Vec<Demand> {
        self.hosts
            .iter()
            .map(|host| nmp_nip29::groups_where_at(host, predicate.lower_at(host)))
            .collect()
    }

    #[cfg(test)]
    fn hosts(&self) -> &BTreeSet<RelayUrl> {
        &self.hosts
    }
}

use crate::LiveQuery;

#[cfg(test)]
mod tests {
    use super::*;
    use nmp_grammar::{
        AccessContext, Binding, CacheMode, Derived, IdentityField, IndexedTagName, SourceAuthority,
    };

    fn host(n: u16) -> RelayUrl {
        RelayUrl::parse(&format!("wss://host-{n}.example.com")).expect("a well-formed host")
    }

    /// An app-suppliable relay set can be empty, so the door is fallible and
    /// NO scope exists on that path -- the invalid state is unconstructible
    /// rather than validated later.
    #[test]
    fn an_empty_relay_set_forms_no_scope() {
        assert_eq!(on([]).err(), Some(RelayScopeError::EmptyRelaySet));
    }

    #[test]
    fn duplicate_and_unsorted_hosts_canonicalize_to_one_sorted_set() {
        let a = on([host(2), host(1), host(2)]).expect("three inputs, two hosts");
        let b = on([host(1), host(2)]).expect("two hosts");
        assert_eq!(a, b);
        assert_eq!(a.hosts().len(), 2);
    }
    fn pinned(host: RelayUrl) -> SourceAuthority {
        SourceAuthority::Pinned(BTreeSet::from([host]))
    }

    fn derived(binding: &Binding) -> &Derived {
        match binding {
            Binding::Derived(inner) => inner,
            other => panic!("expected Derived, got {other:?}"),
        }
    }

    /// THE falsifier this issue turns on. A depth-2 NIP-29-owned graph over a
    /// TWO-host scope: for each host, the outer listing demand AND the inner
    /// member-evidence demand nested inside its `#d` binding must both be
    /// pinned to that host EXACTLY -- not to the other host, not to both, not
    /// to an inherited or defaulted source.
    ///
    /// Checking only the outer demand does not satisfy this issue: the
    /// silent-under-resolution bug lives entirely at the inner level.
    #[test]
    fn scope_stamps_exact_hosts_on_every_nested_nip29_demand() {
        let scope = on([host(1), host(2)]).expect("two hosts");
        let predicate = member_list_includes(Binding::Reactive(IdentityField::ActivePubkey));
        let branches = scope.listing_branches(&predicate);
        let d = IndexedTagName::new('d').expect("d is a single ASCII letter");

        assert_eq!(branches.len(), 2, "one complete branch per host");
        for (index, expected) in [host(1), host(2)].into_iter().enumerate() {
            let outer = &branches[index];
            assert_eq!(
                outer.source,
                pinned(expected.clone()),
                "depth 0 (the listing) must be pinned to {expected} alone"
            );
            assert_eq!(outer.cache, CacheMode::Strict);
            assert_eq!(outer.access, AccessContext::Public);

            let inner = &derived(
                outer
                    .selection
                    .tags
                    .get(&d)
                    .expect("the listing keys #d on the predicate"),
            )
            .inner;
            assert_eq!(
                inner.source,
                pinned(expected.clone()),
                "depth 1 (the member-list evidence) must be pinned to {expected} alone, \
                 not inherited and not cross-hosted"
            );
            assert_eq!(
                inner.cache,
                CacheMode::Strict,
                "depth 1 must also refuse a cached row {expected} never served -- Pinned \
                 scopes the wire, CacheMode scopes the cache, and both must name this host"
            );
            assert_eq!(inner.access, AccessContext::Public);
            assert_eq!(
                inner.selection.kinds,
                Some(BTreeSet::from([39002u16])),
                "the evidence branch is the member list NIP-29 defines"
            );
        }
    }

    /// A group read is likewise one complete branch per host, each scoped by
    /// `#h` and pinned to that host alone.
    #[test]
    fn a_group_read_is_one_complete_branch_per_host() {
        let scope = on([host(1), host(2)]).expect("two hosts");
        let group = scope.group("photographers");
        let branches = group
            .read_branches(nmp_grammar::Filter::default())
            .expect("a plain selection scopes");
        assert_eq!(branches.len(), 2);
        let h = IndexedTagName::new('h').expect("h is a single ASCII letter");
        for (branch, expected) in branches.iter().zip([host(1), host(2)]) {
            assert_eq!(branch.source, pinned(expected));
            assert_eq!(
                branch.selection.tags.get(&h),
                Some(&Binding::Literal(BTreeSet::from([
                    "photographers".to_string()
                ])))
            );
        }
    }

    /// Multi-relay reads are ONE ordinary live query with one complete
    /// singleton-host branch per host -- never `Pinned({A, B})`, never a
    /// `Vec<Demand>` the app has to merge, never a NIP-29 observe door.
    #[test]
    fn a_multi_host_read_is_one_live_query_with_one_branch_per_host() {
        let scope = on([host(1), host(2)]).expect("two hosts");
        let query = scope
            .group("photographers")
            .read(nmp_grammar::Filter::default())
            .expect("a two-host group read declares two branches");
        assert_eq!(query.branches().len(), 2);
        for (branch, expected) in query.branches().iter().zip([host(1), host(2)]) {
            assert_eq!(branch.source, pinned(expected));
        }
        assert_eq!(query.aggregate_result_limit(), None);
    }

    /// PROTOCOL-READSTHROUGHTHEONEDOOR-005 (direct half): two DISTINCT group
    /// ids narrowed from the SAME single-host scope produce branches that are
    /// identical in every respect except the one row that names the group --
    /// same host, same pinning, same kinds -- so a listing over both is
    /// separated by `#h` alone, never by an accidental difference elsewhere
    /// in the branch.
    #[test]
    fn two_group_ids_on_one_host_differ_only_in_their_h_branch() {
        let scope = on([host(1)]).expect("one host forms a scope");
        let selection = nmp_grammar::Filter {
            kinds: Some(BTreeSet::from([9u16])),
            ..nmp_grammar::Filter::default()
        };
        let photographers = scope
            .group("photographers")
            .read_branches(selection.clone())
            .expect("a plain selection scopes");
        let darkroom = scope
            .group("darkroom")
            .read_branches(selection)
            .expect("a plain selection scopes");
        assert_eq!(photographers.len(), 1);
        assert_eq!(darkroom.len(), 1);
        let h = IndexedTagName::new('h').expect("h is a single ASCII letter");

        assert_eq!(photographers[0].source, darkroom[0].source, "same host");
        assert_eq!(
            photographers[0].selection.kinds, darkroom[0].selection.kinds,
            "same app-selected kinds"
        );
        assert_eq!(
            photographers[0].selection.tags.get(&h),
            Some(&Binding::Literal(BTreeSet::from(["photographers".to_string()])))
        );
        assert_eq!(
            darkroom[0].selection.tags.get(&h),
            Some(&Binding::Literal(BTreeSet::from(["darkroom".to_string()])))
        );
        assert_ne!(
            photographers[0].selection.tags.get(&h),
            darkroom[0].selection.tags.get(&h),
            "the only difference between the two branches is the h row"
        );
    }

    #[test]
    fn a_multi_host_listing_is_one_live_query_with_one_branch_per_host() {
        let scope = on([host(1), host(2)]).expect("two hosts");
        let query = scope
            .groups_where(&member_list_includes(Binding::Reactive(
                IdentityField::ActivePubkey,
            )))
            .expect("a two-host listing declares two branches");
        assert_eq!(query.branches().len(), 2);
    }
}
