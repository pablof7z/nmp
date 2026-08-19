//! [`RelayScope`] and [`on`] -- the top of the NIP-29 door (#1033).
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
//! # Why this crate owns all of it now
//!
//! Moved back here from `nmp` by #1707. This crate is engine-free by
//! construction for its schema/tag/predicate half, but the final door needs
//! both halves at once: the retained relay scope AND the opaque write intent
//! the one publish door takes -- so `Group`/`Groups`/`GroupListWrites`/the
//! records observation all live in this crate too, reaching `nmp`'s own
//! engine surface as an ordinary `nmp-nip29 -> nmp` dependency. `nmp` must
//! not know what a NIP-29 group or a kind:10009 saved-groups list means.
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

use std::collections::BTreeSet;

use nmp_grammar::Demand;
use nostr::RelayUrl;

use nmp::Engine;

use crate::{Group, GroupObservation, GroupObserveError, GroupPredicate, GroupRecord, Groups};

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
    /// door itself ([`Group::publish`]), which mints them into an ordinary
    /// [`WriteIntent`](nmp_grammar::WriteIntent) -- see [`Group`]'s own doc
    /// for why that trade is the right one.
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
    ) -> Result<Groups, crate::GroupContextError> {
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
    /// [`all`](crate::all). Evidence observed at one relay therefore never
    /// constrains a listing at another.
    ///
    /// Each delivery is a complete [`GroupSnapshot`](crate::GroupSnapshot) per
    /// matching group -- metadata, admins, members, availability, and the
    /// per-host breakdown beside them. The app never sees a row delta and
    /// never walks a `p` row.
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
        crate::record_observation::observe(
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
            .map(|host| crate::group_records_at(host, records, predicate.lower_at(host), limit))
            .collect()
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

