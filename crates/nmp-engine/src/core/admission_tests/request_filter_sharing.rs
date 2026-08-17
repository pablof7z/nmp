//! Grouped-request execution-evidence allocation proofs.

use super::*;
use std::sync::Arc;

#[test]
fn grouped_handoff_shares_one_immutable_filter_across_every_observation_fact() {
    const OWNERS: usize = 207;
    let relay = RelayUrl::parse("wss://shared-request-filter.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    let observations = (0..OWNERS)
        .map(|index| {
            observation_id(&core.handle(EngineMsg::Subscribe(query(
                &relay,
                &format!("owner-{index:03}"),
                Freshness::Live,
            ))))
        })
        .collect::<Vec<_>>();

    let admitted = flush(&mut core);
    assert_eq!(
        wire_ops(&admitted)
            .into_iter()
            .filter(|operation| matches!(operation, WireOp::Req(_, _)))
            .count(),
        1
    );
    let accepted = accept_first_request(&mut core, &session, 81);
    let filters = accepted
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitObservationEvidence(_, evidence) => Some(evidence),
            _ => None,
        })
        .flatten()
        .filter_map(|evidence| match &evidence.fact {
            ObservationFact::RelayRequest { filter, .. } => Some(filter),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(filters.len(), OWNERS);
    assert!(filters.iter().all(|filter| Arc::ptr_eq(filters[0], filter)));

    for observation in observations {
        core.handle(EngineMsg::Unsubscribe(observation));
    }
    assert_eq!(
        core.bench_ownership_census(),
        CoreOwnershipCensus::default()
    );
}

/// A `RelayRequest` says WHY the relay was asked, not only which one.
///
/// This is the accountability half of making `ReadRouting::Auto` the
/// default. An app that names no routing hands the decision to NMP, and a
/// decision nobody can see is the filter-shape inference #847 deleted,
/// wearing a better name. An `Explicit` demand is the tightest case to pin:
/// exactly one lane can possibly have asked, so the reported set is exact
/// rather than merely plausible.
///
/// Break it by dropping `lanes` from the fact, by filling it with
/// `BTreeSet::new()` at the plan-install site, or by having
/// `Router::request_lanes` return the wrong request's provenance.
#[test]
fn an_accepted_request_reports_the_lane_that_asked_for_it() {
    let relay = RelayUrl::parse("wss://lane-reporting.example").unwrap();
    let session = RelaySessionKey::unauthenticated(relay.clone());
    let mut core = EngineCore::new(RedbStore::temporary().expect("temporary Redb store"), 20);
    core.handle(EngineMsg::Subscribe(query(
        &relay,
        "owner",
        Freshness::Live,
    )));
    flush(&mut core);

    let lanes = accept_first_request(&mut core, &session, 82)
        .iter()
        .filter_map(|effect| match effect {
            Effect::EmitObservationEvidence(_, evidence) => Some(evidence),
            _ => None,
        })
        .flatten()
        .filter_map(|evidence| match &evidence.fact {
            ObservationFact::RelayRequest { lanes, .. } => Some(lanes.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();

    assert_eq!(
        lanes,
        vec![BTreeSet::from([nmp_router::Lane::Exact])],
        "an Explicit demand is routed by exactly one lane, and the trace must say so"
    );
}
