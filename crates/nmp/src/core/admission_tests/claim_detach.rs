//! claim detach admission proofs.

use super::*;

#[test]
fn local_owner_detach_prunes_the_current_attribution_generation_before_eose() {
    let relay = RelayUrl::parse("wss://core-metadata-detach.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let incumbent = query_atom(&relay, "incumbent");
    let added = query_atom(&relay, "added");
    let incumbent_claim = coverage_key(&incumbent);
    let added_claim = coverage_key(&added);
    let incumbent_demand = DemandKey::for_atom(&incumbent);
    let added_demand = DemandKey::for_atom(&added);
    let sub_id = SubId::for_wire(
        relay,
        &incumbent.filter,
        &incumbent.source,
        incumbent.access,
    );
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    core.attribution.observe_atom(&incumbent);
    core.attribution.observe_atom(&added);
    core.attribution
        .retain_live_request_claims(&sub_id, BTreeSet::from([incumbent_claim]));
    core.record_observed_request(RequestSend {
        session: &session,
        sub_id: &sub_id,
        filter: &incumbent.filter,
        coverage_claims: BTreeSet::from([incumbent_claim]),
        owner_demands: BTreeSet::from([incumbent_demand]),
        replay: false,
        event_failure_target: EventFailureTarget::ThisSend,
    });
    core.apply_request_metadata_updates(
        &[nmp_router::RequestMetadataUpdate {
            session: session.clone(),
            sub_id: sub_id.clone(),
            filter_hash: incumbent.filter.hash(),
            added_coverage_claims: BTreeSet::from([added_claim]),
            added_owner_demands: BTreeSet::from([added_demand]),
        }],
        &mut Vec::new(),
    );

    core.attribution.release_atom(&added);
    core.apply_request_metadata_removals(&[nmp_router::RequestMetadataRemoval {
        session: session.clone(),
        sub_id: sub_id.clone(),
        filter_hash: incumbent.filter.hash(),
        removed_coverage_claims: BTreeSet::from([added_claim]),
        removed_owner_demands: BTreeSet::from([added_demand]),
    }]);

    let pending = core.pending_request_evidence[&(session.clone(), sub_id.clone())]
        .back()
        .unwrap();
    assert_eq!(pending.owner_demands, BTreeSet::from([incumbent_demand]));
    let census = core.bench_ownership_census();
    assert_eq!(census.attribution_live_shape_keys, 1);
    assert_eq!(census.attribution_live_shape_refs, 1);
    assert_eq!(census.attribution_inflight_shape_keys, 1);
    assert_eq!(census.attribution_inflight_shape_refs, 1);

    let completed = core
        .attribution
        .attribute_eose_detailed(
            &session,
            &wire_sub_id_string(&sub_id),
            Timestamp::from(100u64),
        )
        .unwrap();
    let completed_claims: BTreeSet<_> = completed
        .eligible_claims()
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        completed_claims,
        BTreeSet::from([incumbent_claim]),
        "a late EOSE must not persist a claim with no current local owner"
    );

    core.abandon_sub(&sub_id);
    core.attribution.release_live_request_claims(&sub_id);
    core.attribution.release_atom(&incumbent);
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

#[test]
fn aliased_current_claim_stays_until_its_last_owner_and_can_reattach_before_eose() {
    let relay = RelayUrl::parse("wss://core-metadata-alias.example").unwrap();
    let session = RelaySessionKey::public(relay.clone());
    let incumbent = query_atom(&relay, "incumbent");
    let mut first_alias = query_atom(&relay, "aliased");
    first_alias.filter.since = Some(10);
    let mut second_alias = query_atom(&relay, "aliased");
    second_alias.filter.since = Some(20);
    let incumbent_claim = coverage_key(&incumbent);
    let alias_claim = coverage_key(&first_alias);
    assert_eq!(alias_claim, coverage_key(&second_alias));
    let incumbent_demand = DemandKey::for_atom(&incumbent);
    let first_demand = DemandKey::for_atom(&first_alias);
    let second_demand = DemandKey::for_atom(&second_alias);
    assert_ne!(first_demand, second_demand);
    let sub_id = SubId::for_wire(
        relay,
        &incumbent.filter,
        &incumbent.source,
        incumbent.access,
    );
    let mut core = EngineCore::new(MemoryStore::new(), 20);
    for atom in [&incumbent, &first_alias, &second_alias] {
        core.attribution.observe_atom(atom);
    }
    core.attribution
        .retain_live_request_claims(&sub_id, BTreeSet::from([incumbent_claim]));
    core.record_observed_request(RequestSend {
        session: &session,
        sub_id: &sub_id,
        filter: &incumbent.filter,
        coverage_claims: BTreeSet::from([incumbent_claim]),
        owner_demands: BTreeSet::from([incumbent_demand]),
        replay: false,
        event_failure_target: EventFailureTarget::ThisSend,
    });
    core.apply_request_metadata_updates(
        &[nmp_router::RequestMetadataUpdate {
            session: session.clone(),
            sub_id: sub_id.clone(),
            filter_hash: incumbent.filter.hash(),
            added_coverage_claims: BTreeSet::from([alias_claim]),
            added_owner_demands: BTreeSet::from([first_demand, second_demand]),
        }],
        &mut Vec::new(),
    );

    core.attribution.release_atom(&first_alias);
    core.apply_request_metadata_removals(&[nmp_router::RequestMetadataRemoval {
        session: session.clone(),
        sub_id: sub_id.clone(),
        filter_hash: incumbent.filter.hash(),
        removed_coverage_claims: BTreeSet::new(),
        removed_owner_demands: BTreeSet::from([first_demand]),
    }]);
    assert_eq!(
        core.attribution.current_claims(&sub_id),
        BTreeSet::from([incumbent_claim, alias_claim]),
        "the remaining exact DemandKey owner retains its aliased claim"
    );

    core.attribution.release_atom(&second_alias);
    core.apply_request_metadata_removals(&[nmp_router::RequestMetadataRemoval {
        session: session.clone(),
        sub_id: sub_id.clone(),
        filter_hash: incumbent.filter.hash(),
        removed_coverage_claims: BTreeSet::from([alias_claim]),
        removed_owner_demands: BTreeSet::from([second_demand]),
    }]);
    assert_eq!(
        core.attribution.current_claims(&sub_id),
        BTreeSet::from([incumbent_claim])
    );

    core.attribution.observe_atom(&first_alias);
    core.apply_request_metadata_updates(
        &[nmp_router::RequestMetadataUpdate {
            session: session.clone(),
            sub_id: sub_id.clone(),
            filter_hash: incumbent.filter.hash(),
            added_coverage_claims: BTreeSet::from([alias_claim]),
            added_owner_demands: BTreeSet::from([first_demand]),
        }],
        &mut Vec::new(),
    );
    assert_eq!(
        core.attribution.current_claims(&sub_id),
        BTreeSet::from([incumbent_claim, alias_claim])
    );

    let completed = core
        .attribution
        .attribute_eose_detailed(
            &session,
            &wire_sub_id_string(&sub_id),
            Timestamp::from(100u64),
        )
        .unwrap();
    let completed_claims: BTreeSet<_> = completed
        .eligible_claims()
        .unwrap()
        .into_iter()
        .map(|(key, _)| key)
        .collect();
    assert_eq!(
        completed_claims,
        BTreeSet::from([incumbent_claim, alias_claim])
    );

    core.abandon_sub(&sub_id);
    core.attribution.release_live_request_claims(&sub_id);
    core.attribution.release_atom(&first_alias);
    core.attribution.release_atom(&incumbent);
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}
