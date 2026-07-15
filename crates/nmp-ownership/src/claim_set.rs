//! [`ClaimSet`] -- the folded, overlap-checked claim table
//! (routing-and-ownership.md §4.2, layer 1's MECHANISM).

use std::fmt;

use crate::kind_claim::KindClaim;
use crate::kind_scope::KindScope;
use crate::module_id::ModuleId;
use crate::route_policy::RoutePolicy;

/// The folded, overlap-checked claim table (routing-and-ownership.md §4.2).
/// Layer 1's mechanism: engine construction (Unit E) folds linked modules'
/// claims through [`ClaimSet::build`] and propagates the typed error; layer
/// 2 (`nmp-audit`) folds every workspace module's claims through the same
/// function; layer 3's publish gate is [`ClaimSet::route_policy`] -- one
/// lookup, kind -> owning claim's policy.
#[derive(Clone, Debug, Default)]
pub struct ClaimSet {
    claims: Vec<KindClaim>,
}

/// Typed exclusivity-collision error: two claims whose scopes intersect
/// where at least one side is `exclusive`. Names both owners, both scopes,
/// and a witness kind concretely claimed by both (the legacy
/// `NMP-OWNERSHIP-COLLISION` map, minus the linker).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClaimOverlap {
    pub first_owner: ModuleId,
    pub first_scope: KindScope,
    pub second_owner: ModuleId,
    pub second_scope: KindScope,
    /// One concrete kind claimed by both.
    pub witness_kind: u16,
}

impl fmt::Display for ClaimOverlap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "NMP-OWNERSHIP-COLLISION: {} ({:?}) and {} ({:?}) both claim kind {}",
            self.first_owner,
            self.first_scope,
            self.second_owner,
            self.second_scope,
            self.witness_kind
        )
    }
}

impl std::error::Error for ClaimOverlap {}

impl ClaimSet {
    /// Folds `claims` in input order, erroring on the FIRST pair (`i < j`
    /// scan order over the input, deterministic) whose scopes overlap AND
    /// at least one of the two claims is `exclusive`. Two NON-exclusive
    /// overlapping claims are permitted (deliberate sharing, §4.1). A
    /// same-owner overlap is ALSO an error when exclusivity is involved --
    /// double-claiming is drift, and it is strictly safer to refuse than to
    /// guess it was intentional.
    pub fn build(claims: impl IntoIterator<Item = KindClaim>) -> Result<Self, ClaimOverlap> {
        let claims: Vec<KindClaim> = claims.into_iter().collect();
        for (i, first) in claims.iter().enumerate() {
            for second in &claims[(i + 1)..] {
                if !(first.exclusive || second.exclusive) {
                    // Deliberate sharing (§4.1): permitted.
                    continue;
                }
                if let Some(witness_kind) = first.scope.intersection_witness(&second.scope) {
                    return Err(ClaimOverlap {
                        first_owner: first.owner,
                        first_scope: first.scope.clone(),
                        second_owner: second.owner,
                        second_scope: second.scope.clone(),
                        witness_kind,
                    });
                }
            }
        }
        Ok(Self { claims })
    }

    /// An empty claim table -- today's all-`Automatic` behavior (no modules
    /// linked).
    pub fn empty() -> Self {
        Self::default()
    }

    /// The folded claims, in their original build order.
    pub fn claims(&self) -> &[KindClaim] {
        &self.claims
    }

    /// The exclusive claim covering `kind`, if any. Unique by construction
    /// of [`ClaimSet::build`] (two exclusive claims can never both cover
    /// the same kind -- that's exactly the overlap `build` refuses).
    pub fn exclusive_owner(&self, kind: u16) -> Option<&KindClaim> {
        self.claims
            .iter()
            .find(|claim| claim.exclusive && claim.scope.contains(kind))
    }

