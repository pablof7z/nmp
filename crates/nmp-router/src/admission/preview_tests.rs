use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, ContextualAtom, ReadRouting, RelaySessionKey};
use nmp_store::coverage_key;
use nostr::RelayUrl;

use crate::facts::LocalFacts;
use crate::{
    AdvertisedRelayLimits, CompileBudget, DemandKey, Router, RuleRegistry, SubId, WireReq,
};

fn pinned(relay: &RelayUrl, kind: u16) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    }
}

fn request_count(preview: &crate::AdmissionPreview) -> usize {
    preview.plan.reqs.values().map(Vec::len).sum()
}

#[test]
fn a_new_source_cannot_borrow_freshness_after_the_global_relay_cap_is_full() {
    let incumbent_relay = RelayUrl::parse("wss://preview-cap-incumbent.example").unwrap();
    let candidate_relay = RelayUrl::parse("wss://preview-cap-candidate.example").unwrap();
    let incumbent = pinned(&incumbent_relay, 1);
    let candidate = pinned(&candidate_relay, 2);
    let candidate_demand = DemandKey::for_atom(&candidate);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.admit(&BTreeSet::from([incumbent]), &LocalFacts::new(), 1);

    let before = router.ownership_census();
    let preview = router.preview_admission(&BTreeSet::from([candidate]), &LocalFacts::new(), 1);

    assert_eq!(request_count(&preview), 0);
    assert_eq!(
        preview.plan.limited_demands,
        BTreeSet::from([candidate_demand])
    );
    assert_eq!(router.ownership_census(), before);
}

#[test]
fn an_exact_running_request_remains_freshness_eligible_when_the_cap_is_full() {
    let relay = RelayUrl::parse("wss://preview-exact-incumbent.example").unwrap();
    let atom = pinned(&relay, 1);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.admit(&BTreeSet::from([atom.clone()]), &LocalFacts::new(), 1);

    let before = router.ownership_census();
    let preview = router.preview_admission(&BTreeSet::from([atom]), &LocalFacts::new(), 1);

    assert_eq!(request_count(&preview), 1);
    assert!(preview.plan.limited_demands.is_empty());
    assert_eq!(preview.work.candidate_atoms, 1);
    assert_eq!(preview.work.incumbent_demand_edges_visited, 1);
    assert_eq!(preview.work.incumbent_request_entries_visited, 1);
    assert_eq!(preview.work.coalesce_pair_attempts, 0);
    assert_eq!(router.ownership_census(), before);
}

#[test]
fn a_new_request_cannot_borrow_freshness_after_the_session_budget_is_full() {
    let relay = RelayUrl::parse("wss://preview-subscription-budget.example").unwrap();
    let incumbent = pinned(&relay, 1);
    let candidate = pinned(&relay, 2);
    let candidate_demand = DemandKey::for_atom(&candidate);
    let budget = CompileBudget::with_relay_cap(20).advertising(
        relay,
        AdvertisedRelayLimits {
            max_subscriptions: Some(1),
            max_subid_length: None,
        },
    );
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.admit(
        &BTreeSet::from([incumbent]),
        &LocalFacts::new(),
        budget.clone(),
    );

    let before = router.ownership_census();
    let preview =
        router.preview_admission(&BTreeSet::from([candidate]), &LocalFacts::new(), budget);

    assert_eq!(request_count(&preview), 0);
    assert_eq!(
        preview.plan.limited_demands,
        BTreeSet::from([candidate_demand])
    );
    assert_eq!(router.ownership_census(), before);
}

#[test]
fn one_preview_never_visits_ten_thousand_unrelated_incumbent_demand_edges() {
    const INCUMBENTS: u16 = 10_000;
    let incumbent_relay = RelayUrl::parse("wss://preview-10k-incumbent.example").unwrap();
    let candidate_relay = RelayUrl::parse("wss://preview-10k-candidate.example").unwrap();
    let session = RelaySessionKey::unauthenticated(incumbent_relay.clone());
    let mut atoms = Vec::with_capacity(INCUMBENTS as usize);
    let mut kinds = BTreeSet::new();
    let mut owner_demands = BTreeSet::new();
    let mut coverage_claims = BTreeSet::new();
    for kind in 1..=INCUMBENTS {
        let atom = pinned(&incumbent_relay, kind);
        kinds.insert(kind);
        owner_demands.insert(DemandKey::for_atom(&atom));
        coverage_claims.insert(coverage_key(&atom));
        atoms.push(atom);
    }
    let physical = ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(kinds),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![incumbent_relay.clone()]),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    };
    let sub_id = SubId::allocate(
        incumbent_relay,
        &physical.routing,
        physical.authenticate_as,
        1,
    );
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.prev_plan.reqs.insert(
        session,
        vec![WireReq {
            sub_id,
            filter: physical.filter,
            routing: physical.routing,
            provenance: BTreeSet::new(),
            coverage_claims,
            owner_demands,
            coverage_assignments: BTreeSet::new(),
        }],
    );
    router.rebuild_active_indexes(atoms);

    let before = router.ownership_census();
    let preview = router.preview_admission(
        &BTreeSet::from([pinned(&candidate_relay, 20_000)]),
        &LocalFacts::new(),
        20,
    );

    assert_eq!(request_count(&preview), 1);
    assert_eq!(preview.work.candidate_atoms, 1);
    assert_eq!(preview.work.incumbent_demand_edges_visited, 0);
    assert_eq!(preview.work.incumbent_request_entries_visited, 0);
    assert_eq!(preview.work.coalesce_pair_attempts, 0);
    assert_eq!(router.ownership_census(), before);
}
