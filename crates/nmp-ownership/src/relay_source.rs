//! [`RelaySource`] -- where a [`crate::RoutePolicy`] sources relays
//! (routing-and-ownership.md §3.1).

/// The subset of `nmp-router`'s `Lane` vocabulary (`facts.rs::Lane`) that
/// a `RelaySource` can pin to.
///
/// `nmp-ownership` deliberately does NOT depend on `nmp-router` -- that
/// dependency direction is backwards for the modularity north star: every
/// future `nmp-mod-*` protocol crate must be able to depend on
/// `nmp-ownership` alone, without linking the whole router
/// (routing-build-plan.md §7.1, owner-resolved Q7; an `nmp-ownership` ->
/// `nmp-router` edge would make that impossible, a `nmp-router` ->
/// `nmp-ownership` edge is fine and is exactly what Unit E adds).
///
/// `Lane` is also the wrong shape here even ignoring the dependency
/// direction: it's the superset of every relay-bearing FACT lane
/// (`Nip65Write`, `Hint`, `AppRelay`, ...), most of which are never legal
/// pin targets for a policy. Only the lanes that are already module-fed,
/// single-purpose pinned facts qualify (routing-and-ownership.md §5: the
/// built seam -- "`Lane::GroupHost`/`Lane::DmInbox` ... were built as
/// exactly this kind of module-fed fact; Part B/C give them their
/// supplier"). So this is its own small, closed enum naming only those
/// lanes, not a re-export or a newtype wrapper around `Lane`. When
/// `nmp-router` adopts `RelaySource` (Unit E), it converts between the
/// two explicitly (a `From<PinnedLane> for Lane` on the router side) --
/// there is no shared type, by design.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub enum PinnedLane {
    /// NIP-29 group-anchor relays derived from group state.
    GroupHost,
    /// kind:10050 DM inbox relays.
    DmInbox,
}

/// Where a `RoutePolicy` sources relays for reads or writes. CLOSED
/// vocabulary -- extend the enum through review, never admit a
/// closure/callback (VISION §10: values in, code after).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RelaySource {
    /// The default: author's write-marked 10002 (reads/author-writes) +
    /// p-tag recipients' read-marked 10002 (writes), §2.
    Nip65Default,
    /// A per-pubkey replaceable relay-list kind other than 10002 (NIP-17
    /// -> kind:10050; drafts -> the user's draft-relay list kind).
    /// Discovery rides `sync_discovery` unchanged: every 1xxxx kind is
    /// already a `DiscoveryKind`.
    RelayListKind { kind: u16 },
    /// Pinned facts the owning module already ingested into the
    /// directory under a named lane (NIP-29 -> `PinnedLane::GroupHost`).
    /// The router reads `pinned_relays()`; it never knows what a "group"
    /// is.
    PinnedLane(PinnedLane),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relay_source_variants_construct_and_compare() {
        assert_eq!(RelaySource::Nip65Default, RelaySource::Nip65Default);
        assert_ne!(
            RelaySource::RelayListKind { kind: 10050 },
            RelaySource::RelayListKind { kind: 10002 }
        );
        assert_eq!(
            RelaySource::PinnedLane(PinnedLane::GroupHost),
            RelaySource::PinnedLane(PinnedLane::GroupHost)
        );
        assert_ne!(
            RelaySource::PinnedLane(PinnedLane::GroupHost),
            RelaySource::PinnedLane(PinnedLane::DmInbox)
        );
    }
}
