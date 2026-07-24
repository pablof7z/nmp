use nmp_ownership::{KindClaim, KindScope, ModuleId};

const CLAIMS: [KindClaim; 1] = [KindClaim {
    owner: ModuleId::new("nip65"),
    scope: KindScope::Kind(10002),
    exclusive: true,
    route_policy: None,
    // Kind 10002 is in DiscoveryKinds (10000..=19999). Its ordinary
    // acquisition is intentionally indexer-eligible; only the first
    // publication needs the separate bootstrap operation.
    discovery_ack: true,
}];

/// NIP-65 owns the kind:10002 relay-list schema. Its first-publication
/// bootstrap route is a contextual contribution for this exact schema, not a
/// replacement for the engine's ordinary AuthorOutbox policy after ingest.
pub fn claims() -> &'static [KindClaim] {
    &CLAIMS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nip65_exclusively_claims_kind_10002_and_acknowledges_discovery() {
        assert_eq!(claims().len(), 1);
        assert_eq!(claims()[0].owner, ModuleId::new("nip65"));
        assert!(claims()[0].exclusive);
        assert!(claims()[0].scope.contains(10002));
        assert!(claims()[0].route_policy.is_none());
        assert!(claims()[0].discovery_ack);
    }
}
