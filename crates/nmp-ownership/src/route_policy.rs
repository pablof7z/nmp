//! [`RoutePolicy`] -- the override primitive a [`crate::KindClaim`] may
//! attach for the kinds it owns (routing-and-ownership.md §3.1).

use crate::relay_source::RelaySource;
use crate::route_class::RouteClass;

/// Whether the app lanes (appRelay/fallbackRelay) still apply under a
/// `RoutePolicy` override. No default -- the default policy is `Apply`,
/// an override must say so explicitly (§3.1: "an override defaults to
/// Skip for BOTH read and write" is a modeling *convention* modules
/// follow, not a type-level default; the struct never lets a field go
/// unset).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AppLanes {
    Apply,
    Skip,
}

/// What happens when a `RoutePolicy`'s source resolves to zero relays.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum FailMode {
    Closed,
    OpenToAppLanes,
}

/// Supplied by a module for the kinds it OWNS (audited, Unit G). One
/// policy covers reads AND writes for those kinds. No `Default` impl --
/// every field is a load-bearing trust decision, and `route_class` alone
/// already can't be defaulted (`RouteClass` has none). A `RoutePolicy` is
/// only ever reachable attached to a `KindClaim` (`KindClaim::route_policy`)
/// -- there is no standalone "register a policy" API (§4.3).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RoutePolicy {
    /// Where relays come from when reading these kinds.
    pub read_source: RelaySource,
    /// Where relays come from when writing these kinds.
    pub write_source: RelaySource,
    /// Whether the app lanes still apply.
    pub app_lanes: AppLanes,
    /// What happens when the source resolves to ZERO relays.
    pub on_empty: FailMode,
    /// The typed provenance every wire route/publish under this policy
    /// carries.
    pub route_class: RouteClass,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_source::PinnedLane;

    /// Only possible from inside `nmp-ownership` -- `RouteClass` has no
    /// external constructor, so this is also a smoke test that the whole
    /// struct actually assembles and its fields round-trip.
    #[test]
    fn route_policy_constructs_and_fields_round_trip() {
        let nip17_reads = RoutePolicy {
            read_source: RelaySource::RelayListKind { kind: 10050 },
            write_source: RelaySource::RelayListKind { kind: 10050 },
            app_lanes: AppLanes::Skip,
            on_empty: FailMode::Closed,
            route_class: RouteClass::VerifiedPrivateInbox,
        };
        assert_eq!(
            nip17_reads.read_source,
            RelaySource::RelayListKind { kind: 10050 }
        );
        assert_eq!(nip17_reads.app_lanes, AppLanes::Skip);
        assert_eq!(nip17_reads.on_empty, FailMode::Closed);
        assert_eq!(nip17_reads.route_class, RouteClass::VerifiedPrivateInbox);

        let nip29_reads = RoutePolicy {
            read_source: RelaySource::PinnedLane(PinnedLane::GroupHost),
            write_source: RelaySource::PinnedLane(PinnedLane::GroupHost),
            app_lanes: AppLanes::Skip,
            on_empty: FailMode::Closed,
            route_class: RouteClass::HostPinned,
        };
        assert_eq!(nip29_reads.route_class, RouteClass::HostPinned);

        // Distinct instances with distinct field values are distinguishable.
        assert_ne!(nip17_reads, nip29_reads);

        // `Copy` -- policies are cheap value types, not handles.
        let copied = nip17_reads;
        assert_eq!(copied, nip17_reads);
    }
}
