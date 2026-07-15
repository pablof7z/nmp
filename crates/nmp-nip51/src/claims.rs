//! `nmp-nip51`'s ownership declaration (#63/#108, `nmp-ownership`'s
//! `KindClaim` vocabulary).

use nmp_ownership::{KindClaim, KindScope, ModuleId};

/// `nmp-nip51`'s one and only claim: kind:10009, exclusive. `route_policy:
/// None`: this crate does not override wire routing for the kind it owns
/// -- the read uses the ordinary `AuthorOutboxes` default (see
/// [`crate::active_account_demand`]), so there is no routing authority to
/// attach.
const CLAIMS: [KindClaim; 1] = [KindClaim {
    owner: ModuleId::new("nip51"),
    scope: KindScope::Kind(10009),
    exclusive: true,
    route_policy: None,
    // Kind 10009 falls in the 1xxxx replaceable-list discovery range
    // (10000..=19999, DiscoveryKinds) -- consciously acknowledged.
    discovery_ack: true,
}];

pub fn claims() -> &'static [KindClaim] {
    &CLAIMS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nip51_exclusively_claims_kind_10009() {
        let claims = claims();
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].owner, ModuleId::new("nip51"));
        assert!(claims[0].exclusive);
        assert!(claims[0].scope.contains(10009));
        assert!(claims[0].route_policy.is_none());
        // Kind 10009 is a discovery kind -- the claim must consciously
        // acknowledge that (routing-and-ownership.md §4.2 layer 2 check c).
        assert!(claims[0].discovery_ack);
    }
}
