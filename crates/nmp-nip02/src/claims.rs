use nmp_ownership::{KindClaim, KindScope, ModuleId};

const CLAIMS: [KindClaim; 1] = [KindClaim {
    owner: ModuleId::new("nip02"),
    scope: KindScope::Kind(3),
    exclusive: true,
    route_policy: None,
}];

/// NIP-02 owns the kind:3 contact-list schema. Acquisition and publication
/// still use NMP's ordinary author-outbox routing; there is no module-owned
/// relay policy to install.
pub fn claims() -> &'static [KindClaim] {
    &CLAIMS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nip02_exclusively_claims_kind_3() {
        assert_eq!(claims().len(), 1);
        assert_eq!(claims()[0].owner, ModuleId::new("nip02"));
        assert!(claims()[0].exclusive);
        assert!(claims()[0].scope.contains(3));
        assert!(claims()[0].route_policy.is_none());
    }
}
