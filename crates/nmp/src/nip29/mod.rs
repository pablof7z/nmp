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
mod groups;
mod predicate;
mod read;
mod records;

use std::collections::BTreeSet;

use nmp_grammar::Demand;
use nostr::RelayUrl;

use crate::engine::Engine;

pub use group::{Group, GroupPublishError};
pub use groups::Groups;
pub use nmp_nip29::GroupContextError;
pub use nmp_nip29::{
    current_account_group_list_demand, parse_simple_groups_list_from_raw_tags_tolerant,
    parse_simple_groups_list_tolerant, SimpleGroupEntry, SimpleGroupsList,
};
pub use nmp_nip29::{GroupUser, GroupUsersError};
pub use predicate::{
    admin_list_includes, all, any_of, groups_whose_record_matches, member_list_includes, GroupIds,
    GroupPredicate, GroupPredicateError,
};
pub use read::GroupReadError;
pub use records::{
    GroupAvailability, GroupObservation, GroupObserveError, GroupSnapshot, GroupWaitError,
    HostRecords,
};

// What one relay-signed record SAYS is `nmp-nip29`'s, beside the schema it
// parses. The facade re-exports the values so an app never imports two crates
// to read one snapshot.
pub use nmp_nip29::{GroupMetadata, GroupRecord, ListedRecord, ListedSubject};

