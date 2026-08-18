use std::collections::BTreeSet;

use nmp_grammar::{ConcreteFilter, ContextualAtom, ReadRouting, RelaySessionKey};
use nmp_store::coverage_key;
use nostr::{Keys, RelayUrl};

use crate::facts::LocalFacts;
use crate::{
    DemandKey, Lane, RouteKind, RouteProvenance, Router, RuleRegistry, SubId, WireOp, WireReq,
};
use std::time::Instant;

fn pinned(relay: &RelayUrl, kinds: impl IntoIterator<Item = u16>) -> ContextualAtom {
    ContextualAtom {
        filter: ConcreteFilter {
            kinds: Some(kinds.into_iter().collect()),
            ..ConcreteFilter::default()
        },
        routing: ReadRouting::Explicit(vec![relay.clone()]),
        authenticate_as: None,
        routing_evidence: BTreeSet::new(),
    }
}

#[test]
fn exact_metadata_attach_examines_only_candidate_entries_over_ten_thousand_incumbent_claims() {
    let relay = RelayUrl::parse("wss://metadata-delta-10k.example").unwrap();
    let wide = pinned(&relay, [1, 2]);
    let one = pinned(&relay, [1]);
    let two = pinned(&relay, [2]);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.admit(&BTreeSet::from([wide.clone()]), &LocalFacts::new(), 20);

    let incumbent = router.prev_plan.reqs.values_mut().flatten().next().unwrap();
    for kind in 10_000..20_000 {
        incumbent
            .coverage_claims
            .insert(coverage_key(&pinned(&relay, [kind])));
    }
    assert_eq!(incumbent.coverage_claims.len(), 10_001);

    router.reset_admission_work();
    let outcome = router.admit(
        &BTreeSet::from([one.clone(), two.clone()]),
        &LocalFacts::new(),
        20,
    );
    assert!(outcome.wire.ops.is_empty());
    assert_eq!(outcome.request_metadata_updates.len(), 1);
    assert_eq!(
        outcome.request_metadata_updates[0]
            .added_owner_demands
            .len(),
        2
    );
    assert_eq!(
        outcome.request_metadata_updates[0]
            .added_coverage_claims
            .len(),
        2
    );
    assert_eq!(router.admission_work().metadata_entries_examined, 5);

    for atom in [wide, one, two] {
        router.withdraw([atom], 20);
    }
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn withdrawing_one_attached_owner_prunes_only_its_local_request_metadata() {
    let relay = RelayUrl::parse("wss://metadata-detach.example").unwrap();
    let wide = pinned(&relay, [1, 2]);
    let one = pinned(&relay, [1]);
    let two = pinned(&relay, [2]);
    let wide_demand = DemandKey::for_atom(&wide);
    let one_demand = DemandKey::for_atom(&one);
    let two_demand = DemandKey::for_atom(&two);
    let wide_claim = coverage_key(&wide);
    let one_claim = coverage_key(&one);
    let two_claim = coverage_key(&two);
    let mut router = Router::new(RuleRegistry::default_widen_only());

    router.admit(&BTreeSet::from([wide.clone()]), &LocalFacts::new(), 20);
    let attached = router.admit(
        &BTreeSet::from([one.clone(), two.clone()]),
        &LocalFacts::new(),
        20,
    );
    assert!(attached.wire.ops.is_empty());

    let withdrawal = router.withdraw([one], 20);
    assert!(withdrawal.wire.ops.is_empty());
    assert_eq!(withdrawal.request_metadata_removals.len(), 1);
    assert_eq!(
        withdrawal.request_metadata_removals[0].removed_owner_demands,
        BTreeSet::from([one_demand.clone()])
    );
    assert_eq!(
        withdrawal.request_metadata_removals[0].removed_coverage_claims,
        BTreeSet::from([one_claim])
    );
    let request = router.prev_plan.reqs.values().flatten().next().unwrap();
    assert_eq!(
        request.owner_demands,
        BTreeSet::from([wide_demand.clone(), two_demand.clone()])
    );
    assert_eq!(
        request.coverage_claims,
        BTreeSet::from([wide_claim, two_claim.clone()])
    );
    assert!(!router.requests_by_demand.contains_key(&one_demand));
    assert!(router.requests_by_demand.contains_key(&wide_demand));
    assert!(router.requests_by_demand.contains_key(&two_demand));

    let withdrawal = router.withdraw([wide], 20);
    assert!(withdrawal.wire.ops.is_empty());
    let request = router.prev_plan.reqs.values().flatten().next().unwrap();
    assert_eq!(request.owner_demands, BTreeSet::from([two_demand]));
    assert_eq!(request.coverage_claims, BTreeSet::from([two_claim.clone()]));

    let final_withdrawal = router.withdraw([two], 20);
    assert_eq!(
        final_withdrawal
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn ten_thousand_local_owner_detaches_touch_only_the_departing_metadata() {
    const OWNERS: u16 = 10_000;
    let relay = RelayUrl::parse("wss://metadata-detach-10k.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let mut atoms = Vec::with_capacity(OWNERS as usize);
    let mut kinds = BTreeSet::new();
    let mut owner_demands = BTreeSet::new();
    let mut coverage_claims = BTreeSet::new();
    for kind in 1..=OWNERS {
        let atom = pinned(&relay, [kind]);
        kinds.insert(kind);
        owner_demands.insert(DemandKey::for_atom(&atom));
        coverage_claims.insert(coverage_key(&atom));
        atoms.push(atom);
    }
    let physical = pinned(&relay, kinds);
    let sub_id = SubId::allocate(relay, &physical.routing, physical.authenticate_as, 1);
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
    router.rebuild_active_indexes(atoms.clone());

    router.reset_withdrawal_work();
    let started = Instant::now();
    let mut closes = 0;
    for atom in atoms {
        let outcome = router.withdraw([atom], 20);
        closes += outcome
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count();
    }
    eprintln!("10k exact metadata detaches: {:?}", started.elapsed());

    let work = router.withdrawal_work();
    assert_eq!(closes, 1);
    assert_eq!(work.dropped_atoms, OWNERS as u64);
    assert_eq!(work.request_edges_touched, OWNERS as u64);
    assert_eq!(work.metadata_owner_entries_touched, OWNERS as u64 - 1);
    assert_eq!(work.metadata_claim_entries_touched, OWNERS as u64 - 1);
    assert_eq!(work.metadata_assignment_entries_touched, 0);
    assert_eq!(work.metadata_provenance_entries_touched, 0);
    assert_eq!(work.plan_request_entries_visited, 1);
    assert_eq!(work.requests_closed, 1);
    assert_eq!(work.physical_coverage_edges_released, OWNERS as u64);
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn aliased_claim_stays_until_its_last_distinct_demand_owner_leaves() {
    let relay = RelayUrl::parse("wss://metadata-claim-alias.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let mut first = pinned(&relay, [1]);
    first.filter.since = Some(10);
    let mut second = first.clone();
    second.filter.since = Some(20);
    let claim = coverage_key(&first);
    assert_eq!(claim, coverage_key(&second));
    let first_demand = DemandKey::for_atom(&first);
    let second_demand = DemandKey::for_atom(&second);
    assert_ne!(first_demand, second_demand);
    let sub_id = SubId::allocate(relay, &first.routing, first.authenticate_as, 1);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.prev_plan.reqs.insert(
        session,
        vec![WireReq {
            sub_id,
            filter: first.filter.clone(),
            routing: first.routing.clone(),
            provenance: BTreeSet::new(),
            coverage_claims: BTreeSet::from([claim.clone()]),
            owner_demands: BTreeSet::from([first_demand, second_demand.clone()]),
            coverage_assignments: BTreeSet::new(),
        }],
    );
    router.rebuild_active_indexes([first.clone(), second.clone()]);

    let first_outcome = router.withdraw([first], 20);
    assert!(first_outcome.wire.ops.is_empty());
    assert_eq!(first_outcome.request_metadata_removals.len(), 1);
    assert!(first_outcome.request_metadata_removals[0]
        .removed_coverage_claims
        .is_empty());
    let request = router.prev_plan.reqs.values().flatten().next().unwrap();
    assert_eq!(request.coverage_claims, BTreeSet::from([claim.clone()]));
    assert_eq!(request.owner_demands, BTreeSet::from([second_demand.clone()]));

    let final_outcome = router.withdraw([second], 20);
    assert_eq!(
        final_outcome
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        1
    );
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn full_compile_probes_only_the_one_surviving_request_out_of_ten_thousand_priors() {
    let relay = RelayUrl::parse("wss://full-metadata-survivor.example").unwrap();
    let current = pinned(&relay, [1]);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.compile(&BTreeSet::from([current.clone()]), &LocalFacts::new(), 20);

    for index in 1..10_000 {
        let stale_relay =
            RelayUrl::parse(&format!("wss://full-metadata-stale-{index:05}.example")).unwrap();
        let stale = pinned(&stale_relay, [((index % 50_000) + 2) as u16]);
        let sub_id = SubId::allocate(
            stale_relay.clone(),
            &stale.routing,
            stale.authenticate_as,
            index as u64,
        );
        let session = RelaySessionKey::unauthenticated(stale_relay);
        let request_key = (session.clone(), sub_id.clone());
        let physical_claims = BTreeSet::from([coverage_key(&stale)]);
        router.prev_plan.reqs.insert(
            session.clone(),
            vec![WireReq {
                sub_id: sub_id.clone(),
                filter: stale.filter.clone(),
                routing: stale.routing.clone(),
                provenance: BTreeSet::new(),
                coverage_claims: physical_claims.clone(),
                owner_demands: BTreeSet::from([DemandKey::for_atom(&stale)]),
                coverage_assignments: BTreeSet::new(),
            }],
        );
        router
            .request_position_by_key
            .insert(request_key.clone(), 0);
        router.index_physical_request_claims(&request_key, &physical_claims);
    }
    assert_eq!(router.prev_plan.reqs.len(), 10_000);

    router.reset_full_metadata_work();
    let outcome = router.compile(&BTreeSet::from([current.clone()]), &LocalFacts::new(), 20);
    assert_eq!(
        outcome
            .wire
            .ops
            .iter()
            .flat_map(|(_, ops)| ops)
            .filter(|op| matches!(op, WireOp::Close(_)))
            .count(),
        9_999
    );
    assert_eq!(router.full_metadata_work().requests_probed, 1);
    assert_eq!(router.full_metadata_work().candidate_entries_examined, 3);

    router.withdraw([current], 20);
    assert_eq!(router.ownership_census(), Default::default());
}

#[test]
fn full_compile_indexes_only_added_metadata_over_ten_thousand_incumbent_edges() {
    let relay = RelayUrl::parse("wss://full-metadata-edges.example").unwrap();
    let wide = pinned(&relay, [1, 2]);
    let added = pinned(&relay, [1]);
    let mut router = Router::new(RuleRegistry::default_widen_only());
    router.compile(&BTreeSet::from([wide.clone()]), &LocalFacts::new(), 20);

    let incumbent = router.prev_plan.reqs.values_mut().flatten().next().unwrap();
    for kind in 10_000..20_000 {
        let historical = pinned(&relay, [kind]);
        let demand = DemandKey::for_atom(&historical);
        let author = Keys::generate().public_key();
        incumbent.owner_demands.insert(demand.clone());
        incumbent.coverage_claims.insert(coverage_key(&historical));
        incumbent.coverage_assignments.insert((demand.clone(), author));
        incumbent.provenance.insert(RouteProvenance {
            relay: relay.clone(),
            lane: Lane::Provenance,
            covers_authors: BTreeSet::from([author]),
            route_kind: RouteKind::Coverage,
        });
    }
    assert_eq!(incumbent.owner_demands.len(), 10_001);
    assert_eq!(incumbent.coverage_assignments.len(), 10_000);
    assert_eq!(incumbent.provenance.len(), 10_001);
    router.rebuild_active_indexes([wide.clone()]);
    router.last_diag = crate::diag::build(
        &router.prev_plan,
        &crate::CompileBudget::with_relay_cap(20),
        Default::default(),
        Vec::new(),
    );

    router.reset_full_metadata_work();
    let outcome = router.compile(
        &BTreeSet::from([wide.clone(), added.clone()]),
        &LocalFacts::new(),
        20,
    );
    assert!(outcome.wire.ops.is_empty());
    assert_eq!(outcome.request_metadata_updates.len(), 1);
    assert_eq!(
        outcome.request_metadata_updates[0]
            .added_owner_demands
            .len(),
        1
    );
    assert_eq!(
        outcome.request_metadata_updates[0]
            .added_coverage_claims
            .len(),
        1
    );
    let work = router.full_metadata_work();
    assert_eq!(work.requests_probed, 1);
    assert_eq!(work.owner_edges_visited, 1);
    assert_eq!(work.assignment_edges_visited, 0);
    assert_eq!(work.provenance_author_edges_visited, 0);
    assert_eq!(work.diagnostic_provenance_edges_visited, 0);

    router.withdraw([wide], 20);
    router.withdraw([added], 20);
    assert_eq!(router.ownership_census(), Default::default());
}