    /// The future layer-3 publish-gate lookup: the exclusive owner's
    /// `RoutePolicy` for `kind`, if the owning claim installed one.
    /// `None` means either the kind is unowned, or its owner didn't
    /// override routing -- both cases fall through to the default policy.
    pub fn route_policy(&self, kind: u16) -> Option<&RoutePolicy> {
        self.exclusive_owner(kind)
            .and_then(|claim| claim.route_policy.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relay_source::RelaySource;
    use crate::route_class::RouteClass;
    use crate::route_policy::{AppLanes, FailMode};

    fn claim(owner: &'static str, scope: KindScope, exclusive: bool) -> KindClaim {
        KindClaim {
            owner: ModuleId::new(owner),
            scope,
            exclusive,
            route_policy: None,
            discovery_ack: false,
        }
    }

    fn policy_bearing_claim() -> KindClaim {
        // Modeled on kind_claim.rs's nip17 fixture.
        KindClaim {
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
            discovery_ack: true,
        }
    }

    #[test]
    fn empty_and_default_are_equivalent_and_have_no_claims() {
        assert!(ClaimSet::empty().claims().is_empty());
        assert!(ClaimSet::default().claims().is_empty());
        assert!(ClaimSet::empty().exclusive_owner(0).is_none());
        assert!(ClaimSet::empty().route_policy(0).is_none());
    }

    #[test]
    fn non_exclusive_overlap_is_permitted() {
        let a = claim("shared-a", KindScope::Kind(42), false);
        let b = claim("shared-b", KindScope::Kind(42), false);
        let set = ClaimSet::build([a, b]).expect("deliberate sharing must fold ok");
        assert_eq!(set.claims().len(), 2);
    }

    #[test]
    fn exclusive_plus_nonexclusive_overlap_is_an_error() {
        let a = claim("exclusive-owner", KindScope::Kind(42), true);
        let b = claim("nonexclusive-reader", KindScope::Kind(42), false);
        let err = ClaimSet::build([a, b]).expect_err("one exclusive side must refuse the fold");
        assert_eq!(err.witness_kind, 42);
    }

    #[test]
    fn two_exclusive_claims_on_one_kind_is_an_error_with_correct_witness_and_owners() {
        let a = claim("owner-a", KindScope::Range(9000..=9030), true);
        let b = claim("owner-b", KindScope::Set(&[9015, 200]), true);
        let err = ClaimSet::build([a, b]).expect_err("overlapping exclusive scopes must refuse");
        assert_eq!(err.first_owner, ModuleId::new("owner-a"));
        assert_eq!(err.second_owner, ModuleId::new("owner-b"));
        assert_eq!(err.witness_kind, 9015);
        let msg = err.to_string();
        assert!(msg.starts_with("NMP-OWNERSHIP-COLLISION: "));
        assert!(msg.contains("owner-a"));
        assert!(msg.contains("owner-b"));
        assert!(msg.contains("9015"));
    }

    #[test]
    fn same_owner_double_claim_with_exclusivity_is_an_error() {
        let a = claim("double-claimer", KindScope::Kind(7), true);
        let b = claim("double-claimer", KindScope::Kind(7), true);
        let err = ClaimSet::build([a, b]).expect_err("same-owner exclusive double-claim is drift");
        assert_eq!(err.first_owner, ModuleId::new("double-claimer"));
        assert_eq!(err.second_owner, ModuleId::new("double-claimer"));
        assert_eq!(err.witness_kind, 7);
    }

    #[test]
    fn first_conflicting_pair_in_scan_order_is_reported_deterministically() {
        // Three claims: 0 and 2 conflict (both exclusive, overlapping);
        // 0 and 1 do not; 1 and 2 do not. The `i < j` scan order (i
        // outer, j inner) reaches (0, 1) then (0, 2) before ever
        // considering (1, 2) -- so the error must name claims 0 and 2,
        // not some other pair, and it must not depend on where the
        // non-conflicting claim 1 sits.
        let claim0 = claim("claim-zero", KindScope::Kind(50), true);
        let claim1 = claim("claim-one", KindScope::Kind(99), true);
        let claim2 = claim("claim-two", KindScope::Kind(50), true);
        let err = ClaimSet::build([claim0, claim1, claim2]).expect_err("0 and 2 overlap");
        assert_eq!(err.first_owner, ModuleId::new("claim-zero"));
        assert_eq!(err.second_owner, ModuleId::new("claim-two"));
        assert_eq!(err.witness_kind, 50);
    }

    #[test]
    fn exclusive_owner_and_route_policy_lookups() {
        let set = ClaimSet::build([policy_bearing_claim()]).expect("single claim folds ok");
        let owner = set.exclusive_owner(1059).expect("1059 is claimed");
        assert_eq!(owner.owner, ModuleId::new("nip17"));
        assert_eq!(
            set.route_policy(10050)
                .expect("policy attached")
                .route_class,
            RouteClass::VerifiedPrivateInbox
        );
        // A kind nobody claims: both lookups are None.
        assert!(set.exclusive_owner(9999).is_none());
        assert!(set.route_policy(9999).is_none());
    }

    #[test]
    fn unowned_kind_returns_none_from_both_lookups() {
        let set = ClaimSet::build([claim("some-owner", KindScope::Kind(1), true)])
            .expect("single claim folds ok");
        assert!(set.exclusive_owner(2).is_none());
        assert!(set.route_policy(2).is_none());
    }
}