// The NIP-29-owned composers, re-exported as themselves. An app that wants
// the raw builder for a named operation gets it; the group's own methods are
// the ordinary path.
pub use nmp_nip29::{
    add_users, create_group, create_invite, delete_event, delete_group, edit_metadata,
    join_request, leave_request, remove_users, GroupMetadataEdit, JoinAccess, ReadAccess,
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
    /// Contacts nothing, subscribes to nothing, and needs no current account:
    /// a kind:9021 join request into a group you cannot read yet is
    /// expressible. The returned [`Group`] retains this scope's hosts and the
    /// id privately; there is no accessor for either, so a layer that is
    /// handed a `Group` cannot reconstruct the authority and route something
    /// elsewhere under it. The one thing that does yield both is the write
    /// door itself ([`Group::intent`]), which mints them into an ordinary
    /// [`WriteIntent`](crate::WriteIntent) -- see [`Group`]'s own doc for why
    /// that trade is the right one.
    #[must_use]
    pub fn group(&self, group_id: impl Into<String>) -> Group {
        Group::new(self.hosts.clone(), group_id.into())
    }

    /// Narrow to the SEVERAL groups one write belongs to, keeping the same
    /// hosts (#1281).
    ///
    /// The write-only sibling of [`Self::group`], for the one event shape a
    /// single group id cannot express: a kind:30315 session status is
    /// addressable at `(author, d=status)` and carries one `h` per room the
    /// session occupies, so publishing it once per room would make each copy
    /// replace the last. See [`Groups`] for why every workaround is worse.
    ///
    /// Fallible for exactly the reason [`on`] is: the id set is
    /// caller-supplied and can be empty, and an event with no `h` row is not
    /// in a group at all. Duplicates collapse and order is the id order, so
    /// two callers naming the same rooms differently get the SAME value.
    ///
    /// It is a write context and nothing else — no read, no records, no
    /// named operation. Those are all per-group by definition, and
    /// [`Self::group`] is where they live.
    pub fn groups(
        &self,
        group_ids: impl IntoIterator<Item = impl Into<String>>,
    ) -> Result<Groups, GroupContextError> {
        Groups::new(
            self.hosts.clone(),
            group_ids.into_iter().map(Into::into).collect(),
        )
    }

    /// Watch the relay-signed records of every group matching `predicate`.
    ///
    /// One complete branch per host: at host `H` the read selects exactly the
    /// kinds `records` names, pinned to `H`, keyed on `d` by the predicate
    /// lowered AT `H` -- or keyed on nothing at all, when the predicate is
    /// [`all`]. Evidence observed at one relay therefore never constrains a
    /// listing at another.
    ///
    /// Each delivery is a complete [`GroupSnapshot`] per matching group --
    /// metadata, admins, members, availability, and the per-host breakdown
    /// beside them. The app never sees a row delta and never walks a `p` row.
    ///
    /// ```text
    /// let watching = nip29::on(hosts)?.observe(
    ///     &engine,
    ///     nip29::member_list_includes(Binding::Reactive(IdentityField::ActivePubkey))
    ///         .union([nip29::any_of(Binding::Literal(pinned_ids))]),
    ///     [GroupRecord::Metadata, GroupRecord::Admins, GroupRecord::Members],
    ///     None,
    /// )?;
    ///
    /// // a directory of everything this relay advertises, 250 rooms per host
    /// let browsing = nip29::on(hosts)?.observe(
    ///     &engine, nip29::all(), [GroupRecord::Metadata], Some(250),
    /// )?;
    /// while let Some(snapshots) = watching.next().await? { /* ... */ }
    /// ```
    ///
    /// # `limit` bounds each host's own branch
    ///
    /// It is the ordinary NIP-01 `Filter::limit`, applied to every branch, and
    /// it is the ONLY bound this door offers -- there is no `all`-specific
    /// knob, because unboundedness is not `all`-specific: a relay that lists
    /// one subject in very many groups makes `member_list_includes` just as
    /// large. `None` asks for whatever the relay chooses to answer with.
    ///
    /// It is deliberately NOT a
    /// [`LiveQuery`](nmp_grammar::LiveQuery)-level aggregate bound. Two hosts
    /// with `Some(250)` may deliver up to 500 snapshots, because each host was
    /// asked for 250 of its OWN; presenting a per-branch bound as a global one
    /// would be a second owner of row membership.
    ///
    /// Branches scale with HOSTS, not groups: a hundred groups on two relays
    /// is two branches.
    pub fn observe(
        &self,
        engine: &Engine,
        predicate: impl Into<GroupPredicate>,
        records: impl IntoIterator<Item = GroupRecord>,
        limit: Option<usize>,
    ) -> Result<GroupObservation, GroupObserveError> {
        let records: BTreeSet<GroupRecord> = records.into_iter().collect();
        if records.is_empty() {
            return Err(GroupObserveError::NoRecordSelected);
        }
        let predicate = predicate.into();
        records::observe(
            engine,
            self.hosts.clone(),
            BTreeSet::new(),
            self.records_branches(&predicate, &records, limit),
        )
    }

    /// One complete records branch per host, in canonical host order. Split
    /// out so the per-branch source-stamping property is assertable on its own
    /// -- it is the property the whole design exists to guarantee, and it must
    /// be provable for a MULTI-host scope without depending on how branches
    /// are later aggregated into one live query.
    pub(crate) fn records_branches(
        &self,
        predicate: &GroupPredicate,
        records: &BTreeSet<GroupRecord>,
        limit: Option<usize>,
    ) -> Vec<Demand> {
        self.hosts
            .iter()
            .map(|host| nmp_nip29::group_records_at(host, records, predicate.lower_at(host), limit))
            .collect()
    }

    #[cfg(test)]
    fn hosts(&self) -> &BTreeSet<RelayUrl> {
        &self.hosts
    }
}

