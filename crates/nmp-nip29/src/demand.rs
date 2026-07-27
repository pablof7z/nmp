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

use nmp_grammar::{AccessContext, Demand, Filter, SourceAuthority};
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
}
