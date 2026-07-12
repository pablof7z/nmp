//! Host-scoped NIP-29 read constructors (#108) -- the selected host rides
//! ENTIRELY as `SourceAuthority::Pinned({host})` on the `Demand` itself
//! (#107's primitive), never as a directory-fact `Lane::GroupHost` pinned
//! lookup (`nmp-router`'s `RelayDirectory::pinned_relays`/
//! `FixtureDirectory::with_group_host`) -- that path is a DIFFERENT,
//! test-fixture-only mechanism for operator/directory-discovered facts,
//! and reusing it here would launder a query-declared selection (the user
//! explicitly picked which host to browse) as if it were operator-
//! configured relay state, exactly what #108 warns against. Because the
//! host lives in `Demand::source`, it already flows through
//! `ContextualAtom` identity, per-source `AcquisitionEvidence`, and
//! diagnostics for free -- no new mechanism needed.

use std::collections::BTreeSet;

use nmp_grammar::{AccessContext, Binding, Demand, Filter, IndexedTagName, SourceAuthority};
use nostr::RelayUrl;

/// Group discovery on a selected host: `kinds:[39000]`, pinned to exactly
/// that host. INFALLIBLE -- see this module's doc for why both of
/// `Demand::new`'s `DemandError` variants are unreachable for a singleton
/// pinned relay set.
pub fn group_discovery_demand(host: RelayUrl) -> Demand {
    pinned_demand(
        Filter {
            kinds: Some(BTreeSet::from([39000u16])),
            ..Filter::default()
        },
        host,
    )
}

/// Group content on a selected host, scoped by the group's `h` tag:
/// `kinds:[9, 30315]`. INFALLIBLE, same reasoning as
/// [`group_discovery_demand`].
pub fn group_content_demand(host: RelayUrl, group_id: &str) -> Demand {
    let h = IndexedTagName::new('h').expect("'h' is an ASCII letter");
    pinned_demand(
        Filter {
            kinds: Some(BTreeSet::from([9u16, 30315u16])),
            tags: std::collections::BTreeMap::from([(
                h,
                Binding::Literal(BTreeSet::from([group_id.to_string()])),
            )]),
            ..Filter::default()
        },
        host,
    )
}

/// Shared constructor: `Demand::new(selection, Pinned({host}), Public)`,
/// unwrapped via `expect` rather than propagating a `Result` -- both of
/// `Demand::new`'s validation rules are UNREACHABLE for every call site in
/// this module:
/// - `PinnedRequiresNonemptyRelaySet` never fires: `{host}` is a
///   single-element set, structurally always non-empty.
/// - `AuthorOutboxesRequiresBoundAuthors` never fires: the source here is
///   always `Pinned`, never `AuthorOutboxes`, so that rule doesn't apply
///   regardless of `selection.authors`.
///
/// If a future caller widens either constructor to accept a caller-
/// supplied relay SET (rather than one fixed selected host), fallibility
/// MUST be restored here -- an app-suppliable set can be empty.
fn pinned_demand(selection: Filter, host: RelayUrl) -> Demand {
    Demand::new(
        selection,
        SourceAuthority::Pinned(BTreeSet::from([host])),
        AccessContext::Public,
    )
    .expect("a singleton pinned relay set can never violate Demand::new's validation rules")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host(n: u16) -> RelayUrl {
        RelayUrl::parse(&format!("wss://host-{n}.example.com")).unwrap()
    }

    #[test]
    fn group_discovery_demand_pins_exactly_the_selected_host() {
        let demand = group_discovery_demand(host(1));
        assert_eq!(demand.selection.kinds, Some(BTreeSet::from([39000u16])));
        assert_eq!(
            demand.source,
            SourceAuthority::Pinned(BTreeSet::from([host(1)]))
        );
        assert_eq!(demand.access, AccessContext::Public);
    }

    #[test]
    fn group_content_demand_scopes_by_h_tag_and_pins_host() {
        let demand = group_content_demand(host(1), "group-a");
        assert_eq!(
            demand.selection.kinds,
            Some(BTreeSet::from([9u16, 30315u16]))
        );
        let h = IndexedTagName::new('h').unwrap();
        assert_eq!(
            demand.selection.tags.get(&h),
            Some(&Binding::Literal(BTreeSet::from(["group-a".to_string()])))
        );
        assert_eq!(
            demand.source,
            SourceAuthority::Pinned(BTreeSet::from([host(1)]))
        );
    }

    /// #108 Done-when: "Equal group filters on different hosts retain
    /// separate identity" -- the protocol-level instance of #107's own
    /// R1-vs-R2 engine falsifier: the identical group_id on two different
    /// hosts must produce two DIFFERENT `Demand`s (different `source`,
    /// hence different `atom_context()`/identity), never one aliased onto
    /// the other.
    #[test]
    fn identical_group_content_on_different_hosts_yields_distinct_demands() {
        let on_host_1 = group_content_demand(host(1), "group-a");
        let on_host_2 = group_content_demand(host(2), "group-a");
        assert_eq!(on_host_1.selection, on_host_2.selection);
        assert_ne!(on_host_1.source, on_host_2.source);
        assert_ne!(
            on_host_1.atom_context(),
            on_host_2.atom_context(),
            "same selection, different pinned host, must never alias identity"
        );
    }
}
