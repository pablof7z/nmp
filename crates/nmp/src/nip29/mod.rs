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
        read::one_live_query(
            self.hosts
                .iter()
                .map(|host| nmp_nip29::groups_where_at(host, predicate.lower_at(host)))
                .collect(),
        )
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
}
