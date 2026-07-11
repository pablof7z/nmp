//! [`KindClaim`] -- one typed ownership fact per module
//! (routing-and-ownership.md §4.1).

use crate::kind_scope::KindScope;
use crate::module_id::ModuleId;
use crate::route_policy::RoutePolicy;

/// Declared by a protocol module, const/static data. Registered at engine
/// construction; collected by the Unit G workspace audit (`nmp-audit`).
/// A module exports `pub fn claims() -> &'static [KindClaim]`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct KindClaim {
    pub owner: ModuleId,
    pub scope: KindScope,
    /// `true`: no other module may claim an overlapping scope, and the
    /// runtime publish gate applies (§4.3). Non-exclusive claims exist
    /// for shared mechanisms (none known yet -- the variant exists so
    /// the audit can distinguish deliberate sharing from drift).
    pub exclusive: bool,
    /// Routing authority: present iff this module overrides routing for
    /// this scope. A `RoutePolicy` is ONLY reachable attached to a claim
    /// -- this field IS the gate (no ownership, no route override);
    /// `KindClaim` has no other way to carry one, so route authority is
    /// ownership by construction, not by convention.
    pub route_policy: Option<RoutePolicy>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_source::RelaySource;
    use crate::route_class::RouteClass;
    use crate::route_policy::{AppLanes, FailMode};

    #[test]
    fn kind_claim_constructs_with_and_without_a_route_policy() {
        let nip29 = KindClaim {
            owner: ModuleId::new("nip29"),
            scope: KindScope::Range(9000..=9030),
            exclusive: true,
            route_policy: None,
        };
        assert_eq!(nip29.owner.0, "nip29");
        assert!(nip29.route_policy.is_none());
        assert!(nip29.scope.contains(9010));

        let nip17 = KindClaim {
            owner: ModuleId::new("nip17"),
            scope: KindScope::Set(&[1059, 13, 14, 15, 10050]),
            exclusive: true,
            route_policy: Some(RoutePolicy {
                read_source: RelaySource::RelayListKind { kind: 10050 },
                write_source: RelaySource::RelayListKind { kind: 10050 },
                app_lanes: AppLanes::Skip,
                on_empty: FailMode::Closed,
                route_class: RouteClass::VerifiedPrivateInbox,
            }),
        };
        assert!(nip17.scope.contains(1059));
        let policy = nip17.route_policy.expect("nip17 overrides routing");
        assert_eq!(policy.route_class, RouteClass::VerifiedPrivateInbox);

        // The two real claims from the spec's own worked examples never
        // overlap each other's scope.
        assert!(!nip29.scope.overlaps(&nip17.scope));
    }
}