/// Name the relays and narrow to one group id in one call.
///
/// Sugar over [`on`] plus [`RelayScope::group`], for the overwhelmingly
/// common case: an app opening one room already knows its id, and should not
/// have to phrase a discovery predicate to watch it.
pub fn group(
    hosts: impl IntoIterator<Item = RelayUrl>,
    group_id: impl Into<String>,
) -> Result<Group, RelayScopeError> {
    Ok(on(hosts)?.group(group_id))
}

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
        let branches = scope.records_branches(
            &predicate.clone().into(),
            &BTreeSet::from([GroupRecord::Members]),
            None,
        );
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
            Some(&Binding::Literal(BTreeSet::from([
                "photographers".to_string()
            ])))
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
        let query = read::one_live_query(scope.records_branches(
            &member_list_includes(Binding::Reactive(IdentityField::ActivePubkey)).into(),
            &BTreeSet::from([GroupRecord::Metadata]),
            None,
        ))
        .expect("a two-host listing declares two branches");
        assert_eq!(query.branches().len(), 2);
    }

    /// THE #1252 falsifier at the door an app actually calls. A directory
    /// asks every host for the groups it advertises, and every branch carries
    /// NO group-id row: the ids a directory wants are the ANSWER, so a branch
    /// that still keyed itself on some id set would show the app only rooms it
    /// already knew -- indistinguishable, on screen, from a relay hosting
    /// nothing.
    #[test]
    fn an_unconstrained_directory_asks_every_host_with_no_group_id_row() {
        let scope = on([host(1), host(2)]).expect("two hosts");
        let branches =
            scope.records_branches(&all(), &BTreeSet::from([GroupRecord::Metadata]), Some(250));
        let d = IndexedTagName::new('d').expect("d is a single ASCII letter");

        assert_eq!(branches.len(), 2, "one complete branch per host");
        for (branch, expected) in branches.iter().zip([host(1), host(2)]) {
            assert_eq!(branch.source, pinned(expected));
            assert_eq!(branch.cache, CacheMode::Strict);
            assert_eq!(branch.selection.kinds, Some(BTreeSet::from([39000u16])));
            assert_eq!(
                branch.selection.tags.get(&d),
                None,
                "an unconstrained directory must not key itself on any group id"
            );
            assert!(branch.selection.tags.is_empty());
            assert_eq!(
                branch.selection.limit,
                Some(250),
                "the app's own per-host bound is the only thing bounding it"
            );
        }
    }

    /// The bound is the ordinary NIP-01 per-branch `limit`, and it is never
    /// promoted to a bound on the merged union. Two hosts asked for 250 of
    /// their OWN groups were asked for 250 each; declaring 250 globally would
    /// make the live query a second owner of row membership.
    #[test]
    fn a_per_host_bound_is_never_reported_as_a_bound_on_the_union() {
        let scope = on([host(1), host(2)]).expect("two hosts");
        let query = read::one_live_query(scope.records_branches(
            &all(),
            &BTreeSet::from([GroupRecord::Metadata]),
            Some(250),
        ))
        .expect("a two-host directory declares two branches");
        assert_eq!(query.branches().len(), 2);
        for branch in query.branches() {
            assert_eq!(branch.selection.limit, Some(250));
        }
        assert_eq!(query.aggregate_result_limit(), None);
    }

    /// The record selection is the app's, and only the kinds it named reach
    /// the wire. A directory screen paying two relays for two lists it never
    /// renders is a real per-relay cost, so "all three" is not a default.
    #[test]
    fn only_the_selected_records_reach_the_wire() {
        let scope = on([host(1)]).expect("one host");
        let predicate: GroupPredicate =
            member_list_includes(Binding::Reactive(IdentityField::ActivePubkey)).into();
        for (records, expected) in [
            (BTreeSet::from([GroupRecord::Metadata]), vec![39000u16]),
            (BTreeSet::from([GroupRecord::Admins]), vec![39001u16]),
            (BTreeSet::from([GroupRecord::Members]), vec![39002u16]),
            (
                BTreeSet::from([GroupRecord::Metadata, GroupRecord::Members]),
                vec![39000u16, 39002u16],
            ),
        ] {
            let branches = scope.records_branches(&predicate, &records, None);
            assert_eq!(
                branches[0].selection.kinds,
                Some(expected.iter().copied().collect::<BTreeSet<u16>>()),
                "selecting {records:?} must ask a relay for exactly {expected:?}"
            );
        }
    }

    /// A scope-wide observation is refused, not opened empty, when the app
    /// selected no record: an empty kind set matches nothing and would
    /// deliver a permanently empty snapshot -- the same
    /// indistinguishable-from-real-emptiness failure #1245 was about.
    #[test]
    fn an_empty_record_selection_is_refused_rather_than_observed() {
        let engine = crate::Engine::new(crate::EngineConfig::default()).expect("an engine");
        let scope = on([host(1)]).expect("one host");
        assert_eq!(
            scope
                .observe(
                    &engine,
                    member_list_includes(Binding::Reactive(IdentityField::ActivePubkey)),
                    [],
                    None,
                )
                .err(),
            Some(GroupObserveError::NoRecordSelected)
        );
        assert_eq!(
            scope.group("photographers").observe(&engine, []).err(),
            Some(GroupObserveError::NoRecordSelected)
        );
        engine.shutdown();
    }
}
